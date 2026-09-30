// Pure form logic for the two dashboard pages that have no island of their own:
// /dashboard/keys/new and /dashboard/settings.
//
// It lives in lib/ rather than in the .astro frontmatter for the same reason
// lib/retry-wait.ts does: the test suite loads this module directly with Node
// (explicit .ts extensions, no bundler), and these are exactly the rules that
// must not silently invert.
//
// Sources: docs/website/06-api-keys-and-limits.md (model access, limits, key
// reveal) and docs/website/03-functional-spec.md sections "API keys" and
// "Settings". Nothing here is guessed.

import { confirmationError, passwordRuleError } from './auth-flow.ts';

/**
 * Every model the CONFIG knows by name: config/apikita.toml, [[models]]
 * `name = ...`, in file order. Two are routed today (`flash`,
 * `deepseek-v4-flash`); `deepseek-v4-pro` and the three `dummy-*` entries
 * are registered with `weight = 0.0` on every endpoint and can never be
 * selected.
 *
 * IT WAS CALLED `ENABLED_MODELS`, which said the opposite of what it holds: four of
 * the five entries are disabled on every endpoint, and the summary this feeds tells
 * the customer so in as many words - "model(s) that are not currently enabled". The
 * name and the user-facing text contradicted each other, and a reader of the code had
 * to take the comment below on trust to learn it. It is the full INVENTORY because a
 * key's allowlist can name a model that has since been retired, and
 * `describeModelAccess` reports those rather than dropping them.
 *
 * The KeyManagement island offers its own routable subset and is not driven from this
 * list, so the two can differ while this one stays the config's full inventory.
 */
export const KNOWN_MODELS: readonly string[] = [
  'flash',
  'deepseek-v4-flash',
  'deepseek-v4-pro',
  'dummy-glm-5.3-flash',
  'dummy-glm-5.2',
  'dummy-qwen-4-max',
];

/**
 * Whether a key whose stored allowlist is `allowlist` may call `model`.
 *
 * docs/website/06-api-keys-and-limits.md line 41: "A key with an empty allowlist
 * can call **nothing** — deny by default." Line 30: "'All models' means the key's
 * allowlist includes **every currently enabled model**", and line 31-32 says it is
 * stored as an explicit list rather than a boolean so that enabling a model later
 * does not grant it to every existing key.
 *
 * This is the UI-side mirror of `is_model_allowed` in
 * server/src/routes/proxy.rs, which was fixed to deny by default. Inverting it
 * here — reading an empty list as "all models" — would hand the dearest
 * configured model to a key whose owner never granted it, which is the exact
 * regression this function exists to make impossible.
 */
export function allowsModel(allowlist: readonly string[], model: string): boolean {
  return allowlist.includes(model);
}

/**
 * The model-access summary for one key's allowlist, in the vocabulary the keys
 * list already uses ("All models" / "None (deny by default)").
 *
 * An allowlist entry that is no longer enabled is reported rather than hidden:
 * a key created while a model was enabled still carries it, and a summary that
 * silently dropped it would misstate what the key is configured with.
 */
export function describeModelAccess(
  allowlist: readonly string[],
  enabled: readonly string[] = KNOWN_MODELS,
): string {
  const granted = enabled.filter((model) => allowsModel(allowlist, model));
  const retired = allowlist.filter((model) => !enabled.includes(model));
  const retiredNote =
    retired.length === 0 ? '' : ` It also lists ${retired.length} model(s) that are not currently enabled: ${retired.join(', ')}.`;

  if (granted.length === 0) {
    return (
      'No models — a key created with an empty allowlist can call nothing. ' +
      'The allowlist is deny by default.' +
      retiredNote
    );
  }
  if (granted.length === enabled.length) {
    return `All ${enabled.length} enabled models (${granted.join(', ')}).` + retiredNote;
  }
  return `${granted.length} of ${enabled.length} enabled models (${granted.join(', ')}).` + retiredNote;
}

