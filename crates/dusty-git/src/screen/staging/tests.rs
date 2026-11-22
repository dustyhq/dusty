use super::*;

const EDIT: &str = "changes/edit.txt";
const CONFLICT: &str = "changes/conflict.txt";

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .args([
            "-c",
            "user.name=Stage test",
            "-c",
            "user.email=stage@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap()
}

fn run(root: &Path, args: &[&str]) {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn merge_conflict(root: &Path, conflict: &str) {
    run(root, &["init", "-b", "main"]);
    std::fs::create_dir(root.join("changes")).unwrap();
    let file = root.join(conflict);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(root.join(EDIT), "base\n").unwrap();
    std::fs::write(&file, "base\n").unwrap();
    run(root, &["add", EDIT, conflict]);
    run(root, &["commit", "-m", "Base"]);
    run(root, &["switch", "-c", "topic"]);
    std::fs::write(&file, "topic\n").unwrap();
    run(root, &["commit", "-am", "Topic"]);
    run(root, &["switch", "main"]);
    std::fs::write(&file, "main\n").unwrap();
    run(root, &["commit", "-am", "Main"]);
    assert!(!git(root, &["merge", "--no-edit", "topic"]).status.success());
    assert!(index_id(root, conflict, 2).is_some());
}

fn index_id(root: &Path, path: &str, stage: i32) -> Option<git2::Oid> {
    let repo = git2::Repository::open(root).unwrap();
    repo.index()
        .unwrap()
        .get_path(Path::new(path), stage)
        .map(|entry| entry.id)
}

fn index_blob(root: &Path, path: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open(root).unwrap();
    let id = index_id(root, path, 0)?;
    Some(repo.find_blob(id).unwrap().content().to_vec())
}

fn wait(screen: &mut GitScreen) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while screen.busy || screen.refreshing || screen.refreshing_after_write {
        assert!(std::time::Instant::now() < deadline, "Git did not finish");
        screen.poll();
        std::thread::yield_now();
    }
    assert!(screen.snapshot.is_some(), "{}", screen.notice);
}

fn key(screen: &mut GitScreen, code: KeyCode, kind: KeyInputKind) {
    screen.event(UiEvent::Key {
        key: KeyEvent::plain(code),
        kind,
    });
}

fn press(screen: &mut GitScreen, code: KeyCode) {
    key(screen, code, KeyInputKind::Press);
}

fn press_enter(screen: &mut GitScreen, control: bool) {
    let mut key = KeyEvent::plain(KeyCode::Enter);
    key.modifiers.control = control;
    screen.event(UiEvent::Key {
        key,
        kind: KeyInputKind::Press,
    });
}

fn filter(screen: &mut GitScreen, text: &str) {
    press(screen, KeyCode::Char('/'));
    screen.event(UiEvent::Paste(text.into()));
    press(screen, KeyCode::Enter);
}

#[test]
fn bulk_toggles_use_the_nonconflicted_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    merge_conflict(root, CONFLICT);
    let conflict = index_id(root, CONFLICT, 2);
    std::fs::write(root.join(EDIT), "edited\n").unwrap();
    run(root, &["add", EDIT]);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    for (selection, marked) in [('a', false), ('-', false), ('-', true)] {
        if marked {
            screen.file_selection.marked = [PathBuf::from(EDIT), PathBuf::from(CONFLICT)].into();
        } else if selection == '-' {
            screen.changes_tree = true;
            screen.selected[0] = screen
                .rows(0)
                .iter()
                .position(|row| row.directory && row.value == Path::new("changes"))
                .unwrap();
        }
        press(&mut screen, KeyCode::Char(selection));
        wait(&mut screen);
        assert_eq!(
            index_blob(root, EDIT).as_deref(),
            Some(b"base\n".as_slice()),
            "{}",
            screen.notice
        );
        assert_eq!(index_id(root, CONFLICT, 2), conflict);

        press(&mut screen, KeyCode::Char(selection));
        wait(&mut screen);
        assert_eq!(
            index_blob(root, EDIT).as_deref(),
            Some(b"edited\n".as_slice()),
            "{}",
            screen.notice
        );
        assert_eq!(index_id(root, CONFLICT, 2), conflict);
    }
}

#[test]
fn bulk_unstage_restores_a_file_replaced_by_a_directory() {
    bulk_unstage_replacement("docs", "docs/intro.txt");
}

