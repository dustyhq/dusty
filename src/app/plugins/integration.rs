//! Extension catalog and package/registry transitions; editor publication stays in dispatch.
use crate::app::configuration::ContributionError;
use crate::app::configuration::ContributionKeymapPolicy;
use crate::app::plugins::Plugins;
use crate::plugins::PluginContributionCatalog;
use crate::plugins::PluginContributionIndex;
use crate::plugins::PluginLimits;
use crate::plugins::PluginLoadId;
use crate::tools::LockedPackage;
use crate::tools::PackageDiscovery;
use crate::tools::PackageError;
use crate::tools::PackageEvent;
use crate::tools::PackageLock;
use crate::tools::PackageOperation;
use crate::tools::RegistryEvent;
use crate::tools::RegistryRequest;
use crate::tools::RegistryResult;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
enum PackageChange {
    Install,
    Update,
}

impl PackageChange {
    fn from_operation(operation: &str) -> Option<Self> {
        match operation {
            "install" | "registry_prepare" => Some(Self::Install),
            "update" => Some(Self::Update),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct PackageCandidate {
    load: PluginLoadId,
    name: String,
    token: Option<u64>,
    installed: Option<LockedPackage>,
    change: Option<PackageChange>,
    index: Option<PluginContributionIndex>,
    runtime_ready: bool,
}

pub(in crate::app) enum ExtensionUpdate {
    Remove(String, PluginLoadId),
    Error(String),
    Conflict(String),
    PublishPackage(String, PluginLoadId, LockedPackage),
    PackageProblem { message: String, diagnostic: String },
    Message(&'static str, Vec<(&'static str, String)>),
    Lock(PackageLock),
    Failed { operation: String, error: String },
}
pub(in crate::app) enum RegistryUpdate {
    Present(Box<RegistryResult>, Option<crate::app::picker::PickerEpoch>),
    ValidateArtifact,
    Error(String),
}
impl Plugins {
    pub(in crate::app) fn pending_packages(&self) -> usize {
        self.packages.pending_count()
            + self.package_aborts.len()
            + usize::from(self.package_refresh)
    }
    pub(in crate::app) fn contributions(&self) -> &PluginContributionCatalog {
        &self.plugin_contributions
    }
    pub(in crate::app) fn install_index(
        &mut self,
        index: PluginContributionIndex,
    ) -> Result<(), String> {
        self.plugin_contributions
            .install(index.clone())
            .map_err(|e| e.to_string())?;
        self.install_events(&index).map_err(|e| e.to_string())?;
        self.providers.install(&index).map_err(|e| e.detail)?;
        self.virtual_documents.install(&index).map_err(|e| e.detail)
    }
    pub(in crate::app) fn remove_contribution(&mut self, plugin: &str, load: PluginLoadId) -> bool {
        self.plugin_contributions.remove_load(plugin, load.0)
    }
    pub(in crate::app) fn take_rejected(&mut self, plugin: &str, load: PluginLoadId) -> bool {
        self.rejected_plugin_loads.remove(&(plugin.into(), load))
    }
    pub(in crate::app) fn index_pending(&self, plugin: &str) -> bool {
        self.pending_index_activation.contains_key(plugin)
    }
    pub(in crate::app) fn queue_index(
        &mut self,
        plugin: String,
        load: PluginLoadId,
        checksum: String,
    ) {
        self.pending_index_activation
            .insert(plugin, (load, checksum));
    }
    pub(in crate::app) fn activate_pending(
        &mut self,
        deferred: bool,
        plugin: &str,
        load: PluginLoadId,
    ) -> Result<(), (String, PluginLoadId, String)> {
        let activations = if deferred {
            self.pending_index_activation.clone()
        } else {
            let Some((candidate, checksum)) = self
                .pending_index_activation
                .get(plugin)
                .filter(|(candidate, _)| *candidate == load)
                .cloned()
            else {
                return Ok(());
            };
            BTreeMap::from([(plugin.into(), (candidate, checksum))])
        };
        for (plugin, (load, checksum)) in activations {
            let result = self.manager.activate_indexed(&plugin, load, checksum);
            self.pending_index_activation.remove(&plugin);
            result.map_err(|e| (plugin, load, e.to_string()))?;
        }
        Ok(())
    }
    pub(in crate::app) fn reject_index(&mut self, plugin: String, load: PluginLoadId) {
        self.rejected_plugin_loads.insert((plugin, load));
    }
    pub(in crate::app) fn reject_expected(
        &mut self,
        expected: impl Iterator<Item = String>,
        failed: Option<(&str, PluginLoadId)>,
    ) {
        for plugin in expected {
            if let Some(load) = failed
                .filter(|(owner, _)| plugin == *owner)
                .map(|(_, load)| load)
                .or_else(|| self.manager.load_id(&plugin))
            {
                self.reject_index(plugin.clone(), load);
                self.pending_index_activation
                    .entry(plugin)
                    .or_insert_with(|| (load, String::new()));
            }
        }
    }
    pub(in crate::app) fn abort_indexes(&mut self) -> Vec<(String, PluginLoadId)> {
        let pending = std::mem::take(&mut self.pending_index_activation);
        pending
            .into_iter()
            .map(|(plugin, (load, _))| {
                self.reject_index(plugin.clone(), load);
                let _ = self.manager.disable_load(&plugin, load);
                (plugin, load)
            })
            .collect()
    }
    pub(in crate::app) fn request_package(
        &mut self,
        operation: PackageOperation,
    ) -> Result<u64, PackageError> {
        // Serialize mutations through preparation, plugin loading, and commit acknowledgement.
        if matches!(
            operation,
            PackageOperation::Install { .. }
                | PackageOperation::PrepareRegistry { .. }
                | PackageOperation::Rollback
                | PackageOperation::Remove(_)
        ) && (self.pending_packages() != 0
            || !self.package_preparing.is_empty()
            || !self.package_candidates.is_empty()
            || !self.package_commits.is_empty())
        {
            return Err(PackageError::Busy);
        }
        if let PackageOperation::Abort { token } = operation {
            // Keep rejected aborts for the next poll; enqueueing transfers the token to the worker.
            self.package_aborts.insert(token);
            let id = self.packages.request(PackageOperation::Abort { token })?;
            self.package_aborts.remove(&token);
            return Ok(id);
        }
        let preparing = matches!(
            operation,
            PackageOperation::Install { .. } | PackageOperation::PrepareRegistry { .. }
        );
        let id = self.packages.request(operation)?;
        if preparing {
            self.package_preparing.insert(id);
        }
        Ok(id)
    }
    pub(in crate::app) fn start_packages(&mut self) {
        self.package_startup_request = self.packages.request(PackageOperation::Activations).ok();
    }
    pub(in crate::app) fn request_registry(
        &mut self,
        request: RegistryRequest,
        install: bool,
        picker: Option<crate::app::picker::PickerEpoch>,
    ) -> Result<(), String> {
        let id = self.registry.request(request).map_err(|e| e.to_string())?;
        self.registry_intents.insert(id, (install, picker));
        Ok(())
    }
    fn accept_registry(&mut self, event: RegistryEvent) -> Option<RegistryUpdate> {
        // A stale workspace/request must not turn a late artifact into a new install.
        let (install, picker) = self.registry_intents.remove(&event.id)?;
        Some(match event.result {
            Ok(RegistryResult::Show(metadata)) if install => match self.request_registry(
                RegistryRequest::Fetch {
                    metadata: Box::new(metadata),
                },
                true,
                None,
            ) {
                Ok(()) => return None,
                Err(error) => RegistryUpdate::Error(error),
            },
            Ok(RegistryResult::Artifact { metadata, bytes }) => {
                if !install {
                    RegistryUpdate::Error(
                        "registry artifact was not requested for installation".into(),
                    )
                } else if self.pending_registry_artifact.is_some() {
                    RegistryUpdate::Error("another registry artifact is awaiting validation".into())
                } else {
                    self.pending_registry_artifact = Some((metadata, bytes));
                    RegistryUpdate::ValidateArtifact
                }
            }
            Ok(result) => RegistryUpdate::Present(Box::new(result), picker),
            Err(error) => RegistryUpdate::Error(error.to_string()),
        })
    }
    pub(in crate::app) fn poll_registry(&mut self) -> Vec<RegistryUpdate> {
        self.registry
            .poll()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|event| self.accept_registry(event))
            .collect()
    }
    pub(in crate::app) fn discard_artifact(&mut self) {
        self.pending_registry_artifact = None;
    }
    pub(in crate::app) fn prepare_artifact(&mut self, limits: PluginLimits) -> Result<(), String> {
        if let Some((metadata, component)) = self.pending_registry_artifact.take() {
            let name = metadata.package.name.clone();
            self.request_package(PackageOperation::PrepareRegistry {
                metadata: Box::new(metadata),
                component,
                limits,
            })
            .map_err(|error| match error {
                PackageError::Busy => Self::registry_install_busy(&name),
                _ => format!("Couldn't install package \"{name}\"."),
            })?;
        }
        Ok(())
    }
    fn registry_install_busy(name: &str) -> String {
        format!(
            "Couldn't install package \"{name}\"; another package operation is still in progress."
        )
    }
    fn abort_token(&mut self, token: u64) {
        let _ = self.request_package(PackageOperation::Abort { token });
    }
    pub(super) fn cancel_package_candidate(&mut self, plugin: &str) {
        if let Some(candidate) = self.package_candidates.remove(plugin) {
            let _ = self.manager.unload_load(plugin, candidate.load);
            if let Some(token) = candidate.token {
                self.abort_token(token);
            }
        }
    }
    fn package_candidate(&self, plugin: &str, load: PluginLoadId) -> Option<&PackageCandidate> {
        self.package_candidates
            .get(plugin)
            .filter(|candidate| candidate.load == load)
    }
    fn failed_package_load(
        &mut self,
        plugin: &str,
        load: PluginLoadId,
        diagnostic: impl ToString,
    ) -> Option<ExtensionUpdate> {
        let candidate = self.package_candidate(plugin, load)?;
        let name = candidate.name.clone();
        let change = candidate.change;
        let diagnostic = diagnostic.to_string();
        self.cancel_package_candidate(plugin);
        Some(if let Some(change) = change {
            self.package_change_problem(change, Some(&name), diagnostic)
        } else {
            self.package_load_problem(plugin, &name, diagnostic)
        })
    }
    fn package_load_problem(
        &mut self,
        plugin: &str,
        name: &str,
        diagnostic: String,
    ) -> ExtensionUpdate {
        self.package_discovery_problem(plugin, name, diagnostic, true)
    }
    fn package_discovery_problem(
        &mut self,
        plugin: &str,
        name: &str,
        diagnostic: String,
        loading: bool,
    ) -> ExtensionUpdate {
        let message = if loading {
            format!("Couldn't load package \"{name}\".")
        } else {
            format!("Couldn't check package \"{name}\".")
        };
        if let Some(row) = self.browser_packages.get_mut(plugin) {
            row.error = Some(message.clone());
        }
        self.browser_dirty = true;
        ExtensionUpdate::PackageProblem {
            message,
            diagnostic,
        }
    }
    fn package_change_problem(
        &self,
        change: PackageChange,
        name: Option<&str>,
        diagnostic: String,
    ) -> ExtensionUpdate {
        let message = match (change, name) {
            (PackageChange::Install, Some(name)) => format!("Couldn't install package \"{name}\"."),
            (PackageChange::Install, None) => "Couldn't install the package.".into(),
            (PackageChange::Update, None) => "Couldn't update the package.".into(),
            (PackageChange::Update, Some(name)) => {
                let kept =
                    self.package_managed_plugins
                        .iter()
                        .find_map(|(plugin, (load, package))| {
                            (package.name == name && self.manager.load_id(plugin) == Some(*load))
                                .then_some(&package.version)
                        });
                match kept {
                    Some(version) => format!("Couldn't update \"{name}\"; kept version {version}."),
                    None => format!("Couldn't update \"{name}\"."),
                }
            }
        };
        ExtensionUpdate::PackageProblem {
            message,
            diagnostic,
        }
    }
    fn submit_package_commit(
        &mut self,
        plugin: &str,
        load: PluginLoadId,
    ) -> Result<(), PackageError> {
        let candidate = self
            .package_candidates
            .get_mut(plugin)
            .filter(|candidate| candidate.load == load)
            .ok_or(PackageError::MissingPrepared)?;
        let token = candidate.token.ok_or(PackageError::MissingPrepared)?;
        let id = self.packages.request(PackageOperation::Commit { token })?;
        candidate.token = None;
        self.package_commits.insert(id, (plugin.into(), load));
        Ok(())
    }
    pub(in crate::app) fn cancel_pending_packages(&mut self) -> Vec<(String, PluginLoadId)> {
        self.registry_intents.clear();
        self.pending_registry_artifact = None;
        self.package_preparing.clear();
        // Installed packages come from the user store, not from the workspace roots.
        let workspace_candidates = self
            .package_candidates
            .iter()
            .filter(|(_, candidate)| candidate.installed.is_none())
            .map(|(plugin, _)| plugin.clone())
            .collect::<Vec<_>>();
        for plugin in workspace_candidates {
            self.cancel_package_candidate(&plugin);
        }
        // Staged loads have no live contributions, so the caller has nothing to remove.
        Vec::new()
    }
    pub(in crate::app) fn worker_stopped(&mut self) -> Option<Vec<String>> {
        if self.plugin_worker_reported {
            return None;
        }
        self.plugin_worker_reported = true;
        self.pending_index_activation.clear();
        self.rejected_plugin_loads.clear();
        for plugin in self.package_candidates.keys().cloned().collect::<Vec<_>>() {
            self.cancel_package_candidate(&plugin);
        }
        let unregister = self.manager.statuses.keys().cloned().collect();
        self.package_managed_plugins.clear();
        Some(unregister)
    }
    pub(in crate::app) fn clear_contributions(&mut self) {
        self.plugin_contributions.clear_active();
        self.clear_events();
    }
    fn load_prepared(
        &mut self,
        request: u64,
        prepared: crate::tools::PreparedPackage,
        change: PackageChange,
        limits: &PluginLimits,
    ) -> Vec<ExtensionUpdate> {
        let mut updates = Vec::new();
        let plugin = prepared.plugin.id.clone();
        let token = prepared.token;
        if !self.package_preparing.remove(&request) {
            self.abort_token(token);
            return updates;
        }
        if self.manager.load_id(&plugin).is_some_and(|load| {
            !self
                .package_managed_plugins
                .get(&plugin)
                .is_some_and(|(current, _)| *current == load)
        }) {
            self.abort_token(token);
            updates.push(self.package_change_problem(
                change,
                Some(&prepared.manifest.name),
                crate::plugins::PluginError::AlreadyActive.to_string(),
            ));
            return updates;
        }
        match self.manager.stage_load(
            self.package_store.clone(),
            prepared.object,
            prepared.plugin,
            limits.clone(),
            None,
        ) {
            Ok(load) => {
                self.package_candidates.insert(
                    plugin,
                    PackageCandidate {
                        load,
                        name: prepared.manifest.name,
                        token: Some(token),
                        installed: None,
                        change: Some(change),
                        index: None,
                        runtime_ready: false,
                    },
                );
            }
            Err(error) => {
                self.abort_token(token);
                updates.push(self.package_change_problem(
                    change,
                    Some(&prepared.manifest.name),
                    error.to_string(),
                ));
            }
        }
        updates
    }
    fn refresh_package_row(&mut self, plugin: &crate::plugins::PluginManifest) {
        self.browser_dirty = true;
        self.browser_packages.insert(
            plugin.id.clone(),
            crate::app::plugins::browser::BrowserPlugin {
                version: plugin.version.clone(),
                capabilities: plugin.capabilities.clone(),
                folder: self.package_store.clone(),
                error: None,
            },
        );
    }
    fn load_activations(
        &mut self,
        activations: Vec<PackageDiscovery>,
        limits: &PluginLimits,
        config: &crate::config::PluginsConfig,
        safe_mode: bool,
    ) -> Vec<ExtensionUpdate> {
        let mut updates = Vec::new();
        self.browser_dirty = true;
        self.browser_packages.clear();
        // A matching recorded manifest digest still identifies a running plugin if its file can't be read.
        self.package_managed_plugins
            .retain(|plugin, (load, installed)| {
                let unchanged = activations.iter().any(|entry| {
                    entry.package.name == installed.name
                        && entry.package.content_sha256 == installed.content_sha256
                        && entry.package.activation_sha256 == installed.activation_sha256
                        && entry
                            .plugin
                            .as_ref()
                            .is_none_or(|manifest| &manifest.id == plugin)
                });
                if !unchanged {
                    let _ = self.manager.unload_load(plugin, *load);
                    updates.push(ExtensionUpdate::Remove(plugin.clone(), *load));
                }
                unchanged
            });
        for PackageDiscovery {
            package,
            plugin,
            object,
        } in activations
        {
            let mut id = plugin
                .as_ref()
                .map_or_else(|| package.name.clone(), |plugin| plugin.id.clone());
            if let Some(manifest) = &plugin {
                self.refresh_package_row(manifest);
            } else {
                if let Some((owner, _)) = self
                    .package_managed_plugins
                    .iter()
                    .find(|(_, (_, installed))| installed.name == package.name)
                {
                    id.clone_from(owner);
                }
                self.browser_packages.insert(
                    id.clone(),
                    crate::app::plugins::browser::BrowserPlugin {
                        version: package.version.clone(),
                        capabilities: Default::default(),
                        folder: self.package_store.clone(),
                        error: None,
                    },
                );
            }
            let loading = !safe_mode
                && (!config.entries.get(&id).is_some_and(|entry| !entry.enabled)
                    || self.enabled_plugins.contains(&id))
                && !self.disabled_plugins.contains(&id);
            let (path, object_lock) = match object {
                Ok(object) => object,
                Err(error) => {
                    updates.push(self.package_discovery_problem(
                        &id,
                        &package.name,
                        error.to_string(),
                        loading,
                    ));
                    continue;
                }
            };
            let Some(manifest) = plugin else {
                continue;
            };
            if !loading {
                continue;
            }
            if let Some(load) = self.manager.load_id(&id) {
                if !self
                    .package_managed_plugins
                    .get(&id)
                    .is_some_and(|(current, _)| *current == load)
                {
                    updates.push(ExtensionUpdate::Conflict(id));
                }
                continue;
            }
            match self.manager.stage_load(
                self.package_store.clone(),
                path,
                manifest,
                limits.clone(),
                Some(object_lock),
            ) {
                Ok(load) => {
                    self.package_candidates.insert(
                        id,
                        PackageCandidate {
                            load,
                            name: package.name.clone(),
                            token: None,
                            installed: Some(package),
                            change: None,
                            index: None,
                            runtime_ready: false,
                        },
                    );
                }
                Err(error) => {
                    updates.push(self.package_load_problem(&id, &package.name, error.to_string()))
                }
            }
        }
        updates
    }
    pub(in crate::app) fn poll_packages(
        &mut self,
        limits: PluginLimits,
        config: &crate::config::PluginsConfig,
        safe_mode: bool,
    ) -> Vec<ExtensionUpdate> {
        let mut updates = Vec::new();
        for event in self.packages.poll().unwrap_or_default() {
            match event {
                PackageEvent::Completed(mut result) => {
                    if let Some(error) = result.cleanup_error.take() {
                        updates.push(ExtensionUpdate::Error(format!(
                            "package cleanup failed: {error}"
                        )));
                    }
                    let prepared_operation = result.prepared.is_some();
                    let activation_operation = result.operation == "activations";
                    if let Some(prepared) = result.prepared.take() {
                        let policy_disabled = config
                            .entries
                            .get(&prepared.plugin.id)
                            .is_some_and(|entry| !entry.enabled);
                        if !safe_mode
                            && !policy_disabled
                            && !self.disabled_plugins.contains(&prepared.plugin.id)
                            && let Some(change) = PackageChange::from_operation(&result.operation)
                        {
                            updates
                                .extend(self.load_prepared(result.id, prepared, change, &limits));
                        } else {
                            self.package_preparing.remove(&result.id);
                            self.abort_token(prepared.token);
                        }
                    }
                    if let Some(activations) = result.activations.take() {
                        updates.extend(self.load_activations(
                            activations,
                            &limits,
                            config,
                            safe_mode,
                        ));
                    }
                    if let Some((plugin, load)) = self.package_commits.remove(&result.id)
                        && let Some(candidate) = self.package_candidate(&plugin, load)
                        && let Some(package) = result.lock.as_ref().and_then(|lock| {
                            lock.packages
                                .iter()
                                .find(|entry| entry.name == candidate.name)
                        })
                    {
                        updates.push(ExtensionUpdate::PublishPackage(
                            plugin,
                            load,
                            package.clone(),
                        ));
                    }
                    if self.package_startup_request == Some(result.id) {
                        self.package_startup_request = None;
                    }
                    if let Some(lock) = result.lock {
                        updates.push(ExtensionUpdate::Lock(lock));
                    } else if result.operation == "verify" && result.verified == Some(0) {
                        updates.push(ExtensionUpdate::Message(
                            "package_nothing_to_verify",
                            Vec::new(),
                        ));
                    } else if !prepared_operation
                        && !activation_operation
                        && result.operation != "abort"
                    {
                        updates.push(ExtensionUpdate::Message(
                            "package_operation_complete",
                            vec![
                                ("operation", result.operation),
                                (
                                    "count",
                                    result.verified.or(result.removed).unwrap_or(0).to_string(),
                                ),
                            ],
                        ));
                    }
                }
                PackageEvent::Failed {
                    id,
                    operation,
                    package,
                    error,
                } => {
                    let preparing = self.package_preparing.remove(&id);
                    if self.package_startup_request == Some(id) {
                        self.package_startup_request = None;
                    }
                    if let Some((plugin, load)) = self.package_commits.remove(&id) {
                        if let Some(update) = self.failed_package_load(&plugin, load, error) {
                            updates.push(update);
                        }
                    } else if let Some(change) = PackageChange::from_operation(&operation) {
                        // Root changes drop preparation intents; late failures are no longer ours.
                        if preparing {
                            let update = match (operation.as_str(), package.as_deref(), &error) {
                                ("registry_prepare", Some(name), PackageError::Busy) => {
                                    ExtensionUpdate::PackageProblem {
                                        message: Self::registry_install_busy(name),
                                        diagnostic: error.to_string(),
                                    }
                                }
                                _ => self.package_change_problem(
                                    change,
                                    package.as_deref(),
                                    error.to_string(),
                                ),
                            };
                            updates.push(update);
                        }
                    } else if operation == "activations" {
                        updates.push(ExtensionUpdate::PackageProblem {
                            message: "Couldn't read installed packages.".into(),
                            diagnostic: error.to_string(),
                        });
                    } else if operation != "abort" {
                        updates.push(ExtensionUpdate::Failed {
                            operation,
                            error: error.to_string(),
                        });
                    }
                }
            }
        }
        for token in self.package_aborts.clone() {
            if self
                .request_package(PackageOperation::Abort { token })
                .is_err()
            {
                break;
            }
        }
        if self.package_refresh
            && self.packages.pending_count() == 0
            && self.package_aborts.is_empty()
            && self.package_preparing.is_empty()
            && self.package_candidates.is_empty()
            && self.package_commits.is_empty()
            && self.package_startup_request.is_none()
            && let Ok(id) = self.packages.request(PackageOperation::Activations)
        {
            self.package_startup_request = Some(id);
            self.package_refresh = false;
        }
        updates
    }
}
#[cfg(test)]
impl Plugins {
    pub(in crate::app) fn fixture_contribution(
        &mut self,
        index: PluginContributionIndex,
    ) -> Result<(), crate::plugins::PluginError> {
        self.plugin_contributions.install(index)
    }
    pub(in crate::app) fn fixture_pending_index(
        &mut self,
        plugin: String,
        source: (PluginLoadId, String),
    ) {
        self.queue_index(plugin, source.0, source.1);
    }
    pub(in crate::app) fn pending_indexes(&self) -> &BTreeMap<String, (PluginLoadId, String)> {
        &self.pending_index_activation
    }
    pub(in crate::app) fn rejected_indexes(
        &self,
    ) -> &std::collections::BTreeSet<(String, PluginLoadId)> {
        &self.rejected_plugin_loads
    }
}

use crate::app::commands::AppEffect;
use crate::app::search::SEARCH_MAXIMUM_FILE_BYTES;
use crate::editor::ArgumentSchema;
use crate::ui::render::SCRATCH_LABEL;

use crate::app::App;
use crate::app::picker::PickerPayload;
use crate::app::windows::NativeDescriptions;
use crate::editor::CommandAuthority;
use crate::editor::CommandId;
use crate::editor::CommandOutcome;
use crate::editor::CommandRegistry;
use crate::editor::CommandSource;
use crate::editor::CommandSpec;
use crate::editor::ContextRequirement;
use crate::editor::EditorState;
use crate::plugins::PluginEvent;
use crate::telemetry::LogLevel;
use crate::workspace::PickerAction;
use crate::workspace::PickerItem;
use crate::workspace::PickerItemId;
use std::collections::BTreeSet;

use crate::app::plugins::VirtualContext;
use crate::app::plugins::VirtualOpen;
use crate::app::plugins::VirtualPublication;
use crate::app::plugins::WindowUpdate;
use crate::app::search::workflow::ABS_MAX_BYTES;
use crate::app::search::workflow::SearchMatcherKind;
use crate::app::search::workflow::SearchPresenterKind;
use crate::app::search::workflow::SearchRequestId;
use crate::app::search::workflow::SearchSourceKind;
use crate::app::search::workflow::SearchStreamId;
use crate::app::search::workflow::SearchWorkflowBroker;
use crate::app::search::workflow::SearchWorkflowChunk;
use crate::app::search::workflow::SearchWorkflowError;
use crate::app::search::workflow::SearchWorkflowId;
use crate::app::search::workflow::SearchWorkflowSpec;
use crate::app::search::workflow::picker_item_adapter;
use crate::app::search::workflow::plugin_item_adapter;
use crate::ui::PromptKind;

pub(in crate::app) fn compose_plugin_picker_items(
    projection: &crate::plugins::PluginPickerProjection,
) -> Result<Vec<(PickerItem, PickerPayload)>, SearchWorkflowError> {
    let spec = SearchWorkflowSpec {
        id: SearchWorkflowId(projection.request),
        source: SearchSourceKind::PluginProvider {
            provider: projection.provider.clone(),
        },
        matcher: SearchMatcherKind::Fuzzy,
        presenter: SearchPresenterKind::Picker,
        maximum_items: projection.items.len().max(1),
        maximum_bytes: ABS_MAX_BYTES,
    };
    let mut broker = SearchWorkflowBroker::try_new(spec)?;
    broker.begin(
        SearchRequestId(projection.request),
        SearchStreamId(projection.revision),
        &projection.query,
    )?;
    let workflow = projection
        .items
        .iter()
        .map(|item| plugin_item_adapter(&item.id, &item.primary, item.secondary.as_deref()))
        .collect();
    broker.accept(SearchWorkflowChunk {
        request: SearchRequestId(projection.request),
        stream_id: SearchStreamId(projection.revision),
        sequence: 0,
        items: workflow,
        complete: true,
    })?;
    let snapshot = broker
        .snapshot_if_changed(0)
        .ok_or(SearchWorkflowError::Stale)?;
    let mut source = projection
        .items
        .iter()
        .map(|item| (item.id.clone(), item))
        .collect::<BTreeMap<_, _>>();
    Ok(snapshot
        .items
        .into_iter()
        .filter_map(|ranked| {
            source.remove(&ranked.stable_key).map(|item| {
                let id = PickerItemId(format!(
                    "plugin-provider:{}:{}",
                    projection.request, item.id
                ));
                (
                    PickerItem {
                        badge: String::new(),
                        kind: crate::workspace::PickerItemKind::Entry,
                        id,
                        label: item.primary.clone(),
                        detail: item.secondary.clone().unwrap_or_default(),
                        source: projection.provider.clone(),
                        action: PickerAction::None,
                    },
                    PickerPayload::PluginProviderItem {
                        plugin: projection.plugin.clone(),
                        load: projection.load,
                        request: projection.request,
                        item: item.id.clone(),
                        action_command: projection.action_command.clone(),
                    },
                )
            })
        })
        .collect())
}

pub(in crate::app) fn compose_command_picker_items(
    items: Vec<(PickerItem, PickerPayload)>,
) -> Result<Vec<(PickerItem, PickerPayload)>, SearchWorkflowError> {
    let spec = SearchWorkflowSpec {
        id: SearchWorkflowId(1),
        source: SearchSourceKind::Commands,
        matcher: SearchMatcherKind::Fuzzy,
        presenter: SearchPresenterKind::Picker,
        maximum_items: items.len().max(1),
        maximum_bytes: ABS_MAX_BYTES,
    };
    let mut broker = SearchWorkflowBroker::try_new(spec)?;
    broker.begin(SearchRequestId(1), SearchStreamId(1), "")?;
    let workflow = items
        .iter()
        .map(|(item, payload)| {
            let mut item = item.clone();
            if let PickerPayload::Command(command) = payload {
                item.action = PickerAction::InvokeCommand {
                    command: command.clone(),
                }
            }
            picker_item_adapter(&item)
        })
        .collect();
    broker.accept(SearchWorkflowChunk {
        request: SearchRequestId(1),
        stream_id: SearchStreamId(1),
        sequence: 0,
        items: workflow,
        complete: true,
    })?;
    let order = broker
        .snapshot_if_changed(0)
        .ok_or(SearchWorkflowError::Stale)?
        .items
        .into_iter()
        .map(|item| item.stable_key)
        .collect::<Vec<_>>();
    let mut by_id = items
        .into_iter()
        .map(|pair| (pair.0.id.0.clone(), pair))
        .collect::<BTreeMap<_, _>>();
    Ok(order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect())
}
impl App {
    pub(in crate::app) fn plugin_limits(&self) -> PluginLimits {
        let policy = &self.configuration.settings().plugins;
        PluginLimits {
            maximum_component_bytes: policy.maximum_component_bytes,
            maximum_commands: policy.maximum_commands,
            fuel: policy.fuel,
            epoch_deadline_ticks: policy.epoch_deadline_ticks,
            maximum_memory_bytes: policy.maximum_memory_bytes,
            maximum_table_elements: policy.maximum_table_elements,
            maximum_instances: policy.maximum_instances,
        }
    }
}

impl App {
    pub(in crate::app) fn load_configured_plugins(&mut self) {
        if self.workspace.safe_mode() {
            self.show_error(self.message("safe_mode_blocked", &[]));
            return;
        }
        let policy = self.configuration.settings().plugins.clone();
        let store = if policy.store.is_absolute() {
            policy.store
        } else {
            crate::config::user_state_directory().join(policy.store)
        };
        let limits = self.plugin_limits();
        for (id, entry) in &policy.entries {
            if self.extension.disabled_plugins.contains(id)
                || (!entry.enabled && !self.extension.enabled_plugins.contains(id))
            {
                continue;
            }
            if let Err(error) = self.extension.index_from_store(
                id.clone(),
                store.clone(),
                entry.manifest.clone(),
                entry.component.clone(),
                limits.clone(),
            ) {
                if self.configuration.deferred_plugin_keymaps() {
                    let expected = self
                        .configuration
                        .expected_plugin_indexes()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>();
                    self.extension.reject_expected(expected.into_iter(), None);
                    self.abort_pending_index_batch(error);
                    break;
                }
                self.show_error(self.message(
                    "plugin_load_failed",
                    &[("id", id.clone()), ("diagnostic", error.to_string())],
                ));
            }
        }
    }
}

impl App {
    pub(in crate::app) fn handle_plugin_worker_stopped(
        &mut self,
        error: crate::plugins::PluginError,
    ) {
        let Some(unregister) = self.extension.worker_stopped() else {
            return;
        };
        for plugin in unregister {
            self.commands.unregister_plugin(&plugin);
        }
        let owned_loads = self
            .extension
            .contributions()
            .ordered()
            .map(|index| (index.owner.clone(), PluginLoadId(index.plugin_load)))
            .collect::<Vec<_>>();
        for (plugin, load) in owned_loads {
            self.remove_plugin_contributions(&plugin, load);
        }
        self.extension.clear_contributions();
        if let Ok(candidate) = self.configuration.prepare_base_bindings(
            &self.commands,
            self.extension.contributions(),
            true,
            &BTreeSet::new(),
        ) && let Err(error) = self.commit_contribution_candidate(candidate)
        {
            self.show_error(error);
        }
        self.health.record(
            LogLevel::Error,
            "plugins",
            "worker-stopped",
            BTreeMap::from([("error".into(), error.to_string())]),
        );
        self.show_error(self.message(
            "plugin_worker_stopped",
            &[("diagnostic", error.to_string())],
        ));
    }
}

impl App {
    pub(in crate::app) fn register_indexed_plugin_commands(
        &mut self,
        index: &crate::plugins::PluginContributionIndex,
    ) -> Result<(), String> {
        self.register_plugin_commands(&index.owner, &index.commands)
    }
}

impl App {
    fn register_plugin_commands(
        &mut self,
        plugin: &str,
        declarations: &BTreeMap<String, crate::plugins::PluginCommandDeclaration>,
    ) -> Result<(), String> {
        let authority = self.plugin_authority(plugin);
        Self::register_plugin_commands_in(&mut self.commands, plugin, declarations, authority)
    }