/**
 * A problem with one numeric limit field, or null when the value is acceptable.
 *
 * docs/website/06-api-keys-and-limits.md line 45: "A limit of `0` or `null` means
 * 'no limit of this kind'." A blank field is that same unlimited default, so it
 * is accepted rather than coerced to 0 behind the user's back.
 *
 * server/src/routes/keys.rs `check_spend_limit` refuses a negative
 * `spend_limit_idr` because it "has no meaning and would fail every request",
 * and `limit_reached` treats a non-positive limit as no limit at all. So the
 * form must reject a negative locally: sending it would earn a 402 whose
 * `details.reason` is `invalid_spend_limit_idr`, which is not a sentence a
 * customer should ever have to read.
 *
 * The shape check is a whole-number test, not `Number()` — `Number()` accepts
 * `-5`, `1e3`, `0x10` and `1.5`, four values the API's integer fields do not
 * mean. Same discipline as `parseRetryAfter`.
 *
 * `max` is the largest value the field's Rust type can hold: `i32` for
 * `rate_limit_rpm` and `i64` for the two money/token limits
 * (server/src/routes/keys.rs lines 38-42). A string of digits is not enough —
 * `3000000000` is all digits and overflows `i32`, and a 23-digit
 * `spend_limit_idr` becomes `1e+23` through `Number()`, which
 * `JSON.stringify` emits as a float and serde's `i64` refuses. Either one is
 * rejected by axum before a handler runs, so the user would get the server's raw
 * generic 400 instead of the local sentence this function exists to give.
 * `Number.isSafeInteger` catches the second case; `value > max` the first.
 *
 * The default is JavaScript's own safe-integer ceiling, which is the contract
 * this function had before `max` existed: any run of digits, bounded only by
 * what the language can represent exactly.
 */
export function limitError(raw: string, label: string, max = Number.MAX_SAFE_INTEGER): string | null {
  const text = raw.trim();
  if (text === '') return null;
  if (!/^\d+$/.test(text)) {
    return `${label} must be a whole number of 0 or more. Leave it blank or 0 for no limit.`;
  }
  const value = Number(text);
  if (!Number.isSafeInteger(value) || value > max) {
    return `${label} must be a whole number between 0 and ${max.toLocaleString('en-US')}. Leave it blank or 0 for no limit.`;
  }
  return null;
}

/**
 * The `expires_at` value for a `<input type="date">` value, or null for "never".
 *
 * A date input yields YYYY-MM-DD, which `new Date()` reads as midnight **UTC** —
 * expiring the key at the very start of the day the user picked, so a key set to
 * expire "on the 31st" would already be dead on the 31st. The chosen day is
 * therefore inclusive and the timestamp is its last millisecond, matching the
 * "This key expired on {date}" copy in docs/website/03-functional-spec.md line 169.
 *
 * `Date.UTC` rolls impossible dates over (2025-02-30 becomes 2025-03-02), so the
 * round trip is checked and a rolled-over date is refused rather than stored as a
 * different day than the user chose.
 */
export function expiryToIso(date: string): string | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date.trim());
  if (match === null) return null;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const end = new Date(Date.UTC(year, month - 1, day, 23, 59, 59, 999));
  if (
    end.getUTCFullYear() !== year ||
    end.getUTCMonth() !== month - 1 ||
    end.getUTCDate() !== day
  ) {
    return null;
  }
  return end.toISOString();
}

/** The create-key form's raw string fields, exactly as the island reads them. */
export interface CreateKeyFields {
  label: string;
  models: readonly string[];
  spend_limit_idr: string;
  token_limit: string;
  rate_limit_rpm: string;
  expires_on: string;
}

/**
 * The `POST /api/keys` body the form produces. Mirrors `CreateKeyRequest` in
 * server/src/routes/keys.rs lines 32-44, including `expires_at`, which the island
 * used to hardcode to `null` — a field it collected but never sent.
 */
export interface CreateKeyBody {
  label: string;
  models: string[];
  spend_limit_idr: number;
  token_limit: number;
  rate_limit_rpm: number;
  expires_at: string | null;
}

export type CreateKeyBuild = { ok: true; body: CreateKeyBody } | { ok: false; error: string };

/** A blank limit field is the API's own "no limit of this kind" (0), not a guess. */
function wholeNumber(raw: string): number {
  const text = raw.trim();
  return text === '' ? 0 : Number(text);
}

