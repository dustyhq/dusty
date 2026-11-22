//! Branch, worktree and submodule listing, plus worktree creation and removal.
//! Check refusal conditions before writes;
//! retain running mutations until their completion is handled.
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitBranchRequestId(pub u64);
#[derive(Clone, Debug)]
pub struct GitBranchListRequest {
    pub repository: PathBuf,
    pub deadline: Duration,
}
#[derive(Clone, Debug)]
pub enum GitBranchRequest {
    List(GitBranchListRequest),
    CreateWorktree {
        repository: PathBuf,
        name: String,
        path: PathBuf,
        /// Branch or ref to check out in the new worktree.
        reference: String,
        deadline: Duration,
    },
    RemoveWorktree {
        repository: PathBuf,
        name: String,
        deadline: Duration,
    },
    /// Check out a REVISION rather than a branch. A review must not create or move a ref, so its
    /// worktree is DETACHED at the revision that was inspected.
    CreateDetachedWorktree {
        repository: PathBuf,
        name: String,
        path: PathBuf,
        revision: String,
        deadline: Duration,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitBranchEntry {
    pub name: String,
    pub remote: bool,
    pub upstream: Option<String>,
    /// Commits the upstream does not have. Only the checked-out branch pays for it.
    pub ahead: u32,
    /// Commits the working branch does not have.
    pub behind: u32,
    /// The worktree that has this branch checked out, in its own path terms.
    pub checked_out: Option<PathBuf>,
    pub head: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitWorktreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub dirty: bool,
    /// A Git operation in progress in that worktree, named.
    pub operation: Option<String>,
    pub locked: bool,
    pub main: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitSubmoduleEntry {
    pub name: String,
    /// Relative to the repository workdir, not its Git directory.
    pub path: PathBuf,
    pub initialized: bool,
    /// Uncommitted files, independently of a HEAD/index gitlink mismatch.
    pub dirty: bool,
    /// The checked-out submodule's actual HEAD, not the superproject's HEAD gitlink.
    pub head: Option<String>,
    /// The expected gitlink in the superproject index.
    pub index: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitBranchList {
    pub request_id: GitBranchRequestId,
    pub branches: Vec<GitBranchEntry>,
    pub worktrees: Vec<GitWorktreeEntry>,
    pub submodules: Result<Vec<GitSubmoduleEntry>, String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitBranchResult {
    List(GitBranchList),
    Changed,
}
#[derive(Debug, Error, Eq, PartialEq)]
pub enum GitBranchError {
    #[error("invalid Git branch request")]
    Invalid,
    #[error("not a Git repository")]
    NotRepository,
    #[error("branch `{0}` does not exist")]
    UnknownBranch(String),
    #[error("worktree `{0}` does not exist")]
    UnknownWorktree(String),
    #[error("a worktree named `{0}` already exists")]
    WorktreeExists(String),
    #[error("branch `{0}` is checked out in another worktree")]
    RefusedCheckedOut(String),
    #[error("worktree `{0}` has uncommitted changes")]
    RefusedDirtyWorktree(String),
    #[error("worktree `{0}` is locked")]
    RefusedLockedWorktree(String),
    #[error("worktree `{0}` has a Git operation in progress: {1}")]
    RefusedActiveOperation(String, String),
    #[error("destination `{0}` is not empty")]
    RefusedDestinationNotEmpty(String),
    #[error("branch `{0}` is already checked out in another worktree")]
    RefusedBranchAlreadyCheckedOut(String),
    #[error("Git branch deadline exceeded")]
    Deadline,
    #[error("Git branch request was cancelled")]
    Cancelled,
    #[error("a worktree mutation is still running; wait for it to finish")]
    Busy,
    #[error("Git branch worker stopped")]
    Stopped,
    #[error("Git failed: {0}")]
    Backend(String),
}
pub(super) enum GitBranchEvent {
    Complete(Result<GitBranchResult, GitBranchError>),
}
#[derive(Default)]
pub struct GitBranchWorker {
    pub(super) next: u64,
    active: Option<GitBranchRequestId>,
    cancel: Option<Arc<AtomicBool>>,
    receiver: Option<Receiver<GitBranchEvent>>,
    accepted: Option<GitBranchResult>,
    mutating: bool,
}
impl GitBranchWorker {
    pub fn start(
        &mut self,
        request: GitBranchRequest,
    ) -> Result<GitBranchRequestId, GitBranchError> {
        if !valid_request(&request) {
            return Err(GitBranchError::Invalid);
        }
        if self.mutating && self.receiver.is_some() {
            return Err(GitBranchError::Busy);
        }
        self.cancel();
        let mutating = !matches!(request, GitBranchRequest::List(_));
        self.next = self.next.saturating_add(1);
        let request_id = GitBranchRequestId(self.next);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&cancel);
        let (tx, rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(format!("dusty-git-branch-{}", request_id.0))
            .spawn(move || {
                let result = run(request_id, &request, &worker);
                let _ = tx.send(GitBranchEvent::Complete(result));
            })
            .map_err(|_| GitBranchError::Stopped)?;
        self.mutating = mutating;
        self.active = Some(request_id);
        self.cancel = Some(cancel);
        self.receiver = Some(rx);
        Ok(request_id)
    }
    pub fn cancel(&mut self) {
        if self.removal_check.is_some() {
            self.cancel_removal_check();
            self.cancel = None;
        } else if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        // A mutation may already be writing. Keep its completion owned until poll handles it.
        if !self.mutating {
            self.active = None;
            self.receiver = None;
            self.removal_check = None;
        }
    }
    pub fn poll(&mut self) -> Result<Option<&GitBranchResult>, GitBranchError> {
        let event = match self.receiver.as_ref() {
            Some(receiver) => receiver.try_recv(),
            None => return Ok(None),
        };
        match event {
            Ok(GitBranchEvent::Complete(result)) => {
                let expected = self.active.take();
                self.receiver = None;
                self.cancel = None;
                self.mutating = false;
                let result = result?;
                // A page carries its request_id; a mutation is accepted for the active request.
                if let GitBranchResult::List(list) = &result
                    && Some(list.request_id) != expected
                {
                    return Ok(None);
                }
                self.accepted = Some(result);
                Ok(self.accepted.as_ref())
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.mutating = false;
                self.cancel();
                Err(GitBranchError::Stopped)
            }
        }
    }
}
impl Drop for GitBranchWorker {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn valid_request(request: &GitBranchRequest) -> bool {
    let (repository, deadline) = match request {
        GitBranchRequest::List(request) => (&request.repository, request.deadline),
        GitBranchRequest::CreateWorktree {
            repository,
            name,
            path,
            reference,
            deadline,
        } => {
            if name.trim().is_empty() || path.as_os_str().is_empty() || reference.trim().is_empty()
            {
                return false;
            }
            (repository, *deadline)
        }
        GitBranchRequest::RemoveWorktree {
            repository,
            name,
            deadline,
        } => {
            if name.trim().is_empty() {
                return false;
            }
            (repository, *deadline)
        }
        GitBranchRequest::CreateDetachedWorktree {
            repository,
            name,
            path,
            revision,
            deadline,
        } => {
            if name.trim().is_empty() || path.as_os_str().is_empty() || revision.trim().is_empty() {
                return false;
            }
            (repository, *deadline)
        }
    };
    !repository.as_os_str().is_empty() && !deadline.is_zero() && deadline <= Duration::from_secs(30)
}

fn open(repository: &Path) -> Result<git2::Repository, GitBranchError> {
    git2::Repository::open(repository).map_err(|_| GitBranchError::NotRepository)
}

fn check_request(cancel: &AtomicBool, expires: Instant) -> Result<(), GitBranchError> {
    if cancel.load(Ordering::Acquire) {
        Err(GitBranchError::Cancelled)
    } else if Instant::now() >= expires {
        Err(GitBranchError::Deadline)
    } else {
        Ok(())
    }
}

fn main_repository(repository: &git2::Repository) -> Result<git2::Repository, GitBranchError> {
    if !repository.is_worktree() {
        return open(repository.workdir().ok_or(GitBranchError::NotRepository)?);
    }
    let main = open(repository.commondir())?;
    // libgit2 honors core.worktree; otherwise it infers a conventional .git directory.
    // A separate Git directory without core.worktree has no backlink to its primary checkout.
    let path = main.workdir().ok_or(GitBranchError::NotRepository)?;
    if !path.join(".git").exists() {
        return Err(GitBranchError::Backend(
            "cannot locate the primary checkout; open it directly or set core.worktree".into(),
        ));
    }
    let checked = open(path)?;
    if checked.is_worktree()
        || crate::files::canonicalize(checked.commondir()).map_err(backend)?
            != crate::files::canonicalize(repository.commondir()).map_err(backend)?
    {
        return Err(GitBranchError::NotRepository);
    }
    Ok(main)
}

fn checkout_options(cancel: &AtomicBool, expires: Instant) -> git2::build::CheckoutBuilder<'_> {
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.notify_on(git2::CheckoutNotificationType::all());
    checkout.notify(move |_, _, _, _, _| check_request(cancel, expires).is_ok());
    checkout
}

/// One linked worktree as read for attribution: name, path, branch and lock state.
struct WorktreeRef {
    name: String,
    path: PathBuf,
    branch: Option<String>,
    locked: bool,
}

/// The linked worktrees and the branch each has checked out.
fn worktree_branches(
    repository: &git2::Repository,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<Vec<WorktreeRef>, GitBranchError> {
    check_request(cancel, expires)?;
    let names = repository.worktrees().map_err(backend)?;
    let mut found = Vec::new();
    for index in 0..names.len() {
        check_request(cancel, expires)?;
        let Some(name) = names.get(index) else {
            continue;
        };
        let Ok(worktree) = repository.find_worktree(name) else {
            continue;
        };
        let path = worktree.path().to_path_buf();
        let branch = git2::Repository::open(&path).ok().and_then(|opened| {
            opened
                .head()
                .ok()
                .filter(|head| head.is_branch())
                .and_then(|head| head.shorthand().map(str::to_owned))
        });
        let locked = worktree
            .is_locked()
            .is_ok_and(|status| !matches!(status, git2::WorktreeLockStatus::Unlocked));
        found.push(WorktreeRef {
            name: name.to_owned(),
            path,
            branch,
            locked,
        });
    }
    check_request(cancel, expires)?;
    Ok(found)
}

/// Git's own reason, without libgit2's class and code suffix.
fn backend(error: impl std::fmt::Display) -> GitBranchError {
    let message = error.to_string();
    let reason = message.split("; class=").next().unwrap_or(&message);
    GitBranchError::Backend(reason.chars().take(512).collect())
}

fn operation_name(state: git2::RepositoryState) -> Option<String> {
    match state {
        git2::RepositoryState::Clean => None,
        git2::RepositoryState::Merge => Some("merge".into()),
        git2::RepositoryState::Revert | git2::RepositoryState::RevertSequence => {
            Some("revert".into())
        }
        git2::RepositoryState::CherryPick | git2::RepositoryState::CherryPickSequence => {
            Some("cherry-pick".into())
        }
        git2::RepositoryState::Bisect => Some("bisect".into()),
        git2::RepositoryState::Rebase
        | git2::RepositoryState::RebaseInteractive
        | git2::RepositoryState::RebaseMerge => Some("rebase".into()),
        git2::RepositoryState::ApplyMailbox | git2::RepositoryState::ApplyMailboxOrRebase => {
            Some("apply-mailbox".into())
        }
    }
}

fn read_list(
    request_id: GitBranchRequestId,
    request: &GitBranchListRequest,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<GitBranchResult, GitBranchError> {
    check_request(cancel, expires)?;
    let repository = open(&request.repository)?;
    let main = main_repository(&repository)?;
    let (requested_dirty, submodules) = list_worktree_state(&repository, cancel, expires)?;
    let main_dirty = if repository.is_worktree() {
        list_worktree_state(&main, cancel, expires)?.0
    } else {
        requested_dirty
    };
    let main_path = main
        .workdir()
        .and_then(|path| crate::files::canonicalize(path).ok());
    let mut worktrees = vec![GitWorktreeEntry {
        name: "main".into(),
        path: main_path.clone().unwrap_or_default(),
        branch: main
            .head()
            .ok()
            .filter(|head| head.is_branch())
            .and_then(|head| head.shorthand().map(str::to_owned)),
        dirty: main_dirty,
        operation: operation_name(main.state()),
        locked: false,
        main: true,
    }];
    for linked in worktree_branches(&repository, cancel, expires)? {
        check_request(cancel, expires)?;
        let opened = git2::Repository::open(&linked.path).ok();
        let (dirty, operation) = match &opened {
            Some(opened) => (
                if opened.path() == repository.path() {
                    requested_dirty
                } else {
                    list_worktree_state(opened, cancel, expires)?.0
                },
                operation_name(opened.state()),
            ),
            None => (false, None),
        };
        worktrees.push(GitWorktreeEntry {
            name: linked.name,
            path: linked.path,
            branch: linked.branch,
            dirty,
            operation,
            locked: linked.locked,
            main: false,
        });
    }
    let mut branches = Vec::new();
    for kind in [git2::BranchType::Local, git2::BranchType::Remote] {
        let iterator = repository.branches(Some(kind)).map_err(backend)?;
        for entry in iterator {
            check_request(cancel, expires)?;
            let (branch, _) = entry.map_err(backend)?;
            let Some(name) = branch.name().ok().flatten().map(str::to_owned) else {
                continue;
            };
            let upstream = branch
                .upstream()
                .ok()
                .and_then(|upstream| upstream.name().ok().flatten().map(str::to_owned));
            let checked_out = worktrees
                .iter()
                .find(|worktree| worktree.branch.as_deref() == Some(name.as_str()))
                .map(|worktree| worktree.path.clone());
            // Divergence belongs to the branch a person is standing on; reading it
            // for every reference would turn one list into an unbounded walk.
            let (ahead, behind) = if kind == git2::BranchType::Local && branch.is_head() {
                branch
                    .upstream()
                    .ok()
                    .and_then(|upstream| {
                        let local = branch.get().target()?;
                        let remote = upstream.get().target()?;
                        repository.graph_ahead_behind(local, remote).ok()
                    })
                    .map_or((0, 0), |(ahead, behind)| {
                        (
                            u32::try_from(ahead).unwrap_or(u32::MAX),
                            u32::try_from(behind).unwrap_or(u32::MAX),
                        )
                    })
            } else {
                (0, 0)
            };
            branches.push(GitBranchEntry {
                name,
                remote: kind == git2::BranchType::Remote,
                upstream,
                ahead,
                behind,
                checked_out,
                head: !branch.is_head().eq(&false) && branch.is_head(),
            });
        }
    }
    check_request(cancel, expires)?;
    Ok(GitBranchResult::List(GitBranchList {
        request_id,
        branches,
        worktrees,
        submodules,
    }))
}

/// Only relative, portable paths may reach libgit2's submodule lookup: lookup itself reads
/// working-directory metadata, so checking the returned Submodule would already be too late.
fn safe_submodule_relative_path(path: &Path) -> bool {
    use std::path::Component;
    let text = path.to_string_lossy();
    if text.contains('\\') || text.starts_with('-') {
        return false;
    }
    let mut normal = false;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.contains(':') || name.eq_ignore_ascii_case(".git") {
                    return false;
                }
                normal = true;
            }
            Component::CurDir => {}
            _ => return false,
        }
    }
    normal
}

/// Check the nearest existing ancestor as well, so an uninitialized path below an escaping
/// symlink cannot pass merely because its final directory does not exist yet.
fn contained_submodule_path(root: &Path, relative: &Path) -> bool {
    let mut candidate = root.join(relative);
    loop {
        match crate::files::canonicalize(&candidate) {
            Ok(resolved) => return resolved.starts_with(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&candidate) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    // In particular, do not accept dangling symlinks.
                    _ => return false,
                }
                if !candidate.pop() || !candidate.starts_with(root) {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// The bool says whether Git's ordinary recursive status scan is safe to use. Use libgit2's
/// config parser before find_submodule, rather than submodules(), which probes every path
/// before returning the entries to its caller.
fn read_submodules(
    repository: &git2::Repository,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<(Vec<GitSubmoduleEntry>, bool), GitBranchError> {
    check_request(cancel, expires)?;
    let root = repository.workdir().ok_or(GitBranchError::NotRepository)?;
    let root = crate::files::canonicalize(root).map_err(backend)?;
    let modules = root.join(".gitmodules");
    match std::fs::symlink_metadata(&modules) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), true));
        }
        Err(error) => return Err(backend(error)),
        Ok(_) => {}
    }
    if !contained_submodule_path(&root, Path::new(".gitmodules")) {
        return Err(backend("unsafe .gitmodules path"));
    }
    let config = git2::Config::open(&modules).map_err(backend)?;
    let mut entries = config.entries(Some("^submodule\\.")).map_err(backend)?;
    let mut configured = std::collections::BTreeMap::new();
    let mut safe_for_status = true;
    while let Some(entry) = entries.next() {
        check_request(cancel, expires)?;
        let entry = entry.map_err(backend)?;
        let (name, property) = entry
            .name()
            .and_then(|key| key.strip_prefix("submodule."))
            .and_then(|key| key.rsplit_once('.'))
            .ok_or_else(|| backend("invalid submodule name"))?;
        if !safe_submodule_relative_path(Path::new(name)) {
            safe_for_status = false;
            continue;
        }
        let path = configured.entry(name.to_owned()).or_insert(None);
        if property == "path" {
            // ConfigEntry::value() panics for a valueless (boolean) config entry.
            if !entry.has_value() {
                return Err(backend("submodule path has no value"));
            }
            *path = Some(PathBuf::from(
                entry
                    .value()
                    .ok_or_else(|| backend("invalid submodule path"))?,
            ));
        }
    }
    let gitdir = crate::files::canonicalize(repository.commondir()).map_err(backend)?;
    let mut found = Vec::new();
    for (name, path) in configured {
        check_request(cancel, expires)?;
        let path = path.ok_or_else(|| backend(format!("submodule `{name}` has no path")))?;
        if !safe_submodule_relative_path(&path)
            || !contained_submodule_path(&root, &path)
            || !contained_submodule_path(&gitdir, &Path::new("modules").join(&name))
            || !contained_submodule_path(&root, &path.join(".git"))
        {
            safe_for_status = false;
            continue;
        }
        let submodule = repository.find_submodule(&name).map_err(backend)?;
        let opened = match submodule.open() {
            Ok(opened) => Some(opened),
            Err(error) if error.code() == git2::ErrorCode::NotFound => None,
            Err(error) => return Err(backend(error)),
        };
        let head = opened
            .as_ref()
            .and_then(|opened| opened.head().ok().and_then(|head| head.target()))
            .map(|oid| oid.to_string());
        // Reuse the existing deadline-killed status job instead of an uncancellable libgit2
        // status scan. Nested submodules are inventoried when their parent is opened.
        let dirty = match &opened {
            Some(opened) => has_changes_with_options(opened, cancel, expires, true)?,
            None => false,
        };
        found.push(GitSubmoduleEntry {
            name,
            path,
            initialized: opened.is_some(),
            dirty,
            head,
            index: submodule.index_id().map(|oid| oid.to_string()),
        });
    }
    check_request(cancel, expires)?;
    Ok((found, safe_for_status))
}

fn list_worktree_state(
    repository: &git2::Repository,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<(bool, Result<Vec<GitSubmoduleEntry>, String>), GitBranchError> {
    let inventory = read_submodules(repository, cancel, expires);
    // Cancellation/deadlines are request errors, never a partial submodule error.
    check_request(cancel, expires)?;
    let safe_for_status = inventory.as_ref().is_ok_and(|(_, safe)| *safe);
    let submodules = inventory
        .map(|(entries, _)| entries)
        .map_err(|error| error.to_string());
    // Malformed or unsafe configuration must neither erase the branch/worktree page nor
    // send Git's recursive status scan through paths we have deliberately rejected.
    let dirty = has_changes_with_options(repository, cancel, expires, !safe_for_status)?
        || (!safe_for_status
            && submodules.as_ref().is_ok_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.dirty || (entry.initialized && entry.head != entry.index))
            }));
    Ok((dirty, submodules))
}

fn has_changes(
    repository: &git2::Repository,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<bool, GitBranchError> {
    has_changes_with_options(repository, cancel, expires, false)
}

fn has_changes_with_options(
    repository: &git2::Repository,
    cancel: &AtomicBool,
    expires: Instant,
    ignore_submodules: bool,
) -> Result<bool, GitBranchError> {
    check_request(cancel, expires)?;
    let workdir = repository.workdir().ok_or(GitBranchError::NotRepository)?;
    // libgit2's status scan can't be stopped, so `git status` runs as a job the deadline kills.
    // Its output is capped at a few lines: any change at all is enough to answer.
    let started = Instant::now();
    let policy = crate::status::GitStatusPolicy {
        root: workdir.to_path_buf(),
        root_revision: 0,
        maximum_files: 1,
        maximum_path_bytes: 512,
        deadline: expires.saturating_duration_since(started),
    };
    let mut args = vec!["status", "--porcelain", "--untracked-files=normal"];
    if ignore_submodules {
        args.push("--ignore-submodules=all");
    }
    let (output, truncated) = crate::status::status_command(
        workdir, &args, &policy, started, cancel,
    )
    .map_err(|error| match error {
        crate::status::GitStatusError::Deadline => GitBranchError::Deadline,
        crate::status::GitStatusError::Cancelled => GitBranchError::Cancelled,
        error => backend(error),
    })?;
    check_request(cancel, expires)?;
    Ok(truncated || !output.is_empty())
}

/// A worktree inside the main checkout would otherwise show as untracked in its status.
fn exclude_from_status(
    repository: &git2::Repository,
    path: &Path,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<(), GitBranchError> {
    use std::io::Read;
    use std::io::Write;
    let root = repository.workdir().ok_or(GitBranchError::NotRepository)?;
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(());
    };
    let relative = relative.to_string_lossy();
    if relative.is_empty() || relative.contains(['\n', '\r']) {
        return Ok(());
    }
    let mut line = String::from("/");
    for ch in relative.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']' | ' ' | '#' | '!') {
            line.push('\\');
        }
        line.push(ch);
    }
    line.push('/');
    let exclude = repository.commondir().join("info").join("exclude");
    check_request(cancel, expires)?;
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).map_err(backend)?;
    }
    check_request(cancel, expires)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&exclude)
        .map_err(backend)?;
    let mut file =
        crate::files::lock_exclusive(file, false).map_err(|(_, error)| backend(error))?;
    check_request(cancel, expires)?;
    let mut current = Vec::new();
    file.read_to_end(&mut current).map_err(backend)?;
    if current
        .split(|byte| *byte == b'\n')
        .any(|existing| existing == line.as_bytes())
    {
        return Ok(());
    }
    let addition = format!(
        "{}{line}\n",
        if current.is_empty() || current.ends_with(b"\n") {
            ""
        } else {
            "\n"
        }
    );
    check_request(cancel, expires)?;
    file.write_all(addition.as_bytes()).map_err(backend)
}