#[test]
fn bulk_unstage_restores_a_directory_replaced_by_a_file() {
    bulk_unstage_replacement("docs/intro.txt", "docs");
}

fn stage_replacement(root: &Path, original: &str, replacement: &str) {
    run(root, &["init", "-b", "main"]);
    if original != "docs" {
        std::fs::create_dir(root.join("docs")).unwrap();
    }
    std::fs::write(root.join(original), "before\n").unwrap();
    run(root, &["add", "--all"]);
    run(root, &["commit", "-m", "Before"]);
    std::fs::remove_file(root.join(original)).unwrap();
    if original == "docs" {
        std::fs::create_dir(root.join("docs")).unwrap();
    } else {
        std::fs::remove_dir(root.join("docs")).unwrap();
    }
    std::fs::write(root.join(replacement), "replacement\n").unwrap();
    run(root, &["add", "--all"]);
}

fn bulk_unstage_replacement(original: &str, replacement: &str) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    stage_replacement(root, original, replacement);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    press(&mut screen, KeyCode::Char('a'));
    assert!(
        screen.busy,
        "bulk Unstage must run on the worker: {}",
        screen.notice
    );
    wait(&mut screen);
    assert_eq!(
        index_blob(root, original).as_deref(),
        Some(b"before\n".as_slice()),
        "{}",
        screen.notice
    );
    assert!(index_blob(root, replacement).is_none());
    assert_eq!(
        std::fs::read_to_string(root.join(replacement)).unwrap(),
        "replacement\n"
    );

    crate::history::undo(root, false)
        .unwrap()
        .restore_index(root)
        .unwrap();
    assert!(index_blob(root, original).is_none());
    assert_eq!(
        index_blob(root, replacement).as_deref(),
        Some(b"replacement\n".as_slice())
    );
}

#[test]
fn marked_unstage_does_not_include_unmarked_replacement_paths() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    stage_replacement(root, "docs", "docs/intro.txt");
    std::fs::write(root.join("docs/other.txt"), "unmarked child\n").unwrap();
    std::fs::write(root.join("docs-other.txt"), "unmarked neighbour\n").unwrap();
    run(root, &["add", "--all"]);
    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    let before = std::fs::read(root.join(".git/index")).unwrap();
    screen.file_selection.marked = ["docs", "docs/intro.txt"].map(PathBuf::from).into();
    press(&mut screen, KeyCode::Char('u'));
    wait(&mut screen);
    assert_eq!(
        screen.notice,
        "Select both the file and the changed files in the directory, then try again."
    );
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), before);

    screen.file_selection.marked = ["docs", "docs/intro.txt", "docs/other.txt"]
        .map(PathBuf::from)
        .into();
    press(&mut screen, KeyCode::Char('u'));
    wait(&mut screen);
    assert_eq!(
        index_blob(root, "docs").as_deref(),
        Some(b"before\n".as_slice())
    );
    assert!(index_blob(root, "docs/intro.txt").is_none());
    assert!(index_blob(root, "docs/other.txt").is_none());
    assert_eq!(
        index_blob(root, "docs-other.txt").as_deref(),
        Some(b"unmarked neighbour\n".as_slice())
    );
}

fn replace_config_with_directory(root: &Path) {
    std::fs::remove_file(root.join("config")).unwrap();
    std::fs::create_dir(root.join("config")).unwrap();
    std::fs::write(root.join("config/app.toml"), "new config\n").unwrap();
    std::fs::write(root.join("config/other.toml"), "other config\n").unwrap();
}

fn tracked_config_replacement(root: &Path) {
    run(root, &["init", "-b", "main"]);
    std::fs::write(root.join("config"), "old config\n").unwrap();
    run(root, &["add", "config"]);
    run(root, &["commit", "-m", "Base"]);
    replace_config_with_directory(root);
}

#[test]
fn stage_all_replaces_a_tracked_file_only_with_in_scope_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    tracked_config_replacement(root);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    screen.file_selection.marked =
        [PathBuf::from("config"), PathBuf::from("config/app.toml")].into();
    press(&mut screen, KeyCode::Char('s'));
    assert!(!screen.busy);
    assert_eq!(screen.notice, MARKED_DIRECTORY_SCOPE);

    filter(&mut screen, "app.toml");
    press(&mut screen, KeyCode::Char('a'));
    assert!(!screen.busy);
    assert_eq!(screen.notice, DIRECTORY_SCOPE);
    assert!(index_id(root, "config/app.toml", 0).is_none());
    assert!(index_id(root, "config/other.toml", 0).is_none());
    assert_eq!(
        index_blob(root, "config").as_deref(),
        Some(b"old config\n".as_slice())
    );

    press(&mut screen, KeyCode::Escape);
    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    assert_config_replacement_staged(root, &screen.notice);
}

