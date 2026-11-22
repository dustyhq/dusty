//! `ForgeJobs::request` / `ForgeJobs::write`: provider-layer work on the same
//! single, cancellable forge worker. Host, transport and credential are resolved on the worker
//! so the UI thread never runs a CLI or opens a socket.

use super::ForgeCommands;
use super::ForgeJobs;
use super::ForgeReply;
use crate::cache;
use crate::cli::redact;
use crate::detect;
use crate::provider;
use crate::provider::Conditional;
use crate::provider::Fetched;
use crate::provider::ForgeContext;
use crate::provider::Provider;
use crate::provider::RepoRef;
use crate::request::ForgeError;
use crate::request::Payload;
use crate::request::Read;
use crate::request::Receipt;
use crate::request::Write;
use crate::transport;
use crate::transport::AccountNamespace;
use crate::transport::ConnectOptions;
use crate::transport::HostKey;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Where a request goes. `host: None` means "the forge of the repository's remote" (the
/// first `Read::Repository`); `repo: None` with a host is a host-wide read (inbox search).
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ForgeTarget {
    pub host: Option<String>,
    pub repo: Option<String>,
}

impl ForgeTarget {
    pub fn remote() -> Self {
        Self::default()
    }

    pub fn repository(host: &str, repo: &str) -> Self {
        Self {
            host: Some(host.into()),
            repo: Some(repo.into()),
        }
    }

    pub fn host(host: &str) -> Self {
        Self {
            host: Some(host.into()),
            repo: None,
        }
    }

    pub fn resolve(&self, root: &Path) -> Result<(HostKey, Option<RepoRef>), ForgeError> {
        match &self.host {
            Some(host) => {
                let host = super::valid_host(host).map_err(ForgeError::Failed)?;
                let repo =
                    match &self.repo {
                        Some(repo) => Some(RepoRef::parse(repo).ok_or_else(|| {
                            ForgeError::Failed(format!("invalid repository {repo}"))
                        })?),
                        None => None,
                    };
                Ok((HostKey::new(&host), repo))
            }
            None => {
                let remote = detect::choose_remote(root)?;
                Ok((remote.host, Some(remote.repo)))
            }
        }
    }
}

/// Provider, endpoints and transport for a target.
pub fn connect(
    root: &Path,
    target: &ForgeTarget,
    cancel: &AtomicBool,
) -> Result<(Arc<dyn Provider>, ForgeContext), ForgeError> {
    let (host, repo) = target.resolve(root)?;
    connect_to(root, &host, repo, cancel)
}

/// `connect` for a host and repository already resolved.
pub fn connect_to(
    root: &Path,
    host: &HostKey,
    repo: Option<RepoRef>,
    cancel: &AtomicBool,
) -> Result<(Arc<dyn Provider>, ForgeContext), ForgeError> {
    let options = ConnectOptions::from_env();
    let detected = detect::detect(host, &options, cancel)?;
    let provider = provider::build(detected.kind)?;
    let endpoints = detected.endpoints;
    let transport = transport::for_host(&endpoints, &options, cancel)?;
    // Detection already saw this instance refuse anonymous API calls: say so instead of
    // sending reads that can only fail.
    if detected.sign_in_required && transport.kind().anonymous() {
        let error = ForgeError::SignInRequired {
            host: endpoints.host.to_string(),
            hints: transport.sign_in_advice().map_or_else(
                || crate::credential::sign_in_hints(endpoints.kind, &endpoints.host),
                |advice| vec![advice.to_owned()],
            ),
        };
        crate::host_state::observe_error(&endpoints, &error);
        return Err(error);
    }
    Ok((
        provider,
        ForgeContext::new(root.to_path_buf(), endpoints, options, repo, transport),
    ))
}

/// Anonymous quotas are small (60/hour on github.com): cached reads stay fresh this many times
/// longer, so revalidation sweeps and warm-ups spend less of it.
pub const ANONYMOUS_TTL_FACTOR: u32 = 4;

/// The budget for a payload on this context.
fn budget(ctx: &ForgeContext, read: &Read, value: &Payload) -> std::time::Duration {
    let ttl = cache::ttl(read, Some(value));
    if ctx.anonymous() {
        ttl.saturating_mul(ANONYMOUS_TTL_FACTOR)
    } else {
        ttl
    }
}

fn failed(error: String) -> ForgeError {
    ForgeError::Failed(redact(&error))
}

/// One conditional read written through to the disk cache: a list this provider can
/// read incrementally asks only for what changed since the cached copy and merges into it; a
/// payload is stored with its freshness budget, a 304 or unchanged fingerprint renews the entry.
pub fn fetch_through(
    provider: &dyn Provider,
    ctx: &ForgeContext,
    read: &Read,
    conditional: Conditional,
    cancel: &AtomicBool,
) -> Result<Fetched, ForgeError> {
    let Some(account) = ctx.account() else {
        return provider.read(ctx, read, &Conditional::default(), cancel);
    };
    let cache = cache::Cache::shared();
    let key = cache::CacheKey::new(account, ctx.repo.as_ref(), read);
    let (conditional, base) = match &cache {
        Some(cache) => crate::incremental::prepare(provider, ctx, cache, &key, read, conditional),
        None => (conditional, None),
    };
    let started = cache::now_ms();
    let result = crate::inflight::read(provider, ctx, read, &conditional, cancel);
    let result = match (result, base) {
        (Ok(fetched), Some(base)) => Ok(crate::incremental::merge(read, base, fetched)),
        (result, _) => result,
    };
    if let (Ok(fetched), Some(cache)) = (&result, &cache) {
        match fetched {
            Fetched::Payload {
                value,
                etag,
                fingerprint,
            } => {
                cache.store_since(
                    &key,
                    value,
                    budget(ctx, read, value),
                    etag.clone(),
                    fingerprint.clone(),
                    started,
                );
                // A batch also fills each item's own entry, so opening one of them after a
                // prefetch is a cache hit.
                for (read, value, fingerprint) in cache::items_of(value) {
                    cache.store_since(
                        &cache::CacheKey::new(account, ctx.repo.as_ref(), &read),
                        &value,
                        budget(ctx, &read, &value),
                        None,
                        fingerprint,
                        started,
                    );
                }
            }
            Fetched::NotModified => {
                cache.touch(&key);
            }
        }
        let missing = key.missing();
        if missing.file(cache.root()).exists() {
            cache.remove(&missing);
        }
    }
    // A durable "not found" (an expired log, a deleted run, an endpoint this forge lacks) is
    // remembered too, so a restart paints it from the cache like any answer.
    if let (Err(error @ ForgeError::NotFound(_)), Some(cache)) = (&result, &cache) {
        cache.store_since(
            &key.missing(),
            redact(&error.to_string()),
            cache::MISSING_TTL,
            None,
            None,
            started,
        );
    }
    result
}

/// Where a cached read's answer came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Served {
    /// A fresh cache entry; nothing was sent.
    Cache,
    /// The forge answered 304 (or an unchanged fingerprint); the cached copy was renewed.
    Revalidated,
    Network,
}

