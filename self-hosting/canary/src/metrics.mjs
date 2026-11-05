export const PROPAGATION_BUCKETS = Object.freeze([
  0.01,
  0.025,
  0.05,
  0.1,
  0.25,
  0.5,
  1,
  2.5,
  5,
  10,
  30,
]);
export const RUN_OUTCOMES = Object.freeze(['success', 'timeout', 'error']);

export function createRecorder(startedAt = Date.now()) {
  const buckets = PROPAGATION_BUCKETS.map((le) => ({ le, count: 0 }));
  const runs = { success: 0, timeout: 0, error: 0 };
  let observations = 0;
  let sum = 0;
  let lastSuccess = 0;
  const started = Math.floor(startedAt / 1000);

  return {
    recordSuccess(seconds, at = Date.now()) {
      if (!Number.isFinite(seconds) || seconds < 0) {
        throw new Error('propagation time must be a non-negative number');
      }
      if (!Number.isFinite(at)) {
        throw new Error('success timestamp must be a number');
      }
      observations += 1;
      sum += seconds;
      for (const bucket of buckets) {
        if (seconds <= bucket.le) {
          bucket.count += 1;
        }
      }
      runs.success += 1;
      lastSuccess = Math.floor(at / 1000);
    },
    recordFailure(outcome) {
      if (outcome !== 'timeout' && outcome !== 'error') {
        throw new Error(`unknown canary outcome: ${outcome}`);
      }
      runs[outcome] += 1;
    },
    snapshot() {
      return {
        buckets: buckets.map(({ le, count }) => ({ le, count })),
        observations,
        sum,
        lastSuccess,
        started,
        runs: { ...runs },
      };
    },
  };
}

function metricNumber(value) {
  if (!Number.isFinite(value)) {
    return '0';
  }
  return String(Number(value.toFixed(6)));
}

export function renderMetrics(snapshot) {
  const lines = [];
  lines.push(
    '# HELP backendrelay_canary_propagation_seconds Write-to-subscribed-refresh propagation time in seconds.',
  );
  lines.push('# TYPE backendrelay_canary_propagation_seconds histogram');
  for (const bucket of snapshot.buckets) {
    lines.push(
      `backendrelay_canary_propagation_seconds_bucket{le="${bucket.le}"} ${bucket.count}`,
    );
  }
  lines.push(
    `backendrelay_canary_propagation_seconds_bucket{le="+Inf"} ${snapshot.observations}`,
  );
  lines.push(`backendrelay_canary_propagation_seconds_sum ${metricNumber(snapshot.sum)}`);
  lines.push(
    `backendrelay_canary_propagation_seconds_count ${snapshot.observations}`,
  );
  lines.push('# HELP backendrelay_canary_runs_total Canary probe runs by outcome.');
  lines.push('# TYPE backendrelay_canary_runs_total counter');
  for (const outcome of RUN_OUTCOMES) {
    lines.push(
      `backendrelay_canary_runs_total{outcome="${outcome}"} ${snapshot.runs[outcome] ?? 0}`,
    );
  }
  lines.push(
    '# HELP backendrelay_canary_last_success_timestamp_seconds Unix time of the last successful probe; zero before the first success.',
  );
  lines.push('# TYPE backendrelay_canary_last_success_timestamp_seconds gauge');
  lines.push(
    `backendrelay_canary_last_success_timestamp_seconds ${snapshot.lastSuccess}`,
  );
  lines.push(
    '# HELP backendrelay_canary_started_timestamp_seconds Unix time the canary process started.',
  );
  lines.push('# TYPE backendrelay_canary_started_timestamp_seconds gauge');
  lines.push(
    `backendrelay_canary_started_timestamp_seconds ${snapshot.started}`,
  );
  return `${lines.join('\n')}\n`;
}
