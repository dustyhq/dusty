//! The Git sidebar's repository, reference and history tabs.
use super::*;

pub(super) struct ReflogEntry {
    pub id: String,
    pub label: String,
    pub key: PathBuf,
}
impl ReflogEntry {
    pub fn parse(text: &str) -> Vec<Self> {
        let mut commit_move_tip = None;
        text.lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, '\t');
                let id = fields.next()?;
                let date = fields.next()?;
                let subject = fields.next()?;
                git2::Oid::from_str(id).ok()?;
                Some((id, date, subject))
            })
            .enumerate()
            .filter_map(|(index, (id, date, subject))| {
                // switch -C can log the branch reset immediately before the
                // same-tip Dusty marker. Keep the marker's real ordinal and key.
                let duplicate = commit_move_tip == Some(id)
                    && subject.strip_prefix("branch: Reset to ") == Some(id);
                commit_move_tip = crate::history::is_commit_move_marker(subject).then_some(id);
                if duplicate {
                    return None;
                }
                Some(Self {
                    id: id.into(),
                    label: format!(
                        "{} HEAD@{{{index}}} {}",
                        &id[..7],
                        reflog_display_subject(subject)
                    ),
                    // A refresh can insert a new entry at HEAD@{0}. Preserve the selected event,
                    // rather than silently inspecting whatever now occupies its old ordinal.
                    key: format!("{id}\t{date}\t{subject}").into(),
                })
            })
            .collect()
    }
}

fn reflog_display_subject(subject: &str) -> std::borrow::Cow<'_, str> {
    use crate::history::{REDO_ACTION, SKIPPED_INDEX, UNDO_ACTION, UNSTAGE_ACTION};

    if subject.starts_with(UNSTAGE_ACTION) {
        return "Unstage changes".into();
    }
    if (subject.starts_with(UNDO_ACTION) || subject.starts_with(REDO_ACTION))
        && let Some((label, skip)) = subject.split_once(SKIPPED_INDEX)
        && let Some((_, suffix)) = skip.split_once(']')
    {
        return format!("{label}{suffix}").into();
    }
    subject.into()
}

impl GitScreen {
    pub(super) fn switch_sidebar_tab(&mut self, next: bool) {
        self.clear_file_selection();
        self.commit_open = false;
        self.commit = None;
        self.last_list = self.focus;
        self.notice.clear();
        match self.focus {
            0 => self.files_view = (self.files_view + if next { 1 } else { 2 }) % 3,
            1 => {
                let tabs = [0, 2, 3];
                let at = tabs
                    .iter()
                    .position(|view| *view == self.subview)
                    .unwrap_or(0);
                self.subview = tabs[(at + if next { 1 } else { 2 }) % tabs.len()];
            }
            2 => {
                self.history_view = 1 - self.history_view;
                if self.history_view == 1 {
                    let result = self.reload_reflog();
                    self.report(result);
                }
            }
            _ => return,
        }
        self.offsets[self.focus] = 0;
        self.preview_offset = 0;
        self.preview_column = 0;
        self.clamp();
        self.select_history();
        self.load_preview();
    }

