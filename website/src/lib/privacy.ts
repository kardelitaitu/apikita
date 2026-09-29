// The customer-facing privacy disclosure, as data.
//
// It lives in lib/ rather than inline in pages/privacy.astro for the same reason
// lib/dashboard-form.ts does: the test suite loads this module directly with Node,
// and a disclosure that drops a category is exactly what a test must catch.
//
// THE NEVER-THE-PROMPT ROW, and the claim it carries. That row is the most
// load-bearing sentence on this page: the framing above it - a proxy, not a
// data processor - is the whole business position, and it rests on no column in
// this system holding request or response text.
//
// Verified 2026-09-29, three ways, because the claim is worth more than the
// confidence behind it:
//   1. the usage_events schema has no text column of any kind - model, token
//      counts, cost, ref and timestamps only;
//   2. no migration mentions prompt, completion, request_body or response_body;
//   3. the only free-text columns in the database are the three review bodies,
//      which are customer-written testimonials the same page discloses below.
//
// What is NOT covered: the claim is about the DATABASE. The one test that comes
// close, a_customer_prompt_never_reaches_the log, covers the log. A prompt column
// added to a request-path table for debugging would break this claim silently -
// the column lands, every test stays green, and this page goes on telling
// customers their prompts are not stored. If you add one, change this row and
// this comment together.
// document's, restated in plain language. docs/data-retention.md:32-34 states the
// rule this file exists to honour: the /privacy page "must be updated in the same
// change if any of it moves again". tests/privacy.test.ts enforces the coverage
// that rule implies.

export interface StoredRow { what: string; where: string; sensitivity: string; }
export interface NotStoredRow { what: string; why: string; }
export interface RetentionRow { what: string; keep: string; why: string; }
export interface RequestRow { request: string; how: string; }

// docs/data-retention.md, "What is stored".
export const stored = [
  { what: 'Email', where: 'PocketBase', sensitivity: 'Personal' },
  { what: 'Password hash', where: 'PocketBase', sensitivity: 'Sensitive, but not usable if leaked (hashed)' },
  { what: 'Google account link', where: 'PocketBase', sensitivity: 'Personal' },
  { what: 'Telegram ID', where: 'Embedded SQLite (the database file)', sensitivity: 'Personal, pseudonymous' },
  { what: 'Wallet balance + ledger', where: 'Embedded SQLite (the database file)', sensitivity: 'Financial' },
  { what: 'Top-up history (amounts, dates)', where: 'Embedded SQLite (the database file)', sensitivity: 'Financial' },
  { what: 'Token usage per day', where: 'Embedded SQLite (the database file)', sensitivity: 'Behavioural' },
  { what: 'Per-request usage (model, token counts, cost, time)', where: 'Embedded SQLite (the database file)', sensitivity: 'Behavioural — never the prompt or the completion itself' },
  { what: 'API keys', where: 'Embedded SQLite (the database file)', sensitivity: 'Credentials (hashed) — the plaintext is never stored' },
  { what: 'Reviews + edit history', where: 'Embedded SQLite (the database file)', sensitivity: 'Opinion, published aggregate only' },
  // This row is a CUSTOMER-FACING disclosure and it previously claimed an IP hash the
  // sessions table does not hold: sessions.ip_hash is in the schema and NOTHING writes
  // it. Overstating collection is the safer direction to be wrong in and it is still a
  // false statement, so it is corrected rather than left because it errs conservatively.
  { what: 'Sessions', where: 'Embedded SQLite (the database file)', sensitivity: 'Contains the user agent of the login request, and no IP address and no IP hash — the sessions row never had them' },
  // The next three rows were ABSENT, and their absence is the serious direction. The
  // page listed no IP-derived data at all, while docs/data-retention.md has always
  // disclosed these windows — so a reader comparing the two documents would conclude
  // the privacy page had been corrected into silence. It had not; it had been corrected
  // into omission.
  { what: 'Salted IP hash of API-key traffic (per key, per day)', where: 'Embedded SQLite (the database file)', sensitivity: 'Pseudonymous, not anonymous — HMAC-SHA256 of the address under a salt replaced at each UTC midnight, so the same visitor is not linkable across days. Held 7 days per seen-address and 90 per day total' },
  { what: 'Salted IP hash of failed link-code attempts', where: 'Embedded SQLite (the database file)', sensitivity: 'Pseudonymous, same salt scheme, held 7 days. Exists to detect credential guessing against one Telegram account' },
  { what: 'Telegram link codes, and operator audit rows', where: 'Embedded SQLite (the database file)', sensitivity: 'A link code is deleted once used or 24h after expiry; an operator audit row records which operator did what to which account' },
];

// docs/data-retention.md, "What is NOT stored".
export const notStored = [
  { what: 'Customer prompts', why: 'We are a proxy, not a data processor. Logging prompts makes us one without consent.' },
  { what: 'Model completions', why: 'Same.' },
  { what: 'Raw IP addresses', why: 'Only a hash, for abuse correlation.' },
  { what: 'Card or payment credentials', why: 'Midtrans handles payment; we never see them.' },
  { what: 'Plaintext API keys', why: 'Shown once, then only the hash.' },
  { what: 'Midtrans server key', why: 'Not customer data, but never logged either.' },
];

// docs/data-retention.md, "Retention periods".
export const retention = [
  { what: 'Ledger', keep: 'Forever', why: 'Financial record; it is the authoritative audit trail' },
  { what: 'Top-ups', keep: 'Forever', why: 'Financial; matches the ledger' },
  { what: 'Usage daily', keep: '24 months', why: 'Billing disputes, then aggregate only' },
  { what: 'Per-request usage', keep: '90 days', why: 'Covers the 30-day spend window plus a dispute window. Deleted 90 days after the request by the nightly retention job' },
  { what: 'Sessions (expired/revoked)', keep: '30 days', why: 'Tidy up, but keep recent for security review' },
  { what: 'Reviews', keep: 'Until deleted by user', why: 'Published aggregate; individual text is theirs' },
  { what: 'Review history', keep: 'Same as review', why: 'Needed to make an edit meaningful' },
  { what: 'link_codes', keep: 'Until used or expired + 24h', why: 'Then delete' },
  { what: 'Link-redemption attempts', keep: '7 days', why: 'Hashed source of failed link-code attempts; used only to stop credential attacks, then deleted' },
  { what: 'Logs', keep: '30-90 days', why: 'Debugging window; not a database' },
  { what: 'Accounts (closed)', keep: 'Keep record, drop personal data', why: 'See below' },
];

// docs/data-retention.md, "Access and deletion requests".
export const requests = [
  { request: 'See your data', how: 'Dashboard + bot: profile, balance, usage, keys' },
  { request: 'Export it', how: 'Download your account data from Settings — balance, ledger, top-ups, usage and key metadata' },
  { request: 'Correct it', how: 'Edit profile; the ledger is immutable by design' },
  { request: 'Delete your account', how: 'The closure flow below, anonymising where possible' },
  { request: 'Delete a review', how: '/review withdraw — the bot path' },
];

/** Every disclosure array, for a test that wants to check coverage across all. */
export const allDisclosures = { stored, notStored, retention, requests };
