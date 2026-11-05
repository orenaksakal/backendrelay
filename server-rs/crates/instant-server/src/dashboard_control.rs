use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use base64::Engine as _;
use chrono::{DateTime, NaiveDateTime, Utc};
use instant_auth::generate_magic_code;
use instant_auth::oauth_origins::is_custom_scheme;
use instant_auth::oauth_store::{
    OAuthClientPatch, OAuthClientRecord, OAuthProvider, create_oauth_client, create_oauth_provider,
    delete_oauth_client, oauth_client_by_id, oauth_provider_by_id, update_oauth_client,
};
use instant_http::{
    AdminAttribute, ApiError, ApiErrorType, AttributeCatalogKind, DashboardRequest,
    DashboardResponse, DashboardResponseBody,
};
use instant_instaql::{AttributeCatalog, Cardinality, system_attr_id, system_attribute_catalog};
use instant_permissions::{
    RateLimit, RateLimitBucketConfig, RateLimitConsumer, RateLimitDecision, RateLimitRequest,
    RefillType, RuleSet,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, GenericClient, error::SqlState};
use uuid::Uuid;

use instant_transact::{
    AppStatusWriteCheck, TransactionOptions, compile_transaction,
    execute_transaction_with_app_status_write_check,
};

use crate::MAX_MAGIC_CODE_EXPIRY_MINUTES;
use crate::PostgresPool;
use crate::admin_metadata::{ATTRIBUTE_COLUMNS, row_to_attribute};
use crate::config_app::{
    ConfigAppError, KNOWN_CONFIG_FLAGS, is_reserved_app_id, list_config_flags, set_config_flag,
    unset_config_flag,
};
use crate::dashboard::DashboardSubsystem;
use crate::dashboard_refresh_token::{
    INSERT_SQL as INSERT_REFRESH_TOKEN_SQL, LOAD_USER_SQL, hash_token,
};
use crate::db_error::{database_error, internal_error};
use crate::postgres_dashboard_identity::check_dashboard_email;
use crate::postgres_pool::{PostgresClientGuard, PostgresClientSource};

#[derive(Clone, Debug)]
pub enum DashboardControlHookOperation {
    AfterCreate {
        app_id: Uuid,
        body: Value,
    },
    ClearApp {
        app_id: Uuid,
    },
    EnrichApp {
        app_id: Uuid,
        user_id: Option<Uuid>,
        app: Value,
    },
    SoftDeletedAttributes {
        app_id: Uuid,
    },
    OrgDetails {
        org_id: Uuid,
        user_id: Uuid,
        org: Value,
    },
    AfterTransfer {
        app_id: Uuid,
        org_id: Uuid,
        user_id: Uuid,
        removed_members: Value,
    },
    DeliverInvite {
        target: &'static str,
        foreign_key: Uuid,
        inviter_id: Uuid,
        inviter_email: String,
        invitee_email: String,
        title: String,
        role: String,
    },
    AuthorizeClaim {
        app_id: Uuid,
        current_creator_id: Option<Uuid>,
        claimant_id: Uuid,
        claimant_email: String,
    },
    EncryptOAuthSecret {
        client_id: Uuid,
        plaintext: String,
    },
    ValidateOAuthDiscovery {
        endpoint: String,
    },
    AuthorizeSharedOAuthCredentials {
        app_id: Uuid,
        provider_name: String,
    },
    DashboardAuthDefaultSender {
        app_id: Uuid,
    },
    SpecialAppConfiguration {
        kind: &'static str,
    },
    EnqueueIndexingJob {
        job_id: Uuid,
    },
    ValidateWebhookSink {
        url: String,
    },
    EnableWebhookLogs {
        app_id: Uuid,
    },
    NotifyWebhookEvent {
        app_id: Uuid,
        webhook_id: Uuid,
        isn: String,
    },
    AuthorizeBackup {
        app_id: Uuid,
        creator_id: Option<Uuid>,
        work_estimate: i64,
        client_ip: Option<String>,
    },
    BackupRateLimitConfiguration,
    EnqueueBackupJob {
        job_id: Uuid,
    },
    ListBackupFiles {
        app_id: Uuid,
        backup_id: Uuid,
        storage_prefix: String,
    },
    PresignBackupFile {
        app_id: Uuid,
        backup_id: Uuid,
        object_key: String,
    },
    StreamBackupStorageFiles {
        app_id: Uuid,
        backup_id: Uuid,
        object_key: String,
    },
    UploadDashboardFile {
        app_id: Uuid,
        path: String,
        content_type: Option<String>,
        content_length: Option<u64>,
        body: Vec<u8>,
    },
    DeleteDashboardFiles {
        app_id: Uuid,
        paths: Vec<String>,
    },
    SessionCounts,
    AppSessionStats {
        app_id: Uuid,
    },
    /// Per-app live session totals for a whole workspace in one fanout, keyed
    /// by app ID. Used by the apps summary so it never loops per app.
    AppSessionCounts,
    ActiveSessionStats,
    DefaultEmailTemplate,
    SyncEmailSender {
        app_id: Uuid,
        email: String,
        name: String,
    },
    GetEmailSender {
        postmark_id: i32,
    },
    /// Checks a sender domain with the configured email provider. Returns
    /// `null` when the provider has no domain-level verification (Postmark),
    /// otherwise `{"enabled", "zone-id", "error"}`.
    VerifyEmailDomain {
        app_id: Uuid,
        domain: String,
    },
    AuthorizeEmailSend {
        kind: &'static str,
        app_id: Uuid,
        email: String,
    },
    DeliverDashboardEmail {
        kind: &'static str,
        app_id: Uuid,
        to: String,
        sender_email: Option<String>,
        sender_name: String,
        subject: String,
        body: String,
    },
    AuthorizeDashboardAdmin {
        email: String,
    },
    PersistRestoreZip {
        job_id: Uuid,
        app_id: Uuid,
        body: Vec<u8>,
        staged_path: Option<String>,
    },
    StageBackupRestoreZip {
        job_id: Uuid,
        storage_prefix: String,
    },
    DiscardRestoreZip {
        path: String,
    },
    EnqueueRestoreJob {
        job_id: Uuid,
    },
    DashboardAdminRecentUsers,
    DashboardAdminTopUsers {
        days: i32,
    },
    DashboardAdminStorageMetrics,
    DashboardAdminOverviewDaily,
    DashboardAdminOverviewMinute,
    DashboardOAuthConfiguration {
        redirect_to_dev: bool,
    },
    DashboardOAuthExchangeCode {
        code: String,
        redirect_uri: String,
    },
    AuthorizeDashboardOAuthSignup {
        email: String,
    },
}

/// Complex schema/rules/clear/transfer projections shared with existing Rust kernels.
pub trait DashboardControlHooks: Send + Sync + 'static {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

/// Dedicated streaming boundary for the potentially large backup `$files`
/// projection. Keeping this separate from JSON-valued control hooks lets the
/// HTTP layer preserve backpressure all the way to the object store.
pub trait DashboardBackupStreamer: Send + Sync + 'static {
    fn stream_backup_storage_files(
        &self,
        app_id: Uuid,
        backup_id: Uuid,
        object_key: String,
    ) -> Pin<Box<dyn Future<Output = Result<DashboardResponseBody, ApiError>> + Send + '_>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopDashboardControlHooks;

impl DashboardControlHooks for NoopDashboardControlHooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        ready(Ok(match operation {
            DashboardControlHookOperation::EnrichApp { app, .. }
            | DashboardControlHookOperation::OrgDetails { org: app, .. } => app,
            DashboardControlHookOperation::SoftDeletedAttributes { .. } => json!([]),
            DashboardControlHookOperation::AfterTransfer {
                removed_members, ..
            } => json!({
                "credit": null,
                "app_member_changes": {"removed": removed_members.as_array().into_iter().flatten().map(|member| json!({
                    "member": member,
                    "reason": "user_is_member_of_org",
                })).collect::<Vec<_>>()}
            }),
            DashboardControlHookOperation::AfterCreate { .. }
            | DashboardControlHookOperation::ClearApp { .. }
            | DashboardControlHookOperation::DeliverInvite { .. }
            | DashboardControlHookOperation::EncryptOAuthSecret { .. }
            | DashboardControlHookOperation::DashboardAuthDefaultSender { .. }
            | DashboardControlHookOperation::SpecialAppConfiguration { .. }
            | DashboardControlHookOperation::EnqueueIndexingJob { .. }
            | DashboardControlHookOperation::EnableWebhookLogs { .. }
            | DashboardControlHookOperation::NotifyWebhookEvent { .. }
            | DashboardControlHookOperation::EnqueueBackupJob { .. }
            | DashboardControlHookOperation::ListBackupFiles { .. }
            | DashboardControlHookOperation::PresignBackupFile { .. }
            | DashboardControlHookOperation::StreamBackupStorageFiles { .. }
            | DashboardControlHookOperation::UploadDashboardFile { .. }
            | DashboardControlHookOperation::DeleteDashboardFiles { .. }
            | DashboardControlHookOperation::SyncEmailSender { .. }
            | DashboardControlHookOperation::GetEmailSender { .. }
            | DashboardControlHookOperation::VerifyEmailDomain { .. }
            | DashboardControlHookOperation::DeliverDashboardEmail { .. }
            | DashboardControlHookOperation::PersistRestoreZip { .. }
            | DashboardControlHookOperation::StageBackupRestoreZip { .. }
            | DashboardControlHookOperation::DiscardRestoreZip { .. }
            | DashboardControlHookOperation::EnqueueRestoreJob { .. }
            | DashboardControlHookOperation::DashboardAdminRecentUsers
            | DashboardControlHookOperation::DashboardAdminTopUsers { .. }
            | DashboardControlHookOperation::DashboardAdminStorageMetrics
            | DashboardControlHookOperation::DashboardAdminOverviewDaily
            | DashboardControlHookOperation::DashboardAdminOverviewMinute
            | DashboardControlHookOperation::DashboardOAuthConfiguration { .. }
            | DashboardControlHookOperation::DashboardOAuthExchangeCode { .. } => Value::Null,
            DashboardControlHookOperation::AuthorizeClaim { .. }
            | DashboardControlHookOperation::ValidateOAuthDiscovery { .. }
            | DashboardControlHookOperation::AuthorizeSharedOAuthCredentials { .. }
            | DashboardControlHookOperation::ValidateWebhookSink { .. }
            | DashboardControlHookOperation::AuthorizeEmailSend { .. }
            | DashboardControlHookOperation::AuthorizeDashboardAdmin { .. }
            | DashboardControlHookOperation::AuthorizeDashboardOAuthSignup { .. } => {
                Value::Bool(false)
            }
            DashboardControlHookOperation::AuthorizeBackup { .. } => Value::Bool(true),
            DashboardControlHookOperation::BackupRateLimitConfiguration => json!({
                "app": {"capacity": 1, "windowMinutes": 5},
                "ip": {"capacity": 2, "windowMinutes": 5},
            }),
            DashboardControlHookOperation::SessionCounts
            | DashboardControlHookOperation::AppSessionCounts => json!({}),
            DashboardControlHookOperation::AppSessionStats { .. } => {
                json!({"count": 0, "origins": {}})
            }
            DashboardControlHookOperation::ActiveSessionStats => {
                json!({"total-count": 0, "total-queries": 0})
            }
            DashboardControlHookOperation::DefaultEmailTemplate => json!({
                "sender-email": "verify@example.test",
                "subject": "{code} is your verification code for {app_title}",
                "body": "Your verification code is {code}. It expires in {expiration}.",
                "verification-code-expiry-minutes": 1440
            }),
        }))
    }
}

/// Strict terminal for the production adapter chain. App creation has no
/// provider-owned post-commit action; every other operation must be claimed by
/// a concrete native adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProductionDashboardTerminalHooks;

impl DashboardControlHooks for ProductionDashboardTerminalHooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        ready(match operation {
            DashboardControlHookOperation::AfterCreate { .. } => Ok(Value::Null),
            DashboardControlHookOperation::AfterTransfer {
                removed_members, ..
            } => Ok(json!({
                "credit": null,
                "app_member_changes": {"removed": removed_members.as_array().into_iter().flatten().map(|member| json!({
                    "member": member,
                    "reason": "user_is_member_of_org",
                })).collect::<Vec<_>>()}
            })),
            other => Err(ApiError::new(
                ApiErrorType::Unknown,
                format!("Production dashboard operation has no native adapter: {other:?}"),
            )),
        })
    }
}

/// Distributed dashboard throttles backed by the same production consumer as
/// permission-rule limits.
pub struct DashboardRateLimitHooks<D> {
    inner: D,
    consumer: Arc<dyn RateLimitConsumer>,
    magic_code_rate_limiter: Arc<dyn crate::MagicCodeRateLimiter>,
}

impl<D> DashboardRateLimitHooks<D> {
    #[must_use]
    pub fn new(
        inner: D,
        consumer: Arc<dyn RateLimitConsumer>,
        magic_code_rate_limiter: Arc<dyn crate::MagicCodeRateLimiter>,
    ) -> Self {
        Self {
            inner,
            consumer,
            magic_code_rate_limiter,
        }
    }
}

impl<D> DashboardControlHooks for DashboardRateLimitHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::AuthorizeEmailSend {
                kind,
                app_id,
                email,
            } => {
                match kind {
                    "sender-verification" => {
                        self.magic_code_rate_limiter
                            .check_custom_sender(&email)
                            .await?;
                    }
                    "test" => {
                        self.magic_code_rate_limiter
                            .check_send(app_id, &email)
                            .await?;
                    }
                    _ => {
                        return Err(ApiError::new(
                            ApiErrorType::Unknown,
                            format!("Unknown dashboard email rate-limit kind: {kind}"),
                        ));
                    }
                }
                Ok(Value::Bool(true))
            }
            DashboardControlHookOperation::AuthorizeBackup {
                app_id,
                creator_id,
                work_estimate,
                client_ip,
            } => {
                let policy_allowed = self
                    .inner
                    .execute(DashboardControlHookOperation::AuthorizeBackup {
                        app_id,
                        creator_id,
                        work_estimate,
                        client_ip: client_ip.clone(),
                    })
                    .await?
                    .as_bool()
                    .unwrap_or(false);
                if !policy_allowed {
                    return Ok(Value::Bool(false));
                }
                let configuration = self
                    .inner
                    .execute(DashboardControlHookOperation::BackupRateLimitConfiguration)
                    .await?;
                let (app_capacity, app_window) = backup_rate_limit(&configuration, "app", 1, 5);
                let mut requests = vec![dashboard_rate_limit_request(
                    app_id,
                    "on-demand-backup-app",
                    Value::Null,
                    app_capacity,
                    app_window,
                )];
                if let Some(client_ip) = client_ip {
                    let (ip_capacity, ip_window) = backup_rate_limit(&configuration, "ip", 2, 5);
                    requests.push(dashboard_rate_limit_request(
                        Uuid::nil(),
                        "on-demand-backup-ip",
                        json!(client_ip),
                        ip_capacity,
                        ip_window,
                    ));
                }
                let decisions = self
                    .consumer
                    .consume_all(&requests)
                    .map_err(dashboard_rate_limit_store_error)?;
                Ok(Value::Bool(decisions.iter().all(|decision| {
                    matches!(decision, RateLimitDecision::Allowed { .. })
                })))
            }
            other => self.inner.execute(other).await,
        }
    }
}

fn dashboard_rate_limit_store_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::log_database_error(&error.to_string());
    ApiError::new(ApiErrorType::Unknown, "Dashboard rate-limit store failed")
}

fn backup_rate_limit(
    configuration: &Value,
    key: &str,
    default_capacity: u64,
    default_window_minutes: u64,
) -> (u64, Duration) {
    let value = configuration.get(key);
    let capacity = value
        .and_then(|value| value.get("capacity"))
        .and_then(Value::as_u64)
        .filter(|capacity| *capacity > 0)
        .unwrap_or(default_capacity);
    let window_minutes = value
        .and_then(|value| value.get("windowMinutes"))
        .and_then(Value::as_u64)
        .filter(|minutes| *minutes > 0)
        .unwrap_or(default_window_minutes);
    (capacity, Duration::from_mins(window_minutes))
}

fn dashboard_rate_limit_request(
    app_id: Uuid,
    bucket_name: &str,
    bucket_key: Value,
    capacity: u64,
    refill_period: Duration,
) -> RateLimitRequest {
    RateLimitRequest {
        app_id,
        bucket_name: bucket_name.to_owned(),
        config: RateLimitBucketConfig {
            limits: vec![RateLimit {
                capacity,
                refill_amount: capacity,
                refill_period,
                refill_type: RefillType::Greedy,
            }],
        },
        bucket_key,
        tokens: 1,
    }
}

impl<T> DashboardControlHooks for std::sync::Arc<T>
where
    T: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        T::execute(self, operation).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Role {
    Collaborator,
    Admin,
    Owner,
}

impl Role {
    const fn name(self) -> &'static str {
        match self {
            Self::Collaborator => "collaborator",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }
}

#[derive(Clone, Debug)]
struct Principal {
    user_id: Option<Uuid>,
    admin_app_id: Option<Uuid>,
    role: Option<Role>,
    /// The caller presented an OAuth platform token with the `apps-write`
    /// scope, which explicitly grants management rights beyond a member role.
    admin_scope: bool,
}

struct DashboardOAuthConfig {
    authorization_endpoint: String,
    client_id: String,
    callback_url: String,
    dashboard_origin: String,
    dev_dashboard_origin: String,
    secure_cookie: bool,
}

/// PostgreSQL-backed dashboard app and organization control plane.
pub struct DashboardControlService<H> {
    client: PostgresClientSource,
    hooks: H,
    backup_streamer: Option<Arc<dyn DashboardBackupStreamer>>,
    webhook_limit: Arc<dyn WebhookLimitPolicy>,
    dashboard_allowed_emails: Option<Arc<HashSet<String>>>,
    superuser_email: Option<String>,
}

pub type WebhookLimitFuture<'a> = Pin<Box<dyn Future<Output = Result<i64, ApiError>> + Send + 'a>>;

/// Resolves the live global cap for active webhooks per app.
pub trait WebhookLimitPolicy: std::fmt::Debug + Send + Sync + 'static {
    fn maximum_active_webhooks(&self) -> WebhookLimitFuture<'_>;
}

#[derive(Clone, Copy, Debug)]
pub struct StaticWebhookLimit(pub i64);

impl WebhookLimitPolicy for StaticWebhookLimit {
    fn maximum_active_webhooks(&self) -> WebhookLimitFuture<'_> {
        Box::pin(ready(Ok(self.0)))
    }
}

impl<H> DashboardControlService<H> {
    #[must_use]
    pub fn new(client: Client, hooks: H) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            hooks,
            backup_streamer: None,
            webhook_limit: Arc::new(StaticWebhookLimit(100)),
            dashboard_allowed_emails: None,
            superuser_email: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, hooks: H) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            hooks,
            backup_streamer: None,
            webhook_limit: Arc::new(StaticWebhookLimit(100)),
            dashboard_allowed_emails: None,
            superuser_email: None,
        }
    }

    async fn acquire_client(&self) -> Result<PostgresClientGuard<'_>, ApiError> {
        self.client.acquire().await.map_err(database_error)
    }

    #[must_use]
    pub fn with_backup_streamer(mut self, streamer: Arc<dyn DashboardBackupStreamer>) -> Self {
        self.backup_streamer = Some(streamer);
        self
    }

    #[must_use]
    pub fn with_webhook_limit_policy(mut self, policy: Arc<dyn WebhookLimitPolicy>) -> Self {
        self.webhook_limit = policy;
        self
    }

    /// Applies the operator's dashboard email allowlist and superuser override
    /// to bearer tokens and Google logins.
    #[must_use]
    pub fn with_dashboard_email_policy(
        mut self,
        allowed_emails: Option<HashSet<String>>,
        superuser_email: Option<String>,
    ) -> Self {
        self.dashboard_allowed_emails = allowed_emails.map(Arc::new);
        self.superuser_email = superuser_email.map(|email| email.trim().to_ascii_lowercase());
        self
    }

    fn dashboard_email_allowed(&self, email: &str) -> bool {
        check_dashboard_email(
            self.dashboard_allowed_emails.as_deref(),
            self.superuser_email.as_deref(),
            email,
        )
        .is_ok()
    }

    async fn check_dashboard_bearer_email(
        &self,
        request: &DashboardRequest,
    ) -> Result<(), ApiError> {
        if self.dashboard_allowed_emails.is_none() {
            return Ok(());
        }
        let Some(bearer) = optional_bearer(request).filter(|value| !value.is_empty()) else {
            return Ok(());
        };
        let client = self.acquire_client().await?;
        let Ok(principal) = authenticate_scoped(&*client, bearer, "apps-read").await else {
            return Ok(());
        };
        let Some(user_id) = principal.user_id else {
            return Ok(());
        };
        let email = client
            .query_one("SELECT email FROM instant_users WHERE id = $1", &[&user_id])
            .await
            .map_err(database_error)?
            .get::<_, String>(0);
        check_dashboard_email(
            self.dashboard_allowed_emails.as_deref(),
            self.superuser_email.as_deref(),
            &email,
        )
    }
}

impl<H> DashboardSubsystem for DashboardControlService<H>
where
    H: DashboardControlHooks,
{
    async fn authorize_restore_upload(&self, request: DashboardRequest) -> Result<(), ApiError> {
        self.authenticate_dashboard_admin(&request).await?;
        Ok(())
    }

    fn matches(&self, request: &DashboardRequest) -> bool {
        is_control_route(request.method.as_str(), &request.path)
    }

    #[allow(clippy::too_many_lines)] // Exact method/path matrix is clearest as one dispatcher.
    async fn handle(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        self.check_dashboard_bearer_email(&request).await?;
        let request_path = request.path.clone();
        match (request.method.as_str(), request_path.as_str()) {
            ("GET", "/dash/admin") => self.get_dashboard_admin(request).await,
            ("GET", "/dash/check-admin") => self.check_dashboard_admin(request).await,
            ("GET", "/dash/operator/flags") => self.get_operator_flags(request).await,
            ("PUT", path) if operator_flag_name(path).is_some() => {
                self.put_operator_flag(request).await
            }
            ("DELETE", path) if operator_flag_name(path).is_some() => {
                self.delete_operator_flag(request).await
            }
            ("GET", "/dash/top") => self.get_dashboard_top_users(request).await,
            ("GET", "/dash/storage") => self.get_dashboard_storage(request).await,
            ("GET", "/dash/overview/daily") => self.get_dashboard_overview_daily(request).await,
            ("GET", "/dash/overview/minute") => self.get_dashboard_overview_minute(request).await,
            ("GET", "/dash/oauth/start") => self.start_dashboard_oauth(request).await,
            ("GET", "/dash/oauth/callback") => self.callback_dashboard_oauth(request).await,
            ("POST", "/dash/oauth/token") => self.exchange_dashboard_oauth_code(request).await,
            ("POST", "/dash/restores/zip") => self.create_restore_job(request).await,
            ("GET", "/dash/restore-jobs") => self.list_restore_jobs(request).await,
            ("GET", path) if app_setting_id(path, "restore-jobs").is_some() => {
                self.list_app_restore_jobs(request).await
            }
            ("POST", path) if backup_action_ids(path, "restore").is_some() => {
                self.create_backup_restore_job(request).await
            }
            ("GET", "/dash/stats/active_sessions") => self.get_active_session_stats(request).await,
            ("GET", path) if restore_job_id(path).is_some() => self.get_restore_job(request).await,
            ("DELETE", path) if restore_job_id(path).is_some() => {
                self.cancel_restore_job(request).await
            }
            ("POST", "/dash/apps") => self.create_app(request).await,
            ("POST", "/dash/orgs") => self.create_org(request).await,
            ("POST", "/dash/apps/ephemeral") => self.create_special_app(request, true).await,
            ("POST", "/dash/apps/get_a_db") => self.create_special_app(request, false).await,
            ("GET", path) if special_app_id(path, true, None).is_some() => {
                self.get_special_app(request, true).await
            }
            ("GET", path) if special_app_id(path, false, None).is_some() => {
                self.get_special_app(request, false).await
            }
            ("POST", path) if special_app_id(path, true, Some("status")).is_some() => {
                self.set_special_app_status(request).await
            }
            ("POST", path) if action_app_id(path, "clear").is_some() => {
                self.clear_app(request).await
            }
            ("POST", path) if action_app_id(path, "track-import").is_some() => {
                self.track_app_import(request).await
            }
            ("GET", path) if action_app_id(path, "stats").is_some() => {
                self.get_app_session_stats(request).await
            }
            ("GET", path) if action_app_id(path, "usage").is_some() => {
                self.get_app_usage(request).await
            }
            ("GET", "/dash/apps-summary") => self.get_apps_summary(request).await,
            ("POST", path) if action_app_id(path, "tokens").is_some() => {
                self.rotate_admin_token(request).await
            }
            ("POST", path) if app_setting_id(path, "tokens/rotate").is_some() => {
                self.rotate_admin_token_server(request).await
            }
            ("GET", path) if action_app_id(path, "soft_deleted_attrs").is_some() => {
                self.soft_deleted_attributes(request).await
            }
            ("POST", path) if action_app_id(path, "rename").is_some() => {
                self.rename_app(request).await
            }
            ("POST", path) if action_app_id(path, "status").is_some() => {
                self.set_app_status(request).await
            }
            ("POST", path) if action_app_id(path, "set-magic-code-expiry").is_some() => {
                self.set_magic_code_expiry(request).await
            }
            ("POST", path) if app_setting_id(path, "rules/validate").is_some() => {
                self.validate_rules(request).await
            }
            ("POST", path) if action_app_id(path, "rules").is_some() => {
                self.put_rules(request).await
            }
            ("GET", path) if action_app_id(path, "rule-versions").is_some() => {
                self.get_rule_versions(request).await
            }
            ("GET", path)
                if app_setting_id(path, "perms/pull").is_some()
                    || app_setting_id(path, "schema/pull").is_some() =>
            {
                self.handle_cli_pull(request).await
            }
            ("POST", path) if app_setting_id(path, "schema/push/plan").is_some() => {
                self.plan_schema_push(request, false).await
            }
            ("POST", path) if app_setting_id(path, "schema/push/apply").is_some() => {
                self.plan_schema_push(request, true).await
            }
            ("POST", path) if app_setting_id(path, "schema/steps/apply").is_some() => {
                self.apply_schema_steps(request).await
            }
            ("PUT", path) if app_setting_id(path, "storage/upload").is_some() => {
                self.upload_dashboard_file(request).await
            }
            ("POST", path) if app_setting_id(path, "storage/files/delete").is_some() => {
                self.delete_dashboard_files(request).await
            }
            ("GET", path) if indexing_job_ids(path).is_some() => {
                self.get_indexing_job(request).await
            }
            ("GET", path) if indexing_job_group_ids(path).is_some() => {
                self.get_indexing_job_group(request).await
            }
            ("POST", path) if app_setting_id(path, "indexing-jobs").is_some() => {
                self.create_indexing_job(request).await
            }
            ("GET" | "PUT", path) if app_setting_id(path, "backup-schedule").is_some() => {
                self.backup_schedule(request).await
            }
            ("GET", path) if app_setting_id(path, "backups").is_some() => {
                self.list_backups(request).await
            }
            ("POST", path) if app_setting_id(path, "backups").is_some() => {
                self.create_backup_job(request).await
            }
            ("GET", path) if app_setting_id(path, "backup-jobs").is_some() => {
                self.list_backup_jobs(request).await
            }
            ("GET", path) if backup_job_ids(path).is_some() => self.get_backup_job(request).await,
            ("DELETE", path) if backup_job_ids(path).is_some() => {
                self.cancel_backup_job(request).await
            }
            ("GET", path) if backup_action_ids(path, "files").is_some() => {
                self.list_backup_files(request).await
            }
            ("GET", path) if backup_action_ids(path, "file-url").is_some() => {
                self.get_backup_file_url(request).await
            }
            ("GET", path) if backup_action_ids(path, "storage-files").is_some() => {
                self.stream_backup_storage_files(request).await
            }
            ("DELETE", path) if backup_ids(path).is_some() => self.delete_backup(request).await,
            ("GET", path) if app_setting_id(path, "webhooks").is_some() => {
                self.list_webhooks(request).await
            }
            ("POST", path) if app_setting_id(path, "webhooks").is_some() => {
                self.create_webhook(request).await
            }
            ("POST", path) if webhook_action_ids(path, "enable").is_some() => {
                self.set_webhook_enabled(request, true).await
            }
            ("POST", path) if webhook_action_ids(path, "disable").is_some() => {
                self.set_webhook_enabled(request, false).await
            }
            ("GET", path) if webhook_action_ids(path, "events").is_some() => {
                self.list_webhook_events(request).await
            }
            ("GET", path) if webhook_event_ids(path).is_some() => {
                self.get_webhook_event(request).await
            }
            ("POST", path) if webhook_event_ids(path).is_some() => {
                self.resend_webhook_event(request).await
            }
            ("POST", path) if webhook_ids(path).is_some() => self.update_webhook(request).await,
            ("DELETE", path) if webhook_ids(path).is_some() => self.delete_webhook(request).await,
            ("POST", path) if transfer_ids(path).is_some() => self.transfer_app(request).await,
            ("POST", path) if claim_app_id(path).is_some() => self.claim_app(request).await,
            ("POST", path) if app_setting_id(path, "authorized_redirect_origins").is_some() => {
                self.create_authorized_origin(request).await
            }
            ("DELETE", path) if authorized_origin_ids(path).is_some() => {
                self.delete_authorized_origin(request).await
            }
            ("POST", path) if app_setting_id(path, "oauth_service_providers").is_some() => {
                self.create_oauth_provider(request).await
            }
            ("GET", path) if app_setting_id(path, "auth").is_some() => {
                self.get_app_auth(request).await
            }
            ("POST", path) if app_setting_id(path, "oauth_clients").is_some() => {
                self.create_oauth_client(request).await
            }
            ("POST", path) if oauth_client_ids(path).is_some() => {
                self.update_oauth_client(request).await
            }
            ("DELETE", path) if oauth_client_ids(path).is_some() => {
                self.delete_oauth_client(request).await
            }
            ("GET" | "POST" | "DELETE", path) if app_setting_id(path, "test_users").is_some() => {
                self.handle_test_users(request).await
            }
            ("GET", "/dash/default-email-template") => self.get_default_email_template().await,
            ("GET", path) if app_setting_id(path, "sender-verification").is_some() => {
                self.get_sender_verification(request).await
            }
            ("POST", path)
                if app_setting_id(path, "sender-verification/send-magic-code").is_some() =>
            {
                self.send_sender_verification_code(request).await
            }
            ("POST", path)
                if app_setting_id(path, "sender-verification/verify-magic-code").is_some() =>
            {
                self.verify_sender_code(request).await
            }
            ("GET", path) if app_setting_id(path, "email_status").is_some() => {
                self.get_email_status(request).await
            }
            ("POST", path) if app_setting_id(path, "email_templates").is_some() => {
                self.put_email_template(request).await
            }
            ("DELETE", path) if email_template_ids(path).is_some() => {
                self.delete_email_template(request).await
            }
            ("POST", path) if app_setting_id(path, "send-test-email").is_some() => {
                self.send_test_email(request).await
            }
            ("GET", path) if app_setting_id(path, "oauth-apps").is_some() => {
                self.list_oauth_apps(request).await
            }
            ("POST", path) if app_setting_id(path, "oauth-apps").is_some() => {
                self.create_oauth_app(request).await
            }
            ("POST", path) if oauth_app_ids(path).is_some() => self.update_oauth_app(request).await,
            ("DELETE", path) if oauth_app_ids(path).is_some() => {
                self.delete_oauth_app(request).await
            }
            ("POST", path) if oauth_app_client_collection_ids(path).is_some() => {
                self.create_oauth_app_client(request).await
            }
            ("POST", path) if oauth_app_client_ids(path).is_some() => {
                self.update_oauth_app_client(request).await
            }
            ("DELETE", path) if oauth_app_client_ids(path).is_some() => {
                self.delete_oauth_app_client(request).await
            }
            ("POST", path) if oauth_app_client_secret_collection_ids(path).is_some() => {
                self.create_oauth_app_client_secret(request).await
            }
            ("DELETE", path) if oauth_app_client_secret_ids(path).is_some() => {
                self.delete_oauth_app_client_secret(request).await
            }
            ("GET", "/dash/user/oauth_apps") => self.list_user_oauth_apps(request).await,
            ("POST", "/dash/user/oauth_apps/revoke_access") => {
                self.revoke_user_oauth_app(request).await
            }
            ("POST", path) if team_target(path, "invite/send").is_some() => {
                self.send_invite(request).await
            }
            ("DELETE", path) if team_target(path, "invite/revoke").is_some() => {
                self.revoke_invite(request).await
            }
            ("DELETE", path) if team_target(path, "members/remove").is_some() => {
                self.remove_member(request).await
            }
            ("POST", path) if team_target(path, "members/update").is_some() => {
                self.update_member(request).await
            }
            ("POST", "/dash/invites/accept") => self.accept_invite(request).await,
            ("POST", "/dash/invites/decline") => self.decline_invite(request).await,
            ("POST", path) if action_org_id(path, "rename").is_some() => {
                self.rename_org(request).await
            }
            ("GET", path) if direct_app_id(path).is_some() => self.get_app(request).await,
            ("DELETE", path) if direct_app_id(path).is_some() => self.delete_app(request).await,
            ("GET", path) if direct_org_id(path).is_some() => self.get_org(request).await,
            ("DELETE", path) if direct_org_id(path).is_some() => self.delete_org(request).await,
            ("GET", "/dash/session_counts") => self.get_session_counts().await,
            _ => Err(ApiError::new(
                ApiErrorType::Unknown,
                "Dashboard control route classifier mismatch",
            )),
        }
    }
}

