//! Plugin keymaps and window rules validated against the accepted configuration.

use crate::app::commands::AppEffect;
use crate::app::configuration::Configuration;
use crate::app::configuration::ContributionCandidate;
use crate::app::configuration::KeymapCompilation;
use crate::app::configuration::ResolvedKeymaps;
use crate::app::configuration::compile::KeymapNotice;
use crate::app::configuration::compile::compile_catalog_keymaps;
use crate::app::configuration::plugin_owner_from_id;
use crate::app::configuration::resolve::StartupKeymapFallback;
use crate::config::WindowPlacementConfig;
use crate::config::WindowsConfig;
use crate::editor::CommandRegistry;
use crate::plugins::PluginContributionCatalog;
use crate::plugins::PluginWindowPlacementKind;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[error("{diagnostic}")]
pub(in crate::app) struct ContributionError {
    diagnostic: String,
    pub(in crate::app) shortcut_notice: Option<String>,
}

impl From<String> for ContributionError {
    fn from(diagnostic: String) -> Self {
        Self {
            diagnostic,
            shortcut_notice: None,
        }
    }
}

pub(super) struct ContributionKeymaps {
    resolved: ResolvedKeymaps,
    notice: Option<KeymapNotice>,
    using_startup_fallback: bool,
}

#[derive(Clone, Copy)]
pub(in crate::app) enum ContributionKeymapPolicy {
    RetainStartupFallback,
    UpdatePreflight,
}

impl Configuration {
    fn compile_contribution_keymaps(
        &self,
        commands: &CommandRegistry<AppEffect>,
        catalog: &PluginContributionCatalog,
        pending_owners: &BTreeSet<String>,
        keymap_policy: ContributionKeymapPolicy,
    ) -> Result<ContributionKeymaps, ContributionError> {
        let source = self.raw_keymaps();
        let policy = &self.settings().input.keymaps;
        let compile = |compilation| {
            compile_catalog_keymaps(
                commands,
                compilation,
                catalog,
                pending_owners,
                Some(self.keymaps()),
            )
            .map_err(|errors| ContributionError {
                diagnostic: errors.describe(
                    &source.path,
                    &self.accepted.sources.settings,
                    &Default::default(),
                ),
                shortcut_notice: errors.shortcut_notice().map(str::to_owned),
            })
        };
        let fallback = self.accepted.startup_keymap_fallback.as_ref();
        let fallback_notice = match fallback {
            Some(StartupKeymapFallback::UntilReload(notice)) => notice,
            _ => match compile(KeymapCompilation::new(source, policy)) {
                Ok((resolved, notice)) => {
                    return Ok(Self::contribution_keymaps(resolved, notice, false));
                }
                Err(error) => match (fallback, keymap_policy) {
                    (
                        Some(StartupKeymapFallback::Retryable(notice)),
                        ContributionKeymapPolicy::RetainStartupFallback,
                    ) => notice,
                    // An update rejection must not be turned into a bundled-key retry.
                    _ => return Err(error),
                },
            },
        };
        let bundled = KeymapCompilation::new(&self.accepted.base_keymaps, policy).bundled_only();
        let (resolved, skipped) = compile(bundled)?;
        let notice = KeymapNotice::combine(Some(fallback_notice.clone()), skipped);
        Ok(Self::contribution_keymaps(resolved, notice, true))
    }

    fn contribution_keymaps(
        mut resolved: ResolvedKeymaps,
        notice: Option<KeymapNotice>,
        using_startup_fallback: bool,
    ) -> ContributionKeymaps {
        if let Some(notice) = &notice {
            resolved.conflicts.push(notice.detail.clone());
        }
        ContributionKeymaps {
            resolved,
            notice,
            using_startup_fallback,
        }
    }

    pub(in crate::app) fn prepare_base_bindings(
        &self,
        commands: &CommandRegistry<AppEffect>,
        catalog: &PluginContributionCatalog,
        deferred: bool,
        pending_owners: &BTreeSet<String>,
    ) -> Result<ContributionCandidate, String> {
        let keymaps = self
            .compile_contribution_keymaps(
                commands,
                catalog,
                pending_owners,
                ContributionKeymapPolicy::RetainStartupFallback,
            )
            .map_err(|error| error.to_string())?;
        Ok(ContributionCandidate {
            stamp: self.stamp.clone(),
            catalog: catalog.version(),
            keymaps: Some(keymaps),
            command_references: None,
            windows: self.settings().windows.clone(),
            deferred,
        })
    }

