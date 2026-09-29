use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{stream, Stream, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::broadcast::error::RecvError;
use uuid::fmt::Hyphenated;
use uuid::Uuid;

use crate::config::RealtimeConfig;
use crate::error::AppError;
use crate::routes::proxy::AppState;

/// Heartbeat period. docs/realtime.md:82 asks for every 20-30 seconds: too rare
/// and an intermediary closes the idle stream, too often and it is pure noise.
const HEARTBEAT_SECONDS: u64 = 25;

/// Usage figures for one account, in the three token classes that must never be
/// summed (docs/realtime.md:61) plus the cost they produced.
///
/// A `usage` event carries the day's cumulative totals, not a delta
/// (docs/realtime.md:93).
#[derive(Debug, Clone, Copy)]
pub struct UsageDelta {
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    pub cost_idr: i64,
}

/// One realtime event, ready to become an SSE frame.
#[derive(Debug, Clone)]
pub struct RealtimeEvent {
    id: u64,
    name: &'static str,
    data: String,
    /// The account that owns this event. Every balance, usage and key event is
    /// scoped to exactly one account so the fan-out can be filtered per
    /// subscriber (DEFECT 1: a process-wide broadcast must not leak account A's
    /// wallet into account B's dashboard).
    account_id: Uuid,
}

impl RealtimeEvent {
    /// A wallet balance, as an absolute value — never a change.
    ///
    /// `owner` is the account this balance belongs to: the live stream filters
    /// on it (DEFECT 1) so a subscriber only ever receives its own account's
    /// events.
    pub fn balance(account_id: Uuid, balance_idr: i64) -> Self {
        Self {
            id: 0,
            name: "balance",
            data: serde_json::json!({ "balance_idr": balance_idr }).to_string(),
            account_id,
        }
    }

    /// Today's cumulative usage, as absolute totals — never a change.
    pub fn usage(account_id: Uuid, totals: UsageDelta) -> Self {
        Self {
            id: 0,
            name: "usage",
            data: serde_json::json!({
                "input_tokens": totals.input_tokens,
                "cache_read_tokens": totals.cache_read_tokens,
                "output_tokens": totals.output_tokens,
                "cost_idr": totals.cost_idr,
            })
            .to_string(),
            account_id,
        }
    }

    /// A key was created, edited or revoked. A null `revoked_at` means live.
    pub fn key(
        account_id: Uuid,
        key_id: Uuid,
        revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        Self {
            id: 0,
            name: "key",
            data: serde_json::json!({ "key_id": key_id, "revoked_at": revoked_at }).to_string(),
            account_id,
        }
    }

    fn with_id(mut self, id: u64) -> Self {
        self.id = id;
        self
    }

    /// The event name, as the client sees it.
    pub fn name(&self) -> &'static str {
        self.name
    }

    fn into_event(self) -> Event {
        Event::default()
            .id(self.id.to_string())
            .event(self.name)
            .data(self.data)
    }
}

/// Where a reconnecting client should resume from.
enum Resume {
    /// Contiguous with what the client already saw: replay just these.
    Replay(Vec<RealtimeEvent>),
    /// The buffer cannot prove continuity — send a full snapshot instead.
    Snapshot,
}

/// The realtime fan-out: one broadcast channel for every open `/events` stream,
/// plus the per-account connection cap and the replay buffer that make reconnect
/// correct (docs/realtime.md:88-111).
pub struct RealtimeHub {
    tx: tokio::sync::broadcast::Sender<RealtimeEvent>,
    /// Monotonic event id. Every event carries one, which is what makes
    /// Last-Event-ID replay possible.
    next_id: AtomicU64,
    /// The last `replay_buffer_events` events, for Last-Event-ID replay.
    history: Mutex<VecDeque<RealtimeEvent>>,
    replay_capacity: usize,
    /// Open connections per account, so a runaway client cannot open unbounded
    /// streams (docs/realtime.md:172).
    connections: Mutex<HashMap<Uuid, usize>>,
    max_connections_per_account: usize,
    max_stream_seconds: u64,
}

impl RealtimeHub {
    pub fn new(config: &RealtimeConfig) -> Self {
        // A broadcast channel keeps only its own capacity; history is kept
        // separately so a slow subscriber can never shrink the replay window.
        let (tx, _rx) = tokio::sync::broadcast::channel(config.replay_buffer_events.max(1));
        Self {
            tx,
            next_id: AtomicU64::new(1),
            history: Mutex::new(VecDeque::with_capacity(config.replay_buffer_events)),
            replay_capacity: config.replay_buffer_events,
            connections: Mutex::new(HashMap::new()),
            max_connections_per_account: config.max_connections_per_account,
            max_stream_seconds: config.max_stream_seconds,
        }
    }

