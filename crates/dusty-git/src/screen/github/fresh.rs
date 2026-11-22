//! Caching and freshness in the GitHub workspace: paint from the disk cache at
//! once, revalidate in the background, merge without moving the reader, warm what the user is
//! likely to open next on a low-priority worker, search locally while the remote search runs,
//! and show writes before the forge confirms them.
//!
//! All of it is on only when the disk cache is (`forge::cache::Cache::shared`); without it the
//! workspace behaves exactly as before: memory only, explicit refreshes, no warm worker.
use super::*;
use crate::forge::ForgeJobs;
use crate::forge::cache::{self, Cache, CacheKey};
use crate::forge::merge;
use crate::forge::model::Fingerprint;
use crate::forge::provider::{Conditional, Fetched};
use crate::forge::records;
use crate::forge::request::{ForgeError, Payload};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Interactive work must have been idle this long before the warm worker starts a request.
const WARM_IDLE: Duration = Duration::from_millis(150);
/// The cursor rests this long on a row before its neighbours' details are prefetched.
const PREFETCH_REST: Duration = Duration::from_millis(120);
/// How long one turn (a key, a poll) may wait for the disk in all (`load_disk_soon`).
const DISK_BUDGET: Duration = Duration::from_millis(8);
/// The cursor rests this long on a row before the forge is asked for its details: `j`/`k`
/// through the list paint cached details and send nothing for the rows passed over.
const DWELL: Duration = Duration::from_millis(150);
/// Live search waits for typing to pause.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Visible entries are checked against their budget this often.
const SWEEP: Duration = Duration::from_secs(1);
/// An entry loaded from disk is confirmed once per session unless it is younger than this.
const CONFIRM_AFTER: Duration = Duration::from_secs(10);
/// Local hits shown while the remote search runs.
const LOCAL_HITS: usize = 50;

pub(super) struct Meta {
    pub(super) fetched: SystemTime,
    /// When this session last asked the forge (answered or not).
    pub(super) checked: Option<Instant>,
    pub(super) ttl: Duration,
    pub(super) etag: Option<String>,
    pub(super) fingerprint: Option<Fingerprint>,
    /// The last background check failed; the entry is shown as cached.
    pub(super) offline: bool,
    /// The forge has confirmed this entry during this session.
    pub(super) verified: bool,
}

impl Meta {
    fn age(&self) -> Duration {
        self.fetched.elapsed().unwrap_or_default()
    }
}

pub(super) struct Live {
    pub(super) page: usize,
    pub(super) query: String,
    pub(super) state: String,
    changed: Instant,
    sent: bool,
    corpus: Option<Vec<Value>>,
}

pub(super) struct LiveRows {
    pub(super) local: Vec<Value>,
    pub(super) remote: Vec<Value>,
    pub(super) divider: String,
}

type WarmScope = (String, String, u64, Option<AccountNamespace>);
/// One optimistic change to a cached value.
type Edit = Box<dyn Fn(&mut Value)>;

pub(super) struct Fresh {
    cache: Option<Arc<Cache>>,
    pub(super) meta: HashMap<Key, Meta>,
    /// Background revalidations in flight: merged quietly, failures only mark the entry.
    pub(super) implicit: HashSet<Key>,
    warm: ForgeJobs,
    warm_next: u64,
    warm_active: Option<Request>,
    warm_queue: VecDeque<Request>,
    warmed: Option<WarmScope>,
    idle_since: Option<Instant>,
    sweep: Instant,
    /// Row identities that changed in the last merge; highlighted for one repaint.
    pub(super) flash: HashSet<String>,
    prefetched: HashSet<Key>,
    rest: (usize, usize, Instant),
    /// The cursor moved to the row in `rest` from another row of the same list.
    moving: bool,
    /// `ensure` runs for a row the cursor is passing over: details are read from disk only.
    pub(super) holding: bool,
    /// The freshness chips last painted: their ages change with no reply to repaint them.
    chips: Vec<Option<String>>,
    remote: Option<Option<(String, String)>>,
    pub(super) live: Option<Live>,
    pub(super) live_key: Option<Key>,
    /// Values replaced by an optimistic write, restored if the forge refuses it.
    undo: Vec<Undo>,
    /// Gitea's and Forgejo's list check marks, asked for the rows on screen.
    pub(super) rows: super::row_checks::Rows,
    /// Disk reads for the rows the cursor passes over, off the UI thread.
    disk: super::disk::Disk,
    /// Until when this turn (a key, a poll) may wait for the disk (`load_disk_soon`).
    pub(super) disk_until: Option<Instant>,
    /// Keys the disk had nothing for, in scope `.0`: not read again each time `ensure`
    /// asks (the badges ask on every key), until the forge answers them.
    disk_absent: (u64, HashSet<Key>),
    /// The warm-up's keys still being read from disk: queued for the forge once the disk says
    /// whether it holds them, fresh.
    warm_disk: Vec<Request>,
}

struct Undo {
    key: Key,
    scope: CacheScope,
    before: Value,
}

impl Fresh {
    pub(super) fn new() -> Self {
        Self::with(Cache::shared())
    }

    pub(super) fn with(cache: Option<Arc<Cache>>) -> Self {
        Self {
            cache,
            meta: HashMap::new(),
            implicit: HashSet::new(),
            warm: ForgeJobs::warm(),
            warm_next: 0,
            warm_active: None,
            warm_queue: VecDeque::new(),
            warmed: None,
            idle_since: None,
            sweep: Instant::now(),
            flash: HashSet::new(),
            prefetched: HashSet::new(),
            rest: (0, usize::MAX, Instant::now()),
            moving: false,
            holding: false,
            chips: Vec::new(),
            remote: None,
            live: None,
            live_key: None,
            undo: Vec::new(),
            rows: Default::default(),
            disk: Default::default(),
            disk_until: None,
            disk_absent: Default::default(),
            warm_disk: Vec::new(),
        }
    }

    /// A background read for the warm worker, ahead of the startup warm-up.
    pub(super) fn push_warm(&mut self, request: Request) {
        self.warm_queue.push_front(request);
    }

    pub(super) fn enabled(&self) -> bool {
        self.cache.is_some()
    }

    /// Abandon pending checks without dropping any cached data.
    pub(super) fn abandon_checks(&mut self) {
        if self.warm_active.take().is_some() {
            self.warm.cancel();
        }
        self.implicit.clear();
        self.warm_queue.clear();
        self.warm_disk.clear();
    }

    /// A different repository: nothing known about the old one applies.
    pub(super) fn forget(&mut self) {
        self.abandon_checks();
        self.meta.clear();
        self.remote = None;
        self.rows = Default::default();
        self.prefetched.clear();
        self.flash.clear();
        self.live = None;
        self.live_key = None;
        self.undo.clear();
    }

    pub(super) fn uncheck(&mut self, key: &Key) {
        if self.implicit.remove(key)
            && let Some(meta) = self.meta.get_mut(key)
        {
            meta.checked = None;
        }
    }

