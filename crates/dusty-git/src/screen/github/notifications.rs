//! Notifications (GitLab's To-Do List), laid out like
//! github.com/notifications: a sidebar of views (`Inbox`, `Saved`, `Done`), filters
//! (`Assigned`, `Participating`, `Mentioned`, `Team mentioned`, `Review requested`) and
//! repositories, each with its unread count; beside it the chosen view's threads, grouped by
//! repository, one row each (type glyph, title, `#13`, reason chip, time), with `All │
//! Unread` above them. `Enter` opens the pull request or issue reader in its own tab (`Esc`
//! there comes back here), the run in Actions, or a release in the browser; `e` marks a
//! thread done, `s` saves it, `m` / `M` mark read and `u` unsubscribes. Saved and Done live
//! in this repository's Git config; the only forge writes are mark-read and unsubscribe.
//! The list is read the first time the page is painted, never while `<`/`>` pass over it,
//! so a visit costs one request.

use super::super::shell::chrome::{self, Span, bold, st};
use super::super::shell::glyphs::{Glyph, glyph};
use super::super::shell::layout::{self, Mode, Rect};
use super::super::{FormAction, GitScreen};
use super::*;
use crate::config::ExplorerIconMode;
use crate::forge::host_state::{self, LegacyAccess};
use crate::forge::model::{ItemKind, Lifecycle, Notification, NotifyReason, NotifyTarget};
use crate::forge::records;
use crate::forge::request::{NotifyFilter, Page};
use crate::ui::{CellGrid, Frame, GitChip, GitRole, GitSurface};
use std::collections::HashSet;

pub(in crate::screen) const PAGE: usize = 4;
const PANE: GitSurface = GitSurface::Pane;
/// This repository's local Git config keeps the threads saved and marked done here, as
/// scoped `host/account/thread` words; the oldest go once one namespace holds this many.
const SAVED_KEY: &str = "dusty-git.notifySaved";
const DONE_KEY: &str = "dusty-git.notifyDone";
const KEEP: usize = 500;

/// GitHub's sidebar filters, in its order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Filter {
    Assigned,
    Participating,
    Mentioned,
    TeamMentioned,
    ReviewRequested,
}

const FILTERS: [Filter; 5] = [
    Filter::Assigned,
    Filter::Participating,
    Filter::Mentioned,
    Filter::TeamMentioned,
    Filter::ReviewRequested,
];

fn team_mention(reason: &NotifyReason) -> bool {
    matches!(reason, NotifyReason::Other(other) if other == "team_mention")
}

impl Filter {
    fn name(self) -> &'static str {
        match self {
            Self::Assigned => "Assigned",
            Self::Participating => "Participating",
            Self::Mentioned => "Mentioned",
            Self::TeamMentioned => "Team mentioned",
            Self::ReviewRequested => "Review requested",
        }
    }

    fn holds(self, thread: &Notification) -> bool {
        match self {
            Self::Assigned => thread.reason == NotifyReason::Assigned,
            // GitHub's "participating": you are in the conversation, not only watching it.
            Self::Participating => {
                matches!(
                    thread.reason,
                    NotifyReason::Assigned
                        | NotifyReason::Author
                        | NotifyReason::Comment
                        | NotifyReason::Mentioned
                        | NotifyReason::ReviewRequested
                        | NotifyReason::StateChange
                ) || team_mention(&thread.reason)
            }
            Self::Mentioned => thread.reason == NotifyReason::Mentioned,
            Self::TeamMentioned => team_mention(&thread.reason),
            Self::ReviewRequested => thread.reason == NotifyReason::ReviewRequested,
        }
    }
}

/// What the list shows: a sidebar entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum View {
    #[default]
    Inbox,
    Saved,
    Done,
    Filter(Filter),
    Repo(String),
}

/// One sidebar row: a heading, a gap, or an entry with its count.
enum Side {
    Heading(&'static str),
    Gap,
    Entry(View, String, usize),
}

#[derive(Default)]
pub(super) struct Inbox {
    view: View,
    /// GitHub's `All │ Unread` switch over the list.
    unread_only: bool,
    selected: usize,
    /// The sidebar has the keys (`Tab`), and its cursor among the entries.
    side: bool,
    side_at: usize,
    /// The page has been painted once: its list may be read.
    wanted: bool,
    /// Local reads and confirmed writes, ahead of the forge's next list.
    read: HashSet<String>,
    gone: HashSet<String>,
    /// Saved and done threads, partitioned by account in local Git config.
    marks: Option<Marks>,
    marks_scope: Option<MarkScope>,
    /// A reader or run opened from here: its `Esc` comes back to Notifications.
    pub(super) back: bool,
    /// The thread that was opened, for the cursor to land on again.
    opened: Option<String>,
    /// `/`: the threads whose subject or repository holds this text.
    pub(super) filter: super::runs::Input,
}

#[derive(Default)]
struct Marks {
    saved: Vec<String>,
    done: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkScope {
    prefix: String,
    adopted: Option<String>,
    persistent: bool,
}

impl MarkScope {
    fn new(host: &str, account: Option<&AccountNamespace>, access: LegacyAccess) -> Self {
        let legacy = format!("{host}/");
        match account {
            Some(account) => {
                let namespace = match account.login() {
                    Some(login) => {
                        let encoded: String =
                            login.bytes().map(|byte| format!("{byte:02x}")).collect();
                        format!("user-{encoded}")
                    }
                    None => "anonymous".into(),
                };
                Self {
                    prefix: format!("{}/{namespace}/", account.host()),
                    adopted: (access == LegacyAccess::Owner).then_some(legacy),
                    persistent: true,
                }
            }
            None if access == LegacyAccess::Unclaimed => Self {
                prefix: legacy,
                adopted: None,
                persistent: true,
            },
            None => Self {
                prefix: format!("{host}/unverified/"),
                adopted: None,
                persistent: false,
            },
        }
    }

