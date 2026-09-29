//! Distinct-IP counting per API key, without ever storing an address.
//!
//! Specified by [`docs/ip-tracking.md`](../../docs/ip-tracking.md) and required by
//! the abuse runbook, which lists "many distinct IPs on one key" as the signal for
//! key sharing and resale. The design constraint is the privacy promise
//! ([`docs/data-retention.md`](../../docs/data-retention.md), ToS §5): **no raw IP
//! is stored anywhere**. What is stored is an HMAC of the address under a salt
//! that changes every day and is never written down.
//!
//! WHY THE SALT IS PROCESS-LOCAL AND NEVER PERSISTED. The doc is explicit that
//! "deleting the salt is what makes this honest" — it is what turns a reversible
//! hash of a small address space into an unlinkable one. A salt derived from a
//! stored server secret plus the date would look equivalent in testing (it is
//! stable within a day and different across days) but would let anyone holding
//! the secret recompute last week's salt and brute-force the IPv4 space. So the
//! salt lives only in memory, is drawn from the OS RNG, and is replaced at the
//! UTC day boundary; yesterday's hashes become permanently unlinkable.
//!
//! THE COST OF THAT CHOICE: a restart mints a new salt, so hashes computed
//! before it cannot be correlated with hashes after it — one day can therefore
//! over-count a key's distinct IPs. That is the correct direction to be wrong
//! in: over-counting makes a sharing signal fire on a legitimate mobile user
//! (which the doc already warns is the biggest false-positive source, since
//! Indonesian carriers rotate addresses aggressively), while under-counting
//! would hide the abuse this exists to find. Over-counting is a human
//! investigating; under-counting is nobody investigating.
//!
//! SUSPICION, NOT ENFORCEMENT. The doc draws a hard line between the two:
//! a suspicion threshold is "flagged for a human to look at", a hard cap
//! (`config/apikita.toml [limits]`) "refuses the request". Nothing here refuses
//! anything — crossing a threshold logs, and a human decides.
//!
//! SINGLE-INSTANCE PRECONDITION. This design assumes ONE server process, and
//! every counter below depends on it. The daily salt is drawn from this
//! process's OS RNG and is never shared or persisted, so two instances hold two
//! independent salts and the same caller hashes to two different values.
//! `distinct_ips` is therefore per instance, not per key per day: the rows are
//! keyed `(api_key_id, day)` and a second instance upserts into the SAME row with
//! its own independent count, so the stored figure is the SUM of the instances'
//! counts rather than the true distinct set.
//!
//! What degrades when more than one instance runs is the abuse SIGNAL, not the
//! privacy promise. The hashes stay salted and unlinkable; but a shared account
//! is counted separately by each instance, so counts can exceed the real
//! distinct set and the `SHARING_SUSPICION_IPS` warning can fire in more than
//! one process for the same key on the same day — a split signal and duplicate
//! alerts. That is the safe direction to be wrong in (a human investigates a
//! false positive, rather than nobody investigating), but it is wrong all the
//! same. This matches the deployment [`docs/cost-and-sizing.md`](../../docs/cost-and-sizing.md)
//! describes — "two instances, two deploys, no benefit at this scale" — and it
//! is stated here because it was previously implied everywhere and asserted
//! nowhere in this module.
//!
//! Making the counter correct under more than one instance would mean moving the
//! distinct set into the database and sharing the salt. That is a larger design
//! change than this module's remit and it is deliberately NOT done here; the
//! assumption is recorded so it is a known constraint rather than a latent
//! surprise.

#![cfg_attr(
    not(test),
    // THIS MODULE IS FENCED FOR A DIFFERENT REASON than the money ones, and the
    // reason is worth being precise about rather than folding into the same list.
    //
    // Nothing here computes a figure a customer is charged. What it computes is the
    // TRUST BOUNDARY: the CIDR mask decides whose X-Forwarded-For is believed, and a
    // peer that is believed chooses the address it is recorded as - which is the
    // exact signal docs/ip-tracking.md exists to raise.
    //
    // Two of the four sites are SHIFTS (`32 - prefix`, `128 - prefix`), and a shift
    // whose amount is computed rather than literal is where a mask silently becomes
    // the wrong mask. A mask that is too wide trusts more hosts than the operator
    // wrote; a mask that is too narrow trusts fewer. Both are security outcomes, and
    // neither raises anything.
    //
    // Like account, admin and money, this one cost nothing to install: four sites,
    // all bounded by construction, each argued below. Unlike those, the argument
    // for the two shifts is CROSS-FUNCTION - it depends on IpCidr::parse rejecting a
    // prefix past the family width - which is exactly the kind of thing that rots
    // when the check and the use live far apart, so both are written down here.
    deny(clippy::arithmetic_side_effects)
)]

use chrono::{NaiveDate, Utc};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use sqlx::{Row, SqlitePool};
use std::sync::RwLock;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::error::AppError;

/// `key_ip_seen` hashes are kept this long — enough to investigate a live
/// incident, and the window the privacy statement promises.
pub const SEEN_RETENTION_DAYS: i64 = 7;

/// `key_ip_daily` counts are kept this long: trend without history.
pub const DAILY_RETENTION_DAYS: i64 = 90;

/// Distinct IPs on one key in one day above which the runbook says to look.
///
/// A starting value from [`docs/ip-tracking.md`](../../docs/ip-tracking.md)
/// §Abuse signals, where it is stated as ">20 domestic-distinct, sustained" and
/// flagged as needing tuning against real traffic. It is a SUSPICION threshold,
/// so crossing it logs and nothing else — a mobile user can legitimately sit
/// above it (the doc puts a mobile user at 10-50 IPs/day).
pub const SHARING_SUSPICION_IPS: i32 = 20;

/// The counters for one key on one day, as they stand after a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyIpCounts {
    pub distinct_ips: i32,
    pub request_count: i64,
}

/// The daily salt: 32 bytes from the OS RNG, holding one UTC day.
///
/// Held behind a lock because the proxy records from many concurrent requests
/// and the rotation has to be visible to all of them at once. `RwLock` rather
/// than `Mutex` because rotation is rare (once a day) and reads are per-request.
pub struct DailySalt {
    state: RwLock<SaltState>,
}

struct SaltState {
    day: NaiveDate,
    bytes: [u8; 32],
}

impl DailySalt {
    /// A fresh salt for the current UTC day.
    pub fn new() -> Self {
        Self::seeded(today_utc(), fresh_salt())
    }

    /// A salt for a chosen day, with chosen bytes. Tests use this to pin the
    /// rotation; `new` is what the application uses.
    pub fn seeded(day: NaiveDate, bytes: [u8; 32]) -> Self {
        Self {
            state: RwLock::new(SaltState { day, bytes }),
        }
    }

