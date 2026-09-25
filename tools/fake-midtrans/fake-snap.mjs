#!/usr/bin/env node
// Fake-Midtrans Snap server.
//
// A stand-in for POST https://app.sandbox.midtrans.com/snap/v1/transactions,
// so the create_topup success path can be exercised without a real Snap
// account. It answers the request the server actually sends and returns the
// response shape the server actually parses -- both read off
// server/src/routes/account.rs (build_snap_payload, create_snap_transaction).
//
// REQUEST (built by create_snap_transaction + build_snap_payload):
//
//   POST /snap/v1/transactions
//   Authorization: Basic base64(<MIDTRANS_SERVER_KEY> + ":" + "")   <- empty password
//   Accept: application/json
//
//   { "transaction_details": { "order_id": "topup_<uuid>", "gross_amount": 50000 },
//     "customer_details": { "email": "user@example.com" } }   <- omitted when no email
//
// RESPONSE (real Snap 201 body, trimmed to what the server reads):
//
//   { "token": "<uuid>",
//     "redirect_url": "https://app.sandbox.midtrans.com/snap/v4/redirection/<uuid>?order_id=<order_id>",
//     "order_id": "<order_id>" }
//
// The server reads exactly two things: a non-empty `token` string, and an
// optional `redirect_url`. Extra keys are ignored, so `order_id` is echoed
// for human correlation only.
//
// Node 22 stdlib only: node:http, node:crypto, node:process.

import process from 'node:process';
import crypto from 'node:crypto';
import http from 'node:http';

const HELP = `fake-midtrans Snap server

USAGE:
  node fake-snap.mjs [options]
  node fake-snap.mjs --help

SERVES:
  POST /snap/v1/transactions     -> 201 { token, redirect_url, order_id }
  (any other method/path         -> 404)

AUTH:
  Authorization: Basic base64(<server key> + ":" + "")
  A missing, blank, non-Basic, undecodable, or empty-username header is
  rejected with 401 and a Snap-shaped error body.

FAILURE MODES (for negative testing):
  ?fail=<status>   Answer with that HTTP status instead of 201, e.g.
                   /snap/v1/transactions?fail=500. Body is Snap's error shape:
                   { "error_messages": ["..."], "status_code": "500" }.
  FAKE_SNAP_FAIL   Same, as an environment default when no ?fail= is present.

ENVIRONMENT:
  FAKE_SNAP_PORT   Listen port. Default: 8788
  FAKE_SNAP_HOST   Bind address. Default: 127.0.0.1
  FAKE_SNAP_FAIL   Default failure status code (see above).

OPTIONS:
  --port=<n>       Override the listen port.
  --fail=<status>  Force a failure status for every request.
  --help           Show this message and exit.
`;

const MAX_BODY_BYTES = 1024 * 1024; // 1 MiB: this tool only ever sees a tiny JSON body

function parseArgs(argv) {
  const opts = { port: undefined, fail: undefined, help: false };
  for (const a of argv) {
    if (a === '--help' || a === '-h') { opts.help = true; continue; }
    const m = a.match(/^--([a-zA-Z-]+)=(.*)$/);
    if (!m) { console.error('Unknown argument: ' + a); process.exit(2); }
    const key = m[1];
    const val = m[2];
    if (key === 'port') opts.port = val;
    else if (key === 'fail') opts.fail = val;
    else { console.error('Unknown argument: ' + a); process.exit(2); }
  }
  return opts;
}

/// A Snap error body: the server reads `error_messages` (array) first, then
/// `status_message` -- see create_snap_transaction.
function errorBody(status, message) {
  return { error_messages: [message], status_code: String(status) };
}

function sendJson(res, status, payload) {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(body),
  });
  res.end(body);
}