fn checked_out_path(
    repository: &git2::Repository,
    name: &str,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<Option<PathBuf>, GitBranchError> {
    // The primary checkout's HEAD lives in the common directory.
    if let Ok(main) = open(repository.commondir())
        && let Ok(head) = main.head()
        && head.is_branch()
        && head.shorthand() == Some(name)
    {
        return Ok(Some(main.workdir().unwrap_or(main.path()).to_path_buf()));
    }
    Ok(worktree_branches(repository, cancel, expires)?
        .into_iter()
        .find(|linked| linked.branch.as_deref() == Some(name))
        .map(|linked| linked.path))
}

/// Where a new worktree's HEAD points.
enum WorktreeHead<'a> {
    Branch(&'a str),
    Detached(git2::Oid),
}

/// Add worktree `name` at `path` with `head` checked out. libgit2 checks a new worktree out with
/// no way to stop it, so the worktree starts on an empty commit and its files come from a
/// checkout the deadline can interrupt. A failed or interrupted checkout removes the worktree.
fn add_worktree(
    repository: &git2::Repository,
    name: &str,
    path: &Path,
    head: WorktreeHead<'_>,
    cancel: &AtomicBool,
    expires: Instant,
) -> Result<(), GitBranchError> {
    let empty = repository
        .treebuilder(None)
        .and_then(|builder| builder.write())
        .and_then(|tree| repository.find_tree(tree))
        .map_err(backend)?;
    let signature = git2::Signature::now("Dusty", "dusty@localhost").map_err(backend)?;
    let start = repository
        .commit(None, &signature, &signature, "worktree start", &empty, &[])
        .map_err(backend)?;
    let temporary = format!("refs/heads/dusty-worktree/{name}");
    let starting = repository
        .reference(&temporary, start, true, "worktree start")
        .map_err(backend)?;
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&starting));
    let added = check_request(cancel, expires).and_then(|()| {
        repository
            .worktree(name, path, Some(&options))
            .map_err(backend)
    });
    let delete_temporary = || {
        if let Ok(mut reference) = repository.find_reference(&temporary) {
            let _ = reference.delete();
        }
    };
    let worktree = match added {
        Ok(worktree) => worktree,
        Err(error) => {
            drop(starting);
            delete_temporary();
            return Err(error);
        }
    };
    drop(starting);
    let checked_out = git2::Repository::open(path)
        .map_err(backend)
        .and_then(|linked| {
            match head {
                WorktreeHead::Branch(reference) => linked.set_head(reference),
                WorktreeHead::Detached(oid) => linked.set_head_detached(oid),
            }
            .map_err(backend)?;
            let mut checkout = checkout_options(cancel, expires);
            checkout.force();
            linked.checkout_head(Some(&mut checkout)).map_err(backend)
        })
        .and_then(|()| check_request(cancel, expires));
    delete_temporary();
    if let Err(error) = checked_out {
        let _ = std::fs::remove_dir_all(path);
        let mut prune = git2::WorktreePruneOptions::new();
        prune.valid(true).working_tree(true);
        let _ = worktree.prune(Some(&mut prune));
        return Err(check_request(cancel, expires).err().unwrap_or(error));
    }
    Ok(())
}