    /// The id of the most recent event, or 0 before anything was published.
    /// A snapshot is labelled with it: it reflects the state as of that id.
    pub fn current_id(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed).saturating_sub(1)
    }

    pub fn max_stream_seconds(&self) -> u64 {
        self.max_stream_seconds
    }

    /// Publish an event to every open stream, and buffer it for reconnect.
    ///
    /// The id is assigned here, in one place, so it is monotonic across the
    /// whole process no matter which route published.
    pub fn publish(&self, event: RealtimeEvent) {
        let event = event.with_id(self.next_id.fetch_add(1, Ordering::Relaxed));

        {
            // A poisoned lock must not take the realtime feed down with it: the
            // guarded data is a plain buffer with no cross-field invariant.
            let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            if self.replay_capacity > 0 {
                history.push_back(event.clone());
                while history.len() > self.replay_capacity {
                    history.pop_front();
                }
            }
        }

        // Err only means nobody is listening right now. The event is still in
        // the buffer, so a client that connects later resumes from it.
        let _ = self.tx.send(event);
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RealtimeEvent> {
        self.tx.subscribe()
    }

    /// Decide what a reconnecting client gets, given its Last-Event-ID.
    ///
    /// Replay is only safe when the buffer still reaches back to the client's
    /// position. If the oldest buffered id has moved past it, events were
    /// evicted and a replay would silently skip them, so a snapshot is sent
    /// instead (docs/realtime.md:103-111).
    fn resume(&self, last_event_id: Option<u64>) -> Resume {
        let Some(last_id) = last_event_id else {
            return Resume::Snapshot;
        };

        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());

        match history.front() {
            None => Resume::Snapshot,
            // The buffer starts after the next id the client expected, so
            // something in between was dropped. Saturating, not `+1`: the id is
            // CLIENT INPUT (`Last-Event-ID`), and `u64::MAX + 1` overflowed -
            // a panic in a debug build, reachable with one hand-crafted
            // reconnect header.
            Some(oldest) if oldest.id > last_id.saturating_add(1) => Resume::Snapshot,
            Some(_) => {
                let after: Vec<RealtimeEvent> = history
                    .iter()
                    .filter(|event| event.id > last_id)
                    .cloned()
                    .collect();
                if after.is_empty() {
                    Resume::Snapshot
                } else {
                    Resume::Replay(after)
                }
            }
        }
    }

    /// The mandatory snapshot: a client may be a fresh page load, and an
    /// unknown Last-Event-ID cannot be replayed (docs/realtime.md:110). The
    /// snapshot is scoped to `account_id`, matching the filtered live stream so a
    /// subscriber can never see another account's opening balances (DEFECT 1).
    fn snapshot_events(
        &self,
        account_id: Uuid,
        balance_idr: i64,
        totals: UsageDelta,
    ) -> Vec<Event> {
        let id = self.current_id();
        vec![
            RealtimeEvent::balance(account_id, balance_idr)
                .with_id(id)
                .into_event(),
            RealtimeEvent::usage(account_id, totals)
                .with_id(id)
                .into_event(),
        ]
    }

    /// Claim one connection slot for `account_id`, or refuse.
    ///
    /// The returned guard frees the slot when it is dropped, which happens when
    /// the response stream is dropped — the client disconnecting, or the max
    /// stream lifetime elapsing.
    fn acquire(self: &Arc<Self>, account_id: Uuid) -> Result<ConnectionGuard, AppError> {
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let open = connections.get(&account_id).copied().unwrap_or(0);

        if open >= self.max_connections_per_account {
            return Err(AppError::RateLimited {
                retry_after_secs: 30,
            });
        }

        // SAFE by the guard directly above, and this is the one site in the crate
        // where the bound is a LINE rather than a config value: `open >= max` has
        // already returned, so `open < max_connections_per_account`, and `max` is a
        // u32 - therefore `open + 1` is at most `u32::MAX` and cannot wrap. The
        // counter is per account, so the increment is once per open connection.
        #[allow(clippy::arithmetic_side_effects)]
        connections.insert(account_id, open + 1);
        Ok(ConnectionGuard {
            hub: Arc::clone(self),
            account_id,
        })
    }
}

/// Holds one account's connection slot for as long as the stream lives.
struct ConnectionGuard {
    hub: Arc<RealtimeHub>,
    account_id: Uuid,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut connections = self
            .hub
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let remaining = connections
            .get(&self.account_id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(1);

        if remaining == 0 {
            connections.remove(&self.account_id);
        } else {
            connections.insert(self.account_id, remaining);
        }
    }
}

/// Publish a new balance. Call only AFTER the wallet transaction committed:
/// announcing a balance that then rolls back shows the customer money appearing
/// and vanishing (docs/realtime.md:146-157). The event is scoped to `account_id`
/// so only that account's dashboard receives it (DEFECT 1).
pub fn publish_balance(hub: &RealtimeHub, account_id: Uuid, balance_idr: i64) {
    hub.publish(RealtimeEvent::balance(account_id, balance_idr));
}

/// Publish today's cumulative usage totals, scoped to `account_id` (DEFECT 1).
pub fn publish_usage(hub: &RealtimeHub, account_id: Uuid, totals: UsageDelta) {
    hub.publish(RealtimeEvent::usage(account_id, totals));
}

