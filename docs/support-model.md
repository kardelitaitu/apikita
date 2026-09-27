# Support Model

**This document was missing.** Support cost per customer is cited as the decisive
input in the financial model and in 30+ places across the plan — but nothing
specified *how support is actually delivered*, which is what determines that cost.

> Cost side: [`business/03-financial-model.md`](business/03-financial-model.md). Channels:
> [`telegram/README.md`](telegram/README.md). Risk: [`business/05-risk.md`](business/05-risk.md) R4.

## The strategy: peer support in the Telegram channel

**The community is the first line of support, not you.** Telegram rooms exist so
customers help each other, and every question answered by a peer is a ticket you do
not handle.

This is the single highest-leverage decision available, because infrastructure cost
is small and mostly fixed while support scales *per customer*.

## What it buys

At a mixed-workload customer contributing ~38,000 IDR of gross margin:

| Support model | Effective cost/customer | Break-even customers |
| --- | ---: | ---: |
| All on you | 25,000 IDR | **281** |
| Half deflected by peers | 12,500 IDR | **130** |
| Mostly deflected (~80%) | 5,000 IDR | **98** |

**Halving the support burden roughly halves the break-even customer count.** That is
a bigger lever than any pricing change.

## What it does NOT do

**Peer support does not make the cost zero — it moves it.** The costs it introduces:

| New cost | Why it matters |
| --- | --- |
| **Wrong answers** | A peer gives bad advice; you still get the ticket, now worse and later |
| **Unanswered questions** | No reply means the ticket reaches you anyway, with delay and frustration |
| **Moderation** | Someone must watch the rooms, even lightly |
| **Attribution** | A wrong answer in your channel is attributed to **you**, not the peer |
| **Slower resolution** | Waiting an hour for a peer vs five minutes from you |

**Net deflection is the metric, not questions asked.** A room with 100 questions and
60 peer-answered correctly is a win; a room with 100 questions and 10 answers is a
second inbox.

## Which rooms do what

| Room | Support role |
| --- | --- |
| **#1 discussion** | **The support surface.** Users ask, users answer |
| #2 updates | Announcements; you post, nobody replies |
| #3 topup | Feed only — **not** a support channel |
| #4 review | Bot-collected; not support |

**#3 is the trap.** A top-up feed invites "my balance is wrong" replies. That needs
to be redirected, not answered there, or the feed becomes an inbox.

## What makes deflection actually work

Peer support only reduces *your* load if the answers already exist. In priority order:

1. **Documentation that answers the common questions.** Every question asked twice
   should become a doc page. This is the compounding asset.
2. **Error messages that explain the fix.** "Insufficient balance — top up" deflects
   a ticket. "Error 402" creates one.
3. **The status banner.** "Upstream degraded" prevents "is it down?" for everyone
   at once.
4. **A searchable channel history.** A room with no pinned FAQ generates the same
   question weekly.

**The plan already requires (2) and (3)** — see [`error-model.md`](error-model.md) and
[`observability.md`](observability.md). They should be understood as support
infrastructure, not just UX.

## What still needs you

Deflection has a floor. Some categories cannot be delegated:

| Category | Why it stays with you |
| --- | --- |
| **Billing disputes** | Money; requires authority |
| **Account takeover / security** | Requires access |
| **Abuse reports** | Requires judgement and action |
| **Refunds and adjustments** | Admin surface, audited |
| **Upstream outages** | Only you can act |

**Estimate the floor as 20-30% of support load**, not zero. That is why the table
above stops at ~80% deflection rather than 100%.

## Risks of this strategy

| Risk | Mitigation |
| --- | --- |
| **Nobody answers** | You must seed answers initially; an empty room stays empty |
| **Wrong answers persist** | Pin correct answers; correct publicly when needed |
| **A few users do all the work** | Recognise them; they are unpaid staff and will burn out |
| **Channel becomes hostile** | Moderation policy and a willingness to remove people |
| **Competitors watch the rooms** | They learn your pricing and pain points. Accept it |

**"Nobody answers" is the failure mode to plan for.** Launching a community that
nobody uses is worse than no community, because it signals nobody is home.

## How to measure it

The financial model demands this number. Instrument it from the first customer:

| Metric | How |
| --- | --- |
| Questions per customer per month | Count support-relevant messages |
| **Deflection rate** | peer-answered / total asked |
| **Your time spent** | A simple log; minutes per ticket |
| Support cost in IDR | minutes x your hourly value / active customers |
| Top question categories | Drives what to document next |

**Revisit the pricing decision once this is measured.** The cache-heavy question
(`cache-pricing-options.md` ) may resolve itself: at a 5,000 IDR support cost the
agent workload is already positive, and no cache multiplier is needed.

## The strategic read

This reframes the business. It is **not** an infrastructure business — servers are
small and fixed. **It is a question of how much human attention each customer needs.**

That makes the community a core asset rather than a nice-to-have, and it makes
anything that reduces support load worth more than any server optimisation.

## Open items

- [ ] Who answers first in an empty room? Decide the seeding approach.
 - [ ] Moderation policy for the discussion room (currently undefined).
- [ ] Whether to pin an FAQ, and who keeps it current.
- [ ] How support time is logged, and by whom.
- [ ] Recognise helpful users — how, and does it cost anything.
