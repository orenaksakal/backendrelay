use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::net::IpAddr;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use instant_datalog::MatchCteOptions;
use instant_http::{
    AdminActor, AdminDataBackend, AdminQueryPermissionsRequest, AdminQueryRequest,
    AdminRequestContext, AdminTransactPermissionsRequest, AdminTransactRequest,
    AdminTransactResponse, ApiError, ApiErrorType, RuntimeFrameworkBackend,
    RuntimeFrameworkQueryTopics, RuntimeFrameworkRequest, RuntimeFrameworkResponse,
};
use instant_instaql::{
    AsyncFileUrlSigner, Attribute, AttributeCatalog, ExecutedNode, Form, QueryConstraint,
    coarse_invalidation_topics, coerce_forms, collect_query_invalidation_topics, compile_query,
    normalized_query_hash, shape_response, transform_file_results_async,
    transform_file_results_async_owned,
};
use instant_permissions::{
    EvaluationBindings, EvaluationItem, InMemoryRateLimiter, PermissionAction,
    PermissionCheckScope, PermissionDataLoader, PermissionError, PermissionQueryPlanningError,
    PermissionedTransactionError, PermissionedTransactionOptions, PostgresPermissionDataLoader,
    QueryPermissionOptions, RateLimitConsumer, RefCache, RefRequest, RuleSet, audit_executed_query,
    evaluate_batch_async, execute_permissioned_transaction_with_source_order,
    filter_executed_query_owned, prepare_borrowed_query_permissions,
    prepare_owned_query_permissions,
};
use instant_realtime::QueryCacheError;
use instant_transact::{
    ExecutionError, TransactionError, TransactionOptions, TransactionReceipt, TransactionStep,
    compile_admin_transaction_with_order, execute_transaction, validate_reserved_identity_names,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_postgres::Client;
use tokio_postgres::types::{Json, Type};
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::Instrument as _;
use url::Url;
use uuid::Uuid;

use crate::db_error::query_validation_error;
use crate::postgres_conditions::postgres_condition;
use crate::postgres_pool::{DatabaseWorkload, PostgresClientSource, PostgresPoolError};
use crate::root_frame_cache::{RootFrameCaches, RootFrameKey, rules_digest};
use crate::runtime_realtime::{
    InstantQueryTopics, RuntimeJoinRowsQueryBackend, RuntimeRealtimeBatchQuery,
    RuntimeRealtimeBatchQueryResult, RuntimeRealtimeCatalogBackend, RuntimeRealtimeIdentityBackend,
    RuntimeRealtimeQueryMode, RuntimeRealtimeRefreshBatch, RuntimeStreamPermissionBackend,
    RuntimeSyncBackend, RuntimeSyncSnapshot, RuntimeSyncSnapshotRequest, RuntimeSyncSubscription,
    RuntimeSyncSubscriptionCreate, RuntimeSyncSubscriptionResume, RuntimeTreeQueryBackend,
    RuntimeTreeQueryResponse,
};
use crate::statement_cancel::StatementTracker;
use crate::{
    AdminRequestOperation, AdminRequestRateLimitPolicy, AdminTransactionQueue,
    AdminTransactionQueuePolicy, AllowAllAdminRequestRateLimits, DisabledAdminTransactionQueue,
    DisabledTransactionPoolPressure, OAuthSecretCipher, PostgresPool,
    TransactionPoolPressurePolicy,
};
use crate::{AppStatus, AppStatusFuture, AppStatusSnapshot, EffectiveAppStatusPolicy};

#[path = "framework_results.rs"]
mod framework_results;
use framework_results::{framework_cursor, framework_result};

#[path = "root_frames.rs"]
pub(crate) mod root_frames;

/// Capability requested from an admin token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminDataScope {
    AppsRead,
    AppsWrite,
    Read,
    Write,
    StorageRead,
    StorageWrite,
}

pub type ReplicaReadPolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>>;
pub type QueryPlannerPolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<MatchCteOptions, ApiError>> + Send + 'a>>;
pub type QueryModifierPolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'a>>;
pub type QuerySettingsPolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<PostgresQuerySetting>, ApiError>> + Send + 'a>>;
pub type QueryPreparationPolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PreparedInstantQuery, ApiError>> + Send + 'a>>;

/// Live routing decision for realtime query refreshes. The replica client is
/// separately forced read-only; this policy only decides whether an eligible
/// app should use it for the current refresh.
pub trait ReplicaReadPolicy: Send + Sync + 'static {
    fn enabled(&self) -> ReplicaReadPolicyFuture<'_>;
}

#[derive(Debug, Default)]
pub struct PrimaryOnlyReplicaReadPolicy;

impl ReplicaReadPolicy for PrimaryOnlyReplicaReadPolicy {
    fn enabled(&self) -> ReplicaReadPolicyFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

/// Resolves live query-planner controls before an Instant query is planned.
pub trait QueryPlannerPolicy: Send + Sync + 'static {
    fn options<'a>(&'a self, query: &'a Value) -> QueryPlannerPolicyFuture<'a>;
}

/// Resolves emergency config-app overrides for a public Instant query. The
/// supplied hash always belongs to `query`, before any modifier is applied.
pub trait QueryModifierPolicy: Send + Sync + 'static {
    fn apply<'a>(
        &'a self,
        app_id: Uuid,
        original_query_hash: i32,
        query: &'a Value,
    ) -> QueryModifierPolicyFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PostgresQuerySetting {
    pub setting: String,
    pub value: String,
}

/// Rules served to a permissioned framework query: either a cached snapshot or
/// code read under the authorization lease and compiled after it is released.
enum FrameworkRulesLoad {
    Cached(Arc<RuleSet>),
    Pending {
        code: Result<Value, ApiError>,
        read: Option<crate::control_plane_cache::ControlRead>,
    },
}

/// Resolves ordered transaction-local PostgreSQL settings for an original
/// normalized Instant query hash.
pub trait QuerySettingsPolicy: Send + Sync + 'static {
    fn settings(&self, original_query_hash: i32) -> QuerySettingsPolicyFuture<'_>;
}

/// Resolves all operational controls for one original query in a fresh read.
/// Individual policies remain available when a caller overrides one control.
pub trait QueryPreparationPolicy:
    QueryModifierPolicy + QuerySettingsPolicy + QueryPlannerPolicy
{
    /// Optionally resolves already-valid operational controls synchronously.
    /// Must never acquire a database lease, wait for a reload, use expired
    /// controls or perform side effects. None keeps the ordinary fresh path.
    fn try_prepare_no_io(
        &self,
        _app_id: Uuid,
        _original: &Value,
    ) -> Option<Result<PreparedInstantQuery, ApiError>> {
        None
    }

    /// Opts a side-effect-free, cancellation-safe policy reader into concurrent
    /// preparation before authentication completes. Custom policies preserve
    /// their original post-authorization call ordering unless they opt in.
    fn may_overlap_authorization(&self) -> bool {
        false
    }

    fn prepare<'a>(&'a self, app_id: Uuid, original: &'a Value)
    -> QueryPreparationPolicyFuture<'a>;
}

#[derive(Debug, Default)]
struct ActiveAppStatusPolicy;

impl EffectiveAppStatusPolicy for ActiveAppStatusPolicy {
    fn status(&self, _app_id: Uuid) -> AppStatusFuture<'_> {
        Box::pin(std::future::ready(Ok(AppStatusSnapshot {
            own_status: AppStatus::Active,
            effective_status: AppStatus::Active,
        })))
    }
}

#[derive(Debug, Default)]
pub struct DefaultQuerySettingsPolicy;

impl QuerySettingsPolicy for DefaultQuerySettingsPolicy {
    fn settings(&self, _original_query_hash: i32) -> QuerySettingsPolicyFuture<'_> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
}

#[derive(Debug, Default)]
pub struct DefaultQueryModifierPolicy;

impl QueryModifierPolicy for DefaultQueryModifierPolicy {
    fn apply<'a>(
        &'a self,
        _app_id: Uuid,
        _original_query_hash: i32,
        query: &'a Value,
    ) -> QueryModifierPolicyFuture<'a> {
        Box::pin(std::future::ready(Ok(query.clone())))
    }
}

/// Effective query and operational controls resolved from the original query.
#[derive(Debug)]
pub struct PreparedInstantQuery {
    pub effective: Value,
    pub original_hash: i32,
    pub planner_options: MatchCteOptions,
    pub postgres_settings: Vec<PostgresQuerySetting>,
    /// Whether query/refresh view checks may load only their compiled rule's
    /// attribute projection. On by default; the failover control toggle
    /// `disable-permission-attribute-projection` rolls it back.
    pub permission_attribute_projection: bool,
    /// Whether eligible native queries may share pre-permission root frames
    /// through the per-app frame cache. On by default; the failover control
    /// toggle `disable-shared-root-frames` keeps the single-snapshot frame path
    /// without cross-session reuse.
    pub shared_root_frames: bool,
}

impl PreparedInstantQuery {
    pub(crate) fn with_explicit_postgres_settings_precedence(mut self) -> Self {
        if self.planner_options.small_page_jit_off == Some(true)
            && self.postgres_settings.iter().any(|setting| {
                let name = setting.setting.to_ascii_lowercase();
                name == "jit" || name.starts_with("jit_")
            })
        {
            // Fresh owner/custom-policy settings take precedence over the
            // automatic statement hint, including JIT cost thresholds. Keep
            // their original ordering and values; do not issue any extra read.
            self.planner_options.small_page_jit_off = Some(false);
        }
        self
    }
}

#[tracing::instrument(name = "datalog/prepare-query", skip_all)]
pub(crate) async fn prepare_instant_query(
    preparation_policy: Option<&dyn QueryPreparationPolicy>,
    modifier_policy: &dyn QueryModifierPolicy,
    settings_policy: &dyn QuerySettingsPolicy,
    planner_policy: &dyn QueryPlannerPolicy,
    app_id: Uuid,
    original: &Value,
) -> Result<PreparedInstantQuery, ApiError> {
    if let Some(policy) = preparation_policy {
        return policy
            .prepare(app_id, original)
            .await
            .map(PreparedInstantQuery::with_explicit_postgres_settings_precedence);
    }
    let original_hash = normalized_query_hash(original)
        .map_err(|error| ApiError::new(ApiErrorType::ValidationFailed, error.to_string()))?;
    let effective = modifier_policy
        .apply(app_id, original_hash, original)
        .await?;
    let postgres_settings = settings_policy.settings(original_hash).await?;
    let planner_options = planner_policy.options(original).await?;
    Ok(PreparedInstantQuery {
        effective,
        original_hash,
        planner_options,
        postgres_settings,
        permission_attribute_projection: true,
        shared_root_frames: true,
    }
    .with_explicit_postgres_settings_precedence())
}

#[derive(Debug, Default)]
pub struct DefaultQueryPlannerPolicy;

impl QueryPlannerPolicy for DefaultQueryPlannerPolicy {
    fn options<'a>(&'a self, _query: &'a Value) -> QueryPlannerPolicyFuture<'a> {
        Box::pin(std::future::ready(Ok(MatchCteOptions::default())))
    }
}

struct QueryPolicies {
    modifier: Arc<dyn QueryModifierPolicy>,
    settings: Arc<dyn QuerySettingsPolicy>,
    planner: Arc<dyn QueryPlannerPolicy>,
    preparation: Option<Arc<dyn QueryPreparationPolicy>>,
}

impl Default for QueryPolicies {
    fn default() -> Self {
        Self {
            modifier: Arc::new(DefaultQueryModifierPolicy),
            settings: Arc::new(DefaultQuerySettingsPolicy),
            planner: Arc::new(DefaultQueryPlannerPolicy),
            preparation: None,
        }
    }
}

impl QueryPolicies {
    fn set_modifier(&mut self, policy: Arc<dyn QueryModifierPolicy>) {
        self.modifier = policy;
        self.preparation = None;
    }

    fn set_settings(&mut self, policy: Arc<dyn QuerySettingsPolicy>) {
        self.settings = policy;
        self.preparation = None;
    }

    fn set_planner(&mut self, policy: Arc<dyn QueryPlannerPolicy>) {
        self.planner = policy;
        self.preparation = None;
    }

    fn set_preparation(&mut self, policy: Arc<dyn QueryPreparationPolicy>) {
        self.modifier = policy.clone();
        self.settings = policy.clone();
        self.planner = policy.clone();
        self.preparation = Some(policy);
    }

    async fn prepare(
        &self,
        app_id: Uuid,
        original: &Value,
    ) -> Result<PreparedInstantQuery, ApiError> {
        prepare_instant_query(
            self.preparation.as_deref(),
            self.modifier.as_ref(),
            self.settings.as_ref(),
            self.planner.as_ref(),
            app_id,
            original,
        )
        .await
    }
}

/// Catalog metadata and, when explicitly prepared, routing for this refresh only.
/// The source hint is not authorization and cannot be reused by another app.
#[derive(Debug)]
pub struct RefreshCatalogSnapshot {
    /// Shared so a refresh that reads the catalog cache never copies it.
    pub catalog: Arc<AttributeCatalog>,
    pub(crate) source: Option<(Uuid, bool)>,
}

impl RefreshCatalogSnapshot {
    fn native_source_for(&self, app_id: Uuid) -> bool {
        self.source == Some((app_id, false))
    }
}

impl From<AttributeCatalog> for RefreshCatalogSnapshot {
    fn from(catalog: AttributeCatalog) -> Self {
        Self {
            catalog: Arc::new(catalog),
            source: None,
        }
    }
}

impl From<Arc<AttributeCatalog>> for RefreshCatalogSnapshot {
    fn from(catalog: Arc<AttributeCatalog>) -> Self {
        Self {
            catalog,
            source: None,
        }
    }
}

