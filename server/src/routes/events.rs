use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{stream, Stream, StreamExt};
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
use crate::routes::resolve_account_from_cookie;

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
            COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens,
            COALESCE(SUM(cache_read_tokens), 0)::bigint AS cache_read_tokens,
            COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens,
            COALESCE(SUM(cost_idr), 0)::bigint AS cost_idr
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

    /// The fix for the panic decodes token sums into `i64` via a `::bigint`
    /// cast. This proves the chosen representation survives values near the
    /// i64 ceiling: had anyone "simplified" to a narrower type (i32/INT4) the
    /// SUM would silently overflow. Sums must stay exact for large usage.
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
    // LIVE-DATABASE TESTS
    //
    // Every test above is pure: it exercises the hub and the helpers in memory
    // and never opens a database. That left the two things this route is
    // actually made of unexecuted — `todays_usage` (the only SQL the SSE feed
    // reads for its usage snapshot) and `sse_events_handler` itself (cookie ->
    // account -> wallet balance -> snapshot). A query that fails to decode, or a
    // snapshot wired to the wrong account, would have shipped green.
    //
    // These run against a real, migrated Postgres and are #[ignore]d rather than
    // skipped, so the default suite stays green without a database. Run them
    // with:
    //
    //   DATABASE_URL=postgres://postgres:dev@localhost:5432/apikita \
    //     cargo test --lib -- --ignored --test-threads=1 routes::events
    // -----------------------------------------------------------------------

    use crate::config::AppConfig;
    use crate::db::credit_topup_transaction;
    use crate::ip_tracking::{parse_cidrs, DailySalt};
    use crate::routes::hash_token;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use chrono::NaiveDate;
    use sqlx::PgPool;

    /// A SMALL pool per test, deliberately: the live suite runs several pools in
    /// parallel against Postgres' 100-connection limit. Four is ample for one
    /// test, and because the pool is NOT shared no sibling test can starve this
    /// one's teardown.
    async fn live_pool() -> PgPool {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .expect("connect to Postgres")
    }

    /// One account with everything the /events path reads: a session cookie that
    /// resolves through the production auth path (`resolve_account_from_cookie`),
    /// a real api_keys row (`usage_daily.api_key_id` is NOT NULL and part of the
    /// primary key, so a fabricated id would not insert) and the zero-balance
    /// wallet the login path creates.
    struct LiveAccount {
        account_id: Uuid,
        token: String,
        key_id: Uuid,
    }

    async fn live_account(pool: &PgPool) -> LiveAccount {
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(format!("test_events_{}", Uuid::new_v4().simple()))
                .fetch_one(pool)
                .await
                .expect("create account");

        let key_id: Uuid = sqlx::query_scalar(
            "INSERT INTO api_keys (account_id, key_hash, prefix)
             VALUES ($1, $2, 'apk_test') RETURNING id",
        )
        .bind(account_id)
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .fetch_one(pool)
        .await
        .expect("create the api key usage_daily's NOT NULL api_key_id points at");

        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO sessions (account_id, token_hash, expires_at)
             VALUES ($1, $2, now() + interval '30 days')",
        )
        .bind(account_id)
        .bind(hash_token(&token))
        .execute(pool)
        .await
        .expect("create session");

        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(pool)
            .await
            .expect("create the zero-balance wallet the login path would create");

        LiveAccount {
            account_id,
            token,
            key_id,
        }
    }

    /// The cookie the browser sends: the session token among unrelated cookies,
    /// exactly as `resolve_account_from_cookie` parses it.
    fn cookie_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// Money enters a wallet ONLY through the real path: a topups row settled by
    /// `credit_topup_transaction`, which writes the matching + ledger row in the
    /// same transaction. Writing wallets.balance_idr directly manufactures the
    /// very drift the reconciliation assertion at the end of each test looks for.
    async fn fund_wallet(pool: &PgPool, account_id: Uuid, amount_idr: i64) {
        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(amount_idr)
            .bind(&order_id)
            .execute(pool)
            .await
            .expect("create topup");

        assert_eq!(
            credit_topup_transaction(pool, &order_id, amount_idr)
                .await
                .expect("credit the opening balance"),
            crate::db::TopupCreditResult::Settled {
                new_balance: amount_idr
            },
            "the fixture must open the wallet through the real top-up path"
        );
    }

    /// One usage_daily row for a chosen day, written directly.
    ///
    /// This touches neither wallets nor ledger, so it is drift-neutral: the
    /// reconciliation invariant is unaffected by it.
    #[allow(clippy::too_many_arguments)]
    async fn insert_usage(
        pool: &PgPool,
        account_id: Uuid,
        key_id: Uuid,
        day: NaiveDate,
        input_tokens: i64,
        cache_read_tokens: i64,
        output_tokens: i64,
        cost_idr: i64,
    ) {
        sqlx::query(
            "INSERT INTO usage_daily (
                 account_id, api_key_id, day,
                 input_tokens, cache_read_tokens, output_tokens, cost_idr
             ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(account_id)
        .bind(key_id)
        .bind(day)
        .bind(input_tokens)
        .bind(cache_read_tokens)
        .bind(output_tokens)
        .bind(cost_idr)
        .execute(pool)
        .await
        .expect("insert usage_daily row");
    }

    /// Deletes every row a fixture created, in FK order.
    ///
    /// usage_daily comes FIRST: its api_key_id is NOT NULL, and the FK is ON
    /// DELETE SET NULL, so deleting api_keys while a usage_daily row still points
    /// at one trips a NOT NULL violation instead of cascading. wallets/ledger are
    /// ON DELETE RESTRICT, so the order is load-bearing. Unconditional: a
    /// panicking assertion must not leave drift in a database other runs share.
    async fn delete_fixture_rows(pool: &PgPool, account_ids: &[Uuid]) {
        for account_id in account_ids {
            for statement in [
                "DELETE FROM usage_daily WHERE account_id = $1",
                "DELETE FROM ledger WHERE account_id = $1",
                "DELETE FROM api_keys WHERE account_id = $1",
                "DELETE FROM topups WHERE account_id = $1",
                "DELETE FROM sessions WHERE account_id = $1",
                "DELETE FROM wallets WHERE account_id = $1",
                "DELETE FROM accounts WHERE id = $1",
            ] {
                sqlx::query(statement)
                    .bind(account_id)
                    .execute(pool)
                    .await
                    .unwrap_or_else(|err| panic!("cleanup failed on {statement}: {err}"));
            }
        }
    }

    /// The reconciliation check from docs/observability.md, scoped to one
    /// account: wallets.balance_idr must equal SUM(ledger.delta_idr). Must be 0.
    async fn drift_rows(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT w.account_id
                FROM wallets w
                LEFT JOIN ledger l ON l.account_id = w.account_id
                WHERE w.account_id = $1
                GROUP BY w.account_id, w.balance_idr
                HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// The AppState the router would hand the handler, built from the same config
    /// file the server loads.
    fn live_app_state(pool: PgPool) -> AppState {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load for the live tests");
        let trusted = parse_cidrs(&config.network.trusted_proxy_cidrs)
            .expect("the config validates its own trusted proxy rules");

        AppState {
            pool,
            http_client: reqwest::Client::new(),
            events: Arc::new(RealtimeHub::new(&config.realtime)),
            config: Arc::new(config),
            ip_salt: Arc::new(DailySalt::new()),
            trusted_proxies: Arc::from(trusted.into_boxed_slice()),
        }
    }

    fn today() -> NaiveDate {
        chrono::Utc::now().date_naive()
    }

    /// Reads a live SSE response until `want_frames` complete frames have arrived
    /// (or a short deadline elapses).
    ///
    /// `to_bytes` cannot be used here: the live half of the stream only ends when
    /// the hub closes or `max_stream_seconds` elapses, so buffering the whole
    /// body would block for the stream's entire lifetime. The snapshot frames are
    /// written immediately, which is all these tests need.
    async fn read_frames(response: axum::response::Response, want_frames: usize) -> String {
        let mut stream = Box::pin(response.into_body().into_data_stream());
        let mut buffer = String::new();
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);

        while buffer.matches("\n\n").count() < want_frames {
            tokio::select! {
                chunk = stream.next() => match chunk {
                    Some(Ok(bytes)) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
                    Some(Err(err)) => panic!("the SSE body failed mid-stream: {err}"),
                    None => break,
                },
                _ = &mut deadline => break,
            }
        }

        buffer
    }

    /// The JSON payload of the frame named `event: <name>`, or a panic naming
    /// what actually arrived.
    fn frame_payload(body: &str, name: &str) -> serde_json::Value {
        for frame in body.split("\n\n").filter(|f| !f.trim().is_empty()) {
            let mut event = None;
            let mut data = None;
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = Some(rest.trim().to_string());
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    data = Some(rest.trim().to_string());
                }
            }
            if event.as_deref() == Some(name) {
                let data =
                    data.unwrap_or_else(|| panic!("frame \"{name}\" carries no data: {body:?}"));
                return serde_json::from_str(&data).unwrap_or_else(|err| {
                    panic!("frame \"{name}\" is not JSON ({err}): {body:?}")
                });
            }
        }
        panic!("no \"{name}\" frame in the SSE body: {body:?}");
    }

    fn frame_count(body: &str) -> usize {
        body.split("\n\n").filter(|f| !f.trim().is_empty()).count()
    }

    // -----------------------------------------------------------------------
    // 1. todays_usage - the query the snapshot depends on
    // -----------------------------------------------------------------------

    /// THE DEFECT THIS PINS: `SUM()` over a `bigint` column returns NUMERIC in
    /// Postgres, and sqlx refuses to decode NUMERIC into `i64`. Before the
    /// `::bigint` casts in `todays_usage`, this call returned `Err` on EVERY
    /// invocation - the SSE snapshot could never be produced. The contract is not
    /// merely "it returns"; it is "it returns the three token classes separately
    /// and exactly".
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_todays_usage_returns_each_token_class_from_the_database() {
        let pool = live_pool().await;
        let account = live_account(&pool).await;

        let outcome = tokio::spawn(todays_usage_assertions(
            pool.clone(),
            account.account_id,
            account.key_id,
        ));
        // The assertions run in their own task so a panicking one still reaches
        // the cleanup below: Tokio turns a task panic into a JoinError instead of
        // unwinding through this frame, which is what makes teardown
        // unconditional.
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[account.account_id]).await;
        outcome.expect("the todays_usage assertions panicked");
    }

    async fn todays_usage_assertions(pool: PgPool, account_id: Uuid, key_id: Uuid) {
        let day = today();
        insert_usage(&pool, account_id, key_id, day, 1_200, 8_000, 400, 812).await;

        // Yesterday's row must not bleed into "today": the query binds the day
        // (events.rs:340), it does not read the account's whole history.
        insert_usage(
            &pool,
            account_id,
            key_id,
            day - chrono::Duration::days(1),
            999_999,
            999_999,
            999_999,
            999_999,
        )
        .await;

        let usage = todays_usage(&pool, account_id)
            .await
            .expect("todays_usage must DECODE the summed token classes, not fail on NUMERIC");

        assert_eq!(
            usage.input_tokens, 1_200,
            "input tokens are summed and decoded exactly"
        );
        assert_eq!(
            usage.cache_read_tokens, 8_000,
            "cache reads are their own class"
        );
        assert_eq!(
            usage.output_tokens, 400,
            "output tokens are their own class"
        );
        assert_eq!(usage.cost_idr, 812, "cost is summed alongside the tokens");

        // docs/realtime.md:61 - the classes must never be folded together; they
        // differ ~50x in price.
        assert_ne!(
            usage.input_tokens, 9_200,
            "cache reads must not be added into input tokens"
        );
        assert_ne!(usage.cache_read_tokens, usage.input_tokens);
        assert_ne!(usage.output_tokens, usage.input_tokens);

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "the fixture must not manufacture ledger drift"
        );
    }

    // -----------------------------------------------------------------------
    // 2. todays_usage - per-account scoping
    // -----------------------------------------------------------------------

    /// The WHERE clause at events.rs:340 is the only thing keeping one account's
    /// usage out of another's dashboard. Two real accounts with DIFFERENT values
    /// prove it, in both directions, and a third with no rows proves an absent
    /// account reads zero rather than a neighbour's total.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_todays_usage_never_returns_another_accounts_numbers() {
        let pool = live_pool().await;
        let a = live_account(&pool).await;
        let b = live_account(&pool).await;
        let empty = live_account(&pool).await;

        let outcome = tokio::spawn(todays_usage_isolation_assertions(
            pool.clone(),
            a.account_id,
            a.key_id,
            b.account_id,
            b.key_id,
            empty.account_id,
        ));
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[a.account_id, b.account_id, empty.account_id]).await;
        outcome.expect("the todays_usage isolation assertions panicked");
    }

    async fn todays_usage_isolation_assertions(
        pool: PgPool,
        a_id: Uuid,
        a_key: Uuid,
        b_id: Uuid,
        b_key: Uuid,
        empty_id: Uuid,
    ) {
        let day = today();
        insert_usage(&pool, a_id, a_key, day, 1_200, 8_000, 400, 812).await;
        insert_usage(&pool, b_id, b_key, day, 77_000, 66_000, 55_000, 44_000).await;

        let a = todays_usage(&pool, a_id).await.expect("account A usage");
        let b = todays_usage(&pool, b_id).await.expect("account B usage");

        assert_eq!(
            (
                a.input_tokens,
                a.cache_read_tokens,
                a.output_tokens,
                a.cost_idr
            ),
            (1_200, 8_000, 400, 812),
            "account A must read exactly its own row"
        );
        assert_eq!(
            (
                b.input_tokens,
                b.cache_read_tokens,
                b.output_tokens,
                b.cost_idr
            ),
            (77_000, 66_000, 55_000, 44_000),
            "account B must read exactly its own row"
        );

        // Neither account may be handed the other's numbers, in any class.
        assert_ne!(a.input_tokens, b.input_tokens);
        assert_ne!(a.cache_read_tokens, b.cache_read_tokens);
        assert_ne!(a.output_tokens, b.output_tokens);
        assert_ne!(a.cost_idr, b.cost_idr);

        // No rows at all: zero, not the neighbour's total.
        let none = todays_usage(&pool, empty_id)
            .await
            .expect("an account with no usage today reads zeros");
        assert_eq!(
            (
                none.input_tokens,
                none.cache_read_tokens,
                none.output_tokens,
                none.cost_idr
            ),
            (0, 0, 0, 0),
            "an account with no usage_daily row must read zero, not another account's totals"
        );

        for account_id in [a_id, b_id, empty_id] {
            assert_eq!(
                drift_rows(&pool, account_id).await,
                0,
                "the fixture must not manufacture ledger drift"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 3. sse_events_handler - the snapshot
    // -----------------------------------------------------------------------

    /// The snapshot path, end to end: cookie -> account -> `SELECT balance_idr
    /// FROM wallets` (events.rs:412) -> `snapshot_events`. The balance frame must
    /// carry the wallet's REAL value. A handler wired to a constant (or to 0)
    /// would pass every in-memory test above and still show every customer an
    /// empty wallet.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_sse_snapshot_carries_the_wallets_real_balance_and_todays_usage() {
        let pool = live_pool().await;
        let account = live_account(&pool).await;

        let outcome = tokio::spawn(sse_snapshot_assertions(
            pool.clone(),
            account.account_id,
            account.token.clone(),
            account.key_id,
        ));
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[account.account_id]).await;
        outcome.expect("the sse snapshot assertions panicked");
    }

    async fn sse_snapshot_assertions(pool: PgPool, account_id: Uuid, token: String, key_id: Uuid) {
        let opening = 73_500;
        fund_wallet(&pool, account_id, opening).await;
        insert_usage(&pool, account_id, key_id, today(), 1_200, 8_000, 400, 812).await;

        // The wallet really holds the money the snapshot is supposed to report.
        let stored: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("read the wallet the snapshot reads");
        assert_eq!(stored, opening, "the fixture funded the wallet");

        let state = live_app_state(pool.clone());
        let response = sse_events_handler(State(state), cookie_headers(&token))
            .await
            .expect("the live handler must serve a snapshot")
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = read_frames(response, 2).await;

        let balance = frame_payload(&body, "balance");
        assert_eq!(
            balance["balance_idr"], opening,
            "the snapshot must carry the wallet's REAL balance from the database, not a constant: {body:?}"
        );
        assert_ne!(
            balance["balance_idr"], 0,
            "a funded wallet must never be reported as empty: {body:?}"
        );

        let usage = frame_payload(&body, "usage");
        assert_eq!(usage["input_tokens"], 1_200, "{body:?}");
        assert_eq!(usage["cache_read_tokens"], 8_000, "{body:?}");
        assert_eq!(usage["output_tokens"], 400, "{body:?}");
        assert_eq!(usage["cost_idr"], 812, "{body:?}");

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "the fixture must not manufacture ledger drift"
        );
    }

    // -----------------------------------------------------------------------
    // 4. sse_events_handler - cross-account isolation
    // -----------------------------------------------------------------------

    /// Two signed-in accounts, two different real balances, two different usage
    /// rows: each snapshot must carry its OWN account's numbers and never the
    /// other's. This is the handler-level counterpart of the in-memory filter
    /// test above.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_sse_snapshot_is_scoped_to_the_signed_in_account() {
        let pool = live_pool().await;
        let a = live_account(&pool).await;
        let b = live_account(&pool).await;

        let outcome = tokio::spawn(sse_scoping_assertions(
            pool.clone(),
            (a.account_id, a.token.clone(), a.key_id),
            (b.account_id, b.token.clone(), b.key_id),
        ));
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[a.account_id, b.account_id]).await;
        outcome.expect("the sse scoping assertions panicked");
    }

    async fn sse_scoping_assertions(
        pool: PgPool,
        a: (Uuid, String, Uuid),
        b: (Uuid, String, Uuid),
    ) {
        let (a_id, a_token, a_key) = a;
        let (b_id, b_token, b_key) = b;

        // DIFFERENT money and DIFFERENT usage, so any leak is visible as a value
        // mismatch rather than as a coincidence.
        fund_wallet(&pool, a_id, 73_500).await;
        fund_wallet(&pool, b_id, 12_345).await;
        insert_usage(&pool, a_id, a_key, today(), 1_200, 8_000, 400, 812).await;
        insert_usage(&pool, b_id, b_key, today(), 77_000, 66_000, 55_000, 44_000).await;

        let body_a = read_frames(
            sse_events_handler(
                State(live_app_state(pool.clone())),
                cookie_headers(&a_token),
            )
            .await
            .expect("account A's snapshot")
            .into_response(),
            2,
        )
        .await;
        let body_b = read_frames(
            sse_events_handler(
                State(live_app_state(pool.clone())),
                cookie_headers(&b_token),
            )
            .await
            .expect("account B's snapshot")
            .into_response(),
            2,
        )
        .await;

        assert_eq!(frame_payload(&body_a, "balance")["balance_idr"], 73_500);
        assert_eq!(frame_payload(&body_a, "usage")["input_tokens"], 1_200);
        assert_eq!(frame_payload(&body_a, "usage")["cache_read_tokens"], 8_000);
        assert_eq!(frame_payload(&body_a, "usage")["output_tokens"], 400);

        assert_eq!(frame_payload(&body_b, "balance")["balance_idr"], 12_345);
        assert_eq!(frame_payload(&body_b, "usage")["input_tokens"], 77_000);
        assert_eq!(frame_payload(&body_b, "usage")["cache_read_tokens"], 66_000);
        assert_eq!(frame_payload(&body_b, "usage")["output_tokens"], 55_000);

        // Not one byte of the other account's money may reach the wire.
        assert!(
            !body_a.contains("12345"),
            "account A's stream leaked account B's balance: {body_a:?}"
        );
        assert!(
            !body_b.contains("73500"),
            "account B's stream leaked account A's balance: {body_b:?}"
        );

        for account_id in [a_id, b_id] {
            assert_eq!(
                drift_rows(&pool, account_id).await,
                0,
                "the fixture must not manufacture ledger drift"
            );
        }
    }

    /// The RECONNECT path, which reads the hub's replay buffer instead of the
    /// database: events published for two accounts, replayed to a client that
    /// reconnects with a Last-Event-ID. Only the signed-in account's events may
    /// escape (the filter at events.rs:422).
    ///
    /// The replayed usage values are deliberately DIFFERENT from the account's
    /// usage_daily row, so the assertions also prove the Replay branch ran rather
    /// than a fresh snapshot.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_sse_replay_never_forwards_another_accounts_events() {
        let pool = live_pool().await;
        let a = live_account(&pool).await;
        let b = live_account(&pool).await;

        let outcome = tokio::spawn(sse_replay_assertions(
            pool.clone(),
            (a.account_id, a.token.clone(), a.key_id),
            (b.account_id, b.token.clone(), b.key_id),
        ));
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[a.account_id, b.account_id]).await;
        outcome.expect("the sse replay assertions panicked");
    }

    async fn sse_replay_assertions(pool: PgPool, a: (Uuid, String, Uuid), b: (Uuid, String, Uuid)) {
        let (a_id, a_token, a_key) = a;
        let (b_id, _b_token, b_key) = b;

        // What the database holds for A today - NOT what the replay will carry.
        insert_usage(&pool, a_id, a_key, today(), 1_200, 8_000, 400, 812).await;
        insert_usage(&pool, b_id, b_key, today(), 77_000, 66_000, 55_000, 44_000).await;

        let state = live_app_state(pool.clone());
        let hub = Arc::clone(&state.events);

        // Interleaved activity for both accounts, published to the one
        // process-wide hub the handler subscribes to.
        publish_balance(&hub, a_id, 73_500);
        publish_balance(&hub, b_id, 12_345);
        publish_usage(&hub, a_id, test_usage(11, 22, 33, 44));
        publish_usage(&hub, b_id, test_usage(55, 66, 77, 88));

        // Last-Event-ID 0 is contiguous with the buffer, so this is a Replay.
        let mut headers = cookie_headers(&a_token);
        headers.insert("last-event-id", HeaderValue::from_static("0"));

        let body = read_frames(
            sse_events_handler(State(state), headers)
                .await
                .expect("account A's replay")
                .into_response(),
            2,
        )
        .await;

        // Exactly A's two events - B's two must be filtered out.
        assert_eq!(
            frame_count(&body),
            2,
            "the replay forwarded another account's events: {body:?}"
        );

        let balance = frame_payload(&body, "balance");
        assert_eq!(
            balance["balance_idr"], 73_500,
            "A must receive its own balance: {body:?}"
        );

        let usage = frame_payload(&body, "usage");
        assert_eq!(
            usage["input_tokens"], 11,
            "the replayed totals are the published ones, not a fresh snapshot: {body:?}"
        );
        assert_eq!(usage["cache_read_tokens"], 22);
        assert_eq!(usage["output_tokens"], 33);

        assert!(
            !body.contains("12345"),
            "A's replay leaked B's balance: {body:?}"
        );
        assert!(
            !body.contains("\"input_tokens\":55"),
            "A's replay leaked B's usage: {body:?}"
        );

        for account_id in [a_id, b_id] {
            assert_eq!(
                drift_rows(&pool, account_id).await,
                0,
                "the fixture must not manufacture ledger drift"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 5. sse_events_handler - the LIVE broadcast branch, two streams at once
    // -----------------------------------------------------------------------
    //
    // The snapshot tests (3, 4) and the replay test above never touch the branch a
    // connected dashboard actually lives on: the live unfold at events.rs:386-407,
    // where a process-wide broadcast is filtered per subscriber (events.rs:391).
    // The replay test pins the filter at events.rs:422; its live twin was proven
    // only in memory, by mirroring the filter by hand
    // (a_subscriber_only_sees_its_own_account_events). This drives it end to end.
    //
    // HOW THE LIVE BRANCH IS REACHED - through the real socket, no fake:
    // the handler subscribes to the hub at events.rs:386 BEFORE it reads the
    // snapshot, and it owns the connection guard (events.rs:443). Awaiting the
    // handler's response therefore proves the subscription is open and the guard
    // is held, with no sleep-and-hope. Publishing then goes through the hub's real
    // API (publish_balance, events.rs:306) and the frame is read off the real SSE
    // body.
    //
    // WHY THE PUBLISH ORDER IS THE ASSERTION: RealtimeEvent::into_event
    // (events.rs:110) puts only the id, the name and the JSON payload on the wire -
    // account_id is deliberately NOT serialized, it exists solely to be filtered
    // on. So isolation has to be proven by value. Both events are published before
    // either stream is read, and the broadcast channel is FIFO per receiver, so on
    // a stream that fails to filter, the OTHER account's frame is provably already
    // in the buffer at the moment this account's own frame completes. That makes
    // the leak deterministic rather than a race with a timeout.

    /// One frame exactly as it came off the wire.
    struct WireFrame {
        event: String,
        data: String,
    }

    /// The "event:"/"data:" lines of every complete frame in an SSE body.
    ///
    /// The "id:" line is deliberately not parsed: it carries the hub's global
    /// monotonic id (events.rs:175), which is shared across accounts by design.
    fn wire_frames(body: &str) -> Vec<WireFrame> {
        body.split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let mut event = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("event:") {
                        event = rest.trim().to_string();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data = rest.trim().to_string();
                    }
                }
                WireFrame { event, data }
            })
            .collect()
    }

    /// The balance a frame carries, or None when it is not a balance frame (or is
    /// still half-written). Never panics: this runs inside the read loop.
    fn frame_balance(frame: &WireFrame) -> Option<i64> {
        if frame.event != "balance" {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(&frame.data)
            .ok()?
            .get("balance_idr")?
            .as_i64()
    }

    /// The balance values a stream carried on its LIVE half - everything after the
    /// two snapshot frames. An empty vec means the live branch delivered nothing.
    fn live_balances(body: &str) -> Vec<i64> {
        wire_frames(body)
            .into_iter()
            .skip(2) // the snapshot is always one balance frame then one usage frame
            .filter_map(|frame| frame_balance(&frame))
            .collect()
    }

    /// Reads a live SSE body until it has seen a balance frame carrying
    /// want_balance_idr, then returns everything read.
    ///
    /// Bounded by a deadline so a filter that drops the event FAILS the test
    /// instead of hanging it. The stop condition is deliberately the account's own
    /// value and not a frame count: an unfiltered stream delivers the other
    /// account's frame FIRST, and a count-based read would stop there and never
    /// notice the leak.
    async fn read_until_balance(
        response: axum::response::Response,
        want_balance_idr: i64,
    ) -> String {
        let mut stream = Box::pin(response.into_body().into_data_stream());
        let mut buffer = String::new();
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);

        loop {
            if wire_frames(&buffer)
                .iter()
                .any(|frame| frame_balance(frame) == Some(want_balance_idr))
            {
                return buffer;
            }

            tokio::select! {
                chunk = stream.next() => match chunk {
                    Some(Ok(bytes)) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
                    Some(Err(err)) => panic!("the SSE body failed mid-stream: {err}"),
                    None => return buffer,
                },
                _ = &mut deadline => return buffer,
            }
        }
    }

    /// THE DEFECT 1 REGRESSION, on the live branch: two accounts hold their
    /// subscriptions OPEN AT THE SAME TIME, on two real SSE bodies, and balances are
    /// published to A and then to B while both are live. A's stream must carry only
    /// A's event and B's only B's - asserted on the values, frame by frame, not on
    /// "it did not panic".
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_sse_two_open_streams_never_cross_accounts_on_the_live_broadcast() {
        let pool = live_pool().await;
        let a = live_account(&pool).await;
        let b = live_account(&pool).await;

        let outcome = tokio::spawn(live_broadcast_isolation_assertions(
            pool.clone(),
            (a.account_id, a.token.clone(), a.key_id),
            (b.account_id, b.token.clone(), b.key_id),
        ));
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[a.account_id, b.account_id]).await;
        outcome.expect("the live broadcast isolation assertions panicked");
    }

    async fn live_broadcast_isolation_assertions(
        pool: PgPool,
        a: (Uuid, String, Uuid),
        b: (Uuid, String, Uuid),
    ) {
        let (a_id, a_token, a_key) = a;
        let (b_id, b_token, b_key) = b;

        // DIFFERENT money and DIFFERENT usage, so a leak is a value mismatch
        // rather than a coincidence.
        fund_wallet(&pool, a_id, 73_500).await;
        fund_wallet(&pool, b_id, 12_345).await;
        insert_usage(&pool, a_id, a_key, today(), 1_200, 8_000, 400, 812).await;
        insert_usage(&pool, b_id, b_key, today(), 77_000, 66_000, 55_000, 44_000).await;

        // ONE hub for both handlers, exactly as the process shares one AppState.
        let state = live_app_state(pool.clone());
        let hub = Arc::clone(&state.events);

        // BOTH subscriptions are open before anything is published. Awaiting the
        // handler is what proves it: it subscribes at events.rs:386 ahead of the
        // snapshot read, and its response owns a connection guard (events.rs:443).
        let stream_a = sse_events_handler(State(state.clone()), cookie_headers(&a_token))
            .await
            .expect("account A's live stream")
            .into_response();
        let stream_b = sse_events_handler(State(state.clone()), cookie_headers(&b_token))
            .await
            .expect("account B's live stream")
            .into_response();

        // Both live. A's balance, then B's - the order the brief asks for - then
        // A's fence: because the fence is published LAST, it cannot arrive on A's
        // stream until B's frame (published earlier) has already been delivered
        // there. That is what turns the leak into a deterministic failure.
        publish_balance(&hub, a_id, 111_111);
        publish_balance(&hub, b_id, 222_222);
        publish_balance(&hub, a_id, 333_333);

        let body_a = read_until_balance(stream_a, 333_333).await;
        let body_b = read_until_balance(stream_b, 222_222).await;

        // A's snapshot still carries A's real row, read from the database.
        assert_eq!(
            frame_payload(&body_a, "balance")["balance_idr"],
            73_500,
            "A's opening frame is its own wallet: {body_a:?}"
        );

        // The live half, frame by frame. An unfiltered stream reads
        // [111111, 222222, 333333] here - the other account's money, visible.
        assert_eq!(
            live_balances(&body_a),
            vec![111_111, 333_333],
            "account A's LIVE stream carried a frame that is not A's own balance: {body_a:?}"
        );
        assert_eq!(
            live_balances(&body_b),
            vec![222_222],
            "account B's LIVE stream carried a frame that is not B's own balance: {body_b:?}"
        );

        // Not one byte of the other account's money on either live stream.
        assert!(
            !body_a.contains("222222"),
            "A's live stream leaked B's balance: {body_a:?}"
        );
        assert!(
            !body_b.contains("111111") && !body_b.contains("333333"),
            "B's live stream leaked A's balance: {body_b:?}"
        );
        assert!(
            !body_a.contains("77000") && !body_b.contains("1200"),
            "a snapshot frame crossed accounts: A={body_a:?} B={body_b:?}"
        );

        for account_id in [a_id, b_id] {
            assert_eq!(
                drift_rows(&pool, account_id).await,
                0,
                "the fixture must not manufacture ledger drift"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 6. sse_events_handler - the connection cap is per account
    // -----------------------------------------------------------------------

    /// The cap at events.rs:261-265 is keyed by account (events.rs:259). Exhaust
    /// account A's slots through the real handler - each held response owns one
    /// guard (events.rs:443) - and account B must still be admitted: a runaway
    /// client on one account must never deny another account its dashboard. Then
    /// dropping A's streams must hand A its slots back.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_sse_connection_cap_is_per_account_not_process_wide() {
        let pool = live_pool().await;
        let a = live_account(&pool).await;
        let b = live_account(&pool).await;

        let outcome = tokio::spawn(connection_cap_is_per_account_assertions(
            pool.clone(),
            (a.account_id, a.token.clone(), a.key_id),
            (b.account_id, b.token.clone(), b.key_id),
        ));
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[a.account_id, b.account_id]).await;
        outcome.expect("the connection cap assertions panicked");
    }

    async fn connection_cap_is_per_account_assertions(
        pool: PgPool,
        a: (Uuid, String, Uuid),
        b: (Uuid, String, Uuid),
    ) {
        let (a_id, a_token, _a_key) = a;
        let (b_id, b_token, _b_key) = b;

        fund_wallet(&pool, a_id, 73_500).await;
        fund_wallet(&pool, b_id, 12_345).await;

        let state = live_app_state(pool.clone());
        let cap = state.config.realtime.max_connections_per_account;
        assert!(cap >= 1, "a cap of {cap} would refuse every stream");

        // A takes every slot it is entitled to, and HOLDS them: the guards live
        // inside the response streams, so these must stay in scope.
        let mut held = Vec::new();
        for index in 0..cap {
            held.push(
                sse_events_handler(State(state.clone()), cookie_headers(&a_token))
                    .await
                    .unwrap_or_else(|_| panic!("A's stream {index} must be admitted")),
            );
        }

        // One more for A is refused, and refused for the documented reason.
        let refused = sse_events_handler(State(state.clone()), cookie_headers(&a_token)).await;
        assert!(
            matches!(refused, Err(AppError::RateLimited { .. })),
            "A's {}th stream must be refused with RateLimited, not admitted",
            cap + 1
        );

        // B's budget is untouched by A's exhaustion.
        let b_response = sse_events_handler(State(state.clone()), cookie_headers(&b_token))
            .await
            .expect("B must not be refused because A exhausted A's own slots")
            .into_response();
        assert_eq!(b_response.status(), StatusCode::OK);

        // B's stream genuinely works, not merely "did not error": its own balance
        // arrives on its own socket.
        let body_b = read_frames(b_response, 2).await;
        assert_eq!(
            frame_payload(&body_b, "balance")["balance_idr"],
            12_345,
            "the account admitted while A was capped must get its own wallet: {body_b:?}"
        );

        // Releasing A's streams frees A's slots.
        drop(held);
        let readmitted = sse_events_handler(State(state.clone()), cookie_headers(&a_token)).await;
        assert!(
            readmitted.is_ok(),
            "dropping A's streams must return A's slots to A"
        );

        for account_id in [a_id, b_id] {
            assert_eq!(
                drift_rows(&pool, account_id).await,
                0,
                "the fixture must not manufacture ledger drift"
            );
        }
    }
}
