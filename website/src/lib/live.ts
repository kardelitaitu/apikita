// The live-balance store: SSE first, polling as the fallback.
//
// The contract is docs/realtime.md. Two rules it makes explicit and this file
// obeys: events are ABSOLUTE, never deltas, and on error the last value is kept
// and marked stale — never blanked, never shown as live.

import { ApiError, apiFetch, API_BASE, type Me, type UsageToday } from './api.ts';

/** docs/realtime.md: poll every 30-60s. Never fake realtime with tight polling. */
export const POLL_INTERVAL_MS = 30_000;

/** Bounded backoff for recreating a permanently-closed stream: ~1s, 2s, 4s, then 8s forever. */
export const RETRY_BASE_MS = 1_000;
export const RETRY_MAX_MS = 8_000;

/**
 * The retry policy, as a pure function so it can be checked without a browser.
 *
 * EventSource only auto-reconnects after a PREVIOUSLY SUCCESSFUL connection drops.
 * When its FIRST response is not a 200 text/event-stream (API down, session gone,
 * an intermediary in the way) it CLOSES for good and never retries — so we must
 * recreate it ourselves. Doubling from 1s and capped at 8s: a cold start recovers
 * in about a second, and an outage costs one request per 8s instead of a hot loop.
 */
export function nextRetryDelay(attempt: number): number {
  const step = Math.max(0, Math.floor(attempt));
  return Math.min(RETRY_BASE_MS * 2 ** step, RETRY_MAX_MS);
}

export type LiveStatus = 'connecting' | 'live' | 'stale';

export interface LiveState {
  status: LiveStatus;
  /** null until the first snapshot arrives; rendered as loading, not as zero. */
  balanceIdr: number | null;
  usage: UsageToday | null;
  /** Key ids the stream has reported revoked, for cross-tab consistency. */
  revokedKeyIds: string[];
  /** Set when the last request failed for a reason other than 401. */
  error: string | null;
  requestId: string | null;
}

export type LiveListener = (state: LiveState) => void;

export interface LiveStore {
  getState(): LiveState;
  subscribe(listener: LiveListener): () => void;
  start(): void;
  stop(): void;
}

/**
 * One store per document. Anchored on window rather than on module identity so
 * the layout's script and a page's script share it even if the bundler emits
 * them as separate chunks.
 */
export function getLiveStore(): LiveStore {
  const holder = window as unknown as { __apikitaLive?: LiveStore };
  holder.__apikitaLive ??= createLiveStore();
  return holder.__apikitaLive;
}

export function createLiveStore(): LiveStore {
  let state: LiveState = {
    status: 'connecting',
    balanceIdr: null,
    usage: null,
    revokedKeyIds: [],
    error: null,
    requestId: null,
  };

  const listeners = new Set<LiveListener>();
  let source: EventSource | null = null;
  let pollTimer: number | null = null;
  let retryTimer: number | null = null;
  /** Retries since the last successful open; reset on open. */
  let attempt = 0;
  /** True after stop(), so a pending retry does not resurrect the stream. */
  let stopped = false;

  function set(patch: Partial<LiveState>): void {
    state = { ...state, ...patch };
    for (const listener of listeners) listener(state);
  }

  function applyMe(me: Me): void {
    set({ balanceIdr: me.balance_idr, usage: me.usage_today, error: null, requestId: null });
  }

  /** Returns null when the session is gone — apiFetch has already redirected. */
  async function loadMe(): Promise<Me | null> {
    try {
      return await apiFetch<Me>('/api/me');
    } catch (err) {
      if (err instanceof ApiError) {
        if (err.status === 401) return null;
        set({ error: err.message, requestId: err.requestId });
        return null;
      }
      set({ error: err instanceof Error ? err.message : 'Request failed.' });
      return null;
    }
  }

  function stopPolling(): void {
    if (pollTimer === null) return;
    window.clearInterval(pollTimer);
    pollTimer = null;
  }

  /**
   * The fallback path. It keeps the stale badge up on purpose: polled data is
   * a last known value, not live data.
   */
  function startPolling(): void {
    if (pollTimer !== null) return;
    void loadMe().then((me) => {
      if (me) applyMe(me);
    });
    pollTimer = window.setInterval(() => {
      void loadMe().then((me) => {
        if (me) applyMe(me);
      });
    }, POLL_INTERVAL_MS);
  }

  /**
   * Recreate the stream after it closed for good — a cold start whose first
   * response was not the event stream never gets another chance otherwise.
   */
  function scheduleRetry(): void {
    if (stopped || retryTimer !== null) return;
    const delay = nextRetryDelay(attempt);
    attempt += 1;
    retryTimer = window.setTimeout(() => {
      retryTimer = null;
      connect();
    }, delay);
  }

  function connect(): void {
    // A fresh EventSource replaces the dead one; the old one stays closed.
    source?.close();
    const es = new EventSource(`${API_BASE}/events`, { withCredentials: true });
    source = es;

    // Connected: the server sends a full snapshot on connect, so polling stops
    // here and the badge clears when that snapshot lands.
    es.addEventListener('open', () => {
      attempt = 0;
      stopPolling();
      set({ status: 'live', error: null, requestId: null });
    });

    es.addEventListener('balance', (event) => {
      const data = JSON.parse((event as MessageEvent).data) as { balance_idr: number };
      set({ balanceIdr: data.balance_idr, status: 'live', error: null, requestId: null });
    });

    es.addEventListener('usage', (event) => {
      const data = JSON.parse((event as MessageEvent).data) as UsageToday;
      set({ usage: data, status: 'live', error: null, requestId: null });
    });

    es.addEventListener('key', (event) => {
      const data = JSON.parse((event as MessageEvent).data) as {
        key_id: string;
        revoked_at: string | null;
      };
      if (data.revoked_at && !state.revokedKeyIds.includes(data.key_id)) {
        set({ revokedKeyIds: [...state.revokedKeyIds, data.key_id] });
      }
    });

    // Two different things arrive on 'error': EventSource's own connection
    // failure (no data) and the terminal `event: error` frame docs/error-model.md
    // defines for a stream that died mid-answer (has data). Both mean the
    // stream is no longer live.
    es.addEventListener('error', (event) => {
      const data = (event as MessageEvent).data;
      let message: string | null = null;
      let requestId: string | null = null;
      if (typeof data === 'string' && data.length > 0) {
        try {
          const parsed = JSON.parse(data) as { error?: { message?: string; request_id?: string } };
          message = parsed.error?.message ?? null;
          requestId = parsed.error?.request_id ?? null;
        } catch {
          message = null;
        }
      }
      // Keep the last value, mark it old, and keep the poll fallback running.
      set({ status: 'stale', error: message, requestId });
      startPolling();
      // CLOSED means EventSource has given up (or never connected at all); only
      // then is recreating it our job. CONNECTING means it is already retrying.
      if (es.readyState === EventSource.CLOSED) {
        if (source === es) source = null;
        scheduleRetry();
      }
    });
  }

  return {
    getState: () => state,

    subscribe(listener: LiveListener): () => void {
      listeners.add(listener);
      listener(state);
      return () => {
        listeners.delete(listener);
      };
    },

    start(): void {
      stopped = false;
      void (async () => {
        // First paint, and the cheapest way to find out the session is dead.
        const me = await loadMe();
        if (me === null && state.balanceIdr === null && state.error === null) return;
        if (me) applyMe(me);
        connect();
      })();
    },

    stop(): void {
      stopped = true;
      stopPolling();
      if (retryTimer !== null) {
        window.clearTimeout(retryTimer);
        retryTimer = null;
      }
      source?.close();
      source = null;
    },
  };
}
