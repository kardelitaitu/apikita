// The login page's one notice element.
//
// Login used to build this line itself — `${message} (request_id: ${id})` — inside
// an `.astro` script block. Neither `tsc` nor the suite can see those blocks, so
// the format could drift with every 429 still green, and it did drift from the
// shared one-line renderer. The wiring now lives here so the suite exercises it,
// and it delegates both the message and the correlation id to the shared
// renderers (`describeError` then `inlineNotice`) so every one-line 429 reads the
// same. Not a screen of its own: the page keeps no format.
//
// Node loads this module directly (the test suite), which is why the import
// carries an explicit `.ts` extension.

import { describeError, inlineNotice } from './errors.ts';

/** The single element the login notice writes to. */
export interface NoticeText {
  textContent: string | null;
}

/**
 * Render a failed sign-in into login's one notice element.
 *
 * The whole error view is handed to the shared `inlineNotice`, so the notice is
 * `<message> (<request_id>)` — the same shape the keys modal shows. Login
 * previously wrote `<message> (request_id: <id>)`; the shared shape drops the
 * redundant `request_id:` label and shows the same id.
 */
export function renderLoginError(target: NoticeText, err: unknown): void {
  target.textContent = inlineNotice(describeError(err));
}
