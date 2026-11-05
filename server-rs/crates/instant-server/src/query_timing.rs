//! Parentless, fixed-field native-refresh query diagnostics, never a gate input.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const PER_WINDOW: usize = 8;
const THRESHOLD: Duration = Duration::from_millis(750);

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

#[derive(Clone, Copy)]
pub(crate) enum QueryStage {
    Status,
    AuthorizationCheckout,
    AuthorizationRead,
    Preparation,
    DataCheckout,
    SnapshotBegin,
    PermissionPlan,
    QuerySettings,
    QuerySql,
    PermissionFilter,
    ResultFinish,
}

const STAGES: usize = QueryStage::ResultFinish as usize + 1;

pub(crate) struct QueryTiming {
    started: Option<Instant>,
    stages: [AtomicU64; STAGES],
}

impl QueryTiming {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            started: enabled.then(Instant::now),
            stages: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub(crate) async fn measure<T>(&self, stage: QueryStage, work: impl Future<Output = T>) -> T {
        let started = self.started.map(|_| Instant::now());
        let result = work.await;
        if let Some(started) = started {
            self.record(stage, started.elapsed());
        }
        result
    }

    pub(crate) fn record(&self, stage: QueryStage, duration: Duration) {
        if self.started.is_some() {
            self.stages[stage as usize].store(
                u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    fn millis(&self, stage: QueryStage) -> f64 {
        self.stages[stage as usize].load(Ordering::Relaxed) as f64 / 1_000_000.0
    }

    pub(crate) fn finish(&self, retained_authorization_lease: bool) {
        let Some(started) = self.started else {
            return;
        };
        let total = started.elapsed();
        if total < THRESHOLD
            || !BUDGET
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .allow(Instant::now())
        {
            return;
        }
        // No query text, IDs, credentials, decisions, errors or inherited spans.
        // Authorization and preparation can overlap, so stages are not additive.
        tracing::info!(target: "instant_server::safe_query_timing", parent: None,
            status_ms = self.millis(QueryStage::Status),
            authorization_checkout_ms = self.millis(QueryStage::AuthorizationCheckout),
            authorization_read_ms = self.millis(QueryStage::AuthorizationRead),
            preparation_ms = self.millis(QueryStage::Preparation),
            data_checkout_ms = self.millis(QueryStage::DataCheckout),
            snapshot_begin_ms = self.millis(QueryStage::SnapshotBegin),
            permission_plan_ms = self.millis(QueryStage::PermissionPlan),
            query_settings_ms = self.millis(QueryStage::QuerySettings),
            query_sql_ms = self.millis(QueryStage::QuerySql),
            permission_filter_ms = self.millis(QueryStage::PermissionFilter),
            result_finish_ms = self.millis(QueryStage::ResultFinish),
            total_ms = total.as_secs_f64() * 1000.0,
            retained_authorization_lease,
            "slow native refresh query stages");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_budget_is_bounded_without_accumulating_credit() {
        let now = Instant::now();
        let mut budget = Budget {
            started: now,
            remaining: PER_WINDOW,
        };
        for _ in 0..PER_WINDOW {
            assert!(budget.allow(now));
        }
        assert!(!budget.allow(now + WINDOW - Duration::from_nanos(1)));
        assert!(budget.allow(now + WINDOW));
        assert!(budget.allow(now + WINDOW * 10));
        assert_eq!(budget.remaining, PER_WINDOW - 1);
    }

    #[tokio::test]
    async fn measurement_preserves_results_and_disabled_stages_remain_empty() {
        for enabled in [false, true] {
            let timing = QueryTiming::new(enabled);
            let value = timing.measure(QueryStage::QuerySql, async { 42 }).await;
            assert_eq!(value, 42);
            timing.record(QueryStage::AuthorizationRead, Duration::from_millis(23));
            assert_eq!(
                timing.millis(QueryStage::AuthorizationRead),
                if enabled { 23.0 } else { 0.0 }
            );
            assert_eq!(timing.millis(QueryStage::DataCheckout), 0.0);
        }
    }
}
