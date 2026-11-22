//! Command references and typed arguments in configuration declarations.

use crate::app::commands::AppEffect;
use crate::config::LoadedConfig;
use crate::editor::CommandError;
use crate::editor::CommandId;
use crate::editor::CommandInvocation;
use crate::editor::CommandRegistry;
use crate::editor::CommandValue;
use crate::editor::InvocationSource;
use std::collections::BTreeSet;

pub(in crate::app) fn validate_configured_commands(
    commands: &CommandRegistry<AppEffect>,
    loaded: &LoadedConfig,
    allow_unavailable_plugin_labels: bool,
) -> Result<(), String> {
    let mut errors = Vec::new();
    let command_ids = commands
        .specs()
        .map(|spec| spec.id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    for spec in commands.specs() {
        match loaded.config.command_labels.get(spec.id.as_str()) {
            Some(label)
                if !label.is_empty() && !label.contains('\u{1b}') && label.len() <= 1_024 => {}
            Some(_) => errors.push(format!(
                "command label for `{}` is empty, unsafe, or oversized",
                spec.id
            )),
            None if spec.id.as_str().starts_with("plugin.") => {}
            None => errors.push(format!("missing command label for `{}`", spec.id)),
        }
    }
    let enabled_plugins = crate::app::configuration::resolve::enabled_plugin_owners(loaded);
    for extra in loaded.config.command_labels.keys().filter(|id| {
        !command_ids.contains(*id)
            && !(CommandId::new(id.as_str()).is_ok()
                && plugin_command_owner(id).is_some_and(|owner| {
                    allow_unavailable_plugin_labels || enabled_plugins.contains(owner)
                }))
    }) {
        errors.push(format!(
            "command label references unknown command `{extra}`"
        ));
    }
    for (kind, configured) in [
        ("text", &loaded.config.input.text_commands),
        ("paste", &loaded.config.input.paste_commands),
        (
            "character fallback",
            &loaded.config.input.character_fallback_commands,
        ),
    ] {
        for (context, command_id) in configured {
            let invocation =
                CommandInvocation::new(command_id, InvocationSource::Startup).map(|invocation| {
                    invocation.with_argument("text", CommandValue::String(String::new()))
                });
            if let Err(error) =
                invocation.and_then(|invocation| commands.validate_invocation_shape(&invocation))
            {
                errors.push(format!("{kind} command for `{context}`: {error}"));
            }
        }
    }
    for (context, command_id) in &loaded.config.input.unmatched_commands {
        let invocation = CommandInvocation::new(command_id, InvocationSource::Startup);
        if let Err(error) =
            invocation.and_then(|invocation| commands.validate_invocation_shape(&invocation))
        {
            errors.push(format!("unmatched command for `{context}`: {error}"));
        }
    }
    if let Err(error) = validate_command_references(
        commands,
        &loaded.config,
        &loaded.dashboard,
        &enabled_plugins,
    ) {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn validate_command_references(
    commands: &CommandRegistry<AppEffect>,
    config: &crate::config::Config,
    dashboard: &crate::config::DashboardConfig,
    deferred_plugins: &BTreeSet<String>,
) -> Result<(), String> {
    CommandReferences::new(config, dashboard)
        .resolve(
            commands,
            &config.command_labels,
            deferred_plugins,
            CommandReferencePolicy::Check,
        )
        .map(|_| ())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandReferencePolicy {
    Check,
    Runtime,
}

pub(super) fn plugin_command_owner(id: &str) -> Option<&str> {
    let owner = super::plugin_owner_from_id(id)?;
    (id.split('.').count() >= 3).then_some(owner)
}

#[derive(Clone)]
pub(super) struct CommandReferences {
    pub(super) aliases: std::collections::BTreeMap<String, String>,
    pub(super) dashboard: crate::config::DashboardConfig,
}

impl CommandReferences {
    pub(super) fn new(
        config: &crate::config::Config,
        dashboard: &crate::config::DashboardConfig,
    ) -> Self {
        Self {
            aliases: config.input.command_aliases.clone(),
            dashboard: dashboard.clone(),
        }
    }

    pub(super) fn resolve(
        &self,
        commands: &CommandRegistry<AppEffect>,
        labels: &std::collections::BTreeMap<String, String>,
        pending: &BTreeSet<String>,
        policy: CommandReferencePolicy,
    ) -> Result<(Self, Vec<String>), String> {
        let mut errors = Vec::new();
        let mut notices = Vec::new();
        let mut effective = self.clone();
        let registered = commands
            .specs()
            .map(|spec| spec.id.as_str())
            .collect::<BTreeSet<_>>();
        let keep_pending = policy == CommandReferencePolicy::Check;

        effective.aliases.retain(|alias, command_id| {
            if alias.is_empty() || alias.chars().any(char::is_whitespace) || alias.contains('.') {
                errors.push(format!("invalid command alias `{alias}`"));
                return false;
            }
            let valid_label = labels.get(command_id.as_str()).is_some_and(|label| {
                !label.is_empty() && !label.contains('\u{1b}') && label.len() <= 1_024
            });
            if !valid_label {
                errors.push(format!(
                    "command alias `{alias}` requires a valid command label for `{command_id}`"
                ));
            }
            if let Err(error) = CommandId::new(command_id.as_str()) {
                errors.push(format!("command alias `{alias}`: {error}"));
                return false;
            }
            if registered.contains(command_id.as_str()) {
                return valid_label;
            }
            if let Some(owner) = plugin_command_owner(command_id) {
                if pending.contains(owner) {
                    return keep_pending && valid_label;
                }
                if policy == CommandReferencePolicy::Runtime {
                    notices.push(format!(
                        "Command alias \"{alias}\" refers to unknown command \"{command_id}\"."
                    ));
                    return false;
                }
            }
            errors.push(format!(
                "command alias `{alias}` references unknown command `{command_id}`"
            ));
            false
        });

        effective.dashboard.action.retain(|action| {
            let invocation = CommandInvocation::new(&action.command, InvocationSource::Startup)
                .and_then(|mut invocation| {
                    for (name, value) in &action.arguments {
                        let value = configuration_value(value).map_err(|()| {
                            CommandError::InvalidArguments(format!(
                                "dashboard argument `{name}` has unsupported type"
                            ))
                        })?;
                        invocation.arguments.insert(name.clone(), value);
                    }
                    Ok(invocation)
                });
            // Classify absence after constructing the invocation. A known
            // command with invalid arguments must not take the missing-target path.
            let validation =
                invocation.and_then(|invocation| commands.validate_invocation_shape(&invocation));
            match validation {
                Ok(()) => true,
                Err(error) => {
                    if let CommandError::Unknown(id) = &error
                        && let Some(owner) = plugin_command_owner(id.as_str())
                    {
                        if pending.contains(owner) {
                            return keep_pending;
                        }
                        if policy == CommandReferencePolicy::Runtime {
                            notices.push(format!(
                                "Skipped dashboard action \"{}\": unknown command \"{id}\".",
                                action.label,
                            ));
                            return false;
                        }
                    }
                    errors.push(format!("dashboard action `{}`: {error}", action.label));
                    false
                }
            }
        });
        if errors.is_empty() {
            Ok((effective, notices))
        } else {
            Err(errors.join("; "))
        }
    }
}

pub(in crate::app) fn configuration_value(value: &toml::Value) -> Result<CommandValue, ()> {
    match value {
        toml::Value::String(value) => Ok(CommandValue::String(value.clone())),
        toml::Value::Boolean(value) => Ok(CommandValue::Bool(*value)),
        toml::Value::Integer(value) => Ok(CommandValue::Integer(*value)),
        _ => Err(()),
    }
}

pub(in crate::app) fn invocation_fingerprint(invocation: &CommandInvocation) -> String {
    let mut fingerprint = invocation.id.as_str().to_owned();
    for (name, value) in &invocation.arguments {
        fingerprint.push('|');
        fingerprint.push_str(&name.len().to_string());
        fingerprint.push(':');
        fingerprint.push_str(name);
        fingerprint.push('=');
        match value {
            CommandValue::String(value) => {
                fingerprint.push_str("string:");
                fingerprint.push_str(&value.len().to_string());
                fingerprint.push(':');
                fingerprint.push_str(value);
            }
            CommandValue::Bool(value) => {
                fingerprint.push_str(if *value { "bool:true" } else { "bool:false" })
            }
            CommandValue::Integer(value) => {
                fingerprint.push_str("integer:");
                fingerprint.push_str(&value.to_string());
            }
            CommandValue::Number(_) | CommandValue::List(_) | CommandValue::Record(_) => {
                fingerprint.push_str(&format!("{value:?}"));
            }
        }
    }
    fingerprint
}

pub(in crate::app) fn keymap_entries(
    loaded: &crate::config::LoadedKeymaps,
) -> (
    &[crate::config::KeymapEntry],
    usize,
    &[crate::config::KeyGroupEntry],
    usize,
) {
    (
        &loaded.entries,
        loaded.bundled_count,
        &loaded.groups,
        loaded.bundled_group_count,
    )
}
