//! Native Git selections use the existing checked mutation and bounded command workers.
use super::*;
use crate::mutation::{
    IndexLock, IndexWrite, MARKED_DIRECTORY_SCOPE, index_entries_fingerprint, index_fingerprint,
    indexed_file_ancestors, run_index_command,
};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[cfg(test)]
mod tests;

const CHECK_STAGE: &str = "Couldn't check what is staged";
const STAGE_FAILED: &str = "Couldn't stage the selected files.";
const STAGE_CHANGED: &str = "Files changed. Refresh and try staging again.";
const DISCARD_LOCKED: &str = "Another Git command is using the repository. Try discarding again.";
const STAGE_GIT_TIMEOUT: Duration = Duration::from_secs(30);
const DIRECTORY_SCOPE: &str = "Show the replaced file and all its changed contents before staging.";

#[derive(Clone, Copy)]
enum StageScope {
    All,
    Directory,
    Marked,
}

impl StageScope {
    fn replacement_refusal(self) -> &'static str {
        match self {
            Self::Marked => MARKED_DIRECTORY_SCOPE,
            Self::All | Self::Directory => DIRECTORY_SCOPE,
        }
    }
}

#[derive(Clone)]
pub(super) struct BulkStage {
    scope: StageScope,
    paths: BTreeSet<PathBuf>,
    candidate: GitIndexFingerprint,
    index: GitIndexFingerprint,
    head: Option<GitHeadId>,
    notice: Option<String>,
    pub(super) warning: String,
}

pub(super) struct BulkStageWorker {
    receiver: Receiver<Result<BulkStageResult, String>>,
    cancel: Arc<AtomicBool>,
    pending_consent: Option<BulkStage>,
}

impl Drop for BulkStageWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

enum BulkStageResult {
    NeedsConsent(BulkStage),
    Staged(Option<String>),
}

fn unmerged_paths(index: &git2::Index) -> Result<BTreeSet<PathBuf>, git2::Error> {
    let mut paths = BTreeSet::new();
    for conflict in index.conflicts()? {
        let conflict = conflict?;
        paths.extend(
            [conflict.ancestor, conflict.our, conflict.their]
                .into_iter()
                .flatten()
                .filter_map(|entry| crate::conflict::git_path(&entry.path)),
        );
    }
    Ok(paths)
}

type IndexVersion = (git2::Oid, u32);

fn stage_zero(index: &git2::Index, path: &Path) -> Option<IndexVersion> {
    index.get_path(path, 0).map(|entry| (entry.id, entry.mode))
}