    fn view(&self, list: Vec<String>) -> Vec<String> {
        let matches = |entry: &str| mark_id(entry, &self.prefix).is_some();
        let mut marks: Vec<_> = list
            .iter()
            .filter(|entry| matches(entry))
            .cloned()
            .collect();
        if let Some(prefix) = &self.adopted {
            for id in list.iter().filter_map(|entry| mark_id(entry, prefix)) {
                let mark = format!("{}{id}", self.prefix);
                if !marks.contains(&mark) {
                    marks.push(mark);
                }
            }
        }
        marks
    }
}

fn mark_id<'mark>(mark: &'mark str, prefix: &str) -> Option<&'mark str> {
    mark.strip_prefix(prefix)
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

fn local_config(root: &std::path::Path) -> Option<git2::Config> {
    git2::Repository::open(root)
        .ok()?
        .config()
        .ok()?
        .open_level(git2::ConfigLevel::Local)
        .ok()
}

fn load_marks(root: &std::path::Path) -> Marks {
    let Some(config) = local_config(root) else {
        return Marks::default();
    };
    Marks {
        saved: read_marks(&config, SAVED_KEY),
        done: read_marks(&config, DONE_KEY),
    }
}

fn read_marks(config: &git2::Config, key: &str) -> Vec<String> {
    config
        .get_string(key)
        .map(|text| text.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn set_mark(list: &mut Vec<String>, prefix: &str, mark: &str, present: bool) {
    list.retain(|existing| existing != mark);
    if present {
        list.push(mark.to_owned());
        let mut over = list
            .iter()
            .filter(|entry| mark_id(entry, prefix).is_some())
            .count()
            .saturating_sub(KEEP);
        list.retain(|entry| {
            if over > 0 && mark_id(entry, prefix).is_some() {
                over -= 1;
                false
            } else {
                true
            }
        });
    }
}

fn store_mark(root: &std::path::Path, key: &str, scope: &MarkScope, id: &str, present: bool) {
    if !scope.persistent {
        return;
    }
    if let Some(mut config) = local_config(root) {
        // Replay just this edit against disk; other accounts' marks are not ours to replace.
        let mut list = read_marks(&config, key);
        if let Some(prefix) = &scope.adopted {
            // Editing an adopted mark must also retire its old spelling, so removing it
            // cannot make it reappear when the list is loaded again.
            list.retain(|entry| mark_id(entry, prefix) != Some(id));
        }
        set_mark(
            &mut list,
            &scope.prefix,
            &format!("{}{id}", scope.prefix),
            present,
        );
        let _ = if list.is_empty() {
            config.remove(key)
        } else {
            config.set_str(key, &list.join(" "))
        };
    }
}

fn reason_word(workspace: &Workspace, thread: &Notification) -> String {
    match &thread.reason {
        NotifyReason::Mentioned => "Mention".into(),
        NotifyReason::ReviewRequested => workspace.requested_words().0.into(),
        NotifyReason::Assigned => "Assigned".into(),
        NotifyReason::Author => "Author".into(),
        NotifyReason::Comment => "Comment".into(),
        NotifyReason::StateChange => "State change".into(),
        // "Run failed" / "Pipeline passed": the forge's own word, not the bare "CI", with the
        // outcome the subject names; a failure never reads as a neutral "finished" (r16
        // notif-actions-08).
        NotifyReason::CiActivity => {
            let outcome =
                match suite_subject(&thread.title).and_then(|(_, conclusion, _)| conclusion) {
                    Some("failure") => "failed",
                    Some("success") => "passed",
                    Some("cancelled") => "cancelled",
                    _ => "finished",
                };
            let mut word = format!("{} {outcome}", workspace.terms().run_noun);
            if let Some(first) = word.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            word
        }
        NotifyReason::Subscribed => "Subscribed".into(),
        NotifyReason::Other(other) => {
            let mut word = other.replace('_', " ");
            if let Some(first) = word.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            word
        }
    }
}

impl Workspace {
    /// The forge's threads, as this session last saw them (marks applied).
    fn threads(&self) -> Vec<Notification> {
        let Some(value) = self.value(&Key::Notifications) else {
            return Vec::new();
        };
        let items = value
            .get("Notifications")
            .map_or(Value::Null, |page| page["items"].clone());
        let mut threads: Vec<Notification> = serde_json::from_value(items).unwrap_or_default();
        // Use the shared write queue and active request as the optimistic overlay. Once the
        // request finishes, a rejected change simply drops away; the source list never moved.
        let pending = self
            .active
            .requests()
            .chain(self.queue.iter())
            .find_map(|request| match &request.op {
                Op::Write(
                    target,
                    write @ (Write::MarkRead(_) | Write::MarkAllRead | Write::Unsubscribe(_)),
                ) if target.host.as_deref() == Some(self.host.as_str()) => Some(write),
                _ => None,
            });
        threads.retain(|thread| {
            !self.notify.gone.contains(&thread.id.0)
                && !matches!(pending, Some(Write::Unsubscribe(id)) if *id == thread.id)
        });
        for thread in &mut threads {
            if self.notify.read.contains(&thread.id.0)
                || matches!(pending, Some(Write::MarkAllRead))
                || matches!(pending, Some(Write::MarkRead(id)) if *id == thread.id)
            {
                thread.unread = false;
            }
        }
        threads
    }

    fn thread_mark(&self, thread: &Notification) -> Option<String> {
        self.notify
            .marks_scope
            .as_ref()
            .map(|scope| format!("{}{}", scope.prefix, thread.id.0))
    }

    fn is_done(&self, thread: &Notification) -> bool {
        self.thread_mark(thread).is_some_and(|mark| {
            self.notify
                .marks
                .as_ref()
                .is_some_and(|marks| marks.done.contains(&mark))
        })
    }

    fn is_saved(&self, thread: &Notification) -> bool {
        self.thread_mark(thread).is_some_and(|mark| {
            self.notify
                .marks
                .as_ref()
                .is_some_and(|marks| marks.saved.contains(&mark))
        })
    }

    fn ensure_marks(&mut self) {
        let access = host_state::legacy_access(&HostKey::new(&self.host), self.account.as_ref());
        self.ensure_marks_with(access);
    }

    fn ensure_marks_with(&mut self, access: LegacyAccess) {
        if self.host.is_empty() {
            return;
        }
        let scope = MarkScope::new(&self.host, self.account.as_ref(), access);
        if self.notify.marks_scope.as_ref() != Some(&scope) {
            let marks = if scope.persistent {
                load_marks(&self.root)
            } else {
                Marks::default()
            };
            self.notify.marks = Some(Marks {
                saved: scope.view(marks.saved),
                done: scope.view(marks.done),
            });
            self.notify.marks_scope = Some(scope);
        }
    }

    fn in_view(&self, view: &View, thread: &Notification) -> bool {
        match view {
            View::Inbox => !self.is_done(thread),
            View::Saved => self.is_saved(thread),
            View::Done => self.is_done(thread),
            View::Filter(filter) => !self.is_done(thread) && filter.holds(thread),
            View::Repo(repo) => !self.is_done(thread) && thread_repo(self, thread) == *repo,
        }
    }

    /// The chosen view's threads (the `All │ Unread` switch applied), grouped by
    /// repository in the order each first appears, newest first within a group.
    fn shown_threads(&self) -> Vec<Notification> {
        let mut shown = self
            .threads()
            .into_iter()
            .filter(|thread| self.in_view(&self.notify.view, thread))
            .filter(|thread| !self.notify.unread_only || thread.unread)
            .filter(|thread| self.filter_keeps(thread))
            .collect::<Vec<_>>();
        let mut order: Vec<String> = Vec::new();
        for thread in &shown {
            let repo = thread_repo(self, thread);
            if !order.contains(&repo) {
                order.push(repo);
            }
        }
        shown.sort_by_key(|thread| {
            order
                .iter()
                .position(|repo| *repo == thread_repo(self, thread))
        });
        shown
    }

    /// The sidebar: views, filters and repositories with their counts (Inbox, filters and
    /// repositories count unread threads; Saved and Done count every thread they hold).
    fn side_rows(&self, threads: &[Notification]) -> Vec<Side> {
        let open = threads
            .iter()
            .filter(|thread| !self.is_done(thread))
            .collect::<Vec<_>>();
        let unread = |keep: &dyn Fn(&Notification) -> bool| {
            open.iter()
                .filter(|thread| thread.unread && keep(thread))
                .count()
        };
        let mut rows = vec![
            Side::Entry(View::Inbox, "Inbox".into(), unread(&|_| true)),
            Side::Entry(
                View::Saved,
                "Saved".into(),
                threads
                    .iter()
                    .filter(|thread| self.is_saved(thread))
                    .count(),
            ),
            Side::Entry(
                View::Done,
                "Done".into(),
                threads.iter().filter(|thread| self.is_done(thread)).count(),
            ),
            Side::Gap,
            Side::Heading("Filters"),
        ];
        for filter in FILTERS {
            rows.push(Side::Entry(
                View::Filter(filter),
                filter.name().into(),
                unread(&|thread| filter.holds(thread)),
            ));
        }
        let mut repos: Vec<String> = Vec::new();
        for thread in &open {
            let repo = thread_repo(self, thread);
            if !repos.contains(&repo) {
                repos.push(repo);
            }
        }
        if !repos.is_empty() {
            rows.push(Side::Gap);
            rows.push(Side::Heading("Repositories"));
            for repo in repos {
                let count = unread(&|thread| thread_repo(self, thread) == repo);
                rows.push(Side::Entry(View::Repo(repo.clone()), repo, count));
            }
        }
        rows
    }

    fn side_entries(&self) -> Vec<View> {
        self.side_rows(&self.threads())
            .into_iter()
            .filter_map(|row| match row {
                Side::Entry(view, ..) => Some(view),
                _ => None,
            })
            .collect()
    }

    /// Unread threads not marked done, for the top tab; `None` until the page has been read
    /// once.
    pub(in crate::screen) fn unread_notifications(&self) -> Option<usize> {
        self.value(&Key::Notifications)?;
        Some(
            self.threads()
                .iter()
                .filter(|thread| thread.unread && !self.is_done(thread))
                .count(),
        )
    }

    /// Whether the forge offers notifications to this session (a sign-in, and a forge with an
    /// inbox); the tab is skipped otherwise.
    pub(in crate::screen) fn notifications_available(&self) -> bool {
        self.capabilities().notifications.allowed()
    }

    pub(super) fn notification_reads(&mut self, refresh: bool) {
        if self.notify.wanted {
            let read = self.read(Read::Notifications {
                filter: inbox_filter(),
                page: Page {
                    number: 1,
                    per_page: 50,
                },
            });
            self.enqueue(Key::Notifications, read, refresh);
            // A check-suite row shows its run's outcome: one read of the newest runs, under
            // the Actions list's own key, so either page reuses the other's (and the disk's).
            if self.notify_wants_runs() {
                self.enqueue(Key::Runs(0), self.runs_read(0), false);
            }
        }
    }

    /// Notifications is shown with a check-suite thread from this repository.
    pub(super) fn notify_wants_runs(&self) -> bool {
        self.page == PAGE
            && self.threads().iter().any(|thread| {
                matches!(thread.target, NotifyTarget::Run(_)) && self.thread_here(thread)
            })
    }

    fn thread_here(&self, thread: &Notification) -> bool {
        thread.repo.is_empty() || thread.repo.eq_ignore_ascii_case(&self.repo)
    }

    /// The Notifications tab's unread badge on the other forge pages: the inbox's
    /// first page is refreshed once the shown page has its own list, queued behind
    /// everything else, whenever the inbox has been read before.
    pub(super) fn badge_read(&mut self) {
        let ready = match self.page {
            1 | 2 => self.value(&self.list_key()).is_some(),
            3 => self.value(&Key::Workflows).is_some(),
            _ => false,
        };
        if !ready || !self.notifications_available() {
            return;
        }
        self.ensure_marks();
        let read = self.read(Read::Notifications {
            filter: inbox_filter(),
            page: Page {
                number: 1,
                per_page: 50,
            },
        });
        // The badge costs no request of its own the first time: it shows once the inbox
        // has been read, in this session or one before it (the disk cache), and from then
        // on it is kept fresh like any other cached read.
        if self.load_disk(&Key::Notifications, &read) {
            self.enqueue(Key::Notifications, read, false);
        }
    }

    fn selected_thread(&self) -> Option<Notification> {
        let threads = self.shown_threads();
        let at = self.notify.selected.min(threads.len().saturating_sub(1));
        threads.into_iter().nth(at)
    }

    /// Keys on the Notifications page.
    /// The `/` filter: every word in the subject, the repository or the reason.
    fn filter_keeps(&self, thread: &Notification) -> bool {
        let query = self.notify.filter.query.to_lowercase();
        if query.trim().is_empty() {
            return true;
        }
        let haystack = format!(
            "{} {} {}",
            thread.title,
            thread.repo,
            reason_word(self, thread)
        )
        .to_lowercase();
        query.split_whitespace().all(|word| haystack.contains(word))
    }

    /// Notifications types into its `/` filter: every key is the filter's.
    pub(in crate::screen) fn notifications_typing(&self) -> bool {
        self.page == PAGE && self.notify.filter.editing
    }

    fn filter_typing(&mut self, key: &KeyEvent) {
        let input = &mut self.notify.filter;
        match key.code {
            KeyCode::Escape => *input = super::runs::Input::default(),
            KeyCode::Enter => {
                input.editing = false;
                if input.query.is_empty() {
                    *input = super::runs::Input::default();
                }
            }
            KeyCode::Backspace => {
                input.query.pop();
            }
            KeyCode::Char(c) if !key.modifiers.control && !key.modifiers.alt => input.query.push(c),
            _ => return,
        }
        self.notify.selected = 0;
        self.notify.side = false;
    }

    /// `o`: the selected thread on the forge's website.
    fn browse_thread(&mut self) -> Result<Option<Form>, String> {
        let thread = self.selected_thread().ok_or("No notification selected")?;
        let here = self.thread_here(&thread);
        match &thread.target {
            NotifyTarget::Item { kind, number } if here => {
                let page = if *kind == ItemKind::PullRequest { 2 } else { 1 };
                self.activate(Action::Browser(page, *number))
            }
            NotifyTarget::Item { kind, number } => {
                let (page, path) = if *kind == ItemKind::PullRequest {
                    (2, "pull")
                } else {
                    (1, "issues")
                };
                if self
                    .forge_kind()
                    .is_some_and(|kind| kind != crate::forge::provider::ProviderKind::GitHub)
                {
                    return Err("Open it first (Enter), then o opens it on the forge".into());
                }
                let base = self.repository_browser_base(&thread.repo);
                let url = format!("{base}/{path}/{number}");
                self.activate(Action::Web(page, *number, url))
            }
            NotifyTarget::Run(_) => {
                let url = suite_run(self, &thread)
                    .map(|run| string(run, "url").to_owned())
                    .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                    .ok_or("The run is not known yet · Enter opens it in Actions")?;
                self.activate(Action::Web(3, 0, url))
            }
            NotifyTarget::Release(_) => self.open_release(&thread),
            _ => Err("This notification has no page to open".into()),
        }
    }

    fn release_url(&self, thread: &Notification) -> Option<String> {
        let NotifyTarget::Release(url) = &thread.target else {
            return None;
        };
        let address = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let (host, path) = address.split_once('/')?;
        // GitHub's notification can name a REST release ID, not its tag. There is no
        // release reader: show the repository's Releases page instead of REST JSON.
        if host.eq_ignore_ascii_case("api.github.com") || path.starts_with("api/v3/repos/") {
            let base = self.repository_browser_base(&thread_repo(self, thread));
            Some(format!("{base}/releases"))
        } else {
            Some(url.clone())
        }
    }

    fn open_release(&mut self, thread: &Notification) -> Result<Option<Form>, String> {
        let url = self
            .release_url(thread)
            .ok_or("This notification has no page to open")?;
        self.activate(Action::Web(PAGE, 0, url))
    }

    pub(super) fn notification_key(&mut self, key: &KeyEvent) -> Result<Option<Form>, String> {
        self.ensure_marks();
        if self.notify.filter.editing {
            self.filter_typing(key);
            return Ok(None);
        }
        // The shared key table (`keymap.rs`): `Space e` hides the sidebar and brings it back.
        match self.chord_key(key) {
            keymap::Step::Wait | keymap::Step::Cancel => return Ok(None),
            keymap::Step::Run(keymap::Named::ToggleSidebar) => {
                self.toggle_sidebar();
                if self.sidebar_hidden() {
                    self.notify.side = false;
                }
                return Ok(None);
            }
            keymap::Step::Run(keymap::Named::Toggle) => {
                return self.notification_key(&KeyEvent::plain(KeyCode::Enter));
            }
            keymap::Step::Pass => {}
        }
        if key.code == KeyCode::Tab {
            if !self.sidebar_hidden() {
                self.notify.side = !self.notify.side;
                self.sync_side_cursor();
            }
            return Ok(None);
        }
        if matches!(key.code, KeyCode::Char('r' | 'R')) {
            self.notice = "Refreshing notifications…".into();
            self.notice_error = false;
            self.notification_reads(true);
            return Ok(None);
        }
        if key.code == KeyCode::Char('/') {
            self.notify.filter.editing = true;
            self.notify.side = false;
            return Ok(None);
        }
        if key.code == KeyCode::Escape && self.notify.filter.active() {
            self.notify.filter = super::runs::Input::default();
            self.notify.selected = 0;
            return Ok(None);
        }
        if self.notify.side && !self.sidebar_hidden() {
            return self.sidebar_key(key);
        }
        if key.code == KeyCode::Char('o') {
            return self.browse_thread();
        }
        let count = self.shown_threads().len();
        let before = self.notify.selected;
        if let Some(direction) = control_direction(key)
            && match direction {
                SpatialDirection::Up => before == 0,
                SpatialDirection::Down => before >= count.saturating_sub(1),
                SpatialDirection::Left => self.sidebar_hidden(),
                SpatialDirection::Right => true,
            }
        {
            if direction == SpatialDirection::Right
                && let Some(thread) = self.selected_thread()
                && match &thread.target {
                    NotifyTarget::Item { .. } => true,
                    NotifyTarget::Run(_) => self.thread_here(&thread),
                    _ => false,
                }
            {
                return self.open_thread(&thread);
            }
            self.focus_exit = Some(direction);
            return Ok(None);
        }
        let moving = matches!(
            key.code,
            KeyCode::Down
                | KeyCode::Up
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char('j' | 'k' | 'g' | 'G')
        );
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.notify.selected = (self.notify.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.notify.selected = self.notify.selected.saturating_sub(1)
            }
            KeyCode::Home | KeyCode::Char('g') => self.notify.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.notify.selected = count.saturating_sub(1),
            KeyCode::Left | KeyCode::Char('h') if !self.sidebar_hidden() => {
                self.notify.side = true;
                self.sync_side_cursor();
            }
            KeyCode::Char('[' | ']') => {
                self.notify.unread_only = !self.notify.unread_only;
                self.notify.selected = 0;
            }
            KeyCode::Char('m') => {
                let thread = self.selected_thread().ok_or("No notification selected")?;
                if thread.unread {
                    self.mark(Write::MarkRead(thread.id))?;
                }
            }
            KeyCode::Char('M') => self.mark(Write::MarkAllRead)?,
            KeyCode::Char('e') => {
                let thread = self.selected_thread().ok_or("No notification selected")?;
                self.toggle_done(&thread)?;
                self.notify.selected = self
                    .notify
                    .selected
                    .min(self.shown_threads().len().saturating_sub(1));
            }
            KeyCode::Char('s') => {
                let thread = self.selected_thread().ok_or("No notification selected")?;
                let saved = !self.is_saved(&thread);
                self.edit_mark(SAVED_KEY, &thread, saved);
                self.notice = if saved {
                    format!("✓ Saved {}", target_words(&thread))
                } else {
                    format!("✓ Removed {} from Saved", target_words(&thread))
                };
                self.notice_error = false;
                self.notify.selected = self
                    .notify
                    .selected
                    .min(self.shown_threads().len().saturating_sub(1));
            }
            KeyCode::Char('u') => {
                let thread = self.selected_thread().ok_or("No notification selected")?;
                self.mark(Write::Unsubscribe(thread.id))?;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let thread = self.selected_thread().ok_or("No notification selected")?;
                return self.open_thread(&thread);
            }
            _ => {}
        }
        // A receipt is about the row it was made on: moving off that row clears it, so
        // `✓ Saved …` never sits beside another thread (R15 toast).
        if moving && self.notify.selected != before && !self.notice_error {
            self.notice.clear();
        }
        Ok(None)
    }

    /// Keys while the sidebar has them: move among its entries, `Enter` shows one.
    fn sidebar_key(&mut self, key: &KeyEvent) -> Result<Option<Form>, String> {
        let entries = self.side_entries();
        let last = entries.len().saturating_sub(1);
        if let Some(direction) = control_direction(key)
            && match direction {
                SpatialDirection::Up => self.notify.side_at == 0,
                SpatialDirection::Down => self.notify.side_at >= last,
                SpatialDirection::Left => true,
                SpatialDirection::Right => false,
            }
        {
            self.focus_exit = Some(direction);
            return Ok(None);
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.notify.side_at = (self.notify.side_at + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.notify.side_at = self.notify.side_at.saturating_sub(1)
            }
            KeyCode::Home | KeyCode::Char('g') => self.notify.side_at = 0,
            KeyCode::End | KeyCode::Char('G') => self.notify.side_at = last,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(view) = entries.get(self.notify.side_at.min(last)) {
                    self.notify.view = view.clone();
                    self.notify.selected = 0;
                }
                self.notify.side = false;
            }
            KeyCode::Escape => self.notify.side = false,
            _ => {}
        }
        Ok(None)
    }

    /// The sidebar cursor starts on the view the list shows.
    fn sync_side_cursor(&mut self) {
        let view = self.notify.view.clone();
        self.notify.side_at = self
            .side_entries()
            .iter()
            .position(|entry| *entry == view)
            .unwrap_or(0);
    }

    fn edit_mark(&mut self, key: &str, thread: &Notification, present: bool) {
        self.ensure_marks();
        self.edit_loaded_mark(key, thread, present);
    }

    fn edit_loaded_mark(&mut self, key: &str, thread: &Notification, present: bool) {
        let Some(scope) = self.notify.marks_scope.as_ref() else {
            return;
        };
        let mark = format!("{}{}", scope.prefix, thread.id.0);
        let marks = self.notify.marks.get_or_insert_with(Marks::default);
        let list = if key == SAVED_KEY {
            &mut marks.saved
        } else {
            &mut marks.done
        };
        set_mark(list, &scope.prefix, &mark, present);
        store_mark(&self.root, key, scope, &thread.id.0, present);
    }

    /// `e`: Done, as on GitHub; in Done it moves the thread back to the inbox. Done is this
    /// repository's own mark; a thread still unread is also marked read on the forge.
    fn toggle_done(&mut self, thread: &Notification) -> Result<(), String> {
        self.ensure_marks();
        let done = !self.is_done(thread);
        if done && thread.unread && self.notifications_available() {
            self.mark(Write::MarkRead(thread.id.clone()))?;
        }
        self.edit_mark(DONE_KEY, thread, done);
        self.notice_error = false;
        if !done {
            self.notice = format!("✓ Moved {} to the inbox", target_words(thread));
            return Ok(());
        }
        self.notice = format!("✓ Done · {}", target_words(thread));
        Ok(())
    }

    fn mark(&mut self, write: Write) -> Result<(), String> {
        if let Some(reason) = self.capabilities().notifications.reason() {
            return Err(reason.into());
        }
        if self.writing() {
            return Err("Wait for the current action to finish".into());
        }
        self.cache.remove(&Key::Write);
        self.enqueue(Key::Write, self.write_op(self.target(), write), false);
        self.notice.clear();
        self.notice_error = false;
        Ok(())
    }

    /// `Enter`: the thread's pull request or issue in its own tab (this repository), whose
    /// `Esc` comes back here; its run in Actions; or the browser for anything else.
    fn open_thread(&mut self, thread: &Notification) -> Result<Option<Form>, String> {
        let here = self.thread_here(thread);
        match &thread.target {
            NotifyTarget::Item { kind, number } if here => {
                let page = if *kind == ItemKind::PullRequest { 2 } else { 1 };
                self.notify.read.insert(thread.id.0.clone());
                self.page(page);
                self.open_link(page, *number);
                self.notify.back = true;
                self.notify.opened = Some(thread.id.0.clone());
                Ok(None)
            }
            NotifyTarget::Item { kind, number } => {
                let page = if *kind == ItemKind::PullRequest { 2 } else { 1 };
                self.activate(Action::OpenRemote(inbox::Target {
                    host: self.host.clone(),
                    repo: thread.repo.clone(),
                    page,
                    number: *number,
                }))
            }
            NotifyTarget::Run(id) if here => {
                // GitHub's check suite names no run: the newest run read for its workflow and
                // branch is the one; with neither, Actions opens on the newest runs.
                let run = id
                    .0
                    .rsplit('/')
                    .next()
                    .and_then(|tail| tail.parse::<u64>().ok())
                    .or_else(|| suite_run(self, thread).and_then(|run| run["databaseId"].as_u64()));
                self.notify.read.insert(thread.id.0.clone());
                self.page(3);
                // Its `Esc` comes back here, as a reader's does.
                self.notify.back = true;
                self.notify.opened = Some(thread.id.0.clone());
                match run {
                    Some(run) => self.activate(Action::Run(run)),
                    None => Ok(None),
                }
            }
            NotifyTarget::Release(_) => self.open_release(thread),
            _ => Err("This notification has no target to open here".into()),
        }
    }

    /// `Esc` in a reader or run opened from here: Notifications again, the cursor on the
    /// thread that was opened (where the view still shows it).
    pub(super) fn back_to_notifications(&mut self) {
        let opened = self.notify.opened.take();
        self.page(PAGE);
        let shown = self.shown_threads();
        if let Some(at) = opened.and_then(|id| shown.iter().position(|thread| thread.id.0 == id)) {
            self.notify.selected = at;
        }
        self.notify.selected = self.notify.selected.min(shown.len().saturating_sub(1));
    }

    /// Commit the change only on the shared write receipt, before the list is refreshed.
    pub(super) fn notification_written(&mut self, write: &Write) -> Option<String> {
        Some(match write {
            Write::MarkRead(id) => {
                self.notify.read.insert(id.0.clone());
                match self.threads().iter().find(|thread| thread.id == *id) {
                    Some(thread) if self.is_done(thread) => {
                        format!("✓ Done · {}", target_words(thread))
                    }
                    Some(thread) => format!("✓ Marked {} read", target_words(thread)),
                    None => "✓ Marked read".into(),
                }
            }
            Write::MarkAllRead => {
                let threads = self.threads();
                self.notify
                    .read
                    .extend(threads.into_iter().map(|thread| thread.id.0));
                "✓ Marked every notification read".into()
            }
            Write::Unsubscribe(id) => {
                self.notify.gone.insert(id.0.clone());
                self.notify.selected = self
                    .notify
                    .selected
                    .min(self.shown_threads().len().saturating_sub(1));
                "✓ Unsubscribed".into()
            }
            _ => return None,
        })
    }

    pub(super) fn notification_write_error(&self, write: &Write, error: &str) -> Option<String> {
        let action = match write {
            Write::MarkRead(_) => "Couldn't mark it read",
            Write::MarkAllRead => "Couldn't mark all notifications read",
            Write::Unsubscribe(_) => "Couldn't unsubscribe",
            _ => return None,
        };
        let head = error.split(" · ").next().unwrap_or_default();
        Some(match head {
            "No permission"
            | "Rate limited"
            | "Network error"
            | "Not found"
            | "Unexpected response"
            | "Timed out waiting for the forge" => {
                format!("{action}: {}", self.explain(error).headline)
            }
            "Unsupported here" => format!("{action}: not available on this site"),
            _ if head.starts_with("Not signed in")
                || head.starts_with("Sign in to ")
                || head.ends_with("requires sign-in") =>
            {
                format!(
                    "{action}: {}{}",
                    self.explain(error).headline,
                    sign_in_notice_suffix()
                )
            }
            _ => action.into(),
        })
    }
}

/// The inbox reads read threads too (GitHub's `all=true`), as github.com's inbox shows
/// them: a thread marked read or done stays in its view instead of leaving on the next read.
fn inbox_filter() -> NotifyFilter {
    NotifyFilter {
        all: true,
        since: None,
    }
}

/// The thread's repository, `owner/name`; this repository's when the forge leaves it out.
fn thread_repo(workspace: &Workspace, thread: &Notification) -> String {
    if thread.repo.is_empty() {
        workspace.repo.clone()
    } else {
        thread.repo.clone()
    }
}

/// What a thread is about: its subject (`Reconnect loses queued progress #7`), else `orbit #12`.
fn target_words(thread: &Notification) -> String {
    let repo = thread.repo.rsplit('/').next().unwrap_or(&thread.repo);
    // The subject the row shows, so a receipt names what it acted on (`Saved Reconnect loses
    // queued progress #7`), not only where it lives; long titles keep their start.
    let title = chrome::ellipsize(thread.title.trim(), 48);
    if !title.is_empty() {
        return match &thread.target {
            NotifyTarget::Item { number, .. } => format!("{title} #{number}"),
            _ => title,
        };
    }
    match &thread.target {
        NotifyTarget::Item { number, .. } => format!("{repo} #{number}"),
        NotifyTarget::Run(id) if id.0.is_empty() => format!("{repo} workflow run"),
        NotifyTarget::Run(id) => format!("{repo} run {}", id.0),
        NotifyTarget::Release(tag) => format!("{repo} {tag}"),
        NotifyTarget::Other(other) => format!("{repo} {other}"),
    }
}

/// The row's type glyph, coloured by the item's state when a list this session read knows it
/// (GitHub's green open, purple merged, red closed), else plain.
fn target_mark(
    workspace: &Workspace,
    thread: &Notification,
    mode: ExplorerIconMode,
) -> (&'static str, GitRole) {
    match &thread.target {
        NotifyTarget::Item { kind, number } => {
            let pull = *kind == ItemKind::PullRequest;
            let here = thread.repo.is_empty() || thread.repo.eq_ignore_ascii_case(&workspace.repo);
            let known = here
                .then(|| known_row(workspace, if pull { 2 } else { 1 }, *number))
                .flatten();
            match (known.map(|row| row.lifecycle), pull) {
                (Some(Lifecycle::Merged), _) => (glyph(Glyph::PullMerged, mode), GitRole::Ref),
                (Some(Lifecycle::Draft), _) => (glyph(Glyph::PullRequest, mode), GitRole::Muted),
                (Some(Lifecycle::Closed), true) => {
                    (glyph(Glyph::PullRequest, mode), GitRole::Removed)
                }
                (Some(Lifecycle::Closed), false) => (glyph(Glyph::IssueClosed, mode), GitRole::Ref),
                (Some(Lifecycle::ClosedNotPlanned), _) => {
                    (glyph(Glyph::NotPlanned, mode), GitRole::Muted)
                }
                (Some(Lifecycle::Open), true) => (glyph(Glyph::PullRequest, mode), GitRole::Added),
                (Some(Lifecycle::Open), false) => (glyph(Glyph::Issue, mode), GitRole::Added),
                (None, true) => (glyph(Glyph::PullRequest, mode), GitRole::Text),
                (None, false) => (glyph(Glyph::Issue, mode), GitRole::Text),
            }
        }
        // A check suite: its newest run's outcome as Actions draws it; before that run is
        // read, the outcome its subject names (`… failed for …`).
        NotifyTarget::Run(_) => {
            match (suite_run(workspace, thread), suite_subject(&thread.title)) {
                (Some(run), _) => runs::run_mark(run, mode),
                (None, Some((_, Some(conclusion), _))) => runs::run_mark(
                    &serde_json::json!({"status": "completed", "conclusion": conclusion}),
                    mode,
                ),
                _ => (glyph(Glyph::Actions, mode), GitRole::Text),
            }
        }
        _ => (glyph(Glyph::Notification, mode), GitRole::Text),
    }
}

/// A check-suite subject as GitHub writes it, `CI workflow run failed for main branch` (or
/// `Quality checks failed for fix/x`): its workflow, the conclusion its verb names (none for
/// `completed`) and its branch.
fn suite_subject(title: &str) -> Option<(String, Option<&'static str>, String)> {
    let at = title.rfind(" for ")?;
    let tail = title[at + 5..].trim();
    let branch = tail
        .strip_suffix(" branch")
        .unwrap_or(tail)
        .trim()
        .to_owned();
    let head = title[..at].trim_end();
    const VERBS: [(&str, &str); 6] = [
        ("failed", "failure"),
        ("succeeded", "success"),
        ("passed", "success"),
        ("cancelled", "cancelled"),
        ("canceled", "cancelled"),
        ("completed", ""),
    ];
    let (rest, conclusion) = VERBS.iter().find_map(|(verb, conclusion)| {
        let rest = head.strip_suffix(verb)?;
        rest.ends_with(' ').then(|| {
            (
                rest.trim_end(),
                (!conclusion.is_empty()).then_some(*conclusion),
            )
        })
    })?;
    let workflow = rest
        .strip_suffix(" workflow run")
        .or_else(|| rest.strip_suffix(" workflow"))
        .unwrap_or(rest)
        .trim()
        .to_owned();
    (!branch.is_empty()).then_some((workflow, conclusion, branch))
}

/// The newest run this session read for a check-suite thread's workflow and branch.
fn suite_run<'a>(workspace: &'a Workspace, thread: &Notification) -> Option<&'a Value> {
    let (workflow, _, branch) = suite_subject(&thread.title)?;
    if !workspace.thread_here(thread) {
        return None;
    }
    workspace
        .cache
        .iter()
        .filter(|(key, _)| matches!(key, Key::Runs(_)))
        .filter_map(|(_, reply)| reply.as_ref().ok()?.value.as_array())
        .flatten()
        .filter(|run| {
            run["headBranch"].as_str() == Some(branch.as_str())
                && (workflow.is_empty()
                    || run["workflowName"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&workflow)))
        })
        .max_by_key(|run| {
            records::epoch(run["createdAt"].as_str().unwrap_or_default()).unwrap_or(0)
        })
}

