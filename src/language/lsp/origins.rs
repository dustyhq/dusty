//! Private event origins and consuming replies bound to the producing session.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::editor::text::{PositionEncoding, TextVersion};

#[cfg(test)]
use crate::language::lsp::messages::{LspDiagnostic, LspWorkspaceEdit};
use crate::language::lsp::messages::{LspEvent, ServerRequestId};
use crate::language::lsp::session::SessionCommand;

pub(super) type DocumentSource = (crate::editor::BufferId, TextVersion);
pub(crate) type WorkspaceFileHash = [u8; 32];

#[derive(Clone, Copy)]
pub(crate) struct PreparedDiskFile {
    pub(crate) hash: WorkspaceFileHash,
    pub(crate) modified: Option<SystemTime>,
}
impl PreparedDiskFile {
    pub(crate) fn from_bytes(path: &Path, bytes: &[u8]) -> Self {
        Self {
            hash: workspace_file_hash(bytes),
            modified: file_modified(path),
        }
    }
}

fn file_modified(path: &Path) -> Option<SystemTime> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata.is_file().then_some(())?;
    metadata.modified().ok()
}

fn predates_dispatch(modified: Option<SystemTime>, sent_at: Option<SystemTime>) -> bool {
    // Coarse filesystems can report a write near dispatch in the same 2-second tick.
    modified.zip(sent_at).is_some_and(|(modified, sent_at)| {
        modified
            .checked_add(Duration::from_secs(2))
            .is_some_and(|safe| safe < sent_at)
    })
}

#[derive(Clone, Debug)]
pub(crate) struct EditContext {
    pub(super) session: Arc<()>,
    pub(super) server: Arc<str>,
    pub(super) encoding: PositionEncoding,
    pub(super) diagnostic_ambiguous: bool,
    pub(super) sources: Arc<HashMap<String, DocumentSource>>,
    pub(super) wire_versions: Option<Arc<HashMap<String, Option<u64>>>>,
    pub(super) disk: Option<(String, crate::editor::text::TextBuffer)>,
    pub(super) workspace: Option<Arc<WorkspaceEditBaseline>>,
    pub(super) workspace_sent_at: Option<SystemTime>,
    pub(super) command_sources: Arc<[CommandEditSource]>,
    pub(super) request_document: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct CommandEditSource {
    pub(super) encoding: PositionEncoding,
    pub(super) sources: Arc<HashMap<String, DocumentSource>>,
    pub(super) wire_versions: Arc<HashMap<String, Option<u64>>>,
    pub(super) workspace: Arc<WorkspaceEditBaseline>,
    pub(super) sent_at: SystemTime,
}

/// A bounded content inventory captured before dispatch. Uncaptured files may
/// use an older mtime; resource destinations are checked directly at approval.
#[derive(Debug)]
pub(super) struct WorkspaceEditBaseline {
    roots: Arc<[PathBuf]>,
    captured: bool,
    files: HashMap<PathBuf, WorkspaceFileHash>,
    entries: HashSet<PathBuf>,
    aliases: HashMap<PathBuf, PathBuf>,
}

fn workspace_file_hash(bytes: &[u8]) -> WorkspaceFileHash {
    Sha256::digest(bytes).into()
}

impl WorkspaceEditBaseline {
    const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
    const MAX_ENTRIES: usize = 32_768;
    const MAX_CAPTURE_TIME: Duration = Duration::from_secs(3);

    pub(super) fn new(roots: Arc<[PathBuf]>) -> Self {
        Self {
            roots,
            captured: false,
            files: HashMap::new(),
            entries: HashSet::new(),
            aliases: HashMap::new(),
        }
    }
    pub(super) fn capture(roots: Arc<[PathBuf]>, cancelled: &AtomicBool) -> Self {
        Self::capture_with_entry_limit(roots, cancelled, Self::MAX_ENTRIES)
    }

