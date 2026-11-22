//! Pull, push, undo and redo, and history rewriting on the Git tab:
//! each opens a sheet that shows what will move and the command, then runs as a write job.
use super::*;
use crate::command::{CHECKOUT_TIMEOUT, GitOutput, supervised_git_output};
use crate::history::{self, Edit, PullMode, Run};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

/// A network write may take minutes; a local rebase far less.
const NETWORK: Duration = Duration::from_secs(300);
const LOCAL: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub(super) struct Checkout {
    run: Run,
    target: OsString,
    undo: Option<history::Undo>,
    timeout: Duration,
}

impl Checkout {
    pub(super) fn branch(target: OsString, track: bool) -> Self {
        let mut args = vec!["switch".into()];
        if track {
            args.push("--track".into());
        }
        args.extend(["--".into(), target.clone()]);
        Self {
            run: Run::new(crate::command::protect_checkout(args)),
            target,
            undo: None,
            timeout: CHECKOUT_TIMEOUT,
        }
    }

    #[cfg(all(test, unix))]
    pub(super) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(super) fn detached(id: String) -> Self {
        let target = OsString::from(id);
        let args = crate::command::protect_checkout([
            "checkout".into(),
            "--detach".into(),
            target.clone(),
            "--".into(),
        ]);
        Self {
            run: Run::new(args),
            target,
            undo: None,
            timeout: CHECKOUT_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub(super) struct OverwriteIgnored {
    checkout: Checkout,
    paths: Vec<PathBuf>,
}

impl OverwriteIgnored {
    pub(super) fn warning(&self) -> String {
        let paths = crate::command::display_paths(self.paths.iter().take(3).map(PathBuf::as_path));
        let target = self.checkout.target.to_string_lossy();
        if self.can_replace() {
            format!(
                "Switching to {target} would replace ignored files:\n{paths}\nA copy will be kept in Stashes."
            )
        } else {
            format!(
                "Switching to {target} would replace {} ignored files.\nMove these files before switching.\n{paths}, …",
                self.paths.len()
            )
        }
    }

    pub(super) fn can_replace(&self) -> bool {
        self.paths.len() <= 3
    }
}

pub(super) enum CheckoutOutcome {
    Finished(String),
    Collision(Box<OverwriteIgnored>),
    StashRecovery(String),
}

pub(super) type CheckoutReply = Receiver<Result<CheckoutOutcome, String>>;

enum CheckoutAttempt {
    Applied(String),
    Refused(GitOutput),
}

fn run_checkout(root: &Path, checkout: &Checkout) -> Result<CheckoutAttempt, String> {
    let check = checkout.run.check.as_ref();
    if let Some(check) = check {
        check.verify(root)?;
    }
    let result = supervised_git_output(
        root,
        &checkout.run.args,
        &checkout.run.env,
        checkout.timeout,
    );
    if let Ok(output) = &result
        && output.success()
    {
        return Ok(CheckoutAttempt::Applied(checkout_success(checkout, output)));
    }
    if let Some(undo) = &checkout.undo
        && check.is_some_and(|check| check.undo_landed(root))
    {
        // HEAD and its reflog marker have already moved; don't retry the switch
        // or offer recovery for an ignored-file collision.
        let done = if undo.redo { "Redone." } else { "Undone." };
        let warning = match result {
            Ok(output) => match output_message(&output).lines().next() {
                Some(detail) => format!("The post-checkout hook failed: {}", detail.trim()),
                None => "The post-checkout hook failed.".into(),
            },
            Err(error) => error,
        };
        return Ok(CheckoutAttempt::Applied(format!("{done} {warning}")));
    }
    result.map(CheckoutAttempt::Refused)
}

fn checkout_success(checkout: &Checkout, output: &GitOutput) -> String {
    if let Some(undo) = &checkout.undo {
        let (_, to) = undo.head();
        let verb = if undo.redo { "Redid" } else { "Undid" };
        format!("{verb} {} · HEAD at {to}", undo.entry)
    } else {
        output_message(output)
    }
}

fn output_message(output: &GitOutput) -> String {
    let stream = if output.stderr.iter().all(u8::is_ascii_whitespace) {
        &output.stdout
    } else {
        &output.stderr
    };
    String::from_utf8_lossy(stream).trim().into()
}

fn attempt_checkout(root: &Path, checkout: Checkout) -> Result<CheckoutOutcome, String> {
    let output = match run_checkout(root, &checkout) {
        Ok(CheckoutAttempt::Applied(message)) => return Ok(CheckoutOutcome::Finished(message)),
        Ok(CheckoutAttempt::Refused(output)) => output,
        Err(error) if error == history::CHECKOUT_CHANGED => {
            return Err(history::CHECKOUT_CHANGED_AFTER_CLOSE.into());
        }
        Err(error) => return Err(error),
    };
    // Commit Undo used to be an ordinary write job: don't offer to stash
    // ignored files on that path. Checkout Undo retains its collision flow.
    let paths = if matches!(
        &checkout.run.check,
        Some(history::CheckoutCheck::CommitUndo { .. })
    ) {
        Vec::new()
    } else {
        crate::command::checkout_ignored(root, &output.stderr)?
    };
    if paths.is_empty() {
        let message = output_message(&output);
        return Err(if message.is_empty() {
            "Git command failed".into()
        } else {
            message
        });
    }
    Ok(CheckoutOutcome::Collision(Box::new(OverwriteIgnored {
        checkout,
        paths,
    })))
}

fn stash_tip(root: &Path) -> Result<Option<String>, String> {
    let result = supervised_git_output(
        root,
        &[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "refs/stash".into(),
        ],
        &[],
        CHECKOUT_TIMEOUT,
    )?;
    if result.success() {
        let id = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(Some(id));
        }
    } else if result.code == Some(1) {
        return Ok(None);
    }
    Err("Can't find the ignored files in Stashes".into())
}

fn stash_selector(root: &Path, id: &str) -> String {
    let args = ["stash".into(), "list".into(), "--format=%H %gd".into()];
    if let Ok(result) = supervised_git_output(root, &args, &[], CHECKOUT_TIMEOUT)
        && result.success()
        && let Some(selector) = String::from_utf8_lossy(&result.stdout)
            .lines()
            .find_map(|line| {
                let (hash, selector) = line.split_once(' ')?;
                (hash == id).then_some(selector)
            })
    {
        return selector.to_owned();
    }
    id.to_owned()
}

/// Check out just the saved untracked tree, using a private index. Without
/// --force, checkout-index will not overwrite a file that appeared meanwhile.
fn restore_ignored(root: &Path, stash: &str, paths: &[PathBuf]) -> bool {
    let Ok(temp) = tempfile::tempdir() else {
        return false;
    };
    let env = [(
        "GIT_INDEX_FILE".into(),
        temp.path().join("index").into_os_string(),
    )];
    let tree = ["read-tree".into(), format!("{stash}^3").into()];
    let mut restore = vec!["checkout-index".into(), "--".into()];
    restore.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let run = |args: &[OsString]| {
        supervised_git_output(root, args, &env, CHECKOUT_TIMEOUT)
            .is_ok_and(|result| result.success())
    };
    run(&tree) && run(&restore)
}

fn replace_ignored(root: &Path, plan: OverwriteIgnored) -> Result<CheckoutOutcome, String> {
    let OverwriteIgnored { checkout, paths } = plan;
    // Retry without moving anything first. Git, not a second ref resolver,
    // decides whether these same paths still block the selected checkout.
    let output = match run_checkout(root, &checkout)? {
        CheckoutAttempt::Applied(message) => return Ok(CheckoutOutcome::Finished(message)),
        CheckoutAttempt::Refused(output) => output,
    };
    if crate::command::checkout_ignored(root, &output.stderr)? != paths {
        return Err("The branch or ignored files changed. Cancel and try again.".into());
    }
    let before = stash_tip(root)?;
    let mut save = [
        "--literal-pathspecs",
        "stash",
        "push",
        "--all",
        "--message",
        "Ignored files saved before switching",
        "--",
    ]
    .map(OsString::from)
    .to_vec();
    save.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let saved = supervised_git_output(root, &save, &[], CHECKOUT_TIMEOUT);
    let Some(stash) = stash_tip(root)?.filter(|after| Some(after) != before.as_ref()) else {
        return Err(saved
            .err()
            .unwrap_or_else(|| "Can't save the ignored files in Stashes".into()));
    };
    let failure = match saved {
        Ok(saved) if saved.success() => match run_checkout(root, &checkout) {
            Ok(CheckoutAttempt::Applied(message)) => {
                let saved = "Ignored files saved in Stashes";
                let notice = if checkout.undo.is_some() {
                    format!("{message} · {saved}")
                } else {
                    saved.into()
                };
                return Ok(CheckoutOutcome::Finished(notice));
            }
            Ok(CheckoutAttempt::Refused(output)) => output_message(&output),
            Err(error) => error,
        },
        Ok(_) => "Can't save the ignored files in Stashes".into(),
        Err(error) => error,
    };
    if restore_ignored(root, &stash, &paths) {
        Err(failure)
    } else {
        let selector = stash_selector(root, &stash);
        // Leave room for the recovery entry even if a hook prints pages of output.
        let failure = match failure.char_indices().nth(8192) {
            Some((end, _)) => format!("{}…", &failure[..end]),
            None => failure,
        };
        Ok(CheckoutOutcome::StashRecovery(format!(
            "Switch failed. Your ignored files were saved in Stashes. Stash: {selector}.\n{failure}"
        )))
    }
}

/// What a confirmed sheet runs.
#[derive(Clone)]
pub(super) enum Op {
    Pull(history::Pull),
    Push(history::Push),
    Undo(history::Undo),
    Rewrite(Box<history::Rewrite>),
    PushTag(String),
    /// `y` on a commit: what each choice of the menu copies.
    Copy(Vec<(String, String)>),
}

/// The commands of a confirmed sheet still to run, and the notice once they all succeed.
pub(super) struct Running {
    rest: Vec<Run>,
    timeout: Duration,
    done: String,
    fixup_written: bool,
    capture_warning: Option<String>,
    /// The last progress line Git printed, while it runs.
    pub(super) progress: Option<String>,
}

impl Op {
    /// Whether confirming changes the repository or a remote (the copy menu does not).
    pub(super) fn writes(&self) -> bool {
        !matches!(self, Self::Copy(_))
    }

    /// The sheet's confirm button: the verb the title asks about.
    pub(super) fn verb(&self) -> &'static str {
        self.button()
            .trim_start_matches("[ ")
            .trim_end_matches(" ]")
    }

    /// The confirm button of the older painter, bracketed.
    pub(super) fn button(&self) -> &'static str {
        match self {
            Self::Pull(_) => "[ Pull ]",
            Self::Push(_) => "[ Push ]",
            Self::Undo(undo) if undo.redo => "[ Redo ]",
            Self::Undo(_) => "[ Undo ]",
            Self::PushTag(_) => "[ Push tag ]",
            Self::Copy(_) => "[ Copy ]",
            Self::Rewrite(plan) => match plan.edit {
                Edit::Squash => "[ Squash ]",
                Edit::Fixup => "[ Fixup ]",
                Edit::Drop => "[ Drop ]",
                Edit::Edit => "[ Edit ]",
                Edit::Reword(_) => "[ Reword ]",
                Edit::MoveDown | Edit::MoveUp => "[ Move ]",
                Edit::Amend => "[ Amend ]",
            },
        }
    }
}

