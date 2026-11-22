//! The on-disk forge cache and painting from it (F3).
#![cfg(unix)]
use dusty::editor::input::KeyCode;
use dusty::forge::cache::{Cache, CacheKey, Caps, Envelope};
use dusty::forge::request::{Payload, Read};
use dusty::forge::transport::{AccountNamespace, HostKey};
use std::time::{Duration, Instant};

#[path = "support/forge_fake.rs"]
mod forge_fake;
#[path = "support/forge_screen.rs"]
mod forge_screen;
#[path = "support/forge_world.rs"]
mod forge_world;
#[path = "support/records.rs"]
mod records;
use forge_screen::*;
use records::lists;

fn cache_key(read: &Read) -> CacheKey {
    let account = AccountNamespace::anonymous(HostKey::new("github.com"));
    CacheKey::for_read(&account, Some("example/demo"), read)
}

#[test]
fn entries_round_trip_and_a_corrupt_file_is_a_miss_never_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(dir.path());
    let labels = cache_key(&Read::Labels);
    assert!(cache.store(
        &labels,
        Payload::Text("bug".into()),
        Duration::from_secs(60),
        Some("\"e1\"".into()),
        None
    ));
    let envelope: Envelope<Payload> = cache.get(&labels).unwrap();
    assert_eq!(envelope.data, Payload::Text("bug".into()));
    assert_eq!(envelope.etag.as_deref(), Some("\"e1\""));
    assert!(envelope.fresh());
    let file = labels.file(dir.path());
    assert!(
        file.starts_with(
            dir.path()
                .join("github.com/_accounts/anonymous/example/demo/labels")
        )
    );
    // No temporary is left behind by the atomic write.
    let siblings = std::fs::read_dir(file.parent().unwrap()).unwrap().count();
    assert_eq!(siblings, 1);

    for garbage in [&b"{ not json"[..], b"", b"\xff\xfe\x00", b"{\"schema\":1}"] {
        std::fs::write(&file, garbage).unwrap();
        assert!(cache.get::<Payload>(&labels).is_none());
        assert!(!file.exists(), "a corrupt entry is unlinked");
        cache.store(
            &labels,
            Payload::Text("bug".into()),
            Duration::from_secs(60),
            None,
            None,
        );
    }
    assert_eq!(cache.stats().corrupt, 4);
    // A newer schema (or another key hashed to the same name) is a miss too.
    let mut stale: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    stale["schema"] = 999.into();
    std::fs::write(&file, stale.to_string()).unwrap();
    assert!(cache.get::<Payload>(&labels).is_none());

    // A broken index is rebuilt by scanning the entries that are there.
    cache.store(
        &labels,
        Payload::Text("bug".into()),
        Duration::from_secs(60),
        None,
        None,
    );
    let directory = labels.directory(dir.path());
    std::fs::write(directory.join("index.json"), "garbage").unwrap();
    let index = cache.index(&directory);
    assert_eq!(index.entries.len(), 1);
    assert!(index.total_bytes > 0);
}

#[test]
fn touch_renews_and_invalidate_expires_without_touching_data() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(dir.path());
    let repo = cache_key(&Read::Repository);
    cache.store(
        &repo,
        Payload::Text("demo".into()),
        Duration::from_secs(3600),
        Some("\"v1\"".into()),
        None,
    );
    let mut old: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(repo.file(dir.path())).unwrap()).unwrap();
    old["fetched_at"] = (old["fetched_at"].as_u64().unwrap() - 7_200_000).into();
    std::fs::write(repo.file(dir.path()), old.to_string()).unwrap();
    assert!(!cache.get::<Payload>(&repo).unwrap().fresh());
    assert!(cache.touch(&repo));
    let renewed = cache.get::<Payload>(&repo).unwrap();
    assert!(renewed.fresh());
    assert_eq!(renewed.data, Payload::Text("demo".into()));
    assert_eq!(renewed.etag.as_deref(), Some("\"v1\""));
    assert!(cache.invalidate(&repo));
    let expired = cache.get::<Payload>(&repo).unwrap();
    assert!(!expired.fresh());
    assert_eq!(expired.data, Payload::Text("demo".into()));
}

