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

### One link in the route chain has no check, and a naive one is wrong

The API surface is guarded in three directions: every route `MOUNTED` lists is
mounted by `create_router` (the table test drives the real router, not a test app),
every route `MOUNTED` lists is in the spec, and every route the spec presents as
built is mounted. What is **not** checked is the first link going the other way: a
route added to `create_router` and forgotten in `MOUNTED` would escape all three,
because every check starts from the table rather than the router.

It is empty today — the two agree. It is recorded because closing it is a trap,
and one I fell into while writing this. A source-level parse of `create_router` looks
trivial and is not: several `.route(` calls put the path on the NEXT line, so a
regex of the form `route("PATH"` matches twenty of twenty-six and reports
`/webhooks/midtrans` — the Midtrans webhook — as unmounted. It is mounted, and
`webhooks.rs` has a test that routes it.

axum does not expose its route table, so a sound check cannot be written by
scraping the source. The honest options are to enumerate the routes in a macro or a
constant that `create_router` itself consumes, or to leave the link unverified and
**say so**, which is what this paragraph is. A check built on the regex would have
reported a missing payment webhook, and that is worse than no check: it is a false
alarm on the one route that moves money.

### Four false alarms, and what they had in common

Every one of these rounds has produced a finding that turned out, on checking, not
to be one. A regex that reported the Midtrans webhook unmounted - the parse missed
multi-line route calls. A hand-inserted fixture the STRICT schema refused on its
reason vocabulary. A rewrite that panicked on behaviour the code documents as
correct. And an api-spec that appeared to advertise four unbuilt endpoints in the
present tense: it does not. They carry a warning marker, the marker is **defined**
four lines below the table, and the count matches the code SPEC_ONLY list exactly.

The fourth is the instructive one. The review section gives a full request contract,
and reading that section alone suggests a live endpoint. The legend is at the top
of the document and the contract is at the bottom, and I read one and not the other.

**A document misleads in parts less often than a reader assumes.** Before reporting
that a document disagrees with the code, read the part of it that would have
explained the thing you found: the legend, the note, the status column, the count. A
false alarm costs more than a missed one here, because the reader either has to
re-check it or learns that findings in this repository are noisy - and a repository
where findings are noisy is one where the real ones stop being trusted.

### The never-mentioned measurement, and its one blind spot

The technique that found the fictional 422 example is simple: list the names the
schema declares, or the keys the config file sets, and report the ones no source file
mentions. It is cheap, it is mechanical, and it has found real defects that reading
the document in question did not.

It is also a LOWER BOUND, and the reason is worth writing down so nobody trusts it
for more than it is worth. A name that is mentioned is not necessarily a name that is
used. `max_context_tokens` appears seven times in the crate - as a struct field on two
config types, in three test fixtures, and in the parser key list - and is read by
nothing. Every one of those seven is a declaration, a literal, or a string, and none is
an expression that affects behaviour.

So this measurement cannot find PARSED BUT UNREAD configuration, which is a real
category here and the one most likely to be mistaken for a working setting by whoever
put it in the file. Distinguishing a field declaration from a field read needs real
analysis rather than a name search: count the occurrences, and a field that changes
behaviour will appear in a comparison or an arithmetic, not only in a struct literal.

That is a heuristic and is described as one. Doing it properly means following uses of
a field rather than searching for its name, which is what a call graph is for. Until
then the honest statement is: this catches what is entirely unreferenced, and a
configuration key that is referenced in a struct and nowhere else still looks healthy
to it.

### Six guards that passed when the thing they checked was wrong

A guard is proven by CHANGING the thing it checks. A green guard proves nothing on
its own, and six in this repository were green against defects:

| The guard compared | Against | Found by |
| --- | --- | --- |
| a word list | the privacy page's own sentence | adding `raw_text` to the claim |
| `**90 days**` | the document's markdown | unbolding a cell |
| a shared decode helper | nothing - it was simply assumed | seeding a DATE-keyed row |
| one straddling seed | the other table's window | swapping one constant for a neighbour's |
| a hand-kept `SWEEP` list | the entrypoint that enforces it | 30 to 45 in the script |
| two constants | a document it never opened | renaming the test to what it did |

The last is the clearest and the worst. `the_retention_windows_match_the_privacy_statement`
read no privacy statement: its whole body was `assert_eq!(SEEN_RETENTION_DAYS, 7)` and
`assert_eq!(DAILY_RETENTION_DAYS, 90)` - a hand-kept copy of two numbers compared to two
constants. The name promised a document the body never touched.

**WHAT ALL SIX HAVE IN COMMON IS A RESTATEMENT.** Each one compared a COPY of the thing
to a source, and a copy that is correct says nothing about the original. The fix is
always the same shape and it is not a bigger test: read the ORIGINAL. Parse the number
out of the script, open the page, find the row by its own text. The moment a check holds
a literal that restates something, the literal is the weak part and the check is theatre.

**THE TEST THAT FINDS IT.** Change the thing, run the guard, and read the result. That is
five minutes, and it found six defects that no amount of reading had. The corollary
matters just as much: a mutation that BREAKS THE BUILD has shown the wiring is
load-bearing, which is weaker evidence than an assertion failing - a compile error says
this name matters, a failed assertion says this value does.

### Where this rule stops, and why it stops there

The log guard checks the SEVENTH restatement, and it works because `logged`, `logs` and
`warns` are unambiguous: a test with one of those in its name is claiming to observe a
log. The obvious next step was to extend the same idea to COUNT claims - `once`,
`exactly`, `only` - and the measurement says not to.

Seventeen test names in the crate contain `once` or `exactly`. Most of them are not count
claims at all: `returns exactly the token the response carried`, `bills exactly what the
upstream reported`, `the_error_frame_is_exactly_one_event_line_then_one_data_line` are
adverbs of PRECISION, not assertions about a number of occurrences. A guard that could not
tell those apart would be either vacuous - matching nothing - or noisy, and this suite has
already produced enough false alarms that the bar for a matcher is that it is quiet.

**The word is only checkable where the claim it makes is unambiguous.** That is the same
condition as the log guard having to rely on `capture_logs` existing: a rule about
over-claiming is only enforceable where the claim could have been true in the first place.

Spot-checked rather than assumed. The one test that genuinely claims a count in a money
path - `live_webhook_settlement_credits_exactly_once_and_a_replay_does_not` - does
measure it: it asserts the ledger row count is 1 after the first webhook and 0 after each
replay. The pattern holds where it matters; it simply is not machine-checkable by name.

### Testing a guard by deleting the wrong file

A vacuity check asks whether a guard passes when the thing it guards is absent. The easy
way to get that wrong is to delete a file the guard was never reading.

`ci-docs-check` passed with `docs/data-retention.md` missing, which looked like a fifth
vacuous pass. It is not one: that check reads exactly one file, `docs/ci-cd.md`, and
compares the CI workflow steps against it. Deleting the retention document is outside its
subject entirely, so a pass was the only correct answer. Removing the file it actually
reads fails it with exit 3.

The same holds for the others: `reconcile-check` fails with `reconcile.sql` missing, and
`alert-check` fails with `probe.sh` missing. **All three fail loudly on their own input.**

**THE DISCIPLINE, WHICH IS THE WHOLE POINT.** Before concluding a check is vacuous, ask
whether it would have read that file at all. A check that passes because its subject was
never in scope is not broken - and a finding built on deleting the wrong file is a false
alarm, which costs the reader more than the missed defect would have. This is the fifth
false alarm recorded here, and the same shape as the rest: the defect was in the
measurement, not in the code.

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
