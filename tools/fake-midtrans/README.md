# Fake-Midtrans Webhook Sender

A tiny, dependency-free Node 22 script that forges Midtrans payment
notifications and POSTs them to a webhook target. It computes the signature
**exactly** the way the server does, so the server's verification path is
exercised end to end.

## Signature formula (verbatim from `server/src/money.rs`)

The server computes and verifies the Midtrans SHA-512 signature as:

```
SHA512(order_id + status_code + gross_amount + server_key)
```

Concretely, from `server/src/money.rs` `compute_midtrans_signature`:

```rust
let mut hasher = Sha512::new();
hasher.update(order_id.as_bytes());
hasher.update(status_code.as_bytes());
hasher.update(gross_amount.as_bytes());
hasher.update(server_key.as_bytes());
hex::encode(hasher.finalize())
```

and the `MidtransNotification` struct it deserializes from the JSON body:

```rust
pub struct MidtransNotification {
    pub order_id: String,
    pub status_code: String,
    pub gross_amount: String,
    pub transaction_status: String,
    pub signature_key: String,
    pub fraud_status: Option<String>,
}
```

The server handler (`server/src/routes/webhooks.rs`) takes this JSON, verifies
the signature with `verify_midtrans_signature`, then evaluates
`transaction_status`:

| `transaction_status`          | Action (from `money.rs`)        |
| ------------------------------ | --------------------------------- |
| `capture`, `settlement`       | Credit `amount_idr`               |
| `refund`, `partial_refund`    | DebitRefund `amount_idr`          |
| `deny`, `cancel`, `expire`   | TerminalNoAction                   |
| anything else                  | Pending                            |

## Usage

```bash
node tools/fake-midtrans/send-webhook.mjs --scenario=settlement
node tools/fake-midtrans/send-webhook.mjs --help
```

### Scenarios

| Scenario       | What the script sends                                                   | Expected server response*                                  |
| -------------- | ----------------------------------------------------------------------- | ----------------------------------------------------------- |
| `settlement`   | Valid signature, `transaction_status=settlement`                        | `200 {"status":"settled"}`                                  |
| `bad-signature`| Signature computed with a **wrong** server key                           | `401 {"error":"invalid signature"}`                         |
| `wrong-amount` | Valid signature but `gross_amount` that does not match stored order     | `400 {"error":"amount mismatch"}`                           |
| `replay`       | Re-sends an already-settled `order_id` (use `--order-id`)              | `200 {"status":"already_settled"}`                         |
| `refund`       | Valid signature, `transaction_status=refund`                            | `200 {"status":"refund_recorded"}`                         |

*Expected responses are read off `server/src/routes/webhooks.rs`. They are
**unverified against a running server** — see the status note below.

### Options

| Option              | Meaning                                                            |
| ------------------- | ------------------------------------------------------------------ |
| `--scenario=<name>` | One of the scenarios above (required unless `--help`).          |
| `--order-id=<id>`   | Override `order_id` (use this to drive the `replay` scenario). |
| `--amount=<idr>`    | Override `gross_amount` in IDR (e.g. `50000.00`).              |
| `--help`            | Print usage and exit.                                              |

## Environment variables

| Variable             | Purpose                                              | Default                                        |
| -------------------- | ---------------------------------------------------- | ---------------------------------------------- |
| `MIDTRANS_TARGET`    | POST target URL for the webhook.                     | `http://127.0.0.1:8080/webhooks/midtrans`      |
| `MIDTRANS_SERVER_KEY`| Server key used to compute the signature.            | `SB-Mid-server-TEST12345`                      |

The server's `MIDTRANS_SERVER_KEY` must match the value used here, otherwise
even the `settlement` scenario is rejected with `401 invalid signature`.

## Status: NOT verified end-to-end

```
WARNING: These webhook scenarios have NOT been verified against a running server.
The signature formula and expected JSON fields are reproduced verbatim from
server/src/money.rs and server/src/routes/webhooks.rs; the documented expected
responses are inferred from that source and remain unverified until the stack
can be brought up.
```

**The blocker this used to cite is gone.** It read "the Docker daemon is down and
no PostgreSQL instance is running" — and the database is now a SQLite file, so
neither Docker nor a database server is required to start the API. What remains is
that nobody has run it:

```sh
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate
DATABASE_URL=sqlite://data/server.db cargo run --bin api
node tools/fake-midtrans/send-webhook.mjs settlement
```

## Implementation notes

- Node 22 standard library only: `node:http`, `node:https`, `node:crypto`,
  `node:process`. No `npm install`, no third-party dependencies.
- The signature is computed over the four values in the order
  `order_id`, `status_code`, `gross_amount`, `server_key` — matching
  `compute_midtrans_signature`.
