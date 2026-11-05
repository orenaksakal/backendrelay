use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::Utc;
use instant_auth::{
    AppUser, AuthError, CreateUser, create_magic_code, delete_refresh_token,
    invalidate_magic_codes, sign_in_guest_with_fields, user_by_refresh_token, verify_magic_code,
};
use instant_http::{
    ApiError, ApiErrorType, RuntimeAuthBackend, RuntimeAuthSession, RuntimeUser,
    SendMagicCodeRequest, SignInGuestRequest, SignOutRequest, VerifyMagicCodeRequest,
    VerifyRefreshTokenRequest,
};
use instant_instaql::AttributeCatalog;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::db_error::{database_error, internal_error};
use crate::postgres_pool::PostgresClientSource;

/// Outbound email seam for plaintext one-time codes.
pub trait MagicCodeDelivery: Send + Sync + 'static {
    fn deliver(
        &self,
        app_id: Uuid,
        email: &str,
        code: &str,
        expiry_minutes: u64,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub type MagicCodeExpiryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<u64, ApiError>> + Send + 'a>>;

/// Resolves the current per-app magic-code TTL, including the live config fallback.
pub trait MagicCodeExpiryPolicy: std::fmt::Debug + Send + Sync + 'static {
    fn expiry_minutes(&self, app_id: Uuid) -> MagicCodeExpiryFuture<'_>;
}

#[derive(Clone, Copy, Debug)]
pub struct StaticMagicCodeExpiry(pub u64);

/// Upper bound for an app-configured magic-code lifetime. Larger stored values
/// are clamped rather than rejected so existing apps keep working.
pub const MAX_MAGIC_CODE_EXPIRY_MINUTES: u64 = 60;

impl MagicCodeExpiryPolicy for StaticMagicCodeExpiry {
    fn expiry_minutes(&self, _app_id: Uuid) -> MagicCodeExpiryFuture<'_> {
        Box::pin(std::future::ready(Ok(self
            .0
            .min(MAX_MAGIC_CODE_EXPIRY_MINUTES))))
    }
}

pub type MagicCodeRateLimitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>>;

pub type MagicCodeVerifyFailureFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>>;

/// Separate send/consume throttles for public runtime magic codes.
pub trait MagicCodeRateLimiter: std::fmt::Debug + Send + Sync + 'static {
    fn check_send<'a>(&'a self, app_id: Uuid, email: &'a str) -> MagicCodeRateLimitFuture<'a>;

    /// Applies the public runtime route's per-client-IP quota in addition to
    /// the per-(app, email) send limit.
    fn check_send_from_ip<'a>(
        &'a self,
        app_id: Uuid,
        email: &'a str,
        _client_ip: Option<&'a str>,
    ) -> MagicCodeRateLimitFuture<'a> {
        self.check_send(app_id, email)
    }

    fn check_verify<'a>(&'a self, app_id: Uuid, email: &'a str) -> MagicCodeRateLimitFuture<'a>;

    /// Applies the public runtime route's per-client-IP quota in addition to
    /// the per-(app, email) verify limit.
    fn check_verify_from_ip<'a>(
        &'a self,
        app_id: Uuid,
        email: &'a str,
        _client_ip: Option<&'a str>,
    ) -> MagicCodeRateLimitFuture<'a> {
        self.check_verify(app_id, email)
    }

    /// Records one failed verification. The returned flag means the
    /// (app, email) pair reached its lockout threshold and its active codes
    /// must be invalidated.
    fn record_verify_failure<'a>(
        &'a self,
        _app_id: Uuid,
        _email: &'a str,
    ) -> MagicCodeVerifyFailureFuture<'a> {
        Box::pin(std::future::ready(Ok(false)))
    }

    /// Clears the failed-verification counter after a successful sign-in or a
    /// newly issued code.
    fn reset_verify_failures<'a>(
        &'a self,
        _app_id: Uuid,
        _email: &'a str,
    ) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn check_custom_sender<'a>(&'a self, email: &'a str) -> MagicCodeRateLimitFuture<'a>;
}

