/**
 * fake-check — the two fake services do what their READMEs say.
 *
 * WHY THIS EXISTS. `tools/fake-upstream/` and `tools/fake-midtrans/` are consulted by hand and cited by
 * comments in `server/src/` (`upstream/client.rs:667`, `:1489`) but nothing RUNS them. The last three
 * defects found in this repository were all in tooling that CI never executed — a gate that aborted at a
 * syntax error, a gate whose assertion could not pass, and a gate whose PATH manipulation was defeated
 * by usrmerge — so "documented but never run" is exactly the category worth closing.
 *
 * WHAT IT ASSERTS, and why each is a behaviour rather than a source read:
 *
 *   fake-upstream  its README publishes a SIX-ROW failure-injection table. That is a contract, and a
 *                  mock whose contract has drifted makes every test built on it meaningless. Each row is
 *                  exercised against a live instance.
 *   fake-midtrans  it claims to compute the Midtrans signature "exactly the way the server does". A
 *                  drifted copy of a formula is a classic defect and would make its "valid signature"
 *                  scenarios pass against nothing. Checked against the server's own PINNED VECTOR
 *                  (`server/src/money.rs:1005`), which is described there as independently computed.
 *
 * Exit: 0 all documented behaviours hold, 1 one or more did not, 3 a tool could not be started.
 *
 * A NOTE ON THE MIDSTREAM ROW, because it caught me. Reading the response with `fetch()` reports ZERO
 * chunks for `?fail=midstream`, against a README promising 3-5: fetch discards a chunked response whose
 * socket then dies. A RAW SOCKET shows the three chunks arriving. So this asserts on the raw socket, and
 * the fact that a client library can invert the verdict is why the row is worth gating at all.
 */
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const net = require('node:net');
const path = require('node:path');

const REPO = path.resolve(__dirname, '..', '..');
const UPSTREAM = path.join(REPO, 'tools', 'fake-upstream');
const SLEEP = (ms) => new Promise((r) => setTimeout(r, ms));

const failures = [];
const fail = (m) => { failures.push(m); console.error(`fake-check:   FAIL - ${m}`); };
const ok = (m) => console.log(`fake-check:   ok - ${m}`);

// ---------------------------------------------------------------------------
// fake-midtrans: the signature formula, against the server's pinned vector
// ---------------------------------------------------------------------------
//
// Reads the vector OUT OF THE SERVER rather than restating it here. A copy of a constant is a second
// place for it to drift, and the whole point of this check is that two places agree.
function serverVector() {
  const src = fs.readFileSync(path.join(REPO, 'server', 'src', 'money.rs'), 'utf8');
  const m = src.match(/const VECTOR_SIG: &str = "([0-9a-f]+)"/);
  if (!m) return null;
  const key = src.match(/const VECTOR_KEY: &str = "([^"]+)"/);
  // The inputs money.rs documents on the line above the constant.
  const inputs = src.match(/SHA512\("([^"]+)" \+ "([^"]+)" \+ "([^"]+)" \+ "([^"]+)"\)/);
  if (!key || !inputs) return null;
  return { sig: m[1], key: key[1], inputs: [inputs[1], inputs[2], inputs[3], inputs[4]] };
}

/**
 * The tool's OWN computeSignature, read out of its source.
 *
 * WHY NOT TRANSCRIBE IT HERE, which is what this did first and why the check was worthless: an inline
 * copy asserts that the COPY matches the server vector. MUTATION-TESTED — swapping `orderId` and
 * `statusCode` inside `send-webhook.mjs` left the gate GREEN, because the gate never opened the file.
 * It was comparing a constant it held against another constant it held.
 *
 * `send-webhook.mjs` is a SCRIPT that calls `main()` on import and exports nothing, so requiring it
 * would fire a webhook at whatever `WEBHOOK_URL` points at. The function is therefore extracted from the
 * source and evaluated, the same technique `tools/loadtest-check/sampler-probe.js` uses for the pid
 * sampler — and for the same reason: the assertion must run the real code, not a copy of it.
 */
