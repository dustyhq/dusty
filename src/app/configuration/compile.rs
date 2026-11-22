//! Keymap compilation with bundled, plugin and user precedence.

use crate::app::commands::AppEffect;
use crate::app::configuration::KeymapInspection;
use crate::app::configuration::KeymapOperation;
use crate::app::configuration::ResolvedKeyGroup;
use crate::app::configuration::ResolvedKeymaps;
use crate::app::configuration::ReverseBinding;
use crate::app::configuration::configured_context;
use crate::app::configuration::invocation_fingerprint;
use crate::app::configuration::keymap_entries;
use crate::app::configuration::plugin_owner_from_id;
use crate::editor::ArgumentType;
use crate::editor::CommandError;
use crate::editor::CommandId;
use crate::editor::CommandInvocation;
use crate::editor::CommandRegistry;
use crate::editor::CommandValue;
use crate::editor::InvocationSource;
use crate::editor::input::InputContextError;
use crate::editor::input::InputContextLayer;
use crate::editor::input::InputContexts;
use crate::editor::input::KeyEvent;
use crate::editor::input::KeymapLayer;
use crate::editor::input::KeymapSequence;
use crate::editor::input::MappingResolution;
use crate::plugins::PluginContributionCatalog;
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

#[cfg(test)]
pub(crate) fn compile_keymaps(
    commands: &CommandRegistry<AppEffect>,
    loaded: &crate::config::LoadedKeymaps,
    policy: &crate::config::KeymapPolicyConfig,
) -> (ResolvedKeymaps, Option<String>) {
    match compile_keymaps_with_fallback(commands, loaded, policy) {
        Ok((keymaps, None)) => (keymaps, None),
        Ok((mut keymaps, Some(fallback))) => {
            let notice = fallback.diagnostics.fallback_notice(
                &loaded.path,
                &loaded.path.with_file_name("config.toml"),
                &BTreeMap::new(),
            );
            keymaps.conflicts.push(notice.detail);
            (keymaps, Some(notice.summary))
        }
        Err(errors) => (
            ResolvedKeymaps::default(),
            Some(errors.describe(&loaded.path, &loaded.path, &BTreeMap::new())),
        ),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::app) struct KeymapNotice {
    pub(in crate::app) summary: String,
    pub(in crate::app) detail: String,
    history: Vec<String>,
}

impl KeymapNotice {
    fn new(summary: String, detail: String) -> Self {
        Self {
            summary: format!("Using default keys. {summary}"),
            history: vec![detail.clone()],
            detail,
        }
    }

    pub(super) fn skipped(messages: Vec<String>) -> Option<Self> {
        if messages.is_empty() {
            return None;
        }
        let message = messages.join("; ");
        Some(Self {
            summary: message.clone(),
            detail: message,
            history: messages,
        })
    }

    pub(super) fn combine(first: Option<Self>, second: Option<Self>) -> Option<Self> {
        match (first, second) {
            (Some(first), Some(second)) => Some(Self {
                summary: format!("{}; {}", first.summary, second.summary),
                detail: format!("{}; {}", first.detail, second.detail),
                history: first.history.into_iter().chain(second.history).collect(),
            }),
            (first, second) => first.or(second),
        }
    }

    pub(in crate::app) fn new_history_entries<'a>(
        &'a self,
        previous: Option<&'a Self>,
    ) -> impl Iterator<Item = &'a String> {
        self.history
            .iter()
            .filter(move |entry| previous.is_none_or(|notice| !notice.history.contains(entry)))
    }

    pub(super) fn from_read_error(path: &std::path::Path, detail: &str) -> Self {
        Self::new(shorten_file_location(path, detail), detail.to_owned())
    }
}

fn short_file_label(path: &std::path::Path) -> std::borrow::Cow<'_, str> {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
}

pub(super) fn shorten_file_location(path: &std::path::Path, detail: &str) -> String {
    detail
        .strip_prefix(path.to_string_lossy().as_ref())
        .map_or_else(
            || detail.to_owned(),
            |rest| format!("{}{rest}", short_file_label(path)),
        )
}

#[derive(Clone, Debug)]
struct KeymapError {
    reason: String,
    line: Option<usize>,
    source: Option<String>,
    skip: Option<SkippedDeclaration>,
    shortcut_notice: Option<String>,
}

#[derive(Clone, Debug)]
struct SkippedDeclaration {
    origin: KeymapContributionOrigin,
    message: String,
}

#[derive(Debug, Default)]
pub(crate) struct KeymapErrors {
    errors: Vec<KeymapError>,
    policy_key: Option<&'static str>,
}

impl KeymapErrors {
    fn from_reason(reason: String) -> Self {
        Self::from_source(reason, None)
    }

    fn from_source(reason: String, source: Option<String>) -> Self {
        Self {
            errors: vec![KeymapError {
                reason,
                line: None,
                source,
                skip: None,
                shortcut_notice: None,
            }],
            policy_key: None,
        }
    }

    fn at(&mut self, origin: &KeymapContributionOrigin, reason: String) {
        self.at_with_notice(origin, reason, None);
    }

    fn at_with_notice(
        &mut self,
        origin: &KeymapContributionOrigin,
        reason: String,
        skip_message: Option<String>,
    ) {
        self.errors.push(KeymapError {
            reason,
            line: origin.line,
            source: (origin.layer != InputContextLayer::User).then(|| origin.source.clone()),
            skip: skip_message.map(|message| SkippedDeclaration {
                origin: origin.clone(),
                message,
            }),
            shortcut_notice: None,
        });
    }

    pub(super) fn shortcut_notice(&self) -> Option<&str> {
        self.errors
            .iter()
            .find_map(|error| error.shortcut_notice.as_deref())
    }

    fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    fn render(
        &self,
        keymaps: &std::path::Path,
        settings: &std::path::Path,
        settings_lines: &BTreeMap<String, usize>,
        short: bool,
    ) -> String {
        let path = if self.policy_key.is_some() {
            settings
        } else {
            keymaps
        };
        let file = if short {
            short_file_label(path)
        } else {
            path.to_string_lossy()
        };
        let policy_line = self
            .policy_key
            .and_then(|key| settings_lines.get(key).copied());
        self.errors
            .iter()
            .map(|error| {
                let file = match error.source.as_deref() {
                    Some(source) if short => short_file_label(std::path::Path::new(source)),
                    Some(source) => source.into(),
                    None => file.clone(),
                };
                let reason = &error.reason;
                match policy_line.or(error.line) {
                    Some(line) => format!("{file}, line {line}: {reason}"),
                    None => format!("{file}: {reason}"),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub(super) fn describe(
        &self,
        keymaps: &std::path::Path,
        settings: &std::path::Path,
        settings_lines: &BTreeMap<String, usize>,
    ) -> String {
        self.render(keymaps, settings, settings_lines, false)
    }

    pub(super) fn describe_short(
        &self,
        keymaps: &std::path::Path,
        settings: &std::path::Path,
        settings_lines: &BTreeMap<String, usize>,
    ) -> String {
        self.render(keymaps, settings, settings_lines, true)
    }

    pub(super) fn fallback_notice(
        &self,
        keymaps: &std::path::Path,
        settings: &std::path::Path,
        settings_lines: &BTreeMap<String, usize>,
    ) -> KeymapNotice {
        KeymapNotice::new(
            self.describe_short(keymaps, settings, settings_lines),
            self.describe(keymaps, settings, settings_lines),
        )
    }
}

pub(super) struct KeymapFallback {
    pub(super) diagnostics: KeymapErrors,
    pub(super) policy: Option<crate::config::KeymapPolicyConfig>,
}

#[cfg(test)]
fn compile_keymaps_with_fallback(
    commands: &CommandRegistry<AppEffect>,
    loaded: &crate::config::LoadedKeymaps,
    policy: &crate::config::KeymapPolicyConfig,
) -> Result<(ResolvedKeymaps, Option<KeymapFallback>), KeymapErrors> {
    let source = KeymapCompilation::new(loaded, policy);
    let diagnostics = match compile_keymap_entries(commands, source) {
        Ok(keymaps) => return Ok((keymaps, None)),
        Err(diagnostics) => diagnostics,
    };
    compile_startup_fallback(commands, source, diagnostics)
        .map(|(keymaps, fallback)| (keymaps, Some(fallback)))
}

pub(super) fn compile_startup_fallback(
    commands: &CommandRegistry<AppEffect>,
    source: KeymapCompilation<'_>,
    diagnostics: KeymapErrors,
) -> Result<(ResolvedKeymaps, KeymapFallback), KeymapErrors> {
    if source.bundled_count == 0 && source.bundled_group_count == 0 {
        return Err(diagnostics);
    }
    let bundled = source.bundled_only();
    let (keymaps, fallback) = match compile_keymap_entries(commands, bundled) {
        Ok(keymaps) => (
            keymaps,
            KeymapFallback {
                diagnostics,
                policy: None,
            },
        ),
        Err(diagnostics) => {
            let policy = crate::config::load_bundled_config().config.input.keymaps;
            let keymaps = compile_keymap_entries(
                commands,
                KeymapCompilation {
                    policy: &policy,
                    ..bundled
                },
            )?;
            (
                keymaps,
                KeymapFallback {
                    diagnostics,
                    policy: Some(policy),
                },
            )
        }
    };
    Ok((keymaps, fallback))
}

pub(in crate::app) fn literal_key_sequence(
    name: &str,
    source: &str,
) -> Result<Vec<KeyEvent>, String> {
    let tokens = source.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if source.len() > 64 || tokens.len() > 4 {
        return Err(format!("{name} must contain at most 4 keys and 64 bytes"));
    }
    if source.chars().any(char::is_control) {
        return Err(format!("{name} must not contain control characters"));
    }
    if tokens
        .iter()
        .any(|token| matches!(*token, "<leader>" | "<localleader>"))
    {
        return Err(format!(
            "{name} must be a literal sequence without leader tokens"
        ));
    }
    tokens
        .into_iter()
        .map(|token| KeyEvent::from_str(token).map_err(|error| format!("{name}: {error}")))
        .collect()
}

pub(in crate::app) fn expanded_key_sequence(
    source: &str,
    leader: &[KeyEvent],
    local_leader: &[KeyEvent],
) -> Result<Vec<KeyEvent>, String> {
    let mut expanded = Vec::new();
    for token in source.split_whitespace() {
        match token {
            "<leader>" => expanded.extend_from_slice(leader),
            "<localleader>" => expanded.extend_from_slice(local_leader),
            _ => expanded.push(KeyEvent::from_str(token).map_err(|error| error.to_string())?),
        }
        if expanded.len() > 64 {
            return Err("expanded key sequence exceeds 64 keys".into());
        }
    }
    if expanded.is_empty() {
        Err("key sequence is empty".into())
    } else {
        Ok(expanded)
    }
}

pub(in crate::app) fn display_key_sequence(sequence: &[KeyEvent]) -> String {
    sequence
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(in crate::app) fn display_key_declaration(source: &str, effective: &str) -> String {
    if source == effective {
        format!("`{source}`")
    } else {
        format!("`{source}` (expanded to `{effective}`)")
    }
}

pub(in crate::app) fn validate_keymap_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err("id must contain between 1 and 128 bytes".into());
    }
    let segments = id.split('.').collect::<Vec<_>>();
    if segments.len() < 2
        || segments.iter().any(|segment| {
            let mut characters = segment.chars();
            !characters
                .next()
                .is_some_and(|first| first.is_ascii_lowercase())
                || characters.any(|character| {
                    !character.is_ascii_lowercase()
                        && !character.is_ascii_digit()
                        && character != '-'
                })
        })
    {
        return Err(
            "id must have at least two dot-separated lowercase ASCII segments; each segment starts with a letter and then uses letters, digits, or `-`"
                .into(),
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::app) struct KeymapContributionOrigin {
    layer: InputContextLayer,
    owner: Option<String>,
    plugin_load: u64,
    source: String,
    declaration: usize,
    span_start: Option<usize>,
    line: Option<usize>,
}

#[derive(Clone)]
pub(in crate::app) struct EffectiveKeymapDeclaration {
    id: String,
    context: String,
    mode: String,
    keys: String,
    command: Option<String>,
    arguments: BTreeMap<String, toml::Value>,
    operation: KeymapOperation,
    desc: Option<String>,
    origin: KeymapContributionOrigin,
}

pub(in crate::app) fn regular_keymap_declaration(
    entry: &crate::config::KeymapEntry,
    origin: KeymapContributionOrigin,
) -> Result<EffectiveKeymapDeclaration, String> {
    let operations = usize::from(entry.command.is_some())
        + usize::from(entry.unmap)
        + usize::from(entry.pass_through);
    if entry.disable || operations != 1 {
        return Err(
            "must choose one command, exact-sequence unmap, exact-sequence pass_through, or identity-only disable"
                .into(),
        );
    }
    if !entry.arguments.is_empty() && entry.command.is_none() {
        return Err("arguments require a command declaration".into());
    }
    let context = entry
        .context
        .as_ref()
        .filter(|context| !context.is_empty())
        .ok_or_else(|| "command/unmap/pass_through declaration requires `context`".to_owned())?;
    let keys = entry
        .keys
        .as_ref()
        .filter(|keys| !keys.is_empty())
        .ok_or_else(|| "command/unmap/pass_through declaration requires `keys`".to_owned())?;
    if entry.desc.as_ref().is_some_and(|desc| {
        desc.is_empty() || desc.chars().count() > 64 || desc.chars().any(char::is_control)
    }) {
        return Err("desc must contain 1..=64 non-control characters".into());
    }
    Ok(EffectiveKeymapDeclaration {
        id: entry.id.clone(),
        context: context.clone(),
        mode: entry.mode.clone(),
        keys: keys.clone(),
        command: entry.command.clone(),
        arguments: entry.arguments.clone(),
        operation: if entry.command.is_some() {
            KeymapOperation::Command
        } else if entry.unmap {
            KeymapOperation::Unmap
        } else {
            KeymapOperation::PassThrough
        },
        desc: entry.desc.clone(),
        origin,
    })
}

pub(in crate::app) fn identity_disable_is_well_formed(entry: &crate::config::KeymapEntry) -> bool {
    entry.disable
        && entry.command.is_none()
        && !entry.unmap
        && !entry.pass_through
        && entry.context.is_none()
        && entry.mode.is_empty()
        && entry.keys.is_none()
        && entry.arguments.is_empty()
        && entry.desc.is_none()
}

pub(in crate::app) fn resolve_keymap_identities(
    entries: &[crate::config::KeymapEntry],
    origins: &[KeymapContributionOrigin],
) -> Result<Vec<EffectiveKeymapDeclaration>, KeymapErrors> {
    let mut diagnostics = KeymapErrors::default();
    let mut slots = Vec::<EffectiveKeymapDeclaration>::new();
    let mut identities = HashMap::<String, usize>::new();
    for (source_index, entry) in entries.iter().enumerate() {
        let origin = origins[source_index].clone();
        let declaration = origin.declaration;
        if let Err(error) = validate_keymap_id(&entry.id) {
            diagnostics.at(
                &origin,
                format!(
                    "keymap {} has invalid id `{}`: {error}",
                    declaration, entry.id
                ),
            );
            continue;
        }
        if let Some(slot) = identities.get(&entry.id).copied() {
            let previous = &slots[slot];
            if previous.origin.layer == origin.layer {
                let detail = if previous.origin.owner != origin.owner {
                    "across plugin owners"
                } else {
                    "in the same contribution layer"
                };
                diagnostics.at(
                    &origin,
                    format!(
                        "keymap {} duplicates id `{}` from keymap {} {detail}",
                        declaration, entry.id, previous.origin.declaration
                    ),
                );
                continue;
            }
            if previous.origin.layer > origin.layer {
                diagnostics.at(
                    &origin,
                    format!(
                        "keymap {} id `{}` has lower precedence than its existing contribution",
                        declaration, entry.id
                    ),
                );
                continue;
            }
            if identity_disable_is_well_formed(entry) {
                slots[slot] = EffectiveKeymapDeclaration {
                    id: entry.id.clone(),
                    context: previous.context.clone(),
                    mode: previous.mode.clone(),
                    keys: previous.keys.clone(),
                    command: None,
                    arguments: BTreeMap::new(),
                    operation: KeymapOperation::Disable,
                    desc: None,
                    origin,
                };
                continue;
            }
            if entry.disable {
                diagnostics.at(&origin, format!(
                    "keymap {} identity disable for id `{}` must contain only `id` and `disable = true`",
                    declaration,
                    entry.id
                ));
                continue;
            }
            match regular_keymap_declaration(entry, origin.clone()) {
                Ok(replacement) => slots[slot] = replacement,
                Err(error) => diagnostics.at(
                    &origin,
                    format!(
                        "keymap {} replacement for id `{}` {error}",
                        declaration, entry.id
                    ),
                ),
            }
            continue;
        }
        if entry.disable {
            let detail = if identity_disable_is_well_formed(entry) {
                "targets no lower contribution"
            } else {
                "must contain only `id` and `disable = true`"
            };
            diagnostics.at(
                &origin,
                format!(
                    "keymap {} identity disable for id `{}` {detail}",
                    declaration, entry.id
                ),
            );
            continue;
        }
        match regular_keymap_declaration(entry, origin.clone()) {
            Ok(declaration) => {
                identities.insert(entry.id.clone(), slots.len());
                slots.push(declaration);
            }
            Err(error) => diagnostics.at(
                &origin,
                format!("keymap {} id `{}` {error}", declaration, entry.id),
            ),
        }
    }
    if diagnostics.is_empty() {
        slots.sort_by_key(|entry| entry.origin.layer);
        Ok(slots)
    } else {
        Err(diagnostics)
    }
}

#[derive(Clone)]
pub(in crate::app) struct EffectiveKeyGroupDeclaration {
    id: String,
    context: String,
    mode: String,
    prefix: String,
    label: String,
    icon: Option<String>,
    order: i32,
    origin: KeymapContributionOrigin,
}

pub(in crate::app) fn concrete_key_group(
    entry: &crate::config::KeyGroupEntry,
    origin: KeymapContributionOrigin,
) -> Result<EffectiveKeyGroupDeclaration, String> {
    if entry.disable {
        return Err("identity-only disable must contain only `id` and `disable = true`".into());
    }
    let context = entry
        .context
        .as_ref()
        .filter(|context| !context.is_empty())
        .ok_or_else(|| "requires `context`".to_owned())?;
    let prefix = entry
        .prefix
        .as_ref()
        .filter(|prefix| !prefix.is_empty())
        .ok_or_else(|| "requires `prefix`".to_owned())?;
    let label = entry
        .label
        .as_ref()
        .filter(|label| !label.is_empty())
        .ok_or_else(|| "requires `label`".to_owned())?;
    if prefix.len() > 1_024 {
        return Err("prefix exceeds 1024 bytes".into());
    }
    if label.len() > 256
        || label.chars().count() > 128
        || unicode_width::UnicodeWidthStr::width(label.as_str()) > 128
        || label.chars().any(char::is_control)
    {
        return Err(
            "label must contain 1..=128 non-control characters and at most 256 bytes".into(),
        );
    }
    if entry.icon.as_ref().is_some_and(|icon| {
        icon.is_empty()
            || icon.len() > 32
            || icon.chars().count() > 8
            || !(1..=8).contains(&unicode_width::UnicodeWidthStr::width(icon.as_str()))
            || icon.chars().any(char::is_control)
    }) {
        return Err("icon must contain 1..=8 non-control characters and at most 32 bytes".into());
    }
    if !(-100_000..=100_000).contains(&entry.order) {
        return Err("order is outside -100000..=100000".into());
    }
    Ok(EffectiveKeyGroupDeclaration {
        id: entry.id.clone(),
        context: context.clone(),
        mode: entry.mode.clone(),
        prefix: prefix.clone(),
        label: label.clone(),
        icon: entry.icon.clone(),
        order: entry.order,
        origin,
    })
}

pub(in crate::app) fn key_group_disable_is_well_formed(
    entry: &crate::config::KeyGroupEntry,
) -> bool {
    entry.disable
        && entry.context.is_none()
        && entry.mode.is_empty()
        && entry.prefix.is_none()
        && entry.label.is_none()
        && entry.icon.is_none()
        && entry.order == 0
}

pub(in crate::app) fn resolve_key_group_identities(
    entries: &[crate::config::KeyGroupEntry],
    origins: &[KeymapContributionOrigin],
) -> Result<Vec<EffectiveKeyGroupDeclaration>, KeymapErrors> {
    let mut diagnostics = KeymapErrors::default();
    let mut slots = Vec::<Option<EffectiveKeyGroupDeclaration>>::new();
    let mut identities = HashMap::<String, usize>::new();
    let mut identity_origins = HashMap::<String, KeymapContributionOrigin>::new();
    for (source_index, entry) in entries.iter().enumerate() {
        let origin = origins[source_index].clone();
        let declaration = origin.declaration;
        if let Err(error) = validate_keymap_id(&entry.id) {
            diagnostics.at(
                &origin,
                format!(
                    "key group {} has invalid id `{}`: {error}",
                    declaration, entry.id
                ),
            );
            continue;
        }
        if let Some(slot) = identities.get(&entry.id).copied() {
            let previous = &identity_origins[&entry.id];
            if previous.layer == origin.layer {
                diagnostics.at(
                    &origin,
                    format!(
                        "key group {} duplicates id `{}` in the same contribution layer",
                        declaration, entry.id
                    ),
                );
                continue;
            }
            if previous.layer > origin.layer {
                diagnostics.at(
                    &origin,
                    format!(
                        "key group {} id `{}` has lower precedence than its existing contribution",
                        declaration, entry.id
                    ),
                );
                continue;
            }
            if entry.disable {
                if key_group_disable_is_well_formed(entry) {
                    slots[slot] = None;
                    identity_origins.insert(entry.id.clone(), origin);
                } else {
                    diagnostics.at(&origin, format!(
                        "key group {} identity disable for id `{}` must contain only `id` and `disable = true`",
                        declaration,
                        entry.id
                    ));
                }
                continue;
            }
            match concrete_key_group(entry, origin.clone()) {
                Ok(replacement) => {
                    slots[slot] = Some(replacement);
                    identity_origins.insert(entry.id.clone(), origin);
                }
                Err(error) => diagnostics.at(
                    &origin,
                    format!(
                        "key group {} replacement for id `{}` {error}",
                        declaration, entry.id
                    ),
                ),
            }
            continue;
        }
        if entry.disable {
            let detail = if key_group_disable_is_well_formed(entry) {
                "targets no lower contribution"
            } else {
                "must contain only `id` and `disable = true`"
            };
            diagnostics.at(
                &origin,
                format!(
                    "key group {} identity disable for id `{}` {detail}",
                    declaration, entry.id
                ),
            );
            continue;
        }
        match concrete_key_group(entry, origin.clone()) {
            Ok(group) => {
                identities.insert(entry.id.clone(), slots.len());
                identity_origins.insert(entry.id.clone(), origin);
                slots.push(Some(group));
            }
            Err(error) => diagnostics.at(
                &origin,
                format!("key group {} id `{}` {error}", declaration, entry.id),
            ),
        }
    }
    if diagnostics.is_empty() {
        Ok(slots.into_iter().flatten().collect())
    } else {
        Err(diagnostics)
    }
}

pub(in crate::app) fn standard_contribution_origins(
    count: usize,
    bundled_count: usize,
    user_path: &std::path::Path,
    user_spans: &[std::ops::Range<usize>],
    user_lines: &BTreeMap<usize, usize>,
    kind: &str,
) -> Vec<KeymapContributionOrigin> {
    (0..count)
        .map(|index| {
            if index < bundled_count {
                KeymapContributionOrigin {
                    layer: InputContextLayer::Bundled,
                    owner: None,
                    plugin_load: 0,
                    source: format!("<bundled:keymaps>:{kind} {}", index + 1),
                    declaration: index + 1,
                    span_start: None,
                    line: None,
                }
            } else {
                let declaration = index + 1 - bundled_count;
                let source = user_spans.get(declaration - 1).map_or_else(
                    || format!("{}:{kind} {declaration}", user_path.display()),
                    |span| {
                        format!(
                            "{}:bytes {}..{} ({kind} {declaration})",
                            user_path.display(),
                            span.start,
                            span.end
                        )
                    },
                );
                KeymapContributionOrigin {
                    layer: InputContextLayer::User,
                    owner: None,
                    plugin_load: 0,
                    source,
                    declaration,
                    span_start: user_spans.get(declaration - 1).map(|span| span.start),
                    line: user_spans
                        .get(declaration - 1)
                        .and_then(|span| user_lines.get(&span.start).copied()),
                }
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(in crate::app) struct KeymapCompilation<'a> {
    pub(crate) entries: &'a [crate::config::KeymapEntry],
    pub(crate) bundled_count: usize,
    pub(crate) groups: &'a [crate::config::KeyGroupEntry],
    pub(crate) bundled_group_count: usize,
    pub(crate) user_path: &'a std::path::Path,
    pub(crate) user_spans: &'a [std::ops::Range<usize>],
    pub(crate) user_group_spans: &'a [std::ops::Range<usize>],
    user_lines: &'a BTreeMap<usize, usize>,
    pub(crate) policy: &'a crate::config::KeymapPolicyConfig,
}

impl<'a> KeymapCompilation<'a> {
    pub(super) fn new(
        loaded: &'a crate::config::LoadedKeymaps,
        policy: &'a crate::config::KeymapPolicyConfig,
    ) -> Self {
        let (entries, bundled_count, groups, bundled_group_count) = keymap_entries(loaded);
        Self {
            entries,
            bundled_count,
            groups,
            bundled_group_count,
            user_path: &loaded.path,
            user_spans: &loaded.user_spans,
            user_group_spans: &loaded.user_group_spans,
            user_lines: &loaded.user_lines,
            policy,
        }
    }

    pub(super) fn bundled_only(self) -> Self {
        Self {
            entries: &self.entries[..self.bundled_count],
            groups: &self.groups[..self.bundled_group_count],
            user_spans: &[],
            user_group_spans: &[],
            ..self
        }
    }
}

pub(in crate::app) fn compile_keymap_entries(
    commands: &CommandRegistry<AppEffect>,
    source: KeymapCompilation<'_>,
) -> Result<ResolvedKeymaps, KeymapErrors> {
    let entry_origins = standard_contribution_origins(
        source.entries.len(),
        source.bundled_count,
        source.user_path,
        source.user_spans,
        source.user_lines,
        "declaration",
    );
    let group_origins = standard_contribution_origins(
        source.groups.len(),
        source.bundled_group_count,
        source.user_path,
        source.user_group_spans,
        source.user_lines,
        "key-group",
    );
    compile_typed_keymap_entries(
        commands,
        source.entries,
        &entry_origins,
        source.groups,
        &group_origins,
        &CompileEnvironment {
            contexts: &InputContexts::bundled(),
            policy: source.policy,
        },
        None,
    )
}

#[cfg(test)]
pub(crate) fn compile_keymaps_with_plugin_catalog(
    commands: &CommandRegistry<AppEffect>,
    loaded: &crate::config::LoadedKeymaps,
    catalog: &crate::plugins::PluginContributionCatalog,
    policy: &crate::config::KeymapPolicyConfig,
) -> Result<ResolvedKeymaps, KeymapErrors> {
    compile_catalog_keymaps(
        commands,
        KeymapCompilation::new(loaded, policy),
        catalog,
        &BTreeSet::new(),
        None,
    )
    .map(|(keys, _)| keys)
}

pub(super) fn compile_catalog_keymaps(
    commands: &CommandRegistry<AppEffect>,
    source: KeymapCompilation<'_>,
    catalog: &PluginContributionCatalog,
    pending_owners: &BTreeSet<String>,
    retained: Option<&ResolvedKeymaps>,
) -> Result<(ResolvedKeymaps, Option<KeymapNotice>), KeymapErrors> {
    let contexts = catalog
        .context_snapshot_with_source()
        .map_err(|(source, error)| {
            KeymapErrors::from_source(error.to_string(), Some(source.to_owned()))
        })?;
    let bundled_count = source.bundled_count;
    let bundled_group_count = source.bundled_group_count;
    let mut entries = source.entries[..source.bundled_count].to_vec();
    let mut entry_origins = standard_contribution_origins(
        bundled_count,
        bundled_count,
        source.user_path,
        &[],
        source.user_lines,
        "declaration",
    );
    let (leader, local_leader) = parse_leaders(source.policy)?;
    let mut plugins = catalog
        .ordered()
        .flat_map(|index| {
            index
                .keymaps
                .iter()
                .enumerate()
                .map(move |(offset, entry)| (index, offset, entry))
        })
        .collect::<Vec<_>>();
    if let Some(retained) = retained {
        // A new or changed shortcut must not displace an existing one. At
        // startup, ties fall back to the catalog's stable order.
        plugins.sort_by_cached_key(|(_, _, entry)| {
            let context = configured_context(&contexts, &entry.context, &entry.mode);
            let keys = expanded_key_sequence(&entry.keys, &leader, &local_leader)
                .map(|keys| display_key_sequence(&keys));
            !retained.inspections.iter().any(|binding| {
                binding.contribution_layer == InputContextLayer::Plugin
                    && binding.id == entry.id
                    && context
                        .as_ref()
                        .is_ok_and(|context| binding.context == *context)
                    && keys.as_ref().is_ok_and(|keys| binding.keys == *keys)
            })
        });
    }
    for (index, offset, entry) in plugins {
        entries.push(crate::config::KeymapEntry {
            id: entry.id.clone(),
            context: Some(entry.context.clone()),
            mode: entry.mode.clone(),
            keys: Some(entry.keys.clone()),
            command: Some(entry.command.clone()),
            arguments: entry.arguments.clone(),
            unmap: false,
            disable: false,
            pass_through: false,
            desc: None,
        });
        entry_origins.push(KeymapContributionOrigin {
            layer: InputContextLayer::Plugin,
            owner: Some(index.owner.clone()),
            plugin_load: index.plugin_load,
            source: format!("{}:plugin-keymap {}", index.source, offset + 1),
            declaration: offset + 1,
            span_start: None,
            line: None,
        });
    }
    entries.extend_from_slice(&source.entries[source.bundled_count..]);
    entry_origins.extend(standard_contribution_origins(
        source.entries.len() - source.bundled_count,
        0,
        source.user_path,
        source.user_spans,
        source.user_lines,
        "declaration",
    ));
    let mut groups = source.groups[..source.bundled_group_count].to_vec();
    let mut group_origins = standard_contribution_origins(
        bundled_group_count,
        bundled_group_count,
        source.user_path,
        &[],
        source.user_lines,
        "key-group",
    );
    for index in catalog.ordered() {
        for (offset, group) in index.key_groups.iter().enumerate() {
            groups.push(crate::config::KeyGroupEntry {
                id: group.id.clone(),
                context: Some(group.context.clone()),
                mode: group.mode.clone(),
                prefix: Some(group.prefix.clone()),
                label: Some(group.label.clone()),
                icon: group.icon.clone(),
                order: group.order,
                disable: false,
            });
            group_origins.push(KeymapContributionOrigin {
                layer: InputContextLayer::Plugin,
                owner: Some(index.owner.clone()),
                plugin_load: index.plugin_load,
                source: format!("{}:plugin-key-group {}", index.source, offset + 1),
                declaration: offset + 1,
                span_start: None,
                line: None,
            });
        }
    }
    groups.extend_from_slice(&source.groups[source.bundled_group_count..]);
    group_origins.extend(standard_contribution_origins(
        source.groups.len() - source.bundled_group_count,
        0,
        source.user_path,
        source.user_group_spans,
        source.user_lines,
        "key-group",
    ));
    let environment = CompileEnvironment {
        contexts: &contexts,
        policy: source.policy,
    };
    let mut unavailable = UnavailableShortcuts::default();
    for (owner, entry) in catalog.inactive_keymaps() {
        if let (Ok(context), Ok(keys)) = (
            dependency_context(catalog, &contexts, &entry.context, &entry.mode),
            expanded_key_sequence(&entry.keys, &leader, &local_leader),
        ) {
            unavailable
                .continuations
                .push((context, keys, BTreeSet::from([owner.to_owned()])));
        }
    }
    let duplicates = duplicate_user_ids(
        entries
            .iter()
            .map(|entry| entry.id.as_str())
            .zip(&entry_origins),
    );
    let (mut entries, mut entry_origins): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .zip(entry_origins)
        .filter(|(entry, origin)| {
            if origin.layer != InputContextLayer::User || duplicates.contains(&entry.id) {
                return true;
            }
            let context = entry.context.as_deref().unwrap_or_default();
            let command = entry.command.as_deref().unwrap_or_default();
            // The ID is an override target, not a dependency. A disable is the
            // exception: it needs the lower-priority declaration to exist.
            let target = if entry.disable { entry.id.as_str() } else { "" };
            let mut owners = unavailable_owners(catalog, &[target, context]);
            if (owners.is_empty() && missing_plugin(catalog, command).is_none())
                || validate_keymap_id(&entry.id).is_err()
            {
                return true;
            }
            // Only park declarations whose validation needs an unavailable provider.
            if identity_disable_is_well_formed(entry) {
                unavailable.park("keymap", origin, &entry.id, owners);
                return false;
            }
            let Ok(declaration) = regular_keymap_declaration(entry, origin.clone()) else {
                return true;
            };
            let (Ok(context), Ok(keys)) = (
                dependency_context(catalog, &contexts, &declaration.context, &declaration.mode),
                expanded_key_sequence(&declaration.keys, &leader, &local_leader),
            ) else {
                return true;
            };
            if let Some(command) = declaration.command.as_deref() {
                match keymap_invocation(commands, command, &declaration.arguments) {
                    Ok(_) => {}
                    Err(KeymapInvocationError::MissingPluginCommand(id)) => {
                        let Some(owner) = missing_plugin(catalog, id.as_str()) else {
                            return true;
                        };
                        owners.insert(owner.to_owned());
                    }
                    Err(
                        KeymapInvocationError::Invalid(_)
                        | KeymapInvocationError::InvalidArguments { .. },
                    ) => return true,
                }
                if !owners.is_empty() {
                    unavailable
                        .continuations
                        .push((context, keys, owners.clone()));
                }
            }
            if owners.is_empty() {
                return true;
            }
            unavailable.park("keymap", origin, &declaration.keys, owners);
            false
        })
        .unzip();
    let duplicates = duplicate_user_ids(
        groups
            .iter()
            .map(|group| group.id.as_str())
            .zip(&group_origins),
    );
    let (mut groups, mut group_origins): (Vec<_>, Vec<_>) = groups
        .into_iter()
        .zip(group_origins)
        .filter(|(group, origin)| {
            if origin.layer != InputContextLayer::User || duplicates.contains(&group.id) {
                return true;
            }
            let target = if group.disable { group.id.as_str() } else { "" };
            let owners = unavailable_owners(
                catalog,
                &[target, group.context.as_deref().unwrap_or_default()],
            );
            if owners.is_empty() || validate_keymap_id(&group.id).is_err() {
                return true;
            }
            if key_group_disable_is_well_formed(group) {
                unavailable.park("group", origin, &group.id, owners);
                return false;
            }
            let Ok(declaration) = concrete_key_group(group, origin.clone()) else {
                return true;
            };
            if dependency_context(catalog, &contexts, &declaration.context, &declaration.mode)
                .is_err()
                || expanded_key_sequence(&declaration.prefix, &leader, &local_leader).is_err()
            {
                return true;
            }
            unavailable.park("group", origin, &declaration.prefix, owners);
            false
        })
        .unzip();
    let mut skipped = Vec::new();
    loop {
        let mut attempt = unavailable.clone();
        match compile_typed_keymap_entries(
            commands,
            &entries,
            &entry_origins,
            &groups,
            &group_origins,
            &environment,
            Some(&mut attempt),
        ) {
            Ok(keys) => {
                return Ok((
                    keys,
                    KeymapNotice::combine(
                        attempt.notice(source.user_path, pending_owners),
                        KeymapNotice::skipped(skipped),
                    ),
                ));
            }
            Err(errors) if retained.is_some() => {
                // Retry without the offending declaration, not its owner or
                // its layer. Recompilation can restore a lower-priority binding.
                let Some(SkippedDeclaration { origin, message }) =
                    errors.errors.iter().find_map(|error| error.skip.as_ref())
                else {
                    return Err(errors);
                };
                if let Some(index) = entry_origins
                    .iter()
                    .position(|candidate| candidate == origin)
                {
                    entries.remove(index);
                    entry_origins.remove(index);
                } else if let Some(index) = group_origins
                    .iter()
                    .position(|candidate| candidate == origin)
                {
                    groups.remove(index);
                    group_origins.remove(index);
                } else {
                    return Err(errors);
                }
                if !skipped.contains(message) {
                    skipped.push(message.clone());
                }
            }
            Err(errors) => return Err(errors),
        }
    }
}

struct CompileEnvironment<'a> {
    contexts: &'a InputContexts,
    policy: &'a crate::config::KeymapPolicyConfig,
}

#[derive(Clone)]
struct UnavailableShortcut {
    shortcut: String,
    line: Option<usize>,
    owners: BTreeSet<String>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ShortcutSource {
    Span(usize),
    Declaration(usize),
}

#[derive(Clone, Default)]
struct UnavailableShortcuts {
    declarations: BTreeMap<(&'static str, ShortcutSource), UnavailableShortcut>,
    continuations: Vec<(String, Vec<KeyEvent>, BTreeSet<String>)>,
}

impl UnavailableShortcuts {
    fn park(
        &mut self,
        kind: &'static str,
        origin: &KeymapContributionOrigin,
        shortcut: &str,
        owners: BTreeSet<String>,
    ) {
        // A global shortcut expands into many entries that share this source span.
        let source = origin.span_start.map_or(
            ShortcutSource::Declaration(origin.declaration),
            ShortcutSource::Span,
        );
        self.declarations
            .entry((kind, source))
            .or_insert_with(|| UnavailableShortcut {
                shortcut: shortcut.to_owned(),
                line: origin.line,
                owners: BTreeSet::new(),
            })
            .owners
            .extend(owners);
    }

    fn continuation_owners(&self, context: &str, prefix: &[KeyEvent]) -> BTreeSet<String> {
        self.continuations
            .iter()
            .filter(|(candidate, keys, _)| {
                candidate == context && keys.len() > prefix.len() && keys.starts_with(prefix)
            })
            .flat_map(|(_, _, owners)| owners.iter().cloned())
            .collect()
    }

    fn notice(
        self,
        path: &std::path::Path,
        pending_owners: &BTreeSet<String>,
    ) -> Option<KeymapNotice> {
        let mut summary = BTreeMap::<String, BTreeSet<String>>::new();
        let mut details = Vec::new();
        for UnavailableShortcut {
            shortcut,
            line,
            owners,
        } in self.declarations.into_values()
        {
            let location = line.map_or_else(
                || path.display().to_string(),
                |line| format!("{}, line {line}", path.display()),
            );
            for owner in owners
                .into_iter()
                .filter(|owner| !pending_owners.contains(owner))
            {
                details.push(format!(
                    "{location}: Skipped {shortcut}: plugin {owner} isn't available."
                ));
                summary.entry(owner).or_default().insert(shortcut.clone());
            }
        }
        if details.is_empty() {
            return None;
        }
        let summary = summary
            .into_iter()
            .map(|(owner, shortcuts)| {
                let shortcuts = shortcuts.into_iter().collect::<Vec<_>>().join(", ");
                format!("Skipped {shortcuts}: plugin {owner} isn't available.")
            })
            .collect::<Vec<_>>()
            .join(" ");
        Some(KeymapNotice {
            summary,
            detail: details.join("; "),
            history: details,
        })
    }
}

fn missing_plugin<'a>(catalog: &PluginContributionCatalog, id: &'a str) -> Option<&'a str> {
    let owner = plugin_owner_from_id(id)?;
    (id.split('.').count() >= 3 && catalog.get(owner).is_none()).then_some(owner)
}

fn duplicate_user_ids<'a>(
    ids: impl Iterator<Item = (&'a str, &'a KeymapContributionOrigin)>,
) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    ids.filter(|(_, origin)| origin.layer == InputContextLayer::User)
        .map(|(id, _)| id)
        .filter(|&id| !seen.insert(id))
        .map(str::to_owned)
        .collect()
}

fn dependency_context(
    catalog: &PluginContributionCatalog,
    contexts: &InputContexts,
    context: &str,
    mode: &str,
) -> Result<String, String> {
    if mode.is_empty()
        && is_plugin_context_id(context)
        && missing_plugin(catalog, context).is_some()
    {
        Ok(context.to_owned())
    } else {
        configured_context(contexts, context, mode)
    }
}

fn is_plugin_context_id(context: &str) -> bool {
    context.len() <= 128
        && context.starts_with("plugin.")
        && context.split('.').count() == 3
        && context.split('.').all(|segment| {
            segment.len() <= 64
                && segment
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        })
}

fn unavailable_owners(catalog: &PluginContributionCatalog, ids: &[&str]) -> BTreeSet<String> {
    ids.iter()
        .filter_map(|id| missing_plugin(catalog, id).map(str::to_owned))
        .collect()
}

fn parse_leaders(
    policy: &crate::config::KeymapPolicyConfig,
) -> Result<(Vec<KeyEvent>, Vec<KeyEvent>), KeymapErrors> {
    let parse = |key: &'static str, value: &str| {
        literal_key_sequence(key, value).map_err(|reason| KeymapErrors {
            policy_key: Some(key),
            ..KeymapErrors::from_reason(reason)
        })
    };
    Ok((
        parse("input.keymaps.leader", &policy.leader)?,
        parse("input.keymaps.local_leader", &policy.local_leader)?,
    ))
}

enum KeymapInvocationError {
    Invalid(String),
    MissingPluginCommand(CommandId),
    InvalidArguments {
        diagnostic: String,
        explanation: String,
    },
}

fn keymap_invocation(
    commands: &CommandRegistry<AppEffect>,
    command_id: &str,
    arguments: &BTreeMap<String, toml::Value>,
) -> Result<CommandInvocation, KeymapInvocationError> {
    let mut invocation = CommandInvocation::new(command_id, InvocationSource::Keymap)
        .map_err(|error| KeymapInvocationError::Invalid(error.to_string()))?;
    for (name, value) in arguments {
        let value = match value {
            toml::Value::String(value) => CommandValue::String(value.clone()),
            toml::Value::Boolean(value) => CommandValue::Bool(*value),
            toml::Value::Integer(value) => CommandValue::Integer(*value),
            _ => {
                return Err(KeymapInvocationError::Invalid(format!(
                    "argument `{name}` must be string, boolean, or integer"
                )));
            }
        };
        invocation.arguments.insert(name.clone(), value);
    }
    commands
        .validate_invocation_shape(&invocation)
        .map_err(|error| match error {
            CommandError::Unknown(id)
                if super::validation::plugin_command_owner(id.as_str()).is_some() =>
            {
                KeymapInvocationError::MissingPluginCommand(id)
            }
            error @ CommandError::InvalidArguments(_)
                if super::validation::plugin_command_owner(command_id).is_some() =>
            {
                let boolean_argument = commands
                    .specs()
                    .find(|spec| spec.id == invocation.id)
                    .and_then(|spec| {
                        invocation.arguments.iter().find_map(|(name, value)| {
                            (spec.arguments.fields.get(name)?.kind == ArgumentType::Bool
                                && matches!(value, CommandValue::String(_)))
                            .then_some(name)
                        })
                    });
                let explanation = match boolean_argument {
                    Some(name) => {
                        format!(
                            "passes text to a command that now expects true/false for \"{name}\""
                        )
                    }
                    None => format!("passes arguments that no longer match \"{command_id}\""),
                };
                KeymapInvocationError::InvalidArguments {
                    diagnostic: error.to_string(),
                    explanation,
                }
            }
            error => KeymapInvocationError::Invalid(error.to_string()),
        })?;
    Ok(invocation)
}

fn compile_typed_keymap_entries(
    commands: &CommandRegistry<AppEffect>,
    entries: &[crate::config::KeymapEntry],
    entry_origins: &[KeymapContributionOrigin],
    groups: &[crate::config::KeyGroupEntry],
    group_origins: &[KeymapContributionOrigin],
    environment: &CompileEnvironment<'_>,
    mut unavailable: Option<&mut UnavailableShortcuts>,
) -> Result<ResolvedKeymaps, KeymapErrors> {
    let contexts = environment.contexts;
    let mut result = ResolvedKeymaps::default();
    let mut diagnostics = KeymapErrors::default();
    // A key declared twice meets again in every context it expands to; say so once.
    let mut key_conflicts = Vec::<(
        String,
        Vec<String>,
        KeymapContributionOrigin,
        Option<String>,
    )>::new();
    if entries.len() != entry_origins.len() || groups.len() != group_origins.len() {
        return Err(KeymapErrors::from_reason(
            "keymap contribution provenance is incomplete".into(),
        ));
    }
    let (leader, local_leader) = parse_leaders(environment.policy)?;
    let effective_entries = resolve_keymap_identities(entries, entry_origins)?;
    let mut sequence_declarations =
        HashMap::<(String, Vec<KeyEvent>, InputContextLayer), usize>::new();
    let mut effective_declarations = HashMap::<(String, Vec<KeyEvent>), usize>::new();
    for (index, entry) in effective_entries.iter().enumerate() {
        let context = match contexts.resolve(&entry.context, &entry.mode) {
            Ok(context) => context,
            Err(error) => {
                let reason = format!(
                    "keymap id `{}` from declaration {}: {error}",
                    entry.id, entry.origin.declaration
                );
                // A missing context must not hide independently invalid keys or arguments.
                let missing_context = matches!(error, InputContextError::Unknown(_))
                    && is_plugin_context_id(&entry.context)
                    && expanded_key_sequence(&entry.keys, &leader, &local_leader).is_ok()
                    && entry.command.as_deref().is_none_or(|command| {
                        matches!(
                            keymap_invocation(commands, command, &entry.arguments),
                            Ok(_) | Err(KeymapInvocationError::MissingPluginCommand(_))
                        )
                    });
                let notice = missing_context.then(|| {
                    format!(
                        "Skipped shortcut \"{}\": input context \"{}\" doesn't exist.",
                        entry.id, entry.context
                    )
                });
                diagnostics.at_with_notice(&entry.origin, reason, notice);
                continue;
            }
        };
        let sequence = match expanded_key_sequence(&entry.keys, &leader, &local_leader) {
            Ok(sequence) => sequence,
            Err(error) => {
                diagnostics.at(
                    &entry.origin,
                    format!(
                        "keymap id `{}` from declaration {}: {error}",
                        entry.id, entry.origin.declaration
                    ),
                );
                continue;
            }
        };
        let effective_keys = display_key_sequence(&sequence);
        let contribution_layer = entry.origin.layer;
        if let Some(previous) =
            effective_declarations.insert((context.clone(), sequence.clone()), index)
            && effective_entries[previous].origin.layer != contribution_layer
        {
            let declared = display_key_declaration(&entry.keys, &effective_keys);
            let previous_entry = &effective_entries[previous];
            let previous_declared = display_key_declaration(&previous_entry.keys, &effective_keys);
            result.conflicts.push(format!(
                "keymap id `{}` {declared} shadows id `{}` {previous_declared} in `{context}`",
                entry.id, previous_entry.id
            ));
        }
        if let Some(previous) = sequence_declarations.insert(
            (context.clone(), sequence.clone(), contribution_layer),
            index,
        ) {
            let declared = display_key_declaration(&entry.keys, &effective_keys);
            let previous_entry = &effective_entries[previous];
            let previous_declared = display_key_declaration(&previous_entry.keys, &effective_keys);
            // A `global` entry is one entry per context, and each of those ids ends in its context.
            let suffix = format!(".{}", context.replace(['.', '_'], "-"));
            let base = |id: &'_ str| id.strip_suffix(&suffix).unwrap_or(id).to_owned();
            let conflict = format!(
                "keymap id `{}` {declared} conflicts with id `{}` {previous_declared}",
                base(&entry.id),
                base(&previous_entry.id)
            );
            match key_conflicts
                .iter_mut()
                .find(|(text, _, _, _)| *text == conflict)
            {
                Some((_, contexts, _, _)) => contexts.push(context.clone()),
                None => {
                    let notice = (contribution_layer == InputContextLayer::Plugin).then(|| {
                        format!(
                            "Skipped shortcut \"{}\" ({effective_keys}); it conflicts with \"{}\".",
                            entry.id, previous_entry.id
                        )
                    });
                    key_conflicts.push((
                        conflict,
                        vec![context.clone()],
                        entry.origin.clone(),
                        notice,
                    ));
                }
            }
            continue;
        }
        if contribution_layer != InputContextLayer::Bundled
            && entry.operation != KeymapOperation::PassThrough
        {
            result.inspections.retain(|inspection| {
                inspection.context != context || inspection.keys != effective_keys
            });
        }
        let source = entry.origin.source.clone();
        let layer = match contribution_layer {
            InputContextLayer::Bundled => KeymapLayer::Global,
            InputContextLayer::Plugin => KeymapLayer::PluginDefaults,
            InputContextLayer::User => KeymapLayer::Mode,
        };
        let keymap = result
            .bindings
            .entry(context.clone())
            .or_default()
            .layer_mut(layer);
        if matches!(
            entry.operation,
            KeymapOperation::Unmap | KeymapOperation::Disable
        ) {
            keymap.insert_disabled(&sequence);
            result.inspections.retain(|inspection| {
                inspection.context != context || inspection.keys != effective_keys
            });
            result.inspections.push(KeymapInspection {
                id: entry.id.clone(),
                context,
                contribution_layer: entry.origin.layer,
                contribution_owner: entry.origin.owner.clone(),
                plugin_load: entry.origin.plugin_load,
                operation: entry.operation,
                command: None,
                arguments: BTreeMap::new(),
                source_keys: entry.keys.clone(),
                keys: effective_keys,
                desc: None,
                source,
            });
            continue;
        }
        if entry.operation == KeymapOperation::PassThrough {
            keymap.insert_pass_through(&sequence);
            result.inspections.push(KeymapInspection {
                id: entry.id.clone(),
                context,
                contribution_layer: entry.origin.layer,
                contribution_owner: entry.origin.owner.clone(),
                plugin_load: entry.origin.plugin_load,
                operation: entry.operation,
                command: None,
                arguments: BTreeMap::new(),
                source_keys: entry.keys.clone(),
                keys: effective_keys,
                desc: None,
                source,
            });
            continue;
        }
        let command_id = entry
            .command
            .as_deref()
            .expect("identity resolver guarantees command operation");
        let invocation = match keymap_invocation(commands, command_id, &entry.arguments) {
            Ok(invocation) => invocation,
            Err(KeymapInvocationError::MissingPluginCommand(command)) => {
                let notice = format!(
                    "Skipped shortcut \"{}\": unknown command \"{command}\".",
                    entry.id,
                );
                let reason = format!(
                    "keymap id `{}`: {}",
                    entry.id,
                    CommandError::Unknown(command)
                );
                diagnostics.at_with_notice(&entry.origin, reason, Some(notice));
                continue;
            }
            Err(KeymapInvocationError::Invalid(error)) => {
                diagnostics.at(&entry.origin, format!("keymap id `{}`: {error}", entry.id));
                continue;
            }
            Err(KeymapInvocationError::InvalidArguments {
                diagnostic,
                explanation,
            }) => {
                diagnostics.errors.push(KeymapError {
                    reason: format!("keymap id `{}`: {diagnostic}", entry.id),
                    line: entry.origin.line,
                    source: (entry.origin.layer != InputContextLayer::User)
                        .then(|| entry.origin.source.clone()),
                    skip: None,
                    shortcut_notice: (entry.origin.layer == InputContextLayer::User)
                        .then(|| format!("Shortcut \"{effective_keys}\" {explanation}.")),
                });
                continue;
            }
        };
        result.inspections.push(KeymapInspection {
            id: entry.id.clone(),
            context: context.clone(),
            contribution_layer: entry.origin.layer,
            contribution_owner: entry.origin.owner.clone(),
            plugin_load: entry.origin.plugin_load,
            operation: KeymapOperation::Command,
            command: Some(invocation.id.as_str().into()),
            arguments: invocation.arguments.clone(),
            source_keys: entry.keys.clone(),
            keys: effective_keys.clone(),
            desc: entry.desc.clone(),
            source,
        });
        keymap.insert(&sequence, invocation);
    }

    if let Ok(effective_groups) = resolve_key_group_identities(groups, group_origins)
        .inspect_err(|errors| diagnostics.errors.extend(errors.errors.iter().cloned()))
    {
        let mut group_prefixes =
            HashMap::<(String, Vec<KeyEvent>), (String, KeymapContributionOrigin)>::new();
        for group in effective_groups {
            let context = match contexts.resolve(&group.context, &group.mode) {
                Ok(context) => context,
                Err(error) => {
                    let reason = format!(
                        "key group id `{}` from declaration {}: {error}",
                        group.id, group.origin.declaration
                    );
                    let missing_context = matches!(error, InputContextError::Unknown(_))
                        && is_plugin_context_id(&group.context)
                        && expanded_key_sequence(&group.prefix, &leader, &local_leader).is_ok();
                    let notice = missing_context.then(|| {
                        format!(
                            "Skipped key group \"{}\": input context \"{}\" doesn't exist.",
                            group.id, group.context
                        )
                    });
                    diagnostics.at_with_notice(&group.origin, reason, notice);
                    continue;
                }
            };
            let sequence = match expanded_key_sequence(&group.prefix, &leader, &local_leader) {
                Ok(sequence) => sequence,
                Err(error) => {
                    diagnostics.at(
                        &group.origin,
                        format!(
                            "key group id `{}` from declaration {}: {error}",
                            group.id, group.origin.declaration
                        ),
                    );
                    continue;
                }
            };
            let effective_prefix = display_key_sequence(&sequence);
            let group_key = (context.clone(), sequence.clone());
            if let Some((previous, previous_origin)) = group_prefixes.get(&group_key) {
                // The plugin is the loser in either declaration order. The retry
                // can then keep the user's group without falling back to bundled keys.
                let (skipped, origin, kept) = if previous_origin.layer == InputContextLayer::Plugin
                    && group.origin.layer == InputContextLayer::User
                {
                    (previous, previous_origin, &group.id)
                } else {
                    (&group.id, &group.origin, previous)
                };
                diagnostics.at_with_notice(
                    origin,
                    format!(
                        "key group id `{skipped}` prefix {effective_prefix} duplicates id `{kept}` in `{context}`",
                    ),
                    (origin.layer == InputContextLayer::Plugin).then(|| format!(
                        "Skipped key group \"{skipped}\" ({effective_prefix}); it conflicts with \"{kept}\"."
                    )),
                );
                continue;
            }
            let reachable = result.inspections.iter().any(|inspection| {
                if inspection.context != context || inspection.command.is_none() {
                    return false;
                }
                inspection
                    .keys
                    .split_whitespace()
                    .map(KeyEvent::from_str)
                    .collect::<Result<Vec<_>, _>>()
                    .is_ok_and(|binding| {
                        binding.len() > sequence.len() && binding.starts_with(&sequence)
                    })
            });
            if !reachable {
                if group.origin.layer == InputContextLayer::User
                    && let Some(unavailable) = unavailable.as_deref_mut()
                {
                    let owners = unavailable.continuation_owners(&context, &sequence);
                    if !owners.is_empty() {
                        unavailable.park("group", &group.origin, &group.prefix, owners);
                        continue;
                    }
                }
                diagnostics.at(
                    &group.origin,
                    format!(
                        "key group id `{}`: no shortcut starts with {} in `{context}`",
                        group.id,
                        display_key_declaration(&group.prefix, &effective_prefix)
                    ),
                );
                continue;
            }
            group_prefixes.insert(group_key, (group.id.clone(), group.origin.clone()));
            let source = group.origin.source.clone();
            result.groups.push(ResolvedKeyGroup {
                id: group.id,
                context,
                contribution_layer: group.origin.layer,
                contribution_owner: group.origin.owner,
                plugin_load: group.origin.plugin_load,
                source_prefix: group.prefix,
                prefix: effective_prefix,
                label: group.label,
                icon: group.icon,
                order: group.order,
                source,
            });
        }
        result.groups.sort_by(|left, right| {
            left.context
                .cmp(&right.context)
                .then_with(|| left.order.cmp(&right.order))
                .then_with(|| left.prefix.cmp(&right.prefix))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
    // Replay the actual layered resolver: an exact command or disabled ancestor can
    // make a declaration unreachable even when its own sequence was not overridden.
    result.inspections.retain(|inspection| {
        let sequence = inspection
            .keys
            .split_whitespace()
            .map(|key| KeyEvent::from_str(key).expect("compiled key sequence"))
            .collect::<Vec<_>>();
        let bindings = &result.bindings[&inspection.context];
        let mut cursor = KeymapSequence::default();
        for (index, key) in sequence.iter().enumerate() {
            let mut resolution = cursor.feed(bindings, key.clone());
            if index + 1 < sequence.len() {
                if !matches!(resolution, MappingResolution::Pending | MappingResolution::Ambiguous) {
                    return false;
                }
                continue;
            }
            let Some(command) = &inspection.command else {
                return true;
            };
            if resolution == MappingResolution::Ambiguous {
                resolution = cursor.flush_timeout();
            }
            return matches!(resolution, MappingResolution::Ready(invocation)
                if invocation.id.as_str() == command && invocation.arguments == inspection.arguments);
        }
        false
    });
    result.groups.retain(|group| {
        result.inspections.iter().any(|inspection| {
            inspection.context == group.context
                && inspection.command.is_some()
                && inspection
                    .keys
                    .strip_prefix(&group.prefix)
                    .is_some_and(|suffix| suffix.starts_with(' '))
        })
    });
    // Choose presentation bindings only after all overrides and reachability checks,
    // so overriding the preferred shortcut can fall back to another working one.
    for inspection in &result.inspections {
        let Some(command) = &inspection.command else {
            continue;
        };
        let mut invocation =
            CommandInvocation::new(command, InvocationSource::Keymap).expect("validated command");
        invocation.arguments = inspection.arguments.clone();
        let key = (
            inspection.context.clone(),
            invocation_fingerprint(&invocation),
        );
        let replaces = result.reverse.get(&key).is_none_or(|existing| {
            inspection.contribution_layer > existing.layer
                || (inspection.contribution_layer == existing.layer
                    && inspection.keys.split_whitespace().count()
                        < existing.keys.split_whitespace().count())
        });
        if replaces {
            result.reverse.insert(
                key,
                ReverseBinding {
                    id: inspection.id.clone(),
                    keys: inspection.keys.clone(),
                    layer: inspection.contribution_layer,
                },
            );
        }
    }
    for (conflict, contexts, origin, notice) in key_conflicts {
        let reason = match contexts.as_slice() {
            [only] => format!("{conflict} in `{only}`"),
            [first, rest @ ..] => {
                format!("{conflict} in `{first}` and {} other contexts", rest.len())
            }
            [] => conflict,
        };
        diagnostics.at_with_notice(&origin, reason, notice);
    }
    if diagnostics.is_empty() {
        Ok(result)
    } else {
        Err(diagnostics)
    }
}
