//! Tokens. Dusty never asks for one: it reads the forge CLI's sign-in or the
//! documented environment variables, first hit wins:
//!
//! | provider | env | CLI store | CLI command |
//! |---|---|---|---|
//! | GitHub | `DUSTY_FORGE_TOKEN`, `GH_TOKEN`/`GITHUB_TOKEN` (github.com, `*.ghe.com`), `GH_ENTERPRISE_TOKEN`/`GITHUB_ENTERPRISE_TOKEN` (others) | — | `gh auth token --hostname H` |
//! | GitLab | `DUSTY_FORGE_TOKEN`, `GITLAB_TOKEN` | glab `config.yml` `hosts.H.token` | `glab auth status --hostname H --show-token` |
//! | Gitea/Forgejo | `DUSTY_FORGE_TOKEN`, `GITEA_SERVER_TOKEN` (with `GITEA_SERVER_URL`), `FORGEJO_TOKEN`, `GITEA_TOKEN` | fj `keys.json`, tea `config.yml` | — |
//!
//! Credentials must match the API origin. Unscoped environment tokens are for the public
//! service only. A success is cached 5 min, a miss 10 s, a crash never. Tokens never
//! enter argv, logs, notices, the cache or `hosts.json`; `Secret` prints `<redacted>` and every
//! token read is registered with `cli::redact`, which strips it from any text verbatim.

use crate::cli;
use crate::provider::ProviderKind;
use crate::request::ForgeError;
use crate::transport::ConnectOptions;
use crate::transport::Endpoints;
use crate::transport::HostKey;
use crate::transport::api_origin;
use crate::yaml_lite;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use std::time::Instant;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(token: impl Into<String>) -> Self {
        let secret = Self(token.into());
        cli::remember_secret(&secret.0);
        secret
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub enum CredentialSource {
    Env(&'static str),
    GhAuthToken,
    GlabConfig,
    GlabAuthStatus,
    FjKeys,
    /// A tea login, by name (`tea login "cb"`); passthrough uses the same name.
    TeaConfig(String),
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialSource::Env(name) => write!(formatter, "${name}"),
            CredentialSource::GhAuthToken => formatter.write_str("gh token"),
            CredentialSource::GlabConfig => formatter.write_str("glab config"),
            CredentialSource::GlabAuthStatus => formatter.write_str("glab auth status"),
            CredentialSource::FjKeys => formatter.write_str("fj keys"),
            CredentialSource::TeaConfig(login) => write!(formatter, "tea login \"{login}\""),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Credential {
    pub token: Secret,
    pub source: CredentialSource,
    /// The account, when the store names it or after the live `GET /user` check.
    pub user: Option<String>,
    pub obtained: Instant,
}

impl Credential {
    pub fn new(token: impl Into<String>, source: CredentialSource, user: Option<String>) -> Self {
        Self {
            token: Secret::new(token),
            source,
            user,
            obtained: Instant::now(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum CredentialState {
    Present(Credential),
    Missing {
        tried: Vec<CredentialSource>,
        hint: String,
    },
    /// The lookup itself failed (timeout, unreadable output); retried on the next request.
    Crashed(String),
}

pub const PRESENT_TTL: Duration = Duration::from_secs(5 * 60);
pub const MISSING_TTL: Duration = Duration::from_secs(10);
const AUTH_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const STORE_CAP: u64 = 1024 * 1024;

/// The environment credential lookup reads: the process's, or an explicit map (tests).
/// Debug prints names only.
#[derive(Clone, Default)]
pub struct Env {
    vars: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.vars {
            None => formatter.write_str("Env(process)"),
            Some(vars) => formatter.debug_list().entries(vars.keys()).finish(),
        }
    }
}

impl Env {
    pub fn process() -> Self {
        Self { vars: None }
    }

    pub(crate) fn is_process(&self) -> bool {
        self.vars.is_none()
    }

    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            vars: Some(
                pairs
                    .into_iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect(),
            ),
        }
    }

    /// A set, non-blank variable, trimmed.
    pub fn get(&self, name: &str) -> Option<String> {
        let value = match &self.vars {
            None => crate::sandbox::var(name),
            Some(vars) => vars.get(name).cloned(),
        }?;
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.get(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }

    fn home(&self) -> Option<PathBuf> {
        self.path("HOME")
    }
}

/// Where the CLIs keep their sign-ins. `from_env` follows each tool's own lookup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Stores {
    pub glab_config: Option<PathBuf>,
    pub fj_keys: Vec<PathBuf>,
    pub tea_config: Option<PathBuf>,
}

impl Stores {
    pub fn from_env(env: &Env) -> Self {
        let config_home = env
            .path("XDG_CONFIG_HOME")
            .or_else(|| env.home().map(|home| home.join(".config")));
        let data_home = env
            .path("XDG_DATA_HOME")
            .or_else(|| env.home().map(|home| home.join(".local/share")));
        let glab_config = env
            .path("GLAB_CONFIG_DIR")
            .or_else(|| config_home.as_ref().map(|home| home.join("glab-cli")))
            .map(|directory| directory.join("config.yml"));
        let mut fj_keys: Vec<PathBuf> = data_home
            .iter()
            .map(|home| home.join("forgejo-cli/keys.json"))
            .collect();
        let mut tea_config = config_home.as_ref().map(|home| home.join("tea/config.yml"));
        if cfg!(target_os = "macos")
            && let Some(home) = env.home()
        {
            let support = home.join("Library/Application Support");
            for organisation in ["forgejo-cli", "Cyborus"] {
                fj_keys.push(support.join(format!("{organisation}.forgejo-cli/keys.json")));
            }
            if !tea_config.as_ref().is_some_and(|path| path.is_file()) {
                tea_config = Some(support.join("tea/config.yml"));
            }
        }
        Self {
            glab_config,
            fj_keys,
            tea_config,
        }
    }
}

/// The cache of resolutions, with the clock passed in so tests can age it.
#[derive(Default)]
pub struct CredentialCache {
    entries: Mutex<HashMap<CacheKey, (CredentialState, Instant)>>,
}

type CacheKey = (Endpoints, ConnectOptions);

impl CredentialCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, (CredentialState, Instant)>> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn get(
        &self,
        endpoints: &Endpoints,
        options: &ConnectOptions,
        now: Instant,
    ) -> Option<CredentialState> {
        let key = (endpoints.clone(), options.clone());
        let entries = self.entries();
        let (state, at) = entries.get(&key)?;
        let ttl = match state {
            CredentialState::Present(_) => PRESENT_TTL,
            _ => MISSING_TTL,
        };
        (now.saturating_duration_since(*at) < ttl).then(|| state.clone())
    }

    /// Store a resolution; a crash is never cached.
    pub fn put(
        &self,
        endpoints: &Endpoints,
        options: &ConnectOptions,
        state: &CredentialState,
        now: Instant,
    ) {
        if matches!(state, CredentialState::Crashed(_)) {
            return;
        }
        self.entries()
            .insert((endpoints.clone(), options.clone()), (state.clone(), now));
    }

    pub fn evict(&self, endpoints: &Endpoints, options: &ConnectOptions) {
        self.entries().remove(&(endpoints.clone(), options.clone()));
    }

    pub fn clear(&self) {
        self.entries().clear();
    }
}

fn cache() -> &'static CredentialCache {
    static CACHE: OnceLock<CredentialCache> = OnceLock::new();
    CACHE.get_or_init(CredentialCache::new)
}

/// A host's token, from the process environment and the CLI stores, cached.
pub fn resolve(
    endpoints: &Endpoints,
    options: &ConnectOptions,
    cancel: &AtomicBool,
) -> CredentialState {
    if let Some(state) = cache().get(endpoints, options, Instant::now()) {
        return state;
    }
    let state = lookup(endpoints, options, &Env::process(), cancel);
    cache().put(endpoints, options, &state, Instant::now());
    state
}

/// The forge refused the token (401): forget it, and remember that one was refused so the
/// screens can say "expired or revoked" instead of "not signed in".
pub fn evict(endpoints: &Endpoints, options: &ConnectOptions) {
    cache().evict(endpoints, options);
    refused_hosts()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(endpoints.host.to_string());
}

fn refused_hosts() -> &'static Mutex<std::collections::BTreeSet<String>> {
    static REFUSED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    REFUSED.get_or_init(Default::default)
}

/// A token for `host` was refused this session and none has been accepted since.
pub fn refused(host: &str) -> bool {
    refused_hosts()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(host)
}

/// A token for `host` was accepted (`GET /user` answered).
pub fn accepted(host: &HostKey) {
    refused_hosts()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&host.to_string());
}