fn assert_config_replacement_staged(root: &Path, notice: &str) {
    assert!(index_id(root, "config", 0).is_none(), "{notice}");
    assert_eq!(
        index_blob(root, "config/app.toml").as_deref(),
        Some(b"new config\n".as_slice()),
        "{notice}"
    );
    assert_eq!(
        index_blob(root, "config/other.toml").as_deref(),
        Some(b"other config\n".as_slice()),
        "{notice}"
    );
}

#[test]
fn filtered_stage_all_replaces_config_when_all_its_paths_are_shown() {
    for tree in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        tracked_config_replacement(root);
        std::fs::write(root.join("hidden.txt"), "not in the filter\n").unwrap();

        let mut screen = GitScreen::open(root).unwrap();
        wait(&mut screen);
        screen.changes_tree = tree;
        filter(&mut screen, "config");
        let shown: BTreeSet<_> = screen
            .rows(0)
            .iter()
            .filter(|row| !row.directory)
            .map(|row| row.value.clone())
            .collect();
        let expected = ["config", "config/app.toml", "config/other.toml"].map(PathBuf::from);
        assert_eq!(shown, BTreeSet::from(expected), "tree: {tree}");

        press(&mut screen, KeyCode::Char('a'));
        wait(&mut screen);

        assert_config_replacement_staged(root, &screen.notice);
        assert!(index_id(root, "hidden.txt", 0).is_none());
    }
}

fn staged_config_replacement(root: &Path) {
    run(root, &["init", "-b", "main"]);
    run(root, &["commit", "--allow-empty", "-m", "Base"]);
    std::fs::write(root.join("config"), "staged only\n").unwrap();
    run(root, &["add", "config"]);
    replace_config_with_directory(root);
}

#[test]
fn staging_one_replacement_child_keeps_the_staged_parent() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    staged_config_replacement(root);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    screen.changes_tree = true;
    screen.selected[0] = screen
        .rows(0)
        .iter()
        .position(|row| !row.directory && row.value == Path::new("config/app.toml"))
        .unwrap();
    press(&mut screen, KeyCode::Char('s'));
    wait(&mut screen);

    assert_eq!(
        index_blob(root, "config").as_deref(),
        Some(b"staged only\n".as_slice()),
        "{}",
        screen.notice
    );
    assert!(index_id(root, "config/app.toml", 0).is_none());
    assert_eq!(screen.notice, MARKED_DIRECTORY_SCOPE);
}

#[test]
fn staging_one_new_file_keeps_its_tracked_sibling() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    run(root, &["init", "-b", "main"]);
    std::fs::create_dir(root.join("config")).unwrap();
    std::fs::write(root.join("config/defaults.toml"), "tracked defaults\n").unwrap();
    run(root, &["add", "config/defaults.toml"]);
    run(root, &["commit", "-m", "Base"]);
    std::fs::write(root.join("config/app.toml"), "new config\n").unwrap();

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    screen.changes_tree = true;
    screen.selected[0] = screen
        .rows(0)
        .iter()
        .position(|row| !row.directory && row.value == Path::new("config/app.toml"))
        .unwrap();
    press(&mut screen, KeyCode::Char('s'));
    wait(&mut screen);

    assert_eq!(
        index_blob(root, "config/app.toml").as_deref(),
        Some(b"new config\n".as_slice()),
        "{}",
        screen.notice
    );
    assert_eq!(
        index_blob(root, "config/defaults.toml").as_deref(),
        Some(b"tracked defaults\n".as_slice())
    );
}

#[test]
fn stage_all_confirms_before_a_directory_replaces_a_staged_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    staged_config_replacement(root);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    for control in [false, true] {
        press(&mut screen, KeyCode::Char('a'));
        wait(&mut screen);
        let form = screen.form.as_ref().unwrap();
        assert_eq!(form.title, "Replace staged changes?");
        assert!(form.cancel_focused());

        press_enter(&mut screen, control);
        assert!(screen.form.is_none());
        assert!(!screen.busy);
        assert_eq!(
            index_blob(root, "config").as_deref(),
            Some(b"staged only\n".as_slice()),
            "Ctrl: {control}"
        );
        assert!(index_id(root, "config/app.toml", 0).is_none());
    }

    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Enter);
    wait(&mut screen);
    assert_config_replacement_staged(root, &screen.notice);
}

