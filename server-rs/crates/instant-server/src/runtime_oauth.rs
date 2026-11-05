use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use instant_auth::oauth::{
    DiscoveryDocument, GenericCodeExchange, IdTokenOptions, OAuthError, OAuthExchangeResult,
    OAuthUserInfo, apple_client_secret, exchange_generic_code, exchange_github_code,
    generic_authorization_url, parse_safe_url, verify_id_token,
};
use instant_auth::oauth_origins::{authorized_origins, is_authorized_origin};
use instant_auth::oauth_store::{
    OAuthClientRecord, OAuthProvider, OAuthSignIn, consume_oauth_code, consume_oauth_redirect,
    create_oauth_code, create_oauth_redirect, oauth_client_by_id, oauth_client_by_name,
    oauth_provider_by_id, sign_in_with_oauth_reusing_refresh,
};
use instant_auth::{
    AuthError, CreateUser, constant_time_code_eq, hash_refresh_token, user_by_refresh_token,
};
use instant_http::{
    ApiError, ApiErrorType, OAuthCallbackRequest, OAuthCallbackResponse, OAuthIdTokenRequest,
    OAuthSessionResponse, OAuthStartRequest, OAuthStartResponse, OAuthTokenRequest,
    RuntimeOAuthBackend,
};
use instant_instaql::AttributeCatalog;
use serde_json::{Value as JsonValue, json};
use tokio_postgres::Client;
use url::Url;
use uuid::Uuid;

use crate::db_error::{database_error, scrub_database_error};
use crate::postgres_pool::PostgresClientSource;
use crate::runtime_auth::{auth_error, runtime_user};
use crate::{OAuthMetadataCache, PostgresPool};

const GITHUB_AUTHORIZATION_ENDPOINT: &str = "https://github.com/login/oauth/authorize";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedOAuthCredentials {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Secret-management seam for stored or shared OAuth client credentials.
pub trait OAuthCredentialProvider: Send + Sync + 'static {
    fn resolve(
        &self,
        app_id: Uuid,
        client: &OAuthClientRecord,
        provider: &OAuthProvider,
    ) -> impl Future<Output = Result<ResolvedOAuthCredentials, ApiError>> + Send;
}

/// Per-application catalog and authorization decisions needed by OAuth signup.
pub trait RuntimeOAuthPolicy: Send + Sync + 'static {
    fn load_catalog(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send;

    /// # Errors
    ///
    /// Returns the application's signup-rule rejection.
    fn authorize_signup(
        &self,
        app_id: Uuid,
        user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;

    /// # Errors
    ///
    /// Returns the shared-credential quota or policy rejection.
    fn authorize_shared_credentials(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;
}

/// Static catalog policy useful for single-application deployments and tests.
#[derive(Clone, Debug)]
pub struct StaticOAuthPolicy {
    catalog: AttributeCatalog,
}

impl StaticOAuthPolicy {
    #[must_use]
    pub fn new(catalog: AttributeCatalog) -> Self {
        Self { catalog }
    }
}

impl RuntimeOAuthPolicy for StaticOAuthPolicy {
    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(self.catalog.clone()))
    }

    fn authorize_signup(
        &self,
        _app_id: Uuid,
        _user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        std::future::ready(Ok(()))
    }

    fn authorize_shared_credentials(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        std::future::ready(Ok(()))
    }
}

/// PostgreSQL-backed runtime OAuth coordinator.
///
/// Provider discovery and JWKS requests use `instant-auth`'s DNS-pinned,
/// bounded SSRF-safe HTTP implementation. Credential decryption and shared
/// credential selection stay behind [`OAuthCredentialProvider`].
pub struct RuntimeOAuthService<C, P> {
    client: PostgresClientSource,
    credentials: C,
    policy: P,
    server_origin: String,
    secure_cookie: bool,
    metadata: Arc<OAuthMetadataCache>,
}

