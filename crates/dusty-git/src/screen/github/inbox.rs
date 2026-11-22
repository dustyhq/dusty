//! Host-scoped search lists. Detail caches remain single-repository: switching a
//! remote row invalidates them, never the local Git root or a confirmed write.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::screen) struct Target {
    pub host: String,
    pub repo: String,
    pub page: usize,
    pub number: u64,
}
impl Target {
    pub(super) fn from_url(
        text: &str,
        host: &str,
        web_alias: Option<&HostKey>,
        page: usize,
        number: u64,
    ) -> Result<Self, String> {
        let url = reqwest::Url::parse(text).map_err(|_| "Search returned an invalid item URL")?;
        let same_host = HostKey::from_url(&url).is_some_and(|found| {
            Some(&found) == web_alias || HostKey::parse_authority(host).as_ref() == Some(&found)
        });
        let parts = url.path().trim_matches('/').split('/').collect::<Vec<_>>();
        if url.scheme() != "https"
            || !same_host
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || parts.len() != 4
            || parts[2] != if page == 1 { "issues" } else { "pull" }
            || parts[3].parse::<u64>().ok() != Some(number)
            || number == 0
        {
            return Err("Search returned an item outside the requested host/type".into());
        }
        let repo = format!("{}/{}", parts[0], parts[1]).to_ascii_lowercase();
        if !crate::forge::pull_request::valid_repository(&repo) {
            return Err("Search returned an invalid repository".into());
        }
        Ok(Self {
            host: host.into(),
            repo,
            page,
            number,
        })
    }
    pub(super) fn from_row(
        value: &Value,
        host: &str,
        web_alias: Option<&HostKey>,
        page: usize,
    ) -> Result<Self, String> {
        let target = Self::from_url(
            string(value, "url"),
            host,
            web_alias,
            page,
            number(value, "number"),
        )?;
        if !string(&value["repository"], "nameWithOwner").eq_ignore_ascii_case(&target.repo) {
            return Err("Search item URL and repository disagree".into());
        }
        Ok(target)
    }
    pub(super) fn repository(&self) -> ForgeTarget {
        ForgeTarget::repository(&self.host, &self.repo)
    }

    fn identity(&self) -> String {
        format!("{}/{}/{}/{}", self.host, self.repo, self.page, self.number)
    }
}

#[derive(Default)]
struct Settings {
    queries: [String; 2],
    states: [String; 2],
    views: [Option<views::SavedView>; 2],
    selected: [usize; 2],
}
#[derive(Default)]
pub(super) struct Inbox {
    pub enabled: bool,
    home: String,
    local: Option<Settings>,
    remote: Option<Settings>,
    pub origin_page: Option<usize>,
    /// The repository list behind a link opened outside the Inbox.
    local_link_origin: Option<(usize, String)>,
}

pub(super) fn validate_rows(
    value: &mut Value,
    host: &str,
    web_alias: Option<&HostKey>,
    page: usize,
    state: &str,
) -> Result<(), String> {
    let rows = value
        .as_array_mut()
        .ok_or("Search did not return an item array")?;
    if rows.len() > LIST_CEILING {
        return Err("Search exceeded the bounded result limit".into());
    }
    let mut identities = BTreeSet::new();
    for row in rows {
        let target = Target::from_row(row, host, web_alias, page)?;
        if !identities.insert(target.identity()) {
            return Err("Search returned duplicate item identities; reload the section".into());
        }
        // Search exports closed for both closed-unmerged and merged PRs.
        // Only the explicit --merged section supplies the missing merge fact;
        // never infer it for ordinary closed/all searches or for issues.
        if let Some(row_state) = row.get_mut("state") {
            let mut normalized = row_state.as_str().unwrap_or("").to_ascii_uppercase();
            if page == 2 && state == "merged" {
                if !matches!(normalized.as_str(), "CLOSED" | "MERGED") {
                    return Err("Merged search returned a non-closed pull request".into());
                }
                normalized = "MERGED".into();
            }
            *row_state = Value::String(normalized);
        } else if page == 2 && state == "merged" {
            return Err("Merged search omitted pull request state".into());
        }
    }
    Ok(())
}

