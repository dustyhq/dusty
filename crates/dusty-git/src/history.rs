//! Pull, push, undo and redo, and history rewriting. Each is a plan the
//! Git tab shows in a confirm sheet. HEAD changes run as native Git commands; an
//! index-only undo restores its saved snapshot without touching HEAD or the worktree.
use crate::files::RootIdentity;
use git2::{
    DiffOptions, FileMode, Index, IndexEntryExtendedFlag, IndexEntryFlag, IndexTime, Oid,
    Repository, RepositoryState,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use sha2::{Digest, Sha256};

/// One `git` invocation: its arguments and the variables it adds to Git's environment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Run {
    pub args: Vec<OsString>,
    pub env: Vec<(String, OsString)>,
    pub(crate) check: Option<CheckoutCheck>,
}

impl Run {
    pub(crate) fn new<I: IntoIterator<Item = S>, S: Into<OsString>>(args: I) -> Self {
        Self {
            args: args.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    fn checked(mut self, check: CheckoutCheck) -> Self {
        self.check = Some(check);
        self
    }

    fn with(mut self, key: &str, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The command as shown in a sheet, with shortened commit hashes.
    pub fn display(&self) -> String {
        let words = self
            .args
            .iter()
            .filter(|arg| {
                arg.to_str() != Some("-c")
                    && !arg.to_string_lossy().starts_with("rebase.updateRefs")
            })
            .map(|arg| {
                let arg = arg.to_string_lossy();
                if arg.len() == 40 && arg.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    arg[..7].to_owned()
                } else {
                    arg.into_owned()
                }
            })
            .collect::<Vec<_>>();
        format!("git {}", words.join(" "))
    }
}

fn open(root: &Path) -> Result<Repository, String> {
    Repository::open(root).map_err(|error| error.message().to_owned())
}

fn failed(error: git2::Error) -> String {
    error.message().to_owned()
}

/// Refuses while a merge, rebase, cherry-pick, revert or bisect is paused.
fn settled(repository: &Repository) -> Result<(), String> {
    let name = match repository.state() {
        RepositoryState::Clean => return Ok(()),
        RepositoryState::Merge => "merge",
        RepositoryState::Revert | RepositoryState::RevertSequence => "revert",
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => "cherry-pick",
        RepositoryState::Bisect => "bisect",
        _ => "rebase",
    };
    Err(format!(
        "A {name} is paused: continue or abort it first (C or X)"
    ))
}

pub fn short(id: Oid) -> String {
    id.to_string()[..7].to_owned()
}

fn summary(commit: &git2::Commit) -> String {
    commit.summary().unwrap_or("").to_owned()
}

/// The current branch's short name; `None` on a detached HEAD.
fn branch(repository: &Repository) -> Result<Option<String>, String> {
    let head = repository.find_reference("HEAD").map_err(failed)?;
    Ok(head
        .symbolic_target()
        .and_then(|name| name.strip_prefix("refs/heads/"))
        .map(str::to_owned))
}

// ---------------------------------------------------------------------------------------
// Pull and branch push

/// How `git pull` joins the fetched branch to the local one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullMode {
    Merge,
    Rebase,
    FastForward,
}

impl PullMode {
    pub const ALL: [Self; 3] = [Self::Merge, Self::Rebase, Self::FastForward];

    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::FastForward => "fast-forward only",
        }
    }

    pub fn parse(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.label() == label)
    }
}

/// An attached checkout and the reviewed remote ref or short destination name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetworkTarget {
    checkout: RootIdentity,
    git_dir: RootIdentity,
    pub(crate) branch: String,
    pub(crate) remote: String,
    pub(crate) reference: String,
    endpoints: EndpointFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteUse {
    Fetch,
    Push,
}

// Git's output can contain credentials. Keep only a fingerprint, never the URL.
type EndpointFingerprint = [u8; 32];

fn remote_endpoints(root: &Path, remote: &str, usage: RemoteUse) -> Option<EndpointFingerprint> {
    let mut args = vec!["remote", "get-url"];
    if usage == RemoteUse::Push {
        args.extend(["--push", "--all"]);
    }
    args.extend(["--", remote]);
    let run = |args: &[&str]| {
        crate::command::git_output(
            root,
            &args.iter().copied().map(OsString::from).collect::<Vec<_>>(),
            &[],
        )
        .ok()
        .filter(|output| output.status.success() && !output.stdout.is_empty())
    };
    let output = run(&args).or_else(|| {
        if remote != "." {
            return None;
        }
        // An unnamed local remote (branch.<name>.remote = .) still uses Git's
        // URL rules. Temporarily give it a URL so get-url can resolve it.
        let mut local = vec!["-c", "remote...url=."];
        local.extend(args);
        run(&local)
    })?;
    Some(Sha256::digest(&output.stdout).into())
}

impl NetworkTarget {
    fn capture(
        repository: &Repository,
        branch: String,
        remote: String,
        reference: String,
        endpoints: EndpointFingerprint,
    ) -> Result<Self, String> {
        let checkout = repository.workdir().ok_or("Open a checkout first.")?;
        Ok(Self {
            checkout: RootIdentity::capture(checkout).map_err(|_| "Can't check the checkout.")?,
            git_dir: RootIdentity::capture(repository.path())
                .map_err(|_| "Can't check the checkout.")?,
            branch,
            remote,
            reference,
            endpoints,
        })
    }

    pub(crate) fn checkout(&self) -> &Path {
        &self.checkout.canonical_path
    }

    pub(crate) fn other_branch(&self) -> String {
        let name = self
            .reference
            .strip_prefix("refs/heads/")
            .unwrap_or(&self.reference);
        if self.remote == "." {
            name.into()
        } else {
            format!("{}/{name}", self.remote)
        }
    }
}

fn branch_merge_ref(config: &git2::Config, branch: &str) -> Option<String> {
    let mut entries = config
        .multivar(&format!("branch.{branch}.merge"), None)
        .ok()?;
    let reference = entries.next()?.ok()?.value()?.to_owned();
    (entries.next().is_none() && git2::Reference::is_valid_name(&reference)).then_some(reference)
}

fn config_snapshot(repository: &Repository) -> Result<git2::Config, String> {
    repository
        .config()
        .and_then(|mut config| config.snapshot())
        .map_err(failed)
}

fn configured_remote(repository: &Repository, remote: &str) -> bool {
    remote == "." || git2::Remote::is_valid_name(remote) && repository.find_remote(remote).is_ok()
}

/// `p`: the branch, its upstream and the mode its configuration asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pull {
    pub(crate) target: NetworkTarget,
    pub mode: PullMode,
}

/// The pull the current branch is configured for: `branch.<name>.rebase`, then
/// `pull.rebase`, then `pull.ff = only`, as `git pull` reads them.
pub fn pull(root: &Path) -> Result<Pull, String> {
    pull_plan(&open(root)?)
}

fn pull_plan(repository: &Repository) -> Result<Pull, String> {
    settled(repository)?;
    let branch = branch(repository)?.ok_or("Pull needs a branch: HEAD is detached")?;
    let config = config_snapshot(repository)?;
    let no_upstream = || format!("{branch} has no upstream · u on the branch sets one");
    let remote = config
        .get_string(&format!("branch.{branch}.remote"))
        .ok()
        .filter(|remote| configured_remote(repository, remote))
        .ok_or_else(no_upstream)?;
    let reference = branch_merge_ref(&config, &branch).ok_or_else(no_upstream)?;
    let rebase = |key: &str| {
        config.get_string(key).ok().map(|value| {
            !matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "no" | "off" | "0"
            )
        })
    };
    let mode = match rebase(&format!("branch.{branch}.rebase")).or_else(|| rebase("pull.rebase")) {
        Some(true) => PullMode::Rebase,
        _ if config
            .get_string("pull.ff")
            .is_ok_and(|value| value == "only") =>
        {
            PullMode::FastForward
        }
        _ => PullMode::Merge,
    };
    let checkout = repository.workdir().ok_or("Open a checkout first.")?;
    let endpoints = remote_endpoints(checkout, &remote, RemoteUse::Fetch)
        .ok_or("Can't confirm where this pull will come from.")?;
    Ok(Pull {
        target: NetworkTarget::capture(repository, branch, remote, reference, endpoints)?,
        mode,
    })
}

/// Pull the reviewed remote ref into the reviewed branch.
pub fn pull_run(plan: Pull, mode: PullMode) -> Run {
    let flags: &[&str] = match mode {
        PullMode::Merge => &["--no-rebase"],
        PullMode::Rebase => &["--rebase", "--autostash"],
        PullMode::FastForward => &["--ff-only"],
    };
    let target = plan.target;
    Run::new(
        ["pull", "--progress"]
            .into_iter()
            .chain(flags.iter().copied())
            .chain(["--", &target.remote, &target.reference]),
    )
    .with("GIT_EDITOR", "true")
    .checked(CheckoutCheck::Pull(target))
}

pub(crate) const NETWORK_TARGET_CHANGED: &str =
    "The checkout, branch, or remote changed. Reopen this action.";
const NO_PUSH_TARGET: &str = "Can't confirm where this push will go. Use Git to push.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Push {
    pub(crate) target: NetworkTarget,
    force: bool,
    set_upstream: bool,
}