/// Authenticates an admin request and loads the application's current attributes.
pub trait AdminDataAuthorizer: Send + Sync + 'static {
    fn authorize(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send;

    fn authorize_only(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move { self.authorize(context, scope).await.map(drop) }
    }

    /// Loads the current app plus system attribute catalog without conferring
    /// admin authority. Runtime user/guest paths use this after their own auth.
    fn load_catalog(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send;

    /// Shared form of [`Self::load_catalog`] for callers that only need to read
    /// the catalog. Snapshot-cache-backed authorizers hand out one `Arc` instead
    /// of deep-cloning the catalog on every request.
    fn load_catalog_shared(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<Arc<AttributeCatalog>, ApiError>> + Send {
        async move { self.load_catalog(app_id).await.map(Arc::new) }
    }

    /// Custom authorizers keep their existing catalog behavior and do not
    /// confer a source shortcut. Production may prepare both fresh reads using
    /// one metadata lease, without sharing auth, bindings or query results.
    fn load_refresh_catalog(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<RefreshCatalogSnapshot, ApiError>> + Send {
        async move {
            self.load_catalog_shared(app_id)
                .await
                .map(RefreshCatalogSnapshot::from)
        }
    }
}

/// Live PostgreSQL implementation of the primary admin query and transaction routes.
///
/// Impersonated requests are rejected until the permission-aware execution path is
/// connected, preventing them from being silently upgraded to admin access.
pub struct AdminDataService<A> {
    client: PostgresClientSource,
    transaction_authorization_client: Option<PostgresClientSource>,
    datalog_batcher: PooledInstaqlBatcher,
    realtime_replica_client: Option<PostgresClientSource>,
    realtime_replica_datalog_batcher: Option<PooledInstaqlBatcher>,
    replica_read_policy: Arc<dyn ReplicaReadPolicy>,
    query_policies: QueryPolicies,
    app_status_policy: Arc<dyn EffectiveAppStatusPolicy>,
    admin_transaction_queue_policy: Arc<dyn AdminTransactionQueuePolicy>,
    admin_transaction_queue: Arc<AdminTransactionQueue>,
    transaction_pool_pressure_policy: Arc<dyn TransactionPoolPressurePolicy>,
    authorizer: Arc<A>,
    transaction_options: TransactionOptions,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
    admin_request_rate_limits: Arc<dyn AdminRequestRateLimitPolicy>,
    admin_request_rate_limits_enabled: bool,
    file_url_signer: Option<Arc<dyn AsyncFileUrlSigner>>,
    byop_cipher: Option<Arc<OAuthSecretCipher>>,
    byop_pools: Arc<ByopPoolRegistry>,
    refresh_source_reads: Option<crate::refresh_source_reads::RefreshSourceReads>,
    native_init_catalog_handoff: bool,
    native_sdk_typed_protocol: bool,
    native_structural_query_plans: bool,
    control_plane: Option<Arc<crate::ControlPlaneCache>>,
    root_frame_caches: Option<Arc<RootFrameCaches>>,
}

type ByopPoolRegistry = Mutex<HashMap<Uuid, Arc<ByopConnectionPool>>>;

const MAX_DATALOG_BATCH_KEYS: usize = 1_024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DatalogBatchIsolationKey {
    app_id: Uuid,
    workload: DatabaseWorkload,
    operation: &'static str,
    authorization: DatalogBatchAuthorization,
    catalog_digest: [u8; 32],
    planner_digest: [u8; 32],
    postgres_settings_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DatalogBatchAuthorization {
    Admin,
}

type InstaqlBatchLoader = instant_pg::BatchLoader<
    instant_instaql::CompiledQuery,
    Vec<ExecutedNode>,
    PooledInstaqlBatchError,
>;

fn compiled_query_executes_sql(query: &instant_instaql::CompiledQuery) -> bool {
    query.roots.iter().any(|root| !root.missing)
}

// A same-request lease is reusable only after the independent control future
// completed. Ordinary/BYOP and non-overlapping custom policies keep their old
// release-and-checkout path. No SQL result or authorization state is shared.
fn native_authorization_lease_handoff(native_only: bool, preparation_finished: bool) -> bool {
    native_only && preparation_finished
}

fn native_init_catalog_handoff_allowed(
    enabled: bool,
    app_id: Uuid,
    catalog_app_id: Uuid,
    actor: &AdminActor,
) -> bool {
    enabled && app_id == catalog_app_id && matches!(actor, AdminActor::RefreshToken(_))
}

// Call only after the request's source resolution returned native. Existing
// refresh hints keep their protocol; ordinary SDK reads require production
// opt-in. Other actors and custom assemblies keep their original path.
fn native_framework_typed_protocol(
    native_only: bool,
    workload: DatabaseWorkload,
    sdk_opt_in: bool,
    actor: &AdminActor,
) -> bool {
    (native_only && workload == DatabaseWorkload::Refresh)
        || (!native_only
            && sdk_opt_in
            && matches!(actor, AdminActor::Guest | AdminActor::RefreshToken(_)))
}

fn batch_requires_settings_transaction(
    settings: &[PostgresQuerySetting],
    queries: &[instant_instaql::CompiledQuery],
) -> bool {
    !settings.is_empty() && queries.iter().any(compiled_query_executes_sql)
}

#[derive(Debug, Error)]
enum PooledInstaqlBatchError {
    #[error(transparent)]
    Pool(#[from] PostgresPoolError),
    #[error(transparent)]
    Query(#[from] instant_pg::PgQueryError),
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
}

#[derive(Clone)]
struct PooledInstaqlBatcher {
    source: PostgresClientSource,
    loaders: Arc<StdMutex<HashMap<DatalogBatchIsolationKey, InstaqlBatchLoader>>>,
}

impl PooledInstaqlBatcher {
    fn new(source: PostgresClientSource) -> Self {
        Self {
            source,
            loaders: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn loader(
        &self,
        key: DatalogBatchIsolationKey,
        options: MatchCteOptions,
        postgres_settings: Vec<PostgresQuerySetting>,
    ) -> InstaqlBatchLoader {
        let mut loaders = self
            .loaders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(loader) = loaders.get(&key) {
            return loader.clone();
        }
        let source = self.source.for_workload(key.workload);
        let executor_key = key.clone();
        let loader = instant_pg::BatchLoader::new(move |queries| {
            let source = source.clone();
            let key = executor_key.clone();
            let options = options.clone();
            let postgres_settings = postgres_settings.clone();
            let batch_size = queries.len();
            async move {
                let mut client = source
                    .acquire_for_datalog(key.app_id, key.operation)
                    .await?;
                if !batch_requires_settings_transaction(&postgres_settings, &queries) {
                    return instant_pg::execute_instaql_requests(
                        &*client, &queries, key.app_id, &options,
                    )
                    .await
                    .map_err(PooledInstaqlBatchError::from);
                }
                let transaction = client
                    .transaction()
                    .await
                    .map_err(PooledInstaqlBatchError::from)?;
                apply_postgres_query_settings(&transaction, &postgres_settings)
                    .await
                    .map_err(PooledInstaqlBatchError::from)?;
                let results = instant_pg::execute_instaql_requests(
                    &transaction,
                    &queries,
                    key.app_id,
                    &options,
                )
                .await
                .map_err(PooledInstaqlBatchError::from)?;
                transaction
                    .rollback()
                    .await
                    .map_err(PooledInstaqlBatchError::from)?;
                Ok(results)
            }
            .instrument(tracing::info_span!(
                "datalog/execute-batch",
                app.id = %key.app_id,
                datalog.operation = key.operation,
                datalog.batch.size = batch_size,
            ))
        });
        if loaders.len() >= MAX_DATALOG_BATCH_KEYS {
            loaders.retain(|_, loader| loader.queued_len() != 0);
        }
        if loaders.len() < MAX_DATALOG_BATCH_KEYS {
            loaders.insert(key, loader.clone());
        }
        loader
    }

    async fn query(
        &self,
        app_id: Uuid,
        operation: &'static str,
        catalog: &AttributeCatalog,
        options: &MatchCteOptions,
        postgres_settings: &[PostgresQuerySetting],
        query: instant_instaql::CompiledQuery,
    ) -> Result<Vec<ExecutedNode>, ApiError> {
        let key = DatalogBatchIsolationKey {
            app_id,
            workload: DatabaseWorkload::current(),
            operation,
            authorization: DatalogBatchAuthorization::Admin,
            catalog_digest: datalog_batch_digest(catalog)?,
            planner_digest: datalog_batch_digest(options)?,
            postgres_settings_digest: datalog_batch_digest(&postgres_settings)?,
        };
        self.loader(key, options.clone(), postgres_settings.to_vec())
            .load(query)
            .await
            .map_err(datalog_batch_load_error)
    }
}

fn datalog_batch_digest(value: &impl serde::Serialize) -> Result<[u8; 32], ApiError> {
    serde_json::to_value(value)
        .and_then(|value| serde_json::to_vec(&value))
        .map(|serialized| Sha256::digest(serialized).into())
        .map_err(internal_error)
}

/// Applies source-ordered PostgreSQL query flags to the current transaction.
/// Local settings are cleared by commit/rollback before a pooled connection is
/// reused. Invalid settings deliberately leave the transaction aborted, which
/// matches the fail-closed upstream connection behavior.
///
/// # Errors
///
/// Returns the PostgreSQL error from the parameterized `set_config` statement.
pub async fn apply_postgres_query_settings(
    transaction: &tokio_postgres::Transaction<'_>,
    settings: &[PostgresQuerySetting],
) -> Result<(), tokio_postgres::Error> {
    if settings.is_empty() {
        return Ok(());
    }
    let names = settings
        .iter()
        .map(|setting| setting.setting.as_str())
        .collect::<Vec<_>>();
    let values = settings
        .iter()
        .map(|setting| setting.value.as_str())
        .collect::<Vec<_>>();
    transaction
        .query(
            "SELECT set_config(config.setting, config.value, true)
             FROM unnest($1::text[], $2::text[]) WITH ORDINALITY
                  AS config(setting, value, ord)
             ORDER BY config.ord",
            &[&names, &values],
        )
        .await?;
    Ok(())
}

fn datalog_batch_load_error(
    error: instant_pg::BatchLoadError<PooledInstaqlBatchError>,
) -> ApiError {
    match &error {
        instant_pg::BatchLoadError::Executor(executor) => match executor.as_ref() {
            PooledInstaqlBatchError::Query(instant_pg::PgQueryError::QueryTimeout { .. }) => {
                ApiError::new(ApiErrorType::Timeout, executor.to_string())
            }
            PooledInstaqlBatchError::Pool(PostgresPoolError::AppCapacity) => {
                datalog_pool_error(PostgresPoolError::AppCapacity)
            }
            PooledInstaqlBatchError::Pool(_)
            | PooledInstaqlBatchError::Query(_)
            | PooledInstaqlBatchError::Database(_) => internal_error(executor),
        },
        instant_pg::BatchLoadError::ResultCount { .. }
        | instant_pg::BatchLoadError::WorkerDropped => internal_error(error),
    }
}

struct ByopContext {
    namespace: String,
    schema: instant_pg::ByopSchema,
}

struct ConnectedByop {
    context: ByopContext,
    client: ByopClientGuard,
}

struct ByopClientGuard {
    client: Option<OwnedMutexGuard<Option<Client>>>,
    registration: Option<crate::statement_cancel::StatementRegistration>,
    pool: Weak<ByopConnectionPool>,
}

impl ByopClientGuard {
    fn new(
        client: OwnedMutexGuard<Option<Client>>,
        pool: &Arc<ByopConnectionPool>,
    ) -> Option<Self> {
        if pool.is_retired() {
            return None;
        }
        let registration = crate::statement_cancel::register_current(client.as_ref()?);
        Some(Self {
            client: Some(client),
            registration,
            pool: Arc::downgrade(pool),
        })
    }
}

impl Drop for ByopClientGuard {
    fn drop(&mut self) {
        let cancellation_requested = self
            .registration
            .as_ref()
            .is_some_and(crate::statement_cancel::StatementRegistration::cancellation_requested);
        let pool = self.pool.upgrade();
        if cancellation_requested && let Some(pool) = &pool {
            pool.retire();
        }
        // A retired generation can never be reused. Existing, unaffected
        // leases finish first and then discard their own physical sessions.
        if (cancellation_requested || pool.as_ref().is_none_or(|pool| pool.is_retired()))
            && let Some(client) = self.client.as_deref_mut()
        {
            drop(client.take());
        }
        // Unlock or destroy the session before reporting statement completion.
        drop(self.client.take());
        drop(self.registration.take());
    }
}

impl Deref for ByopClientGuard {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        self.client
            .as_deref()
            .and_then(Option::as_ref)
            .expect("BYOP client guard was already dropped")
    }
}

impl DerefMut for ByopClientGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.client
            .as_deref_mut()
            .and_then(Option::as_mut)
            .expect("BYOP client guard was already dropped")
    }
}

struct ShapedQueryResponse {
    data: Value,
    result_meta: Value,
    instant_topics: Option<InstantQueryTopics>,
}

struct PreparedAdminTransaction {
    catalog: AttributeCatalog,
    steps: Vec<TransactionStep>,
    source_steps: Vec<TransactionStep>,
    permission: Option<PreparedTransactionPermission>,
}

#[doc(hidden)]
#[derive(Debug)]
pub struct PreparedAdminQueryPermissions {
    context: AdminRequestContext,
    auth: Value,
    rules_override: Option<RuleSet>,
}

struct PreparedTransactionPermission {
    auth: Value,
    rules: Arc<RuleSet>,
    client_ip: Option<IpAddr>,
}

impl ShapedQueryResponse {
    fn from_instaql(response: instant_instaql::InstaqlResponse) -> Result<Self, ApiError> {
        let page_info = response
            .page_info
            .iter()
            .map(|(key, info)| {
                (
                    key.clone(),
                    json!({
                        "start-cursor": info.start_cursor.as_ref().map(framework_cursor),
                        "end-cursor": info.end_cursor.as_ref().map(framework_cursor),
                        "has-next-page?": info.has_next_page,
                        "has-previous-page?": info.has_previous_page,
                    }),
                )
            })
            .collect::<Map<_, _>>();
        Ok(Self {
            data: serde_json::to_value(response.data).map_err(internal_error)?,
            result_meta: json!({
                "page-info": page_info,
                "aggregate": response.aggregates,
            }),
            instant_topics: None,
        })
    }

    fn from_byop(response: instant_pg::ByopResponse) -> Result<Self, ApiError> {
        let page_info = response
            .page_info
            .iter()
            .map(|(key, info)| {
                (
                    key.clone(),
                    json!({
                        "start-cursor": info.start_cursor,
                        "end-cursor": info.end_cursor,
                    }),
                )
            })
            .collect::<Map<_, _>>();
        Ok(Self {
            data: serde_json::to_value(response.data).map_err(internal_error)?,
            result_meta: json!({
                "page-info": page_info,
                "aggregate": response.aggregates,
            }),
            instant_topics: None,
        })
    }
}

fn instant_query_topics(
    compiled: &instant_instaql::CompiledQuery,
    executed: &[ExecutedNode],
) -> Result<InstantQueryTopics, ApiError> {
    Ok(InstantQueryTopics {
        precise: collect_query_invalidation_topics(compiled, executed)
            .map_err(|error| internal_error(format!("{error:?}")))?,
        coarse: coarse_invalidation_topics(compiled)
            .map_err(|error| internal_error(format!("{error:?}")))?,
    })
}

fn framework_query_topics(topics: InstantQueryTopics) -> RuntimeFrameworkQueryTopics {
    RuntimeFrameworkQueryTopics {
        precise: topics.precise,
        coarse: topics.coarse,
    }
}

struct ByopConnectionPool {
    app_id: Uuid,
    version: [u8; 32],
    clients: Vec<Arc<Mutex<Option<Client>>>>,
    next: AtomicUsize,
    last_used: std::sync::Mutex<std::time::Instant>,
    retired: AtomicBool,
    registry: Weak<ByopPoolRegistry>,
}

impl ByopConnectionPool {
    fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    fn retire(self: &Arc<Self>) {
        if self.retired.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = Arc::clone(self);
        std::mem::drop(runtime.spawn(async move {
            let mut registry = registry.lock().await;
            if registry
                .get(&pool.app_id)
                .is_some_and(|current| Arc::ptr_eq(current, &pool))
            {
                registry.remove(&pool.app_id);
            }
        }));
    }

    async fn checkout(self: &Arc<Self>) -> Option<ByopClientGuard> {
        if self.is_retired() {
            return None;
        }
        *self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = std::time::Instant::now();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
        for offset in 0..self.clients.len() {
            let index = (start + offset) % self.clients.len();
            if let Ok(client) = Arc::clone(&self.clients[index]).try_lock_owned() {
                return ByopClientGuard::new(client, self);
            }
        }
        let client = Arc::clone(&self.clients[start]).lock_owned().await;
        ByopClientGuard::new(client, self)
    }
}

fn matching_byop_pool(
    pools: &HashMap<Uuid, Arc<ByopConnectionPool>>,
    app_id: Uuid,
    version: [u8; 32],
) -> Option<Arc<ByopConnectionPool>> {
    pools
        .get(&app_id)
        .filter(|pool| pool.version == version && !pool.is_retired())
        .cloned()
}

const BYOP_QUERY_POOL_SIZE: usize = 4;
const BYOP_QUERY_POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

impl<A> AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    #[must_use]
    pub fn new(client: Client, authorizer: Arc<A>) -> Self {
        let client = PostgresClientSource::fixed(client);
        Self {
            datalog_batcher: PooledInstaqlBatcher::new(client.clone()),
            client,
            transaction_authorization_client: None,
            realtime_replica_client: None,
            realtime_replica_datalog_batcher: None,
            replica_read_policy: Arc::new(PrimaryOnlyReplicaReadPolicy),
            query_policies: QueryPolicies::default(),
            app_status_policy: Arc::new(ActiveAppStatusPolicy),
            admin_transaction_queue_policy: Arc::new(DisabledAdminTransactionQueue),
            admin_transaction_queue: Arc::new(AdminTransactionQueue::default()),
            transaction_pool_pressure_policy: Arc::new(DisabledTransactionPoolPressure),
            authorizer,
            transaction_options: TransactionOptions {
                admin: true,
                ..TransactionOptions::default()
            },
            rate_limit_consumer: Arc::new(InMemoryRateLimiter::default()),
            admin_request_rate_limits: Arc::new(AllowAllAdminRequestRateLimits),
            admin_request_rate_limits_enabled: false,
            file_url_signer: None,
            byop_cipher: None,
            byop_pools: Arc::new(Mutex::new(HashMap::new())),
            refresh_source_reads: None,
            native_init_catalog_handoff: false,
            native_sdk_typed_protocol: false,
            native_structural_query_plans: false,
            control_plane: None,
            root_frame_caches: None,
        }
    }

    /// Creates the production service around a bounded native PostgreSQL pool.
    #[must_use]
    pub fn from_pool(pool: PostgresPool, authorizer: Arc<A>) -> Self {
        let client = PostgresClientSource::pooled(pool);
        Self {
            datalog_batcher: PooledInstaqlBatcher::new(client.clone()),
            client,
            transaction_authorization_client: None,
            realtime_replica_client: None,
            realtime_replica_datalog_batcher: None,
            replica_read_policy: Arc::new(PrimaryOnlyReplicaReadPolicy),
            query_policies: QueryPolicies::default(),
            app_status_policy: Arc::new(ActiveAppStatusPolicy),
            admin_transaction_queue_policy: Arc::new(DisabledAdminTransactionQueue),
            admin_transaction_queue: Arc::new(AdminTransactionQueue::default()),
            transaction_pool_pressure_policy: Arc::new(DisabledTransactionPoolPressure),
            authorizer,
            transaction_options: TransactionOptions {
                admin: true,
                ..TransactionOptions::default()
            },
            rate_limit_consumer: Arc::new(InMemoryRateLimiter::default()),
            admin_request_rate_limits: Arc::new(AllowAllAdminRequestRateLimits),
            admin_request_rate_limits_enabled: false,
            file_url_signer: None,
            byop_cipher: None,
            byop_pools: Arc::new(Mutex::new(HashMap::new())),
            refresh_source_reads: None,
            native_init_catalog_handoff: false,
            native_sdk_typed_protocol: false,
            native_structural_query_plans: false,
            control_plane: None,
            root_frame_caches: None,
        }
    }

    #[must_use]
    pub fn with_transaction_options(mut self, options: TransactionOptions) -> Self {
        self.transaction_options = options;
        self
    }

    /// Serves rules, session auth and source routing from WAL-invalidated
    /// snapshots, as upstream's model caches do, instead of reading them for
    /// every operation. The cache must also be registered for invalidation.
    #[must_use]
    pub fn with_control_plane_cache(mut self, cache: Arc<crate::ControlPlaneCache>) -> Self {
        self.control_plane = Some(cache);
        self
    }

    /// Rules and session auth already resolved on this machine, if both are.
    fn cached_authorization(
        &self,
        app_id: Uuid,
        actor: &AdminActor,
    ) -> Option<(Value, Arc<RuleSet>)> {
        let cache = self.control_plane.as_ref()?;
        let auth = match actor {
            AdminActor::Guest => json!({}),
            AdminActor::RefreshToken(_) | AdminActor::Email(_) => {
                cache.cached_auth(app_id, &auth_key(actor)?)?
            }
            AdminActor::Admin => return None,
        };
        Some((auth, cache.cached_rules(app_id)?))
    }

    async fn session_auth(
        &self,
        client: &Client,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        actor: &AdminActor,
    ) -> Result<Value, ApiError> {
        let (Some(cache), Some(key)) = (&self.control_plane, auth_key(actor)) else {
            return impersonated_auth(client, app_id, catalog, actor).await;
        };
        if let Some(auth) = cache.cached_auth(app_id, &key) {
            return Ok(auth);
        }
        let read = cache.begin_auth_read(app_id);
        let (auth, sources) =
            impersonated_auth_with_sources(client, app_id, catalog, actor).await?;
        cache.finish_auth_read(app_id, key, read, auth.clone(), sources);
        Ok(auth)
    }

    async fn app_rules(&self, client: &Client, app_id: Uuid) -> Result<Arc<RuleSet>, ApiError> {
        let load = || async { load_rules(client, app_id, None).await.map(Arc::new) };
        match &self.control_plane {
            Some(cache) => cache.rules(app_id, load).await,
            None => load().await,
        }
    }

    #[must_use]
    pub fn with_rate_limit_consumer(mut self, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        self.rate_limit_consumer = consumer;
        self
    }

    #[must_use]
    pub fn with_admin_request_rate_limit_policy(
        mut self,
        policy: Arc<dyn AdminRequestRateLimitPolicy>,
    ) -> Self {
        self.admin_request_rate_limits = policy;
        self.admin_request_rate_limits_enabled = true;
        self
    }

    #[must_use]
    pub fn with_query_planner_policy(mut self, policy: Arc<dyn QueryPlannerPolicy>) -> Self {
        self.query_policies.set_planner(policy);
        self
    }

    #[must_use]
    pub fn with_query_modifier_policy(mut self, policy: Arc<dyn QueryModifierPolicy>) -> Self {
        self.query_policies.set_modifier(policy);
        self
    }

    #[must_use]
    pub fn with_query_settings_policy(mut self, policy: Arc<dyn QuerySettingsPolicy>) -> Self {
        self.query_policies.set_settings(policy);
        self
    }

    /// Uses one fresh policy read per query. A later individual policy override
    /// restores separate reads while retaining the other installed controls.
    #[must_use]
    pub fn with_query_preparation_policy(
        mut self,
        policy: Arc<dyn QueryPreparationPolicy>,
    ) -> Self {
        self.query_policies.set_preparation(policy);
        self
    }

    #[must_use]
    pub fn with_app_status_policy(mut self, policy: Arc<dyn EffectiveAppStatusPolicy>) -> Self {
        self.app_status_policy = policy;
        self
    }

    /// Concurrent refreshes may share a newly executed source-routing read.
    /// External routes still perform the existing fresh connection/schema and
    /// permission resolution; later requests never reuse a completed decision.
    pub(crate) fn with_fresh_refresh_routing_reads(mut self) -> Self {
        self.refresh_source_reads = Some(crate::refresh_source_reads::reader(&self.client));
        self
    }

    /// Explicit production opt-in; custom assembly keeps its original reads.
    pub(crate) fn with_native_realtime_init_catalog_handoff(mut self) -> Self {
        self.native_init_catalog_handoff = true;
        self
    }

    /// One-exchange main SQL for SDK reads only after their fresh native lookup.
    /// Custom assembly retains the original describe-and-execute path.
    pub(crate) fn with_native_sdk_typed_protocol(mut self) -> Self {
        self.native_sdk_typed_protocol = true;
        self
    }

    /// Explicit production-only SQL-plan reuse; never cache data or decisions.
    pub(crate) fn with_native_structural_query_plans(mut self) -> Self {
        self.native_structural_query_plans = true;
        self
    }

    /// Short, fresh primary auth/rule reads must not queue behind the data work
    /// they precede. This uses an existing detached reservation, not new slots.
    /// Custom/fixed-client assembly retains its original source and ordering.
    pub(crate) fn with_transaction_authorization_pool(mut self, pool: PostgresPool) -> Self {
        self.transaction_authorization_client = Some(PostgresClientSource::pooled_read_only(pool));
        self
    }

    async fn realtime_user_id_on_catalog(
        &self,
        context: &AdminRequestContext,
        catalog: &AttributeCatalog,
    ) -> Result<Option<Uuid>, ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        // Always read the token/user now. The supplied value is metadata from
        // this init only, not an identity, token, permission or TTL cache.
        let auth = impersonated_auth(&client, context.app_id, catalog, &context.actor).await?;
        auth.get("id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(Some)
            .ok_or_else(|| internal_error("resolved realtime user is missing a UUID id"))
    }

    async fn assert_read_allowed(&self, app_id: Uuid) -> Result<(), ApiError> {
        self.app_status_policy
            .status(app_id)
            .await?
            .effective_status
            .assert_read_allowed()
    }

    #[must_use]
    pub fn with_admin_transaction_queue_policy(
        mut self,
        policy: Arc<dyn AdminTransactionQueuePolicy>,
    ) -> Self {
        self.admin_transaction_queue_policy = policy;
        self
    }

    #[must_use]
    pub fn with_transaction_pool_pressure_policy(
        mut self,
        policy: Arc<dyn TransactionPoolPressurePolicy>,
    ) -> Self {
        self.transaction_pool_pressure_policy = policy;
        self
    }

    async fn prepare_instant_query(
        &self,
        app_id: Uuid,
        original_query: &Value,
    ) -> Result<PreparedInstantQuery, ApiError> {
        self.query_policies.prepare(app_id, original_query).await
    }

    /// Adds the read-only client used only by realtime tree-query refreshes.
    /// Ordinary admin queries and every transaction remain on the primary.
    #[must_use]
    pub fn with_realtime_read_replica(
        mut self,
        client: Client,
        policy: Arc<dyn ReplicaReadPolicy>,
    ) -> Self {
        let client = PostgresClientSource::fixed(client);
        self.realtime_replica_datalog_batcher = Some(PooledInstaqlBatcher::new(client.clone()));
        self.realtime_replica_client = Some(client);
        self.replica_read_policy = policy;
        self
    }

    /// Adds a pooled replica source. Every lease is forced read-only and its
    /// session state is reset by the pool before the connection is reused.
    #[must_use]
    pub fn with_realtime_read_replica_pool(
        mut self,
        pool: PostgresPool,
        policy: Arc<dyn ReplicaReadPolicy>,
    ) -> Self {
        let client = PostgresClientSource::pooled_read_only(pool);
        self.realtime_replica_datalog_batcher = Some(PooledInstaqlBatcher::new(client.clone()));
        self.realtime_replica_client = Some(client);
        self.replica_read_policy = policy;
        self
    }

    #[must_use]
    pub fn with_file_url_signer(mut self, signer: Arc<dyn AsyncFileUrlSigner>) -> Self {
        self.file_url_signer = Some(signer);
        self
    }

    /// Shares the per-app root frame registry with the realtime service, which
    /// retires and clears it alongside the session result cache.
    #[must_use]
    pub fn with_root_frame_caches(mut self, caches: Arc<RootFrameCaches>) -> Self {
        self.root_frame_caches = Some(caches);
        self
    }

    /// Frame cache hit, miss and load counters for one app, if a registry is
    /// configured. Read-only and never clears accumulated counters.
    #[must_use]
    pub fn root_frame_cache_stats(&self, app_id: Uuid) -> Option<crate::RootFrameCacheStats> {
        self.root_frame_caches
            .as_ref()
            .map(|caches| caches.stats(app_id))
    }

    #[must_use]
    pub fn with_byop_cipher(mut self, cipher: Arc<OAuthSecretCipher>) -> Self {
        self.byop_cipher = Some(cipher);
        self
    }

    async fn shape_instant_response(
        &self,
        compiled: &instant_instaql::CompiledQuery,
        results: &[ExecutedNode],
        catalog: &AttributeCatalog,
        app_id: Uuid,
        inference: bool,
    ) -> Result<instant_instaql::InstaqlResponse, ApiError> {
        let transformed = self
            .transform_instant_file_results(compiled, results, catalog, app_id)
            .await?;
        shape_response(compiled, &transformed, catalog, inference).map_err(internal_error)
    }

    async fn transform_instant_file_results(
        &self,
        compiled: &instant_instaql::CompiledQuery,
        results: &[ExecutedNode],
        catalog: &AttributeCatalog,
        app_id: Uuid,
    ) -> Result<Vec<ExecutedNode>, ApiError> {
        match &self.file_url_signer {
            Some(signer) => {
                transform_file_results_async(compiled, results, catalog, app_id, signer.as_ref())
                    .await
                    .map_err(internal_error)
            }
            None => Ok(results.to_vec()),
        }
    }

    async fn transform_instant_file_results_owned(
        &self,
        compiled: &instant_instaql::CompiledQuery,
        results: Vec<ExecutedNode>,
        catalog: &AttributeCatalog,
        app_id: Uuid,
    ) -> Result<Vec<ExecutedNode>, ApiError> {
        match &self.file_url_signer {
            Some(signer) => transform_file_results_async_owned(
                compiled,
                results,
                catalog,
                app_id,
                signer.as_ref(),
            )
            .await
            .map_err(internal_error),
            None => Ok(results),
        }
    }

    /// Loads every root of an eligible native query through the shared per-app
    /// frame cache. Each miss gets its own repeatable-read snapshot; concurrent
    /// sessions needing the same root join that load.
    #[allow(clippy::too_many_arguments)]
    async fn shared_root_frames(
        &self,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        shared_catalog: Option<&Arc<AttributeCatalog>>,
        rules: &RuleSet,
        compiled: &instant_instaql::CompiledQuery,
        options: &MatchCteOptions,
        settings: &[PostgresQuerySetting],
        permission_attribute_projection: bool,
        external: bool,
        typed_protocol: bool,
    ) -> Result<Vec<Arc<root_frames::RootFrame>>, ApiError> {
        let Some(caches) = self.root_frame_caches.as_ref() else {
            return Err(internal_error("shared root frames are not configured"));
        };
        let cache = caches.cache(app_id).await?;
        let counters = caches.counters(app_id);
        // Refresh sessions already hold a shared catalog; only callers without
        // one (per-request paths and tests) pay for a private copy.
        let catalog = shared_catalog.map_or_else(|| Arc::new(catalog.clone()), Arc::clone);
        let catalog_identity = caches.catalog_identity(app_id, &catalog);
        let rules_identity = rules_digest(rules);
        let workload = DatabaseWorkload::current();
        let mut frames = Vec::with_capacity(compiled.roots.len());
        for root in &compiled.roots {
            let single_root = instant_instaql::CompiledQuery {
                roots: vec![root.clone()],
            };
            let key = RootFrameKey::new(
                app_id,
                catalog_identity,
                external,
                rules_identity,
                &single_root,
                options,
                settings,
                permission_attribute_projection,
            );
            if cache.contains(&key) {
                counters.hit();
            } else {
                counters.miss();
            }
            // Like the session result cache, a loader owns its own tracker
            // rather than any individual waiter's: one request timing out must
            // not cancel work another session is waiting for. Dropping the
            // loader (last waiter or invalidation) still marks its leases for
            // disposal.
            let load = || {
                let frame = load_root_frame(
                    self.client.clone(),
                    self.file_url_signer.clone(),
                    self.native_structural_query_plans,
                    typed_protocol,
                    app_id,
                    single_root.clone(),
                    Arc::clone(&catalog),
                    rules.clone(),
                    options.clone(),
                    settings.to_vec(),
                    permission_attribute_projection,
                );
                async move {
                    workload
                        .scope(StatementTracker::default().scope(frame))
                        .await
                }
            };
            // Boxed so the frame state machine never sits inline in this
            // request's future (or the loader closure the cache holds).
            let shared_load = Box::pin(load());
            let load_counters = Arc::clone(&counters);
            let loaded = match cache
                .get_or_load(key, move || async move {
                    load_counters.load();
                    shared_load.await
                })
                .await
            {
                Ok(frame) => frame,
                // A security clear, cluster-lag recovery or cache removal
                // cancelled the shared load this request joined. Load the root
                // privately instead of failing the refresh: its own snapshot is
                // at least as new as the cancelled one, and it is never
                // published to the shared cache.
                Err(QueryCacheError::Cancelled) => Arc::new(Box::pin(load()).await?),
                Err(error) => return Err(root_frame_cache_error(error)),
            };
            frames.push(loaded);
        }
        Ok(frames)
    }

    async fn create_byop_pool(
        app_id: Uuid,
        connection_string: &str,
        version: [u8; 32],
        registry: &Arc<ByopPoolRegistry>,
    ) -> Result<Arc<ByopConnectionPool>, ApiError> {
        let connections = futures_util::future::try_join_all((0..BYOP_QUERY_POOL_SIZE).map(|_| {
            let connection_string = connection_string.to_owned();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio_postgres::connect(
                        &connection_string,
                        MakeRustlsConnect::with_webpki_roots(),
                    ),
                )
                .await
                .map_err(|_| {
                    internal_error(format!(
                        "timed out connecting to BYOP database for app {app_id}"
                    ))
                })?
                .map_err(|error| {
                    internal_error(format!(
                        "could not connect to BYOP database for app {app_id}: {error}"
                    ))
                })
            }
        }))
        .await?;
        let clients = connections
            .into_iter()
            .map(|(client, connection)| {
                std::mem::drop(tokio::spawn(async move {
                    let _connection_lifetime_is_owned_by_the_client = connection.await;
                }));
                Arc::new(Mutex::new(Some(client)))
            })
            .collect();
        Ok(Arc::new(ByopConnectionPool {
            app_id,
            version,
            clients,
            next: AtomicUsize::new(0),
            last_used: std::sync::Mutex::new(std::time::Instant::now()),
            retired: AtomicBool::new(false),
            registry: Arc::downgrade(registry),
        }))
    }

    fn expire_byop_pool(
        app_id: Uuid,
        pool: &Arc<ByopConnectionPool>,
        pools: Arc<ByopPoolRegistry>,
    ) {
        let pool = Arc::downgrade(pool);
        std::mem::drop(tokio::spawn(async move {
            loop {
                tokio::time::sleep(BYOP_QUERY_POOL_IDLE_TIMEOUT).await;
                let Some(pool) = pool.upgrade() else {
                    break;
                };
                let idle_for = pool
                    .last_used
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .elapsed();
                if idle_for < BYOP_QUERY_POOL_IDLE_TIMEOUT {
                    continue;
                }
                let mut pools = pools.lock().await;
                if pools
                    .get(&app_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &pool))
                {
                    pools.remove(&app_id);
                }
                break;
            }
        }));
    }

    async fn resolve_byop_pool(
        &self,
        app_id: Uuid,
        connection_string: &str,
        version: [u8; 32],
    ) -> Result<Arc<ByopConnectionPool>, ApiError> {
        if let Some(pool) = matching_byop_pool(&*self.byop_pools.lock().await, app_id, version) {
            return Ok(pool);
        }
        let created =
            Self::create_byop_pool(app_id, connection_string, version, &self.byop_pools).await?;
        let mut pools = self.byop_pools.lock().await;
        if let Some(existing) = matching_byop_pool(&pools, app_id, version) {
            return Ok(existing);
        }
        pools.insert(app_id, Arc::clone(&created));
        drop(pools);
        Self::expire_byop_pool(app_id, &created, Arc::clone(&self.byop_pools));
        Ok(created)
    }

    async fn byop_context_if_configured(
        &self,
        client: &Client,
        app_id: Uuid,
    ) -> Result<Option<ConnectedByop>, ApiError> {
        let connection_string = client
            .query_typed_opt(
                "SELECT connection_string FROM apps WHERE id = $1 AND connection_string IS NOT NULL",
                &[(&app_id, Type::UUID)],
            )
            .await
            .map_err(internal_error)?
            .map(|row| row.get::<_, Vec<u8>>(0));
        let Some(connection_string) = connection_string else {
            self.byop_pools.lock().await.remove(&app_id);
            return Ok(None);
        };
        let cipher = self.byop_cipher.as_ref().ok_or_else(|| {
            internal_error("BYOP app found but the server has no AEAD keyset configured")
        })?;
        let connection_string = cipher
            .decrypt_bytes(app_id, &connection_string)
            .map_err(internal_error)?;
        let namespace = byop_namespace(&connection_string).map_err(internal_error)?;
        let driver_connection_string =
            byop_driver_connection_string(&connection_string).map_err(internal_error)?;
        let version = byop_connection_version(&driver_connection_string, &namespace);
        let mut pool = self
            .resolve_byop_pool(app_id, &driver_connection_string, version)
            .await?;
        let mut byop_client = None;
        for attempt in 0..2 {
            match pool.checkout().await {
                Some(client) if !client.is_closed() => {
                    byop_client = Some(client);
                    break;
                }
                client => {
                    pool.retire();
                    drop(client);
                }
            }
            if attempt == 0 {
                pool = self
                    .resolve_byop_pool(app_id, &driver_connection_string, version)
                    .await?;
            }
        }
        let byop_client = byop_client.ok_or_else(|| {
            internal_error(format!(
                "could not acquire a healthy BYOP connection for app {app_id}"
            ))
        })?;
        let schema = instant_pg::introspect_byop_schema(&*byop_client, &namespace)
            .await
            .map_err(internal_error)?;
        Ok(Some(ConnectedByop {
            context: ByopContext { namespace, schema },
            client: byop_client,
        }))
    }

    async fn query_byop_if_configured(
        &self,
        client: &Client,
        app_id: Uuid,
        query: &Value,
        inference: bool,
    ) -> Result<Option<ShapedQueryResponse>, ApiError> {
        let Some(byop) = self.byop_context_if_configured(client, app_id).await? else {
            return Ok(None);
        };
        let executed = instant_pg::execute_byop_instaql(
            &*byop.client,
            &byop.context.namespace,
            &byop.context.schema,
            query,
            true,
            inference,
        )
        .await
        .map_err(byop_query_error)?;
        ShapedQueryResponse::from_byop(executed.response).map(Some)
    }

    async fn authorize_debug(
        &self,
        context: &AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        if context.bearer_token.is_none() {
            return Err(ApiError::new(
                ApiErrorType::ParamMissing,
                "Missing parameter: [headers authorization]",
            )
            .unauthorized());
        }
        self.authorizer
            .authorize_only(context.clone(), scope)
            .await?;
        if context.actor == AdminActor::Admin {
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "Cannot test perms as admin",
            )
            .with_hint(json!({"in": "non-admin"})));
        }
        self.authorizer.load_catalog(context.app_id).await
    }

    async fn prepare_query_permissions_request(
        &self,
        context: AdminRequestContext,
        rules_override: Option<Value>,
    ) -> Result<PreparedAdminQueryPermissions, ApiError> {
        if context.bearer_token.is_none() {
            return Err(ApiError::new(
                ApiErrorType::ParamMissing,
                "Missing parameter: [headers authorization]",
            )
            .unauthorized());
        }
        self.authorizer
            .authorize_only(context.clone(), AdminDataScope::Read)
            .await?;
        if context.actor == AdminActor::Admin {
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "Cannot test perms as admin",
            )
            .with_hint(json!({"in": "non-admin"})));
        }
        let auth = match context.actor {
            AdminActor::Guest => json!({}),
            AdminActor::Email(_) | AdminActor::RefreshToken(_) => {
                let catalog = self.authorizer.load_catalog(context.app_id).await?;
                let client = self.client.acquire().await.map_err(internal_error)?;
                impersonated_auth(&client, context.app_id, &catalog, &context.actor).await?
            }
            AdminActor::Admin => unreachable!("admin actor was rejected above"),
        };
        let rules_override = rules_override.map(parse_rule_set).transpose()?;
        self.admin_request_rate_limits
            .check(context.app_id, AdminRequestOperation::Query)
            .await?;
        Ok(PreparedAdminQueryPermissions {
            context,
            auth,
            rules_override,
        })
    }

    async fn catalog_for_context(
        &self,
        context: &AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        match context.actor {
            AdminActor::Admin | AdminActor::Email(_) => {
                self.authorizer.authorize(context.clone(), scope).await
            }
            AdminActor::Guest | AdminActor::RefreshToken(_) => {
                self.authorizer.load_catalog(context.app_id).await
            }
        }
    }

    async fn catalog_for_context_shared(
        &self,
        context: &AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<Arc<AttributeCatalog>, ApiError> {
        match context.actor {
            AdminActor::Admin | AdminActor::Email(_) => Ok(Arc::new(
                self.authorizer.authorize(context.clone(), scope).await?,
            )),
            AdminActor::Guest | AdminActor::RefreshToken(_) => {
                self.authorizer.load_catalog_shared(context.app_id).await
            }
        }
    }

    async fn sync_actor_user_id(
        &self,
        context: &AdminRequestContext,
    ) -> Result<Option<Uuid>, ApiError> {
        if matches!(context.actor, AdminActor::Admin | AdminActor::Guest) {
            return Ok(None);
        }
        let catalog = self
            .catalog_for_context(context, AdminDataScope::Read)
            .await?;
        let client = self.client.acquire().await.map_err(internal_error)?;
        impersonated_auth(&client, context.app_id, &catalog, &context.actor)
            .await?
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .map(Some)
            .ok_or_else(|| internal_error("sync actor is missing a UUID id"))
    }

    async fn authorize_stream_action(
        &self,
        context: AdminRequestContext,
        action: PermissionAction,
        data: Value,
        rule_params: Value,
    ) -> Result<(), ApiError> {
        if context.actor == AdminActor::Admin {
            return Ok(());
        }
        let catalog = self
            .catalog_for_context(&context, AdminDataScope::Read)
            .await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let auth = impersonated_auth(&client, context.app_id, &catalog, &context.actor).await?;
        let rules = load_rules(&client, context.app_id, None).await?;
        let program = rules
            .program("$streams", action)
            .map_err(typed_permission_error)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorType::PermissionDenied,
                    format!("Permission denied for `$streams.{}`", action.as_str()),
                )
            })?;
        let mut bindings = EvaluationBindings {
            data,
            data_etype: Some("$streams".to_owned()),
            auth,
            rule_params,
            ..EvaluationBindings::default()
        };
        bindings.request.ip = context.client_ip.map(|ip| ip.to_string());
        rules
            .configure_rate_limits(
                context.app_id,
                &mut bindings,
                Arc::clone(&self.rate_limit_consumer),
            )
            .map_err(typed_permission_error)?;
        let loader = PostgresPermissionDataLoader {
            app_id: context.app_id,
            catalog: &catalog,
        };
        let transaction = client.transaction().await.map_err(internal_error)?;
        let mut refs = RefCache::new();
        let allowed = evaluate_batch_async(
            &[EvaluationItem {
                program: &program,
                bindings: &bindings,
            }],
            &mut refs,
            |requests| loader.load_refs(&transaction, requests),
        )
        .await
        .map_err(typed_permission_error)?[0];
        transaction.rollback().await.map_err(internal_error)?;
        if allowed {
            Ok(())
        } else {
            Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                format!("Permission denied for `$streams.{}`", action.as_str()),
            ))
        }
    }

    // One permissioned query keeps its auth and resolved client metadata explicit.
    #[allow(clippy::too_many_arguments)]
    async fn permissioned_query(
        &self,
        query_client: &PostgresClientSource,
        operation: &'static str,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        auth: Value,
        client_ip: Option<IpAddr>,
        request: AdminQueryRequest,
    ) -> Result<ShapedQueryResponse, ApiError> {
        let mut client = query_client
            .acquire_for_datalog(app_id, operation)
            .await
            .map_err(datalog_pool_error)?;
        let rules = load_rules(&client, app_id, None).await?;
        let mut raw_query = request.query;
        let rule_params = raw_query.remove("$$ruleParams").unwrap_or(Value::Null);
        let original_query = Value::Object(raw_query);
        let mut bindings = EvaluationBindings {
            auth,
            rule_params,
            ..EvaluationBindings::default()
        };
        bindings.request.ip = client_ip.map(|ip| ip.to_string());
        rules
            .configure_rate_limits(app_id, &mut bindings, Arc::clone(&self.rate_limit_consumer))
            .map_err(typed_permission_error)?;

        if let Some(mut byop) = self.byop_context_if_configured(&client, app_id).await? {
            let response = self
                .permissioned_byop_query(
                    &mut client,
                    &mut byop.client,
                    app_id,
                    catalog,
                    &byop.context,
                    &original_query,
                    &bindings,
                    &rules,
                    request.inference,
                )
                .await?;
            return ShapedQueryResponse::from_byop(response.response);
        }

        // Controls may queue on a separate pool or borrow this same pool.
        // No transaction has started; retain the resolved rules and BYOP decision
        // while releasing the data lease across that independent wait.
        drop(client);
        let prepared = self.prepare_instant_query(app_id, &original_query).await?;
        let raw_query = prepared.effective;

        let mut client = query_client
            .acquire_for_datalog(app_id, operation)
            .await
            .map_err(datalog_pool_error)?;
        // Query rows, permission entities and refs must share a snapshot: a
        // concurrent delete must not remove data between execution and filtering.
        let transaction = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(internal_error)?;
        let loader = PostgresPermissionDataLoader { app_id, catalog };
        let mut refs = RefCache::new();
        let permission_plan = rules
            .plan_query_permissions_async(&raw_query, catalog, &bindings, &mut refs, |requests| {
                loader.load_refs(&transaction, requests)
            })
            .await
            .map_err(permission_query_planning_error)?;
        let compiled = permission_plan
            .compile(&raw_query, catalog, false)
            .map_err(|error| {
                ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                    .with_hint(json!({"input": raw_query}))
            })?;
        let options = prepared.planner_options;
        if compiled_query_executes_sql(&compiled) {
            apply_postgres_query_settings(&transaction, &prepared.postgres_settings)
                .await
                .map_err(internal_error)?;
        }
        let executed = instant_pg::query_instaql(&transaction, &compiled, app_id, &options)
            .await
            .map_err(pg_query_error)?;
        let transformed = self
            .transform_instant_file_results_owned(&compiled, executed, catalog, app_id)
            .await?;
        let post_bindings = permission_plan.post_filter_bindings(&bindings);
        let filtered = filter_executed_query_owned(
            &rules,
            &compiled,
            transformed,
            catalog,
            &post_bindings,
            QueryPermissionOptions {
                attribute_projection: prepared.permission_attribute_projection,
            },
            &mut refs,
            |loads| loader.load_query_entities(&transaction, loads),
            |requests| loader.load_refs(&transaction, requests),
        )
        .await
        .map_err(typed_permission_error)?;
        let entity_counts = permission_plan
            .checked_entity_counts(&compiled, &filtered)
            .map_err(typed_permission_error)?;
        permission_plan
            .consume_rate_limits(&entity_counts, self.rate_limit_consumer.as_ref())
            .map_err(typed_permission_error)?;
        let topics = instant_query_topics(&compiled, &filtered)?;
        transaction.rollback().await.map_err(internal_error)?;
        let response = shape_response(&compiled, &filtered, catalog, request.inference)
            .map_err(internal_error)?;
        let mut response = ShapedQueryResponse::from_instaql(response)?;
        response.instant_topics = Some(topics);
        Ok(response)
    }

    /// Plans and EXPLAINs an `InstaQL` query as an impersonated app user. The
    /// transaction is read-only; no query result is shaped or returned.
    ///
    /// # Errors
    ///
    /// Returns an `ApiError` when the actor is an admin, the catalog, rules or
    /// impersonated session cannot be loaded, the query cannot be planned, or
    /// PostgreSQL cannot run the read-only `EXPLAIN`.
    pub async fn explain_instaql_as_user(
        &self,
        app_id: Uuid,
        actor: AdminActor,
        query: Map<String, Value>,
    ) -> Result<Value, ApiError> {
        if actor == AdminActor::Admin {
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "Cannot explain as admin",
            ));
        }
        let catalog = self.authorizer.load_catalog(app_id).await?;
        let mut raw_query = query;
        let rule_params = raw_query.remove("$$ruleParams").unwrap_or(Value::Null);
        let original_query = Value::Object(raw_query);
        let client = self
            .client
            .acquire_for_datalog(app_id, "explain")
            .await
            .map_err(datalog_pool_error)?;
        let auth = impersonated_auth(&client, app_id, &catalog, &actor).await?;
        let rules = load_rules(&client, app_id, None).await?;
        let mut bindings = EvaluationBindings {
            auth,
            rule_params,
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(app_id, &mut bindings, Arc::clone(&self.rate_limit_consumer))
            .map_err(typed_permission_error)?;
        // Query preparation may touch independent controls; never hold a data
        // lease across it.
        drop(client);
        self.assert_read_allowed(app_id).await?;
        let prepared = self.prepare_instant_query(app_id, &original_query).await?;
        let effective = prepared.effective;
        let mut client = self
            .client
            .acquire_for_datalog(app_id, "explain")
            .await
            .map_err(datalog_pool_error)?;
        let transaction = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(internal_error)?;
        let loader = PostgresPermissionDataLoader {
            app_id,
            catalog: &catalog,
        };
        let mut refs = RefCache::new();
        let permission_plan = rules
            .plan_query_permissions_async(&effective, &catalog, &bindings, &mut refs, |requests| {
                loader.load_refs(&transaction, requests)
            })
            .await
            .map_err(permission_query_planning_error)?;
        let compiled = permission_plan
            .compile(&effective, &catalog, false)
            .map_err(|error| {
                ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                    .with_hint(json!({"input": effective}))
            })?;
        if compiled_query_executes_sql(&compiled) {
            apply_postgres_query_settings(&transaction, &prepared.postgres_settings)
                .await
                .map_err(internal_error)?;
        }
        let analysis = instant_pg::explain_instaql(
            &transaction,
            &compiled,
            app_id,
            &prepared.planner_options,
            std::time::Duration::from_secs(10),
        )
        .await
        .map_err(pg_query_error)?;
        transaction.rollback().await.map_err(internal_error)?;
        serde_json::to_value(analysis).map_err(internal_error)
    }

    async fn permissioned_http_query_with_request_limit(
        &self,
        query_client: &PostgresClientSource,
        context: AdminRequestContext,
        request: AdminQueryRequest,
        operation: &'static str,
        rate_limit_operation: AdminRequestOperation,
    ) -> Result<ShapedQueryResponse, ApiError> {
        let app_id = context.app_id;
        let client_ip = context.client_ip;
        let auth = match context.actor {
            AdminActor::Guest => json!({}),
            AdminActor::Email(_) | AdminActor::RefreshToken(_) => {
                if matches!(context.actor, AdminActor::Email(_)) {
                    self.authorizer
                        .authorize_only(context.clone(), AdminDataScope::Read)
                        .await?;
                }
                let auth_catalog = self.authorizer.load_catalog(app_id).await?;
                let auth_client = query_client
                    .acquire_for_datalog(app_id, operation)
                    .await
                    .map_err(datalog_pool_error)?;
                impersonated_auth(&auth_client, app_id, &auth_catalog, &context.actor).await?
            }
            AdminActor::Admin => unreachable!("admin actor uses the admin query path"),
        };
        self.admin_request_rate_limits
            .check(app_id, rate_limit_operation)
            .await?;
        let catalog = self.authorizer.load_catalog(app_id).await?;
        self.assert_read_allowed(app_id).await?;
        self.permissioned_query(
            query_client,
            operation,
            app_id,
            &catalog,
            auth,
            client_ip,
            request,
        )
        .await
    }

    #[tracing::instrument(
        name = "datalog/execute",
        skip_all,
        fields(app.id = %context.app_id, datalog.operation = operation)
    )]
    async fn query_shaped_on(
        &self,
        query_client: &PostgresClientSource,
        query_batcher: &PooledInstaqlBatcher,
        context: AdminRequestContext,
        request: AdminQueryRequest,
        operation: &'static str,
        request_rate_limit: Option<AdminRequestOperation>,
    ) -> Result<ShapedQueryResponse, ApiError> {
        let app_id = context.app_id;
        validate_query_number_representations(&request.query)?;
        if self.admin_request_rate_limits_enabled
            && context.actor != AdminActor::Admin
            && let Some(rate_limit_operation) = request_rate_limit
        {
            return self
                .permissioned_http_query_with_request_limit(
                    query_client,
                    context,
                    request,
                    operation,
                    rate_limit_operation,
                )
                .await;
        }
        let catalog = if self.admin_request_rate_limits_enabled
            && context.actor == AdminActor::Admin
            && let Some(rate_limit_operation) = request_rate_limit
        {
            self.authorizer
                .authorize_only(context.clone(), AdminDataScope::Read)
                .await?;
            self.admin_request_rate_limits
                .check(app_id, rate_limit_operation)
                .await?;
            self.authorizer.load_catalog(app_id).await?
        } else {
            self.catalog_for_context(&context, AdminDataScope::Read)
                .await?
        };
        if context.actor != AdminActor::Admin {
            let client = query_client
                .acquire_for_datalog(app_id, operation)
                .await
                .map_err(datalog_pool_error)?;
            // Pinned `admin.routes/get-perms!` resolves impersonation before
            // InstaQL reaches its app-status gate. Besides preserving error
            // precedence, this prevents an invalid `as-token` from probing an
            // app's disabled state.
            let auth = impersonated_auth(&client, app_id, &catalog, &context.actor).await?;
            // Status/rate policies may borrow this same pool. Never hold a
            // lease across them: a saturated pool would wait on itself.
            drop(client);
            if let Some(operation) = request_rate_limit {
                self.admin_request_rate_limits
                    .check(app_id, operation)
                    .await?;
            }
            self.assert_read_allowed(app_id).await?;
            return self
                .permissioned_query(
                    query_client,
                    operation,
                    app_id,
                    &catalog,
                    auth,
                    context.client_ip,
                    request,
                )
                .await;
        }
        // Resolve this for every authorized logical query, before choosing or
        // touching a BYOP database. App status and the global sunset overlay
        // are live controls rather than service-start configuration.
        self.assert_read_allowed(app_id).await?;
        let original_query = Value::Object(request.query);
        let client = query_client
            .acquire_for_datalog(app_id, operation)
            .await
            .map_err(datalog_pool_error)?;
        if let Some(response) = self
            .query_byop_if_configured(&client, app_id, &original_query, request.inference)
            .await?
        {
            return Ok(response);
        }
        drop(client);
        let prepared = self.prepare_instant_query(app_id, &original_query).await?;
        let raw_query = prepared.effective;
        let compiled = compile_query(&raw_query, &catalog, true).map_err(|error| {
            ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                .with_hint(serde_json::json!({"input": raw_query}))
        })?;
        let options = prepared.planner_options;
        let executed = query_batcher
            .query(
                app_id,
                operation,
                &catalog,
                &options,
                &prepared.postgres_settings,
                compiled.clone(),
            )
            .await?;
        let topics = instant_query_topics(&compiled, &executed)?;
        let response = self
            .shape_instant_response(&compiled, &executed, &catalog, app_id, request.inference)
            .await?;
        let mut response = ShapedQueryResponse::from_instaql(response)?;
        response.instant_topics = Some(topics);
        Ok(response)
    }

    async fn query_shaped(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<ShapedQueryResponse, ApiError> {
        self.query_shaped_on(
            &self.client,
            &self.datalog_batcher,
            context,
            request,
            "admin-query",
            Some(AdminRequestOperation::Query),
        )
        .await
    }

    async fn realtime_query_shaped(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<ShapedQueryResponse, ApiError> {
        let replica_available = self.realtime_replica_client.is_some();
        let enabled = if context.app_id != crate::INSTANT_CONFIG_APP_ID && replica_available {
            self.replica_read_policy.enabled().await?
        } else {
            false
        };
        let use_replica = should_use_realtime_replica(context.app_id, replica_available, enabled);
        let (client, batcher) = match (
            use_replica,
            self.realtime_replica_client.as_ref(),
            self.realtime_replica_datalog_batcher.as_ref(),
        ) {
            (true, Some(replica), Some(batcher)) => (replica, batcher),
            _ => (&self.client, &self.datalog_batcher),
        };
        self.query_shaped_on(
            client,
            batcher,
            context,
            request,
            "realtime-tree-query",
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn permissioned_byop_query(
        &self,
        instant_client: &mut Client,
        byop_client: &mut Client,
        app_id: Uuid,
        instant_catalog: &AttributeCatalog,
        byop: &ByopContext,
        raw_query: &Value,
        bindings: &EvaluationBindings,
        rules: &RuleSet,
        inference: bool,
    ) -> Result<instant_pg::ByopExecution, ApiError> {
        let forms = coerce_forms(raw_query, &byop.schema.catalog).map_err(|error| {
            ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                .with_hint(json!({"input": raw_query}))
        })?;
        let instant_transaction = instant_client.transaction().await.map_err(internal_error)?;
        let byop_transaction = byop_client.transaction().await.map_err(internal_error)?;
        let mut refs = RefCache::new();
        let permission_plan = rules
            .plan_query_permissions_async(
                raw_query,
                &byop.schema.catalog,
                bindings,
                &mut refs,
                |requests| {
                    load_byop_permission_refs(
                        &instant_transaction,
                        &byop_transaction,
                        app_id,
                        instant_catalog,
                        byop,
                        requests,
                    )
                },
            )
            .await
            .map_err(permission_query_planning_error)?;
        let mut execution = instant_pg::execute_byop_instaql_with_constraints(
            &byop_transaction,
            &byop.namespace,
            &byop.schema,
            raw_query,
            false,
            inference,
            &permission_plan.constraints,
        )
        .await
        .map_err(byop_query_error)?;
        let post_bindings = permission_plan.post_filter_bindings(bindings);
        for form in &forms {
            let entities = execution
                .response
                .data
                .remove(&form.key)
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            let filtered = filter_byop_form(
                &instant_transaction,
                &byop_transaction,
                app_id,
                instant_catalog,
                byop,
                rules,
                form,
                entities,
                &post_bindings,
                &mut refs,
            )
            .await
            .map_err(typed_permission_error)?;
            execution
                .response
                .data
                .insert(form.key.clone(), Value::Array(filtered));
        }
        let mut counted = HashMap::new();
        for form in &forms {
            if let Some(Value::Array(entities)) = execution.response.data.get(&form.key) {
                count_byop_entities(byop, form, entities, &mut counted);
            }
        }
        let counts = counted
            .into_iter()
            .map(|(etype, ids)| {
                u64::try_from(ids.len())
                    .map(|count| (etype, count))
                    .map_err(internal_error)
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        permission_plan
            .consume_rate_limits(&counts, self.rate_limit_consumer.as_ref())
            .map_err(typed_permission_error)?;
        byop_transaction.rollback().await.map_err(internal_error)?;
        instant_transaction
            .rollback()
            .await
            .map_err(internal_error)?;
        Ok(execution)
    }

    async fn execute_realtime_byop_query(
        &self,
        instant_client: &mut Client,
        byop: &mut ConnectedByop,
        instant_catalog: &AttributeCatalog,
        request: RuntimeRealtimeBatchQuery,
        auth_and_rules: Option<&(Value, RuleSet)>,
    ) -> Result<RuntimeRealtimeBatchQueryResult, ApiError> {
        let raw_query = Value::Object(request.query);
        let (execution, effective_query) = if request.context.actor == AdminActor::Admin {
            let execution = instant_pg::execute_byop_instaql(
                &*byop.client,
                &byop.context.namespace,
                &byop.context.schema,
                &raw_query,
                true,
                false,
            )
            .await
            .map_err(byop_query_error)?;
            (execution, raw_query)
        } else {
            let (auth, rules) = auth_and_rules.ok_or_else(|| {
                internal_error("realtime BYOP permission context was not prepared")
            })?;
            let mut query = raw_query
                .as_object()
                .cloned()
                .ok_or_else(|| internal_error("realtime query is not an object"))?;
            let rule_params = query.remove("$$ruleParams").unwrap_or(Value::Null);
            let query = Value::Object(query);
            let mut bindings = EvaluationBindings {
                auth: auth.clone(),
                rule_params,
                ..EvaluationBindings::default()
            };
            bindings.request.ip = request.context.client_ip.map(|ip| ip.to_string());
            rules
                .configure_rate_limits(
                    request.context.app_id,
                    &mut bindings,
                    Arc::clone(&self.rate_limit_consumer),
                )
                .map_err(typed_permission_error)?;
            let execution = self
                .permissioned_byop_query(
                    instant_client,
                    &mut byop.client,
                    request.context.app_id,
                    instant_catalog,
                    &byop.context,
                    &query,
                    &bindings,
                    rules,
                    false,
                )
                .await?;
            (execution, query)
        };
        match request.mode {
            RuntimeRealtimeQueryMode::JoinRows => Ok(RuntimeRealtimeBatchQueryResult {
                result: byop_framework_result(
                    &byop.context,
                    &effective_query,
                    &execution.response,
                )?,
                result_meta: json!({}),
                byop_topics: Some(execution.topics),
                instant_topics: None,
            }),
            RuntimeRealtimeQueryMode::Tree => {
                let response = ShapedQueryResponse::from_byop(execution.response)?;
                Ok(RuntimeRealtimeBatchQueryResult {
                    result: response.data,
                    result_meta: response.result_meta,
                    byop_topics: None,
                    instant_topics: None,
                })
            }
        }
    }

    async fn admin_framework_query(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
        allow_replica: bool,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        let app_id = context.app_id;
        validate_query_number_representations(&request.query)?;
        let replica_available = self.realtime_replica_client.is_some();
        let enabled =
            if allow_replica && app_id != crate::INSTANT_CONFIG_APP_ID && replica_available {
                self.replica_read_policy.enabled().await?
            } else {
                false
            };
        let use_replica = should_use_realtime_replica(app_id, replica_available, enabled);
        let (query_client, query_batcher) = match (
            use_replica,
            self.realtime_replica_client.as_ref(),
            self.realtime_replica_datalog_batcher.as_ref(),
        ) {
            (true, Some(replica), Some(batcher)) => (replica, batcher),
            _ => (&self.client, &self.datalog_batcher),
        };
        let catalog = self
            .authorizer
            .authorize(context, AdminDataScope::Read)
            .await?;
        self.assert_read_allowed(app_id).await?;
        let original_query = Value::Object(request.query);
        let client = query_client
            .acquire_for_datalog(app_id, "realtime-framework-query")
            .await
            .map_err(datalog_pool_error)?;
        if let Some(byop) = self.byop_context_if_configured(&client, app_id).await? {
            let execution = instant_pg::execute_byop_instaql(
                &*byop.client,
                &byop.context.namespace,
                &byop.context.schema,
                &original_query,
                true,
                request.inference,
            )
            .await
            .map_err(byop_query_error)?;
            let result =
                byop_framework_result(&byop.context, &original_query, &execution.response)?;
            return Ok(RuntimeFrameworkResponse {
                result,
                attrs: Value::Array(
                    byop.context
                        .schema
                        .catalog
                        .attributes
                        .iter()
                        .map(framework_attr)
                        .collect(),
                ),
                byop_topics: Some(serialize_byop_topics(execution.topics)?),
                instant_topics: None,
            });
        }
        // No transaction has started. Release the data lease while live
        // controls wait so other refreshes can load their catalog and status.
        drop(client);
        let prepared = self.prepare_instant_query(app_id, &original_query).await?;
        let raw_query = prepared.effective;
        let compiled = compile_query(&raw_query, &catalog, true).map_err(|error| {
            ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                .with_hint(json!({"input": raw_query}))
        })?;
        let options = prepared.planner_options;
        let executed = query_batcher
            .query(
                app_id,
                "realtime-framework-query",
                &catalog,
                &options,
                &prepared.postgres_settings,
                compiled.clone(),
            )
            .await?;
        let transformed = self
            .transform_instant_file_results(&compiled, &executed, &catalog, app_id)
            .await?;
        let result = framework_result(&compiled.roots, &transformed);
        let instant_topics = Some(framework_query_topics(instant_query_topics(
            &compiled, &executed,
        )?));
        Ok(RuntimeFrameworkResponse {
            result,
            attrs: Value::Array(catalog.attributes.iter().map(framework_attr).collect()),
            byop_topics: None,
            instant_topics,
        })
    }

    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        name = "datalog/execute",
        skip_all,
        fields(app.id = %context.app_id, datalog.operation = "framework-query")
    )]
    async fn permissioned_framework_query(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        include_attrs: bool,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        let catalog = self
            .catalog_for_context_shared(&context, AdminDataScope::Read)
            .await?;
        self.permissioned_framework_query_on_catalog_shared(
            context,
            raw_query,
            &catalog,
            false,
            include_attrs,
        )
        .await
    }

    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        name = "datalog/execute",
        skip_all,
        fields(app.id = %context.app_id, datalog.operation = "framework-query")
    )]
    async fn permissioned_framework_query_on_catalog(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        native_only: bool,
        include_attrs: bool,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        self.permissioned_framework_query_on_catalog_with_path(
            context,
            raw_query,
            catalog,
            None,
            native_only,
            include_attrs,
            false,
            &mut false,
        )
        .await
    }

    /// Shared-catalog form used by the refresh handoff. Passing the same `Arc`
    /// into frame loaders avoids a full catalog copy per session.
    async fn permissioned_framework_query_on_catalog_shared(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &Arc<AttributeCatalog>,
        native_only: bool,
        include_attrs: bool,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        self.permissioned_framework_query_on_catalog_with_path(
            context,
            raw_query,
            catalog,
            Some(catalog),
            native_only,
            include_attrs,
            false,
            &mut false,
        )
        .await
    }

    /// Runs the permissioned framework query through both implementations and
    /// reports whether the frame path was eligible.
    ///
    /// Differential verification only: production requests always choose the
    /// path themselves. The frame side runs with the refresh workload, which is
    /// the only workload that may reuse shared frames.
    #[doc(hidden)]
    #[allow(clippy::too_many_lines)]
    pub async fn differential_native_framework_query(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        include_attrs: bool,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        DatabaseWorkload::Refresh
            .scope(self.differential_native_framework_query_on_workload(
                context,
                raw_query,
                catalog,
                include_attrs,
            ))
            .await
    }

    /// Differential verification for a coarse-selected refresh batch.
    ///
    /// The refresh workload runs with shared frame reuse disabled, exactly as
    /// `execute_pending_refresh` does when the coarse-topics flag selects a
    /// batch: the per-request frames must still produce the response and the
    /// shared cache must stay untouched.
    #[doc(hidden)]
    pub async fn differential_coarse_native_framework_query(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        include_attrs: bool,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        DatabaseWorkload::Refresh
            .scope(crate::root_frame_cache::without_shared_root_frame_reuse(
                self.differential_native_framework_query_on_workload(
                    context,
                    raw_query,
                    catalog,
                    include_attrs,
                ),
            ))
            .await
    }

    /// Runs only the production path of a subscription refresh and reports
    /// whether it used root frames.
    ///
    /// Verification only: lets a test hold the frame load in flight (for
    /// example behind a table lock) without first running the existing path,
    /// which would block on the same lock.
    #[doc(hidden)]
    pub async fn refresh_native_framework_query(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        include_attrs: bool,
    ) -> (Result<RuntimeFrameworkResponse, ApiError>, bool) {
        let mut used_root_frames = false;
        let result = DatabaseWorkload::Refresh
            .scope(self.permissioned_framework_query_on_catalog_with_path(
                context,
                raw_query,
                catalog,
                None,
                true,
                include_attrs,
                false,
                &mut used_root_frames,
            ))
            .await;
        (result, used_root_frames)
    }

    /// Differential verification for an initial add-query execution.
    ///
    /// The frame split is eligible, but the shared cache must stay untouched and
    /// the per-request frames must produce the same response.
    #[doc(hidden)]
    #[allow(clippy::too_many_lines)]
    pub async fn differential_initial_native_framework_query(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        include_attrs: bool,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        self.differential_native_framework_query_on_workload(
            context,
            raw_query,
            catalog,
            include_attrs,
        )
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn differential_native_framework_query_on_workload(
        &self,
        context: AdminRequestContext,
        raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        include_attrs: bool,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        let existing = self
            .permissioned_framework_query_on_catalog_with_path(
                context.clone(),
                raw_query.clone(),
                catalog,
                None,
                true,
                include_attrs,
                true,
                &mut false,
            )
            .await;
        let mut used_root_frames = false;
        let frame_path = self
            .permissioned_framework_query_on_catalog_with_path(
                context,
                raw_query,
                catalog,
                None,
                true,
                include_attrs,
                false,
                &mut used_root_frames,
            )
            .await;
        (existing, frame_path, used_root_frames)
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    async fn permissioned_framework_query_on_catalog_with_path(
        &self,
        context: AdminRequestContext,
        mut raw_query: Map<String, Value>,
        catalog: &AttributeCatalog,
        shared_catalog: Option<&Arc<AttributeCatalog>>,
        native_only: bool,
        include_attrs: bool,
        force_existing_path: bool,
        used_root_frames: &mut bool,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        use crate::query_timing::{QueryStage, QueryTiming};
        let refresh_workload = DatabaseWorkload::current() == DatabaseWorkload::Refresh;
        let timing = QueryTiming::new(native_only && refresh_workload);
        let app_id = context.app_id;
        validate_query_number_representations(&raw_query)?;
        timing
            .measure(QueryStage::Status, self.assert_read_allowed(app_id))
            .await?;
        let rule_params = raw_query.remove("$$ruleParams").unwrap_or(Value::Null);
        let original_query = Value::Object(raw_query);
        let preparation_finished = AtomicBool::new(false);
        let client_ip = context.client_ip;
        let overlap = native_only
            && self
                .query_policies
                .preparation
                .as_ref()
                .is_some_and(|policy| policy.may_overlap_authorization());
        // Rules code is read under the authorization lease but compiled only
        // after that lease is released, so validation never blocks refreshes
        // that need the same pool connection.
        let authorize = async {
            // Warm rules and session auth need no lease at all. Otherwise
            // pipeline both reads, preserving auth-error priority. Never open
            // a BYOP connection or execute application SQL before authorization.
            let (client, auth, rules) =
                if let Some((auth, rules)) = self.cached_authorization(app_id, &context.actor) {
                    (None, auth, FrameworkRulesLoad::Cached(rules))
                } else {
                    let client = timing
                        .measure(
                            QueryStage::AuthorizationCheckout,
                            self.client.acquire_for_datalog(app_id, "framework-query"),
                        )
                        .await
                        .map_err(datalog_pool_error)?;
                    if let Some(rules) = self
                        .control_plane
                        .as_ref()
                        .and_then(|cache| cache.cached_rules(app_id))
                    {
                        let auth = timing
                            .measure(
                                QueryStage::AuthorizationRead,
                                self.session_auth(&client, app_id, catalog, &context.actor),
                            )
                            .await;
                        (Some(client), auth?, FrameworkRulesLoad::Cached(rules))
                    } else {
                        let read = self
                            .control_plane
                            .as_ref()
                            .map(|cache| cache.begin_rules_read(app_id));
                        let (auth, code) = timing
                            .measure(QueryStage::AuthorizationRead, async {
                                tokio::join!(
                                    self.session_auth(&client, app_id, catalog, &context.actor),
                                    load_rule_code(&client, app_id, None),
                                )
                            })
                            .await;
                        (
                            Some(client),
                            auth?,
                            FrameworkRulesLoad::Pending { code, read },
                        )
                    }
                };
            // Production overlap stays withdrawn. A warm operational snapshot
            // can instead finish preparation synchronously after fresh auth,
            // while this same request still owns its data lease. A miss never
            // starts a reload or holds the lease across a control wait.
            let preparation_started = std::time::Instant::now();
            let ready_preparation = if native_only && !overlap {
                self.query_policies
                    .preparation
                    .as_ref()
                    .and_then(|policy| policy.try_prepare_no_io(app_id, &original_query))
            } else {
                None
            };
            if ready_preparation.is_some() {
                timing.record(QueryStage::Preparation, preparation_started.elapsed());
            }
            let mut client = client;
            if native_only
                && client.is_some()
                && !native_authorization_lease_handoff(
                    native_only,
                    ready_preparation.is_some() || preparation_finished.load(Ordering::Acquire),
                )
            {
                // Never occupy a data connection while control preparation is
                // pending. If it already completed, retain this same request's
                // lease instead of adding another checkout/queue round trip.
                drop(client.take());
            }
            let rules = match rules {
                FrameworkRulesLoad::Cached(rules) => rules,
                FrameworkRulesLoad::Pending { code, read } => {
                    let rules = Arc::new(parse_rule_set(code?)?);
                    if let (Some(cache), Some(read)) = (&self.control_plane, read) {
                        cache.finish_rules_read(app_id, read, Arc::clone(&rules));
                    }
                    rules
                }
            };
            let mut bindings = EvaluationBindings {
                auth,
                rule_params,
                ..EvaluationBindings::default()
            };
            bindings.request.ip = client_ip.map(|ip| ip.to_string());
            rules
                .configure_rate_limits(app_id, &mut bindings, Arc::clone(&self.rate_limit_consumer))
                .map_err(typed_permission_error)?;
            Ok::<_, ApiError>((client, rules, bindings, ready_preparation))
        };
        let ((mut client, rules, bindings, ready_preparation), prepared) = if overlap {
            // Fresh native controls use a detached reservation and can wait in
            // parallel with fresh authorization. Defer control errors until
            // authorization succeeds; try_join cancels controls immediately on
            // auth failure. Both futures stay inside shared admission/cancellation.
            let (authorization, prepared) = tokio::try_join!(authorize, async {
                let prepared = timing
                    .measure(
                        QueryStage::Preparation,
                        self.prepare_instant_query(app_id, &original_query),
                    )
                    .await;
                preparation_finished.store(true, Ordering::Release);
                Ok::<_, ApiError>(prepared)
            })?;
            (authorization, Some(prepared))
        } else {
            (authorize.await?, None)
        };
        // A source known to be native skips the routing read; a cold or
        // external route reads it on this request's own lease.
        if !native_only
            && self
                .control_plane
                .as_ref()
                .and_then(|cache| cache.cached_external_source(app_id))
                != Some(false)
        {
            if client.is_none() {
                client = Some(
                    timing
                        .measure(
                            QueryStage::AuthorizationCheckout,
                            self.client.acquire_for_datalog(app_id, "framework-query"),
                        )
                        .await
                        .map_err(datalog_pool_error)?,
                );
            }
            let routing = self
                .control_plane
                .as_ref()
                .map(|cache| cache.begin_external_source_read(app_id));
            let byop = self
                .byop_context_if_configured(
                    client
                        .as_ref()
                        .expect("unresolved source retains its lease"),
                    app_id,
                )
                .await?;
            if let (Some(cache), Some(routing)) = (&self.control_plane, routing) {
                cache.finish_external_source_read(app_id, routing, byop.is_some());
            }
            if let Some(mut byop) = byop {
                let response = self
                    .permissioned_byop_query(
                        client.as_mut().expect("BYOP source retains its lease"),
                        &mut byop.client,
                        app_id,
                        catalog,
                        &byop.context,
                        &original_query,
                        &bindings,
                        &rules,
                        false,
                    )
                    .await?;
                let result =
                    byop_framework_result(&byop.context, &original_query, &response.response)?;
                return Ok(RuntimeFrameworkResponse {
                    result,
                    attrs: Value::Array(
                        byop.context
                            .schema
                            .catalog
                            .attributes
                            .iter()
                            .map(framework_attr)
                            .collect(),
                    ),
                    byop_topics: Some(serialize_byop_topics(response.topics)?),
                    instant_topics: None,
                });
            }
        }
        // Ordinary SDK reads reach here only after their own fresh source
        // lookup ruled out BYOP. They can use the same no-I/O native handoff;
        // they must not unconditionally release and queue again while controls
        // are already ready. External routes returned above, and custom/cold
        // controls still release before any asynchronous preparation.
        let ready_preparation = if !native_only {
            let ready = self
                .query_policies
                .preparation
                .as_ref()
                .and_then(|policy| policy.try_prepare_no_io(app_id, &original_query));
            if ready.is_none() {
                drop(client.take());
            }
            ready
        } else {
            ready_preparation
        };
        let prepared = match prepared {
            Some(prepared) => prepared?,
            None => match ready_preparation {
                Some(prepared) => prepared?,
                None => {
                    timing
                        .measure(
                            QueryStage::Preparation,
                            self.prepare_instant_query(app_id, &original_query),
                        )
                        .await?
                }
            },
        };
        let raw_query = prepared.effective;
        let retained_authorization_lease = client.is_some();
        let mut client = match client {
            Some(client) => client,
            None => timing
                .measure(
                    QueryStage::DataCheckout,
                    self.client.acquire_for_datalog(app_id, "framework-query"),
                )
                .await
                .map_err(datalog_pool_error)?,
        };
        // Query rows, permission entities and refs must share a snapshot: a
        // concurrent delete must not remove data between execution and filtering.
        let statement_cache = if self.native_structural_query_plans {
            client.native_statement_cache()
        } else {
            None
        };
        // Reuse the physical connection's cached plan for the permission
        // entity read. Preparation is skipped on fixed-client lanes and a
        // failed prepare falls back to the text path rather than failing the
        // query; execution still binds fresh entity IDs inside this request's
        // snapshot.
        let permission_entity_statement = if statement_cache.is_some() {
            client
                .prepare_typed_cached(
                    instant_permissions::PERMISSION_ENTITY_SQL,
                    instant_permissions::permission_entity_parameter_types(),
                )
                .await
                .ok()
        } else {
            None
        };
        let permission_projection_statement = if statement_cache.is_some() {
            client
                .prepare_typed_cached(
                    instant_permissions::PERMISSION_ENTITY_PROJECTED_SQL,
                    instant_permissions::permission_entity_projected_parameter_types(),
                )
                .await
                .ok()
        } else {
            None
        };
        let transaction = timing
            .measure(QueryStage::SnapshotBegin, async {
                client
                    .build_transaction()
                    .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                    .read_only(true)
                    .start()
                    .await
            })
            .await
            .map_err(internal_error)?;
        let loader = PostgresPermissionDataLoader { app_id, catalog };
        let mut refs = RefCache::new();
        let planning_started = std::time::Instant::now();
        let permission_plan = rules
            .plan_query_permissions_async(&raw_query, catalog, &bindings, &mut refs, |requests| {
                loader.load_refs(&transaction, requests)
            })
            .instrument(tracing::info_span!("permissions/plan-query"))
            .await
            .map_err(permission_query_planning_error)?;
        let compiled = {
            let _span = tracing::info_span!("permissions/compile-query").entered();
            permission_plan
                .compile(&raw_query, catalog, false)
                .map_err(|error| {
                    ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                        .with_hint(json!({"input": raw_query}))
                })?
        };
        let options = prepared.planner_options;
        timing.record(QueryStage::PermissionPlan, planning_started.elapsed());
        // Decide before executing SQL. The frame path only serves the native,
        // non-admin, callback-free, rate-limit-free query shape; everything else
        // keeps the existing snapshot-backed loader path unchanged.
        let use_root_frames = !force_existing_path
            && root_frames::eligible(
                native_only,
                &context.actor,
                permission_plan.rate_limits.is_empty(),
                compiled_query_executes_sql(&compiled),
                &rules,
                &compiled,
            );
        *used_root_frames = use_root_frames;
        let typed_protocol = native_framework_typed_protocol(
            native_only,
            DatabaseWorkload::current(),
            self.native_sdk_typed_protocol,
            &context.actor,
        );
        // Shared frame path: the planning transaction saw a fresh snapshot, but
        // every frame load owns its own snapshot and connection. Release the
        // planning lease first so frame loads cannot queue behind this request.
        // Only subscription refreshes reuse frames across sessions; initial
        // add-query executions use the per-request split below, so a one-off
        // read cannot create an entry nobody will look up again.
        if use_root_frames
            && refresh_workload
            && prepared.shared_root_frames
            && self.root_frame_caches.is_some()
            && !crate::root_frame_cache::shared_root_frame_reuse_disabled()
        {
            let snapshot_end = transaction.rollback().await;
            drop(client);
            let frames_started = std::time::Instant::now();
            let frames = self
                .shared_root_frames(
                    app_id,
                    catalog,
                    shared_catalog,
                    &rules,
                    &compiled,
                    &options,
                    &prepared.postgres_settings,
                    prepared.permission_attribute_projection,
                    !native_only,
                    typed_protocol,
                )
                .await;
            // Frame acquisition includes cache waits and any per-root SQL. The
            // SnapshotBegin stage measured the planning snapshot; each miss's
            // own snapshot begins inside this interval. QuerySettings is applied
            // inside each miss rather than on the planning transaction, so that
            // stage remains zero on this path.
            timing.record(QueryStage::QuerySql, frames_started.elapsed());
            let frames = frames?;
            // Preserve frame errors over a rollback failure, as the existing
            // path preserves permission errors over snapshot close failures.
            snapshot_end.map_err(internal_error)?;
            let post_bindings = permission_plan.post_filter_bindings(&bindings);
            let permission_options = QueryPermissionOptions {
                attribute_projection: prepared.permission_attribute_projection,
            };
            // Evaluate each root only against its own frame. Entity data is
            // never merged across frames: they may come from different
            // snapshots or rule-attribute projections.
            let single_roots = compiled
                .roots
                .iter()
                .cloned()
                .map(|root| instant_instaql::CompiledQuery { roots: vec![root] })
                .collect::<Vec<_>>();
            let mut prepared_roots = Vec::with_capacity(single_roots.len());
            for (single_root, frame) in single_roots.iter().zip(&frames) {
                // Frames are shared by every session: prepare against the
                // frame's own tree and entity map instead of deep-copying both
                // into this request.
                prepared_roots.push(
                    tracing::info_span!("permissions/prepare-root-frame")
                        .in_scope(|| {
                            prepare_borrowed_query_permissions(
                                &rules,
                                single_root,
                                &frame.nodes,
                                catalog,
                                &post_bindings,
                                &frame.entity_data,
                                permission_options,
                            )
                        })
                        .map_err(typed_permission_error)?,
                );
            }
            let filtering_started = std::time::Instant::now();
            let mut filtered = Vec::new();
            for root_permissions in prepared_roots {
                filtered.extend(
                    root_permissions
                        .filter(&mut refs, |_| async {
                            Err(PermissionError::Loader(
                                "CPU-only permission pass requested a reference load".into(),
                            ))
                        })
                        .instrument(tracing::info_span!("permissions/filter-root-frame"))
                        .await
                        .map_err(typed_permission_error)?,
                );
            }
            timing.record(QueryStage::PermissionFilter, filtering_started.elapsed());
            let finishing_started = std::time::Instant::now();
            let counts = permission_plan
                .checked_entity_counts(&compiled, &filtered)
                .map_err(typed_permission_error)?;
            permission_plan
                .consume_rate_limits(&counts, self.rate_limit_consumer.as_ref())
                .map_err(typed_permission_error)?;
            let instant_topics = Some(framework_query_topics(instant_query_topics(
                &compiled, &filtered,
            )?));
            let result = framework_result(&compiled.roots, &filtered);
            timing.record(QueryStage::ResultFinish, finishing_started.elapsed());
            timing.finish(retained_authorization_lease);
            return Ok(RuntimeFrameworkResponse {
                result,
                attrs: if include_attrs {
                    Value::Array(catalog.attributes.iter().map(framework_attr).collect())
                } else {
                    Value::Null
                },
                byop_topics: None,
                instant_topics,
            });
        }
        if compiled_query_executes_sql(&compiled) {
            timing
                .measure(
                    QueryStage::QuerySettings,
                    apply_postgres_query_settings(&transaction, &prepared.postgres_settings),
                )
                .await
                .map_err(internal_error)?;
        }
        // BYOP returned above. Ordinary SDK reads have now made their own fresh
        // negative source lookup. Explicit built-in types remove description,
        // not fresh parameter-sensitive planning: no statement is retained.
        // Schema-defined parameters still use the original describe path.
        let executed = timing
            .measure(QueryStage::QuerySql, async {
                if typed_protocol {
                    if self.native_structural_query_plans && statement_cache.is_some() {
                        let preparer = crate::postgres_pool::NativeQueryPreparer::new(
                            transaction.client(),
                            statement_cache,
                        );
                        instant_pg::query_instaql_with_structural_preparer(
                            &transaction,
                            &compiled,
                            app_id,
                            &options,
                            instant_pg::DEFAULT_QUERY_TIMEOUT,
                            &preparer,
                        )
                        .await
                    } else {
                        instant_pg::query_instaql_with_typed_parameters(
                            &transaction,
                            &compiled,
                            app_id,
                            &options,
                            instant_pg::DEFAULT_QUERY_TIMEOUT,
                        )
                        .await
                    }
                } else {
                    instant_pg::query_instaql(&transaction, &compiled, app_id, &options).await
                }
            })
            .await
            .map_err(pg_query_error)?;
        let filtering_started = std::time::Instant::now();
        let transformed = self
            .transform_instant_file_results_owned(&compiled, executed, catalog, app_id)
            .await?;
        let post_bindings = permission_plan.post_filter_bindings(&bindings);
        let permission_options = QueryPermissionOptions {
            attribute_projection: prepared.permission_attribute_projection,
        };
        let permissions = if use_root_frames {
            // One statement already produced every root. Split the transformed
            // result into per-root frames and load their union once in this same
            // snapshot; no input here is shared with another session.
            let frames = root_frames::load(
                &rules,
                &compiled,
                transformed,
                catalog,
                prepared.permission_attribute_projection,
                |loads| {
                    let transaction = &transaction;
                    let loader = &loader;
                    let complete = permission_entity_statement.as_ref();
                    let projected = permission_projection_statement.as_ref();
                    async move {
                        match (complete, projected) {
                            (Some(complete), Some(projected)) => {
                                instant_permissions::load_permission_entity_loads_with_statements(
                                    transaction,
                                    complete,
                                    projected,
                                    app_id,
                                    catalog,
                                    loads,
                                )
                                .await
                            }
                            _ => loader.load_query_entities(transaction, loads).await,
                        }
                    }
                },
            )
            .instrument(tracing::info_span!("permissions/load-root-frames"))
            .await
            .map_err(typed_permission_error)?;
            let (nodes, entity_data) = root_frames::RootFrame::assemble(frames);
            prepare_owned_query_permissions(
                &rules,
                &compiled,
                nodes,
                catalog,
                &post_bindings,
                permission_options,
                move |_loads| async move { Ok(entity_data) },
            )
            .instrument(tracing::info_span!("permissions/prepare-query"))
            .await
            .map_err(typed_permission_error)?
        } else {
            prepare_owned_query_permissions(
                &rules,
                &compiled,
                transformed,
                catalog,
                &post_bindings,
                permission_options,
                |loads| {
                    let transaction = &transaction;
                    let loader = &loader;
                    let complete = permission_entity_statement.as_ref();
                    let projected = permission_projection_statement.as_ref();
                    async move {
                        match (complete, projected) {
                            (Some(complete), Some(projected)) => {
                                instant_permissions::load_permission_entity_loads_with_statements(
                                    transaction,
                                    complete,
                                    projected,
                                    app_id,
                                    catalog,
                                    loads,
                                )
                                .await
                            }
                            _ => loader.load_query_entities(transaction, loads).await,
                        }
                    }
                },
            )
            .instrument(tracing::info_span!("permissions/prepare-query"))
            .await
            .map_err(typed_permission_error)?
        };
        let finish_checks = |filtered: Vec<ExecutedNode>| {
            let counts = permission_plan
                .checked_entity_counts(&compiled, &filtered)
                .map_err(typed_permission_error)?;
            permission_plan
                .consume_rate_limits(&counts, self.rate_limit_consumer.as_ref())
                .map_err(typed_permission_error)?;
            let topics = Some(framework_query_topics(instant_query_topics(
                &compiled, &filtered,
            )?));
            Ok::<_, ApiError>((filtered, topics))
        };
        let ((filtered, instant_topics), finishing_started, early_rollback_elapsed) =
            if permissions.can_release_snapshot() && permission_plan.rate_limits.is_empty() {
                // All complete permission records came from the same fresh
                // repeatable-read snapshot as the query rows. No reachable rule
                // or field bind can call ref/limit, including dynamic/dead calls.
                // The remaining CEL work and topic validation use only those
                // immutable inputs, so they need not pin a database connection.
                let ending_started = std::time::Instant::now();
                let snapshot_end = transaction.rollback().await;
                let rollback_elapsed = ending_started.elapsed();
                drop(client);
                let filtered = permissions
                    .filter(&mut refs, |_| async {
                        Err(PermissionError::Loader(
                            "CPU-only permission pass requested a reference load".into(),
                        ))
                    })
                    .instrument(tracing::info_span!("permissions/filter-query"))
                    .await
                    .map_err(typed_permission_error)?;
                timing.record(
                    QueryStage::PermissionFilter,
                    filtering_started.elapsed().saturating_sub(rollback_elapsed),
                );
                let finishing_started = std::time::Instant::now();
                let checked = finish_checks(filtered)?;
                // Preserve permission/count/topic error priority over rollback
                // failures, although the read snapshot was physically ended first.
                snapshot_end.map_err(internal_error)?;
                (checked, finishing_started, rollback_elapsed)
            } else {
                // Callback-capable rules and deferred query limits keep the
                // original open snapshot and fresh loader for every retry round.
                let filtered = permissions
                    .filter(&mut refs, |requests| {
                        loader.load_refs(&transaction, requests)
                    })
                    .instrument(tracing::info_span!("permissions/filter-query"))
                    .await
                    .map_err(typed_permission_error)?;
                timing.record(QueryStage::PermissionFilter, filtering_started.elapsed());
                let finishing_started = std::time::Instant::now();
                let checked = finish_checks(filtered)?;
                transaction.rollback().await.map_err(internal_error)?;
                drop(client);
                (checked, finishing_started, std::time::Duration::ZERO)
            };
        let result = framework_result(&compiled.roots, &filtered);
        timing.record(
            QueryStage::ResultFinish,
            finishing_started
                .elapsed()
                .saturating_add(early_rollback_elapsed),
        );
        timing.finish(retained_authorization_lease);
        Ok(RuntimeFrameworkResponse {
            result,
            attrs: if include_attrs {
                Value::Array(catalog.attributes.iter().map(framework_attr).collect())
            } else {
                Value::Null
            },
            byop_topics: None,
            instant_topics,
        })
    }

    async fn framework_query(
        &self,
        request: RuntimeFrameworkRequest,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        let actor = request
            .refresh_token
            .map_or(AdminActor::Guest, AdminActor::RefreshToken);
        self.permissioned_framework_query(
            AdminRequestContext {
                app_id: request.app_id,
                bearer_token: None,
                actor,
                client_ip: request.client_ip,
            },
            request.query,
            true,
        )
        .await
    }

    async fn permissioned_transact(
        &self,
        client: &mut Client,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        steps: &[TransactionStep],
        source_steps: &[TransactionStep],
        permission: PreparedTransactionPermission,
    ) -> Result<TransactionReceipt, ApiError> {
        validate_dynamic_reserved_names(client, steps).await?;
        let loader = PostgresPermissionDataLoader { app_id, catalog };
        let mut transaction_options = self.transaction_options;
        transaction_options.admin = false;
        let receipt = execute_permissioned_transaction_with_source_order(
            client,
            app_id,
            catalog,
            &permission.rules,
            steps,
            source_steps,
            &EvaluationBindings {
                auth: permission.auth,
                request: instant_permissions::RequestBinding {
                    ip: permission.client_ip.map(|ip| ip.to_string()),
                    ..instant_permissions::RequestBinding::default()
                },
                ..EvaluationBindings::default()
            },
            PermissionedTransactionOptions {
                transaction: transaction_options,
                rate_limit_consumer: Some(Arc::clone(&self.rate_limit_consumer)),
                ..PermissionedTransactionOptions::default()
            },
            &loader,
        )
        .await
        .map_err(|error| transaction_permission_error(error, Some(catalog)))?;
        Ok(receipt.transaction)
    }

    async fn execute_transact(
        &self,
        context: AdminRequestContext,
        request: AdminTransactRequest,
    ) -> Result<TransactionReceipt, ApiError> {
        let prepared = self.prepare_transact(&context, request).await?;
        self.execute_prepared_transact(context.app_id, prepared)
            .await
    }

    async fn prepare_transact(
        &self,
        context: &AdminRequestContext,
        request: AdminTransactRequest,
    ) -> Result<PreparedAdminTransaction, ApiError> {
        let started = std::time::Instant::now();
        let catalog = self
            .catalog_for_context(context, AdminDataScope::Write)
            .await?;
        let catalog_ms = started.elapsed().as_secs_f64() * 1000.0;
        let authorization_started = std::time::Instant::now();
        let permission = if context.actor == AdminActor::Admin {
            None
        } else if let Some((auth, rules)) =
            self.cached_authorization(context.app_id, &context.actor)
        {
            Some(PreparedTransactionPermission {
                auth,
                rules,
                client_ip: context.client_ip,
            })
        } else {
            let client = self
                .transaction_authorization_client
                .as_ref()
                .unwrap_or(&self.client)
                .acquire()
                .await
                .map_err(internal_error)?;
            Some(PreparedTransactionPermission {
                auth: self
                    .session_auth(&client, context.app_id, &catalog, &context.actor)
                    .await?,
                rules: self.app_rules(&client, context.app_id).await?,
                client_ip: context.client_ip,
            })
        };
        let authorization_ms = authorization_started.elapsed().as_secs_f64() * 1000.0;
        let compile_started = std::time::Instant::now();
        let raw_steps = Value::Array(request.steps);
        let compiled = compile_admin_transaction_with_order(
            &raw_steps,
            &catalog,
            request.throw_on_missing_attrs,
        )
        .map_err(|error| admin_transaction_validation_error(&error, &raw_steps))?;
        tracing::debug!(target: "instant_server::transaction_stages",
            catalog_ms, authorization_ms,
            compile_ms = compile_started.elapsed().as_secs_f64() * 1000.0,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            "transaction preparation completed");
        Ok(PreparedAdminTransaction {
            catalog,
            steps: compiled.grouped_steps,
            source_steps: compiled.source_steps,
            permission,
        })
    }

    async fn execute_prepared_transact(
        &self,
        app_id: Uuid,
        prepared: PreparedAdminTransaction,
    ) -> Result<TransactionReceipt, ApiError> {
        let started = std::time::Instant::now();
        let PreparedAdminTransaction {
            catalog,
            steps,
            source_steps,
            permission,
        } = prepared;
        if let Some(permission) = permission {
            self.transaction_pool_pressure_policy.check(app_id).await?;
            let mut client = self.client.acquire().await.map_err(internal_error)?;
            let admission_ms = started.elapsed().as_secs_f64() * 1000.0;
            let execution_started = std::time::Instant::now();
            let result = self
                .permissioned_transact(
                    &mut client,
                    app_id,
                    &catalog,
                    &steps,
                    &source_steps,
                    permission,
                )
                .await;
            tracing::debug!(target: "instant_server::transaction_stages",
                admission_ms, execution_ms = execution_started.elapsed().as_secs_f64() * 1000.0,
                total_ms = started.elapsed().as_secs_f64() * 1000.0,
                succeeded = result.is_ok(), "permissioned transaction execution completed");
            return result;
        }
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        validate_dynamic_reserved_names(&client, &steps).await?;
        execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &steps,
            self.transaction_options,
        )
        .await
        .map_err(|error| transaction_execution_error_with_catalog(error, Some(&catalog)))
    }
}

#[cfg(test)]
#[path = "transaction_authorization_tests.rs"]
mod transaction_authorization_tests;

#[cfg(test)]
#[path = "structural_query_tests.rs"]
mod structural_query_tests;

fn should_use_realtime_replica(app_id: Uuid, replica_available: bool, enabled: bool) -> bool {
    app_id != crate::INSTANT_CONFIG_APP_ID && replica_available && enabled
}

pub(crate) fn byop_namespace(connection_string: &str) -> Result<String, url::ParseError> {
    let parsed = Url::parse(connection_string)?;
    Ok(parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "currentSchema").then(|| value.into_owned()))
        .unwrap_or_else(|| "public".to_owned()))
}