#[cfg(test)]
fn run(
    request_id: GitBranchRequestId,
    request: &GitBranchRequest,
    cancel: &AtomicBool,
) -> Result<GitBranchResult, GitBranchError> {
    let deadline = match request {
        GitBranchRequest::List(request) => request.deadline,
        GitBranchRequest::CreateWorktree { deadline, .. }
        | GitBranchRequest::RemoveWorktree { deadline, .. }
        | GitBranchRequest::CreateDetachedWorktree { deadline, .. } => *deadline,
    };
    let expires = Instant::now() + deadline;
    check_request(cancel, expires)?;
    match request {
        GitBranchRequest::List(request) => read_list(request_id, request, cancel, expires),
        GitBranchRequest::CreateDetachedWorktree {
            repository,
            name,
            path,
            revision,
            ..
        } => {
            let repository = open(repository)?;
            // Validate everything that could refuse before creating anything.
            if let Ok(metadata) = std::fs::symlink_metadata(path)
                && (!metadata.file_type().is_dir()
                    || std::fs::read_dir(path).map_err(backend)?.next().is_some())
            {
                return Err(GitBranchError::RefusedDestinationNotEmpty(
                    path.display().to_string(),
                ));
            }
            if operation_name(repository.state()).is_some() {
                return Err(GitBranchError::RefusedActiveOperation(
                    "main".into(),
                    operation_name(repository.state()).unwrap_or_default(),
                ));
            }
            if repository.find_worktree(name).is_ok() {
                return Err(GitBranchError::UnknownWorktree(name.clone()));
            }
            // The revision must be a real object: a review checks out what was INSPECTED.
            let object = repository
                .revparse_single(revision)
                .map_err(|_| GitBranchError::UnknownBranch(revision.clone()))?;
            let oid = object
                .peel_to_commit()
                .map_err(|_| GitBranchError::UnknownBranch(revision.clone()))?
                .id();
            // Detached: no branch is created and none is moved by looking at a review.
            add_worktree(
                &repository,
                name,
                path,
                WorktreeHead::Detached(oid),
                cancel,
                expires,
            )?;
            Ok(GitBranchResult::Changed)
        }
        GitBranchRequest::CreateWorktree {
            repository,
            name,
            path,
            reference,
            ..
        } => {
            let repository = open(repository)?;
            // Absolute destinations do not require locating a separate primary checkout.
            // Relative destinations must never be anchored to the metadata directory.
            let main = if path.is_absolute() {
                main_repository(&repository).ok()
            } else {
                Some(main_repository(&repository)?)
            };
            let path = if path.is_absolute() {
                path.clone()
            } else {
                main.as_ref()
                    .and_then(|main| main.workdir())
                    .ok_or(GitBranchError::NotRepository)?
                    .join(path)
            };
            let path = path.as_path();
            // Validate everything that could refuse before creating anything.
            if let Ok(metadata) = std::fs::symlink_metadata(path)
                && (!metadata.file_type().is_dir()
                    || std::fs::read_dir(path).map_err(backend)?.next().is_some())
            {
                return Err(GitBranchError::RefusedDestinationNotEmpty(
                    path.display().to_string(),
                ));
            }
            let short = reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference.as_str());
            if checked_out_path(&repository, short, cancel, expires)?.is_some() {
                return Err(GitBranchError::RefusedBranchAlreadyCheckedOut(
                    short.to_owned(),
                ));
            }
            if operation_name(repository.state()).is_some() {
                return Err(GitBranchError::RefusedActiveOperation(
                    "main".into(),
                    operation_name(repository.state()).unwrap_or_default(),
                ));
            }
            if repository.find_worktree(name).is_ok() {
                return Err(GitBranchError::WorktreeExists(name.clone()));
            }
            // A branch that doesn't exist yet starts at HEAD, like `git worktree add -b`.
            let existing = repository.find_branch(short, git2::BranchType::Local).ok();
            let new_branch = existing.is_none();
            check_request(cancel, expires)?;
            let branch = match existing {
                Some(branch) => branch,
                None => {
                    let head = repository
                        .head()
                        .and_then(|head| head.peel_to_commit())
                        .map_err(backend)?;
                    repository
                        .branch(short, &head, false)
                        .map_err(|_| GitBranchError::UnknownBranch(short.to_owned()))?
                }
            };
            let reference = branch.into_reference();
            let reference = reference
                .name()
                .ok_or_else(|| GitBranchError::UnknownBranch(short.to_owned()))?
                .to_owned();
            let added = check_request(cancel, expires).and_then(|()| {
                path.parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .map_err(backend)
                    .and_then(|()| {
                        add_worktree(
                            &repository,
                            name,
                            path,
                            WorktreeHead::Branch(&reference),
                            cancel,
                            expires,
                        )
                    })
            });
            if let Err(error) = added {
                if new_branch
                    && let Ok(mut branch) = repository.find_branch(short, git2::BranchType::Local)
                {
                    let _ = branch.delete();
                }
                check_request(cancel, expires)?;
                return Err(error);
            }
            check_request(cancel, expires)?;
            if let Some(main) = main {
                exclude_from_status(&main, path, cancel, expires)?;
            }
            check_request(cancel, expires)?;
            Ok(GitBranchResult::Changed)
        }
        GitBranchRequest::RemoveWorktree {
            repository, name, ..
        } => {
            let repository = open(repository)?;
            let worktree = repository
                .find_worktree(name)
                .map_err(|_| GitBranchError::UnknownWorktree(name.clone()))?;
            if !matches!(
                worktree
                    .is_locked()
                    .unwrap_or(git2::WorktreeLockStatus::Unlocked),
                git2::WorktreeLockStatus::Unlocked
            ) {
                return Err(GitBranchError::RefusedLockedWorktree(name.clone()));
            }
            let path = worktree.path().to_path_buf();
            if path.try_exists().map_err(backend)? {
                let opened = git2::Repository::open(&path).map_err(backend)?;
                let workdir = opened
                    .workdir()
                    .and_then(|path| crate::files::canonicalize(path).ok());
                if workdir != Some(crate::files::canonicalize(&path).map_err(backend)?) {
                    return Err(GitBranchError::RefusedDirtyWorktree(name.clone()));
                }
                if has_changes(&opened, cancel, expires)? {
                    return Err(GitBranchError::RefusedDirtyWorktree(name.clone()));
                }
                if let Some(operation) = operation_name(opened.state()) {
                    return Err(GitBranchError::RefusedActiveOperation(
                        name.clone(),
                        operation,
                    ));
                }
            }
            check_request(cancel, expires)?;
            // Keep the administrative data if deletion fails, so the checkout remains registered.
            if path.exists() {
                remove_checkout(&path).map_err(backend)?;
            }
            let mut options = git2::WorktreePruneOptions::new();
            options.valid(true);
            worktree.prune(Some(&mut options)).map_err(backend)?;
            check_request(cancel, expires)?;
            Ok(GitBranchResult::Changed)
        }
    }
}