impl Workspace {
    fn take_list_settings(&mut self) -> Settings {
        Settings {
            queries: std::mem::take(&mut self.queries),
            states: std::mem::take(&mut self.states),
            views: std::mem::take(&mut self.active_views),
            selected: std::mem::take(&mut self.selected),
        }
    }
    fn put_list_settings(&mut self, settings: Settings) {
        self.queries = settings.queries;
        self.states = settings.states;
        self.active_views = settings.views;
        self.selected = settings.selected;
    }
    pub(super) fn toggle_inbox(&mut self) -> Result<(), String> {
        if self.writing() {
            return Err("Wait for the current action to finish".into());
        }
        if self.repo.is_empty() || self.host.is_empty() {
            return Err("Load a GitHub repository before opening i inbox".into());
        }
        if !self.inbox.enabled {
            self.view_scope()?;
            if self.inbox.local_link_origin.is_some() {
                self.return_to_list();
            }
        }
        self.remember_list_selection();
        if self.inbox.enabled {
            self.inbox.remote = Some(self.take_list_settings());
            self.inbox.enabled = false;
            let home = self.inbox.home.clone();
            self.switch_detail_repository(home);
            let local = self.inbox.local.take().unwrap_or_default();
            self.put_list_settings(local);
        } else {
            self.inbox.home = self.repo.clone();
            self.inbox.local = Some(self.take_list_settings());
            let settings = self.inbox.remote.take().unwrap_or_else(|| Settings {
                queries: ["assignee:@me".into(), "review-requested:@me".into()],
                states: ["open".into(), "open".into()],
                ..Settings::default()
            });
            self.put_list_settings(settings);
            self.inbox.enabled = true;
        }
        self.inbox.origin_page = None;
        self.opened = None;
        self.focus = 0;
        self.reset_remote_detail();
        if matches!(self.page, 1 | 2) {
            self.restore_list_selection();
        }
        self.notice.clear();
        self.notice_error = false;
        self.ensure(false);
        Ok(())
    }
    pub(super) fn reset_remote_detail(&mut self) {
        self.opened = None;
        self.reader_row = None;
        self.selected_commit = None;
        self.detail_rows.clear();
        self.detail_cursor = 0;
        self.detail_scroll = 0;
        self.horizontal = 0;
        self.detail_find = find::DetailFind::default();
        self.reading_position = lines::ReadingPosition::default();
        self.selected_files.clear();
        self.collapsed.clear();
        self.reveal_file = self.page == 2 && self.pr_tab == 3;
        self.pending_pr = false;
        self.compose = super::create::Compose::default();
        self.ready_form = None;
        self.reset_run_write_scope();
    }
    /// Keep the known spelling for drafts and storage; a search row lowercases the repo.
    pub(super) fn known_repository_spelling(&self, repo: &str) -> Option<&str> {
        let local = if self.inbox.enabled {
            self.inbox.home.as_str()
        } else {
            self.list_repository()
        };
        [&*self.repo, local]
            .into_iter()
            .find(|known| known.eq_ignore_ascii_case(repo))
    }

    pub(super) fn switch_detail_repository(&mut self, repo: String) {
        if self.repo.eq_ignore_ascii_case(&repo) {
            return;
        }
        self.scope_epoch = self
            .scope_epoch
            .checked_add(1)
            .expect("GitHub scope generation exhausted");
        self.repo = self
            .known_repository_spelling(&repo)
            .map_or(repo, str::to_owned);
        self.again
            .retain(|key, _| matches!(key, Key::List(..) | Key::Inbox(..)));
        // Repository metadata belongs to home; inbox creation/develop are disabled.
        // Lists have independent identities; only number-keyed detail state is evicted.
        self.cache
            .retain(|key, _| matches!(key, Key::Repository | Key::List(..) | Key::Inbox(..)));
        self.queue.retain(|request| {
            matches!(
                request.key,
                Key::Write | Key::WorktreeWrite(_) | Key::Inbox(..)
            )
        });
        self.reset_remote_detail();
    }
    pub(super) fn sync_inbox_repository(&mut self) {
        if !self.inbox.enabled || !matches!(self.page, 1 | 2) || self.opened.is_some() {
            return;
        }
        let target = self
            .items()
            .get(self.selected[self.page - 1])
            .and_then(|row| self.inbox_row_target(row, self.page).ok());
        if let Some(target) = target {
            self.switch_detail_repository(target.repo);
        }
    }
    pub(super) fn remember_remote_link(&mut self, item_page: usize) {
        if self.inbox.local_link_origin.is_none() && self.inbox.origin_page.is_none() {
            self.remember_list_selection();
        }
        if self.inbox.enabled {
            self.inbox.origin_page.get_or_insert(self.page);
        } else {
            // A foreign notification opens its item over the checkout's corresponding list.
            let page = if self.page == notifications::PAGE {
                item_page
            } else {
                self.page
            };
            self.inbox
                .local_link_origin
                .get_or_insert_with(|| (page, self.repo.clone()));
        }
    }
    pub(super) fn list_repository(&self) -> &str {
        self.inbox
            .local_link_origin
            .as_ref()
            .map_or(&self.repo, |(_, repo)| repo)
    }
    /// Account reset discards the inbox's return paths, so restore the checkout first.
    pub(super) fn restore_checkout_for_account_reset(&mut self) {
        let home = if self.inbox.enabled && !self.inbox.home.is_empty() {
            Some(std::mem::take(&mut self.inbox.home))
        } else {
            self.inbox.local_link_origin.take().map(|(_, repo)| repo)
        };
        if let Some(repo) = home {
            self.repo = repo;
            self.focus = 0;
        }
    }