/**
 * The create-key request, or the first reason it must not be sent.
 *
 * This is the single place the island's form becomes a request, so the three
 * rules the API enforces on the way in are checked where the user can still see
 * them:
 *
 * 1. **Every limit goes through `limitError`.** A negative `spend_limit_idr`
 *    earns a 402 from `check_spend_limit` in server/src/routes/keys.rs, and the
 *    island's old `num()` helper silently turned `-5` into `0` instead of
 *    saying so — a value the user typed, quietly replaced.
 * 2. **The chosen expiry day is sent.** `expiryToIso` makes the day inclusive;
 *    blank is `null`, which the API reads as "never".
 * 3. **An impossible date is refused, not downgraded to "never".** `expiryToIso`
 *    returns null for 2025-02-30, and a non-blank field that parses to nothing
 *    must not quietly become a key that never expires.
 * 4. **A past expiry day is refused too.** Nothing stops the API accepting an
 *    `expires_at` already behind us, so the key would be created and then list
 *    itself as "Expired" the moment it appeared — a key that cannot ever
 *    authenticate. Blank still means "never expires".
 */
export function buildCreateKeyRequest(fields: CreateKeyFields): CreateKeyBuild {
  // The bounds are the server's own field types (server/src/routes/keys.rs lines
  // 38-42): i64 for the two money/token limits, i32 for the per-minute rate. The
  // i64 fields are bounded by MAX_SAFE_INTEGER instead, because a larger value —
  // i64::MAX included — cannot survive JSON.stringify as the number the user
  // typed, so sending it would store a different limit than the one shown.
  const limits: [raw: string, label: string, max: number][] = [
    [fields.spend_limit_idr, 'Spend limit (IDR)', Number.MAX_SAFE_INTEGER],
    [fields.token_limit, 'Token limit', Number.MAX_SAFE_INTEGER],
    [fields.rate_limit_rpm, 'Rate (req/min)', 2_147_483_647],
  ];
  for (const [raw, label, max] of limits) {
    const error = limitError(raw, label, max);
    if (error !== null) return { ok: false, error };
  }

  const expires_on = fields.expires_on.trim();
  const expires_at = expiryToIso(expires_on);
  if (expires_on !== '' && expires_at === null) {
    return { ok: false, error: 'Expires on must be a real calendar date, or blank for a key that never expires.' };
  }
  // `expiryToIso` ends the chosen day at 23:59:59.999Z, so a date is past only
  // once that whole day is behind us — today is still a valid last day.
  if (expires_at !== null && Date.parse(expires_at) < Date.now()) {
    return { ok: false, error: 'Expires on must be today or a future date, or blank for a key that never expires.' };
  }

  return {
    ok: true,
    body: {
      label: fields.label.trim(),
      models: [...fields.models],
      spend_limit_idr: wholeNumber(fields.spend_limit_idr),
      token_limit: wholeNumber(fields.token_limit),
      rate_limit_rpm: wholeNumber(fields.rate_limit_rpm),
      expires_at,
    },
  };
}

/**
 * The plaintext key a create response is allowed to reveal, or null.
 *
 * docs/website/06-api-keys-and-limits.md lines 122-124: "**The plaintext key is
 * shown exactly once**, at creation. It is never stored and never retrievable."
 * Two rules follow, and this is both of them in one place:
 *
 * 1. **Only the create response can reveal a key.** Nothing else in the app has
 *    one to show, so there is no second call site that could try.
 * 2. **A missing field is never invented.** The keys island already refuses to
 *    fabricate one ("never invent a key", KeyManagement.astro); the same rule
 *    applies here, and the page surfaces the missing field instead.
 */
export function plaintextKeyOf(response: { key?: unknown } | null | undefined): string | null {
  const key = response?.key;
  return typeof key === 'string' && key.length > 0 ? key : null;
}

/**
 * A problem with a password change, or null when it may be sent.
 *
 * docs/website/03-functional-spec.md line 151: "Change password (requires current
 * password)". The current password is checked for presence only — the server is
 * the only thing that can say whether it is right, and a client that claimed
 * otherwise would be guessing at a credential.
 *
 * The new-password rules are the ones /signup and /reset/confirm already apply,
 * reused rather than restated, so a rule cannot be relaxed on one page and
 * enforced on another.
 */
export function passwordChangeError(
  current: string,
  next: string,
  confirm: string,
): string | null {
  if (current.length === 0) return 'Enter your current password.';
  return passwordRuleError(next) ?? confirmationError(next, confirm);
}

/**
 * The Telegram link flow, as pure logic.
 *
 * docs/server/api-spec.md: `POST /api/telegram/link-code` issues a 6-digit
 * code — single-use, 5-minute TTL, bound to the account — and `DELETE
 * /api/telegram` unlinks. The server owns issuance, expiry and the hourly cap;
 * these helpers only format what it returns and validate what the user typed.
 */

