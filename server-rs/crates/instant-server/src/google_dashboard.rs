use std::collections::BTreeSet;

use instant_auth::oauth::{IdTokenOptions, fetch_jwks, verify_id_token};
use instant_http::{ApiError, ApiErrorType};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::{DashboardControlHookOperation, DashboardControlHooks, ServerConfig};

const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_JWKS_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/certs";
const GOOGLE_ISSUER: &str = "https://accounts.google.com";
const DEVELOPMENT_DASHBOARD_ORIGIN: &str = "http://localhost:3000";

/// Composable dashboard Google OAuth provider boundary.
///
/// The surrounding dashboard service owns state/cookie/code persistence. This
/// adapter owns provider configuration, code exchange, and signed ID-token
/// verification, then delegates every unrelated hook to `inner`.
pub struct GoogleDashboardControlHooks<D> {
    inner: D,
    http: reqwest::Client,
    credentials: Option<GoogleCredentials>,
    callback_url: String,
    dashboard_origin: String,
    secure_cookie: bool,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_endpoint: String,
}

struct GoogleCredentials {
    client_id: String,
    client_secret: String,
}

impl<D> GoogleDashboardControlHooks<D> {
    #[must_use]
    pub fn from_config(config: &ServerConfig, inner: D) -> Self {
        let credentials = config
            .google_oauth_client()
            .map(|(client_id, client_secret)| GoogleCredentials {
                client_id: client_id.to_owned(),
                client_secret: client_secret.to_owned(),
            });
        Self {
            inner,
            http: reqwest::Client::new(),
            credentials,
            callback_url: format!("{}/dash/oauth/callback", config.server_origin()),
            dashboard_origin: config.dashboard_origin().to_owned(),
            secure_cookie: config.secure_cookies(),
            authorization_endpoint: GOOGLE_AUTHORIZATION_ENDPOINT.to_owned(),
            token_endpoint: GOOGLE_TOKEN_ENDPOINT.to_owned(),
            jwks_endpoint: GOOGLE_JWKS_ENDPOINT.to_owned(),
        }
    }

    #[must_use]
    pub fn with_provider_endpoints(
        mut self,
        token_endpoint: impl Into<String>,
        jwks_endpoint: impl Into<String>,
    ) -> Self {
        self.token_endpoint = token_endpoint.into();
        self.jwks_endpoint = jwks_endpoint.into();
        self
    }

    fn configuration(&self) -> Result<Value, ApiError> {
        let credentials = self.credentials.as_ref().ok_or_else(oauth_unavailable)?;
        Ok(json!({
            "authorization_endpoint": self.authorization_endpoint,
            "client_id": credentials.client_id,
            "callback_url": self.callback_url,
            "dashboard_origin": self.dashboard_origin,
            "dev_dashboard_origin": DEVELOPMENT_DASHBOARD_ORIGIN,
            "secure_cookie": self.secure_cookie,
        }))
    }

    async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Value {
        let Some(credentials) = self.credentials.as_ref() else {
            return json!({"error": "Dashboard Google OAuth is not configured"});
        };
        let Ok(response) = self
            .http
            .post(&self.token_endpoint)
            .form(&[
                ("client_id", credentials.client_id.as_str()),
                ("client_secret", credentials.client_secret.as_str()),
                ("code", code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
        else {
            return json!({"error": "Google token exchange failed"});
        };
        let success = response.status().is_success();
        let Ok(body) = response.json::<GoogleTokenResponse>().await else {
            return json!({"error": "Google returned a malformed token response"});
        };
        if !success {
            return json!({
                "error": body.error_description.unwrap_or_else(|| "Unknown error".to_owned())
            });
        }
        let Some(id_token) = body.id_token else {
            return json!({"error": "Google did not return an ID token"});
        };
        let Ok(jwks_url) = Url::parse(&self.jwks_endpoint) else {
            return json!({"error": "Google JWKS endpoint is invalid"});
        };
        let Ok(jwks) = fetch_jwks(&jwks_url).await else {
            return json!({"error": "Google signing keys are unavailable"});
        };
        let algorithms = BTreeSet::from(["RS256".to_owned()]);
        let accepted_audiences = [credentials.client_id.clone()];
        match verify_id_token(
            &id_token,
            &jwks,
            &IdTokenOptions {
                issuer: GOOGLE_ISSUER,
                accepted_audiences: &accepted_audiences,
                supported_algorithms: &algorithms,
                nonce: None,
                allow_unverified_email: false,
            },
        ) {
            Ok(user) => json!({
                "subject": user.subject,
                "email": user.email,
                "email_verified": user.email.is_some(),
            }),
            Err(_) => json!({"error": "Google ID token verification failed"}),
        }
    }
}

impl<D> DashboardControlHooks for GoogleDashboardControlHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::DashboardOAuthConfiguration { .. } => {
                self.configuration()
            }
            DashboardControlHookOperation::DashboardOAuthExchangeCode { code, redirect_uri } => {
                Ok(self.exchange_code(&code, &redirect_uri).await)
            }
            other => self.inner.execute(other).await,
        }
    }
}

#[derive(Deserialize)]
struct GoogleTokenResponse {
    id_token: Option<String>,
    error_description: Option<String>,
}

fn oauth_unavailable() -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        "Dashboard Google OAuth is not configured",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopDashboardControlHooks;

    #[tokio::test]
    async fn configuration_uses_validated_server_values_and_delegates_other_hooks() {
        let config = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_DASHBOARD_URL", "https://dash.example.test"),
            ("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID", "client-id"),
            (
                "INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET",
                "client-secret",
            ),
        ])
        .unwrap();
        let hooks = GoogleDashboardControlHooks::from_config(&config, NoopDashboardControlHooks);
        let value = hooks
            .execute(DashboardControlHookOperation::DashboardOAuthConfiguration {
                redirect_to_dev: false,
            })
            .await
            .unwrap();
        assert_eq!(value["client_id"], "client-id");
        assert_eq!(
            value["callback_url"],
            "https://api.example.test/dash/oauth/callback"
        );
        assert_eq!(value["dashboard_origin"], "https://dash.example.test");
        assert_eq!(value["secure_cookie"], false);
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeBackup {
                    app_id: uuid::Uuid::nil(),
                    creator_id: None,
                    work_estimate: 0,
                    client_ip: None,
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
    }

    #[tokio::test]
    async fn missing_credentials_fail_closed_without_network_io() {
        let config = ServerConfig::from_pairs(Vec::<(String, String)>::new()).unwrap();
        let hooks = GoogleDashboardControlHooks::from_config(&config, NoopDashboardControlHooks);
        let error = hooks
            .execute(DashboardControlHookOperation::DashboardOAuthConfiguration {
                redirect_to_dev: false,
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.body().message,
            "Dashboard Google OAuth is not configured"
        );
    }

    #[tokio::test]
    async fn self_hosted_secure_cookie_setting_reaches_dashboard_oauth() {
        let config = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_SECURE_COOKIES", "true"),
            ("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID", "client-id"),
            (
                "INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET",
                "client-secret",
            ),
        ])
        .unwrap();
        let hooks = GoogleDashboardControlHooks::from_config(&config, NoopDashboardControlHooks);
        let value = hooks
            .execute(DashboardControlHookOperation::DashboardOAuthConfiguration {
                redirect_to_dev: false,
            })
            .await
            .unwrap();

        assert_eq!(config.environment(), crate::ServerEnvironment::Development);
        assert_eq!(value["secure_cookie"], true);
    }
}
