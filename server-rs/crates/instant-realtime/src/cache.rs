use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

/// Capacity and expiry controls for a shared query cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryCacheOptions {
    pub max_weight: u64,
    pub ttl: Duration,
    /// Maximum distinct loads this cache may run at once. Zero disables the limit.
    pub max_in_flight: usize,
}

/// A read-only, aggregate view of cache pressure. No keys or values are exposed.
///
/// Counts include entries awaiting lazy expiry. In-flight loads retain admission
/// until their futures are dropped, including cancelled and retired loads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueryCacheMetrics {
    pub entries: usize,
    pub ready_weight: u64,
    pub in_flight: usize,
    pub retired_loads: usize,
    /// Entries visited by selective retirement, including in-flight loads.
    pub retire_scanned: u64,
    /// Entries removed by selective retirement, either expired or matching.
    pub retire_retired: u64,
    /// Cumulative time spent scanning entries while holding the cache lock.
    pub retire_nanos: u64,
    /// Entries removed by opportunistic expired-entry pruning on insertion.
    pub expired_pruned: u64,
}

/// Epoch captured immediately before a load is registered.
///
/// The publisher decides what the counters mean. A cache without a publisher
/// retains every successful load exactly as before.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PublishToken {
    pub version: u64,
    pub data: u64,
    pub security: u64,
}

/// Final, cheap confirmation evaluated while the cache lock is held.
///
/// A permit normally holds an invalidation read fence: invalidation records
/// cannot interleave between the publisher's decision and the insertion.
pub struct PublishPermit<'a> {
    _hold: Option<Box<dyn Sync + 'a>>,
    confirm: Box<dyn Fn() -> bool + 'a>,
}

impl<'a> PublishPermit<'a> {
    /// Allows retention while keeping an opaque fence alive until insertion.
    pub fn guarded(hold: Box<dyn Sync + 'a>, confirm: impl Fn() -> bool + 'a) -> Self {
        Self {
            _hold: Some(hold),
            confirm: Box::new(confirm),
        }
    }

    /// Allows retention with no additional fence.
    pub fn unconditional() -> Self {
        Self {
            _hold: None,
            confirm: Box::new(|| true),
        }
    }

    /// Rechecked under the cache lock before a loaded value becomes reusable.
    #[must_use]
    pub fn confirm(&self) -> bool {
        (self.confirm)()
    }
}

impl fmt::Debug for PublishPermit<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishPermit")
            .finish_non_exhaustive()
    }
}

/// Decides whether a completed load may become reusable.
///
/// `begin` runs before the cache lock is taken; `authorize` may take an external
/// fence and must not take the cache lock. The returned permit holds that fence
/// until the cache has inserted or rejected the value. Invalidation must take
/// the same fence before retiring ready entries, which orders check and insert
/// against it without inverting the cache lock.
pub trait QueryCachePublisher<V>: Send + Sync {
    fn begin(&self) -> PublishToken;
    fn authorize<'a>(&'a self, token: PublishToken, value: &V) -> Option<PublishPermit<'a>>;
}

impl Default for QueryCacheOptions {
    fn default() -> Self {
        Self {
            max_weight: 500 * 1024 * 1024,
            ttl: Duration::from_hours(1),
            max_in_flight: 0,
        }
    }
}

/// Process-wide in-flight admission shared by independently keyed query caches.
///
/// A zero limit disables process-wide admission.
#[derive(Debug)]
pub struct QueryCacheBudget {
    state: Mutex<QueryCacheBudgetState>,
    max_in_flight: usize,
}

#[derive(Debug, Default)]
struct QueryCacheBudgetState {
    in_flight: usize,
}

impl QueryCacheBudget {
    #[must_use]
    pub const fn new(max_in_flight: usize) -> Self {
        Self {
            state: Mutex::new(QueryCacheBudgetState { in_flight: 0 }),
            max_in_flight,
        }
    }

    fn try_begin_load(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.max_in_flight > 0 && state.in_flight >= self.max_in_flight {
            return false;
        }
        state.in_flight = state.in_flight.saturating_add(1);
        true
    }

    fn finish_load(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
    }
}

/// A read-only view of a process-wide result-cache byte budget.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResultCacheBudgetSnapshot {
    /// Approximate ready bytes retained across every attached cache.
    pub bytes: u64,
    /// Configured cap; zero means the budget is disabled.
    pub max_bytes: u64,
    /// Entries evicted by the budget since process start.
    pub evictions: u64,
}

/// Process-wide ready-byte budget shared by independently keyed query caches.
///
/// Each attached cache reports its ready bytes and the oldest access tick it
/// still retains. When their sum exceeds `max_bytes`, the cache holding the
/// globally oldest entry evicts its own least-recently-used entries, so the cap
/// holds across apps without holding two cache locks at once.
pub struct ResultCacheBudget {
    max_bytes: u64,
    ticks: AtomicU64,
    state: Mutex<ResultCacheBudgetState>,
}

struct ResultCacheBudgetState {
    next_id: u64,
    evictions: u64,
    caches: BTreeMap<u64, BudgetCache>,
}

struct BudgetCache {
    ready_bytes: Arc<AtomicU64>,
    oldest_access: Arc<AtomicU64>,
    evict: Arc<dyn Fn(u64) -> (u64, u64) + Send + Sync>,
}

/// A cache's membership in a [`ResultCacheBudget`]; dropping it leaves the budget.
struct ResultCacheRegistration {
    budget: Arc<ResultCacheBudget>,
    id: u64,
    ready_bytes: Arc<AtomicU64>,
    oldest_access: Arc<AtomicU64>,
}

impl ResultCacheBudget {
    /// Creates a budget that caps ready bytes across caches. Zero disables the cap.
    #[must_use]
    pub const fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            ticks: AtomicU64::new(0),
            state: Mutex::new(ResultCacheBudgetState {
                next_id: 0,
                evictions: 0,
                caches: BTreeMap::new(),
            }),
        }
    }

    /// Samples the approximate retained bytes and cumulative budget evictions.
    #[must_use]
    pub fn snapshot(&self) -> ResultCacheBudgetSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ResultCacheBudgetSnapshot {
            bytes: state.caches.values().fold(0_u64, |total, cache| {
                total.saturating_add(cache.ready_bytes.load(Ordering::Relaxed))
            }),
            max_bytes: self.max_bytes,
            evictions: state.evictions,
        }
    }

    fn next_tick(&self) -> u64 {
        self.ticks.fetch_add(1, Ordering::Relaxed)
    }

    fn register(
        self: &Arc<Self>,
        ready_bytes: Arc<AtomicU64>,
        oldest_access: Arc<AtomicU64>,
        evict: Arc<dyn Fn(u64) -> (u64, u64) + Send + Sync>,
    ) -> ResultCacheRegistration {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.caches.insert(
            id,
            BudgetCache {
                ready_bytes: Arc::clone(&ready_bytes),
                oldest_access: Arc::clone(&oldest_access),
                evict,
            },
        );
        ResultCacheRegistration {
            budget: Arc::clone(self),
            id,
            ready_bytes,
            oldest_access,
        }
    }

    fn unregister(&self, id: u64) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .caches
            .remove(&id);
    }

    fn enforce(&self) {
        if self.max_bytes == 0 {
            return;
        }
        loop {
            let (overflow, evict) = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let used = state.caches.values().fold(0_u64, |total, cache| {
                    total.saturating_add(cache.ready_bytes.load(Ordering::Relaxed))
                });
                if used <= self.max_bytes {
                    return;
                }
                let evict = state
                    .caches
                    .values()
                    .filter(|cache| cache.ready_bytes.load(Ordering::Relaxed) > 0)
                    .min_by_key(|cache| cache.oldest_access.load(Ordering::Relaxed))
                    .map(|cache| Arc::clone(&cache.evict));
                let Some(evict) = evict else { return };
                (used - self.max_bytes, evict)
            };
            // The budget lock is released here: an eviction takes only the
            // victim cache's state lock, so publishing usage and enforcing the
            // budget never wait on each other.
            let (freed, evicted) = evict(overflow);
            if evicted > 0 {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.evictions = state.evictions.saturating_add(evicted);
            }
            if freed == 0 {
                return;
            }
        }
    }
}

impl ResultCacheRegistration {
    fn next_tick(&self) -> u64 {
        self.budget.next_tick()
    }

    fn publish(&self, ready_bytes: u64, oldest_access: Option<u64>) {
        self.ready_bytes.store(ready_bytes, Ordering::Relaxed);
        self.oldest_access
            .store(oldest_access.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn enforce(&self) {
        self.budget.enforce();
    }
}

impl Drop for ResultCacheRegistration {
    fn drop(&mut self) {
        self.budget.unregister(self.id);
    }
}

/// Shared asynchronous Datalog result cache.
///
/// Concurrent waiters for a key share one loader. Successful values remain cached,
/// loader failures are evicted, and the loader future is dropped when its last
/// waiter disappears or explicit invalidation cancels the entry.
pub struct QueryCache<K, V, E> {
    inner: Arc<CacheInner<K, V, E>>,
}

/// Already-invalidated payloads whose destruction may be moved outside outer locks.
///
/// This handle exposes no keys or values and cannot make them reusable. Cache
/// mutation and any requested cancellation complete before it is returned. Drop
/// it promptly after releasing application/session locks, without an intervening wait.
#[must_use = "release outer locks before dropping invalidated query payloads"]
pub struct QueryCacheCleanup<K, V> {
    _order: BTreeMap<u64, K>,
    _ready: Vec<(K, Arc<V>)>,
}

impl<K, V> fmt::Debug for QueryCacheCleanup<K, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueryCacheCleanup")
            .finish_non_exhaustive()
    }
}

struct CacheInner<K, V, E> {
    state: Mutex<CacheState<K, V, E>>,
    options: QueryCacheOptions,
    weigh: Arc<dyn Fn(&V) -> u64 + Send + Sync>,
    budget: Option<Arc<QueryCacheBudget>>,
    result_cache: std::sync::OnceLock<ResultCacheRegistration>,
    publisher: Option<Arc<dyn QueryCachePublisher<V>>>,
}

struct LoadCleanup {
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl LoadCleanup {
    fn new(cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cleanup: Some(Box::new(cleanup)),
        }
    }
}