    pub(super) fn retain_list_metadata(&mut self) {
        self.meta
            .retain(|key, _| matches!(key, Key::Repository | Key::List(..) | Key::Inbox(..)));
        self.prefetched.clear();
    }

    pub(super) fn drop_warm(&mut self, key: &Key) {
        self.warm_queue.retain(|request| &request.key != key);
    }
}

fn kind_of(page: usize) -> ItemKind {
    if page == 1 {
        ItemKind::Issue
    } else {
        ItemKind::PullRequest
    }
}

/// A list's age column in one relative format from minutes to years (`3w ago`, `2mo ago`,
/// `1y ago`), so a list never mixes `2d ago` with `Nov 6` and `Aug 20 2025`.
pub(super) fn relative(stamp: &str, now: i64) -> String {
    let Some(at) = records::epoch(stamp) else {
        return String::new();
    };
    let seconds = (now - at).max(0);
    let days = seconds / 86_400;
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else if days < 14 {
        format!("{days}d ago")
    } else if days < 60 {
        format!("{}w ago", days / 7)
    } else if days < 365 {
        format!("{}mo ago", days / 30)
    } else {
        format!("{}y ago", days / 365)
    }
}

fn ago(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 45 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", (seconds / 60).max(1))
    } else if seconds < 48 * 3600 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Whether a list row (legacy shape) matches a GitHub-style query and state, as far as the
/// cached row can tell. Qualifiers the row cannot answer are left to the remote search.
pub(super) fn matches(row: &Value, query: &str, state: &str) -> bool {
    let row_state = string(row, "state");
    let draft = row["isDraft"].as_bool() == Some(true);
    let state_ok = match state {
        "open" => row_state == "OPEN",
        "closed" => matches!(row_state, "CLOSED" | "MERGED"),
        "merged" => row_state == "MERGED",
        _ => true,
    };
    if !state_ok {
        return false;
    }
    let title = string(row, "title").to_lowercase();
    let author = login(row).to_lowercase();
    let labels = array(row, "labels")
        .iter()
        .map(|label| string(label, "name").to_lowercase())
        .collect::<Vec<_>>();
    query.split_whitespace().all(|term| {
        let term = term.to_lowercase();
        let (negated, term) = match term.strip_prefix('-') {
            Some(rest) if rest.contains(':') => (true, rest.to_owned()),
            _ => (false, term),
        };
        let verdict = match term.split_once(':') {
            Some(("is" | "state", value)) => match value {
                "open" => row_state == "OPEN",
                "closed" => matches!(row_state, "CLOSED" | "MERGED"),
                "merged" => row_state == "MERGED",
                "draft" => draft,
                _ => true,
            },
            Some(("draft", value)) => draft == (value == "true"),
            Some(("author", value)) => author == value.trim_start_matches('@'),
            Some(("label", value)) => {
                let value = value.trim_matches('"');
                labels.iter().any(|label| label == value)
            }
            Some(("no", "label")) => labels.is_empty(),
            Some((_, _)) => return true,
            None => {
                let digits = term.trim_start_matches('#');
                title.contains(term.as_str())
                    || !digits.is_empty()
                        && digits.chars().all(|c| c.is_ascii_digit())
                        && number(row, "number").to_string() == digits
                    || author == term
            }
        };
        verdict != negated
    })
}

impl Workspace {
    pub(super) fn warm_pending(&self, key: &Key) -> bool {
        self.fresh
            .warm_active
            .as_ref()
            .is_some_and(|request| &request.key == key && self.request_is_current(request))
    }

    /// Whether the warm worker holds a current read whose key matches.
    pub(super) fn warm_has(&self, matches: impl Fn(&Key) -> bool) -> bool {
        self.fresh
            .warm_active
            .iter()
            .chain(&self.fresh.warm_queue)
            .any(|request| matches(&request.key) && self.request_is_current(request))
    }

    /// The cache entry a read lands in: the target's host and repository, or the remote's.
    fn cache_key(&mut self, op: &Op) -> Option<CacheKey> {
        let Op::Read(target, read) = op else {
            return None;
        };
        let (host, repo) = match &target.host {
            Some(host) => (host.clone(), target.repo.clone()),
            None => {
                let remote = self.fresh.remote.get_or_insert_with(|| {
                    crate::forge::detect::choose_remote(&self.root)
                        .ok()
                        .map(|remote| (remote.host.to_string(), remote.repo.full()))
                });
                let (host, repo) = remote.clone()?;
                (host, Some(repo))
            }
        };
        let host = HostKey::new(&host);
        if self.account.is_none() && !self.skip_remembered_account {
            self.skip_remembered_account = true;
            self.discover_account(
                crate::forge::host_state::remembered_account(&host).map(|(account, _)| account),
            );
        }
        let account = self.account.as_ref()?;
        (host == *account.host()).then(|| CacheKey::for_read(account, repo.as_deref(), read))
    }

    /// Fill a memory miss from disk within this turn's budget (`load_disk_soon`). Returns
    /// whether the key is cached now: `false` while the disk is still being read, and the
    /// key's handling runs again when it lands (`poll_disk`).
    pub(super) fn load_disk(&mut self, key: &Key, op: &Op) -> bool {
        self.load_disk_soon(key, op).unwrap_or(false)
    }

    /// Fill a memory miss from disk on this thread, however long the disk takes: a live
    /// search, which asks the forge at once for whatever the disk lacks.
    /// Returns whether the key is now cached.
    pub(super) fn load_disk_now(&mut self, key: &Key, op: &Op) -> bool {
        let Some((read, cache_key, cache)) = self.disk_entry(key, op) else {
            return self.cache.contains_key(key);
        };
        let found = super::disk::read(&cache, key, &cache_key);
        self.apply_disk(key, &read, cache_key.account.host(), found)
    }

    /// `load_disk` on the disk worker: the entry is painted when it lands (`poll_disk`), and
    /// the key's handling (`ensure`) runs again then. `None` while the disk is being read,
    /// else whether the key is cached now.
    pub(super) fn load_disk_later(&mut self, key: &Key, op: &Op) -> Option<bool> {
        let Some((read, cache_key, cache)) = self.disk_entry(key, op) else {
            return Some(self.cache.contains_key(key));
        };
        self.fresh
            .disk
            .load(&cache, key.clone(), read, cache_key, self.scope_epoch);
        None
    }

    /// `load_disk` through the disk worker, waiting for it while this turn's budget
    /// (`DISK_BUDGET`) lasts: a quick disk paints at once as a direct read would, and a slow
    /// one never holds up a key (its entry paints when it lands, `poll_disk`). `None` while
    /// the disk is still being read, else whether the key is cached now.
    pub(super) fn load_disk_soon(&mut self, key: &Key, op: &Op) -> Option<bool> {
        let Some((read, cache_key, cache)) = self.disk_entry(key, op) else {
            return Some(self.cache.contains_key(key));
        };
        let epoch = self.scope_epoch;
        self.fresh
            .disk
            .load(&cache, key.clone(), read.clone(), cache_key.clone(), epoch);
        let until = *self
            .fresh
            .disk_until
            .get_or_insert_with(|| Instant::now() + DISK_BUDGET);
        let loaded = self.fresh.disk.wait(key, until)?;
        if !self.disk_read_is_current(&loaded) {
            // An answer for the scope before: ask again for this one.
            self.fresh
                .disk
                .load(&cache, key.clone(), read, cache_key, epoch);
            return None;
        }
        Some(self.apply_disk(
            &loaded.key,
            &loaded.read,
            loaded.cache_key.account.host(),
            loaded.found,
        ))
    }