impl<C, P> RuntimeOAuthService<C, P>
where
    C: OAuthCredentialProvider,
    P: RuntimeOAuthPolicy,
{
    #[must_use]
    pub fn new(
        client: Client,
        credentials: C,
        policy: P,
        server_origin: impl Into<String>,
        secure_cookie: bool,
    ) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            credentials,
            policy,
            server_origin: server_origin.into().trim_end_matches('/').to_owned(),
            secure_cookie,
            metadata: Arc::new(OAuthMetadataCache::new()),
        }
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        credentials: C,
        policy: P,
        server_origin: impl Into<String>,
        secure_cookie: bool,
    ) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            credentials,
            policy,
            server_origin: server_origin.into().trim_end_matches('/').to_owned(),
            secure_cookie,
            metadata: Arc::new(OAuthMetadataCache::new()),
        }
    }

    #[must_use]
    pub fn with_metadata_cache(mut self, metadata: Arc<OAuthMetadataCache>) -> Self {
        self.metadata = metadata;
        self
    }

    #[must_use]
    pub fn metadata_cache(&self) -> Arc<OAuthMetadataCache> {
        Arc::clone(&self.metadata)
    }

    fn callback_url(&self) -> String {
        format!("{}/runtime/oauth/callback", self.server_origin)
    }

    async fn client_and_provider(
        &self,
        app_id: Uuid,
        client_id: Uuid,
    ) -> Result<(OAuthClientRecord, OAuthProvider), ApiError> {
        let client = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        let oauth_client = oauth_client_by_id(&*client, app_id, client_id)
            .await
            .map_err(auth_error)?
            .ok_or_else(|| oauth_error("Missing OAuth client."))?;
        let provider = oauth_provider_by_id(&*client, app_id, oauth_client.provider_id)
            .await
            .map_err(auth_error)?
            .ok_or_else(|| oauth_error("Missing OAuth provider."))?;
        Ok((oauth_client, provider))
    }

    async fn discovery(
        &self,
        client: &OAuthClientRecord,
    ) -> Result<Option<DiscoveryDocument>, ApiError> {
        let Some(endpoint) = client.discovery_endpoint.as_deref() else {
            return Ok(None);
        };
        let endpoint = parse_safe_url(endpoint).map_err(oauth_transport)?;
        self.metadata
            .discovery(&endpoint)
            .await
            .map(Some)
            .map_err(oauth_transport)
    }

    async fn authorization_url(
        &self,
        app_id: Uuid,
        client: &OAuthClientRecord,
        provider: &OAuthProvider,
        state: &str,
        redirect_to: &str,
        hosted_domain: Option<&str>,
    ) -> Result<Url, ApiError> {
        let credentials = self.credentials.resolve(app_id, client, provider).await?;
        if provider.name == "github" {
            let mut url = Url::parse(GITHUB_AUTHORIZATION_ENDPOINT).map_err(oauth_transport)?;
            url.query_pairs_mut().extend_pairs([
                ("scope", "read:user user:email"),
                ("response_type", "code"),
                ("state", state),
                ("redirect_uri", redirect_to),
                ("client_id", credentials.client_id.as_str()),
            ]);
            return Ok(url);
        }
        let discovery = self
            .discovery(client)
            .await?
            .ok_or_else(|| oauth_error("Unsupported OAuth client."))?;
        let endpoint = discovery
            .authorization_endpoint
            .as_ref()
            .ok_or_else(|| oauth_error("OAuth discovery omitted authorization_endpoint."))?;
        Ok(generic_authorization_url(
            endpoint,
            state,
            redirect_to,
            &credentials.client_id,
            hosted_domain,
        ))
    }

    async fn exchange_provider_code(
        &self,
        app_id: Uuid,
        client: &OAuthClientRecord,
        provider: &OAuthProvider,
        code: &str,
        redirect_to: &str,
    ) -> Result<OAuthExchangeResult, ApiError> {
        let credentials = self.credentials.resolve(app_id, client, provider).await?;
        let secret = credentials
            .client_secret
            .as_deref()
            .ok_or_else(|| oauth_error("OAuth client secret is missing."))?;
        if provider.name == "github" {
            return exchange_github_code(&credentials.client_id, secret, code, redirect_to)
                .await
                .map_err(oauth_transport);
        }
        let discovery = self
            .discovery(client)
            .await?
            .ok_or_else(|| oauth_error("Unsupported OAuth client."))?;
        let token_endpoint = discovery
            .token_endpoint
            .as_ref()
            .ok_or_else(|| oauth_error("OAuth discovery omitted token_endpoint."))?;
        let effective_secret = if matches!(
            discovery.issuer.as_deref(),
            Some("https://account.apple.com" | "https://appleid.apple.com")
        ) {
            let meta = client.meta.as_ref().and_then(JsonValue::as_object);
            apple_client_secret(
                &credentials.client_id,
                meta.and_then(|value| value.get("teamId"))
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| oauth_error("Apple OAuth client is missing teamId."))?,
                meta.and_then(|value| value.get("keyId"))
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| oauth_error("Apple OAuth client is missing keyId."))?,
                secret,
                u64::try_from(Utc::now().timestamp()).unwrap_or_default(),
            )
            .map_err(oauth_transport)?
        } else {
            secret.to_owned()
        };
        exchange_generic_code(&GenericCodeExchange {
            token_endpoint,
            userinfo_endpoint: discovery.userinfo_endpoint.as_ref(),
            client_id: &credentials.client_id,
            client_secret: &effective_secret,
            code,
            redirect_uri: redirect_to,
        })
        .await
        .map_err(oauth_transport)
    }

    async fn verify_provider_id_token(
        &self,
        app_id: Uuid,
        client: &OAuthClientRecord,
        provider: &OAuthProvider,
        request: &OAuthIdTokenRequest,
    ) -> Result<OAuthUserInfo, ApiError> {
        if provider.name == "github" {
            return Err(validation("GitHub OAuth does not support ID tokens."));
        }
        let credentials = self.credentials.resolve(app_id, client, provider).await?;
        let discovery = self
            .discovery(client)
            .await?
            .ok_or_else(|| validation("OAuth client does not support id_token."))?;
        let jwks_uri = discovery
            .jwks_uri
            .as_ref()
            .ok_or_else(|| validation("OAuth client does not support id_token."))?;
        let issuer = discovery
            .issuer
            .as_deref()
            .ok_or_else(|| validation("OAuth client does not support id_token."))?;
        let algorithms: BTreeSet<String> = discovery.signing_algorithms();
        let mut jwks = self
            .metadata
            .jwks(jwks_uri, false)
            .await
            .map_err(oauth_transport)?;
        let allow_unverified_email = client
            .meta
            .as_ref()
            .and_then(|value| value.get("allowUnverifiedEmail"))
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let accepted_audiences =
            accepted_audiences(&credentials.client_id, &client.extra_audiences);
        let options = IdTokenOptions {
            issuer,
            accepted_audiences: &accepted_audiences,
            supported_algorithms: &algorithms,
            nonce: request.nonce.as_deref(),
            allow_unverified_email,
        };
        let mut verified = verify_id_token(&request.id_token, &jwks, &options);
        if matches!(verified, Err(OAuthError::MissingSigningKey))
            && self.metadata.refresh_jwks_on_missing_key(jwks_uri).await
        {
            jwks = self
                .metadata
                .jwks(jwks_uri, true)
                .await
                .map_err(oauth_transport)?;
            verified = verify_id_token(&request.id_token, &jwks, &options);
        }
        verified.map_err(|error| validation(&error.to_string()))
    }

    async fn assert_origin(
        &self,
        app_id: Uuid,
        oauth_client: &OAuthClientRecord,
        origin: Option<&str>,
    ) -> Result<(), ApiError> {
        let Some(origin) = origin else {
            return Ok(());
        };
        let client = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        let origins = authorized_origins(&*client, app_id)
            .await
            .map_err(auth_error)?;
        if is_authorized_origin(&origins, origin, oauth_client.use_shared_credentials) {
            Ok(())
        } else {
            Err(validation("Unauthorized origin."))
        }
    }

    async fn oauth_sign_in(
        &self,
        app_id: Uuid,
        client: &OAuthClientRecord,
        user_info: OAuthUserInfo,
        refresh_token: Option<Uuid>,
        reuse_current_refresh: bool,
        extra_fields: std::collections::BTreeMap<String, JsonValue>,
    ) -> Result<instant_auth::oauth_store::OAuthSession, ApiError> {
        let catalog = self.policy.load_catalog(app_id).await?;
        let mut database = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        let current_user = match refresh_token {
            Some(token) => user_by_refresh_token(&*database, app_id, &catalog, token)
                .await
                .map_err(auth_error)?,
            None => None,
        };
        let guest_user_id = current_user
            .as_ref()
            .filter(|user| user.is_guest())
            .map(|user| user.id);
        let reusable_refresh = reuse_current_refresh
            .then(|| refresh_token.zip(current_user.as_ref().map(|user| user.id)))
            .flatten();
        let policy = &self.policy;
        sign_in_with_oauth_reusing_refresh(
            &mut database,
            app_id,
            &catalog,
            &OAuthSignIn {
                provider_id: client.provider_id,
                subject: user_info.subject,
                email: user_info.email.map(|value| value.trim().to_lowercase()),
                image_url: user_info.image_url,
                guest_user_id,
                extra_fields,
                use_shared_credentials: client.use_shared_credentials,
            },
            reusable_refresh,
            |user| {
                let user = user.clone();
                async move { policy.authorize_signup(app_id, &user).await }
            },
            || policy.authorize_shared_credentials(app_id),
        )
        .await
        .map_err(auth_error)
    }
}