/// `r` in the accounts popover: forget every cached credential.
pub fn clear_cache() {
    cache().clear();
    gh_tokens()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// The command that signs a host in, shown verbatim in notices and the accounts popover.
pub fn login_hint(kind: ProviderKind, host: &HostKey) -> String {
    match kind {
        ProviderKind::GitHub => format!("gh auth login --hostname {host}"),
        ProviderKind::GitLab => format!("glab auth login --hostname {host}"),
        ProviderKind::Gitea | ProviderKind::Forgejo => {
            format!("tea login add --url https://{host}")
        }
    }
}

/// The one sign-in to offer while a host is browsed anonymously: the CLI's login when it is
/// installed, else its token variable (never `gh auth login` to someone without gh).
pub fn anonymous_hint(kind: ProviderKind, host: &HostKey) -> String {
    let cli = match kind {
        ProviderKind::GitHub => crate::transport::Cli::Gh,
        ProviderKind::GitLab => crate::transport::Cli::Glab,
        ProviderKind::Gitea | ProviderKind::Forgejo => crate::transport::Cli::Tea,
    };
    let installed = crate::sandbox::active() || cli.executable().is_some_and(|path| path.is_file());
    let mut hints = sign_in_hints(kind, host);
    if installed || hints.len() < 2 {
        hints.swap_remove(0)
    } else {
        hints.pop().unwrap_or_default()
    }
}

/// Every way to sign in to a host, the CLI's own login first: the copy for an instance that
/// refuses anonymous API calls, and for the accounts popover.
pub fn sign_in_hints(kind: ProviderKind, host: &HostKey) -> Vec<String> {
    let mut hints = vec![login_hint(kind, host)];
    match kind {
        ProviderKind::GitHub => {
            let name = github_env_names(host)[0];
            hints.push(
                if host.host == "github.com" && host.port.is_none_or(|port| port == 443) {
                    format!("{name}=<token>")
                } else {
                    format!("{name}=<token> GH_HOST={host}")
                },
            );
        }
        ProviderKind::GitLab => {
            hints.push(
                if host.host == "gitlab.com" && host.port.is_none_or(|port| port == 443) {
                    "GITLAB_TOKEN=<token>".into()
                } else {
                    format!("GITLAB_TOKEN=<token> GITLAB_HOST={host}")
                },
            );
        }
        ProviderKind::Gitea | ProviderKind::Forgejo => {
            hints.push(format!("fj -H {host} auth login"));
            hints.push(format!("FORGEJO_TOKEN=<token> FORGEJO_URL=https://{host}"));
        }
    }
    hints
}

/// One read or write that needs a credential while a host is browsed anonymously: `what` is
/// the action (`read logs`, `comment`), the copy names the CLI login that fixes it.
pub fn sign_in_to(what: &str, kind: ProviderKind, host: &HostKey) -> ForgeError {
    ForgeError::Unsupported {
        capability: "sign_in",
        reason: {
            let hint = anonymous_hint(kind, host);
            let verb = if hint.contains('=') { "set" } else { "run" };
            format!("Sign in to {what} · {verb} {hint}")
        },
    }
}

/// The uncached lookup, in precedence order.
pub fn lookup(
    endpoints: &Endpoints,
    options: &ConnectOptions,
    env: &Env,
    cancel: &AtomicBool,
) -> CredentialState {
    let mut tried = Vec::new();
    let Some(scope) = Scope::new(endpoints) else {
        return CredentialState::Missing {
            tried,
            hint: crate::transport::INVALID_API_ADDRESS.into(),
        };
    };
    if let Some(credential) = environment_credential(&scope, env, &mut tried) {
        return CredentialState::Present(credential);
    }
    let found = match endpoints.kind {
        ProviderKind::GitHub => github(&scope, options, &mut tried, cancel),
        ProviderKind::GitLab => gitlab(&scope, options, &mut tried, cancel),
        ProviderKind::Gitea | ProviderKind::Forgejo => gitea(&scope, options, &mut tried),
    };
    match found {
        Ok(Some(credential)) => CredentialState::Present(credential),
        Ok(None) => CredentialState::Missing {
            tried,
            hint: saved_address_advice(endpoints, options, env)
                .unwrap_or_else(|| login_hint(endpoints.kind, &endpoints.host)),
        },
        Err(crash) => CredentialState::Crashed(cli::redact(&crash)),
    }
}

pub(crate) const DIRECT_TOKEN_ADVICE: &str =
    "To use your environment token, switch to direct mode (DUSTY_FORGE_TRANSPORT=http).";

pub(crate) fn direct_token_advice(endpoints: &Endpoints, env: &Env) -> Option<&'static str> {
    environment_credential(&Scope::new(endpoints)?, env, &mut Vec::new())
        .map(|_| DIRECT_TOKEN_ADVICE)
}

