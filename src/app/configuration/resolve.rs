//! Configuration resolution, source locations and value explanations.

use crate::app::commands::AppEffect;
use crate::app::commands::register_app_commands;
use crate::app::configuration::KeymapCompilation;
use crate::app::configuration::compile::KeymapNotice;
use crate::app::configuration::compile::expanded_key_sequence;
use crate::app::configuration::compile::{compile_catalog_keymaps, compile_startup_fallback};
use crate::app::configuration::compile_keymap_entries;
use crate::app::configuration::configured_context;
use crate::app::configuration::literal_key_sequence;
use crate::app::configuration::plugin_owner_from_id;
use crate::app::configuration::validate_configured_commands;
use crate::config::{ConfigurationCandidateError, LoadedConfig};
use crate::editor::CommandRegistry;
use crate::editor::input::InputContexts;
use crate::editor::register_builtin_commands;
use crate::plugins::PluginContributionCatalog;
use crate::ui::DashboardAction;
use std::collections::BTreeSet;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn enabled_plugin_owners(loaded: &LoadedConfig) -> BTreeSet<String> {
    let mut enabled = loaded
        .config
        .plugins
        .entries
        .iter()
        .filter(|(_, entry)| entry.enabled)
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    enabled.extend(
        loaded
            .installed_plugins
            .iter()
            .filter(|owner| {
                loaded
                    .config
                    .plugins
                    .entries
                    .get(*owner)
                    .is_none_or(|entry| entry.enabled)
            })
            .cloned(),
    );
    enabled
}