/// A fresh cache hit in the remembered namespace; no credentials are consulted here.
pub(crate) fn remembered_read(
    host: &HostKey,
    repo: Option<&RepoRef>,
    read: &Read,
) -> Option<(Payload, provider::ProviderKind)> {
    let (account, kind) = crate::host_state::remembered_account(host)?;
    let cache = cache::Cache::shared()?;
    let envelope = cache.get::<Payload>(&cache::CacheKey::new(&account, repo, read))?;
    envelope.fresh().then_some((envelope.data, kind))
}

/// A read for a one-shot caller (`dusty-git forge`): a fresh cache entry answers without a
/// request; otherwise the read revalidates the cached copy with its ETag and writes through.
pub fn cached_read(
    provider: &dyn Provider,
    ctx: &ForgeContext,
    read: &Read,
    refresh: bool,
    cancel: &AtomicBool,
) -> Result<(Payload, Served), ForgeError> {
    let cache = cache::Cache::shared();
    let cached = cache
        .as_ref()
        .zip(ctx.account())
        .and_then(|(cache, account)| {
            cache.get::<Payload>(&cache::CacheKey::new(account, ctx.repo.as_ref(), read))
        });
    if let Some(envelope) = &cached
        && envelope.fresh()
        && !refresh
    {
        return Ok((envelope.data.clone(), Served::Cache));
    }
    let conditional = Conditional {
        etag: cached.as_ref().and_then(|envelope| envelope.etag.clone()),
        fingerprint: cached
            .as_ref()
            .and_then(|envelope| envelope.fingerprint.clone()),
        ..Conditional::default()
    };
    match fetch_through(provider, ctx, read, conditional, cancel)? {
        Fetched::Payload { value, .. } => Ok((value, Served::Network)),
        Fetched::NotModified => match cached {
            Some(envelope) => Ok((envelope.data, Served::Revalidated)),
            // Only a cached copy can be revalidated; read it again in full.
            None => match fetch_through(provider, ctx, read, Conditional::default(), cancel)? {
                Fetched::Payload { value, .. } => Ok((value, Served::Network)),
                Fetched::NotModified => Err(ForgeError::BadResponse(
                    "the forge answered 304 to an unconditional read".into(),
                )),
            },
        },
    }
}