impl Drop for LoadCleanup {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[derive(Debug)]
struct CacheState<K, V, E> {
    next_generation: u64,
    next_access: u64,
    ready_weight: u64,
    in_flight: usize,
    retire_scanned: u64,
    retire_retired: u64,
    retire_nanos: u64,
    expired_pruned: u64,
    entries: HashMap<K, CacheEntry<V, E>>,
    // Exactly one record per reusable ready value. Hits update in O(log n);
    // weighted eviction no longer scans all cached queries for each victim.
    ready_order: BTreeMap<u64, K>,
    retired: HashMap<u64, RetiredLoad<K>>,
}

#[derive(Debug)]
struct RetiredLoad<K> {
    key: K,
    cancel: Option<oneshot::Sender<()>>,
    /// Selective retirement detaches a load from new joiners but still allows a
    /// publisher to reuse its completed snapshot when no invalidation affects it.
    publishable: bool,
}

#[derive(Debug)]
enum CacheEntry<V, E> {
    Ready {
        value: Arc<V>,
        inserted_at: Instant,
        weight: u64,
        last_access: u64,
    },
    Loading {
        generation: u64,
        updates: watch::Sender<LoadState<V, E>>,
        cancel: Option<oneshot::Sender<()>>,
    },
}

#[derive(Debug)]
enum LoadState<V, E> {
    Pending,
    Ready(Arc<V>),
    Failed(Arc<E>),
}

impl<V, E> Clone for LoadState<V, E> {
    fn clone(&self) -> Self {
        match self {
            Self::Pending => Self::Pending,
            Self::Ready(value) => Self::Ready(Arc::clone(value)),
            Self::Failed(error) => Self::Failed(Arc::clone(error)),
        }
    }
}

/// Failure observed while waiting for a cached query.
#[derive(Debug)]
pub enum QueryCacheError<E> {
    Loader(Arc<E>),
    Cancelled,
    Overloaded,
}

impl<E> Clone for QueryCacheError<E> {
    fn clone(&self) -> Self {
        match self {
            Self::Loader(error) => Self::Loader(Arc::clone(error)),
            Self::Cancelled => Self::Cancelled,
            Self::Overloaded => Self::Overloaded,
        }
    }
}

impl<E: PartialEq> PartialEq for QueryCacheError<E> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Loader(left), Self::Loader(right)) => left == right,
            (Self::Cancelled, Self::Cancelled) | (Self::Overloaded, Self::Overloaded) => true,
            _ => false,
        }
    }
}

impl<E: Eq> Eq for QueryCacheError<E> {}

impl<E: fmt::Display> fmt::Display for QueryCacheError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loader(error) => write!(formatter, "query loader failed: {error}"),
            Self::Cancelled => formatter.write_str("query load was cancelled"),
            Self::Overloaded => formatter.write_str("query cache load capacity is exhausted"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for QueryCacheError<E> {}

impl<K, V, E> Clone for QueryCache<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V, E> fmt::Debug for QueryCache<K, V, E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueryCache")
            .field("options", &self.inner.options)
            .finish_non_exhaustive()
    }
}

impl<K, V, E> Default for QueryCache<K, V, E> {
    fn default() -> Self {
        Self::with_options(QueryCacheOptions::default(), |_| 1)
    }
}

impl<K, V, E> QueryCache<K, V, E> {
    #[must_use]
    pub fn metrics_snapshot(&self) -> QueryCacheMetrics {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        QueryCacheMetrics {
            entries: state.entries.len(),
            ready_weight: state.ready_weight,
            in_flight: state.in_flight,
            retired_loads: state.retired.len(),
            retire_scanned: state.retire_scanned,
            retire_retired: state.retire_retired,
            retire_nanos: state.retire_nanos,
            expired_pruned: state.expired_pruned,
        }
    }

    /// Creates a bounded cache with a caller-provided value weigher.
    pub fn with_options(
        options: QueryCacheOptions,
        weigh: impl Fn(&V) -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self::with_options_and_budget(options, weigh, None)
    }

    /// Creates a cache that participates in optional process-wide load admission.
    pub fn with_options_and_budget(
        options: QueryCacheOptions,
        weigh: impl Fn(&V) -> u64 + Send + Sync + 'static,
        budget: Option<Arc<QueryCacheBudget>>,
    ) -> Self {
        Self {
            inner: Arc::new(CacheInner {
                state: Mutex::new(CacheState {
                    next_generation: 0,
                    next_access: 0,
                    ready_weight: 0,
                    in_flight: 0,
                    retire_scanned: 0,
                    retire_retired: 0,
                    retire_nanos: 0,
                    expired_pruned: 0,
                    entries: HashMap::new(),
                    ready_order: BTreeMap::new(),
                    retired: HashMap::new(),
                }),
                options,
                weigh: Arc::new(weigh),
                budget,
                result_cache: std::sync::OnceLock::new(),
                publisher: None,
            }),
        }
    }

    /// Joins a process-wide ready-byte budget shared with other caches.
    ///
    /// # Panics
    ///
    /// Panics when a budget is already configured for this cache.
    #[must_use]
    pub fn with_result_cache_budget(self, budget: &Arc<ResultCacheBudget>) -> Self
    where
        K: Clone + Eq + Hash + Send + 'static,
        V: Send + Sync + 'static,
        E: Send + Sync + 'static,
    {
        let ready_bytes = Arc::new(AtomicU64::new(0));
        let oldest_access = Arc::new(AtomicU64::new(u64::MAX));
        let weak = Arc::downgrade(&self.inner);
        let evict = {
            let ready_bytes = Arc::clone(&ready_bytes);
            let oldest_access = Arc::clone(&oldest_access);
            Arc::new(move |target: u64| -> (u64, u64) {
                let Some(inner) = weak.upgrade() else {
                    return (0, 0);
                };
                let mut state = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (freed, evicted) = evict_ready_bytes(&mut state, target);
                ready_bytes.store(state.ready_weight, Ordering::Relaxed);
                oldest_access.store(
                    state
                        .ready_order
                        .first_key_value()
                        .map_or(u64::MAX, |(access, _)| *access),
                    Ordering::Relaxed,
                );
                drop(state);
                let count = u64::try_from(evicted.len()).unwrap_or(u64::MAX);
                drop(evicted);
                (freed, count)
            })
        };
        let registration = budget.register(ready_bytes, oldest_access, evict);
        assert!(
            self.inner.result_cache.set(registration).is_ok(),
            "a result cache budget must be configured only once"
        );
        self
    }

    /// Installs the publisher that conditionally retains completed loads.
    ///
    /// # Panics
    ///
    /// Panics when called after the cache handle has been cloned.
    #[must_use]
    pub fn with_publisher(mut self, publisher: Arc<dyn QueryCachePublisher<V>>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("a query cache publisher must be configured before cloning the cache")
            .publisher = Some(publisher);
        self
    }
}

impl<K: Clone + Eq + Hash, V, E> CacheInner<K, V, E> {
    fn next_access(&self, state: &mut CacheState<K, V, E>) -> u64 {
        match self.result_cache.get() {
            Some(registration) => registration.next_tick(),
            None => next_access(state),
        }
    }

    fn publish_usage(&self, ready_weight: u64, order: &BTreeMap<u64, K>) {
        let Some(registration) = self.result_cache.get() else {
            return;
        };
        registration.publish(
            ready_weight,
            order.first_key_value().map(|(access, _)| *access),
        );
    }

    fn enforce_budget(&self) {
        if let Some(registration) = self.result_cache.get() {
            registration.enforce();
        }
    }
}

