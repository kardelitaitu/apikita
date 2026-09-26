use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, Response, StatusCode},
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{SqlitePool, Row};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;
use uuid::fmt::Hyphenated;

use crate::config::AppConfig;
use crate::db::{
    debit_usage_transaction, release_reservation_transaction, reserve_balance_transaction,
    ReservationResult, UsageSettlement,
};
use crate::error::AppError;
use crate::ip_tracking::{ip_hash, record_key_ip, resolve_client_ip, today_utc, DailySalt, IpCidr};
use crate::money::calculate_token_cost_idr;
// Only the tests re-derive the reservation: the handler calls the ONE shared
// rule on ModelConfig, so a second copy of the arithmetic cannot drift from it.
#[cfg(test)]
use crate::money::calculate_preflight_reservation_idr;
use crate::routes::events::{publish_balance, publish_usage, todays_usage, RealtimeHub};
// The 30-day window and the spend read are shared with the key-management routes
// on purpose: the limit the proxy enforces and the number the dashboard shows
// must come from one definition, not two that can drift.
use crate::routes::keys::{key_spend_used, key_tokens_used, SPEND_WINDOW_DAYS};
use crate::upstream::{parse_usage_from_sse, UpstreamClient, UpstreamError, UpstreamStream, Usage};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<AppConfig>,
    pub http_client: reqwest::Client,
    /// The realtime fan-out behind `GET /events`.
    pub events: Arc<RealtimeHub>,
    /// The daily salt behind every IP hash. One per process, not per request:
    /// the whole point is that every request on a given day hashes under the
    /// same salt, so the distinct count means something. Never persisted.
    pub ip_salt: Arc<DailySalt>,
    /// Parsed once at startup from `config.network.trusted_proxy_cidrs`; see
    /// `resolve_client_ip` for why an untrusted peer's forwarded header is
    /// ignored rather than believed.
    pub trusted_proxies: Arc<[IpCidr]>,
}

/// Records which address a served request came from, against the key that
/// served it.
///
/// FIRE-AND-FORGET, on purpose. This is an abuse signal on the streaming hot
/// path, and it must never be the reason a request fails or a stream stalls: a
/// database blip here should cost an operator a signal, not cost a customer
/// their answer. The counts are a suspicion threshold, not enforcement —
/// nothing downstream acts on them without a human — so losing one is a
/// recoverable loss and blocking on one is not.
fn record_request_source(state: &AppState, key_id: Uuid, peer: SocketAddr, headers: &HeaderMap) {
    let ip = resolve_client_ip(peer.ip(), headers, &state.trusted_proxies);

    // Both derived here, before the spawn, because the salt must be read for
    // THIS day at THIS moment: after a spawn the day could roll over and the
    // hash would then be under a different salt than the counter's row.
    let day = today_utc();
    let hash = ip_hash(&state.ip_salt.salt_for_day(day), &ip);

    let pool = state.pool.clone();
    tokio::spawn(async move {
        if let Err(err) = record_key_ip(&pool, key_id, day, &hash).await {
            warn!(key_id = %key_id, error = %err, "Could not record the request source");
        }
    });
}

impl axum::extract::FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

/// Owns a held reservation and gives it back if dropped before the settlement
/// transaction has committed and credited the hold back.
///
/// The hold is taken in the handler BEFORE the upstream call and released inside
/// the settlement transaction (`debit_usage_transaction`). Between those two
/// points the money is out of the wallet and must come back on EVERY exit: a
/// client reset (this future dropped mid-await), a `?`, an early return, or a
/// failed settlement. Carrying the hold in a guard makes that structural
/// instead of depending on every exit site remembering to call `release_quietly`.
///
/// `Drop` cannot await, so the release is fire-and-forget: it spawns a task. A
/// dropped guard means the settlement did NOT claim the hold (it only releases
/// inside its committed transaction), so crediting it back is always correct and
/// never double-counts. On the success path the caller consumes the guard with
/// `defuse` once the debit has committed - the exact moment the money is already
/// back in the wallet.
struct ReservationGuard {
    pool: SqlitePool,
    account_id: Uuid,
    reserved_idr: i64,
    reservation_ref: String,
    model: String,
    defused: bool,
}

impl ReservationGuard {
    fn new(
        pool: &SqlitePool,
        account_id: Uuid,
        reserved_idr: i64,
        reservation_ref: &str,
        model: &str,
    ) -> Self {
        Self {
            pool: pool.clone(),
            account_id,
            reserved_idr,
            reservation_ref: reservation_ref.to_string(),
            model: model.to_string(),
            defused: false,
        }
    }

    /// The settlement transaction has committed and credited the hold back in the
    /// same transaction, so dropping this guard must not release it again. Flip
    /// the `defused` flag in place; callers may still read `reservation_ref` /
    /// `model` afterwards to drive the explicit release.
    fn defuse(&mut self) {
        self.defused = true;
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        let pool = self.pool.clone();
        let account_id = self.account_id;
        let reserved_idr = self.reserved_idr;
        let reservation_ref = std::mem::take(&mut self.reservation_ref);
        let model = std::mem::take(&mut self.model);
        // Fire-and-forget: `Drop` cannot await and the hold must come back even
        // if this future is being torn down.
        tokio::spawn(async move {
            release_quietly(&pool, account_id, reserved_idr, &reservation_ref, &model).await;
        });
    }
}

/// The only fields the enforcement order needs before forwarding. The body is
/// forwarded verbatim, so this is a lens over it rather than the payload: a
/// parameter this struct does not know about — `temperature`, `tools`, `stop`,
/// `top_p`, ... — still reaches the upstream untouched.
#[derive(Debug, Deserialize)]
struct RequestMeta {
    model: String,
    #[serde(default)]
    max_tokens: Option<u64>,
}

/// How many trailing bytes are retained to parse the usage block.
///
/// Only the tail is kept, never the body: usage arrives in the final SSE
/// chunks, while every chunk is forwarded the moment it arrives. 64 KiB is far
/// more than one usage chunk needs and stays a fixed, negligible allocation.
const USAGE_TAIL_CAP: usize = 64 * 1024;

/// The upstream client, built exactly once per process: it owns the connection
/// pool, the per-endpoint key pools and the circuit breakers, all of which are
/// worthless unless they are shared across requests.
static UPSTREAM: OnceLock<UpstreamClient> = OnceLock::new();

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

/// Whether `model` is permitted for a key whose stored allowlist is
/// `allowed_models`.
///
/// DENY BY DEFAULT (DEFECT 2): an empty allowlist permits NO model — a key made
/// without an explicit model list cannot call the dearest configured model. Only
/// a model that appears in the list is allowed. The list is matched against the
/// model name exactly as the client requested it (before any upstream rewrite).
fn is_model_allowed(allowed_models: &[String], model: &str) -> bool {
    allowed_models.iter().any(|m| m == model)
}

/// Whether a key's rolling 30-day usage has reached its ceiling.
///
/// ONE rule for both the spend limit and the token limit: they differ only in
/// the unit they count, and two copies of the same boundary is how the two
/// start disagreeing. 0 (or a negative value, which key management refuses to
/// store) means "no limit" (docs/website/06-api-keys-and-limits.md), so it
/// never blocks. The comparison is `>=`, not `>`: the window total is what has
/// already been used, so a key sitting exactly on its ceiling is out of budget
/// and the next request is the one that would exceed it. Pure, so the boundary
/// is tested without a database or a request.
fn limit_reached(limit: i64, used: i64) -> bool {
    limit > 0 && used >= limit
}

/// The rate-limit window: one minute, the unit `rate_limit_rpm` is stated in.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// What a rate check decided. A plain value rather than a `Result` so the
/// boundary is testable without a clock, a key or a database.
#[derive(Debug, PartialEq, Eq)]
enum RateDecision {
    Allow,
    /// Refused; `retry_after_secs` is how long until the window rolls over.
    Deny {
        retry_after_secs: u64,
    },
}

/// One key's request counter for the current minute.
///
/// FIXED window, not sliding: a counter plus the instant its minute began. A
/// burst straddling a boundary can therefore pass twice in quick succession —
/// the documented cost of a fixed window, and much cheaper than retaining one
/// timestamp per request. Sustained throughput is still bounded at the
/// configured rate.
struct RateWindow {
    started_at: Instant,
    count: u32,
}

impl RateWindow {
    fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            count: 0,
        }
    }

    /// Count this request against the window.
    ///
    /// `limit_rpm` is always > 0 here: the caller skips the whole mechanism for
    /// a key with no rate limit, so an unlimited key never allocates a window.
    fn check(&mut self, limit_rpm: u32, now: Instant) -> RateDecision {
        if now.saturating_duration_since(self.started_at) >= RATE_WINDOW {
            self.started_at = now;
            self.count = 0;
        }

        if self.count >= limit_rpm {
            let remaining =
                RATE_WINDOW.saturating_sub(now.saturating_duration_since(self.started_at));
            // At least 1: a `Retry-After: 0` invites an immediate retry that is
            // refused again, which reads as a broken limiter.
            return RateDecision::Deny {
                retry_after_secs: remaining.as_secs().max(1),
            };
        }

        self.count += 1;
        RateDecision::Allow
    }
}

/// Per-key rate windows.
///
/// SINGLE-PROCESS, exactly like the key-metadata cache: another instance behind
/// the load balancer keeps its own counter, so the effective ceiling is
/// `rate_limit_rpm` PER INSTANCE rather than per key fleet-wide. Closing that
/// needs shared state (Redis or similar), which this build has no dependency
/// for. Setting `rate_limit_rpm` to 0 disables the check entirely.
static RATE_WINDOWS: OnceLock<Mutex<HashMap<Uuid, RateWindow>>> = OnceLock::new();

/// The same backstop as the key cache, for the same reason: only a key that
/// authenticated can create a window, but a pathological number of live keys
/// must not pin unbounded memory.
const RATE_WINDOW_CAPACITY: usize = 4096;

/// Count one request for `key_id` against its minute, returning the decision.
fn check_rate_limit(key_id: Uuid, limit_rpm: u32, now: Instant) -> RateDecision {
    let windows = RATE_WINDOWS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut windows = windows.lock().unwrap_or_else(|e| e.into_inner());
    check_rate_limit_in(&mut windows, RATE_WINDOW_CAPACITY, key_id, limit_rpm, now)
}

/// The decision itself, over an explicit table.
///
/// Split out from the process-wide map so the eviction rule can be pinned by a
/// pure test: the table is synchronous and the mutex is held only for map
/// operations, so nothing here needs a server, a clock or a database.
fn check_rate_limit_in(
    windows: &mut HashMap<Uuid, RateWindow>,
    capacity: usize,
    key_id: Uuid,
    limit_rpm: u32,
    now: Instant,
) -> RateDecision {
    // A key that ALREADY holds a window needs no room in the table, so it must
    // never be subject to eviction: its counter is the very thing being
    // enforced. Evicting before this lookup is what let a key sitting on its
    // ceiling reset its own window by making another request.
    if let Some(window) = windows.get_mut(&key_id) {
        return window.check(limit_rpm, now);
    }

    if windows.len() >= capacity {
        // Reclaim every window whose minute is over. Dropping one resets
        // nothing — that key had already rolled over — so a dead window is
        // always the victim of choice.
        windows.retain(|_, w| now.saturating_duration_since(w.started_at) < RATE_WINDOW);
        if windows.len() >= capacity {
            // DELIBERATE FAIL-OPEN. The table is full of LIVE windows, so any
            // victim is a key mid-minute and dropping it hands that key a fresh
            // allowance — it is allowed through MORE often, never less. This is
            // the explicit tradeoff: bounded memory (RATE_WINDOW_CAPACITY live
            // keys) in exchange for a bounded amount of over-admission, and it
            // is only reachable at that many simultaneously-live keys. The
            // expired sweep above means the common case never gets here. A
            // per-key LRU would need a dependency this build does not carry.
            if let Some(victim) = windows.keys().next().copied() {
                windows.remove(&victim);
            }
        }
    }

    windows
        .entry(key_id)
        .or_insert_with(|| RateWindow::new(now))
        .check(limit_rpm, now)
}

/// Maximum cached API-key records.
///
/// The cache can only ever be filled by a key that actually exists in the
/// database: a presented key with no row is NOT cached (see `load_key_metadata`),
/// so an unauthenticated caller cannot grow this map. In practice the working set
/// is the number of live keys. The cap is a backstop so a pathological number of
/// keys cannot pin unbounded memory; at roughly 250 bytes per record this bounds
/// the cache at about 1 MiB.
const KEY_CACHE_CAPACITY: usize = 4096;

/// The api-key fields the request path decides on. Caching exactly these keeps a
/// cache hit indistinguishable from a fresh read: the same values drive the same
/// checks.
#[derive(Debug, Clone, PartialEq)]
struct KeyMetadata {
    key_id: Uuid,
    account_id: Uuid,
    models: Value,
    /// The key's rolling 30-day spend ceiling, in IDR; 0 means "no limit"
    /// (docs/website/06-api-keys-and-limits.md). It lives in the cached record
    /// because it is a column of the same `api_keys` row every other check
    /// reads: a cache HIT must decide exactly like a MISS, and a limit that
    /// existed only on the miss path would make the two paths disagree.
    spend_limit_idr: i64,
    /// The key's rolling 30-day token ceiling, in tokens; 0 means "no limit".
    /// Same reasoning as `spend_limit_idr`: it is a column of the same row, so
    /// caching it is what keeps a cache HIT enforcing exactly like a MISS.
    token_limit: i64,
    /// The key's requests-per-minute ceiling; 0 means "no limit". Cached for the
    /// same reason — a narrowed rate limit must not be honoured from a stale
    /// record any longer than the other fields are.
    rate_limit_rpm: i32,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

struct CacheEntry {
    meta: KeyMetadata,
    stored_at: Instant,
}

/// A bounded, TTL-ed cache of API-key metadata, keyed by the HASHED key.
///
/// The plaintext key is never retained: the caller hashes the presented token and
/// only that digest is used as the key here, so a memory dump of this map cannot
/// yield a usable credential.
///
/// STALENESS TRADEOFF - the documented cost of this cache:
/// a key revoked, or its model allowlist narrowed, will still be honoured for up to
/// `limits.key_metadata_cache_seconds` after the change lands in the database. That
/// window is the price of removing a DB round-trip from every proxied request.
/// It is bounded and configurable, and setting the config value to 0 disables the
/// cache entirely (every lookup misses), which is the way to trade the DB read back
/// for immediate revocation.
///
/// Expiry is judged against a monotonic `Instant`, so a wall-clock adjustment can
/// neither expire entries early nor keep them alive past the TTL.
struct KeyCache {
    entries: HashMap<String, CacheEntry>,
    /// Insertion order, for eviction. Kept in step with `entries`: a key is
    /// pushed exactly once, when it is first inserted.
    order: std::collections::VecDeque<String>,
    ttl: Duration,
    capacity: usize,
    /// Bumped by every invalidation. A lookup that read the row BEFORE an
    /// invalidation landed must not put its pre-change record back into the map
    /// (see `insert_if_unchanged`), or the invalidation would be undone by a
    /// request that was already in flight.
    generation: u64,
}

impl KeyCache {
    fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
            ttl,
            capacity: capacity.max(1),
            generation: 0,
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    /// Forget one key, and mark the generation so a lookup already in flight
    /// cannot re-insert what it read before this call.
    ///
    /// Both containers are cleaned: leaving the hash in `order` would grow the
    /// queue with slots no map entry owns.
    fn remove(&mut self, key_hash: &str) {
        self.entries.remove(key_hash);
        self.order.retain(|key| key != key_hash);
        self.generation += 1;
    }

