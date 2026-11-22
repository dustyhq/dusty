//! Pull request review through the forge CLI (`gh` or `tea`): pick a pull request to check out
//! into its own worktree, list its review comments, and comment on the current line. Viewing,
//! diffing and submitting a GitHub review run `gh` in a terminal float (the `gh_pr_*` profiles).

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::app::picker::PickerPayload;
use crate::forge::ForgeJobs;
use crate::forge::ForgeReply;
use crate::forge::ReviewCommentTarget;
use crate::language::lsp::LspLocation;
use crate::language::lsp::LspPosition;
use crate::language::lsp::LspRange;
use crate::language::lsp::file_uri;
use crate::ui::PromptKind;
use crate::ui::render::ReviewCommentTitle;
use crate::workspace::PickerAction;
use crate::workspace::PickerItem;
use crate::workspace::PickerItemId;
use crate::workspace::PickerItemKind;

const LOADING_REVIEW_COMMENT: &str = "loading pull request for review comment…";

#[derive(Clone)]
struct ReviewCommentDraft {
    target: ReviewCommentTarget,
    body: String,
}

#[derive(Default)]
enum ReviewComment {
    #[default]
    Idle,
    Loading {
        discard_notice: Option<String>,
    },
    // The prompt owns the editable body until it is closed or submitted.
    Editing(ReviewCommentTarget),
    Saved {
        draft: ReviewCommentDraft,
        // Kept only if the resolved draft has yet to open its first prompt.
        discard_notice: Option<String>,
    },
    Sending(ReviewCommentDraft),
}

enum ReviewCommentRequest {
    Resolve {
        path: PathBuf,
        line: usize,
        text: String,
    },
    Send(ReviewCommentDraft),
}

#[derive(Default)]
pub(in crate::app) struct PullRequests {
    jobs: ForgeJobs,
    source: Option<crate::app::workspace::WorkspaceVersion>,
    request_id: u64,
    review_comment: ReviewComment,
    #[cfg(test)]
    review_requests: Option<Vec<ReviewCommentRequest>>,
    #[cfg(test)]
    next_review_request_error: Option<String>,
}

impl PullRequests {
    pub(in crate::app) fn prepare(&mut self, source: crate::app::workspace::WorkspaceVersion) {
        if self.source.as_ref() != Some(&source) {
            self.jobs.cancel();
            self.request_id += 1;
            self.source = Some(source);
            self.review_comment = ReviewComment::Idle;
        }
    }

    fn request_review_comment(
        &mut self,
        root: PathBuf,
        request: ReviewCommentRequest,
    ) -> Result<(), String> {
        #[cfg(test)]
        if let Some(error) = self.next_review_request_error.take() {
            return Err(error);
        }
        #[cfg(test)]
        if let Some(requests) = &mut self.review_requests {
            requests.push(request);
            return Ok(());
        }
        match request {
            ReviewCommentRequest::Resolve { path, line, text } => {
                self.jobs
                    .comment_target(root, self.request_id, path, line, text)
            }
            ReviewCommentRequest::Send(ReviewCommentDraft { target, body }) => {
                self.jobs.comment(root, self.request_id, target, body)
            }
        }
    }

    fn take_review_comment_draft(&mut self, body: String) -> Option<ReviewCommentDraft> {
        match std::mem::take(&mut self.review_comment) {
            ReviewComment::Editing(target) => Some(ReviewCommentDraft { target, body }),
            other => {
                self.review_comment = other;
                None
            }
        }
    }
}

fn review_comment_path<'a>(root: &Path, target: &'a ReviewCommentTarget) -> &'a Path {
    target.path.strip_prefix(root).unwrap_or(&target.path)
}

impl App {
    fn pull_request_request_id(&mut self) -> u64 {
        let source = self.workspace.version();
        self.pull_requests.prepare(source);
        self.pull_requests.request_id
    }

    fn pull_request_is_current(&self, root: &std::path::Path, request_id: u64) -> bool {
        root == self.state.workspace_root
            && request_id == self.pull_requests.request_id
            && self.pull_requests.source.as_ref() == Some(&self.workspace.version())
    }

    pub(in crate::app) fn list_pull_requests(&mut self) {
        let request_id = self.pull_request_request_id();
        match self
            .pull_requests
            .jobs
            .list(self.state.workspace_root.clone(), request_id)
        {
            Ok(()) => self.notify_message("loading pull requests…".into(), false),
            Err(error) => self.show_error(error),
        }
    }