function midtransSignatureFromSource() {
  const file = path.join(REPO, 'tools', 'fake-midtrans', 'send-webhook.mjs');
  const src = fs.readFileSync(file, 'utf8');
  const start = src.indexOf('function computeSignature(');
  if (start === -1) {
    fail('could not find computeSignature in tools/fake-midtrans/send-webhook.mjs — the check would otherwise compare a copy against a constant');
    return null;
  }
  // Take the whole function: from its declaration to the first line that closes it at column 0.
  const end = src.indexOf('\n}', start);
  if (end === -1) {
    fail('computeSignature in send-webhook.mjs has no recognizable end, so it cannot be evaluated');
    return null;
  }
  const body = src.slice(start, end + 2);
  // `crypto` is the only free variable the function needs.
  const mod = { exports: {} };
  try {
    // eslint-disable-next-line no-new-func
    new Function('crypto', 'module', `${body}\nmodule.exports = { computeSignature };`)(crypto, mod);
  } catch (e) {
    fail(`computeSignature in send-webhook.mjs could not be evaluated: ${e.message}`);
    return null;
  }
  if (typeof mod.exports.computeSignature !== 'function') {
    fail('send-webhook.mjs exposes no computeSignature after evaluation');
    return null;
  }
  return mod.exports.computeSignature;
}

function checkMidtransSignature() {
  const v = serverVector();
  if (!v) {
    fail('could not read VECTOR_SIG and its inputs from server/src/money.rs — this check would otherwise compare against a value it invented');
    return;
  }
  const computeSignature = midtransSignatureFromSource();
  if (!computeSignature) return;

  const js = computeSignature(v.inputs[0], v.inputs[1], v.inputs[2], v.inputs[3]);
  if (js === v.sig) {
    ok(`fake-midtrans reproduces the server's pinned vector (${v.sig.slice(0, 16)}…)`);
  } else {
    fail(`fake-midtrans computes ${String(js).slice(0, 16)}… but server/src/money.rs pins ${v.sig.slice(0, 16)}… — the tool forges signatures the server rejects`);
  }
  // Field order must be load-bearing, or the concatenation is not a signature.
  const swapped = computeSignature(v.inputs[1], v.inputs[0], v.inputs[2], v.inputs[3]);
  if (swapped === v.sig) fail('swapping order_id and status_code did not change the digest, so field order is not load-bearing');
  else ok('field order is load-bearing (swapping order_id and status_code changes the digest)');
}

/**
 * THE PAYLOAD MUST MATCH ITS SIGNATURE, which the check above cannot see.
 *
 * WHY THIS EXISTS. `checkMidtransSignature` evaluates the fake's `computeSignature` against the server's
 * pinned vector, which proves the FUNCTION is right. It never sends anything, so drift between what is
 * SIGNED and what is SENT is invisible to it. MEASURED, by mutation, in the round this was added:
 *
 *   signing `grossAmount` while the body sends `finalGrossAmount`   -> SURVIVED
 *   renaming the body's `order_id` to `order_id_camel`               -> SURVIVED
 *
 * The consequence, since the server recomputes the signature over what it reads from the body: a drifted
 * payload is a webhook the real server answers **401 invalid signature**. An operator following the
 * README gets a refusal with no explanation, and the tool's stated purpose - "the signature the server
 * computes" - is false.
 *
 * HOW IT CHECKS WITHOUT NETWORK. `buildNotification` is evaluated from source the same way
 * `computeSignature` is, so the assertion runs the real code rather than a copy. The notification is then
 * re-verified with the REAL server check: recompute the digest over the body's own fields and compare it
 * to the body's `signature_key`. That is the same pair `money.rs::verify_midtrans_signature` compares,
 * applied to the fake's own output.
 */