/// The list row of `number` on `page` from any list this session read, for its state.
fn known_row(workspace: &Workspace, page: usize, number: u64) -> Option<records::ItemRecord> {
    workspace.cache.iter().find_map(|(key, reply)| {
        let Key::List(at, ..) = key else { return None };
        if *at != page {
            return None;
        }
        reply
            .as_ref()
            .ok()?
            .value
            .as_array()?
            .iter()
            .find(|row| row["number"].as_u64() == Some(number))
            .map(records::ItemRecord::from_row)
    })
}

fn put(grid: &mut CellGrid, x: u16, y: u16, spans: &[Span], right: u16) -> u16 {
    chrome::put_spans(grid, x, y, spans, right)
}

/// The sidebar's width: a fifth of the screen, room for `Review requested` and a count.
fn side_width(width: u16) -> u16 {
    (width / 5).clamp(26, 34)
}

const MARK_ALL_FAILURE: &str = "Couldn't mark all notifications read";

fn sign_in_notice_suffix() -> String {
    format!(" · {} sign in", super::states::SIGN_IN_KEY)
}

/// The Notifications page: top bar, sidebar, list and footer.
pub(in crate::screen) fn frame(app: &mut GitScreen, width: u16, height: u16) -> Frame {
    let mut frame = Frame {
        grid: CellGrid::new(width, height),
        cursor: None,
    };
    let mode = layout::mode(width, height);
    if mode == Mode::TooSmall {
        let text = format!(
            "Dusty Git needs {}×{}",
            layout::MIN_WIDTH,
            layout::MIN_HEIGHT
        );
        frame.grid.put_text_clipped(
            width.saturating_sub(chrome::width(&text)) / 2,
            height / 2,
            &text,
            bold(GitRole::Modified, GitSurface::Canvas),
            width,
        );
        return frame;
    }
    if !app.github.notify.wanted {
        app.github.notify.wanted = true;
        app.github.ensure(false);
    }
    app.github.ensure_marks();
    chrome::fill(
        &mut frame.grid,
        Rect::new(0, 0, width, height),
        st(GitRole::Text, GitSurface::Canvas),
    );
    super::super::shell::top_bar(app, &mut frame.grid, width);
    let icons = app.icon_mode;
    let workspace = &app.github;
    let body = Rect::new(0, 1, width, height.saturating_sub(2));
    // The sidebar is always there, as on every forge screen, until `toggle_sidebar`.
    let hidden = workspace.sidebar_hidden();
    let list_rect = if hidden {
        body
    } else {
        let left = side_width(width);
        sidebar(&mut frame, workspace, Rect::new(0, 1, left, body.h), icons);
        Rect::new(left, 1, width - left, body.h)
    };
    let shown = workspace.shown_threads();
    let selected = workspace.notify.selected.min(shown.len().saturating_sub(1));
    list(&mut frame, workspace, list_rect, &shown, selected, icons);
    // This page's own notices (`✓ Marked orbit #13 read`) sit on the list's status row,
    // under the rows they are about, so a toast never takes keys from the footer (r11
    // INB-12) nor overwrites the frame. The app's notices keep the footer's
    // right end.
    if !workspace.notice.is_empty() && list_rect.w > 16 {
        let (mark, role) = if workspace.notice_error {
            ("✕ ", GitRole::Removed)
        } else {
            ("", GitRole::Modified)
        };
        let inner = list_rect.inner();
        let (left, right) = (inner.x + 1, inner.right().saturating_sub(1));
        let room = usize::from(right.saturating_sub(left));
        let suffix = sign_in_notice_suffix();
        let suffix_width = usize::from(chrome::width(&suffix));
        let text = match workspace.notice.strip_suffix(&suffix) {
            Some(notice) if workspace.notice_error && room > suffix_width => {
                let room = room - suffix_width;
                // Keep the action and signed-out reason together; the host can yield first.
                let notice = match notice.strip_prefix(MARK_ALL_FAILURE) {
                    // When the reason is longer than ": Not signed in", the
                    // ellipsis takes one more cell.
                    Some(reason)
                        if reason.starts_with(": Not signed in")
                            && usize::from(chrome::width(mark))
                                + MARK_ALL_FAILURE.len()
                                + ": Not signed in".len()
                                + usize::from(reason.len() > ": Not signed in".len())
                                > room =>
                    {
                        "Mark all failed: not signed in"
                    }
                    _ => notice,
                };
                let shortened = chrome::ellipsize(&format!("{mark}{notice}"), room);
                format!("{shortened}{suffix}")
            }
            _ => chrome::ellipsize(&format!("{mark}{}", workspace.notice), room),
        };
        let row = inner.bottom().saturating_sub(1);
        // Over `Showing N` when both do not fit: the notice is the news.
        let at = right.saturating_sub(chrome::width(&text)).max(left);
        if at < left + 12 {
            frame.grid.put_text_clipped(
                left,
                row,
                &" ".repeat(room),
                st(GitRole::Text, PANE),
                right,
            );
        }
        frame
            .grid
            .put_text_clipped(at, row, &text, bold(role, PANE), right);
    }
    let footer = Rect::new(0, height - 1, width, 1);
    let message = (!app.notice.is_empty()).then_some((app.notice.as_str(), true));
    let keys = workspace.key_table();
    let toggle = keys.keys(keymap::Named::ToggleSidebar).unwrap_or_default();
    if let Some(form) = &app.form {
        // An open sheet (`o`'s browser confirmation): its keys, not the list's.
        let verb = match &form.action {
            FormAction::Github(action) => super::create::submit_verb(action),
            _ => None,
        };
        let mut hints = vec![("Enter", verb.unwrap_or("confirm")), ("Esc", "cancel")];
        if form.fields.len() > 1 {
            hints.extend([("Tab", "next field"), ("Shift+Tab", "previous")]);
        }
        chrome::footer(&mut frame.grid, footer, &hints, "", message);
    } else if workspace.notify.filter.editing {
        chrome::footer(
            &mut frame.grid,
            footer,
            &[("Enter", "filter"), ("Esc", "cancel")],
            "",
            message,
        );
    } else if workspace.leader_pending() {
        // Each chord whole (`Space e sidebar`), as on Actions.
        let chords = keys
            .after_leader()
            .into_iter()
            .map(|(key, label)| (format!("{} {key}", keys.leader_name()), label))
            .collect::<Vec<_>>();
        let mut hints = chords
            .iter()
            .map(|(key, label)| (key.as_str(), *label))
            .collect::<Vec<_>>();
        hints.push(("Esc", "cancel"));
        chrome::footer(&mut frame.grid, footer, &hints, "", message);
    } else if workspace.notify.side && !hidden {
        let hints = vec![
            ("Enter", "show"),
            ("j/k", "move"),
            ("Tab", "list"),
            (toggle.as_str(), "sidebar"),
            ("r", "refresh"),
        ];
        chrome::footer_keys_first(&mut frame.grid, footer, &hints, &workspace.repo, message);
    } else {
        // `m mark read` only while the selected thread is unread, and the repo
        // tag yields to the keys.
        let thread = workspace.selected_thread();
        let unread = thread.as_ref().is_some_and(|thread| thread.unread);
        let done = thread
            .as_ref()
            .is_some_and(|thread| workspace.is_done(thread));
        let saved = thread
            .as_ref()
            .is_some_and(|thread| workspace.is_saved(thread));
        let mut hints = Vec::new();
        match thread.as_ref() {
            Some(thread) if matches!(thread.target, NotifyTarget::Release(_)) => {
                if workspace.release_url(thread).is_some() {
                    hints.push(("Enter", "browser"));
                }
            }
            Some(_) => hints.push(("Enter", "open")),
            None => {}
        }
        hints.extend([
            ("e", if done { "inbox" } else { "done" }),
            ("s", if saved { "unsave" } else { "save" }),
        ]);
        if unread {
            hints.push(("m", "mark read"));
        }
        // In priority order; the width decides how many show (`/ filter` and `o browser`
        // ahead of the rarer `u` and `M`, r15 narrow pass). The sidebar toggle outranks them
        // too, and Tab moves between the panels, as on Actions.
        hints.extend([("/", "filter"), ("[ ]", "unread"), ("o", "browser")]);
        hints.push((toggle.as_str(), "sidebar"));
        if !hidden {
            hints.push(("Tab", "panels"));
        }
        hints.extend([("u", "unsubscribe"), ("M", "mark all")]);
        hints.retain(|(key, _)| !key.is_empty());
        chrome::footer_keys_first_fill(&mut frame.grid, footer, &hints, &workspace.repo, message);
    }
    super::accounts::paint(&mut frame, &app.github, 0, width, height);
    // Its sheets (`o`'s browser confirmation) over the inbox, as on every forge page.
    if let Some(form) = &app.form {
        let lay = layout::layout(width, height, false, [false; 4]);
        if !super::super::shell::sheet::paint(app, &mut frame, &lay) {
            super::super::render::render_form(&mut frame, form, width, height);
        }
    }
    frame
}

