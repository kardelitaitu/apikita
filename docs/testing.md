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

### The link in the route chain that had no check, and why the naive one is wrong

The API surface is guarded in three directions: every route `MOUNTED` lists is
mounted by `create_router` (the table test drives the real router, not a test app),
every route `MOUNTED` lists is in the spec, and every route the spec presents as
built is mounted. What is **not** checked is the first link going the other way: a
route added to `create_router` and forgotten in `MOUNTED` would escape all three,
because every check starts from the table rather than the router.

It was empty when this paragraph was written; the two agreed. **It is closed now,
and it was not empty when it was closed.** The route-inventory check
(`the_route_inventory_matches_the_mounted_table`, in `server/src/routes/mod.rs`)
found **six** routes that were mounted by `create_router`, documented in
`docs/server/api-spec.md`, and absent from `MOUNTED`: `GET /api/admin/metrics`,
`GET /api/usage/recent`, `GET /api/export`, `GET /api/admin/accounts`,
`GET /api/admin/audit` and `GET /api/admin/accounts/{id}/audit`. Nothing had
failed, because every check begins at the table. The module comment above
`MOUNTED` had also said "13 route calls, 15 rows" against a router mounting 26
calls; it had been wrong for long enough that nobody was reading it.

How the link is closed, given that axum does not expose its route table:
`create_router` no longer writes its paths inline. It expands a `routes!` macro
over `pub const ROUTES`, a raw-string literal of `.route("PATH", expr);` lines,
and the **path literals in that inventory are the same strings the router is built
from** — so the test reads the bytes the router consumed. Each line ends in `);`
rather than `)` because `rustfmt` never emits the semicolon, which makes a line
rewritten by a formatter detectable. The check compares the inventory to `MOUNTED`
in both directions, carries a positive control (`/api/bot/link` must be found, or
the parse is vacuous), and pins both counts.

Two costs are stated in the code rather than hidden. The method-router
*expressions* are still read by no test — the inventory check reads only paths, and
axum offers no reflection — so deleting a `.post(...)` from a chain is caught by
nothing. And these lines are format-sensitive: one `cargo fmt` run re-wraps them,
and a re-wrap can swallow a semicolon and with it a route.

This section used to deny that the crate had a format gate in CI at all, and that was
wrong in two ways at once. The step has been in `ci.yml` since the workflow was
added. And the danger is *narrower than the warning implied but has a firmer floor*.
Measured against `rustfmt 1.9.0-stable`, a full `cargo fmt` over `routes/mod.rs`
**preserved all 231 `);`-terminated lines** and changed only indentation. And a
swallowed semicolon is not a silent edit at all: removing one by hand turns the
`routes!` macro inside out and the crate fails to build with `error: no rules
expected `.``. There is no state in which a route disappears from the inventory while
the router keeps serving it, because the literal and the router are expanded from
the same bytes by the same macro.

So the semicolon convention is load-bearing for a different reason than the one
first written here: it makes a hand-deleted line *visible in review*, and it fails
loudly under a formatter rather than quietly. The guard that catches an inventory
that drifts while still compiling is the inventory test, which compares the literal
to `MOUNTED` in both directions and pins both counts. What the format step does
catch, and did on its first honest run, is everything else: the block above
`.fallback(unrouted)` had been indented four spaces deeper than the paren closing
the `routes!` call for as long as it had existed.

The trap this check had to avoid is the one this section originally recorded: a
source-level parse of `create_router` looks trivial and is not. Several `.route(`
calls put the path on the NEXT line, so a regex of the form `route("PATH"` matches
twenty of twenty-six and reports `/webhooks/midtrans` — the Midtrans webhook — as
unmounted. It is mounted, and `webhooks.rs` has a test that routes it.

So a check cannot be written by scraping the source, and a check built on the
regex would have reported a missing payment webhook, which is worse than no check:
it is a false alarm on the one route that moves money. The `routes!`/`ROUTES`
arrangement is the first of the two honest options this paragraph originally
listed — enumerate the routes in something `create_router` itself consumes — chosen
over the second (leave the link unverified and say so) only because the inventory
parse is now line-anchored and count-pinned, which is what stops it degenerating
into the regex. The check's own first run failed on a **false positive** of a
different kind: it compared the inventory's route count to `MOUNTED`'s row count,
and those differ by design, because `MOUNTED` carries one row per request and the
two dual-method paths are one route each. That is the failure mode described in
the next section, and it is why the comparison now deduplicates paths first.

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
used. `max_context_tokens` is declared on two config types, set by every model entry in
`config/apikita.toml`, and read by nothing. Every one of those occurrences is a
declaration, a literal, or a string, and none is an expression that affects behaviour.

