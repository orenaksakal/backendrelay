use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use instant_http::ApiError;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_postgres::{CancelToken, Client};

const DEFAULT_STATEMENT_CANCEL_WAIT: Duration = Duration::from_millis(500);
const CANCEL_REQUEST_NETWORK_TIMEOUT: Duration = Duration::from_secs(5);

pub type StatementCancelWaitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Duration, ApiError>> + Send + 'a>>;

/// Resolves the total time allowed for active statements to finish after
/// their out-of-band cancellation requests have been issued.
pub trait StatementCancelWaitPolicy: Send + Sync + 'static {
    fn wait_duration(&self) -> StatementCancelWaitFuture<'_>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticStatementCancelWait(pub Duration);

impl Default for StaticStatementCancelWait {
    fn default() -> Self {
        Self(DEFAULT_STATEMENT_CANCEL_WAIT)
    }
}

impl StatementCancelWaitPolicy for StaticStatementCancelWait {
    fn wait_duration(&self) -> StatementCancelWaitFuture<'_> {
        Box::pin(std::future::ready(Ok(self.0)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelWaitOutcome {
    pub requested: usize,
    pub completed: usize,
    pub timed_out: bool,
}

#[derive(Default)]
struct TrackerState {
    next_id: u64,
    active: HashMap<u64, TrackedStatement>,
}

struct TrackedStatement {
    cancel_token: Option<CancelToken>,
    cancellation_requested: Arc<AtomicBool>,
}

#[derive(Default)]
struct TrackerInner {
    state: Mutex<TrackerState>,
    changed: Notify,
}

/// Task-scoped registry of PostgreSQL connection leases owned by one request.
///
/// Registrations are RAII values. A registration completes only after its
/// connection lease has been dropped or discarded, which prevents a cancelling
/// connection from being returned to the pool before the request drain ends.
#[derive(Clone, Default)]
pub struct StatementTracker {
    inner: Arc<TrackerInner>,
}

tokio::task_local! {
    static CURRENT_STATEMENT_TRACKER: StatementTracker;
}

// Drop runs before fields are dropped: mark leases before dropping query
// futures, whose PostgreSQL cancellation guards can send detached cancels.
struct CancellationSafeScope<F> {
    future: Pin<Box<F>>,
    tracker: StatementTracker,
    completed: bool,
}

impl<F: Future> Future for CancellationSafeScope<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.future.as_mut().poll(cx);
        if result.is_ready() {
            this.completed = true;
        }
        result
    }
}

impl<F> Drop for CancellationSafeScope<F> {
    fn drop(&mut self) {
        if !self.completed {
            for statement in tracker_state(&self.tracker.inner).active.values() {
                statement
                    .cancellation_requested
                    .store(true, Ordering::Release);
            }
        }
    }
}

impl StatementTracker {
    /// Installs this tracker for connection leases acquired while `future` is
    /// polled. Child async work inherits it only while it remains in this task;
    /// detached/shared workers must install their own scope. Aborting a scope
    /// marks its active leases for disposal before their query futures drop.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT_STATEMENT_TRACKER
            .scope(
                self.clone(),
                CancellationSafeScope {
                    future: Box::pin(future),
                    tracker: self.clone(),
                    completed: false,
                },
            )
            .await
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        tracker_state(&self.inner).active.len()
    }

    /// Requests cancellation for the current snapshot, then waits against one
    /// shared absolute deadline for those registrations to finish.
    pub async fn cancel_and_wait(&self, timeout: Duration) -> CancelWaitOutcome {
        let deadline = Instant::now() + timeout;
        let snapshot = {
            let state = tracker_state(&self.inner);
            state
                .active
                .iter()
                .map(|(id, statement)| {
                    statement
                        .cancellation_requested
                        .store(true, Ordering::Release);
                    (*id, statement.cancel_token.clone())
                })
                .collect::<Vec<_>>()
        };
        let requested = snapshot.len();
        if requested == 0 {
            return CancelWaitOutcome {
                requested: 0,
                completed: 0,
                timed_out: false,
            };
        }
        let tracked_ids = snapshot.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        for (_, token) in snapshot {
            let Some(token) = token else { continue };
            tokio::spawn(async move {
                let _request_was_delivered =
                    instant_pg::request_query_cancel(&token, CANCEL_REQUEST_NETWORK_TIMEOUT).await;
            });
        }

        loop {
            let notified = self.inner.changed.notified();
            let remaining = {
                let state = tracker_state(&self.inner);
                tracked_ids
                    .iter()
                    .filter(|id| state.active.contains_key(id))
                    .count()
            };
            if remaining == 0 {
                return CancelWaitOutcome {
                    requested,
                    completed: requested,
                    timed_out: false,
                };
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let remaining = {
                    let state = tracker_state(&self.inner);
                    tracked_ids
                        .iter()
                        .filter(|id| state.active.contains_key(id))
                        .count()
                };
                return CancelWaitOutcome {
                    requested,
                    completed: requested.saturating_sub(remaining),
                    timed_out: remaining != 0,
                };
            }
        }
    }

