//! Small, parentless, fixed-field tail diagnostic. Never a workload/gate input.
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const PER_WINDOW: usize = 8;

struct Budget {
    started: Instant,
    remaining: usize,
}

impl Budget {
    fn allow(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.started) >= WINDOW {
            self.started = now;
            self.remaining = PER_WINDOW;
        }
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

static BUDGET: LazyLock<Mutex<Budget>> = LazyLock::new(|| {
    Mutex::new(Budget {
        started: Instant::now(),
        remaining: PER_WINDOW,
    })
});

pub(crate) struct RefreshTiming {
    pub(crate) throttle_ms: f64,
    pub(crate) catalog_ms: f64,
    pub(crate) status_ms: f64,
    pub(crate) source_batch_ms: f64,
    pub(crate) query_and_result_ms: f64,
    pub(crate) total_ms: f64,
    pub(crate) stale_query_count: usize,
    pub(crate) external_catalog: bool,
}

pub(crate) fn observe(timing: RefreshTiming) {
    if timing.total_ms < 2000.0
        || [
            timing.throttle_ms,
            timing.catalog_ms,
            timing.status_ms,
            timing.source_batch_ms,
            timing.query_and_result_ms,
            timing.total_ms,
        ]
        .iter()
        .any(|n| !n.is_finite() || *n < 0.0)
    {
        return;
    }
    let allowed = BUDGET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .allow(Instant::now());
    if !allowed {
        return;
    }
    // No inherited tracing spans: those may contain app/query identifiers.
    tracing::info!(target: "instant_server::safe_refresh_timing", parent: None,
        throttle_ms=timing.throttle_ms, catalog_ms=timing.catalog_ms,
        status_ms=timing.status_ms, source_batch_ms=timing.source_batch_ms,
        query_and_result_ms=timing.query_and_result_ms, total_ms=timing.total_ms,
        stale_query_count=timing.stale_query_count, external_catalog=timing.external_catalog,
        "slow realtime refresh stages");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_refresh_budget_is_bounded_and_resets_without_accumulating_credit() {
        let now = Instant::now();
        let mut budget = Budget {
            started: now,
            remaining: PER_WINDOW,
        };
        for _ in 0..PER_WINDOW {
            assert!(budget.allow(now));
        }
        assert!(!budget.allow(now));
        assert!(!budget.allow(now + WINDOW - Duration::from_nanos(1)));
        assert!(budget.allow(now + WINDOW));
        for _ in 1..PER_WINDOW {
            assert!(budget.allow(now + WINDOW));
        }
        assert!(!budget.allow(now + WINDOW));
        assert!(budget.allow(now + WINDOW * 10));
        assert_eq!(budget.remaining, PER_WINDOW - 1);
    }
}
