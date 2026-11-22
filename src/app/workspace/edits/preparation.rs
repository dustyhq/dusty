//! Workspace file preparation and tracked I/O submission.
use crate::app::workspace::edits::OriginKey;
use crate::app::workspace::edits::Source;
use crate::app::workspace::edits::WorkspaceEdits;
use crate::editor::DocumentSnapshot as FileSnapshot;
use crate::editor::WorkspaceEditSources;
use crate::files::CheckedResourceIntent;
use crate::files::RequestId;
use crate::files::VfsResult;
use crate::language::lsp::PreparedDiskFile;
use crate::workspace::ProposalId;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub(in crate::app) enum InputFile {
    Read(PathBuf),
    ReadMatchingDisk {
        path: PathBuf,
        text: crate::editor::text::TextBuffer,
    },
    Create(PathBuf),
    Resource {
        source: PathBuf,
        target: Option<PathBuf>,
    },
}
impl InputFile {
    pub(in crate::app) fn path(&self) -> &Path {
        match self {
            Self::Read(path) | Self::Create(path) | Self::ReadMatchingDisk { path, .. } => path,
            Self::Resource { source, .. } => source,
        }
    }
}
pub(in crate::app) struct PreparationTicket {
    pub(in crate::app) owner: Arc<()>,
    pub(in crate::app) key: Arc<()>,
    pub(in crate::app) id: ProposalId,
}
#[cfg(test)]
impl PreparationTicket {
    pub(in crate::app) fn id(&self) -> ProposalId {
        self.id
    }
}
pub(in crate::app) enum Slot {
    Waiting(InputFile),
    Pending(RequestId),
    Complete,
}
pub(in crate::app) struct Preparation {
    pub source: Source,
    pub label: String,
    pub origin: Option<OriginKey>,
    pub all_files: bool,
    pub(in crate::app) slots: Vec<Slot>,
    pub(in crate::app) requests: HashMap<RequestId, (usize, InputFile, Arc<()>)>,
    pub(in crate::app) snapshots: Vec<FileSnapshot>,
    disk_paths: HashSet<PathBuf>,
    digests: HashMap<PathBuf, PreparedDiskFile>,
    hashing: Option<std::thread::JoinHandle<HashedFiles>>,
    hashed: bool,
    pub(in crate::app) paths: HashMap<PathBuf, PathBuf>,
    pub(in crate::app) creates: Vec<PathBuf>,
    pub(in crate::app) resources: Vec<CheckedResourceIntent>,
    pub(in crate::app) maximum: usize,
    pub(in crate::app) roots: Vec<PathBuf>,
}
struct HashedFiles {
    snapshots: Vec<FileSnapshot>,
    resources: Vec<CheckedResourceIntent>,
    digests: HashMap<PathBuf, PreparedDiskFile>,
}

