//! Private Prometheus exposition. Labels are a fixed vocabulary, never request data.
//!
//! The one exception is the `app` label. It is attached only after an app id
//! resolved to an existing app, its value is an operator-chosen slug or an id
//! prefix, and at most [`APP_LABEL_LIMIT`] apps get a label per process. Every
//! other app shares `app="other"`, so a request can never add unbounded series.
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use axum::{Router, routing::get};
use instant_realtime::{SDK_LABELS, SDK_VERSION_OTHER};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::EmailProvider;
use crate::runtime_realtime::RuntimeRealtimeMetrics;

const METRICS_ALLOW_NONLOOPBACK_ENV: &str = "INSTANT_METRICS_ALLOW_NONLOOPBACK";
const BUCKETS_US: [u64; 12] = [
    5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 750_000, 1_000_000, 2_000_000,
    3_000_000, 10_000_000,
];
const GROUPS: [&str; 6] = [
    "runtime",
    "admin",
    "dashboard",
    "platform",
    "health",
    "other",
];

struct RequestMetrics {
    counts: [AtomicU64; 6],
    duration_us: AtomicU64,
    buckets: [AtomicU64; 13],
}

impl RequestMetrics {
    const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; 6],
            duration_us: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; 13],
        }
    }
}

static REQUESTS: [RequestMetrics; 6] = [const { RequestMetrics::new() }; 6];
static ENABLED: AtomicBool = AtomicBool::new(false);
static PANICS: AtomicU64 = AtomicU64::new(0);
static POOL_WAITING: AtomicU64 = AtomicU64::new(0);
static POOL_WAITS: RequestMetrics = RequestMetrics::new();
const POOL_ROLES: [&str; 5] = ["interactive", "refresh", "control", "failover", "other"];
static ROLE_WAITING: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static ROLE_WAITS: [RequestMetrics; 5] = [const { RequestMetrics::new() }; 5];
const CHECKOUT_OUTCOMES: [&str; 3] = ["completed", "error", "cancelled"];
const ACQUIRE_OUTCOMES: [&str; 3] = ["acquired", "error", "cancelled"];
const LEASE_DISPOSITIONS: [&str; 2] = ["returned", "discarded"];
static CHECKOUT_MODE: [[RequestMetrics; 3]; 5] =
    [const { [const { RequestMetrics::new() }; 3] }; 5];
static WAIT_CONNECT: [[RequestMetrics; 3]; 5] = [const { [const { RequestMetrics::new() }; 3] }; 5];
static SOURCE_LEASE: [[RequestMetrics; 2]; 5] = [const { [const { RequestMetrics::new() }; 2] }; 5];
static CONNECTION_CREATIONS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const EMAIL_PROVIDERS: [&str; 5] = ["auto", "postmark", "sendgrid", "resend", "cloudflare"];
const EMAIL_OUTCOMES: [&str; 3] = ["sent", "failed", "rate_limited"];
static EMAILS: [[AtomicU64; 3]; 5] = [const { [const { AtomicU64::new(0) }; 3] }; 5];
const WEBHOOK_DELIVERY_OUTCOMES: [&str; 4] = ["delivered", "failed", "retry_exhausted", "rejected"];
static WEBHOOK_DELIVERIES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
const STORAGE_OPS: [&str; 4] = ["upload", "download_url", "delete", "list"];
const STORAGE_OP_OUTCOMES: [&str; 3] = ["ok", "error", "rejected"];
static STORAGE_OP_RUNS: [[AtomicU64; 3]; 4] = [const { [const { AtomicU64::new(0) }; 3] }; 4];
/// Every supervised scheduled task name. Names are a fixed, trusted vocabulary;
/// anything else is reported as `other`.
const TASKS: [&str; 26] = [
    "backup-pool-refresh",
    "backup-recovery",
    "custodian",
    "custodian-reaper",
    "daily-backup",
    "daily-metrics",
    "dashboard-identity-policy-refresh",
    "ephemeral-app-sweep",
    "hard-deletion-sweep",
    "history-truncator",
    "http-response-policy-refresh",
    "indexing-recovery",
    "native-gauges",
    "oauth-metadata-refresh",
    "process-test-worker",
    "rate-limit-key-sweep",
    "replication-slot-cleanup",
    "restore-recovery",
    "sender-domain-recheck",
    "storage-sweep",
    "triples-size-collection",
    "wal-log-truncator",
    "webhook-pending",
    "webhook-retries",
    "webhook-stuck-recovery",
    "other",
];
const TASK_OUTCOMES: [&str; 4] = ["success", "failure", "timeout", "panic"];
static TASK_RUNS: [[AtomicU64; 4]; TASKS.len()] =
    [const { [const { AtomicU64::new(0) }; 4] }; TASKS.len()];
static TASK_LAST_SUCCESS: [AtomicU64; TASKS.len()] = [const { AtomicU64::new(0) }; TASKS.len()];
/// Realtime protocol operations. Names are a fixed, trusted vocabulary;
/// unknown operations are reported as `other`.
const RUNTIME_OPS: [&str; 16] = [
    "init",
    "add-query",
    "remove-query",
    "transact",
    "refresh",
    "start-sync",
    "remove-sync",
    "join-room",
    "leave-room",
    "set-presence",
    "client-broadcast",
    "start-stream",
    "append-stream",
    "subscribe-stream",
    "unsubscribe-stream",
    "other",
];
const RUNTIME_OP_OUTCOMES: [&str; 3] = ["ok", "error", "rate_limited"];
static RUNTIME_OP_RUNS: [[AtomicU64; 3]; RUNTIME_OPS.len()] =
    [const { [const { AtomicU64::new(0) }; 3] }; RUNTIME_OPS.len()];
static RUNTIME_OP_DURATIONS: [RequestMetrics; RUNTIME_OPS.len()] =
    [const { RequestMetrics::new() }; RUNTIME_OPS.len()];
/// How many apps may carry their own `app` label. Later apps share `other`.
pub(crate) const APP_LABEL_LIMIT: usize = 100;
const APP_LABEL_OTHER: usize = APP_LABEL_LIMIT;
const APP_LABEL_SLOTS: usize = APP_LABEL_LIMIT + 1;
const APP_SLUG_BYTES: usize = 32;
/// Error surfaces carrying an `app` label. Fixed vocabulary, like the groups.
const APP_ERROR_SURFACES: [&str; 4] = ["runtime", "admin", "storage", "webhook"];

/// One app's error surface. Kept as an enum so a typo cannot silently drop a
/// per-app error count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppErrorSurface {
    Runtime,
    Admin,
    Storage,
    Webhook,
}

impl AppErrorSurface {
    const fn index(self) -> usize {
        match self {
            Self::Runtime => 0,
            Self::Admin => 1,
            Self::Storage => 2,
            Self::Webhook => 3,
        }
    }
}

#[derive(Default)]
struct AppLabelState {
    /// App ids in label order. Index is the slot; length is at most the limit.
    slots: Vec<Uuid>,
    /// Every resolved app id, including overflow apps mapped to `other`.
    index: HashMap<Uuid, usize>,
    /// Operator-chosen slugs from the `app-metric-slugs` config flag.
    slugs: HashMap<Uuid, String>,
}

struct AppCounters {
    ops: [[[AtomicU64; 3]; RUNTIME_OPS.len()]; APP_LABEL_SLOTS],
    db_micros: [AtomicU64; APP_LABEL_SLOTS],
    errors: [[AtomicU64; APP_ERROR_SURFACES.len()]; APP_LABEL_SLOTS],
}

impl AppCounters {
    fn new() -> Self {
        Self {
            ops: std::array::from_fn(|_| {
                std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0)))
            }),
            db_micros: std::array::from_fn(|_| AtomicU64::new(0)),
            errors: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

/// Per-app counters with bounded labels. Recording an app id that never
/// resolved is a no-op, so unknown ids cannot create series.
struct AppMetrics {
    labels: Mutex<AppLabelState>,
    counters: Box<AppCounters>,
}

static APP_METRICS: OnceLock<AppMetrics> = OnceLock::new();

fn app_metrics() -> &'static AppMetrics {
    APP_METRICS.get_or_init(AppMetrics::new)
}

