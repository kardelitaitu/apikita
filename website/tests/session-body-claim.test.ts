// The sign-in response body is EMPTY, and both sides must keep saying so.
//
// WHY THIS EXISTS
//
// `POST /auth/login` and `POST /auth/google` used to answer with
// `{ "account_id": "uuid", "balance_idr": 50000 }`. Both fields were read by
// nobody, on either side of the wire, from the day they were written:
//
//   server/src/routes/auth.rs     `AuthSessionResponse { account_id, balance_idr }`
//   website/src/lib/auth-api.ts   `SessionResult { account_id, balance_idr }`
//   website/src/pages/login.astro `.then(googleSignIn).then(() => location.assign(…))`
//
// The third line is the defect. `login.astro` is the ONLY caller, and it throws
// the resolved value away in both handlers - so the page that consumes the
// credential never looks at the body beside it. The credential is the
// `Set-Cookie: session=…`, which is HttpOnly and was always what signed the
// caller in.
//
// The cost was not zero. `balance_idr` made the login path run a wallet `SELECT`
// whose result no client consumed, on the one route every session passes through.
// The balance has a delivery path clients DO read - the SSE `balance` event
// (`server/src/routes/events.rs` -> `website/src/lib/live.ts`) - and `account_id`
// is answerable from `GET /api/me`. A field published but unread is a SECOND
// definition of a fact, free to drift from the one in use, and the next person to
// read docs/server/api-spec.md writes a client against it.
//
// WHAT THIS CHECKS
//
//   1. The server struct declares NO fields. Re-adding one means editing this
//      file and saying why - which is the point, because the reason it was
//      removed is not obvious from the struct alone.
//   2. The client's mirror type declares no field names either. `SessionResult`
//      is `Record<string, unknown>`, so it cannot name a field; a return to a
//      fielded interface fails here.
//   3. The login route does NOT read the wallet. This is the assertion that
//      would have caught the original defect at the moment it was introduced:
//      the SELECT is real work whose result nothing consumed. Verified on the
//      route bodies with comments stripped, so the prose above cannot satisfy it.
//   4. The API spec publishes the empty body for BOTH verbs. The spec was the
//      last place still advertising the two fields after the code stopped
//      serving them.
//   5. Guard the guard: every file scanned was actually read and is still
//      substantial. A path that moved would otherwise make 1-4 pass over
//      nothing, which is the W38/W41 failure this repository has hit before.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions, no new dependency. Tests run with cwd = website/, so the server
// tree is `../server` and docs are `../docs`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const SERVER = join('..', 'server', 'src');
const DOCS = join('..', 'docs');

/**
 * Rust source with comments removed, so a check about what the code DOES is not
 * satisfied or defeated by prose about what the code does. That matters more than
 * usual here: the doc-comment above `AuthSessionResponse` names both removed
 * fields while explaining why they are gone.
 *
 * Handles `//`, `///` and NESTED `/* *​/` (which nests in Rust, unlike C). It does
 * not model string literals, so a `//` inside a string truncates a line - fine for
 * the SQL and struct shapes checked here. It must never fail OPEN: the callers
 * assert on the stripped text, and a stripper returning "" would make every
 * absence assertion pass, so each caller checks the length is still substantial.
 */
function stripComments(source: string): string {
  let out = '';
  let i = 0;
  let depth = 0;

  while (i < source.length) {
    if (depth > 0) {
      if (source.startsWith('/*', i)) {
        depth++;
        i += 2;
      } else if (source.startsWith('*/', i)) {
        depth--;
        i += 2;
      } else {
        i++;
      }
      continue;
    }
    if (source.startsWith('//', i)) {
      const nl = source.indexOf('\n', i);
      i = nl === -1 ? source.length : nl;
      continue;
    }
    if (source.startsWith('/*', i)) {
      depth = 1;
      i += 2;
      continue;
    }
    out += source[i];
    i++;
  }
  return out;
}

function rust(file: string): string {
  return readFileSync(join(SERVER, file), 'utf8');
}

/** The login and Google sign-in route bodies, comments stripped. */
function signInRoutes(): string {
  const code = stripComments(rust('routes/auth.rs'));
  // BOUNDED at the test module, and that bound is load-bearing. The first version
  // sliced to end-of-file reasoning that "the assertions are about what does NOT
  // appear, so a wider window can only make them stronger" - and it failed, because
  // a wider window is not stronger, it is a wider claim. It swept in the
  // `#[cfg(test)]` block, whose `ledger_drift_rows` helper legitimately selects
  // `w.balance_idr` to check the wallet-vs-ledger invariant. The guard then reported
  // the route for reading a column only a test reads: a defect that did not exist,
  // found by a window that was too big rather than a rule that was too strict.
  const start = code.indexOf('pub async fn login');
  assert.ok(start !== -1, 'routes/auth.rs no longer has a `login` handler');
  const end = code.indexOf('#[cfg(test)]', start);
  assert.ok(
    end > start,
    'the `#[cfg(test)]` module was not found after the login handler, so this slice cannot ' +
      'be bounded to the production code and would report the test helpers as route code',
  );
  const body = code.slice(start, end);
  assert.ok(
    body.length > 1_000,
    `the login route slice is only ${body.length} characters, so it is not the handler`,
  );
  return body;
}