    /// Whether the disk is being read for `key`.
    pub(super) fn disk_loading(&self, key: &Key) -> bool {
        self.fresh.disk.loading.contains(key)
    }

    /// Entries the disk worker read; returns whether any was painted.
    pub(super) fn poll_disk(&mut self) -> bool {
        let mut painted = false;
        let mut answered = false;
        for loaded in self.fresh.disk.replies() {
            answered = true;
            if !self.disk_read_is_current(&loaded) || self.cache.contains_key(&loaded.key) {
                continue;
            }
            painted |= self.apply_disk(
                &loaded.key,
                &loaded.read,
                loaded.cache_key.account.host(),
                loaded.found,
            );
        }
        // The rows' handling waited for the disk: the forge is asked now if it must be.
        if painted || answered && !self.cursor_moving() {
            self.ensure(false);
        }
        painted
    }

    fn disk_read_is_current(&self, loaded: &super::disk::Loaded) -> bool {
        if loaded.epoch != self.scope_epoch {
            return false;
        }
        if !Self::is_repository_read(&loaded.key, &loaded.read) {
            return true;
        }
        let cache_key = &loaded.cache_key;
        self.account.as_ref() == Some(&cache_key.account)
            && cache_key.account.host() == &HostKey::new(&self.host)
            && cache_key
                .repo
                .as_deref()
                .is_some_and(|repo| repo.eq_ignore_ascii_case(self.read_repository(&loaded.key)))
    }

    /// What a disk read of `key` needs, when the memory misses it and the cache is on.
    fn disk_entry(&mut self, key: &Key, op: &Op) -> Option<(Read, CacheKey, Arc<Cache>)> {
        if !self.fresh.enabled() || self.cache.contains_key(key) {
            return None;
        }
        if self.fresh.disk_absent.0 != self.scope_epoch {
            self.fresh.disk_absent = (self.scope_epoch, HashSet::new());
        }
        if self.fresh.disk_absent.1.contains(key) {
            return None;
        }
        let Op::Read(target, read) = op else {
            return None;
        };
        if matches!(read, Read::Fingerprint(_))
            || Self::is_repository_read(key, read) && !self.read_target_is_current(key, target)
        {
            return None;
        }
        let cache_key = self.cache_key(op)?;
        let cache = self.fresh.cache.clone()?;
        Some((read.clone(), cache_key, cache))
    }

    /// Put what the disk held for `key` in memory. Returns whether the key is now cached.
    fn apply_disk(
        &mut self,
        key: &Key,
        read: &Read,
        routing_host: &HostKey,
        found: super::disk::Found,
    ) -> bool {
        let envelope = match found {
            super::disk::Found::Payload(envelope) => envelope,
            super::disk::Found::Missing(envelope) => {
                // A "not found" the forge gave last time, shown until it is asked again.
                self.fresh.meta.insert(
                    key.clone(),
                    Meta {
                        fetched: envelope.fetched(),
                        checked: None,
                        ttl: Duration::from_secs(envelope.ttl),
                        etag: None,
                        fingerprint: None,
                        offline: false,
                        verified: false,
                    },
                );
                self.cache
                    .insert(key.clone(), Err(crate::forge::redact(&envelope.data)));
                return true;
            }
            super::disk::Found::Nothing => {
                if self.fresh.disk_absent.0 == self.scope_epoch {
                    self.fresh.disk_absent.1.insert(key.clone());
                }
                return false;
            }
        };
        self.note_totals(key, &envelope.data);
        let (mut value, text) = crate::forge::legacy::encode(shape(key), &envelope.data);
        if let Key::Inbox(host, page, state, _) = key
            && inbox::validate_rows(
                &mut value,
                host,
                self.advertised_web_host().as_ref(),
                *page,
                state,
            )
            .is_err()
        {
            return false;
        }
        if *key == Key::Repository && !self.apply_repository(&value, Some(routing_host)) {
            return false;
        }
        if let Read::List { page, .. } | Read::Search { page, .. } = read {
            self.list_limits.insert(key.clone(), page.per_page as usize);
        }
        self.fresh.meta.insert(
            key.clone(),
            Meta {
                fetched: envelope.fetched(),
                checked: None,
                ttl: Duration::from_secs(envelope.ttl),
                etag: envelope.etag.clone(),
                fingerprint: envelope.fingerprint.clone(),
                offline: false,
                verified: false,
            },
        );
        self.cache.insert(key.clone(), Ok(Reply { text, value }));
        true
    }

    /// Stale-while-revalidate: whether a cached entry should be asked about again.
    pub(super) fn needs_check(&self, key: &Key) -> bool {
        if !self.fresh.enabled() {
            return false;
        }
        let Some(meta) = self.fresh.meta.get(key) else {
            return false;
        };
        // The last check failed (offline, rate limited, a server error): retry with backoff, a
        // quarter of how old the shown data is, from 10 s up to 5 min.
        if meta.offline {
            let wait = (meta.age() / 4).clamp(CONFIRM_AFTER, Duration::from_secs(300));
            return meta.checked.is_none_or(|checked| checked.elapsed() >= wait);
        }
        // Check the remembered identity in the background even when the repository body
        // is fresh. The warm namespace is only provisional until a live request answers.
        if *key == Key::Repository && !meta.verified && meta.checked.is_none() {
            return true;
        }
        if let Some(checked) = meta.checked
            && checked.elapsed() < meta.ttl.clamp(CONFIRM_AFTER, Duration::from_secs(3600))
        {
            return false;
        }
        meta.age() >= meta.ttl || meta.checked.is_none() && meta.age() >= CONFIRM_AFTER
    }

    /// Queue a background check of a cached entry; the cached value stays on screen.
    pub(super) fn revalidate(&mut self, key: Key, op: Op) {
        if self.pending(&key) || self.warm_pending(&key) {
            return;
        }
        self.fresh.drop_warm(&key);
        // An opened pull request is checked by its fingerprint first.
        let op = match (&key, op) {
            (Key::Item(2, number), Op::Read(target, Read::PullRequest(_)))
                if self
                    .fresh
                    .meta
                    .get(&key)
                    .is_some_and(|meta| meta.fingerprint.is_some()) =>
            {
                Op::Read(target, Read::Fingerprint(*number))
            }
            (_, op) => op,
        };
        let request = self.request(key, op);
        if !self.request_is_current(&request) {
            return;
        }
        // Asked now: a failing forge is not asked again before the entry's budget runs out.
        if let Some(meta) = self.fresh.meta.get_mut(&request.key) {
            meta.checked = Some(Instant::now());
        }
        self.fresh.implicit.insert(request.key.clone());
        self.queue.push_back(request);
    }