    /// The cached record, or None when it is absent or past its TTL.
    ///
    /// `now` is a parameter rather than a call to `Instant::now()` so the TTL rule
    /// is testable without sleeping. An expired entry is a miss and is left for
    /// `purge_expired` to reclaim.
    fn get(&self, key_hash: &str, now: Instant) -> Option<KeyMetadata> {
        let entry = self.entries.get(key_hash)?;
        if now.duration_since(entry.stored_at) < self.ttl {
            Some(entry.meta.clone())
        } else {
            None
        }
    }

    fn insert(&mut self, key_hash: String, meta: KeyMetadata, now: Instant) {
        if self.entries.len() >= self.capacity {
            // Expired entries are worthless, so reclaim them first; only if that
            // frees nothing do we evict a live one.
            self.purge_expired(now);
        }

        while self.entries.len() >= self.capacity {
            // FIFO by first insertion. A true LRU would need per-hit bookkeeping
            // for no real gain here: the working set is all live keys, so which
            // one goes is close to arbitrary either way.
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }

        // Refresh in place: the key is already in `order`, and pushing it again
        // would put two entries in the queue for one map slot.
        let inserted = self
            .entries
            .insert(
                key_hash.clone(),
                CacheEntry {
                    meta,
                    stored_at: now,
                },
            )
            .is_none();
        if inserted {
            self.order.push_back(key_hash);
        }
    }

    /// Insert only when nothing has been invalidated since `seen_generation`.
    ///
    /// The lookup reads the row outside the lock, so a revocation can land in
    /// between: without this check that lookup would write a pre-revocation
    /// record back into the cache, where it would live out a fresh full TTL and
    /// silently undo the invalidation. The value read is still returned to the
    /// caller (the read genuinely preceded the change) but it is not cached; the
    /// next request re-reads and caches the post-change row.
    fn insert_if_unchanged(
        &mut self,
        key_hash: String,
        meta: KeyMetadata,
        now: Instant,
        seen_generation: u64,
    ) -> bool {
        if self.generation != seen_generation {
            return false;
        }
        self.insert(key_hash, meta, now);
        true
    }

    fn purge_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, entry| now.duration_since(entry.stored_at) < ttl);
        self.order.retain(|key| self.entries.contains_key(key));
    }

    /// Used by the tests to assert the cap holds; not needed on the hot path.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The key-metadata cache, built once from config on the first request.
static KEY_CACHE: OnceLock<Mutex<KeyCache>> = OnceLock::new();

fn key_cache(config: &AppConfig) -> &'static Mutex<KeyCache> {
    KEY_CACHE.get_or_init(|| {
        Mutex::new(KeyCache::new(
            Duration::from_secs(config.limits.key_metadata_cache_seconds),
            KEY_CACHE_CAPACITY,
        ))
    })
}

/// Resolve the presented key hash to its metadata, from the cache when fresh and
/// from the database otherwise.
///
/// Only a key that exists in the database is ever inserted, so an unknown key costs
/// one query and leaves no trace - a caller cannot use the cache as a memory
/// amplifier by spraying made-up tokens.
async fn load_key_metadata(
    cache: &Mutex<KeyCache>,
    pool: &SqlitePool,
    key_hash: &str,
) -> Result<KeyMetadata, AppError> {
    // The guard is scoped so the std Mutex is never held across the await below.
    let seen_generation = {
        let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(meta) = cache.get(key_hash, Instant::now()) {
            return Ok(meta);
        }
        cache.generation()
    };

    let row = sqlx::query(
        r#"
        SELECT id, account_id, models, spend_limit_idr, token_limit, rate_limit_rpm,
               expires_at, revoked_at
        FROM api_keys
        WHERE key_hash = ?
        "#,
    )
    .bind(key_hash)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Err(AppError::Unauthenticated);
    };

    let meta = KeyMetadata {
        key_id: row.get::<Hyphenated, _>("id").into_uuid(),
        account_id: row.get::<Hyphenated, _>("account_id").into_uuid(),
        models: row.get("models"),
        spend_limit_idr: row.get("spend_limit_idr"),
        token_limit: row.get("token_limit"),
        rate_limit_rpm: row.get("rate_limit_rpm"),
        expires_at: row.get("expires_at"),
        revoked_at: row.get("revoked_at"),
    };

    // Not cached if an invalidation landed while this row was being read.
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert_if_unchanged(
            key_hash.to_string(),
            meta.clone(),
            Instant::now(),
            seen_generation,
        );

    Ok(meta)
}

/// Drop one key from the metadata cache, addressed by its HASH.
///
/// The hash is the only identifier the cache ever holds — the plaintext key
/// exists nowhere but the caller's Authorization header — so this is the only
/// handle an invalidation could take, and it keeps the function safe to call
/// with a value that has already been logged or stored.
///
/// Call this whenever a key's enforcement inputs change: revocation, and a
/// narrowed limit or allowlist. `config` is needed only to reach the same
/// `OnceLock` instance the request path uses, so the process-wide cache stays
/// the single source the two paths share.
///
/// RESIDUAL STALENESS, stated plainly: this cache lives in THIS process. An
/// invalidation here does not reach any other instance behind the load
/// balancer, so a key revoked on instance A can still be honoured by instance B
/// for up to `limits.key_metadata_cache_seconds`. This narrows the window; it
/// does not make revocation global. Setting that config value to 0 is the only
/// way to make it immediate everywhere.
pub fn invalidate_key_cache(config: &AppConfig, key_hash: &str) {
    key_cache(config)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(key_hash);
}
/// What the tee saw once the upstream stream ended.
enum StreamEnd {
    /// The stream completed and the tail parsed into a usage report.
    Settled(Usage),
    /// The stream completed but reported no usage — truncated, or killed
    /// mid-way. Nothing may be billed from this.
    NoUsage,
    /// The client hung up before the stream ended. The upstream body comes with
    /// it, unread: the upstream has already generated the partial answer and
    /// still reports usage for it, so the settlement task drains the body itself
    /// instead of letting it be dropped unread.
    Hangup(Option<UpstreamStream>),
}

/// Forwards every upstream chunk the moment it arrives while retaining only the
/// last `USAGE_TAIL_CAP` bytes, so the trailing usage block can be parsed on
/// completion.
///
/// It owns the upstream stream rather than borrowing it, so it is `'static` and
/// can be handed straight to `Body::from_stream`. Dropping it — a client that
/// hangs up mid-answer — HANDS the unread upstream body to the settlement task
/// rather than dropping it: the upstream generated that answer and will report
/// usage for it, so abandoning the body would bill nothing while the provider
/// charges us. The settlement task drains it and settles what the upstream
/// reported (docs/failover.md:138-144).
struct MeteredStream {
    inner: Option<UpstreamStream>,
    tail: Vec<u8>,
    /// Handed the outcome exactly once, when the stream ends.
    settle: Option<tokio::sync::oneshot::Sender<StreamEnd>>,
    done: bool,
}

impl MeteredStream {
    fn new(inner: UpstreamStream, settle: tokio::sync::oneshot::Sender<StreamEnd>) -> Self {
        Self {
            inner: Some(inner),
            tail: Vec::new(),
            settle: Some(settle),
            done: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > USAGE_TAIL_CAP {
            let excess = self.tail.len() - USAGE_TAIL_CAP;
            self.tail.drain(..excess);
        }
    }

    /// The upstream ended. `upstream_ok` is false when the end was a transport
    /// error: a stream that died mid-way is not a success and must not be
    /// reported as one to the key pool.
    ///
    /// Returns whether a usage block was seen — the only evidence that the
    /// answer actually completed rather than being cut off.
    fn finish(&mut self, upstream_ok: bool) -> bool {
        self.done = true;

        let usage = parse_usage_from_sse(&self.tail);

        if let Some(inner) = self.inner.take() {
            if upstream_ok && usage.is_some() {
                inner.finish_ok();
            } else {
                // 0 is a transport-level cut, never a configured rate-limit
                // status: the key is freed without a cooldown.
                inner.finish_status(0);
            }
        }

        if let Some(settle) = self.settle.take() {
            let end = match usage {
                Some(usage) => StreamEnd::Settled(usage),
                None => StreamEnd::NoUsage,
            };
            let _ = settle.send(end);
        }

        usage.is_some()
    }
}

/// The client is gone, so the stream is being abandoned rather than polled.
///
/// The upstream body is HANDED OVER, not finished: it goes down the settlement
/// channel so the usage the upstream still reports for the partial answer is read
/// and billed instead of evaporating. The key lease travels with the body and is
/// reported by whoever consumes it (a transport-level cut, status 0: a client
/// hangup says nothing about the upstream, so the key is freed without a cooldown).
/// Finishing the lease HERE would drop the body unread — exactly the defect this
/// is fixing.
impl Drop for MeteredStream {
    fn drop(&mut self) {
        if self.done {
            // The stream ended on its own; `finish` already took the lease and
            // sent the outcome. A second send would be lost anyway.
            return;
        }
        self.done = true;

        match self.settle.take() {
            Some(settle) => {
                // The settlement task owns the body now and reports the lease.
                let _ = settle.send(StreamEnd::Hangup(self.inner.take()));
            }
            None => {
                // Nobody is listening for the outcome, so the lease must still be
                // freed here rather than leaked out of the key pool.
                if let Some(inner) = self.inner.take() {
                    inner.finish_status(0);
                }
            }
        }
    }
}

impl Stream for MeteredStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }

        // Bound the borrow of `inner` before touching the rest of `self`.
        let polled = match this.inner.as_mut() {
            Some(inner) => inner.bytes().poll_next_unpin(cx),
            None => Poll::Ready(None),
        };

        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                if this.finish(true) {
                    Poll::Ready(None)
                } else {
                    // The upstream closed without ever reporting usage: the
                    // answer was truncated. Saying so is the only honest
                    // outcome — a client cannot tell a partial answer from a
                    // complete one (docs/error-model.md).
                    Poll::Ready(Some(Ok(Bytes::from(error_event(
                        "upstream_incomplete",
                        "The upstream stream ended before the answer completed.",
                    )))))
                }
            }
            Poll::Ready(Some(Ok(chunk))) => {
                this.push(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finish(false);
                // The status line is long gone, so a mid-stream failure can
                // only be announced in-band (docs/error-model.md). Never
                // retried: a retry would append a second answer and bill for
                // both (docs/failover.md:133). The detail is logged, not sent:
                // a reqwest error carries the provider URL, and the client must
                // never see provider names (docs/error-model.md:159).
                warn!(error = %err, "Upstream stream failed mid-answer");
                Poll::Ready(Some(Ok(Bytes::from(error_event(
                    "upstream_failed",
                    "The upstream stream failed before the answer completed.",
                )))))
            }
        }
    }
}

/// The Retry-After for a 503, from the pool's shortest remaining cooldown.
///
/// docs/error-model.md:99-110 defines the value as "the shortest remaining
/// cooldown across the endpoint pool", floored at 1 second. A None from the
/// accessor means NO breaker is open, so there is no cooldown to report: the
/// documented 1-second floor applies, and `cause` records which path produced
/// the 503 so an operator can see why the floor applied instead of a real
/// number (docs/error-model.md:96 forbids an unexplained value).
fn no_upstream_retry_after(
    cooldown: Option<u64>,
    model: &str,
    cause: &str,
    account_id: Uuid,
) -> u64 {
    match cooldown {
        Some(secs) => secs,
        None => {
            warn!(
                account_id = %account_id,
                model = %model,
                cause = %cause,
                "503 with no open breaker: emitting the documented 1-second Retry-After floor"
            );
            1
        }
    }
}

/// Maps a failed upstream call to the error the client may see.
///
/// A transport error's Display embeds the provider URL, so it is logged and
/// withheld: the client gets the generic upstream-unavailable error instead
/// (DEFECT 3, docs/error-model.md:159). The other variants carry no provider URL
/// or hostname.
fn upstream_error(
    err: UpstreamError,
    model: &str,
    account_id: Uuid,
    upstream: &UpstreamClient,
) -> AppError {
    match err {
        UpstreamError::NoModel(_) => AppError::ModelNotAllowed(model.to_string()),
        // A breaker trip: a real cooldown exists, so report it.
        UpstreamError::NoHealthyUpstream(_) => AppError::NoUpstreamAvailable {
            retry_after_secs: no_upstream_retry_after(
                upstream.shortest_cooldown_secs(model),
                model,
                "every endpoint unhealthy",
                account_id,
            ),
        },
        UpstreamError::Transport(detail) => {
            warn!(
                account_id = %account_id,
                model = %model,
                error = %detail,
                "upstream transport error; detail withheld from client"
            );
            AppError::NoUpstreamAvailable {
                retry_after_secs: no_upstream_retry_after(
                    upstream.shortest_cooldown_secs(model),
                    model,
                    "transport error",
                    account_id,
                ),
            }
        }
        other => AppError::Internal(other.to_string()),
    }
}

/// Reads an abandoned upstream stream to its end, keeping the usage tail, so the
/// tokens the upstream generated for a customer who hung up can be settled.
///
/// This is the only thing that can be done with the body once the client is gone:
/// dropping it unread is what made a client abort free. Nothing is forwarded
/// anywhere — there is no client left — and no chunk is retained except the tail.
///
/// `None` means the upstream reported no usage. That is the documented washed case
/// (docs/failover.md:138-144) and settles nothing: token counts are never invented.
async fn drain_for_usage(stream: Option<UpstreamStream>) -> Option<Usage> {
    let mut stream = stream?;
    let mut tail: Vec<u8> = Vec::new();
    let mut transport_ok = true;

    {
        let bytes = stream.bytes();
        futures_util::pin_mut!(bytes);
        while let Some(chunk) = bytes.next().await {
            match chunk {
                Ok(chunk) => {
                    tail.extend_from_slice(&chunk);
                    if tail.len() > USAGE_TAIL_CAP {
                        let excess = tail.len() - USAGE_TAIL_CAP;
                        tail.drain(..excess);
                    }
                }
                // The upstream died mid-answer. Whatever it reported before
                // dying is all the evidence there is; a transport cut says
                // nothing about the key, so it is not a rate-limit status.
                Err(err) => {
                    transport_ok = false;
                    warn!(error = %err, "Upstream stream failed while draining an aborted request");
                    break;
                }
            }
        }
    }

    let usage = parse_usage_from_sse(&tail);

    if transport_ok && usage.is_some() {
        stream.finish_ok();
    } else {
        stream.finish_status(0);
    }

    usage
}

/// A terminal SSE error event, then the stream closes cleanly.
fn error_event(code: &str, message: &str) -> String {
    let payload = json!({
        "error": {
            "code": code,
            "message": message,
            "request_id": format!("req_{}", Uuid::new_v4().simple()),
        }
    });
    format!("event: error\ndata: {payload}\n\n")
}

/// The caller's explicit `stream` flag, if they sent one.
///
/// `None` covers both "the field is absent" and "it is present but not a bool"
/// (including `null`): neither is an explicit `false`, and the OpenAI contract
/// makes the field optional with a server default.
fn requested_stream_flag(body: &Value) -> Option<bool> {
    body.as_object()?.get("stream")?.as_bool()
}

/// The stream-flag decision, pure so it is testable without a body parser.
///
/// DEFECT 3 was that an explicit `stream: false` was silently answered with SSE:
/// the field was overwritten with `true` and the response was always
/// `text/event-stream`, so a client that asked for one JSON object got an event
/// stream and could not parse it. This gateway only serves the streaming path
/// (it is the one that carries the usage block settlement is built on), so the
/// honest answer is to refuse — the caller learns at the status line, with the
/// offending field named, instead of at the first unparseable byte.
///
/// 422, not 400: the body is well-formed and the value is a legitimate type, it
/// is simply a value this endpoint does not serve — the `validation_failed`
/// row in docs/error-model.md. `null`/absent/garbage all fall through to the
/// streaming path, because `stream: null` is what several OpenAI SDKs send for
/// "unset".
fn stream_flag_allowed(requested: Option<bool>) -> Result<(), AppError> {
    match requested {
        Some(false) => Err(AppError::ValidationFailed {
            message: "stream must be true: this endpoint serves text/event-stream only; set stream to true or omit the field".into(),
            // docs/error-model.md rule 5: name the field so the caller can
            // point at `stream` in their body without parsing the prose.
            field: "stream".into(),
        }),
        _ => Ok(()),
    }
}

