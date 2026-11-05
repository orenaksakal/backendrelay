use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::future::Future;
use std::ops::{Deref, DerefMut};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deadpool_postgres::{
    BuildError, Hook, HookError, Manager, ManagerConfig, Object, Pool, PoolError, RecyclingMethod,
    Runtime,
};
use thiserror::Error;
use tokio::sync::{Mutex, MutexGuard};
use tokio_postgres::Client;
use tokio_postgres_rustls::MakeRustlsConnect;

const POOL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const POOL_CREATE_TIMEOUT: Duration = Duration::from_secs(10);
const POOL_RECYCLE_TIMEOUT: Duration = Duration::from_secs(10);
const POOL_MAX_LIFETIME: Duration = Duration::from_secs(600);
const POOL_SUMMARY_INTERVAL: Duration = Duration::from_secs(10);
// One runaway statement must not pin a connection forever. Interactive
// requests fail fast; refresh and control work keeps a longer budget for
// catalog and policy reads. Maintenance pools stream backups and restores and
// keep the server's own statement timeout.
const DEFAULT_STATEMENT_TIMEOUT_INTERACTIVE: Duration = Duration::from_secs(15);
const DEFAULT_STATEMENT_TIMEOUT_BACKGROUND: Duration = Duration::from_secs(30);
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);
// Noncumulative buckets, with one additional bucket above the final bound.
const POOL_WAIT_BUCKET_UPPER_BOUNDS_MS: [f64; 20] = [
    0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1000.0, 2000.0,
    4000.0, 8000.0, 16000.0, 32000.0, 64000.0,
];
static NEXT_POOL_SUMMARY_ID: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    static DATABASE_WORKLOAD: DatabaseWorkload;
    // Hooks are polled inline by deadpool's get future, including when two
    // different checkout modes are joined on the same task. Never infer the
    // requested mode from the previous borrower or retain session-mode state.
    static CHECKOUT_READ_ONLY: bool;
    static CHECKOUT_TIMINGS: Option<CheckoutTimings>;
}

// One inline task-local accumulator per get(), not per physical connection.
// It retains timings only, never mode state, SQL, parameters or request data.
struct CheckoutTimings {
    name: &'static str,
    started: Instant,
    state: StdMutex<CheckoutTimingState>,
    #[cfg(test)]
    report: Option<Arc<StdMutex<Option<CheckoutTimingReport>>>>,
}

#[cfg(test)]
struct CheckoutTimingReport {
    outside_mode: Duration,
    unfinished_mode: bool,
    outcome: crate::metrics::PoolTimingOutcome,
}

struct CheckoutTimingState {
    mode_started: Option<Instant>,
    mode_elapsed: Duration,
    outcome: crate::metrics::PoolTimingOutcome,
}

impl CheckoutTimings {
    fn start(name: &'static str) -> Option<Self> {
        crate::metrics::enabled().then(|| Self::new(name, Instant::now()))
    }

    fn new(name: &'static str, started: Instant) -> Self {
        Self {
            name,
            started,
            state: StdMutex::new(CheckoutTimingState {
                mode_started: None,
                mode_elapsed: Duration::ZERO,
                outcome: crate::metrics::PoolTimingOutcome::Cancelled,
            }),
            #[cfg(test)]
            report: None,
        }
    }

    fn begin_mode(&self, now: Instant) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.mode_started.is_none());
        state.mode_started = Some(now);
    }

    fn end_mode(&self, now: Instant, outcome: crate::metrics::PoolTimingOutcome) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(started) = state.mode_started.take() {
            let elapsed = now.saturating_duration_since(started);
            state.mode_elapsed = state.mode_elapsed.saturating_add(elapsed);
            crate::metrics::record_checkout_mode(self.name, elapsed, outcome);
        }
    }

    fn finish(&self, outcome: crate::metrics::PoolTimingOutcome) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcome = outcome;
    }

    fn elapsed_parts(&self, now: Instant) -> (Duration, Duration) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = state.mode_started.map_or(Duration::ZERO, |started| {
            now.saturating_duration_since(started)
        });
        let mode = state.mode_elapsed.saturating_add(active);
        (
            now.saturating_duration_since(self.started)
                .saturating_sub(mode),
            mode,
        )
    }
}

impl Drop for CheckoutTimings {
    fn drop(&mut self) {
        let now = Instant::now();
        let (outside_mode, _) = self.elapsed_parts(now);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(test)]
        if let Some(report) = &self.report {
            *report.lock().unwrap() = Some(CheckoutTimingReport {
                outside_mode,
                unfinished_mode: state.mode_started.is_some(),
                outcome: state.outcome,
            });
        }
        if let Some(started) = state.mode_started.take() {
            crate::metrics::record_checkout_mode(
                self.name,
                now.saturating_duration_since(started),
                crate::metrics::PoolTimingOutcome::Cancelled,
            );
        }
        // This residual includes queueing, connection creation, fast recycling
        // and bookkeeping; it is not advertised as pure semaphore queue time.
        crate::metrics::record_wait_connect(self.name, outside_mode, state.outcome);
    }
}

async fn configure_checkout(
    client: &deadpool_postgres::ClientWrapper,
    always_set: bool,
) -> Result<(), tokio_postgres::Error> {
    let read_only = CHECKOUT_READ_ONLY.try_with(|value| *value).unwrap_or(false);
    // Hot data pools skip the redundant SET for the overwhelmingly common
    // unchanged-mode checkout; a new connection, every mode change and every
    // checkout of a verifying pool still issue exactly one statement. The
    // registry holds a weak reference so a recycled address cannot alias
    // another connection.
    if !always_set && connection_mode(&client.statement_cache) == Some(read_only) {
        return Ok(());
    }
    let _ = CHECKOUT_TIMINGS.try_with(|timing| {
        if let Some(timing) = timing {
            timing.begin_mode(Instant::now());
        }
    });
    let result = client
        .batch_execute(if read_only {
            "SET default_transaction_read_only = on"
        } else {
            "SET default_transaction_read_only = off"
        })
        .await;
    let _ = CHECKOUT_TIMINGS.try_with(|timing| {
        if let Some(timing) = timing {
            timing.end_mode(
                Instant::now(),
                if result.is_ok() {
                    crate::metrics::PoolTimingOutcome::Completed
                } else {
                    crate::metrics::PoolTimingOutcome::Error
                },
            );
        }
    });
    if result.is_ok() {
        record_connection_mode(&client.statement_cache, read_only);
    }
    result
}

/// Session mode per live physical connection.
///
/// The key is the address of the connection's `StatementCache` allocation; the
/// stored `Weak` keeps that allocation alive only as an identity witness, so an
/// address reused after a connection is discarded is treated as unknown rather
/// than inheriting the previous mode.
struct ConnectionMode {
    cache: Weak<deadpool_postgres::StatementCache>,
    read_only: bool,
}

static CONNECTION_MODES: OnceLock<StdMutex<HashMap<usize, ConnectionMode>>> = OnceLock::new();

fn connection_modes() -> &'static StdMutex<HashMap<usize, ConnectionMode>> {
    CONNECTION_MODES.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn connection_mode(cache: &Arc<deadpool_postgres::StatementCache>) -> Option<bool> {
    let key = Arc::as_ptr(cache) as usize;
    let modes = connection_modes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = modes.get(&key)?;
    // A live strong count and an equal allocation address prove identity: the
    // address cannot be reused while the cache is still alive.
    (entry.cache.strong_count() > 0 && entry.cache.as_ptr() == Arc::as_ptr(cache))
        .then_some(entry.read_only)
}

fn record_connection_mode(cache: &Arc<deadpool_postgres::StatementCache>, read_only: bool) {
    let key = Arc::as_ptr(cache) as usize;
    let mut modes = connection_modes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if modes.len() >= 4096 {
        modes.retain(|_, entry| entry.cache.strong_count() > 0);
    }
    modes.insert(
        key,
        ConnectionMode {
            cache: Arc::downgrade(cache),
            read_only,
        },
    );
}

/// Session bounds applied once when a physical connection is created.
///
/// The values are process-wide (`INSTANT_PG_*`); a `SET LOCAL` inside a
/// transaction still takes precedence for that statement. Per-query policy
/// overrides therefore keep working exactly as before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PostgresSessionSettings {
    statement_timeout_interactive: Duration,
    statement_timeout_background: Duration,
    lock_wait: Duration,
    idle_transaction_wait: Duration,
}

impl Default for PostgresSessionSettings {
    fn default() -> Self {
        Self {
            statement_timeout_interactive: DEFAULT_STATEMENT_TIMEOUT_INTERACTIVE,
            statement_timeout_background: DEFAULT_STATEMENT_TIMEOUT_BACKGROUND,
            lock_wait: DEFAULT_LOCK_TIMEOUT,
            idle_transaction_wait: DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT,
        }
    }
}

impl PostgresSessionSettings {
    fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank or malformed values keep the default rather than failing startup:
    /// a typo in one timeout must not take the whole server down.
    fn from_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Self {
        let defaults = Self::default();
        Self {
            statement_timeout_interactive: lookup_millis(
                &mut get,
                "INSTANT_PG_STATEMENT_TIMEOUT_INTERACTIVE_MS",
                defaults.statement_timeout_interactive,
            ),
            statement_timeout_background: lookup_millis(
                &mut get,
                "INSTANT_PG_STATEMENT_TIMEOUT_BACKGROUND_MS",
                defaults.statement_timeout_background,
            ),
            lock_wait: lookup_millis(&mut get, "INSTANT_PG_LOCK_TIMEOUT_MS", defaults.lock_wait),
            idle_transaction_wait: lookup_millis(
                &mut get,
                "INSTANT_PG_IDLE_IN_TX_TIMEOUT_MS",
                defaults.idle_transaction_wait,
            ),
        }
    }

    fn apply_sql(self, role: PoolSessionRole) -> String {
        use std::fmt::Write as _;
        let (statement_timeout, lock_timeout) = match role {
            PoolSessionRole::Interactive => (
                Some(self.statement_timeout_interactive),
                Some(self.lock_wait),
            ),
            PoolSessionRole::Background => (
                Some(self.statement_timeout_background),
                Some(self.lock_wait),
            ),
            // Maintenance keeps the server's statement and lock semantics: a
            // fleet-wide bootstrap advisory lock or a restore DDL wait may
            // legitimately outlive an interactive request.
            PoolSessionRole::Maintenance => (None, None),
        };
        let mut sql = String::new();
        if let Some(timeout) = statement_timeout {
            let _ = write!(sql, "SET statement_timeout = '{}'; ", timeout.as_millis());
        }
        if let Some(timeout) = lock_timeout {
            let _ = write!(sql, "SET lock_timeout = '{}'; ", timeout.as_millis());
        }
        let _ = write!(
            sql,
            "SET idle_in_transaction_session_timeout = '{}'",
            self.idle_transaction_wait.as_millis()
        );
        sql
    }
}

fn lookup_millis(
    get: &mut impl FnMut(&str) -> Option<String>,
    name: &str,
    default: Duration,
) -> Duration {
    get(name)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(default, Duration::from_millis)
}

fn session_settings() -> &'static PostgresSessionSettings {
    static SETTINGS: OnceLock<PostgresSessionSettings> = OnceLock::new();
    SETTINGS.get_or_init(PostgresSessionSettings::from_env)
}

/// The three roles a pool can play, decided by the name its owner gives it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PoolSessionRole {
    Interactive,
    Background,
    Maintenance,
}

// These pools stream backups, restores, snapshot exports or bulk maintenance
// statements that may legitimately run for minutes, and the sketch bootstrap
// waits on a fleet-wide advisory lock. They keep the server's statement and
// lock timeouts and only receive the idle-in-transaction bound.
const MAINTENANCE_POOLS: &[&str] = &[
    "backup_control",
    "backup_snapshot",
    "custodian",
    "ephemeral_sweeper",
    "nightly_backup_control",
    "partition_retention",
    "rate_limit_sweeper",
    "restore_worker",
    "sketch_aggregator_snapshot",
    "sketch_aggregator_write",
    "storage_sweeper",
    "triples_size_collector",
];

fn pool_session_role(name: &str) -> PoolSessionRole {
    match name {
        // The shared pool is the primary data pool when a deployment is too
        // small to partition (see `workload_capacities`).
        "interactive" | "shared" => PoolSessionRole::Interactive,
        "refresh" | "control" => PoolSessionRole::Background,
        name if MAINTENANCE_POOLS.contains(&name) => PoolSessionRole::Maintenance,
        _ => PoolSessionRole::Background,
    }
}

/// Whether a pool proves a recycled connection is still alive before handing it
/// out.
///
/// Commit 3f4449ec stopped issuing the session-mode `SET` on unchanged modes to
/// save one round trip per checkout. That statement had also been the only
/// liveness probe on a recycled connection: `RecyclingMethod::Fast` just checks
/// `Client::is_closed()`, which stays false until the connection task has
/// processed a FATAL. A backend terminated with `57P01` (or by a restart or
/// failover) could therefore be delivered to the next caller.
///
/// Hot pools keep the saved round trip and accept that the first checkout
/// right after such a termination may fail with a closed connection; deadpool
/// discards it once the driver observes the close. After a real restart or
/// failover the socket closes while PostgreSQL is down, so `is_closed()` is
/// already true by the time requests resume and the window is milliseconds.
/// Background, control and maintenance pools verify on every recycle instead,
/// because one failed job or lost control write is not worth the microsecond.
/// The unpartitioned `shared` fallback verifies too, so a small deployment
/// keeps the failover behavior it had before 3f4449ec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckoutVerification {
    /// Re-assert the session mode on every checkout, even when unchanged. The
    /// statement doubles as the liveness probe.
    EveryCheckout,
    /// Skip the mode statement when the recorded session mode already matches.
    ModeChangeOnly,
}

