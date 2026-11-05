use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::FutureExt as _;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

pub type GroupedProcessFuture<R, E> = Pin<Box<dyn Future<Output = Result<R, E>> + Send + 'static>>;

type CombineFn<K, V> = dyn Fn(&K, &V, &V) -> Option<V> + Send + Sync;
type ProcessFn<K, V, R, E> = dyn Fn(K, V) -> GroupedProcessFuture<R, E> + Send + Sync;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveExecutorClass {
    Standard,
    Expanded,
}

impl ReceiveExecutorClass {
    const fn encoded(self) -> u8 {
        match self {
            Self::Standard => 0,
            Self::Expanded => 1,
        }
    }

    fn decode(value: u8) -> Self {
        if value == Self::Expanded.encoded() {
            Self::Expanded
        } else {
            Self::Standard
        }
    }

    #[must_use]
    pub const fn multiplier(self) -> usize {
        match self {
            Self::Standard => 20,
            Self::Expanded => 100,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Expanded => "expanded",
        }
    }

    #[must_use]
    pub fn worker_limit(self, logical_cpus: usize) -> usize {
        self.multiplier().saturating_mul(logical_cpus.max(1)).max(1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupedReceiveDispatchConfig {
    pub capacity: usize,
    pub logical_cpus: usize,
    pub executor_class: ReceiveExecutorClass,
}

impl GroupedReceiveDispatchConfig {
    #[must_use]
    pub fn worker_limit(self) -> usize {
        self.executor_class.worker_limit(self.logical_cpus)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupedReceiveMetrics {
    pub queued: usize,
    pub active_groups: usize,
    pub worker_count: usize,
    pub puts: u64,
    pub longest_wait: Option<Duration>,
    pub executor_class: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchCompletionError {
    Shutdown,
    Panicked,
}

enum DispatchOutcome<R, E> {
    Completed(Arc<Result<R, E>>),
    Shutdown,
    Panicked,
}

pub struct DispatchCompletion<R, E> {
    receiver: oneshot::Receiver<DispatchOutcome<R, E>>,
}

impl<R, E> DispatchCompletion<R, E> {
    /// Waits for the processed result shared by every input in a combined item.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchCompletionError::Shutdown`] when shutdown releases the
    /// input before it becomes active, or [`DispatchCompletionError::Panicked`]
    /// when the group's processor panicked.
    pub async fn wait(self) -> Result<Arc<Result<R, E>>, DispatchCompletionError> {
        match self.receiver.await {
            Ok(DispatchOutcome::Completed(result)) => Ok(result),
            Ok(DispatchOutcome::Panicked) => Err(DispatchCompletionError::Panicked),
            Ok(DispatchOutcome::Shutdown) | Err(_) => Err(DispatchCompletionError::Shutdown),
        }
    }
}

#[derive(Debug)]
pub enum TryEnqueueError<V> {
    Full(V),
    Shutdown(V),
}

struct Queued<V, R, E> {
    value: V,
    enqueued_at: Instant,
    completions: Vec<oneshot::Sender<DispatchOutcome<R, E>>>,
    capacity_permits: Vec<OwnedSemaphorePermit>,
}

struct Group<V, R, E> {
    queue: VecDeque<Queued<V, R, E>>,
}

struct DispatchState<K, V, R, E> {
    accepting: bool,
    groups: HashMap<K, Group<V, R, E>>,
    closed_groups: HashSet<K>,
    queued: usize,
    active_groups: usize,
    worker_count: usize,
    puts: u64,
}

struct DispatchInner<K, V, R, E> {
    state: Mutex<DispatchState<K, V, R, E>>,
    capacity: Arc<Semaphore>,
    standard_workers: Arc<Semaphore>,
    expanded_workers: Arc<Semaphore>,
    worker_shutdown: Notify,
    changed: Notify,
    executor_class: AtomicU8,
    combine: Arc<CombineFn<K, V>>,
    process: Arc<ProcessFn<K, V, R, E>>,
}

pub struct GroupedReceiveDispatcher<K, V, R, E> {
    inner: Arc<DispatchInner<K, V, R, E>>,
}

impl<K, V, R, E> Clone for GroupedReceiveDispatcher<K, V, R, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V, R, E> GroupedReceiveDispatcher<K, V, R, E>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + 'static,
    R: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    /// Creates a dispatcher with bounded input capacity and global worker pools.
    ///
    /// # Panics
    ///
    /// Panics when `config.capacity` is zero.
    #[must_use]
    pub fn new(
        config: GroupedReceiveDispatchConfig,
        combine: impl Fn(&K, &V, &V) -> Option<V> + Send + Sync + 'static,
        process: impl Fn(K, V) -> GroupedProcessFuture<R, E> + Send + Sync + 'static,
    ) -> Self {
        assert!(
            config.capacity > 0,
            "receive queue capacity must be positive"
        );
        Self {
            inner: Arc::new(DispatchInner {
                state: Mutex::new(DispatchState {
                    accepting: true,
                    groups: HashMap::new(),
                    closed_groups: HashSet::new(),
                    queued: 0,
                    active_groups: 0,
                    worker_count: 0,
                    puts: 0,
                }),
                capacity: Arc::new(Semaphore::new(config.capacity)),
                standard_workers: Arc::new(Semaphore::new(
                    ReceiveExecutorClass::Standard.worker_limit(config.logical_cpus),
                )),
                expanded_workers: Arc::new(Semaphore::new(
                    ReceiveExecutorClass::Expanded.worker_limit(config.logical_cpus),
                )),
                worker_shutdown: Notify::new(),
                changed: Notify::new(),
                executor_class: AtomicU8::new(config.executor_class.encoded()),
                combine: Arc::new(combine),
                process: Arc::new(process),
            }),
        }
    }

    pub fn set_executor_class(&self, executor_class: ReceiveExecutorClass) {
        self.inner
            .executor_class
            .store(executor_class.encoded(), Ordering::Release);
    }

    #[must_use]
    pub fn executor_class(&self) -> ReceiveExecutorClass {
        ReceiveExecutorClass::decode(self.inner.executor_class.load(Ordering::Acquire))
    }

    /// Enqueues an input, waiting for bounded queue capacity when necessary.
    ///
    /// # Errors
    ///
    /// Returns the original value when the dispatcher or exact group has shut
    /// down before the input can be accepted.
    pub async fn enqueue(
        &self,
        key: K,
        value: V,
    ) -> Result<DispatchCompletion<R, E>, TryEnqueueError<V>> {
        let executor_class = self.executor_class();
        let Ok(permit) = Arc::clone(&self.inner.capacity).acquire_owned().await else {
            return Err(TryEnqueueError::Shutdown(value));
        };
        self.enqueue_with_permit(key, value, permit, executor_class)
    }

    /// Attempts to enqueue an input without waiting for capacity.
    ///
    /// # Errors
    ///
    /// Returns the original value when capacity is full or shutdown prevents
    /// the input from being accepted.
    pub fn try_enqueue(
        &self,
        key: K,
        value: V,
    ) -> Result<DispatchCompletion<R, E>, TryEnqueueError<V>> {
        let executor_class = self.executor_class();
        let permit = match Arc::clone(&self.inner.capacity).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Err(TryEnqueueError::Full(value));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(TryEnqueueError::Shutdown(value));
            }
        };
        self.enqueue_with_permit(key, value, permit, executor_class)
    }

    fn enqueue_with_permit(
        &self,
        key: K,
        value: V,
        permit: OwnedSemaphorePermit,
        executor_class: ReceiveExecutorClass,
    ) -> Result<DispatchCompletion<R, E>, TryEnqueueError<V>> {
        let (sender, receiver) = oneshot::channel();
        let mut state = dispatch_state(&self.inner);
        if !state.accepting || state.closed_groups.contains(&key) {
            return Err(TryEnqueueError::Shutdown(value));
        }
        let start_worker = !state.groups.contains_key(&key);
        state
            .groups
            .entry(key.clone())
            .or_insert_with(|| Group {
                queue: VecDeque::new(),
            })
            .queue
            .push_back(Queued {
                value,
                enqueued_at: Instant::now(),
                completions: vec![sender],
                capacity_permits: vec![permit],
            });
        state.queued = state.queued.saturating_add(1);
        state.puts = state.puts.saturating_add(1);
        if start_worker {
            state.active_groups = state.active_groups.saturating_add(1);
        }
        drop(state);
        if start_worker {
            spawn_group_worker(Arc::clone(&self.inner), key, executor_class);
        }
        Ok(DispatchCompletion { receiver })
    }

    #[must_use]
    pub fn metrics_snapshot(&self) -> GroupedReceiveMetrics {
        self.snapshot(true)
    }

    /// Non-consuming pressure observation for the private benchmark listener.
    /// Unlike the existing interval gauge, this never resets the puts counter.
    #[must_use]
    pub fn pressure_snapshot(&self) -> GroupedReceiveMetrics {
        self.snapshot(false)
    }

    fn snapshot(&self, reset_puts: bool) -> GroupedReceiveMetrics {
        let now = Instant::now();
        let mut state = dispatch_state(&self.inner);
        let longest_wait = state
            .groups
            .values()
            .filter_map(|group| group.queue.front())
            .map(|item| now.saturating_duration_since(item.enqueued_at))
            .max();
        let metrics = GroupedReceiveMetrics {
            queued: state.queued,
            active_groups: state.active_groups,
            worker_count: state.worker_count,
            puts: state.puts,
            longest_wait,
            executor_class: self.executor_class().as_str(),
        };
        if reset_puts {
            state.puts = 0;
        }
        metrics
    }

    pub async fn shutdown_group(&self, key: &K) -> usize {
        let released = {
            let mut state = dispatch_state(&self.inner);
            state.closed_groups.insert(key.clone());
            let Some(group) = state.groups.get_mut(key) else {
                return 0;
            };
            let released = release_pending(&mut group.queue);
            state.queued = state.queued.saturating_sub(released);
            released
        };
        self.inner.worker_shutdown.notify_waiters();
        self.inner.changed.notify_waiters();
        wait_for_group_exit(&self.inner, key).await;
        released
    }

    pub async fn shutdown(&self) -> usize {
        let released = {
            let mut state = dispatch_state(&self.inner);
            if state.accepting {
                state.accepting = false;
                self.inner.capacity.close();
            }
            let mut released = 0;
            for group in state.groups.values_mut() {
                released += release_pending(&mut group.queue);
            }
            state.queued = state.queued.saturating_sub(released);
            released
        };
        self.inner.worker_shutdown.notify_waiters();
        self.inner.changed.notify_waiters();
        wait_for_all_groups(&self.inner).await;
        released
    }
}

fn spawn_group_worker<K, V, R, E>(
    inner: Arc<DispatchInner<K, V, R, E>>,
    key: K,
    executor_class: ReceiveExecutorClass,
) where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + 'static,
    R: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    tokio::spawn(async move {
        run_group(inner, key, executor_class).await;
    });
}

async fn run_group<K, V, R, E>(
    inner: Arc<DispatchInner<K, V, R, E>>,
    key: K,
    initial_executor_class: ReceiveExecutorClass,
) where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + 'static,
    R: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    let mut scheduled_executor_class = Some(initial_executor_class);
    loop {
        // Normal group completion must not wake every queued worker. Only a
        // shutdown can invalidate the pending group while its permit is queued.
        let shutdown_changed = inner.worker_shutdown.notified();
        if !group_has_pending_or_finish(&inner, &key) {
            return;
        }
        let executor_class = scheduled_executor_class.take().unwrap_or_else(|| {
            ReceiveExecutorClass::decode(inner.executor_class.load(Ordering::Acquire))
        });
        let worker_pool = match executor_class {
            ReceiveExecutorClass::Standard => &inner.standard_workers,
            ReceiveExecutorClass::Expanded => &inner.expanded_workers,
        };
        let worker = {
            let mut changed = shutdown_changed;
            // Preserve this registration across unrelated shutdown notices too:
            // dropping acquire_owned would cancel our FIFO position and let a
            // later group overtake us each time another group changes state.
            let acquisition = Arc::clone(worker_pool).acquire_owned();
            tokio::pin!(acquisition);
            loop {
                tokio::select! {
                    permit = &mut acquisition => {
                        break permit.expect(
                            "receive worker semaphore remains open for dispatcher lifetime",
                        );
                    }
                    () = changed => {
                        changed = inner.worker_shutdown.notified();
                        if !group_has_pending_or_finish(&inner, &key) {
                            return;
                        }
                    }
                }
            }
        };
        let _worker_count = ActiveWorkerCount::new(Arc::clone(&inner));
        let Some(item) = poll_group(&inner, &key) else {
            drop(worker);
            return;
        };
        let Queued {
            value,
            completions,
            capacity_permits,
            ..
        } = item;
        drop(capacity_permits);
        // A panic must not leave this group registered with no worker: the
        // key would never drain and its queue slots would fill the global
        // receive queue. The installed panic hook already counts it.
        let processed = AssertUnwindSafe(async { (inner.process)(key.clone(), value).await })
            .catch_unwind()
            .await;
        drop(worker);
        let Ok(result) = processed else {
            abandon_group(&inner, &key);
            for completion in completions {
                let _ignored_closed_receiver = completion.send(DispatchOutcome::Panicked);
            }
            return;
        };
        let result = Arc::new(result);
        for completion in completions {
            let _ignored_closed_receiver =
                completion.send(DispatchOutcome::Completed(Arc::clone(&result)));
        }
    }
}

/// Releases a group whose processor panicked: pending inputs complete as
/// shutdown, their capacity permits drop, and the key disappears exactly as
/// the normal empty-queue path removes it.
fn abandon_group<K, V, R, E>(inner: &Arc<DispatchInner<K, V, R, E>>, key: &K)
where
    K: Eq + Hash,
{
    let mut state = dispatch_state(inner);
    let Some(mut group) = state.groups.remove(key) else {
        return;
    };
    let released = release_pending(&mut group.queue);
    state.queued = state.queued.saturating_sub(released);
    finish_group(&mut state);
    drop(state);
    inner.changed.notify_waiters();
}

struct ActiveWorkerCount<K, V, R, E> {
    inner: Arc<DispatchInner<K, V, R, E>>,
}

impl<K, V, R, E> ActiveWorkerCount<K, V, R, E> {
    fn new(inner: Arc<DispatchInner<K, V, R, E>>) -> Self {
        inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .worker_count += 1;
        Self { inner }
    }
}

impl<K, V, R, E> Drop for ActiveWorkerCount<K, V, R, E> {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.worker_count = state.worker_count.saturating_sub(1);
    }
}