function checkNotificationMatchesItsSignature() {
  const src = fs.readFileSync(path.join(REPO, 'tools/fake-midtrans/send-webhook.mjs'), 'utf8').replace(/\r\n/g, '\n');

  const take = (name) => {
    const start = src.indexOf(`function ${name}(`);
    if (start === -1) return null;
    const end = src.indexOf('\n}', start);
    return end === -1 ? null : src.slice(start, end + 2);
  };

  const pieces = [take('computeSignature'), take('buildNotification')];
  if (pieces.some((p) => p === null)) {
    fail('could not extract computeSignature and buildNotification from send-webhook.mjs — the payload check would otherwise compare against a copy');
    return;
  }

  let build;
  try {
    const mod = { exports: {} };
    // eslint-disable-next-line no-new-func
    new Function('crypto', 'process', 'module', `${pieces[0]}\n${pieces[1]}\nmodule.exports = buildNotification;`)(crypto, process, mod);
    build = mod.exports;
  } catch (e) {
    fail(`buildNotification could not be evaluated: ${e.message}`);
    return;
  }
  if (typeof build !== 'function') {
    fail('send-webhook.mjs exposes no buildNotification after evaluation');
    return;
  }

  // Driven for every scenario the tool advertises, including the ones signed with the wrong key,
  // because a wrong-key webhook is REJECTED and must be - that is not drift.
  for (const scenario of ['settlement', 'bad-signature', 'wrong-amount', 'refund']) {
    const prev = process.env.MIDTRANS_SERVER_KEY;
    process.env.MIDTRANS_SERVER_KEY = 'SB-Mid-server-TEST12345';
    let n;
    try {
      n = build(scenario, { orderId: 'topup_vector_order', amount: '50000.00' });
    } catch (e) {
      fail(`buildNotification(${scenario}) threw: ${e.message}`);
      continue;
    } finally {
      if (prev === undefined) delete process.env.MIDTRANS_SERVER_KEY;
      else process.env.MIDTRANS_SERVER_KEY = prev;
    }

    // THE PAYLOAD'S OWN FIELDS, re-signed. This is exactly what the server does.
    const expected = crypto.createHash('sha512')
      .update(String(n.order_id))
      .update(String(n.status_code))
      .update(String(n.gross_amount))
      .update('SB-Mid-server-TEST12345')
      .digest('hex');

    if (scenario === 'bad-signature') {
      // Deliberately forged elsewhere: it MUST NOT verify, and if it does, the fake's "invalid" case
      // is not invalid at all.
      if (n.signature_key === expected) fail(`bad-signature verified anyway, so the tool's forged case is not one: ${scenario}`);
      else ok(`bad-signature is genuinely forged (${scenario})`);
      continue;
    }
    if (n.signature_key !== expected) {
      fail(`the ${scenario} payload's signature_key does not match its OWN body fields — the tool would post a webhook the server answers 401`);
    } else {
      ok(`the ${scenario} payload signs exactly what it sends (${scenario})`);
    }
  }
}

