#!/usr/bin/env node
// Fake-Midtrans webhook sender.
//
// Computes the Midtrans SHA-512 signature exactly the way the server does
// (see server/src/money.rs) and POSTs a JSON notification to a target URL.
//
// Signature formula (verbatim from server/src/money.rs):
//
//   SHA512(order_id + status_code + gross_amount + server_key)
//
// The server (server/src/money.rs + server/src/routes/webhooks.rs) expects a
// JSON body with these exact fields (in any JSON key order -- the signature is
// computed over the *values* in the order above, not the JSON key order):
//
//   order_id           String
//   status_code        String
//   gross_amount       String  (e.g. "50000.00")
//   transaction_status String  (e.g. "settlement", "refund", ...)
//   signature_key      String  (computed)
//   fraud_status       String | null  (optional)
//
// Node 22 stdlib only: node:http, node:https, node:crypto, node:process.

import process from 'node:process';
import crypto from 'node:crypto';
import http from 'node:http';
import https from 'node:https';

const HELP = `fake-midtrans webhook sender

USAGE:
  node send-webhook.mjs --scenario=<name> [options]
  node send-webhook.mjs --help

SCENARIOS:
  settlement      Valid signature, transaction_status=settlement (expect 200 settled).
  bad-signature   Signature computed with a wrong server key (expect 401 invalid signature).
  wrong-amount    Valid signature but gross_amount that does not match the stored
                  order (expect 400 amount mismatch).
  replay          Re-sends an already-settled order_id (expect 200 already_settled).
  refund          Valid signature, transaction_status=refund. The platform does NOT
                  refund: the notification is acknowledged with
                  200 {"status":"refund_not_supported"} and changes nothing.

ENVIRONMENT:
  MIDTRANS_TARGET      POST target URL.
                       Default: http://127.0.0.1:8080/webhooks/midtrans
  MIDTRANS_SERVER_KEY  Server key used to compute the signature.
                       Default: SB-Mid-server-TEST12345

OPTIONS:
  --scenario=<name>    One of the scenarios above (required unless --help).
  --order-id=<id>      Override the order_id (useful for replay).
  --amount=<idr>       Override gross_amount in IDR (e.g. 50000.00).
  --help               Show this message and exit.
`;

function parseArgs(argv) {
  const opts = { scenario: undefined, orderId: undefined, amount: undefined, help: false };
  for (const a of argv) {
    if (a === '--help' || a === '-h') { opts.help = true; continue; }
    const m = a.match(/^--([a-zA-Z-]+)=(.*)$/);
    if (!m) { console.error('Unknown argument: ' + a); process.exit(2); }
    const key = m[1];
    const val = m[2];
    if (key === 'scenario') opts.scenario = val;
    else if (key === 'order-id') opts.orderId = val;
    else if (key === 'amount') opts.amount = val;
    else { console.error('Unknown argument: ' + a); process.exit(2); }
  }
  return opts;
}

function computeSignature(orderId, statusCode, grossAmount, serverKey) {
  // SHA512(order_id + status_code + gross_amount + server_key)
  return crypto
    .createHash('sha512')
    .update(orderId)
    .update(statusCode)
    .update(grossAmount)
    .update(serverKey)
    .digest('hex');
}

function buildNotification(scenario, opts) {
  const serverKey = process.env.MIDTRANS_SERVER_KEY || 'SB-Mid-server-TEST12345';
  const orderId = opts.orderId || ('topup_' + Date.now());
  const grossAmount = opts.amount || '50000.00';
  const statusCode = '200';

  let transactionStatus = 'settlement';
  let signingKey = serverKey;          // key used to compute signature_key
  let finalGrossAmount = grossAmount;  // body gross_amount actually sent

  switch (scenario) {
    case 'settlement':
      transactionStatus = 'settlement';
      break;
    case 'bad-signature':
      // Valid-looking payload but signed with the WRONG key.
      signingKey = serverKey + '_WRONG';
      break;
    case 'wrong-amount':
      // Signature is valid for the amount we send, but it won't match the
      // stored order total -> server returns 400 amount mismatch.
      finalGrossAmount = '99999.00';
      break;
    case 'replay':
      // Same order_id as an earlier settlement. Server is idempotent and
      // returns 200 already_settled.
      transactionStatus = 'settlement';
      break;
    case 'refund':
      // The platform does not refund. The server acknowledges this with
      // 200 {"status":"refund_not_supported"}, logs it at error!, and changes
      // nothing: the topup stays settled, no ledger row is written, and the
      // wallet cannot move. (docs/decisions.md: "Non-refundable".)
      transactionStatus = 'refund';
      break;
    default:
      console.error('Unknown scenario: ' + scenario);
      process.exit(2);
  }

  const signatureKey = computeSignature(orderId, statusCode, finalGrossAmount, signingKey);

  return {
    order_id: orderId,
    status_code: statusCode,
    gross_amount: finalGrossAmount,
    transaction_status: transactionStatus,
    signature_key: signatureKey,
    fraud_status: 'accept',
  };
}

function postJson(target, payload) {
  return new Promise((resolve, reject) => {
    const url = new URL(target);
    const body = JSON.stringify(payload);
    const lib = url.protocol === 'https:' ? https : http;
    const req = lib.request(
      url,
      {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'Content-Length': Buffer.byteLength(body),
        },
      },
      (res) => {
        let data = '';
        res.on('data', (c) => (data += c));
        res.on('end', () => resolve({ status: res.statusCode, body: data }));
      },
    );
    req.on('error', reject);
    req.write(body);
    req.end();
  });
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    console.log(HELP);
    return;
  }
  if (!opts.scenario) {
    console.error('Missing --scenario. Use --help for usage.');
    process.exit(2);
  }

  const target = process.env.MIDTRANS_TARGET || 'http://127.0.0.1:8080/webhooks/midtrans';
  const serverKey = process.env.MIDTRANS_SERVER_KEY || 'SB-Mid-server-TEST12345';

  const notification = buildNotification(opts.scenario, opts);

  console.log('[scenario]  ' + opts.scenario);
  console.log('[target]    ' + target);
  console.log('[payload]   ' + JSON.stringify(notification));
  console.log('[signature] ' + notification.signature_key);
  console.log('[serverkey] ' + serverKey);

  try {
    const res = await postJson(target, notification);
    console.log('[status]    HTTP ' + res.status);
    if (res.body) console.log('[response]  ' + res.body);
  } catch (err) {
    console.log('[status]    NETWORK ERROR: ' + err.message);
    process.exitCode = 1;
  }
}

main();
