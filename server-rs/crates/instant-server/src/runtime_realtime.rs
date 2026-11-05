use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use futures_util::{FutureExt as _, StreamExt as _, stream::FuturesOrdered};
use instant_http::{
    AdminActor, AdminDataBackend, AdminRequestContext, AdminTransactRequest, ApiError,
    ApiErrorType, RuntimeDrainSignal, RuntimeFrameworkResponse, RuntimeRealtimeBackend,
    RuntimeRealtimeSession, RuntimeSsePushRequest, RuntimeTransport,
};
use instant_instaql::{
    Cardinality, InvalidationEntity, InvalidationPredicate, InvalidationPredicateOutcome,
    coerce_forms, compile_invalidation_predicate, normalized_query_hash,
};
use instant_realtime::{
    BroadcastPayload, ClientFeature, ClientFeatures, PeerPresence, PeerUser, PresenceDelivery,
    PresenceEdit, PresenceMessage, PresenceUpdate, QueryCacheBudget, QueryCacheError, QueryTopic,
    ResultCacheBudget, RoomKey, RoomStore, SessionTopicIndex, presence_message, sdk_session,
    topics_intersect,
};
use instant_wal::{
    EntitySnapshot, Isn, MaxSeenIsn, WalAction, WalCacheEviction, WalColumn, WalRecord,
    extract_entities_after, extract_entities_before, topics_for_record,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::db_error::{database_error, internal_error, query_validation_error};
use crate::metrics::RuntimeOpOutcome;
use crate::realtime_receive_dispatch::{
    GroupedReceiveDispatchConfig, GroupedReceiveDispatcher, TryEnqueueError,
};
use crate::realtime_receive_policy::{
    RealtimeReceiveExecutorPolicy, StaticRealtimeReceiveExecutorPolicy,
};
use crate::runtime_stream::{
    InMemoryRuntimeStreamBackend, RuntimeStreamAppendRequest, RuntimeStreamAppendResult,
    RuntimeStreamBackend, RuntimeStreamDisconnectRequest, RuntimeStreamLoadRequest,
    RuntimeStreamRecord, RuntimeStreamStartRequest,
};
use crate::statement_cancel::{
    StatementCancelWaitPolicy, StatementTracker, StaticStatementCancelWait,
};
use crate::{
    AdminDataAuthorizer, AllowAllAppRateLimits, AppRateLimitPolicy, AppStatus, AppStatusFuture,
    AppStatusSnapshot, EffectiveAppStatusPolicy,
};

mod cleanup;
mod query_cache;
mod refresh_admission;
mod session_registry;
mod sync_history;

use cleanup::ClusterCleanupCoordinator;
#[cfg(test)]
use cleanup::DEFAULT_CLUSTER_CLEANUP_CONCURRENCY;
pub use cleanup::RuntimeCleanupMetrics;
use query_cache::{
    RuntimeQueryCache, app_query_cache, clear_app_query_cache, data_invalidated_since,
    frame_epochs, invalidate_app_query_reuse, invalidate_app_root_frames, query_caches,
    query_epoch, query_epochs, result_cache_budget,
};
#[cfg(test)]
use query_cache::{query_cache_budget, session_query_result_weight};
use refresh_admission::RefreshAdmission;
use session_registry::SessionRegistry;
use sync_history::SyncHistoryState;

const DEFAULT_REFRESH_ACTIVE: NonZeroUsize = NonZeroUsize::new(32).expect("positive default");
pub(crate) const DEFAULT_REFRESH_WAITERS: NonZeroUsize =
    NonZeroUsize::new(16_384).expect("positive default");

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSyncSnapshotRequest {
    pub context: AdminRequestContext,
    pub query: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSyncSnapshot {
    pub join_rows: Vec<Value>,
    pub tx_id: i64,
    pub attribute_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSyncSubscriptionCreate {
    pub context: AdminRequestContext,
    pub subscription_id: Uuid,
    pub token: Uuid,
    pub query: Map<String, Value>,
    pub attribute_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSyncSubscriptionResume {
    pub context: AdminRequestContext,
    pub subscription_id: Uuid,
    pub token: Uuid,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSyncSubscription {
    pub app_id: Uuid,
    pub query: Map<String, Value>,
    pub attribute_ids: Vec<Uuid>,
}

/// Produces the authorized join-row snapshot used by the sync-table protocol.
pub trait RuntimeSyncBackend: Send + Sync + 'static {
    fn sync_snapshot(
        &self,
        request: RuntimeSyncSnapshotRequest,
    ) -> impl Future<Output = Result<RuntimeSyncSnapshot, ApiError>> + Send;

    fn create_sync_subscription(
        &self,
        request: RuntimeSyncSubscriptionCreate,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn delete_sync_subscription(
        &self,
        context: AdminRequestContext,
        subscription_id: Uuid,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn resume_sync_subscription(
        &self,
        request: RuntimeSyncSubscriptionResume,
    ) -> impl Future<Output = Result<RuntimeSyncSubscription, ApiError>> + Send;
}

/// Evaluates the application `$streams` rules before realtime stream access.
pub trait RuntimeStreamPermissionBackend: Send + Sync + 'static {
    fn authorize_stream_create(
        &self,
        context: AdminRequestContext,
        client_id: String,
        rule_params: Value,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn authorize_stream_view(
        &self,
        context: AdminRequestContext,
        stream: Value,
        rule_params: Value,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRealtimeQueryMode {
    Tree,
    JoinRows,
}

#[derive(Clone, Debug)]
pub struct RuntimeRealtimeBatchQuery {
    pub context: AdminRequestContext,
    pub query: Map<String, Value>,
    pub versions: Option<Value>,
    pub mode: RuntimeRealtimeQueryMode,
}

#[derive(Clone, Debug)]
pub struct RuntimeRealtimeBatchQueryResult {
    pub result: Value,
    pub result_meta: Value,
    pub byop_topics: Option<Vec<instant_pg::ByopTopic>>,
    pub instant_topics: Option<InstantQueryTopics>,
}

#[derive(Clone, Debug)]
pub struct RuntimeRealtimeRefreshBatch {
    /// Shared by every stale query in this refresh and by any frame loaders it
    /// starts, so one catalog copy serves the whole batch.
    pub catalog: Arc<instant_instaql::AttributeCatalog>,
    pub external_catalog: bool,
    /// Present when the backend executed the refresh after one fresh source and
    /// catalog resolution. Queries retain their own authorization/row snapshots.
    /// `None` leaves execution with the normal Instant path.
    pub results: Option<Vec<Result<RuntimeRealtimeBatchQueryResult, ApiError>>>,
}

/// Resolves the catalog exposed to an authorized realtime SDK session.
pub trait RuntimeRealtimeCatalogBackend: Send + Sync + 'static {
    fn refresh_realtime_batch_on_catalog(
        &self,
        app_id: Uuid,
        snapshot: crate::admin_data::RefreshCatalogSnapshot,
        queries: Vec<RuntimeRealtimeBatchQuery>,
    ) -> impl Future<Output = Result<RuntimeRealtimeRefreshBatch, ApiError>> + Send {
        let catalog =
            Arc::try_unwrap(snapshot.catalog).unwrap_or_else(|catalog| (*catalog).clone());
        self.refresh_realtime_batch(app_id, catalog, queries)
    }

    /// Resolves the catalog exposed to a realtime SDK session after normal
    /// Instant authorization and app-status checks have completed. The bool
    /// identifies an externally introspected BYOP catalog.
    fn resolve_realtime_catalog(
        &self,
        _app_id: Uuid,
        instant_catalog: instant_instaql::AttributeCatalog,
    ) -> impl Future<Output = Result<(instant_instaql::AttributeCatalog, bool), ApiError>> + Send
    {
        std::future::ready(Ok((instant_catalog, false)))
    }

    fn refresh_realtime_batch(
        &self,
        app_id: Uuid,
        instant_catalog: instant_instaql::AttributeCatalog,
        _queries: Vec<RuntimeRealtimeBatchQuery>,
    ) -> impl Future<Output = Result<RuntimeRealtimeRefreshBatch, ApiError>> + Send {
        async move {
            let (catalog, external_catalog) = self
                .resolve_realtime_catalog(app_id, instant_catalog)
                .await?;
            Ok(RuntimeRealtimeRefreshBatch {
                catalog: Arc::new(catalog),
                external_catalog,
                results: None,
            })
        }
    }
}

/// Resolves the public user fragment attached to presence and broadcast payloads.
pub trait RuntimeRealtimeIdentityBackend: Send + Sync + 'static {
    fn resolve_realtime_user_id(
        &self,
        context: AdminRequestContext,
    ) -> impl Future<Output = Result<Option<Uuid>, ApiError>> + Send;

    /// Optional same-init native catalog handoff, never an identity decision.
    /// Custom backends keep their existing resolver unless explicitly overridden.
    fn resolve_realtime_user_id_on_native_catalog(
        &self,
        context: AdminRequestContext,
        _catalog_app_id: Uuid,
        _catalog: instant_instaql::AttributeCatalog,
    ) -> impl Future<Output = Result<Option<Uuid>, ApiError>> + Send {
        self.resolve_realtime_user_id(context)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeTreeQueryResponse {
    pub result: Value,
    pub result_meta: Value,
    pub instant_topics: Option<InstantQueryTopics>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstantQueryTopics {
    pub precise: Vec<instant_datalog::InvalidationTopic>,
    pub coarse: Vec<instant_datalog::InvalidationTopic>,
}

/// Produces the object-tree representation and its root pagination/aggregate metadata.
pub trait RuntimeTreeQueryBackend: Send + Sync + 'static {
    fn query_tree(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
    ) -> impl Future<Output = Result<RuntimeTreeQueryResponse, ApiError>> + Send;

    fn query_raw(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        async move {
            self.query_tree(context, request)
                .await
                .map(|response| response.result)
        }
    }
}

/// Executes the Core join-row query protocol with the session actor's authorization.
pub trait RuntimeJoinRowsQueryBackend: Send + Sync + 'static {
    fn query_join_rows(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
    ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send;

    /// Reuses only the source/catalog freshly resolved for this refresh. The
    /// runtime calls this inside the normal cache/admission/cancellation loader,
    /// never as pre-executed native batch work. Backends may ignore the hint.
    fn query_join_rows_on_native_catalog(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
        _catalog_app_id: Uuid,
        _catalog: instant_instaql::AttributeCatalog,
    ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send {
        self.query_join_rows(context, request)
    }

    /// The session already owns its independently refreshed catalog. Backends
    /// may omit response attrs that this internal loader never consumes; custom
    /// backends retain their existing behavior by default.
    fn query_join_rows_without_attrs(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
    ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send {
        self.query_join_rows(context, request)
    }

    fn query_join_rows_on_native_catalog_without_attrs(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
        catalog_app_id: Uuid,
        catalog: instant_instaql::AttributeCatalog,
    ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send {
        self.query_join_rows_on_native_catalog(context, request, catalog_app_id, catalog)
    }

    /// Shared-catalog form of
    /// [`Self::query_join_rows_on_native_catalog_without_attrs`]. A refresh
    /// batch holds one catalog for every stale query; backends that can read it
    /// by reference avoid a deep clone per query. The default keeps the owned
    /// handoff for backends that require one.
    fn query_join_rows_on_native_catalog_without_attrs_shared(
        &self,
        context: AdminRequestContext,
        request: instant_http::AdminQueryRequest,
        catalog_app_id: Uuid,
        catalog: Arc<instant_instaql::AttributeCatalog>,
    ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send {
        let catalog = Arc::try_unwrap(catalog).unwrap_or_else(|catalog| (*catalog).clone());
        self.query_join_rows_on_native_catalog_without_attrs(
            context,
            request,
            catalog_app_id,
            catalog,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeRealtimeTransaction {
    pub tx_id: i64,
    pub fallback_isn: Isn,
}

/// Executes a realtime transaction while preserving its WAL fallback position.
pub trait RuntimeRealtimeTransactionBackend: Send + Sync + 'static {
    fn transact_realtime(
        &self,
        context: AdminRequestContext,
        request: AdminTransactRequest,
    ) -> impl Future<Output = Result<RuntimeRealtimeTransaction, ApiError>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLocalAppSessionReport {
    pub count: usize,
    pub origins: HashMap<String, usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RuntimeQueryHashCount {
    pub hash: i32,
    pub count: usize,
}

/// Local-process runtime diagnostics for one app.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct RuntimeAppRuntimeReport {
    pub sessions: usize,
    pub subscriptions_by_query_hash: Vec<RuntimeQueryHashCount>,
    pub oldest_refresh_wait_ms: u64,
}

/// Read-only reporting boundary over the local realtime session registry.
pub trait RuntimeSessionReporter: Send + Sync + 'static {
    fn machine_id(&self) -> Uuid;
    fn local_session_report(&self) -> HashMap<Uuid, RuntimeLocalAppSessionReport>;
    fn local_session_count(&self) -> usize;
    fn local_query_count(&self) -> usize;
}

pub type RealtimeClusterFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub type RefreshThrottleFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Duration>, ApiError>> + Send + 'a>>;
pub type SseRetryIntervalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Duration, ApiError>> + Send + 'a>>;
pub type DatalogCacheSettingsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DatalogCacheSettings, ApiError>> + Send + 'a>>;
pub type ReceiveTimeoutFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Duration, ApiError>> + Send + 'a>>;
pub type CombineTransactsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>>;

pub const DEFAULT_RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
// Coalesce only records already available to the poller; never wait to fill a window.
pub const MAX_WAL_REFRESH_BATCH: usize = 128;
// Bound unacknowledged windows and per-session completion waiters together.
pub const MAX_WAL_REFRESH_WINDOWS: usize = 32;
// Bound one session refresh so a stalled query cannot pin a WAL window.
const DEFAULT_WAL_SINK_REFRESH_DEADLINE_MS: usize = 10_000;
// A timed-out refresh retries its own session without waiting for another WAL
// invalidation; consecutive timeouts back off from one second to thirty.
const WAL_SINK_RESYNC_BACKOFF_MIN: Duration = Duration::from_secs(1);
const WAL_SINK_RESYNC_BACKOFF_MAX: Duration = Duration::from_secs(30);

fn wal_sink_resync_backoff(consecutive_timeouts: u32) -> Duration {
    let shift = consecutive_timeouts.saturating_sub(1).min(5);
    WAL_SINK_RESYNC_BACKOFF_MIN
        .saturating_mul(1u32 << shift)
        .min(WAL_SINK_RESYNC_BACKOFF_MAX)
}
const MAX_WAL_CATALOG_BATCH: usize = 32;

const CLUSTER_SUBSCRIBE_RETRY_MIN: Duration = Duration::from_millis(250);
const CLUSTER_SUBSCRIBE_RETRY_MAX: Duration = Duration::from_secs(5);
const CLUSTER_OWNER_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const SYNC_FRAME_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const SYNC_TRANSACTION_HISTORY_LIMIT: usize = 10_000;
const OUTBOUND_QUEUE_LIMIT: usize = 256;
const PRESENCE_PATCH_EDIT_LIMIT: usize = 128;
const PRESENCE_PATCH_BYTE_LIMIT: usize = 64 * 1024;
const PRESENCE_FULL_BYTE_LIMIT: usize = 1024 * 1024;
const TRANSACT_INBOX_LIMIT: usize = 128;
const REFRESH_INBOX_LIMIT: usize = 128;
const GROUPED_RECEIVE_INBOX_LIMIT: usize = 1_024;
const RECEIVE_EXECUTOR_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct RuntimeSessionSender {
    sender: mpsc::UnboundedSender<OutboundFrame>,
    state: Arc<OutboundQueueState>,
    presence: Arc<Mutex<OutboundPresenceState>>,
    disconnect: Arc<dyn Fn() + Send + Sync>,
}

enum OutboundFrame {
    Message {
        value: Value,
        delivered: Option<oneshot::Sender<OutboundDelivery>>,
    },
    PresenceWake {
        room_id: String,
    },
}

#[derive(Default)]
struct OutboundPresenceState {
    pending: HashMap<String, PendingPresence>,
}

enum PendingPresence {
    Patch {
        edits: Vec<PresenceEdit>,
        bytes: usize,
    },
    Full {
        data: Arc<BTreeMap<Uuid, PeerPresence>>,
        bytes: usize,
    },
}

impl PendingPresence {
    const fn bytes(&self) -> usize {
        match self {
            Self::Patch { bytes, .. } | Self::Full { bytes, .. } => *bytes,
        }
    }
}

#[derive(Default)]
struct OutboundQueueState {
    queued: AtomicUsize,
    closed: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundDelivery {
    Delivered,
    ConnectionClosed,
    TimedOut,
}

impl RuntimeSessionSender {
    fn disconnect_once(&self) {
        if !self.state.closed.swap(true, Ordering::AcqRel) {
            (self.disconnect)();
        }
    }

    fn enqueue_frame(&self, frame: OutboundFrame) -> Result<(), ()> {
        if self.state.closed.load(Ordering::Acquire) {
            return Err(());
        }
        if self
            .state
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < OUTBOUND_QUEUE_LIMIT).then_some(queued + 1)
            })
            .is_err()
        {
            self.disconnect_once();
            return Err(());
        }
        if self.sender.send(frame).is_err() {
            self.state.queued.fetch_sub(1, Ordering::AcqRel);
            self.disconnect_once();
            return Err(());
        }
        Ok(())
    }

    fn send(&self, value: Value) -> Result<(), ()> {
        self.enqueue_frame(OutboundFrame::Message {
            value,
            delivered: None,
        })
    }

    fn send_confirmed(
        &self,
        value: Value,
        delivered: oneshot::Sender<OutboundDelivery>,
    ) -> Result<(), ()> {
        self.enqueue_frame(OutboundFrame::Message {
            value,
            delivered: Some(delivered),
        })
    }

    fn send_presence(&self, features: &ClientFeatures, delivery: PresenceDelivery) {
        let Some(message) = presence_message(features, delivery.clone()) else {
            return;
        };
        let room_id = delivery.room.room_id;
        let pending = match message {
            PresenceMessage::PatchPresence { edits, .. } => {
                let bytes = serde_json::to_vec(&edits)
                    .expect("presence edits are JSON serializable")
                    .len();
                if edits.len() > PRESENCE_PATCH_EDIT_LIMIT || bytes > PRESENCE_PATCH_BYTE_LIMIT {
                    let bytes = serde_json::to_vec(&delivery.data)
                        .expect("presence data is JSON serializable")
                        .len();
                    PendingPresence::Full {
                        data: delivery.data.clone(),
                        bytes,
                    }
                } else {
                    PendingPresence::Patch { edits, bytes }
                }
            }
            PresenceMessage::RefreshPresence { data, .. } => {
                let bytes = serde_json::to_vec(&data)
                    .expect("presence data is JSON serializable")
                    .len();
                PendingPresence::Full { data, bytes }
            }
        };
        let enqueue_wake = {
            let mut state = self.presence.lock().unwrap();
            let previous = state.pending.remove(&room_id);
            let is_new_room = previous.is_none();
            let merged = match (previous, pending) {
                (
                    Some(PendingPresence::Patch {
                        edits: mut older,
                        bytes: older_bytes,
                    }),
                    PendingPresence::Patch {
                        edits: newer,
                        bytes: newer_bytes,
                    },
                ) if older.len().saturating_add(newer.len()) <= PRESENCE_PATCH_EDIT_LIMIT
                    && older_bytes.saturating_add(newer_bytes) <= PRESENCE_PATCH_BYTE_LIMIT =>
                {
                    older.extend(newer);
                    PendingPresence::Patch {
                        edits: older,
                        bytes: older_bytes.saturating_add(newer_bytes),
                    }
                }
                (Some(PendingPresence::Full { .. }), _)
                | (Some(PendingPresence::Patch { .. }), PendingPresence::Patch { .. }) => {
                    let bytes = serde_json::to_vec(&delivery.data)
                        .expect("presence data is JSON serializable")
                        .len();
                    PendingPresence::Full {
                        data: delivery.data,
                        bytes,
                    }
                }
                (_, replacement) => replacement,
            };
            if room_id.len().saturating_add(merged.bytes()) > PRESENCE_FULL_BYTE_LIMIT {
                drop(state);
                self.disconnect_once();
                return;
            }
            state.pending.insert(room_id.clone(), merged);
            is_new_room
        };
        if enqueue_wake
            && self
                .enqueue_frame(OutboundFrame::PresenceWake {
                    room_id: room_id.clone(),
                })
                .is_err()
        {
            let mut state = self.presence.lock().unwrap();
            state.pending.remove(&room_id);
        }
    }

    #[cfg(test)]
    fn try_send(&self, value: Value) -> Result<(), ()> {
        self.send(value)
    }
}

pub trait ReceiveTimeoutPolicy: Send + Sync + 'static {
    fn timeout(&self, app_id: Option<Uuid>) -> ReceiveTimeoutFuture<'_>;
}

/// Fresh limits for one receive handler, never retained across messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiveHandlerTimeouts {
    pub timeout: Duration,
    pub cancel_wait: Duration,
}

pub type ReceiveHandlerTimeoutsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReceiveHandlerTimeouts, ApiError>> + Send + 'a>>;

/// Explicit opt-in for resolving adjacent receive/cancellation controls together.
pub trait ReceiveHandlerTimeoutPolicy: Send + Sync + 'static {
    fn handler_timeouts(&self, app_id: Option<Uuid>) -> ReceiveHandlerTimeoutsFuture<'_>;
}

#[derive(Clone, Copy, Debug)]
pub struct StaticReceiveTimeout(pub Duration);

impl Default for StaticReceiveTimeout {
    fn default() -> Self {
        Self(DEFAULT_RECEIVE_TIMEOUT)
    }
}

impl ReceiveTimeoutPolicy for StaticReceiveTimeout {
    fn timeout(&self, _app_id: Option<Uuid>) -> ReceiveTimeoutFuture<'_> {
        Box::pin(std::future::ready(Ok(self.0)))
    }
}

/// Live policy controlling the conservative realtime transaction combiner.
pub trait CombineTransactsPolicy: Send + Sync + 'static {
    fn combine_transacts(&self, app_id: Uuid) -> CombineTransactsFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CombineTransactsByDefault;

impl CombineTransactsPolicy for CombineTransactsByDefault {
    fn combine_transacts(&self, _app_id: Uuid) -> CombineTransactsFuture<'_> {
        Box::pin(std::future::ready(Ok(true)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatalogCacheSettings {
    pub max_weight: u64,
    pub ttl: Duration,
    pub size_multiple: u64,
    /// Process-wide distinct loader limit. Zero disables the process-wide limit.
    pub process_max_in_flight: usize,
    /// Per-app distinct loader limit. Zero disables the per-app limit.
    pub max_in_flight: usize,
}

impl Default for DatalogCacheSettings {
    fn default() -> Self {
        Self {
            max_weight: 500 * 1024 * 1024,
            ttl: Duration::from_hours(1),
            size_multiple: 4,
            process_max_in_flight: 0,
            max_in_flight: 0,
        }
    }
}

pub trait DatalogCachePolicy: Send + Sync + 'static {
    fn settings(&self) -> DatalogCacheSettingsFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StaticDatalogCachePolicy(pub DatalogCacheSettings);

impl DatalogCachePolicy for StaticDatalogCachePolicy {
    fn settings(&self) -> DatalogCacheSettingsFuture<'_> {
        Box::pin(std::future::ready(Ok(self.0)))
    }
}

pub trait SseRetryIntervalPolicy: Send + Sync + 'static {
    fn retry_interval(&self) -> SseRetryIntervalFuture<'_>;
}

#[derive(Clone, Copy, Debug)]
pub struct StaticSseRetryInterval(pub Duration);

impl Default for StaticSseRetryInterval {
    fn default() -> Self {
        Self(Duration::from_millis(500))
    }
}

impl SseRetryIntervalPolicy for StaticSseRetryInterval {
    fn retry_interval(&self) -> SseRetryIntervalFuture<'_> {
        Box::pin(std::future::ready(Ok(self.0)))
    }
}

/// Resolves the delay applied to a session after a refresh has no visible
/// computations or attribute changes.
pub trait RefreshThrottlePolicy: Send + Sync + 'static {
    fn delay_after_noop(&self, app_id: Uuid) -> RefreshThrottleFuture<'_>;
}

pub type CoarseTopicsFuture<'a> = Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>>;

/// Chooses whether an app keeps conservative query-plan topics instead of the
/// more selective dependencies returned by query execution.
pub trait CoarseTopicsPolicy: Send + Sync + 'static {
    fn use_coarse_topics(&self, app_id: Uuid) -> CoarseTopicsFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PreciseQueryTopics;

impl CoarseTopicsPolicy for PreciseQueryTopics {
    fn use_coarse_topics(&self, _app_id: Uuid) -> CoarseTopicsFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoRefreshThrottle;

impl RefreshThrottlePolicy for NoRefreshThrottle {
    fn delay_after_noop(&self, _app_id: Uuid) -> RefreshThrottleFuture<'_> {
        Box::pin(std::future::ready(Ok(None)))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeRoomSnapshot {
    pub revision: i64,
    pub peers: BTreeMap<Uuid, PeerPresence>,
}

impl std::ops::Deref for RuntimeRoomSnapshot {
    type Target = BTreeMap<Uuid, PeerPresence>;

    fn deref(&self) -> &Self::Target {
        &self.peers
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeRealtimeClusterEvent {
    Presence {
        source_machine_id: Uuid,
        revision: i64,
        room: RoomKey,
        peers: BTreeMap<Uuid, PeerPresence>,
    },
    Broadcast {
        source_machine_id: Uuid,
        room: RoomKey,
        payload: BroadcastPayload,
    },
    StreamAppend {
        source_machine_id: Uuid,
        app_id: Uuid,
        append: RuntimeStreamAppendResult,
    },
    StreamWriterDisconnected {
        source_machine_id: Uuid,
        app_id: Uuid,
        stream_id: Uuid,
    },
    SsePush {
        target_machine_id: Uuid,
        app_id: Uuid,
        session_id: Uuid,
        messages: Vec<Value>,
    },
    Wal {
        source_machine_id: Uuid,
        record: WalRecord,
    },
    /// Consecutive durable WAL events acknowledged only after every refresh completes.
    WalBatch { records: Vec<WalRecord> },
    ByopWal {
        source_machine_id: Uuid,
        app_id: Uuid,
        record: instant_pg::ByopWalRecord,
        topics: Vec<instant_pg::ByopTopic>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RealtimeClusterConsumerKind {
    Runtime,
    Admin,
}

impl RealtimeClusterConsumerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Admin => "admin",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeRealtimeClusterDelivery {
    pub event_id: i64,
    pub event: RuntimeRealtimeClusterEvent,
}

/// Rust-native cross-machine room state and broadcast boundary.
pub trait RuntimeRealtimeCluster: Send + Sync + 'static {
    fn join<'a>(
        &'a self,
        room: &'a RoomKey,
        peer: PeerPresence,
    ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot>;

    fn set_presence<'a>(
        &'a self,
        room: &'a RoomKey,
        session_id: Uuid,
        data: Value,
    ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot>;

    fn leave<'a>(
        &'a self,
        room: &'a RoomKey,
        session_id: Uuid,
    ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot>;

    fn publish_broadcast<'a>(
        &'a self,
        room: &'a RoomKey,
        payload: BroadcastPayload,
    ) -> RealtimeClusterFuture<'a, ()>;

    fn publish_stream_append(
        &self,
        app_id: Uuid,
        append: RuntimeStreamAppendResult,
    ) -> RealtimeClusterFuture<'_, ()>;

    fn publish_stream_disconnect(
        &self,
        app_id: Uuid,
        stream_id: Uuid,
    ) -> RealtimeClusterFuture<'_, ()>;

    fn register_sse_session(
        &self,
        app_id: Uuid,
        session_id: Uuid,
        token_hash: [u8; 32],
    ) -> RealtimeClusterFuture<'_, ()>;

    fn unregister_sse_session(&self, session_id: Uuid) -> RealtimeClusterFuture<'_, ()>;

    fn forward_sse_push(&self, request: RuntimeSsePushRequest) -> RealtimeClusterFuture<'_, bool>;

    fn publish_wal(&self, record: WalRecord) -> RealtimeClusterFuture<'_, ()>;

    fn publish_byop_wal(
        &self,
        app_id: Uuid,
        record: instant_pg::ByopWalRecord,
        topics: Vec<instant_pg::ByopTopic>,
    ) -> RealtimeClusterFuture<'_, ()>;

    fn subscribe(
        &self,
        consumer: RealtimeClusterConsumerKind,
    ) -> RealtimeClusterFuture<'_, broadcast::Receiver<RuntimeRealtimeClusterDelivery>>;

    fn acknowledge(
        &self,
        consumer: RealtimeClusterConsumerKind,
        event_id: i64,
    ) -> RealtimeClusterFuture<'_, ()>;
}

pub(crate) async fn await_cluster_while_owned<T, F>(owner: &Weak<T>, future: F) -> Option<F::Output>
where
    F: Future,
{
    tokio::pin!(future);
    loop {
        // Immediate retries can win the select before the periodic ownership
        // check. Check before polling too, so shutdown cannot keep retrying.
        if owner.strong_count() == 0 {
            return None;
        }
        tokio::select! {
            output = &mut future => return Some(output),
            () = tokio::time::sleep(CLUSTER_OWNER_CHECK_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod cluster_test_support {
    use super::*;

    pub(crate) struct FailFirstSubscribeCluster {
        attempts: AtomicUsize,
        fail_first: bool,
        close_receivers: bool,
        fail_acknowledgements: AtomicBool,
        pub(crate) fail_wal_publication: AtomicBool,
        acknowledgements: Mutex<Vec<(RealtimeClusterConsumerKind, i64)>>,
        events: broadcast::Sender<RuntimeRealtimeClusterDelivery>,
    }

    impl FailFirstSubscribeCluster {
        pub(crate) fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                fail_first: true,
                close_receivers: false,
                fail_acknowledgements: AtomicBool::new(false),
                fail_wal_publication: AtomicBool::new(false),
                acknowledgements: Mutex::new(Vec::new()),
                events: broadcast::channel(8).0,
            }
        }

        pub(crate) fn closed() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                fail_first: false,
                close_receivers: true,
                fail_acknowledgements: AtomicBool::new(false),
                fail_wal_publication: AtomicBool::new(false),
                acknowledgements: Mutex::new(Vec::new()),
                events: broadcast::channel(8).0,
            }
        }

        pub(crate) fn fail_acknowledgements(&self) {
            self.fail_acknowledgements.store(true, Ordering::Release);
        }

        pub(crate) fn attempts(&self) -> usize {
            self.attempts.load(Ordering::Acquire)
        }

        pub(crate) fn send(&self, delivery: RuntimeRealtimeClusterDelivery) {
            self.events.send(delivery).expect("consumer is subscribed");
        }

        pub(crate) fn acknowledged(
            &self,
            consumer: RealtimeClusterConsumerKind,
            event_id: i64,
        ) -> bool {
            self.acknowledgements
                .lock()
                .unwrap()
                .contains(&(consumer, event_id))
        }
    }

    impl RuntimeRealtimeCluster for FailFirstSubscribeCluster {
        fn join<'a>(
            &'a self,
            _room: &'a RoomKey,
            _peer: PeerPresence,
        ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
            Box::pin(std::future::ready(Ok(RuntimeRoomSnapshot {
                revision: 0,
                peers: BTreeMap::new(),
            })))
        }

        fn set_presence<'a>(
            &'a self,
            _room: &'a RoomKey,
            _session_id: Uuid,
            _data: Value,
        ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
            Box::pin(std::future::ready(Ok(RuntimeRoomSnapshot {
                revision: 0,
                peers: BTreeMap::new(),
            })))
        }

        fn leave<'a>(
            &'a self,
            _room: &'a RoomKey,
            _session_id: Uuid,
        ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
            Box::pin(std::future::ready(Ok(RuntimeRoomSnapshot {
                revision: 0,
                peers: BTreeMap::new(),
            })))
        }

        fn publish_broadcast<'a>(
            &'a self,
            _room: &'a RoomKey,
            _payload: BroadcastPayload,
        ) -> RealtimeClusterFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn publish_stream_append(
            &self,
            _app_id: Uuid,
            _append: RuntimeStreamAppendResult,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn publish_stream_disconnect(
            &self,
            _app_id: Uuid,
            _stream_id: Uuid,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn register_sse_session(
            &self,
            _app_id: Uuid,
            _session_id: Uuid,
            _token_hash: [u8; 32],
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn unregister_sse_session(&self, _session_id: Uuid) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn forward_sse_push(
            &self,
            _request: RuntimeSsePushRequest,
        ) -> RealtimeClusterFuture<'_, bool> {
            Box::pin(std::future::ready(Ok(false)))
        }

        fn publish_wal(&self, _record: WalRecord) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(
                if self.fail_wal_publication.load(Ordering::Acquire) {
                    Err("injected WAL publication failure".to_owned())
                } else {
                    Ok(())
                },
            ))
        }

        fn publish_byop_wal(
            &self,
            _app_id: Uuid,
            _record: instant_pg::ByopWalRecord,
            _topics: Vec<instant_pg::ByopTopic>,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn subscribe(
            &self,
            _consumer: RealtimeClusterConsumerKind,
        ) -> RealtimeClusterFuture<'_, broadcast::Receiver<RuntimeRealtimeClusterDelivery>>
        {
            let attempt = self.attempts.fetch_add(1, Ordering::AcqRel);
            Box::pin(std::future::ready(if self.fail_first && attempt == 0 {
                Err("fail-first subscribe".to_owned())
            } else if self.close_receivers {
                let (sender, receiver) = broadcast::channel(1);
                drop(sender);
                Ok(receiver)
            } else {
                Ok(self.events.subscribe())
            }))
        }

        fn acknowledge(
            &self,
            consumer: RealtimeClusterConsumerKind,
            event_id: i64,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(async move {
                self.acknowledgements
                    .lock()
                    .unwrap()
                    .push((consumer, event_id));
                if self.fail_acknowledgements.load(Ordering::Acquire) {
                    Err("persistent acknowledgement failure".to_owned())
                } else {
                    Ok(())
                }
            })
        }
    }
}

pub struct RuntimeRealtimeService<A, D> {
    authorizer: Arc<A>,
    data: Arc<D>,
    stream_backend: Arc<dyn RuntimeStreamBackend>,
    cluster: Option<Arc<dyn RuntimeRealtimeCluster>>,
    app_rate_limits: Arc<dyn AppRateLimitPolicy>,
    app_status: Arc<dyn EffectiveAppStatusPolicy>,
    refresh_throttle: Arc<dyn RefreshThrottlePolicy>,
    coarse_topics: Arc<dyn CoarseTopicsPolicy>,
    refresh_read_controls: Option<Arc<dyn crate::RefreshReadControlsPolicy>>,
    sse_retry_interval: Arc<dyn SseRetryIntervalPolicy>,
    datalog_cache: Arc<dyn DatalogCachePolicy>,
    receive_timeout: Arc<dyn ReceiveTimeoutPolicy>,
    receive_handler_timeouts: Option<Arc<dyn ReceiveHandlerTimeoutPolicy>>,
    combine_transacts: Arc<dyn CombineTransactsPolicy>,
    statement_cancel_wait: Arc<dyn StatementCancelWaitPolicy>,
    receive_executor: Arc<dyn RealtimeReceiveExecutorPolicy>,
    receive_dispatcher: Arc<OnceLock<RuntimeReceiveDispatcher>>,
    join_room_logger: Arc<dyn crate::JoinRoomLogger>,
    inner: Arc<RealtimeInner>,
    transaction_isn_wait: Duration,
    admission_limits: RuntimeAdmissionLimits,
    wal_sink_refresh_deadline: Duration,
}

impl<A, D> Clone for RuntimeRealtimeService<A, D> {
    fn clone(&self) -> Self {
        Self {
            authorizer: Arc::clone(&self.authorizer),
            data: Arc::clone(&self.data),
            stream_backend: Arc::clone(&self.stream_backend),
            cluster: self.cluster.as_ref().map(Arc::clone),
            app_rate_limits: Arc::clone(&self.app_rate_limits),
            app_status: Arc::clone(&self.app_status),
            refresh_throttle: Arc::clone(&self.refresh_throttle),
            coarse_topics: Arc::clone(&self.coarse_topics),
            refresh_read_controls: self.refresh_read_controls.as_ref().map(Arc::clone),
            sse_retry_interval: Arc::clone(&self.sse_retry_interval),
            datalog_cache: Arc::clone(&self.datalog_cache),
            receive_timeout: Arc::clone(&self.receive_timeout),
            receive_handler_timeouts: self.receive_handler_timeouts.as_ref().map(Arc::clone),
            combine_transacts: Arc::clone(&self.combine_transacts),
            statement_cancel_wait: Arc::clone(&self.statement_cancel_wait),
            receive_executor: Arc::clone(&self.receive_executor),
            receive_dispatcher: Arc::clone(&self.receive_dispatcher),
            join_room_logger: Arc::clone(&self.join_room_logger),
            inner: Arc::clone(&self.inner),
            transaction_isn_wait: self.transaction_isn_wait,
            admission_limits: self.admission_limits,
            wal_sink_refresh_deadline: self.wal_sink_refresh_deadline,
        }
    }
}

#[derive(Debug, Default)]
struct ActiveEffectiveAppStatusPolicy;

impl EffectiveAppStatusPolicy for ActiveEffectiveAppStatusPolicy {
    fn status(&self, _app_id: Uuid) -> AppStatusFuture<'_> {
        Box::pin(std::future::ready(Ok(AppStatusSnapshot {
            own_status: AppStatus::Active,
            effective_status: AppStatus::Active,
        })))
    }
}

struct RealtimeInner {
    machine_id: Uuid,
    /// Set when the process begins terminating. New transports are refused and
    /// every existing session observes its own per-session signal.
    drain: RuntimeDrainSignal,
    #[cfg(test)]
    last_invalidation_candidates: AtomicUsize,
    sessions: Mutex<SessionRegistry>,
    subscription_index: Mutex<SessionTopicIndex>,
    refresh_admission: Arc<RefreshAdmission>,
    rooms: Arc<Mutex<RoomStore>>,
    sync: Mutex<SyncState>,
    sync_history: Arc<RuntimeSyncHistory>,
    streams: Mutex<StreamState>,
    processed: Mutex<HashMap<Uuid, ProcessedWatermark>>,
    watermark_seed: Arc<RealtimeWatermarkSeed>,
    query_caches: Mutex<HashMap<Uuid, RuntimeQueryCache>>,
    query_epochs: Arc<instant_realtime::AppInvalidationEpochs>,
    // Frames keep a separate epoch registry so WAL invalidation can record and
    // retire them before taking the session registry. See `epoch` and
    // `query_cache` module docs for the fence and ordering.
    frame_epochs: Arc<instant_realtime::AppInvalidationEpochs>,
    root_frame_caches: Option<Arc<crate::root_frame_cache::RootFrameCaches>>,
    // Last digested refresh catalog per app. Equality with the fresh catalog is
    // checked on every use, so this only skips re-encoding identical metadata.
    catalog_digests: Mutex<HashMap<Uuid, (Arc<instant_instaql::AttributeCatalog>, [u8; 32])>>,
    query_cache_budget: Mutex<Option<Arc<QueryCacheBudget>>>,
    result_cache_budget: Mutex<Option<Arc<ResultCacheBudget>>>,
    query_admission_rejections: AtomicU64,
    transaction_admission_rejections: AtomicU64,
    transaction_isns: TransactionIsnTracker,
    cluster_deliveries: ClusterDeliveryTracker,
    local_broadcasts: Mutex<HashMap<Uuid, usize>>,
    cleanup: Arc<ClusterCleanupCoordinator>,
    next_ungrouped_receive: AtomicU64,
}

/// Admission caps for the public realtime transport.
///
/// Environment variables provide deployment defaults; config-app flags with the
/// same names minus the `INSTANT_` prefix and with `_` replaced by `-` may
/// override them when the process starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeAdmissionLimits {
    pub sessions_per_ip: usize,
    pub sessions_per_app: usize,
    pub subscriptions_per_session: usize,
    pub query_json_bytes: usize,
}

impl Default for RuntimeAdmissionLimits {
    fn default() -> Self {
        Self {
            sessions_per_ip: 100,
            sessions_per_app: 10_000,
            subscriptions_per_session: 500,
            query_json_bytes: 256 * 1024,
        }
    }
}

impl RuntimeAdmissionLimits {
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            sessions_per_ip: env_usize(
                "INSTANT_MAX_LIVE_SESSIONS_PER_IP",
                defaults.sessions_per_ip,
            ),
            sessions_per_app: env_usize("INSTANT_MAX_SESSIONS_PER_APP", defaults.sessions_per_app),
            subscriptions_per_session: env_usize(
                "INSTANT_MAX_SUBSCRIPTIONS_PER_SESSION",
                defaults.subscriptions_per_session,
            ),
            query_json_bytes: env_usize(
                "INSTANT_MAX_INSTAQL_QUERY_BYTES",
                defaults.query_json_bytes,
            ),
        }
    }

    /// Applies a positive integer config-app flag. Malformed or non-positive
    /// values are ignored so a bad flag cannot disable every live session.
    pub fn apply_config_flag(&mut self, name: &str, value: &Value) -> bool {
        let Some(limit) = value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
        else {
            return false;
        };
        match name {
            "max-live-sessions-per-ip" => self.sessions_per_ip = limit,
            "max-sessions-per-app" => self.sessions_per_app = limit,
            "max-subscriptions-per-session" => self.subscriptions_per_session = limit,
            "max-instaql-query-bytes" => self.query_json_bytes = limit,
            _ => return false,
        }
        true
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Fixed-cardinality process aggregates for the private metrics listener.
///
/// Gauges are sampled across independently locked resources, rather than one
/// atomic snapshot. Cache weight is the configured estimate, not resident memory.
/// No query text, credentials or payloads are included; app ids appear only in
/// the bounded per-app series rendered by the metrics listener.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeRealtimeMetrics {
    pub sessions: usize,
    pub subscriptions: usize,
    pub cache_apps: usize,
    pub cache_entries: usize,
    pub cache_weight_bytes: u64,
    pub cache_in_flight: usize,
    pub cache_retired_loads: usize,
    pub result_cache_bytes: u64,
    pub result_cache_max_bytes: u64,
    pub result_cache_evictions_total: u64,
    pub root_frame_cache_entries: usize,
    pub root_frame_cache_bytes: u64,
    pub root_frame_cache_hits_total: u64,
    pub root_frame_cache_misses_total: u64,
    pub root_frame_cache_loads_total: u64,
    pub root_frame_cache_retire_scanned_total: u64,
    pub root_frame_cache_retire_retired_total: u64,
    pub root_frame_cache_retire_nanos_total: u64,
    pub root_frame_cache_expired_total: u64,
    pub outbound_queued: usize,
    pub transact_queued: usize,
    pub refresh_queued: usize,
    pub refresh_active: usize,
    pub refresh_waiting: usize,
    pub refresh_waiting_apps: usize,
    pub refresh_admission_rejections_total: u64,
    pub receive_queued: usize,
    pub receive_active_groups: usize,
    pub receive_workers: usize,
    pub receive_oldest_wait_ms: u64,
    pub query_admission_rejections_total: u64,
    pub transaction_admission_rejections_total: u64,
    /// Realtime sessions per resolved app, ordered by app id.
    pub app_sessions: Vec<(Uuid, u64)>,
    /// Realtime sessions per SDK family and normalized version.
    pub sdk_sessions: Vec<(String, String, u64)>,
    pub cleanup: RuntimeCleanupMetrics,
}

/// Suppresses a replay after application succeeds but before the durable
/// acknowledgement retry completes. Cross-process delivery remains
/// at-least-once because client-visible effects live in process memory.
#[derive(Default)]
pub(crate) struct ClusterDeliveryTracker {
    applied_through: AtomicI64,
}

impl ClusterDeliveryTracker {
    pub(crate) fn should_apply(&self, event_id: i64) -> bool {
        event_id > self.applied_through.load(Ordering::Acquire)
    }

    pub(crate) fn mark_applied(&self, event_id: i64) {
        self.applied_through.fetch_max(event_id, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessedWatermark {
    pub(crate) tx_id: Option<u64>,
    pub(crate) isn: Option<Isn>,
}

impl ProcessedWatermark {
    pub(crate) fn advance(&mut self, tx_id: u64, isn: Option<Isn>) {
        self.tx_id = Some(self.tx_id.map_or(tx_id, |current| current.max(tx_id)));
        if let Some(isn) = isn {
            self.isn = Some(self.isn.map_or(isn, |current| current.max(isn)));
        }
    }
}

/// Process-global maxima copied into an app's realtime state the first time it
/// is observed. This mirrors the JVM store's `create-conn` baseline without
/// conflating it with later app-scoped invalidations.
#[derive(Debug, Default)]
pub struct RealtimeWatermarkSeed {
    max_tx_id: Mutex<Option<u64>>,
    max_seen_isn: MaxSeenIsn,
    has_seen_isn: AtomicBool,
}

impl RealtimeWatermarkSeed {
    #[must_use]
    pub fn initialized(max_tx_id: u64, max_seen_isn: Isn) -> Self {
        let seed = Self::default();
        *seed
            .max_tx_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(max_tx_id);
        seed.max_seen_isn.set(max_seen_isn);
        seed.has_seen_isn.store(true, Ordering::Release);
        seed
    }

    pub(crate) fn snapshot(&self) -> ProcessedWatermark {
        let tx_id = *self
            .max_tx_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let isn = self
            .has_seen_isn
            .load(Ordering::Acquire)
            .then(|| self.max_seen_isn.get());
        ProcessedWatermark { tx_id, isn }
    }

    pub(crate) fn observe_tx_id(&self, tx_id: i64) {
        let Ok(tx_id) = u64::try_from(tx_id) else {
            return;
        };
        let mut maximum = self
            .max_tx_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *maximum = Some(maximum.map_or(tx_id, |current| current.max(tx_id)));
    }

    pub(crate) fn observe_isn(&self, isn: Isn) {
        self.max_seen_isn.advance(isn);
        self.has_seen_isn.store(true, Ordering::Release);
    }
}

const RECENT_TRANSACTION_ISN_LIMIT: usize = 1_024;
pub(crate) const DEFAULT_TRANSACTION_ISN_WAIT: Duration = Duration::from_millis(2_500);

#[derive(Default)]
struct TransactionIsnState {
    recent: HashMap<i64, Isn>,
    recent_order: VecDeque<i64>,
    next_waiter_id: u64,
    waiters: HashMap<i64, Vec<(u64, oneshot::Sender<Isn>)>>,
}

#[derive(Default)]
pub(crate) struct TransactionIsnTracker {
    state: Mutex<TransactionIsnState>,
}

#[derive(Debug)]
enum RefreshReadControlsFailure {
    Status(ApiError),
    Topics(ApiError),
}

impl TransactionIsnTracker {
    pub(crate) fn deliver(&self, tx_id: i64, isn: Isn) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.recent.insert(tx_id, isn).is_none() {
            state.recent_order.push_back(tx_id);
        }
        while state.recent_order.len() > RECENT_TRANSACTION_ISN_LIMIT {
            if let Some(expired) = state.recent_order.pop_front() {
                state.recent.remove(&expired);
            }
        }
        if let Some(waiters) = state.waiters.remove(&tx_id) {
            for (_, waiter) in waiters {
                let _receiver_was_dropped = waiter.send(isn);
            }
        }
    }

    pub(crate) async fn wait(&self, tx_id: i64, duration: Duration) -> Option<Isn> {
        let (waiter_id, receiver) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(isn) = state.recent.get(&tx_id) {
                return Some(*isn);
            }
            let (sender, receiver) = oneshot::channel();
            let waiter_id = state.next_waiter_id;
            state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
            state
                .waiters
                .entry(tx_id)
                .or_default()
                .push((waiter_id, sender));
            (waiter_id, receiver)
        };
        let _registration = TransactionIsnWaitRegistration {
            tracker: self,
            tx_id,
            waiter_id,
        };
        tokio::time::timeout(duration, receiver)
            .await
            .ok()
            .and_then(Result::ok)
    }
}

struct TransactionIsnWaitRegistration<'a> {
    tracker: &'a TransactionIsnTracker,
    tx_id: i64,
    waiter_id: u64,
}

impl Drop for TransactionIsnWaitRegistration<'_> {
    fn drop(&mut self) {
        let mut state = self
            .tracker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(waiters) = state.waiters.get_mut(&self.tx_id) {
            waiters.retain(|(waiter_id, _)| *waiter_id != self.waiter_id);
            if waiters.is_empty() {
                state.waiters.remove(&self.tx_id);
            }
        }
    }
}

struct RuntimeSessionEntry {
    app_id: Option<Uuid>,
    origin: Option<String>,
    client_ip: Option<IpAddr>,
    actor: AdminActor,
    bearer_token: Option<String>,
    versions: Option<Value>,
    client_features: ClientFeatures,
    token_hash: Option<[u8; 32]>,
    user_id: Option<Uuid>,
    sender: RuntimeSessionSender,
    /// Per-session close signal, tripped by `drain_all_connections` so the
    /// transport can send a close frame or end its stream itself.
    drain: RuntimeDrainSignal,
    queries: HashMap<String, RegisteredQuery>,
    refresh_not_before: Option<tokio::time::Instant>,
    transact_sender: mpsc::Sender<PendingTransaction>,
    transact_worker: Arc<TransactionWorkerControl>,
    refresh_sender: mpsc::Sender<PendingRefresh>,
    refresh_worker: Arc<TransactionWorkerControl>,
    next_refresh_generation: u64,
    refresh_attrs_generation: u64,
    refresh_force_generation: u64,
    /// Consecutive refresh deadline hits; drives the forced-resync backoff and
    /// resets on the next successful refresh.
    wal_sink_consecutive_timeouts: u32,
    external_catalog: bool,
    catalog_digest: Option<[u8; 32]>,
    active_handlers: Arc<ActiveStatementHandlers>,
}

/// Request-local header snapshot. Ordinary operations must be independent of
/// how many subscriptions/results the session owns. Refresh selects only stale
/// queries under the same registry lock instead of cloning the whole entry.
#[derive(Clone)]
struct SessionSnapshot {
    app_id: Option<Uuid>,
    actor: AdminActor,
    bearer_token: Option<String>,
    client_ip: Option<IpAddr>,
    versions: Option<Value>,
    user_id: Option<Uuid>,
    sender: RuntimeSessionSender,
    transact_sender: mpsc::Sender<PendingTransaction>,
    refresh_attrs_generation: u64,
    refresh_force_generation: u64,
    external_catalog: bool,
    catalog_digest: Option<[u8; 32]>,
}

impl SessionSnapshot {
    fn from_entry(entry: &RuntimeSessionEntry) -> Self {
        Self {
            app_id: entry.app_id,
            actor: entry.actor.clone(),
            bearer_token: entry.bearer_token.clone(),
            client_ip: entry.client_ip,
            versions: entry.versions.clone(),
            user_id: entry.user_id,
            sender: entry.sender.clone(),
            transact_sender: entry.transact_sender.clone(),
            refresh_attrs_generation: entry.refresh_attrs_generation,
            refresh_force_generation: entry.refresh_force_generation,
            external_catalog: entry.external_catalog,
            catalog_digest: entry.catalog_digest,
        }
    }
}

#[derive(Clone)]
struct ActiveStatementHandler {
    tracker: StatementTracker,
    cancel_wait: Duration,
    disconnect: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct ActiveStatementHandlers {
    next_generation: AtomicU64,
    handlers: Mutex<HashMap<u64, ActiveStatementHandler>>,
}

impl ActiveStatementHandlers {
    fn register(
        self: &Arc<Self>,
        tracker: StatementTracker,
        cancel_wait: Duration,
        disconnect: tokio::sync::watch::Sender<bool>,
    ) -> ActiveStatementRegistration {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        self.handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                generation,
                ActiveStatementHandler {
                    tracker,
                    cancel_wait,
                    disconnect,
                },
            );
        ActiveStatementRegistration {
            handlers: Arc::clone(self),
            generation,
        }
    }

    fn snapshot(&self) -> Vec<ActiveStatementHandler> {
        self.handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
}

struct ActiveStatementRegistration {
    handlers: Arc<ActiveStatementHandlers>,
    generation: u64,
}

impl Drop for ActiveStatementRegistration {
    fn drop(&mut self) {
        self.handlers
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.generation);
    }
}

enum ScopedHandlerOutcome<T> {
    Completed(T),
    TimedOut,
    Disconnected,
}

fn api_error_outcome(error: &ApiError) -> RuntimeOpOutcome {
    if error.body().kind == ApiErrorType::RateLimited {
        RuntimeOpOutcome::RateLimited
    } else {
        RuntimeOpOutcome::Error
    }
}

fn result_outcome<T>(result: &Result<T, ApiError>) -> RuntimeOpOutcome {
    match result {
        Ok(_) => RuntimeOpOutcome::Ok,
        Err(error) => api_error_outcome(error),
    }
}

/// Classifies the point where a queued transaction's client-visible result is
/// produced. `Disconnected` sends no frame but is still not a success.
fn transaction_outcome(result: &ScopedHandlerOutcome<Result<Value, ApiError>>) -> RuntimeOpOutcome {
    match result {
        ScopedHandlerOutcome::Completed(Ok(_)) => RuntimeOpOutcome::Ok,
        ScopedHandlerOutcome::Completed(Err(error)) => api_error_outcome(error),
        ScopedHandlerOutcome::TimedOut | ScopedHandlerOutcome::Disconnected => {
            RuntimeOpOutcome::Error
        }
    }
}

async fn run_scoped_handler<F>(
    handlers: &Arc<ActiveStatementHandlers>,
    receive_timeout: Duration,
    cancel_wait: Duration,
    future: F,
) -> ScopedHandlerOutcome<F::Output>
where
    F: Future,
{
    let tracker = StatementTracker::default();
    let (disconnect, mut disconnected) = tokio::sync::watch::channel(false);
    let _registration = handlers.register(tracker.clone(), cancel_wait, disconnect);
    let scoped = tracker.scope(future);
    tokio::pin!(scoped);
    let timeout = tokio::time::sleep(receive_timeout);
    tokio::pin!(timeout);
    let outcome = tokio::select! {
        result = &mut scoped => return ScopedHandlerOutcome::Completed(result),
        () = &mut timeout => ScopedHandlerOutcome::TimedOut,
        _ = disconnected.changed() => ScopedHandlerOutcome::Disconnected,
    };

    let cancellation = tracker.cancel_and_wait(cancel_wait);
    tokio::pin!(cancellation);
    tokio::select! {
        _ = &mut cancellation => {}
        _ = &mut scoped => {
            let _outcome = cancellation.await;
        }
    }
    outcome
}

async fn run_disconnect_scoped_handler<F>(
    handlers: &Arc<ActiveStatementHandlers>,
    cancel_wait: Duration,
    future: F,
) -> Option<F::Output>
where
    F: Future,
{
    let tracker = StatementTracker::default();
    let (disconnect, mut disconnected) = tokio::sync::watch::channel(false);
    let _registration = handlers.register(tracker.clone(), cancel_wait, disconnect);
    let scoped = tracker.scope(future);
    tokio::pin!(scoped);
    tokio::select! {
        result = &mut scoped => return Some(result),
        _ = disconnected.changed() => {}
    }
    let cancellation = tracker.cancel_and_wait(cancel_wait);
    tokio::pin!(cancellation);
    tokio::select! {
        _ = &mut cancellation => {}
        _ = &mut scoped => {
            let _outcome = cancellation.await;
        }
    }
    None
}

#[derive(Clone, Debug)]
struct PendingTransaction {
    original: Value,
    steps: Vec<Value>,
    redundant: Vec<Value>,
    /// Captured when the message was received so the worker can record the
    /// full queue wait plus execution time at completion.
    started: Instant,
}

struct PendingRefresh {
    processed_tx_id: Value,
    processed_isn: Value,
    // Durable windows carry their cluster event id for deadline diagnostics.
    window_id: Option<i64>,
    completions: Vec<RefreshCompletion>,
}

impl PendingRefresh {
    fn watermark(&self) -> ProcessedWatermark {
        ProcessedWatermark {
            tx_id: self.processed_tx_id.as_u64(),
            isn: self
                .processed_isn
                .as_str()
                .and_then(|value| value.parse().ok()),
        }
    }

    fn set_watermark(&mut self, watermark: ProcessedWatermark) {
        self.processed_tx_id = json!(watermark.tx_id);
        self.processed_isn = json!(watermark.isn);
    }

    fn coalesce(&mut self, next: Self) {
        let current = self.watermark();
        let incoming = next.watermark();
        // Catch-up requests may arrive after a newer WAL request. Unknown lag
        // boundaries dominate this batch; known boundaries never move backward.
        self.set_watermark(ProcessedWatermark {
            tx_id: current.tx_id.zip(incoming.tx_id).map(|(a, b)| a.max(b)),
            isn: current.isn.zip(incoming.isn).map(|(a, b)| a.max(b)),
        });
        self.window_id = next.window_id.or(self.window_id);
        self.completions.extend(next.completions);
    }

    fn preserve_completed_watermark(&mut self, completed: ProcessedWatermark) {
        let current = self.watermark();
        self.set_watermark(ProcessedWatermark {
            tx_id: current
                .tx_id
                .map(|id| completed.tx_id.map_or(id, |prior| prior.max(id))),
            isn: current
                .isn
                .map(|isn| completed.isn.map_or(isn, |prior| prior.max(isn))),
        });
    }
}

enum RefreshCompletion {
    Once(oneshot::Sender<Result<(), ApiError>>),
    // Durable windows must retry in the session worker before later watermarks.
    Durable(oneshot::Sender<()>),
}

struct PreparedWalBatch {
    refreshes: BTreeMap<Uuid, (i64, Isn)>,
    selected_refreshes: usize,
    records: usize,
    started: tokio::time::Instant,
}

type WalCompletion = Pin<Box<dyn Future<Output = ()> + Send>>;
type OrderedWalCompletions = FuturesOrdered<Pin<Box<dyn Future<Output = i64> + Send>>>;

#[expect(
    clippy::large_enum_variant,
    reason = "This transient step stays in the consumer future; boxing would allocate on every delivery."
)]
enum RuntimeConsumerStep {
    Delivery(Result<RuntimeRealtimeClusterDelivery, broadcast::error::RecvError>),
    Completed(i64),
}

fn can_pipeline_wal(event: &RuntimeRealtimeClusterEvent) -> bool {
    let ordinary =
        |record: &WalRecord| record.attr_changes.is_empty() && !wal_schema_refresh(record);
    match event {
        RuntimeRealtimeClusterEvent::Wal { record, .. } => ordinary(record),
        RuntimeRealtimeClusterEvent::WalBatch { records } => {
            records.len() <= MAX_WAL_REFRESH_BATCH && records.iter().all(ordinary)
        }
        _ => false,
    }
}

async fn next_runtime_consumer_step(
    events: &mut broadcast::Receiver<RuntimeRealtimeClusterDelivery>,
    pending: &mut OrderedWalCompletions,
) -> RuntimeConsumerStep {
    tokio::select! {
        biased;
        Some(event_id) = pending.next(), if !pending.is_empty() => {
            RuntimeConsumerStep::Completed(event_id)
        }
        received = events.recv(), if pending.len() < MAX_WAL_REFRESH_WINDOWS => {
            RuntimeConsumerStep::Delivery(received)
        }
    }
}

async fn complete_runtime_delivery(
    owner: &Weak<RealtimeInner>,
    cluster: &dyn RuntimeRealtimeCluster,
    event_id: i64,
) -> bool {
    let Some(inner) = owner.upgrade() else {
        return false;
    };
    inner.cluster_deliveries.mark_applied(event_id);
    drop(inner);
    loop {
        let Some(acknowledgement) = await_cluster_while_owned(
            owner,
            cluster.acknowledge(RealtimeClusterConsumerKind::Runtime, event_id),
        )
        .await
        else {
            return false;
        };
        if acknowledgement.is_ok() {
            return true;
        }
        if await_cluster_while_owned(owner, tokio::time::sleep(CLUSTER_SUBSCRIBE_RETRY_MIN))
            .await
            .is_none()
        {
            return false;
        }
    }
}

#[derive(Default)]
struct TransactionWorkerControl {
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl TransactionWorkerControl {
    fn install(&self, abort: tokio::task::AbortHandle) {
        *self
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(abort);
    }

    fn abort(&self) {
        if let Some(abort) = self
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            abort.abort();
        }
    }
}

#[derive(Clone)]
struct RegisteredQuery {
    query: Map<String, Value>,
    return_type: QueryReturnType,
    byop_topics: Option<Vec<instant_pg::ByopTopic>>,
    instant_topics: Option<Vec<QueryTopic>>,
    invalidation_predicate: Option<InvalidationPredicate>,
    last_result: CachedQueryResult,
    stale_generation: u64,
}

#[derive(Clone)]
struct SessionQueryResult {
    result: Arc<Value>,
    result_meta: Arc<Value>,
    byop_topics: Option<Vec<instant_pg::ByopTopic>>,
    instant_topics: Option<InstantQueryTopics>,
}

fn join_rows_session_result(
    response: RuntimeFrameworkResponse,
) -> Result<SessionQueryResult, ApiError> {
    let byop_topics = response
        .byop_topics
        .map(|topics| {
            topics
                .into_iter()
                .map(|topic| serde_json::from_value(topic).map_err(internal_error))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(SessionQueryResult {
        result: Arc::new(response.result),
        result_meta: Arc::new(json!({})),
        byop_topics,
        instant_topics: response.instant_topics.map(instant_query_topics),
    })
}

#[derive(Clone, Debug, PartialEq)]
struct CachedQueryResult {
    result: Arc<Value>,
    result_meta: Arc<Value>,
}

impl CachedQueryResult {
    fn from_execution(execution: &SessionQueryResult) -> Self {
        Self {
            result: Arc::clone(&execution.result),
            result_meta: Arc::clone(&execution.result_meta),
        }
    }
}

/// Detached query work must not retain the session's subscriptions, cached
/// results, outbound channels or worker controls. Authentication and versions
/// are still captured at the same boundary and used for every fresh execution.
#[derive(Clone)]
struct SessionQueryContext {
    request: AdminRequestContext,
    versions: Option<Value>,
}

impl SessionQueryContext {
    fn from_entry(entry: &SessionSnapshot) -> Self {
        let app_id = entry.app_id.expect("initialized entry has app id");
        Self {
            request: entry_context(entry, app_id),
            versions: entry.versions.clone(),
        }
    }
}

/// Exact key used by the grouped realtime receive queue. JSON-bearing key
/// parts are recursively key-sorted before serialization so object insertion
/// order cannot change group identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RealtimeGroupKey {
    Transact(Uuid),
    Room(Uuid, String),
    Query(Uuid, String),
    Refresh(Uuid),
    RefreshPresence(Uuid, String),
    RefreshSyncTable(String),
    Error(Uuid),
    AppendStream(Uuid, String),
    Init(Uuid),
    SseInit(Uuid),
    Sync(Uuid, String),
    ModifySync(String),
    StartStream(Uuid, String),
    SubscribeStream(Uuid, String),
    UnsubscribeStream(Uuid, String),
    Ungrouped(Uuid, u64),
}

#[derive(Clone, Debug)]
struct GroupedReceiveMessage {
    session_id: Uuid,
    message: Value,
}

type RuntimeReceiveDispatcher =
    GroupedReceiveDispatcher<RealtimeGroupKey, GroupedReceiveMessage, (), Infallible>;

fn serialized_group_part(event: &Value, field: &str) -> String {
    fn canonicalize(value: &Value) -> Value {
        match value {
            Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
            Value::Object(values) => {
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), canonicalize(&values[key])))
                        .collect(),
                )
            }
            scalar => scalar.clone(),
        }
    }

    serde_json::to_string(&canonicalize(event.get(field).unwrap_or(&Value::Null)))
        .expect("realtime events contain serializable JSON")
}

fn realtime_group_key(
    session_id: Uuid,
    event: &Value,
    ungrouped_sequence: u64,
) -> RealtimeGroupKey {
    let part = |field| serialized_group_part(event, field);
    match event.get("op").and_then(Value::as_str) {
        Some("transact") => RealtimeGroupKey::Transact(session_id),
        Some(
            "join-room" | "leave-room" | "set-presence" | "client-broadcast" | "server-broadcast",
        ) => RealtimeGroupKey::Room(session_id, part("room-id")),
        Some("add-query" | "remove-query") => RealtimeGroupKey::Query(session_id, part("q")),
        Some("refresh") => RealtimeGroupKey::Refresh(session_id),
        Some("refresh-presence") => RealtimeGroupKey::RefreshPresence(session_id, part("room-id")),
        Some("refresh-sync-table") => RealtimeGroupKey::RefreshSyncTable(part("subscription-id")),
        Some("error") => RealtimeGroupKey::Error(session_id),
        Some("append-stream") => RealtimeGroupKey::AppendStream(session_id, part("stream-id")),
        Some("init") => RealtimeGroupKey::Init(session_id),
        Some("sse-init") => RealtimeGroupKey::SseInit(session_id),
        Some("start-sync") => RealtimeGroupKey::Sync(session_id, part("q")),
        Some("remove-sync" | "resync-table") => {
            RealtimeGroupKey::ModifySync(part("subscription-id"))
        }
        Some("start-stream") => RealtimeGroupKey::StartStream(session_id, part("client-id")),
        Some("subscribe-stream") => RealtimeGroupKey::SubscribeStream(
            session_id,
            if event
                .get("stream-id")
                .is_some_and(|stream_id| !stream_id.is_null() && stream_id != &Value::Bool(false))
            {
                part("stream-id")
            } else {
                part("client-id")
            },
        ),
        Some("unsubscribe-stream") => {
            RealtimeGroupKey::UnsubscribeStream(session_id, part("subscribe-event-id"))
        }
        _ => RealtimeGroupKey::Ungrouped(session_id, ungrouped_sequence),
    }
}

const GROUPED_BROADCAST_PAYLOADS: &str = "__grouped-server-broadcast-payloads";

fn combine_grouped_receive_events(first: &Value, second: &Value) -> Option<Value> {
    let pair = (
        first.get("op").and_then(Value::as_str),
        second.get("op").and_then(Value::as_str),
    );
    match pair {
        (Some("refresh"), Some("refresh"))
        | (Some("set-presence"), Some("set-presence"))
        | (Some("refresh-sync-table"), Some("refresh-sync-table")) => Some(second.clone()),
        (Some("refresh-presence"), Some("refresh-presence")) => {
            let mut combined = second.clone();
            let mut edits = first
                .get("edits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            edits.extend(
                second
                    .get("edits")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            combined
                .as_object_mut()?
                .insert("edits".to_owned(), Value::Array(edits));
            Some(combined)
        }
        (Some("server-broadcast"), Some("server-broadcast")) => {
            Some(combine_server_broadcasts(first, second)?)
        }
        (Some("append-stream"), Some("append-stream")) => {
            let mut combined = second.clone();
            let mut chunks = first
                .get("chunks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            chunks.extend(
                second
                    .get("chunks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            let combined = combined.as_object_mut()?;
            combined.insert("chunks".to_owned(), Value::Array(chunks));
            combined.insert(
                "offset".to_owned(),
                first.get("offset").cloned().unwrap_or(Value::Null),
            );
            Some(Value::Object(combined.clone()))
        }
        _ => None,
    }
}

fn combine_server_broadcasts(first: &Value, second: &Value) -> Option<Value> {
    if let Some(payloads) = first
        .get(GROUPED_BROADCAST_PAYLOADS)
        .and_then(Value::as_array)
    {
        let mut combined = first.clone();
        let mut payloads = payloads.clone();
        payloads.push(json!({
            "topic": second.get("topic").cloned().unwrap_or(Value::Null),
            "data": second.get("data").cloned().unwrap_or(Value::Null),
        }));
        combined.as_object_mut()?.insert(
            GROUPED_BROADCAST_PAYLOADS.to_owned(),
            Value::Array(payloads),
        );
        return Some(combined);
    }
    let mut combined = second.clone();
    let combined = combined.as_object_mut()?;
    combined.remove("topic");
    combined.remove("data");
    combined.insert(
        GROUPED_BROADCAST_PAYLOADS.to_owned(),
        json!([
            {
                "topic": first.get("topic").cloned().unwrap_or(Value::Null),
                "data": first.get("data").cloned().unwrap_or(Value::Null),
            },
            {
                "topic": second.get("topic").cloned().unwrap_or(Value::Null),
                "data": second.get("data").cloned().unwrap_or(Value::Null),
            }
        ]),
    );
    Some(Value::Object(combined.clone()))
}

pub(crate) fn instant_query_topics(
    topics: instant_http::RuntimeFrameworkQueryTopics,
) -> InstantQueryTopics {
    InstantQueryTopics {
        precise: topics.precise,
        coarse: topics.coarse,
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SharedQueryKey {
    actor: SharedQueryActor,
    query: Arc<str>,
    return_type: QueryReturnType,
    versions: Arc<str>,
}

impl SharedQueryKey {
    fn new(
        entry: &SessionSnapshot,
        query: &Map<String, Value>,
        return_type: QueryReturnType,
    ) -> Self {
        Self {
            actor: SharedQueryActor::from(&entry.actor),
            query: canonical_query(query).into(),
            return_type,
            versions: serde_json::to_string(&entry.versions)
                .expect("SDK versions are JSON serializable")
                .into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SharedQueryActor {
    Admin,
    Guest,
    RefreshToken(Uuid),
    Email(Arc<str>),
}

impl From<&AdminActor> for SharedQueryActor {
    fn from(actor: &AdminActor) -> Self {
        match actor {
            AdminActor::Admin => Self::Admin,
            AdminActor::Guest => Self::Guest,
            AdminActor::RefreshToken(token) => Self::RefreshToken(*token),
            AdminActor::Email(email) => Self::Email(email.as_str().into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
enum QueryReturnType {
    #[default]
    JoinRows,
    Tree,
}

impl QueryReturnType {
    fn from_message(message: &Value) -> Self {
        if message.get("return-type").and_then(Value::as_str) == Some("tree") {
            Self::Tree
        } else {
            Self::JoinRows
        }
    }
}

#[derive(Clone)]
struct ActiveSyncSubscription {
    session: Uuid,
    app: Uuid,
    subscription: Uuid,
    sent_tx: i64,
    attribute_ids: Vec<Uuid>,
    /// Linked queries and permissions need a full query evaluation after every
    /// transaction; raw triple deltas cannot safely express their visibility.
    reset_on_change: bool,
    ready: bool,
    draining: bool,
}

struct StartSyncGuard<D: RuntimeSyncBackend> {
    inner: Arc<RealtimeInner>,
    data: Arc<D>,
    stream_backend: Arc<dyn RuntimeStreamBackend>,
    cluster: Option<Arc<dyn RuntimeRealtimeCluster>>,
    context: AdminRequestContext,
    session_id: Uuid,
    subscription_id: Uuid,
    create_task: Option<tokio::task::JoinHandle<Result<(), ApiError>>>,
    durable: bool,
    armed: bool,
}

impl<D: RuntimeSyncBackend> StartSyncGuard<D> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<D: RuntimeSyncBackend> Drop for StartSyncGuard<D> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.durable || self.create_task.is_some() {
            sync_state(&self.inner)
                .stale_subscriptions
                .insert(self.subscription_id, self.context.app_id);
        }
        cleanup_runtime_session(
            &self.inner,
            &self.stream_backend,
            self.cluster.as_ref(),
            self.session_id,
        );
        let data = Arc::clone(&self.data);
        let inner = Arc::clone(&self.inner);
        let context = self.context.clone();
        let subscription_id = self.subscription_id;
        let create_task = self.create_task.take();
        let durable = self.durable;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let should_delete = if let Some(create_task) = create_task {
                    matches!(create_task.await, Ok(Ok(())))
                } else {
                    durable
                };
                if !should_delete {
                    let mut state = sync_state(&inner);
                    if state.stale_subscriptions.get(&subscription_id) == Some(&context.app_id) {
                        state.stale_subscriptions.remove(&subscription_id);
                    }
                    return;
                }
                if let Err(error) = RuntimeSyncBackend::delete_sync_subscription(
                    data.as_ref(),
                    context,
                    subscription_id,
                )
                .await
                {
                    tracing::warn!(
                        %subscription_id,
                        error = %error.body().message,
                        "failed to clean durable sync subscription after initial delivery failure"
                    );
                }
            });
        }
    }
}

#[derive(Clone)]
struct SyncTransaction {
    app_id: Uuid,
    tx_id: i64,
    changes: Vec<Value>,
}

/// Process-lifetime sync-table transaction history shared across realtime
/// service rebuilds. Durable subscriptions can therefore catch up after an
/// in-process database failover has replaced the HTTP/realtime route tree.
#[derive(Default)]
pub struct RuntimeSyncHistory {
    state: Mutex<SyncHistoryState>,
}

impl RuntimeSyncHistory {
    pub(crate) fn record(&self, app_id: Uuid, tx_id: i64, changes: Vec<Value>) {
        let mut history = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        record_sync_history(
            &mut history,
            SyncTransaction {
                app_id,
                tx_id,
                changes,
            },
            SYNC_TRANSACTION_HISTORY_LIMIT,
        );
    }

    pub(crate) fn transactions_after(
        &self,
        app_id: Uuid,
        sent_tx: i64,
        attribute_ids: &[Uuid],
    ) -> Result<(Option<i64>, Vec<Value>), ()> {
        let history = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if history
            .evicted_through
            .get(&app_id)
            .is_some_and(|evicted_tx| sent_tx < *evicted_tx)
        {
            return Err(());
        }
        let latest_tx = history
            .after(app_id, sent_tx)
            .map(|transaction| transaction.tx_id)
            .max();
        let transactions = history
            .after(app_id, sent_tx)
            .filter_map(|transaction| {
                let changes = transaction
                    .changes
                    .iter()
                    .filter(|change| sync_change_matches(change, attribute_ids))
                    .cloned()
                    .collect::<Vec<_>>();
                (!changes.is_empty())
                    .then(|| json!({"tx-id": transaction.tx_id, "changes": changes}))
            })
            .collect();
        Ok((latest_tx, transactions))
    }
}

#[derive(Default)]
struct SyncState {
    active: HashMap<(Uuid, Uuid), ActiveSyncSubscription>,
    stale_subscriptions: HashMap<Uuid, Uuid>,
}

#[derive(Clone)]
struct StreamReader {
    session: Uuid,
    stream: Uuid,
    event: String,
}

#[derive(Default)]
struct StreamState {
    writers: HashMap<Uuid, Uuid>,
    readers: HashMap<(Uuid, String), StreamReader>,
}

impl<A, D> RuntimeRealtimeService<A, D> {
    /// Local-process session and subscription diagnostics for one app. Query
    /// hashes follow the same normalized form used for config overrides.
    #[must_use]
    pub fn app_runtime_report(&self, app_id: Uuid) -> RuntimeAppRuntimeReport {
        const TOP_QUERY_HASHES: usize = 50;
        let (sessions, subscriptions_by_query_hash) = {
            let entries = sessions(&self.inner);
            let session_ids = entries.ids_for_app(app_id).collect::<Vec<_>>();
            let mut counts = HashMap::new();
            for session_id in &session_ids {
                let Some(entry) = entries.get(session_id) else {
                    continue;
                };
                for query in entry.queries.values() {
                    let mut query = query.query.clone();
                    query.remove("$$ruleParams");
                    if let Ok(hash) = normalized_query_hash(&Value::Object(query)) {
                        *counts.entry(hash).or_insert(0_usize) += 1;
                    }
                }
            }
            let mut counts = counts
                .into_iter()
                .map(|(hash, count)| RuntimeQueryHashCount { hash, count })
                .collect::<Vec<_>>();
            counts.sort_by(|left, right| {
                right
                    .count
                    .cmp(&left.count)
                    .then_with(|| left.hash.cmp(&right.hash))
            });
            counts.truncate(TOP_QUERY_HASHES);
            (session_ids.len(), counts)
        };
        let oldest_refresh_wait_ms = self
            .inner
            .refresh_admission
            .oldest_wait(app_id)
            .map_or(0, |wait| {
                u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)
            });
        RuntimeAppRuntimeReport {
            sessions,
            subscriptions_by_query_hash,
            oldest_refresh_wait_ms,
        }
    }
}

impl<A, D> RuntimeRealtimeService<A, D>
where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeJoinRowsQueryBackend
        + RuntimeTreeQueryBackend
        + RuntimeSyncBackend
        + RuntimeStreamPermissionBackend
        + RuntimeRealtimeCatalogBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend,
{
    #[must_use]
    pub fn new(authorizer: Arc<A>, data: Arc<D>, machine_id: Uuid) -> Self {
        let receive_dispatcher = Arc::new(OnceLock::new());
        Self {
            authorizer,
            data,
            stream_backend: Arc::new(InMemoryRuntimeStreamBackend::default()),
            cluster: None,
            app_rate_limits: Arc::new(AllowAllAppRateLimits),
            app_status: Arc::new(ActiveEffectiveAppStatusPolicy),
            refresh_throttle: Arc::new(NoRefreshThrottle),
            coarse_topics: Arc::new(PreciseQueryTopics),
            refresh_read_controls: None,
            sse_retry_interval: Arc::new(StaticSseRetryInterval::default()),
            datalog_cache: Arc::new(StaticDatalogCachePolicy::default()),
            receive_timeout: Arc::new(StaticReceiveTimeout::default()),
            receive_handler_timeouts: None,
            combine_transacts: Arc::new(CombineTransactsByDefault),
            statement_cancel_wait: Arc::new(StaticStatementCancelWait::default()),
            receive_executor: Arc::new(StaticRealtimeReceiveExecutorPolicy::default()),
            receive_dispatcher: Arc::clone(&receive_dispatcher),
            join_room_logger: Arc::new(crate::NoopJoinRoomLogger),
            transaction_isn_wait: DEFAULT_TRANSACTION_ISN_WAIT,
            admission_limits: RuntimeAdmissionLimits::from_env(),
            wal_sink_refresh_deadline: Duration::from_millis(
                u64::try_from(env_usize(
                    "INSTANT_WAL_SINK_REFRESH_DEADLINE_MS",
                    DEFAULT_WAL_SINK_REFRESH_DEADLINE_MS,
                ))
                .unwrap_or(u64::MAX),
            ),
            inner: Arc::new(RealtimeInner {
                machine_id,
                drain: RuntimeDrainSignal::default(),
                #[cfg(test)]
                last_invalidation_candidates: AtomicUsize::new(0),
                sessions: Mutex::new(SessionRegistry::default()),
                subscription_index: Mutex::new(SessionTopicIndex::default()),
                refresh_admission: Arc::new(RefreshAdmission::new(
                    DEFAULT_REFRESH_ACTIVE,
                    DEFAULT_REFRESH_WAITERS,
                )),
                rooms: Arc::new(Mutex::new(RoomStore::default())),
                sync: Mutex::new(SyncState::default()),
                sync_history: Arc::new(RuntimeSyncHistory::default()),
                streams: Mutex::new(StreamState::default()),
                processed: Mutex::new(HashMap::new()),
                watermark_seed: Arc::new(RealtimeWatermarkSeed::default()),
                query_caches: Mutex::new(HashMap::new()),
                query_epochs: Arc::new(instant_realtime::AppInvalidationEpochs::new(
                    instant_realtime::DATA_INVALIDATION_LOG_LIMIT,
                )),
                frame_epochs: Arc::new(instant_realtime::AppInvalidationEpochs::new(
                    instant_realtime::DATA_INVALIDATION_LOG_LIMIT,
                )),
                root_frame_caches: None,
                catalog_digests: Mutex::new(HashMap::new()),
                query_cache_budget: Mutex::new(None),
                result_cache_budget: Mutex::new(None),
                query_admission_rejections: AtomicU64::new(0),
                transaction_admission_rejections: AtomicU64::new(0),
                transaction_isns: TransactionIsnTracker::default(),
                cluster_deliveries: ClusterDeliveryTracker::default(),
                local_broadcasts: Mutex::new(HashMap::new()),
                cleanup: Arc::new(ClusterCleanupCoordinator::default()),
                next_ungrouped_receive: AtomicU64::new(0),
            }),
        }
    }

    /// Shares the startup/global transaction and ISN maxima with other
    /// realtime transports in this process.
    ///
    /// # Panics
    ///
    /// Panics when called after the service's internal state has been cloned.
    #[must_use]
    pub fn with_watermark_seed(mut self, seed: Arc<RealtimeWatermarkSeed>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("watermark seed must be configured before cloning the service")
            .watermark_seed = seed;
        self
    }

    /// Shares the bounded sync-table catch-up window across service rebuilds.
    /// Active subscriptions and sessions remain local to each service.
    ///
    /// # Panics
    ///
    /// Panics when called after the service's internal state has been cloned.
    #[must_use]
    pub fn with_sync_history(mut self, history: Arc<RuntimeSyncHistory>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("sync history must be configured before cloning the service")
            .sync_history = history;
        self
    }

    #[must_use]
    pub fn with_stream_backend(mut self, backend: Arc<dyn RuntimeStreamBackend>) -> Self {
        self.stream_backend = backend;
        self
    }

    #[must_use]
    pub fn with_transaction_isn_wait(mut self, duration: Duration) -> Self {
        self.transaction_isn_wait = duration;
        self
    }

    /// Bounds one session refresh inside a WAL window. A refresh that exceeds
    /// the deadline fails and reports an error to its session; the window's
    /// other refreshes still complete.
    #[must_use]
    pub fn with_wal_sink_refresh_deadline(mut self, deadline: Duration) -> Self {
        self.wal_sink_refresh_deadline = deadline;
        self
    }

    #[must_use]
    pub const fn with_admission_limits(mut self, limits: RuntimeAdmissionLimits) -> Self {
        self.admission_limits = limits;
        self
    }

    /// Bounds expensive session refreshes, with FIFO per app and fair app turns.
    /// Configure before sharing the service. This never changes database pools.
    ///
    /// # Panics
    /// Panics if this service has already been cloned/shared.
    #[must_use]
    pub fn with_refresh_admission(
        mut self,
        max_active: NonZeroUsize,
        max_waiting: NonZeroUsize,
    ) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("refresh admission must be configured before cloning the service")
            .refresh_admission = Arc::new(RefreshAdmission::new(max_active, max_waiting));
        self
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn with_cluster(mut self, cluster: Arc<dyn RuntimeRealtimeCluster>) -> Self {
        let owner = Arc::downgrade(&self.inner);
        let authorizer = Arc::clone(&self.authorizer);
        let data = Arc::clone(&self.data);
        let stream_backend = Arc::clone(&self.stream_backend);
        let app_rate_limits = Arc::clone(&self.app_rate_limits);
        let app_status = Arc::clone(&self.app_status);
        let refresh_throttle = Arc::clone(&self.refresh_throttle);
        let coarse_topics = Arc::clone(&self.coarse_topics);
        let refresh_read_controls = self.refresh_read_controls.as_ref().map(Arc::clone);
        let sse_retry_interval = Arc::clone(&self.sse_retry_interval);
        let datalog_cache = Arc::clone(&self.datalog_cache);
        let receive_timeout = Arc::clone(&self.receive_timeout);
        let receive_handler_timeouts = self.receive_handler_timeouts.as_ref().map(Arc::clone);
        let combine_transacts = Arc::clone(&self.combine_transacts);
        let statement_cancel_wait = Arc::clone(&self.statement_cancel_wait);
        let receive_executor = Arc::clone(&self.receive_executor);
        let receive_dispatcher = Arc::clone(&self.receive_dispatcher);
        let join_room_logger = Arc::clone(&self.join_room_logger);
        let transaction_isn_wait = self.transaction_isn_wait;
        let admission_limits = self.admission_limits;
        let wal_sink_refresh_deadline = self.wal_sink_refresh_deadline;
        let event_cluster = Arc::clone(&cluster);
        tokio::spawn(async move {
            let mut retry_delay = CLUSTER_SUBSCRIBE_RETRY_MIN;
            loop {
                if owner.upgrade().is_none() {
                    break;
                }
                let Some(subscription) = await_cluster_while_owned(
                    &owner,
                    event_cluster.subscribe(RealtimeClusterConsumerKind::Runtime),
                )
                .await
                else {
                    return;
                };
                let Ok(mut events) = subscription else {
                    if await_cluster_while_owned(&owner, tokio::time::sleep(retry_delay))
                        .await
                        .is_none()
                    {
                        return;
                    }
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
                    continue;
                };
                let mut pending = OrderedWalCompletions::new();
                loop {
                    let Some(step) = await_cluster_while_owned(
                        &owner,
                        next_runtime_consumer_step(&mut events, &mut pending),
                    )
                    .await
                    else {
                        return;
                    };
                    let received = match step {
                        RuntimeConsumerStep::Completed(event_id) => {
                            if !complete_runtime_delivery(&owner, event_cluster.as_ref(), event_id)
                                .await
                            {
                                return;
                            }
                            continue;
                        }
                        RuntimeConsumerStep::Delivery(received) => received,
                    };
                    let delivery = match received {
                        Ok(delivery) => Some(delivery),
                        Err(broadcast::error::RecvError::Lagged(_)) => None,
                        Err(broadcast::error::RecvError::Closed) => {
                            if await_cluster_while_owned(&owner, tokio::time::sleep(retry_delay))
                                .await
                                .is_none()
                            {
                                return;
                            }
                            retry_delay = retry_delay
                                .saturating_mul(2)
                                .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
                            break;
                        }
                    };
                    // Schema, policy, presence, streams and recovery retain their
                    // ordered boundary. Only ordinary WAL windows overlap.
                    if !delivery
                        .as_ref()
                        .is_some_and(|delivery| can_pipeline_wal(&delivery.event))
                    {
                        while let Some(event_id) = pending.next().await {
                            if !complete_runtime_delivery(&owner, event_cluster.as_ref(), event_id)
                                .await
                            {
                                return;
                            }
                        }
                    }
                    let Some(inner) = owner.upgrade() else {
                        return;
                    };
                    let service = RuntimeRealtimeService {
                        authorizer: Arc::clone(&authorizer),
                        data: Arc::clone(&data),
                        stream_backend: Arc::clone(&stream_backend),
                        cluster: Some(Arc::clone(&event_cluster)),
                        app_rate_limits: Arc::clone(&app_rate_limits),
                        app_status: Arc::clone(&app_status),
                        refresh_throttle: Arc::clone(&refresh_throttle),
                        coarse_topics: Arc::clone(&coarse_topics),
                        refresh_read_controls: refresh_read_controls.as_ref().map(Arc::clone),
                        sse_retry_interval: Arc::clone(&sse_retry_interval),
                        datalog_cache: Arc::clone(&datalog_cache),
                        receive_timeout: Arc::clone(&receive_timeout),
                        receive_handler_timeouts: receive_handler_timeouts.as_ref().map(Arc::clone),
                        combine_transacts: Arc::clone(&combine_transacts),
                        statement_cancel_wait: Arc::clone(&statement_cancel_wait),
                        receive_executor: Arc::clone(&receive_executor),
                        receive_dispatcher: Arc::clone(&receive_dispatcher),
                        join_room_logger: Arc::clone(&join_room_logger),
                        inner,
                        transaction_isn_wait,
                        admission_limits,
                        wal_sink_refresh_deadline,
                    };
                    if let Some(delivery) = delivery {
                        let event_id = delivery.event_id;
                        if can_pipeline_wal(&delivery.event) {
                            if service.inner.cluster_deliveries.should_apply(event_id) {
                                let records = match delivery.event {
                                    RuntimeRealtimeClusterEvent::Wal { record, .. } => vec![record],
                                    RuntimeRealtimeClusterEvent::WalBatch { records } => records,
                                    _ => unreachable!("pipeline classification only accepts WAL"),
                                };
                                let completion = loop {
                                    match service.schedule_wal_batch(&records, event_id).await {
                                        Ok(completion) => break completion,
                                        Err(_) => {
                                            tokio::time::sleep(CLUSTER_SUBSCRIBE_RETRY_MIN).await;
                                        }
                                    }
                                };
                                pending.push_back(Box::pin(async move {
                                    completion.await;
                                    event_id
                                }));
                            } else {
                                pending.push_back(Box::pin(std::future::ready(event_id)));
                            }
                            tracing::debug!(
                                target: "instant_server::realtime_latency",
                                event_id,
                                pending_windows = pending.len(),
                                "realtime WAL window scheduled"
                            );
                        } else {
                            crate::postgres_pool::DatabaseWorkload::Refresh
                                .scope(deliver_cluster_delivery(&service, delivery))
                                .await;
                            drop(service);
                            if !complete_runtime_delivery(&owner, event_cluster.as_ref(), event_id)
                                .await
                            {
                                return;
                            }
                        }
                    } else {
                        deliver_stream_rate_limit(&service.inner);
                        service.recover_cluster_lag().await;
                    }
                    retry_delay = CLUSTER_SUBSCRIBE_RETRY_MIN;
                }
            }
        });
        self.cluster = Some(cluster);
        self
    }

    #[must_use]
    pub fn with_app_rate_limit_policy(mut self, policy: Arc<dyn AppRateLimitPolicy>) -> Self {
        self.app_rate_limits = policy;
        self
    }

    #[must_use]
    pub fn with_effective_app_status_policy(
        mut self,
        policy: Arc<dyn EffectiveAppStatusPolicy>,
    ) -> Self {
        self.app_status = policy;
        self.refresh_read_controls = None;
        self
    }

    #[must_use]
    pub fn with_refresh_throttle_policy(mut self, policy: Arc<dyn RefreshThrottlePolicy>) -> Self {
        self.refresh_throttle = policy;
        self
    }

    #[must_use]
    pub fn with_coarse_topics_policy(mut self, policy: Arc<dyn CoarseTopicsPolicy>) -> Self {
        self.coarse_topics = policy;
        self.refresh_read_controls = None;
        self
    }

    /// Combines only the adjacent fresh refresh status/topic reads. Custom
    /// replacements of either standalone policy restore the sequential path.
    #[must_use]
    pub fn with_refresh_read_controls_policy(
        mut self,
        policy: Arc<dyn crate::RefreshReadControlsPolicy>,
    ) -> Self {
        self.refresh_read_controls = Some(policy);
        self
    }

    async fn read_refresh_controls(
        &self,
        app_id: Uuid,
        has_stale_queries: bool,
    ) -> Result<bool, RefreshReadControlsFailure> {
        if let Some(policy) = &self.refresh_read_controls {
            let controls = match policy.read(app_id, has_stale_queries).await {
                Ok(controls) => controls,
                Err(error) => {
                    // A combined SQL failure has no reliable status/topic
                    // attribution. Re-read only to preserve the original error
                    // priority and topic retry path. Even if both reads now
                    // succeed, never turn the failed combined read into success.
                    self.read_separate_refresh_controls(app_id, has_stale_queries)
                        .await?;
                    return Err(RefreshReadControlsFailure::Status(error));
                }
            };
            controls
                .status
                .effective_status
                .assert_read_allowed()
                .map_err(RefreshReadControlsFailure::Status)?;
            return Ok(has_stale_queries && controls.use_coarse_topics);
        }
        self.read_separate_refresh_controls(app_id, has_stale_queries)
            .await
    }

    async fn read_separate_refresh_controls(
        &self,
        app_id: Uuid,
        has_stale_queries: bool,
    ) -> Result<bool, RefreshReadControlsFailure> {
        self.app_status
            .status(app_id)
            .await
            .map_err(RefreshReadControlsFailure::Status)?
            .effective_status
            .assert_read_allowed()
            .map_err(RefreshReadControlsFailure::Status)?;
        if has_stale_queries {
            self.coarse_topics
                .use_coarse_topics(app_id)
                .await
                .map_err(RefreshReadControlsFailure::Topics)
        } else {
            Ok(false)
        }
    }

    #[must_use]
    pub fn with_sse_retry_interval_policy(
        mut self,
        policy: Arc<dyn SseRetryIntervalPolicy>,
    ) -> Self {
        self.sse_retry_interval = policy;
        self
    }

    #[must_use]
    pub fn with_datalog_cache_policy(mut self, policy: Arc<dyn DatalogCachePolicy>) -> Self {
        self.datalog_cache = policy;
        self
    }

    /// Shares the per-app root frame registry with the framework query service.
    ///
    /// Frames are fenced by their own epoch registry and are retired before the
    /// session lock, ahead of the refresh selection a WAL triggers. They are
    /// also charged to the same process-wide ready-byte budget as session
    /// results, so the two shares evict together.
    ///
    /// # Panics
    ///
    /// Panics when called after the service's internal state has been cloned.
    #[must_use]
    pub fn with_root_frame_caches(
        mut self,
        caches: Arc<crate::root_frame_cache::RootFrameCaches>,
    ) -> Self {
        caches.set_invalidation_epochs(Arc::clone(&self.inner.frame_epochs));
        caches.set_result_cache_budget(result_cache_budget(&self.inner));
        Arc::get_mut(&mut self.inner)
            .expect("root frame caches must be configured before cloning the service")
            .root_frame_caches = Some(caches);
        self
    }

    #[must_use]
    pub fn with_receive_timeout_policy(mut self, policy: Arc<dyn ReceiveTimeoutPolicy>) -> Self {
        self.receive_timeout = policy;
        self.receive_handler_timeouts = None;
        self
    }

    #[must_use]
    pub fn with_receive_handler_timeout_policy(
        mut self,
        policy: Arc<dyn ReceiveHandlerTimeoutPolicy>,
    ) -> Self {
        self.receive_handler_timeouts = Some(policy);
        self
    }

    #[must_use]
    pub fn with_combine_transacts_policy(
        mut self,
        policy: Arc<dyn CombineTransactsPolicy>,
    ) -> Self {
        self.combine_transacts = policy;
        self
    }

    #[must_use]
    pub fn with_statement_cancel_wait_policy(
        mut self,
        policy: Arc<dyn StatementCancelWaitPolicy>,
    ) -> Self {
        self.statement_cancel_wait = policy;
        self.receive_handler_timeouts = None;
        self
    }

    #[must_use]
    pub fn with_receive_executor_policy(
        mut self,
        policy: Arc<dyn RealtimeReceiveExecutorPolicy>,
    ) -> Self {
        self.receive_executor = policy;
        self
    }

    #[must_use]
    pub fn with_join_room_logger(mut self, logger: Arc<dyn crate::JoinRoomLogger>) -> Self {
        self.join_room_logger = logger;
        self
    }

    #[must_use]
    pub fn session_count(&self) -> usize {
        sessions(&self.inner).len()
    }

    #[must_use]
    pub fn cleanup_metrics(&self) -> RuntimeCleanupMetrics {
        self.inner.cleanup.metrics_snapshot()
    }

    #[must_use]
    pub fn metrics_snapshot(&self) -> RuntimeRealtimeMetrics {
        let mut metrics = RuntimeRealtimeMetrics::default();
        {
            let entries = sessions(&self.inner);
            metrics.sessions = entries.len();
            let mut app_sessions = BTreeMap::new();
            let mut sdk_sessions = BTreeMap::new();
            for entry in entries.values() {
                metrics.subscriptions = metrics.subscriptions.saturating_add(entry.queries.len());
                if let Some(app_id) = entry.app_id {
                    *app_sessions.entry(app_id).or_insert(0_u64) += 1;
                }
                if let Some(versions) = entry.versions.as_ref() {
                    let (sdk, version) = sdk_session(&client_versions(Some(versions)));
                    *sdk_sessions
                        .entry((sdk.to_owned(), version))
                        .or_insert(0_u64) += 1;
                }
                metrics.outbound_queued = metrics
                    .outbound_queued
                    .saturating_add(entry.sender.state.queued.load(Ordering::Relaxed));
                metrics.transact_queued = metrics.transact_queued.saturating_add(
                    entry.transact_sender.max_capacity() - entry.transact_sender.capacity(),
                );
                metrics.refresh_queued = metrics.refresh_queued.saturating_add(
                    entry.refresh_sender.max_capacity() - entry.refresh_sender.capacity(),
                );
            }
            metrics.app_sessions = app_sessions.into_iter().collect();
            metrics.sdk_sessions = sdk_sessions
                .into_iter()
                .map(|((sdk, version), count)| (sdk, version, count))
                .collect();
        }
        {
            let caches = query_caches(&self.inner);
            metrics.cache_apps = caches.len();
            for cache in caches.values() {
                let snapshot = cache.metrics_snapshot();
                metrics.cache_entries = metrics.cache_entries.saturating_add(snapshot.entries);
                metrics.cache_weight_bytes = metrics
                    .cache_weight_bytes
                    .saturating_add(snapshot.ready_weight);
                metrics.cache_in_flight =
                    metrics.cache_in_flight.saturating_add(snapshot.in_flight);
                metrics.cache_retired_loads = metrics
                    .cache_retired_loads
                    .saturating_add(snapshot.retired_loads);
            }
        }
        let result_budget = self
            .inner
            .result_cache_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(budget) = result_budget {
            let snapshot = budget.snapshot();
            metrics.result_cache_bytes = snapshot.bytes;
            metrics.result_cache_max_bytes = snapshot.max_bytes;
            metrics.result_cache_evictions_total = snapshot.evictions;
        }
        if let Some(caches) = &self.inner.root_frame_caches {
            let frames = caches.totals();
            metrics.root_frame_cache_entries = frames.entries;
            metrics.root_frame_cache_bytes = frames.ready_bytes;
            metrics.root_frame_cache_hits_total = frames.hits;
            metrics.root_frame_cache_misses_total = frames.misses;
            metrics.root_frame_cache_loads_total = frames.loads;
            metrics.root_frame_cache_retire_scanned_total = frames.retire_scanned;
            metrics.root_frame_cache_retire_retired_total = frames.retire_retired;
            metrics.root_frame_cache_retire_nanos_total = frames.retire_nanos;
            metrics.root_frame_cache_expired_total = frames.expired_pruned;
        }
        metrics.query_admission_rejections_total = self
            .inner
            .query_admission_rejections
            .load(Ordering::Relaxed);
        metrics.transaction_admission_rejections_total = self
            .inner
            .transaction_admission_rejections
            .load(Ordering::Relaxed);
        metrics.cleanup = self.cleanup_metrics();
        let admission = self.inner.refresh_admission.metrics();
        metrics.refresh_active = admission.active;
        metrics.refresh_waiting = admission.waiting;
        metrics.refresh_waiting_apps = admission.waiting_apps;
        metrics.refresh_admission_rejections_total = admission.rejected;
        if let Some(dispatcher) = self.receive_dispatcher.get() {
            // Observation must not reset the existing receive gauge's puts.
            let receive = dispatcher.pressure_snapshot();
            metrics.receive_queued = receive.queued;
            metrics.receive_active_groups = receive.active_groups;
            metrics.receive_workers = receive.worker_count;
            metrics.receive_oldest_wait_ms =
                u64::try_from(receive.longest_wait.unwrap_or_default().as_millis())
                    .unwrap_or(u64::MAX);
        }
        metrics
    }

    /// Sets the maximum number of concurrent cluster cleanup attempts.
    #[must_use]
    pub fn with_cluster_cleanup_concurrency(self, concurrency: NonZeroUsize) -> Self {
        self.inner.cleanup.set_concurrency(concurrency);
        self
    }

    /// Gradually closes the local realtime sessions for one app. App-proxy
    /// target changes use this to spread reconnects across the new backend.
    pub fn drain_app_connections(&self, app_id: Uuid, total: Duration, max_gap: Duration) -> usize {
        let session_ids = sessions(&self.inner)
            .ids_for_app(app_id)
            .collect::<Vec<_>>();
        let count = session_ids.len();
        let gap = u32::try_from(count)
            .ok()
            .filter(|count| *count > 0)
            .map_or(Duration::ZERO, |count| (total / count).min(max_gap));
        let inner = Arc::clone(&self.inner);
        let stream_backend = Arc::clone(&self.stream_backend);
        let cluster = self.cluster.as_ref().map(Arc::clone);
        std::mem::drop(tokio::spawn(async move {
            for session_id in session_ids {
                cleanup_runtime_session(&inner, &stream_backend, cluster.as_ref(), session_id);
                tokio::time::sleep(gap).await;
            }
        }));
        count
    }

    /// Gradually closes every local realtime session. Load-balancer target
    /// deregistration uses this to move clients before the target group's
    /// connection-termination deadline instead of dropping them all at once.
    ///
    /// New transports are refused as soon as this starts. Each session then
    /// receives its own close signal, paced by `max_gap`, so its transport can
    /// send a 1012 close frame (WebSocket) or end the stream (SSE) instead of
    /// being dropped without one.
    pub async fn drain_all_connections(&self, total: Duration, max_gap: Duration) -> usize {
        self.inner.drain.begin_drain();
        let session_signals = {
            let entries = sessions(&self.inner);
            entries
                .keys()
                .filter_map(|session_id| entries.get(session_id).map(|entry| entry.drain.clone()))
                .collect::<Vec<_>>()
        };
        let count = session_signals.len();
        let gap = u32::try_from(count)
            .ok()
            .filter(|count| *count > 0)
            .map_or(Duration::ZERO, |count| (total / count).min(max_gap));
        for signal in session_signals {
            signal.begin_drain();
            tokio::time::sleep(gap).await;
        }
        count
    }

    /// Refreshes every local session after replication lost WAL continuity.
    ///
    /// A recreated slot starts at the current WAL position, so records that
    /// existed only in the invalidated slot's range can never arrive. Marking
    /// every query stale with a forced generation is the same fence
    /// `wal_schema_refresh` applies: sessions re-run their queries and receive
    /// `refresh-ok` even when a result is unchanged.
    ///
    /// # Errors
    ///
    /// Returns the first session refresh failure after every session has been
    /// attempted.
    pub async fn force_full_resync(&self) -> Result<(), ApiError> {
        let app_ids = {
            let entries = sessions(&self.inner);
            entries.app_ids().collect::<Vec<_>>()
        };
        for app_id in app_ids {
            clear_app_query_cache(&self.inner, app_id);
        }
        let session_ids = {
            let entries = sessions(&self.inner);
            entries.keys().copied().collect::<Vec<_>>()
        };
        let results = futures_util::future::join_all(
            session_ids
                .into_iter()
                .map(|session_id| self.force_session_resync(session_id)),
        )
        .await;
        results.into_iter().collect()
    }

    /// Marks one session's queries stale and queues its refresh at the current
    /// watermark. A WAL sink deadline hit uses this to retry its own session
    /// instead of waiting for an unrelated invalidation.
    async fn force_session_resync(&self, session_id: Uuid) -> Result<(), ApiError> {
        let watermark = self.inner.watermark_seed.snapshot();
        let processed_tx_id = json!(watermark.tx_id.unwrap_or_default());
        let processed_isn = json!(watermark.isn.unwrap_or_default());
        {
            let mut entries = sessions(&self.inner);
            let Some(entry) = entries.get_mut(&session_id) else {
                return Ok(());
            };
            if entry.app_id.is_none() {
                return Ok(());
            }
            entry.next_refresh_generation = entry.next_refresh_generation.wrapping_add(1);
            let generation = entry.next_refresh_generation;
            entry.refresh_attrs_generation = generation;
            entry.refresh_force_generation = generation;
            for query in entry.queries.values_mut() {
                query.stale_generation = generation;
            }
        }
        self.enqueue_refresh(session_id, processed_tx_id, processed_isn)
            .await
    }

    /// Returns the exact room registry used by runtime presence and broadcasts.
    /// Admin presence projections must consume this handle rather than create a
    /// process-local shadow store.
    #[must_use]
    pub fn room_store(&self) -> Arc<Mutex<RoomStore>> {
        Arc::clone(&self.inner.rooms)
    }

    #[allow(clippy::too_many_lines)]
    async fn open_runtime_session(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
        origin: Option<String>,
        client_ip: Option<IpAddr>,
    ) -> Result<RuntimeRealtimeSession, ApiError> {
        let session_id = Uuid::new_v4();
        let retry_interval = if transport == RuntimeTransport::ServerSentEvents {
            self.sse_retry_interval.retry_interval().await?
        } else {
            Duration::from_millis(500)
        };
        let (transport_sender, receiver) = mpsc::channel(128);
        let (outbound_sender, outbound_receiver) = mpsc::unbounded_channel();
        let (transact_sender, transact_receiver) = mpsc::channel(TRANSACT_INBOX_LIMIT);
        let transact_worker = Arc::new(TransactionWorkerControl::default());
        let (refresh_sender, refresh_receiver) = mpsc::channel(REFRESH_INBOX_LIMIT);
        let refresh_worker = Arc::new(TransactionWorkerControl::default());
        let outbound_state = Arc::new(OutboundQueueState::default());
        let outbound_presence = Arc::new(Mutex::new(OutboundPresenceState::default()));
        let disconnect_inner = Arc::downgrade(&self.inner);
        let disconnect_stream_backend = Arc::clone(&self.stream_backend);
        let disconnect_cluster = self.cluster.as_ref().map(Arc::clone);
        let disconnect = Arc::new(move || {
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                return;
            };
            let inner = disconnect_inner.clone();
            let stream_backend = Arc::clone(&disconnect_stream_backend);
            let cluster = disconnect_cluster.clone();
            runtime.spawn(async move {
                if let Some(inner) = inner.upgrade() {
                    cleanup_runtime_session(&inner, &stream_backend, cluster.as_ref(), session_id);
                }
            });
        }) as Arc<dyn Fn() + Send + Sync>;
        let sender = RuntimeSessionSender {
            sender: outbound_sender,
            state: Arc::clone(&outbound_state),
            presence: Arc::clone(&outbound_presence),
            disconnect: Arc::clone(&disconnect),
        };
        let (token_hash, sse_init) = if transport == RuntimeTransport::ServerSentEvents {
            let token = Uuid::new_v4();
            (
                Some(Sha256::digest(token.as_bytes()).into()),
                Some(json!({
                    "op": "sse-init",
                    "machine-id": self.inner.machine_id,
                    "session-id": session_id,
                    "sse-token": token,
                })),
            )
        } else {
            (None, None)
        };
        let session_drain = RuntimeDrainSignal::default();
        {
            let mut entries = sessions(&self.inner);
            if let Some(client_ip) = client_ip
                .filter(|address| !crate::magic_code_rate_limit::is_internal_address(*address))
            {
                let limit = self.admission_limits.sessions_per_ip;
                if entries.count_for_ip(client_ip) >= limit {
                    return Err(ApiError::new(
                        ApiErrorType::RateLimited,
                        format!(
                            "Too many live sessions from this client IP (limit {limit}). Close another session and retry."
                        ),
                    ));
                }
            }
            // SSE sessions arrive already attached to their app; WebSocket
            // sessions attach during `init`, which repeats this check.
            if let Some(app_id) = app_id {
                let limit = self.admission_limits.sessions_per_app;
                if entries.count_for_app(app_id) >= limit {
                    return Err(app_session_capacity_error(limit));
                }
            }
            entries.insert(
                session_id,
                RuntimeSessionEntry {
                    app_id,
                    origin,
                    client_ip,
                    actor: AdminActor::Guest,
                    bearer_token: None,
                    versions: None,
                    client_features: ClientFeatures::default(),
                    token_hash,
                    user_id: None,
                    sender: sender.clone(),
                    drain: session_drain.clone(),
                    queries: HashMap::new(),
                    refresh_not_before: None,
                    transact_sender,
                    transact_worker: Arc::clone(&transact_worker),
                    refresh_sender,
                    refresh_worker: Arc::clone(&refresh_worker),
                    next_refresh_generation: 0,
                    refresh_attrs_generation: 0,
                    refresh_force_generation: 0,
                    wal_sink_consecutive_timeouts: 0,
                    external_catalog: false,
                    catalog_digest: None,
                    active_handlers: Arc::new(ActiveStatementHandlers::default()),
                },
            );
        }
        let transaction_service = self.clone();
        let worker = tokio::spawn(async move {
            transaction_service
                .run_transaction_worker(session_id, transact_receiver)
                .await;
        });
        transact_worker.install(worker.abort_handle());
        let refresh_service = self.clone();
        let worker = tokio::spawn(async move {
            crate::postgres_pool::DatabaseWorkload::Refresh
                .scope(refresh_service.run_refresh_worker(session_id, refresh_receiver))
                .await;
        });
        refresh_worker.install(worker.abort_handle());
        if let Some(init) = sse_init {
            send(&sender, init);
        }
        if let (Some(app_id), Some(token_hash), Some(cluster)) =
            (app_id, token_hash, self.cluster.as_ref())
            && let Err(error) = cluster
                .register_sse_session(app_id, session_id, token_hash)
                .await
        {
            if let Some(entry) = sessions(&self.inner).remove(&session_id) {
                entry.transact_worker.abort();
                entry.refresh_worker.abort();
            }
            return Err(cluster_error(error));
        }
        tokio::spawn(run_outbound_pump(
            outbound_receiver,
            transport_sender,
            outbound_state,
            outbound_presence,
            disconnect,
        ));
        if self.inner.drain.is_draining() {
            // The drain started while this session was opening. Close it with
            // the same signal as the sessions that were already live instead of
            // leaving a client attached to a process that is leaving.
            session_drain.begin_drain();
        }
        let inner = Arc::downgrade(&self.inner);
        let stream_backend = Arc::clone(&self.stream_backend);
        let cluster = self.cluster.as_ref().map(Arc::clone);
        Ok(RuntimeRealtimeSession::new(session_id, receiver, move || {
            if let Some(inner) = inner.upgrade() {
                cleanup_runtime_session(&inner, &stream_backend, cluster.as_ref(), session_id);
            }
        })
        .with_retry_interval(retry_interval)
        .with_drain_signal(session_drain))
    }

    fn grouped_receive_dispatcher(&self) -> RuntimeReceiveDispatcher {
        self.receive_dispatcher
            .get_or_init(|| {
                let initial_class = self.receive_executor.current_selection().class;
                spawn_receive_executor_refresh(Arc::downgrade(&self.receive_executor));
                let mut worker_service = self.clone();
                // The dispatcher owns this worker clone. Give it a detached
                // holder so the process closure does not retain itself.
                worker_service.receive_dispatcher = Arc::new(OnceLock::new());
                GroupedReceiveDispatcher::new(
                    GroupedReceiveDispatchConfig {
                        capacity: GROUPED_RECEIVE_INBOX_LIMIT,
                        logical_cpus: std::thread::available_parallelism()
                            .map_or(1, std::num::NonZeroUsize::get),
                        executor_class: initial_class,
                    },
                    |_key, first: &GroupedReceiveMessage, second: &GroupedReceiveMessage| {
                        combine_grouped_receive_events(&first.message, &second.message).map(
                            |message| GroupedReceiveMessage {
                                session_id: second.session_id,
                                message,
                            },
                        )
                    },
                    move |_key, event| {
                        let service = worker_service.clone();
                        Box::pin(async move {
                            let GroupedReceiveMessage {
                                session_id,
                                message,
                            } = event;
                            let client_event_id = client_event_id_from(&message);
                            let outcome = AssertUnwindSafe(
                                service.process_message_bounded(session_id, message.clone()),
                            )
                            .catch_unwind()
                            .await;
                            if outcome.is_err() {
                                // The panic hook already counted this; tell the
                                // session instead of leaving it waiting forever.
                                service.send_error(
                                    session_id,
                                    client_event_id.as_ref(),
                                    &message,
                                    &ApiError::new(ApiErrorType::Unknown, "Internal error"),
                                );
                            }
                            Ok(())
                        })
                    },
                )
            })
            .clone()
    }

    async fn enqueue_grouped_message(
        &self,
        session_id: Uuid,
        message: Value,
    ) -> Result<(), ApiError> {
        let initialized = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.versions.is_some())
            .ok_or_else(session_missing)?;
        let sequence = self
            .inner
            .next_ungrouped_receive
            .fetch_add(1, Ordering::Relaxed);
        let key = if initialized {
            realtime_group_key(session_id, &message, sequence)
        } else {
            // An init is a protocol barrier. Messages received before it
            // finishes share its group, preventing a fast query/transact in
            // the same WebSocket or SSE batch from overtaking initialization.
            RealtimeGroupKey::Init(session_id)
        };
        let dispatcher = self.grouped_receive_dispatcher();
        dispatcher.set_executor_class(self.receive_executor.current_selection().class);
        let completion = match dispatcher
            .enqueue(
                key.clone(),
                GroupedReceiveMessage {
                    session_id,
                    message,
                },
            )
            .await
        {
            Ok(completion) => completion,
            Err(TryEnqueueError::Shutdown(_)) => {
                return Err(session_missing());
            }
            Err(TryEnqueueError::Full(_)) => unreachable!("async enqueue waits for capacity"),
        };
        drop(completion);
        Ok(())
    }

    async fn process_message(&self, session_id: Uuid, message: Value) {
        let client_event_id = message.get("client-event-id").cloned();
        let op = message.get("op").and_then(Value::as_str);
        let started = Instant::now();
        // `serde_json`'s arbitrary-precision parsing accepts literals such as
        // `1e400`, which have no finite representation. Reject them here so a
        // panic cannot escape the grouped receive worker.
        if let Some(query) = message.get("q")
            && let Err(error) = instant_instaql::validate_query_numbers(query)
        {
            self.send_error(
                session_id,
                client_event_id.as_ref(),
                &message,
                &query_validation_error(error),
            );
            crate::metrics::record_runtime_op(op, RuntimeOpOutcome::Error, started.elapsed());
            return;
        }
        let app_id = if op == Some("init") {
            None
        } else {
            sessions(&self.inner)
                .get(&session_id)
                .and_then(|entry| entry.app_id)
        };
        let rate_limit_result = match app_id {
            Some(app_id) => self.app_rate_limits.check_app_ids(&[app_id]).await,
            None => Ok(()),
        };
        let result = match rate_limit_result {
            Err(error) => {
                // Transact admission failures are recorded exactly once. The
                // queued worker records a successful admission; anything
                // rejected before the queue is recorded here.
                if op == Some("transact") {
                    crate::metrics::record_runtime_op(
                        op,
                        api_error_outcome(&error),
                        started.elapsed(),
                    );
                }
                Err(error)
            }
            Ok(()) => match op {
                Some("init") => {
                    self.init(session_id, &message, client_event_id.clone())
                        .await
                }
                Some("add-query") => {
                    self.add_query(session_id, &message, client_event_id.clone())
                        .await
                }
                Some("remove-query") => self.remove_query(session_id, &message, client_event_id),
                Some("transact") => {
                    // `process_message_bounded` normally intercepts transactions
                    // so the queued worker records completion. If a caller
                    // bypasses it, enqueue admission failures are still recorded
                    // here; a successful enqueue is recorded by the worker.
                    let result = self.enqueue_transaction(session_id, message.clone(), started);
                    if result.is_err() {
                        crate::metrics::record_runtime_op(
                            Some("transact"),
                            result_outcome(&result),
                            started.elapsed(),
                        );
                    }
                    result
                }
                Some("start-sync") => self.start_sync(session_id, &message, client_event_id).await,
                Some("remove-sync") => self.remove_sync(session_id, &message).await,
                Some("refresh-sync-table") => self.refresh_sync_table(session_id, &message).await,
                Some("resync-table") => Self::resync_table(session_id, &message),
                Some("join-room") => self.join_room(session_id, &message, client_event_id).await,
                Some("leave-room") => self.leave_room(session_id, &message, client_event_id).await,
                Some("set-presence") => {
                    self.set_presence(session_id, &message, client_event_id)
                        .await
                }
                Some("client-broadcast") => {
                    self.broadcast(session_id, &message, client_event_id).await
                }
                Some("start-stream") => {
                    self.start_stream(session_id, &message, client_event_id)
                        .await
                }
                Some("append-stream") => self.append_stream(session_id, &message).await,
                Some("subscribe-stream") => {
                    self.subscribe_stream(session_id, &message, client_event_id.as_ref())
                        .await
                }
                Some("unsubscribe-stream") => self.unsubscribe_stream(session_id, &message),
                _ => Err(ApiError::new(ApiErrorType::ParamMalformed, "Invalid op")
                    .with_hint(json!({"op": message.get("op").cloned().unwrap_or(Value::Null)}))),
            },
        };
        if op != Some("transact") {
            crate::metrics::record_runtime_op(op, result_outcome(&result), started.elapsed());
        }
        let resolved_app = if op == Some("init") {
            sessions(&self.inner)
                .get(&session_id)
                .and_then(|entry| entry.app_id)
        } else {
            app_id
        };
        if let Some(resolved_app) = resolved_app {
            let outcome = result_outcome(&result);
            crate::metrics::record_app_op(resolved_app, op, outcome);
            if result.is_err() {
                crate::metrics::record_app_error(
                    resolved_app,
                    crate::metrics::AppErrorSurface::Runtime,
                );
            }
        }
        if let Err(error) = result {
            let client_event_id = client_event_id_from(&message);
            self.send_error(session_id, client_event_id.as_ref(), &message, &error);
        }
    }

    async fn receive_handler_timeouts(
        &self,
        app_id: Option<Uuid>,
    ) -> Result<ReceiveHandlerTimeouts, ApiError> {
        if let Some(policy) = &self.receive_handler_timeouts {
            return policy.handler_timeouts(app_id).await;
        }
        // Custom policies retain their original order and error priority.
        let timeout = self.receive_timeout.timeout(app_id).await?;
        let cancel_wait = self.statement_cancel_wait.wait_duration().await?;
        Ok(ReceiveHandlerTimeouts {
            timeout,
            cancel_wait,
        })
    }

    async fn process_message_bounded(&self, session_id: Uuid, message: Value) {
        let started = Instant::now();
        let op = message.get("op").and_then(Value::as_str);
        if op == Some("transact") {
            // The queued worker carries this Instant and records the
            // completion; only admission failures are recorded here.
            let result = self.enqueue_transaction(session_id, message.clone(), started);
            if let Err(ref error) = result {
                crate::metrics::record_runtime_op(
                    Some("transact"),
                    result_outcome(&result),
                    started.elapsed(),
                );
                let client_event_id = client_event_id_from(&message);
                self.send_error(session_id, client_event_id.as_ref(), &message, error);
            }
            return;
        }
        let (app_id, active_handlers) = {
            let entries = sessions(&self.inner);
            let Some(entry) = entries.get(&session_id) else {
                return;
            };
            let app_id = if op == Some("init") {
                None
            } else {
                entry.app_id
            };
            (app_id, Arc::clone(&entry.active_handlers))
        };
        let ReceiveHandlerTimeouts {
            timeout: timeout_duration,
            cancel_wait,
        } = match self.receive_handler_timeouts(app_id).await {
            Ok(limits) => limits,
            Err(error) => {
                // The error frame is produced here, before `process_message`
                // runs, so this is the op's only result.
                let outcome = api_error_outcome(&error);
                crate::metrics::record_runtime_op(op, outcome, started.elapsed());
                if let Some(app_id) = app_id {
                    crate::metrics::record_app_op(app_id, op, outcome);
                    crate::metrics::record_app_error(
                        app_id,
                        crate::metrics::AppErrorSurface::Runtime,
                    );
                }
                let client_event_id = client_event_id_from(&message);
                self.send_error(session_id, client_event_id.as_ref(), &message, &error);
                return;
            }
        };
        if matches!(
            run_scoped_handler(
                &active_handlers,
                timeout_duration,
                cancel_wait,
                self.process_message(session_id, message.clone()),
            )
            .await,
            ScopedHandlerOutcome::TimedOut
        ) {
            let timeout_millis = u64::try_from(timeout_duration.as_millis()).unwrap_or(u64::MAX);
            let error = ApiError::new(
                ApiErrorType::OperationTimedOut,
                "Operation timed out: handle-receive",
            )
            .with_hint(json!({"timeout-ms": timeout_millis}));
            let client_event_id = client_event_id_from(&message);
            self.send_error(session_id, client_event_id.as_ref(), &message, &error);
        }
    }

    async fn init(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        use crate::add_query_timing::{AddQueryStage, AddQueryTiming};
        let mut timing = AddQueryTiming::new_init();
        let (already_initialized, expected_app_id, sse_token_hash, client_ip) =
            sessions(&self.inner)
                .get(&session_id)
                .map(|entry| {
                    (
                        entry.versions.is_some() || !matches!(entry.actor, AdminActor::Guest),
                        entry.app_id,
                        entry.token_hash,
                        entry.client_ip,
                    )
                })
                .ok_or_else(session_missing)?;
        if already_initialized {
            return Err(protocol_error("`init` has already run for this session."));
        }
        let app_id = required_uuid(message, "app-id")?;
        // These are raw destructured init fields upstream, not get-optional-param!
        // fields. Preserve Clojure truthiness without assigning parameter-helper
        // envelopes to them.
        let refresh_token = optional_init_uuid(message, "refresh-token")?;
        let admin_token = optional_init_string(message, "__admin-token")?;
        let versions = optional_truthy_value(message, "versions").cloned();
        note_sdk_versions(versions.as_ref());
        let client_features = client_features(versions.as_ref());
        if expected_app_id.is_some_and(|expected| expected != app_id) {
            return Err(protocol_error("SSE app_id does not match init app-id."));
        }
        let (catalog, actor, bearer_token) = if let Some(admin_token) = admin_token {
            let context = AdminRequestContext {
                app_id,
                bearer_token: Some(admin_token.clone()),
                actor: AdminActor::Admin,
                client_ip,
            };
            (
                timing
                    .measure(
                        AddQueryStage::InitCatalog,
                        self.authorizer
                            .authorize(context, crate::AdminDataScope::Read),
                    )
                    .await?,
                AdminActor::Admin,
                Some(admin_token),
            )
        } else {
            (
                timing
                    .measure(
                        AddQueryStage::InitCatalog,
                        self.authorizer.load_catalog(app_id),
                    )
                    .await?,
                refresh_token.map_or(AdminActor::Guest, AdminActor::RefreshToken),
                None,
            )
        };
        // Keep the original fresh Instant catalog, not a merged BYOP catalog.
        // A native init may hand this metadata to fresh identity resolution;
        // no token/user/permission result is handed off or retained.
        let identity_catalog = catalog.clone();
        let (catalog, external_catalog) = timing
            .measure(
                AddQueryStage::InitSource,
                self.data.resolve_realtime_catalog(app_id, catalog),
            )
            .await?;
        let admin = actor == AdminActor::Admin;
        let user_id = timing
            .measure(AddQueryStage::InitIdentity, async {
                let context = AdminRequestContext {
                    app_id,
                    bearer_token: bearer_token.clone(),
                    actor: actor.clone(),
                    client_ip,
                };
                if external_catalog {
                    self.data.resolve_realtime_user_id(context).await
                } else {
                    self.data
                        .resolve_realtime_user_id_on_native_catalog(
                            context,
                            app_id,
                            identity_catalog,
                        )
                        .await
                }
            })
            .await?;
        let app_status = timing
            .measure(AddQueryStage::InitStatus, self.app_status.status(app_id))
            .await?
            .effective_status;
        let attrs = realtime_catalog_attrs(&catalog);
        let catalog_digest = realtime_catalog_digest(&attrs);
        if let (Some(token_hash), Some(cluster)) = (sse_token_hash, self.cluster.as_ref()) {
            cluster
                .register_sse_session(app_id, session_id, token_hash)
                .await
                .map_err(cluster_error)?;
            if !sessions(&self.inner).contains_key(&session_id) {
                let _ = cluster.unregister_sse_session(session_id).await;
                return Err(session_missing());
            }
        }
        let sender = {
            let mut entries = sessions(&self.inner);
            if entries
                .get(&session_id)
                .is_none_or(|entry| entry.app_id != Some(app_id))
            {
                let limit = self.admission_limits.sessions_per_app;
                if entries.count_for_app(app_id) >= limit {
                    return Err(app_session_capacity_error(limit));
                }
            }
            let entry = entries
                .attach_app(session_id, app_id)
                .ok_or_else(session_missing)?;
            entry.actor = actor;
            entry.bearer_token = bearer_token;
            entry.user_id = user_id;
            entry.versions = Some(versions.clone().unwrap_or_else(|| json!({})));
            entry.client_features = client_features;
            entry.external_catalog = external_catalog;
            entry.catalog_digest = Some(catalog_digest);
            entry.sender.clone()
        };
        // The app id now resolved to an existing app's catalog and status, so
        // this app may carry a bounded `app` label.
        crate::metrics::register_app(app_id);
        send(
            &sender,
            with_client_id(
                json!({
                    "op": "init-ok",
                    "session-id": session_id,
                    "auth": {"admin?": admin},
                    "attrs": attrs,
                    "app-status": {"status": app_status.as_str()},
                }),
                client_event_id,
            ),
        );
        timing.complete();
        Ok(())
    }

    async fn add_query(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        use crate::add_query_timing::{AddQueryStage, AddQueryTiming};
        let mut timing = AddQueryTiming::new();
        let query = message
            .get("q")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| protocol_error("Query can not be null."))?;
        let query_json_bytes = serde_json::to_vec(&query)
            .expect("an InstaQL query object serializes")
            .len();
        let query_limit = self.admission_limits.query_json_bytes;
        if query_json_bytes > query_limit {
            return Err(ApiError::new(
                ApiErrorType::ParameterLimitExceeded,
                format!("InstaQL query JSON exceeds the {query_limit} byte limit"),
            ));
        }
        let key = canonical_query(&query);
        let (entry, exists, active_subscriptions) =
            with_initialized_entry(&self.inner, session_id, |entry| {
                (
                    SessionSnapshot::from_entry(entry),
                    entry.queries.contains_key(&key),
                    entry.queries.len(),
                )
            })?;
        if exists {
            send(
                &entry.sender,
                with_client_id(
                    json!({"op": "add-query-exists", "q": query}),
                    client_event_id,
                ),
            );
            return Ok(());
        }
        let subscription_limit = self.admission_limits.subscriptions_per_session;
        if active_subscriptions >= subscription_limit {
            return Err(ApiError::new(
                ApiErrorType::RateLimited,
                format!(
                    "Session has reached the limit of {subscription_limit} active subscriptions. Remove a query before adding another."
                ),
            ));
        }
        let app_id = entry.app_id.expect("initialized entry has app id");
        let return_type = QueryReturnType::from_message(message);
        loop {
            timing.attempt();
            // The watermark belongs to the query start, not its completion.
            let epoch = query_epoch(&self.inner, app_id);
            let processed = processed_watermark(&self.inner, app_id);
            let execution = timing
                .measure(
                    AddQueryStage::Execution,
                    self.execute_session_query(&entry, query.clone(), return_type),
                )
                .await?;
            let instant_topics = timing
                .measure(
                    AddQueryStage::Topics,
                    self.select_query_topics(app_id, &execution),
                )
                .await?;
            let invalidation_predicate = timing
                .measure(
                    AddQueryStage::Predicate,
                    self.compile_query_invalidation_predicate(app_id, &query, &execution),
                )
                .await;
            {
                // WAL selection and epoch advancement hold this same registry
                // lock. A commit during execution or metadata loading cannot
                // disappear in the gap before the new query enters the index.
                let mut entries = sessions(&self.inner);
                let current_epoch = query_epoch(&self.inner, app_id);
                if current_epoch.security != epoch.security {
                    // Rules/schema/status invalidation must be reauthorized
                    // before any pre-invalidation result can be published.
                    timing.retry_security();
                    continue;
                }
                let Some(current) = entries.get_mut(&session_id) else {
                    return Ok(());
                };
                let subscription_limit = self.admission_limits.subscriptions_per_session;
                if current.queries.len() >= subscription_limit {
                    return Err(ApiError::new(
                        ApiErrorType::RateLimited,
                        format!(
                            "Session has reached the limit of {subscription_limit} active subscriptions. Remove a query before adding another."
                        ),
                    ));
                }
                let mut registered = RegisteredQuery {
                    query: query.clone(),
                    return_type,
                    byop_topics: execution.byop_topics.clone(),
                    instant_topics,
                    invalidation_predicate,
                    last_result: CachedQueryResult::from_execution(&execution),
                    stale_generation: 0,
                };
                // Writes routed while this query executed could not select it.
                // Apply their routing test now; any other app write leaves the
                // result current instead of queueing a duplicate execution.
                let changed = current_epoch.data != epoch.data
                    && data_invalidated_since(
                        &self.inner,
                        app_id,
                        epoch.data,
                        current_epoch.data,
                        |topics| query_wal_match(&registered, Some(topics), None).should_refresh(),
                    );
                if changed {
                    current.next_refresh_generation =
                        current.next_refresh_generation.wrapping_add(1);
                    registered.stale_generation = current.next_refresh_generation;
                }
                current.queries.insert(key.clone(), registered);
                index_session_queries(&self.inner, session_id, current);
                if changed {
                    // Queue catch-up before acknowledging, without joining its
                    // execution to the receive handler's deadline. Waiting for
                    // refresh completion after add-query-ok can otherwise emit
                    // a second, contradictory timeout for a successful query.
                    // The session lock also prevents an executing refresh from
                    // clearing this query's stale generation before scheduling.
                    let processed = processed_watermark(&self.inner, app_id);
                    match current.refresh_sender.try_send(PendingRefresh {
                        processed_tx_id: json!(processed.tx_id),
                        processed_isn: json!(processed.isn),
                        window_id: None,
                        completions: vec![],
                    }) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {
                            // A full inbox already guarantees another fresh
                            // execution, which will see this registered stale
                            // query. Older queued watermarks understate progress
                            // safely; durable WAL receipts retain their ordering.
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    }
                }
                send(
                    &current.sender,
                    with_client_id(
                        json!({
                            "op": "add-query-ok",
                            "q": query,
                            "result": execution.result,
                            "result-meta": execution.result_meta,
                            "processed-tx-id": processed.tx_id,
                            "processed-isn": processed.isn,
                        }),
                        client_event_id.clone(),
                    ),
                );
            }
            timing.complete();
            return Ok(());
        }
    }

    async fn execute_session_query(
        &self,
        entry: &SessionSnapshot,
        query: Map<String, Value>,
        return_type: QueryReturnType,
    ) -> Result<SessionQueryResult, ApiError> {
        self.execute_session_query_on_workload(
            entry,
            query,
            return_type,
            None,
            crate::postgres_pool::DatabaseWorkload::Interactive,
        )
        .await
    }

    async fn execute_session_query_with_native_catalog(
        &self,
        entry: &SessionSnapshot,
        query: Map<String, Value>,
        return_type: QueryReturnType,
        native_catalog: Option<(Uuid, Arc<instant_instaql::AttributeCatalog>)>,
    ) -> Result<SessionQueryResult, ApiError> {
        self.execute_session_query_on_workload(
            entry,
            query,
            return_type,
            native_catalog,
            crate::postgres_pool::DatabaseWorkload::Refresh,
        )
        .await
    }

    async fn execute_session_query_on_workload(
        &self,
        entry: &SessionSnapshot,
        query: Map<String, Value>,
        return_type: QueryReturnType,
        mut native_catalog: Option<(Uuid, Arc<instant_instaql::AttributeCatalog>)>,
        workload: crate::postgres_pool::DatabaseWorkload,
    ) -> Result<SessionQueryResult, ApiError> {
        let app_id = entry.app_id.expect("initialized entry has app id");
        let key = SharedQueryKey::new(entry, &query, return_type);
        // The query cache spawns this loader on a new task, which does not
        // inherit task-locals. Capture the refresh's frame-reuse decision here
        // and re-enter it around the execution so a coarse batch cannot reach
        // the shared frame cache through the detached loader.
        let disable_shared_root_frame_reuse =
            crate::root_frame_cache::shared_root_frame_reuse_disabled();
        loop {
            let cache = app_query_cache(&self.inner, app_id, self.datalog_cache.as_ref()).await?;
            let service = self.clone();
            let context = SessionQueryContext::from_entry(entry);
            let query = query.clone();
            let catalog = native_catalog.clone();
            match cache
                .get_or_load(key.clone(), move || async move {
                    // The cache owns this detached loader, not any individual
                    // waiter. Its own scope discards leases if invalidation or
                    // the last waiter drops it during a PostgreSQL query.
                    // Freeze the originating operation before the shared cache
                    // spawns its loader. Initial SDK reads use the existing
                    // interactive reservation; WAL refreshes stay on refresh.
                    // Identical in-flight callers still share the first loader,
                    // its admission budget, cancellation scope and cache key.
                    let tracker = StatementTracker::default();
                    let execution =
                        workload.scope(tracker.scope(service.execute_session_query_uncached(
                            &context,
                            query,
                            return_type,
                            catalog,
                        )));
                    if disable_shared_root_frame_reuse {
                        crate::root_frame_cache::without_shared_root_frame_reuse(execution).await
                    } else {
                        execution.await
                    }
                })
                .await
            {
                Ok(result) => return Ok(result.as_ref().clone()),
                Err(QueryCacheError::Loader(error)) => return Err(error.as_ref().clone()),
                // A concurrent schema/rule invalidation fences the hint too.
                // Retry using the ordinary path, which reloads its catalog.
                Err(QueryCacheError::Cancelled) => {
                    native_catalog = None;
                }
                Err(QueryCacheError::Overloaded) => {
                    self.inner
                        .query_admission_rejections
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(ApiError::new(
                        ApiErrorType::RateLimited,
                        "Realtime query capacity is temporarily exhausted",
                    ));
                }
            }
        }
    }

    async fn execute_session_query_uncached(
        &self,
        context: &SessionQueryContext,
        query: Map<String, Value>,
        return_type: QueryReturnType,
        native_catalog: Option<(Uuid, Arc<instant_instaql::AttributeCatalog>)>,
    ) -> Result<SessionQueryResult, ApiError> {
        let request = instant_http::AdminQueryRequest {
            query,
            inference: false,
            versions: context.versions.clone(),
        };
        let context = context.request.clone();
        match return_type {
            QueryReturnType::Tree => {
                RuntimeTreeQueryBackend::query_tree(self.data.as_ref(), context, request)
                    .await
                    .map(|response| SessionQueryResult {
                        result: Arc::new(response.result),
                        result_meta: Arc::new(response.result_meta),
                        byop_topics: None,
                        instant_topics: response.instant_topics,
                    })
            }
            QueryReturnType::JoinRows => {
                let response = match native_catalog {
                    Some((catalog_app_id, catalog)) => {
                        self.data
                            .query_join_rows_on_native_catalog_without_attrs_shared(
                                context,
                                request,
                                catalog_app_id,
                                catalog,
                            )
                            .await
                    }
                    None => {
                        self.data
                            .query_join_rows_without_attrs(context, request)
                            .await
                    }
                };
                response.and_then(join_rows_session_result)
            }
        }
    }

    async fn select_query_topics(
        &self,
        app_id: Uuid,
        result: &SessionQueryResult,
    ) -> Result<Option<Vec<QueryTopic>>, ApiError> {
        if result.instant_topics.is_none() {
            return Ok(None);
        }
        let use_coarse = self.coarse_topics.use_coarse_topics(app_id).await?;
        Ok(query_topics_for_mode(result, use_coarse))
    }

    async fn compile_query_invalidation_predicate(
        &self,
        app_id: Uuid,
        query: &Map<String, Value>,
        result: &SessionQueryResult,
    ) -> Option<InvalidationPredicate> {
        if result.instant_topics.is_none() || result.byop_topics.is_some() {
            return None;
        }
        let catalog = self.authorizer.load_catalog(app_id).await.ok()?;
        compile_query_invalidation_predicate(query, &catalog)
    }

    fn remove_query(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let query = message
            .get("q")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol_error("Query can not be null."))?;
        let mut entries = sessions(&self.inner);
        let entry = entries.get_mut(&session_id).ok_or_else(session_missing)?;
        // Subscription ownership is separate from cached data validity. WAL and
        // policy changes invalidate data; removing a waiter must not cancel
        // another session's shared loader.
        entry.queries.remove(&canonical_query(query));
        index_session_queries(&self.inner, session_id, entry);
        send(
            &entry.sender,
            with_client_id(
                json!({"op": "remove-query-ok", "q": query}),
                client_event_id,
            ),
        );
        Ok(())
    }

    fn enqueue_transaction(
        &self,
        session_id: Uuid,
        message: Value,
        started: Instant,
    ) -> Result<(), ApiError> {
        let steps = message
            .get("tx-steps")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| protocol_error("Invalid transaction steps"))?;
        let entry = initialized_entry(&self.inner, session_id)?;
        entry
            .transact_sender
            .try_send(PendingTransaction {
                original: message,
                steps,
                redundant: Vec::new(),
                started,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    self.inner
                        .transaction_admission_rejections
                        .fetch_add(1, Ordering::Relaxed);
                    ApiError::new(
                        ApiErrorType::RateLimited,
                        "Realtime transaction queue is full.",
                    )
                }
                mpsc::error::TrySendError::Closed(_) => session_missing(),
            })
    }

    async fn run_transaction_worker(
        &self,
        session_id: Uuid,
        mut receiver: mpsc::Receiver<PendingTransaction>,
    ) {
        let mut pending = VecDeque::new();
        let mut has_executed = false;
        loop {
            let transaction = if let Some(transaction) = pending.pop_front() {
                Some(transaction)
            } else {
                receiver.recv().await
            };
            let Some(mut transaction) = transaction else {
                return;
            };

            if has_executed {
                loop {
                    let next = pending.pop_front().or_else(|| receiver.try_recv().ok());
                    let Some(next) = next else {
                        break;
                    };
                    if self
                        .transactions_match(session_id, &transaction.steps, &next.steps)
                        .await
                    {
                        let mut redundant = std::mem::take(&mut transaction.redundant);
                        redundant.push(transaction.original);
                        redundant.extend(next.redundant);
                        transaction = PendingTransaction {
                            original: next.original,
                            steps: next.steps,
                            redundant,
                            // Measure the combined delivery from its oldest
                            // received message.
                            started: transaction.started.min(next.started),
                        };
                    } else {
                        pending.push_front(next);
                        break;
                    }
                }
            }
            self.process_transaction_bounded(session_id, transaction)
                .await;
            has_executed = true;
        }
    }

    async fn transactions_match(&self, session_id: Uuid, left: &[Value], right: &[Value]) -> bool {
        let Ok(entry) = initialized_entry(&self.inner, session_id) else {
            return false;
        };
        let app_id = entry.app_id.expect("initialized entry has app id");
        if !matches!(
            self.combine_transacts.combine_transacts(app_id).await,
            Ok(true)
        ) {
            return false;
        }
        let Ok(catalog) = self.authorizer.load_catalog(app_id).await else {
            return false;
        };
        matching_transaction_steps(left, right, &catalog)
    }

    async fn process_transaction_bounded(&self, session_id: Uuid, transaction: PendingTransaction) {
        let record = |app_id: Option<Uuid>, outcome: RuntimeOpOutcome| {
            crate::metrics::record_runtime_op(
                Some("transact"),
                outcome,
                transaction.started.elapsed(),
            );
            if let Some(app_id) = app_id {
                crate::metrics::record_app_op(app_id, Some("transact"), outcome);
                if outcome == RuntimeOpOutcome::Error {
                    crate::metrics::record_app_error(
                        app_id,
                        crate::metrics::AppErrorSurface::Runtime,
                    );
                }
            }
        };
        let (app_id, active_handlers) = {
            let entries = sessions(&self.inner);
            let Some(entry) = entries.get(&session_id) else {
                record(None, RuntimeOpOutcome::Error);
                return;
            };
            (entry.app_id, Arc::clone(&entry.active_handlers))
        };
        let ReceiveHandlerTimeouts {
            timeout: timeout_duration,
            cancel_wait,
        } = match self.receive_handler_timeouts(app_id).await {
            Ok(limits) => limits,
            Err(error) => {
                self.send_transaction_errors(session_id, &transaction, &error);
                record(app_id, api_error_outcome(&error));
                return;
            }
        };
        let result = run_scoped_handler(
            &active_handlers,
            timeout_duration,
            cancel_wait,
            self.execute_transaction(session_id, &transaction.steps),
        )
        .await;
        let outcome = transaction_outcome(&result);
        match result {
            ScopedHandlerOutcome::Completed(Ok(response)) => {
                self.send_transaction_successes(session_id, &transaction, &response);
            }
            ScopedHandlerOutcome::Completed(Err(error)) => {
                self.send_transaction_errors(session_id, &transaction, &error);
            }
            ScopedHandlerOutcome::TimedOut => {
                let timeout_millis =
                    u64::try_from(timeout_duration.as_millis()).unwrap_or(u64::MAX);
                let error = ApiError::new(
                    ApiErrorType::OperationTimedOut,
                    "Operation timed out: handle-receive",
                )
                .with_hint(json!({"timeout-ms": timeout_millis}));
                self.send_transaction_errors(session_id, &transaction, &error);
            }
            ScopedHandlerOutcome::Disconnected => {}
        }
        // The client-visible result (success response or error frame) was
        // produced above; record the queued transaction exactly once here.
        record(app_id, outcome);
    }

    async fn execute_transaction(
        &self,
        session_id: Uuid,
        steps: &[Value],
    ) -> Result<Value, ApiError> {
        let started = tokio::time::Instant::now();
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        self.app_rate_limits.check_app_ids(&[app_id]).await?;
        let backend_started = tokio::time::Instant::now();
        let transaction = self
            .data
            .transact_realtime(
                AdminRequestContext {
                    app_id,
                    bearer_token: entry.bearer_token,
                    actor: entry.actor,
                    client_ip: entry.client_ip,
                },
                AdminTransactRequest {
                    steps: steps.to_vec(),
                    throw_on_missing_attrs: false,
                },
            )
            .await?;
        let backend_elapsed = backend_started.elapsed();
        let backend_ms = backend_elapsed.as_secs_f64() * 1000.0;
        let max_isn_wait = DEFAULT_RECEIVE_TIMEOUT
            .saturating_sub(started.elapsed())
            .saturating_sub(Duration::from_secs(1))
            .min(self.transaction_isn_wait);
        let isn_started = tokio::time::Instant::now();
        let observed_isn = self
            .inner
            .transaction_isns
            .wait(transaction.tx_id, max_isn_wait)
            .await;
        let isn_wait_elapsed = isn_started.elapsed();
        let total_elapsed = started.elapsed();
        tracing::debug!(
            target: "instant_server::realtime_latency",
            tx_id = transaction.tx_id,
            backend_ms,
            isn_wait_ms = isn_wait_elapsed.as_secs_f64() * 1000.0,
            total_ms = total_elapsed.as_secs_f64() * 1000.0,
            isn_fallback = observed_isn.is_none(),
            "realtime transaction completed"
        );
        let isn = observed_isn.unwrap_or(transaction.fallback_isn);
        Ok(json!({"op": "transact-ok", "tx-id": transaction.tx_id, "isn": isn}))
    }

    fn send_transaction_successes(
        &self,
        session_id: Uuid,
        transaction: &PendingTransaction,
        response: &Value,
    ) {
        let Some(sender) = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.sender.clone())
        else {
            return;
        };
        for original in transaction
            .redundant
            .iter()
            .chain(std::iter::once(&transaction.original))
        {
            send(
                &sender,
                with_client_id(response.clone(), client_event_id_from(original)),
            );
        }
    }

    fn send_transaction_errors(
        &self,
        session_id: Uuid,
        transaction: &PendingTransaction,
        error: &ApiError,
    ) {
        for original in transaction
            .redundant
            .iter()
            .chain(std::iter::once(&transaction.original))
        {
            let client_event_id = client_event_id_from(original);
            self.send_error(session_id, client_event_id.as_ref(), original, error);
        }
    }

    async fn enqueue_refresh(
        &self,
        session_id: Uuid,
        processed_tx_id: Value,
        processed_isn: Value,
    ) -> Result<(), ApiError> {
        let sender = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.refresh_sender.clone());
        let Some(sender) = sender else {
            return Ok(());
        };
        let (completion, result) = oneshot::channel();
        if sender
            .send(PendingRefresh {
                processed_tx_id,
                processed_isn,
                window_id: None,
                completions: vec![RefreshCompletion::Once(completion)],
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        result.await.unwrap_or(Ok(()))
    }

    async fn enqueue_durable_refresh(
        &self,
        session_id: Uuid,
        tx_id: i64,
        isn: Isn,
        window_id: i64,
    ) -> oneshot::Receiver<()> {
        let (completion, result) = oneshot::channel();
        let sender = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.refresh_sender.clone());
        if let Some(sender) = sender {
            let _disconnected = sender
                .send(PendingRefresh {
                    processed_tx_id: json!(tx_id),
                    processed_isn: json!(isn),
                    window_id: Some(window_id),
                    completions: vec![RefreshCompletion::Durable(completion)],
                })
                .await;
        }
        result
    }

    async fn run_refresh_worker(
        &self,
        session_id: Uuid,
        mut receiver: mpsc::Receiver<PendingRefresh>,
    ) {
        let mut has_executed = false;
        let mut completed = ProcessedWatermark::default();
        while let Some(mut refresh) = receiver.recv().await {
            if has_executed {
                while let Ok(next) = receiver.try_recv() {
                    refresh.coalesce(next);
                }
            }
            let result = loop {
                // Raise only the boundary of a fresh execution. An already
                // running query must keep the watermark captured at its start.
                refresh.preserve_completed_watermark(completed);
                let attempt = tokio::time::timeout(
                    self.wal_sink_refresh_deadline,
                    self.process_refresh_bounded(
                        session_id,
                        refresh.processed_tx_id.clone(),
                        refresh.processed_isn.clone(),
                    ),
                )
                .await;
                let timed_out = attempt.is_err();
                let result = if let Ok(result) = attempt {
                    result
                } else {
                    Err(self.report_wal_sink_refresh_timeout(
                        session_id,
                        refresh.window_id,
                        &refresh.processed_tx_id,
                        &refresh.processed_isn,
                    ))
                };
                // A timed-out refresh fails on its own; the window completes so
                // one stalled query cannot hold every later window behind it.
                if result.is_ok()
                    || timed_out
                    || !refresh
                        .completions
                        .iter()
                        .any(|completion| matches!(completion, RefreshCompletion::Durable(_)))
                {
                    break result;
                }
                // Keep the failed watermark ahead of later work. New invalidations
                // can coalesce into this retry without allowing acknowledgement
                // to pass an unsuccessful refresh.
                tokio::time::sleep(CLUSTER_SUBSCRIBE_RETRY_MIN).await;
                while let Ok(next) = receiver.try_recv() {
                    refresh.coalesce(next);
                }
            };
            if result.is_ok() {
                if let Some(entry) = sessions(&self.inner).get_mut(&session_id) {
                    entry.wal_sink_consecutive_timeouts = 0;
                }
                let watermark = refresh.watermark();
                completed.tx_id = completed.tx_id.max(watermark.tx_id);
                completed.isn = completed.isn.max(watermark.isn);
            }
            for completion in refresh.completions {
                match completion {
                    RefreshCompletion::Once(completion) => {
                        let _caller_was_dropped = completion.send(result.clone());
                    }
                    RefreshCompletion::Durable(completion) => {
                        let _caller_was_dropped = completion.send(());
                    }
                }
            }
            has_executed = true;
        }
    }

    /// Retries one session's refresh after a deadline hit on a cloned handle so
    /// the WAL window is never blocked. A session that disconnects meanwhile is
    /// skipped, as is a retry superseded by a newer timeout or a reset.
    fn schedule_wal_sink_forced_resync(&self, session_id: Uuid, consecutive_timeouts: u32) {
        let delay = wal_sink_resync_backoff(consecutive_timeouts);
        let service = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let still_due = sessions(&service.inner)
                .get(&session_id)
                .is_some_and(|entry| entry.wal_sink_consecutive_timeouts == consecutive_timeouts);
            if !still_due {
                return;
            }
            crate::metrics::record_wal_sink_forced_resync();
            if let Err(error) = service.force_session_resync(session_id).await {
                tracing::debug!(
                    %session_id,
                    error = %error.body().message,
                    "realtime forced WAL sink resync failed"
                );
            }
        });
    }

    fn report_wal_sink_refresh_timeout(
        &self,
        session_id: Uuid,
        window_id: Option<i64>,
        processed_tx_id: &Value,
        processed_isn: &Value,
    ) -> ApiError {
        let app_id = sessions(&self.inner)
            .get(&session_id)
            .and_then(|entry| entry.app_id);
        let consecutive_timeouts = sessions(&self.inner).get_mut(&session_id).map(|entry| {
            entry.wal_sink_consecutive_timeouts =
                entry.wal_sink_consecutive_timeouts.saturating_add(1);
            entry.wal_sink_consecutive_timeouts
        });
        let resync_after_ms = consecutive_timeouts.map(|count| {
            u64::try_from(wal_sink_resync_backoff(count).as_millis()).unwrap_or(u64::MAX)
        });
        let deadline_ms =
            u64::try_from(self.wal_sink_refresh_deadline.as_millis()).unwrap_or(u64::MAX);
        tracing::warn!(
            event_name = "realtime/wal-sink-refresh-timeout",
            %session_id,
            app_id = ?app_id,
            window_id = ?window_id,
            deadline_ms,
            consecutive_timeouts = ?consecutive_timeouts,
            resync_after_ms = ?resync_after_ms,
            "realtime WAL window refresh exceeded its deadline"
        );
        crate::metrics::record_wal_sink_refresh_timeout();
        if let Some(count) = consecutive_timeouts {
            self.schedule_wal_sink_forced_resync(session_id, count);
        }
        let error = ApiError::new(
            ApiErrorType::Timeout,
            format!(
                "Realtime refresh exceeded its {deadline_ms} ms deadline; the session retries on its own."
            ),
        );
        self.send_refresh_error(session_id, processed_tx_id, processed_isn, &error);
        error
    }

    async fn process_refresh_bounded(
        &self,
        session_id: Uuid,
        processed_tx_id: Value,
        processed_isn: Value,
    ) -> Result<(), ApiError> {
        let handlers = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| Arc::clone(&entry.active_handlers));
        let Some(handlers) = handlers else {
            return Ok(());
        };
        let cancel_wait = self.statement_cancel_wait.wait_duration().await?;
        run_disconnect_scoped_handler(
            &handlers,
            cancel_wait,
            self.execute_pending_refresh(session_id, processed_tx_id, processed_isn),
        )
        .await
        .unwrap_or(Ok(()))
    }

    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(name = "realtime/refresh-session", skip_all)]
    async fn execute_pending_refresh(
        &self,
        session_id: Uuid,
        processed_tx_id: Value,
        processed_isn: Value,
    ) -> Result<(), ApiError> {
        let started = tokio::time::Instant::now();
        self.wait_for_refresh_slot(session_id).await;
        let throttle_ms = started.elapsed().as_secs_f64() * 1000.0;
        let app_id = with_initialized_entry(&self.inner, session_id, |entry| entry.app_id)?
            .expect("initialized entry has app id");
        // Wait inside the disconnect/cancellation scope, before catalog/auth/SQL
        // work and before capturing stale generations. A cancelled queued session
        // removes its ticket; an active slot stays owned through handler cleanup.
        let _admission = self
            .inner
            .refresh_admission
            .acquire(app_id)
            .await
            .map_err(|()| {
                ApiError::new(
                    ApiErrorType::RateLimited,
                    "Realtime refresh admission is full",
                )
            })?;
        let catalog_started = tokio::time::Instant::now();
        let (entry, stale_queries) = with_initialized_entry(&self.inner, session_id, |entry| {
            let stale = entry
                .queries
                .iter()
                .filter(|(_, query)| query.stale_generation != 0)
                .map(|(key, query)| (key.clone(), query.clone()))
                .collect::<Vec<_>>();
            (SessionSnapshot::from_entry(entry), stale)
        })?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let context = entry_context(&entry, app_id);
        let instant_catalog = if entry.actor == AdminActor::Admin {
            self.authorizer
                .authorize(context.clone(), crate::AdminDataScope::Read)
                .await
                .map(crate::admin_data::RefreshCatalogSnapshot::from)
        } else {
            self.authorizer.load_refresh_catalog(app_id).await
        };
        let instant_catalog = match instant_catalog {
            Ok(catalog) => catalog,
            Err(error) => {
                self.send_refresh_error(session_id, &processed_tx_id, &processed_isn, &error);
                return Ok(());
            }
        };
        let catalog_ms = catalog_started.elapsed().as_secs_f64() * 1000.0;
        let stale_query_count = stale_queries.len();
        let attrs_generation = entry.refresh_attrs_generation;
        let force_generation = entry.refresh_force_generation;
        let status_started = tokio::time::Instant::now();
        let use_coarse_topics = match self
            .read_refresh_controls(app_id, !stale_queries.is_empty())
            .await
        {
            Ok(value) => value,
            Err(RefreshReadControlsFailure::Status(error)) => {
                self.send_refresh_error(session_id, &processed_tx_id, &processed_isn, &error);
                return Ok(());
            }
            Err(RefreshReadControlsFailure::Topics(error)) => return Err(error),
        };
        let status_ms = status_started.elapsed().as_secs_f64() * 1000.0;
        let batch_queries = stale_queries
            .iter()
            .map(|(_, registered)| RuntimeRealtimeBatchQuery {
                context: context.clone(),
                query: registered.query.clone(),
                versions: entry.versions.clone(),
                mode: match registered.return_type {
                    QueryReturnType::Tree => RuntimeRealtimeQueryMode::Tree,
                    QueryReturnType::JoinRows => RuntimeRealtimeQueryMode::JoinRows,
                },
            })
            .collect();
        let backend_started = tokio::time::Instant::now();
        let refresh = async {
            self.data
                .refresh_realtime_batch_on_catalog(app_id, instant_catalog, batch_queries)
                .await
        };
        // A coarse-triggered refresh selected these queries with coarse topics,
        // but frames retire on precise ones. Keep that batch on the per-request
        // frame path so it cannot read a frame this WAL did not retire.
        let refresh_batch = if use_coarse_topics {
            crate::root_frame_cache::without_shared_root_frame_reuse(refresh).await
        } else {
            refresh.await
        };
        let refresh_batch = match refresh_batch {
            Ok(batch) => batch,
            Err(error) => {
                self.send_refresh_error(session_id, &processed_tx_id, &processed_isn, &error);
                return Ok(());
            }
        };
        let backend_batch_ms = backend_started.elapsed().as_secs_f64() * 1000.0;
        let queries_started = tokio::time::Instant::now();
        let refreshed_digest = refresh_catalog_digest(&self.inner, app_id, &refresh_batch.catalog);
        let source_or_attrs_changed = entry.external_catalog != refresh_batch.external_catalog
            || entry.catalog_digest != Some(refreshed_digest);
        let mut batch_results = refresh_batch.results.map(VecDeque::from);
        let mut computations = Vec::with_capacity(stale_queries.len());
        for (key, registered) in stale_queries {
            let result = match batch_results.as_mut() {
                Some(results) => results
                    .pop_front()
                    .unwrap_or_else(|| {
                        Err(ApiError::new(
                            ApiErrorType::Unknown,
                            "realtime refresh batch returned too few query results",
                        ))
                    })
                    .map(|result| SessionQueryResult {
                        result: Arc::new(result.result),
                        result_meta: Arc::new(result.result_meta),
                        byop_topics: result.byop_topics,
                        instant_topics: result.instant_topics,
                    }),
                None => {
                    let query = self.execute_session_query_with_native_catalog(
                        &entry,
                        registered.query.clone(),
                        registered.return_type,
                        (!refresh_batch.external_catalog
                            && matches!(registered.return_type, QueryReturnType::JoinRows))
                        .then(|| (app_id, Arc::clone(&refresh_batch.catalog))),
                    );
                    // The query cache's detached loader inherits this decision
                    // explicitly, but the call itself must also run on the
                    // per-request frame path.
                    if use_coarse_topics {
                        crate::root_frame_cache::without_shared_root_frame_reuse(query).await
                    } else {
                        query.await
                    }
                }
            };
            if let Some(computation) = self.apply_refreshed_query_result(
                session_id,
                &processed_tx_id,
                &processed_isn,
                &key,
                &registered,
                use_coarse_topics,
                force_generation != 0,
                &refresh_batch.catalog,
                result,
            ) {
                computations.push(computation);
            }
        }
        let query_and_result_ms = queries_started.elapsed().as_secs_f64() * 1000.0;
        let total_ms = started.elapsed().as_secs_f64() * 1000.0;
        tracing::debug!(
            target: "instant_server::realtime_latency",
            stale_query_count,
            throttle_ms,
            catalog_ms,
            status_ms,
            backend_batch_ms,
            query_and_result_ms,
            total_ms,
            "realtime session refresh stages"
        );
        crate::refresh_timing::observe(crate::refresh_timing::RefreshTiming {
            throttle_ms,
            catalog_ms,
            status_ms,
            source_batch_ms: backend_batch_ms,
            query_and_result_ms,
            total_ms,
            stale_query_count,
            external_catalog: refresh_batch.external_catalog,
        });
        if batch_results
            .as_ref()
            .is_some_and(|results| !results.is_empty())
        {
            self.send_refresh_error(
                session_id,
                &processed_tx_id,
                &processed_isn,
                &ApiError::new(
                    ApiErrorType::Unknown,
                    "realtime refresh batch returned too many query results",
                ),
            );
        }
        if let Some(current) = sessions(&self.inner).get_mut(&session_id) {
            current.external_catalog = refresh_batch.external_catalog;
            current.catalog_digest = Some(refreshed_digest);
            if current.refresh_attrs_generation == attrs_generation {
                current.refresh_attrs_generation = 0;
            }
            if current.refresh_force_generation == force_generation {
                current.refresh_force_generation = 0;
            }
        }
        let schema_refresh = attrs_generation != 0 || source_or_attrs_changed;
        if computations.is_empty() && !schema_refresh {
            if supports_skip_attrs(entry.versions.as_ref()) {
                self.throttle_noop_sessions(app_id, &[session_id]).await?;
            }
            return Ok(());
        }
        let mut event = json!({
            "op": "refresh-ok",
            "processed-tx-id": processed_tx_id,
            "processed-isn": processed_isn,
            "computations": computations,
        });
        if schema_refresh || !supports_skip_attrs(entry.versions.as_ref()) {
            event["attrs"] = realtime_catalog_attrs(&refresh_batch.catalog);
        }
        if let Some(sender) = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.sender.clone())
        {
            send(&sender, event);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_refreshed_query_result(
        &self,
        session_id: Uuid,
        processed_tx_id: &Value,
        processed_isn: &Value,
        key: &str,
        registered: &RegisteredQuery,
        use_coarse_topics: bool,
        force: bool,
        refreshed_catalog: &instant_instaql::AttributeCatalog,
        result: Result<SessionQueryResult, ApiError>,
    ) -> Option<Value> {
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.send_refresh_error(session_id, processed_tx_id, processed_isn, &error);
                return None;
            }
        };
        let instant_topics = query_topics_for_mode(&result, use_coarse_topics);
        let cached_result = CachedQueryResult::from_execution(&result);
        let changed = force || cached_result != registered.last_result;
        // Parsing/coercion can walk a large query and catalog. Do not serialize
        // unrelated sessions behind that CPU work on the global registry lock.
        let invalidation_predicate = if result.byop_topics.is_none() && instant_topics.is_some() {
            compile_query_invalidation_predicate(&registered.query, refreshed_catalog)
        } else {
            None
        };
        if let Some(current) = sessions(&self.inner).get_mut(&session_id)
            && let Some(query) = current.queries.get_mut(key)
        {
            query.byop_topics = result.byop_topics;
            query.instant_topics = instant_topics;
            query.invalidation_predicate = invalidation_predicate;
            query.last_result = cached_result;
            if query.stale_generation == registered.stale_generation {
                query.stale_generation = 0;
            }
            index_session_queries(&self.inner, session_id, current);
        }
        changed.then(|| {
            json!({
                "instaql-query": registered.query.clone(),
                "instaql-result": result.result,
                "result-meta": result.result_meta,
            })
        })
    }

    async fn start_sync(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let query = message
            .get("q")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| protocol_error("Query can not be null."))?;
        validate_sync_query(&query)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let snapshot = RuntimeSyncBackend::sync_snapshot(
            self.data.as_ref(),
            RuntimeSyncSnapshotRequest {
                context: AdminRequestContext {
                    app_id,
                    bearer_token: entry.bearer_token.clone(),
                    actor: entry.actor.clone(),
                    client_ip: entry.client_ip,
                },
                query: query.clone(),
            },
        )
        .await?;
        let subscription_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let tx_id = snapshot.tx_id;
        {
            let mut state = sync_state(&self.inner);
            state.active.insert(
                (session_id, subscription_id),
                ActiveSyncSubscription {
                    session: session_id,
                    app: app_id,
                    subscription: subscription_id,
                    sent_tx: tx_id,
                    attribute_ids: snapshot.attribute_ids.clone(),
                    reset_on_change: true,
                    ready: false,
                    draining: true,
                },
            );
        }
        let context = entry_context(&entry, app_id);
        let mut ownership = self.start_sync_guard(context.clone(), session_id, subscription_id);
        let start_event = with_client_id(
            json!({
                "op": "start-sync-ok",
                "q": query,
                "subscription-id": subscription_id,
                "token": token,
            }),
            client_event_id,
        );
        send_sync_frame(&entry.sender, start_event).await?;
        for batch in snapshot.join_rows.chunks(100) {
            send_sync_frame(
                &entry.sender,
                json!({
                    "op": "sync-load-batch",
                    "subscription-id": subscription_id,
                    "join-rows": batch,
                }),
            )
            .await?;
        }
        let data = Arc::clone(&self.data);
        let create_request = RuntimeSyncSubscriptionCreate {
            context: context.clone(),
            subscription_id,
            token,
            query,
            attribute_ids: snapshot.attribute_ids,
        };
        ownership.create_task = Some(tokio::spawn(async move {
            RuntimeSyncBackend::create_sync_subscription(data.as_ref(), create_request).await
        }));
        let create_join_result = ownership
            .create_task
            .as_mut()
            .expect("sync create task was installed")
            .await;
        ownership.create_task = None;
        let create_result = create_join_result.map_err(internal_error)?;
        create_result?;
        ownership.durable = true;
        send_sync_frame(
            &entry.sender,
            json!({
                "op": "sync-init-finish",
                "subscription-id": subscription_id,
                "tx-id": tx_id,
            }),
        )
        .await?;
        self.drain_claimed_sync_transactions(session_id, subscription_id, true)
            .await?;
        ownership.disarm();
        Ok(())
    }

    fn start_sync_guard(
        &self,
        context: AdminRequestContext,
        session_id: Uuid,
        subscription_id: Uuid,
    ) -> StartSyncGuard<D> {
        StartSyncGuard {
            inner: Arc::clone(&self.inner),
            data: Arc::clone(&self.data),
            stream_backend: Arc::clone(&self.stream_backend),
            cluster: self.cluster.as_ref().map(Arc::clone),
            context,
            session_id,
            subscription_id,
            create_task: None,
            durable: false,
            armed: true,
        }
    }

    fn close_runtime_session(&self, session_id: Uuid) {
        cleanup_runtime_session(
            &self.inner,
            &self.stream_backend,
            self.cluster.as_ref(),
            session_id,
        );
    }

    fn claim_sync_drain(&self, session_id: Uuid, subscription_id: Uuid) -> bool {
        let mut state = sync_state(&self.inner);
        let Some(active) = state.active.get_mut(&(session_id, subscription_id)) else {
            return false;
        };
        if !active.ready || active.draining {
            return false;
        }
        active.draining = true;
        true
    }

    fn mark_sync_delivery_stale(&self, session_id: Uuid, subscription_id: Uuid) {
        let app_id = {
            let mut state = sync_state(&self.inner);
            let app_id = state
                .active
                .remove(&(session_id, subscription_id))
                .map(|subscription| subscription.app);
            if let Some(app_id) = app_id {
                state.stale_subscriptions.insert(subscription_id, app_id);
            }
            app_id
        };
        if app_id.is_some() {
            self.close_runtime_session(session_id);
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keeps snapshot initialization, restart delivery, and bounded legacy delta delivery in one ordered lifecycle."
    )]
    async fn drain_claimed_sync_transactions(
        &self,
        session_id: Uuid,
        subscription_id: Uuid,
        initializing: bool,
    ) -> Result<(), ApiError> {
        let sender = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.sender.clone())
            .ok_or_else(session_missing)?;
        loop {
            let (latest_tx, transactions, restart) = {
                let mut state = sync_state(&self.inner);
                let Some(subscription) = state.active.get(&(session_id, subscription_id)).cloned()
                else {
                    return Err(session_missing());
                };
                let history = self
                    .inner
                    .sync_history
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let latest_tx = history
                    .after(subscription.app, subscription.sent_tx)
                    .map(|transaction| transaction.tx_id)
                    .max();
                let Some(latest_tx) = latest_tx else {
                    if let Some(active) = state.active.get_mut(&(session_id, subscription_id)) {
                        active.ready |= initializing;
                        active.draining = false;
                    }
                    return Ok(());
                };
                let transactions = history
                    .after(subscription.app, subscription.sent_tx)
                    .filter(|transaction| transaction.tx_id <= latest_tx)
                    .filter_map(|transaction| {
                        let changes = transaction
                            .changes
                            .iter()
                            .filter(|change| {
                                sync_change_matches(change, &subscription.attribute_ids)
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        (!changes.is_empty()).then_some(SyncTransaction {
                            app_id: transaction.app_id,
                            tx_id: transaction.tx_id,
                            changes,
                        })
                    })
                    .collect::<Vec<_>>();
                let restart = subscription.reset_on_change;
                if restart {
                    state.active.remove(&(session_id, subscription_id));
                    state
                        .stale_subscriptions
                        .insert(subscription_id, subscription.app);
                }
                (latest_tx, transactions, restart)
            };
            if restart {
                send_sync_frame(
                    &sender,
                    json!({
                        "op": "error",
                        "status": 500,
                        "client-event-id": Value::Null,
                        "original-event": {
                            "op": "resync-table",
                            "subscription-id": subscription_id,
                        },
                        "type": ApiErrorType::Unknown,
                        "message": "Sync data changed; restarting the sync table.",
                        "hint": Value::Null,
                    }),
                )
                .await?;
                return Ok(());
            }
            for batch in transactions.chunks(100) {
                let txes = batch
                    .iter()
                    .map(|transaction| {
                        json!({"tx-id": transaction.tx_id, "changes": transaction.changes})
                    })
                    .collect::<Vec<_>>();
                send_sync_frame(
                    &sender,
                    json!({
                        "op": "sync-update-triples",
                        "subscription-id": subscription_id,
                        "txes": txes,
                    }),
                )
                .await?;
                if let Some(last) = batch.last()
                    && let Some(active) = sync_state(&self.inner)
                        .active
                        .get_mut(&(session_id, subscription_id))
                {
                    active.sent_tx = active.sent_tx.max(last.tx_id);
                }
            }
            if let Some(active) = sync_state(&self.inner)
                .active
                .get_mut(&(session_id, subscription_id))
            {
                active.sent_tx = active.sent_tx.max(latest_tx);
            }
        }
    }

    async fn remove_sync(&self, session_id: Uuid, message: &Value) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let subscription_id = required_uuid(message, "subscription-id")?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let (removed, stale) = {
            let mut state = sync_state(&self.inner);
            let owned = state
                .active
                .get(&(session_id, subscription_id))
                .is_some_and(|subscription| subscription.app == app_id);
            if owned {
                state.active.remove(&(session_id, subscription_id));
            }
            let stale_owned = if state.stale_subscriptions.get(&subscription_id) == Some(&app_id) {
                state.stale_subscriptions.remove(&subscription_id);
                true
            } else {
                false
            };
            (owned, stale_owned)
        };
        let keep_subscription = optional_truthy_value(message, "keep-subscription").is_some();
        if stale || (removed && !keep_subscription) {
            RuntimeSyncBackend::delete_sync_subscription(
                self.data.as_ref(),
                AdminRequestContext {
                    app_id,
                    bearer_token: entry.bearer_token,
                    actor: entry.actor,
                    client_ip: entry.client_ip,
                },
                subscription_id,
            )
            .await?;
        }
        Ok(())
    }

    async fn refresh_sync_table(&self, session_id: Uuid, message: &Value) -> Result<(), ApiError> {
        let Some(subscription_id) = message.get("subscription-id").and_then(coerce_uuid) else {
            // This internal event destructures the id upstream instead of using
            // get-param!, so a malformed/no-longer-relevant lookup is a no-op.
            return Ok(());
        };
        if !self.claim_sync_drain(session_id, subscription_id) {
            return Ok(());
        }
        let result = self
            .drain_claimed_sync_transactions(session_id, subscription_id, false)
            .await;
        if result.is_err() {
            self.mark_sync_delivery_stale(session_id, subscription_id);
        }
        result
    }

    fn resync_table(session_id: Uuid, message: &Value) -> Result<(), ApiError> {
        let _ = session_id;
        required_long(message, "tx-id")?;
        required_uuid(message, "subscription-id")?;
        required_uuid(message, "token")?;
        // A persisted raw-triple snapshot may no longer be visible under the
        // current identity. The client clears it and starts a fresh snapshot.
        Err(ApiError::new(
            ApiErrorType::Unknown,
            "Sync subscriptions restart when reconnecting.",
        ))
    }

    /// Records raw sync-table change records and wakes every matching active
    /// subscription for the application. The WAL invalidator calls this seam.
    pub async fn publish_sync_transaction(&self, app_id: Uuid, tx_id: i64, changes: Vec<Value>) {
        let (subscriptions, stale_sessions) = {
            let mut state = sync_state(&self.inner);
            let mut history = self
                .inner
                .sync_history
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let stale_sessions = record_sync_transaction(
                &mut state,
                &mut history,
                SyncTransaction {
                    app_id,
                    tx_id,
                    changes,
                },
                SYNC_TRANSACTION_HISTORY_LIMIT,
            );
            let subscriptions = state
                .active
                .values_mut()
                .filter(|subscription| subscription.app == app_id)
                .filter(|subscription| subscription.ready && !subscription.draining)
                .map(|subscription| {
                    subscription.draining = true;
                    (subscription.session, subscription.subscription)
                })
                .collect::<Vec<_>>();
            (subscriptions, stale_sessions)
        };
        for session_id in stale_sessions {
            self.close_runtime_session(session_id);
        }
        futures_util::future::join_all(subscriptions.into_iter().map(
            |(session_id, subscription_id)| async move {
                if self
                    .drain_claimed_sync_transactions(session_id, subscription_id, false)
                    .await
                    .is_err()
                {
                    self.mark_sync_delivery_stale(session_id, subscription_id);
                }
            },
        ))
        .await;
    }

    /// Applies one committed WAL record to sync-table history and live query sessions.
    ///
    /// # Errors
    ///
    /// Returns an authorization or query error when schema reload or refresh execution fails.
    pub async fn apply_wal_record(&self, record: &WalRecord) -> Result<(), ApiError> {
        let started = tokio::time::Instant::now();
        self.inner
            .transaction_isns
            .deliver(record.tx_id, record.isn);
        let schema_refresh = wal_schema_refresh(record);
        let result = self
            .apply_wal_record_for_sessions(record, None, schema_refresh)
            .await;
        tracing::debug!(
            target: "instant_server::realtime_latency",
            tx_id = record.tx_id,
            refresh_ms = started.elapsed().as_secs_f64() * 1000.0,
            success = result.is_ok(),
            "realtime WAL delivery completed"
        );
        result
    }

    /// Applies ordered invalidation and sync history, coalescing query refreshes
    /// within windows of at most 128 records. Completion waits for every session.
    ///
    /// # Errors
    /// Returns an invalidation or refresh error; the caller must replay rather
    /// than acknowledge an unsuccessful batch.
    pub async fn apply_wal_records(&self, records: &[WalRecord]) -> Result<(), ApiError> {
        for records in records.chunks(MAX_WAL_REFRESH_BATCH) {
            let prepared = crate::postgres_pool::DatabaseWorkload::Refresh
                .scope(self.prepare_wal_batch(records))
                .await?;
            let preparation_ms = prepared.started.elapsed().as_secs_f64() * 1000.0;
            let selected_sessions = prepared.refreshes.len();
            let refresh_started = tokio::time::Instant::now();
            // Await every completion, including when one refresh fails. The caller
            // must never acknowledge a batch with unfinished session work.
            let results = futures_util::future::join_all(prepared.refreshes.into_iter().map(
                |(session_id, (tx_id, isn))| {
                    self.enqueue_refresh(session_id, json!(tx_id), json!(isn))
                },
            ))
            .await;
            let result = results.into_iter().collect::<Result<(), ApiError>>();
            tracing::debug!(
                target: "instant_server::realtime_latency",
                batch_records = records.len(),
                selected_sessions,
                coalesced_refreshes = prepared.selected_refreshes - selected_sessions,
                preparation_ms,
                refresh_ms = refresh_started.elapsed().as_secs_f64() * 1000.0,
                total_ms = prepared.started.elapsed().as_secs_f64() * 1000.0,
                success = result.is_ok(),
                "realtime WAL batch completed"
            );
            result?;
        }
        Ok(())
    }

    async fn prepare_wal_batch(&self, records: &[WalRecord]) -> Result<PreparedWalBatch, ApiError> {
        let mut prepared = PreparedWalBatch {
            refreshes: BTreeMap::new(),
            selected_refreshes: 0,
            records: records.len(),
            started: tokio::time::Instant::now(),
        };
        let mut catalogs = BTreeMap::new();
        for (index, record) in records.iter().enumerate() {
            if index % MAX_WAL_CATALOG_BATCH == 0 {
                catalogs.clear();
            }
            if !sessions(&self.inner).has_app(record.app_id) {
                catalogs.remove(&record.app_id);
                self.publish_sync_transaction(
                    record.app_id,
                    record.tx_id,
                    sync_changes_from_wal(record),
                )
                .await;
                continue;
            }
            self.inner
                .transaction_isns
                .deliver(record.tx_id, record.isn);
            let selected = self
                .prepare_wal_record_for_sessions(
                    record,
                    None,
                    wal_schema_refresh(record),
                    &mut catalogs,
                )
                .await?;
            prepared.selected_refreshes += selected.len();
            for session_id in selected {
                prepared
                    .refreshes
                    .insert(session_id, (record.tx_id, record.isn));
            }
        }
        Ok(prepared)
    }

    async fn schedule_wal_batch(
        &self,
        records: &[WalRecord],
        window_id: i64,
    ) -> Result<WalCompletion, ApiError> {
        let prepared = crate::postgres_pool::DatabaseWorkload::Refresh
            .scope(self.prepare_wal_batch(records))
            .await?;
        let preparation_ms = prepared.started.elapsed().as_secs_f64() * 1000.0;
        let selected_sessions = prepared.refreshes.len();
        // Queue in WAL order before preparing the next window. Each session
        // worker preserves this order while unrelated sessions finish freely.
        let completions = futures_util::future::join_all(prepared.refreshes.into_iter().map(
            |(session_id, (tx_id, isn))| {
                self.enqueue_durable_refresh(session_id, tx_id, isn, window_id)
            },
        ))
        .await;
        Ok(Box::pin(async move {
            let refresh_started = tokio::time::Instant::now();
            // Closed receivers mean the session disconnected; its worker no
            // longer has a client-visible obligation. Durable workers retry all
            // remaining execution failures before resolving these receipts.
            let _disconnected = futures_util::future::join_all(completions).await;
            tracing::debug!(
                target: "instant_server::realtime_latency",
                batch_records = prepared.records,
                selected_sessions,
                coalesced_refreshes = prepared.selected_refreshes - selected_sessions,
                preparation_ms,
                refresh_ms = refresh_started.elapsed().as_secs_f64() * 1000.0,
                total_ms = prepared.started.elapsed().as_secs_f64() * 1000.0,
                "realtime WAL window completed"
            );
        }))
    }

    /// Applies the precise session selection produced by the realtime invalidation kernel.
    ///
    /// # Errors
    ///
    /// Returns an authorization or query error when schema reload or refresh execution fails.
    pub async fn apply_replication_outcome(
        &self,
        outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), ApiError> {
        let Some(record) = &outcome.record else {
            return Ok(());
        };
        let refresh_sessions = outcome
            .dispatch
            .as_ref()
            .map(|dispatch| &dispatch.refresh_sessions);
        let schema_refresh = outcome.dispatch.as_ref().map_or_else(
            || wal_schema_refresh(record),
            |dispatch| dispatch.schema_refresh,
        );
        self.apply_wal_record_for_sessions(record, refresh_sessions, schema_refresh)
            .await
    }

    fn apply_cache_eviction(&self, eviction: &WalCacheEviction) {
        let (app_id, changed_status) = match eviction {
            WalCacheEviction::Attribute { app_id, .. } | WalCacheEviction::Rules { app_id } => {
                (*app_id, None)
            }
            WalCacheEviction::App {
                app_id,
                changed_status,
            } => (*app_id, changed_status.as_ref()),
            WalCacheEviction::StreamMachine {
                stream_id,
                machine_id,
            } => {
                if *machine_id != self.inner.machine_id {
                    // A reconnect on another machine owns the durable buffer now. Drop
                    // only local writer ownership: readers remain registered for cluster
                    // delivery, and cleanup cannot flush on behalf of the stale writer.
                    stream_state(&self.inner).writers.remove(stream_id);
                }
                return;
            }
            // Rust reads users and webhook configuration directly from PostgreSQL;
            // there is no corresponding process-local model cache to invalidate.
            WalCacheEviction::TransactionMaxSeen { tx_id } => {
                if let Some(tx_id) = tx_id {
                    self.inner.watermark_seed.observe_tx_id(*tx_id);
                }
                return;
            }
            WalCacheEviction::InstantUser { .. } | WalCacheEviction::Webhook { .. } => return,
        };
        let Some(app_id) = app_id else {
            return;
        };
        clear_app_query_cache(&self.inner, app_id);
        let Some(status) = changed_status else {
            return;
        };
        let senders = {
            let entries = sessions(&self.inner);
            entries
                .ids_for_app(app_id)
                .filter_map(|id| entries.get(&id))
                .map(|entry| entry.sender.clone())
                .collect::<Vec<_>>()
        };
        for sender in senders {
            send(
                &sender,
                json!({"op": "app-status-changed", "status": status}),
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_wal_record_for_sessions(
        &self,
        record: &WalRecord,
        refresh_sessions: Option<&BTreeSet<Uuid>>,
        schema_refresh: bool,
    ) -> Result<(), ApiError> {
        let selected = crate::postgres_pool::DatabaseWorkload::Refresh
            .scope(self.prepare_wal_record_for_sessions(
                record,
                refresh_sessions,
                schema_refresh,
                &mut BTreeMap::new(),
            ))
            .await?;
        let results = futures_util::future::join_all(selected.into_iter().map(|session_id| {
            self.enqueue_refresh(session_id, json!(record.tx_id), json!(record.isn))
        }))
        .await;
        results.into_iter().collect()
    }

    async fn prepare_wal_record_for_sessions(
        &self,
        record: &WalRecord,
        refresh_sessions: Option<&BTreeSet<Uuid>>,
        schema_refresh: bool,
        catalogs: &mut BTreeMap<Uuid, instant_instaql::AttributeCatalog>,
    ) -> Result<Vec<Uuid>, ApiError> {
        let tx_id = u64::try_from(record.tx_id).map_err(|_| {
            ApiError::new(ApiErrorType::Unknown, "committed tx-id cannot be negative")
        })?;
        // Metadata is shared only within this ordered batch. Schema events
        // invalidate it before later records can use the changed attributes.
        if schema_refresh {
            catalogs.remove(&record.app_id);
        }
        self.publish_sync_transaction(record.app_id, record.tx_id, sync_changes_from_wal(record))
            .await;
        let invalidation_topics: Option<Arc<[instant_wal::Topic]>> = (refresh_sessions.is_none()
            && !schema_refresh)
            .then(|| topics_for_record(record))
            .transpose()
            .map_err(internal_error)?
            .map(Arc::from);
        let needs_entity_images =
            !schema_refresh && (!record.messages.is_empty() || !record.wal_logs.is_empty()) && {
                let entries = sessions(&self.inner);
                wal_candidate_sessions(
                    &self.inner,
                    &entries,
                    record.app_id,
                    refresh_sessions,
                    invalidation_topics.as_deref(),
                )
                .iter()
                .filter_map(|id| entries.get(id))
                .flat_map(|entry| entry.queries.values())
                .any(|query| query.invalidation_predicate.is_some())
            };
        let entity_images = if needs_entity_images {
            if !catalogs.contains_key(&record.app_id)
                && let Ok(catalog) = self.authorizer.load_catalog(record.app_id).await
            {
                catalogs.insert(record.app_id, catalog);
            }
            // Failed loads are not cached: this record refreshes conservatively
            // and the next one may retry. Authorization decisions are never reused.
            catalogs
                .get(&record.app_id)
                .and_then(|catalog| wal_entity_images(record, catalog))
        } else {
            None
        };
        let (refresh_session_ids, filter_summary, candidate_sessions) = select_wal_refresh_sessions(
            &self.inner,
            record,
            tx_id,
            refresh_sessions,
            schema_refresh,
            invalidation_topics.as_ref(),
            entity_images.as_ref(),
        );
        #[cfg(test)]
        self.inner
            .last_invalidation_candidates
            .store(filter_summary.initial_count, Ordering::Relaxed);
        if filter_summary.initial_count > 0 {
            tracing::debug!(
                event_name = "store/filter-queries-summary",
                app_id = %record.app_id,
                candidate_sessions,
                indexed_routing = invalidation_topics.is_some(),
                initial_count = filter_summary.initial_count,
                topic_removed = filter_summary.topic_removed,
                iq_topic_removed = filter_summary.predicate_removed,
                iq_topic_matched = filter_summary.predicate_matched,
                iq_topic_fail_open = filter_summary.predicate_fail_open,
                conservative_refresh = filter_summary.conservative_refresh,
                final_count = filter_summary.initial_count
                    - filter_summary.topic_removed
                    - filter_summary.predicate_removed,
                "filtered realtime queries for WAL refresh"
            );
        }
        Ok(refresh_session_ids)
    }

    async fn apply_byop_record(
        &self,
        app_id: Uuid,
        record: &instant_pg::ByopWalRecord,
        topics: &[instant_pg::ByopTopic],
    ) -> Result<(), ApiError> {
        let root_frame_cleanup = invalidate_app_root_frames(&self.inner, app_id, true, None);
        let cache_cleanup;
        let refresh_session_ids = {
            let mut entries = sessions(&self.inner);
            cache_cleanup = invalidate_app_query_reuse(&self.inner, app_id, true, None);
            advance_processed_watermark(&self.inner, app_id, record.tx_id, None);
            let mut refresh_session_ids = Vec::new();
            let session_ids = entries.ids_for_app(app_id).collect::<Vec<_>>();
            for session_id in session_ids {
                let entry = entries
                    .get_mut(&session_id)
                    .expect("indexed session exists");
                entry.next_refresh_generation = entry.next_refresh_generation.wrapping_add(1);
                let generation = entry.next_refresh_generation;
                let mut selected = false;
                for query in entry.queries.values_mut() {
                    if query.byop_topics.as_ref().is_none_or(|query_topics| {
                        instant_pg::byop_topics_intersect(topics, query_topics)
                    }) {
                        query.stale_generation = generation;
                        selected = true;
                    }
                }
                if selected {
                    refresh_session_ids.push(session_id);
                }
            }
            refresh_session_ids
        };
        drop(cache_cleanup);
        drop(root_frame_cleanup);
        let results =
            futures_util::future::join_all(refresh_session_ids.into_iter().map(|session_id| {
                self.enqueue_refresh(session_id, json!(record.tx_id), Value::Null)
            }))
            .await;
        results.into_iter().collect()
    }

    /// Recovers conservatively when the cluster broadcast receiver drops one
    /// or more events. Exact topics and transaction watermarks are no longer
    /// available, so every local query is recomputed once and reported with an
    /// unknown processed watermark.
    async fn recover_cluster_lag(&self) {
        self.recover_sync_subscriptions_after_cluster_lag();
        let app_ids = sessions(&self.inner).app_ids().collect::<BTreeSet<_>>();
        for app_id in app_ids {
            clear_app_query_cache(&self.inner, app_id);
            let session_ids = {
                let mut entries = sessions(&self.inner);
                let session_ids = entries.ids_for_app(app_id).collect::<Vec<_>>();
                for session_id in &session_ids {
                    let entry = entries.get_mut(session_id).expect("indexed session exists");
                    entry.next_refresh_generation = entry.next_refresh_generation.wrapping_add(1);
                    let generation = entry.next_refresh_generation;
                    for query in entry.queries.values_mut() {
                        query.stale_generation = generation;
                    }
                    entry.refresh_attrs_generation = generation;
                    entry.refresh_force_generation = generation;
                }
                session_ids
            };
            for session_id in session_ids {
                if let Err(error) = self
                    .enqueue_refresh(session_id, Value::Null, Value::Null)
                    .await
                {
                    self.send_error(session_id, None, &Value::Null, &error);
                }
            }
        }
    }

    fn recover_sync_subscriptions_after_cluster_lag(&self) {
        let subscriptions = {
            let mut state = sync_state(&self.inner);
            let subscriptions = state
                .active
                .values_mut()
                .filter(|subscription| subscription.ready)
                .map(|subscription| {
                    subscription.ready = false;
                    (
                        subscription.session,
                        subscription.subscription,
                        subscription.app,
                    )
                })
                .collect::<Vec<_>>();
            state.stale_subscriptions.extend(
                subscriptions
                    .iter()
                    .map(|(_, subscription_id, app_id)| (*subscription_id, *app_id)),
            );
            subscriptions
        };
        for (session_id, subscription_id, _) in subscriptions {
            let Some(sender) = sessions(&self.inner)
                .get(&session_id)
                .map(|entry| entry.sender.clone())
            else {
                continue;
            };
            let message = json!({
                "op": "error",
                "status": 500,
                "client-event-id": Value::Null,
                "original-event": {
                    "op": "resync-table",
                    "subscription-id": subscription_id,
                },
                "type": ApiErrorType::Unknown,
                "message": "Realtime updates were interrupted; restarting the sync table.",
                "hint": Value::Null,
            });
            send(&sender, message);
        }
    }

    async fn wait_for_refresh_slot(&self, session_id: Uuid) {
        let deadline = sessions(&self.inner)
            .get_mut(&session_id)
            .and_then(|entry| entry.refresh_not_before.take());
        if let Some(deadline) = deadline {
            tokio::time::sleep_until(deadline).await;
        }
    }

    async fn throttle_noop_sessions(
        &self,
        app_id: Uuid,
        session_ids: &[Uuid],
    ) -> Result<(), ApiError> {
        if session_ids.is_empty() {
            return Ok(());
        }
        let Some(delay) = self.refresh_throttle.delay_after_noop(app_id).await? else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + delay;
        let mut current = sessions(&self.inner);
        for session_id in session_ids {
            if let Some(entry) = current.get_mut(session_id) {
                entry.refresh_not_before = Some(deadline);
            }
        }
        Ok(())
    }

    async fn start_stream(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let client_id = required_non_blank_string(message, "client-id")?;
        self.data
            .authorize_stream_create(
                entry_context(&entry, app_id),
                client_id.clone(),
                optional_truthy_value(message, "rule-params")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
            .await?;
        let reconnect_token = required_uuid(message, "reconnect-token")?;
        let record = self
            .stream_backend
            .start_stream(RuntimeStreamStartRequest {
                context: entry_context(&entry, app_id),
                client_id,
                reconnect_token,
                machine_id: self.inner.machine_id,
            })
            .await?;
        stream_state(&self.inner)
            .writers
            .insert(record.stream_id, session_id);
        send(
            &entry.sender,
            with_client_id(
                json!({
                    "op": "start-stream-ok",
                    "client-id": record.client_id,
                    "stream-id": record.stream_id,
                    "offset": record.size,
                }),
                client_event_id,
            ),
        );
        Ok(())
    }

    async fn append_stream(&self, session_id: Uuid, message: &Value) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let stream_id = required_uuid(message, "stream-id")?;
        let done = optional_bool(message, "done")?.unwrap_or(false);
        let abort_reason = optional_truthy_value(message, "abort-reason")
            .map(|value| coerce_abort_reason(value, "abort-reason"))
            .transpose()?;
        let expected_offset = required_non_negative_u64(message, "offset")?;
        let chunks = required_string_array(message, "chunks")?;
        if stream_state(&self.inner).writers.get(&stream_id) != Some(&session_id) {
            return Err(protocol_error("Stream updated from a different machine."));
        }
        let appended = self
            .stream_backend
            .append_stream(RuntimeStreamAppendRequest {
                context: AdminRequestContext {
                    app_id,
                    bearer_token: entry.bearer_token,
                    actor: entry.actor,
                    client_ip: entry.client_ip,
                },
                stream_id,
                machine_id: self.inner.machine_id,
                expected_offset,
                chunks,
                done,
                abort_reason,
            })
            .await?;
        let readers = stream_state(&self.inner)
            .readers
            .values()
            .filter(|reader| reader.stream == stream_id)
            .cloned()
            .collect::<Vec<_>>();
        for reader in readers {
            if let Some(sender) = sessions(&self.inner)
                .get(&reader.session)
                .map(|session| session.sender.clone())
            {
                if !appended.content.is_empty() {
                    send(
                        &sender,
                        json!({
                            "op": "stream-append",
                            "client-event-id": reader.event,
                            "stream-id": stream_id,
                            "client-id": appended.client_id,
                            "offset": appended.offset,
                            "content": appended.content,
                        }),
                    );
                }
                if appended.done {
                    send(
                        &sender,
                        json!({
                            "op": "stream-append",
                            "client-event-id": reader.event,
                            "stream-id": stream_id,
                            "client-id": appended.client_id,
                            "done": true,
                            "abort-reason": appended.abort_reason,
                        }),
                    );
                }
            }
        }
        if let Some(cluster) = &self.cluster {
            // The backend has consumed the offset already. Event publication is
            // best-effort so a transient cluster failure cannot make the writer
            // retry an append that actually succeeded.
            let _ = cluster
                .publish_stream_append(app_id, appended.clone())
                .await;
        }
        if let Some(flushed_offset) = appended.flushed_offset {
            send(
                &entry.sender,
                json!({
                    "op": "stream-flushed",
                    "stream-id": stream_id,
                    "offset": flushed_offset,
                    "done": appended.done,
                }),
            );
        }
        Ok(())
    }

    async fn subscribe_stream(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<&Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let requested_stream_id = optional_uuid(message, "stream-id")?;
        let requested_client_id = optional_non_blank_string(message, "client-id")?;
        if requested_stream_id.is_none() && requested_client_id.is_none() {
            return Err(protocol_error(
                "Must provide either a stream-id or a client-id",
            ));
        }
        let requested_offset = optional_non_negative_u64(message, "offset")?.unwrap_or(0);
        let subscribe_event_id = client_event_id
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| protocol_error("Invalid client-event-id"))?
            .to_owned();
        let record = self
            .stream_backend
            .load_stream(RuntimeStreamLoadRequest {
                context: entry_context(&entry, app_id),
                stream_id: requested_stream_id,
                client_id: requested_client_id,
                requested_offset,
            })
            .await?;
        self.data
            .authorize_stream_view(
                entry_context(&entry, app_id),
                stream_permission_data(&record),
                optional_truthy_value(message, "rule-params")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
            .await?;
        if !record.done {
            let mut state = stream_state(&self.inner);
            state.readers.insert(
                (session_id, subscribe_event_id.clone()),
                StreamReader {
                    session: session_id,
                    stream: record.stream_id,
                    event: subscribe_event_id.clone(),
                },
            );
        }
        let files = record
            .files
            .iter()
            .map(|file| json!({"id": file.id, "size": file.size, "url": file.url}))
            .collect::<Vec<_>>();
        send(
            &entry.sender,
            json!({
                "op": "stream-append",
                "client-event-id": subscribe_event_id,
                "stream-id": record.stream_id,
                "client-id": record.client_id,
                "offset": record.replay_offset,
                "files": files,
                "content": record.content,
                "done": record.done,
                "abort-reason": record.abort_reason,
            }),
        );
        Ok(())
    }

    fn unsubscribe_stream(&self, session_id: Uuid, message: &Value) -> Result<(), ApiError> {
        let _entry = initialized_entry(&self.inner, session_id)?;
        let subscribe_event_id = optional_non_blank_string(message, "subscribe-event-id")?;
        let Some(subscribe_event_id) = subscribe_event_id else {
            return Err(protocol_error("Stream subscription is missing."));
        };
        if stream_state(&self.inner)
            .readers
            .remove(&(session_id, subscribe_event_id))
            .is_none()
        {
            return Err(protocol_error("Stream subscription is missing."));
        }
        Ok(())
    }

    async fn join_room(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        self.app_status
            .status(app_id)
            .await?
            .effective_status
            .assert_read_allowed()?;
        let room_id = required_string(message, "room-id")?;
        let mut data = message.get("data").cloned().unwrap_or_else(|| json!({}));
        if data.is_null() || data.as_bool() == Some(false) {
            data = json!({});
        }
        let update = if let Some(cluster) = &self.cluster {
            let room = RoomKey::new(app_id, room_id);
            rooms(&self.inner).register_local_session(room.clone(), session_id);
            let peer = PeerPresence {
                peer_id: session_id,
                instance_id: self.inner.machine_id.to_string(),
                user: entry.user_id.map(|id| PeerUser { id }),
                data,
            };
            let snapshot = match cluster.join(&room, peer).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    rooms(&self.inner).unregister_local_session(&room, session_id);
                    return Err(cluster_error(error));
                }
            };
            rooms(&self.inner).apply_versioned_remote_snapshot(
                &room,
                snapshot.revision,
                snapshot.peers,
            )
        } else {
            rooms(&self.inner)
                .join(
                    app_id,
                    room_id,
                    session_id,
                    self.inner.machine_id.to_string(),
                    entry.user_id,
                    data,
                )
                .map_err(room_error)?
        };
        self.deliver_presence(update);
        self.join_room_logger.log_join(app_id);
        send(
            &entry.sender,
            with_client_id(
                json!({"op": "join-room-ok", "room-id": room_id}),
                client_event_id,
            ),
        );
        Ok(())
    }

    async fn leave_room(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let room_id = required_string(message, "room-id")?;
        let update = if let Some(cluster) = &self.cluster {
            let room = RoomKey::new(app_id, room_id);
            let snapshot = cluster
                .leave(&room, session_id)
                .await
                .map_err(cluster_error)?;
            let update = rooms(&self.inner).apply_versioned_remote_snapshot(
                &room,
                snapshot.revision,
                snapshot.peers,
            );
            rooms(&self.inner).unregister_local_session(&room, session_id);
            update
        } else {
            rooms(&self.inner)
                .leave(app_id, room_id, session_id)
                .map_err(room_error)?
        };
        self.deliver_presence(update);
        send(
            &entry.sender,
            with_client_id(
                json!({"op": "leave-room-ok", "room-id": room_id}),
                client_event_id,
            ),
        );
        Ok(())
    }

    async fn set_presence(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let room_id = required_string(message, "room-id")?;
        let data = message.get("data").cloned().unwrap_or(Value::Null);
        let update = if let Some(cluster) = &self.cluster {
            if !rooms(&self.inner).in_room(app_id, room_id, session_id) {
                return Err(room_error(instant_realtime::RoomError::NotInRoom {
                    session_id,
                    room_id: room_id.to_owned(),
                }));
            }
            let room = RoomKey::new(app_id, room_id);
            let snapshot = cluster
                .set_presence(&room, session_id, data)
                .await
                .map_err(cluster_error)?;
            rooms(&self.inner).apply_versioned_remote_snapshot(
                &room,
                snapshot.revision,
                snapshot.peers,
            )
        } else {
            rooms(&self.inner)
                .set_presence(app_id, room_id, session_id, data)
                .map_err(room_error)?
        };
        self.deliver_presence(update);
        send(
            &entry.sender,
            with_client_id(
                json!({"op": "set-presence-ok", "room-id": room_id}),
                client_event_id,
            ),
        );
        Ok(())
    }

    async fn broadcast(
        &self,
        session_id: Uuid,
        message: &Value,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let entry = initialized_entry(&self.inner, session_id)?;
        let app_id = entry.app_id.expect("initialized entry has app id");
        let room_id = required_string(message, "room-id")?;
        let plan = rooms(&self.inner)
            .broadcast(
                app_id,
                room_id,
                session_id,
                entry.user_id,
                message.get("topic").cloned().unwrap_or(Value::Null),
                message.get("data").cloned().unwrap_or(Value::Null),
            )
            .map_err(room_error)?;
        for recipient in plan.local_session_ids {
            if let Some(sender) = sessions(&self.inner)
                .get(&recipient)
                .map(|entry| entry.sender.clone())
            {
                send(
                    &sender,
                    json!({
                        "op": "server-broadcast",
                        "room-id": plan.recipient_payload.room_id,
                        "topic": plan.recipient_payload.topic,
                        "data": plan.recipient_payload.data,
                    }),
                );
            }
        }
        if let Some(cluster) = &self.cluster {
            record_runtime_broadcast(&self.inner, session_id);
            if let Err(error) = cluster
                .publish_broadcast(
                    &RoomKey::new(app_id, room_id),
                    plan.recipient_payload.clone(),
                )
                .await
            {
                consume_runtime_broadcast(&self.inner, session_id);
                return Err(cluster_error(error));
            }
        }
        send(
            &entry.sender,
            with_client_id(
                json!({
                    "op": "client-broadcast-ok",
                    "room-id": plan.sender_ack.room_id,
                    "topic": plan.sender_ack.topic,
                    "data": plan.sender_ack.data,
                }),
                client_event_id,
            ),
        );
        Ok(())
    }

    fn deliver_presence(&self, update: PresenceUpdate) {
        deliver_presence_update(&self.inner, update);
    }

    fn send_error(
        &self,
        session_id: Uuid,
        client_event_id: Option<&Value>,
        original: &Value,
        error: &ApiError,
    ) {
        let (sender, app_id) = sessions(&self.inner)
            .get(&session_id)
            .map_or((None, None), |entry| {
                (Some(entry.sender.clone()), entry.app_id)
            });
        let app_id = app_id.map_or_else(|| "unknown".to_owned(), |app_id| app_id.to_string());
        let status = error.status();
        let error_type = error.body().kind;
        // `op` is client-supplied; bound it before it reaches the logs.
        let op: String = original
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .chars()
            .take(64)
            .collect();
        if status.is_server_error() {
            tracing::error!(
                event_name = "realtime/error",
                app_id = %app_id,
                op = %op,
                %session_id,
                error_type = ?error_type,
                status = status.as_u16(),
                "realtime error"
            );
        } else {
            tracing::warn!(
                event_name = "realtime/error",
                app_id = %app_id,
                op = %op,
                %session_id,
                error_type = ?error_type,
                status = status.as_u16(),
                "realtime error"
            );
        }
        let Some(sender) = sender else {
            return;
        };
        let mut body = json!({
            "op": "error",
            "status": status.as_u16(),
            "client-event-id": client_event_id,
            "original-event": original,
            "type": error_type,
            "message": error.body().message,
            "hint": error.body().hint,
        });
        if let Some(trace_id) = instant_http::request_id::current_request_id() {
            body["trace-id"] = Value::String(trace_id);
        }
        send(&sender, body);
    }

    fn send_refresh_error(
        &self,
        session_id: Uuid,
        processed_tx_id: &Value,
        processed_isn: &Value,
        error: &ApiError,
    ) {
        // The client only sees an opaque error event; keep the cause in the
        // server log so a failed refresh can be diagnosed.
        tracing::warn!(
            event_name = "realtime/refresh-error",
            %session_id,
            error_type = ?error.body().kind,
            message = %error.body().message,
            "realtime refresh failed"
        );
        self.send_error(
            session_id,
            None,
            &json!({
                "op": "refresh",
                "session-id": session_id,
                "processed-tx-id": processed_tx_id,
                "processed-isn": processed_isn,
            }),
            error,
        );
    }
}

fn query_topics_for_mode(result: &SessionQueryResult, use_coarse: bool) -> Option<Vec<QueryTopic>> {
    let topics = result.instant_topics.as_ref()?;
    let selected = if use_coarse {
        &topics.coarse
    } else {
        &topics.precise
    };
    Some(selected.iter().map(QueryTopic::from).collect())
}

fn matching_transaction_steps(
    left: &[Value],
    right: &[Value],
    catalog: &instant_instaql::AttributeCatalog,
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            let (Some(left), Some(right)) = (left.as_array(), right.as_array()) else {
                return false;
            };
            if left.len() != 5
                || right.len() != 5
                || left[0] != "add-triple"
                || right[0] != "add-triple"
                || left[1] != right[1]
                || left[2] != right[2]
                || left[4] != right[4]
            {
                return false;
            }
            left[2]
                .as_str()
                .and_then(|attribute_id| Uuid::parse_str(attribute_id).ok())
                .and_then(|attribute_id| catalog.by_id(attribute_id))
                .is_some_and(|attribute| attribute.cardinality == Cardinality::One)
        })
}

fn compile_query_invalidation_predicate(
    query: &Map<String, Value>,
    catalog: &instant_instaql::AttributeCatalog,
) -> Option<InvalidationPredicate> {
    let query = Value::Object(query.clone());
    let forms = coerce_forms(&query, catalog).ok()?;
    compile_invalidation_predicate(&forms, catalog).ok()
}

fn wal_entity_images(
    record: &WalRecord,
    catalog: &instant_instaql::AttributeCatalog,
) -> Option<(EntitySnapshot, EntitySnapshot)> {
    let after = extract_entities_after(record).ok()?;
    let before = extract_entities_before(catalog, after.clone(), record).ok()?;
    Some((before, after))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalQueryMatch {
    TopicMiss,
    ConservativeRefresh,
    PredicateMatch,
    PredicateFailOpen,
    PredicateMiss,
}

impl WalQueryMatch {
    const fn should_refresh(self) -> bool {
        !matches!(self, Self::TopicMiss | Self::PredicateMiss)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct InvalidationFilterSummary {
    initial_count: usize,
    topic_removed: usize,
    predicate_removed: usize,
    predicate_matched: usize,
    predicate_fail_open: usize,
    conservative_refresh: usize,
}

impl InvalidationFilterSummary {
    fn observe(&mut self, outcome: WalQueryMatch) {
        self.initial_count += 1;
        match outcome {
            WalQueryMatch::TopicMiss => self.topic_removed += 1,
            WalQueryMatch::ConservativeRefresh => self.conservative_refresh += 1,
            WalQueryMatch::PredicateMatch => self.predicate_matched += 1,
            WalQueryMatch::PredicateFailOpen => self.predicate_fail_open += 1,
            WalQueryMatch::PredicateMiss => self.predicate_removed += 1,
        }
    }
}

fn query_wal_match(
    query: &RegisteredQuery,
    topics: Option<&[instant_wal::Topic]>,
    entity_images: Option<&(EntitySnapshot, EntitySnapshot)>,
) -> WalQueryMatch {
    if topics.is_some_and(|topics| {
        query
            .instant_topics
            .as_ref()
            .is_some_and(|dependencies| !topics_intersect(topics, dependencies))
    }) {
        return WalQueryMatch::TopicMiss;
    }
    let Some(predicate) = &query.invalidation_predicate else {
        return WalQueryMatch::ConservativeRefresh;
    };
    let Some((before, after)) = entity_images else {
        return WalQueryMatch::PredicateFailOpen;
    };
    match (
        snapshot_predicate_outcome(predicate, before),
        snapshot_predicate_outcome(predicate, after),
    ) {
        (InvalidationPredicateOutcome::Match, _) | (_, InvalidationPredicateOutcome::Match) => {
            WalQueryMatch::PredicateMatch
        }
        (InvalidationPredicateOutcome::FailOpen, _)
        | (_, InvalidationPredicateOutcome::FailOpen) => WalQueryMatch::PredicateFailOpen,
        (InvalidationPredicateOutcome::NoMatch, InvalidationPredicateOutcome::NoMatch) => {
            WalQueryMatch::PredicateMiss
        }
    }
}

fn snapshot_predicate_outcome(
    predicate: &InvalidationPredicate,
    snapshot: &EntitySnapshot,
) -> InvalidationPredicateOutcome {
    let mut outcome = InvalidationPredicateOutcome::NoMatch;
    for (etype, entities) in snapshot {
        for attrs in entities.values() {
            match predicate.match_outcome(&InvalidationEntity {
                etype: etype.clone(),
                attrs: attrs.clone(),
            }) {
                InvalidationPredicateOutcome::Match => {
                    return InvalidationPredicateOutcome::Match;
                }
                InvalidationPredicateOutcome::FailOpen => {
                    outcome = InvalidationPredicateOutcome::FailOpen;
                }
                InvalidationPredicateOutcome::NoMatch => {}
            }
        }
    }
    outcome
}

impl<A, D> crate::replication::RealtimeInvalidationConsumer for RuntimeRealtimeService<A, D>
where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeJoinRowsQueryBackend
        + RuntimeTreeQueryBackend
        + RuntimeSyncBackend
        + RuntimeStreamPermissionBackend
        + RuntimeRealtimeCatalogBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend,
{
    fn requires_local_delivery(&self) -> bool {
        self.cluster.is_none()
    }

    async fn stage(
        &self,
        outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), String> {
        let Some(record) = &outcome.record else {
            return Ok(());
        };
        let started = tokio::time::Instant::now();
        if let Some(cluster) = &self.cluster {
            let result = cluster.publish_wal(record.clone()).await;
            tracing::debug!(
                target: "instant_server::realtime_latency",
                tx_id = record.tx_id,
                stage_ms = started.elapsed().as_secs_f64() * 1000.0,
                success = result.is_ok(),
                "realtime committed WAL publication completed"
            );
            result?;
        }
        // The sink persisted this committed WAL before staging it. Production
        // APIs each consume their own invalidator slot, independently of the
        // ordered cluster refresh lane. Release writers after durable staging;
        // query results and processed watermarks still advance only on delivery.
        self.inner
            .transaction_isns
            .deliver(record.tx_id, record.isn);
        Ok(())
    }

    async fn stage_persisted(
        &self,
        outcome: &instant_realtime::ReplicationPipelineOutcome,
        published_machine: Option<Uuid>,
    ) -> Result<(), String> {
        if self.cluster.is_some() && published_machine == Some(self.inner.machine_id) {
            if let Some(record) = &outcome.record {
                self.inner
                    .transaction_isns
                    .deliver(record.tx_id, record.isn);
            }
            return Ok(());
        }
        self.stage(outcome).await
    }

    async fn deliver(
        &self,
        outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), String> {
        if self.cluster.is_some() {
            return Ok(());
        }
        if let Some(record) = &outcome.record {
            // The authoritative live registry owns the dependencies returned by
            // native query execution. Derive selection from the raw record here
            // so local replication and cluster-delivered WAL use the same index.
            self.apply_wal_record(record)
                .await
                .map_err(|error| error.body().message.clone())?;
        }
        Ok(())
    }

    async fn deliver_byop(
        &self,
        app_id: Uuid,
        record: &instant_pg::ByopWalRecord,
        topics: &[instant_pg::ByopTopic],
    ) -> Result<(), String> {
        if self.cluster.is_some() {
            return Ok(());
        }
        self.apply_byop_record(app_id, record, topics)
            .await
            .map_err(|error| error.body().message.clone())
    }

    async fn stage_byop(
        &self,
        app_id: Uuid,
        record: &instant_pg::ByopWalRecord,
        topics: &[instant_pg::ByopTopic],
    ) -> Result<(), String> {
        if let Some(cluster) = &self.cluster {
            cluster
                .publish_byop_wal(app_id, record.clone(), topics.to_vec())
                .await?;
        }
        Ok(())
    }

    fn evict_cache(
        &self,
        eviction: &WalCacheEviction,
    ) -> impl Future<Output = Result<(), String>> + Send {
        self.apply_cache_eviction(eviction);
        std::future::ready(Ok(()))
    }

    fn observe_max_seen_isn(&self, isn: Isn) -> impl Future<Output = Result<(), String>> + Send {
        self.inner.watermark_seed.observe_isn(isn);
        std::future::ready(Ok(()))
    }
}

impl<A, D> RuntimeRealtimeBackend for RuntimeRealtimeService<A, D>
where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeJoinRowsQueryBackend
        + RuntimeTreeQueryBackend
        + RuntimeSyncBackend
        + RuntimeStreamPermissionBackend
        + RuntimeRealtimeCatalogBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend,
{
    fn open(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
    ) -> impl std::future::Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
        self.open_runtime_session(app_id, transport, None, None)
    }

    fn open_with_origin(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
        origin: Option<String>,
    ) -> impl std::future::Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
        self.open_runtime_session(app_id, transport, origin, None)
    }

    fn open_with_client(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
        origin: Option<String>,
        client_ip: Option<IpAddr>,
    ) -> impl std::future::Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
        self.open_runtime_session(app_id, transport, origin, client_ip)
    }

    fn drain_signal(&self) -> RuntimeDrainSignal {
        self.inner.drain.clone()
    }

    async fn message(&self, session_id: Uuid, message: Value) -> Result<(), ApiError> {
        self.enqueue_grouped_message(session_id, message).await
    }

    async fn push(&self, request: RuntimeSsePushRequest) -> Result<(), ApiError> {
        if request.machine_id != self.inner.machine_id {
            let Some(cluster) = &self.cluster else {
                return Err(session_missing());
            };
            let accepted = cluster
                .forward_sse_push(request)
                .await
                .map_err(cluster_error)?;
            if accepted {
                return Ok(());
            }
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Invalid SSE session credentials",
            )
            .unauthorized());
        }
        let (app_id, token_hash) = sessions(&self.inner)
            .get(&request.session_id)
            .map(|entry| (entry.app_id, entry.token_hash))
            .ok_or_else(session_missing)?;
        if app_id != Some(request.app_id)
            || !token_hash.is_some_and(|expected| {
                constant_time_eq(&expected, &Sha256::digest(request.sse_token.as_bytes()))
            })
        {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Invalid SSE session credentials",
            )
            .unauthorized());
        }
        for message in request.messages {
            self.enqueue_grouped_message(request.session_id, message)
                .await?;
        }
        Ok(())
    }
}

fn spawn_receive_executor_refresh(policy: Weak<dyn RealtimeReceiveExecutorPolicy>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        loop {
            let Some(policy) = policy.upgrade() else {
                break;
            };
            drop(tokio::time::timeout(RECEIVE_EXECUTOR_REFRESH_INTERVAL, policy.refresh()).await);
            drop(policy);
            tokio::time::sleep(RECEIVE_EXECUTOR_REFRESH_INTERVAL).await;
        }
    });
}

#[allow(clippy::too_many_lines)]
fn cleanup_runtime_session(
    inner: &RealtimeInner,
    stream_backend: &Arc<dyn RuntimeStreamBackend>,
    cluster: Option<&Arc<dyn RuntimeRealtimeCluster>>,
    session_id: Uuid,
) {
    let cluster_rooms = rooms(inner).local_rooms_for_session(session_id);
    let disconnected_entry = {
        let mut entries = sessions(inner);
        let entry = entries.remove(&session_id);
        if let Some(app_id) = entry.as_ref().and_then(|entry| entry.app_id) {
            subscription_index(inner).remove(app_id, session_id);
        }
        entry
    };
    if let Some(entry) = disconnected_entry.as_ref() {
        schedule_active_handler_cleanup(
            &entry.active_handlers,
            Arc::clone(&entry.transact_worker),
            Arc::clone(&entry.refresh_worker),
        );
    }
    cleanup_app_query_cache(inner, disconnected_entry.as_ref());
    sync_state(inner)
        .active
        .retain(|(owner_session_id, _), _| *owner_session_id != session_id);
    let (writer_streams, disconnected_readers) = {
        let mut state = stream_state(inner);
        state
            .readers
            .retain(|(reader_session_id, _), _| *reader_session_id != session_id);
        let stream_ids = state
            .writers
            .iter()
            .filter(|(_, writer_session)| **writer_session == session_id)
            .map(|(stream_id, _)| *stream_id)
            .collect::<Vec<_>>();
        state
            .writers
            .retain(|_, writer_session| *writer_session != session_id);
        let readers = state.readers.values().cloned().collect::<Vec<_>>();
        let disconnected_readers = stream_ids
            .iter()
            .copied()
            .flat_map(|stream_id| {
                readers
                    .iter()
                    .filter(move |reader| reader.stream == stream_id)
                    .cloned()
            })
            .collect::<Vec<_>>();
        (stream_ids, disconnected_readers)
    };
    if let Some(entry) = disconnected_entry
        && let Some(app_id) = entry.app_id
    {
        for stream_id in writer_streams {
            schedule_stream_disconnect(
                Arc::clone(stream_backend),
                cluster.map(Arc::clone),
                RuntimeStreamDisconnectRequest {
                    context: entry_context(&SessionSnapshot::from_entry(&entry), app_id),
                    stream_id,
                    machine_id: inner.machine_id,
                },
            );
        }
    }
    for reader in disconnected_readers {
        if let Some(sender) = sessions(inner)
            .get(&reader.session)
            .map(|entry| entry.sender.clone())
        {
            send(
                &sender,
                json!({
                    "op": "stream-append",
                    "client-event-id": reader.event,
                    "stream-id": reader.stream,
                    "error": true,
                    "retry": true,
                    "error-type": "writer-disconnected",
                }),
            );
        }
    }
    for update in rooms(inner).leave_all(session_id) {
        deliver_presence_update(inner, update);
    }
    if let Some(cluster) = cluster {
        inner
            .cleanup
            .schedule(Arc::clone(cluster), session_id, cluster_rooms);
    }
}

fn schedule_active_handler_cleanup(
    handlers: &ActiveStatementHandlers,
    transaction_worker: Arc<TransactionWorkerControl>,
    refresh_worker: Arc<TransactionWorkerControl>,
) {
    let active = handlers.snapshot();
    if active.is_empty() {
        transaction_worker.abort();
        refresh_worker.abort();
        return;
    }
    for handler in &active {
        let _disconnect_was_delivered = handler.disconnect.send(true);
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        transaction_worker.abort();
        refresh_worker.abort();
        return;
    };
    runtime.spawn(async move {
        let mut cancellations = tokio::task::JoinSet::new();
        for handler in active {
            cancellations
                .spawn(async move { handler.tracker.cancel_and_wait(handler.cancel_wait).await });
        }
        while cancellations.join_next().await.is_some() {}
        transaction_worker.abort();
        refresh_worker.abort();
    });
}

fn cleanup_app_query_cache(inner: &RealtimeInner, disconnected: Option<&RuntimeSessionEntry>) {
    let Some(entry) = disconnected else {
        return;
    };
    let Some(app_id) = entry.app_id else {
        return;
    };
    let entries = sessions(inner);
    let has_remaining_sessions = entries.has_app(app_id);
    let root_frame_cleanup = if has_remaining_sessions {
        None
    } else {
        query_caches(inner).remove(&app_id);
        query_epochs(inner).remove(app_id);
        frame_epochs(inner).remove(app_id);
        catalog_digests(inner).remove(&app_id);
        inner
            .root_frame_caches
            .as_ref()
            .and_then(|caches| caches.remove(app_id))
    };
    drop(entries);
    drop(root_frame_cleanup);
}

fn schedule_stream_disconnect(
    backend: Arc<dyn RuntimeStreamBackend>,
    cluster: Option<Arc<dyn RuntimeRealtimeCluster>>,
    request: RuntimeStreamDisconnectRequest,
) {
    let app_id = request.context.app_id;
    let stream_id = request.stream_id;
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            if backend.disconnect_stream(request).await.is_ok()
                && let Some(cluster) = cluster
            {
                let _ = cluster.publish_stream_disconnect(app_id, stream_id).await;
            }
        });
    }
}

async fn deliver_cluster_event<A, D>(
    service: &RuntimeRealtimeService<A, D>,
    event: RuntimeRealtimeClusterEvent,
) where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeJoinRowsQueryBackend
        + RuntimeTreeQueryBackend
        + RuntimeSyncBackend
        + RuntimeStreamPermissionBackend
        + RuntimeRealtimeCatalogBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend,
{
    let inner = &service.inner;
    match event {
        RuntimeRealtimeClusterEvent::Presence {
            revision,
            room,
            peers,
            ..
        } => {
            deliver_remote_presence(inner, &room, revision, peers);
        }
        RuntimeRealtimeClusterEvent::Broadcast {
            source_machine_id,
            room,
            payload,
        } => {
            let local_runtime_echo = source_machine_id == inner.machine_id
                && consume_runtime_broadcast(inner, payload.data.peer_id);
            if !local_runtime_echo {
                deliver_remote_broadcast(inner, &room, &payload);
            }
        }
        RuntimeRealtimeClusterEvent::StreamAppend {
            source_machine_id,
            app_id,
            append,
        } => {
            if source_machine_id != inner.machine_id {
                deliver_stream_append(inner, app_id, &append);
            }
        }
        RuntimeRealtimeClusterEvent::StreamWriterDisconnected {
            source_machine_id,
            app_id,
            stream_id,
        } => {
            if source_machine_id != inner.machine_id {
                deliver_stream_disconnect(inner, app_id, stream_id);
            }
        }
        RuntimeRealtimeClusterEvent::SsePush {
            target_machine_id,
            app_id,
            session_id,
            messages,
        } => {
            if target_machine_id != inner.machine_id
                || sessions(inner)
                    .get(&session_id)
                    .and_then(|entry| entry.app_id)
                    != Some(app_id)
            {
                return;
            }
            for message in messages {
                let _ = service.enqueue_grouped_message(session_id, message).await;
            }
        }
        RuntimeRealtimeClusterEvent::Wal { record, .. } => {
            let has_sessions = sessions(inner).has_app(record.app_id);
            if has_sessions {
                while service.apply_wal_record(&record).await.is_err() {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            } else {
                service
                    .publish_sync_transaction(
                        record.app_id,
                        record.tx_id,
                        sync_changes_from_wal(&record),
                    )
                    .await;
            }
        }
        RuntimeRealtimeClusterEvent::WalBatch { records } => {
            while service.apply_wal_records(&records).await.is_err() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        RuntimeRealtimeClusterEvent::ByopWal {
            app_id,
            record,
            topics,
            ..
        } => {
            while sessions(inner).has_app(app_id)
                && service
                    .apply_byop_record(app_id, &record, &topics)
                    .await
                    .is_err()
            {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
}

async fn deliver_cluster_delivery<A, D>(
    service: &RuntimeRealtimeService<A, D>,
    delivery: RuntimeRealtimeClusterDelivery,
) where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeJoinRowsQueryBackend
        + RuntimeTreeQueryBackend
        + RuntimeSyncBackend
        + RuntimeStreamPermissionBackend
        + RuntimeRealtimeCatalogBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend,
{
    if !service
        .inner
        .cluster_deliveries
        .should_apply(delivery.event_id)
    {
        return;
    }
    deliver_cluster_event(service, delivery.event).await;
    service
        .inner
        .cluster_deliveries
        .mark_applied(delivery.event_id);
}

fn deliver_remote_presence(
    inner: &RealtimeInner,
    room: &RoomKey,
    revision: i64,
    peers: BTreeMap<Uuid, PeerPresence>,
) {
    let update = rooms(inner).apply_versioned_remote_snapshot(room, revision, peers);
    deliver_presence_update(inner, update);
}

fn deliver_presence_update(inner: &RealtimeInner, update: PresenceUpdate) {
    for delivery in update.deliveries {
        let recipient = sessions(inner)
            .get(&delivery.session_id)
            .map(|entry| (entry.sender.clone(), entry.client_features.clone()));
        let Some((sender, features)) = recipient else {
            continue;
        };
        sender.send_presence(&features, delivery);
    }
}

fn deliver_remote_broadcast(inner: &RealtimeInner, room: &RoomKey, payload: &BroadcastPayload) {
    for recipient in rooms(inner).local_session_ids(room) {
        if recipient == payload.data.peer_id {
            continue;
        }
        if let Some(sender) = sessions(inner)
            .get(&recipient)
            .map(|entry| entry.sender.clone())
        {
            send(
                &sender,
                json!({
                    "op": "server-broadcast",
                    "room-id": payload.room_id,
                    "topic": payload.topic,
                    "data": payload.data,
                }),
            );
        }
    }
}

fn deliver_stream_append(
    inner: &RealtimeInner,
    app_id: Uuid,
    appended: &RuntimeStreamAppendResult,
) {
    let readers = stream_state(inner)
        .readers
        .values()
        .filter(|reader| reader.stream == appended.stream_id)
        .cloned()
        .collect::<Vec<_>>();
    for reader in readers {
        let Some((session_app_id, sender)) = sessions(inner)
            .get(&reader.session)
            .map(|entry| (entry.app_id, entry.sender.clone()))
        else {
            continue;
        };
        if session_app_id != Some(app_id) {
            continue;
        }
        if !appended.content.is_empty() {
            send(
                &sender,
                json!({
                    "op": "stream-append",
                    "client-event-id": reader.event,
                    "stream-id": appended.stream_id,
                    "client-id": appended.client_id,
                    "offset": appended.offset,
                    "content": appended.content,
                }),
            );
        }
        if appended.done {
            send(
                &sender,
                json!({
                    "op": "stream-append",
                    "client-event-id": reader.event,
                    "stream-id": appended.stream_id,
                    "client-id": appended.client_id,
                    "done": true,
                    "abort-reason": appended.abort_reason,
                }),
            );
        }
    }
}

fn deliver_stream_disconnect(inner: &RealtimeInner, app_id: Uuid, stream_id: Uuid) {
    let readers = stream_state(inner)
        .readers
        .values()
        .filter(|reader| reader.stream == stream_id)
        .cloned()
        .collect::<Vec<_>>();
    for reader in readers {
        let Some((session_app_id, sender)) = sessions(inner)
            .get(&reader.session)
            .map(|entry| (entry.app_id, entry.sender.clone()))
        else {
            continue;
        };
        if session_app_id == Some(app_id) {
            send(
                &sender,
                json!({
                    "op": "stream-append",
                    "client-event-id": reader.event,
                    "stream-id": stream_id,
                    "error": true,
                    "retry": true,
                    "error-type": "writer-disconnected",
                }),
            );
        }
    }
}

fn deliver_stream_rate_limit(inner: &RealtimeInner) {
    let readers = {
        let mut state = stream_state(inner);
        std::mem::take(&mut state.readers)
            .into_values()
            .collect::<Vec<_>>()
    };
    for reader in readers {
        if let Some(sender) = sessions(inner)
            .get(&reader.session)
            .map(|entry| entry.sender.clone())
        {
            send(
                &sender,
                json!({
                    "op": "stream-append",
                    "client-event-id": reader.event,
                    "stream-id": reader.stream,
                    "error": true,
                    "retry": true,
                    "error-type": "rate-limit",
                }),
            );
        }
    }
}

fn initialized_entry(inner: &RealtimeInner, session_id: Uuid) -> Result<SessionSnapshot, ApiError> {
    with_initialized_entry(inner, session_id, SessionSnapshot::from_entry)
}

fn with_initialized_entry<T>(
    inner: &RealtimeInner,
    session_id: Uuid,
    snapshot: impl FnOnce(&RuntimeSessionEntry) -> T,
) -> Result<T, ApiError> {
    let entries = sessions(inner);
    let entry = entries.get(&session_id).ok_or_else(session_missing)?;
    if entry.app_id.is_none() || entry.versions.is_none() {
        Err(protocol_error("`init` has not run for this session."))
    } else {
        Ok(snapshot(entry))
    }
}

fn clojure_truthy(value: &Value) -> bool {
    !value.is_null() && value != &Value::Bool(false)
}

fn optional_truthy_value<'a>(message: &'a Value, key: &str) -> Option<&'a Value> {
    message.get(key).filter(|value| clojure_truthy(value))
}

fn missing_param(key: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: [\"{key}\"]"),
    )
    .with_hint(json!({"in": [key]}))
}

fn malformed_param(key: &str, input: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: [\"{key}\"]"),
    )
    .with_hint(json!({"in": [key], "original-input": input}))
}

fn required_param<'a>(message: &'a Value, key: &str) -> Result<&'a Value, ApiError> {
    optional_truthy_value(message, key).ok_or_else(|| missing_param(key))
}

fn coerce_uuid(value: &Value) -> Option<Uuid> {
    let value = java_string_trim(value.as_str()?);
    (value.len() == 36)
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}

fn required_uuid(message: &Value, key: &str) -> Result<Uuid, ApiError> {
    let value = required_param(message, key)?;
    coerce_uuid(value).ok_or_else(|| malformed_param(key, value))
}

fn optional_uuid(message: &Value, key: &str) -> Result<Option<Uuid>, ApiError> {
    optional_truthy_value(message, key)
        .map(|value| coerce_uuid(value).ok_or_else(|| malformed_param(key, value)))
        .transpose()
}

fn optional_init_uuid(message: &Value, key: &str) -> Result<Option<Uuid>, ApiError> {
    optional_truthy_value(message, key)
        .map(|value| coerce_uuid(value).ok_or_else(|| protocol_error(&format!("Invalid {key}"))))
        .transpose()
}

fn optional_init_string(message: &Value, key: &str) -> Result<Option<String>, ApiError> {
    optional_truthy_value(message, key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| protocol_error(&format!("Invalid {key}")))
        })
        .transpose()
}

fn coerce_non_blank_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => {
            (!clojure_string_blank(value)).then(|| java_string_trim(value).to_owned())
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn required_non_blank_string(message: &Value, key: &str) -> Result<String, ApiError> {
    let value = required_param(message, key)?;
    coerce_non_blank_string(value).ok_or_else(|| malformed_param(key, value))
}

fn optional_non_blank_string(message: &Value, key: &str) -> Result<Option<String>, ApiError> {
    optional_truthy_value(message, key)
        .map(|value| coerce_non_blank_string(value).ok_or_else(|| malformed_param(key, value)))
        .transpose()
}

fn required_string<'a>(message: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    let value = required_param(message, key)?;
    value.as_str().ok_or_else(|| malformed_param(key, value))
}

fn required_non_negative_u64(message: &Value, key: &str) -> Result<u64, ApiError> {
    let value = required_param(message, key)?;
    value.as_u64().ok_or_else(|| malformed_param(key, value))
}

fn optional_non_negative_u64(message: &Value, key: &str) -> Result<Option<u64>, ApiError> {
    optional_truthy_value(message, key)
        .map(|value| value.as_u64().ok_or_else(|| malformed_param(key, value)))
        .transpose()
}

#[allow(clippy::cast_possible_truncation)] // Matches Java Number.longValue for JSON floats.
fn required_long(message: &Value, key: &str) -> Result<i64, ApiError> {
    let value = required_param(message, key)?;
    // Clojure's `long` delegates to Number.longValue: signed integers are
    // unchanged, wider positive integers wrap their low 64 bits, and floating
    // values truncate with Java's saturating float-to-long conversion.
    let coerced = value
        .as_i64()
        .or_else(|| value.as_u64().map(u64::cast_signed))
        .or_else(|| value.as_f64().map(|number| number as i64));
    coerced.ok_or_else(|| malformed_param(key, value))
}

fn optional_bool(message: &Value, key: &str) -> Result<Option<bool>, ApiError> {
    optional_truthy_value(message, key)
        .map(|value| value.as_bool().ok_or_else(|| malformed_param(key, value)))
        .transpose()
}

fn required_string_array(message: &Value, key: &str) -> Result<Vec<String>, ApiError> {
    let value = required_param(message, key)?;
    let values = value
        .as_array()
        .ok_or_else(|| malformed_param(key, value))?;
    values
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| malformed_param(key, value))
}

fn coerce_abort_reason(value: &Value, key: &str) -> Result<String, ApiError> {
    if let Some(string) = value.as_str() {
        if clojure_string_blank(string) {
            return Err(malformed_param(key, value));
        }
        Ok(java_string_trim(string).to_owned())
    } else {
        Err(ApiError::new(ApiErrorType::Unknown, "Something went wrong"))
    }
}

fn java_string_trim(value: &str) -> &str {
    value.trim_matches(|character| character <= '\u{20}')
}

fn clojure_string_blank(value: &str) -> bool {
    value.chars().all(java_character_is_whitespace)
}

fn java_character_is_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000D}'
            | '\u{001C}'..='\u{0020}'
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200A}'
            | '\u{2028}'..='\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

fn entry_context(entry: &SessionSnapshot, app_id: Uuid) -> AdminRequestContext {
    AdminRequestContext {
        app_id,
        bearer_token: entry.bearer_token.clone(),
        actor: entry.actor.clone(),
        client_ip: entry.client_ip,
    }
}

fn stream_permission_data(record: &RuntimeStreamRecord) -> Value {
    json!({
        "id": record.stream_id,
        "clientId": record.client_id,
        "machineId": record.machine_id,
        "size": record.size,
        "done": record.done,
        "abortReason": record.abort_reason,
    })
}

pub(crate) fn validate_sync_query(query: &Map<String, Value>) -> Result<(), ApiError> {
    if query.is_empty() {
        return Err(protocol_error("Query is empty."));
    }
    if query.values().any(|forms| !forms.is_object()) {
        return Err(protocol_error("Invalid sync query"));
    }
    Ok(())
}

fn sync_change_matches(change: &Value, attribute_ids: &[Uuid]) -> bool {
    change
        .get("triple")
        .and_then(Value::as_array)
        .and_then(|triple| triple.get(1))
        .and_then(Value::as_str)
        .and_then(|attribute_id| Uuid::parse_str(attribute_id).ok())
        .is_some_and(|attribute_id| attribute_ids.contains(&attribute_id))
}

fn wal_schema_refresh(record: &WalRecord) -> bool {
    record.attr_changes.iter().any(|change| {
        matches!(
            change.action,
            WalAction::Insert | WalAction::Delete | WalAction::Truncate
        )
    }) || !record.ident_changes.is_empty()
        || record
            .triple_changes
            .iter()
            .any(|change| change.action == WalAction::Truncate)
}

pub(crate) fn sync_changes_from_wal(record: &WalRecord) -> Vec<Value> {
    let mut changes = Vec::new();
    for change in &record.triple_changes {
        match change.action {
            WalAction::Insert => changes.push(json!({
                "action": "added",
                "triple": wal_columns_to_triple(&change.columns),
            })),
            WalAction::Delete => changes.push(json!({
                "action": "removed",
                "triple": wal_columns_to_triple(&change.identity),
            })),
            WalAction::Update => {
                changes.push(json!({
                    "action": "removed",
                    "triple": wal_columns_to_triple(&change.identity),
                }));
                changes.push(json!({
                    "action": "added",
                    "triple": wal_columns_to_triple(&change.columns),
                }));
            }
            WalAction::Begin
            | WalAction::Truncate
            | WalAction::Message
            | WalAction::Close
            | WalAction::Unknown(_) => {}
        }
    }
    changes
}

fn wal_columns_to_triple(columns: &[WalColumn]) -> Value {
    let mut triple = vec![Value::Null; 4];
    for column in columns {
        match column.name.as_str() {
            "entity_id" => triple[0] = column.value.clone(),
            "attr_id" => triple[1] = column.value.clone(),
            "value" => {
                triple[2] = column
                    .value
                    .as_str()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| column.value.clone());
            }
            "created_at" => triple[3] = column.value.clone(),
            _ => {}
        }
    }
    Value::Array(triple)
}

fn with_client_id(mut event: Value, client_event_id: Option<Value>) -> Value {
    if let Some(client_event_id) = client_event_id {
        event["client-event-id"] = client_event_id;
    }
    event
}

fn client_event_id_from(message: &Value) -> Option<Value> {
    message.get("client-event-id").cloned()
}

fn canonical_query(query: &Map<String, Value>) -> String {
    serde_json::to_string(query).expect("JSON maps are serializable")
}

fn supports_skip_attrs(versions: Option<&Value>) -> bool {
    client_features(versions).supports(ClientFeature::SkipAttrs)
}

pub(crate) fn client_versions(versions: Option<&Value>) -> BTreeMap<String, String> {
    versions
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, version)| {
            version
                .as_str()
                .map(|version| (name.clone(), version.to_owned()))
        })
        .collect()
}

fn client_features(versions: Option<&Value>) -> ClientFeatures {
    ClientFeatures::from_versions(&client_versions(versions))
}

/// Caps the warning memory and log volume from client-supplied versions.
const WARNED_SDK_VERSION_LIMIT: usize = 32;

static WARNED_UNTESTED_SDK_VERSIONS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

/// Logs one warning per distinct SDK version newer than the tested release.
pub(crate) fn note_sdk_versions(versions: Option<&Value>) {
    let versions = client_versions(versions);
    let warned = WARNED_UNTESTED_SDK_VERSIONS.get_or_init(|| Mutex::new(BTreeSet::new()));
    let mut warned = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (major, minor, patch) = instant_realtime::MAX_TESTED_SDK_VERSION;
    let max_tested = format!("{major}.{minor}.{patch}");
    for version in untested_sdk_versions(&versions, &mut warned) {
        tracing::warn!(
            sdk_version = %version,
            max_tested_sdk_version = %max_tested,
            "SDK version is newer than the newest tested release"
        );
    }
}

fn untested_sdk_versions(
    versions: &BTreeMap<String, String>,
    warned: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut new = Vec::new();
    for version in versions.values() {
        let Some(version) = instant_realtime::normalize_sdk_version(version) else {
            continue;
        };
        if !instant_realtime::sdk_version_is_newer(&version) || warned.contains(&version) {
            continue;
        }
        if warned.len() >= WARNED_SDK_VERSION_LIMIT {
            continue;
        }
        warned.insert(version.clone());
        new.push(version);
    }
    new
}

fn realtime_catalog_attrs(catalog: &instant_instaql::AttributeCatalog) -> Value {
    Value::Array(
        catalog
            .attributes
            .iter()
            .map(super::admin_data::framework_attr)
            .collect(),
    )
}

fn realtime_catalog_digest(attrs: &impl serde::Serialize) -> [u8; 32] {
    struct DigestWriter(Sha256);
    impl std::io::Write for DigestWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = DigestWriter(Sha256::new());
    serde_json::to_writer(&mut writer, attrs)
        .expect("realtime catalog attributes are JSON serializable");
    writer.0.finalize().into()
}

fn catalog_digests(
    inner: &RealtimeInner,
) -> MutexGuard<'_, HashMap<Uuid, (Arc<instant_instaql::AttributeCatalog>, [u8; 32])>> {
    inner
        .catalog_digests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// Every session refresh compares its fresh catalog with the one acknowledged by
// the client. Encoding and hashing the full attribute list per refresh is pure
// repetition while metadata is unchanged; reuse the digest of an equal catalog.
fn refresh_catalog_digest(
    inner: &RealtimeInner,
    app_id: Uuid,
    catalog: &Arc<instant_instaql::AttributeCatalog>,
) -> [u8; 32] {
    if let Some((digested, digest)) = catalog_digests(inner).get(&app_id)
        && (Arc::ptr_eq(digested, catalog) || digested.as_ref() == catalog.as_ref())
    {
        return *digest;
    }
    let digest = realtime_catalog_digest(&crate::attribute_wire::FrameworkCatalog(catalog));
    catalog_digests(inner).insert(app_id, (Arc::clone(catalog), digest));
    digest
}

fn send(sender: &RuntimeSessionSender, value: Value) {
    let _ = sender.send(value);
}

async fn send_sync_frame(sender: &RuntimeSessionSender, value: Value) -> Result<(), ApiError> {
    let (delivered, delivery) = oneshot::channel();
    sender.send_confirmed(value, delivered).map_err(|()| {
        ApiError::new(
            ApiErrorType::ConnectionClosed,
            "Realtime connection closed during sync delivery.",
        )
    })?;
    match delivery.await {
        Ok(OutboundDelivery::Delivered) => Ok(()),
        Ok(OutboundDelivery::ConnectionClosed) | Err(_) => Err(ApiError::new(
            ApiErrorType::ConnectionClosed,
            "Realtime connection closed during sync delivery.",
        )),
        Ok(OutboundDelivery::TimedOut) => Err(ApiError::new(
            ApiErrorType::OperationTimedOut,
            "Timed out delivering sync data to the realtime connection.",
        )),
    }
}

async fn run_outbound_pump(
    mut outbound: mpsc::UnboundedReceiver<OutboundFrame>,
    transport: mpsc::Sender<Value>,
    state: Arc<OutboundQueueState>,
    presence: Arc<Mutex<OutboundPresenceState>>,
    disconnect: Arc<dyn Fn() + Send + Sync>,
) {
    while let Some(frame) = outbound.recv().await {
        match frame {
            OutboundFrame::Message { value, delivered } => {
                let delivery = deliver_outbound_value(&transport, value).await;
                state.queued.fetch_sub(1, Ordering::AcqRel);
                if let Some(delivered) = delivered {
                    let _ = delivered.send(delivery);
                }
                if delivery != OutboundDelivery::Delivered {
                    if !state.closed.swap(true, Ordering::AcqRel) {
                        disconnect();
                    }
                    return;
                }
            }
            OutboundFrame::PresenceWake { room_id } => {
                let message = {
                    let mut pending = presence.lock().unwrap();
                    pending
                        .pending
                        .remove(&room_id)
                        .map(|message| match message {
                            PendingPresence::Patch { edits, .. } => {
                                PresenceMessage::PatchPresence { room_id, edits }
                            }
                            PendingPresence::Full { data, .. } => {
                                PresenceMessage::RefreshPresence { room_id, data }
                            }
                        })
                };
                if let Some(message) = message
                    && deliver_outbound_value(
                        &transport,
                        serde_json::to_value(message)
                            .expect("presence messages are JSON serializable"),
                    )
                    .await
                        != OutboundDelivery::Delivered
                {
                    state.queued.fetch_sub(1, Ordering::AcqRel);
                    if !state.closed.swap(true, Ordering::AcqRel) {
                        disconnect();
                    }
                    return;
                }
                state.queued.fetch_sub(1, Ordering::AcqRel);
            }
        }
        if state.closed.load(Ordering::Acquire) {
            break;
        }
    }
}

async fn deliver_outbound_value(transport: &mpsc::Sender<Value>, value: Value) -> OutboundDelivery {
    match tokio::time::timeout(SYNC_FRAME_SEND_TIMEOUT, transport.send(value)).await {
        Ok(Ok(())) => OutboundDelivery::Delivered,
        Ok(Err(_)) => OutboundDelivery::ConnectionClosed,
        Err(_) => OutboundDelivery::TimedOut,
    }
}

fn protocol_error(message: &str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn room_error(error: impl std::fmt::Display) -> ApiError {
    protocol_error(&error.to_string())
}

fn cluster_error(error: impl std::fmt::Display) -> ApiError {
    database_error(error)
}

fn session_missing() -> ApiError {
    ApiError::new(ApiErrorType::SessionMissing, "Session is missing")
}

fn app_session_capacity_error(limit: usize) -> ApiError {
    ApiError::new(
        ApiErrorType::RateLimited,
        format!(
            "App has reached the limit of {limit} concurrent sessions. Close another session and retry."
        ),
    )
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn sessions(inner: &RealtimeInner) -> MutexGuard<'_, SessionRegistry> {
    inner
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// Always acquire the session registry before this index when both are needed.
// Registration, replacement, removal and WAL selection share that lock order.
fn subscription_index(inner: &RealtimeInner) -> MutexGuard<'_, SessionTopicIndex> {
    inner
        .subscription_index
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn index_session_queries(inner: &RealtimeInner, session_id: Uuid, entry: &RuntimeSessionEntry) {
    if let Some(app_id) = entry.app_id {
        subscription_index(inner).replace(
            app_id,
            session_id,
            entry
                .queries
                .values()
                .map(|query| query.instant_topics.as_deref()),
        );
    }
}

fn select_wal_refresh_sessions(
    inner: &RealtimeInner,
    record: &WalRecord,
    tx_id: u64,
    refresh_sessions: Option<&BTreeSet<Uuid>>,
    schema_refresh: bool,
    routing_topics: Option<&Arc<[instant_wal::Topic]>>,
    entity_images: Option<&(EntitySnapshot, EntitySnapshot)>,
) -> (Vec<Uuid>, InvalidationFilterSummary, usize) {
    let app_id = record.app_id;
    let invalidation_topics = routing_topics.map(|topics| &topics[..]);
    // Invalidate reuse before exposing this watermark to new queries. Queries
    // still executing were not candidates here; keep the exact topics this
    // selection used so their registration can apply the same test.
    let routing = (refresh_sessions.is_none() && !schema_refresh)
        .then(|| routing_topics.cloned())
        .flatten();
    // Frames must be retired before selection marks sessions under the session
    // lock: a selected refresh is dispatched only after that, and must not read
    // a frame this WAL affects. Frame correctness is fenced by the frame epoch,
    // not by the session lock.
    let root_frame_cleanup =
        invalidate_app_root_frames(inner, app_id, schema_refresh, routing.clone());
    let cache_cleanup;
    let mut sessions = sessions(inner);
    cache_cleanup = invalidate_app_query_reuse(inner, app_id, schema_refresh, routing);
    advance_processed_watermark(inner, app_id, tx_id, Some(record.isn));
    let mut refresh_session_ids = Vec::new();
    let mut summary = InvalidationFilterSummary::default();
    let candidates = wal_candidate_sessions(
        inner,
        &sessions,
        app_id,
        refresh_sessions,
        invalidation_topics,
    );
    let candidate_sessions = candidates.len();
    for session_id in candidates {
        let Some(entry) = sessions.get_mut(&session_id) else {
            continue;
        };
        if entry.app_id != Some(app_id) {
            continue;
        }
        if entry.queries.is_empty() && !schema_refresh
            || refresh_sessions.is_some_and(|sessions| !sessions.contains(&session_id))
        {
            continue;
        }
        entry.next_refresh_generation = entry.next_refresh_generation.wrapping_add(1);
        let generation = entry.next_refresh_generation;
        let mut selected = schema_refresh;
        for query in entry.queries.values_mut() {
            let should_refresh = if schema_refresh || refresh_sessions.is_some() {
                true
            } else {
                let outcome = query_wal_match(query, invalidation_topics, entity_images);
                summary.observe(outcome);
                outcome.should_refresh()
            };
            if should_refresh {
                query.stale_generation = generation;
                selected = true;
            }
        }
        if selected {
            if schema_refresh {
                entry.refresh_attrs_generation = generation;
                entry.refresh_force_generation = generation;
            }
            refresh_session_ids.push(session_id);
        }
    }
    drop(sessions);
    drop(cache_cleanup);
    drop(root_frame_cleanup);
    (refresh_session_ids, summary, candidate_sessions)
}

fn wal_candidate_sessions(
    inner: &RealtimeInner,
    entries: &SessionRegistry,
    app_id: Uuid,
    selected: Option<&BTreeSet<Uuid>>,
    topics: Option<&[instant_wal::Topic]>,
) -> BTreeSet<Uuid> {
    if let Some(selected) = selected {
        return selected.clone();
    }
    if let Some(topics) = topics {
        return subscription_index(inner).candidates(app_id, topics);
    }
    entries.ids_for_app(app_id).collect()
}

impl<A, D> RuntimeSessionReporter for RuntimeRealtimeService<A, D>
where
    A: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn machine_id(&self) -> Uuid {
        self.inner.machine_id
    }

    fn local_session_report(&self) -> HashMap<Uuid, RuntimeLocalAppSessionReport> {
        let mut reports = HashMap::new();
        for entry in sessions(&self.inner)
            .values()
            .filter(|entry| entry.versions.is_some())
        {
            let Some(app_id) = entry.app_id else {
                continue;
            };
            let report = reports
                .entry(app_id)
                .or_insert_with(|| RuntimeLocalAppSessionReport {
                    count: 0,
                    origins: HashMap::new(),
                });
            report.count += 1;
            if let Some(origin) = &entry.origin {
                *report.origins.entry(origin.clone()).or_insert(0) += 1;
            }
        }
        reports
    }

    fn local_session_count(&self) -> usize {
        sessions(&self.inner)
            .values()
            .filter(|entry| entry.versions.is_some())
            .count()
    }

    fn local_query_count(&self) -> usize {
        sessions(&self.inner)
            .values()
            .filter(|entry| entry.versions.is_some())
            .map(|entry| entry.queries.len())
            .sum()
    }
}

fn rooms(inner: &RealtimeInner) -> MutexGuard<'_, RoomStore> {
    inner
        .rooms
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn processed_watermark(inner: &RealtimeInner, app_id: Uuid) -> ProcessedWatermark {
    let mut processed = inner
        .processed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *processed
        .entry(app_id)
        .or_insert_with(|| inner.watermark_seed.snapshot())
}

fn advance_processed_watermark(inner: &RealtimeInner, app_id: Uuid, tx_id: u64, isn: Option<Isn>) {
    inner
        .processed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(app_id)
        .or_insert_with(|| inner.watermark_seed.snapshot())
        .advance(tx_id, isn);
}

fn sync_state(inner: &RealtimeInner) -> MutexGuard<'_, SyncState> {
    inner
        .sync
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record_runtime_broadcast(inner: &RealtimeInner, session_id: Uuid) {
    let mut broadcasts = inner
        .local_broadcasts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *broadcasts.entry(session_id).or_default() += 1;
}

fn consume_runtime_broadcast(inner: &RealtimeInner, session_id: Uuid) -> bool {
    let mut broadcasts = inner
        .local_broadcasts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(count) = broadcasts.get_mut(&session_id) else {
        return false;
    };
    *count -= 1;
    if *count == 0 {
        broadcasts.remove(&session_id);
    }
    true
}

fn record_sync_transaction(
    state: &mut SyncState,
    history: &mut SyncHistoryState,
    transaction: SyncTransaction,
    history_limit: usize,
) -> BTreeSet<Uuid> {
    record_sync_history(history, transaction, history_limit);
    let stale_keys = state
        .active
        .iter()
        .filter(|(_, subscription)| {
            history
                .evicted_through
                .get(&subscription.app)
                .is_some_and(|tx_id| subscription.sent_tx < *tx_id)
        })
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    let mut stale_sessions = BTreeSet::new();
    for key in stale_keys {
        if let Some(subscription) = state.active.remove(&key) {
            state
                .stale_subscriptions
                .insert(subscription.subscription, subscription.app);
            stale_sessions.insert(subscription.session);
        }
    }
    stale_sessions
}

fn record_sync_history(
    history: &mut SyncHistoryState,
    transaction: SyncTransaction,
    history_limit: usize,
) {
    history.record(transaction, history_limit);
}

fn stream_state(inner: &RealtimeInner) -> MutexGuard<'_, StreamState> {
    inner
        .streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU8;

    use instant_datalog::DataType;
    use instant_http::{
        AdminQueryPermissionsRequest, AdminQueryRequest, AdminTransactPermissionsRequest,
        RuntimeFrameworkQueryTopics, RuntimeFrameworkResponse,
    };
    use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};

    use super::*;

    fn assert_param_error(error: &ApiError, kind: ApiErrorType, message: &str, hint: Value) {
        assert_eq!(error.body().kind, kind);
        assert_eq!(error.body().message, message);
        assert_eq!(error.body().hint, Some(hint));
    }

    #[test]
    fn cluster_coordination_errors_hide_the_store_cause() {
        let error = cluster_error("connection to server at 10.0.0.5 failed");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Database error");
    }

    #[test]
    fn realtime_params_match_the_pinned_clojure_coercion_table() {
        // Oracle 4e0a1bd8:
        // util/exception.clj:410-438 (required/optional truthiness and envelopes),
        // util/uuid.clj:11-21 (trim, then exact 36-character UUID parsing), and
        // reactive/session.clj:638-641 (room ids accept every string, including "").
        let id = Uuid::new_v4();
        let padded = format!("  {id}\n");
        assert_eq!(required_uuid(&json!({"id": padded}), "id"), Ok(id));
        assert_eq!(optional_uuid(&json!({"id": false}), "id"), Ok(None));
        assert_eq!(required_string(&json!({"room-id": ""}), "room-id"), Ok(""));
        assert_eq!(
            required_non_blank_string(&json!({"client-id": 42}), "client-id"),
            Ok("42".to_owned())
        );
        assert_eq!(required_long(&json!({"tx-id": -7}), "tx-id"), Ok(-7));
        assert_eq!(required_long(&json!({"tx-id": 4.9}), "tx-id"), Ok(4));
        assert_eq!(required_long(&json!({"tx-id": -4.9}), "tx-id"), Ok(-4));
        assert_eq!(
            required_long(&json!({"tx-id": i64::MAX}), "tx-id"),
            Ok(i64::MAX)
        );
        assert_eq!(required_long(&json!({"tx-id": u64::MAX}), "tx-id"), Ok(-1));
        assert_eq!(
            required_long(&json!({"tx-id": 1e100}), "tx-id"),
            Ok(i64::MAX)
        );
        assert_eq!(
            required_long(&json!({"tx-id": -1e100}), "tx-id"),
            Ok(i64::MIN)
        );
        assert_eq!(
            coerce_abort_reason(&json!("  stopped  "), "abort-reason"),
            Ok("stopped".to_owned())
        );
        let error = coerce_abort_reason(&json!({"code": 7}), "abort-reason").unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(
            error.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );

        // Java String.trim removes only code points <= U+0020. Clojure's
        // string/blank? uses Character.isWhitespace, which deliberately does
        // not classify NBSP as whitespace.
        let nbsp = '\u{00a0}';
        assert!(coerce_uuid(&json!(format!("{nbsp}{id}{nbsp}"))).is_none());
        assert_eq!(
            coerce_non_blank_string(&json!(format!("{nbsp}x{nbsp}"))),
            Some(format!("{nbsp}x{nbsp}"))
        );
        assert_eq!(
            coerce_non_blank_string(&json!(nbsp.to_string())),
            Some(nbsp.to_string())
        );
        assert!(coerce_non_blank_string(&json!("\u{2003}")).is_none());

        for input in [json!({}), json!({"id": null}), json!({"id": false})] {
            let error = required_uuid(&input, "id").unwrap_err();
            assert_param_error(
                &error,
                ApiErrorType::ParamMissing,
                "Missing parameter: [\"id\"]",
                json!({"in": ["id"]}),
            );
        }

        for malformed in [json!(id.simple().to_string()), json!(true), json!(" ")] {
            let input = json!({"id": malformed.clone()});
            let error = required_uuid(&input, "id").unwrap_err();
            assert_param_error(
                &error,
                ApiErrorType::ParamMalformed,
                "Malformed parameter: [\"id\"]",
                json!({"in": ["id"], "original-input": malformed}),
            );
        }

        for malformed in [json!("7"), json!(true)] {
            let input = json!({"tx-id": malformed.clone()});
            let error = required_long(&input, "tx-id").unwrap_err();
            assert_param_error(
                &error,
                ApiErrorType::ParamMalformed,
                "Malformed parameter: [\"tx-id\"]",
                json!({"in": ["tx-id"], "original-input": malformed}),
            );
        }

        let blank = json!(" \t ");
        let error = coerce_abort_reason(&blank, "abort-reason").unwrap_err();
        assert_param_error(
            &error,
            ApiErrorType::ParamMalformed,
            "Malformed parameter: [\"abort-reason\"]",
            json!({"in": ["abort-reason"], "original-input": blank}),
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn realtime_group_keys_match_the_pinned_receive_queue_mapping() {
        let session_id = Uuid::new_v4();
        let part = |value: Value| serde_json::to_string(&value).unwrap();
        let cases = [
            (
                json!({"op": "transact"}),
                RealtimeGroupKey::Transact(session_id),
            ),
            (
                json!({"op": "join-room", "room-id": "lobby"}),
                RealtimeGroupKey::Room(session_id, part(json!("lobby"))),
            ),
            (
                json!({"op": "leave-room", "room-id": "lobby"}),
                RealtimeGroupKey::Room(session_id, part(json!("lobby"))),
            ),
            (
                json!({"op": "set-presence", "room-id": "lobby"}),
                RealtimeGroupKey::Room(session_id, part(json!("lobby"))),
            ),
            (
                json!({"op": "client-broadcast", "room-id": "lobby"}),
                RealtimeGroupKey::Room(session_id, part(json!("lobby"))),
            ),
            (
                json!({"op": "server-broadcast", "room-id": "lobby"}),
                RealtimeGroupKey::Room(session_id, part(json!("lobby"))),
            ),
            (
                json!({"op": "add-query", "q": {"users": {}}}),
                RealtimeGroupKey::Query(session_id, part(json!({"users": {}}))),
            ),
            (
                json!({"op": "remove-query", "q": {"users": {}}}),
                RealtimeGroupKey::Query(session_id, part(json!({"users": {}}))),
            ),
            (
                json!({"op": "refresh"}),
                RealtimeGroupKey::Refresh(session_id),
            ),
            (
                json!({"op": "refresh-presence", "room-id": "lobby"}),
                RealtimeGroupKey::RefreshPresence(session_id, part(json!("lobby"))),
            ),
            (
                json!({"op": "refresh-sync-table", "subscription-id": "sub"}),
                RealtimeGroupKey::RefreshSyncTable(part(json!("sub"))),
            ),
            (json!({"op": "error"}), RealtimeGroupKey::Error(session_id)),
            (
                json!({"op": "append-stream", "stream-id": "stream"}),
                RealtimeGroupKey::AppendStream(session_id, part(json!("stream"))),
            ),
            (json!({"op": "init"}), RealtimeGroupKey::Init(session_id)),
            (
                json!({"op": "sse-init"}),
                RealtimeGroupKey::SseInit(session_id),
            ),
            (
                json!({"op": "start-sync", "q": {"users": {}}}),
                RealtimeGroupKey::Sync(session_id, part(json!({"users": {}}))),
            ),
            (
                json!({"op": "remove-sync", "subscription-id": "sub"}),
                RealtimeGroupKey::ModifySync(part(json!("sub"))),
            ),
            (
                json!({"op": "resync-table", "subscription-id": "sub"}),
                RealtimeGroupKey::ModifySync(part(json!("sub"))),
            ),
            (
                json!({"op": "start-stream", "client-id": "client"}),
                RealtimeGroupKey::StartStream(session_id, part(json!("client"))),
            ),
            (
                json!({"op": "subscribe-stream", "stream-id": "stream", "client-id": "ignored"}),
                RealtimeGroupKey::SubscribeStream(session_id, part(json!("stream"))),
            ),
            (
                json!({"op": "subscribe-stream", "client-id": "client"}),
                RealtimeGroupKey::SubscribeStream(session_id, part(json!("client"))),
            ),
            (
                json!({"op": "subscribe-stream", "stream-id": null, "client-id": "client"}),
                RealtimeGroupKey::SubscribeStream(session_id, part(json!("client"))),
            ),
            (
                json!({"op": "unsubscribe-stream", "subscribe-event-id": "event"}),
                RealtimeGroupKey::UnsubscribeStream(session_id, part(json!("event"))),
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(
                realtime_group_key(session_id, &event, 7),
                expected,
                "{event}"
            );
        }
        assert_ne!(
            realtime_group_key(session_id, &json!({"op": "unknown"}), 7),
            realtime_group_key(session_id, &json!({"op": "unknown"}), 8),
        );
        let mut first_query = Map::new();
        first_query.insert("z".to_owned(), json!({"b": 2, "a": 1}));
        first_query.insert("a".to_owned(), json!([]));
        let mut second_query = Map::new();
        second_query.insert("a".to_owned(), json!([]));
        second_query.insert("z".to_owned(), json!({"a": 1, "b": 2}));
        assert_eq!(
            realtime_group_key(session_id, &json!({"op": "add-query", "q": first_query}), 1,),
            realtime_group_key(
                session_id,
                &json!({"op": "add-query", "q": second_query}),
                2,
            ),
            "object insertion order must not split a query group",
        );
    }

    #[test]
    fn grouped_receive_combiners_are_pair_exact() {
        let keep_newest = ["refresh", "set-presence", "refresh-sync-table"];
        for op in keep_newest {
            let first = json!({"op": op, "value": 1});
            let second = json!({"op": op, "value": 2});
            assert_eq!(
                combine_grouped_receive_events(&first, &second),
                Some(second)
            );
        }
        assert_eq!(
            combine_grouped_receive_events(
                &json!({"op": "refresh-presence", "edits": [1, 2]}),
                &json!({"op": "refresh-presence", "edits": [3], "newest": true}),
            ),
            Some(json!({
                "op": "refresh-presence",
                "edits": [1, 2, 3],
                "newest": true,
            }))
        );
        assert_eq!(
            combine_grouped_receive_events(
                &json!({"op": "append-stream", "offset": 10, "chunks": ["a"]}),
                &json!({"op": "append-stream", "offset": 11, "chunks": ["b", "c"]}),
            ),
            Some(json!({"op": "append-stream", "offset": 10, "chunks": ["a", "b", "c"]}))
        );
        for (first, second) in [
            (json!({"op": "refresh"}), json!({"op": "set-presence"})),
            (json!({"op": "transact"}), json!({"op": "transact"})),
            (json!({"op": "add-query"}), json!({"op": "add-query"})),
        ] {
            assert_eq!(combine_grouped_receive_events(&first, &second), None);
        }
    }

    #[test]
    fn sync_query_validation_allows_multiple_roots_links_and_where() {
        let query = serde_json::from_value(json!({
            "posts": {
                "$": {"where": {"title": "published"}, "order": {"title": "asc"}},
                "author": {}
            },
            "users": {"$": {"where": {"name": "Ada"}}}
        }))
        .unwrap();

        assert!(validate_sync_query(&query).is_ok());
    }

    #[test]
    fn grouped_server_broadcasts_preserve_payload_order_across_repeated_combines() {
        let first = json!({"op": "server-broadcast", "room-id": "r", "topic": "a", "data": 1});
        let second = json!({"op": "server-broadcast", "room-id": "r", "topic": "b", "data": 2});
        let third = json!({"op": "server-broadcast", "room-id": "r", "topic": "c", "data": 3});
        let combined = combine_grouped_receive_events(&first, &second).unwrap();
        assert_eq!(combined.get("topic"), None);
        assert_eq!(combined.get("data"), None);
        let combined = combine_grouped_receive_events(&combined, &third).unwrap();
        assert_eq!(
            combined[GROUPED_BROADCAST_PAYLOADS],
            json!([
                {"topic": "a", "data": 1},
                {"topic": "b", "data": 2},
                {"topic": "c", "data": 3},
            ])
        );
    }

    #[test]
    fn cluster_delivery_tracker_suppresses_apply_before_ack_replay() {
        let tracker = ClusterDeliveryTracker::default();
        assert!(tracker.should_apply(41));
        tracker.mark_applied(41);
        assert!(!tracker.should_apply(41));
        assert!(!tracker.should_apply(40));
        assert!(tracker.should_apply(42));
        tracker.mark_applied(42);
        assert!(!tracker.should_apply(42));
    }

    #[tokio::test]
    async fn transaction_isn_tracker_handles_delivery_before_wait_and_timeout_cleanup() {
        let tracker = TransactionIsnTracker::default();
        let isn = Isn::new(2, instant_wal::Lsn::from_u64(0x55));
        tracker.deliver(7, isn);
        assert_eq!(tracker.wait(7, Duration::ZERO).await, Some(isn));

        assert_eq!(tracker.wait(8, Duration::ZERO).await, None);
        let state = tracker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!state.waiters.contains_key(&8));
    }

    #[tokio::test]
    async fn cancelling_an_isn_wait_removes_its_registration() {
        let tracker = Arc::new(TransactionIsnTracker::default());
        let waiter = {
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move { tracker.wait(9, Duration::from_secs(60)).await })
        };
        loop {
            if tracker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .waiters
                .contains_key(&9)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        waiter.abort();
        let _ = waiter.await;
        assert!(
            !tracker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .waiters
                .contains_key(&9)
        );
    }

    #[test]
    fn refresh_throttling_uses_the_shared_strict_skip_attrs_feature_gate() {
        assert!(!supports_skip_attrs(Some(
            &json!({"@instantdb/core": "0.20.4"})
        )));
        assert!(supports_skip_attrs(Some(
            &json!({"@instantdb/core": "0.20.5"})
        )));
        assert!(!supports_skip_attrs(Some(
            &json!({"@instantdb/core": "not-semver"})
        )));
    }

    #[test]
    fn untested_sdk_versions_warn_once_per_normalized_version() {
        let (major, minor, patch) = instant_realtime::MAX_TESTED_SDK_VERSION;
        let max_tested = format!("{major}.{minor}.{patch}");
        let next_patch = format!("{major}.{minor}.{}", patch + 1);
        let second_patch = format!("{major}.{minor}.{}", patch + 2);
        let third_patch = format!("{major}.{minor}.{}", patch + 3);
        let mut warned = BTreeSet::new();
        let versions = client_versions(Some(
            &json!({"@instantdb/core": format!("v{next_patch}-alpha.1+build.7")}),
        ));
        assert_eq!(
            untested_sdk_versions(&versions, &mut warned),
            vec![next_patch.clone()]
        );
        assert!(untested_sdk_versions(&versions, &mut warned).is_empty());

        // The same version from another SDK, with different metadata, is the
        // same warning.
        let other = client_versions(Some(&json!({
            "@instantdb/react": format!("{next_patch}+build.9"),
            "@instantdb/core": max_tested.clone(),
        })));
        assert!(untested_sdk_versions(&other, &mut warned).is_empty());

        let older = client_versions(Some(
            &json!({"@instantdb/core": max_tested, "@instantdb/react": "not-semver"}),
        ));
        assert!(untested_sdk_versions(&older, &mut warned).is_empty());

        let newer = client_versions(Some(&json!({
            "@instantdb/core": second_patch.clone(),
            "@instantdb/react": third_patch.clone(),
        })));
        assert_eq!(
            untested_sdk_versions(&newer, &mut warned),
            vec![second_patch, third_patch]
        );
        assert!(untested_sdk_versions(&newer, &mut warned).is_empty());

        let limit = u64::try_from(WARNED_SDK_VERSION_LIMIT).expect("warning limit fits in u64");
        let mut full = (0..limit)
            .map(|offset| format!("{major}.{minor}.{}", patch + 1 + offset))
            .collect::<BTreeSet<_>>();
        let extra = client_versions(Some(&json!({
            "@instantdb/core": format!("{major}.{minor}.{}", patch + 1 + limit),
        })));
        assert!(untested_sdk_versions(&extra, &mut full).is_empty());
        assert_eq!(full.len(), WARNED_SDK_VERSION_LIMIT);
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = LogBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            LogBuffer(Arc::clone(&self.0))
        }
    }

    #[test]
    fn note_sdk_versions_warns_once_per_newer_version() {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let (major, minor, patch) = instant_realtime::MAX_TESTED_SDK_VERSION;
        let max_tested = format!("{major}.{minor}.{patch}");
        let normalized_newer = format!("{}.{minor}.{patch}", major + 1);
        let versions = json!({"@instantdb/core": format!("v{normalized_newer}-rc.1")});
        tracing::subscriber::with_default(subscriber, || {
            note_sdk_versions(Some(&versions));
            note_sdk_versions(Some(&versions));
        });
        let logs = String::from_utf8(
            logs.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .unwrap();
        assert_eq!(logs.matches(&normalized_newer).count(), 1, "{logs}");
        assert_eq!(
            logs.matches("SDK version is newer than the newest tested release")
                .count(),
            1,
            "{logs}"
        );
        assert!(
            logs.contains(&format!("max_tested_sdk_version={max_tested}")),
            "{logs}"
        );
    }

    #[derive(Default)]
    struct TestAuthorizer;

    struct TestAppRateLimits {
        limited_app: Uuid,
    }

    struct TestRefreshThrottle {
        app_id: Uuid,
        delay: Duration,
    }

    struct TestCoarseTopics(bool);

    struct TestRefreshReadControls {
        calls: Mutex<Vec<(Uuid, bool)>>,
        value: Mutex<crate::RefreshReadControls>,
        fail: AtomicBool,
    }

    impl TestRefreshReadControls {
        fn active() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
                value: Mutex::new(crate::RefreshReadControls {
                    status: AppStatusSnapshot {
                        own_status: AppStatus::Active,
                        effective_status: AppStatus::Active,
                    },
                    use_coarse_topics: true,
                }),
            }
        }
    }

    impl crate::RefreshReadControlsPolicy for TestRefreshReadControls {
        fn read(&self, app_id: Uuid, stale: bool) -> crate::RefreshReadControlsFuture<'_> {
            self.calls.lock().unwrap().push((app_id, stale));
            let value = if self.fail.load(Ordering::Acquire) {
                Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "combined control read failed",
                ))
            } else {
                Ok(*self.value.lock().unwrap())
            };
            Box::pin(std::future::ready(value))
        }
    }

    #[tokio::test]
    async fn failed_combined_refresh_read_keeps_topic_retry_and_cannot_fall_back_to_success() {
        let policy = Arc::new(TestRefreshReadControls::active());
        policy.fail.store(true, Ordering::Release);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_coarse_topics_policy(Arc::new(TestCoarseTopicsFailure {
            fail: AtomicBool::new(true),
        }))
        .with_refresh_read_controls_policy(policy.clone());
        let app_id = Uuid::new_v4();
        assert!(matches!(
            service.read_refresh_controls(app_id, true).await,
            Err(RefreshReadControlsFailure::Topics(_))
        ));
        let successful_standalone = service
            .with_coarse_topics_policy(Arc::new(TestCoarseTopics(false)))
            .with_refresh_read_controls_policy(policy.clone());
        assert!(
            matches!(successful_standalone.read_refresh_controls(app_id,true).await,Err(RefreshReadControlsFailure::Status(error)) if error.body().message == "combined control read failed")
        );
        let disabled = successful_standalone
            .with_effective_app_status_policy(Arc::new(TestEffectiveAppStatusPolicy::new(
                AppStatus::Disabled,
            )))
            .with_refresh_read_controls_policy(policy);
        assert!(
            matches!(disabled.read_refresh_controls(app_id,true).await,Err(RefreshReadControlsFailure::Status(error)) if error.body().kind == ApiErrorType::AppDisabled)
        );
    }

    #[tokio::test]
    async fn combined_refresh_controls_are_per_refresh_and_preserve_status_enforcement() {
        let policy = Arc::new(TestRefreshReadControls::active());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_refresh_read_controls_policy(policy.clone());
        let app_id = Uuid::new_v4();
        assert!(service.read_refresh_controls(app_id, true).await.unwrap());
        assert!(
            !service
                .clone()
                .read_refresh_controls(app_id, false)
                .await
                .unwrap()
        );
        policy.value.lock().unwrap().status.effective_status = AppStatus::Disabled;
        assert!(
            matches!(service.read_refresh_controls(app_id, true).await, Err(RefreshReadControlsFailure::Status(error)) if error.body().kind == ApiErrorType::AppDisabled)
        );
        assert_eq!(
            *policy.calls.lock().unwrap(),
            vec![(app_id, true), (app_id, false), (app_id, true)]
        );
    }

    #[tokio::test]
    async fn either_custom_refresh_policy_disables_combined_controls() {
        let policy = Arc::new(TestRefreshReadControls::active());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_refresh_read_controls_policy(policy.clone());
        let app_id = Uuid::new_v4();
        let custom_topics = service
            .clone()
            .with_coarse_topics_policy(Arc::new(TestCoarseTopics(false)));
        assert!(custom_topics.refresh_read_controls.is_none());
        assert!(
            !custom_topics
                .read_refresh_controls(app_id, true)
                .await
                .unwrap()
        );
        let custom_status = service.with_effective_app_status_policy(Arc::new(
            TestEffectiveAppStatusPolicy::new(AppStatus::Disabled),
        ));
        assert!(custom_status.refresh_read_controls.is_none());
        assert!(matches!(
            custom_status.read_refresh_controls(app_id, true).await,
            Err(RefreshReadControlsFailure::Status(_))
        ));
        assert!(policy.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn separate_refresh_policies_keep_status_priority_and_topic_retry_errors() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_coarse_topics_policy(Arc::new(TestCoarseTopicsFailure {
            fail: AtomicBool::new(true),
        }));
        let app_id = Uuid::new_v4();
        assert!(!service.read_refresh_controls(app_id, false).await.unwrap());
        assert!(matches!(
            service.read_refresh_controls(app_id, true).await,
            Err(RefreshReadControlsFailure::Topics(_))
        ));
        let disabled = service.with_effective_app_status_policy(Arc::new(
            TestEffectiveAppStatusPolicy::new(AppStatus::Disabled),
        ));
        assert!(
            matches!(disabled.read_refresh_controls(app_id,true).await, Err(RefreshReadControlsFailure::Status(error)) if error.body().kind == ApiErrorType::AppDisabled)
        );
    }

    #[tokio::test]
    async fn combined_refresh_controls_do_not_replace_standalone_cancellation_read() {
        let policy = Arc::new(TestRefreshReadControls::active());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_refresh_read_controls_policy(policy.clone())
        .with_statement_cancel_wait_policy(Arc::new(TestStatementCancelWait {
            fail: AtomicBool::new(true),
        }));
        let session = service
            .open(Some(Uuid::new_v4()), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        assert!(
            service
                .process_refresh_bounded(session.session_id, Value::Null, Value::Null)
                .await
                .is_err()
        );
        assert!(policy.calls.lock().unwrap().is_empty());
    }

    #[derive(Default)]
    struct TestCoarseTopicsFailure {
        fail: AtomicBool,
    }

    struct TestDatalogCachePolicy {
        calls: std::sync::atomic::AtomicUsize,
        settings: DatalogCacheSettings,
    }

    struct TestReceiveTimeout {
        duration: Duration,
        app_ids: Mutex<Vec<Option<Uuid>>>,
    }

    #[derive(Default)]
    struct TestStatementCancelWait {
        fail: AtomicBool,
    }

    struct TestLiveReceiveExecutorPolicy {
        expanded: AtomicBool,
        refresh_started: AtomicBool,
        refresh_gate: Arc<tokio::sync::Semaphore>,
    }

    struct TestEffectiveAppStatusPolicy {
        status: AtomicU8,
        app_ids: Mutex<Vec<Uuid>>,
    }

    impl TestEffectiveAppStatusPolicy {
        fn new(status: AppStatus) -> Self {
            Self {
                status: AtomicU8::new(match status {
                    AppStatus::Active => 0,
                    AppStatus::ReadOnly => 1,
                    AppStatus::Disabled => 2,
                }),
                app_ids: Mutex::new(Vec::new()),
            }
        }

        fn set(&self, status: AppStatus) {
            self.status.store(
                match status {
                    AppStatus::Active => 0,
                    AppStatus::ReadOnly => 1,
                    AppStatus::Disabled => 2,
                },
                Ordering::Release,
            );
        }
    }

    impl EffectiveAppStatusPolicy for TestEffectiveAppStatusPolicy {
        fn status(&self, app_id: Uuid) -> AppStatusFuture<'_> {
            self.app_ids.lock().unwrap().push(app_id);
            let status = match self.status.load(Ordering::Acquire) {
                0 => AppStatus::Active,
                1 => AppStatus::ReadOnly,
                _ => AppStatus::Disabled,
            };
            Box::pin(std::future::ready(Ok(AppStatusSnapshot {
                own_status: status,
                effective_status: status,
            })))
        }
    }

    impl Default for TestLiveReceiveExecutorPolicy {
        fn default() -> Self {
            Self {
                expanded: AtomicBool::new(false),
                refresh_started: AtomicBool::new(false),
                refresh_gate: Arc::new(tokio::sync::Semaphore::new(0)),
            }
        }
    }

    impl RealtimeReceiveExecutorPolicy for TestLiveReceiveExecutorPolicy {
        fn current_selection(
            &self,
        ) -> crate::realtime_receive_policy::RealtimeReceiveExecutorSelection {
            let class = if self.expanded.load(Ordering::Acquire) {
                crate::realtime_receive_dispatch::ReceiveExecutorClass::Expanded
            } else {
                crate::realtime_receive_dispatch::ReceiveExecutorClass::Standard
            };
            crate::realtime_receive_policy::RealtimeReceiveExecutorSelection {
                class,
                worker_limit: class.worker_limit(1),
                fallback: None,
            }
        }

        fn refresh<'a>(
            &'a self,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = crate::realtime_receive_policy::RealtimeReceiveExecutorSelection,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.refresh_started.store(true, Ordering::Release);
                let permit = Arc::clone(&self.refresh_gate)
                    .acquire_owned()
                    .await
                    .unwrap();
                permit.forget();
                self.expanded.store(true, Ordering::Release);
                self.current_selection()
            })
        }
    }

    impl StatementCancelWaitPolicy for TestStatementCancelWait {
        fn wait_duration(&self) -> crate::statement_cancel::StatementCancelWaitFuture<'_> {
            let result = if self.fail.load(Ordering::Acquire) {
                Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "invalid statement cancellation policy",
                ))
            } else {
                Ok(Duration::from_millis(10))
            };
            Box::pin(std::future::ready(result))
        }
    }

    #[derive(Default)]
    struct TestJoinRoomLogger {
        app_ids: Mutex<Vec<Uuid>>,
    }

    impl crate::JoinRoomLogger for TestJoinRoomLogger {
        fn log_join(&self, app_id: Uuid) {
            self.app_ids.lock().unwrap().push(app_id);
        }
    }

    impl ReceiveTimeoutPolicy for TestReceiveTimeout {
        fn timeout(&self, app_id: Option<Uuid>) -> ReceiveTimeoutFuture<'_> {
            self.app_ids.lock().unwrap().push(app_id);
            Box::pin(std::future::ready(Ok(self.duration)))
        }
    }

    impl DatalogCachePolicy for TestDatalogCachePolicy {
        fn settings(&self) -> DatalogCacheSettingsFuture<'_> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(std::future::ready(Ok(self.settings)))
        }
    }

    impl CoarseTopicsPolicy for TestCoarseTopics {
        fn use_coarse_topics(&self, _app_id: Uuid) -> CoarseTopicsFuture<'_> {
            Box::pin(std::future::ready(Ok(self.0)))
        }
    }

    impl CoarseTopicsPolicy for TestCoarseTopicsFailure {
        fn use_coarse_topics(&self, _app_id: Uuid) -> CoarseTopicsFuture<'_> {
            let result = if self.fail.load(Ordering::Acquire) {
                Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "refresh policy failed",
                ))
            } else {
                Ok(false)
            };
            Box::pin(std::future::ready(result))
        }
    }

    impl RefreshThrottlePolicy for TestRefreshThrottle {
        fn delay_after_noop(&self, app_id: Uuid) -> RefreshThrottleFuture<'_> {
            Box::pin(std::future::ready(Ok(
                (app_id == self.app_id).then_some(self.delay)
            )))
        }
    }

    impl AppRateLimitPolicy for TestAppRateLimits {
        fn check_app_ids<'a>(
            &'a self,
            app_ids: &'a [Uuid],
        ) -> crate::app_rate_limit::AppRateLimitFuture<'a> {
            Box::pin(async move {
                if app_ids.contains(&self.limited_app) {
                    Err(ApiError::new(
                        ApiErrorType::RateLimited,
                        "You're making too many requests.",
                    ))
                } else {
                    Ok(())
                }
            })
        }
    }

    impl AdminDataAuthorizer for TestAuthorizer {
        fn authorize(
            &self,
            _context: AdminRequestContext,
            _scope: crate::AdminDataScope,
        ) -> impl std::future::Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(AttributeCatalog::default()))
        }

        fn load_catalog(
            &self,
            _app_id: Uuid,
        ) -> impl std::future::Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(AttributeCatalog::default()))
        }
    }

    #[derive(Clone)]
    struct CatalogAuthorizer(AttributeCatalog);

    impl AdminDataAuthorizer for CatalogAuthorizer {
        fn authorize(
            &self,
            _context: AdminRequestContext,
            _scope: crate::AdminDataScope,
        ) -> impl std::future::Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(self.0.clone()))
        }

        fn load_catalog(
            &self,
            _app_id: Uuid,
        ) -> impl std::future::Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(self.0.clone()))
        }
    }

    struct CountingCatalogAuthorizer {
        catalog: AttributeCatalog,
        loads: AtomicUsize,
        fail_next: AtomicBool,
    }

    impl AdminDataAuthorizer for CountingCatalogAuthorizer {
        fn authorize(
            &self,
            _context: AdminRequestContext,
            _scope: crate::AdminDataScope,
        ) -> impl std::future::Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(self.catalog.clone()))
        }
        fn load_catalog(
            &self,
            _app_id: Uuid,
        ) -> impl std::future::Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            self.loads.fetch_add(1, Ordering::Relaxed);
            std::future::ready(if self.fail_next.swap(false, Ordering::Relaxed) {
                Err(ApiError::new(ApiErrorType::Unknown, "catalog unavailable"))
            } else {
                Ok(self.catalog.clone())
            })
        }
    }

    struct TestSyncSubscription {
        app_id: Uuid,
        token_hash: [u8; 32],
        query: Map<String, Value>,
        attribute_ids: Vec<Uuid>,
    }

    struct IdempotentLeaveCluster {
        leave_calls: AtomicUsize,
        active_leaves: AtomicUsize,
        max_active_leaves: AtomicUsize,
        block_leaves: AtomicBool,
        fail_leaves: AtomicBool,
        leave_started: Arc<tokio::sync::Semaphore>,
        leave_release: Arc<tokio::sync::Semaphore>,
        revision: AtomicI64,
        rooms: Mutex<HashMap<RoomKey, BTreeMap<Uuid, PeerPresence>>>,
    }

    impl Default for IdempotentLeaveCluster {
        fn default() -> Self {
            Self {
                leave_calls: AtomicUsize::new(0),
                active_leaves: AtomicUsize::new(0),
                max_active_leaves: AtomicUsize::new(0),
                block_leaves: AtomicBool::new(false),
                fail_leaves: AtomicBool::new(false),
                leave_started: Arc::new(tokio::sync::Semaphore::new(0)),
                leave_release: Arc::new(tokio::sync::Semaphore::new(0)),
                revision: AtomicI64::new(0),
                rooms: Mutex::new(HashMap::new()),
            }
        }
    }

    impl RuntimeRealtimeCluster for IdempotentLeaveCluster {
        fn join<'a>(
            &'a self,
            room: &'a RoomKey,
            peer: PeerPresence,
        ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
            let mut rooms = self.rooms.lock().unwrap();
            let peers = rooms.entry(room.clone()).or_default();
            peers.insert(peer.peer_id, peer);
            let snapshot = peers.clone();
            Box::pin(std::future::ready(Ok(RuntimeRoomSnapshot {
                revision: self.revision.fetch_add(1, Ordering::Relaxed) + 1,
                peers: snapshot,
            })))
        }

        fn set_presence<'a>(
            &'a self,
            room: &'a RoomKey,
            session_id: Uuid,
            data: Value,
        ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
            let mut rooms = self.rooms.lock().unwrap();
            let peers = rooms.entry(room.clone()).or_default();
            if let Some(peer) = peers.get_mut(&session_id) {
                peer.data = data;
            }
            let snapshot = peers.clone();
            Box::pin(std::future::ready(Ok(RuntimeRoomSnapshot {
                revision: self.revision.fetch_add(1, Ordering::Relaxed) + 1,
                peers: snapshot,
            })))
        }

        fn leave<'a>(
            &'a self,
            room: &'a RoomKey,
            session_id: Uuid,
        ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
            Box::pin(async move {
                self.leave_calls.fetch_add(1, Ordering::Relaxed);
                if self.fail_leaves.load(Ordering::Acquire) {
                    return Err("leave unavailable".to_owned());
                }
                if self.block_leaves.load(Ordering::Acquire) {
                    let active = self.active_leaves.fetch_add(1, Ordering::AcqRel) + 1;
                    self.max_active_leaves.fetch_max(active, Ordering::AcqRel);
                    self.leave_started.add_permits(1);
                    let permit = self
                        .leave_release
                        .acquire()
                        .await
                        .map_err(|error| error.to_string())?;
                    permit.forget();
                    self.active_leaves.fetch_sub(1, Ordering::AcqRel);
                }
                let mut rooms = self.rooms.lock().unwrap();
                let peers = rooms.entry(room.clone()).or_default();
                peers.remove(&session_id);
                let snapshot = peers.clone();
                Ok(RuntimeRoomSnapshot {
                    revision: self.revision.fetch_add(1, Ordering::Relaxed) + 1,
                    peers: snapshot,
                })
            })
        }

        fn publish_broadcast<'a>(
            &'a self,
            _room: &'a RoomKey,
            _payload: BroadcastPayload,
        ) -> RealtimeClusterFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn publish_stream_append(
            &self,
            _app_id: Uuid,
            _append: RuntimeStreamAppendResult,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn publish_stream_disconnect(
            &self,
            _app_id: Uuid,
            _stream_id: Uuid,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn register_sse_session(
            &self,
            _app_id: Uuid,
            _session_id: Uuid,
            _token_hash: [u8; 32],
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn unregister_sse_session(&self, _session_id: Uuid) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn forward_sse_push(
            &self,
            _request: RuntimeSsePushRequest,
        ) -> RealtimeClusterFuture<'_, bool> {
            Box::pin(std::future::ready(Ok(false)))
        }

        fn publish_wal(&self, _record: WalRecord) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn publish_byop_wal(
            &self,
            _app_id: Uuid,
            _record: instant_pg::ByopWalRecord,
            _topics: Vec<instant_pg::ByopTopic>,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn subscribe(
            &self,
            _consumer: RealtimeClusterConsumerKind,
        ) -> RealtimeClusterFuture<'_, broadcast::Receiver<RuntimeRealtimeClusterDelivery>>
        {
            let (_sender, receiver) = broadcast::channel(1);
            Box::pin(std::future::ready(Ok(receiver)))
        }

        fn acknowledge(
            &self,
            _consumer: RealtimeClusterConsumerKind,
            _event_id: i64,
        ) -> RealtimeClusterFuture<'_, ()> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    #[derive(Default)]
    struct TestData {
        sync_subscriptions: Mutex<HashMap<Uuid, TestSyncSubscription>>,
        sync_snapshot_rows: std::sync::atomic::AtomicUsize,
        allow_guest_streams: bool,
        byop_topics: Mutex<Option<Vec<Value>>>,
        instant_topics: Mutex<Option<RuntimeFrameworkQueryTopics>>,
        join_rows_actors: Mutex<Vec<AdminActor>>,
        query_count: std::sync::atomic::AtomicUsize,
        query_workloads: Mutex<Vec<crate::postgres_pool::DatabaseWorkload>>,
        query_pool: Mutex<Option<crate::PostgresPool>>,
        native_catalog_count: AtomicUsize,
        query_errors_remaining: AtomicUsize,
        query_version: std::sync::atomic::AtomicUsize,
        realtime_catalog: Mutex<Option<AttributeCatalog>>,
        realtime_catalog_calls: std::sync::atomic::AtomicUsize,
        realtime_catalog_error: Mutex<Option<ApiError>>,
        realtime_batch_calls: std::sync::atomic::AtomicUsize,
        realtime_batch_modes: Mutex<Vec<Vec<RuntimeRealtimeQueryMode>>>,
        realtime_batch_reuse_disabled: Mutex<Vec<bool>>,
        query_join_rows_reuse_disabled: Mutex<Vec<bool>>,
        query_delay: Mutex<Option<Duration>>,
        query_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        query_gate_app_id: Mutex<Option<Uuid>>,
        transaction_calls: Mutex<Vec<Vec<Value>>>,
        transaction_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        transaction_error: Mutex<Option<ApiError>>,
    }

    async fn open_test_query_session(
        service: &RuntimeRealtimeService<TestAuthorizer, TestData>,
        app_id: Uuid,
    ) -> RuntimeRealtimeSession {
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        session
    }

    #[tokio::test]
    async fn app_runtime_report_counts_local_sessions_and_query_hashes() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data, Uuid::new_v4());
        let app_id = Uuid::new_v4();
        let _session = open_test_query_session(&service, app_id).await;
        let report = service.app_runtime_report(app_id);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.subscriptions_by_query_hash.len(), 1);
        assert_eq!(report.subscriptions_by_query_hash[0].count, 1);
        assert_eq!(
            report.subscriptions_by_query_hash[0].hash,
            normalized_query_hash(&json!({"goals": {}})).unwrap()
        );
        assert_eq!(report.oldest_refresh_wait_ms, 0);
        assert_eq!(
            service.app_runtime_report(Uuid::new_v4()),
            RuntimeAppRuntimeReport::default()
        );
    }

    fn wal_sink_consecutive_timeouts(
        service: &RuntimeRealtimeService<TestAuthorizer, TestData>,
        session_id: Uuid,
    ) -> u32 {
        sessions(&service.inner)
            .get(&session_id)
            .unwrap()
            .wal_sink_consecutive_timeouts
    }

    #[tokio::test]
    async fn unrepresentable_query_numbers_get_an_error_reply_and_do_not_stall_sessions() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut poisoned = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(poisoned.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        poisoned.recv().await.unwrap();
        let mut healthy = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(healthy.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        healthy.recv().await.unwrap();

        // `1e400` parses under arbitrary precision but overflows `f64`, which
        // used to panic query hashing and abandon the receive group.
        let message: Value = serde_json::from_str(
            r#"{"op": "add-query", "client-event-id": "poison", "q": {"goals": {"$": {"limit": 1e400}}}}"#,
        )
        .unwrap();
        service.message(poisoned.session_id, message).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), poisoned.recv())
            .await
            .expect("poisoned session must receive an error reply")
            .unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "validation-failed");
        assert_eq!(error["status"], 400);
        assert_eq!(error["client-event-id"], "poison");

        // The panic-free rejection must not poison the dispatcher or the app.
        service
            .message(
                healthy.session_id,
                json!({"op": "add-query", "client-event-id": "healthy", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(1), healthy.recv())
            .await
            .expect("the second session must keep working")
            .unwrap();
        assert_eq!(reply["op"], "add-query-ok");
        assert_eq!(reply["client-event-id"], "healthy");
        assert_eq!(
            data.query_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the rejected query must not reach the data backend"
        );
    }

    #[tokio::test]
    async fn queued_refresh_disconnect_releases_admission_before_any_backend_work() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_refresh_admission(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(2).unwrap());
        let app = Uuid::new_v4();
        let session = open_test_query_session(&service, app).await;
        let held = service
            .inner
            .refresh_admission
            .acquire(Uuid::new_v4())
            .await
            .unwrap();
        let before = data.realtime_batch_calls.load(Ordering::Relaxed);
        let worker = service.clone();
        let refresh = tokio::spawn(async move {
            worker
                .apply_wal_record(&refreshing_wal_record(app, 84, 7, 0x42))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.metrics_snapshot().refresh_waiting != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(data.realtime_batch_calls.load(Ordering::Relaxed), before);
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), refresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.metrics_snapshot().refresh_waiting != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(held);
        assert_eq!(service.metrics_snapshot().refresh_active, 0);
        assert_eq!(data.realtime_batch_calls.load(Ordering::Relaxed), before);
    }

    #[tokio::test]
    async fn durable_refresh_admission_overload_retries_without_acknowledging_early() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_refresh_admission(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap());
        let a = open_test_query_session(&service, Uuid::new_v4()).await;
        let b = open_test_query_session(&service, Uuid::new_v4()).await;
        let held = service
            .inner
            .refresh_admission
            .acquire(Uuid::new_v4())
            .await
            .unwrap();
        let isn = Isn::new(7, instant_wal::Lsn::from_u64(0x42));
        let mut first = service
            .enqueue_durable_refresh(a.session_id, 84, isn, 84)
            .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.metrics_snapshot().refresh_waiting != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut second = service
            .enqueue_durable_refresh(b.session_id, 85, isn, 85)
            .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while service
                .metrics_snapshot()
                .refresh_admission_rejections_total
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            first.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            second.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), async {
            first.await.unwrap();
            second.await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(service.metrics_snapshot().refresh_active, 0);
        assert_eq!(service.metrics_snapshot().refresh_waiting, 0);
    }

    #[tokio::test]
    async fn unrelated_wal_preserves_registered_inflight_refresh() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let observed_attr = Uuid::new_v4();
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(observed_attr));
        let mut session = open_test_query_session(&service, app_id).await;
        let make_record = |attr| {
            test_wal_record(
                app_id,
                vec![triple_entry(
                    WalAction::Insert,
                    wal_columns(&[
                        ("entity_id", json!(Uuid::new_v4())),
                        ("attr_id", json!(attr)),
                        ("value", json!("true")),
                        ("ea", json!(true)),
                    ]),
                    vec![],
                )],
            )
        };
        data.query_version.store(1, Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let before = data.query_count.load(Ordering::Relaxed);
        let refresh_service = service.clone();
        let affected_record = make_record(observed_attr);
        let refresh = tokio::spawn(async move {
            refresh_service
                .apply_wal_record(&affected_record)
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Same app, unrelated dependency: index routing selects no observer,
        // but cache invalidation used to cancel and restart its running query.
        for _ in 0..8 {
            let selected = service
                .prepare_wal_record_for_sessions(
                    &make_record(Uuid::new_v4()),
                    None,
                    false,
                    &mut BTreeMap::new(),
                )
                .await
                .unwrap();
            assert!(selected.is_empty());
            tokio::task::yield_now().await;
        }
        gate.add_permits(16);
        tokio::time::timeout(Duration::from_secs(1), refresh)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-ok");
        assert_eq!(
            data.query_count.load(Ordering::Relaxed) - before,
            1,
            "unrelated WAL restarted the observer's database query"
        );
    }

    struct GatedCoarseTopics {
        calls: AtomicUsize,
        gate: Arc<tokio::sync::Semaphore>,
    }

    impl CoarseTopicsPolicy for GatedCoarseTopics {
        fn use_coarse_topics(&self, _app_id: Uuid) -> CoarseTopicsFuture<'_> {
            let first = self.calls.fetch_add(1, Ordering::Relaxed) == 0;
            Box::pin(async move {
                if first {
                    self.gate.acquire().await.unwrap().forget();
                }
                Ok(false)
            })
        }
    }

    #[tokio::test]
    async fn initial_query_catches_up_after_wal_during_load() {
        assert_initial_query_invalidation(false, false).await;
    }

    #[tokio::test]
    async fn initial_query_catches_up_after_wal_before_registration() {
        assert_initial_query_invalidation(true, false).await;
    }

    #[tokio::test]
    async fn initial_query_reauthorizes_after_rules_before_registration() {
        assert_initial_query_invalidation(true, true).await;
    }

    #[tokio::test]
    async fn initial_query_ignores_unrelated_wal_during_load() {
        assert_initial_query_ignores_unrelated_wal(false).await;
    }

    #[tokio::test]
    async fn initial_query_ignores_unrelated_wal_before_registration() {
        assert_initial_query_ignores_unrelated_wal(true).await;
    }

    #[tokio::test]
    async fn initial_query_catches_up_when_the_routing_window_is_unknown() {
        let data = Arc::new(TestData::default());
        let attr = Uuid::new_v4();
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "q":{"goals":{}}}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // More unrelated invalidations than the log retains: the window can no
        // longer be checked, so registration must fall back to a catch-up.
        for _ in 0..=instant_realtime::DATA_INVALIDATION_LOG_LIMIT {
            service
                .apply_wal_record(&attribute_wal_record(app_id, Uuid::new_v4()))
                .await
                .unwrap();
        }
        data.query_version.store(1, Ordering::Relaxed);
        gate.add_permits(16);
        let initial = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial["op"], "add-query-ok");
        let catch_up = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(catch_up["op"], "refresh-ok");
        assert_eq!(data.query_count.load(Ordering::Relaxed), 2);
    }

    async fn assert_initial_query_ignores_unrelated_wal(metadata_gap: bool) {
        let data = Arc::new(TestData::default());
        let attr = Uuid::new_v4();
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let policy = Arc::new(GatedCoarseTopics {
            calls: AtomicUsize::new(0),
            gate: Arc::clone(&gate),
        });
        let mut service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        if metadata_gap {
            service = service.with_coarse_topics_policy(policy.clone());
        } else {
            *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        }
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "q":{"goals":{}}}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while if metadata_gap {
                policy.calls.load(Ordering::Relaxed) == 0
            } else {
                data.query_count.load(Ordering::Relaxed) == 0
            } {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Another write in the same app, routed while this query executed,
        // shares no topic with the query's dependencies.
        service
            .apply_wal_record(&attribute_wal_record(app_id, Uuid::new_v4()))
            .await
            .unwrap();
        gate.add_permits(16);
        let initial = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial["op"], "add-query-ok");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), session.recv())
                .await
                .is_err(),
            "an unrelated write must not queue a duplicate catch-up execution"
        );
        assert_eq!(data.query_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            sessions(&service.inner)[&session.session_id].queries
                [&canonical_query(json!({"goals":{}}).as_object().unwrap())]
                .stale_generation,
            0
        );
    }

    #[tokio::test]
    async fn acknowledged_query_does_not_timeout_while_catch_up_refresh_is_blocked() {
        assert_acknowledged_query_catch_up(false).await;
    }

    #[tokio::test]
    async fn acknowledged_query_catches_up_with_a_full_refresh_inbox() {
        assert_acknowledged_query_catch_up(true).await;
    }

    async fn assert_acknowledged_query_catch_up(full_inbox: bool) {
        let data = Arc::new(TestData::default());
        let app_id = Uuid::new_v4();
        let metadata_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let refresh_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let policy = Arc::new(GatedCoarseTopics {
            calls: AtomicUsize::new(0),
            gate: Arc::clone(&metadata_gate),
        });
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_coarse_topics_policy(policy.clone())
        .with_receive_timeout_policy(Arc::new(StaticReceiveTimeout(Duration::from_millis(100))));
        let attr = Uuid::new_v4();
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr));
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "client-event-id":"initial", "q":{"goals":{}}}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while policy.calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Commit after the initial result was computed but before registration.
        service
            .apply_wal_record(&attribute_wal_record(app_id, attr))
            .await
            .unwrap();
        data.query_version.store(1, Ordering::Relaxed);
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&refresh_gate));
        if full_inbox {
            let sender = {
                let mut entries = sessions(&service.inner);
                let current = entries.get_mut(&session.session_id).unwrap();
                current.refresh_not_before =
                    Some(tokio::time::Instant::now() + Duration::from_millis(300));
                current.refresh_sender.clone()
            };
            let pending = || PendingRefresh {
                processed_tx_id: json!(0),
                processed_isn: Value::Null,
                window_id: None,
                completions: vec![],
            };
            sender.try_send(pending()).unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while sessions(&service.inner)[&session.session_id]
                    .refresh_not_before
                    .is_some()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            for _ in 0..REFRESH_INBOX_LIMIT {
                sender.try_send(pending()).unwrap();
            }
            assert_eq!(sender.capacity(), 0);
        }
        metadata_gate.add_permits(1);
        let initial = session.recv().await.unwrap();
        assert_eq!(initial["op"], "add-query-ok");
        assert_eq!(initial["client-event-id"], "initial");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), session.recv())
                .await
                .is_err(),
            "an acknowledged query must not later emit a receive timeout while its refresh waits"
        );
        refresh_gate.add_permits(1);
        let refresh = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(
            refresh["computations"][0]["instaql-result"][0]["version"],
            1
        );
    }

    async fn assert_initial_query_invalidation(metadata_gap: bool, security: bool) {
        let data = Arc::new(TestData::default());
        let attr = Uuid::new_v4();
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let policy = Arc::new(GatedCoarseTopics {
            calls: AtomicUsize::new(0),
            gate: Arc::clone(&gate),
        });
        let mut service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        if metadata_gap {
            service = service.with_coarse_topics_policy(policy.clone());
        } else {
            *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        }
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "q":{"goals":{}}}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while if metadata_gap {
                policy.calls.load(Ordering::Relaxed) == 0
            } else {
                data.query_count.load(Ordering::Relaxed) == 0
            } {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            sessions(&service.inner)[&session.session_id]
                .queries
                .is_empty()
        );
        data.query_version.store(1, Ordering::Relaxed);
        if security {
            service.apply_cache_eviction(&WalCacheEviction::Rules {
                app_id: Some(app_id),
            });
        } else {
            service
                .apply_wal_record(&test_wal_record(
                    app_id,
                    vec![triple_entry(
                        WalAction::Insert,
                        wal_columns(&[
                            ("entity_id", json!(Uuid::new_v4())),
                            ("attr_id", json!(attr)),
                            ("value", json!("true")),
                            ("ea", json!(true)),
                        ]),
                        vec![],
                    )],
                ))
                .await
                .unwrap();
        }
        gate.add_permits(16);
        let initial = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial["op"], "add-query-ok");
        if security {
            assert_eq!(
                initial["result"][0]["version"], 1,
                "rules invalidation must not publish a pre-invalidation result"
            );
        } else {
            assert_eq!(initial["result"][0]["version"], 0);
            let catch_up = tokio::time::timeout(Duration::from_secs(1), session.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(catch_up["op"], "refresh-ok");
            assert_eq!(
                catch_up["computations"][0]["instaql-result"][0]["version"], 1,
                "a commit before registration must trigger a fresh catch-up query"
            );
        }
        assert_eq!(data.query_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn overlapping_wal_keeps_newer_refresh_after_retired_result() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let attr = Uuid::new_v4();
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr));
        let mut session = open_test_query_session(&service, app_id).await;
        let mut record = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(Uuid::new_v4())),
                    ("attr_id", json!(attr)),
                    ("value", json!("true")),
                    ("ea", json!(true)),
                ]),
                vec![],
            )],
        );
        record.tx_id = 10;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        data.query_version.store(1, Ordering::Relaxed);
        let before = data.query_count.load(Ordering::Relaxed);
        let first_service = service.clone();
        let first_record = record.clone();
        let first =
            tokio::spawn(
                async move { first_service.apply_wal_record(&first_record).await.unwrap() },
            );
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        data.query_version.store(2, Ordering::Relaxed);
        record.tx_id = 11;
        let selected = service
            .prepare_wal_record_for_sessions(&record, None, false, &mut BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(selected, vec![session.session_id]);
        let second_service = service.clone();
        let id = session.session_id;
        let second = tokio::spawn(async move {
            second_service
                .enqueue_refresh(id, json!(11), Value::Null)
                .await
                .unwrap();
        });
        gate.add_permits(1);
        let older = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(older["processed-tx-id"], 10);
        assert_eq!(older["computations"][0]["instaql-result"][0]["version"], 1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) < before + 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            sessions(&service.inner)[&session.session_id]
                .queries
                .values()
                .all(|q| q.stale_generation != 0)
        );
        gate.add_permits(1);
        let newer = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(newer["processed-tx-id"], 11);
        assert_eq!(newer["computations"][0]["instaql-result"][0]["version"], 2);
        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(data.query_count.load(Ordering::Relaxed), before + 2);
    }

    #[test]
    fn refresh_watermark_merge_preserves_unknown_lag_and_completed_floor() {
        let make = |tx, isn| PendingRefresh {
            processed_tx_id: tx,
            processed_isn: isn,
            window_id: None,
            completions: vec![],
        };
        let mut known = make(json!(101), json!("9/0/FF"));
        known.coalesce(make(json!(100), json!("9/0/1")));
        assert_eq!(known.processed_tx_id, 101);
        assert_eq!(known.processed_isn, "9/0/FF");
        let floor = known.watermark();
        let mut late = make(json!(99), json!("9/0/2"));
        late.preserve_completed_watermark(floor);
        assert_eq!(late.watermark(), floor);
        for unknown_first in [false, true] {
            let mut unknown = make(Value::Null, Value::Null);
            let mut known = make(json!(102), json!("9/0/100"));
            let mut combined = if unknown_first {
                unknown.coalesce(known);
                unknown
            } else {
                known.coalesce(unknown);
                known
            };
            combined.preserve_completed_watermark(floor);
            assert_eq!(combined.watermark(), ProcessedWatermark::default());
        }
    }

    #[tokio::test]
    async fn late_catch_up_does_not_lower_queued_durable_watermark() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        data.query_version.store(1, Ordering::Relaxed);
        let mut record = test_wal_record(app_id, vec![]);
        record.tx_id = 100;
        let before = data.query_count.load(Ordering::Relaxed);
        let first_service = service.clone();
        let first_record = record.clone();
        let first =
            tokio::spawn(
                async move { first_service.apply_wal_record(&first_record).await.unwrap() },
            );
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        record.tx_id = 101;
        service
            .prepare_wal_record_for_sessions(&record, None, false, &mut BTreeMap::new())
            .await
            .unwrap();
        let durable = service
            .enqueue_durable_refresh(session.session_id, 101, record.isn, 101)
            .await;
        let sender = sessions(&service.inner)[&session.session_id]
            .refresh_sender
            .clone();
        sender
            .send(PendingRefresh {
                processed_tx_id: json!(100),
                processed_isn: json!(record.isn),
                window_id: None,
                completions: vec![],
            })
            .await
            .unwrap();
        data.query_version.store(2, Ordering::Relaxed);
        gate.add_permits(2);
        let older = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(older["processed-tx-id"], 100);
        let newer = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(newer["computations"][0]["instaql-result"][0]["version"], 2);
        assert_eq!(
            newer["processed-tx-id"], 101,
            "late initial catch-up must not replace the newer durable watermark"
        );
        tokio::time::timeout(Duration::from_secs(1), durable)
            .await
            .unwrap()
            .unwrap();
        first.await.unwrap();
    }

    #[tokio::test]
    async fn removing_query_preserves_another_sessions_inflight_loader() {
        assert_session_removal_preserves_shared_loader(false).await;
    }

    #[tokio::test]
    async fn disconnect_preserves_another_sessions_inflight_loader() {
        assert_session_removal_preserves_shared_loader(true).await;
    }

    async fn assert_session_removal_preserves_shared_loader(disconnect: bool) {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut first = open_test_query_session(&service, app_id).await;
        let mut second = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(second.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        second.recv().await.unwrap();
        clear_app_query_cache(&service.inner, app_id);
        let initial = data.query_count.load(Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        service
            .message(
                second.session_id,
                json!({"op":"add-query", "q":{"goals":{}}}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) == initial {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if disconnect {
            drop(first);
        } else {
            service
                .message(
                    first.session_id,
                    json!({"op":"remove-query", "q":{"goals":{}}}),
                )
                .await
                .unwrap();
            assert_eq!(first.recv().await.unwrap()["op"], "remove-query-ok");
        }
        let restarted = tokio::time::timeout(Duration::from_millis(100), async {
            while data.query_count.load(Ordering::Relaxed) == initial + 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        gate.add_permits(10);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), second.recv())
                .await
                .unwrap()
                .unwrap()["op"],
            "add-query-ok"
        );
        assert!(
            !restarted,
            "subscription removal restarted another session's in-flight database query"
        );
    }

    #[tokio::test]
    async fn grouped_receive_serializes_one_key_and_runs_different_keys_concurrently() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));

        tokio::time::timeout(
            Duration::from_millis(50),
            service.message(
                session.session_id,
                json!({"op": "add-query", "q": {"a": {}}, "client-event-id": "add-a"}),
            ),
        )
        .await
        .expect("receive ingress must return after enqueue")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "remove-query", "q": {"a": {}}, "client-event-id": "remove-a"}),
            )
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"b": {}}, "client-event-id": "add-b"}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the different query group must run while query a is blocked");
        gate.add_permits(2);

        let mut client_events = Vec::new();
        for _ in 0..3 {
            client_events.push(
                tokio::time::timeout(Duration::from_secs(1), session.recv())
                    .await
                    .unwrap()
                    .unwrap()["client-event-id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let add_a = client_events
            .iter()
            .position(|event| event == "add-a")
            .unwrap();
        let remove_a = client_events
            .iter()
            .position(|event| event == "remove-a")
            .unwrap();
        assert!(add_a < remove_a, "same-group protocol order must be stable");
        assert_eq!(service.local_query_count(), 1);
    }

    #[tokio::test]
    async fn grouped_receive_cleanup_cancels_active_work_and_drains_the_session_group() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        *data.query_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"blocked": {}}}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        drop(session);
        let dispatcher = service.grouped_receive_dispatcher();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let metrics = dispatcher.metrics_snapshot();
                if metrics.active_groups == 0 && metrics.queued == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect cancellation and group drain must be bounded");
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn add_query_retries_across_repeated_cache_invalidations() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));

        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        for expected_query_count in 1..=2 {
            tokio::time::timeout(Duration::from_secs(1), async {
                while data.query_count.load(Ordering::Relaxed) != expected_query_count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("query retry did not begin after cache invalidation");
            query_caches(&service.inner)
                .get(&app_id)
                .expect("query cache exists while query is loading")
                .clear();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("query did not retry after the second cache invalidation");
        gate.add_permits(1);

        let response = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .expect("retried query did not complete")
            .expect("runtime session closed");
        assert_eq!(response["op"], "add-query-ok");
    }

    #[tokio::test]
    async fn blocked_transaction_does_not_block_a_different_receive_group() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        let transaction_gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.transaction_gate.lock().unwrap() = Some(Arc::clone(&transaction_gate));

        service
            .message(
                session.session_id,
                json!({"op": "transact", "tx-steps": [], "client-event-id": "tx"}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.transaction_calls.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"users": {}}, "client-event-id": "query"}),
            )
            .await
            .unwrap();
        let query = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .expect("a query group must execute while the transaction is blocked")
            .unwrap();
        assert_eq!(query["client-event-id"], "query");

        transaction_gate.add_permits(1);
        let transaction = session.recv().await.unwrap();
        assert_eq!(transaction["client-event-id"], "tx");
    }

    #[tokio::test]
    async fn disconnect_does_not_permanently_close_shared_subscription_groups() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let shared_key = RealtimeGroupKey::ModifySync(serialized_group_part(
            &json!({"subscription-id": "shared"}),
            "subscription-id",
        ));
        let dispatcher = service.grouped_receive_dispatcher();
        let first = dispatcher
            .enqueue(
                shared_key.clone(),
                GroupedReceiveMessage {
                    session_id: session.session_id,
                    message: json!({"op": "remove-sync", "subscription-id": "shared"}),
                },
            )
            .await
            .unwrap();
        drop(session);
        first.wait().await.unwrap().as_ref().unwrap();

        let completion = dispatcher
            .enqueue(
                shared_key,
                GroupedReceiveMessage {
                    session_id: Uuid::new_v4(),
                    message: json!({"op": "remove-sync", "subscription-id": "shared"}),
                },
            )
            .await
            .expect("a reconnect may reuse the subscription-scoped group key");
        completion.wait().await.unwrap().as_ref().unwrap();
    }

    #[tokio::test]
    async fn live_executor_refresh_updates_cached_class_without_blocking_ingress() {
        let policy = Arc::new(TestLiveReceiveExecutorPolicy::default());
        let service =
            RuntimeRealtimeService::new(
                Arc::new(TestAuthorizer),
                Arc::new(TestData::default()),
                Uuid::new_v4(),
            )
            .with_receive_executor_policy(
                Arc::clone(&policy) as Arc<dyn RealtimeReceiveExecutorPolicy>
            );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();

        tokio::time::timeout(
            Duration::from_millis(50),
            service.message(session.session_id, json!({"op": "init", "app-id": app_id})),
        )
        .await
        .expect("ingress must use the synchronous cached selection")
        .unwrap();
        session.recv().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !policy.refresh_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            service.grouped_receive_dispatcher().executor_class(),
            crate::realtime_receive_dispatch::ReceiveExecutorClass::Standard,
        );

        policy.refresh_gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !policy.expanded.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"users": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(
            service.grouped_receive_dispatcher().executor_class(),
            crate::realtime_receive_dispatch::ReceiveExecutorClass::Expanded,
        );
    }

    fn test_sync_attribute_id() -> Uuid {
        Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap()
    }

    fn wal_columns(values: &[(&str, Value)]) -> Vec<WalColumn> {
        values
            .iter()
            .map(|(name, value)| WalColumn {
                name: (*name).to_owned(),
                value: value.clone(),
            })
            .collect()
    }

    fn test_wal_record(app_id: Uuid, triple_changes: Vec<instant_wal::WalEntry>) -> WalRecord {
        let lsn = instant_wal::Lsn::from_u64(0x42);
        WalRecord {
            app_id,
            tx_id: 84,
            isn: instant_wal::Isn::new(7, lsn),
            previous_isn: None,
            tx_created_at: None,
            tx_bytes: 0,
            next_lsn: lsn,
            attr_changes: Vec::new(),
            ident_changes: Vec::new(),
            triple_changes,
            messages: Vec::new(),
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }

    fn attribute_wal_record(app_id: Uuid, attr: Uuid) -> WalRecord {
        test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(Uuid::new_v4())),
                    ("attr_id", json!(attr)),
                    ("value", json!("true")),
                    ("ea", json!(true)),
                ]),
                vec![],
            )],
        )
    }

    fn refreshing_wal_record(app_id: Uuid, tx_id: i64, segment: i32, lsn: u64) -> WalRecord {
        let mut record = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(Uuid::new_v4())),
                    ("attr_id", json!(Uuid::new_v4())),
                    ("value", json!(tx_id)),
                    ("created_at", json!(tx_id)),
                ]),
                Vec::new(),
            )],
        );
        record.tx_id = tx_id;
        record.isn = Isn::new(segment, instant_wal::Lsn::from_u64(lsn));
        record
            .attr_changes
            .push(triple_entry(WalAction::Insert, Vec::new(), Vec::new()));
        record
    }

    fn ordinary_wal_record(app_id: Uuid, tx_id: i64, segment: i32, lsn: u64) -> WalRecord {
        let mut record = refreshing_wal_record(app_id, tx_id, segment, lsn);
        record.attr_changes.clear();
        record
    }

    fn triple_entry(
        action: WalAction,
        columns: Vec<WalColumn>,
        identity: Vec<WalColumn>,
    ) -> instant_wal::WalEntry {
        instant_wal::WalEntry {
            action,
            tx_bytes: 0,
            table: Some("triples".to_owned()),
            columns,
            identity,
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn entity_log_message(etype: &str, entity_id: Uuid, attrs: &Value) -> instant_wal::WalEntry {
        instant_wal::WalEntry {
            action: WalAction::Message,
            tx_bytes: 0,
            table: None,
            columns: Vec::new(),
            identity: Vec::new(),
            prefix: Some("update_ents".to_owned()),
            content: Some(json!([[etype, entity_id, attrs]]).to_string()),
            lsn: None,
            nextlsn: None,
        }
    }

    impl RuntimeJoinRowsQueryBackend for TestData {
        async fn query_join_rows_on_native_catalog(
            &self,
            context: AdminRequestContext,
            request: AdminQueryRequest,
            catalog_app_id: Uuid,
            _catalog: AttributeCatalog,
        ) -> Result<RuntimeFrameworkResponse, ApiError> {
            assert_eq!(context.app_id, catalog_app_id);
            self.native_catalog_count.fetch_add(1, Ordering::Relaxed);
            self.query_join_rows(context, request).await
        }

        fn query_join_rows(
            &self,
            context: AdminRequestContext,
            _request: AdminQueryRequest,
        ) -> impl std::future::Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send
        {
            self.join_rows_actors.lock().unwrap().push(context.actor);
            self.query_join_rows_reuse_disabled
                .lock()
                .unwrap()
                .push(crate::root_frame_cache::shared_root_frame_reuse_disabled());
            self.query_workloads
                .lock()
                .unwrap()
                .push(crate::postgres_pool::DatabaseWorkload::current());
            let pool = self.query_pool.lock().unwrap().clone();
            self.query_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let byop_topics = self.byop_topics.lock().unwrap().clone();
            let instant_topics = self.instant_topics.lock().unwrap().clone();
            let query_error = self
                .query_errors_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
            let version = self
                .query_version
                .load(std::sync::atomic::Ordering::Relaxed);
            let delay = *self.query_delay.lock().unwrap();
            let gate = self.query_gate.lock().unwrap().clone().filter(|_| {
                self.query_gate_app_id
                    .lock()
                    .unwrap()
                    .is_none_or(|app| app == context.app_id)
            });
            async move {
                let _lease = if let Some(pool) = &pool {
                    Some(pool.acquire().await.map_err(|_| {
                        ApiError::new(ApiErrorType::Unknown, "test pool checkout failed")
                    })?)
                } else {
                    None
                };
                if query_error {
                    return Err(ApiError::new(ApiErrorType::Unknown, "db error"));
                }
                if let Some(gate) = gate {
                    let permit = gate.acquire_owned().await.unwrap();
                    permit.forget();
                }
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                Ok(RuntimeFrameworkResponse {
                    result: json!([{
                        "data": {
                            "datalog-result": {"join-rows": [[]]},
                            "aggregate": {"goals": {"count": 2}},
                            "page-info": {
                                "goals": {
                                    "has-next-page?": false,
                                    "has-previous-page?": true,
                                }
                            },
                        },
                        "version": version,
                    }]),
                    attrs: json!([]),
                    byop_topics,
                    instant_topics,
                })
            }
        }
    }

    impl RuntimeSyncBackend for TestData {
        fn sync_snapshot(
            &self,
            _request: RuntimeSyncSnapshotRequest,
        ) -> impl Future<Output = Result<RuntimeSyncSnapshot, ApiError>> + Send {
            let row_count = self
                .sync_snapshot_rows
                .load(std::sync::atomic::Ordering::Relaxed);
            let join_rows = if row_count == 0 {
                vec![
                    json!([["entity-1", "attr-1", "value-1", 1]]),
                    json!([["entity-2", "attr-1", "value-2", 2]]),
                ]
            } else {
                (0..row_count)
                    .map(|index| {
                        json!([[
                            format!("entity-{index}"),
                            test_sync_attribute_id(),
                            index,
                            index
                        ]])
                    })
                    .collect()
            };
            std::future::ready(Ok(RuntimeSyncSnapshot {
                join_rows,
                tx_id: 0,
                attribute_ids: vec![test_sync_attribute_id()],
            }))
        }

        fn create_sync_subscription(
            &self,
            request: RuntimeSyncSubscriptionCreate,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.sync_subscriptions.lock().unwrap().insert(
                request.subscription_id,
                TestSyncSubscription {
                    app_id: request.context.app_id,
                    token_hash: Sha256::digest(request.token.as_bytes()).into(),
                    query: request.query,
                    attribute_ids: request.attribute_ids,
                },
            );
            std::future::ready(Ok(()))
        }

        fn delete_sync_subscription(
            &self,
            _context: AdminRequestContext,
            subscription_id: Uuid,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.sync_subscriptions
                .lock()
                .unwrap()
                .remove(&subscription_id);
            std::future::ready(Ok(()))
        }

        fn resume_sync_subscription(
            &self,
            request: RuntimeSyncSubscriptionResume,
        ) -> impl Future<Output = Result<RuntimeSyncSubscription, ApiError>> + Send {
            let result = self
                .sync_subscriptions
                .lock()
                .unwrap()
                .get(&request.subscription_id)
                .filter(|subscription| {
                    subscription.app_id == request.context.app_id
                        && constant_time_eq(
                            &subscription.token_hash,
                            &Sha256::digest(request.token.as_bytes()),
                        )
                })
                .map(|subscription| RuntimeSyncSubscription {
                    app_id: subscription.app_id,
                    query: subscription.query.clone(),
                    attribute_ids: subscription.attribute_ids.clone(),
                })
                .ok_or_else(|| protocol_error("Invalid sync subscription or token."));
            std::future::ready(result)
        }
    }

    impl RuntimeStreamPermissionBackend for TestData {
        fn authorize_stream_create(
            &self,
            context: AdminRequestContext,
            _client_id: String,
            _rule_params: Value,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(test_stream_permission(self.allow_guest_streams, &context))
        }

        fn authorize_stream_view(
            &self,
            context: AdminRequestContext,
            _stream: Value,
            _rule_params: Value,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(test_stream_permission(self.allow_guest_streams, &context))
        }
    }

    impl RuntimeRealtimeCatalogBackend for TestData {
        fn resolve_realtime_catalog(
            &self,
            _app_id: Uuid,
            instant_catalog: AttributeCatalog,
        ) -> impl Future<Output = Result<(AttributeCatalog, bool), ApiError>> + Send {
            self.realtime_catalog_calls.fetch_add(1, Ordering::Relaxed);
            let error = self.realtime_catalog_error.lock().unwrap().take();
            let external_catalog = self.realtime_catalog.lock().unwrap().clone();
            std::future::ready(error.map_or_else(
                || Ok(external_catalog.map_or((instant_catalog, false), |catalog| (catalog, true))),
                Err,
            ))
        }

        fn refresh_realtime_batch(
            &self,
            _app_id: Uuid,
            instant_catalog: AttributeCatalog,
            queries: Vec<RuntimeRealtimeBatchQuery>,
        ) -> impl Future<Output = Result<RuntimeRealtimeRefreshBatch, ApiError>> + Send {
            self.realtime_batch_calls.fetch_add(1, Ordering::Relaxed);
            self.realtime_catalog_calls.fetch_add(1, Ordering::Relaxed);
            self.realtime_batch_modes
                .lock()
                .unwrap()
                .push(queries.iter().map(|request| request.mode).collect());
            self.realtime_batch_reuse_disabled
                .lock()
                .unwrap()
                .push(crate::root_frame_cache::shared_root_frame_reuse_disabled());
            let error = self.realtime_catalog_error.lock().unwrap().take();
            let external_catalog = self.realtime_catalog.lock().unwrap().clone();
            let version = self.query_version.load(Ordering::Relaxed);
            std::future::ready(if let Some(error) = error {
                Err(error)
            } else if let Some(catalog) = external_catalog {
                self.query_count.fetch_add(queries.len(), Ordering::Relaxed);
                Ok(RuntimeRealtimeRefreshBatch {
                    results: Some(
                        queries
                            .into_iter()
                            .map(|_| {
                                Ok(RuntimeRealtimeBatchQueryResult {
                                    result: json!([{
                                        "data": {"datalog-result": {"join-rows": [[]]}},
                                        "version": version,
                                        "catalog-size": catalog.attributes.len(),
                                    }]),
                                    result_meta: json!({}),
                                    byop_topics: None,
                                    instant_topics: None,
                                })
                            })
                            .collect(),
                    ),
                    catalog: Arc::new(catalog),
                    external_catalog: true,
                })
            } else {
                Ok(RuntimeRealtimeRefreshBatch {
                    catalog: Arc::new(instant_catalog),
                    external_catalog: false,
                    results: None,
                })
            })
        }
    }

    impl RuntimeRealtimeIdentityBackend for TestData {
        fn resolve_realtime_user_id(
            &self,
            context: AdminRequestContext,
        ) -> impl Future<Output = Result<Option<Uuid>, ApiError>> + Send {
            let user_id = match context.actor {
                AdminActor::RefreshToken(token) => Some(token),
                AdminActor::Admin | AdminActor::Email(_) | AdminActor::Guest => None,
            };
            std::future::ready(Ok(user_id))
        }
    }

    #[tokio::test]
    async fn custom_identity_backend_ignores_native_metadata_hint_by_default() {
        let backend = TestData::default();
        let token = Uuid::new_v4();
        let context = AdminRequestContext {
            app_id: Uuid::new_v4(),
            bearer_token: None,
            actor: AdminActor::RefreshToken(token),
            client_ip: None,
        };
        assert_eq!(
            backend
                .resolve_realtime_user_id_on_native_catalog(
                    context,
                    Uuid::new_v4(),
                    AttributeCatalog::default(),
                )
                .await
                .unwrap(),
            Some(token),
            "a custom backend's existing resolver keeps control of identity",
        );
    }

    impl RuntimeRealtimeTransactionBackend for TestData {
        fn transact_realtime(
            &self,
            _context: AdminRequestContext,
            request: AdminTransactRequest,
        ) -> impl Future<Output = Result<RuntimeRealtimeTransaction, ApiError>> + Send {
            let call = {
                let mut calls = self.transaction_calls.lock().unwrap();
                calls.push(request.steps);
                i64::try_from(calls.len()).unwrap()
            };
            let gate = self.transaction_gate.lock().unwrap().clone();
            let error = self.transaction_error.lock().unwrap().clone();
            async move {
                if let Some(gate) = gate {
                    let permit = gate.acquire_owned().await.unwrap();
                    permit.forget();
                }
                if let Some(error) = error {
                    return Err(error);
                }
                Ok(RuntimeRealtimeTransaction {
                    tx_id: 41 + call,
                    fallback_isn: Isn::new(2, instant_wal::Lsn::from_u64(0x40)),
                })
            }
        }
    }

    fn test_stream_permission(
        allow_guest_streams: bool,
        context: &AdminRequestContext,
    ) -> Result<(), ApiError> {
        if context.actor == AdminActor::Admin || allow_guest_streams {
            Ok(())
        } else {
            Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Permission denied for `$streams`",
            ))
        }
    }

    impl AdminDataBackend for TestData {
        type PreparedQueryPermissions = AdminRequestContext;

        fn query(
            &self,
            _context: AdminRequestContext,
            _request: AdminQueryRequest,
        ) -> impl std::future::Future<Output = Result<Value, ApiError>> + Send {
            self.query_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let version = self
                .query_version
                .load(std::sync::atomic::Ordering::Relaxed);
            std::future::ready(Ok(json!({
                "version": version,
                "data": {
                    "datalog-result": {
                        "join-rows": [
                            [["entity-1", "attr-1", "value-1", "2026-01-01T00:00:00Z"]],
                            [["entity-2", "attr-1", "value-2", "2026-01-01T00:00:01Z"]]
                        ]
                    }
                }
            })))
        }

        fn transact(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactRequest,
        ) -> impl std::future::Future<
            Output = Result<instant_http::AdminTransactResponse, ApiError>,
        > + Send {
            std::future::ready(Ok(instant_http::AdminTransactResponse { tx_id: 42 }))
        }

        fn prepare_query_permissions(
            &self,
            context: AdminRequestContext,
            _rules_override: Option<Value>,
        ) -> impl std::future::Future<Output = Result<Self::PreparedQueryPermissions, ApiError>> + Send
        {
            std::future::ready(Ok(context))
        }

        fn query_permissions(
            &self,
            _prepared: Self::PreparedQueryPermissions,
            _request: AdminQueryPermissionsRequest,
        ) -> impl std::future::Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(json!({})))
        }

        fn transact_permissions(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactPermissionsRequest,
        ) -> impl std::future::Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(json!({})))
        }
    }

    impl RuntimeTreeQueryBackend for TestData {
        fn query_tree(
            &self,
            context: AdminRequestContext,
            request: AdminQueryRequest,
        ) -> impl Future<Output = Result<RuntimeTreeQueryResponse, ApiError>> + Send {
            let result = AdminDataBackend::query(self, context, request);
            async move {
                Ok(RuntimeTreeQueryResponse {
                    result: result.await?,
                    result_meta: json!({
                        "page-info": {
                            "treeGoals": {
                                "start-cursor": ["entity-1", "attr-1", null, 1],
                                "end-cursor": ["entity-2", "attr-1", null, 2],
                                "has-next-page?": false,
                                "has-previous-page?": true,
                            }
                        },
                        "aggregate": {"treeGoals": {"count": 2}},
                    }),
                    instant_topics: None,
                })
            }
        }
    }

    #[tokio::test]
    async fn websocket_session_runs_init_query_transaction_and_room_protocol() {
        let join_room_logger = Arc::new(TestJoinRoomLogger::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::ZERO)
        .with_join_room_logger(Arc::clone(&join_room_logger) as Arc<dyn crate::JoinRoomLogger>);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open_with_origin(
                Some(app_id),
                RuntimeTransport::WebSocket,
                Some("https://app.example.com".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(service.session_count(), 1);

        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": app_id, "client-event-id": "init"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}, "client-event-id": "q"}),
            )
            .await
            .unwrap();
        let initial_query = session.recv().await.unwrap();
        assert_eq!(initial_query["op"], "add-query-ok");
        assert_eq!(initial_query["processed-tx-id"], Value::Null);
        assert_eq!(initial_query["processed-isn"], Value::Null);
        let report = service.local_session_report();
        assert_eq!(report[&app_id].count, 1);
        assert_eq!(report[&app_id].origins["https://app.example.com"], 1);
        assert_eq!(service.local_session_count(), 1);
        assert_eq!(service.local_query_count(), 1);

        service
            .message(
                session.session_id,
                json!({
                    "op": "add-query",
                    "return-type": "tree",
                    "q": {"treeGoals": {}},
                    "client-event-id": "tree-q"
                }),
            )
            .await
            .unwrap();
        let tree = session.recv().await.unwrap();
        assert_eq!(tree["op"], "add-query-ok");
        assert!(tree["result"].is_object());
        assert_eq!(tree["result-meta"]["aggregate"]["treeGoals"]["count"], 2);
        assert_eq!(
            tree["result-meta"]["page-info"]["treeGoals"]["has-previous-page?"],
            true
        );
        assert_eq!(service.local_query_count(), 2);

        service
            .message(
                session.session_id,
                json!({"op": "transact", "tx-steps": [], "client-event-id": "tx"}),
            )
            .await
            .unwrap();
        let transaction = session.recv().await.unwrap();
        assert_eq!(transaction["tx-id"], 42);
        assert_eq!(transaction["isn"], "2/0/40");

        service
            .message(
                session.session_id,
                json!({"op": "join-room", "room-id": "lobby", "data": {"online": true}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(session.recv().await.unwrap()["op"], "join-room-ok");
        assert_eq!(*join_room_logger.app_ids.lock().unwrap(), vec![app_id]);

        drop(session);
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn admin_core_queries_preserve_join_rows_across_initial_and_refresh_results() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret",
                    "versions": {"@instantdb/core": "0.20.5"},
                }),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["auth"]["admin?"], true);

        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let initial = session.recv().await.unwrap();
        assert_eq!(initial["op"], "add-query-ok");
        assert!(initial["result"].is_array());
        assert_eq!(
            initial["result"][0]["data"]["aggregate"]["goals"]["count"],
            2
        );
        assert_eq!(
            initial["result"][0]["data"]["page-info"]["goals"]["has-previous-page?"],
            true
        );
        assert_eq!(
            *data.join_rows_actors.lock().unwrap(),
            vec![AdminActor::Admin]
        );

        data.query_version.store(1, Ordering::Relaxed);
        let mut schema_record = test_wal_record(app_id, Vec::new());
        schema_record
            .attr_changes
            .push(triple_entry(WalAction::Insert, Vec::new(), Vec::new()));
        service.apply_wal_record(&schema_record).await.unwrap();
        let refresh = session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        let computation = &refresh["computations"][0];
        assert_eq!(computation["instaql-query"], json!({"goals": {}}));
        assert!(computation["instaql-result"].is_array());
        assert_eq!(
            computation["instaql-result"][0]["data"]["aggregate"]["goals"]["count"],
            2
        );
        assert_eq!(
            *data.realtime_batch_modes.lock().unwrap(),
            vec![vec![RuntimeRealtimeQueryMode::JoinRows]]
        );
        assert_eq!(
            *data.join_rows_actors.lock().unwrap(),
            vec![AdminActor::Admin, AdminActor::Admin]
        );

        service
            .message(
                session.session_id,
                json!({
                    "op": "add-query",
                    "return-type": "tree",
                    "q": {"treeGoals": {}},
                }),
            )
            .await
            .unwrap();
        let tree = session.recv().await.unwrap();
        assert_eq!(tree["op"], "add-query-ok");
        assert!(tree["result"].is_object());
        assert_eq!(tree["result-meta"]["aggregate"]["treeGoals"]["count"], 2);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn realtime_param_envelopes_cover_init_sync_stream_room_and_unknown_ops() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();

        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": false, "client-event-id": "missing"}),
            )
            .await
            .unwrap();
        assert_eq!(
            session.recv().await.unwrap(),
            json!({
                "op": "error",
                "status": 400,
                "client-event-id": "missing",
                "original-event": {"op": "init", "app-id": false, "client-event-id": "missing"},
                "type": "param-missing",
                "message": "Missing parameter: [\"app-id\"]",
                "hint": {"in": ["app-id"]},
            })
        );

        let malformed = app_id.simple().to_string();
        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": malformed, "client-event-id": "malformed"}),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["type"], "param-malformed");
        assert_eq!(error["message"], "Malformed parameter: [\"app-id\"]");
        assert_eq!(error["hint"]["original-input"], malformed);

        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": format!("  {app_id}  ")}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        service
            .message(session.session_id, json!({"op": "init"}))
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["type"], "validation-failed");
        assert_eq!(error["message"], "`init` has already run for this session.");

        service
            .message(session.session_id, json!({"op": "remove-sync"}))
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["type"], "param-missing");
        assert_eq!(error["hint"], json!({"in": ["subscription-id"]}));

        service
            .message(
                session.session_id,
                json!({"op": "start-stream", "client-id": "", "reconnect-token": Uuid::new_v4()}),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["type"], "param-malformed");
        assert_eq!(
            error["hint"],
            json!({"in": ["client-id"], "original-input": ""})
        );

        service
            .message(
                session.session_id,
                json!({"op": "join-room", "room-id": "", "client-event-id": "empty-room"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-presence");
        let joined = session.recv().await.unwrap();
        assert_eq!(joined["op"], "join-room-ok");
        assert_eq!(joined["room-id"], "");

        let unknown = json!({"op": "future-op", "client-event-id": "unknown"});
        service
            .message(session.session_id, unknown.clone())
            .await
            .unwrap();
        assert_eq!(
            session.recv().await.unwrap(),
            json!({
                "op": "error",
                "status": 400,
                "client-event-id": "unknown",
                "original-event": unknown,
                "type": "param-malformed",
                "message": "Invalid op",
                "hint": {"op": "future-op"},
            })
        );
    }

    #[tokio::test]
    async fn realtime_multi_parameter_errors_follow_the_oracle_validation_order() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut guest = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(guest.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        guest.recv().await.unwrap();

        service
            .message(
                guest.session_id,
                json!({"op": "start-stream", "client-id": "client"}),
            )
            .await
            .unwrap();
        assert_eq!(guest.recv().await.unwrap()["type"], "permission-denied");

        service
            .message(
                guest.session_id,
                json!({"op": "append-stream", "stream-id": Uuid::new_v4(), "done": "yes"}),
            )
            .await
            .unwrap();
        let error = guest.recv().await.unwrap();
        assert_eq!(error["type"], "param-malformed");
        assert_eq!(error["hint"]["in"], json!(["done"]));

        let mut admin = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                admin.session_id,
                json!({"op": "init", "app-id": app_id, "__admin-token": "admin"}),
            )
            .await
            .unwrap();
        admin.recv().await.unwrap();
        service
            .message(admin.session_id, json!({"op": "resync-table"}))
            .await
            .unwrap();
        let error = admin.recv().await.unwrap();
        assert_eq!(error["type"], "param-missing");
        assert_eq!(error["hint"]["in"], json!(["tx-id"]));
    }

    #[tokio::test]
    async fn websocket_and_sse_sessions_share_unknown_op_error_envelopes() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        for transport in [
            RuntimeTransport::WebSocket,
            RuntimeTransport::ServerSentEvents,
        ] {
            let transport_name = match transport {
                RuntimeTransport::WebSocket => "websocket",
                RuntimeTransport::ServerSentEvents => "sse",
            };
            let mut session = service.open(None, transport).await.unwrap();
            if transport == RuntimeTransport::ServerSentEvents {
                assert_eq!(session.recv().await.unwrap()["op"], "sse-init");
            }
            let original = json!({"op": 7, "client-event-id": transport_name});
            service
                .message(session.session_id, original.clone())
                .await
                .unwrap();
            assert_eq!(
                session.recv().await.unwrap(),
                json!({
                    "op": "error",
                    "status": 400,
                    "client-event-id": transport_name,
                    "original-event": original,
                    "type": "param-malformed",
                    "message": "Invalid op",
                    "hint": {"op": 7},
                })
            );
        }
    }

    #[tokio::test]
    async fn clustered_presence_preserves_a_leading_nul_room_id() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let room = RoomKey::new(app_id, "\0room");
        let peer = PeerPresence {
            peer_id: session.session_id,
            instance_id: "remote".to_owned(),
            user: None,
            data: json!({}),
        };
        deliver_presence_update(
            &service.inner,
            PresenceUpdate {
                deliveries: vec![PresenceDelivery {
                    session_id: session.session_id,
                    room,
                    data: Arc::new(BTreeMap::from([(session.session_id, peer)])),
                    edits: None,
                }],
            },
        );
        assert_eq!(session.recv().await.unwrap()["room-id"], "\0room");
    }

    #[tokio::test]
    async fn clustered_empty_room_supports_presence_and_broadcast() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();

        service
            .message(
                session.session_id,
                json!({"op": "join-room", "room-id": ""}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["room-id"], "");
        let joined = session.recv().await.unwrap();
        assert_eq!(joined["op"], "join-room-ok");
        assert_eq!(joined["room-id"], "");

        service
            .message(
                session.session_id,
                json!({"op": "set-presence", "room-id": "", "data": {"online": true}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["room-id"], "");
        let presence_ok = session.recv().await.unwrap();
        assert_eq!(presence_ok["op"], "set-presence-ok");
        assert_eq!(presence_ok["room-id"], "");

        service
            .message(
                session.session_id,
                json!({"op": "client-broadcast", "room-id": "", "topic": "ping"}),
            )
            .await
            .unwrap();
        let broadcast_ok = session.recv().await.unwrap();
        assert_eq!(broadcast_ok["op"], "client-broadcast-ok");
        assert_eq!(broadcast_ok["room-id"], "");
        assert!(
            cluster
                .rooms
                .lock()
                .unwrap()
                .contains_key(&RoomKey::new(app_id, ""))
        );
    }

    #[tokio::test]
    async fn effective_app_status_is_live_at_realtime_operation_boundaries() {
        let app_id = Uuid::new_v4();
        let status = Arc::new(TestEffectiveAppStatusPolicy::new(AppStatus::ReadOnly));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_effective_app_status_policy(Arc::clone(&status) as Arc<dyn EffectiveAppStatusPolicy>);
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": app_id, "client-event-id": "init"}),
            )
            .await
            .unwrap();
        let initialized = session.recv().await.unwrap();
        assert_eq!(initialized["op"], "init-ok");
        assert_eq!(initialized["app-status"]["status"], "read-only");

        status.set(AppStatus::Disabled);
        service
            .message(
                session.session_id,
                json!({"op": "join-room", "room-id": "lobby", "client-event-id": "disabled"}),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["status"], 400);
        assert_eq!(error["type"], "app-disabled");
        assert_eq!(error["message"], "This app is currently disabled.");
        assert_eq!(error["hint"], json!({"status": "disabled"}));

        status.set(AppStatus::ReadOnly);
        service
            .message(
                session.session_id,
                json!({"op": "join-room", "room-id": "lobby", "client-event-id": "read-only"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(session.recv().await.unwrap()["op"], "join-room-ok");
        assert!(
            status
                .app_ids
                .lock()
                .unwrap()
                .iter()
                .all(|observed| *observed == app_id)
        );
    }

    #[tokio::test]
    async fn presence_leave_and_broadcast_do_not_recheck_app_status_after_join() {
        let app_id = Uuid::new_v4();
        let status = Arc::new(TestEffectiveAppStatusPolicy::new(AppStatus::ReadOnly));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_effective_app_status_policy(Arc::clone(&status) as Arc<dyn EffectiveAppStatusPolicy>);
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": app_id, "client-event-id": "init"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        let observed_at_init = status.app_ids.lock().unwrap().len();

        service
            .message(
                session.session_id,
                json!({"op": "join-room", "room-id": "lobby", "client-event-id": "join"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(session.recv().await.unwrap()["op"], "join-room-ok");

        // The JVM rechecks read access only at init and join-room. Presence
        // mutations, broadcasts, and leaves on an already-joined session stay
        // live for an in-room reader even after the app flips to disabled.
        status.set(AppStatus::Disabled);
        service
            .message(
                session.session_id,
                json!({"op": "set-presence", "room-id": "lobby", "data": {"count": 1}}),
            )
            .await
            .unwrap();
        loop {
            let event = session.recv().await.unwrap();
            if event["op"] == "set-presence-ok" {
                break;
            }
        }
        service
            .message(
                session.session_id,
                json!({"op": "client-broadcast", "room-id": "lobby", "topic": "t", "data": {"x": 1}}),
            )
            .await
            .unwrap();
        let broadcast_event = session.recv().await.unwrap();
        assert_eq!(
            broadcast_event["op"], "client-broadcast-ok",
            "broadcast response: {broadcast_event}"
        );
        service
            .message(
                session.session_id,
                json!({"op": "leave-room", "room-id": "lobby", "client-event-id": "leave"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "leave-room-ok");

        // Exactly the init and join-room status reads happened; none of the
        // presence operations rechecked the shared policy.
        let mut observed = status.app_ids.lock().unwrap();
        assert_eq!(observed.len(), observed_at_init + 1);
        assert_eq!(observed.pop(), Some(app_id));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn presence_delivery_negotiates_per_recipient_across_all_sources() {
        let machine_id = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            machine_id,
        );
        let app_id = Uuid::new_v4();
        let mut old = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut new = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for (session_id, version) in [(old.session_id, "0.17.5"), (new.session_id, "0.17.6")] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "versions": {"@instantdb/core": version}
                    }),
                )
                .await
                .unwrap();
        }
        old.recv().await.unwrap();
        new.recv().await.unwrap();

        service
            .message(
                old.session_id,
                json!({"op": "join-room", "room-id": "lobby", "data": {"count": 0}}),
            )
            .await
            .unwrap();
        assert_eq!(old.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(old.recv().await.unwrap()["op"], "join-room-ok");
        service
            .message(
                new.session_id,
                json!({"op": "join-room", "room-id": "lobby", "data": {"count": 0}}),
            )
            .await
            .unwrap();
        assert_eq!(old.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(new.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(new.recv().await.unwrap()["op"], "join-room-ok");

        service
            .message(
                new.session_id,
                json!({"op": "set-presence", "room-id": "lobby", "data": {"count": 1}}),
            )
            .await
            .unwrap();
        assert_eq!(old.recv().await.unwrap()["op"], "refresh-presence");
        let patch = new.recv().await.unwrap();
        assert_eq!(patch["op"], "patch-presence");
        assert!(!patch["edits"].as_array().unwrap().is_empty());
        assert_eq!(new.recv().await.unwrap()["op"], "set-presence-ok");

        service
            .message(
                new.session_id,
                json!({"op": "set-presence", "room-id": "lobby", "data": {"count": 1}}),
            )
            .await
            .unwrap();
        assert_eq!(new.recv().await.unwrap()["op"], "set-presence-ok");
        for session in [&mut old, &mut new] {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), session.recv())
                    .await
                    .is_err(),
                "semantic no-op presence edits must be suppressed"
            );
        }

        let room = RoomKey::new(app_id, "lobby");
        let mut clustered = service
            .room_store()
            .lock()
            .unwrap()
            .room_data(app_id, "lobby")
            .unwrap()
            .clone();
        let remote_peer = Uuid::new_v4();
        clustered.insert(
            remote_peer,
            PeerPresence {
                peer_id: remote_peer,
                instance_id: Uuid::new_v4().to_string(),
                user: None,
                data: json!({"remote": true}),
            },
        );
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::Presence {
                source_machine_id: Uuid::new_v4(),
                revision: 1,
                room: room.clone(),
                peers: clustered.clone(),
            },
        )
        .await;
        assert_eq!(old.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(new.recv().await.unwrap()["op"], "patch-presence");
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::Presence {
                source_machine_id: Uuid::new_v4(),
                revision: 2,
                room,
                peers: clustered,
            },
        )
        .await;
        for session in [&mut old, &mut new] {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), session.recv())
                    .await
                    .is_err()
            );
        }

        let mut departing = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                departing.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "versions": {"@instantdb/core": "0.17.6"}
                }),
            )
            .await
            .unwrap();
        departing.recv().await.unwrap();
        service
            .message(
                departing.session_id,
                json!({"op": "join-room", "room-id": "lobby"}),
            )
            .await
            .unwrap();
        assert_eq!(old.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(new.recv().await.unwrap()["op"], "patch-presence");
        assert_eq!(departing.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(departing.recv().await.unwrap()["op"], "join-room-ok");
        drop(departing);
        assert_eq!(old.recv().await.unwrap()["op"], "refresh-presence");
        let disconnected = new.recv().await.unwrap();
        assert_eq!(disconnected["op"], "patch-presence");
        assert!(
            disconnected["edits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edit| edit[1] == "-")
        );

        service
            .message(
                old.session_id,
                json!({"op": "leave-room", "room-id": "lobby"}),
            )
            .await
            .unwrap();
        let removed = new.recv().await.unwrap();
        assert_eq!(removed["op"], "patch-presence");
        assert!(
            removed["edits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edit| edit[1] == "-")
        );
        assert_eq!(old.recv().await.unwrap()["op"], "leave-room-ok");
    }

    #[tokio::test]
    async fn blocked_presence_recipient_coalesces_burst_before_outbound_queue_limit() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let session_id = session.session_id;
        let room = RoomKey::new(app_id, "lobby");

        for sequence in 0..1_024 {
            let peer = PeerPresence {
                peer_id: session_id,
                instance_id: "device".to_owned(),
                user: None,
                data: json!({"sequence": sequence}),
            };
            deliver_presence_update(
                &service.inner,
                PresenceUpdate {
                    deliveries: vec![PresenceDelivery {
                        session_id,
                        room: room.clone(),
                        data: Arc::new(BTreeMap::from([(session_id, peer)])),
                        edits: None,
                    }],
                },
            );
        }

        let sender = sessions(&service.inner)[&session_id].sender.clone();
        assert!(!sender.state.closed.load(Ordering::Acquire));
        assert!(sender.state.queued.load(Ordering::Acquire) <= 1);
        let mut received_latest = false;
        for _ in 0..130 {
            let event = tokio::time::timeout(Duration::from_secs(1), session.recv())
                .await
                .expect("coalesced presence delivery stalled")
                .expect("presence burst disconnected the recipient");
            assert_eq!(event["op"], "refresh-presence");
            if event["data"][session_id.to_string()]["data"]["sequence"] == 1_023 {
                received_latest = true;
                break;
            }
        }
        assert!(received_latest, "the newest full presence state must win");
        assert!(sessions(&service.inner).contains_key(&session_id));
    }

    #[test]
    fn huge_pending_patch_burst_collapses_to_one_latest_full_snapshot() {
        let (outbound, mut receiver) = mpsc::unbounded_channel();
        let state = Arc::new(OutboundQueueState::default());
        let presence = Arc::new(Mutex::new(OutboundPresenceState::default()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnected_flag = Arc::clone(&disconnected);
        let sender = RuntimeSessionSender {
            sender: outbound,
            state: Arc::clone(&state),
            presence: Arc::clone(&presence),
            disconnect: Arc::new(move || {
                disconnected_flag.store(true, Ordering::Release);
            }),
        };
        let features = ClientFeatures::from_versions(&BTreeMap::from([(
            "@instantdb/core".to_owned(),
            "0.17.6".to_owned(),
        )]));
        let session_id = Uuid::new_v4();
        let room = RoomKey::new(Uuid::new_v4(), "lobby");
        for sequence in 0..10_000 {
            let peer = PeerPresence {
                peer_id: session_id,
                instance_id: "device".to_owned(),
                user: None,
                data: json!({"sequence": sequence}),
            };
            sender.send_presence(
                &features,
                PresenceDelivery {
                    session_id,
                    room: room.clone(),
                    data: Arc::new(BTreeMap::from([(session_id, peer)])),
                    edits: Some(vec![PresenceEdit {
                        path: vec![],
                        operation: instant_realtime::PresenceEditOperation::Replace,
                        value: Some(Value::from(sequence)),
                    }]),
                },
            );
        }

        assert_eq!(state.queued.load(Ordering::Acquire), 1);
        assert!(!disconnected.load(Ordering::Acquire));
        assert!(matches!(
            receiver.try_recv(),
            Ok(OutboundFrame::PresenceWake { ref room_id }) if room_id == "lobby"
        ));
        let pending = presence.lock().unwrap();
        let PendingPresence::Full { data, bytes } = &pending.pending["lobby"] else {
            panic!("oversized patch burst must collapse to a full snapshot");
        };
        assert!(*bytes <= PRESENCE_FULL_BYTE_LIMIT);
        assert_eq!(data[&session_id].data["sequence"], 9_999);
    }

    #[tokio::test]
    async fn many_pending_rooms_stay_bounded_and_do_not_starve_confirmed_frames() {
        let (outbound, receiver) = mpsc::unbounded_channel();
        let state = Arc::new(OutboundQueueState::default());
        let presence = Arc::new(Mutex::new(OutboundPresenceState::default()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnected_flag = Arc::clone(&disconnected);
        let sender = RuntimeSessionSender {
            sender: outbound.clone(),
            state: Arc::clone(&state),
            presence: Arc::clone(&presence),
            disconnect: Arc::new(move || {
                disconnected_flag.store(true, Ordering::Release);
            }),
        };
        let features = ClientFeatures::default();
        let app_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        for sequence in 0..200 {
            let room_id = format!("room-{sequence}");
            sender.send_presence(
                &features,
                PresenceDelivery {
                    session_id,
                    room: RoomKey::new(app_id, room_id),
                    data: Arc::new(BTreeMap::from([(
                        session_id,
                        PeerPresence {
                            peer_id: session_id,
                            instance_id: "device".to_owned(),
                            user: None,
                            data: json!({"sequence": sequence}),
                        },
                    )])),
                    edits: None,
                },
            );
        }
        sender.send_presence(
            &features,
            PresenceDelivery {
                session_id,
                room: RoomKey::new(app_id, "room-199"),
                data: Arc::new(BTreeMap::from([(
                    session_id,
                    PeerPresence {
                        peer_id: session_id,
                        instance_id: "device".to_owned(),
                        user: None,
                        data: json!({"sequence": 999}),
                    },
                )])),
                edits: None,
            },
        );
        assert_eq!(presence.lock().unwrap().pending.len(), 200);
        assert_eq!(state.queued.load(Ordering::Acquire), 200);
        assert!(state.queued.load(Ordering::Acquire) < OUTBOUND_QUEUE_LIMIT);

        let (confirmed, confirmation) = oneshot::channel();
        sender
            .send_confirmed(json!({"op": "ordinary-confirmed"}), confirmed)
            .unwrap();
        let (transport, mut delivered) = mpsc::channel(1);
        let pump = tokio::spawn(run_outbound_pump(
            receiver,
            transport,
            Arc::clone(&state),
            Arc::clone(&presence),
            sender.disconnect.clone(),
        ));
        let mut presence_count = 0;
        let mut latest_seen = false;
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(frame) = delivered.recv().await {
                if frame["op"] == "ordinary-confirmed" {
                    break;
                }
                presence_count += 1;
                if frame["room-id"] == "room-199" {
                    latest_seen = frame["data"][session_id.to_string()]["data"]["sequence"] == 999;
                }
            }
        })
        .await
        .expect("confirmed frame was starved by presence");
        assert_eq!(confirmation.await.unwrap(), OutboundDelivery::Delivered);
        assert_eq!(presence_count, 200);
        assert!(latest_seen);
        assert_eq!(presence.lock().unwrap().pending.len(), 0);
        assert_eq!(state.queued.load(Ordering::Acquire), 0);
        assert!(!disconnected.load(Ordering::Acquire));
        drop(sender);
        drop(outbound);
        pump.await.unwrap();
    }

    #[test]
    fn distinct_pending_rooms_obey_the_outbound_queue_bound() {
        let (outbound, _receiver) = mpsc::unbounded_channel();
        let state = Arc::new(OutboundQueueState::default());
        let presence = Arc::new(Mutex::new(OutboundPresenceState::default()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnected_flag = Arc::clone(&disconnected);
        let sender = RuntimeSessionSender {
            sender: outbound,
            state: Arc::clone(&state),
            presence: Arc::clone(&presence),
            disconnect: Arc::new(move || {
                disconnected_flag.store(true, Ordering::Release);
            }),
        };
        let app_id = Uuid::new_v4();
        for sequence in 0..=OUTBOUND_QUEUE_LIMIT {
            sender.send_presence(
                &ClientFeatures::default(),
                PresenceDelivery {
                    session_id: Uuid::nil(),
                    room: RoomKey::new(app_id, format!("room-{sequence}")),
                    data: Arc::new(BTreeMap::new()),
                    edits: None,
                },
            );
        }
        assert_eq!(state.queued.load(Ordering::Acquire), OUTBOUND_QUEUE_LIMIT);
        assert_eq!(presence.lock().unwrap().pending.len(), OUTBOUND_QUEUE_LIMIT);
        assert!(disconnected.load(Ordering::Acquire));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn clustered_disconnect_negotiates_one_removal_and_suppresses_repeated_snapshot() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        let machine_id = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            machine_id,
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let app_id = Uuid::new_v4();
        let mut old = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut new = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut departing = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for (session_id, version) in [
            (old.session_id, "0.17.5"),
            (new.session_id, "0.17.6"),
            (departing.session_id, "0.17.6"),
        ] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "versions": {"@instantdb/core": version}
                    }),
                )
                .await
                .unwrap();
        }
        old.recv().await.unwrap();
        new.recv().await.unwrap();
        departing.recv().await.unwrap();

        for session_id in [old.session_id, new.session_id, departing.session_id] {
            service
                .message(session_id, json!({"op": "join-room", "room-id": "lobby"}))
                .await
                .unwrap();
            if session_id == old.session_id {
                old.recv().await.unwrap();
                old.recv().await.unwrap();
            } else if session_id == new.session_id {
                old.recv().await.unwrap();
                new.recv().await.unwrap();
                new.recv().await.unwrap();
            } else {
                old.recv().await.unwrap();
                new.recv().await.unwrap();
                departing.recv().await.unwrap();
                departing.recv().await.unwrap();
            }
        }

        let departing_id = departing.session_id;
        drop(departing);
        let old_removal = old.recv().await.unwrap();
        assert_eq!(old_removal["op"], "refresh-presence");
        assert!(old_removal["data"].get(departing_id.to_string()).is_none());
        let new_removal = new.recv().await.unwrap();
        assert_eq!(new_removal["op"], "patch-presence");
        assert_eq!(
            new_removal["edits"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|edit| edit[1] == "-" && edit[0][0] == departing_id.to_string())
                .count(),
            1
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while cluster.leave_calls.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cluster leave did not complete");
        let room = RoomKey::new(app_id, "lobby");
        let peers = cluster.rooms.lock().unwrap()[&room].clone();
        let revision = cluster.revision.load(Ordering::Relaxed) + 1;
        for _ in 0..2 {
            deliver_cluster_event(
                &service,
                RuntimeRealtimeClusterEvent::Presence {
                    source_machine_id: Uuid::new_v4(),
                    revision,
                    room: room.clone(),
                    peers: peers.clone(),
                },
            )
            .await;
        }
        for session in [&mut old, &mut new] {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), session.recv())
                    .await
                    .is_err(),
                "repeated cluster snapshots must not duplicate disconnect removal"
            );
        }
    }

    #[tokio::test]
    async fn clustered_leave_is_idempotent_for_a_session_not_in_the_room() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        for event_id in ["first", "second"] {
            service
                .message(
                    session.session_id,
                    json!({
                        "op": "leave-room",
                        "room-id": "missing",
                        "client-event-id": event_id,
                    }),
                )
                .await
                .unwrap();
            let response = session.recv().await.unwrap();
            assert_eq!(response["op"], "leave-room-ok");
            assert_eq!(response["client-event-id"], event_id);
        }
        assert_eq!(cluster.leave_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cluster_cleanup_deduplicates_pending_work_and_limits_attempts() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        cluster.block_leaves.store(true, Ordering::Release);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let session_id = Uuid::new_v4();
        let app_id = Uuid::new_v4();
        let rooms = (0..(DEFAULT_CLUSTER_CLEANUP_CONCURRENCY.get() * 2))
            .map(|index| RoomKey::new(app_id, format!("room-{index}")))
            .collect::<Vec<_>>();
        let cleanup = Arc::clone(&service.inner.cleanup);

        cleanup.schedule(
            Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>,
            session_id,
            rooms.clone(),
        );
        cleanup.schedule(
            Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>,
            session_id,
            rooms,
        );

        assert_eq!(
            cleanup.metrics_snapshot().pending,
            DEFAULT_CLUSTER_CLEANUP_CONCURRENCY.get() * 2 + 1
        );
        let started = tokio::time::timeout(
            Duration::from_secs(1),
            cluster
                .leave_started
                .acquire_many(u32::try_from(DEFAULT_CLUSTER_CLEANUP_CONCURRENCY.get()).unwrap()),
        )
        .await
        .expect("cleanup attempts did not start")
        .unwrap();
        started.forget();
        assert_eq!(
            cluster.max_active_leaves.load(Ordering::Acquire),
            DEFAULT_CLUSTER_CLEANUP_CONCURRENCY.get()
        );

        cluster
            .leave_release
            .add_permits(DEFAULT_CLUSTER_CLEANUP_CONCURRENCY.get() * 2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().pending != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup did not drain");
        assert_eq!(
            cluster.leave_calls.load(Ordering::Acquire),
            DEFAULT_CLUSTER_CLEANUP_CONCURRENCY.get() * 2
        );
    }

    #[tokio::test]
    async fn cluster_cleanup_retains_failed_room_leave_until_it_succeeds() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        cluster.fail_leaves.store(true, Ordering::Release);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let cleanup = Arc::clone(&service.inner.cleanup);
        cleanup.schedule(
            Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>,
            Uuid::new_v4(),
            [RoomKey::new(Uuid::new_v4(), "lobby")],
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().failures == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup failure was not recorded");
        let failed = cleanup.metrics_snapshot();
        assert_eq!(failed.pending, 1);
        assert!(failed.oldest_retry_age.is_some());

        cluster.fail_leaves.store(false, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().pending != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup did not retry after recovery");
        assert_eq!(cluster.leave_calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cluster_cleanup_times_out_an_attempt_and_retries_it() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        cluster.block_leaves.store(true, Ordering::Release);
        let cleanup = Arc::new(ClusterCleanupCoordinator::with_attempt_timeout(
            NonZeroUsize::new(1).unwrap(),
            Duration::from_millis(10),
        ));
        cleanup.schedule(
            Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>,
            Uuid::new_v4(),
            [RoomKey::new(Uuid::new_v4(), "lobby")],
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().failures == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup attempt did not time out");
        assert_eq!(cleanup.metrics_snapshot().pending, 1);

        cluster.leave_release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().pending != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out cleanup was not retried");
        assert_eq!(cluster.leave_calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cluster_cleanup_retries_after_service_drop() {
        let cluster = Arc::new(IdempotentLeaveCluster::default());
        cluster.fail_leaves.store(true, Ordering::Release);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let cleanup = Arc::clone(&service.inner.cleanup);
        cleanup.schedule(
            Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>,
            Uuid::new_v4(),
            [RoomKey::new(Uuid::new_v4(), "lobby")],
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().failures == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup failure was not recorded");

        drop(service);
        cluster.fail_leaves.store(false, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanup.metrics_snapshot().pending != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup was cancelled when the service dropped");
        assert_eq!(cluster.leave_calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_cleanup_concurrency_can_follow_cluster_startup() {
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>)
        .with_cluster_cleanup_concurrency(NonZeroUsize::new(3).unwrap());

        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cluster worker did not start");
        assert_eq!(service.inner.cleanup.concurrency.load(Ordering::Acquire), 3);
    }

    #[test]
    fn transaction_matcher_is_exact_and_cardinality_one_only() {
        let attribute_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let mut attribute = predicate_attribute(attribute_id, "title", None);
        let left = vec![json!([
            "add-triple",
            entity_id,
            attribute_id,
            "old",
            {"mode": "upsert"}
        ])];
        let right = vec![json!([
            "add-triple",
            entity_id,
            attribute_id,
            "new",
            {"mode": "upsert"}
        ])];
        let catalog = AttributeCatalog {
            attributes: vec![attribute.clone()],
        };
        assert!(matching_transaction_steps(&left, &right, &catalog));

        attribute.cardinality = Cardinality::Many;
        assert!(!matching_transaction_steps(
            &left,
            &right,
            &AttributeCatalog {
                attributes: vec![attribute],
            }
        ));
        assert!(!matching_transaction_steps(
            &left,
            &[json!(["retract-triple", entity_id, attribute_id, "new", {"mode": "upsert"}])],
            &catalog,
        ));
        assert!(!matching_transaction_steps(
            &left,
            &[json!(["add-triple", entity_id, attribute_id, "new"])],
            &catalog,
        ));
        assert!(!matching_transaction_steps(
            &left,
            &[json!(["add-triple", Uuid::new_v4(), attribute_id, "new", {"mode": "upsert"}])],
            &catalog,
        ));
        assert!(!matching_transaction_steps(
            &left,
            &[json!(["add-triple", entity_id, Uuid::new_v4(), "new", {"mode": "upsert"}])],
            &catalog,
        ));
        assert!(!matching_transaction_steps(
            &left,
            &[json!(["add-triple", entity_id, attribute_id, "new", {"mode": "update"}])],
            &catalog,
        ));
        assert!(!matching_transaction_steps(
            &[json!(["add-triple", entity_id, "not-a-uuid", "old", {"mode": "upsert"}])],
            &[json!(["add-triple", entity_id, "not-a-uuid", "new", {"mode": "upsert"}])],
            &catalog,
        ));
        assert!(!matching_transaction_steps(
            &left,
            &[right[0].clone(), right[0].clone()],
            &catalog,
        ));
    }

    #[tokio::test]
    async fn queued_transactions_execute_first_then_combine_pending_newest_without_hol() {
        let attribute_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.transaction_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(AttributeCatalog {
                attributes: vec![predicate_attribute(attribute_id, "title", None)],
            })),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::ZERO);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        let event = |client: &str, value: &str| {
            json!({
                "op": "transact",
                "client-event-id": client,
                "tx-steps": [["add-triple", entity_id, attribute_id, value, {"mode": "upsert"}]],
            })
        };
        for (client, value) in [("a", "value-a"), ("b", "value-b"), ("c", "value-c")] {
            service
                .message(session.session_id, event(client, value))
                .await
                .unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while data.transaction_calls.lock().unwrap().len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // Unrelated receive work remains responsive while the DB transaction is blocked.
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "client-event-id": "query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["client-event-id"], "query");

        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.transaction_calls.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let calls = data.transaction_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][0][3], "value-a");
        assert_eq!(calls[1][0][3], "value-c");

        gate.add_permits(1);
        let mut transactions = HashMap::new();
        for _ in 0..3 {
            let response = session.recv().await.unwrap();
            transactions.insert(
                response["client-event-id"].as_str().unwrap().to_owned(),
                response["tx-id"].as_i64().unwrap(),
            );
        }
        assert_eq!(transactions["a"], 42);
        assert_eq!(transactions["b"], 43);
        assert_eq!(transactions["c"], 43);
    }

    #[tokio::test]
    async fn combined_transaction_errors_are_shaped_for_every_original_event() {
        let attribute_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.transaction_gate.lock().unwrap() = Some(Arc::clone(&gate));
        *data.transaction_error.lock().unwrap() =
            Some(ApiError::new(ApiErrorType::ValidationFailed, "test boom"));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(AttributeCatalog {
                attributes: vec![predicate_attribute(attribute_id, "title", None)],
            })),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        let event = |client: &str, value: &str| {
            json!({
                "op": "transact",
                "client-event-id": client,
                "tx-steps": [["add-triple", entity_id, attribute_id, value, {"mode": "upsert"}]],
            })
        };
        for (client, value) in [("a", "value-a"), ("b", "value-b"), ("c", "value-c")] {
            service
                .message(session.session_id, event(client, value))
                .await
                .unwrap();
        }
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.transaction_calls.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.add_permits(1);

        let mut originals = HashMap::new();
        for _ in 0..3 {
            let error = session.recv().await.unwrap();
            assert_eq!(error["op"], "error");
            assert_eq!(error["type"], "validation-failed");
            assert_eq!(error["message"], "test boom");
            let client = error["client-event-id"].as_str().unwrap().to_owned();
            assert_eq!(error["original-event"]["client-event-id"], client);
            originals.insert(client, error["original-event"]["tx-steps"][0][3].clone());
        }
        assert_eq!(originals["a"], "value-a");
        assert_eq!(originals["b"], "value-b");
        assert_eq!(originals["c"], "value-c");
    }

    #[tokio::test]
    async fn combined_transaction_timeout_is_fanned_out_to_every_original_event() {
        let attribute_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        *data.transaction_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(AttributeCatalog {
                attributes: vec![predicate_attribute(attribute_id, "title", None)],
            })),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_receive_timeout_policy(Arc::new(StaticReceiveTimeout(Duration::from_millis(10))));
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        for (client, value) in [("a", "value-a"), ("b", "value-b"), ("c", "value-c")] {
            service
                .message(
                    session.session_id,
                    json!({
                        "op": "transact",
                        "client-event-id": client,
                        "tx-steps": [["add-triple", entity_id, attribute_id, value, {"mode": "upsert"}]],
                    }),
                )
                .await
                .unwrap();
        }
        let mut ids = BTreeSet::new();
        for _ in 0..3 {
            let error = session.recv().await.unwrap();
            assert_eq!(error["type"], "operation-timed-out");
            assert_eq!(error["hint"], json!({"timeout-ms": 10}));
            assert_eq!(
                error["original-event"]["client-event-id"],
                error["client-event-id"]
            );
            ids.insert(error["client-event-id"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            ids,
            BTreeSet::from(["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
        assert_eq!(data.transaction_calls.lock().unwrap().len(), 2);
        assert!(
            sessions(&service.inner)[&session.session_id]
                .active_handlers
                .handlers
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn finished_transaction_results_map_to_one_metric_outcome() {
        assert_eq!(
            transaction_outcome(&ScopedHandlerOutcome::Completed(Ok(json!({
                "op": "transact-ok"
            })))),
            RuntimeOpOutcome::Ok
        );
        assert_eq!(
            transaction_outcome(&ScopedHandlerOutcome::Completed(Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "test boom"
            )))),
            RuntimeOpOutcome::Error
        );
        assert_eq!(
            transaction_outcome(&ScopedHandlerOutcome::Completed(Err(ApiError::new(
                ApiErrorType::RateLimited,
                "queue is full"
            )))),
            RuntimeOpOutcome::RateLimited
        );
        assert_eq!(
            transaction_outcome(&ScopedHandlerOutcome::TimedOut),
            RuntimeOpOutcome::Error
        );
        assert_eq!(
            transaction_outcome(&ScopedHandlerOutcome::Disconnected),
            RuntimeOpOutcome::Error
        );
    }

    async fn transact_metric_outcomes() -> Vec<RuntimeOpOutcome> {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let outcomes = crate::metrics::runtime_op_test_records()
                    .into_iter()
                    .filter(|(op, _)| op.as_deref() == Some("transact"))
                    .map(|(_, outcome)| outcome)
                    .collect::<Vec<_>>();
                if !outcomes.is_empty() {
                    break outcomes;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn failed_queued_transaction_is_recorded_once_as_error() {
        let data = Arc::new(TestData::default());
        *data.transaction_error.lock().unwrap() =
            Some(ApiError::new(ApiErrorType::ValidationFailed, "test boom"));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::ZERO);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        crate::metrics::clear_runtime_op_test_records();
        service
            .message(
                session.session_id,
                json!({
                    "op": "transact",
                    "client-event-id": "tx",
                    "tx-steps": [[
                        "add-triple",
                        Uuid::new_v4(),
                        Uuid::new_v4(),
                        "value",
                        {"mode": "upsert"},
                    ]],
                }),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "error");
        assert_eq!(
            transact_metric_outcomes().await,
            vec![RuntimeOpOutcome::Error]
        );
    }

    #[tokio::test]
    async fn successful_queued_transaction_is_recorded_once_as_ok() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::ZERO);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        crate::metrics::clear_runtime_op_test_records();
        service
            .message(
                session.session_id,
                json!({"op": "transact", "client-event-id": "tx", "tx-steps": []}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "transact-ok");
        assert_eq!(transact_metric_outcomes().await, vec![RuntimeOpOutcome::Ok]);
    }

    #[tokio::test]
    async fn transaction_rejected_before_the_queue_is_recorded_once() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::ZERO);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        crate::metrics::clear_runtime_op_test_records();
        service
            .message(
                session.session_id,
                json!({"op": "transact", "client-event-id": "tx", "tx-steps": []}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "error");
        assert_eq!(
            transact_metric_outcomes().await,
            vec![RuntimeOpOutcome::Error]
        );
    }

    #[tokio::test]
    async fn sse_transaction_batch_acks_are_delivered_on_the_session_stream() {
        let attribute_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.transaction_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(AttributeCatalog {
                attributes: vec![predicate_attribute(attribute_id, "title", None)],
            })),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::ZERO);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::ServerSentEvents)
            .await
            .unwrap();
        let init = session.recv().await.unwrap();
        let token = Uuid::parse_str(init["sse-token"].as_str().unwrap()).unwrap();
        let event = |client: &str, value: &str| {
            json!({
                "op": "transact",
                "client-event-id": client,
                "tx-steps": [["add-triple", entity_id, attribute_id, value, {"mode": "upsert"}]],
            })
        };
        service
            .push(RuntimeSsePushRequest {
                app_id,
                machine_id: service.inner.machine_id,
                session_id: session.session_id,
                sse_token: token,
                messages: vec![
                    json!({"op": "init", "app-id": app_id}),
                    event("a", "value-a"),
                    event("b", "value-b"),
                    event("c", "value-c"),
                ],
            })
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.transaction_calls.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.add_permits(1);
        let mut ids = BTreeSet::new();
        for _ in 0..3 {
            let response = session.recv().await.unwrap();
            assert_eq!(response["op"], "transact-ok");
            ids.insert(response["client-event-id"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            ids,
            BTreeSet::from(["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
    }

    #[tokio::test]
    async fn session_cleanup_aborts_a_blocked_transaction_worker_and_discards_pending() {
        let data = Arc::new(TestData::default());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.transaction_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        for client in ["a", "b"] {
            service
                .message(
                    session.session_id,
                    json!({"op": "transact", "client-event-id": client, "tx-steps": []}),
                )
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.transaction_calls.lock().unwrap().len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let control = sessions(&service.inner)[&session.session_id]
            .transact_worker
            .clone();
        let abort = control
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .unwrap()
            .clone();
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.add_permits(2);
        tokio::task::yield_now().await;
        assert_eq!(data.transaction_calls.lock().unwrap().len(), 1);
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn process_wide_drain_closes_every_local_transport() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let first = service
            .open(Some(Uuid::new_v4()), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let second = service
            .open(Some(Uuid::new_v4()), RuntimeTransport::ServerSentEvents)
            .await
            .unwrap();
        assert_eq!(service.session_count(), 2);

        let drained = service
            .drain_all_connections(Duration::ZERO, Duration::from_secs(1))
            .await;

        assert_eq!(drained, 2);
        // The drain signals each transport instead of removing sessions the
        // transport still owns, so the client sees a 1012 close frame.
        assert!(first.drain_signal().is_draining());
        assert!(second.drain_signal().is_draining());
        assert!(service.drain_signal().is_draining());
        drop((first, second));
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn sessions_opened_during_a_drain_are_asked_to_close_immediately() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        assert_eq!(
            service
                .drain_all_connections(Duration::ZERO, Duration::ZERO)
                .await,
            0
        );

        let session = service
            .open(Some(Uuid::new_v4()), RuntimeTransport::WebSocket)
            .await
            .unwrap();

        assert!(session.drain_signal().is_draining());
    }

    #[tokio::test]
    async fn websocket_receive_timeout_uses_the_app_policy_and_exact_error_envelope() {
        let data = Arc::new(TestData::default());
        *data.query_delay.lock().unwrap() = Some(Duration::from_secs(60));
        let policy = Arc::new(TestReceiveTimeout {
            duration: Duration::from_millis(5),
            app_ids: Mutex::new(Vec::new()),
        });
        let service = RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data, Uuid::new_v4())
            .with_receive_timeout_policy(Arc::clone(&policy) as Arc<dyn ReceiveTimeoutPolicy>);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        let query = json!({
            "op": "add-query",
            "client-event-id": "slow-query",
            "q": {"goals": {}}
        });
        service
            .message(session.session_id, query.clone())
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["status"], 500);
        assert_eq!(error["type"], "operation-timed-out");
        assert_eq!(error["message"], "Operation timed out: handle-receive");
        assert_eq!(error["hint"], json!({"timeout-ms": 5}));
        assert_eq!(error["original-event"], query);
        assert_eq!(error["client-event-id"], "slow-query");
        assert_eq!(*policy.app_ids.lock().unwrap(), [None, Some(app_id)]);
        assert!(
            sessions(&service.inner)[&session.session_id]
                .active_handlers
                .handlers
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn combined_receive_limits_remain_per_call_and_custom_setters_restore_sequential_policies()
     {
        struct Combined(Mutex<Vec<Option<Uuid>>>);
        impl ReceiveHandlerTimeoutPolicy for Combined {
            fn handler_timeouts(&self, app_id: Option<Uuid>) -> ReceiveHandlerTimeoutsFuture<'_> {
                self.0.lock().unwrap().push(app_id);
                Box::pin(std::future::ready(Ok(ReceiveHandlerTimeouts {
                    timeout: Duration::from_millis(17),
                    cancel_wait: Duration::from_millis(19),
                })))
            }
        }
        let combined = Arc::new(Combined(Mutex::new(Vec::new())));
        let custom = Arc::new(TestReceiveTimeout {
            duration: Duration::from_millis(23),
            app_ids: Mutex::new(Vec::new()),
        });
        let app = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_receive_timeout_policy(Arc::clone(&custom) as Arc<dyn ReceiveTimeoutPolicy>)
        .with_receive_handler_timeout_policy(
            Arc::clone(&combined) as Arc<dyn ReceiveHandlerTimeoutPolicy>
        );
        assert_eq!(
            service
                .receive_handler_timeouts(None)
                .await
                .unwrap()
                .timeout,
            Duration::from_millis(17)
        );
        assert_eq!(
            service
                .clone()
                .receive_handler_timeouts(Some(app))
                .await
                .unwrap()
                .cancel_wait,
            Duration::from_millis(19)
        );
        assert_eq!(*combined.0.lock().unwrap(), [None, Some(app)]);
        assert!(custom.app_ids.lock().unwrap().is_empty());
        let service = service.with_statement_cancel_wait_policy(Arc::new(
            crate::StaticStatementCancelWait(Duration::from_millis(29)),
        ));
        assert_eq!(
            service.receive_handler_timeouts(Some(app)).await.unwrap(),
            ReceiveHandlerTimeouts {
                timeout: Duration::from_millis(23),
                cancel_wait: Duration::from_millis(29)
            }
        );
        let service = service
            .with_receive_handler_timeout_policy(combined.clone())
            .with_receive_timeout_policy(Arc::clone(&custom) as Arc<dyn ReceiveTimeoutPolicy>);
        assert_eq!(
            service
                .receive_handler_timeouts(None)
                .await
                .unwrap()
                .timeout,
            Duration::from_millis(23)
        );
        assert_eq!(*custom.app_ids.lock().unwrap(), [Some(app), None]);
        assert_eq!(combined.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn session_cleanup_signals_and_terminates_an_active_non_transaction_handler() {
        let data = Arc::new(TestData::default());
        *data.query_delay.lock().unwrap() = Some(Duration::from_secs(60));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_receive_timeout_policy(Arc::new(StaticReceiveTimeout(Duration::from_secs(60))))
        .with_statement_cancel_wait_policy(Arc::new(StaticStatementCancelWait(
            Duration::from_millis(10),
        )));
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        let handlers = Arc::clone(&sessions(&service.inner)[&session.session_id].active_handlers);
        let message_service = service.clone();
        let session_id = session.session_id;
        let handler = tokio::spawn(async move {
            message_service
                .message(session_id, json!({"op": "add-query", "q": {"goals": {}}}))
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handlers.handlers.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), handler)
            .await
            .expect("disconnect must terminate an active handler")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handlers.handlers.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect must drain the grouped receive worker");
        assert!(handlers.handlers.lock().unwrap().is_empty());
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn sse_disconnect_terminates_an_active_push_handler_with_no_late_delivery() {
        let data = Arc::new(TestData::default());
        *data.query_delay.lock().unwrap() = Some(Duration::from_secs(60));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_receive_timeout_policy(Arc::new(StaticReceiveTimeout(Duration::from_secs(60))))
        .with_statement_cancel_wait_policy(Arc::new(StaticStatementCancelWait(
            Duration::from_millis(10),
        )));
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::ServerSentEvents)
            .await
            .unwrap();
        let sse_init = session.recv().await.unwrap();
        let token = Uuid::parse_str(sse_init["sse-token"].as_str().unwrap()).unwrap();
        service
            .push(RuntimeSsePushRequest {
                app_id,
                machine_id: service.inner.machine_id,
                session_id: session.session_id,
                sse_token: token,
                messages: vec![json!({"op": "init", "app-id": app_id})],
            })
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        let handlers = Arc::clone(&sessions(&service.inner)[&session.session_id].active_handlers);
        let push_service = service.clone();
        let session_id = session.session_id;
        let push = tokio::spawn(async move {
            push_service
                .push(RuntimeSsePushRequest {
                    app_id,
                    machine_id: push_service.inner.machine_id,
                    session_id,
                    sse_token: token,
                    messages: vec![json!({"op": "add-query", "q": {"goals": {}}})],
                })
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handlers.handlers.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), push)
            .await
            .expect("SSE disconnect must terminate its active POST handler")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handlers.handlers.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("SSE disconnect must drain the grouped receive worker");
        assert!(handlers.handlers.lock().unwrap().is_empty());
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_policy_failure_prevents_handler_execution() {
        let data = Arc::new(TestData::default());
        let policy = Arc::new(TestStatementCancelWait::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_statement_cancel_wait_policy(
            Arc::clone(&policy) as Arc<dyn StatementCancelWaitPolicy>
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        policy.fail.store(true, Ordering::Release);
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "unknown");
        assert_eq!(error["message"], "invalid statement cancellation policy");
        assert_eq!(data.query_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn sse_push_applies_the_same_receive_timeout_per_message() {
        let data = Arc::new(TestData::default());
        *data.query_delay.lock().unwrap() = Some(Duration::from_secs(60));
        let service = RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data, Uuid::new_v4())
            .with_receive_timeout_policy(Arc::new(StaticReceiveTimeout(Duration::from_millis(5))));
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::ServerSentEvents)
            .await
            .unwrap();
        let sse_init = session.recv().await.unwrap();
        let token = Uuid::parse_str(sse_init["sse-token"].as_str().unwrap()).unwrap();
        service
            .push(RuntimeSsePushRequest {
                app_id,
                machine_id: service.inner.machine_id,
                session_id: session.session_id,
                sse_token: token,
                messages: vec![
                    json!({"op": "init", "app-id": app_id}),
                    json!({"op": "add-query", "q": {"goals": {}}}),
                ],
            })
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        assert_eq!(session.recv().await.unwrap()["type"], "operation-timed-out");
    }

    #[tokio::test]
    async fn per_app_cache_uses_one_policy_snapshot_and_respects_its_weight_limit() {
        let data = Arc::new(TestData::default());
        let policy = Arc::new(TestDatalogCachePolicy {
            calls: std::sync::atomic::AtomicUsize::new(0),
            settings: DatalogCacheSettings {
                max_weight: 1,
                ttl: Duration::from_secs(60),
                size_multiple: 1,
                process_max_in_flight: 0,
                max_in_flight: 0,
            },
        });
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_datalog_cache_policy(Arc::clone(&policy) as Arc<dyn DatalogCachePolicy>);
        let app_id = Uuid::new_v4();
        let mut first = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut second = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session in [&mut first, &mut second] {
            service
                .message(session.session_id, json!({"op": "init", "app-id": app_id}))
                .await
                .unwrap();
            session.recv().await.unwrap();
            service
                .message(
                    session.session_id,
                    json!({"op": "add-query", "q": {"goals": {}}}),
                )
                .await
                .unwrap();
            assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");
        }

        assert_eq!(policy.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            data.query_count.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "the oversized result must be shared while loading but not retained"
        );
    }

    #[test]
    fn cache_result_weight_preserves_encoded_size_and_saturation() {
        for value in [
            Value::Null,
            json!({"escaped\"key": ["\u{0}\\\n", "é🦀", true, false, -42, 1.25]}),
            serde_json::from_str("1234567890123456789012345678901234567890").unwrap(),
        ] {
            let result = SessionQueryResult {
                result: Arc::new(value),
                result_meta: Arc::new(json!({"page-info": {"end-cursor": "\t\"é"}})),
                byop_topics: None,
                instant_topics: None,
            };
            let previous_bytes = (serde_json::to_vec(&result.result).unwrap().len()
                + serde_json::to_vec(&result.result_meta).unwrap().len())
                as u64;
            for multiple in [0, 1, 8, u64::MAX] {
                assert_eq!(
                    session_query_result_weight(&result, multiple),
                    previous_bytes.saturating_mul(multiple)
                );
            }
        }
    }

    #[test]
    fn streaming_catalog_fingerprint_matches_exact_wire_bytes() {
        for attrs in [
            json!([]),
            json!([{"id": "é\"\\\n", "metadata": [null, false, 1.25, "🦀"]}]),
            serde_json::from_str("[1234567890123456789012345678901234567890]").unwrap(),
            json!(
                (0..1000)
                    .map(|n| json!({"id": n, "label": "field"}))
                    .collect::<Vec<_>>()
            ),
        ] {
            let previous: [u8; 32] = Sha256::digest(serde_json::to_vec(&attrs).unwrap()).into();
            assert_eq!(realtime_catalog_digest(&attrs), previous);
        }
        assert_ne!(
            realtime_catalog_digest(&json!([1, 2])),
            realtime_catalog_digest(&json!([2, 1]))
        );
        let mut catalog = AttributeCatalog {
            attributes: vec![predicate_attribute(Uuid::new_v4(), "escaped\"\\é", None)],
        };
        let borrowed = |catalog: &AttributeCatalog| {
            realtime_catalog_digest(&crate::attribute_wire::FrameworkCatalog(catalog))
        };
        assert_eq!(
            borrowed(&catalog),
            realtime_catalog_digest(&realtime_catalog_attrs(&catalog))
        );
        let first = borrowed(&catalog);
        catalog.attributes[0].label.push_str("changed");
        assert_ne!(first, borrowed(&catalog));
        assert_eq!(
            borrowed(&catalog),
            realtime_catalog_digest(&realtime_catalog_attrs(&catalog))
        );
    }

    #[test]
    fn shared_cache_key_text_is_immutable_and_clones_without_copying_buffers() {
        use std::hash::{Hash, Hasher};
        let key = SharedQueryKey {
            actor: SharedQueryActor::Email("person@example.invalid".into()),
            query: "query".repeat(1024).into(),
            return_type: QueryReturnType::JoinRows,
            versions: "{\"core\":\"1.0\"}".into(),
        };
        let copy = key.clone();
        assert!(Arc::ptr_eq(&key.query, &copy.query));
        assert!(Arc::ptr_eq(&key.versions, &copy.versions));
        let separate = SharedQueryKey {
            query: key.query.to_string().into(),
            versions: key.versions.to_string().into(),
            ..copy
        };
        assert_eq!(key, separate);
        let hash = |key: &SharedQueryKey| {
            let mut state = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut state);
            state.finish()
        };
        assert_eq!(hash(&key), hash(&separate));
        assert_ne!(
            key,
            SharedQueryKey {
                actor: SharedQueryActor::Guest,
                ..separate
            }
        );
    }

    #[tokio::test]
    async fn session_headers_and_detached_contexts_do_not_retain_subscription_payloads() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data, Uuid::new_v4());
        let app = Uuid::new_v4();
        let session = open_test_query_session(&service, app).await;
        let payload = Arc::new(json!({"large": "x".repeat(128 * 1024)}));
        {
            let mut entries = sessions(&service.inner);
            let query = entries
                .get_mut(&session.session_id)
                .unwrap()
                .queries
                .values_mut()
                .next()
                .unwrap();
            query.last_result.result = Arc::clone(&payload);
        }
        let owners = Arc::strong_count(&payload);
        let entry = initialized_entry(&service.inner, session.session_id).unwrap();
        let clone = entry.clone();
        let channels = Arc::strong_count(&entry.sender.state);
        let context = SessionQueryContext::from_entry(&clone);
        assert_eq!(
            Arc::strong_count(&payload),
            owners,
            "header/context snapshots must not retain any query result"
        );
        assert_eq!(
            Arc::strong_count(&entry.sender.state),
            channels,
            "loader context must not keep outbound delivery alive"
        );
        assert_eq!(context.request.app_id, app);
        assert_eq!(context.request.actor, entry.actor);
        assert_eq!(context.request.bearer_token, entry.bearer_token);
        assert_eq!(context.versions, entry.versions);
    }

    #[tokio::test]
    async fn cached_query_results_share_payloads_but_compare_and_replace_by_value() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app = Uuid::new_v4();
        let session = open_test_query_session(&service, app).await;
        let entry = initialized_entry(&service.inner, session.session_id).unwrap();
        let query = json!({"goals": {}}).as_object().unwrap().clone();
        let first = service
            .execute_session_query(&entry, query.clone(), QueryReturnType::JoinRows)
            .await
            .unwrap();
        let second = service
            .execute_session_query(&entry, query, QueryReturnType::JoinRows)
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&first.result, &second.result),
            "cache hits must not deep-copy payloads"
        );
        assert!(Arc::ptr_eq(&first.result_meta, &second.result_meta));
        let snapshot = CachedQueryResult::from_execution(&first);
        let copy = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot.result, &copy.result));
        let equal_separate = CachedQueryResult {
            result: Arc::new((*snapshot.result).clone()),
            result_meta: Arc::new((*snapshot.result_meta).clone()),
        };
        assert_eq!(
            snapshot, equal_separate,
            "pointer identity is not result equality"
        );
        let mut current = snapshot.clone();
        current.result = Arc::new(json!({"replacement": true}));
        assert_ne!(current, copy);
        assert_eq!(
            copy, snapshot,
            "replacing current results must leave old generations immutable"
        );
        assert_eq!(serde_json::to_value(&first.result).unwrap(), *first.result);
    }

    #[tokio::test]
    async fn cache_cleanup_retains_only_unreachable_payloads_after_the_epoch_fence() {
        for security in [false, true] {
            let service = RuntimeRealtimeService::new(
                Arc::new(TestAuthorizer),
                Arc::new(TestData::default()),
                Uuid::new_v4(),
            );
            let app = Uuid::new_v4();
            let cache = RuntimeQueryCache::default();
            query_caches(&service.inner).insert(app, cache.clone());
            let before = query_epoch(&service.inner, app);
            let key = SharedQueryKey {
                actor: SharedQueryActor::Guest,
                query: "{}".into(),
                return_type: QueryReturnType::JoinRows,
                versions: "{}".into(),
            };
            let payload = Arc::new(json!({"fresh": false}));
            let weak = Arc::downgrade(&payload);
            drop(
                cache
                    .get_or_load(key.clone(), move || async move {
                        Ok(SessionQueryResult {
                            result: payload,
                            result_meta: Arc::new(Value::Null),
                            byop_topics: None,
                            instant_topics: None,
                        })
                    })
                    .await
                    .unwrap(),
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                while cache.metrics_snapshot().in_flight != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let cleanup;
            {
                let _entries = sessions(&service.inner);
                cleanup = invalidate_app_query_reuse(&service.inner, app, security, None);
                let after = query_epoch(&service.inner, app);
                assert_eq!(after.data, before.data + 1);
                assert_eq!(after.security, before.security + u64::from(security));
                assert_eq!(cache.metrics_snapshot().entries, 0);
                assert_eq!(cache.metrics_snapshot().ready_weight, 0);
                assert!(
                    weak.upgrade().is_some(),
                    "old payload is unreachable but not destroyed under the registry lock"
                );
            }
            assert!(!cache.contains(&key));
            drop(cleanup);
            assert!(weak.upgrade().is_none());
        }
    }

    #[tokio::test]
    async fn root_frame_cache_retires_and_clears_with_its_own_epoch() {
        for security in [false, true] {
            let frames = Arc::new(crate::RootFrameCaches::new(Arc::new(
                StaticDatalogCachePolicy::default(),
            )));
            let service = RuntimeRealtimeService::new(
                Arc::new(TestAuthorizer),
                Arc::new(TestData::default()),
                Uuid::new_v4(),
            )
            .with_root_frame_caches(Arc::clone(&frames));
            let app = Uuid::new_v4();
            let cache = frames.cache(app).await.unwrap();
            let key = crate::root_frame_cache::RootFrameKey::synthetic(app, 1);
            cache
                .get_or_load(key.clone(), || async {
                    Ok(crate::admin_data::root_frames::RootFrame::default())
                })
                .await
                .unwrap();
            assert!(cache.contains(&key));
            let before = frame_epochs(&service.inner).get_or_create(app).current();
            // Frames are retired before the session lock, never under it.
            let root_frames = invalidate_app_root_frames(&service.inner, app, security, None);
            let cleanup;
            {
                let _entries = sessions(&service.inner);
                cleanup = invalidate_app_query_reuse(&service.inner, app, security, None);
            }
            let after = frame_epochs(&service.inner).get(app).unwrap().current();
            assert_eq!(after.data, before.data + 1);
            assert_eq!(after.security, before.security + u64::from(security));
            assert!(!cache.contains(&key));
            assert_eq!(frames.stats(app).entries, 0);
            drop(cleanup);
            drop(root_frames);
        }
    }

    #[tokio::test]
    async fn invalidation_without_a_reader_does_not_create_an_epoch() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app = Uuid::new_v4();
        assert!(query_epochs(&service.inner).get(app).is_none());
        let cleanup;
        {
            let _entries = sessions(&service.inner);
            cleanup = invalidate_app_query_reuse(&service.inner, app, false, None);
        }
        drop(cleanup);
        assert!(
            query_epochs(&service.inner).get(app).is_none(),
            "WAL traffic without a session-path reader must not retain an epoch"
        );

        let before = query_epoch(&service.inner, app);
        assert_eq!((before.data, before.security), (0, 0));
        assert!(query_epochs(&service.inner).get(app).is_some());
        let cleanup;
        {
            let _entries = sessions(&service.inner);
            cleanup = invalidate_app_query_reuse(&service.inner, app, false, None);
            let after = query_epoch(&service.inner, app);
            assert_eq!(
                (after.data, after.security),
                (before.data + 1, before.security)
            );
        }
        drop(cleanup);
    }

    #[tokio::test]
    async fn root_frame_retirement_follows_wal_topics_and_fences_in_flight_loads() {
        use instant_datalog::IndexKey;
        use instant_realtime::{QueryTopic, QueryTopicPart, QueryValue};
        use instant_wal::{MatchSet, Topic};

        let frames = Arc::new(crate::RootFrameCaches::new(Arc::new(
            StaticDatalogCachePolicy::default(),
        )));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_root_frame_caches(Arc::clone(&frames));
        let app = Uuid::new_v4();
        let cache = frames.cache(app).await.unwrap();

        fn topic(attribute: Uuid) -> QueryTopic {
            QueryTopic {
                indexes: vec![IndexKey::Ea],
                entities: QueryTopicPart::Any,
                attributes: QueryTopicPart::Values(vec![QueryValue::Uuid(attribute)]),
                values: QueryTopicPart::Any,
            }
        }
        let frame = |attribute: Uuid| crate::admin_data::root_frames::RootFrame {
            nodes: Vec::new(),
            entity_data: HashMap::new(),
            topics: Some(crate::admin_data::root_frames::FrameTopics::new(vec![
                topic(attribute),
            ])),
        };
        let key = |tag: u64| crate::root_frame_cache::RootFrameKey::synthetic(app, tag);
        for (tag, attribute) in [(1, 1), (2, 2)] {
            cache
                .get_or_load(key(tag), move || async move {
                    Ok(frame(Uuid::from_u128(attribute)))
                })
                .await
                .unwrap();
        }
        // Frames whose topics could not be computed retire on any data WAL.
        let uncomputable = key(3);
        cache
            .get_or_load(uncomputable.clone(), || async {
                Ok(crate::admin_data::root_frames::RootFrame::default())
            })
            .await
            .unwrap();

        let routing: Arc<[instant_wal::Topic]> = Arc::from(vec![Topic {
            indexes: vec![IndexKey::Ea],
            entities: MatchSet::Any,
            attributes: MatchSet::Values(vec![Uuid::from_u128(2)]),
            values: MatchSet::Any,
        }]);
        let cleanup =
            invalidate_app_root_frames(&service.inner, app, false, Some(Arc::clone(&routing)));
        assert!(
            cache.contains(&key(1)),
            "a frame whose topics do not intersect the WAL keeps reuse"
        );
        assert!(!cache.contains(&key(2)));
        assert!(!cache.contains(&uncomputable));
        drop(cleanup);

        // A load already in flight when an unrelated WAL applies may still
        // publish: the epoch proves the WAL cannot have changed its frame.
        let in_flight = key(4);
        let (started, started_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let in_flight = in_flight.clone();
            async move {
                cache
                    .get_or_load(in_flight, move || async move {
                        started.send(()).unwrap();
                        release_receiver.await.unwrap();
                        Ok(frame(Uuid::from_u128(1)))
                    })
                    .await
            }
        });
        started_receiver.await.unwrap();
        let cleanup =
            invalidate_app_root_frames(&service.inner, app, false, Some(Arc::clone(&routing)));
        release.send(()).unwrap();
        assert!(
            waiter.await.unwrap().is_ok(),
            "existing waiters still finish"
        );
        assert!(
            cache.contains(&in_flight),
            "an unrelated WAL lets an in-flight load publish"
        );
        drop(cleanup);

        // The same load with an intersecting WAL is delivered to its waiters but
        // never becomes reusable.
        let intersecting = key(5);
        let (started, started_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let intersecting = intersecting.clone();
            async move {
                cache
                    .get_or_load(intersecting, move || async move {
                        started.send(()).unwrap();
                        release_receiver.await.unwrap();
                        Ok(frame(Uuid::from_u128(2)))
                    })
                    .await
            }
        });
        started_receiver.await.unwrap();
        let cleanup = invalidate_app_root_frames(&service.inner, app, false, Some(routing));
        release.send(()).unwrap();
        assert!(waiter.await.unwrap().is_ok());
        assert!(
            !cache.contains(&intersecting),
            "an intersecting WAL blocks publication"
        );
        drop(cleanup);
    }

    #[tokio::test]
    async fn root_frame_retirement_runs_before_the_session_registry_lock() {
        let frames = Arc::new(crate::RootFrameCaches::new(Arc::new(
            StaticDatalogCachePolicy::default(),
        )));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_root_frame_caches(Arc::clone(&frames));
        let app = Uuid::new_v4();
        let cache = frames.cache(app).await.unwrap();
        cache
            .get_or_load(
                crate::root_frame_cache::RootFrameKey::synthetic(app, 1),
                || async { Ok(crate::admin_data::root_frames::RootFrame::default()) },
            )
            .await
            .unwrap();
        let session = open_test_query_session(&service, app).await;

        let unlocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = Arc::clone(&service.inner);
        frames.set_retire_probe(Some(Arc::new({
            let unlocked = Arc::clone(&unlocked);
            move || {
                unlocked.store(inner.sessions.try_lock().is_ok(), Ordering::Relaxed);
            }
        })));
        let record = attribute_wal_record(app, Uuid::new_v4());
        let selected = service
            .prepare_wal_record_for_sessions(&record, None, false, &mut BTreeMap::new())
            .await
            .unwrap();
        frames.set_retire_probe(None);

        assert!(selected.contains(&session.session_id));
        assert!(
            unlocked.load(Ordering::Relaxed),
            "the frame scan must not hold the session registry"
        );
    }

    #[tokio::test]
    async fn wal_selection_retires_frames_it_can_affect_before_returning_refresh_sessions() {
        use instant_datalog::IndexKey;
        use instant_realtime::{QueryTopic, QueryTopicPart, QueryValue};

        let frames = Arc::new(crate::RootFrameCaches::new(Arc::new(
            StaticDatalogCachePolicy::default(),
        )));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_root_frame_caches(Arc::clone(&frames));
        let app = Uuid::new_v4();
        let cache = frames.cache(app).await.unwrap();
        let attribute = Uuid::new_v4();
        let key = crate::root_frame_cache::RootFrameKey::synthetic(app, 1);
        let frame = crate::admin_data::root_frames::RootFrame {
            nodes: Vec::new(),
            entity_data: HashMap::new(),
            topics: Some(crate::admin_data::root_frames::FrameTopics::new(vec![
                QueryTopic {
                    indexes: vec![IndexKey::Ea],
                    entities: QueryTopicPart::Any,
                    attributes: QueryTopicPart::Values(vec![QueryValue::Uuid(attribute)]),
                    values: QueryTopicPart::Any,
                },
            ])),
        };
        cache
            .get_or_load(key.clone(), move || async move { Ok(frame) })
            .await
            .unwrap();
        let session = open_test_query_session(&service, app).await;

        let record = attribute_wal_record(app, attribute);
        let selected = service
            .prepare_wal_record_for_sessions(&record, None, false, &mut BTreeMap::new())
            .await
            .unwrap();

        assert!(selected.contains(&session.session_id));
        assert!(
            !cache.contains(&key),
            "a selected refresh must not read a frame its WAL affects"
        );
    }

    #[tokio::test]
    async fn frame_and_session_result_epoch_registries_are_independent() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app = Uuid::new_v4();
        assert!(frame_epochs(&service.inner).get(app).is_none());
        assert!(query_epochs(&service.inner).get(app).is_none());

        // Frame invalidation must not create or record the session result epoch.
        drop(invalidate_app_root_frames(&service.inner, app, false, None));
        assert!(frame_epochs(&service.inner).get(app).is_none());
        assert!(query_epochs(&service.inner).get(app).is_none());

        let frame_epoch = frame_epochs(&service.inner).get_or_create(app);
        let before_frame = frame_epoch.current();
        let before_query = query_epoch(&service.inner, app);
        drop(invalidate_app_query_reuse(&service.inner, app, false, None));
        assert_eq!(frame_epoch.current().data, before_frame.data);
        let after_query = query_epoch(&service.inner, app);
        assert_eq!(after_query.data, before_query.data + 1);

        // Frame invalidation must not record the session result epoch.
        drop(invalidate_app_root_frames(&service.inner, app, true, None));
        let query_after_frame_invalidation = query_epoch(&service.inner, app);
        assert_eq!(
            (
                query_after_frame_invalidation.data,
                query_after_frame_invalidation.security
            ),
            (after_query.data, after_query.security)
        );
        let after_frame = frame_epoch.current();
        assert_eq!(after_frame.data, before_frame.data + 1);
        assert_eq!(after_frame.security, before_frame.security + 1);
    }

    #[tokio::test]
    async fn root_frame_metrics_report_real_values() {
        let frames = Arc::new(crate::RootFrameCaches::new(Arc::new(
            StaticDatalogCachePolicy::default(),
        )));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_root_frame_caches(Arc::clone(&frames));
        let app = Uuid::new_v4();
        let cache = frames.cache(app).await.unwrap();
        let counters = frames.counters(app);
        let key = crate::root_frame_cache::RootFrameKey::synthetic(app, 1);

        counters.miss();
        cache
            .get_or_load(key.clone(), {
                let counters = Arc::clone(&counters);
                move || async move {
                    counters.load();
                    Ok(crate::root_frame_cache::RootFrame::default())
                }
            })
            .await
            .unwrap();
        counters.hit();
        cache
            .get_or_load(key.clone(), || async {
                Ok(crate::root_frame_cache::RootFrame::default())
            })
            .await
            .unwrap();

        let metrics = service.metrics_snapshot();
        assert_eq!(metrics.root_frame_cache_entries, 1);
        assert_eq!(metrics.root_frame_cache_hits_total, 1);
        assert_eq!(metrics.root_frame_cache_misses_total, 1);
        assert_eq!(metrics.root_frame_cache_loads_total, 1);

        frames.retire(app);
        let metrics = service.metrics_snapshot();
        assert_eq!(metrics.root_frame_cache_entries, 0);
        assert!(metrics.root_frame_cache_retire_scanned_total >= 1);
        assert!(metrics.root_frame_cache_retire_retired_total >= 1);
    }

    #[tokio::test]
    async fn realtime_metrics_follow_cache_and_session_lifetimes() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        assert_eq!(
            service.metrics_snapshot(),
            RuntimeRealtimeMetrics::default()
        );
        let app_id = Uuid::new_v4();
        let session = open_test_query_session(&service, app_id).await;
        let snapshot = service.metrics_snapshot();
        assert_eq!(snapshot.sessions, 1);
        assert_eq!(
            snapshot.sdk_sessions,
            vec![(
                "other".to_owned(),
                instant_realtime::SDK_VERSION_OTHER.to_owned(),
                1
            )]
        );
        assert_eq!(snapshot.subscriptions, 1);
        assert_eq!(snapshot.cache_apps, 1);
        assert_eq!(snapshot.cache_entries, 1);
        assert!(snapshot.cache_weight_bytes > 0);
        assert_eq!(snapshot.result_cache_bytes, snapshot.cache_weight_bytes);
        assert!(snapshot.result_cache_max_bytes > 0);
        assert_eq!(snapshot.result_cache_evictions_total, 0);
        assert_eq!(snapshot.cache_in_flight, 0);
        assert_eq!(snapshot.cache_retired_loads, 0);
        assert_eq!(snapshot.query_admission_rejections_total, 0);
        assert_eq!(snapshot.transaction_admission_rejections_total, 0);
        // Sampling must be read-only; a scrape cannot clear accumulated state.
        assert_eq!(service.metrics_snapshot(), snapshot);
        clear_app_query_cache(&service.inner, app_id);
        let cleared = service.metrics_snapshot();
        assert_eq!(cleared.cache_entries, 0);
        assert_eq!(cleared.cache_weight_bytes, 0);
        assert_eq!(cleared.result_cache_bytes, 0);
        assert_eq!(cleared.subscriptions, 1);
        drop(session);
        let empty = service.metrics_snapshot();
        assert_eq!(empty.sessions, 0);
        assert_eq!(empty.cache_entries, 0);
        assert_eq!(empty.result_cache_bytes, 0);
        assert!(empty.result_cache_max_bytes > 0);
    }

    #[test]
    fn disabled_process_load_admission_does_not_create_a_shared_budget() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );

        assert!(query_cache_budget(&service.inner, DatalogCacheSettings::default()).is_none());
    }

    #[tokio::test]
    async fn add_query_reports_the_latest_monotonic_app_watermark() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();

        let newest = test_wal_record(app_id, Vec::new());
        service.apply_wal_record(&newest).await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"newest": {}}}),
            )
            .await
            .unwrap();
        let response = session.recv().await.unwrap();
        assert_eq!(response["processed-tx-id"], newest.tx_id);
        assert_eq!(response["processed-isn"], json!(newest.isn));

        service
            .message(
                session.session_id,
                json!({"op": "remove-query", "q": {"newest": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "remove-query-ok");

        let mut older = test_wal_record(app_id, Vec::new());
        older.tx_id = newest.tx_id - 1;
        older.isn = Isn::new(
            newest.isn.slot_num() - 1,
            instant_wal::Lsn::from_u64(u64::MAX),
        );
        service.apply_wal_record(&older).await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"stillNewest": {}}}),
            )
            .await
            .unwrap();
        let response = session.recv().await.unwrap();
        assert_eq!(response["processed-tx-id"], newest.tx_id);
        assert_eq!(response["processed-isn"], json!(newest.isn));
    }

    #[tokio::test]
    async fn add_query_does_not_retry_an_unknown_error() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        data.query_errors_remaining.store(1, Ordering::Relaxed);

        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .expect("add-query retry did not complete")
            .unwrap();
        assert_eq!(response["op"], "error");
        assert_eq!(response["type"], "unknown");
        assert_eq!(data.query_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn new_apps_inherit_shared_startup_and_wal_watermarks() {
        let initial_isn = Isn::new(2, instant_wal::Lsn::from_u64(50));
        let seed = Arc::new(RealtimeWatermarkSeed::initialized(70, initial_isn));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_watermark_seed(Arc::clone(&seed));

        let existing_app = Uuid::new_v4();
        assert_eq!(
            processed_watermark(&service.inner, existing_app),
            ProcessedWatermark {
                tx_id: Some(70),
                isn: Some(initial_isn),
            }
        );

        service.apply_cache_eviction(&WalCacheEviction::TransactionMaxSeen { tx_id: Some(71) });
        seed.observe_isn(Isn::new(2, instant_wal::Lsn::from_u64(51)));
        // Out-of-order observations cannot lower either process-global maximum.
        service.apply_cache_eviction(&WalCacheEviction::TransactionMaxSeen { tx_id: Some(69) });
        seed.observe_isn(initial_isn);

        assert_eq!(
            processed_watermark(&service.inner, existing_app),
            ProcessedWatermark {
                tx_id: Some(70),
                isn: Some(initial_isn),
            },
            "an existing app only advances from its own invalidations"
        );

        assert_eq!(
            processed_watermark(&service.inner, Uuid::new_v4()),
            ProcessedWatermark {
                tx_id: Some(71),
                isn: Some(Isn::new(2, instant_wal::Lsn::from_u64(51))),
            }
        );
    }

    #[tokio::test]
    async fn unchanged_wal_is_silent_but_schema_wal_forces_query_refresh() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data, Uuid::new_v4());
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret",
                    "versions": {"@instantdb/core": "0.20.5"}
                }),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();

        let record = test_wal_record(app_id, Vec::new());
        service.apply_wal_record(&record).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), session.recv())
                .await
                .is_err()
        );
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"latest": {}}}),
            )
            .await
            .unwrap();
        let latest = session.recv().await.unwrap();
        assert_eq!(latest["processed-tx-id"], record.tx_id);

        let mut schema_record = record;
        schema_record.tx_id += 1;
        schema_record.isn = Isn::new(8, instant_wal::Lsn::from_u64(0x43));
        schema_record
            .attr_changes
            .push(triple_entry(WalAction::Insert, Vec::new(), Vec::new()));
        service.apply_wal_record(&schema_record).await.unwrap();
        let refresh = session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(refresh["computations"].as_array().unwrap().len(), 2);
        assert_eq!(refresh["attrs"], json!([]));
    }

    #[tokio::test]
    async fn configured_noop_refresh_delays_the_next_session_refresh() {
        let data = Arc::new(TestData::default());
        let app_id = Uuid::new_v4();
        let delay = Duration::from_millis(40);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_refresh_throttle_policy(Arc::new(TestRefreshThrottle { app_id, delay }));
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret",
                    "versions": {"@instantdb/core": "0.20.5"}
                }),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();

        let first = test_wal_record(app_id, Vec::new());
        service.apply_wal_record(&first).await.unwrap();
        let mut second = first;
        second.tx_id += 1;
        second.isn = Isn::new(7, instant_wal::Lsn::from_u64(0x43));
        let started = tokio::time::Instant::now();
        service.apply_wal_record(&second).await.unwrap();
        assert!(started.elapsed() >= delay);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), session.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn refresh_dispatcher_keeps_in_flight_work_and_coalesces_pending_work() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();

        let initial_query_count = data.query_count.load(Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        data.query_version.store(1, Ordering::Relaxed);

        let first_record = refreshing_wal_record(app_id, 84, 7, 0x42);
        let first_service = service.clone();
        let first =
            tokio::spawn(async move { first_service.apply_wal_record(&first_record).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != initial_query_count + 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        data.query_version.store(2, Ordering::Relaxed);
        let second_record = refreshing_wal_record(app_id, 85, 8, 0x43);
        let third_record = refreshing_wal_record(app_id, 86, 9, 0x44);
        let second_service = service.clone();
        let second =
            tokio::spawn(async move { second_service.apply_wal_record(&second_record).await });
        let third_service = service.clone();
        let third =
            tokio::spawn(async move { third_service.apply_wal_record(&third_record).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let pending = REFRESH_INBOX_LIMIT
                    - sessions(&service.inner)
                        .get(&session.session_id)
                        .unwrap()
                        .refresh_sender
                        .capacity();
                if pending == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != initial_query_count + 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.add_permits(1);

        tokio::time::timeout(Duration::from_secs(1), async {
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();
            third.await.unwrap().unwrap();
        })
        .await
        .expect("all logical refresh callers must complete");
        assert_eq!(
            data.query_count.load(Ordering::Relaxed),
            initial_query_count + 2,
            "the two pending logical refreshes share one physical query"
        );
        let first_refresh = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_refresh["processed-tx-id"], 84);
        let combined_refresh = tokio::time::timeout(Duration::from_secs(1), session.recv())
            .await
            .expect("the combined pending refresh must emit its newest watermark")
            .unwrap();
        assert_eq!(combined_refresh["processed-tx-id"], 86);
        assert_eq!(combined_refresh["processed-isn"], "9/0/44");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), session.recv())
                .await
                .is_err(),
            "the discarded pending watermark must not produce its own frame"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn combined_refresh_fans_the_same_error_to_every_waiter() {
        let data = Arc::new(TestData::default());
        let policy = Arc::new(TestCoarseTopicsFailure::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_coarse_topics_policy(policy.clone());
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;
        let initial_query_count = data.query_count.load(Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        data.query_version.store(1, Ordering::Relaxed);

        let first_service = service.clone();
        let first = tokio::spawn(async move {
            first_service
                .apply_wal_record(&refreshing_wal_record(app_id, 84, 7, 0x42))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) != initial_query_count + 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let second_service = service.clone();
        let second = tokio::spawn(async move {
            second_service
                .apply_wal_record(&refreshing_wal_record(app_id, 85, 8, 0x43))
                .await
        });
        let third_service = service.clone();
        let third = tokio::spawn(async move {
            third_service
                .apply_wal_record(&refreshing_wal_record(app_id, 86, 9, 0x44))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let pending = REFRESH_INBOX_LIMIT
                    - sessions(&service.inner)
                        .get(&session.session_id)
                        .unwrap()
                        .refresh_sender
                        .capacity();
                if pending == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        policy.fail.store(true, Ordering::Release);
        gate.add_permits(1);

        first.await.unwrap().unwrap();
        let second_error = second.await.unwrap().unwrap_err();
        let third_error = third.await.unwrap().unwrap_err();
        assert_eq!(second_error.body().kind, ApiErrorType::Unknown);
        assert_eq!(second_error.body().message, "refresh policy failed");
        assert_eq!(third_error.body().kind, second_error.body().kind);
        assert_eq!(third_error.body().message, second_error.body().message);
        assert_eq!(session.recv().await.unwrap()["processed-tx-id"], 84);
    }

    #[tokio::test]
    async fn refresh_queue_is_bounded_and_session_cleanup_releases_all_waiters() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let session = open_test_query_session(&service, app_id).await;
        let session_id = session.session_id;
        let initial_query_count = data.query_count.load(Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(gate);

        let first_service = service.clone();
        let first = tokio::spawn(async move {
            first_service
                .apply_wal_record(&refreshing_wal_record(app_id, 84, 7, 0x42))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) < initial_query_count + 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let mut waiters = Vec::with_capacity(REFRESH_INBOX_LIMIT + 1);
        for tx_id in 0..=REFRESH_INBOX_LIMIT {
            let enqueue_service = service.clone();
            waiters.push(tokio::spawn(async move {
                enqueue_service
                    .enqueue_refresh(session_id, json!(tx_id), Value::Null)
                    .await
            }));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while sessions(&service.inner)
                .get(&session_id)
                .unwrap()
                .refresh_sender
                .capacity()
                != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(waiters.iter().any(|waiter| !waiter.is_finished()));
        assert_eq!(
            service.metrics_snapshot().refresh_queued,
            REFRESH_INBOX_LIMIT
        );

        drop(session);
        tokio::time::timeout(Duration::from_secs(1), async {
            first.await.unwrap().unwrap();
            for waiter in waiters {
                waiter.await.unwrap().unwrap();
            }
        })
        .await
        .expect("cleanup must abort the active refresh and release queued/backpressured callers");
        assert_eq!(service.session_count(), 0);
        assert_eq!(service.metrics_snapshot().refresh_queued, 0);
    }

    #[tokio::test]
    async fn wal_batch_refreshes_independent_sessions_before_slow_query_finishes() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let slow_app = Uuid::new_v4();
        let fast_app = Uuid::new_v4();
        let _slow_session = open_test_query_session(&service, slow_app).await;
        let mut fast_session = open_test_query_session(&service, fast_app).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        *data.query_gate_app_id.lock().unwrap() = Some(slow_app);
        data.query_version.store(1, Ordering::Relaxed);
        let runner = service.clone();
        let batch = tokio::spawn(async move {
            deliver_cluster_delivery(
                &runner,
                RuntimeRealtimeClusterDelivery {
                    event_id: 42,
                    event: RuntimeRealtimeClusterEvent::WalBatch {
                        records: vec![
                            refreshing_wal_record(slow_app, 41, 7, 0x41),
                            refreshing_wal_record(fast_app, 42, 7, 0x42),
                        ],
                    },
                },
            )
            .await;
        });
        let fast = tokio::time::timeout(Duration::from_millis(200), fast_session.recv()).await;
        let completed_early = batch.is_finished();
        assert!(
            service.inner.cluster_deliveries.should_apply(42),
            "batch must remain replayable while a refresh is blocked"
        );
        gate.add_permits(10);
        batch.await.unwrap();
        assert!(!service.inner.cluster_deliveries.should_apply(42));
        assert!(
            !completed_early,
            "the batch must not complete before its slow refresh"
        );
        assert_eq!(
            fast.expect("independent refresh was blocked behind another session")
                .unwrap()["processed-tx-id"],
            42
        );
    }

    #[tokio::test]
    async fn wal_backlog_window_does_not_block_later_independent_session() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let slow_app = Uuid::new_v4();
        let fast_app = Uuid::new_v4();
        let _slow = open_test_query_session(&service, slow_app).await;
        let mut fast = open_test_query_session(&service, fast_app).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        *data.query_gate_app_id.lock().unwrap() = Some(slow_app);
        data.query_version.store(1, Ordering::Relaxed);
        let mut records = (1..=32)
            .map(|tx| refreshing_wal_record(slow_app, tx, 7, u64::try_from(tx).unwrap()))
            .collect::<Vec<_>>();
        records.push(refreshing_wal_record(fast_app, 33, 7, 33));
        let runner = service.clone();
        let work = tokio::spawn(async move { runner.apply_wal_records(&records).await });
        let delivered = tokio::time::timeout(Duration::from_millis(200), fast.recv()).await;
        let completed_early = work.is_finished();
        gate.add_permits(32);
        work.await.unwrap().unwrap();
        assert!(
            !completed_early,
            "unfinished refreshes must remain replayable"
        );
        assert_eq!(
            delivered
                .expect("later independent session waited behind the first 32 records")
                .unwrap()["processed-tx-id"],
            33,
        );
    }

    #[tokio::test]
    async fn runtime_wal_pipeline_delivers_later_window_without_acknowledging_gap() {
        let data = Arc::new(TestData::default());
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let slow_app = Uuid::new_v4();
        let fast_app = Uuid::new_v4();
        let _slow = open_test_query_session(&service, slow_app).await;
        let mut fast = open_test_query_session(&service, fast_app).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        *data.query_gate_app_id.lock().unwrap() = Some(slow_app);
        data.query_version.store(1, Ordering::Relaxed);
        for (app_id, event_id) in [(slow_app, 128), (fast_app, 129)] {
            cluster.send(RuntimeRealtimeClusterDelivery {
                event_id,
                event: RuntimeRealtimeClusterEvent::WalBatch {
                    records: vec![ordinary_wal_record(
                        app_id,
                        event_id,
                        7,
                        u64::try_from(event_id).unwrap(),
                    )],
                },
            });
        }
        let delivered = tokio::time::timeout(Duration::from_millis(200), fast.recv()).await;
        assert!(
            !cluster.acknowledged(RealtimeClusterConsumerKind::Runtime, 129),
            "durable cursor must not pass the blocked preceding WAL window"
        );
        gate.add_permits(16);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cluster.acknowledged(RealtimeClusterConsumerKind::Runtime, 129) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("contiguous completions did not advance the durable cursor");
        assert_eq!(
            delivered
                .expect("later window waited for another session's refresh")
                .unwrap()["processed-tx-id"],
            129,
        );
    }

    #[tokio::test]
    async fn durable_wal_windows_retry_and_coalesce_with_the_latest_session_watermark() {
        let data = Arc::new(TestData::default());
        let policy = Arc::new(TestCoarseTopicsFailure::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_coarse_topics_policy(policy.clone());
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;
        data.query_version.store(1, Ordering::Relaxed);
        policy.fail.store(true, Ordering::Release);
        let mut first = service
            .schedule_wal_batch(&[ordinary_wal_record(app_id, 41, 7, 41)], 41)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut first)
                .await
                .is_err(),
            "failed durable refresh must retain its completion receipt"
        );
        let second = service
            .schedule_wal_batch(&[ordinary_wal_record(app_id, 42, 7, 42)], 42)
            .await
            .unwrap();
        let third = service
            .schedule_wal_batch(&[ordinary_wal_record(app_id, 43, 7, 43)], 43)
            .await
            .unwrap();
        policy.fail.store(false, Ordering::Release);
        tokio::time::timeout(
            Duration::from_secs(2),
            futures_util::future::join_all([first, second, third]),
        )
        .await
        .expect("durable retry did not complete every coalesced window");
        assert_eq!(session.recv().await.unwrap()["processed-tx-id"], 43);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), session.recv())
                .await
                .is_err(),
            "older failed watermarks must not overtake the successful retry"
        );
        let history = service.inner.sync_history.state.lock().unwrap();
        assert_eq!(
            history
                .for_app(app_id)
                .map(|tx| tx.tx_id)
                .collect::<Vec<_>>(),
            vec![41, 42, 43],
            "retrying refreshes must preserve ordered sync history without replaying preparation"
        );
    }

    #[test]
    fn wal_sink_resync_backoff_doubles_to_a_cap() {
        assert_eq!(wal_sink_resync_backoff(1), Duration::from_secs(1));
        assert_eq!(wal_sink_resync_backoff(2), Duration::from_secs(2));
        assert_eq!(wal_sink_resync_backoff(5), Duration::from_secs(16));
        assert_eq!(wal_sink_resync_backoff(6), Duration::from_secs(30));
        assert_eq!(wal_sink_resync_backoff(60), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn wal_sink_refresh_deadline_fails_one_refresh_without_stalling_its_window() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_wal_sink_refresh_deadline(Duration::from_millis(100));
        let slow_app = Uuid::new_v4();
        let fast_app = Uuid::new_v4();
        let mut slow = open_test_query_session(&service, slow_app).await;
        let mut fast = open_test_query_session(&service, fast_app).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        *data.query_gate_app_id.lock().unwrap() = Some(slow_app);
        data.query_version.store(1, Ordering::Relaxed);
        let before = crate::metrics::wal_sink_refresh_timeouts();
        let completion = service
            .schedule_wal_batch(
                &[
                    refreshing_wal_record(slow_app, 51, 7, 51),
                    refreshing_wal_record(fast_app, 52, 7, 52),
                ],
                77,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), completion)
            .await
            .expect("a timed-out refresh must not stall its WAL window");
        let fast_reply = tokio::time::timeout(Duration::from_secs(1), fast.recv())
            .await
            .expect("the other session in the window still refreshed")
            .unwrap();
        assert_eq!(fast_reply["op"], "refresh-ok");
        assert_eq!(fast_reply["processed-tx-id"], 52);
        let slow_error = tokio::time::timeout(Duration::from_secs(1), slow.recv())
            .await
            .expect("the timed-out refresh reported an error to its session")
            .unwrap();
        assert_eq!(slow_error["op"], "error");
        assert_eq!(slow_error["type"], "timeout");
        assert!(crate::metrics::wal_sink_refresh_timeouts() > before);
        assert_eq!(wal_sink_consecutive_timeouts(&service, slow.session_id), 1);
        // The abandoned refresh retries on its own once the backend recovers;
        // no new WAL event is needed to make the session converge.
        gate.add_permits(8);
        let resyncs = crate::metrics::wal_sink_forced_resyncs();
        let retry = tokio::time::timeout(Duration::from_secs(5), slow.recv())
            .await
            .expect("the timed-out session never retried its refresh")
            .unwrap();
        assert_eq!(retry["op"], "refresh-ok");
        assert!(crate::metrics::wal_sink_forced_resyncs() > resyncs);
        assert_eq!(wal_sink_consecutive_timeouts(&service, slow.session_id), 0);
    }

    #[tokio::test(start_paused = true)]
    #[allow(clippy::too_many_lines)] // One timeline asserts growth, reset, and re-growth.
    async fn wal_sink_forced_resync_backoff_grows_and_resets_after_success() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_wal_sink_refresh_deadline(Duration::from_millis(100));
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        data.query_version.store(1, Ordering::Relaxed);
        let completion = service
            .schedule_wal_batch(&[refreshing_wal_record(app_id, 61, 7, 61)], 61)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), completion)
            .await
            .expect("a timed-out refresh must not stall its WAL window");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), session.recv())
                .await
                .expect("the deadline failure was reported")
                .unwrap()["type"],
            "timeout"
        );
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            1
        );

        // The first retry waits one second: nothing runs before that.
        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            1
        );
        // Its retry hits the still-blocked backend and times out again.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), session.recv())
                .await
                .expect("the first forced retry ran and failed")
                .unwrap()["type"],
            "timeout"
        );
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            2
        );

        // The backoff doubled: one more second is not enough for the retry.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            2
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), session.recv())
                .await
                .expect("the second forced retry ran and failed")
                .unwrap()["type"],
            "timeout"
        );
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            3
        );

        // A successful refresh resets the consecutive count.
        gate.add_permits(16);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), session.recv())
                .await
                .expect("the forced retry converged once the backend recovered")
                .unwrap()["op"],
            "refresh-ok"
        );
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            0
        );

        // After the reset, the next deadline hit backs off from one second
        // again instead of staying at the large delay.
        let closed = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&closed));
        let completion = service
            .schedule_wal_batch(&[refreshing_wal_record(app_id, 62, 7, 62)], 62)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), completion)
            .await
            .expect("a timed-out refresh must not stall its WAL window");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), session.recv())
                .await
                .expect("the reset deadline failure was reported")
                .unwrap()["type"],
            "timeout"
        );
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            1
        );
        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            wal_sink_consecutive_timeouts(&service, session.session_id),
            1
        );
        closed.add_permits(8);
    }

    #[tokio::test]
    async fn runtime_wal_completion_window_backpressures_without_skipping_completed_gaps() {
        let (events, mut receiver) = broadcast::channel(1);
        let (release, blocked) = oneshot::channel::<()>();
        let mut pending = OrderedWalCompletions::new();
        pending.push_back(Box::pin(async move {
            blocked.await.unwrap();
            1
        }));
        for event_id in 2..=MAX_WAL_REFRESH_WINDOWS {
            pending.push_back(Box::pin(std::future::ready(
                i64::try_from(event_id).unwrap(),
            )));
        }
        events
            .send(RuntimeRealtimeClusterDelivery {
                event_id: 99,
                event: RuntimeRealtimeClusterEvent::WalBatch {
                    records: Vec::new(),
                },
            })
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                next_runtime_consumer_step(&mut receiver, &mut pending)
            )
            .await
            .is_err(),
            "completed later windows must neither skip a gap nor free unacknowledged capacity"
        );
        assert_eq!(
            receiver.len(),
            1,
            "capacity must be enforced before consuming another delivery"
        );
        release.send(()).unwrap();
        for expected in 1..=MAX_WAL_REFRESH_WINDOWS {
            let step = next_runtime_consumer_step(&mut receiver, &mut pending).await;
            assert!(
                matches!(step, RuntimeConsumerStep::Completed(event_id) if event_id == i64::try_from(expected).unwrap())
            );
        }
        assert!(
            matches!(next_runtime_consumer_step(&mut receiver, &mut pending).await, RuntimeConsumerStep::Delivery(Ok(delivery)) if delivery.event_id == 99)
        );
    }

    #[test]
    fn schema_changes_and_oversized_wal_deliveries_are_pipeline_fences() {
        let app_id = Uuid::new_v4();
        let ordinary = ordinary_wal_record(app_id, 1, 7, 1);
        assert!(can_pipeline_wal(&RuntimeRealtimeClusterEvent::WalBatch {
            records: vec![ordinary.clone()]
        }));
        let mut schema = ordinary.clone();
        schema
            .ident_changes
            .push(triple_entry(WalAction::Insert, Vec::new(), Vec::new()));
        assert!(!can_pipeline_wal(&RuntimeRealtimeClusterEvent::WalBatch {
            records: vec![ordinary.clone(), schema]
        }));
        let mut updated_attribute = ordinary.clone();
        updated_attribute.attr_changes.push(triple_entry(
            WalAction::Update,
            Vec::new(),
            Vec::new(),
        ));
        assert!(!can_pipeline_wal(&RuntimeRealtimeClusterEvent::Wal {
            record: updated_attribute,
            source_machine_id: Uuid::nil(),
        }));
        assert!(!can_pipeline_wal(&RuntimeRealtimeClusterEvent::WalBatch {
            records: vec![ordinary; MAX_WAL_REFRESH_BATCH + 1]
        }));
    }

    #[tokio::test]
    async fn wal_backlog_window_coalesces_repeated_session_work() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;
        let initial = data.query_count.load(Ordering::Relaxed);
        data.query_version.store(1, Ordering::Relaxed);
        let records = (1..=128)
            .map(|tx| refreshing_wal_record(app_id, tx, 7, u64::try_from(tx).unwrap()))
            .collect::<Vec<_>>();
        service.apply_wal_records(&records).await.unwrap();
        assert_eq!(session.recv().await.unwrap()["processed-tx-id"], 128);
        assert_eq!(
            data.query_count.load(Ordering::Relaxed) - initial,
            1,
            "a bounded backlog window should recompute each session only once"
        );
        let history = service.inner.sync_history.state.lock().unwrap();
        let tx_ids = history
            .for_app(app_id)
            .map(|tx| tx.tx_id)
            .collect::<Vec<_>>();
        assert_eq!(tx_ids, (1..=128).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn wal_batch_coalesces_refreshes_with_the_last_ordered_watermark() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;
        let initial = data.query_count.load(Ordering::Relaxed);
        data.query_version.store(1, Ordering::Relaxed);
        service
            .apply_wal_records(&[
                refreshing_wal_record(app_id, 41, 7, 0x41),
                refreshing_wal_record(app_id, 42, 7, 0x42),
                refreshing_wal_record(app_id, 43, 7, 0x43),
            ])
            .await
            .unwrap();
        let refresh = session.recv().await.unwrap();
        let tx_ids = service
            .inner
            .sync_history
            .state
            .lock()
            .unwrap()
            .for_app(app_id)
            .map(|tx| tx.tx_id)
            .collect::<Vec<_>>();
        assert_eq!(
            tx_ids,
            vec![41, 42, 43],
            "query coalescing must preserve every sync transaction"
        );
        assert_eq!(
            data.query_count.load(Ordering::Relaxed),
            initial + 1,
            "one batch should execute one refresh per affected session"
        );
        assert_eq!(refresh["processed-tx-id"], 43);
        assert_eq!(
            refresh["processed-isn"],
            json!(Isn::new(7, instant_wal::Lsn::from_u64(0x43)))
        );
    }

    #[tokio::test]
    async fn staged_commit_releases_writer_while_earlier_refresh_is_blocked() {
        let data = Arc::new(TestData::default());
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_cluster(cluster as Arc<dyn RuntimeRealtimeCluster>);
        let app_id = Uuid::new_v4();
        let session = open_test_query_session(&service, app_id).await;
        let initial_queries = data.query_count.load(Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(Arc::clone(&gate));
        data.query_version.store(1, Ordering::Relaxed);
        let earlier = refreshing_wal_record(app_id, 41, 7, 0x41);
        let refresher = service.clone();
        let refresh = tokio::spawn(async move {
            deliver_cluster_event(
                &refresher,
                RuntimeRealtimeClusterEvent::Wal {
                    source_machine_id: Uuid::new_v4(),
                    record: earlier,
                },
            )
            .await;
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) == initial_queries {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The production transaction backend has committed tx 42. Its WAL stage
        // succeeds while the earlier query remains blocked in the delivery lane.
        let committed = refreshing_wal_record(app_id, 42, 8, 0x42);
        let watermark_before_stage = processed_watermark(&service.inner, app_id);
        crate::replication::RealtimeInvalidationConsumer::stage(
            &service,
            &instant_realtime::ReplicationPipelineOutcome {
                record: Some(committed.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            processed_watermark(&service.inner, app_id),
            watermark_before_stage,
            "commit notification must not advance the query completion watermark"
        );
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            service.execute_transaction(session.session_id, &[]),
        )
        .await;
        gate.add_permits(1);
        refresh.await.unwrap();
        let response = response
            .expect("durably staged commit must not wait behind query refresh")
            .unwrap();
        assert_eq!(response["tx-id"], 42);
        assert_eq!(response["isn"], json!(committed.isn));
        assert_eq!(
            data.query_count.load(Ordering::Relaxed),
            initial_queries + 1,
            "commit observation must not execute or publish a newer query result"
        );
    }

    #[tokio::test]
    async fn atomic_publication_releases_writer_only_for_the_matching_machine() {
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        cluster.fail_wal_publication.store(true, Ordering::Release);
        let machine_id = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            machine_id,
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let record = test_wal_record(Uuid::new_v4(), Vec::new());
        let outcome = instant_realtime::ReplicationPipelineOutcome {
            record: Some(record.clone()),
            ..Default::default()
        };
        for publication in [None, Some(Uuid::new_v4())] {
            assert!(
                crate::replication::RealtimeInvalidationConsumer::stage_persisted(
                    &service,
                    &outcome,
                    publication
                )
                .await
                .is_err()
            );
            assert_eq!(
                service
                    .inner
                    .transaction_isns
                    .wait(record.tx_id, Duration::ZERO)
                    .await,
                None
            );
        }
        // The normal publisher is failing: success here proves we do not execute
        // it twice after a matching durable publication has already committed.
        crate::replication::RealtimeInvalidationConsumer::stage_persisted(
            &service,
            &outcome,
            Some(machine_id),
        )
        .await
        .unwrap();
        assert_eq!(
            service
                .inner
                .transaction_isns
                .wait(record.tx_id, Duration::ZERO)
                .await,
            Some(record.isn)
        );
        assert_eq!(
            service
                .inner
                .transaction_isns
                .wait(record.tx_id + 1, Duration::ZERO)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn failed_stage_does_not_notify_writer_and_retry_does() {
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        cluster.fail_wal_publication.store(true, Ordering::Release);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let record = test_wal_record(Uuid::new_v4(), Vec::new());
        let outcome = instant_realtime::ReplicationPipelineOutcome {
            record: Some(record.clone()),
            ..Default::default()
        };
        assert!(
            crate::replication::RealtimeInvalidationConsumer::stage(&service, &outcome)
                .await
                .is_err()
        );
        assert_eq!(
            service
                .inner
                .transaction_isns
                .wait(record.tx_id, Duration::ZERO)
                .await,
            None
        );
        cluster.fail_wal_publication.store(false, Ordering::Release);
        let waiting_service = service.clone();
        let waiter = tokio::spawn(async move {
            waiting_service
                .inner
                .transaction_isns
                .wait(record.tx_id, Duration::from_secs(1))
                .await
        });
        tokio::task::yield_now().await;
        crate::replication::RealtimeInvalidationConsumer::stage(&service, &outcome)
            .await
            .unwrap();
        assert_eq!(waiter.await.unwrap(), Some(record.isn));
        assert_eq!(
            service
                .inner
                .transaction_isns
                .wait(record.tx_id + 1, Duration::ZERO)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn transaction_response_prefers_isn_delivered_by_invalidator() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_transaction_isn_wait(Duration::from_secs(1));
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();

        let transaction_service = service.clone();
        let session_id = session.session_id;
        let transaction = tokio::spawn(async move {
            transaction_service
                .message(session_id, json!({"op": "transact", "tx-steps": []}))
                .await
        });
        tokio::task::yield_now().await;
        let mut record = test_wal_record(app_id, Vec::new());
        record.tx_id = 42;
        service.apply_wal_record(&record).await.unwrap();
        transaction.await.unwrap().unwrap();

        let response = session.recv().await.unwrap();
        assert_eq!(response["tx-id"], 42);
        assert_eq!(response["isn"], json!(record.isn));
    }

    #[tokio::test]
    async fn authenticated_presence_and_broadcasts_include_the_public_user_id() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let mut observer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut authenticated = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(observer.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        service
            .message(
                authenticated.session_id,
                json!({"op": "init", "app-id": app_id, "refresh-token": user_id}),
            )
            .await
            .unwrap();
        observer.recv().await.unwrap();
        authenticated.recv().await.unwrap();

        service
            .message(
                observer.session_id,
                json!({"op": "join-room", "room-id": "lobby"}),
            )
            .await
            .unwrap();
        observer.recv().await.unwrap();
        observer.recv().await.unwrap();
        service
            .message(
                authenticated.session_id,
                json!({"op": "join-room", "room-id": "lobby"}),
            )
            .await
            .unwrap();
        let presence = observer.recv().await.unwrap();
        assert_eq!(
            presence["data"][authenticated.session_id.to_string()]["user"]["id"],
            json!(user_id)
        );
        authenticated.recv().await.unwrap();
        authenticated.recv().await.unwrap();
        let shared_rooms = service.room_store();
        assert_eq!(
            shared_rooms
                .lock()
                .unwrap()
                .room_data(app_id, "lobby")
                .unwrap()
                .len(),
            2
        );

        service
            .message(
                authenticated.session_id,
                json!({
                    "op": "client-broadcast",
                    "room-id": "lobby",
                    "topic": "wave",
                    "data": {"hello": true}
                }),
            )
            .await
            .unwrap();
        let broadcast = observer.recv().await.unwrap();
        assert_eq!(broadcast["data"]["user"]["id"], json!(user_id));
        assert_eq!(
            broadcast["data"]["peer-id"],
            json!(authenticated.session_id)
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn sync_table_restarts_after_changes_and_route_rebuilds() {
        let data = Arc::new(TestData::default());
        let sync_history = Arc::new(RuntimeSyncHistory::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_sync_history(Arc::clone(&sync_history));
        let app_id = Uuid::new_v4();
        let mut first = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                first.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret"
                }),
            )
            .await
            .unwrap();
        assert_eq!(first.recv().await.unwrap()["auth"]["admin?"], true);

        service
            .message(
                first.session_id,
                json!({
                    "op": "start-sync",
                    "q": {"goals": {"$": {"order": {"serverCreatedAt": "asc"}}}},
                    "client-event-id": "start-1"
                }),
            )
            .await
            .unwrap();
        let started = first.recv().await.unwrap();
        assert_eq!(started["op"], "start-sync-ok");
        assert_eq!(started["client-event-id"], "start-1");
        let subscription_id =
            Uuid::parse_str(started["subscription-id"].as_str().unwrap()).unwrap();
        let token = Uuid::parse_str(started["token"].as_str().unwrap()).unwrap();
        let batch = first.recv().await.unwrap();
        assert_eq!(batch["op"], "sync-load-batch");
        assert_eq!(batch["join-rows"].as_array().unwrap().len(), 2);
        assert_eq!(first.recv().await.unwrap()["op"], "sync-init-finish");

        service
            .publish_sync_transaction(
                app_id,
                41,
                vec![json!({
                    "action": "added",
                    "triple": ["e", test_sync_attribute_id(), "v", 1]
                })],
            )
            .await;
        let restart = first.recv().await.unwrap();
        assert_eq!(restart["op"], "error");
        assert_eq!(restart["original-event"]["op"], "resync-table");

        service
            .message(
                first.session_id,
                json!({
                    "op": "remove-sync",
                    "subscription-id": subscription_id,
                    "keep-subscription": false
                }),
            )
            .await
            .unwrap();
        drop(first);

        drop(service);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_sync_history(sync_history);

        let mut resumed = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                resumed.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret"
                }),
            )
            .await
            .unwrap();
        resumed.recv().await.unwrap();
        service
            .message(
                resumed.session_id,
                json!({
                    "op": "resync-table",
                    "subscription-id": subscription_id,
                    "token": token,
                    "tx-id": 41
                }),
            )
            .await
            .unwrap();
        let replay = resumed.recv().await.unwrap();
        assert_eq!(replay["op"], "error");
        assert_eq!(replay["original-event"]["op"], "resync-table");
    }

    #[tokio::test]
    async fn sync_snapshot_waits_for_a_delayed_runtime_consumer_without_losing_frames() {
        const ROW_COUNT: usize = 12_701;
        let data = Arc::new(TestData::default());
        data.sync_snapshot_rows
            .store(ROW_COUNT, std::sync::atomic::Ordering::Relaxed);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": app_id, "__admin-token": "admin-secret"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        let start_service = service.clone();
        let session_id = session.session_id;
        let start = tokio::spawn(async move {
            start_service
                .message(session_id, json!({"op": "start-sync", "q": {"goals": {}}}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        service
            .publish_sync_transaction(
                app_id,
                1,
                vec![json!({
                    "action": "added",
                    "triple": ["catch-up", test_sync_attribute_id(), "value", 1]
                })],
            )
            .await;

        let mut subscription_id = None;
        let mut rows = 0;
        let mut finished = false;
        let mut caught_up = false;
        while !caught_up {
            let event = tokio::time::timeout(Duration::from_secs(5), session.recv())
                .await
                .expect("sync delivery stalled")
                .expect("runtime transport closed before catch-up");
            match event["op"].as_str() {
                Some("start-sync-ok") => {
                    subscription_id = event["subscription-id"].as_str().map(str::to_owned);
                }
                Some("sync-load-batch") => {
                    assert!(!finished);
                    rows += event["join-rows"].as_array().unwrap().len();
                }
                Some("sync-init-finish") => {
                    assert_eq!(rows, ROW_COUNT);
                    finished = true;
                }
                Some("error") => {
                    assert!(finished, "restart arrived before initialization finished");
                    assert_eq!(event["original-event"]["op"], "resync-table");
                    caught_up = true;
                }
                _ => {}
            }
        }
        start.await.unwrap().unwrap();
        let subscription_id = Uuid::parse_str(subscription_id.as_deref().unwrap()).unwrap();
        assert!(
            !sync_state(&service.inner)
                .active
                .contains_key(&(session_id, subscription_id))
        );
    }

    #[tokio::test]
    async fn receive_timeout_cancels_sync_and_cleans_durable_and_active_state() {
        let data = Arc::new(TestData::default());
        data.sync_snapshot_rows
            .store(12_700, std::sync::atomic::Ordering::Relaxed);
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        // Leave enough budget for the 127 ordered delivery acknowledgements;
        // the 129th frame still deterministically blocks behind the 128-slot
        // transport and exercises cancellation after durable creation.
        .with_receive_timeout_policy(Arc::new(StaticReceiveTimeout(Duration::from_millis(500))));
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": app_id, "__admin-token": "admin-secret"}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        service
            .message(
                session.session_id,
                json!({"op": "start-sync", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let subscription_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some((_, subscription)) = sync_state(&service.inner)
                    .active
                    .iter()
                    .find(|((owner, _), _)| *owner == session.session_id)
                {
                    break subscription.subscription;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("active sync subscription was never installed");

        tokio::time::timeout(Duration::from_secs(1), async {
            while data.sync_subscriptions.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable sync subscription was never created");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if data.sync_subscriptions.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable cleanup did not complete");
        let state = sync_state(&service.inner);
        assert!(
            !state
                .active
                .contains_key(&(session.session_id, subscription_id))
        );
        assert_eq!(
            state.stale_subscriptions.get(&subscription_id),
            Some(&app_id)
        );
    }

    #[tokio::test]
    async fn ordinary_frames_remain_ordered_across_transport_backpressure() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let mut session = service
            .open(None, RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let sender = sessions(&service.inner)
            .get(&session.session_id)
            .unwrap()
            .sender
            .clone();
        for index in 0..256 {
            send(&sender, json!({"op": "ordered", "index": index}));
        }

        for expected in 0..256 {
            let event = tokio::time::timeout(Duration::from_secs(2), session.recv())
                .await
                .expect("ordered outbound frame stalled")
                .expect("runtime transport closed");
            assert_eq!(event["index"], expected);
        }
    }

    #[tokio::test]
    async fn outbound_ingress_is_bounded_and_overflow_disconnects_the_session() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let session = service
            .open(None, RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let session_id = session.session_id;
        let sender = sessions(&service.inner)
            .get(&session_id)
            .unwrap()
            .sender
            .clone();

        let rejected = (0..1024).any(|index| {
            sender
                .try_send(json!({"op": "overflow", "index": index}))
                .is_err()
        });

        assert!(
            rejected,
            "the logical outbound queue must have a hard bound"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while sessions(&service.inner).contains_key(&session_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("overflowed session was not cleaned up");
    }

    #[tokio::test]
    async fn stalled_outbound_transport_is_disconnected_instead_of_dropping_frames() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let session = service
            .open(None, RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let session_id = session.session_id;
        let sender = sessions(&service.inner)
            .get(&session_id)
            .unwrap()
            .sender
            .clone();
        for index in 0..130 {
            send(&sender, json!({"op": "stalled", "index": index}));
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            while sessions(&service.inner).contains_key(&session_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stalled session was not cleaned up");
    }

    #[tokio::test]
    async fn ongoing_sync_restart_waits_for_backpressured_enqueue() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "init", "app-id": app_id, "__admin-token": "admin-secret"}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "start-sync", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let started = session.recv().await.unwrap();
        let subscription_id =
            Uuid::parse_str(started["subscription-id"].as_str().unwrap()).unwrap();
        while session.recv().await.unwrap()["op"] != "sync-init-finish" {}
        let sender = sessions(&service.inner)
            .get(&session.session_id)
            .unwrap()
            .sender
            .clone();
        for index in 0..128 {
            sender
                .try_send(json!({"op": "filler", "index": index}))
                .unwrap();
        }

        let publish_service = service.clone();
        let publish = tokio::spawn(async move {
            publish_service
                .publish_sync_transaction(
                    app_id,
                    41,
                    vec![json!({
                        "action": "added",
                        "triple": ["ongoing", test_sync_attribute_id(), "value", 1]
                    })],
                )
                .await;
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !sync_state(&service.inner)
                    .active
                    .contains_key(&(session.session_id, subscription_id))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ongoing update did not claim the sync restart");

        let mut restart = None;
        while restart.is_none() {
            let event = tokio::time::timeout(Duration::from_secs(2), session.recv())
                .await
                .expect("ongoing update stalled")
                .expect("runtime transport closed");
            if event["op"] == "error" {
                restart = Some(event);
            }
        }
        publish.await.unwrap();
        assert_eq!(restart.unwrap()["original-event"]["op"], "resync-table");
    }

    #[test]
    fn sync_history_eviction_stales_subscriptions_that_have_not_consumed_the_entry() {
        let app_id = Uuid::new_v4();
        let slow_session = Uuid::new_v4();
        let fast_session = Uuid::new_v4();
        let slow_subscription = Uuid::new_v4();
        let fast_subscription = Uuid::new_v4();
        let mut state = SyncState::default();
        let mut history = SyncHistoryState::default();
        for (session, subscription, sent_tx) in [
            (slow_session, slow_subscription, 0),
            (fast_session, fast_subscription, 1),
        ] {
            state.active.insert(
                (session, subscription),
                ActiveSyncSubscription {
                    session,
                    app: app_id,
                    subscription,
                    sent_tx,
                    attribute_ids: vec![test_sync_attribute_id()],
                    reset_on_change: false,
                    ready: true,
                    draining: false,
                },
            );
        }
        record_sync_transaction(
            &mut state,
            &mut history,
            SyncTransaction {
                app_id,
                tx_id: 1,
                changes: Vec::new(),
            },
            1,
        );
        let stale_sessions = record_sync_transaction(
            &mut state,
            &mut history,
            SyncTransaction {
                app_id,
                tx_id: 2,
                changes: Vec::new(),
            },
            1,
        );

        assert_eq!(stale_sessions, BTreeSet::from([slow_session]));
        assert!(
            !state
                .active
                .contains_key(&(slow_session, slow_subscription))
        );
        assert!(
            state
                .active
                .contains_key(&(fast_session, fast_subscription))
        );
        assert_eq!(
            state.stale_subscriptions.get(&slow_subscription),
            Some(&app_id)
        );
        assert_eq!(history.evicted_through.get(&app_id), Some(&1));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn cluster_lag_requests_sync_table_restart_once_and_quiesces_subscription() {
        let app_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret"
                }),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        service
            .message(
                session.session_id,
                json!({"op": "start-sync", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        let started = session.recv().await.unwrap();
        assert_eq!(started["op"], "start-sync-ok");
        let subscription_id =
            Uuid::parse_str(started["subscription-id"].as_str().unwrap()).expect("subscription id");
        let token = Uuid::parse_str(started["token"].as_str().unwrap()).expect("sync token");
        while session.recv().await.unwrap()["op"] != "sync-init-finish" {}

        service.recover_sync_subscriptions_after_cluster_lag();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["original-event"]["op"], "resync-table");
        assert_eq!(
            error["original-event"]["subscription-id"],
            json!(subscription_id)
        );
        assert!(
            !sync_state(&service.inner)
                .active
                .get(&(session.session_id, subscription_id))
                .unwrap()
                .ready
        );
        assert!(
            sync_state(&service.inner)
                .stale_subscriptions
                .contains_key(&subscription_id)
        );

        service.recover_sync_subscriptions_after_cluster_lag();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), session.recv())
                .await
                .is_err(),
            "a quiesced subscription must not receive duplicate resync errors"
        );

        drop(session);
        let mut reconnected = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                reconnected.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret"
                }),
            )
            .await
            .unwrap();
        assert_eq!(reconnected.recv().await.unwrap()["op"], "init-ok");
        service
            .message(
                reconnected.session_id,
                json!({
                    "op": "resync-table",
                    "subscription-id": subscription_id,
                    "token": token,
                    "tx-id": 40,
                }),
            )
            .await
            .unwrap();
        let stale_error = reconnected.recv().await.unwrap();
        assert_eq!(stale_error["op"], "error");
        assert_eq!(stale_error["original-event"]["op"], "resync-table");
        assert_eq!(
            stale_error["original-event"]["subscription-id"],
            json!(subscription_id)
        );

        service
            .message(
                reconnected.session_id,
                json!({
                    "op": "remove-sync",
                    "subscription-id": subscription_id,
                }),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while sync_state(&service.inner)
                .stale_subscriptions
                .contains_key(&subscription_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("remove-sync did not finish in its grouped worker");
        assert!(
            !sync_state(&service.inner)
                .active
                .contains_key(&(reconnected.session_id, subscription_id))
        );
        assert!(
            !sync_state(&service.inner)
                .stale_subscriptions
                .contains_key(&subscription_id)
        );
        assert!(
            !data
                .sync_subscriptions
                .lock()
                .unwrap()
                .contains_key(&subscription_id)
        );
    }

    #[test]
    fn wal_triples_map_insert_update_and_delete_to_sync_changes() {
        let attr_id = test_sync_attribute_id();
        let before = wal_columns(&[
            ("entity_id", json!("entity-1")),
            ("attr_id", json!(attr_id)),
            ("value", json!("{\"score\":1}")),
            ("created_at", json!(10)),
        ]);
        let after = wal_columns(&[
            ("entity_id", json!("entity-1")),
            ("attr_id", json!(attr_id)),
            ("value", json!("{\"score\":2}")),
            ("created_at", json!(11)),
        ]);
        let record = test_wal_record(
            Uuid::new_v4(),
            vec![
                triple_entry(WalAction::Insert, before.clone(), Vec::new()),
                triple_entry(WalAction::Update, after.clone(), before.clone()),
                triple_entry(WalAction::Delete, Vec::new(), after),
            ],
        );

        let changes = sync_changes_from_wal(&record);

        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0]["action"], "added");
        assert_eq!(changes[0]["triple"][2], json!({"score": 1}));
        assert_eq!(changes[1]["action"], "removed");
        assert_eq!(changes[1]["triple"][2], json!({"score": 1}));
        assert_eq!(changes[2]["action"], "added");
        assert_eq!(changes[2]["triple"][2], json!({"score": 2}));
        assert_eq!(changes[3]["action"], "removed");
    }

    #[test]
    fn truncate_changes_force_a_full_runtime_refresh() {
        let app_id = Uuid::new_v4();
        let truncate = || triple_entry(WalAction::Truncate, Vec::new(), Vec::new());

        let triples = test_wal_record(app_id, vec![truncate()]);
        assert!(wal_schema_refresh(&triples));

        let mut attrs = test_wal_record(app_id, Vec::new());
        let mut attr_truncate = truncate();
        attr_truncate.table = Some("attrs".to_owned());
        attrs.attr_changes.push(attr_truncate);
        assert!(wal_schema_refresh(&attrs));

        let mut idents = test_wal_record(app_id, Vec::new());
        let mut ident_truncate = truncate();
        ident_truncate.table = Some("idents".to_owned());
        idents.ident_changes.push(ident_truncate);
        assert!(wal_schema_refresh(&idents));
    }

    #[tokio::test]
    async fn committed_wal_refreshes_queries_and_matching_sync_subscriptions() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut query_session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut sync_session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [query_session.session_id, sync_session.session_id] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "__admin-token": "admin-secret"
                    }),
                )
                .await
                .unwrap();
        }
        query_session.recv().await.unwrap();
        sync_session.recv().await.unwrap();
        service
            .message(
                query_session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        query_session.recv().await.unwrap();
        service
            .message(
                sync_session.session_id,
                json!({"op": "start-sync", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(sync_session.recv().await.unwrap()["op"], "start-sync-ok");
        assert_eq!(sync_session.recv().await.unwrap()["op"], "sync-load-batch");
        assert_eq!(sync_session.recv().await.unwrap()["op"], "sync-init-finish");
        let record = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(Uuid::new_v4())),
                    ("attr_id", json!(test_sync_attribute_id())),
                    ("value", json!("{\"done\":true}")),
                    ("created_at", json!(12)),
                ]),
                Vec::new(),
            )],
        );

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service.apply_wal_record(&record).await.unwrap();

        let update = sync_session.recv().await.unwrap();
        assert_eq!(update["op"], "error");
        assert_eq!(update["original-event"]["op"], "resync-table");
        let refresh = query_session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(refresh["processed-tx-id"], 84);
        assert_eq!(refresh["processed-isn"], json!(record.isn));
        assert_eq!(refresh["computations"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn identical_queries_share_execution_within_auth_and_invalidation_generation() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut first_guest = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut second_guest = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut admin = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [first_guest.session_id, second_guest.session_id] {
            service
                .message(session_id, json!({"op": "init", "app-id": app_id}))
                .await
                .unwrap();
        }
        service
            .message(
                admin.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret"
                }),
            )
            .await
            .unwrap();
        for session in [&mut first_guest, &mut second_guest, &mut admin] {
            assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        }

        for session_id in [
            first_guest.session_id,
            second_guest.session_id,
            admin.session_id,
        ] {
            service
                .message(session_id, json!({"op": "add-query", "q": {"goals": {}}}))
                .await
                .unwrap();
        }
        for session in [&mut first_guest, &mut second_guest, &mut admin] {
            assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");
        }
        assert_eq!(
            data.query_count.load(std::sync::atomic::Ordering::Relaxed),
            2
        );

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        let record = test_wal_record(app_id, Vec::new());
        service.apply_wal_record(&record).await.unwrap();
        for session in [&mut first_guest, &mut second_guest, &mut admin] {
            let refresh = session.recv().await.unwrap();
            assert_eq!(refresh["op"], "refresh-ok");
            assert_eq!(refresh["computations"].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            data.query_count.load(std::sync::atomic::Ordering::Relaxed),
            4
        );

        service.apply_wal_record(&record).await.unwrap();
        for session in [&mut first_guest, &mut second_guest, &mut admin] {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), session.recv())
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            data.query_count.load(std::sync::atomic::Ordering::Relaxed),
            6
        );
    }

    #[tokio::test]
    async fn foreground_query_and_wal_refresh_freeze_distinct_workloads_at_the_loader_boundary() {
        use crate::postgres_pool::DatabaseWorkload;
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data.clone(), Uuid::new_v4());
        let app = Uuid::new_v4();
        let mut session = DatabaseWorkload::Refresh
            .scope(open_test_query_session(&service, app))
            .await;
        assert_eq!(
            *data.query_workloads.lock().unwrap(),
            [DatabaseWorkload::Interactive]
        );
        data.query_version.store(1, Ordering::Relaxed);
        service
            .apply_wal_record(&refreshing_wal_record(app, 84, 7, 0x42))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-ok");
        assert_eq!(
            *data.query_workloads.lock().unwrap(),
            [DatabaseWorkload::Interactive, DatabaseWorkload::Refresh]
        );
    }

    #[tokio::test]
    async fn coarse_selected_refresh_disables_shared_frame_reuse_for_its_batch() {
        for coarse in [true, false] {
            let data = Arc::new(TestData::default());
            let service = RuntimeRealtimeService::new(
                Arc::new(TestAuthorizer),
                Arc::clone(&data),
                Uuid::new_v4(),
            )
            .with_coarse_topics_policy(Arc::new(TestCoarseTopics(coarse)));
            let app = Uuid::new_v4();
            let mut session = open_test_query_session(&service, app).await;
            data.query_version.store(1, Ordering::Relaxed);
            service
                .apply_wal_record(&refreshing_wal_record(app, 84, 7, 0x42))
                .await
                .unwrap();
            assert_eq!(session.recv().await.unwrap()["op"], "refresh-ok");
            assert_eq!(
                *data.realtime_batch_reuse_disabled.lock().unwrap(),
                vec![coarse],
                "only a coarse-triggered batch may fall back to the per-request frames"
            );
            assert_eq!(
                *data.query_join_rows_reuse_disabled.lock().unwrap(),
                vec![false, coarse],
                "the detached refresh loader must carry the same decision as the batch"
            );
        }
    }

    #[tokio::test]
    async fn foreground_and_refresh_waiters_keep_shared_execution_and_global_admission() {
        use crate::postgres_pool::DatabaseWorkload;
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data.clone(), Uuid::new_v4())
                .with_datalog_cache_policy(Arc::new(TestDatalogCachePolicy {
                    calls: AtomicUsize::new(0),
                    settings: DatalogCacheSettings {
                        process_max_in_flight: 1,
                        max_in_flight: 1,
                        ..DatalogCacheSettings::default()
                    },
                }));
        let app = Uuid::new_v4();
        let session = open_test_query_session(&service, app).await;
        let entry = initialized_entry(&service.inner, session.session_id).unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(gate.clone());
        for foreground_first in [true, false] {
            clear_app_query_cache(&service.inner, app);
            let query = json!({"goals":{"$":{"limit":7}}})
                .as_object()
                .unwrap()
                .clone();
            let before = data.query_count.load(Ordering::Relaxed);
            let run = |foreground,
                       service: RuntimeRealtimeService<TestAuthorizer, TestData>,
                       entry: SessionSnapshot,
                       query| async move {
                if foreground {
                    service
                        .execute_session_query(&entry, query, QueryReturnType::JoinRows)
                        .await
                } else {
                    service
                        .execute_session_query_with_native_catalog(
                            &entry,
                            query,
                            QueryReturnType::JoinRows,
                            Some((app, Arc::new(AttributeCatalog::default()))),
                        )
                        .await
                }
            };
            let first = tokio::spawn(run(
                foreground_first,
                service.clone(),
                entry.clone(),
                query.clone(),
            ));
            tokio::time::timeout(Duration::from_secs(1), async {
                while data.query_count.load(Ordering::Relaxed) != before + 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let second = tokio::spawn(run(
                !foreground_first,
                service.clone(),
                entry.clone(),
                query,
            ));
            let different = json!({"goals":{"$":{"limit":8}}})
                .as_object()
                .unwrap()
                .clone();
            let error = run(!foreground_first, service.clone(), entry.clone(), different)
                .await
                .err()
                .unwrap();
            assert_eq!(error.body().kind, ApiErrorType::RateLimited);
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert_eq!(data.query_count.load(Ordering::Relaxed), before + 1);
            assert_eq!(
                *data.query_workloads.lock().unwrap().last().unwrap(),
                if foreground_first {
                    DatabaseWorkload::Interactive
                } else {
                    DatabaseWorkload::Refresh
                }
            );
            gate.add_permits(1);
            let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(first, second)
            })
            .await
            .unwrap();
            assert_eq!(
                first.unwrap().unwrap().result,
                second.unwrap().unwrap().result
            );
            assert_eq!(data.query_count.load(Ordering::Relaxed), before + 1);
        }
    }

    #[tokio::test]
    async fn initial_sdk_read_bypasses_saturated_refresh_without_using_control_or_new_slots() {
        use crate::postgres_pool::DatabaseWorkload;
        let Some(url) = instant_test_support::test_database_url(
            "initial_sdk_read_bypasses_saturated_refresh_without_using_control_or_new_slots",
        ) else {
            return;
        };
        let pool = crate::PostgresPool::connect_workload_partitioned(&url, 16)
            .await
            .unwrap();
        // Each partition performs one startup credential/connection probe.
        let control_before = pool.control_pool().acquisition_count();
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data.clone(), Uuid::new_v4());
        let app = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app).await;
        *data.query_pool.lock().unwrap() = Some(pool.clone());
        let refresh = DatabaseWorkload::Refresh
            .scope(async {
                let mut leases = Vec::new();
                for _ in 0..pool.max_size() {
                    leases.push(pool.acquire().await.unwrap());
                }
                leases
            })
            .await;
        tokio::time::timeout(
            Duration::from_millis(500),
            service.message(
                session.session_id,
                json!({"op":"add-query","q":{"goals":{"$":{"limit":7}}}}),
            ),
        )
        .await
        .expect("initial read inherited saturated refresh reservation")
        .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");
        assert_eq!(
            *data.query_workloads.lock().unwrap().last().unwrap(),
            DatabaseWorkload::Interactive
        );
        drop(refresh);
        let mut interactive = Vec::new();
        for _ in 0..pool.max_size() {
            interactive.push(pool.acquire().await.unwrap());
        }
        let entry = initialized_entry(&service.inner, session.session_id).unwrap();
        let query = json!({"goals":{"$":{"limit":8}}})
            .as_object()
            .unwrap()
            .clone();
        tokio::time::timeout(
            Duration::from_millis(500),
            service.execute_session_query_with_native_catalog(
                &entry,
                query,
                QueryReturnType::JoinRows,
                Some((app, Arc::new(AttributeCatalog::default()))),
            ),
        )
        .await
        .expect("refresh inherited saturated interactive reservation")
        .unwrap();
        assert_eq!(
            *data.query_workloads.lock().unwrap().last().unwrap(),
            DatabaseWorkload::Refresh
        );
        assert_eq!(pool.control_pool().acquisition_count(), control_before);
        assert_eq!(pool.max_size(), 6);
        assert_eq!(pool.control_pool().max_size(), 4);
        assert_eq!(
            DatabaseWorkload::Refresh
                .scope(async { pool.max_size() })
                .await,
            6
        );
    }

    #[tokio::test]
    async fn native_catalog_hint_stays_inside_shared_execution_and_admission() {
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data.clone(), Uuid::new_v4())
                .with_datalog_cache_policy(Arc::new(TestDatalogCachePolicy {
                    calls: AtomicUsize::new(0),
                    settings: DatalogCacheSettings {
                        process_max_in_flight: 1,
                        max_in_flight: 1,
                        ..DatalogCacheSettings::default()
                    },
                }));
        let app = Uuid::new_v4();
        let session = open_test_query_session(&service, app).await;
        let entry = initialized_entry(&service.inner, session.session_id).unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(gate.clone());
        let query = json!({"goals":{"$":{"limit":7}}})
            .as_object()
            .unwrap()
            .clone();
        let run = |service: RuntimeRealtimeService<TestAuthorizer, TestData>,
                   entry: SessionSnapshot,
                   query: Map<String, Value>| async move {
            service
                .execute_session_query_with_native_catalog(
                    &entry,
                    query,
                    QueryReturnType::JoinRows,
                    Some((app, Arc::new(AttributeCatalog::default()))),
                )
                .await
        };
        let first = tokio::spawn(run(service.clone(), entry.clone(), query.clone()));
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.native_catalog_count.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second = tokio::spawn(run(service.clone(), entry.clone(), query));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let different = json!({"goals":{"$":{"limit":8}}})
            .as_object()
            .unwrap()
            .clone();
        let error = run(service.clone(), entry, different)
            .await
            .err()
            .expect("a distinct query must honor the existing admission limit");
        assert_eq!(error.body().kind, ApiErrorType::RateLimited);
        assert_eq!(
            data.native_catalog_count.load(Ordering::Relaxed),
            1,
            "hint bypassed shared execution/admission"
        );
        gate.add_permits(2);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(data.native_catalog_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn invalidated_native_catalog_hint_retries_through_fresh_metadata_path() {
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), data.clone(), Uuid::new_v4());
        let app = Uuid::new_v4();
        let session = open_test_query_session(&service, app).await;
        let entry = initialized_entry(&service.inner, session.session_id).unwrap();
        let before = data.query_count.load(Ordering::Relaxed);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *data.query_gate.lock().unwrap() = Some(gate.clone());
        let query = json!({"goals":{"$":{"limit":7}}})
            .as_object()
            .unwrap()
            .clone();
        let worker = service.clone();
        let load = tokio::spawn(async move {
            worker
                .execute_session_query_with_native_catalog(
                    &entry,
                    query,
                    QueryReturnType::JoinRows,
                    Some((app, Arc::new(AttributeCatalog::default()))),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.native_catalog_count.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        clear_app_query_cache(&service.inner, app);
        tokio::time::timeout(Duration::from_secs(1), async {
            while data.query_count.load(Ordering::Relaxed) < before + 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            data.native_catalog_count.load(Ordering::Relaxed),
            1,
            "retry reused a catalog hint from an invalidated generation"
        );
        gate.add_permits(2);
        load.await.unwrap().unwrap();
    }

    async fn assert_fully_unchanged_byop_is_silent(
        service: &RuntimeRealtimeService<TestAuthorizer, TestData>,
        sessions: [&mut RuntimeRealtimeSession; 2],
        app_id: Uuid,
        topic: &instant_pg::ByopTopic,
    ) {
        let record = instant_pg::ByopWalRecord {
            tx_id: 103,
            next_lsn: instant_wal::Lsn::from_u64(103),
            changes: Vec::new(),
        };
        service
            .apply_byop_record(app_id, &record, std::slice::from_ref(topic))
            .await
            .unwrap();
        for session in sessions {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), session.recv())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn committed_byop_wal_refreshes_queries_only_for_the_configured_app() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let affected_attr = Uuid::new_v4();
        let unrelated_attr = Uuid::new_v4();
        let mut affected = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut unrelated = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session in [&affected, &unrelated] {
            service
                .message(
                    session.session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id
                    }),
                )
                .await
                .unwrap();
        }
        affected.recv().await.unwrap();
        unrelated.recv().await.unwrap();
        let dependency = |attribute| instant_pg::ByopTopic {
            entities: instant_pg::ByopMatchSet::Any,
            attributes: instant_pg::ByopMatchSet::Values(vec![attribute]),
            values: instant_pg::ByopMatchSet::Any,
        };
        *data.byop_topics.lock().unwrap() = Some(vec![
            serde_json::to_value(dependency(affected_attr)).unwrap(),
        ]);
        service
            .message(
                affected.session_id,
                json!({"op": "add-query", "q": {"posts": {}}}),
            )
            .await
            .unwrap();
        affected.recv().await.unwrap();
        *data.byop_topics.lock().unwrap() = Some(vec![
            serde_json::to_value(dependency(unrelated_attr)).unwrap(),
        ]);
        service
            .message(
                unrelated.session_id,
                json!({"op": "add-query", "q": {"users": {}}}),
            )
            .await
            .unwrap();
        unrelated.recv().await.unwrap();
        let record = instant_pg::ByopWalRecord {
            tx_id: 101,
            next_lsn: instant_wal::Lsn::from_u64(101),
            changes: Vec::new(),
        };

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service
            .apply_byop_record(app_id, &record, &[dependency(affected_attr)])
            .await
            .unwrap();

        let refresh = affected.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(refresh["processed-tx-id"], 101);
        assert_eq!(refresh["processed-isn"], Value::Null);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), unrelated.recv())
                .await
                .is_err()
        );

        let unchanged = instant_pg::ByopWalRecord {
            tx_id: 102,
            next_lsn: instant_wal::Lsn::from_u64(102),
            changes: Vec::new(),
        };
        service
            .apply_byop_record(app_id, &unchanged, &[dependency(unrelated_attr)])
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), affected.recv())
                .await
                .is_err()
        );
        assert_eq!(unrelated.recv().await.unwrap()["op"], "refresh-ok");

        assert_fully_unchanged_byop_is_silent(
            &service,
            [&mut affected, &mut unrelated],
            app_id,
            &dependency(unrelated_attr),
        )
        .await;
    }

    #[tokio::test]
    async fn remote_wal_event_refreshes_sessions_on_this_machine() {
        let local_machine = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), Arc::clone(&data), local_machine);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: Uuid::new_v4(),
                record: test_wal_record(app_id, Vec::new()),
            },
        )
        .await;

        let refresh = session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(refresh["processed-tx-id"], 84);
    }

    #[tokio::test]
    async fn runtime_cluster_consumer_retries_failed_subscription_then_delivers_and_acks() {
        let local_machine = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), Arc::clone(&data), local_machine)
                .with_transaction_isn_wait(Duration::ZERO)
                .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;

        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime cluster consumer did not retry subscription");

        data.query_version.store(1, Ordering::Relaxed);
        let event_id = 71;
        cluster.send(RuntimeRealtimeClusterDelivery {
            event_id,
            event: RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: Uuid::new_v4(),
                record: test_wal_record(app_id, Vec::new()),
            },
        });

        let refresh = tokio::time::timeout(Duration::from_secs(2), session.recv())
            .await
            .expect("runtime cluster delivery timed out")
            .expect("runtime session closed");
        assert_eq!(refresh["op"], "refresh-ok");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cluster.acknowledged(RealtimeClusterConsumerKind::Runtime, event_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime cluster delivery was not acknowledged");
    }

    #[tokio::test]
    async fn runtime_cluster_consumer_stops_while_idle_after_service_drop() {
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime cluster consumer did not become idle");

        drop(service);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&cluster) != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("idle runtime cluster consumer retained the cluster");
    }

    #[tokio::test]
    async fn ready_cluster_retry_stops_when_owner_is_already_gone() {
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        drop(owner);
        assert_eq!(
            await_cluster_while_owned(&weak, std::future::ready(7)).await,
            None
        );
    }

    #[tokio::test]
    async fn runtime_cluster_consumer_stops_during_persistent_ack_failure() {
        let app_id = Uuid::new_v4();
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::new());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime cluster consumer did not subscribe");
        cluster.fail_acknowledgements();
        let event_id = 72;
        cluster.send(RuntimeRealtimeClusterDelivery {
            event_id,
            event: RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: Uuid::new_v4(),
                record: test_wal_record(app_id, Vec::new()),
            },
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cluster.acknowledged(RealtimeClusterConsumerKind::Runtime, event_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime acknowledgement was not attempted");

        drop(service);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&cluster) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime acknowledgement retry retained the cluster");
    }

    #[tokio::test]
    async fn runtime_cluster_consumer_backs_off_repeated_closed_receivers() {
        let cluster = Arc::new(cluster_test_support::FailFirstSubscribeCluster::closed());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime cluster consumer did not retry closed receivers");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(cluster.attempts(), 3);
        drop(service);
    }

    #[tokio::test]
    async fn local_wal_event_is_applied_from_the_durable_cluster_lane() {
        // Cluster-enabled coordinators do not apply directly from the
        // replication queue. The durable event lane is authoritative even for
        // the source machine, so a restart between staging and delivery cannot
        // lose the refresh.
        let local_machine = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service =
            RuntimeRealtimeService::new(Arc::new(TestAuthorizer), Arc::clone(&data), local_machine);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: local_machine,
                record: test_wal_record(app_id, Vec::new()),
            },
        )
        .await;

        let refresh = session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
    }

    #[tokio::test]
    async fn raw_cache_intents_clear_app_queries_and_broadcast_status() {
        let app_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        let cache = RuntimeQueryCache::default();
        let key = SharedQueryKey {
            actor: SharedQueryActor::Admin,
            query: "cache-intent".into(),
            return_type: QueryReturnType::JoinRows,
            versions: "null".into(),
        };
        cache
            .get_or_load(key.clone(), || async {
                Ok(SessionQueryResult {
                    result: Arc::new(json!({"version": 1})),
                    result_meta: Arc::new(Value::Null),
                    byop_topics: None,
                    instant_topics: None,
                })
            })
            .await
            .unwrap();
        query_caches(&service.inner).insert(app_id, cache.clone());

        service.apply_cache_eviction(&WalCacheEviction::Rules {
            app_id: Some(app_id),
        });
        let reloads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reload_counter = Arc::clone(&reloads);
        cache
            .get_or_load(key, move || async move {
                reload_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SessionQueryResult {
                    result: Arc::new(json!({"version": 2})),
                    result_meta: Arc::new(Value::Null),
                    byop_topics: None,
                    instant_topics: None,
                })
            })
            .await
            .unwrap();
        assert_eq!(reloads.load(std::sync::atomic::Ordering::SeqCst), 1);

        service.apply_cache_eviction(&WalCacheEviction::App {
            app_id: Some(app_id),
            changed_status: Some(json!("paused")),
        });
        let event = session.recv().await.unwrap();
        assert_eq!(event["op"], "app-status-changed");
        assert_eq!(event["status"], "paused");
    }

    #[tokio::test]
    async fn cluster_lag_recomputes_all_local_queries_with_unknown_watermark() {
        let app_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"goals": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service.recover_cluster_lag().await;

        let refresh = session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(refresh["processed-tx-id"], Value::Null);
        assert_eq!(refresh["processed-isn"], Value::Null);
        assert_eq!(refresh["computations"].as_array().unwrap().len(), 1);
        assert!(refresh["attrs"].is_array());
    }

    #[tokio::test]
    async fn replication_dispatch_refreshes_only_topic_affected_sessions() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut affected = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut unaffected = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [affected.session_id, unaffected.session_id] {
            service
                .message(session_id, json!({"op": "init", "app-id": app_id}))
                .await
                .unwrap();
        }
        affected.recv().await.unwrap();
        unaffected.recv().await.unwrap();
        for session_id in [affected.session_id, unaffected.session_id] {
            service
                .message(session_id, json!({"op": "add-query", "q": {"goals": {}}}))
                .await
                .unwrap();
        }
        affected.recv().await.unwrap();
        unaffected.recv().await.unwrap();
        let record = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!("entity-4")),
                    ("attr_id", json!(test_sync_attribute_id())),
                    ("value", json!("true")),
                    ("created_at", json!(13)),
                ]),
                Vec::new(),
            )],
        );
        let outcome = instant_realtime::ReplicationPipelineOutcome {
            record: Some(record),
            dispatch: Some(instant_realtime::DispatchOutcome {
                refresh_sessions: BTreeSet::from([affected.session_id]),
                ..instant_realtime::DispatchOutcome::default()
            }),
            ..instant_realtime::ReplicationPipelineOutcome::default()
        };

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service.apply_replication_outcome(&outcome).await.unwrap();

        assert_eq!(affected.recv().await.unwrap()["op"], "refresh-ok");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), unaffected.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn indexed_wal_selection_examines_only_matching_dependencies() {
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let target_attr = Uuid::new_v4();
        let mut subscribers = Vec::new();
        for index in 0..1000 {
            let attr_id = if index == 0 {
                target_attr
            } else {
                Uuid::new_v4()
            };
            *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr_id));
            clear_app_query_cache(&service.inner, app_id);
            subscribers.push(open_test_query_session(&service, app_id).await);
        }
        data.query_version.store(1, Ordering::Relaxed);
        let record = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(Uuid::new_v4())),
                    ("attr_id", json!(target_attr)),
                    ("value", json!("true")),
                    ("ea", json!(true)),
                ]),
                vec![],
            )],
        );
        service.apply_wal_record(&record).await.unwrap();
        assert_eq!(subscribers[0].recv().await.unwrap()["op"], "refresh-ok");
        assert_eq!(
            service
                .inner
                .last_invalidation_candidates
                .load(Ordering::Relaxed),
            1,
            "one write should not examine all 1000 subscriptions"
        );
        // The backend now returns the last subscriber's dependency. Refresh
        // completion must replace the target's old route, not accumulate it.
        service.apply_wal_record(&record).await.unwrap();
        assert_eq!(
            service
                .inner
                .last_invalidation_candidates
                .load(Ordering::Relaxed),
            0
        );
        let broad = instant_wal::Topic {
            indexes: vec![instant_datalog::IndexKey::Ea],
            entities: instant_wal::MatchSet::Any,
            attributes: instant_wal::MatchSet::Any,
            values: instant_wal::MatchSet::Any,
        };
        service
            .message(
                subscribers[1].session_id,
                json!({"op":"remove-query", "q":{"goals":{}}}),
            )
            .await
            .unwrap();
        assert_eq!(
            subscribers[1].recv().await.unwrap()["op"],
            "remove-query-ok"
        );
        assert_eq!(
            subscription_index(&service.inner)
                .candidates(app_id, std::slice::from_ref(&broad))
                .len(),
            999
        );
        drop(subscribers);
        assert!(
            subscription_index(&service.inner)
                .candidates(app_id, &[broad])
                .is_empty()
        );
    }

    #[tokio::test]
    async fn native_wal_selection_refreshes_only_sessions_with_intersecting_query_topics() {
        fn framework_topics(attr_id: Uuid) -> RuntimeFrameworkQueryTopics {
            let dependency = instant_datalog::InvalidationTopic {
                indexes: vec![instant_datalog::IndexKey::Ea],
                entity: instant_datalog::TopicPart::Any,
                attribute: instant_datalog::TopicPart::Values(vec![
                    instant_datalog::DatalogValue::Uuid(attr_id),
                ]),
                value: instant_datalog::TopicPart::Any,
            };
            RuntimeFrameworkQueryTopics {
                precise: vec![dependency.clone()],
                coarse: vec![dependency],
            }
        }

        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let affected_attr = Uuid::new_v4();
        let unrelated_attr = Uuid::new_v4();
        let mut affected = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut unrelated = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [affected.session_id, unrelated.session_id] {
            service
                .message(session_id, json!({"op": "init", "app-id": app_id}))
                .await
                .unwrap();
        }
        affected.recv().await.unwrap();
        unrelated.recv().await.unwrap();

        *data.instant_topics.lock().unwrap() = Some(framework_topics(affected_attr));
        service
            .message(
                affected.session_id,
                json!({"op": "add-query", "q": {"posts": {}}}),
            )
            .await
            .unwrap();
        affected.recv().await.unwrap();
        *data.instant_topics.lock().unwrap() = Some(framework_topics(unrelated_attr));
        service
            .message(
                unrelated.session_id,
                json!({"op": "add-query", "q": {"users": {}}}),
            )
            .await
            .unwrap();
        unrelated.recv().await.unwrap();

        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service
            .apply_wal_record(&test_wal_record(
                app_id,
                vec![triple_entry(
                    WalAction::Insert,
                    wal_columns(&[
                        ("entity_id", json!(Uuid::new_v4())),
                        ("attr_id", json!(affected_attr)),
                        ("value", json!(true)),
                        ("created_at", json!(13)),
                        ("ea", json!(true)),
                    ]),
                    Vec::new(),
                )],
            ))
            .await
            .unwrap();

        assert_eq!(affected.recv().await.unwrap()["op"], "refresh-ok");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), unrelated.recv())
                .await
                .is_err()
        );

        data.query_version
            .store(2, std::sync::atomic::Ordering::Relaxed);
        service
            .apply_wal_record(&test_wal_record(
                app_id,
                vec![triple_entry(WalAction::Truncate, Vec::new(), Vec::new())],
            ))
            .await
            .unwrap();

        assert_eq!(affected.recv().await.unwrap()["op"], "refresh-ok");
        assert_eq!(unrelated.recv().await.unwrap()["op"], "refresh-ok");
    }

    fn predicate_attribute(id: Uuid, label: &str, checked: Option<DataType>) -> Attribute {
        Attribute {
            id,
            wire: None,
            etype: "users".to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: true,
            required: false,
            checked_data_type: checked,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn byop_realtime_init_and_refresh_use_the_live_external_catalog() {
        let initial_catalog = AttributeCatalog {
            attributes: vec![predicate_attribute(Uuid::new_v4(), "name", None)],
        };
        let changed_catalog = AttributeCatalog {
            attributes: vec![
                initial_catalog.attributes[0].clone(),
                predicate_attribute(Uuid::new_v4(), "email", None),
            ],
        };
        let data = Arc::new(TestData::default());
        *data.realtime_catalog.lock().unwrap() = Some(initial_catalog.clone());
        let status = Arc::new(TestEffectiveAppStatusPolicy::new(AppStatus::Active));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(AttributeCatalog::default())),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_effective_app_status_policy(Arc::clone(&status) as Arc<dyn EffectiveAppStatusPolicy>);
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();

        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        let initialized = session.recv().await.unwrap();
        assert_eq!(initialized["op"], "init-ok");
        assert_eq!(
            initialized["attrs"],
            realtime_catalog_attrs(&initial_catalog)
        );
        assert_eq!(data.realtime_catalog_calls.load(Ordering::Relaxed), 1);

        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"users": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");

        *data.realtime_catalog.lock().unwrap() = Some(changed_catalog.clone());
        data.query_version.store(1, Ordering::Relaxed);
        service
            .apply_byop_record(
                app_id,
                &instant_pg::ByopWalRecord {
                    tx_id: 101,
                    next_lsn: instant_wal::Lsn::from_u64(101),
                    changes: Vec::new(),
                },
                &[],
            )
            .await
            .unwrap();

        let refresh = session.recv().await.unwrap();
        assert_eq!(refresh["op"], "refresh-ok");
        assert_eq!(refresh["attrs"], realtime_catalog_attrs(&changed_catalog));
        assert_eq!(data.realtime_catalog_calls.load(Ordering::Relaxed), 2);

        *data.realtime_catalog_error.lock().unwrap() = Some(ApiError::new(
            ApiErrorType::Unknown,
            "catalog refresh failed",
        ));
        data.query_version.store(2, Ordering::Relaxed);
        service
            .apply_byop_record(
                app_id,
                &instant_pg::ByopWalRecord {
                    tx_id: 102,
                    next_lsn: instant_wal::Lsn::from_u64(102),
                    changes: Vec::new(),
                },
                &[],
            )
            .await
            .unwrap();
        let failed = session.recv().await.unwrap();
        assert_eq!(
            json!({
                "op": "error",
                "status": 500,
                "client-event-id": Value::Null,
                "original-event": {
                    "op": "refresh",
                    "session-id": session.session_id,
                    "processed-tx-id": 102,
                    "processed-isn": Value::Null,
                },
                "type": "unknown",
                "message": "catalog refresh failed",
                "hint": Value::Null,
            }),
            failed
        );
        assert_eq!(data.realtime_catalog_calls.load(Ordering::Relaxed), 3);

        service
            .apply_byop_record(
                app_id,
                &instant_pg::ByopWalRecord {
                    tx_id: 103,
                    next_lsn: instant_wal::Lsn::from_u64(103),
                    changes: Vec::new(),
                },
                &[],
            )
            .await
            .unwrap();
        let recovered = session.recv().await.unwrap();
        assert_eq!(recovered["op"], "refresh-ok");
        assert_eq!(
            recovered["attrs"],
            realtime_catalog_attrs(&changed_catalog),
            "legacy clients receive attrs on every emitted refresh"
        );
        assert_eq!(data.realtime_catalog_calls.load(Ordering::Relaxed), 4);

        status.set(AppStatus::Disabled);
        data.query_version.store(3, Ordering::Relaxed);
        service
            .apply_byop_record(
                app_id,
                &instant_pg::ByopWalRecord {
                    tx_id: 104,
                    next_lsn: instant_wal::Lsn::from_u64(104),
                    changes: Vec::new(),
                },
                &[],
            )
            .await
            .unwrap();
        let disabled = session.recv().await.unwrap();
        assert_eq!(
            json!({
                "op": "error",
                "status": 400,
                "client-event-id": Value::Null,
                "original-event": {
                    "op": "refresh",
                    "session-id": session.session_id,
                    "processed-tx-id": 104,
                    "processed-isn": Value::Null,
                },
                "type": "app-disabled",
                "message": "This app is currently disabled.",
                "hint": {"status": "disabled"},
            }),
            disabled
        );
        assert_eq!(data.realtime_catalog_calls.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn realtime_refresh_detects_byop_transition_and_batches_one_catalog_snapshot() {
        let instant_catalog = AttributeCatalog {
            attributes: vec![predicate_attribute(Uuid::new_v4(), "instant-name", None)],
        };
        let external_catalog = AttributeCatalog {
            attributes: vec![
                predicate_attribute(Uuid::new_v4(), "external-name", None),
                predicate_attribute(Uuid::new_v4(), "external-email", None),
            ],
        };
        let data = Arc::new(TestData::default());
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(instant_catalog.clone())),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                session.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "versions": {"@instantdb/core": "0.20.5"}
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            session.recv().await.unwrap()["attrs"],
            realtime_catalog_attrs(&instant_catalog)
        );
        for query in [json!({"users": {}}), json!({"posts": {}})] {
            service
                .message(session.session_id, json!({"op": "add-query", "q": query}))
                .await
                .unwrap();
            assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");
        }

        *data.realtime_catalog.lock().unwrap() = Some(external_catalog.clone());
        data.query_version.store(1, Ordering::Relaxed);
        service
            .apply_byop_record(
                app_id,
                &instant_pg::ByopWalRecord {
                    tx_id: 201,
                    next_lsn: instant_wal::Lsn::from_u64(201),
                    changes: Vec::new(),
                },
                &[],
            )
            .await
            .unwrap();
        let transitioned = session.recv().await.unwrap();
        assert_eq!(transitioned["op"], "refresh-ok");
        assert_eq!(transitioned["computations"].as_array().unwrap().len(), 2);
        assert_eq!(
            transitioned["attrs"],
            realtime_catalog_attrs(&external_catalog)
        );
        for computation in transitioned["computations"].as_array().unwrap() {
            assert_eq!(computation["instaql-result"][0]["catalog-size"], 2);
        }
        assert_eq!(data.realtime_batch_calls.load(Ordering::Relaxed), 1);

        data.query_version.store(2, Ordering::Relaxed);
        service
            .apply_byop_record(
                app_id,
                &instant_pg::ByopWalRecord {
                    tx_id: 202,
                    next_lsn: instant_wal::Lsn::from_u64(202),
                    changes: Vec::new(),
                },
                &[],
            )
            .await
            .unwrap();
        let unchanged_schema = session.recv().await.unwrap();
        assert_eq!(unchanged_schema["op"], "refresh-ok");
        assert!(unchanged_schema.get("attrs").is_none());
        assert_eq!(data.realtime_batch_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn realtime_init_resolves_attrs_before_app_status_and_preserves_error_envelope() {
        let app_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        *data.realtime_catalog_error.lock().unwrap() =
            Some(ApiError::new(ApiErrorType::Unknown, "init catalog failed"));
        let status = Arc::new(TestEffectiveAppStatusPolicy::new(AppStatus::Disabled));
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_effective_app_status_policy(Arc::clone(&status) as Arc<dyn EffectiveAppStatusPolicy>);
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let original = json!({
            "op": "init",
            "app-id": app_id,
            "client-event-id": "init-order"
        });
        service
            .message(session.session_id, original.clone())
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["message"], "init catalog failed");
        assert_eq!(error["client-event-id"], "init-order");
        assert_eq!(error["original-event"], original);
        assert!(status.app_ids.lock().unwrap().is_empty());
    }

    fn entity_attrs(attr_id: Uuid, value: Value) -> Value {
        Value::Object(Map::from_iter([(attr_id.to_string(), value)]))
    }

    fn framework_dependency(attr_id: Uuid) -> RuntimeFrameworkQueryTopics {
        let dependency = instant_datalog::InvalidationTopic {
            indexes: vec![instant_datalog::IndexKey::Ea],
            entity: instant_datalog::TopicPart::Any,
            attribute: instant_datalog::TopicPart::Values(vec![
                instant_datalog::DatalogValue::Uuid(attr_id),
            ]),
            value: instant_datalog::TopicPart::Any,
        };
        RuntimeFrameworkQueryTopics {
            precise: vec![dependency.clone()],
            coarse: vec![dependency],
        }
    }

    #[tokio::test]
    async fn wal_batch_reuses_catalog_for_entity_image_filtering() {
        let attr_id = Uuid::new_v4();
        let authorizer = Arc::new(CountingCatalogAuthorizer {
            catalog: AttributeCatalog {
                attributes: vec![predicate_attribute(attr_id, "status", None)],
            },
            loads: AtomicUsize::new(0),
            fail_next: AtomicBool::new(false),
        });
        let data = Arc::new(TestData::default());
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr_id));
        let service =
            RuntimeRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&data), Uuid::new_v4());
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "q":{"users":{"$":{"where":{"status":"active"}}}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        let initial_loads = authorizer.loads.load(Ordering::Relaxed);
        let initial_queries = data.query_count.load(Ordering::Relaxed);
        let records = (1..=32)
            .map(|tx_id| {
                let entity_id = Uuid::new_v4();
                let mut record = test_wal_record(
                    app_id,
                    vec![triple_entry(
                        WalAction::Insert,
                        wal_columns(&[
                            ("entity_id", json!(entity_id)),
                            ("attr_id", json!(attr_id)),
                            ("value", json!(json!("inactive").to_string())),
                            ("ea", json!(true)),
                        ]),
                        vec![],
                    )],
                );
                record.tx_id = tx_id;
                record.messages.push(entity_log_message(
                    "users",
                    entity_id,
                    &entity_attrs(attr_id, json!("inactive")),
                ));
                record
            })
            .collect::<Vec<_>>();
        service.apply_wal_records(&records).await.unwrap();
        assert_eq!(
            data.query_count.load(Ordering::Relaxed),
            initial_queries,
            "nonmatching entity images must still suppress query refresh"
        );
        assert_eq!(
            authorizer.loads.load(Ordering::Relaxed) - initial_loads,
            1,
            "a batch must not repeat the same catalog read for each entity image"
        );
        let more = records.iter().cycle().take(65).cloned().collect::<Vec<_>>();
        service.apply_wal_records(&more).await.unwrap();
        assert_eq!(
            authorizer.loads.load(Ordering::Relaxed) - initial_loads,
            4,
            "metadata must be reloaded at each bounded batch boundary"
        );
        let other_app = Uuid::new_v4();
        let mut other = service
            .open(Some(other_app), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(other.session_id, json!({"op":"init", "app-id":other_app}))
            .await
            .unwrap();
        other.recv().await.unwrap();
        service
            .message(
                other.session_id,
                json!({"op":"add-query", "q":{"users":{"$":{"where":{"status":"active"}}}}}),
            )
            .await
            .unwrap();
        other.recv().await.unwrap();
        let before = authorizer.loads.load(Ordering::Relaxed);
        let mut other_record = records[1].clone();
        other_record.app_id = other_app;
        service
            .apply_wal_records(&[records[0].clone(), other_record])
            .await
            .unwrap();
        assert_eq!(
            authorizer.loads.load(Ordering::Relaxed) - before,
            2,
            "catalog reads must remain scoped to their application"
        );
    }

    #[tokio::test]
    async fn wal_batch_reloads_catalog_after_schema_change() {
        let attr_id = Uuid::new_v4();
        let authorizer = Arc::new(CountingCatalogAuthorizer {
            catalog: AttributeCatalog {
                attributes: vec![predicate_attribute(attr_id, "status", None)],
            },
            loads: AtomicUsize::new(0),
            fail_next: AtomicBool::new(false),
        });
        let data = Arc::new(TestData::default());
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr_id));
        let service =
            RuntimeRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&data), Uuid::new_v4());
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "q":{"users":{"$":{"where":{"status":"active"}}}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        let initial_loads = authorizer.loads.load(Ordering::Relaxed);
        let initial_queries = data.query_count.load(Ordering::Relaxed);
        let records = (1..=32)
            .map(|tx_id| {
                let entity_id = Uuid::new_v4();
                let mut record = test_wal_record(
                    app_id,
                    vec![triple_entry(
                        WalAction::Insert,
                        wal_columns(&[
                            ("entity_id", json!(entity_id)),
                            ("attr_id", json!(attr_id)),
                            ("value", json!(json!("inactive").to_string())),
                            ("ea", json!(true)),
                        ]),
                        vec![],
                    )],
                );
                record.tx_id = tx_id;
                record.messages.push(entity_log_message(
                    "users",
                    entity_id,
                    &entity_attrs(attr_id, json!("inactive")),
                ));
                record
            })
            .collect::<Vec<_>>();
        let schema = refreshing_wal_record(app_id, 40, 7, 0x40);
        service
            .apply_wal_records(&[records[0].clone(), schema, records[1].clone()])
            .await
            .unwrap();
        assert_eq!(
            authorizer.loads.load(Ordering::Relaxed) - initial_loads,
            3,
            "schema change must trigger a fresh filtering catalog plus the authorized session refresh"
        );
        assert_eq!(
            data.query_count.load(Ordering::Relaxed),
            initial_queries + 1
        );
    }

    #[tokio::test]
    async fn wal_batch_retries_failed_catalog_load_without_suppressing_refresh() {
        let attr_id = Uuid::new_v4();
        let authorizer = Arc::new(CountingCatalogAuthorizer {
            catalog: AttributeCatalog {
                attributes: vec![predicate_attribute(attr_id, "status", None)],
            },
            loads: AtomicUsize::new(0),
            fail_next: AtomicBool::new(false),
        });
        let data = Arc::new(TestData::default());
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr_id));
        let service =
            RuntimeRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&data), Uuid::new_v4());
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op":"init", "app-id":app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op":"add-query", "q":{"users":{"$":{"where":{"status":"active"}}}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();
        let initial_loads = authorizer.loads.load(Ordering::Relaxed);
        let initial_queries = data.query_count.load(Ordering::Relaxed);
        let records = (1..=32)
            .map(|tx_id| {
                let entity_id = Uuid::new_v4();
                let mut record = test_wal_record(
                    app_id,
                    vec![triple_entry(
                        WalAction::Insert,
                        wal_columns(&[
                            ("entity_id", json!(entity_id)),
                            ("attr_id", json!(attr_id)),
                            ("value", json!(json!("inactive").to_string())),
                            ("ea", json!(true)),
                        ]),
                        vec![],
                    )],
                );
                record.tx_id = tx_id;
                record.messages.push(entity_log_message(
                    "users",
                    entity_id,
                    &entity_attrs(attr_id, json!("inactive")),
                ));
                record
            })
            .collect::<Vec<_>>();
        authorizer.fail_next.store(true, Ordering::Relaxed);
        service.apply_wal_records(&records).await.unwrap();
        assert_eq!(
            authorizer.loads.load(Ordering::Relaxed) - initial_loads,
            3,
            "failed filtering read must retry, with an independent read for session authorization"
        );
        assert_eq!(
            data.query_count.load(Ordering::Relaxed),
            initial_queries + 1,
            "catalog failure must conservatively refresh the affected query"
        );
    }

    #[tokio::test]
    async fn entity_images_suppress_false_positive_refreshes_but_match_before_images() {
        let attr_id = Uuid::new_v4();
        let catalog = AttributeCatalog {
            attributes: vec![predicate_attribute(attr_id, "status", None)],
        };
        let data = Arc::new(TestData::default());
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr_id));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(catalog)),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"users": {"$": {"where": {"status": "active"}}}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();

        let triple_insert = || {
            triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(entity_id)),
                    ("attr_id", json!(attr_id)),
                    ("value", json!(json!("inactive").to_string())),
                    ("ea", json!(true)),
                ]),
                Vec::new(),
            )
        };
        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service
            .apply_wal_record(&test_wal_record(app_id, vec![triple_insert()]))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-ok");

        let mut nonmatching = test_wal_record(app_id, vec![triple_insert()]);
        nonmatching.messages.push(entity_log_message(
            "users",
            entity_id,
            &entity_attrs(attr_id, json!("inactive")),
        ));
        service.apply_wal_record(&nonmatching).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), session.recv())
                .await
                .is_err()
        );

        let mut matching_before = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Update,
                wal_columns(&[
                    ("entity_id", json!(entity_id)),
                    ("attr_id", json!(attr_id)),
                    ("value", json!(json!("inactive").to_string())),
                    ("ea", json!(true)),
                ]),
                wal_columns(&[
                    ("entity_id", json!(entity_id)),
                    ("attr_id", json!(attr_id)),
                    ("value", json!(json!("active").to_string())),
                    ("ea", json!(true)),
                ]),
            )],
        );
        matching_before.messages.push(entity_log_message(
            "users",
            entity_id,
            &entity_attrs(attr_id, json!("inactive")),
        ));
        data.query_version
            .store(2, std::sync::atomic::Ordering::Relaxed);
        service.apply_wal_record(&matching_before).await.unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-ok");
    }

    #[tokio::test]
    async fn malformed_entity_date_fails_open_to_a_refresh() {
        let attr_id = Uuid::new_v4();
        let catalog = AttributeCatalog {
            attributes: vec![predicate_attribute(
                attr_id,
                "createdAt",
                Some(DataType::Date),
            )],
        };
        let data = Arc::new(TestData::default());
        *data.instant_topics.lock().unwrap() = Some(framework_dependency(attr_id));
        let service = RuntimeRealtimeService::new(
            Arc::new(CatalogAuthorizer(catalog)),
            Arc::clone(&data),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"users": {"$": {"where": {"createdAt": "2026-08-29T00:00:00Z"}}}}}),
            )
            .await
            .unwrap();
        session.recv().await.unwrap();

        let mut record = test_wal_record(
            app_id,
            vec![triple_entry(
                WalAction::Insert,
                wal_columns(&[
                    ("entity_id", json!(entity_id)),
                    ("attr_id", json!(attr_id)),
                    ("value", json!(json!("2026-08-30T00:00:00Z").to_string())),
                    ("checked_data_type", json!("date")),
                    ("ea", json!(true)),
                ]),
                Vec::new(),
            )],
        );
        record.messages.push(entity_log_message(
            "users",
            entity_id,
            &entity_attrs(attr_id, json!({"malformed": true})),
        ));
        data.query_version
            .store(1, std::sync::atomic::Ordering::Relaxed);
        service.apply_wal_record(&record).await.unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "refresh-ok");
    }

    #[test]
    fn invalidation_observability_distinguishes_removal_match_and_fail_open() {
        let attr_id = Uuid::new_v4();
        let catalog = AttributeCatalog {
            attributes: vec![predicate_attribute(
                attr_id,
                "createdAt",
                Some(DataType::Date),
            )],
        };
        let query = Map::from_iter([(
            "users".to_owned(),
            json!({"$": {"where": {"createdAt": "2026-08-29T00:00:00Z"}}}),
        )]);
        let registered = RegisteredQuery {
            invalidation_predicate: compile_query_invalidation_predicate(&query, &catalog),
            query,
            return_type: QueryReturnType::Tree,
            byop_topics: None,
            instant_topics: None,
            last_result: CachedQueryResult {
                result: Arc::new(Value::Null),
                result_meta: Arc::new(Value::Null),
            },
            stale_generation: 0,
        };
        let before = EntitySnapshot::default();
        let mut matching = EntitySnapshot::default();
        matching.entry("users".to_owned()).or_default().insert(
            Uuid::new_v4(),
            HashMap::from_iter([(attr_id, json!("2026-08-29T00:00:00Z"))]),
        );
        let mut missing = EntitySnapshot::default();
        missing.entry("users".to_owned()).or_default().insert(
            Uuid::new_v4(),
            HashMap::from_iter([(attr_id, json!("2026-08-30T00:00:00Z"))]),
        );
        let mut malformed = EntitySnapshot::default();
        malformed.entry("users".to_owned()).or_default().insert(
            Uuid::new_v4(),
            HashMap::from_iter([(attr_id, json!({"malformed": true}))]),
        );

        assert_eq!(
            query_wal_match(&registered, None, Some(&(before.clone(), matching))),
            WalQueryMatch::PredicateMatch
        );
        assert_eq!(
            query_wal_match(&registered, None, Some(&(before.clone(), missing))),
            WalQueryMatch::PredicateMiss
        );
        assert_eq!(
            query_wal_match(&registered, None, Some(&(before, malformed))),
            WalQueryMatch::PredicateFailOpen
        );

        let mut summary = InvalidationFilterSummary::default();
        for outcome in [
            WalQueryMatch::TopicMiss,
            WalQueryMatch::PredicateMiss,
            WalQueryMatch::PredicateMatch,
            WalQueryMatch::PredicateFailOpen,
            WalQueryMatch::ConservativeRefresh,
        ] {
            summary.observe(outcome);
        }
        assert_eq!(
            summary,
            InvalidationFilterSummary {
                initial_count: 5,
                topic_removed: 1,
                predicate_removed: 1,
                predicate_matched: 1,
                predicate_fail_open: 1,
                conservative_refresh: 1,
            }
        );
    }

    #[tokio::test]
    async fn coarse_topic_policy_selects_plan_dependencies_instead_of_precise_results() {
        fn dependency(attr_id: Uuid) -> instant_datalog::InvalidationTopic {
            instant_datalog::InvalidationTopic {
                indexes: vec![instant_datalog::IndexKey::Ea],
                entity: instant_datalog::TopicPart::Any,
                attribute: instant_datalog::TopicPart::Values(vec![
                    instant_datalog::DatalogValue::Uuid(attr_id),
                ]),
                value: instant_datalog::TopicPart::Any,
            }
        }

        let precise_attr = Uuid::new_v4();
        let coarse_attr = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_coarse_topics_policy(Arc::new(TestCoarseTopics(true)));
        let selected = service
            .select_query_topics(
                Uuid::new_v4(),
                &SessionQueryResult {
                    result: Arc::new(Value::Null),
                    result_meta: Arc::new(Value::Null),
                    byop_topics: None,
                    instant_topics: Some(InstantQueryTopics {
                        precise: vec![dependency(precise_attr)],
                        coarse: vec![dependency(coarse_attr)],
                    }),
                },
            )
            .await
            .unwrap()
            .unwrap();
        let invalidation = instant_wal::Topic {
            indexes: vec![instant_datalog::IndexKey::Ea],
            entities: instant_wal::MatchSet::Any,
            attributes: instant_wal::MatchSet::Values(vec![coarse_attr]),
            values: instant_wal::MatchSet::Any,
        };
        assert!(topics_intersect(&[invalidation], &selected));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn stream_protocol_supports_offsets_readers_reconnect_and_completion() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_stream_backend(Arc::new(
            InMemoryRuntimeStreamBackend::default().with_flush_limit(1),
        ));
        let app_id = Uuid::new_v4();
        let reconnect_token = Uuid::new_v4();
        let mut writer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut reader = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [writer.session_id, reader.session_id] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "__admin-token": "admin-secret"
                    }),
                )
                .await
                .unwrap();
        }
        writer.recv().await.unwrap();
        reader.recv().await.unwrap();

        service
            .message(
                writer.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "upload-7",
                    "reconnect-token": reconnect_token,
                    "client-event-id": "start"
                }),
            )
            .await
            .unwrap();
        let started = writer.recv().await.unwrap();
        assert_eq!(started["op"], "start-stream-ok");
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();

        service
            .message(
                reader.session_id,
                json!({
                    "op": "subscribe-stream",
                    "client-id": "upload-7",
                    "offset": 0,
                    "client-event-id": "subscribe-1"
                }),
            )
            .await
            .unwrap();
        let initial = reader.recv().await.unwrap();
        assert_eq!(initial["op"], "stream-append");
        assert_eq!(initial["content"], "");

        service
            .message(
                writer.session_id,
                json!({
                    "op": "append-stream",
                    "stream-id": stream_id,
                    "offset": 0,
                    "chunks": ["hé", "llo"]
                }),
            )
            .await
            .unwrap();
        let append = reader.recv().await.unwrap();
        assert_eq!(append["content"], "héllo");
        assert_eq!(append["offset"], 0);
        let flushed = writer.recv().await.unwrap();
        assert_eq!(flushed["op"], "stream-flushed");
        assert_eq!(flushed["offset"], 6);

        drop(writer);
        let disconnected = reader.recv().await.unwrap();
        assert_eq!(disconnected["error-type"], "writer-disconnected");
        assert_eq!(disconnected["retry"], true);

        let mut replacement = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(
                replacement.session_id,
                json!({
                    "op": "init",
                    "app-id": app_id,
                    "__admin-token": "admin-secret"
                }),
            )
            .await
            .unwrap();
        replacement.recv().await.unwrap();
        service
            .message(
                replacement.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "upload-7",
                    "reconnect-token": reconnect_token
                }),
            )
            .await
            .unwrap();
        assert_eq!(replacement.recv().await.unwrap()["offset"], 6);
        service
            .message(
                replacement.session_id,
                json!({
                    "op": "append-stream",
                    "stream-id": stream_id,
                    "offset": 6,
                    "chunks": ["!"],
                    "done": true,
                    "abort-reason": "finished"
                }),
            )
            .await
            .unwrap();
        assert_eq!(reader.recv().await.unwrap()["content"], "!");
        let complete = reader.recv().await.unwrap();
        assert_eq!(complete["done"], true);
        assert_eq!(complete["abort-reason"], "finished");
        let final_flush = replacement.recv().await.unwrap();
        assert_eq!(final_flush["offset"], 7);
        assert_eq!(final_flush["done"], true);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One flow proves writer fencing and reader retention.
    async fn stream_machine_eviction_fences_only_a_displaced_writer() {
        let local_machine = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            local_machine,
        );
        let app_id = Uuid::new_v4();
        let mut writer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut reader = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [writer.session_id, reader.session_id] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "__admin-token": "admin-secret"
                    }),
                )
                .await
                .unwrap();
        }
        writer.recv().await.unwrap();
        reader.recv().await.unwrap();
        service
            .message(
                writer.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "ownership-fence",
                    "reconnect-token": Uuid::new_v4()
                }),
            )
            .await
            .unwrap();
        let started = writer.recv().await.unwrap();
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();
        service
            .message(
                reader.session_id,
                json!({
                    "op": "subscribe-stream",
                    "stream-id": stream_id,
                    "client-event-id": "ownership-reader"
                }),
            )
            .await
            .unwrap();
        reader.recv().await.unwrap();

        service.apply_cache_eviction(&WalCacheEviction::StreamMachine {
            stream_id,
            machine_id: local_machine,
        });
        assert_eq!(
            stream_state(&service.inner).writers.get(&stream_id),
            Some(&writer.session_id)
        );

        service.apply_cache_eviction(&WalCacheEviction::StreamMachine {
            stream_id,
            machine_id: Uuid::new_v4(),
        });
        {
            let state = stream_state(&service.inner);
            assert!(!state.writers.contains_key(&stream_id));
            assert!(
                state
                    .readers
                    .values()
                    .any(|entry| entry.stream == stream_id)
            );
        }

        service
            .message(
                writer.session_id,
                json!({
                    "op": "append-stream",
                    "stream-id": stream_id,
                    "offset": 0,
                    "chunks": ["stale"]
                }),
            )
            .await
            .unwrap();
        let error = writer.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["message"], "Stream updated from a different machine.");

        drop(writer);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reader.recv())
                .await
                .is_err(),
            "stale writer cleanup must not disconnect readers or flush its buffer"
        );
        assert!(
            stream_state(&service.inner)
                .readers
                .values()
                .any(|entry| entry.stream == stream_id)
        );
    }

    #[tokio::test]
    async fn cluster_lag_releases_stream_readers_with_retryable_rate_limit() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut writer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut reader = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [writer.session_id, reader.session_id] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "__admin-token": "admin-secret"
                    }),
                )
                .await
                .unwrap();
        }
        writer.recv().await.unwrap();
        reader.recv().await.unwrap();
        service
            .message(
                writer.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "lagged-stream",
                    "reconnect-token": Uuid::new_v4()
                }),
            )
            .await
            .unwrap();
        let started = writer.recv().await.unwrap();
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();
        service
            .message(
                reader.session_id,
                json!({
                    "op": "subscribe-stream",
                    "stream-id": stream_id,
                    "client-event-id": "lagged-reader"
                }),
            )
            .await
            .unwrap();
        reader.recv().await.unwrap();

        deliver_stream_rate_limit(&service.inner);
        let error = reader.recv().await.unwrap();
        assert_eq!(error["error-type"], "rate-limit");
        assert_eq!(error["retry"], true);
        assert_eq!(error["client-event-id"], "lagged-reader");
        assert!(stream_state(&service.inner).readers.is_empty());
    }

    #[tokio::test]
    async fn stream_flush_ack_waits_for_the_buffer_limit() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_stream_backend(Arc::new(
            InMemoryRuntimeStreamBackend::default().with_flush_limit(5),
        ));
        let app_id = Uuid::new_v4();
        let mut writer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut reader = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [writer.session_id, reader.session_id] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "__admin-token": "admin-secret"
                    }),
                )
                .await
                .unwrap();
        }
        writer.recv().await.unwrap();
        reader.recv().await.unwrap();
        service
            .message(
                writer.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "buffered-wire",
                    "reconnect-token": Uuid::new_v4()
                }),
            )
            .await
            .unwrap();
        let started = writer.recv().await.unwrap();
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();
        service
            .message(
                reader.session_id,
                json!({
                    "op": "subscribe-stream",
                    "stream-id": stream_id,
                    "client-event-id": "buffered-reader"
                }),
            )
            .await
            .unwrap();
        reader.recv().await.unwrap();

        service
            .message(
                writer.session_id,
                json!({
                    "op": "append-stream",
                    "stream-id": stream_id,
                    "offset": 0,
                    "chunks": ["four"]
                }),
            )
            .await
            .unwrap();
        assert_eq!(reader.recv().await.unwrap()["content"], "four");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), writer.recv())
                .await
                .is_err()
        );

        service
            .message(
                writer.session_id,
                json!({
                    "op": "append-stream",
                    "stream-id": stream_id,
                    "offset": 4,
                    "chunks": ["!"]
                }),
            )
            .await
            .unwrap();
        assert_eq!(reader.recv().await.unwrap()["content"], "!");
        let flushed = writer.recv().await.unwrap();
        assert_eq!(flushed["op"], "stream-flushed");
        assert_eq!(flushed["offset"], 5);
    }

    #[tokio::test]
    async fn remote_stream_events_reach_only_local_subscribers_for_the_app() {
        let local_machine = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            local_machine,
        );
        let app_id = Uuid::new_v4();
        let mut writer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut reader = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [writer.session_id, reader.session_id] {
            service
                .message(
                    session_id,
                    json!({
                        "op": "init",
                        "app-id": app_id,
                        "__admin-token": "admin-secret"
                    }),
                )
                .await
                .unwrap();
        }
        writer.recv().await.unwrap();
        reader.recv().await.unwrap();
        service
            .message(
                writer.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "remote-writer",
                    "reconnect-token": Uuid::new_v4()
                }),
            )
            .await
            .unwrap();
        let started = writer.recv().await.unwrap();
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();
        service
            .message(
                reader.session_id,
                json!({
                    "op": "subscribe-stream",
                    "stream-id": stream_id,
                    "client-event-id": "remote-reader"
                }),
            )
            .await
            .unwrap();
        reader.recv().await.unwrap();

        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::StreamAppend {
                source_machine_id: Uuid::new_v4(),
                app_id,
                append: RuntimeStreamAppendResult {
                    stream_id,
                    client_id: "remote-writer".to_owned(),
                    offset: 0,
                    new_offset: 5,
                    content: "hello".to_owned(),
                    done: false,
                    abort_reason: None,
                    flushed_offset: None,
                },
            },
        )
        .await;
        assert_eq!(reader.recv().await.unwrap()["content"], "hello");

        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::StreamWriterDisconnected {
                source_machine_id: Uuid::new_v4(),
                app_id,
                stream_id,
            },
        )
        .await;
        assert_eq!(
            reader.recv().await.unwrap()["error-type"],
            "writer-disconnected"
        );
    }

    #[tokio::test]
    async fn routed_sse_push_executes_on_the_session_owner() {
        let machine_id = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            machine_id,
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::ServerSentEvents)
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "sse-init");
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::SsePush {
                target_machine_id: machine_id,
                app_id,
                session_id: session.session_id,
                messages: vec![json!({
                    "op": "add-query",
                    "client-event-id": "remote-sse",
                    "q": {"goals": {}}
                })],
            },
        )
        .await;
        let response = session.recv().await.unwrap();
        assert_eq!(response["op"], "add-query-ok");
        assert_eq!(response["client-event-id"], "remote-sse");
    }

    #[tokio::test]
    async fn sync_and_stream_extensions_do_not_elevate_guest_sessions() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();
        service
            .message(
                session.session_id,
                json!({"op": "start-stream", "client-id": "x", "reconnect-token": Uuid::new_v4()}),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "permission-denied");
    }

    #[tokio::test]
    async fn initialized_operations_honor_the_dynamic_app_rate_limit() {
        let data = Arc::new(TestData::default());
        let app_id = Uuid::new_v4();
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::clone(&data),
            Uuid::new_v4(),
        )
        .with_app_rate_limit_policy(Arc::new(TestAppRateLimits {
            limited_app: app_id,
        }));
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();

        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

        service
            .message(
                session.session_id,
                json!({
                    "op": "add-query",
                    "client-event-id": "limited-query",
                    "q": {"goals": {}}
                }),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["status"], 429);
        assert_eq!(error["type"], "rate-limited");
        assert_eq!(error["client-event-id"], "limited-query");
        assert_eq!(
            data.query_count.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn stream_rules_can_authorize_guest_writers_and_readers() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData {
                allow_guest_streams: true,
                ..TestData::default()
            }),
            Uuid::new_v4(),
        );
        let app_id = Uuid::new_v4();
        let mut writer = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        let mut reader = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        for session_id in [writer.session_id, reader.session_id] {
            service
                .message(session_id, json!({"op": "init", "app-id": app_id}))
                .await
                .unwrap();
        }
        writer.recv().await.unwrap();
        reader.recv().await.unwrap();
        service
            .message(
                writer.session_id,
                json!({
                    "op": "start-stream",
                    "client-id": "guest-stream",
                    "reconnect-token": Uuid::new_v4(),
                    "rule-params": {"scope": "shared"}
                }),
            )
            .await
            .unwrap();
        let started = writer.recv().await.unwrap();
        assert_eq!(started["op"], "start-stream-ok");
        service
            .message(
                reader.session_id,
                json!({
                    "op": "subscribe-stream",
                    "client-id": "guest-stream",
                    "client-event-id": "guest-reader",
                    "rule-params": {"scope": "shared"}
                }),
            )
            .await
            .unwrap();
        assert_eq!(reader.recv().await.unwrap()["op"], "stream-append");
    }

    #[test]
    fn admission_limit_flags_override_defaults_and_reject_bad_values() {
        let mut limits = RuntimeAdmissionLimits::default();
        assert!(limits.apply_config_flag("max-live-sessions-per-ip", &json!(7)));
        assert_eq!(limits.sessions_per_ip, 7);
        assert!(limits.apply_config_flag("max-subscriptions-per-session", &json!(8)));
        assert_eq!(limits.subscriptions_per_session, 8);
        assert!(limits.apply_config_flag("max-instaql-query-bytes", &json!(9)));
        assert_eq!(limits.query_json_bytes, 9);
        assert!(limits.apply_config_flag("max-sessions-per-app", &json!(10)));
        assert_eq!(limits.sessions_per_app, 10);
        assert!(!limits.apply_config_flag("max-sessions-per-app", &json!(0)));
        assert_eq!(limits.sessions_per_app, 10);
        assert!(!limits.apply_config_flag("max-live-sessions-per-ip", &json!(0)));
        assert!(!limits.apply_config_flag("max-live-sessions-per-ip", &json!("7")));
        assert!(!limits.apply_config_flag("unknown-limit", &json!(7)));
        assert_eq!(limits.sessions_per_ip, 7);
    }

    #[tokio::test]
    async fn live_session_cap_rejects_the_next_public_ip_session() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_admission_limits(RuntimeAdmissionLimits {
            sessions_per_ip: 2,
            ..RuntimeAdmissionLimits::default()
        });
        let client_ip = Some("203.0.113.10".parse::<IpAddr>().unwrap());

        let first = service
            .open_with_client(None, RuntimeTransport::WebSocket, None, client_ip)
            .await
            .unwrap();
        let second = service
            .open_with_client(None, RuntimeTransport::WebSocket, None, client_ip)
            .await
            .unwrap();
        let error = service
            .open_with_client(None, RuntimeTransport::WebSocket, None, client_ip)
            .await
            .err()
            .expect("the third public session is rejected");
        assert_eq!(error.body().kind, ApiErrorType::RateLimited);
        assert!(error.body().message.contains("limit 2"));
        drop((first, second));
    }

    #[tokio::test]
    async fn app_session_cap_rejects_an_extra_session_with_a_clear_error_op() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_admission_limits(RuntimeAdmissionLimits {
            sessions_per_app: 1,
            ..RuntimeAdmissionLimits::default()
        });
        let app_id = Uuid::new_v4();
        let mut first = service
            .open(None, RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(first.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        assert_eq!(first.recv().await.unwrap()["op"], "init-ok");

        let mut second = service
            .open(None, RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(second.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        let error = second.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "rate-limited");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("limit of 1 concurrent sessions")
        );
        drop((first, second));
    }

    #[tokio::test]
    async fn private_client_addresses_skip_the_live_session_cap() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_admission_limits(RuntimeAdmissionLimits {
            sessions_per_ip: 1,
            ..RuntimeAdmissionLimits::default()
        });
        let client_ip = Some("10.0.0.4".parse::<IpAddr>().unwrap());

        let first = service
            .open_with_client(None, RuntimeTransport::WebSocket, None, client_ip)
            .await
            .unwrap();
        let second = service
            .open_with_client(None, RuntimeTransport::WebSocket, None, client_ip)
            .await
            .unwrap();
        drop((first, second));
    }

    #[tokio::test]
    async fn subscription_cap_rejects_an_extra_query_with_a_clear_error_op() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_admission_limits(RuntimeAdmissionLimits {
            subscriptions_per_session: 1,
            ..RuntimeAdmissionLimits::default()
        });
        let app_id = Uuid::new_v4();
        let mut session = open_test_query_session(&service, app_id).await;

        service
            .message(
                session.session_id,
                json!({"op": "add-query", "q": {"todos": {}}}),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "rate-limited");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("limit of 1 active subscriptions")
        );
    }

    #[tokio::test]
    async fn query_json_size_cap_rejects_an_oversized_instaql_query() {
        let service = RuntimeRealtimeService::new(
            Arc::new(TestAuthorizer),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_admission_limits(RuntimeAdmissionLimits {
            query_json_bytes: 64,
            ..RuntimeAdmissionLimits::default()
        });
        let app_id = Uuid::new_v4();
        let mut session = service
            .open(Some(app_id), RuntimeTransport::WebSocket)
            .await
            .unwrap();
        service
            .message(session.session_id, json!({"op": "init", "app-id": app_id}))
            .await
            .unwrap();
        session.recv().await.unwrap();

        service
            .message(
                session.session_id,
                json!({
                    "op": "add-query",
                    "q": {"goals": {"$": {"where": {"name": "x".repeat(128)}}}},
                }),
            )
            .await
            .unwrap();
        let error = session.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "parameter-limit-exceeded");
        assert_eq!(error["status"], 400);
        assert!(error["message"].as_str().unwrap().contains("64 byte limit"));
    }
}