pub const API_PATH_NOT_FOUND: &str = "Dusty couldn't find the forge API at the configured address.";

/// Whether a sign-in hint is an explanation rather than a command or token example.
pub fn is_sign_in_advice(hint: &str) -> bool {
    hint.starts_with("Your ")
        || hint.starts_with("If you're signed in with ")
        || hint.starts_with(API_PATH_NOT_FOUND)
        || hint.starts_with("glab's saved API address")
        || hint == DIRECT_TOKEN_ADVICE
        || hint == crate::transport::INVALID_API_ADDRESS
}

/// A saved sign-in's routing origin and the REST base safe to show to a user.
pub(crate) struct SignInAddress {
    pub origin: String,
    base: String,
    saved: bool,
}

impl SignInAddress {
    pub(crate) fn new(url: &str, kind: ProviderKind) -> Option<Self> {
        let url = reqwest::Url::parse(url).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let origin = url.origin().ascii_serialization();
        let suffix = crate::detect::rest_api_suffix(kind);
        let path = url.path().trim_end_matches('/');
        // Construct from parsed parts: userinfo, query and fragment are not settings
        // Dusty should show or copy. The installation prefix is independent of the origin gate.
        let base = if kind != ProviderKind::GitHub && !path.ends_with(suffix) {
            format!("{origin}{path}{suffix}")
        } else {
            format!("{origin}{path}")
        };
        Some(Self {
            origin,
            base,
            saved: true,
        })
    }

    pub(crate) fn default_for(endpoints: &Endpoints) -> Option<Self> {
        let url = match endpoints.kind {
            ProviderKind::GitHub => Endpoints::default_github_api_base(&endpoints.host),
            _ => format!("https://{}", endpoints.host),
        };
        let mut address = Self::new(&url, endpoints.kind)?;
        address.saved = false;
        Some(address)
    }
}

/// Whether a hint is an environment-token example, optionally introduced by prose.
pub fn is_token_example(hint: &str) -> bool {
    hint.contains("=<token>")
}

fn scoped_token_example(endpoints: &Endpoints) -> Option<String> {
    let origin = api_origin(&endpoints.api_base)?;
    Some(match endpoints.kind {
        ProviderKind::GitHub => {
            let name = github_env_names(&endpoints.host)[0];
            format!("{name}=<token> GH_HOST={origin}")
        }
        ProviderKind::GitLab => format!("GITLAB_TOKEN=<token> GITLAB_HOST={origin}"),
        ProviderKind::Gitea | ProviderKind::Forgejo => {
            format!("GITEA_SERVER_TOKEN=<token> GITEA_SERVER_URL={origin}")
        }
    })
}

pub(crate) fn address_advice(
    cli: crate::transport::Cli,
    address: &SignInAddress,
    endpoints: &Endpoints,
    env: &Env,
    pinned_login: bool,
) -> String {
    use crate::transport::Cli;
    let base = &address.base;
    let tool = cli.name();
    let mut advice = if address.saved {
        if cli == Cli::Glab {
            "glab's saved API address and Dusty's API address don't match.".to_owned()
        } else {
            "Your saved sign-in and Dusty's API address don't match.".to_owned()
        }
    } else {
        format!(
            "If you're signed in with {tool}, Dusty's API address must match the address {tool} uses."
        )
    };
    if crate::detect::github_api_base_override(endpoints.kind, env).is_some() {
        advice.push_str(&format!(" Set DUSTY_FORGE_API_BASE to {base} or unset it."));
    } else {
        advice.push_str(&format!(
            " Set api_base to {base} in Dusty's forge settings."
        ));
    }
    if address.saved {
        let field = if cli == Cli::Glab {
            "glab's api_host (and api_protocol for a bare host)"
        } else {
            "the tea login's url"
        };
        let suffix = crate::detect::rest_api_suffix(endpoints.kind);
        advice.push_str(&format!(
            " Use {field}, keeping the installation path and adding {suffix} if absent."
        ));
    }
    match cli {
        Cli::Gh | Cli::Glab => {
            let command = login_hint(endpoints.kind, &endpoints.host);
            advice.push_str(&format!(" To sign in to the repository's host, run: {command}"));
        }
        Cli::Tea if pinned_login => advice.push_str(" To use a different tea login, change login in Dusty's forge settings to a matching tea login, or remove login to let Dusty pick one."),
        Cli::Tea => advice.push_str(" To select a matching tea login, set login in Dusty's forge settings."),
    }
    if let Some(example) = scoped_token_example(endpoints) {
        if cli == Cli::Tea {
            advice.push_str(" To use a personal access token instead, create one in the forge's Applications settings.");
        }
        advice.push_str(&format!(
            "\nOr set both variables and use direct mode (DUSTY_FORGE_TRANSPORT=http): {example}"
        ));
    }
    advice
}