    /// What the forge is told it may answer 304 to.
    pub(super) fn conditional(&self, key: &Key, read: &Read) -> Conditional {
        let meta = self.fresh.meta.get(key);
        match read {
            Read::Repository | Read::RepositoryNamed(_) if self.cache.contains_key(key) => {
                Conditional {
                    etag: meta.and_then(|meta| meta.etag.clone()),
                    ..Conditional::default()
                }
            }
            Read::Fingerprint(_) => Conditional {
                fingerprint: meta.and_then(|meta| meta.fingerprint.clone()),
                ..Conditional::default()
            },
            _ => Conditional::default(),
        }
    }

    fn confirmed(&mut self, key: &Key) {
        if let Some(meta) = self.fresh.meta.get_mut(key) {
            meta.fetched = SystemTime::now();
            meta.checked = Some(Instant::now());
            meta.offline = false;
            meta.verified = true;
        }
    }

    /// Apply a reply only while its original read still belongs to this scope.
    pub(super) fn apply(
        &mut self,
        request: Request,
        routing_host: Option<HostKey>,
        fetched: Result<Fetched, ForgeError>,
        account: Option<AccountNamespace>,
        background: bool,
    ) {
        if !self.request_is_current(&request)
            || request.read_scope.is_some()
                && routing_host
                    .as_ref()
                    .is_some_and(|host| *host != HostKey::new(&self.host))
        {
            return;
        }
        let protect_projection = self.protects_read(&request.key)
            || (background || self.fresh.implicit.contains(&request.key))
                && self.has_read_projection();
        if !self.accept_read_account(
            routing_host.is_some(),
            account,
            fetched.as_ref().err(),
            protect_projection,
        ) {
            return;
        }
        if !background {
            self.accounts.observe(&self.host, fetched.as_ref().err());
        }
        let key = request.key.clone();
        self.apply_read(
            request,
            routing_host,
            fetched.map_err(|error| error.to_string()),
            background,
        );
        self.ask_again(&key);
    }

    fn apply_read(
        &mut self,
        mut request: Request,
        routing_host: Option<HostKey>,
        fetched: Result<Fetched, String>,
        background: bool,
    ) {
        let implicit = self.fresh.implicit.remove(&request.key) || background;
        let read = match &request.op {
            Op::Read(_, read) => Some(read.clone()),
            Op::Write(..) => None,
        };
        let cached = self.cache.get(&request.key).is_some_and(Result::is_ok);
        let (etag, fingerprint, ttl) = match &fetched {
            Ok(Fetched::Payload {
                value,
                etag,
                fingerprint,
            }) => (
                etag.clone(),
                fingerprint.clone(),
                read.as_ref().map_or(Duration::from_secs(60), |read| {
                    cache::ttl(read, Some(value))
                }),
            ),
            _ => (None, None, Duration::from_secs(60)),
        };
        let parsed = match fetched {
            Ok(Fetched::Payload {
                value: Payload::Fingerprint(fingerprint),
                ..
            }) => {
                let same = self
                    .fresh
                    .meta
                    .get(&request.key)
                    .and_then(|meta| meta.fingerprint.as_ref())
                    == Some(&fingerprint);
                if same {
                    self.confirmed(&request.key);
                } else if let (Key::Item(page, number), Op::Read(_, read)) =
                    (&request.key, &mut request.op)
                {
                    *read = if *page == 1 {
                        Read::Issue(*number)
                    } else {
                        Read::PullRequest(*number)
                    };
                    self.fresh.implicit.insert(request.key.clone());
                    self.queue.push_front(request);
                }
                self.ensure(false);
                return;
            }
            Ok(Fetched::NotModified) if cached => {
                self.confirmed(&request.key);
                self.ensure(false);
                return;
            }
            Ok(Fetched::NotModified) => Err("Unexpected response · not modified".into()),
            Ok(Fetched::Payload { value, .. }) => {
                self.note_totals(&request.key, &value);
                let (mut value, text) = crate::forge::legacy::encode(shape(&request.key), &value);
                match &request.key {
                    Key::Repository => {
                        if self.apply_repository(&value, routing_host.as_ref()) {
                            Ok(Reply { value, text })
                        } else {
                            Err("Forge returned an invalid repository".into())
                        }
                    }
                    Key::Inbox(host, page, state, _) => inbox::validate_rows(
                        &mut value,
                        host,
                        self.advertised_web_host().as_ref(),
                        *page,
                        state,
                    )
                    .map(|()| Reply { value, text }),
                    _ => Ok(Reply { value, text }),
                }
            }
            Err(error) => Err(error),
        };
        if background && parsed.is_err() && !cached {
            // A failed warm-up leaves the key to the interactive worker.
            return;
        }
        if parsed.is_ok()
            && let Some(limit) = request.list_limit()
        {
            self.list_limits.insert(request.key.clone(), limit);
        }
        let selected_item = if request.key == self.list_key() {
            self.items()
                .get(self.selected[self.page.saturating_sub(1).min(1)])
                .map(|item| self.row_identity(item))
                .or_else(|| {
                    self.list_selections
                        .get(&request.key)
                        .map(|(id, _)| id.clone())
                })
        } else {
            None
        };
        let selected_run = if request.key == Key::Runs(self.workflow_id()) {
            self.runs()
                .get(self.run_selected)
                .map(|run| number(run, "databaseId"))
        } else {
            None
        };
        match parsed {
            Err(error) if cached => {
                if implicit {
                    let meta = self.fresh.meta.get_mut(&request.key);
                    if let Some(meta) = meta {
                        meta.offline = true;
                        meta.checked = Some(Instant::now());
                    }
                } else if !matches!(request.key, Key::List(..) | Key::Inbox(..))
                    || matches!(self.page, 1 | 2) && request.key == self.list_key()
                {
                    // In words, with how old what stays on screen is.
                    let age = self.fresh.meta.get(&request.key).map(Meta::age);
                    self.notice = self.stale_notice(&error, age);
                    self.notice_error = true;
                }
            }
            Ok(mut reply) => {
                let unchanged = cached
                    && self.value(&request.key) == Some(&reply.value)
                    && (!matches!(request.key, Key::Diff(_))
                        || self.raw(&request.key) == Some(reply.text.as_str()));
                if !unchanged {
                    if implicit
                        && cached
                        && matches!(self.page, 1 | 2)
                        && request.key == self.list_key()
                    {
                        reply = self.merge_list(&request.key, reply);
                    }
                    self.cache.insert(request.key.clone(), Ok(reply));
                }
                // The worker wrote it to disk too.
                self.fresh.disk_absent.1.remove(&request.key);
                self.fresh.meta.insert(
                    request.key.clone(),
                    Meta {
                        fetched: SystemTime::now(),
                        checked: Some(Instant::now()),
                        ttl,
                        etag,
                        fingerprint,
                        offline: false,
                        verified: true,
                    },
                );
            }
            Err(error) => {
                self.cache.insert(request.key.clone(), Err(error));
            }
        }
        if request.key == self.list_key() {
            self.refresh_reader_preview();
        }
        if let Some(id) = selected_item {
            let index = self.page.saturating_sub(1).min(1);
            self.selected[index] = self
                .items()
                .iter()
                .position(|item| self.row_identity(item) == id)
                .unwrap_or_else(|| self.selected[index].min(self.items().len().saturating_sub(1)));
            self.remember_list_selection();
        }
        if let Some(id) = selected_run
            && let Some(index) = self
                .runs()
                .iter()
                .position(|run| number(run, "databaseId") == id)
        {
            self.run_selected = index;
        }
        self.ensure(false);
    }