/// `Inbox`, `Saved`, `Done`; `Filters`; `Repositories`: GitHub's notification sidebar.
#[allow(clippy::explicit_counter_loop)] // rows stop at the pane bottom, not the list end
fn sidebar(frame: &mut Frame, workspace: &Workspace, rect: Rect, mode: ExplorerIconMode) {
    let focused = workspace.notify.side;
    let terms = workspace.terms();
    let title: Vec<Span> = vec![
        (
            format!("{} ", glyph(Glyph::Notification, mode)),
            st(GitRole::Focus, PANE),
        ),
        (
            terms.notifications_tab.to_owned(),
            bold(GitRole::Text, PANE),
        ),
    ];
    chrome::pane(&mut frame.grid, rect, &title, "", focused);
    let inner = rect.inner();
    let (x, right) = (inner.x + 1, inner.right().saturating_sub(1));
    let mut y = inner.y;
    let mut entry = 0usize;
    for row in workspace.side_rows(&workspace.threads()) {
        if y >= inner.bottom() {
            break;
        }
        match row {
            Side::Gap => {}
            Side::Heading(name) => {
                frame
                    .grid
                    .put_text_clipped(x, y, name, bold(GitRole::Muted, PANE), right);
            }
            Side::Entry(view, name, count) => {
                let chosen = view == workspace.notify.view;
                let cursor = if focused {
                    entry == workspace.notify.side_at
                } else {
                    chosen
                };
                // Unfocused, the chosen view keeps an inactive band and no edge: the bar is
                // the focused pane's alone (R15 focus).
                let surface = match (cursor, focused) {
                    (true, true) => GitSurface::Cursor,
                    (true, false) => GitSurface::CursorInactive,
                    _ => PANE,
                };
                if cursor {
                    chrome::fill(
                        &mut frame.grid,
                        Rect::new(inner.x, y, inner.w, 1),
                        st(GitRole::Text, surface),
                    );
                    chrome::selection_edge(
                        &mut frame.grid,
                        inner.x,
                        y..y + 1,
                        focused,
                        surface,
                        mode,
                    );
                    if focused {
                        frame.cursor =
                            Some((inner.x, y, crossterm::cursor::SetCursorStyle::SteadyBar));
                    }
                }
                let count = if count > 0 {
                    count.to_string()
                } else {
                    String::new()
                };
                let count_at = right.saturating_sub(chrome::width(&count));
                let room = usize::from(count_at.saturating_sub(x + 1));
                let style = if chosen {
                    bold(GitRole::Text, surface)
                } else {
                    st(GitRole::Text, surface)
                };
                frame
                    .grid
                    .put_text_clipped(x, y, &chrome::ellipsize(&name, room), style, count_at);
                if !count.is_empty() {
                    frame.grid.put_text_clipped(
                        count_at,
                        y,
                        &count,
                        st(GitRole::Muted, surface),
                        right,
                    );
                }
                entry += 1;
            }
        }
        y += 1;
    }
}

