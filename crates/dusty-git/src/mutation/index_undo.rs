//! Record index-only changes in the same undo order as HEAD changes.
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use git2::{Index, Oid, Repository, Signature};

use super::{GitHeadId, GitIndexFingerprint, GitMutationError, IndexWrite};
use super::{git_head_id, index_fingerprint};
use crate::history::UNSTAGE_ACTION;

const RECOVERY_REF: &str = "refs/worktree/dusty-unstage";
const READ_UNDO: &str = "Couldn't read the saved staged changes";

fn snapshot(
    repo: &Repository,
    record: Oid,
    before: bool,
) -> Result<(Oid, GitIndexFingerprint), String> {
    let tree = repo.find_tree(record).map_err(|_| READ_UNDO)?;
    let entry = tree
        .get_name(if before { "before" } else { "after" })
        .ok_or(READ_UNDO)?;
    let hashes = tree.get_name("fingerprints").ok_or(READ_UNDO)?;
    let hashes = repo.find_blob(hashes.id()).map_err(|_| READ_UNDO)?;
    let bytes = if before {
        hashes.content().get(..32)
    } else {
        hashes.content().get(32..)
    };
    let hash = bytes
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(READ_UNDO)?;
    Ok((entry.id(), GitIndexFingerprint(hash)))
}

/// Keep the exact index (including modes, flags and unrelated conflicts) before and after
/// unstaging. A worktree-local ref and its reflog keep the snapshots and changed blobs alive
/// through Git's garbage collection; the HEAD reflog supplies the order for Undo and Redo.
pub(super) fn commit_unstage(
    repo: &Repository,
    write: &IndexWrite,
    workdir: &Path,
    head: Option<GitHeadId>,
) -> Result<GitIndexFingerprint, GitMutationError> {
    let previous = Index::open(&write.path).map_err(|_| GitMutationError::SaveUnstage)?;
    let next = Index::open(&write.temporary).map_err(|_| GitMutationError::SaveUnstage)?;
    let before = index_fingerprint(&previous);
    let after = index_fingerprint(&next);
    if before == after {
        return Err(GitMutationError::NoChange);
    }

    let save_index = |path: &Path| {
        let bytes = fs::read(path).map_err(|_| GitMutationError::SaveUnstage)?;
        repo.blob(&bytes).map_err(|_| GitMutationError::SaveUnstage)
    };
    let before_blob = save_index(&write.path)?;
    let after_blob = save_index(&write.temporary)?;
    let save = || -> Result<Oid, git2::Error> {
        let blobs = |index: &Index| {
            index
                .iter()
                .filter(|entry| {
                    entry.mode != u32::from(git2::FileMode::Commit) && !entry.id.is_zero()
                })
                .map(|entry| entry.id)
                .collect::<BTreeSet<_>>()
        };
        let mut tree = repo.treebuilder(None)?;
        let mode = i32::from(git2::FileMode::Blob);
        // Git can't see the blob IDs embedded in the raw index. Pin those that won't
        // remain reachable from the index on one side of the operation.
        for id in blobs(&previous).symmetric_difference(&blobs(&next)) {
            tree.insert(id.to_string(), *id, mode)?;
        }
        tree.insert("before", before_blob, mode)?;
        tree.insert("after", after_blob, mode)?;
        tree.insert(
            "fingerprints",
            repo.blob(&[before.0, after.0].concat())?,
            mode,
        )?;
        let record = tree.write()?;
        repo.reference_ensure_log(RECOVERY_REF)?;
        repo.reference(RECOVERY_REF, record, true, "Unstage changes")?;
        Ok(record)
    };
    let record = save().map_err(|_| GitMutationError::SaveUnstage)?;
    let head_oid = head
        .map_or(Ok(Oid::zero()), |id| Oid::from_bytes(&id.0))
        .map_err(|_| GitMutationError::SaveUnstage)?;
    let (_head_lock, mut log) = head_log(repo, head_oid, &format!("{UNSTAGE_ACTION}{record}"))
        .map_err(|_| GitMutationError::SaveUnstage)?;
    // Also check HEAD after preparing the recovery record, before changing the index.
    if git_head_id(workdir)? != head {
        return Err(GitMutationError::StaleHead);
    }
    log.write().map_err(|_| GitMutationError::SaveUnstage)?;
    write.commit()?;
    Ok(after)
}

fn head_log<'a>(
    repo: &'a Repository,
    head: Oid,
    message: &str,
) -> Result<(git2::Transaction<'a>, git2::Reflog), git2::Error> {
    let mut transaction = repo.transaction()?;
    transaction.lock_ref("HEAD")?;
    repo.reference_ensure_log("HEAD")?;
    let mut log = repo.reflog("HEAD")?;
    let signature = repo
        .signature()
        .or_else(|_| Signature::now("Dusty", "dusty@localhost"))?;
    log.append(head, &signature, Some(message))?;
    // A linked worktree can have reflog entries with no message. libgit2's
    // transaction.set_reflog cannot duplicate those entries, but Reflog::write
    // preserves them. Keep HEAD locked until the caller has written the index.
    Ok((transaction, log))
}

pub(crate) fn index_undo_matches(
    repo: &Repository,
    record: Oid,
    redo: bool,
) -> Result<bool, String> {
    let (_, expected) = snapshot(repo, record, redo)?;
    let index = repo.index().map_err(|_| READ_UNDO)?;
    Ok(index_fingerprint(&index) == expected)
}

pub(crate) fn apply_index_undo(
    repo: &Repository,
    record: Oid,
    head: Oid,
    redo: bool,
    action: &str,
) -> Result<(), String> {
    let error = if redo {
        "Couldn't unstage the changes again"
    } else {
        "Couldn't restore the staged changes"
    };
    let write = IndexWrite::start(repo).map_err(|_| error)?;
    if !index_undo_matches(repo, record, redo)? {
        return Err("The staged changes have changed since unstaging".into());
    }
    let (target, _) = snapshot(repo, record, !redo)?;
    let blob = repo.find_blob(target).map_err(|_| READ_UNDO)?;
    let (_head_lock, mut log) = head_log(repo, head, action).map_err(|_| error)?;
    let workdir = repo.workdir().ok_or(error)?;
    let current = git_head_id(workdir).map_err(|_| error)?;
    let expected = GitHeadId::from_oid(&head).filter(|_| !head.is_zero());
    if current != expected {
        return Err("The checked-out commit changed. Close this and try again.".into());
    }
    fs::write(&write.temporary, blob.content()).map_err(|_| error)?;
    // Disk files may have changed since this snapshot; don't trust its cached stat
    // data on the strength of a newly written index timestamp.
    fs::OpenOptions::new()
        .write(true)
        .open(&write.temporary)
        .and_then(|file| file.set_modified(SystemTime::UNIX_EPOCH))
        .map_err(|_| error)?;
    log.write().map_err(|_| error)?;
    write.commit().map_err(|_| error.into())
}