pub(in crate::app) fn base_safe_keymaps(
    loaded: &LoadedConfig,
) -> (crate::config::LoadedKeymaps, BTreeSet<String>, bool) {
    let enabled = enabled_plugin_owners(loaded);
    let mut base = loaded.keymaps.clone();
    let mut entries = base.entries[..base.bundled_count].to_vec();
    let mut spans = Vec::new();
    let mut deferred = loaded
        .config
        .input
        .command_aliases
        .values()
        .chain(loaded.dashboard.action.iter().map(|action| &action.command))
        .any(|id| plugin_owner_from_id(id).is_some_and(|owner| enabled.contains(owner)));
    let mut deferred_entries = Vec::new();
    for (offset, entry) in base.entries[base.bundled_count..].iter().enumerate() {
        let dependencies = [
            entry.disable.then_some(entry.id.as_str()),
            entry.command.as_deref(),
            entry.context.as_deref(),
        ];
        if dependencies
            .into_iter()
            .flatten()
            .filter_map(plugin_owner_from_id)
            .any(|owner| enabled.contains(owner))
        {
            deferred = true;
            deferred_entries.push(entry);
        } else {
            entries.push(entry.clone());
            if let Some(span) = base.user_spans.get(offset) {
                spans.push(span.clone());
            }
        }
    }
    let policy = &loaded.config.input.keymaps;
    let leaders = literal_key_sequence("input.keymaps.leader", &policy.leader).and_then(|leader| {
        literal_key_sequence("input.keymaps.local_leader", &policy.local_leader)
            .map(|local_leader| (leader, local_leader))
    });
    let contexts = InputContexts::bundled();
    let mut groups = base.groups[..base.bundled_group_count].to_vec();
    let mut group_spans = Vec::new();
    for (offset, group) in base.groups[base.bundled_group_count..].iter().enumerate() {
        let depends_on_plugin = [
            group.disable.then_some(group.id.as_str()),
            group.context.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(plugin_owner_from_id)
        .any(|owner| enabled.contains(owner));
        let deferred_continuation = leaders.as_ref().is_ok_and(|(leader, local_leader)| {
            let Some(prefix) = group
                .prefix
                .as_deref()
                .and_then(|prefix| expanded_key_sequence(prefix, leader, local_leader).ok())
            else {
                return false;
            };
            let Some(context) = group
                .context
                .as_deref()
                .and_then(|context| configured_context(&contexts, context, &group.mode).ok())
            else {
                return false;
            };
            deferred_entries.iter().any(|entry| {
                entry.context.as_deref().is_some_and(|entry_context| {
                    configured_context(&contexts, entry_context, &entry.mode)
                        .is_ok_and(|entry_context| entry_context == context)
                }) && entry.keys.as_deref().is_some_and(|keys| {
                    expanded_key_sequence(keys, leader, local_leader)
                        .is_ok_and(|keys| keys.len() > prefix.len() && keys.starts_with(&prefix))
                })
            })
        });
        if depends_on_plugin || deferred_continuation {
            deferred = true;
        } else {
            groups.push(group.clone());
            if let Some(span) = base.user_group_spans.get(offset) {
                group_spans.push(span.clone());
            }
        }
    }
    base.entries = entries;
    base.groups = groups;
    base.user_spans = spans;
    base.user_group_spans = group_spans;
    (base, enabled, deferred)
}

pub(in crate::app) fn resolve_configuration(
    commands: &CommandRegistry<AppEffect>,
    loaded: LoadedConfig,
    strict_sources: bool,
) -> Result<(ResolvedConfiguration, Option<String>), String> {
    resolve_configuration_with_catalog(commands, loaded, strict_sources, None, None)
        .map_err(|error| error.to_string())
}

pub(in crate::app) fn resolve_startup_configuration(
    commands: &CommandRegistry<AppEffect>,
    loaded: LoadedConfig,
    safe_mode: bool,
) -> Result<(ResolvedConfiguration, Option<String>), String> {
    let no_pending_owners = BTreeSet::new();
    resolve_configuration_with_catalog(
        commands,
        loaded,
        false,
        None,
        safe_mode.then_some(&no_pending_owners),
    )
    .map_err(|error| error.to_string())
}

pub(super) fn resolve_configuration_with_catalog(
    commands: &CommandRegistry<AppEffect>,
    mut loaded: LoadedConfig,
    strict_sources: bool,
    live: Option<(&PluginContributionCatalog, &ResolvedKeymaps)>,
    pending_owners: Option<&BTreeSet<String>>,
) -> Result<(ResolvedConfiguration, Option<String>), ConfigurationCandidateError> {
    let (catalog, retained) = live.unzip();
    let resolve_references = !strict_sources || catalog.is_some();
    if loaded.user_configuration {
        loaded.installed_plugins = crate::tools::installed_plugins(
            &crate::config::user_state_directory().join("packages"),
        );
    }
    let mut diagnostics = Vec::new();
    diagnostics.extend(loaded.diagnostic.iter().cloned());
    if strict_sources {
        if !diagnostics.is_empty() {
            diagnostics.extend(loaded.keymaps.diagnostic.iter().cloned());
            return Err(diagnostics.join("; ").into());
        }
        if let Some(detail) = &loaded.keymaps.diagnostic {
            return Err(if catalog.is_some() {
                ConfigurationCandidateError::keymap_source(&loaded.keymaps.path, detail)
            } else {
                ConfigurationCandidateError::Keymaps(detail.clone())
            });
        }
    }
    let raw_command_references =
        super::validation::CommandReferences::new(&loaded.config, &loaded.dashboard);
    let (base_keymaps, expected_plugin_indexes, mut deferred_plugin_keymaps) =
        base_safe_keymaps(&loaded);
    let mut skipped = Vec::new();
    if resolve_references {
        use super::validation::CommandReferencePolicy;
        let pending = pending_owners.unwrap_or(&expected_plugin_indexes);
        let (references, notices) = raw_command_references.resolve(
            commands,
            &loaded.config.command_labels,
            pending,
            CommandReferencePolicy::Runtime,
        )?;
        loaded.config.input.command_aliases = references.aliases;
        loaded.dashboard = references.dashboard;
        skipped.extend(notices);
    }
    if let Err(error) = validate_configured_commands(commands, &loaded, resolve_references) {
        diagnostics.push(error);
        return Err(diagnostics.join("; ").into());
    }
    let mut keymap_notice = loaded
        .keymaps
        .diagnostic
        .as_deref()
        .map(|error| KeymapNotice::from_read_error(&loaded.keymaps.path, error));
    let mut startup_keymap_fallback = keymap_notice
        .clone()
        .map(StartupKeymapFallback::UntilReload);
    let source = KeymapCompilation::new(&loaded.keymaps, &loaded.config.input.keymaps);
    let describe = |errors: &super::compile::KeymapErrors| {
        if catalog.is_some() {
            errors.describe_short(
                &loaded.keymaps.path,
                &loaded.path,
                &loaded.keymap_policy_lines,
            )
        } else {
            errors.describe(
                &loaded.keymaps.path,
                &loaded.path,
                &loaded.keymap_policy_lines,
            )
        }
    };
    let mut keymaps = if strict_sources && catalog.is_none() {
        // --check-config has no runtime catalog; keep its base-only validation.
        compile_keymap_entries(
            commands,
            KeymapCompilation::new(&base_keymaps, &loaded.config.input.keymaps),
        )
        .map_err(|errors| ConfigurationCandidateError::Keymaps(describe(&errors)))?
    } else {
        let empty_catalog = PluginContributionCatalog::default();
        // At startup, enabled providers haven't had a chance to index yet.
        let pending_owners = pending_owners.unwrap_or(&expected_plugin_indexes);
        match compile_catalog_keymaps(
            commands,
            source,
            catalog.unwrap_or(&empty_catalog),
            pending_owners,
            retained,
        ) {
            Ok((keys, notice)) => {
                keymap_notice = keymap_notice.or(notice);
                keys
            }
            Err(errors) if strict_sources => {
                return Err(ConfigurationCandidateError::Keymaps(describe(&errors)));
            }
            Err(errors) => {
                let (keys, fallback) = compile_startup_fallback(commands, source, errors)
                    .map_err(|errors| ConfigurationCandidateError::Keymaps(describe(&errors)))?;
                let notice = fallback.diagnostics.fallback_notice(
                    &loaded.keymaps.path,
                    &loaded.path,
                    &loaded.keymap_policy_lines,
                );
                startup_keymap_fallback = Some(if fallback.policy.is_some() {
                    StartupKeymapFallback::UntilReload(notice.clone())
                } else {
                    StartupKeymapFallback::Retryable(notice.clone())
                });
                if let Some(policy) = fallback.policy {
                    loaded.config.input.keymaps = policy;
                    for key in ["input.keymaps.leader", "input.keymaps.local_leader"] {
                        loaded.user_setting_keys.remove(key);
                        loaded.user_setting_spans.remove(key);
                    }
                }
                keymap_notice = Some(notice);
                keys
            }
        }
    };
    let mut settings = loaded.config;
    let raw_windows = settings.windows.clone();
    if resolve_references {
        let empty_catalog = PluginContributionCatalog::default();
        let (windows, notices) = super::Configuration::filter_plugin_window_rules(
            &raw_windows,
            catalog.unwrap_or(&empty_catalog),
            pending_owners.unwrap_or(&expected_plugin_indexes),
        )?;
        settings.windows = windows;
        skipped.extend(notices);
    } else if let Some(rule) = settings.windows.rules.iter().find(|rule| {
        plugin_owner_from_id(&rule.content_type)
            .is_some_and(|owner| !expected_plugin_indexes.contains(owner))
    }) {
        return Err(format!(
            "plugin window selector `{}` does not name an enabled plugin",
            rule.content_type
        )
        .into());
    }
    if raw_windows.rules.iter().any(|rule| {
        plugin_owner_from_id(&rule.content_type)
            .is_some_and(|owner| expected_plugin_indexes.contains(owner))
    }) {
        deferred_plugin_keymaps = true;
        if !resolve_references {
            settings
                .windows
                .rules
                .retain(|rule| !rule.content_type.starts_with("plugin."));
        }
    }
    keymap_notice = KeymapNotice::combine(keymap_notice, KeymapNotice::skipped(skipped));
    if let Some(notice) = &keymap_notice {
        diagnostics.insert(0, notice.summary.clone());
        keymaps.conflicts.push(notice.detail.clone());
    }
    let keymap_source = if loaded.keymaps.bundled_count > 0
        && loaded.keymaps.entries.len() == loaded.keymaps.bundled_count
    {
        PathBuf::from("<bundled:keymaps>")
    } else {
        loaded.keymaps.path.clone()
    };
    let resolved = ResolvedConfiguration {
        settings,
        dashboard: loaded.dashboard,
        theme: loaded.theme,
        raw_keymaps: loaded.keymaps,
        base_keymaps,
        raw_windows,
        raw_command_references,
        deferred_plugin_keymaps,
        expected_plugin_indexes,
        keymaps,
        keymap_notice,
        startup_keymap_fallback,
        sources: ConfigurationSources {
            settings: loaded.path,
            keymaps: keymap_source,
            dashboard: loaded.dashboard_path,
            theme: loaded.theme_path,
        },
        user_setting_keys: loaded.user_setting_keys,
        user_setting_spans: loaded.user_setting_spans,
        dashboard_spans: loaded.dashboard_spans,
        theme_spans: loaded.theme_spans,
    };
    Ok((
        resolved,
        (!diagnostics.is_empty()).then(|| diagnostics.join("; ")),
    ))
}

pub fn validate_loaded_configuration(loaded: LoadedConfig) -> Result<(), String> {
    let mut commands = CommandRegistry::default();
    register_builtin_commands(&mut commands)
        .map_err(|error| format!("built-in command registry failed: {error}"))?;
    register_app_commands(&mut commands)
        .map_err(|error| format!("application command registry failed: {error}"))?;
    resolve_configuration(&commands, loaded, true).map(|_| ())
}

pub(in crate::app) fn dashboard_actions(
    configuration: &ResolvedConfiguration,
) -> Vec<DashboardAction> {
    configuration
        .dashboard
        .action
        .iter()
        .filter(|action| action.visible)
        .map(|action| DashboardAction {
            command: action.command.clone(),
            label: action.label.clone(),
            icon: action.icon.clone(),
            key: action
                .show_binding
                .then(|| {
                    configuration
                        .keymaps
                        .binding_for_action("dashboard", &action.command, &action.arguments)
                        .map(str::to_owned)
                })
                .flatten(),
        })
        .collect()
}

pub(in crate::app) const fn configuration_reload_is_newer(current: u64, candidate: u64) -> bool {
    candidate > current
}

pub(in crate::app) fn configured_grapheme_width(
    grapheme: &str,
    ambiguous_width: crate::config::AmbiguousWidth,
) -> usize {
    let width = if ambiguous_width == crate::config::AmbiguousWidth::Wide {
        UnicodeWidthStr::width_cjk(grapheme)
    } else {
        UnicodeWidthStr::width(grapheme)
    };
    width.max(1)
}

pub(in crate::app) fn configured_text_width(
    text: &str,
    tab_width: usize,
    ambiguous_width: crate::config::AmbiguousWidth,
) -> usize {
    let mut column = 0_usize;
    for grapheme in text.graphemes(true) {
        if grapheme == "\t" {
            column += tab_width - (column % tab_width);
        } else {
            column += configured_grapheme_width(grapheme, ambiguous_width);
        }
    }
    column
}

use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::app) struct ConfigurationSources {
    pub settings: PathBuf,
    pub keymaps: PathBuf,
    pub dashboard: PathBuf,
    pub theme: PathBuf,
}