    pub(in crate::app) fn list_review_comments(&mut self) {
        self.pull_requests.prepare(self.workspace.version());
        match self.pull_requests.jobs.comments(
            self.state.workspace_root.clone(),
            self.pull_requests.request_id,
        ) {
            Ok(()) => self.notify_message("loading review comments…".into(), false),
            Err(error) => self.show_error(error),
        }
    }

    pub(super) fn begin_review_comment_at_cursor(&mut self) {
        self.pull_requests.prepare(self.workspace.version());
        if matches!(
            self.pull_requests.review_comment,
            ReviewComment::Loading { .. } | ReviewComment::Sending(_)
        ) {
            self.notify_message("a review comment request is still running".into(), false);
            return;
        }
        let (Some(view), Some(buffer)) = (self.state.active_view(), self.state.active_buffer_id())
        else {
            self.show_error("no file to comment on");
            return;
        };
        let Some(path) = self.state.buffers[buffer].path.clone() else {
            self.show_error("the current buffer has no file");
            return;
        };
        let line = self.state.buffers[buffer]
            .text
            .byte_to_line(self.state.views[view].cursor)
            .unwrap_or(0)
            + 1;
        if self
            .input
            .prompt()
            .is_some_and(|prompt| prompt.kind == PromptKind::ReviewComment)
        {
            self.close_prompt();
        }
        if matches!(
            &self.pull_requests.review_comment,
            ReviewComment::Saved { draft, .. } if draft.target.path == path && draft.target.line == line
        ) && let ReviewComment::Saved {
            draft,
            discard_notice,
        } = std::mem::take(&mut self.pull_requests.review_comment)
        {
            self.open_review_comment(draft, discard_notice);
            return;
        }
        match self.pull_requests.request_review_comment(
            self.state.workspace_root.clone(),
            ReviewCommentRequest::Resolve {
                path,
                line,
                text: self.state.buffers[buffer].text.to_string(),
            },
        ) {
            Ok(()) => {
                let previous = std::mem::take(&mut self.pull_requests.review_comment);
                let discard_notice = if let ReviewComment::Saved { draft, .. } = previous
                    && !draft.body.is_empty()
                {
                    let path = review_comment_path(&self.state.workspace_root, &draft.target);
                    let notice = format!(
                        "Discarded the review draft for {}:{}.",
                        path.display(),
                        draft.target.line
                    );
                    self.notify_warning(notice.clone());
                    Some(notice)
                } else {
                    self.notify_message(LOADING_REVIEW_COMMENT.into(), false);
                    None
                };
                self.pull_requests.review_comment = ReviewComment::Loading { discard_notice };
            }
            Err(error) => self.show_error(error),
        }
    }

    fn open_review_comment(&mut self, draft: ReviewCommentDraft, discard_notice: Option<String>) {
        let title = ReviewCommentTitle {
            pull_request: draft.target.head.number,
            path: review_comment_path(&self.state.workspace_root, &draft.target)
                .to_string_lossy()
                .into_owned(),
            line: draft.target.line,
        };
        if let Some(notice) = discard_notice.as_deref()
            && self.notices.current() == Some((false, notice))
        {
            self.clear_notice();
        }
        self.open_prompt_with_review_title(
            PromptKind::ReviewComment,
            draft.body,
            Some(title),
            discard_notice,
        );
        self.pull_requests.review_comment = ReviewComment::Editing(draft.target);
    }

    pub(in crate::app) fn save_review_comment(&mut self) {
        let Some(prompt) = self.input.prompt() else {
            return;
        };
        if prompt.kind == PromptKind::ReviewComment
            && self.pull_requests.source.as_ref() == Some(&self.workspace.version())
            && let Some(draft) = self
                .pull_requests
                .take_review_comment_draft(prompt.input.clone())
        {
            self.pull_requests.review_comment = ReviewComment::Saved {
                draft,
                discard_notice: None,
            };
        }
    }

