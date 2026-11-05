//! Process-level dependency wiring for the Rust `InstantDB` server.

#![recursion_limit = "256"]

mod add_query_timing;
mod admin_auth;
mod admin_data;
mod admin_metadata;
mod admin_presence;
mod admin_realtime;
mod admin_request_rate_limit;
mod admin_storage;
mod admin_transaction_queue;
mod app_interactive_admission;
mod app_rate_limit;
mod app_status_policy;
mod attribute_wire;
mod audit;
mod aurora_clone;
mod aurora_database;
mod backup_schedule;
mod backup_worker;
mod cloudflare_email;
mod cloudwatch;
mod config;
mod config_app;
mod control_plane_cache;
mod custodian;
mod daily_metrics;
mod dashboard;
mod dashboard_config;
mod dashboard_control;
mod dashboard_refresh_token;
mod database_failover;
mod datalog_cache_policy;
mod db_error;
mod email_router;
mod error_reporting;
mod failover;
mod fresh_control_reads;
mod gauges;
mod google_dashboard;
mod http_response_policy;
mod indexing_worker;
mod invalidation_queue;
mod join_room_logger;
mod key_rotation;
mod keyset_loader;
mod legacy_nippy;
mod lifecycle;
mod load_balancer;
mod magic_code_rate_limit;
mod maintenance;
pub mod metrics;
mod migrations;
// Unit tests use the executable's allocator too. Ordinary library consumers
// continue choosing their own allocator; do not impose one through the library.
#[cfg(test)]
mod native_allocator;
mod nightly_backup;
mod oauth_credentials;
mod oauth_metadata;
mod oauth_settings;
mod partition_retention;
mod platform;
mod platform_oauth;
mod platform_oauth_postgres;
mod postgres_admin_authorizer;
mod postgres_coarse_topics;
mod postgres_conditions;
mod postgres_connections;
mod postgres_dashboard_core;
mod postgres_dashboard_identity;
mod postgres_dashboard_metrics;
mod postgres_dashboard_policy;
mod postgres_dashboard_summary;
mod postgres_magic_code_expiry;
mod postgres_pool;
mod postgres_rate_limit;
mod postgres_realtime_cluster;
mod postgres_runtime_oauth_policy;
mod postgres_storage_policy;
mod postgres_superadmin;
mod postgres_superadmin_store;
mod postgres_wal_health;
mod postgres_webhook_limit;
mod postmark;
mod process;
mod process_memory;
mod production_bootstrap;
mod production_dashboard;
mod production_platform;
mod production_process;
mod production_routes;
mod production_runtime_auth;
mod production_runtime_oauth;
mod query_timing;
mod realtime_receive_dispatch;
mod realtime_receive_policy;
mod receive_timeout;
mod refresh_catalog_reads;
mod refresh_read_controls;
mod refresh_source_reads;
mod refresh_throttle;
mod refresh_timing;
mod replication;
mod replication_slot_cleanup;
mod resend;
mod restore_worker;
mod root_frame_cache;
mod runtime_auth;
mod runtime_oauth;
mod runtime_realtime;
mod runtime_stream;
mod s3_storage;
mod self_hosted_secrets;
mod sender_domain_recheck;
mod sendgrid;
mod sse_retry_policy;
mod statement_cancel;
mod statement_cancel_policy;
mod storage_content;
mod storage_sweeper;
mod superadmin;
mod superadmin_debug;
mod system_catalog_migration;
mod telemetry;
mod transaction_pool_pressure;
mod triples_size;
mod version;
mod wal_history;
mod webhook_payload;
mod webhook_security;
mod webhook_worker;

