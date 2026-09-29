# Testing this repository

What the suite is for, what it actually checks, and - the part that took longest
to learn - **what it cannot check**. That last section is not a disclaimer. Three
ideas for an automatic guard in this repository were built and measured to death,
and the measurements are here so nobody builds them again.

Run everything with `cd server && cargo test --workspace` (the website has its
own: `cd website && npm test`).

## The rule that produces most of the real defects

**A duplicate rule drifts, and it drifts toward the weaker reading.** Not
differently - weaker. Every instance found in this repository behaved the same
way, and that is not a coincidence worth ignoring:

| What was duplicated | What the copy did |
| --- | --- |
| `resolve_account_from_cookie` (three copies) | skipped the idle bound, so an 8-day-idle session was refused by `/api/*` and accepted everywhere else |
| Session token hash (six copies) | identical, so nothing was wrong until one was edited |
| API key hash (two copies) | `keys.rs` wrote the stored value and `proxy.rs` computed the compared one, from different functions |
| `today_utc()` (eighteen call sites) | re-derived inline, so a correction to the one definition would have split the salt boundary from the counts |

The fix is always the same: **find the one definition, point everything at it,
and record that the guarantee is now structural rather than assumed.** That last
part matters. A comment saying "one definition exists" while eighteen sites
bypass it is worse than no comment.

## The four kinds of claim a test can check

A test that pins a *stated* rule is the highest-value thing in this suite, and
there are four shapes of it, all in `server/src/doc_claims.rs`,
`server/src/doc_schema.rs` and the test modules beside the code:

- **document to code** - a retention period or a session lifetime that the config
  holds. Changing either alone fails.
- **document to document** - the IP retention policy is published twice, and a
  promise that differs between the page a customer reads and the page an
  operator reads is not a promise.
- **document to tree** - `telegram/` holds its README and nothing else. This is
  the only one that goes stale by someone doing **the next piece of work**, and
  its failure is a nudge that says what to update.
- **document to source** - "the Midtrans server key is never logged" is a claim
  about code, and it is the only kind that catches a secret reaching a
  `tracing` macro.

Plus one against the **schema**: `usage_events` must have exactly its current
column set, because a `prompt TEXT` column added for debugging would make the
privacy page's central claim - *we are a proxy, not a data processor* - false,
and false **silently**.

## What a test here cannot check, and why

Three proposals were built, measured, and rejected. The numbers are the point.

| Proposed check | Measured | Verdict |
| --- | --- | --- |
| Every `pub fn` has a production call site | 37 of 137 have none, **all false positives** | axum route handlers, which the ROUTER calls by path. Would need an exception list that is really a list of every endpoint in the product. |
| A comment's quoted text must sit near the line it cites | 4 candidates crate-wide, **0 misplaced** | measured, nothing left to catch, not built |
| A production function name appears in only one place | 251 scanned, **13 collisions, 0 real** | `new` ×8, `main` ×6, `IntoResponse`/`fmt`/`drop` trait impls, per-type methods. All legitimate. |

**The common finding: name- and shape-based checks cannot see the duplication
this repository actually had.** The dangerous copies had *different bodies* -
three session resolvers, each a little weaker than the original - so a
duplicate-body check would have caught only the hash copies, and a duplicate-name
check catches none of the four. What separates real duplication from those 13
false positives is **whether the copies are meant to be the same rule**, and no
source-level test can make that judgement.

So: automated checking here is excellent at pinning a stated rule to a stated
instant, and blind to two rules quietly disagreeing. The first is a test. The
second is a review.

### One duplication that cannot be tested at all, and currently agrees

The stranded-hold detector - a money invariant, because a stranded hold is
invisible money - exists in **two languages**: the query in
`server/src/bin/hold-sweep.rs` and an equivalent one in
`.docker/maintenance/entrypoint.sh`, because the scheduler image does not ship the
Rust binary. Both comments insist the predicate is the same, and both are right
to: identical `ref LIKE 'reserve_%' AND delta_idr < 0`, grouped by account and ref,
with no positive row under the same ref. They differ only in what the caller does
next - the binary joins `accounts` to name the customer it may credit, and the
scheduler applies an age bound so it does not report a hold still inside its
settlement window. **Those are different jobs, and the difference is right.**

