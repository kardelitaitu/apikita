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
    if (midChunks < 3) fail(`?fail=midstream delivered ${midChunks} chunks over a raw socket, README says 3-5`);
    if (mid.includes('[DONE]')) fail('?fail=midstream included [DONE], but the README says the cut is before it');
    if (/usage/.test(mid)) fail('?fail=midstream included a usage block, but the README says there is none');
    if (midChunks >= 3 && !mid.includes('[DONE]') && !/usage/.test(mid)) {
      ok(`?fail=midstream sent ${midChunks} chunks then cut, with no [DONE] and no usage`);
    }

    const t0 = Date.now();
    await post('?fail=slow&delay=900');
    const slowMs = Date.now() - t0;
    if (slowMs < 800) fail(`?fail=slow returned in ${slowMs} ms, but was asked to delay 900 ms`);
    else ok(`?fail=slow delayed the first chunk (${slowMs} ms for a 900 ms delay)`);

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
  await checkFakeUpstream(Number(process.env.FAKE_CHECK_PORT || 18799));

  console.log('');
  if (failures.length) {
    console.error(`fake-check: ${failures.length} documented behaviour(s) DID NOT hold`);
    process.exit(1);
  }
  console.log('fake-check: OK - every documented failure-injection row and the signature vector hold');
})();