fn staged_replacements(
    repo: &git2::Repository,
    before: &git2::Index,
    after: &git2::Index,
    paths: &BTreeSet<PathBuf>,
    head: Option<GitHeadId>,
) -> Result<BTreeSet<PathBuf>, git2::Error> {
    let tree = head
        .map(|id| repo.find_commit(git2::Oid::from_bytes(&id.0)?)?.tree())
        .transpose()?;
    let mut replacing = BTreeSet::new();
    for path in paths {
        let staged = before.get_path(path, 0);
        // Intent-to-add does not contain any staged work to lose.
        if staged
            .as_ref()
            .is_some_and(|entry| entry.flags_extended & INTENT_TO_ADD != 0)
        {
            continue;
        }
        let staged = staged.map(|entry| (entry.id, entry.mode));
        let candidate = stage_zero(after, path);
        if staged == candidate {
            continue;
        }
        let committed = match tree.as_ref().map(|tree| tree.get_path(path)).transpose() {
            Ok(entry) => entry
                .filter(|entry| entry.kind() != Some(git2::ObjectType::Tree))
                .map(|entry| (entry.id(), entry.filemode() as u32)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => None,
            Err(error) => return Err(error),
        };
        if staged != committed && !keeps_staged_changes(repo, committed, staged, candidate) {
            replacing.insert(path.clone());
        }
    }
    Ok(replacing)
}

fn keeps_staged_changes(
    repo: &git2::Repository,
    committed: Option<IndexVersion>,
    staged: Option<IndexVersion>,
    candidate: Option<IndexVersion>,
) -> bool {
    let (Some((staged_blob, staged_mode)), Some((candidate_blob, candidate_mode))) =
        (staged, candidate)
    else {
        // Adding back a staged deletion or removing a staged file loses staged work.
        return false;
    };
    if committed.is_none_or(|(_, mode)| mode != staged_mode) && staged_mode != candidate_mode {
        return false;
    }
    if staged_blob == candidate_blob || committed.is_some_and(|(blob, _)| blob == staged_blob) {
        return true;
    }

    // Only text blobs have changes that can be checked line by line. A different
    // binary, symlink, or submodule version requires consent.
    let regular = |mode| matches!(mode, 0o100644 | 0o100755);
    regular(staged_mode)
        && regular(candidate_mode)
        && committed.is_none_or(|(_, mode)| regular(mode))
        && text_changes_survive(
            repo,
            committed.map(|(blob, _)| blob),
            staged_blob,
            candidate_blob,
        )
        .unwrap_or(false)
}

fn text_changes_survive(
    repo: &git2::Repository,
    committed: Option<git2::Oid>,
    staged: git2::Oid,
    candidate: git2::Oid,
) -> Result<bool, git2::Error> {
    let committed = committed.map(|oid| repo.find_blob(oid)).transpose()?;
    let committed_bytes: &[u8] = committed.as_ref().map_or(&[], git2::Blob::content);
    let staged = repo.find_blob(staged)?;
    let candidate = repo.find_blob(candidate)?;
    if committed.as_ref().is_some_and(git2::Blob::is_binary)
        || staged.is_binary()
        || candidate.is_binary()
    {
        return Ok(false);
    }

    // Both sets of ranges use the staged blob's coordinates. Zero-context hunks
    // distinguish a staged edit from changes elsewhere in the candidate.
    let mut options = git2::DiffOptions::new();
    options.context_lines(0);
    let staged_patch = git2::Patch::from_buffers(
        committed_bytes,
        None,
        staged.content(),
        None,
        Some(&mut options),
    )?;
    let candidate_patch =
        git2::Patch::from_blobs(&staged, None, &candidate, None, Some(&mut options))?;
    if staged_patch.num_hunks() == 0 || candidate_patch.num_hunks() == 0 {
        return Ok(false);
    }
    let range = |start, lines| -> Range<u32> {
        let start = if lines == 0 { start } else { start - 1 };
        start..start + lines
    };
    let mut removed = BTreeSet::new();
    let mut next = 0;
    for i in 0..staged_patch.num_hunks() {
        let (staged_hunk, staged_lines) = staged_patch.hunk(i)?;
        for line in 0..staged_lines {
            let line = staged_patch.line_in_hunk(i, line)?;
            if line.origin() == '-' {
                removed.insert(line.content());
            }
        }
        let staged = range(staged_hunk.new_start(), staged_hunk.new_lines());
        while next < candidate_patch.num_hunks() {
            let (hunk, _) = candidate_patch.hunk(next)?;
            let changed = range(hunk.old_start(), hunk.old_lines());
            // A deletion has no lines to match. Protect its gap and its anchors.
            let overlaps = if staged.is_empty() {
                changed.start <= staged.start && staged.start <= changed.end
            } else {
                changed.start < staged.end && staged.start < changed.end
            };
            if overlaps {
                return Ok(false);
            }
            if changed.end > staged.start {
                break;
            }
            next += 1;
        }
    }

    // Diff alignment can move a reintroduced line away from its original gap.
    // Compare additions across the whole candidate instead of trusting adjacency.
    if !removed.is_empty() {
        for i in 0..candidate_patch.num_hunks() {
            let (hunk, changed_lines) = candidate_patch.hunk(i)?;
            if hunk.new_lines() == 0 {
                continue;
            }
            for line in 0..changed_lines {
                let line = candidate_patch.line_in_hunk(i, line)?;
                if line.origin() == '+' && removed.contains(line.content()) {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn verify_stage_state(
    root: &Path,
    index: GitIndexFingerprint,
    head: Option<GitHeadId>,
) -> Result<(), String> {
    if git_index_fingerprint(root).map_err(|_| CHECK_STAGE)? != index
        || git_head_id(root).map_err(|_| CHECK_STAGE)? != head
    {
        return Err(STAGE_CHANGED.into());
    }
    Ok(())
}

fn private_stage_args(root: &Path, empty_hooks: &Path) -> Result<Vec<OsString>, String> {
    let mut hooks_path = OsString::from("core.hooksPath=");
    hooks_path.push(empty_hooks);
    let mut args = vec![
        "--literal-pathspecs".into(),
        "-c".into(),
        "core.splitIndex=false".into(),
        "-c".into(),
        hooks_path,
    ];

    // Newer Git also supports hooks declared in config, outside the hooks directory.
    let keys = crate::command::supervised_git_output(
        root,
        &[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^hook\..*\.event$",
        ]
        .map(OsString::from),
        &[],
        STAGE_GIT_TIMEOUT,
    )
    .map_err(|_| STAGE_FAILED)?;
    match keys.code {
        Some(0) => {
            let keys = keys.stdout.strip_suffix(&[0]).ok_or(STAGE_FAILED)?;
            let mut names = BTreeSet::new();
            for key in keys.split(|&byte| byte == 0) {
                let key = std::str::from_utf8(key).map_err(|_| STAGE_FAILED)?;
                names.insert(key.strip_suffix(".event").ok_or(STAGE_FAILED)?);
            }
            for name in names {
                args.extend(["-c".into(), format!("{name}.enabled=false").into()]);
            }
        }
        Some(1) if keys.stdout.is_empty() => {} // No matching config keys.
        _ => return Err(STAGE_FAILED.into()),
    }
    args.extend(
        [
            "add",
            "--all",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]
        .map(OsString::from),
    );
    Ok(args)
}

fn notify_stage_hook(root: &Path) {
    // The real index is already installed. Git owns hook discovery and execution.
    // Git before 2.36 lacks this command; silently skip the notification there.
    let _ = crate::command::supervised_git_output(
        root,
        &[
            "hook",
            "run",
            "--ignore-missing",
            "post-index-change",
            "--",
            "0",
            "0",
        ]
        .map(OsString::from),
        &[],
        STAGE_GIT_TIMEOUT,
    );
}

struct BulkStageRequest {
    paths: BTreeSet<PathBuf>,
    scope: StageScope,
    index: GitIndexFingerprint,
    head: Option<GitHeadId>,
    notice: Option<String>,
    reviewed: Option<GitIndexFingerprint>,
}

impl BulkStageRequest {
    fn apply(self, root: &Path, cancel: &AtomicBool) -> Result<BulkStageResult, String> {
        if cancel.load(Ordering::Acquire) {
            return Err(STAGE_FAILED.into());
        }
        let repo = git2::Repository::open(root).map_err(|_| CHECK_STAGE)?;
        let empty_hooks = tempfile::tempdir_in(repo.path()).map_err(|_| STAGE_FAILED)?;
        let args = private_stage_args(root, empty_hooks.path())?;
        let write = IndexWrite::start(&repo).map_err(|_| STAGE_CHANGED)?;
        verify_stage_state(root, self.index, self.head)?;
        let before = repo.index().map_err(|_| CHECK_STAGE)?;
        if index_fingerprint(&before) != self.index {
            return Err(STAGE_CHANGED.into());
        }

        // Ask Git itself what add would publish, with its actual clean filters,
        // symlink/submodule handling, and file mode rules. It writes only the copy.
        let mut input = Vec::new();
        let selected: BTreeSet<_> = self
            .paths
            .iter()
            .map(|path| path.as_os_str().as_encoded_bytes())
            .collect();
        for path in &selected {
            input.extend_from_slice(path);
            input.push(0);
        }
        run_index_command(root, &write.temporary, args, input, cancel).map_err(|_| {
            if self.reviewed.is_some() {
                STAGE_CHANGED
            } else {
                STAGE_FAILED
            }
        })?;
        let after = git2::Index::open(&write.temporary).map_err(|_| CHECK_STAGE)?;
        let outside_selection = |index: &git2::Index| {
            index_entries_fingerprint(
                index
                    .iter()
                    .filter(|entry| !selected.contains(entry.path.as_slice())),
            )
        };
        if outside_selection(&before) != outside_selection(&after) {
            return Err(match self.reviewed {
                Some(_) => STAGE_CHANGED,
                None => self.scope.replacement_refusal(),
            }
            .into());
        }
        verify_stage_state(root, self.index, self.head)?;

        let candidate = index_fingerprint(&after);
        if let Some(reviewed) = self.reviewed {
            if candidate != reviewed {
                return Err(STAGE_CHANGED.into());
            }
        } else {
            let replacing = staged_replacements(&repo, &before, &after, &self.paths, self.head)
                .map_err(|_| CHECK_STAGE)?;
            if let Some(first) = replacing.first() {
                let more = if replacing.len() > 1 {
                    format!(" (and {} more)", replacing.len() - 1)
                } else {
                    String::new()
                };
                let warning = format!(
                    "Staging the working copy would erase the staged version of {}{more}, which isn't saved anywhere else.",
                    first.display()
                );
                return Ok(BulkStageResult::NeedsConsent(BulkStage {
                    scope: self.scope,
                    paths: self.paths,
                    candidate,
                    index: self.index,
                    head: self.head,
                    notice: self.notice,
                    warning,
                }));
            }
        }

        if cancel.load(Ordering::Acquire) {
            return Err(STAGE_FAILED.into());
        }
        verify_stage_state(root, self.index, self.head)?;
        // There is no second add: the exact index we checked is the one installed.
        write.commit().map_err(|_| STAGE_FAILED)?;
        drop(write); // The hook can read or even write the live index.
        notify_stage_hook(root);
        Ok(BulkStageResult::Staged(self.notice))
    }
}

impl GitScreen {
    pub(super) fn stage_directory(&mut self, directory: &Path) -> Result<(), String> {
        self.allow_write()?;
        if self.filters[0].active() {
            return Err(
                "Clear the filter before toggling a whole directory, or select a matching file"
                    .into(),
            );
        }
        let snapshot = self.snapshot.as_ref().ok_or("Status is loading")?;
        if snapshot.truncated {
            return Err(
                "Status is incomplete; toggle individual files instead of a directory".into(),
            );
        }
        let paths = snapshot
            .entries
            .iter()
            .filter(|entry| entry.relative_path.starts_with(directory))
            .map(|entry| entry.relative_path.clone())
            .collect::<BTreeSet<_>>();
        if paths.is_empty() {
            return Err("This directory has no changed descendants".into());
        }
        self.stage_bulk(&paths, StageScope::Directory)
    }

    pub(super) fn stage_all(&mut self) -> Result<(), String> {
        let snapshot = self.snapshot.as_ref().ok_or("Status is loading")?;
        if snapshot.truncated {
            return Err(
                "Too many changed files to stage them all here. Use git add in a terminal.".into(),
            );
        }
        let paths = if self.filters[0].active() {
            let paths = self
                .rows(0)
                .iter()
                .filter(|row| !row.directory)
                .map(|row| row.value.clone())
                .collect::<BTreeSet<_>>();
            if paths.is_empty() {
                return Err("No files match the filter".into());
            }
            paths
        } else {
            snapshot
                .entries
                .iter()
                .map(|entry| entry.relative_path.clone())
                .collect()
        };
        if paths.is_empty() {
            return Ok(());
        }
        self.stage_bulk(&paths, StageScope::All)
    }

    /// Marked files share the same exact-path safeguards as directory toggles and `a`.
    pub(super) fn stage_paths(&mut self, selection: &BTreeSet<PathBuf>) -> Result<(), String> {
        self.stage_bulk(selection, StageScope::Marked)
    }

    fn stage_bulk(
        &mut self,
        selection: &BTreeSet<PathBuf>,
        scope: StageScope,
    ) -> Result<(), String> {
        self.allow_write()?;
        let toggle_all = matches!(scope, StageScope::All);
        let snapshot = self.snapshot.as_ref().ok_or("Status is loading")?;
        let mut entries = selection
            .iter()
            .filter_map(|path| memo::entry(&snapshot.entries, path))
            .collect::<Vec<_>>();
        if entries.is_empty() || entries.len() != selection.len() {
            return Err("The marked files changed; refresh and select them again".into());
        }
        let (index_before, head) = self.bulk_stage_state()?;
        let (repository, index) = self.stage_index()?;
        let conflicted = unmerged_paths(&index).map_err(|_| CHECK_STAGE)?;
        let conflicts_with = |path: &Path| {
            use std::ops::Bound::{Included, Unbounded};
            path.ancestors()
                .any(|ancestor| conflicted.contains(ancestor))
                || conflicted
                    .range::<Path, _>((Included(path), Unbounded))
                    .next()
                    .is_some_and(|conflict| conflict.starts_with(path))
        };
        entries.retain(|entry| {
            entry.status != GitFileStatus::Conflicted
                && !conflicts_with(&entry.relative_path)
                && entry
                    .original_path
                    .as_deref()
                    .is_none_or(|path| !conflicts_with(path))
        });
        let skipped = selection.len() - entries.len();
        let skip_notice = match skipped {
            0 => String::new(),
            1 => "1 file with conflicts was skipped: resolve it, then stage it on its own.".into(),
            n => format!(
                "{n} files with conflicts were skipped: resolve them, then stage them one at a time."
            ),
        };
        if entries.is_empty() {
            self.notice = skip_notice;
            return Ok(());
        }
        // Dirty submodules can look unstaged in a selection even when their pointer matches.
        let fully_staged = |entry: &GitStatusEntry| {
            entry.sources.staged
                && (!entry.sources.unstaged
                    || (!toggle_all && self.submodule_pointer_matches_index(&entry.relative_path)))
        };
        // Conflicts aren't part of the write and must not determine which way it goes.
        let all_staged = entries.iter().all(|entry| fully_staged(entry));
        let unstage = match if toggle_all { None } else { self.stage_intent } {
            Some(true) if all_staged => {
                if skipped != 0 {
                    self.notice = skip_notice;
                    return Ok(());
                }
                return Err("Already staged · u unstages them".into());
            }
            Some(true) => false,
            Some(false) if !entries.iter().any(|entry| entry.sources.staged) => {
                if skipped != 0 {
                    self.notice = skip_notice;
                    return Ok(());
                }
                return Err("Nothing staged here · s stages them".into());
            }
            Some(false) => true,
            None => all_staged,
        };
        let mut paths = BTreeSet::new();
        let mut count = 0;
        for entry in entries {
            let needs_change = if unstage {
                entry.sources.staged
            } else {
                !fully_staged(entry)
            };
            if !needs_change {
                continue;
            }
            count += 1;
            paths.insert(entry.relative_path.clone());
            if let Some(original) = &entry.original_path
                && (unstage || index.get_path(original, 0).is_some())
            {
                // The source and destination together are the rename shown by this row.
                paths.insert(original.clone());
            }
        }
        let notice = (skipped != 0).then(|| {
            let verb = if unstage { "Unstaged" } else { "Staged" };
            let files = if count == 1 { "file" } else { "files" };
            format!("{verb} {count} {files}. {skip_notice}")
        });
        if unstage {
            return self.unstage_paths(paths, index_before, head, notice);
        }

        self.check_stage_paths(&repository, &index, &paths, scope)?;
        self.start_bulk_stage(BulkStageRequest {
            paths,
            scope,
            index: index_before,
            head,
            notice,
            reviewed: None,
        })
    }

    fn bulk_stage_state(&self) -> Result<(GitIndexFingerprint, Option<GitHeadId>), String> {
        Ok((
            git_index_fingerprint(&self.root).map_err(|_| CHECK_STAGE)?,
            git_head_id(&self.root).map_err(|_| CHECK_STAGE)?,
        ))
    }

    fn stage_index(&self) -> Result<(git2::Repository, git2::Index), String> {
        let repo = git2::Repository::open(&self.root).map_err(|_| CHECK_STAGE)?;
        let index = repo.index().map_err(|_| CHECK_STAGE)?;
        Ok((repo, index))
    }

    pub(super) fn stage_anyway(&mut self, plan: BulkStage) -> Result<(), String> {
        self.start_bulk_stage(BulkStageRequest {
            scope: plan.scope,
            paths: plan.paths,
            index: plan.index,
            head: plan.head,
            notice: plan.notice,
            reviewed: Some(plan.candidate),
        })
    }

    fn start_bulk_stage(&mut self, request: BulkStageRequest) -> Result<(), String> {
        self.allow_write()?;
        if self.bulk_stage_worker.is_some() {
            // A decision is already waiting to be shown.
            return Ok(());
        }
        let root = self.root.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("dusty-git-bulk-stage".into())
            .spawn(move || {
                let _ = sender.send(request.apply(&root, &worker_cancel));
            })
            .map_err(|_| STAGE_FAILED)?;
        self.bulk_stage_worker = Some(BulkStageWorker {
            receiver,
            cancel,
            pending_consent: None,
        });
        self.busy = true;
        self.notice = "Running Git…".into();
        Ok(())
    }

    pub(super) fn poll_bulk_stage(&mut self) -> bool {
        let Some(worker) = &mut self.bulk_stage_worker else {
            return false;
        };
        if worker.pending_consent.is_some()
            && (self.form.is_some() || self.busy || self.refreshing_after_write)
        {
            return false;
        }
        let result = if let Some(plan) = worker.pending_consent.take() {
            Ok(BulkStageResult::NeedsConsent(plan))
        } else {
            match worker.receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => Err(STAGE_FAILED.into()),
            }
        };
        self.busy = false;
        match result {
            Ok(BulkStageResult::NeedsConsent(plan)) if self.form.is_some() => {
                worker.pending_consent = Some(plan);
                if self.notice == "Running Git…" {
                    self.notice.clear();
                }
                return true;
            }
            Ok(BulkStageResult::NeedsConsent(plan)) => {
                self.form(
                    "Replace staged changes?",
                    FormAction::StageAnyway(Box::new(plan)),
                    vec![],
                );
                self.form.as_mut().expect("just opened").selected = 1; // Cancel.
            }
            Ok(BulkStageResult::Staged(notice)) => {
                self.receipt = None;
                self.notice = notice.unwrap_or_default();
                let result = self.refresh();
                self.refreshing_after_write = result.is_ok();
                self.report(result);
            }
            Err(error) => {
                self.receipt = None;
                self.notice = error;
            }
        }
        self.bulk_stage_worker = None;
        true
    }

    fn check_stage_paths(
        &self,
        repo: &git2::Repository,
        index: &git2::Index,
        paths: &BTreeSet<PathBuf>,
        scope: StageScope,
    ) -> Result<(), String> {
        let mut directories = Vec::new();
        for path in paths {
            // Adding a descendant also removes any tracked file it replaced.
            if indexed_file_ancestors(index, path).any(|parent| !paths.contains(parent)) {
                return Err(scope.replacement_refusal().into());
            }
            if !std::fs::symlink_metadata(self.root.join(path))
                .is_ok_and(|metadata| metadata.file_type().is_dir())
            {
                continue;
            }
            let gitlink = index
                .get_path(path, 0)
                .is_some_and(|entry| entry.mode == 0o160000)
                || self
                    .refs
                    .as_ref()
                    .and_then(|refs| refs.submodules.as_ref().ok())
                    .is_some_and(|modules| modules.iter().any(|module| &module.path == path));
            if !gitlink {
                directories.push(path);
            }
        }
        if directories.is_empty() {
            return Ok(());
        }

        // A literal directory still recurses. Check the worktree rather than relying
        // on the last status snapshot: untracked descendants may not be in it yet.
        let mut options = git2::DiffOptions::new();
        options
            .disable_pathspec_match(true)
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_unreadable(true);
        for directory in directories {
            options.pathspec(directory);
        }
        let diff = repo
            .diff_index_to_workdir(Some(index), Some(&mut options))
            .map_err(|_| CHECK_STAGE)?;
        for delta in diff.deltas() {
            let file = delta.new_file();
            let path = file.path().ok_or(CHECK_STAGE)?;
            if !paths.contains(path) {
                return Err(scope.replacement_refusal().into());
            }
        }
        Ok(())
    }

    pub(super) fn unstage_paths(
        &mut self,
        paths: BTreeSet<PathBuf>,
        index: GitIndexFingerprint,
        head: Option<GitHeadId>,
        notice: Option<String>,
    ) -> Result<(), String> {
        self.allow_write()?;
        self.mutation
            .start_unstage_paths(self.root.clone(), paths, index, head)
            .map_err(|error| error.to_string())?;
        self.stage_notice = notice;
        self.busy = true;
        self.notice = "Running Git…".into();
        Ok(())
    }

    pub(super) fn stage_file(&mut self, mut plan: GitChangePlanRequest) -> Result<(), String> {
        if plan.kind != GitMutationKind::StageFile {
            return self.apply_stage_file(plan);
        }
        let path = &plan.policy.file;
        let (repo, index) = self.stage_index()?;
        let file = self.root.join(path);
        if !(1..=3).any(|stage| index.get_path(path, stage).is_some())
            || std::fs::symlink_metadata(&file)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return self.apply_stage_file(plan);
        }
        let (hash, bytes) =
            worktree_hash(&file).map_err(|_| "Couldn't check for conflict markers")?;
        let size = repo
            .get_attr(
                path,
                "conflict-marker-size",
                git2::AttrCheckFlags::default(),
            )
            .map_err(|_| "Couldn't check for conflict markers")?
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|size| *size > 0);
        // The staging worker must apply exactly the bytes checked here, even if
        // the user decides to keep the markers.
        plan.expected_worktree = Some(hash);
        if conflicts::has_markers(&String::from_utf8_lossy(&bytes), size) {
            self.form = Some(Form {
                title: format!("Conflict markers remain in {}.", path.display()),
                action: FormAction::StageConflict {
                    plan: Box::new(plan),
                    path_top: std::cell::Cell::new(0),
                },
                fields: Vec::new(),
                selected: 1, // Cancel.
                focus_moved: false,
            });
            self.notice.clear();
            return Ok(());
        }
        self.apply_stage_file(plan)
    }

    pub(super) fn apply_stage_file(&mut self, plan: GitChangePlanRequest) -> Result<(), String> {
        self.mutation
            .start(GitMutationRequest::Plan(plan))
            .map_err(|error| error.to_string())?;
        self.busy = true;
        Ok(())
    }

    pub(super) fn clear_hunks(&mut self) {
        self.hunks.cancel();
        self.hunk_policy = None;
        self.hunk_snapshot = None;
        self.hunk_head = None;
        self.hunk_error = None;
    }

    pub(super) fn start_hunks(&mut self, row: &Row) {
        if self.last_list != 0 || self.files_view != 0 || row.directory || self.all_files {
            return;
        }
        let Some(status) = &self.snapshot else { return };
        let Some(entry) = status
            .entries
            .iter()
            .find(|entry| entry.relative_path == row.value)
        else {
            return;
        };
        if matches!(
            entry.status,
            GitFileStatus::Untracked | GitFileStatus::Conflicted
        ) {
            self.hunk_error =
                Some("Use s in the sidebar for untracked files or resolved conflicts".into());
            return;
        }
        if entry
            .original_path
            .as_ref()
            .is_some_and(|old| old != &row.value)
        {
            self.hunk_error =
                Some("Use s or u in the sidebar to stage or unstage both rename paths".into());
            return;
        }
        let policy = GitHunkPolicy {
            scope_root: self.root.clone(),
            repository_id: status.repository_id,
            status_revision: status.revision,
            root_revision: status.root_revision,
            file: row.value.clone(),
            layer: if self.preview_staged() {
                GitDiffLayer::Staged
            } else {
                GitDiffLayer::Unstaged
            },
            maximum_hunks: 10_000,
            maximum_lines: 100_000,
            maximum_bytes: 8 * 1024 * 1024,
            deadline: Duration::from_secs(5),
        };
        let head = match git_head_id(&self.root) {
            Ok(Some(id)) => GitHeadExpectation::At(id),
            Ok(None) => GitHeadExpectation::Unborn,
            Err(error) => {
                self.hunk_error = Some(error.to_string());
                return;
            }
        };
        match self.hunks.start(policy.clone()) {
            Ok(_) => {
                self.hunk_policy = Some(policy);
                self.hunk_head = Some(head);
            }
            Err(error) => self.hunk_error = Some(error.to_string()),
        }
    }

    fn diff_selection(&self) -> Result<(&GitHunkSnapshot, usize, usize), String> {
        if self.last_list != 0
            || self.files_view != 0
            || self.focus != 4
            || self.commit.is_some()
            || self.all_files
        {
            return Err("Select a working-tree or index diff".into());
        }
        if self.refreshing || self.jobs.preview_loading() {
            return Err(DIFF_REFRESHING.into());
        }
        if let Some(error) = &self.hunk_error {
            return Err(error.clone());
        }
        let snapshot = self
            .hunk_snapshot
            .as_ref()
            .ok_or("Hunks are loading; use s in the sidebar for a whole file")?;
        if self
            .selected_row()
            .is_none_or(|row| row.value != snapshot.file)
            || self
                .snapshot
                .as_ref()
                .is_none_or(|status| status.revision != snapshot.status_revision)
            || (snapshot.layer == GitDiffLayer::Staged) != self.preview_staged()
        {
            return Err("The diff selection changed; refresh before changing code".into());
        }
        let cursor = self
            .preview_cursor
            .ok_or("Move the cursor onto a diff line")?;
        let (hunk, line) = self
            .preview
            .captured_line(cursor, snapshot)
            .ok_or("The displayed hunk no longer matches Git; refresh before changing code")?;
        Ok((snapshot, hunk, line))
    }

    pub(super) fn start_line_range(&mut self) -> Result<(), String> {
        if self.preview_split {
            return Err("Use | for unified view before selecting an exact diff-line range".into());
        }
        if self.preview_anchor.take().is_some() {
            self.notice.clear();
            return Ok(());
        }
        let (snapshot, _, _) = self.diff_selection()?;
        let action = if snapshot.layer == GitDiffLayer::Staged {
            "unstage"
        } else {
            "stage"
        };
        self.preview_anchor = self.preview_cursor;
        self.notice =
            format!("Range started · j/k extend · a {action} selected diff lines · Esc cancel");
        Ok(())
    }

    /// Exact added/removed diff-line indices, never a partial cross-hunk range.
    fn selected_changed_diff_lines(
        &self,
        snapshot: &GitHunkSnapshot,
        hunk_index: usize,
        line_index: usize,
    ) -> Result<Vec<u32>, String> {
        let first = if let Some(anchor) = self.preview_anchor {
            let (anchor_hunk, anchor_line) = self
                .preview
                .captured_line(anchor, snapshot)
                .ok_or("The selected lines changed · Esc cancel")?;
            if anchor_hunk != hunk_index {
                return Err("Select a line range within one hunk; no changes were made".into());
            }
            anchor_line
        } else {
            line_index
        };
        let selected = (first.min(line_index)..=first.max(line_index))
            .filter(|&line| snapshot.hunks[hunk_index].lines[line].kind != GitDiffLineKind::Context)
            .map(|line| line as u32)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err("Select an added or removed line; context lines cannot be selected".into());
        }
        Ok(selected)
    }

    pub(super) fn stage_diff(&mut self, lines: bool) -> Result<(), String> {
        self.allow_write()?;
        let (snapshot, hunk_index, line_index) = self.diff_selection()?;
        let unstage = snapshot.layer == GitDiffLayer::Staged;
        match self.stage_intent {
            Some(true) if unstage => {
                return Err(
                    "This diff is staged · u unstages a hunk, ] shows the working tree".into(),
                );
            }
            Some(false) if !unstage => {
                return Err(
                    "This diff is not staged · ] shows the staged diff, where u unstages".into(),
                );
            }
            _ => {}
        }
        let action = if unstage { "unstage" } else { "stage" };
        if !lines && self.preview_anchor.is_some() {
            return Err(format!(
                "a {action}s the selected range; Esc cancels it before {} {action}s a whole hunk",
                if unstage { "u" } else { "s" }
            ));
        }
        let selected_lines = if lines {
            self.selected_changed_diff_lines(snapshot, hunk_index, line_index)?
        } else {
            Vec::new()
        };
        let hunk = snapshot.hunks[hunk_index].clone();
        let plan = GitChangePlanRequest {
            kind: match (unstage, lines) {
                (true, true) => GitMutationKind::UnstageLines,
                (true, false) => GitMutationKind::UnstageHunk,
                (false, true) => GitMutationKind::StageLines,
                (false, false) => GitMutationKind::StageHunk,
            },
            policy: self
                .hunk_policy
                .clone()
                .ok_or("Can't stage this hunk · r refresh")?,
            hunk_snapshot_revision: snapshot.revision,
            hunk_id: Some(hunk.id),
            selected: Some(hunk),
            selected_lines,
            expected_index: Some(snapshot.index_fingerprint),
            expected_worktree: snapshot.worktree_sha256,
            expected_head: self.hunk_head.ok_or("Can't stage this hunk · r refresh")?,
            expected_document_revision: None,
            unsaved_documents: Vec::new(),
            maximum_patch_bytes: 1024 * 1024,
        };
        self.mutation
            .start(GitMutationRequest::Plan(plan))
            .map_err(|error| error.to_string())?;
        self.busy = true;
        self.preview_anchor = None;
        self.notice.clear();
        Ok(())
    }

    pub(super) fn discard_diff(&mut self, lines: bool) -> Result<(), String> {
        self.allow_write()?;
        let (snapshot, hunk_index, line_index) = self.diff_selection()?;
        if snapshot.layer != GitDiffLayer::Unstaged {
            return Err("Discard requires an unstaged diff; the index is never discarded".into());
        }
        // An active preview range always wins over the whole-hunk binding. File
        // marks are deliberately not consulted: this is one captured code target.
        let exact = lines || self.preview_anchor.is_some();
        let selected_lines = if exact {
            self.selected_changed_diff_lines(snapshot, hunk_index, line_index)?
        } else {
            Vec::new()
        };
        let hunk = snapshot.hunks[hunk_index].clone();
        crate::mutation::validate_reset_selection(&hunk, &selected_lines)
            .map_err(|error| error.to_string())?;
        let scope = if exact {
            let first = selected_lines[0] + 1;
            let last = selected_lines[selected_lines.len() - 1] + 1;
            if first == last {
                let line = &hunk.lines[first as usize - 1];
                let source = match line.kind {
                    GitDiffLineKind::Add => format!("+{}", line.new_line.unwrap_or(0)),
                    GitDiffLineKind::Delete => format!("-{}", line.old_line.unwrap_or(0)),
                    GitDiffLineKind::Context => unreachable!("only changed lines are selected"),
                };
                format!("Hunk {} · diff line {first} ({source})", hunk_index + 1)
            } else {
                format!(
                    "Hunk {} · diff lines {first}–{last} ({} changes)",
                    hunk_index + 1,
                    selected_lines.len()
                )
            }
        } else {
            format!(
                "Hunk {}/{} · {}",
                hunk_index + 1,
                snapshot.hunks.len(),
                hunk.header.trim()
            )
        };
        let request = GitResetHunkRequest {
            policy: self
                .hunk_policy
                .clone()
                .ok_or("Can't stage this hunk · r refresh")?,
            hunk_snapshot_revision: snapshot.revision,
            hunk_id: hunk.id,
            selected: hunk,
            selected_lines,
            expected_index: Some(snapshot.index_fingerprint),
            expected_worktree: snapshot.worktree_sha256,
            maximum_patch_bytes: 1024 * 1024,
        };
        let fields = [
            ("File", request.policy.file.display().to_string()),
            ("Scope (index is kept)", scope),
        ]
        .into_iter()
        .map(|(label, value)| {
            let mut field = Field::new(label, value);
            field.read_only = true;
            field
        })
        .collect::<Vec<_>>();
        self.form = Some(Form {
            title: "Discard unstaged change?".into(),
            action: FormAction::Discard(Box::new(request)),
            selected: fields.len(),
            focus_moved: false,
            fields,
        });
        self.notice.clear();
        Ok(())
    }
}

/// What a path looked like when a file discard sheet opened. The sheet confirms exactly
/// this; any later change to the index entry or the bytes on disk refuses the discard.
#[derive(Clone, Debug, Eq, PartialEq)]
enum DiskIdentity {
    Missing,
    File { blob: git2::Oid, executable: bool },
    Symlink(PathBuf),
    Directory,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiscardTarget {
    path: PathBuf,
    /// `(blob, mode)` of the stage-0 index entry; `None` for an untracked path, which is
    /// deleted rather than restored.
    index: Option<IndexVersion>,
    disk: DiskIdentity,
}

/// A confirmed Changes-pane discard (`d`): the worktree side of each row is
/// restored from the index, and untracked files are deleted. The index is never touched.
#[derive(Clone, Debug)]
pub(super) struct FileDiscard {
    targets: Vec<DiscardTarget>,
    // A staged rename source need not be restored, but it must still be checked.
    rename_sources: Vec<(PathBuf, Option<IndexVersion>)>,
    /// Rows the sheet named, for the receipt.
    restored: Vec<PathBuf>,
    deleted: Vec<PathBuf>,
    /// Marks to clear once the discard is done.
    marked: bool,
}

const INTENT_TO_ADD: u16 = 1 << 13;

fn disk_identity(path: &Path) -> Result<DiskIdentity, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DiskIdentity::Missing);
        }
        Err(error) => {
            return Err(format!(
                "Couldn't read {}{}",
                path.display(),
                super::words::io_reason(&error)
            ));
        }
    };
    let kind = metadata.file_type();
    Ok(if kind.is_symlink() {
        DiskIdentity::Symlink(std::fs::read_link(path).map_err(|error| error.to_string())?)
    } else if kind.is_file() {
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        DiskIdentity::File {
            blob: git2::Oid::hash_file(git2::ObjectType::Blob, path)
                .map_err(|error| error.message().to_owned())?,
            executable,
        }
    } else if kind.is_dir() {
        DiskIdentity::Directory
    } else {
        DiskIdentity::Other
    })
}

