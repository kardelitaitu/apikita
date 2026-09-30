// The native identity client: the public auth pages' one entry point.
//
// THIS REPLACES lib/pocketbase.ts. Identity used to live in PocketBase, so these
// pages were SDK calls from the browser and the Rust API had exactly three auth
// verbs (/auth/exchange, /auth/logout, /auth/logout-all). The port moved identity
// into the Rust API, so the calls are now ours and each one sets its own session
// cookie — there is no token exchange and no third-party SDK in the bundle.
//
// Why the browser does the Google flow and not the server: Google Identity
// Services hands the page an ID token, and the page posts that ONE token to
// /auth/google. The server verifies the signature, the issuer, the audience and
// the expiry (identity/google.rs) and then decides which account it belongs to.
// The server never holds a client secret, never runs a redirect dance, and never
// sees a Google password — which is also why there is no OAuth code to exchange
// on this side.
//
// docs/website/03-functional-spec.md's neutrality rule governs every reply here:
// a page must not reveal whether an address is registered, and the one thing it
// may admit to is that the request never reached the server. `signupReply` and
// `resetRequestReply` in auth-flow.ts own that wording; this module only makes
// the calls.

import { API_BASE, ApiError } from './api.ts';

/** What every native auth verb answers a success with. */
export interface SessionResult {
  account_id: string;
  balance_idr: number;
}

/**
 * The Google ID token's claim shape, as far as the page reads it.
 *
 * The page reads `credential` ONLY. It does not inspect the claims — deciding
 * whether the token is real is the server's job, and a page that pre-checked them
 * would be a second, weaker verifier.
 */
interface GoogleCredentialResponse {
  credential?: unknown;
}

interface GoogleIdentityServices {
  accounts: {
    id: {
      initialize(config: { client_id: string; callback: (r: GoogleCredentialResponse) => void }): void;
      renderButton(parent: HTMLElement, options: Record<string, unknown>): void;
    };
  };
}

declare global {
  interface Window {
    google?: GoogleIdentityServices;
  }
}

/**
 * The Google client id, as a build-time PUBLIC_* variable.
 *
 * PUBLIC because it is public by design: it travels in a URL, it is not a secret,
 * and the server verifies the resulting token independently. The CLIENT SECRET is
 * the thing that must never reach a browser, and this design does not have one.
 */
export const GOOGLE_CLIENT_ID: string =
  import.meta.env?.PUBLIC_GOOGLE_CLIENT_ID ?? '';

/** The GIS script, loaded once. */
const GIS_SRC = 'https://accounts.google.com/gsi/client';
let gisPromise: Promise<void> | null = null;

function loadGis(): Promise<void> {
  if (typeof window === 'undefined') return Promise.resolve();
  if (window.google?.accounts?.id) return Promise.resolve();
  if (gisPromise !== null) return gisPromise;

  gisPromise = new Promise<void>((resolve, reject) => {
    const script = document.createElement('script');
    script.src = GIS_SRC;
    script.async = true;
    script.defer = true;
    script.onload = () => resolve();
    script.onerror = () => {
      // Reset so a later click can retry — a permanent rejection would make the
      // button dead for the rest of the page's life after one flaky load.
      gisPromise = null;
      reject(new Error('Could not load Google sign-in.'));
    };
    document.head.appendChild(script);
  });

  return gisPromise;
}

/**
 * Runs the Google popup and resolves with the ID token.
 *
 * A PROMISE rather than a rendered button because the pages already own their own
 * button markup and styling; `renderButton` would replace it with an iframe whose
 * appearance we cannot match and whose click handler we cannot disable during a
 * request. `initialize` + `prompt()` keeps the existing button and the existing
 * busy state.
 *
 * If `PUBLIC_GOOGLE_CLIENT_ID` is unset the returned promise rejects with a plain
 * Error, which the pages render with their generic copy. That is the honest
 * behaviour for a deployment with Google sign-in switched off — better than a
 * button that silently does nothing.
 */
export async function googleIdToken(): Promise<string> {
  if (GOOGLE_CLIENT_ID === '') {
    throw new Error('Google sign-in is not configured.');
  }

  await loadGis();

  const gis = window.google;
  if (!gis?.accounts?.id) {
    throw new Error('Could not load Google sign-in.');
  }

  return new Promise<string>((resolve, reject) => {
    gis.accounts.id.initialize({
      client_id: GOOGLE_CLIENT_ID,
      callback: (response: GoogleCredentialResponse) => {
        if (typeof response.credential === 'string' && response.credential.length > 0) {
          resolve(response.credential);
        } else {
          // A dismissed or failed popup. Not an error worth differentiating: the
          // pages show the same neutral copy either way.
          reject(new Error('Google sign-in was not completed.'));
        }
      },
    });

    // `prompt` is not part of the typed surface above because `initialize` is all
    // this module needs to declare; the GIS object does carry it.
    const prompt = (gis.accounts.id as unknown as { prompt?: () => void }).prompt;
    if (typeof prompt === 'function') {
      prompt.call(gis.accounts.id);
    } else {
      reject(new Error('Could not load Google sign-in.'));
    }
  });
}

/** POSTs a JSON body to a native auth verb, with the session cookie. */
async function postAuth<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(API_BASE + path, {
    method: 'POST',
    // The session cookie is HttpOnly and set by the response to this call, so the
    // request must opt into credentials — without it the browser would drop it
    // and every later /api call would be signed out.
    credentials: 'include',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(body),
  });

  if (!res.ok) {
    let parsed: { error?: { code: string; message: string; request_id?: string; details?: Record<string, unknown> } } | null = null;
    try {
      parsed = (await res.json()) as typeof parsed;
    } catch {
      // A non-JSON body: fall through to the status line, exactly as apiFetch does.
    }
    throw new ApiError(
      res.status,
      parsed?.error ?? null,
      res.statusText,
      null,
    );
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/**
 * POST /auth/signup.
 *
 * The caller is told NOTHING about whether the address was already registered:
 * the endpoint answers with the same neutral reply either way, and this function
 * has no branch that could distinguish them.
 */
export function signup(email: string, password: string): Promise<SessionResult | null> {
  return postAuth<SessionResult | null>('/auth/signup', { email, password });
}

/** POST /auth/login. A 401 here is a wrong email/password, not a missing session. */
export function login(email: string, password: string): Promise<SessionResult> {
  return postAuth<SessionResult>('/auth/login', { email, password });
}

/** POST /auth/google with the GIS credential. */
export function googleSignIn(idToken: string): Promise<SessionResult> {
  return postAuth<SessionResult>('/auth/google', { id_token: idToken });
}

/** POST /auth/verify with the token from the link. */
export function verifyEmail(token: string): Promise<void> {
  return postAuth<void>('/auth/verify', { token });
}

/** POST /auth/reset — asks for a reset link. Neutral by construction. */
export function requestPasswordReset(email: string): Promise<void> {
  return postAuth<void>('/auth/reset', { email });
}

/** POST /auth/reset/confirm with the token and the new password. */
export function confirmPasswordReset(token: string, password: string): Promise<void> {
  return postAuth<void>('/auth/reset/confirm', { token, password });
}

/** POST /auth/verification/resend — asks for another verification link. */
export function resendVerification(email: string): Promise<void> {
  return postAuth<void>('/auth/verification/resend', { email });
}