**THIS PARAGRAPH WAS WRONG AND WAS CORRECTED HERE, which is the whole reason it is
worth reading.** It used to say the measurement cannot find PARSED BUT UNREAD
configuration. It can: the fix was to require a FIELD ACCESS rather than a name, so a
struct declaration, a fixture literal and the parser key list stopped counting as uses.
That change found seventeen such keys, and `config/apikita.toml` now marks each one
where it is set. This file was the authority the config comments pointed at, so a stale
claim here was actively misleading a reader rather than merely out of date.

What remains true of the name search is worth keeping, because it is the weaker tool and
still the one people reach for: counting occurrences does not distinguish a declaration
from a read, and a field that changes behaviour appears in a comparison or an arithmetic
rather than only in a struct literal. A leading dot does. The measurement is still a
heuristic - a key read through a macro or a generated accessor would be missed - and the
honest description of its limit is that one, not the one this paragraph used to give.

**THE SECOND PARAGRAPH ABOVE WAS ALSO STALE, and stale in the direction that makes the
tool look weaker than it is.** Until the sentence was replaced, this section ended by
saying the measurement "catches what is entirely unreferenced, and a configuration key
that is referenced in a struct and nowhere else still looks healthy to it" - which is
exactly the behaviour the paragraph before it says was fixed. A reader who got this far
was told the tool cannot do the thing the previous paragraph had just described it
doing, and two paragraphs of a file about stale claims were themselves out of date in a
way only a reader who read all of them would notice. What is true, and what the
`config/apikita.toml` markings are produced by, is the field-access rule: a name a
struct declares and no leading dot ever follows is reported.

**AND THE FIELD-ACCESS RULE ITSELF WAS THEN CHECKED, which found two more things.**
Requiring a dot was implemented and run, and its first two failures were both real.

The first was a field it could not see. The corpus deliberately SKIPPED `config.rs`, on
the reasoning that skipping the file where every field is declared removes the most
obvious source of false "wired" answers. It does - and it also removes every field
access that happens to live there, which is where the config types' own methods are.
`max_output_tokens` is read by `reserved_output_tokens`, whose body is
`self.max_output_tokens`, and the guard reported it as read by nothing. The test module
is stripped before the corpus is used, so the strip was already what kept the fixtures
from answering for every field; excluding the file was never doing that work. The file
is now included.

The second was a limit, not a bug, and it is the one worth remembering. Requiring a dot
flagged the offpeak rate class - `input_offpeak`, `output_offpeak`, `cache_read_offpeak`
- as now-read, because `validate()` reads all three: once in the finiteness loop, once
in the peak/offpeak cache-discount comparison. Settlement prices from the PEAK rates
only, so none of those reads prices anything. The dot rule cannot tell a read that
DECIDES behaviour from a read that INSPECTS a value and moves on, and every startup
VALIDATION takes the second shape. A source-level rule about reads therefore cannot
decide wiring on its own for any field that validation touches.

The resolution is an explicit exemption list, `READ_ONLY_FOR_VALIDATION`, rather than a
widening of the rule - widening is how a check stops being able to fail. The honest
statement of what the list now claims is narrower than it used to be: not "nothing reads
this", but "nothing CHARGES on this". A field on it can be read all over `validate()`
forever and nothing will object. That is the trade, and it is written at the exemption
rather than inferred by the next reader.

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

**AND THE MUTATION HARNESS NEEDS ITS OWN CONTROL, because "the mutant survived" and "no
test ran" look identical from outside.** Measured twice in one session while proving the
route counts guarded. The first attempt filtered on a test name that did not exist, so
`cargo test --lib <name>` reported `test result: ok. 0 passed; 0 failed; 0 ignored;
625 filtered out` - and the harness read `ok` as PASS. Both pinned counts appeared to
survive mutation, which would have been reported as "the count is not enforced"; the pins
were fine and the runner was wrong. The second attempt hit the same class one level down:
the file is CRLF, so an anchor written with a bare `\n` matched nothing, the mutation was
never applied, and the unmutated tree passed - again indistinguishable from a survivor.

So a mutation result is only evidence once three things hold, and each is cheap:

  - **the mutation was APPLIED.** Assert the anchor matched before writing, and re-read
    the file to confirm it changed. A `String.replace` with no match returns the input
    unchanged and raises nothing.
  - **a test actually RAN.** Parse the passed/failed counts and treat `0 passed; 0 failed`
    as an ERROR, never as success. `cargo test` exits 0 when a filter matches nothing,
    which is the trap: the same exit code means "all green" and "nothing executed".
  - **the restore is byte-identical**, checked by hash rather than by having run a copy
    command. A restore that silently failed leaves a mutation in the tree, and the next
    round reads mutated source as if it were real.