fn remove_registered_worktree(
    repository: &git2::Repository,
    name: &str,
    path: &Path,
    remove: impl FnOnce(&Path, &[OsString]) -> bool,
) -> Result<(), GitBranchError> {
    // Run from the common Git directory, never the checkout being removed.
    let common = repository.commondir();
    let args = [
        "worktree".into(),
        "remove".into(),
        "--force".into(),
        "--".into(),
        path.as_os_str().to_owned(),
    ];
    if remove(common, &args) {
        return Ok(());
    }
    let missing = |path: &Path| {
        path.symlink_metadata()
            .is_err_and(|error| error.kind() == ErrorKind::NotFound)
    };
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_dir())
        && missing(&common.join("worktrees").join(name))
    {
        return Err(GitBranchError::RemovalIncomplete(
            crate::command::display_paths([path]),
        ));
    }
    Err(GitBranchError::RemovalFailed)
}

fn removal_target(
    repository: &git2::Repository,
    name: &str,
) -> Result<git2::Worktree, GitBranchError> {
    let worktree = repository
        .find_worktree(name)
        .map_err(|_| GitBranchError::UnknownWorktree(name.into()))?;
    if !matches!(
        worktree.is_locked().map_err(backend)?,
        git2::WorktreeLockStatus::Unlocked
    ) {
        return Err(GitBranchError::RefusedLockedWorktree(name.into()));
    }
    Ok(worktree)
}

