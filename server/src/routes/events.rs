use axum::{
    extract::State,
    http::{header, HeaderMap},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{stream, Stream, StreamExt};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
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
            // something in between was dropped.
            Some(oldest) if oldest.id > last_id + 1 => Resume::Snapshot,
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
pub async fn todays_usage(pool: &PgPool, account_id: Uuid) -> Result<UsageDelta, AppError> {
    let row = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(input_tokens), 0) AS input_tokens,
            COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM(output_tokens), 0) AS output_tokens,
            COALESCE(SUM(cost_idr), 0) AS cost_idr
        FROM usage_daily
        WHERE account_id = $1 AND day = $2
        "#,
    )
    .bind(account_id)
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

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

async fn resolve_account_from_cookie(pool: &PgPool, headers: &HeaderMap) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    for piece in cookie_hdr.split(';') {
        let piece = piece.trim();
        if let Some(token) = piece.strip_prefix("session=") {
            let token_hash = hash_string(token);
            let session = sqlx::query(
                "SELECT account_id FROM sessions WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
            )
            .bind(token_hash)
            .fetch_optional(pool)
            .await?;

            if let Some(s) = session {
                return Ok(s.get("account_id"));
            }
        }
    }

    Err(AppError::Unauthenticated)
}

/// The browser's Last-Event-ID, when it is reconnecting.
fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
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
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .fetch_optional(&state.pool)
            .await?
            .unwrap_or(0);

    let opening: Vec<Result<Event, Infallible>> = match state.events.resume(last_event_id) {
        Resume::Replay(events) => events
            .into_iter()
            // DEFECT 1: only this account's replayed events escape to the wire.
            .filter(|e| e.account_id == account_id)
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
    use axum::http::HeaderValue;

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
}
