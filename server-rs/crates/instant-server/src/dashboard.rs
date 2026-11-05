use std::future::{Future, ready};
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use chrono::{NaiveDateTime, Utc};
use instant_auth::{generate_magic_code, generate_personal_access_token};
use instant_http::{
    ApiError, ApiErrorType, DashboardBackend, DashboardRequest, DashboardResponse,
    resolve_client_ip,
};
use ipnet::IpNet;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, GenericClient};
use uuid::Uuid;

use crate::PostgresPool;
use crate::dashboard_refresh_token::{
    INSERT_SQL as INSERT_REFRESH_TOKEN_SQL, LOAD_SESSION_SQL, hash_token,
};
use crate::db_error::{database_error, internal_error};
use crate::postgres_pool::PostgresClientSource;

const CLI_MIN_VERSION: &str = "0.19.0";
static DASHBOARD_EMAIL_PATTERN: OnceLock<regex::Regex> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct DashboardUser {
    pub id: Uuid,
    pub email: String,
    pub created_at: NaiveDateTime,
}

pub trait DashboardMagicCodeDelivery: Send + Sync + 'static {
    fn deliver(
        &self,
        email: String,
        code: String,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub trait DashboardIdentityPolicy: Send + Sync + 'static {
    /// Enforces the dashboard send-code rate policy.
    ///
    /// # Errors
    ///
    /// Returns a rate-limit or deployment-policy error.
    fn check_send(&self, email: &str) -> Result<(), ApiError>;

    /// Enforces the dashboard verify-code rate policy.
    ///
    /// # Errors
    ///
    /// Returns a rate-limit or deployment-policy error.
    fn check_verify(&self, email: &str) -> Result<(), ApiError>;

    /// Records one failed verification and reports whether the email reached
    /// its lockout threshold.
    fn record_verify_failure(&self, _email: &str) -> bool {
        false
    }

    /// Clears the failed-verification counter after a successful sign-in or a
    /// newly issued code.
    fn reset_verify_failures(&self, _email: &str) {}

    /// Determines whether a new dashboard account may be created.
    ///
    /// # Errors
    ///
    /// Returns the configured signup-closed or allowlist error.
    fn allow_signup(&self, email: &str) -> Result<(), ApiError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowDashboardIdentity;

impl DashboardIdentityPolicy for AllowDashboardIdentity {
    fn check_send(&self, _email: &str) -> Result<(), ApiError> {
        Ok(())
    }

    fn check_verify(&self, _email: &str) -> Result<(), ApiError> {
        Ok(())
    }

    fn allow_signup(&self, _email: &str) -> Result<(), ApiError> {
        Ok(())
    }
}

/// Loads the compound `/dash` response (apps, orgs, profile, invites and flags).
pub trait DashboardSummaryProvider: Send + Sync + 'static {
    fn summary(&self, user: DashboardUser) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

/// A dashboard subsystem can claim requests before the remaining control-plane backend.
pub trait DashboardSubsystem: Send + Sync + 'static {
    fn matches(&self, request: &DashboardRequest) -> bool;

    fn authorize_restore_upload(
        &self,
        _request: DashboardRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        ready(Err(ApiError::new(
            ApiErrorType::PermissionDenied,
            "Restore upload authorization is unavailable",
        )
        .unauthorized()))
    }

    fn handle(
        &self,
        request: DashboardRequest,
    ) -> impl Future<Output = Result<DashboardResponse, ApiError>> + Send;
}

/// Composes a live subsystem with the not-yet-specialized dashboard backend.
pub struct DashboardService<I, F> {
    identity: I,
    fallback: F,
}

impl<I, F> DashboardService<I, F> {
    #[must_use]
    pub const fn new(identity: I, fallback: F) -> Self {
        Self { identity, fallback }
    }
}

impl<I, F> DashboardBackend for DashboardService<I, F>
where
    I: DashboardSubsystem,
    F: DashboardBackend,
{
    async fn authorize_restore_upload(&self, request: DashboardRequest) -> Result<(), ApiError> {
        if self.identity.matches(&request) {
            self.identity.authorize_restore_upload(request).await
        } else {
            self.fallback.authorize_restore_upload(request).await
        }
    }

    async fn handle(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        if self.identity.matches(&request) {
            self.identity.handle(request).await
        } else {
            self.fallback.handle(request).await
        }
    }
}