#[test]
fn caps_evict_least_recently_used_but_never_the_repository_or_large_logs() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(dir.path()).with_caps(Caps {
        repo: 40 * 1024,
        host: 1024 * 1024,
    });
    let big = "x".repeat(10 * 1024);
    let repo = cache_key(&Read::Repository);
    cache.store(
        &repo,
        Payload::Text(big.clone()),
        Duration::from_secs(60),
        None,
        None,
    );
    for number in 1..=8 {
        cache.store(
            &cache_key(&Read::Issue(number)),
            Payload::Text(big.clone()),
            Duration::from_secs(60),
            None,
            None,
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        cache.get::<Payload>(&repo).is_some(),
        "the repository is never evicted"
    );
    assert!(
        cache.get::<Payload>(&cache_key(&Read::Issue(8))).is_some(),
        "the newest entry stays"
    );
    assert!(
        cache.get::<Payload>(&cache_key(&Read::Issue(1))).is_none(),
        "the oldest entry goes"
    );
    let index = cache.index(&repo.directory(dir.path()));
    assert!(index.total_bytes <= 40 * 1024, "{}", index.total_bytes);
    assert!(cache.stats().evicted > 0);

    let log = cache_key(&Read::JobLog(7u64.into()));
    let huge = "y".repeat(9 * 1024 * 1024);
    assert!(!cache.store(
        &log,
        Payload::Text(huge),
        Duration::from_secs(60),
        None,
        None
    ));
    assert!(cache.get::<Payload>(&log).is_none());
}

#[test]
fn host_wide_reads_share_one_directory_per_host() {
    let account = AccountNamespace::anonymous(HostKey::new("GitHub.COM"));
    let search = CacheKey::new(&account, None, &Read::Viewer);
    assert!(
        search
            .file(std::path::Path::new("/c"))
            .starts_with("/c/github.com/_accounts/anonymous/_host/viewer")
    );
}

#[test]
fn first_paint_comes_from_disk_while_the_forge_is_slow() {
    if fixture_child(
        "first_paint_comes_from_disk_while_the_forge_is_slow",
        serde_json::json!({"calls":[],"comments":["Cached fixture ready"],"issues":navigation_issues()}),
    ) {
        return;
    }
    {
        let mut screen = open();
        goto(&mut screen, 0, 1);
        wait(&mut screen, "Cached fixture ready");
        settle(&mut screen, Duration::from_millis(400));
    }
    assert!(
        entries(&cache_dir()).len() >= 3,
        "the first session wrote the repository, the list and the item"
    );
    // The last session was an hour ago, so everything shown is also checked again. The forge
    // holds that check of the list until released, so whatever the list shows meanwhile came
    // from disk: no timing decides it.
    age_cache(3600);
    update_state(|state| {
        state["read_gates"] =
            serde_json::json!([{"prefix": ["issue", "list"], "name": "slow-list"}]);
    });
    let started = Instant::now();
    let mut screen = open();
    let opened = started.elapsed();
    // The Git tab is on screen first, as in a real start; measure from the tab switch.
    let _ = view(&mut screen);
    let at = Instant::now();
    goto(&mut screen, 0, 1);
    wait_for(&mut screen, "cached rows", |frame| {
        lists(frame, 8, "Navigation issue 8") && lists(frame, 1, "Navigation issue 1")
    });
    let first_paint = at.elapsed();
    let frame = view(&mut screen);
    assert!(
        frame.contains("cached") || frame.contains("updated"),
        "{frame}"
    );
    assert!(
        !frame.contains("updated just now"),
        "the held list check cannot have finished:\n{frame}"
    );
    println!(
        "measured: first paint from cache {first_paint:?} (screen open {opened:?}, list held)"
    );
    // The background check finishes later and changes nothing visible.
    let before = view(&mut screen)
        .lines()
        .filter(|line| {
            // A record's title row: `│   ○ Navigation issue 3 …`, or `│▎  ○ …` under the cursor.
            line.strip_prefix('│')
                .map(|rest| rest.trim_start_matches([' ', '▸', '▎']))
                .is_some_and(|rest| rest.starts_with("○ Navigation issue"))
        })
        // The list's half: the reader beside it fills in from its own reads meanwhile.
        .map(|line| line.split("││").next().unwrap_or(line).to_owned())
        .collect::<Vec<_>>();
    release("slow-list");
    wait(&mut screen, "updated just now");
    let after = view(&mut screen)
        .lines()
        .filter(|line| {
            // A record's title row: `│   ○ Navigation issue 3 …`, or `│▎  ○ …` under the cursor.
            line.strip_prefix('│')
                .map(|rest| rest.trim_start_matches([' ', '▸', '▎']))
                .is_some_and(|rest| rest.starts_with("○ Navigation issue"))
        })
        // The list's half: the reader beside it fills in from its own reads meanwhile.
        .map(|line| line.split("││").next().unwrap_or(line).to_owned())
        .collect::<Vec<_>>();
    // Every cached record's title row: the seven navigation issues, the cursor's included.
    assert_eq!(before.len(), 7, "{before:#?}");
    assert_eq!(before, after);
}

