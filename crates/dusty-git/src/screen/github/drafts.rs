//! The pending review: inline comments held for the next review of a
//! pull request. They are kept under the forge state directory (never in the repository),
//! per host, account, repository and pull request, so a restart keeps them until the review
//! is sent or they are deleted. `V` in the Files reader marks the first line of a range; `c` on the
//! last line comments on the whole range.
use super::super::shell::glyphs::{Glyph, glyph};
use super::lines::Line;
use super::prose::{Rich, role, strong};
use super::*;
use crate::config::ExplorerIconMode;
use crate::forge::host_state::{self, LegacyAccess};
use crate::forge::model::Side;
use crate::forge::request::InlineDraft;
use crate::ui::{Emphasis, GitChip, GitRole, GitSurface, Style};
use unicode_width::UnicodeWidthStr;

pub(super) type Drafts = BTreeMap<u64, Vec<PendingDraft>>;

// Keep a snapshot per directory so edits and deletes go back to the file that was loaded.
#[derive(Default)]
struct SavedDrafts(BTreeMap<PathBuf, Drafts>);

impl SavedDrafts {
    fn matches(&self, drafts: &Drafts) -> bool {
        let mut count = 0;
        for saved in self.0.values() {
            for (number, comments) in saved {
                count += comments.len();
                if !drafts
                    .get(number)
                    .is_some_and(|current| comments.iter().all(|comment| current.contains(comment)))
                {
                    return false;
                }
            }
        }
        count == drafts.values().map(Vec::len).sum::<usize>()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingDraft {
    pub(super) id: String,
    pub(super) comment: InlineDraft,
    pub(super) head: String,
}

impl PendingDraft {
    pub(super) fn new(comment: InlineDraft, head: String) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let process = std::process::id();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            id: format!("{process}-{time}-{sequence}"),
            comment,
            head,
        }
    }
}

const SCHEMA: u64 = 1;

/// Where a range comment starts (`V` in the Files reader).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Anchor {
    pub(super) number: u64,
    pub(super) path: String,
    pub(super) line: u32,
    pub(super) side: &'static str,
    pub(super) head: String,
}

type Scope = (String, String, Option<super::AccountNamespace>);

pub(super) struct SentReview {
    pub(super) ids: BTreeSet<String>,
    pub(super) account: Option<super::AccountNamespace>,
    /// Spelling of the draft directory at submit, independent of the wire target.
    pub(super) repo: String,
}

#[derive(Default)]
pub(super) struct Review {
    /// The host, repository and namespace whose drafts `inline_drafts` holds.
    scope: Option<Scope>,
    /// What the state directory holds for that scope: only changes are written.
    saved: SavedDrafts,
    parked: BTreeMap<Scope, ParkedDrafts>,
    save_issue: Option<SaveIssue>,
    retry_at: Option<std::time::Instant>,
    pub(super) anchor: Option<Anchor>,
}

struct ParkedDrafts {
    drafts: Drafts,
    saved: SavedDrafts,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveIssue {
    Failed,
    NoDirectory,
}

impl SaveIssue {
    fn notice(self) -> &'static str {
        match self {
            Self::Failed => "Couldn't save review drafts",
            Self::NoDirectory => "Review drafts aren't saved between sessions.",
        }
    }
}

/// A failed write is retried on a later poll, not on every paint loop.
const SAVE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

fn segment(text: &str) -> String {
    let mut escaped = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

fn directories(account: Option<&AccountNamespace>, host: &str, repo: &str) -> Vec<PathBuf> {
    let env = crate::forge::credential::Env::process();
    // Unit tests never touch the real state directory.
    if cfg!(any(test, feature = "test-support")) && env.get("DUSTY_FORGE_STATE_DIR").is_none() {
        return Vec::new();
    }
    let Some(hosts) = crate::forge::detect::hosts_path(&env) else {
        return Vec::new();
    };
    let Some(state) = hosts.parent() else {
        return Vec::new();
    };
    let access = host_state::legacy_access(&HostKey::new(host), account);
    directories_in(state, account, host, repo, access)
}

fn directories_in(
    state: &std::path::Path,
    account: Option<&AccountNamespace>,
    host: &str,
    repo: &str,
    access: LegacyAccess,
) -> Vec<PathBuf> {
    let host_dir = state.join("drafts").join(segment(host));
    let name = segment(repo);
    let mut paths = match account {
        Some(account) if *account.host() == HostKey::new(host) => repository_directories(
            crate::forge::cache::account_directory(&host_dir, account).join(&name),
        ),
        None if access == LegacyAccess::Unclaimed => {
            return repository_directories(host_dir.join(name));
        }
        _ => return Vec::new(),
    };
    if access == LegacyAccess::Owner {
        // The claim gives this account exclusive access to the original files. Keep them
        // in place so edits and sent-review retirement can use their real stored spelling.
        paths.extend(
            repository_directories(host_dir.join(name))
                .into_iter()
                .filter(|path| path.is_dir()),
        );
    }
    paths
}

/// Discover stored spellings by their actual directory entries. An insensitive filesystem
/// lists one entry even if both spellings work as lookups. The write directory comes first.
fn repository_directories(preferred: PathBuf) -> Vec<PathBuf> {
    let Some((parent, name)) = preferred.parent().zip(preferred.file_name()) else {
        return vec![preferred];
    };
    let mut paths: Vec<_> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().eq_ignore_ascii_case(name))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    paths.sort();
    if let Some(index) = paths.iter().position(|path| *path == preferred) {
        paths.swap(0, index);
    } else if paths.len() != 1 || !preferred.is_dir() {
        // If the sole entry already resolves by the preferred path, use its actual spelling.
        paths.insert(0, preferred);
    }
    paths
}

fn side_name(side: Side) -> &'static str {
    side.as_github()
}