/// The name of the view the list shows, for its pane title.
fn view_name(view: &View) -> String {
    match view {
        View::Inbox => "Inbox".into(),
        View::Saved => "Saved".into(),
        View::Done => "Done".into(),
        View::Filter(filter) => filter.name().into(),
        View::Repo(repo) => repo.clone(),
    }
}

fn list(
    frame: &mut Frame,
    workspace: &Workspace,
    rect: Rect,
    shown: &[Notification],
    selected: usize,
    mode: ExplorerIconMode,
) {
    let focused = !workspace.notify.side || workspace.sidebar_hidden();
    let title: Vec<Span> = vec![(view_name(&workspace.notify.view), bold(GitRole::Text, PANE))];
    chrome::pane(&mut frame.grid, rect, &title, "", focused);
    let inner = rect.inner();
    let (x, right) = (inner.x + 1, inner.right().saturating_sub(1));
    let mut y = inner.y;
    // GitHub's `All │ Unread` switch, counted from the view's threads.
    let in_view = workspace
        .threads()
        .into_iter()
        .filter(|thread| workspace.in_view(&workspace.notify.view, thread))
        .collect::<Vec<_>>();
    let unread = in_view.iter().filter(|thread| thread.unread).count();
    let counted = |name: &str, count: usize| {
        if count > 0 {
            format!("{name} {count}")
        } else {
            name.to_owned()
        }
    };
    let tabs = [counted("All", in_view.len()), counted("Unread", unread)];
    let active = usize::from(workspace.notify.unread_only);
    let tabs = chrome::fit_tabs(&tabs, active, inner.w);
    chrome::sub_tabs(
        &mut frame.grid,
        inner.x,
        y,
        inner.right(),
        &tabs,
        active,
        focused,
    );
    y += 1;
    // The `/` filter's row, as Actions shows its own (`󰍉 queue▏  Esc clears`).
    let filter = &workspace.notify.filter;
    if filter.active() {
        let mut spans: Vec<Span> = vec![
            (
                format!("{} ", glyph(Glyph::Search, mode)),
                st(GitRole::Modified, PANE),
            ),
            (filter.query.clone(), bold(GitRole::Modified, PANE)),
        ];
        if filter.editing {
            spans.push(("▏".into(), st(GitRole::Modified, PANE)));
        }
        spans.push(("  Esc clears".into(), st(GitRole::Muted, PANE)));
        put(&mut frame.grid, x, y, &spans, right);
        if filter.editing {
            frame.cursor = Some((
                x + 2 + chrome::width(&filter.query),
                y,
                crossterm::cursor::SetCursorStyle::SteadyBar,
            ));
        }
        y += 1;
    }
    for column in inner.x..inner.right() {
        frame
            .grid
            .set_cell(column, y, "─", st(GitRole::Border, PANE));
    }
    y += 1;
    if let Some((text, _)) = workspace.host_notice(workspace.value(&Key::Notifications).is_some()) {
        frame.grid.put_text_clipped(
            x,
            y,
            &chrome::ellipsize(&text, usize::from(right - x)),
            st(GitRole::Muted, PANE),
            right,
        );
        y += 2;
    }
    let bottom = inner.bottom();
    if workspace.value(&Key::Notifications).is_none() {
        let text = match workspace.cache.get(&Key::Notifications) {
            Some(Err(error)) => format!("✕ {}", workspace.explain(error).line()),
            _ if !workspace.notifications_available() => workspace
                .capabilities()
                .notifications
                .reason()
                .unwrap_or("Notifications are not available here")
                .to_owned(),
            _ => match workspace.blocked(&Key::Notifications) {
                Some((headline, hint)) => format!("{headline} · {hint}"),
                None => "Loading…".to_owned(),
            },
        };
        frame.grid.put_text_clipped(
            x,
            y,
            &chrome::ellipsize(&text, usize::from(right - x)),
            st(GitRole::Muted, PANE),
            right,
        );
        return;
    }
    if shown.is_empty() {
        let text = match &workspace.notify.view {
            _ if workspace.notify.unread_only && !in_view.is_empty() => {
                "No unread notifications here · [ ] shows all".to_owned()
            }
            View::Inbox => format!(
                "Inbox zero · nothing new in {}",
                workspace.terms().notifications_tab.to_lowercase()
            ),
            View::Saved => "Nothing saved · s saves a notification for later".to_owned(),
            View::Done => "Nothing done yet · e marks a notification done".to_owned(),
            _ => "Nothing here · Tab picks another view".to_owned(),
        };
        frame.grid.put_text_clipped(
            x,
            y,
            &chrome::ellipsize(&text, usize::from(right - x)),
            st(GitRole::Muted, PANE),
            right,
        );
        return;
    }
    // The last row is the list's status, as on Issues and Pull requests:
    // `Showing 6` on the left, this page's notices on the right.
    let bottom = bottom.saturating_sub(1);
    frame.grid.put_text_clipped(
        x,
        bottom,
        &format!("Showing {}", shown.len()),
        st(GitRole::Muted, PANE),
        right,
    );
    // Repository headings and their threads, as GitHub groups the inbox.
    enum Line<'a> {
        Heading(String, usize, usize),
        Thread(usize, &'a Notification),
    }
    let mut lines: Vec<Line> = Vec::new();
    for (index, thread) in shown.iter().enumerate() {
        let repo = thread_repo(workspace, thread);
        if index == 0 || thread_repo(workspace, &shown[index - 1]) != repo {
            let group = shown
                .iter()
                .filter(|other| thread_repo(workspace, other) == repo);
            let (count, unread) = group.fold((0, 0), |(count, unread), thread| {
                (count + 1, unread + usize::from(thread.unread))
            });
            lines.push(Line::Heading(repo, count, unread));
        }
        lines.push(Line::Thread(index, thread));
    }
    let visible = usize::from(bottom.saturating_sub(y)).max(1);
    let at = lines
        .iter()
        .position(|line| matches!(line, Line::Thread(index, _) if *index == selected))
        .unwrap_or(0);
    // The selected row's heading stays in view with it where it can.
    let first = (at + 1).saturating_sub(visible);
    let now = records::now();
    // One time slot for every row, so the reason chips line up (`2m ago` beside `14m ago`).
    let slot = shown
        .iter()
        .map(|thread| chrome::width(&super::fresh::relative(&thread.updated.0, now)))
        .max()
        .unwrap_or(0);
    for line in lines.iter().skip(first).take(visible) {
        match line {
            Line::Heading(repo, count, unread) => {
                frame
                    .grid
                    .put_text_clipped(x, y, repo, bold(GitRole::Text, PANE), right);
                // The group says what it holds and how much of it is new; the sidebar's
                // repository count is the unread part alone (R15 counts).
                let count = if *unread > 0 {
                    format!("{count} · {unread} unread")
                } else {
                    count.to_string()
                };
                frame.grid.put_text_clipped(
                    right.saturating_sub(chrome::width(&count)),
                    y,
                    &count,
                    st(GitRole::Muted, PANE),
                    right,
                );
            }
            Line::Thread(index, thread) => {
                let current = *index == selected;
                thread_row(
                    frame,
                    workspace,
                    thread,
                    Rect::new(inner.x, y, inner.w, 1),
                    current && focused,
                    current,
                    now,
                    slot,
                    mode,
                );
            }
        }
        y += 1;
    }
}