    pub(in crate::app) fn submit_review_comment(&mut self, body: String) {
        // Take the draft first so closing the prompt cannot save it a second time.
        let draft = self.pull_requests.take_review_comment_draft(body);
        self.close_prompt();
        let Some(draft) = draft else {
            return;
        };
        if self.pull_requests.source.as_ref() != Some(&self.workspace.version()) {
            self.show_error("the comment belongs to a previous workspace");
            return;
        }
        if draft.body.trim().is_empty() {
            return;
        }
        // Retain the original draft until the worker confirms it was sent.
        let request = ReviewCommentRequest::Send(draft.clone());
        match self
            .pull_requests
            .request_review_comment(self.state.workspace_root.clone(), request)
        {
            Ok(()) => {
                self.pull_requests.review_comment = ReviewComment::Sending(draft);
                self.notify_message("sending comment…".into(), false);
            }
            Err(error) => {
                self.open_review_comment(draft, None);
                self.show_error(error);
            }
        }
    }

    pub(in crate::app) fn checkout_pull_request(
        &mut self,
        root: PathBuf,
        request_id: u64,
        number: u64,
        local: Option<PathBuf>,
    ) {
        if !self.pull_request_is_current(&root, request_id) {
            return;
        }
        if let Some(path) = local {
            self.switch_workspace_root(path);
            return;
        }
        if let Err(failure) = self.workspace.execution_allowed() {
            self.show_workspace_failure(failure);
            return;
        }
        match self.pull_requests.jobs.checkout(
            root,
            request_id,
            number,
            self.workspace.edit_scope().clone(),
        ) {
            Ok(()) => self.notify_message(format!("checking out #{number}…"), false),
            Err(error) => self.show_error(error),
        }
    }