pub(in crate::app) struct PreparedFiles<'a> {
    snapshots: &'a [FileSnapshot],
    paths: &'a HashMap<PathBuf, PathBuf>,
    digests: &'a HashMap<PathBuf, PreparedDiskFile>,
}
impl PreparedFiles<'_> {
    pub(in crate::app) fn canonical(&self, requested: &Path) -> Option<&Path> {
        self.paths.get(requested).map(PathBuf::as_path)
    }
    pub(in crate::app) fn disk_file(&self, canonical: &Path) -> Option<&PreparedDiskFile> {
        self.digests.get(canonical)
    }
    pub(in crate::app) fn snapshot(&self, requested: &Path) -> Option<&FileSnapshot> {
        let canonical = self.paths.get(requested)?;
        self.snapshots
            .iter()
            .find(|s| &s.canonical_path == canonical)
    }
    pub(in crate::app) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.snapshots.iter().map(|s| s.canonical_path.as_path())
    }
}
// This borrow keeps the preparation and its sources alive until submission.
// Submission records the pending reply before releasing the borrow.
pub(in crate::app) struct PreparedIo<'a> {
    preparation: &'a mut Preparation,
    index: usize,
    _workspace: &'a crate::app::workspace::Workspace,
    _configuration: &'a u64,
    _editor: &'a crate::editor::EditorState,
}
impl PreparedIo<'_> {
    pub(in crate::app) fn file(&self, worker: &Arc<()>) -> Result<InputFile, String> {
        if self
            .preparation
            .requests
            .values()
            .any(|(_, _, expected)| !Arc::ptr_eq(worker, expected))
        {
            return Err("workspace preparation already belongs to another document worker".into());
        }
        match &self.preparation.slots[self.index] {
            Slot::Waiting(file) => Ok(file.clone()),
            _ => Err("workspace operation is no longer waiting".into()),
        }
    }
    pub(in crate::app) fn roots(&self) -> &[PathBuf] {
        &self.preparation.roots
    }
    pub(in crate::app) fn maximum(&self) -> usize {
        self.preparation.maximum
    }
    pub(in crate::app) fn finish(
        self,
        submission: crate::app::documents::WorkspaceSubmission,
    ) -> Result<(), String> {
        let (worker, result) = submission.into_parts();
        let id = result?;
        if self.preparation.requests.contains_key(&id) {
            return Err("workspace request identity reused".into());
        }
        let Slot::Waiting(file) =
            std::mem::replace(&mut self.preparation.slots[self.index], Slot::Pending(id))
        else {
            return Err("workspace operation changed during submission".into());
        };
        self.preparation
            .requests
            .insert(id, (self.index, file, worker));
        Ok(())
    }
}
impl WorkspaceEdits {
    pub(in crate::app) fn start_files(
        &mut self,
        ticket: PreparationTicket,
        label: String,
        origin: Option<OriginKey>,
        all_files: bool,
        files: Vec<InputFile>,
        policy: (Vec<PathBuf>, usize),
    ) -> Result<(), String> {
        let (roots, maximum) = policy;
        self.check_ticket(&ticket)?;
        let preparing = self.preparing.as_mut().expect("checked source");
        preparing.label = label;
        preparing.origin = origin;
        preparing.all_files = all_files;
        if files.len() > 1024 || maximum == 0 {
            return Err("workspace edit preparation exceeds its safety limit".into());
        }
        for file in &files {
            if !roots.iter().any(|root| file.path().starts_with(root))
                || matches!(file, InputFile::Resource { target: Some(target), .. } if !roots.iter().any(|root| target.starts_with(root)))
            {
                return Err("workspace edit path is outside the allowed roots".into());
            }
        }
        let preparing = self.preparing.as_mut().expect("checked source");
        if !preparing.slots.is_empty() {
            return Err("workspace file preparation already started".into());
        }
        let creates = files
            .iter()
            .filter_map(|file| {
                if let InputFile::Create(path) = file {
                    Some(path.clone())
                } else {
                    None
                }
            })
            .collect::<std::collections::HashSet<_>>();
        preparing.slots = files
            .into_iter()
            .filter(|file| !matches!(file, InputFile::Read(path) if creates.contains(path)))
            .map(Slot::Waiting)
            .collect();
        preparing.roots = roots;
        preparing.maximum = maximum;
        Ok(())
    }
    pub(in crate::app) fn next_file(&self) -> Option<(usize, &InputFile)> {
        self.preparing
            .as_ref()?
            .slots
            .iter()
            .enumerate()
            .find_map(|(index, slot)| match slot {
                Slot::Waiting(file) => Some((index, file)),
                _ => None,
            })
    }
    pub(in crate::app) fn preparing_sources(&self) -> Option<&WorkspaceEditSources> {
        self.preparing.as_ref().map(|p| &p.source.editor)
    }
    pub(in crate::app) fn preparing_origin(&self) -> Option<&OriginKey> {
        self.preparing.as_ref()?.origin.as_ref()
    }
    pub(in crate::app) fn preparing(&self) -> bool {
        self.preparing.is_some()
    }
    pub(in crate::app) fn files_ready(&self) -> bool {
        self.preparing
            .as_ref()
            .is_some_and(|p| p.slots.iter().all(|slot| matches!(slot, Slot::Complete)))
    }
    pub(in crate::app) fn prepared_files(&self) -> Result<PreparedFiles<'_>, String> {
        if !self.files_ready() {
            return Err("workspace file preparation is incomplete".into());
        }
        let p = self.preparing.as_ref().expect("ready preparation");
        Ok(PreparedFiles {
            snapshots: &p.snapshots,
            paths: &p.paths,
            digests: &p.digests,
        })
    }
    /// Hash only the chosen edit's already-scoped reads on a preparation worker.
    /// Move the bytes, rather than copying the full read set into the worker.
    pub(in crate::app) fn hash_prepared_files(&mut self) -> Result<bool, String> {
        let p = self
            .preparing
            .as_mut()
            .ok_or("workspace preparation is stale")?;
        if p.hashed || p.disk_paths.is_empty() && p.resources.is_empty() && p.hashing.is_none() {
            return Ok(true);
        }
        if let Some(worker) = &p.hashing {
            if !worker.is_finished() {
                return Ok(false);
            }
            let hashed = p
                .hashing
                .take()
                .expect("hash worker")
                .join()
                .map_err(|_| "workspace preparation worker failed")?;
            p.snapshots = hashed.snapshots;
            p.resources = hashed.resources;
            p.digests = hashed.digests;
            p.hashed = true;
            return Ok(true);
        }
        const MAX_HASH_BYTES: usize = 256 * 1024 * 1024;
        let disk_bytes = p
            .snapshots
            .iter()
            .filter(|snapshot| p.disk_paths.contains(&snapshot.canonical_path))
            .map(|snapshot| snapshot.bytes.len())
            .chain(
                p.resources
                    .iter()
                    .map(|resource| resource.source_bytes.len()),
            )
            .fold(0_usize, usize::saturating_add);
        if disk_bytes > MAX_HASH_BYTES {
            return Err("workspace edit preparation exceeds its safety limit".into());
        }
        let snapshots = std::mem::take(&mut p.snapshots);
        let resources = std::mem::take(&mut p.resources);
        let disk_paths = p.disk_paths.clone();
        p.hashing = Some(
            std::thread::Builder::new()
                .name("dusty-workspace-edit-verify".into())
                .spawn(move || {
                    let mut digests = HashMap::new();
                    for snapshot in &snapshots {
                        if disk_paths.contains(&snapshot.canonical_path) {
                            digests
                                .entry(snapshot.canonical_path.clone())
                                .or_insert_with(|| {
                                    PreparedDiskFile::from_bytes(
                                        &snapshot.canonical_path,
                                        &snapshot.bytes,
                                    )
                                });
                        }
                    }
                    for resource in &resources {
                        digests.insert(
                            resource.source.clone(),
                            PreparedDiskFile::from_bytes(&resource.source, &resource.source_bytes),
                        );
                    }
                    HashedFiles {
                        snapshots,
                        resources,
                        digests,
                    }
                })
                .map_err(|error| error.to_string())?,
        );
        Ok(false)
    }

    pub(in crate::app) fn normalize_lsp_alias_snapshots(
        &mut self,
        editor: &crate::editor::EditorState,
    ) -> Result<(), String> {
        let p = self
            .preparing
            .as_mut()
            .ok_or("workspace preparation is stale")?;
        for file in &mut p.snapshots {
            if file.revision.is_none()
                && p.disk_paths.contains(&file.canonical_path)
                && let Some(open) =
                    editor.workspace_snapshot(&p.source.editor, &file.canonical_path, p.maximum)?
            {
                *file = open.into_file(&p.source.editor, &file.canonical_path)?;
            }
        }
        Ok(())
    }

    pub(in crate::app) fn open_snapshot(
        &mut self,
        index: usize,
        snapshot: crate::editor::WorkspaceSnapshot,
    ) -> Result<(), String> {
        let p = self
            .preparing
            .as_mut()
            .ok_or("workspace preparation is stale")?;
        let Some(Slot::Waiting(InputFile::Read(path))) = p.slots.get(index) else {
            return Err("workspace snapshot did not match a pending read".into());
        };
        let snapshot = snapshot.into_file(&p.source.editor, path)?;
        if snapshot.bytes.len() > p.maximum {
            return Err("workspace snapshot exceeds or differs from its accepted read".into());
        }
        p.paths
            .insert(path.clone(), snapshot.canonical_path.clone());
        p.snapshots.push(snapshot);
        p.slots[index] = Slot::Complete;
        Ok(())
    }
    pub(in crate::app) fn prepare_io<'a>(
        &'a mut self,
        index: usize,
        workspace: &'a crate::app::workspace::Workspace,
        configuration: &'a u64,
        editor: &'a crate::editor::EditorState,
    ) -> Result<PreparedIo<'a>, String> {
        let p = self
            .preparing
            .as_mut()
            .ok_or("workspace preparation is stale")?;
        if !p.source.current(&workspace.version(), *configuration) || *configuration == u64::MAX {
            return Err("workspace preparation source changed or exhausted".into());
        }
        let Some(Slot::Waiting(file)) = p.slots.get(index) else {
            return Err("workspace operation is not waiting".into());
        };
        editor.validate_workspace_sources(&p.source.editor, std::iter::once(file.path()))?;
        Ok(PreparedIo {
            preparation: p,
            index,
            _workspace: workspace,
            _configuration: configuration,
            _editor: editor,
        })
    }
    pub(in crate::app) fn owns_preparation_reply(
        &self,
        reply: &crate::app::documents::WorkspaceIoReply,
    ) -> bool {
        self.preparing
            .as_ref()
            .and_then(|p| p.requests.get(&reply.id()))
            .is_some_and(|(_, _, worker)| reply.is_from(worker))
    }
    pub(in crate::app) fn accept_preparation_reply(
        &mut self,
        reply: crate::app::documents::WorkspaceIoReply,
        workspace: &crate::app::workspace::WorkspaceVersion,
        configuration: u64,
    ) -> Result<(), String> {
        if !self.owns_preparation_reply(&reply) {
            return Err("unowned workspace preparation terminal".into());
        }
        let result = reply.into_result();
        let id = match &result {
            VfsResult::Read { id, .. }
            | VfsResult::CreateTarget { id, .. }
            | VfsResult::ResourceResolved { id, .. } => *id,
            _ => return Err("wrong workspace preparation terminal".into()),
        };
        let p = self
            .preparing
            .as_mut()
            .ok_or("workspace preparation is stale")?;
        let (index, file, _) = p
            .requests
            .remove(&id)
            .ok_or("unowned workspace preparation terminal")?;
        if !matches!(p.slots.get(index), Some(Slot::Pending(expected)) if *expected == id)
            || !p.source.current(workspace, configuration)
        {
            return Err("workspace preparation source changed".into());
        }
        p.slots[index] = Slot::Complete;
        match (file, result) {
            (
                file @ (InputFile::Read(_) | InputFile::ReadMatchingDisk { .. }),
                VfsResult::Read { path, result, .. },
            ) => {
                let bytes = result.map_err(|e| e.to_string())?;
                if bytes.len() > p.maximum {
                    return Err("workspace read exceeds its requested byte limit".into());
                }
                if !p.roots.iter().any(|root| path.starts_with(root)) {
                    return Err("workspace read escaped its scope".into());
                }
                let revision = if let InputFile::ReadMatchingDisk { text, .. } = &file {
                    if !text.matches_encoded_text(&bytes) {
                        return Err(crate::app::language::edits::STALE_EDIT.into());
                    }
                    Some(text.revision())
                } else {
                    None
                };
                p.disk_paths.insert(path.clone());
                p.paths.insert(file.path().to_owned(), path.clone());
                p.snapshots.push(FileSnapshot {
                    canonical_path: path,
                    revision,
                    bytes,
                });
            }
            (InputFile::Create(requested), VfsResult::CreateTarget { result, .. }) => {
                let path = result.map_err(|e| e.to_string())?;
                p.paths.insert(requested, path.clone());
                p.creates.push(path.clone());
                p.snapshots.push(FileSnapshot {
                    canonical_path: path,
                    revision: None,
                    bytes: Vec::new(),
                });
            }
            (
                InputFile::Resource { source, target },
                VfsResult::ResourceResolved { result, .. },
            ) => {
                let intent = result.map_err(|e| e.to_string())?;
                p.paths.insert(source, intent.source.clone());
                if let (Some(requested), crate::files::ResourceIntentKind::Rename { target, .. }) =
                    (target, &intent.kind)
                {
                    p.paths.insert(requested, target.clone());
                }
                p.resources.push(intent);
            }
            _ => return Err("workspace worker returned the wrong operation terminal".into()),
        }
        Ok(())
    }
}

impl Preparation {
    pub(in crate::app) fn new(source: Source) -> Self {
        Self {
            source,
            label: String::new(),
            origin: None,
            all_files: false,
            slots: Vec::new(),
            requests: HashMap::new(),
            snapshots: Vec::new(),
            disk_paths: HashSet::new(),
            digests: HashMap::new(),
            hashing: None,
            hashed: false,
            paths: HashMap::new(),
            creates: Vec::new(),
            resources: Vec::new(),
            maximum: 0,
            roots: Vec::new(),
        }
    }
}