impl Push {
    pub(crate) fn run(self) -> Run {
        let mut args = vec![
            "push".into(),
            "--progress".into(),
            "--no-follow-tags".into(),
        ];
        if self.set_upstream {
            args.push("--set-upstream".into());
        }
        let target = &self.target;
        let force = if self.force { "+" } else { "" };
        args.extend([
            "--".to_owned(),
            target.remote.clone(),
            format!("{force}refs/heads/{}:{}", target.branch, target.reference),
        ]);
        Run::new(args).checked(CheckoutCheck::Push(self))
    }
}

/// A default push can name several refs. Only offer the branch sheet if Git's
/// configuration picks one destination for the current branch.
pub(crate) fn push(root: &Path) -> Result<Push, String> {
    push_plan(&open(root)?)
}

fn push_plan(repository: &Repository) -> Result<Push, String> {
    let branch = branch(repository)?.ok_or("Switch to a branch before pushing.")?;
    let config = config_snapshot(repository)?;
    let remotes = repository.remotes().map_err(failed)?;
    let default_remote = if remotes.len() == 1 {
        remotes.get(0).unwrap_or("origin")
    } else {
        "origin"
    };
    let fetch_remote = config
        .get_string(&format!("branch.{branch}.remote"))
        .unwrap_or_else(|_| default_remote.to_owned());
    let remote = config
        .get_string(&format!("branch.{branch}.pushRemote"))
        .or_else(|_| config.get_string("remote.pushDefault"))
        .unwrap_or_else(|_| fetch_remote.clone());
    if !configured_remote(repository, &remote)
        || config
            .get_bool(&format!("remote.{remote}.mirror"))
            .unwrap_or(false)
    {
        return Err(NO_PUSH_TARGET.into());
    }
    let source = format!("refs/heads/{branch}");
    let checkout = repository.workdir().ok_or("Open a checkout first.")?;
    let endpoints = remote_endpoints(checkout, &remote, RemoteUse::Push).ok_or(NO_PUSH_TARGET)?;
    let mut force = false;
    let mut set_upstream = false;
    let mut specs = config
        .multivar(&format!("remote.{remote}.push"), None)
        .map_err(failed)?;
    let spec = specs.next().transpose().map_err(failed)?;
    let reference = if let Some(spec) = spec {
        let spec = spec.value().ok_or(NO_PUSH_TARGET)?.to_owned();
        if specs.next().is_some() {
            return Err(NO_PUSH_TARGET.into());
        }
        force = spec.starts_with('+');
        let unforced = spec.strip_prefix('+').unwrap_or(&spec);
        let (from, to) = unforced
            .split_once(':')
            .unwrap_or((unforced, source.as_str()));
        let short_source = from == branch
            && repository
                .find_reference(&format!("refs/tags/{from}"))
                .is_err();
        if from != source && from != "HEAD" && !short_source {
            return Err(NO_PUSH_TARGET.into());
        }
        push_destination(repository, &remote, to).ok_or(NO_PUSH_TARGET)?
    } else {
        let upstream = branch_merge_ref(&config, &branch);
        let auto_setup = config.get_entry(&format!("branch.{branch}.merge")).is_err()
            && config.get_bool("push.autoSetupRemote").unwrap_or(false);
        let mode = config
            .get_string("push.default")
            .unwrap_or_else(|_| "simple".into());
        let reference = match mode.as_str() {
            "simple" if remote != fetch_remote => source,
            "current" => {
                set_upstream = auto_setup;
                source
            }
            "simple" | "upstream" | "tracking" if remote == fetch_remote => {
                set_upstream = auto_setup;
                let destination = if auto_setup {
                    source.clone()
                } else {
                    upstream.ok_or(NO_PUSH_TARGET)?
                };
                if mode == "simple" && destination != source {
                    return Err(NO_PUSH_TARGET.into());
                }
                destination
            }
            _ => return Err(NO_PUSH_TARGET.into()),
        };
        if !full_ref(&reference) {
            return Err(NO_PUSH_TARGET.into());
        }
        reference
    };
    Ok(Push {
        target: NetworkTarget::capture(repository, branch, remote, reference, endpoints)?,
        force,
        set_upstream,
    })
}

fn full_ref(reference: &str) -> bool {
    reference.starts_with("refs/") && git2::Reference::is_valid_name(reference)
}

/// A remote-tracking branch can qualify a short destination. A local tag
/// says nothing about the remote; without a tracking branch, let Git resolve it.
fn push_destination(repository: &Repository, remote: &str, name: &str) -> Option<String> {
    if name.starts_with("refs/") {
        return full_ref(name).then(|| name.to_owned());
    }
    let branch = format!("refs/heads/{name}");
    if !full_ref(&branch) {
        return None;
    }
    let tracking = format!("refs/remotes/{remote}/{name}");
    if repository
        .find_reference(&tracking)
        .is_ok_and(|reference| reference.target().is_some())
    {
        Some(branch)
    } else {
        Some(name.into())
    }
}

// ---------------------------------------------------------------------------------------
// Undo and redo

pub const UNDO_ACTION: &str = "[dusty undo]";
pub const REDO_ACTION: &str = "[dusty redo]";
pub(crate) const UNSTAGE_ACTION: &str = "unstage: ";
const DETACHED_CHECKOUT: &str = " (detached checkout)";
pub(crate) const SKIPPED_INDEX: &str = " [skipped unstage:";
const UPDATING_HEAD: &str = ": updating HEAD";

pub(crate) const CHECKOUT_CHANGED: &str = "Your checkout changed. Cancel and try again.";
pub(crate) const CHECKOUT_CHANGED_AFTER_CLOSE: &str =
    "Your checkout changed. Open the action again.";
const COMMIT_OUTSIDE_CHECKOUT: &str = "That commit isn't in the history of your checkout.";
pub(crate) const FIXUP_WITHOUT_HISTORY: &str =
    "Git isn't recording checkout history. No fixup commit was written.";
pub(crate) const CHECKOUT_CHANGED_AT_CAPTURE: &str =
    "The checked-out branch changed. Press the key again.";

/// A local branch at a particular tip, or a commit checked out directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckoutTarget {
    Branch { reference: String, tip: Oid },
    Detached(Oid),
}

impl CheckoutTarget {
    fn from_reflog(name: &str, tip: Oid) -> Self {
        // Git records a detached source as its full hash. A name alone is not
        // proof of detachment: don't let a tag stand in for a missing branch.
        if name == tip.to_string() {
            Self::Detached(tip)
        } else {
            Self::Branch {
                reference: format!("refs/heads/{name}"),
                tip,
            }
        }
    }

    pub(crate) fn reference(&self) -> Option<&str> {
        match self {
            Self::Branch { reference, .. } => Some(reference),
            Self::Detached(_) => None,
        }
    }

    pub(crate) fn tip(&self) -> Oid {
        match *self {
            Self::Branch { tip, .. } | Self::Detached(tip) => tip,
        }
    }

    pub(crate) fn revision(&self) -> String {
        self.reference()
            .map_or_else(|| self.tip().to_string(), str::to_owned)
    }

    fn label(&self) -> String {
        self.reference().map_or_else(
            || short(self.tip()),
            |reference| ref_label(reference).to_owned(),
        )
    }

    fn local_tip(repository: &Repository, reference: &str) -> Option<Oid> {
        repository.find_reference(reference).ok()?.target()
    }

    fn matches_head(&self, repository: &Repository) -> bool {
        self.matches_head_at(repository, self.tip())
    }

    fn matches_head_at(&self, repository: &Repository, tip: Oid) -> bool {
        let Ok(head) = repository.find_reference("HEAD") else {
            return false;
        };
        match self {
            Self::Branch { reference, .. } => {
                head.symbolic_target() == Some(reference.as_str())
                    && Self::local_tip(repository, reference) == Some(tip)
            }
            Self::Detached(_) => head.target() == Some(tip),
        }
    }

    fn still_exists(&self, repository: &Repository) -> bool {
        match self {
            Self::Branch { reference, tip } => Self::local_tip(repository, reference) == Some(*tip),
            Self::Detached(tip) => repository.find_commit(*tip).is_ok(),
        }
    }

