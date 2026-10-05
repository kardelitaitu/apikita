// A test that verifies the RENDERED output keeps the promises the source tests pin.
//
// WHY. MEASURED this round: `git grep -l "dist/" website/tests` matches several files, but NONE opens
// a file under dist/ - every mention is a comment or a skip-list entry. CI runs the contract tests
// BEFORE `npm run build`, so the artifact does not even exist when they run. The rendered HTML is
// built, deployed, and read by nothing.
//
// That is LATENT rather than broken: every claim below holds in the output today, checked by hand.
// But source order is not rendered order - a layout can move a block, an Astro slot can drop it, a
// condition can evaluate the other way - and `landing-claims.test.ts` pins the signup disclosure
// ABOVE the submit control by reading bytes of signup.astro. That assertion is about the SOURCE. If
// the layout renders the block below the button, the source test stays green and the customer reads
// the notice after the thing it is meant to inform.
//
// WHAT IT DOES WHEN dist/ IS ABSENT. It SKIPS, loudly. `dist/` is gitignored, so a fresh clone has
// none, and `npm test:unit` must not fail for a contributor who has not built. The skip says what it
// did not check, which is the rule this repository applies elsewhere.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, existsSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(root, 'dist');

/** Every built page: [relative path, html]. */
function builtPages(dir = dist, rel = '', out: Array<[string, string]> = []): Array<[string, string]> {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    const r = rel ? `${rel}/${entry}` : entry;
    if (statSync(path).isDirectory()) builtPages(path, r, out);
    else if (entry.endsWith('.html')) out.push([r, readFileSync(path, 'utf8')]);
  }
  return out;
}

const page = (pages: Array<[string, string]>, name: string) => pages.find(([f]) => f === name)?.[1];

test('the BUILT pages keep the promises the source tests pin', (t) => {
  if (!existsSync(dist)) {
    t.skip('website/dist does not exist, so the rendered output was NOT checked. Run `npm run build` to include it.');
    return;
  }

  const pages = builtPages();

  // A build that produced nothing looks exactly like a build that passed. The site is multi-page
  // (astro output:'static'), and docs/website/README.md states 18 pages.
  assert.ok(
    pages.length >= 10,
    `dist/ holds only ${pages.length} built page(s), so reading it proves nothing about the site. Either the build failed or it is looking at the wrong directory.`,
  );

  const landing = page(pages, 'index.html');
  const signup = page(pages, 'signup/index.html');
  const privacy = page(pages, 'privacy/index.html');
  assert.ok(landing, 'dist/index.html is missing, so the landing page was not built');
  assert.ok(signup, 'dist/signup/index.html is missing, so the signup page was not built');
  assert.ok(privacy, 'dist/privacy/index.html is missing, so the privacy page was not built');

  // THE MONEY TERMS. Each is a claim the checklist marks done and a source test pins; each is also
  // something a customer relies on before paying.
  for (const [label, re] of [
    ['the non-refundable prepaid policy', /non-refundable/i],
    ['the two-year credit expiry', /2 years/i],
    ['the first-deposit minimum', /50,000 IDR/],
    ['that remaining balances are paid out on closure', /paid out/i],
  ] as Array<[string, RegExp]>) {
    assert.match(
      landing,
      re,
      `dist/index.html does not state ${label}. The source tests read the .astro file, so a layout or conditional change can remove this from the RENDERED page while they stay green.`,
    );
  }

  // THE CROSS-BORDER DISCLOSURE, on the signup page, and specifically ABOVE the submit control.
  // landing-claims.test.ts asserts this ordering against signup.astro; this is the same assertion
  // against the bytes a browser receives.
  assert.match(
    signup,
    /mainland China/i,
    'dist/signup/index.html does not name the destination jurisdiction. The disclosure is a checklist gate and the source test reads signup.astro, not the rendered page.',
  );
  const disclosureAt = signup.search(/mainland China/i);
  const submitAt = signup.search(/<button[^>]*type="submit"|type="submit"/i);
  assert.ok(
    submitAt > -1,
    'dist/signup/index.html has no submit control, so the ordering assertion below would be vacuous',
  );
  assert.ok(
    disclosureAt < submitAt,
    'the rendered signup page places the cross-border disclosure AFTER the submit control. landing-claims.test.ts pins this ordering in signup.astro, and a customer reads the form top-down - so a notice rendered below the button is not consent to it. The source assertion is still green because it never reads the built page.',
  );

  // THE FOOTER LINKS RESOLVE. Astro resolves `import.meta.glob` at build time, so a link can be
  // present in the rendered page while its target was never built - which is the 404 the footer's own
  // comment says it avoids by withholding `/terms`. What it does not do is check the links it DOES
  // emit. So walk from the landing page: every internal href must correspond to a page in dist/.
  const emitted = new Set([...landing.matchAll(/href="(\/[^"#?]*)"/g)].map((m) => m[1]));
  assert.ok(
    emitted.size > 0,
    'dist/index.html emits no internal links at all, so the resolution check below would pass over an empty set',
  );
  const missing = [...emitted].filter((href) => {
    const candidates = [join(dist, href, 'index.html'), join(dist, `${href}.html`), join(dist, href)];
    return !candidates.some((c) => existsSync(c));
  });
  assert.deepEqual(
    missing,
    [],
    `dist/index.html links to ${missing.join(', ')}, which the build did not produce. A link to a 404 is worse than no link - that reasoning is in the footer's own comment about /terms, and it applies to every href the build emits.`,
  );

  // AND THE DELIBERATE ABSENCE, asserted so it cannot become an accident. The Terms page is an open
  // checklist item, so the footer withholds the link. If a future build emits it without the page
  // existing, the check above catches it; this states which link is expected to be gone.
  assert.ok(
    !landing.includes('href="/terms"') || existsSync(join(dist, 'terms', 'index.html')),
    'dist/index.html links to /terms and the build produced no terms page',
  );
});