    fn register(&self, cancel_token: Option<CancelToken>) -> StatementRegistration {
        let cancellation_requested = Arc::new(AtomicBool::new(false));
        let id = {
            let mut state = tracker_state(&self.inner);
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1);
            state.active.insert(
                id,
                TrackedStatement {
                    cancel_token,
                    cancellation_requested: Arc::clone(&cancellation_requested),
                },
            );
            id
        };
        StatementRegistration {
            tracker: Arc::downgrade(&self.inner),
            id,
            cancellation_requested,
        }
    }
}

pub(crate) fn register_current(client: &Client) -> Option<StatementRegistration> {
    CURRENT_STATEMENT_TRACKER
        .try_with(|tracker| tracker.register(Some(client.cancel_token())))
        .ok()
}

pub(crate) struct StatementRegistration {
    tracker: Weak<TrackerInner>,
    id: u64,
    cancellation_requested: Arc<AtomicBool>,
}

impl StatementRegistration {
    pub(crate) fn cancellation_requested(&self) -> bool {
        self.cancellation_requested.load(Ordering::Acquire)
    }
}

impl Drop for StatementRegistration {
    fn drop(&mut self) {
        let Some(tracker) = self.tracker.upgrade() else {
            return;
        };
        tracker_state(&tracker).active.remove(&self.id);
        tracker.changed.notify_waiters();
    }
}

fn tracker_state(inner: &TrackerInner) -> MutexGuard<'_, TrackerState> {
    inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scope_is_task_local_and_registration_is_raii() {
        let tracker = StatementTracker::default();
        tracker
            .scope(async {
                let registration = CURRENT_STATEMENT_TRACKER.with(|current| current.register(None));
                assert_eq!(tracker.active_count(), 1);
                drop(registration);
                assert_eq!(tracker.active_count(), 0);
            })
            .await;
        assert!(CURRENT_STATEMENT_TRACKER.try_with(|_| ()).is_err());
    }

    #[tokio::test]
    async fn cancel_wait_uses_one_deadline_and_counts_only_its_snapshot() {
        let tracker = StatementTracker::default();
        let first = tracker.register(None);
        let second = tracker.register(None);
        let later_tracker = tracker.clone();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(first);
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(second);
            later_tracker.register(None)
        });
        let outcome = tracker.cancel_and_wait(Duration::from_secs(1)).await;
        assert_eq!(
            outcome,
            CancelWaitOutcome {
                requested: 2,
                completed: 2,
                timed_out: false,
            }
        );
        let later = release.await.unwrap();
        assert!(!later.cancellation_requested());
        assert_eq!(tracker.active_count(), 1);
        drop(later);
    }

    #[tokio::test]
    async fn zero_wait_reports_the_held_snapshot_without_waiting() {
        let tracker = StatementTracker::default();
        let registration = tracker.register(None);
        assert!(!registration.cancellation_requested());
        let outcome = tracker.cancel_and_wait(Duration::ZERO).await;
        assert_eq!(
            outcome,
            CancelWaitOutcome {
                requested: 1,
                completed: 0,
                timed_out: true,
            }
        );
        assert!(registration.cancellation_requested());
        drop(registration);
        assert_eq!(tracker.active_count(), 0);
    }

    #[tokio::test]
    async fn completed_registrations_make_repeated_cancellation_a_noop() {
        let tracker = StatementTracker::default();
        let registration = tracker.register(None);
        drop(registration);
        assert_eq!(
            tracker.cancel_and_wait(Duration::ZERO).await,
            CancelWaitOutcome {
                requested: 0,
                completed: 0,
                timed_out: false,
            }
        );
    }

    #[tokio::test]
    async fn static_policy_defaults_to_the_pinned_half_second() {
        let policy = StaticStatementCancelWait::default();
        assert_eq!(
            policy.wait_duration().await.unwrap(),
            Duration::from_millis(500)
        );
    }
}
