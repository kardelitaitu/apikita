# IP Tracking & Abuse Signals

Detecting key sharing and credential attacks **without building a surveillance
system**. Required by [`abuse-runbook.md`](abuse-runbook.md), constrained by
[`data-retention.md`](data-retention.md).

## The tension

| Need | Constraint |
| --- | --- |
| Know how many distinct IPs use a key | Do not store raw IP addresses |
| Detect credential attacks | Do not build a per-user location history |
| Respond to abuse fast | Do not become a data processor |

**Raw IPs are not stored anywhere.** The schema's `sessions.ip_hash` exists for
exactly this reason and the same approach applies here.

## What to store

**A hash, with a rotating salt.** Not the IP.

```text
ip_key = HMAC-SHA256(key = daily_salt, message = client_ip)

**Argument order is stated explicitly because it is easy to reverse.** The salt is
the **key**; the IP is the **message**. Reversing them still produces a stable
hash, so the bug would not surface in testing — but the salt would no longer be
secret-dependent, weakening the whole point.
```

| Property | Effect |
| --- | --- |
| Deterministic within a day | You can count distinct IPs for that day |
| Different across days | You cannot build a location history for a user |
| Salted | The hash cannot be reversed by brute-forcing the IPv4 space |
| Salt is deleted | Yesterday's hashes become permanently unlinkable |

**The rotating salt is the whole design.** It gives you the aggregate signal —
`distinct_ips` for a key today — while making longitudinal tracking impossible, even
for you.

### Why not just store the IP

Because the Terms of Service and privacy policy promise it is not stored, and a
promise the code contradicts is worse than no promise. See
[`terms-of-service.md`](terms-of-service.md) §5.

## Schema

```sql
-- one row per key per day; no IP list, just the count and a set size
-- Schema: docs/website/02-data-model.md (single source of truth)
-- key_ip_daily: api_key_id, day, distinct_ips, request_count
--               PRIMARY KEY (api_key_id, day)

-- raw hash->key mappings, short-lived, for computing distinct counts within a day
-- key_ip_seen: api_key_id, day, ip_hash
--              PRIMARY KEY (api_key_id, day, ip_hash)
```

**`key_ip_seen` is the only place a hash lives, and it is retained for days, not
months.** The daily aggregate is what you keep.

## Retention

| Data | Keep | Why |
| --- | --- | --- |
| `key_ip_seen` hashes | **7 days** | Enough to investigate a live incident |
| `link_redemption_attempts` hashes | **7 days** | Same class as `key_ip_seen`: a salted hash answering "who was this". Kept only so a live credential attack can be investigated — the `link_redemption_per_hour` cap is a rolling 1-hour window, so anything older than an hour is already inert for enforcement |
| `key_ip_daily` counts | 90 days | Trend without history |
| Daily salt | **deleted after the day** | Makes the hashes unlinkable forever |

**Deleting the salt is what makes this honest.** Even with the hashes, nobody —
including you — can recover the addresses after the salt is gone.

Deleting the ROWS is a separate job and a separate guarantee: the salt going
away stops yesterday's hashes being linkable, but the hashes would still sit in
`key_ip_seen` indefinitely. Both halves are needed, and neither substitutes for
the other.

## Implementation

`server/src/ip_tracking.rs`. The decisions that the design above left open, and
what was settled:

| Question | Answer |
| --- | --- |
| Where does the salt live? | **Process memory only.** 32 bytes from the OS RNG, replaced at the UTC day boundary, never written to disk or to the database |
| Relay or backend computes the hash? | **The backend.** It owns the salt and the tables; the relay has neither |
| How is the caller's address known? | `X-Forwarded-For`, but **only from a peer inside `network.trusted_proxy_cidrs`** |
| What enforces retention? | The maintenance scheduler (`.docker/maintenance/entrypoint.sh`), nightly, in inline SQL alongside the other jobs. **Not** `cargo run --bin ip-purge` — that binary is not shipped in the server image, and the scheduler logs it as NOT WIRED. See "Not enforced" below. |

**The salt is never persisted, on purpose.** A salt derived from a stored server
secret plus the date would behave identically in every test — stable within a
day, different across days — while letting anyone holding the secret recompute
last week's salt and brute-force the IPv4 space. So it is generated fresh and
discarded on rotation, and there is deliberately no accessor for a past day's
salt.

**The cost: a restart mints a new salt,** so hashes from before it cannot be
correlated with hashes after it, and one day can over-count a key's distinct
IPs. That is the right direction to be wrong in. Over-counting makes a sharing
signal fire on a legitimate mobile user, which is a human investigating and
dismissing; under-counting is nobody investigating at all.

**`X-Forwarded-For` is client-controlled, and that is the threat.** A caller who
sets it on a directly-received request chooses what gets recorded. For this
signal the dangerous direction is under-counting: an account reselling one key
would pin a single forged address and sit at one distinct IP forever. So the
header is consulted only when the TCP peer is a configured trusted proxy, and
the chain is walked **right to left** — each hop appends the address it received
the connection from, so the caller is the first address that is not itself a
trusted proxy. Walking left to right would stop on the caller's own forged
entry.