impl<H> DashboardControlService<H>
where
    H: DashboardControlHooks,
{
    async fn initialize_created_app(&self, app_id: Uuid, body: &Value) -> Result<(), ApiError> {
        let schema = body.get("schema");
        let rules = body.get("rules").and_then(|rules| rules.get("code"));
        if schema.is_none() && rules.is_none() {
            return Ok(());
        }

        let mut client = self.acquire_client().await?;
        let mut jobs_to_enqueue = Vec::new();
        if let Some(schema) = schema {
            let attributes = load_dashboard_attributes(&*client, app_id).await?;
            let plan = schema_push_plan(&attributes, schema, true, false)?;
            let steps = plan
                .get("steps")
                .and_then(Value::as_array)
                .expect("schema plans always contain steps");
            let (_, jobs) =
                apply_schema_plan(&mut client, app_id, &attributes, steps, false).await?;
            jobs_to_enqueue = jobs;
        }
        if let Some(code) = rules {
            if let Err(issues) = RuleSet::try_new(code.clone()) {
                let errors = issues
                    .into_iter()
                    .map(|issue| json!({"message": issue.message, "path": issue.path}))
                    .collect::<Vec<_>>();
                return Err(validation("Rule validation failed").with_hint(json!({
                    "record-type": "rule",
                    "errors": errors
                })));
            }
            client
                .execute(
                    "INSERT INTO rules (app_id, code) VALUES ($1, $2)
                     ON CONFLICT (app_id) DO UPDATE
                     SET code = excluded.code, version = rules.version + 1
                     WHERE rules.code IS DISTINCT FROM excluded.code",
                    &[&app_id, &tokio_postgres::types::Json(code)],
                )
                .await
                .map_err(database_error)?;
        }
        drop(client);

        for job_id in jobs_to_enqueue {
            self.hooks
                .execute(DashboardControlHookOperation::EnqueueIndexingJob { job_id })
                .await?;
        }
        Ok(())
    }

    async fn authenticate_dashboard_admin(
        &self,
        request: &DashboardRequest,
    ) -> Result<(Uuid, String), ApiError> {
        let bearer = bearer(request)?;
        let client = self.acquire_client().await?;
        let principal = authenticate_refresh(&*client, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        let email = client
            .query_one("SELECT email FROM instant_users WHERE id = $1", &[&user_id])
            .await
            .map_err(database_error)?
            .get::<_, String>(0);
        drop(client);
        check_dashboard_email(
            self.dashboard_allowed_emails.as_deref(),
            self.superuser_email.as_deref(),
            &email,
        )?;
        let authorized = self
            .hooks
            .execute(DashboardControlHookOperation::AuthorizeDashboardAdmin {
                email: email.clone(),
            })
            .await?
            .as_bool()
            .unwrap_or(false);
        if !authorized {
            return Err(permission("Dashboard administrator access required"));
        }
        Ok((user_id, email))
    }

    async fn dashboard_admin_operation(
        &self,
        request: &DashboardRequest,
        operation: DashboardControlHookOperation,
    ) -> Result<Value, ApiError> {
        self.authenticate_dashboard_admin(request).await?;
        self.hooks.execute(operation).await
    }

    async fn get_dashboard_admin(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let users = self
            .dashboard_admin_operation(
                &request,
                DashboardControlHookOperation::DashboardAdminRecentUsers,
            )
            .await?;
        require_array(&users, "Dashboard recent users are unavailable")?;
        json_response(json!({"users": users}))
    }

    async fn check_dashboard_admin(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.authenticate_dashboard_admin(&request).await?;
        json_response(json!({"ok": true}))
    }

    async fn authorize_operator(&self, request: &DashboardRequest) -> Result<(), ApiError> {
        self.authenticate_dashboard_admin(request)
            .await
            .map(|_| ())
            .map_err(|error| {
                if error.body().kind == ApiErrorType::PermissionDenied {
                    ApiError::new(ApiErrorType::PermissionDenied, "Superuser access required")
                        .with_status(StatusCode::FORBIDDEN)
                } else {
                    error
                }
            })
    }

    async fn get_operator_flags(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.authorize_operator(&request).await?;
        let client = self.acquire_client().await?;
        let flags = list_config_flags(&client)
            .await
            .map_err(config_flag_error)?;
        drop(client);
        let known = KNOWN_CONFIG_FLAGS
            .iter()
            .map(|flag| {
                json!({
                    "name": flag.name,
                    "type": flag.value_type,
                    "description": flag.description,
                })
            })
            .collect::<Vec<_>>();
        json_response(json!({
            "flags": Value::Object(flags.into_iter().collect()),
            "known": known,
        }))
    }

    async fn put_operator_flag(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let name = operator_flag_name(&request.path)
            .ok_or_else(|| malformed("Malformed flag name"))?
            .to_owned();
        self.authorize_operator(&request).await?;
        let body = json_body(&request.body)?;
        let value = body.get("value").ok_or_else(|| missing("value"))?.clone();
        let mut client = self.acquire_client().await?;
        set_config_flag(&mut client, &name, value.clone())
            .await
            .map_err(config_flag_error)?;
        json_response(json!({"name": name, "value": value}))
    }

    async fn delete_operator_flag(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let name = operator_flag_name(&request.path)
            .ok_or_else(|| malformed("Malformed flag name"))?
            .to_owned();
        self.authorize_operator(&request).await?;
        let mut client = self.acquire_client().await?;
        let deleted = unset_config_flag(&mut client, &name)
            .await
            .map_err(config_flag_error)?;
        json_response(json!({"name": name, "deleted": deleted}))
    }

    async fn get_dashboard_top_users(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let days = query_parameter(request.query.as_deref(), "n")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(7);
        let users = self
            .dashboard_admin_operation(
                &request,
                DashboardControlHookOperation::DashboardAdminTopUsers { days },
            )
            .await?;
        require_array(&users, "Dashboard top users are unavailable")?;
        json_response(json!({"users": users}))
    }

    async fn get_dashboard_storage(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let apps = self
            .dashboard_admin_operation(
                &request,
                DashboardControlHookOperation::DashboardAdminStorageMetrics,
            )
            .await?;
        require_array(&apps, "Dashboard storage metrics are unavailable")?;
        json_response(json!({"apps": apps}))
    }

    async fn get_dashboard_overview_daily(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let overview = self
            .dashboard_admin_operation(
                &request,
                DashboardControlHookOperation::DashboardAdminOverviewDaily,
            )
            .await?;
        require_object(&overview, "Dashboard daily overview is unavailable")?;
        json_response(overview)
    }

    async fn get_dashboard_overview_minute(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let overview = self
            .dashboard_admin_operation(
                &request,
                DashboardControlHookOperation::DashboardAdminOverviewMinute,
            )
            .await?;
        require_object(&overview, "Dashboard minute overview is unavailable")?;
        json_response(overview)
    }

    async fn dashboard_oauth_config(
        &self,
        redirect_to_dev: bool,
    ) -> Result<DashboardOAuthConfig, ApiError> {
        let value = self
            .hooks
            .execute(DashboardControlHookOperation::DashboardOAuthConfiguration { redirect_to_dev })
            .await?;
        let string = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    ApiError::new(
                        ApiErrorType::Unknown,
                        "Dashboard OAuth configuration is unavailable",
                    )
                })
        };
        Ok(DashboardOAuthConfig {
            authorization_endpoint: string("authorization_endpoint")?,
            client_id: string("client_id")?,
            callback_url: string("callback_url")?,
            dashboard_origin: string("dashboard_origin")?,
            dev_dashboard_origin: string("dev_dashboard_origin")?,
            secure_cookie: value
                .get("secure_cookie")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    }

    async fn start_dashboard_oauth(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let redirect_to_dev = query_parameter(request.query.as_deref(), "redirect_to_dev")
            .is_some_and(|value| value == "true");
        let config = self.dashboard_oauth_config(redirect_to_dev).await?;
        let redirect_path = coerce_dashboard_redirect_path(query_parameter(
            request.query.as_deref(),
            "redirect_path",
        ));
        let ticket = optional_query_uuid(request.query.as_deref(), "ticket")?;
        let state = Uuid::new_v4();
        let cookie = Uuid::new_v4();
        let state_string = state.to_string();
        let mut authorization_url = url::Url::parse(&config.authorization_endpoint)
            .map_err(|_| malformed("Invalid dashboard OAuth authorization endpoint"))?;
        authorization_url.query_pairs_mut().extend_pairs([
            ("scope", "email"),
            ("response_type", "code"),
            ("state", state_string.as_str()),
            ("redirect_uri", config.callback_url.as_str()),
            ("client_id", config.client_id.as_str()),
        ]);
        let client = self.acquire_client().await?;
        client
            .execute(
                "INSERT INTO instant_oauth_redirects
                   (lookup_key, state, cookie, service, redirect_path, redirect_to_dev, ticket)
                 VALUES ($1, $2, $3, 'google', $4, $5, $6)",
                &[
                    &hash_dashboard_oauth_uuid(state),
                    &state,
                    &cookie,
                    &redirect_path,
                    &redirect_to_dev,
                    &ticket,
                ],
            )
            .await
            .map_err(database_error)?;
        dashboard_redirect_response(
            authorization_url.as_str(),
            Some((cookie, config.secure_cookie)),
        )
    }

    #[allow(clippy::too_many_lines)]
    async fn callback_dashboard_oauth(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let provider_error = query_parameter(request.query.as_deref(), "error");
        let state_param = query_parameter(request.query.as_deref(), "state");
        let state = state_param
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok());
        let cookie_param = dashboard_oauth_cookie(&request);
        let code = query_parameter(request.query.as_deref(), "code");

        let client = self.acquire_client().await?;
        let oauth_redirect = if let (Some(state), Some(_)) = (state, cookie_param) {
            client
                .query_opt(
                    "DELETE FROM instant_oauth_redirects WHERE lookup_key = $1
                     RETURNING cookie, redirect_path, redirect_to_dev, ticket, created_at",
                    &[&hash_dashboard_oauth_uuid(state)],
                )
                .await
                .map_err(database_error)?
        } else {
            None
        };
        drop(client);

        let redirect_to_dev = oauth_redirect
            .as_ref()
            .and_then(|row| row.get::<_, Option<bool>>(2))
            .unwrap_or(false);
        let ticket = oauth_redirect
            .as_ref()
            .and_then(|row| row.get::<_, Option<Uuid>>(3));
        let config = self.dashboard_oauth_config(redirect_to_dev).await?;
        let mut error = provider_error.map(|value| format!("Error from Google: {value}"));
        if error.is_none() && state_param.is_none() {
            error = Some("Missing state param in OAuth redirect.".to_owned());
        } else if error.is_none() && cookie_param.is_none() {
            error = Some("Missing cookie.".to_owned());
        } else if error.is_none() && state.is_none() {
            error = Some("Invalid state param in OAuth redirect.".to_owned());
        } else if error.is_none() && code.is_none() {
            error = Some("Missing code param in OAuth redirect.".to_owned());
        } else if error.is_none() && oauth_redirect.is_none() {
            error = Some("Could not find OAuth request.".to_owned());
        }

        if error.is_none()
            && let (Some(cookie), Some(row)) = (cookie_param, oauth_redirect.as_ref())
        {
            let expected = row.get::<_, Uuid>(0).to_string();
            if !instant_auth::constant_time_code_eq(&cookie.to_string(), &expected) {
                error = Some("Mismatch in OAuth request cookie.".to_owned());
            } else if Utc::now()
                .signed_duration_since(row.get::<_, DateTime<Utc>>(4))
                .num_minutes()
                > 10
            {
                error = Some("Request is expired.".to_owned());
            }
        }

        let mut claims = None;
        if error.is_none() {
            let exchange = self
                .hooks
                .execute(DashboardControlHookOperation::DashboardOAuthExchangeCode {
                    code: code.clone().expect("validated OAuth code"),
                    redirect_uri: config.callback_url.clone(),
                })
                .await?;
            if let Some(message) = exchange.get("error").and_then(Value::as_str) {
                error = Some(format!("Error fetching user data from Google: {message}."));
            } else {
                claims = Some(exchange);
            }
        }

        let (email, google_sub) = if let Some(claims) = claims.as_ref() {
            if !claims
                .get("email_verified")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                error = Some("Could not verify email.".to_owned());
            }
            let email = claims
                .get("email")
                .and_then(Value::as_str)
                .map(|email| json!({"email": email}))
                .and_then(|body| email_value(&body, "email").ok());
            let subject = claims
                .get("subject")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            if error.is_none() && email.is_none() {
                error = Some("Could not determine email.".to_owned());
            }
            if error.is_none() && subject.is_none() {
                error = Some("Could not determine user info.".to_owned());
            }
            (email, subject)
        } else {
            (None, None)
        };

        let mut dashboard_code = None;
        if error.is_none() {
            let email = email.expect("validated dashboard OAuth email");
            let google_sub = google_sub.expect("validated dashboard OAuth subject");
            if !self.dashboard_email_allowed(&email) {
                return dashboard_oauth_callback_response(
                    &config,
                    Some("This email is not allowed to sign in to this dashboard."),
                    None,
                    ticket,
                    redirect_to_dev,
                );
            }
            let client = self.acquire_client().await?;
            let users = client
                .query(
                    "SELECT id, email, google_sub FROM instant_users
                     WHERE email = $1 OR google_sub = $2",
                    &[&email, &google_sub],
                )
                .await
                .map_err(database_error)?;
            drop(client);
            if users.len() > 1 {
                error = Some("Could not create or update user.".to_owned());
            } else {
                let user_id = if let Some(user) = users.first() {
                    let user_id = user.get::<_, Uuid>(0);
                    let current_email = user.get::<_, String>(1);
                    let current_sub = user.get::<_, Option<String>>(2);
                    let client = self.acquire_client().await?;
                    if current_email != email {
                        client
                            .execute(
                                "UPDATE instant_users SET email = $1 WHERE id = $2",
                                &[&email, &user_id],
                            )
                            .await
                            .map_err(database_error)?;
                    } else if current_sub.as_deref() != Some(google_sub.as_str()) {
                        client
                            .execute(
                                "UPDATE instant_users SET google_sub = $1 WHERE id = $2",
                                &[&google_sub, &user_id],
                            )
                            .await
                            .map_err(database_error)?;
                    }
                    user_id
                } else {
                    let allowed = self
                        .hooks
                        .execute(
                            DashboardControlHookOperation::AuthorizeDashboardOAuthSignup {
                                email: email.clone(),
                            },
                        )
                        .await?
                        .as_bool()
                        .unwrap_or(false);
                    if !allowed {
                        return dashboard_oauth_callback_response(
                            &config,
                            Some("Signups are not allowed for this email."),
                            None,
                            ticket,
                            redirect_to_dev,
                        );
                    }
                    let user_id = Uuid::new_v4();
                    let client = self.acquire_client().await?;
                    client
                        .execute(
                            "INSERT INTO instant_users (id, email, google_sub)
                             VALUES ($1, $2, $3)",
                            &[&user_id, &email, &google_sub],
                        )
                        .await
                        .map_err(database_error)?;
                    user_id
                };
                let generated_code = Uuid::new_v4();
                let redirect_path = oauth_redirect
                    .as_ref()
                    .expect("validated dashboard OAuth redirect")
                    .get::<_, String>(1);
                let client = self.acquire_client().await?;
                client
                    .execute(
                        "INSERT INTO instant_oauth_codes (lookup_key, user_id, redirect_path)
                         VALUES ($1, $2, $3)",
                        &[
                            &hash_dashboard_oauth_uuid(generated_code),
                            &user_id,
                            &redirect_path,
                        ],
                    )
                    .await
                    .map_err(database_error)?;
                dashboard_code = Some(generated_code);
            }
        }

        dashboard_oauth_callback_response(
            &config,
            error.as_deref(),
            dashboard_code,
            ticket,
            redirect_to_dev,
        )
    }

    async fn exchange_dashboard_oauth_code(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let body = json_body(&request.body)?;
        let code = uuid_value(&body, "code")?;
        let client = self.acquire_client().await?;
        let oauth_code = client
            .query_opt(
                "DELETE FROM instant_oauth_codes WHERE lookup_key = $1
                 RETURNING user_id, redirect_path, created_at",
                &[&hash_dashboard_oauth_uuid(code)],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("instant-oauth-code"))?;
        if Utc::now()
            .signed_duration_since(oauth_code.get::<_, DateTime<Utc>>(2))
            .num_minutes()
            > 5
        {
            return Err(ApiError::new(
                ApiErrorType::RecordExpired,
                "Record expired: instant-oauth-code",
            ));
        }
        let user_id = oauth_code.get::<_, Uuid>(0);
        let redirect_path = oauth_code.get::<_, String>(1);
        let refresh_token = Uuid::new_v4();
        client
            .execute(
                INSERT_REFRESH_TOKEN_SQL,
                &[&refresh_token, &hash_token(refresh_token), &user_id],
            )
            .await
            .map_err(database_error)?;
        let user = client
            .query_one(
                "SELECT id, email, created_at FROM instant_users WHERE id = $1",
                &[&user_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({
            "token": refresh_token,
            "redirect_path": redirect_path,
            "user": {
                "id": user.get::<_, Uuid>(0),
                "email": user.get::<_, String>(1),
                "created_at": user.get::<_, NaiveDateTime>(2),
            }
        }))
    }

    async fn create_restore_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (user_id, _) = self.authenticate_dashboard_admin(&request).await?;
        let app_id =
            optional_query_uuid(request.query.as_deref(), "app_id")?.unwrap_or_else(Uuid::new_v4);
        if is_reserved_app_id(app_id) {
            return Err(validation(
                "This app ID is reserved for deployment configuration",
            ));
        }
        let org_id = optional_query_uuid(request.query.as_deref(), "org_id")?;
        let title = query_parameter(request.query.as_deref(), "title")
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if query_parameter(request.query.as_deref(), "title").is_some() && title.is_none() {
            return Err(malformed("Restore title must not be blank"));
        }
        let creator_id = org_id.is_none().then_some(user_id);
        let client = self.acquire_client().await?;
        if let Some(org_id) = org_id
            && client
                .query_opt("SELECT id FROM orgs WHERE id = $1", &[&org_id])
                .await
                .map_err(database_error)?
                .is_none()
        {
            return Err(record_not_found("org"));
        }
        if client
            .query_opt("SELECT id FROM apps WHERE id = $1", &[&app_id])
            .await
            .map_err(database_error)?
            .is_some()
        {
            return Err(validation(
                "An app already exists with this id, so the restore cannot create it.",
            ));
        }
        drop(client);

        let job_id = Uuid::new_v4();
        let zip_path = self
            .hooks
            .execute(DashboardControlHookOperation::PersistRestoreZip {
                job_id,
                app_id,
                body: request.body.to_vec(),
                staged_path: request
                    .staged_body_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            })
            .await?
            .as_str()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorType::Unknown,
                    "Restore ZIP persistence is unavailable",
                )
            })?;

        let client = self.acquire_client().await?;
        let inserted = client
            .query_one(
                "INSERT INTO app_restore_jobs
                   (id, app_id, creator_id, org_id, title, zip_path, job_status)
                 VALUES ($1, $2, $3, $4, $5, $6, 'waiting')
                 RETURNING id, app_id, title, job_status, progress, error,
                           created_at, done_at, updated_at",
                &[&job_id, &app_id, &creator_id, &org_id, &title, &zip_path],
            )
            .await;
        drop(client);
        let row = match inserted {
            Ok(row) => row,
            Err(error) => {
                let _ = self
                    .hooks
                    .execute(DashboardControlHookOperation::DiscardRestoreZip {
                        path: zip_path.clone(),
                    })
                    .await;
                return Err(database_error(error));
            }
        };
        let job = restore_job_json(&row);
        self.hooks
            .execute(DashboardControlHookOperation::EnqueueRestoreJob { job_id })
            .await?;
        json_response(json!({"restore-job": job}))
    }

    async fn list_restore_jobs(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.authenticate_dashboard_admin(&request).await?;
        let client = self.acquire_client().await?;
        let jobs = client
            .query(
                &format!("{} ORDER BY created_at DESC LIMIT 20", restore_job_select()),
                &[],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(restore_job_json)
            .collect::<Vec<_>>();
        json_response(json!({"restore-jobs": jobs}))
    }

    async fn get_restore_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.authenticate_dashboard_admin(&request).await?;
        let job_id =
            restore_job_id(&request.path).ok_or_else(|| malformed("Malformed restore job ID"))?;
        let client = self.acquire_client().await?;
        let job = client
            .query_opt(
                &format!("{} WHERE id = $1", restore_job_select()),
                &[&job_id],
            )
            .await
            .map_err(database_error)?
            .as_ref()
            .map(restore_job_json);
        json_response(json!({"restore-job": job}))
    }

    async fn cancel_restore_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.authenticate_dashboard_admin(&request).await?;
        let job_id =
            restore_job_id(&request.path).ok_or_else(|| malformed("Malformed restore job ID"))?;
        let client = self.acquire_client().await?;
        let job = client
            .query_opt(
                "UPDATE app_restore_jobs
                 SET job_status = 'cancelled', done_at = now()
                 WHERE id = $1 AND job_status IN ('waiting', 'processing')
                 RETURNING id, app_id, title, job_status, progress, error,
                           created_at, done_at, updated_at",
                &[&job_id],
            )
            .await
            .map_err(database_error)?
            .as_ref()
            .map(restore_job_json);
        json_response(json!({"restore-job": job}))
    }

    async fn create_backup_restore_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, backup_id) = backup_action_ids(&request.path, "restore")
            .ok_or_else(|| malformed("Malformed backup ID"))?;
        let body = if request.body.is_empty() {
            json!({})
        } else {
            json_body(&request.body)?
        };
        let requested_title = optional_non_blank_string(&body, "title")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Owner, true, false).await?;
        let (creator_id, org_id, source_title, storage_prefix) =
            backup_restore_source(&*client, app_id, backup_id).await?;
        let title = requested_title.unwrap_or_else(|| {
            format!(
                "{source_title} (restored {})",
                Utc::now().format("%Y-%m-%d")
            )
        });
        let target_app_id = Uuid::new_v4();
        if is_reserved_app_id(target_app_id) {
            return Err(validation(
                "This app ID is reserved for deployment configuration",
            ));
        }
        drop(client);

        let job_id = Uuid::new_v4();
        let zip_path = self
            .hooks
            .execute(DashboardControlHookOperation::StageBackupRestoreZip {
                job_id,
                storage_prefix,
            })
            .await?
            .as_str()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::new(ApiErrorType::Unknown, "Restore ZIP staging is unavailable")
            })?;

        let client = self.acquire_client().await?;
        let inserted = client
            .query_one(
                "INSERT INTO app_restore_jobs
                   (id, app_id, source_app_id, creator_id, org_id, title, zip_path, job_status)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'waiting')
                 RETURNING id, app_id, title, job_status, progress, error,
                           created_at, done_at, updated_at",
                &[
                    &job_id,
                    &target_app_id,
                    &app_id,
                    &creator_id,
                    &org_id,
                    &title,
                    &zip_path,
                ],
            )
            .await;
        drop(client);
        let row = match inserted {
            Ok(row) => row,
            Err(error) => {
                let _ = self
                    .hooks
                    .execute(DashboardControlHookOperation::DiscardRestoreZip {
                        path: zip_path.clone(),
                    })
                    .await;
                return Err(database_error(error));
            }
        };
        let job = restore_job_json(&row);
        self.hooks
            .execute(DashboardControlHookOperation::EnqueueRestoreJob { job_id })
            .await?;
        json_response(json!({
            "app": {"id": target_app_id, "title": title},
            "job": {"id": job["id"]},
        }))
    }

    async fn list_app_restore_jobs(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "restore-jobs")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, false).await?;
        let jobs = client
            .query(
                &format!(
                    "{} WHERE app_id = $1 OR source_app_id = $1
                     ORDER BY created_at DESC LIMIT 50",
                    restore_job_select()
                ),
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(restore_job_json)
            .collect::<Vec<_>>();
        json_response(json!({"restore-jobs": jobs}))
    }

    async fn track_app_import(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        action_app_id(&request.path, "track-import")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        json_response(json!({"ok": true}))
    }

    async fn get_active_session_stats(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.authenticate_dashboard_admin(&request).await?;
        let stats = self
            .hooks
            .execute(DashboardControlHookOperation::ActiveSessionStats)
            .await?;
        if stats.get("total-count").and_then(Value::as_u64).is_none()
            || stats.get("total-queries").and_then(Value::as_u64).is_none()
        {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "Active session statistics are unavailable",
            ));
        }
        json_response(stats)
    }

    async fn get_session_counts(&self) -> Result<DashboardResponse, ApiError> {
        let counts = self
            .hooks
            .execute(DashboardControlHookOperation::SessionCounts)
            .await?;
        json_response(counts)
    }

    async fn get_app_session_stats(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "stats").ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        drop(client);
        let stats = self
            .hooks
            .execute(DashboardControlHookOperation::AppSessionStats { app_id })
            .await?;
        if stats.get("count").and_then(Value::as_i64).is_none()
            || !stats.get("origins").is_some_and(Value::is_object)
        {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "Session statistics are unavailable",
            ));
        }
        json_response(stats)
    }

    /// Usage totals for one app: stored bytes, file storage and write volume.
    async fn get_app_usage(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "usage").ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let totals = client
            .query_one(
                "SELECT coalesce(sum(pg_size), 0)::bigint,
                        coalesce(sum(files_size), 0)::bigint
                   FROM triples_size_aggregate
                  WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(database_error)?;
        let file_count = match system_attr_id("$files", "size") {
            Ok(attr_id) => client
                .query_one(
                    "SELECT sum(total)::bigint
                       FROM attr_sketches
                      WHERE app_id = $1 AND attr_id = $2",
                    &[&app_id, &attr_id],
                )
                .await
                .map_err(database_error)?
                .get::<_, Option<i64>>(0),
            Err(_) => None,
        };
        let write_counts = client
            .query_one(
                "SELECT count(*) FILTER (WHERE created_at >= now() - interval '24 hours'),
                        count(*) FILTER (WHERE created_at >= now() - interval '7 days')
                   FROM transactions
                  WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(database_error)?;
        let daily = client
            .query(
                "SELECT to_char(series.day, 'YYYY-MM-DD') AS day,
                        coalesce(sum(daily.count), 0)::bigint AS count
                   FROM generate_series(current_date - 13, current_date, interval '1 day')
                        AS series(day)
                   LEFT JOIN daily_app_transactions daily
                     ON daily.app_id = $1 AND daily.date = series.day::date
                  GROUP BY series.day
                  ORDER BY series.day",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(|row| {
                json!({
                    "date": row.get::<_, String>(0),
                    "count": row.get::<_, i64>(1),
                })
            })
            .collect::<Vec<_>>();
        drop(client);
        let live_sessions = self.live_session_count(app_id).await?;
        json_response(json!({
            "appBytes": totals.get::<_, i64>(0),
            "storageBytes": totals.get::<_, i64>(1),
            "fileCount": file_count,
            "liveSessions": live_sessions,
            "transactions": {
                "last24h": write_counts.get::<_, i64>(0),
                "last7d": write_counts.get::<_, i64>(1),
                "daily": daily,
            },
        }))
    }

    /// Usage summaries for every app the caller can see in one workspace.
    /// Each metric is a single grouped query over the workspace's app IDs.
    #[allow(clippy::too_many_lines)] // Workspace selection and the metric joins read best in one place.
    async fn get_apps_summary(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let bearer = bearer(&request)?;
        let workspace = query_parameter(request.query.as_deref(), "workspace")
            .ok_or_else(|| validation("workspace must be personal or an organization ID"))?;
        let workspace = DashboardWorkspace::parse(&workspace)?;
        let client = self.acquire_client().await?;
        let principal = authenticate_refresh(&*client, &bearer).await?;
        let user_id = principal
            .user_id
            .ok_or_else(|| permission("User token required"))?;
        let app_ids = match workspace {
            DashboardWorkspace::Personal => client
                .query(
                    "SELECT app.id
                       FROM apps app
                       LEFT JOIN app_members member
                         ON member.app_id = app.id AND member.user_id = $1
                      WHERE app.org_id IS NULL AND app.deletion_marked_at IS NULL
                        AND (app.creator_id = $1 OR member.user_id IS NOT NULL)
                      ORDER BY app.created_at, app.id",
                    &[&user_id],
                )
                .await
                .map_err(database_error)?
                .iter()
                .map(|row| row.get::<_, Uuid>(0))
                .collect::<Vec<_>>(),
            DashboardWorkspace::Org(org_id) => {
                require_org_or_app_member(&*client, user_id, org_id).await?;
                client
                    .query(
                        "SELECT app.id
                           FROM apps app
                          WHERE app.org_id = $2 AND app.deletion_marked_at IS NULL
                            AND (EXISTS (
                                   SELECT 1 FROM org_members
                                    WHERE org_members.org_id = $2
                                      AND org_members.user_id = $1
                                 )
                                 OR EXISTS (
                                   SELECT 1 FROM app_members
                                    WHERE app_members.app_id = app.id
                                      AND app_members.user_id = $1
                                 ))
                          ORDER BY app.created_at, app.id",
                        &[&user_id, &org_id],
                    )
                    .await
                    .map_err(database_error)?
                    .iter()
                    .map(|row| row.get::<_, Uuid>(0))
                    .collect::<Vec<_>>()
            }
        };
        if app_ids.is_empty() {
            drop(client);
            return json_response(json!({"apps": []}));
        }
        let sizes = client
            .query(
                "SELECT app_id,
                        coalesce(sum(pg_size), 0)::bigint,
                        coalesce(sum(files_size), 0)::bigint
                   FROM triples_size_aggregate
                  WHERE app_id = ANY($1::uuid[])
                  GROUP BY app_id",
                &[&app_ids],
            )
            .await
            .map_err(database_error)?;
        let last_writes = client
            .query(
                "SELECT app_id, max(created_at)
                   FROM transactions
                  WHERE app_id = ANY($1::uuid[])
                  GROUP BY app_id",
                &[&app_ids],
            )
            .await
            .map_err(database_error)?;
        let last_backups = client
            .query(
                "SELECT app_id, max(backup_at)
                   FROM app_backups
                  WHERE app_id = ANY($1::uuid[])
                    AND deletion_marked_at IS NULL
                    AND expires_at > now()
                  GROUP BY app_id",
                &[&app_ids],
            )
            .await
            .map_err(database_error)?;
        drop(client);
        let sessions = self
            .hooks
            .execute(DashboardControlHookOperation::AppSessionCounts)
            .await?;
        let app_bytes = sizes
            .iter()
            .map(|row| (row.get::<_, Uuid>(0), row.get::<_, i64>(1)))
            .collect::<HashMap<_, _>>();
        let storage_bytes = sizes
            .iter()
            .map(|row| (row.get::<_, Uuid>(0), row.get::<_, i64>(2)))
            .collect::<HashMap<_, _>>();
        let last_write_at = last_writes
            .iter()
            .filter_map(|row| {
                row.get::<_, Option<NaiveDateTime>>(1)
                    .map(|value| (row.get::<_, Uuid>(0), value.and_utc()))
            })
            .collect::<HashMap<_, _>>();
        let last_backup_at = last_backups
            .iter()
            .filter_map(|row| {
                row.get::<_, Option<DateTime<Utc>>>(1)
                    .map(|value| (row.get::<_, Uuid>(0), value))
            })
            .collect::<HashMap<_, _>>();
        let live_sessions = sessions
            .as_object()
            .into_iter()
            .flat_map(serde_json::Map::iter)
            .filter_map(|(app_id, count)| {
                Some((Uuid::parse_str(app_id).ok()?, count.as_i64().unwrap_or(0)))
            })
            .collect::<HashMap<_, _>>();
        let apps = app_ids
            .iter()
            .map(|app_id| {
                json!({
                    "id": app_id,
                    "liveSessions": live_sessions.get(app_id).copied().unwrap_or(0),
                    "appBytes": app_bytes.get(app_id).copied().unwrap_or(0),
                    "storageBytes": storage_bytes.get(app_id).copied().unwrap_or(0),
                    "lastWriteAt": last_write_at.get(app_id),
                    "lastBackupAt": last_backup_at.get(app_id),
                })
            })
            .collect::<Vec<_>>();
        json_response(json!({"apps": apps}))
    }

    async fn live_session_count(&self, app_id: Uuid) -> Result<i64, ApiError> {
        let counts = self
            .hooks
            .execute(DashboardControlHookOperation::AppSessionCounts)
            .await?;
        Ok(counts
            .get(app_id.to_string())
            .and_then(Value::as_i64)
            .unwrap_or(0))
    }

    async fn get_default_email_template(&self) -> Result<DashboardResponse, ApiError> {
        let template = self
            .hooks
            .execute(DashboardControlHookOperation::DefaultEmailTemplate)
            .await?;
        for field in ["sender-email", "subject", "body"] {
            if template.get(field).and_then(Value::as_str).is_none() {
                return Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "Default email template is unavailable",
                ));
            }
        }
        json_response(json!({
            "email-type": "magic-code",
            "sender-email": template["sender-email"],
            "subject": template["subject"],
            "body": template["body"],
        }))
    }

    async fn email_verification_expiry_minutes(&self) -> Result<i32, ApiError> {
        let configuration = self
            .hooks
            .execute(DashboardControlHookOperation::DefaultEmailTemplate)
            .await?;
        configuration
            .get("verification-code-expiry-minutes")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorType::Unknown,
                    "Email verification configuration is unavailable",
                )
            })
    }

    async fn get_sender_verification(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "sender-verification")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(&*client, &bearer, app_id, Role::Admin, true, "apps-read").await?;
        let info = load_email_info(&*client, app_id).await?;
        let verified = info.as_ref().and_then(|row| row.get::<_, Option<bool>>(10));
        let postmark_id = info.as_ref().and_then(|row| row.get::<_, Option<i32>>(8));
        let sender_email = info
            .as_ref()
            .and_then(|row| row.get::<_, Option<String>>(7));
        let domain = match sender_email.as_deref().map(crate::postmark::email_domain) {
            Some(domain) if !domain.is_empty() => {
                load_sender_domain(&*client, app_id, &domain).await?
            }
            _ => Value::Null,
        };
        drop(client);
        let verification = if let Some(postmark_id) = postmark_id {
            let value = self
                .hooks
                .execute(DashboardControlHookOperation::GetEmailSender { postmark_id })
                .await?;
            if !value.is_object() && !value.is_null() {
                return Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "Email sender verification is unavailable",
                ));
            }
            value
        } else {
            Value::Null
        };
        json_response(json!({
            "instant": {"verified?": verified},
            "verification": verification,
            "domain": domain,
        }))
    }

    async fn get_email_status(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "email_status")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(&*client, &bearer, app_id, Role::Admin, true, "apps-read").await?;
        let info = load_email_info(&*client, app_id)
            .await?
            .as_ref()
            .map(email_info_json);
        json_response(json!({"info": info}))
    }

    #[allow(clippy::too_many_lines)] // Keeps sender resolution and persistence in protocol order.
    async fn put_email_template(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "email_templates")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let email_type = required_string(&body, "email-type")?.trim().to_owned();
        let subject = required_string(&body, "subject")?.trim().to_owned();
        let template_body = required_string(&body, "body")?.trim().to_owned();
        if !subject.contains("{code}") {
            return Err(validation(
                "Subject does not contain template variable: '{code}'",
            ));
        }
        if !template_body.contains("{code}") {
            return Err(validation(
                "Body does not contain template variable: '{code}'",
            ));
        }
        let sender_email = optional_email_value(&body, "sender-email")?;
        let custom_sender_name = optional_string(&body, "sender-name");
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_dashboard_user_scoped(&*client, &bearer, app_id, Role::Admin, "apps-write")
            .await?;
        let app = load_app(&*client, app_id).await?;
        let sender_name = custom_sender_name.unwrap_or_else(|| app.get(3));
        let existing_sender = load_email_info(&*client, app_id).await?.and_then(|row| {
            row.get::<_, Option<String>>(7).map(|email| {
                (
                    email,
                    row.get::<_, Option<String>>(5),
                    row.get::<_, Option<i32>>(8),
                )
            })
        });
        drop(client);
        let postmark_id = if let Some(email) = &sender_email {
            if let Some((_, _, Some(postmark_id))) =
                existing_sender
                    .as_ref()
                    .filter(|(existing_email, existing_name, postmark_id)| {
                        postmark_id.is_some()
                            && existing_email.eq_ignore_ascii_case(email)
                            && existing_name.as_deref() == Some(sender_name.as_str())
                    })
            {
                Some(*postmark_id)
            } else {
                let value = self
                    .hooks
                    .execute(DashboardControlHookOperation::SyncEmailSender {
                        app_id,
                        email: email.clone(),
                        name: sender_name.clone(),
                    })
                    .await?;
                // `null` means the provider registers domains itself (Cloudflare).
                value
                    .as_i64()
                    .map(|id| i32::try_from(id).map_err(|_| malformed("Malformed email sender ID")))
                    .transpose()?
            }
        } else {
            None
        };
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let sender_id = if let Some(email) = &sender_email {
            let sender_id = transaction
                .query_one(
                    "INSERT INTO app_email_senders
                       (id, postmark_id, email, name, email_provider)
                     VALUES ($1, $2, $3, $4,
                             CASE WHEN $2::integer IS NULL
                               THEN 'cloudflare' ELSE 'postmark' END)
                     ON CONFLICT (email) DO UPDATE
                     SET postmark_id = EXCLUDED.postmark_id, name = EXCLUDED.name,
                         email_provider = EXCLUDED.email_provider
                     RETURNING id",
                    &[&Uuid::new_v4(), &postmark_id, email, &sender_name],
                )
                .await
                .map_err(database_error)?
                .get::<_, Uuid>(0);
            transaction
                .execute(
                    "INSERT INTO app_email_verifications (id, app_id, sender_id, verified)
                     VALUES ($1, $2, $3, false)
                     ON CONFLICT (app_id, sender_id) DO UPDATE
                     SET sender_id = EXCLUDED.sender_id",
                    &[&Uuid::new_v4(), &app_id, &sender_id],
                )
                .await
                .map_err(database_error)?;
            // A changed sender must be re-verified before its address is used.
            let domain = crate::postmark::email_domain(email);
            let changed = existing_sender
                .as_ref()
                .is_none_or(|(existing, _, _)| !existing.eq_ignore_ascii_case(email));
            if postmark_id.is_none() && !domain.is_empty() && changed {
                transaction
                    .execute(
                        "INSERT INTO app_sender_domains (app_id, domain)
                         VALUES ($1, $2)
                         ON CONFLICT (app_id, domain) DO UPDATE SET
                           verified_at = NULL, ownership_verified_at = NULL,
                           last_checked_at = now()",
                        &[&app_id, &domain],
                    )
                    .await
                    .map_err(database_error)?;
            }
            Some(sender_id)
        } else {
            None
        };
        let template_id = transaction
            .query_one(
                "INSERT INTO app_email_templates
                   (id, app_id, sender_id, email_type, subject, name, body)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (app_id, email_type) DO UPDATE SET
                   sender_id = EXCLUDED.sender_id, subject = EXCLUDED.subject,
                   name = EXCLUDED.name, body = EXCLUDED.body
                 RETURNING id",
                &[
                    &Uuid::new_v4(),
                    &app_id,
                    &sender_id,
                    &email_type,
                    &subject,
                    &sender_name,
                    &template_body,
                ],
            )
            .await
            .map_err(database_error)?
            .get::<_, Uuid>(0);
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({"id": template_id}))
    }

    async fn delete_email_template(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, template_id) = email_template_ids(&request.path)
            .ok_or_else(|| malformed("Malformed email template ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_dashboard_user_scoped(&*client, &bearer, app_id, Role::Admin, "apps-write")
            .await?;
        client
            .execute(
                "DELETE FROM app_email_templates WHERE id = $1 AND app_id = $2",
                &[&template_id, &app_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({}))
    }

    async fn send_sender_verification_code(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "sender-verification/send-magic-code")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_dashboard_user_scoped(&*client, &bearer, app_id, Role::Admin, "apps-write")
            .await?;
        let app = load_app(&*client, app_id).await?;
        let app_title: String = app.get(3);
        let info = load_email_info(&*client, app_id)
            .await?
            .ok_or_else(|| record_not_found("app-email-verification"))?;
        let verification_id: Uuid = info
            .get::<_, Option<Uuid>>(9)
            .ok_or_else(|| record_not_found("app-email-verification"))?;
        let sender_email: String = info
            .get::<_, Option<String>>(7)
            .ok_or_else(|| record_not_found("app-email-sender"))?;
        drop(client);
        self.authorize_email_send("sender-verification", app_id, &sender_email)
            .await?;
        let code = generate_magic_code();
        let client = self.acquire_client().await?;
        client
            .execute(
                "DELETE FROM app_email_verification_codes
                 WHERE verification_id = $1 AND app_id = $2",
                &[&verification_id, &app_id],
            )
            .await
            .map_err(database_error)?;
        client
            .execute(
                "INSERT INTO app_email_verification_codes
                   (id, code, app_id, verification_id) VALUES ($1, $2, $3, $4)",
                &[&Uuid::new_v4(), &code, &app_id, &verification_id],
            )
            .await
            .map_err(database_error)?;
        drop(client);
        self.deliver_dashboard_email(
            "sender-verification",
            app_id,
            sender_email,
            None,
            app_title.clone(),
            format!("{code} is your Instant sender verification code for app: {app_title}"),
            format!("Use {code} to verify your custom sender for {app_title}."),
        )
        .await?;
        json_response(json!({"sent": true}))
    }

    #[allow(clippy::too_many_lines)] // Keeps code consumption and domain state in one transaction.
    async fn verify_sender_code(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "sender-verification/verify-magic-code")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let submitted_code = required_string(&json_body(&request.body)?, "code")?
            .trim()
            .to_owned();
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_dashboard_user_scoped(&*client, &bearer, app_id, Role::Admin, "apps-write")
            .await?;
        let info = load_email_info(&*client, app_id)
            .await?
            .ok_or_else(|| record_not_found("app-email-verification"))?;
        let verification_id: Uuid = info
            .get::<_, Option<Uuid>>(9)
            .ok_or_else(|| record_not_found("app-email-verification"))?;
        let sender_email: String = info
            .get::<_, Option<String>>(7)
            .ok_or_else(|| record_not_found("app-email-sender"))?;
        drop(client);
        let expiry_minutes = self.email_verification_expiry_minutes().await?;
        self.authorize_email_send("sender-verification", app_id, &sender_email)
            .await?;
        let domain = crate::postmark::email_domain(&sender_email);
        let domain_check = if domain.is_empty() {
            Value::Null
        } else {
            self.hooks
                .execute(DashboardControlHookOperation::VerifyEmailDomain {
                    app_id,
                    domain: domain.clone(),
                })
                .await?
        };
        let cloudflare = (!domain_check.is_null()).then(|| {
            (
                domain_check
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                domain_check
                    .get("zone-id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                domain_check
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        });
        client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let consumed = transaction
            .query_opt(
                "DELETE FROM app_email_verification_codes
                 WHERE code = $1 AND verification_id = $2 AND app_id = $3
                   AND created_at >= now() - make_interval(mins => $4)
                 RETURNING id",
                &[&submitted_code, &verification_id, &app_id, &expiry_minutes],
            )
            .await
            .map_err(database_error)?;
        if consumed.is_none() {
            return Err(validation("Invalid verification code."));
        }
        transaction
            .execute(
                "UPDATE app_email_verifications SET verified = true
                 WHERE id = $1 AND app_id = $2",
                &[&verification_id, &app_id],
            )
            .await
            .map_err(database_error)?;
        if let Some((enabled, zone_id, last_error)) = &cloudflare {
            transaction
                .execute(
                    "INSERT INTO app_sender_domains
                       (app_id, domain, cloudflare_zone_id, verified_at,
                        ownership_verified_at, last_checked_at, last_error)
                     VALUES ($1, $2, $3,
                             CASE WHEN $4 THEN now() ELSE NULL END, now(), now(), $5)
                     ON CONFLICT (app_id, domain) DO UPDATE SET
                       cloudflare_zone_id = EXCLUDED.cloudflare_zone_id,
                       verified_at = EXCLUDED.verified_at,
                       ownership_verified_at = now(),
                       last_checked_at = now(),
                       last_error = EXCLUDED.last_error",
                    &[&app_id, &domain, &zone_id, &enabled, &last_error],
                )
                .await
                .map_err(database_error)?;
        }
        transaction.commit().await.map_err(database_error)?;
        cloudflare.map_or_else(
            || json_response(json!({"verified": true})),
            |(enabled, _, last_error)| {
                json_response(json!({
                    "verified": enabled,
                    "cloudflare-enabled": enabled,
                    "error": last_error,
                }))
            },
        )
    }

    async fn send_test_email(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "send-test-email")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let subject = required_string(&body, "subject")?.trim().to_owned();
        let template_body = required_string(&body, "body")?.trim().to_owned();
        let sender_email = optional_email_value(&body, "sender-email")?;
        let custom_sender_name = optional_string(&body, "sender-name");
        let to = email_value(&body, "to")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_dashboard_user_scoped(&*client, &bearer, app_id, Role::Admin, "apps-write")
            .await?;
        let app = load_app(&*client, app_id).await?;
        let app_title: String = app.get(3);
        let expiry_minutes = configured_login_expiry(app.get::<_, Option<i32>>(6));
        let allowed = client
            .query_opt(
                "SELECT 1 FROM instant_users u
                 WHERE u.email = $2 AND (
                   EXISTS (SELECT 1 FROM apps a WHERE a.id = $1 AND a.creator_id = u.id)
                   OR EXISTS (SELECT 1 FROM app_members m WHERE m.app_id = $1 AND m.user_id = u.id)
                   OR EXISTS (
                     SELECT 1 FROM apps a JOIN org_members om ON om.org_id = a.org_id
                     WHERE a.id = $1 AND om.user_id = u.id
                   )
                 )",
                &[&app_id, &to],
            )
            .await
            .map_err(database_error)?
            .is_some();
        drop(client);
        if !allowed {
            return Err(validation(
                "You can only send a test email to a member of this app.",
            ));
        }
        self.authorize_email_send("test", app_id, &to).await?;
        let sender_name = custom_sender_name.unwrap_or_else(|| app_title.clone());
        let subject = render_test_template(&subject, &to, &app_title, expiry_minutes, false);
        let rendered_body =
            render_test_template(&template_body, &to, &app_title, expiry_minutes, true);
        self.deliver_dashboard_email(
            "test",
            app_id,
            to.clone(),
            sender_email,
            sender_name,
            subject,
            rendered_body,
        )
        .await?;
        json_response(json!({"sent-to": to}))
    }

    async fn authorize_email_send(
        &self,
        kind: &'static str,
        app_id: Uuid,
        email: &str,
    ) -> Result<(), ApiError> {
        let allowed = self
            .hooks
            .execute(DashboardControlHookOperation::AuthorizeEmailSend {
                kind,
                app_id,
                email: email.to_owned(),
            })
            .await?
            .as_bool()
            .unwrap_or(false);
        if allowed {
            Ok(())
        } else {
            Err(permission("Email delivery is not available"))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_dashboard_email(
        &self,
        kind: &'static str,
        app_id: Uuid,
        to: String,
        sender_email: Option<String>,
        sender_name: String,
        subject: String,
        body: String,
    ) -> Result<(), ApiError> {
        let delivered = self
            .hooks
            .execute(DashboardControlHookOperation::DeliverDashboardEmail {
                kind,
                app_id,
                to,
                sender_email,
                sender_name,
                subject,
                body,
            })
            .await?;
        if delivered.as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(ApiError::new(
                ApiErrorType::Unknown,
                "Email delivery is unavailable",
            ))
        }
    }

    async fn list_oauth_apps(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "oauth-apps")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let rows = client
            .query(
                "SELECT id, app_id, app_name, granted_scopes, is_public,
                        support_email, app_home_page, app_privacy_policy_link,
                        app_tos_link, app_logo, created_at, updated_at
                 FROM instant_oauth_apps WHERE app_id = $1 ORDER BY created_at DESC",
                &[&app_id],
            )
            .await
            .map_err(database_error)?;
        let mut apps = Vec::with_capacity(rows.len());
        for row in rows {
            let oauth_app_id: Uuid = row.get(0);
            let client_rows = client
                .query(
                    "SELECT client_id, oauth_app_id, client_name, authorized_redirect_urls,
                            created_at, updated_at
                     FROM instant_oauth_app_clients WHERE oauth_app_id = $1
                     ORDER BY created_at DESC",
                    &[&oauth_app_id],
                )
                .await
                .map_err(database_error)?;
            let mut clients = Vec::with_capacity(client_rows.len());
            for client_row in client_rows {
                let client_id: Uuid = client_row.get(0);
                let secrets = client
                    .query(
                        "SELECT id, client_id, first_four, created_at
                         FROM instant_oauth_app_client_secrets WHERE client_id = $1
                         ORDER BY created_at DESC",
                        &[&client_id],
                    )
                    .await
                    .map_err(database_error)?
                    .iter()
                    .map(oauth_app_secret_json)
                    .collect::<Vec<_>>();
                let mut value = oauth_app_client_json(&client_row);
                value["clientSecrets"] = Value::Array(secrets);
                clients.push(value);
            }
            let mut value = oauth_app_json(&row)?;
            value["clients"] = Value::Array(clients);
            apps.push(value);
        }
        json_response(json!({"apps": apps}))
    }

    async fn create_oauth_app(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "oauth-apps")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let app_name = required_string(&body, "app_name")?.trim().to_owned();
        let fields = oauth_app_input(&body)?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let row = client
            .query_one(
                "INSERT INTO instant_oauth_apps
                   (id, app_id, app_name, granted_scopes, is_public, support_email,
                    app_home_page, app_privacy_policy_link, app_tos_link, app_logo)
                 VALUES ($1, $2, $3, '{}', false, $4, $5, $6, $7, $8)
                 RETURNING id, app_id, app_name, granted_scopes, is_public,
                           support_email, app_home_page, app_privacy_policy_link,
                           app_tos_link, app_logo, created_at, updated_at",
                &[
                    &Uuid::new_v4(),
                    &app_id,
                    &app_name,
                    &fields.support_email,
                    &fields.home_page,
                    &fields.privacy_policy,
                    &fields.terms,
                    &fields.logo,
                ],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"app": oauth_app_json(&row)?}))
    }

    async fn update_oauth_app(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, oauth_app_id) =
            oauth_app_ids(&request.path).ok_or_else(|| malformed("Malformed OAuth app ID"))?;
        let body = json_body(&request.body)?;
        let app_name = optional_string(&body, "app_name");
        let fields = oauth_app_input(&body)?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let row = client
            .query_opt(
                "UPDATE instant_oauth_apps SET
                   app_name = coalesce($3, app_name),
                   support_email = coalesce($4, support_email),
                   app_home_page = coalesce($5, app_home_page),
                   app_privacy_policy_link = coalesce($6, app_privacy_policy_link),
                   app_tos_link = coalesce($7, app_tos_link),
                   app_logo = coalesce($8, app_logo)
                 WHERE id = $1 AND app_id = $2
                 RETURNING id, app_id, app_name, granted_scopes, is_public,
                           support_email, app_home_page, app_privacy_policy_link,
                           app_tos_link, app_logo, created_at, updated_at",
                &[
                    &oauth_app_id,
                    &app_id,
                    &app_name,
                    &fields.support_email,
                    &fields.home_page,
                    &fields.privacy_policy,
                    &fields.terms,
                    &fields.logo,
                ],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-app"))?;
        json_response(json!({"app": oauth_app_json(&row)?}))
    }

    async fn delete_oauth_app(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, oauth_app_id) =
            oauth_app_ids(&request.path).ok_or_else(|| malformed("Malformed OAuth app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        let row = client
            .query_opt(
                "DELETE FROM instant_oauth_apps WHERE id = $1 AND app_id = $2
                 RETURNING id, app_id, app_name, granted_scopes, is_public,
                           support_email, app_home_page, app_privacy_policy_link,
                           app_tos_link, app_logo, created_at, updated_at",
                &[&oauth_app_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-app"))?;
        json_response(json!({"app": oauth_app_json(&row)?}))
    }

    async fn create_oauth_app_client(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, oauth_app_id) = oauth_app_client_collection_ids(&request.path)
            .ok_or_else(|| malformed("Malformed OAuth app ID"))?;
        let body = json_body(&request.body)?;
        let client_name = required_string(&body, "client_name")?.trim().to_owned();
        let redirects =
            optional_string_array(&body, "authorized_redirect_urls")?.unwrap_or_default();
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let oauth_app = client
            .query_opt(
                "SELECT is_public FROM instant_oauth_apps WHERE id = $1 AND app_id = $2",
                &[&oauth_app_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-app"))?;
        validate_redirects(&redirects, !oauth_app.get::<_, bool>(0))?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let client_id = Uuid::new_v4();
        let client_row = transaction
            .query_one(
                "INSERT INTO instant_oauth_app_clients
                   (client_id, oauth_app_id, client_name, authorized_redirect_urls)
                 VALUES ($1, $2, $3, $4)
                 RETURNING client_id, oauth_app_id, client_name,
                           authorized_redirect_urls, created_at, updated_at",
                &[&client_id, &oauth_app_id, &client_name, &redirects],
            )
            .await
            .map_err(database_error)?;
        let (secret_row, secret_value) = insert_oauth_app_secret(&transaction, client_id).await?;
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({
            "client": oauth_app_client_json(&client_row),
            "clientSecret": oauth_app_secret_json(&secret_row),
            "secretValue": secret_value,
        }))
    }

    async fn update_oauth_app_client(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, client_id) = oauth_app_client_ids(&request.path)
            .ok_or_else(|| malformed("Malformed OAuth client ID"))?;
        let body = json_body(&request.body)?;
        let client_name = optional_string(&body, "client_name");
        let add = optional_string(&body, "add_redirect_url");
        let remove = optional_string(&body, "remove_redirect_url");
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let row = client
            .query_opt(
                "SELECT c.authorized_redirect_urls, a.is_public
                 FROM instant_oauth_app_clients c
                 JOIN instant_oauth_apps a ON a.id = c.oauth_app_id
                 WHERE c.client_id = $1 AND a.app_id = $2",
                &[&client_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-client"))?;
        let mut redirects = row.get::<_, Option<Vec<String>>>(0).unwrap_or_default();
        if let Some(value) = add {
            validate_redirects(std::slice::from_ref(&value), !row.get::<_, bool>(1))?;
            if !redirects.contains(&value) {
                redirects.push(value);
            }
        }
        if let Some(value) = remove {
            redirects.retain(|redirect| redirect != &value);
        }
        let updated = client
            .query_opt(
                "UPDATE instant_oauth_app_clients c SET
                   client_name = coalesce($3, client_name),
                   authorized_redirect_urls = $4
                 FROM instant_oauth_apps a
                 WHERE c.client_id = $1 AND c.oauth_app_id = a.id AND a.app_id = $2
                 RETURNING c.client_id, c.oauth_app_id, c.client_name,
                           c.authorized_redirect_urls, c.created_at, c.updated_at",
                &[&client_id, &app_id, &client_name, &redirects],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-client"))?;
        json_response(json!({"client": oauth_app_client_json(&updated)}))
    }

    async fn delete_oauth_app_client(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, client_id) = oauth_app_client_ids(&request.path)
            .ok_or_else(|| malformed("Malformed OAuth client ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        let row = client
            .query_opt(
                "DELETE FROM instant_oauth_app_clients c
                 USING instant_oauth_apps a
                 WHERE c.client_id = $1 AND c.oauth_app_id = a.id AND a.app_id = $2
                 RETURNING c.client_id, c.oauth_app_id, c.client_name,
                           c.authorized_redirect_urls, c.created_at, c.updated_at",
                &[&client_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-client"))?;
        json_response(json!({"client": oauth_app_client_json(&row)}))
    }

    async fn create_oauth_app_client_secret(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, client_id) = oauth_app_client_secret_collection_ids(&request.path)
            .ok_or_else(|| malformed("Malformed OAuth client ID"))?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        if client
            .query_opt(
                "SELECT 1 FROM instant_oauth_app_clients c
                 JOIN instant_oauth_apps a ON a.id = c.oauth_app_id
                 WHERE c.client_id = $1 AND a.app_id = $2",
                &[&client_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .is_none()
        {
            return Err(record_not_found("oauth-app-client-secrets"));
        }
        let transaction = client.transaction().await.map_err(database_error)?;
        let (row, secret_value) = insert_oauth_app_secret(&transaction, client_id).await?;
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({
            "clientSecret": oauth_app_secret_json(&row),
            "secretValue": secret_value,
        }))
    }

    async fn delete_oauth_app_client_secret(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, secret_id) = oauth_app_client_secret_ids(&request.path)
            .ok_or_else(|| malformed("Malformed OAuth client secret ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let row = client
            .query_opt(
                "DELETE FROM instant_oauth_app_client_secrets s
                 USING instant_oauth_app_clients c, instant_oauth_apps a
                 WHERE s.id = $1 AND s.client_id = c.client_id
                   AND c.oauth_app_id = a.id AND a.app_id = $2
                 RETURNING s.id, s.client_id, s.first_four, s.created_at",
                &[&secret_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-app-client-secrets"))?;
        json_response(json!({"clientSecret": oauth_app_secret_json(&row)}))
    }

    async fn list_user_oauth_apps(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let user_id = authenticate_refresh(&*client, &bearer)
            .await?
            .user_id
            .expect("refresh token has a user");
        let apps = user_authorized_oauth_apps(&*client, user_id).await?;
        json_response(json!({"oauthApps": apps}))
    }

    async fn revoke_user_oauth_app(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let oauth_app_id = uuid_value(&json_body(&request.body)?, "oauthAppId")?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let user_id = authenticate_refresh(&*client, &bearer)
            .await?
            .user_id
            .expect("refresh token has a user");
        let transaction = client.transaction().await.map_err(database_error)?;
        transaction
            .execute(
                "DELETE FROM instant_user_oauth_access_tokens t
                 USING instant_oauth_app_clients c
                 WHERE t.client_id = c.client_id AND t.user_id = $1
                   AND c.oauth_app_id = $2",
                &[&user_id, &oauth_app_id],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "DELETE FROM instant_user_oauth_refresh_tokens t
                 USING instant_oauth_app_clients c
                 WHERE t.client_id = c.client_id AND t.user_id = $1
                   AND c.oauth_app_id = $2",
                &[&user_id, &oauth_app_id],
            )
            .await
            .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        let apps = user_authorized_oauth_apps(&*client, user_id).await?;
        json_response(json!({"oauthApps": apps}))
    }

    async fn create_special_app(
        &self,
        request: DashboardRequest,
        ephemeral: bool,
    ) -> Result<DashboardResponse, ApiError> {
        let kind = if ephemeral { "ephemeral" } else { "get-a-db" };
        let (creator_id, enabled) = self.special_app_configuration(kind).await?;
        if ephemeral && !enabled {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Ephemeral app creation is disabled",
            )
            .with_status(StatusCode::FORBIDDEN));
        }
        let body = json_body(&request.body)?;
        let title = app_title_value(&body)?;
        let mut client = self.acquire_client().await?;
        if !ephemeral {
            let bearer = bearer(&request)?;
            let principal = authenticate_management_user(&*client, &bearer, true).await?;
            if principal.user_id != Some(creator_id) {
                return Err(permission(
                    "Only the get-a-db service user may create this app",
                ));
            }
        }
        let app_id = Uuid::new_v4();
        if is_reserved_app_id(app_id) {
            return Err(validation(
                "This app ID is reserved for deployment configuration",
            ));
        }
        let transaction = client.transaction().await.map_err(database_error)?;
        let admin_token = Uuid::new_v4();
        let row = transaction
            .query_one(
                "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, $3)
                 RETURNING id, creator_id, org_id, title, created_at, status, magic_code_expiry_minutes",
                &[&app_id, &creator_id, &title],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
                &[&admin_token, &app_id],
            )
            .await
            .map_err(database_error)?;
        let app = special_app_json(&row, admin_token);
        let created_at: NaiveDateTime = row.get(4);
        transaction.commit().await.map_err(database_error)?;
        drop(client);
        self.initialize_created_app(app_id, &body).await?;
        self.hooks
            .execute(DashboardControlHookOperation::AfterCreate { app_id, body })
            .await?;
        if ephemeral {
            json_response(json!({"app": app, "expires_ms": special_expiry_ms(created_at)}))
        } else {
            json_response(json!({"app": app}))
        }
    }

    async fn get_special_app(
        &self,
        request: DashboardRequest,
        ephemeral: bool,
    ) -> Result<DashboardResponse, ApiError> {
        let kind = if ephemeral { "ephemeral" } else { "get-a-db" };
        let app_id = special_app_id(&request.path, ephemeral, None)
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let (creator_id, _) = self.special_app_configuration(kind).await?;
        let client = self.acquire_client().await?;
        let row = load_app(&*client, app_id).await?;
        if row.get::<_, Option<Uuid>>(1) != Some(creator_id) {
            return Err(permission("This is not the requested special app type"));
        }
        let created_at: NaiveDateTime = row.get(4);
        let app = app_json(&row, None);
        if ephemeral {
            json_response(json!({"app": app, "expires_ms": special_expiry_ms(created_at)}))
        } else {
            json_response(json!({"app": app}))
        }
    }

    async fn set_special_app_status(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = special_app_id(&request.path, true, Some("status"))
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let (creator_id, _) = self.special_app_configuration("ephemeral").await?;
        let body = json_body(&request.body)?;
        let admin_token = uuid_value(&body, "admin-token")?;
        let status = required_string(&body, "status")?;
        if !matches!(status, "active" | "read-only" | "disabled") {
            return Err(validation(
                "Status must be one of: active, read-only, disabled.",
            ));
        }
        let client = self.acquire_client().await?;
        let row = load_app(&*client, app_id).await?;
        if row.get::<_, Option<Uuid>>(1) != Some(creator_id) {
            return Err(permission("This is not an ephemeral app"));
        }
        if client
            .query_opt(
                "SELECT 1 FROM app_admin_tokens WHERE app_id = $1 AND token = $2",
                &[&app_id, &admin_token],
            )
            .await
            .map_err(database_error)?
            .is_none()
        {
            return Err(record_not_found("app-admin-token"));
        }
        client
            .execute(
                "UPDATE apps SET status = $1 WHERE id = $2",
                &[&status, &app_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"status": status}))
    }

    async fn special_app_configuration(
        &self,
        kind: &'static str,
    ) -> Result<(Uuid, bool), ApiError> {
        let value = self
            .hooks
            .execute(DashboardControlHookOperation::SpecialAppConfiguration { kind })
            .await?;
        let creator_id = value
            .get("creator_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                ApiError::new(ApiErrorType::Unknown, "Special app creator is unavailable")
            })?;
        let enabled = value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok((creator_id, enabled))
    }

    async fn get_test_users(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "test_users")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, false).await?;
        let users = client
            .query(
                "SELECT id, app_id, email, code, created_at
                 FROM app_test_users WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(test_user_json)
            .collect::<Vec<_>>();
        json_response(json!({"test-users": users}))
    }

    async fn handle_test_users(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        match request.method.as_str() {
            "GET" => self.get_test_users(request).await,
            "POST" => self.create_test_user(request).await,
            "DELETE" => self.delete_test_user(request).await,
            _ => Err(ApiError::new(
                ApiErrorType::Unknown,
                "Test-user route classifier mismatch",
            )),
        }
    }

    async fn create_test_user(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "test_users")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let email = email_value(&body, "email")?;
        let code = required_string(&body, "code")?.trim();
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(validation("Code must be a 6-digit number."));
        }
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal =
            authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        if principal.role < Some(Role::Admin) {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Only an admin may set a static test-user code",
            )
            .with_status(StatusCode::FORBIDDEN));
        }
        let row = client
            .query_one(
                "INSERT INTO app_test_users (id, app_id, email, code)
                 VALUES ($1, $2, $3, $4)
                 RETURNING id, app_id, email, code, created_at",
                &[&Uuid::new_v4(), &app_id, &email, &code],
            )
            .await
            .map_err(test_user_insert_error)?;
        json_response(json!({"test-user": test_user_json(&row)}))
    }

    async fn delete_test_user(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "test_users")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let id = uuid_value(&json_body(&request.body)?, "id")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, false).await?;
        let user = client
            .query_opt(
                "DELETE FROM app_test_users WHERE app_id = $1 AND id = $2
                 RETURNING id, app_id, email, code, created_at",
                &[&app_id, &id],
            )
            .await
            .map_err(database_error)?
            .as_ref()
            .map(test_user_json);
        json_response(json!({"test-user": user}))
    }

    async fn upload_dashboard_file(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "storage/upload")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let path = dashboard_upload_path(request.query.as_deref(), &request.headers)?;
        let content_type = request
            .headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_length = request
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| malformed("Malformed content-length"))
            })
            .transpose()?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        drop(client);
        let data = self
            .hooks
            .execute(DashboardControlHookOperation::UploadDashboardFile {
                app_id,
                path,
                content_type,
                content_length,
                body: request.body.to_vec(),
            })
            .await?;
        if data.is_null() {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "Dashboard storage upload is unavailable",
            ));
        }
        json_response(json!({"data": data}))
    }

    async fn delete_dashboard_files(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "storage/files/delete")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let paths = body
            .get("filenames")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("filenames"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| malformed("Malformed filename"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        drop(client);
        let data = self
            .hooks
            .execute(DashboardControlHookOperation::DeleteDashboardFiles { app_id, paths })
            .await?;
        if data.is_null() {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "Dashboard storage deletion is unavailable",
            ));
        }
        json_response(json!({"data": data}))
    }

    /// Compiles a rules document and reports its errors without saving it.
    ///
    /// The compilation is the same `RuleSet::try_new` call `put_rules` makes,
    /// so an editor sees the exact errors a save would return. Nothing is
    /// written.
    async fn validate_rules(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "rules/validate")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        drop(client);
        let code = json_body(&request.body)?
            .get("code")
            .cloned()
            .ok_or_else(|| missing("code"))?;
        match RuleSet::try_new(code) {
            Ok(_) => json_response(json!({"ok": true})),
            Err(issues) => {
                let errors = issues
                    .into_iter()
                    .map(|issue| json!({"in": issue.path, "message": issue.message}))
                    .collect::<Vec<_>>();
                json_response(json!({"ok": false, "errors": errors}))
            }
        }
    }

    async fn put_rules(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "rules").ok_or_else(|| malformed("Malformed app ID"))?;
        let code = json_body(&request.body)?
            .get("code")
            .cloned()
            .ok_or_else(|| missing("code"))?;
        if let Err(issues) = RuleSet::try_new(code.clone()) {
            let errors = issues
                .into_iter()
                .map(|issue| json!({"message": issue.message, "path": issue.path}))
                .collect::<Vec<_>>();
            return Err(validation("Rule validation failed").with_hint(json!({
                "record-type": "rule",
                "errors": errors
            })));
        }
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let rules = client
            .query_opt(
                "INSERT INTO rules (app_id, code) VALUES ($1, $2)
                 ON CONFLICT (app_id) DO UPDATE
                 SET code = excluded.code, version = rules.version + 1
                 WHERE rules.code IS DISTINCT FROM excluded.code
                 RETURNING app_id, code, version",
                &[&app_id, &tokio_postgres::types::Json(&code)],
            )
            .await
            .map_err(database_error)?
            .as_ref()
            .map(rule_json);
        drop(client);
        json_response(json!({"rules": rules}))
    }

    async fn get_rule_versions(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = action_app_id(&request.path, "rule-versions")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let versions = client
            .query(
                "SELECT version, edits, created_at FROM rule_versions
                 WHERE app_id = $1 ORDER BY version DESC LIMIT 50",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(rule_version_json)
            .collect::<Vec<_>>();
        json_response(json!({"versions": versions}))
    }

    async fn pull_permissions(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "perms/pull")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let code = client
            .query_opt("SELECT code FROM rules WHERE app_id = $1", &[&app_id])
            .await
            .map_err(database_error)?
            .map(|row| row.get::<_, tokio_postgres::types::Json<Value>>(0).0);
        drop(client);
        json_response(json!({"perms": code}))
    }

    async fn handle_cli_pull(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        if app_setting_id(&request.path, "perms/pull").is_some() {
            self.pull_permissions(request).await
        } else {
            self.pull_schema(request).await
        }
    }

    async fn pull_schema(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "schema/pull")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let app = load_app(&*client, app_id).await?;
        let statement = format!(
            "SELECT {ATTRIBUTE_COLUMNS}
             FROM attrs
             WHERE (app_id = $1 OR app_id = $2)
               AND deletion_marked_at IS NULL
             ORDER BY id"
        );
        let attributes = client
            .query(
                &statement,
                &[&app_id, &instant_instaql::SYSTEM_CATALOG_APP_ID],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(row_to_attribute)
            .collect::<Result<Vec<_>, _>>()?;
        let schema = attributes_to_dashboard_schema(&attributes);
        let app_title = app.get::<_, String>(3);
        drop(client);
        json_response(json!({
            "schema": schema,
            "attrs": attributes,
            "app-title": app_title,
        }))
    }

    async fn plan_schema_push(
        &self,
        request: DashboardRequest,
        apply: bool,
    ) -> Result<DashboardResponse, ApiError> {
        let setting = if apply {
            "schema/push/apply"
        } else {
            "schema/push/plan"
        };
        let app_id =
            app_setting_id(&request.path, setting).ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let client_schema = body.get("schema").cloned().unwrap_or_else(|| json!({}));
        let check_types = clojure_truthy(body.get("check_types"));
        let background_updates = clojure_truthy(body.get("supports_background_updates"));
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, apply, true).await?;
        let attributes = load_dashboard_attributes(&*client, app_id).await?;
        let mut plan =
            schema_push_plan(&attributes, &client_schema, check_types, background_updates)?;
        if !apply {
            return json_response(plan);
        }
        let steps = plan
            .get("steps")
            .and_then(Value::as_array)
            .cloned()
            .expect("schema plans always contain steps");
        let (result, jobs_to_enqueue) =
            apply_schema_plan(&mut client, app_id, &attributes, &steps, false).await?;
        plan.as_object_mut()
            .expect("schema plans are objects")
            .extend(
                result
                    .as_object()
                    .expect("apply results are objects")
                    .clone(),
            );
        drop(client);
        for job_id in jobs_to_enqueue {
            self.hooks
                .execute(DashboardControlHookOperation::EnqueueIndexingJob { job_id })
                .await?;
        }
        json_response(plan)
    }

    async fn apply_schema_steps(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "schema/steps/apply")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let steps = body
            .get("steps")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| validation("steps must be a collection"))?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let attributes = load_dashboard_attributes(&*client, app_id).await?;
        let (result, jobs_to_enqueue) =
            apply_schema_plan(&mut client, app_id, &attributes, &steps, true).await?;
        drop(client);
        for job_id in jobs_to_enqueue {
            self.hooks
                .execute(DashboardControlHookOperation::EnqueueIndexingJob { job_id })
                .await?;
        }
        json_response(result)
    }

    async fn get_indexing_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, job_id) = indexing_job_ids(&request.path)
            .ok_or_else(|| malformed("Malformed indexing job ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let job = client
            .query_opt(&indexing_job_query("j.id = $2"), &[&app_id, &job_id])
            .await
            .map_err(database_error)?
            .as_ref()
            .map(indexing_job_json);
        json_response(json!({"job": job}))
    }

    async fn get_indexing_job_group(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, group_id) = indexing_job_group_ids(&request.path)
            .ok_or_else(|| malformed("Malformed indexing job group ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let jobs = client
            .query(
                &indexing_job_query("j.group_id = $2"),
                &[&app_id, &group_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(indexing_job_json)
            .collect::<Vec<_>>();
        json_response(json!({"jobs": jobs}))
    }

    async fn create_indexing_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "indexing-jobs")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let attr_id = uuid_value(&body, "attr-id")?;
        let job_type = required_string(&body, "job-type")?;
        let Some((serial_key, initial_stage)) = indexing_job_spec(job_type) else {
            return Err(validation("Invalid indexing job type"));
        };
        let checked_data_type = if job_type == "check-data-type" {
            Some(required_string(&body, "checked-data-type")?)
        } else {
            None
        };
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        if client
            .query_opt(
                "SELECT 1 FROM attrs
                 WHERE app_id = $1 AND id = $2 AND deletion_marked_at IS NULL",
                &[&app_id, &attr_id],
            )
            .await
            .map_err(database_error)?
            .is_none()
        {
            return Err(record_not_found("attrs"));
        }
        let job_id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO indexing_jobs (
                   id, app_id, attr_id, job_serial_key, job_type, job_status,
                   job_stage, checked_data_type
                 ) VALUES (
                   $1, $2, $3, $4, $5, 'waiting', $6,
                   CASE WHEN $7::text IS NULL THEN NULL ELSE $7::text::checked_data_type END
                 )",
                &[
                    &job_id,
                    &app_id,
                    &attr_id,
                    &serial_key,
                    &job_type,
                    &initial_stage,
                    &checked_data_type,
                ],
            )
            .await
            .map_err(database_error)?;
        let row = client
            .query_one(&indexing_job_query("j.id = $2"), &[&app_id, &job_id])
            .await
            .map_err(database_error)?;
        let job = indexing_job_json(&row);
        drop(client);
        self.hooks
            .execute(DashboardControlHookOperation::EnqueueIndexingJob { job_id })
            .await?;
        json_response(json!({"job": job}))
    }

    async fn backup_schedule(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "backup-schedule")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let writing = request.method == "PUT";
        authorize_app(
            &*client,
            &bearer,
            app_id,
            if writing {
                Role::Admin
            } else {
                Role::Collaborator
            },
            writing,
            true,
        )
        .await?;
        if writing {
            let body = json_body(&request.body)?;
            let cron = body
                .get("cron")
                .and_then(Value::as_str)
                .filter(|v| v.len() <= 128)
                .ok_or_else(|| validation("Enter a five-field cron expression."))?;
            let enabled = body
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| validation("enabled must be a boolean"))?;
            let parsed = crate::backup_schedule::Cron::parse(cron)?;
            let next = crate::backup_schedule::next_cron(parsed, chrono::Utc::now()).await?;
            if enabled {
                let creator_id = load_app(&*client, app_id).await?.get::<_, Option<Uuid>>(1);
                let work_estimate: i64 = client
                    .query_one(
                        "SELECT coalesce(sum(total), 0)::bigint FROM attr_sketches WHERE app_id=$1",
                        &[&app_id],
                    )
                    .await
                    .map_err(database_error)?
                    .get(0);
                if !self
                    .hooks
                    .execute(DashboardControlHookOperation::AuthorizeBackup {
                        app_id,
                        creator_id,
                        work_estimate,
                        client_ip: forwarded_client_ip(&request),
                    })
                    .await?
                    .as_bool()
                    .unwrap_or(false)
                {
                    return Err(permission("This app cannot schedule backups"));
                }
            }
            client
                .execute(
                    "INSERT INTO app_backup_schedules(app_id,cron,enabled,next_run_at)
                VALUES ($1,$2,$3,$4) ON CONFLICT(app_id) DO UPDATE
                SET cron=$2,enabled=$3,next_run_at=$4,updated_at=now()",
                    &[&app_id, &cron, &enabled, &next],
                )
                .await
                .map_err(database_error)?;
        }
        let row = client.query_opt("SELECT cron,enabled,next_run_at,last_enqueued_at FROM app_backup_schedules WHERE app_id=$1", &[&app_id])
            .await.map_err(database_error)?;
        let schedule = row.map(|row| {
            json!({
                "cron": row.get::<_,String>(0), "enabled": row.get::<_,bool>(1),
                "next_run_at": row.get::<_,chrono::DateTime<chrono::Utc>>(2),
                "last_enqueued_at": row.get::<_,Option<chrono::DateTime<chrono::Utc>>>(3),
            })
        });
        json_response(json!({"schedule":schedule}))
    }

    async fn list_backups(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "backups")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            false,
            "data-read",
        )
        .await?;
        let backups = client
            .query(
                "SELECT id, isn::text, backup_at, files_size, db_size,
                        uncompressed_size, description, expires_at
                 FROM app_backups
                 WHERE app_id = $1 AND expires_at > now()
                   AND deletion_marked_at IS NULL
                 ORDER BY backup_at DESC",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(backup_json)
            .collect::<Vec<_>>();
        json_response(json!({"backups": backups}))
    }

    async fn create_backup_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "backups")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let description = optional_non_blank_string(&body, "description")?;
        let bearer = bearer(&request)?;
        let client_ip = forwarded_client_ip(&request);
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let creator_id = load_app(&*client, app_id).await?.get::<_, Option<Uuid>>(1);
        let work_estimate = client
            .query_one(
                "SELECT coalesce(sum(total), 0)::bigint FROM attr_sketches WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .get::<_, i64>(0);
        let allowed = self
            .hooks
            .execute(DashboardControlHookOperation::AuthorizeBackup {
                app_id,
                creator_id,
                work_estimate,
                client_ip,
            })
            .await?
            .as_bool()
            .unwrap_or(false);
        if !allowed {
            return Err(permission("This app cannot create an on-demand backup"));
        }
        if client
            .query_opt(
                "SELECT id FROM app_backup_jobs
                 WHERE app_id = $1 AND job_status IN ('waiting', 'processing')
                 LIMIT 1",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .is_some()
        {
            return Err(validation("A backup is already in progress for this app."));
        }
        let job_id = Uuid::new_v4();
        let row = client
            .query_one(
                "INSERT INTO app_backup_jobs
                   (id, app_id, description, work_estimate, job_status)
                 VALUES ($1, $2, $3, $4, 'waiting')
                 RETURNING id, app_id, app_backup_id, job_status, description,
                           work_estimate, work_completed, created_at, updated_at, done_at",
                &[&job_id, &app_id, &description, &work_estimate],
            )
            .await
            .map_err(|error| {
                if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) {
                    validation("A backup is already in progress for this app.")
                } else {
                    database_error(error)
                }
            })?;
        let job = backup_job_json(&row);
        drop(client);
        self.hooks
            .execute(DashboardControlHookOperation::EnqueueBackupJob { job_id })
            .await?;
        json_response(json!({"job": job}))
    }

    async fn list_backup_jobs(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "backup-jobs")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let jobs = client
            .query(
                &format!(
                    "{} WHERE app_id = $1 AND job_status IN ('waiting', 'processing')
                     ORDER BY created_at ASC",
                    backup_job_select()
                ),
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(backup_job_json)
            .collect::<Vec<_>>();
        json_response(json!({"jobs": jobs}))
    }

    async fn get_backup_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, job_id) =
            backup_job_ids(&request.path).ok_or_else(|| malformed("Malformed backup job ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let job = client
            .query_opt(
                &format!("{} WHERE app_id = $1 AND id = $2", backup_job_select()),
                &[&app_id, &job_id],
            )
            .await
            .map_err(database_error)?
            .as_ref()
            .map(backup_job_json);
        json_response(json!({"job": job}))
    }

    async fn cancel_backup_job(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, job_id) =
            backup_job_ids(&request.path).ok_or_else(|| malformed("Malformed backup job ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let changed = client
            .execute(
                "UPDATE app_backup_jobs
                 SET job_status = 'cancelled', worker_id = NULL, done_at = now()
                 WHERE app_id = $1 AND id = $2
                   AND job_status IN ('waiting', 'processing')",
                &[&app_id, &job_id],
            )
            .await
            .map_err(database_error)?;
        if changed == 0 {
            return Err(record_not_found("app-backup-job"));
        }
        json_response(json!({"id": job_id}))
    }

    async fn delete_backup(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, backup_id) =
            backup_ids(&request.path).ok_or_else(|| malformed("Malformed backup ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, true).await?;
        let changed = client
            .execute(
                "UPDATE app_backups SET deletion_marked_at = now()
                 WHERE app_id = $1 AND id = $2 AND deletion_marked_at IS NULL",
                &[&app_id, &backup_id],
            )
            .await
            .map_err(database_error)?;
        if changed == 0 {
            return Err(record_not_found("app-backup"));
        }
        json_response(json!({"id": backup_id}))
    }

    async fn list_backup_files(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, backup_id) = backup_action_ids(&request.path, "files")
            .ok_or_else(|| malformed("Malformed backup ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            false,
            "data-read",
        )
        .await?;
        let storage_prefix = active_backup_prefix(&*client, app_id, backup_id).await?;
        drop(client);
        let files = self
            .hooks
            .execute(DashboardControlHookOperation::ListBackupFiles {
                app_id,
                backup_id,
                storage_prefix,
            })
            .await?;
        if !files.is_array() {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "Backup object listing is unavailable",
            ));
        }
        json_response(json!({"files": files}))
    }

    async fn get_backup_file_url(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, backup_id) = backup_action_ids(&request.path, "file-url")
            .ok_or_else(|| malformed("Malformed backup ID"))?;
        let file_name = query_parameter(request.query.as_deref(), "name")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| missing("name"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            false,
            "data-read",
        )
        .await?;
        let prefix = active_backup_prefix(&*client, app_id, backup_id).await?;
        drop(client);
        let value = self
            .hooks
            .execute(DashboardControlHookOperation::PresignBackupFile {
                app_id,
                backup_id,
                object_key: format!("{prefix}/{file_name}"),
            })
            .await?;
        let url = value
            .as_str()
            .ok_or_else(|| ApiError::new(ApiErrorType::Unknown, "Backup signing is unavailable"))?;
        json_response(json!({"url": url}))
    }

    async fn stream_backup_storage_files(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, backup_id) = backup_action_ids(&request.path, "storage-files")
            .ok_or_else(|| malformed("Malformed backup ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            false,
            "data-read",
        )
        .await?;
        let prefix = active_backup_prefix(&*client, app_id, backup_id).await?;
        drop(client);
        let object_key = format!("{prefix}/entities/$files.jsonl");
        let body = if let Some(streamer) = &self.backup_streamer {
            streamer
                .stream_backup_storage_files(app_id, backup_id, object_key)
                .await?
        } else {
            let value = self
                .hooks
                .execute(DashboardControlHookOperation::StreamBackupStorageFiles {
                    app_id,
                    backup_id,
                    object_key,
                })
                .await?;
            value
                .as_str()
                .ok_or_else(|| {
                    ApiError::new(
                        ApiErrorType::Unknown,
                        "Backup file streaming is unavailable",
                    )
                })?
                .to_owned()
                .into()
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/x-ndjson"),
        );
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        );
        headers.insert(
            axum::http::HeaderName::from_static("x-accel-buffering"),
            axum::http::HeaderValue::from_static("no"),
        );
        Ok(DashboardResponse {
            status: axum::http::StatusCode::OK,
            headers,
            body,
        })
    }

    async fn list_webhooks(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "webhooks")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-read",
        )
        .await?;
        let webhooks = client
            .query(&webhook_query("TRUE"), &[&app_id])
            .await
            .map_err(database_error)?
            .iter()
            .map(webhook_json)
            .collect::<Vec<_>>();
        json_response(json!({"webhooks": webhooks}))
    }

    async fn create_webhook(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "webhooks")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let url = non_blank_string(&body, "url")?;
        let namespaces = string_vector(&body, "namespaces")?;
        let actions = webhook_actions(&body)?;
        self.validate_webhook_sink(&url).await?;
        self.hooks
            .execute(DashboardControlHookOperation::EnableWebhookLogs { app_id })
            .await?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        authorize_app_scoped(
            &transaction,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-write",
        )
        .await?;
        transaction
            .query_one("SELECT id FROM apps WHERE id = $1 FOR UPDATE", &[&app_id])
            .await
            .map_err(database_error)?;
        let maximum_active_webhooks = self.webhook_limit.maximum_active_webhooks().await?;
        enforce_webhook_limit(&transaction, app_id, maximum_active_webhooks).await?;
        let id_attr_ids = resolve_webhook_namespaces(&transaction, app_id, &namespaces).await?;
        let sink = json!({"url": url});
        ensure_unique_webhook(&transaction, app_id, None, &id_attr_ids, &actions, &sink).await?;
        let webhook_id = Uuid::new_v4();
        let topics = webhook_topics(&id_attr_ids);
        transaction
            .execute(
                "INSERT INTO webhooks
                   (id, app_id, topics, id_attr_ids, actions, status, sink)
                 VALUES ($1, $2, $3, $4, $5::text[]::webhook_action[],
                         'active'::webhook_status, $6)",
                &[
                    &webhook_id,
                    &app_id,
                    &topics,
                    &id_attr_ids,
                    &actions,
                    &tokio_postgres::types::Json(&sink),
                ],
            )
            .await
            .map_err(database_error)?;
        let row = transaction
            .query_one(&webhook_query("w.id = $2"), &[&app_id, &webhook_id])
            .await
            .map_err(database_error)?;
        let webhook = webhook_json(&row);
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({"webhook": webhook}))
    }

    async fn update_webhook(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, webhook_id) =
            webhook_ids(&request.path).ok_or_else(|| malformed("Malformed webhook ID"))?;
        let body = json_body(&request.body)?;
        let url = optional_non_blank_string(&body, "url")?;
        let namespaces = optional_string_vector(&body, "namespaces")?;
        let actions = optional_webhook_actions(&body)?;
        if let Some(url) = &url {
            self.validate_webhook_sink(url).await?;
        }
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        authorize_app_scoped(
            &transaction,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-write",
        )
        .await?;
        let existing = transaction
            .query_opt(&webhook_query("w.id = $2"), &[&app_id, &webhook_id])
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("webhook"))?;
        let current_sink = existing.get::<_, tokio_postgres::types::Json<Value>>(1).0;
        let current_ids: Vec<Uuid> = existing.get(8);
        let current_actions: Vec<String> = existing.get(3);
        let new_ids = match &namespaces {
            Some(namespaces) => {
                resolve_webhook_namespaces(&transaction, app_id, namespaces).await?
            }
            None => current_ids,
        };
        let new_actions = actions.unwrap_or(current_actions);
        let new_sink = url.map_or(current_sink, |url| json!({"url": url}));
        ensure_unique_webhook(
            &transaction,
            app_id,
            Some(webhook_id),
            &new_ids,
            &new_actions,
            &new_sink,
        )
        .await?;
        let topics = webhook_topics(&new_ids);
        transaction
            .execute(
                "UPDATE webhooks SET topics = $3, id_attr_ids = $4,
                   actions = $5::text[]::webhook_action[], sink = $6
                 WHERE app_id = $1 AND id = $2",
                &[
                    &app_id,
                    &webhook_id,
                    &topics,
                    &new_ids,
                    &new_actions,
                    &tokio_postgres::types::Json(&new_sink),
                ],
            )
            .await
            .map_err(database_error)?;
        let row = transaction
            .query_one(&webhook_query("w.id = $2"), &[&app_id, &webhook_id])
            .await
            .map_err(database_error)?;
        let webhook = webhook_json(&row);
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({"webhook": webhook}))
    }

    async fn delete_webhook(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, webhook_id) =
            webhook_ids(&request.path).ok_or_else(|| malformed("Malformed webhook ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-write",
        )
        .await?;
        let row = client
            .query_opt(&webhook_query("w.id = $2"), &[&app_id, &webhook_id])
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("webhook"))?;
        let webhook = webhook_json(&row);
        client
            .execute(
                "DELETE FROM webhooks WHERE app_id = $1 AND id = $2",
                &[&app_id, &webhook_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"webhook": webhook}))
    }

    async fn set_webhook_enabled(
        &self,
        request: DashboardRequest,
        enabled: bool,
    ) -> Result<DashboardResponse, ApiError> {
        let action = if enabled { "enable" } else { "disable" };
        let (app_id, webhook_id) = webhook_action_ids(&request.path, action)
            .ok_or_else(|| malformed("Malformed webhook ID"))?;
        let reason = if enabled {
            None
        } else {
            optional_non_blank_string(&json_body(&request.body)?, "reason")?
        };
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        authorize_app_scoped(
            &transaction,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-write",
        )
        .await?;
        transaction
            .query_one("SELECT id FROM apps WHERE id = $1 FOR UPDATE", &[&app_id])
            .await
            .map_err(database_error)?;
        if enabled {
            let maximum_active_webhooks = self.webhook_limit.maximum_active_webhooks().await?;
            enforce_webhook_limit(&transaction, app_id, maximum_active_webhooks).await?;
            let row = transaction
                .query_opt(&webhook_query("w.id = $2"), &[&app_id, &webhook_id])
                .await
                .map_err(database_error)?
                .ok_or_else(|| record_not_found("webhook"))?;
            let id_attr_ids = row.get::<_, Vec<Uuid>>(8);
            let actions = row.get::<_, Vec<String>>(3);
            let sink = row.get::<_, tokio_postgres::types::Json<Value>>(1).0;
            ensure_unique_webhook(
                &transaction,
                app_id,
                Some(webhook_id),
                &id_attr_ids,
                &actions,
                &sink,
            )
            .await?;
        }
        let changed = transaction
            .execute(
                "UPDATE webhooks
                 SET status = $3::text::webhook_status,
                     disabled_reason = CASE WHEN $3::text = 'active' THEN NULL ELSE $4 END
                 WHERE app_id = $1 AND id = $2",
                &[
                    &app_id,
                    &webhook_id,
                    &if enabled { "active" } else { "disabled" },
                    &reason,
                ],
            )
            .await
            .map_err(database_error)?;
        if changed == 0 {
            return Err(record_not_found("webhook"));
        }
        let row = transaction
            .query_one(&webhook_query("w.id = $2"), &[&app_id, &webhook_id])
            .await
            .map_err(database_error)?;
        let webhook = webhook_json(&row);
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({"webhook": webhook}))
    }

    async fn list_webhook_events(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, webhook_id) = webhook_action_ids(&request.path, "events")
            .ok_or_else(|| malformed("Malformed webhook ID"))?;
        let after = webhook_after_cursor(request.query.as_deref())?;
        let (after_created_at, after_isn) = after.map_or((None, None), |(created_at, isn)| {
            (Some(created_at), Some(isn.to_postgres_composite()))
        });
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-read",
        )
        .await?;
        let rows = client
            .query(
                &webhook_events_query(true),
                &[&app_id, &webhook_id, &after_created_at, &after_isn],
            )
            .await
            .map_err(database_error)?;
        let has_next_page = rows.len() == 101;
        let results = rows
            .iter()
            .take(100)
            .map(webhook_event_json)
            .collect::<Vec<_>>();
        let start_cursor = rows.first().map(webhook_event_cursor);
        let end_cursor = rows
            .get(results.len().saturating_sub(1))
            .map(webhook_event_cursor);
        json_response(json!({
            "events": results,
            "pageInfo": {
                "startCursor": start_cursor,
                "endCursor": end_cursor,
                "hasNextPage": has_next_page,
            }
        }))
    }

    async fn get_webhook_event(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, webhook_id, isn) = webhook_event_ids(&request.path)
            .ok_or_else(|| malformed("Malformed webhook event ISN"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-read",
        )
        .await?;
        let row = client
            .query_opt(
                &webhook_event_by_isn_query(),
                &[&app_id, &webhook_id, &isn.to_postgres_composite()],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("webhook-event"))?;
        json_response(json!({"event": webhook_event_json(&row)}))
    }

    async fn resend_webhook_event(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, webhook_id, isn) = webhook_event_ids(&request.path)
            .ok_or_else(|| malformed("Malformed webhook event ISN"))?;
        let public_isn = isn.to_string();
        let postgres_isn = isn.to_postgres_composite();
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app_scoped(
            &*client,
            &bearer,
            app_id,
            Role::Collaborator,
            true,
            "data-write",
        )
        .await?;
        let changed = client
            .execute(
                "UPDATE webhook_events
                 SET status = 'pending'::webhook_event_status
                 WHERE app_id = $1 AND webhook_id = $2
                   AND isn = $3::text::isn AND machine_id IS NULL",
                &[&app_id, &webhook_id, &postgres_isn],
            )
            .await
            .map_err(database_error)?;
        if changed == 0 {
            return Err(validation(
                "Could not resend event. Try again in one minute.",
            ));
        }
        let row = client
            .query_one(
                &webhook_event_by_isn_query(),
                &[&app_id, &webhook_id, &postgres_isn],
            )
            .await
            .map_err(database_error)?;
        let event = webhook_event_json(&row);
        drop(client);
        self.hooks
            .execute(DashboardControlHookOperation::NotifyWebhookEvent {
                app_id,
                webhook_id,
                isn: public_isn,
            })
            .await?;
        json_response(json!({"event": event}))
    }

    async fn validate_webhook_sink(&self, value: &str) -> Result<(), ApiError> {
        let url = instant_auth::oauth::parse_safe_url(value)
            .map_err(|_| validation("The Webhook url must be a public https url"))?;
        if url.scheme() != "https" {
            return Err(validation("The Webhook url must be https"));
        }
        let allowed = self
            .hooks
            .execute(DashboardControlHookOperation::ValidateWebhookSink {
                url: value.to_owned(),
            })
            .await?;
        if allowed.as_bool() == Some(true) {
            Ok(())
        } else {
            Err(validation("The Webhook url must be a public url"))
        }
    }

    async fn create_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let body = json_body(&request.body)?;
        let id = uuid_value(&body, "id")?;
        if is_reserved_app_id(id) {
            return Err(validation(
                "This app ID is reserved for deployment configuration",
            ));
        }
        let admin_token = uuid_value(&body, "admin_token")?;
        let title = app_title_value(&body)?;
        let org_id = optional_uuid_value(&body, "org_id")?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let principal = authenticate(&transaction, &bearer, true).await?;
        let user_id = principal
            .user_id
            .ok_or_else(|| permission("An admin token cannot create an app"))?;
        if let Some(org_id) = org_id {
            require_org_role(&transaction, user_id, org_id, Role::Collaborator).await?;
        }
        let creator_id = org_id.is_none().then_some(user_id);
        let row = transaction
            .query_one(
                "INSERT INTO apps (id, creator_id, org_id, title)
                 VALUES ($1, $2, $3, $4)
                 RETURNING id, creator_id, org_id, title, created_at, status, magic_code_expiry_minutes",
                &[&id, &creator_id, &org_id, &title],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
                &[&admin_token, &id],
            )
            .await
            .map_err(database_error)?;
        let app = app_json(&row, Some(admin_token));
        transaction.commit().await.map_err(database_error)?;
        drop(client);
        self.initialize_created_app(id, &body).await?;
        self.hooks
            .execute(DashboardControlHookOperation::AfterCreate { app_id: id, body })
            .await?;
        json_response(json!({"app": app}))
    }

    async fn get_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id = direct_app_id(&request.path).ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal =
            authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let row = load_app(&*client, app_id).await?;
        let token = if can_see_admin_token(&principal) {
            Some(
                client
                    .query_one(
                        "SELECT token FROM app_admin_tokens WHERE app_id = $1",
                        &[&app_id],
                    )
                    .await
                    .map_err(database_error)?
                    .get(0),
            )
        } else {
            None
        };
        let app = app_json(&row, token);
        drop(client);
        let mut app = self
            .hooks
            .execute(DashboardControlHookOperation::EnrichApp {
                app_id,
                user_id: principal.user_id,
                app,
            })
            .await?;
        // Management tokens have no user row for the projection to resolve, so
        // the authorized role remains authoritative for this response.
        if let Some(role) = principal.role {
            app["role"] = json!(role.name());
        }
        json_response(json!({"app": app}))
    }

    async fn delete_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id = direct_app_id(&request.path).ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal = authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        let row = load_app(&*client, app_id).await?;
        let creator_id: Option<Uuid> = row.get(1);
        if creator_id.is_some() && creator_id != principal.user_id {
            return Err(permission(
                "Only the owner can delete a personally owned app",
            ));
        }
        client
            .execute(
                "UPDATE apps SET deletion_marked_at = now()
                 WHERE id = $1 AND deletion_marked_at IS NULL",
                &[&app_id],
            )
            .await
            .map_err(database_error)?;
        drop(client);
        json_response(json!({"ok": true}))
    }

    async fn clear_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "clear").ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Owner, true, false).await?;
        drop(client);
        self.hooks
            .execute(DashboardControlHookOperation::ClearApp { app_id })
            .await?;
        json_response(json!({"ok": true}))
    }

    async fn rotate_admin_token(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "tokens").ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let token = uuid_value(&body, "admin-token")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        let row = client
            .query_one(
                "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)
                 ON CONFLICT (app_id) DO UPDATE SET token = EXCLUDED.token
                 RETURNING token, app_id, created_at",
                &[&token, &app_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({
            "token": row.get::<_, Uuid>(0),
            "app_id": row.get::<_, Uuid>(1),
            "created_at": row.get::<_, NaiveDateTime>(2)
        }))
    }

    async fn rotate_admin_token_server(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "tokens/rotate")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        let admin_token = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)
                 ON CONFLICT (app_id) DO UPDATE SET token = EXCLUDED.token",
                &[&admin_token, &app_id],
            )
            .await
            .map_err(database_error)?;
        drop(client);
        json_response(json!({"adminToken": admin_token}))
    }

    async fn soft_deleted_attributes(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = action_app_id(&request.path, "soft_deleted_attrs")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        drop(client);
        let attributes = self
            .hooks
            .execute(DashboardControlHookOperation::SoftDeletedAttributes { app_id })
            .await?;
        json_response(json!({"attrs": attributes, "grace-period-days": 2}))
    }

    async fn rename_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "rename").ok_or_else(|| malformed("Malformed app ID"))?;
        let title = app_title_value(&json_body(&request.body)?)?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Owner, true, false).await?;
        client
            .execute(
                "UPDATE apps SET title = $1 WHERE id = $2",
                &[&title, &app_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({}))
    }

    async fn set_app_status(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id =
            action_app_id(&request.path, "status").ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let status = required_string(&body, "status")?;
        if !matches!(status, "active" | "read-only" | "disabled") {
            return Err(validation(
                "Status must be one of: active, read-only, disabled.",
            ));
        }
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        client
            .execute(
                "UPDATE apps SET status = $1 WHERE id = $2",
                &[&status, &app_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"status": status}))
    }

    async fn set_magic_code_expiry(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = action_app_id(&request.path, "set-magic-code-expiry")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let expiry = body
            .get("expiry")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| malformed("Malformed expiry"))?;
        if expiry <= 0 {
            return Err(validation("The magic token expiry must be positive."));
        }
        let max = i32::try_from(MAX_MAGIC_CODE_EXPIRY_MINUTES).unwrap_or(i32::MAX);
        let expiry = expiry.min(max);
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Admin, true, false).await?;
        let row = client
            .query_one(
                "UPDATE apps SET magic_code_expiry_minutes = $1 WHERE id = $2
                 RETURNING id, creator_id, org_id, title, created_at, status, magic_code_expiry_minutes",
                &[&expiry, &app_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"app": app_json(&row, None)}))
    }

    async fn create_org(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let title = org_title_value(&json_body(&request.body)?)?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let principal = authenticate_refresh(&transaction, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        let org_id = Uuid::new_v4();
        let row = transaction
            .query_one(
                "INSERT INTO orgs (id, title) VALUES ($1, $2)
                 RETURNING id, title, created_at, updated_at",
                &[&org_id, &title],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO org_members (id, org_id, user_id, role)
                 VALUES ($1, $2, $3, 'owner')",
                &[&Uuid::new_v4(), &org_id, &user_id],
            )
            .await
            .map_err(database_error)?;
        let org = org_json(&row);
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({"org": org}))
    }

    async fn get_org(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let org_id = direct_org_id(&request.path).ok_or_else(|| malformed("Malformed org ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal = authenticate_refresh(&*client, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        require_org_or_app_member(&*client, user_id, org_id).await?;
        let row = client
            .query_opt(
                "SELECT id, title, created_at, updated_at FROM orgs WHERE id = $1",
                &[&org_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("org"))?;
        let org = org_json(&row);
        drop(client);
        let details = self
            .hooks
            .execute(DashboardControlHookOperation::OrgDetails {
                org_id,
                user_id,
                org,
            })
            .await?;
        json_response(details)
    }

    async fn delete_org(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let org_id = direct_org_id(&request.path).ok_or_else(|| malformed("Malformed org ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal = authenticate_refresh(&*client, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        require_org_role(&*client, user_id, org_id, Role::Owner).await?;
        client
            .execute("DELETE FROM orgs WHERE id = $1", &[&org_id])
            .await
            .map_err(database_error)?;
        json_response(json!({"ok": true}))
    }

    async fn rename_org(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let org_id =
            action_org_id(&request.path, "rename").ok_or_else(|| malformed("Malformed org ID"))?;
        let title = org_title_value(&json_body(&request.body)?)?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal = authenticate_refresh(&*client, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        require_org_role(&*client, user_id, org_id, Role::Admin).await?;
        client
            .execute(
                "UPDATE orgs SET title = $1 WHERE id = $2",
                &[&title, &org_id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({}))
    }

    async fn transfer_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let (app_id, org_id) = transfer_ids(&request.path)
            .ok_or_else(|| malformed("Malformed app or organization ID"))?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let principal =
            authorize_app(&transaction, &bearer, app_id, Role::Owner, true, false).await?;
        let user_id = principal
            .user_id
            .ok_or_else(|| permission("User token required"))?;
        require_org_role(&transaction, user_id, org_id, Role::Admin).await?;
        let removed_members = transaction
            .query(
                "DELETE FROM app_members app_member
                 USING org_members org_member
                 WHERE app_member.app_id = $1
                   AND org_member.org_id = $2
                   AND org_member.user_id = app_member.user_id
                   AND CASE org_member.role
                         WHEN 'owner' THEN 1 WHEN 'admin' THEN 2
                         WHEN 'collaborator' THEN 3 ELSE 999 END
                       <= CASE app_member.member_role
                         WHEN 'owner' THEN 1 WHEN 'admin' THEN 2
                         WHEN 'collaborator' THEN 3 ELSE 999 END
                 RETURNING app_member.id, app_member.user_id, app_member.member_role,
                   (SELECT email FROM instant_users WHERE id = app_member.user_id) AS email",
                &[&app_id, &org_id],
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| {
                json!({
                    "id": row.get::<_, Uuid>("id"),
                    "user_id": row.get::<_, Uuid>("user_id"),
                    "email": row.get::<_, String>("email"),
                    "role": row.get::<_, String>("member_role"),
                })
            })
            .collect::<Vec<_>>();
        transaction
            .execute(
                "UPDATE apps SET creator_id = NULL, org_id = $1 WHERE id = $2",
                &[&org_id, &app_id],
            )
            .await
            .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        drop(client);
        let result = self
            .hooks
            .execute(DashboardControlHookOperation::AfterTransfer {
                app_id,
                org_id,
                user_id,
                removed_members: Value::Array(removed_members),
            })
            .await?;
        json_response(result)
    }

    async fn claim_app(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id = claim_app_id(&request.path).ok_or_else(|| malformed("Malformed app ID"))?;
        let token = uuid_value(&json_body(&request.body)?, "token")?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let principal = authenticate_refresh(&transaction, &bearer).await?;
        let claimant_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        let claimant_email: String = transaction
            .query_one(
                "SELECT email FROM instant_users WHERE id = $1",
                &[&claimant_id],
            )
            .await
            .map_err(database_error)?
            .get(0);
        let app = load_app(&transaction, app_id).await?;
        let current_creator_id: Option<Uuid> = app.get(1);
        let allowed = self
            .hooks
            .execute(DashboardControlHookOperation::AuthorizeClaim {
                app_id,
                current_creator_id,
                claimant_id,
                claimant_email,
            })
            .await?
            .as_bool()
            .unwrap_or(false);
        if !allowed {
            return Err(permission("This app is not claimable"));
        }
        if transaction
            .query_opt(
                "SELECT 1 FROM app_admin_tokens WHERE app_id = $1 AND token = $2",
                &[&app_id, &token],
            )
            .await
            .map_err(database_error)?
            .is_none()
        {
            return Err(record_not_found("app-admin-token"));
        }
        transaction
            .execute(
                "UPDATE apps SET creator_id = $1 WHERE id = $2",
                &[&claimant_id, &app_id],
            )
            .await
            .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        drop(client);
        json_response(json!({}))
    }

    async fn create_authorized_origin(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "authorized_redirect_origins")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let service = non_blank_string(&body, "service")?;
        let params = string_array(&body, "params")?;
        validate_authorized_origin(&service, &params)?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let row = client
            .query_one(
                "INSERT INTO app_authorized_redirect_origins (id, app_id, service, params)
                 VALUES ($1, $2, $3, $4)
                 RETURNING id, service, params, created_at",
                &[&Uuid::new_v4(), &app_id, &service, &params],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"origin": authorized_origin_json(&row)}))
    }

    async fn delete_authorized_origin(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, origin_id) = authorized_origin_ids(&request.path)
            .ok_or_else(|| malformed("Malformed app or origin ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let row = client
            .query_opt(
                "DELETE FROM app_authorized_redirect_origins
                 WHERE id = $1 AND app_id = $2
                 RETURNING id, service, params, created_at",
                &[&origin_id, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("app-authorized-redirect-origin"))?;
        json_response(json!({"origin": authorized_origin_json(&row)}))
    }

    async fn create_oauth_provider(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "oauth_service_providers")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let provider_name = non_blank_string(&json_body(&request.body)?, "provider_name")?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let provider = OAuthProvider {
            id: Uuid::new_v4(),
            name: provider_name,
        };
        let catalog = system_attribute_catalog().map_err(auth_model_error)?;
        create_oauth_provider(&mut client, app_id, &catalog, &provider)
            .await
            .map_err(auth_model_error)?;
        let created_at = entity_created_at(&*client, app_id, provider.id).await?;
        json_response(json!({
            "provider": {
                "id": provider.id,
                "provider_name": provider.name,
                "created_at": created_at
            }
        }))
    }

    async fn get_app_auth(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let app_id =
            app_setting_id(&request.path, "auth").ok_or_else(|| malformed("Malformed app ID"))?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, false, true).await?;
        let origin_rows = client
            .query(
                "SELECT id, service, params, created_at
                 FROM app_authorized_redirect_origins
                 WHERE app_id = $1 ORDER BY created_at DESC",
                &[&app_id],
            )
            .await
            .map_err(database_error)?;
        let authorized_redirect_origins = if origin_rows.is_empty() {
            Value::Null
        } else {
            Value::Array(origin_rows.iter().map(authorized_origin_json).collect())
        };
        let mut providers = Vec::new();
        for (entity_id, created_at) in
            system_entity_ids(&*client, app_id, "$oauthProviders").await?
        {
            let provider = oauth_provider_by_id(&*client, app_id, entity_id)
                .await
                .map_err(auth_model_error)?
                .ok_or_else(|| record_not_found("oauth-service-provider"))?;
            providers.push(json!({
                "id": provider.id,
                "provider_name": provider.name,
                "created_at": created_at
            }));
        }
        let mut clients = Vec::new();
        for (entity_id, created_at) in system_entity_ids(&*client, app_id, "$oauthClients").await? {
            let oauth_client = oauth_client_by_id(&*client, app_id, entity_id)
                .await
                .map_err(auth_model_error)?
                .ok_or_else(|| record_not_found("app-oauth-client"))?;
            clients.push(oauth_client_json(&oauth_client, created_at, true));
        }
        drop(client);
        let default_sender_email = self
            .hooks
            .execute(DashboardControlHookOperation::DashboardAuthDefaultSender { app_id })
            .await?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorType::Unknown,
                    "Dashboard auth default sender is unavailable",
                )
            })?;
        json_response(json!({
            "authorized_redirect_origins": authorized_redirect_origins,
            "oauth_service_providers": providers,
            "oauth_clients": clients,
            "default_sender_email": default_sender_email,
        }))
    }

    async fn create_oauth_client(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let app_id = app_setting_id(&request.path, "oauth_clients")
            .ok_or_else(|| malformed("Malformed app ID"))?;
        let body = json_body(&request.body)?;
        let provider_id = uuid_value(&body, "provider_id")?;
        let name = non_blank_string(&body, "client_name")?;
        let client_id = optional_string_input(&body, "client_id")?.flatten();
        let secret = optional_string_input(&body, "client_secret")?.flatten();
        let extra_audiences = optional_extra_audiences(&body)?.unwrap_or_default();
        let meta = optional_object(&body, "meta")?;
        let redirect_to = optional_string_input(&body, "redirect_to")?.flatten();
        if let Some(redirect_to) = &redirect_to {
            validate_redirect_url(redirect_to)?;
        }
        let use_shared_credentials = clojure_truthy(body.get("use_shared_credentials"));
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let provider = oauth_provider_by_id(&*client, app_id, provider_id)
            .await
            .map_err(auth_model_error)?
            .ok_or_else(|| record_not_found("oauth-service-provider"))?;
        let discovery_endpoint = if provider.name == "github" {
            None
        } else {
            Some(non_blank_string(&body, "discovery_endpoint")?)
        };
        if let Some(endpoint) = &discovery_endpoint {
            self.validate_oauth_discovery(endpoint).await?;
        }
        if use_shared_credentials {
            self.authorize_shared_credentials(app_id, &provider.name)
                .await?;
        }
        let record_id = Uuid::new_v4();
        let encrypted_client_secret = self.encrypt_oauth_secret(record_id, secret).await?;
        let record = OAuthClientRecord {
            id: record_id,
            provider_id,
            name,
            client_id,
            encrypted_client_secret,
            extra_audiences,
            discovery_endpoint,
            meta,
            redirect_to,
            use_shared_credentials,
        };
        let catalog = system_attribute_catalog().map_err(auth_model_error)?;
        create_oauth_client(&mut client, app_id, &catalog, &record)
            .await
            .map_err(auth_model_error)?;
        let created_at = entity_created_at(&*client, app_id, record.id).await?;
        json_response(json!({"client": oauth_client_json(&record, created_at, false)}))
    }

    async fn update_oauth_client(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, client_record_id) = oauth_client_ids(&request.path)
            .ok_or_else(|| malformed("Malformed app or OAuth client ID"))?;
        let body = json_body(&request.body)?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let existing = oauth_client_by_id(&*client, app_id, client_record_id)
            .await
            .map_err(auth_model_error)?
            .ok_or_else(|| record_not_found("app-oauth-client"))?;
        let provider = oauth_provider_by_id(&*client, app_id, existing.provider_id)
            .await
            .map_err(auth_model_error)?
            .ok_or_else(|| record_not_found("oauth-service-provider"))?;
        let client_id = optional_string_input(&body, "client_id")?;
        let extra_audiences = optional_extra_audiences(&body)?;
        let discovery_endpoint = optional_string_input(&body, "discovery_endpoint")?;
        let redirect_to = optional_string_input(&body, "redirect_to")?;
        if let Some(Some(endpoint)) = &discovery_endpoint {
            self.validate_oauth_discovery(endpoint).await?;
        }
        if let Some(Some(redirect_to)) = &redirect_to {
            validate_redirect_url(redirect_to)?;
        }
        let use_shared_credentials = body
            .get("use_shared_credentials")
            .map(|value| clojure_truthy(Some(value)));
        if use_shared_credentials == Some(true) {
            self.authorize_shared_credentials(app_id, &provider.name)
                .await?;
        }
        let encrypted_client_secret = match optional_string_input(&body, "client_secret")? {
            None => None,
            Some(secret) => Some(self.encrypt_oauth_secret(client_record_id, secret).await?),
        };
        let patch = OAuthClientPatch {
            client_id,
            encrypted_client_secret,
            extra_audiences,
            discovery_endpoint,
            meta: optional_object_patch(&body, "meta")?,
            redirect_to,
            use_shared_credentials,
        };
        let catalog = system_attribute_catalog().map_err(auth_model_error)?;
        update_oauth_client(&mut client, app_id, &catalog, client_record_id, &patch)
            .await
            .map_err(auth_model_error)?;
        let updated = oauth_client_by_id(&*client, app_id, client_record_id)
            .await
            .map_err(auth_model_error)?
            .ok_or_else(|| record_not_found("app-oauth-client"))?;
        let created_at = entity_created_at(&*client, app_id, client_record_id).await?;
        json_response(json!({"client": oauth_client_json(&updated, created_at, true)}))
    }

    async fn delete_oauth_client(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let (app_id, client_record_id) = oauth_client_ids(&request.path)
            .ok_or_else(|| malformed("Malformed app or OAuth client ID"))?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        authorize_app(&*client, &bearer, app_id, Role::Collaborator, true, true).await?;
        let existing = oauth_client_by_id(&*client, app_id, client_record_id)
            .await
            .map_err(auth_model_error)?
            .ok_or_else(|| record_not_found("app-oauth-client"))?;
        let created_at = entity_created_at(&*client, app_id, client_record_id).await?;
        let catalog = system_attribute_catalog().map_err(auth_model_error)?;
        if !delete_oauth_client(&mut client, app_id, &catalog, client_record_id)
            .await
            .map_err(auth_model_error)?
        {
            return Err(record_not_found("app-oauth-client"));
        }
        json_response(json!({"client": oauth_client_json(&existing, created_at, false)}))
    }

    async fn encrypt_oauth_secret(
        &self,
        client_id: Uuid,
        plaintext: Option<String>,
    ) -> Result<Option<String>, ApiError> {
        let Some(plaintext) = plaintext else {
            return Ok(None);
        };
        self.hooks
            .execute(DashboardControlHookOperation::EncryptOAuthSecret {
                client_id,
                plaintext,
            })
            .await?
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| {
                ApiError::new(ApiErrorType::Unknown, "OAuth secret encryption unavailable")
            })
    }

    async fn validate_oauth_discovery(&self, endpoint: &str) -> Result<(), ApiError> {
        let valid = self
            .hooks
            .execute(DashboardControlHookOperation::ValidateOAuthDiscovery {
                endpoint: endpoint.to_owned(),
            })
            .await?
            .as_bool()
            .unwrap_or(false);
        if valid {
            Ok(())
        } else {
            Err(validation("Could not validate discovery endpoint."))
        }
    }

    async fn authorize_shared_credentials(
        &self,
        app_id: Uuid,
        provider_name: &str,
    ) -> Result<(), ApiError> {
        let allowed = self
            .hooks
            .execute(
                DashboardControlHookOperation::AuthorizeSharedOAuthCredentials {
                    app_id,
                    provider_name: provider_name.to_owned(),
                },
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if allowed {
            Ok(())
        } else {
            Err(permission("Shared OAuth credentials are not available"))
        }
    }

    async fn send_invite(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let target = team_target(&request.path, "invite/send")
            .ok_or_else(|| malformed("Malformed team target"))?;
        let body = json_body(&request.body)?;
        let invitee_email = email_value(&body, "invitee-email")?;
        let role = member_role_value(&body)?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let (inviter_id, inviter_role) =
            authorize_team(&*client, &bearer, target, Role::Admin).await?;
        if role == "owner" && inviter_role < Role::Owner {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Only an owner may invite another owner",
            )
            .with_status(StatusCode::FORBIDDEN));
        }
        let sender_email: String = client
            .query_one(
                "SELECT email FROM instant_users WHERE id = $1",
                &[&inviter_id],
            )
            .await
            .map_err(database_error)?
            .get(0);
        let title: String = match target {
            TeamTarget::App(app_id) => client
                .query_one("SELECT title FROM apps WHERE id = $1", &[&app_id])
                .await
                .map_err(database_error)?
                .get(0),
            TeamTarget::Org(org_id) => client
                .query_one("SELECT title FROM orgs WHERE id = $1", &[&org_id])
                .await
                .map_err(database_error)?
                .get(0),
        };
        let record_id = Uuid::new_v4();
        match target {
            TeamTarget::App(app_id) => {
                client
                    .execute(
                        "INSERT INTO app_member_invites
                         (id, app_id, inviter_id, invitee_email, invitee_role, status, sent_at)
                         VALUES ($1, $2, $3, $4, $5, 'pending', now())
                         ON CONFLICT (app_id, invitee_email) DO UPDATE
                         SET status = 'pending', sent_at = now(), invitee_role = EXCLUDED.invitee_role",
                        &[&record_id, &app_id, &inviter_id, &invitee_email, &role],
                    )
                    .await
                    .map_err(database_error)?;
            }
            TeamTarget::Org(org_id) => {
                client
                    .execute(
                        "INSERT INTO org_member_invites
                         (id, org_id, inviter_id, invitee_email, invitee_role, status, sent_at)
                         VALUES ($1, $2, $3, $4, $5, 'pending', now())
                         ON CONFLICT (org_id, invitee_email) DO UPDATE
                         SET status = 'pending', sent_at = now(), invitee_role = EXCLUDED.invitee_role",
                        &[&record_id, &org_id, &inviter_id, &invitee_email, &role],
                    )
                    .await
                    .map_err(database_error)?;
            }
        }
        drop(client);
        self.hooks
            .execute(DashboardControlHookOperation::DeliverInvite {
                target: target.name(),
                foreign_key: target.id(),
                inviter_id,
                inviter_email: sender_email,
                invitee_email,
                title,
                role,
            })
            .await?;
        json_response(json!({}))
    }

    async fn accept_invite(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let invite_id = uuid_value(&json_body(&request.body)?, "invite-id")?;
        let bearer = bearer(&request)?;
        let mut client = self.acquire_client().await?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let principal = authenticate_refresh(&transaction, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        let user_email: String = transaction
            .query_one("SELECT email FROM instant_users WHERE id = $1", &[&user_id])
            .await
            .map_err(database_error)?
            .get(0);
        let invite = load_invite(&transaction, invite_id).await?;
        if invite.invitee_email != user_email {
            return Err(permission("Only the invitee may accept this invitation"));
        }
        if invite.status == "revoked" {
            return Err(permission("This invitation has been revoked"));
        }
        let updated = match invite.target {
            TeamTarget::App(_) => {
                transaction
                    .execute(
                        "UPDATE app_member_invites SET status = 'accepted'
                     WHERE id = $1 AND status = 'pending' AND sent_at >= now() - interval '3 days'",
                        &[&invite_id],
                    )
                    .await
            }
            TeamTarget::Org(_) => {
                transaction
                    .execute(
                        "UPDATE org_member_invites SET status = 'accepted'
                     WHERE id = $1 AND status = 'pending' AND sent_at >= now() - interval '3 days'",
                        &[&invite_id],
                    )
                    .await
            }
        }
        .map_err(database_error)?;
        if updated == 0 {
            return Err(record_not_found("member-invite"));
        }
        match invite.target {
            TeamTarget::App(app_id) if invite.role == "creator" => {
                transaction
                    .execute(
                        "UPDATE apps SET creator_id = $1 WHERE id = $2",
                        &[&user_id, &app_id],
                    )
                    .await
                    .map_err(database_error)?;
            }
            TeamTarget::App(app_id) => {
                transaction
                    .execute(
                        "INSERT INTO app_members (id, app_id, user_id, member_role)
                         VALUES ($1, $2, $3, $4)",
                        &[&Uuid::new_v4(), &app_id, &user_id, &invite.role],
                    )
                    .await
                    .map_err(database_error)?;
            }
            TeamTarget::Org(org_id) => {
                transaction
                    .execute(
                        "INSERT INTO org_members (id, org_id, user_id, role)
                         VALUES ($1, $2, $3, $4)",
                        &[&Uuid::new_v4(), &org_id, &user_id, &invite.role],
                    )
                    .await
                    .map_err(database_error)?;
            }
        }
        transaction.commit().await.map_err(database_error)?;
        json_response(json!({}))
    }

    async fn decline_invite(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let invite_id = uuid_value(&json_body(&request.body)?, "invite-id")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let principal = authenticate_refresh(&*client, &bearer).await?;
        let user_id = principal
            .user_id
            .expect("refresh tokens always have a user");
        let user_email: String = client
            .query_one("SELECT email FROM instant_users WHERE id = $1", &[&user_id])
            .await
            .map_err(database_error)?
            .get(0);
        let invite = load_invite(&*client, invite_id).await?;
        if invite.invitee_email != user_email {
            return Err(permission("Only the invitee may decline this invitation"));
        }
        update_invite_status(&*client, invite.target, invite_id, None).await?;
        json_response(json!({}))
    }

    async fn revoke_invite(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let target = team_target(&request.path, "invite/revoke")
            .ok_or_else(|| malformed("Malformed team target"))?;
        let invite_id = uuid_value(&json_body(&request.body)?, "invite-id")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        authorize_team(&*client, &bearer, target, Role::Admin).await?;
        update_invite_status(&*client, target, invite_id, Some(target.id())).await?;
        json_response(json!({}))
    }

    async fn remove_member(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        self.mutate_member(request, None).await
    }

    async fn update_member(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let body = json_body(&request.body)?;
        let new_role = member_role_value(&body)?;
        let new_role = role(&new_role).expect("member role validation returns a known role");
        self.mutate_member_with_body(request, body, Some(new_role))
            .await
    }

    async fn mutate_member(
        &self,
        request: DashboardRequest,
        new_role: Option<Role>,
    ) -> Result<DashboardResponse, ApiError> {
        let body = json_body(&request.body)?;
        self.mutate_member_with_body(request, body, new_role).await
    }

    async fn mutate_member_with_body(
        &self,
        request: DashboardRequest,
        body: Value,
        new_role: Option<Role>,
    ) -> Result<DashboardResponse, ApiError> {
        let suffix = if new_role.is_some() {
            "members/update"
        } else {
            "members/remove"
        };
        let target =
            team_target(&request.path, suffix).ok_or_else(|| malformed("Malformed team target"))?;
        let member_id = uuid_value(&body, "id")?;
        let bearer = bearer(&request)?;
        let client = self.acquire_client().await?;
        let required = if new_role.is_some() {
            Role::Admin
        } else {
            Role::Collaborator
        };
        let (_, actor_role) = authorize_team(&*client, &bearer, target, required).await?;
        let member_role = load_member_role(&*client, target, member_id).await?;
        if actor_role < member_role || new_role.is_some_and(|role| actor_role < role) {
            return Err(permission("Insufficient role to modify this member"));
        }
        mutate_member_row(&*client, target, member_id, new_role).await?;
        json_response(json!({}))
    }
}