impl<K, V, E> QueryCache<K, V, E>
where
    K: Clone + Eq + Hash + Send + 'static,
    V: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    /// Returns a cached result or runs and shares `loader` for the key.
    ///
    /// # Errors
    ///
    /// Returns the shared loader error, [`QueryCacheError::Cancelled`] when the entry is
    /// invalidated or its loader panics, or [`QueryCacheError::Overloaded`] when a distinct
    /// load exceeds configured admission.
    pub async fn get_or_load<F, Fut>(&self, key: K, loader: F) -> Result<Arc<V>, QueryCacheError<E>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
    {
        // Capture the external fence without holding the cache lock, so a
        // publisher may use a lock whose invalidation path also takes this
        // cache's lock. Joining an existing load ignores this token.
        let publish_token = self
            .inner
            .publisher
            .as_ref()
            .map(|publisher| publisher.begin());
        let (receiver, load) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            remove_expired(&mut state, &key, self.inner.options.ttl);
            self.inner
                .publish_usage(state.ready_weight, &state.ready_order);
            let access = self.inner.next_access(&mut state);
            if let Some(entry) = state.entries.get_mut(&key) {
                match entry {
                    CacheEntry::Ready {
                        value, last_access, ..
                    } => {
                        let previous = std::mem::replace(last_access, access);
                        let value = Arc::clone(value);
                        state.ready_order.remove(&previous);
                        state.ready_order.insert(access, key.clone());
                        self.inner
                            .publish_usage(state.ready_weight, &state.ready_order);
                        return Ok(value);
                    }
                    CacheEntry::Loading { updates, .. } => (updates.subscribe(), None),
                }
            } else {
                if (self.inner.options.max_in_flight > 0
                    && state.in_flight >= self.inner.options.max_in_flight)
                    || !self
                        .inner
                        .budget
                        .as_ref()
                        .is_none_or(|budget| budget.try_begin_load())
                {
                    return Err(QueryCacheError::Overloaded);
                }
                let generation = state.next_generation;
                state.next_generation = state.next_generation.wrapping_add(1);
                state.in_flight = state.in_flight.saturating_add(1);
                let (updates, receiver) = watch::channel(LoadState::Pending);
                let (cancel, cancel_receiver) = oneshot::channel();
                state.entries.insert(
                    key.clone(),
                    CacheEntry::Loading {
                        generation,
                        updates: updates.clone(),
                        cancel: Some(cancel),
                    },
                );
                (receiver, Some((generation, updates, cancel_receiver)))
            }
        };
        if let Some((generation, updates, cancel_receiver)) = load {
            let cache = self.clone();
            let cleanup_cache = cache.clone();
            let cleanup_key = key.clone();
            // This guard is captured by the task before it can be aborted. Within the task it
            // is declared before the loader so the loader is dropped before capacity is released.
            let cleanup = LoadCleanup::new(move || {
                cleanup_cache.cleanup_load(&cleanup_key, generation);
            });
            tokio::spawn(async move {
                let _cleanup = cleanup;
                let mut no_waiters = Box::pin(updates.closed());
                let mut cancelled = Box::pin(cancel_receiver);
                let mut load = Box::pin(loader());
                tokio::select! {
                    result = &mut load => cache.finish(key, generation, result, &updates, publish_token),
                    () = &mut no_waiters => {},
                    _ = &mut cancelled => {},
                }
            });
        }
        wait_for_result(receiver).await
    }

    /// Removes one cached query and cancels its current and retired loaders.
    pub fn invalidate(&self, key: &K) -> bool {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.entries.remove(key);
        let retired = state
            .retired
            .extract_if(|_, load| &load.key == key)
            .map(|(_, load)| load)
            .collect::<Vec<_>>();
        let removed = entry.is_some() || !retired.is_empty();
        if let Some(CacheEntry::Ready {
            weight,
            last_access,
            ..
        }) = &entry
        {
            state.ready_weight = state.ready_weight.saturating_sub(*weight);
            state.ready_order.remove(last_access);
        }
        self.inner
            .publish_usage(state.ready_weight, &state.ready_order);
        drop(state);
        if let Some(CacheEntry::Loading {
            cancel: Some(cancel),
            ..
        }) = entry
        {
            let _ = cancel.send(());
        }
        for load in retired {
            if let Some(cancel) = load.cancel {
                let _ = cancel.send(());
            }
        }
        removed
    }

    /// Removes reusable entries while allowing existing waiters to finish.
    ///
    /// Future lookups cannot join retired loaders, and retired results are never
    /// cached. Existing waiters may still receive a result from before retirement;
    /// callers must preserve their original freshness boundary. Use [`Self::clear`]
    /// when existing waiters must also be cancelled.
    ///
    /// Retired loaders retain admission until their futures are dropped. They
    /// remain subject to [`Self::invalidate`], [`Self::clear`] and last-waiter
    /// cancellation.
    pub fn retire(&self) {
        drop(self.retire_deferred());
    }

    /// Retires exactly as [`Self::retire`], but defers removed payload destruction.
    ///
    /// Reuse eligibility, generations, existing waiters and admission change at
    /// this call, not when the returned cleanup handle is dropped. This is useful
    /// when an outer lock serializes retirement with subscription registration.
    pub fn retire_deferred(&self) -> QueryCacheCleanup<K, V> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = std::mem::take(&mut state.entries);
        let order = std::mem::take(&mut state.ready_order);
        state.ready_weight = 0;
        let mut ready = Vec::new();
        for (key, entry) in entries {
            match entry {
                CacheEntry::Loading {
                    generation, cancel, ..
                } => {
                    state.retired.insert(
                        generation,
                        RetiredLoad {
                            key,
                            cancel,
                            publishable: false,
                        },
                    );
                }
                CacheEntry::Ready { value, .. } => ready.push((key, value)),
            }
        }
        // A hard retirement also withdraws publication of loads detached by an
        // earlier selective retirement.
        for load in state.retired.values_mut() {
            load.publishable = false;
        }
        self.inner
            .publish_usage(state.ready_weight, &state.ready_order);
        // JSON payload destruction may be much more expensive than retirement
        // bookkeeping. Hits, admission and metrics must not wait on it.
        drop(state);
        QueryCacheCleanup {
            _ready: ready,
            _order: order,
        }
    }

    /// Removes ready entries selected by `retire` or older than the cache TTL.
    ///
    /// In-flight loads are detached from new joiners without being cancelled. A
    /// publisher decides at completion whether their snapshot may still be
    /// reused, so a WAL no longer forces every concurrent refresh to reload.
    /// Existing waiters always receive the completed value. Ready values that
    /// `retire` returns false for remain reusable. Use [`Self::clear`] when
    /// existing waiters must also be cancelled.
    ///
    /// The caller holds whatever lock fences registration against this call, so a
    /// loader is either visible here or starts after the caller's invalidation
    /// has been recorded.
    pub fn retire_matching(
        &self,
        mut retire: impl FnMut(&K, &V) -> bool,
    ) -> QueryCacheCleanup<K, V> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scanned_started = Instant::now();
        let ttl = self.inner.options.ttl;
        let mut ready = Vec::new();
        let mut order = BTreeMap::new();
        let mut detached = Vec::new();
        let mut scanned = 0_u64;
        let mut retired = 0_u64;
        {
            // Remove matching entries in place. Kept entries are never moved or
            // rehashed, which matters because `K` can retain compiled-query bytes.
            let state = &mut *state;
            let mut extracted = state.entries.extract_if(|key, entry| {
                scanned = scanned.saturating_add(1);
                match entry {
                    CacheEntry::Loading { .. } => true,
                    CacheEntry::Ready {
                        value, inserted_at, ..
                    } => {
                        scanned_started.saturating_duration_since(*inserted_at) >= ttl
                            || retire(key, value)
                    }
                }
            });
            for (key, entry) in extracted.by_ref() {
                match entry {
                    CacheEntry::Loading {
                        generation, cancel, ..
                    } => {
                        // Detach from new joiners without cancelling: existing
                        // waiters keep their loader, and completion may publish
                        // if the epoch proves this retirement cannot affect it.
                        detached.push((
                            generation,
                            RetiredLoad {
                                key,
                                cancel,
                                publishable: true,
                            },
                        ));
                    }
                    CacheEntry::Ready {
                        value,
                        weight,
                        last_access,
                        ..
                    } => {
                        state.ready_weight = state.ready_weight.saturating_sub(weight);
                        state.ready_order.remove(&last_access);
                        order.insert(last_access, key.clone());
                        ready.push((key, value));
                        retired = retired.saturating_add(1);
                    }
                }
            }
        }
        state.retired.extend(detached);
        self.inner
            .publish_usage(state.ready_weight, &state.ready_order);
        state.retire_scanned = state.retire_scanned.saturating_add(scanned);
        state.retire_retired = state.retire_retired.saturating_add(retired);
        state.retire_nanos = state.retire_nanos.saturating_add(
            u64::try_from(scanned_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        // Payload destruction may be much more expensive than retirement
        // bookkeeping. Hits, admission and metrics must not wait on it.
        drop(state);
        QueryCacheCleanup {
            _ready: ready,
            _order: order,
        }
    }

    /// Removes all entries and cancels current and retired loaders.
    pub fn clear(&self) {
        drop(self.clear_deferred());
    }

    /// Clears and cancels exactly as [`Self::clear`], deferring only payload drops.
    ///
    /// Holding the returned handle never delays cancellation, permits stale
    /// reuse, or releases admission before a cancelled loader has exited.
    pub fn clear_deferred(&self) -> QueryCacheCleanup<K, V> {
        let (entries, retired, order) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entries = std::mem::take(&mut state.entries);
            let retired = std::mem::take(&mut state.retired);
            let order = std::mem::take(&mut state.ready_order);
            state.ready_weight = 0;
            self.inner.publish_usage(0, &state.ready_order);
            (entries, retired, order)
        };
        let mut ready = Vec::new();
        for (key, entry) in entries {
            match entry {
                CacheEntry::Loading { cancel, .. } => {
                    if let Some(cancel) = cancel {
                        let _ = cancel.send(());
                    }
                }
                CacheEntry::Ready { value, .. } => ready.push((key, value)),
            }
        }
        for load in retired.into_values() {
            if let Some(cancel) = load.cancel {
                let _ = cancel.send(());
            }
        }
        QueryCacheCleanup {
            _ready: ready,
            _order: order,
        }
    }

    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_expired(&mut state, key, self.inner.options.ttl);
        self.inner
            .publish_usage(state.ready_weight, &state.ready_order);
        state.entries.contains_key(key)
    }

    fn finish(
        &self,
        key: K,
        generation: u64,
        result: Result<V, E>,
        updates: &watch::Sender<LoadState<V, E>>,
        publish_token: Option<PublishToken>,
    ) {
        // Serialization-based weighers may traverse a large result. Keep that
        // work outside the state mutex so hits, admission and invalidation can
        // proceed. The registered generation is rechecked under the mutex below:
        // a concurrent retirement may deliver only to its original waiters, and
        // a hard invalidation must neither publish nor cache the old result.
        let weight = result.as_ref().ok().and_then(|value| {
            let current = {
                let state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                generation_matches(&state.entries, &key, generation)
                    || state
                        .retired
                        .get(&generation)
                        .is_some_and(|load| load.key == key && load.publishable)
            };
            // A result already retired or cancelled cannot be retained. Preserve
            // its completion semantics without invoking the retention weigher.
            current.then(|| (self.inner.weigh)(value))
        });
        // Ask the publisher outside the cache lock. A permit keeps its fence
        // alive until insertion, so invalidation cannot slip between the
        // decision and the insert.
        let permit = match (&result, publish_token) {
            (Ok(value), Some(token)) => self
                .inner
                .publisher
                .as_ref()
                .and_then(|publisher| publisher.authorize(token, value)),
            _ => None,
        };
        let publisher_required = self.inner.publisher.is_some();
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !generation_matches(&state.entries, &key, generation) {
            let Some(load) = state.retired.remove(&generation) else {
                return;
            };
            if load.key != key {
                state.retired.insert(generation, load);
                return;
            }
            let mut evicted = Vec::new();
            let mut retained = false;
            match result {
                Ok(value) => {
                    let value = Arc::new(value);
                    // Detached waiters always receive their value.
                    updates.send_replace(LoadState::Ready(Arc::clone(&value)));
                    let authorized = load.publishable
                        && (!publisher_required
                            || permit.as_ref().is_some_and(PublishPermit::confirm));
                    // A newer load or ready value for this key wins. Publishing
                    // here only fills a hole left by this detached load.
                    if authorized
                        && !state.entries.contains_key(&key)
                        && let Some(weight) = weight.filter(|weight| {
                            *weight <= self.inner.options.max_weight
                                && self.inner.options.max_weight > 0
                        })
                    {
                        let access = self.inner.next_access(&mut state);
                        state.ready_weight = state.ready_weight.saturating_add(weight);
                        state.ready_order.insert(access, key.clone());
                        state.entries.insert(
                            key,
                            CacheEntry::Ready {
                                value,
                                inserted_at: Instant::now(),
                                weight,
                                last_access: access,
                            },
                        );
                        evicted = evict_overweight(&mut state, self.inner.options.max_weight);
                        evicted.extend(prune_expired(
                            &mut state,
                            self.inner.options.ttl,
                            EXPIRED_PRUNE_LIMIT,
                        ));
                        retained = true;
                    }
                }
                Err(error) => {
                    updates.send_replace(LoadState::Failed(Arc::new(error)));
                }
            }
            self.inner
                .publish_usage(state.ready_weight, &state.ready_order);
            drop(state);
            drop(permit);
            drop(evicted);
            if retained {
                self.inner.enforce_budget();
            }
            return;
        }
        let mut evicted = Vec::new();
        let mut retained = false;
        match result {
            Ok(value) => {
                let value = Arc::new(value);
                let access = self.inner.next_access(&mut state);
                let authorized =
                    !publisher_required || permit.as_ref().is_some_and(PublishPermit::confirm);
                if let Some(weight) = weight.filter(|weight| {
                    authorized
                        && *weight <= self.inner.options.max_weight
                        && self.inner.options.max_weight > 0
                }) {
                    state.ready_weight = state.ready_weight.saturating_add(weight);
                    state.ready_order.insert(access, key.clone());
                    state.entries.insert(
                        key,
                        CacheEntry::Ready {
                            value: Arc::clone(&value),
                            inserted_at: Instant::now(),
                            weight,
                            last_access: access,
                        },
                    );
                    evicted = evict_overweight(&mut state, self.inner.options.max_weight);
                    evicted.extend(prune_expired(
                        &mut state,
                        self.inner.options.ttl,
                        EXPIRED_PRUNE_LIMIT,
                    ));
                    retained = true;
                } else {
                    state.entries.remove(&key);
                }
                updates.send_replace(LoadState::Ready(value));
            }
            Err(error) => {
                state.entries.remove(&key);
                updates.send_replace(LoadState::Failed(Arc::new(error)));
            }
        }
        self.inner
            .publish_usage(state.ready_weight, &state.ready_order);
        drop(state);
        drop(permit);
        drop(evicted);
        if retained {
            self.inner.enforce_budget();
        }
    }

    fn cleanup_load(&self, key: &K, generation: u64) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if generation_matches(&state.entries, key, generation) {
            state.entries.remove(key);
        }
        state.retired.remove(&generation);
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(budget) = &self.inner.budget {
            budget.finish_load();
        }
    }
}