    pub(in crate::app) fn process_pull_request_events(&mut self) {
        self.pull_requests.prepare(self.workspace.version());
        while let Some(event) = self.pull_requests.jobs.try_reply() {
            if !self.pull_request_is_current(&event.root, event.request_id) {
                continue;
            }
            let root = event.root;
            let request_id = event.request_id;
            // A target reply answers the loading notice, not a draft-discard notice.
            if !matches!(&event.reply, ForgeReply::CommentTarget(_))
                || self.notices.current() == Some((false, LOADING_REVIEW_COMMENT))
            {
                self.clear_notice();
            }
            match event.reply {
                ForgeReply::List(Err(error))
                | ForgeReply::CheckedOut(Err(error))
                | ForgeReply::Comments(Err(error)) => self.show_error(error),
                ForgeReply::CommentTarget(Err(error)) => {
                    self.pull_requests.review_comment = ReviewComment::Idle;
                    self.show_error(error);
                }
                ForgeReply::CommentTarget(Ok(target)) => {
                    let ReviewComment::Loading { discard_notice } =
                        &mut self.pull_requests.review_comment
                    else {
                        continue;
                    };
                    let discard_notice = discard_notice.take();
                    let draft = ReviewCommentDraft {
                        target,
                        body: String::new(),
                    };
                    if self.input.prompt().is_none() {
                        self.open_review_comment(draft, discard_notice);
                    } else {
                        self.pull_requests.review_comment = ReviewComment::Saved {
                            draft,
                            discard_notice,
                        };
                        self.notify_message(
                            "review comment ready; open it again when finished with this prompt"
                                .into(),
                            false,
                        );
                    }
                }
                ForgeReply::Commented(Err(error)) => {
                    if let ReviewComment::Sending(draft) =
                        std::mem::take(&mut self.pull_requests.review_comment)
                    {
                        if self.input.prompt().is_none() {
                            self.open_review_comment(draft, None);
                        } else {
                            self.pull_requests.review_comment = ReviewComment::Saved {
                                draft,
                                discard_notice: None,
                            };
                        }
                    }
                    self.show_error(error);
                }
                ForgeReply::Offline { error, local } if local.is_empty() => {
                    self.notify_warning(format!("pull requests unavailable: {error}"));
                }
                ForgeReply::Offline { error, local } => {
                    let items = local
                        .into_iter()
                        .map(|(number, path)| {
                            (
                                PickerItem {
                                    badge: "local".into(),
                                    kind: PickerItemKind::Entry,
                                    id: PickerItemId(format!("pr:{number}")),
                                    label: format!("#{number}"),
                                    detail: path.display().to_string(),
                                    source: String::new(),
                                    action: PickerAction::None,
                                },
                                PickerPayload::PullRequest {
                                    root: root.clone(),
                                    request_id,
                                    number,
                                    local: Some(path),
                                },
                            )
                        })
                        .collect();
                    self.install_picker("Pull Requests (local)".to_owned(), items);
                    self.search
                        .set_picker_footer("Enter Open the worktree · Esc Esc Close".to_owned());
                    self.sync_picker_ui();
                    self.notify_warning(format!("{error} · showing local checkouts"));
                }
                ForgeReply::List(Ok(list)) if list.is_empty() => {
                    self.notify_message("no open pull requests".into(), false);
                }
                ForgeReply::List(Ok(list)) => {
                    let items = list
                        .into_iter()
                        .map(|pr| {
                            (
                                PickerItem {
                                    badge: if pr.draft {
                                        "draft".into()
                                    } else {
                                        String::new()
                                    },
                                    kind: PickerItemKind::Entry,
                                    id: PickerItemId(format!("pr:{}", pr.number)),
                                    label: format!("#{} {}", pr.number, pr.title),
                                    detail: format!("@{} · {}", pr.author, pr.branch),
                                    source: String::new(),
                                    action: PickerAction::None,
                                },
                                PickerPayload::PullRequest {
                                    root: root.clone(),
                                    request_id,
                                    number: pr.number,
                                    local: None,
                                },
                            )
                        })
                        .collect();
                    self.install_picker("Pull Requests".to_owned(), items);
                    self.search.set_picker_footer(
                        "Enter Check out in a worktree · Esc Esc Close".to_owned(),
                    );
                    self.sync_picker_ui();
                }
                ForgeReply::CheckedOut(Ok(path)) => self.switch_workspace_root(path),
                ForgeReply::Comments(Ok(comments)) if comments.is_empty() => {
                    self.notify_message("no review comments".into(), false);
                }
                ForgeReply::Comments(Ok(comments)) => {
                    let root = self.state.workspace_root.clone();
                    let items = comments
                        .into_iter()
                        .enumerate()
                        .map(|(index, comment)| {
                            let line = comment.line.unwrap_or(1).max(1) as usize;
                            let shown = comment.path.strip_prefix(&root).unwrap_or(&comment.path);
                            let position = LspPosition {
                                line: line - 1,
                                character: 0,
                            };
                            (
                                PickerItem {
                                    badge: if comment.line.is_none() {
                                        "outdated".into()
                                    } else {
                                        String::new()
                                    },
                                    kind: PickerItemKind::Entry,
                                    id: PickerItemId(format!("review:{index}")),
                                    label: comment
                                        .body
                                        .lines()
                                        .next()
                                        .unwrap_or_default()
                                        .to_owned(),
                                    detail: format!(
                                        "{}:{line} · @{}",
                                        shown.display(),
                                        comment.author
                                    ),
                                    source: String::new(),
                                    action: PickerAction::None,
                                },
                                PickerPayload::LspLocation(
                                    LspLocation {
                                        uri: file_uri(&comment.path),
                                        range: LspRange {
                                            start: position.clone(),
                                            end: position,
                                        },
                                    },
                                    crate::editor::text::PositionEncoding::Utf16,
                                ),
                            )
                        })
                        .collect();
                    self.install_picker("Review Comments".to_owned(), items);
                    self.search
                        .set_picker_footer("Enter Go to line · Esc Esc Close".to_owned());
                    self.sync_picker_ui();
                }
                ForgeReply::Fetched(..) | ForgeReply::Written(..) => {}
                ForgeReply::Commented(Ok(())) => {
                    self.pull_requests.review_comment = ReviewComment::Idle;
                    self.notify_message("comment sent".into(), false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::git::pull_requests::*;

    pub(in crate::app) fn app() -> App {
        App::new(
            crate::app::startup::StartupTarget::Dashboard {
                root: tempfile::tempdir().unwrap().keep(),
            },
            crate::config::load_bundled_config(),
            false,
            tempfile::tempdir().unwrap().keep(),
            false,
            false,
            std::sync::Arc::new(crate::clock::SystemClock),
        )
        .unwrap()
    }

    #[test]
    fn checkout_requires_trust_and_respects_safe_mode() {
        let mut app = app();
        let request_id = app.pull_request_request_id();
        app.checkout_pull_request(app.state.workspace_root.clone(), request_id, 1, None);
        assert!(app.pull_requests.jobs.try_reply().is_none());
        app.workspace.fixture_safe_mode(true);
        let request_id = app.pull_request_request_id();
        app.checkout_pull_request(app.state.workspace_root.clone(), request_id, 1, None);
        assert!(app.pull_requests.jobs.try_reply().is_none());
    }

    #[test]
    fn outdated_workspace_replies_do_not_open_pickers_or_switch_roots() {
        let mut app = app();
        app.pull_requests.prepare(app.workspace.version());
        app.pull_requests.jobs.reply_for_test(
            app.state.workspace_root.clone(),
            app.pull_requests.request_id,
            ForgeReply::List(Ok(vec![crate::forge::PullRequest {
                number: 1,
                title: "old".into(),
                author: "author".into(),
                branch: "old".into(),
                draft: false,
            }])),
        );
        app.pull_requests.jobs.reply_for_test(
            app.state.workspace_root.clone(),
            app.pull_requests.request_id,
            ForgeReply::CheckedOut(Ok(app.state.workspace_root.join("old"))),
        );
        app.workspace.fixture_safe_mode(true);
        app.process_pull_request_events();
        assert!(app.ui.picker.is_none());
        assert!(!app.workspace.root_operation_pending());
    }

    fn review_app() -> App {
        let mut app = app();
        app.set_dashboard_visibility(false);
        let path = app.state.workspace_root.join("file.txt");
        app.state
            .open_text(
                crate::editor::text::TextBuffer::from_text("first\nsecond\n"),
                Some(path),
            )
            .unwrap();
        app.pull_requests.review_requests = Some(Vec::new());
        app
    }

    fn invoke_review_comment(app: &mut App) {
        app.process_effect(crate::app::commands::AppEffect::PullRequestComment);
    }

    fn review_requests(app: &App) -> &[ReviewCommentRequest] {
        app.pull_requests.review_requests.as_deref().unwrap()
    }

    fn forge_reply(app: &mut App, reply: ForgeReply) {
        app.pull_requests.jobs.reply_for_test(
            app.state.workspace_root.clone(),
            app.pull_requests.request_id,
            reply,
        );
        app.process_pull_request_events();
    }

    fn resolve_review_comment(app: &mut App, expected_line: usize) {
        let Some(ReviewCommentRequest::Resolve { path, line, text }) = review_requests(app).last()
        else {
            panic!("expected a request to resolve the cursor anchor");
        };
        assert_eq!(path, &app.state.workspace_root.join("file.txt"));
        assert_eq!(*line, expected_line);
        let target = ReviewCommentTarget {
            path: path.clone(),
            line: *line,
            text: text.clone(),
            head: crate::forge::PullRequestHead {
                number: 7,
                commit: "abc".into(),
            },
        };
        forge_reply(app, ForgeReply::CommentTarget(Ok(target)));
    }

    fn assert_review_prompt(app: &App, line: usize, body: &str) {
        let prompt = app.input.prompt().expect("review comment prompt");
        assert_eq!(prompt.kind, PromptKind::ReviewComment);
        let title = prompt.review_comment_title.as_ref().expect("review target");
        assert_eq!(title.pull_request, 7);
        assert_eq!(title.path, "file.txt");
        assert_eq!(title.line, line);
        assert_eq!(prompt.input, body);
    }

    fn move_to_second_line(app: &mut App) {
        let view = app.state.active_view().unwrap();
        app.state.views[view].cursor = crate::editor::text::ByteOffset("first\n".len());
    }

    #[test]
    fn review_comment_on_another_line_starts_fresh_and_sends_to_that_line() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("Text for line one");
        app.close_prompt();

        const DISCARD_NOTICE: &str = "Discarded the review draft for file.txt:1.";
        move_to_second_line(&mut app);
        invoke_review_comment(&mut app);
        assert_eq!(app.notices.current(), Some((false, DISCARD_NOTICE)));
        app.clear_notice(); // The toast can expire before the target comes back.
        resolve_review_comment(&mut app, 2);
        assert_review_prompt(&app, 2, "");
        app.prompt_insert("Text for line two");

        let (width, height) = (100, 30);
        app.handle_event(crate::ui::UiEvent::Resize(width, height));
        app.reconcile_editor_windows();
        let config = app.configuration.settings();
        let frame = crate::ui::render(
            &app.state,
            &app.ui,
            config,
            app.configuration.dashboard(),
            width,
            height,
        );
        let prompt = app.ui.prompt.as_ref().expect("projected review prompt");
        let geometry = crate::ui::prompt_geometry_for_state(width, height, config, prompt);
        assert!(app.ui.notification.is_none());
        assert!(
            frame
                .grid
                .row_text(geometry.top)
                .contains("Review comment on #7 · file.txt:2")
        );
        let hint_y = geometry.top + 1;
        assert!(frame.grid.row_text(hint_y).contains(DISCARD_NOTICE));
        assert_eq!(
            frame
                .grid
                .get(geometry.left + 2, hint_y)
                .expect("discard hint")
                .style,
            crate::ui::Style::Muted
        );
        assert!(
            frame
                .grid
                .row_text(hint_y + 1)
                .contains("Text for line two")
        );

        app.submit_prompt();
        assert!(app.input.prompt().is_none());
        let [
            ReviewCommentRequest::Resolve { line: 1, .. },
            ReviewCommentRequest::Resolve { line: 2, .. },
            ReviewCommentRequest::Send(draft),
        ] = review_requests(&app)
        else {
            panic!("expected a fresh resolve and send after moving the cursor");
        };
        assert_eq!(draft.target.path, app.state.workspace_root.join("file.txt"));
        assert_eq!(draft.target.line, 2);
        assert_eq!(draft.body, "Text for line two");
    }

    #[test]
    fn busy_forge_preserves_the_draft_on_its_original_line() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("Text for line one");
        app.close_prompt();

        move_to_second_line(&mut app);
        // Simulate the forge refusing a new job while the comments list is loading.
        let busy = "a forge request is already running";
        app.pull_requests.next_review_request_error = Some(busy.into());
        let notices_before = app.notices.history().count();
        invoke_review_comment(&mut app);
        assert!(app.input.prompt().is_none());
        assert_eq!(app.notices.current(), Some((true, busy)));
        assert_eq!(app.notices.history().count(), notices_before + 1);

        let view = app.state.active_view().unwrap();
        app.state.views[view].cursor = crate::editor::text::ByteOffset(0);
        invoke_review_comment(&mut app);
        assert_review_prompt(&app, 1, "Text for line one");
        assert_eq!(review_requests(&app).len(), 1);
    }

    #[test]
    fn dismissed_review_comment_reopens_on_its_line() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("my draft");
        app.close_prompt();

        invoke_review_comment(&mut app);
        assert_review_prompt(&app, 1, "my draft");
        assert_eq!(review_requests(&app).len(), 1);

        // Replacing a prompt must preserve the same anchored draft as dismissing it.
        app.prompt_insert(" with more text");
        app.open_prompt(PromptKind::Command, String::new());
        app.close_prompt();
        invoke_review_comment(&mut app);
        assert_review_prompt(&app, 1, "my draft with more text");
        assert_eq!(review_requests(&app).len(), 1);
    }

    #[test]
    fn sending_review_comments_cannot_reopen_the_prompt() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("sending draft");
        app.submit_prompt();

        invoke_review_comment(&mut app);
        assert!(app.input.prompt().is_none());
        assert_eq!(review_requests(&app).len(), 2);
        forge_reply(&mut app, ForgeReply::Commented(Ok(())));

        invoke_review_comment(&mut app);
        assert_eq!(review_requests(&app).len(), 3);
        resolve_review_comment(&mut app, 1);
        assert_review_prompt(&app, 1, "");
    }

    #[test]
    fn loading_review_comments_keeps_the_hint_for_the_review_prompt_only() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("Text for line one");
        app.close_prompt();

        move_to_second_line(&mut app);
        invoke_review_comment(&mut app);
        app.open_prompt(PromptKind::Command, "workspace.replace query=draft".into());
        let command_help = app.input.prompt().unwrap().help.clone();
        resolve_review_comment(&mut app, 2);
        let prompt = app.input.prompt().unwrap();
        assert_eq!(prompt.kind, PromptKind::Command);
        assert_eq!(prompt.input, "workspace.replace query=draft");
        assert_eq!(prompt.help, command_help);

        app.close_prompt();
        invoke_review_comment(&mut app);
        assert_review_prompt(&app, 2, "");
        assert_eq!(
            app.input.prompt().unwrap().help.as_deref(),
            Some("Discarded the review draft for file.txt:1.")
        );

        app.close_prompt();
        invoke_review_comment(&mut app);
        assert_review_prompt(&app, 2, "");
        assert!(app.input.prompt().unwrap().help.is_none());
        assert_eq!(review_requests(&app).len(), 2);
    }