#[must_use]
#[allow(clippy::too_many_lines)] // Mirrors the authoritative method/path declaration matrix.
pub fn is_control_route(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        (
            "POST",
            "/dash/apps"
                | "/dash/orgs"
                | "/dash/apps/ephemeral"
                | "/dash/apps/get_a_db"
                | "/dash/restores/zip"
                | "/dash/oauth/token"
        )
    ) || method == "GET" && path == "/dash/restore-jobs"
        || method == "GET" && path == "/dash/stats/active_sessions"
        || method == "GET"
            && matches!(
                path,
                "/dash/admin"
                    | "/dash/check-admin"
                    | "/dash/top"
                    | "/dash/storage"
                    | "/dash/overview/daily"
                    | "/dash/overview/minute"
                    | "/dash/oauth/start"
                    | "/dash/oauth/callback"
            )
        || matches!(method, "GET" | "DELETE") && restore_job_id(path).is_some()
        || matches!(method, "GET" | "DELETE")
            && (direct_app_id(path).is_some() || direct_org_id(path).is_some())
        || method == "POST"
            && (action_app_id(path, "clear").is_some()
                || action_app_id(path, "track-import").is_some()
                || action_app_id(path, "tokens").is_some()
                || app_setting_id(path, "tokens/rotate").is_some()
                || action_app_id(path, "rename").is_some()
                || action_app_id(path, "status").is_some()
                || action_app_id(path, "set-magic-code-expiry").is_some()
                || action_app_id(path, "rules").is_some()
                || action_org_id(path, "rename").is_some()
                || transfer_ids(path).is_some()
                || claim_app_id(path).is_some()
                || app_setting_id(path, "authorized_redirect_origins").is_some()
                || app_setting_id(path, "oauth_service_providers").is_some()
                || app_setting_id(path, "oauth_clients").is_some()
                || app_setting_id(path, "test_users").is_some()
                || app_setting_id(path, "indexing-jobs").is_some()
                || app_setting_id(path, "schema/push/plan").is_some()
                || app_setting_id(path, "schema/push/apply").is_some()
                || app_setting_id(path, "schema/steps/apply").is_some()
                || app_setting_id(path, "storage/files/delete").is_some()
                || app_setting_id(path, "backups").is_some()
                || app_setting_id(path, "webhooks").is_some()
                || backup_action_ids(path, "restore").is_some()
                || webhook_ids(path).is_some()
                || webhook_action_ids(path, "enable").is_some()
                || webhook_action_ids(path, "disable").is_some()
                || webhook_event_ids(path).is_some()
                || oauth_client_ids(path).is_some()
                || team_target(path, "invite/send").is_some()
                || team_target(path, "members/update").is_some()
                || path == "/dash/invites/accept"
                || path == "/dash/invites/decline"
                || special_app_id(path, true, Some("status")).is_some())
        || method == "DELETE"
            && (team_target(path, "invite/revoke").is_some()
                || team_target(path, "members/remove").is_some()
                || authorized_origin_ids(path).is_some()
                || oauth_client_ids(path).is_some()
                || webhook_ids(path).is_some())
        || method == "DELETE" && (backup_ids(path).is_some() || backup_job_ids(path).is_some())
        || method == "GET" && action_app_id(path, "soft_deleted_attrs").is_some()
        || method == "GET" && action_app_id(path, "stats").is_some()
        || method == "GET" && action_app_id(path, "usage").is_some()
        || method == "GET" && path == "/dash/apps-summary"
        || method == "GET" && path == "/dash/operator/flags"
        || matches!(method, "PUT" | "DELETE") && operator_flag_name(path).is_some()
        || method == "GET" && path == "/dash/session_counts"
        || method == "GET" && path == "/dash/default-email-template"
        || method == "GET"
            && (app_setting_id(path, "sender-verification").is_some()
                || app_setting_id(path, "email_status").is_some())
        || method == "POST"
            && (app_setting_id(path, "sender-verification/send-magic-code").is_some()
                || app_setting_id(path, "sender-verification/verify-magic-code").is_some()
                || app_setting_id(path, "email_templates").is_some()
                || app_setting_id(path, "send-test-email").is_some())
        || method == "DELETE" && email_template_ids(path).is_some()
        || matches!(method, "GET" | "POST") && app_setting_id(path, "oauth-apps").is_some()
        || matches!(method, "POST" | "DELETE") && oauth_app_ids(path).is_some()
        || method == "POST" && oauth_app_client_collection_ids(path).is_some()
        || matches!(method, "POST" | "DELETE") && oauth_app_client_ids(path).is_some()
        || method == "POST" && oauth_app_client_secret_collection_ids(path).is_some()
        || method == "DELETE" && oauth_app_client_secret_ids(path).is_some()
        || method == "GET" && path == "/dash/user/oauth_apps"
        || method == "POST" && path == "/dash/user/oauth_apps/revoke_access"
        || method == "PUT" && app_setting_id(path, "storage/upload").is_some()
        || method == "GET" && action_app_id(path, "rule-versions").is_some()
        || method == "POST" && app_setting_id(path, "rules/validate").is_some()
        || matches!(method, "GET" | "PUT") && app_setting_id(path, "backup-schedule").is_some()
        || method == "GET" && app_setting_id(path, "auth").is_some()
        || method == "GET" && app_setting_id(path, "perms/pull").is_some()
        || method == "GET" && app_setting_id(path, "schema/pull").is_some()
        || method == "GET" && app_setting_id(path, "webhooks").is_some()
        || method == "GET" && webhook_action_ids(path, "events").is_some()
        || method == "GET" && webhook_event_ids(path).is_some()
        || method == "GET"
            && (app_setting_id(path, "backups").is_some()
                || app_setting_id(path, "backup-jobs").is_some()
                || app_setting_id(path, "restore-jobs").is_some()
                || backup_ids(path).is_some()
                || backup_job_ids(path).is_some()
                || backup_action_ids(path, "files").is_some()
                || backup_action_ids(path, "file-url").is_some()
                || backup_action_ids(path, "storage-files").is_some())
        || matches!(method, "GET" | "DELETE") && app_setting_id(path, "test_users").is_some()
        || method == "GET"
            && (indexing_job_ids(path).is_some() || indexing_job_group_ids(path).is_some())
        || method == "GET"
            && (special_app_id(path, true, None).is_some()
                || special_app_id(path, false, None).is_some())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TeamTarget {
    App(Uuid),
    Org(Uuid),
}