    pub(in crate::app) fn prepare_plugin_contributions(
        &self,
        commands: &CommandRegistry<AppEffect>,
        catalog: &PluginContributionCatalog,
        pending_owners: &BTreeSet<String>,
        keymap_policy: ContributionKeymapPolicy,
    ) -> Result<ContributionCandidate, ContributionError> {
        let (references, mut notices) = self.accepted.raw_command_references.resolve(
            commands,
            &self.settings().command_labels,
            pending_owners,
            super::validation::CommandReferencePolicy::Runtime,
        )?;
        let mut keymaps =
            self.compile_contribution_keymaps(commands, catalog, pending_owners, keymap_policy)?;
        let (windows, skipped) =
            Self::filter_plugin_window_rules(self.raw_windows(), catalog, pending_owners)?;
        notices.extend(skipped);
        keymaps.resolved.conflicts.extend(notices.iter().cloned());
        keymaps.notice = KeymapNotice::combine(keymaps.notice, KeymapNotice::skipped(notices));
        Ok(ContributionCandidate {
            stamp: self.stamp.clone(),
            catalog: catalog.version(),
            keymaps: Some(keymaps),
            command_references: Some(references),
            windows,
            deferred: self.deferred_plugin_keymaps(),
        })
    }

    pub(in crate::app) fn prepare_builtin_rules(
        &self,
        catalog: &PluginContributionCatalog,
    ) -> ContributionCandidate {
        ContributionCandidate {
            stamp: self.stamp.clone(),
            catalog: catalog.version(),
            keymaps: None,
            command_references: None,
            windows: WindowsConfig {
                rules: self
                    .raw_windows()
                    .rules
                    .iter()
                    .filter(|rule| !rule.content_type.starts_with("plugin."))
                    .cloned()
                    .collect(),
            },
            deferred: self.deferred_plugin_keymaps(),
        }
    }

    pub(in crate::app) fn prepare_builtin_contributions(
        &self,
        commands: &CommandRegistry<AppEffect>,
        catalog: &PluginContributionCatalog,
        deferred: bool,
        pending_owners: &BTreeSet<String>,
    ) -> Result<ContributionCandidate, String> {
        let mut candidate = self.prepare_builtin_rules(catalog);
        let keymaps = self
            .compile_contribution_keymaps(
                commands,
                catalog,
                pending_owners,
                ContributionKeymapPolicy::RetainStartupFallback,
            )
            .map_err(|error| error.to_string())?;
        candidate.keymaps = Some(keymaps);
        candidate.deferred = deferred;
        Ok(candidate)
    }

    pub(in crate::app) fn check_contributions(
        &self,
        candidate: &ContributionCandidate,
        catalog: &PluginContributionCatalog,
    ) -> Result<(), String> {
        self.check_stamp(&candidate.stamp)?;
        Self::check_catalog(&candidate.catalog, catalog)
    }

    pub(in crate::app) fn commit_contributions(
        &mut self,
        candidate: ContributionCandidate,
        catalog: &PluginContributionCatalog,
    ) -> Result<crate::app::configuration::AcceptedContributions, String> {
        self.check_contributions(&candidate, catalog)?;
        let accepted = crate::app::configuration::AcceptedContributions(candidate.window_source());
        if let Some(keymaps) = candidate.keymaps {
            self.accepted.keymaps = keymaps.resolved;
            self.accepted.keymap_notice = keymaps.notice;
            if !keymaps.using_startup_fallback {
                self.accepted.startup_keymap_fallback = None;
            }
            self.accepted.deferred_plugin_keymaps = candidate.deferred;
            self.binding_revision = self.binding_revision.saturating_add(1);
        }
        if let Some(references) = candidate.command_references {
            self.accepted.settings.input.command_aliases = references.aliases;
            self.accepted.dashboard = references.dashboard;
        }
        self.accepted.settings.windows = candidate.windows;
        self.stamp = Arc::new(());
        Ok(accepted)
    }