/// The selected checkout and its local-only paths at the time of review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitWorktreeRemoval {
    path: PathBuf,
    inventory: Option<Arc<RemovalInventory>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RemovalInventory {
    summary: Vec<LocalPath>,
    trees: Vec<TreeDigest>,
    samples: BTreeMap<PathBuf, [u8; 32]>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LocalPath {
    relative: PathBuf,
    ignored: bool,
    directory: bool,
}

const REMOVAL_SAMPLES: usize = 128;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TreeDigest {
    entries: u64,
    sum: [u64; 4],
}

impl TreeDigest {
    fn add(&mut self, hash: [u8; 32]) {
        self.entries += 1;
        // Addition is independent of read_dir order, unlike a running SHA or
        // sorting every filename. Each contribution already includes its path.
        for (sum, bytes) in self.sum.iter_mut().zip(hash.chunks_exact(8)) {
            *sum = sum.wrapping_add(u64::from_le_bytes(bytes.try_into().expect("eight bytes")));
        }
    }
}

fn remove_checkout_once(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    /// A missing checkout can only have its worktree registration pruned.
    pub fn is_missing(&self) -> bool {
        self.inventory.is_none()
    }

    /// Whether no local-only paths were found.
    pub fn is_clean(&self) -> bool {
        self.inventory
            .as_ref()
            .is_none_or(|inventory| inventory.summary.is_empty())
    }

    fn change_notice(&self, actual: &Self) -> String {
        let detail = self
            .inventory
            .as_deref()
            .zip(actual.inventory.as_deref())
            .and_then(|(old, new)| {
                let known_absent = |sample: &BTreeMap<PathBuf, [u8; 32]>, path: &Path| {
                    sample.len() < REMOVAL_SAMPLES
                        || sample
                            .last_key_value()
                            .is_some_and(|(last, _)| path <= last.as_path())
                };
                for (path, hash) in &new.samples {
                    let change = match old.samples.get(path) {
                        Some(before) if before != hash => "changed",
                        None if known_absent(&old.samples, path) => "was added",
                        _ => continue,
                    };
                    return Some(format!(
                        "{} {change}.",
                        crate::command::display_paths([path.as_path()])
                    ));
                }
                for path in old.samples.keys() {
                    if !new.samples.contains_key(path) && known_absent(&new.samples, path) {
                        return Some(format!(
                            "{} was removed.",
                            crate::command::display_paths([path.as_path()])
                        ));
                    }
                }
                for (entry, digest) in new.summary.iter().zip(&new.trees) {
                    let change = match old.summary.binary_search(entry) {
                        Ok(index) if old.trees[index] != *digest => "changed",
                        Err(_) => "was added",
                        _ => continue,
                    };
                    return Some(format!(
                        "{} {change}.",
                        crate::command::display_paths([entry.relative.as_path()])
                    ));
                }
                old.summary
                    .iter()
                    .find(|entry| new.summary.binary_search(entry).is_err())
                    .map(|entry| {
                        format!(
                            "{} was removed.",
                            crate::command::display_paths([entry.relative.as_path()])
                        )
                    })
            });
        let instruction = "Review what will be deleted again.";
        match detail {
            Some(detail) => format!("{detail} {instruction}"),
            None => format!("The worktree's files changed. {instruction}"),
        }
        remove_entry(&entry_path, entry.file_type()?)?;
    }

    /// Warnings to show with a removal confirmation.
    pub fn warnings(&self) -> impl Iterator<Item = String> + '_ {
        let summary = self
            .inventory
            .as_ref()
            .map_or(&[][..], |inventory| inventory.summary.as_slice());
        [("untracked", false), ("ignored", true)]
            .into_iter()
            .filter_map(move |(label, ignored)| {
                let mut paths = summary.iter().filter(|entry| entry.ignored == ignored);
                let first: Vec<_> = paths.by_ref().take(3).collect();
                let mut directories = first.iter().any(|entry| entry.directory);
                let count = paths.fold(first.len(), |count, entry| {
                    directories |= entry.directory;
                    count + 1
                });
                (count != 0).then(|| {
                    let noun = if directories { "path" } else { "file" };
                    let suffix = if count == 1 { "" } else { "s" };
                    let more = if count > first.len() { ", …" } else { "" };
                    let paths = crate::command::display_paths(
                        first.iter().map(|entry| entry.relative.as_path()),
                    );
                    format!("Also deletes {count} {label} {noun}{suffix}: {paths}{more}")
                })
            })
            .chain(
                summary
                    .iter()
                    .any(|entry| entry.directory)
                    .then(|| "Listed directories will be deleted with their contents.".into()),
            )
    }
    std::fs::remove_dir(path)
}

