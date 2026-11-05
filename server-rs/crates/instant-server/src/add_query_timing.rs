//! Bounded, parentless add-query/init tail stages, including handler cancellation.
//! No IDs, query text, credentials, error strings or permission decisions.
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const PER_WINDOW: usize = 8;
const THRESHOLD: Duration = Duration::from_secs(2);

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
pub(crate) enum AddQueryStage {
    Execution,
    Topics,
    Predicate,
    InitCatalog,
    InitSource,
    InitIdentity,
    InitStatus,
}

const STAGES: usize = AddQueryStage::InitStatus as usize + 1;

pub(crate) struct AddQueryTiming {
    started: Instant,
    stages: [Duration; STAGES],
    interrupted: bool,
    attempts: u64,
    security_retries: u64,
    completed: bool,
    init: bool,
}

// This timer stays in its own future. Only one sequential stage runs at a time.
struct StageGuard<'a> {
    timer: &'a mut AddQueryTiming,
    stage: AddQueryStage,
    started: Instant,
    finished: bool,
}

impl Drop for StageGuard<'_> {
    fn drop(&mut self) {
        let value = &mut self.timer.stages[self.stage as usize];
        *value = value.saturating_add(self.started.elapsed());
        if !self.finished {
            self.timer.interrupted = true;
        }
    }
}

impl AddQueryTiming {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            stages: [Duration::ZERO; STAGES],
            interrupted: false,
            attempts: 0,
            security_retries: 0,
            completed: false,
            init: false,
        }
    }

    pub(crate) fn new_init() -> Self {
        let mut timing = Self::new();
        timing.init = true;
        timing
    }

    pub(crate) fn attempt(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    pub(crate) fn retry_security(&mut self) {
        self.security_retries = self.security_retries.saturating_add(1);
    }

    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }

    pub(crate) async fn measure<T>(
        &mut self,
        stage: AddQueryStage,
        work: impl Future<Output = T>,
    ) -> T {
        let mut guard = StageGuard {
            timer: self,
            stage,
            started: Instant::now(),
            finished: false,
        };
        let result = work.await;
        guard.finished = true;
        result
    }

    fn millis(&self, stage: AddQueryStage) -> f64 {
        self.stages[stage as usize].as_secs_f64() * 1000.0
    }
}

impl Drop for AddQueryTiming {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed < THRESHOLD
            || !BUDGET
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .allow(Instant::now())
        {
            return;
        }
        if self.init {
            tracing::info!(target: "instant_server::safe_init_timing", parent: None,
                catalog_ms = self.millis(AddQueryStage::InitCatalog),
                source_ms = self.millis(AddQueryStage::InitSource),
                identity_ms = self.millis(AddQueryStage::InitIdentity),
                status_ms = self.millis(AddQueryStage::InitStatus),
                total_ms = elapsed.as_secs_f64() * 1000.0,
                completed = self.completed,
                interrupted = self.interrupted,
                "slow realtime init stages");
            return;
        }
        tracing::info!(target: "instant_server::safe_add_query_timing", parent: None,
            execution_ms = self.millis(AddQueryStage::Execution),
            topics_ms = self.millis(AddQueryStage::Topics),
            predicate_ms = self.millis(AddQueryStage::Predicate),
            total_ms = elapsed.as_secs_f64() * 1000.0,
            attempts = self.attempts,
            security_retries = self.security_retries,
            completed = self.completed,
            interrupted = self.interrupted,
            "slow realtime add-query stages");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stage_drop_records_cancelled_time_without_changing_future_output() {
        let mut timing = AddQueryTiming::new();
        let value = timing.measure(AddQueryStage::Topics, async { 42 }).await;
        assert_eq!(value, 42);
        assert!(!timing.interrupted);
        let mut pending =
            Box::pin(timing.measure(AddQueryStage::Execution, std::future::pending::<()>()));
        // Poll once, then cancel it as the receive deadline does. No sleep.
        std::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(pending);
        assert!(timing.interrupted);
        assert!(!timing.completed);
        timing.attempt();
        timing.retry_security();
        assert_eq!(timing.attempts, 1);
        assert_eq!(timing.security_retries, 1);
    }

    #[tokio::test]
    async fn init_stages_keep_output_and_completion_without_ids_or_decisions() {
        let mut timing = AddQueryTiming::new_init();
        assert!(timing.init);
        assert_eq!(
            timing
                .measure(AddQueryStage::InitIdentity, async { 42 })
                .await,
            42
        );
        timing.complete();
        assert!(timing.completed);
        assert!(!timing.interrupted);
    }

    #[test]
    fn observations_are_bounded_without_carrying_unused_credit() {
        let now = Instant::now();
        let mut budget = Budget {
            started: now,
            remaining: PER_WINDOW,
        };
        for _ in 0..PER_WINDOW {
            assert!(budget.allow(now));
        }
        assert!(!budget.allow(now));
        assert!(budget.allow(now + WINDOW));
        assert_eq!(budget.remaining, PER_WINDOW - 1);
    }
}
