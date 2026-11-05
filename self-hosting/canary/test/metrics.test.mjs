import assert from 'node:assert/strict';
import test from 'node:test';

import {
  createRecorder,
  PROPAGATION_BUCKETS,
  renderMetrics,
} from '../src/metrics.mjs';

test('renders zeroed metrics before the first probe', () => {
  const text = renderMetrics(createRecorder(1_700_000_000_000).snapshot());
  assert.match(text, /# TYPE backendrelay_canary_propagation_seconds histogram/);
  assert.match(text, /backendrelay_canary_propagation_seconds_bucket\{le="\+Inf"\} 0/);
  assert.match(text, /backendrelay_canary_propagation_seconds_sum 0/);
  assert.match(text, /backendrelay_canary_propagation_seconds_count 0/);
  for (const outcome of ['success', 'timeout', 'error']) {
    assert.match(
      text,
      new RegExp(`backendrelay_canary_runs_total\\{outcome="${outcome}"\\} 0`),
    );
  }
  assert.match(
    text,
    /backendrelay_canary_last_success_timestamp_seconds 0\n/,
  );
  assert.match(
    text,
    /backendrelay_canary_started_timestamp_seconds 1700000000\n$/,
  );
});

test('records cumulative histogram buckets and run outcomes', () => {
  const recorder = createRecorder();
  recorder.recordSuccess(0.05, 1_700_000_000_500);
  recorder.recordSuccess(0.2, 1_700_000_120_900);
  recorder.recordFailure('timeout');
  recorder.recordFailure('error');
  const snapshot = recorder.snapshot();
  const counts = Object.fromEntries(
    snapshot.buckets.map(({ le, count }) => [String(le), count]),
  );
  assert.equal(counts['0.01'], 0);
  assert.equal(counts['0.05'], 1);
  assert.equal(counts['0.1'], 1);
  assert.equal(counts['0.25'], 2);
  assert.equal(counts[String(PROPAGATION_BUCKETS.at(-1))], 2);
  assert.equal(snapshot.observations, 2);
  assert.ok(Math.abs(snapshot.sum - 0.25) < 1e-9);
  assert.equal(snapshot.runs.success, 2);
  assert.equal(snapshot.runs.timeout, 1);
  assert.equal(snapshot.runs.error, 1);
  assert.equal(snapshot.lastSuccess, 1_700_000_120);

  const text = renderMetrics(snapshot);
  assert.match(text, /backendrelay_canary_propagation_seconds_bucket\{le="0.05"\} 1/);
  assert.match(text, /backendrelay_canary_propagation_seconds_bucket\{le="\+Inf"\} 2/);
  assert.match(text, /backendrelay_canary_propagation_seconds_sum 0.25/);
  assert.match(text, /backendrelay_canary_runs_total\{outcome="timeout"\} 1/);
  assert.match(
    text,
    /backendrelay_canary_last_success_timestamp_seconds 1700000120\n/,
  );
});

test('rejects invalid recordings', () => {
  const recorder = createRecorder();
  assert.throws(() => recorder.recordSuccess(-1));
  assert.throws(() => recorder.recordSuccess(Number.NaN));
  assert.throws(() => recorder.recordFailure('success'));
  assert.throws(() => recorder.recordFailure('unknown'));
});
