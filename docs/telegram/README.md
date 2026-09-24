# Telegram — Channel and Bot

The Telegram surface: a channel with discussion rooms, plus a bot.

> **Decision: payments via Telegram are DEFERRED.** Top-ups happen on the website
> through Midtrans QRIS only. The bot does not take money. See
> [Why payments are deferred](#why-payments-are-deferred).

## What this actually is

It is **not** "a bot with four commands". It is a **channel** with four rooms,
three of which need no bot logic at all.

| Room | Participants | Bot's role |
| --- | --- | --- |
| **#1 discussion** | customers talk to each other | **None.** The bot must not post here. |
| **#2 updates** | you broadcast | Posts announcements |
| **#3 topup** | bot posts, users read | **The only room with real behaviour** |
| **#4 review** | users submit, stored | Collects and stores reviews |

**Running a bot that chatters in a discussion room is the fastest way to make a
community hate it.** The bot is silent in #1 by design.

## Why payments are deferred

The original plan had the bot collecting top-ups. Deferring it removes a category
of work and risk:

| Removed by deferring | Why it mattered |
| --- | --- |
| Dynamic QRIS generation inside Telegram | A second payment integration to build and verify |
| Webhook reconciliation per surface | Two places money can arrive; harder to audit |
| Refund/dispute handling in chat | Payment disputes are not resolvable in a chat window |
| "I paid but my balance is wrong" in #3 | The highest-volume support ticket a bot can generate |

**The topup room becomes a notification feed instead of a till.** The bot reports
what happened on the website — it never initiates or confirms a payment itself.

If Telegram payments are revisited later, the constraint is: the bot must never be
the authority on a payment. It would link out to the website, and the website's
webhook remains the only thing that credits a balance. See
[`docs/website/04-payments.md`](../website/04-payments.md).

## Room #3 — topup

A **feed of top-up events**, not a payment interface.

### Message format

```
User  : e******a@gmail.com
Topup : IDR 50.000 via QRIS
Thursday, 1 December 2026
```

### Email masking — exactly as specified

**Show the first and last character; replace everything between with six
asterisks.**

```
local-part: keep [0], emit "******", keep [last]
domain:     kept in full
```

| Real email | Posted as |
| --- | --- |
| `endika@gmail.com` | `e******a@gmail.com` |
| `edika@gmail.com` | `e******a@gmail.com` |
| `ea@gmail.com` | `e******a@gmail.com` |
| `e@gmail.com` | `e******@gmail.com` |
| `verylongemailaddress@example.com` | `v******s@example.com` |

**A fixed six asterisks is deliberate, not cosmetic.** A mask that echoed the real
length (`en******ka@gmail.com`) would leak the local-part length, and two
addresses of different lengths would be distinguishable. Constant width removes
that signal.

**Edge cases:**

- **Single-character local part** (`e@gmail.com`) → no last character to show; emit
  `e******@domain`. Do not show the same character twice.
- **No `@`** (malformed or a non-email identifier) → mask the whole value as
  `********`. Never post a raw unparsable string.
- **Two-character local part** (`ea@`) → first and last are both shown, which
  reveals the entire local part. This is inherent to "show first and last" and is
  accepted — a 2-character local part is not meaningfully concealable anyway.

**The domain is not masked.** It is kept so the message reads like an account
reference. That is a deliberate tradeoff: the domain is visible, so a customer on
a rare personal domain is more identifiable. If that becomes a concern, mask the
domain to `g****.com` too — but then the reference is much less useful.

### What is NOT posted

- **No balance.** The earlier draft included it; it is the most sensitive number
  and the least useful to the room.
- **No full email, ever.**
- **No internal ids** (`account_id`, `order_id`) — they are noise to customers and
  useful to an attacker.

### Amount and date formatting

Match the example exactly, because it is already locale-correct for the audience:

| Field | Format | Note |
| --- | --- | --- |
| Amount | `IDR 50.000` | Indonesian thousands separator is a **dot**, not a comma |
| Payment | `via QRIS` | The channel used |
| Date | `Thursday, 1 December 2026` | Spelled-out weekday and month |

**Do not "fix" the dot separator to a comma.** `IDR 50,000` reads as fifty rupiah
to an Indonesian reader. The example is right.

### Timezone — needs a decision

Indonesia spans **WIB (UTC+7), WITA (UTC+8), WIT (UTC+9)**. "Thursday, 1 December
2026" is ambiguous without a zone, and a timestamp near midnight can land on a
different day depending on which is used.

**Recommended: display in WIB (UTC+7)** and store in UTC. State the zone in the
channel description so nobody has to guess. If the customer base is
Jakarta-centric, WIB is the obvious choice.

### When to post — post on SETTLEMENT

`topups.status` moves `pending` → `settled`. **Post only when settled.**

Posting at creation announces a payment that may never complete — a QRIS code that
is never scanned, or a payment that fails. A room advertising top-ups that never
happened destroys trust in the feed.

**Never post failures.** A declined or expired payment announced in a shared room
is both embarrassing and a privacy leak. Failures go to the user's **DM**, if
anywhere.

### Volume

At scale this room is one message per settled top-up.

| Customers | Top-ups/mo (est.) | Messages |
| --- | ---: | ---: |
| 10 | 20 | ~1/day |
| 100 | 200 | ~7/day |
| 200 | 400 | ~13/day |

The break-even target is ~100–200 customers
(see [`docs/business/03-financial-model.md`](../business/03-financial-model.md)),
where the feed is ~7–13 messages a day. **Acceptable** — this is what the room is
for. Revisit only if it becomes unreadable.

### Room access

If this room is **public**, a masked email is still personally identifiable, and
the feed reveals how much a specific customer spends. Restrict the room to
authenticated customers, or accept that the feed is public.

**Recommended: customers only.** The masking protects against casual exposure, not
against someone who knows the first and last letter of a customer's address.

### The user's own notifications

The room is the **feed**. Anything specific to one user — low balance, a failed
payment, a successful top-up they initiated — also goes to their **DM**. Do not
rely on the room to inform a user about their own money.

## Room #4 — review

One review per user, editable. **The bot is the only way to write a review.**

| Action | Telegram | Website |
| --- | --- | --- |
| Write / edit a review | **Yes** | **No** |
| Withdraw | **Yes** | No |
| Read an aggregate | Yes | Yes (read-only) |
| Read one's own review | Yes (`/review show`) | No |

**The website cannot create or edit a review.** It may display the aggregate only.
Reviews are the Telegram channel's contribution, and allowing a second writer makes
"who reviewed" ambiguous. The API returns `403` if a browser session tries to
post one — see [`docs/server/api-spec.md`](../server/api-spec.md).

## Commands

| Command | Effect |
| --- | --- |
| `/review` | Start or edit. If a review exists, it edits that one. |
| `/review show` | Show the caller's current review |
| `/review withdraw` | Mark it withdrawn (does not delete) |
| `/review history` | The caller's own edit history |

**`/review` is the same command for create and edit.** A separate `/edit` is an
extra thing to learn, and the user does not care which one they are doing — they
want to end up with the review they want.

## Conversation

The bot asks one question at a time. Two messages, no inline keyboard needed.

```
User:  /review

Bot:   Rate us 1-5. Reply with a number.

User:  5

Bot:   Got it. Add a comment? Reply with text, or send /skip.

User:  fast and cheap, using it daily

```

Then the bot confirms and stores:

```
⭐ Review saved

Rating  : 5/5
Comment : fast and cheap, using it daily

Changed your mind later? Send /review again.
```

### Edit flow

```
User:  /review

Bot:   You rated us 5/5 on 1 Dec 2026.
       "fast and cheap, using it daily"

       Edit it? Reply with a new rating (1-5), or /cancel.

User:  4

Bot:   Updated. Add a new comment? Reply with text, /skip to keep the old one.
```

**Defaults on edit preserve what the user does not change.** Sending `/skip` keeps
the existing comment. Forcing them to retype it is the difference between an edit
and a punishment.

## Validation

| Input | Bot's response |
| --- | --- |
| Not 1-5 | "Rate us 1-5." Re-ask. Do not accept "great" or "4/5" |
| Comment over the cap | "Keep it under 1000 characters." Re-ask. Do not silently truncate |
| `/cancel` at any point | Abandon, change nothing |
| No response for 10 minutes | Drop the conversation; the next `/review` starts fresh |

**Never silently truncate a comment.** Truncation loses meaning at the end of a
sentence, and the user believes their full text was stored.

## State

A review is a short multi-step conversation, so the bot needs per-user state:

-- Schema: docs/website/02-data-model.md (single source of truth)
-- review_sessions: telegram_id, step, rating, body, editing, expires_at

**Staged values are not the review.** Nothing is written to `reviews` until the
flow completes. Abandoning halfway must leave the existing review untouched.

## Who may review

**Anyone in the channel may submit.** The bot records whether they are a customer,
rather than blocking non-customers.

| Submitter | `is_customer` | Displayed as |
| --- | --- | --- |
| Has a settled top-up | `true` | "Verified customer" |
| No purchase | `false` | "Unverified" |

**`is_customer` is computed server-side** from whether a settled top-up exists.
Never accept the flag from the client, and never let the bot claim verification it
did not check.

Blocking non-customers outright loses real feedback and is hard to explain. Labeling
keeps the signal while staying honest about its source.

## Keyed on the account, not the Telegram id

The review must survive linking and unlinking. So:

- Store `account_id`, not just `telegram_id`.
- A review submitted before linking is held against `telegram_id` and
  **re-attributed on link, atomically** — see
  [02-data-model.md](../website/02-data-model.md) for the constraint gap this closes.
- "One per user" is enforced on `account_id` once linked, `telegram_id` before.

## Editable, with history

- `/review` creates or **edits in place** — one row, never a second.
- Before overwriting, copy the old values into `review_history` **in the same
  transaction**.
- An edit from 5 stars to 1 is a signal. Overwriting destroys it.

## Withdrawal

`/review withdraw` sets `withdrawn_at`. It does **not** delete the row.

- A deleted row would free the unique slot, letting the user submit a second
  review — breaking "one review per user".
- Withdrawn reviews are excluded from the public aggregate but remain visible to
  the operator.
- The user can un-withdraw by submitting again.

## Public or private

**The bot does not post individual reviews to the room.**

- Publishing a username next to a negative review invites retaliation.
- **Store all; publish only the aggregate** — "4.6 average from 23 customers".
- Ask before publishing an individual review, even a positive one.

## Content

| Field | Rule |
| --- | --- |
| Rating | 1-5, required. Makes the set aggregatable |
| Comment | Optional, max 1000 chars |

Unbounded free text in a bot invites spam, and you are the moderator. A rating plus
short text stays manageable.

## Rate limiting

- Cap `/review` starts per user per hour. A user spamming edits is either
  confused or abusing the history table.
- Cap total reviews per Telegram account per day — one review, many edits is
  legitimate; 50 submissions is not.

## Room #2 — updates

Broadcast only. **You can post these manually** — a bot is optional here.

Build the bot path only if you want scheduled or automated announcements. The
cheapest correct implementation is a pinned message you edit.

## Room #1 — discussion

**No bot behaviour.** It exists so customers can talk to each other and to you.

The bot's only relationship with this room is that it does **not** post in it.

## Link codes

The bot **does** need to handle `/link`. Full flow and rate-limit requirements:
[`docs/architecture/identity.md`](../architecture/identity.md) §Telegram linking.

**Redemption must be rate-limited per Telegram user and per IP.** A 6-digit code is
brute-forceable, and a guess attaches an attacker's Telegram to a funded wallet.
This is the highest-risk endpoint in the Telegram surface.

## Data needed

**The schema lives in one place:** [`docs/website/02-data-model.md`](../website/02-data-model.md).
It is not duplicated here — a second copy drifts, and a drifted schema is a bug.

| Table | Purpose |
| --- | --- |
| `telegram_links` | `telegram_id` → account |
| `link_codes` | Short-lived `/link` codes |
| `reviews` | One per user; `withdrawn_at` instead of deletion |
| `review_history` | Prior values on each edit |
| `review_sessions` | Transient bot conversation state |

**Two things in that schema matter for the bot:**

1. **`reviews` is keyed on the account**, with a partial unique index on
   `telegram_id` for pre-link submissions. Re-attribution on link must be
   atomic, or the same person ends up with two review rows — the indexes sit on
   different columns and cannot catch that themselves.
2. **`withdrawn_at` is a flag, not a delete.** Deleting would free the unique
   slot and let a user submit twice.

## Bot stack

Undecided, and **low-risk to defer**. The bot is a thin client over
[`docs/server/api-spec.md`](../server/api-spec.md) — it holds only its own token and
never touches the database directly.

## Open items

**Decided:**

- [x] Payments via Telegram — **deferred**. Website only.
- [x] Top-up feed identity — **mask email, first and last char, six asterisks**.
- [x] Balances in the room — **not posted**.
- [x] Post timing — **on settlement only**; failures go to DM.

**Still open:**

- [x] Timezone: **WIB (UTC+7)**, stored UTC.
- [x] Room access: **customers only** — masking hides identity, not spend amounts.
- [x] Review gating: **open, labelled `is_customer`**.
- [x] Review publication: **aggregate only**.
- [ ] Bot language/runtime (deliberately deferred).
- [ ] Moderation policy for review text.
- [x] Low-balance DM: **below 10,000 IDR, max 1/day**.