fn saved_address_advice(
    endpoints: &Endpoints,
    options: &ConnectOptions,
    env: &Env,
) -> Option<String> {
    use crate::transport::Cli;
    let expected = api_origin(&endpoints.api_base)?;
    match endpoints.kind {
        ProviderKind::GitHub => {
            let address = SignInAddress::default_for(endpoints)?;
            (address.origin != expected && options.gh.as_ref().is_some_and(|gh| gh.is_file()))
                .then(|| address_advice(Cli::Gh, &address, endpoints, env, false))
        }
        ProviderKind::GitLab => {
            let path = options.stores.glab_config.as_deref()?;
            let address = glab_saved_api(path, endpoints).ok().flatten()?;
            (address.origin != expected)
                .then(|| address_advice(Cli::Glab, &address, endpoints, env, false))
        }
        ProviderKind::Gitea | ProviderKind::Forgejo => tea_cli_login(endpoints, options, env).err(),
    }
}

fn environment_credential(
    scope: &Scope<'_>,
    env: &Env,
    tried: &mut Vec<CredentialSource>,
) -> Option<Credential> {
    let endpoints = scope.endpoints;
    let host = &endpoints.host;
    let practice_token = || {
        // The sealed tutor environment supplies both the server and a fake sign-in.
        if env.vars.is_none() && scope.origin.starts_with("http://") {
            crate::sandbox::practice_token(host, &scope.origin)
        } else {
            None
        }
    };
    // The generic token has no provider in its name. Don't infer its provider from a repo.
    let dusty_provider = match env.get("DUSTY_FORGE_PROVIDER") {
        Some(kind) => crate::detect::parse_kind(&kind),
        None => Some(ProviderKind::GitHub),
    };
    if let Some(token) = scope
        .env_token(
            env,
            "DUSTY_FORGE_TOKEN",
            &["DUSTY_FORGE_HOST"],
            dusty_provider
                .filter(|kind| *kind == endpoints.kind)
                .map(public_host),
        )
        .or_else(practice_token)
    {
        return Some(Credential::new(
            token,
            CredentialSource::Env("DUSTY_FORGE_TOKEN"),
            None,
        ));
    }
    tried.push(CredentialSource::Env("DUSTY_FORGE_TOKEN"));
    let mut token_for = |name, selectors: &[&str], public_host| {
        tried.push(CredentialSource::Env(name));
        scope
            .env_token(env, name, selectors, public_host)
            .map(|token| Credential::new(token, CredentialSource::Env(name), None))
    };
    match endpoints.kind {
        ProviderKind::GitHub => github_env_names(host).iter().find_map(|&name| {
            let public = matches!(name, "GH_TOKEN" | "GITHUB_TOKEN").then_some("github.com");
            token_for(name, &["GH_HOST"], public)
        }),
        ProviderKind::GitLab => token_for(
            "GITLAB_TOKEN",
            &["GITLAB_HOST", "GL_HOST"],
            Some("gitlab.com"),
        ),
        ProviderKind::Gitea | ProviderKind::Forgejo => {
            token_for("GITEA_SERVER_TOKEN", &["GITEA_SERVER_URL"], None)
                .or_else(|| {
                    token_for(
                        "FORGEJO_TOKEN",
                        &["GITEA_SERVER_URL", "FORGEJO_URL"],
                        Some("codeberg.org"),
                    )
                })
                .or_else(|| {
                    token_for(
                        "GITEA_TOKEN",
                        &["GITEA_SERVER_URL", "FORGEJO_URL"],
                        Some("gitea.com"),
                    )
                })
        }
    }
}

struct Scope<'a> {
    endpoints: &'a Endpoints,
    origin: String,
}

fn public_host(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::GitHub => "github.com",
        ProviderKind::GitLab => "gitlab.com",
        ProviderKind::Gitea => "gitea.com",
        ProviderKind::Forgejo => "codeberg.org",
    }
}

impl<'a> Scope<'a> {
    fn new(endpoints: &'a Endpoints) -> Option<Self> {
        Some(Self {
            endpoints,
            origin: api_origin(&endpoints.api_base)?,
        })
    }

    fn default_origin(&self, host: &HostKey) -> bool {
        Endpoints::default_api_origin(self.endpoints.kind, host).as_deref()
            == Some(self.origin.as_str())
    }

    fn address_matches(&self, address: &str) -> bool {
        if address.contains("://") {
            api_origin(address).as_deref() == Some(self.origin.as_str())
        } else {
            self.default_origin(&HostKey::new(address))
        }
    }

    fn env_token(
        &self,
        env: &Env,
        name: &'static str,
        selectors: &[&str],
        public_host: Option<&str>,
    ) -> Option<String> {
        let matches = match selectors.iter().find_map(|selector| env.get(selector)) {
            Some(address) => self.address_matches(&address),
            None => public_host.is_some_and(|host| {
                let public = HostKey::new(host);
                Endpoints::default_api_origin(self.endpoints.kind, &self.endpoints.host)
                    == Endpoints::default_api_origin(self.endpoints.kind, &public)
                    && self.default_origin(&public)
            }),
        };
        matches.then(|| env.get(name)).flatten()
    }
}

/// `https://h:1/x` / `h:1` / `h` → the host key.
pub fn url_host(text: &str) -> Option<HostKey> {
    let text = text.trim();
    if text.contains("://") {
        let url = reqwest::Url::parse(text).ok()?;
        let host = url.host_str()?;
        return Some(match url.port() {
            Some(port) => HostKey::new(&format!("{host}:{port}")),
            None => HostKey::new(host),
        });
    }
    let host = text.split('/').next()?.trim();
    (!host.is_empty()).then(|| HostKey::new(host))
}

/// The env variables gh itself honours for this host.
pub fn github_env_names(host: &HostKey) -> &'static [&'static str] {
    if host.host == "github.com" || host.host.ends_with(".ghe.com") {
        &["GH_TOKEN", "GITHUB_TOKEN"]
    } else {
        &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    }
}

fn github(
    scope: &Scope<'_>,
    options: &ConnectOptions,
    tried: &mut Vec<CredentialSource>,
    cancel: &AtomicBool,
) -> Result<Option<Credential>, String> {
    let host = &scope.endpoints.host;
    let Some(gh) = options.gh.as_ref().filter(|_| scope.default_origin(host)) else {
        return Ok(None);
    };
    tried.push(CredentialSource::GhAuthToken);
    Ok(gh_auth_token(gh, host, cancel)?
        .map(|token| Credential::new(token.expose(), CredentialSource::GhAuthToken, None)))
}

type GhTokens = HashMap<(PathBuf, HostKey), (Option<Secret>, Instant)>;