fn removal_inventory(
    repository: &git2::Repository,
    worktree: &git2::Worktree,
    name: &str,
    cancel: &AtomicBool,
    expires: Instant,
    check: &RemovalCheck,
) -> Result<GitWorktreeRemoval, GitBranchError> {
    check_request(cancel, expires)?;
    let registered = worktree.path();
    match registered.symlink_metadata() {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(GitWorktreeRemoval {
                path: registered.into(),
                inventory: None,
            });
        }
        Err(_) => return Err(GitBranchError::RemovalInventory),
    }
    let canonical =
        |p: &Path| crate::files::canonicalize(p).map_err(|_| GitBranchError::RemovalInventory);
    let path = canonical(registered)?;
    let opened = open(&path).map_err(|_| GitBranchError::RemovalInventory)?;
    let gitdir = repository.commondir().join("worktrees").join(name);
    if !opened.is_worktree()
        || opened.workdir().map(canonical).transpose()?.as_ref() != Some(&path)
        || canonical(opened.path())? != canonical(&gitdir)?
    {
        return Err(GitBranchError::RemovalInventory);
    }
    if let Some(operation) = operation_name(opened.state()) {
        return Err(GitBranchError::RefusedActiveOperation(
            name.into(),
            operation,
        ));
    }
    let started = Instant::now();
    let policy = crate::status::GitStatusPolicy {
        root: path.clone(),
        root_revision: 0,
        maximum_files: check.display_limit,
        maximum_path_bytes: 16 * 1024 * 1024,
        deadline: expires.saturating_duration_since(started),
    };
    let (output, truncated) = crate::status::status_command(
        &path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=traditional",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
        &policy,
        started,
        cancel,
    )
    .map_err(removal_status_error)?;
    if truncated || (!output.is_empty() && !output.ends_with(&[0])) {
        return Err(GitBranchError::RemovalInventory);
    }
    // The superproject's status does not list ignored files inside initialized
    // submodules. Do not force-remove a checkout containing unreviewed submodule data.
    if opened.path().join("modules").is_dir() || path.join(".gitmodules").is_file() {
        let (submodules, truncated) = crate::status::status_command(
            &path,
            &["submodule", "status"],
            &policy,
            started,
            cancel,
        )
        .map_err(removal_status_error)?;
        if truncated
            || submodules
                .split(|byte| *byte == b'\n')
                .any(|line| !line.is_empty() && !line.starts_with(b"-"))
            || (submodules.is_empty() && opened.path().join("modules").is_dir())
        {
            return Err(GitBranchError::RemovalSubmodules);
        }
    }
    let mut summary = Vec::new();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (ignored, relative) = if let Some(path) = entry.strip_prefix(b"!! ") {
            (true, path)
        } else if let Some(path) = entry.strip_prefix(b"?? ") {
            (false, path)
        } else {
            return Err(GitBranchError::RefusedDirtyWorktree(name.into()));
        };
        let directory = relative.ends_with(b"/");
        let relative =
            crate::command::native_path(relative).ok_or(GitBranchError::RemovalInventory)?;
        if summary.len() == check.display_limit {
            return Err(GitBranchError::RemovalInventory);
        }
        summary.push(LocalPath {
            relative,
            ignored,
            directory,
        });
    }
    summary.sort_unstable();
    let mut samples = BTreeMap::new();
    let trees = summary
        .iter()
        .map(|entry| digest_removal_tree(&path, entry, &mut samples, cancel, check))
        .collect::<Result<_, _>>()?;
    Ok(GitWorktreeRemoval {
        path,
        inventory: Some(Arc::new(RemovalInventory {
            summary,
            trees,
            samples,
        })),
    })
}

