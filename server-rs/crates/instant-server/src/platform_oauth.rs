use std::future::Future;

use instant_http::{
    ApiError, ApiErrorType, PlatformOAuthBackend, PlatformOAuthClaimRequest,
    PlatformOAuthDecisionRequest, PlatformOAuthStartRequest, PlatformOAuthStartResponse,
    PlatformOAuthTokenRequest,
};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::ServerConfig;

const ALL_SCOPES: &[&str] = &[
    "apps-read",
    "apps-write",
    "data-read",
    "data-write",
    "storage-read",
    "storage-write",
];

/// Durable OAuth-app model boundary. Implementations atomically claim one-use records
/// and retain only token hashes, as the JVM model does.
pub trait PlatformOAuthStore: Send + Sync + 'static {
    fn create_redirect(
        &self,
        redirect_id: Uuid,
        cookie: Uuid,
        request: PlatformOAuthStartRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn claim_redirect(
        &self,
        request: PlatformOAuthClaimRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn grant_redirect(
        &self,
        request: PlatformOAuthDecisionRequest,
        code: Uuid,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;

    fn deny_redirect(
        &self,
        request: PlatformOAuthDecisionRequest,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;

    fn exchange_token(
        &self,
        request: PlatformOAuthTokenRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn access_token_info(
        &self,
        access_token: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn revoke_token(&self, token: String) -> impl Future<Output = Result<(), ApiError>> + Send;
}

/// OAuth 2.0 authorization-server protocol coordinator for third-party Instant apps.
pub struct PlatformOAuthService<S> {
    store: S,
    dashboard_origin: Url,
    secure_cookie: bool,
}

impl<S> PlatformOAuthService<S> {
    /// Creates the service from the process's validated public configuration.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::new`].
    pub fn from_config(store: S, config: &ServerConfig) -> Result<Self, String> {
        Self::new(store, config.dashboard_origin(), config.secure_cookies())
    }

    /// Creates the service from an absolute dashboard origin.
    ///
    /// # Errors
    ///
    /// Rejects non-HTTP(S) origins, origins with credentials, query strings, or fragments.
    pub fn new(store: S, dashboard_origin: &str, secure_cookie: bool) -> Result<Self, String> {
        let dashboard_origin = Url::parse(dashboard_origin).map_err(|error| error.to_string())?;
        if !matches!(dashboard_origin.scheme(), "http" | "https")
            || dashboard_origin.host_str().is_none()
            || !dashboard_origin.username().is_empty()
            || dashboard_origin.password().is_some()
            || dashboard_origin.query().is_some()
            || dashboard_origin.fragment().is_some()
        {
            return Err("Dashboard origin must be an absolute HTTP(S) origin".to_owned());
        }
        Ok(Self {
            store,
            dashboard_origin,
            secure_cookie,
        })
    }
}

impl<S> PlatformOAuthBackend for PlatformOAuthService<S>
where
    S: PlatformOAuthStore,
{
    async fn start(
        &self,
        request: PlatformOAuthStartRequest,
    ) -> Result<PlatformOAuthStartResponse, ApiError> {
        for scope in &request.scopes {
            if !ALL_SCOPES.contains(&scope.as_str()) {
                return Err(malformed(format!("Invalid scope {scope}")));
            }
        }
        validate_redirect_uri(&request.redirect_uri)?;
        let redirect_id = Uuid::new_v4();
        let cookie = Uuid::new_v4();
        self.store
            .create_redirect(redirect_id, cookie, request)
            .await?;
        let mut dashboard_url = self.dashboard_origin.clone();
        dashboard_url.set_path("/platform/oauth/start");
        dashboard_url
            .query_pairs_mut()
            .append_pair("redirect-id", &redirect_id.to_string());
        Ok(PlatformOAuthStartResponse {
            dashboard_url: dashboard_url.into(),
            cookie,
            secure_cookie: self.secure_cookie,
        })
    }

    async fn claim(&self, request: PlatformOAuthClaimRequest) -> Result<Value, ApiError> {
        self.store.claim_redirect(request).await
    }

    async fn grant(&self, request: PlatformOAuthDecisionRequest) -> Result<String, ApiError> {
        self.store.grant_redirect(request, Uuid::new_v4()).await
    }

    async fn deny(&self, request: PlatformOAuthDecisionRequest) -> Result<String, ApiError> {
        require_cookie(&request)?;
        self.store.deny_redirect(request).await
    }

    async fn token(&self, request: PlatformOAuthTokenRequest) -> Result<Value, ApiError> {
        validate_token_request(&request)?;
        self.store.exchange_token(request).await
    }

    async fn token_info(&self, access_token: String) -> Result<Value, ApiError> {
        if !access_token.starts_with("pat_") {
            return Err(validation(
                "The access_token is not a valid platform OAuth access token.",
            ));
        }
        self.store.access_token_info(access_token).await
    }

    async fn revoke(&self, token: String) -> Result<(), ApiError> {
        if !(token.starts_with("pat_") || token.starts_with("prt_")) {
            return Err(malformed(
                "Token is not an access token or a refresh token.",
            ));
        }
        self.store.revoke_token(token).await
    }
}

fn validate_redirect_uri(input: &str) -> Result<(), ApiError> {
    let uri = Url::parse(input).map_err(|_| malformed("Invalid redirect_uri"))?;
    if !matches!(uri.scheme(), "http" | "https") || uri.host_str().is_none() {
        return Err(malformed("Invalid redirect_uri"));
    }
    if uri.username().is_empty() && uri.password().is_none() {
        Ok(())
    } else {
        Err(malformed("redirect_uri must not include credentials"))
    }
}

fn require_cookie(request: &PlatformOAuthDecisionRequest) -> Result<(), ApiError> {
    request
        .cookie
        .map(|_| ())
        .ok_or_else(|| ApiError::new(ApiErrorType::ParamMissing, "Missing cookie."))
}

fn validate_token_request(request: &PlatformOAuthTokenRequest) -> Result<(), ApiError> {
    match (
        request.client_secret.as_deref(),
        request.grant_type.as_str(),
    ) {
        (None, "authorization_code") => {
            require(request.code.as_ref(), "code")?;
            require_non_blank(request.redirect_uri.as_deref(), "redirect_uri")?;
            require_non_blank(request.code_verifier.as_deref(), "code_verifier")
        }
        (None, _) => Err(malformed(
            "Only authorization_code is allowed for the PKCE flow.",
        )),
        (Some(_), "authorization_code") => {
            require(request.code.as_ref(), "code")?;
            require_non_blank(request.redirect_uri.as_deref(), "redirect_uri")
        }
        (Some(_), "refresh_token") => {
            require_non_blank(request.refresh_token.as_deref(), "refresh_token")
        }
        (Some(_), _) => Err(malformed(
            "Expected authorization_code or refresh_token grant_type.",
        )),
    }
}

fn require<T>(value: Option<&T>, name: &'static str) -> Result<(), ApiError> {
    value
        .map(|_| ())
        .ok_or_else(|| ApiError::new(ApiErrorType::ParamMissing, format!("Missing {name}")))
}

fn require_non_blank(value: Option<&str>, name: &'static str) -> Result<(), ApiError> {
    value
        .filter(|value| !value.is_empty())
        .map(|_| ())
        .ok_or_else(|| ApiError::new(ApiErrorType::ParamMissing, format!("Missing {name}")))
}

fn malformed(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, message)
}

fn validation(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Store {
        starts: Mutex<Vec<(Uuid, Uuid, PlatformOAuthStartRequest)>>,
        tokens: Mutex<Vec<PlatformOAuthTokenRequest>>,
    }

    impl PlatformOAuthStore for Store {
        fn create_redirect(
            &self,
            redirect_id: Uuid,
            cookie: Uuid,
            request: PlatformOAuthStartRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.starts
                .lock()
                .unwrap()
                .push((redirect_id, cookie, request));
            ready(Ok(()))
        }

        fn claim_redirect(
            &self,
            _request: PlatformOAuthClaimRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            ready(Ok(serde_json::json!({"grantToken": Uuid::nil()})))
        }

        fn grant_redirect(
            &self,
            _request: PlatformOAuthDecisionRequest,
            code: Uuid,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            ready(Ok(format!("https://client.test/cb?code={code}")))
        }

        fn deny_redirect(
            &self,
            _request: PlatformOAuthDecisionRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            ready(Ok("https://client.test/cb?error=access_denied".to_owned()))
        }

        fn exchange_token(
            &self,
            request: PlatformOAuthTokenRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.tokens.lock().unwrap().push(request);
            ready(Ok(serde_json::json!({"access_token": "pat_secret"})))
        }

        fn access_token_info(
            &self,
            _access_token: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            ready(Ok(serde_json::json!({"token_type": "Bearer"})))
        }

        fn revoke_token(
            &self,
            _token: String,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            ready(Ok(()))
        }
    }

    fn start_request() -> PlatformOAuthStartRequest {
        PlatformOAuthStartRequest {
            client_id: Uuid::nil(),
            redirect_uri: "https://client.test/cb".to_owned(),
            scopes: vec!["apps-read".to_owned()],
            state: "state".to_owned(),
            code_challenge: None,
            code_challenge_method: None,
        }
    }

    #[tokio::test]
    async fn service_validates_protocol_before_touching_the_store() {
        let service =
            PlatformOAuthService::new(Store::default(), "https://dash.test", true).unwrap();
        let started = service.start(start_request()).await.unwrap();
        assert!(
            started
                .dashboard_url
                .starts_with("https://dash.test/platform/oauth/start?redirect-id=")
        );
        assert!(started.secure_cookie);

        let mut invalid = start_request();
        invalid.scopes = vec!["root".to_owned()];
        assert!(service.start(invalid).await.is_err());
        assert_eq!(service.store.starts.lock().unwrap().len(), 1);

        let invalid_token = PlatformOAuthTokenRequest {
            grant_type: "refresh_token".to_owned(),
            client_id: Uuid::nil(),
            client_secret: None,
            redirect_uri: None,
            code: None,
            code_verifier: None,
            refresh_token: Some("prt_secret".to_owned()),
        };
        assert!(service.token(invalid_token).await.is_err());
        assert!(service.store.tokens.lock().unwrap().is_empty());
        assert!(
            service
                .token_info("prt_wrong-kind".to_owned())
                .await
                .is_err()
        );
        assert!(
            service
                .revoke("instantdb_invalid".to_owned())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn self_hosted_secure_cookie_setting_reaches_platform_oauth() {
        let config = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_DASHBOARD_URL", "https://dash.example.test"),
            ("INSTANT_SECURE_COOKIES", "true"),
        ])
        .unwrap();
        let service = PlatformOAuthService::from_config(Store::default(), &config).unwrap();

        assert!(service.start(start_request()).await.unwrap().secure_cookie);
        assert!(!config.environment().is_hosted());

        let localhost = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "http://localhost:8888"),
            ("INSTANT_SECURE_COOKIES", "true"),
        ])
        .unwrap();
        let service = PlatformOAuthService::from_config(Store::default(), &localhost).unwrap();
        assert!(!service.start(start_request()).await.unwrap().secure_cookie);
    }
}
