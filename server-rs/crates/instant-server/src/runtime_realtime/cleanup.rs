//! Bounded cluster cleanup with retained, deduplicated retries.
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::FutureExt as _;
use instant_realtime::RoomKey;
use tokio::sync::Notify;
use uuid::Uuid;

use super::{CLUSTER_SUBSCRIBE_RETRY_MAX, CLUSTER_SUBSCRIBE_RETRY_MIN, RuntimeRealtimeCluster};

pub(super) const DEFAULT_CLUSTER_CLEANUP_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(8).unwrap();
// A cluster RPC must not permanently occupy one of the bounded cleanup slots.
const CLUSTER_CLEANUP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeCleanupMetrics {
    pub pending: usize,
    pub active: usize,
    pub oldest_retry_age: Option<Duration>,
    pub failures: u64,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ClusterCleanupKey {
    SseSession(Uuid),
    RoomSession { room: RoomKey, session_id: Uuid },
}

struct PendingClusterCleanup {
    first_pending_at: Instant,
    next_attempt_at: tokio::time::Instant,
    retry_delay: Duration,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CleanupDeadline {
    at: tokio::time::Instant,
    key: ClusterCleanupKey,
}

#[derive(Default)]
struct ClusterCleanupState {
    pending: HashMap<ClusterCleanupKey, PendingClusterCleanup>,
    active: BTreeSet<ClusterCleanupKey>,
    ready: VecDeque<ClusterCleanupKey>,
    retries: BinaryHeap<Reverse<CleanupDeadline>>,
    worker_running: bool,
    failures: u64,
}

pub(super) struct ClusterCleanupCoordinator {
    notify: Notify,
    state: Mutex<ClusterCleanupState>,
    pub(super) concurrency: AtomicUsize,
    attempt_timeout: Duration,
}

impl Default for ClusterCleanupCoordinator {
    fn default() -> Self {
        Self::new(DEFAULT_CLUSTER_CLEANUP_CONCURRENCY)
    }
}

impl ClusterCleanupCoordinator {
    fn new(concurrency: NonZeroUsize) -> Self {
        Self {
            notify: Notify::new(),
            state: Mutex::new(ClusterCleanupState::default()),
            concurrency: AtomicUsize::new(concurrency.get()),
            attempt_timeout: CLUSTER_CLEANUP_ATTEMPT_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn with_attempt_timeout(
        concurrency: NonZeroUsize,
        attempt_timeout: Duration,
    ) -> Self {
        Self {
            notify: Notify::new(),
            state: Mutex::new(ClusterCleanupState::default()),
            concurrency: AtomicUsize::new(concurrency.get()),
            attempt_timeout,
        }
    }

    pub(super) fn set_concurrency(&self, concurrency: NonZeroUsize) {
        self.concurrency.store(concurrency.get(), Ordering::Release);
        self.notify.notify_one();
    }
    pub(super) fn metrics_snapshot(&self) -> RuntimeCleanupMetrics {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        RuntimeCleanupMetrics {
            pending: state.pending.len(),
            active: state.active.len(),
            oldest_retry_age: state
                .pending
                .values()
                .map(|pending| pending.first_pending_at.elapsed())
                .max(),
            failures: state.failures,
        }
    }

    pub(super) fn schedule(
        self: &Arc<Self>,
        cluster: Arc<dyn RuntimeRealtimeCluster>,
        session_id: Uuid,
        rooms: impl IntoIterator<Item = RoomKey>,
    ) {
        let now = tokio::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        enqueue_cluster_cleanup(&mut state, ClusterCleanupKey::SseSession(session_id), now);
        for room in rooms {
            enqueue_cluster_cleanup(
                &mut state,
                ClusterCleanupKey::RoomSession { room, session_id },
                now,
            );
        }
        let start_worker = !state.worker_running;
        if start_worker {
            state.worker_running = true;
        }
        drop(state);
        self.notify.notify_one();
        if !start_worker {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .worker_running = false;
            tracing::warn!(%session_id, "cluster cleanup deferred without a Tokio runtime");
            return;
        };
        let coordinator = Arc::clone(self);
        runtime.spawn(async move {
            coordinator.run(cluster).await;
        });
    }

    fn next_ready(&self) -> Option<ClusterCleanupKey> {
        let now = tokio::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        promote_due_cluster_cleanups(&mut state, now);
        while let Some(key) = state.ready.pop_front() {
            if state.pending.contains_key(&key) && state.active.insert(key.clone()) {
                return Some(key);
            }
        }
        None
    }

    fn next_attempt_at(&self) -> Option<tokio::time::Instant> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        promote_due_cluster_cleanups(&mut state, tokio::time::Instant::now());
        next_cluster_cleanup_deadline(&mut state)
    }

    fn finish_if_idle(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.is_empty() && state.active.is_empty() {
            state.worker_running = false;
            true
        } else {
            false
        }
    }

    fn complete(&self, key: &ClusterCleanupKey, succeeded: bool) {
        let now = tokio::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active.remove(key);
        if succeeded {
            state.pending.remove(key);
        } else {
            state.failures = state.failures.saturating_add(1);
            let retry_at = if let Some(pending) = state.pending.get_mut(key) {
                pending.next_attempt_at = now + pending.retry_delay;
                pending.retry_delay = pending
                    .retry_delay
                    .saturating_mul(2)
                    .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
                Some(pending.next_attempt_at)
            } else {
                None
            };
            if let Some(at) = retry_at {
                state.retries.push(Reverse(CleanupDeadline {
                    at,
                    key: key.clone(),
                }));
            }
        }
        drop(state);
        self.notify.notify_one();
    }

    async fn run(self: Arc<Self>, cluster: Arc<dyn RuntimeRealtimeCluster>) {
        let mut attempts = tokio::task::JoinSet::new();
        loop {
            while attempts.len() < self.concurrency.load(Ordering::Acquire) {
                let Some(key) = self.next_ready() else {
                    break;
                };
                let cluster = Arc::clone(&cluster);
                let attempt_timeout = self.attempt_timeout;
                attempts.spawn(async move {
                    let result = tokio::time::timeout(
                        attempt_timeout,
                        AssertUnwindSafe(async {
                            match &key {
                                ClusterCleanupKey::SseSession(session_id) => {
                                    cluster.unregister_sse_session(*session_id).await
                                }
                                ClusterCleanupKey::RoomSession { room, session_id } => {
                                    cluster.leave(room, *session_id).await.map(|_| ())
                                }
                            }
                        })
                        .catch_unwind(),
                    )
                    .await;
                    (
                        key,
                        result.is_ok_and(|result| result.is_ok_and(|result| result.is_ok())),
                    )
                });
            }

            if attempts.is_empty() && self.finish_if_idle() {
                return;
            }

            let wake_at = self
                .next_attempt_at()
                .unwrap_or_else(|| tokio::time::Instant::now() + CLUSTER_SUBSCRIBE_RETRY_MAX);
            tokio::select! {
                result = attempts.join_next(), if !attempts.is_empty() => match result {
                    Some(Ok((key, succeeded))) => self.complete(&key, succeeded),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "cluster cleanup attempt terminated unexpectedly");
                        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.active.clear();
                        state.failures = state.failures.saturating_add(1);
                    }
                    None => {}
                },
                () = self.notify.notified() => {},
                () = tokio::time::sleep_until(wake_at) => {},
            }
        }
    }
}

fn enqueue_cluster_cleanup(
    state: &mut ClusterCleanupState,
    key: ClusterCleanupKey,
    now: tokio::time::Instant,
) {
    if state.pending.contains_key(&key) {
        return;
    }
    state.pending.insert(
        key.clone(),
        PendingClusterCleanup {
            first_pending_at: Instant::now(),
            next_attempt_at: now,
            retry_delay: CLUSTER_SUBSCRIBE_RETRY_MIN,
        },
    );
    state.ready.push_back(key);
}

fn promote_due_cluster_cleanups(state: &mut ClusterCleanupState, now: tokio::time::Instant) {
    while let Some(Reverse(deadline)) = state.retries.peek() {
        if deadline.at > now {
            break;
        }
        let Some(Reverse(deadline)) = state.retries.pop() else {
            break;
        };
        if state
            .pending
            .get(&deadline.key)
            .is_some_and(|pending| pending.next_attempt_at == deadline.at)
        {
            state.ready.push_back(deadline.key);
        }
    }
}

fn next_cluster_cleanup_deadline(state: &mut ClusterCleanupState) -> Option<tokio::time::Instant> {
    while let Some(Reverse(deadline)) = state.retries.peek() {
        if state
            .pending
            .get(&deadline.key)
            .is_some_and(|pending| pending.next_attempt_at == deadline.at)
        {
            return Some(deadline.at);
        }
        let _stale_deadline = state.retries.pop();
    }
    None
}
