use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;

use ipnet::IpNet;
use thiserror::Error;
use url::Url;

use serde::Deserialize;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerEnvironment {
    Development,
    Test,
    Staging,
    Production,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmailProvider {
    #[default]
    Auto,
    Postmark,
    SendGrid,
    Resend,
    Cloudflare,
}

impl ServerEnvironment {
    #[must_use]
    pub const fn is_hosted(self) -> bool {
        matches!(self, Self::Staging | Self::Production)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerStorageConfig {
    pub endpoint: Option<String>,
    pub public_endpoint: Option<String>,
    pub virtual_host_style: bool,
    pub region: String,
    pub bucket: String,
    pub max_upload_bytes: u64,
    pub download_url_ttl_seconds: u32,
}

/// Default cap for a single stored object (100 MiB).
pub const DEFAULT_STORAGE_MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
/// Default presigned download lifetime, matching the SDK (7 days).
pub const DEFAULT_STORAGE_DOWNLOAD_URL_TTL_SECONDS: u32 = 604_800;
/// Shortest accepted presigned download lifetime.
pub const MIN_STORAGE_DOWNLOAD_URL_TTL_SECONDS: u32 = 60;
/// S3's maximum presigned URL lifetime (7 days).
pub const MAX_STORAGE_DOWNLOAD_URL_TTL_SECONDS: u32 = 604_800;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerBackupStorageConfig {
    pub bucket: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerWalHistoryConfig {
    pub bucket: Option<String>,
    pub prefix: Option<String>,
}

/// Outbound email settings. This intentionally has no `Debug` implementation
/// so the Postmark server token cannot be logged through configuration dumps.
pub struct ServerEmailConfig {
    provider: EmailProvider,
    postmark_server_token: Option<String>,
    postmark_account_token: Option<String>,
    sendgrid_token: Option<String>,
    resend_token: Option<String>,
    cloudflare_account_id: Option<String>,
    cloudflare_email_api_token: Option<String>,
    app_sender_name: Option<String>,
    app_sender_email: Option<String>,
    dashboard_sender_name: String,
    dashboard_sender_email: Option<String>,
    team_sender_name: String,
    team_sender_email: Option<String>,
}

/// Decrypted shared OAuth credentials supplied by the process secret manager.
/// Deliberately does not implement `Debug`.
#[derive(Clone, Eq, PartialEq)]
pub struct SharedOAuthClientConfig {
    provider_name: String,
    client_id: String,
    client_secret: String,
}

impl SharedOAuthClientConfig {
    #[must_use]
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    #[must_use]
    pub fn client_secret(&self) -> &str {
        &self.client_secret
    }
}

#[derive(Deserialize)]
struct RawSharedOAuthClientConfig {
    provider_name: String,
    client_id: String,
    client_secret: String,
}

impl ServerEmailConfig {
    #[must_use]
    pub const fn provider(&self) -> EmailProvider {
        self.provider
    }

    #[must_use]
    pub fn postmark_server_token(&self) -> Option<&str> {
        self.postmark_server_token.as_deref()
    }

    #[must_use]
    pub fn postmark_account_token(&self) -> Option<&str> {
        self.postmark_account_token.as_deref()
    }

    #[must_use]
    pub fn resend_token(&self) -> Option<&str> {
        self.resend_token.as_deref()
    }

    #[must_use]
    pub fn sendgrid_token(&self) -> Option<&str> {
        self.sendgrid_token.as_deref()
    }

    #[must_use]
    pub fn cloudflare_account_id(&self) -> Option<&str> {
        self.cloudflare_account_id.as_deref()
    }

    #[must_use]
    pub fn cloudflare_email_api_token(&self) -> Option<&str> {
        self.cloudflare_email_api_token.as_deref()
    }

    #[must_use]
    pub fn app_sender(&self) -> (Option<&str>, Option<&str>) {
        (
            self.app_sender_name.as_deref(),
            self.app_sender_email.as_deref(),
        )
    }

    #[must_use]
    pub fn dashboard_sender(&self) -> (&str, Option<&str>) {
        (
            &self.dashboard_sender_name,
            self.dashboard_sender_email.as_deref(),
        )
    }

    #[must_use]
    pub fn team_sender(&self) -> (&str, Option<&str>) {
        (&self.team_sender_name, self.team_sender_email.as_deref())
    }
}

pub struct ServerConfig {
    environment: ServerEnvironment,
    database_url: String,
    database_cluster_id: Option<String>,
    next_database_url: Option<String>,
    next_database_cluster_id: Option<String>,
    bind_address: SocketAddr,
    server_origin: String,
    dashboard_origin: String,
    secure_cookies: bool,
    trusted_proxy_cidrs: Vec<IpNet>,
    connection_pool_size: usize,
    sketch_batch_policy: crate::SketchBatchPolicy,
    storage: ServerStorageConfig,
    backup_storage: ServerBackupStorageConfig,
    wal_history: ServerWalHistoryConfig,
    email: ServerEmailConfig,
    superuser_email: Option<String>,
    dashboard_allowed_emails: Option<HashSet<String>>,
    dashboard_signup_open: bool,
    ephemeral_apps_enabled: bool,
    nightly_app_backups: Option<bool>,
    google_oauth_client_id: Option<String>,
    google_oauth_client_secret: Option<String>,
    shared_oauth_clients: Vec<SharedOAuthClientConfig>,
    instant_on_instant_app_id: Option<Uuid>,
    sentry_dsn: Option<String>,
    sentry_environment: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ServerConfigError {
    #[error("{0} must be an integer")]
    InvalidInteger(&'static str),
    #[error("CONNECTION_POOL_SIZE must be greater than zero")]
    EmptyConnectionPool,
    #[error(
        "sketch batching requires 1..65536 transactions, 1..67108864 WAL bytes and 1..10000 milliseconds"
    )]
    InvalidSketchBatchPolicy,
    #[error("INSTANT_TRUSTED_PROXY_CIDRS contains an invalid CIDR: {0}")]
    InvalidTrustedProxyCidr(String),
    #[error("DATABASE_URL must be a PostgreSQL connection URL")]
    InvalidDatabaseUrl,
    #[error("{0} must be an absolute HTTP(S) URL")]
    InvalidHttpUrl(&'static str),
    #[error("INSTANT_BACKEND_URL is required in the production and staging profiles")]
    MissingBackendUrl,
    #[error("INSTANT_DASHBOARD_URL is required in the production and staging profiles")]
    MissingDashboardUrl,
    #[error("INSTANT_S3_REGION is required when S3_ENDPOINT is configured")]
    MissingStorageRegion,
    #[error("S3_BUCKET is required when S3_ENDPOINT is configured")]
    MissingStorageBucket,
    #[error("S3_VIRTUAL_HOST_STYLE must be true or false")]
    InvalidStorageAddressingStyle,
    #[error("INSTANT_STORAGE_MAX_UPLOAD_BYTES must be greater than zero")]
    InvalidStorageMaxUploadBytes,
    #[error(
        "INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID and INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET must be set together"
    )]
    IncompleteGoogleOAuthClient,
    #[error(
        "INSTANT_DASHBOARD_ALLOWED_EMAILS must be a non-empty comma-separated list of email addresses"
    )]
    InvalidDashboardAllowedEmails,
    #[error("INSTANT_DASHBOARD_SIGNUP must be open or closed")]
    InvalidDashboardSignup,
    #[error("INSTANT_NIGHTLY_APP_BACKUPS must be true or false")]
    InvalidNightlyAppBackups,
    #[error(
        "INSTANT_SHARED_OAUTH_CLIENTS must be a JSON array of unique, non-blank provider_name, client_id, and client_secret objects"
    )]
    InvalidSharedOAuthClients,
    #[error("INSTANT_ON_INSTANT_APP_ID must be a UUID")]
    InvalidInstantOnInstantAppId,
    #[error("INSTANT_EMAIL_PROVIDER must be postmark, sendgrid, or cloudflare")]
    InvalidEmailProvider,
    #[error(
        "Cloudflare email requires CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_EMAIL_API_TOKEN, and INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL"
    )]
    IncompleteCloudflareEmail,
    #[error("{0} is not supported by the self-hosted instant-server build")]
    AwsHostedFeatureRequired(&'static str),
}

