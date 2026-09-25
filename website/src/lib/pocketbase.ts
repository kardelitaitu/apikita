import PocketBase from 'pocketbase';
import { apiFetch } from './api';

/** Identity lives in PocketBase; the origin is a PUBLIC_* build-time variable. */
export const POCKETBASE_URL: string =
  import.meta.env.PUBLIC_POCKETBASE_URL ?? 'http://127.0.0.1:8090';

export const pb = new PocketBase(POCKETBASE_URL);

export interface ExchangeResult {
  account_id: string;
  balance_idr: number;
}

/**
 * docs/architecture/identity.md: PocketBase authenticates the person, the Rust
 * API issues its own opaque session cookie. The PocketBase token is posted
 * exactly once and is never used as the API credential.
 */
export async function exchangeForSession(): Promise<ExchangeResult> {
  const token = pb.authStore.token;
  if (!token) throw new Error('No PocketBase session to exchange.');
  return apiFetch<ExchangeResult>('/auth/exchange', {
    method: 'POST',
    body: JSON.stringify({ pb_token: token }),
    redirectOn401: false,
  });
}