fn head_change(undo: &history::Undo) -> String {
    let (from, to) = undo.head();
    if let Some(destination) = undo.checkout_destination() {
        // Leave space for the arrow and destination hash on a compact sheet.
        let from = shell::chrome::ellipsize(&from, 17);
        let to = shell::chrome::ellipsize(&to, 17);
        format!("{from} → {to} ({})", history::short(destination.tip()))
    } else {
        format!("{from} → {to}")
    }
}

fn read_only(label: &str, value: String) -> Field {
    let mut field = Field::new(label, value);
    field.read_only = true;
    field.input.cursor = 0;
    field
}

fn wrapped_read_only(label: &str, value: String) -> Field {
    let mut field = read_only(label, value);
    field.multiline = true;
    field
}

impl GitScreen {
    /// `p` pull, `z` `Z` undo and redo, `P` on a tag, and the History keys that rewrite the
    /// selected commit: `s` `f` `d` `e` `J` `K`, and `A` `W` on a commit below HEAD.
    pub(super) fn ops_key(&mut self, key: &KeyEvent) -> bool {
        if self.page != 0 || key.modifiers.control || key.modifiers.alt {
            return false;
        }
        let KeyCode::Char(code) = key.code else {
            return false;
        };
        let history =
            self.focus == 2 && self.last_list == 2 && self.history_view == 0 && !self.commit_open;
        // A commit is in hand on History and in its open file list.
        let commit = history
            || matches!(self.focus, 0 | 4)
                && self.last_list == 2
                && self.history_view == 0
                && self.commit.is_some()
                && self.preview_anchor.is_none();
        let result = match code {
            'y' if commit => self.copy_form(),
            'o' if commit => self.open_commit(),
            'z' | 'Z' => self.undo_form(code == 'Z'),
            'p' if self.focus != 3 => self.pull_form(),
            'P' if self.focus == 1 && self.subview == 3 => self.push_tag_form(),
            's' if history => self.history_form(Edit::Squash),
            'f' if history => self.history_form(Edit::Fixup),
            'd' if history => self.history_form(Edit::Drop),
            'e' if history => self.history_form(Edit::Edit),
            'J' if history => self.history_form(Edit::MoveDown),
            'K' if history => self.history_form(Edit::MoveUp),
            // HEAD keeps the native amend and reword sheets (`rewrite.rs`).
            'A' if history && !self.head_selected() => self.history_form(Edit::Amend),
            'W' if history && !self.head_selected() => {
                self.history_form(Edit::Reword(String::new()))
            }
            _ => return false,
        };
        self.report(result);
        true
    }