    fn capture_with_entry_limit(
        roots: Arc<[PathBuf]>,
        cancelled: &AtomicBool,
        entry_limit: usize,
    ) -> Self {
        let mut roots = roots.to_vec();
        roots.sort_unstable();
        roots.dedup();
        let mut snapshot = Self {
            captured: true,
            ..Self::new(roots.into())
        };
        let Some(mut walk) = crate::files::workspace_search_walk(&snapshot.roots, false, false)
        else {
            return snapshot;
        };
        walk.filter_entry(|entry| entry.file_name() != ".git");
        let mut remaining = Self::MAX_TOTAL_BYTES;
        let deadline = Instant::now() + Self::MAX_CAPTURE_TIME;
        for entry in walk.build().take(entry_limit) {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                break;
            }
            let Ok(entry) = entry else { continue };
            let path = entry.into_path();
            if !snapshot.entries.insert(path.clone()) {
                continue;
            }
            let Some(canonical) = snapshot.scoped(&path) else {
                continue;
            };
            if canonical != path {
                snapshot.aliases.insert(path, canonical);
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_file() && remaining != 0 {
                let limit = remaining.min(Self::MAX_FILE_BYTES);
                if metadata.len() <= limit
                    && let Ok(bytes) = crate::files::read_scoped_file(&path, &snapshot.roots, limit)
                {
                    remaining -= bytes.len() as u64;
                    snapshot.files.insert(path, workspace_file_hash(&bytes));
                }
            }
        }
        snapshot
    }

    fn contains(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    fn scoped(&self, path: &Path) -> Option<PathBuf> {
        let canonical = crate::files::canonicalize(path).ok()?;
        self.contains(&canonical).then_some(canonical)
    }

    fn disk_hash_matches(&self, path: &Path, hash: &WorkspaceFileHash) -> bool {
        crate::files::read_scoped_file(path, &self.roots, Self::MAX_FILE_BYTES)
            .is_ok_and(|bytes| workspace_file_hash(&bytes) == *hash)
    }

    fn dispatch_path(&self, path: &Path) -> PathBuf {
        for prefix in path.ancestors() {
            if let Some(target) = self.aliases.get(prefix) {
                let tail = path.strip_prefix(prefix).expect("ancestor");
                return if tail.as_os_str().is_empty() {
                    target.clone()
                } else {
                    target.join(tail)
                };
            }
        }
        path.to_path_buf()
    }

    fn same_mapping(&self, requested: &Path, canonical: &Path) -> bool {
        let before = self.dispatch_path(requested);
        if before == canonical {
            return true;
        }
        // A Windows spelling alias is accepted only after scoped VFS resolution.
        // An exact entry at dispatch must never be mistaken for another entry.
        #[cfg(windows)]
        {
            !self.entries.contains(&before) && windows_case_alias(&before, canonical)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[cfg(windows)]
fn windows_case_alias(left: &Path, right: &Path) -> bool {
    // This is only a spelling test, never a substitute for filesystem resolution.
    left.to_str()
        .zip(right.to_str())
        .is_some_and(|(left, right)| {
            left.replace('\\', "/").to_uppercase() == right.replace('\\', "/").to_uppercase()
        })
}

impl EditContext {
    pub(crate) fn request_document(&self) -> Option<&str> {
        self.request_document.as_deref()
    }
    pub(crate) fn requires_workspace_baseline(&self) -> bool {
        self.workspace.is_some()
    }
    pub(crate) fn has_workspace_dispatch(&self) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|baseline| baseline.captured)
    }
    pub(crate) fn command_contexts(&self) -> impl Iterator<Item = Self> + '_ {
        self.command_sources.iter().map(|source| Self {
            encoding: source.encoding,
            sources: source.sources.clone(),
            wire_versions: Some(source.wire_versions.clone()),
            workspace: Some(source.workspace.clone()),
            workspace_sent_at: Some(source.sent_at),
            command_sources: Arc::default(),
            ..self.clone()
        })
    }
    pub(crate) fn needs_workspace_verification(&self) -> bool {
        self.has_workspace_dispatch() || !self.command_sources.is_empty()
    }
    pub(crate) fn needs_disk_read(&self, uri: &str) -> bool {
        self.document_source(uri).is_none()
            && self
                .command_sources
                .iter()
                .all(|source| source_at(&source.sources, uri).is_none())
    }
    pub(crate) fn maps_to(&self, requested: &Path, canonical: &Path) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|base| base.contains(canonical) && base.same_mapping(requested, canonical))
    }
    pub(crate) fn matches_prepared_disk(
        &self,
        canonical: &Path,
        prepared: &PreparedDiskFile,
    ) -> bool {
        let Some(base) = &self.workspace else {
            return false;
        };
        match base.files.get(canonical) {
            Some(hash) => *hash == prepared.hash,
            None => predates_dispatch(prepared.modified, self.workspace_sent_at),
        }
    }
    pub(crate) fn disk_file_current(
        &self,
        requested: &Path,
        canonical: &Path,
        hash: Option<&WorkspaceFileHash>,
    ) -> bool {
        let Some(base) = &self.workspace else {
            return false;
        };
        base.scoped(requested).as_deref() == Some(canonical)
            && hash.is_none_or(|hash| base.disk_hash_matches(canonical, hash))
    }