fn gh_tokens() -> &'static Mutex<GhTokens> {
    static TOKENS: OnceLock<Mutex<GhTokens>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

/// `gh auth token --hostname H`: `Ok(None)` when gh has no sign-in for H. Shared by credential
/// lookup and detection (a host gh holds a token for is GitHub), so gh runs once per TTL.
pub fn gh_auth_token(
    gh: &Path,
    host: &HostKey,
    cancel: &AtomicBool,
) -> Result<Option<Secret>, String> {
    let key = (gh.to_path_buf(), host.clone());
    if let Some((token, at)) = gh_tokens()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
    {
        let ttl = if token.is_some() {
            PRESENT_TTL
        } else {
            MISSING_TTL
        };
        if at.elapsed() < ttl {
            return Ok(token.clone());
        }
    }
    let output = run(
        gh,
        &["auth", "token", "--hostname", &host.to_string()],
        cancel,
    )?;
    let token = match output {
        None => None,
        Some((0, stdout, _)) => {
            let token = stdout.trim();
            if token.is_empty() || token.contains(char::is_whitespace) {
                return Err("gh auth token printed no token".into());
            }
            Some(Secret::new(token))
        }
        Some(_) => None,
    };
    gh_tokens()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key, (token.clone(), Instant::now()));
    Ok(token)
}