#[test]
fn bulk_stage_reviews_a_lost_staged_edit_and_refuses_a_changed_candidate() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let name = "edit.txt";
    let file = root.join(name);
    run(root, &["init", "-b", "main"]);
    std::fs::write(&file, "top=0\nbottom=0\n").unwrap();
    run(root, &["add", name]);
    run(root, &["commit", "-m", "Base"]);
    let staged = b"top=1\nbottom=0\n";
    std::fs::write(&file, staged).unwrap();
    run(root, &["add", name]);
    std::fs::write(&file, "top=0\nbottom=1\n").unwrap();

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    assert!(matches!(
        screen.form.as_ref().map(|form| &form.action),
        Some(FormAction::StageAnyway(_))
    ));
    assert_eq!(index_blob(root, name).as_deref(), Some(staged.as_slice()));

    let changed = b"top=0\nbottom=2\n";
    std::fs::write(&file, changed).unwrap();
    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Enter);
    wait(&mut screen);
    assert_eq!(screen.notice, STAGE_CHANGED);
    assert_eq!(index_blob(root, name).as_deref(), Some(staged.as_slice()));

    screen.refresh().unwrap();
    wait(&mut screen);
    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    assert!(screen.form.as_ref().is_some_and(Form::cancel_focused));
    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Enter);
    wait(&mut screen);
    assert_eq!(index_blob(root, name).as_deref(), Some(changed.as_slice()));
}

#[test]
fn bulk_stage_asks_when_a_staged_deletion_reappears_away_from_its_gap() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let file = root.join("edit.txt");
    run(root, &["init", "-b", "main"]);
    std::fs::write(&file, "A\nB\n").unwrap();
    run(root, &["add", "edit.txt"]);
    run(root, &["commit", "-m", "Base"]);
    std::fs::write(&file, "B\n").unwrap();
    run(root, &["add", "edit.txt"]);
    std::fs::write(&file, "B\nA\nB\n").unwrap();

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    assert!(matches!(
        screen.form.as_ref().map(|form| &form.action),
        Some(FormAction::StageAnyway(_))
    ));
    assert_eq!(
        index_blob(root, "edit.txt").as_deref(),
        Some(b"B\n".as_slice())
    );

    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Enter);
    wait(&mut screen);
    assert_eq!(
        index_blob(root, "edit.txt").as_deref(),
        Some(b"B\nA\nB\n".as_slice())
    );
}

#[cfg(unix)]
#[test]
fn bulk_stage_notifies_post_index_change_only_after_publishing() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let file = root.join("edit.txt");
    run(root, &["init", "-b", "main"]);
    std::fs::write(&file, "base\n").unwrap();
    run(root, &["add", "edit.txt"]);
    run(root, &["commit", "-m", "Base"]);
    std::fs::write(&file, "reviewed in the index\n").unwrap();
    run(root, &["add", "edit.txt"]);
    std::fs::write(&file, "new worktree contents\n").unwrap();

    let hooks = root.join(".git/test-hooks");
    std::fs::create_dir(&hooks).unwrap();
    run(root, &["config", "core.hooksPath", ".git/test-hooks"]);
    let hook = hooks.join("post-index-change");
    std::fs::write(
        &hook,
        r#"#!/bin/sh
if test -e .git/index.lock; then
    echo locked >> .git/hook-events
fi
git show :edit.txt >> .git/hook-events || exit 1
printf '%s,%s\n' "$1" "$2" >> .git/hook-events
"#,
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let events = root.join(".git/hook-events");

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    assert!(screen.form.as_ref().is_some_and(Form::cancel_focused));
    assert!(!events.exists(), "preparation must not call the hook");
    press(&mut screen, KeyCode::Enter);
    assert!(screen.form.is_none());
    assert!(!events.exists(), "cancellation must not call the hook");

    press(&mut screen, KeyCode::Char('a'));
    wait(&mut screen);
    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Enter);
    wait(&mut screen);
    assert_eq!(
        std::fs::read_to_string(events).unwrap(),
        "new worktree contents\n0,0\n",
        "one hook call must see the published, unlocked index"
    );
}