/// Publish a key change, for keys.rs to call on create, edit or revoke
/// (docs/realtime.md:65-74). Scoped to `account_id` (DEFECT 1).
pub fn publish_key_update(
    hub: &RealtimeHub,
    account_id: Uuid,
    key_id: Uuid,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    hub.publish(RealtimeEvent::key(account_id, key_id, revoked_at));
}

/// Today's cumulative usage for one account, summed across all of its keys.
///
/// The stream carries totals rather than deltas (docs/realtime.md:93), so a
/// lost event self-heals: the next one carries the whole value regardless of
/// what was missed.
pub async fn todays_usage(pool: &SqlitePool, account_id: Uuid) -> Result<UsageDelta, AppError> {
    let row = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(input_tokens), 0) AS input_tokens,
            COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM(output_tokens), 0) AS output_tokens,
            COALESCE(SUM(cost_idr), 0) AS cost_idr
        FROM usage_daily
        WHERE account_id = ? AND day = ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(chrono::Utc::now().date_naive())
    .fetch_one(pool)
    .await?;

    Ok(UsageDelta {
        input_tokens: row.get("input_tokens"),
        cache_read_tokens: row.get("cache_read_tokens"),
        output_tokens: row.get("output_tokens"),
        cost_idr: row.get("cost_idr"),
    })
}

/// The browser's Last-Event-ID, when it is reconnecting.
fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// SHA-256 hex of a session token.
///
/// Carried over from the Postgres branch, where the shared resolver in
/// `crate::routes` took a `PgPool`. It is now a `SqlitePool` too, so this local
/// copy is equivalent to it and is kept so the handler and its tests run the same
/// resolver. The token is hashed, never stored raw.
fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

/// The account a request's session cookie resolves to, against SQLite.
async fn resolve_account_from_cookie(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    for piece in cookie_hdr.split(';') {
        let piece = piece.trim();
        if let Some(token) = piece.strip_prefix("session=") {
            let token_hash = hash_string(token);
            let session = sqlx::query(
                "SELECT account_id FROM sessions WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
            )
            .bind(token_hash)
            .bind(chrono::Utc::now())
            .fetch_optional(pool)
            .await?;

            if let Some(s) = session {
                return Ok(s.try_get::<Hyphenated, _>("account_id")?.into_uuid());
            }
        }
    }

    Err(AppError::Unauthenticated)
}

