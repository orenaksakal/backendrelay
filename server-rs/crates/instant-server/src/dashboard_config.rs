use std::future::{Future, ready};

use instant_http::{ApiError, ApiErrorType, DashboardRequest, DashboardResponse};
use serde_json::json;

use crate::{
    DashboardControlHooks, DashboardSubsystem, PostgresDashboardPolicyHooks, ServerConfig,
};

pub const DASHBOARD_CONFIG_PATH: &str = "/dash/config";

/// Release builds can stamp `INSTANT_BUILD_VERSION`; local builds report `dev`.
pub const DASHBOARD_BUILD_VERSION: &str = match option_env!("INSTANT_BUILD_VERSION") {
    Some(version) => version,
    None => "dev",
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DashboardSignupMode {
    Open,
    Restricted,
    Closed,
}

impl DashboardSignupMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Restricted => "restricted",
            Self::Closed => "closed",
        }
    }

    #[must_use]
    pub fn from_config(config: &ServerConfig) -> Self {
        if config.dashboard_allowed_emails().is_some() {
            Self::Restricted
        } else if config.dashboard_signup_open() {
            Self::Open
        } else {
            Self::Closed
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DashboardPublicConfigValues {
    pub signup: DashboardSignupMode,
    pub ephemeral_apps: bool,
}

pub trait DashboardPublicConfigSource: Send + Sync + 'static {
    fn read(&self) -> impl Future<Output = Result<DashboardPublicConfigValues, ApiError>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticDashboardPublicConfig(pub DashboardPublicConfigValues);

impl DashboardPublicConfigSource for StaticDashboardPublicConfig {
    fn read(&self) -> impl Future<Output = Result<DashboardPublicConfigValues, ApiError>> + Send {
        ready(Ok(self.0))
    }
}

impl<D> DashboardPublicConfigSource for PostgresDashboardPolicyHooks<D>
where
    D: DashboardControlHooks,
{
    async fn read(&self) -> Result<DashboardPublicConfigValues, ApiError> {
        self.public_config_values().await
    }
}

/// Serves the unauthenticated deployment metadata the dashboard needs before
/// sign-in. It deliberately contains no user, app or secret material.
pub struct DashboardConfigService<S> {
    source: S,
    google_login: bool,
    private: bool,
}

impl<S> DashboardConfigService<S> {
    #[must_use]
    pub const fn new(source: S, google_login: bool, private: bool) -> Self {
        Self {
            source,
            google_login,
            private,
        }
    }

    #[must_use]
    pub fn from_config(source: S, config: &ServerConfig, private: bool) -> Self {
        Self::new(source, config.google_oauth_client().is_some(), private)
    }
}

#[must_use]
pub fn is_dashboard_config_route(method: &str, path: &str) -> bool {
    method == "GET" && path == DASHBOARD_CONFIG_PATH
}

impl<S> DashboardSubsystem for DashboardConfigService<S>
where
    S: DashboardPublicConfigSource,
{
    fn matches(&self, request: &DashboardRequest) -> bool {
        is_dashboard_config_route(request.method.as_str(), &request.path)
    }

    async fn handle(&self, _request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
        let values = self.source.read().await?;
        let body = serde_json::to_vec(&json!({
            "version": DASHBOARD_BUILD_VERSION,
            "googleLogin": self.google_login,
            "signup": values.signup.as_str(),
            "ephemeralApps": values.ephemeral_apps,
            "private": self.private,
        }))
        .map_err(|_| ApiError::new(ApiErrorType::Unknown, "Could not encode dashboard config"))?;
        let mut response = DashboardResponse::json(body);
        response.headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, header};

    use super::*;

    fn request() -> DashboardRequest {
        DashboardRequest {
            method: Method::GET,
            path: DASHBOARD_CONFIG_PATH.to_owned(),
            query: None,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            staged_body_path: None,
            remote_addr: None,
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn body(response: DashboardResponse) -> serde_json::Value {
        serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
    }

    #[test]
    fn signup_mode_follows_the_allowlist_and_signup_switch() {
        let closed = ServerConfig::from_pairs(Vec::<(String, String)>::new()).unwrap();
        assert_eq!(
            DashboardSignupMode::from_config(&closed),
            DashboardSignupMode::Closed
        );

        let open = ServerConfig::from_pairs([("INSTANT_DASHBOARD_SIGNUP", "open")]).unwrap();
        assert_eq!(
            DashboardSignupMode::from_config(&open),
            DashboardSignupMode::Open
        );

        let restricted = ServerConfig::from_pairs([
            ("INSTANT_DASHBOARD_SIGNUP", "open"),
            ("INSTANT_DASHBOARD_ALLOWED_EMAILS", "one@example.test"),
        ])
        .unwrap();
        assert_eq!(
            DashboardSignupMode::from_config(&restricted),
            DashboardSignupMode::Restricted
        );
    }

    #[tokio::test]
    async fn config_response_is_public_and_never_cached() {
        let service = DashboardConfigService::new(
            StaticDashboardPublicConfig(DashboardPublicConfigValues {
                signup: DashboardSignupMode::Restricted,
                ephemeral_apps: true,
            }),
            true,
            true,
        );
        assert!(service.matches(&request()));

        let response = service.handle(request()).await.unwrap();
        assert_eq!(
            response.headers[header::CACHE_CONTROL],
            axum::http::HeaderValue::from_static("no-store")
        );
        let body = body(response);
        assert_eq!(body["version"], DASHBOARD_BUILD_VERSION);
        assert_eq!(body["googleLogin"], true);
        assert_eq!(body["signup"], "restricted");
        assert_eq!(body["ephemeralApps"], true);
        assert_eq!(body["private"], true);
        let mut keys = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "ephemeralApps",
                "googleLogin",
                "private",
                "signup",
                "version"
            ]
        );
    }
}