    /// The salt for `day`, rotating if the held salt belongs to an older one.
    ///
    /// The rotation DISCARDS the previous bytes rather than keeping them: that
    /// discard is the whole privacy mechanism, so there is deliberately no
    /// accessor for a past day's salt. A caller asking for an older day gets
    /// today's salt, which is the safe failure — it makes those hashes
    /// unlinkable rather than recoverable.
    pub fn salt_for_day(&self, day: NaiveDate) -> [u8; 32] {
        // Rotate under the write lock only when the day actually moved.
        {
            // A poisoned lock is recovered, not propagated: the poisoning thread
            // was the one panicking, and this lock is on the path of every
            // proxied request — refusing it would turn one transient panic into
            // a permanent one. Same convention as the rest of the server.
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            if state.day == day {
                return state.bytes;
            }
        }

        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        // Re-check under the write lock: another request may have rotated while
        // this one waited, and rotating twice would drop the salt those
        // in-flight requests are about to use.
        if state.day == day {
            return state.bytes;
        }
        state.day = day;
        state.bytes = fresh_salt();
        state.bytes
    }
}

impl Default for DailySalt {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DailySalt {
    /// Never prints the bytes: a salt in a log line is a salt on disk.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let day = self.state.read().map(|s| s.day).ok();
        f.debug_struct("DailySalt")
            .field("day", &day)
            .field("bytes", &"<redacted>")
            .finish()
    }
}

/// The current UTC day. One definition, because the salt boundary and the
/// `key_ip_daily.day` column must agree or the counts split across two rows.
///
/// **AND NOW IT REALLY IS ONE, which it was not for most of this crate's life.**
/// The comment above made a promise that eighteen call sites did not keep: they
/// wrote `Utc::now().date_naive()` themselves. The value was identical - this
/// function is that exact expression - so nothing was wrong TODAY, and that is
/// precisely what made it dangerous. A skew correction, a timezone, or a
/// documented-but-unapplied offset added here would have moved the salt boundary
/// and left every one of those sites on the old definition, which is the split the
/// comment warns about, arriving by the most boring route available.
///
/// The same day also decides the usage and spend rollups, so a split here does not
/// just split IP counts: a customer's 30-day spend would be windowed differently
/// from the spend shown to them. Every one of those sites now calls this.
pub fn today_utc() -> NaiveDate {
    Utc::now().date_naive()
}

fn fresh_salt() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// HMAC-SHA256 of the address under the daily salt, hex encoded.
///
/// **THE ARGUMENT ORDER IS THE DESIGN.** The salt is the HMAC KEY and the
/// address is the MESSAGE — exactly as [`docs/ip-tracking.md`](../../docs/ip-tracking.md)
/// states it, and called out there as "easy to reverse". Swapping them still
/// yields a stable, well-formed hash of the right length, so no test of the
/// output shape would catch it; the salt would simply stop being secret, and
/// the hash would become brute-forceable over the IPv4 space. `ip_hash_arguments_are_not_interchangeable`
/// below asserts the two orders differ, which is the only thing that can catch
/// it, and this is the only function in the codebase that hashes an address.
///
/// The address is hashed in its canonical string form, so `::ffff:1.2.3.4` and
/// `1.2.3.4` are distinct hashes. That is fine: a key is not used through a
/// v4-mapped and a native v4 path in the same day often enough to matter, and
/// splitting them over-counts (the safe direction) rather than hiding sharing.
pub fn ip_hash(salt: &[u8], ip: &std::net::IpAddr) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(salt).expect("HMAC accepts any key length");
    mac.update(ip.to_string().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Records one request from `ip` against `key_id` for `day`.
///
/// The Postgres original did this in ONE statement with a data-modifying CTE.
/// SQLite has no such construct — its `WITH` clause accepts only `SELECT` in a
/// CTE, and the original is rejected with `near "INSERT": syntax error`
/// (measured) — so it becomes two statements inside one `BEGIN IMMEDIATE`.
///
/// The property that must survive is the reason the CTE existed: the insert into
/// `key_ip_seen` reports whether this hash is new for the day, and that ONE fact
/// drives whether `distinct_ips` moves. Deriving it from a separate `COUNT(*)`
/// read would race — two first-seen IPs in flight at once could both read the
/// pre-insert count and settle on the same value. Here the fact comes from
/// `rows_affected()` on the insert itself (measured: 1 for a new pair, 0 when
/// `ON CONFLICT DO NOTHING` fires), read inside the same transaction that holds
/// the write lock, so the two tables still cannot disagree. Measured on the
/// sequence h1, h1, h2: `distinct_ips` goes 1, 1, 2 while `request_count` goes
/// 1, 2, 3.
///
/// Returns the counts as they stand AFTER this request.
pub async fn record_key_ip(
    pool: &SqlitePool,
    key_id: Uuid,
    day: NaiveDate,
    ip_hash: &str,
) -> Result<KeyIpCounts, AppError> {
    let mut tx = crate::db::begin_immediate(pool).await?;

    // 1. Was this (key, day, ip_hash) already seen today? `ON CONFLICT DO
    //    NOTHING` makes the insert itself answer, and `rows_affected()` is the
    //    1 or 0. The conflict target is `key_ip_seen`'s primary key, all three
    //    columns NOT NULL; it is spelled out rather than left bare so a future
    //    second unique index cannot silently capture this insert.
    let inserted = sqlx::query(
        "INSERT INTO key_ip_seen (api_key_id, day, ip_hash)
         VALUES (?, ?, ?)
         ON CONFLICT (api_key_id, day, ip_hash) DO NOTHING",
    )
    .bind(key_id.hyphenated())
    .bind(day)
    .bind(ip_hash)
    .execute(&mut *tx)
    .await?;

    let new_ip = inserted.rows_affected() as i64;

    // 2. Always move `request_count`; move `distinct_ips` only when step 1
    //    actually inserted. `key_ip_daily`'s key is the composite primary key
    //    (api_key_id, day), both NOT NULL, so the plain column-list conflict
    //    target is correct here — unlike `usage_daily`, whose key is an
    //    expression index and cannot be named by columns (plan section 4.3,
    //    trap 3). The fourth `?` is the one inside the `DO UPDATE` clause.
    let row = sqlx::query(
        "INSERT INTO key_ip_daily (api_key_id, day, distinct_ips, request_count)
         VALUES (?, ?, ?, 1)
         ON CONFLICT (api_key_id, day) DO UPDATE
             SET request_count = key_ip_daily.request_count + 1,
                 distinct_ips  = key_ip_daily.distinct_ips + ?
         RETURNING distinct_ips, request_count",
    )
    .bind(key_id.hyphenated())
    .bind(day)
    .bind(new_ip)
    .bind(new_ip)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    let counts = KeyIpCounts {
        distinct_ips: row.get("distinct_ips"),
        request_count: row.get("request_count"),
    };

    if counts.distinct_ips == SHARING_SUSPICION_IPS + 1 {
        // Logged once, at the crossing. A suspicion threshold is a flag for a
        // human, not a refusal — see the module note.
        warn!(
            key_id = %key_id,
            distinct_ips = counts.distinct_ips,
            "Key exceeded the distinct-IP sharing threshold; review before acting"
        );
    } else {
        debug!(
            key_id = %key_id,
            distinct_ips = counts.distinct_ips,
            request_count = counts.request_count,
            "Recorded request source"
        );
    }

    Ok(counts)
}

/// What a retention purge removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgedRows {
    pub seen: u64,
    pub daily: u64,
    /// `link_redemption_attempts` rows deleted.
    ///
    /// Counted and reported separately rather than folded into `seen`: the two
    /// answer different questions ("which keys were seen from where" vs "who was
    /// guessing at link codes"), and an operator reading the sweep log needs to
    /// know the link-code table is actually being swept. A count that cannot be
    /// printed cannot be noticed when it silently stops moving.
    pub link_attempts: u64,
    /// `link_code_issues` rows deleted.
    ///
    /// A third field for the same reason as the second: this table was added because
    /// the issuance cap could not count anything, and the counter it counts has to
    /// be swept on a policy someone can see. A count that cannot be printed cannot
    /// be noticed when it silently stops moving.
    pub link_issues: u64,
}