    /// The commit `y` and `o` act on: the open commit, or the selected History row.
    fn commit_in_hand(&self) -> Result<git2::Oid, String> {
        let id = if matches!(self.focus, 0 | 4) {
            self.commit.as_ref().map(|commit| commit.id.clone())
        } else {
            self.selected_row()
                .and_then(|row| row.value.to_str().map(str::to_owned))
        };
        id.and_then(|id| git2::Oid::from_str(&id).ok())
            .ok_or_else(|| "Select a commit".into())
    }

    /// `y`: a menu of what to copy: the SHA, the short SHA, the subject, the forge URL.
    fn copy_form(&mut self) -> Result<(), String> {
        let id = self.commit_in_hand()?;
        let subject = git2::Repository::open(&self.root)
            .ok()
            .and_then(|repository| {
                let commit = repository.find_commit(id).ok()?;
                commit.summary().map(str::to_owned)
            })
            .unwrap_or_default();
        let mut items = vec![
            ("SHA".to_owned(), id.to_string()),
            ("Short SHA".to_owned(), history::short(id)),
            ("Subject".to_owned(), subject),
        ];
        if let Ok(url) = commit_url(&self.root, id, &self.github) {
            items.push(("URL".to_owned(), url));
        }
        let mut field = Field::new("Copy", items[0].0.clone());
        field.choices = items.iter().map(|(label, _)| label.clone()).collect();
        let shown = items
            .iter()
            .map(|(label, value)| read_only(label, value.clone()))
            .collect::<Vec<_>>();
        let mut fields = vec![field];
        fields.extend(shown);
        self.op_form(
            format!("Copy from {}", history::short(id)),
            Op::Copy(items),
            fields,
            0,
        );
        Ok(())
    }

    /// `o`: the commit's page on the forge behind the default remote, in the browser.
    fn open_commit(&mut self) -> Result<(), String> {
        let url = commit_url(&self.root, self.commit_in_hand()?, &self.github)?;
        crate::forge::checkout::open_url(&url)?;
        self.notice = format!("Opened {url}");
        Ok(())
    }

    fn head_selected(&self) -> bool {
        let head = git2::Repository::open(&self.root)
            .ok()
            .and_then(|repository| repository.head().ok()?.target())
            .map(|id| id.to_string());
        self.selected_row()
            .and_then(|row| row.value.to_str().map(str::to_owned))
            .is_none_or(|id| Some(id) == head)
    }

    fn op_form(&mut self, title: String, op: Op, fields: Vec<Field>, selected: usize) {
        self.form = Some(Form {
            title,
            action: FormAction::Run(Box::new(op)),
            fields,
            selected,
            focus_moved: false,
        });
        self.notice.clear();
    }

    pub(super) fn pull_form(&mut self) -> Result<(), String> {
        self.allow_write()?;
        let plan = history::pull(&self.root)?;
        let mut mode = Field::new("Mode", plan.mode.label().into());
        mode.choices = PullMode::ALL
            .iter()
            .map(|mode| mode.label().to_owned())
            .collect();
        let target = &plan.target;
        let fields = vec![
            read_only("Branch", target.branch.clone()),
            wrapped_read_only("From", format!("{} {}", target.remote, target.reference)),
            mode,
            wrapped_read_only(
                "Checkout",
                crate::command::display_paths([target.checkout()]),
            ),
        ];
        let selected = fields.len();
        // Enter pulls in the configured mode; Up/Down changes the mode when focused.
        self.op_form(
            format!("Pull {} into {}?", target.other_branch(), target.branch),
            Op::Pull(plan),
            fields,
            selected,
        );
        Ok(())
    }

