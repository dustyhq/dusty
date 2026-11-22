#![cfg(unix)]
//! The GitHub contract on both transports: direct HTTP against the fake
//! forge server, and `gh api` passthrough through a fake `gh` that forwards to the same server.
//! Both must produce the same domain values and the same logical requests; neither may leak the
//! token into argv, logs or error text.

#[path = "support/forge_fake.rs"]
mod forge_fake;

use dusty::forge::cli::CliTransport;
use dusty::forge::credential::Credential;
use dusty::forge::credential::CredentialSource;
use dusty::forge::credential::Secret;
use dusty::forge::http::HttpTransport;
use dusty::forge::model::ItemId;
use dusty::forge::model::ItemKind;
use dusty::forge::model::MergeMethod;
use dusty::forge::provider;
use dusty::forge::provider::Conditional;
use dusty::forge::provider::Fetched;
use dusty::forge::provider::ForgeContext;
use dusty::forge::provider::ProviderKind;
use dusty::forge::provider::RepoRef;
use dusty::forge::request::Filter;
use dusty::forge::request::ForgeError;
use dusty::forge::request::Page;
use dusty::forge::request::Payload;
use dusty::forge::request::Read;
use dusty::forge::request::RunFilter;
use dusty::forge::request::StateFilter;
use dusty::forge::request::Write;
use dusty::forge::transport;
use dusty::forge::transport::Cli;
use dusty::forge::transport::ConnectOptions;
use dusty::forge::transport::Endpoints;
use dusty::forge::transport::HostKey;
use dusty::forge::transport::Transport;
use dusty::forge::transport::TransportKind;
use forge_fake::FAKE_TOKEN;
use forge_fake::ForgeServer;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    Http,
    Gh,
}

struct World {
    directory: tempfile::TempDir,
    server: ForgeServer,
    gh: PathBuf,
}

impl World {
    fn new(state: Value) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, state.to_string()).unwrap();
        let server = ForgeServer::start(&path, "legacy");
        let gh = server.install_gh(&directory.path().join("bin"));
        Self {
            directory,
            server,
            gh,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.directory.path().join("state.json")
    }

    fn state(&self) -> Value {
        forge_fake::state(&self.state_path())
    }

    fn update(&self, change: impl FnOnce(&mut Value)) {
        let mut state = self.state();
        change(&mut state);
        std::fs::write(self.state_path(), state.to_string()).unwrap();
    }

    fn requests(&self) -> Vec<Value> {
        forge_fake::requests(&self.state_path())
    }

    fn clear_requests(&self) {
        self.update(|state| state["requests"] = json!([]));
    }

    fn endpoints(&self, host: &str) -> Endpoints {
        let mut endpoints = Endpoints::github(&HostKey::new(host));
        endpoints.override_api_base(&format!("{}/{host}/api/v3", self.server.url()));
        endpoints
    }

    fn transport(&self, via: Via, host: &str) -> Arc<dyn Transport> {
        match via {
            Via::Http => Arc::new(
                HttpTransport::new(
                    self.endpoints(host),
                    Credential {
                        token: Secret::new(FAKE_TOKEN),
                        source: CredentialSource::GhAuthToken,
                        user: None,
                        obtained: Instant::now(),
                    },
                )
                .unwrap(),
            ),
            Via::Gh => Arc::new(CliTransport::new(Cli::Gh, self.gh.clone(), None)),
        }
    }

    fn context(&self, via: Via, host: &str, repo: Option<&str>) -> ForgeContext {
        ForgeContext::new(
            self.directory.path().to_path_buf(),
            self.endpoints(host),
            ConnectOptions {
                gh: Some(self.gh.clone()),
                no_anonymous: true,
                ..ConnectOptions::default()
            },
            repo.and_then(RepoRef::parse),
            self.transport(via, host),
        )
    }
}

fn read(ctx: &ForgeContext, read: Read) -> Result<Payload, ForgeError> {
    let provider = provider::build(ctx.endpoints.kind).unwrap();
    match provider.read(ctx, &read, &Conditional::default(), &AtomicBool::new(false))? {
        Fetched::Payload { value, .. } => Ok(value),
        Fetched::NotModified => panic!("{read:?} was not modified"),
    }
}