- The script prints the computed signature and the HTTP status (or a network
  error) on every run.

---

# Fake-Midtrans Snap Server (`fake-snap.mjs`)

A stand-in for `POST https://app.sandbox.midtrans.com/snap/v1/transactions`, so
the `create_topup` **success path** can be exercised without a real Snap
account. The webhook sender above tests the money *in*; this server tests the
money *out* — the call that creates a pending topup.

Like the sender, it is dependency-free Node 22: `node fake-snap.mjs` just works,
no `package.json`, no `npm install`.

## What the server sends, and what this fake answers

Both sides are read off `server/src/routes/account.rs`
(`build_snap_payload` + `create_snap_transaction`), not guessed:

| Direction | Exact value |
| --- | --- |
| Request | `POST /snap/v1/transactions` |
| Request header | `Authorization: Basic base64(<MIDTRANS_SERVER_KEY> + ":" + "")` |
| Request header | `Accept: application/json` |
| Request body | `{ "transaction_details": { "order_id": "topup_<uuid>", "gross_amount": 50000 } }` |
| Request body (optional) | `{ "customer_details": { "email": "..." } }` — omitted when the account has no email |
| Response the server reads | `token` (String, required, non-empty) |
| Response the server reads | `redirect_url` (String, optional) |

**Auth scheme — verified, not assumed.** `create_snap_transaction`
(account.rs:388) calls `.basic_auth(server_key, Some(""))`: HTTP Basic with the
server key as the username and an **empty password**. This fake therefore
accepts any non-empty Basic username, and rejects everything else. Unlike the
webhook sender, no key matching is needed — the username is not compared, only
its presence.

Response body (201), with `order_id` echoed for human correlation (the server
ignores it):

```json
{
  "token": "d3cc38f8-13f9-4ceb-a443-6d39e5dfc842",
  "redirect_url": "https://app.sandbox.midtrans.com/snap/v4/redirection/d3cc38f8-13f9-4ceb-a443-6d39e5dfc842?order_id=topup_11111111-2222-3333-4444-555555555555",
  "order_id": "topup_11111111-2222-3333-4444-555555555555"
}
```

The token is a real UUID (`crypto.randomUUID()`), which is the shape Snap uses.

## Usage

```bash
node tools/fake-midtrans/fake-snap.mjs            # listens on 127.0.0.1:8788
node tools/fake-midtrans/fake-snap.mjs --help
node tools/fake-midtrans/fake-snap.mjs --port=9000
```

Every request is logged to stdout with the `order_id` and `gross_amount`, so a
human can see the fake was reached:

```
fake-snap listening on http://127.0.0.1:8788/snap/v1/transactions
[fake-snap] 201 order_id=topup_11111111-... amount=50000 token=d3cc38f8-...
```

## Failure modes — the fake must be able to fail

Rule 3 of `docs/local-development.md`: *"The fake must be able to fail. A fake
that only succeeds tests nothing."* Two selectors, either one works:

| Selector | Effect |
| --- | --- |
| `?fail=<status>` on the URL | Answers that status instead of 201, e.g. `/snap/v1/transactions?fail=500` |
| `FAKE_SNAP_FAIL=<status>` | Same, as a process-wide default (a `--fail=<status>` flag also exists) |

The failure body uses Snap's own error shape — `error_messages` (array) plus
`status_code` — which is exactly what `create_snap_transaction` parses to build
its error message:

```json
{ "error_messages": ["forced failure via the fake Snap server"], "status_code": "500" }
```

Use it to prove the handler **persists nothing** when Snap errors: account.rs:511
places the Snap call before the `INSERT INTO topups`, so a `?fail=500` run must
leave no row and return a 500 with no `topup_id`. The query string wins over the
environment default — but `FAKE_SNAP_FAIL` makes **every** request fail, so a
run that needs both a success and a failure against one server should leave that
variable unset and use `?fail=<status>` per request.

Auth is checked **before** the failure selector: an unauthenticated request gets
401 even when `?fail=500` is present.

| Variable | Purpose | Default |
| --- | --- | --- |
| `FAKE_SNAP_PORT` | Listen port | `8788` |
| `FAKE_SNAP_HOST` | Bind address | `127.0.0.1` |
| `FAKE_SNAP_FAIL` | Default failure status | unset (success) |

## Verified behaviour (real curl output)

Run: `node tools/fake-midtrans/fake-snap.mjs` in one shell, curl from another.