fn recheck_account(screen: &mut dusty::git::screen::GitScreen) {
    key(screen, KeyCode::Char('@'));
    key(screen, KeyCode::Char('r'));
}

#[test]
fn account_recheck_keeps_issue_rows_when_the_network_is_down() {
    if fixture_child(
        "account_recheck_keeps_issue_rows_when_the_network_is_down",
        serde_json::json!({"issues": navigation_issues()}),
    ) {
        return;
    }
    let mut screen = open();
    goto(&mut screen, 0, 1);
    wait_for(&mut screen, "live issue list", |frame| {
        lists(frame, 8, "Navigation issue 8")
    });
    settle(&mut screen, Duration::from_millis(400));
    update_state(|state| state["offline"] = true.into());
    recheck_account(&mut screen);
    assert!(
        lists(&view(&mut screen), 8, "Navigation issue 8"),
        "recheck must not blank the list"
    );
    wait_for(&mut screen, "offline notice with the old rows", |frame| {
        state()["offline_refused"].as_u64().unwrap_or(0) > 0
            && lists(frame, 8, "Navigation issue 8")
            && frame.to_ascii_lowercase().contains("offline")
    });
    settle(&mut screen, Duration::from_millis(400));
    update_state(|state| {
        state["offline"] = false.into();
        for issue in state["issues"].as_array_mut().unwrap() {
            issue["title"] = format!("Reconnected issue {}", issue["number"]).into();
        }
    });
    recheck_account(&mut screen);
    wait_for(
        &mut screen,
        "live rows without the old offline notice",
        |frame| {
            lists(frame, 8, "Reconnected issue 8")
                && !frame.to_ascii_lowercase().contains("offline")
        },
    );
}

