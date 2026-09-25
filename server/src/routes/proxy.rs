use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, Response, StatusCode},
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::{
    debit_usage_transaction, release_reservation_transaction, reserve_balance_transaction,
    ReservationResult, UsageSettlement,
};
use crate::error::AppError;
use crate::money::{calculate_preflight_reservation_idr, calculate_token_cost_idr};
use crate::routes::events::{publish_balance, publish_usage, todays_usage, RealtimeHub};
// The 30-day window and the spend read are shared with the key-management routes
// on purpose: the limit the proxy enforces and the number the dashboard shows
// must come from one definition, not two that can drift.
use crate::routes::keys::{key_spend_used, key_tokens_used, SPEND_WINDOW_DAYS};
use crate::upstream::{parse_usage_from_sse, UpstreamClient, UpstreamError, UpstreamStream, Usage};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<AppConfig>,
    pub http_client: reqwest::Client,
    /// The realtime fan-out behind `GET /events`.
    pub events: Arc<RealtimeHub>,
}

impl axum::extract::FromRef<AppState> for PgPool {
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
    pool: PgPool,
    account_id: Uuid,
    reserved_idr: i64,
    reservation_ref: String,
    model: String,
    defused: bool,
}

impl ReservationGuard {
    fn new(
        pool: &PgPool,
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
    Deny { retry_after_secs: u64 },
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

    if windows.len() >= RATE_WINDOW_CAPACITY {
        windows.retain(|_, w| now.saturating_duration_since(w.started_at) < RATE_WINDOW);
        if windows.len() >= RATE_WINDOW_CAPACITY {
            // Still full of live windows. Dropping one hands that key a fresh
            // window — it is allowed through MORE often, never less, which is
            // the safe direction for a limiter to fail.
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
    pool: &PgPool,
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
        WHERE key_hash = $1
        "#,
    )
    .bind(key_hash)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Err(AppError::Unauthenticated);
    };

    let meta = KeyMetadata {
        key_id: row.get("id"),
        account_id: row.get("account_id"),
        models: row.get("models"),
        spend_limit_idr: row.get("spend_limit_idr"),
        token_limit: row.get("token_limit"),
        rate_limit_rpm: row.get("rate_limit_rpm"),
        expires_at: row.get("expires_at"),
        revoked_at: row.get("revoked_at"),
    };

    // Not cached if an invalidation landed while this row was being read.
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert_if_unchanged(
        key_hash.to_string(),
        meta.clone(),
        Instant::now(),
        seen_generation,
    );

    Ok(meta)
}

/// Drop one key from the metadata cache, addressed by its HASH.

/// The hash is the only identifier the cache ever holds — the plaintext key
/// exists nowhere but the caller's Authorization header — so this is the only
/// handle an invalidation could take, and it keeps the function safe to call
/// with a value that has already been logged or stored.

/// Call this whenever a key's enforcement inputs change: revocation, and a
/// narrowed limit or allowlist. `config` is needed only to reach the same
/// `OnceLock` instance the request path uses, so the process-wide cache stays
/// the single source the two paths share.

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

/// Maps a failed upstream call to the error the client may see.
///
/// A transport error's Display embeds the provider URL, so it is logged and
/// withheld: the client gets the generic upstream-unavailable error instead
/// (DEFECT 3, docs/error-model.md:159). The other variants carry no provider URL
/// or hostname.
fn upstream_error(err: UpstreamError, model: &str, account_id: Uuid) -> AppError {
    match err {
        UpstreamError::NoModel(_) => AppError::ModelNotAllowed(model.to_string()),
        UpstreamError::NoHealthyUpstream(_) => AppError::NoUpstreamAvailable,
        UpstreamError::Transport(detail) => {
            warn!(
                account_id = %account_id,
                model = %model,
                error = %detail,
                "upstream transport error; detail withheld from client"
            );
            AppError::NoUpstreamAvailable
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
        Some(false) => Err(AppError::ValidationFailed(
            "stream must be true: this endpoint serves text/event-stream only; set stream to true or omit the field".into(),
        )),
        _ => Ok(()),
    }
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
        let spend_used_idr =
            key_spend_used(&state.pool, account_id, key_id, chrono::Utc::now().date_naive())
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
        let tokens_used =
            key_tokens_used(&state.pool, account_id, key_id, chrono::Utc::now().date_naive())
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
    // Input is estimated from the raw body length (~4 bytes per token, floored
    // at 1); the output ceiling is the request's own cap, clamped to the hard
    // limit. The reservation must cover the dearest endpoint in the pool, not
    // the one that happens to serve the request — otherwise a failover to a
    // dearer provider can overdraw the balance (docs/failover.md:162-165).
    let estimated_input = (body.len() as u64 / 4).max(1);
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

    // One reservation per endpoint, and the dearest wins. The rates live on
    // the model today, so the endpoints tie and this is a no-op - but it is
    // the rule that must hold once per-endpoint rates exist. The max(1) keeps
    // a model with no endpoints registered reserving at the model rate,
    // exactly as UpstreamClient::worst_case_reservation_idr does.
    let reservation = (0..model_cfg.endpoints.len().max(1))
        .map(|_| {
            calculate_preflight_reservation_idr(
                model_cfg.price,
                estimated_input,
                model_cfg.rates.input_peak,
                max_output,
                model_cfg.rates.output_peak,
            )
        })
        .max()
        .unwrap_or(0);

    // 5. The inbound body with `stream: true` guaranteed. Parsed BEFORE the hold so
    //    a malformed body can never take money: validation precedes the debit.
    let upstream_body = force_streaming(parsed)?;

    // 6. HOLD the reservation. This is the money step: the wallet is debited by
    //    the worst case in a guarded, committed transaction BEFORE the upstream is
    //    called, and the release happens at settlement.
    //
    // The guard is `balance_idr >= $amount` inside the UPDATE, not a read: two
    // concurrent requests from the same account are serialized by the row lock, so
    // only as many as the balance can actually cover are admitted. The previous
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
    let held = reserve_balance_transaction(
        &state.pool,
        account_id,
        reservation,
        Some(&reservation_ref),
    )
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
    let mut guard = ReservationGuard::new(&state.pool, account_id, reserved_idr, &reservation_ref, &meta.model);

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
            release_quietly(&state.pool, account_id, reserved_idr, &reservation_ref, &meta.model)
                .await;
            return Err(upstream_error(err, &meta.model, account_id));
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
    pool: PgPool,
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
        release_quietly(&pool, account_id, reserved_idr, &guard.reservation_ref, &guard.model).await;
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
        release_quietly(&pool, account_id, reserved_idr, &guard.reservation_ref, &guard.model).await;
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
                // (Postgres returns NUMERIC for SUM(bigint) while the row is
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
                // (Postgres returns NUMERIC for SUM(bigint) while the row is
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
            release_quietly(&pool, account_id, reserved_idr, &guard.reservation_ref, &guard.model).await;
        }
    }
}
/// Gives a reservation back, logging a failure instead of discarding it.
///
/// A release that does not land leaves the customer's money debited against a
/// request that was never billed — the mirror image of the defect this fix closes
/// — so a failure is loud even though the client is long gone.
async fn release_quietly(
    pool: &PgPool,
    account_id: Uuid,
    reserved_idr: i64,
    reservation_ref: &str,
    model: &str,
) {
    match release_reservation_transaction(pool, account_id, reserved_idr, Some(reservation_ref)).await
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
        assert_eq!(settlement_plan(Some(partial)), SettlementPlan::Bill(partial));

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
        assert!(!limit_reached(0, 10_000_000), "0 never blocks, whatever was used");
        assert!(!limit_reached(50_000, 49_999), "one unit under the ceiling is fine");
        assert!(limit_reached(50_000, 50_000), "exactly at the ceiling is out of budget");
        assert!(limit_reached(50_000, 60_000), "over the ceiling stays blocked");
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
            assert_eq!(w.check(3, start), RateDecision::Allow, "request {i} is within 3 rpm");
        }
        assert_eq!(
            w.check(3, start),
            RateDecision::Deny { retry_after_secs: 60 },
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
        assert_eq!(w.check(3, start + Duration::from_secs(60)), RateDecision::Allow);
        assert_eq!(w.check(3, start + Duration::from_secs(60)), RateDecision::Allow);
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
            RateDecision::Deny { retry_after_secs: 1 },
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

    // DEFECT 2 regression: the invalidation drops the entry, and a lookup that
    // read the row before the invalidation cannot put it back.
    #[test]
    fn invalidation_drops_one_key_and_stops_a_stale_re_insert() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);
        c.insert("hash-b".into(), meta(None), now);

        c.remove("hash-a");

        assert_eq!(c.get("hash-a", now), None, "the revoked key is gone immediately");
        assert!(c.get("hash-b", now).is_some(), "only the named key is dropped");
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
        assert!(matches!(&err, AppError::ValidationFailed(_)));
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.to_string().contains("stream"), "the field is named");

        assert!(stream_flag_allowed(Some(true)).is_ok());
        assert!(stream_flag_allowed(None).is_ok(), "absent keeps the default");
    }

    #[test]
    fn the_stream_flag_is_read_only_from_a_real_bool() {
        assert_eq!(requested_stream_flag(&json!({"stream": false})), Some(false));
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
        assert_eq!(forced["temperature"], json!(0.2), "the body is otherwise untouched");

        // A non-object body is refused before any money moves.
        assert!(force_streaming(json!([1, 2])).is_err());
    }
}