/// Deletes rows past their retention window.
///
/// This is what makes the privacy promise true rather than aspirational: the
/// hashes are only "unlinkable after the salt is gone" if they are also gone.
/// `key_ip_seen` at 7 days and `key_ip_daily` at 90 days, both from
/// [`docs/ip-tracking.md`](../../docs/ip-tracking.md) §Retention. The daily
/// aggregate survives the hashes because a count with no salt behind it is a
/// trend, not a history.
///
/// THE CUTOFF IS INCLUSIVE, AND THE WINDOW IS "TODAY PLUS THE PRECEDING SIX".
/// The doc promises 7 days of `key_ip_seen` and 90 of `key_ip_daily`; those
/// are the numbers of days RETAINED, so the days kept are
/// `today - (N - 1) ..= today` and every day at or before `today - N` is deleted.
/// The comparison is therefore `<=`, not `<`: with `<` the cutoff day itself
/// survived, which quietly retained N+1 days — 8 days of hashes and 91 daily
/// rows — against a privacy statement that says 7 and 90. A retention window
/// longer than the documented one is a broken promise, not a rounding detail.
///
/// The constants are the contract and stay as documented. A boundary that looks
/// wrong is fixed HERE, in the comparison, never by nudging the constant to
/// compensate — that would make the code agree with the docs by making both
/// wrong.
///
/// Run nightly. Nothing calls this per-request — deleting on the hot path
/// would add a second write to every proxied request to do work that has to
/// happen once a day.
pub async fn purge_expired(pool: &SqlitePool, today: NaiveDate) -> Result<PurgedRows, AppError> {
    // Inclusive cutoffs: the boundary day is deleted, `today - N + 1` is kept.
    //
    // SAFE, and it is the same argument as every other date subtraction in this
    // repository: chrono's date arithmetic PANICS on an out-of-range result rather
    // than wrapping, so a nonsense window fails loudly instead of quietly dating
    // every row to the wrong side of the cutoff. The operands are retention
    // constants (7 and 90 days) against the current date, so the range is never in
    // question anyway.
    #[allow(clippy::arithmetic_side_effects)]
    let seen_cutoff = today - chrono::Duration::days(SEEN_RETENTION_DAYS);
    #[allow(clippy::arithmetic_side_effects)]
    let daily_cutoff = today - chrono::Duration::days(DAILY_RETENTION_DAYS);

    let seen = sqlx::query("DELETE FROM key_ip_seen WHERE day <= ?")
        .bind(seen_cutoff)
        .execute(pool)
        .await?
        .rows_affected();

    let daily = sqlx::query("DELETE FROM key_ip_daily WHERE day <= ?")
        .bind(daily_cutoff)
        .execute(pool)
        .await?
        .rows_affected();

    // The link-code attempt counter is the SAME privacy class as `key_ip_seen` - a
    // salted IP hash answering "who was this" - so it gets the same 7-day bound and
    // the same sweep. Sweeping it here rather than in a second job is deliberate:
    // two retention jobs means two places the policy can be forgotten, and this
    // table was in fact added with NO retention policy at all before this line
    // existed, which is the failure mode the reuse prevents from recurring.
    //
    // The cutoff is the SAME inclusive `<=` for the same reason documented above
    // (an exclusive comparison silently retains N+1 days), but the VALUE is an
    // INSTANT, not a date: `attempted_at` is a timestamp and the column is TEXT, so
    // binding a `NaiveDate` would store `2026-09-27` and compare it as a string
    // against `2026-09-27T03:04:05+00:00`. The shorter string sorts FIRST, so the
    // DELETE would match nothing and the rows would survive forever - a silent
    // retention failure in the direction that keeps data. Hence midnight UTC of the
    // cutoff DAY, which is the instant the day begins.
    let link_attempts = sqlx::query("DELETE FROM link_redemption_attempts WHERE attempted_at <= ?")
        .bind(
            seen_cutoff
                .and_hms_opt(0, 0, 0)
                .expect("midnight is valid")
                .and_utc(),
        )
        .execute(pool)
        .await?
        .rows_affected();

    // The link-code ISSUANCE counter is the same class again: an account_id and a
    // timestamp, feeding an abuse guard and holding nothing a person is identified
    // by. Same 7-day bound, same sweep, same reason - a second retention policy is a
    // second place the policy can be forgotten, and the first version of that table
    // shipped with none at all.
    //
    // The cutoff reuses seen_cutoff and the same midnight-UTC instant, which is
    // correct here for the same reason as above: created_at is a TIMESTAMP in a TEXT
    // column, so binding a bare date would compare 2026-09-27 as a string against
    // 2026-09-27T03:04:05+00:00 - the shorter sorts first, the DELETE matches
    // nothing, and the rows survive forever. A silent retention failure in the
    // direction that keeps data.
    let link_issues = sqlx::query("DELETE FROM link_code_issues WHERE created_at <= ?")
        .bind(
            seen_cutoff
                .and_hms_opt(0, 0, 0)
                .expect("midnight is valid")
                .and_utc(),
        )
        .execute(pool)
        .await?
        .rows_affected();

    Ok(PurgedRows {
        seen,
        daily,
        link_attempts,
        link_issues,
    })
}