    fn register_plugin_commands_in(
        commands: &mut CommandRegistry<AppEffect>,
        plugin: &str,
        declarations: &BTreeMap<String, crate::plugins::PluginCommandDeclaration>,
        authority: CommandAuthority,
    ) -> Result<(), String> {
        let plugin = plugin.to_owned();
        commands.unregister_plugin(&plugin);
        for (command, declaration) in declarations {
            let id = format!("plugin.{plugin}.{command}");
            let id = CommandId::new(&id).map_err(|error| error.to_string())?;
            let mut arguments = ArgumentSchema::default();
            for (name, field) in &declaration.arguments {
                arguments = arguments.field(
                    name,
                    match field.kind {
                        crate::plugins::PluginArgumentKind::String => {
                            crate::editor::ArgumentType::String
                        }
                        crate::plugins::PluginArgumentKind::Boolean => {
                            crate::editor::ArgumentType::Bool
                        }
                        crate::plugins::PluginArgumentKind::Integer => {
                            crate::editor::ArgumentType::Integer
                        }
                        crate::plugins::PluginArgumentKind::Number => {
                            crate::editor::ArgumentType::Number
                        }
                        crate::plugins::PluginArgumentKind::List => {
                            crate::editor::ArgumentType::List
                        }
                        crate::plugins::PluginArgumentKind::Record => {
                            crate::editor::ArgumentType::Record
                        }
                    },
                    field.required,
                );
            }
            let effect_plugin = plugin.clone();
            let effect_command = command.clone();
            let spec = CommandSpec {
                id,
                title: declaration.title.clone(),
                description: declaration.description.clone(),
                arguments,
                result: None,
                access: std::collections::BTreeSet::new(),
                context: match declaration.context {
                    crate::plugins::PluginCommandContext::Application => {
                        ContextRequirement::Application
                    }
                    crate::plugins::PluginCommandContext::Workspace => {
                        ContextRequirement::Workspace
                    }
                    crate::plugins::PluginCommandContext::View => ContextRequirement::View,
                    crate::plugins::PluginCommandContext::Buffer => ContextRequirement::Buffer,
                    crate::plugins::PluginCommandContext::Selection => {
                        ContextRequirement::Selection
                    }
                },
                source: CommandSource::Plugin(plugin.clone()),
            };
            let command_id = spec.id.clone();
            if let Err(error) = commands.register(spec, |_, _| Ok(CommandOutcome::empty())) {
                commands.unregister_plugin(&plugin);
                return Err(error.to_string());
            }
            if let Err(error) = commands.register_wrapper(
                &command_id,
                plugin.clone(),
                0,
                authority.clone(),
                move |_, invocation, pending, previous| {
                    Ok(crate::editor::CommandWrapperAction::Pending(vec![
                        AppEffect::PluginCall {
                            plugin: effect_plugin.clone(),
                            handler: effect_command.clone(),
                            call: Box::new(crate::app::plugins::abi::command_call(
                                invocation,
                                previous.as_ref().and_then(|outcome| outcome.value.as_ref()),
                            )),
                            pending: Some(pending),
                            previous: previous.map(Box::new),
                        },
                    ]))
                },
            ) {
                commands.unregister_plugin(&plugin);
                return Err(error.to_string());
            }
        }
        Ok(())
    }
}

impl App {
    fn package_command_snapshot(
        &self,
        replacing: Option<&str>,
    ) -> Result<CommandRegistry<AppEffect>, String> {
        // Take a snapshot without unregistering live wrappers or pending commands.
        let mut commands = CommandRegistry::default();
        for spec in self.commands.specs() {
            if !matches!(&spec.source, CommandSource::Plugin(owner) if Some(owner.as_str()) == replacing)
            {
                commands
                    .register(spec.clone(), |_, _| Ok(CommandOutcome::empty()))
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(commands)
    }

    fn add_package_index(
        commands: &mut CommandRegistry<AppEffect>,
        catalog: &mut PluginContributionCatalog,
        index: &PluginContributionIndex,
    ) -> Result<(), String> {
        catalog
            .install(index.clone())
            .map_err(|error| error.to_string())?;
        if let Err(error) = catalog
            .context_snapshot()
            .map_err(|error| error.to_string())
            .and_then(|_| {
                Self::register_plugin_commands_in(
                    commands,
                    &index.owner,
                    &index.commands,
                    CommandAuthority::Host,
                )
            })
        {
            commands.unregister_plugin(&index.owner);
            catalog.remove_load(&index.owner, index.plugin_load);
            return Err(error);
        }
        Ok(())
    }

    fn check_package_services(index: &PluginContributionIndex) -> Result<(), String> {
        crate::plugins::PluginHostEventQueue::default()
            .install(index)
            .map_err(|e| e.to_string())?;
        crate::plugins::PluginProviderBroker::default()
            .install(index)
            .map_err(|e| e.detail)?;
        crate::plugins::PluginVirtualDocumentBroker::default()
            .install(index)
            .map_err(|e| e.detail)
    }

    fn prepare_package_policy(
        &self,
        commands: &CommandRegistry<AppEffect>,
        catalog: &PluginContributionCatalog,
        keymap_policy: ContributionKeymapPolicy,
    ) -> Result<crate::app::configuration::ContributionCandidate, ContributionError> {
        let mut pending = self.pending_keymap_providers();
        pending.retain(|owner| catalog.get(owner).is_none());
        let candidate = self.configuration.prepare_plugin_contributions(
            commands,
            catalog,
            &pending,
            keymap_policy,
        )?;
        self.prepare_contribution_windows(&candidate)?;
        self.extension
            .windows
            .clone()
            .set_configured_rules(candidate.windows())?;
        Ok(candidate)
    }

    fn check_package_index(
        &self,
        index: &PluginContributionIndex,
        change: Option<PackageChange>,
    ) -> Result<(), ContributionError> {
        let mut catalog = self.extension.contributions().clone();
        if let Some(current) = catalog.get(&index.owner) {
            catalog.remove_load(&index.owner, current.plugin_load);
        }
        let mut commands = self.package_command_snapshot(Some(&index.owner))?;
        Self::add_package_index(&mut commands, &mut catalog, index)?;
        let keymap_policy = match change {
            Some(PackageChange::Update) => ContributionKeymapPolicy::UpdatePreflight,
            _ => ContributionKeymapPolicy::RetainStartupFallback,
        };
        self.prepare_package_policy(&commands, &catalog, keymap_policy)?;
        Self::check_package_services(index)?;
        Ok(())
    }

    fn index_package_candidate(
        &mut self,
        index: PluginContributionIndex,
    ) -> Result<(), ContributionError> {
        let load = PluginLoadId(index.plugin_load);
        let candidate = self
            .extension
            .package_candidate(&index.owner, load)
            .ok_or_else(|| PackageError::MissingPrepared.to_string())?;
        if candidate.installed.is_some() {
            // These guests remain staged until shared configuration can see the whole batch.
            Self::check_package_services(&index)?;
        } else {
            self.check_package_index(&index, candidate.change)?;
        }
        self.extension
            .manager
            .activate_indexed(&index.owner, load, index.checksum.clone())
            .map_err(|error| error.to_string())?;
        if let Some(candidate) = self.extension.package_candidates.get_mut(&index.owner) {
            candidate.index = Some(index);
        }
        Ok(())
    }

    fn finish_package_activation(
        &mut self,
        plugin: &str,
        load: PluginLoadId,
        component: &crate::plugins::LoadedComponent,
    ) -> Result<(), ContributionError> {
        let candidate = self
            .extension
            .package_candidate(plugin, load)
            .ok_or_else(|| PackageError::MissingPrepared.to_string())?;
        let index = candidate
            .index
            .as_ref()
            .ok_or_else(|| crate::plugins::PluginError::IndexNotAccepted.to_string())?;
        if !component.instantiated {
            return Err(crate::plugins::PluginError::NotActive.to_string().into());
        }
        let activated = component.activated_commands.iter().collect::<BTreeSet<_>>();
        if index.commands.keys().collect::<BTreeSet<_>>() != activated {
            return Err(crate::plugins::PluginError::UndeclaredCommand
                .to_string()
                .into());
        }
        if candidate.installed.is_some() {
            if let Some(candidate) = self.extension.package_candidates.get_mut(plugin) {
                candidate.runtime_ready = true;
            }
        } else {
            // Configuration or another plugin may have changed while the guest activated.
            self.check_package_index(index, candidate.change)?;
            self.extension
                .submit_package_commit(plugin, load)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn adopt_package_runtime(&mut self, index: &PluginContributionIndex) -> Result<(), String> {
        let load = PluginLoadId(index.plugin_load);
        let plugin = &index.owner;
        let previous = self
            .extension
            .manager
            .publish_staged(plugin, load)
            .map_err(|error| error.to_string())?;
        if let Some(previous) = previous {
            self.remove_plugin_load_contributions(plugin, previous, false);
        }
        let result = self
            .extension
            .install_index(index.clone())
            .and_then(|()| self.register_indexed_plugin_commands(index));
        if result.is_err() {
            let _ = self.extension.manager.unload_load(plugin, load);
            self.remove_plugin_load_contributions(plugin, load, false);
        }
        result
    }

    fn record_package_loaded(&mut self, index: PluginContributionIndex, package: LockedPackage) {
        let load = PluginLoadId(index.plugin_load);
        let plugin = index.owner;
        self.extension.package_candidates.remove(&plugin);
        if index.storage.is_some() {
            self.extension.install_storage(&plugin, load);
        }
        self.extension.browser_packages.insert(
            plugin.clone(),
            crate::app::plugins::browser::BrowserPlugin {
                version: index.version,
                capabilities: index.capabilities,
                folder: self.extension.package_store.clone(),
                error: None,
            },
        );
        self.extension
            .package_managed_plugins
            .insert(plugin, (load, package));
        self.extension.browser_dirty = true;
    }

    pub(in crate::app) fn publish_package(
        &mut self,
        plugin: String,
        load: PluginLoadId,
        package: LockedPackage,
    ) {
        let Some(candidate) = self.extension.package_candidate(&plugin, load) else {
            return;
        };
        let Some(index) = candidate.index.clone() else {
            return;
        };
        // The lock already names the replacement. Keep the previous runtime if the
        // current settings no longer accept the candidate.
        let result = self
            .check_package_index(&index, candidate.change)
            .and_then(|()| {
                self.adopt_package_runtime(&index)
                    .map_err(ContributionError::from)
            });
        if let Err(error) = result {
            self.report_package_load_failure(&plugin, load, error);
            return;
        }
        self.record_package_loaded(index, package);
        if let Err(error) = self.recompile_plugin_contributions() {
            self.show_error(error);
        }
    }

    fn report_package_load_failure(
        &mut self,
        plugin: &str,
        load: PluginLoadId,
        error: impl Into<ContributionError>,
    ) {
        let error = error.into();
        let shortcut_notice = self
            .extension
            .package_candidate(plugin, load)
            .filter(|candidate| matches!(candidate.change, Some(PackageChange::Update)))
            .and(error.shortcut_notice.as_deref());
        if let Some(mut update) = self.extension.failed_package_load(plugin, load, &error) {
            if let (Some(notice), ExtensionUpdate::PackageProblem { message, .. }) =
                (shortcut_notice, &mut update)
            {
                message.push(' ');
                message.push_str(notice);
            }
            self.apply_extension_updates(vec![update]);
        }
    }

    fn publish_installed_packages(&mut self) {
        if self.configured_indexes_pending()
            || self
                .extension
                .package_candidates
                .values()
                .any(|candidate| candidate.installed.is_some() && !candidate.runtime_ready)
        {
            return;
        }
        let batch = self
            .extension
            .package_candidates
            .iter()
            .filter_map(|(plugin, candidate)| {
                Some((
                    plugin.clone(),
                    (candidate.index.clone()?, candidate.installed.clone()?),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        if batch.is_empty() {
            return;
        }
        // Check shared aliases, keymaps and window selectors against the complete set
        // of successful staged loads and the live catalog, before making any guest callable.
        let result = self.package_command_snapshot(None).map(|mut commands| {
            let mut catalog = self.extension.contributions().clone();
            let mut accepted = Vec::new();
            for (plugin, (index, package)) in &batch {
                match Self::add_package_index(&mut commands, &mut catalog, index) {
                    Ok(()) => accepted.push((index, package)),
                    Err(error) => self.report_package_load_failure(
                        plugin,
                        PluginLoadId(index.plugin_load),
                        error,
                    ),
                }
            }
            if let Err(error) = self.prepare_package_policy(
                &commands,
                &catalog,
                ContributionKeymapPolicy::RetainStartupFallback,
            ) {
                self.show_error(error);
            }
            accepted
        });
        let accepted = match result {
            Ok(accepted) => accepted,
            Err(error) => {
                for (plugin, (index, _)) in &batch {
                    self.report_package_load_failure(
                        plugin,
                        PluginLoadId(index.plugin_load),
                        error.clone(),
                    );
                }
                return;
            }
        };
        if accepted.is_empty() {
            return;
        }
        for (index, package) in accepted {
            match self.adopt_package_runtime(index) {
                Ok(()) => self.record_package_loaded(index.clone(), package.clone()),
                Err(error) => self.report_package_load_failure(
                    &index.owner,
                    PluginLoadId(index.plugin_load),
                    error,
                ),
            }
        }
        if let Err(error) = self.recompile_plugin_contributions() {
            self.show_error(error);
        }
    }

    pub(in crate::app) fn pending_keymap_providers(&self) -> BTreeSet<String> {
        let extension = &self.extension;
        let discovering = extension.package_startup_request.is_some();
        extension
            .manager
            .statuses
            .keys()
            .filter(|owner| extension.load_id(owner).is_some())
            .chain(extension.package_candidates.keys())
            .chain(
                self.configuration
                    .expected_plugin_indexes()
                    .iter()
                    .filter(|_| discovering),
            )
            .filter(|owner| {
                extension.contributions().get(owner).is_none()
                    && !extension.disabled_plugins.contains(*owner)
            })
            .cloned()
            .collect()
    }

    pub(in crate::app) fn report_settled_keymap_providers(&mut self, pending: &BTreeSet<String>) {
        if pending.is_empty() {
            return;
        }
        let still_pending = self.pending_keymap_providers();
        if pending
            .difference(&still_pending)
            .all(|owner| self.extension.contributions().get(owner).is_some())
        {
            return;
        }
        // A provider can fail before publication. Reconcile its references
        // now that they are no longer waiting on an index.
        if let Err(error) = self.recompile_plugin_contributions() {
            self.show_error(error);
        }
    }

    pub(in crate::app) fn recompile_plugin_contributions(&mut self) -> Result<(), String> {
        let candidate = self
            .configuration
            .prepare_plugin_contributions(
                &self.commands,
                self.extension.contributions(),
                &self.pending_keymap_providers(),
                ContributionKeymapPolicy::RetainStartupFallback,
            )
            .map_err(|error| error.to_string())?;
        self.commit_contribution_candidate(candidate)
    }
}

impl App {
    fn prepare_contribution_windows(
        &self,
        candidate: &crate::app::configuration::ContributionCandidate,
    ) -> Result<crate::app::windows::WindowCandidate, String> {
        let native_windows = crate::config::WindowsConfig {
            rules: candidate
                .windows()
                .rules
                .iter()
                .filter(|rule| !rule.content_type.starts_with("plugin."))
                .cloned()
                .collect(),
        };
        self.prepare_configured_window_rules(&native_windows)
    }

    pub(in crate::app) fn commit_contribution_candidate(
        &mut self,
        candidate: crate::app::configuration::ContributionCandidate,
    ) -> Result<(), String> {
        // Both owners are checked before publication. There is no callback, wait,
        // or configuration mutation between this check and the two accepts.
        self.configuration
            .check_contributions(&candidate, self.extension.contributions())?;
        let windows = self.prepare_contribution_windows(&candidate)?;
        let (rules, catalog) = self.extension.prepare_window_rules(&candidate)?;
        self.window_session.accept_current(
            windows,
            &self.state.window_version(),
            &mut self.state,
        )?;
        let previous_notice = self.configuration.keymap_notice().cloned();
        let accepted = self
            .configuration
            .commit_contributions(candidate, catalog)?;
        rules.finish(accepted)?;
        let notice = self.configuration.keymap_notice();
        if notice != previous_notice.as_ref() {
            let current = self.ui.config_diagnostic.take();
            let other = match (current, previous_notice.as_ref()) {
                (Some(current), Some(previous)) if current == previous.summary => None,
                (Some(current), Some(previous)) => current
                    .strip_prefix(previous.summary.as_str())
                    .and_then(|rest| rest.strip_prefix("; "))
                    .map(str::to_owned)
                    .or(Some(current)),
                (current, _) => current,
            };
            self.ui.config_diagnostic = match (other, notice) {
                (Some(other), Some(notice)) => Some(format!("{}; {other}", notice.summary)),
                (None, Some(notice)) => Some(notice.summary.clone()),
                (other, None) => other,
            };
            if let Some(notice) = notice {
                for entry in notice.new_history_entries(previous_notice.as_ref()) {
                    self.notices.record_history(entry.clone(), false);
                }
            }
        }
        self.project_configuration();
        self.cancel_pending_mapping();
        self.reconcile_editor_windows();
        Ok(())
    }
}

impl App {
    pub(in crate::app) fn remove_plugin_contributions(&mut self, plugin: &str, load: PluginLoadId) {
        self.remove_plugin_load_contributions(plugin, load, true);
    }

    fn remove_plugin_load_contributions(
        &mut self,
        plugin: &str,
        load: PluginLoadId,
        recompile: bool,
    ) {
        match self
            .window_session
            .remove_plugin_load(plugin, load.0, &mut self.state)
            .map(|()| self.extension.remove_windows(plugin, load.0))
        {
            Ok(removed) => {
                if removed {
                    self.ui.plugin_surfaces = self.extension.window_snapshots();
                    self.window_session.invalidate_native();
                    self.reconcile_editor_windows();
                }
            }
            Err(error) => {
                self.show_error(error);
                return;
            }
        }
        let removed_active_confirmation = self.extension.remove_load(plugin, load);
        if removed_active_confirmation
            && self
                .input
                .prompt()
                .is_some_and(|prompt| prompt.kind == PromptKind::PluginConfirmation)
        {
            self.close_prompt();
        }
        let terminals = self.terminal_session.descriptions();
        let title = self.message("explorer_title", &[]);
        let descriptions = NativeDescriptions {
            signature: self.native_surface_signature(),
            explorer_title: &title,
            dashboard_title: &self.configuration.dashboard().identity,
            picker_title: self
                .ui
                .picker
                .as_ref()
                .map_or("Picker", |picker| &picker.title),
            terminals: &terminals,
        };
        let result = self
            .extension
            .remove_virtual_documents(
                plugin,
                load,
                VirtualContext {
                    editor: &self.state,
                    windows: &self.window_session,
                    scratch: SCRATCH_LABEL,
                },
                descriptions,
            )
            .and_then(|staged| {
                staged
                    .map(|staged| {
                        Self::accept_virtual_publication(&mut self.window_session, staged)
                    })
                    .transpose()
            });
        match result {
            Ok(Some(state)) => {
                *self.state = state;
                if self.state.active_view().is_none() {
                    self.set_dashboard_visibility(true);
                }
                self.reconcile_editor_windows();
            }
            Ok(None) => {}
            Err(error) => self.show_error(error),
        }
        if self.extension.remove_provider_load(plugin, load) {
            self.close_picker();
        }

        if self.extension.remove_contribution(plugin, load) {
            self.commands.unregister_plugin(plugin);
            self.remove_plugin_abi(plugin);
            if recompile && let Err(error) = self.recompile_plugin_contributions() {
                self.show_error(error);
            }
        }
    }
}

impl App {
    pub(in crate::app) fn abort_pending_index_batch(&mut self, diagnostic: impl std::fmt::Display) {
        for (owner, load) in self.extension.abort_indexes() {
            self.remove_plugin_contributions(&owner, load);
            self.commands.unregister_plugin(&owner);
        }
        let candidate = self
            .configuration
            .prepare_builtin_contributions(
                &self.commands,
                self.extension.contributions(),
                self.configuration.deferred_plugin_keymaps(),
                &self.pending_keymap_providers(),
            )
            .unwrap_or_else(|_| {
                self.configuration
                    .prepare_builtin_rules(self.extension.contributions())
            });
        if let Err(error) = self.commit_contribution_candidate(candidate) {
            self.show_error(error);
        }
        self.show_error(diagnostic);
    }
}

impl App {
    fn configured_indexes_pending(&self) -> bool {
        // Failed and disabled loads no longer have an outstanding index to wait for.
        self.configuration
            .expected_plugin_indexes()
            .iter()
            .any(|owner| {
                self.extension.load_id(owner).is_some()
                    && self.extension.contributions().get(owner).is_none()
            })
    }

    fn installed_packages_pending(&self) -> bool {
        self.extension.package_startup_request.is_some()
            || self
                .extension
                .package_candidates
                .values()
                .any(|candidate| candidate.installed.is_some())
    }

    fn finish_index_batch(&mut self, plugin: &str, load: PluginLoadId) -> Result<(), String> {
        let deferred = self.configuration.deferred_plugin_keymaps();
        if deferred && (self.configured_indexes_pending() || self.installed_packages_pending()) {
            return Ok(());
        }
        self.recompile_plugin_contributions()?;
        if let Err((failed_plugin, failed_load, error)) =
            self.extension.activate_pending(deferred, plugin, load)
        {
            self.remove_plugin_contributions(&failed_plugin, failed_load);
            let _ = self.extension.disable_load(&failed_plugin, failed_load);
            return Err(error);
        }
        self.configuration.complete_plugin_indexes();
        Ok(())
    }
}

impl App {
    pub(in crate::app) fn process_plugin_storage_events(&mut self) {
        self.extension.poll_storage();
    }
}

impl App {
    pub(in crate::app) fn refresh_plugin_host_events(&mut self) {
        let active_surface = if self.window_session.terminal_focused() {
            "terminal"
        } else if self.window_session.explorer_focused() {
            "explorer"
        } else if self
            .ui
            .private_surfaces
            .iter()
            .any(|surface| surface.focused)
        {
            "plugin"
        } else {
            "editor"
        }
        .to_string();
        let buffer = self.state.active_view().and_then(|view_id| {
            let view = self.state.views.get(view_id)?;
            let buffer = self.state.buffers.get(view.buffer)?;
            let (anchor, head, kind) =
                view.selection
                    .map_or((view.cursor.0, view.cursor.0, 0), |selection| {
                        (
                            selection.anchor.0,
                            selection.head.0,
                            match selection.kind {
                                crate::editor::SelectionKind::Character => 1,
                                crate::editor::SelectionKind::Line => 2,
                                crate::editor::SelectionKind::Block => 3,
                            },
                        )
                    });
            Some((
                view.buffer.data().as_ffi(),
                buffer.text.revision().0,
                view.cursor.0,
                anchor,
                head,
                kind,
            ))
        });
        let graph_revision = self.window_session.graph().revision().unwrap_or(0);
        let observation = (
            self.workspace.root_revision(),
            active_surface.clone(),
            buffer,
            graph_revision,
        );
        self.extension.observe(
            observation,
            self.workspace.roots().len(),
            self.ui.plugin_surfaces.len(),
            self.ui.private_surfaces.len(),
        );
    }
}

impl App {
    pub(in crate::app) fn process_plugin_events(&mut self) {
        self.finish_plugin_folder_open();
        let browser_view = self
            .search
            .plugin_browser_open()
            .then(|| self.search.picker_bookmark())
            .flatten();
        self.poll_plugin_callbacks();
        if let Some(request) = self.extension.expire_providers() {
            self.present_plugin_picker(request);
        }
        if self.persistence.restoring() {
            return;
        }
        if self.extension.invalidate_stale_confirmations(
            &self.state,
            &self.workspace.version(),
            self.configuration.binding_revision(),
            &self.input,
            &self.window_session,
        ) && self
            .input
            .prompt()
            .is_some_and(|prompt| prompt.kind == PromptKind::PluginConfirmation)
        {
            self.close_prompt();
        }
        let _ = self.extension.dispatch_confirmation_results(
            &self.state,
            &self.workspace.version(),
            self.configuration.binding_revision(),
        );
        self.present_next_plugin_confirmation();
        self.discard_stale_plugin_edits();
        let events = match self.extension.poll_owned() {
            Ok(events) => events,
            Err(error) => {
                self.discard_stale_plugin_edits();
                self.handle_plugin_worker_stopped(error);
                return;
            }
        };
        let refresh_browser =
            std::mem::take(&mut self.extension.browser_dirty) || !events.is_empty();
        // Package candidates can settle without ever changing the live catalog.
        let mut pending_keymap_providers = if self.extension.package_candidates.is_empty() {
            BTreeSet::new()
        } else {
            self.pending_keymap_providers()
        };
        let mut failure_diagnostic = None;
        for event in events {
            let event = match event {
                crate::plugins::OwnedPluginEvent::Executed(reply) => {
                    self.apply_plugin_execution(reply);
                    continue;
                }
                crate::plugins::OwnedPluginEvent::Other(event) => event,
            };
            match event {
                PluginEvent::Indexed {
                    load,
                    plugin,
                    index,
                } => {
                    if self.extension.take_rejected(&plugin, load) {
                        let _ = self.extension.disable_load(&plugin, load);
                        continue;
                    }
                    if self.extension.package_candidate(&plugin, load).is_some() {
                        if let Err(error) = self.index_package_candidate(index) {
                            self.report_package_load_failure(&plugin, load, error);
                        }
                        continue;
                    }
                    let checksum = index.checksum.clone();
                    let accepted = self
                        .extension
                        .install_index(index.clone())
                        .and_then(|()| self.register_indexed_plugin_commands(&index));
                    self.extension.reset_observation();
                    if accepted.is_ok() && index.storage.is_some() {
                        self.extension.install_storage(&plugin, load);
                    }
                    self.extension.queue_index(plugin.clone(), load, checksum);
                    if let Err(error) =
                        accepted.and_then(|()| self.finish_index_batch(&plugin, load))
                    {
                        self.abort_pending_index_batch(error);
                        continue;
                    }
                    if self.extension.index_pending(&plugin) {
                        continue;
                    }
                    self.health.record(
                        LogLevel::Info,
                        "plugins",
                        "indexed-and-accepted",
                        BTreeMap::from([
                            ("plugin".into(), plugin),
                            ("load".into(), load.0.to_string()),
                            ("contexts".into(), index.contexts.len().to_string()),
                        ]),
                    );
                }
                PluginEvent::Loaded { load, component } => {
                    let plugin = component.manifest.id.clone();
                    if self.extension.take_rejected(&plugin, load) {
                        let _ = self.extension.disable_load(&plugin, load);
                        continue;
                    }
                    if self.extension.package_candidate(&plugin, load).is_some() {
                        if let Err(error) =
                            self.finish_package_activation(&plugin, load, &component)
                        {
                            self.report_package_load_failure(&plugin, load, error);
                        }
                        continue;
                    }
                    if let Some(index) = self
                        .extension
                        .contributions()
                        .get(&plugin)
                        .filter(|index| index.plugin_load == load.0)
                    {
                        let declared = index.commands.keys().cloned().collect::<BTreeSet<_>>();
                        let activated = component
                            .activated_commands
                            .iter()
                            .cloned()
                            .collect::<BTreeSet<_>>();
                        if declared != activated {
                            self.remove_plugin_contributions(&plugin, load);
                            let _ = self.extension.disable_load(&plugin, load);
                            self.show_error(crate::plugins::PluginError::UndeclaredCommand);
                            continue;
                        }
                    }
                    let declarations = component
                        .manifest
                        .commands
                        .iter()
                        .filter(|(command, _)| component.activated_commands.contains(command))
                        .map(|(command, declaration)| (command.clone(), declaration.clone()))
                        .collect();
                    if let Err(error) = self.register_plugin_commands(&plugin, &declarations) {
                        self.show_error(error);
                        continue;
                    }
                    self.health.record(
                        LogLevel::Info,
                        "plugins",
                        "active",
                        BTreeMap::from([
                            ("plugin".into(), plugin),
                            (
                                "commands".into(),
                                component.activated_commands.len().to_string(),
                            ),
                        ]),
                    );
                }
                PluginEvent::CommandFailed { plugin, error, .. } => {
                    failure_diagnostic = Some(format!("Plugin {plugin}: {error}"));
                }
                PluginEvent::Executed(_) => self.show_error("unowned plugin execution event"),
                PluginEvent::Failed {
                    load,
                    plugin,
                    error,
                } => {
                    if self.extension.take_rejected(&plugin, load) {
                        continue;
                    }
                    if let Some(update) = self.extension.failed_package_load(&plugin, load, &error)
                    {
                        self.apply_extension_updates(vec![update]);
                        continue;
                    }
                    pending_keymap_providers.insert(plugin.clone());
                    self.extension.pending_index_activation.remove(&plugin);
                    let finish_deferred = self.configuration.deferred_plugin_keymaps()
                        && self
                            .configuration
                            .expected_plugin_indexes()
                            .contains(&plugin);
                    self.remove_plugin_contributions(&plugin, load);
                    self.commands.unregister_plugin(&plugin);
                    if finish_deferred && let Err(error) = self.finish_index_batch(&plugin, load) {
                        self.abort_pending_index_batch(error);
                    }
                    failure_diagnostic = Some(format!("Plugin {plugin}: {error}"));
                    self.health.record(
                        LogLevel::Error,
                        "plugins",
                        "failed",
                        BTreeMap::from([
                            ("plugin".into(), plugin),
                            ("error".into(), error.to_string()),
                        ]),
                    );
                }
            }
        }
        self.publish_installed_packages();
        if self.configuration.deferred_plugin_keymaps()
            && !self.installed_packages_pending()
            && !self.configured_indexes_pending()
            && let Some((plugin, load)) = self
                .extension
                .pending_index_activation
                .first_key_value()
                .map(|(plugin, (load, _))| (plugin.clone(), *load))
            && let Err(error) = self.finish_index_batch(&plugin, load)
        {
            self.abort_pending_index_batch(error);
        }
        self.report_settled_keymap_providers(&pending_keymap_providers);
        if refresh_browser {
            if browser_view.is_some()
                && (self.search.picker().is_none() || self.search.plugin_browser_open())
            {
                // Contribution changes invalidate picker bindings; keep the browser's query and selection.
                self.present_plugin_browser();
                self.search.restore_picker_view(browser_view, false);
                self.sync_picker_ui();
            } else {
                self.refresh_plugin_browser();
            }
        }
        self.discard_stale_plugin_edits();
        let _ = self.extension.dispatch_confirmation_results(
            &self.state,
            &self.workspace.version(),
            self.configuration.binding_revision(),
        );
        self.present_next_plugin_confirmation();
        if let Some(diagnostic) = failure_diagnostic {
            self.show_error(diagnostic);
        }
    }
}

impl App {
    pub(in crate::app) fn apply_plugin_ui_request(
        &mut self,
        index: &crate::plugins::PluginContributionIndex,
        request: crate::plugins::PluginUiRequest,
    ) -> crate::plugins::PluginUiEvent {
        match self.extension.prepare_window(
            index,
            request,
            &self.state,
            &self.window_session,
            self.configuration.settings().editor.ambiguous_width
                == crate::config::AmbiguousWidth::Wide,
        ) {
            WindowUpdate::Ready(event) => event,
            WindowUpdate::Publish(source, transition) => {
                let accepted = self.window_session.accept_plugin_transition(
                    *transition,
                    &self.state.window_version(),
                    source.catalog(),
                    &mut self.state,
                );
                source.finish(accepted, self.window_session.graph().revision())
            }
        }
    }
}

impl App {
    pub(in crate::app) fn apply_plugin_execution(
        &mut self,
        reply: crate::plugins::PluginExecutionReply,
    ) {
        let mut source = match self.extension.receive(
            reply,
            &self.workspace.version(),
            self.configuration.binding_revision(),
        ) {
            Ok(source) => source,
            Err(error) => {
                self.show_error(error);
                return;
            }
        };
        self.apply_plugin_call_source(&mut source);
        if let Some(error) = &source.execution.error {
            self.show_error(error);
        }
        let confirmation_source = source.confirmation_source.clone();
        let navigation_current =
            confirmation_source.navigation_current(&self.state, &self.window_session);
        let confirmation_result = source.confirmation_result;
        let source_actions_allowed = source.source_actions_allowed;
        let mut execution = source.execution;
        let load = source.load;
        let confirmation_requests = std::mem::take(&mut execution.confirmation_requests);
        if let Err(error) = self
            .extension
            .queue_confirmations(confirmation_source, confirmation_requests)
        {
            self.show_error(error);
        }
        if !source_actions_allowed {
            execution.messages.clear();
            execution.ui_requests.clear();
            execution.host_event_acknowledgements.clear();
            execution.storage_requests.clear();
            execution.provider_chunks.clear();
            execution.virtual_document_proposals.clear();
        }
        let plugin = execution.plugin.clone();
        if confirmation_result {
            let disallowed_ui = execution.ui_requests.iter().any(|request| {
                !matches!(
                    request.kind,
                    crate::plugins::PluginUiRequestKind::ReplaceUi { .. }
                        | crate::plugins::PluginUiRequestKind::ReplacePrivate { .. }
                        | crate::plugins::PluginUiRequestKind::PatchPrivate { .. }
                )
            });
            if disallowed_ui
                || !execution.storage_requests.is_empty()
                || !execution.provider_chunks.is_empty()
                || !execution.virtual_document_proposals.is_empty()
            {
                execution.ui_requests.clear();
                execution.storage_requests.clear();
                execution.provider_chunks.clear();
                execution.virtual_document_proposals.clear();
                self.show_error(
                    "plugin confirmation result may update existing plugin UI only".to_owned(),
                );
            }
        }
        let mut provider_continuations = Vec::new();
        if self.extension.is_active_load(&plugin, load) {
            self.extension
                .acknowledge(&plugin, load, &execution.host_event_acknowledgements);
            if let Some(policy) = self
                .extension
                .contributions()
                .get(&plugin)
                .filter(|index| index.plugin_load == load.0)
                .and_then(|index| index.storage.clone())
            {
                self.extension.request_storage(
                    &plugin,
                    load,
                    &policy,
                    std::mem::take(&mut execution.storage_requests),
                );
            }
            let workspace_revision = Some(self.workspace.root_revision());
            for proposal in std::mem::take(&mut execution.virtual_document_proposals) {
                let closing = matches!(
                    proposal.kind,
                    crate::plugins::PluginVirtualDocumentProposalKind::CloseAcknowledged
                );
                let result = self
                    .extension
                    .prepare_virtual(
                        &plugin,
                        load,
                        proposal,
                        navigation_current,
                        VirtualContext {
                            editor: &self.state,
                            windows: &self.window_session,
                            scratch: SCRATCH_LABEL,
                        },
                    )
                    .and_then(|staged| {
                        Self::accept_virtual_publication(&mut self.window_session, staged)
                    });
                match result {
                    Ok(state) => {
                        *self.state = state;
                        if !closing {
                            self.set_dashboard_visibility(false);
                        }
                        self.reconcile_editor_windows();
                    }
                    Err(error) => self.show_error(error),
                }
            }
            let buffer_revision = self
                .state
                .active_buffer_id()
                .and_then(|buffer| self.state.buffers.get(buffer))
                .map(|buffer| buffer.text.revision().0);
            for chunk in std::mem::take(&mut execution.provider_chunks) {
                match self.extension.provider_chunk(
                    &plugin,
                    load,
                    (workspace_revision, buffer_revision),
                    chunk,
                ) {
                    Ok(update) => {
                        if let Some(request) = update.refresh {
                            self.present_plugin_picker(request);
                        }
                        if let Some(command) = update.command {
                            provider_continuations.push(command);
                        }
                    }
                    Err(error) => self.show_error(error.detail),
                }
            }
        }
        for command in provider_continuations {
            self.process_effect(AppEffect::PluginExecute {
                plugin: plugin.clone(),
                command,
                arguments: Vec::new(),
            });
        }
        if !execution.ui_requests.is_empty()
            && self.extension.is_active_load(&plugin, load)
            && let Some(index) = self
                .extension
                .contributions()
                .get(&plugin)
                .filter(|index| index.plugin_load == load.0)
                .cloned()
        {
            let requests = std::mem::take(&mut execution.ui_requests);
            let events = requests
                .into_iter()
                .map(|request| self.apply_plugin_ui_request(&index, request))
                .collect::<Vec<_>>();
            self.extension.queue_ui(plugin.clone(), load, events);
            self.ui.plugin_surfaces = self.extension.window_snapshots();
            self.window_session.invalidate_native();
            self.reconcile_editor_windows();
        }
        for message in execution.messages {
            self.notify_message(message, false);
        }
        match source.preview {
            Ok(Some(key)) => self.begin_plugin_workspace_proposal(
                key,
                format!("plugin {}: {}", execution.plugin, execution.command),
            ),
            Ok(None) => {}
            Err(error) => self.show_error(error),
        }
        self.health.record(
            LogLevel::Info,
            "plugins",
            "executed",
            BTreeMap::from([
                ("plugin".into(), execution.plugin),
                ("command".into(), execution.command),
                ("request_id".into(), execution.request_id.to_string()),
            ]),
        );
    }
}

impl App {
    pub(in crate::app) fn discard_stale_plugin_edits(&mut self) {
        let (discarded, key) = self.extension.discard_stale_edits(
            &self.state,
            &self.workspace.version(),
            self.configuration.binding_revision(),
        );
        if discarded != 0 {
            self.show_error(format!(
                "{discarded} plugin invocations discarded before their results were accepted"
            ));
        }
        if let Some(key) = key {
            if self
                .workspace_edits
                .origin()
                .is_some_and(|origin| origin.same(&key))
            {
                self.abort_workspace_edit("plugin proposal source changed".into());
            } else {
                let _ = self.extension.finish_edit(&key);
            }
        }
    }
}

impl App {
    fn begin_plugin_workspace_proposal(
        &mut self,
        key: crate::app::workspace::edits::OriginKey,
        label: String,
    ) {
        let result = (|| {
            self.workspace_edits.check_begin()?;
            let prepared = self.extension.prepare_edit(
                &key,
                &self.state,
                &self.workspace.version(),
                self.configuration.binding_revision(),
            )?;
            let path = prepared.draft.canonical_path.clone();
            let ticket = self.workspace_edits.begin(
                self.workspace.version(),
                self.configuration.binding_revision(),
                prepared.sources,
            )?;
            let maximum = SEARCH_MAXIMUM_FILE_BYTES;
            let roots = self
                .workspace
                .roots()
                .iter()
                .map(|r| r.identity.canonical_path.clone())
                .collect();
            self.workspace_edits.start_files(
                ticket,
                label,
                Some(key.clone()),
                false,
                vec![crate::app::workspace::edits::InputFile::Read(path.clone())],
                (roots, maximum),
            )?;
            let snapshot = self
                .state
                .workspace_snapshot(
                    self.workspace_edits
                        .preparing_sources()
                        .ok_or("plugin workspace preparation missing")?,
                    &path,
                    maximum,
                )?
                .ok_or("plugin snapshot document closed")?;
            self.workspace_edits.open_snapshot(0, snapshot)?;
            self.workspace_edits.finish_preparation(
                vec![prepared.draft],
                self.workspace.edit_scope(),
                &self.workspace.version(),
                self.configuration.binding_revision(),
            )
        })();
        match result {
            Ok(()) => self.show_workspace_proposal(),
            Err(error) => {
                if self
                    .workspace_edits
                    .origin()
                    .is_some_and(|origin| origin.same(&key))
                {
                    self.abort_workspace_edit(error);
                } else {
                    let _ = self.extension.finish_edit(&key);
                    self.show_error(error);
                }
            }
        }
    }
}

impl App {
    pub(in crate::app) fn accept_virtual_publication(
        windows: &mut crate::app::windows::Windows,
        (source, transition): (
            VirtualPublication<'_>,
            crate::app::windows::EditorWindowCandidate,
        ),
    ) -> Result<EditorState, String> {
        let accepted =
            windows.accept_editor_transition(transition, source.before(), source.catalog())?;
        source.finish(accepted)
    }
}

impl App {
    pub(in crate::app) fn route_virtual_open(&mut self, result: Result<VirtualOpen, String>) {
        match result {
            Ok(VirtualOpen::Execute {
                plugin,
                load,
                command,
                document,
            }) => {
                if !self.execute_plugin_command(plugin.clone(), command, Vec::new()) {
                    self.extension
                        .cancel_virtual_request(&plugin, load, document);
                }
            }
            Ok(VirtualOpen::Existing(buffer)) => {
                let mut draft = self.state.draft();
                if let Err(error) = draft
                    .show_document(buffer)
                    .map_err(str::to_owned)
                    .and_then(|_| self.commit_editor_draft(draft, None, false))
                {
                    self.show_error(error);
                } else {
                    self.set_dashboard_visibility(false);
                    self.reconcile_editor_windows();
                }
            }
            Err(error) => self.show_error(error),
        }
    }
}

use slotmap::Key;

#[cfg(test)]
mod tests {
    use super::{ExtensionUpdate, PackageCandidate, PackageChange, RegistryUpdate};
    use crate::app::plugins::Plugins;
    use crate::plugins::PluginLimits;
    use crate::plugins::PluginLoadId;
    use crate::plugins::PluginManifest;
    use crate::tools::PackageManifest;
    use crate::tools::PackageSource;
    use crate::tools::RegistryEvent;
    use crate::tools::RegistryPackageMetadata;
    use crate::tools::RegistryResult;
    use crate::tools::RegistryWorker;
    use crate::tools::{LockedPackage, PackageDiscovery};
    use std::collections::BTreeMap;
    use std::time::Duration;
    use std::time::Instant;
    fn with_user_keymaps(
        root: &std::path::Path,
        keymaps: &str,
        expected_owners: &[&str],
    ) -> crate::config::LoadedConfig {
        let mut loaded = crate::config::load_bundled_config();
        loaded
            .installed_plugins
            .extend(expected_owners.iter().map(|owner| (*owner).to_owned()));
        let user = crate::config::parse_keymaps(keymaps, root.join("keymaps.toml"));
        assert!(user.diagnostic.is_none(), "{:?}", user.diagnostic);
        loaded.keymaps.path = user.path;
        loaded.keymaps.entries.extend(user.entries);
        loaded.keymaps.groups.extend(user.groups);
        loaded.keymaps.user_spans = user.user_spans;
        loaded.keymaps.user_group_spans = user.user_group_spans;
        loaded.keymaps.user_lines = user.user_lines;
        loaded
    }

    fn start_with_keymaps(
        root: &std::path::Path,
        keymaps: &str,
        expected_owners: &[&str],
    ) -> crate::app::App {
        crate::app::App::new(
            crate::app::startup::StartupTarget::Dashboard { root: root.into() },
            with_user_keymaps(root, keymaps, expected_owners),
            false,
            root.join("state"),
            false,
            false,
            std::sync::Arc::new(crate::clock::SystemClock),
        )
        .unwrap()
    }

    const DASHBOARD_USER_KEYS: &str = "[[key_group]]\nid = 'user.demo-menu'\ncontext = 'dashboard'\nprefix = 'x'\nlabel = 'Demo'\n\n[[keymap]]\nid = 'builtin.dashboard.surface.file-new.n'\ndisable = true\n";

    const DASHBOARD_PLUGIN_BINDING: &str = "plugin.demo.keymap.dashboard";

    fn dashboard_plugin_index() -> crate::plugins::PluginContributionIndex {
        let manifest = crate::app::tests::plugin_manifest("demo", DASHBOARD_PLUGIN_BINDING, "x y");
        crate::plugins::index_manifest_contributions(&manifest, 1, "demo.toml".into()).unwrap()
    }

    fn index_dashboard_plugin(app: &mut crate::app::App) {
        let index = dashboard_plugin_index();
        app.register_indexed_plugin_commands(&index).unwrap();
        app.extension.fixture_contribution(index).unwrap();
        app.recompile_plugin_contributions().unwrap();
    }

    #[test]
    fn plugin_contributions_use_the_keymap_accepted_at_startup() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(
            root.path(),
            "# User bindings\n[[keymap]]\nid = 'user.mine'\ncontext = 'editor'\nmode = 'normal'\nkeys = 'Q'\ncommand = 'no.such.command'\n",
            &["demo"],
        );
        index_dashboard_plugin(&mut app);

        let notice = app.ui.config_diagnostic.as_deref().unwrap();
        assert!(
            notice.starts_with("Using default keys. keymaps.toml, line 2: "),
            "{notice}"
        );
        assert!(notice.contains("no.such.command"), "{notice}");

        let keys = app.configuration.keymaps();
        assert_eq!(
            keys.inspect_id(DASHBOARD_PLUGIN_BINDING)
                .and_then(|binding| binding.command)
                .as_deref(),
            Some("plugin.demo.run")
        );
        assert!(keys.inspect_key("editor.normal", ":").is_some());
        assert!(keys.inspect_id("user.mine").is_none());
    }

    fn assert_dashboard_user_keys(app: &crate::app::App) {
        let keys = app.configuration.keymaps();
        assert!(
            keys.groups()
                .iter()
                .any(|group| group.id == "user.demo-menu" && group.prefix == "x")
        );
        assert!(keys.inspect_id(DASHBOARD_PLUGIN_BINDING).is_some());
        assert_eq!(
            keys.inspect_key("dashboard", "n").unwrap().operation,
            crate::app::configuration::KeymapOperation::Disable
        );
    }

    const DASHBOARD_PLUGIN_COMMAND: &str = "[[keymap]]\nid = 'user.demo-run'\ncontext = 'dashboard'\nkeys = 'Q'\ncommand = 'plugin.demo.run'\n\n[[keymap]]\nid = 'builtin.dashboard.surface.file-new.n'\ndisable = true\n";

    fn assert_dashboard_new_file_disabled(app: &crate::app::App) {
        assert_eq!(
            app.configuration
                .keymaps()
                .inspect_key("dashboard", "n")
                .unwrap()
                .operation,
            crate::app::configuration::KeymapOperation::Disable
        );
    }

    #[test]
    fn missing_startup_manifest_does_not_abort_a_healthy_index() {
        let root = tempfile::tempdir().unwrap();
        let mut app =
            start_with_keymaps(root.path(), DASHBOARD_PLUGIN_COMMAND, &["demo", "missing"]);
        let (manager, fixture) = crate::plugins::ExecutionFixture::new(8);
        app.extension.fixture_manager(manager);
        let missing = app
            .extension
            .index_from_store(
                "missing".into(),
                root.path().into(),
                "missing.toml".into(),
                "missing.wasm".into(),
                limits(),
            )
            .unwrap();

        fixture.indexed(dashboard_plugin_index());
        app.process_plugin_events();
        assert!(app.extension.index_pending("demo"));
        assert_dashboard_new_file_disabled(&app);

        fixture.load_failed(
            "missing",
            missing,
            crate::plugins::PluginError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
        );
        app.process_plugin_events();

        assert!(app.extension.contributions().get("missing").is_none());
        assert!(app.extension.contributions().get("demo").is_some());
        assert!(!app.extension.index_pending("demo"));
        // The host switches to Loading when it accepts the ActivateIndexed request.
        assert!(matches!(
            app.extension.manager.statuses.get("demo"),
            Some(crate::plugins::PluginStatus::Loading)
        ));
        assert_eq!(
            app.configuration
                .keymaps()
                .inspect_key("dashboard", "Q")
                .and_then(|binding| binding.command)
                .as_deref(),
            Some("plugin.demo.run")
        );
        assert_dashboard_new_file_disabled(&app);
        assert_eq!(app.ui.config_diagnostic, None);
    }

    #[test]
    fn startup_reports_skipped_global_key_only_after_provider_failure() {
        let global_shortcut = DASHBOARD_PLUGIN_COMMAND.replace("'dashboard'", "'global'");
        let warning = "left out settings Dusty doesn't have from config.toml: old.setting";
        for load_succeeds in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let mut loaded = with_user_keymaps(root.path(), &global_shortcut, &["demo"]);
            loaded.warning = Some(warning.into());
            let mut app = crate::app::App::new(
                crate::app::startup::StartupTarget::Dashboard {
                    root: root.path().into(),
                },
                loaded,
                false,
                root.path().join("state"),
                false,
                false,
                std::sync::Arc::new(crate::clock::SystemClock),
            )
            .unwrap();
            let (manager, fixture) = crate::plugins::ExecutionFixture::new(8);
            app.extension.fixture_manager(manager);

            let skipped = "Skipped Q: plugin demo isn't available.";
            assert_eq!(app.ui.notification.as_deref(), Some(warning));
            assert!(app.ui.config_diagnostic.is_none());
            assert!(
                !app.notices
                    .history()
                    .any(|(_, message)| message.contains(skipped))
            );

            if load_succeeds {
                fixture.indexed(dashboard_plugin_index());
            } else {
                fixture.load_failed(
                    "demo",
                    PluginLoadId(1),
                    crate::plugins::PluginError::Io(std::io::Error::from(
                        std::io::ErrorKind::NotFound,
                    )),
                );
            }
            app.process_plugin_events();

            assert!(app.notices.history().any(|(_, message)| message == warning));
            let logged_reasons = app
                .notices
                .history()
                .flat_map(|(_, message)| message.match_indices(skipped))
                .count();
            assert_eq!(logged_reasons, usize::from(!load_succeeds));
            if load_succeeds {
                assert!(app.ui.config_diagnostic.is_none());
                assert_eq!(
                    app.configuration
                        .keymaps()
                        .inspect_key("dashboard", "Q")
                        .and_then(|binding| binding.command)
                        .as_deref(),
                    Some("plugin.demo.run")
                );
            } else {
                assert_eq!(app.ui.config_diagnostic.as_deref(), Some(skipped));
            }
            assert_dashboard_new_file_disabled(&app);
        }
    }

    #[test]
    fn a_host_command_override_survives_disabling_the_plugin_that_owned_its_id() {
        let root = tempfile::tempdir().unwrap();
        let keys = format!(
            "[[keymap]]\nid = '{DASHBOARD_PLUGIN_BINDING}'\ncontext = 'dashboard'\nkeys = 'Q'\ncommand = 'health.open'\n"
        );
        let mut app = start_with_keymaps(root.path(), &keys, &["demo"]);
        let (manager, fixture) = crate::plugins::ExecutionFixture::new(8);
        app.extension.fixture_manager(manager);
        fixture.indexed(dashboard_plugin_index());
        app.process_plugin_events();

        let shortcut = |app: &crate::app::App| {
            app.configuration
                .keymaps()
                .inspect_key("dashboard", "Q")
                .and_then(|binding| binding.command)
        };
        assert_eq!(shortcut(&app).as_deref(), Some("health.open"));
        app.disable_browser_plugin("demo");

        assert!(app.extension.contributions().get("demo").is_none());
        assert_eq!(shortcut(&app).as_deref(), Some("health.open"));
        assert!(app.ui.config_diagnostic.is_none());
    }

    #[test]
    fn disabling_or_failing_the_provider_only_sets_aside_its_shortcuts() {
        for disable in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let mut app = start_with_keymaps(root.path(), "", &["demo"]);
            let (manager, fixture) = crate::plugins::ExecutionFixture::new(8);
            app.extension.fixture_manager(manager);
            fixture.indexed(dashboard_plugin_index());
            app.process_plugin_events();
            app.apply_configuration_candidate(
                with_user_keymaps(root.path(), DASHBOARD_PLUGIN_COMMAND, &["demo"]),
                false,
            )
            .unwrap();
            assert!(
                app.configuration
                    .keymaps()
                    .inspect_id("user.demo-run")
                    .is_some()
            );
            assert!(app.ui.config_diagnostic.is_none());

            let detail = format!(
                "{}, line 1: Skipped Q: plugin demo isn't available.",
                root.path().join("keymaps.toml").display()
            );
            let message_count = |app: &crate::app::App| {
                app.notices
                    .history()
                    .filter(|(_, message)| message == &detail)
                    .count()
            };
            assert_eq!(message_count(&app), 0);
            if disable {
                app.disable_browser_plugin("demo");
            } else {
                fixture.load_failed(
                    "demo",
                    PluginLoadId(1),
                    crate::plugins::PluginError::InvalidComponent,
                );
                app.process_plugin_events();
            }

            assert!(
                app.configuration
                    .keymaps()
                    .inspect_id("user.demo-run")
                    .is_none()
            );
            assert_dashboard_new_file_disabled(&app);
            assert_eq!(
                app.ui.config_diagnostic.as_deref(),
                Some("Skipped Q: plugin demo isn't available.")
            );
            assert_eq!(
                message_count(&app),
                1,
                "the full reason must be in :messages"
            );
            app.recompile_plugin_contributions().unwrap();
            assert_eq!(message_count(&app), 1);
        }
    }

    #[test]
    fn losing_a_plugin_default_only_parks_its_user_group() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(root.path(), DASHBOARD_USER_KEYS, &["demo"]);
        index_dashboard_plugin(&mut app);
        assert_eq!(app.ui.config_diagnostic, None);

        app.remove_plugin_contributions("demo", PluginLoadId(1));

        assert!(
            !app.configuration
                .keymaps()
                .groups()
                .iter()
                .any(|group| group.id == "user.demo-menu")
        );
        assert_dashboard_new_file_disabled(&app);
        assert_eq!(
            app.ui.config_diagnostic.as_deref(),
            Some("Skipped x: plugin demo isn't available.")
        );
        index_dashboard_plugin(&mut app);
        assert_dashboard_user_keys(&app);
        assert!(app.ui.config_diagnostic.is_none());
    }

    #[test]
    fn strict_reload_uses_the_current_catalog_and_keeps_the_last_good_keys() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(root.path(), "", &["demo"]);
        index_dashboard_plugin(&mut app);
        let loaded = with_user_keymaps(root.path(), DASHBOARD_USER_KEYS, &["demo"]);
        app.apply_configuration_candidate(loaded, false).unwrap();
        assert_dashboard_user_keys(&app);
        let revision = app.configuration.binding_revision();

        let broken = "[[keymap]]\nid = 'user.bad-context'\ncontext = 'dashbaord'\nkeys = 'Q'\ncommand = 'health.open'\n\n[[keymap]]\nid = 'user.bad-key'\ncontext = 'dashboard'\nkeys = 'nonsense'\ncommand = 'health.open'\n";
        app.finish_configuration_reload(crate::config::ConfigurationWorkerResult {
            reload_id: 1,
            automatic: false,
            candidate: Ok(with_user_keymaps(root.path(), broken, &["demo"])),
        });
        let notice = app.ui.notification.as_deref().unwrap();
        assert!(
            notice.starts_with("keymaps.toml, line 1: keymap id `user.bad-context`"),
            "{notice}"
        );
        assert!(
            notice.contains("keymaps.toml, line 7: keymap id `user.bad-key`"),
            "{notice}"
        );
        assert!(
            notice.ends_with(". Kept previous settings and keys."),
            "{notice}"
        );
        assert_eq!(app.configuration.binding_revision(), revision);
        assert_dashboard_user_keys(&app);
    }

    #[test]
    fn reload_rejects_a_user_group_whose_only_binding_was_deleted() {
        let root = tempfile::tempdir().unwrap();
        let group = "[[key_group]]\nid = 'user.dashboard-group'\ncontext = 'dashboard'\nprefix = 'f10'\nlabel = 'User group'\n";
        let binding = "[[keymap]]\nid = 'user.dashboard-binding'\ncontext = 'dashboard'\nkeys = 'f10 a'\ncommand = 'health.open'\n";
        let mut app = start_with_keymaps(root.path(), &format!("{group}\n{binding}"), &[]);
        let width = app.configuration.settings().editor.tab_width;
        let revision = app.configuration.binding_revision();
        assert!(
            app.configuration
                .keymaps()
                .inspect_id("user.dashboard-binding")
                .is_some()
        );

        let mut loaded = with_user_keymaps(root.path(), group, &[]);
        loaded.config.editor.tab_width = width + 1;
        let error = app
            .apply_configuration_candidate(loaded, false)
            .unwrap_err();
        assert!(
            error.to_string().contains("no shortcut starts with"),
            "{error}"
        );
        assert_eq!(app.configuration.settings().editor.tab_width, width);
        assert_eq!(app.configuration.binding_revision(), revision);
        let keymaps = app.configuration.keymaps();
        assert!(keymaps.inspect_id("user.dashboard-binding").is_some());
        assert!(
            keymaps
                .groups()
                .iter()
                .any(|group| group.id == "user.dashboard-group")
        );
    }

    #[test]
    fn reload_rejects_a_user_binding_missing_keys_even_with_a_live_plugin_command() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(root.path(), "", &["demo"]);
        index_dashboard_plugin(&mut app);
        let binding = "[[keymap]]\nid = 'user.plugin-command'\ncontext = 'dashboard'\ncommand = 'plugin.demo.run'\n";
        let good = format!("{binding}keys = 'f10 a'\n");
        app.apply_configuration_candidate(with_user_keymaps(root.path(), &good, &["demo"]), false)
            .unwrap();
        let width = app.configuration.settings().editor.tab_width;
        let revision = app.configuration.binding_revision();

        let mut loaded = with_user_keymaps(root.path(), binding, &["demo"]);
        loaded.config.editor.tab_width = width + 1;
        let error = app
            .apply_configuration_candidate(loaded, false)
            .unwrap_err();
        assert!(error.to_string().contains("requires `keys`"), "{error}");
        assert_eq!(app.configuration.settings().editor.tab_width, width);
        assert_eq!(app.configuration.binding_revision(), revision);
        assert_eq!(
            app.configuration
                .keymaps()
                .inspect_id("user.plugin-command")
                .unwrap()
                .keys,
            "f10 a",
        );
    }

    #[test]
    fn plugin_conflict_names_the_skipped_and_retained_shortcuts() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(root.path(), "", &[]);
        index_dashboard_plugin(&mut app);
        let manifest =
            crate::app::tests::plugin_manifest("other", "plugin.other.keymap.dashboard", "x y");
        let index = crate::plugins::index_manifest_contributions(&manifest, 2, "other.toml".into())
            .unwrap();
        app.check_package_index(&index, Some(PackageChange::Install))
            .unwrap();
        app.register_indexed_plugin_commands(&index).unwrap();
        app.extension.fixture_contribution(index).unwrap();
        app.recompile_plugin_contributions().unwrap();
        assert_eq!(
            app.ui.config_diagnostic.as_deref(),
            Some(
                "Skipped shortcut \"plugin.other.keymap.dashboard\" (x y); it conflicts with \"plugin.demo.keymap.dashboard\"."
            ),
        );
    }

    #[test]
    fn an_unrelated_index_does_not_relog_a_keymap_error() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(
            root.path(),
            "[[keymap]]\nid = 'builtin.dashboard.typo'\ndisable = true\n",
            &[],
        );
        let before = app.ui.config_diagnostic.clone();
        let messages = app.notices.history().count();
        assert!(
            before
                .as_deref()
                .is_some_and(|notice| notice.contains("line 1: keymap 1"))
        );
        index_dashboard_plugin(&mut app);
        assert_eq!(app.ui.config_diagnostic, before);
        assert_eq!(app.notices.history().count(), messages);
    }