impl CheckoutVerification {
    fn for_pool_name(name: &str) -> Self {
        match name {
            // The partitioned interactive and refresh pools carry the request
            // hot path. The single-connection realtime writers (presence,
            // broadcast, room events, acknowledgements) and the per-request
            // rate-limit consumer are serialized per message, so an extra round
            // trip would cap their throughput; one failed presence write or
            // rate-limit check after a terminated backend is harmless.
            "interactive"
            | "refresh"
            | "realtime_room_writer"
            | "realtime_room_events"
            | "realtime_acknowledgement"
            | "rate_limit" => Self::ModeChangeOnly,
            _ => Self::EveryCheckout,
        }
    }
}

async fn apply_session_settings(
    client: &deadpool_postgres::ClientWrapper,
    role: PoolSessionRole,
) -> Result<(), tokio_postgres::Error> {
    client
        .batch_execute(&session_settings().apply_sql(role))
        .await
}

/// Execution class is explicit at task boundaries; spawned work must carry it.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum DatabaseWorkload {
    #[default]
    Interactive,
    Refresh,
}

impl DatabaseWorkload {
    pub(crate) fn current() -> Self {
        DATABASE_WORKLOAD
            .try_with(|workload| *workload)
            .unwrap_or_default()
    }

    pub(crate) async fn scope<F: Future>(self, future: F) -> F::Output {
        DATABASE_WORKLOAD.scope(self, future).await
    }
}

fn connection_expired(age: Duration) -> bool {
    age >= POOL_MAX_LIFETIME
}

/// A bounded, lazily growing PostgreSQL connection pool with native Tokio
/// connection drivers.
#[derive(Clone)]
pub struct PostgresPool {
    inner: Pool,
    name: &'static str,
    summary: Option<Arc<PoolAcquisitionSummary>>,
    refresh: Option<Arc<Self>>,
    control: Option<Arc<Self>>,
    /// Only the interactive data pool enforces a per-app share; refresh,
    /// control and maintenance pools keep their own admission policy.
    app_admission: Option<Arc<crate::app_interactive_admission::AppInteractiveAdmission>>,
    #[cfg(test)]
    acquisitions: Arc<AtomicUsize>,
}

// A dropped acquisition future is a cancelled wait, not a successful checkout.
// This includes scheduled policies whose deadline expires while the pool is full.
struct PoolAcquisition<'a> {
    _metrics: Option<crate::metrics::PoolWait>,
    pool: &'a PostgresPool,
    started: Option<Instant>,
    summarize: bool,
    outcome: PoolAcquisitionOutcome,
}

impl<'a> PoolAcquisition<'a> {
    fn new(pool: &'a PostgresPool) -> Self {
        let summarize = pool.summary.is_some()
            && tracing::enabled!(target: "instant_server::database_pool_summary", tracing::Level::DEBUG);
        let detailed =
            tracing::enabled!(target: "instant_server::database_pool", tracing::Level::DEBUG);
        // The disabled path does not read the clock, status counters, or mutex.
        let started = (summarize || detailed).then(Instant::now);
        if let (true, Some(summary), Some(started)) = (summarize, &pool.summary, started) {
            summary.begin(started, pool.physical_status().waiting);
        }
        Self {
            _metrics: crate::metrics::PoolWait::start(pool.name),
            pool,
            started,
            summarize,
            outcome: PoolAcquisitionOutcome::Cancelled,
        }
    }

    async fn acquire(&self) -> Result<Object, PoolError> {
        if !self.summarize {
            return self.pool.inner.get().await;
        }
        let mut checkout = std::pin::pin!(self.pool.inner.get());
        let mut observed_wait = false;
        std::future::poll_fn(|context| {
            let result = checkout.as_mut().poll(context);
            if result.is_pending() && !observed_wait {
                observed_wait = true;
                if let Some(summary) = &self.pool.summary {
                    // Observe after enqueue as well, including a sole waiter
                    // that disappears before its cancelled Drop runs.
                    summary.observe_waiting(self.pool.physical_status().waiting);
                }
            }
            result
        })
        .await
    }
}

impl Drop for PoolAcquisition<'_> {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        let detailed =
            tracing::enabled!(target: "instant_server::database_pool", tracing::Level::DEBUG);
        let summarize = self.summarize
            && tracing::enabled!(target: "instant_server::database_pool_summary", tracing::Level::DEBUG);
        if !detailed && !summarize {
            return;
        }
        let now = Instant::now();
        let wait = now.saturating_duration_since(started);
        let status = self.pool.physical_status();
        if let (true, Some(summary)) = (summarize, &self.pool.summary)
            && let Some(interval) = summary.record(now, wait, self.outcome, status.waiting)
        {
            interval.emit(self.pool.name, summary.id, status);
        }
        if !detailed {
            return;
        }
        tracing::debug!(
            target: "instant_server::database_pool",
            pool = self.pool.name,
            wait_ms = wait.as_secs_f64() * 1000.0,
            outcome = self.outcome.as_str(),
            max_size = status.max_size,
            size = status.size,
            available = status.available,
            waiting = status.waiting,
            "database connection acquisition"
        );
    }
}

#[derive(Clone, Copy)]
enum PoolAcquisitionOutcome {
    Acquired,
    Cancelled,
    Error,
}

impl PoolAcquisitionOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

#[derive(Default)]
struct PoolWaitSummary {
    count: u64,
    sum_ms: f64,
    min_ms: f64,
    max_ms: f64,
    buckets: [u64; POOL_WAIT_BUCKET_UPPER_BOUNDS_MS.len() + 1],
}

impl PoolWaitSummary {
    fn record(&mut self, wait: Duration) {
        let wait_ms = wait.as_secs_f64() * 1000.0;
        if self.count == 0 || wait_ms < self.min_ms {
            self.min_ms = wait_ms;
        }
        self.max_ms = self.max_ms.max(wait_ms);
        self.sum_ms += wait_ms;
        self.count = self.count.saturating_add(1);
        let bucket = POOL_WAIT_BUCKET_UPPER_BOUNDS_MS.partition_point(|bound| *bound < wait_ms);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
}

struct PoolSummaryInterval {
    started: Instant,
    started_unix_ms: u64,
    timestamp_remainder: Duration,
    acquired: PoolWaitSummary,
    cancelled: PoolWaitSummary,
    error: PoolWaitSummary,
    waiting_max: usize,
}

impl PoolSummaryInterval {
    fn new(started: Instant, started_unix_ms: u64, waiting: usize) -> Self {
        Self {
            started,
            started_unix_ms,
            timestamp_remainder: Duration::ZERO,
            acquired: PoolWaitSummary::default(),
            cancelled: PoolWaitSummary::default(),
            error: PoolWaitSummary::default(),
            waiting_max: waiting,
        }
    }
}

struct PoolSummarySnapshot {
    interval: PoolSummaryInterval,
    elapsed: Duration,
    ended_unix_ms: u64,
}

impl PoolSummarySnapshot {
    fn emit(self, pool: &'static str, pool_id: u64, status: PostgresPoolStatus) {
        let PoolSummaryInterval {
            started_unix_ms,
            acquired,
            cancelled,
            error,
            waiting_max,
            ..
        } = self.interval;
        tracing::debug!(
            target: "instant_server::database_pool_summary",
            pool,
            pool_id,
            interval_start_unix_ms = started_unix_ms,
            interval_end_unix_ms = self.ended_unix_ms,
            interval_ms = self.elapsed.as_secs_f64() * 1000.0,
            max_size = status.max_size,
            size = status.size,
            available = status.available,
            waiting = status.waiting,
            waiting_max,
            wait_bucket_upper_bounds_ms = ?POOL_WAIT_BUCKET_UPPER_BOUNDS_MS,
            acquired_count = acquired.count,
            acquired_wait_ms_sum = acquired.sum_ms,
            acquired_wait_ms_min = acquired.min_ms,
            acquired_wait_ms_max = acquired.max_ms,
            acquired_wait_bucket_counts = ?acquired.buckets,
            cancelled_count = cancelled.count,
            cancelled_wait_ms_sum = cancelled.sum_ms,
            cancelled_wait_ms_min = cancelled.min_ms,
            cancelled_wait_ms_max = cancelled.max_ms,
            cancelled_wait_bucket_counts = ?cancelled.buckets,
            error_count = error.count,
            error_wait_ms_sum = error.sum_ms,
            error_wait_ms_min = error.min_ms,
            error_wait_ms_max = error.max_ms,
            error_wait_bucket_counts = ?error.buckets,
            "database connection acquisition summary"
        );
    }
}

// Each physical pool owns one fixed-size accumulator; clones and detached
// workload handles retain its Arc. Unnamed per-app pools do not allocate one.
struct PoolAcquisitionSummary {
    id: u64,
    interval: StdMutex<Option<PoolSummaryInterval>>,
}

impl PoolAcquisitionSummary {
    fn new() -> Self {
        Self {
            id: NEXT_POOL_SUMMARY_ID.fetch_add(1, Ordering::Relaxed),
            interval: StdMutex::new(None),
        }
    }

    fn begin(&self, now: Instant, waiting: usize) {
        self.begin_at(now, waiting, || {
            u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        });
    }

    fn begin_at(&self, now: Instant, waiting: usize, unix_ms: impl FnOnce() -> u64) {
        let mut state = self
            .interval
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interval =
            state.get_or_insert_with(|| PoolSummaryInterval::new(now, unix_ms(), waiting));
        interval.waiting_max = interval.waiting_max.max(waiting);
    }

    fn observe_waiting(&self, waiting: usize) {
        let mut state = self
            .interval
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(interval) = state.as_mut() {
            interval.waiting_max = interval.waiting_max.max(waiting);
        }
    }

    fn record(
        &self,
        now: Instant,
        wait: Duration,
        outcome: PoolAcquisitionOutcome,
        waiting: usize,
    ) -> Option<PoolSummarySnapshot> {
        let mut state = self
            .interval
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interval = state.as_mut()?;
        interval.waiting_max = interval.waiting_max.max(waiting);
        match outcome {
            PoolAcquisitionOutcome::Acquired => &mut interval.acquired,
            PoolAcquisitionOutcome::Cancelled => &mut interval.cancelled,
            PoolAcquisitionOutcome::Error => &mut interval.error,
        }
        .record(wait);
        let elapsed = now.saturating_duration_since(interval.started);
        if elapsed < POOL_SUMMARY_INTERVAL {
            return None;
        }
        // A single lock covers accounting and rollover so concurrent completions
        // cannot duplicate intervals or lose records. Times use one wall-clock
        // anchor and monotonic elapsed time, unaffected by later clock changes.
        // Carry fractional milliseconds so rounding each emitted boundary does
        // not accumulate drift away from the original clock anchor.
        let anchored_elapsed = elapsed.saturating_add(interval.timestamp_remainder);
        let ended_unix_ms = interval
            .started_unix_ms
            .saturating_add(u64::try_from(anchored_elapsed.as_millis()).unwrap_or(u64::MAX));
        let mut next = PoolSummaryInterval::new(now, ended_unix_ms, waiting);
        next.timestamp_remainder =
            Duration::from_nanos(u64::from(anchored_elapsed.subsec_nanos() % 1_000_000));
        let completed = std::mem::replace(interval, next);
        Some(PoolSummarySnapshot {
            interval: completed,
            elapsed,
            ended_unix_ms,
        })
    }
}

/// Atomic snapshot of deadpool's current capacity counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PostgresPoolStatus {
    pub max_size: usize,
    pub size: usize,
    pub available: usize,
    pub waiting: usize,
}

#[derive(Clone)]
pub(crate) enum PostgresClientSource {
    Fixed(Arc<Mutex<Client>>),
    Pool { pool: PostgresPool, read_only: bool },
}

impl fmt::Debug for PostgresClientSource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresClientSource")
            .field(
                "kind",
                &match self {
                    Self::Fixed(_) => "fixed",
                    Self::Pool {
                        read_only: false, ..
                    } => "pool",
                    Self::Pool {
                        read_only: true, ..
                    } => "read-only-pool",
                },
            )
            .finish()
    }
}

// The production variant intentionally stays inline: boxing it would add a
// heap allocation to every database checkout merely to optimize the smaller
// fixed-client compatibility constructor used by tests and embedders.
#[allow(clippy::large_enum_variant)]
enum PostgresClientGuardInner<'a> {
    Fixed(MutexGuard<'a, Client>),
    Pooled(Object),
}

