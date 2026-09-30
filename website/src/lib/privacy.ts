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
//   2. no migration mentions prompt, completion, request_body or response_body -
//      and that one is now ENFORCED rather than checked once, by
//      no_schema_column_is_named_for_a_prompt_or_a_completion, which reads every
//      migration. It uses the four words above plus messages and conversation;
//      when it was first written it had three of the four and had swapped
//      response_body for two of its own, which is the kind of drift that makes a
//      guard decoration;
//   3. the only free-text columns in the database are the three review bodies,
//      which are customer-written testimonials the same page discloses below.
//      This one is a judgement rather than a check - "free text" is not a schema
//      property - and is the reason the guard above looks at NAMES.
//
// The DATABASE side of this claim is now checked. A prompt column added to the
// schema for debugging used to break it silently - the column lands, every other
// test stays green, and this page goes on telling customers their prompts are not
// stored. no_schema_column_is_named_for_a_prompt_or_a_completion fails the moment a
// migration declares one, and it reads compact single-line tables as well as the
// multi-line style, because the first version did not and its mutation proved it.
//
// What is still NOT covered: the check looks at COLUMN NAMES, so a column called
// payload, raw or debug would not be caught. That is deliberate rather than a
// limitation to apologise for - a test cannot decide whether every string in the
// crate is prompt text - and the second half of the control is the other half of
// this comment: if you add one, change this row and this comment together.
// document's, restated in plain language. docs/data-retention.md:42-43 states the
// rule this file exists to honour: the /privacy page "must be updated in the same
// change if any of it moves again". tests/privacy.test.ts enforces the coverage
// that rule implies.

export interface StoredRow { what: string; where: string; sensitivity: string; }
export interface NotStoredRow { what: string; why: string; }
export interface RetentionRow { what: string; keep: string; why: string; }
export interface RequestRow { request: string; how: string; }