#[test]
fn marked_discard_leaves_workfiles_alone_during_a_competing_index_write() {
    const TRACKED: &str = "z-tracked.txt";
    const UNTRACKED: &str = "a-untracked.txt";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    run(root, &["init", "-b", "main"]);
    std::fs::write(root.join(TRACKED), "base\n").unwrap();
    run(root, &["add", TRACKED]);
    run(root, &["commit", "-m", "Base"]);
    std::fs::write(root.join(TRACKED), "modified\n").unwrap();
    std::fs::write(root.join(UNTRACKED), "untracked\n").unwrap();

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    screen.file_selection.marked = [TRACKED, UNTRACKED].map(PathBuf::from).into();
    press(&mut screen, KeyCode::Char('D'));
    assert!(matches!(
        screen.form.as_ref().map(|form| &form.action),
        Some(FormAction::DiscardFiles(_))
    ));

    // A writer may be about to publish unmerged stages for the untracked file.
    // Its ownership of the lock must keep discard from touching either path.
    let lock = root.join(".git/index.lock");
    std::fs::write(&lock, "other writer's lock").unwrap();
    press(&mut screen, KeyCode::Enter);
    assert_eq!(screen.notice, DISCARD_LOCKED);
    assert_eq!(std::fs::read(root.join(TRACKED)).unwrap(), b"modified\n");
    assert_eq!(std::fs::read(root.join(UNTRACKED)).unwrap(), b"untracked\n");
    assert_eq!(std::fs::read(lock).unwrap(), b"other writer's lock");
}

#[test]
fn whole_file_discard_refuses_a_live_conflict_in_the_file_or_rename_source() {
    const SOURCE: &str = "old.txt";
    const TARGET: &str = "new.txt";
    const UNTRACKED: &str = "untracked.txt";
    for conflict in [TARGET, SOURCE] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        run(root, &["init", "-b", "main"]);
        std::fs::write(root.join(SOURCE), "base\n").unwrap();
        run(root, &["add", SOURCE]);
        run(root, &["commit", "-m", "Base"]);
        run(root, &["mv", SOURCE, TARGET]);
        std::fs::write(root.join(TARGET), "unstaged edit\n").unwrap();
        std::fs::write(root.join(UNTRACKED), "untracked\n").unwrap();

        let mut screen = GitScreen::open(root).unwrap();
        wait(&mut screen);
        screen.selected[0] = screen
            .rows(0)
            .iter()
            .position(|row| row.value == Path::new(TARGET))
            .unwrap();
        screen.file_selection.marked = [TARGET, UNTRACKED].map(PathBuf::from).into();
        press(&mut screen, KeyCode::Char('D'));
        assert!(matches!(
            screen.form.as_ref().map(|form| &form.action),
            Some(FormAction::DiscardFiles(_))
        ));

        // Change only the index; the workfile being discarded still matches.
        // The rename source already had no stage-zero entry at review time.
        let repo = git2::Repository::open(root).unwrap();
        let mut index = repo.index().unwrap();
        let target = index.get_path(Path::new(TARGET), 0).unwrap();
        let mut entry = index.get_path(Path::new(TARGET), 0).unwrap();
        if conflict == TARGET {
            index.remove(Path::new(TARGET), 0).unwrap();
        }
        entry.path = conflict.as_bytes().to_vec();
        entry.flags = (entry.flags & !(3 << 12)) | (2 << 12);
        index.add(&entry).unwrap();
        index.write().unwrap();
        assert!(index_id(root, conflict, 2).is_some());

        press(&mut screen, KeyCode::Enter);
        let notice = format!("Resolve the conflict in {conflict} before discarding.");
        assert_eq!(screen.notice, notice);
        assert!(screen.form.is_none());
        assert_eq!(
            std::fs::read(root.join(TARGET)).unwrap(),
            b"unstaged edit\n"
        );
        assert!(root.join(UNTRACKED).is_file());

        press(&mut screen, KeyCode::Char('D'));
        assert_eq!(screen.notice, notice);
        assert!(screen.form.is_none());
        screen.clear_file_selection();
        press(&mut screen, KeyCode::Char('d'));
        assert_eq!(screen.notice, notice);
        assert!(screen.form.is_none());

        // Once there is no conflict, tracked restores and untracked deletes still work.
        index.remove(Path::new(conflict), 2).unwrap();
        if conflict == TARGET {
            index.add(&target).unwrap();
        }
        index.write().unwrap();
        screen.file_selection.marked = [TARGET, UNTRACKED].map(PathBuf::from).into();
        press(&mut screen, KeyCode::Char('D'));
        press(&mut screen, KeyCode::Enter);
        assert_eq!(std::fs::read(root.join(TARGET)).unwrap(), b"base\n");
        assert!(!root.join(UNTRACKED).exists());
    }
}