fn discard_conflict(path: &Path) -> String {
    format!(
        "Resolve the conflict in {} before discarding.",
        path.display()
    )
}

fn check_discard_conflict(index: &git2::Index, path: &Path) -> Result<(), String> {
    if (1..=3).any(|stage| index.get_path(path, stage).is_some()) {
        return Err(discard_conflict(path));
    }
    Ok(())
}

fn discard_changed(path: &Path) -> String {
    format!("{} changed. Refresh and try again.", path.display())
}

/// The identity of one path, refusing what a file discard must never touch.
fn discard_target(root: &Path, index: &git2::Index, path: &Path) -> Result<DiscardTarget, String> {
    if !path
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("{} is not a repository path", path.display()));
    }
    check_discard_conflict(index, path)?;
    let entry = index.get_path(path, 0);
    if let Some(entry) = &entry {
        if entry.mode == 0o160000 {
            return Err(format!(
                "{} is a submodule; discard its changes inside the submodule",
                path.display()
            ));
        }
        if entry.flags_extended & INTENT_TO_ADD != 0 {
            return Err(format!(
                "{} is marked intent-to-add; stage or unstage it before discarding",
                path.display()
            ));
        }
    }
    let disk = disk_identity(&root.join(path))?;
    let refusal = match disk {
        DiskIdentity::Directory => Some("is now a directory; review its files before discarding"),
        DiskIdentity::Other => Some("has an unsupported path type"),
        DiskIdentity::Missing if entry.is_none() => Some("no longer exists; refresh with r"),
        _ => None,
    };
    if let Some(refusal) = refusal {
        return Err(format!("{} {refusal}", path.display()));
    }
    Ok(DiscardTarget {
        path: path.to_path_buf(),
        index: entry.map(|entry| (entry.id, entry.mode)),
        disk,
    })
}