/** The body of `POST /api/telegram/link-code` (server/src/routes/telegram.rs). */
export interface LinkCodeResponse {
  code: string;
  expires_at: string;
  ttl_minutes: number;
}

/**
 * The Telegram code as the user should read it back.
 *
 * The server zero-pads the code ("000042") because redemption compares the
 * string, so the leading zeros ARE the code. Trimming or stripping them would
 * send the bot a different value than was issued, so this preserves them and
 * only guards against an unexpectedly short code by refusing to pad in a way
 * that changes meaning — it returns the code unchanged.
 */
export function formatLinkCode(code: string): string {
  return code.trim();
}

/**
 * Whether the typed code is the 6-digit shape `/link` expects.
 *
 * The bot redeems `{code, telegram_id}`; a code that is not six digits cannot
 * match any issued code, so the dashboard refuses to show a "sent" state for it.
 * Digits only — the server draws from `next_u32() % 1_000_000` and zero-pads,
 * so letters never occur.
 */
export function isPlausibleLinkCode(candidate: string): boolean {
  return /^\d{6}$/.test(candidate.trim());
}

/**
 * The instruction text to show beside an issued code.
 *
 * It names the exact chat command so the user does not have to guess the syntax,
 * and states the TTL the server reported rather than a hardcoded number — a
 * hardcoded "5 minutes" would silently lie if the TTL changed.
 */
export function linkCodeInstruction(code: string, ttlMinutes: number): string {
  return `Send /link ${formatLinkCode(code)} to the apikita bot in Telegram. The code works once and expires in ${ttlMinutes} minute${ttlMinutes === 1 ? '' : 's'}.`;
}

/**
 * Which stored limit a field is being compared against, for the TTL warning.
 *
 * The create form has no previous value; an EDIT does, and the difference drives
 * a real user-facing consequence (see `describeLimitChange`).
 */
export interface ExistingKeyLimits {
  spend_limit_idr: number;
  token_limit: number;
  rate_limit_rpm: number;
}

/** The edit form's raw fields — the same shape as create, so one parser serves both. */
export type UpdateKeyFields = CreateKeyFields;

/**
 * The `PATCH /api/keys/:id` body, or the first reason it must not be sent.
 *
 * Every field the API accepts is optional (server/src/routes/keys.rs:72-80), and
 * this always sends the full set the form shows rather than a diff: the form is
 * the source of truth for what the key should be, and sending only changed fields
 * would silently preserve a value the user deliberately cleared. It reuses
 * `buildCreateKeyRequest`'s validation verbatim — same limits, same expiry rules,
 * same "impossible date is refused" behaviour — so create and edit cannot drift.
 */
export function buildUpdateKeyRequest(fields: UpdateKeyFields): CreateKeyBuild {
  return buildCreateKeyRequest(fields);
}

/**
 * Whether an edit LOWERS any limit, and therefore takes up to the metadata cache
 * TTL to apply.
 *
 * docs/server/api-spec.md:397-398 — "Raising takes effect immediately; lowering is
 * subject to the proxy's metadata cache TTL (≤60s). The response should say so,
 * so the UI can warn honestly." This is that warning's trigger.
 *
 * Only a STRICT decrease counts. Raising, or leaving a limit unchanged, is not a
 * reduction and must not raise the warning — a warning shown when nothing was
 * lowered is noise the user learns to ignore.
 */
export function lowersAnyLimit(
  before: ExistingKeyLimits,
  after: ExistingKeyLimits,
): boolean {
  return (
    after.spend_limit_idr < before.spend_limit_idr ||
    after.token_limit < before.token_limit ||
    after.rate_limit_rpm < before.rate_limit_rpm
  );
}

/**
 * The warning to show when an edit lowers a limit, or null when it does not.
 *
 * The sentence is deliberately specific about the window rather than saying
 * "shortly": the whole point of telling the user is that a reduction is NOT
 * instant, and a vague promise would defeat it.
 */
export function limitReductionWarning(
  before: ExistingKeyLimits,
  after: ExistingKeyLimits,
): string | null {
  if (!lowersAnyLimit(before, after)) return null;
  return 'You lowered a limit. The proxy applies a reduction within its metadata cache TTL, up to 60 seconds — until then, requests a reduced limit would refuse can still pass. A RAISE takes effect immediately.';
}