Nothing tests that they agree, and nothing cheaply could: one is a Rust string
constant, the other a shell heredoc. Both are exercised - CI seeds a stranded
hold and asserts the scheduler's sweep fires, and the binary has its own tests -
but each is tested alone, which is precisely the blind spot above. A drift here
would mean the nightly job silently reporting nothing while the host binary still
works, or the reverse.

It is recorded here so the next reader does not assume the agreement is covered
by a test. It is currently true; it is true by reading, not by assertion.

## Three habits that decide whether any of it works

**Mutation-verify a guard.** A check that has never been seen to fire is a check
nobody trusts - and some are shipped with zero current targets deliberately
(the `telegram/` one, the secret-log one). Both are mutation-verified, which is
the only thing that makes "nothing is wrong right now" a statement about the
guard rather than about the day.

**A mutation that passes tells you what the test does not cover.** Two examples
from this suite, both worth the time: a guard meant to catch an under-reserving
hold passed a mutation that halved the price above a million tokens, because a
ladder spanning seven orders of magnitude cannot see a rate change - the
property it pinned was monotonicity in the token count, and the fix was to derive
the real bound. A test for mid-stream revocation passed a mutation that set
`revoked_at` a year in the future, because that is a state no code produces;
it had tested an impossible state instead of the real failure.

**A test of a shared function cannot see a caller that does not use it.** This is
the blind spot that let three weaker session resolvers survive with every test
green: `routes/mod.rs` pinned the idle bound by calling the resolver
*directly*, so it passed whatever the handlers did. The test that matters is the
one at the **handler**, driving the route a request actually takes.

**Guards written for this class found a live instance of it on the first run — twice.**
Closing the mount/spec chain found the api-spec test that checked its routes were
unmounted and had never opened the spec. Reading the error status table found a
documented-and-served code missing from the hand copy. Neither was hypothetical.

That is worth weighing when deciding what to close next: the chains that had gone
unverified for the longest are the ones most likely to have drifted quietly, and
"no test covers it" is not evidence that there is nothing to find.

### A hand-kept copy is not always the same bug

The guards above all read the file they describe rather than keeping a copy. That
rule is right for a **definition** and wrong for a **fixture**, and the difference
is worth stating before someone "fixes" the wrong one.

- A copy that **claims to BE the published contract** must be read from it. The
  `DOCUMENTED` table in `error.rs` said any drift would fail a test; it was true
  of the code half and false of the document half, and the document had fifteen
  rows to the copy's fourteen. That was a real defect.
- A copy used as a **fixture** is a tripwire, and reading it from the file would
  destroy it. `money.rs` transcribes the shipped rates as literals *on purpose*:
  a config edit that silently changes what a customer is billed should **break a
  test**, not quietly rewrite the expected value. Pin those constants to the
  config and the pricing tests become tautological - they would assert whatever
  the config says, which is the thing that is supposed to be under test.

So the question is not "is this a copy" but **"what happens if the other file
changes"**. For a definition, a copy that drifts is a lie. For a fixture, a copy
that follows is a test that has stopped testing.

## The vacuity guard, applied everywhere

A check that silently matches nothing passes over an empty set and reports a
clean sheet. So every guard here carries one:

- the citation check asserts the pattern finds something;
- the secret-log scan asserts it saw more than 100 macro calls, because a scan
  that finds almost nothing is not a scan;
- the column reader is tested on `reviews`, whose nested `CHECK` constraint
  stops a naive scan at the first closing parenthesis and silently reports fewer
  columns than the table has;
- `data-retention.md` and `telegram/README.md` are read with `expect`, so a
  path that resolves to nothing is an error rather than an empty document that
  satisfies every assertion.

## When you add a rule

Extend the existing guard rather than adding a second copy of the rule, and
extend it in **both directions** - a check that only confirms the document
mentions a number is also satisfied by changing the constant alone.

If the rule is stated in a document, the guard should read the document. If it
is stated in a comment, the guard should read the thing the comment is about.
A claim that lives only in prose is a claim that will be true until the day
someone edits the prose.