fn remove_expired<K: Eq + Hash, V, E>(state: &mut CacheState<K, V, E>, key: &K, ttl: Duration) {
    let expired = match state.entries.get(key) {
        Some(CacheEntry::Ready {
            inserted_at,
            weight,
            last_access,
            ..
        }) if inserted_at.elapsed() >= ttl => Some((*weight, *last_access)),
        _ => None,
    };
    if let Some((weight, access)) = expired {
        state.entries.remove(key);
        state.ready_order.remove(&access);
        state.ready_weight = state.ready_weight.saturating_sub(weight);
    }
}

fn next_access<K: Clone + Eq + Hash, V, E>(state: &mut CacheState<K, V, E>) -> u64 {
    if state.next_access == u64::MAX {
        // Counter rollover must not make new entries appear oldest or leave
        // duplicate access IDs. Renumber the bounded live index in LRU order.
        let order = std::mem::take(&mut state.ready_order);
        state.next_access = 0;
        for key in order.into_values() {
            if let Some(CacheEntry::Ready { last_access, .. }) = state.entries.get_mut(&key) {
                *last_access = state.next_access;
                state.ready_order.insert(state.next_access, key);
                state.next_access += 1;
            }
        }
    }
    let access = state.next_access;
    state.next_access += 1;
    access
}

fn evict_overweight<K: Eq + Hash, V, E>(
    state: &mut CacheState<K, V, E>,
    max_weight: u64,
) -> Vec<CacheEntry<V, E>> {
    let mut removed = Vec::new();
    while state.ready_weight > max_weight {
        let Some((_, key)) = state.ready_order.pop_first() else {
            break;
        };
        if let Some(entry @ CacheEntry::Ready { .. }) = state.entries.remove(&key) {
            if let CacheEntry::Ready { weight, .. } = &entry {
                state.ready_weight = state.ready_weight.saturating_sub(*weight);
            }
            removed.push(entry);
        }
    }
    removed
}

/// Evicts least-recently-used ready entries until `target` bytes are freed.
///
/// Removed payloads are returned so their destruction happens after the state
/// lock is released.
fn evict_ready_bytes<K: Eq + Hash, V, E>(
    state: &mut CacheState<K, V, E>,
    target: u64,
) -> (u64, Vec<CacheEntry<V, E>>) {
    let mut freed = 0_u64;
    let mut evicted = Vec::new();
    while freed < target {
        let Some((_, key)) = state.ready_order.pop_first() else {
            break;
        };
        if let Some(entry @ CacheEntry::Ready { weight, .. }) = state.entries.remove(&key) {
            state.ready_weight = state.ready_weight.saturating_sub(weight);
            freed = freed.saturating_add(weight);
            evicted.push(entry);
        }
    }
    (freed, evicted)
}

/// Opportunistic insertion-time expiry. Only the oldest `limit` LRU records are
/// inspected, so an insert never inherits an unbounded scan. One-off queries
/// therefore cannot grow the map just by never looking their key up again.
const EXPIRED_PRUNE_LIMIT: usize = 64;

fn prune_expired<K: Clone + Eq + Hash, V, E>(
    state: &mut CacheState<K, V, E>,
    ttl: Duration,
    limit: usize,
) -> Vec<CacheEntry<V, E>> {
    let expired = state
        .ready_order
        .iter()
        .take(limit)
        .filter_map(|(access, key)| {
            let CacheEntry::Ready { inserted_at, .. } = state.entries.get(key)? else {
                return None;
            };
            (inserted_at.elapsed() >= ttl).then(|| (*access, key.clone()))
        })
        .collect::<Vec<_>>();
    if expired.is_empty() {
        return Vec::new();
    }
    let mut removed = Vec::with_capacity(expired.len());
    for (access, key) in expired {
        state.ready_order.remove(&access);
        if let Some(entry @ CacheEntry::Ready { weight, .. }) = state.entries.remove(&key) {
            state.ready_weight = state.ready_weight.saturating_sub(weight);
            removed.push(entry);
        }
    }
    state.expired_pruned = state
        .expired_pruned
        .saturating_add(u64::try_from(removed.len()).unwrap_or(u64::MAX));
    removed
}

fn generation_matches<K: Eq + Hash, V, E>(
    entries: &HashMap<K, CacheEntry<V, E>>,
    key: &K,
    generation: u64,
) -> bool {
    matches!(
        entries.get(key),
        Some(CacheEntry::Loading {
            generation: current,
            ..
        }) if *current == generation
    )
}