pub const ACCOUNT_CHANGED: &str = "Forge account changed. Confirm the change again.";
pub const UNVERIFIED_SIGN_IN: &str = "Couldn't verify your sign-in. Check it before trying again.";

pub(super) enum Confirmation<'a> {
    Captured(Option<&'a AccountNamespace>),
    Immediate,
}

/// Run one write against its provider and drop what it made stale from the cache.
/// Porcelain acts locally; a PR creation pushes first when asked and remembers its base
/// (`branch.<head>.gh-merge-base`).
pub fn write_through(
    commands: &ForgeCommands<'_>,
    root: &Path,
    target: &ForgeTarget,
    write: &Write,
    account: Option<&AccountNamespace>,
) -> Result<Receipt, ForgeError> {
    write_connected(
        commands,
        root,
        target,
        write,
        Confirmation::Captured(account),
    )
}

/// A one-shot command connects and acts without an intervening queue or confirmation form.
pub(crate) fn write_now(
    commands: &ForgeCommands<'_>,
    root: &Path,
    target: &ForgeTarget,
    write: &Write,
) -> Result<Receipt, ForgeError> {
    write_connected(commands, root, target, write, Confirmation::Immediate)
}

fn write_connected(
    commands: &ForgeCommands<'_>,
    root: &Path,
    target: &ForgeTarget,
    write: &Write,
    confirmation: Confirmation<'_>,
) -> Result<Receipt, ForgeError> {
    if write.porcelain() {
        return commands.porcelain(root, target, write, confirmation);
    }
    let target = match write {
        Write::CreatePullRequest(request) => {
            ForgeTarget::repository(&request.host, &request.base_repository)
        }
        _ => target.clone(),
    };
    let (provider, ctx) = connect(root, &target, commands.cancelled)?;
    dispatch_write(
        provider.as_ref(),
        &ctx,
        write,
        confirmation,
        commands.cancelled,
        |request| super::pull_request::push_reviewed(commands, root, request).map_err(failed),
    )
}

fn dispatch_write(
    provider: &dyn Provider,
    ctx: &ForgeContext,
    write: &Write,
    confirmation: Confirmation<'_>,
    cancel: &AtomicBool,
    push_reviewed: impl FnOnce(&super::pull_request::CreatePullRequest) -> Result<(), ForgeError>,
) -> Result<Receipt, ForgeError> {
    // Compare before either a provider write or a PR's prerequisite push.
    if let Confirmation::Captured(expected) = confirmation {
        check_confirmation(expected, ctx.account())?;
    }
    ctx.require_credential(write, local_write(provider.kind(), write))?;
    let pushed = match write {
        Write::CreatePullRequest(request) => {
            push_reviewed(request)?;
            request.push.is_some()
        }
        _ => false,
    };
    let written = provider.write(ctx, write, cancel);
    if let Ok(receipt) = &written {
        invalidate(ctx, &receipt.invalidates);
        if let Write::CreatePullRequest(request) = write {
            crate::template::set_merge_base(root, &request.head_branch, &request.base_branch);
        }
    }
    written.map_err(|error| match error {
        _ if pushed => {
            ForgeError::Failed("Branch pushed, but the pull request wasn't created".into())
        }
        error => error,
    })
}

