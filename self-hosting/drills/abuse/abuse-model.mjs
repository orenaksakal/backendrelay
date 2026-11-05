// Verdicts for the P7.10 abuse drill. Kept separate from the driver so the
// percentile and rate-limit arithmetic is testable without a staging stack.

export function summary(values) {
  const sorted = [...values].filter(Number.isFinite).sort((left, right) => left - right);
  if (sorted.length === 0) {
    return { count: 0, p50Ms: null, p95Ms: null, p99Ms: null, maxMs: null };
  }
  const at = (quantile) => sorted[Math.ceil(sorted.length * quantile) - 1];
  return {
    count: sorted.length,
    p50Ms: at(0.5),
    p95Ms: at(0.95),
    p99Ms: at(0.99),
    maxMs: sorted.at(-1),
  };
}

// App B must stay near its own baseline, not near an absolute number: staging
// runs on the production host, so an idle app is fast and a fixed ceiling would
// hide a regression.
export function latencyHolds(baseline, during, { factor = 1.5, floorMs = 250 } = {}) {
  if (!Number.isFinite(baseline?.p95Ms) || baseline.p95Ms <= 0) {
    return { ok: false, reason: 'baseline p95 is missing' };
  }
  if (!Number.isFinite(during?.p95Ms)) {
    return { ok: false, reason: 'hammered p95 is missing' };
  }
  const ceiling = Math.max(baseline.p95Ms * factor, floorMs);
  const ok = during.p95Ms <= ceiling;
  return {
    ok,
    ceilingMs: ceiling,
    reason: ok
      ? `p95 ${during.p95Ms.toFixed(1)} ms is within ${ceiling.toFixed(1)} ms`
      : `p95 ${during.p95Ms.toFixed(1)} ms exceeds ${ceiling.toFixed(1)} ms`,
  };
}

// A limit holds when the server accepted no more than the configured capacity
// and then refused with the rate-limited error. Fewer accepted requests still
// pass: a process-wide bucket (live sessions per address) may already hold
// sessions from the same NAT address before the drill starts.
export function limitHolds({ accepted, limit, refusal }) {
  if (!Number.isInteger(accepted) || accepted < 1) {
    return { ok: false, reason: 'no request was accepted' };
  }
  if (!Number.isInteger(limit) || limit < 1) {
    return { ok: false, reason: 'expected limit is not a positive integer' };
  }
  if (accepted > limit) {
    return { ok: false, reason: `${accepted} accepted, over the ${limit} limit` };
  }
  if (refusal?.status !== 429 || refusal?.type !== 'rate-limited') {
    return { ok: false, reason: 'no rate-limited refusal after the accepted requests' };
  }
  return {
    ok: true,
    fresh: accepted === limit,
    reason:
      accepted === limit
        ? `limit reached at exactly ${limit}`
        : `${accepted} of ${limit} accepted; the bucket may be partly used or the cap may be lower`,
  };
}

// One run is a table row in the runbook; the schema stays stable so past
// results remain comparable.
export function describeRun(result) {
  const checks = result.checks ?? [];
  const failed = checks.filter((check) => !check.ok);
  return {
    result: failed.length === 0 ? 'passed' : 'failed',
    total: checks.length,
    failed: failed.map((check) => check.name),
  };
}