fn removal_status_error(error: crate::status::GitStatusError) -> GitBranchError {
    match error {
        crate::status::GitStatusError::Deadline => GitBranchError::Deadline,
        crate::status::GitStatusError::Cancelled => GitBranchError::Cancelled,
        _ => GitBranchError::RemovalInventory,
    }
}

/// Walk one collapsed status path without following symlinks. Keep only the
/// depth-first traversal stack, a digest and a bounded sample across all trees.
fn digest_removal_tree(
    root: &Path,
    entry: &LocalPath,
    samples: &mut BTreeMap<PathBuf, [u8; 32]>,
    cancel: &AtomicBool,
    check: &RemovalCheck,
) -> Result<TreeDigest, GitBranchError> {
    #[cfg(not(test))]
    let _ = check;
    let unreadable = |_| GitBranchError::RemovalInventory;
    // Strip Git's trailing slash before lstat so it cannot follow a symlink.
    let relative: PathBuf = entry
        .relative
        .components()
        .map(|part| part.as_os_str())
        .collect();
    let mut path = root.join(relative);
    let mut digest = TreeDigest::default();
    let mut stack = Vec::new();
    let mut expected_directory = Some(entry.directory);
    loop {
        check_cancel(cancel)?;
        let metadata = path.symlink_metadata().map_err(unreadable)?;
        let kind = metadata.file_type();
        if expected_directory
            .take()
            .is_some_and(|expected| expected != kind.is_dir())
        {
            return Err(GitBranchError::RemovalInventory);
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| GitBranchError::RemovalInventory)?;
        let hash = removal_stamp(relative, &path, &metadata)?;
        digest.add(hash);
        #[cfg(test)]
        if digest.entries == 1 {
            check.pause_at(RemovalCheckpoint::FirstEntry);
        }
        if kind.is_dir() {
            stack.push(fs::read_dir(&path).map_err(unreadable)?);
        } else {
            // Keep the lexically first paths so the sample does not depend on
            // directory iteration order. Avoid sampling directory mtimes: a
            // child's addition otherwise looks like an edit to its parent.
            if samples.len() < REMOVAL_SAMPLES {
                samples.insert(relative.to_path_buf(), hash);
            } else if let Some((last, _)) = samples.last_key_value()
                && relative <= last.as_path()
            {
                samples.insert(relative.to_path_buf(), hash);
                if samples.len() > REMOVAL_SAMPLES {
                    samples.pop_last();
                }
            }
            path.pop();
        }
        loop {
            check_cancel(cancel)?;
            let Some(directory) = stack.last_mut() else {
                return Ok(digest);
            };
            if let Some(child) = directory.next() {
                path.push(child.map_err(unreadable)?.file_name());
                break;
            }
            stack.pop();
            path.pop();
        }
    }
}