    #[test]
    fn installing_the_first_plugin_can_restore_the_user_keymap() {
        let root = tempfile::tempdir().unwrap();
        let mut app = start_with_keymaps(root.path(), DASHBOARD_USER_KEYS, &[]);
        assert!(app.configuration.expected_plugin_indexes().is_empty());
        assert!(app.ui.config_diagnostic.is_some());

        app.check_package_index(&dashboard_plugin_index(), Some(PackageChange::Install))
            .unwrap();
        index_dashboard_plugin(&mut app);

        assert_dashboard_user_keys(&app);
        assert_eq!(app.ui.config_diagnostic, None);
    }

    /// Real guests run here: the budget is the tests' own wait (`PLUGIN_TEST_TICKS`), not the
    /// bundled 100 ms that one stall on a loaded runner outlasts.
    fn limits() -> PluginLimits {
        PluginLimits {
            epoch_deadline_ticks: crate::app::tests::PLUGIN_TEST_TICKS,
            ..PluginLimits::default()
        }
    }
    fn locked(name: &str) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: "1.0.0".into(),
            source: PackageSource::Registry {
                registry: "https://example.invalid".into(),
                version: "1.0.0".into(),
                archive_sha256: "0".repeat(64),
            },
            content_sha256: "0".repeat(64),
            activation_sha256: None,
            dependencies: Vec::new(),
        }
    }

    fn prepared(load: u64, token: u64) -> PackageCandidate {
        PackageCandidate {
            load: PluginLoadId(load),
            name: "demo".into(),
            token: Some(token),
            installed: None,
            change: Some(PackageChange::Update),
            index: None,
            runtime_ready: false,
        }
    }

    fn guest_package(name: &str) -> (PackageManifest, PluginManifest, Vec<u8>) {
        use sha2::{Digest, Sha256};
        let core = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/retained-guest/retained-guest.core.wasm"
        ));
        let bytes = wit_component::ComponentEncoder::default()
            .module(core)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let mut plugin = crate::app::plugins::fixture_manifest(digest.clone());
        plugin.id = name.into();
        plugin.capabilities = [crate::plugins::PluginCapability::ShowMessage].into();
        plugin.contributions = Default::default();
        let mut package = locked(name);
        if let PackageSource::Registry { archive_sha256, .. } = &mut package.source {
            *archive_sha256 = digest;
        }
        (
            PackageManifest {
                name: package.name,
                version: package.version,
                component: "plugin.wasm".into(),
                source: package.source,
                dependencies: Vec::new(),
            },
            plugin,
            bytes,
        )
    }

    fn settle_packages(app: &mut crate::app::App) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.process_package_events();
            app.process_plugin_events();
            if app.extension.pending_packages() == 0 && app.extension.package_candidates.is_empty()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "package operation did not finish"
            );
            std::thread::yield_now();
        }
    }

    fn store(dir: &tempfile::TempDir) -> crate::tools::PackageStore {
        crate::tools::PackageStore::new(dir.path().join("packages"), 1024 * 1024, 64 * 1024 * 1024)
            .unwrap()
    }

    fn install_guest(manager: &crate::tools::PackageStore, name: &str) {
        let (manifest, plugin, bytes) = guest_package(name);
        let object = manager.stage_verified_object(&manifest, &bytes).unwrap();
        manager
            .commit_prepared(&manifest, &plugin, &object)
            .unwrap();
    }

    fn guest_ordinal(app: &mut crate::app::App, plugin: &str, ordinal: u64) {
        let mut invocation = crate::plugins::empty_execution_request();
        invocation.plugin = plugin.into();
        invocation.command = "confirmationprobe".into();
        app.extension.manager.execute(invocation).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            for event in app.extension.manager.poll().unwrap() {
                match event {
                    crate::plugins::PluginEvent::Executed(reply) => {
                        assert!(
                            reply.messages.iter().any(|message| message
                                .starts_with(&format!("confirmationprobe:{ordinal}:"))),
                            "guest counter did not retain its value: {:?}",
                            reply.messages
                        );
                        return;
                    }
                    crate::plugins::PluginEvent::Failed { error, .. }
                    | crate::plugins::PluginEvent::CommandFailed { error, .. } => panic!("{error}"),
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "guest reply did not arrive");
            std::thread::yield_now();
        }
    }

    fn guest_with_panel(name: &str) -> (PackageManifest, PluginManifest, Vec<u8>) {
        let (package, mut plugin, bytes) = guest_package(name);
        let mut panel = crate::app::plugins::fixture_manifest(plugin.component_sha256.clone())
            .contributions
            .content_types
            .remove(0);
        panel.id = format!("plugin.{name}.panel");
        panel.command = format!("plugin.{name}.drive");
        plugin.contributions.content_types.push(panel);
        (package, plugin, bytes)
    }

    fn commit_guest(
        store: &crate::tools::PackageStore,
        (package, plugin, bytes): (PackageManifest, PluginManifest, Vec<u8>),
    ) {
        let object = store.stage_verified_object(&package, &bytes).unwrap();
        store.commit_prepared(&package, &plugin, &object).unwrap();
    }

    #[test]
    fn startup_restores_user_keys_when_an_installed_plugin_supplies_the_group() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (package, mut plugin, bytes) = guest_package("demo");
        plugin
            .contributions
            .keymaps
            .push(crate::plugins::PluginKeymapContribution {
                id: DASHBOARD_PLUGIN_BINDING.into(),
                context: "dashboard".into(),
                mode: String::new(),
                keys: "x y".into(),
                command: "plugin.demo.drive".into(),
                arguments: BTreeMap::new(),
            });
        commit_guest(&store(&state), (package, plugin, bytes));

        let mut loaded = with_user_keymaps(root.path(), DASHBOARD_USER_KEYS, &["demo"]);
        loaded.config.plugins.epoch_deadline_ticks = crate::app::tests::PLUGIN_TEST_TICKS;
        let mut app = crate::app::App::new(
            crate::app::startup::StartupTarget::Dashboard {
                root: root.path().into(),
            },
            loaded,
            false,
            state.path().into(),
            false,
            false,
            std::sync::Arc::new(crate::clock::SystemClock),
        )
        .unwrap();

        let keys = app.configuration.keymaps();
        assert!(
            !keys
                .groups()
                .iter()
                .any(|group| group.id == "user.demo-menu")
        );
        assert_eq!(
            keys.inspect_key("dashboard", "n").unwrap().operation,
            crate::app::configuration::KeymapOperation::Command,
        );
        assert!(
            app.ui
                .config_diagnostic
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Using default keys.")),
        );

        app.extension.start_packages();
        settle_packages(&mut app);

        assert_dashboard_user_keys(&app);
        assert_eq!(app.ui.config_diagnostic, None);
    }

    fn config_with_package_rules(other_selector: &str) -> crate::config::LoadedConfig {
        use crate::config::{WindowPlacementConfig, WindowRuleConfig};
        let mut loaded = crate::config::load_bundled_config();
        for (name, selector) in [("demo", "plugin.demo.panel"), ("other", other_selector)] {
            let command = format!("plugin.{name}.drive");
            loaded.installed_plugins.insert(name.into());
            loaded
                .config
                .command_labels
                .insert(command.clone(), format!("Drive {name}"));
            loaded
                .config
                .input
                .command_aliases
                .insert(format!("{name}drive"), command);
            loaded.config.windows.rules.push(WindowRuleConfig {
                content_type: selector.into(),
                placement: WindowPlacementConfig::Docked {
                    edge: "right".into(),
                    size: 24,
                },
            });
        }
        loaded
    }

    fn set_startup_config(app: &mut crate::app::App, mut loaded: crate::config::LoadedConfig) {
        loaded.config.plugins.epoch_deadline_ticks = crate::app::tests::PLUGIN_TEST_TICKS;
        let (resolved, notice) = crate::app::configuration::resolve::resolve_startup_configuration(
            &app.commands,
            loaded,
            false,
        )
        .unwrap();
        app.configuration.install_fixture(resolved);
        app.ui.config_diagnostic = notice;
    }

    fn reload_twice_with_unchanged_warnings(
        app: &mut crate::app::App,
        mut loaded: crate::config::LoadedConfig,
        warnings: &[&str],
    ) {
        loaded.config.plugins.epoch_deadline_ticks = crate::app::tests::PLUGIN_TEST_TICKS;
        let assert_warnings = |app: &crate::app::App| {
            for warning in warnings {
                assert_eq!(
                    app.notices
                        .history()
                        .filter(|(_, message)| message.as_str() == *warning)
                        .count(),
                    1,
                    "expected one entry in :messages for {warning}"
                );
                assert!(
                    app.ui
                        .config_diagnostic
                        .as_ref()
                        .is_some_and(|summary| summary.contains(*warning))
                );
            }
        };
        assert_warnings(app);
        for (reload_id, tab_width) in [(1, 3), (2, 6)] {
            loaded.config.editor.tab_width = tab_width;
            app.finish_configuration_reload(crate::config::ConfigurationWorkerResult {
                reload_id,
                automatic: true,
                candidate: Ok(loaded.clone()),
            });
            assert_eq!(app.configuration.settings().editor.tab_width, tab_width);
            assert_eq!(app.state.policy.tab_width, tab_width);
            assert_warnings(app);
        }
    }

    fn assert_panel_rules(app: &crate::app::App, expected: &[crate::config::WindowRuleConfig]) {
        assert_eq!(app.configuration.settings().windows.rules, expected);
    }

    #[test]
    fn startup_accepts_config_references_to_both_staged_packages() {
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        for name in ["demo", "other"] {
            commit_guest(&manager, guest_with_panel(name));
        }
        let loaded = config_with_package_rules("plugin.other.panel");
        let expected = loaded.config.windows.rules.clone();
        let aliases = loaded.config.input.command_aliases.clone();
        set_startup_config(&mut app, loaded);
        app.extension.start_packages();
        settle_packages(&mut app);

        assert_panel_rules(&app, &expected);
        assert_eq!(app.configuration.settings().input.command_aliases, aliases);
        for name in ["demo", "other"] {
            assert!(
                app.extension
                    .is_active_load(name, app.extension.load_id(name).unwrap())
            );
        }
    }

    #[test]
    fn startup_skips_only_the_misspelled_plugin_window_rule() {
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        for name in ["demo", "other"] {
            commit_guest(&manager, guest_with_panel(name));
        }
        let loaded = config_with_package_rules("plugin.other.pnael");
        let mut expected = loaded.config.windows.rules.clone();
        expected.retain(|rule| rule.content_type != "plugin.other.pnael");
        let aliases = loaded.config.input.command_aliases.clone();
        set_startup_config(&mut app, loaded.clone());
        app.extension.start_packages();
        settle_packages(&mut app);
        reload_twice_with_unchanged_warnings(
            &mut app,
            loaded,
            &["Window rule \"plugin.other.pnael\" doesn't match any plugin panel."],
        );

        assert_panel_rules(&app, &expected);
        assert_eq!(app.configuration.settings().input.command_aliases, aliases);
        for name in ["demo", "other"] {
            assert!(
                app.extension
                    .is_active_load(name, app.extension.load_id(name).unwrap())
            );
        }
        assert_eq!(
            app.ui.config_diagnostic.as_deref(),
            Some("Window rule \"plugin.other.pnael\" doesn't match any plugin panel."),
        );

        let index = app.extension.contributions().get("demo").unwrap().clone();
        let expected_graph_revision = app.window_session.graph().revision().unwrap();
        let event = app.apply_plugin_ui_request(
            &index,
            crate::plugins::PluginUiRequest {
                token: 1,
                kind: crate::plugins::PluginUiRequestKind::OpenWindow {
                    expected_graph_revision,
                    content_type: "plugin.demo.panel".into(),
                    title: "Demo".into(),
                    placement: crate::plugins::PluginRequestedPlacement::Floating {
                        x: 5,
                        y: 3,
                        width: 24,
                        height: 7,
                    },
                    tree: Some(crate::plugins::PluginUiTree {
                        revision: 1,
                        root: 1,
                        nodes: vec![crate::plugins::PluginUiNode {
                            id: 1,
                            kind: crate::plugins::PluginUiNodeKind::Text,
                            text: "retained plugin".into(),
                            style: crate::plugins::PluginUiStyle::Accent,
                            action: None,
                            children: Vec::new(),
                        }],
                    }),
                },
            },
        );
        assert!(event.error.is_none(), "{:?}", event.error);
        let window = app.extension.window_snapshots()[0].window;
        assert_eq!(
            app.window_session.graph().window(window).unwrap().placement,
            crate::windows::WindowPlacement::Docked {
                edge: crate::windows::DockEdge::Right,
                size: 24
            },
        );
    }

    #[test]
    fn startup_skips_only_the_plugin_binding_that_conflicts_with_the_leader() {
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        for (name, keys) in [("demo", "<leader> z z"), ("other", "f12 z z")] {
            let (package, mut plugin, bytes) = guest_package(name);
            for (suffix, keys) in [
                ("conflict", keys),
                (
                    "independent",
                    if name == "demo" { "f11 z a" } else { "f11 z b" },
                ),
            ] {
                plugin
                    .contributions
                    .keymaps
                    .push(crate::plugins::PluginKeymapContribution {
                        id: format!("plugin.{name}.keymap.{suffix}"),
                        context: "dashboard".into(),
                        mode: String::new(),
                        keys: keys.into(),
                        command: format!("plugin.{name}.drive"),
                        arguments: BTreeMap::new(),
                    });
            }
            commit_guest(&manager, (package, plugin, bytes));
        }
        let mut loaded = crate::config::load_bundled_config();
        loaded.config.input.keymaps.leader = "f12".into();
        set_startup_config(&mut app, loaded.clone());
        app.extension.start_packages();
        settle_packages(&mut app);
        reload_twice_with_unchanged_warnings(
            &mut app,
            loaded,
            &[
                "Skipped shortcut \"plugin.other.keymap.conflict\" (f12 z z); it conflicts with \"plugin.demo.keymap.conflict\".",
            ],
        );

        let keymaps = app.configuration.keymaps();
        assert!(keymaps.inspect_id("plugin.demo.keymap.conflict").is_some());
        assert!(keymaps.inspect_id("plugin.other.keymap.conflict").is_none());
        for name in ["demo", "other"] {
            assert!(
                app.extension
                    .is_active_load(name, app.extension.load_id(name).unwrap())
            );
            assert!(
                keymaps
                    .inspect_id(&format!("plugin.{name}.keymap.independent"))
                    .is_some()
            );
        }
        let notice = app
            .ui
            .config_diagnostic
            .as_deref()
            .expect("visible shortcut notice");
        assert!(notice.contains("plugin.other.keymap.conflict") && notice.contains("conflicts"));
    }

    #[test]
    fn installing_a_conflicting_plugin_group_keeps_user_shortcuts() {
        let ((root, _state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let mut loaded = crate::config::load_bundled_config();
        loaded.keymaps.groups.push(crate::config::KeyGroupEntry {
            id: "user.group.dashboard".into(),
            context: Some("dashboard".into()),
            mode: String::new(),
            prefix: Some("f10".into()),
            label: Some("User group".into()),
            icon: None,
            order: 0,
            disable: false,
        });
        for (id, keys) in [
            ("user.shortcut.grouped", "f10 a"),
            ("user.shortcut.other", "f11 y y"),
        ] {
            loaded.keymaps.entries.push(crate::config::KeymapEntry {
                id: id.into(),
                context: Some("dashboard".into()),
                mode: String::new(),
                keys: Some(keys.into()),
                command: Some("health.open".into()),
                arguments: BTreeMap::new(),
                unmap: false,
                disable: false,
                pass_through: false,
                desc: None,
            });
        }
        set_startup_config(&mut app, loaded);
        let (package, mut plugin, bytes) = guest_package("demo");
        plugin
            .contributions
            .keymaps
            .push(crate::plugins::PluginKeymapContribution {
                id: "plugin.demo.keymap.dashboard".into(),
                context: "dashboard".into(),
                mode: String::new(),
                keys: "f10 b".into(),
                command: "plugin.demo.drive".into(),
                arguments: BTreeMap::new(),
            });
        plugin
            .contributions
            .key_groups
            .push(crate::plugins::PluginKeyGroupContribution {
                id: "plugin.demo.group.dashboard".into(),
                context: "dashboard".into(),
                mode: String::new(),
                prefix: "f10".into(),
                label: "Plugin group".into(),
                icon: None,
                order: 0,
            });
        request_guest_install(&mut app, root.path(), (package, plugin, bytes), false);
        settle_packages(&mut app);

        assert!(
            app.extension
                .is_active_load("demo", app.extension.load_id("demo").unwrap())
        );
        let keymaps = app.configuration.keymaps();
        for id in [
            "user.shortcut.grouped",
            "user.shortcut.other",
            "plugin.demo.keymap.dashboard",
        ] {
            assert!(keymaps.inspect_id(id).is_some(), "{id}");
        }
        assert!(
            keymaps
                .groups()
                .iter()
                .any(|group| group.id == "user.group.dashboard")
        );
        assert!(
            !keymaps
                .groups()
                .iter()
                .any(|group| group.id == "plugin.demo.group.dashboard")
        );
        assert_eq!(
            app.ui.config_diagnostic.as_deref(),
            Some(
                "Skipped key group \"plugin.demo.group.dashboard\" (f10); it conflicts with \"user.group.dashboard\"."
            ),
        );
    }

    fn wait_for_installed_candidate(app: &mut crate::app::App, name: &str) -> PluginLoadId {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Stop at discovery: the app has staged a load but hasn't accepted its index.
            app.process_package_events();
            if let Some(candidate) = app.extension.package_candidates.get(name) {
                return candidate.load;
            }
            assert!(
                Instant::now() < deadline,
                "installed package was not staged"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn adding_a_root_does_not_cancel_an_installed_package_activation() {
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        install_guest(&store(&state), "demo");
        app.extension.start_packages();
        let load = wait_for_installed_candidate(&mut app, "demo");
        let added = tempfile::tempdir().unwrap();
        app.workspace
            .fixture_add_root(
                "added".into(),
                crate::files::RootIdentity::capture(added.path()).unwrap(),
            )
            .unwrap();
        app.invalidate_root_scoped_operations();

        settle_packages(&mut app);
        assert!(app.extension.is_active_load("demo", load));
    }

    #[test]
    fn installed_activation_delivers_workspace_metadata_after_a_root_is_removed() {
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let added = tempfile::tempdir().unwrap();
        let root = app
            .workspace
            .fixture_add_root(
                "added".into(),
                crate::files::RootIdentity::capture(added.path()).unwrap(),
            )
            .unwrap();
        let (package, mut plugin, bytes) = guest_package("demo");
        plugin
            .capabilities
            .insert(crate::plugins::PluginCapability::EventSubscriptions);
        plugin.contributions.subscriptions =
            crate::app::plugins::fixture_manifest(plugin.component_sha256.clone())
                .contributions
                .subscriptions;
        commit_guest(&store(&state), (package, plugin, bytes));

        app.extension.start_packages();
        let load = wait_for_installed_candidate(&mut app, "demo");
        app.remove_workspace_root(root);
        assert!(
            !app.workspace
                .roots()
                .iter()
                .any(|candidate| candidate.id == root)
        );
        let expected = format!("roots={}", app.workspace.roots().len());
        settle_packages(&mut app);
        assert!(app.extension.is_active_load("demo", load));

        app.refresh_plugin_host_events();
        let events = app.extension.deliver("demo", load);
        assert!(events.iter().any(|event| {
            event.kind == crate::plugins::PluginSubscriptionKind::WorkspaceMetadataChanged
                && event.revision == app.workspace.root_revision()
                && event.payload == expected
        }));
    }

    #[test]
    fn failed_package_update_keeps_the_running_guests_state() {
        use crate::tools::PackageOperation;
        let ((root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        install_guest(&manager, "demo");
        app.extension.start_packages();
        settle_packages(&mut app);
        let old = app
            .extension
            .load_id("demo")
            .expect("old version is running");
        guest_ordinal(&mut app, "demo", 1);

        let (mut manifest, mut plugin, bytes) = guest_package("demo");
        manifest.version = "2.0.0".into();
        plugin.version = "2.0.0".into();
        let package_manifest = root.path().join("package.json");
        let plugin_manifest = root.path().join("plugin.toml");
        let component = root.path().join("plugin.wasm");
        std::fs::write(&package_manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        std::fs::write(&plugin_manifest, toml::to_string(&plugin).unwrap()).unwrap();
        std::fs::write(&component, bytes).unwrap();
        // The guest and its contributions are valid; only the lock write fails.
        std::fs::create_dir(
            state
                .path()
                .join("packages/dusty-packages.lock.previous.json"),
        )
        .unwrap();
        app.extension
            .request_package(PackageOperation::Install {
                package_manifest,
                plugin_manifest,
                component,
                roots: vec![crate::files::canonicalize(root.path()).unwrap()],
                limits: limits(),
                update: true,
            })
            .unwrap();
        settle_packages(&mut app);
        assert_eq!(manager.read_lock().unwrap().packages[0].version, "1.0.0");
        assert_eq!(app.extension.load_id("demo"), Some(old));
        assert_eq!(
            app.ui.notification.as_deref(),
            Some("Couldn't update \"demo\"; kept version 1.0.0.")
        );
        guest_ordinal(&mut app, "demo", 2);
    }

    #[test]
    fn broken_package_does_not_unload_or_revert_a_healthy_package() {
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        install_guest(&manager, "demo");
        app.extension.start_packages();
        settle_packages(&mut app);
        let old = app
            .extension
            .load_id("demo")
            .expect("healthy package is running");

        install_guest(&manager, "broken");
        let committed = manager.read_lock().unwrap();
        let digest = committed
            .packages
            .iter()
            .find(|package| package.name == "broken")
            .unwrap()
            .activation_sha256
            .as_ref()
            .unwrap();
        std::fs::remove_file(
            state
                .path()
                .join("packages/activations")
                .join(format!("{digest}.toml")),
        )
        .unwrap();
        app.extension.start_packages();
        settle_packages(&mut app);
        assert_eq!(manager.read_lock().unwrap(), committed);
        assert_eq!(app.extension.load_id("demo"), Some(old));
        assert!(app.extension.load_id("broken").is_none());
        assert_eq!(
            app.extension.browser_packages["broken"].error.as_deref(),
            Some("Couldn't load package \"broken\".")
        );
    }

    #[test]
    fn package_remove_keeps_the_surviving_guest_instance() {
        use crate::tools::PackageOperation;
        let ((_root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        install_guest(&manager, "demo");
        install_guest(&manager, "other");
        app.extension.start_packages();
        settle_packages(&mut app);
        assert!(app.extension.load_id("other").is_some());
        let survivor = app
            .extension
            .load_id("demo")
            .expect("surviving guest loaded");
        guest_ordinal(&mut app, "demo", 1);
        app.extension
            .request_package(PackageOperation::Remove("other".into()))
            .unwrap();
        settle_packages(&mut app);
        assert!(app.extension.load_id("other").is_none());
        assert_eq!(app.extension.load_id("demo"), Some(survivor));
        guest_ordinal(&mut app, "demo", 2);
    }

    fn request_guest_install(
        app: &mut crate::app::App,
        root: &std::path::Path,
        (package, plugin, bytes): (PackageManifest, PluginManifest, Vec<u8>),
        update: bool,
    ) {
        let package_manifest = root.join("package.json");
        let plugin_manifest = root.join("plugin.toml");
        let component = root.join("plugin.wasm");
        std::fs::write(&package_manifest, serde_json::to_vec(&package).unwrap()).unwrap();
        std::fs::write(&plugin_manifest, toml::to_string(&plugin).unwrap()).unwrap();
        std::fs::write(&component, bytes).unwrap();
        app.extension
            .request_package(crate::tools::PackageOperation::Install {
                package_manifest,
                plugin_manifest,
                component,
                roots: vec![crate::files::canonicalize(root).unwrap()],
                limits: limits(),
                update,
            })
            .unwrap();
    }

    #[test]
    fn package_update_rejects_a_changed_shortcut_argument_type() {
        use crate::plugins::{PluginArgumentDeclaration, PluginArgumentKind};

        let ((root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        let guest = |version: &str, kind: PluginArgumentKind| {
            let (mut package, mut plugin, bytes) = guest_package("demo");
            package.version = version.into();
            plugin.version = version.into();
            plugin.commands.get_mut("drive").unwrap().arguments.insert(
                "text".into(),
                PluginArgumentDeclaration {
                    kind,
                    required: true,
                },
            );
            (package, plugin, bytes)
        };
        commit_guest(&manager, guest("1.0.0", PluginArgumentKind::String));
        set_startup_config(
            &mut app,
            with_user_keymaps(
                root.path(),
                r#"
[[keymap]]
id = 'user.demo-drive'
context = 'dashboard'
keys = 'f10 a'
command = 'plugin.demo.drive'
arguments = { text = 'hello' }

[[keymap]]
id = 'user.open-health'
context = 'dashboard'
keys = 'f11 q'
command = 'health.open'
"#,
                &["demo"],
            ),
        );
        app.extension.start_packages();
        settle_packages(&mut app);
        let old = app.extension.load_id("demo").unwrap();
        guest_ordinal(&mut app, "demo", 1);
        let assert_shortcuts = |app: &crate::app::App| {
            let keys = app.configuration.keymaps();
            let binding = keys.inspect_id("user.demo-drive").unwrap();
            assert_eq!(binding.command.as_deref(), Some("plugin.demo.drive"));
            assert_eq!(
                binding.arguments.get("text"),
                Some(&crate::editor::CommandValue::String("hello".into()))
            );
            assert_eq!(
                keys.inspect_id("user.open-health")
                    .unwrap()
                    .command
                    .as_deref(),
                Some("health.open")
            );
        };
        assert_shortcuts(&app);

        request_guest_install(
            &mut app,
            root.path(),
            guest("2.0.0", PluginArgumentKind::Boolean),
            true,
        );
        settle_packages(&mut app);

        assert_eq!(manager.read_lock().unwrap().packages[0].version, "1.0.0");
        assert!(app.extension.is_active_load("demo", old));
        assert_shortcuts(&app);
        assert_eq!(
            app.ui.notification.as_deref(),
            Some(concat!(
                "Couldn't update \"demo\"; kept version 1.0.0. ",
                "Shortcut \"f10 a\" passes text to a command that now expects true/false for \"text\"."
            ))
        );
        guest_ordinal(&mut app, "demo", 2);
    }

    #[test]
    fn survivor_can_update_after_removal_leaves_dangling_config_references() {
        use crate::tools::PackageOperation;
        let ((root, state), mut app) = crate::app::tests::app_with_plugin_queue(8);
        settle_packages(&mut app);
        let manager = store(&state);
        for name in ["demo", "other"] {
            commit_guest(&manager, guest_with_panel(name));
        }
        let mut loaded = config_with_package_rules("plugin.other.panel");
        let mut expected = loaded.config.windows.rules.clone();
        expected.retain(|rule| rule.content_type != "plugin.other.panel");
        let mut aliases = loaded.config.input.command_aliases.clone();
        aliases.remove("otherdrive");
        set_startup_config(&mut app, loaded.clone());
        app.extension.start_packages();
        settle_packages(&mut app);
        let old = app.extension.load_id("demo").unwrap();

        app.extension
            .request_package(PackageOperation::Remove("other".into()))
            .unwrap();
        settle_packages(&mut app);
        assert_eq!(app.extension.load_id("demo"), Some(old));
        assert!(app.extension.load_id("other").is_none());
        assert_panel_rules(&app, &expected);
        let notice = app
            .ui
            .config_diagnostic
            .as_deref()
            .expect("removed references are reported");
        assert!(notice.contains("otherdrive") && notice.contains("plugin.other.panel"));
        loaded.installed_plugins.remove("other");
        reload_twice_with_unchanged_warnings(
            &mut app,
            loaded,
            &[
                "Command alias \"otherdrive\" refers to unknown command \"plugin.other.drive\".",
                "Window rule \"plugin.other.panel\" doesn't match any plugin panel.",
            ],
        );

        let (mut package, mut plugin, bytes) = guest_with_panel("demo");
        package.version = "2.0.0".into();
        plugin.version.clone_from(&package.version);
        request_guest_install(&mut app, root.path(), (package, plugin, bytes), true);
        settle_packages(&mut app);

        assert_eq!(manager.read_lock().unwrap().packages[0].version, "2.0.0");
        let replacement = app.extension.load_id("demo").unwrap();
        assert_ne!(replacement, old);
        assert!(app.extension.is_active_load("demo", replacement));
        assert_panel_rules(&app, &expected);
        assert_eq!(app.configuration.settings().input.command_aliases, aliases);
    }

    #[test]
    fn package_update_replaces_managed_plugin_and_commits_after_load() {
        use crate::plugins::PluginEvent;
        use crate::tools::PackageOperation;
        use crate::tools::PackageStore;
        use sha2::Digest;
        use sha2::Sha256;
        use std::fs;
        use std::thread;
        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        let registry = RegistryWorker::start(
            crate::tools::RegistryPolicy {
                offline: true,
                ..Default::default()
            },
            root.path().join("cache"),
        )
        .unwrap();
        let mut owner = Plugins::new(root.path().join("storage"), 8, packages.clone(), registry);
        let core = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/retained-guest/retained-guest.core.wasm"
        ));
        let bytes = wit_component::ComponentEncoder::default()
            .module(core)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let mut plugin = crate::app::plugins::fixture_manifest(digest.clone());
        let mut package = PackageManifest {
            name: "demo".into(),
            version: "1.0.0".into(),
            component: "plugin.wasm".into(),
            source: PackageSource::Registry {
                registry: "https://example.invalid".into(),
                version: "1.0.0".into(),
                archive_sha256: digest,
            },
            dependencies: Vec::new(),
        };
        fs::write(root.path().join("plugin.wasm"), bytes).unwrap();
        let mut old_load = None;
        for update in [false, true] {
            if update {
                package.version = "2.0.0".into();
                plugin.version = "2.0.0".into();
            }
            fs::write(
                root.path().join("package.json"),
                serde_json::to_vec(&package).unwrap(),
            )
            .unwrap();
            fs::write(
                root.path().join("plugin.toml"),
                toml::to_string(&plugin).unwrap(),
            )
            .unwrap();
            owner
                .request_package(PackageOperation::Install {
                    package_manifest: root.path().join("package.json"),
                    plugin_manifest: root.path().join("plugin.toml"),
                    component: root.path().join("plugin.wasm"),
                    roots: vec![root.path().into()],
                    limits: limits(),
                    update,
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let load = loop {
                for update in owner.poll_packages(limits(), &Default::default(), false) {
                    match update {
                        ExtensionUpdate::Conflict(error) | ExtensionUpdate::Error(error) => {
                            panic!("package preparation failed: {error}")
                        }
                        ExtensionUpdate::Failed { operation, error } => {
                            panic!("package preparation failed: {operation}: {error}")
                        }
                        _ => {}
                    }
                }
                let mut loaded = None;
                for event in owner.manager.poll().unwrap() {
                    match event {
                        PluginEvent::Indexed {
                            load, plugin: id, ..
                        } => {
                            owner
                                .manager
                                .activate_indexed(&id, load, plugin.component_sha256.clone())
                                .unwrap();
                        }
                        PluginEvent::Loaded { load, .. } => loaded = Some(load),
                        PluginEvent::Failed { error, .. } => panic!("plugin load: {error}"),
                        _ => {}
                    }
                }
                if let Some(load) = loaded {
                    break load;
                }
                assert!(
                    Instant::now() < deadline,
                    "replacement load timed out: update={update}, pending={}, statuses={:?}",
                    owner.pending_packages(),
                    owner.statuses(),
                );
                thread::sleep(Duration::from_millis(2));
            };
            assert_ne!(Some(load), old_load);
            assert_eq!(owner.manager.load_id("demo"), old_load);
            let manager =
                PackageStore::new(packages.clone(), 1024 * 1024, 64 * 1024 * 1024).unwrap();
            assert_eq!(
                manager
                    .read_lock()
                    .unwrap()
                    .packages
                    .first()
                    .map(|package| package.version.as_str()),
                if update { Some("1.0.0") } else { None }
            );
            owner.submit_package_commit("demo", load).unwrap();
            while owner.pending_packages() != 0 {
                for update in owner.poll_packages(limits(), &Default::default(), false) {
                    if let ExtensionUpdate::PublishPackage(plugin, staged, package) = update {
                        owner.manager.publish_staged(&plugin, staged).unwrap();
                        owner.package_candidates.remove(&plugin);
                        owner
                            .package_managed_plugins
                            .insert(plugin, (staged, package));
                    }
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(
                manager.read_lock().unwrap().packages[0].version,
                package.version
            );
            old_load = Some(load);
        }
        // Neither a new ID for this package nor another package claiming its ID may load.
        for (package_name, plugin_id) in [("demo", "renamed"), ("unrelated", "demo")] {
            package.name = package_name.into();
            plugin.id = plugin_id.into();
            fs::write(
                root.path().join("package.json"),
                serde_json::to_vec(&package).unwrap(),
            )
            .unwrap();
            fs::write(
                root.path().join("plugin.toml"),
                toml::to_string(&plugin).unwrap(),
            )
            .unwrap();
            owner
                .request_package(PackageOperation::Install {
                    package_manifest: root.path().join("package.json"),
                    plugin_manifest: root.path().join("plugin.toml"),
                    component: root.path().join("plugin.wasm"),
                    roots: vec![root.path().into()],
                    limits: limits(),
                    update: package_name == "demo",
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let updates = owner.poll_packages(limits(), &Default::default(), false);
                if let [
                    ExtensionUpdate::PackageProblem {
                        message,
                        diagnostic,
                    },
                ] = updates.as_slice()
                {
                    assert!(
                        diagnostic.contains("belongs to package demo"),
                        "{diagnostic}"
                    );
                    let expected = if package_name == "demo" {
                        "Couldn't update \"demo\"; kept version 2.0.0."
                    } else {
                        "Couldn't install package \"unrelated\"."
                    };
                    assert_eq!(message, expected);
                    break;
                }
                assert!(updates.is_empty(), "unexpected package update");
                assert!(Instant::now() < deadline, "ownership rejection timed out");
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(owner.manager.load_id("demo"), old_load);
            assert!(owner.manager.load_id("renamed").is_none());
            assert!(owner.package_candidates.is_empty());
            let manager =
                PackageStore::new(packages.clone(), 1024 * 1024, 64 * 1024 * 1024).unwrap();
            assert_eq!(manager.read_lock().unwrap().packages.len(), 1);
            assert_eq!(manager.read_lock().unwrap().packages[0].name, "demo");
        }
        // Package maintenance is allowed in safe mode, but must not activate installed code.
        for operation in [
            PackageOperation::Rollback,
            PackageOperation::Remove("demo".into()),
        ] {
            owner.request_package(operation).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while owner.pending_packages() != 0 {
                for update in owner.poll_packages(limits(), &Default::default(), true) {
                    assert!(!matches!(
                        update,
                        ExtensionUpdate::Error(_) | ExtensionUpdate::Failed { .. }
                    ));
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            }
            assert!(owner.manager.load_id("demo").is_none());
            assert!(owner.package_startup_request.is_none());
        }
    }

    #[test]
    fn registry_presentations_keep_the_requesting_picker_epoch() {
        use crate::app::picker::PickerContent;
        use crate::app::picker::PickerOrigin;
        use crate::app::search::Search;
        use crate::app::workspace::Workspace;
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        let identity = crate::files::RootIdentity::capture(root.path()).unwrap();
        let workspace = Workspace::new(
            crate::workspace::WorkspaceState::new(
                crate::workspace::WorkspaceId(1),
                [("root".into(), identity)],
            )
            .unwrap(),
            root.path().join("trust"),
            1,
            false,
            false,
            1,
        );
        let mut search = Search::default();
        let content = |title: &str| PickerContent {
            surface: crate::ui::PickerSurfaceKind::Generic,
            origin: PickerOrigin::General,
            title: title.into(),
            items: Vec::new(),
            query: String::new(),
            complete: true,
        };
        let epoch = search
            .install_picker(content("registry"), 100, workspace.version(), 1)
            .unwrap();
        owner.registry_intents.insert(1, (false, Some(epoch)));
        let Some(RegistryUpdate::Present(_, Some(epoch))) = owner.accept_registry(RegistryEvent {
            id: 1,
            result: Ok(RegistryResult::Search(
                crate::tools::RegistrySearchResponse {
                    schema: 1,
                    packages: Vec::new(),
                },
            )),
        }) else {
            panic!("search must preserve its picker owner")
        };
        assert!(search.owns_picker_epoch(&epoch));
        search
            .install_picker(content("files"), 100, workspace.version(), 1)
            .unwrap();
        search.picker_insert("my query");
        assert!(
            !search.owns_picker_epoch(&epoch),
            "a late registry result cannot replace the file picker"
        );
        assert_eq!(search.picker().unwrap().query(), "my query");
    }

    #[test]
    fn package_activation_respects_safe_mode_and_disabled_plugins() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        let (manager, _fixture) = crate::plugins::ExecutionFixture::new(4);
        owner.fixture_manager(manager);
        let plugin = PluginManifest {
            id: "other".into(),
            version: "1.0.0".into(),
            component_sha256: "0".repeat(64),
            capabilities: Default::default(),
            commands: Default::default(),
            contributions: Default::default(),
        };
        let lock =
            std::sync::Arc::new(std::fs::File::create(root.path().join("object-lock")).unwrap());
        let activation = || {
            vec![PackageDiscovery {
                package: locked("other"),
                plugin: Some(plugin.clone()),
                object: Ok((root.path().join("other.wasm"), lock.clone())),
            }]
        };
        assert!(
            owner
                .load_activations(activation(), &limits(), &Default::default(), true)
                .is_empty()
        );
        assert!(owner.load_id("other").is_none());
        let mut config = crate::config::PluginsConfig::default();
        config.entries.insert(
            "other".into(),
            crate::config::PluginEntryConfig {
                manifest: "other.toml".into(),
                component: "other.wasm".into(),
                enabled: false,
            },
        );
        assert!(
            owner
                .load_activations(activation(), &limits(), &config, false)
                .is_empty()
        );
        assert!(owner.load_id("other").is_none());
        owner.disable("other").unwrap();
        assert!(
            owner
                .load_activations(activation(), &limits(), &Default::default(), false)
                .is_empty()
        );
        assert!(owner.load_id("other").is_none());
        assert!(matches!(
            owner.statuses().get("other"),
            Some(crate::plugins::PluginStatus::Disabled)
        ));
        // A normal enabled activation still starts loading.
        owner.disabled_plugins.remove("other");
        if let Some(update) = owner
            .load_activations(activation(), &limits(), &Default::default(), false)
            .into_iter()
            .next()
        {
            match update {
                ExtensionUpdate::Error(error) => panic!("enabled activation failed: {error}"),
                _ => panic!("unexpected activation update"),
            }
        }
        assert!(owner.package_candidates.contains_key("other"));
        // Disabling while activation is pending releases the package change blocker.
        owner.disable("other").unwrap();
        assert!(!owner.package_candidates.contains_key("other"));
        owner
            .request_package(crate::tools::PackageOperation::Remove("other".into()))
            .unwrap();
    }

    #[test]
    fn package_activation_conflicts_do_not_request_persistent_rollback() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        let (manager, _fixture) = crate::plugins::ExecutionFixture::new(4);
        owner.fixture_manager(manager);
        let manifest = crate::app::plugins::fixture_manifest("0".repeat(64));
        let updates = owner.load_activations(
            vec![PackageDiscovery {
                package: locked("demo"),
                plugin: Some(manifest),
                object: Ok((
                    root.path().join("demo.wasm"),
                    std::sync::Arc::new(
                        std::fs::File::create(root.path().join("object-lock")).unwrap(),
                    ),
                )),
            }],
            &limits(),
            &Default::default(),
            false,
        );
        assert!(
            matches!(updates.as_slice(), [ExtensionUpdate::Conflict(plugin)] if plugin == "demo")
        );
        assert_eq!(owner.pending_packages(), 0);
        assert_eq!(owner.load_id("demo"), Some(PluginLoadId(1)));
    }

    #[test]
    fn package_changes_wait_for_prepare_load_and_commit() {
        use crate::tools::PackageError;
        use crate::tools::PackageOperation;
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        for stage in 0..3 {
            match stage {
                0 => {
                    owner.package_preparing.insert(1);
                }
                1 => {
                    owner
                        .package_candidates
                        .insert("demo".into(), prepared(1, 1));
                }
                _ => {
                    owner
                        .package_commits
                        .insert(1, ("demo".into(), PluginLoadId(1)));
                }
            }
            for operation in [
                PackageOperation::Remove("demo".into()),
                PackageOperation::Rollback,
            ] {
                assert!(matches!(
                    owner.request_package(operation),
                    Err(PackageError::Busy)
                ));
            }
            assert_eq!(owner.packages.pending_count(), 0);
            owner.package_preparing.clear();
            owner.package_candidates.clear();
            owner.package_commits.clear();
        }
        owner
            .request_package(PackageOperation::Remove("demo".into()))
            .unwrap();
        // A new install cannot start while removal is waiting for the worker either.
        assert!(matches!(
            owner.request_package(PackageOperation::Install {
                package_manifest: root.path().join("package.json"),
                plugin_manifest: root.path().join("plugin.toml"),
                component: root.path().join("plugin.wasm"),
                roots: vec![root.path().into()],
                limits: limits(),
                update: true,
            }),
            Err(PackageError::Busy)
        ));
    }

    #[test]
    fn package_abort_retries_after_queue_saturation() {
        use crate::tools::PackageError;
        use crate::tools::PackageOperation;
        use crate::tools::PackageWorker;
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        owner.packages = PackageWorker::start(root.path().join("packages"), 1, 1024 * 1024, 1024);
        let deadline = Instant::now() + Duration::from_secs(30);
        // One result fills the event queue, the next blocks its worker, and a third fills requests.
        while owner.packages.pending_count() < 3 {
            match owner.packages.request(PackageOperation::List) {
                Ok(_) | Err(PackageError::QueueFull) => {}
                Err(error) => panic!("{error}"),
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(matches!(
            owner.packages.request(PackageOperation::List),
            Err(PackageError::QueueFull)
        ));
        let (manager, _fixture) = crate::plugins::ExecutionFixture::new(4);
        owner.fixture_manager(manager);
        owner
            .package_candidates
            .insert("demo".into(), prepared(1, 41));
        owner.disable("demo").unwrap();
        assert!(owner.package_candidates.is_empty());
        assert!(owner.package_aborts.contains(&41));
        while owner.pending_packages() != 0 {
            owner.poll_packages(limits(), &Default::default(), false);
            assert!(Instant::now() < deadline, "abort was not retried");
            std::thread::yield_now();
        }
        assert!(owner.package_aborts.is_empty());
    }

    #[test]
    fn cancelled_registry_and_replacement_package_requests_are_not_adopted() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        let metadata = RegistryPackageMetadata {
            schema: 1,
            publisher: "owned".into(),
            name: "demo".into(),
            version: "1.0.0".into(),
            yanked: false,
            artifact_path: "demo.wasm".into(),
            artifact_sha256: "0".repeat(64),
            package: PackageManifest {
                name: "demo".into(),
                version: "1.0.0".into(),
                component: "demo.wasm".into(),
                source: PackageSource::Path {
                    path: root.path().into(),
                },
                dependencies: Vec::new(),
            },
            plugin: PluginManifest {
                id: "demo".into(),
                version: "1.0.0".into(),
                component_sha256: "0".repeat(64),
                capabilities: Default::default(),
                commands: Default::default(),
                contributions: Default::default(),
            },
        };
        let event = |id, bytes| RegistryEvent {
            id,
            result: Ok(RegistryResult::Artifact {
                metadata: metadata.clone(),
                bytes,
            }),
        };
        // Synthetic registry responses; no network or installation is performed.
        assert!(owner.accept_registry(event(1, vec![1])).is_none());
        owner.registry_intents.insert(1, (false, None));
        assert!(matches!(
            owner.accept_registry(event(1, vec![1])),
            Some(RegistryUpdate::Error(_))
        ));
        assert!(owner.pending_registry_artifact.is_none());
        owner.registry_intents.insert(2, (true, None));
        assert!(matches!(
            owner.accept_registry(event(2, vec![2])),
            Some(RegistryUpdate::ValidateArtifact)
        ));
        assert!(owner.accept_registry(event(2, vec![3])).is_none());
        owner.registry_intents.insert(3, (true, None));
        assert!(matches!(
            owner.accept_registry(event(3, vec![3])),
            Some(RegistryUpdate::Error(_))
        ));
        assert_eq!(owner.pending_registry_artifact.as_ref().unwrap().1, vec![2]);
        owner.start_packages();
        assert_eq!(
            owner.prepare_artifact(limits()).unwrap_err(),
            "Couldn't install package \"demo\"; another package operation is still in progress.",
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while owner.pending_packages() != 0 {
            let _ = owner.poll_packages(limits(), &Default::default(), false);
            assert!(
                Instant::now() < deadline,
                "startup package discovery did not finish"
            );
            std::thread::yield_now();
        }
        owner.registry_intents.insert(4, (true, None));
        owner.cancel_pending_packages();
        assert!(owner.accept_registry(event(4, vec![4])).is_none());
        assert!(owner.pending_registry_artifact.is_none());
        owner
            .package_candidates
            .insert("demo".into(), prepared(9, 41));
        assert!(
            owner
                .failed_package_load("demo", PluginLoadId(8), "late response")
                .is_none()
        );
        assert_eq!(owner.package_candidates["demo"].token, Some(41));
        owner.package_candidates.clear();
        // Actual package worker/VFS failure: no previous lock exists in this owned root.
        owner
            .request_package(crate::tools::PackageOperation::Rollback)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if owner.poll_packages(limits(), &Default::default(), false).iter().any(|update| matches!(update, ExtensionUpdate::Failed { operation, .. } if operation == "rollback")) { break; }
            assert!(Instant::now() < deadline, "rollback terminal missing");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(owner.pending_packages(), 0);

        // A missing manifest fails on the package worker, but a cancelled
        // preparation no longer owns that failure, even if the reply is already queued.
        owner
            .request_package(crate::tools::PackageOperation::Install {
                package_manifest: root.path().join("missing.json"),
                plugin_manifest: root.path().join("missing.toml"),
                component: root.path().join("missing.wasm"),
                roots: vec![root.path().into()],
                limits: limits(),
                update: true,
            })
            .unwrap();
        owner.cancel_pending_packages();
        let deadline = Instant::now() + Duration::from_secs(30);
        while owner.pending_packages() != 0 {
            assert!(
                owner
                    .poll_packages(limits(), &Default::default(), false)
                    .is_empty()
            );
            assert!(
                Instant::now() < deadline,
                "cancelled preparation did not finish"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn deferred_activation_failure_keeps_unsubmitted_indexes() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        let (mut manager, _fixture) = crate::plugins::ExecutionFixture::new(1);
        manager
            .execute(crate::plugins::empty_execution_request())
            .unwrap();
        manager
            .statuses
            .insert("demo".into(), crate::plugins::PluginStatus::Indexed);
        owner.fixture_manager(manager);
        owner.queue_index("demo".into(), PluginLoadId(1), "a".into());
        owner.queue_index("zeta".into(), PluginLoadId(2), "b".into());
        assert!(
            owner
                .activate_pending(true, "demo", PluginLoadId(1))
                .is_err()
        );
        assert!(!owner.index_pending("demo"));
        assert!(owner.index_pending("zeta"));
    }

    #[test]
    fn prepared_updates_replace_only_managed_loads_and_ignore_cancelled_requests() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            RegistryWorker::start(Default::default(), root.path().join("cache")).unwrap();
        let mut owner = Plugins::new(
            root.path().join("storage"),
            4,
            root.path().join("packages"),
            registry,
        );
        let (manager, _fixture) = crate::plugins::ExecutionFixture::new(4);
        owner.fixture_manager(manager);
        let prepared = crate::tools::PreparedPackage {
            token: 41,
            manifest: PackageManifest {
                name: "demo".into(),
                version: "2.0.0".into(),
                component: "demo.wasm".into(),
                source: PackageSource::Path {
                    path: root.path().into(),
                },
                dependencies: Vec::new(),
            },
            plugin: PluginManifest {
                id: "demo".into(),
                version: "2.0.0".into(),
                component_sha256: "0".repeat(64),
                capabilities: Default::default(),
                commands: Default::default(),
                contributions: Default::default(),
            },
            object: root.path().join("demo.wasm"),
        };
        owner.package_preparing.insert(41);
        let updates = owner.load_prepared(41, prepared.clone(), PackageChange::Update, &limits());
        assert!(matches!(
            updates.as_slice(),
            [ExtensionUpdate::PackageProblem { message, .. }] if message == "Couldn't update \"demo\"."
        ));
        assert_eq!(owner.manager.load_id("demo"), Some(PluginLoadId(1)));
        owner.package_preparing.insert(42);
        owner.cancel_pending_packages();
        assert!(
            owner
                .load_prepared(42, prepared.clone(), PackageChange::Update, &limits())
                .is_empty()
        );
        assert_eq!(owner.manager.load_id("demo"), Some(PluginLoadId(1)));
        owner
            .package_managed_plugins
            .insert("demo".into(), (PluginLoadId(1), locked("demo")));
        owner.package_preparing.insert(43);
        let updates = owner.load_prepared(43, prepared, PackageChange::Update, &limits());
        assert!(
            updates
                .iter()
                .all(|update| !matches!(update, ExtensionUpdate::Remove(_, _)))
        );
        assert_eq!(owner.load_id("demo"), Some(PluginLoadId(1)));
        assert!(updates.is_empty());
        assert_eq!(owner.package_candidates["demo"].load, PluginLoadId(2));
    }
}