/// The input-token estimate behind the pre-flight hold: ONE definition, shared
/// by the handler and the tests.
///
/// The hold is a CEILING over every settlement of the request (proxy.rs:1094-1098,
/// money.rs:400), and input tokens are NOT bytes. `len/4` holds only for ASCII: a
/// UTF-8-dense body is 3 bytes per CJK character and a byte-level BPE emits about
/// ONE token per character, so the old estimate came out ~3x short on exactly the
/// traffic this proxy sees. The shortfall is uncollectable: settlement clamps the
/// debit to what was reserved (db.rs:266-271) and logs a Partial, so the excess is
/// silently written off.
///
/// So the two halves are counted differently:
///
/// * ASCII keeps the documented ~4 bytes per token.
/// * Every byte of a multi-byte sequence is held as a whole token. This is the
///   airtight direction rather than another heuristic: a byte-level BPE that does
///   not carry a code point falls back to one token per BYTE, so it can never
///   emit more tokens than the body has bytes. The dense part therefore cannot be
///   under-counted, and the over-ask it costs is bounded by the model's output
///   ceiling, which is what the hold is really sized by (up to 384k tokens).
///
/// Floored at 1 so a zero-byte body cannot reserve zero and slip past the balance
/// guard. Kept a free function rather than inlined because the handler and the
/// test oracle each carried their own copy of `len/4`, and a rule written twice
/// is a rule that can disagree with itself.
fn estimated_input_tokens(body: &[u8]) -> u64 {
    let mut ascii_bytes = 0u64;
    let mut multi_byte_bytes = 0u64;
    for byte in body {
        if byte.is_ascii() {
            ascii_bytes += 1;
        } else {
            multi_byte_bytes += 1;
        }
    }
    (multi_byte_bytes + ascii_bytes / 4).max(1)
}

/// The inbound body with `"stream": true` guaranteed and nothing else changed.
///
/// A body that is not a JSON object is refused here — before any money moves
/// (validation precedes the debit). Malformed JSON is refused even earlier,
/// when the body is parsed.
fn force_streaming(mut value: Value) -> Result<Value, AppError> {
    value
        .as_object_mut()
        .ok_or_else(|| AppError::InvalidRequest("request body must be a JSON object".into()))?
        .insert("stream".to_string(), Value::Bool(true));
    Ok(value)
}

pub async fn chat_completions(
    State(state): State<AppState>,
    // The TCP peer. Required by `resolve_client_ip`: behind the relay it is the
    // relay, and the caller's address has to come from a header that is only
    // trusted because of where the connection came from. Before `body`, because
    // the body-consuming extractor has to be last.
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, AppError> {
    // 1. Authenticate via Bearer token
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    let presented_key = auth_header
        .strip_prefix("Bearer ")
        .ok_or(AppError::Unauthenticated)?
        .trim();

    let key_hash = hash_string(presented_key);

    // Served from the TTL cache when warm, from the database otherwise. The
    // checks below are identical either way - only the age of the row differs.
    let key = load_key_metadata(key_cache(&state.config), &state.pool, &key_hash).await?;

    let key_id = key.key_id;
    let account_id = key.account_id;
    let models_val = key.models.clone();
    let spend_limit_idr = key.spend_limit_idr;
    let token_limit = key.token_limit;
    let rate_limit_rpm = key.rate_limit_rpm;
    let expires_at = key.expires_at;
    let revoked_at = key.revoked_at;

    if revoked_at.is_some() {
        return Err(AppError::KeyRevoked);
    }

    if let Some(exp) = expires_at {
        if exp < chrono::Utc::now() {
            return Err(AppError::KeyExpired);
        }
    }

    // The upstream client is process-wide state: the pool, the key pools and
    // the breakers only do their job when every request shares them.
    let upstream = UPSTREAM.get_or_init(|| UpstreamClient::new(state.config.clone()));

    // Read only what the checks below need; the body itself goes upstream as-is.
    // Parsed once into a Value and lensed into `RequestMeta`: the stream-flag
    // contract check and the rewrite in step 5 both need the same parse.
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|err| AppError::InvalidRequest(format!("malformed request body: {err}")))?;
    let meta: RequestMeta = serde_json::from_value(parsed.clone())
        .map_err(|err| AppError::InvalidRequest(format!("malformed request body: {err}")))?;

    // An explicit `stream: false` is refused rather than silently answered with
    // SSE (DEFECT 3): this endpoint has one honest mode, and a caller who asked
    // for JSON learns it here instead of from an unparseable response body.
    stream_flag_allowed(requested_stream_flag(&parsed))?;

    // 2. Authorize model. DENY BY DEFAULT: an empty allowlist permits nothing.
    // docs/website/06-api-keys-and-limits.md:41 states the contract explicitly —
    // a key with an empty allowlist can call nothing, so a key created without
    // an explicit model list is refused (DEFECT 2). Only a model that is present
    // in the list is permitted.
    let allowed_models: Vec<String> = serde_json::from_value(models_val).unwrap_or_default();
    if !is_model_allowed(&allowed_models, &meta.model) {
        return Err(AppError::ModelNotAllowed(meta.model.clone()));
    }

    let model_cfg = state
        .config
        .models
        .iter()
        .find(|m| m.name == meta.model)
        .ok_or_else(|| AppError::ModelNotAllowed(meta.model.clone()))?;

    // 3. The key's own requests-per-minute ceiling — step 3 of the documented
    //    enforcement order (docs/failover.md:154, docs/server/api-spec.md:333):
    //    authenticate, allowlist, RATE LIMIT, key spend/token limit, wallet.
    //    Throttling before the money checks is deliberate: a hammering key must
    //    be refused without touching the wallet at all.
    //
    //    A limit of 0 means "no limit", so an unlimited key never allocates a
    //    window and the check costs one branch.
    if rate_limit_rpm > 0 {
        match check_rate_limit(key_id, rate_limit_rpm as u32, Instant::now()) {
            RateDecision::Allow => {}
            RateDecision::Deny { retry_after_secs } => {
                return Err(AppError::RateLimited { retry_after_secs });
            }
        }
    }

    // 4. The key's own rolling 30-day ceilings — step 4 of the same order:
    //    the spend limit (IDR) and the token limit. Checked BEFORE the wallet
    //    and before the upstream is called, so a key over either limit costs
    //    nothing and reaches no provider.
    //
    // Both numbers are the same ones the dashboard shows, because they are
    // literally the same reads: `keys::key_spend_used` sums
    // `usage_daily.cost_idr` and `keys::key_tokens_used` sums the three token
    // counters of the same rows, both over `keys::SPEND_WINDOW_DAYS` (trailing
    // 30 days, inclusive of today). Reusing the functions rather than
    // re-deriving the window is what keeps them from disagreeing: a key the
    // proxy blocks is a key the dashboard shows at or over its limit, and a key
    // the dashboard shows over its limit is one the proxy refuses.
    //
    // Only a limited key pays for a query. Spend is checked first because the
    // docs call it the primary limit: the report names the limit that actually
    // stopped the request, and a key at both ceilings is reported as spend.
    if spend_limit_idr > 0 {
        let spend_used_idr = key_spend_used(
            &state.pool,
            account_id,
            key_id,
            chrono::Utc::now().date_naive(),
        )
        .await?;
        if limit_reached(spend_limit_idr, spend_used_idr) {
            return Err(AppError::KeyLimitExceeded {
                details: Some(json!({
                    "reason": "spend_limit_idr_reached",
                    "limit": "spend_limit_idr",
                    "spend_limit_idr": spend_limit_idr,
                    "spend_used_idr": spend_used_idr,
                    "window_days": SPEND_WINDOW_DAYS,
                })),
            });
        }
    }

    if token_limit > 0 {
        let tokens_used = key_tokens_used(
            &state.pool,
            account_id,
            key_id,
            chrono::Utc::now().date_naive(),
        )
        .await?;
        if limit_reached(token_limit, tokens_used) {
            return Err(AppError::KeyLimitExceeded {
                details: Some(json!({
                    "reason": "token_limit_reached",
                    "limit": "token_limit",
                    "token_limit": token_limit,
                    "tokens_used": tokens_used,
                    "window_days": SPEND_WINDOW_DAYS,
                })),
            });
        }
    }

    // Pre-flight worst-case reservation, taken BEFORE routing.
    //
    // Input is estimated from the body's CODE POINTS, not its raw length, at ~4
    // per token and floored at 1 (see `estimated_input_tokens`: a byte count
    // under-reserves UTF-8-dense input); the output ceiling is the request's own
    // cap, clamped to the hard limit. The reservation must cover the dearest
    // endpoint in the pool, not the one that happens to serve the request —
    // otherwise a failover to a dearer provider can overdraw the balance
    // (docs/failover.md:162-165).
    let estimated_input = estimated_input_tokens(&body);
    // The reservation must cover the worst case the upstream can actually emit,
    // not just the client's cap. A request that omits max_tokens (or asks for
    // less than the model can produce) still lets the upstream stream up to the
    // model's own ceiling, so the hold is taken against
    // max(requested, model.max_output_tokens) and then clamped to the hard limit.
    // The earlier code reserved only min(requested, hard) - which with the
    // defaults left a 4096-token hold guarding a model that emits up to 384000
    // tokens, and an over-long answer overdrew the wallet. The tradeoff is
    // deliberate: when the client asks for less than the model can give we hold
    // more than their cap (never less), because the bound's job is to never
    // under-reserve. The true cost is charged at settlement regardless, so only
    // the over-askers benefit from the extra headroom and the under-reserved case
    // is gone.
    let max_output = meta
        .max_tokens
        .unwrap_or(0)
        .max(model_cfg.max_output_tokens)
        .min(state.config.streaming.hard_max_output_tokens);

    // One reservation per endpoint, and the DEAREST wins: the hold is taken
    // before routing and may be served by any endpoint in the pool, so it must
    // cover the dearest one or a failover to a dearer provider overdraws the
    // balance (docs/failover.md:162-165). An endpoint may override the model's
    // peak rates, so each is priced at its OWN rates.
    //
    // The rule itself lives on ModelConfig, in ONE place: the upstream client
    // used to carry a second copy that read only model-level rates, which made
    // the endpoints tie and the dearest-endpoint rule a no-op - a mutation
    // swapping max() for min() was therefore undetectable. Sharing the one
    // definition is what makes the rule real and testable.
    let reservation = model_cfg.worst_case_reservation_idr(estimated_input, max_output);

    // 5. The inbound body with `stream: true` guaranteed. Parsed BEFORE the hold so
    //    a malformed body can never take money: validation precedes the debit.
    let upstream_body = force_streaming(parsed)?;

    // 6. HOLD the reservation. This is the money step: the wallet is debited by
    //    the worst case in a guarded, committed transaction BEFORE the upstream is
    //    called, and the release happens at settlement.
    //
    // The guard is `balance_idr >= ?1` inside the UPDATE, not a read: SQLite
    // admits one writer at a time, so two concurrent requests from the same
    // account cannot both pass against one stale balance, and only as many as the
    // balance can cover are admitted. The previous
    // code read the balance here and debited nothing, which let an account holding
    // 1 IDR run unbounded concurrent expensive requests — and with
    // `allow_negative_balance_overdraft = true` the 402 branch below was dead, so
    // nothing refused them at all.
    //
    // `allow_negative_balance_overdraft` is deliberately NOT consulted here: a hold
    // is not a balance rule, it is the mechanism that makes the balance rule real.
    // The balance can never go negative (docs/decisions.md D3) — the CHECK
    // constraint is never bypassed and no unaffordable request is admitted.
    let reservation_ref = format!("reserve_{}", Uuid::new_v4().simple());
    let held =
        reserve_balance_transaction(&state.pool, account_id, reservation, Some(&reservation_ref))
            .await?;

    let reserved_idr = match held {
        ReservationResult::Held { reserved_idr, .. } => reserved_idr,
        // Nothing held: the wallet cannot cover the worst case. 402, and no request
        // was made — a customer is never charged for a request that was refused.
        ReservationResult::Insufficient { balance_idr } => {
            return Err(AppError::InsufficientBalance {
                details: Some(json!({
                    "balance_idr": balance_idr,
                    "required_idr": reservation
                })),
            });
        }
        // A zero-token worst case is not a refusal; there is simply nothing to hold.
        ReservationResult::Zero => 0,
    };

    // The hold is now out of the wallet. Carry it in a guard so that EVERY exit
    // before the settlement transaction claims it gives the money back - a client
    // reset that drops this future mid-await, a '?', or an early return. The
    // settlement task takes ownership of the guard; if settlement commits the
    // debit it defuses the guard (the hold was already credited back in that same
    // transaction) and on every other path the guard's Drop releases it.
    let mut guard = ReservationGuard::new(
        &state.pool,
        account_id,
        reserved_idr,
        &reservation_ref,
        &meta.model,
    );

    // Every admission check has passed and the hold is out of the wallet, so
    // this request is actually being served — which is what makes its source
    // worth counting. Refused requests are not: an attacker must not be able to
    // move a key's distinct-IP figure without spending money.
    record_request_source(&state, key_id, peer, &headers);

    // 7. Route and stream. The raw inbound body goes up with `stream: true`
    // ensured; the client rewrites `model` to the endpoint's upstream_model.
    //
    // The hold is out of the wallet from here on, so every exit must give it back.
    // The upstream was never reached, so there is nothing to bill: releasing the
    // whole hold is exactly the net-zero the ledger needs.
    let stream = match upstream.stream_chat(&meta.model, upstream_body).await {
        Ok(stream) => stream,
        Err(err) => {
            // Upstream was never reached, so there is nothing to bill: the whole
            // hold must come back. Defuse the guard so its Drop does not release
            // the same hold a second time, then release synchronously here.
            guard.defuse();
            release_quietly(
                &state.pool,
                account_id,
                reserved_idr,
                &reservation_ref,
                &meta.model,
            )
            .await;
            return Err(upstream_error(err, &meta.model, account_id, upstream));
        }
    };

    let endpoint = stream.endpoint_name().to_string();

    info!(
        account_id = %account_id,
        endpoint = %endpoint,
        reservation,
        "Proxy streaming from upstream"
    );

    // 8. Settlement runs detached: the client is served first, and a billing
    // failure must not turn a completed answer into an error the customer never
    // saw. The tee reports back through this channel when the stream ends —
    // including when the client hangs up, in which case it hands the unread
    // upstream body over so the usage the upstream still reports is settled.
    let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<StreamEnd>();

    tokio::spawn(settle_after_stream(
        settle_rx,
        state.pool.clone(),
        state.config.clone(),
        state.events.clone(),
        account_id,
        key_id,
        meta.model.clone(),
        reservation,
        // Hand the guard to the settlement task. If the task commits the debit it
        // defuses the guard; on any earlier exit the guard's Drop is the safety
        // net that gives the hold back.
        guard,
    ));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(MeteredStream::new(stream, settle_tx)))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// What a settlement should do with a finished request, from the upstream's own
/// evidence alone.
#[derive(Debug, PartialEq, Eq)]
enum SettlementPlan {
    /// The upstream reported usage: bill exactly those tokens, releasing the hold
    /// inside the same settlement transaction.
    Bill(Usage),
    /// No usage was reported — a truncated stream, a client hangup whose upstream
    /// said nothing, or a lost settlement channel. Settle NOTHING and give the whole
    /// hold back. This is the documented washed case (docs/failover.md:138-144);
    /// token counts are never invented to fill the gap.
    Wash,
}