pub use admin_auth::AdminAuthService;
pub use admin_data::{
    AdminDataAuthorizer, AdminDataScope, AdminDataService, DefaultQueryModifierPolicy,
    DefaultQueryPlannerPolicy, DefaultQuerySettingsPolicy, PostgresQuerySetting,
    PreparedAdminQueryPermissions, PreparedInstantQuery, PrimaryOnlyReplicaReadPolicy,
    QueryModifierPolicy, QueryModifierPolicyFuture, QueryPlannerPolicy, QueryPlannerPolicyFuture,
    QueryPreparationPolicy, QueryPreparationPolicyFuture, QuerySettingsPolicy,
    QuerySettingsPolicyFuture, RefreshCatalogSnapshot, ReplicaReadPolicy, ReplicaReadPolicyFuture,
    apply_postgres_query_settings,
};
pub use admin_metadata::AdminMetadataService;
pub use admin_presence::AdminPresenceService;
pub use admin_realtime::AdminRealtimeService;
pub use admin_request_rate_limit::{
    AdminRequestOperation, AdminRequestRateLimitFuture, AdminRequestRateLimitPolicy,
    AllowAllAdminRequestRateLimits, PostgresAdminRequestRateLimitPolicy,
};
pub use admin_storage::{
    AdminStorageService, AllowAllStorage, DashboardStorageControlHooks, StorageAccessPolicy,
    StorageFileUrlSigner, StorageObjectMetadata, StorageObjectStore, StoragePutRequest, object_key,
    object_key_bin,
};
pub use admin_transaction_queue::{
    ADMIN_TRANSACTION_QUEUE_TIMEOUT, AdminTransactionQueue, AdminTransactionQueuePolicy,
    AdminTransactionQueuePolicyFuture, DisabledAdminTransactionQueue,
    PostgresAdminTransactionQueuePolicy,
};
pub use app_rate_limit::{
    AllowAllAppRateLimits, AppRateLimitFuture, AppRateLimitPolicy, PostgresAppRateLimitPolicy,
    app_rate_limit_middleware,
};
pub use app_status_policy::{
    AppStatus, AppStatusFuture, AppStatusSnapshot, EffectiveAppStatusPolicy,
    PostgresEffectiveAppStatusPolicy, app_disabled_error, app_read_only_error,
};
pub use audit::{AuditFinding, AuditReport, AuditSeverity, audit_database};
pub use aurora_clone::{
    AuroraClone, AuroraCloneError, AuroraCloneLifecycle, AwsAuroraCloneProvider,
    DirectPostgresSnapshotProvider, ProductionSnapshotProvider,
};
pub use aurora_database::{AuroraDatabase, AuroraDatabaseError, AwsAuroraDatabaseProvider};
pub use backup_worker::{
    BackupJobDispatcher, BackupJobOutcome, BackupObjectControlHooks, BackupObjectEntry,
    BackupObjectStore, BackupWorkerError, PostgresBackupControlHooks, PostgresBackupWorker,
    PostgresBackupWorkerPool, ScheduledBackupOptions, ScheduledBackupOutcome,
};
pub use cloudflare_email::{
    CloudflareEmailDomainVerifier, CloudflareEmailError, EmailDomainStatus, EmailDomainVerifier,
};
pub use cloudwatch::{
    AwsDatabaseQueryThroughput, AwsWebhookLatencyRecorder, DatabaseQueryThroughput,
    DatabaseQueryThroughputFuture, NoopWebhookLatencyRecorder, WebhookLatencyRecorder,
};
pub use config::{
    DEFAULT_STORAGE_DOWNLOAD_URL_TTL_SECONDS, DEFAULT_STORAGE_MAX_UPLOAD_BYTES, EmailProvider,
    MAX_STORAGE_DOWNLOAD_URL_TTL_SECONDS, MIN_STORAGE_DOWNLOAD_URL_TTL_SECONDS,
    ServerBackupStorageConfig, ServerConfig, ServerConfigError, ServerEmailConfig,
    ServerEnvironment, ServerStorageConfig, ServerWalHistoryConfig, SharedOAuthClientConfig,
};
pub use config_app::{
    ConfigAppError, INSTANT_HOMEPAGE_APP_ID, INSTANT_SYSTEM_CATALOG_APP_ID, get_config_flag,
    is_reserved_app_id, list_config_flags, set_config_flag, unset_config_flag,
};
pub use control_plane_cache::ControlPlaneCache;
pub use custodian::{
    CustodianError, CustodianRuntimeConfig, CustodianTickOutcome, HardDeletionSweepOutcome,
    PostgresCustodian, transaction_retention_days_from_env,
};
pub use daily_metrics::{
    DailyMetricsError, DailyMetricsFailoverGate, DailyMetricsGateFuture, DailyMetricsRun,
    PostgresDailyMetricsWorker,
};
pub use dashboard::{
    AllowDashboardIdentity, DashboardIdentityPolicy, DashboardIdentityService,
    DashboardMagicCodeDelivery, DashboardService, DashboardSubsystem, DashboardSummaryProvider,
    DashboardUser, RejectUnmatchedDashboard, is_identity_route,
};
pub use dashboard_config::{
    DASHBOARD_BUILD_VERSION, DASHBOARD_CONFIG_PATH, DashboardConfigService,
    DashboardPublicConfigSource, DashboardPublicConfigValues, DashboardSignupMode,
    StaticDashboardPublicConfig, is_dashboard_config_route,
};
pub use dashboard_control::{
    DashboardBackupStreamer, DashboardControlHookOperation, DashboardControlHooks,
    DashboardControlService, DashboardRateLimitHooks, NoopDashboardControlHooks,
    ProductionDashboardTerminalHooks, StaticWebhookLimit, WebhookLimitFuture, WebhookLimitPolicy,
    is_control_route,
};
pub use database_failover::{
    DatabaseFailover, DatabaseFailoverError, DatabaseFailoverLeaderError,
    DatabaseFailoverParticipant, DatabaseFailoverParticipantOutcome,
    DatabaseFailoverParticipantProgress, DatabaseFailoverParticipantStatus, DatabaseFailoverPhase,
    DatabaseFailoverReport, DatabaseFailoverTimings, PostgresDatabaseFailoverCoordinator,
    run_database_failover, run_database_failover_participant,
};
pub use datalog_cache_policy::PostgresDatalogCachePolicy;
pub use email_router::{EmailDeliveryClient, EmailDeliveryError};
pub use error_reporting::{ErrorReporting, ErrorReportingError, init_error_reporting};
pub use failover::{FailoverGateError, PostgresFailoverGate, QueryControlCache};
pub use gauges::{GaugeProvider, GaugeRegistration, NativeGaugeRegistry};
pub use google_dashboard::GoogleDashboardControlHooks;
pub use http_response_policy::{HttpResponsePolicyError, PostgresHttpResponsePolicy};
pub use indexing_worker::{
    IndexingJobOutcome, IndexingWorkerError, PostgresIndexingControlHooks, PostgresIndexingWorker,
};
pub use invalidation_queue::{
    DefaultInvalidationQueuePolicy, InvalidationQueueConfig, InvalidationQueuePolicy,
    InvalidationQueuePolicyFuture, PostgresInvalidationQueuePolicy,
    QueuedRealtimeInvalidationConsumer,
};
pub use join_room_logger::{JoinRoomLogger, NoopJoinRoomLogger, PostgresJoinRoomLogger};
pub use key_rotation::{
    SecretReencryptionCount, SecretReencryptionError, SecretReencryptionReport,
    reencrypt_stored_secrets,
};
pub use keyset_loader::{AwsKmsTinkKeysetLoader, TinkKeysetConfig, TinkKeysetLoadError};
pub use legacy_nippy::{LegacyNippyError, decode_legacy_wal_record};
pub use lifecycle::{
    BackgroundSupervisor, JvmTaskSchedules, ScheduledTask, ShutdownReport, TaskEvent,
    TaskEventKind, TaskSchedule,
};
pub use load_balancer::{
    ConnectionDrainFuture, ConnectionDrainer, DEFAULT_DEREGISTER_TARGETS_DRAIN,
    DEREGISTER_TARGETS_DUPLICATE_WINDOW, DEREGISTER_TARGETS_MAX_GAP, DeregisterTargetsDrainPolicy,
    LoadBalancerDrainError, LoadBalancerDrainListener, LoadBalancerEventSource,
    LoadBalancerEventSubscription, LoadBalancerFuture, LoadBalancerMessage,
    PostgresDeregisterTargetsDrainPolicy, is_deregister_targets_for_instance,
};
pub use magic_code_rate_limit::PostgresMagicCodeRateLimiter;
pub use maintenance::{
    DEFAULT_RATE_LIMIT_SWEEP_MAX_LOOPS, EphemeralAppSweepOutcome, MaintenanceSweepError,
    PostgresEphemeralAppSweeper, PostgresRateLimitSweeper, RATE_LIMIT_SWEEP_BACKLOG_ERROR,
    RATE_LIMIT_SWEEP_BATCH_SIZE, RateLimitSweepOutcome,
};
pub use migrations::{
    MigrationError, MigrationOutcome, force_migration_version, migrate_database, rollback_database,
    verify_database_migrations,
};
pub use nightly_backup::{
    NightlyBackupError, NightlyBackupReport, NightlyBackupRun, NightlyBackupSupervisor,
    NightlyBackupSupervisorError, PostgresNightlyBackupWorker,
};
pub use oauth_credentials::{
    ConfiguredOAuthCredentialProvider, OAuthCredentialError, OAuthSecretCipher,
    TinkOAuthSettingsHooks,
};
pub use oauth_metadata::{
    OAuthMetadataCache, OAuthMetadataFetchError, OAuthMetadataRefreshError,
    OAuthMetadataRefreshReport,
};
pub use oauth_settings::OAuthSettingsHooks;
pub use partition_retention::{
    PartitionRetentionError, PartitionRetentionOutcome, PostgresPartitionRetention,
    history_partition_bucket, history_partitions_to_truncate, wal_partitions_to_truncate,
};
pub use platform::{
    Ed25519PublicJwk, PlatformService, WebhookPayloadStore, payload_idempotency_key,
};
pub use platform_oauth::{PlatformOAuthService, PlatformOAuthStore};
pub use platform_oauth_postgres::PostgresPlatformOAuthStore;
pub use postgres_admin_authorizer::PostgresAdminDataAuthorizer;
pub use postgres_coarse_topics::PostgresCoarseTopicsPolicy;
pub use postgres_connections::{PostgresConnectionDrivers, PostgresConnectionError};
pub use postgres_dashboard_core::PostgresDashboardCoreHooks;
pub use postgres_dashboard_identity::PostgresDashboardIdentityPolicy;
pub use postgres_dashboard_metrics::{
    LocalRuntimeSessionReportCollector, PostgresDashboardMetricsHooks,
    PostgresRuntimeSessionReportCollector, RuntimeSessionReportCollector,
};
pub use postgres_dashboard_policy::{INSTANT_CONFIG_APP_ID, PostgresDashboardPolicyHooks};
pub use postgres_dashboard_summary::PostgresDashboardSummaryProvider;
pub use postgres_magic_code_expiry::PostgresMagicCodeExpiryPolicy;
pub use postgres_pool::{PostgresPool, PostgresPoolError, PostgresPoolStatus};
pub use postgres_rate_limit::PostgresRateLimitConsumer;
pub use postgres_realtime_cluster::PostgresRealtimeCluster;
pub use postgres_runtime_oauth_policy::PostgresRuntimeOAuthPolicy;
pub use postgres_storage_policy::PostgresStorageAccessPolicy;
pub use postgres_superadmin::PostgresSuperadminAuthorizer;
pub use postgres_superadmin_store::PostgresSuperadminStore;
pub use postgres_wal_health::{PostgresInvalidationHealthObserver, PostgresWalHealth};
pub use postgres_webhook_limit::PostgresWebhookLimitPolicy;
pub use postmark::{
    EmailSender, PostgresMagicCodeDelivery, PostmarkAccountClient, PostmarkAdapters,
    PostmarkDashboardControlHooks, PostmarkDashboardMagicCodeDelivery, PostmarkEmail,
    PostmarkEmailClient, PostmarkError,
};
pub use process::{ServerProcessError, ServerShutdownReport, ShutdownSignals, run_server_process};
pub use production_bootstrap::{ProductionDatabaseBootstrapError, ProductionDatabaseClients};
pub use production_dashboard::{
    ProductionDashboardAdapters, ProductionDashboardHooks, production_dashboard_adapters,
    production_dashboard_adapters_with_runtime,
    production_dashboard_adapters_with_runtime_policy_pool,
    production_dashboard_adapters_with_runtime_pools,
};
pub use production_platform::{
    ProductionPlatformError, ProductionPlatformService, production_platform_service,
    production_platform_service_from_config,
    production_platform_service_from_config_with_bearer_pool,
    production_platform_service_from_config_with_pools,
};
pub use production_process::{ProductionServerProcessError, run_production_server_process};
pub use production_routes::{
    ProductionAdminData, ProductionInvalidationSink, ProductionInvalidator,
    ProductionNightlyBackup, ProductionRouteAssembly, ProductionRouteAssemblyError,
    ProductionRuntimeRealtime, ProductionServerSecrets, ProductionServerSecretsError,
    build_production_routes, build_production_routes_with_realtime_history,
};
pub use production_runtime_auth::{
    ProductionRuntimeAuthService, production_runtime_auth_service,
    production_runtime_auth_service_from_pool,
};
pub use production_runtime_oauth::{
    ProductionRuntimeOAuthAdapters, ProductionRuntimeOAuthService,
    production_runtime_oauth_adapters, production_runtime_oauth_adapters_from_pool,
    production_runtime_oauth_service, production_runtime_oauth_service_from_pool,
};
pub use realtime_receive_dispatch::{
    DispatchCompletion, DispatchCompletionError, GroupedProcessFuture,
    GroupedReceiveDispatchConfig, GroupedReceiveDispatcher, GroupedReceiveMetrics,
    ReceiveExecutorClass, TryEnqueueError,
};
pub use realtime_receive_policy::{
    PostgresRealtimeReceiveExecutorPolicy, RealtimeReceiveExecutorFallback,
    RealtimeReceiveExecutorPolicy, RealtimeReceiveExecutorSelection, RealtimeReceiveGaugeProvider,
    RealtimeReceiveMetricsSource, StaticRealtimeReceiveExecutorPolicy,
};
pub use receive_timeout::PostgresReceiveTimeoutPolicy;
pub use refresh_read_controls::{
    PostgresRefreshReadControlsPolicy, RefreshReadControls, RefreshReadControlsFuture,
    RefreshReadControlsPolicy,
};
pub use refresh_throttle::PostgresRefreshThrottlePolicy;
pub use replication::{
    ByopInvalidationPipeline, DurableInvalidationSink, InvalidationHealthObserver,
    InvalidationHealthTransitions, InvalidationRuntimeError, InvalidationSupervisorError,
    NoopInvalidationSink, RealtimeInvalidationConsumer, RealtimeInvalidationSink,
    RealtimeInvalidationSinkError, ReconnectPolicy, SketchAggregatorRuntimeError,
    SketchAggregatorSupervisorError, SketchBatchPolicy, run_byop_invalidation_stream,
    run_invalidation_stream, run_sketch_aggregation_stream,
    run_sketch_aggregation_stream_with_policy, supervise_byop_invalidation_stream,
    supervise_invalidation_stream, supervise_invalidation_stream_with_health,
    supervise_sketch_aggregation_stream, supervise_sketch_aggregation_stream_with_policy,
};
pub use replication_slot_cleanup::{
    PostgresReplicationSlotCleaner, ReplicationSlotCleanupError, ReplicationSlotCleanupOutcome,
    drop_process_invalidator_slot,
};
pub use resend::{ResendEmailClient, ResendError};
pub use restore_worker::{
    PostgresBackupRestoreControlHooks, PostgresRestoreControlHooks, PostgresRestoreWorker,
    RestoreJobOutcome, RestoreWorkerError,
};
pub use root_frame_cache::{RootFrameCacheStats, RootFrameCaches};
pub use runtime_auth::{
    AllowAllSignups, MAX_MAGIC_CODE_EXPIRY_MINUTES, MagicCodeDelivery, MagicCodeExpiryFuture,
    MagicCodeExpiryPolicy, MagicCodeRateLimitFuture, MagicCodeRateLimiter,
    MagicCodeVerifyFailureFuture, NoopMagicCodeRateLimiter, RuntimeAuthConfig, RuntimeAuthPolicy,
    RuntimeAuthService, SignupAuthorizer, StaticMagicCodeExpiry, StaticRuntimeAuthPolicy,
};
pub use runtime_oauth::{
    OAuthCredentialProvider, ResolvedOAuthCredentials, RuntimeOAuthPolicy, RuntimeOAuthService,
    StaticOAuthPolicy,
};
pub use runtime_realtime::{
    CoarseTopicsFuture, CoarseTopicsPolicy, CombineTransactsByDefault, CombineTransactsFuture,
    CombineTransactsPolicy, DEFAULT_RECEIVE_TIMEOUT, DatalogCachePolicy, DatalogCacheSettings,
    DatalogCacheSettingsFuture, InstantQueryTopics, MAX_WAL_REFRESH_BATCH, MAX_WAL_REFRESH_WINDOWS,
    NoRefreshThrottle, PreciseQueryTopics, RealtimeClusterConsumerKind, RealtimeClusterFuture,
    RealtimeWatermarkSeed, ReceiveHandlerTimeoutPolicy, ReceiveHandlerTimeouts,
    ReceiveHandlerTimeoutsFuture, ReceiveTimeoutFuture, ReceiveTimeoutPolicy,
    RefreshThrottleFuture, RefreshThrottlePolicy, RuntimeAdmissionLimits,
    RuntimeJoinRowsQueryBackend, RuntimeLocalAppSessionReport, RuntimeRealtimeBatchQuery,
    RuntimeRealtimeBatchQueryResult, RuntimeRealtimeCatalogBackend, RuntimeRealtimeCluster,
    RuntimeRealtimeClusterDelivery, RuntimeRealtimeClusterEvent, RuntimeRealtimeIdentityBackend,
    RuntimeRealtimeMetrics, RuntimeRealtimeQueryMode, RuntimeRealtimeRefreshBatch,
    RuntimeRealtimeService, RuntimeRealtimeTransaction, RuntimeRealtimeTransactionBackend,
    RuntimeRoomSnapshot, RuntimeSessionReporter, RuntimeStreamPermissionBackend,
    RuntimeSyncBackend, RuntimeSyncHistory, RuntimeSyncSnapshot, RuntimeSyncSnapshotRequest,
    RuntimeSyncSubscription, RuntimeSyncSubscriptionCreate, RuntimeSyncSubscriptionResume,
    SseRetryIntervalFuture, SseRetryIntervalPolicy, StaticDatalogCachePolicy, StaticReceiveTimeout,
    StaticSseRetryInterval,
};
pub use runtime_stream::{
    InMemoryRuntimeStreamBackend, PostgresRuntimeStreamBackend, RuntimeStreamAppendRequest,
    RuntimeStreamAppendResult, RuntimeStreamBackend, RuntimeStreamDisconnectRequest,
    RuntimeStreamFile, RuntimeStreamLoadRequest, RuntimeStreamRecord, RuntimeStreamStartRequest,
};
pub use s3_storage::{
    S3ObjectStore, S3ObjectStoreConfig, S3ObjectStoreRuntimeConfig, S3PresigningCredentials,
    s3_object_store_for_bucket_from_config, s3_object_store_from_config,
};
pub use self_hosted_secrets::{
    SelfHostedSecretGenerationError, SelfHostedSecretRotationError,
    generate_self_hosted_secrets_env, rotate_self_hosted_secrets_env,
};
pub use sender_domain_recheck::{
    PostgresSenderDomainRecheck, SenderDomainRecheckError, SenderDomainRecheckOutcome,
};
pub use sendgrid::{SendGridEmailClient, SendGridError};
pub use sse_retry_policy::PostgresSseRetryIntervalPolicy;
pub use statement_cancel::{
    CancelWaitOutcome, StatementCancelWaitFuture, StatementCancelWaitPolicy, StatementTracker,
    StaticStatementCancelWait,
};
pub use statement_cancel_policy::PostgresStatementCancelWaitPolicy;
pub use storage_sweeper::{
    DEFAULT_MAX_LOOPS as DEFAULT_STORAGE_SWEEP_MAX_LOOPS, PostgresStorageSweeper,
    StorageSweepError, StorageSweepOutcome, configured_storage_sweep_max_loops,
};
pub use superadmin::{
    AppRole, SuperadminAccess, SuperadminAuthorizer, SuperadminDebug, SuperadminPrincipal,
    SuperadminScope, SuperadminService, SuperadminStore, required_access,
};
pub use superadmin_debug::{
    SuperadminDebugService, SuperadminExplainBackend, SuperadminRuntimeBackend,
};
pub use system_catalog_migration::{
    SystemCatalogMigrationError, SystemCatalogMigrationOutcome, SystemCatalogVerificationError,
    ensure_system_catalog, verify_system_catalog,
};
pub use telemetry::{TelemetryError, init_telemetry, install_panic_hook, trace_http_request};
pub use transaction_pool_pressure::{
    DisabledTransactionPoolPressure, PostgresTransactionPoolPressurePolicy,
    TransactionPoolPressureFuture, TransactionPoolPressurePolicy,
};
pub use triples_size::{
    DEFAULT_TRIPLES_SIZE_COLLECTION_ALERT_THRESHOLD, DEFAULT_TRIPLES_SIZE_COLLECTION_BATCH_SIZE,
    DEFAULT_TRIPLES_SIZE_COLLECTION_MAX_LOOPS, PostgresTriplesSizeCollector,
    TriplesSizeBatchOutcome, TriplesSizeCollectionConfig, TriplesSizeCollectionError,
    TriplesSizeCollectionOutcome,
};
pub use version::display_version;