impl TeamTarget {
    const fn id(self) -> Uuid {
        match self {
            Self::App(id) | Self::Org(id) => id,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::App(_) => "app",
            Self::Org(_) => "org",
        }
    }
}

struct InviteRecord {
    target: TeamTarget,
    invitee_email: String,
    role: String,
    status: String,
}

async fn authorize_team<C>(
    client: &C,
    bearer: &str,
    target: TeamTarget,
    required: Role,
) -> Result<(Uuid, Role), ApiError>
where
    C: GenericClient + Sync,
{
    match target {
        TeamTarget::App(app_id) => {
            // Authenticate management tokens so an app admin token fails the
            // dashboard-user check below instead of looking like a bad token.
            let principal = authorize_app(client, bearer, app_id, required, true, true).await?;
            let Some(user_id) = principal.user_id else {
                // An app admin token authorizes app data but has no dashboard
                // user, so it must not reach invite/bookkeeping paths.
                return Err(ApiError::new(
                    ApiErrorType::PermissionDenied,
                    "Dashboard user token required",
                )
                .with_status(StatusCode::FORBIDDEN));
            };
            Ok((
                user_id,
                principal.role.expect("authorized app access has a role"),
            ))
        }
        TeamTarget::Org(org_id) => {
            let principal = authenticate_refresh(client, bearer).await?;
            let user_id = principal
                .user_id
                .expect("refresh tokens always have a user");
            let actual = org_role(client, user_id, org_id)
                .await?
                .ok_or_else(|| permission("Insufficient organization member role"))?;
            if actual < required {
                return Err(permission("Insufficient organization member role"));
            }
            Ok((user_id, actual))
        }
    }
}

