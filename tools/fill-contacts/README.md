# fill-contacts

Fill the four owner-input placeholder tokens from one place.

```
node tools/fill-contacts/fill.js --help
node tools/fill-contacts/fill.js --abuse-email abuse@example.id --legal-name "Nama Entitas" --response-hours 48
node tools/fill-contacts/fill.js ... --write
```

Dry run by default. Exit `0` the replacements are consistent, `1` a value is missing or malformed, `3`
a target file is absent.

## Why this exists

Four tokens are spread across five files, and **all four are blocked on the same decision** — a
monitored mailbox and the legal name of the contracting entity:

| token | files | occurrences |
| --- | --- | --- |
| `[[ABUSE_EMAIL]]` | `terms-of-service.md`, `abuse-runbook.md`, `launch-checklist.md` | 5 |
| `[[OWNER_LEGAL_NAME]]` | the same three | 4 |
| `[[PRIVACY_EMAIL]]` | `website/src/lib/privacy.ts` | 2 |
| `[[RESPONSE_HOURS]]` | `terms-of-service.md` | 1 |

Twelve occurrences, one decision. Filling them by hand is four chances to fill three — and a token left
behind is caught only by the build.

## What it refuses

- a **missing** value, naming the token that is short
- a malformed email
- a non-numeric response window
- **another placeholder as a value** — `--abuse-email "[[OTHER]]"` is refused

The last one matters most. The repository's stated position is that *"writing a plausible address would
be worse than the gap — the Terms would promise a channel nobody monitors."* This tool cannot tell a
monitored mailbox from a plausible one, so it does not try; it only refuses to make the problem
invisible.

## It keeps the guard consistent

`website/tests/no-placeholders-ship.test.ts` holds a `PENDING` map whose rows are *claims about what is
outstanding*. Filling a token while leaving its row makes the file contradict itself, so `--write`
removes the row it satisfied. When the last row goes the map is empty, and the guard then fails on **any**
token — which is the correct end state once a real channel exists.

## It is not a gate

There is deliberately **no `check.sh`** beside this tool and no CI step. It edits documents on the
request of a human; it has nothing to verify on every push. The tools that do have gates are listed in
[`tools/README.md`](../README.md).

## What remains after running it

`docs/launch-checklist.md` still needs the surrounding operational facts: the mailbox being **monitored**,
the ToS **published**, and the privacy policy **live**. This tool fills the text; it cannot make a
mailbox exist.