    /// The merge rules for a background list refresh.
    fn merge_list(&mut self, key: &Key, reply: Reply) -> Reply {
        let old = self
            .value(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(fresh) = reply.value.as_array() else {
            return reply;
        };
        let cursor = old
            .get(self.selected[self.page.saturating_sub(1).min(1)])
            .map(|item| self.row_identity(item));
        let word = if self.states[self.page.saturating_sub(1).min(1)] == "open" {
            "closed"
        } else {
            "gone"
        };
        let merged = merge::rows(
            &old,
            fresh,
            |row| self.row_identity(row),
            cursor.as_deref(),
            0,
            true,
            |row| {
                let mut row = row.clone();
                if let Some(object) = row.as_object_mut() {
                    object
                        .entry("dustyGone")
                        .or_insert_with(|| Value::from(word));
                }
                row
            },
        );
        self.fresh.flash.extend(merged.changed);
        self.fresh.flash.extend(merged.inserted);
        let value = Value::Array(merged.rows);
        Reply {
            text: value.to_string(),
            value,
        }
    }

    /// Rows kept only to avoid a jump disappear once the reader leaves the list.
    pub(super) fn drop_ghosts(&mut self) {
        for reply in self.cache.values_mut().flatten() {
            if let Some(rows) = reply.value.as_array_mut()
                && rows.iter().any(|row| row.get("dustyGone").is_some())
            {
                rows.retain(|row| row.get("dustyGone").is_none());
                reply.text = reply.value.to_string();
            }
        }
    }

    /// The only freshness UI: `updated 3m ago · ⟳`.
    pub(super) fn freshness_chip(&self, key: &Key, compact: bool) -> Option<String> {
        if !self.fresh.enabled() || !self.cache.get(key).is_some_and(Result::is_ok) {
            return None;
        }
        let meta = self.fresh.meta.get(key)?;
        let busy = self.pending(key) || self.warm_pending(key);
        let age = ago(meta.age());
        // One string at every width: a border too narrow for it drops the label whole
        // rather than shortening `updated just now` to `now`.
        if compact {
            return None;
        }
        Some(if meta.offline {
            format!("updated {age} · {}", self.stale_word())
        } else {
            format!("updated {age}{}", if busy { " · ⟳" } else { "" })
        })
    }

    /// The list read for a page, state and query (the warm worker and live search use it).
    fn list_read_at(&self, page: usize, state: &str, query: &str, limit: usize) -> Op {
        let filter = Filter {
            state: StateFilter::parse(state),
            query: query.into(),
        };
        let page_of = Page::first(limit.min(LIST_CEILING) as u32);
        if self.inbox.enabled {
            return Op::Read(
                ForgeTarget::host(&self.host),
                Read::Search {
                    kind: kind_of(page),
                    filter,
                    page: page_of,
                },
            );
        }
        self.read(Read::List {
            kind: kind_of(page),
            filter,
            page: page_of,
        })
    }

    fn list_key_at(&self, page: usize, state: &str, query: &str) -> Key {
        if self.inbox.enabled {
            Key::Inbox(self.host.clone(), page, state.into(), query.into())
        } else {
            Key::List(page, state.into(), query.into())
        }
    }

    /// Whether `j`/`k` are moving the list cursor: it reached its row less than `DWELL` ago
    /// from another row of the same list. Records where the cursor rests.
    pub(super) fn cursor_moving(&mut self) -> bool {
        if !self.fresh.enabled()
            || !matches!(self.page, 1 | 2)
            || self.inbox.enabled
            || self.opened.is_some()
        {
            return false;
        }
        let selected = self.selected[self.page - 1];
        let (page, row, _) = self.fresh.rest;
        if (page, row) != (self.page, selected) {
            // The row a page first shows is not a move.
            self.fresh.moving = page == self.page && row != usize::MAX;
            self.fresh.rest = (self.page, selected, Instant::now());
        }
        self.fresh.moving && self.fresh.rest.2.elapsed() < DWELL
    }

    /// The warm worker, prefetching and the revalidation sweep; called from `poll`.
    pub(super) fn poll_fresh(&mut self) -> bool {
        if !self.fresh.enabled() {
            return false;
        }
        let mut changed = self.poll_disk();
        while let Some(event) = self.fresh.warm.try_reply() {
            if event.request_id != self.fresh.warm_next {
                continue;
            }
            let Some(request) = self.fresh.warm_active.take() else {
                continue;
            };
            let ForgeReply::Fetched(_, routing_host, result, account) = event.reply else {
                continue;
            };
            changed = true;
            self.apply(request, routing_host, result, account, true);
        }
        changed |= self.poll_live();
        if self.repo.is_empty() {
            return changed;
        }
        let scope = (
            self.host.clone(),
            self.repo.clone(),
            self.scope_epoch,
            self.account.clone(),
        );
        if self.fresh.warmed.as_ref() != Some(&scope) {
            self.fresh.warmed = Some(scope);
            self.fresh.warm_queue.clear();
            // Startup: everything a first keypress can show.
            let mut warm = vec![
                (
                    Key::List(1, "open".into(), String::new()),
                    self.read(Read::List {
                        kind: ItemKind::Issue,
                        filter: Filter::default(),
                        page: Page::first(LIST_BATCH as u32),
                    }),
                ),
                (
                    Key::List(2, "open".into(), String::new()),
                    self.read(Read::List {
                        kind: ItemKind::PullRequest,
                        filter: Filter::default(),
                        page: Page::first(LIST_BATCH as u32),
                    }),
                ),
                (Key::Workflows, self.read(Read::Workflows)),
                (Key::Runs(0), self.runs_read(0)),
            ];
            if self.inbox.enabled {
                warm.clear();
            }
            // Read by the disk worker within this poll's budget: a slow disk never holds up
            // the first keys.
            self.fresh.warm_disk.clear();
            for (key, op) in warm {
                let cached = self.load_disk_soon(&key, &op);
                let request = self.request(key, op);
                match cached {
                    Some(cached) => {
                        if !cached || self.needs_check(&request.key) {
                            self.fresh.warm_queue.push_back(request);
                        }
                    }
                    None => self.fresh.warm_disk.push(request),
                }
            }
            changed = true;
        }
        if !self.fresh.warm_disk.is_empty() {
            for request in std::mem::take(&mut self.fresh.warm_disk) {
                if !self.request_is_current(&request) {
                    continue;
                }
                if self.disk_loading(&request.key) {
                    self.fresh.warm_disk.push(request);
                } else if !self.cache.contains_key(&request.key) || self.needs_check(&request.key) {
                    self.fresh.warm_queue.push_back(request);
                }
            }
        }
        // After the warm-up is queued (a new scope clears the queue): the list's check marks.
        changed |= self.poll_row_checks();
        // Prefetch the neighbours of a resting cursor.
        if matches!(self.page, 1 | 2) && !self.inbox.enabled && self.opened.is_none() {
            let at = self.page - 1;
            let selected = self.selected[at];
            if !self.cursor_moving() && self.fresh.moving {
                // The cursor dwelt on its row: ask the forge for what it shows.
                self.fresh.moving = false;
                self.ensure(false);
                changed = true;
            }
            if self.fresh.rest.2.elapsed() >= PREFETCH_REST {
                let neighbours = [selected + 1, selected.wrapping_sub(1)]
                    .into_iter()
                    .filter_map(|index| self.items().get(index).map(|item| number(item, "number")))
                    .collect::<Vec<_>>();
                for id in neighbours {
                    let key = Key::Item(self.page, id);
                    if self.fresh.prefetched.contains(&key)
                        || self.pending(&key)
                        || self.disk_loading(&key)
                    {
                        continue;
                    }
                    let op = self.read(if self.page == 1 {
                        Read::Issue(id)
                    } else {
                        Read::PullRequest(id)
                    });
                    // A slow disk is not waited for past the poll's budget: asked again once
                    // its answer has landed.
                    let Some(cached) = self.load_disk_soon(&key, &op) else {
                        continue;
                    };
                    self.fresh.prefetched.insert(key.clone());
                    changed |= cached;
                    // Signed out, one item is six REST reads of a 60-an-hour quota: neighbours
                    // come from disk only, and the forge is asked when one is opened.
                    if !self.read_only()
                        && (!self.cache.contains_key(&key) || self.needs_check(&key))
                    {
                        let request = self.request(key, op);
                        self.fresh.warm_queue.push_back(request);
                    }
                }
            }
        }
        // The chips' ages (`updated just now` → `updated 1m ago`) repaint as they change.
        let chips = [self.list_key(), Key::Item(self.page, self.number())]
            .iter()
            .map(|key| self.freshness_chip(key, false))
            .collect::<Vec<_>>();
        if chips != self.fresh.chips {
            self.fresh.chips = chips;
            changed = true;
        }
        // Revalidate what is on screen as its budget runs out (and poll fingerprints).
        let idle = self.active.idle() && self.queue.is_empty();
        if idle && self.fresh.sweep.elapsed() >= SWEEP {
            self.fresh.sweep = Instant::now();
            self.ensure(false);
        }
        let idle = self.active.idle() && self.queue.is_empty();
        let since = if idle {
            *self.fresh.idle_since.get_or_insert_with(Instant::now)
        } else {
            self.fresh.idle_since = None;
            Instant::now()
        };
        if self.fresh.warm_active.is_none()
            && !self.fresh.warm.busy()
            && since.elapsed() >= WARM_IDLE
            && super::super::jobs::running() == 0
            && std::env::var("DUSTY_FORGE_WARM").as_deref() != Ok("0")
        {
            while let Some(request) = self.fresh.warm_queue.pop_front() {
                if !self.request_is_current(&request)
                    || self.pending(&request.key)
                    || self.cache.get(&request.key).is_some_and(Result::is_ok)
                        && !self.needs_check(&request.key)
                {
                    continue;
                }
                let Op::Read(target, read) = &request.op else {
                    continue;
                };
                self.fresh.warm_next += 1;
                let conditional = self.conditional(&request.key, read);
                let account = self.validation_account(&request.key);
                if self
                    .fresh
                    .warm
                    .fetch_as(
                        self.root.clone(),
                        self.fresh.warm_next,
                        target.clone(),
                        read.clone(),
                        (account, conditional),
                        true,
                    )
                    .is_ok()
                {
                    if let Some(meta) = self.fresh.meta.get_mut(&request.key) {
                        meta.checked = Some(Instant::now());
                    }
                    self.fresh.warm_active = Some(request);
                    changed = true;
                }
                break;
            }
        }
        changed
    }

    /// The filter form's text as the user types (`None` once it closes).
    pub(super) fn live_input(&mut self, input: Option<(String, String)>) {
        let Some((query, state)) = input else {
            if self.fresh.live.take().is_some() {
                self.fresh.live_key = None;
            }
            return;
        };
        if !self.fresh.enabled() || !matches!(self.page, 1 | 2) || self.inbox.enabled {
            return;
        }
        match &mut self.fresh.live {
            Some(live) if live.page == self.page => {
                if live.query != query || live.state != state {
                    live.query = query;
                    live.state = state;
                    live.changed = Instant::now();
                    live.sent = false;
                }
            }
            _ => {
                let unchanged = {
                    let at = self.page - 1;
                    self.queries[at] == query && self.states[at] == state
                };
                self.fresh.live = Some(Live {
                    page: self.page,
                    query,
                    state,
                    changed: Instant::now(),
                    sent: unchanged,
                    corpus: None,
                });
            }
        }
    }

    /// Debounced remote search for the live query; a newer query cancels an older one.
    fn poll_live(&mut self) -> bool {
        let Some(live) = &self.fresh.live else {
            return false;
        };
        if live.sent || live.changed.elapsed() < DEBOUNCE || live.page != self.page {
            return false;
        }
        let (page, query, state) = (live.page, live.query.clone(), live.state.clone());
        if let Some(live) = &mut self.fresh.live {
            live.sent = true;
        }
        let key = self.list_key_at(page, &state, &query);
        if let Some(previous) = self.fresh.live_key.take()
            && previous != key
            && previous != self.list_key()
        {
            // A live search the user typed past: its answer is dropped, not news.
            self.active.cancel(&previous);
            self.fresh.implicit.remove(&previous);
            self.queue.retain(|request| request.key != previous);
        }
        let op = self.list_read_at(page, &state, &query, LIST_BATCH);
        self.fresh.live_key = Some(key.clone());
        let cached = self.load_disk_now(&key, &op);
        if (!cached || self.needs_check(&key)) && !self.pending(&key) {
            if cached {
                self.fresh.implicit.insert(key.clone());
            }
            let request = self.request(key, op);
            self.queue.push_front(request);
        }
        true
    }

    fn corpus(&self, page: usize) -> Vec<Value> {
        let mut rows = BTreeMap::<u64, Value>::new();
        let mut add = |row: &Value| {
            let id = number(row, "number");
            if id == 0 || row.get("dustyGone").is_some() {
                return;
            }
            let newer = rows
                .get(&id)
                .is_none_or(|old| string(old, "updatedAt") < string(row, "updatedAt"));
            if newer {
                rows.insert(id, row.clone());
            }
        };
        for (key, reply) in &self.cache {
            if let (Key::List(at, ..), Ok(reply)) = (key, reply)
                && *at == page
            {
                reply
                    .value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .for_each(&mut add);
            }
        }
        if let (Some(cache), Some(account)) = (&self.fresh.cache, &self.account) {
            let kind = kind_of(page);
            for envelope in cache.scan::<Payload>(account, Some(&self.repo), "list") {
                let listed = serde_json::from_str::<Read>(&envelope.key).is_ok_and(
                    |read| matches!(read, Read::List { kind: listed, .. } if listed == kind),
                );
                if listed {
                    let (value, _) = crate::forge::legacy::encode(
                        crate::forge::legacy::Shape::List(kind),
                        &envelope.data,
                    );
                    value.as_array().into_iter().flatten().for_each(&mut add);
                }
            }
        }
        rows.into_values().rev().collect()
    }

    /// Local hits for the live query, then the remote results below a divider.
    pub(super) fn live_rows(&mut self) -> Option<LiveRows> {
        let live = self.fresh.live.as_ref()?;
        if live.page != self.page {
            return None;
        }
        if live.corpus.is_none() {
            let corpus = self.corpus(self.page);
            if let Some(live) = &mut self.fresh.live {
                live.corpus = Some(corpus);
            }
        }
        let live = self.fresh.live.as_ref()?;
        let local = live
            .corpus
            .iter()
            .flatten()
            .filter(|row| matches(row, &live.query, &live.state))
            .take(LOCAL_HITS)
            .cloned()
            .collect::<Vec<_>>();
        let key = self.list_key_at(live.page, &live.state, &live.query);
        let seen = local
            .iter()
            .map(|row| number(row, "number"))
            .collect::<HashSet<_>>();
        let remote = self
            .value(&key)
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|row| !seen.contains(&number(row, "number")))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let host = if self.host.is_empty() {
            "GitHub"
        } else {
            self.host.as_str()
        };
        let divider = if !live.sent || self.pending(&key) {
            format!("From {host} · searching ⟳")
        } else {
            match self.cache.get(&key) {
                // Nothing new from the forge: no divider for an empty section.
                Some(Ok(_)) if remote.is_empty() => String::new(),
                Some(Ok(_)) => format!("From {host} · {} more", remote.len()),
                Some(Err(_)) => format!("From {host} · offline"),
                None => format!("From {host} · searching ⟳"),
            }
        };
        Some(LiveRows {
            local,
            remote,
            divider,
        })
    }