pub(crate) fn byop_driver_connection_string(
    connection_string: &str,
) -> Result<String, url::ParseError> {
    let connection_string = connection_string
        .strip_prefix("jdbc:")
        .unwrap_or(connection_string);
    let mut parsed = Url::parse(connection_string)?;
    let query = parsed
        .query_pairs()
        .filter(|(key, _)| key != "currentSchema")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    parsed.set_query(None);
    if !query.is_empty() {
        parsed.query_pairs_mut().extend_pairs(query);
    }
    Ok(parsed.to_string())
}

fn byop_connection_version(connection_string: &str, namespace: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(connection_string.as_bytes());
    digest.update([0]);
    digest.update(namespace.as_bytes());
    digest.finalize().into()
}

async fn load_byop_permission_refs(
    instant_transaction: &tokio_postgres::Transaction<'_>,
    byop_transaction: &tokio_postgres::Transaction<'_>,
    app_id: Uuid,
    instant_catalog: &AttributeCatalog,
    byop: &ByopContext,
    requests: Vec<RefRequest>,
) -> Result<RefCache, PermissionError> {
    let (byop_requests, instant_requests): (Vec<_>, Vec<_>) = requests
        .into_iter()
        .partition(|request| byop.schema.tables.contains_key(&request.etype));
    let postgres_loader = PostgresPermissionDataLoader {
        app_id,
        catalog: instant_catalog,
    };
    let mut results = if instant_requests.is_empty() {
        RefCache::new()
    } else {
        postgres_loader
            .load_refs(instant_transaction, instant_requests)
            .await?
    };
    for request in byop_requests {
        let values = instant_pg::query_byop_reference(
            byop_transaction,
            &byop.namespace,
            &byop.schema,
            &request.etype,
            request.entity_id.as_json(),
            &request.path,
        )
        .await
        .map_err(|error| PermissionError::Loader(error.to_string()))?;
        results.insert(request, values);
    }
    Ok(results)
}