impl ServerConfig {
    /// Loads and validates the JVM-compatible production environment contract.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed ports, pool sizes, database URLs, public
    /// origins/endpoints, a configured storage endpoint without a signing
    /// region, or a partially configured dashboard Google OAuth client.
    pub fn from_env() -> Result<Self, ServerConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Builds configuration from explicit environment-style pairs.
    ///
    /// This is useful for launchers that source configuration from a secret manager
    /// while preserving the same names and precedence as the JVM server.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::from_env`].
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, ServerConfigError>
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

    #[allow(clippy::too_many_lines)] // Keeps one fail-closed pass over process configuration.
    fn from_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, ServerConfigError> {
        let environment = if is_true(get("STAGING").as_deref()) {
            ServerEnvironment::Staging
        } else if is_true(get("PRODUCTION").as_deref()) {
            ServerEnvironment::Production
        } else if is_true(get("TEST").as_deref()) {
            ServerEnvironment::Test
        } else {
            ServerEnvironment::Development
        };
        let port = if let Some(value) = get("PORT") {
            parse_integer::<u16>("PORT", &value)?
        } else {
            default_port(environment)
        };
        let database_url = normalize_database_url(
            get("DATABASE_URL")
                .as_deref()
                .unwrap_or("postgresql://localhost:5432/instant"),
        )?;
        let next_database_url = non_blank(get("NEXT_DATABASE_URL"))
            .map(|value| normalize_database_url(&value))
            .transpose()?;
        let database_cluster_id = non_blank(get("DATABASE_CLUSTER_ID"));
        let next_database_cluster_id = non_blank(get("NEXT_DATABASE_CLUSTER_ID"));
        if database_cluster_id.is_some() {
            return Err(ServerConfigError::AwsHostedFeatureRequired(
                "DATABASE_CLUSTER_ID",
            ));
        }
        if next_database_cluster_id.is_some() {
            return Err(ServerConfigError::AwsHostedFeatureRequired(
                "NEXT_DATABASE_CLUSTER_ID",
            ));
        }
        let server_origin = match get("INSTANT_BACKEND_URL") {
            Some(value) => validated_http_url("INSTANT_BACKEND_URL", &value)?,
            None if environment.is_hosted() => return Err(ServerConfigError::MissingBackendUrl),
            None => validated_http_url("INSTANT_BACKEND_URL", &default_server_origin(port))?,
        };
        let dashboard_origin = match get("INSTANT_DASHBOARD_URL") {
            Some(value) => validated_http_url("INSTANT_DASHBOARD_URL", &value)?,
            None if environment.is_hosted() => {
                return Err(ServerConfigError::MissingDashboardUrl);
            }
            None => validated_http_url("INSTANT_DASHBOARD_URL", default_dashboard_origin())?,
        };
        let secure_cookies = Url::parse(&server_origin)
            .is_ok_and(|origin| origin.scheme() == "https")
            && (environment != ServerEnvironment::Development
                || is_true(get("INSTANT_SECURE_COOKIES").as_deref()));
        let trusted_proxy_cidrs = non_blank(get("INSTANT_TRUSTED_PROXY_CIDRS"))
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        value.parse::<IpNet>().map_err(|_| {
                            ServerConfigError::InvalidTrustedProxyCidr(value.to_owned())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let connection_pool_size = get("CONNECTION_POOL_SIZE").map_or_else(
            || Ok(if environment.is_hosted() { 400 } else { 20 }),
            |value| parse_integer::<usize>("CONNECTION_POOL_SIZE", &value),
        )?;
        if connection_pool_size == 0 {
            return Err(ServerConfigError::EmptyConnectionPool);
        }
        let defaults = crate::SketchBatchPolicy::default();
        let sketch_batch_policy = crate::SketchBatchPolicy {
            max_transactions: get("INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS")
                .map(|v| parse_integer("INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS", &v))
                .transpose()?
                .unwrap_or(defaults.max_transactions),
            max_wal_bytes: get("INSTANT_SKETCH_BATCH_MAX_WAL_BYTES")
                .map(|v| parse_integer("INSTANT_SKETCH_BATCH_MAX_WAL_BYTES", &v))
                .transpose()?
                .unwrap_or(defaults.max_wal_bytes),
            max_wait: std::time::Duration::from_millis(
                get("INSTANT_SKETCH_BATCH_MAX_WAIT_MS")
                    .map(|v| parse_integer("INSTANT_SKETCH_BATCH_MAX_WAIT_MS", &v))
                    .transpose()?
                    .unwrap_or(50),
            ),
        };
        if !sketch_batch_policy.valid() {
            return Err(ServerConfigError::InvalidSketchBatchPolicy);
        }
        let endpoint = optional_http_url("S3_ENDPOINT", get("S3_ENDPOINT"))?;
        let storage_region = non_blank(get("INSTANT_S3_REGION"));
        if endpoint.is_some() && storage_region.is_none() {
            return Err(ServerConfigError::MissingStorageRegion);
        }
        let storage_bucket = non_blank(get("S3_BUCKET"));
        if endpoint.is_some() && storage_bucket.is_none() {
            return Err(ServerConfigError::MissingStorageBucket);
        }
        let public_endpoint = optional_http_url(
            "S3_PUBLIC_ENDPOINT",
            get("S3_PUBLIC_ENDPOINT").or_else(|| endpoint.clone()),
        )?;
        let virtual_host_style = match get("S3_VIRTUAL_HOST_STYLE").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(ServerConfigError::InvalidStorageAddressingStyle),
        };
        // Blank counts as unset: compose files pass `${...:-}`.
        let max_upload_bytes = match non_blank(get("INSTANT_STORAGE_MAX_UPLOAD_BYTES")) {
            None => DEFAULT_STORAGE_MAX_UPLOAD_BYTES,
            Some(value) => {
                let parsed = parse_integer::<u64>("INSTANT_STORAGE_MAX_UPLOAD_BYTES", &value)?;
                if parsed == 0 {
                    return Err(ServerConfigError::InvalidStorageMaxUploadBytes);
                }
                parsed
            }
        };
        let download_url_ttl_seconds = non_blank(get("INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS"))
            .map_or(Ok(DEFAULT_STORAGE_DOWNLOAD_URL_TTL_SECONDS), |value| {
                parse_integer::<u32>("INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS", &value)
            })?
            .clamp(
                MIN_STORAGE_DOWNLOAD_URL_TTL_SECONDS,
                MAX_STORAGE_DOWNLOAD_URL_TTL_SECONDS,
            );
        let wal_history_bucket = if get("WAL_HISTORY_STORAGE").as_deref() == Some("pg") {
            None
        } else {
            non_blank(get("WAL_HISTORY_BUCKET_NAME"))
        };
        let wal_history_prefix = if environment == ServerEnvironment::Development {
            non_blank(get("HOSTNAME"))
        } else {
            None
        };
        let google_oauth_client_id = non_blank(get("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID"));
        let google_oauth_client_secret =
            non_blank(get("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET"));
        if google_oauth_client_id.is_some() != google_oauth_client_secret.is_some() {
            return Err(ServerConfigError::IncompleteGoogleOAuthClient);
        }
        let shared_oauth_clients =
            parse_shared_oauth_clients(non_blank(get("INSTANT_SHARED_OAUTH_CLIENTS")).as_deref())?;
        let instant_on_instant_app_id = non_blank(get("INSTANT_ON_INSTANT_APP_ID"))
            .map(|value| {
                Uuid::parse_str(&value).map_err(|_| ServerConfigError::InvalidInstantOnInstantAppId)
            })
            .transpose()?;
        // Blank counts as unset: compose files pass `${SENTRY_DSN:-}`.
        let sentry_dsn = non_blank(get("SENTRY_DSN"));
        let sentry_environment =
            non_blank(get("SENTRY_ENVIRONMENT")).unwrap_or_else(|| "production".to_owned());
        let email_provider = match non_blank(get("INSTANT_EMAIL_PROVIDER"))
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None => EmailProvider::Auto,
            Some("postmark") => EmailProvider::Postmark,
            Some("sendgrid") => EmailProvider::SendGrid,
            Some("resend") => EmailProvider::Resend,
            Some("cloudflare") => EmailProvider::Cloudflare,
            Some(_) => return Err(ServerConfigError::InvalidEmailProvider),
        };
        let cloudflare_account_id = non_blank(get("CLOUDFLARE_ACCOUNT_ID"));
        let cloudflare_email_api_token = non_blank(get("CLOUDFLARE_EMAIL_API_TOKEN"));
        let transactional_sender = non_blank(get("INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL"));
        if email_provider == EmailProvider::Cloudflare
            && (cloudflare_account_id.is_none()
                || cloudflare_email_api_token.is_none()
                || transactional_sender.is_none())
        {
            return Err(ServerConfigError::IncompleteCloudflareEmail);
        }
        let dashboard_signup_open = match non_blank(get("INSTANT_DASHBOARD_SIGNUP"))
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("closed") => false,
            Some("open") => true,
            Some(_) => return Err(ServerConfigError::InvalidDashboardSignup),
        };
        let ephemeral_apps_enabled = is_true(get("INSTANT_EPHEMERAL_APPS").as_deref());
        // Blank counts as unset: compose files pass `${INSTANT_NIGHTLY_APP_BACKUPS:-}`.
        let nightly_app_backups = match non_blank(get("INSTANT_NIGHTLY_APP_BACKUPS")).as_deref() {
            None => None,
            Some("true") => Some(true),
            Some("false") => Some(false),
            Some(_) => return Err(ServerConfigError::InvalidNightlyAppBackups),
        };
        Ok(Self {
            environment,
            database_url,
            database_cluster_id,
            next_database_url,
            next_database_cluster_id,
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            server_origin,
            dashboard_origin,
            secure_cookies,
            trusted_proxy_cidrs,
            connection_pool_size,
            sketch_batch_policy,
            storage: ServerStorageConfig {
                endpoint,
                public_endpoint,
                virtual_host_style,
                region: storage_region.unwrap_or_else(|| "us-east-1".to_owned()),
                bucket: storage_bucket.unwrap_or_default(),
                max_upload_bytes,
                download_url_ttl_seconds,
            },
            backup_storage: ServerBackupStorageConfig {
                bucket: non_blank(get("S3_APP_BACKUPS_BUCKET")),
            },
            wal_history: ServerWalHistoryConfig {
                bucket: wal_history_bucket,
                prefix: wal_history_prefix,
            },
            email: ServerEmailConfig {
                provider: email_provider,
                cloudflare_account_id,
                cloudflare_email_api_token,
                postmark_server_token: non_blank(get("POSTMARK_TOKEN")),
                postmark_account_token: non_blank(get("POSTMARK_ACCOUNT_TOKEN")),
                sendgrid_token: non_blank(get("SENDGRID_TOKEN")),
                resend_token: non_blank(get("RESEND_TOKEN")),
                app_sender_name: non_blank(get("INSTANT_APP_EMAIL_SENDER_NAME")),
                app_sender_email: non_blank(get("INSTANT_APP_EMAIL_SENDER_EMAIL"))
                    .or_else(|| transactional_sender.clone()),
                dashboard_sender_name: non_blank(get("INSTANT_DASHBOARD_EMAIL_SENDER_NAME"))
                    .unwrap_or_else(|| "Instant".to_owned()),
                dashboard_sender_email: non_blank(get("INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL"))
                    .or_else(|| transactional_sender.clone()),
                team_sender_name: non_blank(get("INSTANT_TEAM_EMAIL_SENDER_NAME"))
                    .unwrap_or_else(|| "Instant".to_owned()),
                team_sender_email: non_blank(get("INSTANT_TEAM_EMAIL_SENDER_EMAIL"))
                    .or_else(|| transactional_sender.clone()),
            },
            superuser_email: non_blank(get("INSTANT_SUPERUSER_EMAIL"))
                .map(|email| email.to_ascii_lowercase()),
            dashboard_allowed_emails: non_blank(get("INSTANT_DASHBOARD_ALLOWED_EMAILS"))
                .map(|value| {
                    let emails = value
                        .split(',')
                        .map(|email| email.trim().to_ascii_lowercase())
                        .collect::<HashSet<_>>();
                    if emails.iter().any(|email| {
                        email.contains(char::is_whitespace)
                            || !matches!(email.split_once('@'), Some((local, domain)) if !local.is_empty() && domain.contains('.') && !domain.contains('@'))
                    }) {
                        return Err(ServerConfigError::InvalidDashboardAllowedEmails);
                    }
                    Ok(emails)
                })
                .transpose()?,
            dashboard_signup_open,
            ephemeral_apps_enabled,
            nightly_app_backups,
            google_oauth_client_id,
            google_oauth_client_secret,
            shared_oauth_clients,
            instant_on_instant_app_id,
            sentry_dsn,
            sentry_environment,
        })
    }

    #[must_use]
    pub const fn environment(&self) -> ServerEnvironment {
        self.environment
    }

    #[must_use]
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    #[must_use]
    pub fn database_cluster_id(&self) -> Option<&str> {
        self.database_cluster_id.as_deref()
    }

    #[must_use]
    pub fn next_database_url(&self) -> Option<&str> {
        self.next_database_url.as_deref()
    }

    #[must_use]
    pub fn next_database_cluster_id(&self) -> Option<&str> {
        self.next_database_cluster_id.as_deref()
    }

    #[must_use]
    pub const fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    #[must_use]
    pub fn server_origin(&self) -> &str {
        &self.server_origin
    }

    #[must_use]
    pub fn dashboard_origin(&self) -> &str {
        &self.dashboard_origin
    }

    #[must_use]
    pub const fn secure_cookies(&self) -> bool {
        self.secure_cookies
    }

    #[must_use]
    pub fn trusted_proxy_cidrs(&self) -> &[IpNet] {
        &self.trusted_proxy_cidrs
    }

    #[must_use]
    pub const fn connection_pool_size(&self) -> usize {
        self.connection_pool_size
    }

    #[must_use]
    pub const fn sketch_batch_policy(&self) -> crate::SketchBatchPolicy {
        self.sketch_batch_policy
    }

    #[must_use]
    pub const fn storage(&self) -> &ServerStorageConfig {
        &self.storage
    }

    #[must_use]
    pub const fn backup_storage(&self) -> &ServerBackupStorageConfig {
        &self.backup_storage
    }

    #[must_use]
    pub const fn wal_history(&self) -> &ServerWalHistoryConfig {
        &self.wal_history
    }

    #[must_use]
    pub const fn email(&self) -> &ServerEmailConfig {
        &self.email
    }

    #[must_use]
    pub fn superuser_email(&self) -> Option<&str> {
        self.superuser_email.as_deref()
    }

    /// Optional immutable restriction on dashboard email sign-in and signup.
    /// Runtime application-user authentication is not affected.
    #[must_use]
    pub fn dashboard_allowed_emails(&self) -> Option<&HashSet<String>> {
        self.dashboard_allowed_emails.as_ref()
    }

    /// Whether the dashboard accepts signup from arbitrary emails. The
    /// allowlist and superuser email are still accepted when this is false.
    #[must_use]
    pub const fn dashboard_signup_open(&self) -> bool {
        self.dashboard_signup_open
    }

    /// Whether unauthenticated ephemeral app creation is enabled.
    #[must_use]
    pub const fn ephemeral_apps_enabled(&self) -> bool {
        self.ephemeral_apps_enabled
    }

    /// Optional override for the nightly per-app backup worker. `None` keeps
    /// the profile default, which enables it only in production.
    #[must_use]
    pub const fn nightly_app_backups(&self) -> Option<bool> {
        self.nightly_app_backups
    }

    #[must_use]
    pub const fn instant_on_instant_app_id(&self) -> Option<Uuid> {
        self.instant_on_instant_app_id
    }

    /// Sentry-protocol DSN. `None` (unset or blank) disables error reporting.
    #[must_use]
    pub fn sentry_dsn(&self) -> Option<&str> {
        self.sentry_dsn.as_deref()
    }

    /// Environment label attached to reported errors.
    #[must_use]
    pub fn sentry_environment(&self) -> &str {
        &self.sentry_environment
    }

    #[must_use]
    pub fn google_oauth_client(&self) -> Option<(&str, &str)> {
        Some((
            self.google_oauth_client_id.as_deref()?,
            self.google_oauth_client_secret.as_deref()?,
        ))
    }

    #[must_use]
    pub fn shared_oauth_clients(&self) -> &[SharedOAuthClientConfig] {
        &self.shared_oauth_clients
    }
}

