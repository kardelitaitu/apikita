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
 * The models the proxy currently exposes: config/apikita.toml, [[models]]
 * `name = ...`. The same two the KeyManagement island offers, because a page
 * that described a different set would be describing a different product.
 */
export const ENABLED_MODELS: readonly string[] = ['flash', 'deepseek-v4-flash'];

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
  enabled: readonly string[] = ENABLED_MODELS,
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
 */
export function limitError(raw: string, label: string): string | null {
  const text = raw.trim();
  if (text === '') return null;
  if (!/^\d+$/.test(text)) {
    return `${label} must be a whole number of 0 or more. Leave it blank or 0 for no limit.`;
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