    pub(super) fn worktree_rows(&self) -> Vec<Row> {
        self.refs
            .as_ref()
            .map(|refs| {
                refs.worktrees
                    .iter()
                    .map(|worktree| {
                        Row::plain(
                            format!(
                                "{}  {}{}{}{}",
                                worktree.branch.as_deref().unwrap_or("detached"),
                                worktree
                                    .path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy(),
                                if worktree.dirty { " *" } else { "" },
                                if worktree.locked { " [locked]" } else { "" },
                                worktree
                                    .operation
                                    .as_ref()
                                    .map(|operation| format!(" [{operation}]"))
                                    .unwrap_or_default(),
                            ),
                            worktree.path.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn remote_rows(&self) -> Vec<Row> {
        let mut rows = std::collections::BTreeMap::new();
        for remote in &self.remotes {
            let Some((name, rest)) = remote.split_once(char::is_whitespace) else {
                continue;
            };
            rows.entry(PathBuf::from(name))
                .or_insert_with(|| Row::plain(format!("{name}  {}", rest.trim()), name.into()));
        }
        if let Some(refs) = &self.refs {
            for branch in refs.branches.iter().filter(|branch| branch.remote) {
                rows.insert(
                    branch.name.clone().into(),
                    Row::plain(format!("  {}", branch.name), branch.name.clone().into()),
                );
            }
        }
        rows.into_values().collect()
    }

    pub(super) fn remote_branch(&self, row: &Row) -> bool {
        self.refs.as_ref().is_some_and(|refs| {
            refs.branches
                .iter()
                .any(|branch| branch.remote && Path::new(&branch.name) == row.value)
        })
    }

    pub(super) fn submodule_rows(&self) -> Vec<Row> {
        self.refs
            .as_ref()
            .and_then(|refs| refs.submodules.as_ref().ok())
            .map(|modules| {
                modules
                    .iter()
                    .map(|module| {
                        let entry = self.submodule_status(&module.path);
                        let staged = entry.is_some_and(|entry| entry.sources.staged);
                        let pointer_changed = module.initialized && module.head != module.index;
                        let unstaged = pointer_changed
                            || module.dirty
                            || entry.is_some_and(|entry| entry.sources.unstaged);
                        let mut state = Vec::new();
                        if staged {
                            state.push("staged pointer");
                        }
                        if pointer_changed {
                            state.push("unstaged pointer");
                        }
                        if module.dirty {
                            state.push("child files modified");
                        }
                        if !module.initialized {
                            state.push("uninitialized");
                        }
                        let status_known = self
                            .snapshot
                            .as_ref()
                            .is_some_and(|snapshot| !snapshot.truncated || entry.is_some());
                        if !status_known {
                            state.push("parent status unavailable");
                        } else if state.is_empty() {
                            state.push("clean");
                        }
                        // Match Changes: a check is staged, never merely a clean checkout.
                        let marker = match (staged, unstaged, module.initialized) {
                            (true, true, _) => "◐",
                            (true, false, _) => "✓",
                            (_, _, false) => "−",
                            _ => " ",
                        };
                        let code = self.submodule_change_code(module);
                        Row::plain(
                            format!(
                                "{marker} {code} {}  {}",
                                module.path.display(),
                                state.join(" · "),
                            ),
                            module.path.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn submodule_status(&self, path: &Path) -> Option<&GitStatusEntry> {
        self.snapshot
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.relative_path == path)
    }

    pub(super) fn submodule_change_code(&self, module: &GitSubmoduleEntry) -> String {
        let entry = self.submodule_status(&module.path);
        let known = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.truncated || entry.is_some());
        let mut code = entry
            .map(GitStatusEntry::change_code)
            .unwrap_or_else(|| (if known { "  " } else { "? " }).into())
            .chars()
            .collect::<Vec<_>>();
        // Raw parent diffs can omit untracked child files. Inventory still knows
        // they are dirty: merge that evidence into Y without losing X (or UU/D).
        if code[1] == ' ' && (module.dirty || module.initialized && module.head != module.index) {
            code[1] = 'M';
        }
        code.into_iter().collect()
    }

    /// Child file dirtiness cannot be staged in the parent. Only a new checkout revision
    /// remains actionable after its gitlink was staged, so the next toggle (`-`) must unstage it.
    pub(super) fn submodule_pointer_matches_index(&self, path: &Path) -> bool {
        self.refs
            .as_ref()
            .and_then(|refs| refs.submodules.as_ref().ok())
            .and_then(|modules| modules.iter().find(|module| module.path == path))
            .is_some_and(|module| module.initialized && module.head == module.index)
    }

    pub(super) fn submodule_preview(&mut self, path: &Path) {
        let Some(module) = self
            .refs
            .as_ref()
            .and_then(|refs| refs.submodules.as_ref().ok())
            .and_then(|modules| modules.iter().find(|module| module.path == path))
        else {
            return;
        };
        let entry = self.submodule_status(path);
        let index_state = if entry.is_some_and(|entry| entry.sources.staged) {
            "staged pointer · - unstages when checkout matches index"
        } else if self
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.truncated && entry.is_none())
        {
            "parent status unavailable"
        } else {
            "no staged pointer"
        };
        self.preview = format!("{}\n\nParent index: {}\nCheckout pointer: {}\nCheckout: {}\nWorking files: {}\nExpected index commit: {}\nActual submodule HEAD: {}\n\nEnter opens this submodule as a Git workspace.\ns stages its commit pointer and - toggles it, not its working files.\nu initializes / updates to the expected commit after confirmation.",
            module.path.display(),
            index_state,
            if !module.initialized { "not initialized" } else if module.head != module.index { "unstaged revision · s stages this pointer" } else { "matches index" },
            if module.initialized { "initialized" } else { "not initialized" },
            if module.dirty { "modified; open the submodule to review and commit them" } else { "clean" },
            module.index.as_deref().unwrap_or("none"),
            module.head.as_deref().unwrap_or("none"),
        ).into();
    }

    pub(super) fn open_sidebar_repository(&mut self, row: Row) -> ScreenOutcome {
        if self.files_view == 1 {
            if self.tutor.is_some() {
                let result = self.tutor_worktree(&row.value);
                self.report(result);
                return ScreenOutcome::Continue;
            }
            return ScreenOutcome::OpenRepository(row.value);
        }
        let path = self.root.join(&row.value);
        let allowed = crate::files::canonicalize(&path)
            .ok()
            .zip(crate::files::canonicalize(&self.root).ok())
            .is_some_and(|(path, root)| path.starts_with(root));
        if allowed && git2::Repository::open(&path).is_ok_and(|repo| repo.workdir().is_some()) {
            ScreenOutcome::OpenRepository(path)
        } else {
            self.notice = "Submodule is not initialized; u reviews its initialization".into();
            ScreenOutcome::Continue
        }
    }

    pub(super) fn toggle_submodule(&mut self, path: PathBuf) -> Result<(), String> {
        self.allow_write()?;
        let module = self
            .refs
            .as_ref()
            .and_then(|refs| refs.submodules.as_ref().ok())
            .and_then(|modules| modules.iter().find(|module| module.path == path))
            .ok_or("Select a listed submodule")?;
        let entry = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .entries
                .iter()
                .find(|entry| entry.relative_path == path)
        });
        let unstage = entry.is_some_and(|entry| {
            entry.sources.staged && (module.head == module.index || !entry.sources.unstaged)
        });
        match self.stage_intent {
            Some(true) if unstage => return Err("The pointer is staged · - unstages it".into()),
            Some(false) if !unstage => return Err("No staged pointer to unstage".into()),
            _ => {}
        }
        if !unstage && !module.initialized {
            return Err("Initialize or open the submodule before staging its revision".into());
        }
        if !unstage && module.head == module.index {
            return Err("Commit changes inside the submodule before staging its revision".into());
        }
        if unstage {
            let index = git_index_fingerprint(&self.root).map_err(|error| error.to_string())?;
            let head = git_head_id(&self.root).map_err(|error| error.to_string())?;
            return self.unstage_paths([path].into(), index, head, None);
        }
        self.write(vec![
            "--literal-pathspecs".into(),
            "add".into(),
            "--".into(),
            path.into_os_string(),
        ])
    }

    pub(super) fn initialize_submodule(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let module = self
            .refs
            .as_ref()
            .and_then(|refs| refs.submodules.as_ref().ok())
            .and_then(|modules| modules.iter().find(|module| module.path == row.value));
        let Some(module) = module else { return };
        if module.dirty {
            self.notice =
                "Commit or save the submodule's working changes before updating its checkout"
                    .into();
            return;
        }
        self.form(
            &format!(
                "Initialize / update {} to its index commit?",
                row.value.display()
            ),
            FormAction::Confirm(vec![
                "--literal-pathspecs".into(),
                "submodule".into(),
                "update".into(),
                "--init".into(),
                "--".into(),
                row.value.into_os_string(),
            ]),
            Vec::new(),
        );
    }

    pub(super) fn remove_sidebar_worktree(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(worktree) = self.refs.as_ref().and_then(|refs| {
            refs.worktrees
                .iter()
                .find(|worktree| worktree.path == row.value)
        }) else {
            return;
        };
        if worktree.main {
            self.notice = "The main worktree cannot be removed".into();
            return;
        }
        let result = self.branches.start(GitBranchRequest::ReviewRemoval {
            repository: self.root.clone(),
            name: worktree.name.clone(),
            deadline: GitBranchRequest::REMOVAL_STATUS_TIMEOUT,
        });
        match result {
            Ok(_) => self.show_removal_check(),
            Err(error) => self.notice = error.to_string(),
        }
    }

    pub(super) fn confirm_sidebar_worktree_remove(
        &mut self,
        name: String,
        reviewed: crate::GitWorktreeRemoval,
        change: Option<String>,
    ) {
        let (title, warning) = if reviewed.is_missing() {
            (
                format!("Forget missing worktree {name}?"),
                "Its directory is already gone. No branch is deleted.".into(),
            )
        } else if reviewed.is_clean() {
            (
                format!(
                    "Remove clean, unlocked worktree {}?",
                    reviewed.path().display()
                ),
                String::new(),
            )
        } else {
            (
                format!("Remove worktree {name}?"),
                reviewed.warnings().collect::<Vec<_>>().join("\n"),
            )
        };
        let changed = change.is_some();
        let warning = match change {
            Some(change) => format!("{change}\n{warning}"),
            None => warning,
        };
        self.form(
            &title,
            FormAction::RemoveWorktree {
                name,
                reviewed,
                warning,
                note_top: std::cell::Cell::new(0),
            },
            Vec::new(),
        );
        if changed && let Some(form) = &mut self.form {
            form.selected = form.fields.len() + 1;
        }
    }

    pub(super) fn fetch_sidebar_remote(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let selected = row.value.to_string_lossy();
        let name = self
            .remotes
            .iter()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| selected == *name || selected.starts_with(&format!("{name}/")))
            .max_by_key(|name| name.len())
            .map(str::to_owned);
        if let Some(name) = name {
            self.form(
                &format!("Fetch from {name}?"),
                FormAction::Confirm(vec!["fetch".into(), "--".into(), name.into()]),
                Vec::new(),
            );
        }
    }

    pub(super) fn reload_reflog(&mut self) -> Result<(), String> {
        if self.reflog.is_empty() {
            self.preview = "Loading reflog…".into();
        }
        self.reflog_error = None;
        let result = self.run(
            Request::Reflog,
            [
                "log",
                "-g",
                "-300",
                "--format=%H%x09%gD%x09%gs",
                "--date=raw",
                "HEAD",
                "--",
            ]
            .map(Into::into)
            .into(),
        );
        self.reflog_loading = result.is_ok();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflog_keeps_one_row_per_commit_undo_or_redo() {
        let one = "1".repeat(40);
        let two = "2".repeat(40);
        let three = "3".repeat(40);
        let redo = format!("{three}\tHEAD@{{30 +0000}}\t[dusty redo]: updating HEAD");
        let undo = format!("{two}\tHEAD@{{20 +0000}}\t[dusty undo]: updating HEAD");
        let separate = format!("{one}\tHEAD@{{10 +0000}}\tbranch: Reset to {one}");
        let log = format!(
            "{redo}\n{three}\tHEAD@{{30 +0000}}\tbranch: Reset to {three}\n\
             {undo}\n{two}\tHEAD@{{20 +0000}}\tbranch: Reset to {two}\n{separate}"
        );
        let keys = [redo, undo, separate].map(PathBuf::from);
        let rows = ReflogEntry::parse(&log);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().zip(keys).all(|(row, key)| row.key == key));
        assert_eq!(
            rows[0].label,
            "3333333 HEAD@{0} [dusty redo]: updating HEAD"
        );
        assert_eq!(
            rows[1].label,
            "2222222 HEAD@{2} [dusty undo]: updating HEAD"
        );
        assert!(
            rows[2]
                .label
                .starts_with("1111111 HEAD@{4} branch: Reset to ")
        );
    }

    #[test]
    fn reflog_hides_skipped_unstage_ids_without_changing_row_identity() {
        let id = "a".repeat(40);
        let subject = format!("[dusty undo] [skipped unstage:{id}]: updating HEAD");
        let entry = format!("{id}\tHEAD@{{0}}\t{subject}");
        let rows = ReflogEntry::parse(&entry);
        assert_eq!(
            rows[0].label,
            "aaaaaaa HEAD@{0} [dusty undo]: updating HEAD"
        );
        assert_eq!(rows[0].key, PathBuf::from(entry));
    }
}
