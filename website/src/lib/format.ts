const idr = new Intl.NumberFormat('id-ID', {
  style: 'currency',
  currency: 'IDR',
  maximumFractionDigits: 0,
});

export function formatIdr(value: number): string {
  return idr.format(value);
}

export function formatCount(value: number): string {
  return new Intl.NumberFormat('en-US').format(value);
}

/**
 * The timestamp column of a top-up history row, in the visitor's local clock.
 *
 * WHY THIS EXISTS rather than an inline `new Date(x).toLocaleString()`: that expression returns the
 * literal string `"Invalid Date"` for a value it cannot parse, and the top-up history is a deposit
 * record - printing a word that looks like a date into one is worse than printing the malformed
 * input, which at least tells a reader what actually arrived.
 *
 * NOT A FIX FOR A LIVE BUG, and the distinction is worth keeping. `created_at` is a
 * `chrono::DateTime<Utc>` in `server/src/routes/account.rs`, which always serialises to RFC3339, so
 * this server cannot currently produce a value that fails here. It is a boundary guard.
 *
 * THE RULE IS NOT NEW - it is the one the sibling formatters already follow, and one of them
 * already said so: `lib/recent-usage.ts` documents "an unparseable value returns the raw string,
 * never 'Invalid Date'" and names "the top-up history" as stating the same rule. That sentence was
 * true of the rule and not of this file, whose history row used the unguarded expression. This
 * function is what makes the citation accurate.
 *
 * MEASURED BEFORE IT WAS ADDED: replacing the row's timestamp cell with the literal `'Invalid Date'`
 * - and deleting the cell outright - both left the website suite at 226 pass / 0 fail. The row had
 * no coverage at all, while the equivalent guard in `lib/admin.ts` had some.
 */
export function formatTopupTime(iso: string): string {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) return iso;
  return new Date(ms).toLocaleString();
}