    pub(super) fn push_form(&mut self) -> Result<(), String> {
        self.allow_write()?;
        let plan = history::push(&self.root)?;
        let target = &plan.target;
        let fields = vec![
            read_only("Branch", target.branch.clone()),
            wrapped_read_only("To", format!("{} {}", target.remote, target.reference)),
            wrapped_read_only(
                "Checkout",
                crate::command::display_paths([target.checkout()]),
            ),
        ];
        let selected = fields.len();
        self.op_form(
            format!("Push {} to {}?", target.branch, target.other_branch()),
            Op::Push(plan),
            fields,
            selected,
        );
        Ok(())
    }

    fn undo_form(&mut self, redo: bool) -> Result<(), String> {
        self.allow_write()?;
        let undo = history::undo(&self.root, redo)?;
        let mut fields = vec![read_only(
            if redo { "Redoes" } else { "Undoes" },
            undo.entry.clone(),
        )];
        if let Some(run) = undo.run() {
            fields.push(read_only("HEAD", head_change(&undo)));
            let mut command = read_only("Runs", run.display());
            command.multiline = true;
            fields.push(command);
        }
        let selected = fields.len();
        let title = if redo {
            "Redo the undone action?"
        } else {
            "Undo the last action?"
        };
        self.op_form(title.into(), Op::Undo(undo), fields, selected);
        Ok(())
    }

    fn push_tag_form(&mut self) -> Result<(), String> {
        self.allow_write()?;
        let tag = self
            .selected_row()
            .and_then(|row| row.value.to_str().map(str::to_owned))
            .ok_or("Select a tag to push")?;
        let remote = history::default_remote(&self.root)?;
        let mut field = Field::new("Remote", remote);
        field.choices = git2::Repository::open(&self.root)
            .and_then(|repository| repository.remotes())
            .map(|remotes| remotes.iter().flatten().map(str::to_owned).collect())
            .unwrap_or_default();
        let fields = vec![read_only("Tag", tag.clone()), field];
        self.op_form(format!("Push tag {tag}?"), Op::PushTag(tag), fields, 2);
        Ok(())
    }

    pub(super) fn history_reference(&self) -> Result<&str, String> {
        if self.history_loading
            || self
                .history_target
                .as_ref()
                .is_some_and(|(root, _)| root != &self.root)
        {
            return Err("Open the selected repository and wait for its history".into());
        }
        Ok(self
            .history_target
            .as_ref()
            .and_then(|(_, reference)| reference.to_str())
            .unwrap_or("HEAD"))
    }

    pub(super) fn history_rewrite_hints(&self) -> bool {
        !self.branch.is_empty()
            && self.history_reference().is_ok_and(|reference| {
                reference == "HEAD" || reference.strip_prefix("refs/heads/") == Some(&self.branch)
            })
    }

    fn history_form(&mut self, edit: Edit) -> Result<(), String> {
        self.allow_write()?;
        let reference = self.history_reference()?;
        let target = self
            .selected_row()
            .and_then(|row| {
                row.value
                    .to_str()
                    .and_then(|id| git2::Oid::from_str(id).ok())
            })
            .ok_or("Select a history commit")?;
        let plan = history::rewrite(&self.root, reference, target, edit.clone())?;
        let mut fields = Vec::new();
        if matches!(edit, Edit::Reword(_)) {
            let message = git2::Repository::open(&self.root)
                .ok()
                .and_then(|repository| {
                    let commit = repository.find_commit(target).ok()?;
                    commit.message().map(|text| text.trim_end().to_owned())
                })
                .unwrap_or_default();
            let mut field = Field::new("Message", message);
            field.multiline = true;
            fields.push(field);
        }
        fields.push(read_only("Commit", plan.commit.clone()));
        if let Some(other) = &plan.other {
            let label = match edit {
                Edit::MoveDown | Edit::MoveUp => "Swaps with",
                _ => "Into",
            };
            fields.push(read_only(label, other.clone()));
        }
        if edit == Edit::Amend {
            fields.push(read_only("Source", "Index".into()));
        }
        let replays = plan.steps.len();
        let onto = plan
            .base
            .map_or_else(|| "the root".to_owned(), history::short);
        fields.push(read_only(
            "Rebases",
            format!(
                "{replays} commit{} onto {onto}",
                if replays == 1 { "" } else { "s" }
            ),
        ));
        let runs = plan.runs();
        fields.push(read_only(
            "Runs",
            runs.last().map(Run::display).unwrap_or_default(),
        ));
        let branch = plan
            .checkout
            .reference()
            .map_or("detached HEAD", history::ref_label);
        let id = history::short(target);
        let title = if edit == Edit::Drop {
            let branch = shell::chrome::ellipsize(branch, 29);
            format!("Drop {id} from {branch}?")
        } else {
            format!("{} · {id} on {branch}?", edit.title())
        };
        // A reword starts in its message; every other sheet on its confirm button.
        let selected = if matches!(edit, Edit::Reword(_)) {
            0
        } else {
            fields.len()
        };
        self.op_form(title, Op::Rewrite(Box::new(plan)), fields, selected);
        Ok(())
    }

