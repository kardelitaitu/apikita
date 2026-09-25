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

// Query flags win over the equivalent headers.
function behaviour(url, headers) {
  const q = url.searchParams;
  const pick = (query, header) => q.get(query) ?? headers[header] ?? null;
  return {
    fail: pick('fail', 'x-fake-fail'),
    tps: positive(pick('tps', 'x-fake-tps'), 30),
    delayMs: positive(pick('delay', 'x-fake-delay-ms'), 5000),
    chunks: clamp(Math.trunc(positive(pick('chunks', 'x-fake-chunks'), 4)), 3, 5),
  };
}

function errorBody(status, message) {
  const rateLimited = status === 429;
  return JSON.stringify({
    error: {
      message,
      type: rateLimited ? 'rate_limit_error' : 'server_error',
      code: rateLimited ? 'rate_limit_exceeded' : 'internal_error',
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

  req.resume(); // drain the request body; the fake ignores its content

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
    res.socket.destroy();
    log('socket destroyed mid-stream');
    return;
  }

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
  res.write('data: [DONE]\n\n');
  res.end();
  log(`done, usage ${JSON.stringify(USAGE)}`);
});

server.on('clientError', (err, socket) => {
  if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\n\r\n');
});

server.listen(PORT, HOST, () => {
  console.log(`fake-upstream listening on http://${HOST}:${PORT} (usage ${JSON.stringify(USAGE)})`);
});