fn parse_shared_oauth_clients(
    value: Option<&str>,
) -> Result<Vec<SharedOAuthClientConfig>, ServerConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let raw: Vec<RawSharedOAuthClientConfig> =
        serde_json::from_str(value).map_err(|_| ServerConfigError::InvalidSharedOAuthClients)?;
    let mut providers = HashSet::with_capacity(raw.len());
    raw.into_iter()
        .map(|client| {
            let provider_name = client.provider_name.trim().to_owned();
            let client_id = client.client_id.trim().to_owned();
            let client_secret = client.client_secret.trim().to_owned();
            if provider_name.is_empty()
                || client_id.is_empty()
                || client_secret.is_empty()
                || !providers.insert(provider_name.clone())
            {
                return Err(ServerConfigError::InvalidSharedOAuthClients);
            }
            Ok(SharedOAuthClientConfig {
                provider_name,
                client_id,
                client_secret,
            })
        })
        .collect()
}

fn is_true(value: Option<&str>) -> bool {
    value == Some("true")
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn parse_integer<T>(name: &'static str, value: &str) -> Result<T, ServerConfigError>
where
    T: FromStr,
{
    value
        .parse()
        .map_err(|_| ServerConfigError::InvalidInteger(name))
}

fn normalize_database_url(value: &str) -> Result<String, ServerConfigError> {
    let value = value.strip_prefix("jdbc:").unwrap_or(value);
    if !(value.starts_with("postgres://") || value.starts_with("postgresql://"))
        || tokio_postgres::Config::from_str(value).is_err()
    {
        return Err(ServerConfigError::InvalidDatabaseUrl);
    }
    Ok(value.to_owned())
}

fn validated_http_url(name: &'static str, value: &str) -> Result<String, ServerConfigError> {
    let parsed = Url::parse(value).map_err(|_| ServerConfigError::InvalidHttpUrl(name))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ServerConfigError::InvalidHttpUrl(name));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn optional_http_url(
    name: &'static str,
    value: Option<String>,
) -> Result<Option<String>, ServerConfigError> {
    value
        .map(|value| validated_http_url(name, &value))
        .transpose()
}

const fn default_port(environment: ServerEnvironment) -> u16 {
    if matches!(environment, ServerEnvironment::Test) {
        8886
    } else {
        8888
    }
}

fn default_server_origin(port: u16) -> String {
    format!("http://localhost:{port}")
}

const fn default_dashboard_origin() -> &'static str {
    "http://localhost:3000"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_requires_credentials_and_defaults_all_transactional_senders() {
        assert!(matches!(
            ServerConfig::from_pairs([("INSTANT_EMAIL_PROVIDER", "cloudflare")]),
            Err(ServerConfigError::IncompleteCloudflareEmail)
        ));
        let config = ServerConfig::from_pairs([
            ("INSTANT_EMAIL_PROVIDER", "cloudflare"),
            ("CLOUDFLARE_ACCOUNT_ID", "account"),
            ("CLOUDFLARE_EMAIL_API_TOKEN", "token"),
            (
                "INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL",
                "transactional@example.com",
            ),
        ])
        .unwrap();
        assert_eq!(config.email().provider(), EmailProvider::Cloudflare);
        assert_eq!(
            config.email().app_sender().1,
            Some("transactional@example.com")
        );
        assert_eq!(
            config.email().dashboard_sender().1,
            Some("transactional@example.com")
        );
        assert_eq!(
            config.email().team_sender().1,
            Some("transactional@example.com")
        );
    }

    #[test]
    fn validates_bounded_statistics_persistence_policy() {
        let config = ServerConfig::from_pairs([
            ("INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS", "1024"),
            ("INSTANT_SKETCH_BATCH_MAX_WAL_BYTES", "16777216"),
            ("INSTANT_SKETCH_BATCH_MAX_WAIT_MS", "1000"),
        ])
        .unwrap();
        let policy = config.sketch_batch_policy();
        assert_eq!(policy.max_transactions, 1024);
        assert_eq!(policy.max_wal_bytes, 16 * 1024 * 1024);
        assert_eq!(policy.max_wait, std::time::Duration::from_secs(1));
        for (key, value) in [
            ("INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS", "0"),
            ("INSTANT_SKETCH_BATCH_MAX_TRANSACTIONS", "65537"),
            ("INSTANT_SKETCH_BATCH_MAX_WAL_BYTES", "67108865"),
            ("INSTANT_SKETCH_BATCH_MAX_WAIT_MS", "0"),
            ("INSTANT_SKETCH_BATCH_MAX_WAIT_MS", "10001"),
        ] {
            assert!(matches!(
                ServerConfig::from_pairs([(key, value)]),
                Err(ServerConfigError::InvalidSketchBatchPolicy)
            ));
        }
        assert_eq!(
            ServerConfig::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .sketch_batch_policy(),
            crate::SketchBatchPolicy::default()
        );
    }

    #[test]
    fn defaults_match_the_jvm_development_contract() {
        let config = ServerConfig::from_pairs(Vec::<(String, String)>::new()).unwrap();

        assert_eq!(config.environment(), ServerEnvironment::Development);
        assert_eq!(config.database_url(), "postgresql://localhost:5432/instant");
        assert_eq!(config.database_cluster_id(), None);
        assert_eq!(config.next_database_url(), None);
        assert_eq!(config.next_database_cluster_id(), None);
        assert_eq!(config.bind_address(), "0.0.0.0:8888".parse().unwrap());
        assert_eq!(config.server_origin(), "http://localhost:8888");
        assert_eq!(config.dashboard_origin(), "http://localhost:3000");
        assert!(!config.secure_cookies());
        assert_eq!(config.connection_pool_size(), 20);
        assert_eq!(config.storage().region, "us-east-1");
        assert_eq!(config.storage().bucket, "");
        assert_eq!(config.backup_storage().bucket.as_deref(), None);
        assert_eq!(config.wal_history().bucket.as_deref(), None);
        assert_eq!(config.wal_history().prefix, None);
        assert_eq!(config.email().postmark_server_token(), None);
        assert_eq!(config.email().postmark_account_token(), None);
        assert_eq!(config.email().sendgrid_token(), None);
        assert_eq!(config.email().provider(), EmailProvider::Auto);
        assert!(config.shared_oauth_clients().is_empty());
        assert_eq!(config.superuser_email(), None);
        assert_eq!(config.email().app_sender(), (None, None));
        assert_eq!(config.email().dashboard_sender(), ("Instant", None));
        assert_eq!(config.email().team_sender(), ("Instant", None));
    }

    #[test]
    fn superuser_email_is_normalized_for_operator_authorization() {
        let config =
            ServerConfig::from_pairs([("INSTANT_SUPERUSER_EMAIL", " Root@Example.COM ")]).unwrap();
        assert_eq!(config.superuser_email(), Some("root@example.com"));
    }

    #[test]
    fn dashboard_allowlist_is_optional_normalized_and_fail_closed() {
        assert!(
            ServerConfig::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .dashboard_allowed_emails()
                .is_none()
        );
        for value in ["", "  "] {
            assert!(
                ServerConfig::from_pairs([("INSTANT_DASHBOARD_ALLOWED_EMAILS", value)])
                    .unwrap()
                    .dashboard_allowed_emails()
                    .is_none()
            );
        }
        let config = ServerConfig::from_pairs([(
            "INSTANT_DASHBOARD_ALLOWED_EMAILS",
            " Owner@Example.COM ,second@example.com,owner@example.com",
        )])
        .unwrap();
        assert_eq!(
            config.dashboard_allowed_emails(),
            Some(&HashSet::from([
                "owner@example.com".to_owned(),
                "second@example.com".to_owned(),
            ]))
        );
        for value in [
            "owner@example.com,",
            "owner",
            "@example.com",
            "owner@",
            "owner@bad",
            "owner@ex ample.com",
        ] {
            assert_eq!(
                ServerConfig::from_pairs([("INSTANT_DASHBOARD_ALLOWED_EMAILS", value)])
                    .err()
                    .unwrap(),
                ServerConfigError::InvalidDashboardAllowedEmails
            );
        }
    }

    #[test]
    fn dashboard_signup_defaults_to_closed_and_parses_open_or_closed() {
        let default = ServerConfig::from_pairs([] as [(&str, &str); 0]).unwrap();
        assert!(!default.dashboard_signup_open());

        let open = ServerConfig::from_pairs([("INSTANT_DASHBOARD_SIGNUP", " OPEN ")]).unwrap();
        assert!(open.dashboard_signup_open());

        let closed = ServerConfig::from_pairs([("INSTANT_DASHBOARD_SIGNUP", "closed")]).unwrap();
        assert!(!closed.dashboard_signup_open());

        for value in ["", " ", "restricted"] {
            let result = ServerConfig::from_pairs([("INSTANT_DASHBOARD_SIGNUP", value)]);
            if value.trim().is_empty() {
                assert!(!result.unwrap().dashboard_signup_open());
            } else {
                assert_eq!(
                    result.err().unwrap(),
                    ServerConfigError::InvalidDashboardSignup
                );
            }
        }
    }

    #[test]
    fn ephemeral_apps_default_to_disabled_and_parse_the_boolean_flag() {
        assert!(
            !ServerConfig::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .ephemeral_apps_enabled()
        );
        assert!(
            ServerConfig::from_pairs([("INSTANT_EPHEMERAL_APPS", "true")])
                .unwrap()
                .ephemeral_apps_enabled()
        );
        for value in ["false", "", "TRUE", "1", "yes"] {
            assert!(
                !ServerConfig::from_pairs([("INSTANT_EPHEMERAL_APPS", value)])
                    .unwrap()
                    .ephemeral_apps_enabled()
            );
        }
    }

    #[test]
    fn sentry_configuration_is_optional_and_blank_counts_as_off() {
        for pairs in [
            Vec::<(&str, &str)>::new(),
            vec![("SENTRY_DSN", "")],
            vec![("SENTRY_DSN", "   ")],
        ] {
            let config = ServerConfig::from_pairs(pairs).unwrap();
            assert_eq!(config.sentry_dsn(), None);
            assert_eq!(config.sentry_environment(), "production");
        }
        let config = ServerConfig::from_pairs([
            ("SENTRY_DSN", "https://key@errors.example.test/1"),
            ("SENTRY_ENVIRONMENT", "staging"),
        ])
        .unwrap();
        assert_eq!(
            config.sentry_dsn(),
            Some("https://key@errors.example.test/1")
        );
        assert_eq!(config.sentry_environment(), "staging");
    }

    #[test]
    fn nightly_app_backups_override_the_profile_default() {
        assert_eq!(
            ServerConfig::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .nightly_app_backups(),
            None
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_NIGHTLY_APP_BACKUPS", "true")])
                .unwrap()
                .nightly_app_backups(),
            Some(true)
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_NIGHTLY_APP_BACKUPS", "false")])
                .unwrap()
                .nightly_app_backups(),
            Some(false)
        );
        for value in ["", "  "] {
            assert_eq!(
                ServerConfig::from_pairs([("INSTANT_NIGHTLY_APP_BACKUPS", value)])
                    .unwrap()
                    .nightly_app_backups(),
                None
            );
        }
        for value in ["TRUE", "1", "yes"] {
            assert!(matches!(
                ServerConfig::from_pairs([("INSTANT_NIGHTLY_APP_BACKUPS", value)]),
                Err(ServerConfigError::InvalidNightlyAppBackups)
            ));
        }
    }

    #[test]
    fn staging_precedes_production_and_jdbc_urls_are_normalized() {
        let config = ServerConfig::from_pairs([
            ("PRODUCTION", "true"),
            ("STAGING", "true"),
            ("DATABASE_URL", "jdbc:postgresql://db.internal/instant"),
            (
                "NEXT_DATABASE_URL",
                "jdbc:postgresql://next-db.internal/instant",
            ),
            ("CONNECTION_POOL_SIZE", "37"),
            ("S3_ENDPOINT", "https://objects.example.test/"),
            ("INSTANT_S3_REGION", "auto"),
            ("S3_BUCKET", "staging-files"),
            ("INSTANT_BACKEND_URL", "https://api.example.test/"),
            ("INSTANT_DASHBOARD_URL", "https://dash.example.test/"),
            ("HOSTNAME", "developer-host"),
        ])
        .unwrap();

        assert_eq!(config.environment(), ServerEnvironment::Staging);
        assert_eq!(config.database_url(), "postgresql://db.internal/instant");
        assert_eq!(config.database_cluster_id(), None);
        assert_eq!(
            config.next_database_url(),
            Some("postgresql://next-db.internal/instant")
        );
        assert_eq!(config.next_database_cluster_id(), None);
        assert_eq!(config.bind_address().port(), 8888);
        assert_eq!(config.server_origin(), "https://api.example.test");
        assert_eq!(config.dashboard_origin(), "https://dash.example.test");
        assert!(config.secure_cookies());
        assert_eq!(config.connection_pool_size(), 37);
        assert_eq!(config.wal_history().bucket.as_deref(), None);
        assert_eq!(config.wal_history().prefix, None);
        assert_eq!(
            config.storage().public_endpoint.as_deref(),
            Some("https://objects.example.test")
        );
    }

    #[test]
    fn upstream_2872_storage_addressing_style_is_explicit_and_validated() {
        for (value, expected) in [("true", true), ("false", false)] {
            let config = ServerConfig::from_pairs([("S3_VIRTUAL_HOST_STYLE", value)]).unwrap();
            assert_eq!(config.storage().virtual_host_style, expected);
        }
        let config = ServerConfig::from_pairs(Vec::<(String, String)>::new()).unwrap();
        assert!(!config.storage().virtual_host_style);
        assert!(matches!(
            ServerConfig::from_pairs([("S3_VIRTUAL_HOST_STYLE", "typo")]),
            Err(ServerConfigError::InvalidStorageAddressingStyle)
        ));
    }

    #[test]
    fn storage_upload_cap_and_download_ttl_are_bounded() {
        let config = ServerConfig::from_pairs(Vec::<(String, String)>::new()).unwrap();
        assert_eq!(
            config.storage().max_upload_bytes,
            DEFAULT_STORAGE_MAX_UPLOAD_BYTES
        );
        assert_eq!(
            config.storage().download_url_ttl_seconds,
            DEFAULT_STORAGE_DOWNLOAD_URL_TTL_SECONDS
        );

        let config =
            ServerConfig::from_pairs([("INSTANT_STORAGE_MAX_UPLOAD_BYTES", " 4096 ")]).unwrap();
        assert_eq!(config.storage().max_upload_bytes, 4_096);

        for (value, expected) in [
            ("30", MIN_STORAGE_DOWNLOAD_URL_TTL_SECONDS),
            ("600", 600),
            ("999999", MAX_STORAGE_DOWNLOAD_URL_TTL_SECONDS),
        ] {
            let config =
                ServerConfig::from_pairs([("INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS", value)])
                    .unwrap();
            assert_eq!(config.storage().download_url_ttl_seconds, expected);
        }

        let blank = ServerConfig::from_pairs([
            ("INSTANT_STORAGE_MAX_UPLOAD_BYTES", " "),
            ("INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS", ""),
        ])
        .unwrap();
        assert_eq!(
            blank.storage().max_upload_bytes,
            DEFAULT_STORAGE_MAX_UPLOAD_BYTES
        );
        assert_eq!(
            blank.storage().download_url_ttl_seconds,
            DEFAULT_STORAGE_DOWNLOAD_URL_TTL_SECONDS
        );

        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_STORAGE_MAX_UPLOAD_BYTES", "0")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidStorageMaxUploadBytes
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_STORAGE_MAX_UPLOAD_BYTES", "many")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidInteger("INSTANT_STORAGE_MAX_UPLOAD_BYTES")
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS", "soon")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidInteger("INSTANT_STORAGE_DOWNLOAD_URL_TTL_SECONDS")
        );
    }

    #[test]
    fn self_hosted_build_rejects_aurora_cluster_identifiers_at_config_load() {
        assert_eq!(
            ServerConfig::from_pairs([("DATABASE_CLUSTER_ID", "instant-production")])
                .err()
                .unwrap(),
            ServerConfigError::AwsHostedFeatureRequired("DATABASE_CLUSTER_ID")
        );
        assert_eq!(
            ServerConfig::from_pairs([("NEXT_DATABASE_CLUSTER_ID", "instant-next")])
                .err()
                .unwrap(),
            ServerConfigError::AwsHostedFeatureRequired("NEXT_DATABASE_CLUSTER_ID")
        );
    }

    #[test]
    fn wal_history_supports_development_prefix_override_and_pg_disablement() {
        let development = ServerConfig::from_pairs([
            ("HOSTNAME", "dev-worker-3"),
            ("WAL_HISTORY_BUCKET_NAME", "custom-history"),
        ])
        .unwrap();
        assert_eq!(
            development.wal_history(),
            &ServerWalHistoryConfig {
                bucket: Some("custom-history".to_owned()),
                prefix: Some("dev-worker-3".to_owned()),
            }
        );

        let disabled = ServerConfig::from_pairs([
            ("PRODUCTION", "true"),
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_DASHBOARD_URL", "https://dash.example.test"),
            ("WAL_HISTORY_STORAGE", "pg"),
            ("WAL_HISTORY_BUCKET_NAME", "ignored-history"),
        ])
        .unwrap();
        assert_eq!(disabled.wal_history().bucket, None);
        assert_eq!(disabled.wal_history().prefix, None);

        let test = ServerConfig::from_pairs([("TEST", "true")]).unwrap();
        assert_eq!(test.wal_history().bucket, None);
        assert_eq!(test.backup_storage().bucket, None);
    }

    #[test]
    fn wal_history_bucket_has_no_upstream_default() {
        let development = ServerConfig::from_pairs([] as [(&str, &str); 0]).unwrap();
        assert_eq!(development.wal_history().bucket, None);
    }

    #[test]
    fn hosted_profiles_require_explicit_public_urls() {
        assert_eq!(
            ServerConfig::from_pairs([("PRODUCTION", "true")])
                .err()
                .unwrap(),
            ServerConfigError::MissingBackendUrl
        );
        assert_eq!(
            ServerConfig::from_pairs([
                ("PRODUCTION", "true"),
                ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ])
            .err()
            .unwrap(),
            ServerConfigError::MissingDashboardUrl
        );
        assert_eq!(
            ServerConfig::from_pairs([("STAGING", "true")])
                .err()
                .unwrap(),
            ServerConfigError::MissingBackendUrl
        );
    }

    #[test]
    fn configured_storage_requires_an_explicit_bucket() {
        assert_eq!(
            ServerConfig::from_pairs([
                ("S3_ENDPOINT", "https://objects.example.test"),
                ("INSTANT_S3_REGION", "auto"),
            ])
            .err()
            .unwrap(),
            ServerConfigError::MissingStorageBucket
        );
    }

    #[test]
    fn backup_storage_uses_its_jvm_compatible_bucket_override() {
        let config = ServerConfig::from_pairs([
            ("PRODUCTION", "true"),
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_DASHBOARD_URL", "https://dash.example.test"),
            ("S3_BUCKET", "ordinary-files"),
            ("S3_APP_BACKUPS_BUCKET", "isolated-backups"),
        ])
        .unwrap();

        assert_eq!(config.storage().bucket, "ordinary-files");
        assert_eq!(
            config.backup_storage().bucket.as_deref(),
            Some("isolated-backups")
        );
    }

    #[test]
    fn explicit_port_origin_storage_and_google_pair_are_validated() {
        let config = ServerConfig::from_pairs([
            ("PORT", "9123"),
            ("INSTANT_BACKEND_URL", "https://api.example.test/"),
            ("INSTANT_DASHBOARD_URL", "https://dash.example.test/"),
            ("S3_PUBLIC_ENDPOINT", "https://files.example.test/"),
            ("INSTANT_S3_REGION", "ca-central-1"),
            ("S3_BUCKET", "instant-files"),
            ("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID", "client"),
            ("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET", "secret"),
            ("POSTMARK_TOKEN", "postmark-secret"),
            ("POSTMARK_ACCOUNT_TOKEN", "postmark-account-secret"),
            ("SENDGRID_TOKEN", "sendgrid-secret"),
            ("INSTANT_EMAIL_PROVIDER", " SeNdGrId "),
            ("INSTANT_APP_EMAIL_SENDER_NAME", "Example App"),
            ("INSTANT_APP_EMAIL_SENDER_EMAIL", "app@example.test"),
            ("INSTANT_DASHBOARD_EMAIL_SENDER_NAME", "Example"),
            (
                "INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL",
                "dashboard@example.test",
            ),
            ("INSTANT_TEAM_EMAIL_SENDER_NAME", "Example Team"),
            ("INSTANT_TEAM_EMAIL_SENDER_EMAIL", "teams@example.test"),
            (
                "INSTANT_SHARED_OAUTH_CLIENTS",
                r#"[{"provider_name":"google","client_id":"shared-id","client_secret":"shared-secret"}]"#,
            ),
        ])
        .unwrap();

        assert_eq!(config.bind_address().port(), 9123);
        assert_eq!(config.server_origin(), "https://api.example.test");
        assert_eq!(config.dashboard_origin(), "https://dash.example.test");
        assert_eq!(config.storage().region, "ca-central-1");
        assert_eq!(config.google_oauth_client(), Some(("client", "secret")));
        assert_eq!(
            config.email().postmark_server_token(),
            Some("postmark-secret")
        );
        assert_eq!(
            config.email().postmark_account_token(),
            Some("postmark-account-secret")
        );
        assert_eq!(config.email().sendgrid_token(), Some("sendgrid-secret"));
        assert_eq!(config.email().provider(), EmailProvider::SendGrid);
        assert_eq!(config.shared_oauth_clients().len(), 1);
        assert_eq!(config.shared_oauth_clients()[0].provider_name(), "google");
        assert_eq!(config.shared_oauth_clients()[0].client_id(), "shared-id");
        assert_eq!(
            config.shared_oauth_clients()[0].client_secret(),
            "shared-secret"
        );
        assert_eq!(
            config.email().app_sender(),
            (Some("Example App"), Some("app@example.test"))
        );
        assert_eq!(
            config.email().dashboard_sender(),
            ("Example", Some("dashboard@example.test"))
        );
        assert_eq!(
            config.email().team_sender(),
            ("Example Team", Some("teams@example.test"))
        );
    }

    #[test]
    fn secure_cookies_can_be_enabled_without_hosted_configuration() {
        let config = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_SECURE_COOKIES", "true"),
        ])
        .unwrap();

        assert_eq!(config.environment(), ServerEnvironment::Development);
        assert!(config.secure_cookies());
        assert!(!config.environment().is_hosted());
        assert_eq!(config.database_cluster_id(), None);

        let localhost = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "http://localhost:8888"),
            ("INSTANT_SECURE_COOKIES", "true"),
        ])
        .unwrap();
        assert!(!localhost.secure_cookies());

        let hosted_http = ServerConfig::from_pairs([
            ("PRODUCTION", "true"),
            ("INSTANT_BACKEND_URL", "http://localhost:8888"),
            ("INSTANT_DASHBOARD_URL", "http://localhost:3000"),
        ])
        .unwrap();
        assert!(!hosted_http.secure_cookies());
    }

    #[test]
    fn trusted_proxy_cidrs_are_explicit_and_validated() {
        let config = ServerConfig::from_pairs([(
            "INSTANT_TRUSTED_PROXY_CIDRS",
            "10.0.0.0/8, 172.16.0.0/12,fc00::/7",
        )])
        .unwrap();

        assert_eq!(config.trusted_proxy_cidrs().len(), 3);
        assert!(config.trusted_proxy_cidrs()[0].contains(&"10.2.3.4".parse::<IpAddr>().unwrap()));
        assert!(config.trusted_proxy_cidrs()[2].contains(&"fd00::1".parse::<IpAddr>().unwrap()));
        assert!(
            ServerConfig::from_pairs([("INSTANT_TRUSTED_PROXY_CIDRS", "10.0.0.1/99")]).is_err()
        );
        assert!(
            ServerConfig::from_pairs(std::iter::empty::<(&str, &str)>())
                .unwrap()
                .trusted_proxy_cidrs()
                .is_empty()
        );
    }

    #[test]
    fn storage_region_uses_only_the_provider_neutral_name() {
        let aws_only = ServerConfig::from_pairs([("AWS_REGION", "eu-west-1")]).unwrap();
        assert_eq!(aws_only.storage().region, "us-east-1");

        let preferred = ServerConfig::from_pairs([
            ("AWS_REGION", "eu-west-1"),
            ("INSTANT_S3_REGION", "ca-central-1"),
        ])
        .unwrap();
        assert_eq!(preferred.storage().region, "ca-central-1");
    }

    #[test]
    fn configured_storage_requires_an_explicit_provider_neutral_region() {
        assert_eq!(
            ServerConfig::from_pairs([("S3_ENDPOINT", "https://objects.example.test")])
                .err()
                .unwrap(),
            ServerConfigError::MissingStorageRegion
        );
    }

    #[test]
    fn malformed_or_partial_configuration_fails_closed() {
        assert_eq!(
            ServerConfig::from_pairs([("PORT", "70000")]).err().unwrap(),
            ServerConfigError::InvalidInteger("PORT")
        );
        assert_eq!(
            ServerConfig::from_pairs([("DATABASE_URL", "mysql://localhost/instant")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidDatabaseUrl
        );
        assert_eq!(
            ServerConfig::from_pairs([("NEXT_DATABASE_URL", "mysql://localhost/instant")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidDatabaseUrl
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_BACKEND_URL", "file:///tmp/instant")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidHttpUrl("INSTANT_BACKEND_URL")
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID", "client",)])
                .err()
                .unwrap(),
            ServerConfigError::IncompleteGoogleOAuthClient
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_EMAIL_PROVIDER", "smtp")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidEmailProvider
        );
        assert_eq!(
            ServerConfig::from_pairs([("INSTANT_ON_INSTANT_APP_ID", "not-a-uuid")])
                .err()
                .unwrap(),
            ServerConfigError::InvalidInstantOnInstantAppId
        );
        assert_eq!(
            ServerConfig::from_pairs([(
                "INSTANT_SHARED_OAUTH_CLIENTS",
                r#"[{"provider_name":"google","client_id":"","client_secret":"secret"}]"#,
            )])
            .err()
            .unwrap(),
            ServerConfigError::InvalidSharedOAuthClients
        );
    }
}