    /// The confirmed sheet's commands, built from its fields.
    pub(super) fn submit_op(&mut self, op: Op, values: &[String]) -> Result<(), String> {
        let (runs, timeout, done) = match op {
            Op::Pull(plan) => {
                let mode = values
                    .get(2)
                    .and_then(|label| PullMode::parse(label))
                    .ok_or("Choose merge, rebase or fast-forward only with Up/Down")?;
                let done = format!(
                    "Pulled {} into {} ({})",
                    plan.target.other_branch(),
                    plan.target.branch,
                    mode.label()
                );
                (vec![history::pull_run(plan, mode)], NETWORK, done)
            }
            Op::Push(plan) => {
                let done = format!(
                    "Pushed {} to {}",
                    plan.target.branch,
                    plan.target.other_branch()
                );
                (vec![plan.run()], NETWORK, done)
            }
            Op::Undo(undo) => {
                let Some(run) = undo.run() else {
                    undo.restore_index(&self.root)?;
                    self.notice.clear();
                    let result = self.refresh();
                    self.refreshing_after_write = result.is_ok();
                    return result;
                };
                // Recheck HEAD and staged changes before running the confirmed plan.
                if let Some(check) = &run.check {
                    check.verify(&self.root)?;
                }
                if history::undo(&self.root, undo.redo)? != undo {
                    return Err("The reflog changed; cancel and press the key again".into());
                }
                let target = undo
                    .checkout_destination()
                    .map_or_else(|| undo.head().1, history::CheckoutTarget::revision);
                return self.submit_checkout(Checkout {
                    run,
                    target: target.into(),
                    undo: Some(undo),
                    timeout: CHECKOUT_TIMEOUT,
                });
            }
            Op::Rewrite(mut plan) => {
                if let Edit::Reword(message) = &mut plan.edit {
                    let text = values.first().map_or("", |text| text.trim());
                    if text.is_empty() {
                        return Err("Message is required".into());
                    }
                    *message = text.to_owned();
                }
                history::CheckoutCheck::Rewrite(plan.checkout.clone()).verify(&self.root)?;
                let reference = plan.checkout.reference().unwrap_or("HEAD");
                let fresh = history::rewrite(&self.root, reference, plan.target, plan.edit.clone())
                    .map_err(|error| {
                        history::CheckoutCheck::Rewrite(plan.checkout.clone())
                            .verify(&self.root)
                            .err()
                            .unwrap_or(error)
                    })?;
                if fresh.checkout != plan.checkout || fresh.todo != plan.todo {
                    return Err(history::CHECKOUT_CHANGED.into());
                }
                (plan.runs(), LOCAL, plan.done())
            }
            Op::Copy(items) => {
                let chosen = values.first().map_or("", String::as_str);
                let (label, text) = items
                    .into_iter()
                    .find(|(label, _)| label == chosen)
                    .ok_or("Choose what to copy with Up/Down")?;
                self.outcome = Some(ScreenOutcome::Copy(text));
                let what = match label.as_str() {
                    "Short SHA" => "short SHA",
                    "Subject" => "subject",
                    other => other,
                };
                self.notice = format!("Copied the {what}");
                return Ok(());
            }
            Op::PushTag(tag) => {
                let remote = values.get(1).map_or("", |remote| remote.trim());
                (
                    vec![history::push_tag(remote, &tag)?],
                    NETWORK,
                    format!("Pushed tag {tag} to {remote}"),
                )
            }
        };
        self.start_runs(runs, timeout, done)
    }

    pub(super) fn submit_checkout(&mut self, checkout: Checkout) -> Result<(), String> {
        let detached = matches!(
            self.form.as_ref().map(|form| &form.action),
            Some(FormAction::CheckoutDetached(_))
        );
        self.start_checkout(move |root| attempt_checkout(root, checkout))?;
        if detached {
            self.reflog_submitting = true;
        } else {
            self.form = None;
        }
        Ok(())
    }

    pub(super) fn overwrite_ignored(&mut self, plan: OverwriteIgnored) -> Result<(), String> {
        self.start_checkout(move |root| replace_ignored(root, plan))
    }

    fn start_checkout(
        &mut self,
        operation: impl FnOnce(&Path) -> Result<CheckoutOutcome, String> + Send + 'static,
    ) -> Result<(), String> {
        let root = self.root.clone();
        let (tx, rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("dusty-git-checkout".into())
            .spawn(move || {
                let _ = tx.send(operation(&root));
            })
            .map_err(|_| "Can't start Git")?;
        self.checkout_reply = Some(rx);
        self.busy = true;
        self.notice = "Running Git…".into();
        Ok(())
    }

    pub(super) fn poll_checkout(&mut self) -> bool {
        let Some(reply) = &self.checkout_reply else {
            return false;
        };
        let result = match reply.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("Git stopped before switching".into()),
        };
        self.checkout_reply = None;
        self.busy = false;
        self.finish_reflog_form(result.is_ok());
        match result {
            Ok(CheckoutOutcome::Collision(plan)) => {
                self.form(
                    if plan.can_replace() {
                        "Replace ignored files and switch?"
                    } else {
                        "Switch blocked"
                    },
                    FormAction::OverwriteIgnored {
                        plan,
                        note_top: std::cell::Cell::new(0),
                    },
                    Vec::new(),
                );
                if let Some(form) = &mut self.form {
                    form.selected = form.fields.len() + 1;
                }
            }
            Ok(CheckoutOutcome::StashRecovery(message)) => {
                let refreshed = self.refresh();
                self.refreshing_after_write = refreshed.is_ok();
                self.report(refreshed);
                self.show_action_message("Switch failed", message);
            }
            Ok(CheckoutOutcome::Finished(message)) | Err(message) => {
                self.notice = message.replace(['\r', '\n'], " ");
                let refreshed = self.refresh();
                self.refreshing_after_write = refreshed.is_ok();
                self.report(refreshed);
            }
        }
        true
    }

    fn start_runs(
        &mut self,
        runs: Vec<Run>,
        timeout: Duration,
        done: String,
    ) -> Result<(), String> {
        let mut runs = runs.into_iter();
        let first = runs.next().ok_or("Nothing to run")?;
        self.jobs.start_run(&self.root, first, timeout)?;
        self.busy = true;
        self.notice = "Running Git…".into();
        self.running = Some(Running {
            rest: runs.collect(),
            timeout,
            done,
            fixup_written: false,
            progress: None,
            capture_warning: None,
        });
        Ok(())
    }

    /// A write finished: start the sheet's next command, if it has one and this one
    /// succeeded. `true` while the sheet keeps running.
    pub(super) fn next_op_step(
        &mut self,
        result: &mut Result<String, String>,
        warning: &mut Option<String>,
    ) -> bool {
        let Some(running) = self.running.as_mut() else {
            return false;
        };
        if let Some(previous) = running.capture_warning.take() {
            *warning = Some(match warning.take() {
                Some(current) if current != previous => format!("{previous} · {current}"),
                _ => previous,
            });
        }
        if result.is_err() || running.rest.is_empty() {
            return false;
        }
        let next = running.rest.remove(0);
        running.fixup_written |= matches!(&next.check, Some(history::CheckoutCheck::Autosquash(_)));
        let timeout = running.timeout;
        running.progress = None;
        match self.jobs.start_run(&self.root, next, timeout) {
            Ok(()) => {
                running.capture_warning = warning.take();
                true
            }
            Err(error) => {
                *result = Err(error);
                false
            }
        }
    }