fn group_has_pending_or_finish<K, V, R, E>(inner: &Arc<DispatchInner<K, V, R, E>>, key: &K) -> bool
where
    K: Eq + Hash,
{
    let mut state = dispatch_state(inner);
    match state.groups.get(key) {
        Some(group) if !group.queue.is_empty() => true,
        Some(_) => {
            state.groups.remove(key);
            finish_group(&mut state);
            inner.changed.notify_waiters();
            false
        }
        None => false,
    }
}

fn poll_group<K, V, R, E>(
    inner: &Arc<DispatchInner<K, V, R, E>>,
    key: &K,
) -> Option<Queued<V, R, E>>
where
    K: Clone + Eq + Hash,
{
    let mut state = dispatch_state(inner);
    let group = state.groups.get_mut(key)?;
    let Some(mut item) = group.queue.pop_front() else {
        state.groups.remove(key);
        finish_group(&mut state);
        inner.changed.notify_waiters();
        return None;
    };
    let mut removed = 1;
    while let Some(next) = group.queue.front() {
        let Some(combined) = (inner.combine)(key, &item.value, &next.value) else {
            break;
        };
        let next = group.queue.pop_front().expect("peeked queue item exists");
        item.value = combined;
        item.completions.extend(next.completions);
        item.capacity_permits.extend(next.capacity_permits);
        removed += 1;
    }
    state.queued = state.queued.saturating_sub(removed);
    Some(item)
}

