// The customer-facing privacy disclosure, as data.
//
// It lives in lib/ rather than inline in pages/privacy.astro for the same reason
// lib/dashboard-form.ts does: the test suite loads this module directly with Node,
// and a disclosure that drops a category is exactly what a test must catch.
//
// Source of truth: docs/data-retention.md. Every row here is one of that
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
  { what: 'Sessions', where: 'Embedded SQLite (the database file)', sensitivity: 'Contains IP hash and user agent' },
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
  { what: 'Per-request usage', keep: '90 days', why: 'Covers the 30-day spend window plus a dispute window. This retention period is policy; a purge job is not yet running, so for now these rows are kept indefinitely' },
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