/// Fully PostgreSQL-backed superadmin route service.
pub type PostgresSuperadminService<H> =
    SuperadminService<PostgresSuperadminAuthorizer, PostgresSuperadminStore<H>>;

#[must_use]
pub fn postgres_superadmin_service<H>(
    authorizer_client: tokio_postgres::Client,
    store_client: tokio_postgres::Client,
    hooks: H,
) -> PostgresSuperadminService<H> {
    SuperadminService::new(
        PostgresSuperadminAuthorizer::new(authorizer_client),
        PostgresSuperadminStore::new(store_client, hooks),
    )
}

#[must_use]
pub fn postgres_superadmin_service_with_authorizer_pool<H>(
    authorizer_pool: PostgresPool,
    store_client: tokio_postgres::Client,
    hooks: H,
) -> PostgresSuperadminService<H> {
    SuperadminService::new(
        PostgresSuperadminAuthorizer::from_pool(authorizer_pool),
        PostgresSuperadminStore::new(store_client, hooks),
    )
}

#[must_use]
pub fn postgres_superadmin_service_from_pools<H>(
    authorizer_pool: PostgresPool,
    store_pool: PostgresPool,
    hooks: H,
) -> PostgresSuperadminService<H> {
    SuperadminService::new(
        PostgresSuperadminAuthorizer::from_pool(authorizer_pool),
        PostgresSuperadminStore::from_pool(store_pool, hooks),
    )
}
pub use wal_history::{
    NoopWebhookEventNotifier, PostgresDurableInvalidationSink, StoredWebhookEventKey,
    WalHistoryError, WebhookEventNotifier, WebhookEventWakeupNotifier,
};
pub use webhook_payload::{
    PostgresWebhookPayloadStore, S3WebhookHistoryArchive, StorageObjectReader,
    StorageWebhookFileUrlSigner, WebhookFileUrlSigner, WebhookHistoryArchive,
    WebhookPayloadAuthorizer,
};
pub use webhook_security::{
    Ed25519WebhookKeys, GuardedWebhookHttpTransport, JwtOrBearerWebhookPayloadAuthorizer,
    JwtWebhookPayloadAuthorizer, WebhookBearerAuthorizer, WebhookIpWhitelist,
    WebhookIpWhitelistFuture, WebhookSecurityError,
};
pub use webhook_worker::{
    PostgresWebhookWorker, WebhookAttempt, WebhookDashboardControlHooks, WebhookDeliveryRequest,
    WebhookPayloadTokenIssuer, WebhookQueuedEvent, WebhookTransport, WebhookWorkOutcome,
    WebhookWorkerError,
};