**A threshold crossing logs; it does not refuse.** `SHARING_SUSPICION_IPS = 20`
produces one warning at the crossing and nothing else, because a suspicion
threshold is a flag for a human (see §Abuse signals). The server's own salt is
never printed, including in `Debug` output — a salt in a log line is a salt on
disk.

## Abuse signals

What the data actually supports:

| Signal | Computation | Threshold (starting point) |
| --- | --- | --- |
| Key sharing | `distinct_ips` per key per day | >20 domestic-distinct, sustained |
| Volumetric abuse | tokens per account per day | Statistically far above the population |
| Generation-heavy use | output / input ratio | Very high, sustained |
| Repetitive prompts | cache-read ratio | Very high, unusual for the workload |
| Limit circumvention | keys created per account per day | >3 |
| Credential attack | failed logins per IP hash | >10 in an hour |
| Payment abuse | top-up then burn rate | Spend >90% within hours of deposit |

**These are SUSPICION thresholds, not enforcement caps.** The distinction matters:

| Kind | Where | Effect when crossed |
| --- | --- | --- |
| **Hard cap** | `config/apikita.toml` `[limits]` | The request is refused |
| **Suspicion threshold** | This table | Flagged for a human to look at |

For example: **10 keys per account per day is the hard cap**; **more than 3 is the
suspicion threshold.** A customer may legitimately create 5 keys in a day after
hitting a limit — that is a pricing conversation, not an abuse incident.

**Confusing the two is how you either block a real customer or miss a real abuser.**

**Thresholds are starting points, not conclusions.** Every one needs tuning against
real traffic, and a threshold that fires on legitimate use trains you to ignore it.

### Mobile users break naive IP counting

**Indonesian mobile carriers rotate IPs aggressively.** A single legitimate mobile
user can appear as dozens of IPs in a day.

| Context | Expected distinct IPs/day |
| --- | --- |
| Server-side integration | 1-5 |
| Office / stable broadband | 1-3 |
| Mobile user | **10-50** |

**A naive "more than N IPs = sharing" rule will flag most mobile customers.** This is
the single biggest false-positive source in the design. Weight by request volume and
check whether the IPs are from carrier ranges before acting.

## What is deliberately NOT built

| Not built | Why |
| --- | --- |
| Per-user IP history | Surveillance; unjustifiable for a routing business |
| GeoIP location tracking | Same, and inaccurate |
| Device fingerprinting | Same |
| Long-term IP retention | Violates the stated policy |
| Prompt inspection | Never stored |

**The business needs to know *how many*, not *who*.** Every design choice above
serves that.

## Privacy statement consistency

The privacy policy must match this exactly:

| Statement | True because |
| --- | --- |
| We do not store your IP address | Only a salted hash, deleted within 7 days |
| The hash cannot be reversed | Salt is deleted daily |
| We do not track location | No GeoIP, no history |
| We count distinct sources per key | Aggregate only, for abuse detection |

**If any of these stops being true, the policy must change the same day.** These are
the kind of statements that become false through a well-intentioned feature addition.

## Open items

- [ ] Confirm the retention window against any Indonesian obligation.
- [x] Salt storage: where, and how it is guaranteed deleted — process memory,
      never persisted; rotated at the UTC day boundary. See §Implementation.
- [ ] Whether to expose `distinct_ips` to the customer (they may want to see sharing)
- [ ] Threshold tuning process.
- [x] Whether the edge relay or the backend computes the hash — the backend, with
      the relay's `X-Forwarded-For` trusted only from a configured CIDR. See
      §Implementation.
- [ ] The purge BINARY has no scheduler behind it; its WORK runs nightly. These are two
      different statements and only the second is a privacy property.
      **ENFORCED TODAY.** `run_retention` in `.docker/maintenance/entrypoint.sh` applies
      both windows through `sqlite3` on every scheduled run:
      `DELETE FROM key_ip_seen WHERE day <= today - 7` and
      `DELETE FROM key_ip_daily WHERE day <= today - 90`. A delete that does not run is
      reported as `job retention: FAILED`, so a missed sweep is loud rather than silent.
      **NOT ENFORCED.** The standalone `server/src/bin/ip-purge.rs` is not shipped in the
      server image, so an operator cannot trigger a one-off purge on demand without a Rust
      toolchain. That is a convenience, not a retention gap - the windows above hold.
      *(Correction: this item once stated that no nightly job ran the purge and that it
      was still pending. That was true when written and went false when the retention
      sweep took the work inline - the "well-intentioned feature addition" the paragraph
      above this list warns about. It is corrected rather than deleted because the
      distinction it now draws is the one a reader of a privacy document actually needs.)*