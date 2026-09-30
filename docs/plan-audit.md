# Plan Audit

A deliberate review of the whole plan, asking whether it holds together rather than
whether it is internally consistent. **The consistency checks pass** (332 links, no
dead anchors, arithmetic reproduces). This document records the problems that
consistency checks *cannot* find.

## What I found

### 1. The business case closes, but only for some workloads

At M = 2.00, 20M tokens/month:

| Workload | Contribution | Verdict |
| --- | ---: | --- |
| Chat-shaped | **+55,175 IDR** | Comfortable |
| Mixed | **+10,678 IDR** | Thin but positive |
| Cache-heavy | **−14,101 IDR** | **Loss-making at any markup** |

**This is now correctly recorded** in [`business/03-financial-model.md`](business/03-financial-model.md).
It was *not* correctly recorded before this audit — the model contained two
contradictory tables from different rate bases.

### 2. Nothing in the plan segments customers by workload

**This is the most serious gap.** The economics depend on workload shape more than
on volume or spend, but every control the plan has — deposit floors, spend limits,
rate limits — filters on *amount consumed*, not *shape of usage*.

A customer running long-context RAG at 20M tokens/month is a liability. A customer
running chat at the same 20M tokens/month is profitable. **The plan cannot tell them
apart, and currently admits both.**

**Consequence:** the business could hit a healthy-looking customer count while
losing money on an increasing share of it, with no metric that surfaces it.

**What would fix it:** track output-token share per account (already in `usage_daily`),
surface the cache-read ratio as an operational metric, and decide a policy *before*
acquiring cache-heavy users on volume alone. Options: a volume floor that scales
with cache ratio, separate cache-read pricing, or accepting the loss as a
loss-leader. **This is an open decision, not a documentation gap.**

### 3. The decisive assumption is still unmeasured

Support cost per customer — assumed at 25,000 IDR/month — decides viability outright:

| Support cost | Mixed-workload contribution | Break-even customers |
| ---: | ---: | ---: |
| 5,000 IDR | +30,678 | **~98** |
| 25,000 IDR | +10,678 | **~281** |
| 50,000 IDR | **−14,322** | **never** |

**A 5x error in this single number moves break-even from ~100 customers to
unreachable.** It is an assumption with no data behind it. It should be measured
with the first ten customers, before any spend on growth.

### 4. Infrastructure cost is assumed, not priced

3,000,000 IDR/month (~$165) is assumed across Northflank, a VPS relay, Cloudflare,
and CI. **No quotes have been obtained.** At $300 actual, break-even roughly doubles.

### 5. Six moving parts, one operator

> **Since this audit:** the identity port landed. PocketBase is gone — identity is
> served by the Rust API itself in the embedded SQLite database, so there is no
> separate auth system to run. The count below is updated; the finding is not.
> See [`architecture/identity.md`](architecture/identity.md).

| Component | Why it exists |
| --- | --- |
| Rust API + proxy | The product, and now identity too |
| SQLite (embedded) | Money and identity |
| VPS relay | Flood absorption, cost |
| Cloudflare Pages | Frontend |
| Cloudflare edge | DDoS, DNS, TLS |
| CI | Correctness |

Each is individually justified. **Collectively they are the operational surface one
person must run**, for a product whose margin is ~10,000 IDR on a mixed-workload
customer. The objective is *low cost server*; complexity is a cost even when each
component is cheap.

**Candidates to defer at launch:** the edge relay (Cloudflare already absorbs
volumetric attacks). The PocketBase hybrid was once the other candidate — a second
system to back up and reconcile — and it is no longer a choice: the identity port
folded it into the API, which is why the surface above is six and not seven.

### 6. The whitepaper remains misleading

It still describes a system that charges a flat rate and cuts streams mid-answer,
and its pricing figures are ~2.4x low. It is labelled as superseded at the top, but
it is 146 lines of authoritative-looking detail. **Consider deleting it** rather than
keeping a corrected-in-footnotes version.

## What is genuinely solid

- **The money model.** Append-only ledger, unique `order_id`, `CHECK` on balance,
  reconciliation query. This is the strongest part of the plan.
- **The API contract.** 30+ endpoints with error codes, enforcement order, and
  idempotency rules.
- **The schema.** 15 tables, parser-validated, index coverage mapped.
- **Operational docs.** Deployment, backup drill, abuse runbook, admin audit.
- **The consistency discipline.** Decisions single-sourced, schema single-sourced,
  open items single-sourced.

## The honest summary

**The plan is internally consistent and unusually well documented. Its risks are
commercial, not technical.**

| Risk | Severity | Status |
| --- | --- | --- |
| Unmeasured support cost | **High** | Assumption; measure at 10 customers |
| No workload segmentation | **High** | **Open decision**, schema supports the data |
| Unpriced infrastructure | Medium | Get quotes before launch |
| Operational surface vs one operator | Medium | Consider deferring the relay |
| Whitepaper misleads | Low | Delete or clearly quarantine |

**The technical plan is buildable. Whether it makes money depends on a number nobody
has measured and a customer mix nobody is filtering.**
