#!/usr/bin/env node
// Fake upstream provider: an OpenAI-compatible SSE mock with failure injection.
// Node stdlib only, zero dependencies. See README.md for the contract.
import http from 'node:http';

const PORT = Number(process.env.PORT ?? 8787);
const HOST = process.env.HOST ?? '127.0.0.1';

// Fixed, deterministic billing numbers. Never derived from the stream so the
// settlement tests can assert exact ledger rows.
const USAGE = { prompt_tokens: 11, completion_tokens: 24, total_tokens: 35 };

const CANNED =
  'Hello from the fake upstream. Every word arrives as its own chunk so the ' +
  'proxy observes a real token cadence and settles usage from the final chunk.';

const WORDS = CANNED.split(' ');

let seq = 0;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function clamp(n, lo, hi) {
  return Math.min(Math.max(n, lo), hi);
}

function positive(n, fallback) {
  const v = Number(n);
  return Number.isFinite(v) && v > 0 ? v : fallback;
}

/**
 * PER-KEY FAILURE INJECTION, which this fake claimed to support and did not.
 *
 * THE CLAIM, from its own README: "so the proxy's key cooldown, circuit breaker and mid-stream abort paths
 * are testable without touching a real provider."
 *
 * WHAT IT COULD NOT DO. It never looked at `Authorization`, so a `429` was a property of the REQUEST rather
 * than of the KEY. MEASURED: `server/src/upstream/client.rs` tests key rotation with a bespoke inline
 * `TcpListener` instead — `a_401_from_the_upstream_rotates_to_the_next_key` — because the fake cannot
 * express "this key is parked, that one works". The one behaviour the README names first was the one the
 * tool could not model.
 *
 * WHY IT MATTERS. The server's failover doctrine is per-KEY for a rate limit and per-ENDPOINT for a 5xx
 * (client.rs's own module doc). A fake that returns the same status to every key cannot distinguish the
 * two, so it can only ever exercise half the doctrine.
 *
 * HOW IT WORKS. `x-fake-key-fail` maps a BEARER TOKEN to the status that key receives. The token is
 * matched from the `Authorization: Bearer <token>` header the server sends (client.rs::send).
 *
 *   x-fake-key-fail: sk-bad=429        the key `sk-bad` gets 429; others stream normally
 *   x-fake-key-fail: sk-a=429,sk-b=500 map two keys; anything unlisted streams
 *
 * Requests with no `Authorization` header and no map entry keep the existing behaviour, so every current
 * row is unaffected.
 */
function keyFailure(headers) {
  const spec = headers['x-fake-key-fail'];
  if (typeof spec !== 'string' || spec.length === 0) return null;
  const auth = headers['authorization'];
  if (typeof auth !== 'string') return null;
  const token = auth.replace(/^Bearer\s+/i, '').trim();
  for (const pair of spec.split(',')) {
    const [key, status] = pair.split('=').map((s) => (s ?? '').trim());
    if (key && key === token) {
      const code = Number(status);
      return Number.isInteger(code) && code >= 100 && code <= 599 ? code : null;
    }
  }
  return null;
}

// Query flags win over the equivalent headers.
function behaviour(url, headers) {
  const q = url.searchParams;
  const pick = (query, header) => q.get(query) ?? headers[header] ?? null;
  return {
    fail: pick('fail', 'x-fake-fail'),
    tps: positive(pick('tps', 'x-fake-tps'), 30),
    delayMs: positive(pick('delay', 'x-fake-delay-ms'), 5000),
    chunks: clamp(Math.trunc(positive(pick('chunks', 'x-fake-chunks'), 4)), 3, 5),
    keyFail: keyFailure(headers),
    // `x-fake-usage: omit` forces the usage block to be omitted, so a test can reach the gateway's
    // no-usage path without constructing a body that lacks the opt-in.
    omitUsage: String(headers['x-fake-usage'] ?? '').toLowerCase() === 'omit',
  };
}

function errorBody(status, message) {
  // Three kinds, because the server classifies on STATUS and the body should not contradict it. A 401 is
  // neither a rate limit nor a server fault: `rate_limit_status` in the server's config lists `[429, 401]`,
  // so both park a key, but an OpenAI-shaped 401 says `invalid_api_key`, and mislabelling it
  // `server_error` would make a key problem look like an upstream outage to anyone reading the body.
  const kind =
    status === 429 ? { type: 'rate_limit_error', code: 'rate_limit_exceeded' }
    : status === 401 || status === 403 ? { type: 'invalid_request_error', code: 'invalid_api_key' }
    : { type: 'server_error', code: 'internal_error' };
  return JSON.stringify({
    error: {
      message,
      type: kind.type,
      code: kind.code,
    },
  });
}