#[test]
fn stage_all_leaves_a_capped_status_index_byte_identical() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    run(root, &["init", "-b", "main"]);
    let file = root.join("edit.txt");
    std::fs::write(&file, "base\n").unwrap();
    run(root, &["add", "edit.txt"]);
    run(root, &["commit", "-m", "Base"]);
    std::fs::write(&file, "edited\n").unwrap();

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    screen.snapshot.as_mut().unwrap().truncated = true;
    let index = root.join(".git/index");
    let before = std::fs::read(&index).unwrap();

    for query in [None, Some("edit.txt")] {
        if let Some(query) = query {
            filter(&mut screen, query);
        }
        press(&mut screen, KeyCode::Char('a'));
        assert!(!screen.busy);
        assert_eq!(
            screen.notice,
            "Too many changed files to stage them all here. Use git add in a terminal."
        );
        assert_eq!(std::fs::read(&index).unwrap(), before);
    }
}

#[test]
fn staging_conflict_markers_requires_a_deliberate_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    merge_conflict(root, CONFLICT);
    let marker_bytes = std::fs::read(root.join(CONFLICT)).unwrap();
    let conflict = index_id(root, CONFLICT, 2);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    let stage = KeyCode::Char('s');
    press(&mut screen, stage.clone());
    let form = screen.form.as_ref().unwrap();
    assert_eq!(
        form.title,
        "Conflict markers remain in changes/conflict.txt."
    );
    assert!(form.cancel_focused());

    key(&mut screen, stage.clone(), KeyInputKind::Repeat);
    press(&mut screen, stage.clone()); // Some terminals report repeats as presses.
    for control in [false, true] {
        assert!(screen.form.as_ref().unwrap().cancel_focused());
        press_enter(&mut screen, control);
        assert!(screen.form.is_none());
        assert!(!screen.busy);
        assert_eq!(index_id(root, CONFLICT, 2), conflict, "Ctrl: {control}");
        press(&mut screen, stage.clone());
    }

    press(&mut screen, KeyCode::Tab);
    key(&mut screen, KeyCode::Enter, KeyInputKind::Repeat);
    assert!(screen.form.is_some());
    assert!(!screen.busy);
    assert_eq!(index_id(root, CONFLICT, 2), conflict);
    press(&mut screen, KeyCode::Enter);
    wait(&mut screen);
    assert!(screen.form.is_none());
    assert!(index_id(root, CONFLICT, 2).is_none(), "{}", screen.notice);
    assert_eq!(index_blob(root, CONFLICT), Some(marker_bytes));
}

#[test]
fn staging_conflict_sheet_wraps_the_full_path_and_selects_cancel() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = "src/features/billing/components/checkout/ConfirmPayment.tsx";
    merge_conflict(root, path);

    let mut screen = GitScreen::open(root).unwrap();
    wait(&mut screen);
    press(&mut screen, KeyCode::Char('s'));
    let frame = screen.frame(100, 30);
    let rows: Vec<_> = (0..frame.grid.height())
        .map(|row| frame.grid.row_text(row))
        .collect();
    let title = rows
        .iter()
        .position(|row| row.contains("Conflict markers remain"))
        .expect("the warning title is visible");
    assert!(!rows[title].contains("Conflict markers remain in"));

    let first = &rows[title + 2];
    let start = first
        .find("src/features/")
        .expect("the path starts in the body");
    let head = first[start..].split_whitespace().next().unwrap();
    let tail = path.strip_prefix(head).expect("the first path row matches");
    assert!(!tail.is_empty(), "the path must wrap at this size");
    assert!(rows[title + 3].contains(tail), "{rows:?}");
    assert!(
        rows.iter()
            .any(|row| row.contains("[ Stage anyway ]") && row.contains("›[ Cancel ]")),
        "{rows:?}"
    );
}