    pub(super) fn filter_plugin_window_rules(
        windows: &WindowsConfig,
        catalog: &PluginContributionCatalog,
        pending: &BTreeSet<String>,
    ) -> Result<(WindowsConfig, Vec<String>), String> {
        let mut seen = BTreeSet::new();
        let mut notices = Vec::new();
        let mut effective = WindowsConfig::default();
        for rule in &windows.rules {
            let selector = &rule.content_type;
            if !selector.starts_with("plugin.") {
                effective.rules.push(rule.clone());
                continue;
            }
            if !seen.insert(selector) {
                return Err("windows.rule contains a duplicate content_type".into());
            }
            let Some(declaration) = plugin_owner_from_id(selector)
                .and_then(|owner| catalog.get(owner))
                .and_then(|index| {
                    index
                        .content_types
                        .iter()
                        .find(|content| content.id == *selector)
                })
            else {
                if !plugin_owner_from_id(selector)
                    .is_some_and(|owner| catalog.get(owner).is_none() && pending.contains(owner))
                {
                    notices.push(format!(
                        "Window rule \"{selector}\" doesn't match any plugin panel."
                    ));
                }
                continue;
            };
            let requested = match &rule.placement {
                WindowPlacementConfig::Automatic => None,
                WindowPlacementConfig::Docked { .. } => Some(PluginWindowPlacementKind::Docked),
                WindowPlacementConfig::Floating { .. } => Some(PluginWindowPlacementKind::Floating),
                WindowPlacementConfig::Fullscreen => Some(PluginWindowPlacementKind::Fullscreen),
                WindowPlacementConfig::Hidden => Some(PluginWindowPlacementKind::Hidden),
            };
            if requested
                .is_some_and(|placement| !declaration.allowed_placements.contains(&placement))
            {
                return Err(format!(
                    "plugin window selector `{selector}` requests a disallowed placement"
                ));
            }
            effective.rules.push(rule.clone());
        }
        Ok((effective, notices))
    }
}

#[cfg(test)]
mod tests {
    use super::ContributionKeymapPolicy;
    use crate::app::commands::register_app_commands;
    use crate::app::configuration::Configuration;
    use crate::editor::CommandRegistry;
    use crate::editor::register_builtin_commands;
    use crate::plugins::PluginContributionCatalog;

    #[test]
    fn policy_candidates_reject_foreign_and_changed_owners_without_publishing() {
        let mut commands = CommandRegistry::default();
        register_builtin_commands(&mut commands).unwrap();
        register_app_commands(&mut commands).unwrap();
        let (resolved, _) = crate::app::configuration::resolve_configuration(
            &commands,
            crate::config::load_bundled_config(),
            true,
        )
        .unwrap();
        let mut first = Configuration::new(resolved.clone(), false, None);
        let mut other = Configuration::new(resolved, false, None);
        let catalog = PluginContributionCatalog::default();
        let before = other.settings().windows.clone();
        let revision = other.binding_revision();
        assert!(
            other
                .commit_contributions(first.prepare_builtin_rules(&catalog), &catalog)
                .is_err()
        );
        assert_eq!(other.settings().windows, before);
        assert_eq!(other.binding_revision(), revision);
        let candidate = first
            .prepare_plugin_contributions(
                &commands,
                &catalog,
                &Default::default(),
                ContributionKeymapPolicy::RetainStartupFallback,
            )
            .unwrap();
        let reload = first
            .prepare(
                crate::config::load_bundled_config(),
                &commands,
                &catalog,
                &Default::default(),
            )
            .unwrap();
        first.toggle_inlay_hints();
        assert!(first.commit_contributions(candidate, &catalog).is_err());
        assert!(first.stage_watch(&reload, &catalog).is_err());
        assert!(first.commit(reload, &catalog).is_err());
        let candidate = first
            .prepare_plugin_contributions(
                &commands,
                &catalog,
                &Default::default(),
                ContributionKeymapPolicy::RetainStartupFallback,
            )
            .unwrap();
        let revision = first.binding_revision();
        first.commit_contributions(candidate, &catalog).unwrap();
        assert_eq!(first.binding_revision(), revision + 1);
    }
}