#[derive(Clone)]
pub(super) enum StartupKeymapFallback {
    /// The parsed keys failed validation, which may change when a plugin joins.
    Retryable(KeymapNotice),
    /// The source could not be read, or startup replaced the invalid leader policy.
    UntilReload(KeymapNotice),
}

#[derive(Clone)]
pub(crate) struct ResolvedConfiguration {
    pub settings: crate::config::Config,
    pub dashboard: crate::config::DashboardConfig,
    pub theme: crate::config::ThemeConfig,
    pub raw_keymaps: crate::config::LoadedKeymaps,
    pub base_keymaps: crate::config::LoadedKeymaps,
    pub raw_windows: crate::config::WindowsConfig,
    pub(super) raw_command_references: super::validation::CommandReferences,
    pub deferred_plugin_keymaps: bool,
    pub expected_plugin_indexes: BTreeSet<String>,
    pub keymaps: ResolvedKeymaps,
    /// Active fallback, not the status of the parsed declarations.
    pub(super) keymap_notice: Option<KeymapNotice>,
    /// Cleared when the original user keymaps are accepted, never entered by a plugin change.
    pub(super) startup_keymap_fallback: Option<StartupKeymapFallback>,
    pub(in crate::app) sources: ConfigurationSources,
    pub user_setting_keys: BTreeSet<String>,
    pub user_setting_spans: BTreeMap<String, std::ops::Range<usize>>,
    pub dashboard_spans: BTreeMap<String, std::ops::Range<usize>>,
    pub theme_spans: BTreeMap<String, std::ops::Range<usize>>,
}

