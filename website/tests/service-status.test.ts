// Executable contract for the service-status indicator logic
// (src/lib/service-status.ts).
//
// The rule that matters most: the indicator may report REACHABILITY of the
// platform, never the health of an upstream provider. docs/observability.md:193
// forbids /health from checking upstreams, so a badge claiming "all systems
// operational" of the model providers would be asserting something the endpoint
// cannot know. The label/detail copy is pinned here so that stays true.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  healthPath,
  statusDetail,
  statusFromResponse,
  statusLabel,
  type ServiceStatus,
} from '../src/lib/service-status.ts';

const ALL: ServiceStatus[] = ['checking', 'operational', 'degraded', 'unreachable'];

test('every state has a label and a detail, and they are distinct', () => {
  const labels = ALL.map(statusLabel);
  const details = ALL.map(statusDetail);
  for (const label of labels) assert.ok(label.length > 0);
  for (const detail of details) assert.ok(detail.length > 0);
  // No two states read the same — a badge that cannot be told apart is useless.
  assert.equal(new Set(labels).size, ALL.length, labels.join(' | '));
});

test('a 200 healthy is operational', () => {
  assert.equal(statusFromResponse(200, { status: 'healthy', database: 'connected' }), 'operational');
});

test('a 503 degraded is degraded, not unreachable', () => {
  // The platform answered. "Something it depends on is down" is a different fact
  // from "we could not reach it", and the two must not collapse.
  assert.equal(statusFromResponse(503, { status: 'degraded', database: 'database unavailable' }), 'degraded');
});

test('no response is unreachable', () => {
  assert.equal(statusFromResponse(0, null), 'unreachable');
});

test('a malformed or unexpected body is never shown as healthy', () => {
  // A 200 with no body, or a body that is not the documented shape.
  assert.equal(statusFromResponse(200, null), 'unreachable');
  assert.equal(statusFromResponse(200, { status: 'ok', database: 'connected' }), 'unreachable');
  // A 500 with a valid-looking body is still not healthy.
  assert.equal(statusFromResponse(500, { status: 'healthy', database: 'connected' }), 'unreachable');
  // The two codes are accepted ONLY with their matching status string, so a
  // 503 that happens to say "healthy" is refused.
  assert.equal(statusFromResponse(503, { status: 'healthy', database: 'connected' }), 'unreachable');
});

test('the copy never claims to know upstream health', () => {
  // The words that would overstate. If any appears, the indicator is promising
  // something /health cannot report.
  for (const status of ALL) {
    const text = (statusLabel(status) + ' ' + statusDetail(status)).toLowerCase();
    for (const forbidden of ['upstream', 'provider', 'model']) {
      assert.ok(!text.includes(forbidden), `${status} overstates: ${text}`);
    }
  }
});

test('the health path is the documented one', () => {
  assert.equal(healthPath(), '/health');
});