// ---------------------------------------------------------------------------
// fake-upstream: every row of its failure-injection table
// ---------------------------------------------------------------------------
async function checkFakeUpstream(port) {
  const dir = UPSTREAM;
  const srv = spawn('node', ['server.mjs'], {
    cwd: dir, env: { ...process.env, PORT: String(port) }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let log = '';
  srv.stdout.on('data', (d) => { log += d; });
  srv.stderr.on('data', (d) => { log += d; });

  const base = `http://127.0.0.1:${port}`;
  const post = async (query, headers = {}) => {
    try {
      const r = await fetch(`${base}/v1/chat/completions${query}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', ...headers },
        body: JSON.stringify({ model: 'flash', messages: [{ role: 'user', content: 'hi' }], stream: true }),
      });
      return { status: r.status, body: await r.text() };
    } catch (e) {
      return { status: 'ERR', body: String(e.cause ? e.cause.code || e.message : e.message) };
    }
  };

  try {
    let up = false;
    for (let i = 0; i < 40; i += 1) {
      await SLEEP(200);
      try { const r = await fetch(`${base}/healthz`); if (r.ok) { up = true; break; } } catch { /* wait */ }
    }
    if (!up) { fail(`fake-upstream did not start on ${port}: ${log.slice(0, 200)}`); return; }

    const h = await fetch(`${base}/healthz`);
    if (h.status !== 200) fail(`/healthz answered ${h.status}, README says 200`);
    else ok('GET /healthz answers 200');

    const happy = await post('');
    const chunks = (happy.body.match(/^data: /gm) || []).length;
    if (happy.status !== 200 || chunks < 3) fail(`happy path: status ${happy.status}, ${chunks} chunks`);
    if (!happy.body.includes('[DONE]')) fail('happy path did not end with [DONE]');
    if (!/usage/.test(happy.body)) fail('happy path carried no usage block');
    if (happy.status === 200 && chunks >= 3 && happy.body.includes('[DONE]') && /usage/.test(happy.body)) {
      ok(`happy path streams ${chunks} chunks, ends [DONE], carries usage`);
    }

    // THE FIXED USAGE NUMBERS, read from the README and compared against a LIVE response.
    //
    // WHY THIS EXISTS. The README says "The final chunk always carries the same `usage` object,
    // independent of how many words actually streamed. **Billing assertions can hard-code these**" and
    // publishes { prompt_tokens: 11, completion_tokens: 24, total_tokens: 35 }.
    //
    // MEASURED before this was added: nothing asserted them. The gate checked that a usage block EXISTS
    // and never what it contains, so the fake could have emitted any numbers and stayed green - while the
    // server's own fixture in `upstream/client.rs` hard-codes 11/24/35 in three separate literals. Those
    // two agree today by COINCIDENCE, which is the shape of a pair that drifts: two places describing the
    // same numbers with nothing comparing them.
    //
    // The numbers are read OUT OF THE README rather than restated here, because a copy in this file would
    // be a third place for them to drift - the failure the signature check in this same file documents.
    const readmeUsage = (() => {
      const readme = fs.readFileSync(path.join(REPO, 'tools', 'fake-upstream', 'README.md'), 'utf8').replace(/\r\n/g, '\n');
      const m = readme.match(/"prompt_tokens":\s*(\d+),\s*"completion_tokens":\s*(\d+),\s*"total_tokens":\s*(\d+)/);
      return m ? { prompt_tokens: Number(m[1]), completion_tokens: Number(m[2]), total_tokens: Number(m[3]) } : null;
    })();

    if (!readmeUsage) {
      fail('could not read the published usage numbers out of tools/fake-upstream/README.md, so the billing contract is unchecked');
    } else {
      const m = happy.body.match(/"usage"\s*:\s*\{[^}]*\}/g);
      const last = m ? m[m.length - 1] : null;
      let got = null;
      try { got = last ? JSON.parse('{' + last + '}').usage : null; } catch { /* unparseable */ }
      const same = got
        && got.prompt_tokens === readmeUsage.prompt_tokens
        && got.completion_tokens === readmeUsage.completion_tokens
        && got.total_tokens === readmeUsage.total_tokens;
      if (!same) {
        fail(`the usage block is ${JSON.stringify(got)} but the README publishes ${JSON.stringify(readmeUsage)} — a billing assertion that hard-codes the documented numbers would fail`);
      } else {
        ok(`the usage block matches the published numbers (${readmeUsage.prompt_tokens}/${readmeUsage.completion_tokens}/${readmeUsage.total_tokens})`);
      }
    }

    for (const code of [429, 500]) {
      const r = await post(`?fail=${code}`);
      let shaped = false;
      try { const j = JSON.parse(r.body); shaped = j && typeof j === 'object' && 'error' in j; } catch { /* not json */ }
      if (r.status !== code) fail(`?fail=${code} answered ${r.status}, README says an immediate ${code}`);
      if (!shaped) fail(`?fail=${code} body is not OpenAI-shaped JSON: ${r.body.slice(0, 60)}`);
      if (r.status === code && shaped) ok(`?fail=${code} is immediate and OpenAI-shaped`);
    }

    const viaHeader = await post('', { 'x-fake-fail': '429' });
    if (viaHeader.status !== 429) fail(`x-fake-fail: 429 answered ${viaHeader.status}`);
    else ok('the x-fake-fail header form works');

    // PER-KEY FAILURE - the row the README names first and the fake could not express.
    //
    // The server's doctrine is per-KEY for a rate limit and per-ENDPOINT for a 5xx. A fake that answers
    // every key the same way cannot distinguish them, so this asserts the distinction: one key is parked,
    // another streams. MEASURED before it existed: `client.rs` used a bespoke inline TcpListener for key
    // rotation because the fake never read `Authorization`.
    const keyed = (token, map) => post('', map ? { authorization: `Bearer ${token}`, 'x-fake-key-fail': map } : { authorization: `Bearer ${token}` });

    const parked = await keyed('sk-bad', 'sk-bad=429');
    if (parked.status !== 429) fail(`a key mapped to 429 answered ${parked.status}, so rotation cannot be exercised`);
    else ok('a key mapped to 429 is parked (rate_limit_error)');

    const rotated = await keyed('sk-good', 'sk-bad=429');
    if (rotated.status !== 200) fail(`an UNMAPPED key answered ${rotated.status} while another key was parked - rotation would see no difference`);
    else ok('an unmapped key still streams while another key is parked');

    const twoKeys = await keyed('sk-good', 'sk-bad=429,sk-good=500');
    if (twoKeys.status !== 500) fail(`a two-key map did not reach the second key: answered ${twoKeys.status}`);
    else ok('a two-key map names each key independently');

    const unauth = await post('');
    if (unauth.status !== 200) fail(`a request with no Authorization answered ${unauth.status}; the map must be opt-in`);
    else ok('the key map is opt-in: no Authorization means normal streaming');

    const authBody = await keyed('sk-bad', 'sk-bad=401');
    let authType = null;
    try { authType = JSON.parse(authBody.body)?.error?.type ?? null; } catch { /* not json */ }
    if (authBody.status !== 401 || authType !== 'invalid_request_error') {
      fail(`a 401 body should read invalid_request_error, got status ${authBody.status} type ${authType}`);
    } else {
      ok('a 401 body says invalid_request_error, not server_error');
    }

    const both = await post('?fail=500', { 'x-fake-fail': '429' });
    if (both.status !== 500) fail(`query flag must WIN over the header: asked ?fail=500 with x-fake-fail: 429, got ${both.status}`);
    else ok('the query flag WINS over the header when both are present');

    // midstream over a RAW SOCKET: fetch() reports zero chunks here, and that is fetch's doing.
    const mid = await new Promise((resolve) => {
      const body = JSON.stringify({ model: 'flash', messages: [{ role: 'user', content: 'hi' }], stream: true });
      const s = net.connect(port, '127.0.0.1', () => {
        s.write(`POST /v1/chat/completions?fail=midstream HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: ${body.length}\r\n\r\n${body}`);
      });
      let data = '';
      let settled = false;
      const done = () => { if (!settled) { settled = true; try { s.destroy(); } catch { /* gone */ } resolve(data); } };
      s.on('data', (d) => { data += d.toString(); });
      s.on('end', done);
      s.on('close', done);
      s.on('error', done);
      setTimeout(done, 10000);
    });
    const midChunks = (mid.match(/^data: /gm) || []).length;
    // BOTH BOUNDS, not just the floor. This read `midChunks < 3` until the round that measured it:
    // changing the fake's DEFAULT cut from 4 to 6 **SURVIVED** the gate, because 6 satisfies "at least 3"
    // while the README's row says the default is 3-5. A one-sided assertion against a two-sided promise is
    // the same defect as a threshold no measurement can cross.
    if (midChunks < 3 || midChunks > 5) {
      fail(`?fail=midstream delivered ${midChunks} chunks over a raw socket, but the README's row says the default cut is 3-5`);
    }
    if (mid.includes('[DONE]')) fail('?fail=midstream included [DONE], but the README says the cut is before it');
    if (/usage/.test(mid)) fail('?fail=midstream included a usage block, but the README says there is none');
    if (midChunks >= 3 && midChunks <= 5 && !mid.includes('[DONE]') && !/usage/.test(mid)) {
      ok(`?fail=midstream sent ${midChunks} chunks then cut, with no [DONE] and no usage`);
    }

    const t0 = Date.now();
    await post('?fail=slow&delay=900');
    const slowMs = Date.now() - t0;
    if (slowMs < 800) fail(`?fail=slow returned in ${slowMs} ms, but was asked to delay 900 ms`);
    else ok(`?fail=slow delayed the first chunk (${slowMs} ms for a 900 ms delay)`);

    // ?chunks=N - "Content chunks sent before a mid-stream cut (clamped to 3-5)". The midstream row above
    // runs the DEFAULT cut; this asserts the flag moves it, and that the clamp holds, which is what a row
    // documenting a bounded parameter means.
    //
    // Read over a raw socket for the same reason the midstream row is: fetch discards a chunked response
    // whose socket ends, and a client library inverting a verdict is exactly why that row exists.
    //
    // AND ASSERTING THIS FOUND A DEFECT IN THE FAKE, which is the point of gating a mock's README:
    // `res.write()` was followed by `res.socket.destroy()`, which discards the file-descriptor buffer, so
    // EVERY cut came out one chunk short - `?chunks=3` delivered 2 and `?chunks=8` delivered 4, each
    // against a README promising the number asked for. The fake now ends the response, draining first.
    const rawPost = (query) => new Promise((resolve) => {
      const body = JSON.stringify({ model: 'flash', messages: [{ role: 'user', content: 'hi' }], stream: true });
      const s = net.connect(port, '127.0.0.1', () => {
        s.write(`POST /v1/chat/completions${query} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: ${body.length}\r\n\r\n${body}`);
      });
      let data = '';
      let settled = false;
      const done = () => { if (!settled) { settled = true; try { s.destroy(); } catch { /* gone */ } resolve(data); } };
      s.on('data', (d) => { data += d.toString(); });
      s.on('end', done);
      s.on('close', done);
      s.on('error', done);
      setTimeout(done, 10000);
    });

    for (const [asked, delivered] of [['1', 3], ['3', 3], ['8', 5], ['99', 5]]) {
      const out = await rawPost(`?fail=midstream&chunks=${asked}`);
      const n = (out.match(/^data: /gm) || []).length;
      if (n === delivered) {
        ok(`?chunks=${asked} delivers ${n} chunk(s), clamped into the documented 3-5`);
      } else {
        fail(`?chunks=${asked} delivered ${n} chunk(s), expected ${delivered} — the count the README promises is not what a client receives`);
      }
    }

    const slowTps = Date.now();
    await post('?tps=5');
    const slowDur = Date.now() - slowTps;
    const fastTps = Date.now();
    await post('?tps=400');
    const fastDur = Date.now() - fastTps;
    if (!(fastDur < slowDur)) fail(`?tps did not change the duration: tps=5 took ${slowDur} ms, tps=400 took ${fastDur} ms`);
    else ok(`?tps changes the chunk rate (tps=5 ${slowDur} ms vs tps=400 ${fastDur} ms)`);

    if (!/-> /.test(log)) fail('the server logged no "<method> <url> -> <behaviour>" lines, which the README promises');
    else ok('each request is logged with the behaviour it chose');
  } finally {
    srv.kill();
    await SLEEP(200);
  }
}

(async () => {
  console.log('fake-check: the two fake services do what their READMEs say');
  checkMidtransSignature();
  checkNotificationMatchesItsSignature();
  await checkFakeUpstream(Number(process.env.FAKE_CHECK_PORT || 18799));

  console.log('');
  if (failures.length) {
    console.error(`fake-check: ${failures.length} documented behaviour(s) DID NOT hold`);
    process.exit(1);
  }
  console.log('fake-check: OK - every documented failure-injection row and the signature vector hold');
})();