async fn load_invite<C>(client: &C, id: Uuid) -> Result<InviteRecord, ApiError>
where
    C: GenericClient + Sync,
{
    if let Some(row) = client
        .query_opt(
            "SELECT app_id, invitee_email, invitee_role, status
             FROM app_member_invites WHERE id = $1",
            &[&id],
        )
        .await
        .map_err(database_error)?
    {
        return Ok(InviteRecord {
            target: TeamTarget::App(row.get(0)),
            invitee_email: row.get(1),
            role: row.get(2),
            status: row.get(3),
        });
    }
    client
        .query_opt(
            "SELECT org_id, invitee_email, invitee_role, status
             FROM org_member_invites WHERE id = $1",
            &[&id],
        )
        .await
        .map_err(database_error)?
        .map(|row| InviteRecord {
            target: TeamTarget::Org(row.get(0)),
            invitee_email: row.get(1),
            role: row.get(2),
            status: row.get(3),
        })
        .ok_or_else(|| record_not_found("member-invite"))
}

async fn update_invite_status<C>(
    client: &C,
    target: TeamTarget,
    invite_id: Uuid,
    foreign_key: Option<Uuid>,
) -> Result<(), ApiError>
where
    C: GenericClient + Sync,
{
    let changed = match target {
        TeamTarget::App(app_id) => {
            client
                .execute(
                    "UPDATE app_member_invites SET status = 'revoked'
                 WHERE id = $1 AND app_id = $2 AND status = 'pending'",
                    &[&invite_id, &foreign_key.unwrap_or(app_id)],
                )
                .await
        }
        TeamTarget::Org(org_id) => {
            client
                .execute(
                    "UPDATE org_member_invites SET status = 'revoked'
                 WHERE id = $1 AND org_id = $2 AND status = 'pending'",
                    &[&invite_id, &foreign_key.unwrap_or(org_id)],
                )
                .await
        }
    }
    .map_err(database_error)?;
    if changed == 0 {
        Err(record_not_found("member-invite"))
    } else {
        Ok(())
    }
}

async fn load_member_role<C>(
    client: &C,
    target: TeamTarget,
    member_id: Uuid,
) -> Result<Role, ApiError>
where
    C: GenericClient + Sync,
{
    let row = match target {
        TeamTarget::App(app_id) => {
            client
                .query_opt(
                    "SELECT member_role FROM app_members WHERE id = $1 AND app_id = $2",
                    &[&member_id, &app_id],
                )
                .await
        }
        TeamTarget::Org(org_id) => {
            client
                .query_opt(
                    "SELECT role FROM org_members WHERE id = $1 AND org_id = $2",
                    &[&member_id, &org_id],
                )
                .await
        }
    }
    .map_err(database_error)?
    .ok_or_else(|| record_not_found("member"))?;
    role(row.get::<_, String>(0).as_str()).ok_or_else(|| validation("Invalid member role"))
}

async fn mutate_member_row<C>(
    client: &C,
    target: TeamTarget,
    member_id: Uuid,
    new_role: Option<Role>,
) -> Result<(), ApiError>
where
    C: GenericClient + Sync,
{
    let changed = match (target, new_role) {
        (TeamTarget::App(app_id), Some(role)) => {
            client
                .execute(
                    "UPDATE app_members SET member_role = $1 WHERE id = $2 AND app_id = $3",
                    &[&role.name(), &member_id, &app_id],
                )
                .await
        }
        (TeamTarget::Org(org_id), Some(role)) => {
            client
                .execute(
                    "UPDATE org_members SET role = $1 WHERE id = $2 AND org_id = $3",
                    &[&role.name(), &member_id, &org_id],
                )
                .await
        }
        (TeamTarget::App(app_id), None) => {
            client
                .execute(
                    "DELETE FROM app_members WHERE id = $1 AND app_id = $2",
                    &[&member_id, &app_id],
                )
                .await
        }
        (TeamTarget::Org(org_id), None) => {
            client
                .execute(
                    "DELETE FROM org_members WHERE id = $1 AND org_id = $2",
                    &[&member_id, &org_id],
                )
                .await
        }
    }
    .map_err(database_error)?;
    if changed == 0 {
        Err(record_not_found("member"))
    } else {
        Ok(())
    }
}

async fn authenticate<C>(client: &C, bearer: &str, write: bool) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    let required_scope = if write { "apps-write" } else { "apps-read" };
    authenticate_scoped(client, bearer, required_scope).await
}

async fn authenticate_scoped<C>(
    client: &C,
    bearer: &str,
    required_scope: &str,
) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    if let Ok(token) = Uuid::parse_str(bearer) {
        if let Some(row) = client
            .query_opt(LOAD_USER_SQL, &[&hash_token(token)])
            .await
            .map_err(database_error)?
        {
            return Ok(Principal {
                user_id: Some(row.get(0)),
                admin_app_id: None,
                role: None,
                admin_scope: false,
            });
        }
        if let Some(row) = client
            .query_opt(
                "SELECT app_id FROM app_admin_tokens WHERE token = $1",
                &[&token],
            )
            .await
            .map_err(database_error)?
        {
            return Ok(Principal {
                user_id: None,
                admin_app_id: Some(row.get(0)),
                role: None,
                admin_scope: false,
            });
        }
    }
    let digest = Sha256::digest(bearer.as_bytes()).to_vec();
    if bearer.starts_with("per_") {
        if let Some(row) = client
            .query_opt(
                "SELECT user_id FROM instant_personal_access_tokens WHERE lookup_key = $1",
                &[&digest],
            )
            .await
            .map_err(database_error)?
        {
            return Ok(Principal {
                user_id: Some(row.get(0)),
                admin_app_id: None,
                role: None,
                admin_scope: false,
            });
        }
    } else if bearer.starts_with("pat_")
        && let Some(row) = client
            .query_opt(
                "SELECT user_id, scopes FROM instant_user_oauth_access_tokens
                 WHERE lookup_key = $1 AND expires_at > now()",
                &[&digest],
            )
            .await
            .map_err(database_error)?
    {
        let scopes: Vec<String> = row.get(1);
        let write_equivalent = required_scope
            .strip_suffix("-read")
            .map(|prefix| format!("{prefix}-write"));
        let allowed = scopes
            .iter()
            .any(|scope| scope == required_scope || write_equivalent.as_ref() == Some(scope));
        if !allowed {
            return Err(permission(
                "OAuth access token is missing the required scope",
            ));
        }
        return Ok(Principal {
            user_id: Some(row.get(0)),
            admin_app_id: None,
            role: None,
            admin_scope: scopes.iter().any(|scope| scope == "apps-write"),
        });
    }
    Err(record_not_found("token").unauthorized())
}

async fn authenticate_management_user<C>(
    client: &C,
    bearer: &str,
    write: bool,
) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    if bearer.starts_with("pat_") {
        return authenticate(client, bearer, write).await;
    }

    // Personal access tokens predate the `per_` prefix. The JVM accepts both
    // forms on management routes, but never refresh sessions or app tokens.
    let digest = Sha256::digest(bearer.as_bytes()).to_vec();
    client
        .query_opt(
            "SELECT user_id FROM instant_personal_access_tokens WHERE lookup_key = $1",
            &[&digest],
        )
        .await
        .map_err(database_error)?
        .map(|row| Principal {
            user_id: Some(row.get(0)),
            admin_app_id: None,
            role: None,
            admin_scope: false,
        })
        .ok_or_else(|| record_not_found("token").unauthorized())
}

async fn authenticate_refresh<C>(client: &C, bearer: &str) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    let token = Uuid::parse_str(bearer).map_err(|_| record_not_found("token").unauthorized())?;
    client
        .query_opt(LOAD_USER_SQL, &[&hash_token(token)])
        .await
        .map_err(database_error)?
        .map(|row| Principal {
            user_id: Some(row.get(0)),
            admin_app_id: None,
            role: None,
            admin_scope: false,
        })
        .ok_or_else(|| record_not_found("token").unauthorized())
}

/// Loads the source app and backup rows for an owner-level backup restore.
async fn backup_restore_source<C>(
    client: &C,
    app_id: Uuid,
    backup_id: Uuid,
) -> Result<(Option<Uuid>, Option<Uuid>, String, String), ApiError>
where
    C: GenericClient + Sync,
{
    let source = client
        .query_opt(
            "SELECT creator_id, org_id, title FROM apps
             WHERE id = $1 AND deletion_marked_at IS NULL",
            &[&app_id],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("app"))?;
    let backup = client
        .query_opt(
            "SELECT storage_prefix FROM app_backups
             WHERE id = $1 AND app_id = $2 AND deletion_marked_at IS NULL
               AND (expires_at IS NULL OR expires_at > now())",
            &[&backup_id, &app_id],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("app-backup"))?;
    Ok((
        source.get::<_, Option<Uuid>>(0),
        source.get::<_, Option<Uuid>>(1),
        source.get::<_, String>(2),
        backup.get::<_, String>(0),
    ))
}

