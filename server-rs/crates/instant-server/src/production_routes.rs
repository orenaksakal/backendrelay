use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::response::{IntoResponse as _, Response};
use axum::{Router, middleware};
use instant_http::{
    AllBackends, ApiError, ApiErrorType, DashboardGatewayToken, HealthState, ServerVersion,
    app_router_with_all_backends, apply_http_response_policy,
};
use instant_permissions::RateLimitConsumer;
use instant_pg::{
    ByopMatchSet, ByopSchema, ByopTopic, ByopWalRecord, ReplicationCancellationToken,
    ReplicationTransportError, SketchPersistenceError, bootstrap_attr_sketches,
    create_exported_snapshot_slot, drop_logical_replication_slot, ensure_logical_replication_slot,
    get_logical_replication_slot, introspect_byop_schema, recreate_invalidated_slot,
    replication_slot_is_lost, sketch_aggregator_checkpoint, validate_replication_start,
};
use instant_realtime::RealtimeStore;
use instant_transact::{TransactionOptions, WalEntityLog};
use instant_wal::{Isn, Lsn, SlotPurpose};
use ipnet::IpNet;
use thiserror::Error;
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::admin_data::{byop_driver_connection_string, byop_namespace};
use crate::metrics::{ReplicationSlotPurpose, record_replication_slot_recreated};
use crate::{
    AdminAuthService, AdminDataService, AdminMetadataService, AdminPresenceService,
    AdminRealtimeService, AdminStorageService, AppRateLimitPolicy, AwsKmsTinkKeysetLoader,
    BackupObjectControlHooks, CombineTransactsPolicy, CustodianTickOutcome,
    DEFAULT_RATE_LIMIT_SWEEP_MAX_LOOPS, DashboardConfigService, DashboardControlService,
    DashboardIdentityService, DashboardRateLimitHooks, DashboardService,
    DashboardStorageControlHooks, DirectPostgresSnapshotProvider, DurableInvalidationSink,
    Ed25519WebhookKeys, FailoverGateError, GuardedWebhookHttpTransport, InvalidationRuntimeError,
    InvalidationSupervisorError, JvmTaskSchedules, MigrationError, NightlyBackupSupervisor,
    NoopDashboardControlHooks, NoopInvalidationSink, OAuthSecretCipher, PlatformOAuthService,
    PostgresAdminDataAuthorizer, PostgresAdminTransactionQueuePolicy, PostgresAppRateLimitPolicy,
    PostgresBackupControlHooks, PostgresBackupRestoreControlHooks, PostgresBackupWorker,
    PostgresBackupWorkerPool, PostgresCoarseTopicsPolicy, PostgresCustodian,
    PostgresDailyMetricsWorker, PostgresDashboardIdentityPolicy, PostgresDashboardPolicyHooks,
    PostgresDashboardSummaryProvider, PostgresDatalogCachePolicy, PostgresEphemeralAppSweeper,
    PostgresFailoverGate, PostgresHttpResponsePolicy, PostgresIndexingControlHooks,
    PostgresIndexingWorker, PostgresInvalidationHealthObserver, PostgresInvalidationQueuePolicy,
    PostgresJoinRoomLogger, PostgresMagicCodeExpiryPolicy, PostgresNightlyBackupWorker,
    PostgresPartitionRetention, PostgresPlatformOAuthStore, PostgresPool, PostgresPoolError,
    PostgresRateLimitConsumer, PostgresRateLimitSweeper, PostgresRealtimeCluster,
    PostgresReceiveTimeoutPolicy, PostgresRefreshThrottlePolicy, PostgresReplicationSlotCleaner,
    PostgresRestoreControlHooks, PostgresRestoreWorker, PostgresRuntimeSessionReportCollector,
    PostgresRuntimeStreamBackend, PostgresSenderDomainRecheck, PostgresSseRetryIntervalPolicy,
    PostgresStorageAccessPolicy, PostgresStorageSweeper, PostgresTransactionPoolPressurePolicy,
    PostgresTriplesSizeCollector, PostgresWalHealth, PostgresWebhookLimitPolicy,
    PostgresWebhookWorker, ProductionDashboardTerminalHooks, ProductionDatabaseBootstrapError,
    ProductionDatabaseClients, ProductionPlatformError, ProductionRuntimeOAuthService,
    ProductionSnapshotProvider, QueuedRealtimeInvalidationConsumer, RATE_LIMIT_SWEEP_BACKLOG_ERROR,
    RATE_LIMIT_SWEEP_BATCH_SIZE, RealtimeInvalidationSink, RealtimeWatermarkSeed, ReconnectPolicy,
    RefreshThrottlePolicy, RejectUnmatchedDashboard, RuntimeAuthConfig, RuntimeRealtimeService,
    S3ObjectStore, S3PresigningCredentials, ScheduledTask, ServerConfig, ServerEnvironment,
    StorageFileUrlSigner, SystemCatalogVerificationError, TaskSchedule, TinkKeysetConfig,
    TinkKeysetLoadError, TriplesSizeCollectionConfig, WebhookDashboardControlHooks,
    WebhookEventWakeupNotifier, app_rate_limit_middleware, configured_storage_sweep_max_loops,
    drop_process_invalidator_slot, production_dashboard_adapters_with_runtime_pools,
    production_platform_service_from_config_with_pools, production_runtime_auth_service_from_pool,
    production_runtime_oauth_adapters_from_pool, s3_object_store_for_bucket_from_config,
    s3_object_store_from_config, supervise_byop_invalidation_stream,
    supervise_invalidation_stream_with_health, transaction_retention_days_from_env,
    verify_database_migrations, verify_system_catalog,
};

pub type ProductionAdminData = AdminDataService<PostgresAdminDataAuthorizer>;
pub type ProductionRuntimeRealtime =
    RuntimeRealtimeService<PostgresAdminDataAuthorizer, ProductionAdminData>;
pub type ProductionAdminRealtime =
    AdminRealtimeService<PostgresAdminDataAuthorizer, ProductionAdminData>;

// These fresh reads must not inherit DatabaseWorkload::Refresh and queue behind
// the data queries they authorize. Keep construction in one place so production
// wiring and the saturated-pool regression exercise the same reservation.
struct RuntimePostgresPolicies {
    app_rate_limits: Arc<PostgresAppRateLimitPolicy>,
    refresh_throttle: Arc<PostgresRefreshThrottlePolicy>,
    coarse_topics: Arc<PostgresCoarseTopicsPolicy>,
    datalog_cache: Arc<PostgresDatalogCachePolicy>,
    effective_app_status_policy: Arc<crate::PostgresEffectiveAppStatusPolicy>,
    receive_timeout: Arc<PostgresReceiveTimeoutPolicy>,
    statement_cancel_wait: Arc<crate::PostgresStatementCancelWaitPolicy>,
    sse_retry_interval: Arc<PostgresSseRetryIntervalPolicy>,
    query_preparation: Arc<PostgresFailoverGate>,
    query_controls: Arc<crate::failover::QueryControlCache>,
    refresh_read_controls: Arc<crate::PostgresRefreshReadControlsPolicy>,
    control_plane: Arc<crate::ControlPlaneCache>,
}

impl RuntimePostgresPolicies {
    fn from_primary_pool(pool: &PostgresPool, administrator_email: Option<String>) -> Self {
        let policy = pool.control_pool();
        let query_controls = Arc::new(crate::failover::QueryControlCache::from_pool(
            policy.clone(),
        ));
        let control_plane = Arc::new(crate::ControlPlaneCache::new(crate::INSTANT_CONFIG_APP_ID));
        let effective_app_status_policy = Arc::new(
            crate::PostgresEffectiveAppStatusPolicy::from_pool(policy.clone())
                .with_batched_reads()
                .with_control_plane_cache(Arc::clone(&control_plane)),
        );
        Self {
            app_rate_limits: Arc::new(
                PostgresAppRateLimitPolicy::from_pool(policy.clone(), administrator_email)
                    .with_control_plane_cache(Arc::clone(&control_plane)),
            ),
            refresh_throttle: Arc::new(
                PostgresRefreshThrottlePolicy::from_pool(policy.clone())
                    .with_query_control_cache(Arc::clone(&query_controls)),
            ),
            coarse_topics: Arc::new(
                PostgresCoarseTopicsPolicy::from_pool(policy.clone())
                    .with_query_control_cache(Arc::clone(&query_controls)),
            ),
            datalog_cache: Arc::new(PostgresDatalogCachePolicy::from_pool(policy.clone())),
            effective_app_status_policy: Arc::clone(&effective_app_status_policy),
            receive_timeout: Arc::new(
                PostgresReceiveTimeoutPolicy::from_pool(policy.clone())
                    .with_control_plane_cache(Arc::clone(&control_plane)),
            ),
            statement_cancel_wait: Arc::new(
                crate::PostgresStatementCancelWaitPolicy::from_pool(policy.clone())
                    .with_batched_reads()
                    .with_control_plane_cache(Arc::clone(&control_plane)),
            ),
            sse_retry_interval: Arc::new(PostgresSseRetryIntervalPolicy::from_pool(policy.clone())),
            query_preparation: Arc::new(
                PostgresFailoverGate::from_pool(policy.clone())
                    .with_query_control_cache(Arc::clone(&query_controls)),
            ),
            refresh_read_controls: Arc::new(
                crate::PostgresRefreshReadControlsPolicy::from_pool(policy)
                    .with_query_control_cache(Arc::clone(&query_controls))
                    .with_status_policy(effective_app_status_policy),
            ),
            query_controls,
            control_plane,
        }
    }
}

const INVALIDATOR_SLOT_NUM: i32 = 2;
pub type ProductionNightlyBackup =
    NightlyBackupSupervisor<S3ObjectStore, ProductionSnapshotProvider>;
pub type ProductionInvalidationSink = RealtimeInvalidationSink<
    crate::PostgresDurableInvalidationSink<WebhookEventWakeupNotifier>,
    (
        (
            Arc<crate::failover::QueryControlCache>,
            Arc<crate::ControlPlaneCache>,
        ),
        QueuedRealtimeInvalidationConsumer<(
            Arc<ProductionRuntimeRealtime>,
            Arc<ProductionAdminRealtime>,
        )>,
    ),
>;

/// Secret-managed production inputs. This deliberately has no `Debug`
/// implementation because every field can contain long-lived key material.
pub struct ProductionServerSecrets {
    pub oauth_keyset: TinkKeysetConfig,
    pub webhook_keyset: TinkKeysetConfig,
    pub rate_limit_hmac_key: Arc<[u8]>,
    pub s3_presigning: Option<S3PresigningCredentials>,
    pub dashboard_private: bool,
    pub dashboard_gateway: Option<DashboardGatewayToken>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProductionServerSecretsError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error(
        "INSTANT_S3_PRESIGN_ACCESS_KEY_ID and INSTANT_S3_PRESIGN_SECRET_ACCESS_KEY must be set together"
    )]
    IncompleteS3PresigningCredentials,
    #[error(
        "INSTANT_DASHBOARD_GATEWAY_TOKEN must contain 32–128 header-safe alphanumeric, hyphen, or underscore characters"
    )]
    InvalidDashboardGatewayToken,
    #[error("INSTANT_DASHBOARD_PRIVATE must be true or false")]
    InvalidDashboardPrivateMode,
    #[error("{0} is not supported by the self-hosted instant-server build")]
    AwsHostedFeatureRequired(&'static str),
}