pub(super) fn check_confirmation(
    expected: Option<&AccountNamespace>,
    connected: Option<&AccountNamespace>,
) -> Result<(), ForgeError> {
    match (expected, connected) {
        (None, _) => Ok(()),
        (Some(expected), Some(connected)) if expected == connected => Ok(()),
        (Some(expected), Some(connected))
            if expected.login().is_some() && connected.login().is_some() =>
        {
            Err(ForgeError::Failed(ACCOUNT_CHANGED.into()))
        }
        (Some(_), _) => Err(ForgeError::Failed(UNVERIFIED_SIGN_IN.into())),
    }
}

/// Writes a provider serves without the forge's write API: viewed files kept on this computer
/// (GitLab, Gitea) and artifact downloads of public projects (GitLab, Gitea).
pub fn local_write(kind: provider::ProviderKind, write: &Write) -> bool {
    kind != provider::ProviderKind::GitHub
        && matches!(
            write,
            Write::SetViewed { .. } | Write::DownloadArtifact { .. }
        )
}

/// What a confirmed write changed is revalidated by the next reader.
pub fn invalidate(ctx: &ForgeContext, reads: &[Read]) {
    if let (Some(cache), Some(account)) = (cache::Cache::shared(), ctx.account()) {
        for read in reads {
            cache.invalidate(&cache::CacheKey::new(account, ctx.repo.as_ref(), read));
        }
    }
}

impl ForgeJobs {
    /// Run one `Read` against the target's provider.
    pub fn request(
        &mut self,
        root: PathBuf,
        request_id: u64,
        target: ForgeTarget,
        read: Read,
    ) -> Result<(), String> {
        self.fetch(root, request_id, target, read, Conditional::default())
    }

    /// Run one conditional `Read` and write the answer through to the disk cache.
    pub fn fetch(
        &mut self,
        root: PathBuf,
        request_id: u64,
        target: ForgeTarget,
        read: Read,
        conditional: Conditional,
    ) -> Result<(), String> {
        let background = self.warming();
        self.fetch_as(
            root,
            request_id,
            target,
            read,
            (None, conditional),
            background,
        )
    }

    /// Fetch a read, optionally in the background (yielding to reads someone waits for).
    /// The validation account is the owner of already painted data being checked, not
    /// merely a remembered login. An uncached read can use a credential without a login.
    pub fn fetch_as(
        &mut self,
        root: PathBuf,
        request_id: u64,
        target: ForgeTarget,
        read: Read,
        validation: (Option<AccountNamespace>, Conditional),
        background: bool,
    ) -> Result<(), String> {
        self.spawn(root, request_id, move |root, commands| {
            let (host, result, account) = match connect(root, &target, commands.cancelled) {
                Ok((provider, ctx)) => {
                    let account = ctx.account().cloned();
                    let (expected, mut conditional) = validation;
                    // Only revalidation of a signed-in projection needs a verified login.
                    // On a miss, fetch_through uses the live namespace (or no cache at all).
                    if expected
                        .as_ref()
                        .and_then(AccountNamespace::login)
                        .is_some()
                        && account.as_ref().and_then(AccountNamespace::login).is_none()
                    {
                        return ForgeReply::Fetched(
                            read,
                            Some(ctx.host().clone()),
                            Err(ForgeError::Failed(UNVERIFIED_SIGN_IN.into())),
                            account,
                        );
                    }
                    if account.is_none() || account != expected {
                        conditional = Conditional::default();
                    }
                    ctx.set_background(background);
                    let result = fetch_through(
                        provider.as_ref(),
                        &ctx,
                        &read,
                        conditional,
                        commands.cancelled,
                    );
                    (Some(ctx.host().clone()), result, account)
                }
                Err(error) => (None, Err(error), None),
            };
            ForgeReply::Fetched(read, host, result, account)
        })
    }