function sseChunk(id, delta, extra) {
  return `data: ${JSON.stringify({
    id,
    object: 'chat.completion.chunk',
    created: Math.floor(Date.now() / 1000),
    model: 'fake-upstream',
    choices: [{ index: 0, delta, finish_reason: null }],
    ...extra,
  })}\n\n`;
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  const b = behaviour(url, req.headers);
  const describe = `fail=${b.fail ?? 'none'} tps=${b.tps} chunks=${b.chunks} delay=${b.delayMs}ms`;
  const log = (note) => console.log(`${new Date().toISOString()} ${req.method} ${req.url} -> ${note}`);

  if (req.method === 'GET' && url.pathname === '/healthz') {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('ok\n');
    log('healthz 200');
    return;
  }

  if (req.method !== 'POST' || url.pathname !== '/v1/chat/completions') {
    res.writeHead(404, { 'content-type': 'application/json' });
    res.end(errorBody(404, 'fake upstream: no such route'));
    log('404 not found');
    return;
  }

  // THE BODY IS READ NOW, where it used to be discarded with `req.resume()`. The usage opt-in lives in it
  // (`stream_options.include_usage`), and a fake that never looks cannot model the upstream that omits the
  // block. Collected as text so a malformed body is not an exception.
  let rawBody = '';
  for await (const chunk of req) rawBody += chunk;

  // PER-KEY failure first, so one key can be parked while another streams - which is the only way to
  // exercise the server's per-KEY rotation (see keyFailure above). It takes precedence over the request-wide
  // `fail`, because a map that named this key is the more specific instruction.
  if (b.keyFail !== null) {
    res.writeHead(b.keyFail, { 'content-type': 'application/json' });
    res.end(
      errorBody(
        b.keyFail,
        b.keyFail === 429 ? 'fake upstream: rate limited for this key' : 'fake upstream: this key is refused',
      ),
    );
    log(`inject ${b.keyFail} for the presented key`);
    return;
  }

  if (b.fail === '429' || b.fail === '500') {
    const status = Number(b.fail);
    res.writeHead(status, { 'content-type': 'application/json' });
    res.end(
      errorBody(status, status === 429 ? 'fake upstream: rate limited' : 'fake upstream: internal error'),
    );
    log(`inject ${status} (${describe})`);
    return;
  }

  if (b.fail === 'slow') {
    log(`inject slow first token (${describe})`);
    await sleep(b.delayMs);
  }

  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
    'x-accel-buffering': 'no',
  });
  res.flushHeaders();

  const id = `chatcmpl-fake-${++seq}`;
  const midstream = b.fail === 'midstream';
  const total = midstream ? b.chunks : WORDS.length;
  const gap = Math.round(1000 / b.tps);

  log(`${midstream ? `inject midstream cut after ${total} chunks` : 'stream'} (${describe})`);

  for (let i = 0; i < total; i += 1) {
    if (res.writableEnded || res.destroyed) return;
    const word = WORDS[i % WORDS.length];
    res.write(sseChunk(id, { content: i === 0 ? word : ` ${word}` }));
    if (i < total - 1) await sleep(gap);
  }

  if (midstream) {
    // Abrupt: no [DONE], no usage block. The proxy must not silently retry.
    //
    // THE LAST CHUNK WAS BEING LOST HERE, and every count came out one short of what was asked for.
    // MEASURED over a raw socket, before this fix: `?chunks=3` delivered 2, `?chunks=8` delivered 4, and
    // the default (4) delivered 3 - always n-1. `res.write()` is asynchronous, and destroying the socket
    // immediately after the final write discards whatever is still buffered, so the chunk the README
    // promises was never flushed.
    //
    // `end()` DRAINS before closing, which is what makes the documented count true. It still sends no
    // `[DONE]` and no usage block, so the cut stays abrupt in every way the row describes: a client sees
    // exactly the promised chunks and then a closed stream.
    res.end();
    log('stream ended mid-way with no [DONE]');
    return;
  }

  // THE USAGE BLOCK IS CONDITIONAL, because that is what a real OpenAI-compatible upstream does.
  //
  // `client.rs::prepare_body` says why this matters, in its own words: the block is emitted "on SSE only
  // when this opt-in is present; otherwise they assume a human is watching and omit it. Settlement reads
  // that block ... and settles nothing without it, so a stream that skipped the opt-in would deliver
  // tokens and bill zero - THE ONE ERROR AN ARBITRAGE GATEWAY CANNOT MAKE."
  //
  // This fake always emitted it, so the failure that comment names was unrepresentable: a test could not
  // ask "what does the gateway do when the opt-in is honoured by nobody". MEASURED: the server handles it
  // correctly (`proxy.rs` calls `finish_status(0)` rather than settling, and three tests cover it), and the
  // fix here is what lets that behaviour be driven end-to-end against something upstream-shaped.
  //
  // The opt-in is read from the REQUEST BODY, which this fake previously never parsed ("the fake ignores
  // its content"). `x-fake-usage: omit` forces the omission regardless, so a test that needs the failure
  // does not have to construct a body.
  const wantsUsage = b.omitUsage
    ? false
    : (() => {
        try {
          const opts = JSON.parse(rawBody)?.stream_options;
          return !!(opts && typeof opts === 'object' && opts.include_usage === true);
        } catch {
          return false;
        }
      })();

  if (wantsUsage) {
    res.write(
      `data: ${JSON.stringify({
        id,
        object: 'chat.completion.chunk',
        created: Math.floor(Date.now() / 1000),
        model: 'fake-upstream',
        choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
        usage: USAGE,
      })}\n\n`,
    );
  }
  res.write('data: [DONE]\n\n');
  res.end();
  log(wantsUsage ? `done, usage ${JSON.stringify(USAGE)}` : 'done, NO usage (the opt-in was not requested)');
});

server.on('clientError', (err, socket) => {
  if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\n\r\n');
});

server.listen(PORT, HOST, () => {
  console.log(`fake-upstream listening on http://${HOST}:${PORT} (usage ${JSON.stringify(USAGE)})`);
});