    pub(super) fn restore_local_list_repository(&mut self) -> Option<usize> {
        let (page, repo) = self.inbox.local_link_origin.take()?;
        self.switch_detail_repository(repo);
        Some(page)
    }
    pub(super) fn return_to_list(&mut self) {
        let origin = self
            .restore_local_list_repository()
            .or_else(|| self.inbox.origin_page.take());
        if let Some(page) = origin {
            self.page = page;
        }
        if origin.is_some() || self.inbox.enabled {
            self.reset_remote_detail();
            // Only a link can leave the current list. Moving focus out of its own
            // reader keeps the list cursor where it already is.
            if origin.is_some() && matches!(self.page, 1 | 2) {
                self.restore_list_selection();
            }
        } else {
            // Moving focus to the same item's list row keeps its reading position.
            // Clear the old action rows only if leaving a different item.
            let list_number = self.list_row().map(|row| number(row, "number"));
            if self.opened_number() != list_number {
                self.detail_rows.clear();
            }
            self.opened = None;
            self.reader_row = None;
        }
        self.ensure(false);
    }
    pub(super) fn inbox_row_target(&self, row: &Value, page: usize) -> Result<Target, String> {
        Target::from_row(row, &self.host, self.advertised_web_host().as_ref(), page)
    }

    pub(super) fn row_identity(&self, item: &Value) -> String {
        if self.inbox.enabled {
            return self
                .inbox_row_target(item, self.page)
                .map(|target| target.identity())
                .unwrap_or_default();
        }
        format!(
            "{}/{}/{}/{}",
            self.host,
            self.list_repository(),
            self.page,
            number(item, "number")
        )
    }
    pub(super) fn cache_scope(&self, key: &Key) -> CacheScope {
        match key {
            Key::List(..) => CacheScope::List(ForgeTarget::repository(
                &self.host,
                &self.list_repository().to_ascii_lowercase(),
            )),
            Key::Inbox(..) => CacheScope::Inbox(self.host.clone()),
            _ => CacheScope::Detail((
                self.host.clone(),
                self.repo.to_ascii_lowercase(),
                self.scope_epoch,
            )),
        }
    }
    /// Whether a reply started under `scope` still belongs on screen.
    pub(super) fn accept_scope(&self, key: &Key, scope: &super::lanes::Scope) -> bool {
        match key {
            Key::Write | Key::WorktreeWrite(_) => true,
            Key::Inbox(host, ..) => host == &self.host && scope.2 == self.scope_epoch,
            _ => {
                scope.0 == self.host
                    && scope.1.eq_ignore_ascii_case(&self.repo)
                    && scope.2 == self.scope_epoch
            }
        }
    }
    /// The current item's repository is not the local checkout.
    pub(super) fn foreign_item(&self) -> bool {
        let checkout = if self.inbox.enabled {
            &self.inbox.home
        } else {
            self.list_repository()
        };
        !self.repo.eq_ignore_ascii_case(checkout)
    }
    pub(super) fn check_inbox_action(&self, action: &Action) -> Result<(), String> {
        if self.inbox.enabled
            && matches!(
                action,
                Action::CreateIssue
                    | Action::CreatePullRequest
                    | Action::Develop(_)
                    | Action::Run(_)
            )
        {
            return Err("Press i to go back to this repository first".into());
        }
        if self.foreign_item()
            && matches!(
                action,
                Action::Checkout(_) | Action::Worktree(_) | Action::Develop(_)
            )
        {
            return Err("Open the item's local repository first.".into());
        }
        if !action.item_scoped()
            || !matches!(self.page, 1 | 2)
                && matches!(action, Action::Browser(..) | Action::Web(..))
        {
            return Ok(());
        }
        let expected = match action {
            Action::Browser(page, id)
            | Action::Web(page, id, _)
            | Action::Edit(page, id)
            | Action::Comment(page, id)
            | Action::State(page, id, _)
            | Action::Metadata(page, id, _) => (*page, *id),
            Action::Develop(id) => (1, *id),
            Action::ConfirmSuggestion(snapshot) => (2, snapshot.number),
            Action::Review(id)
            | Action::Merge(id, _)
            | Action::Checkout(id)
            | Action::Worktree(id)
            | Action::Files(id)
            | Action::Viewed(id, _)
            | Action::CommitSuggestion(id, _)
            | Action::Draft(id, _)
            | Action::Inline { number: id, .. }
            | Action::InlineRange { number: id, .. }
            | Action::Reply { number: id, .. } => (2, *id),
            _ => (self.page, self.number()),
        };
        if self.selected_item_target().is_some_and(|item| {
            item.host == self.host
                && item.repo.eq_ignore_ascii_case(&self.repo)
                && (item.page, item.number) == expected
        }) {
            Ok(())
        } else {
            Err("Selected item changed; reopen the original action (draft preserved)".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn account_recheck_from_a_foreign_inbox_item_restores_the_checkout() {
        let mut ws = Workspace::new(PathBuf::new());
        ws.host = "github.com".into();
        ws.repo = "home/checkout".into();
        ws.page = 1;
        ws.toggle_inbox().unwrap();
        ws.switch_detail_repository("foreign/item".into());
        ws.opened = Some((1, 7));
        ws.accounts.open = true;
        assert!(matches!(
            ws.accounts_key(&KeyEvent::character('r')),
            Some(Ok(None))
        ));
        assert_eq!(
            ws.target(),
            ForgeTarget::repository("github.com", "home/checkout")
        );
        assert!(!ws.inbox.enabled);
        assert!(ws.opened.is_none());
    }

    fn row(repo: &str, page: usize) -> Value {
        json!({"number":7,"repository":{"nameWithOwner":repo},
            "url":format!("https://github.com/{repo}/{}/7", if page == 1 { "issues" } else { "pull" }),
            "title":format!("{repo} seven"),"body":format!("Source bytes for {repo}"),"state":"open"})
    }
    fn workspace(root: PathBuf) -> Workspace {
        let mut workspace = Workspace::new(root);
        workspace.host = "github.com".into();
        workspace.repo = "local/root".into();
        workspace.page = 2;
        workspace.states = ["open".into(), "open".into()];
        workspace.inbox.enabled = true;
        workspace.inbox.home = "local/root".into();
        workspace.cache.insert(
            Key::Repository,
            Ok(Reply {
                value: Value::Null,
                text: String::new(),
            }),
        );
        let mut rows = json!([row("alpha/one", 2), row("beta/two", 2)]);
        validate_rows(&mut rows, "github.com", None, 2, "open").unwrap();
        workspace.cache.insert(
            workspace.list_key(),
            Ok(Reply {
                text: rows.to_string(),
                value: rows,
            }),
        );
        workspace.ensure(false);
        workspace
    }
    fn form_action(form: Form) -> Action {
        let FormAction::Github(action) = form.action else {
            panic!("GitHub form expected")
        };
        action
    }

    #[test]
    fn same_number_rows_have_scoped_reads_forms_drafts_and_writes() {
        let owner = tempfile::tempdir().unwrap();
        git2::Repository::init(owner.path()).unwrap();
        std::fs::write(owner.path().join("source.txt"), "never change me\n").unwrap();
        let mut workspace = workspace(owner.path().into());
        assert_eq!(workspace.repo, "alpha/one");
        let first = form_action(workspace.activate(Action::Comment(2, 7)).unwrap().unwrap());
        workspace.save_draft(&first, vec!["alpha draft 界".into()]);
        workspace.cache.insert(
            Key::Item(2, 7),
            Ok(Reply {
                value: json!({"number":7,"body":"alpha only"}),
                text: String::new(),
            }),
        );
        workspace.selected_files.insert(7, "alpha.rs".into());
        workspace.selected[1] = 1;
        workspace.ensure(false);
        assert_eq!(workspace.repo, "beta/two");
        assert!(workspace.item().is_none());
        assert!(workspace.selected_files.is_empty());
        assert_eq!(
            string(workspace.list_item().unwrap(), "body"),
            "Source bytes for beta/two"
        );
        let second_form = workspace.activate(Action::Comment(2, 7)).unwrap().unwrap();
        assert!(second_form.fields[0].input.input.is_empty());
        assert!(second_form.title.contains("github.com/beta/two · PR #7"));
        let second = form_action(second_form);
        assert_ne!(workspace.draft_key(&first), workspace.draft_key(&second));
        assert!(
            workspace
                .submit(first.clone(), vec!["still alpha".into()])
                .unwrap_err()
                .contains("source changed")
        );
        assert!(!workspace.writing());
        workspace
            .submit(second, vec!["beta comment".into()])
            .unwrap();
        let write = workspace
            .queue
            .iter()
            .find(|request| request.key == Key::Write)
            .unwrap();
        let Op::Write(target, comment, _) = &write.op else {
            panic!("a comment is a write")
        };
        assert_eq!(target, &ForgeTarget::repository("github.com", "beta/two"));
        assert!(
            matches!(comment, Write::Comment { number: 7, body, .. } if body == "beta comment")
        );
        assert!(workspace.check_inbox_action(&Action::Checkout(7)).is_err());
        assert!(workspace.check_inbox_action(&Action::Worktree(7)).is_err());
        workspace.queue.clear();
        workspace.selected[1] = 0;
        workspace.ensure(false);
        let restored = workspace.activate(Action::Comment(2, 7)).unwrap().unwrap();
        assert_eq!(restored.fields[0].input.input, "still alpha");
        assert_eq!(workspace.root, owner.path());
        assert_eq!(
            std::fs::read_to_string(owner.path().join("source.txt")).unwrap(),
            "never change me\n"
        );
        assert!(
            git2::Repository::open(owner.path())
                .unwrap()
                .head()
                .is_err()
        );
    }

    #[test]
    fn stale_same_number_reply_and_repository_round_trip_do_not_replace_current_source() {
        let mut workspace = workspace(PathBuf::new());
        let first_epoch = workspace.scope_epoch;
        let scope: super::super::lanes::Scope =
            ("github.com".into(), "alpha/one".into(), first_epoch);
        workspace.selected[1] = 1;
        workspace.ensure(false);
        assert!(!workspace.accept_scope(&Key::Item(2, 7), &scope));
        workspace.selected[1] = 0;
        workspace.ensure(false);
        assert!(
            !workspace.accept_scope(&Key::Item(2, 7), &scope),
            "A → B → A is a new detail generation"
        );
        workspace.cache.insert(
            Key::Item(2, 7),
            Ok(Reply {
                value: json!({"body":"current alpha"}),
                text: String::new(),
            }),
        );
        workspace.queue.clear();
        workspace.page = 0; // Consume only the injected reply; never launch a CLI in this state test.
        workspace.active.answer_for_test(
            Request::placeholder(Key::Item(2, 7)),
            scope,
            ForgeReply::Fetched(
                Read::PullRequest(7),
                None,
                Ok(crate::forge::provider::Fetched::payload(
                    crate::forge::request::Payload::Text("stale alpha".into()),
                )),
                None,
            ),
        );
        assert!(workspace.poll());
        assert_eq!(
            string(workspace.value(&Key::Item(2, 7)).unwrap(), "body"),
            "current alpha"
        );
        assert!(workspace.active.idle());
    }

    #[test]
    fn a_link_uses_its_repository_for_preview_and_returns_to_the_original_list() {
        let mut workspace = workspace(PathBuf::new());
        workspace.follow(open::parse("https://github.com/Beta/Two/pull/7").unwrap());
        assert_eq!(
            string(workspace.list_item().expect("Inbox fallback"), "title"),
            "beta/two seven"
        );

        workspace.toggle_inbox().unwrap();
        workspace.page(1);
        let origin = workspace.target();
        let mut alpha = row("local/root", 1);
        alpha["number"] = 16.into();
        let mut beta = row("beta/two", 1);
        beta["number"] = 16.into();
        let rows = json!([row("local/root", 1), alpha]);
        workspace.cache.insert(
            workspace.list_key(),
            Ok(Reply {
                text: rows.to_string(),
                value: rows,
            }),
        );
        workspace.selected[0] = 1;
        let target = Target {
            host: "github.com".into(),
            repo: "beta/two".into(),
            page: 1,
            number: number(&beta, "number"),
        };
        workspace.activate(Action::OpenRemote(target)).unwrap();
        assert!(
            workspace.list_item().is_none(),
            "same number, wrong repository"
        );
        assert!(matches!(workspace.list_read(LIST_BATCH), Op::Read(target, _) if target == origin));
        workspace
            .event(&KeyEvent::plain(KeyCode::Escape), None)
            .unwrap();
        assert_eq!(workspace.selected[0], 1);
        let form = workspace
            .event(&KeyEvent::character('x'), None)
            .unwrap()
            .expect("close the highlighted issue");
        let action = form_action(form);
        workspace.selected[0] = 0;
        assert!(
            workspace
                .submit(action.clone(), vec!["Completed".into(), String::new()])
                .unwrap_err()
                .contains("source changed")
        );
        assert!(!workspace.writing());
        workspace.selected[0] = 1;
        workspace
            .submit(action, vec!["Completed".into(), String::new()])
            .unwrap();
        assert!(workspace.queue.iter().any(|request| matches!(
            &request.op,
            Op::Write(target, Write::SetState { number: 16, open: false, .. }, _) if *target == origin
        )));
    }

    fn linked_issue_workspace(root: PathBuf) -> Workspace {
        let mut ws = workspace(root);
        ws.fresh = fresh::Fresh::with(None);
        ws.active = lanes::Lanes::dry();
        ws.toggle_inbox().unwrap();
        ws.page(1);
        let rows = json!([row("local/root", 1)]);
        ws.cache.insert(
            ws.list_key(),
            Ok(Reply {
                text: rows.to_string(),
                value: rows,
            }),
        );
        ws.follow(open::parse("https://github.com/beta/two/issues/7").unwrap());
        let detail = row("beta/two", 1);
        ws.cache.insert(
            Key::Item(1, 7),
            Ok(Reply {
                text: detail.to_string(),
                value: detail,
            }),
        );
        ws
    }

    fn close_issue(ws: &mut Workspace, comment: &str) -> Request {
        let form = ws
            .event(&KeyEvent::character('x'), None)
            .unwrap()
            .expect("close the linked issue");
        ws.submit(form_action(form), vec!["Completed".into(), comment.into()])
            .unwrap();
        let index = ws
            .queue
            .iter()
            .position(|request| request.key == Key::Write)
            .expect("queued issue write");
        ws.queue.remove(index).unwrap()
    }

    #[test]
    fn closing_comment_receipt_keeps_its_follow_up_on_the_linked_issue() {
        let mut ws = linked_issue_workspace(PathBuf::new());
        let target = ws.target();
        let scope = (ws.host.clone(), ws.repo.clone(), ws.scope_epoch);
        let request = close_issue(&mut ws, "Fixed in the linked change");
        let Op::Write(sent_to, write @ Write::Comment { number: 7, .. }, _) = &request.op else {
            panic!("closing comment is sent first");
        };
        assert_eq!(*sent_to, target);
        let receipt = ForgeReply::Written(write.clone(), Ok(Default::default()));
        ws.active.answer_for_test(request, scope, receipt);

        ws.event(&KeyEvent::plain(KeyCode::Escape), None).unwrap();
        assert_eq!(ws.repo, "local/root");
        assert!(ws.poll());
        assert!(ws.queue.iter().chain(ws.active.requests()).any(|request| {
            matches!(
                &request.op,
                Op::Write(sent_to, Write::SetState { number: 7, open: false, .. }, _)
                    if *sent_to == target
            )
        }));
        assert_eq!(string(ws.list_item().unwrap(), "state"), "open");
    }

    #[test]
    fn off_screen_close_and_reopen_receipts_name_the_linked_repository() {
        for (key, open, notice) in [
            ('x', false, "Closed beta/two#7"),
            ('X', true, "Reopened beta/two#7"),
        ] {
            let mut ws = linked_issue_workspace(PathBuf::new());
            if open {
                ws.cache
                    .get_mut(&Key::Item(1, 7))
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .value["state"] = "CLOSED".into();
            }
            let scope = (ws.host.clone(), ws.repo.clone(), ws.scope_epoch);
            let request = if open {
                let form = ws.event(&KeyEvent::character(key), None).unwrap().unwrap();
                ws.submit(form_action(form), vec![]).unwrap();
                let index = ws
                    .queue
                    .iter()
                    .position(|request| request.key == Key::Write)
                    .unwrap();
                ws.queue.remove(index).unwrap()
            } else {
                close_issue(&mut ws, "")
            };
            let Op::Write(
                target,
                write @ Write::SetState {
                    number: 7,
                    open: sent_open,
                    ..
                },
                _,
            ) = &request.op
            else {
                panic!("expected a state change");
            };
            assert_eq!(*target, ws.target());
            assert_eq!(*sent_open, open);
            let reply = ForgeReply::Written(write.clone(), Ok(Default::default()));
            ws.active.answer_for_test(request, scope, reply);
            ws.event(&KeyEvent::plain(KeyCode::Escape), None).unwrap();
            assert_eq!(ws.repo, "local/root");
            ws.poll();
            assert_eq!(ws.notice, notice);
            assert_eq!(string(ws.list_item().unwrap(), "state"), "open");
        }
    }

    #[test]
    fn leaving_a_link_drops_its_detail_rollback_and_deferred_refresh() {
        let root = tempfile::tempdir().unwrap();
        let mut ws = linked_issue_workspace(root.path().into());
        ws.fresh = fresh::Fresh::with(Some(std::sync::Arc::new(crate::forge::cache::Cache::at(
            root.path().join("forge-cache"),
        ))));
        let key = Key::Item(1, 7);
        let request = close_issue(&mut ws, "");
        let Op::Write(target, write @ Write::SetState { number: 7, .. }, _) = request.op else {
            panic!("plain close");
        };
        assert_eq!(string(ws.value(&key).unwrap(), "state"), "CLOSED");

        let scope = (ws.host.clone(), ws.repo.clone(), ws.scope_epoch);
        let op = ws.read(Read::Issue(7));
        ws.active.answer_for_test(
            Request {
                key: key.clone(),
                op,
            },
            scope,
            ForgeReply::Fetched(
                Read::Issue(7),
                None,
                Ok(crate::forge::provider::Fetched::payload(
                    crate::forge::request::Payload::Text(String::new()),
                )),
                None,
            ),
        );
        ws.enqueue(key.clone(), ws.read(Read::Issue(7)), true);
        assert!(ws.again.contains_key(&key));

        ws.event(&KeyEvent::plain(KeyCode::Escape), None).unwrap();
        assert_eq!(ws.repo, "local/root");
        ws.written(&write, Err("Network error".into()), &target);
        assert!(
            ws.value(&key).is_none(),
            "the linked issue's detail was evicted"
        );
        assert_eq!(string(ws.list_item().unwrap(), "title"), "local/root seven");
        // A later completion for the local reader must not restart the linked read.
        assert!(ws.active.finish(&key).is_some());
        ws.queue.clear();
        ws.ask_again(&key);
        assert!(!ws.pending(&key));
    }

    #[test]
    fn inbox_accepts_the_advertised_host_without_using_it_as_a_target() {
        let host = "git.internal.example:8443";
        let alias = HostKey::new("forge.example");
        let target = Target::from_url(
            "https://forge.example/o/r/issues/7",
            host,
            Some(&alias),
            1,
            7,
        )
        .unwrap();
        assert_eq!(target.repository(), ForgeTarget::repository(host, "o/r"));
        assert!(
            Target::from_url(
                "https://forge.example:8443/o/r/issues/7",
                host,
                Some(&alias),
                1,
                7
            )
            .is_err()
        );
    }

    #[test]
    fn inbox_links_must_match_the_requested_port() {
        let host = "git.example.test:8443";
        let target = Target::from_url(
            "https://git.example.test:8443/o/r/issues/7",
            host,
            None,
            1,
            7,
        )
        .unwrap();
        assert_eq!(target.host, host);
        assert!(
            Target::from_url(
                "https://git.example.test:8444/o/r/issues/7",
                host,
                None,
                1,
                7
            )
            .is_err()
        );
        assert!(
            Target::from_url("https://git.example.test/o/r/issues/7", host, None, 1, 7).is_err()
        );
    }

    #[test]
    fn section_and_link_return_restore_repository_qualified_cursor_not_number() {
        let mut workspace = workspace(PathBuf::new());
        workspace.selected[1] = 1;
        workspace.ensure(false);
        workspace.remember_list_selection();
        let origin = workspace.list_key();
        let target = Target::from_url(
            "https://github.com/alpha/one/issues/7",
            "github.com",
            None,
            1,
            7,
        )
        .unwrap();
        workspace.activate(Action::OpenRemote(target)).unwrap();
        assert_eq!((workspace.page, workspace.repo.as_str()), (1, "alpha/one"));
        workspace.return_to_list();
        assert_eq!(workspace.page, 2);
        assert_eq!(workspace.list_key(), origin);
        assert_eq!(workspace.selected[1], 1);
        assert_eq!(workspace.repo, "beta/two");
        workspace
            .apply_view("author:@me".into(), "closed".into(), None)
            .unwrap();
        workspace
            .apply_view(String::new(), "open".into(), None)
            .unwrap();
        assert_eq!(workspace.repo, "beta/two");
        workspace.value(&origin).unwrap();
        let reply = workspace.cache.get_mut(&origin).unwrap().as_mut().unwrap();
        reply.value.as_array_mut().unwrap().reverse();
        workspace.restore_list_selection();
        workspace.ensure(false);
        assert_eq!(workspace.selected[1], 0);
        assert_eq!(workspace.repo, "beta/two");
    }

    #[test]
    fn saved_sections_are_host_and_kind_scoped_and_preserve_local_views() {
        let owner = tempfile::tempdir().unwrap();
        git2::Repository::init(owner.path()).unwrap();
        let mut workspace = workspace(owner.path().into());
        let inbox_scope = workspace.view_scope().unwrap();
        workspace
            .save_view(
                &inbox_scope,
                &[
                    "Reviews".into(),
                    "review-requested:@me".into(),
                    "open".into(),
                ],
            )
            .unwrap();
        workspace.inbox.enabled = false;
        workspace.repo = "local/root".into();
        let local_scope = workspace.view_scope().unwrap();
        workspace
            .save_view(
                &local_scope,
                &["Reviews".into(), "label:local".into(), "all".into()],
            )
            .unwrap();
        workspace.inbox.enabled = true;
        workspace.host = "git.example.test".into();
        let other_host = workspace.view_scope().unwrap();
        workspace
            .save_view(
                &other_host,
                &["Reviews".into(), "author:@me".into(), "closed".into()],
            )
            .unwrap();
        let persisted = views::load(owner.path()).unwrap();
        let encoded = serde_json::to_value(persisted).unwrap();
        assert_eq!(encoded.as_array().unwrap().len(), 3);
        assert!(
            encoded
                .as_array()
                .unwrap()
                .iter()
                .any(|view| view["repo"] == "*" && view["host"] == "github.com")
        );
        assert!(
            encoded
                .as_array()
                .unwrap()
                .iter()
                .any(|view| view["repo"] == "local/root" && view["query"] == "label:local")
        );
        let mut reopened = workspace;
        reopened.host = "github.com".into();
        reopened.choose_view("Saved: Reviews").unwrap();
        assert_eq!(reopened.queries[1], "review-requested:@me");
        assert!(
            reopened
                .save_view(
                    &other_host,
                    &["wrong host".into(), String::new(), "open".into()]
                )
                .is_err()
        );
    }

    #[test]
    fn merged_section_context_does_not_relabel_ordinary_closed_prs_or_issues() {
        let mut rows = json!([row("alpha/one", 2)]);
        rows[0]["state"] = "closed".into();
        for (state, expected) in [
            ("merged", "MERGED"),
            ("closed", "CLOSED"),
            ("all", "CLOSED"),
        ] {
            let mut reply = rows.clone();
            validate_rows(&mut reply, "github.com", None, 2, state).unwrap();
            assert_eq!(reply[0]["state"], expected);
        }
        let mut issue = json!([row("alpha/one", 1)]);
        issue[0]["state"] = "closed".into();
        validate_rows(&mut issue, "github.com", None, 1, "closed").unwrap();
        assert_eq!(issue[0]["state"], "CLOSED");
        let mut wrong_state = json!([row("alpha/one", 2)]);
        assert!(validate_rows(&mut wrong_state, "github.com", None, 2, "merged").is_err());
        let mut workspace = workspace(PathBuf::new());
        workspace.states[1] = "merged".into();
        let Op::Read(_, Read::Search { filter, .. }) = workspace.list_read(100) else {
            panic!("inbox reads search")
        };
        assert_eq!(filter.state, StateFilter::Merged);
    }

    #[test]
    fn native_screen_refuses_stale_remote_checkout_and_worktree_controls() {
        use crate::screen::GitScreen;
        use crate::ui::{KeyInputKind, UiEvent};
        let owner = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(owner.path()).unwrap();
        std::fs::write(owner.path().join("tracked.txt"), "committed\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Inbox Test", "inbox@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        std::fs::write(owner.path().join("tracked.txt"), "staged\n").unwrap();
        index.add_path(std::path::Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(owner.path().join("tracked.txt"), "dirty working copy\n").unwrap();
        let mut screen = GitScreen::open(owner.path()).unwrap();
        let root = screen.root().to_path_buf();
        let head = repo.head().unwrap().target().unwrap();
        let head_file = std::fs::read(repo.path().join("HEAD")).unwrap();
        let index_file = std::fs::read(repo.path().join("index")).unwrap();
        let working = std::fs::read(owner.path().join("tracked.txt")).unwrap();
        screen.page = 2;
        screen.github = workspace(owner.path().into());
        screen.github.cache.insert(Key::Item(2, 7), Ok(Reply {
            value: json!({"number":7,"title":"remote seven","state":"OPEN","body":"remote body","files":[]}),
            text: String::new(),
        }));
        let rows = lines::detail_lines(&screen.github, 100);
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.action, Some(Action::Checkout(_) | Action::Worktree(_))))
        );
        for action in [Action::Checkout(7), Action::Worktree(7)] {
            // Model an already-rendered control arriving after a remote retarget.
            // The native Enter path must reject it even though normal rendering hides it.
            let mut rows = lines::detail_lines(&screen.github, 100);
            rows[0].action = Some(action);
            screen.github.detail_rows = rows;
            screen.github.detail_cursor = 0;
            screen.github.focus = 2;
            screen.github.queue.clear();
            screen.event(UiEvent::Key {
                key: KeyEvent::plain(KeyCode::Enter),
                kind: KeyInputKind::Press,
            });
            assert!(
                screen
                    .notice
                    .contains("Open the item's local repository first")
            );
            assert!(screen.form.is_none());
            assert!(!screen.github.writing());
            assert!(screen.github.queue.is_empty() && screen.github.active.idle());
            assert_eq!(screen.root(), root.as_path());
            assert_eq!(repo.head().unwrap().target(), Some(head));
            assert_eq!(std::fs::read(repo.path().join("HEAD")).unwrap(), head_file);
            assert_eq!(
                std::fs::read(repo.path().join("index")).unwrap(),
                index_file
            );
            assert_eq!(
                std::fs::read(owner.path().join("tracked.txt")).unwrap(),
                working
            );
            assert_eq!(repo.worktrees().unwrap().len(), 0);
        }
    }

    #[test]
    fn bounded_search_uses_only_supported_fields_and_rejects_wrong_host_or_identity() {
        let workspace = workspace(PathBuf::new());
        let Op::Read(_, Read::Search { kind, page, .. }) = workspace.list_read(200) else {
            panic!("inbox reads search")
        };
        assert_eq!((kind, page.per_page), (ItemKind::PullRequest, 200));
        let Op::Read(target, _) = workspace.list_read(200) else {
            panic!("inbox list is a read")
        };
        assert_eq!(
            target,
            ForgeTarget::host("github.com"),
            "search is host-wide"
        );
        let mut invalid = json!([row("alpha/one", 2)]);
        invalid[0]["url"] = "https://elsewhere.test/alpha/one/pull/7".into();
        assert!(validate_rows(&mut invalid, "github.com", None, 2, "open").is_err());
        let mut duplicate = json!([row("alpha/one", 2), row("alpha/one", 2)]);
        assert!(validate_rows(&mut duplicate, "github.com", None, 2, "open").is_err());
        let mut disagreement = json!([row("alpha/one", 2)]);
        disagreement[0]["repository"]["nameWithOwner"] = "beta/two".into();
        assert!(validate_rows(&mut disagreement, "github.com", None, 2, "open").is_err());
    }
}
