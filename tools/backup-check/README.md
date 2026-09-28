# Backup contract check

Verifies [`tools/backup/backup.sh`](../backup/backup.sh) — a backup that does not work is
how money and history are lost.

**Why it exists.** The tool had **no CI coverage**, and running it for the first time found
a **real defect**: the offsite hook was invoked as

```sh
sh -c "$OFFSITE_CMD" apikita-offsite "$ARTIFACT"
```

With `sh -c CMD name arg`, the `name` becomes **`$0` inside CMD**. That works for an INLINE
command and **silently fails for a SCRIPT hook** — the natural shape for any real provider —
which received **nothing** while the script printed *"offsite hook succeeded"* and exited 0.

**The one outcome the tool exists to prevent — a backup that never left the machine,
reported as success — was reachable through the ordinary hook.**

```sh
sh tools/backup-check/check.sh
```

## What it proves

| Property | Why it matters |
| --- | --- |
| **The artifact reaches the hook**, inline *and* script | This is the property that was broken; a script hook is the natural shape |
| A failing hook exits **8** and keeps the local artifact | The local copy must survive a delivery failure |
| No key exits **6**, no offsite hook exits **1** | The documented refusals, each asserted separately |
| **The artifact is encrypted** | The file must not be a readable SQLite database — a plaintext dump that reports success is the worst outcome of all |

## Notes

It builds its own source database, so it needs `sqlite3`. It skips **loudly (exit 3)** when
that is missing, never 0.

The fix it guards is a single-quoted artifact appended to the command string, which
satisfies both hook shapes exactly once. An earlier attempt (`"$OFFSITE_CMD \"$@\"" _
"$ARTIFACT"`) repaired the script form but gave an **inline** hook the artifact twice —
caught by running it, not by reading it.