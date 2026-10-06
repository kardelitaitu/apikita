// The `key` event's CONSUMER, which is what made the whole chain pointless without it.
//
// `docs/realtime.md:73` promises the `key` event fires on create/edit/revoke "so a second browser tab
// stays consistent". Three rounds of work went into making the server EMIT it — `publish_key_update`
// had no caller at all until it was wired into `revoke_key` — and MEASURED after that fix, the state
// it feeds was still read by NOTHING:
//
//   live.ts:38          `revokedKeyIds: string[]`  declared, doc-comment "for cross-tab consistency"
//   live.ts:172         written when a `key` event with a truthy `revoked_at` arrives
//   outside live.ts     ZERO readers
//
// Every sibling field in `LiveState` is read by 2 to 15 files. So a second tab kept rendering a
// Revoke button for a key another tab had already revoked, and clicking it answered 404.
//
// WHY THIS READS THE ISLAND AS TEXT. `KeyManagement.astro`'s `<script>` is neither type-checked by
// astro nor loadable by `node --test` — the same constraint `usage-window.test.ts` records for the
// usage island, which is why its logic was moved into `lib/usage.ts`. The DOM work here cannot move
// (it builds rows with `createElement`), so the assertion is that the SUBSCRIPTION EXISTS and is
// wired to a function that reads the field. That is weaker than running it and is stated rather than
// implied; what it does catch is the deletion of the only consumer, which is the exact regression.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (p: string) => readFileSync(join(root, p), 'utf8');

const KEYS_ISLAND = 'website/src/islands/keys/KeyManagement.astro';
const LIVE = 'website/src/lib/live.ts';

/** Every file under `website/src` that could consume a live-state field. */
function websiteSources(): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  const walk = (dir: string): void => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (/\.(ts|astro)$/.test(entry.name)) {
        out.push([full.replace(root, '').replace(/\\/g, '/'), readFileSync(full, 'utf8')]);
      }
    }
  };
  walk(join(root, 'website', 'src'));
  return out;
}

test('the `key` event has a consumer, not only a producer', () => {
  const island = read(KEYS_ISLAND);
  // The IMPORT, matched as an import statement. `includes('getLiveStore')` was the first version and
  // it passed with the import deleted, because the call site still contained the identifier.
  assert.ok(
    /import\s*\{[^}]*\bgetLiveStore\b[^}]*\}\s*from\s*'\.\.\/\.\.\/lib\/live'/.test(island),
    `${KEYS_ISLAND} no longer imports the live store, so it cannot react to a revocation made in ` +
      'another tab. docs/realtime.md:73 promises the `key` event exists so that a second tab stays ' +
      'consistent, and this page is where that tab is.',
  );
  assert.ok(
    /\b\w+\.subscribe\(\s*applyRevocations\s*\)/.test(island) ||
      /getLiveStore\(\)\s*\.\s*subscribe\(/.test(island),
    `${KEYS_ISLAND} imports the live store but never SUBSCRIBES to it. Importing without subscribing ` +
      'reads like the wiring is done and leaves the page exactly as blind as before - which is the ' +
      'state this test was written for: `revokedKeyIds` had a producer, a doc-comment, and no reader. ' +
      'The subscription may be written either as a chained call or against a hoisted store; both are ' +
      'matched, because pinning the SHAPE of the call rather than its existence would fail on a ' +
      'refactor that kept the behaviour - which is what happened when the store was hoisted so two ' +
      'listeners could share it.',
  );
  assert.ok(
    /\b\w+\.subscribe\(\s*applyKeyChanges\s*\)/.test(island),
    `${KEYS_ISLAND} must ALSO subscribe for create/edit. docs/realtime.md:73 promises the \`key\` ` +
      'event on "created, edited, or revoked" and the two are not interchangeable: a revocation is ' +
      'patched into the rows on screen, while a create or edit carries no field values and has to ' +
      'trigger a REFETCH. A page that subscribes only to revocations handles a third of the promise ' +
      'and still looks wired.',
  );
});

test('the cross-tab revocation field is read outside the module that writes it', () => {
  // The field's own definition and its write site live in live.ts; anything else is a reader.
  const readers = websiteSources().filter(
    ([path, text]) => path !== `/${LIVE.split('/').slice(1).join('/')}` && text.includes('revokedKeyIds'),
  );
  assert.ok(
    readers.length > 0,
    '`revokedKeyIds` is written by live.ts and read by NOTHING. Every sibling field in `LiveState` ' +
      'has at least one consumer, so a field with none is not "unused yet" - it is a documented ' +
      'promise ("for cross-tab consistency") whose last link is missing. The server emits the event, ' +
      'the client stores it, and no page acts on it.',
  );
});

test('the create/edit signal is read outside live.ts too', () => {
  // The same rule for `changedKeyIds`, which was added because the first field could only ever
  // express a revocation. A signal with no reader is the identical defect one field over.
  const readers = websiteSources().filter(
    ([path, text]) =>
      path !== `/${LIVE.split('/').slice(1).join('/')}` && text.includes('changedKeyIds'),
  );
  assert.ok(
    readers.length > 0,
    '`changedKeyIds` is written by live.ts and read by NOTHING, so the create/edit half of ' +
      'docs/realtime.md:73 is stored and discarded exactly as `revokedKeyIds` once was.',
  );
});

test('the field the consumer reads is the one the client writes', () => {
  // Parsed rather than grepped, because a grep for the NAME is satisfied by a wrong reader.
  // MEASURED: replacing the read with `getState().someOtherField` left this test green, since the
  // island still contained the string `revokedKeyIds` in its comment. The assertion has to be about
  // the EXPRESSION, and the consumer's own comment must not be able to satisfy it.
  const island = read(KEYS_ISLAND);
  assert.ok(
    /getState\(\)\s*\.\s*revokedKeyIds\b/.test(island),
    'the keys island must read `revokedKeyIds` OFF THE LIVE STATE: a subscription that reads some ' +
      'other field is wired to nothing. Matching the expression rather than the bare name is ' +
      'deliberate - a test that greps for the word is satisfied by the word appearing in a comment, ' +
      'which is how the first version of this check passed against a broken reader.',
  );
  assert.ok(
    read(LIVE).includes('revokedKeyIds'),
    'live.ts must still declare and write `revokedKeyIds`, which is the field the island reads',
  );
});