fn encode(drafts: &[PendingDraft]) -> Value {
    serde_json::json!({
        "schema": SCHEMA,
        "drafts": drafts
            .iter()
            .map(|PendingDraft { id, comment, head }| {
                serde_json::json!({
                    "id": id,
                    "path": comment.path,
                    "line": comment.line,
                    "start_line": comment.start_line,
                    "side": side_name(comment.side),
                    "body": comment.body,
                    "head": head,
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn decode(value: &Value, source: &str) -> Vec<PendingDraft> {
    if value["schema"].as_u64() != Some(SCHEMA) {
        return Vec::new();
    }
    value["drafts"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, draft)| {
            let path = draft["path"].as_str()?.to_owned();
            let body = draft["body"].as_str()?.to_owned();
            let line = draft["line"].as_u64()?;
            if path.is_empty() || body.trim().is_empty() || line == 0 {
                return None;
            }
            Some(PendingDraft {
                // Old files did not store IDs. The stored spelling and row position keep
                // identical rows in two different files distinct.
                id: draft["id"]
                    .as_str()
                    .map_or_else(|| format!("legacy-{source}-{index}"), str::to_owned),
                comment: InlineDraft {
                    path,
                    line,
                    start_line: draft["start_line"].as_u64(),
                    side: if draft["side"].as_str() == Some("LEFT") {
                        Side::Left
                    } else {
                        Side::Right
                    },
                    body,
                },
                head: draft["head"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

fn load_from(directory: &std::path::Path) -> Drafts {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Drafts::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                return None;
            }
            let number = path.file_stem()?.to_str()?.parse::<u64>().ok()?;
            let drafts = load_file(&path).ok()?;
            (!drafts.is_empty()).then_some((number, drafts))
        })
        .collect()
}

fn load_file(file: &std::path::Path) -> Result<Vec<PendingDraft>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let source = file
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    Ok(decode(&value, source))
}

fn save_in(
    directory: &std::path::Path,
    number: u64,
    drafts: &[PendingDraft],
) -> Result<(), String> {
    let file = directory.join(format!("{number}.json"));
    if drafts.is_empty() {
        return match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut temporary, encode(drafts).to_string().as_bytes())
        .map_err(|error| error.to_string())?;
    temporary
        .persist(&file)
        .map_err(|error| error.error.to_string())?;
    Ok(())
}

/// Keep drafts in their original files, putting only new ones in the preferred directory.
fn flush_scope(
    directories: impl FnOnce() -> Vec<PathBuf>,
    drafts: &mut Drafts,
    saved: &mut SavedDrafts,
) -> Option<SaveIssue> {
    drafts.retain(|_, drafts| !drafts.is_empty());
    if saved.matches(drafts) {
        return None;
    }
    let paths = directories();
    let Some(primary) = paths.first() else {
        return Some(SaveIssue::NoDirectory);
    };
    let mut by_directory = BTreeMap::<PathBuf, Drafts>::new();
    for (&number, comments) in drafts.iter() {
        for draft in comments {
            let source = saved.0.iter().find_map(|(directory, saved)| {
                saved
                    .get(&number)?
                    .iter()
                    .any(|comment| comment.id == draft.id)
                    .then_some(directory)
            });
            by_directory
                .entry(source.unwrap_or(primary).clone())
                .or_default()
                .entry(number)
                .or_default()
                .push(draft.clone());
        }
    }
    let mut issue = None;
    for directory in saved.0.keys() {
        by_directory.entry(directory.clone()).or_default();
    }
    for (directory, drafts) in by_directory {
        let baseline = saved.0.entry(directory.clone()).or_default();
        if let Some(failure) = flush_directory(&directory, &drafts, baseline) {
            issue.get_or_insert(failure);
        }
    }
    saved.0.retain(|_, drafts| !drafts.is_empty());
    issue
}

/// Advance the snapshot only for entries written or removed successfully.
fn flush_directory(
    directory: &std::path::Path,
    drafts: &Drafts,
    saved: &mut Drafts,
) -> Option<SaveIssue> {
    let numbers = drafts
        .keys()
        .chain(saved.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut issue = None;
    for number in numbers {
        let now = drafts.get(&number);
        if saved.get(&number) == now {
            continue;
        }
        if save_in(
            directory,
            number,
            now.map(Vec::as_slice).unwrap_or_default(),
        )
        .is_err()
        {
            issue = Some(SaveIssue::Failed);
        } else if let Some(now) = now {
            saved.insert(number, now.clone());
        } else {
            saved.remove(&number);
        }
    }
    issue
}

fn retire_ids(drafts: &mut Drafts, number: u64, sent: &BTreeSet<String>) {
    if let Some(slot) = drafts.get_mut(&number) {
        slot.retain(|draft| !sent.contains(&draft.id));
        if slot.is_empty() {
            drafts.remove(&number);
        }
    }
}

fn retire_in(
    directory: &std::path::Path,
    number: u64,
    sent: &BTreeSet<String>,
) -> Result<(), String> {
    let mut drafts = load_file(&directory.join(format!("{number}.json")))?;
    let before = drafts.len();
    drafts.retain(|draft| !sent.contains(&draft.id));
    if drafts.len() != before {
        save_in(directory, number, &drafts)?;
    }
    Ok(())
}

/// `line 14` or `lines 12–14`.
pub(super) fn anchor_text(draft: &InlineDraft) -> String {
    match draft.range() {
        Some((start, end)) => format!("lines {start}–{end}"),
        None => format!("line {}", draft.line),
    }
}

/// `1 pending comment`, `2 pending comments`: GitHub's words for the comments a review holds
/// until it is submitted.
pub(super) fn pending(count: usize) -> String {
    plural(count, "pending comment")
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Words wrapped to `width` cells; a word longer than the row is cut.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut rows = Vec::new();
    for paragraph in text.lines() {
        let mut row = String::new();
        for word in paragraph.split_whitespace() {
            let mut word = word.to_owned();
            while word.width() > width {
                let mut used = 0;
                let cut = word
                    .char_indices()
                    .find(|(_, ch)| {
                        used += unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
                        used > width
                    })
                    .map_or(word.len(), |(at, _)| at.max(1));
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                rows.push(word[..cut].to_owned());
                word = word[cut..].to_owned();
            }
            if row.is_empty() {
                row = word;
            } else if row.width() + 1 + word.width() <= width {
                row.push(' ');
                row.push_str(&word);
            } else {
                rows.push(std::mem::replace(&mut row, word));
            }
        }
        rows.push(row);
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

impl Workspace {
    /// The drafts waiting for the review of `number`.
    pub(super) fn review_drafts(&self, number: u64) -> &[PendingDraft] {
        self.inline_drafts
            .get(&number)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn outdated_drafts(&self, number: u64) -> impl Iterator<Item = (usize, &PendingDraft)> {
        let current = self.review_head(number);
        self.review_drafts(number)
            .iter()
            .enumerate()
            .filter(move |(_, draft)| !current.is_empty() && draft.head != current)
    }

    pub(super) fn outdated_notice(&self, number: u64) -> Option<String> {
        match self.outdated_drafts(number).count() {
            0 => None,
            1 => Some("1 draft doesn't match this version of the pull request. See Outdated drafts to copy or delete it.".into()),
            count => Some(format!("{count} drafts don't match this version of the pull request. See Outdated drafts to copy or delete them.")),
        }
    }

    /// Take the user from the refused review sheet to the drafts that block it.
    pub(in crate::screen) fn open_outdated_drafts(&mut self, action: &Action, error: &str) -> bool {
        let Action::Review(number) = action.unscoped() else {
            return false;
        };
        if self.page != 2
            || self.number() != *number
            || self.outdated_notice(*number).as_deref() != Some(error)
        {
            return false;
        }
        if self.activate(Action::PrTab(pr::CONVERSATION)).is_err() {
            return false;
        }
        self.reading = false;
        self.reading_position.context = None;
        true
    }

    /// The one revision shared by all drafts, if they still describe the current head.
    pub(super) fn pending_review_head(&self, number: u64) -> Result<Option<&str>, String> {
        let drafts = self.review_drafts(number);
        let Some(first) = drafts.first() else {
            return Ok(None);
        };
        let current = self.review_head(number);
        if current.is_empty() {
            return Err("Refresh the pull request before submitting drafts.".into());
        }
        if let Some(notice) = self.outdated_notice(number) {
            return Err(notice);
        }
        Ok(Some(&first.head))
    }

    /// Retire only the drafts sent by this write, even if another repository is on screen.
    pub(super) fn clear_sent_review_drafts(
        &mut self,
        target: &ForgeTarget,
        number: u64,
        sent: &SentReview,
    ) -> Result<(), String> {
        self.clear_sent_review_drafts_with(target, number, sent, &directories)
    }

    fn clear_sent_review_drafts_with(
        &mut self,
        target: &ForgeTarget,
        number: u64,
        sent: &SentReview,
        directory: &impl Fn(Option<&AccountNamespace>, &str, &str) -> Vec<PathBuf>,
    ) -> Result<(), String> {
        if sent.ids.is_empty() {
            return Ok(());
        }
        let (Some(host), Some(repo)) = (&target.host, &target.repo) else {
            return Ok(());
        };
        if !sent.repo.eq_ignore_ascii_case(repo) {
            return Ok(());
        }
        let in_scope = |(h, r, account): &Scope| {
            h == host && r.eq_ignore_ascii_case(repo) && account == &sent.account
        };
        let maps = if self.account != sent.account {
            None
        } else if self.review.scope.as_ref().is_some_and(in_scope) {
            Some((&mut self.inline_drafts, &mut self.review.saved))
        } else {
            self.review
                .parked
                .iter_mut()
                .find(|(scope, _)| in_scope(scope))
                .map(|(_, parked)| (&mut parked.drafts, &mut parked.saved))
        };
        let mut saved = None;
        if let Some((drafts, baseline)) = maps {
            retire_ids(drafts, number, &sent.ids);
            saved = Some(baseline);
        }
        let mut failure = None;
        for path in directory(sent.account.as_ref(), host, &sent.repo) {
            match retire_in(&path, number, &sent.ids) {
                Ok(()) => {
                    if let Some(snapshot) = saved
                        .as_deref_mut()
                        .and_then(|saved| saved.0.get_mut(&path))
                    {
                        retire_ids(snapshot, number, &sent.ids);
                    }
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// From `poll`: loads the drafts of the repository on screen and writes what changed.
    pub(super) fn sync_drafts(&mut self) -> bool {
        self.sync_drafts_with(&directories)
    }

    fn sync_drafts_with(
        &mut self,
        directory: &impl Fn(Option<&AccountNamespace>, &str, &str) -> Vec<PathBuf>,
    ) -> bool {
        if self.host.is_empty() || self.repo.is_empty() {
            return false;
        }
        let scope = (self.host.clone(), self.repo.clone(), self.account.clone());
        if self.review.scope.as_ref() == Some(&scope) {
            return self
                .review
                .retry_at
                .is_none_or(|at| std::time::Instant::now() >= at)
                && self.flush_drafts_with(directory);
        }

        let previous = self.review.scope.replace(scope.clone());
        let held = std::mem::take(&mut self.inline_drafts);
        let saved = std::mem::take(&mut self.review.saved);
        let first = previous.is_none();
        if let Some(previous) = previous
            && !saved.matches(&held)
        {
            self.review.parked.insert(
                previous,
                ParkedDrafts {
                    drafts: held,
                    saved,
                },
            );
        } else if first {
            // Drafts made before the repository first resolved stay with it.
            self.inline_drafts = held;
        }
        if let Some(parked) = self.review.parked.remove(&scope) {
            self.inline_drafts = parked.drafts;
            self.review.saved = parked.saved;
        } else {
            let mut loaded = Drafts::new();
            for path in directory(scope.2.as_ref(), &scope.0, &scope.1) {
                let drafts = load_from(&path);
                for (&number, comments) in &drafts {
                    loaded
                        .entry(number)
                        .or_default()
                        .extend_from_slice(comments);
                }
                if !drafts.is_empty() {
                    self.review.saved.0.insert(path, drafts);
                }
            }
            for (number, drafts) in loaded {
                self.inline_drafts.entry(number).or_insert(drafts);
            }
        }
        self.review.anchor = None;
        self.flush_drafts_with(directory);
        true
    }

    pub(super) fn flush_drafts(&mut self) {
        self.flush_drafts_with(&directories);
    }

    pub(super) fn retire_review_drafts(&mut self) {
        self.flush_drafts();
        self.review = Review::default();
        self.inline_drafts.clear();
    }

    fn flush_drafts_with(
        &mut self,
        directory: &impl Fn(Option<&AccountNamespace>, &str, &str) -> Vec<PathBuf>,
    ) -> bool {
        let Some((host, repo, account)) = &self.review.scope else {
            return false;
        };
        let mut issue = flush_scope(
            || directory(account.as_ref(), host, repo),
            &mut self.inline_drafts,
            &mut self.review.saved,
        );
        self.review.parked.retain(|(host, repo, account), parked| {
            if let Some(parked_issue) = flush_scope(
                || directory(account.as_ref(), host, repo),
                &mut parked.drafts,
                &mut parked.saved,
            ) {
                issue.get_or_insert(parked_issue);
                true
            } else {
                false
            }
        });
        self.review.retry_at = issue.map(|_| std::time::Instant::now() + SAVE_RETRY);
        let previous = std::mem::replace(&mut self.review.save_issue, issue);
        match issue {
            Some(issue) if Some(issue) != previous => {
                self.notice = issue.notice().into();
                self.notice_error = issue == SaveIssue::Failed;
                self.notice_transient = false;
                true
            }
            None if previous.is_some_and(|issue| self.notice == issue.notice()) => {
                self.notice.clear();
                self.notice_error = false;
                true
            }
            _ => false,
        }
    }

    fn files_reader(&self) -> bool {
        self.page == 2 && self.pr_tab == pr::FILES && self.focus == 2
    }

    /// Files reader keys for pending comments: `V` marks a range, Esc drops it, `c` (or `C`)
    /// on a pending comment edits it and on the range's last line comments on the range.
    pub(super) fn review_key(&mut self, key: &KeyEvent) -> Option<Result<Option<Form>, String>> {
        if self.page != 2 || self.focus != 2 || key.modifiers.control || key.modifiers.alt {
            return None;
        }
        let row = self
            .detail_rows
            .get(self.detail_cursor)
            .and_then(|row| row.action.clone());
        if let Some(Action::Draft(number, index)) = &row {
            match key.code {
                KeyCode::Char('c' | 'C') => {
                    return Some(self.activate(Action::Draft(*number, *index)));
                }
                KeyCode::Char('y') => {
                    let draft = self.review_drafts(*number).get(*index)?;
                    self.copied = Some(draft.comment.body.clone());
                    return Some(Ok(None));
                }
                _ => {}
            }
        }
        if !self.files_reader() {
            return None;
        }
        match key.code {
            KeyCode::Escape if self.review.anchor.is_some() => {
                self.review.anchor = None;
                self.notice.clear();
                self.notice_error = false;
                Some(Ok(None))
            }
            KeyCode::Char('V') => {
                let Some(Action::Inline {
                    number,
                    path,
                    line,
                    side,
                    head,
                }) = row
                else {
                    return Some(Err("Move to a line of the diff to start a range".into()));
                };
                if let Err(error) = self.check_inline_head(number, &head) {
                    return Some(Err(error));
                }
                let anchor = Anchor {
                    number,
                    path,
                    line,
                    side,
                    head,
                };
                if self.review.anchor.as_ref() == Some(&anchor) {
                    self.review.anchor = None;
                    self.notice.clear();
                } else {
                    self.notice =
                        format!("Range from line {line} · j/k extend, C comments · Esc cancels");
                    self.review.anchor = Some(anchor);
                }
                self.notice_error = false;
                Some(Ok(None))
            }
            KeyCode::Char('c' | 'C') => match row {
                Some(Action::Inline {
                    number,
                    path,
                    line,
                    side,
                    head,
                }) if self.review.anchor.is_some() => {
                    let anchor = self.review.anchor.clone()?;
                    if anchor.number != number || anchor.path != path || anchor.side != side {
                        return Some(Err(
                            "A range stays in one file and on one side · Esc cancels it".into(),
                        ));
                    }
                    if anchor.head != head {
                        return Some(Err(
                            "The range is on an older commit. Press Esc and start it again.".into(),
                        ));
                    }
                    if anchor.line == line {
                        self.review.anchor = None;
                        return Some(self.activate(Action::Inline {
                            number,
                            path,
                            line,
                            side,
                            head,
                        }));
                    }
                    Some(self.activate(Action::InlineRange {
                        number,
                        path,
                        start: anchor.line.min(line),
                        line: anchor.line.max(line),
                        side,
                        head,
                    }))
                }
                // With a range open, `c` belongs to the range: a thread under the cursor is
                // not where it ends, and replying there would leave the range dangling.
                _ if self.review.anchor.is_some() => Some(Err(
                    "Move to a line of the diff to end the range · Esc cancels it".into(),
                )),
                _ => None,
            },
            _ => None,
        }
    }

    /// The sheet for a range comment or for editing a draft.
    pub(super) fn draft_form(&mut self, action: &Action) -> Result<Option<Form>, String> {
        match action {
            Action::InlineRange {
                number,
                path,
                start,
                line,
                head,
                ..
            } => {
                self.check_inline_head(*number, head)?;
                if let Some(reason) = self
                    .capabilities()
                    .inline_comment
                    .reason()
                    .filter(|_| self.read_only())
                {
                    return Err(reason.into());
                }
                if let Some(reason) = self.capabilities().batched_review.reason() {
                    return Err(format!("{reason} · comment on one line with c"));
                }
                let fields = vec![actions::field("Comment", "", true, true)];
                Ok(Some(self.form(
                    &format!("Comment on {path}:{start}–{line}"),
                    action.clone(),
                    fields,
                )))
            }
            Action::Draft(number, index) => {
                let draft = self
                    .review_drafts(*number)
                    .get(*index)
                    .ok_or("That draft was already sent or deleted")?
                    .comment
                    .clone();
                // The saved text, not an older typed value for the same slot.
                self.drafts.remove(&self.draft_key(action));
                let fields = vec![
                    actions::field("Comment", &draft.body, true, true),
                    actions::choice("Pending", "Keep", &["Keep", "Delete"]),
                ];
                Ok(Some(self.form(
                    &format!("Pending comment on {}:{}", draft.path, draft_lines(&draft)),
                    action.clone(),
                    fields,
                )))
            }
            _ => Ok(None),
        }
    }

    /// Saves a range comment or an edited draft; `None` for every other action.
    pub(super) fn submit_draft(
        &mut self,
        action: &Action,
        values: &[String],
    ) -> Option<Result<(), String>> {
        let result = match action {
            Action::InlineRange {
                number,
                path,
                start,
                line,
                side,
                head,
            } => {
                if let Err(error) = self.check_inline_head(*number, head) {
                    return Some(Err(error));
                }
                let body = values.first().map(|body| body.trim()).unwrap_or_default();
                if body.is_empty() {
                    return Some(Err("Comment is required".into()));
                }
                self.inline_drafts
                    .entry(*number)
                    .or_default()
                    .push(PendingDraft::new(
                        InlineDraft {
                            path: path.clone(),
                            line: u64::from(*line),
                            start_line: Some(u64::from(*start)),
                            side: if *side == "LEFT" {
                                Side::Left
                            } else {
                                Side::Right
                            },
                            body: body.to_owned(),
                        },
                        head.clone(),
                    ));
                self.review.anchor = None;
                self.notice = format!(
                    "Added to your review · {}",
                    pending(self.review_drafts(*number).len())
                );
                self.notice_transient = true;
                Ok(())
            }
            Action::Draft(number, index) => {
                let delete = values.get(1).map(String::as_str) == Some("Delete")
                    || values.first().is_none_or(|body| body.trim().is_empty());
                let Some(drafts) = self
                    .inline_drafts
                    .get_mut(number)
                    .filter(|drafts| *index < drafts.len())
                else {
                    return Some(Err("That draft was already sent or deleted".into()));
                };
                if delete {
                    drafts.remove(*index);
                } else {
                    drafts[*index].comment.body = values[0].trim().to_owned();
                }
                let left = drafts.len();
                self.notice = match (delete, left) {
                    (true, 0) => "Pending comment deleted · no review pending".into(),
                    (true, left) => {
                        format!("Pending comment deleted · {} · r review", pending(left))
                    }
                    (false, _) => "Pending comment updated".into(),
                };
                Ok(())
            }
            _ => return None,
        };
        self.drafts.remove(&self.draft_key(action));
        self.notice_error = false;
        self.flush_drafts();
        Some(result)
    }

    /// The line under a sheet's title for draft sheets.
    pub(super) fn draft_note(&self, action: &Action) -> Option<String> {
        match action {
            Action::InlineRange { number, .. } => {
                let waiting = self.review_drafts(*number).len();
                Some(if waiting == 0 {
                    "Starts a review · held as pending until you submit it (r)".into()
                } else {
                    format!("Joins your pending review · {} already", pending(waiting))
                })
            }
            Action::Draft(..) => {
                Some("Only you see pending comments until the review is submitted".into())
            }
            Action::Review(number) => {
                let drafts = self.review_drafts(*number);
                match self.pending_review_head(*number) {
                    Err(_) if self.outdated_drafts(*number).next().is_some() => {
                        Some("Ctrl+Enter opens Outdated drafts".into())
                    }
                    Err(notice) => Some(notice),
                    Ok(Some(_)) => Some(format!("Submits {} as one review", pending(drafts.len()))),
                    Ok(None) => None,
                }
            }
            _ => None,
        }
    }
}

/// `4` or `2–4`: the line or lines a draft is on.
pub(super) fn draft_lines(draft: &InlineDraft) -> String {
    match draft.range() {
        Some((start, end)) => format!("{start}–{end}"),
        None => draft.line.to_string(),
    }
}

/// `2 drafts` for the file heading; empty when there are none.
pub(super) fn file_count(workspace: &Workspace, id: u64, path: &str) -> usize {
    workspace
        .review_drafts(id)
        .iter()
        .filter(|draft| draft.comment.path == path)
        .count()
}

/// The rows of the Files diff between the range's first line and the cursor, tinted.
pub(super) fn mark_range(
    workspace: &Workspace,
    id: u64,
    path: &str,
    rows: &mut [Line],
    diff_start: usize,
) {
    let Some(anchor) = workspace.review.anchor.as_ref().filter(|anchor| {
        anchor.number == id
            && anchor.path == path
            && workspace.check_inline_head(id, &anchor.head).is_ok()
    }) else {
        return;
    };
    let cursor = match workspace
        .detail_rows
        .get(workspace.detail_cursor)
        .and_then(|row| row.action.as_ref())
    {
        Some(Action::Inline {
            line,
            side,
            path: at,
            head,
            ..
        }) if *side == anchor.side && at == path && *head == anchor.head => *line,
        _ => anchor.line,
    };
    let (low, high) = (anchor.line.min(cursor), anchor.line.max(cursor));
    for row in rows.iter_mut().skip(diff_start) {
        if let Some(Action::Inline { line, side, .. }) = &row.action
            && *side == anchor.side
            && (low..=high).contains(line)
        {
            row.band = Some(GitSurface::CursorInactive);
            for band in &mut row.bands {
                band.1 = GitSurface::CursorInactive;
            }
            if row.gutter.is_empty() {
                continue;
            }
            row.gutter_style = Style::Git {
                role: GitRole::Focus,
                surface: GitSurface::CursorInactive,
                emphasis: Emphasis::Bold,
            };
        }
    }
}

/// Outdated drafts live here even if their file is absent from Files.
pub(super) fn outdated_rows(workspace: &Workspace, id: u64, rows: &mut Vec<Line>, width: usize) {
    let mut drafts = workspace.outdated_drafts(id).peekable();
    if drafts.peek().is_none() {
        return;
    }
    rows.push(super::prose::heading(
        "Outdated drafts",
        "y copy · c or Enter to edit or delete",
    ));
    for (index, pending) in drafts {
        let draft = &pending.comment;
        let action = Action::Draft(id, index);
        let mut header = Line::button(
            format!("{}:{}", draft.path, draft_lines(draft)),
            action.clone(),
        );
        header.card = true;
        rows.push(header);
        for text in wrap(
            &draft.body,
            width.saturating_sub(4).min(super::prose::MEASURE),
        ) {
            let mut row = Rich::new().plain("  ").plain(&text).line();
            row.action = Some(action.clone());
            rows.push(row);
        }
    }
    rows.push(super::prose::blank());
}

/// Draft cards under the lines they are about (after any review thread there); drafts whose
/// line is not in this diff follow it.
pub(super) fn place(
    workspace: &Workspace,
    id: u64,
    path: &str,
    rows: &mut Vec<Line>,
    diff_start: usize,
    width: usize,
    mode: ExplorerIconMode,
) {
    let rail = Style::Git {
        role: GitRole::Modified,
        surface: GitSurface::Code,
        emphasis: Emphasis::None,
    };
    let mut placed: Vec<(usize, usize, Vec<Line>)> = Vec::new();
    let mut trailing = Vec::new();
    for (index, pending) in workspace.review_drafts(id).iter().enumerate() {
        let draft = &pending.comment;
        let head = &pending.head;
        if draft.path != path {
            continue;
        }
        let side = if draft.side == Side::Left {
            "LEFT"
        } else {
            "RIGHT"
        };
        let outdated = head.is_empty() || head != workspace.review_head(id);
        let matches_patch = !head.is_empty() && head == workspace.patch_head(id);
        let mut card = Vec::new();
        let header = Rich::new()
            .push(
                format!("{} ", glyph(Glyph::Comment, mode)),
                Style::Git {
                    role: GitRole::Modified,
                    surface: GitSurface::Code,
                    emphasis: Emphasis::None,
                },
            )
            // GitHub's `Pending` label: the comment waits for the review to be submitted.
            .chip(
                "Pending",
                Style::Git {
                    role: GitRole::Modified,
                    surface: GitSurface::Chip(GitChip::Modified),
                    emphasis: Emphasis::Bold,
                },
            )
            .push(
                " ",
                Style::Git {
                    role: GitRole::Muted,
                    surface: GitSurface::Code,
                    emphasis: Emphasis::None,
                },
            )
            .push(
                anchor_text(draft),
                Style::Git {
                    role: GitRole::Muted,
                    surface: GitSurface::Code,
                    emphasis: Emphasis::None,
                },
            );
        let header = if outdated {
            header.sep().push("Outdated", role(GitRole::Muted))
        } else {
            header
        };
        card.push(header.line());
        let measure = width.saturating_sub(6).min(super::prose::MEASURE);
        let prose = |card: &mut Vec<Line>, text: &str| {
            for text in wrap(text, measure) {
                card.push(
                    Rich::new()
                        .push(
                            text,
                            Style::Git {
                                role: GitRole::Text,
                                surface: GitSurface::Code,
                                emphasis: Emphasis::None,
                            },
                        )
                        .line(),
                );
            }
        };
        match super::suggestion::split(&draft.body) {
            // A suggested change reads as a diff against the lines on screen.
            Some(parts) => {
                let start = draft.start_line.unwrap_or(draft.line);
                let original = (start..=draft.line)
                    .take_while(|_| matches_patch)
                    .filter_map(|line| {
                        rows[diff_start..]
                            .iter()
                            .find_map(|row| match (&row.action, &row.source) {
                                (
                                    Some(Action::Inline {
                                        line: at, side: s, ..
                                    }),
                                    Some(source),
                                ) if u64::from(*at) == line && *s == side => {
                                    Some(source.text.to_string())
                                }
                                _ => None,
                            })
                    })
                    .collect::<Vec<_>>();
                for part in parts {
                    match part {
                        super::suggestion::Part::Text(text) => prose(&mut card, &text),
                        super::suggestion::Part::Suggestion(lines) => super::suggestion::rows(
                            &mut card,
                            &original,
                            &lines,
                            width.saturating_sub(4),
                            "",
                        ),
                    }
                }
            }
            None => prose(&mut card, &draft.body),
        }
        for row in &mut card {
            row.band = Some(
                row.band
                    .filter(|band| matches!(band, GitSurface::AddedBand | GitSurface::RemovedBand))
                    .unwrap_or(GitSurface::Code),
            );
            row.gutter = "  ▍ ".into();
            row.gutter_style = rail;
            row.pan = false;
            row.action = Some(Action::Draft(id, index));
        }
        let line = draft.line as u32;
        let target = rows[diff_start..]
            .iter()
            .rposition(|row| matches_patch && matches!(&row.action, Some(Action::Inline { line: at, side: s, .. }) if *at == line && *s == side))
            .map(|at| {
                // After the review threads already under that line.
                let mut at = diff_start + at + 1;
                while rows.get(at).is_some_and(|row| row.band == Some(GitSurface::Code) && !matches!(row.action, Some(Action::Inline { .. }))) {
                    at += 1;
                }
                at
            });
        match target {
            Some(at) => placed.push((at, index, card)),
            None => trailing.extend(card),
        }
    }
    placed.sort_by_key(|(at, index, _)| std::cmp::Reverse((*at, *index)));
    for (at, _, card) in placed {
        rows.splice(at..at, card);
    }
    if !trailing.is_empty() {
        rows.push(super::prose::blank());
        rows.extend(trailing);
    }
}

/// `› Review changes · 2 pending`.
pub(super) fn review_row(workspace: &Workspace, id: u64) -> Line {
    let mut review = Rich::new()
        .push("› ", role(GitRole::Focus))
        .push("Review changes", role(GitRole::Link));
    let waiting = workspace.review_drafts(id).len();
    if waiting > 0 {
        review = review
            .sep()
            .push(format!("{waiting} pending"), strong(GitRole::Modified));
    }
    let mut review = review.line();
    review.action = Some(Action::Review(id));
    review
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(line: u64, start: Option<u64>, body: &str) -> PendingDraft {
        PendingDraft::new(
            InlineDraft {
                path: "src/lib.rs".into(),
                line,
                start_line: start,
                side: Side::Right,
                body: body.into(),
            },
            "a".repeat(40),
        )
    }

    #[test]
    fn drafts_stay_loaded_during_a_same_login_recheck() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "forge.test:8443".into();
        ws.repo = "o/r".into();
        let host = super::super::HostKey::new(&ws.host);
        let account = super::super::AccountNamespace::signed_in(host, "alice").unwrap();
        let directory = |selected: Option<&super::super::AccountNamespace>, _: &str, _: &str| {
            (selected == Some(&account))
                .then(|| state.path().join("alice"))
                .into_iter()
                .collect()
        };
        let pending = draft(14, None, "Saved before re-check");
        save_in(
            &state.path().join("alice"),
            7,
            std::slice::from_ref(&pending),
        )
        .unwrap();
        assert!(ws.accept_read_account(true, Some(account.clone()), None, true));
        ws.sync_drafts_with(&directory);
        assert_eq!(ws.review_drafts(7), std::slice::from_ref(&pending));

        ws.accounts.open = true;
        assert!(matches!(
            ws.accounts_key(&KeyEvent::character('r')),
            Some(Ok(None))
        ));
        ws.sync_drafts_with(&directory);
        assert_eq!(ws.review_drafts(7), std::slice::from_ref(&pending));
        assert!(ws.accept_read_account(true, Some(account.clone()), None, true));
        ws.sync_drafts_with(&directory);
        assert_eq!(ws.review_drafts(7), &[pending]);
    }

    #[test]
    fn a_review_receipt_keeps_same_number_drafts_in_the_other_repository() {
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "github.com".into();
        ws.repo = "Home/Root".into();
        ws.sync_drafts();
        let same_draft = draft(14, None, "Name this");
        let unsent = draft(25, None, "Keep this one");
        ws.inline_drafts
            .insert(7, vec![same_draft.clone(), unsent.clone()]);
        let linked = ForgeTarget::repository(&ws.host, "linked/other");
        let sent = SentReview {
            ids: BTreeSet::from([same_draft.id.clone()]),
            account: None,
            repo: "Home/Root".into(),
        };
        ws.clear_sent_review_drafts(&linked, 7, &sent).unwrap();
        assert_eq!(ws.review_drafts(7), &[same_draft.clone(), unsent.clone()]);
        let same_repo = ForgeTarget::repository(&ws.host, "home/root");
        ws.clear_sent_review_drafts(&same_repo, 7, &sent).unwrap();
        assert_eq!(ws.review_drafts(7), &[unsent]);
    }

    #[test]
    fn inbox_keeps_mixed_case_review_and_form_drafts_in_their_local_scope() {
        let state = tempfile::tempdir().unwrap();
        let directory = |_: Option<&super::super::AccountNamespace>, _: &str, repo: &str| {
            repository_directories(state.path().join(segment(repo)))
        };
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "github.com".into();
        ws.repo = "Acme/Widget".into();
        ws.page = 2;
        ws.states = ["open".into(), "open".into()];
        let home_row = serde_json::json!({
            "number": 7, "title": "Home PR", "state": "OPEN",
            "url": "https://github.com/acme/widget/pull/7",
            "repository": {"nameWithOwner": "acme/widget"}
        });
        let inbox_rows = serde_json::json!([home_row.clone(), {
            "number": 7, "title": "Other PR", "state": "OPEN",
            "url": "https://github.com/other/repo/pull/7",
            "repository": {"nameWithOwner": "other/repo"}
        }]);
        let head = "a".repeat(40);
        let item = serde_json::json!({"headRefOid": head, "number": 7});
        let inbox = Key::Inbox(
            ws.host.clone(),
            2,
            "open".into(),
            "review-requested:@me".into(),
        );
        let reply = |value: Value| {
            Ok(Reply {
                text: value.to_string(),
                value,
            })
        };
        ws.cache
            .insert(ws.list_key(), reply(serde_json::json!([home_row])));
        ws.cache.insert(inbox, reply(inbox_rows));
        ws.cache.insert(Key::Item(2, 7), reply(item.clone()));
        ws.cache.insert(Key::Diff(7), reply(item.clone()));
        ws.sync_drafts_with(&directory);

        let range = ws
            .activate(Action::InlineRange {
                number: 7,
                path: "src/lib.rs".into(),
                start: 10,
                line: 12,
                side: "RIGHT",
                head,
            })
            .unwrap()
            .unwrap();
        let action = |form: &Form| match &form.action {
            FormAction::Github(action) => action.clone(),
            _ => panic!("a GitHub sheet was expected"),
        };
        let range_action = action(&range);
        assert_eq!(
            ws.sheet_note(&range_action, &range.fields).as_deref(),
            Some("Starts a review · held as pending until you submit it (r)")
        );
        ws.submit(range_action, vec!["Pending local draft".into()])
            .unwrap();
        ws.flush_drafts_with(&directory);

        let pending = ws.activate(Action::Draft(7, 0)).unwrap().unwrap();
        assert_eq!(
            ws.sheet_note(&action(&pending), &pending.fields).as_deref(),
            Some("Only you see pending comments until the review is submitted")
        );

        let comment = ws.event(&KeyEvent::character('C'), None).unwrap().unwrap();
        ws.save_draft(&action(&comment), vec!["Unsent local comment".into()]);
        for (ch, repo) in [
            ('i', "Acme/Widget"),
            ('j', "other/repo"),
            ('k', "Acme/Widget"),
        ] {
            ws.event(&KeyEvent::character(ch), None).unwrap();
            assert_eq!(ws.repo, repo);
            ws.sync_drafts_with(&directory);
        }
        assert!(ws.inbox.enabled);
        assert_eq!(ws.repo, "Acme/Widget");
        assert_eq!(ws.review_drafts(7)[0].comment.body, "Pending local draft");
        let directories: Vec<_> = std::fs::read_dir(state.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(directories, ["Acme%2FWidget"]);
        assert!(state.path().join("Acme%2FWidget/7.json").is_file());
        let reopened = ws.event(&KeyEvent::character('C'), None).unwrap().unwrap();
        assert_eq!(reopened.fields[0].input.input, "Unsent local comment");

        ws.cache.insert(Key::Item(2, 7), reply(item));
        let review = ws.event(&KeyEvent::character('r'), None).unwrap().unwrap();
        ws.submit(action(&review), vec!["Comment".into(), String::new()])
            .unwrap();
        assert!(ws.queue.iter().any(|request| matches!(
            &request.op,
            Op::Write(target, Write::SubmitReview { number: 7, drafts, .. }, _)
                if *target == ForgeTarget::repository("github.com", "Acme/Widget")
                    && drafts.len() == 1 && drafts[0].body == "Pending local draft"
        )));
    }

    #[test]
    fn review_receipt_retires_drafts_from_each_stored_spelling() {
        let state = tempfile::tempdir().unwrap();
        let directory = |_: Option<&super::super::AccountNamespace>, _: &str, repo: &str| {
            repository_directories(state.path().join(segment(repo)))
        };
        let mixed = state.path().join("Acme%2FWidget");
        let lower = state.path().join("acme%2Fwidget");
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "github.com".into();
        ws.repo = "Acme/Widget".into();

        // Pre-ID drafts from HEAD. The same row in two distinct files is two comments.
        let sent = draft(14, None, "Send this");
        let held = draft(15, None, "Keep this");
        let write_old = |path: &std::path::Path, drafts: &[PendingDraft]| {
            std::fs::create_dir(path).unwrap();
            let mut value = encode(drafts);
            for row in value["drafts"].as_array_mut().unwrap() {
                row.as_object_mut().unwrap().remove("id");
            }
            std::fs::write(path.join("7.json"), value.to_string()).unwrap();
        };
        write_old(&lower, &[sent.clone(), held.clone()]);
        ws.sync_drafts_with(&directory);
        assert_eq!(ws.review_drafts(7).len(), 2);

        let distinct = !mixed.is_dir();
        if distinct {
            ws.repo = "other/repo".into();
            ws.sync_drafts_with(&directory);
            write_old(&mixed, std::slice::from_ref(&sent));
            ws.repo = "Acme/Widget".into();
            ws.sync_drafts_with(&directory);
        }

        let expected_sent = if distinct { 2 } else { 1 };
        let sent_ids: BTreeSet<_> = ws
            .review_drafts(7)
            .iter()
            .filter(|draft| draft.comment == sent.comment)
            .map(|draft| draft.id.clone())
            .collect();
        assert_eq!(sent_ids.len(), expected_sent);
        assert_eq!(ws.review_drafts(7).len(), expected_sent + 1);
        let confirmed = SentReview {
            ids: sent_ids,
            account: None,
            repo: ws.repo.clone(),
        };
        let new_draft = draft(16, None, "Added after confirming");
        ws.inline_drafts
            .get_mut(&7)
            .unwrap()
            .push(new_draft.clone());
        ws.flush_drafts_with(&directory);
        assert!(load_from(&mixed)[&7].contains(&new_draft));
        if distinct {
            assert!(!load_from(&lower)[&7].contains(&new_draft));
        }

        ws.repo = "other/repo".into();
        ws.sync_drafts_with(&directory);
        let target = ForgeTarget::repository(&ws.host, "acme/widget");
        ws.clear_sent_review_drafts_with(&target, 7, &confirmed, &directory)
            .unwrap();

        // Reopening via the lowercase spelling finds the same surviving drafts, once.
        ws.repo = "acme/widget".into();
        ws.sync_drafts_with(&directory);
        let remaining = ws.review_drafts(7);
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&new_draft));
        assert!(remaining.iter().any(|draft| draft.comment == held.comment));
        if distinct {
            assert_eq!(load_from(&mixed)[&7], [new_draft]);
            let legacy = load_from(&lower);
            assert_eq!(legacy[&7].len(), 1);
            assert_eq!(legacy[&7][0].comment, held.comment);
        }
    }

    #[test]
    fn a_failed_draft_save_stays_unsaved_and_the_next_flush_retries_it() {
        let scratch = tempfile::tempdir().unwrap();
        let state = scratch.path().join("drafts");
        let directory =
            |_: Option<&super::super::AccountNamespace>, _: &str, _: &str| vec![state.clone()];
        std::fs::create_dir(&state).unwrap();
        // A directory at one draft's filename prevents only that draft from being saved.
        let blocked = state.join("7.json");
        std::fs::create_dir(&blocked).unwrap();
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "github.com".into();
        ws.repo = "home/root".into();
        ws.sync_drafts_with(&directory);
        ws.inline_drafts
            .insert(7, vec![draft(14, None, "Retry this")]);
        ws.inline_drafts
            .insert(8, vec![draft(15, None, "Save now")]);

        assert!(ws.flush_drafts_with(&directory));
        assert!(ws.notice_error);
        assert_eq!(ws.notice, "Couldn't save review drafts");
        let saved = &ws.review.saved.0[&state];
        assert!(!saved.contains_key(&7));
        assert_eq!(saved.get(&8), ws.inline_drafts.get(&8));

        ws.notice.clear();
        assert!(!ws.flush_drafts_with(&directory));
        assert!(ws.notice.is_empty(), "the same failure was shown again");
        std::fs::remove_dir(&blocked).unwrap();
        ws.flush_drafts_with(&directory);
        assert!(ws.review.saved.matches(&ws.inline_drafts));
        assert_eq!(load_from(&state), ws.inline_drafts);
    }

    #[test]
    fn drafts_without_a_state_directory_stay_in_memory_by_repository() {
        let directory = |_: Option<&super::super::AccountNamespace>, _: &str, _: &str| Vec::new();
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "github.com".into();
        ws.repo = "home/root".into();
        ws.sync_drafts_with(&directory);
        let held = draft(14, None, "Keep this in memory");
        ws.inline_drafts.insert(7, vec![held.clone()]);
        assert!(ws.flush_drafts_with(&directory));
        assert_eq!(ws.notice, "Review drafts aren't saved between sessions.");
        assert!(ws.review.saved.0.is_empty());

        ws.repo = "home/other".into();
        ws.sync_drafts_with(&directory);
        assert!(ws.inline_drafts.is_empty());
        ws.repo = "home/root".into();
        ws.sync_drafts_with(&directory);
        assert_eq!(ws.review_drafts(7), &[held]);
        assert!(ws.review.saved.0.is_empty());
    }

    #[test]
    fn drafts_round_trip_through_their_file_format() {
        let drafts = vec![
            draft(14, None, "Name this"),
            draft(22, Some(18), "Split this block\nin two"),
        ];
        assert_eq!(decode(&encode(&drafts), "example%2Fdemo"), drafts);
        // An unknown schema reads as nothing rather than as wrong drafts.
        let mut other = encode(&drafts);
        other["schema"] = 9.into();
        assert!(decode(&other, "example%2Fdemo").is_empty());
    }

    #[test]
    fn a_range_reads_as_lines_and_a_single_line_as_line() {
        assert_eq!(
            anchor_text(&draft(22, Some(18), "x").comment),
            "lines 18–22"
        );
        assert_eq!(anchor_text(&draft(22, None, "x").comment), "line 22");
        assert_eq!(anchor_text(&draft(22, Some(22), "x").comment), "line 22");
    }

    #[test]
    fn draft_bodies_wrap_on_words() {
        assert_eq!(
            wrap("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        assert_eq!(wrap("first\n\nsecond", 20), vec!["first", "", "second"]);
        assert_eq!(wrap("abcdefghijkl", 8), vec!["abcdefgh", "ijkl"]);
    }

    #[test]
    fn first_login_adopts_head_drafts_under_both_checkout_spellings() {
        let scratch = tempfile::tempdir().unwrap();
        let state = scratch.path();
        let host = "forge.test:8443";
        let h = HostKey::new(host);
        let alice = AccountNamespace::signed_in(h.clone(), "alice").unwrap();
        let bob = AccountNamespace::signed_in(h, "bob").unwrap();
        // HEAD placed escaped REPO/PR.json directly under drafts/escaped-host. It
        // did not write a draft ID or an _accounts path.
        let legacy_host = state.join("drafts/forge.test%3A8443");
        let lower = legacy_host.join("acme%2Fwidget");
        let mixed = legacy_host.join("Acme%2FWidget");
        let seed = |directory: &std::path::Path, body: &str| {
            std::fs::create_dir_all(directory).unwrap();
            std::fs::write(
                directory.join("7.json"),
                serde_json::json!({
                    "schema": 1,
                    "drafts": [{
                        "path": "src/lib.rs", "line": 14, "side": "RIGHT", "body": body,
                        "head": "a".repeat(40)
                    }]
                })
                .to_string(),
            )
            .unwrap();
        };
        seed(&lower, "lowercase HEAD draft");
        let distinct_spellings = !mixed.exists();

        let load_as = |account: Option<&AccountNamespace>, access, repo: &str| {
            let mut ws = Workspace::new(PathBuf::new());
            ws.host = host.into();
            ws.repo = repo.into();
            ws.account = account.cloned();
            ws.sync_drafts_with(&|account, host, repo| {
                directories_in(state, account, host, repo, access)
            });
            let mut bodies: Vec<_> = ws
                .review_drafts(7)
                .iter()
                .map(|draft| draft.comment.body.clone())
                .collect();
            bodies.sort();
            bodies
        };
        let lower_only = vec!["lowercase HEAD draft".to_owned()];
        assert_eq!(
            load_as(Some(&alice), LegacyAccess::Owner, "Acme/Widget"),
            lower_only
        );
        assert_eq!(
            load_as(None, LegacyAccess::Unclaimed, "Acme/Widget"),
            lower_only
        );

        // Write the mixed-case HEAD layout too. On an insensitive filesystem the first
        // directory has to go before we can create an entry with the other spelling.
        if !distinct_spellings {
            std::fs::remove_dir_all(&lower).unwrap();
        }
        seed(&mixed, "mixed-case HEAD draft");
        let mut expected = if distinct_spellings {
            lower_only
        } else {
            Vec::new()
        };
        expected.push("mixed-case HEAD draft".to_owned());
        for spelling in ["Acme/Widget", "acme/widget"] {
            assert_eq!(
                load_as(Some(&alice), LegacyAccess::Owner, spelling),
                expected
            );
            assert!(load_as(Some(&bob), LegacyAccess::Blocked, spelling).is_empty());
            assert!(load_as(None, LegacyAccess::Blocked, spelling).is_empty());
        }
    }

    #[test]
    fn drafts_are_saved_per_pull_request_and_survive_a_reload() {
        let state = tempfile::tempdir().unwrap();
        let directory = state
            .path()
            .join("drafts/github.com")
            .join(segment("example/demo"));
        let drafts = vec![draft(14, None, "Name this"), draft(22, Some(18), "Range")];
        save_in(&directory, 9, &drafts).unwrap();
        assert_eq!(load_from(&directory).get(&9), Some(&drafts));
        assert!(directory.ends_with("example%2Fdemo"));
        let file = directory.join("9.json");
        assert!(
            file.is_file(),
            "drafts live under the state directory: {file:?}"
        );
        // Sending or deleting the last draft removes the file.
        save_in(&directory, 9, &[]).unwrap();
        assert!(!file.exists());
        assert!(load_from(&directory).is_empty());
    }
}