    /// Show a write before the forge confirms it; `settle` keeps or undoes it.
    pub(super) fn optimistic(&mut self, write: &Write) {
        self.fresh.undo.clear();
        if !self.fresh.enabled() {
            return;
        }
        let page_of = |kind: &ItemKind| if *kind == ItemKind::Issue { 1 } else { 2 };
        let mut edits: Vec<(Key, Edit)> = Vec::new();
        match write {
            Write::Comment { kind, number, body } => {
                let body = body.clone();
                edits.push((
                    Key::timeline(*kind, *number),
                    Box::new(move |timeline| {
                        let event = serde_json::json!({
                            "event": "commented",
                            "user": {"login": "you"},
                            "body": body,
                            "created_at": "sending…",
                            "dustyPending": true,
                        });
                        match timeline.as_array_mut() {
                            Some(pages) if !pages.is_empty() => {
                                if let Some(last) = pages.last_mut().and_then(Value::as_array_mut) {
                                    last.push(event);
                                }
                            }
                            _ => *timeline = serde_json::json!([[event]]),
                        }
                    }),
                ));
            }
            Write::SetState {
                kind,
                number: id,
                open,
                ..
            } => {
                let word = if *open { "OPEN" } else { "CLOSED" };
                let page = page_of(kind);
                edits.push((
                    Key::Item(page, *id),
                    Box::new(move |item| item["state"] = word.into()),
                ));
                if self.inbox.enabled || self.repo.eq_ignore_ascii_case(self.list_repository()) {
                    let id = *id;
                    let scope = self.inbox.enabled.then(|| {
                        (
                            self.host.clone(),
                            self.repo.clone(),
                            self.advertised_web_host(),
                        )
                    });
                    edits.push((
                        self.list_key_at(page, &self.states[page - 1], &self.queries[page - 1]),
                        Box::new(move |rows| {
                            for row in rows.as_array_mut().into_iter().flatten() {
                                if number(row, "number") == id
                                    && scope.as_ref().is_none_or(|(host, repo, alias)| {
                                        inbox::Target::from_row(row, host, alias.as_ref(), page)
                                            .is_ok_and(|target| {
                                                target.repo.eq_ignore_ascii_case(repo)
                                            })
                                    })
                                {
                                    row["state"] = word.into();
                                }
                            }
                        }),
                    ));
                }
            }
            Write::SetLabels {
                kind,
                number,
                add,
                remove,
            } => {
                let (add, remove) = (add.clone(), remove.clone());
                let relabel = move |item: &mut Value| {
                    let mut labels = array(item, "labels")
                        .iter()
                        .filter(|label| !remove.iter().any(|name| name == string(label, "name")))
                        .cloned()
                        .collect::<Vec<_>>();
                    for name in &add {
                        if !labels.iter().any(|label| string(label, "name") == name) {
                            labels.push(serde_json::json!({"name": name}));
                        }
                    }
                    item["labels"] = Value::Array(labels);
                };
                edits.push((Key::Item(page_of(kind), *number), Box::new(relabel)));
            }
            Write::Merge { number, .. } => {
                edits.push((
                    Key::Item(2, *number),
                    Box::new(|item| item["state"] = "MERGED".into()),
                ));
            }
            _ => {}
        }
        for (key, edit) in edits {
            let scope = self.cache_scope(&key);
            let Some(Ok(reply)) = self.cache.get_mut(&key) else {
                continue;
            };
            self.fresh.undo.push(Undo {
                scope,
                key: key.clone(),
                before: reply.value.clone(),
            });
            edit(&mut reply.value);
            reply.text = reply.value.to_string();
            // A read of it already on its way answers for the time before the write: dropped,
            // so it cannot paint over the edit; the write's own refresh reads it again.
            self.active.cancel(&key);
        }
    }