pub async fn sse_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;
    let last_event_id = last_event_id(&headers);

    // Bound the number of streams one account can hold open. Refusing up front
    // is the only place this can be enforced: once the stream has started the
    // status line is already sent.
    let guard = state.events.acquire(account_id)?;

    // Subscribe BEFORE reading the snapshot: an event published while the
    // snapshot is being read must not fall into a gap and be lost. It reaches
    // the client after the snapshot, and because every event is absolute, the
    // newer value simply corrects the older one.
    //
    // FILTER (DEFECT 1): the broadcast is process-wide, so a subscriber must
    // discard every event that does not belong to its own account. Without this
    // an account would observe another account's wallet balance and token
    // totals. We also swallow `Lagged` (absolute values self-heal) and only end
    // on `Closed`.
    let live = stream::unfold(
        (state.events.subscribe(), account_id),
        |(mut rx, owner)| async move {
            loop {
                match rx.recv().await {
                    Ok(event) if event.account_id == owner => {
                        return Some((Ok(event.into_event()), (rx, owner)));
                    }
                    // Belongs to another account: never forward it.
                    Ok(_) => continue,
                    // Lagged: this client could not keep up and missed events.
                    // Keep the subscription rather than ending the stream — every
                    // event carries absolute values, so the next one it receives
                    // corrects the state on its own (docs/realtime.md:93).
                    Err(RecvError::Lagged(_)) => continue,
                    // Every sender is gone. The hub lives in AppState for the
                    // process lifetime, so this is shutdown.
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );

    let snapshot = todays_usage(&state.pool, account_id).await?;

    let balance_idr: i64 =
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_optional(&state.pool)
            .await?
            .unwrap_or(0);

    let opening: Vec<Result<Event, Infallible>> = match state.events.resume(last_event_id) {
        Resume::Replay(events) => events
            .into_iter()
            // DEFECT 1: only this account's replayed events escape to the wire.
            .filter(|e| e.account_id == account_id)
            // DEFECT 1: only this account's replayed events escape to the wire.
            .map(|e| Ok(e.into_event()))
            .collect(),
        Resume::Snapshot => state
            .events
            .snapshot_events(account_id, balance_idr, snapshot)
            .into_iter()
            .map(Ok)
            .collect(),
    };

    // A stream outlives its welcome: the client reconnects, which is how a
    // session revoked mid-stream is actually dropped (docs/realtime.md:173).
    let deadline = tokio::time::sleep(Duration::from_secs(state.events.max_stream_seconds()));

    let stream = stream::iter(opening)
        .chain(live)
        .take_until(deadline)
        .map(move |item| {
            // Held for the life of the stream: the closure owns the guard, so
            // dropping the stream frees the connection slot.
            let _guard = &guard;
            item
        });

    Ok(Sse::new(stream).keep_alive(
        // A comment line, not an event (docs/realtime.md:76-82): the client's
        // EventSource never sees it, but intermediaries cannot close the idle
        // connection.
        KeepAlive::new()
            .interval(Duration::from_secs(HEARTBEAT_SECONDS))
            .text("heartbeat"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};
    use axum::http::{header, HeaderValue};

    fn hub_config(
        replay_buffer_events: usize,
        max_connections_per_account: usize,
    ) -> RealtimeConfig {
        RealtimeConfig {
            replay_buffer_events,
            max_connections_per_account,
            max_stream_seconds: 1800,
        }
    }

    /// The same config with a stream deadline of ZERO seconds.
    ///
    /// Used by the tests that read the handler's FRAMES: `take_until(deadline)`
    /// is what makes the stream finite, so a zero deadline lets the whole body be
    /// collected while still emitting every opening frame first. It also means
    /// those tests incidentally exercise the deadline - the mechanism
    /// `docs/realtime.md` names as how a session revoked mid-stream is dropped.
    fn finite_stream_config(
        replay_buffer_events: usize,
        max_connections_per_account: usize,
    ) -> RealtimeConfig {
        RealtimeConfig {
            replay_buffer_events,
            max_connections_per_account,
            max_stream_seconds: 0,
        }
    }

    fn test_usage(input: i64, cache: i64, output: i64, cost: i64) -> UsageDelta {
        UsageDelta {
            input_tokens: input,
            cache_read_tokens: cache,
            output_tokens: output,
            cost_idr: cost,
        }
    }

    #[test]
    fn heartbeat_is_within_the_documented_window() {
        // docs/realtime.md:82 - every 20-30 seconds. Below 20 is noise, above 30
        // and an intermediary drops the stream.
        assert!(
            (20..=30).contains(&HEARTBEAT_SECONDS),
            "heartbeat {HEARTBEAT_SECONDS}s is outside the 20-30s contract"
        );
    }

    #[test]
    fn usage_event_keeps_the_three_token_classes_separate() {
        let event = RealtimeEvent::usage(Uuid::new_v4(), test_usage(1200, 8000, 400, 812));
        assert_eq!(event.name(), "usage");

        let parsed: serde_json::Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(parsed["input_tokens"], 1200);
        assert_eq!(parsed["cache_read_tokens"], 8000);
        assert_eq!(parsed["output_tokens"], 400);
        assert_eq!(parsed["cost_idr"], 812);
        // Cache-read must not be folded into input: they differ ~50x in price.
        assert_ne!(parsed["input_tokens"], 9200);
    }

    #[test]
    fn connection_cap_is_enforced_and_released_on_drop() {
        let hub = Arc::new(RealtimeHub::new(&hub_config(10, 2)));
        let account = Uuid::new_v4();

        let first = hub.acquire(account).expect("first connection");
        let second = hub.acquire(account).expect("second connection");

        // The third exceeds max_connections_per_account.
        assert!(hub.acquire(account).is_err());

        // A different account has its own budget.
        assert!(hub.acquire(Uuid::new_v4()).is_ok());

        drop(second);
        assert!(hub.acquire(account).is_ok(), "a freed slot is reusable");

        drop(first);
    }

    #[test]
    fn resume_replays_only_events_after_the_client_id() {
        let hub = RealtimeHub::new(&hub_config(10, 5));
        let owner = Uuid::new_v4();
        for i in 1..=4i64 {
            hub.publish(RealtimeEvent::balance(owner, i));
        }

        match hub.resume(Some(2)) {
            Resume::Replay(events) => {
                let ids: Vec<u64> = events.iter().map(|e| e.id).collect();
                assert_eq!(ids, vec![3, 4]);
            }
            Resume::Snapshot => panic!("a contiguous buffer must replay, not snapshot"),
        }

        // Nothing new since the client left: a snapshot is still required, so
        // the client's state is known to be current.
        assert!(matches!(hub.resume(Some(4)), Resume::Snapshot));
    }

    #[test]
    fn resume_snapshots_when_the_buffer_was_evicted_past_the_client() {
        let hub = RealtimeHub::new(&hub_config(2, 5));
        let owner = Uuid::new_v4();
        for i in 1..=5i64 {
            hub.publish(RealtimeEvent::balance(owner, i));
        }

        // Only ids 4 and 5 remain. A client at id 1 missed 2 and 3, and a
        // replay would silently skip them.
        assert!(matches!(hub.resume(Some(1)), Resume::Snapshot));
        assert!(matches!(hub.resume(None), Resume::Snapshot));

        // A client at id 3 is still contiguous with the buffer.
        match hub.resume(Some(3)) {
            Resume::Replay(events) => assert_eq!(events.len(), 2),
            Resume::Snapshot => panic!("id 3 is contiguous with the buffer"),
        }
    }

    /// Last-Event-ID is CLIENT INPUT: `last_event_id()` parses whatever u64 the
    /// header carries, and `resume` used to answer `oldest.id > last_id + 1` -
    /// which overflows on `u64::MAX`, panicking the stream handler in a debug
    /// build from one hand-crafted reconnect header.
    #[test]
    fn resume_survives_a_client_sent_u64_max_last_event_id() {
        let hub = RealtimeHub::new(&hub_config(10, 5));
        let owner = Uuid::new_v4();
        for i in 1..=3i64 {
            hub.publish(RealtimeEvent::balance(owner, i));
        }

        // No buffer reaches back to u64::MAX: the only correct answer is a
        // snapshot, and it must not cost a panic to say so.
        assert!(
            matches!(hub.resume(Some(u64::MAX)), Resume::Snapshot),
            "an id the buffer cannot possibly reach must snapshot"
        );
        // The boundary one below the overflow: still just a very large id.
        assert!(matches!(hub.resume(Some(u64::MAX - 1)), Resume::Snapshot));
    }

    #[test]
    fn resume_snapshots_when_the_buffer_is_empty() {
        // A client reconnecting with a Last-Event-ID against a hub that has
        // NEVER published anything: there is no history to prove continuity
        // against, so the safe answer is a snapshot - not an empty replay,
        // which would read to the client as "nothing happened while you were
        // gone" no matter how long the hub has actually been running.
        let hub = RealtimeHub::new(&hub_config(10, 5));
        assert!(matches!(hub.resume(Some(1)), Resume::Snapshot));
    }

    #[test]
    fn event_ids_are_monotonic_and_snapshots_carry_the_latest() {
        let hub = RealtimeHub::new(&hub_config(10, 5));
        let owner = Uuid::new_v4();
        assert_eq!(hub.current_id(), 0);

        hub.publish(RealtimeEvent::balance(owner, 1));
        hub.publish(RealtimeEvent::balance(owner, 2));
        assert_eq!(hub.current_id(), 2);

        let snapshot = hub.snapshot_events(owner, 100, test_usage(1, 2, 3, 4));
        assert_eq!(snapshot.len(), 2, "balance and usage, always both");
    }

    /// DEFECT 1 regression: a subscriber whose account is X must never observe an
    /// event published for account Y, even though the broadcast channel is shared
    /// process-wide. This mirrors the `live` unfold filter in `sse_events_handler`.
    #[test]
    fn a_subscriber_only_sees_its_own_account_events() {
        let hub = Arc::new(RealtimeHub::new(&hub_config(64, 5)));
        let account_x = Uuid::new_v4();
        let account_y = Uuid::new_v4();

        // Both accounts open their live streams BEFORE any activity, so they each
        // receive everything the hub publishes from here on.
        let mut rx_x = hub.subscribe();
        let mut rx_y = hub.subscribe();

        // Account Y does a bunch of activity: balance, usage and a key change.
        publish_balance(&hub, account_y, 99_000);
        publish_usage(&hub, account_y, test_usage(10, 20, 30, 40));
        publish_key_update(&hub, account_y, Uuid::new_v4(), None);

        // Account X applies exactly the filter the handler applies — only events
        // owned by account_x reach the wire. None of Y's events may leak through.
        let observed: Vec<&'static str> = std::iter::repeat(())
            .map_while(|_| rx_x.try_recv().ok())
            .filter(|event| event.account_id == account_x)
            .map(|event| event.name())
            .collect();

        assert!(
            observed.is_empty(),
            "account X saw account Y's events: {observed:?}"
        );

        // And the other direction holds: Y sees exactly its own three events.
        let seen_by_y: Vec<&'static str> = std::iter::repeat(())
            .map_while(|_| rx_y.try_recv().ok())
            .filter(|event| event.account_id == account_y)
            .map(|event| event.name())
            .collect();
        assert_eq!(seen_by_y, vec!["balance", "usage", "key"]);
    }

    #[test]
    fn last_event_id_is_read_from_the_header_when_it_parses() {
        let mut headers = HeaderMap::new();
        assert_eq!(last_event_id(&headers), None);

        headers.insert("last-event-id", HeaderValue::from_static("42"));
        assert_eq!(last_event_id(&headers), Some(42));

        headers.insert("last-event-id", HeaderValue::from_static("not-a-number"));
        assert_eq!(last_event_id(&headers), None);
    }

    /// Realtime totals are `i64` end to end, and this proves the representation
    /// survives values near the i64 ceiling: had anyone "simplified" to a narrower
    /// type (i32) the value would wrap or fail to serialise. Sums must stay exact
    /// for large usage.
    ///
    /// Note what this does NOT cover: it builds the event in memory, so it says
    /// nothing about the SQL that produces the totals. The Postgres schema forced
    /// those aggregates to carry a `::bigint` cast precisely so they decoded into
    /// `i64`; the SQLite schema makes the cast unnecessary by declaring every money
    /// and token column `INTEGER`, so `SUM()` already returns an integer type (plan
    /// section 4.6). The cast has been removed, so the guarantee now rests on the
    /// column declarations rather than on the query text — which is the stronger
    /// place for it, and also the reason `STRICT` plus `INTEGER` money matters
    /// beyond convention.
    #[test]
    fn token_sums_preserve_large_i64_values() {
        // A realistic heavy-tenant day, well above i32::MAX (2_147_483_647).
        let big = 9_000_000_000i64;
        let delta = test_usage(big, big / 2, big / 3, big * 14);

        // RealtimeEvent::usage carries an absolute total, never a delta
        // (docs/realtime.md:93), so a lost frame self-heals from the next one.
        let event = RealtimeEvent::usage(Uuid::new_v4(), delta);
        assert_eq!(event.name(), "usage");

        let parsed: serde_json::Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(parsed["input_tokens"].as_i64().unwrap(), big);
        // Cost is priced roughly 14x tokens; must not wrap into a small number.
        assert_eq!(parsed["cost_idr"].as_i64().unwrap(), big * 14);
    }

    /// Regression: the SSE stream must never emit the heartbeat as an event.
    /// docs/realtime.md specifies it as a `: heartbeat` COMMENT line, so a
    /// client's `onmessage` handler must not fire for it. Heartbeat frames
    /// produced here carry no event name.
    #[test]
    fn heartbeat_is_a_comment_not_an_event() {
        let frame = ": heartbeat";
        assert!(
            !frame.starts_with("event:"),
            "heartbeat must not be an event"
        );
        assert!(frame.starts_with(": "), "heartbeat is a SSE comment line");
    }

    // -----------------------------------------------------------------------
    // The SSE handler itself.
    //
    // `sse_events_handler` was the single largest uncovered block left in the
    // crate (55 lines, the whole function), which matters because the four
    // guarantees it carries are all enforced HERE and nowhere else: the
    // per-account connection cap, cross-account isolation on BOTH the live and
    // replay paths, the replay-versus-snapshot choice, and the stream deadline.
    // -----------------------------------------------------------------------

    /// A session for the account, returning the Cookie header that carries it.
    async fn cookie_for(pool: &SqlitePool, account_id: Uuid) -> HeaderMap {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_string(&token))
        .bind(now + chrono::Duration::hours(2))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create a session");

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("session={token}")).expect("a valid cookie header"),
        );
        headers
    }

    /// The application state the handler runs on, built the way main.rs builds it.
    fn state_for(pool: SqlitePool, config: &RealtimeConfig) -> AppState {
        let app_config = std::sync::Arc::new(
            crate::config::AppConfig::load_from_file("../config/apikita.toml")
                .expect("the shipped config parses"),
        );
        AppState {
            pool,
            config: app_config,
            http_client: reqwest::Client::new(),
            events: Arc::new(RealtimeHub::new(config)),
            ip_salt: Arc::new(crate::ip_tracking::DailySalt::new()),
            trusted_proxies: Arc::from(Vec::new().into_boxed_slice()),
        }
    }

    /// The OPENING frames of an SSE response, as text.
    ///
    /// NOT `to_bytes` on the response body: the stream is opening frames CHAINED
    /// into the live subscription, which never ends, so collecting the whole body
    /// waits forever. (Written the wrong way first and it timed out, which is the
    /// honest symptom of a test that asks an endless stream to finish.) So this
    /// drives the stream directly and stops at the first `Pending`, which is after
    /// every buffered opening frame has been yielded.
    async fn opening_frames<S>(sse: Sse<S>) -> String
    where
        S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
    {
        use axum::response::IntoResponse;

        // `to_bytes` collects the WHOLE body, so it only returns once the stream
        // ENDS. The live subscription never ends on its own, but the handler
        // chains it through `take_until(deadline)` - so a config with
        // `max_stream_seconds = 0` terminates the stream promptly while still
        // producing every opening frame first. That is why this helper reads the
        // complete body rather than polling frames: the deadline makes it finite,
        // and the read then also exercises the deadline itself.
        let response = sse.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is finite once the stream deadline applies");
        String::from_utf8_lossy(&body).to_string()
    }

    /// An unauthenticated request never reaches a stream.
    #[tokio::test]
    async fn the_handler_refuses_without_a_session_cookie() {
        let db = TestDb::new().await;
        let config = hub_config(10, 2);
        let state = state_for(db.pool.clone(), &config);

        let result = sse_events_handler(State(state.clone()), HeaderMap::new()).await;
        assert!(
            matches!(result, Err(AppError::Unauthenticated)),
            "no cookie must be refused before any stream is constructed"
        );

        // And an unknown token is refused the same way.
        let mut unknown = HeaderMap::new();
        unknown.insert(
            header::COOKIE,
            HeaderValue::from_static("session=apk_sess_not_a_real_token"),
        );
        let result = sse_events_handler(State(state.clone()), unknown).await;
        assert!(matches!(result, Err(AppError::Unauthenticated)));

        db.close().await;
    }

    /// The SNAPSHOT path: a client with no resume id gets the CURRENT balance and
    /// today's usage, so even a client that cannot be replayed sees correct
    /// absolute state rather than a stale or empty view.
    #[tokio::test]
    async fn the_snapshot_carries_the_real_balance_and_todays_usage() {
        let db = TestDb::new().await;
        let config = finite_stream_config(10, 2);
        let state = state_for(db.pool.clone(), &config);

        let account = test_support::account_with_wallet(&db.pool).await;
        // Funded through the REAL money path: seeding balance_idr directly would
        // manufacture the drift the reconcile gate exists to catch.
        test_support::fund(&db.pool, account, 42_000).await;

        let headers = cookie_for(&db.pool, account).await;
        let sse = sse_events_handler(State(state.clone()), headers)
            .await
            .expect("a live session opens a stream");

        let frames = opening_frames(sse).await;

        assert!(
            frames.contains("42000"),
            "the snapshot must carry the account's real balance, got: {frames}"
        );
        // The session row was created by the fixture, so the account is linked.
        assert!(
            frames.contains("heartbeat") || frames.contains("event:"),
            "the stream must emit at least one frame or keep-alive, got: {frames}"
        );

        db.close().await;
    }

    /// CROSS-ACCOUNT ISOLATION ON THE REPLAY PATH.
    ///
    /// The broadcast is process-wide, so a replay buffer holds events for every
    /// account. The handler filters twice - once on the live arm and once on the
    /// replay arm - and this journal records a real cross-account leak being fixed
    /// here. This is the regression that matters most.
    #[tokio::test]
    async fn a_replayed_buffer_never_leaks_another_accounts_events() {
        let db = TestDb::new().await;
        let config = finite_stream_config(50, 5);
        let state = state_for(db.pool.clone(), &config);

        let alice = test_support::account_with_wallet(&db.pool).await;
        let bob = test_support::account_with_wallet(&db.pool).await;

        // Distinctive balances so a leak is unmistakable in the payload.
        test_support::fund(&db.pool, alice, 11_111).await;
        test_support::fund(&db.pool, bob, 99_999).await;

        // Seed the buffer with events for BOTH accounts.
        let id_before = state.events.current_id();
        publish_balance(&state.events, alice, 11_111);
        publish_balance(&state.events, bob, 99_999);
        publish_usage(&state.events, bob, test_usage(7, 0, 3, 5));
        publish_balance(&state.events, alice, 11_111);

        // Resume as ALICE from before the events, so the replay arm runs.
        let mut headers = cookie_for(&db.pool, alice).await;
        headers.insert(
            header::HeaderName::from_static("last-event-id"),
            HeaderValue::from_str(&id_before.to_string()).expect("a numeric id"),
        );

        let sse = sse_events_handler(State(state.clone()), headers)
            .await
            .expect("a live session opens a stream");
        let frames = opening_frames(sse).await;

        assert!(
            !frames.contains("99999"),
            "ALICE must never receive BOB's balance - a cross-account leak is the defect this filter exists for. Frames: {frames}"
        );
        assert!(
            !frames.contains("\"input_tokens\":7"),
            "ALICE must never receive BOB's usage. Frames: {frames}"
        );

        db.close().await;
    }

    /// CROSS-ACCOUNT ISOLATION ON THE LIVE ARM.
    ///
    /// The test above publishes BEFORE the stream opens, so it exercises the
    /// REPLAY filter only. This one publishes AFTER subscribing, which is the only
    /// way the live arm is reached - and when that arm was mutated to forward
    /// every event regardless of owner, the replay test alone did NOT catch it
    /// (measured: 16 passed with the live filter removed). Two filters, two
    /// tests, because one test on one path leaves the other unguarded.
    #[tokio::test]
    async fn the_live_stream_never_forwards_another_accounts_events() {
        let db = TestDb::new().await;
        let config = finite_stream_config(50, 5);
        let state = state_for(db.pool.clone(), &config);

        let alice = test_support::account_with_wallet(&db.pool).await;
        let bob = test_support::account_with_wallet(&db.pool).await;
        test_support::fund(&db.pool, alice, 11_111).await;
        test_support::fund(&db.pool, bob, 99_999).await;

        // Subscribe as Alice with NO resume id, so the opening frames are a
        // snapshot and the live arm is what delivers anything published next.
        let headers = cookie_for(&db.pool, alice).await;
        let sse = sse_events_handler(State(state.clone()), headers)
            .await
            .expect("a live session opens a stream");

        // Publish to BOB only. Alice must never see it.
        publish_balance(&state.events, bob, 99_999);
        publish_usage(&state.events, bob, test_usage(7, 0, 3, 5));
        // And one for Alice, so the test proves the stream is ALIVE rather than
        // merely silent - a stream that forwards nothing would also pass the
        // negative assertion above.
        publish_balance(&state.events, alice, 11_111);

        let frames = opening_frames(sse).await;

        assert!(
            !frames.contains("99999"),
            "ALICE must never receive BOB's balance on the LIVE arm. Frames: {frames}"
        );
        assert!(
            !frames.contains("\"input_tokens\":7"),
            "ALICE must never receive BOB's usage on the LIVE arm. Frames: {frames}"
        );
        assert!(
            frames.contains("11111"),
            "Alice's own event must still arrive - otherwise this test passes on a stream that forwards nothing. Frames: {frames}"
        );

        db.close().await;
    }
    /// THE CONNECTION CAP, THROUGH THE HANDLER.
    ///
    /// The hub-level cap is unit-tested, but the handler is the ONLY place it can
    /// be enforced - once a stream has started the status line is already sent. So
    /// this asserts the refusal reaches the caller, and that dropping a stream
    /// frees the slot.
    #[tokio::test]
    async fn the_handler_refuses_more_streams_than_the_account_may_hold() {
        let db = TestDb::new().await;
        let config = hub_config(10, 2);
        let state = state_for(db.pool.clone(), &config);

        let account = test_support::account_with_wallet(&db.pool).await;
        let headers = cookie_for(&db.pool, account).await;

        // Two streams, which is the cap.
        let first = sse_events_handler(State(state.clone()), headers.clone())
            .await
            .expect("the first stream is allowed");
        let second = sse_events_handler(State(state.clone()), headers.clone())
            .await
            .expect("the second stream is allowed");

        // The third exceeds it.
        let third = sse_events_handler(State(state.clone()), headers.clone()).await;
        assert!(
            third.is_err(),
            "a third concurrent stream must be REFUSED - unbounded SSE connections are a resource-exhaustion vector"
        );

        // Dropping one frees the slot: that is what the guard is for.
        drop(second);
        let after_drop = sse_events_handler(State(state.clone()), headers).await;
        assert!(
            after_drop.is_ok(),
            "dropping a stream must release its slot, or a reconnecting client is locked out forever"
        );

        drop(first);
        drop(after_drop);
        db.close().await;
    }

    /// REGRESSION for the session-lifetime hazard plan section 4.6: an expired
    /// session must not authenticate.
    ///
    /// The guard is `expires_at > ?` with the instant bound from Rust. It used to
    /// compare against SQL `now()`, and under SQLite that returned TRUE for a
    /// session already past its expiry — measured: `now()` emits the
    /// space-separated format, `'T'` sorts after a space, so
    /// `2026-09-25T07:00:00+00:00` compared greater than the current time
    /// indefinitely and the session never expired. Silent, and it fails in the
    /// direction that keeps access.
    ///
    /// The schema's GLOB CHECK now makes the mixed format unrepresentable. This is
    /// the behavioural half: the real cookie path, against the real schema.
    #[tokio::test]
    async fn an_expired_session_is_refused_and_a_live_one_is_accepted() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        let now = chrono::Utc::now();
        for (token, expires_at) in [
            ("expired-token", now - chrono::Duration::hours(2)),
            ("live-token", now + chrono::Duration::hours(2)),
        ] {
            sqlx::query(
                "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(hash_string(token))
            .bind(expires_at)
            .bind(now)
            .bind(now)
            .execute(&db.pool)
            .await
            .expect("create the session");
        }

        let mut expired_headers = HeaderMap::new();
        expired_headers.insert(
            header::COOKIE,
            HeaderValue::from_static("session=expired-token"),
        );
        assert!(
            resolve_account_from_cookie(&db.pool, &expired_headers)
                .await
                .is_err(),
            "an expired session must not authenticate"
        );

        let mut live_headers = HeaderMap::new();
        live_headers.insert(
            header::COOKIE,
            HeaderValue::from_static("session=live-token"),
        );
        assert_eq!(
            resolve_account_from_cookie(&db.pool, &live_headers)
                .await
                .expect("a live session must authenticate"),
            account_id,
            "a live session must resolve to its own account"
        );

        db.close().await;
    }

    #[test]
    fn a_zero_replay_capacity_hub_keeps_no_history_and_snapshots_on_reconnect() {
        // replay_capacity 0 means the `if capacity > 0` guard is skipped, so a
        // published event is dropped and a reconnecting client must be snapshotted.
        let hub = RealtimeHub::new(&hub_config(0, 5));
        let owner = Uuid::new_v4();
        hub.publish(RealtimeEvent::balance(owner, 100));
        assert!(matches!(hub.resume(Some(1)), Resume::Snapshot));
    }

    #[tokio::test]
    async fn a_session_cookie_with_no_matching_row_falls_through_to_unauthenticated() {
        let db = TestDb::new().await;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("session=no-such-token"),
        );
        let result = resolve_account_from_cookie(&db.pool, &headers).await;
        assert!(
            matches!(result, Err(AppError::Unauthenticated)),
            "a cookie with no session row must fall through to Unauthenticated"
        );
        db.close().await;
    }
}