async fn authorize_app<C>(
    client: &C,
    bearer: &str,
    app_id: Uuid,
    required: Role,
    write: bool,
    allow_management_token: bool,
) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    let scope = if write { "apps-write" } else { "apps-read" };
    authorize_app_scoped(
        client,
        bearer,
        app_id,
        required,
        allow_management_token,
        scope,
    )
    .await
}

async fn authorize_app_scoped<C>(
    client: &C,
    bearer: &str,
    app_id: Uuid,
    required: Role,
    allow_management_token: bool,
    management_scope: &str,
) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    let mut principal = if allow_management_token {
        authenticate_scoped(client, bearer, management_scope).await?
    } else {
        authenticate_refresh(client, bearer).await?
    };
    if let Some(admin_app_id) = principal.admin_app_id {
        if admin_app_id == app_id {
            principal.role = Some(Role::Owner);
            return Ok(principal);
        }
        return Err(validation(
            "Admin token does not belong to the requested app",
        ));
    }
    let user_id = principal
        .user_id
        .ok_or_else(|| permission("User token required"))?;
    let row = load_app(client, app_id).await?;
    let creator_id: Option<Uuid> = row.get(1);
    let org_id: Option<Uuid> = row.get(2);
    let app_role = if creator_id == Some(user_id) {
        Some(Role::Owner)
    } else if let Some(row) = client
        .query_opt(
            "SELECT member_role FROM app_members WHERE app_id = $1 AND user_id = $2",
            &[&app_id, &user_id],
        )
        .await
        .map_err(database_error)?
    {
        role(row.get::<_, String>(0).as_str())
    } else {
        None
    };
    let organization_role = if let Some(org_id) = org_id {
        org_role(client, user_id, org_id).await?
    } else {
        None
    };
    let role = app_role.max(organization_role);
    if role.is_some_and(|role| role >= required) {
        principal.role = role;
        Ok(principal)
    } else {
        Err(permission("Insufficient app member role"))
    }
}

/// Email templates and sender settings are dashboard-user operations. An app
/// admin token authorizes app data, but must never configure outbound email.
async fn authorize_dashboard_user_scoped<C>(
    client: &C,
    bearer: &str,
    app_id: Uuid,
    required: Role,
    management_scope: &str,
) -> Result<Principal, ApiError>
where
    C: GenericClient + Sync,
{
    let principal =
        authorize_app_scoped(client, bearer, app_id, required, true, management_scope).await?;
    if principal.user_id.is_none() {
        return Err(ApiError::new(
            ApiErrorType::PermissionDenied,
            "Dashboard user token required",
        )
        .with_status(StatusCode::FORBIDDEN));
    }
    Ok(principal)
}

async fn require_org_role<C>(
    client: &C,
    user_id: Uuid,
    org_id: Uuid,
    required: Role,
) -> Result<(), ApiError>
where
    C: GenericClient + Sync,
{
    if org_role(client, user_id, org_id)
        .await?
        .is_some_and(|role| role >= required)
    {
        Ok(())
    } else {
        Err(permission("Insufficient organization member role"))
    }
}

async fn require_org_or_app_member<C>(
    client: &C,
    user_id: Uuid,
    org_id: Uuid,
) -> Result<(), ApiError>
where
    C: GenericClient + Sync,
{
    if org_role(client, user_id, org_id)
        .await?
        .is_some_and(|role| role >= Role::Collaborator)
    {
        return Ok(());
    }
    let app_member = client
        .query_one(
            "SELECT EXISTS(
               SELECT 1 FROM apps app
               JOIN app_members member ON member.app_id = app.id
               WHERE app.org_id = $1 AND app.deletion_marked_at IS NULL
                 AND member.user_id = $2
             )",
            &[&org_id, &user_id],
        )
        .await
        .map_err(database_error)?
        .get::<_, bool>(0);
    if app_member {
        Ok(())
    } else {
        Err(permission("Insufficient organization member role"))
    }
}

async fn org_role<C>(client: &C, user_id: Uuid, org_id: Uuid) -> Result<Option<Role>, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_opt(
            "SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2",
            &[&org_id, &user_id],
        )
        .await
        .map_err(database_error)
        .map(|row| row.and_then(|row| role(row.get::<_, String>(0).as_str())))
}

fn role(value: &str) -> Option<Role> {
    match value {
        "collaborator" => Some(Role::Collaborator),
        "admin" => Some(Role::Admin),
        "owner" | "creator" => Some(Role::Owner),
        _ => None,
    }
}

async fn load_app<C>(client: &C, app_id: Uuid) -> Result<tokio_postgres::Row, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_opt(
            "SELECT id, creator_id, org_id, title, created_at, status, magic_code_expiry_minutes
             FROM apps WHERE id = $1 AND deletion_marked_at IS NULL",
            &[&app_id],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("app"))
}

/// The app admin token grants full data-plane access, so only callers that can
/// already administer the app may read it back.
fn can_see_admin_token(principal: &Principal) -> bool {
    principal.admin_scope || principal.role.is_some_and(|role| role >= Role::Admin)
}

fn app_json(row: &tokio_postgres::Row, admin_token: Option<Uuid>) -> Value {
    let mut app = json!({
        "id": row.get::<_, Uuid>(0),
        "creator_id": row.get::<_, Option<Uuid>>(1),
        "org_id": row.get::<_, Option<Uuid>>(2),
        "title": row.get::<_, String>(3),
        "created_at": row.get::<_, NaiveDateTime>(4),
        "status": row.get::<_, String>(5),
        "magic_code_expiry_minutes": row.get::<_, Option<i32>>(6),
    });
    if let Some(admin_token) = admin_token {
        app["admin_token"] = json!(admin_token);
    }
    app
}

fn special_app_json(row: &tokio_postgres::Row, admin_token: Uuid) -> Value {
    let mut app = app_json(row, None);
    app["admin-token"] = json!(admin_token);
    app
}

fn org_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "title": row.get::<_, String>(1),
        "created_at": row.get::<_, DateTime<Utc>>(2),
        "updated_at": row.get::<_, DateTime<Utc>>(3),
    })
}

fn direct_app_id(path: &str) -> Option<Uuid> {
    direct_id(path, "/dash/apps/")
}

fn direct_org_id(path: &str) -> Option<Uuid> {
    direct_id(path, "/dash/orgs/")
}

fn direct_id(path: &str, prefix: &str) -> Option<Uuid> {
    let value = path.strip_prefix(prefix)?;
    (!value.contains('/'))
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}

fn operator_flag_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix("/dash/operator/flags/")?;
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

fn action_app_id(path: &str, action: &str) -> Option<Uuid> {
    action_id(path, "/dash/apps/", action)
}

fn action_org_id(path: &str, action: &str) -> Option<Uuid> {
    action_id(path, "/dash/orgs/", action)
}

fn action_id(path: &str, prefix: &str, action: &str) -> Option<Uuid> {
    let value = path.strip_prefix(prefix)?;
    let id = value.strip_suffix(&format!("/{action}"))?;
    (!id.contains('/'))
        .then(|| Uuid::parse_str(id).ok())
        .flatten()
}

fn transfer_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, org_id) = value.split_once("/transfer_to_org/")?;
    Some((Uuid::parse_str(app_id).ok()?, Uuid::parse_str(org_id).ok()?))
}

fn claim_app_id(path: &str) -> Option<Uuid> {
    let value = path.strip_prefix("/dash/apps/")?;
    let value = value
        .strip_prefix("ephemeral/")
        .unwrap_or(value)
        .strip_suffix("/claim")?;
    (!value.contains('/'))
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}

fn special_app_id(path: &str, ephemeral: bool, suffix: Option<&str>) -> Option<Uuid> {
    let kind = if ephemeral { "ephemeral" } else { "get_a_db" };
    let value = path.strip_prefix(&format!("/dash/apps/{kind}/"))?;
    let id = match suffix {
        Some(suffix) => value.strip_suffix(&format!("/{suffix}"))?,
        None => value,
    };
    (!id.contains('/'))
        .then(|| Uuid::parse_str(id).ok())
        .flatten()
}

fn special_expiry_ms(created_at: NaiveDateTime) -> i64 {
    created_at.and_utc().timestamp_millis() + 14 * 24 * 60 * 60 * 1_000
}

fn app_setting_id(path: &str, setting: &str) -> Option<Uuid> {
    let value = path.strip_prefix("/dash/apps/")?;
    let id = value.strip_suffix(&format!("/{setting}"))?;
    (!id.contains('/'))
        .then(|| Uuid::parse_str(id).ok())
        .flatten()
}

fn authorized_origin_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, origin_id) = value.split_once("/authorized_redirect_origins/")?;
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(origin_id).ok()?,
    ))
}

fn oauth_client_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, client_id) = value.split_once("/oauth_clients/")?;
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(client_id).ok()?,
    ))
}

fn email_template_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, template_id) = value.split_once("/email_templates/")?;
    if template_id.contains('/') {
        return None;
    }
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(template_id).ok()?,
    ))
}

fn oauth_app_ids(path: &str) -> Option<(Uuid, Uuid)> {
    two_uuid_path(path, "/dash/apps/", "/oauth-apps/")
}

fn oauth_app_client_collection_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_suffix("/clients")?;
    oauth_app_ids(value)
}

fn oauth_app_client_ids(path: &str) -> Option<(Uuid, Uuid)> {
    two_uuid_path(path, "/dash/apps/", "/oauth-app-clients/")
}

fn oauth_app_client_secret_collection_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_suffix("/client-secrets")?;
    oauth_app_client_ids(value)
}

fn oauth_app_client_secret_ids(path: &str) -> Option<(Uuid, Uuid)> {
    two_uuid_path(path, "/dash/apps/", "/oauth-app-client-secrets/")
}

fn two_uuid_path(path: &str, prefix: &str, separator: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix(prefix)?;
    let (left, right) = value.split_once(separator)?;
    if right.contains('/') {
        return None;
    }
    Some((Uuid::parse_str(left).ok()?, Uuid::parse_str(right).ok()?))
}

fn indexing_job_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, job_id) = value.split_once("/indexing-jobs/")?;
    if job_id.starts_with("group/") || job_id.contains('/') {
        return None;
    }
    Some((Uuid::parse_str(app_id).ok()?, Uuid::parse_str(job_id).ok()?))
}

fn indexing_job_group_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, group_id) = value.split_once("/indexing-jobs/group/")?;
    if group_id.contains('/') {
        return None;
    }
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(group_id).ok()?,
    ))
}

fn backup_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, backup_id) = value.split_once("/backups/")?;
    if backup_id.contains('/') {
        return None;
    }
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(backup_id).ok()?,
    ))
}

fn backup_action_ids(path: &str, action: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, rest) = value.split_once("/backups/")?;
    let backup_id = rest.strip_suffix(&format!("/{action}"))?;
    if backup_id.contains('/') {
        return None;
    }
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(backup_id).ok()?,
    ))
}

fn backup_job_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, job_id) = value.split_once("/backup-jobs/")?;
    if job_id.contains('/') {
        return None;
    }
    Some((Uuid::parse_str(app_id).ok()?, Uuid::parse_str(job_id).ok()?))
}

fn webhook_ids(path: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, webhook_id) = value.split_once("/webhooks/")?;
    if webhook_id.contains('/') {
        return None;
    }
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(webhook_id).ok()?,
    ))
}

fn webhook_action_ids(path: &str, action: &str) -> Option<(Uuid, Uuid)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, rest) = value.split_once("/webhooks/")?;
    let webhook_id = rest.strip_suffix(&format!("/{action}"))?;
    if webhook_id.contains('/') {
        return None;
    }
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(webhook_id).ok()?,
    ))
}

fn webhook_event_ids(path: &str) -> Option<(Uuid, Uuid, instant_wal::Isn)> {
    let value = path.strip_prefix("/dash/apps/")?;
    let (app_id, rest) = value.split_once("/webhooks/")?;
    let (webhook_id, isn) = rest.split_once("/events/")?;
    Some((
        Uuid::parse_str(app_id).ok()?,
        Uuid::parse_str(webhook_id).ok()?,
        isn.parse().ok()?,
    ))
}

fn team_target(path: &str, suffix: &str) -> Option<TeamTarget> {
    let value = path.strip_suffix(&format!("/{suffix}"))?;
    if let Some(id) = value.strip_prefix("/dash/apps/") {
        return Uuid::parse_str(id).ok().map(TeamTarget::App);
    }
    value
        .strip_prefix("/dash/orgs/")
        .and_then(|id| Uuid::parse_str(id).ok())
        .map(TeamTarget::Org)
}

fn bearer(request: &DashboardRequest) -> Result<String, ApiError> {
    optional_bearer(request)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| malformed("Malformed dashboard bearer token").unauthorized())
}

fn optional_bearer(request: &DashboardRequest) -> Option<&str> {
    request
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("Bearer").then_some(token)
        })
}

fn json_body(body: &[u8]) -> Result<Value, ApiError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| malformed("Malformed JSON body"))?;
    value
        .is_object()
        .then_some(value)
        .ok_or_else(|| malformed("Expected a JSON object"))
}

fn required_string<'a>(body: &'a Value, key: &'static str) -> Result<&'a str, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| missing(key))
}

fn non_blank_string(body: &Value, key: &'static str) -> Result<String, ApiError> {
    let value = body.get(key).ok_or_else(|| missing(key))?;
    let value = match value {
        Value::String(value) => value.trim().to_owned(),
        Value::Number(value) => value.to_string(),
        _ => return Err(malformed("Malformed string parameter")),
    };
    (!value.is_empty())
        .then_some(value)
        .ok_or_else(|| malformed("Malformed string parameter"))
}

#[allow(clippy::option_option)]
fn optional_string_input(
    body: &Value,
    key: &'static str,
) -> Result<Option<Option<String>>, ApiError> {
    let Some(value) = body.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(None));
    }
    let value = match value {
        Value::String(value) => value.trim().to_owned(),
        Value::Number(value) => value.to_string(),
        _ => return Err(malformed("Malformed string parameter")),
    };
    Ok(Some((!value.is_empty()).then_some(value)))
}

fn optional_object(body: &Value, key: &'static str) -> Result<Option<Value>, ApiError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) => Ok(Some(Value::Object(value.clone()))),
        Some(_) => Err(malformed("Malformed object parameter")),
    }
}

#[allow(clippy::option_option)]
fn optional_object_patch(
    body: &Value,
    key: &'static str,
) -> Result<Option<Option<Value>>, ApiError> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::Object(value)) => Ok(Some(Some(Value::Object(value.clone())))),
        Some(_) => Err(malformed("Malformed object parameter")),
    }
}

fn clojure_truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

fn validate_redirect_url(value: &str) -> Result<(), ApiError> {
    let url = url::Url::parse(value).map_err(|_| validation("Invalid redirect URI"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(validation("redirect uri may not contain user or password"));
    }
    if url.path().contains("/..") || url.path().contains("\\.") {
        return Err(validation("redirect uri may not contain a path traversal"));
    }
    if url.fragment().is_some() {
        return Err(validation(
            "redirect uri may not contain the fragment component",
        ));
    }
    let localhost = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if (localhost && !matches!(url.scheme(), "http" | "https"))
        || (!localhost && url.scheme() != "https")
    {
        return Err(validation("redirect uri must use the HTTPS scheme"));
    }
    Ok(())
}

fn string_array(body: &Value, key: &'static str) -> Result<Vec<String>, ApiError> {
    body.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| missing(key))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| malformed("Origin parameters must be strings"))
        })
        .collect()
}

fn optional_extra_audiences(body: &Value) -> Result<Option<Vec<String>>, ApiError> {
    const MAX_AUDIENCES: usize = 20;
    const MAX_AUDIENCE_LENGTH: usize = 256;
    let Some(value) = body.get("extra_audiences") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(Vec::new()));
    }
    let values = value
        .as_array()
        .ok_or_else(|| malformed("Malformed extra_audiences parameter"))?;
    let mut audiences = Vec::new();
    for value in values {
        let audience = value
            .as_str()
            .ok_or_else(|| malformed("Malformed extra_audiences parameter"))?
            .trim();
        if audience.is_empty() || audience.chars().count() > MAX_AUDIENCE_LENGTH {
            return Err(validation(
                "extra_audiences entries must be non-empty and at most 256 characters",
            ));
        }
        if !audiences.iter().any(|existing| existing == audience) {
            audiences.push(audience.to_owned());
        }
    }
    if audiences.len() > MAX_AUDIENCES {
        return Err(validation("extra_audiences accepts at most 20 entries"));
    }
    Ok(Some(audiences))
}

fn validate_authorized_origin(service: &str, params: &[String]) -> Result<(), ApiError> {
    let error = match service {
        "netlify" if params.len() != 1 => Some("Netlify should have only the site name param."),
        "vercel" if params.len() != 2 => {
            Some("Vercel should have deployment suffix and project name params.")
        }
        "generic" if params.len() != 1 => Some("Host should be the only parameter."),
        "custom-scheme" if params.len() != 1 => {
            Some("Custom scheme should have only one parameter.")
        }
        "custom-scheme" if !is_custom_scheme(&params[0]) => {
            Some("The requested custom URI scheme is reserved.")
        }
        "netlify" | "vercel" | "generic" | "custom-scheme" => None,
        _ => Some("Unrecognized authorized-origin service."),
    };
    error.map_or(Ok(()), |message| Err(validation(message)))
}

fn authorized_origin_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "service": row.get::<_, String>(1),
        "params": row.get::<_, Vec<String>>(2),
        "created_at": row.get::<_, DateTime<Utc>>(3),
    })
}

fn test_user_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "app_id": row.get::<_, Uuid>(1),
        "email": row.get::<_, String>(2),
        "code": row.get::<_, String>(3),
        "created_at": row.get::<_, DateTime<Utc>>(4),
    })
}

fn rule_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "app_id": row.get::<_, Uuid>(0),
        "code": row.get::<_, tokio_postgres::types::Json<Value>>(1).0,
        "version": row.get::<_, i32>(2),
    })
}

fn rule_version_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "version": row.get::<_, i32>(0),
        "edits": row.get::<_, tokio_postgres::types::Json<Value>>(1).0,
        "created_at": row.get::<_, DateTime<Utc>>(2),
    })
}

fn indexing_job_spec(job_type: &str) -> Option<(&'static str, &'static str)> {
    match job_type {
        "check-data-type" => Some(("data-type", "estimate-work")),
        "remove-data-type" => Some(("data-type", "update-attr-start")),
        "index" | "remove-index" => Some(("index", "update-attr-start")),
        "unique" | "remove-unique" => Some(("unique", "update-attr-start")),
        "required" => Some(("required", "estimate-work")),
        "remove-required" => Some(("required", "update-attr")),
        _ => None,
    }
}

fn indexing_job_query(predicate: &str) -> String {
    format!(
        "SELECT
           j.id, j.app_id, j.group_id, j.attr_id,
           a.etype || '.' || a.label AS attr_name,
           j.job_type, j.job_status, j.job_stage,
           j.work_estimate, j.work_completed, j.error,
           j.checked_data_type::text, j.created_at, j.updated_at, j.done_at,
           j.invalid_unique_value, NULL::jsonb AS invalid_triples_sample, j.error_data
         FROM indexing_jobs j
         JOIN attrs a ON a.id = j.attr_id
         WHERE j.app_id = $1 AND {predicate}
         ORDER BY j.created_at, j.id"
    )
}

fn indexing_job_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "app_id": row.get::<_, Uuid>(1),
        "group_id": row.get::<_, Option<Uuid>>(2),
        "attr_id": row.get::<_, Uuid>(3),
        "attr_name": row.get::<_, String>(4),
        "job_type": row.get::<_, String>(5),
        "job_status": row.get::<_, String>(6),
        "job_stage": row.get::<_, Option<String>>(7),
        "work_estimate": row.get::<_, Option<i32>>(8),
        "work_completed": row.get::<_, Option<i32>>(9),
        "error": row.get::<_, Option<String>>(10),
        "checked_data_type": row.get::<_, Option<String>>(11),
        "created_at": row.get::<_, DateTime<Utc>>(12),
        "updated_at": row.get::<_, DateTime<Utc>>(13),
        "done_at": row.get::<_, Option<DateTime<Utc>>>(14),
        "invalid_unique_value": row
            .get::<_, Option<tokio_postgres::types::Json<Value>>>(15)
            .map(|value| value.0),
        "invalid_triples_sample": row
            .get::<_, Option<tokio_postgres::types::Json<Value>>>(16)
            .map(|value| value.0),
        "error_data": row
            .get::<_, Option<tokio_postgres::types::Json<Value>>>(17)
            .map(|value| value.0),
    })
}

fn backup_job_select() -> &'static str {
    "SELECT id, app_id, app_backup_id, job_status, description,
            work_estimate, work_completed, created_at, updated_at, done_at
     FROM app_backup_jobs"
}

fn restore_job_select() -> &'static str {
    "SELECT id, app_id, title, job_status, progress, error,
            created_at, done_at, updated_at
     FROM app_restore_jobs"
}

fn restore_job_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "app_id": row.get::<_, Uuid>(1),
        "title": row.get::<_, Option<String>>(2),
        "job_status": row.get::<_, String>(3),
        "progress": row.get::<_, Option<String>>(4),
        "error": row.get::<_, Option<String>>(5),
        "created_at": row.get::<_, DateTime<Utc>>(6),
        "done_at": row.get::<_, Option<DateTime<Utc>>>(7),
        "updated_at": row.get::<_, DateTime<Utc>>(8),
    })
}

fn backup_job_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "app_id": row.get::<_, Uuid>(1),
        "app_backup_id": row.get::<_, Option<Uuid>>(2),
        "job_status": row.get::<_, String>(3),
        "description": row.get::<_, Option<String>>(4),
        "work_estimate": row.get::<_, Option<i64>>(5),
        "work_completed": row.get::<_, Option<i64>>(6),
        "created_at": row.get::<_, DateTime<Utc>>(7),
        "updated_at": row.get::<_, DateTime<Utc>>(8),
        "done_at": row.get::<_, Option<DateTime<Utc>>>(9),
    })
}

fn backup_json(row: &tokio_postgres::Row) -> Value {
    let isn = row.get::<_, Option<String>>(1).map(|value| {
        instant_wal::Isn::from_postgres_composite(&value)
            .expect("persisted backup ISNs are valid")
            .to_string()
    });
    json!({
        "id": row.get::<_, Uuid>(0),
        "isn": isn,
        "backup_at": row.get::<_, DateTime<Utc>>(2),
        "files_size": row.get::<_, Option<i64>>(3),
        "db_size": row.get::<_, Option<i64>>(4),
        "uncompressed_size": row.get::<_, Option<i64>>(5),
        "description": row.get::<_, Option<String>>(6),
        "expires_at": row.get::<_, Option<DateTime<Utc>>>(7),
    })
}

async fn active_backup_prefix<C>(
    client: &C,
    app_id: Uuid,
    backup_id: Uuid,
) -> Result<String, ApiError>
where
    C: GenericClient + Sync,
{
    let row = client
        .query_opt(
            "SELECT storage_prefix, expires_at FROM app_backups
             WHERE app_id = $1 AND id = $2 AND deletion_marked_at IS NULL",
            &[&app_id, &backup_id],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("app-backup"))?;
    let expires_at = row.get::<_, Option<DateTime<Utc>>>(1);
    if expires_at.is_some_and(|expires_at| expires_at <= Utc::now()) {
        return Err(
            ApiError::new(ApiErrorType::RecordExpired, "Record expired: app-backup")
                .with_hint(json!({"record-type": "app-backup", "id": backup_id})),
        );
    }
    let prefix = row.get::<_, String>(0);
    if prefix.trim().is_empty() {
        return Err(permission("Backup storage prefix is unavailable"));
    }
    Ok(prefix)
}

fn forwarded_client_ip(request: &DashboardRequest) -> Option<String> {
    request.remote_addr.clone().or_else(|| {
        request
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .or_else(|| {
                request
                    .headers
                    .get("x-real-ip")
                    .and_then(|value| value.to_str().ok())
            })
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn dashboard_upload_path(
    query: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> Result<String, ApiError> {
    query_parameter(query, "path")
        .or_else(|| {
            headers
                .get("path")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| missing("path"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DashboardWorkspace {
    Personal,
    Org(Uuid),
}

impl DashboardWorkspace {
    fn parse(value: &str) -> Result<Self, ApiError> {
        if value == "personal" {
            return Ok(Self::Personal);
        }
        Uuid::parse_str(value)
            .map(Self::Org)
            .map_err(|_| validation("workspace must be personal or an organization ID"))
    }
}

fn query_parameter(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn coerce_dashboard_redirect_path(path: Option<String>) -> String {
    let path = path.unwrap_or_default();
    if path.trim().is_empty() {
        "/dash".to_owned()
    } else if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    }
}

fn hash_dashboard_oauth_uuid(value: Uuid) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn dashboard_oauth_cookie(request: &DashboardRequest) -> Option<Uuid> {
    request
        .headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "__session")
                    .then(|| Uuid::parse_str(value).ok())
                    .flatten()
            })
        })
}

fn dashboard_redirect_response(
    location: &str,
    cookie: Option<(Uuid, bool)>,
) -> Result<DashboardResponse, ApiError> {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location)
            .map_err(|_| ApiError::new(ApiErrorType::OauthError, "Invalid OAuth redirect URL"))?,
    );
    if let Some((cookie, secure)) = cookie {
        let secure = if secure { "; Secure" } else { "" };
        let value = format!(
            "__session={cookie}; Path=/dash/oauth; Max-Age=3600; HttpOnly; SameSite=Lax{secure}"
        );
        headers.insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&value)
                .map_err(|_| ApiError::new(ApiErrorType::Unknown, "Invalid OAuth cookie"))?,
        );
    }
    Ok(DashboardResponse {
        status: StatusCode::FOUND,
        headers,
        body: axum::body::Bytes::new().into(),
    })
}

fn dashboard_oauth_callback_response(
    config: &DashboardOAuthConfig,
    error: Option<&str>,
    code: Option<Uuid>,
    ticket: Option<Uuid>,
    redirect_to_dev: bool,
) -> Result<DashboardResponse, ApiError> {
    let origin = if redirect_to_dev {
        &config.dev_dashboard_origin
    } else {
        &config.dashboard_origin
    };
    let mut url = url::Url::parse(&format!(
        "{}/dash/oauth/callback",
        origin.trim_end_matches('/')
    ))
    .map_err(|_| malformed("Invalid dashboard origin"))?;
    {
        let mut query = url.query_pairs_mut();
        if let Some(error) = error {
            query.append_pair("error", error);
        } else if let Some(code) = code {
            query.append_pair("code", &code.to_string());
        }
        if let Some(ticket) = ticket {
            query.append_pair("ticket", &ticket.to_string());
        }
    }
    dashboard_redirect_response(url.as_str(), None)
}

fn optional_query_uuid(query: Option<&str>, name: &str) -> Result<Option<Uuid>, ApiError> {
    query_parameter(query, name)
        .map(|value| {
            Uuid::parse_str(&value).map_err(|_| malformed("Malformed UUID query parameter"))
        })
        .transpose()
}

fn restore_job_id(path: &str) -> Option<Uuid> {
    let mut parts = path.strip_prefix("/dash/restore-jobs/")?.split('/');
    let id = Uuid::parse_str(parts.next()?).ok()?;
    parts.next().is_none().then_some(id)
}

async fn load_email_info<C>(
    client: &C,
    app_id: Uuid,
) -> Result<Option<tokio_postgres::Row>, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_opt(
            "SELECT t.id, t.app_id, t.email_type, t.body, t.sender_id,
                    t.name, t.subject, s.email, s.postmark_id,
                    v.id AS verification_id, v.verified AS verification_verified
             FROM app_email_templates t
             LEFT JOIN app_email_senders s ON t.sender_id = s.id
             LEFT JOIN app_email_verifications v
               ON t.sender_id = v.sender_id AND v.app_id = t.app_id
             WHERE t.app_id = $1 AND t.email_type = 'magic-code'",
            &[&app_id],
        )
        .await
        .map_err(database_error)
}

/// Sender-domain status for the dashboard. `verified` is true only while the
/// domain was checked as enabled within the last 25 hours.
async fn load_sender_domain<C>(client: &C, app_id: Uuid, domain: &str) -> Result<Value, ApiError>
where
    C: GenericClient + Sync,
{
    let row = client
        .query_opt(
            "SELECT cloudflare_zone_id, verified_at, ownership_verified_at, last_error
             FROM app_sender_domains WHERE app_id = $1 AND domain = $2",
            &[&app_id, &domain],
        )
        .await
        .map_err(database_error)?;
    Ok(row.map_or(Value::Null, |row| {
        json!({
            "domain": domain,
            "cloudflare_zone_id": row.get::<_, Option<String>>(0),
            "cloudflare_enabled": row.get::<_, Option<DateTime<Utc>>>(1).is_some(),
            "ownership_verified": row.get::<_, Option<DateTime<Utc>>>(2).is_some(),
            "verified": crate::postmark::domain_verification_is_fresh(
                row.get(1),
                Utc::now(),
            ),
            "last_error": row.get::<_, Option<String>>(3),
        })
    }))
}

fn email_info_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "app_id": row.get::<_, Uuid>(1),
        "email_type": row.get::<_, String>(2),
        "body": row.get::<_, String>(3),
        "sender_id": row.get::<_, Option<Uuid>>(4),
        "name": row.get::<_, Option<String>>(5),
        "subject": row.get::<_, String>(6),
        "email": row.get::<_, Option<String>>(7),
        "postmark_id": row.get::<_, Option<i32>>(8),
        "verification_id": row.get::<_, Option<Uuid>>(9),
        "verification_verified": row.get::<_, Option<bool>>(10),
    })
}

fn optional_string(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_email_value(body: &Value, key: &'static str) -> Result<Option<String>, ApiError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(_) => email_value(body, key).map(Some),
    }
}

fn configured_login_expiry(minutes: Option<i32>) -> i32 {
    let max = i32::try_from(MAX_MAGIC_CODE_EXPIRY_MINUTES).unwrap_or(i32::MAX);
    minutes.map_or(15, |minutes| minutes.clamp(1, max))
}

fn render_test_template(
    template: &str,
    email: &str,
    app_title: &str,
    expiry_minutes: i32,
    html: bool,
) -> String {
    let escape = |value: &str| {
        if html {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;")
        } else {
            value.to_owned()
        }
    };
    let expiration = if expiry_minutes >= 60 {
        let hours = expiry_minutes / 60;
        format!("{hours} hour{}", if hours > 1 { "s" } else { "" })
    } else {
        format!(
            "{expiry_minutes} minute{}",
            if expiry_minutes > 1 { "s" } else { "" }
        )
    };
    template
        .replace("{user_email}", &escape(email))
        .replace("{code}", "123456")
        .replace("{app_title}", &escape(app_title))
        .replace("{expiration}", &expiration)
}

struct OAuthAppInput {
    support_email: Option<String>,
    home_page: Option<String>,
    privacy_policy: Option<String>,
    terms: Option<String>,
    logo: Option<Vec<u8>>,
}

fn oauth_app_input(body: &Value) -> Result<OAuthAppInput, ApiError> {
    Ok(OAuthAppInput {
        support_email: optional_string(body, "support_email"),
        home_page: optional_web_url(body, "app_home_page")?,
        privacy_policy: optional_web_url(body, "app_privacy_policy_link")?,
        terms: optional_web_url(body, "app_tos_link")?,
        logo: optional_string(body, "app_logo")
            .map(|value| decode_oauth_app_logo(&value))
            .transpose()?,
    })
}

fn optional_web_url(body: &Value, key: &'static str) -> Result<Option<String>, ApiError> {
    optional_string(body, key)
        .map(|value| {
            let parsed = url::Url::parse(&value).map_err(|_| malformed("Malformed web URL"))?;
            if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() {
                Ok(value)
            } else {
                Err(malformed("Malformed web URL"))
            }
        })
        .transpose()
}

fn decode_oauth_app_logo(value: &str) -> Result<Vec<u8>, ApiError> {
    let rest = value
        .strip_prefix("data:image/")
        .ok_or_else(|| malformed("Invalid image url"))?;
    let (mime, encoded) = rest
        .split_once(";base64,")
        .ok_or_else(|| malformed("Invalid image url"))?;
    if mime.len() > 4 {
        return Err(malformed("Invalid image type"));
    }
    if !matches!(mime, "jpg" | "jpeg" | "png" | "svg" | "webp") {
        return Err(malformed("Invalid image type"));
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| malformed("Invalid image"))?;
    if decoded.len() > 1024 * 1024 {
        return Err(malformed("Image is too large"));
    }
    let mut bytes = format!("{mime:<4}").into_bytes();
    bytes.extend(decoded);
    Ok(bytes)
}

fn encode_oauth_app_logo(bytes: &[u8]) -> Result<String, ApiError> {
    if bytes.len() < 4 {
        return Err(ApiError::new(
            ApiErrorType::Unknown,
            "Invalid stored OAuth app logo",
        ));
    }
    let mime = std::str::from_utf8(&bytes[..4])
        .map_err(|_| ApiError::new(ApiErrorType::Unknown, "Invalid stored OAuth app logo"))?
        .trim();
    Ok(format!(
        "data:image/{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes[4..])
    ))
}

fn oauth_app_json(row: &tokio_postgres::Row) -> Result<Value, ApiError> {
    let logo = row
        .get::<_, Option<Vec<u8>>>(9)
        .as_deref()
        .map(encode_oauth_app_logo)
        .transpose()?;
    Ok(json!({
        "id": row.get::<_, Uuid>(0),
        "appId": row.get::<_, Uuid>(1),
        "appName": row.get::<_, String>(2),
        "appLogo": logo,
        "grantedScopes": row.get::<_, Vec<String>>(3),
        "isPublic": row.get::<_, bool>(4),
        "supportEmail": row.get::<_, Option<String>>(5),
        "appHomePage": row.get::<_, Option<String>>(6),
        "appPrivacyPolicyLink": row.get::<_, Option<String>>(7),
        "appTosLink": row.get::<_, Option<String>>(8),
        "createdAt": row.get::<_, DateTime<Utc>>(10),
        "updatedAt": row.get::<_, DateTime<Utc>>(11),
    }))
}