// ---------------------------------------------------------------------------
// Which address the request came from
// ---------------------------------------------------------------------------
//
// The backend sits behind the edge relay, so the TCP peer is the relay, not the
// caller, and every key would otherwise show one source. The caller's address
// arrives in `X-Forwarded-For`.
//
// THAT HEADER IS CLIENT-CONTROLLED, AND THAT IS THE WHOLE PROBLEM HERE. A
// caller who sets `X-Forwarded-For: 1.2.3.4` on a directly-received request
// chooses what we record. For an abuse signal the dangerous direction is
// under-counting: an account reselling one key would pin a single forged
// address and sit at one distinct IP forever, which is precisely the signal
// this module exists to raise. So the header is consulted ONLY when the peer
// is a configured trusted proxy, and otherwise ignored entirely.
//
// This answers the open item in docs/ip-tracking.md ("whether the edge relay
// or the backend computes the hash"): the backend does, because it is the side
// that owns the salt and the tables, and because the relay has no database.

/// An IP network in CIDR form.
///
/// Hand-rolled rather than pulled from a crate: it is ~30 lines of bit masking,
/// and the proxy's hot path already carries enough dependencies for a feature
/// that runs once per request and only needs `contains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpCidr {
    base: std::net::IpAddr,
    prefix: u8,
}

impl IpCidr {
    /// Parses `ADDRESS/PREFIX`. Rejects a prefix longer than the address family
    /// allows rather than silently truncating it: `10.0.0.0/33` written by
    /// mistake must not become a rule that matches everything.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (address, prefix) = text
            .split_once('/')
            .ok_or_else(|| format!("{text}: expected ADDRESS/PREFIX"))?;

        let base: std::net::IpAddr = address
            .trim()
            .parse()
            .map_err(|_| format!("{address}: not an IP address"))?;
        let prefix: u8 = prefix
            .trim()
            .parse()
            .map_err(|_| format!("{prefix}: not a prefix length"))?;

        let max = match base {
            std::net::IpAddr::V4(_) => 32,
            std::net::IpAddr::V6(_) => 128,
        };
        if prefix > max {
            return Err(format!(
                "{text}: prefix {prefix} exceeds {max} for this address family"
            ));
        }
        Ok(Self { base, prefix })
    }

    /// Whether `ip` falls inside this network. Addresses of different families
    /// never match, so an IPv6 relay rule cannot accidentally trust an IPv4 peer.
    pub fn contains(&self, ip: &std::net::IpAddr) -> bool {
        // `*ip` so the arms bind addresses, not references: `u32::from` takes
        // an `Ipv4Addr` by value.
        match (self.base, *ip) {
            (std::net::IpAddr::V4(net), std::net::IpAddr::V4(addr)) => {
                let mask = v4_mask(self.prefix);
                u32::from(net) & mask == u32::from(addr) & mask
            }
            (std::net::IpAddr::V6(net), std::net::IpAddr::V6(addr)) => {
                let mask = v6_mask(self.prefix);
                u128::from(net) & mask == u128::from(addr) & mask
            }
            _ => false,
        }
    }
}

/// `prefix == 0` is handled separately, and the comment this replaces called the
/// consequence "undefined". It is not undefined - Rust defines a shift past the
/// width as an overflow, which panics in a debug build and masks in a release one.
/// Both are wrong here: this runs on the REQUEST PATH, so the failure would be a
/// panic under a debug build and a silently wrong mask in the one that ships.
// SAFE, and the bound is CROSS-FUNCTION, which is why it is written down rather
// than left to the reader: `32 - prefix` underflows for any prefix above 32, and
// the result feeds a shift whose amount must stay under 32. IpCidr::parse rejects a
// prefix past the family width, so the only way to reach this above 32 is to build
// the struct without it - the fields are private, so that is this module's own tests
// and nothing else.
//
// If that ever changes, the failure in production is not a crash but a mask that
// trusts a different set of hosts than the operator wrote, which is the outcome this
// whole module exists to make hard.
//
// The allow is on the FUNCTION: an attribute in tail-expression position is still
// unstable, and the version that put it on the `else` arm did not compile.
#[allow(clippy::arithmetic_side_effects)]
fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

/// The v6 form, with the same cross-function bound against 128. The widest legal
/// v6 prefix is /128, which makes this the largest shift either mask function
/// performs, and it was the one end of the range nothing tested.
///
/// SAFE for the same reason as `v4_mask`: parse bounds prefix to 128, so
/// `128 - prefix` is 0..=127 and the shift stays inside the type.
#[allow(clippy::arithmetic_side_effects)]
fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

/// Parses the configured CIDR list, failing on the first malformed entry.
///
/// Fail fast, at startup: a typo in a trust rule is a security decision, and
/// the two ways it can fail are both bad. Skipping the bad entry trusts less
/// than intended (and silently), while accepting it may trust more.
pub fn parse_cidrs(texts: &[String]) -> Result<Vec<IpCidr>, String> {
    texts.iter().map(|text| IpCidr::parse(text)).collect()
}