/// Run a CLI's auth command: `Ok(None)` when it is not installed, `Err` when it crashed.
fn run(
    program: &Path,
    args: &[&str],
    cancel: &AtomicBool,
) -> Result<Option<(i32, String, String)>, String> {
    let spec = cli::ProcessSpec {
        program: program.to_path_buf(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        cwd: std::env::temp_dir(),
        env: cli::stored_sign_in_env(),
        stdin: None,
        timeout: AUTH_COMMAND_TIMEOUT,
        cap: 256 * 1024,
    };
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match cli::run_process(&spec, cancel) {
        Ok(output) => Ok(Some((
            output.code.unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))),
        Err(crate::transport::TransportError::Spawn(_)) => Ok(None),
        Err(error) => Err(format!("{name} {}: {error:?}", args.join(" "))),
    }
}

fn read_store(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(STORE_CAP).read_to_string(&mut text).ok()?;
    Some(text)
}

/// glab's `config.yml`: `hosts.<H>.token` (matching the key or its `api_host`).
fn glab_config(path: &Path, scope: &Scope<'_>) -> Option<(String, Option<String>)> {
    let value = yaml_lite::parse(&read_store(path)?).ok()?;
    value
        .get("hosts")?
        .entries()
        .iter()
        .find_map(|(name, entry)| {
            let token = entry.text("token")?;
            let address = glab_api(name, entry)?;
            (address.origin == scope.origin
                && (HostKey::new(name) == scope.endpoints.host
                    || entry
                        .text("api_host")
                        .and_then(url_host)
                        .is_some_and(|host| host == scope.endpoints.host)))
            .then(|| (token.to_owned(), entry.text("user").map(str::to_owned)))
        })
}

fn glab_api(name: &str, entry: &yaml_lite::Yaml) -> Option<SignInAddress> {
    let address = entry.text("api_host").unwrap_or(name);
    if address.contains("://") {
        SignInAddress::new(address, ProviderKind::GitLab)
    } else {
        let protocol = entry.text("api_protocol").unwrap_or("https");
        SignInAddress::new(&format!("{protocol}://{address}"), ProviderKind::GitLab)
    }
}

/// The API address glab would use for this host, taking saved settings into account.
pub(crate) fn glab_cli_api(
    endpoints: &Endpoints,
    options: &ConnectOptions,
) -> Option<SignInAddress> {
    let configured = options
        .stores
        .glab_config
        .as_deref()
        .filter(|path| path.exists());
    if let Some(path) = configured
        && let Some(address) = glab_saved_api(path, endpoints).ok()?
    {
        return Some(address);
    }
    SignInAddress::default_for(endpoints)
}

fn glab_saved_api(path: &Path, endpoints: &Endpoints) -> Result<Option<SignInAddress>, ()> {
    let config = yaml_lite::parse(&read_store(path).ok_or(())?).map_err(|_| ())?;
    match config.get("hosts").and_then(|hosts| {
        hosts
            .entries()
            .iter()
            .find(|(name, _)| HostKey::new(name) == endpoints.host)
    }) {
        Some((name, entry)) => glab_api(name, entry).map(Some).ok_or(()),
        None => Ok(None),
    }
}

/// The hosts glab's config holds a token for (detection claims).
pub fn glab_hosts(path: &Path) -> Vec<HostKey> {
    let Some(value) = read_store(path).and_then(|text| yaml_lite::parse(&text).ok()) else {
        return Vec::new();
    };
    value
        .get("hosts")
        .map(|hosts| {
            hosts
                .entries()
                .iter()
                .filter(|(_, entry)| entry.text("token").is_some())
                .map(|(name, _)| HostKey::new(name))
                .collect()
        })
        .unwrap_or_default()
}

/// `glab auth status --hostname H --show-token` → (`Token: …`, `Logged in to H as U`).
pub fn parse_glab_status(text: &str) -> (Option<String>, Option<String>) {
    let mut token = None;
    let mut user = None;
    for line in text.lines() {
        let line = line
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
            .trim();
        if let Some(rest) = line.strip_prefix("Token:") {
            let value = rest.trim();
            if !value.is_empty() && !value.chars().all(|c| c == '*') && !value.contains(' ') {
                token = Some(value.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("Logged in to ")
            && let Some((_, account)) = rest.split_once(" as ")
        {
            user = account
                .split_whitespace()
                .next()
                .map(|account| account.trim_end_matches(',').to_owned());
        }
    }
    (token, user)
}

/// The saved CLI token, deliberately ignoring direct-mode environment credentials.
pub(crate) fn glab_saved_token(
    endpoints: &Endpoints,
    options: &ConnectOptions,
    cancel: &AtomicBool,
) -> Option<Secret> {
    gitlab(&Scope::new(endpoints)?, options, &mut Vec::new(), cancel)
        .ok()
        .flatten()
        .map(|credential| credential.token)
}

fn gitlab(
    scope: &Scope<'_>,
    options: &ConnectOptions,
    tried: &mut Vec<CredentialSource>,
    cancel: &AtomicBool,
) -> Result<Option<Credential>, String> {
    let host = &scope.endpoints.host;
    if let Some(path) = &options.stores.glab_config {
        tried.push(CredentialSource::GlabConfig);
        if let Some((token, user)) = glab_config(path, scope) {
            return Ok(Some(Credential::new(
                token,
                CredentialSource::GlabConfig,
                user,
            )));
        }
    }
    let Some(glab) = options.glab.as_ref().filter(|_| {
        glab_cli_api(scope.endpoints, options).is_some_and(|address| address.origin == scope.origin)
    }) else {
        return Ok(None);
    };
    tried.push(CredentialSource::GlabAuthStatus);
    let host = host.to_string();
    match run(
        glab,
        &["auth", "status", "--hostname", &host, "--show-token"],
        cancel,
    )? {
        Some((code, stdout, stderr)) => {
            let (token, user) = parse_glab_status(&format!("{stdout}\n{stderr}"));
            Ok(token
                .filter(|_| code == 0)
                .map(|token| Credential::new(token, CredentialSource::GlabAuthStatus, user)))
        }
        None => Ok(None),
    }
}

/// fj's `keys.json`: `{"hosts": {"codeberg.org": {"type": "Application", "name": "maya", "token": "…"}}}`.
fn fj_keys(path: &Path, scope: &Scope<'_>) -> Option<(String, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(&read_store(path)?).ok()?;
    value["hosts"]
        .as_object()?
        .iter()
        .find_map(|(name, entry)| {
            let token = entry["token"]
                .as_str()
                .map(str::trim)
                .filter(|token| !token.is_empty())?;
            scope
                .address_matches(name)
                .then(|| (token.to_owned(), entry["name"].as_str().map(str::to_owned)))
        })
}

pub fn fj_hosts(path: &Path) -> Vec<HostKey> {
    let Some(value) =
        read_store(path).and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return Vec::new();
    };
    value["hosts"]
        .as_object()
        .map(|hosts| {
            hosts
                .iter()
                .filter(|(_, entry)| {
                    entry["token"]
                        .as_str()
                        .is_some_and(|token| !token.trim().is_empty())
                })
                .filter_map(|(name, _)| url_host(name))
                .collect()
        })
        .unwrap_or_default()
}

/// One tea login: `logins[]` entry with `name`, `url`, `token`, `user`, `default`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeaLogin {
    pub name: String,
    pub host: HostKey,
    pub user: Option<String>,
    pub default: bool,
    pub has_token: bool,
}

fn tea_entries(path: &Path) -> Option<yaml_lite::Yaml> {
    yaml_lite::parse(&read_store(path)?).ok()
}

pub fn tea_logins(path: &Path) -> Vec<TeaLogin> {
    let Some(value) = tea_entries(path) else {
        return Vec::new();
    };
    value
        .get("logins")
        .map(|logins| {
            logins
                .as_list()
                .iter()
                .filter_map(|login| {
                    Some(TeaLogin {
                        name: login.text("name")?.to_owned(),
                        host: url_host(login.text("url")?)?,
                        user: login.text("user").map(str::to_owned),
                        default: login.text("default") == Some("true"),
                        has_token: login.text("token").is_some(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The tea login for a host: the `default` one when several match, else the first.
pub fn tea_login(path: &Path, host: &HostKey) -> Option<TeaLogin> {
    let logins: Vec<TeaLogin> = tea_logins(path)
        .into_iter()
        .filter(|login| login.host == *host)
        .collect();
    logins
        .iter()
        .find(|login| login.default)
        .or_else(|| logins.first())
        .cloned()
}

fn tea_token(path: &Path, scope: &Scope<'_>) -> Option<(String, String, Option<String>)> {
    let value = tea_entries(path)?;
    let mut matches = value.get("logins")?.as_list().iter().filter(|entry| {
        entry
            .text("url")
            .is_some_and(|url| scope.address_matches(url))
    });
    let first = matches.next()?;
    let entry = if first.text("default") == Some("true") {
        first
    } else {
        matches
            .find(|entry| entry.text("default") == Some("true"))
            .unwrap_or(first)
    };
    Some((
        entry.text("token")?.to_owned(),
        entry.text("name")?.to_owned(),
        entry.text("user").map(str::to_owned),
    ))
}

/// Pick the configured name or a login for this API origin, then require a unique name for tea.
pub(crate) fn tea_cli_login(
    endpoints: &Endpoints,
    options: &ConnectOptions,
    env: &Env,
) -> Result<Option<TeaLogin>, String> {
    let Some((login, address)) = selected_tea_login(endpoints, options) else {
        return Ok(None);
    };
    if api_origin(&endpoints.api_base).as_deref() != Some(address.origin.as_str()) {
        if !address.saved {
            return Ok(None);
        }
        return Err(address_advice(
            crate::transport::Cli::Tea,
            &address,
            endpoints,
            env,
            options.logins.contains_key(&endpoints.host),
        ));
    }
    Ok(Some(login))
}

fn selected_tea_login(
    endpoints: &Endpoints,
    options: &ConnectOptions,
) -> Option<(TeaLogin, SignInAddress)> {
    let scope = Scope::new(endpoints)?;
    let value = tea_entries(options.stores.tea_config.as_deref()?)?;
    let entries = value.get("logins")?.as_list();
    let by_origin = |entry: &&yaml_lite::Yaml| {
        entry
            .text("url")
            .is_some_and(|url| scope.address_matches(url))
    };
    let pinned = options.logins.get(&endpoints.host);
    let candidate = match pinned {
        Some(name) => entries
            .iter()
            .find(|entry| entry.text("name") == Some(name.as_str())),
        None => preferred_tea_login(entries.iter().filter(by_origin)).or_else(|| {
            preferred_tea_login(entries.iter().filter(|entry| {
                entry.text("url").and_then(url_host).as_ref() == Some(&endpoints.host)
            }))
        }),
    };
    let entry = candidate?;
    let name = entry.text("name")?;
    let url = entry.text("url")?;
    if entries
        .iter()
        .filter(|entry| entry.text("name") == Some(name))
        .count()
        != 1
    {
        return None;
    }
    let host = url_host(url)?;
    let address = if url.contains("://") {
        SignInAddress::new(url, endpoints.kind)?
    } else {
        let mut address = SignInAddress::new(&format!("https://{host}"), endpoints.kind)?;
        address.saved = false;
        address
    };
    Some((
        TeaLogin {
            name: name.to_owned(),
            host,
            user: entry.text("user").map(str::to_owned),
            default: entry.text("default") == Some("true"),
            has_token: entry.text("token").is_some(),
        },
        address,
    ))
}

/// A viewer 404 can mean the API is mounted under the saved installation prefix. This
/// diagnosis does not change which origins may receive credentials.
pub(crate) fn rest_path_advice(endpoints: &Endpoints, options: &ConnectOptions) -> Option<String> {
    let saved = match endpoints.kind {
        ProviderKind::GitHub => return None,
        ProviderKind::GitLab => {
            glab_saved_api(options.stores.glab_config.as_deref()?, endpoints).ok()??
        }
        ProviderKind::Gitea | ProviderKind::Forgejo => selected_tea_login(endpoints, options)?.1,
    };
    let active = reqwest::Url::parse(&endpoints.api_base).ok()?;
    if !saved.saved
        || saved.origin != active.origin().ascii_serialization()
        || saved.base == format!("{}{}", saved.origin, active.path().trim_end_matches('/'))
    {
        return None;
    }
    Some(format!(
        "{API_PATH_NOT_FOUND} Set api_base to {}",
        saved.base
    ))
}

fn preferred_tea_login<'a>(
    mut entries: impl Iterator<Item = &'a yaml_lite::Yaml>,
) -> Option<&'a yaml_lite::Yaml> {
    let first = entries.next()?;
    if first.text("default") == Some("true") {
        Some(first)
    } else {
        entries
            .find(|entry| entry.text("default") == Some("true"))
            .or(Some(first))
    }
}

fn gitea(
    scope: &Scope<'_>,
    options: &ConnectOptions,
    tried: &mut Vec<CredentialSource>,
) -> Result<Option<Credential>, String> {
    for path in &options.stores.fj_keys {
        tried.push(CredentialSource::FjKeys);
        if let Some((token, user)) = fj_keys(path, scope) {
            return Ok(Some(Credential::new(token, CredentialSource::FjKeys, user)));
        }
    }
    if let Some(path) = &options.stores.tea_config {
        if let Some((token, name, user)) = tea_token(path, scope) {
            return Ok(Some(Credential::new(
                token,
                CredentialSource::TeaConfig(name),
                user,
            )));
        }
        tried.push(CredentialSource::TeaConfig(String::new()));
    }
    Ok(None)
}

/// Which provider a CLI's stored credential says an unknown host is. Files first (cheap), then
/// `gh auth token`; host-scoped env variables count as the CLI's own.
pub fn claim(
    host: &HostKey,
    options: &ConnectOptions,
    env: &Env,
    cancel: &AtomicBool,
) -> Option<(ProviderKind, CredentialSource)> {
    if env
        .get("GH_HOST")
        .is_some_and(|scope| HostKey::new(&scope) == *host)
        && let Some(name) = ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_TOKEN"]
            .into_iter()
            .find(|name| env.get(name).is_some())
    {
        return Some((ProviderKind::GitHub, CredentialSource::Env(name)));
    }
    for scope in ["GITLAB_HOST", "GL_HOST"] {
        if env
            .get(scope)
            .and_then(|scope| url_host(&scope))
            .is_some_and(|scope| scope == *host)
            && env.get("GITLAB_TOKEN").is_some()
        {
            return Some((ProviderKind::GitLab, CredentialSource::Env("GITLAB_TOKEN")));
        }
    }
    if env
        .get("GITEA_SERVER_URL")
        .and_then(|scope| url_host(&scope))
        .is_some_and(|scope| scope == *host)
        && env.get("GITEA_SERVER_TOKEN").is_some()
    {
        return Some((
            ProviderKind::Gitea,
            CredentialSource::Env("GITEA_SERVER_TOKEN"),
        ));
    }
    if let Some(path) = &options.stores.glab_config
        && glab_hosts(path).contains(host)
    {
        return Some((ProviderKind::GitLab, CredentialSource::GlabConfig));
    }
    if options
        .stores
        .fj_keys
        .iter()
        .any(|path| fj_hosts(path).contains(host))
    {
        return Some((ProviderKind::Forgejo, CredentialSource::FjKeys));
    }
    if let Some(path) = &options.stores.tea_config
        && let Some(login) = tea_login(path, host)
    {
        return Some((ProviderKind::Gitea, CredentialSource::TeaConfig(login.name)));
    }
    if let Some(gh) = &options.gh
        && matches!(gh_auth_token(gh, host, cancel), Ok(Some(_)))
    {
        return Some((ProviderKind::GitHub, CredentialSource::GhAuthToken));
    }
    if let Some(glab) = &options.glab
        && let Ok(Some((0, stdout, stderr))) = run(
            glab,
            &["auth", "status", "--hostname", &host.to_string()],
            cancel,
        )
        && parse_glab_status(&format!("{stdout}\n{stderr}"))
            .1
            .is_some()
    {
        return Some((ProviderKind::GitLab, CredentialSource::GlabAuthStatus));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new("ghp_abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(format!("{secret:?} {secret}"), "<redacted> <redacted>");
        let credential = Credential::new(secret.expose(), CredentialSource::GhAuthToken, None);
        assert!(!format!("{credential:?}").contains("ghp_"));
        let env = Env::from_pairs([("GITLAB_TOKEN", "glpat-should-not-print")]);
        assert!(!format!("{env:?}").contains("should-not-print"));
    }

    #[test]
    fn enterprise_hosts_use_enterprise_tokens() {
        assert_eq!(
            github_env_names(&HostKey::new("github.com")),
            &["GH_TOKEN", "GITHUB_TOKEN"]
        );
        assert_eq!(
            github_env_names(&HostKey::new("git.corp.example")),
            &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
        );
    }

    #[test]
    fn glab_status_text_yields_token_and_account() {
        let text = "gitlab.com\n  ✓ Logged in to gitlab.com as maya (/home/m/.config/glab-cli/config.yml)\n  ✓ Git operations for gitlab.com configured to use ssh protocol.\n  ✓ Token: glpat-0123456789abcdefghij\n";
        assert_eq!(
            parse_glab_status(text),
            (
                Some("glpat-0123456789abcdefghij".into()),
                Some("maya".into())
            )
        );
        let masked = "  ✓ Logged in to gitlab.com as maya\n  ✓ Token: **************************\n";
        assert_eq!(parse_glab_status(masked).0, None);
    }

    fn mismatch_hint(state: CredentialState) -> String {
        match state {
            CredentialState::Missing { hint, .. } => hint,
            state => panic!("expected a missing sign-in, got {state:?}"),
        }
    }

    #[test]
    fn split_glab_api_hint_uses_the_saved_installation_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("glab.yml");
        std::fs::write(
            &config,
            "hosts:\n  git.example.test:\n    api_host: https://forge.example.test/tools/code\n",
        )
        .unwrap();
        let env = Env::from_pairs([] as [(&str, &str); 0]);
        let endpoints = crate::detect::endpoints_for(
            ProviderKind::GitLab,
            &HostKey::new("git.example.test"),
            Some("https://git.example.test/stale/api/v4"),
            &env,
        );
        let options = ConnectOptions {
            stores: Stores {
                glab_config: Some(config),
                ..Stores::default()
            },
            ..ConnectOptions::default()
        };
        let hint = mismatch_hint(lookup(&endpoints, &options, &env, &AtomicBool::new(false)));
        assert!(hint.contains("Set api_base to https://forge.example.test/tools/code/api/v4 "));
        assert!(hint.contains("glab's api_host") && hint.contains("adding /api/v4"));
        assert!(hint.contains("glab auth login --hostname git.example.test"));
        assert!(!hint.contains("/stale/"));
    }

    #[test]
    fn pinned_tea_mismatch_names_login_and_offers_a_token_for_the_current_api() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("tea.yml");
        std::fs::write(
            &config,
            "logins:\n  - name: work\n    url: https://old.example.test/tools/code\n  - name: other\n    url: https://api.example.test:8443\n",
        )
        .unwrap();
        let env = Env::from_pairs([] as [(&str, &str); 0]);
        let host = HostKey::new("git.example.test");
        let endpoints = crate::detect::endpoints_for(
            ProviderKind::Gitea,
            &host,
            Some("https://api.example.test:8443/api/v1"),
            &env,
        );
        let mut options = ConnectOptions {
            stores: Stores {
                tea_config: Some(config),
                ..Stores::default()
            },
            logins: BTreeMap::from([(host.clone(), "work".into())]),
            ..ConnectOptions::default()
        };
        let hint = tea_cli_login(&endpoints, &options, &env).unwrap_err();
        assert!(hint.contains("Set api_base to https://old.example.test/tools/code/api/v1 "));
        assert!(hint.contains("change login") && hint.contains("or remove login"));
        assert!(hint.contains("the tea login's url") && hint.contains("adding /api/v1"));
        assert!(
            hint.contains(
                "GITEA_SERVER_TOKEN=<token> GITEA_SERVER_URL=https://api.example.test:8443"
            )
        );
        options.logins.remove(&host);
        let selected = tea_cli_login(&endpoints, &options, &env).unwrap().unwrap();
        assert_eq!(selected.name, "other");
    }

    #[test]
    fn advice_names_the_github_environment_override_without_echoing_its_value() {
        let fake = tempfile::NamedTempFile::new().unwrap();
        let env = Env::from_pairs([("DUSTY_FORGE_API_BASE", "https://api.example.test/opaque")]);
        let endpoints = crate::detect::endpoints_for(
            ProviderKind::GitHub,
            &HostKey::new("github.com"),
            Some("https://settings.example.test"),
            &env,
        );
        let options = ConnectOptions {
            gh: Some(fake.path().to_owned()),
            ..ConnectOptions::default()
        };
        let hint = mismatch_hint(lookup(&endpoints, &options, &env, &AtomicBool::new(false)));
        assert!(hint.contains("Set DUSTY_FORGE_API_BASE to https://api.github.com or unset it."));
        assert!(hint.contains("gh auth login --hostname github.com"));
        assert!(!hint.contains("Your saved sign-in"));
        assert!(!hint.contains("Set api_base") && !hint.contains("/opaque"));
    }

    #[test]
    fn rendered_advice_keeps_the_saved_path_but_omits_userinfo_query_and_fragment() {
        let env = Env::from_pairs([] as [(&str, &str); 0]);
        let endpoints = crate::detect::endpoints_for(
            ProviderKind::GitLab,
            &HostKey::new("git.example.test"),
            Some("https://git.example.test:8443/api/v4"),
            &env,
        );
        for path in ["/tools/code", "/tools/code/api/v4/"] {
            let url = format!(
                "https://secret-user:secret-password@forge.example.test:9443{path}?private_token=secret-query#secret-fragment"
            );
            let address = SignInAddress::new(&url, endpoints.kind).unwrap();
            let hint = address_advice(
                crate::transport::Cli::Glab,
                &address,
                &endpoints,
                &env,
                false,
            );
            let error = ForgeError::NotAuthenticated {
                host: endpoints.host.to_string(),
                hint,
            };
            let rendered = error.to_string();
            let state =
                crate::host_state::HostState::from_error(Some(endpoints.kind), &error).unwrap();
            let account = crate::host_state::Account::new(
                endpoints.host.clone(),
                Some(endpoints.kind),
                state,
            );
            for text in
                std::iter::once(rendered).chain(account.lines().into_iter().map(|(text, _)| text))
            {
                assert!(!text.contains("secret-") && !text.contains("private_token"));
            }
            assert!(
                error.to_string().contains(
                    "Set api_base to https://forge.example.test:9443/tools/code/api/v4 in"
                )
            );
        }
    }

    #[test]
    fn url_hosts_keep_ports_and_drop_paths() {
        assert_eq!(
            url_host("https://Codeberg.org/"),
            Some(HostKey::new("codeberg.org"))
        );
        assert_eq!(
            url_host("https://git.corp:8443/gitlab"),
            Some(HostKey::new("git.corp:8443"))
        );
        assert_eq!(url_host("codeberg.org"), Some(HostKey::new("codeberg.org")));
    }
}