fn finish_group<K, V, R, E>(state: &mut DispatchState<K, V, R, E>) {
    state.active_groups = state.active_groups.saturating_sub(1);
}

fn release_pending<V, R, E>(queue: &mut VecDeque<Queued<V, R, E>>) -> usize {
    let mut released = 0;
    for item in queue.drain(..) {
        released += 1;
        for completion in item.completions {
            let _ignored_closed_receiver = completion.send(DispatchOutcome::Shutdown);
        }
    }
    released
}

async fn wait_for_group_exit<K, V, R, E>(inner: &Arc<DispatchInner<K, V, R, E>>, key: &K)
where
    K: Eq + Hash,
{
    loop {
        let changed = inner.changed.notified();
        if !dispatch_state(inner).groups.contains_key(key) {
            return;
        }
        changed.await;
    }
}

async fn wait_for_all_groups<K, V, R, E>(inner: &Arc<DispatchInner<K, V, R, E>>) {
    loop {
        let changed = inner.changed.notified();
        if dispatch_state(inner).active_groups == 0 {
            return;
        }
        changed.await;
    }
}

fn dispatch_state<K, V, R, E>(
    inner: &DispatchInner<K, V, R, E>,
) -> MutexGuard<'_, DispatchState<K, V, R, E>> {
    inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Semaphore;

    use super::*;

    fn config(capacity: usize, workers: usize) -> GroupedReceiveDispatchConfig {
        GroupedReceiveDispatchConfig {
            capacity,
            logical_cpus: workers,
            executor_class: ReceiveExecutorClass::Standard,
        }
    }

    #[test]
    fn executor_classes_match_the_reference_cpu_multipliers() {
        assert_eq!(ReceiveExecutorClass::Standard.worker_limit(4), 80);
        assert_eq!(ReceiveExecutorClass::Expanded.worker_limit(4), 400);
        assert_eq!(ReceiveExecutorClass::Standard.worker_limit(0), 20);
    }

    #[tokio::test]
    async fn pressure_observation_does_not_consume_the_interval_put_counter() {
        let dispatcher = GroupedReceiveDispatcher::new(
            config(8, 1),
            |_key, _first: &u8, _second| None,
            |_key: u8, value| Box::pin(async move { Ok::<_, ()>(value) }),
        );
        let _completion = dispatcher.try_enqueue(1, 42).unwrap();
        let first = dispatcher.pressure_snapshot();
        let second = dispatcher.pressure_snapshot();
        assert_eq!(first.puts, 1);
        assert_eq!(second.puts, 1);
        assert_eq!(first.queued, 1);
        assert_eq!(second.queued, 1);
        assert!(second.longest_wait.is_some());
        assert_eq!(dispatcher.metrics_snapshot().puts, 1);
        assert_eq!(dispatcher.pressure_snapshot().puts, 0);
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn same_key_is_sequential_while_different_keys_run_concurrently() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let dispatcher =
            GroupedReceiveDispatcher::new(config(8, 1), |_key, _first, _second| None, {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    let active = Arc::clone(&active);
                    let max_active = Arc::clone(&max_active);
                    Box::pin(async move {
                        let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                        max_active.fetch_max(now, Ordering::AcqRel);
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        active.fetch_sub(1, Ordering::AcqRel);
                        Ok::<_, ()>(value)
                    })
                }
            });
        let first = dispatcher.enqueue("a", 1).await.unwrap();
        let second = dispatcher.enqueue("a", 2).await.unwrap();
        let other = dispatcher.enqueue("b", 3).await.unwrap();
        let permits = started.acquire_many(2).await.unwrap();
        permits.forget();
        assert_eq!(max_active.load(Ordering::Acquire), 2);
        release.add_permits(2);
        assert_eq!(
            first
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            1
        );
        assert_eq!(
            other
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            3
        );
        let permit = started.acquire().await.unwrap();
        permit.forget();
        release.add_permits(1);
        assert_eq!(
            second
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            2
        );
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn active_item_never_combines_but_adjacent_pending_items_do() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher = GroupedReceiveDispatcher::new(
            config(8, 1),
            |_key, first, second| Some(first + second),
            {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            },
        );
        let first = dispatcher.enqueue("group", 1).await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        let second = dispatcher.enqueue("group", 2).await.unwrap();
        let third = dispatcher.enqueue("group", 3).await.unwrap();
        release.add_permits(1);
        assert_eq!(
            first
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            1
        );
        let permit = started.acquire().await.unwrap();
        permit.forget();
        release.add_permits(1);
        assert_eq!(
            second
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            5
        );
        assert_eq!(
            third
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            5
        );
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn items_waiting_for_an_executor_combine_before_the_first_poll() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher = GroupedReceiveDispatcher::new(
            config(22, 1),
            |_key, first, second| Some(first + second),
            {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            },
        );
        let mut blockers = Vec::new();
        for key in 0..20 {
            blockers.push(dispatcher.enqueue(key, 100).await.unwrap());
        }
        let permits = started.acquire_many(20).await.unwrap();
        permits.forget();

        let first = dispatcher.enqueue(20, 1).await.unwrap();
        let second = dispatcher.enqueue(20, 2).await.unwrap();
        release.add_permits(1);
        let permit = started.acquire().await.unwrap();
        permit.forget();
        release.add_permits(20);
        assert_eq!(
            first
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            3
        );
        assert_eq!(
            second
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            3
        );
        for blocker in blockers {
            blocker.wait().await.unwrap();
        }
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn a_non_combinable_neighbor_is_an_exact_barrier() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher = GroupedReceiveDispatcher::new(
            config(10, 1),
            |_key, first, second| (*first != 0 && *second != 0).then_some(first + second),
            {
                let observed = Arc::clone(&observed);
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let observed = Arc::clone(&observed);
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        if value == 9 {
                            started.add_permits(1);
                            let permit = release.acquire().await.unwrap();
                            permit.forget();
                        }
                        observed.lock().unwrap().push(value);
                        Ok::<_, ()>(value)
                    })
                }
            },
        );
        let active = dispatcher.enqueue("group", 9).await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        let first = dispatcher.enqueue("group", 1).await.unwrap();
        let second = dispatcher.enqueue("group", 0).await.unwrap();
        let third = dispatcher.enqueue("group", 2).await.unwrap();
        let fourth = dispatcher.enqueue("group", 3).await.unwrap();
        release.add_permits(1);
        active.wait().await.unwrap();
        first.wait().await.unwrap();
        second.wait().await.unwrap();
        assert_eq!(
            third
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            5
        );
        assert_eq!(
            fourth
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            5
        );
        assert_eq!(*observed.lock().unwrap(), vec![9, 1, 0, 5]);
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn global_worker_limit_is_enforced_across_groups() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher = GroupedReceiveDispatcher::new(
            GroupedReceiveDispatchConfig {
                capacity: 31,
                logical_cpus: 1,
                executor_class: ReceiveExecutorClass::Standard,
            },
            |_key, _first, _second| None,
            {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            },
        );
        let mut completions = Vec::new();
        for key in 0..25 {
            completions.push(dispatcher.enqueue(key, key).await.unwrap());
        }
        let permits = started.acquire_many(20).await.unwrap();
        permits.forget();
        assert!(started.try_acquire().is_err());
        dispatcher.set_executor_class(ReceiveExecutorClass::Expanded);
        let expanded = dispatcher.enqueue(25, 25).await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        release.add_permits(20);
        let permits = started.acquire_many(5).await.unwrap();
        permits.forget();
        release.add_permits(6);
        for completion in completions {
            assert!(completion.wait().await.unwrap().is_ok());
        }
        assert!(expanded.wait().await.unwrap().is_ok());
        assert_eq!(dispatcher.executor_class(), ReceiveExecutorClass::Expanded);
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn unrelated_group_completion_cannot_steal_a_waiting_workers_fifo_position() {
        assert_waiter_fifo(false).await;
    }

    #[tokio::test]
    async fn unrelated_group_shutdown_cannot_steal_a_waiting_workers_fifo_position() {
        assert_waiter_fifo(true).await;
    }

    async fn assert_waiter_fifo(shutdown_notice: bool) {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let dispatcher =
            GroupedReceiveDispatcher::new(config(4, 1), |_key, _first: &u8, _second| None, {
                let observed = Arc::clone(&observed);
                move |key: u8, _value| {
                    let observed = Arc::clone(&observed);
                    Box::pin(async move {
                        observed.lock().unwrap().push(key);
                        Ok::<_, ()>(())
                    })
                }
            });
        let mut held = Arc::clone(&dispatcher.inner.standard_workers)
            .acquire_many_owned(20)
            .await
            .unwrap();
        // Insert without spawning so both worker futures can be polled at exact
        // boundaries. No sleep, runtime scheduling order or timing threshold is
        // used to infer their semaphore queue positions.
        {
            let mut state = dispatch_state(&dispatcher.inner);
            for key in [0, 1] {
                let permit = Arc::clone(&dispatcher.inner.capacity)
                    .try_acquire_owned()
                    .unwrap();
                state.groups.insert(
                    key,
                    Group {
                        queue: VecDeque::from([Queued {
                            value: key,
                            enqueued_at: Instant::now(),
                            completions: Vec::new(),
                            capacity_permits: vec![permit],
                        }]),
                    },
                );
            }
            state.queued = 2;
            state.active_groups = 2;
        }
        let first = run_group(
            Arc::clone(&dispatcher.inner),
            0,
            ReceiveExecutorClass::Standard,
        );
        let second = run_group(
            Arc::clone(&dispatcher.inner),
            1,
            ReceiveExecutorClass::Standard,
        );
        tokio::pin!(first, second);
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        // This is the completion signal emitted when an unrelated group exits.
        // Poll only the first waiter before releasing exactly one worker slot.
        if shutdown_notice {
            dispatcher.inner.worker_shutdown.notify_waiters();
        } else {
            dispatcher.inner.changed.notify_waiters();
        }
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(held.split(1).unwrap());
        std::future::poll_fn(|cx| {
            assert!(
                first.as_mut().poll(cx).is_ready(),
                "an unrelated completion cancelled the first worker wait and moved it behind the second"
            );
            std::task::Poll::Ready(())
        })
        .await;
        second.await;
        assert_eq!(*observed.lock().unwrap(), vec![0, 1]);
        assert_eq!(dispatcher.metrics_snapshot().queued, 0);
        drop(held);
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn queued_group_and_global_shutdown_do_not_need_an_available_worker() {
        let executed = Arc::new(AtomicUsize::new(0));
        let dispatcher =
            GroupedReceiveDispatcher::new(config(4, 1), |_key, _first: &u8, _second| None, {
                let executed = Arc::clone(&executed);
                move |_key: u8, _value| {
                    let executed = Arc::clone(&executed);
                    Box::pin(async move {
                        executed.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, ()>(())
                    })
                }
            });
        let held = Arc::clone(&dispatcher.inner.standard_workers)
            .acquire_many_owned(20)
            .await
            .unwrap();
        let first = dispatcher.enqueue(0, 0).await.unwrap();
        let second = dispatcher.enqueue(1, 1).await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), dispatcher.shutdown_group(&0))
                .await
                .expect("group shutdown waited for a worker permit"),
            1
        );
        assert_eq!(
            first.wait().await.unwrap_err(),
            DispatchCompletionError::Shutdown
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), dispatcher.shutdown())
                .await
                .expect("global shutdown waited for a worker permit"),
            1
        );
        assert_eq!(
            second.wait().await.unwrap_err(),
            DispatchCompletionError::Shutdown
        );
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        assert_eq!(dispatcher.metrics_snapshot().queued, 0);
        assert_eq!(dispatcher.metrics_snapshot().active_groups, 0);
        drop(held);
    }

    #[tokio::test]
    async fn bounded_enqueue_waits_until_a_queued_item_is_polled() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher =
            GroupedReceiveDispatcher::new(config(1, 1), |_key, _first, _second| None, {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            });
        let mut active = Vec::new();
        for key in 0..20 {
            active.push(dispatcher.enqueue(key, key).await.unwrap());
            let permit = started.acquire().await.unwrap();
            permit.forget();
        }
        let queued = dispatcher.try_enqueue(20, 20).unwrap();
        assert!(matches!(
            dispatcher.try_enqueue(21, 21),
            Err(TryEnqueueError::Full(21))
        ));
        let waiting = tokio::spawn({
            let dispatcher = dispatcher.clone();
            async move { dispatcher.enqueue(21, 21).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        release.add_permits(1);
        let permit = started.acquire().await.unwrap();
        permit.forget();
        let accepted = waiting.await.unwrap().unwrap();
        release.add_permits(1);
        let permit = started.acquire().await.unwrap();
        permit.forget();
        release.add_permits(20);
        for completion in active {
            completion.wait().await.unwrap();
        }
        queued.wait().await.unwrap();
        accepted.wait().await.unwrap();
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_group_releases_pending_and_waits_for_active_exit() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher =
            GroupedReceiveDispatcher::new(config(4, 1), |_key, _first, _second| None, {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            });
        let active = dispatcher.enqueue("group", 1).await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        let pending = dispatcher.enqueue("group", 2).await.unwrap();
        let shutdown = tokio::spawn({
            let dispatcher = dispatcher.clone();
            async move { dispatcher.shutdown_group(&"group").await }
        });
        assert_eq!(
            pending.wait().await.unwrap_err(),
            DispatchCompletionError::Shutdown
        );
        assert!(!shutdown.is_finished());
        release.add_permits(1);
        assert_eq!(
            active
                .wait()
                .await
                .unwrap()
                .as_ref()
                .as_ref()
                .copied()
                .unwrap(),
            1
        );
        assert_eq!(shutdown.await.unwrap(), 1);
        assert!(matches!(
            dispatcher.try_enqueue("group", 3),
            Err(TryEnqueueError::Shutdown(3))
        ));
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn global_shutdown_releases_pending_waits_for_active_and_rejects_new_groups() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher =
            GroupedReceiveDispatcher::new(config(4, 1), |_key, _first, _second| None, {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            });
        let active = dispatcher.enqueue("group", 1).await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        let pending = dispatcher.enqueue("group", 2).await.unwrap();
        let shutdown = tokio::spawn({
            let dispatcher = dispatcher.clone();
            async move { dispatcher.shutdown().await }
        });
        assert_eq!(
            pending.wait().await.unwrap_err(),
            DispatchCompletionError::Shutdown
        );
        assert!(!shutdown.is_finished());
        assert!(matches!(
            dispatcher.try_enqueue("other", 3),
            Err(TryEnqueueError::Shutdown(3))
        ));
        release.add_permits(1);
        active.wait().await.unwrap();
        assert_eq!(shutdown.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn metrics_report_queue_state_reset_puts_and_longest_wait() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher = GroupedReceiveDispatcher::new(
            GroupedReceiveDispatchConfig {
                capacity: 4,
                logical_cpus: 1,
                executor_class: ReceiveExecutorClass::Expanded,
            },
            |_key, _first, _second| None,
            {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            },
        );
        let active = dispatcher.enqueue("group", 1).await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        let pending = dispatcher.enqueue("group", 2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        let metrics = dispatcher.metrics_snapshot();
        assert_eq!(metrics.queued, 1);
        assert_eq!(metrics.active_groups, 1);
        assert_eq!(metrics.worker_count, 1);
        assert_eq!(metrics.puts, 2);
        assert!(metrics.longest_wait >= Some(Duration::from_millis(1)));
        assert_eq!(metrics.executor_class, "expanded");
        assert_eq!(dispatcher.metrics_snapshot().puts, 0);
        release.add_permits(1);
        active.wait().await.unwrap();
        let permit = started.acquire().await.unwrap();
        permit.forget();
        release.add_permits(1);
        pending.wait().await.unwrap();
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn worker_metric_counts_only_permit_holders_when_groups_exceed_capacity() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let dispatcher = GroupedReceiveDispatcher::new(
            GroupedReceiveDispatchConfig {
                capacity: 21,
                logical_cpus: 1,
                executor_class: ReceiveExecutorClass::Standard,
            },
            |_key, _first, _second| None,
            {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_key, value| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        started.add_permits(1);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        Ok::<_, ()>(value)
                    })
                }
            },
        );
        let mut completions = Vec::new();
        for group in 0..21 {
            completions.push(dispatcher.enqueue(group, group).await.unwrap());
        }
        for _ in 0..20 {
            let permit = started.acquire().await.unwrap();
            permit.forget();
        }
        let metrics = dispatcher.metrics_snapshot();
        assert_eq!(metrics.active_groups, 21);
        assert_eq!(metrics.worker_count, 20);
        assert_eq!(metrics.queued, 1);

        release.add_permits(21);
        for completion in completions {
            completion.wait().await.unwrap().as_ref().as_ref().unwrap();
        }
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn panicking_group_releases_its_key_and_queue_slots() {
        let dispatcher = GroupedReceiveDispatcher::new(
            config(2, 1),
            |_key, _first: &u8, _second| None,
            |_key: u8, value: u8| {
                Box::pin(async move {
                    assert_ne!(value, 0, "processor panic");
                    Ok::<_, ()>(value)
                })
            },
        );
        let panicked = dispatcher.enqueue(1, 0).await.unwrap();
        assert_eq!(
            panicked.wait().await.unwrap_err(),
            DispatchCompletionError::Panicked
        );
        // The group key must be gone (not merely idle) and its capacity slot
        // released, so ordinary traffic keeps flowing through the bound.
        let metrics = dispatcher.metrics_snapshot();
        assert_eq!(metrics.queued, 0);
        assert_eq!(metrics.active_groups, 0);

        let recovered = dispatcher.enqueue(1, 7).await.unwrap();
        assert_eq!(
            *recovered.wait().await.unwrap().as_ref().as_ref().unwrap(),
            7
        );
        // Both capacity permits (the panicked item and the recovered item) are
        // back, matching the configured bound of two.
        let first = dispatcher.enqueue(2, 1).await.unwrap();
        let second = dispatcher.enqueue(2, 2).await.unwrap();
        assert!(first.wait().await.is_ok());
        assert!(second.wait().await.is_ok());
        dispatcher.shutdown().await;
    }
}