fn write(ctx: &ForgeContext, write: Write) -> Result<dusty::forge::request::Receipt, ForgeError> {
    provider::build(ProviderKind::GitHub)
        .unwrap()
        .write(ctx, &write, &AtomicBool::new(false))
}

#[test]
fn github_pr_diff_retries_when_the_head_moves_without_using_contents_permissions() {
    let world = World::new(json!({"move_head_after_diff": ["b".repeat(40)]}));
    let ctx = world.context(Via::Http, "github.com", Some("example/demo"));

    let Payload::Diff { head, text } = read(&ctx, Read::Diff(9)).unwrap() else {
        panic!("expected a PR patch");
    };
    assert_eq!(head, "b".repeat(40), "retry must use the new head");
    assert!(text.starts_with("diff --git"));

    let requests = world.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["repos/example/demo/pulls/9"; 6],
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["accept"]
                .as_str()
                .is_some_and(|accept| accept.contains("diff")))
            .count(),
        2
    );

    world.update(|state| state["move_head_after_diff"] = json!(["c".repeat(40), "d".repeat(40)]));
    let Payload::Diff { head, text } = read(&ctx, Read::Diff(9)).unwrap() else {
        panic!("expected a visible but unanchored PR patch");
    };
    assert!(
        head.is_empty(),
        "two moving heads can't establish a revision"
    );
    assert!(text.starts_with("diff --git"));
}