fn oauth_app_client_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "clientId": row.get::<_, Uuid>(0),
        "oauthAppId": row.get::<_, Uuid>(1),
        "clientName": row.get::<_, String>(2),
        "authorizedRedirectUrls": row.get::<_, Option<Vec<String>>>(3).unwrap_or_default(),
        "createdAt": row.get::<_, DateTime<Utc>>(4),
        "updatedAt": row.get::<_, DateTime<Utc>>(5),
    })
}

fn oauth_app_secret_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "clientId": row.get::<_, Uuid>(1),
        "firstFour": row.get::<_, String>(2),
        "createdAt": row.get::<_, DateTime<Utc>>(3),
    })
}

async fn insert_oauth_app_secret<C>(
    client: &C,
    client_id: Uuid,
) -> Result<(tokio_postgres::Row, String), ApiError>
where
    C: GenericClient + Sync,
{
    let mut random = [0_u8; 34];
    rand::fill(&mut random);
    let mut secret = String::with_capacity(68);
    for byte in random {
        write!(&mut secret, "{byte:02x}").expect("writing to a string cannot fail");
    }
    let digest = Sha256::digest(secret.as_bytes()).to_vec();
    let row = client
        .query_one(
            "INSERT INTO instant_oauth_app_client_secrets
               (id, client_id, hashed_secret, first_four)
             VALUES ($1, $2, $3, $4)
             RETURNING id, client_id, first_four, created_at",
            &[&Uuid::new_v4(), &client_id, &digest, &&secret[..4]],
        )
        .await
        .map_err(database_error)?;
    Ok((row, secret))
}

fn optional_string_array(body: &Value, key: &'static str) -> Result<Option<Vec<String>>, ApiError> {
    let Some(value) = body.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_array()
        .ok_or_else(|| malformed("Malformed string array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| malformed("Malformed string array"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn validate_redirects(redirects: &[String], allow_localhost: bool) -> Result<(), ApiError> {
    for redirect in redirects {
        let url = url::Url::parse(redirect).map_err(|_| validation("Invalid redirect URL"))?;
        let host = url.host_str().unwrap_or_default();
        let localhost = matches!(host, "localhost" | "127.0.0.1");
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path().contains("/..")
            || url.path().contains("\\.")
            || (localhost && !allow_localhost)
            || (!localhost && url.scheme() != "https")
            || (localhost && !matches!(url.scheme(), "http" | "https"))
        {
            return Err(validation("Invalid redirect URL"));
        }
    }
    Ok(())
}

async fn user_authorized_oauth_apps<C>(client: &C, user_id: Uuid) -> Result<Vec<Value>, ApiError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT DISTINCT a.id, a.app_name, a.app_logo, a.app_home_page,
                    a.app_privacy_policy_link, a.app_tos_link
             FROM instant_oauth_apps a
             JOIN instant_oauth_app_clients c ON c.oauth_app_id = a.id
             WHERE EXISTS (
               SELECT 1 FROM instant_user_oauth_access_tokens t
               WHERE t.client_id = c.client_id AND t.user_id = $1
             ) OR EXISTS (
               SELECT 1 FROM instant_user_oauth_refresh_tokens t
               WHERE t.client_id = c.client_id AND t.user_id = $1
             )
             ORDER BY a.app_name",
            &[&user_id],
        )
        .await
        .map_err(database_error)?;
    rows.iter()
        .map(|row| {
            let logo = row
                .get::<_, Option<Vec<u8>>>(2)
                .as_deref()
                .map(encode_oauth_app_logo)
                .transpose()?;
            Ok(json!({
                "id": row.get::<_, Uuid>(0),
                "name": row.get::<_, String>(1),
                "logo": logo,
                "homePage": row.get::<_, Option<String>>(3),
                "privacyPolicyLink": row.get::<_, Option<String>>(4),
                "tosLink": row.get::<_, Option<String>>(5),
            }))
        })
        .collect()
}

fn webhook_query(predicate: &str) -> String {
    format!(
        "SELECT w.id, w.sink,
           ARRAY(
             SELECT DISTINCT a.etype FROM attrs a
             WHERE a.id = ANY(w.id_attr_ids) ORDER BY a.etype
           ) AS namespaces,
           w.actions::text[], w.status::text, w.disabled_reason,
           w.created_at, w.updated_at, w.id_attr_ids
         FROM webhooks w
         WHERE w.app_id = $1 AND {predicate}
         ORDER BY w.created_at DESC, w.id"
    )
}

fn webhook_json(row: &tokio_postgres::Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "sink": row.get::<_, tokio_postgres::types::Json<Value>>(1).0,
        "namespaces": row.get::<_, Vec<String>>(2),
        "actions": row.get::<_, Vec<String>>(3),
        "status": row.get::<_, String>(4),
        "disabled_reason": row.get::<_, Option<String>>(5),
        "created_at": row.get::<_, DateTime<Utc>>(6),
        "updated_at": row.get::<_, DateTime<Utc>>(7),
    })
}

fn optional_non_blank_string(body: &Value, key: &'static str) -> Result<Option<String>, ApiError> {
    if body.as_object().is_some_and(|body| body.contains_key(key)) {
        non_blank_string(body, key).map(Some)
    } else {
        Ok(None)
    }
}

fn string_vector(body: &Value, key: &'static str) -> Result<Vec<String>, ApiError> {
    body.get(key)
        .ok_or_else(|| missing(key))?
        .as_array()
        .filter(|values| values.iter().all(Value::is_string))
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .ok_or_else(|| malformed("Expected an array of strings"))
}

fn optional_string_vector(
    body: &Value,
    key: &'static str,
) -> Result<Option<Vec<String>>, ApiError> {
    if body.as_object().is_some_and(|body| body.contains_key(key)) {
        string_vector(body, key).map(Some)
    } else {
        Ok(None)
    }
}

fn webhook_actions(body: &Value) -> Result<Vec<String>, ApiError> {
    let actions = string_vector(body, "actions")?;
    validate_webhook_actions(actions)
}

fn optional_webhook_actions(body: &Value) -> Result<Option<Vec<String>>, ApiError> {
    optional_string_vector(body, "actions")?
        .map(validate_webhook_actions)
        .transpose()
}

fn validate_webhook_actions(actions: Vec<String>) -> Result<Vec<String>, ApiError> {
    let actions = actions.into_iter().collect::<BTreeSet<_>>();
    if actions.is_empty() {
        return Err(validation("Webhook must have at least one action."));
    }
    if !actions
        .iter()
        .all(|action| matches!(action.as_str(), "create" | "update" | "delete"))
    {
        return Err(validation("Invalid webhook action"));
    }
    Ok(actions.into_iter().collect())
}

async fn resolve_webhook_namespaces<C>(
    client: &C,
    app_id: Uuid,
    namespaces: &[String],
) -> Result<Vec<Uuid>, ApiError>
where
    C: GenericClient + Sync,
{
    let namespaces = namespaces.iter().cloned().collect::<BTreeSet<_>>();
    if namespaces.is_empty() {
        return Err(validation("Webhook must have at least one table."));
    }
    let requested = namespaces.into_iter().collect::<Vec<_>>();
    let rows = client
        .query(
            "SELECT etype, id FROM attrs
             WHERE (app_id = $1 OR app_id = $2)
               AND etype = ANY($3) AND label = 'id'
               AND deletion_marked_at IS NULL
             ORDER BY etype, id",
            &[&app_id, &instant_instaql::SYSTEM_CATALOG_APP_ID, &requested],
        )
        .await
        .map_err(database_error)?;
    let found = rows
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<BTreeSet<_>>();
    if let Some(missing_namespace) = requested.iter().find(|name| !found.contains(*name)) {
        return Err(validation("Could not find matching webhook table")
            .with_hint(json!({"namespace": missing_namespace})));
    }
    Ok(rows.iter().map(|row| row.get(1)).collect())
}

fn webhook_topics(id_attr_ids: &[Uuid]) -> i64 {
    id_attr_ids.iter().fold(0_i64, |topics, id| {
        let (seed, value) = id.as_u64_pair();
        let input = value.to_ne_bytes();
        let hash = xxhash_rust::xxh3::xxh3_64_with_seed(&input, seed);
        topics | 1_i64.wrapping_shl((hash % 64) as u32)
    })
}

async fn enforce_webhook_limit<C>(
    client: &C,
    app_id: Uuid,
    maximum_active_webhooks: i64,
) -> Result<(), ApiError>
where
    C: GenericClient + Sync,
{
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM webhooks
             WHERE app_id = $1 AND status = 'active'::webhook_status",
            &[&app_id],
        )
        .await
        .map_err(database_error)?
        .get(0);
    check_webhook_limit(count, maximum_active_webhooks)
}

fn check_webhook_limit(count: i64, maximum_active_webhooks: i64) -> Result<(), ApiError> {
    if count >= maximum_active_webhooks {
        Err(validation_owned(format!(
            "An app may not have more than {maximum_active_webhooks} active webhooks."
        )))
    } else {
        Ok(())
    }
}

async fn ensure_unique_webhook<C>(
    client: &C,
    app_id: Uuid,
    ignore_id: Option<Uuid>,
    id_attr_ids: &[Uuid],
    actions: &[String],
    sink: &Value,
) -> Result<(), ApiError>
where
    C: GenericClient + Sync,
{
    let duplicate = client
        .query_opt(
            "SELECT id FROM webhooks
             WHERE app_id = $1 AND status = 'active'::webhook_status
               AND ($2::uuid IS NULL OR id <> $2)
               AND id_attr_ids @> $3 AND id_attr_ids <@ $3
               AND actions @> $4::text[]::webhook_action[]
               AND actions <@ $4::text[]::webhook_action[]
               AND sink = $5",
            &[
                &app_id,
                &ignore_id,
                &id_attr_ids,
                &actions,
                &tokio_postgres::types::Json(sink),
            ],
        )
        .await
        .map_err(database_error)?;
    if let Some(row) = duplicate {
        Err(
            validation("A webhook already exists with all of the same properties")
                .with_hint(json!({"webhook-id": row.get::<_, Uuid>(0)})),
        )
    } else {
        Ok(())
    }
}

fn webhook_events_query(paginated: bool) -> String {
    let cursor = if paginated {
        "AND ($3::timestamptz IS NULL OR
              (created_at, isn) < ($3::timestamptz, $4::text::isn))"
    } else {
        ""
    };
    let limit = if paginated { "LIMIT 101" } else { "" };
    format!(
        "SELECT isn::text, status::text, to_jsonb(attempts),
                next_attempt_after, created_at, updated_at
         FROM webhook_events
         WHERE app_id = $1 AND webhook_id = $2
           AND created_at >= now() - interval '60 days'
           {cursor}
         ORDER BY created_at DESC, isn DESC
         {limit}"
    )
}

fn webhook_event_by_isn_query() -> String {
    "SELECT isn::text, status::text, to_jsonb(attempts),
            next_attempt_after, created_at, updated_at
     FROM webhook_events
     WHERE app_id = $1 AND webhook_id = $2 AND isn = $3::text::isn"
        .to_owned()
}

fn webhook_event_json(row: &tokio_postgres::Row) -> Value {
    let isn = instant_wal::Isn::from_postgres_composite(&row.get::<_, String>(0))
        .expect("persisted webhook ISNs are valid")
        .to_string();
    json!({
        "isn": isn,
        "status": row.get::<_, String>(1),
        "attempts": row
            .get::<_, Option<tokio_postgres::types::Json<Value>>>(2)
            .map(|value| value.0),
        "next_attempt_after": row.get::<_, Option<DateTime<Utc>>>(3),
        "created_at": row.get::<_, DateTime<Utc>>(4),
        "updated_at": row.get::<_, DateTime<Utc>>(5),
    })
}

fn webhook_event_cursor(row: &tokio_postgres::Row) -> String {
    let created_at = row.get::<_, DateTime<Utc>>(4);
    let isn = instant_wal::Isn::from_postgres_composite(&row.get::<_, String>(0))
        .expect("persisted webhook ISNs are valid");
    encode_webhook_event_cursor(created_at, isn)
}

fn encode_webhook_event_cursor(created_at: DateTime<Utc>, isn: instant_wal::Isn) -> String {
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(&created_at.timestamp().to_be_bytes());
    bytes.extend_from_slice(&created_at.timestamp_subsec_nanos().to_be_bytes());
    bytes.extend_from_slice(&isn.to_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn webhook_after_cursor(
    query: Option<&str>,
) -> Result<Option<(DateTime<Utc>, instant_wal::Isn)>, ApiError> {
    let Some(cursor) = query.and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "after")
            .map(|(_, value)| value.into_owned())
    }) else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| malformed("Malformed webhook events cursor"))?;
    let bytes: [u8; 24] = bytes
        .try_into()
        .map_err(|_| malformed("Malformed webhook events cursor"))?;
    let seconds = i64::from_be_bytes(bytes[..8].try_into().expect("fixed cursor slice"));
    let nanos = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed cursor slice"));
    let created_at = DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| malformed("Malformed webhook events cursor"))?;
    let isn = instant_wal::Isn::from_bytes(&bytes[12..])
        .map_err(|_| malformed("Malformed webhook events cursor"))?;
    Ok(Some((created_at, isn)))
}

#[derive(Clone, Debug)]
struct SchemaAttribute {
    id: Option<Uuid>,
    forward_identity: (Option<Uuid>, String, String),
    reverse_identity: Option<(Option<Uuid>, String, String)>,
    value_type: &'static str,
    cardinality: &'static str,
    unique: bool,
    indexed: bool,
    required: bool,
    checked_data_type: Option<String>,
    on_delete: Option<String>,
    on_delete_reverse: Option<String>,
    catalog: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
struct SchemaShape {
    blobs: BTreeMap<String, BTreeMap<String, SchemaAttribute>>,
    refs: BTreeMap<(String, String, String, String), SchemaAttribute>,
}

pub(crate) async fn load_dashboard_attributes<C>(
    client: &C,
    app_id: Uuid,
) -> Result<Vec<AdminAttribute>, ApiError>
where
    C: GenericClient + Sync,
{
    let statement = format!(
        "SELECT {ATTRIBUTE_COLUMNS}
         FROM attrs
         WHERE (app_id = $1 OR app_id = $2)
           AND deletion_marked_at IS NULL
         ORDER BY id"
    );
    client
        .query(
            &statement,
            &[&app_id, &instant_instaql::SYSTEM_CATALOG_APP_ID],
        )
        .await
        .map_err(database_error)?
        .iter()
        .map(row_to_attribute)
        .collect()
}

pub(crate) fn schema_push_plan(
    current_attributes: &[AdminAttribute],
    client_schema: &Value,
    check_types: bool,
    background_updates: bool,
) -> Result<Value, ApiError> {
    let current = schema_shape_from_attributes(current_attributes);
    let new = schema_shape_from_client(client_schema)?;
    let steps = schema_diff_steps(&current, &new, check_types, background_updates);
    validate_schema_plan(current_attributes, &steps)?;
    Ok(json!({
        "new-schema": schema_shape_json(&new),
        "current-schema": attributes_to_dashboard_schema(current_attributes),
        "current-attrs": current_attributes,
        "steps": steps,
    }))
}

fn schema_shape_from_attributes(attributes: &[AdminAttribute]) -> SchemaShape {
    let mut shape = SchemaShape::default();
    for source in attributes
        .iter()
        .filter(|attribute| dashboard_attribute_visible(attribute))
    {
        let (_, etype, label) = &source.forward_identity;
        let attr = SchemaAttribute {
            id: Some(source.id),
            forward_identity: (
                Some(source.forward_identity.0),
                etype.clone(),
                label.clone(),
            ),
            reverse_identity: source
                .reverse_identity
                .as_ref()
                .map(|(id, etype, label)| (Some(*id), etype.clone(), label.clone())),
            value_type: if source.value_type == "ref" {
                "ref"
            } else {
                "blob"
            },
            cardinality: if source.cardinality == "many" {
                "many"
            } else {
                "one"
            },
            unique: source.unique,
            indexed: source.indexed,
            required: source.required || etype == "$files" && label == "url",
            checked_data_type: source.checked_data_type.clone(),
            on_delete: source.on_delete.clone(),
            on_delete_reverse: source.on_delete_reverse.clone(),
            catalog: Some(match source.catalog {
                AttributeCatalogKind::User => "user",
                AttributeCatalogKind::System => "system",
            }),
        };
        if source.value_type == "ref" {
            if let Some((_, reverse_etype, reverse_label)) = &source.reverse_identity {
                shape.refs.insert(
                    (
                        etype.clone(),
                        label.clone(),
                        reverse_etype.clone(),
                        reverse_label.clone(),
                    ),
                    attr,
                );
            }
        } else {
            shape
                .blobs
                .entry(etype.clone())
                .or_default()
                .insert(label.clone(), attr);
        }
    }
    shape
}

#[allow(clippy::too_many_lines)] // Keeps entity/link coercion and reserved filtering together.
fn schema_shape_from_client(value: &Value) -> Result<SchemaShape, ApiError> {
    let root = value
        .as_object()
        .ok_or_else(|| validation("schema must be an object"))?;
    let entities = optional_schema_object(root.get("entities"), "schema.entities")?;
    let links = optional_schema_object(root.get("links"), "schema.links")?;
    let reserved = reserved_system_identities()?;
    let system_links = system_catalog_link_shapes()?;
    let mut shape = SchemaShape::default();
    for (etype, entity) in entities {
        let entity = entity
            .as_object()
            .ok_or_else(|| validation("schema entity definitions must be objects"))?;
        let attrs = optional_schema_object(entity.get("attrs"), "schema entity attrs")?;
        let mut converted = BTreeMap::new();
        for (label, definition) in attrs {
            if reserved.contains(&(etype.clone(), label.clone())) {
                continue;
            }
            let definition = definition
                .as_object()
                .ok_or_else(|| validation("schema attribute definitions must be objects"))?;
            let config = optional_schema_object(definition.get("config"), "attribute config")?;
            let checked_data_type = definition
                .get("valueType")
                .and_then(Value::as_str)
                .filter(|value| matches!(*value, "number" | "string" | "boolean" | "date"))
                .map(str::to_owned);
            converted.insert(
                label.clone(),
                SchemaAttribute {
                    id: None,
                    forward_identity: (None, etype.clone(), label.clone()),
                    reverse_identity: None,
                    value_type: "blob",
                    cardinality: "one",
                    unique: clojure_truthy(config.get("unique")),
                    indexed: clojure_truthy(config.get("indexed")),
                    required: definition.get("required") == Some(&Value::Bool(true)),
                    checked_data_type,
                    on_delete: None,
                    on_delete_reverse: None,
                    catalog: Some(if etype.starts_with('$') {
                        "system"
                    } else {
                        "user"
                    }),
                },
            );
        }
        if !converted.is_empty() {
            shape.blobs.insert(etype.clone(), converted);
        }
    }
    for link in links.values() {
        let link = link
            .as_object()
            .ok_or_else(|| validation("schema link definitions must be objects"))?;
        let forward = link
            .get("forward")
            .and_then(Value::as_object)
            .ok_or_else(|| validation("schema links require a forward definition"))?;
        let reverse = link
            .get("reverse")
            .and_then(Value::as_object)
            .ok_or_else(|| validation("schema links require a reverse definition"))?;
        let from_etype = schema_string(forward, "on")?;
        let from_label = schema_string(forward, "label")?;
        let to_etype = schema_string(reverse, "on")?;
        let to_label = schema_string(reverse, "label")?;
        let cardinality = schema_cardinality(forward, "has")?;
        let reverse_cardinality = schema_cardinality(reverse, "has")?;
        let key = (
            from_etype.clone(),
            from_label.clone(),
            to_etype.clone(),
            to_label.clone(),
        );
        if system_links.contains(&(
            key.clone(),
            cardinality.to_owned(),
            reverse_cardinality.to_owned(),
        )) {
            continue;
        }
        shape.refs.insert(
            key,
            SchemaAttribute {
                id: None,
                forward_identity: (None, from_etype, from_label),
                reverse_identity: Some((None, to_etype, to_label)),
                value_type: "ref",
                cardinality,
                unique: reverse_cardinality == "one",
                indexed: false,
                required: forward.get("required") == Some(&Value::Bool(true)),
                checked_data_type: None,
                on_delete: schema_delete_action(forward.get("onDelete"))?,
                on_delete_reverse: schema_delete_action(reverse.get("onDelete"))?,
                catalog: None,
            },
        );
    }
    Ok(shape)
}

fn optional_schema_object<'a>(
    value: Option<&'a Value>,
    description: &str,
) -> Result<&'a serde_json::Map<String, Value>, ApiError> {
    match value {
        None | Some(Value::Null) => {
            static EMPTY: OnceLock<serde_json::Map<String, Value>> = OnceLock::new();
            Ok(EMPTY.get_or_init(serde_json::Map::new))
        }
        Some(Value::Object(value)) => Ok(value),
        Some(_) => Err(validation_owned(format!("{description} must be an object"))),
    }
}

fn schema_string(
    object: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<String, ApiError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| validation_owned(format!("schema link `{key}` must be a non-blank string")))
}

fn schema_cardinality(
    object: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<&'static str, ApiError> {
    match object.get(key).and_then(Value::as_str) {
        Some("one") => Ok("one"),
        Some("many") => Ok("many"),
        _ => Err(validation(
            "schema link cardinality must be `one` or `many`",
        )),
    }
}

fn schema_delete_action(value: Option<&Value>) -> Result<Option<String>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value == "cascade" => Ok(Some(value.clone())),
        _ => Err(validation("schema link onDelete must be `cascade`")),
    }
}

fn reserved_system_identities() -> Result<HashSet<(String, String)>, ApiError> {
    Ok(system_attribute_catalog()
        .map_err(crate::admin_data::internal_error)?
        .attributes
        .into_iter()
        .flat_map(|attribute| {
            let forward = (attribute.etype, attribute.label);
            let reverse = attribute.reverse_etype.zip(attribute.reverse_label);
            std::iter::once(forward).chain(reverse)
        })
        .collect())
}

type SystemLinkShape = ((String, String, String, String), String, String);

fn system_catalog_link_shapes() -> Result<HashSet<SystemLinkShape>, ApiError> {
    Ok(system_attribute_catalog()
        .map_err(crate::admin_data::internal_error)?
        .attributes
        .into_iter()
        .filter_map(|attribute| {
            let reverse_etype = attribute.reverse_etype?;
            let reverse_label = attribute.reverse_label?;
            Some((
                (
                    attribute.etype,
                    attribute.label,
                    reverse_etype,
                    reverse_label,
                ),
                cardinality_string(attribute.cardinality).to_owned(),
                if attribute.unique { "one" } else { "many" }.to_owned(),
            ))
        })
        .collect())
}

fn schema_shape_json(shape: &SchemaShape) -> Value {
    let refs = shape
        .refs
        .iter()
        .map(|((a, b, c, d), attribute)| {
            (
                clojure_vector_key([a, b, c, d]),
                schema_attribute_json(attribute),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let blobs = shape
        .blobs
        .iter()
        .map(|(etype, attrs)| {
            let attrs = attrs
                .iter()
                .map(|(label, attribute)| (label.clone(), schema_attribute_json(attribute)))
                .collect();
            (etype.clone(), Value::Object(attrs))
        })
        .collect();
    json!({"refs": refs, "blobs": Value::Object(blobs)})
}

fn schema_attribute_json(attribute: &SchemaAttribute) -> Value {
    let mut value = json!({
        "id": attribute.id,
        "value-type": attribute.value_type,
        "cardinality": attribute.cardinality,
        "forward-identity": [
            attribute.forward_identity.0,
            &attribute.forward_identity.1,
            &attribute.forward_identity.2,
        ],
        "unique?": attribute.unique,
        "index?": attribute.indexed,
        "required?": attribute.required,
    });
    let object = value
        .as_object_mut()
        .expect("schema attributes are objects");
    if attribute.value_type == "blob" {
        object.insert(
            "checked-data-type".to_owned(),
            attribute
                .checked_data_type
                .as_ref()
                .map_or(Value::Null, |value| Value::String(value.clone())),
        );
    } else if let Some(identity) = &attribute.reverse_identity {
        object.insert(
            "reverse-identity".to_owned(),
            json!([identity.0, identity.1, identity.2]),
        );
        object.insert(
            "on-delete".to_owned(),
            attribute
                .on_delete
                .as_ref()
                .map_or(Value::Null, |value| Value::String(value.clone())),
        );
        object.insert(
            "on-delete-reverse".to_owned(),
            attribute
                .on_delete_reverse
                .as_ref()
                .map_or(Value::Null, |value| Value::String(value.clone())),
        );
    }
    if let Some(catalog) = attribute.catalog {
        object.insert("catalog".to_owned(), Value::String(catalog.to_owned()));
    }
    value
}

#[allow(clippy::too_many_lines)] // Keeps the JVM planner's operation ordering visible in one pass.
fn schema_diff_steps(
    current: &SchemaShape,
    new: &SchemaShape,
    check_types: bool,
    background_updates: bool,
) -> Vec<Value> {
    let mut steps = Vec::new();
    for (etype, attrs) in &new.blobs {
        if !current.blobs.contains_key(etype) {
            steps.push(json!(["add-attr", {
                "value-type": "blob",
                "cardinality": "one",
                "id": Uuid::new_v4(),
                "forward-identity": [Uuid::new_v4(), etype, "id"],
                "unique?": true,
                "index?": false,
            }]));
        }
        for (label, new_attribute) in attrs {
            if label == "id" {
                continue;
            }
            let current_attribute = current.blobs.get(etype).and_then(|attrs| attrs.get(label));
            let Some(current_attribute) = current_attribute else {
                let mut attribute = schema_attribute_json(new_attribute);
                let object = attribute
                    .as_object_mut()
                    .expect("schema attributes are objects");
                object.insert("id".to_owned(), json!(Uuid::new_v4()));
                object.insert(
                    "forward-identity".to_owned(),
                    json!([Uuid::new_v4(), etype, label]),
                );
                object.remove("catalog");
                object.remove("reverse-identity");
                if new_attribute.checked_data_type.is_none() {
                    object.remove("checked-data-type");
                }
                steps.push(json!(["add-attr", attribute]));
                continue;
            };
            let changed_type = check_types
                && new_attribute.checked_data_type != current_attribute.checked_data_type;
            let changed_unique = new_attribute.unique != current_attribute.unique;
            let changed_index = new_attribute.indexed != current_attribute.indexed;
            let changed_required = new_attribute.required != current_attribute.required;
            if !background_updates && (changed_unique || changed_index || changed_required) {
                steps.push(json!(["update-attr", {
                    "value-type": "blob",
                    "cardinality": "one",
                    "id": current_attribute.id,
                    "forward-identity": identity_json(&current_attribute.forward_identity),
                    "unique?": new_attribute.unique,
                    "index?": new_attribute.indexed,
                    "required?": new_attribute.required,
                }]));
            }
            if background_updates && changed_index {
                steps.push(background_schema_step(
                    if new_attribute.indexed {
                        "index"
                    } else {
                        "remove-index"
                    },
                    current_attribute,
                    None,
                ));
            }
            if background_updates && changed_unique {
                steps.push(background_schema_step(
                    if new_attribute.unique {
                        "unique"
                    } else {
                        "remove-unique"
                    },
                    current_attribute,
                    None,
                ));
            }
            if background_updates && changed_required {
                steps.push(background_schema_step(
                    if new_attribute.required {
                        "required"
                    } else {
                        "remove-required"
                    },
                    current_attribute,
                    None,
                ));
            }
            if changed_type && current_attribute.catalog != Some("system") {
                steps.push(background_schema_step(
                    if new_attribute.checked_data_type.is_some() {
                        "check-data-type"
                    } else {
                        "remove-data-type"
                    },
                    current_attribute,
                    new_attribute.checked_data_type.as_deref(),
                ));
            }
        }
    }
    for (key, new_attribute) in &new.refs {
        let Some(current_attribute) = current.refs.get(key) else {
            let mut attribute = schema_attribute_json(new_attribute);
            let object = attribute
                .as_object_mut()
                .expect("schema attributes are objects");
            object.insert("id".to_owned(), json!(Uuid::new_v4()));
            object.insert(
                "forward-identity".to_owned(),
                json!([Uuid::new_v4(), &key.0, &key.1]),
            );
            object.insert(
                "reverse-identity".to_owned(),
                json!([Uuid::new_v4(), &key.2, &key.3]),
            );
            object.remove("catalog");
            steps.push(json!(["add-attr", attribute]));
            continue;
        };
        let changed_required = new_attribute.required != current_attribute.required;
        let changed_updatable = new_attribute.cardinality != current_attribute.cardinality
            || new_attribute.unique != current_attribute.unique
            || new_attribute.on_delete != current_attribute.on_delete
            || new_attribute.on_delete_reverse != current_attribute.on_delete_reverse;
        if background_updates && changed_required {
            steps.push(background_schema_step(
                if new_attribute.required {
                    "required"
                } else {
                    "remove-required"
                },
                current_attribute,
                None,
            ));
        }
        if changed_updatable || !background_updates {
            steps.push(json!(["update-attr", {
                "value-type": "ref",
                "id": current_attribute.id,
                "forward-identity": identity_json(&current_attribute.forward_identity),
                "reverse-identity": current_attribute.reverse_identity.as_ref().map(identity_json),
                "cardinality": new_attribute.cardinality,
                "unique?": new_attribute.unique,
                "index?": new_attribute.indexed,
                "required?": new_attribute.required,
                "on-delete": new_attribute.on_delete,
                "on-delete-reverse": new_attribute.on_delete_reverse,
            }]));
        }
    }
    steps
}

fn identity_json(identity: &(Option<Uuid>, String, String)) -> Value {
    json!([identity.0, &identity.1, &identity.2])
}

fn background_schema_step(
    action: &str,
    attribute: &SchemaAttribute,
    checked_data_type: Option<&str>,
) -> Value {
    let mut details = json!({
        "attr-id": attribute.id,
        "forward-identity": identity_json(&attribute.forward_identity),
    });
    if let Some(checked_data_type) = checked_data_type {
        details["checked-data-type"] = Value::String(checked_data_type.to_owned());
    }
    json!([action, details])
}

#[allow(clippy::too_many_lines)] // Mirrors the reference planner's two conflict-validation passes.
fn validate_schema_plan(
    current_attributes: &[AdminAttribute],
    steps: &[Value],
) -> Result<(), ApiError> {
    let current_links = current_attributes
        .iter()
        .filter_map(|attribute| {
            attribute.reverse_identity.as_ref().map(|reverse| {
                (
                    (
                        attribute.forward_identity.1.clone(),
                        attribute.forward_identity.2.clone(),
                    ),
                    (reverse.1.clone(), reverse.2.clone()),
                )
            })
        })
        .collect::<Vec<_>>();
    let forward_links = current_links.iter().cloned().collect::<HashMap<_, _>>();
    let reverse_links = current_links
        .iter()
        .map(|(forward, reverse)| (reverse.clone(), forward.clone()))
        .collect::<HashMap<_, _>>();
    let blobs = current_attributes
        .iter()
        .filter(|attribute| attribute.value_type == "blob")
        .map(|attribute| {
            (
                attribute.forward_identity.1.clone(),
                attribute.forward_identity.2.clone(),
            )
        })
        .collect::<HashSet<_>>();
    let mut errors = Vec::new();
    for step in steps {
        let Some(parts) = step.as_array() else {
            continue;
        };
        let Some(action) = parts.first().and_then(Value::as_str) else {
            continue;
        };
        let Some(attribute) = parts.get(1).and_then(Value::as_object) else {
            continue;
        };
        let forward = schema_identity_name(attribute.get("forward-identity"));
        let reverse = schema_identity_name(attribute.get("reverse-identity"));
        if action == "add-attr"
            && let Some(forward) = &forward
        {
            let backward = reverse_links.get(forward);
            let message = if !(reverse.is_none() && backward.is_none())
                && reverse.as_ref() == backward
            {
                Some(backwards_link_message(forward))
            } else if forward_links.contains_key(forward) || reverse_links.contains_key(forward) {
                Some(duplicate_schema_message(forward))
            } else if reverse.as_ref().is_some_and(|reverse| {
                forward_links.contains_key(reverse) || reverse_links.contains_key(reverse)
            }) {
                Some(duplicate_schema_message(
                    reverse.as_ref().expect("checked above"),
                ))
            } else if blobs.contains(forward) {
                Some(duplicate_schema_message(forward))
            } else {
                None
            };
            if let Some(message) = message {
                errors.push(json!({"in": ["schema"], "message": message}));
            }
        }
        if matches!(action, "add-attr" | "update-attr")
            && attribute.get("value-type").and_then(Value::as_str) == Some("ref")
        {
            let invalid_forward = attribute.get("cardinality").and_then(Value::as_str)
                == Some("many")
                && attribute.get("on-delete").and_then(Value::as_str) == Some("cascade");
            let invalid_reverse = attribute.get("unique?").and_then(Value::as_bool) == Some(false)
                && attribute.get("on-delete-reverse").and_then(Value::as_str) == Some("cascade");
            if invalid_forward && let Some(forward) = &forward {
                errors.push(json!({"in": ["schema"], "message": cascade_schema_message(forward)}));
            }
            if invalid_reverse && let Some(reverse) = &reverse {
                errors.push(json!({"in": ["schema"], "message": cascade_schema_message(reverse)}));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(
            validation("Schema plan validation failed").with_hint(json!({
                "record-type": "schema",
                "errors": errors,
            })),
        )
    }
}

fn schema_identity_name(value: Option<&Value>) -> Option<(String, String)> {
    let values = value?.as_array()?;
    Some((
        values.get(1)?.as_str()?.to_owned(),
        values.get(2)?.as_str()?.to_owned(),
    ))
}

fn duplicate_schema_message((etype, label): &(String, String)) -> String {
    format!(
        "{etype}->{label}: Duplicate entry found for attribute. Check your schema file for duplicate link definitions. If it's not in the schema file, it may have been generated by the backend. Check your full schema in the dashboard: https://www.instantdb.com/dash?s=main&t=explorer"
    )
}

fn backwards_link_message((etype, label): &(String, String)) -> String {
    format!(
        "{etype}->{label}: Conflicting link found for attribute. It's possible that you already have a link with the same label names, but in the reverse direction. We cannot automatically swap the direction of the link. To fix this, can: a) swap the `forward` and `reverse` parameters for this link in your schema file, or b) delete the existing link in the dashboard.Check your full schema in the dashboard for a link with the same label names: https://www.instantdb.com/dash?s=main&t=explorer"
    )
}

fn cascade_schema_message((etype, label): &(String, String)) -> String {
    format!(
        "{etype}->{label}: Cascade delete is only possible on links with `has: 'one'`. Check your full schema in the dashboard: https://www.instantdb.com/dash?s=main&t=explorer"
    )
}

pub(crate) async fn apply_schema_plan(
    client: &mut Client,
    app_id: Uuid,
    attributes: &[AdminAttribute],
    input_steps: &[Value],
    allow_deletes: bool,
) -> Result<(Value, Vec<Uuid>), ApiError> {
    apply_schema_plan_with_app_status_write_check(
        client,
        app_id,
        attributes,
        input_steps,
        allow_deletes,
        AppStatusWriteCheck::default(),
    )
    .await
}

pub(crate) async fn apply_schema_plan_with_app_status_write_check(
    client: &mut Client,
    app_id: Uuid,
    attributes: &[AdminAttribute],
    input_steps: &[Value],
    allow_deletes: bool,
    app_status_write_check: AppStatusWriteCheck,
) -> Result<(Value, Vec<Uuid>), ApiError> {
    let direct_steps = input_steps
        .iter()
        .filter(|step| {
            step.as_array()
                .and_then(|step| step.first())
                .and_then(Value::as_str)
                .is_some_and(|action| {
                    matches!(action, "add-attr" | "update-attr")
                        || allow_deletes && action == "delete-attr"
                })
        })
        .cloned()
        .map(normalize_schema_direct_step)
        .collect::<Vec<_>>();
    let transaction = if direct_steps.is_empty() {
        Value::Null
    } else {
        let catalog = attribute_catalog_from_admin(attributes)?;
        let raw = Value::Array(direct_steps);
        let compiled = compile_transaction(&raw, &catalog).map_err(|error| {
            validation_owned(error.to_string()).with_hint(json!({
                "in": error.path,
                "original-input": raw,
            }))
        })?;
        crate::admin_data::validate_dynamic_reserved_names(client, &compiled).await?;
        let receipt = execute_transaction_with_app_status_write_check(
            client,
            app_id,
            &catalog,
            &compiled,
            TransactionOptions {
                admin: true,
                ..TransactionOptions::default()
            },
            app_status_write_check,
        )
        .await
        .map_err(|error| {
            crate::admin_data::transaction_execution_error_with_catalog(error, Some(&catalog))
        })?;
        let results = receipt.schema_mutations.iter().fold(
            serde_json::Map::new(),
            |mut results, mutation| {
                let key = match mutation.operation {
                    instant_transact::TransactionOperation::AddAttr => "add-attr",
                    instant_transact::TransactionOperation::UpdateAttr => "update-attr",
                    instant_transact::TransactionOperation::DeleteAttr => "delete-attr",
                    instant_transact::TransactionOperation::RestoreAttr => "restore-attr",
                    _ => return results,
                };
                results
                    .entry(key.to_owned())
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .expect("schema mutation result groups are arrays")
                    .push(json!(mutation.attr_id));
                results
            },
        );
        json!({
            "id": receipt.id,
            "app_id": app_id,
            "created_at": DateTime::<Utc>::from_timestamp_millis(receipt.created_at_ms),
            "results": results,
        })
    };
    let (steps, indexing_jobs, jobs_to_enqueue) =
        create_schema_indexing_jobs(client, app_id, input_steps).await?;
    Ok((
        json!({
            "transaction": transaction,
            "steps": steps,
            "indexing-jobs": indexing_jobs,
        }),
        jobs_to_enqueue,
    ))
}

fn normalize_schema_direct_step(mut step: Value) -> Value {
    let Some(parts) = step.as_array_mut() else {
        return step;
    };
    if parts.first().and_then(Value::as_str) != Some("add-attr") {
        return step;
    }
    if let Some(attribute) = parts.get_mut(1).and_then(Value::as_object_mut)
        && attribute
            .get("checked-data-type")
            .is_some_and(Value::is_null)
    {
        attribute.remove("checked-data-type");
    }
    step
}

fn attribute_catalog_from_admin(
    attributes: &[AdminAttribute],
) -> Result<AttributeCatalog, ApiError> {
    crate::admin_metadata::attributes_to_catalog(attributes)
}

async fn create_schema_indexing_jobs(
    client: &Client,
    app_id: Uuid,
    input_steps: &[Value],
) -> Result<(Vec<Value>, Value, Vec<Uuid>), ApiError> {
    let group_id = Uuid::new_v4();
    let mut output_steps = Vec::with_capacity(input_steps.len());
    let mut jobs = Vec::new();
    let mut job_ids = Vec::new();
    for step in input_steps {
        let Some(parts) = step.as_array() else {
            return Err(validation("schema steps must be collections"));
        };
        let action = parts
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| validation("schema step action must be a string"))?;
        let Some((serial_key, initial_stage)) = indexing_job_spec(action) else {
            output_steps.push(step.clone());
            continue;
        };
        let details = parts
            .get(1)
            .and_then(Value::as_object)
            .ok_or_else(|| validation("background schema steps require an options object"))?;
        let attr_id = details
            .get("attr-id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| validation("background schema steps require an attr-id"))?;
        let checked_data_type = if action == "check-data-type" {
            Some(
                details
                    .get("checked-data-type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        validation("check-data-type steps require a checked-data-type")
                    })?,
            )
        } else {
            None
        };
        let job_id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO indexing_jobs (
                   id, app_id, group_id, attr_id, job_serial_key, job_type,
                   job_status, job_stage, checked_data_type
                 ) VALUES (
                   $1, $2, $3, $4, $5, $6, 'waiting', $7,
                   CASE WHEN $8::text IS NULL THEN NULL ELSE $8::text::checked_data_type END
                 )",
                &[
                    &job_id,
                    &app_id,
                    &group_id,
                    &attr_id,
                    &serial_key,
                    &action,
                    &initial_stage,
                    &checked_data_type,
                ],
            )
            .await
            .map_err(database_error)?;
        let row = client
            .query_one(&indexing_job_query("j.id = $2"), &[&app_id, &job_id])
            .await
            .map_err(database_error)?;
        jobs.push(indexing_job_json(&row));
        job_ids.push(job_id);
        let mut output = step.clone();
        output
            .get_mut(1)
            .and_then(Value::as_object_mut)
            .expect("validated background step options")
            .insert("job-id".to_owned(), json!(job_id));
        output_steps.push(output);
    }
    let indexing_jobs = if jobs.is_empty() {
        Value::Null
    } else {
        json!({"group-id": group_id, "jobs": jobs})
    };
    Ok((output_steps, indexing_jobs, job_ids))
}

const fn cardinality_string(cardinality: Cardinality) -> &'static str {
    match cardinality {
        Cardinality::One => "one",
        Cardinality::Many => "many",
    }
}

pub(crate) fn attributes_to_dashboard_schema(attributes: &[AdminAttribute]) -> Value {
    let mut refs = serde_json::Map::new();
    let mut blobs = serde_json::Map::new();
    for source in attributes
        .iter()
        .filter(|attribute| dashboard_attribute_visible(attribute))
    {
        let mut attribute = source.clone();
        let (_, forward_etype, forward_label) = &attribute.forward_identity;
        if forward_etype == "$files" && forward_label == "url" {
            attribute.required = true;
        }
        let encoded = serde_json::to_value(&attribute)
            .expect("dashboard attributes always have a JSON representation");
        if attribute.value_type == "ref" {
            if let Some((_, reverse_etype, reverse_label)) = &attribute.reverse_identity {
                refs.insert(
                    clojure_vector_key([
                        forward_etype,
                        forward_label,
                        reverse_etype,
                        reverse_label,
                    ]),
                    encoded,
                );
            }
        } else if attribute.value_type == "blob" {
            blobs
                .entry(forward_etype.clone())
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .expect("dashboard blob namespace is an object")
                .insert(forward_label.clone(), encoded);
        }
    }
    json!({"refs": refs, "blobs": blobs})
}

fn dashboard_attribute_visible(attribute: &AdminAttribute) -> bool {
    let (_, etype, label) = &attribute.forward_identity;
    if attribute.catalog == AttributeCatalogKind::System
        && !matches!(etype.as_str(), "$users" | "$files" | "$streams")
    {
        return false;
    }
    !(etype == "$files"
        && matches!(
            label.as_str(),
            "content-type" | "content-disposition" | "size" | "location-id" | "key-version"
        )
        || etype == "$streams" && matches!(label.as_str(), "machineId" | "hashedReconnectToken"))
}

fn clojure_vector_key(values: [&String; 4]) -> String {
    let values = values.map(|value| {
        serde_json::to_string(value).expect("attribute identity strings always serialize")
    });
    format!("[{} {} {} {}]", values[0], values[1], values[2], values[3])
}

async fn entity_created_at<C>(client: &C, app_id: Uuid, entity_id: Uuid) -> Result<i64, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_one(
            "SELECT min(created_at) FROM triples WHERE app_id = $1 AND entity_id = $2",
            &[&app_id, &entity_id],
        )
        .await
        .map_err(database_error)?
        .get::<_, Option<i64>>(0)
        .ok_or_else(|| record_not_found("system-entity"))
}

async fn system_entity_ids<C>(
    client: &C,
    app_id: Uuid,
    etype: &str,
) -> Result<Vec<(Uuid, i64)>, ApiError>
where
    C: GenericClient + Sync,
{
    let id_attr = system_attr_id(etype, "id").map_err(auth_model_error)?;
    client
        .query(
            "SELECT entity_id, min(created_at) AS created_at
             FROM triples WHERE app_id = $1 AND attr_id = $2
             GROUP BY entity_id ORDER BY created_at DESC, entity_id",
            &[&app_id, &id_attr],
        )
        .await
        .map_err(database_error)
        .map(|rows| {
            rows.into_iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect()
        })
}

fn oauth_client_json(record: &OAuthClientRecord, created_at: i64, include_redirect: bool) -> Value {
    let mut value = json!({
        "id": record.id,
        "provider_id": record.provider_id,
        "client_name": record.name,
        "client_id": record.client_id,
        "extra_audiences": record.extra_audiences,
        "created_at": created_at,
        "meta": record.meta,
        "discovery_endpoint": record.discovery_endpoint,
        "use_shared_credentials": record.use_shared_credentials,
    });
    if include_redirect {
        value["redirect_to"] = json!(record.redirect_to);
    }
    value
}

fn auth_model_error(error: impl std::fmt::Display) -> ApiError {
    internal_error(error)
}

fn app_title_value(body: &Value) -> Result<String, ApiError> {
    let value = body.get("title").ok_or_else(|| missing("title"))?;
    let title = match value {
        Value::String(value) => value.trim().to_owned(),
        Value::Number(value) => value.to_string(),
        _ => return Err(malformed("Malformed title")),
    };
    (!title.is_empty())
        .then_some(title)
        .ok_or_else(|| malformed("Malformed title"))
}

fn member_role_value(body: &Value) -> Result<String, ApiError> {
    let value = required_string(body, "role")?;
    if matches!(value, "collaborator" | "admin" | "owner") {
        Ok(value.to_owned())
    } else {
        Err(validation("Invalid role"))
    }
}

fn email_value(body: &Value, key: &'static str) -> Result<String, ApiError> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let email = required_string(body, key)?.trim().to_lowercase();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$",
        )
        .expect("dashboard email regex is static")
    });
    pattern
        .is_match(&email)
        .then_some(email)
        .ok_or_else(|| malformed("Malformed email"))
}

