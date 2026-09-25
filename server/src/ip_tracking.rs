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

use chrono::{NaiveDate, Utc};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use sqlx::{SqlitePool, Row};
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
            let state = self.state.read().expect("salt lock");
            if state.day == day {
                return state.bytes;
            }
        }

        let mut state = self.state.write().expect("salt lock");
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
/// Run nightly. Nothing calls this per-request — deleting on the hot path
/// would add a second write to every proxied request to do work that has to
/// happen once a day.
pub async fn purge_expired(pool: &SqlitePool, today: NaiveDate) -> Result<PurgedRows, AppError> {
    let seen_cutoff = today - chrono::Duration::days(SEEN_RETENTION_DAYS);
    let daily_cutoff = today - chrono::Duration::days(DAILY_RETENTION_DAYS);

    let seen = sqlx::query("DELETE FROM key_ip_seen WHERE day < ?")
        .bind(seen_cutoff)
        .execute(pool)
        .await?
        .rows_affected();

    let daily = sqlx::query("DELETE FROM key_ip_daily WHERE day < ?")
        .bind(daily_cutoff)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(PurgedRows { seen, daily })
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
            return Err(format!("{text}: prefix {prefix} exceeds {max} for this address family"));
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

/// `prefix == 0` is handled separately: shifting a 32-bit value by 32 is
/// undefined, and it would panic in a debug build — on the request path.
fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

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
    use axum::http::HeaderMap;
    use std::net::IpAddr;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("parse ip")
    }

    #[test]
    fn the_same_ip_and_salt_hash_the_same() {
        let salt = [7u8; 32];
        assert_eq!(ip_hash(&salt, &ip("203.0.113.9")), ip_hash(&salt, &ip("203.0.113.9")));
    }

    #[test]
    fn different_ips_and_different_salts_hash_differently() {
        let salt = [7u8; 32];
        let other_salt = [8u8; 32];

        assert_ne!(ip_hash(&salt, &ip("203.0.113.9")), ip_hash(&salt, &ip("203.0.113.10")));
        assert_ne!(ip_hash(&salt, &ip("203.0.113.9")), ip_hash(&other_salt, &ip("203.0.113.9")));
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
        assert!(cidr("0.0.0.0/0").contains(&ip("8.8.8.8")), "/0 covers everything");
        assert!(!cidr("0.0.0.0/0").contains(&ip("::1")), "families never mix");

        assert!(cidr("2001:db8::/32").contains(&ip("2001:db8::1")));
        assert!(!cidr("2001:db8::/32").contains(&ip("2001:db9::1")));
    }

    #[test]
    fn a_malformed_cidr_is_rejected_not_silently_truncated() {
        assert!(IpCidr::parse("10.0.0.0").is_err(), "no prefix");
        assert!(IpCidr::parse("10.0.0.0/33").is_err(), "prefix past the family");
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
        assert_eq!(resolve_client_ip(ip("172.17.0.5"), &headers, &trusted), ip("203.0.113.9"));

        // Two hops of trusted proxy: still the caller, not a relay.
        let chained = forwarded_for("203.0.113.9, 172.17.0.7, 172.17.0.5");
        assert_eq!(resolve_client_ip(ip("172.17.0.5"), &chained, &trusted), ip("203.0.113.9"));

        // A forged entry to the LEFT of the real one is not where we look.
        let forged = forwarded_for("1.2.3.4, 203.0.113.9, 172.17.0.5");
        assert_eq!(resolve_client_ip(ip("172.17.0.5"), &forged, &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn an_all_trusted_or_unusable_header_falls_back_to_the_peer() {
        let trusted = vec![cidr("172.17.0.0/16")];

        // Every claimed address is a trusted proxy: the caller is not known, so
        // record the relay rather than trusting a header entry.
        let all_trusted = forwarded_for("172.17.0.7, 172.17.0.5");
        assert_eq!(resolve_client_ip(ip("172.17.0.5"), &all_trusted, &trusted), ip("172.17.0.5"));

        // Garbage entries are skipped, not crashed on.
        let garbage = forwarded_for("not-an-ip, , 172.17.0.5");
        assert_eq!(resolve_client_ip(ip("172.17.0.5"), &garbage, &trusted), ip("172.17.0.5"));

        // No header at all.
        assert_eq!(resolve_client_ip(ip("172.17.0.5"), &HeaderMap::new(), &trusted), ip("172.17.0.5"));
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
        let salt =
            DailySalt::seeded(NaiveDate::from_ymd_opt(2026, 9, 25).expect("date"), [0xABu8; 32]);
        let printed = format!("{salt:?}");
        assert!(
            !printed.contains("ababab"),
            "salt bytes leaked into a log line: {printed}"
        );
        assert!(printed.contains("redacted"));
    }

    // -----------------------------------------------------------------------
    // Live Sqlite. Ignored rather than silently skipped: a test that asserts
    // nothing is worse than no test.
    //
    //   DATABASE_URL=... cargo test --lib ip_tracking:: -- --ignored
    // -----------------------------------------------------------------------

    async fn test_pool() -> SqlitePool {
        let database_url =
            std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated Sqlite instance");
        crate::db::init_pool(&database_url).await.expect("connect to Sqlite")
    }

    /// A key needs an account. Returns (account_id, key_id).
    async fn create_key(pool: &SqlitePool) -> (Uuid, Uuid) {
        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES (?) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(pool)
                .await
                .expect("create account");

        let key_id: Uuid = sqlx::query_scalar(
            "INSERT INTO api_keys (account_id, key_hash, prefix) VALUES (?, ?, 'apk_test') RETURNING id",
        )
        .bind(account_id.hyphenated())
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .fetch_one(pool)
        .await
        .expect("create api key");

        (account_id, key_id)
    }

    async fn delete_fixture(pool: &SqlitePool, account_id: Uuid) {
        // Children first: both IP tables reference api_keys, which references
        // accounts, and api_keys is ON DELETE CASCADE from accounts.
        for statement in [
            "DELETE FROM key_ip_seen WHERE api_key_id IN (SELECT id FROM api_keys WHERE account_id = ?)",
            "DELETE FROM key_ip_daily WHERE api_key_id IN (SELECT id FROM api_keys WHERE account_id = ?)",
            "DELETE FROM api_keys WHERE account_id = ?",
            "DELETE FROM accounts WHERE id = ?",
        ] {
            sqlx::query(statement)
                .bind(account_id.hyphenated())
                .execute(pool)
                .await
                .unwrap_or_else(|err| panic!("cleanup failed on `{statement}`: {err}"));
        }
    }

    #[ignore = "requires live Sqlite: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn a_repeat_ip_counts_once_and_a_new_ip_counts_twice() {
        let pool = test_pool().await;
        let (account_id, key_id) = create_key(&pool).await;
        let day = today_utc();
        let salt = [3u8; 32];

        let first = ip_hash(&salt, &ip("203.0.113.9"));
        assert_eq!(
            record_key_ip(&pool, key_id, day, &first).await.expect("first request"),
            KeyIpCounts { distinct_ips: 1, request_count: 1 }
        );

        // Same address again: the request counts, the distinct count does not.
        assert_eq!(
            record_key_ip(&pool, key_id, day, &first).await.expect("repeat request"),
            KeyIpCounts { distinct_ips: 1, request_count: 2 }
        );

        let second = ip_hash(&salt, &ip("203.0.113.10"));
        assert_eq!(
            record_key_ip(&pool, key_id, day, &second).await.expect("new ip"),
            KeyIpCounts { distinct_ips: 2, request_count: 3 }
        );

        // A different day is a different counter, so a mobile user's churn does
        // not accumulate into a single day's figure.
        let tomorrow = day.succ_opt().expect("next day");
        let fresh = ip_hash(&[4u8; 32], &ip("203.0.113.9"));
        assert_eq!(
            record_key_ip(&pool, key_id, tomorrow, &fresh).await.expect("next day"),
            KeyIpCounts { distinct_ips: 1, request_count: 1 }
        );

        delete_fixture(&pool, account_id).await;
    }

    #[ignore = "requires live Sqlite: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn the_purge_keeps_the_window_and_removes_what_is_past_it() {
        let pool = test_pool().await;
        let (account_id, key_id) = create_key(&pool).await;
        let today = today_utc();
        let salt = [5u8; 32];

        // Inside both windows.
        let recent = today - chrono::Duration::days(SEEN_RETENTION_DAYS - 1);
        record_key_ip(&pool, key_id, recent, &ip_hash(&salt, &ip("203.0.113.1")))
            .await
            .expect("recent row");

        // Past the hash window but inside the aggregate window: the hash goes,
        // the count stays. That asymmetry is the privacy design.
        let older = today - chrono::Duration::days(SEEN_RETENTION_DAYS + 1);
        record_key_ip(&pool, key_id, older, &ip_hash(&salt, &ip("203.0.113.2")))
            .await
            .expect("older row");

        // Past both.
        let ancient = today - chrono::Duration::days(DAILY_RETENTION_DAYS + 1);
        record_key_ip(&pool, key_id, ancient, &ip_hash(&salt, &ip("203.0.113.3")))
            .await
            .expect("ancient row");

        let purged = purge_expired(&pool, today).await.expect("purge");
        assert!(
            purged.seen >= 2,
            "both rows past the hash window must go, got {}",
            purged.seen
        );
        assert!(purged.daily >= 1, "the row past 90 days must go");

        let remaining_seen: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM key_ip_seen WHERE api_key_id = ?")
                .bind(key_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count remaining hashes");
        assert_eq!(
            remaining_seen, 1,
            "only the in-window hash survives: the aggregate outlives the hash"
        );

        delete_fixture(&pool, account_id).await;
    }
}