fn removal_stamp(
    relative: &Path,
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<[u8; 32], GitBranchError> {
    fn hash_path(hash: &mut Sha256, path: &Path) {
        let bytes = path.as_os_str().as_encoded_bytes();
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }

    let mut hash = Sha256::new();
    hash_path(&mut hash, relative);
    let kind = metadata.file_type();
    hash.update([
        kind.is_dir() as u8,
        kind.is_file() as u8,
        kind.is_symlink() as u8,
        metadata.permissions().readonly() as u8,
    ]);
    hash.update(metadata.len().to_le_bytes());
    let modified = metadata
        .modified()
        .map_err(|_| GitBranchError::RemovalInventory)?;
    let (before_epoch, modified) = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => (false, duration),
        Err(error) => (true, error.duration()),
    };
    hash.update([before_epoch as u8]);
    hash.update(modified.as_secs().to_le_bytes());
    hash.update(modified.subsec_nanos().to_le_bytes());
    if kind.is_symlink() {
        hash_path(
            &mut hash,
            &fs::read_link(path).map_err(|_| GitBranchError::RemovalInventory)?,
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(metadata.dev().to_le_bytes());
        hash.update(metadata.ino().to_le_bytes());
        hash.update(metadata.mode().to_le_bytes());
        hash.update(metadata.ctime().to_le_bytes());
        hash.update(metadata.ctime_nsec().to_le_bytes());
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod exclude_tests {
    use crate::branches::*;

    fn repository_with_commit(root: &Path) -> git2::Repository {
        let repository = git2::Repository::init(root).unwrap();
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "one", &tree, &[])
            .unwrap();
        drop(tree);
        repository
    }

    #[test]
    fn change_checks_run_git_under_the_request_deadline() {
        let root = tempfile::tempdir().unwrap();
        let repository = repository_with_commit(root.path());
        let cancel = AtomicBool::new(false);
        let later = Instant::now() + Duration::from_secs(10);
        assert!(!has_changes(&repository, &cancel, later).unwrap());
        std::fs::write(root.path().join("untracked.txt"), "new\n").unwrap();
        assert!(has_changes(&repository, &cancel, later).unwrap());
        assert!(matches!(
            has_changes(&repository, &cancel, Instant::now()),
            Err(GitBranchError::Deadline)
        ));
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            has_changes(&repository, &cancel, later),
            Err(GitBranchError::Cancelled)
        ));
    }

    #[test]
    fn a_failed_worktree_checkout_removes_the_new_worktree() {
        let root = tempfile::tempdir().unwrap();
        let repository = repository_with_commit(root.path());
        let path = root.path().join("broken");
        let cancel = AtomicBool::new(false);
        let expires = Instant::now() + Duration::from_secs(10);
        assert!(
            add_worktree(
                &repository,
                "broken",
                &path,
                WorktreeHead::Branch("refs/heads/bad..name"),
                &cancel,
                expires,
            )
            .is_err()
        );
        assert!(!path.exists());
        assert!(repository.find_worktree("broken").is_err());
        assert!(
            repository
                .find_reference("refs/heads/dusty-worktree/broken")
                .is_err()
        );
        let head = repository.head().unwrap().target().unwrap();
        add_worktree(
            &repository,
            "broken",
            &path,
            WorktreeHead::Detached(head),
            &cancel,
            expires,
        )
        .unwrap();
        assert!(path.join("tracked.txt").is_file());
        assert!(
            repository
                .find_reference("refs/heads/dusty-worktree/broken")
                .is_err()
        );
    }

    #[test]
    fn concurrent_exclude_updates_preserve_patterns_without_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init(root.path()).unwrap();
        let exclude = repository.commondir().join("info/exclude");
        std::fs::write(&exclude, b"/retained-\xff").unwrap();
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            for index in 0..16 {
                let root = root.path();
                let barrier = &barrier;
                scope.spawn(move || {
                    let repository = git2::Repository::open(root).unwrap();
                    let cancel = AtomicBool::new(false);
                    let expires = Instant::now() + Duration::from_secs(10);
                    barrier.wait();
                    for name in [format!("child-{index}"), "shared".into()] {
                        exclude_from_status(&repository, &root.join(name), &cancel, expires)
                            .unwrap();
                    }
                });
            }
        });
        let bytes = std::fs::read(exclude).unwrap();
        assert!(bytes.starts_with(b"/retained-\xff\n"));
        let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        for name in (0..16)
            .map(|index| format!("/child-{index}/"))
            .chain(["/shared/".into()])
        {
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| **line == name.as_bytes())
                    .count(),
                1,
                "{name}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod checkout_removal_tests {
    use super::remove_checkout;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_checkout_that_cannot_be_fully_deleted_keeps_its_git_link() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("review");
        let locked = checkout.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(checkout.join(".git"), "gitdir: elsewhere\n").unwrap();
        std::fs::write(checkout.join("a.txt"), "a\n").unwrap();
        std::fs::write(locked.join("held.txt"), "held\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = remove_checkout(&checkout);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Permissions do not stop root; there is nothing to hold the deletion then.
        if result.is_err() {
            assert!(
                checkout.join(".git").is_file(),
                "the link survives a failed deletion"
            );
            remove_checkout(&checkout).unwrap();
        }
        assert!(!checkout.exists());
    }
}