    fn switch(&self) -> Run {
        let args = match self {
            Self::Branch { reference, .. } => [
                "switch".into(),
                "--quiet".into(),
                "--no-guess".into(),
                "--".into(),
                ref_label(reference).into(),
            ],
            Self::Detached(tip) => [
                "switch".into(),
                "--quiet".into(),
                "--detach".into(),
                "--".into(),
                tip.to_string().into(),
            ],
        };
        Run::new(crate::command::protect_checkout(args))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CheckoutCheck {
    Pull(NetworkTarget),
    Push(Push),
    Rewrite(CheckoutTarget),
    Fixup(CheckoutTarget),
    Autosquash(CheckoutTarget),
    CommitUndo {
        source: CheckoutTarget,
        target: Oid,
    },
    Undo {
        source: CheckoutTarget,
        destination: CheckoutTarget,
    },
}

impl CheckoutCheck {
    /// Git updates HEAD before invoking post-checkout. Match both the exact
    /// branch (or detachment) and its tip, including same-commit branch switches.
    pub(crate) fn undo_landed(&self, root: &Path) -> bool {
        open(root).is_ok_and(|repository| match self {
            Self::CommitUndo { source, target } => source.matches_head_at(&repository, *target),
            Self::Undo { destination, .. } => destination.matches_head(&repository),
            _ => false,
        })
    }

    pub(crate) fn verify(&self, root: &Path) -> Result<(), String> {
        let repository = open(root).map_err(|_| CHECKOUT_CHANGED)?;
        let matches = match self {
            Self::Pull(expected) => {
                return pull_plan(&repository)
                    .ok()
                    .filter(|plan| plan.target == *expected)
                    .map(|_| ())
                    .ok_or_else(|| NETWORK_TARGET_CHANGED.into());
            }
            Self::Push(expected) => {
                return push_plan(&repository)
                    .ok()
                    .filter(|plan| plan == expected)
                    .map(|_| ())
                    .ok_or_else(|| NETWORK_TARGET_CHANGED.into());
            }
            Self::Rewrite(checkout) => checkout.matches_head(&repository),
            Self::Fixup(checkout) => {
                let matches = checkout.matches_head(&repository);
                // Autosquash needs the HEAD log to identify the exact commit left by
                // this step. A missing log may stay missing if Git isn't logging.
                if matches
                    && (!repository.reference_has_log("HEAD").unwrap_or(false)
                        || repository.reflog("HEAD").is_err())
                {
                    return Err(FIXUP_WITHOUT_HISTORY.into());
                }
                matches
            }
            Self::Autosquash(before) => {
                // Only accept the commit made by the immediately preceding fixup
                // step, on the approved checkout. A hook could have switched
                // branches (even to the same commit) or moved HEAD again.
                let Some(tip) = repository.reflog("HEAD").ok().and_then(|log| {
                    let entry = log.get(0)?;
                    (entry.id_old() == before.tip()
                        && entry.message()?.starts_with("commit: fixup! "))
                    .then(|| entry.id_new())
                }) else {
                    return Err(CHECKOUT_CHANGED.into());
                };
                let mut after = before.clone();
                match &mut after {
                    CheckoutTarget::Branch { tip: expected, .. }
                    | CheckoutTarget::Detached(expected) => *expected = tip,
                }
                after.matches_head(&repository)
            }
            Self::CommitUndo { source, target } => {
                if !source.matches_head(&repository) {
                    return Err(CHECKOUT_CHANGED.into());
                }
                settled(&repository)?;
                return check_staged(&repository, source.tip(), *target);
            }
            Self::Undo {
                source,
                destination,
            } => source.matches_head(&repository) && destination.still_exists(&repository),
        };
        if matches {
            Ok(())
        } else {
            Err(CHECKOUT_CHANGED.into())
        }
    }
}

/// What one reflog action moved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Move {
    /// A commit, reset, pull, merge or a whole rebase: HEAD went from one commit to another.
    Commit { from: Oid, to: Oid },
    /// Unstage changed only the index. HEAD's reflog names its saved snapshots.
    Index { record: Oid, head: Oid },
    /// A branch switch: from one branch (or detached commit) to another.
    Checkout {
        from: CheckoutTarget,
        to: CheckoutTarget,
    },
}

/// `z` or `Z`: the reflog action to reverse (or replay) and how.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Undo {
    pub redo: bool,
    /// The action as shown in the confirmation.
    pub entry: String,
    pub moved: Move,
    commit_source: Option<CheckoutTarget>,
    // These entries did not apply when walking back to this action. Record the
    // skip only when the chosen action runs, so Redo won't replay them.
    skipped_index: Vec<Oid>,
}

impl Undo {
    /// Where HEAD is now and where it goes.
    pub fn head(&self) -> (String, String) {
        let (from, to) = match &self.moved {
            Move::Commit { from, to } => (short(*from), short(*to)),
            Move::Index { head, .. } => (short(*head), short(*head)),
            Move::Checkout { from, to } => (from.label(), to.label()),
        };
        if self.redo { (from, to) } else { (to, from) }
    }

    fn checkout(&self) -> Option<(&CheckoutTarget, &CheckoutTarget)> {
        let Move::Checkout { from, to } = &self.moved else {
            return None;
        };
        Some(if self.redo { (from, to) } else { (to, from) })
    }

    pub(crate) fn checkout_destination(&self) -> Option<&CheckoutTarget> {
        self.checkout().map(|(_, destination)| destination)
    }

    /// Builds the command for display or execution after rechecking the plan with `undo`.
    pub fn run(&self) -> Option<Run> {
        let mut action = self.reflog_action();
        let run = match &self.moved {
            Move::Index { .. } => return None,
            Move::Commit { from, to } => {
                let source = self.commit_source.as_ref().expect("commit undo source");
                let target = if self.redo { *to } else { *from };
                // A two-tree switch keeps index entries on unchanged paths and
                // refuses collisions on changed ones. Keep the current HEAD kind.
                // Keep switch's success chatter out of hook diagnostics.
                let mut args: Vec<OsString> = vec!["switch".into(), "--quiet".into()];
                if let Some(branch) = source.reference() {
                    args.extend(["--no-track", "-C", ref_label(branch)].map(OsString::from));
                } else {
                    args.push("--detach".into());
                }
                args.push("--".into());
                args.push(target.to_string().into());
                action.push_str(UPDATING_HEAD);
                Run::new(crate::command::protect_checkout(args)).checked(
                    CheckoutCheck::CommitUndo {
                        source: source.clone(),
                        target,
                    },
                )
            }
            Move::Checkout { .. } => {
                let (source, destination) = self.checkout()?;
                destination.switch().checked(CheckoutCheck::Undo {
                    source: source.clone(),
                    destination: destination.clone(),
                })
            }
        };
        Some(run.with("GIT_REFLOG_ACTION", action))
    }

    fn reflog_action(&self) -> String {
        let mut action = if self.redo { REDO_ACTION } else { UNDO_ACTION }.to_owned();
        if matches!(
            &self.moved,
            Move::Checkout {
                to: CheckoutTarget::Detached(_),
                ..
            }
        ) {
            action.push_str(DETACHED_CHECKOUT);
        }
        if !self.skipped_index.is_empty() {
            action.push_str(SKIPPED_INDEX);
            for (i, record) in self.skipped_index.iter().enumerate() {
                if i != 0 {
                    action.push(',');
                }
                action.push_str(&record.to_string());
            }
            action.push(']');
        }
        action
    }

    /// Restore the saved index after rechecking the confirmed history entry.
    pub(crate) fn restore_index(&self, root: &Path) -> Result<(), String> {
        let Move::Index { record, head } = self.moved else {
            return Err("This action doesn't change only the staged files".into());
        };
        if undo(root, self.redo)? != *self {
            return Err("Undo history changed. Open Undo again.".into());
        }
        crate::mutation::apply_index_undo(
            &open(root)?,
            record,
            head,
            self.redo,
            &self.reflog_action(),
        )
    }
}