/// Recheck every captured identity, then restore tracked paths from the index and delete
/// untracked ones. Nothing is written unless every path still matches the sheet.
fn apply_file_discard(root: &Path, plan: &FileDiscard) -> Result<(), String> {
    let repository = git2::Repository::open(root).map_err(|error| error.message().to_owned())?;
    let mut index = repository
        .index()
        .map_err(|error| error.message().to_owned())?;
    // A competing Git command must not change the index while we hash the
    // workfiles or after validation, before a reviewed untracked file is deleted.
    let _lock =
        IndexLock::acquire(index.path().ok_or(DISCARD_LOCKED)?).map_err(|_| DISCARD_LOCKED)?;
    index
        .read(true)
        .map_err(|error| error.message().to_owned())?;
    for (path, version) in &plan.rename_sources {
        check_discard_conflict(&index, path)?;
        if stage_zero(&index, path) != *version {
            return Err(discard_changed(path));
        }
    }
    for target in &plan.targets {
        let now = discard_target(root, &index, &target.path)?;
        if &now != target {
            return Err(discard_changed(&target.path));
        }
    }
    let restore = plan
        .targets
        .iter()
        .filter(|target| target.index.is_some())
        .collect::<Vec<_>>();
    if !restore.is_empty() {
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout
            .force()
            .update_index(false)
            .disable_pathspec_match(true);
        for target in &restore {
            checkout.path(target.path.as_path());
        }
        repository
            .checkout_index(Some(&mut index), Some(&mut checkout))
            .map_err(|error| {
                format!(
                    "Couldn't discard changes{}",
                    super::words::git_reason(error.message())
                )
            })?;
    }
    for target in plan.targets.iter().filter(|target| target.index.is_none()) {
        std::fs::remove_file(root.join(&target.path)).map_err(|error| {
            format!(
                "Couldn't delete {}{}",
                target.path.display(),
                super::words::io_reason(&error)
            )
        })?;
    }
    Ok(())
}