    #[test]
    fn failed_review_comments_restore_the_body_and_target() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("my draft");
        app.submit_prompt();
        move_to_second_line(&mut app);

        forge_reply(&mut app, ForgeReply::Commented(Err("offline".into())));
        assert_review_prompt(&app, 1, "my draft");
        app.submit_prompt();
        let [
            ReviewCommentRequest::Resolve { .. },
            ReviewCommentRequest::Send(first),
            ReviewCommentRequest::Send(retry),
        ] = review_requests(&app)
        else {
            panic!("expected the original send and a retry on the same anchor");
        };
        assert_eq!(first.target.path, app.state.workspace_root.join("file.txt"));
        assert_eq!(first.target.line, 1);
        assert_eq!(first.body, "my draft");
        assert_eq!(retry.target.path, first.target.path);
        assert_eq!(retry.target.line, first.target.line);
        assert_eq!(retry.target.head.commit, first.target.head.commit);
        assert_eq!(retry.body, first.body);
    }

    #[test]
    fn changing_workspace_scope_invalidates_review_comment_draft() {
        let mut app = review_app();
        invoke_review_comment(&mut app);
        resolve_review_comment(&mut app, 1);
        app.prompt_insert("previous workspace draft");
        app.close_prompt();

        app.workspace.fixture_safe_mode(true);
        invoke_review_comment(&mut app);
        assert_eq!(review_requests(&app).len(), 2);
        resolve_review_comment(&mut app, 1);
        assert_review_prompt(&app, 1, "");
    }

    #[test]
    fn outdated_forge_replies_and_picker_payloads_cannot_change_the_workspace() {
        let mut app = app();
        let root = app.state.workspace_root.clone();
        let request_id = app.pull_request_request_id();
        let list = vec![crate::forge::PullRequest {
            number: 7,
            title: "from A".into(),
            author: "ana".into(),
            branch: "fix".into(),
            draft: false,
        }];
        app.pull_requests.jobs.reply_for_test(
            PathBuf::from("/old-root"),
            request_id,
            ForgeReply::List(Ok(list.clone())),
        );
        app.pull_requests.jobs.reply_for_test(
            root.clone(),
            request_id + 1,
            ForgeReply::List(Ok(list.clone())),
        );
        app.process_pull_request_events();
        assert!(app.search.picker().is_none());
        app.pull_requests
            .jobs
            .reply_for_test(root.clone(), request_id, ForgeReply::List(Ok(list)));
        app.process_pull_request_events();
        assert!(
            matches!(app.search.selected_picker_payload(), Some(PickerPayload::PullRequest { root: source, request_id: version, number: 7, local: None }) if source == &root && *version == request_id)
        );
        let other = tempfile::tempdir().unwrap();
        app.checkout_pull_request(
            root.clone(),
            request_id + 1,
            7,
            Some(other.path().to_path_buf()),
        );
        app.checkout_pull_request(
            PathBuf::from("/old-root"),
            request_id,
            7,
            Some(other.path().to_path_buf()),
        );
        assert_eq!(app.state.workspace_root, root);

        // Switching between existing roots does not change root_revision.
        let first = app.workspace.active_root().unwrap();
        let second = app
            .workspace
            .fixture_add_root(
                "other".into(),
                crate::files::RootIdentity::capture(other.path()).unwrap(),
            )
            .unwrap();
        app.workspace.set_active_root(first).unwrap();
        let request_id = app.pull_request_request_id();
        let revision = app.workspace.root_revision();
        app.workspace.set_active_root(second).unwrap();
        app.workspace.set_active_root(first).unwrap();
        assert_eq!(app.workspace.root_revision(), revision);
        assert!(!app.pull_request_is_current(&root, request_id));
        app.close_picker();
        app.pull_requests.jobs.reply_for_test(
            root.clone(),
            request_id,
            ForgeReply::CheckedOut(Ok(other.path().to_path_buf())),
        );
        app.process_pull_request_events();
        app.checkout_pull_request(
            root.clone(),
            request_id,
            7,
            Some(other.path().to_path_buf()),
        );
        assert_eq!(app.state.workspace_root, root);
        assert!(app.pull_request_request_id() > request_id);
    }
}