impl AppMetrics {
    fn new() -> Self {
        Self {
            labels: Mutex::new(AppLabelState::default()),
            counters: Box::new(AppCounters::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AppLabelState> {
        self.labels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claims this app's label slot, or the shared overflow bucket. Only call
    /// this after the id resolved to an existing app.
    fn register(&self, app_id: Uuid) {
        let mut labels = self.lock();
        if labels.index.contains_key(&app_id) {
            return;
        }
        let slot = if labels.slots.len() < APP_LABEL_LIMIT {
            let slot = labels.slots.len();
            labels.slots.push(app_id);
            slot
        } else {
            APP_LABEL_OTHER
        };
        labels.index.insert(app_id, slot);
    }

    fn slot(&self, app_id: Uuid) -> Option<usize> {
        self.lock().index.get(&app_id).copied()
    }

    fn set_slugs(&self, value: &Value) {
        let slugs = parse_app_metric_slugs(value);
        self.lock().slugs = slugs;
    }

    fn record_op(&self, app_id: Uuid, op: Option<&str>, outcome: RuntimeOpOutcome) {
        let Some(slot) = self.slot(app_id) else {
            return;
        };
        self.counters.ops[slot][runtime_op_index(op)][outcome.index()]
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_db_seconds(&self, app_id: Uuid, elapsed: Duration) {
        let Some(slot) = self.slot(app_id) else {
            return;
        };
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.counters.db_micros[slot].fetch_add(micros, Ordering::Relaxed);
    }

    fn record_error(&self, app_id: Uuid, surface: AppErrorSurface) {
        let Some(slot) = self.slot(app_id) else {
            return;
        };
        self.counters.errors[slot][surface.index()].fetch_add(1, Ordering::Relaxed);
    }
}

/// Parses `{"<app-id>": "<slug>"}`. Anything malformed, unsafe to expose as a
/// label value, or colliding with the overflow bucket is ignored.
fn parse_app_metric_slugs(value: &Value) -> HashMap<Uuid, String> {
    let Some(entries) = value.as_object() else {
        return HashMap::new();
    };
    entries
        .iter()
        .filter_map(|(app_id, slug)| {
            let app_id = Uuid::parse_str(app_id).ok()?;
            let slug = slug.as_str()?;
            valid_app_slug(slug).then(|| (app_id, slug.to_owned()))
        })
        .collect()
}

fn valid_app_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= APP_SLUG_BYTES
        && slug != "other"
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn short_app_id(app_id: Uuid) -> String {
    app_id.simple().to_string()[..8].to_owned()
}

fn app_label(labels: &AppLabelState, slot: usize) -> String {
    if slot >= APP_LABEL_LIMIT {
        return "other".to_owned();
    }
    let app_id = labels.slots[slot];
    labels
        .slugs
        .get(&app_id)
        .cloned()
        .unwrap_or_else(|| short_app_id(app_id))
}

const SLOT_PURPOSES: [&str; 2] = ["invalidator", "aggregator"];
static SLOT_RECREATIONS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static WAL_SINK_REFRESH_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static WAL_SINK_FORCED_RESYNCS: AtomicU64 = AtomicU64::new(0);
static INVALIDATION_STREAM_RESTARTS: AtomicU64 = AtomicU64::new(0);
static INVALIDATION_HEALTH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplicationSlotPurpose {
    Invalidator,
    Aggregator,
}

impl ReplicationSlotPurpose {
    const fn index(self) -> usize {
        match self {
            Self::Invalidator => 0,
            Self::Aggregator => 1,
        }
    }
}

/// Counts slots that were invalidated or lost and rebuilt without a restart.
pub(crate) fn record_replication_slot_recreated(purpose: ReplicationSlotPurpose) {
    SLOT_RECREATIONS[purpose.index()].fetch_add(1, Ordering::Relaxed);
}

/// Counts WAL window refreshes abandoned at the per-refresh deadline.
pub(crate) fn record_wal_sink_refresh_timeout() {
    WAL_SINK_REFRESH_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn wal_sink_refresh_timeouts() -> u64 {
    WAL_SINK_REFRESH_TIMEOUTS.load(Ordering::Relaxed)
}

/// Counts session refreshes forced after their previous refresh timed out.
pub(crate) fn record_wal_sink_forced_resync() {
    WAL_SINK_FORCED_RESYNCS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn wal_sink_forced_resyncs() -> u64 {
    WAL_SINK_FORCED_RESYNCS.load(Ordering::Relaxed)
}

/// Counts invalidation stream producer restarts after transport failures.
pub(crate) fn record_invalidation_stream_restart() {
    INVALIDATION_STREAM_RESTARTS.fetch_add(1, Ordering::Relaxed);
}

/// Publishes whether this process's invalidation stream currently reports
/// healthy readiness.
pub(crate) fn set_invalidation_health(healthy: bool) {
    INVALIDATION_HEALTH.store(u64::from(healthy), Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmailOutcome {
    Sent,
    Failed,
    RateLimited,
}

impl EmailOutcome {
    const fn index(self) -> usize {
        match self {
            Self::Sent => 0,
            Self::Failed => 1,
            Self::RateLimited => 2,
        }
    }
}

fn email_provider_index(provider: EmailProvider) -> usize {
    match provider {
        EmailProvider::Auto => 0,
        EmailProvider::Postmark => 1,
        EmailProvider::SendGrid => 2,
        EmailProvider::Resend => 3,
        EmailProvider::Cloudflare => 4,
    }
}

/// Counts process panics observed by the installed panic hook.
pub(crate) fn record_panic() {
    PANICS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn panics() -> u64 {
    PANICS.load(Ordering::Relaxed)
}

/// Counts outbound email attempts using only the fixed provider/outcome
/// vocabulary, never recipient data.
pub(crate) fn record_email(provider: EmailProvider, outcome: EmailOutcome) {
    EMAILS[email_provider_index(provider)][outcome.index()].fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WebhookDeliveryOutcome {
    Delivered,
    Failed,
    RetryExhausted,
    Rejected,
}

impl WebhookDeliveryOutcome {
    const fn index(self) -> usize {
        match self {
            Self::Delivered => 0,
            Self::Failed => 1,
            Self::RetryExhausted => 2,
            Self::Rejected => 3,
        }
    }
}

/// Counts webhook delivery attempts by their fixed terminal or retry state.
pub(crate) fn record_webhook_delivery(outcome: WebhookDeliveryOutcome) {
    WEBHOOK_DELIVERIES[outcome.index()].fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageOp {
    Upload,
    DownloadUrl,
    Delete,
    List,
}

impl StorageOp {
    const fn index(self) -> usize {
        match self {
            Self::Upload => 0,
            Self::DownloadUrl => 1,
            Self::Delete => 2,
            Self::List => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageOpOutcome {
    Ok,
    Error,
    Rejected,
}

impl StorageOpOutcome {
    const fn index(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::Error => 1,
            Self::Rejected => 2,
        }
    }
}

/// Counts storage API operations using only the fixed op/outcome vocabulary.
pub(crate) fn record_storage_op(op: StorageOp, outcome: StorageOpOutcome) {
    STORAGE_OP_RUNS[op.index()][outcome.index()].fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskOutcome {
    Success,
    Failure,
    Timeout,
    Panic,
}

impl TaskOutcome {
    const fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Timeout => 2,
            Self::Panic => 3,
        }
    }
}

fn task_index(task: &str) -> usize {
    TASKS
        .iter()
        .position(|name| *name == task)
        .unwrap_or(TASKS.len() - 1)
}

fn runtime_op_index(op: Option<&str>) -> usize {
    match op {
        Some("init") => 0,
        Some("add-query") => 1,
        Some("remove-query") => 2,
        Some("transact") => 3,
        // The sync-table operations are the client-facing refresh path.
        Some("refresh" | "refresh-sync-table" | "resync-table") => 4,
        Some("start-sync") => 5,
        Some("remove-sync") => 6,
        Some("join-room") => 7,
        Some("leave-room") => 8,
        Some("set-presence") => 9,
        Some("client-broadcast") => 10,
        Some("start-stream") => 11,
        Some("append-stream") => 12,
        Some("subscribe-stream") => 13,
        Some("unsubscribe-stream") => 14,
        _ => RUNTIME_OPS.len() - 1,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeOpOutcome {
    Ok,
    Error,
    RateLimited,
}

impl RuntimeOpOutcome {
    const fn index(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::Error => 1,
            Self::RateLimited => 2,
        }
    }
}

// Per-thread observation seam for tests. Realtime test runtimes are
// current-thread, so a test sees exactly the operations its own service
// produced without racing the process-global counters.
#[cfg(test)]
thread_local! {
    static RUNTIME_OP_TEST_RECORDS: std::cell::RefCell<Vec<(Option<String>, RuntimeOpOutcome)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn clear_runtime_op_test_records() {
    RUNTIME_OP_TEST_RECORDS.with(|records| records.borrow_mut().clear());
}

#[cfg(test)]
pub(crate) fn runtime_op_test_records() -> Vec<(Option<String>, RuntimeOpOutcome)> {
    RUNTIME_OP_TEST_RECORDS.with(|records| records.borrow().clone())
}

/// Counts one dispatched realtime operation and records its handling time.
pub(crate) fn record_runtime_op(op: Option<&str>, outcome: RuntimeOpOutcome, elapsed: Duration) {
    #[cfg(test)]
    RUNTIME_OP_TEST_RECORDS.with(|records| {
        records.borrow_mut().push((op.map(str::to_owned), outcome));
    });
    let index = runtime_op_index(op);
    RUNTIME_OP_RUNS[index][outcome.index()].fetch_add(1, Ordering::Relaxed);
    record_duration(&RUNTIME_OP_DURATIONS[index], elapsed);
}

/// Records that an app id resolved to an existing app. Only registered apps
/// can create per-app series; every later app shares `app="other"`.
pub(crate) fn register_app(app_id: Uuid) {
    app_metrics().register(app_id);
}

/// Counts one realtime operation for a resolved app.
pub(crate) fn record_app_op(app_id: Uuid, op: Option<&str>, outcome: RuntimeOpOutcome) {
    app_metrics().record_op(app_id, op, outcome);
}

/// Adds the time one resolved app's request held a database connection.
pub(crate) fn record_app_db_seconds(app_id: Uuid, elapsed: Duration) {
    app_metrics().record_db_seconds(app_id, elapsed);
}

/// Counts one failed request for a resolved app on a fixed surface.
pub(crate) fn record_app_error(app_id: Uuid, surface: AppErrorSurface) {
    app_metrics().record_error(app_id, surface);
}

/// Installs the operator's `app-metric-slugs` map for this process.
pub(crate) fn set_app_metric_slugs(value: &Value) {
    app_metrics().set_slugs(value);
}

/// Counts one finished scheduled-task run and records the success timestamp.
pub(crate) fn record_task_run(task: &str, outcome: TaskOutcome) {
    let index = task_index(task);
    TASK_RUNS[index][outcome.index()].fetch_add(1, Ordering::Relaxed);
    if outcome == TaskOutcome::Success {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        TASK_LAST_SUCCESS[index].store(now, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PoolTimingOutcome {
    Completed,
    Error,
    Cancelled,
}

impl PoolTimingOutcome {
    const fn index(self) -> usize {
        match self {
            Self::Completed => 0,
            Self::Error => 1,
            Self::Cancelled => 2,
        }
    }
}

fn record_duration(metric: &RequestMetrics, elapsed: Duration) {
    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    metric.duration_us.fetch_add(micros, Ordering::Relaxed);
    let bucket = BUCKETS_US.partition_point(|bound| *bound < micros);
    metric.buckets[bucket].fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_checkout_mode(name: &str, elapsed: Duration, outcome: PoolTimingOutcome) {
    record_duration(&CHECKOUT_MODE[pool_role(name)][outcome.index()], elapsed);
}

pub(crate) fn record_wait_connect(name: &str, elapsed: Duration, outcome: PoolTimingOutcome) {
    record_duration(&WAIT_CONNECT[pool_role(name)][outcome.index()], elapsed);
}

pub(crate) fn record_connection_creation(name: &str) {
    CONNECTION_CREATIONS[pool_role(name)].fetch_add(1, Ordering::Relaxed);
}

/// Covers source-owned pooled guards only, not raw deadpool `Object` callers.
pub(crate) struct SourceLeaseTimer {
    started: std::time::Instant,
    role: usize,
}

impl SourceLeaseTimer {
    pub(crate) fn start(name: &str) -> Option<Self> {
        enabled().then(|| Self {
            started: std::time::Instant::now(),
            role: pool_role(name),
        })
    }

    pub(crate) fn finish(self, discarded: bool) {
        record_duration(
            &SOURCE_LEASE[self.role][usize::from(discarded)],
            self.started.elapsed(),
        );
    }
}

fn render_pool_duration<const N: usize>(
    out: &mut String,
    name: &str,
    role: &str,
    dimension: &str,
    values: &[&str; N],
    metrics: &[RequestMetrics; N],
) {
    for (value, metric) in values.iter().zip(metrics) {
        let bins = metric
            .buckets
            .each_ref()
            .map(|bucket| bucket.load(Ordering::Relaxed));
        let count: u64 = bins.iter().sum();
        let mut cumulative = 0;
        for (limit, bucket) in BUCKETS_US.iter().zip(&bins) {
            cumulative += bucket;
            let _ = writeln!(
                out,
                "{name}_bucket{{role=\"{role}\",{dimension}=\"{value}\",le=\"{}\"}} {cumulative}",
                Duration::from_micros(*limit).as_secs_f64()
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{role=\"{role}\",{dimension}=\"{value}\",le=\"+Inf\"}} {count}\n{name}_count{{role=\"{role}\",{dimension}=\"{value}\"}} {count}\n{name}_sum{{role=\"{role}\",{dimension}=\"{value}\"}} {}",
            Duration::from_micros(metric.duration_us.load(Ordering::Relaxed)).as_secs_f64()
        );
    }
}

fn render_pool_stages(out: &mut String) {
    out.push_str("# HELP backendrelay_pool_checkout_mode_seconds Each session-mode SET attempt, including failed and cancelled attempts.\n# HELP backendrelay_pool_wait_connect_seconds Acquisition residual excluding all session-mode attempts; includes queueing, connection creation and recycling.\n# HELP backendrelay_pool_source_lease_seconds Ready pooled source-guard lifetime until returned or discarded; raw Object callers are not covered.\n# HELP backendrelay_pool_connection_creations_total Physical creations reaching post-create, not necessarily ready checkouts.\n");
    for name in [
        "backendrelay_pool_checkout_mode_seconds",
        "backendrelay_pool_wait_connect_seconds",
        "backendrelay_pool_source_lease_seconds",
    ] {
        let _ = writeln!(out, "# TYPE {name} histogram");
    }
    out.push_str("# TYPE backendrelay_pool_connection_creations_total counter\n");
    for (index, role) in POOL_ROLES.iter().enumerate() {
        render_pool_duration(
            out,
            "backendrelay_pool_checkout_mode_seconds",
            role,
            "outcome",
            &CHECKOUT_OUTCOMES,
            &CHECKOUT_MODE[index],
        );
        render_pool_duration(
            out,
            "backendrelay_pool_wait_connect_seconds",
            role,
            "outcome",
            &ACQUIRE_OUTCOMES,
            &WAIT_CONNECT[index],
        );
        render_pool_duration(
            out,
            "backendrelay_pool_source_lease_seconds",
            role,
            "disposition",
            &LEASE_DISPOSITIONS,
            &SOURCE_LEASE[index],
        );
        let _ = writeln!(
            out,
            "backendrelay_pool_connection_creations_total{{role=\"{role}\"}} {}",
            CONNECTION_CREATIONS[index].load(Ordering::Relaxed)
        );
    }
}

fn pool_role(name: &str) -> usize {
    match name {
        "interactive" => 0,
        "refresh" => 1,
        "control" => 2,
        "failover" | "failover_gate" => 3,
        _ => 4,
    }
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub(crate) struct PoolWait(std::time::Instant, usize);
impl PoolWait {
    pub(crate) fn start(name: &str) -> Option<Self> {
        if !enabled() {
            return None;
        }
        POOL_WAITING.fetch_add(1, Ordering::Relaxed);
        let role = pool_role(name);
        ROLE_WAITING[role].fetch_add(1, Ordering::Relaxed);
        Some(Self(std::time::Instant::now(), role))
    }
}
impl Drop for PoolWait {
    fn drop(&mut self) {
        POOL_WAITING.fetch_sub(1, Ordering::Relaxed);
        ROLE_WAITING[self.1].fetch_sub(1, Ordering::Relaxed);
        let micros = u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX);
        POOL_WAITS.counts[0].fetch_add(1, Ordering::Relaxed);
        POOL_WAITS.duration_us.fetch_add(micros, Ordering::Relaxed);
        let bucket = BUCKETS_US
            .iter()
            .position(|limit| micros <= *limit)
            .unwrap_or(BUCKETS_US.len());
        POOL_WAITS.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        let role = &ROLE_WAITS[self.1];
        role.counts[0].fetch_add(1, Ordering::Relaxed);
        role.duration_us.fetch_add(micros, Ordering::Relaxed);
        role.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }
}

/// Human-readable name of a [`request_group`] index, for logs.
pub(crate) fn request_group_name(group: usize) -> &'static str {
    GROUPS.get(group).copied().unwrap_or("other")
}

pub(crate) fn request_group(path: &str) -> usize {
    match path.split('/').nth(1).unwrap_or("") {
        "runtime" => 0,
        "admin" => 1,
        "dash" => 2,
        "platform" => 3,
        "health" => 4,
        _ => 5,
    }
}

pub(crate) fn record_request(group: usize, status: u16, elapsed: Duration) {
    let metric = &REQUESTS[group];
    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    metric.counts[usize::from(status / 100).min(5)].fetch_add(1, Ordering::Relaxed);
    metric.duration_us.fetch_add(micros, Ordering::Relaxed);
    let bucket = BUCKETS_US
        .iter()
        .position(|limit| micros <= *limit)
        .unwrap_or(BUCKETS_US.len());
    metric.buckets[bucket].fetch_add(1, Ordering::Relaxed);
}

/// Metrics bind to loopback by default. A container overlay may opt in to a
/// private or unspecified address so another container can scrape it; public
/// addresses stay rejected.
pub fn parse_address(value: &str) -> Result<SocketAddr, std::io::Error> {
    parse_address_with(value, allow_nonloopback())
}

fn allow_nonloopback() -> bool {
    std::env::var(METRICS_ALLOW_NONLOOPBACK_ENV).is_ok_and(|value| value.trim() == "true")
}

fn parse_address_with(value: &str, allow_nonloopback: bool) -> Result<SocketAddr, std::io::Error> {
    let address: SocketAddr = value.parse().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "INSTANT_METRICS_ADDR must be a loopback socket address",
        )
    })?;
    if address.port() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "INSTANT_METRICS_ADDR must use loopback and a nonzero port",
        ));
    }
    if address.ip().is_loopback() || (allow_nonloopback && is_private_metrics_ip(address.ip())) {
        return Ok(address);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "INSTANT_METRICS_ADDR must use loopback, or a private or unspecified address with INSTANT_METRICS_ALLOW_NONLOOPBACK=true",
    ))
}

fn is_private_metrics_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_unspecified(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unspecified(),
    }
}

pub struct MetricsListener(tokio::task::JoinHandle<()>);

impl Drop for MetricsListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Plain-text `EnvFilter` directives are tiny; anything larger is rejected
/// before the filter parser sees it.
const LOG_LEVEL_BODY_LIMIT: usize = 512;

fn log_level_router(state: Option<crate::telemetry::LogFilterControl>) -> Router {
    Router::new()
        .route("/log-level", get(current_log_level).post(reload_log_level))
        .with_state(state)
}

async fn current_log_level(
    State(control): State<Option<crate::telemetry::LogFilterControl>>,
) -> Response {
    let Some(control) = control else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "log filter reload is not initialized\n",
        )
            .into_response();
    };
    (axum::http::StatusCode::OK, control.current()).into_response()
}

async fn reload_log_level(
    State(control): State<Option<crate::telemetry::LogFilterControl>>,
    body: String,
) -> Response {
    let Some(control) = control else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "log filter reload is not initialized\n",
        )
            .into_response();
    };
    if body.len() > LOG_LEVEL_BODY_LIMIT {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("log filter directive must be at most {LOG_LEVEL_BODY_LIMIT} bytes\n"),
        )
            .into_response();
    }
    match control.update(&body) {
        Ok(filter) => (axum::http::StatusCode::OK, filter).into_response(),
        Err(error) => (axum::http::StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// Bind failure fails startup. A later listener failure requests process shutdown.
pub async fn start(
    address: SocketAddr,
    snapshot: Arc<dyn Fn() -> String + Send + Sync>,
    shutdown: CancellationToken,
) -> Result<MetricsListener, std::io::Error> {
    let listener = TcpListener::bind(address).await?;
    ENABLED.store(true, Ordering::Relaxed);
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let router = Router::new()
        .route(
            "/metrics",
            get(move || {
                let snapshot = Arc::clone(&snapshot);
                let admission = Arc::clone(&admission);
                async move {
                    let Ok(_permit) = admission.try_acquire_owned() else {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    };
                    (
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "text/plain; version=0.0.4; charset=utf-8",
                        )],
                        snapshot(),
                    )
                        .into_response()
                }
            }),
        )
        .merge(log_level_router(crate::telemetry::log_filter_control()));
    let task = tokio::spawn(async move {
        let stop = shutdown.clone();
        if axum::serve(listener, router)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .is_err()
        {
            tracing::error!("private metrics listener failed");
            shutdown.cancel();
        }
    });
    Ok(MetricsListener(task))
}

fn render_process_memory(out: &mut String) {
    // Linux-only: a development process without a cgroup limit exports no
    // limit gauge, so the memory alert stays quiet instead of inventing one.
    if let Some(bytes) = crate::process_memory::resident_bytes() {
        out.push_str("# HELP backendrelay_process_resident_memory_bytes Resident set size of this process.\n# TYPE backendrelay_process_resident_memory_bytes gauge\n");
        let _ = writeln!(out, "backendrelay_process_resident_memory_bytes {bytes}");
    }
    if let Some(bytes) = crate::process_memory::memory_limit_bytes() {
        out.push_str("# HELP backendrelay_process_memory_limit_bytes Memory limit reported by the container cgroup. Absent when the limit is unlimited or unknown.\n# TYPE backendrelay_process_memory_limit_bytes gauge\n");
        let _ = writeln!(out, "backendrelay_process_memory_limit_bytes {bytes}");
    }
}

fn render_runtime_ops(out: &mut String) {
    out.push_str("# HELP backendrelay_runtime_ops_total Realtime operations by fixed op and outcome.\n# TYPE backendrelay_runtime_ops_total counter\n");
    for (op, counts) in RUNTIME_OPS.iter().zip(&RUNTIME_OP_RUNS) {
        for (outcome, count) in RUNTIME_OP_OUTCOMES.iter().zip(counts) {
            let _ = writeln!(
                out,
                "backendrelay_runtime_ops_total{{op=\"{op}\",outcome=\"{outcome}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
    }
    out.push_str("# HELP backendrelay_runtime_op_duration_seconds Realtime operation handling time by fixed op.\n# TYPE backendrelay_runtime_op_duration_seconds histogram\n");
    for (op, metric) in RUNTIME_OPS.iter().zip(&RUNTIME_OP_DURATIONS) {
        let bins = metric
            .buckets
            .each_ref()
            .map(|bucket| bucket.load(Ordering::Relaxed));
        let count: u64 = bins.iter().sum();
        let mut cumulative = 0;
        for (limit, bucket) in BUCKETS_US.iter().zip(&bins) {
            cumulative += bucket;
            let _ = writeln!(
                out,
                "backendrelay_runtime_op_duration_seconds_bucket{{op=\"{op}\",le=\"{}\"}} {cumulative}",
                Duration::from_micros(*limit).as_secs_f64()
            );
        }
        let _ = writeln!(
            out,
            "backendrelay_runtime_op_duration_seconds_bucket{{op=\"{op}\",le=\"+Inf\"}} {count}\nbackendrelay_runtime_op_duration_seconds_count{{op=\"{op}\"}} {count}\nbackendrelay_runtime_op_duration_seconds_sum{{op=\"{op}\"}} {}",
            Duration::from_micros(metric.duration_us.load(Ordering::Relaxed)).as_secs_f64()
        );
    }
}

/// How many distinct SDK versions get their own `version` label per process.
const SDK_VERSION_LABEL_LIMIT: usize = 20;

#[derive(Default)]
struct SdkVersionLabels {
    versions: Vec<String>,
    index: HashMap<String, usize>,
}

impl SdkVersionLabels {
    fn label(&mut self, version: &str) -> String {
        if version == SDK_VERSION_OTHER {
            return SDK_VERSION_OTHER.to_owned();
        }
        if let Some(index) = self.index.get(version) {
            return self.versions[*index].clone();
        }
        if self.versions.len() >= SDK_VERSION_LABEL_LIMIT {
            return SDK_VERSION_OTHER.to_owned();
        }
        self.index.insert(version.to_owned(), self.versions.len());
        self.versions.push(version.to_owned());
        version.to_owned()
    }
}

static SDK_VERSION_LABELS: OnceLock<Mutex<SdkVersionLabels>> = OnceLock::new();

fn sdk_version_labels() -> &'static Mutex<SdkVersionLabels> {
    SDK_VERSION_LABELS.get_or_init(|| Mutex::new(SdkVersionLabels::default()))
}

fn sdk_session_counts(
    runtime: &[(String, String, u64)],
    admin: &[(String, String, u64)],
    labels: &mut SdkVersionLabels,
) -> BTreeMap<(String, String), u64> {
    let mut counts = BTreeMap::new();
    for (sdk, version, count) in runtime.iter().chain(admin) {
        let sdk = if SDK_LABELS.contains(&sdk.as_str()) {
            sdk.clone()
        } else {
            SDK_VERSION_OTHER.to_owned()
        };
        let entry = counts.entry((sdk, labels.label(version))).or_insert(0_u64);
        *entry = entry.saturating_add(*count);
    }
    counts
}

fn render_sdk_sessions(
    out: &mut String,
    runtime: &[(String, String, u64)],
    admin: &[(String, String, u64)],
    labels: &mut SdkVersionLabels,
) {
    let counts = sdk_session_counts(runtime, admin, labels);
    if counts.is_empty() {
        return;
    }
    out.push_str("# HELP backendrelay_sdk_sessions Live realtime sessions by SDK family and normalized version. Versions past the label limit share `other`.\n# TYPE backendrelay_sdk_sessions gauge\n");
    for ((sdk, version), count) in counts {
        let _ = writeln!(
            out,
            "backendrelay_sdk_sessions{{sdk=\"{sdk}\",version=\"{version}\"}} {count}"
        );
    }
}

/// Renders the bounded per-app series. Idle combinations are omitted: a busy
/// process would otherwise emit thousands of zero series for apps with no
/// traffic on that op or outcome. Apps past [`APP_LABEL_LIMIT`] share `other`.
fn render_app_metrics(out: &mut String, metrics: &AppMetrics, app_sessions: &[(Uuid, u64)]) {
    let (labels, session_counts, overflow) = {
        let state = metrics.lock();
        let mut session_counts = [0_u64; APP_LABEL_SLOTS];
        let mut overflow = false;
        for (app_id, count) in app_sessions {
            if let Some(slot) = state.index.get(app_id).copied() {
                session_counts[slot] = session_counts[slot].saturating_add(*count);
            }
        }
        for slot in state.index.values() {
            overflow |= *slot == APP_LABEL_OTHER;
        }
        let labels = state
            .slots
            .iter()
            .enumerate()
            .map(|(slot, _)| app_label(&state, slot))
            .collect::<Vec<_>>();
        (labels, session_counts, overflow)
    };
    if labels.is_empty() && !overflow {
        return;
    }
    // Slots that emit rows: every named app, plus `other` once an app overflowed
    // or carries sessions.
    let mut slots = labels
        .iter()
        .enumerate()
        .map(|(slot, label)| (slot, label.clone()))
        .collect::<Vec<_>>();
    if overflow || session_counts[APP_LABEL_OTHER] > 0 {
        slots.push((APP_LABEL_OTHER, "other".to_owned()));
    }
    let counters = &metrics.counters;
    out.push_str("# HELP backendrelay_app_sessions Realtime sessions by app. Apps past the label limit share `other`.\n# TYPE backendrelay_app_sessions gauge\n");
    for (slot, label) in &slots {
        let _ = writeln!(
            out,
            "backendrelay_app_sessions{{app=\"{label}\"}} {}",
            session_counts[*slot]
        );
    }
    let mut rows = Vec::new();
    for (slot, label) in &slots {
        for (index, op) in RUNTIME_OPS.iter().enumerate() {
            for (outcome, count) in RUNTIME_OP_OUTCOMES.iter().zip(&counters.ops[*slot][index]) {
                let count = count.load(Ordering::Relaxed);
                if count > 0 {
                    rows.push(format!(
                        "backendrelay_app_ops_total{{app=\"{label}\",op=\"{op}\",outcome=\"{outcome}\"}} {count}"
                    ));
                }
            }
        }
    }
    if !rows.is_empty() {
        out.push_str("# HELP backendrelay_app_ops_total Realtime operations by app, fixed op and outcome. Apps past the label limit share `other`.\n# TYPE backendrelay_app_ops_total counter\n");
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
    }
    let mut rows = Vec::new();
    for (slot, label) in &slots {
        let micros = counters.db_micros[*slot].load(Ordering::Relaxed);
        if micros > 0 {
            rows.push(format!(
                "backendrelay_app_db_seconds_total{{app=\"{label}\"}} {}",
                Duration::from_micros(micros).as_secs_f64()
            ));
        }
    }
    if !rows.is_empty() {
        out.push_str("# HELP backendrelay_app_db_seconds_total Time database connections were held for this app's requests. Apps past the label limit share `other`.\n# TYPE backendrelay_app_db_seconds_total counter\n");
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
    }
    let mut rows = Vec::new();
    for (slot, label) in &slots {
        for (index, surface) in APP_ERROR_SURFACES.iter().enumerate() {
            let count = counters.errors[*slot][index].load(Ordering::Relaxed);
            if count > 0 {
                rows.push(format!(
                    "backendrelay_app_errors_total{{app=\"{label}\",surface=\"{surface}\"}} {count}"
                ));
            }
        }
    }
    if !rows.is_empty() {
        out.push_str("# HELP backendrelay_app_errors_total Failed requests by app and surface. Apps past the label limit share `other`.\n# TYPE backendrelay_app_errors_total counter\n");
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
    }
}

fn render_pool_waits(out: &mut String) {
    let bins = POOL_WAITS
        .buckets
        .each_ref()
        .map(|b| b.load(Ordering::Relaxed));
    let count: u64 = bins.iter().sum();
    let _ = writeln!(
        out,
        "# TYPE backendrelay_pool_waiting gauge\nbackendrelay_pool_waiting {}",
        POOL_WAITING.load(Ordering::Relaxed)
    );
    out.push_str("# TYPE backendrelay_pool_acquisition_seconds histogram\n");
    let mut cumulative = 0;
    for (limit, bucket) in BUCKETS_US.iter().zip(&bins) {
        cumulative += bucket;
        let _ = writeln!(
            out,
            "backendrelay_pool_acquisition_seconds_bucket{{le=\"{}\"}} {}",
            *limit as f64 / 1_000_000.0,
            cumulative
        );
    }
    let _ = writeln!(
        out,
        "backendrelay_pool_acquisition_seconds_bucket{{le=\"+Inf\"}} {count}\nbackendrelay_pool_acquisition_seconds_count {count}\nbackendrelay_pool_acquisition_seconds_sum {}",
        POOL_WAITS.duration_us.load(Ordering::Relaxed) as f64 / 1_000_000.0
    );
    out.push_str("# TYPE backendrelay_pool_waiting_by_role gauge\n# TYPE backendrelay_pool_acquisition_by_role_seconds histogram\n");
    for (index, role) in POOL_ROLES.iter().enumerate() {
        let metric = &ROLE_WAITS[index];
        let bins = metric.buckets.each_ref().map(|b| b.load(Ordering::Relaxed));
        let count: u64 = bins.iter().sum();
        let _ = writeln!(
            out,
            "backendrelay_pool_waiting_by_role{{role=\"{role}\"}} {}",
            ROLE_WAITING[index].load(Ordering::Relaxed)
        );
        let mut cumulative = 0;
        for (limit, bucket) in BUCKETS_US.iter().zip(&bins) {
            cumulative += bucket;
            let _ = writeln!(
                out,
                "backendrelay_pool_acquisition_by_role_seconds_bucket{{role=\"{role}\",le=\"{}\"}} {cumulative}",
                *limit as f64 / 1_000_000.0
            );
        }
        let _ = writeln!(
            out,
            "backendrelay_pool_acquisition_by_role_seconds_bucket{{role=\"{role}\",le=\"+Inf\"}} {count}\nbackendrelay_pool_acquisition_by_role_seconds_count{{role=\"{role}\"}} {count}\nbackendrelay_pool_acquisition_by_role_seconds_sum{{role=\"{role}\"}} {}",
            metric.duration_us.load(Ordering::Relaxed) as f64 / 1_000_000.0
        );
    }
}

fn render_op_counters(out: &mut String) {
    out.push_str("# HELP backendrelay_emails_total Outbound email attempts by configured provider and fixed outcome.\n# TYPE backendrelay_emails_total counter\n");
    for (provider, counts) in EMAIL_PROVIDERS.iter().zip(&EMAILS) {
        for (outcome, count) in EMAIL_OUTCOMES.iter().zip(counts) {
            let _ = writeln!(
                out,
                "backendrelay_emails_total{{provider=\"{provider}\",outcome=\"{outcome}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
    }
    out.push_str("# HELP backendrelay_webhook_deliveries_total Webhook delivery attempts by fixed outcome.\n# TYPE backendrelay_webhook_deliveries_total counter\n");
    for (outcome, count) in WEBHOOK_DELIVERY_OUTCOMES.iter().zip(&WEBHOOK_DELIVERIES) {
        let _ = writeln!(
            out,
            "backendrelay_webhook_deliveries_total{{outcome=\"{outcome}\"}} {}",
            count.load(Ordering::Relaxed)
        );
    }
    out.push_str("# HELP backendrelay_storage_ops_total Storage API operations by fixed op and outcome.\n# TYPE backendrelay_storage_ops_total counter\n");
    for (op, counts) in STORAGE_OPS.iter().zip(&STORAGE_OP_RUNS) {
        for (outcome, count) in STORAGE_OP_OUTCOMES.iter().zip(counts) {
            let _ = writeln!(
                out,
                "backendrelay_storage_ops_total{{op=\"{op}\",outcome=\"{outcome}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
    }
    out.push_str("# HELP backendrelay_task_runs_total Scheduled task runs by fixed task name and outcome.\n# TYPE backendrelay_task_runs_total counter\n");
    for (task, counts) in TASKS.iter().zip(&TASK_RUNS) {
        for (outcome, count) in TASK_OUTCOMES.iter().zip(counts) {
            let _ = writeln!(
                out,
                "backendrelay_task_runs_total{{task=\"{task}\",outcome=\"{outcome}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
    }
    out.push_str("# HELP backendrelay_task_last_success_timestamp_seconds Unix time of the last successful run by fixed task name; zero until the first success.\n# TYPE backendrelay_task_last_success_timestamp_seconds gauge\n");
    for (task, at) in TASKS.iter().zip(&TASK_LAST_SUCCESS) {
        let _ = writeln!(
            out,
            "backendrelay_task_last_success_timestamp_seconds{{task=\"{task}\"}} {}",
            at.load(Ordering::Relaxed)
        );
    }
    out.push_str("# HELP backendrelay_websocket_exits_total Fixed-category WebSocket loop exits; observations, not client error allowances.\n# TYPE backendrelay_websocket_exits_total counter\n");
    for (reason, count) in instant_http::runtime_websocket_exit_metrics() {
        let _ = writeln!(
            out,
            "backendrelay_websocket_exits_total{{reason=\"{reason}\"}} {count}"
        );
    }
}

fn render_runtime_gauges(
    out: &mut String,
    runtime: &RuntimeRealtimeMetrics,
    admin_sessions: usize,
    admin_sdk_sessions: &[(String, String, u64)],
) {
    {
        let mut labels = sdk_version_labels()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        render_sdk_sessions(out, &runtime.sdk_sessions, admin_sdk_sessions, &mut labels);
    }
    let _ = writeln!(
        out,
        "backendrelay_sessions{{consumer=\"admin\"}} {admin_sessions}"
    );
    for (role, m) in [("runtime", runtime)] {
        for (name, value) in [
            ("sessions", m.sessions as u64),
            ("subscriptions", m.subscriptions as u64),
            ("cache_apps", m.cache_apps as u64),
            ("cache_entries", m.cache_entries as u64),
            ("cache_weight_bytes", m.cache_weight_bytes),
            ("cache_in_flight", m.cache_in_flight as u64),
            ("cache_retired_loads", m.cache_retired_loads as u64),
            (
                "root_frame_cache_entries",
                m.root_frame_cache_entries as u64,
            ),
            ("root_frame_cache_bytes", m.root_frame_cache_bytes),
            ("root_frame_cache_hits_total", m.root_frame_cache_hits_total),
            (
                "root_frame_cache_misses_total",
                m.root_frame_cache_misses_total,
            ),
            (
                "root_frame_cache_loads_total",
                m.root_frame_cache_loads_total,
            ),
            (
                "root_frame_cache_retire_scanned_total",
                m.root_frame_cache_retire_scanned_total,
            ),
            (
                "root_frame_cache_retire_retired_total",
                m.root_frame_cache_retire_retired_total,
            ),
            (
                "root_frame_cache_retire_nanos_total",
                m.root_frame_cache_retire_nanos_total,
            ),
            (
                "root_frame_cache_expired_total",
                m.root_frame_cache_expired_total,
            ),
            ("outbound_queued", m.outbound_queued as u64),
            ("transact_queued", m.transact_queued as u64),
            ("refresh_queued", m.refresh_queued as u64),
            ("refresh_active", m.refresh_active as u64),
            ("refresh_waiting", m.refresh_waiting as u64),
            ("refresh_waiting_apps", m.refresh_waiting_apps as u64),
            (
                "refresh_admission_rejections_total",
                m.refresh_admission_rejections_total,
            ),
            ("receive_queued", m.receive_queued as u64),
            ("receive_active_groups", m.receive_active_groups as u64),
            ("receive_workers", m.receive_workers as u64),
            ("receive_oldest_wait_ms", m.receive_oldest_wait_ms),
            ("cleanup_pending", m.cleanup.pending as u64),
            ("cleanup_active", m.cleanup.active as u64),
            ("cleanup_failures_total", m.cleanup.failures),
            (
                "query_admission_rejections_total",
                m.query_admission_rejections_total,
            ),
            (
                "transaction_admission_rejections_total",
                m.transaction_admission_rejections_total,
            ),
        ] {
            let _ = writeln!(out, "backendrelay_{name}{{consumer=\"{role}\"}} {value}");
        }
        let _ = writeln!(
            out,
            "backendrelay_cleanup_oldest_retry_age_seconds{{consumer=\"{role}\"}} {}",
            m.cleanup.oldest_retry_age.unwrap_or_default().as_secs_f64()
        );
    }
    render_result_cache_metrics(out, runtime);
}

fn render_result_cache_metrics(out: &mut String, runtime: &RuntimeRealtimeMetrics) {
    out.push_str("# HELP backendrelay_result_cache_bytes Approximate ready bytes retained by every attached result cache, including shared root frames, under the process-wide budget.\n# TYPE backendrelay_result_cache_bytes gauge\n");
    let _ = writeln!(
        out,
        "backendrelay_result_cache_bytes {}",
        runtime.result_cache_bytes
    );
    out.push_str("# HELP backendrelay_result_cache_max_bytes Configured process-wide result-cache byte budget; zero when disabled.\n# TYPE backendrelay_result_cache_max_bytes gauge\n");
    let _ = writeln!(
        out,
        "backendrelay_result_cache_max_bytes {}",
        runtime.result_cache_max_bytes
    );
    out.push_str("# HELP backendrelay_result_cache_evictions_total Entries evicted from attached result caches by the process-wide byte budget.\n# TYPE backendrelay_result_cache_evictions_total counter\n");
    let _ = writeln!(
        out,
        "backendrelay_result_cache_evictions_total {}",
        runtime.result_cache_evictions_total
    );
}

pub fn render(
    runtime: &RuntimeRealtimeMetrics,
    admin_sessions: usize,
    admin_sdk_sessions: &[(String, String, u64)],
) -> String {
    let mut out = String::with_capacity(20_000);
    out.push_str("# HELP backendrelay_panics_total Process panics observed by the installed panic hook.\n# TYPE backendrelay_panics_total counter\n");
    let _ = writeln!(out, "backendrelay_panics_total {}", panics());
    out.push_str("# HELP backendrelay_replication_slot_recreated_total Invalidated or lost replication slots recreated in place, by fixed purpose.\n# TYPE backendrelay_replication_slot_recreated_total counter\n");
    for (purpose, count) in SLOT_PURPOSES.iter().zip(&SLOT_RECREATIONS) {
        let _ = writeln!(
            out,
            "backendrelay_replication_slot_recreated_total{{purpose=\"{purpose}\"}} {}",
            count.load(Ordering::Relaxed)
        );
    }
    out.push_str("# HELP backendrelay_wal_sink_refresh_timeouts_total WAL window refreshes abandoned at the per-refresh deadline.\n# TYPE backendrelay_wal_sink_refresh_timeouts_total counter\n");
    let _ = writeln!(
        out,
        "backendrelay_wal_sink_refresh_timeouts_total {}",
        wal_sink_refresh_timeouts()
    );
    out.push_str("# HELP backendrelay_wal_sink_forced_resyncs_total Session refreshes forced after a WAL sink refresh deadline.\n# TYPE backendrelay_wal_sink_forced_resyncs_total counter\n");
    let _ = writeln!(
        out,
        "backendrelay_wal_sink_forced_resyncs_total {}",
        wal_sink_forced_resyncs()
    );
    out.push_str("# HELP backendrelay_invalidation_stream_restarts_total Invalidation replication stream producer restarts after transport failures.\n# TYPE backendrelay_invalidation_stream_restarts_total counter\n");
    let _ = writeln!(
        out,
        "backendrelay_invalidation_stream_restarts_total {}",
        INVALIDATION_STREAM_RESTARTS.load(Ordering::Relaxed)
    );
    out.push_str("# HELP backendrelay_invalidation_health Whether this process reports healthy invalidation-stream readiness (1) or withholds it (0).\n# TYPE backendrelay_invalidation_health gauge\n");
    let _ = writeln!(
        out,
        "backendrelay_invalidation_health {}",
        INVALIDATION_HEALTH.load(Ordering::Relaxed)
    );
    out.push_str("# TYPE backendrelay_up gauge\nbackendrelay_up 1\n# TYPE backendrelay_http_requests_total counter\n# TYPE backendrelay_http_request_duration_seconds histogram\n");
    render_process_memory(&mut out);
    for (group, metric) in GROUPS.iter().zip(&REQUESTS) {
        let bins = metric.buckets.each_ref().map(|b| b.load(Ordering::Relaxed));
        let count: u64 = bins.iter().sum();
        for (class, value) in metric.counts.iter().enumerate() {
            let value = value.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "backendrelay_http_requests_total{{surface=\"{group}\",status_class=\"{class}xx\"}} {value}"
            );
        }
        let mut cumulative = 0;
        for (limit, value) in BUCKETS_US.iter().zip(&bins) {
            cumulative += value;
            let _ = writeln!(
                out,
                "backendrelay_http_request_duration_seconds_bucket{{surface=\"{group}\",le=\"{}\"}} {}",
                *limit as f64 / 1_000_000.0,
                cumulative
            );
        }
        let _ = writeln!(
            out,
            "backendrelay_http_request_duration_seconds_bucket{{surface=\"{group}\",le=\"+Inf\"}} {count}"
        );
        let _ = writeln!(
            out,
            "backendrelay_http_request_duration_seconds_count{{surface=\"{group}\"}} {count}"
        );
        let _ = writeln!(
            out,
            "backendrelay_http_request_duration_seconds_sum{{surface=\"{group}\"}} {}",
            metric.duration_us.load(Ordering::Relaxed) as f64 / 1_000_000.0
        );
    }
    render_pool_waits(&mut out);
    render_pool_stages(&mut out);
    render_op_counters(&mut out);
    render_runtime_ops(&mut out);
    render_runtime_gauges(&mut out, runtime, admin_sessions, admin_sdk_sessions);
    render_app_metrics(&mut out, app_metrics(), &runtime.app_sessions);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    async fn log_level_request(router: &Router, method: &str, body: &str) -> (StatusCode, String) {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri("/log-level")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn log_level_endpoint_reloads_valid_filters_and_rejects_invalid_ones() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let (layer, control) =
            crate::telemetry::LogFilterControl::install(tracing_subscriber::EnvFilter::new("info"));
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        let router = log_level_router(Some(control));

        assert_eq!(
            log_level_request(&router, "GET", "").await,
            (StatusCode::OK, "info".to_owned())
        );
        assert_eq!(
            log_level_request(&router, "POST", "debug").await,
            (StatusCode::OK, "debug".to_owned())
        );
        assert_eq!(
            log_level_request(&router, "GET", "").await,
            (StatusCode::OK, "debug".to_owned())
        );

        let (status, _) = log_level_request(&router, "POST", "definitely=not-a-level").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            log_level_request(&router, "GET", "").await,
            (StatusCode::OK, "debug".to_owned())
        );

        let (status, _) =
            log_level_request(&router, "POST", &"x".repeat(LOG_LEVEL_BODY_LIMIT + 1)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            log_level_request(&router, "GET", "").await,
            (StatusCode::OK, "debug".to_owned())
        );
    }

    #[tokio::test]
    async fn log_level_endpoint_reports_an_uninitialized_reload() {
        let router = log_level_router(None);
        assert_eq!(
            log_level_request(&router, "GET", "").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            log_level_request(&router, "POST", "debug").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn email_metric_labels_are_a_fixed_provider_and_outcome_vocabulary() {
        record_email(EmailProvider::Postmark, EmailOutcome::Sent);
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        for provider in EMAIL_PROVIDERS {
            for outcome in EMAIL_OUTCOMES {
                assert!(text.contains(&format!(
                    "backendrelay_emails_total{{provider=\"{provider}\",outcome=\"{outcome}\"}}"
                )));
            }
        }
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_emails_total{"))
                .count(),
            EMAIL_PROVIDERS.len() * EMAIL_OUTCOMES.len()
        );
        assert!(!text.contains("example.com") && !text.contains('@'));
    }

    #[test]
    fn webhook_delivery_metrics_use_a_fixed_outcome_vocabulary() {
        record_webhook_delivery(WebhookDeliveryOutcome::Failed);
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        for outcome in WEBHOOK_DELIVERY_OUTCOMES {
            assert!(text.contains(&format!(
                "backendrelay_webhook_deliveries_total{{outcome=\"{outcome}\"}}"
            )));
        }
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_webhook_deliveries_total{"))
                .count(),
            WEBHOOK_DELIVERY_OUTCOMES.len()
        );
    }

    #[test]
    fn storage_operation_metrics_use_fixed_op_and_outcome_vocabulary() {
        for (index, op) in [
            StorageOp::Upload,
            StorageOp::DownloadUrl,
            StorageOp::Delete,
            StorageOp::List,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(op.index(), index);
        }
        record_storage_op(StorageOp::Upload, StorageOpOutcome::Error);
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        for op in STORAGE_OPS {
            for outcome in STORAGE_OP_OUTCOMES {
                assert!(text.contains(&format!(
                    "backendrelay_storage_ops_total{{op=\"{op}\",outcome=\"{outcome}\"}}"
                )));
            }
        }
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_storage_ops_total{"))
                .count(),
            STORAGE_OPS.len() * STORAGE_OP_OUTCOMES.len()
        );
    }

    #[test]
    fn task_metrics_use_a_fixed_task_and_outcome_vocabulary() {
        for (index, outcome) in [
            TaskOutcome::Success,
            TaskOutcome::Failure,
            TaskOutcome::Timeout,
            TaskOutcome::Panic,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(outcome.index(), index);
        }
        record_task_run("indexing-recovery", TaskOutcome::Success);
        record_task_run("not-a-scheduled-task", TaskOutcome::Failure);
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        for task in TASKS {
            for outcome in TASK_OUTCOMES {
                assert!(text.contains(&format!(
                    "backendrelay_task_runs_total{{task=\"{task}\",outcome=\"{outcome}\"}}"
                )));
            }
            let line = text
                .lines()
                .find(|line| {
                    line.starts_with(&format!(
                        "backendrelay_task_last_success_timestamp_seconds{{task=\"{task}\"}} "
                    ))
                })
                .unwrap();
            line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap();
        }
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_task_runs_total{"))
                .count(),
            TASKS.len() * TASK_OUTCOMES.len()
        );
        assert_eq!(
            text.lines()
                .filter(|line| line
                    .starts_with("backendrelay_task_last_success_timestamp_seconds{"))
                .count(),
            TASKS.len()
        );
        let last_success = text
            .lines()
            .find(|line| {
                line.starts_with(
                    "backendrelay_task_last_success_timestamp_seconds{task=\"indexing-recovery\"} ",
                )
            })
            .unwrap();
        assert!(
            last_success
                .rsplit_once(' ')
                .unwrap()
                .1
                .parse::<u64>()
                .unwrap()
                > 0
        );
        assert!(!text.contains("not-a-scheduled-task"));
        assert_eq!(task_index("other"), TASKS.len() - 1);
        assert!(
            TASK_RUNS[task_index("other")][TaskOutcome::Failure.index()].load(Ordering::Relaxed)
                >= 1
        );
    }

    #[test]
    fn runtime_op_metrics_use_fixed_cardinality_and_map_unknown_ops_to_other() {
        for (index, op) in RUNTIME_OPS.iter().enumerate() {
            assert_eq!(runtime_op_index(Some(op)), index);
        }
        for alias in ["refresh-sync-table", "resync-table"] {
            assert_eq!(
                runtime_op_index(Some(alias)),
                runtime_op_index(Some("refresh"))
            );
        }
        for unknown in [None, Some("not-an-op"), Some("refresh-table")] {
            assert_eq!(runtime_op_index(unknown), RUNTIME_OPS.len() - 1);
        }
        record_runtime_op(
            Some("transact"),
            RuntimeOpOutcome::RateLimited,
            Duration::from_millis(3),
        );
        record_runtime_op(
            Some("not-an-op"),
            RuntimeOpOutcome::Error,
            Duration::from_millis(1),
        );
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        for op in RUNTIME_OPS {
            for outcome in RUNTIME_OP_OUTCOMES {
                assert!(text.contains(&format!(
                    "backendrelay_runtime_ops_total{{op=\"{op}\",outcome=\"{outcome}\"}}"
                )));
            }
            assert!(text.contains(&format!(
                "backendrelay_runtime_op_duration_seconds_count{{op=\"{op}\"}}"
            )));
        }
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_runtime_ops_total{"))
                .count(),
            RUNTIME_OPS.len() * RUNTIME_OP_OUTCOMES.len()
        );
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_runtime_op_duration_seconds_bucket{"))
                .count(),
            RUNTIME_OPS.len() * (BUCKETS_US.len() + 1)
        );
        assert!(!text.contains("not-an-op"));
        let other = runtime_op_index(Some("other"));
        assert!(
            RUNTIME_OP_RUNS[other][RuntimeOpOutcome::Error.index()].load(Ordering::Relaxed) >= 1
        );
    }

    #[test]
    fn sdk_session_metric_keeps_twenty_versions_and_aggregates_the_rest() {
        let mut labels = SdkVersionLabels::default();
        let mut runtime = (0..21)
            .map(|patch| ("core".to_owned(), format!("1.0.{patch}"), 1_u64))
            .collect::<Vec<_>>();
        runtime.push(("react".to_owned(), "1.0.0".to_owned(), 2));
        runtime.push(("not-an-sdk".to_owned(), "1.0.0".to_owned(), 1));
        runtime.push(("admin".to_owned(), SDK_VERSION_OTHER.to_owned(), 1));
        let admin = [("admin".to_owned(), "1.0.0".to_owned(), 3)];
        let counts = sdk_session_counts(&runtime, &admin, &mut labels);

        assert_eq!(labels.versions.len(), SDK_VERSION_LABEL_LIMIT);
        assert!(counts.contains_key(&("core".to_owned(), "1.0.19".to_owned())));
        assert!(!counts.contains_key(&("core".to_owned(), "1.0.20".to_owned())));
        assert_eq!(
            counts.get(&("core".to_owned(), SDK_VERSION_OTHER.to_owned())),
            Some(&1)
        );
        assert_eq!(
            counts.get(&("react".to_owned(), "1.0.0".to_owned())),
            Some(&2)
        );
        assert_eq!(
            counts.get(&("admin".to_owned(), "1.0.0".to_owned())),
            Some(&3)
        );
        assert_eq!(
            counts.get(&(SDK_VERSION_OTHER.to_owned(), "1.0.0".to_owned())),
            Some(&1)
        );
        assert_eq!(
            counts.get(&("admin".to_owned(), SDK_VERSION_OTHER.to_owned())),
            Some(&1)
        );
    }

    #[test]
    fn sdk_session_metric_renders_live_runtime_and_admin_sessions() {
        let mut labels = SdkVersionLabels::default();
        let runtime = [("core".to_owned(), "1.0.66".to_owned(), 2_u64)];
        let admin = [("admin".to_owned(), "1.0.66".to_owned(), 1_u64)];
        let mut text = String::new();
        render_sdk_sessions(&mut text, &runtime, &admin, &mut labels);
        assert!(text.contains("# TYPE backendrelay_sdk_sessions gauge\n"));
        assert!(text.contains("backendrelay_sdk_sessions{sdk=\"core\",version=\"1.0.66\"} 2\n"));
        assert!(text.contains("backendrelay_sdk_sessions{sdk=\"admin\",version=\"1.0.66\"} 1\n"));
    }

    #[test]
    fn websocket_exit_labels_are_fixed_categories_without_request_or_session_data() {
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        let rows = text
            .lines()
            .filter(|line| line.starts_with("backendrelay_websocket_exits_total{"))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 13);
        for ((reason, _), row) in instant_http::runtime_websocket_exit_metrics()
            .into_iter()
            .zip(rows)
        {
            assert!(row.starts_with(&format!(
                "backendrelay_websocket_exits_total{{reason=\"{reason}\"}} "
            )));
            row.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap();
            assert!(
                reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
            );
            assert!(!row.contains("session") && !row.contains("app_id") && !row.contains("token"));
        }
    }

    #[test]
    fn refresh_admission_metrics_have_fixed_cardinality_and_no_tenant_labels() {
        let metrics = RuntimeRealtimeMetrics {
            refresh_active: 12,
            refresh_waiting: 300,
            refresh_waiting_apps: 5,
            refresh_admission_rejections_total: 2,
            ..RuntimeRealtimeMetrics::default()
        };
        let text = render(&metrics, 0, &[]);
        for (name, value) in [
            ("refresh_active", 12),
            ("refresh_waiting", 300),
            ("refresh_waiting_apps", 5),
            ("refresh_admission_rejections_total", 2),
        ] {
            assert!(text.contains(&format!(
                "backendrelay_{name}{{consumer=\"runtime\"}} {value}\n"
            )));
        }
    }
    #[test]
    fn metrics_must_bind_private_loopback() {
        for input in [
            "0.0.0.0:9091",
            "[::]:9091",
            "192.168.1.2:9091",
            "127.0.0.1:0",
            "localhost:9091",
        ] {
            assert!(parse_address_with(input, false).is_err());
        }
        assert!(parse_address_with("127.0.0.1:9091", false).is_ok());
        assert!(parse_address_with("[::1]:9091", false).is_ok());
        assert!(parse_address_with("172.17.0.4:9091", false).is_err());
        assert!(parse_address_with("172.17.0.4:9091", true).is_ok());
        assert!(parse_address_with("0.0.0.0:9091", true).is_ok());
        assert!(parse_address_with("[::]:9091", true).is_ok());
        assert!(parse_address_with("8.8.8.8:9091", true).is_err());
    }
    #[test]
    fn replication_slot_recreation_counter_uses_fixed_purpose_labels() {
        for (index, purpose) in SLOT_PURPOSES.iter().enumerate() {
            assert_eq!(
                match index {
                    0 => ReplicationSlotPurpose::Invalidator,
                    _ => ReplicationSlotPurpose::Aggregator,
                }
                .index(),
                index
            );
            let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
            assert!(text.contains(&format!(
                "backendrelay_replication_slot_recreated_total{{purpose=\"{purpose}\"}}"
            )));
        }
    }

    #[test]
    fn invalidation_health_metrics_are_exported() {
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        assert!(text.contains("# TYPE backendrelay_invalidation_stream_restarts_total counter\n"));
        assert!(text.contains("backendrelay_invalidation_stream_restarts_total "));
        assert!(text.contains("# TYPE backendrelay_invalidation_health gauge\n"));
        assert!(text.contains("backendrelay_invalidation_health "));
    }

    #[test]
    fn result_cache_budget_metrics_are_exported_without_tenant_labels() {
        let metrics = RuntimeRealtimeMetrics {
            result_cache_bytes: 1234,
            result_cache_max_bytes: 4096,
            result_cache_evictions_total: 7,
            root_frame_cache_bytes: 512,
            ..RuntimeRealtimeMetrics::default()
        };
        let text = render(&metrics, 0, &[]);
        assert!(text.contains("backendrelay_result_cache_bytes 1234\n"));
        assert!(text.contains("backendrelay_result_cache_max_bytes 4096\n"));
        assert!(text.contains("backendrelay_result_cache_evictions_total 7\n"));
        assert!(text.contains("backendrelay_root_frame_cache_bytes{consumer=\"runtime\"} 512\n"));
        #[cfg(target_os = "linux")]
        assert!(text.contains("backendrelay_process_resident_memory_bytes "));
    }

    #[test]
    fn request_labels_never_include_identifiers_or_raw_urls() {
        assert_eq!(request_group("/dash/apps/private-token?secret=abc"), 2);
        assert_eq!(request_group("/arbitrary-secret"), 5);
        assert_eq!(GROUPS.len(), REQUESTS.len());
    }

    #[test]
    fn pool_role_labels_are_a_fixed_vocabulary_not_connection_or_app_names() {
        for (index, name) in POOL_ROLES.iter().enumerate() {
            assert_eq!(pool_role(name), index);
        }
        assert_eq!(
            pool_role("postgres://user:secret@example.invalid/private-app"),
            4
        );
        let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
        for role in POOL_ROLES {
            assert!(text.contains(&format!(
                "backendrelay_pool_waiting_by_role{{role=\"{role}\"}}"
            )));
            assert!(text.contains(&format!(
                "backendrelay_pool_acquisition_by_role_seconds_count{{role=\"{role}\"}}"
            )));
        }
        assert!(!text.contains("secret") && !text.contains("private-app"));
        for name in [
            "checkout_mode_seconds",
            "wait_connect_seconds",
            "source_lease_seconds",
            "connection_creations_total",
        ] {
            assert!(text.contains(&format!("backendrelay_pool_{name}")));
        }
        assert!(
            text.len() < 256 * 1024,
            "native snapshot must fit the unchanged private observer ingestion cap"
        );
        assert!(!text.contains("postgres://"));
    }

    #[test]
    fn pool_stage_histograms_keep_exact_local_counts_and_finite_durations() {
        let metrics = [
            RequestMetrics::new(),
            RequestMetrics::new(),
            RequestMetrics::new(),
        ];
        for (index, outcome) in [
            PoolTimingOutcome::Completed,
            PoolTimingOutcome::Error,
            PoolTimingOutcome::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(outcome.index(), index);
            record_duration(
                &metrics[index],
                Duration::from_millis(7 * (u64::try_from(index).unwrap() + 1)),
            );
        }
        let mut text = String::new();
        render_pool_duration(
            &mut text,
            "backendrelay_pool_checkout_mode_seconds",
            "control",
            "outcome",
            &CHECKOUT_OUTCOMES,
            &metrics,
        );
        for (index, outcome) in CHECKOUT_OUTCOMES.iter().enumerate() {
            assert!(text.contains(&format!(
                "_count{{role=\"control\",outcome=\"{outcome}\"}} 1\n"
            )));
            assert!(text.contains(&format!(
                "_sum{{role=\"control\",outcome=\"{outcome}\"}} {}\n",
                Duration::from_millis(7 * (u64::try_from(index).unwrap() + 1)).as_secs_f64()
            )));
        }
        assert!(!text.contains("NaN") && !text.contains("inf"));
    }

    #[test]
    fn app_metric_labels_are_capped_and_overflow_to_other() {
        let metrics = AppMetrics::new();
        let apps = (0..=APP_LABEL_LIMIT)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        for app_id in &apps {
            metrics.register(*app_id);
        }
        for app_id in &apps {
            metrics.record_op(*app_id, Some("transact"), RuntimeOpOutcome::Ok);
        }
        let mut text = String::new();
        render_app_metrics(&mut text, &metrics, &[]);
        let labels = text
            .lines()
            .filter_map(|line| line.strip_prefix("backendrelay_app_ops_total{app=\""))
            .filter_map(|rest| rest.split('"').next())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(labels.len(), APP_LABEL_LIMIT + 1);
        assert!(labels.contains("other"));
        assert!(text.contains(
            "backendrelay_app_ops_total{app=\"other\",op=\"transact\",outcome=\"ok\"} 1\n"
        ));
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_app_ops_total{"))
                .count(),
            APP_LABEL_LIMIT + 1
        );
        // A 102nd app joins the same overflow bucket instead of adding a label.
        let extra = Uuid::new_v4();
        metrics.register(extra);
        metrics.record_op(extra, Some("transact"), RuntimeOpOutcome::Ok);
        let mut text = String::new();
        render_app_metrics(&mut text, &metrics, &[]);
        assert!(text.contains(
            "backendrelay_app_ops_total{app=\"other\",op=\"transact\",outcome=\"ok\"} 2\n"
        ));
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("backendrelay_app_ops_total{"))
                .count(),
            APP_LABEL_LIMIT + 1
        );
    }

    #[test]
    fn unknown_app_ids_create_no_per_app_series() {
        let metrics = AppMetrics::new();
        let unknown = Uuid::new_v4();
        metrics.record_op(unknown, Some("add-query"), RuntimeOpOutcome::Ok);
        metrics.record_db_seconds(unknown, Duration::from_millis(5));
        metrics.record_error(unknown, AppErrorSurface::Runtime);
        let mut text = String::new();
        render_app_metrics(&mut text, &metrics, &[(unknown, 3)]);
        assert!(text.is_empty(), "{text}");
    }

    #[test]
    fn app_metric_slugs_come_from_the_config_app_flag() {
        let metrics = AppMetrics::new();
        let slugged = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let fallback = Uuid::parse_str("66666666-7777-8888-9999-aaaaaaaaaaaa").unwrap();
        metrics.register(slugged);
        metrics.register(fallback);
        let mut slugs = serde_json::Map::new();
        slugs.insert(slugged.to_string(), Value::String("acme-prod".to_owned()));
        slugs.insert(fallback.to_string(), Value::String("Not A Slug".to_owned()));
        slugs.insert("not-an-app".to_owned(), Value::String("ignored".to_owned()));
        slugs.insert(
            Uuid::new_v4().to_string(),
            Value::String("other".to_owned()),
        );
        metrics.set_slugs(&Value::Object(slugs));
        metrics.record_op(slugged, Some("transact"), RuntimeOpOutcome::Ok);
        metrics.record_op(fallback, Some("add-query"), RuntimeOpOutcome::Ok);
        let mut text = String::new();
        render_app_metrics(&mut text, &metrics, &[]);
        assert!(text.contains(
            "backendrelay_app_ops_total{app=\"acme-prod\",op=\"transact\",outcome=\"ok\"} 1\n"
        ));
        assert!(text.contains(
            "backendrelay_app_ops_total{app=\"66666666\",op=\"add-query\",outcome=\"ok\"} 1\n"
        ));
        assert!(!text.contains("Not A Slug") && !text.contains("ignored"));
    }

    #[test]
    fn histogram_scrapes_remain_monotonic_during_concurrent_recording() {
        let writer = std::thread::spawn(|| {
            for index in 0..10_000 {
                record_request(
                    5,
                    200,
                    Duration::from_millis([750, 2_000, 3_000, 11_000][index % 4]),
                );
            }
        });
        for _ in 0..100 {
            let text = render(&RuntimeRealtimeMetrics::default(), 0, &[]);
            for group in GROUPS {
                let prefix = format!(
                    "backendrelay_http_request_duration_seconds_bucket{{surface=\"{group}\","
                );
                let buckets: Vec<u64> = text
                    .lines()
                    .filter(|line| line.starts_with(&prefix))
                    .map(|line| line.rsplit_once(' ').unwrap().1.parse().unwrap())
                    .collect();
                assert_eq!(buckets.len(), BUCKETS_US.len() + 1);
                assert!(buckets.windows(2).all(|pair| pair[0] <= pair[1]));
                let count_prefix = format!(
                    "backendrelay_http_request_duration_seconds_count{{surface=\"{group}\"}} "
                );
                let count: u64 = text
                    .lines()
                    .find_map(|line| line.strip_prefix(&count_prefix))
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(buckets.last().copied(), Some(count));
            }
        }
        writer.join().unwrap();
        assert!(REQUESTS[5].buckets[7].load(Ordering::Relaxed) >= 2500);
        assert!(REQUESTS[5].buckets[9].load(Ordering::Relaxed) >= 2500);
        assert!(REQUESTS[5].buckets[10].load(Ordering::Relaxed) >= 2500);
        assert!(REQUESTS[5].buckets[12].load(Ordering::Relaxed) >= 2500);
    }
}