```
$ curl -s -i -X POST 'http://127.0.0.1:8788/snap/v1/transactions' \
    -H "Authorization: Basic U0ItTWlkLXNlcnZlci1URVNUMTIzNDU6" \
    -H 'Content-Type: application/json' \
    -d '{"transaction_details":{"order_id":"topup_11111111-2222-3333-4444-555555555555","gross_amount":50000}}'
HTTP/1.1 201 Created
Content-Type: application/json
Content-Length: 263

{"token":"d3cc38f8-13f9-4ceb-a443-6d39e5dfc842","redirect_url":"https://app.sandbox.midtrans.com/snap/v4/redirection/d3cc38f8-13f9-4ceb-a443-6d39e5dfc842?order_id=topup_11111111-2222-3333-4444-555555555555","order_id":"topup_11111111-2222-3333-4444-555555555555"}

$ curl -s -i -X POST 'http://127.0.0.1:8788/snap/v1/transactions' -d '{}'
HTTP/1.1 401 Unauthorized
{"error_messages":["Access denied due to unauthorized transaction, please check client or server key"],"status_code":"401"}

$ curl -s -i -X POST 'http://127.0.0.1:8788/snap/v1/transactions?fail=500' \
    -H "Authorization: Basic U0ItTWlkLXNlcnZlci1URVNUMTIzNDU6" -d '{}'
HTTP/1.1 500 Internal Server Error
{"error_messages":["forced failure via the fake Snap server"],"status_code":"500"}
```

`node --check tools/fake-midtrans/fake-snap.mjs` parses clean on Node 22.23.2.

## Can a Rust live test point the server at this fake? NOT WITHOUT A PRODUCTION CHANGE

**As the code stands today, no.** `snap_endpoint` (account.rs:299-304) returns
one of two compile-time constants:

```rust
const SNAP_SANDBOX_URL: &str = "https://app.sandbox.midtrans.com/snap/v1/transactions";
const SNAP_PRODUCTION_URL: &str = "https://app.midtrans.com/snap/v1/transactions";

fn snap_endpoint(midtrans_env: Option<&str>) -> &'static str {
    match midtrans_env.map(str::trim) {
        Some(env) if env.eq_ignore_ascii_case("production") => SNAP_PRODUCTION_URL,
        _ => SNAP_SANDBOX_URL,
    }
}
```

`MIDTRANS_ENV` only chooses *which real Midtrans host* is used — there is no
override hook, no `SNAP_URL`-style variable, and no injectable base URL. So a
local `create_topup` test will always dial the real sandbox host and can never
reach `127.0.0.1:8788`. This fake is therefore verified directly with curl (above)
but **cannot yet be wired into a Rust test**; that is a property of the
production code, not of this tool.

### Minimal seam that would make it possible

One environment read inside `snap_endpoint`, ahead of the `match`:

```rust
// Test/dev seam: an explicit base URL wins over MIDTRANS_ENV.
static SNAP_URL_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();
let override_url = SNAP_URL_OVERRIDE.get_or_init(|| {
    env::var("MIDTRANS_SNAP_URL").ok().map(|u| u.trim().to_string()).filter(|u| !u.is_empty())
});
if let Some(url) = override_url {
    return url.as_str();
}
```

The `OnceLock<Option<String>>` (or returning `Cow<'static, str>`) is needed only
because the current signature returns `&'static str`; a `Cow` return type keeps
the call site at account.rs:488 unchanged. Safety note: an override must never be
readable in production, or a stray env var could redirect a money call — gate it
on a non-production `MIDTRANS_ENV`, or refuse it unless `APIKITA_ALLOW_SNAP_OVERRIDE`
is also set. With that seam in place, a live test runs
`MIDTRANS_SNAP_URL=http://127.0.0.1:8788/snap/v1/transactions` and this fake
answers it unchanged.

**This file does not make that change** — the fence for this task is
`tools/fake-midtrans/**` only. It is recorded here so the required production
edit is a one-line decision rather than a rediscovery.

## Why the fake lives in the repo, not as a separate dev tool

`docs/local-development.md:144` lists "Whether fakes live in the repo or as a
separate dev tool" as an open item. This fake answers it for the Midtrans pair:
**in the repo, under `tools/`**. Rationale: (1) the fake is only correct if it
tracks the server's exact request/response contract — that contract lives in
`server/src/routes/account.rs` and drifts the moment either side moves, so
co-locating them in one reviewable diff is the only way they stay in sync;
(2) it needs no dependencies, so it costs the repo two files and no build step;
(3) a live test can spawn it with a relative path. A separate tool would be a
second repository to keep honest against a contract it cannot see.

## Status

- `fake-snap.mjs` itself: **verified** with the three curl cases above, on
  Node 22.23.2.
- End-to-end through `create_topup`: **not possible today** — see the seam
  section. `docs/local-development.md:60` ("Never develop against real money or
  real providers") is currently unsatisfiable for this endpoint, which is the
  gap this fake exposes.
