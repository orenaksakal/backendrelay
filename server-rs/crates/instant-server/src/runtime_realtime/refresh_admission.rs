//! Bound expensive refresh work before it reaches the database pools.
//! FIFO within an app, round-robin between apps; cancellation removes tickets
//! without spawning admission tasks or retaining empty app buckets.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use uuid::Uuid;

/// Default cap on one app's queued refresh waiters.
pub(super) const DEFAULT_WAITERS_PER_APP: NonZeroUsize =
    NonZeroUsize::new(64).expect("positive default");

fn waiters_per_app_from_env() -> NonZeroUsize {
    std::env::var("INSTANT_REFRESH_WAITERS_PER_APP")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .and_then(NonZeroUsize::new)
        .unwrap_or(DEFAULT_WAITERS_PER_APP)
}

pub(super) struct RefreshAdmission {
    state: Mutex<State>,
    max_active: usize,
    max_waiting: usize,
    max_waiting_per_app: usize,
}

#[derive(Default)]
struct State {
    next_id: u128,
    next_turn: u128,
    active: BTreeSet<u128>,
    apps: HashMap<Uuid, AppQueue>,
    ready: BTreeMap<u128, Uuid>,
    waiting: usize,
    rejected: u64,
}

struct AppQueue {
    turn: u128,
    waiters: BTreeMap<u128, (Instant, oneshot::Sender<()>)>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct AdmissionMetrics {
    pub active: usize,
    pub waiting: usize,
    pub waiting_apps: usize,
    pub rejected: u64,
}

// The same guard owns a pending ticket and its granted slot. A grant racing
// with cancellation cannot leak a slot, including before the receiver is polled.
pub(super) struct RefreshPermit {
    admission: Arc<RefreshAdmission>,
    app: Uuid,
    id: u128,
}

impl RefreshAdmission {
    pub(super) fn new(max_active: NonZeroUsize, max_waiting: NonZeroUsize) -> Self {
        Self::with_per_app_waiters(max_active, max_waiting, waiters_per_app_from_env())
    }

    fn with_per_app_waiters(
        max_active: NonZeroUsize,
        max_waiting: NonZeroUsize,
        max_waiting_per_app: NonZeroUsize,
    ) -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_active: max_active.get(),
            max_waiting: max_waiting.get(),
            max_waiting_per_app: max_waiting_per_app.get(),
        }
    }

    pub(super) async fn acquire(self: &Arc<Self>, app: Uuid) -> Result<RefreshPermit, ()> {
        let (permit, receiver, wake) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.active.len() >= self.max_active && state.waiting >= self.max_waiting {
                state.rejected = state.rejected.saturating_add(1);
                return Err(());
            }
            // One app's refresh burst must not fill every waiter slot. Excess
            // waiters fail fast instead of extending the queue for other apps.
            if state
                .apps
                .get(&app)
                .is_some_and(|queue| queue.waiters.len() >= self.max_waiting_per_app)
            {
                state.rejected = state.rejected.saturating_add(1);
                return Err(());
            }
            let id = state.next_id;
            state.next_id += 1;
            let permit = RefreshPermit {
                admission: Arc::clone(self),
                app,
                id,
            };
            if state.active.len() < self.max_active && state.ready.is_empty() {
                state.active.insert(id);
                (permit, None, Vec::new())
            } else {
                let (sender, receiver) = oneshot::channel();
                if !state.apps.contains_key(&app) {
                    let turn = state.next_turn;
                    state.next_turn += 1;
                    state.ready.insert(turn, app);
                    state.apps.insert(
                        app,
                        AppQueue {
                            turn,
                            waiters: BTreeMap::new(),
                        },
                    );
                }
                state
                    .apps
                    .get_mut(&app)
                    .expect("registered app queue")
                    .waiters
                    .insert(id, (Instant::now(), sender));
                state.waiting += 1;
                let wake = state.dispatch(self.max_active);
                (permit, Some(receiver), wake)
            }
        };
        wake_waiters(wake);
        if let Some(receiver) = receiver {
            receiver.await.map_err(|_| ())?;
        }
        Ok(permit)
    }

    pub(super) fn metrics(&self) -> AdmissionMetrics {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        AdmissionMetrics {
            active: state.active.len(),
            waiting: state.waiting,
            waiting_apps: state.apps.len(),
            rejected: state.rejected,
        }
    }

    /// Age of the oldest queued refresh waiter for one app, if any.
    pub(super) fn oldest_wait(&self, app: Uuid) -> Option<Duration> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .apps
            .get(&app)
            .and_then(|queue| queue.waiters.values().next())
            .map(|(queued, _)| queued.elapsed())
    }
}

