use std::sync::Arc;

use chrono::Utc;
use instant_auth::{
    AppUser, CreateUser, create_magic_code, create_refresh_token, create_user,
    delete_refresh_token, delete_user as delete_app_user, delete_user_refresh_tokens, get_user,
    sign_in_guest_with_fields, user_by_email, user_by_refresh_token, verify_magic_code,
};
use instant_http::{
    AdminAuthBackend, AdminCreateTokenRequest, AdminMagicCodeRequest, AdminRequestContext,
    AdminSignInGuestRequest, AdminSignOutRequest, AdminUser, AdminUserSelector,
    AdminVerifyMagicCodeRequest, ApiError, ApiErrorType, RuntimeAuthSession, RuntimeUser,
};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::admin_data::{AdminDataAuthorizer, AdminDataScope, internal_error, require_admin};
use crate::postgres_pool::PostgresClientSource;
use crate::runtime_auth::{
    MagicCodeDelivery, MagicCodeExpiryPolicy, MagicCodeRateLimiter, NoopMagicCodeRateLimiter,
    RuntimeAuthConfig, StaticMagicCodeExpiry, auth_error, record_magic_code_failure,
    refresh_token_guest_user_id, runtime_user,
};

/// PostgreSQL-backed implementation of the official admin SDK auth routes.
pub struct AdminAuthService<D, A> {
    client: PostgresClientSource,
    authorizer: Arc<A>,
    delivery: D,
    config: RuntimeAuthConfig,
    magic_code_expiry: Arc<dyn MagicCodeExpiryPolicy>,
    magic_code_rate_limiter: Arc<dyn MagicCodeRateLimiter>,
}