    /// Run one `Write`; porcelain acts locally, a PR creation pushes first when asked.
    pub fn write(
        &mut self,
        root: PathBuf,
        request_id: u64,
        target: ForgeTarget,
        write: Write,
        account: Option<AccountNamespace>,
    ) -> Result<(), String> {
        self.spawn(root, request_id, move |root, commands| {
            let result = write_through(commands, root, &target, &write, account.as_ref());
            ForgeReply::Written(write, result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingTransport {
        login: Option<&'static str>,
        anonymous: bool,
        requests: AtomicUsize,
    }

    impl transport::Transport for RecordingTransport {
        fn kind(&self) -> transport::TransportKind {
            if self.anonymous {
                transport::TransportKind::Anonymous
            } else {
                transport::TransportKind::Http {
                    credential_source: crate::credential::CredentialSource::GhAuthToken,
                }
            }
        }

        fn verified_login(&self) -> Option<&str> {
            self.login
        }

        fn graphql_batching(&self) -> bool {
            false
        }

        fn send(
            &self,
            _: &HostKey,
            _: &transport::ApiRequest,
            _: &AtomicBool,
        ) -> Result<transport::ApiResponse, transport::TransportError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Ok(transport::ApiResponse {
                status: 201,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
        }
    }

    fn connection(
        kind: provider::ProviderKind,
        login: Option<&'static str>,
        anonymous: bool,
    ) -> (ForgeContext, Arc<RecordingTransport>) {
        let transport = Arc::new(RecordingTransport {
            login,
            anonymous,
            requests: AtomicUsize::new(0),
        });
        let ctx = ForgeContext::new(
            std::env::temp_dir(),
            detect::endpoints_for(
                kind,
                &HostKey::new("forge.test:8443"),
                None,
                &crate::credential::Env::from_pairs::<&str, &str>([]),
            ),
            ConnectOptions::default(),
            RepoRef::parse("o/r"),
            transport.clone(),
        );
        (ctx, transport)
    }

    #[test]
    fn dispatch_does_not_push_or_write_with_a_different_or_unverifiable_login() {
        let kind = provider::ProviderKind::GitHub;
        let provider = provider::build(kind).unwrap();
        let cancel = AtomicBool::new(false);
        for (login, anonymous, message) in [
            (Some("bob"), false, ACCOUNT_CHANGED),
            (None, false, UNVERIFIED_SIGN_IN),
            (None, true, UNVERIFIED_SIGN_IN),
        ] {
            let (ctx, transport) = connection(kind, login, anonymous);
            let a = AccountNamespace::signed_in(ctx.host().clone(), "alice").unwrap();
            let pushed = std::cell::Cell::new(false);
            let write = Write::CreatePullRequest(super::super::pull_request::CreatePullRequest {
                host: ctx.host().to_string(),
                base_repository: "o/r".into(),
                base_branch: "main".into(),
                head_repository: "o/r".into(),
                head_branch: "feature".into(),
                title: "A new PR".into(),
                body: String::new(),
                draft: false,
                push: Some(super::super::pull_request::PullRequestPush {
                    remote: "origin".into(),
                    commit: "a".repeat(40),
                    ssh_host_alias: None,
                }),
            });
            let result = dispatch_write(
                provider.as_ref(),
                &ctx,
                &write,
                Confirmation::Captured(Some(&a)),
                &cancel,
                |_| {
                    pushed.set(true);
                    Ok(())
                },
            );
            assert!(matches!(result, Err(ForgeError::Failed(text)) if text == message));
            assert!(!pushed.get());
            assert_eq!(transport.requests.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn dispatch_sends_a_write_when_the_login_was_never_verified() {
        let kind = provider::ProviderKind::Gitea;
        let provider = provider::build(kind).unwrap();
        let (ctx, transport) = connection(kind, None, false);
        let result = dispatch_write(
            provider.as_ref(),
            &ctx,
            &Write::Comment {
                kind: crate::model::ItemKind::Issue,
                number: 7,
                body: "Comment without read:user".into(),
            },
            Confirmation::Captured(None),
            &AtomicBool::new(false),
            |_| panic!("posting a comment does not push"),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(transport.requests.load(Ordering::Relaxed), 1);
    }
}
