// The 429 wait: one pure rule, no imports.
//
// docs/error-model.md: a 429 carries `Retry-After` in SECONDS, floored at 1 by
// the server. An hourly cap is 3600 of them, so the copy has to come from the
// header — a fixed "wait a moment" understates the real wait by ~3600x and sends
// the user straight back into the refusal.
//
// Dependency-free on purpose: this is the whole contract of what a rate-limited
// user is told, so it is exercised directly by the test suite rather than through
// a browser, and there is exactly one place that turns a header into a promise.

const MINUTE = 60;
const HOUR = 3600;
const DAY = 86_400;

/**
 * Whole decimal seconds from a `Retry-After` header value, or null.
 *
 * The only place raw header text becomes a number, so the shape is checked
 * before the value is trusted. docs/error-model.md fixes the format as seconds —
 * a whole, positive count — so that is all that is read: a fraction, exponent
 * (`1e3`), hex (`0x10`), the HTTP-date form, zero, a negative, or anything
 * non-numeric reads as "no wait" rather than being coerced into a guessed one.
 * `Number()` alone accepts four of those, which is why it is not the only check.
 */
export function parseRetryAfter(header: string | null): number | null {
  const text = header?.trim() ?? '';
  if (!/^\d+$/.test(text)) return null;
  const seconds = Number(text);
  return Number.isSafeInteger(seconds) && seconds > 0 ? seconds : null;
}

/**
 * What a rate-limited user is told for a wait of `seconds`, or none.
 *
 * Units keep the number readable, and anything past a minute is "about": the
 * header bounds the wait, it is not a schedule. Every step rounds UP, because a
 * promise shorter than the window is the defect being fixed here.
 *
 * Precondition: `seconds` comes from `parseRetryAfter`, so it is a positive
 * integer or null.
 */
export function rateLimitedMessage(seconds: number | null): string {
  if (seconds === null) return 'Too many requests. Try again later.';
  if (seconds < MINUTE) return line(plural(seconds, 'second'));
  if (seconds < HOUR) return line(`about ${plural(Math.ceil(seconds / MINUTE), 'minute')}`);
  if (seconds < DAY) return line(`about ${plural(Math.ceil(seconds / HOUR), 'hour')}`);
  return line(`about ${plural(Math.ceil(seconds / DAY), 'day')}`);
}

function line(wait: string): string {
  return `Too many requests. Try again in ${wait}.`;
}

function plural(count: number, unit: string): string {
  return `${count} ${unit}${count === 1 ? '' : 's'}`;
}