// docs/data-retention.md, "What is stored".
export const stored = [
  // These three rows said PocketBase until the identity port landed. They are the
  // reason the port was worth doing for reasons other than engineering: a customer
  // reading this page was told their email and password hash sat in a second
  // system, and after the port they sit in the same embedded database as the rest
  // of the account. Pointing them at PocketBase would now be the false statement.
  { what: 'Email', where: 'Embedded SQLite (the database file)', sensitivity: 'Personal' },
  { what: 'Password hash', where: 'Embedded SQLite (the database file)', sensitivity: 'Sensitive, but not usable if leaked (Argon2id, salted per hash)' },
  { covers: ['accounts'], what: 'Account record (id, status, sign-up time)', where: 'Embedded SQLite (the database file)', sensitivity: 'The account row itself. The address on it lives in the identity row below, so this row and that one describe the same account' },
  { covers: ['identities', 'identity_tokens'], what: 'Email verification and password-reset links', where: 'Embedded SQLite (the database file)', sensitivity: 'Stored only as a SHA-256 hash of the token, never the token itself. Single use, and deleted once redeemed and on the next request for the same purpose — the raw link exists only in the email we send' },
  { what: 'Google account link', where: 'Embedded SQLite (the database file)', sensitivity: 'Personal — the Google subject id and the verified address, not a Google password or a Google session' },
  { covers: ['telegram_links'], what: 'Telegram ID', where: 'Embedded SQLite (the database file)', sensitivity: 'Personal, pseudonymous' },
  // `covers` names the tables this ONE row speaks for, and it is here because this
  // phrase is shared with `ledger`: a shared phrase is not evidence that either
  // table is disclosed. `doc_claims.rs`'s
  // `every_table_is_either_disclosed_to_the_customer_or_recorded_as_unused`
  // requires each table to be named by the row that covers it.
  { covers: ['wallets', 'ledger'], what: 'Wallet balance + ledger', where: 'Embedded SQLite (the database file)', sensitivity: 'Financial' },
  { covers: ['topups'], what: 'Top-up history (amounts, dates)', where: 'Embedded SQLite (the database file)', sensitivity: 'Financial' },
  { covers: ['usage_daily'], what: 'Token usage per day', where: 'Embedded SQLite (the database file)', sensitivity: 'Behavioural' },
  { covers: ['usage_events'], what: 'Per-request usage (model, token counts, cost, time)', where: 'Embedded SQLite (the database file)', sensitivity: 'Behavioural — never the prompt or the completion itself' },
  { covers: ['api_keys'], what: 'API keys', where: 'Embedded SQLite (the database file)', sensitivity: 'Credentials (hashed) — the plaintext is never stored' },
  { covers: ['reviews', 'review_history', 'review_sessions'], what: 'Reviews + edit history', where: 'Embedded SQLite (the database file)', sensitivity: 'Opinion, published aggregate only' },
  // This row is a CUSTOMER-FACING disclosure and it previously claimed an IP hash the
  // sessions table does not hold: sessions.ip_hash is in the schema and NOTHING writes
  // it. Overstating collection is the safer direction to be wrong in and it is still a
  // false statement, so it is corrected rather than left because it errs conservatively.
  { covers: ['sessions'], what: 'Sessions', where: 'Embedded SQLite (the database file)', sensitivity: 'Contains the user agent of the login request, and no IP address and no IP hash — the sessions row never had them' },
  // The next three rows were ABSENT, and their absence is the serious direction. The
  // page listed no IP-derived data at all, while docs/data-retention.md has always
  // disclosed these windows — so a reader comparing the two documents would conclude
  // the privacy page had been corrected into silence. It had not; it had been corrected
  // into omission.
  { covers: ['key_ip_seen', 'key_ip_daily'], what: 'Salted IP hash of API-key traffic (per key, per day)', where: 'Embedded SQLite (the database file)', sensitivity: 'Pseudonymous, not anonymous — HMAC-SHA256 of the address under a salt replaced at each UTC midnight, so the same visitor is not linkable across days. Held 7 days per seen-address and 90 per day total' },
  { covers: ['link_redemption_attempts'], what: 'Salted IP hash of failed link-code attempts', where: 'Embedded SQLite (the database file)', sensitivity: 'Pseudonymous, same salt scheme, held 7 days and swept nightly. A credential-guessing attempt writes one hash per attempt, and the window is the limit on how long a breached salt would link them' },
  // The sign-in and signup caps. This row was absent while the table was written on
  // every attempt against those endpoints, which is the omission direction that matters:
  // the page disclosed the key_ip tables and the link-code attempts, so a reader had no
  // way to learn that the credential-guessing counter exists at all. It is stated here
  // with BOTH keyings, because the account-keyed half holds no address and a page that
  // implied it did would be overstating collection.
  { covers: ['auth_attempts'], what: 'Sign-in attempt counters', where: 'Embedded SQLite (the database file)', sensitivity: 'Stops credential-guessing against the sign-in, signup, password-reset and resend endpoints. The per-address half is the same salted hash scheme as the rows above; the per-account half holds the account id and the time and NO address at all. Both are held 7 days and swept nightly' },
  { covers: ['link_codes', 'link_code_issues', 'admin_audit'], what: 'Telegram link codes, and operator audit rows', where: 'Embedded SQLite (the database file)', sensitivity: 'A link code is deleted once used or 24h after expiry; an operator audit row records which operator did what to which account' },
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
  // The identity links are short-lived by construction rather than by a sweep: a
  // verification or reset token is consumed once, and issuing a new one deletes
  // any outstanding one of the same kind, so nothing needs a retention job to stop
  // being useful. The bound below is the configured TTL, not a cleanup interval.
  { what: 'Email verification and password-reset links', keep: 'Until used, or the link expires (24h for verification, 30m for a reset)', why: 'A single-use secret delivered to an address. It is stored as a hash only, it is deleted the moment it is redeemed, and the next request for the same kind replaces it — so an unredeemed link is inert at its TTL rather than lingering' },
  { what: 'Reviews', keep: 'Until deleted by user', why: 'Published aggregate; individual text is theirs' },
  { what: 'Review history', keep: 'Same as review', why: 'Needed to make an edit meaningful' },
  // WAS the raw table name `link_codes`, which is the one row on this page that showed
  // a schema identifier to a customer while every other row used a phrase. Internal
  // naming is not what a privacy notice is for, and a row that reads as a database dump
  // makes the eleven around it harder to take seriously.
  { what: 'Telegram link codes', keep: 'Until used or expired + 24h', why: 'A link code is a short-lived secret for binding a Telegram account. It is deleted once used, and 24 hours after it expires otherwise, so an unused code never outlives its usefulness' },
  { what: 'Link-redemption attempts', keep: '7 days', why: 'Hashed source of failed link-code attempts; used only to stop credential attacks, then deleted' },
  { what: 'Sign-in attempt counters', keep: '7 days', why: 'Stops credential-guessing against the sign-in, signup, password-reset and resend endpoints. Held only long enough to investigate a live attack; every cap reads a one-hour window, so anything older is already inert' },
  // The two salt rows were missing here while the nightly sweep deleted them, and the
  // omission is the one this page must never make: it is a disclosure of what is kept.
  // They were findable only in ip-tracking.md, which no customer reads, so a reader had
  // no way to learn that a salted hash of their address is kept at all.
  { what: 'Salt hash of an API key’s caller IP, per address seen', keep: '7 days', why: 'Detects one address spreading a key across many accounts. Purged nightly; the salt is replaced at each UTC midnight so days cannot be linked' },
  { what: 'Salt hash of an API key’s caller IP, per day', keep: '90 days', why: 'A trend, not a history: one count per key per day, so the individual hashes are gone long before the trend is' },
  { what: 'Logs', keep: '30-90 days', why: 'Debugging window; not a database' },
  { what: 'Accounts (closed)', keep: 'Keep record, drop personal data', why: 'See below' },
];

// docs/data-retention.md, "Access and deletion requests".
export const requests = [
  { request: 'See your data', how: 'Dashboard + bot: profile, balance, usage, keys' },
  { request: 'Export it', how: 'Download your account data from Settings — balance, ledger, top-ups, usage and key metadata' },
  { request: 'Correct it', how: 'Edit profile; the ledger is immutable by design' },
  { request: 'Delete your account', how: 'The closure flow below, anonymising where possible' },
  // WAS: /review withdraw — the bot path. That command exists NOWHERE in this
  // repository: not in the bot, which is scaffolding, and not in the server, where the
  // review endpoints are marked DESIGNED, NOT BUILT in the api-spec. A privacy page
  // telling someone how to exercise a deletion right, by a command that does not exist,
  // is the most consequential kind of wrong thing this file can say - and it was the
  // single occurrence of the string `withdraw` in the whole tree, here.
  { request: 'Delete a review', how: 'Reviews cannot be posted or withdrawn yet, so there is nothing published to remove. This is the one right on this page with no working path, and it stays on the page rather than disappearing so the gap is visible rather than inferred' },
];

/** Every disclosure array, for a test that wants to check coverage across all. */
export const allDisclosures = { stored, notStored, retention, requests };