/// Resolves dashboard client addresses from a forwarded chain only when the
/// socket peer belongs to an explicitly trusted proxy network.
pub struct TrustedProxyDashboard<D> {
    inner: D,
    trusted_proxies: Arc<[IpNet]>,
}

impl<D> TrustedProxyDashboard<D> {
    #[must_use]
    pub fn new(inner: D, trusted_proxies: &[IpNet]) -> Self {
        Self {
            inner,
            trusted_proxies: Arc::from(trusted_proxies),
        }
    }
}

impl<D> DashboardBackend for TrustedProxyDashboard<D>
where
    D: DashboardBackend,
{
    async fn authorize_restore_upload(
        &self,
        mut request: DashboardRequest,
    ) -> Result<(), ApiError> {
        request.remote_addr = trusted_client_ip(&request, &self.trusted_proxies);
        request.headers.remove("x-forwarded-for");
        request.headers.remove("x-real-ip");
        self.inner.authorize_restore_upload(request).await
    }

    async fn handle(&self, mut request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        request.remote_addr = trusted_client_ip(&request, &self.trusted_proxies);
        request.headers.remove("x-forwarded-for");
        request.headers.remove("x-real-ip");
        self.inner.handle(request).await
    }
}

fn trusted_client_ip(request: &DashboardRequest, trusted_proxies: &[IpNet]) -> Option<String> {
    let peer = request.remote_addr.as_deref()?.parse::<IpAddr>().ok()?;
    resolve_client_ip(Some(peer), &request.headers, trusted_proxies).map(|ip| ip.to_string())
}

/// Fail-closed terminal backend for dashboard requests that do not match any
/// production subsystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct RejectUnmatchedDashboard;

impl DashboardBackend for RejectUnmatchedDashboard {
    fn handle(
        &self,
        request: DashboardRequest,
    ) -> impl Future<Output = Result<DashboardResponse, ApiError>> + Send {
        ready(Err(ApiError::new(
            ApiErrorType::Unknown,
            format!(
                "No dashboard subsystem handles {} {}",
                request.method, request.path
            ),
        )))
    }
}

/// PostgreSQL-backed dashboard login, CLI login, profile and personal-token subsystem.
pub struct DashboardIdentityService<D, P, S> {
    client: PostgresClientSource,
    delivery: D,
    policy: P,
    summaries: S,
}

impl<D, P, S> DashboardIdentityService<D, P, S> {
    #[must_use]
    pub fn new(client: Client, delivery: D, policy: P, summaries: S) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            delivery,
            policy,
            summaries,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, delivery: D, policy: P, summaries: S) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            delivery,
            policy,
            summaries,
        }
    }
}

impl<D, P, S> DashboardSubsystem for DashboardIdentityService<D, P, S>
where
    D: DashboardMagicCodeDelivery,
    P: DashboardIdentityPolicy,
    S: DashboardSummaryProvider,
{
    fn matches(&self, request: &DashboardRequest) -> bool {
        is_identity_route(request.method.as_str(), &request.path)
    }

    async fn handle(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/dash/auth/send_magic_code") => self.send_magic_code(request).await,
            ("POST", "/dash/auth/verify_magic_code") => self.verify_magic_code(request).await,
            ("GET", "/dash") => self.dashboard(request).await,
            ("GET", "/dash/me") => self.me(request).await,
            ("POST", "/dash/profiles") => self.put_profile(request).await,
            ("POST", "/dash/cli/auth/register") => self.cli_register().await,
            ("POST", "/dash/cli/auth/claim") => self.cli_claim(request).await,
            ("POST", "/dash/cli/auth/void") => self.cli_void(request).await,
            ("POST", "/dash/cli/auth/check") => self.cli_check(request).await,
            ("GET", "/dash/cli/version") => json_response(json!({"min-version": CLI_MIN_VERSION})),
            ("GET", "/dash/personal_access_tokens") => self.list_personal_tokens(request).await,
            ("POST", "/dash/personal_access_tokens") => self.create_personal_token(request).await,
            ("DELETE", path) if path.starts_with("/dash/personal_access_tokens/") => {
                self.delete_personal_token(request).await
            }
            ("POST", "/dash/signout") => self.signout(request).await,
            _ => Err(ApiError::new(
                ApiErrorType::Unknown,
                "Dashboard identity route classifier mismatch",
            )),
        }
    }
}