/// One thread: unread dot, type glyph, title and `#13`; on the right the reason chip and the
/// time, as GitHub's row has them.
#[allow(clippy::too_many_arguments)]
fn thread_row(
    frame: &mut Frame,
    workspace: &Workspace,
    thread: &Notification,
    row: Rect,
    focused: bool,
    current: bool,
    now: i64,
    slot: u16,
    mode: ExplorerIconMode,
) {
    let y = row.y;
    let (x, right) = (row.x + 1, row.right().saturating_sub(1));
    let surface = match (current, focused) {
        (true, true) => GitSurface::Cursor,
        (true, false) => GitSurface::CursorInactive,
        _ => PANE,
    };
    if current {
        chrome::fill(&mut frame.grid, row, st(GitRole::Text, surface));
        chrome::selection_edge(&mut frame.grid, row.x, y..y + 1, focused, surface, mode);
        if focused {
            frame.cursor = Some((row.x, y, crossterm::cursor::SetCursorStyle::SteadyBar));
        }
    }
    // The time keeps its slot on the right; the reason chip sits before it while the title
    // keeps room, else it goes whole (never cut to `review requ`).
    let when = super::fresh::relative(&thread.updated.0, now);
    if !when.is_empty() {
        frame.grid.put_text_clipped(
            right.saturating_sub(chrome::width(&when)),
            y,
            &when,
            st(GitRole::Muted, surface),
            right,
        );
    }
    let time_at = right.saturating_sub(slot.max(chrome::width(&when)));
    let reason = reason_word(workspace, thread).to_lowercase();
    let hue = match thread.reason {
        NotifyReason::Mentioned | NotifyReason::ReviewRequested => GitChip::Modified,
        _ if team_mention(&thread.reason) => GitChip::Modified,
        _ => GitChip::Muted,
    };
    let chip_width = chrome::width(&reason) + 2;
    let number = match &thread.target {
        NotifyTarget::Item { number, .. } => {
            format!(" {}{number}", workspace.sigil(thread_is_pull(thread)))
        }
        _ => String::new(),
    };
    let (mark, role) = target_mark(workspace, thread, mode);
    let saved = if workspace.is_saved(thread) {
        if mode == ExplorerIconMode::Ascii {
            "* "
        } else {
            "★ "
        }
    } else {
        ""
    };
    let lead: Vec<Span> = vec![
        (
            if thread.unread { "● " } else { "  " }.to_owned(),
            st(GitRole::Focus, surface),
        ),
        (format!("{mark} "), st(role, surface)),
    ];
    let lead_width = chrome::spans_width(&lead);
    let title = super::prose::title(&thread.title, false);
    let fixed = lead_width + chrome::width(&number) + chrome::width(saved) + 2;
    let title_end = time_at.saturating_sub(2);
    let with_chip = title_end.saturating_sub(chip_width + 2);
    let chip = with_chip > x + fixed + 12;
    let end = if chip { with_chip } else { title_end };
    let room = usize::from(end.saturating_sub(x + fixed));
    let title_style = if thread.unread {
        bold(GitRole::Text, surface)
    } else {
        st(GitRole::Text, surface)
    };
    let mut spans = lead;
    spans.push((chrome::ellipsize(&title, room), title_style));
    spans.push((number, st(GitRole::Muted, surface)));
    if !saved.is_empty() {
        spans.push((
            format!(" {}", saved.trim_end()),
            st(GitRole::Modified, surface),
        ));
    }
    put(&mut frame.grid, x, y, &spans, end);
    if chip {
        let chip_at = title_end.saturating_sub(chip_width);
        chrome::chip(&mut frame.grid, chip_at, y, title_end, &reason, hue);
    }
}