test('the server session body declares no fields', () => {
  const code = rust('routes/auth.rs');
  assert.ok(code.length > 10_000, 'routes/auth.rs is suspiciously short');

  const m = code.match(/pub struct AuthSessionResponse\s*\{[^}]*\}/);
  assert.ok(m !== null, 'the AuthSessionResponse struct is gone - update this guard');

  const fields = m[0]
    .replace(/pub struct AuthSessionResponse\s*\{/, '')
    .replace(/\}$/, '')
    .trim();
  assert.equal(
    fields,
    '',
    `AuthSessionResponse gained a field: \`${fields}\`. It is the reply to sign-in, ` +
      'and its body was empty because nothing read it - the credential is the ' +
      'Set-Cookie beside it. If a field is genuinely needed by a client, add it ' +
      'through GET /api/me instead, and delete this test deliberately rather than ' +
      'letting it fail quietly.',
  );
});

test('the client mirror names no field either', () => {
  const code = readFileSync(join('src', 'lib', 'auth-api.ts'), 'utf8');
  assert.ok(code.length > 5_000, 'src/lib/auth-api.ts is suspiciously short');

  const m = code.match(/export type SessionResult = ([^;]+);/);
  assert.ok(
    m !== null,
    'SessionResult is no longer a `export type … = …;` alias - if it went back to an ' +
      'interface with fields, the body is being described as non-empty on the client ' +
      'side while the server sends `{}`',
  );
  assert.equal(
    m[1].trim(),
    'Record<string, unknown>',
    `SessionResult is \`${m[1].trim()}\`. It must stay a record with no named fields: ` +
      'the sign-in body is empty, and naming a field here is a client that will read ' +
      '`undefined` at runtime while typechecking clean.',
  );

  // The two callers must not try to consume it either.
  const login = readFileSync(join('src', 'pages', 'login.astro'), 'utf8');
  assert.ok(login.length > 2_000, 'src/pages/login.astro is suspiciously short');
  assert.ok(
    /loginRequest\(/.test(login) && /googleSignIn|googleIdToken/.test(login),
    'login.astro no longer calls the sign-in verbs, so this guard is looking at the wrong page',
  );
  assert.ok(
    !/\.then\(\s*\(\s*\w+\s*\)\s*=>[\s\S]{0,120}\b(account_id|balance_idr)\b/.test(login),
    'login.astro now reads a field off the sign-in reply, but the reply is `{}`',
  );
});

test('the sign-in route does not read the wallet', () => {
  const body = signInRoutes();
  assert.ok(
    !/balance_idr/.test(body),
    'the login route reads `balance_idr` again. It did once, to populate a response ' +
      'field no client consumed - real work on the route every session passes through, ' +
      'for a value the SSE `balance` event already delivers.',
  );
  assert.ok(
    !/FROM wallets/.test(body),
    'the login route queries the `wallets` table again, so it is publishing a balance ' +
      'nothing asked for',
  );
});

test('the API spec publishes the empty body for both verbs', () => {
  // Normalised to LF. `docs/server/api-spec.md` is stored with CRLF endings (checked:
  // every one of its line breaks is CRLF), so a regex written with a bare `\n` matched
  // nothing and this test failed against a spec that already said the right thing.
  // That is the second way this guard was wrong in the same direction: it reported a
  // violation that was not there. Normalising once here keeps the patterns readable.
  const spec = readFileSync(join(DOCS, 'server', 'api-spec.md'), 'utf8').replace(/\r\n/g, '\n');
  assert.ok(spec.length > 20_000, 'api-spec.md is suspiciously short');

  for (const verb of ['POST /auth/login', 'POST /auth/google']) {
    const start = spec.indexOf('### `' + verb + '`');
    assert.ok(start !== -1, `api-spec.md no longer documents ${verb}`);
    // Up to the NEXT `### ` heading, so the window is the section and not a fixed
    // number of characters that a growing paragraph can silently push the fence out of.
    const next = spec.indexOf('\n### ', start + 1);
    const section = spec.slice(start, next === -1 ? spec.length : next);
    assert.ok(
      section.length > 100,
      `the ${verb} section is only ${section.length} characters, so the slice is wrong`,
    );

    assert.ok(
      /\/\/ 200 response\n\{\}/.test(section),
      `${verb} no longer documents an empty 200 body. The server sends \`{}\`, so a ` +
        'spec showing fields is a contract a client would code against and find empty.',
    );
    assert.ok(
      !/account_id"\s*:/.test(section),
      `${verb} advertises an \`account_id\` in its response again`,
    );
  }
});