impl<D, P, S> DashboardIdentityService<D, P, S>
where
    D: DashboardMagicCodeDelivery,
    P: DashboardIdentityPolicy,
    S: DashboardSummaryProvider,
{
    async fn send_magic_code(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let email = email_field(&request.body, "email")?;
        self.policy.check_send(&email)?;
        let code = generate_magic_code();
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let user_id = transaction
            .query_opt("SELECT id FROM instant_users WHERE email = $1", &[&email])
            .await
            .map_err(database_error)?
            .map(|row| row.get(0));
        let user_id = if let Some(user_id) = user_id {
            user_id
        } else {
            self.policy.allow_signup(&email)?;
            let user_id = Uuid::new_v4();
            transaction
                .execute(
                    "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
                    &[&user_id, &email],
                )
                .await
                .map_err(database_error)?;
            user_id
        };
        transaction
            .execute(
                "DELETE FROM instant_user_magic_codes WHERE user_id = $1",
                &[&user_id],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO instant_user_magic_codes (id, code, user_id) VALUES ($1, $2, $3)",
                &[&Uuid::new_v4(), &code, &user_id],
            )
            .await
            .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        drop(client);
        self.policy.reset_verify_failures(&email);
        self.delivery.deliver(email, code).await?;
        json_response(json!({"sent": true}))
    }

    async fn verify_magic_code(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let body = json_body(&request.body)?;
        let email = normalized_email(required_string(&body, "email")?)?;
        let code = required_string(&body, "code")?.trim().to_owned();
        self.policy.check_verify(&email)?;
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let row = transaction
            .query_opt(
                "DELETE FROM instant_user_magic_codes codes
                 USING instant_users users
                 WHERE codes.user_id = users.id AND codes.code = $1 AND users.email = $2
                 RETURNING codes.user_id, codes.created_at",
                &[&code, &email],
            )
            .await
            .map_err(database_error)?;
        let Some(row) = row else {
            if self.policy.record_verify_failure(&email) {
                transaction
                    .execute(
                        "DELETE FROM instant_user_magic_codes codes
                         USING instant_users users
                         WHERE codes.user_id = users.id AND users.email = $1",
                        &[&email],
                    )
                    .await
                    .map_err(database_error)?;
                transaction.commit().await.map_err(database_error)?;
            }
            return Err(record_not_found("instant-user-magic-code"));
        };
        let user_id: Uuid = row.get(0);
        let created_at: NaiveDateTime = row.get(1);
        if Utc::now().naive_utc() - created_at > chrono::Duration::minutes(10) {
            transaction.commit().await.map_err(database_error)?;
            return Err(ApiError::new(
                ApiErrorType::RecordExpired,
                "Record expired: instant-user-magic-code",
            ));
        }
        let disabled = transaction
            .query_opt(
                "SELECT 1 FROM user_flags
                 WHERE user_id = $1 AND flag_name = 'dashboard-login-disabled'",
                &[&user_id],
            )
            .await
            .map_err(database_error)?
            .is_some();
        if disabled {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Dashboard login is disabled",
            ));
        }
        let token = Uuid::new_v4();
        transaction
            .execute(
                INSERT_REFRESH_TOKEN_SQL,
                &[&token, &hash_token(token), &user_id],
            )
            .await
            .map_err(database_error)?;
        let user = load_user(&transaction, user_id).await?;
        transaction.commit().await.map_err(database_error)?;
        self.policy.reset_verify_failures(&email);
        json_response(json!({"token": token, "user": user_json(&user)}))
    }

    async fn dashboard(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        json_response(self.summaries.summary(user).await?)
    }

    async fn me(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        json_response(json!({"user": user_json(&user)}))
    }

    async fn put_profile(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        let body = json_body(&request.body)?;
        let meta = body.get("meta").cloned().ok_or_else(|| missing("meta"))?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_one(
                "INSERT INTO instant_profiles (id, meta) VALUES ($1, $2)
                 ON CONFLICT (id) DO UPDATE SET meta = EXCLUDED.meta
                 RETURNING id, meta, created_at",
                &[&user.id, &tokio_postgres::types::Json(meta)],
            )
            .await
            .map_err(database_error)?;
        let meta: tokio_postgres::types::Json<Value> = row.get(1);
        let created_at: NaiveDateTime = row.get(2);
        json_response(json!({
            "profile": {"id": row.get::<_, Uuid>(0), "meta": meta.0, "created_at": created_at}
        }))
    }

    async fn cli_register(&self) -> Result<DashboardResponse, ApiError> {
        let secret = Uuid::new_v4();
        let ticket = Uuid::new_v4();
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        let client = self.client.acquire().await.map_err(database_error)?;
        client
            .execute(
                "INSERT INTO instant_cli_logins (id, secret) VALUES ($1, $2)",
                &[&ticket, &digest],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"secret": secret, "ticket": ticket}))
    }

    async fn cli_claim(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        let ticket = uuid_field(&request.body, "ticket")?;
        let client = self.client.acquire().await.map_err(database_error)?;
        client
            .execute(
                "UPDATE instant_cli_logins SET user_id = $1 WHERE id = $2",
                &[&user.id, &ticket],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({"ticket": ticket}))
    }

    async fn cli_void(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        self.authenticate(&request).await?;
        let ticket = uuid_field(&request.body, "ticket")?;
        let client = self.client.acquire().await.map_err(database_error)?;
        client
            .execute(
                "UPDATE instant_cli_logins SET used = true WHERE id = $1",
                &[&ticket],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({}))
    }

    async fn cli_check(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let secret = uuid_field(&request.body, "secret")?;
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let row = transaction
            .query_opt(
                "SELECT id, used, user_id, created_at FROM instant_cli_logins
                 WHERE secret = $1 FOR UPDATE",
                &[&digest],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("instant-cli-login"))?;
        let ticket: Uuid = row.get(0);
        let used: bool = row.get(1);
        let user_id: Option<Uuid> = row.get(2);
        let created_at: NaiveDateTime = row.get(3);
        if Utc::now().naive_utc() - created_at > chrono::Duration::minutes(2) {
            return Err(ApiError::new(
                ApiErrorType::RecordExpired,
                "Record expired: instant-cli-login",
            ));
        }
        if used && user_id.is_none() {
            return Err(validation("This request has been denied"));
        }
        let user_id =
            user_id.ok_or_else(|| validation("Waiting for a user to accept this request"))?;
        if used {
            return Err(validation("This request has already been claimed"));
        }
        let changed = transaction
            .execute(
                "UPDATE instant_cli_logins SET used = true
                 WHERE id = $1 AND used = false AND user_id IS NOT NULL",
                &[&ticket],
            )
            .await
            .map_err(database_error)?;
        if changed == 0 {
            return Err(validation("This request has already been claimed"));
        }
        let token = Uuid::new_v4();
        transaction
            .execute(
                INSERT_REFRESH_TOKEN_SQL,
                &[&token, &hash_token(token), &user_id],
            )
            .await
            .map_err(database_error)?;
        let dashboard_user = load_user(&transaction, user_id).await?;
        transaction.commit().await.map_err(database_error)?;
        drop(client);
        json_response(json!({"token": token, "email": dashboard_user.email}))
    }

    async fn list_personal_tokens(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let rows = client
            .query(
                "SELECT id, user_id, name, created_at
                 FROM instant_personal_access_tokens WHERE user_id = $1",
                &[&user.id],
            )
            .await
            .map_err(database_error)?;
        let data = rows
            .into_iter()
            .map(|row| {
                json!({
                    "id": row.get::<_, Uuid>(0),
                    "user_id": row.get::<_, Uuid>(1),
                    "name": row.get::<_, String>(2),
                    "created_at": row.get::<_, NaiveDateTime>(3),
                })
            })
            .collect::<Vec<_>>();
        json_response(json!({"data": data}))
    }

    async fn create_personal_token(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        let body = json_body(&request.body)?;
        let name = required_string(&body, "name")?.trim().to_owned();
        if name.is_empty() {
            return Err(missing("name"));
        }
        let id = Uuid::new_v4();
        let token = generate_personal_access_token();
        let digest = Sha256::digest(token.as_bytes()).to_vec();
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_one(
                "INSERT INTO instant_personal_access_tokens (id, user_id, name, lookup_key)
                 VALUES ($1, $2, $3, $4)
                 RETURNING created_at",
                &[&id, &user.id, &name, &digest],
            )
            .await
            .map_err(database_error)?;
        let created_at: NaiveDateTime = row.get(0);
        json_response(json!({
            "data": {"token": token, "id": id, "user_id": user.id, "name": name, "created_at": created_at}
        }))
    }

    async fn delete_personal_token(
        &self,
        request: DashboardRequest,
    ) -> Result<DashboardResponse, ApiError> {
        let user = self.authenticate(&request).await?;
        let id = request
            .path
            .strip_prefix("/dash/personal_access_tokens/")
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| malformed("Malformed personal access token ID"))?;
        let client = self.client.acquire().await.map_err(database_error)?;
        client
            .execute(
                "DELETE FROM instant_personal_access_tokens WHERE id = $1 AND user_id = $2",
                &[&id, &user.id],
            )
            .await
            .map_err(database_error)?;
        json_response(json!({}))
    }

    async fn signout(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let token = bearer_uuid(&request)?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let changed = client
            .execute(
                "DELETE FROM instant_user_refresh_tokens WHERE id = $1",
                &[&token],
            )
            .await
            .map_err(database_error)?;
        if changed == 0 {
            return Err(record_not_found("instant-user"));
        }
        json_response(json!({}))
    }

    async fn authenticate(&self, request: &DashboardRequest) -> Result<DashboardUser, ApiError> {
        let token = bearer_uuid(request)?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_opt(LOAD_SESSION_SQL, &[&hash_token(token)])
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("instant-user"))?;
        Ok(row_user(&row))
    }
}