A harness missing the second control is the most dangerous of the three, because it fails
in the direction that looks like a finding: every mutant survives, and the report says the
guard is weak when the runner was broken.

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


**AND IT HAPPENED AGAIN IMMEDIATELY, which is the useful part of this entry.** The round
after writing the paragraph above, the same test was run on the remaining checks and
`relay-check` was reported as passing with `docs/edge-relay.md` missing. It does not read
that document: it reads `.docker/nginx/relay.conf`, and it refuses a missing config at
the top with exit 2 before doing anything. The second identical error, in the very next
round, after the lesson was already on the page.

So the honest conclusion is not "ask the question" - that was written down and did not
help. It is that the question is answered by READING THE CHECK before choosing a file to
delete, rather than by choosing a file and then asking. A note in a document does not
change what a careless experiment does; only reading the thing you are about to test does.

The audit itself is complete and the result is clean: every one of the eight tool checks
refuses a missing input. compose-check exits 2 on no `docker-compose.yml`, backup-check 1
on no `backup.sh`, drill-check 1 on no `drill.sh`, reconcile-check 1 on no `reconcile.sql`,
alert-check 1 on no `probe.sh`, ci-docs-check 3 on no `ci-cd.md`, relay-check 2 on no
`relay.conf`, and wind-down-check 3 on no `report.sh`. **None of the eight can pass
vacuously**, and until this was measured that was an assumption rather than a fact.

### Read the code before writing a sentence about it

Three notes in this repository were WRONG when written, and all three were about code
that had just been changed:

- an alert row saying "THREE of the six age-based tables" - written to correct a false
  claim, and falsified hours later by the very change it described;
- a retention row saying "PROMISED, NOT ENFORCED" - correct when written, then enforced
  in the same change, which nobody revisited;
- a config note saying the client "KNOWN, NOT YET FIXED" - the config value it pointed
  at had already been changed, and one line of reading showed the fix was in place.

**THE CAUSE IS THE SAME IN ALL THREE, AND IT IS NOT CARELESSNESS.** Each sentence was
easy to write and felt obviously true, so it was written before the code beside it was
read. In the third case the note was committed *describing a fix made in the previous
commit* - the information was one function away.

The rule that separates the successful checks from these:

> Read the code, THEN write the sentence. In that order the sentence cannot be wrong
> about something you have just looked at.

The failure mode is specific and worth naming because it is invisible while it happens:
a comment that describes a *gap* goes stale the moment the gap closes, and nothing in
CI notices a comment being optimistic. The guards here all check machine-readable things
for exactly this reason, and a comment is the one artefact no guard can reach.

So the discipline for prose is the one for code that this file keeps arriving at:
**assert on something that cannot drift.** For a note about a config value, the thing
that cannot drift is the config value itself - which is why the note is now paired with a
test that reads `config/apikita.toml` rather than restating what it contains.

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

### A negative result is only as good as the set it ranged over

Two shapes. They look identical right up until you ask what set the search covered.

| | what settles it | repeatable by someone else? |
| --- | --- | --- |
| **Countable** | a complete count: the string appears exactly once, in this line; the folder holds N files and I listed them | yes — the count is the evidence |
| **Uncountable** | `Test-Path <somewhere>` returning False: I looked in one place, found nothing, and never named the set I should have covered | no — the place I chose is the claim |

`todo.md` was the second, and it cost four rounds. The launch checklist said of its alert
counts *"see `todo.md`, which now cites the same numbers."* I tested **`docs/todo.md`**,
because I had assumed that a reference in a document under `docs/` points inside `docs/`.
It returned False, I generalised that to a dangling reference, and I wrote the claim into a
launch gate. `todo.md` is at the **repository root**, is git-tracked, and line 168 of it cites
the same numbers. The sentence was correct and I replaced it with a false statement in the
one document whose job is to be trustworthy.

**THE TEST: could you state the SIZE of the set you searched?** `grep -c` gives you a
number, and the number is the finding. `Test-Path` gives you a boolean about a location you
chose, and the location is the claim — so you have to justify the choice before you can use
the result, which is the step `todo.md` skipped.

**WHAT A COUNTABLE NEGATIVE BUYS.** `/review withdraw` was found by grepping the whole
repository for the string and getting exactly one hit. That is a positive count, not an
absence: there is one occurrence and it is the line in the privacy page, so the command does
not exist anywhere. The same search shape that produced the `todo.md` false positive
produced a correct finding in a different round — the difference being that one counted the
whole set and the other did not.

**AND THE POSITIVE VERSION IS BETTER STILL.** A claim is safest when reading the
implementation settles it. *"The proxy never reads a cookie"* is settled by the handler's
signature: it takes `HeaderMap`, so a cookie is not in scope rather than being refused. No
search is involved, and no reader has to trust that a search was thorough.

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