    /// The notice for a finished sheet: its receipt, or where a stopped rebase or merge
    /// waits. `None` when no sheet was running.
    pub(super) fn finish_op(&mut self, result: &Result<String, String>) -> Option<String> {
        let running = self.running.take()?;
        let error = match result {
            Ok(_) => return Some(running.done),
            Err(error) => error,
        };
        let notice = if error == history::CHECKOUT_CHANGED {
            // Confirmed Git command sheets have closed. Native rewrites keep
            // their own forms and handle their worker results separately.
            history::CHECKOUT_CHANGED_AFTER_CLOSE.into()
        } else if error == history::NETWORK_TARGET_CHANGED {
            error.clone()
        } else {
            let state = git2::Repository::open(&self.root)
                .map_or(git2::RepositoryState::Clean, |repository| {
                    repository.state()
                });
            if running.fixup_written
                && matches!(
                    state,
                    git2::RepositoryState::Rebase
                        | git2::RepositoryState::RebaseInteractive
                        | git2::RepositoryState::RebaseMerge
                )
            {
                return Some(
                    "Fixup committed. Resolve conflicts, then C continues, S skips, X aborts."
                        .into(),
                );
            }
            if state != git2::RepositoryState::Clean {
                // Git has no skip for a merge (a pull that merges stops as one).
                let skip = if state == git2::RepositoryState::Merge {
                    ""
                } else {
                    "S skips, "
                };
                format!("Stopped on a conflict · resolve it, then C continues, {skip}X aborts")
            } else {
                error.replace(['\r', '\n'], " ")
            }
        };
        Some(if running.fixup_written {
            format!("{notice} The fixup commit was written.")
        } else {
            notice
        })
    }

    /// While a sheet's command runs, the footer shows Git's last progress line.
    pub(super) fn op_progress(&mut self) -> bool {
        let Some(running) = self.running.as_mut() else {
            return false;
        };
        let line = self.jobs.write_progress();
        if line.is_some() && line != running.progress {
            running.progress = line;
            return true;
        }
        false
    }

    /// The footer's line while Git runs.
    pub(super) fn busy_line(&self) -> &str {
        self.running
            .as_ref()
            .and_then(|running| running.progress.as_deref())
            .unwrap_or("Git operation running…")
    }
}