impl State {
    fn dispatch(&mut self, max_active: usize) -> Vec<oneshot::Sender<()>> {
        let mut wake = Vec::new();
        while self.active.len() < max_active {
            let Some((_, app)) = self.ready.pop_first() else {
                break;
            };
            let queue = self.apps.get_mut(&app).expect("ready app queue exists");
            let (id, (_, sender)) = queue.waiters.pop_first().expect("ready app has waiters");
            if queue.waiters.is_empty() {
                self.apps.remove(&app);
            } else {
                queue.turn = self.next_turn;
                self.next_turn += 1;
                self.ready.insert(queue.turn, app);
            }
            self.waiting -= 1;
            self.active.insert(id);
            wake.push(sender);
        }
        wake
    }
}

impl Drop for RefreshPermit {
    fn drop(&mut self) {
        let (wake, cancelled) = {
            let mut state = self
                .admission
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut cancelled = None;
            if !state.active.remove(&self.id) {
                let mut empty_turn = None;
                if let Some(queue) = state.apps.get_mut(&self.app) {
                    cancelled = queue.waiters.remove(&self.id);
                    if queue.waiters.is_empty() {
                        empty_turn = Some(queue.turn);
                    }
                    if cancelled.is_some() {
                        state.waiting -= 1;
                    }
                }
                if let Some(turn) = empty_turn {
                    state.apps.remove(&self.app);
                    state.ready.remove(&turn);
                }
            }
            (state.dispatch(self.admission.max_active), cancelled)
        };
        // Never run wakers under the admission lock. A closed receiver has
        // already dropped its guard, which also removed any reserved grant.
        drop(cancelled);
        wake_waiters(wake);
    }
}