#[test]
fn limited_gitea_token_can_browse_an_uncached_repo_with_a_remembered_login() {
    use dusty::forge::{ForgeTarget, connect, detect, host_state};
    use forge_world::{Forge, World};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    const TEST: &str = "limited_gitea_token_can_browse_an_uncached_repo_with_a_remembered_login";
    const PHASE: &str = "DUSTY_FORGE_LIMITED_TEST_PHASE";
    let forge = Forge::Gitea;
    let host = HostKey::new(forge.host());
    if let Ok(phase) = std::env::var(PHASE) {
        let account = || host_state::remembered_account(&host).unwrap().0;
        if phase == "verified" {
            // Verify a login without filling any response-cache entries.
            let root = std::path::PathBuf::from(std::env::var_os("DUSTY_GITHUB_ROOT").unwrap());
            let (_, ctx) = connect(&root, &ForgeTarget::remote(), &AtomicBool::new(false)).unwrap();
            assert_eq!(
                ctx.account().and_then(AccountNamespace::login),
                Some("alice")
            );
            assert_eq!(account().login(), Some("alice"));
            assert_eq!(
                detect::hosts_path(&dusty::forge::credential::Env::process()).unwrap(),
                root.parent()
                    .unwrap()
                    .join("platform-state/forge/hosts.json")
            );
        } else {
            assert_eq!(account().login(), Some("alice"));
            let mut screen = open();
            screen.open_target("issues", None);
            wait(&mut screen, "Limited token issue");
            settle(&mut screen, Duration::from_millis(400));
            assert!(
                entries(&cache_dir()).is_empty(),
                "unverified responses reached disk"
            );
            assert_eq!(account().login(), Some("alice"));
        }
        return;
    }

    let world = World::with_state(
        forge,
        json!({
            "fail": [{"path": "user", "status": 200, "body": {"login": "alice"}, "times": 100}],
            "answers": {
                "GET repos/orbit/orbit/issues": {"body": [{
                    "id": 1393, "number": 1393, "title": "Limited token issue", "state": "open",
                    "html_url": "https://gitea.com/orbit/orbit/issues/1393",
                    "created_at": "2026-11-08T10:00:00Z", "updated_at": "2026-11-08T10:00:00Z"
                }]}
            }
        }),
    );
    let run = |phase: &str| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        world.configure(&mut command);
        let output = command
            .args(["--exact", TEST, "--nocapture"])
            .env(PHASE, phase)
            .env("DUSTY_GITHUB_ROOT", &world.repo)
            .env("DUSTY_GITHUB_FIXTURE", world.state_path())
            .env(
                "DUSTY_STATE_HOME",
                world.directory.path().join("platform-state"),
            )
            .env_remove("DUSTY_FORGE_STATE_DIR")
            .env_remove("DUSTY_FORGE_API_BASE")
            .env_remove("XDG_STATE_HOME")
            .env_remove("HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{phase}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    };
    run("verified");
    let before = world.requests().len();
    world.update(|state| {
        state["fail"] = json!([{
            "path": "user", "status": 403, "message": "token does not have read:user", "times": 100
        }]);
    });
    run("limited");
    let requests = world.requests();
    for path in ["user", "repos/orbit/orbit", "repos/orbit/orbit/issues"] {
        assert!(
            requests[before..]
                .iter()
                .any(|req| req["path"] == path && req["auth"] == "valid")
        );
    }
}

#[test]
fn first_live_login_claims_legacy_marks_and_drafts_but_the_second_cannot_see_them() {
    use dusty::forge::host_state::{LegacyAccess, legacy_access, remembered_account};
    use serde_json::json;

    const TEST: &str =
        "first_live_login_claims_legacy_marks_and_drafts_but_the_second_cannot_see_them";
    const THREAD: &str = "Legacy marked thread";
    const DRAFT: &str = "Legacy private inline draft";
    if fixture_child(
        TEST,
        json!({
            "fail": [{"path": "user", "status": 200, "body": {"login": "alice"}, "times": 100}],
            "repository": "Example/Demo",
            "notifications": [{
                "id": "123", "unread": true, "reason": "mention", "updated_at": "2026-11-09T10:00:00Z",
                "repository": {"full_name": "Example/Demo"},
                "subject": {"title": THREAD, "type": "PullRequest",
                            "url": "https://api.github.com/repos/Example/Demo/pulls/9"}
            }]
        }),
    ) {
        return;
    }

    let root = std::path::PathBuf::from(std::env::var_os("DUSTY_GITHUB_ROOT").unwrap());
    let repo = git2::Repository::open(root).unwrap();
    let mut config = repo
        .config()
        .unwrap()
        .open_level(git2::ConfigLevel::Local)
        .unwrap();
    for key in ["dusty-git.notifySaved", "dusty-git.notifyDone"] {
        config.set_str(key, "github.com/123").unwrap();
    }
    let legacy = std::path::PathBuf::from(std::env::var_os("DUSTY_FORGE_STATE_DIR").unwrap())
        .join("drafts/github.com/example%2Fdemo/9.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, json!({
        "schema": 1,
        "drafts": [{
            "path": "example.rs", "line": 1, "side": "RIGHT", "body": DRAFT, "head": "a".repeat(40)
        }]
    }).to_string()).unwrap();

    let host = HostKey::new("github.com");
    assert_eq!(legacy_access(&host, None), LegacyAccess::Unclaimed);
    let mut screen = open();
    screen.open_target("prs", Some(9));
    wait(&mut screen, "Native review");
    let alice = remembered_account(&host).unwrap().0;
    assert_eq!(alice.login(), Some("alice"));
    assert_eq!(legacy_access(&host, Some(&alice)), LegacyAccess::Owner);

    key(&mut screen, KeyCode::Char('r'));
    wait(&mut screen, DRAFT);
    key(&mut screen, KeyCode::Escape);

    let marks_view = |screen: &mut dusty::git::screen::GitScreen, offset| {
        key(screen, KeyCode::Tab);
        key(screen, KeyCode::Home);
        for _ in 0..offset {
            key(screen, KeyCode::Down);
        }
        key(screen, KeyCode::Enter);
    };
    screen.open_target("notifications", None);
    wait(&mut screen, "Saved");
    for offset in [1, 2] {
        marks_view(&mut screen, offset);
        wait(&mut screen, THREAD);
    }
    settle(&mut screen, Duration::from_millis(400));
    update_state(|state| {
        state["fail"] =
            json!([{"path": "user", "status": 200, "body": {"login": "bob"}, "times": 100}]);
        state["pr_title"] = "Bob review".into();
    });
    screen.open_target("prs", None);
    recheck_account(&mut screen);
    wait(&mut screen, "Bob review");
    let bob = remembered_account(&host).unwrap().0;
    assert_eq!(bob.login(), Some("bob"));
    assert_eq!(legacy_access(&host, Some(&bob)), LegacyAccess::Blocked);
    assert_eq!(legacy_access(&host, Some(&alice)), LegacyAccess::Owner);

    key(&mut screen, KeyCode::Char('r'));
    wait(&mut screen, "Review #9 Bob review");
    assert!(!view(&mut screen).contains(DRAFT));
    key(&mut screen, KeyCode::Escape);

    // The same notification is still delivered by the forge; neither legacy mark applies.
    screen.open_target("notifications", None);
    wait(&mut screen, THREAD);
    for (offset, empty) in [(1, "Nothing saved"), (2, "Nothing done yet")] {
        marks_view(&mut screen, offset);
        wait(&mut screen, empty);
        assert!(!view(&mut screen).contains(THREAD));
    }
    assert!(
        legacy.is_file(),
        "adoption should leave the original draft in place"
    );
}