#[derive(Debug, Default)]
pub struct NoopMagicCodeRateLimiter;

impl MagicCodeRateLimiter for NoopMagicCodeRateLimiter {
    fn check_send<'a>(&'a self, _app_id: Uuid, _email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn check_verify<'a>(&'a self, _app_id: Uuid, _email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn check_custom_sender<'a>(&'a self, _email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// `$users` create-rule seam run before one-time-code consumption or guest writes.
pub trait SignupAuthorizer: Send + Sync + 'static {
    /// Checks whether the candidate user may be created.
    ///
    /// # Errors
    ///
    /// Returns an authorization or permission-evaluation failure.
    fn authorize(
        &self,
        app_id: Uuid,
        user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllSignups;

impl SignupAuthorizer for AllowAllSignups {
    fn authorize(
        &self,
        _app_id: Uuid,
        _user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Per-app schema and signup decisions used by public runtime authentication.
pub trait RuntimeAuthPolicy: Send + Sync + 'static {
    fn load_catalog(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send;

    fn authorize_signup(
        &self,
        app_id: Uuid,
        user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;
    fn authorize_signup_with_origin(
        &self,
        app_id: Uuid,
        user: &CreateUser,
        _origin: Option<&str>,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        self.authorize_signup(app_id, user)
    }
}

pub struct StaticRuntimeAuthPolicy<A> {
    catalog: AttributeCatalog,
    authorizer: A,
}

impl<A> StaticRuntimeAuthPolicy<A> {
    #[must_use]
    pub fn new(catalog: AttributeCatalog, authorizer: A) -> Self {
        Self {
            catalog,
            authorizer,
        }
    }
}

impl<A> RuntimeAuthPolicy for StaticRuntimeAuthPolicy<A>
where
    A: SignupAuthorizer,
{
    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(self.catalog.clone()))
    }

    fn authorize_signup(
        &self,
        app_id: Uuid,
        user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        self.authorizer.authorize(app_id, user)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAuthConfig {
    pub magic_code_expiry_minutes: u64,
    pub static_test_code: Option<String>,
}

impl Default for RuntimeAuthConfig {
    fn default() -> Self {
        Self {
            magic_code_expiry_minutes: 15,
            static_test_code: None,
        }
    }
}

/// PostgreSQL-backed implementation of the public runtime-auth HTTP contract.
///
/// Tests and embedders may own one compatibility client. Production uses a
/// dedicated bounded pool so a dropped runtime-auth connection is recreated
/// without coupling mutation leases to signup-policy callbacks.
#[derive(Debug)]
pub struct RuntimeAuthService<D, P> {
    client: PostgresClientSource,
    delivery: D,
    policy: P,
    config: RuntimeAuthConfig,
    magic_code_rate_limiter: Arc<dyn MagicCodeRateLimiter>,
    magic_code_expiry: Arc<dyn MagicCodeExpiryPolicy>,
}

impl<D, P> RuntimeAuthService<D, P> {
    #[must_use]
    pub fn new(client: Client, delivery: D, policy: P, config: RuntimeAuthConfig) -> Self {
        let magic_code_expiry = Arc::new(StaticMagicCodeExpiry(config.magic_code_expiry_minutes));
        Self {
            client: PostgresClientSource::fixed(client),
            delivery,
            policy,
            config,
            magic_code_rate_limiter: Arc::new(NoopMagicCodeRateLimiter),
            magic_code_expiry,
        }
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        delivery: D,
        policy: P,
        config: RuntimeAuthConfig,
    ) -> Self {
        let magic_code_expiry = Arc::new(StaticMagicCodeExpiry(config.magic_code_expiry_minutes));
        Self {
            client: PostgresClientSource::pooled(pool),
            delivery,
            policy,
            config,
            magic_code_rate_limiter: Arc::new(NoopMagicCodeRateLimiter),
            magic_code_expiry,
        }
    }

    #[must_use]
    pub fn with_magic_code_rate_limiter(mut self, limiter: Arc<dyn MagicCodeRateLimiter>) -> Self {
        self.magic_code_rate_limiter = limiter;
        self
    }

    #[must_use]
    pub fn with_magic_code_expiry_policy(mut self, policy: Arc<dyn MagicCodeExpiryPolicy>) -> Self {
        self.magic_code_expiry = policy;
        self
    }
}

impl<D, P> RuntimeAuthBackend for RuntimeAuthService<D, P>
where
    D: MagicCodeDelivery,
    P: RuntimeAuthPolicy,
{
    async fn send_magic_code(&self, request: SendMagicCodeRequest) -> Result<(), ApiError> {
        self.send_magic_code_with_ip(request, None).await
    }

    async fn send_magic_code_with_ip(
        &self,
        request: SendMagicCodeRequest,
        client_ip: Option<String>,
    ) -> Result<(), ApiError> {
        self.magic_code_rate_limiter
            .check_send_from_ip(request.app_id, &request.email, client_ip.as_deref())
            .await?;
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let expiry_minutes = self
            .magic_code_expiry
            .expiry_minutes(request.app_id)
            .await?;
        let mut client = self
            .client
            .acquire()
            .await
            .map_err(runtime_database_error)?;
        let code = create_magic_code(&mut client, request.app_id, &catalog, &request.email, None)
            .await
            .map_err(|error| auth_error_with_catalog(error, Some(&catalog)))?;
        let reset = self
            .magic_code_rate_limiter
            .reset_verify_failures(request.app_id, &request.email);
        // The delivery provider is network-bound; never hold a runtime-auth
        // connection across it, or sends from different apps serialize.
        drop(client);
        reset.await?;
        self.delivery
            .deliver(request.app_id, &code.email, &code.code, expiry_minutes)
            .await
    }

    async fn verify_magic_code(
        &self,
        request: VerifyMagicCodeRequest,
    ) -> Result<RuntimeAuthSession, ApiError> {
        self.verify_magic_code_with_context(request, None, None)
            .await
    }

    async fn verify_magic_code_with_origin(
        &self,
        request: VerifyMagicCodeRequest,
        origin: Option<String>,
    ) -> Result<RuntimeAuthSession, ApiError> {
        self.verify_magic_code_with_context(request, origin, None)
            .await
    }

    async fn verify_magic_code_with_context(
        &self,
        request: VerifyMagicCodeRequest,
        origin: Option<String>,
        client_ip: Option<String>,
    ) -> Result<RuntimeAuthSession, ApiError> {
        self.magic_code_rate_limiter
            .check_verify_from_ip(request.app_id, &request.email, client_ip.as_deref())
            .await?;
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let expiry_minutes = self
            .magic_code_expiry
            .expiry_minutes(request.app_id)
            .await?;
        let mut client = self
            .client
            .acquire()
            .await
            .map_err(runtime_database_error)?;
        let guest_user_id = if let Some(token) = request.refresh_token {
            let user = user_by_refresh_token(&*client, request.app_id, &catalog, token)
                .await
                .map_err(|error| auth_error_with_catalog(error, Some(&catalog)))?;
            refresh_token_guest_user_id(user, request.app_id, token)?
        } else {
            None
        };
        let normalized_email = request.email.trim().to_lowercase();
        let test_user_code = client
            .query_opt(
                "SELECT code FROM app_test_users WHERE app_id = $1 AND email = $2",
                &[&request.app_id, &normalized_email],
            )
            .await
            .map_err(runtime_database_error)?
            .map(|row| row.get::<_, String>(0));
        let policy = &self.policy;
        let session = verify_magic_code(
            &mut client,
            request.app_id,
            &catalog,
            &request.email,
            &request.code,
            guest_user_id,
            request.extra_fields,
            test_user_code
                .as_deref()
                .or(self.config.static_test_code.as_deref()),
            Utc::now().timestamp_millis(),
            expiry_minutes,
            |user| {
                let user = user.clone();
                async move {
                    policy
                        .authorize_signup_with_origin(request.app_id, &user, origin.as_deref())
                        .await
                }
            },
        )
        .await;
        let session = match session {
            Ok(session) => {
                self.magic_code_rate_limiter
                    .reset_verify_failures(request.app_id, &request.email)
                    .await?;
                session
            }
            Err(error) => {
                record_magic_code_failure(
                    &*self.magic_code_rate_limiter,
                    &mut client,
                    request.app_id,
                    &catalog,
                    &request.email,
                    &error,
                )
                .await?;
                return Err(auth_error_with_catalog(error, Some(&catalog)));
            }
        };
        Ok(RuntimeAuthSession {
            user: runtime_user(session.user, session.refresh_token),
            created: Some(session.created),
        })
    }

    async fn verify_refresh_token(
        &self,
        request: VerifyRefreshTokenRequest,
    ) -> Result<RuntimeUser, ApiError> {
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let client = self
            .client
            .acquire()
            .await
            .map_err(runtime_database_error)?;
        let user = user_by_refresh_token(&*client, request.app_id, &catalog, request.refresh_token)
            .await
            .map_err(|error| auth_error_with_catalog(error, Some(&catalog)))?
            .ok_or_else(|| {
                ApiError::new(ApiErrorType::RecordNotFound, "Record not found: app-user")
                    .with_hint(serde_json::json!({"record-type": "app-user"}))
            })?;
        Ok(runtime_user(user, request.refresh_token))
    }

    async fn sign_in_guest(&self, request: SignInGuestRequest) -> Result<RuntimeUser, ApiError> {
        self.sign_in_guest_with_origin(request, None).await
    }

    async fn sign_in_guest_with_origin(
        &self,
        request: SignInGuestRequest,
        origin: Option<String>,
    ) -> Result<RuntimeUser, ApiError> {
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let policy = &self.policy;
        let mut client = self
            .client
            .acquire()
            .await
            .map_err(runtime_database_error)?;
        let session = sign_in_guest_with_fields(
            &mut client,
            request.app_id,
            &catalog,
            request.extra_fields,
            |user| {
                let user = user.clone();
                async move {
                    policy
                        .authorize_signup_with_origin(request.app_id, &user, origin.as_deref())
                        .await
                }
            },
        )
        .await
        .map_err(|error| auth_error_with_catalog(error, Some(&catalog)))?;
        Ok(runtime_user(session.user, session.refresh_token))
    }

    async fn sign_out(&self, request: SignOutRequest) -> Result<(), ApiError> {
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let mut client = self
            .client
            .acquire()
            .await
            .map_err(runtime_database_error)?;
        delete_refresh_token(&mut client, request.app_id, &catalog, request.refresh_token)
            .await
            .map_err(|error| auth_error_with_catalog(error, Some(&catalog)))
    }
}

pub(crate) fn runtime_user(user: AppUser, refresh_token: Uuid) -> RuntimeUser {
    let is_guest = user.is_guest();
    RuntimeUser {
        id: user.id,
        refresh_token,
        email: user.email,
        image_url: user.image_url,
        user_type: user.user_type,
        is_guest,
        extra_fields: user.extra_fields,
    }
}

pub(crate) fn refresh_token_guest_user_id(
    user: Option<AppUser>,
    app_id: Uuid,
    refresh_token: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    let user = user.ok_or_else(|| missing_app_user(app_id, refresh_token))?;
    Ok(user.is_guest().then_some(user.id))
}

pub(crate) async fn record_magic_code_failure(
    limiter: &dyn MagicCodeRateLimiter,
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
    error: &AuthError,
) -> Result<(), ApiError> {
    if !matches!(
        error,
        AuthError::InvalidMagicCode | AuthError::ExpiredMagicCode
    ) {
        return Ok(());
    }
    if limiter.record_verify_failure(app_id, email).await? {
        invalidate_magic_codes(client, app_id, catalog, email)
            .await
            .map_err(|error| auth_error_with_catalog(error, Some(catalog)))?;
    }
    Ok(())
}

fn missing_app_user(app_id: Uuid, refresh_token: Uuid) -> ApiError {
    ApiError::new(ApiErrorType::RecordNotFound, "Record not found: app-user").with_hint(
        serde_json::json!({
            "args": [{
                "app-id": app_id,
                "refresh-token": refresh_token,
            }],
            "record-type": "app-user",
        }),
    )
}

pub(crate) fn auth_error(error: AuthError) -> ApiError {
    auth_error_with_catalog(error, None)
}

fn runtime_database_error(error: impl std::fmt::Display) -> ApiError {
    database_error(error)
}

fn auth_error_with_catalog(error: AuthError, catalog: Option<&AttributeCatalog>) -> ApiError {
    match error {
        AuthError::InvalidMagicCode => ApiError::new(
            ApiErrorType::RecordNotFound,
            "Record not found: app-user-magic-code",
        )
        .with_hint(serde_json::json!({"record-type": "app-user-magic-code"})),
        AuthError::ExpiredMagicCode => ApiError::new(
            ApiErrorType::RecordExpired,
            "Record expired: app-user-magic-code",
        ),
        AuthError::UnknownUserField(field) => ApiError::new(
            ApiErrorType::ValidationFailed,
            format!("Unknown field: {field}. It must be defined in your $users schema."),
        ),
        AuthError::SystemUserField(field) => ApiError::new(
            ApiErrorType::ValidationFailed,
            format!("Cannot set system field: {field}"),
        ),
        AuthError::SignupPermissionDenied => ApiError::new(
            ApiErrorType::PermissionDenied,
            "Permission denied for $users create",
        ),
        AuthError::SignupPermissionEvaluation(message) => {
            ApiError::new(ApiErrorType::PermissionEvaluationFailed, message)
        }
        AuthError::SharedOAuthUserLimit(limit) => ApiError::new(
            ApiErrorType::ValidationFailed,
            format!(
                "Shared dev credentials are limited to {limit} users. Please add your own client_id and client_secret in the dashboard."
            ),
        ),
        AuthError::InvalidToken => {
            ApiError::new(ApiErrorType::ParamMalformed, "Invalid authentication token")
        }
        AuthError::ExpiryOverflow => {
            ApiError::new(ApiErrorType::Unknown, "magic-code expiry is too large")
        }
        AuthError::InvalidOAuthCode => {
            ApiError::new(ApiErrorType::Unknown, "invalid OAuth authorization code")
        }
        AuthError::ExpiredOAuthCode => ApiError::new(
            ApiErrorType::Unknown,
            "OAuth authorization code has expired",
        ),
        AuthError::InvalidOAuthRedirect => {
            ApiError::new(ApiErrorType::Unknown, "invalid OAuth redirect state")
        }
        AuthError::ExpiredOAuthRedirect => {
            ApiError::new(ApiErrorType::Unknown, "OAuth redirect state has expired")
        }
        AuthError::Transaction(error) => {
            crate::admin_data::transaction_execution_error_with_catalog(error, catalog)
        }
        AuthError::Postgres(error) => runtime_database_error(error),
        AuthError::SystemCatalog(error) => internal_error(error),
        AuthError::OAuth(error) if error.is_client_facing() => {
            ApiError::new(ApiErrorType::Unknown, error.to_string())
        }
        AuthError::OAuth(error) => internal_error(error),
        AuthError::MissingCreatedUser(user_id) => {
            internal_error(format!("created user {user_id} could not be loaded"))
        }
        AuthError::MissingOAuthLinkUser(user_id) => {
            internal_error(format!("OAuth link {user_id} points to a missing user"))
        }
        AuthError::InvalidPersistedValue(value) => {
            internal_error(format!("invalid persisted authentication value: {value}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use instant_auth::oauth::OAuthError;

    use super::*;

    #[test]
    fn user_facing_oauth_errors_keep_their_client_text() {
        assert_eq!(
            auth_error(AuthError::InvalidOAuthCode),
            ApiError::new(ApiErrorType::Unknown, "invalid OAuth authorization code")
        );
        assert_eq!(
            auth_error(AuthError::ExpiredOAuthRedirect),
            ApiError::new(ApiErrorType::Unknown, "OAuth redirect state has expired")
        );
    }

    #[test]
    fn oauth_validation_errors_stay_visible_and_infrastructure_errors_do_not() {
        assert_eq!(
            auth_error(AuthError::OAuth(OAuthError::PkceMismatch)),
            ApiError::new(
                ApiErrorType::Unknown,
                "the code_challenge and code_verifier do not match"
            )
        );
        assert_eq!(
            auth_error(AuthError::OAuth(OAuthError::AudienceMismatch)),
            ApiError::new(
                ApiErrorType::Unknown,
                "the id_token was generated for the wrong OAuth client"
            )
        );
        for error in [
            OAuthError::HttpStatus(502),
            OAuthError::Dns(std::io::Error::other("lookup failed")),
        ] {
            let mapped = auth_error(AuthError::OAuth(error));
            assert_eq!(mapped.body().kind, ApiErrorType::Unknown);
            assert_eq!(mapped.body().message, "Internal error");
        }
    }

    #[test]
    fn postgres_auth_failures_hide_the_driver_message() {
        let error = runtime_database_error("error communicating with the server");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Database error");
    }

    #[test]
    fn internal_auth_failures_hide_the_cause() {
        let error = auth_error(AuthError::InvalidPersistedValue(serde_json::json!({
            "refresh-token": "secret-token"
        })));

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");
        assert!(!error.body().message.contains("secret-token"));
    }

    #[test]
    fn transaction_timeouts_keep_the_public_timeout_error_type() {
        assert_eq!(
            auth_error(AuthError::Transaction(
                instant_transact::ExecutionError::QueryTimeout,
            )),
            ApiError::new(
                ApiErrorType::Timeout,
                "PostgreSQL transaction statement exceeded its execution timeout",
            )
        );
    }

    #[test]
    fn app_users_flatten_custom_fields_and_compute_is_guest() {
        let token = Uuid::new_v4();
        let user = runtime_user(
            AppUser {
                id: Uuid::new_v4(),
                email: None,
                user_type: Some("guest".to_owned()),
                image_url: None,
                linked_primary_user: None,
                extra_fields: [("handle".to_owned(), serde_json::json!("one"))].into(),
            },
            token,
        );
        assert!(user.is_guest);
        assert_eq!(user.refresh_token, token);
        assert_eq!(user.extra_fields["handle"], "one");
    }

    #[test]
    fn public_auth_errors_preserve_magic_code_discriminators() {
        let error = auth_error(AuthError::InvalidMagicCode);
        assert_eq!(error.body().kind, ApiErrorType::RecordNotFound);
        assert_eq!(error.status().as_u16(), 400);
        let error = auth_error(AuthError::ExpiredMagicCode);
        assert_eq!(error.body().kind, ApiErrorType::RecordExpired);
    }

    #[test]
    fn supplied_refresh_token_must_resolve_before_magic_code_verification() {
        let app_id = Uuid::new_v4();
        let refresh_token = Uuid::new_v4();
        let error = refresh_token_guest_user_id(None, app_id, refresh_token).unwrap_err();
        assert_eq!(
            serde_json::to_value(error.body()).unwrap(),
            serde_json::json!({
                "type": "record-not-found",
                "message": "Record not found: app-user",
                "hint": {
                    "args": [{
                        "app-id": app_id,
                        "refresh-token": refresh_token,
                    }],
                    "record-type": "app-user"
                }
            })
        );

        let guest_id = Uuid::new_v4();
        let guest = AppUser {
            id: guest_id,
            email: None,
            user_type: Some("guest".to_owned()),
            image_url: None,
            linked_primary_user: None,
            extra_fields: std::collections::BTreeMap::default(),
        };
        assert_eq!(
            refresh_token_guest_user_id(Some(guest), app_id, refresh_token).unwrap(),
            Some(guest_id)
        );

        let user = AppUser {
            id: Uuid::new_v4(),
            email: Some("person@example.com".to_owned()),
            user_type: Some("user".to_owned()),
            image_url: None,
            linked_primary_user: None,
            extra_fields: std::collections::BTreeMap::default(),
        };
        assert_eq!(
            refresh_token_guest_user_id(Some(user), app_id, refresh_token).unwrap(),
            None
        );
    }
}