fn wake_waiters(wake: Vec<oneshot::Sender<()>>) {
    for sender in wake {
        let _ = sender.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;

    fn scheduler(active: usize, pending: usize) -> Arc<RefreshAdmission> {
        Arc::new(RefreshAdmission::new(
            NonZeroUsize::new(active).unwrap(),
            NonZeroUsize::new(pending).unwrap(),
        ))
    }

    fn scheduler_with_app_cap(
        active: usize,
        pending: usize,
        pending_per_app: usize,
    ) -> Arc<RefreshAdmission> {
        Arc::new(RefreshAdmission::with_per_app_waiters(
            NonZeroUsize::new(active).unwrap(),
            NonZeroUsize::new(pending).unwrap(),
            NonZeroUsize::new(pending_per_app).unwrap(),
        ))
    }

    async fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        let mut future = future;
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
    }

    #[tokio::test]
    async fn round_robin_apps_preserve_fifo_without_hot_app_monopolization() {
        let scheduler = scheduler(1, 10);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let first = scheduler.acquire(a).await.unwrap();
        let mut a1 = Box::pin(scheduler.acquire(a));
        let mut a2 = Box::pin(scheduler.acquire(a));
        let mut b1 = Box::pin(scheduler.acquire(b));
        for pending in [&mut a1, &mut a2, &mut b1] {
            assert!(poll_once(pending.as_mut()).await.is_pending());
        }
        drop(first);
        let a1 = a1.await.unwrap();
        assert!(poll_once(b1.as_mut()).await.is_pending());
        drop(a1);
        let b1 = b1.await.unwrap();
        assert!(poll_once(a2.as_mut()).await.is_pending());
        drop(b1);
        drop(a2.await.unwrap());
        assert_eq!(scheduler.metrics(), AdmissionMetrics::default());
    }

    #[tokio::test]
    async fn cancelled_queued_and_unobserved_granted_tickets_release_every_slot() {
        let scheduler = scheduler(1, 2);
        let app = Uuid::new_v4();
        let active = scheduler.acquire(app).await.unwrap();
        let mut cancelled = Box::pin(scheduler.acquire(app));
        let mut granted = Box::pin(scheduler.acquire(Uuid::new_v4()));
        assert!(poll_once(cancelled.as_mut()).await.is_pending());
        assert!(poll_once(granted.as_mut()).await.is_pending());
        assert!(scheduler.acquire(app).await.is_err());
        drop(cancelled);
        assert_eq!(scheduler.metrics().waiting, 1);
        drop(active);
        assert_eq!(scheduler.metrics().active, 1);
        drop(granted);
        assert_eq!(
            scheduler.metrics(),
            AdmissionMetrics {
                rejected: 1,
                ..AdmissionMetrics::default()
            }
        );
        drop(scheduler.acquire(app).await.unwrap());
        assert_eq!(scheduler.metrics().active, 0);
    }

    #[tokio::test]
    async fn one_app_cannot_hold_every_refresh_waiter_slot() {
        let scheduler = scheduler_with_app_cap(1, 10, 2);
        let hot = Uuid::new_v4();
        let other = Uuid::new_v4();
        let active = scheduler.acquire(hot).await.unwrap();
        let mut queued = Vec::new();
        for _ in 0..2 {
            let mut ticket = Box::pin(scheduler.acquire(hot));
            assert!(poll_once(ticket.as_mut()).await.is_pending());
            queued.push(ticket);
        }
        assert_eq!(scheduler.metrics().waiting, 2);
        assert!(
            scheduler.acquire(hot).await.is_err(),
            "a third hot-app waiter must fail with the admission error"
        );
        let mut other_ticket = Box::pin(scheduler.acquire(other));
        assert!(poll_once(other_ticket.as_mut()).await.is_pending());
        assert_eq!(
            scheduler.metrics(),
            AdmissionMetrics {
                active: 1,
                waiting: 3,
                waiting_apps: 2,
                rejected: 1,
            }
        );
        drop((queued, other_ticket, active));
        assert_eq!(
            scheduler.metrics(),
            AdmissionMetrics {
                rejected: 1,
                ..AdmissionMetrics::default()
            }
        );
    }

    #[tokio::test]
    async fn oldest_waiter_age_is_reported_per_app() {
        let scheduler = scheduler(1, 2);
        let app = Uuid::new_v4();
        let other = Uuid::new_v4();
        let active = scheduler.acquire(app).await.unwrap();
        let mut queued = Box::pin(scheduler.acquire(app));
        assert!(poll_once(queued.as_mut()).await.is_pending());
        assert!(scheduler.oldest_wait(app).is_some());
        assert!(scheduler.oldest_wait(other).is_none());
        drop(active);
        drop(queued.await.unwrap());
        assert!(scheduler.oldest_wait(app).is_none());
    }

    #[tokio::test]
    async fn ten_thousand_pending_sessions_cancel_without_retained_app_buckets() {
        let scheduler = scheduler(1, 10_000);
        let active = scheduler.acquire(Uuid::nil()).await.unwrap();
        let mut pending = Vec::new();
        for id in 1..=10_000 {
            let mut ticket = Box::pin(scheduler.acquire(Uuid::from_u128(id)));
            assert!(poll_once(ticket.as_mut()).await.is_pending());
            pending.push(ticket);
        }
        assert_eq!(scheduler.metrics().waiting, 10_000);
        drop(pending);
        assert_eq!(scheduler.metrics().waiting_apps, 0);
        drop(active);
        assert_eq!(scheduler.metrics(), AdmissionMetrics::default());
        let state = scheduler.state.lock().unwrap();
        assert!(state.ready.is_empty() && state.apps.is_empty());
    }

    #[tokio::test]
    async fn concurrent_refreshes_never_exceed_the_active_budget() {
        let scheduler = scheduler(3, 100);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let mut work = tokio::task::JoinSet::new();
        for index in 0..32 {
            let scheduler = Arc::clone(&scheduler);
            let release = Arc::clone(&release);
            let started = started.clone();
            work.spawn(async move {
                let _permit = scheduler.acquire(Uuid::from_u128(index % 4)).await.unwrap();
                assert!(scheduler.metrics().active <= 3);
                started.send(()).unwrap();
                release.acquire().await.unwrap().forget();
            });
        }
        for _ in 0..3 {
            starts.recv().await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while scheduler.metrics().waiting != 29 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(scheduler.metrics().active, 3);
        release.add_permits(32);
        while let Some(result) = work.join_next().await {
            result.unwrap();
        }
        assert_eq!(scheduler.metrics(), AdmissionMetrics::default());
    }
}