pub(in crate::app) fn resolved_toml_value(
    mut value: toml::Value,
    segments: &[String],
) -> Option<toml::Value> {
    for segment in segments {
        value = if let Some(table) = value.as_table() {
            table.get(segment)?.clone()
        } else {
            value
                .as_array()?
                .get(segment.parse::<usize>().ok()?)?
                .clone()
        };
    }
    Some(value)
}

impl ResolvedConfiguration {
    #[must_use]
    pub fn explain(&self, key: &str) -> Option<crate::config::ConfigExplanation> {
        let (value, source, span, layered) = if let Some(key) = key.strip_prefix("theme.") {
            let segments = crate::config::config_key_segments(key)?;
            let canonical = segments.join(".");
            let value = resolved_toml_value(toml::Value::try_from(&self.theme).ok()?, &segments)?;
            (
                value,
                &self.sources.theme,
                self.theme_spans.get(&canonical),
                false,
            )
        } else if let Some(key) = key.strip_prefix("dashboard.") {
            let segments = crate::config::config_key_segments(key)?;
            let canonical = segments.join(".");
            let value =
                resolved_toml_value(toml::Value::try_from(&self.dashboard).ok()?, &segments)?;
            (
                value,
                &self.sources.dashboard,
                self.dashboard_spans.get(&canonical),
                false,
            )
        } else {
            let segments = crate::config::config_key_segments(key)?;
            let value =
                resolved_toml_value(toml::Value::try_from(&self.settings).ok()?, &segments)?;
            let user_key = (1..=segments.len())
                .rev()
                .map(|length| segments[..length].join("."))
                .find(|ancestor| self.user_setting_keys.contains(ancestor));
            let source = if user_key.is_some() {
                &self.sources.settings
            } else {
                // Inherited values continue to identify the bundled declaration.
                static BUNDLED_SETTINGS: std::sync::LazyLock<PathBuf> =
                    std::sync::LazyLock::new(|| PathBuf::from("<bundled:config>"));
                &BUNDLED_SETTINGS
            };
            let span = user_key
                .as_ref()
                .and_then(|key| self.user_setting_spans.get(key));
            (value, source, span, true)
        };
        let mut final_source = source.display().to_string();
        if let Some(span) = span {
            final_source = format!("{final_source}:bytes {}..{}", span.start, span.end);
        }
        let bundled = final_source.starts_with("<bundled:");
        Some(crate::config::ConfigExplanation {
            key: key.into(),
            value: value.to_string(),
            source: if bundled {
                "bundled defaults".into()
            } else {
                final_source.clone()
            },
            contributors: if bundled {
                vec!["bundled defaults".into()]
            } else if layered {
                vec!["bundled defaults".into(), final_source]
            } else {
                vec![final_source]
            },
        })
    }
}

use crate::app::configuration::ResolvedKeymaps;
