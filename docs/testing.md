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

### A mutation can land on a COPY, and then "SURVIVED" is not a finding

**`routes/mod.rs` holds THREE copies of the route list, and only one of them ships.**
In source order: the `ROUTES` string literal (two tests parse it), `create_router`'s own `routes!(...)`
invocation (the only one that builds the router), and `MOUNTED_BY_THE_MACRO = routes!(@paths ...)`.
The two `routes!` invocations are **separate tokens** - the macro is expanded twice - so editing one
does not move the other.

That cost three wrong conclusions in one round, each of which looked like a real defect:

- mutating the verb in the `@paths` copy (`get` -> `post` on `/api/reviews/mine`) survived the suite,
  which read as *"the method is catchable by nothing"*;
- mutating it in the `ROUTES` literal also survived, which read as *"and the literal is worse"*;
- writing a `@methods` arm and a method comparison **failed to close either**, because both new
  checks compared the array copies to `ROUTES` while the mutation was still landing elsewhere.

The shipping copy, mutated directly, is caught **five times over**: changing or deleting
`/api/reviews/mine`, and changing `/api/me`, each fail five distinct tests - including
`every_mounted_route_dispatches_and_every_near_miss_is_refused`,
`the_state_handed_to_the_router_is_the_state_its_handlers_run_on`, and the route's own integration
tests. It was never unprotected. The 261-line "fix" written to close the imaginary gap was reverted,
because a second copy of the route list is exactly the duplicate-that-drifts defect this file's
opening section is about - adding one to guard a gap that does not exist makes the next drift likelier.