pub(crate) struct PostgresClientGuard<'a> {
    inner: Option<PostgresClientGuardInner<'a>>,
    registration: Option<crate::statement_cancel::StatementRegistration>,
    lease_timer: Option<crate::metrics::SourceLeaseTimer>,
    app_permit: Option<crate::app_interactive_admission::AppInteractivePermit>,
    /// App this lease serves, for per-app database time. Only datalog leases set it.
    datalog_app: Option<(uuid::Uuid, Instant)>,
}

// Dynamic SQL is admitted only under hard per-connection count/SQL-size bounds.
// Resetting a full cache drops plans, not data; PostgreSQL remains responsible
// for normal dependency invalidation and generic/custom plan selection.
const NATIVE_STATEMENT_CACHE_ENTRIES: usize = 64;
const NATIVE_STATEMENT_SQL_BYTES: usize = 64 * 1024;

pub(crate) struct NativeQueryPreparer<'a> {
    client: &'a Client,
    cache: Option<Arc<deadpool_postgres::StatementCache>>,
}

impl<'a> NativeQueryPreparer<'a> {
    pub(crate) fn new(
        client: &'a Client,
        cache: Option<Arc<deadpool_postgres::StatementCache>>,
    ) -> Self {
        Self { client, cache }
    }
}

impl instant_pg::QueryStatementPreparer for NativeQueryPreparer<'_> {
    #[tracing::instrument(name = "datalog/prepare-statement", skip_all)]
    async fn prepare(
        &self,
        sql: &str,
    ) -> Result<Option<tokio_postgres::Statement>, tokio_postgres::Error> {
        let Some(cache) = &self.cache else {
            return Ok(None);
        };
        if sql.len() > NATIVE_STATEMENT_SQL_BYTES {
            return Ok(None);
        }
        if cache.size() >= NATIVE_STATEMENT_CACHE_ENTRIES {
            cache.clear();
        }
        cache.prepare(self.client, sql).await.map(Some)
    }
}

impl<'a> PostgresClientGuard<'a> {
    pub(crate) fn native_statement_cache(&self) -> Option<Arc<deadpool_postgres::StatementCache>> {
        match self.inner.as_ref() {
            Some(PostgresClientGuardInner::Pooled(client)) => {
                Some(Arc::clone(&client.statement_cache))
            }
            Some(PostgresClientGuardInner::Fixed(_)) => None,
            None => unreachable!("a live connection guard always owns its lease"),
        }
    }
    /// Reuses a prepared statement only on the physical connection that owns it.
    /// The fixed-client compatibility path prepares normally; production pools
    /// discard their statement cache when a connection is replaced.
    pub(crate) async fn prepare_cached(
        &self,
        query: &str,
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        match self.inner.as_ref() {
            Some(PostgresClientGuardInner::Fixed(client)) => client.prepare(query).await,
            Some(PostgresClientGuardInner::Pooled(client)) => client.prepare_cached(query).await,
            None => unreachable!("a live connection guard always owns its lease"),
        }
    }

    pub(crate) async fn prepare_typed_cached(
        &self,
        query: &'static str,
        types: &[tokio_postgres::types::Type],
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        match self.inner.as_ref() {
            Some(PostgresClientGuardInner::Fixed(client)) => {
                client.prepare_typed(query, types).await
            }
            Some(PostgresClientGuardInner::Pooled(client)) => {
                client.prepare_typed_cached(query, types).await
            }
            None => unreachable!("a live connection guard always owns its lease"),
        }
    }

    // Only fixed SQL text is admitted here. Plans belong to physical connections;
    // parameter values and returned rows are never retained. In particular, live
    // policy changes must be visible on the very next execution of a cached plan.
    pub(crate) async fn query_typed_cached(
        &self,
        query: &'static str,
        params: &[(
            &(dyn tokio_postgres::types::ToSql + Sync),
            tokio_postgres::types::Type,
        )],
    ) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
        let types: Vec<_> = params.iter().map(|(_, ty)| ty.clone()).collect();
        let statement = self.prepare_typed_cached(query, &types).await?;
        let values: Vec<_> = params.iter().map(|(value, _)| *value).collect();
        self.query(&statement, &values).await
    }

    pub(crate) async fn query_typed_cached_opt(
        &self,
        query: &'static str,
        params: &[(
            &(dyn tokio_postgres::types::ToSql + Sync),
            tokio_postgres::types::Type,
        )],
    ) -> Result<Option<tokio_postgres::Row>, tokio_postgres::Error> {
        let types: Vec<_> = params.iter().map(|(_, ty)| ty.clone()).collect();
        let statement = self.prepare_typed_cached(query, &types).await?;
        let values: Vec<_> = params.iter().map(|(value, _)| *value).collect();
        self.query_opt(&statement, &values).await
    }

    pub(crate) async fn query_typed_cached_one(
        &self,
        query: &'static str,
        params: &[(
            &(dyn tokio_postgres::types::ToSql + Sync),
            tokio_postgres::types::Type,
        )],
    ) -> Result<tokio_postgres::Row, tokio_postgres::Error> {
        let types: Vec<_> = params.iter().map(|(_, ty)| ty.clone()).collect();
        let statement = self.prepare_typed_cached(query, &types).await?;
        let values: Vec<_> = params.iter().map(|(value, _)| *value).collect();
        self.query_one(&statement, &values).await
    }

    fn new(inner: PostgresClientGuardInner<'a>) -> Self {
        let registration = crate::statement_cancel::register_current(match &inner {
            PostgresClientGuardInner::Fixed(client) => client,
            PostgresClientGuardInner::Pooled(client) => client,
        });
        Self {
            inner: Some(inner),
            registration,
            lease_timer: None,
            app_permit: None,
            datalog_app: None,
        }
    }
}

impl Drop for PostgresClientGuard<'_> {
    fn drop(&mut self) {
        let cancellation_requested = self
            .registration
            .as_ref()
            .is_some_and(crate::statement_cancel::StatementRegistration::cancellation_requested);
        if let Some(inner) = self.inner.take() {
            match inner {
                // A request drain may time out and abort its query future before
                // PostgreSQL has acknowledged cancellation. Detach that lease so
                // deadpool cannot hand the still-cancelling session to a borrower.
                PostgresClientGuardInner::Pooled(client) if cancellation_requested => {
                    drop(Object::take(client));
                }
                inner => drop(inner),
            }
        }
        if let Some(timer) = self.lease_timer.take() {
            timer.finish(cancellation_requested);
        }
        if let Some((app_id, started)) = self.datalog_app.take() {
            crate::metrics::record_app_db_seconds(app_id, started.elapsed());
        }
        // Completion is visible only after the lease was returned or discarded.
        drop(self.registration.take());
    }
}

impl PostgresClientSource {
    // BatchLoader spawns its own task. Freeze the selected pool before spawning
    // instead of relying on a task-local value to cross that boundary.
    pub(crate) fn for_workload(&self, workload: DatabaseWorkload) -> Self {
        match self {
            Self::Fixed(_) => self.clone(),
            Self::Pool { pool, read_only } => Self::Pool {
                pool: pool.for_workload(workload),
                read_only: *read_only,
            },
        }
    }

    pub(crate) fn fixed(client: Client) -> Self {
        Self::Fixed(Arc::new(Mutex::new(client)))
    }

    pub(crate) const fn fixed_shared(client: Arc<Mutex<Client>>) -> Self {
        Self::Fixed(client)
    }

    pub(crate) const fn pooled(pool: PostgresPool) -> Self {
        Self::Pool {
            pool,
            read_only: false,
        }
    }

    pub(crate) const fn pooled_read_only(pool: PostgresPool) -> Self {
        Self::Pool {
            pool,
            read_only: true,
        }
    }

    pub(crate) async fn acquire(&self) -> Result<PostgresClientGuard<'_>, PostgresPoolError> {
        match self {
            Self::Fixed(client) => Ok(PostgresClientGuard::new(PostgresClientGuardInner::Fixed(
                client.lock().await,
            ))),
            Self::Pool {
                pool,
                read_only: false,
            } => pool
                .acquire()
                .await
                .map(PostgresClientGuardInner::Pooled)
                .map(|inner| {
                    let mut guard = PostgresClientGuard::new(inner);
                    guard.lease_timer = crate::metrics::SourceLeaseTimer::start(
                        pool.selected_pool(DatabaseWorkload::current()).name,
                    );
                    guard
                }),
            Self::Pool {
                pool,
                read_only: true,
            } => pool
                .acquire_read_only()
                .await
                .map(PostgresClientGuardInner::Pooled)
                .map(|inner| {
                    let mut guard = PostgresClientGuard::new(inner);
                    guard.lease_timer = crate::metrics::SourceLeaseTimer::start(
                        pool.selected_pool(DatabaseWorkload::current()).name,
                    );
                    guard
                }),
        }
    }

    fn query_telemetry(&self) -> QueryPoolTelemetry {
        match self {
            Self::Fixed(_) => QueryPoolTelemetry {
                source: "fixed",
                read_only: false,
                status: None,
            },
            Self::Pool { pool, read_only } => QueryPoolTelemetry {
                source: "pool",
                read_only: *read_only,
                status: Some(pool.status()),
            },
        }
    }

    /// Acquires the connection lease owned by one production query execution.
    /// The child span makes pool wait time and pressure visible before the
    /// existing Datalog send-query spans begin.
    #[tracing::instrument(
        name = "datalog/acquire-connection",
        skip_all,
        err,
        fields(
            app.id = %app_id,
            datalog.operation = operation,
            db.pool.source = tracing::field::Empty,
            db.pool.read_only = tracing::field::Empty,
            db.pool.max_size = tracing::field::Empty,
            db.pool.size = tracing::field::Empty,
            db.pool.available = tracing::field::Empty,
            db.pool.waiting = tracing::field::Empty,
        )
    )]
    pub(crate) async fn acquire_for_datalog(
        &self,
        app_id: uuid::Uuid,
        operation: &'static str,
    ) -> Result<PostgresClientGuard<'_>, PostgresPoolError> {
        let telemetry = self.query_telemetry();
        let span = tracing::Span::current();
        span.record("db.pool.source", telemetry.source);
        span.record("db.pool.read_only", telemetry.read_only);
        if let Some(status) = telemetry.status {
            span.record("db.pool.max_size", status.max_size);
            span.record("db.pool.size", status.size);
            span.record("db.pool.available", status.available);
            span.record("db.pool.waiting", status.waiting);
        }
        // Take this app's interactive slot before the pool lease so a hot app
        // queues on its own share instead of starving every other tenant.
        let app_permit = match self {
            Self::Pool { pool, .. } => match &pool.app_admission {
                Some(admission) => Some(
                    admission
                        .acquire(app_id)
                        .await
                        .ok_or(PostgresPoolError::AppCapacity)?,
                ),
                None => None,
            },
            Self::Fixed(_) => None,
        };
        let mut guard = self.acquire().await?;
        guard.app_permit = app_permit;
        if crate::metrics::enabled() {
            guard.datalog_app = Some((app_id, Instant::now()));
        }
        Ok(guard)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QueryPoolTelemetry {
    source: &'static str,
    read_only: bool,
    status: Option<PostgresPoolStatus>,
}

impl Deref for PostgresClientGuard<'_> {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        match self {
            Self {
                inner: Some(PostgresClientGuardInner::Fixed(client)),
                ..
            } => client,
            Self {
                inner: Some(PostgresClientGuardInner::Pooled(client)),
                ..
            } => client,
            Self { inner: None, .. } => unreachable!("Postgres client guard was already dropped"),
        }
    }
}

impl DerefMut for PostgresClientGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self {
                inner: Some(PostgresClientGuardInner::Fixed(client)),
                ..
            } => client,
            Self {
                inner: Some(PostgresClientGuardInner::Pooled(client)),
                ..
            } => client,
            Self { inner: None, .. } => unreachable!("Postgres client guard was already dropped"),
        }
    }
}