impl<D, A> AdminAuthService<D, A>
where
    D: MagicCodeDelivery,
    A: AdminDataAuthorizer,
{
    #[must_use]
    pub fn new(client: Client, authorizer: Arc<A>, delivery: D, config: RuntimeAuthConfig) -> Self {
        let magic_code_expiry = Arc::new(StaticMagicCodeExpiry(config.magic_code_expiry_minutes));
        Self {
            client: PostgresClientSource::fixed(client),
            authorizer,
            delivery,
            config,
            magic_code_expiry,
            magic_code_rate_limiter: Arc::new(NoopMagicCodeRateLimiter),
        }
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        authorizer: Arc<A>,
        delivery: D,
        config: RuntimeAuthConfig,
    ) -> Self {
        let magic_code_expiry = Arc::new(StaticMagicCodeExpiry(config.magic_code_expiry_minutes));
        Self {
            client: PostgresClientSource::pooled(pool),
            authorizer,
            delivery,
            config,
            magic_code_expiry,
            magic_code_rate_limiter: Arc::new(NoopMagicCodeRateLimiter),
        }
    }

    #[must_use]
    pub fn with_magic_code_expiry_policy(mut self, policy: Arc<dyn MagicCodeExpiryPolicy>) -> Self {
        self.magic_code_expiry = policy;
        self
    }

    #[must_use]
    pub fn with_magic_code_rate_limiter(mut self, limiter: Arc<dyn MagicCodeRateLimiter>) -> Self {
        self.magic_code_rate_limiter = limiter;
        self
    }

    async fn authorize(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<(Uuid, instant_instaql::AttributeCatalog), ApiError> {
        require_admin(&context)?;
        let app_id = context.app_id;
        let catalog = self.authorizer.authorize(context, scope).await?;
        Ok((app_id, catalog))
    }
}

impl<D, A> AdminAuthBackend for AdminAuthService<D, A>
where
    D: MagicCodeDelivery,
    A: AdminDataAuthorizer,
{
    async fn get_user(
        &self,
        context: AdminRequestContext,
        selector: AdminUserSelector,
    ) -> Result<Option<AdminUser>, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Read).await?;
        let client = self.client.acquire().await.map_err(internal_error)?;
        selected_user(&client, app_id, &catalog, &selector)
            .await
            .map(|user| user.map(admin_user))
    }

    async fn delete_user(
        &self,
        context: AdminRequestContext,
        selector: AdminUserSelector,
    ) -> Result<Option<AdminUser>, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let Some(user) = selected_user(&client, app_id, &catalog, &selector).await? else {
            return Ok(None);
        };
        delete_app_user(&mut client, app_id, &catalog, user.id)
            .await
            .map_err(auth_error)
            .map(|deleted| deleted.map(admin_user))
    }

    async fn create_token(
        &self,
        context: AdminRequestContext,
        request: AdminCreateTokenRequest,
    ) -> Result<RuntimeAuthSession, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let existing = match &request.selector {
            AdminUserSelector::Email(email) => user_by_email(&*client, app_id, &catalog, email)
                .await
                .map_err(auth_error)?,
            AdminUserSelector::Id(id) => get_user(&*client, app_id, &catalog, *id)
                .await
                .map_err(auth_error)?,
            AdminUserSelector::RefreshToken(_) => {
                return Err(ApiError::new(
                    ApiErrorType::ValidationFailed,
                    "Refresh tokens cannot select a user for token creation",
                ));
            }
        };
        let created = existing.is_none();
        let user = if let Some(user) = existing {
            user
        } else {
            let (id, email) = match request.selector {
                AdminUserSelector::Email(email) => (Uuid::new_v4(), Some(email)),
                AdminUserSelector::Id(id) => (id, None),
                AdminUserSelector::RefreshToken(_) => unreachable!(),
            };
            create_user(
                &mut client,
                app_id,
                &catalog,
                &CreateUser {
                    id,
                    email,
                    user_type: None,
                    image_url: None,
                    extra_fields: request.extra_fields,
                },
            )
            .await
            .map_err(auth_error)?
        };
        let token = create_refresh_token(&mut client, app_id, &catalog, user.id, None)
            .await
            .map_err(auth_error)?;
        Ok(RuntimeAuthSession {
            user: runtime_user(user, token.token),
            created: Some(created),
        })
    }

    async fn sign_out(
        &self,
        context: AdminRequestContext,
        request: AdminSignOutRequest,
    ) -> Result<(), ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        match request.selector {
            AdminUserSelector::Id(user_id) => {
                delete_user_refresh_tokens(&mut client, app_id, &catalog, user_id)
                    .await
                    .map_err(auth_error)
            }
            AdminUserSelector::Email(email) => {
                let user = user_by_email(&*client, app_id, &catalog, &email)
                    .await
                    .map_err(auth_error)?
                    .ok_or_else(missing_app_user)?;
                delete_user_refresh_tokens(&mut client, app_id, &catalog, user.id)
                    .await
                    .map_err(auth_error)
            }
            AdminUserSelector::RefreshToken(token) => {
                delete_refresh_token(&mut client, app_id, &catalog, token)
                    .await
                    .map_err(auth_error)
            }
        }
    }

    async fn generate_magic_code(
        &self,
        context: AdminRequestContext,
        request: AdminMagicCodeRequest,
    ) -> Result<String, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let code = create_magic_code(&mut client, app_id, &catalog, &request.email, None)
            .await
            .map_err(auth_error)?;
        drop(client);
        self.magic_code_rate_limiter
            .reset_verify_failures(app_id, &request.email)
            .await?;
        Ok(code.code)
    }

    async fn send_magic_code(
        &self,
        context: AdminRequestContext,
        request: AdminMagicCodeRequest,
    ) -> Result<String, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        self.magic_code_rate_limiter
            .check_send(app_id, &request.email)
            .await?;
        let expiry_minutes = self.magic_code_expiry.expiry_minutes(app_id).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let code = create_magic_code(&mut client, app_id, &catalog, &request.email, None)
            .await
            .map_err(auth_error)?;
        drop(client);
        self.magic_code_rate_limiter
            .reset_verify_failures(app_id, &request.email)
            .await?;
        self.delivery
            .deliver(app_id, &code.email, &code.code, expiry_minutes)
            .await?;
        Ok(code.code)
    }

    async fn verify_magic_code(
        &self,
        context: AdminRequestContext,
        request: AdminVerifyMagicCodeRequest,
    ) -> Result<RuntimeAuthSession, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        let expiry_minutes = self.magic_code_expiry.expiry_minutes(app_id).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let guest_user_id = if let Some(token) = request.refresh_token {
            let user = user_by_refresh_token(&*client, app_id, &catalog, token)
                .await
                .map_err(auth_error)?;
            refresh_token_guest_user_id(user, app_id, token)?
        } else {
            None
        };
        let session = verify_magic_code(
            &mut client,
            app_id,
            &catalog,
            &request.email,
            &request.code,
            guest_user_id,
            request.extra_fields,
            self.config.static_test_code.as_deref(),
            Utc::now().timestamp_millis(),
            expiry_minutes,
            |_| async { Ok(()) },
        )
        .await;
        let session = match session {
            Ok(session) => {
                self.magic_code_rate_limiter
                    .reset_verify_failures(app_id, &request.email)
                    .await?;
                session
            }
            Err(error) => {
                record_magic_code_failure(
                    &*self.magic_code_rate_limiter,
                    &mut client,
                    app_id,
                    &catalog,
                    &request.email,
                    &error,
                )
                .await?;
                return Err(auth_error(error));
            }
        };
        Ok(RuntimeAuthSession {
            user: runtime_user(session.user, session.refresh_token),
            created: Some(session.created),
        })
    }

    async fn sign_in_guest(
        &self,
        context: AdminRequestContext,
        request: AdminSignInGuestRequest,
    ) -> Result<RuntimeUser, ApiError> {
        let (app_id, catalog) = self.authorize(context, AdminDataScope::Write).await?;
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let session = sign_in_guest_with_fields(
            &mut client,
            app_id,
            &catalog,
            request.extra_fields,
            |_| async { Ok(()) },
        )
        .await
        .map_err(auth_error)?;
        Ok(runtime_user(session.user, session.refresh_token))
    }
}

async fn selected_user(
    client: &Client,
    app_id: Uuid,
    catalog: &instant_instaql::AttributeCatalog,
    selector: &AdminUserSelector,
) -> Result<Option<AppUser>, ApiError> {
    match selector {
        AdminUserSelector::Email(email) => user_by_email(client, app_id, catalog, email)
            .await
            .map_err(auth_error),
        AdminUserSelector::Id(id) => get_user(client, app_id, catalog, *id)
            .await
            .map_err(auth_error),
        AdminUserSelector::RefreshToken(token) => {
            user_by_refresh_token(client, app_id, catalog, *token)
                .await
                .map_err(auth_error)
        }
    }
}

pub(crate) fn admin_user(user: AppUser) -> AdminUser {
    let is_guest = user.is_guest();
    AdminUser {
        id: user.id,
        email: user.email,
        image_url: user.image_url,
        user_type: user.user_type,
        is_guest,
        extra_fields: user.extra_fields,
    }
}

fn missing_app_user() -> ApiError {
    ApiError::new(ApiErrorType::RecordNotFound, "Record not found: app-user")
        .with_hint(serde_json::json!({"record-type": "app-user"}))
}