**The rule this earns: before believing a survivor, print the bytes you changed.** A mutation
harness that edits a file by string match is only a measurement if the string it replaced is the one
the program uses. `grep -c` the anchor and assert it is 1, then re-read the region you intended to
edit - the same discipline as `server/src/doc_claims.rs`'s mutation notes, applied to the harness
rather than the guard. When an anchor appears more than once, an ambiguous-range refusal is the
correct outcome, and scoping the edit to the enclosing function or macro invocation is what turns a
false SURVIVED into a real result.

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
<the rest> filtered out` - and the harness read `ok` as PASS. Both pinned counts appeared to
survive mutation, which would have been reported as "the count is not enforced"; the pins
were fine and the runner was wrong. The second attempt hit the same class one level down:
the file is CRLF, so an anchor written with a bare `\n` matched nothing, the mutation was
never applied, and the unmutated tree passed - again indistinguishable from a survivor.

The filtered-out figure is written as `<the rest>` rather than as the number it was - the
number is incidental to the point (Zero, not the total, is what makes `ok` a lie) and a
literal here would be stale within a round, which is a small version of the restatement
this section is about.

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

**TWO MORE WAYS AN ANCHOR CAN POINT AT THE WRONG PLACE, both measured while re-checking the
rounds that had produced survivors.** The controls above catch "nothing ran" and "nothing was
applied"; these catch "something was applied, to the wrong text":

- **A DOC-COMMENT THAT QUOTES THE MUTATED LINE.** `client.rs` carries a fixture comment reading
  *"MEASURED: replacing `if !endpoint.breaker.allow_request() { continue; }` with `if false` left the
  entire suite green at 641 passed"*. That sentence contains the anchor **verbatim**, so counting
  occurrences in the raw file gave **2** for a line the code has once - which reads as "the mutation
  may have landed on a copy", the exact suspicion it was there to rule out. Comment-stripping
  (`strip_comments`, which `doc_claims.rs` already uses for the same reason) brings it to 1. The
  ambiguity was an artefact of documentation ABOUT the code, not of the code.

- **TWO BLOCKS THAT DIFFER ONLY BY INDENTATION.** `ip_tracking.rs`'s `salt_for_day` holds the same
  three lines twice: the read-lock fast return at 12 spaces and the write-lock re-check at 8. A
  string anchor written from the inner block matches only that one, so a re-test aimed at the
  re-check silently mutated the fast return instead - and reported NOT CAUGHT, against a guard that
  is in fact caught. **When two blocks are near-identical, mutate by LINE INDEX and assert the block
  you are deleting is the one you expect**, rather than by string search.

The second one has a sequel worth stating, because it nearly became a finding. The fast return is a
**lock-contention optimisation**: with it removed, a same-day call takes the write lock, hits the
re-check, and returns the same bytes. The same-day and rotation tests still pass, correctly, and
`M_read` is NOT CAUGHT. **A pure optimisation is not supposed to be pinned by a behavioural test** -
writing one would pin a performance detail and call it correctness. "Not caught" is the right answer
there, and the discipline is to read what the code does before deciding which of the two it is.

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

### A scanner is prose that executes

The rule above is about sentences a person writes. It applies with more force to a scanner -
a regex over source that a person writes to ANSWER a question about other code - because a
wrong scanner does not merely read badly, it produces a confident verdict that gets acted on.

MEASURED OVER EIGHT ROUNDS, auditing this repository's money paths. In twelve cases a
hand-rolled scan and the code disagreed, and **the code was right in all twelve**:

- a text scan for "a non-reserve ledger row" counted rows that a `delta_idr < 0` filter
  excludes anyway, so its conclusion named the wrong mechanism;
- a doc-comment extractor walked backwards from the wrong position and found zero
  candidates in a file full of them;
- the same idea then missed `SET balance_idr = balance_idr + 1` because it looked for the
  column in an expression form it did not expect, and reported a correct test as a false
  positive;
- a call-site counter matched `drift_rows(` without a word boundary, so it also counted
  `db::unpaired_hold_rows(` - a different helper, imported into `proxy.rs` - and produced a
  wrong total;
- the same counter read only the lines AFTER each call, so the single-line form
  `assert_eq!(drift_rows(...).await, 0);` fell outside its window;
- an "insert a test here" script anchored on the first `\n    }\n` after the function
  signature, which matched the closing brace of a call CHAIN inside the helper, so the
  text landed mid-expression and the crate did not compile;
- a scan reported that `db.rs`'s test-only `DELETE FROM` statements polluted a table
  comparison - but that comparison reads only the body of `purge_expired_usage` via
  `sed -n '/^pub async fn purge_expired_usage(/,/^}/p'` and never touches `mod tests`.

**Why this repo is unusually hostile to scanners, which is the useful part.** These sources
mix the following properties *across the tree rather than uniformly*, which is what makes a
pattern learned in one file wrong in the next: two line endings (`auth.rs` and `account.rs`
are CRLF; `keys.rs`, `proxy.rs`, `webhooks.rs` and `db.rs` are LF), `#[cfg(test)]` regions
interleaved with production code, `DELETE FROM` appearing in doc-comments and in test bodies
as well as in production SQL, and multi-line `r#"..."#` raw SQL. A regex that is obviously
correct against one of those is wrong about another, and the failure is silent in both
directions: it over-reports (a false positive that sends the reader after a non-defect) or
under-reports (a missed finding).

**AND THIS PARAGRAPH ITSELF WAS WRONG ON FIRST WRITING**, which is the best available
demonstration of the point. Its opening draft asserted that the three `drift_rows` files were
CRLF and that all eleven `DELETE FROM` sites in `db.rs` were test-only. Both were written from
memory of earlier rounds. Checked before committing: those three files are LF (only `auth.rs`
and `account.rs` are CRLF), and `db.rs`'s eleven sites are five in production SQL, two inside
doc-comments and six in test bodies. The prose was corrected by the same discipline the rest of
this section argues for - a second, independent reading of the thing being described.

**So a scan is only evidence after it has been shown to fail on a known-bad input.** Every
finding in this file that survived scrutiny was confirmed a second way - by mutating the code
and running the suite, or by reproducing the extraction and printing what it actually read.

**The `#[cfg(test)]` trap, with numbers, because it silently inverted a whole round's results.**
The paragraph above says `#[cfg(test)]` regions are "interleaved with production code". The sharp
version of that is not interleaving - it is that **`#[cfg(test)]` appears on ITEMS as well as on
the test MODULE**, and the obvious rule "test code starts at the first `#[cfg(test)]`" is wrong
in 10 files.

MEASURED: 26 test-only items across 10 files sit above their file's `mod tests`. Taking the first
`#[cfg(test)]` as the boundary misclassifies, in these files: `db.rs` 1804 lines (the marker is on a
single `pub const SHIPPED_CREDIT_EXPIRY_MONTHS`, and the real `mod tests` is at line 1980), `proxy.rs`
1764 (`line 49`), `auth.rs` 1259 (`line 47`), `webhooks.rs` 387 (`line 12`), `reviews.rs` 374
(`line 57`), `identity/google.rs` 7 (`line 338`, one `pub fn clear_cache`). About 5,600 lines of
PRODUCTION code were being counted as test code.

This is not a cosmetic miscount. A scan that believes production code is test code reports that the
money module contains **no production SQL** - `db.rs` came back `prod=0 test=15` - which is not an
error the reader can see. The corrected count is `prod=7 test=8`, and those seven statements are the
reserve, settle and ledger paths.

**The correct rule, and the one to reuse:** the test module begins at a `#[cfg(test)]` that is
immediately followed - skipping further attributes and blank lines - by `mod <name> {`. A
`#[cfg(test)]` on anything else is a test-only item, and the lines after it are production until the
real module starts. Written as a helper rather than re-derived per scan, because this session
re-derived it wrong once already.

**Which newline patterns survive CRLF, stated as a rule because three rounds were bitten by it.**
Most of this repository's text files are CRLF - counted over `docs/`, the repository root and
`website/`: **130** of the `.md`, `.toml`, `.yml`, `.json` and `.sh` files. A scan written against an
LF file is wrong about them in one of two directions. The distinguishing factor is not whether the
pattern contains `\n` - almost all of them do - but **whether the newline is the LAST character before
the delimiter or the FIRST of a pair**:

| pattern | on CRLF | why |
| --- | --- | --- |
| `\nX` (`indexOf('\n}')`, `indexOf('\n## ')`) | **matches** | `\nX` is a substring of `\r\nX`; the `\r` sits harmlessly before it |
| `\n\n` | **never matches** | CRLF separates the two newlines with `\r`, so the pair does not occur |
| `/^\n/m` | matches | `m` makes `^` match after the `\n`, which is present |

MEASURED on the real files rather than reasoned about: `docs/error-model.md` is CRLF and
`indexOf('\n## ')` returns 217; `website/src/lib/errors.ts` is CRLF and `indexOf('\n}')` returns 856.
Both guards are correct as written. The `\n\n` form is the dangerous one, and it fails the way the
worst scanners fail - **open**, not closed.

The instance worth keeping: a guard read a paragraph with
`text.slice(text.lastIndexOf('\n\n', at), text.indexOf('\n\n', at))`. On a CRLF document both searches
return `-1`, the fallbacks `?? 0` and `?? text.length` selected the WHOLE FILE, and every citation in
it was then blessed by any occurrence of the word the guard was looking for anywhere in the document.
A decoy reading "the old probe was retired in March", in its own paragraph, ~55 characters away, was
waved through at 5 passed. Normalising **inside the helper** fixed it; normalising at each call site
would have left the next caller free to reintroduce it, and the helper now asserts that what it
returned is not the entire document.

The check to run before trusting any scanner over these files: replace `\n\n` with `\r?\n\r?\n`, or
normalise once at the read. `website/tests/no-shadowing-config.test.ts` already writes `/\r?\n/` for
its frontmatter fence - that is the form to copy.

**A second, smaller lesson from the same round.** Four private money helpers in `db.rs`
(`try_debit`, `try_credit`, `insert_ledger_row`, `expire_one_deposit`) have no *direct* test caller,
which looks alarming and is not a finding: they are reached through public functions that are
tested. Mutating them settled it - removing `try_debit`'s `balance_idr >= ?1` floor fails 5 tests,
including `concurrent_requests_cannot_overdraw_a_one_request_balance`, and doubling `try_credit`'s
operand fails 16. "No caller in test code" is a reason to CHECK coverage, never evidence of its
absence; only the mutation decides.

**A whole-repo scan also reads a STALE PARALLEL COPY of this repository, because one lives in-tree
at `.agents/rg/`.** It is a full duplicate - `server/`, `website/`, `tools/`, `docs/`, `.github/` -
and it is gitignored by `**/.agents/`, so it is invisible to `git status`, to CI, and to every
gate. It is nonetheless on disk, and anything that WALKS the tree sees two of everything.

MEASURED: of its 292 files, 271 are byte-identical to the real ones and **20 differ** - it carries a
pre-`16b35b2` `server/src/bin/hold-sweep.rs`, i.e. the version whose `release_hold` bound
`hold.account_id` raw and could never credit a hold. A scan that counts occurrences double-counts
(`INSERT INTO ledger`: 12 real, 12 in the copy, 24 walked naively), and a scan that looks a file up
by NAME can silently read the stale one - which is how it first surfaced, as duplicate findings for
`tools/backup-check/check.sh` and `server/src/doc_claims.rs`.

**No gate is misled by it, which was checked rather than hoped.** All nine `tools/*-check/check.sh`
were read for a whole-repo walk (`find "$REPO"`, `grep -r` over `$REPO`, `git grep`) and none has
one; all nine were then run against the current tree and all nine exit 0. The single mention of
`.agents` is in `rollback-check`, which uses `.agents/rollback-check-work.$$` as its own scratch
directory - a real dependency on the directory, not on the stale copy inside it.

**So the rule for a scan is: never walk the repository root.** Walk the directories that hold the
code - `server/src`, `website/src`, `tools`, `docs` - and, where a whole-tree sweep is genuinely
wanted, exclude `.agents/` explicitly. `git ls-files` is a better starting point than `fs.walk`,
because it cannot see what git ignores.

The mutation harness has been reliable across all of it for one reason: it compiles and runs
the real thing. Text that only resembles the thing is not a measurement of it.

The operational form, since "be careful" is not one:

> When a scan and a mutation disagree, the mutation is right.
> When only a scan is available, run it against a case whose answer you already know, before
> believing a single one of its findings.

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

## A timing claim needs a counted probe, and the probe needs the right branch

Two endpoints promise that a request for an unknown account does the same WORK as one for a
known account with a wrong password: `signup` hashes before it checks existence, and `login`
hashes a throwaway on the missing-account path. Both comments justify the ordering as an
anti-enumeration measure — *"an early return here would make 'already registered' measurably
faster than 'created'"* — and neither was enforced. Deleting the throwaway hash in `login`
left all 38 `routes::auth` tests green, and moving `signup`'s hash inside the existence branch
did too.

**A stopwatch is the wrong instrument.** This repository's rule is that a flaky guard gets
muted, and a wall-clock assertion about a hash is flaky by construction. The deterministic
form of "the same work happens" is a COUNT: `identity::password` carries a `#[cfg(test)]`
counter, and the assertion is that the missing-account path invokes the hasher *at all*. That
is the fact that distinguishes the two paths, and it does not depend on how loaded the machine
is.

**THEN THE PROBE ITSELF HAS TO BE CHECKED, and this is where the first attempt was wrong.**
The signup assertion was written against an UNKNOWN address, and it passed under the mutation
— because an unknown address takes the create-branch and hashes under *both* orderings. It was
testing that hashing happens, not that it happens FIRST. Moving the probe to an
already-registered address, which is the branch the ordering protects, made it catch the
regression immediately.

**The general shape:** a property of the form "A and B cost the same" cannot be probed on an
input where A and B are the same code path. Ask which branch the claim is about, and probe
that one — then confirm by mutation that the probe fails when the property is removed.

### When a probe cannot be made to fail, delete it

The same round found a **larger** version of the same oracle and could not guard it, which is
worth writing down so the next reader does not spend the time rediscovering why.

Three auth endpoints — signup, `request_password_reset`, `resend_verification` — answer a
neutral reply whether or not the address has an account, and all three call the mailer **only on
the branch where the account exists**. Awaiting that send charged a registered address a full
SMTP conversation and charged an unregistered one nothing, while the reply bodies stayed
byte-identical. Measured against a listener that accepts and never greets: **~1001 ms against
~0.1 ms**, three orders of magnitude larger than the Argon2 difference the hash-ordering
comments were written to close. The code already knew the shape — `send`'s own doc reads *"Does
not retry. A retry here would be a second synchronous wait on the request that caused it"* —
but nothing in the code or the docs recorded that this wait lands on the request path for a
branch that only a registered address reaches.

A route-level test was written, pointed at a dead port, and it **passed with the send awaited**.
Two reasons compounded, and either alone was enough:

- a dead port REFUSES in about 2 ms (measured), so there is no wait to detect;
- the shipped `[email]` section has an **empty `from_address`**, so `EmailSender::new` returns
  `transport: None` and every send is an instant `NotConfigured` whatever `APIKITA_EMAIL_BASE_URL`
  says.

Fixing both and testing the sender directly worked — and took **several minutes**, because
`timeout_of` is `request_timeout_seconds.max(MIN_TIMEOUT_SECONDS)`. The floor is a floor by
design (a zero in the config must not mean "wait forever"), so a test cannot shorten it.

**So it was deleted.** A guard that slow is one the next person removes, and a guard that cannot
fail is worse than none: keeping either would report a property as enforced when nothing enforces
it. What remains is the honest division — the cost is real and is now documented at the call
site, the fix is one `tokio::spawn` in one function, and the limit is recorded here. **The
lesson is not "do not test timing"; it is that some properties are cheaper to make STRUCTURAL
than to measure, and saying which half is measured and which half is read is part of the change.**

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