fn org_title_value(body: &Value) -> Result<String, ApiError> {
    let title = app_title_value(body)?;
    if title.chars().count() <= 140 {
        Ok(title)
    } else {
        Err(validation(
            "Organization titles may not exceed 140 characters",
        ))
    }
}

fn uuid_value(body: &Value, key: &'static str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(required_string(body, key)?).map_err(|_| malformed("Malformed UUID parameter"))
}

fn optional_uuid_value(body: &Value, key: &'static str) -> Result<Option<Uuid>, ApiError> {
    body.get(key)
        .map(|value| {
            value
                .as_str()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| malformed("Malformed UUID parameter"))
        })
        .transpose()
}

fn json_response(value: Value) -> Result<DashboardResponse, ApiError> {
    let encoded = serde_json::to_vec(&value);
    drop(value);
    encoded.map(DashboardResponse::json).map_err(internal_error)
}

fn require_array(value: &Value, message: &'static str) -> Result<(), ApiError> {
    value
        .is_array()
        .then_some(())
        .ok_or_else(|| ApiError::new(ApiErrorType::Unknown, message))
}

fn require_object(value: &Value, message: &'static str) -> Result<(), ApiError> {
    value
        .is_object()
        .then_some(())
        .ok_or_else(|| ApiError::new(ApiErrorType::Unknown, message))
}

fn missing(field: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing {field} parameter"),
    )
}

fn malformed(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, message)
}

fn validation(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn validation_owned(message: String) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn permission(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::PermissionDenied, message).unauthorized()
}

fn config_flag_error(error: ConfigAppError) -> ApiError {
    match error {
        ConfigAppError::Database(error) => database_error(error),
        other => internal_error(other),
    }
}

fn record_not_found(record: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Record not found: {record}"),
    )
    .with_hint(json!({"record-type": record}))
}

fn test_user_insert_error(error: tokio_postgres::Error) -> ApiError {
    if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        ApiError::new(
            ApiErrorType::RecordNotUnique,
            "Record not unique: app-test-user",
        )
        .with_hint(json!({"record-type": "app-test-user"}))
    } else {
        database_error(error)
    }
}

#[cfg(test)]
mod tests {
    use instant_permissions::InMemoryRateLimiter;

    use super::*;

    #[test]
    fn dashboard_rate_limit_store_errors_hide_the_backend_cause() {
        let error = dashboard_rate_limit_store_error("relation \"secret_buckets\" does not exist");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Dashboard rate-limit store failed");
        assert!(!error.body().message.contains("secret_buckets"));
    }

    #[test]
    fn dashboard_login_expiry_defaults_to_fifteen_and_clamps_at_one_hour() {
        assert_eq!(configured_login_expiry(None), 15);
        assert_eq!(configured_login_expiry(Some(0)), 1);
        assert_eq!(configured_login_expiry(Some(30)), 30);
        assert_eq!(configured_login_expiry(Some(60)), 60);
        assert_eq!(configured_login_expiry(Some(61)), 60);
        assert_eq!(configured_login_expiry(Some(1_440)), 60);
    }

    #[derive(Debug, Default)]
    struct TestMagicCodeRateLimiter {
        inner: InMemoryRateLimiter,
    }

    impl TestMagicCodeRateLimiter {
        fn check<'a>(
            &'a self,
            bucket_name: &'static str,
            app_id: Uuid,
            email: &'a str,
        ) -> crate::MagicCodeRateLimitFuture<'a> {
            Box::pin(async move {
                let decision = self
                    .inner
                    .consume(&RateLimitRequest {
                        app_id,
                        bucket_name: bucket_name.to_owned(),
                        config: RateLimitBucketConfig {
                            limits: vec![RateLimit {
                                capacity: 1,
                                refill_amount: 1,
                                refill_period: Duration::from_hours(1),
                                refill_type: RefillType::Interval,
                            }],
                        },
                        bucket_key: json!(email.trim().to_ascii_lowercase()),
                        tokens: 1,
                    })
                    .map_err(|error| ApiError::new(ApiErrorType::Unknown, error))?;
                if matches!(decision, RateLimitDecision::Allowed { .. }) {
                    Ok(())
                } else {
                    Err(ApiError::new(ApiErrorType::RateLimited, "rate limited"))
                }
            })
        }
    }

    impl crate::MagicCodeRateLimiter for TestMagicCodeRateLimiter {
        fn check_send<'a>(
            &'a self,
            app_id: Uuid,
            email: &'a str,
        ) -> crate::MagicCodeRateLimitFuture<'a> {
            self.check("create", app_id, email)
        }

        fn check_verify<'a>(
            &'a self,
            app_id: Uuid,
            email: &'a str,
        ) -> crate::MagicCodeRateLimitFuture<'a> {
            self.check("consume", app_id, email)
        }

        fn check_custom_sender<'a>(
            &'a self,
            email: &'a str,
        ) -> crate::MagicCodeRateLimitFuture<'a> {
            self.check("custom-sender-verify", Uuid::nil(), email)
        }
    }

    #[tokio::test]
    async fn production_terminal_rejects_unclaimed_operations() {
        let terminal = ProductionDashboardTerminalHooks;
        assert_eq!(
            terminal
                .execute(DashboardControlHookOperation::AfterCreate {
                    app_id: Uuid::nil(),
                    body: json!({}),
                })
                .await
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            terminal
                .execute(DashboardControlHookOperation::AfterTransfer {
                    app_id: Uuid::nil(),
                    org_id: Uuid::nil(),
                    user_id: Uuid::nil(),
                    removed_members: json!([]),
                })
                .await
                .unwrap(),
            json!({"credit": null, "app_member_changes": {"removed": []}})
        );
        assert!(
            terminal
                .execute(DashboardControlHookOperation::SessionCounts)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dashboard_rate_limits_email_and_backup_operations() {
        let hooks = DashboardRateLimitHooks::new(
            NoopDashboardControlHooks,
            Arc::new(InMemoryRateLimiter::default()),
            Arc::new(TestMagicCodeRateLimiter::default()),
        );
        let backup = || DashboardControlHookOperation::AuthorizeBackup {
            app_id: Uuid::nil(),
            creator_id: None,
            work_estimate: 1,
            client_ip: None,
        };
        assert_eq!(hooks.execute(backup()).await.unwrap(), Value::Bool(true));
        assert_eq!(hooks.execute(backup()).await.unwrap(), Value::Bool(false));

        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeEmailSend {
                    kind: "sender-verification",
                    app_id: Uuid::new_v4(),
                    email: "USER@example.com".to_owned(),
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeEmailSend {
                    kind: "sender-verification",
                    app_id: Uuid::new_v4(),
                    email: "user@example.com".to_owned(),
                })
                .await
                .unwrap_err()
                .body()
                .kind,
            ApiErrorType::RateLimited
        );

        let app_id = Uuid::new_v4();
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeEmailSend {
                    kind: "test",
                    app_id,
                    email: "user@example.com".to_owned(),
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeEmailSend {
                    kind: "test",
                    app_id,
                    email: "USER@example.com".to_owned(),
                })
                .await
                .unwrap_err()
                .body()
                .kind,
            ApiErrorType::RateLimited
        );
    }

    #[test]
    fn upstream_unicode_dashboard_upload_preserves_query_path_and_legacy_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("path", "legacy.txt".parse().unwrap());
        assert_eq!(dashboard_upload_path(None, &headers).unwrap(), "legacy.txt");
        assert_eq!(
            dashboard_upload_path(
                Some("path=%20%E8%B3%87%E6%96%99%2Fcaf%C3%A9.txt%20"),
                &headers
            )
            .unwrap(),
            " 資料/café.txt "
        );
        assert!(dashboard_upload_path(Some("path=%20"), &headers).is_err());
    }

    #[test]
    fn backup_rate_limit_flags_validate_positive_integer_fields() {
        let configuration = json!({
            "app": {"capacity": 7, "windowMinutes": 11},
            "ip": {"capacity": 0, "windowMinutes": "invalid"},
        });
        assert_eq!(
            backup_rate_limit(&configuration, "app", 1, 5),
            (7, Duration::from_mins(11))
        );
        assert_eq!(
            backup_rate_limit(&configuration, "ip", 2, 5),
            (2, Duration::from_mins(5))
        );
    }

    #[test]
    fn optional_sender_email_treats_the_dashboard_blank_value_as_default() {
        assert_eq!(
            optional_email_value(&json!({"sender-email": ""}), "sender-email").unwrap(),
            None
        );
        assert_eq!(
            optional_email_value(&json!({"sender-email": "  "}), "sender-email").unwrap(),
            None
        );
        assert_eq!(
            optional_email_value(
                &json!({"sender-email": "CUSTOM@EXAMPLE.COM"}),
                "sender-email"
            )
            .unwrap(),
            Some("custom@example.com".to_owned())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn classifier_and_path_parsers_are_exact() {
        let app = Uuid::new_v4();
        let org = Uuid::new_v4();
        assert_eq!(direct_app_id(&format!("/dash/apps/{app}")), Some(app));
        assert_eq!(
            action_app_id(&format!("/dash/apps/{app}/status"), "status"),
            Some(app)
        );
        assert_eq!(
            transfer_ids(&format!("/dash/apps/{app}/transfer_to_org/{org}")),
            Some((app, org))
        );
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/rename")
        ));
        assert_eq!(
            team_target(
                &format!("/dash/orgs/{org}/members/update"),
                "members/update"
            ),
            Some(TeamTarget::Org(org))
        );
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/invite/send")
        ));
        assert!(is_control_route("POST", "/dash/invites/accept"));
        assert_eq!(
            claim_app_id(&format!("/dash/apps/ephemeral/{app}/claim")),
            Some(app)
        );
        assert_eq!(claim_app_id(&format!("/dash/apps/{app}/claim")), Some(app));
        assert_eq!(
            special_app_id(&format!("/dash/apps/ephemeral/{app}"), true, None),
            Some(app)
        );
        assert_eq!(
            special_app_id(
                &format!("/dash/apps/ephemeral/{app}/status"),
                true,
                Some("status")
            ),
            Some(app)
        );
        assert_eq!(
            special_app_id(&format!("/dash/apps/get_a_db/{app}"), false, None),
            Some(app)
        );
        assert!(is_control_route("POST", "/dash/apps/ephemeral"));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/ephemeral/{app}/status")
        ));
        assert!(!is_control_route(
            "GET",
            &format!("/dash/apps/ephemeral/{app}/extra")
        ));
        let origin = Uuid::new_v4();
        assert_eq!(
            authorized_origin_ids(&format!(
                "/dash/apps/{app}/authorized_redirect_origins/{origin}"
            )),
            Some((app, origin))
        );
        let oauth_client = Uuid::new_v4();
        assert_eq!(
            oauth_client_ids(&format!("/dash/apps/{app}/oauth_clients/{oauth_client}")),
            Some((app, oauth_client))
        );
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/oauth_clients")
        ));
        let job = Uuid::new_v4();
        assert_eq!(
            indexing_job_ids(&format!("/dash/apps/{app}/indexing-jobs/{job}")),
            Some((app, job))
        );
        assert_eq!(
            indexing_job_group_ids(&format!("/dash/apps/{app}/indexing-jobs/group/{job}")),
            Some((app, job))
        );
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/indexing-jobs")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/schema/push/plan")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/schema/push/apply")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/schema/steps/apply")
        ));
        assert!(is_control_route(
            "PUT",
            &format!("/dash/apps/{app}/storage/upload")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/storage/files/delete")
        ));
        let backup = Uuid::new_v4();
        assert_eq!(
            backup_ids(&format!("/dash/apps/{app}/backups/{backup}")),
            Some((app, backup))
        );
        assert_eq!(
            backup_action_ids(
                &format!("/dash/apps/{app}/backups/{backup}/storage-files"),
                "storage-files"
            ),
            Some((app, backup))
        );
        assert_eq!(
            backup_action_ids(
                &format!("/dash/apps/{app}/backups/{backup}/restore"),
                "restore"
            ),
            Some((app, backup))
        );
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/backups/{backup}/restore")
        ));
        assert!(is_control_route(
            "GET",
            &format!("/dash/apps/{app}/restore-jobs")
        ));
        assert_eq!(
            backup_job_ids(&format!("/dash/apps/{app}/backup-jobs/{backup}")),
            Some((app, backup))
        );
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/backups")
        ));
        assert!(is_control_route(
            "GET",
            &format!("/dash/apps/{app}/backups/{backup}/files")
        ));
        assert!(is_control_route("GET", &format!("/dash/apps/{app}/stats")));
        assert!(is_control_route("GET", &format!("/dash/apps/{app}/usage")));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/rules/validate")
        ));
        assert!(!is_control_route(
            "GET",
            &format!("/dash/apps/{app}/rules/validate")
        ));
        assert!(is_control_route("GET", "/dash/apps-summary"));
        assert!(!is_control_route("POST", "/dash/apps-summary"));
        assert_eq!(
            operator_flag_name("/dash/operator/flags/combine-transacts"),
            Some("combine-transacts")
        );
        assert_eq!(operator_flag_name("/dash/operator/flags/"), None);
        assert_eq!(operator_flag_name("/dash/operator/flags/a/b"), None);
        assert!(is_control_route("GET", "/dash/operator/flags"));
        assert!(is_control_route(
            "PUT",
            "/dash/operator/flags/combine-transacts"
        ));
        assert!(is_control_route(
            "DELETE",
            "/dash/operator/flags/combine-transacts"
        ));
        assert!(!is_control_route(
            "POST",
            "/dash/operator/flags/combine-transacts"
        ));
        assert!(!is_control_route(
            "GET",
            &format!("/dash/apps/{app}/usage/extra")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/track-import")
        ));
        assert!(is_control_route("GET", "/dash/session_counts"));
        assert!(is_control_route("GET", "/dash/stats/active_sessions"));
        assert!(is_control_route("GET", "/dash/default-email-template"));
        assert!(is_control_route(
            "GET",
            &format!("/dash/apps/{app}/sender-verification")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/sender-verification/send-magic-code")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/sender-verification/verify-magic-code")
        ));
        assert!(is_control_route(
            "GET",
            &format!("/dash/apps/{app}/email_status")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/email_templates")
        ));
        assert!(is_control_route(
            "DELETE",
            &format!("/dash/apps/{app}/email_templates/{oauth_client}")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/send-test-email")
        ));
        assert!(is_control_route(
            "GET",
            &format!("/dash/apps/{app}/oauth-apps")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/oauth-apps")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/oauth-apps/{oauth_client}")
        ));
        assert!(is_control_route(
            "DELETE",
            &format!("/dash/apps/{app}/oauth-apps/{oauth_client}")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/oauth-apps/{oauth_client}/clients")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/oauth-app-clients/{oauth_client}")
        ));
        assert!(is_control_route(
            "DELETE",
            &format!("/dash/apps/{app}/oauth-app-clients/{oauth_client}")
        ));
        assert!(is_control_route(
            "POST",
            &format!("/dash/apps/{app}/oauth-app-clients/{oauth_client}/client-secrets")
        ));
        assert!(is_control_route(
            "DELETE",
            &format!("/dash/apps/{app}/oauth-app-client-secrets/{oauth_client}")
        ));
        assert!(is_control_route("GET", "/dash/user/oauth_apps"));
        assert!(is_control_route(
            "POST",
            "/dash/user/oauth_apps/revoke_access"
        ));
        let restore_job = Uuid::new_v4();
        assert_eq!(
            restore_job_id(&format!("/dash/restore-jobs/{restore_job}")),
            Some(restore_job)
        );
        assert!(is_control_route("POST", "/dash/restores/zip"));
        assert!(is_control_route("GET", "/dash/restore-jobs"));
        for (method, path) in [
            ("GET", "/dash/admin"),
            ("GET", "/dash/check-admin"),
            ("GET", "/dash/top"),
            ("GET", "/dash/storage"),
            ("GET", "/dash/overview/daily"),
            ("GET", "/dash/overview/minute"),
            ("GET", "/dash/oauth/start"),
            ("GET", "/dash/oauth/callback"),
            ("POST", "/dash/oauth/token"),
        ] {
            assert!(is_control_route(method, path), "missing {method} {path}");
        }
        assert!(is_control_route(
            "GET",
            &format!("/dash/restore-jobs/{restore_job}")
        ));
        assert!(is_control_route(
            "DELETE",
            &format!("/dash/restore-jobs/{restore_job}")
        ));
        assert!(!is_control_route(
            "POST",
            &format!("/dash/restore-jobs/{restore_job}")
        ));
        assert!(!is_control_route(
            "GET",
            &format!("/dash/restore-jobs/{restore_job}/extra")
        ));
        assert!(!is_control_route("POST", "/dash/apps/not-a-uuid/status"));
    }

    #[test]
    fn apps_summary_workspace_is_personal_or_an_org_id() {
        assert_eq!(
            DashboardWorkspace::parse("personal").unwrap(),
            DashboardWorkspace::Personal
        );
        let org_id = Uuid::new_v4();
        assert_eq!(
            DashboardWorkspace::parse(&org_id.to_string()).unwrap(),
            DashboardWorkspace::Org(org_id)
        );
        assert!(DashboardWorkspace::parse("Personal").is_err());
        assert!(DashboardWorkspace::parse("not-an-id").is_err());
    }

    #[test]
    fn every_dashboard_declaration_has_a_live_rust_owner() {
        let parameter = Uuid::new_v4().to_string();
        for route in instant_http::DASHBOARD_ROUTES {
            if route.source_path == "/dash/ws_playground" {
                continue;
            }
            let path = route
                .source_path
                .split('/')
                .map(|part| {
                    if part.starts_with(':') {
                        parameter.as_str()
                    } else if part == "*" {
                        "1/0/0"
                    } else {
                        part
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                crate::dashboard::is_identity_route(route.method, &path)
                    || crate::dashboard_config::is_dashboard_config_route(route.method, &path)
                    || is_control_route(route.method, &path),
                "unowned dashboard declaration: {} {}",
                route.method,
                route.source_path
            );
        }
    }

    #[test]
    fn invite_fields_match_the_reference_coercions() {
        let body = json!({"invitee-email": " Team.Member@Example.COM ", "role": "admin"});
        assert_eq!(
            email_value(&body, "invitee-email").unwrap(),
            "team.member@example.com"
        );
        assert_eq!(member_role_value(&body).unwrap(), "admin");
        assert!(member_role_value(&json!({"role": "creator"})).is_err());
        assert!(validate_authorized_origin("generic", &["example.com".to_owned()]).is_ok());
        assert!(validate_authorized_origin("vercel", &["vercel.app".to_owned()]).is_err());
        assert!(validate_authorized_origin("custom-scheme", &["https".to_owned()]).is_err());
    }

    #[test]
    fn webhook_event_cursors_round_trip_jvm_binary_shape() {
        let app = Uuid::new_v4();
        let webhook = Uuid::new_v4();
        assert_eq!(
            webhook_ids(&format!("/dash/apps/{app}/webhooks/{webhook}")),
            Some((app, webhook))
        );
        assert_eq!(
            webhook_action_ids(
                &format!("/dash/apps/{app}/webhooks/{webhook}/events"),
                "events"
            ),
            Some((app, webhook))
        );
        let created_at = DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        let isn = "2/16/B374D848".parse::<instant_wal::Isn>().unwrap();
        assert_eq!(
            webhook_event_ids(&format!("/dash/apps/{app}/webhooks/{webhook}/events/{isn}")),
            Some((app, webhook, isn))
        );
        let cursor = encode_webhook_event_cursor(created_at, isn);
        assert_eq!(
            webhook_after_cursor(Some(&format!("after={cursor}"))).unwrap(),
            Some((created_at, isn))
        );
        assert!(webhook_after_cursor(Some("after=invalid")).is_err());
    }

    #[test]
    fn webhook_limit_uses_the_resolved_dynamic_boundary_and_message() {
        assert!(check_webhook_limit(6, 7).is_ok());
        let at_limit = check_webhook_limit(7, 7).unwrap_err();
        assert_eq!(at_limit.body().kind, ApiErrorType::ValidationFailed);
        assert_eq!(
            at_limit.body().message,
            "An app may not have more than 7 active webhooks."
        );
        assert!(check_webhook_limit(0, 0).is_err());
    }

    #[test]
    fn schema_planner_creates_entity_scalar_and_link_steps() {
        let plan = schema_push_plan(
            &[],
            &json!({
                "entities": {
                    "posts": {"attrs": {
                        "title": {
                            "valueType": "string",
                            "config": {"indexed": true, "unique": false},
                            "required": true
                        }
                    }}
                },
                "links": {
                    "postAuthor": {
                        "forward": {"on": "posts", "label": "author", "has": "one"},
                        "reverse": {"on": "users", "label": "posts", "has": "many"}
                    }
                }
            }),
            true,
            true,
        )
        .unwrap();
        let steps = plan["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0][0], "add-attr");
        assert_eq!(steps[0][1]["forward-identity"][2], "id");
        assert_eq!(steps[1][1]["checked-data-type"], "string");
        assert_eq!(steps[1][1]["required?"], true);
        assert_eq!(steps[2][1]["value-type"], "ref");
        assert_eq!(steps[2][1]["reverse-identity"][1], "users");
    }

    #[test]
    fn schema_steps_treat_null_checked_type_as_an_unchecked_new_attribute() {
        let normalized = normalize_schema_direct_step(json!(["add-attr", {
            "id": Uuid::new_v4(),
            "checked-data-type": null,
        }]));
        assert!(normalized[1].get("checked-data-type").is_none());
    }

    #[test]
    fn schema_planner_uses_background_jobs_for_existing_constraints() {
        let attr_id = Uuid::new_v4();
        let identity_id = Uuid::new_v4();
        let mut current = SchemaShape::default();
        current.blobs.insert(
            "posts".to_owned(),
            BTreeMap::from([(
                "title".to_owned(),
                SchemaAttribute {
                    id: Some(attr_id),
                    forward_identity: (Some(identity_id), "posts".to_owned(), "title".to_owned()),
                    reverse_identity: None,
                    value_type: "blob",
                    cardinality: "one",
                    unique: false,
                    indexed: false,
                    required: false,
                    checked_data_type: None,
                    on_delete: None,
                    on_delete_reverse: None,
                    catalog: Some("user"),
                },
            )]),
        );
        let mut new = current.clone();
        let changed = new
            .blobs
            .get_mut("posts")
            .unwrap()
            .get_mut("title")
            .unwrap();
        changed.indexed = true;
        changed.required = true;
        changed.checked_data_type = Some("string".to_owned());
        let steps = schema_diff_steps(&current, &new, true, true);
        assert_eq!(
            steps
                .iter()
                .map(|step| step[0].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["index", "required", "check-data-type"]
        );
        assert!(
            steps
                .iter()
                .all(|step| step[1]["attr-id"] == json!(attr_id))
        );
    }

    #[test]
    fn backup_client_ip_prefers_the_sanitized_socket_value() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.8".parse().unwrap());
        let request = DashboardRequest {
            method: axum::http::Method::POST,
            path: "/dash/apps/00000000-0000-4000-8000-000000000001/backups".to_owned(),
            query: None,
            headers,
            body: axum::body::Bytes::new(),
            staged_body_path: None,
            remote_addr: Some("203.0.113.9".to_owned()),
        };

        assert_eq!(
            forwarded_client_ip(&request).as_deref(),
            Some("203.0.113.9")
        );
    }
}