#[must_use]
pub fn is_identity_route(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        (
            "POST",
            "/dash/auth/send_magic_code"
                | "/dash/auth/verify_magic_code"
                | "/dash/profiles"
                | "/dash/cli/auth/register"
                | "/dash/cli/auth/claim"
                | "/dash/cli/auth/void"
                | "/dash/cli/auth/check"
                | "/dash/signout"
        ) | ("GET", "/dash" | "/dash/me" | "/dash/cli/version")
            | ("GET" | "POST", "/dash/personal_access_tokens")
    ) || (method == "DELETE" && path.starts_with("/dash/personal_access_tokens/"))
}

async fn load_user<C>(client: &C, user_id: Uuid) -> Result<DashboardUser, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_opt(
            "SELECT id, email, created_at FROM instant_users WHERE id = $1",
            &[&user_id],
        )
        .await
        .map_err(database_error)?
        .map(|row| row_user(&row))
        .ok_or_else(|| record_not_found("instant-user"))
}

fn row_user(row: &tokio_postgres::Row) -> DashboardUser {
    DashboardUser {
        id: row.get(0),
        email: row.get(1),
        created_at: row.get(2),
    }
}

fn user_json(user: &DashboardUser) -> Value {
    json!({"id": user.id, "email": user.email, "created_at": user.created_at})
}