    /// The forge answered the write: keep the optimistic values (a refresh follows) or put the
    /// cached ones back.
    pub(super) fn settle(&mut self, confirmed: bool) {
        let undo = std::mem::take(&mut self.fresh.undo);
        if confirmed {
            return;
        }
        for Undo { key, scope, before } in undo.into_iter().rev() {
            if scope == self.cache_scope(&key) {
                self.cache.insert(
                    key,
                    Ok(Reply {
                        text: before.to_string(),
                        value: before,
                    }),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn warm_row_checks_belong_to_the_list_repository() {
        use crate::forge::model::CiState;
        use crate::forge::records::ItemRecord;

        let mut ws = Workspace::new(PathBuf::new());
        ws.fresh = Fresh::with(None);
        ws.host = "codeberg.org".into();
        ws.repo = "alpha/one".into();
        ws.page = 2;
        let row = json!({ "number": 7 });
        ws.cache.insert(
            ws.list_key(),
            Ok(Reply {
                text: String::new(),
                value: json!([row.clone()]),
            }),
        );
        ws.follow(open::parse("https://codeberg.org/beta/two/pulls/7").unwrap());
        assert_eq!(ws.repo, "beta/two");
        assert_eq!(ws.list_repository(), "alpha/one");

        ws.fresh.rows.show(vec![7]);
        ws.poll_row_checks();
        let request = ws.fresh.warm_queue.pop_front().expect("visible row check");
        let key = Key::RowChecks(vec![7]);
        assert_eq!(request.key, key);
        assert!(matches!(
            &request.op,
            Op::Read(target, Read::Fingerprints(numbers))
                if *target == ForgeTarget::repository("codeberg.org", "alpha/one") && numbers.as_slice() == [7]
        ));

        let other = ws.request(
            key.clone(),
            Op::Read(ws.target(), Read::Fingerprints(vec![7])),
        );
        let fetched = |rollup| {
            Ok(Fetched::payload(Payload::Fingerprints(vec![(
                7,
                Fingerprint {
                    rollup: Some(rollup),
                    ..Default::default()
                },
            )])))
        };
        ws.apply(other, None, fetched(CiState::Failed), None, true);
        assert!(ws.value(&key).is_none(), "B's check must not reach A's row");
        ws.apply(request, None, fetched(CiState::Passed), None, true);
        assert!(ws.poll_row_checks());
        let mut record = ItemRecord::from_row(&row);
        ws.fresh.rows.overlay(ws.list_repository(), &mut record);
        assert_eq!(record.ci(), Some(CiState::Passed));
    }

    #[test]
    fn repository_metadata_keeps_the_remote_port_on_disk_and_wire() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path()).unwrap();
        repo.remote("origin", "https://git.example.test:8443/o/r")
            .unwrap();
        let target = ForgeTarget::remote();
        let (host, repo) = target.resolve(root.path()).unwrap();
        let read = Read::Repository;
        let account = crate::forge::transport::AccountNamespace::anonymous(host.clone());
        let key = CacheKey::new(&account, repo.as_ref(), &read);
        for url in [
            "https://git.example.test:8443/o/r",
            "https://git.example.test/o/r",
            "https://public.example.test/o/r",
        ] {
            for from_disk in [false, true] {
                let mut workspace = Workspace::new(root.path().into());
                workspace.fresh = Fresh::with(None);
                let payload = Payload::Repo(crate::forge::model::Repo {
                    full_name: "o/r".into(),
                    web_url: url.into(),
                    ..Default::default()
                });
                if from_disk {
                    let envelope = cache::Envelope::new(&key, payload, Duration::from_secs(60));
                    assert!(workspace.apply_disk(
                        &Key::Repository,
                        &read,
                        &host,
                        super::super::disk::Found::Payload(envelope),
                    ));
                } else {
                    workspace.apply(
                        workspace.request(Key::Repository, Op::Read(target.clone(), read.clone())),
                        Some(host.clone()),
                        Ok(Fetched::payload(payload)),
                        None,
                        false,
                    );
                }

                let expected = ForgeTarget::repository("git.example.test:8443", "o/r");
                assert_eq!(workspace.target(), expected);
                assert_eq!(workspace.target().resolve(root.path()).unwrap().0, host);
                assert_eq!(
                    string(workspace.value(&Key::Repository).unwrap(), "url"),
                    url
                );
                workspace.page = 1;
                let list = workspace.list_read(LIST_BATCH);
                for op in [
                    workspace.read(Read::Repository),
                    workspace.read(Read::Issue(1)),
                    list,
                ] {
                    let Op::Read(target, _) = op else {
                        panic!("expected a forge read");
                    };
                    assert_eq!(target, expected);
                }
            }
        }
    }

    #[test]
    fn first_list_paint_only_looks_up_the_verified_account() {
        let root = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::at(root.path()));
        let host = crate::forge::transport::HostKey::new("forge.test:8443");
        let a =
            crate::forge::transport::AccountNamespace::signed_in(host.clone(), "alice").unwrap();
        let b = crate::forge::transport::AccountNamespace::signed_in(host.clone(), "bob").unwrap();
        let public = crate::forge::transport::AccountNamespace::anonymous(host.clone());
        let mut first = Workspace::new(root.path().into());
        first.host = host.to_string();
        first.repo = "o/r".into();
        first.page = 1;
        let key = first.list_key();
        let op = first.list_read(LIST_BATCH);
        let Op::Read(_, read) = &op else {
            panic!("list read")
        };
        let payload = Payload::Rows(crate::forge::request::PageOf {
            items: vec![crate::forge::model::ItemRow {
                number: 7,
                title: "Alice's private issue".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let ttl = Duration::from_secs(60);
        assert!(cache.store(
            &CacheKey::for_read(&a, Some("o/r"), read),
            &payload,
            ttl,
            None,
            None
        ));
        let mut public_payload = payload.clone();
        if let Payload::Rows(rows) = &mut public_payload {
            rows.items[0].title = "Public issue".into();
        }
        assert!(cache.store(
            &CacheKey::for_read(&public, Some("o/r"), read),
            public_payload,
            ttl,
            None,
            None
        ));
        drop(first);

        let mut restarted = Workspace::new(root.path().into());
        restarted.fresh = Fresh::with(Some(Arc::new(Cache::at(root.path()))));
        restarted.host = host.to_string();
        restarted.repo = "o/r".into();
        restarted.page = 1;
        assert!(!restarted.load_disk_now(&key, &op));
        restarted.account = Some(b);
        assert!(!restarted.load_disk_now(&key, &op));
        assert!(restarted.value(&key).is_none());
        restarted.reset_account(Some(public));
        assert!(restarted.load_disk_now(&key, &op));
        let rendered = restarted.value(&key).unwrap().to_string();
        assert!(rendered.contains("Public issue"));
        assert!(!rendered.contains("Alice's private issue"));
    }

    #[test]
    fn local_search_understands_common_qualifiers() {
        let row = json!({"number": 12, "title": "Crash on resize", "state": "OPEN",
            "author": {"login": "ana"}, "labels": [{"name": "bug"}]});
        assert!(matches(&row, "crash", "open"));
        assert!(matches(&row, "author:ana label:bug", "all"));
        assert!(!matches(&row, "-label:bug", "all"));
        assert!(matches(&row, "#12", "open"));
        assert!(!matches(&row, "crash", "closed"));
        assert!(matches(&row, "milestone:v2 resize", "open"));
        assert!(!matches(&row, "scroll", "open"));
    }

    #[test]
    fn ages_read_like_the_chip() {
        assert_eq!(ago(Duration::from_secs(5)), "just now");
        assert_eq!(ago(Duration::from_secs(180)), "3m ago");
        assert_eq!(ago(Duration::from_secs(7200)), "2h ago");
    }
}