/// Basic auth with the server key as the username and an empty password.
/// Returns the username, or undefined when the header is missing/unusable.
function basicAuthUser(headers) {
  const raw = headers['authorization'];
  if (typeof raw !== 'string') return undefined;
  const m = raw.match(/^Basic\s+(\S+)$/i);
  if (!m) return undefined;
  let decoded;
  try {
    decoded = Buffer.from(m[1], 'base64').toString('utf8');
  } catch {
    return undefined;
  }
  // Split on the FIRST colon: the server key never contains one, but the
  // empty password leaves a trailing ":" we must not eat.
  const idx = decoded.indexOf(':');
  const user = idx === -1 ? decoded : decoded.slice(0, idx);
  return user.length > 0 ? user : undefined;
}

/// Resolves the requested failure status, or undefined for the happy path.
/// Query string wins over the environment default.
function failureStatus(url, envFail) {
  const raw = url.searchParams.get('fail') ?? envFail;
  if (raw === null || raw === undefined || raw === '') return undefined;
  const status = Number.parseInt(raw, 10);
  if (!Number.isInteger(status) || status < 100 || status > 599) return undefined;
  return status;
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > MAX_BODY_BYTES) {
        reject(new Error('request body too large'));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

function handle(req, res, opts) {
  const url = new URL(req.url, 'http://127.0.0.1');

  if (req.method !== 'POST' || url.pathname !== '/snap/v1/transactions') {
    sendJson(res, 404, errorBody(404, 'not found'));
    return;
  }

  // Auth first: an unauthenticated call must never reach the failure selector,
  // let alone the success path.
  const user = basicAuthUser(req.headers);
  if (user === undefined) {
    console.log('[fake-snap] 401 unauthorized (missing or blank Authorization header)');
    sendJson(res, 401, errorBody(401, 'Access denied due to unauthorized transaction, please check client or server key'));
    return;
  }

  const fail = failureStatus(url, opts.fail);
  if (fail !== undefined) {
    console.log('[fake-snap] ' + fail + ' forced failure (basic user=' + user + ')');
    sendJson(res, fail, errorBody(fail, 'forced failure via the fake Snap server'));
    return;
  }

  readBody(req)
    .then((text) => {
      let payload;
      try {
        payload = JSON.parse(text);
      } catch {
        sendJson(res, 400, errorBody(400, 'invalid JSON body'));
        return;
      }

      const details = payload && payload.transaction_details;
      const orderId = details && typeof details.order_id === 'string' ? details.order_id : '';
      const amount = details ? details.gross_amount : undefined;
      if (!orderId) {
        sendJson(res, 400, errorBody(400, 'transaction_details.order_id is required'));
        return;
      }

      const token = crypto.randomUUID();
      const body = {
        token,
        redirect_url:
          'https://app.sandbox.midtrans.com/snap/v4/redirection/' +
          encodeURIComponent(token) +
          '?order_id=' +
          encodeURIComponent(orderId),
        order_id: orderId,
      };

      // Requirement: a human must be able to see the server reached Snap.
      console.log(
        '[fake-snap] 201 order_id=' + orderId + ' amount=' + amount + ' token=' + token
      );
      sendJson(res, 201, body);
    })
    .catch((e) => {
      if (!res.headersSent) sendJson(res, 400, errorBody(400, 'unreadable request body: ' + e.message));
    });
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    console.log(HELP);
    return;
  }

  const port = Number.parseInt(opts.port ?? process.env.FAKE_SNAP_PORT ?? '8788', 10);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    console.error('Invalid port: ' + (opts.port ?? process.env.FAKE_SNAP_PORT));
    process.exit(2);
  }
  const host = process.env.FAKE_SNAP_HOST || '127.0.0.1';
  const envFail = opts.fail ?? process.env.FAKE_SNAP_FAIL;

  const server = http.createServer((req, res) => handle(req, res, { fail: envFail }));

  server.on('error', (e) => {
    console.error('fake-snap: ' + e.message);
    process.exit(1);
  });

  server.listen(port, host, () => {
    console.log('fake-snap listening on http://' + host + ':' + port + '/snap/v1/transactions');
    if (envFail) console.log('fake-snap: forced failure status ' + envFail);
  });

  for (const sig of ['SIGINT', 'SIGTERM']) {
    process.on(sig, () => {
      server.close(() => process.exit(0));
    });
  }
}

main();