async fn wait_for_result<V, E>(
    mut receiver: watch::Receiver<LoadState<V, E>>,
) -> Result<Arc<V>, QueryCacheError<E>> {
    loop {
        match receiver.borrow_and_update().clone() {
            LoadState::Pending => {}
            LoadState::Ready(value) => return Ok(value),
            LoadState::Failed(error) => return Err(QueryCacheError::Loader(error)),
        }
        if receiver.changed().await.is_err() {
            return Err(QueryCacheError::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

    use tokio::sync::Notify;

    use super::*;

    struct DropSignal(Arc<AtomicUsize>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    type TestQueryCache = QueryCache<String, usize, &'static str>;

    fn publish_topics(attribute: u128) -> Option<Arc<[instant_wal::Topic]>> {
        Some(Arc::from(vec![instant_wal::Topic {
            indexes: Vec::new(),
            entities: instant_wal::MatchSet::Any,
            attributes: instant_wal::MatchSet::Values(vec![uuid::Uuid::from_u128(attribute)]),
            values: instant_wal::MatchSet::Any,
        }]))
    }

    struct EpochTestPublisher {
        epoch: Arc<crate::epoch::InvalidationEpoch>,
    }

    impl QueryCachePublisher<usize> for EpochTestPublisher {
        fn begin(&self) -> PublishToken {
            self.epoch.snapshot().token()
        }

        fn authorize<'a>(
            &'a self,
            token: PublishToken,
            value: &usize,
        ) -> Option<PublishPermit<'a>> {
            let value = *value as u128;
            self.epoch
                .authorize(token, move |topics| {
                    topics.iter().any(|topic| {
                        matches!(&topic.attributes, instant_wal::MatchSet::Values(values)
                            if values.contains(&uuid::Uuid::from_u128(value)))
                    })
                })
                .map(crate::epoch::EpochPermit::into_publish_permit)
        }
    }

    fn epoch_cache(epoch: &Arc<crate::epoch::InvalidationEpoch>) -> TestQueryCache {
        TestQueryCache::with_options(
            QueryCacheOptions {
                max_weight: 1024,
                ttl: Duration::from_secs(60),
                max_in_flight: 0,
            },
            |_| 1,
        )
        .with_publisher(Arc::new(EpochTestPublisher {
            epoch: Arc::clone(epoch),
        }))
    }

    struct GatedLoad {
        waiter: tokio::task::JoinHandle<Result<Arc<usize>, QueryCacheError<&'static str>>>,
        release: oneshot::Sender<()>,
        dropped: Arc<AtomicUsize>,
    }

    async fn start_gated_load(
        cache: &TestQueryCache,
        key: &str,
        result: Result<usize, &'static str>,
    ) -> GatedLoad {
        let (started, started_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let signal = Arc::clone(&dropped);
        let cache = cache.clone();
        let key = key.to_owned();
        let waiter = tokio::spawn(async move {
            cache
                .get_or_load(key, move || async move {
                    let _signal = DropSignal(signal);
                    started.send(()).unwrap();
                    release_receiver.await.unwrap();
                    result
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), started_receiver)
            .await
            .expect("the gated loader must start instead of joining an old generation")
            .unwrap();
        GatedLoad {
            waiter,
            release,
            dropped,
        }
    }

    #[tokio::test]
    async fn concurrent_waiters_share_one_successful_load() {
        let cache = QueryCache::<String, usize, &'static str>::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let first = {
            let cache = cache.clone();
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                cache
                    .get_or_load("q".into(), move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        Ok(42)
                    })
                    .await
            })
        };
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let second = {
            let cache = cache.clone();
            tokio::spawn(async move { cache.get_or_load("q".into(), || async { Ok(999) }).await })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();
        assert_eq!(*first.await.unwrap().unwrap(), 42);
        assert_eq!(*second.await.unwrap().unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(999) })
                .await
                .unwrap(),
            42
        );
    }

    #[tokio::test]
    async fn failures_are_not_cached() {
        let cache = QueryCache::<String, usize, &'static str>::default();
        assert_eq!(
            cache
                .get_or_load("q".into(), || async { Err("oops") })
                .await,
            Err(QueryCacheError::Loader(Arc::new("oops")))
        );
        assert!(!cache.contains(&"q".into()));
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(7) })
                .await
                .unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn a_failed_key_does_not_evict_an_unrelated_ready_value() {
        let cache = QueryCache::<String, usize, &'static str>::default();
        cache
            .get_or_load("ready".into(), || async { Ok(7) })
            .await
            .unwrap();

        assert!(matches!(
            cache
                .get_or_load("failed".into(), || async { Err("oops") })
                .await,
            Err(QueryCacheError::Loader(_))
        ));
        assert_eq!(
            *cache
                .get_or_load("ready".into(), || async { Ok(99) })
                .await
                .unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn invalidation_cancels_in_flight_work() {
        let cache = QueryCache::<String, usize, &'static str>::default();
        let started = Arc::new(Notify::new());
        let waiter = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                cache
                    .get_or_load("q".into(), move || async move {
                        started.notify_one();
                        std::future::pending().await
                    })
                    .await
            })
        };
        started.notified().await;
        assert!(cache.invalidate(&"q".into()));
        assert_eq!(waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
        assert!(!cache.contains(&"q".into()));
    }

    #[tokio::test]
    async fn retirement_preserves_existing_waiters_without_reusing_their_result() {
        let cache = TestQueryCache::default();
        let old = start_gated_load(&cache, "q", Ok(1)).await;
        let second = cache.get_or_load("q".into(), || async {
            panic!("the second waiter must share the existing loader")
        });
        tokio::pin!(second);
        std::future::poll_fn(|context| {
            assert!(second.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;

        cache.retire();
        assert_eq!(old.dropped.load(Ordering::Acquire), 0);
        assert!(!cache.contains(&"q".into()));
        let fresh = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_load("q".into(), || async { Ok(2) }),
        )
        .await
        .expect("new waiters must not join the retired loader")
        .unwrap();
        assert_eq!(*fresh, 2);

        old.release.send(()).unwrap();
        assert_eq!(*old.waiter.await.unwrap().unwrap(), 1);
        assert_eq!(*second.await.unwrap(), 1);
        assert_eq!(old.dropped.load(Ordering::Acquire), 1);
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(3) })
                .await
                .unwrap(),
            2,
            "the older completion must not replace the new ready result"
        );
    }

    #[tokio::test]
    async fn retired_completion_does_not_repopulate_an_empty_cache() {
        let cache = TestQueryCache::default();
        let old = start_gated_load(&cache, "q", Ok(1)).await;
        cache.retire();
        cache.retire();
        old.release.send(()).unwrap();
        assert_eq!(*old.waiter.await.unwrap().unwrap(), 1);
        assert!(!cache.contains(&"q".into()));
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn selective_retirement_keeps_non_matching_ready_values_reusable() {
        let cache = TestQueryCache::default();
        for (key, value) in [("keep", 1), ("retire", 2)] {
            cache
                .get_or_load(key.into(), move || async move { Ok(value) })
                .await
                .unwrap();
        }
        let cleanup = cache.retire_matching(|key, _| key == "retire");
        assert!(!cache.contains(&"retire".into()));
        assert!(cache.contains(&"keep".into()));
        assert_eq!(
            *cache
                .get_or_load("keep".into(), || async { Ok(99) })
                .await
                .unwrap(),
            1,
            "a non-matching ready value must still hit"
        );
        drop(cleanup);
    }

    #[tokio::test]
    async fn selective_retirement_keeps_surviving_entries_lru_state_and_weight() {
        let cache = TestQueryCache::default();
        for (key, value) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
            cache
                .get_or_load(key.into(), move || async move { Ok(value) })
                .await
                .unwrap();
        }
        let before = {
            let state = cache.inner.state.lock().unwrap();
            let access = |key: &str| {
                let Some(CacheEntry::Ready { last_access, .. }) = state.entries.get(key) else {
                    panic!("expected a ready entry for {key}");
                };
                *last_access
            };
            (
                access("a"),
                access("b"),
                access("c"),
                access("d"),
                state.ready_weight,
                state.ready_order.keys().copied().collect::<Vec<_>>(),
            )
        };
        let cleanup = cache.retire_matching(|key, _| key == "b");
        {
            let state = cache.inner.state.lock().unwrap();
            let access = |key: &str| {
                let Some(CacheEntry::Ready {
                    last_access, value, ..
                }) = state.entries.get(key)
                else {
                    panic!("a non-matching entry must survive retirement");
                };
                assert_eq!(
                    state.ready_order.get(last_access).map(String::as_str),
                    Some(key)
                );
                **value
            };
            assert_eq!(access("a"), 1);
            assert_eq!(access("c"), 3);
            assert_eq!(access("d"), 4);
            assert!(!state.entries.contains_key("b"));
            assert_eq!(state.ready_weight, before.4 - 1);
            assert_eq!(
                state.ready_order.keys().copied().collect::<Vec<_>>(),
                vec![before.0, before.2, before.3],
                "surviving entries keep their LRU positions and relative order"
            );
        }
        // A surviving entry still hits without reloading and only then moves to
        // the most-recent position.
        assert_eq!(
            *cache
                .get_or_load("c".into(), || async { Ok(99) })
                .await
                .unwrap(),
            3
        );
        let state = cache.inner.state.lock().unwrap();
        let CacheEntry::Ready { last_access, .. } = state.entries.get("c").unwrap() else {
            panic!("expected a ready entry");
        };
        assert!(*last_access > before.3);
        assert_eq!(
            state
                .ready_order
                .last_key_value()
                .map(|(_, key)| key.as_str()),
            Some("c")
        );
        drop(state);
        drop(cleanup);
    }

    #[tokio::test]
    async fn selective_retirement_leaves_in_flight_loads_to_conditional_publication() {
        let epoch = Arc::new(crate::epoch::InvalidationEpoch::new(8));
        let cache = epoch_cache(&epoch);
        let old = start_gated_load(&cache, "keep", Ok(1)).await;
        // An unrelated WAL retires no ready value and does not block this load.
        epoch.record(false, publish_topics(7));
        let cleanup = cache.retire_matching(|_, _| false);
        old.release.send(()).unwrap();
        assert_eq!(*old.waiter.await.unwrap().unwrap(), 1);
        assert_eq!(
            *cache
                .get_or_load("keep".into(), || async { Ok(3) })
                .await
                .unwrap(),
            1,
            "a non-intersecting WAL must let an in-flight load publish"
        );
        drop(cleanup);
    }

    #[tokio::test]
    async fn conditional_publication_blocks_intersecting_imprecise_security_and_overflow() {
        for blocking in ["intersecting", "imprecise", "security", "overflow"] {
            let epoch = Arc::new(crate::epoch::InvalidationEpoch::new(
                if blocking == "overflow" { 1 } else { 8 },
            ));
            let cache = epoch_cache(&epoch);
            let old = start_gated_load(&cache, "q", Ok(1)).await;
            match blocking {
                "intersecting" => {
                    epoch.record(false, publish_topics(1));
                }
                "imprecise" => {
                    epoch.record(false, None);
                }
                "security" => {
                    epoch.record(true, publish_topics(2));
                }
                "overflow" => {
                    epoch.record(false, publish_topics(2));
                    epoch.record(false, publish_topics(3));
                }
                _ => unreachable!(),
            }
            let cleanup = cache.retire_matching(|_, _| false);
            old.release.send(()).unwrap();
            assert_eq!(
                *old.waiter.await.unwrap().unwrap(),
                1,
                "{blocking}: existing waiters always receive the value"
            );
            assert!(
                !cache.contains(&"q".into()),
                "{blocking}: the stale snapshot must not become reusable"
            );
            drop(cleanup);
        }
    }

    #[tokio::test]
    async fn conditional_publication_races_retirement_without_leaving_a_stale_entry() {
        for (intersecting, expected_cached) in [(true, false), (false, true)] {
            for _ in 0..25 {
                let epoch = Arc::new(crate::epoch::InvalidationEpoch::new(8));
                let cache = epoch_cache(&epoch);
                let old = start_gated_load(&cache, "q", Ok(1)).await;
                let (go, go_receiver) = oneshot::channel();
                let invalidation = tokio::spawn({
                    let cache = cache.clone();
                    let epoch = Arc::clone(&epoch);
                    async move {
                        go_receiver.await.unwrap();
                        epoch.record(false, publish_topics(if intersecting { 1 } else { 2 }));
                        let cleanup = cache.retire_matching(|_, _| false);
                        tokio::task::yield_now().await;
                        drop(cleanup);
                    }
                });
                go.send(()).unwrap();
                old.release.send(()).unwrap();
                assert_eq!(*old.waiter.await.unwrap().unwrap(), 1);
                invalidation.await.unwrap();
                assert_eq!(
                    cache.contains(&"q".into()),
                    expected_cached,
                    "intersecting={intersecting}: check and insert must be ordered against retirement"
                );
            }
        }
    }

    #[tokio::test]
    async fn selective_retirement_prunes_expired_ready_entries() {
        let cache = TestQueryCache::default();
        for key in ["old", "fresh"] {
            cache
                .get_or_load(key.into(), || async { Ok(1) })
                .await
                .unwrap();
        }
        {
            let mut state = cache.inner.state.lock().unwrap();
            let CacheEntry::Ready { inserted_at, .. } = state.entries.get_mut("old").unwrap()
            else {
                panic!("expected a ready entry");
            };
            *inserted_at = Instant::now() - Duration::from_hours(2);
        }
        let cleanup = cache.retire_matching(|_, _| false);
        assert!(
            !cache.contains(&"old".into()),
            "expired entries must be pruned"
        );
        assert!(cache.contains(&"fresh".into()));
        let metrics = cache.metrics_snapshot();
        assert_eq!(metrics.retire_scanned, 2);
        assert_eq!(metrics.retire_retired, 1);
        assert!(metrics.retire_nanos > 0);
        drop(cleanup);
    }

    #[tokio::test]
    async fn insertion_prunes_a_bounded_number_of_expired_entries() {
        let cache = TestQueryCache::with_options(
            QueryCacheOptions {
                max_weight: u64::MAX,
                ttl: Duration::from_secs(60),
                max_in_flight: 0,
            },
            |_| 1,
        );
        for index in 0..100 {
            cache
                .get_or_load(format!("old-{index}"), || async { Ok(1) })
                .await
                .unwrap();
        }
        {
            let mut state = cache.inner.state.lock().unwrap();
            for entry in state.entries.values_mut() {
                let CacheEntry::Ready { inserted_at, .. } = entry else {
                    panic!("expected ready entries");
                };
                *inserted_at = Instant::now() - Duration::from_hours(2);
            }
        }
        cache
            .get_or_load("new".into(), || async { Ok(1) })
            .await
            .unwrap();
        let first = cache.metrics_snapshot();
        assert_eq!(
            first.expired_pruned, EXPIRED_PRUNE_LIMIT as u64,
            "one insertion must not scan or prune more than its bound"
        );
        assert!(cache.contains(&"new".into()));
        cache
            .get_or_load("newer".into(), || async { Ok(1) })
            .await
            .unwrap();
        let second = cache.metrics_snapshot();
        assert_eq!(
            second.expired_pruned, 100,
            "the remaining expired entries are pruned by later insertions"
        );
        assert!(cache.contains(&"new".into()) && cache.contains(&"newer".into()));
    }

    #[tokio::test]
    async fn retirement_evicts_ready_values() {
        let cache = TestQueryCache::default();
        cache
            .get_or_load("q".into(), || async { Ok(1) })
            .await
            .unwrap();
        cache.retire();
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn retired_loader_errors_reach_existing_waiters_without_evicting_replacements() {
        let cache = TestQueryCache::default();
        let old = start_gated_load(&cache, "q", Err("old failure")).await;
        cache.retire();
        cache
            .get_or_load("q".into(), || async { Ok(2) })
            .await
            .unwrap();
        old.release.send(()).unwrap();
        assert_eq!(
            old.waiter.await.unwrap(),
            Err(QueryCacheError::Loader(Arc::new("old failure")))
        );
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(3) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn hard_clear_cancels_current_and_multiple_retired_generations() {
        let cache = TestQueryCache::default();
        let first = start_gated_load(&cache, "q", Ok(1)).await;
        cache.retire();
        let second = start_gated_load(&cache, "q", Ok(2)).await;
        cache.retire();
        let current = start_gated_load(&cache, "q", Ok(3)).await;

        cache.clear();
        for load in [first, second, current] {
            assert_eq!(load.waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
            assert_eq!(load.dropped.load(Ordering::Acquire), 1);
        }
        assert!(!cache.contains(&"q".into()));
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(4) })
                .await
                .unwrap(),
            4
        );
    }

    #[tokio::test]
    async fn key_invalidation_cancels_every_generation_but_preserves_other_retired_keys() {
        let cache = TestQueryCache::default();
        let first = start_gated_load(&cache, "q", Ok(1)).await;
        let other = start_gated_load(&cache, "other", Ok(7)).await;
        cache.retire();
        let second = start_gated_load(&cache, "q", Ok(2)).await;
        cache.retire();
        let current = start_gated_load(&cache, "q", Ok(3)).await;

        assert!(cache.invalidate(&"q".into()));
        for load in [first, second, current] {
            assert_eq!(load.waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
            assert_eq!(load.dropped.load(Ordering::Acquire), 1);
        }
        assert!(!cache.invalidate(&"q".into()));
        assert_eq!(other.dropped.load(Ordering::Acquire), 0);
        other.release.send(()).unwrap();
        assert_eq!(*other.waiter.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn retirement_keeps_local_and_process_admission_until_the_loader_is_dropped() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = TestQueryCache::with_options_and_budget(
            QueryCacheOptions {
                max_in_flight: 1,
                ..QueryCacheOptions::default()
            },
            |_| 1,
            Some(Arc::clone(&budget)),
        );
        let other = TestQueryCache::with_options_and_budget(
            QueryCacheOptions::default(),
            |_| 1,
            Some(budget),
        );
        let old = start_gated_load(&cache, "q", Ok(1)).await;
        cache.retire();
        assert_eq!(
            cache.get_or_load("q".into(), || async { Ok(2) }).await,
            Err(QueryCacheError::Overloaded)
        );
        assert_eq!(
            other.get_or_load("other".into(), || async { Ok(3) }).await,
            Err(QueryCacheError::Overloaded)
        );
        assert!(cache.invalidate(&"q".into()));
        assert_eq!(old.dropped.load(Ordering::Acquire), 0);
        assert_eq!(
            other.get_or_load("other".into(), || async { Ok(3) }).await,
            Err(QueryCacheError::Overloaded)
        );
        assert_eq!(old.waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
        assert_eq!(old.dropped.load(Ordering::Acquire), 1);
        assert_eq!(
            *other
                .get_or_load("other".into(), || async { Ok(3) })
                .await
                .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn dropping_a_retired_last_waiter_preserves_a_replacement_loader() {
        let cache = TestQueryCache::with_options_and_budget(
            QueryCacheOptions {
                max_in_flight: 2,
                ..QueryCacheOptions::default()
            },
            |_| 1,
            Some(Arc::new(QueryCacheBudget::new(2))),
        );
        let old = start_gated_load(&cache, "q", Ok(1)).await;
        cache.retire();
        let replacement = start_gated_load(&cache, "q", Ok(2)).await;
        assert_eq!(
            cache.get_or_load("other".into(), || async { Ok(3) }).await,
            Err(QueryCacheError::Overloaded)
        );
        old.waiter.abort();
        let _ = old.waiter.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while old.dropped.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retired work must stop when its final waiter disappears");
        assert_eq!(
            *cache
                .get_or_load("other".into(), || async { Ok(3) })
                .await
                .unwrap(),
            3,
            "dropping the retired loader must release its admission"
        );
        replacement.release.send(()).unwrap();
        assert_eq!(*replacement.waiter.await.unwrap().unwrap(), 2);
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(3) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn panicking_retired_loader_releases_process_admission() {
        let cache = TestQueryCache::with_options_and_budget(
            QueryCacheOptions::default(),
            |_| 1,
            Some(Arc::new(QueryCacheBudget::new(1))),
        );
        let (started, started_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let loader_cache = cache.clone();
        let waiter = tokio::spawn(async move {
            loader_cache
                .get_or_load("q".into(), move || async move {
                    started.send(()).unwrap();
                    release_receiver.await.unwrap();
                    panic!("retired loader panic")
                })
                .await
        });
        started_receiver.await.unwrap();
        cache.retire();
        release.send(()).unwrap();
        assert_eq!(waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn dropping_last_waiter_cancels_and_evicts_loader() {
        let cache = QueryCache::<String, usize, &'static str>::default();
        let started = Arc::new(Notify::new());
        let waiter = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                cache
                    .get_or_load("q".into(), move || async move {
                        started.notify_one();
                        std::future::pending().await
                    })
                    .await
            })
        };
        started.notified().await;
        waiter.abort();
        for _ in 0..100 {
            if !cache.contains(&"q".into()) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("loader remained cached after its final waiter was dropped");
    }

    #[tokio::test]
    async fn ready_values_expire_after_the_configured_ttl() {
        let cache = QueryCache::<String, usize, &'static str>::with_options(
            QueryCacheOptions {
                max_weight: 10,
                ttl: Duration::ZERO,
                max_in_flight: 0,
            },
            |_| 1,
        );
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(1) })
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn weighted_capacity_evicts_the_least_recently_used_value() {
        let cache = QueryCache::<String, usize, &'static str>::with_options(
            QueryCacheOptions {
                max_weight: 5,
                ttl: Duration::from_secs(60),
                max_in_flight: 0,
            },
            |value| *value as u64,
        );
        cache
            .get_or_load("a".into(), || async { Ok(2) })
            .await
            .unwrap();
        cache
            .get_or_load("b".into(), || async { Ok(2) })
            .await
            .unwrap();
        cache
            .get_or_load("a".into(), || async { Ok(99) })
            .await
            .unwrap();
        cache
            .get_or_load("c".into(), || async { Ok(2) })
            .await
            .unwrap();

        assert!(cache.contains(&"a".into()));
        assert!(!cache.contains(&"b".into()));
        assert!(cache.contains(&"c".into()));
    }

    fn assert_ready_index(cache: &TestQueryCache) {
        let state = cache.inner.state.lock().unwrap();
        let ready = state
            .entries
            .values()
            .filter(|entry| matches!(entry, CacheEntry::Ready { .. }))
            .count();
        assert_eq!(
            state.ready_order.len(),
            ready,
            "one index record per ready value, none per load"
        );
        let mut total = 0_u64;
        for (access, key) in &state.ready_order {
            let CacheEntry::Ready {
                last_access,
                weight,
                ..
            } = state.entries.get(key).unwrap()
            else {
                panic!("LRU index must reference only ready values");
            };
            assert_eq!(last_access, access);
            total = total.saturating_add(*weight);
        }
        assert_eq!(total, state.ready_weight);
    }

    #[tokio::test]
    async fn ready_index_stays_bounded_through_hits_expiry_invalidation_and_retirement() {
        let cache = TestQueryCache::with_options(
            QueryCacheOptions {
                max_weight: 4,
                ..QueryCacheOptions::default()
            },
            |_| 1,
        );
        for step in 0..128 {
            let key = format!("q-{}", step % 6);
            cache
                .get_or_load(key.clone(), || async { Ok(1) })
                .await
                .unwrap();
            cache
                .get_or_load(key.clone(), || async { Ok(999) })
                .await
                .unwrap();
            assert_ready_index(&cache);
            match step % 5 {
                0 => {
                    cache.invalidate(&key);
                }
                1 => cache.retire(),
                2 => cache.clear(),
                3 => {
                    {
                        let mut state = cache.inner.state.lock().unwrap();
                        if let Some(CacheEntry::Ready { inserted_at, .. }) =
                            state.entries.get_mut(&key)
                        {
                            *inserted_at = Instant::now() - Duration::from_hours(2);
                        }
                    }
                    assert!(!cache.contains(&key));
                }
                _ => {}
            }
            assert_ready_index(&cache);
        }
    }

    #[tokio::test]
    async fn lru_counter_rollover_preserves_order_and_unique_index_ids() {
        let cache = TestQueryCache::with_options(
            QueryCacheOptions {
                max_weight: 2,
                ..QueryCacheOptions::default()
            },
            |_| 1,
        );
        for key in ["a", "b"] {
            cache
                .get_or_load(key.into(), || async { Ok(1) })
                .await
                .unwrap();
        }
        cache
            .get_or_load("a".into(), || async { Ok(99) })
            .await
            .unwrap();
        cache.inner.state.lock().unwrap().next_access = u64::MAX;
        cache
            .get_or_load("b".into(), || async { Ok(99) })
            .await
            .unwrap();
        cache
            .get_or_load("c".into(), || async { Ok(1) })
            .await
            .unwrap();
        assert!(!cache.contains(&"a".into()));
        assert!(cache.contains(&"b".into()) && cache.contains(&"c".into()));
        assert_ready_index(&cache);
    }

    #[derive(Debug)]
    struct CountedKey {
        id: usize,
        clones: Arc<AtomicUsize>,
    }

    impl Clone for CountedKey {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                id: self.id,
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl PartialEq for CountedKey {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }
    impl Eq for CountedKey {}
    impl Hash for CountedKey {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            self.id.hash(state);
        }
    }

    #[tokio::test]
    async fn evicting_one_value_does_not_clone_every_cached_key() {
        let clones = Arc::new(AtomicUsize::new(0));
        let cache = QueryCache::<CountedKey, usize, &'static str>::with_options(
            QueryCacheOptions {
                max_weight: 1024,
                ..QueryCacheOptions::default()
            },
            |_| 1,
        );
        for id in 0..1024 {
            cache
                .get_or_load(
                    CountedKey {
                        id,
                        clones: Arc::clone(&clones),
                    },
                    || async { Ok(1) },
                )
                .await
                .unwrap();
        }
        clones.store(0, Ordering::Relaxed);
        cache
            .get_or_load(
                CountedKey {
                    id: 1024,
                    clones: Arc::clone(&clones),
                },
                || async { Ok(1) },
            )
            .await
            .unwrap();
        assert!(
            clones.load(Ordering::Relaxed) <= 12,
            "one eviction must not traverse/clone 1024 keys"
        );
        assert_eq!(cache.metrics_snapshot().entries, 1024);
    }

    struct DropProbe(Box<dyn Fn() + Send + Sync>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    #[tokio::test]
    async fn deferred_payload_cleanup_releases_outer_locks_without_delaying_invalidation() {
        for clear in [false, true] {
            let cache = QueryCache::<String, DropProbe, &'static str>::default();
            let outer = Arc::new(Mutex::new(()));
            let observed = Arc::new(AtomicUsize::new(0));
            let record = Arc::clone(&observed);
            let outer_probe = Arc::clone(&outer);
            let weak_cache = Arc::downgrade(&cache.inner);
            let value = cache
                .get_or_load("old".into(), move || async move {
                    Ok(DropProbe(Box::new(move || {
                        let inner = weak_cache.upgrade().unwrap();
                        record.store(
                            if outer_probe.try_lock().is_ok() && inner.state.try_lock().is_ok() {
                                1
                            } else {
                                2
                            },
                            Ordering::Relaxed,
                        );
                    })))
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while cache.metrics_snapshot().in_flight != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            drop(value);
            let cleanup;
            {
                let _outer_guard = outer.lock().unwrap();
                cleanup = if clear {
                    cache.clear_deferred()
                } else {
                    cache.retire_deferred()
                };
                assert_eq!(cache.metrics_snapshot(), QueryCacheMetrics::default());
                assert_eq!(format!("{cleanup:?}"), "QueryCacheCleanup { .. }");
                assert!(!cache.contains(&"old".to_owned()));
                assert_eq!(observed.load(Ordering::Relaxed), 0);
            }
            let fresh_loads = Arc::new(AtomicUsize::new(0));
            let calls = Arc::clone(&fresh_loads);
            cache
                .get_or_load("old".into(), move || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(DropProbe(Box::new(|| {})))
                })
                .await
                .unwrap();
            assert_eq!(fresh_loads.load(Ordering::Relaxed), 1);
            assert_eq!(observed.load(Ordering::Relaxed), 0);
            drop(cleanup);
            assert_eq!(observed.load(Ordering::Relaxed), 1);
            assert!(cache.contains(&"old".to_owned()));
        }
    }

    #[tokio::test]
    async fn deferred_retirement_keeps_waiters_and_admission_but_never_reuses_old_results() {
        let cache = TestQueryCache::with_options(
            QueryCacheOptions {
                max_in_flight: 2,
                ..QueryCacheOptions::default()
            },
            |_| 1,
        );
        let first = start_gated_load(&cache, "q", Ok(1)).await;
        let cleanup = cache.retire_deferred();
        assert_eq!(cache.metrics_snapshot().in_flight, 1);
        assert_eq!(cache.metrics_snapshot().retired_loads, 1);
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
        first.release.send(()).unwrap();
        assert_eq!(*first.waiter.await.unwrap().unwrap(), 1);
        assert_eq!(first.dropped.load(Ordering::Acquire), 1);
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(3) })
                .await
                .unwrap(),
            2
        );
        drop(cleanup);
        assert_eq!(cache.metrics_snapshot().in_flight, 0);
        assert_eq!(cache.metrics_snapshot().retired_loads, 0);
    }

    #[tokio::test]
    async fn deferred_clear_cancels_current_and_retired_loads_before_cleanup_is_dropped() {
        let cache = TestQueryCache::default();
        let retired = start_gated_load(&cache, "retired", Ok(1)).await;
        let retired_cleanup = cache.retire_deferred();
        let current = start_gated_load(&cache, "current", Ok(2)).await;
        assert_eq!(cache.metrics_snapshot().in_flight, 2);
        let cleanup = cache.clear_deferred();
        tokio::time::timeout(Duration::from_secs(1), async {
            assert_eq!(
                retired.waiter.await.unwrap(),
                Err(QueryCacheError::Cancelled)
            );
            assert_eq!(
                current.waiter.await.unwrap(),
                Err(QueryCacheError::Cancelled)
            );
            while cache.metrics_snapshot().in_flight != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(retired.dropped.load(Ordering::Acquire), 1);
        assert_eq!(current.dropped.load(Ordering::Acquire), 1);
        assert_eq!(cache.metrics_snapshot(), QueryCacheMetrics::default());
        drop((cleanup, retired_cleanup));
    }

    #[tokio::test]
    #[ignore = "manual outer-lock retirement characterization; not capacity acceptance"]
    async fn outer_lock_retirement_benchmark() {
        for deferred in [false, true] {
            let cache = QueryCache::<usize, Vec<Vec<String>>, &'static str>::default();
            for key in 0..100 {
                cache
                    .get_or_load(key, move || async move {
                        Ok((0..1000)
                            .map(|row| {
                                (0..10)
                                    .map(|field| format!("synthetic-{key}-{row}-{field}"))
                                    .collect()
                            })
                            .collect())
                    })
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while cache.metrics_snapshot().in_flight != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let outer = Mutex::new(());
            let cleanup;
            let started = std::time::Instant::now();
            let lock_us;
            {
                let _guard = outer.lock().unwrap();
                cleanup = if deferred {
                    Some(cache.retire_deferred())
                } else {
                    cache.retire();
                    None
                };
                lock_us = started.elapsed().as_micros();
            }
            drop(cleanup);
            println!(
                "cache-retirement deferred={deferred} entries=100 rows=1000 fields=10 lock_us={lock_us} total_us={}",
                started.elapsed().as_micros()
            );
        }
    }

    #[tokio::test]
    async fn retired_and_evicted_payloads_are_destroyed_outside_the_cache_lock() {
        for action in ["retire", "evict"] {
            let cache = QueryCache::<String, DropProbe, &'static str>::with_options(
                QueryCacheOptions {
                    max_weight: 1,
                    ..QueryCacheOptions::default()
                },
                |_| 1,
            );
            let weak = Arc::downgrade(&cache.inner);
            let observed = Arc::new(AtomicUsize::new(0));
            let record = Arc::clone(&observed);
            let value = cache
                .get_or_load("old".into(), move || async move {
                    Ok(DropProbe(Box::new(move || {
                        if let Some(inner) = weak.upgrade() {
                            record.store(
                                if inner.state.try_lock().is_ok() { 1 } else { 2 },
                                Ordering::Relaxed,
                            );
                        }
                    })))
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while cache.metrics_snapshot().in_flight != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            drop(value);
            if action == "retire" {
                cache.retire();
            } else {
                cache
                    .get_or_load("new".into(), || async { Ok(DropProbe(Box::new(|| {}))) })
                    .await
                    .unwrap();
            }
            assert_eq!(
                observed.load(Ordering::Relaxed),
                1,
                "{action} held the cache mutex during value destruction"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn weighing_does_not_block_invalidation_or_publish_an_obsolete_generation() {
        for mutation in ["clear", "invalidate", "retire"] {
            let (started, started_receiver) = oneshot::channel();
            let started = Mutex::new(Some(started));
            let (release, release_receiver) = std::sync::mpsc::sync_channel(1);
            let release_receiver = Mutex::new(release_receiver);
            let cache = TestQueryCache::with_options(QueryCacheOptions::default(), move |value| {
                if *value == 1 {
                    started.lock().unwrap().take().unwrap().send(()).unwrap();
                    release_receiver
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .expect("test must release the blocked weigher");
                }
                1
            });
            let loading = cache.clone();
            let first =
                tokio::spawn(
                    async move { loading.get_or_load("q".into(), || async { Ok(1) }).await },
                );
            tokio::time::timeout(Duration::from_secs(1), started_receiver)
                .await
                .unwrap()
                .unwrap();

            let invalidating = cache.clone();
            let mutation_done = tokio::task::spawn_blocking(move || {
                match mutation {
                    "clear" => invalidating.clear(),
                    "invalidate" => {
                        assert!(invalidating.invalidate(&"q".to_owned()));
                    }
                    "retire" => invalidating.retire(),
                    _ => unreachable!(),
                }
                // Sampling also must not wait for the weigher to finish.
                invalidating.metrics_snapshot()
            });
            let outcome = tokio::time::timeout(Duration::from_secs(1), mutation_done).await;
            if outcome.is_err() {
                // Let the loader exit even when a regression holds the mutex.
                release.send(()).unwrap();
                panic!("{mutation} waited for the result weigher");
            }
            let snapshot = outcome.unwrap().unwrap();
            assert_eq!(snapshot.entries, 0);
            assert_eq!(snapshot.in_flight, 1, "admission lasts until loader exit");
            assert_eq!(snapshot.retired_loads, usize::from(mutation == "retire"));

            assert_eq!(
                *cache
                    .get_or_load("q".into(), || async { Ok(2) })
                    .await
                    .unwrap(),
                2
            );
            release.send(()).unwrap();
            let first = first.await.unwrap();
            if mutation == "retire" {
                assert_eq!(*first.unwrap(), 1, "retirement preserves existing waiters");
            } else {
                assert_eq!(first, Err(QueryCacheError::Cancelled));
            }
            assert_eq!(
                *cache
                    .get_or_load("q".into(), || async { Ok(3) })
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                cache.metrics_snapshot(),
                QueryCacheMetrics {
                    entries: 1,
                    ready_weight: 1,
                    in_flight: 0,
                    retired_loads: 0,
                    ..QueryCacheMetrics::default()
                }
            );
        }
    }

    #[tokio::test]
    async fn panicking_weigher_releases_admission_and_allows_a_new_load() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = TestQueryCache::with_options_and_budget(
            QueryCacheOptions::default(),
            |value| {
                assert_ne!(*value, 1, "weigher panic");
                1
            },
            Some(Arc::clone(&budget)),
        );
        assert_eq!(
            cache.get_or_load("q".into(), || async { Ok(1) }).await,
            Err(QueryCacheError::Cancelled)
        );
        assert_eq!(cache.metrics_snapshot(), QueryCacheMetrics::default());
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
        assert_eq!(budget.state.lock().unwrap().in_flight, 0);
    }

    #[tokio::test]
    async fn an_already_retired_result_does_not_invoke_the_retention_weigher() {
        let cache = TestQueryCache::with_options(QueryCacheOptions::default(), |_| {
            panic!("a retired result cannot be retained or weighed")
        });
        let loading = start_gated_load(&cache, "q", Ok(7)).await;
        cache.retire();
        loading.release.send(()).unwrap();
        assert_eq!(*loading.waiter.await.unwrap().unwrap(), 7);
        assert_eq!(cache.metrics_snapshot(), QueryCacheMetrics::default());
    }

    #[tokio::test]
    async fn an_oversized_value_is_shared_but_not_retained() {
        let cache = QueryCache::<String, usize, &'static str>::with_options(
            QueryCacheOptions {
                max_weight: 1,
                ttl: Duration::from_secs(60),
                max_in_flight: 0,
            },
            |value| *value as u64,
        );
        assert_eq!(
            *cache
                .get_or_load("q".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
        assert!(!cache.contains(&"q".into()));
    }

    #[tokio::test]
    async fn in_flight_limit_rejects_distinct_keys_but_preserves_deduplication() {
        let cache = QueryCache::<String, usize, &'static str>::with_options(
            QueryCacheOptions {
                max_weight: 10,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
        );
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                cache
                    .get_or_load("first".into(), move || async move {
                        started.notify_one();
                        release.notified().await;
                        Ok(1)
                    })
                    .await
            })
        };
        started.notified().await;
        assert_eq!(
            cache.get_or_load("second".into(), || async { Ok(2) }).await,
            Err(QueryCacheError::Overloaded)
        );
        release.notify_waiters();
        assert_eq!(*first.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn process_in_flight_limit_is_shared_across_caches() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let first = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 10,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(Arc::clone(&budget)),
        );
        let second = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 10,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(budget),
        );
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first_load = {
            let first = first.clone();
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                first
                    .get_or_load("first".into(), move || async move {
                        started.notify_one();
                        release.notified().await;
                        Ok(1)
                    })
                    .await
            })
        };
        started.notified().await;
        assert_eq!(
            second
                .get_or_load("second".into(), || async { Ok(2) })
                .await,
            Err(QueryCacheError::Overloaded)
        );
        release.notify_waiters();
        assert_eq!(*first_load.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn invalidation_releases_process_in_flight_admission() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(budget),
        );
        let started = Arc::new(Notify::new());
        let waiter = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                cache
                    .get_or_load("first".into(), move || async move {
                        started.notify_one();
                        std::future::pending().await
                    })
                    .await
            })
        };
        started.notified().await;
        assert!(cache.invalidate(&"first".into()));
        assert_eq!(waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
        assert_eq!(
            *cache
                .get_or_load("second".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn invalidation_holds_admission_until_the_loader_is_dropped() {
        let cache = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(Arc::new(QueryCacheBudget::new(1))),
        );
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            let dropped = Arc::clone(&dropped);
            tokio::spawn(async move {
                cache
                    .get_or_load("first".into(), move || async move {
                        let _signal = DropSignal(dropped);
                        started.notify_one();
                        std::future::pending::<Result<usize, &'static str>>().await
                    })
                    .await
            })
        };
        started.notified().await;
        assert!(cache.invalidate(&"first".into()));
        assert_eq!(
            cache.get_or_load("second".into(), || async { Ok(2) }).await,
            Err(QueryCacheError::Overloaded)
        );
        assert_eq!(waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
        for _ in 0..100 {
            if dropped.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        assert_eq!(
            *cache
                .get_or_load("second".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[test]
    fn runtime_shutdown_before_first_poll_releases_shared_admission() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 0,
            },
            |_| 1,
            Some(Arc::clone(&budget)),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let load = cache.get_or_load("first".into(), || async {
                panic!("loader must not be polled before runtime shutdown")
            });
            tokio::pin!(load);
            std::future::poll_fn(|context| match load.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(_) => panic!("load unexpectedly completed"),
            })
            .await;
        });
        drop(runtime);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(
                *cache
                    .get_or_load("second".into(), || async { Ok(2) })
                    .await
                    .unwrap(),
                2
            );
        });
    }

    #[tokio::test]
    async fn clear_releases_process_in_flight_admission() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(budget),
        );
        let started = Arc::new(Notify::new());
        let waiter = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                cache
                    .get_or_load("first".into(), move || async move {
                        started.notify_one();
                        std::future::pending().await
                    })
                    .await
            })
        };
        started.notified().await;
        cache.clear();
        assert_eq!(waiter.await.unwrap(), Err(QueryCacheError::Cancelled));
        assert_eq!(
            *cache
                .get_or_load("second".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn last_waiter_cancellation_releases_process_in_flight_admission() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(budget),
        );
        let started = Arc::new(Notify::new());
        let waiter = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                cache
                    .get_or_load("first".into(), move || async move {
                        started.notify_one();
                        std::future::pending().await
                    })
                    .await
            })
        };
        started.notified().await;
        waiter.abort();
        let _ = waiter.await;
        for _ in 0..100 {
            if !cache.contains(&"first".into()) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *cache
                .get_or_load("second".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn panicking_loader_releases_process_in_flight_admission() {
        let budget = Arc::new(QueryCacheBudget::new(1));
        let cache = QueryCache::<String, usize, &'static str>::with_options_and_budget(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
            Some(budget),
        );

        assert_eq!(
            cache
                .get_or_load("first".into(), || async { panic!("loader panic") })
                .await,
            Err(QueryCacheError::Cancelled)
        );
        assert_eq!(
            *cache
                .get_or_load("second".into(), || async { Ok(2) })
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn in_flight_limit_applies_when_result_retention_is_disabled() {
        let cache = QueryCache::<String, usize, &'static str>::with_options(
            QueryCacheOptions {
                max_weight: 0,
                ttl: Duration::from_secs(60),
                max_in_flight: 1,
            },
            |_| 1,
        );
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = {
            let cache = cache.clone();
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                cache
                    .get_or_load("first".into(), move || async move {
                        started.notify_one();
                        release.notified().await;
                        Ok(1)
                    })
                    .await
            })
        };
        started.notified().await;
        assert_eq!(
            cache.get_or_load("second".into(), || async { Ok(2) }).await,
            Err(QueryCacheError::Overloaded)
        );
        release.notify_waiters();
        assert_eq!(*first.await.unwrap().unwrap(), 1);
        assert!(!cache.contains(&"first".into()));
    }

    fn budgeted_cache(budget: &Arc<ResultCacheBudget>) -> TestQueryCache {
        TestQueryCache::with_options(
            QueryCacheOptions {
                max_weight: u64::MAX,
                ttl: Duration::from_secs(60),
                max_in_flight: 0,
            },
            |_| 1,
        )
        .with_result_cache_budget(budget)
    }

    async fn load_one(cache: &TestQueryCache, key: &str) {
        assert_eq!(
            *cache
                .get_or_load(key.into(), || async { Ok(1) })
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn process_byte_budget_evicts_the_globally_oldest_entries_across_caches() {
        let budget = Arc::new(ResultCacheBudget::new(2));
        let first = budgeted_cache(&budget);
        let second = budgeted_cache(&budget);
        load_one(&first, "a").await;
        load_one(&second, "b").await;
        assert_eq!(budget.snapshot().bytes, 2);

        // The insertion belongs to the second app, but the first app's entry is
        // globally the least recently used.
        load_one(&second, "c").await;
        assert!(!first.contains(&"a".into()));
        assert!(second.contains(&"b".into()) && second.contains(&"c".into()));

        // The next insertion evicts from the other app for the same reason.
        load_one(&first, "d").await;
        assert!(!second.contains(&"b".into()));
        assert!(second.contains(&"c".into()) && first.contains(&"d".into()));
        assert_eq!(
            budget.snapshot(),
            ResultCacheBudgetSnapshot {
                bytes: 2,
                max_bytes: 2,
                evictions: 2,
            }
        );
    }

    #[tokio::test]
    async fn process_byte_budget_follows_invalidation_and_cache_removal() {
        let budget = Arc::new(ResultCacheBudget::new(1024));
        let first = budgeted_cache(&budget);
        let second = budgeted_cache(&budget);
        load_one(&first, "a").await;
        load_one(&second, "b").await;
        assert_eq!(budget.snapshot().bytes, 2);

        assert!(first.invalidate(&"a".into()));
        assert_eq!(budget.snapshot().bytes, 1);
        first.clear();
        assert_eq!(budget.snapshot().bytes, 1);

        drop(second);
        assert_eq!(
            budget.snapshot(),
            ResultCacheBudgetSnapshot {
                bytes: 0,
                max_bytes: 1024,
                evictions: 0,
            }
        );
    }
}