impl ProductionServerSecrets {
    /// Loads key material references from environment variables without ever
    /// formatting their values into an error or debug representation.
    ///
    /// # Errors
    ///
    /// Returns a name-only error for missing keysets or incomplete static S3
    /// presigning credentials.
    pub fn from_env() -> Result<Self, ProductionServerSecretsError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Builds secret input from explicit environment-style pairs.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::from_env`].
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, ProductionServerSecretsError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let values = pairs
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<HashMap<_, _>>();
        Self::from_lookup(|name| values.get(name).cloned())
    }

    fn from_lookup(
        mut get: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ProductionServerSecretsError> {
        let keyset = |json_name: &'static str,
                      kms_name: &'static str,
                      get: &mut dyn FnMut(&str) -> Option<String>|
         -> Result<TinkKeysetConfig, ProductionServerSecretsError> {
            let json = get(json_name)
                .filter(|value| !value.trim().is_empty())
                .ok_or(ProductionServerSecretsError::Missing(json_name))?;
            let kms_key_url = get(kms_name).filter(|value| !value.trim().is_empty());
            if kms_key_url.is_some() {
                return Err(ProductionServerSecretsError::AwsHostedFeatureRequired(
                    kms_name,
                ));
            }
            Ok(match kms_key_url {
                Some(kms_key_url) => TinkKeysetConfig::encrypted(json, kms_key_url),
                None => TinkKeysetConfig::cleartext(json),
            })
        };
        let oauth_keyset = keyset(
            "INSTANT_AEAD_KEYSET_JSON",
            "INSTANT_AEAD_KEYSET_KMS_KEY_URL",
            &mut get,
        )?;
        let webhook_keyset = keyset(
            "INSTANT_WEBHOOK_KEYSET_JSON",
            "INSTANT_WEBHOOK_KEYSET_KMS_KEY_URL",
            &mut get,
        )?;
        let rate_limit_hmac_key = get("INSTANT_RATE_LIMIT_HMAC_KEY")
            .filter(|value| !value.is_empty())
            .ok_or(ProductionServerSecretsError::Missing(
                "INSTANT_RATE_LIMIT_HMAC_KEY",
            ))?;
        let access_key =
            get("INSTANT_S3_PRESIGN_ACCESS_KEY_ID").filter(|value| !value.trim().is_empty());
        let secret_key =
            get("INSTANT_S3_PRESIGN_SECRET_ACCESS_KEY").filter(|value| !value.trim().is_empty());
        let s3_presigning = match (access_key, secret_key) {
            (None, None) => None,
            (Some(access_key), Some(secret_key)) => Some(S3PresigningCredentials {
                access_key,
                secret_key,
                session_token: get("INSTANT_S3_PRESIGN_SESSION_TOKEN")
                    .filter(|value| !value.trim().is_empty()),
            }),
            _ => return Err(ProductionServerSecretsError::IncompleteS3PresigningCredentials),
        };
        let dashboard_private = match get("INSTANT_DASHBOARD_PRIVATE")
            .filter(|value| !value.trim().is_empty())
            .as_deref()
        {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(ProductionServerSecretsError::InvalidDashboardPrivateMode),
        };
        let gateway_token =
            get("INSTANT_DASHBOARD_GATEWAY_TOKEN").filter(|value| !value.trim().is_empty());
        if dashboard_private && gateway_token.is_none() {
            return Err(ProductionServerSecretsError::Missing(
                "INSTANT_DASHBOARD_GATEWAY_TOKEN",
            ));
        }
        let dashboard_gateway = gateway_token
            .map(|value| {
                DashboardGatewayToken::new(value)
                    .ok_or(ProductionServerSecretsError::InvalidDashboardGatewayToken)
            })
            .transpose()?;
        Ok(Self {
            oauth_keyset,
            webhook_keyset,
            rate_limit_hmac_key: Arc::from(rate_limit_hmac_key.into_bytes()),
            s3_presigning,
            dashboard_private,
            dashboard_gateway,
        })
    }
}

pub struct ProductionRouteAssembly {
    pub router: Router,
    pub health: HealthState,
    pub tasks: Vec<ScheduledTask>,
    pub runtime_realtime: Arc<ProductionRuntimeRealtime>,
    pub admin_realtime: Arc<ProductionAdminRealtime>,
    pub invalidator: ProductionInvalidator,
    pub backup_pool: Arc<PostgresBackupWorkerPool<S3ObjectStore>>,
    pub nightly_backup: Option<Arc<ProductionNightlyBackup>>,
}

pub struct ProductionInvalidator {
    connection_string: String,
    slot_name: String,
    confirmed_lsn: instant_wal::Lsn,
    sink: ProductionInvalidationSink,
    byop_control_pool: PostgresPool,
    byop_cipher: OAuthSecretCipher,
    runtime_realtime: Arc<ProductionRuntimeRealtime>,
    admin_realtime: Arc<ProductionAdminRealtime>,
    instant_on_instant_app_id: Option<Uuid>,
    sketch_aggregator_pool: PostgresPool,
    sketch_aggregator_snapshot_pool: PostgresPool,
    sketch_aggregator_checkpoint: Lsn,
    sketch_batch_policy: crate::SketchBatchPolicy,
    process_id: String,
    health_observer: PostgresInvalidationHealthObserver,
}

#[derive(Clone, Eq, PartialEq)]
struct ProductionByopConfig {
    app_id: Uuid,
    connection_string: String,
    namespace: String,
}

struct RunningByopInvalidator {
    config: ProductionByopConfig,
    cancellation: ReplicationCancellationToken,
    handle: tokio::task::JoinHandle<Result<(), ProductionByopWorkerError>>,
}

#[derive(Debug)]
enum ProductionByopWorkerError {
    Retryable(String),
    FatalSlotLoss(String),
}

impl From<String> for ProductionByopWorkerError {
    fn from(error: String) -> Self {
        Self::Retryable(error)
    }
}

fn classify_byop_worker_error(
    app_id: Uuid,
    error: &InvalidationSupervisorError,
) -> ProductionByopWorkerError {
    let fatal_slot_loss = matches!(
        &error,
        InvalidationSupervisorError::Runtime(InvalidationRuntimeError::Transport(transport))
            if crate::replication::unrecoverable_slot_loss(transport)
    );
    if fatal_slot_loss {
        ProductionByopWorkerError::FatalSlotLoss(format!(
            "BYOP replication slot for app {app_id} was lost and cannot be recreated safely: {error}"
        ))
    } else {
        ProductionByopWorkerError::Retryable(error.to_string())
    }
}

#[derive(Eq, PartialEq)]
struct ByopReconciliation {
    stop: Vec<Uuid>,
    start: Vec<ProductionByopConfig>,
}

fn reconcile_byop_configs(
    current: &BTreeMap<Uuid, ProductionByopConfig>,
    desired: &BTreeMap<Uuid, ProductionByopConfig>,
) -> ByopReconciliation {
    let stop = current
        .iter()
        .filter_map(|(app_id, config)| (desired.get(app_id) != Some(config)).then_some(*app_id))
        .collect();
    let start = desired
        .iter()
        .filter_map(|(app_id, config)| {
            (current.get(app_id) != Some(config)).then_some(config.clone())
        })
        .collect();
    ByopReconciliation { stop, start }
}

impl ProductionInvalidator {
    /// Runs the per-process wal2json invalidator until cancellation or a
    /// permanent replication/pipeline failure.
    ///
    /// # Errors
    ///
    /// Returns permanent connection, protocol, decoding, or downstream
    /// invalidation errors after transient reconnects are exhausted.
    pub async fn run(
        self,
        cancellation: ReplicationCancellationToken,
    ) -> Result<(), InvalidationSupervisorError> {
        let Self {
            connection_string,
            slot_name,
            confirmed_lsn,
            mut sink,
            byop_control_pool,
            byop_cipher,
            runtime_realtime,
            admin_realtime,
            instant_on_instant_app_id,
            sketch_aggregator_pool,
            sketch_aggregator_snapshot_pool,
            sketch_aggregator_checkpoint,
            sketch_batch_policy,
            process_id,
            health_observer,
        } = self;
        let instant_database_url = connection_string.clone();
        let shutdown_control_pool = byop_control_pool.clone();
        let mut supervisors = tokio::task::JoinSet::new();
        let primary_cancellation = cancellation.clone();
        let primary_control_pool = byop_control_pool.clone();
        let primary_runtime_realtime = Arc::clone(&runtime_realtime);
        let primary_slot_name = slot_name.clone();
        supervisors.spawn(Box::pin(async move {
            run_primary_invalidator(
                &connection_string,
                &primary_slot_name,
                confirmed_lsn,
                &primary_control_pool,
                &primary_runtime_realtime,
                &primary_cancellation,
                &mut sink,
                health_observer,
            )
            .await
        }));
        let aggregator_cancellation = cancellation.clone();
        let aggregator_connection_string = instant_database_url.clone();
        supervisors.spawn(async move {
            run_sketch_aggregator_supervisor(
                &aggregator_connection_string,
                &process_id,
                sketch_aggregator_pool,
                sketch_aggregator_snapshot_pool,
                sketch_aggregator_checkpoint,
                sketch_batch_policy,
                &aggregator_cancellation,
            )
            .await
        });
        let byop_cancellation = cancellation.clone();
        supervisors.spawn(async move {
            run_byop_reconciler(
                byop_control_pool,
                byop_cipher,
                runtime_realtime,
                admin_realtime,
                instant_database_url,
                instant_on_instant_app_id,
                byop_cancellation,
            )
            .await
        });

        let graceful_shutdown = cancellation.is_cancelled();
        let Some(result) = supervisors.join_next().await else {
            return Ok(());
        };
        cancellation.cancel();
        while supervisors.join_next().await.is_some() {}
        result??;
        if graceful_shutdown {
            // The coordinated shutdown drains connections before it cancels
            // this supervisor, so the replication connection is already closed.
            drop_process_invalidator_slot(&shutdown_control_pool, &slot_name).await;
        }
        Ok(())
    }

    #[must_use]
    pub fn slot_name(&self) -> &str {
        &self.slot_name
    }
}

/// A slot that stayed healthy for this long resets recovery backoff, so a later
/// independent loss still starts at the short delay.
const SLOT_RECOVERY_STABLE: Duration = Duration::from_secs(60);

fn slot_recovery_delay(retry: u32) -> Duration {
    ReconnectPolicy::new(Duration::from_secs(5), Duration::from_secs(60)).delay(retry)
}

/// Returns `true` when the delay elapsed and `false` when shutdown won.
async fn wait_for_slot_recovery(
    cancellation: &ReplicationCancellationToken,
    delay: Duration,
) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => false,
        () = tokio::time::sleep(delay) => true,
    }
}

fn invalidator_slot_loss(
    error: &InvalidationSupervisorError,
) -> Option<&ReplicationTransportError> {
    let InvalidationSupervisorError::Runtime(InvalidationRuntimeError::Transport(transport)) =
        error
    else {
        return None;
    };
    crate::replication::unrecoverable_slot_loss(transport).then_some(transport)
}

fn sketch_slot_loss(
    error: &crate::SketchAggregatorSupervisorError,
) -> Option<&ReplicationTransportError> {
    let crate::SketchAggregatorSupervisorError::Runtime(
        crate::SketchAggregatorRuntimeError::Transport(transport),
    ) = error
    else {
        return None;
    };
    crate::replication::unrecoverable_slot_loss(transport).then_some(transport)
}

#[allow(clippy::too_many_arguments)] // Recovery owns every handle the primary stream needs.
async fn run_primary_invalidator(
    connection_string: &str,
    slot_name: &str,
    mut confirmed_lsn: Lsn,
    control_pool: &PostgresPool,
    runtime_realtime: &ProductionRuntimeRealtime,
    cancellation: &ReplicationCancellationToken,
    sink: &mut ProductionInvalidationSink,
    health: PostgresInvalidationHealthObserver,
) -> Result<(), InvalidationSupervisorError> {
    let mut retry = 0_u32;
    loop {
        let started = Instant::now();
        let mut store = RealtimeStore::default();
        let result = Box::pin(supervise_invalidation_stream_with_health(
            connection_string,
            slot_name,
            INVALIDATOR_SLOT_NUM,
            SlotPurpose::Invalidator,
            None,
            confirmed_lsn,
            &mut store,
            cancellation,
            Duration::from_secs(10),
            ReconnectPolicy::default(),
            None,
            sink,
            &health,
        ))
        .await;
        let slot_loss = match &result {
            Ok(()) => return result,
            Err(error) => invalidator_slot_loss(error).map(ToString::to_string),
        };
        let Some(error) = slot_loss else {
            return result;
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if started.elapsed() >= SLOT_RECOVERY_STABLE {
            retry = 0;
        }
        tracing::error!(
            purpose = "invalidator",
            slot = %slot_name,
            %error,
            "replication slot was invalidated or lost; recreating it and resyncing local sessions"
        );
        match recreate_invalidator_slot(control_pool, slot_name, runtime_realtime).await {
            Ok(lsn) => confirmed_lsn = lsn,
            Err(error) => tracing::warn!(
                slot = %slot_name,
                %error,
                "could not recreate the invalidator slot; retrying"
            ),
        }
        retry = retry.saturating_add(1);
        if !wait_for_slot_recovery(cancellation, slot_recovery_delay(retry - 1)).await {
            return Ok(());
        }
    }
}

async fn recreate_invalidator_slot(
    control_pool: &PostgresPool,
    slot_name: &str,
    runtime_realtime: &ProductionRuntimeRealtime,
) -> Result<Lsn, String> {
    let client = control_pool
        .acquire()
        .await
        .map_err(|error| error.to_string())?;
    let created = recreate_invalidated_slot(&**client, slot_name)
        .await
        .map_err(|error| error.to_string())?;
    drop(client);
    record_replication_slot_recreated(ReplicationSlotPurpose::Invalidator);
    if let Err(error) = runtime_realtime.force_full_resync().await {
        tracing::error!(
            error = %error.body().message,
            "could not force a full local-session resync after slot recovery"
        );
    }
    Ok(created.consistent_point)
}

#[allow(clippy::too_many_arguments)] // Recovery owns every handle the aggregation stream needs.
async fn run_sketch_aggregator_supervisor(
    connection_string: &str,
    process_id: &str,
    database: PostgresPool,
    snapshot_pool: PostgresPool,
    mut checkpoint: Lsn,
    policy: crate::SketchBatchPolicy,
    cancellation: &ReplicationCancellationToken,
) -> Result<(), InvalidationSupervisorError> {
    let mut retry = 0_u32;
    loop {
        let started = Instant::now();
        let result = crate::supervise_sketch_aggregation_stream_with_policy(
            connection_string,
            "aggregator",
            process_id,
            database.clone(),
            checkpoint,
            cancellation,
            Duration::from_secs(30),
            ReconnectPolicy::default(),
            policy,
        )
        .await;
        let slot_loss = match &result {
            Ok(()) => return Ok(()),
            Err(error) => sketch_slot_loss(error).map(ToString::to_string),
        };
        let Some(error) = slot_loss else {
            return result
                .map_err(|error| InvalidationSupervisorError::Aggregator(error.to_string()));
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if started.elapsed() >= SLOT_RECOVERY_STABLE {
            retry = 0;
        }
        tracing::error!(
            purpose = "aggregator",
            slot = "aggregator",
            %error,
            "aggregation slot was invalidated or lost; rebuilding its sketches"
        );
        match rebuild_sketch_aggregator(connection_string, &snapshot_pool, &database, process_id)
            .await
        {
            Ok(new_checkpoint) => checkpoint = new_checkpoint,
            Err(error) => tracing::warn!(
                %error,
                "could not rebuild the aggregation slot and sketches; retrying"
            ),
        }
        retry = retry.saturating_add(1);
        if !wait_for_slot_recovery(cancellation, slot_recovery_delay(retry - 1)).await {
            return Ok(());
        }
    }
}

async fn rebuild_sketch_aggregator(
    connection_string: &str,
    snapshot_pool: &PostgresPool,
    write_pool: &PostgresPool,
    process_id: &str,
) -> Result<Lsn, String> {
    let checkpoint =
        prepare_sketch_aggregator(connection_string, snapshot_pool, write_pool, process_id)
            .await
            .map_err(|error| error.to_string())?;
    record_replication_slot_recreated(ReplicationSlotPurpose::Aggregator);
    Ok(checkpoint)
}

const BYOP_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

async fn load_byop_configs(
    control_client: &tokio_postgres::Client,
    cipher: &OAuthSecretCipher,
    instant_database_url: &str,
    instant_on_instant_app_id: Option<Uuid>,
) -> Result<BTreeMap<Uuid, ProductionByopConfig>, String> {
    let rows = control_client
        .query(
            "SELECT id, connection_string FROM apps WHERE connection_string IS NOT NULL ORDER BY id",
            &[],
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut configs = BTreeMap::new();
    for row in rows {
        let app_id = row.get::<_, Uuid>("id");
        if Some(app_id) == instant_on_instant_app_id {
            continue;
        }
        let encrypted = row.get::<_, Vec<u8>>("connection_string");
        let connection_string = cipher
            .decrypt_bytes(app_id, &encrypted)
            .map_err(|error| error.to_string())?;
        let namespace = byop_namespace(&connection_string).map_err(|error| error.to_string())?;
        let connection_string =
            byop_driver_connection_string(&connection_string).map_err(|error| error.to_string())?;
        configs.insert(
            app_id,
            ProductionByopConfig {
                app_id,
                connection_string,
                namespace,
            },
        );
    }
    if let Some(app_id) = instant_on_instant_app_id {
        configs.insert(
            app_id,
            ProductionByopConfig {
                app_id,
                connection_string: instant_database_url.to_owned(),
                namespace: "public".to_owned(),
            },
        );
    }
    Ok(configs)
}

async fn load_byop_configs_from_pool(
    control_pool: &PostgresPool,
    cipher: &OAuthSecretCipher,
    instant_database_url: &str,
    instant_on_instant_app_id: Option<Uuid>,
) -> Result<BTreeMap<Uuid, ProductionByopConfig>, String> {
    let control_client = control_pool
        .acquire()
        .await
        .map_err(|error| error.to_string())?;
    load_byop_configs(
        &control_client,
        cipher,
        instant_database_url,
        instant_on_instant_app_id,
    )
    .await
}

async fn run_byop_reconciler(
    control_pool: PostgresPool,
    cipher: OAuthSecretCipher,
    runtime_realtime: Arc<ProductionRuntimeRealtime>,
    admin_realtime: Arc<ProductionAdminRealtime>,
    instant_database_url: String,
    instant_on_instant_app_id: Option<Uuid>,
    cancellation: ReplicationCancellationToken,
) -> Result<(), InvalidationSupervisorError> {
    let mut workers = BTreeMap::<Uuid, RunningByopInvalidator>::new();
    let mut timer = tokio::time::interval(BYOP_RECONCILE_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            _ = timer.tick() => {
                let finished = workers
                    .iter()
                    .filter_map(|(app_id, worker)| worker.handle.is_finished().then_some(*app_id))
                    .collect::<Vec<_>>();
                for app_id in finished {
                    if let Some(worker) = workers.remove(&app_id) {
                        match worker.handle.await {
                            Ok(Err(ProductionByopWorkerError::FatalSlotLoss(error))) => {
                                return Err(InvalidationSupervisorError::Runtime(
                                    InvalidationRuntimeError::Downstream(error),
                                ));
                            }
                            Ok(Err(ProductionByopWorkerError::Retryable(error))) => {
                                tracing::warn!(%app_id, %error, "retrying BYOP invalidator after worker failure");
                            }
                            Ok(Ok(())) | Err(_) => {}
                        }
                    }
                }
                let Ok(configs) = load_byop_configs_from_pool(
                    &control_pool,
                    &cipher,
                    &instant_database_url,
                    instant_on_instant_app_id,
                ).await else {
                    continue;
                };
                let current = workers
                    .iter()
                    .map(|(app_id, worker)| (*app_id, worker.config.clone()))
                    .collect::<BTreeMap<_, _>>();
                let reconciliation = reconcile_byop_configs(&current, &configs);
                for app_id in reconciliation.stop {
                    if let Some(worker) = workers.remove(&app_id) {
                        worker.cancellation.cancel();
                        let _worker_shutdown_is_best_effort = worker.handle.await;
                        cleanup_byop_slot(&worker.config).await;
                    }
                }
                for config in reconciliation.start {
                    let app_id = config.app_id;
                    let worker_cancellation = cancellation.child_token();
                    let task_cancellation = worker_cancellation.clone();
                    let worker_config = config.clone();
                    let worker_realtime = Arc::clone(&runtime_realtime);
                    let worker_admin_realtime = Arc::clone(&admin_realtime);
                    let handle = tokio::spawn(async move {
                        run_production_byop_invalidator(
                            worker_config,
                            worker_realtime,
                            worker_admin_realtime,
                            task_cancellation,
                        ).await
                    });
                    workers.insert(app_id, RunningByopInvalidator {
                        config,
                        cancellation: worker_cancellation,
                        handle,
                    });
                }
            }
        }
    }
    for (_, worker) in workers {
        worker.cancellation.cancel();
        let _worker_shutdown_is_best_effort = worker.handle.await;
    }
    Ok(())
}

async fn cleanup_byop_slot(config: &ProductionByopConfig) {
    let Ok(Ok((client, connection))) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_postgres::connect(
            &config.connection_string,
            MakeRustlsConnect::with_webpki_roots(),
        ),
    )
    .await
    else {
        return;
    };
    std::mem::drop(tokio::spawn(async move {
        let _connection_lifetime_is_owned_by_the_client = connection.await;
    }));
    let _slot_cleanup_is_best_effort =
        drop_logical_replication_slot(&client, &external_byop_slot_name(config.app_id)).await;
}

#[allow(clippy::too_many_lines)]
async fn run_production_byop_invalidator(
    config: ProductionByopConfig,
    runtime_realtime: Arc<ProductionRuntimeRealtime>,
    admin_realtime: Arc<ProductionAdminRealtime>,
    cancellation: ReplicationCancellationToken,
) -> Result<(), ProductionByopWorkerError> {
    let control_pool = PostgresPool::connect_named(&config.connection_string, 1, "byop_control")
        .await
        .map_err(|error| {
            format!(
                "could not create BYOP control pool for app {}: {error}",
                config.app_id
            )
        })?;
    'refresh: loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let control = control_pool.acquire().await.map_err(|error| {
            format!(
                "could not acquire BYOP control connection for app {}: {error}",
                config.app_id
            )
        })?;
        let schema = introspect_byop_schema(&**control, &config.namespace)
            .await
            .map_err(|error| error.to_string())?;
        if schema.tables.is_empty() {
            drop(control);
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(BYOP_RECONCILE_INTERVAL) => continue,
            }
        }
        let slot_name = external_byop_slot_name(config.app_id);
        let slot = ensure_logical_replication_slot(&**control, &slot_name)
            .await
            .map_err(|error| error.to_string())?;
        drop(control);
        let mut sink = RealtimeInvalidationSink::new(
            NoopInvalidationSink,
            (Arc::clone(&runtime_realtime), Arc::clone(&admin_realtime)),
        );
        let stream_cancellation = cancellation.child_token();
        let mut stream = Box::pin(supervise_byop_invalidation_stream(
            &config.connection_string,
            &slot_name,
            config.app_id,
            &config.namespace,
            schema.clone(),
            slot.confirmed_flush_lsn,
            &stream_cancellation,
            Duration::from_secs(SlotPurpose::Byop.status_interval_seconds()),
            ReconnectPolicy::default(),
            &mut sink,
        ));
        let mut schema_timer = tokio::time::interval(BYOP_RECONCILE_INTERVAL);
        schema_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        schema_timer.tick().await;
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    stream_cancellation.cancel();
                    let _stream_shutdown_is_best_effort = stream.await;
                    return Ok(());
                }
                result = &mut stream => {
                    return result.map_err(|error| classify_byop_worker_error(config.app_id, &error));
                },
                _ = schema_timer.tick() => {
                    let Ok(refreshed) = introspect_byop_schema_from_pool(
                        &control_pool,
                        &config.namespace,
                    ).await else {
                        continue;
                    };
                    if refreshed != schema {
                        stream_cancellation.cancel();
                        let _stream_shutdown_is_best_effort = stream.as_mut().await;
                        drop(stream);
                        sink.persist_byop(
                            config.app_id,
                            &ByopWalRecord {
                                tx_id: 0,
                                next_lsn: Lsn::default(),
                                changes: Vec::new(),
                            },
                            &[ByopTopic {
                                entities: ByopMatchSet::Any,
                                attributes: ByopMatchSet::Any,
                                values: ByopMatchSet::Any,
                            }],
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        continue 'refresh;
                    }
                }
            }
        }
    }
}

async fn introspect_byop_schema_from_pool(
    pool: &PostgresPool,
    namespace: &str,
) -> Result<ByopSchema, String> {
    let control = pool.acquire().await.map_err(|error| error.to_string())?;
    introspect_byop_schema(&**control, namespace)
        .await
        .map_err(|error| error.to_string())
}

fn external_byop_slot_name(app_id: Uuid) -> String {
    format!("instant_byop_{}", app_id.simple())
}

#[derive(Debug, Error)]
pub enum ProductionRouteAssemblyError {
    #[error(transparent)]
    Database(#[from] ProductionDatabaseBootstrapError),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error(transparent)]
    SystemCatalog(#[from] SystemCatalogVerificationError),
    #[error("could not prepare the WAL invalidation slot: {0}")]
    ReplicationSlot(#[from] instant_pg::ReplicationSlotError),
    #[error(transparent)]
    SketchPersistence(#[from] SketchPersistenceError),
    #[error("could not prepare the sketch replication slot: {0}")]
    SketchReplication(#[from] ReplicationTransportError),
    #[error("sketch bootstrap worker failed: {0}")]
    SketchBootstrapWorker(#[from] tokio::task::JoinError),
    #[error("sketch bootstrap cannot replace active slot `{0}`")]
    ActiveSketchSlot(String),
    #[error("sketch bootstrap lost its slot-creation race")]
    SketchSlotRace,
    #[error("S3 object store configuration failed: {0}")]
    ObjectStore(String),
    #[error(transparent)]
    Keyset(#[from] TinkKeysetLoadError),
    #[error(transparent)]
    Platform(#[from] ProductionPlatformError),
    #[error("dashboard identity policy startup failed: {0}")]
    DashboardIdentity(String),
    #[error("platform OAuth startup failed: {0}")]
    PlatformOAuth(String),
    #[error(transparent)]
    HttpResponsePolicy(#[from] crate::HttpResponsePolicyError),
    #[error("realtime cluster startup failed: {0}")]
    RealtimeCluster(String),
    #[error("realtime watermark startup failed: {0}")]
    RealtimeWatermark(String),
    #[error("nightly backup startup failed: {0}")]
    NightlyBackup(String),
}

async fn prepare_sketch_aggregator(
    database_url: &str,
    snapshot_pool: &PostgresPool,
    write_pool: &PostgresPool,
    process_id: &str,
) -> Result<Lsn, ProductionRouteAssemblyError> {
    const SLOT_NAME: &str = "aggregator";
    let mut snapshot_client = snapshot_pool.acquire().await?;
    let write_client = write_pool.acquire().await?;
    write_client
        .batch_execute(
            "SELECT pg_advisory_lock(hashtextextended('instantdb-attr-sketch-bootstrap', 0))",
        )
        .await
        .map_err(SketchPersistenceError::from)?;
    let result = prepare_sketch_aggregator_locked(
        database_url,
        &mut snapshot_client,
        &write_client,
        process_id,
        SLOT_NAME,
    )
    .await;
    let unlock = write_client
        .batch_execute(
            "SELECT pg_advisory_unlock(hashtextextended('instantdb-attr-sketch-bootstrap', 0))",
        )
        .await
        .map_err(SketchPersistenceError::from);
    match result {
        Ok(checkpoint) => {
            unlock?;
            Ok(checkpoint)
        }
        Err(error) => {
            let _unlock_failure_does_not_hide_bootstrap_error = unlock;
            Err(error)
        }
    }
}

async fn prepare_sketch_aggregator_locked(
    database_url: &str,
    snapshot_client: &mut tokio_postgres::Client,
    write_client: &tokio_postgres::Client,
    process_id: &str,
    slot_name: &str,
) -> Result<Lsn, ProductionRouteAssemblyError> {
    let checkpoint = sketch_aggregator_checkpoint(write_client, slot_name).await?;
    let lost = replication_slot_is_lost(write_client, slot_name).await?;
    let existing = get_logical_replication_slot(write_client, slot_name).await?;
    if !lost && let (Some(checkpoint), Some(_)) = (checkpoint, existing.as_ref()) {
        let slot = ensure_logical_replication_slot(write_client, slot_name).await?;
        validate_replication_start(&slot, checkpoint, SlotPurpose::Aggregator)?;
        return Ok(checkpoint);
    }

    if let Some(existing) = existing {
        if existing.active {
            return Err(ProductionRouteAssemblyError::ActiveSketchSlot(
                slot_name.to_owned(),
            ));
        }
        // A slot without its atomic status (or status without its slot) cannot
        // resume safely. Both sketches and slot are derived, so rebuild them
        // from a new exported snapshot while holding the fleet-wide lock.
        drop_logical_replication_slot(write_client, slot_name).await?;
    }
    write_client
        .execute(
            "DELETE FROM wal_aggregator_status WHERE slot_name = $1",
            &[&slot_name],
        )
        .await
        .map_err(SketchPersistenceError::from)?;
    write_client
        .execute("DELETE FROM attr_sketches", &[])
        .await
        .map_err(SketchPersistenceError::from)?;
    let connection_string = database_url.to_owned();
    let slot_name_owned = slot_name.to_owned();
    let snapshot_slot = tokio::task::spawn_blocking(move || {
        create_exported_snapshot_slot(&connection_string, &slot_name_owned)
    })
    .await??
    .ok_or(ProductionRouteAssemblyError::SketchSlotRace)?;
    bootstrap_attr_sketches(
        snapshot_client,
        write_client,
        &snapshot_slot,
        slot_name,
        process_id,
    )
    .await?;
    Ok(snapshot_slot.consistent_point)
}

/// Resolves the client address once for handlers that need it, trusting
/// forwarded headers only from configured proxy networks.
async fn resolve_client_ip(
    State(trusted_proxies): State<Arc<[IpNet]>>,
    mut request: Request,
    next: middleware::Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    let client_ip = instant_http::resolve_client_ip(peer, request.headers(), &trusted_proxies);
    request
        .extensions_mut()
        .insert(instant_http::ResolvedClientIp(client_ip));
    next.run(request).await
}

/// Builds one production route tree with an isolated realtime sync history.
/// The process failover loop uses
/// [`build_production_routes_with_realtime_history`] instead so replacements
/// retain the bounded catch-up window.
///
/// # Errors
///
/// Returns an error when database, secret, object-store, or policy setup fails.
pub async fn build_production_routes(
    config: &ServerConfig,
    database_url: &str,
    next_database_url: Option<&str>,
    clients: ProductionDatabaseClients,
    secrets: ProductionServerSecrets,
    machine_id: Uuid,
) -> Result<ProductionRouteAssembly, ProductionRouteAssemblyError> {
    Box::pin(build_production_routes_with_realtime_history(
        config,
        database_url,
        next_database_url,
        clients,
        secrets,
        machine_id,
        Arc::new(crate::RuntimeSyncHistory::default()),
    ))
    .await
}

/// Constructs all fifteen HTTP backend groups, durable worker hooks, shared
/// realtime state, and replication acknowledgement sink from validated process
/// configuration and bounded reconnecting database pools.
///
/// # Errors
///
/// Returns before binding a listener if any database pool, keyset, provider,
/// object store, or policy cannot be initialized.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::similar_names
)]
pub async fn build_production_routes_with_realtime_history(
    config: &ServerConfig,
    database_url: &str,
    next_database_url: Option<&str>,
    clients: ProductionDatabaseClients,
    secrets: ProductionServerSecrets,
    machine_id: Uuid,
    realtime_sync_history: Arc<crate::RuntimeSyncHistory>,
) -> Result<ProductionRouteAssembly, ProductionRouteAssemblyError> {
    let admin_data_pool =
        PostgresPool::connect_workload_partitioned(database_url, config.connection_pool_size())
            .await?;
    let response_policy_pool = admin_data_pool.control_pool();
    let rate_limit_pool = PostgresPool::connect_named(database_url, 3, "rate_limit").await?;
    let admin_request_rate_limit_pool =
        PostgresPool::connect_named(database_url, 1, "admin_request_rate_limit").await?;
    let runtime_auth_pool = PostgresPool::connect_named(database_url, 1, "runtime_auth").await?;
    let runtime_oauth_pool = PostgresPool::connect_named(database_url, 1, "runtime_oauth").await?;
    let runtime_stream_pool =
        PostgresPool::connect_named(database_url, 1, "runtime_stream").await?;
    let superadmin_store_pool =
        PostgresPool::connect_named(database_url, 1, "superadmin_store").await?;
    let dashboard_control_pool =
        PostgresPool::connect_named(database_url, 1, "dashboard_control").await?;
    let dashboard_template_pool =
        PostgresPool::connect_named(database_url, 1, "dashboard_template").await?;
    let dashboard_core_pool =
        PostgresPool::connect_named(database_url, 1, "dashboard_core").await?;
    let dashboard_metrics_pool =
        PostgresPool::connect_named(database_url, 1, "dashboard_metrics").await?;
    let dashboard_policy_pool =
        PostgresPool::connect_named(database_url, 1, "dashboard_policy").await?;
    let invalidation_queue_policy_pool =
        PostgresPool::connect_named(database_url, 1, "invalidation_queue_policy").await?;
    let rate_limit_sweeper_pool =
        PostgresPool::connect_named(database_url, 1, "rate_limit_sweeper").await?;
    let ephemeral_sweeper_pool =
        PostgresPool::connect_named(database_url, 1, "ephemeral_sweeper").await?;
    let triples_size_collector_pool =
        PostgresPool::connect_named(database_url, 1, "triples_size_collector").await?;
    let failover_gate_pool = PostgresPool::connect_named(database_url, 1, "failover_gate").await?;
    let partition_retention_pool =
        PostgresPool::connect_named(database_url, 1, "partition_retention").await?;
    let webhook_worker_pool =
        PostgresPool::connect_named(database_url, 1, "webhook_worker").await?;
    let replication_slot_cleanup_pool =
        PostgresPool::connect_named(database_url, 1, "replication_slot_cleanup").await?;
    let storage_sweeper_pool =
        PostgresPool::connect_named(database_url, 1, "storage_sweeper").await?;
    let indexing_worker_pool =
        PostgresPool::connect_named(database_url, 1, "indexing_worker").await?;
    let backup_control_pool =
        PostgresPool::connect_named(database_url, 1, "backup_control").await?;
    let backup_snapshot_pool =
        PostgresPool::connect_named(database_url, 1, "backup_snapshot").await?;
    let restore_worker_pool =
        PostgresPool::connect_named(database_url, 1, "restore_worker").await?;
    let custodian_pool = PostgresPool::connect_named(database_url, 1, "custodian").await?;
    let realtime_room_writer_pool =
        PostgresPool::connect_named(database_url, 1, "realtime_room_writer").await?;
    // WAL history/webhook commits and cluster WAL publication must not queue
    // behind presence and broadcast writes on the room-writer connection.
    let wal_durable_sink_pool =
        PostgresPool::connect_named(database_url, 1, "wal_durable_sink").await?;
    let realtime_acknowledgement_pool =
        PostgresPool::connect_named(database_url, 1, "realtime_acknowledgement").await?;
    let realtime_room_events_pool =
        PostgresPool::connect_named(database_url, 1, "realtime_room_events").await?;
    let invalidation_control_pool =
        PostgresPool::connect_named(database_url, 1, "invalidation_control").await?;
    let sketch_aggregator_snapshot_pool =
        PostgresPool::connect_named(database_url, 1, "sketch_aggregator_snapshot").await?;
    let sketch_aggregator_write_pool =
        PostgresPool::connect_named(database_url, 1, "sketch_aggregator_write").await?;
    let nightly_backup_control_pool = if config
        .nightly_app_backups()
        .unwrap_or(config.environment() == ServerEnvironment::Production)
    {
        Some(PostgresPool::connect_named(database_url, 1, "nightly_backup_control").await?)
    } else {
        None
    };
    let realtime_replica_pool = match next_database_url {
        Some(next_database_url) if next_database_url != database_url => {
            PostgresPool::connect_named(
                next_database_url,
                config.connection_pool_size(),
                "realtime_replica",
            )
            .await?
        }
        _ => admin_data_pool.clone(),
    };

    // Migration and catalog verification plus the initial realtime
    // watermark are bounded startup work. Use a reconnecting pool lease rather
    // than keeping a dedicated fatal connection alive for the process lifetime.
    let max_tx_id: i64 = {
        let mut system_catalog_client = admin_data_pool.acquire().await?;
        verify_database_migrations(&mut system_catalog_client).await?;
        verify_system_catalog(&mut system_catalog_client).await?;
        system_catalog_client
            .query_one("SELECT COALESCE(MAX(id), 0)::bigint FROM transactions", &[])
            .await
            .map_err(|error| ProductionRouteAssemblyError::RealtimeWatermark(error.to_string()))?
            .get(0)
    };
    let max_tx_id = u64::try_from(max_tx_id).map_err(|_| {
        ProductionRouteAssemblyError::RealtimeWatermark(
            "transactions maximum cannot be negative".to_owned(),
        )
    })?;
    // Match the JVM startup contract: the transaction maximum comes from the
    // database, while the process ISN begins at zero and advances on commits.
    let realtime_watermarks = Arc::new(RealtimeWatermarkSeed::initialized(
        max_tx_id,
        Isn::default(),
    ));
    clients.finish()?;
    let http_response_policy = PostgresHttpResponsePolicy::load_pool(response_policy_pool).await?;
    let failover_gate = Arc::new(PostgresFailoverGate::from_pool(failover_gate_pool));
    let partition_retention = Arc::new(PostgresPartitionRetention::from_pool(
        partition_retention_pool,
    ));

    let transaction_pool_pressure = PostgresTransactionPoolPressurePolicy::from_pools(
        admin_data_pool.clone(),
        rate_limit_pool.clone(),
        config.superuser_email().map(str::to_owned),
    )
    .with_batched_reads();
    let rate_limit_consumer: Arc<dyn RateLimitConsumer> = Arc::new(
        PostgresRateLimitConsumer::from_pool(rate_limit_pool, secrets.rate_limit_hmac_key),
    );
    let admin_request_rate_limits: Arc<dyn crate::AdminRequestRateLimitPolicy> =
        Arc::new(crate::PostgresAdminRequestRateLimitPolicy::from_pool(
            admin_request_rate_limit_pool,
            Arc::clone(&rate_limit_consumer),
        ));
    let magic_code_rate_limiter: Arc<dyn crate::MagicCodeRateLimiter> = Arc::new(
        crate::PostgresMagicCodeRateLimiter::from_pool(
            admin_data_pool.clone(),
            Arc::clone(&rate_limit_consumer),
        )
        .with_email_provider(config.email().provider()),
    );
    let magic_code_expiry = Arc::new(PostgresMagicCodeExpiryPolicy::from_pool(
        admin_data_pool.clone(),
    ));

    let object_store = s3_object_store_from_config(config, secrets.s3_presigning.clone())
        .await
        .map_err(ProductionRouteAssemblyError::ObjectStore)?;
    let objects = Arc::new(object_store);
    let backup_bucket = config.backup_storage().bucket.as_deref().ok_or_else(|| {
        ProductionRouteAssemblyError::ObjectStore(
            "S3_APP_BACKUPS_BUCKET is required for production routes".to_owned(),
        )
    })?;
    let backup_store =
        s3_object_store_for_bucket_from_config(config, backup_bucket, secrets.s3_presigning)
            .await
            .map_err(ProductionRouteAssemblyError::ObjectStore)?;
    let backup_objects = Arc::new(backup_store);
    let keysets = AwsKmsTinkKeysetLoader::new();
    let oauth_cipher = keysets.load_oauth_cipher(&secrets.oauth_keyset).await?;
    let webhook_keys = Arc::new(
        keysets
            .load_webhook_keys(config.server_origin(), &secrets.webhook_keyset)
            .await?,
    );

    let RuntimePostgresPolicies {
        app_rate_limits,
        refresh_throttle,
        coarse_topics,
        datalog_cache,
        effective_app_status_policy,
        receive_timeout,
        statement_cancel_wait,
        sse_retry_interval,
        query_preparation,
        query_controls,
        refresh_read_controls,
        control_plane,
    } = RuntimePostgresPolicies::from_primary_pool(
        &admin_data_pool,
        config.superuser_email().map(str::to_owned),
    );
    let authorizer = Arc::new(
        PostgresAdminDataAuthorizer::from_pool(admin_data_pool.clone())
            .with_fresh_catalog_reads(admin_data_pool.control_pool())
            .with_control_plane_cache(Arc::clone(&control_plane)),
    );
    let transaction_pool_pressure =
        Arc::new(transaction_pool_pressure.with_control_plane_cache(Arc::clone(&control_plane)));
    let root_frame_caches = Arc::new(crate::RootFrameCaches::new(
        Arc::clone(&datalog_cache) as Arc<dyn crate::DatalogCachePolicy>
    ));
    let admin_data = Arc::new(
        AdminDataService::from_pool(admin_data_pool.clone(), Arc::clone(&authorizer))
            .with_control_plane_cache(Arc::clone(&control_plane))
            .with_fresh_refresh_routing_reads()
            .with_native_realtime_init_catalog_handoff()
            .with_native_sdk_typed_protocol()
            .with_native_structural_query_plans()
            .with_root_frame_caches(Arc::clone(&root_frame_caches))
            .with_transaction_authorization_pool(admin_data_pool.control_pool())
            .with_app_status_policy(Arc::clone(&effective_app_status_policy)
                as Arc<dyn crate::EffectiveAppStatusPolicy>)
            .with_realtime_read_replica_pool(
                realtime_replica_pool,
                Arc::clone(&failover_gate) as Arc<dyn crate::ReplicaReadPolicy>,
            )
            .with_query_preparation_policy(
                Arc::clone(&query_preparation) as Arc<dyn crate::QueryPreparationPolicy>
            )
            .with_admin_transaction_queue_policy(Arc::new(
                PostgresAdminTransactionQueuePolicy::from_pool(admin_data_pool.clone()),
            ))
            .with_transaction_pool_pressure_policy(transaction_pool_pressure)
            .with_byop_cipher(Arc::new(oauth_cipher.clone()))
            .with_file_url_signer(Arc::new(StorageFileUrlSigner::new(Arc::clone(&objects))))
            .with_transaction_options(TransactionOptions {
                admin: true,
                invalidator_slot_num: INVALIDATOR_SLOT_NUM,
                wal_entity_log: WalEntityLog::LogicalMessage,
                ..TransactionOptions::default()
            })
            .with_rate_limit_consumer(Arc::clone(&rate_limit_consumer))
            .with_admin_request_rate_limit_policy(admin_request_rate_limits),
    );
    let runtime_stream = Arc::new(
        PostgresRuntimeStreamBackend::from_pool(
            runtime_stream_pool,
            Arc::clone(&authorizer),
            Arc::clone(&objects),
        )
        .with_dynamic_flush_limit(),
    );
    let realtime_cluster: Arc<dyn crate::RuntimeRealtimeCluster> = Arc::new(
        PostgresRealtimeCluster::from_pools_with_listener(
            machine_id,
            realtime_room_writer_pool.clone(),
            realtime_room_events_pool,
            database_url,
        )
        .await
        .map_err(ProductionRouteAssemblyError::RealtimeCluster)?
        .with_acknowledgement_pool(realtime_acknowledgement_pool),
    );
    let admission_limits = {
        let mut limits = crate::RuntimeAdmissionLimits::from_env();
        match admin_data_pool.control_pool().acquire().await {
            Ok(client) => {
                for flag in [
                    "max-live-sessions-per-ip",
                    "max-sessions-per-app",
                    "max-subscriptions-per-session",
                    "max-instaql-query-bytes",
                ] {
                    match crate::get_config_flag(&client, flag).await {
                        Ok(Some(value)) => {
                            limits.apply_config_flag(flag, &value);
                        }
                        Ok(None) => {}
                        Err(error) => tracing::warn!(
                            %flag,
                            %error,
                            "could not read runtime admission flag; keeping the environment default"
                        ),
                    }
                }
                // Operator-chosen app labels for the private metrics listener.
                // Read once per route assembly; a slug change applies on restart.
                match crate::get_config_flag(&client, "app-metric-slugs").await {
                    Ok(Some(value)) => crate::metrics::set_app_metric_slugs(&value),
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        %error,
                        "could not read app-metric-slugs; per-app labels keep app id prefixes"
                    ),
                }
            }
            Err(error) => tracing::warn!(
                %error,
                "could not read runtime admission flags; keeping the environment defaults"
            ),
        }
        limits
    };
    let runtime_realtime = Arc::new(
        RuntimeRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&admin_data), machine_id)
            .with_admission_limits(admission_limits)
            .with_refresh_admission(
                std::num::NonZeroUsize::new(
                    admin_data_pool
                        .for_workload(crate::postgres_pool::DatabaseWorkload::Refresh)
                        .max_size()
                        .saturating_mul(2)
                        .max(1),
                ).unwrap_or(std::num::NonZeroUsize::MIN),
                crate::runtime_realtime::DEFAULT_REFRESH_WAITERS,
            )
            .with_effective_app_status_policy(
                Arc::clone(&effective_app_status_policy)
                    as Arc<dyn crate::EffectiveAppStatusPolicy>,
            )
            .with_watermark_seed(Arc::clone(&realtime_watermarks))
            .with_sync_history(Arc::clone(&realtime_sync_history))
            .with_stream_backend(Arc::clone(&runtime_stream) as Arc<dyn crate::RuntimeStreamBackend>)
            .with_app_rate_limit_policy(Arc::clone(&app_rate_limits) as Arc<dyn AppRateLimitPolicy>)
            .with_refresh_throttle_policy(
                Arc::clone(&refresh_throttle) as Arc<dyn RefreshThrottlePolicy>
            )
            .with_coarse_topics_policy(coarse_topics)
            .with_refresh_read_controls_policy(refresh_read_controls)
            .with_datalog_cache_policy(datalog_cache)
            .with_root_frame_caches(root_frame_caches)
            .with_receive_timeout_policy(Arc::clone(&receive_timeout) as Arc<dyn crate::ReceiveTimeoutPolicy>)
            .with_statement_cancel_wait_policy(statement_cancel_wait)
            .with_receive_handler_timeout_policy(receive_timeout)
            .with_combine_transacts_policy(
                Arc::clone(&failover_gate) as Arc<dyn CombineTransactsPolicy>
            )
            .with_join_room_logger(Arc::new(PostgresJoinRoomLogger::from_pool(
                admin_data_pool.clone(),
            )))
            .with_sse_retry_interval_policy(sse_retry_interval)
            .with_cluster(Arc::clone(&realtime_cluster)),
    );
    let admin_presence = Arc::new(AdminPresenceService::from_pool(
        admin_data_pool.clone(),
        Arc::clone(&authorizer),
        runtime_realtime.room_store(),
    ));
    let admin_metadata = Arc::new(AdminMetadataService::from_pool(
        admin_data_pool.clone(),
        Arc::clone(&authorizer),
    ));
    let storage_policy = Arc::new(PostgresStorageAccessPolicy::from_pool(
        admin_data_pool.clone(),
        config.environment(),
    ));
    let storage = Arc::new(
        AdminStorageService::from_pool(
            admin_data_pool.clone(),
            Arc::clone(&authorizer),
            Arc::clone(&objects),
            config.server_origin(),
        )
        .with_storage_policy(storage_policy)
        .with_max_upload_bytes(config.storage().max_upload_bytes)
        .with_effective_app_status_policy(
            Arc::clone(&effective_app_status_policy) as Arc<dyn crate::EffectiveAppStatusPolicy>
        )
        .with_rate_limit_consumer(Arc::clone(&rate_limit_consumer)),
    );
    let admin_realtime = Arc::new(
        AdminRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&admin_data), machine_id)
            .with_effective_app_status_policy(Arc::clone(&effective_app_status_policy)
                as Arc<dyn crate::EffectiveAppStatusPolicy>)
            .with_watermark_seed(realtime_watermarks)
            .with_sync_history(realtime_sync_history)
            .with_stream_backend(runtime_stream)
            .with_cluster(&realtime_cluster),
    );

    let indexing_worker = Arc::new(PostgresIndexingWorker::from_pool(
        indexing_worker_pool,
        machine_id.to_string(),
    ));
    let backup_worker = Arc::new(PostgresBackupWorker::from_pools(
        backup_control_pool,
        backup_snapshot_pool,
        Arc::clone(&backup_objects),
        machine_id.to_string(),
    ));
    let backup_pool = PostgresBackupWorkerPool::new(
        Arc::clone(&backup_worker),
        Arc::clone(&backup_objects),
        database_url,
        machine_id.to_string(),
    );
    let restore_creator = if config.environment() == ServerEnvironment::Development {
        "ephemeral-apps-dev@backendrelay.invalid"
    } else {
        "ephemeral-apps@backendrelay.invalid"
    };
    let restore_worker = Arc::new(PostgresRestoreWorker::from_pool(
        restore_worker_pool,
        Arc::clone(&backup_objects),
        restore_creator,
    ));
    let transport = Arc::new(
        GuardedWebhookHttpTransport::new(Arc::clone(&webhook_keys))
            .with_ip_whitelist(Arc::clone(&failover_gate) as Arc<dyn crate::WebhookIpWhitelist>),
    );
    let webhook_worker = Arc::new(PostgresWebhookWorker::from_pool(
        webhook_worker_pool,
        Arc::clone(&transport),
        Arc::clone(&webhook_keys),
        config.server_origin(),
        machine_id,
    ));
    let storage_hooks = Arc::new(DashboardStorageControlHooks::new(
        ProductionDashboardTerminalHooks,
        Arc::clone(&storage),
    ));
    let webhook_hooks = Arc::new(WebhookDashboardControlHooks::new(
        storage_hooks,
        Arc::clone(&transport),
        Arc::clone(&webhook_worker),
    ));
    let backup_object_hooks = Arc::new(BackupObjectControlHooks::with_file_objects(
        webhook_hooks,
        Arc::clone(&backup_objects),
        Arc::clone(&objects),
    ));
    let backup_streamer: Arc<dyn crate::DashboardBackupStreamer> = backup_object_hooks.clone();
    let indexing_hooks = Arc::new(PostgresIndexingControlHooks::new(
        backup_object_hooks,
        Arc::clone(&indexing_worker),
    ));
    let backup_hooks = Arc::new(PostgresBackupControlHooks::new(
        indexing_hooks,
        Arc::clone(&backup_pool),
    ));
    let backup_restore_hooks = Arc::new(PostgresBackupRestoreControlHooks::new(
        backup_hooks,
        Arc::clone(&backup_objects),
    ));
    let restore_hooks = Arc::new(PostgresRestoreControlHooks::new(
        backup_restore_hooks,
        Arc::clone(&restore_worker),
    ));
    let runtime_session_reports = Arc::new(PostgresRuntimeSessionReportCollector::from_pool(
        admin_data_pool.clone(),
    ));
    let database_query_throughput: Option<Arc<dyn crate::DatabaseQueryThroughput>> = None;
    let dashboard_public_config = PostgresDashboardPolicyHooks::from_pool(
        dashboard_policy_pool.clone(),
        config.environment(),
        NoopDashboardControlHooks,
    )
    .with_dashboard_allowed_emails(config.dashboard_allowed_emails().cloned())
    .with_dashboard_signup_open(config.dashboard_signup_open())
    .with_ephemeral_apps_enabled(config.ephemeral_apps_enabled());
    let dashboard_config = DashboardConfigService::from_config(
        dashboard_public_config,
        config,
        secrets.dashboard_private,
    );
    let email_adapters = production_dashboard_adapters_with_runtime_pools(
        config,
        dashboard_template_pool,
        dashboard_core_pool,
        dashboard_metrics_pool,
        dashboard_policy_pool,
        oauth_cipher.clone(),
        Vec::new(),
        Some((
            Arc::clone(&runtime_realtime) as Arc<dyn crate::RuntimeSessionReporter>,
            Arc::clone(&runtime_session_reports) as Arc<dyn crate::RuntimeSessionReportCollector>,
        )),
        database_query_throughput,
        restore_hooks,
    );
    let dashboard_hooks = Arc::new(DashboardRateLimitHooks::new(
        email_adapters.dashboard_hooks,
        Arc::clone(&rate_limit_consumer),
        Arc::clone(&magic_code_rate_limiter),
    ));
    let identity_policy = PostgresDashboardIdentityPolicy::load_pool(admin_data_pool.clone())
        .await
        .map_err(|error| {
            ProductionRouteAssemblyError::DashboardIdentity(error.body().message.clone())
        })?
        .with_allowed_emails(config.dashboard_allowed_emails().cloned())
        .with_superuser_email(config.superuser_email().map(str::to_owned))
        .with_signup_open(config.dashboard_signup_open());
    let dashboard_summary = PostgresDashboardSummaryProvider::from_pool(
        admin_data_pool.clone(),
        Arc::clone(&dashboard_hooks),
    )
    .with_superuser_email(config.superuser_email().map(str::to_owned));
    let dashboard_identity = DashboardIdentityService::from_pool(
        admin_data_pool.clone(),
        email_adapters.dashboard_magic_code,
        identity_policy.clone(),
        dashboard_summary,
    );
    let webhook_limit = Arc::new(PostgresWebhookLimitPolicy::from_pool(
        admin_data_pool.clone(),
    ));
    let dashboard_control = DashboardControlService::from_pool(
        dashboard_control_pool.clone(),
        Arc::clone(&dashboard_hooks),
    )
    .with_backup_streamer(backup_streamer)
    .with_webhook_limit_policy(webhook_limit)
    .with_dashboard_email_policy(
        config.dashboard_allowed_emails().cloned(),
        config.superuser_email().map(str::to_owned),
    );
    let dashboard = Arc::new(crate::dashboard::TrustedProxyDashboard::new(
        DashboardService::new(
            dashboard_config,
            DashboardService::new(
                dashboard_identity,
                DashboardService::new(dashboard_control, RejectUnmatchedDashboard),
            ),
        ),
        config.trusted_proxy_cidrs(),
    ));

    let runtime_auth = Arc::new(production_runtime_auth_service_from_pool(
        runtime_auth_pool,
        admin_data_pool.clone(),
        Arc::clone(&email_adapters.magic_code),
        RuntimeAuthConfig::default(),
        Arc::clone(&rate_limit_consumer),
        Arc::clone(&magic_code_rate_limiter),
        Arc::clone(&magic_code_expiry) as Arc<dyn crate::MagicCodeExpiryPolicy>,
    ));
    let admin_auth = Arc::new(
        AdminAuthService::from_pool(
            admin_data_pool.clone(),
            Arc::clone(&authorizer),
            Arc::clone(&email_adapters.magic_code),
            RuntimeAuthConfig::default(),
        )
        .with_magic_code_expiry_policy(magic_code_expiry)
        .with_magic_code_rate_limiter(magic_code_rate_limiter),
    );
    let runtime_oauth = production_runtime_oauth_adapters_from_pool(
        config,
        runtime_oauth_pool,
        admin_data_pool.clone(),
        oauth_cipher.clone(),
        rate_limit_consumer,
    );
    let runtime_oauth_backend: Arc<ProductionRuntimeOAuthService> = runtime_oauth.backend;

    let platform = Arc::new(production_platform_service_from_config_with_pools(
        config,
        admin_data_pool.clone(),
        admin_data_pool.clone(),
        &objects,
        &webhook_keys,
    )?);
    let platform_oauth = Arc::new(
        PlatformOAuthService::from_config(
            PostgresPlatformOAuthStore::from_pool(admin_data_pool.clone()),
            config,
        )
        .map_err(ProductionRouteAssemblyError::PlatformOAuth)?,
    );
    let superadmin_debug = Arc::new(crate::SuperadminDebugService::new(
        Arc::clone(&admin_data),
        Arc::clone(&runtime_realtime),
    ));
    let superadmin = Arc::new(
        crate::postgres_superadmin_service_from_pools(
            admin_data_pool.clone(),
            superadmin_store_pool,
            Arc::clone(&dashboard_hooks),
        )
        .with_debug(superadmin_debug),
    );

    let process_id = machine_id.simple().to_string();
    // Readiness is a fresh critical control read, not an application data query.
    // Reuse the existing control reservation so a subscription ramp cannot hide
    // a healthy WAL stream behind the saturated interactive FIFO. No new pool.
    let shared_wal_health =
        PostgresWalHealth::from_pool(admin_data_pool.control_pool(), process_id.clone());
    let health_observer = PostgresInvalidationHealthObserver::spawn(shared_wal_health.clone());
    let health = HealthState::new(true)
        .with_version(ServerVersion::new(
            crate::version::VERSION,
            crate::version::GIT_SHA,
            crate::version::migration_version(),
        ))
        .with_shared_wal_health(Arc::new(shared_wal_health));
    let router = app_router_with_all_backends(
        health.clone(),
        AllBackends {
            server_origin: config.server_origin().to_owned(),
            dashboard_gateway: secrets.dashboard_gateway,
            runtime_auth,
            runtime_oauth: runtime_oauth_backend,
            runtime_realtime: Arc::clone(&runtime_realtime),
            admin_data,
            admin_auth,
            admin_metadata,
            admin_presence,
            admin_storage: Arc::clone(&storage),
            admin_realtime: Arc::clone(&admin_realtime),
            runtime_storage: storage,
            platform,
            platform_oauth,
            superadmin,
            dashboard,
        },
    )
    .layer(middleware::from_fn_with_state(
        Arc::clone(&app_rate_limits),
        app_rate_limit_middleware::<PostgresAppRateLimitPolicy>,
    ));
    let router = apply_http_response_policy(
        router,
        config.dashboard_origin(),
        http_response_policy.snapshot_handle(),
    );
    let trusted_proxies: Arc<[IpNet]> = Arc::from(config.trusted_proxy_cidrs());
    let router = router
        .layer(middleware::from_fn_with_state(
            trusted_proxies,
            resolve_client_ip,
        ))
        .layer(middleware::from_fn(crate::trace_http_request))
        // Outermost: a handler panic must still produce the standard JSON
        // error contract instead of a dropped connection. The installed panic
        // hook already increments `backendrelay_panics_total`.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ));

    let (notifier, mut webhook_wakeups) = WebhookEventWakeupNotifier::channel();
    let webhook_worker_weak = Arc::downgrade(&webhook_worker);
    std::mem::drop(tokio::spawn(async move {
        while webhook_wakeups.recv().await.is_some() {
            while webhook_wakeups.try_recv().is_ok() {}
            let Some(worker) = webhook_worker_weak.upgrade() else {
                break;
            };
            let _durable_queue_recovers_failures = worker.drain_pending().await;
        }
    }));
    let notifier = Arc::new(notifier);
    let durable = crate::PostgresDurableInvalidationSink::from_cluster_writer(
        wal_durable_sink_pool,
        notifier,
        machine_id,
    );
    let invalidation_queue = QueuedRealtimeInvalidationConsumer::new(
        (Arc::clone(&runtime_realtime), Arc::clone(&admin_realtime)),
        Arc::new(PostgresInvalidationQueuePolicy::from_pool(
            invalidation_queue_policy_pool,
        )),
    );
    // Invalidate planner and control-plane snapshots before the delivery queue
    // can delay or drop app refreshes, and before runtime security fences
    // re-run queries. Keep the consumer's batch/cluster staging contract intact.
    let invalidation_sink = RealtimeInvalidationSink::new(
        durable,
        ((query_controls, control_plane), invalidation_queue),
    );
    let sketch_aggregator_checkpoint = prepare_sketch_aggregator(
        database_url,
        &sketch_aggregator_snapshot_pool,
        &sketch_aggregator_write_pool,
        &process_id,
    )
    .await?;
    let slot_cleaner = Arc::new(PostgresReplicationSlotCleaner::from_pool(
        replication_slot_cleanup_pool,
    ));
    // A process that died without shutting down leaves an inactive slot behind.
    // Its machine heartbeat is the only fleet-wide evidence that it is gone.
    match slot_cleaner.run_stale_machine_cleanup().await {
        Ok(outcome) if !outcome.cleaned.is_empty() => tracing::info!(
            cleaned = ?outcome.cleaned,
            "dropped invalidator slots left by machines with stale heartbeats"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            "could not clean up invalidator slots from stale machines at startup"
        ),
    }
    // Validate/bootstrap the shared aggregator before allocating a per-process
    // slot. A failed aggregator start must not consume another persistent slot
    // on each supervisor retry, eventually exhausting max_replication_slots.
    let slot_name = format!("invalidator_{}", machine_id.simple());
    let invalidation_control = invalidation_control_pool.acquire().await?;
    if replication_slot_is_lost(&**invalidation_control, &slot_name).await? {
        tracing::error!(
            purpose = "invalidator",
            slot = %slot_name,
            "replication slot was invalidated while this machine was down; recreating it"
        );
        drop_logical_replication_slot(&**invalidation_control, &slot_name).await?;
        record_replication_slot_recreated(ReplicationSlotPurpose::Invalidator);
    }
    let slot = ensure_logical_replication_slot(&**invalidation_control, &slot_name).await?;
    drop(invalidation_control);
    let invalidator = ProductionInvalidator {
        connection_string: database_url.to_owned(),
        slot_name,
        confirmed_lsn: slot.confirmed_flush_lsn,
        sink: invalidation_sink,
        byop_control_pool: invalidation_control_pool,
        byop_cipher: oauth_cipher,
        runtime_realtime: Arc::clone(&runtime_realtime),
        admin_realtime: Arc::clone(&admin_realtime),
        instant_on_instant_app_id: config.instant_on_instant_app_id(),
        sketch_aggregator_pool: sketch_aggregator_write_pool,
        sketch_aggregator_snapshot_pool,
        sketch_aggregator_checkpoint,
        sketch_batch_policy: config.sketch_batch_policy(),
        process_id,
        health_observer,
    };
    let nightly_backup = if let Some(nightly_backup_control_pool) = nightly_backup_control_pool {
        let source_cluster_id = "direct-postgres";
        let snapshot_provider = ProductionSnapshotProvider::DirectPostgres(
            DirectPostgresSnapshotProvider::new(database_url),
        );
        Some(NightlyBackupSupervisor::new(
            PostgresNightlyBackupWorker::new(
                nightly_backup_control_pool,
                Arc::clone(&backup_objects),
                Arc::new(snapshot_provider),
                Arc::clone(&failover_gate),
                source_cluster_id,
                machine_id,
                "ephemeral-apps@backendrelay.invalid",
            ),
        ))
    } else {
        None
    };
    let custodian = Arc::new(
        PostgresCustodian::from_pool(custodian_pool, machine_id.to_string())
            .with_environment(config.environment())
            .with_failover_gate(Arc::clone(&failover_gate)),
    );
    let storage_sweeper = Arc::new(PostgresStorageSweeper::from_pool(
        storage_sweeper_pool,
        Arc::clone(&objects),
        machine_id.to_string(),
    ));
    let triples_size_collector = Arc::new(PostgresTriplesSizeCollector::from_pool(
        triples_size_collector_pool,
    ));
    let rate_limit_sweeper = Arc::new(PostgresRateLimitSweeper::from_pool(rate_limit_sweeper_pool));
    let ephemeral_sweeper = Arc::new(PostgresEphemeralAppSweeper::from_pool(
        ephemeral_sweeper_pool,
    ));
    let mut tasks = runtime_oauth.tasks;
    let gauges = Arc::new(crate::NativeGaugeRegistry::default());
    tasks.push(gauges.scheduled_task(config.environment()));
    tasks.push(http_response_policy.refresh_task());
    tasks.push(identity_policy.refresh_task());
    let daily_metrics = Arc::new(PostgresDailyMetricsWorker::from_pool(
        admin_data_pool,
        Arc::clone(&failover_gate) as Arc<dyn crate::DailyMetricsFailoverGate>,
    ));
    tasks.push(daily_metrics.scheduled_task());
    if let Some(verifier) = email_adapters.email_domain_verifier.as_ref() {
        tasks.push(
            Arc::new(PostgresSenderDomainRecheck::from_pool(
                dashboard_control_pool,
                Arc::clone(verifier),
            ))
            .scheduled_task(),
        );
    }
    tasks.extend(worker_tasks(
        config.environment(),
        indexing_worker,
        Arc::clone(&backup_pool),
        restore_worker,
        &slot_cleaner,
        webhook_worker,
        &failover_gate,
        partition_retention,
        nightly_backup.as_ref(),
        &custodian,
        triples_size_collector,
        storage_sweeper,
        rate_limit_sweeper,
        ephemeral_sweeper,
    ));

    Ok(ProductionRouteAssembly {
        router,
        health,
        tasks,
        runtime_realtime,
        admin_realtime,
        invalidator,
        backup_pool,
        nightly_backup,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn worker_tasks<S>(
    environment: ServerEnvironment,
    indexing: Arc<PostgresIndexingWorker>,
    backup: Arc<PostgresBackupWorkerPool<S>>,
    restore: Arc<PostgresRestoreWorker<S>>,
    slot_cleaner: &Arc<PostgresReplicationSlotCleaner>,
    webhooks: Arc<PostgresWebhookWorker<GuardedWebhookHttpTransport, Ed25519WebhookKeys>>,
    failover: &Arc<PostgresFailoverGate>,
    partition_retention: Arc<PostgresPartitionRetention>,
    nightly_backup: Option<&Arc<ProductionNightlyBackup>>,
    custodian: &Arc<PostgresCustodian>,
    triples_size: Arc<PostgresTriplesSizeCollector>,
    storage: Arc<PostgresStorageSweeper<S>>,
    rate_limits: Arc<PostgresRateLimitSweeper>,
    ephemeral: Arc<PostgresEphemeralAppSweeper>,
) -> Vec<ScheduledTask>
where
    S: crate::StorageObjectStore,
{
    let indexing_schedule = if environment == ServerEnvironment::Production {
        JvmTaskSchedules::INDEXING_RECOVERY_PRODUCTION
    } else {
        JvmTaskSchedules::INDEXING_RECOVERY_NON_PRODUCTION
    };
    let webhook_schedule = if environment == ServerEnvironment::Production {
        JvmTaskSchedules::WEBHOOK_KICK_PRODUCTION
    } else {
        JvmTaskSchedules::WEBHOOK_KICK_DEVELOPMENT
    };
    let mut tasks = Vec::new();
    if let Some(nightly_backup) = nightly_backup {
        let nightly_backup = Arc::clone(nightly_backup);
        tasks.push(ScheduledTask::new(
            "daily-backup",
            JvmTaskSchedules::DAILY_BACKUP,
            Duration::from_secs(30),
            move || {
                let worker = Arc::clone(&nightly_backup);
                async move {
                    let date = chrono::Utc::now()
                        .with_timezone(&chrono_tz::US::Pacific)
                        .date_naive();
                    worker.kick(date).await
                }
            },
        ));
    }
    tasks.push(ScheduledTask::new(
        "indexing-recovery",
        indexing_schedule,
        Duration::from_secs(55),
        move || {
            let worker = Arc::clone(&indexing);
            async move { worker.run_available_jobs(100).await.map(drop) }
        },
    ));
    let backup_recovery = Arc::clone(&backup);
    tasks.push(ScheduledTask::new(
        "backup-recovery",
        JvmTaskSchedules::BACKUP_RECOVERY,
        Duration::from_secs(540),
        move || {
            let worker = Arc::clone(&backup_recovery);
            async move {
                worker.reclaim_stuck_jobs().await?;
                worker.refresh_and_kick().await
            }
        },
    ));
    tasks.push(ScheduledTask::new(
        "backup-pool-refresh",
        TaskSchedule::Fixed {
            every: Duration::from_secs(5),
            start_immediately: true,
        },
        Duration::from_secs(4),
        move || {
            let worker = Arc::clone(&backup);
            async move { worker.refresh_and_kick().await }
        },
    ));
    tasks.push(ScheduledTask::new(
        "restore-recovery",
        JvmTaskSchedules::BACKUP_RECOVERY,
        Duration::from_secs(540),
        move || {
            let worker = Arc::clone(&restore);
            async move { worker.run_available_jobs(100).await.map(drop) }
        },
    ));
    tasks.push(slot_cleaner.scheduled_task());
    let wal_retention = Arc::clone(&partition_retention);
    let wal_retention_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new_unbounded(
        "wal-log-truncator",
        JvmTaskSchedules::WAL_LOG_TRUNCATION,
        move || {
            let worker = Arc::clone(&wal_retention);
            let gate = Arc::clone(&wal_retention_gate);
            async move {
                if gate.is_failing_over().await? {
                    return Ok::<(), ProductionWorkerTaskError>(());
                }
                let outcome = worker
                    .truncate_wal(chrono::Utc::now())
                    .instrument(tracing::info_span!("wal-log/truncate-old-partitions!"))
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                tracing::info!(
                    target: "instant.partition-retention",
                    partitions = ?outcome.wal_partitions,
                    "WAL-log partitions truncated"
                );
                Ok(())
            }
        },
    ));
    let history_retention_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new_unbounded(
        "history-truncator",
        JvmTaskSchedules::HISTORY_TRUNCATION,
        move || {
            let worker = Arc::clone(&partition_retention);
            let gate = Arc::clone(&history_retention_gate);
            async move {
                if gate.is_failing_over().await? {
                    return Ok::<(), ProductionWorkerTaskError>(());
                }
                let outcome = worker
                    .truncate_history(chrono::Utc::now())
                    .instrument(tracing::info_span!("history/truncate-old-partitions!"))
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                tracing::info!(
                    target: "instant.partition-retention",
                    partitions = ?outcome.history_partitions,
                    "history partitions truncated"
                );
                Ok(())
            }
        },
    ));
    let pending = Arc::clone(&webhooks);
    tasks.push(ScheduledTask::new(
        "webhook-pending",
        webhook_schedule,
        Duration::from_secs(110),
        move || {
            let worker = Arc::clone(&pending);
            async move { worker.drain_pending().await.map(drop) }
        },
    ));
    let retries = Arc::clone(&webhooks);
    tasks.push(ScheduledTask::new(
        "webhook-retries",
        webhook_schedule,
        Duration::from_secs(110),
        move || {
            let worker = Arc::clone(&retries);
            async move { worker.drain_retries().await.map(drop) }
        },
    ));
    tasks.push(ScheduledTask::new(
        "webhook-stuck-recovery",
        webhook_schedule,
        Duration::from_secs(110),
        move || {
            let worker = Arc::clone(&webhooks);
            async move { worker.free_stuck_events().await.map(drop) }
        },
    ));
    let custodian_tick = Arc::clone(custodian);
    let custodian_error_sleep =
        crate::CustodianRuntimeConfig::for_environment(environment).idle_sleep();
    tasks.push(ScheduledTask::new_persistent("custodian", move || {
        let worker = Arc::clone(&custodian_tick);
        async move {
            loop {
                let config = match worker.runtime_config().await {
                    Ok(config) => config,
                    Err(error) => {
                        tracing::error!(
                            target: "instant.custodian",
                            error = %error,
                            "custodian configuration refresh failed"
                        );
                        tokio::time::sleep(custodian_error_sleep).await;
                        continue;
                    }
                };
                match worker.tick_with_config(config).await {
                    Ok(
                        CustodianTickOutcome::Idle
                        | CustodianTickOutcome::Paused { .. }
                        | CustodianTickOutcome::FailoverPaused,
                    ) => tokio::time::sleep(config.idle_sleep()).await,
                    Ok(
                        CustodianTickOutcome::CompletedStep { .. }
                        | CustodianTickOutcome::AbortedRestoredPlan { .. }
                        | CustodianTickOutcome::FailedAttempt { .. },
                    ) => {}
                    Err(error) => {
                        tracing::error!(
                            target: "instant.custodian",
                            error = %error,
                            "custodian tick failed"
                        );
                        tokio::time::sleep(config.idle_sleep()).await;
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), ProductionWorkerTaskError>(())
        }
    }));
    let custodian_reaper = Arc::clone(custodian);
    let custodian_reaper_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new(
        "custodian-reaper",
        JvmTaskSchedules::CUSTODIAN_REAPER,
        Duration::from_secs(55),
        move || {
            let worker = Arc::clone(&custodian_reaper);
            let gate = Arc::clone(&custodian_reaper_gate);
            async move {
                if destructive_workers_paused(&gate, Some("custodian-disabled?")).await? {
                    return Ok(());
                }
                let config = worker
                    .runtime_config()
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                worker
                    .reap_stuck_with_timeout(chrono::Utc::now(), config.lease_timeout())
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                worker
                    .reap_failed_with_retention(chrono::Utc::now(), config.failed_retention())
                    .await
                    .map(drop)
                    .map_err(ProductionWorkerTaskError::work)
            }
        },
    ));
    let hard_deletion_worker = Arc::clone(custodian);
    let hard_deletion_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new(
        "hard-deletion-sweep",
        JvmTaskSchedules::HARD_DELETION_SWEEP,
        Duration::from_mins(5),
        move || {
            let worker = Arc::clone(&hard_deletion_worker);
            let gate = Arc::clone(&hard_deletion_gate);
            async move {
                if destructive_workers_paused(&gate, Some("hard-deletion-sweeper-disabled?"))
                    .await?
                {
                    return Ok(());
                }
                let config = worker
                    .runtime_config()
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                worker
                    .enqueue_due_deletions(chrono::Utc::now(), 2)
                    .instrument(tracing::info_span!("hard-deletion-sweeper/sweep"))
                    .await
                    .map(drop)
                    .map_err(ProductionWorkerTaskError::work)?;
                let pruned = worker
                    .prune_transactions(
                        chrono::Utc::now(),
                        transaction_retention_days_from_env(),
                        config.batch_size(),
                    )
                    .instrument(tracing::info_span!(
                        "hard-deletion-sweeper/prune-transactions"
                    ))
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                if pruned > 0 {
                    tracing::info!(
                        target: "instant.custodian",
                        transactions = pruned,
                        "deleted transactions beyond the retention window"
                    );
                }
                Ok::<(), ProductionWorkerTaskError>(())
            }
        },
    ));
    let triples_size_gate = Arc::clone(failover);
    let triples_size_schedule = if environment == ServerEnvironment::Development {
        JvmTaskSchedules::TRIPLES_SIZE_COLLECTION_DEVELOPMENT
    } else {
        JvmTaskSchedules::TRIPLES_SIZE_COLLECTION_HOSTED
    };
    tasks.push(ScheduledTask::new_unbounded(
        "triples-size-collection",
        triples_size_schedule,
        move || {
            let worker = Arc::clone(&triples_size);
            let gate = Arc::clone(&triples_size_gate);
            async move {
                if gate.is_failing_over().await? {
                    return Ok::<(), ProductionWorkerTaskError>(());
                }
                let flags = gate
                    .flag_values(&[
                        "disable-triples-size-collection",
                        "triples-size-collection-batch-size",
                        "triples-size-collection-max-loops",
                        "triples-size-collection-loop-limit-alert-threshold",
                    ])
                    .await?;
                let config = TriplesSizeCollectionConfig::from_flags(&flags)
                    .map_err(ProductionWorkerTaskError::work)?;
                if config.disabled {
                    return Ok(());
                }
                let outcome = worker
                    .collect_batches(config.max_loops, config.batch_size)
                    .instrument(tracing::info_span!(
                        target: "instant.triples-size",
                        "instant.model.triples-size-updates/collect-batches",
                        max_loops = config.max_loops,
                    ))
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                tracing::info!(
                    target: "instant.triples-size",
                    loops = outcome.loops,
                    total_collected = outcome.total_collected,
                    total_aggregated = outcome.total_aggregated,
                    consecutive_limit_hits = outcome.consecutive_limit_hits,
                    reached_loop_limit = outcome.reached_loop_limit,
                    "triples size collection pass completed"
                );
                Ok(())
            }
        },
    ));
    let storage_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new(
        "storage-sweep",
        JvmTaskSchedules::STORAGE_SWEEP,
        Duration::from_mins(55),
        move || {
            let worker = Arc::clone(&storage);
            let gate = Arc::clone(&storage_gate);
            async move {
                if destructive_workers_paused(&gate, None).await? {
                    return Ok::<(), ProductionWorkerTaskError>(());
                }
                let value = gate.flag_value("storage-sweeper-max-loops").await?;
                let max_loops = configured_storage_sweep_max_loops(value.as_ref())
                    .map_err(ProductionWorkerTaskError::work)?;
                let outcome = worker
                    .sweep(None, max_loops, 1_000)
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                if outcome.reached_loop_limit {
                    tracing::warn!(
                        target: "instant.storage-sweeper",
                        event_name = "storage-sweeper/warn-too-many-loops!",
                        loops = outcome.loops,
                        max_loops,
                        deleted = outcome.deleted,
                        "storage sweep reached its live loop limit"
                    );
                }
                Ok::<(), ProductionWorkerTaskError>(())
            }
        },
    ));
    let rate_limit_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new(
        "rate-limit-key-sweep",
        JvmTaskSchedules::RATE_LIMIT_KEY_SWEEP,
        Duration::from_mins(55),
        move || {
            let worker = Arc::clone(&rate_limits);
            let gate = Arc::clone(&rate_limit_gate);
            async move {
                if destructive_workers_paused(&gate, None).await? {
                    return Ok(());
                }
                let outcome = worker
                    .sweep(
                        DEFAULT_RATE_LIMIT_SWEEP_MAX_LOOPS,
                        RATE_LIMIT_SWEEP_BATCH_SIZE,
                    )
                    .instrument(tracing::info_span!(
                        target: "instant.rate-limit",
                        "rate-limit/handle-sweep!"
                    ))
                    .await
                    .map_err(ProductionWorkerTaskError::work)?;
                if outcome.reached_loop_limit {
                    tracing::error!(
                        target: "instant.rate-limit",
                        parent: None,
                        event_name = "rate-limit/too-many-loops",
                        max_loops = DEFAULT_RATE_LIMIT_SWEEP_MAX_LOOPS,
                        error = RATE_LIMIT_SWEEP_BACKLOG_ERROR,
                        "{RATE_LIMIT_SWEEP_BACKLOG_ERROR}"
                    );
                }
                Ok::<(), ProductionWorkerTaskError>(())
            }
        },
    ));
    let ephemeral_gate = Arc::clone(failover);
    tasks.push(ScheduledTask::new(
        "ephemeral-app-sweep",
        JvmTaskSchedules::EPHEMERAL_APP_SWEEP,
        Duration::from_mins(55),
        move || {
            let worker = Arc::clone(&ephemeral);
            let gate = Arc::clone(&ephemeral_gate);
            async move {
                if destructive_workers_paused(&gate, None).await? {
                    return Ok(());
                }
                worker
                    .sweep_environment(environment)
                    .instrument(tracing::info_span!("ephemeral-app-sweeper/sweep"))
                    .await
                    .map(drop)
                    .map_err(ProductionWorkerTaskError::work)
            }
        },
    ));
    tasks
}

#[derive(Debug, Error)]
enum ProductionWorkerTaskError {
    #[error(transparent)]
    Failover(#[from] FailoverGateError),
    #[error("worker error: {0}")]
    Work(String),
}

impl ProductionWorkerTaskError {
    fn work(error: impl std::fmt::Display) -> Self {
        Self::Work(error.to_string())
    }
}

async fn destructive_workers_paused(
    gate: &PostgresFailoverGate,
    disabled_toggle: Option<&str>,
) -> Result<bool, FailoverGateError> {
    if gate.is_failing_over().await? {
        return Ok(true);
    }
    match disabled_toggle {
        Some(toggle) => gate.is_toggled(toggle).await,
        None => Ok(false),
    }
}

fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    ApiError::new(ApiErrorType::Unknown, "Internal error").into_response()
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use tokio_postgres::NoTls;

    use super::*;

    #[test]
    fn slot_recovery_backoff_starts_at_five_and_caps_at_sixty_seconds() {
        assert_eq!(slot_recovery_delay(0), Duration::from_secs(5));
        assert_eq!(slot_recovery_delay(1), Duration::from_secs(10));
        assert_eq!(slot_recovery_delay(2), Duration::from_secs(20));
        assert_eq!(slot_recovery_delay(3), Duration::from_secs(40));
        assert_eq!(slot_recovery_delay(4), Duration::from_secs(60));
        assert_eq!(slot_recovery_delay(u32::MAX), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn production_runtime_policies_bypass_saturated_data_pools_even_inside_refresh_scope() {
        use crate::postgres_pool::DatabaseWorkload;
        use crate::{
            CoarseTopicsPolicy, DatalogCachePolicy, EffectiveAppStatusPolicy,
            QueryPreparationPolicy, ReceiveTimeoutPolicy, SseRetryIntervalPolicy,
            StatementCancelWaitPolicy,
        };

        let Some(url) = instant_test_support::test_database_url(
            "production_runtime_policies_bypass_saturated_data_pools_even_inside_refresh_scope",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&url, 16)
            .await
            .unwrap();
        let mut setup = pool.acquire().await.unwrap();
        crate::migrate_database(&mut setup).await.unwrap();
        drop(setup);
        let policies = RuntimePostgresPolicies::from_primary_pool(&pool, None);
        let mut interactive = Vec::new();
        for _ in 0..pool.max_size() {
            interactive.push(pool.acquire().await.unwrap());
        }
        let refresh = DatabaseWorkload::Refresh
            .scope(async {
                let mut leases = Vec::new();
                for _ in 0..pool.max_size() {
                    leases.push(pool.acquire().await.unwrap());
                }
                leases
            })
            .await;
        assert_eq!(
            interactive.len() + refresh.len() + pool.control_pool().max_size(),
            16
        );
        let app = Uuid::new_v4();
        let app_ids = [app];
        let original_query = serde_json::json!({"todos":{}});
        DatabaseWorkload::Refresh
            .scope(async {
                tokio::time::timeout(Duration::from_millis(500), async {
                    let (
                        rate,
                        throttle,
                        topics,
                        cache,
                        status,
                        timeout,
                        cancel,
                        retry,
                        prepared,
                        combined,
                        refresh_controls,
                    ) = tokio::join!(
                        policies.app_rate_limits.check_app_ids(&app_ids),
                        policies.refresh_throttle.delay_after_noop(app),
                        policies.coarse_topics.use_coarse_topics(app),
                        policies.datalog_cache.settings(),
                        policies.effective_app_status_policy.status(app),
                        policies.receive_timeout.timeout(Some(app)),
                        policies.statement_cancel_wait.wait_duration(),
                        policies.sse_retry_interval.retry_interval(),
                        policies.query_preparation.prepare(app, &original_query),
                        crate::ReceiveHandlerTimeoutPolicy::handler_timeouts(
                            policies.receive_timeout.as_ref(),
                            Some(app)
                        ),
                        crate::RefreshReadControlsPolicy::read(
                            policies.refresh_read_controls.as_ref(),
                            app,
                            true
                        ),
                    );
                    rate.unwrap();
                    throttle.unwrap();
                    topics.unwrap();
                    cache.unwrap();
                    assert_eq!(status.unwrap().effective_status, crate::AppStatus::Active);
                    timeout.unwrap();
                    cancel.unwrap();
                    retry.unwrap();
                    combined.unwrap();
                    assert_eq!(
                        refresh_controls.unwrap().status.effective_status,
                        crate::AppStatus::Active
                    );
                    assert_eq!(prepared.unwrap().effective, original_query);
                })
                .await
                .expect("production policy readers inherited the saturated data reservation");
            })
            .await;
        drop((interactive, refresh));
    }

    #[tokio::test]
    async fn fresh_query_controls_do_not_share_the_critical_failover_connection() {
        use crate::QueryPreparationPolicy;
        let Some(url) = instant_test_support::test_database_url(
            "fresh_query_controls_do_not_share_the_critical_failover_connection",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&url, 16)
            .await
            .unwrap();
        let mut setup = pool.acquire().await.unwrap();
        crate::migrate_database(&mut setup).await.unwrap();
        drop(setup);
        let critical = PostgresPool::connect_named(&url, 1, "failover_gate")
            .await
            .unwrap();
        let legacy = PostgresFailoverGate::from_pool(critical.clone());
        let held = critical.acquire().await.unwrap();
        let app = Uuid::new_v4();
        let original = serde_json::json!({"todos":{}});
        assert!(
            tokio::time::timeout(Duration::from_millis(25), legacy.prepare(app, &original))
                .await
                .is_err(),
            "the old shared failover path should reproduce head-of-line blocking"
        );
        let policies = RuntimePostgresPolicies::from_primary_pool(&pool, None);
        let prepared = tokio::time::timeout(
            Duration::from_millis(500),
            policies.query_preparation.prepare(app, &original),
        )
        .await
        .expect("fresh query preparation still shares critical failover capacity")
        .unwrap();
        assert_eq!(prepared.effective, original);
        drop(held);
    }

    fn test_oauth_cipher() -> OAuthSecretCipher {
        let mut proto = vec![0x08, 0x00, 0x12, 0x20];
        proto.extend(0_u8..32);
        let keyset = serde_json::json!({
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
        });
        OAuthSecretCipher::from_tink_json(&keyset.to_string()).unwrap()
    }

    #[tokio::test]
    async fn byop_schema_control_pool_reconnects_after_backend_termination() {
        let Some(url) = instant_test_support::test_database_url(
            "byop_schema_control_pool_reconnects_after_backend_termination",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let schema = format!("byop_schema_control_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.first_table (id uuid PRIMARY KEY)"
            ))
            .await
            .unwrap();
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let initial = introspect_byop_schema_from_pool(&pool, &schema)
            .await
            .unwrap();
        assert!(initial.tables.contains_key("first_table"));

        let doomed = pool.acquire().await.unwrap();
        let doomed_pid = doomed
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert!(
            setup
                .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        drop(doomed);
        setup
            .batch_execute(&format!(
                "CREATE TABLE {schema}.second_table (id uuid PRIMARY KEY)"
            ))
            .await
            .unwrap();

        let refreshed = introspect_byop_schema_from_pool(&pool, &schema)
            .await
            .unwrap();
        assert!(refreshed.tables.contains_key("first_table"));
        assert!(refreshed.tables.contains_key("second_table"));
        let replacement = pool.acquire().await.unwrap();
        assert_ne!(
            replacement
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            doomed_pid
        );
        drop(replacement);

        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn byop_control_pool_replaces_a_terminated_backend() {
        let Some(url) = instant_test_support::test_database_url(
            "byop_control_pool_replaces_a_terminated_backend",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let schema = format!("byop_control_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.apps (
                   id uuid PRIMARY KEY,
                   connection_string bytea
                 )"
            ))
            .await
            .unwrap();
        let cipher = test_oauth_cipher();
        let app_id = Uuid::new_v4();
        let encrypted = cipher
            .encrypt_bytes(
                app_id,
                "postgresql://user:pass@db.example:5432/app?currentSchema=tenant",
            )
            .unwrap();
        setup
            .execute(
                &format!("INSERT INTO {schema}.apps (id, connection_string) VALUES ($1, $2)"),
                &[&app_id, &encrypted],
            )
            .await
            .unwrap();
        let separator = if url.contains('?') { '&' } else { '?' };
        let pool = PostgresPool::connect(
            &format!("{url}{separator}options=-csearch_path%3D{schema}"),
            1,
        )
        .await
        .unwrap();
        let doomed = pool.acquire().await.unwrap();
        let doomed_pid = doomed
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert!(
            setup
                .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        drop(doomed);

        let configs = load_byop_configs_from_pool(&pool, &cipher, &url, None)
            .await
            .unwrap();
        let config = &configs[&app_id];
        assert_eq!(config.namespace, "tenant");
        assert!(!config.connection_string.contains("currentSchema"));
        let replacement = pool.acquire().await.unwrap();
        assert_ne!(
            replacement
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            doomed_pid
        );
        drop(replacement);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    fn byop_config(app_id: Uuid, connection_string: &str) -> ProductionByopConfig {
        ProductionByopConfig {
            app_id,
            connection_string: connection_string.to_owned(),
            namespace: "public".to_owned(),
        }
    }

    #[test]
    fn byop_reconciliation_starts_additions_and_restarts_rotations() {
        let unchanged = Uuid::from_u128(1);
        let rotated = Uuid::from_u128(2);
        let removed = Uuid::from_u128(3);
        let added = Uuid::from_u128(4);
        let current = [
            (unchanged, byop_config(unchanged, "postgresql://old/one")),
            (rotated, byop_config(rotated, "postgresql://old/two")),
            (removed, byop_config(removed, "postgresql://old/three")),
        ]
        .into_iter()
        .collect();
        let desired = [
            (unchanged, byop_config(unchanged, "postgresql://old/one")),
            (rotated, byop_config(rotated, "postgresql://new/two")),
            (added, byop_config(added, "postgresql://new/four")),
        ]
        .into_iter()
        .collect();

        let reconciliation = reconcile_byop_configs(&current, &desired);

        assert_eq!(reconciliation.stop, [rotated, removed]);
        assert_eq!(
            reconciliation
                .start
                .iter()
                .map(|config| config.app_id)
                .collect::<Vec<_>>(),
            [rotated, added]
        );
    }

    #[test]
    fn external_byop_slots_are_stable_valid_and_app_scoped() {
        let app_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let slot_name = external_byop_slot_name(app_id);

        assert_eq!(slot_name, "instant_byop_550e8400e29b41d4a716446655440000");
        assert!(slot_name.len() <= 63);
        assert!(
            slot_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        );
    }

    #[test]
    fn missing_byop_slot_is_fatal_to_production_reconciliation() {
        let app_id = Uuid::from_u128(42);
        let missing = InvalidationSupervisorError::Runtime(InvalidationRuntimeError::Transport(
            ReplicationTransportError::Protocol(
                "replication slot does not exist (SQLSTATE 42704)".to_owned(),
            ),
        ));
        let ordinary = InvalidationSupervisorError::Runtime(InvalidationRuntimeError::Transport(
            ReplicationTransportError::Authentication("temporary secret rotation lag".to_owned()),
        ));

        assert!(matches!(
            classify_byop_worker_error(app_id, &missing),
            ProductionByopWorkerError::FatalSlotLoss(error)
                if error.contains(&app_id.to_string()) && error.contains("cannot be recreated safely")
        ));
        assert!(matches!(
            classify_byop_worker_error(app_id, &ordinary),
            ProductionByopWorkerError::Retryable(_)
        ));
    }

    #[test]
    fn production_secrets_require_both_keysets_without_exposing_values() {
        let error = ProductionServerSecrets::from_pairs([(
            "INSTANT_AEAD_KEYSET_JSON",
            "oauth-secret-material",
        )])
        .err()
        .unwrap();
        assert_eq!(
            error,
            ProductionServerSecretsError::Missing("INSTANT_WEBHOOK_KEYSET_JSON")
        );
        assert!(!error.to_string().contains("oauth-secret-material"));
    }

    #[test]
    fn production_secrets_validate_static_presigning_pairs() {
        let pairs = [
            ("INSTANT_AEAD_KEYSET_JSON", "oauth"),
            ("INSTANT_WEBHOOK_KEYSET_JSON", "webhook"),
            ("INSTANT_RATE_LIMIT_HMAC_KEY", "rate-limit"),
            ("INSTANT_S3_PRESIGN_ACCESS_KEY_ID", "access"),
        ];
        assert_eq!(
            ProductionServerSecrets::from_pairs(pairs).err().unwrap(),
            ProductionServerSecretsError::IncompleteS3PresigningCredentials
        );

        let complete = ProductionServerSecrets::from_pairs([
            ("INSTANT_AEAD_KEYSET_JSON", "oauth"),
            ("INSTANT_WEBHOOK_KEYSET_JSON", "webhook"),
            ("INSTANT_RATE_LIMIT_HMAC_KEY", "rate-limit"),
            ("INSTANT_S3_PRESIGN_ACCESS_KEY_ID", "access"),
            ("INSTANT_S3_PRESIGN_SECRET_ACCESS_KEY", "secret"),
            ("INSTANT_S3_PRESIGN_SESSION_TOKEN", "session"),
        ])
        .unwrap();
        assert_eq!(
            complete.s3_presigning.unwrap().session_token.as_deref(),
            Some("session")
        );
    }

    #[test]
    fn configured_gateway_secret_is_validated_without_leaking_it() {
        let required = [
            ("INSTANT_AEAD_KEYSET_JSON", "oauth"),
            ("INSTANT_WEBHOOK_KEYSET_JSON", "webhook"),
            ("INSTANT_RATE_LIMIT_HMAC_KEY", "rate-limit"),
        ];
        assert!(
            ProductionServerSecrets::from_pairs(required)
                .unwrap()
                .dashboard_gateway
                .is_none()
        );
        for value in ["short", "invalid secret containing whitespace"] {
            let error = ProductionServerSecrets::from_pairs(
                required
                    .into_iter()
                    .chain([("INSTANT_DASHBOARD_GATEWAY_TOKEN", value)]),
            )
            .err()
            .unwrap();
            assert_eq!(
                error,
                ProductionServerSecretsError::InvalidDashboardGatewayToken
            );
            assert!(!error.to_string().contains(value));
        }
        assert!(
            ProductionServerSecrets::from_pairs(
                required
                    .into_iter()
                    .chain([("INSTANT_DASHBOARD_GATEWAY_TOKEN", ""),])
            )
            .unwrap()
            .dashboard_gateway
            .is_none()
        );
        assert!(
            ProductionServerSecrets::from_pairs(required.into_iter().chain([(
                "INSTANT_DASHBOARD_GATEWAY_TOKEN",
                "unit-test-only-origin-gateway-secret"
            ),]))
            .unwrap()
            .dashboard_gateway
            .is_some()
        );
    }

    #[test]
    fn private_dashboard_mode_cannot_start_without_an_origin_credential() {
        let required = [
            ("INSTANT_AEAD_KEYSET_JSON", "oauth"),
            ("INSTANT_WEBHOOK_KEYSET_JSON", "webhook"),
            ("INSTANT_RATE_LIMIT_HMAC_KEY", "rate-limit"),
        ];
        assert_eq!(
            ProductionServerSecrets::from_pairs(
                required
                    .into_iter()
                    .chain([("INSTANT_DASHBOARD_PRIVATE", "true"),])
            )
            .err()
            .unwrap(),
            ProductionServerSecretsError::Missing("INSTANT_DASHBOARD_GATEWAY_TOKEN")
        );
        assert_eq!(
            ProductionServerSecrets::from_pairs(
                required
                    .into_iter()
                    .chain([("INSTANT_DASHBOARD_PRIVATE", "typo"),])
            )
            .err()
            .unwrap(),
            ProductionServerSecretsError::InvalidDashboardPrivateMode
        );
        assert!(
            ProductionServerSecrets::from_pairs(required.into_iter().chain([
                ("INSTANT_DASHBOARD_PRIVATE", "true"),
                (
                    "INSTANT_DASHBOARD_GATEWAY_TOKEN",
                    "unit-test-only-origin-gateway-secret"
                ),
            ]))
            .unwrap()
            .dashboard_gateway
            .is_some()
        );
    }

    #[test]
    fn self_hosted_secrets_reject_aws_kms_inputs_immediately() {
        let required = [
            ("INSTANT_AEAD_KEYSET_JSON", "oauth"),
            ("INSTANT_WEBHOOK_KEYSET_JSON", "webhook"),
            ("INSTANT_RATE_LIMIT_HMAC_KEY", "rate-limit"),
        ];
        let kms = ProductionServerSecrets::from_pairs(required.into_iter().chain([(
            "INSTANT_AEAD_KEYSET_KMS_KEY_URL",
            "aws-kms://secret-material",
        )]))
        .err()
        .unwrap();
        assert_eq!(
            kms,
            ProductionServerSecretsError::AwsHostedFeatureRequired(
                "INSTANT_AEAD_KEYSET_KMS_KEY_URL"
            )
        );
        assert!(!kms.to_string().contains("secret-material"));
    }

    #[tokio::test]
    async fn client_ip_middleware_ignores_forwarded_headers_from_untrusted_peers() {
        use axum::body::{Body, to_bytes};
        use axum::extract::Extension;
        use axum::http::StatusCode;
        use axum::routing::get;
        use tower::ServiceExt as _;

        async fn resolved(
            Extension(instant_http::ResolvedClientIp(ip)): Extension<
                instant_http::ResolvedClientIp,
            >,
        ) -> String {
            ip.map_or_else(|| "none".to_owned(), |ip| ip.to_string())
        }

        let trusted: Arc<[IpNet]> = Arc::from(vec!["10.0.0.0/8".parse::<IpNet>().unwrap()]);
        let app = Router::new()
            .route("/ip", get(resolved))
            .layer(middleware::from_fn_with_state(trusted, resolve_client_ip));

        let request = |peer: &str, forwarded: &str| {
            Request::get("/ip")
                .header("x-forwarded-for", forwarded)
                .extension(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::empty())
                .unwrap()
        };

        let response = app
            .clone()
            .oneshot(request("203.0.113.10:1234", "198.51.100.7"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "203.0.113.10");

        let response = app
            .oneshot(request("10.0.0.4:1234", "198.51.100.7"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "198.51.100.7");
    }

    #[tokio::test]
    async fn catch_panic_layer_returns_the_standard_internal_error_body() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use axum::routing::get;
        use tower::ServiceExt as _;

        async fn boom() -> &'static str {
            panic!("handler panic");
        }

        let app = Router::new().route("/boom", get(boom)).layer(
            tower_http::catch_panic::CatchPanicLayer::custom(panic_response),
        );
        let response = app
            .oneshot(Request::get("/boom").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["type"], "unknown");
        assert_eq!(body["message"], "Internal error");
    }
}
