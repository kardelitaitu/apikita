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
| `key_ip_daily` counts | 90 days | Trend without history |
| Daily salt | **deleted after the day** | Makes the hashes unlinkable forever |

**Deleting the salt is what makes this honest.** Even with the hashes, nobody —
including you — can recover the addresses after the salt is gone.

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
- [ ] Salt storage: where, and how it is guaranteed deleted.
- [ ] Whether to expose `distinct_ips` to the customer (they may want to see sharing)
- [ ] Threshold tuning process.
- [ ] Whether the edge relay or the backend computes the hash (relay sees the real
      client IP behind Cloudflare; the backend sees the relay)