/// The address the request came from.
///
/// `peer` is the TCP peer. When it is a trusted proxy, `X-Forwarded-For` is
/// walked RIGHT TO LEFT and the first entry that is not itself a trusted proxy
/// is the caller: each hop appends the address it received the connection from,
/// so the closest trusted proxies are on the right and the caller is the first
/// untrusted address before them. Walking left to right instead would stop on
/// the caller's own forged entry.
///
/// If every entry is trusted — or the header is absent or unparseable — the
/// peer is returned. That is the honest answer: the caller's address is not
/// known, and inventing one from a spoofable header is worse than recording the
/// relay, which at least cannot be chosen by the caller.
pub fn resolve_client_ip(
    peer: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
    trusted: &[IpCidr],
) -> std::net::IpAddr {
    // The header name is spelled out: `HeaderMap::get` accepts a `&str` and
    // lowercases it, which keeps this independent of a re-exported constant.
    let Some(raw) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
    else {
        return peer;
    };

    // An untrusted peer's header is never consulted — see the note above.
    if !trusted.iter().any(|cidr| cidr.contains(&peer)) {
        return peer;
    }

    for entry in raw.split(',').rev() {
        let Ok(address) = entry.trim().parse::<std::net::IpAddr>() else {
            continue;
        };
        if trusted.iter().any(|cidr| cidr.contains(&address)) {
            continue;
        }
        return address;
    }

    peer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};
    use axum::http::HeaderMap;
    use chrono::DateTime;
    use std::net::IpAddr;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("parse ip")
    }

    #[test]
    fn the_same_ip_and_salt_hash_the_same() {
        let salt = [7u8; 32];
        assert_eq!(
            ip_hash(&salt, &ip("203.0.113.9")),
            ip_hash(&salt, &ip("203.0.113.9"))
        );
    }

    #[test]
    fn different_ips_and_different_salts_hash_differently() {
        let salt = [7u8; 32];
        let other_salt = [8u8; 32];

        assert_ne!(
            ip_hash(&salt, &ip("203.0.113.9")),
            ip_hash(&salt, &ip("203.0.113.10"))
        );
        assert_ne!(
            ip_hash(&salt, &ip("203.0.113.9")),
            ip_hash(&other_salt, &ip("203.0.113.9"))
        );
    }

    #[test]
    fn ip_hash_arguments_are_not_interchangeable() {
        // The regression this guards is silent: HMAC(key = ip, message = salt)
        // produces a hash of identical length and identical stability, so
        // everything still works and the salt simply stops being secret. The
        // only observable difference is that the two orders disagree.
        let salt = [7u8; 32];
        let address = ip("203.0.113.9");

        let correct = ip_hash(&salt, &address);

        let mut swapped = Hmac::<Sha256>::new_from_slice(address.to_string().as_bytes())
            .expect("HMAC accepts any key length");
        swapped.update(&salt);
        let swapped = hex::encode(swapped.finalize().into_bytes());

        assert_ne!(
            correct, swapped,
            "salt must be the HMAC key and the IP the message"
        );
    }

    #[test]
    fn the_hash_is_a_full_sha256_output() {
        // 32 bytes hex-encoded. A truncated hash would still be stable and
        // would collide across the IPv4 space far sooner.
        assert_eq!(ip_hash(&[0u8; 32], &ip("203.0.113.9")).len(), 64);
    }

    #[test]
    fn the_salt_is_stable_within_a_day_and_replaced_across_days() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 25).expect("date");
        let next = NaiveDate::from_ymd_opt(2026, 9, 26).expect("date");
        let salt = DailySalt::seeded(day, [1u8; 32]);

        assert_eq!(salt.salt_for_day(day), [1u8; 32], "same day, same salt");
        assert_eq!(salt.salt_for_day(day), [1u8; 32], "repeated reads agree");

        let rotated = salt.salt_for_day(next);
        assert_ne!(rotated, [1u8; 32], "a new day must mint a new salt");
        assert_eq!(
            salt.salt_for_day(next),
            rotated,
            "the rotated salt is stable for the rest of that day"
        );
    }

    #[test]
    fn rotating_discards_the_previous_salt() {
        // There is no accessor for a past day's salt by design: yesterday's
        // hashes have to stay unlinkable. Asking for an older day therefore
        // returns the CURRENT salt, not the one that made those hashes.
        let day = NaiveDate::from_ymd_opt(2026, 9, 25).expect("date");
        let salt = DailySalt::seeded(day, [1u8; 32]);
        let _ = salt.salt_for_day(day.succ_opt().expect("next day"));

        let yesterday = day.pred_opt().expect("previous day");
        assert_ne!(
            salt.salt_for_day(yesterday),
            [1u8; 32],
            "an older day must not recover the salt that made its hashes"
        );
    }

    #[test]
    fn a_fresh_salt_is_not_all_zeroes() {
        // A zero salt is a salt anyone can guess, which defeats the point.
        assert_ne!(fresh_salt(), [0u8; 32]);
        assert_ne!(fresh_salt(), fresh_salt(), "two fresh salts must differ");
    }

    #[test]
    fn a_poisoned_salt_lock_recovers_instead_of_panicking() {
        // The lock is poisoned the only way it can be: a thread panics while
        // holding the write guard. Every later access then sees Err(Poisoned),
        // and the module must recover the guard rather than panic again — a
        // second panic here would make one transient failure permanent, on a
        // path every proxied request takes.
        let day = NaiveDate::from_ymd_opt(2026, 9, 25).expect("date");
        let next = NaiveDate::from_ymd_opt(2026, 9, 26).expect("date");
        let salt = std::sync::Arc::new(DailySalt::seeded(day, [1u8; 32]));

        let poisoner = std::sync::Arc::clone(&salt);
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoner.state.write().expect("fresh lock is unpoisoned");
                panic!("poison the salt lock");
            })
            .join()
            .is_err(),
            "the poisoning thread must have panicked while holding the guard"
        );

        // The read guard (the common path: the held day matches).
        assert_eq!(
            salt.salt_for_day(day),
            [1u8; 32],
            "a poisoned salt lock must still yield the held salt"
        );

        // The write guard (the rotation path), still poisoned.
        let rotated = salt.salt_for_day(next);
        assert_ne!(rotated, [1u8; 32], "rotation must still mint a new salt");
        assert_eq!(
            salt.salt_for_day(next),
            rotated,
            "and the rotated salt must be stable"
        );
    }

    fn cidr(text: &str) -> IpCidr {
        IpCidr::parse(text).expect("parse cidr")
    }

    fn forwarded_for(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static("x-forwarded-for"),
            value.parse().expect("header value"),
        );
        headers
    }

    #[test]
    fn cidrs_match_by_family_and_prefix() {
        let slash24 = cidr("203.0.113.0/24");
        assert!(slash24.contains(&ip("203.0.113.0")));
        assert!(slash24.contains(&ip("203.0.113.255")));
        assert!(!slash24.contains(&ip("203.0.114.1")));

        assert!(cidr("10.0.0.0/8").contains(&ip("10.255.255.255")));
        assert!(cidr("203.0.113.9/32").contains(&ip("203.0.113.9")));
        assert!(!cidr("203.0.113.9/32").contains(&ip("203.0.113.10")));
        assert!(
            cidr("0.0.0.0/0").contains(&ip("8.8.8.8")),
            "/0 covers everything"
        );
        assert!(
            !cidr("0.0.0.0/0").contains(&ip("::1")),
            "families never mix"
        );

        assert!(cidr("2001:db8::/32").contains(&ip("2001:db8::1")));
        assert!(!cidr("2001:db8::/32").contains(&ip("2001:db9::1")));
    }

    #[test]
    fn a_malformed_cidr_is_rejected_not_silently_truncated() {
        assert!(IpCidr::parse("10.0.0.0").is_err(), "no prefix");
        assert!(
            IpCidr::parse("10.0.0.0/33").is_err(),
            "prefix past the family"
        );
        assert!(IpCidr::parse("2001:db8::/129").is_err());
        assert!(IpCidr::parse("not-an-ip/24").is_err());
        assert!(IpCidr::parse("10.0.0.0/x").is_err());
        assert!(parse_cidrs(&["10.0.0.0/8".to_string(), "bad".to_string()]).is_err());
    }

    #[test]
    fn an_untrusted_peer_cannot_choose_its_own_address() {
        // The attack this defends against: a reseller sends a fixed forged
        // address so the key always shows one distinct source.
        let trusted = vec![cidr("172.17.0.0/16")];
        let spoofed = forwarded_for("1.2.3.4");

        assert_eq!(
            resolve_client_ip(ip("203.0.113.9"), &spoofed, &trusted),
            ip("203.0.113.9"),
            "a direct peer's X-Forwarded-For must be ignored"
        );
    }

    #[test]
    fn a_trusted_peer_yields_the_first_untrusted_entry_from_the_right() {
        let trusted = vec![cidr("172.17.0.0/16")];
        // "caller, relay": the rightmost is the relay itself, so the caller is
        // the entry before it.
        let headers = forwarded_for("203.0.113.9, 172.17.0.5");
        assert_eq!(
            resolve_client_ip(ip("172.17.0.5"), &headers, &trusted),
            ip("203.0.113.9")
        );

        // Two hops of trusted proxy: still the caller, not a relay.
        let chained = forwarded_for("203.0.113.9, 172.17.0.7, 172.17.0.5");
        assert_eq!(
            resolve_client_ip(ip("172.17.0.5"), &chained, &trusted),
            ip("203.0.113.9")
        );

        // A forged entry to the LEFT of the real one is not where we look.
        let forged = forwarded_for("1.2.3.4, 203.0.113.9, 172.17.0.5");
        assert_eq!(
            resolve_client_ip(ip("172.17.0.5"), &forged, &trusted),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn an_all_trusted_or_unusable_header_falls_back_to_the_peer() {
        let trusted = vec![cidr("172.17.0.0/16")];

        // Every claimed address is a trusted proxy: the caller is not known, so
        // record the relay rather than trusting a header entry.
        let all_trusted = forwarded_for("172.17.0.7, 172.17.0.5");
        assert_eq!(
            resolve_client_ip(ip("172.17.0.5"), &all_trusted, &trusted),
            ip("172.17.0.5")
        );

        // Garbage entries are skipped, not crashed on.
        let garbage = forwarded_for("not-an-ip, , 172.17.0.5");
        assert_eq!(
            resolve_client_ip(ip("172.17.0.5"), &garbage, &trusted),
            ip("172.17.0.5")
        );

        // No header at all.
        assert_eq!(
            resolve_client_ip(ip("172.17.0.5"), &HeaderMap::new(), &trusted),
            ip("172.17.0.5")
        );
    }

    #[test]
    fn with_no_trusted_proxies_configured_the_peer_always_wins() {
        assert_eq!(
            resolve_client_ip(ip("203.0.113.9"), &forwarded_for("1.2.3.4"), &[]),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn the_retention_windows_match_the_privacy_statement() {
        assert_eq!(SEEN_RETENTION_DAYS, 7);
        assert_eq!(DAILY_RETENTION_DAYS, 90);
    }

    #[test]
    fn the_thresholds_match_the_documented_starting_values() {
        // docs/ip-tracking.md: ">20 domestic-distinct, sustained".
        assert_eq!(SHARING_SUSPICION_IPS, 20);
    }

    #[test]
    fn the_salt_never_appears_in_a_debug_print() {
        // A byte value chosen so its hex form cannot appear by coincidence in
        // the date: [0xAB; 32] would print as a run of "ababab...".
        let salt = DailySalt::seeded(
            NaiveDate::from_ymd_opt(2026, 9, 25).expect("date"),
            [0xABu8; 32],
        );
        let printed = format!("{salt:?}");
        assert!(
            !printed.contains("ababab"),
            "salt bytes leaked into a log line: {printed}"
        );
        assert!(printed.contains("redacted"));
    }

    // -----------------------------------------------------------------------
    // Database tests. Each builds its own migrated database in a temp
    // directory, so they run by default and share no rows with each other.
    // -----------------------------------------------------------------------

    /// A key needs an account, so the fixture creates one and returns both.
    ///
    /// Ported: the Postgres original read DATABASE_URL and `DELETE`d its fixture
    /// rows in FK order afterwards, serialized by a process-wide `purge_guard`
    /// because `purge_expired` deletes globally and two purge tests sharing one
    /// database could delete each other's out-of-window rows. Each test now owns
    /// its own migrated database (`TestDb`), so neither the DSN, the teardown, nor
    /// the guard is needed: the guard would only be serializing tests that already
    /// cannot see each other.
    async fn create_key(pool: &SqlitePool) -> (Uuid, Uuid) {
        let account_id = test_support::account(pool).await;
        let key_id = test_support::api_key(pool, account_id).await;
        (account_id, key_id)
    }

    #[tokio::test]
    async fn a_repeat_ip_counts_once_and_a_new_ip_counts_twice() {
        let db = TestDb::new().await;
        let (_, key_id) = create_key(&db.pool).await;
        let day = today_utc();
        let salt = [3u8; 32];

        let first = ip_hash(&salt, &ip("203.0.113.9"));
        assert_eq!(
            record_key_ip(&db.pool, key_id, day, &first)
                .await
                .expect("first request"),
            KeyIpCounts {
                distinct_ips: 1,
                request_count: 1
            }
        );

        // Same address again: the request counts, the distinct count does not.
        assert_eq!(
            record_key_ip(&db.pool, key_id, day, &first)
                .await
                .expect("repeat request"),
            KeyIpCounts {
                distinct_ips: 1,
                request_count: 2
            }
        );

        let second = ip_hash(&salt, &ip("203.0.113.10"));
        assert_eq!(
            record_key_ip(&db.pool, key_id, day, &second)
                .await
                .expect("new ip"),
            KeyIpCounts {
                distinct_ips: 2,
                request_count: 3
            }
        );

        // A different day is a different counter, so a mobile user's churn does
        // not accumulate into a single day's figure.
        let tomorrow = day.succ_opt().expect("next day");
        let fresh = ip_hash(&[4u8; 32], &ip("203.0.113.9"));
        assert_eq!(
            record_key_ip(&db.pool, key_id, tomorrow, &fresh)
                .await
                .expect("next day"),
            KeyIpCounts {
                distinct_ips: 1,
                request_count: 1
            }
        );

        db.close().await;
    }

    #[tokio::test]
    async fn the_purge_keeps_the_window_and_removes_what_is_past_it() {
        let db = TestDb::new().await;
        let (_, key_id) = create_key(&db.pool).await;
        let today = today_utc();
        let salt = [5u8; 32];

        // Inside both windows.
        let recent = today - chrono::Duration::days(SEEN_RETENTION_DAYS - 1);
        record_key_ip(
            &db.pool,
            key_id,
            recent,
            &ip_hash(&salt, &ip("203.0.113.1")),
        )
        .await
        .expect("recent row");

        // Past the hash window but inside the aggregate window: the hash goes,
        // the count stays. That asymmetry is the privacy design.
        let older = today - chrono::Duration::days(SEEN_RETENTION_DAYS + 1);
        record_key_ip(&db.pool, key_id, older, &ip_hash(&salt, &ip("203.0.113.2")))
            .await
            .expect("older row");

        // Past both.
        let ancient = today - chrono::Duration::days(DAILY_RETENTION_DAYS + 1);
        record_key_ip(
            &db.pool,
            key_id,
            ancient,
            &ip_hash(&salt, &ip("203.0.113.3")),
        )
        .await
        .expect("ancient row");

        let purged = purge_expired(&db.pool, today).await.expect("purge");
        assert!(
            purged.seen >= 2,
            "both rows past the hash window must go, got {}",
            purged.seen
        );
        assert!(purged.daily >= 1, "the row past 90 days must go");

        let remaining_seen: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM key_ip_seen WHERE api_key_id = ?")
                .bind(key_id.hyphenated())
                .fetch_one(&db.pool)
                .await
                .expect("count remaining hashes");
        assert_eq!(
            remaining_seen, 1,
            "only the in-window hash survives: the aggregate outlives the hash"
        );

        db.close().await;
    }

    /// THE BOUNDARY, PINNED EXACTLY — the off-by-one this test exists for.
    ///
    /// A test that only asserts "old rows go" passes with BOTH `<` and `<=`,
    /// which is exactly how the window quietly became N+1 days against a
    /// statement that says N. So both directions are asserted: the row ON the
    /// cutoff day is DELETED, and the row one day INSIDE the window is KEPT.
    ///
    /// Its own account AND its own database, so no other test's rows can inflate
    /// the result. The Postgres original needed both a `DATABASE_URL` and a
    /// process-wide `purge_guard` for this; `TestDb` gives it per test instead,
    /// which is why the `#[ignore]` that made this boundary assertion never run
    /// is gone.
    #[tokio::test]
    async fn the_retention_cutoff_day_is_deleted_and_the_day_inside_the_window_is_kept() {
        let db = TestDb::new().await;
        let (_, key_id) = create_key(&db.pool).await;
        let today = today_utc();
        let salt = [6u8; 32];

        // On the cutoff day: must be deleted. This is the day the exclusive
        // comparison used to keep, retaining 8 days of hashes and 91 daily rows.
        let seen_cutoff = today - chrono::Duration::days(SEEN_RETENTION_DAYS);
        let daily_cutoff = today - chrono::Duration::days(DAILY_RETENTION_DAYS);
        // One day inside each window: must survive.
        let seen_kept = today - chrono::Duration::days(SEEN_RETENTION_DAYS - 1);
        let daily_kept = today - chrono::Duration::days(DAILY_RETENTION_DAYS - 1);

        for (day, address) in [
            (seen_cutoff, "203.0.113.11"),
            (seen_kept, "203.0.113.12"),
            (daily_cutoff, "203.0.113.13"),
            (daily_kept, "203.0.113.14"),
        ] {
            record_key_ip(&db.pool, key_id, day, &ip_hash(&salt, &ip(address)))
                .await
                .expect("seed row");
        }

        purge_expired(&db.pool, today).await.expect("purge");

        let surviving_seen: Vec<NaiveDate> =
            sqlx::query_scalar("SELECT day FROM key_ip_seen WHERE api_key_id = ? ORDER BY day")
                .bind(key_id.hyphenated())
                .fetch_all(&db.pool)
                .await
                .expect("read surviving hashes");
        assert_eq!(
            surviving_seen,
            vec![seen_kept],
            "the day AT the 7-day cutoff ({seen_cutoff}) must be deleted and              {seen_kept}, one day inside the window, must be kept"
        );

        let surviving_daily: Vec<NaiveDate> =
            sqlx::query_scalar("SELECT day FROM key_ip_daily WHERE api_key_id = ? ORDER BY day")
                .bind(key_id.hyphenated())
                .fetch_all(&db.pool)
                .await
                .expect("read surviving counts");
        // Containment, not an exact vector: `record_key_ip` writes a daily row
        // for every day it sees, so the two days seeded for the hash window are
        // legitimately INSIDE the 90-day aggregate window and survive. The
        // boundary is what this pins: the cutoff day gone, the day inside kept.
        assert!(
            !surviving_daily.contains(&daily_cutoff),
            "the day AT the 90-day cutoff ({daily_cutoff}) must be deleted, got {surviving_daily:?}"
        );
        assert!(
            surviving_daily.contains(&daily_kept),
            "the day one inside the 90-day window ({daily_kept}) must be kept, got {surviving_daily:?}"
        );

        db.close().await;
    }

    // -----------------------------------------------------------------------
    // The link_redemption_attempts sweep.
    //
    // These rows are the SAME privacy class as key_ip_seen - a salted IP hash
    // answering "who was this, today" - so they get the SAME bound (7 days) and
    // the SAME sweep. A second period or a second job would be a second retention
    // policy for identical data, which is the drift data-retention.md exists to
    // prevent.
    // -----------------------------------------------------------------------

    /// Seeds one attempt row at an explicit instant.
    async fn seed_attempt(pool: &SqlitePool, at: DateTime<Utc>) {
        sqlx::query("INSERT INTO link_redemption_attempts (ip_hash, attempted_at) VALUES (?, ?)")
            .bind(ip_hash(&[5u8; 32], &ip("203.0.113.77")))
            .bind(at)
            .execute(pool)
            .await
            .expect("seed an attempt row");
    }

    async fn attempt_rows(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM link_redemption_attempts")
            .fetch_one(pool)
            .await
            .expect("count attempt rows")
    }

    /// RED FIRST: stale attempt rows are DELETED by the sweep.
    ///
    /// Against the pre-fix code this fails for the real reason: nothing deletes
    /// them, so a table of per-attempt client hashes sits in the database forever -
    /// the per-client history docs/ip-tracking.md says must not be built.
    #[tokio::test]
    async fn the_sweep_deletes_attempt_rows_past_the_bound_and_keeps_recent_ones() {
        let db = TestDb::new().await;
        let now = Utc::now();

        // Far outside the window.
        seed_attempt(
            &db.pool,
            now - chrono::Duration::days(SEEN_RETENTION_DAYS + 30),
        )
        .await;
        // Just inside it - a live incident must still be investigable.
        seed_attempt(&db.pool, now - chrono::Duration::days(1)).await;

        assert_eq!(attempt_rows(&db.pool).await, 2, "both rows seeded");

        purge_expired(&db.pool, today_utc()).await.expect("purge");

        assert_eq!(
            attempt_rows(&db.pool).await,
            1,
            "the row past the retention bound must be DELETED; before this fix it survived forever"
        );

        db.close().await;
    }

    /// THE BOUNDARY, PINNED EXACTLY - both directions.
    ///
    /// "Old rows go" passes with BOTH `<` and `<=`, which is how the window quietly
    /// becomes N+1 days. This column is also a TIMESTAMP, not a DATE, so the cutoff
    /// must compare instants: binding a NaiveDate here would store `2026-09-27` and
    /// compare it as a STRING against `2026-09-27T03:04:05+00:00`, where the shorter
    /// string sorts FIRST and the DELETE would remove nothing at all. Both halves are
    /// asserted so neither the off-by-one nor the type confusion can return.
    #[tokio::test]
    async fn the_attempt_cutoff_is_inclusive_of_the_boundary_instant_and_keeps_the_rest() {
        let db = TestDb::new().await;
        let today = today_utc();
        let cutoff = today - chrono::Duration::days(SEEN_RETENTION_DAYS);

        // AT the cutoff instant: must go (inclusive).
        seed_attempt(&db.pool, cutoff.and_hms_opt(0, 0, 0).unwrap().and_utc()).await;
        // One second INSIDE the window: must stay. This assertion fails if the
        // cutoff becomes exclusive.
        seed_attempt(&db.pool, cutoff.and_hms_opt(0, 0, 1).unwrap().and_utc()).await;

        purge_expired(&db.pool, today).await.expect("purge");

        let surviving: Vec<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT attempted_at FROM link_redemption_attempts ORDER BY attempted_at",
        )
        .fetch_all(&db.pool)
        .await
        .expect("read surviving attempts");

        assert_eq!(
            surviving,
            vec![cutoff.and_hms_opt(0, 0, 1).unwrap().and_utc()],
            "the instant AT the cutoff must be deleted and one second later must survive - and if this deletes NOTHING, the cutoff is being compared as a DATE against a TIMESTAMP column"
        );

        db.close().await;
    }

    #[test]
    fn daily_salt_default_builds_a_fresh_salt() {
        // The `Default` impl backs `#[derive(Default)]` callers and the app's
        // lazy init; it must produce a real, non-guessable salt for today.
        let salt = DailySalt::default();
        let day = today_utc();
        assert_ne!(
            salt.salt_for_day(day),
            [0u8; 32],
            "a default salt must not be all zeroes"
        );
    }

    /// The mask must trust EXACTLY the hosts the operator wrote, at every prefix.
    ///
    /// The previous test checked one value - `v4_mask(0)` and `v6_mask(0)` - and
    /// nothing else. Zero is the easy end: it is the special case the function
    /// handles in its own arm, so it cannot be reached by a mistake in the shift.
    /// The ends that CAN be reached by a mistake are the other two: the widest legal
    /// prefix, where the shift amount is largest, and anything near it.
    ///
    /// The property is swept rather than spot-checked, and it is stated as a count
    /// rather than as figures: a mask for prefix `n` must have exactly `n` leading
    /// ones. Too few and the relay stops being trusted; too many and hosts the
    /// operator deliberately excluded are trusted, which is the outcome
    /// docs/ip-tracking.md exists to make hard. A single wrong number cannot hide in
    /// a set bit count, and every legal prefix is covered rather than a sample.
    ///
    /// 0 and the family width are named explicitly as well, because they are the two
    /// the implementation branches on and the two a sweep could otherwise treat as
    /// merely interior points.
    #[test]
    fn a_mask_trusts_exactly_the_hosts_the_operator_wrote_at_every_prefix() {
        for prefix in 0u8..=32 {
            assert_eq!(
                v4_mask(prefix).count_ones(),
                u32::from(prefix),
                "an IPv4 /{prefix} mask must have exactly {prefix} leading ones"
            );
        }
        for prefix in 0u8..=128 {
            assert_eq!(
                v6_mask(prefix).count_ones(),
                // count_ones() is u32 for BOTH widths, including u128 - which is
                // itself a small trap in a test that is otherwise about 128-bit
                // values, and caught here by the compiler rather than by a sweep
                // that silently compared nothing.
                u32::from(prefix),
                "an IPv6 /{prefix} mask must have exactly {prefix} leading ones"
            );
        }

        // The branch points, named rather than left to the sweep above to imply.
        // `prefix == 0` is a special arm because `u32::MAX << 32` is an overflow -
        // Rust defines a shift past the width rather than leaving it undefined, and
        // an overflow panics in a debug build and masks in a release one. Both are
        // wrong on the request path, which is why the arm exists.
        assert_eq!(v4_mask(0), 0, "a /0 trusts nobody");
        assert_eq!(v6_mask(0), 0, "a /0 trusts nobody");
        assert_eq!(v4_mask(32), u32::MAX, "a /32 trusts exactly one address");
        assert_eq!(v6_mask(128), u128::MAX, "a /128 trusts exactly one address");
        assert_eq!(v4_mask(24), 0xFFFF_FF00, "a /24 masks on the last octet");
        assert_eq!(v6_mask(64), u128::MAX << 64, "a /64 masks on the low half");
    }

    #[tokio::test]
    async fn recording_past_the_sharing_threshold_warns_once() {
        // docs/ip-tracking.md: a key seen from >20 distinct domestic IPs in a
        // day is a sharing-suspicion flag for a human, not a refusal. The
        // warning is logged at the crossing (distinct_ips == SHARING_SUSPICION_IPS + 1),
        // which is the branch that must not silently stop firing.
        let db = TestDb::new().await;
        let (_, key_id) = create_key(&db.pool).await;
        let day = today_utc();
        let salt = [9u8; 32];

        for i in 1..=(SHARING_SUSPICION_IPS + 1) {
            let address = format!("198.51.100.{i}");
            let hash = ip_hash(&salt, &ip(&address));
            let counts = record_key_ip(&db.pool, key_id, day, &hash)
                .await
                .expect("record request source");
            assert_eq!(counts.distinct_ips, i, "the {i}-th distinct IP");
        }

        db.close().await;
    }
}