fn thread_is_pull(thread: &Notification) -> bool {
    matches!(
        thread.target,
        NotifyTarget::Item {
            kind: ItemKind::PullRequest,
            ..
        }
    )
}

impl Workspace {
    /// What the Git tutor's Notifications lessons check (`src/git/screen/tutor`), read only:
    /// only unread threads are shown, and the sidebar has the keys.
    pub(super) fn tutor_notifications(&self) -> (bool, bool) {
        (self.notify.unread_only, self.notify.side)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::model::{ItemId, Timestamp};

    #[test]
    fn legacy_marks_persist_unverified_then_belong_only_to_the_first_login() {
        let root = tempfile::tempdir().unwrap();
        git2::Repository::init(root.path()).unwrap();
        let mut ws = Workspace::new(root.path().into());
        ws.host = "forge.test:8443".into();
        let host = HostKey::new(&ws.host);
        let alice = AccountNamespace::signed_in(host.clone(), "alice").unwrap();
        let bob = AccountNamespace::signed_in(host, "bob").unwrap();
        let existing = thread(NotifyReason::Mentioned);
        let mut added = existing.clone();
        added.id = ItemId::new("2");
        let mut bob_thread = existing.clone();
        bob_thread.id = ItemId::new("3");

        // Seed HEAD's exact spelling. An unverifiable session uses the same Git config.
        let mut config = local_config(&ws.root).unwrap();
        for key in [SAVED_KEY, DONE_KEY] {
            config.set_str(key, "forge.test:8443/1").unwrap();
        }
        ws.ensure_marks_with(LegacyAccess::Unclaimed);
        assert!(ws.is_saved(&existing) && ws.is_done(&existing));
        ws.edit_loaded_mark(SAVED_KEY, &added, true);
        ws.edit_loaded_mark(DONE_KEY, &added, true);
        assert!(
            load_marks(&ws.root)
                .saved
                .contains(&"forge.test:8443/2".to_owned())
        );
        assert!(
            load_marks(&ws.root)
                .done
                .contains(&"forge.test:8443/2".to_owned())
        );

        ws.account = Some(alice.clone());
        ws.ensure_marks_with(LegacyAccess::Owner);
        for thread in [&existing, &added] {
            assert!(ws.is_saved(thread) && ws.is_done(thread));
        }
        ws.edit_loaded_mark(SAVED_KEY, &added, false);
        ws.edit_loaded_mark(DONE_KEY, &added, false);
        ws.account = Some(bob);
        ws.ensure_marks_with(LegacyAccess::Blocked);
        assert!(!ws.is_saved(&existing) && !ws.is_done(&existing));
        ws.edit_loaded_mark(SAVED_KEY, &bob_thread, true);
        ws.edit_loaded_mark(DONE_KEY, &bob_thread, true);
        let bob_mark = ws.thread_mark(&bob_thread).unwrap();

        ws.account = Some(alice);
        ws.ensure_marks_with(LegacyAccess::Owner);
        assert!(ws.is_saved(&existing) && ws.is_done(&existing));
        assert!(!ws.is_saved(&added) && !ws.is_done(&added));
        assert!(!ws.is_saved(&bob_thread) && !ws.is_done(&bob_thread));
        let marks = load_marks(&ws.root);
        for list in [marks.saved, marks.done] {
            assert!(
                list.contains(&bob_mark),
                "an edit must keep other accounts' marks"
            );
        }
    }

    #[test]
    fn notification_errors_keep_account_confirmation_reasons() {
        let ws = Workspace::new(PathBuf::new());
        for reason in [
            crate::forge::ACCOUNT_CHANGED,
            crate::forge::UNVERIFIED_SIGN_IN,
        ] {
            assert_eq!(
                ws.notification_write_error(&Write::MarkAllRead, reason)
                    .as_deref(),
                Some(reason)
            );
        }
    }

    fn thread(reason: NotifyReason) -> Notification {
        Notification {
            id: ItemId::new("1"),
            title: "t".into(),
            reason,
            unread: true,
            repo: "o/r".into(),
            target: NotifyTarget::Item {
                kind: ItemKind::Issue,
                number: 1,
            },
            updated: Timestamp("2026-01-01T00:00:00Z".into()),
            comment: None,
        }
    }

    #[test]
    fn ctrl_l_opens_in_app_notifications_and_reports_browser_only_edges() {
        let directory = tempfile::tempdir().unwrap();
        let pr = || NotifyTarget::Item {
            kind: ItemKind::PullRequest,
            number: 7,
        };
        for (repo, target, expected_page, expected_item, expected_run, expected_edge) in [
            ("o/r", pr(), 2, Some((2, 7)), None, None),
            ("o/other", pr(), 2, Some((2, 7)), None, None),
            (
                "o/r",
                NotifyTarget::Run(ItemId::new("42")),
                3,
                None,
                Some(42),
                None,
            ),
            (
                "o/r",
                NotifyTarget::Release("https://notifications.example/o/r/releases/tag/v1".into()),
                PAGE,
                None,
                None,
                Some(SpatialDirection::Right),
            ),
        ] {
            let mut workspace = Workspace::new(directory.path().to_path_buf());
            workspace.host = "notifications.example".into();
            workspace.repo = "o/r".into();
            workspace.active = lanes::Lanes::dry();
            workspace.page = PAGE;
            let mut notification = thread(NotifyReason::Subscribed);
            notification.repo = repo.into();
            notification.target = target;
            let value = serde_json::json!({"Notifications": {"items": [notification]}});
            workspace.cache.insert(
                Key::Notifications,
                Ok(Reply {
                    text: value.to_string(),
                    value,
                }),
            );

            let key = "ctrl-l".parse().unwrap();
            assert!(workspace.notification_key(&key).unwrap().is_none());
            assert_eq!(workspace.page, expected_page);
            assert_eq!(workspace.repo, repo);
            assert_eq!(workspace.opened, expected_item);
            assert_eq!(workspace.run_id, expected_run);
            assert_eq!(workspace.focus_exit, expected_edge);
        }
    }

    #[test]
    fn rejected_notification_writes_restore_the_inbox() {
        use crate::forge::request::ForgeError;

        let mut workspace = Workspace::new(PathBuf::new());
        workspace.host = "notifications.example".into();
        workspace.repo = "o/r".into();
        workspace.active = lanes::Lanes::dry();
        let mut first = thread(NotifyReason::Mentioned);
        first.unread = false;
        let mut last = thread(NotifyReason::Subscribed);
        last.id = ItemId::new("2");
        let value = serde_json::json!({"Notifications": {"items": [&first, &last]}});
        workspace.cache.insert(
            Key::Notifications,
            Ok(Reply {
                text: value.to_string(),
                value,
            }),
        );

        let press = |workspace: &mut Workspace, ch| {
            workspace
                .notification_key(&KeyEvent::character(ch))
                .unwrap();
        };
        // An already-read row doesn't start a write or post a notice.
        press(&mut workspace, 'm');
        assert!(!workspace.writing());
        assert!(workspace.notice.is_empty());

        workspace.notify.selected = 1;
        for (ch, action) in [
            ('m', "Couldn't mark it read"),
            ('u', "Couldn't unsubscribe"),
        ] {
            press(&mut workspace, ch);
            assert!(workspace.start_reads());
            let shown = workspace.shown_threads();
            if ch == 'm' {
                assert!(!shown[1].unread);
            } else {
                assert_eq!(shown, [first.clone()]);
            }

            // Let the fake forge reject the running request through the normal reply path.
            let request = workspace.active.finish(&Key::Write).unwrap();
            let Op::Write(_, write, _) = &request.op else {
                panic!("expected a notification write");
            };
            let error = if ch == 'm' {
                ForgeError::NotAuthenticated {
                    host: workspace.host.clone(),
                    hint: "glab's saved API address and Dusty's API address don't match. Set api_base to https://api.example.test/tools/code/api/v4.".into(),
                }
            } else {
                ForgeError::Network("500".into())
            };
            let reply = ForgeReply::Written(write.clone(), Err(error));
            let scope = (
                workspace.host.clone(),
                workspace.repo.clone(),
                workspace.scope_epoch,
            );
            workspace.active.answer_for_test(request, scope, reply);
            assert!(workspace.poll());

            assert_eq!(workspace.shown_threads(), [first.clone(), last.clone()]);
            assert_eq!(workspace.selected_thread().unwrap().id, last.id);
            assert!(workspace.notice_error);
            let reason = if ch == 'm' {
                "Not signed in to notifications.example · @ sign in"
            } else {
                "notifications.example is having problems"
            };
            assert_eq!(workspace.notice, format!("{action}: {reason}"));
        }
    }

    #[test]
    fn rejected_mark_all_keeps_the_sign_in_reason_and_shortcut_at_100_and_80_columns() {
        let root = tempfile::tempdir().unwrap();
        git2::Repository::init(root.path()).unwrap();
        let mut app = GitScreen::open(root.path()).unwrap();
        app.page = PAGE;
        app.github = Workspace::new(root.path().into());
        app.github.host = "github.com".into();
        app.github.repo = "o/r".into();
        app.github.page = PAGE;
        app.github.active = lanes::Lanes::dry();
        app.github.notify.wanted = true;
        assert!(!app.github.sidebar_hidden());
        // Inject the queued write directly so the test doesn't depend on the global
        // account registry's state for github.com before the write is sent.
        let target = app.github.target();
        app.github.enqueue(
            Key::Write,
            Op::Write(target, Write::MarkAllRead, None),
            false,
        );
        assert!(app.github.start_reads());

        let request = app.github.active.finish(&Key::Write).unwrap();
        let error = crate::forge::request::ForgeError::NotAuthenticated {
            host: "github.com".into(),
            hint:
                "If you're signed in with gh, Dusty's API address must match the address gh uses."
                    .into(),
        };
        let scope = (
            app.github.host.clone(),
            app.github.repo.clone(),
            app.github.scope_epoch,
        );
        let reply = ForgeReply::Written(Write::MarkAllRead, Err(error));
        app.github.active.answer_for_test(request, scope, reply);
        assert!(app.github.poll());

        let shown = frame(&mut app, 100, 30);
        let status = shown.grid.row_text(27);
        assert!(
            status.contains("Couldn't mark all notifications read"),
            "{status}"
        );
        assert!(
            status.contains("Not signed in") && status.contains("@ sign in"),
            "{status}"
        );

        let shown = frame(&mut app, 80, 30);
        let status = shown.grid.row_text(27);
        assert!(
            status.contains("Mark all failed: not signed in · @ sign in"),
            "{status}"
        );
    }

    #[test]
    fn the_filters_read_reasons_as_github_does() {
        let team = thread(NotifyReason::Other("team_mention".into()));
        assert!(Filter::TeamMentioned.holds(&team) && Filter::Participating.holds(&team));
        assert!(!Filter::Mentioned.holds(&team));
        let ci = thread(NotifyReason::CiActivity);
        assert!(
            FILTERS.iter().all(|filter| !filter.holds(&ci)),
            "CI activity is no participation"
        );
        let review = thread(NotifyReason::ReviewRequested);
        assert!(Filter::ReviewRequested.holds(&review) && Filter::Participating.holds(&review));
        assert!(!Filter::Participating.holds(&thread(NotifyReason::Subscribed)));
    }

    #[test]
    fn a_check_suite_subject_names_its_workflow_outcome_and_branch() {
        let parse = |title: &str| suite_subject(title);
        assert_eq!(
            parse("CI workflow run failed for main branch"),
            Some(("CI".into(), Some("failure"), "main".into()))
        );
        assert_eq!(
            parse("Quality checks succeeded for fix/ci-regression"),
            Some((
                "Quality checks".into(),
                Some("success"),
                "fix/ci-regression".into()
            ))
        );
        assert_eq!(
            parse("Docs workflow run cancelled for feat/for-real branch"),
            Some(("Docs".into(), Some("cancelled"), "feat/for-real".into()))
        );
        assert_eq!(
            parse("CI workflow run completed for main branch"),
            Some(("CI".into(), None, "main".into()))
        );
        assert_eq!(
            parse("Unfailed for main"),
            None,
            "the verb is a word of its own"
        );
        assert_eq!(parse("Retry flaky uploads"), None);
    }

    #[test]
    fn a_check_suite_row_shows_its_newest_runs_outcome_else_its_subjects() {
        let mut workspace = Workspace::new(std::path::PathBuf::new());
        workspace.repo = "octo/orbit".into();
        let mut suite = thread(NotifyReason::CiActivity);
        suite.repo = "octo/orbit".into();
        suite.title = "CI workflow run failed for retry-uploads branch".into();
        suite.target = NotifyTarget::Run(ItemId::new(""));
        let mode = ExplorerIconMode::Ascii;
        // Not read yet: the subject's verb.
        assert_eq!(
            target_mark(&workspace, &suite, mode),
            runs::run_mark(
                &serde_json::json!({"status": "completed", "conclusion": "failure"}),
                mode
            )
        );
        // Read: the newest run of that workflow on that branch (a rerun now running), not an
        // older one, another branch's or another workflow's.
        let run =
            |id: u64, workflow: &str, branch: &str, status: &str, conclusion: &str, at: &str| {
                serde_json::json!({"databaseId": id, "workflowName": workflow, "headBranch": branch,
                "status": status, "conclusion": conclusion, "createdAt": at})
            };
        let rows = serde_json::json!([
            run(
                1,
                "CI",
                "retry-uploads",
                "completed",
                "failure",
                "2026-11-14T05:00:00Z"
            ),
            run(
                2,
                "CI",
                "retry-uploads",
                "in_progress",
                "",
                "2026-11-14T06:00:00Z"
            ),
            run(
                3,
                "CI",
                "main",
                "completed",
                "success",
                "2026-11-14T07:00:00Z"
            ),
            run(
                4,
                "Docs",
                "retry-uploads",
                "completed",
                "success",
                "2026-11-14T08:00:00Z"
            ),
        ]);
        workspace.cache.insert(
            Key::Runs(0),
            Ok(Reply {
                text: String::new(),
                value: rows,
            }),
        );
        assert_eq!(
            suite_run(&workspace, &suite).and_then(|run| run["databaseId"].as_u64()),
            Some(2)
        );
        assert_eq!(
            target_mark(&workspace, &suite, mode),
            runs::run_mark(&serde_json::json!({"status": "in_progress"}), mode)
        );
        // Another repository's suite is never matched against this one's runs.
        suite.repo = "octo/other".into();
        assert!(suite_run(&workspace, &suite).is_none());
    }
}
