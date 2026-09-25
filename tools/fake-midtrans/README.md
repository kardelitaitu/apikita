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
WARNING: Live end-to-end verification is NOT currently possible.
The Docker daemon is down and no PostgreSQL instance is running, so the
apikita server cannot be started and these webhook scenarios have NOT been
verified against a running server. The signature formula and expected JSON
fields are reproduced verbatim from server/src/money.rs and
server/src/routes/webhooks.rs; the documented expected responses are inferred
from that source and remain unverified until the stack can be brought up.
```

## Implementation notes

- Node 22 standard library only: `node:http`, `node:https`, `node:crypto`,
  `node:process`. No `npm install`, no third-party dependencies.
- The signature is computed over the four values in the order
  `order_id`, `status_code`, `gross_amount`, `server_key` — matching
  `compute_midtrans_signature`.
- The script prints the computed signature and the HTTP status (or a network
  error) on every run.