impl GitScreen {
    /// `d` in Changes: the marked set if any, else the selected row. `D` is the marked set.
    pub(super) fn discard_files_form(&mut self, marked_only: bool) -> Result<(), String> {
        self.allow_write()?;
        if self.all_files {
            return Err("F returns to changed files before discarding".into());
        }
        let marked = !self.file_selection.marked.is_empty();
        let selection = if marked {
            self.finish_file_range();
            self.file_selection.marked.clone()
        } else if marked_only {
            return Err("No marked files · x marks files, then D discards them".into());
        } else {
            let row = self
                .selected_row()
                .filter(|_| self.last_list == 0)
                .ok_or("Select a changed file")?;
            if row.directory {
                return Err(
                    "Select a file, or mark files with x; a directory is not discarded whole"
                        .into(),
                );
            }
            BTreeSet::from([row.value])
        };
        let snapshot = self.snapshot.as_ref().ok_or("Status is loading")?;
        let entries = snapshot
            .entries
            .iter()
            .filter(|entry| selection.contains(&entry.relative_path))
            .collect::<Vec<_>>();
        if entries.is_empty() || entries.len() != selection.len() {
            return Err(if marked {
                "The marked files changed; refresh and select them again".into()
            } else {
                "Select a changed file".into()
            });
        }
        let repository = git2::Repository::open(&self.root).map_err(|error| error.to_string())?;
        let index = repository.index().map_err(|error| error.to_string())?;
        let mut targets = Vec::new();
        let mut rename_sources = Vec::new();
        let (mut restored, mut deleted, mut kept) = (Vec::new(), Vec::new(), Vec::new());
        let mut rows = Vec::new();
        for entry in entries {
            let path = &entry.relative_path;
            if entry.status == GitFileStatus::Conflicted {
                return Err(discard_conflict(path));
            }
            let original = entry
                .original_path
                .as_ref()
                .filter(|source| *source != path);
            if let Some(original) = original {
                check_discard_conflict(&index, original)?;
            }
            if !entry.sources.unstaged {
                check_discard_conflict(&index, path)?;
                kept.push(path.clone());
                continue;
            }
            let target = discard_target(&self.root, &index, path)?;
            let original = original.map(|path| (path, stage_zero(&index, path)));
            if let Some((source, version)) = original {
                rename_sources.push((source.clone(), version));
            }
            let delete = entry.unstaged_status == Some(GitFileStatus::Untracked);
            if delete != target.index.is_none() {
                return Err(discard_changed(path));
            }
            if delete {
                // Not in the index (untracked, or re-created after a staged delete): the
                // worktree change IS the file, so discarding it deletes it.
                targets.push(target);
                deleted.push(path.clone());
                rows.push(("Delete", format!("{} · untracked", path.display())));
                continue;
            }
            targets.push(target);
            let code = entry.change_code();
            // An unstaged rename's source is still in the index: restore it too, so the
            // row's whole worktree change is undone. A staged rename's source is not.
            let mut label = format!("{} · {}", path.display(), code.trim());
            if let Some((original, version)) = original {
                if version.is_some() {
                    targets.push(discard_target(&self.root, &index, original)?);
                }
                label = format!("{label} · renamed from {}", original.display());
            }
            if entry.sources.staged {
                label.push_str(" · staged part kept");
            }
            restored.push(path.clone());
            rows.push(("Restore", label));
        }
        if targets.is_empty() {
            return Err(match kept.as_slice() {
                [path] => format!(
                    "{} has only staged changes; nothing in the worktree to discard (u unstages)",
                    path.display()
                ),
                _ => {
                    "The marked files have only staged changes; nothing in the worktree to discard"
                        .into()
                }
            });
        }
        let files = restored.len() + deleted.len();
        let title = match (restored.as_slice(), deleted.as_slice()) {
            ([path], []) => format!("Discard changes in {}?", path.display()),
            ([], [path]) => format!("Delete untracked file {}?", path.display()),
            _ => format!("Discard changes in {files} files?"),
        };
        let mut fields = Vec::new();
        if files == 1 {
            let (_, row) = rows.remove(0);
            fields.push(("Target", row));
        } else {
            fields.push(("Target", format!("{files} files")));
            const SHOWN: usize = 8;
            let more = rows.len().saturating_sub(SHOWN);
            fields.extend(rows.into_iter().take(SHOWN));
            if more > 0 {
                fields.push(("More", format!("{more} more file(s)")));
            }
        }
        if !restored.is_empty() {
            fields.push(("Scope", "Worktree only; the index keeps its copy.".into()));
        }
        if !deleted.is_empty() {
            fields.push(("Untracked", "Deleted from disk; Git keeps no copy.".into()));
        }
        if !kept.is_empty() {
            fields.push((
                "Kept",
                format!("{} staged-only file(s) have nothing to discard", kept.len()),
            ));
        }
        let fields = fields
            .into_iter()
            .map(|(label, value)| {
                let mut field = Field::new(label, value);
                field.read_only = true;
                field.input.cursor = 0;
                field
            })
            .collect::<Vec<_>>();
        self.form = Some(Form {
            title,
            action: FormAction::DiscardFiles(Box::new(FileDiscard {
                targets,
                rename_sources,
                restored,
                deleted,
                marked,
            })),
            // The cursor rests on the primary Discard button.
            selected: fields.len(),
            focus_moved: false,
            fields,
        });
        self.notice.clear();
        Ok(())
    }

    /// Submit a confirmed file discard: its captured targets, never the current rows.
    pub(super) fn discard_files(&mut self, plan: &FileDiscard) -> Result<(), String> {
        apply_file_discard(&self.root, plan)?;
        let receipt = match (plan.restored.as_slice(), plan.deleted.as_slice()) {
            ([path], []) => format!("Discarded worktree changes in {}", path.display()),
            ([], [path]) => format!("Deleted untracked file {}", path.display()),
            (restored, deleted) => {
                let mut text = format!(
                    "Discarded changes in {} files",
                    restored.len() + deleted.len()
                );
                if !deleted.is_empty() {
                    text.push_str(&format!(" · {} untracked deleted", deleted.len()));
                }
                text
            }
        };
        if plan.marked {
            self.clear_file_selection();
        }
        self.preview_anchor = None;
        self.receipt = Some((ReceiptTone::Ok, receipt));
        // Like a Git write: no further write until the refreshed status lands.
        let result = self.refresh();
        self.refreshing_after_write = result.is_ok();
        result
    }
}