impl<C, P> RuntimeOAuthBackend for RuntimeOAuthService<C, P>
where
    C: OAuthCredentialProvider,
    P: RuntimeOAuthPolicy,
{
    async fn start(&self, request: OAuthStartRequest) -> Result<OAuthStartResponse, ApiError> {
        let (oauth_client, provider, origins) = {
            let database = self
                .client
                .acquire()
                .await
                .map_err(runtime_oauth_database_error)?;
            let oauth_client =
                oauth_client_by_name(&*database, request.app_id, &request.client_name)
                    .await
                    .map_err(auth_error)?
                    .ok_or_else(|| oauth_error("Missing OAuth client."))?;
            let provider =
                oauth_provider_by_id(&*database, request.app_id, oauth_client.provider_id)
                    .await
                    .map_err(auth_error)?
                    .ok_or_else(|| oauth_error("Missing OAuth provider."))?;
            let origins = authorized_origins(&*database, request.app_id)
                .await
                .map_err(auth_error)?;
            (oauth_client, provider, origins)
        };
        if !is_authorized_origin(
            &origins,
            &request.redirect_uri,
            oauth_client.use_shared_credentials,
        ) {
            return Err(validation(
                "Invalid redirect_uri. If you're the developer, make sure to add your website to the list of approved domains.",
            ));
        }
        let app_redirect = add_query(
            &request.redirect_uri,
            request
                .state
                .as_ref()
                .map(|state| ("state", state.as_str())),
        )?;
        let state = Uuid::new_v4();
        let cookie = Uuid::new_v4();
        let provider_state = format!("{}{state}", request.app_id);
        let redirect_to = oauth_client
            .redirect_to
            .clone()
            .unwrap_or_else(|| self.callback_url());
        let authorization_url = self
            .authorization_url(
                request.app_id,
                &oauth_client,
                &provider,
                &provider_state,
                &redirect_to,
                request.hosted_domain.as_deref(),
            )
            .await?;
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let mut database = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        create_oauth_redirect(
            &mut database,
            request.app_id,
            &catalog,
            state,
            cookie,
            &app_redirect,
            Some(&redirect_to),
            oauth_client.id,
            request.code_challenge_method.as_deref(),
            request.code_challenge.as_deref(),
        )
        .await
        .map_err(oauth_model_error)?;
        Ok(OAuthStartResponse {
            authorization_url: authorization_url.to_string(),
            cookie,
            secure_cookie: self.secure_cookie,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn callback(
        &self,
        request: OAuthCallbackRequest,
    ) -> Result<OAuthCallbackResponse, ApiError> {
        if let Some(error) = request.params.get("error") {
            return Err(oauth_error(error));
        }
        let state_param = request
            .params
            .get("state")
            .ok_or_else(|| oauth_error("Missing state param in OAuth redirect."))?;
        if state_param.len() != 72 {
            return Err(oauth_error("Invalid state param in OAuth redirect."));
        }
        let app_id = Uuid::parse_str(&state_param[..36])
            .map_err(|_| oauth_error("Invalid state param in OAuth redirect."))?;
        let state = Uuid::parse_str(&state_param[36..])
            .map_err(|_| oauth_error("Invalid state param in OAuth redirect."))?;
        let cookie = request
            .cookie
            .ok_or_else(|| oauth_error("Missing cookie."))?;
        let catalog = self.policy.load_catalog(app_id).await?;
        let mut database = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        let oauth_redirect = consume_oauth_redirect(
            &mut database,
            app_id,
            &catalog,
            state,
            Utc::now().timestamp_millis(),
        )
        .await
        .map_err(oauth_model_error)?;
        drop(database);
        let expected = hash_refresh_token(cookie);
        let actual = hex(&oauth_redirect.cookie_hash);
        if !constant_time_code_eq(&expected, &actual) {
            return Err(oauth_error("Mismatch in OAuth request cookie."));
        }
        let provider_code = request
            .params
            .get("code")
            .ok_or_else(|| oauth_error("Missing code param in OAuth redirect."))?;
        let (oauth_client, provider) = self
            .client_and_provider(app_id, oauth_redirect.client_id)
            .await?;
        let redirect_to = oauth_redirect
            .redirect_to
            .clone()
            .unwrap_or_else(|| self.callback_url());
        let user_info = match self
            .exchange_provider_code(
                app_id,
                &oauth_client,
                &provider,
                provider_code,
                &redirect_to,
            )
            .await?
        {
            OAuthExchangeResult::Success(info) => info,
            OAuthExchangeResult::Error(message) => {
                return Ok(OAuthCallbackResponse::Redirect(add_query_pairs(
                    &oauth_redirect.redirect_url,
                    &[("error", &message), ("_instant_oauth_redirect", "true")],
                )?));
            }
        };
        let email = user_info.email.clone().unwrap_or_default();
        let code = Uuid::new_v4();
        let mut database = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        create_oauth_code(
            &mut database,
            app_id,
            &catalog,
            code,
            oauth_client.id,
            json!({
                "email": user_info.email,
                "sub": user_info.subject,
                "imageURL": user_info.image_url,
            }),
            oauth_redirect.code_challenge_method.as_deref(),
            oauth_redirect.code_challenge.as_deref(),
        )
        .await
        .map_err(auth_error)?;
        let redirect_url = add_query_pairs(
            &oauth_redirect.redirect_url,
            &[
                ("code", &code.to_string()),
                ("_instant_oauth_redirect", "true"),
            ],
        )?;
        let is_web =
            Url::parse(&redirect_url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
        if is_web {
            Ok(OAuthCallbackResponse::Redirect(redirect_url))
        } else {
            Ok(OAuthCallbackResponse::ExternalApp {
                email,
                redirect_url,
            })
        }
    }

    async fn exchange_code(
        &self,
        request: OAuthTokenRequest,
    ) -> Result<OAuthSessionResponse, ApiError> {
        let catalog = self.policy.load_catalog(request.app_id).await?;
        let mut database = self
            .client
            .acquire()
            .await
            .map_err(runtime_oauth_database_error)?;
        let code = consume_oauth_code(
            &mut database,
            request.app_id,
            &catalog,
            request.code,
            request.code_verifier.as_deref(),
            Utc::now().timestamp_millis(),
        )
        .await
        .map_err(oauth_model_error)?;
        drop(database);
        let (oauth_client, _) = self
            .client_and_provider(request.app_id, code.client_id)
            .await?;
        self.assert_origin(request.app_id, &oauth_client, request.origin.as_deref())
            .await?;
        let user_info = stored_user_info(&code.user_info)?;
        let session = self
            .oauth_sign_in(
                request.app_id,
                &oauth_client,
                user_info,
                request.refresh_token,
                false,
                request.extra_fields,
            )
            .await?;
        Ok(OAuthSessionResponse {
            user: runtime_user(session.user, session.refresh_token),
            created: session.created,
            refresh_token: Some(session.refresh_token),
        })
    }

    async fn exchange_id_token(
        &self,
        request: OAuthIdTokenRequest,
    ) -> Result<OAuthSessionResponse, ApiError> {
        let (oauth_client, provider) = {
            let database = self
                .client
                .acquire()
                .await
                .map_err(runtime_oauth_database_error)?;
            let oauth_client =
                oauth_client_by_name(&*database, request.app_id, &request.client_name)
                    .await
                    .map_err(auth_error)?
                    .ok_or_else(|| oauth_error("Missing OAuth client."))?;
            let provider =
                oauth_provider_by_id(&*database, request.app_id, oauth_client.provider_id)
                    .await
                    .map_err(auth_error)?
                    .ok_or_else(|| oauth_error("Missing OAuth provider."))?;
            (oauth_client, provider)
        };
        self.assert_origin(request.app_id, &oauth_client, request.origin.as_deref())
            .await?;
        let user_info = self
            .verify_provider_id_token(request.app_id, &oauth_client, &provider, &request)
            .await?;
        let session = self
            .oauth_sign_in(
                request.app_id,
                &oauth_client,
                user_info,
                request.refresh_token,
                true,
                request.extra_fields,
            )
            .await?;
        Ok(OAuthSessionResponse {
            user: runtime_user(session.user, session.refresh_token),
            created: session.created,
            refresh_token: None,
        })
    }
}

fn stored_user_info(value: &JsonValue) -> Result<OAuthUserInfo, ApiError> {
    let object = value
        .as_object()
        .ok_or_else(|| oauth_error("OAuth code contained invalid user info."))?;
    let subject = object
        .get("sub")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| oauth_error("OAuth code omitted the user subject."))?;
    Ok(OAuthUserInfo {
        subject: subject.to_owned(),
        email: object
            .get("email")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        image_url: object
            .get("imageURL")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
    })
}

fn add_query(url: &str, pair: Option<(&str, &str)>) -> Result<String, ApiError> {
    pair.map_or_else(
        || Ok(url.to_owned()),
        |pair| add_query_pairs(url, std::slice::from_ref(&pair)),
    )
}

fn add_query_pairs(url: &str, pairs: &[(&str, &str)]) -> Result<String, ApiError> {
    let mut url = Url::parse(url).map_err(|_| validation("Invalid redirect URI."))?;
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    Ok(url.to_string())
}

fn accepted_audiences(client_id: &str, extra_audiences: &[String]) -> Vec<String> {
    let mut accepted = Vec::with_capacity(extra_audiences.len() + 1);
    accepted.push(client_id.to_owned());
    for audience in extra_audiences {
        if !accepted.iter().any(|existing| existing == audience) {
            accepted.push(audience.clone());
        }
    }
    accepted
}

fn oauth_error(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorType::OauthError, message)
}

fn runtime_oauth_database_error(error: impl std::fmt::Display) -> ApiError {
    database_error(error)
}

fn oauth_transport(error: impl std::fmt::Display) -> ApiError {
    let scrubbed = scrub_database_error(&error.to_string());
    tracing::warn!(
        target: "backendrelay::oauth",
        { error.message = %scrubbed },
        "OAuth provider transport failure"
    );
    oauth_error("Could not reach the OAuth provider")
}

fn oauth_configuration_error(error: impl std::fmt::Display) -> ApiError {
    let scrubbed = scrub_database_error(&error.to_string());
    tracing::warn!(
        target: "backendrelay::oauth",
        { error.message = %scrubbed },
        "OAuth provider configuration failure"
    );
    oauth_error("OAuth provider configuration error")
}

fn oauth_model_error(error: AuthError) -> ApiError {
    match error {
        AuthError::InvalidOAuthCode
        | AuthError::ExpiredOAuthCode
        | AuthError::InvalidOAuthRedirect
        | AuthError::ExpiredOAuthRedirect => oauth_error(error.to_string()),
        AuthError::OAuth(oauth) if oauth.is_client_facing() => oauth_error(oauth.to_string()),
        AuthError::OAuth(
            oauth @ (OAuthError::MissingSigningKey
            | OAuthError::UnsupportedSigningAlgorithm
            | OAuthError::AppleClientSecret(_)),
        ) => oauth_configuration_error(oauth),
        AuthError::OAuth(oauth) => oauth_transport(oauth),
        other => auth_error(other),
    }
}

fn validation(message: &str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn hex(value: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(value.len() * 2);
    for byte in value {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_query_helpers_preserve_existing_state() {
        let result = add_query_pairs(
            "myapp://callback?state=caller",
            &[("code", "one"), ("_instant_oauth_redirect", "true")],
        )
        .unwrap();
        assert!(result.contains("state=caller"));
        assert!(result.contains("code=one"));
        assert!(result.contains("_instant_oauth_redirect=true"));
    }

    #[test]
    fn accepted_audiences_always_include_the_resolved_client_id() {
        assert_eq!(
            accepted_audiences("client", &["native-client".to_owned(), "client".to_owned()]),
            vec!["client".to_owned(), "native-client".to_owned()]
        );
    }

    #[test]
    fn oauth_transport_errors_hide_the_provider_response() {
        let error = oauth_transport(
            "tls handshake failed for https://provider.example/token?client_secret=abc123",
        );
        assert_eq!(error.body().kind, ApiErrorType::OauthError);
        assert_eq!(error.body().message, "Could not reach the OAuth provider");
        assert!(!error.body().message.contains("provider.example"));
    }

    #[test]
    fn oauth_model_errors_keep_integration_text_and_hide_infrastructure() {
        assert_eq!(
            oauth_model_error(AuthError::OAuth(OAuthError::PkceMismatch)),
            oauth_error("the code_challenge and code_verifier do not match")
        );
        assert_eq!(
            oauth_model_error(AuthError::OAuth(OAuthError::AudienceMismatch)),
            oauth_error("the id_token was generated for the wrong OAuth client")
        );
        for error in [
            OAuthError::HttpStatus(502),
            OAuthError::Dns(std::io::Error::other("lookup failed")),
        ] {
            let mapped = oauth_model_error(AuthError::OAuth(error));
            assert_eq!(mapped.body().kind, ApiErrorType::OauthError);
            assert_eq!(mapped.body().message, "Could not reach the OAuth provider");
        }
        let signing_error =
            || jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidToken);
        for error in [
            OAuthError::MissingSigningKey,
            OAuthError::UnsupportedSigningAlgorithm,
            OAuthError::AppleClientSecret(signing_error()),
        ] {
            assert_eq!(
                oauth_model_error(AuthError::OAuth(error)),
                oauth_error("OAuth provider configuration error")
            );
        }
    }

    #[test]
    fn stored_user_info_requires_a_subject() {
        assert!(stored_user_info(&json!({"email": "a@example.com"})).is_err());
        assert_eq!(
            stored_user_info(&json!({"sub": "42", "imageURL": "image"}))
                .unwrap()
                .subject,
            "42"
        );
    }
}