#[derive(Debug, Error)]
pub enum PostgresPoolError {
    #[error("PostgreSQL pool size must be greater than zero")]
    Empty,
    #[error("invalid PostgreSQL pool configuration")]
    Configuration(#[source] tokio_postgres::Error),
    #[error("could not build PostgreSQL pool")]
    Build(#[source] BuildError),
    #[error("could not acquire PostgreSQL connection from pool")]
    Acquire(#[source] PoolError),
    #[error("could not validate PostgreSQL pool connection")]
    Validate(#[source] tokio_postgres::Error),
    #[error("could not configure PostgreSQL connection session")]
    Session(#[source] tokio_postgres::Error),
    #[error("app interactive database capacity is exhausted")]
    AppCapacity,
}

impl PostgresPool {
    /// Partitions the existing connection budget, reserving interactive and
    /// policy capacity without increasing total primary-database connections.
    /// Budgets below eight retain the shared pool. Larger budgets leave at least
    /// six interactive slots, preserving the default five-slot pressure buffer.
    pub(crate) async fn connect_workload_partitioned(
        database_url: &str,
        max_size: usize,
    ) -> Result<Self, PostgresPoolError> {
        let Some((interactive, refresh, control)) = workload_capacities(max_size) else {
            return Self::connect(database_url, max_size).await;
        };
        let mut pool = Self::connect_named(database_url, interactive, "interactive").await?;
        pool.refresh = Some(Arc::new(
            Self::connect_named(database_url, refresh, "refresh").await?,
        ));
        pool.control = Some(Arc::new(
            Self::connect_named(database_url, control, "control").await?,
        ));
        tracing::info!(
            interactive_connections = interactive,
            refresh_connections = refresh,
            control_connections = control,
            total_connections = max_size,
            "primary database workload capacity"
        );
        Ok(pool)
    }

    pub(crate) fn control_pool(&self) -> Self {
        self.control.as_deref().unwrap_or(self).clone()
    }

    pub(crate) fn for_workload(&self, workload: DatabaseWorkload) -> Self {
        let mut selected = self.selected_pool(workload).clone();
        selected.refresh = None;
        selected.control = None;
        selected
    }

    fn selected_pool(&self, workload: DatabaseWorkload) -> &Self {
        match workload {
            DatabaseWorkload::Refresh => self.refresh.as_deref().unwrap_or(self),
            DatabaseWorkload::Interactive => self,
        }
    }

    /// Builds a pool and opens one connection immediately so invalid startup
    /// credentials, TLS, and endpoint configuration fail before serving.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe configuration, build, connection, or validation
    /// error. The database URL is never retained in the public error text.
    pub async fn connect(database_url: &str, max_size: usize) -> Result<Self, PostgresPoolError> {
        Self::connect_named(database_url, max_size, "shared").await
    }

    pub(crate) async fn connect_named(
        database_url: &str,
        max_size: usize,
        name: &'static str,
    ) -> Result<Self, PostgresPoolError> {
        let role = pool_session_role(name);
        Self::connect_with_role(database_url, max_size, name, role).await
    }

    /// A pool for long-running maintenance work (backup snapshots, restores,
    /// custodian sweeps) that keeps the server's statement timeout.
    pub(crate) async fn connect_maintenance(
        database_url: &str,
        max_size: usize,
        name: &'static str,
    ) -> Result<Self, PostgresPoolError> {
        Self::connect_with_role(database_url, max_size, name, PoolSessionRole::Maintenance).await
    }

    async fn connect_with_role(
        database_url: &str,
        max_size: usize,
        name: &'static str,
        role: PoolSessionRole,
    ) -> Result<Self, PostgresPoolError> {
        let verification = CheckoutVerification::for_pool_name(name);
        let mut pool = Self::build_with_role(database_url, max_size, role, verification)?;
        pool.name = name;
        if name != "shared" {
            pool.summary = Some(Arc::new(PoolAcquisitionSummary::new()));
        }
        let client = pool.acquire().await?;
        client
            .simple_query("SELECT 1")
            .await
            .map_err(PostgresPoolError::Validate)?;
        drop(client);
        Ok(pool)
    }

    #[cfg(test)]
    fn build(database_url: &str, max_size: usize) -> Result<Self, PostgresPoolError> {
        Self::build_named(database_url, max_size, "shared")
    }

    #[cfg(test)]
    fn build_named(
        database_url: &str,
        max_size: usize,
        name: &'static str,
    ) -> Result<Self, PostgresPoolError> {
        let mut pool = Self::build_with_role(
            database_url,
            max_size,
            pool_session_role(name),
            CheckoutVerification::for_pool_name(name),
        )?;
        pool.name = name;
        Ok(pool)
    }

    fn build_with_role(
        database_url: &str,
        max_size: usize,
        role: PoolSessionRole,
        verification: CheckoutVerification,
    ) -> Result<Self, PostgresPoolError> {
        if max_size == 0 {
            return Err(PostgresPoolError::Empty);
        }
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let postgres_config = database_url
            .parse::<tokio_postgres::Config>()
            .map_err(PostgresPoolError::Configuration)?;
        let manager = Manager::from_config(
            postgres_config,
            MakeRustlsConnect::with_webpki_roots(),
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let verify_on_recycle = verification == CheckoutVerification::EveryCheckout;
        let inner = Pool::builder(manager)
            .max_size(max_size)
            .wait_timeout(Some(POOL_WAIT_TIMEOUT))
            .create_timeout(Some(POOL_CREATE_TIMEOUT))
            .recycle_timeout(Some(POOL_RECYCLE_TIMEOUT))
            .runtime(Runtime::Tokio1)
            .post_create(Hook::async_fn(move |client, _| {
                Box::pin(async move {
                    let _ = CHECKOUT_TIMINGS.try_with(|timing| {
                        if let Some(timing) = timing {
                            crate::metrics::record_connection_creation(timing.name);
                        }
                    });
                    apply_session_settings(client, role)
                        .await
                        .map_err(HookError::Backend)?;
                    configure_checkout(client, true)
                        .await
                        .map_err(HookError::Backend)
                })
            }))
            .pre_recycle(Hook::async_fn(move |client, metrics| {
                Box::pin(async move {
                    if connection_expired(metrics.age()) {
                        return Err(HookError::message(
                            "PostgreSQL connection reached maximum lifetime",
                        ));
                    }
                    configure_checkout(client, verify_on_recycle)
                        .await
                        .map_err(HookError::Backend)
                })
            }))
            .build()
            .map_err(PostgresPoolError::Build)?;
        Ok(Self {
            inner,
            name: "shared",
            summary: None,
            refresh: None,
            control: None,
            app_admission: (role == PoolSessionRole::Interactive).then(|| {
                Arc::new(
                    crate::app_interactive_admission::AppInteractiveAdmission::new(
                        max_size,
                        POOL_WAIT_TIMEOUT,
                    ),
                )
            }),
            #[cfg(test)]
            acquisitions: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Acquires a read-write checkout. On the hot pools (see
    /// [`CheckoutVerification`]) a recycled connection whose current session
    /// mode is already read-write is delivered without another SET; every
    /// other pool re-asserts the mode on each checkout as a liveness probe.
    ///
    /// # Errors
    ///
    /// Returns when acquisition, creation, recycling, or its timeout fails.
    pub async fn acquire(&self) -> Result<Object, PostgresPoolError> {
        self.acquire_with_mode(false).await
    }

    async fn acquire_with_mode(&self, read_only: bool) -> Result<Object, PostgresPoolError> {
        let pool = self.selected_pool(DatabaseWorkload::current());
        let mut acquisition = PoolAcquisition::new(pool);
        // Every new/recycled connection is explicitly configured exactly once
        // before delivery. Deadpool drops its unready connection if this future
        // is cancelled or its hook fails, so a half-configured lease cannot leak.
        let result = CHECKOUT_TIMINGS
            .scope(CheckoutTimings::start(pool.name), async {
                let result = CHECKOUT_READ_ONLY
                    .scope(read_only, acquisition.acquire())
                    .await;
                let _ = CHECKOUT_TIMINGS.try_with(|timing| {
                    if let Some(timing) = timing {
                        timing.finish(if result.is_ok() {
                            crate::metrics::PoolTimingOutcome::Completed
                        } else {
                            crate::metrics::PoolTimingOutcome::Error
                        });
                    }
                });
                result
            })
            .await;
        acquisition.outcome = if result.is_ok() {
            PoolAcquisitionOutcome::Acquired
        } else {
            PoolAcquisitionOutcome::Error
        };
        let client = result.map_err(PostgresPoolError::Acquire)?;
        #[cfg(test)]
        pool.acquisitions.fetch_add(1, Ordering::Relaxed);
        Ok(client)
    }

    /// Acquires a checkout and enforces read-only transactions for its entire
    /// lease. A new connection and every mode switch issue one mode statement,
    /// without an intermediate read-write checkout. Later read-write borrowers
    /// reset it; unchanged modes are not re-asserted.
    ///
    /// # Errors
    ///
    /// Returns when acquisition or session configuration fails.
    pub async fn acquire_read_only(&self) -> Result<Object, PostgresPoolError> {
        self.acquire_with_mode(true).await
    }

    #[must_use]
    pub fn max_size(&self) -> usize {
        self.status().max_size
    }

    #[must_use]
    pub fn status(&self) -> PostgresPoolStatus {
        self.selected_pool(DatabaseWorkload::current())
            .physical_status()
    }

    fn physical_status(&self) -> PostgresPoolStatus {
        let status = self.inner.status();
        PostgresPoolStatus {
            max_size: status.max_size,
            size: status.size,
            available: status.available,
            waiting: status.waiting,
        }
    }

    #[cfg(test)]
    pub(crate) fn acquisition_count(&self) -> usize {
        self.acquisitions.load(Ordering::Relaxed)
    }
}

fn workload_capacities(total: usize) -> Option<(usize, usize, usize)> {
    if total < 8 {
        return None;
    }
    // Fresh per-operation policy reads share this reservation. One connection
    // at the production 16-slot budget serializes otherwise-independent apps
    // behind every network exchange. Keep the reservation proportional at larger
    // budgets too: capping it at four leaves fresh reads queued while data slots
    // are idle. Rebalance without adding connections, retaining refresh capacity
    // and at least six interactive/write slots.
    let control = (total / 4).min(total - 7);
    let refresh = ((total - control) / 2).min(total - control - 6);
    Some((total - refresh - control, refresh, control))
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_pg::QueryStatementPreparer as _;
    use tracing_subscriber::prelude::*;

    #[test]
    fn checkout_timings_separate_all_mode_attempts_from_wait_connect_time() {
        use crate::metrics::PoolTimingOutcome;
        let start = Instant::now();
        let probe = CheckoutTimings::new("interactive", start);
        probe.begin_mode(start + Duration::from_millis(10));
        probe.end_mode(start + Duration::from_millis(15), PoolTimingOutcome::Error);
        probe.begin_mode(start + Duration::from_millis(25));
        probe.end_mode(
            start + Duration::from_millis(30),
            PoolTimingOutcome::Completed,
        );
        assert_eq!(
            probe.elapsed_parts(start + Duration::from_millis(40)),
            (Duration::from_millis(30), Duration::from_millis(10))
        );
        probe.finish(PoolTimingOutcome::Completed);
        assert_eq!(
            probe.state.lock().unwrap().outcome,
            PoolTimingOutcome::Completed
        );
    }

    #[test]
    fn cancelled_or_failed_checkout_timings_keep_unfinished_work() {
        use crate::metrics::PoolTimingOutcome;
        let start = Instant::now();
        let probe = CheckoutTimings::new("refresh", start);
        assert_eq!(
            probe.state.lock().unwrap().outcome,
            PoolTimingOutcome::Cancelled
        );
        assert_eq!(
            probe.elapsed_parts(start + Duration::from_millis(20)),
            (Duration::from_millis(20), Duration::ZERO)
        );
        probe.begin_mode(start + Duration::from_millis(20));
        assert_eq!(
            probe.elapsed_parts(start + Duration::from_millis(35)),
            (Duration::from_millis(20), Duration::from_millis(15))
        );
        probe.end_mode(start + Duration::from_millis(35), PoolTimingOutcome::Error);
        probe.finish(PoolTimingOutcome::Error);
        assert_eq!(
            probe.elapsed_parts(start + Duration::from_millis(40)),
            (Duration::from_millis(25), Duration::from_millis(15))
        );
        assert_eq!(
            probe.state.lock().unwrap().outcome,
            PoolTimingOutcome::Error
        );
    }

    #[tokio::test]
    async fn joined_checkout_timing_scopes_do_not_share_state_or_modes() {
        let run = |name, read_only| {
            CHECKOUT_TIMINGS.scope(
                Some(CheckoutTimings::new(name, Instant::now())),
                async move {
                    CHECKOUT_READ_ONLY
                        .scope(read_only, async move {
                            tokio::task::yield_now().await;
                            CHECKOUT_TIMINGS.with(|probe| {
                                let probe = probe.as_ref().unwrap();
                                assert_eq!(probe.name, name);
                                assert_eq!(CHECKOUT_READ_ONLY.with(|mode| *mode), read_only);
                                probe.begin_mode(Instant::now());
                                probe.end_mode(
                                    Instant::now(),
                                    crate::metrics::PoolTimingOutcome::Completed,
                                );
                                probe.finish(crate::metrics::PoolTimingOutcome::Completed);
                            });
                        })
                        .await;
                },
            )
        };
        tokio::join!(run("interactive", false), run("refresh", true));
        assert!(CHECKOUT_TIMINGS.try_with(|_| ()).is_err());
        assert!(CHECKOUT_READ_ONLY.try_with(|_| ()).is_err());
    }

    #[tokio::test]
    async fn dropped_checkout_scope_reports_cancelled_waits_and_in_progress_mode_exchanges() {
        for enter_mode in [false, true] {
            let report = Arc::new(StdMutex::new(None));
            let mut probe = CheckoutTimings::new("control", Instant::now());
            probe.report = Some(Arc::clone(&report));
            let entered = Arc::new(tokio::sync::Notify::new());
            let signal = Arc::clone(&entered);
            let worker = tokio::spawn(CHECKOUT_TIMINGS.scope(Some(probe), async move {
                if enter_mode {
                    CHECKOUT_TIMINGS
                        .with(|probe| probe.as_ref().unwrap().begin_mode(Instant::now()));
                }
                signal.notify_one();
                std::future::pending::<()>().await;
            }));
            entered.notified().await;
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
            let observation = report
                .lock()
                .unwrap()
                .take()
                .expect("cancelled scope did not emit timings");
            assert_eq!(
                observation.outcome,
                crate::metrics::PoolTimingOutcome::Cancelled
            );
            assert_eq!(observation.unfinished_mode, enter_mode);
            assert!(!observation.outside_mode.is_zero());
        }
    }

    #[test]
    fn checkout_verification_skips_only_the_per_message_hot_pools() {
        for name in [
            "interactive",
            "refresh",
            "realtime_room_writer",
            "realtime_room_events",
            "realtime_acknowledgement",
            "rate_limit",
        ] {
            assert_eq!(
                CheckoutVerification::for_pool_name(name),
                CheckoutVerification::ModeChangeOnly,
                "{name} is on the per-message hot path"
            );
        }
        for name in [
            "shared",
            "control",
            "byop_control",
            "indexing_worker",
            "restore_worker",
            "webhook_worker",
            "custodian",
            "replication_slot_cleanup",
            "triples_size_collector",
            "wal_durable_sink",
            "sketch_aggregator_write",
        ] {
            assert_eq!(
                CheckoutVerification::for_pool_name(name),
                CheckoutVerification::EveryCheckout,
                "{name} must verify a recycled connection"
            );
        }
    }

    fn summary_test_pool() -> PostgresPool {
        let mut pool =
            PostgresPool::build("postgresql://user:secret@example.invalid/database", 1).unwrap();
        pool.name = "summary-test";
        pool.summary = Some(Arc::new(PoolAcquisitionSummary::new()));
        pool
    }

    #[tokio::test]
    async fn native_query_plans_are_connection_local_bounded_and_keep_parameters_and_rows_fresh() {
        use instant_datalog::{RenderedQuery, SqlParameter};
        let Some(url) = instant_test_support::test_database_url(
            "native_query_plans_are_connection_local_bounded_and_keep_parameters_and_rows_fresh",
        ) else {
            return;
        };
        let source = PostgresClientSource::pooled(PostgresPool::connect(&url, 2).await.unwrap());
        let a = source.acquire().await.unwrap();
        let b = source.acquire().await.unwrap();
        const SQL: &str = "SELECT $1::uuid, $2::jsonb, value FROM native_plan_probe";
        for (client, start) in [(&a, 10_i32), (&b, 20_i32)] {
            client.batch_execute(&format!("CREATE TEMP TABLE native_plan_probe(value int); INSERT INTO native_plan_probe VALUES ({start})")).await.unwrap();
            let cache = client.native_statement_cache().unwrap();
            let preparer = NativeQueryPreparer::new(client, Some(cache.clone()));
            for i in 0..8_i32 {
                let app = uuid::Uuid::new_v4();
                let payload = serde_json::json!({"revision":i});
                let query = RenderedQuery {
                    sql: SQL.to_owned(),
                    parameters: vec![SqlParameter::Uuid(app), SqlParameter::Json(payload.clone())],
                };
                let rows = instant_pg::query_with_timeout_and_preparer(
                    &**client,
                    &query,
                    Duration::from_secs(1),
                    &preparer,
                )
                .await
                .unwrap();
                assert_eq!(rows[0].get::<_, uuid::Uuid>(0), app);
                assert_eq!(
                    rows[0]
                        .get::<_, tokio_postgres::types::Json<serde_json::Value>>(1)
                        .0,
                    payload
                );
                assert_eq!(rows[0].get::<_, i32>(2), start + i);
                client
                    .execute("UPDATE native_plan_probe SET value=value+1", &[])
                    .await
                    .unwrap();
            }
            let plans = client.query_one("SELECT count(*),sum(generic_plans)::bigint FROM pg_prepared_statements WHERE statement=$1",&[&SQL]).await.unwrap();
            assert_eq!(plans.get::<_, i64>(0), 1);
            assert!(plans.get::<_, Option<i64>>(1).unwrap() > 0);
            client.batch_execute("ALTER TABLE native_plan_probe ADD COLUMN fresh text; UPDATE native_plan_probe SET value=99").await.unwrap();
            let statement = preparer.prepare(SQL).await.unwrap().unwrap();
            let app = uuid::Uuid::new_v4();
            let row = client
                .query_one(&statement, &[&app, &serde_json::json!({})])
                .await
                .unwrap();
            assert_eq!(row.get::<_, i32>(2), 99);
            for i in 0..NATIVE_STATEMENT_CACHE_ENTRIES + 3 {
                preparer
                    .prepare(&format!("SELECT $1::bigint /* native-shape-{i} */"))
                    .await
                    .unwrap();
                assert!(cache.size() <= NATIVE_STATEMENT_CACHE_ENTRIES);
            }
            let large = format!("SELECT 1 /*{}*/", "x".repeat(NATIVE_STATEMENT_SQL_BYTES));
            let count = cache.size();
            assert!(preparer.prepare(&large).await.unwrap().is_none());
            assert_eq!(
                cache.size(),
                count,
                "oversized SQL must not enter the statement cache"
            );
        }
    }

    #[tokio::test]
    async fn native_prepared_query_timeout_drains_before_connection_reuse() {
        use instant_datalog::RenderedQuery;
        let Some(url) = instant_test_support::test_database_url(
            "native_prepared_query_timeout_drains_before_connection_reuse",
        ) else {
            return;
        };
        let source = PostgresClientSource::pooled(PostgresPool::connect(&url, 1).await.unwrap());
        let client = source.acquire().await.unwrap();
        let preparer = NativeQueryPreparer::new(&client, client.native_statement_cache());
        let slow = RenderedQuery {
            sql: "SELECT pg_sleep(10)".to_owned(),
            parameters: vec![],
        };
        let result = instant_pg::query_with_timeout_and_preparer(
            &*client,
            &slow,
            Duration::from_millis(20),
            &preparer,
        )
        .await;
        assert!(matches!(
            result,
            Err(instant_pg::PgQueryError::QueryTimeout { .. })
        ));
        assert_eq!(
            client
                .query_one("SELECT 42", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            42
        );
    }

    #[tokio::test]
    async fn native_prepared_transaction_preserves_read_only_and_fixed_client_fallback() {
        use instant_datalog::{RenderedQuery, SqlParameter};
        let Some(url) = instant_test_support::test_database_url(
            "native_prepared_transaction_preserves_read_only_and_fixed_client_fallback",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let source = PostgresClientSource::pooled_read_only(pool.clone());
        let mut client = source.acquire().await.unwrap();
        let cache = client.native_statement_cache();
        let transaction = client
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .unwrap();
        let preparer = NativeQueryPreparer::new(transaction.client(), cache);
        let query = RenderedQuery {
            sql: "SELECT current_setting('transaction_read_only'),$1::bigint".to_owned(),
            parameters: vec![SqlParameter::Integer(7)],
        };
        let rows = instant_pg::query_with_timeout_and_preparer(
            &transaction,
            &query,
            Duration::from_secs(1),
            &preparer,
        )
        .await
        .unwrap();
        assert_eq!(rows[0].get::<_, String>(0), "on");
        assert_eq!(rows[0].get::<_, i64>(1), 7);
        let write = RenderedQuery {
            sql: "CREATE TABLE forbidden_native_plan_write(id int)".to_owned(),
            parameters: vec![],
        };
        assert!(
            instant_pg::query_with_timeout_and_preparer(
                &transaction,
                &write,
                Duration::from_secs(1),
                &preparer
            )
            .await
            .is_err()
        );
        transaction.rollback().await.unwrap();
        drop(client);
        let client = pool.acquire().await.unwrap();
        assert_eq!(
            client
                .query_one("SHOW default_transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "off"
        );
        let fixed = NativeQueryPreparer::new(&client, None);
        assert!(fixed.prepare("SELECT 1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn native_statement_preparation_is_inside_the_query_deadline() {
        use instant_datalog::RenderedQuery;
        let Some(url) = instant_test_support::test_database_url(
            "native_statement_preparation_is_inside_the_query_deadline",
        ) else {
            return;
        };
        let source = PostgresClientSource::pooled(PostgresPool::connect(&url, 1).await.unwrap());
        let client = source.acquire().await.unwrap();
        let table = format!("native_prepare_deadline_{}", uuid::Uuid::new_v4().simple());
        client
            .batch_execute(&format!("CREATE TABLE {table}(id int)"))
            .await
            .unwrap();
        let (locker, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        locker
            .batch_execute(&format!(
                "BEGIN; LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"
            ))
            .await
            .unwrap();
        let preparer = NativeQueryPreparer::new(&client, client.native_statement_cache());
        let query = RenderedQuery {
            sql: format!("SELECT id FROM {table}"),
            parameters: vec![],
        };
        let result = instant_pg::query_with_timeout_and_preparer(
            &*client,
            &query,
            Duration::from_millis(20),
            &preparer,
        )
        .await;
        assert!(matches!(
            result,
            Err(instant_pg::PgQueryError::QueryTimeout { .. })
        ));
        locker.batch_execute("ROLLBACK").await.unwrap();
        drop(locker);
        driver.await.unwrap().unwrap();
        assert_eq!(
            client
                .query_one("SELECT 42", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            42
        );
        client
            .batch_execute(&format!("DROP TABLE {table}"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn typed_prepared_plans_reuse_sql_not_parameter_values_or_results() {
        use tokio_postgres::types::Type;
        let Some(url) = instant_test_support::test_database_url(
            "typed_prepared_plans_reuse_sql_not_parameter_values_or_results",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let source = PostgresClientSource::pooled(pool);
        let client = source.acquire().await.unwrap();
        client.batch_execute("CREATE TEMP TABLE typed_plan_probe (id int PRIMARY KEY, value int); INSERT INTO typed_plan_probe VALUES (1,10),(2,20)").await.unwrap();
        const SQL: &str = "SELECT value FROM typed_plan_probe WHERE id = $1";
        for i in 0..12 {
            let id = if i % 2 == 0 { 1_i32 } else { 2_i32 };
            let value = client
                .query_typed_cached_one(SQL, &[(&id, Type::INT4)])
                .await
                .unwrap()
                .get::<_, i32>(0);
            assert_eq!(value, if id == 1 { 10 + i / 2 } else { 20 + i / 2 });
            client
                .execute(
                    "UPDATE typed_plan_probe SET value=value+1 WHERE id=$1",
                    &[&id],
                )
                .await
                .unwrap();
        }
        let prepared = client.query_one("SELECT count(*), sum(generic_plans)::bigint FROM pg_prepared_statements WHERE statement=$1", &[&SQL]).await.unwrap();
        assert_eq!(prepared.get::<_, i64>(0), 1);
        assert!(prepared.get::<_, Option<i64>>(1).unwrap() > 0);
        drop(client);
        let client = source.acquire().await.unwrap();
        assert_eq!(
            client
                .query_typed_cached_one(SQL, &[(&1_i32, Type::INT4)])
                .await
                .unwrap()
                .get::<_, i32>(0),
            16
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM pg_prepared_statements WHERE statement=$1",
                    &[&SQL]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
    }

    #[tokio::test]
    async fn typed_prepared_plans_are_connection_local_and_parameter_type_scoped() {
        use tokio_postgres::types::Type;
        let Some(url) = instant_test_support::test_database_url(
            "typed_prepared_plans_are_connection_local_and_parameter_type_scoped",
        ) else {
            return;
        };
        let source = PostgresClientSource::pooled(PostgresPool::connect(&url, 2).await.unwrap());
        let a = source.acquire().await.unwrap();
        let b = source.acquire().await.unwrap();
        const SQL: &str = "SELECT pg_backend_pid(), $1::bigint";
        let first = a
            .query_typed_cached_one(SQL, &[(&7_i32, Type::INT4)])
            .await
            .unwrap();
        let second = b
            .query_typed_cached_one(SQL, &[(&9_i32, Type::INT4)])
            .await
            .unwrap();
        assert_ne!(first.get::<_, i32>(0), second.get::<_, i32>(0));
        assert_eq!(first.get::<_, i64>(1), 7);
        assert_eq!(second.get::<_, i64>(1), 9);
        assert_eq!(
            a.query_typed_cached_one(SQL, &[(&11_i64, Type::INT8)])
                .await
                .unwrap()
                .get::<_, i64>(1),
            11
        );
        assert_eq!(
            a.query_one(
                "SELECT count(*) FROM pg_prepared_statements WHERE statement=$1",
                &[&SQL]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
            2
        );
        assert_eq!(
            b.query_one(
                "SELECT count(*) FROM pg_prepared_statements WHERE statement=$1",
                &[&SQL]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
            1
        );
    }

    #[tokio::test]
    async fn typed_prepared_queries_preserve_row_cardinality_and_fixed_client_support() {
        use tokio_postgres::types::Type;
        let Some(url) = instant_test_support::test_database_url(
            "typed_prepared_queries_preserve_row_cardinality_and_fixed_client_support",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let source = PostgresClientSource::fixed(client);
        let client = source.acquire().await.unwrap();
        const SQL: &str = "SELECT generate_series(1,$1::int)";
        assert_eq!(
            client
                .query_typed_cached(SQL, &[(&2_i32, Type::INT4)])
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            client
                .query_typed_cached_opt(SQL, &[(&0_i32, Type::INT4)])
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            client
                .query_typed_cached_one(SQL, &[(&0_i32, Type::INT4)])
                .await
                .is_err()
        );
        assert!(
            client
                .query_typed_cached_opt(SQL, &[(&2_i32, Type::INT4)])
                .await
                .is_err()
        );
        assert_eq!(
            client
                .query_typed_cached_one(SQL, &[(&1_i32, Type::INT4)])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );
        drop(client);
        drop(source);
        driver.await.unwrap().unwrap();
    }

    fn summary_test_subscriber() -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::registry().with(tracing_subscriber::filter::Targets::new().with_target(
            "instant_server::database_pool_summary",
            tracing::Level::DEBUG,
        ))
    }

    #[test]
    fn acquisition_summary_stays_dormant_when_disabled_or_pool_is_unnamed() {
        let pool = summary_test_pool();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(tracing_subscriber::filter::LevelFilter::OFF),
            || {
                let acquisition = PoolAcquisition::new(&pool);
                assert!(acquisition.started.is_none());
                drop(acquisition);
            },
        );
        assert!(
            pool.summary
                .as_ref()
                .unwrap()
                .interval
                .lock()
                .unwrap()
                .is_none()
        );
        let unnamed =
            PostgresPool::build("postgresql://user:secret@example.invalid/database", 1).unwrap();
        tracing::subscriber::with_default(summary_test_subscriber(), || {
            let acquisition = PoolAcquisition::new(&unnamed);
            assert!(acquisition.started.is_none());
            assert!(!acquisition.summarize);
        });
    }

    #[test]
    fn acquisition_summary_records_cancelled_waits_on_drop_and_shares_pinned_handles() {
        let refresh = summary_test_pool();
        let summary = refresh.summary.as_ref().unwrap().clone();
        let mut root = summary_test_pool();
        root.refresh = Some(Arc::new(refresh));
        let pinned = root.for_workload(DatabaseWorkload::Refresh);
        let cloned = pinned.clone();
        assert!(Arc::ptr_eq(&summary, cloned.summary.as_ref().unwrap()));
        assert!(!Arc::ptr_eq(&summary, root.summary.as_ref().unwrap()));
        tracing::subscriber::with_default(summary_test_subscriber(), || {
            drop(PoolAcquisition::new(&pinned));
            let mut acquisition = PoolAcquisition::new(&cloned);
            acquisition.outcome = PoolAcquisitionOutcome::Acquired;
            drop(acquisition);
            let mut acquisition = PoolAcquisition::new(&cloned);
            acquisition.outcome = PoolAcquisitionOutcome::Error;
        });
        let state = summary.interval.lock().unwrap();
        let interval = state.as_ref().unwrap();
        assert_eq!(interval.acquired.count, 1);
        assert_eq!(interval.cancelled.count, 1);
        assert_eq!(interval.error.count, 1);
    }

    #[test]
    fn acquisition_summary_histogram_covers_exact_boundaries_and_overflow() {
        let mut summary = PoolWaitSummary::default();
        for micros in [0, 100, 101, 250, 251, 64_000_000, 65_000_000] {
            summary.record(Duration::from_micros(micros));
        }
        assert_eq!(summary.count, 7);
        assert!(summary.min_ms.abs() < f64::EPSILON);
        assert!((summary.max_ms - 65_000.0).abs() < f64::EPSILON);
        assert!((summary.sum_ms - 129_000.702).abs() < 0.000_001);
        assert_eq!(
            summary.buckets,
            [
                2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1
            ]
        );
        let encoded = format!("{:?}", summary.buckets);
        let decoded: Vec<u64> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, summary.buckets);
    }

    #[test]
    fn acquisition_summary_drains_only_after_ten_seconds_with_contiguous_intervals() {
        let summary = PoolAcquisitionSummary::new();
        let start = Instant::now();
        summary.begin_at(start, 3, || 1_000_000);
        summary.begin_at(start + Duration::from_secs(1), 9, || {
            panic!("the wall clock should only be read when collection starts")
        });
        assert!(
            summary
                .record(
                    start + Duration::from_secs(9),
                    Duration::from_millis(4),
                    PoolAcquisitionOutcome::Acquired,
                    2,
                )
                .is_none()
        );
        let first = summary
            .record(
                start + Duration::from_secs(10),
                Duration::from_secs(2),
                PoolAcquisitionOutcome::Cancelled,
                5,
            )
            .unwrap();
        assert_eq!(first.interval.started_unix_ms, 1_000_000);
        assert_eq!(first.ended_unix_ms, 1_010_000);
        assert_eq!(first.elapsed, Duration::from_secs(10));
        assert_eq!(first.interval.waiting_max, 9);
        assert_eq!(first.interval.acquired.count, 1);
        assert_eq!(first.interval.cancelled.count, 1);
        assert_eq!(first.interval.error.count, 0);
        assert!(
            summary
                .record(
                    start + Duration::from_secs(10),
                    Duration::from_millis(1),
                    PoolAcquisitionOutcome::Acquired,
                    0,
                )
                .is_none()
        );
        let second = summary
            .record(
                start + Duration::from_secs(20),
                Duration::from_millis(10),
                PoolAcquisitionOutcome::Error,
                0,
            )
            .unwrap();
        assert_eq!(second.interval.started_unix_ms, first.ended_unix_ms);
        assert_eq!(second.ended_unix_ms, 1_020_000);
        assert_eq!(second.interval.acquired.count, 1);
        assert_eq!(second.interval.cancelled.count, 0);
        assert_eq!(second.interval.error.count, 1);
    }

    #[test]
    fn acquisition_summary_preserves_submillisecond_time_across_rollovers() {
        let summary = PoolAcquisitionSummary::new();
        let start = Instant::now();
        let unix_start = 1_789_130_000_000;
        summary.begin_at(start, 0, || unix_start);
        // Forty minutes of ten-second intervals, each ending just before the
        // next millisecond. Truncating every interval independently loses 239
        // milliseconds and can move load observations across a hold boundary.
        let interval_duration = Duration::from_micros(10_000_999);
        let mut previous_end = unix_start;
        for index in 1..=240 {
            let snapshot = summary
                .record(
                    start + interval_duration * index,
                    Duration::from_millis(1),
                    PoolAcquisitionOutcome::Acquired,
                    0,
                )
                .unwrap();
            assert_eq!(snapshot.interval.started_unix_ms, previous_end);
            assert_eq!(snapshot.elapsed, interval_duration);
            previous_end = snapshot.ended_unix_ms;
        }
        assert_eq!(
            previous_end,
            unix_start + u64::try_from((interval_duration * 240).as_millis()).unwrap(),
            "interval timestamps must retain the original clock anchor without accumulating rounding loss"
        );
    }

    #[test]
    fn acquisition_summary_concurrent_completions_do_not_duplicate_or_lose_intervals() {
        let summary = Arc::new(PoolAcquisitionSummary::new());
        let start = Instant::now();
        summary.begin_at(start, 0, || 0);
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let tasks = (0..16)
            .map(|_| {
                let summary = summary.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    summary.record(
                        start + POOL_SUMMARY_INTERVAL,
                        Duration::from_millis(1),
                        PoolAcquisitionOutcome::Acquired,
                        0,
                    )
                })
            })
            .collect::<Vec<_>>();
        let snapshots = tasks
            .into_iter()
            .filter_map(|task| task.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(snapshots.len(), 1);
        let remainder = summary
            .record(
                start + POOL_SUMMARY_INTERVAL * 2,
                Duration::from_millis(1),
                PoolAcquisitionOutcome::Acquired,
                0,
            )
            .unwrap();
        assert_eq!(
            snapshots[0].interval.acquired.count + remainder.interval.acquired.count,
            17
        );
        assert_eq!(
            remainder.interval.started_unix_ms,
            snapshots[0].ended_unix_ms
        );
    }

    #[test]
    fn acquisition_summary_emits_json_wire_contract() {
        #[derive(Clone)]
        struct Output(Arc<StdMutex<Vec<u8>>>);
        impl std::io::Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Output(Arc::new(StdMutex::new(Vec::new())));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        let summary = PoolAcquisitionSummary::new();
        let start = Instant::now();
        summary.begin_at(start, 0, || 1_789_130_000_000);
        let interval = summary
            .record(
                start + Duration::from_secs(10),
                Duration::from_millis(1500),
                PoolAcquisitionOutcome::Cancelled,
                3,
            )
            .unwrap();
        tracing::subscriber::with_default(subscriber, || {
            interval.emit(
                "refresh",
                summary.id,
                PostgresPoolStatus {
                    max_size: 31,
                    size: 31,
                    available: 0,
                    waiting: 3,
                },
            );
        });
        let bytes = output.0.lock().unwrap();
        let event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let fields = &event["fields"];
        assert_eq!(fields["message"], "database connection acquisition summary");
        assert_eq!(fields["cancelled_count"], 1);
        assert_eq!(fields["cancelled_wait_ms_sum"], 1500.0);
        let bins: Vec<u64> =
            serde_json::from_str(fields["cancelled_wait_bucket_counts"].as_str().unwrap()).unwrap();
        assert_eq!(bins.iter().sum::<u64>(), 1);
        let edges: Vec<f64> =
            serde_json::from_str(fields["wait_bucket_upper_bounds_ms"].as_str().unwrap()).unwrap();
        assert_eq!(edges, POOL_WAIT_BUCKET_UPPER_BOUNDS_MS);
        // This secret-free line lets the Python collector validate the actual
        // tracing JSON wire format in the local validation command.
        println!(
            "POOL_SUMMARY_FIXTURE {}",
            String::from_utf8_lossy(&bytes).trim()
        );
    }

    #[tokio::test]
    async fn acquisition_summary_counts_real_pool_wait_cancellation_once() {
        let Some(database_url) = instant_test_support::test_database_url(
            "acquisition_summary_counts_real_pool_wait_cancellation_once",
        ) else {
            return;
        };
        // Attach after startup and the held checkout so the aggregate contains
        // exactly the acquisition future cancelled below.
        let mut pool = PostgresPool::connect(&database_url, 1).await.unwrap();
        let held = pool.acquire().await.unwrap();
        pool.name = "summary-test";
        pool.summary = Some(Arc::new(PoolAcquisitionSummary::new()));
        tracing::subscriber::with_default(summary_test_subscriber(), || {
            let mut pending = Box::pin(pool.acquire());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(pending.as_mut().poll(&mut context).is_pending());
            assert_eq!(pool.status().waiting, 1);
            drop(pending);
        });
        {
            let state = pool.summary.as_ref().unwrap().interval.lock().unwrap();
            let interval = state.as_ref().unwrap();
            assert_eq!(interval.cancelled.count, 1);
            assert_eq!(interval.acquired.count, 0);
            assert_eq!(interval.error.count, 0);
            assert_eq!(interval.waiting_max, 1);
        }
        drop(held);
        assert!(pool.acquire().await.is_ok());
    }

    #[tokio::test]
    async fn saturated_refresh_pool_preserves_write_and_policy_capacity() {
        let Some(database_url) = instant_test_support::test_database_url(
            "saturated_refresh_pool_preserves_write_and_policy_capacity",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&database_url, 8)
            .await
            .unwrap();
        let control = pool.control_pool();
        let mut setup = pool.acquire().await.unwrap();
        crate::migrate_database(&mut setup).await.unwrap();
        drop(setup);
        let refresh_leases = DatabaseWorkload::Refresh
            .scope(async {
                let mut leases = Vec::new();
                for _ in 0..pool.max_size() {
                    leases.push(pool.acquire().await.unwrap());
                }
                leases
            })
            .await;
        let write = tokio::time::timeout(Duration::from_millis(200), pool.acquire())
            .await
            .expect("refresh saturation blocked an interactive write")
            .unwrap();
        write
            .batch_execute(
                "CREATE TEMP TABLE workload_write (id int); INSERT INTO workload_write VALUES (1)",
            )
            .await
            .unwrap();
        let policy = tokio::time::timeout(Duration::from_millis(200), control.acquire())
            .await
            .expect("refresh saturation blocked policy refresh")
            .unwrap();
        assert_eq!(
            policy
                .query_one("SELECT 1::int", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );
        drop(policy);
        tokio::time::timeout(
            Duration::from_millis(900),
            crate::PostgresHttpResponsePolicy::load_pool(control.clone()),
        )
        .await
        .expect("real HTTP policy refresh missed its deadline under refresh saturation")
        .unwrap();
        assert_eq!(
            refresh_leases.len() + pool.max_size() + control.max_size(),
            8
        );
    }

    #[tokio::test]
    async fn detached_read_only_source_keeps_its_refresh_reservation() {
        let Some(database_url) = instant_test_support::test_database_url(
            "detached_read_only_source_keeps_its_refresh_reservation",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&database_url, 8)
            .await
            .unwrap();
        let mut interactive_leases = Vec::new();
        for _ in 0..pool.max_size() {
            interactive_leases.push(pool.acquire().await.unwrap());
        }
        let source = PostgresClientSource::pooled_read_only(pool.clone())
            .for_workload(DatabaseWorkload::Refresh);
        let task = tokio::spawn(async move {
            assert_eq!(DatabaseWorkload::current(), DatabaseWorkload::Interactive);
            let client = source.acquire().await.unwrap();
            client
                .query_one("SHOW default_transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0)
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(900), task)
                .await
                .expect("detached refresh leaked into saturated interactive pool")
                .unwrap(),
            "on"
        );
        let refresh = pool.for_workload(DatabaseWorkload::Refresh);
        let client = refresh.acquire().await.unwrap();
        assert_eq!(
            client
                .query_one("SHOW default_transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "off"
        );
    }

    #[test]
    fn session_settings_take_environment_overrides_and_ignore_blank_or_invalid_values() {
        let defaults = PostgresSessionSettings::default();
        assert_eq!(
            defaults,
            PostgresSessionSettings {
                statement_timeout_interactive: Duration::from_secs(15),
                statement_timeout_background: Duration::from_secs(30),
                lock_wait: Duration::from_secs(5),
                idle_transaction_wait: Duration::from_secs(60),
            }
        );
        assert_eq!(PostgresSessionSettings::from_lookup(|_| None), defaults);
        assert_eq!(
            PostgresSessionSettings::from_lookup(|_| Some("   ".to_owned())),
            defaults
        );
        assert_eq!(
            PostgresSessionSettings::from_lookup(|_| Some("soon".to_owned())),
            defaults
        );
        assert_eq!(
            PostgresSessionSettings::from_lookup(|name| match name {
                "INSTANT_PG_STATEMENT_TIMEOUT_INTERACTIVE_MS" => Some("1000".to_owned()),
                "INSTANT_PG_STATEMENT_TIMEOUT_BACKGROUND_MS" => Some("2000".to_owned()),
                "INSTANT_PG_LOCK_TIMEOUT_MS" => Some("250".to_owned()),
                "INSTANT_PG_IDLE_IN_TX_TIMEOUT_MS" => Some("5000".to_owned()),
                _ => None,
            }),
            PostgresSessionSettings {
                statement_timeout_interactive: Duration::from_millis(1000),
                statement_timeout_background: Duration::from_millis(2000),
                lock_wait: Duration::from_millis(250),
                idle_transaction_wait: Duration::from_millis(5000),
            }
        );
    }

    #[test]
    fn session_settings_follow_the_pool_role_and_leave_maintenance_unbounded() {
        for (name, role) in [
            ("interactive", PoolSessionRole::Interactive),
            ("shared", PoolSessionRole::Interactive),
            ("refresh", PoolSessionRole::Background),
            ("control", PoolSessionRole::Background),
            ("webhook_worker", PoolSessionRole::Background),
            ("custodian", PoolSessionRole::Maintenance),
            ("backup_snapshot", PoolSessionRole::Maintenance),
            ("restore_worker", PoolSessionRole::Maintenance),
            ("migrate", PoolSessionRole::Background),
        ] {
            assert_eq!(pool_session_role(name), role, "role for {name}");
        }
        let settings = PostgresSessionSettings::default();
        let interactive = settings.apply_sql(PoolSessionRole::Interactive);
        assert!(interactive.contains("SET statement_timeout = '15000'"));
        assert!(interactive.contains("SET lock_timeout = '5000'"));
        assert!(interactive.contains("SET idle_in_transaction_session_timeout = '60000'"));
        let background = settings.apply_sql(PoolSessionRole::Background);
        assert!(background.contains("SET statement_timeout = '30000'"));
        let maintenance = settings.apply_sql(PoolSessionRole::Maintenance);
        assert!(!maintenance.contains("statement_timeout"));
        assert!(!maintenance.contains("lock_timeout"));
        assert!(maintenance.contains("SET idle_in_transaction_session_timeout = '60000'"));
    }

    async fn show_setting(client: &Client, name: &str) -> String {
        client
            .query_one(&format!("SHOW {name}"), &[])
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    async fn pooled_connections_apply_role_timeouts_at_creation() {
        let Some(database_url) = instant_test_support::test_database_url(
            "pooled_connections_apply_role_timeouts_at_creation",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&database_url, 8)
            .await
            .unwrap();
        let interactive = pool.acquire().await.unwrap();
        assert_eq!(show_setting(&interactive, "statement_timeout").await, "15s");
        assert_eq!(show_setting(&interactive, "lock_timeout").await, "5s");
        assert_eq!(
            show_setting(&interactive, "idle_in_transaction_session_timeout").await,
            "1min"
        );
        let refresh = pool
            .for_workload(DatabaseWorkload::Refresh)
            .acquire()
            .await
            .unwrap();
        assert_eq!(show_setting(&refresh, "statement_timeout").await, "30s");
        let control = pool.control_pool().acquire().await.unwrap();
        assert_eq!(show_setting(&control, "statement_timeout").await, "30s");
        // Maintenance keeps the server default so backups and restores are not
        // cancelled halfway through a large stream.
        let maintenance = PostgresPool::connect_maintenance(&database_url, 1, "custodian")
            .await
            .unwrap()
            .acquire()
            .await
            .unwrap();
        assert_eq!(show_setting(&maintenance, "statement_timeout").await, "0");
        assert_eq!(show_setting(&maintenance, "lock_timeout").await, "0");
        assert_eq!(
            show_setting(&maintenance, "idle_in_transaction_session_timeout").await,
            "1min"
        );
    }

    #[tokio::test]
    async fn one_app_cannot_starve_another_apps_interactive_queries() {
        let Some(database_url) = instant_test_support::test_database_url(
            "one_app_cannot_starve_another_apps_interactive_queries",
        ) else {
            return;
        };
        // Eight slots partition into six interactive connections, so one app
        // keeps four permits and two connections remain for everyone else.
        let pool = PostgresPool::connect_workload_partitioned(&database_url, 8)
            .await
            .unwrap();
        let source = PostgresClientSource::pooled(pool);
        let hot_app = uuid::Uuid::new_v4();
        let other_app = uuid::Uuid::new_v4();
        let mut hot = Vec::new();
        for _ in 0..4 {
            hot.push(source.acquire_for_datalog(hot_app, "test").await.unwrap());
        }
        let started = Instant::now();
        // The hot app's slow queries and the other app's fresh query overlap.
        let (slow, other) = tokio::join!(
            futures_util::future::join_all(
                hot.iter()
                    .map(|client| client.simple_query("SELECT pg_sleep(0.2)"))
            ),
            source.acquire_for_datalog(other_app, "test"),
        );
        for result in slow {
            result.unwrap();
        }
        let client = other.unwrap();
        assert_eq!(
            client
                .query_one("SELECT 1::int", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a saturated app delayed another app's query for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn workload_partitions_preserve_total_budget_and_default_write_headroom() {
        for total in 0..8 {
            assert_eq!(workload_capacities(total), None);
        }
        for total in 8..=400 {
            let (interactive, refresh, control) = workload_capacities(total).unwrap();
            assert_eq!(interactive + refresh + control, total);
            assert!(interactive > 5);
            assert!(refresh > 0 && control > 0 && control <= total / 4);
        }
        assert_eq!(workload_capacities(8), Some((6, 1, 1)));
        assert_eq!(workload_capacities(48), Some((18, 18, 12)));
        assert_eq!(workload_capacities(64), Some((24, 24, 16)));
    }

    #[test]
    fn sixteen_slot_budget_reserves_parallel_policy_capacity_without_more_connections() {
        assert_eq!(workload_capacities(16), Some((6, 6, 4)));
    }

    #[tokio::test]
    async fn larger_policy_reservation_preserves_independent_data_headroom() {
        let Some(database_url) = instant_test_support::test_database_url(
            "larger_policy_reservation_preserves_independent_data_headroom",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&database_url, 48)
            .await
            .unwrap();
        let control = pool.control_pool();
        let refresh = pool.for_workload(DatabaseWorkload::Refresh);
        assert_eq!(
            (pool.max_size(), refresh.max_size(), control.max_size()),
            (18, 18, 12)
        );
        let mut held = Vec::new();
        for _ in 0..control.max_size() {
            held.push(control.acquire().await.unwrap());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), control.acquire())
                .await
                .is_err(),
            "larger policy reservation must still be bounded"
        );
        for data_pool in [&pool, &refresh] {
            let data = tokio::time::timeout(Duration::from_millis(200), data_pool.acquire())
                .await
                .expect("policy saturation blocked an independent data reservation")
                .unwrap();
            assert_eq!(
                data.query_one("SELECT 1::int", &[])
                    .await
                    .unwrap()
                    .get::<_, i32>(0),
                1
            );
        }
        drop(held);
        assert!(control.acquire().await.is_ok());
    }

    #[tokio::test]
    async fn one_blocked_policy_lease_does_not_serialize_other_apps() {
        let Some(database_url) = instant_test_support::test_database_url(
            "one_blocked_policy_lease_does_not_serialize_other_apps",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&database_url, 16)
            .await
            .unwrap();
        let control = pool.control_pool();
        let held = control.acquire().await.unwrap();
        let other_app = tokio::time::timeout(Duration::from_millis(200), control.acquire())
            .await
            .expect("one app's policy lease blocked every other app")
            .unwrap();
        let third_app = control.acquire().await.unwrap();
        let fourth_app = control.acquire().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), control.acquire())
                .await
                .is_err(),
            "policy reservation must remain bounded"
        );
        assert_eq!(
            other_app
                .query_one("SELECT 1::int", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );
        assert_eq!(
            pool.max_size()
                + control.max_size()
                + DatabaseWorkload::Refresh
                    .scope(async { pool.max_size() })
                    .await,
            16
        );
        drop((held, other_app, third_app, fourth_app));
    }

    #[test]
    fn pool_rejects_zero_size_without_exposing_database_url() {
        let secret_url = "postgresql://user:very-secret@example.invalid/database";
        let error = PostgresPool::build(secret_url, 0).err().unwrap();
        assert_eq!(
            error.to_string(),
            "PostgreSQL pool size must be greater than zero"
        );
        assert!(!error.to_string().contains("very-secret"));
    }

    #[test]
    fn pool_retires_connections_at_the_jvm_ten_minute_boundary() {
        assert!(!connection_expired(Duration::from_secs(599)));
        assert!(connection_expired(Duration::from_secs(600)));
        assert!(connection_expired(Duration::from_secs(601)));
    }

    #[test]
    fn pool_preserves_configured_capacity_and_bounded_waits() {
        let pool =
            PostgresPool::build("postgresql://user:secret@example.invalid/database", 37).unwrap();
        assert_eq!(pool.max_size(), 37);
        assert_eq!(
            pool.status(),
            PostgresPoolStatus {
                max_size: 37,
                size: 0,
                available: 0,
                waiting: 0,
            }
        );
        let timeouts = pool.inner.timeouts();
        assert_eq!(timeouts.wait, Some(POOL_WAIT_TIMEOUT));
        assert_eq!(timeouts.create, Some(POOL_CREATE_TIMEOUT));
        assert_eq!(timeouts.recycle, Some(POOL_RECYCLE_TIMEOUT));
        let debug = format!("{:?}", PostgresClientSource::pooled(pool));
        assert!(debug.contains("pool"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn datalog_telemetry_distinguishes_primary_and_read_only_pool_sources() {
        let pool =
            PostgresPool::build("postgresql://user:secret@example.invalid/database", 37).unwrap();
        let status = pool.status();
        assert_eq!(
            PostgresClientSource::pooled(pool.clone()).query_telemetry(),
            QueryPoolTelemetry {
                source: "pool",
                read_only: false,
                status: Some(status),
            }
        );
        assert_eq!(
            PostgresClientSource::pooled_read_only(pool).query_telemetry(),
            QueryPoolTelemetry {
                source: "pool",
                read_only: true,
                status: Some(status),
            }
        );
    }

    #[tokio::test]
    async fn pool_is_bounded_and_read_only_leases_are_reset() {
        let Some(database_url) = instant_test_support::test_database_url(
            "pool_is_bounded_and_read_only_leases_are_reset",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
        assert_eq!(pool.max_size(), 2);

        let read_only = pool.acquire_read_only().await.unwrap();
        let setting = read_only
            .query_one("SHOW default_transaction_read_only", &[])
            .await
            .unwrap()
            .get::<_, String>(0);
        assert_eq!(setting, "on");
        drop(read_only);

        let read_write = pool.acquire().await.unwrap();
        let setting = read_write
            .query_one("SHOW default_transaction_read_only", &[])
            .await
            .unwrap()
            .get::<_, String>(0);
        assert_eq!(setting, "off");

        drop(read_write);
        let source = PostgresClientSource::pooled(pool.clone());
        let available_before = pool.status().available;
        let lease = source
            .acquire_for_datalog(uuid::Uuid::nil(), "test-query")
            .await
            .unwrap();
        assert_eq!(pool.status().available, available_before.saturating_sub(1));
        drop(lease);
        assert_eq!(pool.status().available, available_before);
    }

    #[tokio::test]
    async fn checkout_modes_cover_new_connections_concurrent_futures_and_mode_switches() {
        let Some(url) = instant_test_support::test_database_url(
            "checkout_modes_cover_new_connections_concurrent_futures_and_mode_switches",
        ) else {
            return;
        };
        let pool = PostgresPool::build(&url, 2).unwrap();
        for _ in 0..3 {
            // Join on one task, not separate task-local contexts. Both initial
            // connections and recycled connections must receive their own mode.
            let (reader, writer) =
                tokio::try_join!(pool.acquire_read_only(), pool.acquire()).unwrap();
            let mode = |client: Object| async move {
                let setting: String = client
                    .query_one("SHOW default_transaction_read_only", &[])
                    .await
                    .unwrap()
                    .get(0);
                (client, setting)
            };
            let ((reader, read_mode), (writer, write_mode)) =
                tokio::join!(mode(reader), mode(writer));
            assert_eq!(read_mode, "on");
            assert_eq!(write_mode, "off");
            drop(reader);
            drop(writer);
        }
        // Reusing the same physical connections for both modes must re-assert
        // the requested setting on every switch, whichever slot is handed out.
        for _ in 0..3 {
            let reader = pool.acquire_read_only().await.unwrap();
            assert_eq!(
                reader
                    .query_one("SHOW default_transaction_read_only", &[])
                    .await
                    .unwrap()
                    .get::<_, String>(0),
                "on"
            );
            drop(reader);
            let writer = pool.acquire().await.unwrap();
            assert_eq!(
                writer
                    .query_one("SHOW default_transaction_read_only", &[])
                    .await
                    .unwrap()
                    .get::<_, String>(0),
                "off"
            );
            drop(writer);
        }
        assert_eq!(pool.status().size, 2);
        assert_eq!(pool.status().available, 2);
    }

    #[test]
    fn production_code_never_sets_transaction_read_only_guc() {
        fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read source directory") {
                let path = entry.expect("source entry").path();
                if path.is_dir() {
                    scan(&path, offenders);
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                // The pool owns the one legitimate session-mode statement.
                if name == "postgres_pool.rs" || name.ends_with("_tests.rs") {
                    continue;
                }
                if !name.ends_with(".rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read source file");
                if source.contains("SET default_transaction_read_only") {
                    offenders.push(path.display().to_string());
                }
            }
        }
        let mut offenders = Vec::new();
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../");
        for entry in std::fs::read_dir(&crates).expect("read crates directory") {
            let source = entry.expect("crate entry").path().join("src");
            if source.is_dir() {
                scan(&source, &mut offenders);
            }
        }
        assert!(
            offenders.is_empty(),
            "production code must not change the session read-only mode outside the pool: {offenders:?}"
        );
    }

    #[tokio::test]
    async fn checkout_sends_at_most_one_mode_statement_and_skips_unchanged_modes() {
        let Some(url) = instant_test_support::test_database_url(
            "checkout_sends_at_most_one_mode_statement_and_skips_unchanged_modes",
        ) else {
            return;
        };
        if std::env::var("TEST_DATABASE_STATEMENT_COUNTS").as_deref() != Ok("1") {
            return;
        }
        let (observer, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let count = || async {
            observer.query_one(
                "SELECT coalesce(sum(calls),0)::bigint FROM pg_stat_statements WHERE query LIKE 'SET default_transaction_read_only = %'", &[]
            ).await.unwrap().get::<_, i64>(0)
        };
        let pool = PostgresPool::build_named(&url, 1, "interactive").unwrap();
        let mut total = 0;
        for _ in 0..3 {
            let before = count().await;
            let reader = pool.acquire_read_only().await.unwrap();
            let delta = count().await - before;
            assert!(
                (0..=1).contains(&delta),
                "a checkout must issue at most one mode statement, issued {delta}"
            );
            total += delta;
            assert_eq!(
                reader
                    .query_one("SHOW default_transaction_read_only", &[])
                    .await
                    .unwrap()
                    .get::<_, String>(0),
                "on"
            );
            drop(reader);
        }
        assert_eq!(
            total, 1,
            "one new connection needs one mode statement; unchanged modes are skipped"
        );
        // A mode switch must re-assert on the same recycled connection.
        let before = count().await;
        let writer = pool.acquire().await.unwrap();
        assert_eq!(count().await - before, 1);
        assert_eq!(
            writer
                .query_one("SHOW default_transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "off"
        );
        drop(writer);
        drop(observer);
        driver.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn verifying_pool_sends_one_mode_statement_on_every_checkout() {
        let Some(url) = instant_test_support::test_database_url(
            "verifying_pool_sends_one_mode_statement_on_every_checkout",
        ) else {
            return;
        };
        if std::env::var("TEST_DATABASE_STATEMENT_COUNTS").as_deref() != Ok("1") {
            return;
        }
        let (observer, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let count = || async {
            observer.query_one(
                "SELECT coalesce(sum(calls),0)::bigint FROM pg_stat_statements WHERE query LIKE 'SET default_transaction_read_only = %'", &[]
            ).await.unwrap().get::<_, i64>(0)
        };
        let pool = PostgresPool::build_named(&url, 1, "control").unwrap();
        for _ in 0..3 {
            let before = count().await;
            let reader = pool.acquire_read_only().await.unwrap();
            assert_eq!(
                count().await - before,
                1,
                "a verifying pool must re-assert the mode on every checkout"
            );
            assert_eq!(
                reader
                    .query_one("SHOW default_transaction_read_only", &[])
                    .await
                    .unwrap()
                    .get::<_, String>(0),
                "on"
            );
            drop(reader);
        }
        let before = count().await;
        let writer = pool.acquire().await.unwrap();
        assert_eq!(count().await - before, 1);
        assert_eq!(
            writer
                .query_one("SHOW default_transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "off"
        );
        drop(writer);
        drop(observer);
        driver.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn hot_pool_discards_a_closed_connection_after_a_connection_level_error() {
        let Some(url) = instant_test_support::test_database_url(
            "hot_pool_discards_a_closed_connection_after_a_connection_level_error",
        ) else {
            return;
        };
        let pool = PostgresPool::build_named(&url, 1, "interactive").unwrap();
        let doomed = pool.acquire().await.unwrap();
        let doomed_pid = doomed
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        drop(doomed);
        let (observer, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        observer
            .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
            .await
            .unwrap();
        // Wait until the backend is really gone so the next query on the pooled
        // connection must fail instead of racing the termination signal.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let alive = observer
                    .query_one(
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                        &[&doomed_pid],
                    )
                    .await
                    .unwrap()
                    .get::<_, bool>(0);
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the terminated backend did not exit");
        // The hot policy keeps the unchanged-mode skip, so this lease may still
        // be the terminated backend and fail with a closed connection. That one
        // failure is accepted; the driver observes the FATAL asynchronously, so
        // wait for it to report the client closed before returning the lease.
        let first = pool.acquire().await.unwrap();
        if first.query_one("SELECT 1", &[]).await.is_err() {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !first.is_closed() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("a failed connection must eventually report itself closed");
        }
        drop(first);
        let replacement = pool.acquire().await.unwrap();
        let replacement_pid = replacement
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert_ne!(
            replacement_pid, doomed_pid,
            "the checkout after a connection-level error must not reuse the terminated backend"
        );
        // A statement error is not a connection error: the lease stays usable.
        assert!(replacement.query_one("SELECT 1/0", &[]).await.is_err());
        drop(replacement);
        let reused = pool.acquire().await.unwrap();
        assert_eq!(
            reused
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            replacement_pid,
            "a statement error must not discard the connection"
        );
        drop(reused);
        drop(observer);
        driver.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn failed_or_cancelled_mode_hooks_discard_unready_connections() {
        let Some(url) = instant_test_support::test_database_url(
            "failed_or_cancelled_mode_hooks_discard_unready_connections",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let client = pool.acquire().await.unwrap();
        let failed_pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        assert!(client.batch_execute("BEGIN; SELECT 1/0").await.is_err());
        drop(client);
        let client = pool.acquire_read_only().await.unwrap();
        let cancelled_pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        assert_ne!(
            failed_pid, cancelled_pid,
            "a failed mode SET must discard the aborted transaction"
        );
        // Leave a server-side query queued so the next mode switch cannot
        // finish. Dropping its future/lease here is deliberate test-only
        // poisoning. A same-mode checkout would skip the SET, so request the
        // other mode to exercise the hook itself.
        {
            let sleep = client.simple_query("SELECT pg_sleep(10)");
            tokio::pin!(sleep);
            std::future::poll_fn(|cx| {
                assert!(sleep.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }
        drop(client);
        {
            let checkout = pool.acquire();
            tokio::pin!(checkout);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), checkout.as_mut())
                    .await
                    .is_err()
            );
        }
        let client = pool.acquire().await.unwrap();
        let row = client
            .query_one(
                "SELECT pg_backend_pid(),current_setting('default_transaction_read_only')",
                &[],
            )
            .await
            .unwrap();
        assert_ne!(
            row.get::<_, i32>(0),
            cancelled_pid,
            "cancelling the in-flight hook must not return a half-configured connection to the pool"
        );
        assert_eq!(row.get::<_, String>(1), "off");
    }

    #[tokio::test]
    async fn aborted_query_scope_discards_connection_before_late_cancel() {
        let Some(database_url) = instant_test_support::test_database_url(
            "aborted_query_scope_discards_connection_before_late_cancel",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
        let source = PostgresClientSource::pooled(pool.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            crate::statement_cancel::StatementTracker::default()
                .scope(async move {
                    let client = source.acquire().await.unwrap();
                    let pid: i32 = client
                        .query_one("SELECT pg_backend_pid()", &[])
                        .await
                        .unwrap()
                        .get(0);
                    let rendered = instant_datalog::RenderedQuery {
                        sql: "SELECT pg_sleep(10)".to_owned(),
                        parameters: Vec::new(),
                    };
                    let query = instant_pg::query_with_timeout(
                        &*client,
                        &rendered,
                        Duration::from_secs(30),
                    );
                    tokio::pin!(query);
                    // Poll once so the real cancellation guard and query exist.
                    std::future::poll_fn(|cx| {
                        assert!(std::future::Future::poll(query.as_mut(), cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    started_tx.send(pid).unwrap();
                    query.await
                })
                .await
        });
        let original_pid = started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let replacement = pool.acquire().await.unwrap();
        let replacement_pid: i32 = replacement
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        assert_ne!(
            original_pid, replacement_pid,
            "aborting a query scope must discard its lease before the asynchronous cancel can reach another borrower"
        );
    }

    #[tokio::test]
    async fn canceled_tracked_lease_is_detached_before_the_pool_is_reused() {
        let Some(database_url) = instant_test_support::test_database_url(
            "canceled_tracked_lease_is_detached_before_the_pool_is_reused",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
        let source = PostgresClientSource::pooled(pool.clone());
        let tracker = crate::statement_cancel::StatementTracker::default();
        let scoped_tracker = tracker.clone();
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let query_task = tokio::spawn(async move {
            scoped_tracker
                .scope(async move {
                    let client = source.acquire().await.unwrap();
                    let pid = client
                        .query_one("SELECT pg_backend_pid()", &[])
                        .await
                        .unwrap()
                        .get::<_, i32>(0);
                    pid_tx.send(pid).unwrap();
                    client
                        .simple_query("SELECT pg_sleep(10) /* statement_cancel_core */")
                        .await
                })
                .await
        });
        let canceled_pid = pid_rx.await.unwrap();

        let (observer, observer_connection) =
            tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
                .await
                .unwrap();
        let observer_driver = tokio::spawn(observer_connection);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let is_active = observer
                    .query_one(
                        "SELECT EXISTS (
                           SELECT 1 FROM pg_stat_activity
                           WHERE pid = $1 AND state = 'active'
                             AND query LIKE '%statement_cancel_core%'
                         )",
                        &[&canceled_pid],
                    )
                    .await
                    .unwrap()
                    .get::<_, bool>(0);
                if is_active {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tracked statement did not become active");

        let outcome = tracker.cancel_and_wait(Duration::from_secs(2)).await;
        assert_eq!(
            outcome,
            crate::statement_cancel::CancelWaitOutcome {
                requested: 1,
                completed: 1,
                timed_out: false,
            }
        );
        let error = tokio::time::timeout(Duration::from_secs(1), query_task)
            .await
            .expect("canceled statement handler did not finish")
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        );
        assert!(matches!(
            instant_pg::PgQueryError::from(error),
            instant_pg::PgQueryError::QueryCancelled
        ));

        let replacement = tokio::time::timeout(Duration::from_secs(2), pool.acquire())
            .await
            .expect("replacement pool checkout timed out")
            .unwrap();
        let replacement_pid = replacement
            .query_one("SELECT pg_backend_pid(), 1::int", &[])
            .await
            .unwrap();
        assert_ne!(replacement_pid.get::<_, i32>(0), canceled_pid);
        assert_eq!(replacement_pid.get::<_, i32>(1), 1);

        drop(observer);
        observer_driver.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn statement_timeout_is_not_a_retryable_query_cancellation() {
        let Some(database_url) = instant_test_support::test_database_url(
            "statement_timeout_is_not_a_retryable_query_cancellation",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
        let client = pool.acquire().await.unwrap();
        client
            .batch_execute("SET statement_timeout = '10ms'")
            .await
            .unwrap();
        let error = client
            .simple_query("SELECT pg_sleep(0.1)")
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        );
        assert!(matches!(
            instant_pg::PgQueryError::from(error),
            instant_pg::PgQueryError::Postgres(_)
        ));
    }
}
