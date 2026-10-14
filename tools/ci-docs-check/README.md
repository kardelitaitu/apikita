# CI documentation check

Asserts that [`docs/ci-cd.md`](../../docs/ci-cd.md) describes the pipeline that
**actually runs**.

**Why it exists.** The document is where someone answers *"what will stop my PR?"* and
*"what is checked before merge?"*. Its stage table listed **ten** stages while the
workflow ran **twenty-six** — the seven tool-contract checks added by W35–W43 were absent
entirely, each with a careful rationale in its commit message and a README beside its
script, but not in the one document a reader looks at.

**An understated pipeline is not a tidiness problem.** A reader runs the ten documented
commands locally, sees green, and is surprised by seven failures they did not know
existed — which is how people start working *around* CI instead of with it. The old table
also had a "Blocks merge" column, so an omission read as *"these are advisory"*, the
opposite of true.

```sh
sh tools/ci-docs-check/check.sh
```

| Direction | Assertion |
| --- | --- |
| Every workflow step | is **named** in the document, verbatim |
| Every check the document advertises | **exists** in the workflow |
| The workflow did not parse | **exit 3**, never a vacuous pass |
| The document looks truncated | **exit 3** |

The last two exist because this is the same class of guard as W41/W43: a scan over a tree
it never read would pass over an empty set. It counts the steps it found and refuses to
certify fewer than fifteen.

## Mutation-tested

| Mutation | Expected |
| --- | --- |
| A workflow step added that the document does not name | caught, naming the step |
| A step renamed so the document advertises a non-existent one | caught |
| The workflow emptied (parse failure) | exit 3, not 0 |

## Note

Setup steps that only install a toolchain are listed in the document too, in their own
table, precisely so the check can demand an exact match rather than carrying an
allow-list. An allow-list would be the thing that goes stale next.