/// The web page of commit `id` on the forge behind the default remote: GitHub's and
/// Gitea's `/commit/<sha>`, GitLab's `/-/commit/<sha>`.
fn commit_url(
    root: &Path,
    id: git2::Oid,
    forge: &super::github::Workspace,
) -> Result<String, String> {
    let remote =
        history::default_remote(root).map_err(|_| "No remote to open the commit on".to_owned())?;
    let repository = git2::Repository::open(root).map_err(|error| error.message().to_owned())?;
    let remote = repository
        .find_remote(&remote)
        .map_err(|error| error.message().to_owned())?;
    let remote = remote.url().ok_or("The remote is not a web forge")?;
    let (base, gitlab) = match forge.advertised_checkout_base(remote) {
        Some(known) => known,
        None => {
            let base = crate::web_base(remote).ok_or("The remote is not a web forge")?;
            let gitlab = base.host_str().is_some_and(|host| host.contains("gitlab"));
            (base.as_str().trim_end_matches('/').to_owned(), gitlab)
        }
    };
    Ok(if gitlab {
        format!("{base}/-/commit/{id}")
    } else {
        format!("{base}/commit/{id}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use history::tests::{commit, git, head, repo};
    use std::fs;
    use std::time::Instant;

    #[test]
    fn pull_sheet_refuses_a_branch_switch_before_submit() {
        let upstream = repo();
        let base = commit(upstream.path(), "base");
        let local = tempfile::tempdir().unwrap();
        git(
            local.path(),
            &["clone", "-q", upstream.path().to_str().unwrap(), "local"],
        );
        let root = local.path().join("local");
        git(&root, &["branch", "--track", "other", "origin/main"]);
        let latest = commit(upstream.path(), "new");
        let mut screen = GitScreen::open(&root).unwrap();

        screen.pull_form().unwrap();
        git(&root, &["switch", "other"]);
        screen.submit().unwrap();
        drain_write(&mut screen);
        assert_eq!(screen.notice, history::NETWORK_TARGET_CHANGED);
        assert_eq!(head(&root), base);

        git(&root, &["switch", "main"]);
        screen.pull_form().unwrap();
        screen.submit().unwrap();
        drain_write(&mut screen);
        assert_eq!(head(&root), latest);
    }

    #[test]
    fn pull_sheet_refuses_fetch_url_rewrite_drift() {
        let upstream = repo();
        let base = commit(upstream.path(), "base");
        let local = tempfile::tempdir().unwrap();
        git(
            local.path(),
            &["clone", "-q", upstream.path().to_str().unwrap(), "work"],
        );
        let root = local.path().join("work");
        let other = tempfile::tempdir().unwrap();
        git(
            other.path(),
            &["clone", "-q", upstream.path().to_str().unwrap(), "other"],
        );
        let other = other.path().join("other");
        git(&other, &["config", "user.name", "History"]);
        git(&other, &["config", "user.email", "history@example.invalid"]);
        git(&other, &["config", "commit.gpgsign", "false"]);
        commit(&other, "different");
        let alias = "fetch-alias:project";
        let key = format!("url.{}.insteadOf", upstream.path().display());
        git(&root, &["remote", "set-url", "origin", alias]);
        git(&root, &["config", &key, alias]);
        let mut screen = GitScreen::open(&root).unwrap();
        screen.pull_form().unwrap();

        git(&root, &["config", "--unset", &key]);
        git(
            &root,
            &[
                "config",
                &format!("url.{}.insteadOf", other.display()),
                alias,
            ],
        );
        screen.submit().unwrap();
        drain_write(&mut screen);
        assert_eq!(screen.notice, history::NETWORK_TARGET_CHANGED);
        assert_eq!(head(&root), base);
    }

    #[cfg(unix)]
    #[test]
    fn opening_pull_and_push_sheets_does_not_start_a_transport() {
        let local = repo();
        let root = local.path();
        commit(root, "base");
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            root,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        for service in ["uploadpack", "receivepack"] {
            git(
                root,
                &[
                    "config",
                    &format!("remote.origin.{service}"),
                    "touch .git/transport-attempted; false",
                ],
            );
        }
        git(root, &["config", "branch.main.remote", "origin"]);
        git(root, &["config", "branch.main.merge", "refs/heads/main"]);
        git(root, &["config", "remote.origin.push", "HEAD:review"]);
        let mut screen = GitScreen::open(root).unwrap();

        screen.pull_form().unwrap();
        let push = screen.push_form();
        assert!(
            !root.join(".git/transport-attempted").exists(),
            "opening a sheet must not start upload-pack or receive-pack",
        );
        push.unwrap();
        assert_eq!(
            screen.form.as_ref().unwrap().fields[1].input.input,
            "origin review"
        );
        assert!(!screen.busy);
    }

    #[test]
    fn push_sheet_uses_a_tracked_short_branch_and_refuses_push_url_drift() {
        let local = repo();
        let root = local.path();
        let base = commit(root, "base").to_string();
        let targets = tempfile::tempdir().unwrap();
        let [approved, other] = ["approved.git", "other.git"].map(|name| {
            git(
                targets.path(),
                &["clone", "-q", "--bare", root.to_str().unwrap(), name],
            );
            let target = targets.path().join(name);
            git(&target, &["update-ref", "refs/heads/review", &base]);
            target
        });
        let alias = "write-alias:project";
        let key = format!("url.{}.pushInsteadOf", approved.display());
        // The fetch and push URLs differ. Only the latter is relevant to this sheet.
        git(root, &["remote", "add", "origin", alias]);
        git(
            root,
            &[
                "config",
                &format!("url.{}.insteadOf", other.display()),
                alias,
            ],
        );
        git(root, &["config", &key, alias]);
        git(root, &["config", "remote.origin.push", "HEAD:review"]);
        git(root, &["update-ref", "refs/remotes/origin/review", &base]);

        let first = commit(root, "first").to_string();
        let mut screen = GitScreen::open(root).unwrap();
        screen.push_form().unwrap();
        screen.submit().unwrap();
        drain_write(&mut screen);
        assert_eq!(git(&approved, &["rev-parse", "refs/heads/review"]), first);
        assert_eq!(git(&approved, &["rev-parse", "refs/heads/main"]), base);
        assert_eq!(git(&other, &["rev-parse", "refs/heads/review"]), base);

        commit(root, "second");
        screen.push_form().unwrap();
        git(root, &["config", "--unset", &key]);
        git(
            root,
            &[
                "config",
                &format!("url.{}.pushInsteadOf", other.display()),
                alias,
            ],
        );
        screen.submit().unwrap();
        drain_write(&mut screen);
        assert_eq!(screen.notice, history::NETWORK_TARGET_CHANGED);
        assert_eq!(git(&other, &["rev-parse", "refs/heads/review"]), base);
        assert_eq!(git(&approved, &["rev-parse", "refs/heads/review"]), first);
    }

    #[test]
    fn network_sheets_show_long_targets_at_100_columns() {
        let dirs = tempfile::tempdir().unwrap();
        let parent = dirs
            .path()
            .join(format!("{}-CHECKOUT-tail", "long".repeat(20)));
        let root = parent.join("work");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "--template=", "-b", "main"]);
        git(&root, &["config", "user.name", "History"]);
        git(&root, &["config", "user.email", "history@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        commit(&root, "base");
        git(&root, &["remote", "add", "origin", "."]);
        git(&root, &["config", "branch.main.remote", "origin"]);
        let fetch = format!("refs/heads/{}-FROM-tail", "long".repeat(25));
        let push = format!("refs/heads/{}-TO-tail", "long".repeat(25));
        git(&root, &["config", "branch.main.merge", &fetch]);
        git(
            &root,
            &["config", "remote.origin.push", &format!("HEAD:{push}")],
        );

        let mut screen = GitScreen::open(&root).unwrap();
        for (open, label, tail) in [
            (
                GitScreen::pull_form as fn(&mut GitScreen) -> Result<(), String>,
                "From",
                "FROM-tail",
            ),
            (GitScreen::push_form, "To", "TO-tail"),
        ] {
            open(&mut screen).unwrap();
            let frame = screen.frame(100, 30);
            let grid = &frame.grid;
            let rows = (0..grid.height())
                .map(|row| grid.row_text(row))
                .collect::<Vec<_>>();
            for (label, tail) in [(label, tail), ("Checkout", "CHECKOUT-tail")] {
                let values = wrapped_sheet_value(grid, &rows, label);
                assert!(
                    values.contains(tail),
                    "{tail} not visible after {label}:\n{}",
                    rows.join("\n")
                );
            }
        }
    }

    fn wrapped_sheet_value(grid: &crate::ui::CellGrid, rows: &[String], label: &str) -> String {
        let label_row = (0..grid.height())
            .find(|&y| {
                rows[usize::from(y)]
                    .split('│')
                    .any(|text| text.trim() == label)
            })
            .unwrap();
        // The dialog is centered. Use its own borders, not the background
        // pane borders that may also be present on these rows.
        let is_edge = |x, y| grid.get(x, y).is_some_and(|cell| cell.symbol == "│");
        let middle = grid.width() / 2;
        let left = (0..middle).rev().find(|&x| is_edge(x, label_row)).unwrap();
        let right = (middle..grid.width())
            .find(|&x| is_edge(x, label_row))
            .unwrap();
        (label_row + 1..grid.height())
            .take_while(|&y| is_edge(left, y) && is_edge(right, y))
            .map(|y| {
                (left + 1..right)
                    .filter_map(|x| grid.get(x, y))
                    .filter(|cell| !cell.continuation)
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .fold(String::new(), |mut values, row| {
                values.push_str(row.trim());
                values
            })
    }

    #[test]
    fn network_sheets_distinguish_shared_branch_prefixes_and_show_the_focused_checkout() {
        let dir = repo();
        let root = dir.path();
        commit(root, "base");
        git(root, &["remote", "add", "origin", "."]);
        git(
            root,
            &["config", "remote.origin.push", "HEAD:refs/heads/shared"],
        );
        let mut screen = GitScreen::open(root).unwrap();
        for suffix in ["alpha", "bravo"] {
            let name = format!("topic-{}-{suffix}", "long".repeat(20));
            git(root, &["switch", "-c", &name]);
            git(
                root,
                &["config", &format!("branch.{name}.remote"), "origin"],
            );
            git(
                root,
                &[
                    "config",
                    &format!("branch.{name}.merge"),
                    "refs/heads/shared",
                ],
            );
            for open in [
                GitScreen::pull_form as fn(&mut GitScreen) -> Result<(), String>,
                GitScreen::push_form,
            ] {
                open(&mut screen).unwrap();
                let frame = screen.frame(100, 30);
                let rows: Vec<_> = (0..30).map(|y| frame.grid.row_text(y)).collect();
                assert!(
                    rows.iter()
                        .flat_map(|row| row.split('│'))
                        .map(str::trim)
                        .any(|part| {
                            part.starts_with("Branch  topic-")
                                && part.contains('…')
                                && part.ends_with(suffix)
                        }),
                    "branch {suffix} is not distinguishable in the sheet body:\n{}",
                    rows.join("\n"),
                );

                // Exercise layout without needing a filesystem that permits this path length.
                let form = screen.form.as_mut().unwrap();
                let checkout = form.fields.len() - 1;
                form.fields[checkout].input.input =
                    format!("/checkout-head/{}/checkout-tail", "directory/".repeat(165));
                form.move_focus(checkout);
                let frame = screen.frame(100, 30);
                let rows: Vec<_> = (0..30).map(|y| frame.grid.row_text(y)).collect();
                let checkout = wrapped_sheet_value(&frame.grid, &rows, "Checkout");
                assert!(
                    checkout.starts_with("/checkout-head/")
                        && checkout.contains('…')
                        && checkout.contains("/checkout-tail"),
                    "focused checkout lost its distinguishing text:\n{}",
                    rows.join("\n"),
                );
            }
        }
    }

    fn drain_write(screen: &mut GitScreen) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while screen.busy || screen.refreshing_after_write {
            assert!(Instant::now() < deadline, "Git job did not finish");
            screen.poll();
            std::thread::yield_now();
        }
    }

    fn amend_through_jobs(root: &Path, target: git2::Oid) -> String {
        let mut screen = GitScreen::open(root).unwrap();
        let plan = history::rewrite(root, "refs/heads/main", target, Edit::Amend).unwrap();
        screen.submit_op(Op::Rewrite(Box::new(plan)), &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while screen.busy {
            assert!(Instant::now() < deadline, "amend job did not finish");
            screen.poll();
            std::thread::yield_now();
        }
        screen.notice
    }

    #[test]
    fn amend_job_refuses_before_writing_fixup_without_a_head_log() {
        let directory = repo();
        let root = directory.path();
        git(root, &["config", "core.logAllRefUpdates", "false"]);
        let target = commit(root, "first");
        let tip = commit(root, "second");
        fs::write(root.join("first"), "amendment\n").unwrap();
        git(root, &["add", "first"]);
        assert!(
            !git2::Repository::open(root)
                .unwrap()
                .reference_has_log("HEAD")
                .unwrap()
        );

        assert_eq!(
            amend_through_jobs(root, target),
            history::FIXUP_WITHOUT_HISTORY
        );
        assert_eq!(head(root), tip);
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "first");
    }

    #[test]
    fn autosquash_conflict_reports_fixup_and_recovery_keys_together() {
        let directory = repo();
        let root = directory.path();
        let target = commit(root, "first");
        fs::write(root.join("first"), "later change\n").unwrap();
        git(root, &["add", "first"]);
        git(root, &["commit", "-q", "-m", "Later change"]);
        fs::write(root.join("first"), "fixup change\n").unwrap();
        git(root, &["add", "first"]);

        assert_eq!(
            amend_through_jobs(root, target),
            "Fixup committed. Resolve conflicts, then C continues, S skips, X aborts."
        );
        let repository = git2::Repository::open(root).unwrap();
        assert!(repository.index().unwrap().has_conflicts());
    }

    #[cfg(unix)]
    #[test]
    fn amend_does_not_autosquash_after_a_hook_switches_branches() {
        use std::os::unix::fs::PermissionsExt;

        let directory = repo();
        let root = directory.path();
        commit(root, "alpha");
        let target = commit(root, "bravo");
        git(root, &["switch", "-q", "-c", "topic"]);
        fs::write(root.join("bravo"), "topic change\n").unwrap();
        git(root, &["add", "bravo"]);
        git(root, &["commit", "-q", "--fixup", &target.to_string()]);
        let topic = head(root);
        git(root, &["switch", "-q", "main"]);
        commit(root, "charlie");
        fs::write(root.join("alpha"), "main change\n").unwrap();
        git(root, &["add", "alpha"]);

        git(root, &["config", "core.hooksPath", ".git/hooks"]);
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
        let hook = root.join(".git/hooks/post-commit");
        fs::write(&hook, "#!/bin/sh\ngit switch -q topic\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            amend_through_jobs(root, target),
            "Your checkout changed. Open the action again. The fixup commit was written."
        );
        assert_eq!(git(root, &["branch", "--show-current"]), "topic");
        assert_eq!(head(root), topic);
        assert_eq!(
            git(root, &["log", "-1", "--format=%s", "main"]),
            "fixup! bravo"
        );
    }
}