/// The billing decision, pure so it is testable without a database or a stream.
///
/// The single rule: bill what the upstream reported, and nothing when it reported
/// nothing. A partial answer is billed by exactly the tokens the upstream says it
/// generated — the client receiving less of the answer than it asked for does not
/// change what the provider charged us.
fn settlement_plan(usage: Option<Usage>) -> SettlementPlan {
    match usage {
        Some(usage) => SettlementPlan::Bill(usage),
        None => SettlementPlan::Wash,
    }
}

/// Bills the request once the upstream stream has ended, and gives back whatever
/// of the reservation the real cost did not use.
///
/// Every exit from here releases the hold: `reserved_idr` is out of the wallet for
/// the whole upstream call, and the settlement transaction credits it back before
/// charging the true cost, so the ledger nets to exactly `-cost_idr`.
#[allow(clippy::too_many_arguments)]
async fn settle_after_stream(
    end: tokio::sync::oneshot::Receiver<StreamEnd>,
    pool: SqlitePool,
    config: Arc<AppConfig>,
    events: Arc<RealtimeHub>,
    account_id: Uuid,
    key_id: Uuid,
    model: String,
    reserved_idr: i64,
    // Owns the hold. The task takes it so that EVERY exit releases the hold:
    // the guard's Drop is the safety net (client hangup, task cancellation, a
    // panic), and the explicit `defuse` calls claim ownership only after a path
    // has already credited the hold back in its own transaction. A guard can
    // therefore never double-credit the wallet.
    mut guard: ReservationGuard,
) {
    // Each arm reports only what the upstream actually said; the single rule below
    // then decides bill-or-wash, so there is exactly one place a hold can be
    // released and exactly one place a charge can be made.
    let reported = match end.await {
        Ok(StreamEnd::Settled(usage)) => Some(usage),
        Ok(StreamEnd::NoUsage) => {
            // docs/failover.md: a stream that ends without a usage block was
            // truncated. Settle nothing rather than inventing token counts.
            warn!(
                account_id = %account_id,
                model = %model,
                "Upstream stream ended without usage; nothing settled"
            );
            None
        }
        Ok(StreamEnd::Hangup(stream)) => {
            // The client hung up mid-answer. The upstream keeps generating and still
            // reports usage for what it produced, so the abandoned body is drained to
            // its end rather than dropped: the tokens were consumed upstream and must
            // be billed. There is no client left to send the answer to, so this is
            // logged quietly rather than treated as a delivery failure.
            let drained = drain_for_usage(stream).await;
            match drained {
                Some(usage) => info!(
                    account_id = %account_id,
                    model = %model,
                    input_tokens = usage.input_tokens,
                    cache_read_tokens = usage.cache_read_tokens,
                    output_tokens = usage.output_tokens,
                    "Client hung up; settling the usage the upstream still reported"
                ),
                None => info!(
                    account_id = %account_id,
                    model = %model,
                    "Client hung up; upstream reported no usage, nothing settled"
                ),
            }
            drained
        }
        Err(_) => {
            // The settlement channel closed with no outcome at all. The tee sends on
            // drop, so this is a task-level loss rather than a client hangup; the hold
            // must not be left stranded either way.
            warn!(
                account_id = %account_id,
                model = %model,
                reserved_idr,
                "Settlement channel closed without an outcome; releasing the reservation"
            );
            None
        }
    };

    // Bill exactly what the upstream reported, and give the whole hold back when it
    // reported nothing. No usage means no charge — never an invented one.
    let SettlementPlan::Bill(usage) = settlement_plan(reported) else {
        // No usage reported: nothing billed, so the whole hold must come back.
        // Defuse the guard (its Drop must not release a second time) then release.
        guard.defuse();
        release_quietly(
            &pool,
            account_id,
            reserved_idr,
            &guard.reservation_ref,
            &guard.model,
        )
        .await;
        return;
    };

    let Some(model_cfg) = config.models.iter().find(|m| m.name == model) else {
        // The model was routed moments ago, so this cannot happen without a config
        // reload mid-request. The hold must still come back: a stranded reservation
        // is money the customer cannot spend.
        warn!(
            account_id = %account_id,
            model = %model,
            reserved_idr,
            "Settled model is no longer configured; releasing the reservation"
        );
        // Stranded hold on a routing race: give it back, defusing the guard so
        // its Drop does not release a second time.
        guard.defuse();
        release_quietly(
            &pool,
            account_id,
            reserved_idr,
            &guard.reservation_ref,
            &guard.model,
        )
        .await;
        return;
    };

    // Cache-read tokens stay their own counter, never folded into input: they
    // are a subset of the upstream's prompt_tokens and cost ~50x less
    // (docs/local-development.md:113-117).
    let cost_idr = calculate_token_cost_idr(
        model_cfg.price,
        usage.input_tokens as u64,
        model_cfg.rates.input_peak,
        usage.cache_read_tokens as u64,
        model_cfg.rates.cache_read_peak,
        usage.output_tokens as u64,
        model_cfg.rates.output_peak,
    );

    match debit_usage_transaction(
        &pool,
        account_id,
        Some(key_id),
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.output_tokens,
        cost_idr,
        // Pair this settlement's release with the exact hold that reserved the
        // money. The ref ties the `-reserved` ledger row, the `+reserved`
        // release, and the `-cost` charge into one traceable reservation. A
        // missing ref (the old `None`) made the release unrecoverable as a
        // stranded hold in the ledger.
        Some(&guard.reservation_ref),
        reserved_idr,
    )
    .await
    {
        Ok(UsageSettlement::Settled { new_balance }) => {
            // The debit already credited the hold back in the same committed
            // transaction, so claim ownership now: defusing the guard stops its
            // Drop from releasing the same hold a second time.
            guard.defuse();
            info!(
                account_id = %account_id,
                cost_idr,
                new_balance,
                input_tokens = usage.input_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                output_tokens = usage.output_tokens,
                "Proxy request settled"
            );

            // Only now that the debit has committed. Emitting before commit
            // could announce a balance that then rolls back
            // (docs/realtime.md:146-157). Scoped to account_id so only this
            // account's dashboard receives it (DEFECT 1).
            publish_balance(&events, account_id, new_balance);

            // The event carries today's CUMULATIVE totals, not this request's
            // delta: absolute values make a lost event self-healing
            // (docs/realtime.md:93). If the read fails the balance event has
            // already gone out and the next settlement will correct the
            // usage figures, so this is logged, not fatal.
            match todays_usage(&pool, account_id).await {
                Ok(totals) => publish_usage(&events, account_id, totals),
                // A decode/aggregate failure here is not cosmetic: it silently
                // drops the usage event subscribers rely on, and a type mismatch
                // (Sqlite returns NUMERIC for SUM(bigint) while the row is
                // decoded into i64) is exactly how that went unnoticed. Loud, with
                // the account and the model, so it is diagnosable from the log.
                Err(err) => error!(
                    account_id = %account_id,
                    model = %model,
                    error = %err,
                    "Usage event skipped: today's totals could not be read"
                ),
            }
        }
        // The balance ran out mid-request. The answer was already streamed and
        // the usage WAS reported, so it is on the books: the debit was clamped,
        // the real counters were recorded, and the difference is money consumed
        // and not collected. An error, not a warning - an undercharge nobody
        // sees is the same class of defect as a refund nobody sees.
        //
        // The money layer (db.rs) already logged the shortfall with the amounts;
        // this line adds the request context an operator needs to place it.
        Ok(UsageSettlement::Partial {
            new_balance,
            debited_idr,
            shortfall_idr,
        }) => {
            // The clamped debit still credited the hold back in its transaction
            // (see db.rs), so claim it here: defusing stops the guard's Drop from
            // releasing the same hold a second time.
            guard.defuse();
            error!(
                account_id = %account_id,
                key_id = %key_id,
                model = %model,
                cost_idr,
                debited_idr,
                shortfall_idr,
                new_balance,
                input_tokens = usage.input_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                output_tokens = usage.output_tokens,
                "Proxy request UNDERCHARGED: balance could not cover the reported usage"
            );

            // The clamped debit did commit, so the balance the dashboard shows is
            // real, and so are the usage totals that back the 30-day spend.
            publish_balance(&events, account_id, new_balance);
            match todays_usage(&pool, account_id).await {
                Ok(totals) => publish_usage(&events, account_id, totals),
                // A decode/aggregate failure here is not cosmetic: it silently
                // drops the usage event subscribers rely on, and a type mismatch
                // (Sqlite returns NUMERIC for SUM(bigint) while the row is
                // decoded into i64) is exactly how that went unnoticed. Loud, with
                // the account and the model, so it is diagnosable from the log.
                Err(err) => error!(
                    account_id = %account_id,
                    model = %model,
                    error = %err,
                    "Usage event skipped: today's totals could not be read"
                ),
            }
        }
        // Only a real failure reaches here now: the wallet being short is a
        // recorded outcome, not an error. What is left is the database being
        // unreachable, or a clamped debit that still did not apply. The hold was
        // NOT released by the debit (that only happens on the Ok arms), so the
        // money is sitting out of the wallet. Defuse the guard and release it: a
        // failed settlement must never strand the customer's money. This is the
        // exact bug this fix closes - the old code warned and walked away.
        Err(err) => {
            warn!(
                account_id = %account_id,
                cost_idr,
                error = %err,
                "Proxy settlement failed"
            );
            guard.defuse();
            release_quietly(
                &pool,
                account_id,
                reserved_idr,
                &guard.reservation_ref,
                &guard.model,
            )
            .await;
        }
    }
}
/// Gives a reservation back, logging a failure instead of discarding it.
///
/// A release that does not land leaves the customer's money debited against a
/// request that was never billed — the mirror image of the defect this fix closes
/// — so a failure is loud even though the client is long gone.
async fn release_quietly(
    pool: &SqlitePool,
    account_id: Uuid,
    reserved_idr: i64,
    reservation_ref: &str,
    model: &str,
) {
    match release_reservation_transaction(pool, account_id, reserved_idr, Some(reservation_ref))
        .await
    {
        Ok(_) => {}
        Err(err) => error!(
            account_id = %account_id,
            model = %model,
            reserved_idr,
            error = %err,
            "Failed to release the reservation; the hold is stranded"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // The 503 Retry-After decision (docs/error-model.md:99-112)
    // ---------------------------------------------------------------------

    #[test]
    fn an_available_cooldown_is_passed_through_unchanged() {
        // docs/error-model.md:99-110: the value IS the pool's shortest
        // remaining cooldown, so it must not be re-rounded or replaced.
        assert_eq!(
            no_upstream_retry_after(Some(30), "flash", "t", Uuid::nil()),
            30
        );
        assert_eq!(
            no_upstream_retry_after(Some(1), "flash", "t", Uuid::nil()),
            1
        );
        assert_eq!(
            no_upstream_retry_after(Some(900), "flash", "t", Uuid::nil()),
            900
        );
    }

    #[test]
    fn no_open_breaker_falls_back_to_the_documented_one_second_floor() {
        // docs/error-model.md:112 - "Floor it at 1 second. A zero or negative
        // value is a malformed header." The floor is the documented value, not
        // an estimate; the warn! inside records the cause.
        assert_eq!(
            no_upstream_retry_after(None, "flash", "transport error", Uuid::nil()),
            1
        );
        assert_eq!(
            no_upstream_retry_after(None, "flash", "every endpoint unhealthy", Uuid::nil()),
            1,
            "the floor must never be 0, which would be a malformed header"
        );
    }

    fn meta(revoked_at: Option<chrono::DateTime<chrono::Utc>>) -> KeyMetadata {
        KeyMetadata {
            key_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            models: json!(["deepseek-flash"]),
            spend_limit_idr: 0,
            token_limit: 0,
            rate_limit_rpm: 0,
            expires_at: None,
            revoked_at,
        }
    }

    fn cache(ttl_secs: u64, capacity: usize) -> KeyCache {
        KeyCache::new(Duration::from_secs(ttl_secs), capacity)
    }

    #[test]
    fn a_fresh_entry_is_a_hit() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        let stored = meta(None);

        c.insert("hash-a".into(), stored.clone(), now);

        assert_eq!(c.get("hash-a", now), Some(stored));
        // An unknown key is always a miss, never a default. The handler turns
        // that miss into a fresh read.
        assert_eq!(c.get("hash-b", now), None);
    }

    #[test]
    fn an_entry_at_or_past_its_ttl_is_a_miss() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);

        // One second inside the window is still a hit.
        assert!(c.get("hash-a", now + Duration::from_secs(59)).is_some());
        // At the boundary and beyond it the entry must not be served: the
        // handler re-reads instead.
        assert_eq!(c.get("hash-a", now + Duration::from_secs(60)), None);
        assert_eq!(c.get("hash-a", now + Duration::from_secs(61)), None);
    }

    #[test]
    fn a_zero_ttl_disables_the_cache() {
        // The config value is the escape hatch: 0 means every lookup misses, so
        // revocation is immediate again at the cost of a read per request.
        let mut c = cache(0, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);

        assert_eq!(c.get("hash-a", now), None);
    }

    #[test]
    fn the_capacity_cap_evicts_and_stays_bounded() {
        let mut c = cache(60, 2);
        let now = Instant::now();

        c.insert("a".into(), meta(None), now);
        c.insert("b".into(), meta(None), now);
        c.insert("c".into(), meta(None), now);

        assert_eq!(c.len(), 2, "the cap must hold");
        // FIFO: the oldest insertion went, the two newest stayed.
        assert_eq!(c.get("a", now), None);
        assert!(c.get("b", now).is_some());
        assert!(c.get("c", now).is_some());
        // The order queue must not leak entries the map has dropped, or it
        // would grow without bound while the map stayed capped.
        assert_eq!(c.order.len(), c.len());
    }

    #[test]
    fn an_expired_entry_is_reclaimed_before_a_live_one_is_evicted() {
        let mut c = cache(60, 2);
        let start = Instant::now();

        c.insert("stale".into(), meta(None), start);
        c.insert("live".into(), meta(None), start);

        // Well past the TTL and at capacity: the dead entry is reclaimed rather
        // than evicting the one still inside its window.
        let later = start + Duration::from_secs(120);
        c.insert("new".into(), meta(None), later);

        assert_eq!(c.get("stale", later), None);
        assert!(c.get("new", later).is_some());
        assert_eq!(c.len(), 1, "both expired entries were reclaimed");
    }

    #[test]
    fn re_inserting_a_key_does_not_duplicate_its_order_slot() {
        let mut c = cache(60, 4);
        let now = Instant::now();

        c.insert("a".into(), meta(None), now);
        c.insert("a".into(), meta(None), now);
        c.insert("a".into(), meta(None), now);

        assert_eq!(c.len(), 1);
        assert_eq!(c.order.len(), 1, "one map slot, one queue slot");
    }

    #[test]
    fn a_revoked_key_is_not_served_beyond_the_ttl() {
        let mut c = cache(60, 16);
        let now = Instant::now();

        // Cached while the key was still live.
        c.insert("hash-a".into(), meta(None), now);

        // Inside the window the stale live record is still honoured - the
        // documented staleness cost, asserted rather than assumed.
        assert!(c.get("hash-a", now + Duration::from_secs(30)).is_some());

        // Past the window the entry is gone, so the handler re-reads and sees
        // the revocation. The cache cannot extend a revoked key's life.
        assert_eq!(c.get("hash-a", now + Duration::from_secs(61)), None);
    }

    #[test]
    fn a_purge_drops_expired_entries_and_their_order_slots() {
        let mut c = cache(60, 16);
        let start = Instant::now();

        c.insert("old".into(), meta(None), start);
        c.insert("new".into(), meta(None), start + Duration::from_secs(120));

        c.purge_expired(start + Duration::from_secs(121));

        assert_eq!(c.len(), 1);
        assert_eq!(c.order.len(), 1);
        assert_eq!(c.get("old", start + Duration::from_secs(121)), None);
    }

    #[test]
    fn cached_metadata_carries_every_field_the_checks_read() {
        // A hit must drive the same decision as a fresh read, so the record has
        // to carry the account, the key id, the model allowlist and BOTH dates.
        let revoked = chrono::Utc::now();
        let stored = meta(Some(revoked));

        assert_eq!(stored.revoked_at, Some(revoked));
        assert_eq!(stored.models, json!(["deepseek-flash"]));
        assert_ne!(stored.key_id, Uuid::nil());
        assert_ne!(stored.account_id, Uuid::nil());
    }

    /// A completed stream that reported usage is billed by exactly what the
    /// upstream said, and a stream that reported nothing is washed — never billed
    /// against invented counts (docs/failover.md:138-144).
    #[test]
    fn a_reported_usage_is_billed_and_a_missing_one_is_washed() {
        let full = Usage {
            input_tokens: 11,
            cache_read_tokens: 5,
            output_tokens: 40,
        };
        assert_eq!(settlement_plan(Some(full)), SettlementPlan::Bill(full));

        // The documented washed case: the upstream never reported usage.
        assert_eq!(settlement_plan(None), SettlementPlan::Wash);
    }

    /// A partial answer is still billed by the tokens the upstream generated —
    /// the client receiving less than it asked for does not change what the
    /// provider charged us. Only "no usage at all" is washed.
    #[test]
    fn a_partial_answer_is_billed_while_no_report_is_washed() {
        let partial = Usage {
            input_tokens: 3,
            cache_read_tokens: 0,
            output_tokens: 1,
        };
        assert_eq!(
            settlement_plan(Some(partial)),
            SettlementPlan::Bill(partial)
        );

        // Zero-valued but PRESENT usage is a report, not an absence: the upstream
        // spoke, so it is billed rather than washed.
        let zero = Usage::default();
        assert_eq!(settlement_plan(Some(zero)), SettlementPlan::Bill(zero));
        assert_ne!(settlement_plan(Some(zero)), SettlementPlan::Wash);
    }

    // DEFECT 2 regression: an empty allowlist denies everything. A key created
    // without an explicit model list must not be able to call any model, least
    // of all the dearest one in the pool.
    #[test]
    fn empty_allowlist_denies_every_model() {
        assert!(!is_model_allowed(&[], "deepseek-flash"));
        assert!(!is_model_allowed(&[], "gpt-4o"));
        assert!(!is_model_allowed(&[], ""));
    }

    #[test]
    fn listed_model_is_allowed() {
        let allowed = vec!["deepseek-flash".to_string(), "gpt-4o-mini".to_string()];
        assert!(is_model_allowed(&allowed, "deepseek-flash"));
        assert!(is_model_allowed(&allowed, "gpt-4o-mini"));
    }

    #[test]
    fn unlisted_model_is_denied() {
        let allowed = vec!["deepseek-flash".to_string()];
        assert!(!is_model_allowed(&allowed, "gpt-4o"));
        // A non-empty list does not fall back to allowing anything else.
        assert!(!is_model_allowed(&allowed, "deepseek-flash-v2"));
    }

    // DEFECT 1 regression: the per-key 30-day spend limit now has a decision on
    // the request path. 0 is unlimited, the ceiling itself already blocks, and
    // spending past it keeps blocking.
    #[test]
    fn a_limit_blocks_at_and_above_the_ceiling() {
        assert!(!limit_reached(0, 0), "0 means no limit");
        assert!(
            !limit_reached(0, 10_000_000),
            "0 never blocks, whatever was used"
        );
        assert!(
            !limit_reached(50_000, 49_999),
            "one unit under the ceiling is fine"
        );
        assert!(
            limit_reached(50_000, 50_000),
            "exactly at the ceiling is out of budget"
        );
        assert!(
            limit_reached(50_000, 60_000),
            "over the ceiling stays blocked"
        );
        // A negative limit is refused at key-management time, so it can never be
        // stored; it is treated as "no limit" rather than blocking every request.
        assert!(!limit_reached(-1, 10));
    }

    // The TOKEN limit is the same rule in a different unit, so it inherits the
    // same boundary — asserted separately because the two are separate columns
    // and a future divergence would otherwise go unnoticed.
    #[test]
    fn a_token_limit_blocks_at_and_above_the_ceiling() {
        assert!(!limit_reached(0, 1_000_000), "0 tokens means no limit");
        assert!(!limit_reached(1_000, 999));
        assert!(limit_reached(1_000, 1_000));
        assert!(limit_reached(1_000, 1_001));
    }

    // The rate window: `limit_rpm` requests pass per minute, the next is refused
    // with a Retry-After pointing at the rollover.
    #[test]
    fn the_rate_window_admits_the_limit_then_refuses() {
        let start = Instant::now();
        let mut w = RateWindow::new(start);

        for i in 0..3 {
            assert_eq!(
                w.check(3, start),
                RateDecision::Allow,
                "request {i} is within 3 rpm"
            );
        }
        assert_eq!(
            w.check(3, start),
            RateDecision::Deny {
                retry_after_secs: 60
            },
            "the fourth request in the same minute is refused"
        );
    }

    #[test]
    fn the_rate_window_rolls_over_and_forgets_the_previous_minute() {
        let start = Instant::now();
        let mut w = RateWindow::new(start);
        for _ in 0..3 {
            w.check(3, start);
        }
        assert!(matches!(w.check(3, start), RateDecision::Deny { .. }));

        // One second short of the minute is still the same window.
        assert!(matches!(
            w.check(3, start + Duration::from_secs(59)),
            RateDecision::Deny { .. }
        ));

        // At the boundary the window restarts: the counter is zeroed, not just
        // aged, so a key at its ceiling gets a full allowance again.
        assert_eq!(
            w.check(3, start + Duration::from_secs(60)),
            RateDecision::Allow
        );
        assert_eq!(
            w.check(3, start + Duration::from_secs(60)),
            RateDecision::Allow
        );
    }

    #[test]
    fn a_denied_rate_check_reports_a_retry_after_of_at_least_one_second() {
        let start = Instant::now();
        let mut w = RateWindow::new(start);
        w.check(1, start);

        // 59.5s into the window: the honest remainder rounds DOWN to 59s, and the
        // refusal must never advertise 0, which would invite an immediate retry
        // that is refused again.
        let late = start + Duration::from_millis(59_500);
        assert_eq!(
            w.check(1, late),
            RateDecision::Deny {
                retry_after_secs: 1
            },
            "sub-second remainders floor at 1"
        );
    }

    // A separate key gets a separate window: one key at its ceiling must not
    // throttle another.
    #[test]
    fn rate_windows_are_per_key() {
        let now = Instant::now();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        assert_eq!(check_rate_limit(a, 1, now), RateDecision::Allow);
        assert!(matches!(
            check_rate_limit(a, 1, now),
            RateDecision::Deny { .. }
        ));
        assert_eq!(
            check_rate_limit(b, 1, now),
            RateDecision::Allow,
            "a different key has its own counter"
        );
    }

    // DEFECT: the capacity backstop ran BEFORE the requesting key was looked
    // up, and evicted `keys().next()` — an arbitrary entry — as soon as the
    // table was at capacity. When the only windows present are live (the sole
    // case the preceding `retain` leaves behind), the victim was a window that
    // was still being enforced, and dropping it silently RESET that key's
    // minute: its next request started from zero and was ALLOWED even though it
    // was sitting on its ceiling. That is fail-OPEN on a money-adjacent path —
    // a key at its ceiling buys extra requests by crowding the table.
    //
    // Capacity 1 makes the victim unambiguous: the only entry is the key being
    // limited, so the defect showed up as the limiter resetting the very window
    // it was enforcing, on that key's own request.
    #[test]
    fn a_key_at_its_ceiling_cannot_reset_its_own_window() {
        let now = Instant::now();
        let capacity = 1;
        let key = Uuid::new_v4();
        let mut windows = HashMap::new();

        assert_eq!(
            check_rate_limit_in(&mut windows, capacity, key, 1, now),
            RateDecision::Allow
        );
        // The key is at its ceiling. Its second request must be refused, and the
        // table being full must not hand it a fresh window on the way in.
        assert!(
            matches!(
                check_rate_limit_in(&mut windows, capacity, key, 1, now),
                RateDecision::Deny { .. }
            ),
            "a key at its ceiling must be refused, not handed a fresh window by eviction"
        );
        assert_eq!(
            windows.get(&key).map(|w| w.count),
            Some(1),
            "the count that justifies the refusal must survive the request"
        );

        // Repeated attempts keep being refused: the reset is not merely deferred
        // to the next call.
        for _ in 0..5 {
            assert!(matches!(
                check_rate_limit_in(&mut windows, capacity, key, 1, now),
                RateDecision::Deny { .. }
            ));
        }
        assert_eq!(windows.get(&key).map(|w| w.count), Some(1));
    }

    // The residual tradeoff, pinned so it stays deliberate. Once the table is
    // full of LIVE windows a new key has nowhere to go, so the backstop evicts
    // an arbitrary live window (fail-OPEN) to keep memory bounded. This is
    // reachable only at RATE_WINDOW_CAPACITY simultaneously-live keys, and only
    // after the expired sweep found nothing to reclaim. If this ever becomes
    // unacceptable the fix is shared/evictable state, not a bigger map.
    #[test]
    fn the_live_window_backstop_stays_bounded_and_is_the_documented_fail_open() {
        let now = Instant::now();
        let capacity = 2;
        let mut windows = HashMap::new();

        for _ in 0..capacity {
            check_rate_limit_in(&mut windows, capacity, Uuid::new_v4(), 1, now);
        }
        assert_eq!(windows.len(), capacity);

        // Every window is live, so the newcomer must displace one of them.
        assert_eq!(
            check_rate_limit_in(&mut windows, capacity, Uuid::new_v4(), 1, now),
            RateDecision::Allow,
            "a key with no window of its own is still answered"
        );
        assert_eq!(windows.len(), capacity, "the table never exceeds its cap");
    }

    // The backstop must still reclaim memory and still answer a key that has no
    // window of its own. Reclaiming a window whose minute is over resets
    // nothing: that key had already rolled over.
    #[test]
    fn the_backstop_reclaims_expired_windows_without_wedging() {
        let now = Instant::now();
        let capacity = 2;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut windows = HashMap::new();

        assert_eq!(
            check_rate_limit_in(&mut windows, capacity, a, 1, now),
            RateDecision::Allow
        );
        assert_eq!(
            check_rate_limit_in(&mut windows, capacity, b, 1, now),
            RateDecision::Allow
        );
        assert_eq!(windows.len(), capacity, "the table is full of live windows");

        // A third key while the table is full is still answered...
        assert_eq!(
            check_rate_limit_in(&mut windows, capacity, Uuid::new_v4(), 1, now),
            RateDecision::Allow
        );
        assert!(windows.len() <= capacity, "memory stays bounded");

        // ...and once the live windows roll over the table is reclaimed rather
        // than wedged full forever.
        let later = now + RATE_WINDOW + Duration::from_secs(1);
        assert_eq!(
            check_rate_limit_in(&mut windows, capacity, Uuid::new_v4(), 1, later),
            RateDecision::Allow
        );
        assert!(!windows.contains_key(&a), "an expired window is reclaimed");
        assert!(!windows.contains_key(&b), "an expired window is reclaimed");
        assert!(windows.len() <= capacity, "memory stays bounded");
    }

    // DEFECT 2 regression: the invalidation drops the entry, and a lookup that
    // read the row before the invalidation cannot put it back.
    #[test]
    fn invalidation_drops_one_key_and_stops_a_stale_re_insert() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);
        c.insert("hash-b".into(), meta(None), now);

        c.remove("hash-a");

        assert_eq!(
            c.get("hash-a", now),
            None,
            "the revoked key is gone immediately"
        );
        assert!(
            c.get("hash-b", now).is_some(),
            "only the named key is dropped"
        );
        assert_eq!(c.order.len(), c.len(), "no orphaned eviction slot");
    }

    #[test]
    fn a_lookup_that_predates_an_invalidation_does_not_re_cache() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);

        // The request read this generation, then went to the database. A
        // revocation lands while it is in flight.
        let seen = c.generation();
        c.remove("hash-a");

        // Without the generation guard this would restore the pre-revocation
        // record for a fresh full TTL, silently undoing the invalidation.
        assert!(!c.insert_if_unchanged("hash-a".into(), meta(None), now, seen));
        assert_eq!(c.get("hash-a", now), None);

        // A lookup that started after the invalidation caches normally.
        let seen = c.generation();
        assert!(c.insert_if_unchanged("hash-a".into(), meta(None), now, seen));
        assert!(c.get("hash-a", now).is_some());
    }

    // DEFECT 3 regression: an explicit `stream: false` is refused rather than
    // answered with SSE; absent, null and non-bool values keep the streaming
    // default (OpenAI SDKs send `stream: null` for "unset").
    #[test]
    fn an_explicit_non_streaming_request_is_refused_not_silently_streamed() {
        let err = stream_flag_allowed(Some(false)).unwrap_err();
        assert!(matches!(&err, AppError::ValidationFailed { .. }));
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.to_string().contains("stream"), "the message names it");

        // docs/error-model.md rule 5: the machine-readable field name, so the
        // UI highlights the input instead of matching on the prose.
        assert_eq!(
            err.details(),
            Some(json!({ "field": "stream" })),
            "a 422 from this site must carry details.field = \"stream\""
        );

        assert!(stream_flag_allowed(Some(true)).is_ok());
        assert!(
            stream_flag_allowed(None).is_ok(),
            "absent keeps the default"
        );
    }

    #[test]
    fn the_stream_flag_is_read_only_from_a_real_bool() {
        assert_eq!(
            requested_stream_flag(&json!({"stream": false})),
            Some(false)
        );
        assert_eq!(requested_stream_flag(&json!({"stream": true})), Some(true));
        assert_eq!(requested_stream_flag(&json!({"stream": null})), None);
        assert_eq!(requested_stream_flag(&json!({"stream": "false"})), None);
        assert_eq!(requested_stream_flag(&json!({"model": "flash"})), None);
        assert_eq!(requested_stream_flag(&json!([1, 2])), None);
    }

    #[test]
    fn a_streamed_body_is_forced_true_and_other_fields_survive() {
        let forced = force_streaming(json!({"model": "flash", "stream": true, "temperature": 0.2}))
            .expect("a JSON object is accepted");
        assert_eq!(forced["stream"], json!(true));
        assert_eq!(
            forced["temperature"],
            json!(0.2),
            "the body is otherwise untouched"
        );

        // A non-object body is refused before any money moves.
        assert!(force_streaming(json!([1, 2])).is_err());
    }

    // ---------------------------------------------------------------------
    // The terminal SSE error frame (docs/error-model.md:151-164)
    //
    // SSE is a line protocol: `field: value` lines separated by a single
    // \n, the whole event terminated by a blank line. These tests assert
    // against that grammar, not against whatever the function currently
    // returns — a frame that merely looks plausible is what silently drops
    // the event in an EventSource and hangs the client.
    // ---------------------------------------------------------------------

    /// The data field's value, with the `data: ` prefix stripped.
    fn data_field(frame: &str) -> &str {
        frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or_else(|| panic!("no `data: ` field in {frame:?}"))
    }

    fn request_id_of(frame: &str) -> String {
        let parsed: Value = serde_json::from_str(data_field(frame))
            .unwrap_or_else(|e| panic!("the data field must be JSON: {e}"));
        parsed["error"]["request_id"]
            .as_str()
            .expect("docs/error-model.md:171 - always include request_id")
            .to_string()
    }

    // ---------------------------------------------------------------------
    // The pre-flight input estimate: a hold must cover the TRUE token cost
    // ---------------------------------------------------------------------

    /// The hold is taken BEFORE the upstream is called and is the only thing
    /// standing between a request and the balance. The handler's own comment
    /// (proxy.rs:1094-1098) and money.rs:400 state the invariant as "never
    /// under-reserve": the reservation is a CEILING over every settlement of the
    /// same request.
    ///
    /// The input side is a byte-length heuristic, and bytes are not tokens. A
    /// UTF-8-dense body (CJK: 3 bytes per character, and ~1 token per character
    /// for a typical BPE tokenizer) carries roughly 3x the tokens a `len/4`
    /// estimate assumes, so the hold comes out smaller than the true cost and the
    /// difference is never collected: settlement clamps the debit to what was
    /// reserved (db.rs:266-271) and logs a Partial. Uncollected revenue.
    #[test]
    fn the_input_estimate_covers_a_utf8_dense_body() {
        let config = live_config();
        let model = config
            .models
            .iter()
            .find(|m| m.name == "flash")
            .expect("the shipped config must carry the flash model");

        // A body that is overwhelmingly multi-byte: ~3000 CJK characters (3
        // bytes each) plus a small JSON envelope. A BPE tokenizer emits about
        // ONE token per CJK character, so this body really is ~3000 input
        // tokens - it just does not look like it to a byte counter.
        let prompt = "请用中文详细解释这个系统的架构。".repeat(200);
        let dense = format!(
            r#"{{"model":"flash","stream":true,"messages":[{{"role":"user","content":"{prompt}"}}]}}"#
        )
        .into_bytes();
        // The same BYTE LENGTH of ASCII. A byte-length heuristic cannot tell
        // the two apart, which is the whole defect.
        let sparse = vec![b'a'; dense.len()];
        let hold = |body: &[u8]| model.worst_case_reservation_idr(estimated_input_tokens(body), 0);

        // POSITIVE CONTROL: the fixture can only expose the defect if the dense
        // body really is byte-dense (more than 4 bytes per token).
        let dense_chars = dense.iter().filter(|b| **b & 0xC0 != 0x80).count() as u64;
        assert!(
            dense_chars * 4 > dense.len() as u64,
            "the fixture must be byte-dense or it asserts nothing: {dense_chars} chars in {} bytes",
            dense.len()
        );

        // (1) Model-free: two bodies of the SAME length, one of them dense, must
        // not be held against the same money.
        assert!(
            hold(&dense) > hold(&sparse),
            "a byte-length estimate prices a UTF-8-dense body exactly like an ASCII body of the \
             same length (dense {} IDR vs sparse {} IDR), but the dense one costs more",
            hold(&dense),
            hold(&sparse)
        );

        // (2) The invariant itself (proxy.rs:1094-1098, money.rs:400): the hold
        // is a CEILING over the settlement of this request, so it must cover the
        // tokens the body really is - one token per CJK character plus ~4 ASCII
        // bytes per token for the envelope.
        let non_ascii_chars = dense
            .iter()
            .filter(|b| !b.is_ascii() && **b & 0xC0 != 0x80)
            .count() as u64;
        let ascii_bytes = dense.iter().filter(|b| b.is_ascii()).count() as u64;
        let true_tokens = non_ascii_chars + (ascii_bytes / 4).max(1);
        let true_cost = calculate_token_cost_idr(
            model.price,
            true_tokens,
            model.rates.input_peak,
            0,
            model.rates.cache_read_peak,
            0,
            model.rates.output_peak,
        );

        assert!(
            hold(&dense) >= true_cost,
            "the hold must cover the true cost of the body: {true_tokens} tokens in {} bytes, \
             hold {} IDR, but the input alone bills {true_cost} IDR",
            dense.len(),
            hold(&dense)
        );
    }

    #[test]
    fn the_error_frame_ends_with_a_blank_line() {
        let frame = error_event("upstream_failed", "boom");
        // The blank line is what tells a client the event is complete. Without
        // it the client keeps buffering and the error is never dispatched.
        assert!(
            frame.ends_with("\n\n"),
            "an SSE event is terminated by a blank line, got {frame:?}"
        );
        assert_eq!(
            &frame[frame.len() - 2..],
            "\n\n",
            "the LAST TWO CHARACTERS must be the terminator, got {:?}",
            &frame[frame.len() - 2..]
        );
    }

    #[test]
    fn the_error_frame_is_exactly_one_event_line_then_one_data_line() {
        let frame = error_event("upstream_failed", "boom");

        // Two field lines plus the blank terminator: split on \n leaves the
        // empty tail after the terminator.
        let lines: Vec<&str> = frame.split('\n').collect();
        assert_eq!(lines.len(), 4, "frame shape is wrong: {lines:?}");
        assert_eq!(
            lines[0], "event: error",
            "the event name must be exactly `error`"
        );
        assert!(
            lines[1].starts_with("data: "),
            "line 2 must be data, got {:?}",
            lines[1]
        );
        assert_eq!(lines[2], "", "the blank line terminates the event");
        assert_eq!(lines[3], "", "nothing may follow the terminator");

        // Exactly one of each: a stray field line could mis-frame the event.
        assert_eq!(lines.iter().filter(|l| l.starts_with("event:")).count(), 1);
        assert_eq!(lines.iter().filter(|l| l.starts_with("data:")).count(), 1);

        // The name is separated from the data by exactly one \n — a blank line
        // between them would end the event before its payload.
        assert_eq!(
            frame.find("\ndata: "),
            Some("event: error".len()),
            "exactly one newline must separate the event name from the data"
        );

        // The spec also allows CR as a line ending; emitting one would be a
        // second, hidden separator.
        assert!(!frame.contains('\r'), "no CR in the frame: {frame:?}");
    }

    #[test]
    fn the_error_frame_data_parses_back_to_what_was_passed_in() {
        let frame = error_event("upstream_failed", "The upstream stream failed.");
        let parsed: Value = serde_json::from_str(data_field(&frame))
            .unwrap_or_else(|e| panic!("the data field must be machine-parseable JSON: {e}"));

        assert_eq!(parsed["error"]["code"], json!("upstream_failed"));
        assert_eq!(
            parsed["error"]["message"],
            json!("The upstream stream failed.")
        );
        assert!(
            parsed["error"]["request_id"].is_string(),
            "the frame must carry the documented request_id"
        );
    }

    #[test]
    fn a_newline_in_the_code_or_message_cannot_break_out_of_the_data_field() {
        let frame = error_event("line1\nline2", "line1\nline2");

        // JSON-serialised, so the newline is the two characters \ and n. A
        // literal one would split the frame and let content inject a field or a
        // whole event.
        assert!(
            !frame.contains("line1\nline2"),
            "a literal newline escaped the JSON string: {frame:?}"
        );
        assert!(
            frame.contains(r"line1\nline2"),
            "the newline must be JSON-escaped: {frame:?}"
        );

        // Only the three framing newlines survive: after `event:`, after
        // `data:`, and the blank terminator.
        assert_eq!(
            frame.matches('\n').count(),
            3,
            "unexpected newline in {frame:?}"
        );
        assert_eq!(
            frame.split('\n').filter(|l| l.starts_with("data:")).count(),
            1,
            "the payload must stay on one line"
        );

        // Escaping must not have lost the value.
        let parsed: Value = serde_json::from_str(data_field(&frame)).unwrap();
        assert_eq!(parsed["error"]["code"], json!("line1\nline2"));
        assert_eq!(parsed["error"]["message"], json!("line1\nline2"));
    }

    #[test]
    fn every_error_event_carries_a_fresh_documented_request_id() {
        // Per-event, not per-process: the same inputs must not produce the same
        // id, or it correlates with nothing (docs/error-model.md:27).
        let first = request_id_of(&error_event("upstream_failed", "boom"));
        let second = request_id_of(&error_event("upstream_failed", "boom"));
        assert_ne!(first, second, "request_id must be generated per event");

        for id in [&first, &second] {
            assert!(
                id.starts_with("req_"),
                "documented form is req_..., got {id}"
            );
            let hex = &id["req_".len()..];
            assert_eq!(hex.len(), 32, "the suffix is a bare uuid, got {id}");
            assert!(
                hex.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "the suffix is lowercase hex, got {id}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // LIVE POSTGRES: the money path of chat_completions.
    //
    // Every test above this line is pure. chat_completions is the ONLY handler
    // that spends money, and until now not one of its database-visible steps had
    // ever been executed: the real api_keys lookup in load_key_metadata, the
    // HOLD taken by reserve_balance_transaction, and the settlement
    // (debit_usage_transaction / release_reservation_transaction) that follows
    // the upstream call.
    //
    // An upstream provider is unreachable from a test, and faking one would only
    // prove the fake works. So the database half is driven the way the handler
    // drives it, and nothing is faked:
    //
    //   * the FAILURE path goes through the real handler end to end. With no
    //     provider key in the environment the pool is empty, stream_chat refuses
    //     before any socket is opened, and the handler's release arm runs - which
    //     is the arm that decides whether a refused request strands money.
    //   * the SETTLEMENT path calls settle_after_stream itself, the very function
    //     the handler spawns, with the same ReservationGuard, the same ref and the
    //     same amounts the handler computes.
    //
    //   DATABASE_URL=... cargo test --lib -- --ignored --test-threads=1 routes::proxy
    // ---------------------------------------------------------------------

    use crate::config::AppConfig;
    use crate::db::unpaired_hold_rows;
    use crate::ip_tracking::{parse_cidrs, DailySalt, IpCidr};
    use crate::routes::events::RealtimeHub;
    use crate::test_support::{self, TestDb};

    fn live_config() -> Arc<AppConfig> {
        for path in ["../config/apikita.toml", "config/apikita.toml"] {
            if std::path::Path::new(path).exists() {
                return Arc::new(AppConfig::load_from_file(path).expect("parse apikita.toml"));
            }
        }
        panic!("could not find apikita.toml for testing");
    }

    /// The real application state, built the way main.rs builds it, so the
    /// handler runs against the same config and the same process-wide key cache
    /// the serving process uses.
    fn test_state(pool: SqlitePool) -> AppState {
        test_state_with(pool, live_config())
    }

    /// The real application state over an EXPLICIT config.
    ///
    /// Needed because the dearest-endpoint rule is only observable when the
    /// endpoints of one model carry genuinely different rates: the shipped
    /// config has them all tied, so a test of that rule must build the
    /// heterogeneous pool itself rather than edit the shared config file.
    fn test_state_with(pool: SqlitePool, config: Arc<AppConfig>) -> AppState {
        let events = Arc::new(RealtimeHub::new(&config.realtime));
        let trusted_proxies: Arc<[IpCidr]> = Arc::from(
            parse_cidrs(&config.network.trusted_proxy_cidrs)
                .expect("config CIDRs parse")
                .into_boxed_slice(),
        );
        AppState {
            pool,
            config,
            http_client: reqwest::Client::new(),
            events,
            ip_salt: Arc::new(DailySalt::new()),
            trusted_proxies,
        }
    }

    /// An account row. The SQLite schema has no DEFAULT for id, created_at or
    /// updated_at (plan section 4.1, correction 1), so the Postgres
    /// RETURNING id shape would fail at runtime rather than here.
    async fn create_account(pool: &SqlitePool) -> Uuid {
        test_support::account(pool).await
    }

    /// Money enters a wallet ONLY through credit_topup_transaction, which writes
    /// the matching +ledger row in the same transaction. Writing
    /// wallets.balance_idr directly manufactures exactly the drift the
    /// reconciliation assertion at the end of every test looks for.
    ///
    /// test_support::fund does exactly this - the wallet row, the topups row and
    /// the real credit - so the fixture cannot drift from the money-in path.
    async fn open_wallet(pool: &SqlitePool, account_id: Uuid, opening_idr: i64) {
        test_support::wallet(pool, account_id).await;
        test_support::fund(pool, account_id, opening_idr).await;
    }

    /// An api_keys row whose key_hash is the SAME hash chat_completions derives
    /// from the bearer token, so the lookup under test is the production one and
    /// not a bypass around it.
    async fn create_api_key(
        pool: &SqlitePool,
        account_id: Uuid,
        plaintext: &str,
        models: &[&str],
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO api_keys (id, account_id, key_hash, prefix, label, models, created_at)
             VALUES (?, ?, ?, 'apk_test', 'proxy-live', ?, ?)",
        )
        .bind(id.hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_string(plaintext))
        .bind(serde_json::to_value(models).expect("models as JSON"))
        .bind(chrono::Utc::now())
        .execute(pool)
        .await
        .expect("create api key");
        id
    }

    /// INVARIANT (a), scoped to THIS fixture's account: wallets.balance_idr must
    /// equal SUM(ledger.delta_idr). It must return 0 rows.
    async fn drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT w.account_id
                FROM wallets w
                LEFT JOIN ledger l ON l.account_id = w.account_id
                WHERE w.account_id = ?
                GROUP BY w.account_id, w.balance_idr
                HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// Every ledger move of the fixture, in order: (delta_idr, ref).
    async fn ledger_deltas(pool: &SqlitePool, account_id: Uuid) -> Vec<(i64, Option<String>)> {
        sqlx::query_as("SELECT delta_idr, ref FROM ledger WHERE account_id = ? ORDER BY id")
            .bind(account_id.hyphenated())
            .fetch_all(pool)
            .await
            .expect("read ledger")
    }

    async fn wallet_balance(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("read balance")
    }

    /// Today's usage row for the account: the three token classes SEPARATELY
    /// plus the cost. None means nothing was ever billed.
    async fn usage_today(pool: &SqlitePool, account_id: Uuid) -> Option<(i64, i64, i64, i64)> {
        sqlx::query_as(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr
             FROM usage_daily WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_optional(pool)
        .await
        .expect("read usage_daily")
    }

    /// The worst-case hold chat_completions MUST take for this body, recomputed
    /// independently from the config: the DEAREST endpoint's worst case, over
    /// max(requested, the model's own ceiling) clamped to the hard limit. The
    /// input side goes through the SAME `estimated_input_tokens` the handler
    /// calls - the estimate is not re-derived here, because a second copy of a
    /// rule is a second rule.
    fn expected_hold_idr(
        config: &AppConfig,
        model: &str,
        body: &[u8],
        max_tokens: Option<u64>,
    ) -> i64 {
        // Re-derived by per_endpoint_holds_idr, which prices each endpoint from
        // the RAW config rather than by calling the production rule: an oracle
        // that calls the code under test moves with it, and a mutation of that
        // code would then be invisible (the whole reason this helper exists).
        per_endpoint_holds_idr(config, model, body, max_tokens)
            .into_iter()
            .max()
            .unwrap_or(0)
    }

    /// The hold the handler MUST take for EACH endpoint of the pool, recomputed
    /// independently from the config: one entry per endpoint, each priced at its
    /// own peak-rate override (else the model rate). The DEAREST entry is the hold.
    ///
    /// Returned as a vector, not just the maximum, so a test can assert the pool
    /// is genuinely heterogeneous. On a flat pool every entry is equal, max and
    /// min coincide, and any assertion about "the dearest endpoint" is vacuous -
    /// which is exactly how the max->min mutation survived before per-endpoint
    /// rates existed.
    fn per_endpoint_holds_idr(
        config: &AppConfig,
        model: &str,
        body: &[u8],
        max_tokens: Option<u64>,
    ) -> Vec<i64> {
        let model_cfg = config
            .models
            .iter()
            .find(|m| m.name == model)
            .expect("the model must be in the config");
        let estimated_input = estimated_input_tokens(body);
        let max_output = max_tokens
            .unwrap_or(0)
            .max(model_cfg.max_output_tokens)
            .min(config.streaming.hard_max_output_tokens);

        let at = |input_peak: f64, output_peak: f64| {
            calculate_preflight_reservation_idr(
                model_cfg.price,
                estimated_input,
                input_peak,
                max_output,
                output_peak,
            )
        };

        let holds: Vec<i64> = model_cfg
            .endpoints
            .iter()
            .map(|endpoint| {
                at(
                    endpoint.input_peak.unwrap_or(model_cfg.rates.input_peak),
                    endpoint.output_peak.unwrap_or(model_cfg.rates.output_peak),
                )
            })
            .collect();

        if holds.is_empty() {
            vec![at(model_cfg.rates.input_peak, model_cfg.rates.output_peak)]
        } else {
            holds
        }
    }

    /// POST /v1/chat/completions reached through the REAL handler, with the peer
    /// address supplied the way the router supplies it. The response is returned
    /// as-is: a streaming success and an AppError are both real outcomes here.
    async fn call_chat_completions(
        state: &AppState,
        bearer: &str,
        body: &str,
    ) -> Result<Response<Body>, AppError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {bearer}").parse().unwrap(),
        );
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());

        chat_completions(
            State(state.clone()),
            axum::extract::connect_info::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))),
            headers,
            Bytes::from(body.to_string()),
        )
        .await
    }

    /// Runs the money assertions, then closes the fixture's database whether they
    /// passed or panicked.
    ///
    /// The Postgres original deleted its rows by name here, so a failing run could
    /// not leave residue in a database other runs shared. SQLite makes that
    /// unnecessary: TestDb owns a private temp database and close() removes it.
    /// close() is awaited rather than trusted to drop order, for the reason
    /// documented in test_support - dropping the pool only signals the close, so
    /// the removal would race it.
    async fn with_fixture<F>(db: TestDb, assertions: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let outcome = tokio::spawn(assertions).await;
        db.close().await;
        outcome.expect("the money-path assertions panicked");
    }

    /// THE FAILURE PATH, THROUGH THE REAL HANDLER. A request that never reaches a
    /// provider must cost nothing and must leave no stranded hold.
    ///
    /// This is the property that decides whether a refused customer loses money:
    /// the hold is out of the wallet for the whole upstream call, so an exit that
    /// forgets to give it back is money debited against a request that was never
    /// billed. INVARIANT (b): unpaired_hold_rows must be 0.
    #[tokio::test]
    async fn a_failed_upstream_releases_the_whole_hold_and_never_strands_it() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let opening_idr = 50_000;
        open_wallet(&pool, account_id, opening_idr).await;
        let key = format!("apk_live_{}", Uuid::new_v4().simple());
        let _key_id = create_api_key(&pool, account_id, &key, &["flash"]).await;

        let body = r#"{"model":"flash","stream":true}"#;
        let expected_hold = expected_hold_idr(&state.config, "flash", body.as_bytes(), None);
        assert!(
            expected_hold > 0,
            "the worst case must be a real hold, otherwise the test asserts nothing: {expected_hold}"
        );

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            let err = call_chat_completions(&state, &key, body)
                .await
                .expect_err("with no provider key in the environment the upstream is unreachable");
            assert_eq!(
                err.status_code(),
                StatusCode::SERVICE_UNAVAILABLE,
                "an unreachable upstream is a 503, got {err:?}"
            );
            assert_eq!(err.code(), "no_upstream_available");

            let deltas = ledger_deltas(&pool_for_assertions, account_id).await;

            // The HOLD was really taken, at the dearest endpoint's worst case -
            // not skipped, and not sized by whatever endpoint happened to answer.
            let holds: Vec<i64> = deltas
                .iter()
                .filter(|(delta, reference)| {
                    *delta < 0
                        && reference
                            .as_deref()
                            .is_some_and(|r| r.starts_with("reserve_"))
                })
                .map(|(delta, _)| *delta)
                .collect();
            assert_eq!(
                holds,
                vec![-expected_hold],
                "the handler must place exactly one hold, for the dearest endpoint's worst case"
            );

            // ... and it came back IN FULL, under the SAME ref, so the sweep can
            // pair the two rows.
            let reservation_ref = deltas
                .iter()
                .find(|(delta, _)| *delta == -expected_hold)
                .and_then(|(_, reference)| reference.clone())
                .expect("the hold carries its reservation ref");
            let released: i64 = deltas
                .iter()
                .filter(|(delta, reference)| {
                    *delta > 0 && reference.as_deref() == Some(reservation_ref.as_str())
                })
                .map(|(delta, _)| *delta)
                .sum();
            assert_eq!(
                released, expected_hold,
                "a request that never reached a provider must give the whole hold back"
            );

            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                opening_idr,
                "a refused request must cost the customer nothing"
            );
            assert_eq!(
                unpaired_hold_rows(&pool_for_assertions, account_id)
                    .await
                    .expect("stranded-hold sweep"),
                0,
                "INVARIANT (b): a failed request must leave ZERO stranded holds"
            );
            assert_eq!(
                usage_today(&pool_for_assertions, account_id).await,
                None,
                "nothing was generated, so nothing may be billed"
            );
            assert_eq!(
                drift_rows(&pool_for_assertions, account_id).await,
                0,
                "INVARIANT (a): balance_idr must equal SUM(ledger.delta_idr)"
            );
        })
        .await;
    }

    /// THE SETTLEMENT PATH, THROUGH THE FUNCTION THE HANDLER SPAWNS.
    ///
    /// The hold is taken the way the handler takes it, then settle_after_stream is
    /// handed the very outcome a completed stream would hand it. INVARIANTS (a)
    /// and (c): the wallet ends exactly -cost_idr from the opening balance, the
    /// ledger's whole move for the request is exactly -cost_idr, and the three
    /// token classes are recorded SEPARATELY - never summed.
    #[tokio::test]
    async fn settlement_debits_the_real_usage_and_releases_the_rest_of_the_hold() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let opening_idr = 50_000;
        open_wallet(&pool, account_id, opening_idr).await;
        let key_id = create_api_key(
            &pool,
            account_id,
            &format!("apk_live_{}", Uuid::new_v4().simple()),
            &["flash"],
        )
        .await;

        let body = r#"{"model":"flash","stream":true}"#;
        let held_idr = expected_hold_idr(&state.config, "flash", body.as_bytes(), None);
        let model_cfg = state
            .config
            .models
            .iter()
            .find(|m| m.name == "flash")
            .expect("flash is configured");

        let usage = Usage {
            input_tokens: 1_000,
            cache_read_tokens: 400,
            output_tokens: 2_000,
        };
        let cost_idr = calculate_token_cost_idr(
            model_cfg.price,
            usage.input_tokens as u64,
            model_cfg.rates.input_peak,
            usage.cache_read_tokens as u64,
            model_cfg.rates.cache_read_peak,
            usage.output_tokens as u64,
            model_cfg.rates.output_peak,
        );
        assert!(
            held_idr > cost_idr,
            "the worst-case hold must cover the real cost, or the fixture proves nothing ({held_idr} vs {cost_idr})"
        );

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            // Step 6 of the handler: the HOLD, before the upstream call.
            let reservation_ref = format!("reserve_{}", Uuid::new_v4().simple());
            let held = reserve_balance_transaction(
                &pool_for_assertions,
                account_id,
                held_idr,
                Some(&reservation_ref),
            )
            .await
            .expect("place the hold");
            assert!(
                matches!(&held, ReservationResult::Held { reserved_idr, .. } if *reserved_idr == held_idr),
                "the wallet must cover the worst case, got {held:?}"
            );
            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                opening_idr - held_idr,
                "the hold must be OUT of the wallet for the whole upstream call"
            );

            // Step 8: the settlement task, with the guard, the ref and the
            // outcome a completed stream reports.
            let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<StreamEnd>();
            let guard = ReservationGuard::new(
                &pool_for_assertions,
                account_id,
                held_idr,
                &reservation_ref,
                "flash",
            );
            assert!(
                settle_tx.send(StreamEnd::Settled(usage)).is_ok(),
                "the settlement task must still be listening"
            );

            settle_after_stream(
                settle_rx,
                pool_for_assertions.clone(),
                state.config.clone(),
                state.events.clone(),
                account_id,
                key_id,
                "flash".to_string(),
                held_idr,
                guard,
            )
            .await;

            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                opening_idr - cost_idr,
                "the request must cost exactly the reported usage, not the hold"
            );

            let (input, cache_read, output, cost) = usage_today(&pool_for_assertions, account_id)
                .await
                .expect("a billed request writes its usage row");
            assert_eq!(
                (input, cache_read, output),
                (
                    usage.input_tokens,
                    usage.cache_read_tokens,
                    usage.output_tokens
                ),
                "the three token classes are ALWAYS separate - never summed"
            );
            assert_eq!(cost, cost_idr, "usage_daily carries what the request cost");

            // Scoped to THIS request's reservation ref: the opening top-up is in
            // the same ledger, so the account-wide sum would be dominated by it.
            // Every row the request wrote carries the one ref it reserved under.
            let request_rows: Vec<i64> = ledger_deltas(&pool_for_assertions, account_id)
                .await
                .into_iter()
                .filter(|(_, reference)| reference.as_deref() == Some(reservation_ref.as_str()))
                .map(|(delta, _)| delta)
                .collect();
            assert_eq!(
                request_rows,
                vec![-held_idr, held_idr, -cost_idr],
                "the request's ledger move is -hold, +hold, -cost, in that order"
            );
            assert_eq!(
                request_rows.iter().sum::<i64>(),
                -cost_idr,
                "the whole request's ledger move is exactly -cost_idr: -hold +hold -cost"
            );
            assert_eq!(
                unpaired_hold_rows(&pool_for_assertions, account_id)
                    .await
                    .expect("stranded-hold sweep"),
                0,
                "INVARIANT (b): a settled hold must be paired, not stranded"
            );
            assert_eq!(
                drift_rows(&pool_for_assertions, account_id).await,
                0,
                "INVARIANT (a): balance_idr must equal SUM(ledger.delta_idr)"
            );
        })
        .await;
    }

    /// THE WASHED CASE. A stream that ended without a usage report is billed
    /// NOTHING and gives the whole hold back (docs/failover.md:138-144). Token
    /// counts are never invented to fill the gap.
    #[tokio::test]
    async fn a_stream_without_usage_is_washed_and_the_whole_hold_returns() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let opening_idr = 50_000;
        open_wallet(&pool, account_id, opening_idr).await;
        let key_id = create_api_key(
            &pool,
            account_id,
            &format!("apk_live_{}", Uuid::new_v4().simple()),
            &["flash"],
        )
        .await;

        let held_idr = expected_hold_idr(
            &state.config,
            "flash",
            r#"{"model":"flash","stream":true}"#.as_bytes(),
            None,
        );

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            let reservation_ref = format!("reserve_{}", Uuid::new_v4().simple());
            assert!(matches!(
                reserve_balance_transaction(
                    &pool_for_assertions,
                    account_id,
                    held_idr,
                    Some(&reservation_ref)
                )
                .await
                .expect("place the hold"),
                ReservationResult::Held { .. }
            ));

            let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<StreamEnd>();
            let guard = ReservationGuard::new(
                &pool_for_assertions,
                account_id,
                held_idr,
                &reservation_ref,
                "flash",
            );
            assert!(
                settle_tx.send(StreamEnd::NoUsage).is_ok(),
                "the settlement task must still be listening"
            );

            settle_after_stream(
                settle_rx,
                pool_for_assertions.clone(),
                state.config.clone(),
                state.events.clone(),
                account_id,
                key_id,
                "flash".to_string(),
                held_idr,
                guard,
            )
            .await;

            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                opening_idr,
                "an unreported stream must not be billed against invented token counts"
            );
            assert_eq!(
                usage_today(&pool_for_assertions, account_id).await,
                None,
                "no usage was reported, so no usage row may exist"
            );
            // Scoped to this request's ref: the opening top-up lives in the same
            // ledger and must not be counted as part of the request's move.
            let request_rows: Vec<i64> = ledger_deltas(&pool_for_assertions, account_id)
                .await
                .into_iter()
                .filter(|(_, reference)| reference.as_deref() == Some(reservation_ref.as_str()))
                .map(|(delta, _)| delta)
                .collect();
            assert_eq!(
                request_rows,
                vec![-held_idr, held_idr],
                "the hold went out and came back under the same ref"
            );
            assert_eq!(
                request_rows.iter().sum::<i64>(),
                0,
                "the hold went out and came back: the ledger nets to zero"
            );
            assert_eq!(
                unpaired_hold_rows(&pool_for_assertions, account_id)
                    .await
                    .expect("stranded-hold sweep"),
                0,
                "INVARIANT (b): a washed request must leave ZERO stranded holds"
            );
            assert_eq!(
                drift_rows(&pool_for_assertions, account_id).await,
                0,
                "INVARIANT (a): balance_idr must equal SUM(ledger.delta_idr)"
            );
        })
        .await;
    }

    /// A PARTIAL SETTLEMENT STILL BILLS (clamp_debit / UsageSettlement::Partial).
    ///
    /// The answer was already streamed and the usage WAS reported, so it is on the
    /// books even when the wallet cannot cover it: the debit is clamped to the
    /// balance, usage_daily still carries the FULL cost, and reconciliation still
    /// holds because the ledger records only what was actually taken. The balance
    /// never goes negative (docs/decisions.md D3).
    #[tokio::test]
    async fn a_clamped_debit_still_records_the_full_usage_and_stays_reconciled() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let opening_idr = 1_000;
        open_wallet(&pool, account_id, opening_idr).await;
        let key_id = create_api_key(
            &pool,
            account_id,
            &format!("apk_live_{}", Uuid::new_v4().simple()),
            &["flash"],
        )
        .await;

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            // The whole balance is held, so the settlement has nothing left to
            // collect from once the hold is released.
            let reservation_ref = format!("reserve_{}", Uuid::new_v4().simple());
            assert!(matches!(
                reserve_balance_transaction(
                    &pool_for_assertions,
                    account_id,
                    opening_idr,
                    Some(&reservation_ref)
                )
                .await
                .expect("place the hold"),
                ReservationResult::Held { .. }
            ));

            let cost_idr = 2_500;
            let outcome = debit_usage_transaction(
                &pool_for_assertions,
                account_id,
                Some(key_id),
                500,
                100,
                1_000,
                cost_idr,
                Some(&reservation_ref),
                opening_idr,
            )
            .await
            .expect("the settlement must record the usage, not discard it");

            assert_eq!(
                outcome,
                UsageSettlement::Partial {
                    new_balance: 0,
                    debited_idr: opening_idr,
                    shortfall_idr: cost_idr - opening_idr,
                },
                "a wallet that cannot cover the cost is a recorded shortfall, not a dropped charge"
            );
            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                0,
                "the clamped debit lands on 0 and never below it"
            );

            let (input, cache_read, output, cost) = usage_today(&pool_for_assertions, account_id)
                .await
                .expect("the tokens were consumed, so the usage row exists");
            assert_eq!(
                (input, cache_read, output),
                (500, 100, 1_000),
                "the real counters are recorded, and the three classes stay separate"
            );
            assert_eq!(
                cost, cost_idr,
                "usage_daily carries the FULL cost: the 30-day spend window must not be understated"
            );

            // Scoped to this request's ref: the ledger records only what was
            // ACTUALLY taken, so it still matches the wallet.
            let request_rows: Vec<i64> = ledger_deltas(&pool_for_assertions, account_id)
                .await
                .into_iter()
                .filter(|(_, reference)| reference.as_deref() == Some(reservation_ref.as_str()))
                .map(|(delta, _)| delta)
                .collect();
            assert_eq!(
                request_rows,
                vec![-opening_idr, opening_idr, -opening_idr],
                "the ledger records the hold, its release, and only the clamped debit"
            );
            assert_eq!(
                request_rows.iter().sum::<i64>(),
                -opening_idr,
                "the ledger records only what was ACTUALLY taken, so it still matches the wallet"
            );
            assert_eq!(
                unpaired_hold_rows(&pool_for_assertions, account_id)
                    .await
                    .expect("stranded-hold sweep"),
                0,
                "INVARIANT (b): a clamped settlement still pairs its hold"
            );
            assert_eq!(
                drift_rows(&pool_for_assertions, account_id).await,
                0,
                "INVARIANT (a): a clamped debit must not break balance_idr = SUM(ledger.delta_idr)"
            );
        })
        .await;
    }

    // ---------------------------------------------------------------------
    // THE DEAREST-ENDPOINT RULE, made observable.
    //
    // docs/failover.md:162-165: the hold is taken BEFORE routing and applies
    // whichever endpoint serves the request, so it must cover the DEAREST
    // endpoint in the pool. Until per-endpoint rates existed, every endpoint of
    // a model tied (rates lived only on the model), so max() and min() over the
    // pool were the SAME value: swapping .max() for .min() was program
    // equivalence, and a mutation doing exactly that survived the whole suite.
    //
    // This test builds a pool whose endpoints carry GENUINELY different rates
    // and drives the real handler, so the dearest figure is the only correct
    // hold and the mutation now fails.
    // ---------------------------------------------------------------------

    /// A clone of the live config whose `flash` pool holds endpoints with
    /// genuinely different peak rates, cheapest first. Returns the config and
    /// the per-endpoint holds the handler must choose between.
    fn heterogeneous_flash_config() -> Arc<AppConfig> {
        let mut config = (*live_config()).clone();
        let flash = config
            .models
            .iter_mut()
            .find(|m| m.name == "flash")
            .expect("flash is configured");
        assert!(
            flash.endpoints.len() >= 2,
            "the fixture needs at least two endpoints to have a dearest one"
        );

        let (input_peak, output_peak) = (flash.rates.input_peak, flash.rates.output_peak);
        for (index, endpoint) in flash.endpoints.iter_mut().enumerate() {
            // 1x, 3x, 9x ...: strictly increasing, so the last endpoint is
            // unambiguously the dearest and the first unambiguously the cheapest.
            let factor = 3f64.powi(index as i32);
            endpoint.input_peak = Some(input_peak * factor);
            endpoint.output_peak = Some(output_peak * factor);
        }

        Arc::new(config)
    }

    #[tokio::test]
    async fn the_hold_covers_the_dearest_endpoint_not_the_cheapest() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let config = heterogeneous_flash_config();
        let state = test_state_with(pool.clone(), config.clone());
        let opening_idr = 500_000;
        open_wallet(&pool, account_id, opening_idr).await;
        let key = format!("apk_live_{}", Uuid::new_v4().simple());
        let _key_id = create_api_key(&pool, account_id, &key, &["flash"]).await;

        let body = r#"{"model":"flash","stream":true}"#;
        let per_endpoint = per_endpoint_holds_idr(&config, "flash", body.as_bytes(), None);
        let cheapest = *per_endpoint.iter().min().expect("at least one endpoint");
        let dearest = *per_endpoint.iter().max().expect("at least one endpoint");

        // POSITIVE CONTROL on the fixture itself. If the pool is flat, max and
        // min coincide and every assertion below is vacuous - the exact
        // condition that let the mutation survive. This test is worthless
        // unless the pool genuinely differs.
        assert!(
            dearest > cheapest,
            "the fixture must hold endpoints with genuinely different rates, got {per_endpoint:?}"
        );

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            // The REAL handler. With no provider key in the environment the
            // upstream is unreachable, so the request takes the hold, fails to
            // route, and gives the whole hold back - which is exactly the
            // money step under test, with no upstream faked.
            let err = call_chat_completions(&state, &key, body)
                .await
                .expect_err("with no provider key in the environment the upstream is unreachable");
            assert_eq!(err.status_code(), StatusCode::SERVICE_UNAVAILABLE);

            let holds: Vec<i64> = ledger_deltas(&pool_for_assertions, account_id)
                .await
                .into_iter()
                .filter(|(delta, reference)| {
                    *delta < 0
                        && reference
                            .as_deref()
                            .is_some_and(|r| r.starts_with("reserve_"))
                })
                .map(|(delta, _)| -delta)
                .collect();

            assert_eq!(
                holds,
                vec![dearest],
                "the hold must be the DEAREST endpoint worst case ({dearest}), not the cheapest ({cheapest}); pool holds were {per_endpoint:?}"
            );
            assert_ne!(
                holds,
                vec![cheapest],
                "reserving at the CHEAPEST endpoint would under-reserve and overdraw on a failover"
            );

            // The whole hold came back, so the account is untouched...
            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                opening_idr,
                "a refused request must cost the customer nothing"
            );
            // ...and the ledger still reconciles (INVARIANT (a)).
            assert_eq!(drift_rows(&pool_for_assertions, account_id).await, 0);
            assert_eq!(
                unpaired_hold_rows(&pool_for_assertions, account_id)
                    .await
                    .expect("stranded-hold sweep"),
                0,
                "INVARIANT (b): no stranded holds"
            );
        })
        .await;
    }

    // ---------------------------------------------------------------------
    // THE DOCUMENTED ENFORCEMENT ORDER, pinned as a REGRESSION.
    //
    // docs/website/06-api-keys-and-limits.md:130-146 fixes the per-request
    // sequence - authenticate, then authorize the model, then throttle, then
    // check money - and states the consequence explicitly: "Checking the wallet
    // before the model allowlist leaks that a model exists to a key not
    // permitted to use it."
    //
    // The order is only observable through the error the caller gets back, so
    // these tests assert the documented `code` (docs/error-model.md:25 - "code
    // is the contract; message is not") and, for the leak itself, that the
    // answer is IDENTICAL at a generous balance and at zero.
    //
    // The requested model in the leak test is a REAL configured model
    // (`deepseek-v4-flash`, config/apikita.toml:397), never an unknown name:
    // an unknown model is refused by the config lookup that sits between the
    // allowlist and the money, which would mask the very reordering under test.
    // ---------------------------------------------------------------------

    /// A key carrying an explicit `spend_limit_idr`. The allowlist is passed the
    /// same way `create_api_key` takes it; only the limit differs.
    async fn create_api_key_with_spend_limit(
        pool: &SqlitePool,
        account_id: Uuid,
        plaintext: &str,
        models: &[&str],
        spend_limit_idr: i64,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO api_keys
                 (id, account_id, key_hash, prefix, label, models, spend_limit_idr, created_at)
             VALUES (?, ?, ?, 'apk_test', 'proxy-live', ?, ?, ?)",
        )
        .bind(id.hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_string(plaintext))
        .bind(serde_json::to_value(models).expect("models as JSON"))
        .bind(spend_limit_idr)
        .bind(chrono::Utc::now())
        .execute(pool)
        .await
        .expect("create api key");
        id
    }

    /// The wallet the login path creates and nothing else: a real row, zero
    /// balance, no ledger history. `open_wallet` is the ONLY helper allowed to
    /// put money in (through `credit_topup_transaction`); this one exists for the
    /// opposite case, an account that has never topped up.
    async fn open_empty_wallet(pool: &SqlitePool, account_id: Uuid) {
        test_support::wallet(pool, account_id).await;
    }

    /// Spend ALREADY recorded against a key, written with the same table, columns
    /// and day that `db::debit_usage_transaction` writes, so the proxy's own
    /// `keys::key_spend_used` read sees it. Seeding the row is what makes "this
    /// key is over its limit" a fixture instead of a whole billed request; the
    /// read under test is still the production one.
    async fn seed_key_spend(pool: &SqlitePool, account_id: Uuid, key_id: Uuid, cost_idr: i64) {
        sqlx::query(
            "INSERT INTO usage_daily (account_id, api_key_id, day, cost_idr)
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_id.hyphenated())
        .bind(key_id.hyphenated())
        .bind(chrono::Utc::now().date_naive())
        .bind(cost_idr)
        .execute(pool)
        .await
        .expect("seed the key's 30-day spend");
    }

    /// (a) A key whose allowlist EXCLUDES the requested model is refused with the
    /// MODEL error - and the refusal does not depend on the wallet at all.
    ///
    /// Same key shape, same request, two wallets: one generously funded, one at
    /// zero. A balance-dependent answer IS the documented leak: the caller whose
    /// wallet happens to be empty would learn about money the funded caller was
    /// never told. Both must get 403 `model_not_allowed`.
    #[tokio::test]
    async fn a_model_outside_the_allowlist_is_refused_identically_at_any_balance() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());

        // A REAL configured model the key is NOT allowed: the allowlist is the
        // only check that can refuse it, so the money steps are genuinely in play.
        let body = r#"{"model":"deepseek-v4-flash","stream":true}"#;

        let funded_account = create_account(&pool).await;
        let funded_opening = 50_000;
        open_wallet(&pool, funded_account, funded_opening).await;
        let funded_key = format!("apk_live_{}", Uuid::new_v4().simple());
        create_api_key(&pool, funded_account, &funded_key, &["flash"]).await;

        let broke_account = create_account(&pool).await;
        open_empty_wallet(&pool, broke_account).await;
        let broke_key = format!("apk_live_{}", Uuid::new_v4().simple());
        create_api_key(&pool, broke_account, &broke_key, &["flash"]).await;

        let pool_for_assertions = pool.clone();
        let assertions = tokio::spawn(async move {
            let funded = call_chat_completions(&state, &funded_key, body)
                .await
                .expect_err("a model outside the allowlist is always refused");
            let broke = call_chat_completions(&state, &broke_key, body)
                .await
                .expect_err("a model outside the allowlist is always refused");

            let answer = (funded.status_code(), funded.code());
            assert_eq!(
                answer,
                (StatusCode::FORBIDDEN, "model_not_allowed"),
                "the model error is the documented contract, got {funded:?}"
            );
            assert_eq!(
                (broke.status_code(), broke.code()),
                answer,
                "THE LEAK: a zero-balance wallet must not change the answer, got {broke:?} vs {funded:?}"
            );

            // The wallet was never consulted on either account: the funded one
            // still holds exactly its top-up, and its ledger never saw a hold.
            let deltas = ledger_deltas(&pool_for_assertions, funded_account).await;
            assert_eq!(
                deltas.iter().map(|(delta, _)| *delta).collect::<Vec<_>>(),
                vec![funded_opening],
                "a model-denied request must not move the ledger at all"
            );
            assert!(
                deltas.iter().all(|(_, reference)| !reference
                    .as_deref()
                    .is_some_and(|r| r.starts_with("reserve_"))),
                "no balance may be reserved before the allowlist refuses: {deltas:?}"
            );
            assert_eq!(
                wallet_balance(&pool_for_assertions, funded_account).await,
                funded_opening,
                "the balance must be untouched by a model denial"
            );
            assert_eq!(wallet_balance(&pool_for_assertions, broke_account).await, 0);

            for account in [funded_account, broke_account] {
                assert_eq!(
                    drift_rows(&pool_for_assertions, account).await,
                    0,
                    "INVARIANT (a): balance_idr must equal SUM(ledger.delta_idr)"
                );
                assert_eq!(
                    unpaired_hold_rows(&pool_for_assertions, account)
                        .await
                        .expect("stranded-hold sweep"),
                    0,
                    "INVARIANT (b): a refusal must leave ZERO stranded holds"
                );
            }
        });

        let outcome = assertions.await;
        // Both accounts live in this test's own temp database, so closing it is
        // the whole teardown: there is no shared table to delete rows out of.
        db.close().await;
        outcome.expect("the enforcement-order assertions panicked");
    }

    /// (b) A key that IS allowed the model but whose wallet cannot cover the worst
    /// case gets the MONEY error, not the model error.
    ///
    /// This is the other half of the same order: once the model is permitted, the
    /// balance is what refuses - and the refusal names the hold it could not cover.
    #[tokio::test]
    async fn an_allowed_model_with_an_uncovered_balance_is_refused_for_money() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        open_empty_wallet(&pool, account_id).await;
        let key = format!("apk_live_{}", Uuid::new_v4().simple());
        create_api_key(&pool, account_id, &key, &["flash"]).await;

        let body = r#"{"model":"flash","stream":true}"#;
        let expected_hold = expected_hold_idr(&state.config, "flash", body.as_bytes(), None);
        assert!(
            expected_hold > 0,
            "the worst case must be a real hold, otherwise this test asserts nothing"
        );

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            let err = call_chat_completions(&state, &key, body)
                .await
                .expect_err("an empty wallet cannot cover the worst case");

            assert_eq!(
                err.status_code(),
                StatusCode::PAYMENT_REQUIRED,
                "an unaffordable request is a 402, got {err:?}"
            );
            assert_eq!(err.code(), "insufficient_balance");
            assert_ne!(
                err.code(),
                "model_not_allowed",
                "the model IS allowed here - the money is what is missing"
            );
            assert_eq!(
                err.details(),
                Some(json!({ "balance_idr": 0, "required_idr": expected_hold })),
                "the 402 must name the hold the wallet could not cover"
            );

            // A reservation that matched no row writes nothing: no ledger row, no
            // drift, no stranded hold.
            assert_eq!(wallet_balance(&pool_for_assertions, account_id).await, 0);
            assert_eq!(
                ledger_deltas(&pool_for_assertions, account_id).await,
                Vec::<(i64, Option<String>)>::new(),
                "a refused reservation must not write a ledger row"
            );
            assert_eq!(drift_rows(&pool_for_assertions, account_id).await, 0);
            assert_eq!(
                unpaired_hold_rows(&pool_for_assertions, account_id)
                    .await
                    .expect("stranded-hold sweep"),
                0,
                "INVARIANT (b): a refused reservation leaves ZERO stranded holds"
            );
        })
        .await;
    }

    /// (c) A key over its own 30-day spend limit gets the LIMIT error - and it
    /// gets it while the wallet could easily pay, which is what pins the limit
    /// ahead of the money (docs/website/06-api-keys-and-limits.md:137-139).
    #[tokio::test]
    async fn a_key_over_its_spend_limit_is_refused_for_the_limit_not_for_money() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let opening_idr = 500_000;
        open_wallet(&pool, account_id, opening_idr).await;

        let key = format!("apk_live_{}", Uuid::new_v4().simple());
        let spend_limit_idr = 10_000;
        let key_id =
            create_api_key_with_spend_limit(&pool, account_id, &key, &["flash"], spend_limit_idr)
                .await;
        seed_key_spend(&pool, account_id, key_id, spend_limit_idr).await;

        let body = r#"{"model":"flash","stream":true}"#;
        assert!(
            expected_hold_idr(&state.config, "flash", body.as_bytes(), None) < opening_idr,
            "the wallet must be able to pay, or the limit is not what stopped the request"
        );

        let pool_for_assertions = pool.clone();
        with_fixture(db, async move {
            let err = call_chat_completions(&state, &key, body)
                .await
                .expect_err("the key is exactly at its ceiling, which already blocks");

            assert_eq!(
                err.status_code(),
                StatusCode::PAYMENT_REQUIRED,
                "a key limit is a 402, not a 429 (docs/error-model.md:73-86), got {err:?}"
            );
            assert_eq!(err.code(), "key_limit_exceeded");
            assert_eq!(
                err.details()
                    .and_then(|details| details["reason"].as_str().map(str::to_string)),
                Some("spend_limit_idr_reached".to_string()),
                "the documented limit is the one reported, got {:?}",
                err.details()
            );
            assert_eq!(
                wallet_balance(&pool_for_assertions, account_id).await,
                opening_idr,
                "the limit refuses before any money moves"
            );
            assert_eq!(drift_rows(&pool_for_assertions, account_id).await, 0);
        })
        .await;
    }
}