    pub(crate) fn destination_in_scope(&self, canonical: &Path) -> bool {
        // Preparation has already resolved the parent in the live workspace. An
        // absent name has no text or alias of its own to prove from the scan.
        self.workspace
            .as_ref()
            .is_some_and(|base| base.contains(canonical))
    }

    pub(crate) fn destination_current(&self, requested: &Path, canonical: &Path) -> bool {
        let Some(base) = &self.workspace else {
            return false;
        };
        let Some(parent) = requested.parent().and_then(|parent| base.scoped(parent)) else {
            return false;
        };
        if Some(parent.as_path()) != canonical.parent()
            || requested.file_name() != canonical.file_name()
        {
            return false;
        }
        match std::fs::symlink_metadata(canonical) {
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            Ok(metadata) if metadata.is_file() => {
                let Some(existing) = base.scoped(canonical) else {
                    return false;
                };
                match base.files.get(&existing) {
                    Some(hash) => base.disk_hash_matches(&existing, hash),
                    None => predates_dispatch(metadata.modified().ok(), self.workspace_sent_at),
                }
            }
            Ok(_) => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn workspace_capture_entries(&self) -> &HashSet<PathBuf> {
        &self.workspace.as_ref().expect("workspace capture").entries
    }

    #[cfg(test)]
    pub(crate) fn with_workspace_baseline(self, root: &Path) -> Self {
        self.with_workspace_capture_entry_limit(root, WorkspaceEditBaseline::MAX_ENTRIES)
    }

    #[cfg(test)]
    pub(crate) fn with_workspace_capture_entry_limit(
        mut self,
        root: &Path,
        entry_limit: usize,
    ) -> Self {
        let root = crate::files::canonicalize(root).unwrap();
        self.workspace = Some(Arc::new(WorkspaceEditBaseline::capture_with_entry_limit(
            Arc::from([root]),
            &AtomicBool::new(false),
            entry_limit,
        )));
        self.workspace_sent_at = Some(SystemTime::now());
        self
    }
    pub(crate) fn diagnostic_producer(
        &self,
        prefix: &str,
    ) -> Result<crate::workspace::DiagnosticProducer, crate::workspace::WorkspaceError> {
        Ok(crate::workspace::DiagnosticProducer::from_runtime(
            crate::workspace::DiagnosticOwner::new(format!("{prefix}{}", self.server))?,
            self.session.clone(),
        ))
    }
    pub(crate) fn diagnostic_ambiguous(&self) -> bool {
        self.diagnostic_ambiguous
    }
    pub(crate) fn disk_document(&self, uri: &str) -> Option<&crate::editor::text::TextBuffer> {
        self.disk
            .as_ref()
            .filter(|(path, _)| path == uri)
            .map(|(_, text)| text)
    }
    pub(crate) fn encoding(&self) -> PositionEncoding {
        self.encoding
    }
    pub(crate) fn document_source(&self, uri: &str) -> Option<&DocumentSource> {
        source_at(&self.sources, uri)
    }
    pub(crate) fn knows_wire_document(&self, uri: &str) -> bool {
        self.wire_versions
            .as_ref()
            .is_some_and(|versions| source_at(versions, uri).is_some())
    }
    pub(crate) fn version_matches(&self, uri: &str, version: u64, editor_version: u64) -> bool {
        self.wire_versions
            .as_ref()
            .map_or(version == editor_version, |versions| {
                source_at(versions, uri).copied().flatten() == Some(version)
            })
    }
}

fn source_at<'a, T>(sources: &'a HashMap<String, T>, uri: &str) -> Option<&'a T> {
    let source = sources.get(uri);
    #[cfg(unix)]
    {
        source
    }
    #[cfg(windows)]
    {
        source.or_else(|| {
            let path = super::paths::path_from_file_uri(uri)?;
            sources.iter().find_map(|(address, source)| {
                super::paths::path_from_file_uri(address)
                    .is_some_and(|other| super::paths::same_file_path(&path, &other))
                    .then_some(source)
            })
        })
    }
}
pub(crate) struct OwnedLspEvent {
    pub event: LspEvent,
    pub context: EditContext,
    pub reply: Option<ApplyEditReply>,
}
#[derive(Clone)]
pub(super) struct EventSender {
    pub channel: SyncSender<OwnedLspEvent>,
    pub context: EditContext,
    pub commands: SyncSender<SessionCommand>,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    pub stop_requested: Arc<std::sync::atomic::AtomicBool>,
}
impl EventSender {
    pub fn send(&self, event: LspEvent) {
        let expendable = matches!(event, LspEvent::Log { .. });
        let mut event = OwnedLspEvent {
            event,
            context: self.context.clone(),
            reply: None,
        };
        loop {
            if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            match self.channel.try_send(event) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(_)) if expendable => return,
                Err(TrySendError::Full(value)) => {
                    event = value;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
    pub fn apply(
        &self,
        event: LspEvent,
        request_id: ServerRequestId,
        key: Arc<()>,
        encoding: PositionEncoding,
        sources: HashMap<String, (crate::editor::BufferId, TextVersion)>,
    ) -> bool {
        let context = EditContext {
            encoding,
            sources: Arc::new(sources),
            ..self.context.clone()
        };
        let reply = ApplyEditReply {
            sender: self.commands.clone(),
            request_id,
            key,
            context: context.clone(),
        };
        self.channel
            .try_send(OwnedLspEvent {
                event,
                context,
                reply: Some(reply),
            })
            .is_ok()
    }
}
pub(crate) struct ApplyEditReply {
    sender: SyncSender<SessionCommand>,
    request_id: ServerRequestId,
    key: Arc<()>,
    context: EditContext,
}
impl ApplyEditReply {
    pub(crate) fn context(&self) -> &EditContext {
        &self.context
    }
    pub(crate) fn finish(self, applied: bool, reason: Option<String>) -> PendingEditReply {
        let (completed, completion) = mpsc::sync_channel(1);
        PendingEditReply {
            completion,
            sender: self.sender,
            server: self.context.server,
            command: Some(SessionCommand::RespondOwnedApplyEdit {
                request_id: self.request_id,
                key: self.key,
                applied,
                reason,
                completed,
            }),
        }
    }
}
pub(crate) struct PendingEditReply {
    sender: SyncSender<SessionCommand>,
    server: Arc<str>,
    command: Option<SessionCommand>,
    completion: Receiver<Result<(), String>>,
}
impl PendingEditReply {
    pub(crate) fn try_send(&mut self) -> Result<bool, String> {
        let Some(command) = self.command.take() else {
            return match self.completion.try_recv() {
                Ok(result) => result.map(|()| true),
                Err(TryRecvError::Empty) => Ok(false),
                Err(TryRecvError::Disconnected) => Err(format!(
                    "requesting language server {} stopped before acknowledging its edit response",
                    self.server
                )),
            };
        };
        match self.sender.try_send(command) {
            Ok(()) => Ok(false),
            Err(TrySendError::Full(command)) => {
                self.command = Some(command);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Err(format!(
                "requesting language server {} stopped before its edit response could be delivered",
                self.server
            )),
        }
    }
}

#[cfg(test)]
pub(crate) struct EditSourceFixture {
    pub(super) events: EventSender,
    pub(super) commands: Receiver<SessionCommand>,
    pub(super) pending: HashMap<ServerRequestId, Arc<()>>,
}
#[cfg(test)]
impl EditSourceFixture {
    pub(crate) fn context(&self) -> EditContext {
        self.events.context.clone()
    }
    pub(crate) fn publish_result(
        &self,
        request_id: u64,
        method: &str,
        document: &crate::language::lsp::messages::LspDocument,
        result: crate::language::lsp::messages::LspFeatureResult,
    ) {
        self.events.send(LspEvent::Result {
            server: self.events.context.server.to_string(),
            request_id,
            method: method.into(),
            uri: document.uri(),
            version: document.version,
            result,
        });
    }
    pub(crate) fn synced_document(&self) -> Option<crate::language::lsp::messages::LspDocument> {
        match self.commands.try_recv().ok()? {
            SessionCommand::SyncOwned { document, .. } => Some(document),
            _ => panic!("expected owned document sync"),
        }
    }
    pub(crate) fn closed_uri(&self) -> Option<String> {
        while let Ok(command) = self.commands.try_recv() {
            if let SessionCommand::Close(uri) = command {
                return Some(uri);
            }
        }
        None
    }
    pub(crate) fn saved_uri(&self) -> Option<String> {
        self.saved_document().map(|document| document.uri())
    }
    pub(crate) fn saved_document(&self) -> Option<crate::language::lsp::messages::LspDocument> {
        while let Ok(command) = self.commands.try_recv() {
            if let SessionCommand::Save(document) = command {
                return Some(document);
            }
        }
        None
    }
    pub(crate) fn configured(
        &self,
    ) -> Option<crate::language::lsp::server::LspServerConfiguration> {
        match self.commands.try_recv().ok()? {
            SessionCommand::Configure(configuration) => Some(configuration),
            _ => None,
        }
    }
    pub(crate) fn requested_feature(&self) -> Option<crate::language::lsp::messages::LspFeature> {
        while let Ok(command) = self.commands.try_recv() {
            if let SessionCommand::Request { feature, .. } = command {
                return Some(feature);
            }
        }
        None
    }
    /// Consume queued document syncs before taking a request, as the worker does.
    /// Replies from this fixture then carry the sources the server saw at dispatch.
    pub(crate) fn requested_feature_after_sync(
        &mut self,
    ) -> Option<crate::language::lsp::messages::LspFeature> {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                SessionCommand::SyncOwned { document, source } => {
                    Arc::make_mut(&mut self.events.context.sources).insert(document.uri(), source);
                }
                SessionCommand::Request {
                    feature, document, ..
                } => {
                    self.events.context.request_document =
                        feature.carries_document_edits().then(|| document.uri());
                    return Some(feature);
                }
                _ => {}
            }
        }
        None
    }
    pub(crate) fn executed_command(&self) -> Option<String> {
        while let Ok(command) = self.commands.try_recv() {
            if let SessionCommand::Request {
                feature: crate::language::lsp::messages::LspFeature::ExecuteCommand(command),
                ..
            } = command
            {
                return Some(command.command);
            }
        }
        None
    }
    pub(crate) fn request(&mut self, id: ServerRequestId, edit: LspWorkspaceEdit) -> bool {
        if self.pending.contains_key(&id) {
            return false;
        }
        let key = Arc::new(());
        if !self.events.apply(
            LspEvent::ApplyEditRequest {
                server: self.events.context.server.to_string(),
                request_id: id.clone(),
                label: None,
                edit,
            },
            id.clone(),
            key.clone(),
            self.events.context.encoding,
            (*self.events.context.sources).clone(),
        ) {
            return false;
        }
        self.pending.insert(id, key);
        true
    }
    pub(crate) fn publish_diagnostics(
        &self,
        uri: &str,
        version: Option<u64>,
        diagnostics: Vec<LspDiagnostic>,
        lineage: &DiagnosticLineage,
    ) {
        let events = EventSender {
            context: EditContext {
                diagnostic_ambiguous: lineage.ambiguous(uri),
                ..self.events.context.clone()
            },
            ..self.events.clone()
        };
        events.send(LspEvent::Diagnostics {
            server: self.events.context.server.to_string(),
            uri: uri.into(),
            version,
            diagnostics,
        });
    }
    pub(crate) fn observe_diagnostic_document(
        &mut self,
        uri: &str,
        source: (crate::editor::BufferId, TextVersion),
        lineage: &mut DiagnosticLineage,
    ) {
        lineage.observe(uri.into(), &source);
        Arc::make_mut(&mut self.events.context.sources).insert(uri.into(), source);
    }
    pub(crate) fn take_synced_source(&self) -> Option<crate::editor::BufferId> {
        match self.commands.try_recv().ok()? {
            SessionCommand::SyncOwned { source, .. } => Some(source.0),
            _ => panic!("expected owned document sync"),
        }
    }
    pub(crate) fn publish_disk_diagnostics(
        &self,
        uri: &str,
        text: &str,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        let events = EventSender {
            context: EditContext {
                sources: Arc::new(HashMap::new()),
                disk: Some((uri.into(), crate::editor::text::TextBuffer::from_text(text))),
                workspace: None,
                request_document: None,
                ..self.events.context.clone()
            },
            ..self.events.clone()
        };
        events.send(LspEvent::Diagnostics {
            server: self.events.context.server.to_string(),
            uri: uri.into(),
            version: None,
            diagnostics,
        });
    }
    pub(crate) fn block_responses(&self) {
        self.events
            .commands
            .try_send(SessionCommand::Health)
            .unwrap();
    }
    pub(crate) fn unblock_responses(&self) {
        assert!(matches!(
            self.commands.try_recv(),
            Ok(SessionCommand::Health)
        ));
    }
    pub(crate) fn response(&mut self) -> Option<(ServerRequestId, bool)> {
        match self.commands.try_recv().ok()? {
            SessionCommand::RespondOwnedApplyEdit {
                request_id,
                key,
                applied,
                completed,
                ..
            } => {
                assert!(
                    self.pending
                        .remove(&request_id)
                        .is_some_and(|expected| Arc::ptr_eq(&expected, &key))
                );
                completed.try_send(Ok(())).unwrap();
                Some((request_id, applied))
            }
            _ => panic!("unexpected command in bounded edit endpoint"),
        }
    }
}

// The wire protocol has no instance identifier. Once a URI is reused, push
// diagnostics are quarantined for this session rather than relabeled as fresh.
#[derive(Default)]
pub(crate) struct DiagnosticLineage {
    first: HashMap<String, (crate::editor::BufferId, TextVersion)>,
    ambiguous: std::collections::HashSet<String>,
    exhausted: bool,
}
impl DiagnosticLineage {
    pub(crate) fn observe(&mut self, uri: String, source: &(crate::editor::BufferId, TextVersion)) {
        if let Some(first) = self.first.get(&uri) {
            if first.0 != source.0 || !first.1.same_instance(&source.1) {
                self.ambiguous.insert(uri);
            }
        } else if self.first.len() < 4096 {
            self.first.insert(uri, source.clone());
        } else {
            self.exhausted = true;
        }
    }
    pub(crate) fn tracked(&self, uri: &str) -> bool {
        self.first.contains_key(uri)
    }
    pub(crate) fn ambiguous(&self, uri: &str) -> bool {
        self.ambiguous.contains(uri) || self.exhausted && !self.tracked(uri)
    }
}

#[cfg(test)]
mod tests {
    use crate::language::lsp::origins::*;
    #[test]
    fn full_event_queue_retains_lifecycle_and_result_events() {
        let (channel, receiver) = mpsc::sync_channel(1);
        let (commands, _) = mpsc::sync_channel(1);
        let sender = EventSender {
            channel,
            commands,
            cancelled: Default::default(),
            stop_requested: Default::default(),
            context: EditContext {
                session: Arc::new(()),
                server: "fixture".into(),
                encoding: PositionEncoding::Utf16,
                diagnostic_ambiguous: false,
                sources: Arc::new(HashMap::new()),
                wire_versions: None,
                command_sources: Default::default(),
                workspace_sent_at: None,
                disk: None,
                workspace: None,
                request_document: None,
            },
        };
        sender.send(LspEvent::Log {
            server: "fixture".into(),
            method: None,
            message: "full".into(),
        });
        let worker = std::thread::spawn(move || {
            sender.send(LspEvent::Running {
                server: "fixture".into(),
                capabilities: Default::default(),
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(matches!(
            receiver.recv().unwrap().event,
            LspEvent::Log { .. }
        ));
        assert!(matches!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .event,
            LspEvent::Running { .. }
        ));
        worker.join().unwrap();
    }
}