#[test]
fn remembered_account_paints_during_verification_and_offline() {
    const TEST: &str = "remembered_account_paints_during_verification_and_offline";
    const PHASE: &str = "DUSTY_FORGE_REMEMBERED_TEST_PHASE";
    if fixture_child(
        TEST,
        serde_json::json!({"issues": navigation_issues(), "comments": ["Warm fixture ready"]}),
    ) {
        return;
    }
    if let Some(phase) = std::env::var_os(PHASE) {
        let before = requests().len();
        let mut screen = open();
        goto(&mut screen, 0, 1);
        wait_for(&mut screen, "remembered rows", |frame| {
            lists(frame, 8, "Navigation issue 8")
        });
        if phase == "online" {
            // Stay on the old disk rows even after the forge has received the new check.
            wait_for(&mut screen, "blocked identity check", |frame| {
                lists(frame, 8, "Navigation issue 8")
                    && requests()
                        .iter()
                        .skip(before)
                        .any(|request| request["path"] == "user")
            });
            release("verify-remembered");
            wait_for(&mut screen, "new account's rows", |frame| {
                lists(frame, 8, "Bob issue 8") && !frame.contains("Navigation issue")
            });
            assert_eq!(
                dusty::forge::host_state::remembered_account(&HostKey::new("github.com"))
                    .unwrap()
                    .0
                    .login(),
                Some("bob"),
            );
        }
        return;
    }

    {
        let mut screen = open();
        goto(&mut screen, 0, 1);
        wait(&mut screen, "Warm fixture ready");
        settle(&mut screen, Duration::from_millis(400));
    }
    let restart = |phase: &str| {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", TEST, "--nocapture"])
            .env(PHASE, phase);
        if phase == "offline" {
            child.env("DUSTY_FORGE_OFFLINE", "1");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{phase}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let before = requests().len();
    restart("offline");
    assert_eq!(
        requests().len(),
        before,
        "offline paint made a network request"
    );

    update_state(|state| {
        state["read_gates"] = serde_json::json!([{"path": "user", "name": "verify-remembered"}]);
        state["fail"] = serde_json::json!([{
            "path": "user", "status": 200, "body": {"login": "bob"}, "times": 100
        }]);
        let mut issues = navigation_issues();
        for issue in &mut issues {
            issue["title"] = format!("Bob issue {}", issue["number"]).into();
        }
        state["issues"] = serde_json::json!(issues);
    });
    restart("online");
}

#[test]
fn a_corrupt_cache_is_ignored_by_the_screen() {
    if fixture_child(
        "a_corrupt_cache_is_ignored_by_the_screen",
        serde_json::json!({"calls":[],"comments":["Corrupt fixture ready"],"issues":navigation_issues()}),
    ) {
        return;
    }
    {
        let mut screen = open();
        goto(&mut screen, 0, 1);
        wait(&mut screen, "Corrupt fixture ready");
        settle(&mut screen, Duration::from_millis(400));
    }
    let files = entries(&cache_dir());
    assert!(!files.is_empty());
    for (index, path) in files.iter().enumerate() {
        std::fs::write(
            path,
            if index % 2 == 0 {
                "{\"schema\":1,\"key\":"
            } else {
                "\u{0}"
            },
        )
        .unwrap();
    }
    let mut screen = open();
    goto(&mut screen, 0, 1);
    wait_for(&mut screen, "#8", |frame| {
        lists(frame, 8, "Navigation issue 8")
    });
    wait(&mut screen, "Corrupt fixture ready");
}