/// Refuse to overwrite staged-only content on paths the commit move can touch.
fn check_staged(repository: &Repository, head: Oid, target: Oid) -> Result<(), String> {
    let unreadable = "Can't check staged changes. Nothing was changed.";
    let workdir = repository.workdir().ok_or(unreadable)?;
    let tree = repository
        .find_commit(head)
        .and_then(|commit| commit.tree())
        .map_err(|_| unreadable)?;
    let target = repository
        .find_commit(target)
        .and_then(|commit| commit.tree())
        .map_err(|_| unreadable)?;
    let changed = repository
        .diff_tree_to_tree(Some(&tree), Some(&target), None)
        .map_err(|_| unreadable)?;
    let touched = changed
        .deltas()
        .map(|delta| {
            delta
                .new_file()
                .path()
                .map(Path::to_owned)
                .ok_or(unreadable)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if touched.is_empty() {
        return Ok(());
    }
    let mut index = repository.index().map_err(|_| unreadable)?;
    // libgit2 can omit added entries marked assume-unchanged from the staged diff.
    // Clear that flag in this read-only snapshot, not in the on-disk index.
    let assume_unchanged = IndexEntryFlag::VALID.bits();
    let hidden: Vec<_> = index
        .iter()
        .filter(|entry| entry.flags & assume_unchanged != 0)
        .collect();
    for mut entry in hidden {
        entry.flags &= !assume_unchanged;
        index.add(&entry).map_err(|_| unreadable)?;
    }
    let staged = repository
        .diff_tree_to_index(
            Some(&tree),
            Some(&index),
            Some(DiffOptions::new().include_typechange(true)),
        )
        .map_err(|_| unreadable)?;

    // Diff just the staged paths, without flags or cached stat data that can
    // hide differences in the working copy. Never update the real index.
    let mut compare = Index::new().map_err(|_| unreadable)?;
    let mut options = DiffOptions::new();
    options
        .disable_pathspec_match(true)
        .include_typechange(true);
    let mut mismatches = BTreeSet::new();
    for delta in staged.deltas() {
        let path = delta.new_file().path().ok_or(unreadable)?;
        // An ancestor/descendant can also collide in a file/directory replacement.
        use std::ops::Bound::{Included, Unbounded};
        if !path.ancestors().any(|parent| touched.contains(parent))
            && !touched
                .range::<Path, _>((Included(path), Unbounded))
                .next()
                .is_some_and(|p| p.starts_with(path))
        {
            continue;
        }
        let disk = workdir.join(path);
        let entry = index
            .get_path(path, 0)
            .filter(|entry| Some(entry.path.as_slice()) == delta.new_file().path_bytes());
        if let Some(mut entry) = entry {
            if IndexEntryExtendedFlag::from_bits_truncate(entry.flags_extended).is_intent_to_add() {
                continue;
            }
            if entry.mode == u32::from(FileMode::Commit) {
                // Compare the checked-out commit even when Git is set to ignore submodule changes.
                let checked_out =
                    Repository::open(&disk).and_then(|repo| repo.head().map(|head| head.target()));
                if checked_out.ok().flatten() != Some(entry.id) {
                    mismatches.insert(path.to_owned());
                }
                continue;
            }
            entry.flags = 0;
            entry.flags_extended = 0;
            entry.mtime = IndexTime::new(0, 0);
            entry.ino = 0;
            entry.file_size = 0;
            compare.add(&entry).map_err(|_| unreadable)?;
            options.pathspec(path);
        } else {
            // A staged deletion also matches if the old file was replaced by a directory.
            match fs::symlink_metadata(&disk) {
                Ok(metadata)
                    if metadata.is_dir() && delta.old_file().mode() != FileMode::Commit => {}
                Ok(_) => {
                    mismatches.insert(path.to_owned());
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
                Err(_) => return Err(unreadable.into()),
            }
        }
    }

    if !compare.is_empty() {
        let working = repository
            .diff_index_to_workdir(Some(&compare), Some(&mut options))
            .map_err(|_| unreadable)?;
        for delta in working.deltas() {
            mismatches.insert(delta.old_file().path().ok_or(unreadable)?.to_owned());
        }
    }
    if let Some(path) = mismatches.first() {
        let more = if mismatches.len() > 1 {
            format!(" (and {} more)", mismatches.len() - 1)
        } else {
            String::new()
        };
        return Err(format!(
            "Undo would drop staged changes in {}{more}. Commit or unstage them first.",
            path.display()
        ));
    }
    Ok(())
}

/// A rebase's (or `pull --rebase`'s) step word: `start`, `pick`, `finish`, `abort`.
fn rebase_step(message: &str) -> Option<&str> {
    let head = message.split(':').next()?;
    if !(head.starts_with("rebase") || head.starts_with("pull")) {
        return None;
    }
    let open = head.rfind('(')?;
    head[open + 1..].strip_suffix(')')
}

// These entries advance or reset HEAD without switching between a branch and a
// detached commit. Commit moves have a suffix that checkout moves don't.
fn preserves_checkout(message: &str) -> bool {
    message.starts_with("commit:")
        || message.starts_with("commit (")
        || message.starts_with("reset: moving to ")
        || message == crate::rewrite::REFLOG_ACTION
        || is_commit_move_marker(message)
}

pub(crate) fn is_commit_move_marker(message: &str) -> bool {
    (message.starts_with(UNDO_ACTION) || message.starts_with(REDO_ACTION))
        && message.ends_with(UPDATING_HEAD)
}

/// The action `z` (or `Z`) reverses, walking HEAD's reflog as lazygit does: each
/// `[dusty undo]` entry skips one more action back, each `[dusty redo]` one less, and a
/// finished rebase counts as one action from its start to its end.
pub fn undo(root: &Path, redo: bool) -> Result<Undo, String> {
    let repository = open(root)?;
    settled(&repository)?;
    let reflog = repository.reflog("HEAD").map_err(failed)?;
    let mut detached_at = repository
        .find_reference("HEAD")
        .ok()
        .and_then(|head| head.target());
    let mut counter = 0i32;
    let mut skipped_index = Vec::new();
    let mut past_skips = BTreeMap::<Oid, usize>::new();
    // At each undo depth, remember the newest replay of that action, in the
    // original direction. A bare marker can't tell us how HEAD was checked out.
    let mut replays = BTreeMap::new();
    let mut finish: Option<Oid> = None;
    for entry in reflog.iter() {
        let message = entry.message().unwrap_or("");
        let (old, new) = (entry.id_old(), entry.id_new());
        // Follow live detachment back only through a continuous chain of
        // commits and resets. A switch or an unknown entry breaks that proof.
        let detached_destination = detached_at == Some(new);
        detached_at =
            (detached_destination && preserves_checkout(message.trim_end())).then_some(old);
        let moved = if let Some(to) = finish {
            if rebase_step(message) == Some("start") {
                finish = None;
                Some(Move::Commit { from: old, to })
            } else {
                None
            }
        } else if let Some(detail) = message.strip_prefix(UNDO_ACTION) {
            let detail = if let Some((detail, skipped)) = detail.split_once(SKIPPED_INDEX) {
                if let Some((records, _)) = skipped.split_once(']') {
                    for record in records.split(',').filter_map(|id| Oid::from_str(id).ok()) {
                        *past_skips.entry(record).or_default() += 1;
                    }
                }
                detail
            } else {
                detail
            };
            counter += 1;
            replays
                .entry(counter)
                .or_insert_with(|| (detail.trim_end() == DETACHED_CHECKOUT).then_some((new, old)));
            None
        } else if let Some(detail) = message.strip_prefix(REDO_ACTION) {
            replays
                .entry(counter)
                .or_insert_with(|| (detail.trim_end() == DETACHED_CHECKOUT).then_some((old, new)));
            counter -= 1;
            None
        } else if matches!(rebase_step(message), Some("finish" | "abort")) {
            finish = Some(new);
            None
        } else if rebase_step(message) == Some("start") {
            return Err("A rebase is in progress: continue or abort it first (C or X)".into());
        } else if let Some(record) = message.strip_prefix(UNSTAGE_ACTION) {
            let Ok(record) = Oid::from_str(record) else {
                continue;
            };
            if let Some(skips) = past_skips.get_mut(&record)
                && *skips != 0
            {
                *skips -= 1;
                continue;
            }
            Some(Move::Index { record, head: new })
        } else if let Some(rest) = message.strip_prefix("checkout: moving from ") {
            rest.split_once(" to ").map(|(from, to)| {
                // Git writes the literal destination (e.g. a tag), not whether it
                // detached. Require proof from live HEAD or a replay of this
                // specific move. Never guess based on revision lookup.
                let detached =
                    detached_destination || replays.get(&counter) == Some(&Some((old, new)));
                Move::Checkout {
                    from: CheckoutTarget::from_reflog(from, old),
                    to: if detached {
                        CheckoutTarget::Detached(new)
                    } else {
                        CheckoutTarget::from_reflog(to.trim_end(), new)
                    },
                }
            })
        } else if message == crate::rewrite::REFLOG_ACTION
            || [
                "commit",
                "reset: moving to",
                "pull",
                "merge",
                "cherry-pick",
                "revert",
            ]
            .iter()
            .any(|prefix| message.starts_with(prefix))
        {
            Some(Move::Commit { from: old, to: new })
        } else {
            None
        };
        let Some(moved) = moved else { continue };
        let noop = match &moved {
            Move::Commit { from, to } => from == to,
            Move::Index { .. } => false,
            Move::Checkout { from, to } => from == to,
        };
        if noop {
            continue;
        }
        let fire = if redo {
            if counter <= 0 {
                return Err("Nothing to redo".into());
            }
            counter == 1
        } else {
            counter == 0
        };
        if fire {
            let head = repository.head().ok().and_then(|head| head.target());
            let unexpected_head = "Undo can't follow the last change to HEAD.";
            if let Move::Index {
                record,
                head: expected,
            } = moved
            {
                let matches_head = head.unwrap_or_else(Oid::zero) == expected;
                let applies =
                    matches_head && crate::mutation::index_undo_matches(&repository, record, redo)?;
                if !applies && !redo {
                    skipped_index.push(record);
                    continue;
                }
                if !matches_head {
                    return Err(unexpected_head.into());
                }
                if !applies {
                    return Err("The staged changes have changed since unstaging".into());
                }
            }
            let mut undo = Undo {
                redo,
                entry: if matches!(moved, Move::Index { .. }) {
                    "Unstage changes".into()
                } else {
                    message.trim_end().to_owned()
                },
                moved,
                commit_source: None,
                skipped_index,
            };
            match &undo.moved {
                Move::Index { .. } => {}
                Move::Commit { from, to } => {
                    let expected_head = if redo { *from } else { *to };
                    if head != Some(expected_head) {
                        return Err(unexpected_head.into());
                    }
                    if !redo && from.is_zero() {
                        return Err("The first commit has nothing before it to return to".into());
                    }
                    check_staged(&repository, expected_head, if redo { *to } else { *from })?;
                    undo.commit_source = Some(checked_out_history(&repository, "HEAD")?);
                }
                Move::Checkout { .. } => {
                    let (source, target) = undo.checkout().expect("checkout action");
                    if !source.matches_head(&repository) {
                        return Err("Your current checkout no longer matches that action.".into());
                    }
                    if !target.still_exists(&repository) {
                        return Err(format!(
                            "{} has moved since you left it. Check it out from Branches instead.",
                            target.label()
                        ));
                    }
                }
            }
            return Ok(undo);
        }
        replays.remove(&counter);
        counter -= 1;
    }
    Err(if redo {
        "Nothing to redo"
    } else {
        "Nothing to undo"
    }
    .into())
}

// ---------------------------------------------------------------------------------------
// History rewriting

/// What a History key does to the selected commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Edit {
    /// Into the commit below it (its parent), messages combined.
    Squash,
    /// Into the commit below it, keeping that commit's message.
    Fixup,
    Drop,
    /// Stop the rebase at the commit.
    Edit,
    Reword(String),
    /// Swap with the commit below (older).
    MoveDown,
    /// Swap with the commit above (newer).
    MoveUp,
    /// Fold the staged changes into the commit (a `fixup!` commit, then autosquash).
    Amend,
}

impl Edit {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Squash => "Squash into the commit below",
            Self::Fixup => "Fixup into the commit below",
            Self::Drop => "Drop",
            Self::Edit => "Edit (stop the rebase here)",
            Self::Reword(_) => "Reword",
            Self::MoveDown => "Move down",
            Self::MoveUp => "Move up",
            Self::Amend => "Amend with the staged changes",
        }
    }
}