#[allow(clippy::too_many_arguments)]
fn filter_byop_form<'a>(
    instant_transaction: &'a tokio_postgres::Transaction<'_>,
    byop_transaction: &'a tokio_postgres::Transaction<'_>,
    app_id: Uuid,
    instant_catalog: &'a AttributeCatalog,
    byop: &'a ByopContext,
    rules: &'a RuleSet,
    form: &'a Form,
    mut entities: Vec<Value>,
    bindings: &'a EvaluationBindings,
    refs: &'a mut RefCache,
) -> Pin<Box<dyn Future<Output = Result<Vec<Value>, PermissionError>> + Send + 'a>> {
    Box::pin(async move {
        for entity in &mut entities {
            let object = entity
                .as_object_mut()
                .ok_or(PermissionError::InvalidEntityData)?;
            for child in &form.children {
                let Some(value) = object.remove(&child.key) else {
                    continue;
                };
                let singular = value.is_object() || value.is_null();
                let child_entities = match value {
                    Value::Null => Vec::new(),
                    Value::Object(object) => vec![Value::Object(object)],
                    Value::Array(values) => values,
                    _ => return Err(PermissionError::InvalidEntityData),
                };
                let mut filtered = filter_byop_form(
                    instant_transaction,
                    byop_transaction,
                    app_id,
                    instant_catalog,
                    byop,
                    rules,
                    child,
                    child_entities,
                    bindings,
                    refs,
                )
                .await?;
                object.insert(
                    child.key.clone(),
                    if singular {
                        filtered.pop().unwrap_or(Value::Null)
                    } else {
                        Value::Array(filtered)
                    },
                );
            }
        }
        rules
            .filter_view_entities_async(&form.etype, &entities, bindings, refs, |requests| {
                load_byop_permission_refs(
                    instant_transaction,
                    byop_transaction,
                    app_id,
                    instant_catalog,
                    byop,
                    requests,
                )
            })
            .await
    })
}

fn count_byop_entities(
    byop: &ByopContext,
    form: &Form,
    entities: &[Value],
    counts: &mut HashMap<String, HashSet<String>>,
) {
    for entity in entities {
        let Some(object) = entity.as_object() else {
            continue;
        };
        if let Some(id) = byop
            .schema
            .tables
            .get(&form.etype)
            .and_then(|table| object.get(&table.primary_key.field))
        {
            counts
                .entry(form.etype.clone())
                .or_default()
                .insert(id.to_string());
        }
        for child in &form.children {
            match object.get(&child.key) {
                Some(Value::Array(values)) => count_byop_entities(byop, child, values, counts),
                Some(value @ Value::Object(_)) => {
                    count_byop_entities(byop, child, std::slice::from_ref(value), counts);
                }
                _ => {}
            }
        }
    }
}

fn byop_framework_result(
    byop: &ByopContext,
    query: &Value,
    response: &instant_pg::ByopResponse,
) -> Result<Value, ApiError> {
    let forms = coerce_forms(query, &byop.schema.catalog).map_err(internal_error)?;
    let mut triples = BTreeMap::new();
    for form in &forms {
        if let Some(Value::Array(entities)) = response.data.get(&form.key) {
            collect_byop_framework_rows(byop, form, entities, &mut triples)?;
        }
    }
    let mut data = Map::from_iter([(
        "datalog-result".to_owned(),
        json!({"join-rows": [triples.into_values().collect::<Vec<_>>()]}),
    )]);
    if !response.page_info.is_empty() {
        data.insert(
            "page-info".to_owned(),
            Value::Object(
                response
                    .page_info
                    .iter()
                    .map(|(key, page)| {
                        (
                            key.clone(),
                            json!({
                                "start-cursor": page.start_cursor,
                                "end-cursor": page.end_cursor,
                            }),
                        )
                    })
                    .collect(),
            ),
        );
    }
    if !response.aggregates.is_empty() {
        data.insert(
            "aggregate".to_owned(),
            serde_json::to_value(&response.aggregates).map_err(internal_error)?,
        );
    }
    Ok(json!([{"data": data, "child-nodes": []}]))
}

fn serialize_byop_topics(topics: Vec<instant_pg::ByopTopic>) -> Result<Vec<Value>, ApiError> {
    topics
        .into_iter()
        .map(|topic| serde_json::to_value(topic).map_err(internal_error))
        .collect()
}

fn collect_byop_framework_rows(
    byop: &ByopContext,
    form: &Form,
    entities: &[Value],
    triples: &mut BTreeMap<String, Value>,
) -> Result<(), ApiError> {
    let table = byop
        .schema
        .tables
        .get(&form.etype)
        .ok_or_else(|| internal_error(format!("unknown BYOP table `{}`", form.etype)))?;
    for entity in entities {
        let object = entity
            .as_object()
            .ok_or_else(|| internal_error("BYOP permission result is not an object"))?;
        let entity_id = object
            .get(&table.primary_key.field)
            .ok_or_else(|| internal_error("BYOP permission result is missing its primary key"))?;
        for (field_name, field) in &table.fields {
            if let Some(value) = object.get(field_name) {
                insert_byop_framework_row(triples, json!([entity_id, field.attribute_id, value]))?;
            }
        }
        for child in &form.children {
            let relation = table.relations.get(&child.key).ok_or_else(|| {
                internal_error(format!(
                    "unknown BYOP relation `{}.{}`",
                    form.etype, child.key
                ))
            })?;
            let child_table =
                byop.schema.tables.get(&child.etype).ok_or_else(|| {
                    internal_error(format!("unknown BYOP table `{}`", child.etype))
                })?;
            let child_entities = match object.get(&child.key) {
                Some(Value::Array(values)) => values.as_slice(),
                Some(value @ Value::Object(_)) => std::slice::from_ref(value),
                _ => &[],
            };
            collect_byop_framework_rows(byop, child, child_entities, triples)?;
            for child_entity in child_entities {
                let child_id = child_entity
                    .as_object()
                    .and_then(|object| object.get(&child_table.primary_key.field))
                    .ok_or_else(|| {
                        internal_error("BYOP child permission result is missing its primary key")
                    })?;
                let row = match relation.direction {
                    instant_pg::ByopRelationDirection::Forward => {
                        json!([entity_id, relation.attribute_id, child_id])
                    }
                    instant_pg::ByopRelationDirection::Reverse => {
                        json!([child_id, relation.attribute_id, entity_id])
                    }
                };
                insert_byop_framework_row(triples, row)?;
            }
        }
    }
    Ok(())
}

fn insert_byop_framework_row(
    triples: &mut BTreeMap<String, Value>,
    row: Value,
) -> Result<(), ApiError> {
    let key = serde_json::to_string(&row).map_err(internal_error)?;
    triples.insert(key, row);
    Ok(())
}

impl<A> RuntimeTreeQueryBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn query_tree(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<RuntimeTreeQueryResponse, ApiError> {
        self.realtime_query_shaped(context, request)
            .await
            .map(|response| RuntimeTreeQueryResponse {
                result: response.data,
                result_meta: response.result_meta,
                instant_topics: response.instant_topics,
            })
    }

    async fn query_raw(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<Value, ApiError> {
        self.realtime_query_shaped(context, request)
            .await
            .map(|response| response.data)
    }
}

impl<A> AdminDataBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    type PreparedQueryPermissions = PreparedAdminQueryPermissions;

    async fn query(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<Value, ApiError> {
        // The synchronous JVM admin endpoint returns the object tree directly.
        let app_id = context.app_id;
        let result = self
            .query_shaped(context, request)
            .await
            .map(|response| response.data);
        if result.is_ok() {
            crate::metrics::register_app(app_id);
        } else {
            crate::metrics::record_app_error(app_id, crate::metrics::AppErrorSurface::Admin);
        }
        result
    }

    async fn transact(
        &self,
        context: AdminRequestContext,
        request: AdminTransactRequest,
    ) -> Result<AdminTransactResponse, ApiError> {
        let app_id = context.app_id;
        let result = async {
            let prepared = self.prepare_transact(&context, request).await?;
            self.admin_request_rate_limits
                .check(app_id, AdminRequestOperation::Transact)
                .await?;
            if !self.admin_transaction_queue_policy.enabled(app_id).await? {
                return Ok(AdminTransactResponse {
                    tx_id: self.execute_prepared_transact(app_id, prepared).await?.id,
                });
            }
            let grouping_steps = prepared.steps.clone();
            let receipt = Box::pin(self.admin_transaction_queue.run(
                app_id,
                &grouping_steps,
                self.execute_prepared_transact(app_id, prepared),
            ))
            .await?;
            Ok(AdminTransactResponse { tx_id: receipt.id })
        }
        .await;
        if result.is_ok() {
            crate::metrics::register_app(app_id);
        } else {
            crate::metrics::record_app_error(app_id, crate::metrics::AppErrorSurface::Admin);
        }
        result
    }

    async fn prepare_query_permissions(
        &self,
        context: AdminRequestContext,
        rules_override: Option<Value>,
    ) -> Result<Self::PreparedQueryPermissions, ApiError> {
        self.prepare_query_permissions_request(context, rules_override)
            .await
    }

    #[tracing::instrument(
        name = "datalog/execute",
        skip_all,
        fields(app.id = %prepared.context.app_id, datalog.operation = "query-permissions")
    )]
    async fn query_permissions(
        &self,
        prepared: Self::PreparedQueryPermissions,
        request: AdminQueryPermissionsRequest,
    ) -> Result<Value, ApiError> {
        let PreparedAdminQueryPermissions {
            context,
            auth,
            rules_override,
        } = prepared;
        let app_id = context.app_id;
        let catalog = self.authorizer.load_catalog(app_id).await?;
        self.assert_read_allowed(app_id).await?;
        let mut client = self
            .client
            .acquire_for_datalog(app_id, "query-permissions")
            .await
            .map_err(datalog_pool_error)?;
        let rules = match rules_override {
            Some(rules) => rules,
            None => load_rules(&client, app_id, None).await?,
        };
        let mut raw_query = request.query;
        let rule_params = raw_query.remove("$$ruleParams").unwrap_or(Value::Null);
        let original_query = Value::Object(raw_query);
        let mut bindings = EvaluationBindings {
            auth,
            rule_params,
            ..EvaluationBindings::default()
        };
        bindings.request.ip = request.ip_override;
        bindings.request.origin = request.origin_override;
        rules
            .configure_rate_limits(app_id, &mut bindings, Arc::clone(&self.rate_limit_consumer))
            .map_err(typed_permission_error)?;

        let prepared = self.prepare_instant_query(app_id, &original_query).await?;
        let raw_query = prepared.effective;

        // Audit must evaluate the same entity and reference versions as its query.
        let transaction = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(internal_error)?;
        let loader = PostgresPermissionDataLoader {
            app_id,
            catalog: &catalog,
        };
        let mut refs = RefCache::new();
        let permission_plan = rules
            .plan_query_permissions_async(&raw_query, &catalog, &bindings, &mut refs, |requests| {
                loader.load_refs(&transaction, requests)
            })
            .await
            .map_err(permission_query_planning_error)?;
        let compiled = compile_query(&raw_query, &catalog, false).map_err(|error| {
            ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                .with_hint(json!({"input": raw_query}))
        })?;
        let options = prepared.planner_options;
        if compiled_query_executes_sql(&compiled) {
            apply_postgres_query_settings(&transaction, &prepared.postgres_settings)
                .await
                .map_err(internal_error)?;
        }
        let executed = instant_pg::query_instaql(&transaction, &compiled, app_id, &options)
            .await
            .map_err(pg_query_error)?;
        let transformed = self
            .transform_instant_file_results(&compiled, &executed, &catalog, app_id)
            .await?;
        let audit = audit_executed_query(
            &rules,
            &compiled,
            &transformed,
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            &mut refs,
            |loads| loader.load_query_entities(&transaction, loads),
            |requests| loader.load_refs(&transaction, requests),
        )
        .await
        .map_err(typed_permission_error)?;
        transaction.rollback().await.map_err(internal_error)?;
        let response = shape_response(&compiled, &audit.results, &catalog, request.inference)
            .map_err(internal_error)?;
        let checks = audit
            .checks
            .into_iter()
            .map(|check| {
                json!({
                    "id": check.entity_id,
                    "entity": check.etype,
                    "label": check.label,
                    "record": check.record,
                    "program": check.program.as_ref().map_or_else(|| json!({}), program_json),
                    "check": check.allowed,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "check-results": checks,
            "result": response.data,
            "rule-wheres": rule_wheres_json(permission_plan.constraints),
        }))
    }

    async fn transact_permissions(
        &self,
        context: AdminRequestContext,
        request: AdminTransactPermissionsRequest,
    ) -> Result<Value, ApiError> {
        let app_id = context.app_id;
        let catalog = self
            .authorize_debug(&context, AdminDataScope::Write)
            .await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let auth = impersonated_auth(&client, app_id, &catalog, &context.actor).await?;
        let rules = load_rules(&client, app_id, request.rules_override).await?;
        let raw_steps = Value::Array(request.steps);
        let compiled = compile_admin_transaction_with_order(
            &raw_steps,
            &catalog,
            request.throw_on_missing_attrs,
        )
        .map_err(|error| admin_transaction_validation_error(&error, &raw_steps))?;
        let steps = compiled.grouped_steps;
        self.admin_request_rate_limits
            .check(app_id, AdminRequestOperation::Transact)
            .await?;
        self.transaction_pool_pressure_policy.check(app_id).await?;
        validate_dynamic_reserved_names(&client, &steps).await?;
        let mut bindings = EvaluationBindings {
            auth,
            ..EvaluationBindings::default()
        };
        bindings.request.ip = request.ip_override;
        bindings.request.origin = request.origin_override;
        let loader = PostgresPermissionDataLoader {
            app_id,
            catalog: &catalog,
        };
        let receipt = execute_permissioned_transaction_with_source_order(
            &mut client,
            app_id,
            &catalog,
            &rules,
            &steps,
            &compiled.source_steps,
            &bindings,
            PermissionedTransactionOptions {
                transaction: self.transaction_options,
                admin_check: true,
                admin_dry_run: !request.dangerously_commit,
                rate_limit_consumer: Some(Arc::clone(&self.rate_limit_consumer)),
                ..PermissionedTransactionOptions::default()
            },
            &loader,
        )
        .await
        .map_err(|error| transaction_permission_error(error, Some(&catalog)))?;
        let checks = receipt
            .checks
            .into_iter()
            .map(|check| {
                let mut result = json!({
                    "scope": match check.scope {
                        PermissionCheckScope::Object => "object",
                        PermissionCheckScope::Attribute => "attr",
                    },
                    "action": check.action.as_str(),
                    "etype": check.etype,
                    "program": check.program.as_ref().map_or_else(|| json!({}), program_json),
                    "check-result": check.allowed,
                    "check-pass?": check.allowed,
                });
                if let Some(entity_id) = check.entity_id {
                    result["eid"] = json!(entity_id);
                }
                result
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tx-id": receipt.transaction.id,
            "all-checks-ok?": receipt.all_checks_allowed,
            "committed?": receipt.committed,
            "check-results": checks,
        }))
    }
}

impl<A> crate::runtime_realtime::RuntimeRealtimeTransactionBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn transact_realtime(
        &self,
        context: AdminRequestContext,
        request: AdminTransactRequest,
    ) -> Result<crate::runtime_realtime::RuntimeRealtimeTransaction, ApiError> {
        let receipt = self.execute_transact(context, request).await?;
        Ok(crate::runtime_realtime::RuntimeRealtimeTransaction {
            tx_id: receipt.id,
            fallback_isn: receipt.fallback_isn,
        })
    }
}

impl<A> RuntimeFrameworkBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn query(
        &self,
        request: RuntimeFrameworkRequest,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        self.framework_query(request).await
    }
}

impl<A> RuntimeJoinRowsQueryBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn query_join_rows(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        if context.actor == AdminActor::Admin {
            self.admin_framework_query(context, request, true).await
        } else {
            self.permissioned_framework_query(context, request.query, true)
                .await
        }
    }

    async fn query_join_rows_on_native_catalog(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
        catalog_app_id: Uuid,
        catalog: AttributeCatalog,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        if context.app_id == catalog_app_id
            && matches!(
                context.actor,
                AdminActor::Guest | AdminActor::RefreshToken(_)
            )
        {
            self.permissioned_framework_query_on_catalog(
                context,
                request.query,
                &catalog,
                true,
                true,
            )
            .await
        } else {
            self.query_join_rows(context, request).await
        }
    }

    async fn query_join_rows_without_attrs(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        if context.actor == AdminActor::Admin {
            self.query_join_rows(context, request).await
        } else {
            self.permissioned_framework_query(context, request.query, false)
                .await
        }
    }

    async fn query_join_rows_on_native_catalog_without_attrs(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
        catalog_app_id: Uuid,
        catalog: AttributeCatalog,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        self.query_join_rows_on_native_catalog_without_attrs_shared(
            context,
            request,
            catalog_app_id,
            Arc::new(catalog),
        )
        .await
    }

    async fn query_join_rows_on_native_catalog_without_attrs_shared(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
        catalog_app_id: Uuid,
        catalog: Arc<AttributeCatalog>,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        if context.app_id == catalog_app_id
            && matches!(
                context.actor,
                AdminActor::Guest | AdminActor::RefreshToken(_)
            )
        {
            self.permissioned_framework_query_on_catalog_shared(
                context,
                request.query,
                &catalog,
                true,
                false,
            )
            .await
        } else {
            self.query_join_rows_without_attrs(context, request).await
        }
    }
}

impl<A> RuntimeStreamPermissionBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn authorize_stream_create(
        &self,
        context: AdminRequestContext,
        client_id: String,
        rule_params: Value,
    ) -> Result<(), ApiError> {
        self.authorize_stream_action(
            context,
            PermissionAction::Create,
            json!({"clientId": client_id}),
            rule_params,
        )
        .await
    }

    async fn authorize_stream_view(
        &self,
        context: AdminRequestContext,
        stream: Value,
        rule_params: Value,
    ) -> Result<(), ApiError> {
        self.authorize_stream_action(context, PermissionAction::View, stream, rule_params)
            .await
    }
}

impl<A> RuntimeRealtimeCatalogBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn refresh_realtime_batch_on_catalog(
        &self,
        app_id: Uuid,
        snapshot: RefreshCatalogSnapshot,
        queries: Vec<RuntimeRealtimeBatchQuery>,
    ) -> Result<RuntimeRealtimeRefreshBatch, ApiError> {
        if self.refresh_source_reads.is_some() && snapshot.native_source_for(app_id) {
            // Consume only this refresh's app-scoped fresh native routing read.
            // Per-query auth, rules, refs and row snapshots are still executed
            // by the ordinary guarded cache loader. BYOP/custom paths stay live.
            self.byop_pools.lock().await.remove(&app_id);
            return Ok(RuntimeRealtimeRefreshBatch {
                catalog: snapshot.catalog,
                external_catalog: false,
                results: None,
            });
        }
        let catalog =
            Arc::try_unwrap(snapshot.catalog).unwrap_or_else(|catalog| (*catalog).clone());
        self.refresh_realtime_batch(app_id, catalog, queries).await
    }

    async fn resolve_realtime_catalog(
        &self,
        app_id: Uuid,
        instant_catalog: AttributeCatalog,
    ) -> Result<(AttributeCatalog, bool), ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        let Some(byop) = self.byop_context_if_configured(&client, app_id).await? else {
            return Ok((instant_catalog, false));
        };
        Ok((
            realtime_client_catalog(byop.context.schema.catalog, instant_catalog),
            true,
        ))
    }

    async fn refresh_realtime_batch(
        &self,
        app_id: Uuid,
        instant_catalog: AttributeCatalog,
        queries: Vec<RuntimeRealtimeBatchQuery>,
    ) -> Result<RuntimeRealtimeRefreshBatch, ApiError> {
        if let Some(reads) = &self.refresh_source_reads
            && !reads.load(app_id).await?
        {
            // Match the original negative BYOP lookup's stale-pool retirement.
            self.byop_pools.lock().await.remove(&app_id);
            return Ok(RuntimeRealtimeRefreshBatch {
                catalog: Arc::new(instant_catalog),
                external_catalog: false,
                results: None,
            });
        }
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let Some(mut byop) = self.byop_context_if_configured(&client, app_id).await? else {
            // Execution stays in the runtime's shared cache, admission budget
            // and cancellation scope. Only the freshly resolved catalog/source
            // is handed to that guarded loader; no native work executes here.
            return Ok(RuntimeRealtimeRefreshBatch {
                catalog: Arc::new(instant_catalog),
                external_catalog: false,
                results: None,
            });
        };
        let auth_and_rules = if let Some(request) = queries
            .iter()
            .find(|request| request.context.actor != AdminActor::Admin)
        {
            let auth = impersonated_auth(&client, app_id, &instant_catalog, &request.context.actor)
                .await?;
            let rules = load_rules(&client, app_id, None).await?;
            Some((auth, rules))
        } else {
            None
        };
        let catalog = byop.context.schema.catalog.clone();
        let mut results = Vec::with_capacity(queries.len());
        for request in queries {
            results.push(
                self.execute_realtime_byop_query(
                    &mut client,
                    &mut byop,
                    &instant_catalog,
                    request,
                    auth_and_rules.as_ref(),
                )
                .await,
            );
        }
        Ok(RuntimeRealtimeRefreshBatch {
            catalog: Arc::new(realtime_client_catalog(catalog, instant_catalog)),
            external_catalog: true,
            results: Some(results),
        })
    }
}

fn realtime_client_catalog(
    mut external: AttributeCatalog,
    instant: AttributeCatalog,
) -> AttributeCatalog {
    // Core indexes identities in wire order with last-write-wins semantics.
    // Keep external IDs available for BYOP result decoding, but let primary
    // identities drive optimistic Instant transactions such as auth.ref setup.
    external.attributes.extend(instant.attributes);
    external
}

impl<A> RuntimeRealtimeIdentityBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn resolve_realtime_user_id(
        &self,
        context: AdminRequestContext,
    ) -> Result<Option<Uuid>, ApiError> {
        if matches!(context.actor, AdminActor::Admin | AdminActor::Guest) {
            return Ok(None);
        }
        let catalog = self
            .catalog_for_context(&context, AdminDataScope::Read)
            .await?;
        self.realtime_user_id_on_catalog(&context, &catalog).await
    }

    async fn resolve_realtime_user_id_on_native_catalog(
        &self,
        context: AdminRequestContext,
        catalog_app_id: Uuid,
        catalog: AttributeCatalog,
    ) -> Result<Option<Uuid>, ApiError> {
        if !native_init_catalog_handoff_allowed(
            self.native_init_catalog_handoff,
            context.app_id,
            catalog_app_id,
            &context.actor,
        ) {
            return self.resolve_realtime_user_id(context).await;
        }
        self.realtime_user_id_on_catalog(&context, &catalog).await
    }
}

impl<A> RuntimeSyncBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn sync_snapshot(
        &self,
        request: RuntimeSyncSnapshotRequest,
    ) -> Result<RuntimeSyncSnapshot, ApiError> {
        let tx_id = {
            let client = self.client.acquire().await.map_err(internal_error)?;
            client
                .query_one(
                    "select coalesce(max(id), 0)::bigint from transactions where app_id = $1",
                    &[&request.context.app_id],
                )
                .await
                .map_err(internal_error)?
                .get::<_, i64>(0)
        };
        let context = request.context;
        let query_request = AdminQueryRequest {
            query: request.query,
            inference: false,
            versions: None,
        };
        let response = if context.actor == AdminActor::Admin {
            // The watermark above comes from the primary. Reading snapshot rows
            // from a replica could label stale data with that watermark.
            self.admin_framework_query(context, query_request, false)
                .await?
        } else {
            self.permissioned_framework_query(context, query_request.query, true)
                .await?
        };
        if response.byop_topics.is_some() {
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "Sync tables do not support BYOP.",
            ));
        }
        let triples = response
            .result
            .as_array()
            .and_then(|result| result.first())
            .and_then(|result| result.get("data"))
            .and_then(|data| data.get("datalog-result"))
            .and_then(|result| result.get("join-rows"))
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| internal_error("framework sync query returned invalid join rows"))?;
        let join_rows = sync_snapshot_join_rows(triples);
        let attribute_ids = response
            .attrs
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|attribute| attribute.get("id").and_then(Value::as_str))
            .map(|id| Uuid::parse_str(id).map_err(internal_error))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RuntimeSyncSnapshot {
            join_rows,
            tx_id,
            attribute_ids,
        })
    }

    async fn create_sync_subscription(
        &self,
        request: RuntimeSyncSubscriptionCreate,
    ) -> Result<(), ApiError> {
        let is_admin = request.context.actor == AdminActor::Admin;
        if is_admin {
            self.authorizer
                .authorize_only(request.context.clone(), AdminDataScope::Write)
                .await?;
        }
        let user_id = self.sync_actor_user_id(&request.context).await?;
        let query = serde_json::to_string(&request.query).map_err(internal_error)?;
        let token_hash = Sha256::digest(request.token.as_bytes()).to_vec();
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let transaction = client.transaction().await.map_err(internal_error)?;
        transaction
            .execute(
                "insert into sync_subs(id, app_id, query, token_hash, is_admin, user_id) \
                 values ($1, $2, $3, $4, $5, $6)",
                &[
                    &request.subscription_id,
                    &request.context.app_id,
                    &query,
                    &token_hash,
                    &is_admin,
                    &user_id,
                ],
            )
            .await
            .map_err(internal_error)?;
        transaction
            .execute(
                "insert into sync_sub_topics(sync_sub_id, topic_num, idx, e, a, v, v_filter) \
                 select $1, 0, array['ea']::topics_idx[], array[''::bytea], \
                        array_agg(uuid_send(attribute_id)), array['{}'::jsonb], null \
                 from unnest($2::uuid[]) as attributes(attribute_id)",
                &[&request.subscription_id, &request.attribute_ids],
            )
            .await
            .map_err(internal_error)?;
        transaction.commit().await.map_err(internal_error)
    }

    async fn delete_sync_subscription(
        &self,
        context: AdminRequestContext,
        subscription_id: Uuid,
    ) -> Result<(), ApiError> {
        let is_admin = context.actor == AdminActor::Admin;
        if is_admin {
            self.authorizer
                .authorize_only(context.clone(), AdminDataScope::Write)
                .await?;
        }
        let user_id = self.sync_actor_user_id(&context).await?;
        let client = self.client.acquire().await.map_err(internal_error)?;
        client
            .execute(
                "delete from sync_subs where id = $1 and app_id = $2 \
                 and is_admin = $3 and user_id is not distinct from $4",
                &[&subscription_id, &context.app_id, &is_admin, &user_id],
            )
            .await
            .map_err(internal_error)?;
        Ok(())
    }

    async fn resume_sync_subscription(
        &self,
        request: RuntimeSyncSubscriptionResume,
    ) -> Result<RuntimeSyncSubscription, ApiError> {
        require_admin(&request.context)?;
        self.authorizer
            .authorize(request.context.clone(), AdminDataScope::Read)
            .await?;
        let client = self.client.acquire().await.map_err(internal_error)?;
        let record = client
            .query_opt(
                "select app_id, query, token_hash, is_admin, user_id \
                 from sync_subs where id = $1",
                &[&request.subscription_id],
            )
            .await
            .map_err(internal_error)?
            .ok_or_else(|| sync_validation("Subscription is missing."))?;
        let app_id = record.get::<_, Uuid>(0);
        let query = record.get::<_, String>(1);
        let expected_hash = record.get::<_, Vec<u8>>(2);
        let is_admin = record.get::<_, bool>(3);
        let user_id = record.get::<_, Option<Uuid>>(4);
        let provided_hash = Sha256::digest(request.token.as_bytes());
        if app_id != request.context.app_id
            || !constant_bytes_equal(&expected_hash, provided_hash.as_slice())
        {
            return Err(sync_validation("Invalid token."));
        }
        if !is_admin || user_id.is_some() {
            return Err(sync_validation(
                "Subscription ownership does not match this admin session.",
            ));
        }
        let query = serde_json::from_str::<Map<String, Value>>(&query).map_err(internal_error)?;
        let topic_rows = client
            .query(
                "select encode(attribute_id, 'hex') \
                 from sync_sub_topics, unnest(a) as attributes(attribute_id) \
                 where sync_sub_id = $1 and octet_length(attribute_id) = 16 \
                 order by topic_num, attribute_id",
                &[&request.subscription_id],
            )
            .await
            .map_err(internal_error)?;
        let attribute_ids = topic_rows
            .into_iter()
            .map(|row| row.get::<_, String>(0))
            .map(|encoded| Uuid::parse_str(&encoded).map_err(internal_error))
            .collect::<Result<Vec<_>, _>>()?;
        if attribute_ids.is_empty() {
            return Err(sync_validation("Subscription topics are missing."));
        }
        Ok(RuntimeSyncSubscription {
            app_id,
            query,
            attribute_ids,
        })
    }
}

fn constant_bytes_equal(left: &[u8], right: &[u8]) -> bool {
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

fn sync_snapshot_join_rows(rows: Vec<Value>) -> Vec<Value> {
    let mut entities = Vec::<Vec<Value>>::new();
    let mut entity_indexes = HashMap::<String, usize>::new();
    for triple in rows
        .into_iter()
        .flat_map(|row| row.as_array().cloned().unwrap_or_default())
    {
        let Some(entity) = triple.as_array().and_then(|triple| triple.first()) else {
            continue;
        };
        let entity_key =
            serde_json::to_string(entity).expect("sync-table entities are JSON serializable");
        let index = if let Some(index) = entity_indexes.get(&entity_key) {
            *index
        } else {
            let index = entities.len();
            entity_indexes.insert(entity_key, index);
            entities.push(Vec::new());
            index
        };
        entities[index].push(triple);
    }
    entities.into_iter().map(Value::Array).collect()
}

fn sync_validation(message: &str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

pub(crate) fn framework_attr(attribute: &Attribute) -> Value {
    serde_json::to_value(crate::attribute_wire::FrameworkAttribute(attribute))
        .expect("framework attributes are JSON serializable")
}

pub(crate) async fn impersonated_auth(
    client: &Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    actor: &AdminActor,
) -> Result<Value, ApiError> {
    impersonated_auth_with_sources(client, app_id, catalog, actor)
        .await
        .map(|(auth, _)| auth)
}

fn auth_key(actor: &AdminActor) -> Option<crate::control_plane_cache::AuthKey> {
    match actor {
        AdminActor::RefreshToken(token) => {
            Some(crate::control_plane_cache::AuthKey::RefreshToken(*token))
        }
        AdminActor::Email(email) => Some(crate::control_plane_cache::AuthKey::Email(email.clone())),
        AdminActor::Admin | AdminActor::Guest => None,
    }
}

/// Resolves the impersonated user's auth object together with the user and
/// refresh-token entities it was read from.
#[tracing::instrument(name = "auth/resolve-user", skip_all)]
async fn impersonated_auth_with_sources(
    client: &Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    actor: &AdminActor,
) -> Result<(Value, Vec<Uuid>), ApiError> {
    let (user, token_entity) = match actor {
        AdminActor::Guest => return Ok((json!({}), Vec::new())),
        AdminActor::RefreshToken(token) => {
            instant_auth::user_and_token_by_refresh_token(client, app_id, catalog, *token)
                .await
                .map_err(super::runtime_auth::auth_error)?
                .map(|(user, token_entity)| (user, Some(token_entity)))
        }
        AdminActor::Email(email) => instant_auth::user_by_email(client, app_id, catalog, email)
            .await
            .map_err(super::runtime_auth::auth_error)?
            .map(|user| (user, None)),
        AdminActor::Admin => {
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "Cannot test perms as admin",
            ));
        }
    }
    .ok_or_else(|| ApiError::new(ApiErrorType::RecordNotFound, "Could not find app user"))?;
    let sources = std::iter::once(user.id).chain(token_entity).collect();
    let mut auth = Map::new();
    auth.insert("id".to_owned(), json!(user.id));
    if let Some(email) = user.email {
        auth.insert("email".to_owned(), Value::String(email));
    }
    if let Some(user_type) = user.user_type {
        auth.insert("type".to_owned(), Value::String(user_type));
    }
    if let Some(image_url) = user.image_url {
        auth.insert("imageURL".to_owned(), Value::String(image_url));
    }
    auth.extend(user.extra_fields);
    Ok((Value::Object(auth), sources))
}

pub(crate) async fn load_rules(
    client: &Client,
    app_id: Uuid,
    rules_override: Option<Value>,
) -> Result<RuleSet, ApiError> {
    parse_rule_set(load_rule_code(client, app_id, rules_override).await?)
}

#[tracing::instrument(name = "permissions/read-rules", skip_all)]
async fn load_rule_code(
    client: &Client,
    app_id: Uuid,
    rules_override: Option<Value>,
) -> Result<Value, ApiError> {
    if let Some(code) = rules_override {
        Ok(code)
    } else {
        Ok(client
            .query_typed_opt(
                "SELECT code FROM rules WHERE app_id = $1",
                &[(&app_id, Type::UUID)],
            )
            .await
            .map_err(internal_error)?
            .map_or_else(|| json!({}), |row| row.get::<_, Json<Value>>(0).0))
    }
}

#[tracing::instrument(name = "permissions/compile-rules", skip_all)]
fn parse_rule_set(code: Value) -> Result<RuleSet, ApiError> {
    RuleSet::try_new(code).map_err(|issues| {
        ApiError::new(ApiErrorType::ValidationFailed, "Invalid permission rules").with_hint(json!({
            "errors": issues.into_iter().map(|issue| json!({
                "message": issue.message,
                "path": issue.path,
            })).collect::<Vec<_>>()
        }))
    })
}

fn program_json(program: &instant_permissions::PermissionProgramInfo) -> Value {
    json!({
        "etype": program.etype,
        "action": program.action.as_str(),
        "code": program.code,
        "display-code": program.display_code,
    })
}

fn rule_wheres_json(constraints: std::collections::HashMap<String, QueryConstraint>) -> Value {
    Value::Object(
        constraints
            .into_iter()
            .map(|(etype, constraint)| {
                let value = match constraint {
                    QueryConstraint::Where(conditions) => json!({
                        "short-circuit?": false,
                        "where-clauses": conditions,
                    }),
                    QueryConstraint::DenyAll => json!({
                        "short-circuit?": true,
                        "where-clauses": [],
                    }),
                };
                (etype, value)
            })
            .collect(),
    )
}

/// One root frame load on its own repeatable-read snapshot.
///
/// Settings, statement-cancel registration and pool workload are applied inside
/// the shared-cache task, exactly as the request path would apply them.
#[allow(clippy::too_many_arguments)]
async fn load_root_frame(
    source: PostgresClientSource,
    signer: Option<Arc<dyn AsyncFileUrlSigner>>,
    structural: bool,
    typed_protocol: bool,
    app_id: Uuid,
    single_root: instant_instaql::CompiledQuery,
    catalog: Arc<AttributeCatalog>,
    rules: RuleSet,
    options: MatchCteOptions,
    settings: Vec<PostgresQuerySetting>,
    permission_attribute_projection: bool,
) -> Result<root_frames::RootFrame, ApiError> {
    let mut client = source
        .acquire_for_datalog(app_id, "framework-root-frame")
        .await
        .map_err(datalog_pool_error)?;
    let statement_cache = if structural {
        client.native_statement_cache()
    } else {
        None
    };
    let permission_entity_statement = if statement_cache.is_some() {
        client
            .prepare_typed_cached(
                instant_permissions::PERMISSION_ENTITY_SQL,
                instant_permissions::permission_entity_parameter_types(),
            )
            .await
            .ok()
    } else {
        None
    };
    let permission_projection_statement = if statement_cache.is_some() {
        client
            .prepare_typed_cached(
                instant_permissions::PERMISSION_ENTITY_PROJECTED_SQL,
                instant_permissions::permission_entity_projected_parameter_types(),
            )
            .await
            .ok()
    } else {
        None
    };
    let transaction = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(internal_error)?;
    if compiled_query_executes_sql(&single_root) {
        apply_postgres_query_settings(&transaction, &settings)
            .await
            .map_err(internal_error)?;
    }
    let executed = if typed_protocol {
        if structural && statement_cache.is_some() {
            let preparer = crate::postgres_pool::NativeQueryPreparer::new(
                transaction.client(),
                statement_cache,
            );
            instant_pg::query_instaql_with_structural_preparer(
                &transaction,
                &single_root,
                app_id,
                &options,
                instant_pg::DEFAULT_QUERY_TIMEOUT,
                &preparer,
            )
            .await
        } else {
            instant_pg::query_instaql_with_typed_parameters(
                &transaction,
                &single_root,
                app_id,
                &options,
                instant_pg::DEFAULT_QUERY_TIMEOUT,
            )
            .await
        }
    } else {
        instant_pg::query_instaql(&transaction, &single_root, app_id, &options).await
    }
    .map_err(pg_query_error)?;
    let transformed = match signer.as_ref() {
        Some(signer) => transform_file_results_async_owned(
            &single_root,
            executed,
            catalog.as_ref(),
            app_id,
            signer.as_ref(),
        )
        .await
        .map_err(internal_error)?,
        None => executed,
    };
    let mut transformed = transformed;
    if transformed.len() != 1 {
        return Err(typed_permission_error(
            PermissionError::QueryResultCardinality {
                expected: 1,
                actual: transformed.len(),
            },
        ));
    }
    let node = transformed.pop().expect("single-root result has one node");
    let loader = PostgresPermissionDataLoader {
        app_id,
        catalog: catalog.as_ref(),
    };
    let load_catalog = Arc::clone(&catalog);
    let frame = root_frames::frame_from_node(
        &rules,
        &single_root,
        node,
        catalog.as_ref(),
        permission_attribute_projection,
        |loads| {
            let transaction = &transaction;
            let loader = &loader;
            let complete = permission_entity_statement.as_ref();
            let projected = permission_projection_statement.as_ref();
            let catalog = &load_catalog;
            async move {
                match (complete, projected) {
                    (Some(complete), Some(projected)) => {
                        instant_permissions::load_permission_entity_loads_with_statements(
                            transaction,
                            complete,
                            projected,
                            app_id,
                            catalog.as_ref(),
                            loads,
                        )
                        .await
                    }
                    _ => loader.load_query_entities(transaction, loads).await,
                }
            }
        },
    )
    .await
    .map_err(typed_permission_error)?;
    transaction.rollback().await.map_err(internal_error)?;
    let topics = root_frames::invalidation_topics(&single_root, &frame.nodes, &frame.entity_data);
    Ok(root_frames::RootFrame { topics, ..frame })
}

fn root_frame_cache_error(error: QueryCacheError<ApiError>) -> ApiError {
    match error {
        QueryCacheError::Loader(error) => error.as_ref().clone(),
        QueryCacheError::Cancelled => {
            internal_error("shared root frame load was cancelled by invalidation")
        }
        QueryCacheError::Overloaded => ApiError::new(
            ApiErrorType::RateLimited,
            "Realtime query capacity is temporarily exhausted",
        ),
    }
}

pub(crate) fn typed_permission_error(error: PermissionError) -> ApiError {
    match error {
        PermissionError::PermissionDenied { etype, scope, .. } => ApiError::new(
            ApiErrorType::PermissionDenied,
            "Permission denied: not perms-pass?",
        )
        .with_hint(json!({
            "input": [etype, scope],
            "expected": "perms-pass?",
        })),
        PermissionError::RateLimited {
            retry_at,
            remaining_tokens,
        } => {
            let remaining = retry_at.signed_duration_since(chrono::Utc::now());
            let retry_after = u64::try_from(remaining.num_milliseconds().max(0))
                .unwrap_or(u64::MAX)
                .saturating_add(999)
                / 1_000;
            ApiError::new(
                ApiErrorType::RateLimited,
                "Your request exceeded the rate limit.",
            )
            .with_hint(json!({
                "retry-at": retry_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                "retry-after": retry_after,
                "remaining-tokens": remaining_tokens,
            }))
        }
        PermissionError::InvalidTransactionLinkValue { value, tx_step } => {
            const MESSAGE: &str = "Expected link value to be a uuid.";
            ApiError::new(
                ApiErrorType::ValidationFailed,
                format!("Validation failed for eid: {MESSAGE}"),
            )
            .with_hint(json!({
                "data-type": "eid",
                "input": value.clone(),
                "errors": [{
                    "message": MESSAGE,
                    "hint": {
                        "tx-step": tx_step
                    }
                }]
            }))
        }
        PermissionError::InvalidTransactionLookupType {
            data_type,
            input,
            message,
            tx_step,
            include_tx_step_in_error,
        } => {
            let detail = if include_tx_step_in_error {
                json!({"tx-step": tx_step, "message": message.clone()})
            } else {
                json!({"message": message.clone()})
            };
            ApiError::new(
                ApiErrorType::ValidationFailed,
                format!("Validation failed for {data_type}: {message}"),
            )
            .with_hint(json!({
                "data-type": data_type,
                "input": input,
                "errors": [detail],
            }))
        }
        error => ApiError::new(ApiErrorType::PermissionEvaluationFailed, error.to_string()),
    }
}

fn admin_transaction_validation_error(error: &TransactionError, input: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ValidationFailed,
        format!("Validation failed for steps: {}", error.message),
    )
    .with_hint(json!({
        "data-type": "steps",
        "input": input,
        "errors": [{
            "message": error.message,
            "in": error.path,
        }],
    }))
}

fn permission_query_planning_error(error: PermissionQueryPlanningError) -> ApiError {
    match error {
        PermissionQueryPlanningError::Permission(error) => typed_permission_error(error),
        error @ PermissionQueryPlanningError::Instaql(_) => {
            ApiError::new(ApiErrorType::PermissionEvaluationFailed, error.to_string())
        }
    }
}

fn transaction_permission_error(
    error: PermissionedTransactionError,
    catalog: Option<&AttributeCatalog>,
) -> ApiError {
    match error {
        PermissionedTransactionError::Permission(error) => typed_permission_error(error),
        PermissionedTransactionError::Execution(error) => {
            transaction_execution_error_with_catalog(error, catalog)
        }
        PermissionedTransactionError::Postgres(error) => {
            transaction_postgres_error(&error, catalog)
        }
    }
}

pub(crate) fn transaction_execution_error(error: ExecutionError) -> ApiError {
    transaction_execution_error_with_catalog(error, None)
}

pub(crate) fn transaction_execution_error_with_catalog(
    error: ExecutionError,
    catalog: Option<&AttributeCatalog>,
) -> ApiError {
    match error {
        ExecutionError::QueryTimeout => ApiError::new(ApiErrorType::Timeout, error.to_string()),
        ExecutionError::AppReadOnly => crate::app_read_only_error(),
        ExecutionError::AppDisabled => crate::app_disabled_error(),
        ExecutionError::ParameterLimitExceeded => parameter_limit_error(),
        ExecutionError::Postgres(error) => transaction_postgres_error(&error, catalog),
        ExecutionError::InvalidAppStatus(_) | ExecutionError::InvalidWalLsn(_) => {
            internal_error(error)
        }
        ExecutionError::LookupUpdateCollision(lookups) => {
            const MESSAGE: &str = "Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists.";
            let input = lookups
                .into_iter()
                .map(|lookup| json!([lookup.attr_id, lookup.value]))
                .collect::<Vec<_>>();
            ApiError::new(
                ApiErrorType::ValidationFailed,
                format!("Validation failed for lookup: {MESSAGE}"),
            )
            .with_hint(json!({
                "data-type": "lookup",
                "input": input,
                "errors": [{"message": MESSAGE}],
            }))
        }
        ExecutionError::InvalidReferenceValue {
            attr_id,
            value,
            entity_id,
        } => {
            let mut hint = json!({
                "value": value,
                "checked-data-type": null,
                "attr-id": attr_id,
            });
            if let Some(entity_id) = entity_id {
                hint["entity-id"] = json!(entity_id);
            }
            if let Some(attribute) = catalog.and_then(|catalog| catalog.by_id(attr_id)) {
                hint["namespace"] = json!(attribute.etype);
                hint["attribute"] = json!(attribute.label);
            }
            ApiError::new(
                ApiErrorType::ValidationFailed,
                "Linked value must be a valid uuid.",
            )
            .with_hint(hint)
        }
        ExecutionError::RequiredAttributeUpdateOnExistingEntities {
            attr_id,
            etype,
            label,
        } => {
            let message = format!(
                "Can't update attribute `{label}` to required because `{etype}` already have entities without it"
            );
            ApiError::new(
                ApiErrorType::ValidationFailed,
                format!("Validation failed for attributes: {message}"),
            )
            .with_hint(json!({
                "data-type": "attributes",
                "input": {"id": attr_id},
                "errors": [{"message": message}],
            }))
        }
        ExecutionError::CreateExisting(steps) => transaction_mode_validation_error(&steps, true),
        ExecutionError::UpdateMissing(steps) => transaction_mode_validation_error(&steps, false),
        error => ApiError::new(ApiErrorType::ValidationFailed, error.to_string()),
    }
}

fn transaction_mode_validation_error(steps: &[TransactionStep], creating: bool) -> ApiError {
    let entities = steps
        .iter()
        .filter_map(mode_validation_triple)
        .map(|(_, triple)| mode_entity_display(&triple.entity))
        .collect::<Vec<_>>()
        .join(", ");
    let action = if creating { "Creating" } else { "Updating" };
    let existence = if creating {
        "that exist"
    } else {
        "that don't exist"
    };
    let message = format!("{action} entities {existence}: {entities}");
    let input = steps
        .iter()
        .filter_map(mode_validation_triple)
        .map(|(operation, triple)| {
            json!({
                "op": operation,
                "eid": mode_entity_json(&triple.entity),
                "etype": triple.etype,
                "aid": triple.attr_id,
                "value": triple.value,
                "rev-etype": triple.reverse_etype,
                "opts": {"mode": triple.options.mode},
            })
        })
        .collect::<Vec<_>>();
    ApiError::new(
        ApiErrorType::ValidationFailed,
        format!("Validation failed for tx-step: {message}"),
    )
    .with_hint(json!({
        "data-type": "tx-step",
        "input": input,
        "errors": [{"message": message}],
    }))
}

fn mode_validation_triple(
    step: &TransactionStep,
) -> Option<(&'static str, &instant_transact::TripleStep)> {
    match step {
        TransactionStep::AddTriple(triple) => Some(("add-triple", triple)),
        TransactionStep::DeepMergeTriple(triple) => Some(("deep-merge-triple", triple)),
        _ => None,
    }
}

fn mode_entity_json(entity: &instant_transact::EntityLookup) -> Value {
    match entity {
        instant_transact::EntityLookup::Id(entity_id) => json!(entity_id),
        instant_transact::EntityLookup::Lookup(lookup) => json!([lookup.attr_id, lookup.value]),
    }
}

fn mode_entity_display(entity: &instant_transact::EntityLookup) -> String {
    match entity {
        instant_transact::EntityLookup::Id(entity_id) => entity_id.to_string(),
        instant_transact::EntityLookup::Lookup(lookup) => {
            json!([lookup.attr_id, lookup.value]).to_string()
        }
    }
}

fn transaction_postgres_error(
    error: &tokio_postgres::Error,
    catalog: Option<&AttributeCatalog>,
) -> ApiError {
    let rendered = error.to_string();
    if rendered.starts_with("PreparedStatement can have at most") {
        return parameter_limit_error();
    }
    let Some(database) = error.as_db_error() else {
        return internal_error(rendered);
    };
    classify_transaction_database_error_with_catalog(
        database.code().code(),
        database.message(),
        database.table(),
        database.constraint(),
        database.detail(),
        catalog,
    )
}

#[cfg(test)]
fn classify_transaction_database_error(
    code: &str,
    message: &str,
    table: Option<&str>,
    constraint: Option<&str>,
    detail: Option<&str>,
) -> ApiError {
    classify_transaction_database_error_with_catalog(code, message, table, constraint, detail, None)
}

fn classify_transaction_database_error_with_catalog(
    code: &str,
    message: &str,
    table: Option<&str>,
    constraint: Option<&str>,
    detail: Option<&str>,
    catalog: Option<&AttributeCatalog>,
) -> ApiError {
    let condition = postgres_condition(code).unwrap_or_else(|| generic_postgres_condition(code));
    let mut hint = Map::new();
    if let Some(table) = table {
        hint.insert("table".to_owned(), Value::String(table.to_owned()));
    }
    hint.insert("condition".to_owned(), Value::String(condition.to_owned()));
    if let Some(constraint) = constraint {
        hint.insert(
            "constraint".to_owned(),
            Value::String(constraint.to_owned()),
        );
    }
    let with_hint = |error: ApiError| error.with_hint(Value::Object(hint.clone()));
    match code {
        "23505" => classify_unique_violation(table, constraint, detail, hint, catalog),
        "23503" => with_hint(ApiError::new(
            ApiErrorType::RecordForeignKeyInvalid,
            if table == Some("apps")
                && constraint == Some("apps_org_id_fkey")
                && message.starts_with("update or delete on table \"orgs\"")
            {
                "The org can't be deleted while it still has apps."
            } else {
                "Foreign Key Invalid: foreign-key-violation"
            },
        )),
        "23514" => classify_check_violation(constraint, detail, hint, catalog),
        "57014" => ApiError::new(
            ApiErrorType::Timeout,
            "The query took too long to complete.",
        ),
        "25P03" => ApiError::new(
            ApiErrorType::Timeout,
            "The transaction took too long to complete.",
        ),
        "22023" if message.starts_with("PreparedStatement can have at most") => {
            parameter_limit_error()
        }
        "22P05" => with_hint(ApiError::new(
            ApiErrorType::ValidationFailed,
            if detail.is_some_and(|detail| detail.contains("\\u0000")) {
                "A value in the transaction contains null bytes, which cannot be stored."
            } else {
                "A value in the transaction contains characters that cannot be stored."
            },
        )),
        "P0001" => classify_raised_exception(message, hint),
        _ => ApiError::new(
            ApiErrorType::SqlException,
            format!("SQL Exception: {condition}"),
        )
        .with_hint(Value::Object(hint)),
    }
}

fn classify_unique_violation(
    table: Option<&str>,
    constraint: Option<&str>,
    detail: Option<&str>,
    mut hint: Map<String, Value>,
    catalog: Option<&AttributeCatalog>,
) -> ApiError {
    let record_type = table.map_or_else(|| "unknown".to_owned(), singular_table_name);
    let parsed = detail.and_then(parse_unique_detail);
    let message = if table == Some("idents") && constraint == Some("app_ident_uq") {
        parsed.as_ref().and_then(|details| {
            let etype = details.get("etype")?;
            let label = details.get("label")?;
            hint.clear();
            hint.insert("record-type".to_owned(), json!(record_type));
            hint.insert("etype".to_owned(), json!(etype));
            hint.insert("label".to_owned(), json!(label));
            Some(format!("`{label}` already exists on `{etype}`"))
        })
    } else {
        let mut triple_message = None;
        if table == Some("triples")
            && let Some(details) = parsed.as_ref()
            && let Some(attr_id) = details
                .get("attr_id")
                .and_then(|attr_id| attr_id.parse::<Uuid>().ok())
            && let Some(value) = details
                .iter()
                .find(|(column, _)| column.ends_with("json_null_to_null(value)"))
                .map(|(_, value)| value)
        {
            hint.clear();
            hint.insert("record-type".to_owned(), json!(record_type));
            hint.insert("attr-id".to_owned(), json!(attr_id));
            hint.insert("value".to_owned(), json!(value));
            if let Some(attribute) = catalog.and_then(|catalog| catalog.by_id(attr_id)) {
                hint.insert("etype".to_owned(), json!(attribute.etype));
                hint.insert("label".to_owned(), json!(attribute.label));
                triple_message = Some(format!(
                    "`{}` is a unique attribute on `{}` and an entity already exists with `{}.{}` = {}",
                    attribute.label, attribute.etype, attribute.etype, attribute.label, value,
                ));
            }
        }
        triple_message
    };
    hint.entry("record-type".to_owned())
        .or_insert_with(|| json!(record_type));
    ApiError::new(
        ApiErrorType::RecordNotUnique,
        message.unwrap_or_else(|| format!("Record not unique: {record_type}")),
    )
    .with_hint(Value::Object(hint))
}

