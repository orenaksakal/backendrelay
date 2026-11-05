import assert from 'node:assert/strict';
import test from 'node:test';

import { describeRun, latencyHolds, limitHolds, summary } from './abuse-model.mjs';

test('summary keeps an empty sample distinguishable from a fast one', () => {
  assert.deepEqual(summary([]), {
    count: 0,
    p50Ms: null,
    p95Ms: null,
    p99Ms: null,
    maxMs: null,
  });
  assert.equal(summary([10]).p95Ms, 10);
});

test('summary reports nearest-rank percentiles and the maximum', () => {
  assert.deepEqual(summary([5, 1, 3]), {
    count: 3,
    p50Ms: 3,
    p95Ms: 5,
    p99Ms: 5,
    maxMs: 5,
  });
  assert.equal(summary([1, 2, 3, 100]).p95Ms, 100);
});

test('app B p95 may grow by the factor or up to the floor, never past both', () => {
  const baseline = { p95Ms: 1000 };
  assert.equal(latencyHolds(baseline, { p95Ms: 1400 }).ok, true);
  assert.equal(latencyHolds(baseline, { p95Ms: 1600 }).ok, false);
  assert.equal(latencyHolds({ p95Ms: 100 }, { p95Ms: 240 }).ok, true);
  assert.equal(latencyHolds({ p95Ms: 100 }, { p95Ms: 260 }).ok, false);
  assert.equal(latencyHolds(baseline, { p95Ms: null }).ok, false);
  assert.equal(latencyHolds({ p95Ms: null }, { p95Ms: 10 }).ok, false);
});

test('a limit holds only when the refusal is the rate-limited error', () => {
  const refusal = { status: 429, type: 'rate-limited' };
  assert.deepEqual(limitHolds({ accepted: 30, limit: 30, refusal }), {
    ok: true,
    fresh: true,
    reason: 'limit reached at exactly 30',
  });
  const partial = limitHolds({ accepted: 7, limit: 30, refusal });
  assert.equal(partial.ok, true);
  assert.equal(partial.fresh, false);
  assert.equal(limitHolds({ accepted: 31, limit: 30, refusal }).ok, false);
  assert.equal(limitHolds({ accepted: 30, limit: 30, refusal: { status: 429 } }).ok, false);
  assert.equal(limitHolds({ accepted: 30, limit: 30, refusal: { status: 503, type: 'rate-limited' } }).ok, false);
  assert.equal(limitHolds({ accepted: 0, limit: 30, refusal }).ok, false);
});

test('a run is failed by any failed check and names those checks', () => {
  assert.deepEqual(describeRun({ checks: [{ name: 'a', ok: true }] }), {
    result: 'passed',
    total: 1,
    failed: [],
  });
  assert.deepEqual(describeRun({ checks: [{ name: 'a', ok: true }, { name: 'b', ok: false }] }), {
    result: 'failed',
    total: 2,
    failed: ['b'],
  });
  assert.deepEqual(describeRun({}), { result: 'passed', total: 0, failed: [] });
});