/// A rebase of the current checkout that rewrites one commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rewrite {
    pub edit: Edit,
    pub(crate) checkout: CheckoutTarget,
    pub target: Oid,
    /// `1a2b3c4 subject` of the selected commit.
    pub commit: String,
    /// The other commit a squash, fixup or move involves.
    pub other: Option<String>,
    /// The commit the rebase replays onto; `None` rewrites from the root.
    pub base: Option<Oid>,
    /// The todo, oldest first, as `pick <hash>` lines.
    pub todo: Vec<String>,
    /// For a sheet: the todo with short hashes and subjects.
    pub steps: Vec<String>,
}

fn line(commit: &git2::Commit) -> String {
    format!("{} {}", short(commit.id()), summary(commit))
}

/// The label for an exact ref selected in the History pane.
pub(crate) fn ref_label(reference: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .into_iter()
        .find_map(|prefix| reference.strip_prefix(prefix))
        .unwrap_or(reference)
}

fn checked_out_history(repository: &Repository, reference: &str) -> Result<CheckoutTarget, String> {
    let head = repository.find_reference("HEAD").map_err(failed)?;
    if reference == "HEAD"
        && let Some(tip) = head.target()
    {
        return Ok(CheckoutTarget::Detached(tip));
    }
    if let Some(name) = head
        .symbolic_target()
        .filter(|name| name.starts_with("refs/heads/"))
        && (reference == "HEAD" || name == reference)
    {
        let tip =
            CheckoutTarget::local_tip(repository, name).ok_or("The branch has no commits yet")?;
        return Ok(CheckoutTarget::Branch {
            reference: name.to_owned(),
            tip,
        });
    }
    if let Some(branch) = reference.strip_prefix("refs/heads/") {
        Err(format!("Check out {branch} to change its history."))
    } else {
        Err("Check out a local branch to change its history.".into())
    }
}

pub(crate) fn history_checkout(
    root: &Path,
    reference: &str,
    selection: Option<Oid>,
) -> Result<CheckoutTarget, String> {
    let repository = open(root)?;
    let checkout = checked_out_history(&repository, reference)?;
    if let Some(selection) = selection {
        check_detached_selection(&repository, &checkout, selection)?;
    }
    Ok(checkout)
}

fn check_detached_selection(
    repository: &Repository,
    checkout: &CheckoutTarget,
    selection: Oid,
) -> Result<(), String> {
    if checkout.reference().is_none()
        && selection != checkout.tip()
        && !repository
            .graph_descendant_of(checkout.tip(), selection)
            .map_err(failed)?
    {
        return Err(COMMIT_OUTSIDE_CHECKOUT.into());
    }
    Ok(())
}

/// Plans `edit` on `target` in the displayed history. The reference must be the
/// checked-out branch or default History, with no merges in the rewrite.
pub fn rewrite(root: &Path, reference: &str, target: Oid, edit: Edit) -> Result<Rewrite, String> {
    let repository = open(root)?;
    settled(&repository)?;
    let checkout = checked_out_history(&repository, reference)?;
    check_detached_selection(&repository, &checkout, target)?;
    let head = repository
        .find_commit(checkout.tip())
        .map_err(|_| "The branch has no commits yet")?;
    // Newest first, HEAD down to the target.
    let mut chain = Vec::new();
    let mut at = Some(head);
    while let Some(commit) = at {
        if commit.parent_count() > 1 {
            return Err(format!(
                "{} is a merge commit: rewriting across merges is not supported",
                short(commit.id())
            ));
        }
        let found = commit.id() == target;
        at = commit.parent(0).ok();
        chain.push(commit);
        if found {
            break;
        }
        if chain.len() > 100_000 {
            break;
        }
    }
    if chain.last().map(|commit| commit.id()) != Some(target) {
        return Err(COMMIT_OUTSIDE_CHECKOUT.into());
    }
    let index = chain.len() - 1;
    let selected = &chain[index];
    let parent = selected.parent(0).ok();
    if parent
        .as_ref()
        .is_some_and(|parent| parent.parent_count() > 1)
    {
        return Err(
            "The commit below is a merge commit: rewriting across merges is not supported".into(),
        );
    }
    let reaches_parent = matches!(edit, Edit::Squash | Edit::Fixup | Edit::MoveDown);
    if reaches_parent && parent.is_none() {
        return Err("The first commit has no commit below it".into());
    }
    if edit == Edit::MoveUp && index == 0 {
        return Err("The newest commit has no commit above it".into());
    }
    if edit == Edit::Amend {
        let staged = chain[0]
            .tree()
            .and_then(|tree| repository.diff_tree_to_index(Some(&tree), None, None))
            .map_err(failed)?;
        if staged.deltas().count() == 0 {
            return Err("Nothing staged · s stages the selected change".into());
        }
    }
    // The commits the rebase replays, oldest first.
    let mut commits: Vec<&git2::Commit> = chain.iter().rev().collect();
    let base = if reaches_parent {
        let parent = parent.as_ref().expect("checked above");
        commits.insert(0, parent);
        parent.parent(0).ok().map(|commit| commit.id())
    } else {
        parent.as_ref().map(|commit| commit.id())
    };
    let at = if reaches_parent { 1 } else { 0 };
    let mut verbs = vec!["pick"; commits.len()];
    match &edit {
        Edit::Squash => verbs[at] = "squash",
        Edit::Fixup => verbs[at] = "fixup",
        Edit::Drop => verbs[at] = "drop",
        Edit::Edit => verbs[at] = "edit",
        Edit::Reword(_) => verbs[at] = "reword",
        Edit::MoveDown => commits.swap(0, 1),
        Edit::MoveUp => commits.swap(0, 1),
        Edit::Amend => {}
    }
    let other = match &edit {
        Edit::Squash | Edit::Fixup | Edit::MoveDown => parent.as_ref().map(line),
        Edit::MoveUp => Some(line(&chain[index - 1])),
        _ => None,
    };
    let todo = commits
        .iter()
        .zip(&verbs)
        .map(|(commit, verb)| format!("{verb} {}", commit.id()))
        .collect();
    let steps = commits
        .iter()
        .zip(&verbs)
        .map(|(commit, verb)| format!("{verb} {}", line(commit)))
        .collect();
    Ok(Rewrite {
        commit: line(selected),
        edit,
        checkout,
        target,
        other,
        base,
        todo,
        steps,
    })
}