fn bearer_uuid(request: &DashboardRequest) -> Result<Uuid, ApiError> {
    request
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_value)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| malformed("Malformed dashboard bearer token").unauthorized())
}

fn bearer_value(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
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

fn email_field(body: &[u8], key: &'static str) -> Result<String, ApiError> {
    normalized_email(required_string(&json_body(body)?, key)?)
}

fn normalized_email(input: &str) -> Result<String, ApiError> {
    let email = input.trim().to_lowercase();
    let pattern = DASHBOARD_EMAIL_PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$",
        )
        .expect("dashboard email regex is static and valid")
    });
    if pattern.is_match(&email) {
        Ok(email)
    } else {
        Err(malformed("Malformed email"))
    }
}

fn uuid_field(body: &[u8], key: &'static str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(required_string(&json_body(body)?, key)?)
        .map_err(|_| malformed("Malformed UUID parameter"))
}

fn json_response(value: Value) -> Result<DashboardResponse, ApiError> {
    let result = serde_json::to_vec(&value);
    drop(value);
    result.map(DashboardResponse::json).map_err(internal_error)
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

fn record_not_found(record: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Record not found: {record}"),
    )
    .with_hint(json!({"record-type": record}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_classifier_is_exact_and_email_normalization_is_safe() {
        assert!(is_identity_route("POST", "/dash/auth/send_magic_code"));
        assert!(is_identity_route(
            "DELETE",
            "/dash/personal_access_tokens/00000000-0000-0000-0000-000000000001"
        ));
        assert!(!is_identity_route("GET", "/dash/apps"));
        assert!(!is_identity_route("DELETE", "/dash/personal_access_tokens"));
        assert_eq!(
            normalized_email(" Test@Example.COM ").unwrap(),
            "test@example.com"
        );
        assert!(normalized_email("not-an-email").is_err());
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let token = Uuid::new_v4();
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::header::AUTHORIZATION,
                format!("{scheme} {token}").parse().unwrap(),
            );
            let request = DashboardRequest {
                method: axum::http::Method::GET,
                path: "/dash/me".to_owned(),
                query: None,
                headers,
                body: axum::body::Bytes::new(),
                staged_body_path: None,
                remote_addr: None,
            };
            assert_eq!(bearer_uuid(&request).unwrap(), token);
        }
    }
}