fn parse_unique_detail(detail: &str) -> Option<HashMap<String, String>> {
    let mut offset = 0;
    let columns = parse_parenthesized_columns(detail, &mut offset)?;
    let values = parse_parenthesized_columns(detail, &mut offset)?;
    (columns.len() == values.len()).then(|| columns.into_iter().zip(values).collect())
}

fn parse_parenthesized_columns(input: &str, offset: &mut usize) -> Option<Vec<String>> {
    let bytes = input.as_bytes();
    let start = bytes
        .get(*offset..)?
        .iter()
        .position(|byte| *byte == b'(')?
        + *offset
        + 1;
    let mut columns = Vec::new();
    let mut column_start = start;
    let mut nested = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut index = start;
    while let Some(byte) = bytes.get(index).copied() {
        match byte {
            b'\\' if quoted => escaped = !escaped,
            b'"' if !escaped => quoted = !quoted,
            b'(' if !quoted => nested += 1,
            b')' if !quoted && nested > 0 => nested -= 1,
            b')' if !quoted => {
                columns.push(input[column_start..index].trim().to_owned());
                *offset = index + 1;
                return Some(columns);
            }
            b',' if !quoted && nested == 0 => {
                columns.push(input[column_start..index].trim().to_owned());
                column_start = index + 1;
            }
            _ => {}
        }
        if byte != b'\\' {
            escaped = false;
        }
        index += 1;
    }
    None
}

fn classify_check_violation(
    constraint: Option<&str>,
    detail: Option<&str>,
    hint: Map<String, Value>,
    catalog: Option<&AttributeCatalog>,
) -> ApiError {
    if let Some(mut triple) = detail.and_then(extract_triple_constraint_detail) {
        let attribute = triple
            .attr_id
            .and_then(|attr_id| catalog.and_then(|catalog| catalog.by_id(attr_id)));
        if let Some(attribute) = attribute {
            triple
                .hint
                .insert("namespace".to_owned(), json!(attribute.etype));
            triple
                .hint
                .insert("attribute".to_owned(), json!(attribute.label));
        }
        let message = match constraint {
            Some("valid_value_data_type") => triple.checked_data_type.as_ref().map_or_else(
                || "Invalid value type.".to_owned(),
                |checked| {
                    let actual = triple.value_type.map_or_else(String::new, |kind| {
                        format!(" but the provided value type is {kind}")
                    });
                    let field = attribute.map_or_else(String::new, |attribute| {
                        format!(" for {}.{}", attribute.etype, attribute.label)
                    });
                    format!("Invalid value type{field}. Value must be a {checked}{actual}.")
                },
            ),
            Some("indexed_values_are_constrained") if triple.unique => {
                "Value is too large for a unique attribute.".to_owned()
            }
            Some("indexed_values_are_constrained") => {
                "Value is too large for an indexed attribute.".to_owned()
            }
            Some("valid_ref_value") => "Linked value must be a valid uuid.".to_owned(),
            _ => "Check Violation: check-violation".to_owned(),
        };
        let kind = if matches!(
            constraint,
            Some("valid_value_data_type" | "indexed_values_are_constrained" | "valid_ref_value")
        ) {
            ApiErrorType::ValidationFailed
        } else {
            ApiErrorType::RecordCheckViolation
        };
        let mut hint = triple.hint;
        if constraint == Some("indexed_values_are_constrained") {
            hint.insert("value-too-large?".to_owned(), Value::Bool(true));
        }
        return ApiError::new(kind, message).with_hint(Value::Object(hint));
    }
    if constraint == Some("orgs_title_check") {
        return ApiError::new(
            ApiErrorType::ValidationFailed,
            "The title for the org is too long. The maximum length is 140 characters.",
        )
        .with_hint(Value::Object(hint));
    }
    ApiError::new(
        ApiErrorType::RecordCheckViolation,
        "Check Violation: check-violation",
    )
    .with_hint(Value::Object(hint))
}

struct TripleConstraintDetail {
    hint: Map<String, Value>,
    checked_data_type: Option<String>,
    value_type: Option<&'static str>,
    unique: bool,
    attr_id: Option<Uuid>,
}

fn extract_triple_constraint_detail(detail: &str) -> Option<TripleConstraintDetail> {
    let row = detail.strip_prefix("Failing row contains (")?;
    let end = row.rfind(')')?;
    let row = &row[..end];
    let commas = row
        .match_indices(',')
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if commas.len() < 12 {
        return None;
    }
    let mut borders = Vec::with_capacity(14);
    borders.push(usize::MAX);
    borders.extend(commas.iter().take(3).copied());
    borders.extend(commas.iter().rev().take(9).rev().copied());
    borders.push(row.len());
    let values = borders
        .windows(2)
        .map(|window| {
            let start = window[0].wrapping_add(1);
            row[start..window[1]].trim()
        })
        .collect::<Vec<_>>();
    if values.len() < 12 {
        return None;
    }
    let (value, value_type) = match serde_json::from_str(values[3]) {
        Ok(value) => {
            let kind = match &value {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
            };
            (value, Some(kind))
        }
        Err(_) => (json!(values[3]), None),
    };
    let checked_data_type = (values[11] != "null").then(|| values[11].to_owned());
    let mut hint = Map::new();
    hint.insert("value".to_owned(), value);
    hint.insert(
        "checked-data-type".to_owned(),
        checked_data_type.clone().map_or(Value::Null, Value::String),
    );
    hint.insert("attr-id".to_owned(), json!(values[2]));
    hint.insert("entity-id".to_owned(), json!(values[1]));
    Some(TripleConstraintDetail {
        hint,
        checked_data_type,
        value_type,
        unique: values[7] == "t",
        attr_id: values[2].parse().ok(),
    })
}

fn classify_raised_exception(message: &str, hint: Map<String, Value>) -> ApiError {
    match message {
        "modify_org_id_on_org_member" => ApiError::new(
            ApiErrorType::ValidationFailed,
            "Org members can not move between orgs.",
        ),
        "remove_last_org_owner" => ApiError::new(
            ApiErrorType::ValidationFailed,
            "There must be at least one member of the org that is an owner.",
        ),
        "required_set_on_reserved_attrs" => ApiError::new(
            ApiErrorType::ValidationFailed,
            "You can't create a required system attribute. Make sure it's optional. Check out the docs to learn more: https://www.instantdb.com/docs/modeling-data#required-constraints",
        ),
        message => ApiError::new(
            ApiErrorType::SqlRaise,
            format!("Raised Exception: {message}"),
        )
        .with_hint(Value::Object(hint)),
    }
}

fn parameter_limit_error() -> ApiError {
    ApiError::new(
        ApiErrorType::ParameterLimitExceeded,
        "There are too many parameters in the transaction or query.",
    )
    .with_hint(json!({
        "message": "Consider batching transactions to reduce the number of writes in a single transaction.",
        "doc-urls": ["https://www.instantdb.com/docs/instaml#batching-transactions"],
    }))
}

fn generic_postgres_condition(_code: &str) -> &'static str {
    // `instant.jdbc.pgerrors/extract-data` falls back to the `:unknown`
    // condition when PostgreSQL (or an extension) returns a SQLSTATE outside
    // its generated table. Keep the argument to make the deliberate fallback
    // boundary explicit at the call site.
    "unknown"
}

fn singular_table_name(table: &str) -> String {
    let dashed = table.replace('_', "-");
    dashed.strip_suffix("ies").map_or_else(
        || dashed.strip_suffix('s').unwrap_or(&dashed).to_owned(),
        |stem| format!("{stem}y"),
    )
}

pub(crate) fn require_admin(context: &AdminRequestContext) -> Result<(), ApiError> {
    if context.actor == AdminActor::Admin {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorType::PermissionDenied,
            "This endpoint requires an admin actor",
        )
        .unauthorized())
    }
}

pub(crate) fn internal_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::internal_error(error)
}

/// Maps a datalog lease failure. An app that stays over its interactive share
/// is a transient capacity limit, not a server fault.
pub(crate) fn datalog_pool_error(error: PostgresPoolError) -> ApiError {
    match error {
        PostgresPoolError::AppCapacity => ApiError::new(
            ApiErrorType::RateLimited,
            "Realtime query capacity is temporarily exhausted",
        ),
        error => internal_error(error),
    }
}

pub(crate) async fn validate_dynamic_reserved_names(
    client: &Client,
    steps: &[TransactionStep],
) -> Result<(), ApiError> {
    if !steps
        .iter()
        .any(|step| matches!(step, TransactionStep::AddAttr(_)))
    {
        return Ok(());
    }
    let row = client
        .query_opt(
            "SELECT flag_value.value
             FROM attrs setting_attr
             JOIN triples flag_setting
               ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
             JOIN attrs value_attr
               ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
              AND value_attr.label = 'value'
             JOIN triples flag_value
               ON flag_value.app_id = $1
              AND flag_value.entity_id = flag_setting.entity_id
              AND flag_value.attr_id = value_attr.id
             WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
               AND setting_attr.label = 'setting'
               AND flag_setting.value = to_jsonb($2::text)
             LIMIT 1",
            &[
                &crate::INSTANT_CONFIG_APP_ID,
                &"reserved-system-catalog-ident-names",
            ],
        )
        .await
        .map_err(internal_error)?;
    let reserved = row
        .map(|row| row.get::<_, Json<Value>>(0).0)
        .map_or_else(HashSet::new, |value| reserved_identity_names(&value));
    validate_reserved_identity_names(steps, &reserved).map_err(transaction_execution_error)
}

fn reserved_identity_names(value: &Value) -> HashSet<(String, String)> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|identity| {
            let identity = identity.as_array()?;
            let [etype, label] = identity.as_slice() else {
                return None;
            };
            Some((etype.as_str()?.to_owned(), label.as_str()?.to_owned()))
        })
        .collect()
}

/// Rejects query numbers that have no finite, representable form before
/// either BYOP routing or query hashing can consume them.
fn validate_query_number_representations(query: &Map<String, Value>) -> Result<(), ApiError> {
    for value in query.values() {
        instant_instaql::validate_query_numbers(value).map_err(query_validation_error)?;
    }
    Ok(())
}

fn pg_query_error(error: instant_pg::PgQueryError) -> ApiError {
    match error {
        instant_pg::PgQueryError::QueryTimeout { .. } => {
            ApiError::new(ApiErrorType::Timeout, error.to_string())
        }
        error => internal_error(error),
    }
}