impl Rewrite {
    fn rebase(&self, autosquash: bool) -> Run {
        let mut args: Vec<OsString> = [
            "-c",
            "rebase.updateRefs=false",
            "rebase",
            "-i",
            "--autostash",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        args.push(
            if autosquash {
                "--autosquash"
            } else {
                "--no-autosquash"
            }
            .into(),
        );
        args.push(match self.base {
            Some(base) => base.to_string().into(),
            None => "--root".into(),
        });
        Run::new(args)
    }

    /// The commands, in order: a rebase whose todo Dusty Git writes through
    /// `GIT_SEQUENCE_EDITOR`; an amend first commits the staged changes as `fixup!`.
    pub fn runs(&self) -> Vec<Run> {
        let editor = |run: Run| {
            let message = match &self.edit {
                Edit::Reword(message) => Some(message.clone()),
                _ => None,
            };
            match message {
                // The sequencer opens the editor once, for the reworded commit's message.
                Some(message) => run
                    .with("GIT_EDITOR", "printf '%s\\n' \"$DUSTY_MESSAGE\" >")
                    .with("DUSTY_MESSAGE", message),
                None => run.with("GIT_EDITOR", "true"),
            }
        };
        if self.edit == Edit::Amend {
            return vec![
                Run::new(["commit".into(), format!("--fixup={}", self.target)])
                    .with("GIT_EDITOR", "true")
                    .with("GIT_REFLOG_ACTION", "commit")
                    .checked(CheckoutCheck::Fixup(self.checkout.clone())),
                self.rebase(true)
                    .checked(CheckoutCheck::Autosquash(self.checkout.clone()))
                    .with("GIT_SEQUENCE_EDITOR", "true")
                    .with("GIT_EDITOR", "true"),
            ];
        }
        let mut todo = self.todo.join("\n");
        todo.push('\n');
        vec![editor(
            self.rebase(false)
                .checked(CheckoutCheck::Rewrite(self.checkout.clone()))
                .with("GIT_SEQUENCE_EDITOR", "printf '%s' \"$DUSTY_TODO\" >")
                .with("DUSTY_TODO", todo),
        )]
    }

    /// The notice once the rebase finished.
    pub fn done(&self) -> String {
        let commit = &self.commit;
        match &self.edit {
            Edit::Squash => format!("Squashed {commit} into the commit below"),
            Edit::Fixup => format!("Fixed {commit} up into the commit below"),
            Edit::Drop => format!("Dropped {commit}"),
            Edit::Edit => format!("Stopped at {commit} · amend it, then C continues, X aborts"),
            Edit::Reword(_) => format!("Reworded {commit}"),
            Edit::MoveDown => format!("Moved {commit} down"),
            Edit::MoveUp => format!("Moved {commit} up"),
            Edit::Amend => format!("Amended {commit} with the staged changes"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Tag push

/// The remote a tag push goes to first: the branch's upstream remote, `origin`, or the only
/// remote.
pub fn default_remote(root: &Path) -> Result<String, String> {
    let repository = open(root)?;
    let remotes = repository.remotes().map_err(failed)?;
    let remotes = remotes
        .iter()
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if remotes.is_empty() {
        return Err("No remotes to push the tag to".into());
    }
    let upstream = branch(&repository)?.and_then(|branch| {
        repository
            .branch_upstream_remote(&format!("refs/heads/{branch}"))
            .ok()
            .and_then(|remote| remote.as_str().map(str::to_owned))
    });
    Ok(upstream
        .filter(|remote| remotes.contains(remote))
        .or_else(|| remotes.iter().find(|remote| *remote == "origin").cloned())
        .unwrap_or_else(|| remotes[0].clone()))
}

/// `git push <remote> refs/tags/<tag>:refs/tags/<tag>`: the tag only, never a branch.
pub fn push_tag(remote: &str, tag: &str) -> Result<Run, String> {
    if remote.is_empty() || remote.starts_with('-') {
        return Err("Choose a remote".into());
    }
    let reference = format!("refs/tags/{tag}");
    if tag.starts_with('-') || !git2::Reference::is_valid_name(&reference) {
        return Err(format!("{tag} is not a valid tag name"));
    }
    Ok(Run::new([
        "push".into(),
        "--progress".into(),
        "--".into(),
        remote.to_owned(),
        format!("{reference}:{reference}"),
    ]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    pub(crate) fn git(root: &Path, args: &[&str]) -> String {
        git_env(root, args, &[])
    }

    fn git_env(root: &Path, args: &[&str], env: &[(String, OsString)]) -> String {
        let mut command = Command::new("git");
        command
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_REFLOG_ACTION");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    fn run(root: &Path, run: &Run) {
        if let Some(check) = &run.check {
            check.verify(root).unwrap();
        }
        let args = run
            .args
            .iter()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        git_env(root, &args, &run.env);
    }

    pub(crate) fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--template=", "-b", "main"]);
        git(root, &["config", "user.name", "History"]);
        git(root, &["config", "user.email", "history@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "core.autocrlf", "false"]);
        dir
    }

    pub(crate) fn commit(root: &Path, name: &str) -> Oid {
        fs::write(root.join(name), format!("{name}\n")).unwrap();
        git(root, &["add", "--", name]);
        git(root, &["commit", "-q", "-m", name]);
        head(root)
    }

    pub(crate) fn head(root: &Path) -> Oid {
        Oid::from_str(&git(root, &["rev-parse", "HEAD"])).unwrap()
    }

    fn subjects(root: &Path) -> String {
        git(root, &["log", "--format=%s"]).replace('\n', " ")
    }

    fn unstage_fixture(root: &Path, file: &str) {
        use crate::mutation::{GitMutationRequestId, git_head_id, git_index_fingerprint};
        crate::mutation::unstage_paths(
            GitMutationRequestId(1),
            root,
            &[Path::new(file).to_path_buf()].into(),
            git_index_fingerprint(root).unwrap(),
            git_head_id(root).unwrap(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
    }

    #[test]
    fn undo_and_redo_interleave_commits_and_unstage() {
        let dir = repo();
        let root = dir.path();
        let base = commit(root, "edit.txt");
        fs::write(root.join("edit.txt"), "staged only\n").unwrap();
        git(root, &["add", "edit.txt"]);
        fs::write(root.join("edit.txt"), "working copy\n").unwrap();
        unstage_fixture(root, "edit.txt");
        let tip = commit(root, "tip");

        let commit_undo = undo(root, false).unwrap();
        assert_eq!(
            commit_undo.moved,
            Move::Commit {
                from: base,
                to: tip
            }
        );
        run(root, &commit_undo.run().unwrap());
        let index_undo = undo(root, false).unwrap();
        assert!(matches!(index_undo.moved, Move::Index { .. }));
        index_undo.restore_index(root).unwrap();
        assert_eq!(git(root, &["show", ":edit.txt"]), "staged only");
        assert_eq!(head(root), base);
        assert_eq!(
            fs::read_to_string(root.join("edit.txt")).unwrap(),
            "working copy\n"
        );

        let index_redo = undo(root, true).unwrap();
        assert_eq!(index_redo.moved, index_undo.moved);
        index_redo.restore_index(root).unwrap();
        assert_eq!(git(root, &["show", ":edit.txt"]), "edit.txt");
        let commit_redo = undo(root, true).unwrap();
        assert_eq!(commit_redo.moved, commit_undo.moved);
        run(root, &commit_redo.run().unwrap());
        assert_eq!(head(root), tip);
    }

    #[test]
    fn redo_unstage_after_undoing_and_redoing_an_unrelated_commit() {
        let dir = repo();
        let root = dir.path();
        let base = commit(root, "edit.txt");
        let tip = commit(root, "tip");
        fs::write(root.join("edit.txt"), "staged only\n").unwrap();
        git(root, &["add", "edit.txt"]);
        fs::write(root.join("edit.txt"), "working copy\n").unwrap();
        unstage_fixture(root, "edit.txt");

        let index_undo = undo(root, false).unwrap();
        index_undo.restore_index(root).unwrap();
        for (redo, expected_head) in [(false, base), (true, tip)] {
            let plan = undo(root, redo).unwrap();
            assert_eq!(
                plan.moved,
                Move::Commit {
                    from: base,
                    to: tip
                }
            );
            run(root, &plan.run().unwrap());
            assert_eq!(head(root), expected_head);
            assert_eq!(git(root, &["show", ":edit.txt"]), "staged only");
        }
        let index_redo = undo(root, true).unwrap();
        assert_eq!(index_redo.moved, index_undo.moved);
        index_redo.restore_index(root).unwrap();
        assert_eq!(head(root), tip);
        assert_eq!(git(root, &["show", ":edit.txt"]), "edit.txt");
        assert_eq!(fs::read(root.join("edit.txt")).unwrap(), b"working copy\n");
    }

    #[test]
    fn bulk_unstage_undo_works_before_the_first_commit_and_in_a_linked_worktree() {
        let dir = repo();
        let root = dir.path();
        fs::write(root.join("first.txt"), "staged only\n").unwrap();
        git(root, &["add", "first.txt"]);
        fs::write(root.join("first.txt"), "working copy\n").unwrap();
        unstage_fixture(root, "first.txt");
        assert!(git(root, &["ls-files", "--", "first.txt"]).is_empty());
        undo(root, false).unwrap().restore_index(root).unwrap();
        assert_eq!(git(root, &["show", ":first.txt"]), "staged only");
        assert_eq!(
            fs::read_to_string(root.join("first.txt")).unwrap(),
            "working copy\n"
        );

        git(root, &["commit", "-m", "First"]);
        let linked = root.join("linked");
        git(
            root,
            &["worktree", "add", "-b", "other", linked.to_str().unwrap()],
        );
        fs::write(linked.join("first.txt"), "linked only\n").unwrap();
        git(&linked, &["add", "first.txt"]);
        fs::write(linked.join("first.txt"), "linked working copy\n").unwrap();
        unstage_fixture(&linked, "first.txt");
        assert_eq!(git(&linked, &["show", ":first.txt"]), "staged only");
        undo(&linked, false)
            .unwrap()
            .restore_index(&linked)
            .unwrap();
        assert_eq!(git(&linked, &["show", ":first.txt"]), "linked only");
        assert_eq!(git(root, &["show", ":first.txt"]), "staged only");
    }

    #[test]
    fn restaging_does_not_block_older_undo_or_poison_redo() {
        let dir = repo();
        let root = dir.path();
        let base = commit(root, "edit.txt");
        let tip = commit(root, "tip");
        fs::write(root.join("edit.txt"), "new edit\n").unwrap();
        git(root, &["add", "edit.txt"]);
        unstage_fixture(root, "edit.txt");
        let recovery = undo(root, false).unwrap();
        let Move::Index { record, .. } = recovery.moved else {
            panic!("expected an index action");
        };
        git(root, &["add", "edit.txt"]);

        let plan = undo(root, false).unwrap();
        assert_eq!(
            plan.moved,
            Move::Commit {
                from: base,
                to: tip
            }
        );
        assert_eq!(git(root, &["show", ":edit.txt"]), "new edit");
        assert!(Repository::open(root).unwrap().find_tree(record).is_ok());
        run(root, &plan.run().unwrap());
        assert_eq!(head(root), base);
        assert_eq!(git(root, &["show", ":edit.txt"]), "new edit");

        let redo = undo(root, true).unwrap();
        assert_eq!(redo.moved, plan.moved);
        run(root, &redo.run().unwrap());
        assert_eq!(head(root), tip);
        assert_eq!(git(root, &["show", ":edit.txt"]), "new edit");
        assert_eq!(undo(root, false).unwrap().moved, plan.moved);
        assert_eq!(
            fs::read_to_string(root.join("edit.txt")).unwrap(),
            "new edit\n"
        );
    }

    #[test]
    fn undo_walks_back_one_action_at_a_time_and_redo_walks_forward() {
        let dir = repo();
        let root = dir.path();
        let a = commit(root, "a");
        let b = commit(root, "b");
        let c = commit(root, "c");
        let first = undo(root, false).unwrap();
        assert_eq!(first.moved, Move::Commit { from: b, to: c });
        assert_eq!(first.entry, "commit: c");
        assert_eq!(first.head(), (short(c), short(b)));
        assert!(matches!(undo(root, true), Err(message) if message == "Nothing to redo"));
        run(root, &first.run().unwrap());
        assert_eq!(head(root), b);
        let second = undo(root, false).unwrap();
        assert_eq!(second.moved, Move::Commit { from: a, to: b });
        run(root, &second.run().unwrap());
        assert_eq!(head(root), a);
        assert!(
            undo(root, false).is_err(),
            "the first commit has nothing before it"
        );
        let again = undo(root, true).unwrap();
        assert_eq!(again.head(), (short(a), short(b)));
        run(root, &again.run().unwrap());
        assert_eq!(head(root), b);
        run(root, &undo(root, true).unwrap().run().unwrap());
        assert_eq!(head(root), c);
        assert!(undo(root, true).is_err());
        // Undo after the redos starts from the newest action again.
        run(root, &undo(root, false).unwrap().run().unwrap());
        assert_eq!(head(root), b);
    }

    #[test]
    fn undo_and_redo_refuse_to_drop_index_only_content() {
        let dir = repo();
        let root = dir.path();
        let base = commit(root, "edit.txt");
        fs::write(root.join("edit.txt"), "tip edit\n").unwrap();
        fs::write(root.join("new.txt"), "tip new file\n").unwrap();
        git(root, &["add", "edit.txt", "new.txt"]);
        git(root, &["commit", "-m", "Touch both paths"]);
        let tip = head(root);
        // Capture the redo plan's source while the checkout is still clean.
        run(root, &undo(root, false).unwrap().run().unwrap());

        for (redo, current, target) in [(true, base, tip), (false, tip, base)] {
            let approved = undo(root, redo).unwrap().run().unwrap();
            let clean_edit = git(root, &["show", "HEAD:edit.txt"]) + "\n";
            fs::write(root.join("edit.txt"), "staged only\n").unwrap();
            git(root, &["add", "edit.txt"]);
            fs::write(root.join("edit.txt"), &clean_edit).unwrap();
            let refusal =
                "Undo would drop staged changes in edit.txt. Commit or unstage them first.";
            assert_eq!(undo(root, redo).unwrap_err(), refusal);
            assert_eq!(approved.check.unwrap().verify(root).unwrap_err(), refusal);

            fs::write(root.join("new.txt"), "new staged file\n").unwrap();
            git(root, &["add", "new.txt"]);
            git(root, &["update-index", "--assume-unchanged", "new.txt"]);
            fs::remove_file(root.join("new.txt")).unwrap();
            let index = fs::read(root.join(".git/index")).unwrap();
            assert_eq!(
                undo(root, redo).unwrap_err(),
                "Undo would drop staged changes in edit.txt (and 1 more). Commit or unstage them first."
            );
            assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
            assert_eq!(git(root, &["show", ":edit.txt"]), "staged only");
            assert_eq!(git(root, &["show", ":new.txt"]), "new staged file");
            assert_eq!(
                fs::read_to_string(root.join("edit.txt")).unwrap(),
                clean_edit
            );
            assert!(!root.join("new.txt").exists());
            assert_eq!(head(root), current);

            // Resolve the collision explicitly before retrying the commit move.
            git(root, &["reset", "--hard", "HEAD"]);
            run(root, &undo(root, redo).unwrap().run().unwrap());
            assert_eq!(head(root), target);
        }
    }

    #[test]
    fn undo_targets_the_commit_before_a_native_amend_and_refuses_an_unknown_move() {
        let dir = repo();
        let root = dir.path();
        commit(root, "base");
        let before = commit(root, "edit.txt");
        fs::write(root.join("edit.txt"), "amended\n").unwrap();
        git(root, &["add", "edit.txt"]);
        // Publish the rewritten commit with the message used by the native rewrite.
        let tree = git(root, &["write-tree"]);
        let amended = git(
            root,
            &["commit-tree", &tree, "-p", "HEAD^", "-m", "amended"],
        );
        git(
            root,
            &[
                "update-ref",
                "-m",
                crate::rewrite::REFLOG_ACTION,
                "HEAD",
                &amended,
            ],
        );
        let amended = head(root);

        let plan = undo(root, false).unwrap();
        assert_eq!(
            plan.moved,
            Move::Commit {
                from: before,
                to: amended
            }
        );
        run(root, &plan.run().unwrap());
        assert_eq!(head(root), before);

        git(
            root,
            &[
                "update-ref",
                "-m",
                "unknown move",
                "HEAD",
                &amended.to_string(),
            ],
        );
        assert_eq!(
            undo(root, false).unwrap_err(),
            "Undo can't follow the last change to HEAD."
        );
        assert_eq!(head(root), amended);
    }

    #[test]
    fn undo_switches_back_and_treats_a_rebase_as_one_action() {
        let dir = repo();
        let root = dir.path();
        let a = commit(root, "a");
        git(root, &["switch", "-q", "-c", "topic"]);
        let undo_switch = undo(root, false).unwrap();
        assert_eq!(
            undo_switch.moved,
            Move::Checkout {
                from: CheckoutTarget::Branch {
                    reference: "refs/heads/main".into(),
                    tip: a
                },
                to: CheckoutTarget::Branch {
                    reference: "refs/heads/topic".into(),
                    tip: a
                },
            }
        );
        run(root, &undo_switch.run().unwrap());
        assert_eq!(git(root, &["branch", "--show-current"]), "main");
        run(root, &undo(root, true).unwrap().run().unwrap());
        assert_eq!(git(root, &["branch", "--show-current"]), "topic");
        commit(root, "b");
        let before = commit(root, "c");
        let plan = rewrite(root, "refs/heads/topic", before, Edit::MoveDown).unwrap();
        for step in plan.runs() {
            run(root, &step);
        }
        assert_eq!(subjects(root), "b c a");
        let rebase = undo(root, false).unwrap();
        assert!(rebase.entry.starts_with("rebase"), "{}", rebase.entry);
        run(root, &rebase.run().unwrap());
        assert_eq!(head(root), before);
        assert_eq!(subjects(root), "c b a");
    }

    #[test]
    fn checkout_undo_and_redo_require_the_recorded_branch_tips() {
        let dir = repo();
        let root = dir.path();
        let base = commit(root, "a");
        git(root, &["branch", "topic"]);
        let main = commit(root, "b");
        git(root, &["switch", "-q", "topic"]);

        git(root, &["branch", "-f", "main", &base.to_string()]);
        assert_eq!(
            undo(root, false).unwrap_err(),
            "main has moved since you left it. Check it out from Branches instead."
        );
        assert_eq!(head(root), base);
        assert_eq!(git(root, &["branch", "--show-current"]), "topic");

        git(root, &["branch", "-f", "main", &main.to_string()]);
        let plan = undo(root, false).unwrap();
        assert_eq!(plan.checkout_destination().unwrap().tip(), main);
        run(root, &plan.run().unwrap());
        assert_eq!(head(root), main);
        assert_eq!(git(root, &["branch", "--show-current"]), "main");

        git(root, &["branch", "-f", "topic", &main.to_string()]);
        assert_eq!(
            undo(root, true).unwrap_err(),
            "topic has moved since you left it. Check it out from Branches instead."
        );
        assert_eq!(head(root), main);
        git(root, &["branch", "-f", "topic", &base.to_string()]);
        run(root, &undo(root, true).unwrap().run().unwrap());
        assert_eq!(head(root), base);
        assert_eq!(git(root, &["branch", "--show-current"]), "topic");
    }

    #[test]
    fn checkout_undo_never_substitutes_a_same_named_tag_for_the_branch() {
        let dir = repo();
        let root = dir.path();
        let original = commit(root, "a");
        git(root, &["tag", "main"]);
        git(root, &["switch", "-q", "-c", "topic"]);
        let approved = undo(root, false).unwrap().run().unwrap();
        let check = approved.check.as_ref().unwrap();
        check.verify(root).unwrap();

        git(root, &["branch", "-D", "main"]);
        assert_eq!(
            git(root, &["rev-parse", "refs/tags/main"]),
            original.to_string()
        );
        assert_eq!(
            undo(root, false).unwrap_err(),
            "main has moved since you left it. Check it out from Branches instead."
        );
        assert_eq!(check.verify(root).unwrap_err(), CHECKOUT_CHANGED);
        assert_eq!(git(root, &["branch", "--show-current"]), "topic");
    }

    #[test]
    fn checkout_undo_and_redo_remember_a_named_detachment() {
        let dir = repo();
        let root = dir.path();
        let tagged = commit(root, "a");
        git(root, &["tag", "v1.0"]);
        let main = commit(root, "b");
        git(root, &["checkout", "-q", "v1.0"]);
        let detached = commit(root, "detached");

        for _ in 0..2 {
            run(root, &undo(root, false).unwrap().run().unwrap());
            assert_eq!(git(root, &["branch", "--show-current"]), "");
            assert_eq!(head(root), tagged);

            run(root, &undo(root, false).unwrap().run().unwrap());
            assert_eq!(git(root, &["branch", "--show-current"]), "main");
            assert_eq!(head(root), main);

            // Redo the approved detachment, not a new resolution of the tag.
            git(root, &["tag", "-f", "v1.0", &main.to_string()]);
            let plan = undo(root, true).unwrap();
            assert_eq!(
                plan.checkout_destination(),
                Some(&CheckoutTarget::Detached(tagged))
            );
            run(root, &plan.run().unwrap());
            assert_eq!(git(root, &["branch", "--show-current"]), "");
            assert_eq!(head(root), tagged);

            run(root, &undo(root, true).unwrap().run().unwrap());
            assert_eq!(git(root, &["branch", "--show-current"]), "");
            assert_eq!(head(root), detached);
        }
    }

    #[test]
    fn detached_rewrite_checks_ancestry_before_refusing_merges() {
        let dir = repo();
        let root = dir.path();
        let base = commit(root, "base");
        git(root, &["switch", "-q", "-c", "outside"]);
        let outside = commit(root, "outside");
        git(root, &["switch", "-q", "-c", "merged", &base.to_string()]);
        let merged = commit(root, "merged");
        git(root, &["switch", "-q", "main"]);
        commit(root, "main-change");
        git(root, &["merge", "--no-ff", "-m", "merge topic", "merged"]);
        let merge = head(root);
        git(root, &["switch", "-q", "--detach", &merge.to_string()]);

        assert_eq!(
            rewrite(root, "HEAD", outside, Edit::Drop).unwrap_err(),
            COMMIT_OUTSIDE_CHECKOUT
        );
        assert_eq!(
            rewrite(root, "HEAD", merged, Edit::Drop).unwrap_err(),
            format!(
                "{} is a merge commit: rewriting across merges is not supported",
                short(merge)
            )
        );
    }

    #[test]
    fn dropping_a_commit_requires_the_displayed_branch_to_be_checked_out() {
        let dir = repo();
        let root = dir.path();
        commit(root, "a");
        let lower = commit(root, "b");
        git(root, &["branch", "lower"]);
        let main = commit(root, "c");

        assert_eq!(
            rewrite(root, "refs/heads/lower", lower, Edit::Drop).unwrap_err(),
            "Check out lower to change its history."
        );
        assert_eq!(
            git(root, &["rev-parse", "refs/heads/main"]),
            main.to_string()
        );

        let plan = rewrite(root, "refs/heads/main", lower, Edit::Drop).unwrap();
        for command in plan.runs() {
            run(root, &command);
        }
        assert_eq!(subjects(root), "c a");
        assert_eq!(
            git(root, &["rev-parse", "refs/heads/lower"]),
            lower.to_string()
        );
    }

    #[test]
    fn each_history_edit_rewrites_the_selected_commit() {
        let dir = repo();
        let root = dir.path();
        commit(root, "a");
        let b = commit(root, "b");
        commit(root, "c");
        let cases: [(Edit, &str); 6] = [
            (Edit::Drop, "c a"),
            (Edit::Squash, "c a"),
            (Edit::Fixup, "c a"),
            (Edit::MoveDown, "c a b"),
            (Edit::MoveUp, "b c a"),
            (Edit::Reword("bee\n\nbody".into()), "c bee a"),
        ];
        let start = head(root);
        for (edit, expected) in cases {
            git(root, &["reset", "-q", "--hard", &start.to_string()]);
            let plan = rewrite(root, "refs/heads/main", b, edit.clone()).unwrap();
            for step in plan.runs() {
                run(root, &step);
            }
            assert_eq!(subjects(root), expected, "{edit:?}");
            if edit == Edit::Squash {
                let message = git(root, &["log", "-1", "--format=%B", "HEAD~1"]);
                assert!(message.contains('a') && message.contains('b'), "{message}");
            }
        }
        git(root, &["reset", "-q", "--hard", &start.to_string()]);
        fs::write(root.join("a"), "amended\n").unwrap();
        git(root, &["add", "a"]);
        fs::write(root.join("c"), "unstaged\n").unwrap();
        let plan = rewrite(root, "refs/heads/main", b, Edit::Amend).unwrap();
        for step in plan.runs() {
            run(root, &step);
        }
        assert_eq!(subjects(root), "c b a");
        assert_eq!(git(root, &["show", "HEAD~1:a"]), "amended");
        assert_eq!(fs::read_to_string(root.join("c")).unwrap(), "unstaged\n");
        assert!(rewrite(root, "refs/heads/main", head(root), Edit::MoveUp).is_err());
    }

    #[test]
    fn the_root_commit_rebases_from_the_root_and_merges_are_refused() {
        let dir = repo();
        let root = dir.path();
        let a = commit(root, "a");
        let b = commit(root, "b");
        let plan = rewrite(root, "refs/heads/main", b, Edit::Squash).unwrap();
        assert_eq!(plan.base, None);
        assert!(plan.runs()[0].args.contains(&OsString::from("--root")));
        assert!(rewrite(root, "refs/heads/main", a, Edit::Squash).is_err());
        git(root, &["switch", "-q", "-c", "side", &a.to_string()]);
        commit(root, "side");
        git(root, &["switch", "-q", "main"]);
        git(root, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        assert!(
            rewrite(root, "refs/heads/main", b, Edit::Drop)
                .unwrap_err()
                .contains("merge")
        );
    }

    #[test]
    fn push_destination_expands_only_the_named_remote_tracking_branch() {
        let dir = repo();
        let root = dir.path();
        let tip = commit(root, "base").to_string();
        git(root, &["remote", "add", "origin", "."]);
        git(root, &["update-ref", "refs/remotes/origin/review", &tip]);
        git(root, &["update-ref", "refs/tags/release", &tip]);
        git(root, &["update-ref", "refs/remotes/origin/shared", &tip]);
        git(root, &["update-ref", "refs/tags/shared", &tip]);
        git(root, &["update-ref", "refs/remotes/other/unknown", &tip]);

        for (configured, expected) in [
            ("refs/heads/explicit", "refs/heads/explicit"),
            ("refs/tags/release", "refs/tags/release"),
            ("review", "refs/heads/review"),
            ("release", "release"),
            ("shared", "refs/heads/shared"),
            ("unknown", "unknown"),
        ] {
            git(
                root,
                &[
                    "config",
                    "remote.origin.push",
                    &format!("HEAD:{configured}"),
                ],
            );
            let plan = push(root).unwrap();
            assert_eq!(plan.target.reference, expected, "{configured}");
            assert_eq!(
                plan.run().args.last(),
                Some(&OsString::from(format!("refs/heads/main:{expected}"))),
                "{configured}",
            );
        }
    }

    #[test]
    fn push_destination_ignores_a_local_tag_when_the_remote_has_a_branch() {
        let dir = repo();
        let root = dir.path();
        commit(root, "base");
        let remote = tempfile::tempdir().unwrap();
        git(
            remote.path(),
            &["init", "--template=", "--bare", "-b", "main"],
        );
        let url = remote.path().to_str().unwrap();
        // Seed the remote branch before adding origin so no tracking ref is recorded.
        git(root, &["push", url, "HEAD:refs/heads/review"]);
        git(root, &["remote", "add", "origin", url]);
        git(root, &["config", "remote.origin.push", "HEAD:review"]);
        git(root, &["tag", "review"]);
        let tip = commit(root, "next");

        let plan = push(root).unwrap();
        assert_eq!(plan.target.reference, "review");
        run(root, &plan.run());
        assert_eq!(
            git(remote.path(), &["rev-parse", "refs/heads/review"]),
            tip.to_string(),
        );
        assert_eq!(
            git(remote.path(), &["for-each-ref", "refs/tags/review"]),
            "",
            "a local-only tag must not change the remote destination",
        );
    }

    #[test]
    fn generic_push_pins_the_configured_remote_and_branch() {
        let directory = repo();
        let root = directory.path();
        commit(root, "one");
        git(root, &["remote", "add", "origin", "."]);
        git(root, &["remote", "add", "review", "."]);
        git(root, &["config", "branch.main.remote", "origin"]);
        git(root, &["config", "branch.main.merge", "refs/heads/main"]);
        assert_eq!(
            push(root).unwrap().run().display(),
            "git push --progress --no-follow-tags -- origin refs/heads/main:refs/heads/main"
        );
        git(root, &["config", "branch.main.pushRemote", "review"]);
        git(
            root,
            &[
                "config",
                "remote.review.push",
                "refs/heads/main:refs/heads/approved",
            ],
        );

        let run = push(root).unwrap().run();
        assert_eq!(
            run.display(),
            "git push --progress --no-follow-tags -- review refs/heads/main:refs/heads/approved"
        );
        let check = run.check.unwrap();
        assert!(check.verify(root).is_ok());
        git(root, &["config", "branch.main.pushRemote", "origin"]);
        assert_eq!(check.verify(root).unwrap_err(), NETWORK_TARGET_CHANGED);
    }

    #[test]
    fn pull_reads_the_branch_then_the_pull_configuration() {
        let dir = repo();
        let root = dir.path();
        commit(root, "a");
        assert!(pull(root).unwrap_err().contains("no upstream"));
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            root,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        git(root, &["push", "-q", "-u", "origin", "main"]);
        let plan = pull(root).unwrap();
        assert_eq!(
            (
                plan.target.branch.as_str(),
                plan.target.other_branch().as_str()
            ),
            ("main", "origin/main")
        );
        assert_eq!(plan.mode, PullMode::Merge);
        git(root, &["config", "pull.ff", "only"]);
        assert_eq!(pull(root).unwrap().mode, PullMode::FastForward);
        git(root, &["config", "pull.rebase", "true"]);
        assert_eq!(pull(root).unwrap().mode, PullMode::Rebase);
        git(root, &["config", "branch.main.rebase", "false"]);
        assert_eq!(pull(root).unwrap().mode, PullMode::FastForward);
        assert_eq!(default_remote(root).unwrap(), "origin");
        assert!(push_tag("origin", "-x").is_err());
        assert_eq!(
            push_tag("origin", "v1").unwrap().display(),
            "git push --progress -- origin refs/tags/v1:refs/tags/v1"
        );
    }
}