/// The domain values every read of the contract produced, for comparing the two transports.
fn contract(world: &World, via: Via) -> Vec<String> {
    let ctx = world.context(via, "github.com", Some("example/demo"));
    let mut seen = Vec::new();

    let Payload::Repo(repo) = read(&ctx, Read::Repository).unwrap() else {
        panic!()
    };
    assert_eq!(
        (repo.full_name.as_str(), repo.default_branch.as_str()),
        ("example/demo", "main")
    );
    seen.push(format!("{repo:?}"));

    let filter = Filter {
        state: StateFilter::Open,
        query: String::new(),
    };
    let Payload::Rows(rows) = read(
        &ctx,
        Read::List {
            kind: ItemKind::Issue,
            filter: filter.clone(),
            page: Page::first(30),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(rows.items.len(), 1);
    assert_eq!(rows.items[0].number, 7);
    assert_eq!(rows.items[0].title, "Preserve keyboard drafts");
    assert!(
        rows.items[0]
            .body_preview
            .as_deref()
            .unwrap()
            .starts_with("A real native issue")
    );
    assert_eq!(rows.total, Some(1));
    seen.push(format!("{rows:?}"));

    let Payload::Issue(issue) = read(&ctx, Read::Issue(7)).unwrap() else {
        panic!()
    };
    assert_eq!(issue.assignees[0].login, "bob");
    assert_eq!(issue.milestone.as_ref().unwrap().title, "v1");
    assert_eq!(issue.linked_prs[0].number, 9);
    seen.push(format!("{issue:?}"));

    let Payload::PullRequest(pull) = read(&ctx, Read::PullRequest(9)).unwrap() else {
        panic!()
    };
    assert_eq!(pull.head.sha, "a".repeat(40));
    assert_eq!(pull.head.branch, "feature/native");
    assert_eq!(pull.files.len(), 2);
    assert_eq!(pull.checks.len(), 1);
    assert_eq!(pull.checks[0].name, "test (linux)");
    assert_eq!(pull.linked_issues[0].number, 7);
    seen.push(format!("{pull:?}"));

    let Payload::Timeline(timeline) = read(
        &ctx,
        Read::Timeline {
            kind: ItemKind::PullRequest,
            number: 7,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(timeline.len(), 1);
    seen.push(format!("{timeline:?}"));

    // Two GraphQL pages: the provider follows `pageInfo` through an `after` cursor.
    world.update(|state| {
        state["review_threads"] = json!({"example/demo#9": [
            {"id":"THREAD_A","isResolved":false,"comments":{"nodes":[{"author":{"login":"reviewer"},"body":"First","path":"example.rs","line":1,"originalLine":1}]}},
            {"id":"THREAD_B","isResolved":true,"comments":{"nodes":[{"author":{"login":"reviewer"},"body":"Second","path":"src/nested/other.rs","line":1,"originalLine":1}]}}
        ]});
    });
    let Payload::Threads(threads) = read(&ctx, Read::Threads(9)).unwrap() else {
        panic!()
    };
    assert_eq!(
        threads
            .iter()
            .map(|thread| (thread.id.0.as_str(), thread.resolved))
            .collect::<Vec<_>>(),
        [("THREAD_A", Some(false)), ("THREAD_B", Some(true))]
    );
    seen.push(format!("{threads:?}"));

    let Payload::Viewed(viewed) = read(&ctx, Read::Viewed(9)).unwrap() else {
        panic!()
    };
    assert_eq!(viewed.len(), 2);
    seen.push(format!("{viewed:?}"));

    let Payload::Diff { head, text: diff } = read(&ctx, Read::Diff(9)).unwrap() else {
        panic!("expected a revision-bound pull request diff");
    };
    assert_eq!(head, "a".repeat(40));
    assert!(diff.starts_with("diff --git a/example.rs"), "{diff}");
    seen.push(diff);

    let Payload::Workflows(workflows) = read(&ctx, Read::Workflows).unwrap() else {
        panic!()
    };
    assert_eq!(workflows[0].path, ".github/workflows/native.yml");
    seen.push(format!("{workflows:?}"));

    let Payload::Text(source) = read(
        &ctx,
        Read::WorkflowSource {
            id: ItemId::new(31),
            reference: Some("main".into()),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert!(source.contains("workflow_dispatch"));

    let Payload::Runs(runs) = read(
        &ctx,
        Read::Runs {
            filter: RunFilter::default(),
            page: Page::first(20),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(runs.items[0].id.0, "91");

    // The legacy world's run settles on its second view; read past that.
    world.update(|state| state["run_views"] = json!(5));
    let Payload::Run(run) = read(
        &ctx,
        Read::Run {
            id: ItemId::new(91),
            attempt: None,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(run.jobs.len(), 3);
    assert_eq!(
        run.run.web_url,
        "https://github.com/example/demo/actions/runs/91"
    );
    seen.push(format!("{:?}", run.jobs));

    let Payload::Log(log) = read(&ctx, Read::JobLog(ItemId::new(42))).unwrap() else {
        panic!()
    };
    assert!(
        log.text.contains("all native checks passed"),
        "{}",
        log.text
    );
    seen.push(log.text.clone());

    // Host-wide search: no repository term.
    let inbox = world.context(via, "github.com", None);
    let Payload::Rows(found) = read(
        &inbox,
        Read::Search {
            kind: ItemKind::PullRequest,
            filter,
            page: Page::first(30),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(found.items[0].number, 9);
    seen.push(format!("{found:?}"));

    // Writes: a REST POST with a JSON body, a GraphQL mutation, a REST PUT.
    let receipt = write(
        &ctx,
        Write::Comment {
            kind: ItemKind::Issue,
            number: 7,
            body: format!("Sent {via:?} 界"),
        },
    )
    .unwrap();
    assert_eq!(
        receipt.web_url.as_deref(),
        Some("https://github.com/example/demo/issues/7#issuecomment-1")
    );
    write(
        &ctx,
        Write::Resolve {
            thread: ItemId::from("THREAD_A"),
            resolved: true,
        },
    )
    .unwrap();
    write(
        &ctx,
        Write::Merge {
            number: 9,
            method: MergeMethod::Squash,
            head: "a".repeat(40),
            delete_branch: false,
            title: None,
            message: None,
        },
    )
    .unwrap();
    let state = world.state();
    assert_eq!(state["comments"], json!([format!("Sent {via:?} 界")]));
    assert_eq!(state["thread_resolved"]["THREAD_A"], true);
    seen
}

fn assert_requests_went(world: &World, via: Via) {
    let requests = world.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        if request["path"] == "auth token" {
            continue;
        }
        match via {
            Via::Http => {
                assert_eq!(request["transport"], "http", "{request}");
                assert_eq!(request["auth"], "valid", "{request}");
            }
            Via::Gh => {
                assert_eq!(request["transport"], "gh", "{request}");
                let argv = request["gh_argv"].as_array().unwrap();
                let words = argv
                    .iter()
                    .map(|word| word.as_str().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(words[0], "api");
                assert!(words.contains(&"--include"), "{words:?}");
                assert!(
                    words
                        .windows(2)
                        .any(|pair| pair == ["--hostname", "github.com"]),
                    "{words:?}"
                );
                if request["method"] != "GET" {
                    // stdin, or a private file for authored text (`ApiRequest::body_file`).
                    assert!(
                        words.windows(2).any(|pair| pair[0] == "--input"
                            && (pair[1] == "-" || pair[1].contains("dusty-forge-body-"))),
                        "{words:?}"
                    );
                }
            }
        }
    }
}

fn logical(world: &World) -> Vec<(Value, Value, Value)> {
    world
        .requests()
        .into_iter()
        .filter(|request| request["path"] != "auth token")
        .map(|request| {
            (
                request["method"].clone(),
                request["path"].clone(),
                request["operation"].clone(),
            )
        })
        .collect()
}

#[test]
fn github_contract_matches_on_direct_http_and_gh_passthrough() {
    let http = World::new(json!({}));
    let over_http = contract(&http, Via::Http);
    assert_requests_went(&http, Via::Http);

    let gh = World::new(json!({}));
    let over_gh = contract(&gh, Via::Gh);
    assert_requests_went(&gh, Via::Gh);

    assert_eq!(
        over_http, over_gh,
        "both transports decode to the same domain values"
    );
    assert_eq!(
        logical(&http),
        logical(&gh),
        "both transports send the same requests"
    );
    for world in [&http, &gh] {
        let log = std::fs::read_to_string(world.state_path()).unwrap();
        assert!(!log.contains(FAKE_TOKEN), "the request log holds no token");
    }
}

#[test]
fn errors_classify_the_same_on_both_transports() {
    for via in [Via::Http, Via::Gh] {
        let world = World::new(json!({}));
        let ctx = world.context(via, "github.com", Some("example/demo"));
        world.update(|state| {
            state["fail"] = json!([
                {"path":"repos/example/demo/labels","status":404,"message":"Not Found","times":1},
                {"path":"repos/example/demo/milestones","status":403,"message":"Resource not accessible","times":1},
                {"path":"pulls/9/merge","status":405,"message":"Pull Request is not mergeable","times":1},
                {"path":"actions/workflows","status":502,"message":"Server Error","times":1},
                {"path":"repos/example/demo/assignees","status":403,"message":"API rate limit exceeded","times":1}
            ]);
        });
        assert!(
            matches!(read(&ctx, Read::Labels), Err(ForgeError::NotFound(_))),
            "{via:?}"
        );
        assert!(
            matches!(read(&ctx, Read::Milestones), Err(ForgeError::Forbidden(_))),
            "{via:?}"
        );
        let merge = write(
            &ctx,
            Write::Merge {
                number: 9,
                method: MergeMethod::Merge,
                head: "a".repeat(40),
                delete_branch: false,
                title: None,
                message: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            merge.to_string(),
            "Conflict · Pull Request is not mergeable",
            "{via:?}"
        );
        assert!(
            matches!(read(&ctx, Read::Workflows), Err(ForgeError::Network(_))),
            "{via:?}"
        );
        // A GraphQL error with null data is a failure carrying the server's message.
        world.update(|state| state["fail_item_views"] = json!(1));
        let error = read(&ctx, Read::Issue(7)).unwrap_err();
        assert_eq!(
            error,
            ForgeError::Failed("Fixture refresh refused".into()),
            "{via:?}"
        );
        // Last: a rate limit pauses this credential, so later reads would
        // be refused locally.
        assert!(
            matches!(
                read(&ctx, Read::Collaborators),
                Err(ForgeError::RateLimited { .. })
            ),
            "{via:?}"
        );
    }
}

#[test]
fn no_token_reaches_argv_logs_or_error_text() {
    for via in [Via::Http, Via::Gh] {
        let world = World::new(json!({}));
        let ctx = world.context(via, "github.com", Some("example/demo"));
        // A server that echoes a token back in its error message.
        world.update(|state| {
            state["fail"] = json!([
                {"path":"repos/example/demo/labels","status":422,
                 "message":format!("token {FAKE_TOKEN} is not allowed"),"times":1},
                {"path":"repos/example/demo/milestones","status":500,
                 "message":format!("Authorization: Bearer {FAKE_TOKEN}"),"times":1}
            ]);
        });
        for error in [
            read(&ctx, Read::Labels).unwrap_err(),
            read(&ctx, Read::Milestones).unwrap_err(),
        ] {
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(FAKE_TOKEN), "{via:?}: {shown}");
            assert!(shown.contains("<redacted>"), "{via:?}: {shown}");
        }
        // The world itself holds the echoing rules, so check only what the server logged.
        for request in world.requests() {
            assert!(
                !request.to_string().contains(FAKE_TOKEN),
                "{via:?}: {request}"
            );
        }
    }
    // The credential type never prints its secret.
    let credential = Credential {
        token: Secret::new(FAKE_TOKEN),
        source: CredentialSource::GhAuthToken,
        user: None,
        obtained: Instant::now(),
    };
    assert!(!format!("{credential:?} {}", credential.token).contains(FAKE_TOKEN));
    // gh's argv for a write carries the body on stdin and no token, whatever the body says.
    let request = dusty::forge::transport::ApiRequest::new(
        dusty::forge::transport::Method::Post,
        "repos/example/demo/issues/7/comments",
    )
    .json(json!({ "body": FAKE_TOKEN }));
    let (argv, stdin, env) =
        dusty::forge::cli::argv(Cli::Gh, &HostKey::new("github.com"), &request, None);
    assert!(!argv.iter().any(|arg| arg.contains(FAKE_TOKEN)), "{argv:?}");
    assert!(argv.windows(2).any(|pair| pair == ["--input", "-"]));
    assert!(
        String::from_utf8(stdin.unwrap())
            .unwrap()
            .contains(FAKE_TOKEN)
    );
    assert!(env.iter().all(|(_, value)| !value.contains(FAKE_TOKEN)));
}

#[test]
fn passthrough_output_parses_as_an_http_response() {
    let world = World::new(json!({}));
    let output = std::process::Command::new(&world.gh)
        .args([
            "api",
            "repos/example/demo",
            "--hostname",
            "github.com",
            "--method",
            "GET",
            "--include",
            "-H",
            "Accept: application/vnd.github+json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let response = dusty::forge::cli::parse_included(&output.stdout).unwrap();
    assert_eq!(response.status, 200);
    assert!(
        response
            .header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(response.json().unwrap()["full_name"], "example/demo");
    // An HTTP error still prints the response, so the status and body survive.
    let output = std::process::Command::new(&world.gh)
        .args([
            "api",
            "repos/example/demo/nothing",
            "--hostname",
            "github.com",
            "--include",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let response = dusty::forge::cli::parse_included(&output.stdout).unwrap();
    assert_eq!(response.status, 404);
    assert!(String::from_utf8_lossy(&output.stderr).contains("(HTTP 404)"));
}

// glab permits a saved sign-in to name an explicit API origin, including the fixture's
// loopback scheme and port. A saved gh sign-in can only name the host's normal HTTPS API.
fn glab_fixture(world: &World, host: &str, signed_in: bool) -> (Endpoints, ConnectOptions) {
    let glab = world
        .server
        .install_glab(&world.directory.path().join("bin"));
    let endpoints = dusty::forge::detect::endpoints_for(
        ProviderKind::GitLab,
        &HostKey::new(host),
        Some(&format!("{}/{host}/api/v4", world.server.url())),
        &dusty::forge::credential::Env::from_pairs([] as [(&str, &str); 0]),
    );
    let config = world.directory.path().join("glab.yml");
    let mut login = format!(
        "hosts:\n  {host}:\n    api_host: {}/{host}\n",
        world.server.url()
    );
    if signed_in {
        login.push_str(&format!("    token: {FAKE_TOKEN}\n"));
    }
    std::fs::write(&config, login).unwrap();
    let options = ConnectOptions {
        glab: Some(glab),
        stores: dusty::forge::credential::Stores {
            glab_config: Some(config),
            ..Default::default()
        },
        no_anonymous: true,
        ..Default::default()
    };
    (endpoints, options)
}

#[test]
fn a_rejected_token_falls_back_to_passthrough_once() {
    let world = World::new(json!({ "world": "gitlab" }));
    let (endpoints, options) = glab_fixture(&world, "git.fallback.test", true);
    let cancel = AtomicBool::new(false);
    let chosen = transport::for_host(&endpoints, &options, &cancel).unwrap();
    assert_eq!(
        chosen.kind(),
        TransportKind::Http {
            credential_source: CredentialSource::GlabConfig,
        },
    );
    let other = World::new(json!({ "world": "gitlab" }));
    let mut other_endpoints = endpoints.clone();
    other_endpoints.override_api_base(&format!("{}/{}/api/v4", other.server.url(), endpoints.host));
    let error = transport::for_host(&other_endpoints, &options, &cancel)
        .err()
        .unwrap();
    assert!(
        matches!(&error, ForgeError::NotAuthenticated { hint, .. }
        if hint.contains(&format!("Set api_base to {}/{}/api/v4 ", world.server.url(), endpoints.host))),
        "{error}"
    );
    assert!(
        other.requests().is_empty(),
        "sent a request to the other port"
    );

    // A token for the second address is revoked after connection. The live 401 should
    // explain why the saved glab sign-in cannot take over.
    let direct = HttpTransport::new(
        other_endpoints.clone(),
        Credential::new(FAKE_TOKEN, CredentialSource::Env("GITLAB_TOKEN"), None),
    )
    .unwrap();
    let direct = Arc::new(direct);
    let project = "projects/orbit%2Fteam%2Forbit";
    let expected_base = format!("{}/{}/api/v4", world.server.url(), endpoints.host);
    for (status, message, expected_kind) in [
        (401, "Bad credentials", "NotAuthenticated"),
        (401, "Only signed in user is allowed", "SignInRequired"),
        (403, "Only signed in user is allowed", "SignInRequired"),
        (403, "This project is forbidden", "Forbidden"),
    ] {
        other.update(|state| {
            state["fail"] = json!([{"transport":"http","path":project,"status":status,
                "message":message,"times":1}]);
        });
        let revoked = ForgeContext::new(
            other.directory.path().to_path_buf(),
            other_endpoints.clone(),
            options.clone(),
            RepoRef::parse("orbit/team/orbit"),
            direct.clone(),
        );
        let error = read(&revoked, Read::Repository).unwrap_err();
        let failure = dusty::forge::command::Failure::from(error);
        assert_eq!(failure.kind, expected_kind, "{failure:?}");
        if status == 401 {
            assert!(failure.hint.contains(&expected_base), "{failure:?}");
        }
    }
    assert!(
        other
            .requests()
            .iter()
            .all(|request| request["transport"] == "http")
    );

    // Reads that always require sign-in and preflight write refusals should use the
    // anonymous transport's remedy without sending anything.
    let options_for_public = ConnectOptions {
        no_anonymous: false,
        ..options.clone()
    };
    let anonymous = transport::for_host(&other_endpoints, &options_for_public, &cancel).unwrap();
    assert_eq!(anonymous.kind(), TransportKind::Anonymous);
    let public = ForgeContext::new(
        other.directory.path().to_path_buf(),
        other_endpoints,
        options_for_public,
        RepoRef::parse("orbit/team/orbit"),
        anonymous,
    );
    other.clear_requests();
    let error = read(&public, Read::JobLog(ItemId::new(42)))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Sign in to read logs · ") && error.contains(&expected_base));
    let write = Write::CreateIssue {
        title: "Fixture".into(),
        body: String::new(),
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let error = provider::build(ProviderKind::GitLab)
        .unwrap()
        .write(&public, &write, &cancel)
        .unwrap_err();
    assert!(
        matches!(error, ForgeError::NotAuthenticated { hint, .. } if hint.contains(&expected_base))
    );
    assert!(other.requests().is_empty());

    world.update(|state| {
        state["fail"] = json!([{"transport":"http","path":project,"status":401,
            "message":"Bad credentials","times":1}]);
    });
    let ctx = ForgeContext::new(
        world.directory.path().to_path_buf(),
        endpoints.clone(),
        options.clone(),
        RepoRef::parse("orbit/team/orbit"),
        chosen.clone(),
    );
    let request = transport::ApiRequest::get(project);
    let repo = ctx.json(&request, &cancel).unwrap();
    assert_eq!(repo["path_with_namespace"], "orbit/team/orbit");
    assert_eq!(ctx.transport_kind(), TransportKind::Cli(Cli::Glab));
    let repository_reads = world
        .requests()
        .into_iter()
        .filter(|request| request["path"] == project)
        .map(|request| request["transport"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(repository_reads, ["http", "glab"]);

    // Once the context switches, it won't try HTTP again.
    world.update(|state| {
        state["reject_http_auth"] = json!(true);
    });
    let ctx = ForgeContext::new(
        world.directory.path().to_path_buf(),
        endpoints,
        options,
        RepoRef::parse("orbit/team/orbit"),
        chosen,
    );
    world.clear_requests();
    ctx.json(&request, &cancel).unwrap();
    ctx.json(&request, &cancel).unwrap();
    let requests = world.requests();
    let transports = requests
        .iter()
        .filter(|request| request["path"] == project)
        .map(|request| request["transport"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(transports, ["http", "glab", "glab"]);
    // The fallback checks glab's own sign-in once before using it; nothing else is sent.
    let others = requests
        .iter()
        .filter(|request| request["path"] != project)
        .map(|request| {
            format!(
                "{} {}",
                request["transport"].as_str().unwrap(),
                request["path"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert!(
        others.len() <= 1 && others.iter().all(|request| request == "glab user"),
        "{others:?}"
    );
}

#[test]
fn a_same_origin_viewer_404_suggests_the_saved_rest_path() {
    let host = "git.prefix.test";
    let world = World::new(json!({ "world": "gitlab" }));
    let (saved, options) = glab_fixture(&world, host, true);
    let options = ConnectOptions {
        force: Some(transport::Force::Http),
        ..options
    };
    let mut missing_prefix = saved.clone();
    missing_prefix.override_api_base(&format!("{}/{host}/wrong/api/v4", world.server.url()));
    world.update(|state| {
        state["fail"] =
            json!([{"transport":"http","path":"wrong/api/v4/user","status":404,"times":1}]);
    });
    let cancel = AtomicBool::new(false);
    let error = transport::for_host(&missing_prefix, &options, &cancel)
        .err()
        .unwrap();
    assert!(matches!(&error, ForgeError::NotFound(_)));
    let failure = dusty::forge::command::Failure::from(error);
    assert_eq!(failure.kind, "NotFound");
    let json = failure.json();
    assert_eq!(
        json["error"]["message"],
        "Not found · Dusty couldn't find the forge API at the configured address."
    );
    assert_eq!(
        json["error"]["hint"],
        format!("Set api_base to {}", saved.api_base)
    );
    let chosen = transport::for_host(&saved, &options, &cancel).unwrap();
    assert!(matches!(chosen.kind(), TransportKind::Http { .. }));
    let requests = world.requests();
    assert_eq!(requests[0]["path"], "wrong/api/v4/user");
    assert_eq!(requests[1]["path"], "user");
}

#[test]
fn normal_github_dot_com_gh_sign_in_is_ready_without_a_notice() {
    let world = World::new(json!({}));
    let host = HostKey::new("github.com");
    let mut endpoints = Endpoints::github(&host);
    // Use the default origin for selection; the fake gh sends only to its loopback sidecar.
    endpoints.override_api_base("https://api.github.com");
    let options = ConnectOptions {
        gh: Some(world.gh.clone()),
        force: Some(transport::Force::Cli),
        ..ConnectOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let chosen = transport::for_host(&endpoints, &options, &cancel).unwrap();
    assert_eq!(chosen.kind(), TransportKind::Cli(Cli::Gh));
    let ctx = ForgeContext::new(
        world.directory.path().to_path_buf(),
        endpoints,
        options,
        RepoRef::parse("example/demo"),
        chosen,
    );
    assert!(matches!(
        read(&ctx, Read::Repository).unwrap(),
        Payload::Repo(_)
    ));
    assert!(
        world
            .requests()
            .iter()
            .any(|request| request["transport"] == "gh")
    );
    let account = dusty::forge::host_state::account(&host).unwrap();
    assert!(matches!(
        account.state,
        dusty::forge::host_state::HostState::Ready { .. }
    ));
    assert!(account.state.notice("github.com", None).is_none());
}

#[test]
fn a_signed_out_host_without_a_cli_is_a_state_not_a_crash() {
    let world = World::new(json!({"glab_auth": {"*": "missing"}}));
    let host = "git.signedout.test";
    let (endpoints, options) = glab_fixture(&world, host, false);
    let forced = ConnectOptions {
        force: Some(transport::Force::Http),
        ..options.clone()
    };
    let cancel = AtomicBool::new(false);
    let error = transport::for_host(&endpoints, &forced, &cancel)
        .err()
        .unwrap();
    assert!(
        matches!(&error, ForgeError::NotAuthenticated { hint, .. } if hint == &format!("glab auth login --hostname {host}")),
        "{error:?}"
    );
    // Without forcing, a missing token means CLI passthrough.
    let chosen = transport::for_host(&endpoints, &options, &cancel).unwrap();
    assert_eq!(chosen.kind(), TransportKind::Cli(Cli::Glab));
    let missing = ConnectOptions {
        no_anonymous: true,
        ..ConnectOptions::default()
    };
    let error = transport::for_host(&world.endpoints("ghe.nogh.test"), &missing, &cancel)
        .err()
        .unwrap();
    assert_eq!(error, ForgeError::CliMissing(Cli::Gh));
    // By default the same host is browsed anonymously instead.
    let anonymous = transport::for_host(
        &world.endpoints("ghe.anonymous.test"),
        &ConnectOptions::default(),
        &cancel,
    )
    .unwrap();
    assert_eq!(anonymous.kind(), TransportKind::Anonymous);
    // So is a configured gh path that is not there.
    let nowhere = ConnectOptions {
        gh: Some(world.directory.path().join("nowhere/gh")),
        ..ConnectOptions::default()
    };
    let anonymous =
        transport::for_host(&world.endpoints("ghe.nowhere.test"), &nowhere, &cancel).unwrap();
    assert_eq!(anonymous.kind(), TransportKind::Anonymous);
}

#[test]
fn redirects_carry_the_token_only_to_the_api_origin() {
    let world = World::new(json!({}));
    let port = world.server.port;
    world.update(|state| {
        state["fail"] = json!([
            {"path":"actions/jobs/42/logs","status":302,"location":format!("http://localhost:{port}/_blob/foreign"),"times":1},
            {"path":"actions/jobs/43/logs","status":302,"location":format!("http://127.0.0.1:{port}/_blob/same"),"times":1}
        ]);
    });
    let ctx = world.context(Via::Http, "github.com", Some("example/demo"));
    for job in [42, 43] {
        let Payload::Log(log) = read(&ctx, Read::JobLog(ItemId::new(job))).unwrap() else {
            panic!()
        };
        assert_eq!(log.text, "blob payload\n");
    }
    let blobs = world.state()["blob_requests"].clone();
    assert_eq!(
        blobs,
        json!([
            {"path":"/_blob/foreign","authorization":"absent"},
            {"path":"/_blob/same","authorization":"present"}
        ])
    );
}

#[test]
fn host_keys_and_endpoints_follow_gh() {
    assert_eq!(HostKey::new("GitHub.COM.").to_string(), "github.com");
    let github = Endpoints::github(&HostKey::new("github.com"));
    if std::env::var_os("DUSTY_FORGE_API_BASE").is_none() {
        assert_eq!(github.api_base, "https://api.github.com");
        assert_eq!(
            github.graphql.as_deref(),
            Some("https://api.github.com/graphql")
        );
        let ghes = Endpoints::github(&HostKey::new("git.corp.example:8443"));
        assert_eq!(ghes.api_base, "https://git.corp.example:8443/api/v3");
        assert_eq!(
            ghes.graphql.as_deref(),
            Some("https://git.corp.example:8443/api/graphql")
        );
    }
    assert!(Endpoints::check_scheme("http://127.0.0.1:9/x").is_ok());
    assert!(Endpoints::check_scheme("http://example.com/x").is_err());
}