fn byop_query_error(error: instant_pg::ByopQueryError) -> ApiError {
    match error {
        instant_pg::ByopQueryError::Query(instant_pg::PgQueryError::QueryTimeout { .. }) => {
            ApiError::new(ApiErrorType::Timeout, error.to_string())
        }
        error => internal_error(error),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refresh_catalog_source_hint_is_app_scoped_and_not_authorization() {
        let app = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let catalog = instant_instaql::AttributeCatalog::default();
        assert!(!super::RefreshCatalogSnapshot::from(catalog.clone()).native_source_for(app));
        for (source, expected) in [
            (Some((app, false)), true),
            (Some((app, true)), false),
            (Some((other, false)), false),
            (None, false),
        ] {
            let snapshot = super::RefreshCatalogSnapshot {
                catalog: std::sync::Arc::new(catalog.clone()),
                source,
            };
            assert_eq!(snapshot.native_source_for(app), expected);
        }
    }

    #[tokio::test]
    async fn custom_authorizer_refresh_defaults_to_its_catalog_without_a_route_hint() {
        struct CatalogOnly(AtomicUsize);
        impl super::AdminDataAuthorizer for CatalogOnly {
            async fn authorize(
                &self,
                _: AdminRequestContext,
                _: super::AdminDataScope,
            ) -> Result<AttributeCatalog, ApiError> {
                panic!("guest catalog loading must not acquire admin authority")
            }

            async fn load_catalog(&self, _: Uuid) -> Result<AttributeCatalog, ApiError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(AttributeCatalog::default())
            }
        }
        let authorizer = CatalogOnly(AtomicUsize::new(0));
        let snapshot =
            super::AdminDataAuthorizer::load_refresh_catalog(&authorizer, Uuid::new_v4())
                .await
                .unwrap();
        assert!(snapshot.source.is_none());
        assert_eq!(authorizer.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn native_lease_handoff_requires_completed_controls_and_native_routing() {
        assert!(super::native_authorization_lease_handoff(true, true));
        assert!(!super::native_authorization_lease_handoff(true, false));
        assert!(!super::native_authorization_lease_handoff(false, true));
        assert!(!super::native_authorization_lease_handoff(false, false));
    }

    #[test]
    fn init_metadata_handoff_requires_opt_in_matching_app_and_refresh_token_actor() {
        let app = Uuid::new_v4();
        let token = AdminActor::RefreshToken(Uuid::new_v4());
        assert!(super::native_init_catalog_handoff_allowed(
            true, app, app, &token
        ));
        assert!(!super::native_init_catalog_handoff_allowed(
            false, app, app, &token
        ));
        assert!(!super::native_init_catalog_handoff_allowed(
            true,
            app,
            Uuid::new_v4(),
            &token
        ));
        for actor in [
            AdminActor::Guest,
            AdminActor::Admin,
            AdminActor::Email("test@example.test".into()),
        ] {
            assert!(!super::native_init_catalog_handoff_allowed(
                true, app, app, &actor
            ));
        }
    }

    #[test]
    fn initial_native_typed_protocol_is_explicit_and_preserves_other_paths() {
        use super::DatabaseWorkload;

        for workload in [DatabaseWorkload::Interactive, DatabaseWorkload::Refresh] {
            for actor in [AdminActor::Guest, AdminActor::RefreshToken(Uuid::new_v4())] {
                assert!(super::native_framework_typed_protocol(
                    false, workload, true, &actor
                ));
                assert!(!super::native_framework_typed_protocol(
                    false, workload, false, &actor
                ));
            }
            for actor in [
                AdminActor::Admin,
                AdminActor::Email("test@example.test".into()),
            ] {
                assert!(!super::native_framework_typed_protocol(
                    false, workload, true, &actor
                ));
            }
        }
        // The pre-existing refresh path must not depend on the new opt-in, and
        // an initial native-hint call must not silently acquire refresh scope.
        for opt_in in [false, true] {
            assert!(super::native_framework_typed_protocol(
                true,
                DatabaseWorkload::Refresh,
                opt_in,
                &AdminActor::Guest
            ));
            assert!(!super::native_framework_typed_protocol(
                true,
                DatabaseWorkload::Interactive,
                opt_in,
                &AdminActor::Guest
            ));
        }
    }

    #[tokio::test]
    async fn live_init_handoff_skips_only_catalog_reload_and_keeps_token_revocation_fresh() {
        use crate::RuntimeRealtimeIdentityBackend;
        let Some(url) = instant_test_support::test_database_url(
            "live_init_handoff_skips_only_catalog_reload_and_keeps_token_revocation_fresh",
        ) else {
            return;
        };
        struct RejectReload(AtomicUsize);
        impl super::AdminDataAuthorizer for RejectReload {
            async fn authorize(
                &self,
                _: AdminRequestContext,
                _: super::AdminDataScope,
            ) -> Result<AttributeCatalog, ApiError> {
                Err(super::internal_error("unexpected admin authorization"))
            }
            async fn load_catalog(&self, _: Uuid) -> Result<AttributeCatalog, ApiError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(super::internal_error("catalog reload requested"))
            }
        }
        let (mut setup, driver) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(driver);
        let creator = Uuid::new_v4();
        let app = Uuid::new_v4();
        setup
            .execute(
                "INSERT INTO instant_users(id,email) VALUES($1,$2)",
                &[&creator, &format!("{creator}@example.test")],
            )
            .await
            .unwrap();
        setup
            .execute(
                "INSERT INTO apps(id,creator_id,title) VALUES($1,$2,'init metadata handoff test')",
                &[&app, &creator],
            )
            .await
            .unwrap();
        let catalog = instant_instaql::system_attribute_catalog().unwrap();
        let user = instant_auth::sign_in_guest(&mut setup, app, &catalog)
            .await
            .unwrap();
        let pool = crate::PostgresPool::connect(&url, 1).await.unwrap();
        let authorizer = Arc::new(RejectReload(AtomicUsize::new(0)));
        let service = AdminDataService::from_pool(pool.clone(), Arc::clone(&authorizer));
        let context = AdminRequestContext {
            app_id: app,
            bearer_token: None,
            actor: AdminActor::RefreshToken(user.refresh_token),
            client_ip: None,
        };
        assert!(
            service
                .resolve_realtime_user_id_on_native_catalog(context.clone(), app, catalog.clone())
                .await
                .is_err(),
            "opt-out must use original custom authorizer"
        );
        assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
        let service = service.with_native_realtime_init_catalog_handoff();
        let before = pool.acquisition_count();
        assert_eq!(
            service
                .resolve_realtime_user_id_on_native_catalog(context.clone(), app, catalog.clone())
                .await
                .unwrap(),
            Some(user.user.id)
        );
        assert_eq!(
            pool.acquisition_count() - before,
            1,
            "only the fresh identity lease remains"
        );
        assert_eq!(
            authorizer.0.load(Ordering::SeqCst),
            1,
            "same-init metadata avoids catalog reload only"
        );
        assert!(
            service
                .resolve_realtime_user_id_on_native_catalog(
                    context.clone(),
                    Uuid::new_v4(),
                    catalog.clone()
                )
                .await
                .is_err(),
            "mismatched hint must fall back"
        );
        assert_eq!(authorizer.0.load(Ordering::SeqCst), 2);
        instant_auth::delete_refresh_token(&mut setup, app, &catalog, user.refresh_token)
            .await
            .unwrap();
        assert!(
            service
                .resolve_realtime_user_id_on_native_catalog(context, app, catalog)
                .await
                .is_err(),
            "same metadata must not retain a revoked identity"
        );
        assert_eq!(authorizer.0.load(Ordering::SeqCst), 2);
        drop(service);
        drop(pool);
        // Only this test's freshly generated fixture identifiers are removed.
        setup
            .execute("DELETE FROM triples WHERE app_id=$1", &[&app])
            .await
            .unwrap();
        setup
            .execute("DELETE FROM apps WHERE id=$1", &[&app])
            .await
            .unwrap();
        setup
            .execute("DELETE FROM instant_users WHERE id=$1", &[&creator])
            .await
            .unwrap();
        drop(setup);
        driver.await.unwrap().unwrap();
    }
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use base64::Engine as _;
    use instant_datalog::MatchCteOptions;
    use instant_http::{
        AdminActor, AdminDataBackend, AdminQueryPermissionsRequest, AdminQueryRequest,
        AdminRequestContext, AdminTransactPermissionsRequest, AdminTransactRequest,
        AdminTransactResponse, ApiError, ApiErrorType, admin_data_router,
    };
    use instant_instaql::{
        Attribute, AttributeCatalog, AttributeValueType, Cardinality, normalized_query_hash,
    };
    use instant_permissions::{
        PermissionError, PermissionQueryPlanningError, PermissionedTransactionError,
    };
    use instant_pg::{
        ByopField, ByopPageInfo, ByopPrimaryKey, ByopQueryError, ByopResponse, ByopSchema,
        ByopTable, PgQueryError,
    };
    use instant_transact::{
        EntityLookup, ExecutionError, LookupRef, TransactionMode, TransactionStep, TripleOptions,
        TripleStep,
    };
    use serde_json::{Value, json};
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_postgres::Client;
    use tokio_postgres_rustls::MakeRustlsConnect;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        OAuthSecretCipher, RuntimeRealtimeBatchQuery, RuntimeRealtimeCatalogBackend,
        RuntimeRealtimeQueryMode,
    };

    use super::{
        AdminDataService, ByopConnectionPool, ByopContext, ByopPoolRegistry,
        DatalogBatchAuthorization, DatalogBatchIsolationKey, PooledInstaqlBatchError,
        PooledInstaqlBatcher, RuntimeTreeQueryBackend, ShapedQueryResponse,
        batch_requires_settings_transaction, byop_connection_version,
        byop_driver_connection_string, byop_framework_result, byop_namespace, byop_query_error,
        classify_transaction_database_error, classify_transaction_database_error_with_catalog,
        datalog_batch_digest, datalog_batch_load_error, framework_attr, matching_byop_pool,
        parse_unique_detail, permission_query_planning_error, pg_query_error,
        prepare_instant_query, realtime_client_catalog, reserved_identity_names,
        should_use_realtime_replica, sync_snapshot_join_rows, transaction_execution_error,
        transaction_execution_error_with_catalog, transaction_permission_error,
        typed_permission_error,
    };

    struct NeverAuthorizer;

    #[test]
    fn explicit_jit_settings_take_precedence_over_the_automatic_small_page_hint() {
        for setting in [
            "jit",
            "JIT",
            "jit_above_cost",
            "JiT_inline_above_cost",
            "jit_optimize_above_cost",
            "work_mem",
        ] {
            let postgres_settings = vec![super::PostgresQuerySetting {
                setting: setting.to_owned(),
                value: "original".to_owned(),
            }];
            let prepared = super::PreparedInstantQuery {
                effective: json!({"users":{}}),
                original_hash: 17,
                planner_options: MatchCteOptions {
                    small_page_jit_off: Some(true),
                    pg_hints_enabled: Some(true),
                    ..MatchCteOptions::default()
                },
                postgres_settings: postgres_settings.clone(),
                permission_attribute_projection: true,
                shared_root_frames: true,
            }
            .with_explicit_postgres_settings_precedence();
            assert_eq!(
                prepared.planner_options.small_page_jit_off,
                Some(setting == "work_mem")
            );
            assert_eq!(prepared.planner_options.pg_hints_enabled, Some(true));
            assert_eq!(prepared.postgres_settings, postgres_settings);
            assert_eq!(prepared.original_hash, 17);
            assert_eq!(prepared.effective, json!({"users":{}}));
        }
        for enabled in [None, Some(false)] {
            let prepared = super::PreparedInstantQuery {
                effective: json!({}),
                original_hash: 0,
                planner_options: MatchCteOptions {
                    small_page_jit_off: enabled,
                    ..MatchCteOptions::default()
                },
                postgres_settings: vec![super::PostgresQuerySetting {
                    setting: "jit".into(),
                    value: "on".into(),
                }],
                permission_attribute_projection: true,
                shared_root_frames: true,
            }
            .with_explicit_postgres_settings_precedence();
            assert_eq!(prepared.planner_options.small_page_jit_off, enabled);
        }
    }

    fn test_secret_cipher() -> OAuthSecretCipher {
        let mut proto = vec![0x08, 0x00, 0x12, 0x20];
        proto.extend(0_u8..32);
        OAuthSecretCipher::from_tink_json(
            &json!({
                "primaryKeyId": 42,
                "key": [{
                    "keyData": {
                        "typeUrl": "type.googleapis.com/google.crypto.tink.AesGcmKey",
                        "value": base64::engine::general_purpose::STANDARD.encode(proto),
                        "keyMaterialType": "SYMMETRIC"
                    },
                    "status": "ENABLED",
                    "keyId": 42,
                    "outputPrefixType": "TINK"
                }]
            })
            .to_string(),
        )
        .unwrap()
    }

    impl super::AdminDataAuthorizer for NeverAuthorizer {
        fn authorize(
            &self,
            _context: AdminRequestContext,
            _scope: super::AdminDataScope,
        ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::pending()
        }

        fn load_catalog(
            &self,
            _app_id: Uuid,
        ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::pending()
        }
    }

    struct SequenceAppStatusPolicy {
        calls: AtomicUsize,
        statuses: Vec<crate::AppStatus>,
    }

    impl crate::EffectiveAppStatusPolicy for SequenceAppStatusPolicy {
        fn status(&self, _app_id: Uuid) -> crate::AppStatusFuture<'_> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let effective_status = self.statuses[index];
            Box::pin(std::future::ready(Ok(crate::AppStatusSnapshot {
                own_status: effective_status,
                effective_status,
            })))
        }
    }

    #[derive(Default)]
    struct CountingAuthorizer {
        calls: AtomicUsize,
    }

    impl super::AdminDataAuthorizer for CountingAuthorizer {
        fn authorize(
            &self,
            _context: AdminRequestContext,
            _scope: super::AdminDataScope,
        ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(AttributeCatalog::default()))
        }

        fn load_catalog(
            &self,
            _app_id: Uuid,
        ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(AttributeCatalog::default()))
        }
    }

    #[derive(Default)]
    struct RecordingAdminRequestRateLimits {
        calls: Mutex<Vec<(Uuid, crate::AdminRequestOperation)>>,
    }

    impl crate::AdminRequestRateLimitPolicy for RecordingAdminRequestRateLimits {
        fn check(
            &self,
            app_id: Uuid,
            operation: crate::AdminRequestOperation,
        ) -> crate::AdminRequestRateLimitFuture<'_> {
            self.calls.lock().unwrap().push((app_id, operation));
            Box::pin(std::future::ready(Err(ApiError::new(
                ApiErrorType::RateLimited,
                "recorded admin request limit",
            ))))
        }
    }

    fn assert_rate_limited<T>(result: Result<T, ApiError>) {
        let Err(error) = result else {
            panic!("admin operation was not rate limited");
        };
        assert_eq!(error.body().kind, ApiErrorType::RateLimited);
    }

    #[tokio::test]
    async fn admin_http_operations_are_limited_once_while_realtime_queries_bypass_the_policy() {
        let Some(database_url) = instant_test_support::test_database_url(
            "admin_http_operations_are_limited_once_while_realtime_queries_bypass_the_policy",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let app_id = Uuid::new_v4();
        let policy = Arc::new(RecordingAdminRequestRateLimits::default());
        let service = AdminDataService::new(client, Arc::new(CountingAuthorizer::default()))
            .with_admin_request_rate_limit_policy(
                Arc::clone(&policy) as Arc<dyn crate::AdminRequestRateLimitPolicy>
            );
        let admin = AdminRequestContext {
            app_id,
            bearer_token: Some("admin-token".to_owned()),
            actor: AdminActor::Admin,
            client_ip: None,
        };
        let guest = AdminRequestContext {
            app_id,
            bearer_token: Some("admin-token".to_owned()),
            actor: AdminActor::Guest,
            client_ip: None,
        };
        let query = AdminQueryRequest {
            query: serde_json::Map::new(),
            inference: false,
            versions: None,
        };

        assert_rate_limited(service.query(admin.clone(), query.clone()).await);
        assert_rate_limited(
            service
                .transact(
                    admin.clone(),
                    AdminTransactRequest {
                        steps: Vec::new(),
                        throw_on_missing_attrs: false,
                    },
                )
                .await,
        );
        assert_rate_limited(
            service
                .prepare_query_permissions(guest.clone(), Some(json!({})))
                .await,
        );
        assert_rate_limited(
            service
                .transact_permissions(
                    guest,
                    AdminTransactPermissionsRequest {
                        steps: Vec::new(),
                        rules_override: Some(json!({})),
                        dangerously_commit: false,
                        throw_on_missing_attrs: false,
                        ip_override: None,
                        origin_override: None,
                    },
                )
                .await,
        );
        assert_eq!(
            *policy.calls.lock().unwrap(),
            vec![
                (app_id, crate::AdminRequestOperation::Query),
                (app_id, crate::AdminRequestOperation::Transact),
                (app_id, crate::AdminRequestOperation::Query),
                (app_id, crate::AdminRequestOperation::Transact),
            ]
        );

        let _ = RuntimeTreeQueryBackend::query_raw(&service, admin, query).await;
        assert_eq!(policy.calls.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn admin_query_status_is_live_and_disabled_stops_before_query_or_byop_work() {
        let Some(database_url) = instant_test_support::test_database_url(
            "admin_query_status_is_live_and_disabled_stops_before_query_or_byop_work",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let app_id = Uuid::new_v4();
        let authorizer = Arc::new(CountingAuthorizer::default());
        let status = Arc::new(SequenceAppStatusPolicy {
            calls: AtomicUsize::new(0),
            statuses: vec![crate::AppStatus::Disabled, crate::AppStatus::ReadOnly],
        });
        let service =
            AdminDataService::new(client, Arc::clone(&authorizer)).with_app_status_policy(
                Arc::clone(&status) as Arc<dyn crate::EffectiveAppStatusPolicy>,
            );
        let context = AdminRequestContext {
            app_id,
            bearer_token: Some("admin-secret".to_owned()),
            actor: AdminActor::Admin,
            client_ip: None,
        };
        let request = AdminQueryRequest {
            query: serde_json::Map::new(),
            inference: false,
            versions: None,
        };

        let disabled = service
            .query(context.clone(), request.clone())
            .await
            .unwrap_err();
        assert_eq!(disabled.body().kind, ApiErrorType::AppDisabled);
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);

        service.assert_read_allowed(app_id).await.unwrap();
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(status.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalid_impersonation_precedes_disabled_status_for_query_and_permission_audit() {
        let Some(database_url) = instant_test_support::test_database_url(
            "invalid_impersonation_precedes_disabled_status_for_query_and_permission_audit",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let app_id = Uuid::new_v4();
        let authorizer = Arc::new(CountingAuthorizer::default());
        let status = Arc::new(SequenceAppStatusPolicy {
            calls: AtomicUsize::new(0),
            statuses: vec![crate::AppStatus::Disabled],
        });
        let service =
            AdminDataService::new(client, Arc::clone(&authorizer)).with_app_status_policy(
                Arc::clone(&status) as Arc<dyn crate::EffectiveAppStatusPolicy>,
            );
        let actor = AdminActor::RefreshToken(Uuid::new_v4());
        let query = AdminQueryRequest {
            query: serde_json::Map::new(),
            inference: false,
            versions: None,
        };

        let query_error = service
            .query(
                AdminRequestContext {
                    app_id,
                    bearer_token: None,
                    actor: actor.clone(),
                    client_ip: None,
                },
                query,
            )
            .await
            .unwrap_err();
        assert_eq!(query_error.body().kind, ApiErrorType::RecordNotFound);
        assert_eq!(query_error.body().message, "Could not find app user");
        assert_eq!(status.calls.load(Ordering::SeqCst), 0);

        let permission_error = service
            .prepare_query_permissions(
                AdminRequestContext {
                    app_id,
                    bearer_token: Some("admin-secret".to_owned()),
                    actor,
                    client_ip: None,
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(permission_error.body().kind, ApiErrorType::RecordNotFound);
        assert_eq!(permission_error.body().message, "Could not find app user");
        assert_eq!(status.calls.load(Ordering::SeqCst), 0);
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 3);
    }

    fn empty_byop_pool(
        app_id: Uuid,
        version: [u8; 32],
        registry: &Arc<ByopPoolRegistry>,
    ) -> Arc<ByopConnectionPool> {
        Arc::new(ByopConnectionPool {
            app_id,
            version,
            clients: Vec::new(),
            next: AtomicUsize::new(0),
            last_used: std::sync::Mutex::new(std::time::Instant::now()),
            retired: AtomicBool::new(false),
            registry: Arc::downgrade(registry),
        })
    }

    async fn wait_for_backend_query(observer: &Client, pid: i32, marker: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let active = observer
                    .query_one(
                        "SELECT EXISTS (
                           SELECT 1 FROM pg_stat_activity
                           WHERE pid = $1 AND state = 'active' AND query LIKE $2
                         )",
                        &[&pid, &format!("%{marker}%")],
                    )
                    .await
                    .unwrap()
                    .get::<_, bool>(0);
                if active {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("BYOP statement did not become active");
    }

    async fn wait_for_backend_exit(observer: &Client, pid: i32) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let exists = observer
                    .query_one(
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                        &[&pid],
                    )
                    .await
                    .unwrap()
                    .get::<_, bool>(0);
                if !exists {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discarded BYOP backend remained connected");
    }

    #[derive(Default)]
    struct RecordingModifierPolicy {
        calls: Mutex<Vec<(Uuid, i32, Value)>>,
    }

    impl super::QueryModifierPolicy for RecordingModifierPolicy {
        fn apply<'a>(
            &'a self,
            app_id: Uuid,
            original_query_hash: i32,
            query: &'a Value,
        ) -> super::QueryModifierPolicyFuture<'a> {
            self.calls
                .lock()
                .unwrap()
                .push((app_id, original_query_hash, query.clone()));
            Box::pin(std::future::ready(Ok(
                json!({"posts": {"$": {"limit": 7}}}),
            )))
        }
    }

    #[derive(Default)]
    struct RecordingPlannerPolicy {
        calls: Mutex<Vec<Value>>,
    }

    #[derive(Default)]
    struct RecordingSettingsPolicy {
        calls: Mutex<Vec<i32>>,
    }

    impl super::QuerySettingsPolicy for RecordingSettingsPolicy {
        fn settings(&self, original_query_hash: i32) -> super::QuerySettingsPolicyFuture<'_> {
            self.calls.lock().unwrap().push(original_query_hash);
            Box::pin(std::future::ready(Ok(vec![super::PostgresQuerySetting {
                setting: "enable_nestloop".to_owned(),
                value: "off".to_owned(),
            }])))
        }
    }

    impl super::QueryPlannerPolicy for RecordingPlannerPolicy {
        fn options<'a>(&'a self, query: &'a Value) -> super::QueryPlannerPolicyFuture<'a> {
            self.calls.lock().unwrap().push(query.clone());
            Box::pin(std::future::ready(Ok(MatchCteOptions::default())))
        }
    }

    #[derive(Default)]
    struct CountingQueryControlReads {
        reads: AtomicUsize,
    }

    impl super::QueryModifierPolicy for CountingQueryControlReads {
        fn apply<'a>(
            &'a self,
            _app_id: Uuid,
            _original_query_hash: i32,
            query: &'a Value,
        ) -> super::QueryModifierPolicyFuture<'a> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(std::future::ready(Ok(query.clone())))
        }
    }

    impl super::QuerySettingsPolicy for CountingQueryControlReads {
        fn settings(&self, _original_query_hash: i32) -> super::QuerySettingsPolicyFuture<'_> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(std::future::ready(Ok(Vec::new())))
        }
    }

    impl super::QueryPlannerPolicy for CountingQueryControlReads {
        fn options<'a>(&'a self, _query: &'a Value) -> super::QueryPlannerPolicyFuture<'a> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(std::future::ready(Ok(MatchCteOptions::default())))
        }
    }

    impl super::QueryPreparationPolicy for CountingQueryControlReads {
        fn prepare<'a>(
            &'a self,
            _app_id: Uuid,
            original: &'a Value,
        ) -> super::QueryPreparationPolicyFuture<'a> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(std::future::ready(Ok(super::PreparedInstantQuery {
                effective: original.clone(),
                original_hash: normalized_query_hash(original).unwrap(),
                planner_options: MatchCteOptions::default(),
                postgres_settings: Vec::new(),
                permission_attribute_projection: true,
                shared_root_frames: true,
            })))
        }
    }

    #[tokio::test]
    async fn query_preparation_reads_operational_controls_once() {
        let controls = CountingQueryControlReads::default();
        let original = json!({"todos": {}});
        let prepared = prepare_instant_query(
            Some(&controls),
            &controls,
            &controls,
            &controls,
            Uuid::new_v4(),
            &original,
        )
        .await
        .unwrap();
        assert_eq!(prepared.effective, original);
        assert_eq!(controls.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn query_preparation_rejects_unrepresentable_numbers() {
        let policies = super::QueryPolicies::default();
        let original: Value =
            serde_json::from_str(r#"{"todos": {"$": {"limit": 1e400}}}"#).unwrap();
        let error = policies
            .prepare(Uuid::new_v4(), &original)
            .await
            .unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(error.status().as_u16(), 400);
        assert!(error.body().message.contains("not finite"));
    }

    #[tokio::test]
    async fn individual_query_control_overrides_preserve_other_combined_controls() {
        let original = json!({"todos": {}});
        for overridden in ["modifier", "settings", "planner"] {
            let controls = Arc::new(CountingQueryControlReads::default());
            let modifier = Arc::new(RecordingModifierPolicy::default());
            let settings = Arc::new(RecordingSettingsPolicy::default());
            let planner = Arc::new(RecordingPlannerPolicy::default());
            let mut policies = super::QueryPolicies::default();
            policies.set_preparation(controls.clone());
            match overridden {
                "modifier" => policies.set_modifier(modifier.clone()),
                "settings" => policies.set_settings(settings.clone()),
                _ => policies.set_planner(planner.clone()),
            }

            let prepared = policies.prepare(Uuid::new_v4(), &original).await.unwrap();
            assert_eq!(controls.reads.load(Ordering::Relaxed), 2, "{overridden}");
            assert_eq!(
                modifier.calls.lock().unwrap().len(),
                usize::from(overridden == "modifier")
            );
            assert_eq!(
                settings.calls.lock().unwrap().len(),
                usize::from(overridden == "settings")
            );
            assert_eq!(
                planner.calls.lock().unwrap().len(),
                usize::from(overridden == "planner")
            );
            if overridden == "modifier" {
                assert_eq!(prepared.effective, json!({"posts": {"$": {"limit": 7}}}));
            } else {
                assert_eq!(prepared.effective, original);
            }
        }
    }

    #[tokio::test]
    async fn query_preparation_hands_effective_query_forward_and_original_to_controls() {
        let app_id = Uuid::new_v4();
        let original = json!({"posts": {}});
        let modifier = RecordingModifierPolicy::default();
        let settings = RecordingSettingsPolicy::default();
        let planner = RecordingPlannerPolicy::default();
        let prepared =
            prepare_instant_query(None, &modifier, &settings, &planner, app_id, &original)
                .await
                .unwrap();
        assert_eq!(prepared.effective, json!({"posts": {"$": {"limit": 7}}}));
        assert_eq!(
            prepared.original_hash,
            normalized_query_hash(&original).unwrap()
        );
        assert_eq!(
            modifier.calls.lock().unwrap().as_slice(),
            &[(app_id, prepared.original_hash, original.clone())]
        );
        assert_eq!(planner.calls.lock().unwrap().as_slice(), &[original]);
        assert_eq!(
            settings.calls.lock().unwrap().as_slice(),
            &[prepared.original_hash]
        );
        assert_eq!(prepared.postgres_settings[0].setting, "enable_nestloop");
    }

    #[test]
    fn query_settings_transaction_keeps_empty_and_missing_query_hot_paths() {
        let missing = instant_instaql::compile_query(
            &json!({"missing": {}}),
            &AttributeCatalog::default(),
            true,
        )
        .unwrap();
        let settings = vec![super::PostgresQuerySetting {
            setting: "enable_nestloop".to_owned(),
            value: "off".to_owned(),
        }];
        assert!(!batch_requires_settings_transaction(&[], &[]));
        assert!(!batch_requires_settings_transaction(&settings, &[missing]));
    }

    #[derive(Clone)]
    struct RejectingAdminTransactionBackend {
        error: ApiError,
    }

    impl AdminDataBackend for RejectingAdminTransactionBackend {
        type PreparedQueryPermissions = AdminRequestContext;

        fn query(
            &self,
            _context: AdminRequestContext,
            _request: AdminQueryRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            std::future::ready(Err(self.error.clone()))
        }

        fn transact(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactRequest,
        ) -> impl Future<Output = Result<AdminTransactResponse, ApiError>> + Send {
            std::future::ready(Err(self.error.clone()))
        }

        fn prepare_query_permissions(
            &self,
            context: AdminRequestContext,
            _rules_override: Option<Value>,
        ) -> impl Future<Output = Result<Self::PreparedQueryPermissions, ApiError>> + Send {
            std::future::ready(Ok(context))
        }

        fn query_permissions(
            &self,
            _prepared: Self::PreparedQueryPermissions,
            _request: AdminQueryPermissionsRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            std::future::ready(Err(self.error.clone()))
        }

        fn transact_permissions(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactPermissionsRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            std::future::ready(Err(self.error.clone()))
        }
    }

    async fn admin_transaction_error_envelope(error: ApiError) -> serde_json::Value {
        let app_id = Uuid::from_u128(1);
        let response = admin_data_router(Arc::new(RejectingAdminTransactionBackend { error }))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/transact")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("app-id", app_id.to_string())
                    .header(header::AUTHORIZATION, "Bearer test-admin-token")
                    .body(Body::from(r#"{"steps":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[test]
    fn realtime_replica_routing_requires_flag_capacity_and_non_config_app() {
        let app_id = Uuid::new_v4();
        assert!(should_use_realtime_replica(app_id, true, true));
        assert!(!should_use_realtime_replica(app_id, false, true));
        assert!(!should_use_realtime_replica(app_id, true, false));
        assert!(!should_use_realtime_replica(
            crate::INSTANT_CONFIG_APP_ID,
            true,
            true
        ));
    }

    #[test]
    fn sync_snapshot_rows_keep_each_entity_together() {
        let rows = vec![
            json!([
                ["post-1", "posts-id", "post-1", 1],
                ["post-1", "posts-author", "user-1", 1],
            ]),
            json!([
                ["user-1", "users-id", "user-1", 1],
                ["post-1", "posts-title", "linked", 1],
            ]),
            json!([["comment-1", "comments-id", "comment-1", 1]]),
        ];

        assert_eq!(
            sync_snapshot_join_rows(rows),
            vec![
                json!([
                    ["post-1", "posts-id", "post-1", 1],
                    ["post-1", "posts-author", "user-1", 1],
                    ["post-1", "posts-title", "linked", 1],
                ]),
                json!([["user-1", "users-id", "user-1", 1]]),
                json!([["comment-1", "comments-id", "comment-1", 1]]),
            ]
        );
    }

    #[test]
    fn datalog_batch_keys_isolate_app_catalog_policy_settings_and_operation() {
        let app_id = Uuid::new_v4();
        let catalog = AttributeCatalog::default();
        let options = instant_datalog::MatchCteOptions::default();
        let key = DatalogBatchIsolationKey {
            app_id,
            workload: super::DatabaseWorkload::Interactive,
            operation: "admin-query",
            authorization: DatalogBatchAuthorization::Admin,
            catalog_digest: datalog_batch_digest(&catalog).unwrap(),
            planner_digest: datalog_batch_digest(&options).unwrap(),
            postgres_settings_digest: datalog_batch_digest(
                &Vec::<super::PostgresQuerySetting>::new(),
            )
            .unwrap(),
        };
        let mut changed_app = key.clone();
        changed_app.app_id = Uuid::new_v4();
        let mut changed_workload = key.clone();
        changed_workload.workload = super::DatabaseWorkload::Refresh;
        let mut changed_operation = key.clone();
        changed_operation.operation = "realtime-tree-query";
        let mut changed_catalog = catalog;
        changed_catalog.attributes.push(instant_instaql::Attribute {
            id: Uuid::new_v4(),
            wire: None,
            etype: "goals".to_owned(),
            label: "title".to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        });
        let mut changed_catalog_key = key.clone();
        changed_catalog_key.catalog_digest = datalog_batch_digest(&changed_catalog).unwrap();
        let mut changed_options = options;
        changed_options.page_info = true;
        let mut changed_policy_key = key.clone();
        changed_policy_key.planner_digest = datalog_batch_digest(&changed_options).unwrap();
        let mut changed_settings_key = key.clone();
        changed_settings_key.postgres_settings_digest =
            datalog_batch_digest(&vec![super::PostgresQuerySetting {
                setting: "enable_nestloop".to_owned(),
                value: "off".to_owned(),
            }])
            .unwrap();

        assert_ne!(key, changed_app);
        assert_ne!(key, changed_workload);
        assert_ne!(key, changed_operation);
        assert_ne!(key, changed_catalog_key);
        assert_ne!(key, changed_policy_key);
        assert_ne!(key, changed_settings_key);
    }

    #[test]
    fn datalog_batch_timeout_keeps_the_public_timeout_contract() {
        let error = datalog_batch_load_error(instant_pg::BatchLoadError::Executor(Arc::new(
            PooledInstaqlBatchError::Query(PgQueryError::QueryTimeout {
                milliseconds: 30_000,
            }),
        )));
        assert_eq!(error.body().kind, ApiErrorType::Timeout);
    }

    #[tokio::test]
    async fn compatible_concurrent_queries_share_one_real_pool_lease() {
        let Some(database_url) = instant_test_support::test_database_url(
            "compatible_concurrent_queries_share_one_real_pool_lease",
        ) else {
            return;
        };
        let pool = crate::PostgresPool::connect(&database_url, 4)
            .await
            .unwrap();
        let baseline = pool.acquisition_count();
        let batcher = PooledInstaqlBatcher::new(
            crate::postgres_pool::PostgresClientSource::pooled(pool.clone()),
        );
        let app_id = Uuid::new_v4();
        let catalog = AttributeCatalog::default();
        let options = instant_datalog::MatchCteOptions::default();
        let queries = (0..10).map(|_| {
            batcher.query(
                app_id,
                "test-admin-query",
                &catalog,
                &options,
                &[],
                instant_instaql::CompiledQuery { roots: Vec::new() },
            )
        });
        let results = futures_util::future::join_all(queries).await;
        assert!(
            results
                .iter()
                .all(|result| result.as_ref().is_ok_and(Vec::is_empty))
        );
        assert_eq!(pool.acquisition_count() - baseline, 1);
    }

    #[tokio::test]
    async fn detached_datalog_batches_do_not_borrow_the_interactive_reservation() {
        let Some(database_url) = instant_test_support::test_database_url(
            "detached_datalog_batches_do_not_borrow_the_interactive_reservation",
        ) else {
            return;
        };
        let pool = crate::PostgresPool::connect_workload_partitioned(&database_url, 8)
            .await
            .unwrap();
        let batcher = PooledInstaqlBatcher::new(
            crate::postgres_pool::PostgresClientSource::pooled(pool.clone()),
        );
        let app_id = Uuid::new_v4();
        let catalog = AttributeCatalog::default();
        let options = instant_datalog::MatchCteOptions::default();
        // Create an interactive loader for the identical query first. Refresh
        // must not reuse its pinned executor after that lane becomes full.
        batcher
            .query(
                app_id,
                "test-query",
                &catalog,
                &options,
                &[],
                instant_instaql::CompiledQuery { roots: Vec::new() },
            )
            .await
            .unwrap();
        let mut interactive_leases = Vec::new();
        for _ in 0..pool.max_size() {
            interactive_leases.push(pool.acquire().await.unwrap());
        }
        let refresh = pool.for_workload(super::DatabaseWorkload::Refresh);
        let baseline = refresh.acquisition_count();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(900),
            super::DatabaseWorkload::Refresh.scope(batcher.query(
                app_id,
                "test-query",
                &catalog,
                &options,
                &[],
                instant_instaql::CompiledQuery { roots: Vec::new() },
            )),
        )
        .await
        .expect("detached Datalog batch used the saturated interactive lane")
        .unwrap();
        assert!(result.is_empty());
        assert_eq!(refresh.acquisition_count() - baseline, 1);
    }

    #[test]
    fn tree_query_metadata_uses_the_sdk_wire_keys_and_cursor_shape() {
        let entity_id = Uuid::from_u128(1);
        let attr_id = Uuid::from_u128(2);
        let cursor = instant_datalog::PageCursorResult {
            entity_id,
            attr_id,
            value: Some(instant_datalog::DatalogValue::Json(json!("ordered"))),
            created_at: 17,
        };
        let shaped = ShapedQueryResponse::from_instaql(instant_instaql::InstaqlResponse {
            data: BTreeMap::from([("goals".to_owned(), json!([]))]),
            page_info: BTreeMap::from([(
                "goals".to_owned(),
                instant_datalog::PageResultInfo {
                    start_cursor: Some(cursor.clone()),
                    end_cursor: Some(cursor),
                    has_next_page: true,
                    has_previous_page: false,
                },
            )]),
            aggregates: BTreeMap::from([(
                "goalCount".to_owned(),
                BTreeMap::from([("count".to_owned(), 3)]),
            )]),
        })
        .unwrap();

        assert_eq!(
            shaped.result_meta["page-info"]["goals"]["start-cursor"],
            json!([entity_id, attr_id, "ordered", 17])
        );
        assert_eq!(
            shaped.result_meta["page-info"]["goals"]["has-next-page?"],
            true
        );
        assert_eq!(shaped.result_meta["aggregate"]["goalCount"]["count"], 3);
    }

    #[test]
    fn postgres_query_timeouts_keep_the_public_timeout_error_type() {
        let error = pg_query_error(PgQueryError::QueryTimeout {
            milliseconds: 30_000,
        });

        assert_eq!(
            error,
            instant_http::ApiError::new(
                ApiErrorType::Timeout,
                "PostgreSQL query exceeded its 30000ms execution timeout",
            )
        );
        let byop_error = byop_query_error(ByopQueryError::Query(PgQueryError::QueryTimeout {
            milliseconds: 30_000,
        }));
        assert_eq!(
            byop_error,
            instant_http::ApiError::new(
                ApiErrorType::Timeout,
                "PostgreSQL query exceeded its 30000ms execution timeout",
            )
        );
        let expected = instant_http::ApiError::new(
            ApiErrorType::Timeout,
            "PostgreSQL transaction statement exceeded its execution timeout",
        );
        assert_eq!(
            transaction_execution_error(ExecutionError::QueryTimeout),
            expected
        );
        assert_eq!(
            transaction_permission_error(
                PermissionedTransactionError::Execution(ExecutionError::QueryTimeout,),
                None
            ),
            expected
        );
    }

    #[test]
    fn permission_rate_limits_keep_the_public_status_type_and_hints() {
        let before = chrono::Utc::now();
        let error = permission_query_planning_error(PermissionQueryPlanningError::Permission(
            PermissionError::rate_limited(std::time::Duration::from_millis(1_250), 7),
        ));
        let after = chrono::Utc::now();

        assert_eq!(error.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.body().kind, ApiErrorType::RateLimited);
        assert_eq!(
            error.body().message,
            "Your request exceeded the rate limit."
        );
        let hint = error.body().hint.as_ref().unwrap();
        assert_eq!(hint["retry-after"], 2);
        assert_eq!(hint["remaining-tokens"], 7);
        let retry_at = chrono::DateTime::parse_from_rfc3339(hint["retry-at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(retry_at >= before + chrono::Duration::milliseconds(1_250));
        assert!(retry_at <= after + chrono::Duration::milliseconds(1_250));

        let sub_millisecond = permission_query_planning_error(
            PermissionQueryPlanningError::Permission(PermissionError::RateLimited {
                retry_at: chrono::Utc::now() + chrono::Duration::microseconds(1_000_500),
                remaining_tokens: 1,
            }),
        );
        assert_eq!(
            sub_millisecond.body().hint.as_ref().unwrap()["retry-after"],
            1
        );
    }

    #[test]
    fn transaction_permission_denial_matches_the_jvm_public_envelope() {
        let error = typed_permission_error(PermissionError::PermissionDenied {
            etype: "posts".to_owned(),
            scope: "object",
            action: "update",
            entity_id: Some(Uuid::from_u128(7)),
        });

        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.body().kind, ApiErrorType::PermissionDenied);
        assert_eq!(error.body().message, "Permission denied: not perms-pass?");
        assert_eq!(
            error.body().hint,
            Some(json!({
                "input": ["posts", "object"],
                "expected": "perms-pass?",
            }))
        );
    }

    #[test]
    fn ordinary_permission_evaluation_errors_remain_bad_requests() {
        let error = typed_permission_error(PermissionError::Evaluation("boom".to_owned()));

        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.body().kind, ApiErrorType::PermissionEvaluationFailed);
        assert_eq!(
            error.body().message,
            "could not evaluate permission rule: boom"
        );
        assert_eq!(error.body().hint, None);
    }

    #[test]
    fn transaction_semantic_errors_remain_public_validation_failures() {
        let error = ExecutionError::UnknownAttribute(Uuid::from_u128(7));
        let expected = instant_http::ApiError::new(
            ApiErrorType::ValidationFailed,
            "transaction references unknown attribute 00000000-0000-0000-0000-000000000007",
        );
        assert_eq!(transaction_execution_error(error), expected);
        assert_eq!(
            transaction_permission_error(
                PermissionedTransactionError::Execution(ExecutionError::UnknownAttribute(
                    Uuid::from_u128(7)
                ),),
                None
            ),
            expected
        );
    }

    #[test]
    fn invalid_reference_semantics_match_for_admin_and_permissioned_callers() {
        let attr_id = Uuid::from_u128(7);
        let entity_id = Uuid::from_u128(8);
        let catalog = AttributeCatalog {
            attributes: vec![Attribute {
                id: attr_id,
                wire: None,
                etype: "posts".to_owned(),
                label: "author".to_owned(),
                value_type: AttributeValueType::Ref,
                cardinality: Cardinality::One,
                unique: false,
                setting_unique: false,
                indexed: false,
                required: false,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        };
        let make_error = || ExecutionError::InvalidReferenceValue {
            attr_id,
            value: json!({"foo": "bar"}),
            entity_id: Some(entity_id),
        };
        let expected = instant_http::ApiError::new(
            ApiErrorType::ValidationFailed,
            "Linked value must be a valid uuid.",
        )
        .with_hint(json!({
            "value": {"foo": "bar"},
            "checked-data-type": null,
            "attr-id": attr_id,
            "entity-id": entity_id,
            "namespace": "posts",
            "attribute": "author",
        }));

        assert_eq!(
            transaction_execution_error_with_catalog(make_error(), Some(&catalog)),
            expected
        );
        assert_eq!(
            transaction_permission_error(
                PermissionedTransactionError::Execution(make_error()),
                Some(&catalog),
            ),
            expected
        );
    }

    #[test]
    fn transaction_mode_errors_keep_the_jvm_tx_step_envelope() {
        let entity_id = Uuid::from_u128(7);
        let attr_id = Uuid::from_u128(8);
        let triple = TripleStep {
            entity: EntityLookup::Id(entity_id),
            etype: Some("posts".to_owned()),
            attr_id,
            reverse_etype: None,
            value: json!(entity_id),
            options: TripleOptions {
                mode: Some(TransactionMode::Create),
            },
        };
        let error = transaction_execution_error(ExecutionError::CreateExisting(vec![
            TransactionStep::AddTriple(triple),
        ]));
        assert_eq!(error.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            error.body().message,
            format!("Validation failed for tx-step: Creating entities that exist: {entity_id}")
        );
        assert_eq!(error.body().hint.as_ref().unwrap()["data-type"], "tx-step");
        assert_eq!(
            error.body().hint.as_ref().unwrap()["input"][0]["eid"],
            json!(entity_id)
        );
        assert_eq!(
            error.body().hint.as_ref().unwrap()["input"][0]["opts"]["mode"],
            "create"
        );
    }

    #[test]
    fn transaction_app_status_errors_keep_public_types_and_hints() {
        for (error, kind, status) in [
            (
                ExecutionError::AppReadOnly,
                ApiErrorType::AppReadOnly,
                "read-only",
            ),
            (
                ExecutionError::AppDisabled,
                ApiErrorType::AppDisabled,
                "disabled",
            ),
        ] {
            let mapped = transaction_execution_error(error);
            assert_eq!(mapped.body().kind, kind);
            assert_eq!(mapped.body().hint, Some(json!({"status": status})));
        }

        let invalid =
            transaction_execution_error(ExecutionError::InvalidAppStatus("paused".to_owned()));
        assert_eq!(invalid.body().kind, ApiErrorType::Unknown);
    }

    fn lookup_collision_error() -> ExecutionError {
        ExecutionError::LookupUpdateCollision(vec![
            LookupRef {
                attr_id: Uuid::from_u128(7),
                value: json!("first@example.com"),
            },
            LookupRef {
                attr_id: Uuid::from_u128(8),
                value: json!({"nested": true}),
            },
        ])
    }

    #[test]
    fn lookup_collision_preserves_the_jvm_validation_payload() {
        let error = transaction_execution_error(lookup_collision_error());
        assert_eq!(error.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            error.body().message,
            "Validation failed for lookup: Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists."
        );
        assert_eq!(
            error.body().hint,
            Some(json!({
                "data-type": "lookup",
                "input": [
                    ["00000000-0000-0000-0000-000000000007", "first@example.com"],
                    ["00000000-0000-0000-0000-000000000008", {"nested": true}],
                ],
                "errors": [{
                    "message": "Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists."
                }],
            }))
        );
    }

    #[tokio::test]
    async fn lookup_collision_http_envelope_retains_lookup_refs() {
        let body =
            admin_transaction_error_envelope(transaction_execution_error(lookup_collision_error()))
                .await;
        assert_eq!(
            body,
            json!({
                "type": "validation-failed",
                "message": "Validation failed for lookup: Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists.",
                "hint": {
                    "data-type": "lookup",
                    "input": [
                        ["00000000-0000-0000-0000-000000000007", "first@example.com"],
                        ["00000000-0000-0000-0000-000000000008", {"nested": true}],
                    ],
                    "errors": [{
                        "message": "Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists."
                    }],
                },
            })
        );
    }

    #[test]
    fn transaction_database_errors_match_jvm_public_types() {
        let cases = [
            (
                "23505",
                "duplicate key",
                Some("triples"),
                Some("av_index"),
                None,
                ApiErrorType::RecordNotUnique,
            ),
            (
                "23503",
                "foreign key",
                Some("triples"),
                Some("triples_attr_id_fkey"),
                None,
                ApiErrorType::RecordForeignKeyInvalid,
            ),
            (
                "23514",
                "check",
                Some("triples"),
                Some("valid_ref_value"),
                None,
                ApiErrorType::RecordCheckViolation,
            ),
            (
                "23514",
                "check",
                Some("widgets"),
                Some("widgets_size_check"),
                None,
                ApiErrorType::RecordCheckViolation,
            ),
            (
                "57014",
                "canceling statement",
                None,
                None,
                None,
                ApiErrorType::Timeout,
            ),
            (
                "25P03",
                "idle timeout",
                None,
                None,
                None,
                ApiErrorType::Timeout,
            ),
            (
                "22P05",
                "encoding",
                Some("triples"),
                None,
                Some("invalid byte sequence \\u0000"),
                ApiErrorType::ValidationFailed,
            ),
            (
                "P0001",
                "custom_trigger_failure",
                None,
                None,
                None,
                ApiErrorType::SqlRaise,
            ),
        ];
        for (code, message, table, constraint, detail, kind) in cases {
            let error =
                classify_transaction_database_error(code, message, table, constraint, detail);
            assert_eq!(error.body().kind, kind, "SQLSTATE {code}");
        }
        let connection =
            classify_transaction_database_error("08006", "connection lost", None, None, None);
        assert_eq!(connection.body().kind, ApiErrorType::SqlException);
        assert_eq!(
            connection.body().message,
            "SQL Exception: connection-failure"
        );
    }

    #[test]
    fn transaction_parameter_and_trigger_messages_match_jvm_contracts() {
        let parameters = classify_transaction_database_error(
            "22023",
            "PreparedStatement can have at most 65,535 parameters",
            None,
            None,
            None,
        );
        assert_eq!(parameters.body().kind, ApiErrorType::ParameterLimitExceeded);
        assert_eq!(
            transaction_execution_error(ExecutionError::ParameterLimitExceeded),
            parameters
        );
        assert_eq!(
            parameters.body().hint,
            Some(json!({
                "message": "Consider batching transactions to reduce the number of writes in a single transaction.",
                "doc-urls": ["https://www.instantdb.com/docs/instaml#batching-transactions"],
            }))
        );

        let required = classify_transaction_database_error(
            "P0001",
            "required_set_on_reserved_attrs",
            None,
            None,
            None,
        );
        assert_eq!(required.body().kind, ApiErrorType::ValidationFailed);
        assert!(
            required
                .body()
                .message
                .contains("required system attribute")
        );
    }

    #[test]
    fn default_postgres_conditions_match_the_jvm_sql_exception_contract() {
        let cases = [
            ("23502", "not-null-violation"),
            ("22012", "division-by-zero"),
            ("25006", "read-only-sql-transaction"),
            ("40001", "serialization-failure"),
            ("40P01", "deadlock-detected"),
            ("42P01", "undefined-table"),
            ("54000", "program-limit-exceeded"),
        ];
        for (code, condition) in cases {
            let error =
                classify_transaction_database_error(code, "database error", None, None, None);
            assert_eq!(error.body().kind, ApiErrorType::SqlException, "{code}");
            assert_eq!(
                error.body().message,
                format!("SQL Exception: {condition}"),
                "{code}"
            );
            assert_eq!(
                error.body().hint.as_ref().unwrap()["condition"],
                condition,
                "{code}"
            );
        }

        let extension =
            classify_transaction_database_error("ZP001", "extension error", None, None, None);
        assert_eq!(extension.body().kind, ApiErrorType::SqlException);
        assert_eq!(extension.body().message, "SQL Exception: unknown");
        assert_eq!(
            extension.body().hint.as_ref().unwrap()["condition"],
            "unknown"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One live schema exercises PostgreSQL's real detail rendering.
    async fn live_postgres_error_details_keep_jvm_messages_hints_and_public_types() {
        let Some(database_url) = instant_test_support::test_database_url(
            "live_postgres_error_details_keep_jvm_messages_hints_and_public_types",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let connection_task = tokio::spawn(connection);
        client
            .batch_execute(
                "CREATE TEMP TABLE transaction_error_fixture (
                   id bigint PRIMARY KEY,
                   required_value text NOT NULL
                 )",
            )
            .await
            .unwrap();

        let not_null = client
            .execute(
                "INSERT INTO transaction_error_fixture (id, required_value) VALUES ($1, $2)",
                &[&1_i64, &Option::<String>::None],
            )
            .await
            .unwrap_err();
        let mapped = super::transaction_postgres_error(&not_null, None);
        assert_eq!(mapped.body().kind, ApiErrorType::SqlException);
        assert_eq!(mapped.body().message, "SQL Exception: not-null-violation");
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["table"],
            "transaction_error_fixture"
        );
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["condition"],
            "not-null-violation"
        );

        let division = client.query_one("SELECT 1 / 0", &[]).await.unwrap_err();
        let mapped = super::transaction_postgres_error(&division, None);
        assert_eq!(mapped.body().kind, ApiErrorType::SqlException);
        assert_eq!(mapped.body().message, "SQL Exception: division-by-zero");
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["condition"],
            "division-by-zero"
        );

        // Mirror the relevant production migration column order and constraint
        // names. PostgreSQL itself renders the detail strings consumed below;
        // this catches drift that synthetic unit strings cannot reveal.
        client
            .batch_execute(
                "CREATE TEMP TABLE attrs (id uuid PRIMARY KEY);
                 CREATE TEMP TABLE idents (
                   id uuid PRIMARY KEY,
                   app_id uuid NOT NULL,
                   attr_id uuid NOT NULL,
                   etype text NOT NULL,
                   label text NOT NULL,
                   CONSTRAINT app_ident_uq UNIQUE (app_id, etype, label)
                 );
                 CREATE OR REPLACE FUNCTION pg_temp.json_null_to_null(value jsonb)
                 RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
                   SELECT CASE WHEN value = 'null'::jsonb THEN NULL ELSE value END
                 $$;
                 CREATE TEMP TABLE triples (
                   app_id uuid NOT NULL,
                   entity_id uuid NOT NULL,
                   attr_id uuid CONSTRAINT triples_attr_id_fkey REFERENCES attrs(id),
                   value jsonb NOT NULL,
                   value_md5 text NOT NULL,
                   ea boolean NOT NULL,
                   eav boolean NOT NULL,
                   av boolean NOT NULL,
                   ave boolean NOT NULL,
                   vae boolean NOT NULL,
                   created_at bigint NOT NULL DEFAULT 1700000000000,
                   checked_data_type text,
                   pg_size integer DEFAULT 128,
                   CONSTRAINT valid_value_data_type CHECK (
                     checked_data_type IS NULL
                     OR checked_data_type <> 'number'
                     OR jsonb_typeof(value) = 'number'
                   ),
                   CONSTRAINT indexed_values_are_constrained CHECK (
                     NOT (eav OR av OR ave OR vae) OR pg_column_size(value) <= 1024
                   ),
                   CONSTRAINT valid_ref_value CHECK (
                     NOT (eav OR vae)
                     OR (jsonb_typeof(value) = 'string'
                         AND (value #>> '{}') ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$')
                   )
                 );
                 CREATE UNIQUE INDEX av_index
                   ON triples (app_id, attr_id, pg_temp.json_null_to_null(value)) WHERE av;",
            )
            .await
            .unwrap();

        let app_id = Uuid::from_u128(1);
        let attr_id = Uuid::from_u128(2);
        let entity_id = Uuid::from_u128(3);
        client
            .execute("INSERT INTO attrs (id) VALUES ($1)", &[&attr_id])
            .await
            .unwrap();

        let ident_id = Uuid::from_u128(10);
        client
            .execute(
                "INSERT INTO idents (id, app_id, attr_id, etype, label)
                 VALUES ($1, $2, $3, 'users', 'email')",
                &[&ident_id, &app_id, &attr_id],
            )
            .await
            .unwrap();
        let duplicate_ident = client
            .execute(
                "INSERT INTO idents (id, app_id, attr_id, etype, label)
                 VALUES ($1, $2, $3, 'users', 'email')",
                &[&Uuid::from_u128(11), &app_id, &attr_id],
            )
            .await
            .unwrap_err();
        let mapped = super::transaction_postgres_error(&duplicate_ident, None);
        assert_eq!(mapped.body().kind, ApiErrorType::RecordNotUnique);
        assert_eq!(mapped.body().message, "`email` already exists on `users`");
        assert_eq!(
            mapped.body().hint,
            Some(json!({
                "record-type": "ident",
                "etype": "users",
                "label": "email",
            }))
        );

        #[allow(
            clippy::items_after_statements,
            clippy::too_many_arguments,
            clippy::fn_params_excessive_bools
        )]
        async fn insert_triple(
            client: &tokio_postgres::Client,
            app_id: Uuid,
            attr_id: Uuid,
            entity_id: Uuid,
            value: serde_json::Value,
            eav: bool,
            av: bool,
            ave: bool,
            vae: bool,
            checked_data_type: Option<&str>,
        ) -> Result<u64, tokio_postgres::Error> {
            let value = tokio_postgres::types::Json(value);
            client
                .execute(
                    "INSERT INTO triples
                 (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
                  checked_data_type)
                 VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text),
                         true, $5, $6, $7, $8, $9)",
                    &[
                        &app_id,
                        &entity_id,
                        &attr_id,
                        &value,
                        &eav,
                        &av,
                        &ave,
                        &vae,
                        &checked_data_type,
                    ],
                )
                .await
        }
        insert_triple(
            &client,
            app_id,
            attr_id,
            entity_id,
            json!("hello, world"),
            false,
            true,
            true,
            false,
            None,
        )
        .await
        .unwrap();

        let catalog = AttributeCatalog {
            attributes: vec![instant_instaql::Attribute {
                id: attr_id,
                wire: None,
                etype: "profiles".to_owned(),
                label: "email".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: true,
                setting_unique: false,
                indexed: true,
                required: false,
                checked_data_type: Some(instant_datalog::DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        };
        let duplicate_triple = insert_triple(
            &client,
            app_id,
            attr_id,
            Uuid::from_u128(4),
            json!("hello, world"),
            false,
            true,
            true,
            false,
            None,
        )
        .await
        .unwrap_err();
        let mapped = super::transaction_postgres_error(&duplicate_triple, Some(&catalog));
        assert_eq!(mapped.body().kind, ApiErrorType::RecordNotUnique);
        assert_eq!(
            mapped.body().message,
            "`email` is a unique attribute on `profiles` and an entity already exists with `profiles.email` = \"hello, world\""
        );
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["attr-id"],
            json!(attr_id)
        );
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["value"],
            "\"hello, world\""
        );

        let bad_type = insert_triple(
            &client,
            app_id,
            attr_id,
            Uuid::from_u128(5),
            json!("forty-two"),
            false,
            false,
            false,
            false,
            Some("number"),
        )
        .await
        .unwrap_err();
        let mapped = super::transaction_postgres_error(&bad_type, Some(&catalog));
        assert_eq!(mapped.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            mapped.body().message,
            "Invalid value type for profiles.email. Value must be a number but the provided value type is string."
        );
        assert_eq!(mapped.body().hint.as_ref().unwrap()["value"], "forty-two");
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["checked-data-type"],
            "number"
        );
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["entity-id"],
            json!(Uuid::from_u128(5))
        );

        let oversized = insert_triple(
            &client,
            app_id,
            attr_id,
            Uuid::from_u128(6),
            json!("a".repeat(2048)),
            false,
            true,
            true,
            false,
            None,
        )
        .await
        .unwrap_err();
        let mapped = super::transaction_postgres_error(&oversized, Some(&catalog));
        assert_eq!(mapped.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            mapped.body().message,
            "Value is too large for a unique attribute."
        );
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["value-too-large?"],
            true
        );

        let invalid_ref = insert_triple(
            &client,
            app_id,
            attr_id,
            Uuid::from_u128(7),
            json!("not-a-uuid"),
            true,
            false,
            false,
            true,
            None,
        )
        .await
        .unwrap_err();
        let mapped = super::transaction_postgres_error(&invalid_ref, Some(&catalog));
        assert_eq!(mapped.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(mapped.body().message, "Linked value must be a valid uuid.");

        let missing_attr = Uuid::from_u128(999);
        let foreign_key = client
            .execute(
                "INSERT INTO triples
                 (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae)
                 VALUES ($1, $2, $3, 'null', md5('null'), true, false, false, false, false)",
                &[&app_id, &Uuid::from_u128(8), &missing_attr],
            )
            .await
            .unwrap_err();
        let mapped = super::transaction_postgres_error(&foreign_key, Some(&catalog));
        assert_eq!(mapped.body().kind, ApiErrorType::RecordForeignKeyInvalid);
        assert_eq!(
            mapped.body().message,
            "Foreign Key Invalid: foreign-key-violation"
        );
        assert_eq!(mapped.body().hint.as_ref().unwrap()["table"], "triples");
        assert_eq!(
            mapped.body().hint.as_ref().unwrap()["constraint"],
            "triples_attr_id_fkey"
        );

        drop(client);
        connection_task.await.unwrap().unwrap();
    }

    #[test]
    fn org_deletion_foreign_key_message_matches_jvm_contract() {
        let error = classify_transaction_database_error(
            "23503",
            "update or delete on table \"orgs\" violates foreign key constraint",
            Some("apps"),
            Some("apps_org_id_fkey"),
            None,
        );
        assert_eq!(error.body().kind, ApiErrorType::RecordForeignKeyInvalid);
        assert_eq!(
            error.body().message,
            "The org can't be deleted while it still has apps."
        );

        let unrelated = classify_transaction_database_error(
            "23503",
            "update or delete on table \"orgs\" violates foreign key constraint",
            Some("other_table"),
            Some("other_fkey"),
            None,
        );
        assert_eq!(
            unrelated.body().message,
            "Foreign Key Invalid: foreign-key-violation"
        );
    }

    #[test]
    fn unique_constraint_details_preserve_identity_and_attribute_hints() {
        let idents = classify_transaction_database_error(
            "23505",
            "duplicate key",
            Some("idents"),
            Some("app_ident_uq"),
            Some(
                "Key (app_id, etype, label)=(00000000-0000-0000-0000-000000000001, users, email) already exists.",
            ),
        );
        assert_eq!(idents.body().message, "`email` already exists on `users`");
        assert_eq!(
            idents.body().hint,
            Some(json!({
                "record-type": "ident",
                "etype": "users",
                "label": "email",
            }))
        );

        let attr_id = Uuid::from_u128(9);
        let detail = format!(
            "Key (app_id, attr_id, json_null_to_null(value))=(00000000-0000-0000-0000-000000000001, {attr_id}, \"hello, world\") already exists."
        );
        let triples = classify_transaction_database_error(
            "23505",
            "duplicate key",
            Some("triples"),
            Some("av_index"),
            Some(&detail),
        );
        assert_eq!(triples.body().kind, ApiErrorType::RecordNotUnique);
        assert_eq!(
            triples.body().hint.as_ref().unwrap()["attr-id"],
            json!(attr_id)
        );
        assert_eq!(
            triples.body().hint.as_ref().unwrap()["value"],
            "\"hello, world\""
        );
    }

    #[test]
    fn postgres_unique_detail_parser_handles_functions_and_quoted_commas() {
        assert_eq!(
            parse_unique_detail(
                "Key (app_id, json_null_to_null(value))=(app, \"hello, world\") already exists."
            )
            .unwrap(),
            HashMap::from([
                ("app_id".to_owned(), "app".to_owned()),
                (
                    "json_null_to_null(value)".to_owned(),
                    "\"hello, world\"".to_owned(),
                ),
            ])
        );
        assert!(parse_unique_detail("malformed detail").is_none());
    }

    #[test]
    fn triple_check_details_preserve_values_types_and_unique_size_hints() {
        let attr_id = Uuid::from_u128(2);
        let entity_id = Uuid::from_u128(3);
        let detail = format!(
            "Failing row contains (00000000-0000-0000-0000-000000000001, {entity_id}, {attr_id}, {{\"left\":1,\"right\":2}}, md5, t, f, f, t, f, 1700000000000, number, 128)."
        );
        let typed = classify_transaction_database_error(
            "23514",
            "check",
            Some("triples"),
            Some("valid_value_data_type"),
            Some(&detail),
        );
        assert_eq!(typed.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            typed.body().message,
            "Invalid value type. Value must be a number but the provided value type is object."
        );
        assert_eq!(
            typed.body().hint.as_ref().unwrap()["attr-id"],
            json!(attr_id)
        );
        assert_eq!(
            typed.body().hint.as_ref().unwrap()["entity-id"],
            json!(entity_id)
        );
        assert_eq!(
            typed.body().hint.as_ref().unwrap()["value"],
            json!({"left": 1, "right": 2})
        );

        let unique_detail = detail.replace(", f, f, t, f, 170", ", f, t, t, f, 170");
        let oversized = classify_transaction_database_error(
            "23514",
            "check",
            Some("triples"),
            Some("indexed_values_are_constrained"),
            Some(&unique_detail),
        );
        assert_eq!(
            oversized.body().message,
            "Value is too large for a unique attribute."
        );
        assert_eq!(
            oversized.body().hint.as_ref().unwrap()["value-too-large?"],
            true
        );

        let ref_detail = format!(
            "Failing row contains (00000000-0000-0000-0000-000000000001, {entity_id}, {attr_id}, \"not-a-uuid\", md5, t, t, f, f, t, 1700000000000, null, 128)."
        );
        let invalid_ref = classify_transaction_database_error(
            "23514",
            "check",
            Some("triples"),
            Some("valid_ref_value"),
            Some(&ref_detail),
        );
        assert_eq!(invalid_ref.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            invalid_ref.body().message,
            "Linked value must be a valid uuid."
        );
        assert_eq!(
            invalid_ref.body().hint,
            Some(json!({
                "value": "not-a-uuid",
                "checked-data-type": null,
                "attr-id": attr_id.to_string(),
                "entity-id": entity_id.to_string(),
            }))
        );
    }

    #[test]
    fn foreign_key_details_keep_the_exact_jvm_public_envelope() {
        let error = classify_transaction_database_error(
            "23503",
            "insert or update on table \"triples\" violates foreign key constraint",
            Some("triples"),
            Some("triples_attr_id_fkey"),
            Some("Key (attr_id)=(missing) is not present in table \"attrs\"."),
        );
        assert_eq!(error.body().kind, ApiErrorType::RecordForeignKeyInvalid);
        assert_eq!(
            error.body().message,
            "Foreign Key Invalid: foreign-key-violation"
        );
        assert_eq!(
            error.body().hint,
            Some(json!({
                "table": "triples",
                "condition": "foreign-key-violation",
                "constraint": "triples_attr_id_fkey",
            }))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One matrix keeps each public constraint envelope explicit.
    async fn constraint_errors_keep_exact_envelopes_through_admin_http() {
        let attr_id = Uuid::from_u128(2);
        let entity_id = Uuid::from_u128(3);
        let catalog = AttributeCatalog {
            attributes: vec![instant_instaql::Attribute {
                id: attr_id,
                wire: None,
                etype: "profiles".to_owned(),
                label: "email".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: true,
                setting_unique: false,
                indexed: true,
                required: false,
                checked_data_type: Some(instant_datalog::DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        };
        let triple_unique_detail = format!(
            "Key (app_id, attr_id, json_null_to_null(value))=(00000000-0000-0000-0000-000000000001, {attr_id}, \"hello, world\") already exists."
        );
        let checked_detail = format!(
            "Failing row contains (00000000-0000-0000-0000-000000000001, {entity_id}, {attr_id}, \"forty-two\", md5, t, f, f, t, f, 1700000000000, number, 128)."
        );
        let oversized_detail = format!(
            "Failing row contains (00000000-0000-0000-0000-000000000001, {entity_id}, {attr_id}, \"large\", md5, t, f, t, t, f, 1700000000000, null, 128)."
        );
        let ref_detail = format!(
            "Failing row contains (00000000-0000-0000-0000-000000000001, {entity_id}, {attr_id}, \"not-a-uuid\", md5, t, t, f, f, t, 1700000000000, null, 128)."
        );
        let cases = vec![
            (
                classify_transaction_database_error(
                    "23505",
                    "duplicate key",
                    Some("idents"),
                    Some("app_ident_uq"),
                    Some(
                        "Key (app_id, etype, label)=(00000000-0000-0000-0000-000000000001, users, email) already exists.",
                    ),
                ),
                json!({
                    "type": "record-not-unique",
                    "message": "`email` already exists on `users`",
                    "hint": {"record-type": "ident", "etype": "users", "label": "email"},
                }),
            ),
            (
                classify_transaction_database_error_with_catalog(
                    "23505",
                    "duplicate key",
                    Some("triples"),
                    Some("av_index"),
                    Some(&triple_unique_detail),
                    Some(&catalog),
                ),
                json!({
                    "type": "record-not-unique",
                    "message": "`email` is a unique attribute on `profiles` and an entity already exists with `profiles.email` = \"hello, world\"",
                    "hint": {
                        "record-type": "triple",
                        "attr-id": attr_id,
                        "value": "\"hello, world\"",
                        "etype": "profiles",
                        "label": "email",
                    },
                }),
            ),
            (
                classify_transaction_database_error_with_catalog(
                    "23505",
                    "duplicate key",
                    Some("triples"),
                    Some("triples_unique_value_idx"),
                    Some(&triple_unique_detail.replace(
                        "json_null_to_null(value)",
                        "pg_temp.json_null_to_null(value)",
                    )),
                    Some(&catalog),
                ),
                json!({
                    "type": "record-not-unique",
                    "message": "`email` is a unique attribute on `profiles` and an entity already exists with `profiles.email` = \"hello, world\"",
                    "hint": {
                        "record-type": "triple",
                        "attr-id": attr_id,
                        "value": "\"hello, world\"",
                        "etype": "profiles",
                        "label": "email",
                    },
                }),
            ),
            (
                classify_transaction_database_error_with_catalog(
                    "23514",
                    "check",
                    Some("triples"),
                    Some("valid_value_data_type"),
                    Some(&checked_detail),
                    Some(&catalog),
                ),
                json!({
                    "type": "validation-failed",
                    "message": "Invalid value type for profiles.email. Value must be a number but the provided value type is string.",
                    "hint": {
                        "value": "forty-two",
                        "checked-data-type": "number",
                        "attr-id": attr_id,
                        "entity-id": entity_id,
                        "namespace": "profiles",
                        "attribute": "email",
                    },
                }),
            ),
            (
                classify_transaction_database_error_with_catalog(
                    "23514",
                    "check",
                    Some("triples"),
                    Some("indexed_values_are_constrained"),
                    Some(&oversized_detail),
                    Some(&catalog),
                ),
                json!({
                    "type": "validation-failed",
                    "message": "Value is too large for a unique attribute.",
                    "hint": {
                        "value": "large",
                        "checked-data-type": null,
                        "attr-id": attr_id,
                        "entity-id": entity_id,
                        "namespace": "profiles",
                        "attribute": "email",
                        "value-too-large?": true,
                    },
                }),
            ),
            (
                classify_transaction_database_error_with_catalog(
                    "23514",
                    "check",
                    Some("triples"),
                    Some("valid_ref_value"),
                    Some(&ref_detail),
                    Some(&catalog),
                ),
                json!({
                    "type": "validation-failed",
                    "message": "Linked value must be a valid uuid.",
                    "hint": {
                        "value": "not-a-uuid",
                        "checked-data-type": null,
                        "attr-id": attr_id,
                        "entity-id": entity_id,
                        "namespace": "profiles",
                        "attribute": "email",
                    },
                }),
            ),
            (
                classify_transaction_database_error(
                    "23503",
                    "insert or update on table \"triples\" violates foreign key constraint",
                    Some("triples"),
                    Some("triples_attr_id_fkey"),
                    Some("Key (attr_id)=(missing) is not present in table \"attrs\"."),
                ),
                json!({
                    "type": "record-foreign-key-invalid",
                    "message": "Foreign Key Invalid: foreign-key-violation",
                    "hint": {
                        "table": "triples",
                        "condition": "foreign-key-violation",
                        "constraint": "triples_attr_id_fkey",
                    },
                }),
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(admin_transaction_error_envelope(error).await, expected);
        }
    }

    #[test]
    fn active_catalog_enriches_unique_and_checked_value_errors() {
        let attr_id = Uuid::from_u128(2);
        let entity_id = Uuid::from_u128(3);
        let catalog = AttributeCatalog {
            attributes: vec![instant_instaql::Attribute {
                id: attr_id,
                wire: None,
                etype: "profiles".to_owned(),
                label: "age".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: true,
                setting_unique: false,
                indexed: true,
                required: false,
                checked_data_type: Some(instant_datalog::DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        };
        let unique_detail = format!(
            "Key (app_id, attr_id, json_null_to_null(value))=(00000000-0000-0000-0000-000000000001, {attr_id}, 42) already exists."
        );
        let unique = classify_transaction_database_error_with_catalog(
            "23505",
            "duplicate key",
            Some("triples"),
            Some("av_index"),
            Some(&unique_detail),
            Some(&catalog),
        );
        assert_eq!(
            unique.body().message,
            "`age` is a unique attribute on `profiles` and an entity already exists with `profiles.age` = 42"
        );
        assert_eq!(unique.body().hint.as_ref().unwrap()["etype"], "profiles");
        assert_eq!(unique.body().hint.as_ref().unwrap()["label"], "age");

        let check_detail = format!(
            "Failing row contains (00000000-0000-0000-0000-000000000001, {entity_id}, {attr_id}, \"forty-two\", md5, t, f, f, t, f, 1700000000000, number, 128)."
        );
        let checked = classify_transaction_database_error_with_catalog(
            "23514",
            "check",
            Some("triples"),
            Some("valid_value_data_type"),
            Some(&check_detail),
            Some(&catalog),
        );
        assert_eq!(
            checked.body().message,
            "Invalid value type for profiles.age. Value must be a number but the provided value type is string."
        );
        assert_eq!(
            checked.body().hint.as_ref().unwrap()["namespace"],
            "profiles"
        );
        assert_eq!(checked.body().hint.as_ref().unwrap()["attribute"], "age");
    }

    #[test]
    fn reserved_identity_flag_accepts_only_two_string_tuples() {
        assert_eq!(
            reserved_identity_names(&json!([
                ["$users", "futureField"],
                ["profiles", "owner"],
                ["too", "many", "parts"],
                ["missing"],
                ["wrong", 42],
                null,
                ["$users", "futureField"]
            ])),
            HashSet::from([
                ("$users".to_owned(), "futureField".to_owned()),
                ("profiles".to_owned(), "owner".to_owned()),
            ])
        );
        assert!(reserved_identity_names(&json!(null)).is_empty());
        assert_eq!(
            transaction_execution_error(ExecutionError::ReservedIdentity {
                etype: "$users".to_owned(),
                label: "futureField".to_owned(),
            }),
            instant_http::ApiError::new(
                ApiErrorType::ValidationFailed,
                "$users.futureField is reserved by the system catalog",
            )
        );
    }

    #[test]
    fn byop_pool_versions_rotate_for_database_or_namespace_changes() {
        let original = byop_connection_version("postgresql://db/one", "public");

        assert_eq!(
            original,
            byop_connection_version("postgresql://db/one", "public")
        );
        assert_ne!(
            original,
            byop_connection_version("postgresql://db/two", "public")
        );
        assert_ne!(
            original,
            byop_connection_version("postgresql://db/one", "tenant")
        );
    }

    #[tokio::test]
    async fn retired_byop_generation_cannot_remove_its_replacement() {
        let app_id = Uuid::new_v4();
        let version = [7; 32];
        let registry = Arc::new(AsyncMutex::new(HashMap::new()));
        let retired = empty_byop_pool(app_id, version, &registry);
        let replacement = empty_byop_pool(app_id, version, &registry);

        let mut pools = registry.lock().await;
        pools.insert(app_id, Arc::clone(&retired));
        retired.retire();
        pools.insert(app_id, Arc::clone(&replacement));
        drop(pools);
        tokio::task::yield_now().await;

        assert!(retired.is_retired());
        assert!(
            matching_byop_pool(&*registry.lock().await, app_id, version)
                .is_some_and(|pool| Arc::ptr_eq(&pool, &replacement))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn live_realtime_batch_uses_one_byop_context_for_ordered_query_modes() {
        let Some(database_url) = instant_test_support::test_database_url(
            "live_realtime_batch_uses_one_byop_context_for_ordered_query_modes",
        ) else {
            return;
        };
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (primary, primary_connection) =
            tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
                .await
                .unwrap();
        let primary_driver = tokio::spawn(primary_connection);
        let app_id = Uuid::new_v4();
        let creator_id = Uuid::new_v4();
        let namespace = format!("byop_batch_{}", Uuid::new_v4().simple());
        primary
            .batch_execute(&format!(
                "CREATE SCHEMA {namespace};
                 CREATE TABLE {namespace}.users (id uuid PRIMARY KEY, name text NOT NULL);
                 CREATE TABLE {namespace}.posts (id uuid PRIMARY KEY, title text NOT NULL);"
            ))
            .await
            .unwrap();
        let mut byop_url = url::Url::parse(&database_url).unwrap();
        byop_url
            .query_pairs_mut()
            .append_pair("currentSchema", &namespace);
        let cipher = Arc::new(test_secret_cipher());
        let encrypted = cipher.encrypt_bytes(app_id, byop_url.as_str()).unwrap();
        primary
            .execute(
                "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
                &[&creator_id, &format!("{creator_id}@example.test")],
            )
            .await
            .unwrap();
        primary
            .execute(
                "INSERT INTO apps (id, creator_id, title, connection_string)
                 VALUES ($1, $2, 'realtime batch test', $3)",
                &[&app_id, &creator_id, &encrypted],
            )
            .await
            .unwrap();

        let service = AdminDataService::new(primary, Arc::new(CountingAuthorizer::default()))
            .with_byop_cipher(cipher);
        let context = AdminRequestContext {
            app_id,
            bearer_token: Some("admin-secret".to_owned()),
            actor: AdminActor::Admin,
            client_ip: None,
        };
        let query = |name: &str, mode| RuntimeRealtimeBatchQuery {
            context: context.clone(),
            query: json!({(name): {}}).as_object().unwrap().clone(),
            versions: None,
            mode,
        };
        let batch = service
            .refresh_realtime_batch(
                app_id,
                AttributeCatalog::default(),
                vec![
                    query("users", RuntimeRealtimeQueryMode::Tree),
                    query("posts", RuntimeRealtimeQueryMode::JoinRows),
                ],
            )
            .await
            .unwrap();
        assert!(batch.external_catalog);
        assert_eq!(
            batch
                .catalog
                .attributes
                .iter()
                .map(|attr| attr.etype.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["users", "posts"])
        );
        let results = batch.results.unwrap();
        assert_eq!(results.len(), 2);
        let tree = results[0].as_ref().unwrap();
        assert!(tree.result.get("users").is_some());
        assert!(tree.byop_topics.is_none());
        let admin_join_rows = results[1].as_ref().unwrap();
        assert!(admin_join_rows.result.is_array());
        assert_eq!(admin_join_rows.result_meta, json!({}));
        assert!(
            admin_join_rows
                .byop_topics
                .as_ref()
                .is_some_and(|topics| !topics.is_empty())
        );

        let primary = service.client.acquire().await.unwrap();
        primary
            .execute("DELETE FROM apps WHERE id = $1", &[&app_id])
            .await
            .unwrap();
        primary
            .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
            .await
            .unwrap();
        primary
            .batch_execute(&format!("DROP SCHEMA {namespace} CASCADE"))
            .await
            .unwrap();
        drop(primary);
        drop(service);
        primary_driver.abort();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn canceled_byop_lease_is_discarded_and_generation_is_replaced() {
        let Some(database_url) = instant_test_support::test_database_url(
            "canceled_byop_lease_is_discarded_and_generation_is_replaced",
        ) else {
            return;
        };
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (primary, primary_connection) =
            tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
                .await
                .unwrap();
        let primary_driver = tokio::spawn(primary_connection);
        let service = AdminDataService::new(primary, Arc::new(NeverAuthorizer));
        let app_id = Uuid::new_v4();
        let version = [9; 32];
        let pool = service
            .resolve_byop_pool(app_id, &database_url, version)
            .await
            .unwrap();

        let unaffected = pool.checkout().await.unwrap();
        let unaffected_pid = unaffected
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        let tracker = crate::statement_cancel::StatementTracker::default();
        let scoped_tracker = tracker.clone();
        let query_pool = Arc::clone(&pool);
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let query_task = tokio::spawn(async move {
            scoped_tracker
                .scope(async move {
                    let client = query_pool.checkout().await.unwrap();
                    let pid = client
                        .query_one("SELECT pg_backend_pid()", &[])
                        .await
                        .unwrap()
                        .get::<_, i32>(0);
                    pid_tx.send(pid).unwrap();
                    client
                        .simple_query("SELECT pg_sleep(10) /* byop_cancel_generation */")
                        .await
                })
                .await
        });
        let canceled_pid = pid_rx.await.unwrap();
        assert_ne!(canceled_pid, unaffected_pid);

        let (observer, observer_connection) =
            tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
                .await
                .unwrap();
        let observer_driver = tokio::spawn(observer_connection);
        wait_for_backend_query(&observer, canceled_pid, "byop_cancel_generation").await;
        let outcome = tracker
            .cancel_and_wait(std::time::Duration::from_secs(2))
            .await;
        assert_eq!(outcome.requested, 1);
        assert_eq!(outcome.completed, 1);
        assert!(!outcome.timed_out);
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), query_task)
            .await
            .expect("canceled BYOP handler did not finish")
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        );
        wait_for_backend_exit(&observer, canceled_pid).await;

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if !service
                    .byop_pools
                    .lock()
                    .await
                    .get(&app_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &pool))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retired BYOP generation was not evicted");
        assert!(pool.is_retired());
        assert!(pool.checkout().await.is_none());

        let row = unaffected
            .query_one("SELECT pg_backend_pid(), 1::int", &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, i32>(0), unaffected_pid);
        assert_eq!(row.get::<_, i32>(1), 1);
        drop(unaffected);
        wait_for_backend_exit(&observer, unaffected_pid).await;

        let (replacement, concurrent_replacement) = tokio::join!(
            service.resolve_byop_pool(app_id, &database_url, version),
            service.resolve_byop_pool(app_id, &database_url, version),
        );
        let replacement = replacement.unwrap();
        let concurrent_replacement = concurrent_replacement.unwrap();
        assert!(!Arc::ptr_eq(&replacement, &pool));
        assert!(Arc::ptr_eq(&replacement, &concurrent_replacement));
        let replacement_client = replacement.checkout().await.unwrap();
        let replacement_pid = replacement_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert_ne!(replacement_pid, canceled_pid);
        assert_ne!(replacement_pid, unaffected_pid);

        drop(replacement_client);
        drop(observer);
        observer_driver.await.unwrap().unwrap();
        drop(service);
        primary_driver.await.unwrap().unwrap();
    }

    #[test]
    fn byop_namespace_matches_the_jvm_current_schema_default() {
        assert_eq!(
            byop_namespace("postgresql://db.example/instant?currentSchema=tenant_42").unwrap(),
            "tenant_42"
        );
        assert_eq!(
            byop_namespace("jdbc:postgresql://db.example/instant?currentSchema=customer%20data")
                .unwrap(),
            "customer data"
        );
        assert_eq!(
            byop_namespace("postgresql://db.example/instant").unwrap(),
            "public"
        );
        assert_eq!(
            byop_driver_connection_string(
                "jdbc:postgresql://db.example/instant?currentSchema=tenant_42&sslmode=require"
            )
            .unwrap(),
            "postgresql://db.example/instant?sslmode=require"
        );
    }

    #[test]
    fn byop_realtime_catalog_keeps_external_ids_before_primary_transaction_identities() {
        let attribute = |id, value_type| instant_instaql::Attribute {
            id,
            wire: None,
            etype: "teams".to_owned(),
            label: "id".to_owned(),
            value_type,
            cardinality: Cardinality::One,
            unique: true,
            setting_unique: false,
            indexed: true,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        };
        let external_id = Uuid::from_u128(1);
        let instant_id = Uuid::from_u128(2);
        let merged = realtime_client_catalog(
            AttributeCatalog {
                attributes: vec![attribute(external_id, AttributeValueType::Blob)],
            },
            AttributeCatalog {
                attributes: vec![attribute(instant_id, AttributeValueType::Blob)],
            },
        );

        assert_eq!(
            merged
                .attributes
                .iter()
                .map(|attribute| attribute.id)
                .collect::<Vec<_>>(),
            vec![external_id, instant_id]
        );
    }

    #[test]
    fn byop_framework_projection_preserves_arbitrary_ids_and_synthetic_attributes() {
        let id = Uuid::from_u128(1);
        let title = Uuid::from_u128(2);
        let attribute = |id, label: &str| instant_instaql::Attribute {
            id,
            wire: None,
            etype: "posts".to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "post_key",
            setting_unique: false,
            indexed: label == "post_key",
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        };
        let byop = ByopContext {
            namespace: "tenant".to_owned(),
            schema: ByopSchema {
                catalog: AttributeCatalog {
                    attributes: vec![attribute(id, "post_key"), attribute(title, "title")],
                },
                tables: BTreeMap::from([(
                    "posts".to_owned(),
                    ByopTable {
                        fields: BTreeMap::from([
                            (
                                "post_key".to_owned(),
                                ByopField {
                                    database_type: "int8".to_owned(),
                                    attribute_id: id,
                                    indexed: true,
                                    unique: true,
                                },
                            ),
                            (
                                "title".to_owned(),
                                ByopField {
                                    database_type: "text".to_owned(),
                                    attribute_id: title,
                                    indexed: false,
                                    unique: false,
                                },
                            ),
                        ]),
                        attribute_id_to_field: BTreeMap::from([
                            (id, "post_key".to_owned()),
                            (title, "title".to_owned()),
                        ]),
                        relations: BTreeMap::new(),
                        primary_key: ByopPrimaryKey {
                            field: "post_key".to_owned(),
                            attribute_id: id,
                        },
                        created_at_field: None,
                    },
                )]),
            },
        };
        let response = ByopResponse {
            data: BTreeMap::from([(
                "posts".to_owned(),
                json!([{"post_key": 42, "title": "native"}]),
            )]),
            page_info: BTreeMap::from([(
                "posts".to_owned(),
                ByopPageInfo {
                    start_cursor: Some(vec![json!(42)]),
                    end_cursor: Some(vec![json!(42)]),
                },
            )]),
            aggregates: BTreeMap::from([(
                "posts".to_owned(),
                BTreeMap::from([("count".to_owned(), 1)]),
            )]),
        };
        let projected = byop_framework_result(&byop, &json!({"posts": {}}), &response).unwrap();
        let rows = projected[0]["data"]["datalog-result"]["join-rows"][0]
            .as_array()
            .unwrap();
        assert!(rows.contains(&json!([42, id, 42])));
        assert!(rows.contains(&json!([42, title, "native"])));
        assert_eq!(projected[0]["data"]["aggregate"]["posts"]["count"], 1);
        assert_eq!(
            projected[0]["data"]["page-info"]["posts"]["start-cursor"],
            json!([42])
        );
        assert_eq!(
            framework_attr(&byop.schema.catalog.attributes[0])["forward-identity"],
            json!([id, "posts", "post_key"])
        );
        assert_eq!(
            framework_attr(&byop.schema.catalog.attributes[0])["inferred-types"],
            Value::Null
        );
    }

    #[test]
    fn framework_projection_uses_persisted_wire_metadata() {
        let attr_id = Uuid::from_u128(1);
        let forward_id = Uuid::from_u128(2);
        let reverse_id = Uuid::from_u128(3);
        let attribute = instant_instaql::Attribute {
            id: attr_id,
            wire: Some(instant_instaql::AttributeWireMetadata {
                forward_identity_id: forward_id,
                reverse_identity_id: Some(reverse_id),
                inferred_types: Some(vec!["number".to_owned(), "boolean".to_owned()]),
            }),
            etype: "posts".to_owned(),
            label: "author".to_owned(),
            value_type: AttributeValueType::Ref,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: Some("users".to_owned()),
            reverse_label: Some("posts".to_owned()),
            on_delete: None,
            on_delete_reverse: None,
        };

        let projected = framework_attr(&attribute);
        assert_eq!(
            projected["forward-identity"],
            json!([forward_id, "posts", "author"])
        );
        assert_eq!(
            projected["reverse-identity"],
            json!([reverse_id, "users", "posts"])
        );
        assert_eq!(projected["inferred-types"], json!(["number", "boolean"]));
    }
}
