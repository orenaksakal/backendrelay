use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::{
    AdminAuthBackend, AdminDataBackend, AdminMetadataBackend, AdminPresenceBackend,
    AdminRealtimeBackend, AdminStorageBackend, ApiError, ApiErrorType, DashboardBackend,
    PlatformBackend, PlatformOAuthBackend, RuntimeAuthBackend, RuntimeFrameworkBackend,
    RuntimeOAuthBackend, RuntimeRealtimeBackend, RuntimeStorageBackend, SuperadminBackend,
    admin_auth_router, admin_data_router, admin_metadata_router, admin_presence_router,
    admin_realtime_router, admin_storage_router, dashboard_router, demo_router,
    platform_oauth_router, platform_router, runtime_auth_router, runtime_framework_router,
    runtime_oauth_router, runtime_openid_router, runtime_realtime_router, runtime_storage_router,
    superadmin_router,
};

/// Audited route counts: migrated routes plus native Rust extensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteGroup {
    pub name: &'static str,
    pub routes: usize,
}

pub const ROUTE_GROUPS: &[RouteGroup] = &[
    RouteGroup {
        name: "dash",
        // Includes the public config probe, the workspace usage summary, the
        // operator flags API, the rules validation probe, the owner-level
        // backup restore routes, the server-side admin-token rotation route
        // and GET and PUT backup-schedule native extensions.
        routes: 134,
    },
    RouteGroup {
        name: "admin",
        routes: 24,
    },
    RouteGroup {
        name: "runtime",
        routes: 17,
    },
    RouteGroup {
        name: "superadmin",
        routes: 16,
    },
    RouteGroup {
        name: "oauth-apps",
        routes: 7,
    },
    RouteGroup {
        name: "storage",
        routes: 5,
    },
    RouteGroup {
        name: "core",
        routes: 3,
    },
    RouteGroup {
        name: "health",
        routes: 1,
    },
    RouteGroup {
        name: "demo",
        routes: 1,
    },
];

/// Audited Clojure routes currently implemented by the Rust router.
pub const IMPLEMENTED_ROUTES: &[(&str, &str)] = &[
    ("GET", "/"),
    ("GET", "/health/system"),
    ("GET", "/demo/csp"),
    ("GET", "/.well-known/webhooks/jwks.json"),
    ("GET", "/webhooks/payload/:app_id/:webhook_id/*"),
    ("GET", "/platform/oauth/start"),
    ("POST", "/platform/oauth/claim"),
    ("POST", "/platform/oauth/grant"),
    ("POST", "/platform/oauth/deny"),
    ("POST", "/platform/oauth/token"),
    ("GET", "/platform/oauth/token-info"),
    ("POST", "/platform/oauth/revoke"),
    ("GET", "/superadmin/apps"),
    ("GET", "/superadmin/orgs"),
    ("GET", "/superadmin/orgs/:org_id/apps"),
    ("POST", "/superadmin/apps"),
    ("GET", "/superadmin/apps/:app_id"),
    ("POST", "/superadmin/apps/:app_id"),
    ("POST", "/superadmin/apps/:app_id/transfers/send"),
    ("POST", "/superadmin/apps/:app_id/transfers/revoke"),
    ("GET", "/superadmin/apps/:app_id/schema"),
    ("POST", "/superadmin/apps/:app_id/schema/push/plan"),
    ("POST", "/superadmin/apps/:app_id/schema/push/apply"),
    ("GET", "/superadmin/apps/:app_id/perms"),
    ("POST", "/superadmin/apps/:app_id/perms"),
    ("DELETE", "/superadmin/apps/:app_id"),
    ("GET", "/superadmin/apps/:app_id/runtime"),
    ("POST", "/superadmin/apps/:app_id/explain"),
    ("POST", "/runtime/auth/send_magic_code"),
    ("POST", "/runtime/auth/verify_magic_code"),
    ("POST", "/runtime/auth/verify_refresh_token"),
    ("POST", "/runtime/auth/sign_in_guest"),
    ("POST", "/runtime/signout"),
    ("GET", "/runtime/oauth/start"),
    ("GET", "/runtime/:app_id/oauth/start"),
    ("GET", "/runtime/oauth/callback"),
    ("POST", "/runtime/oauth/callback"),
    ("POST", "/runtime/oauth/token"),
    ("POST", "/runtime/:app_id/oauth/token"),
    ("POST", "/runtime/oauth/id_token"),
    ("POST", "/runtime/framework/query"),
    ("GET", "/runtime/session"),
    ("GET", "/runtime/sse"),
    ("POST", "/runtime/sse"),
    ("POST", "/admin/query"),
    ("POST", "/admin/transact"),
    ("POST", "/admin/refresh_tokens"),
    ("POST", "/admin/sign_out"),
    ("POST", "/admin/magic_code"),
    ("POST", "/admin/send_magic_code"),
    ("POST", "/admin/verify_magic_code"),
    ("POST", "/admin/sign_in_guest"),
    ("GET", "/admin/users"),
    ("DELETE", "/admin/users"),
    ("GET", "/admin/schema"),
    ("GET", "/admin/soft_deleted_attrs"),
    ("POST", "/admin/query_perms_check"),
    ("POST", "/admin/transact_perms_check"),
    ("GET", "/admin/rooms/presence"),
    ("POST", "/admin/storage/signed-upload-url"),
    ("GET", "/admin/storage/signed-download-url"),
    ("PUT", "/admin/storage/upload"),
    ("GET", "/admin/storage/files"),
    ("DELETE", "/admin/storage/files"),
    ("POST", "/admin/storage/files/delete"),
    ("POST", "/admin/sse"),
    ("POST", "/admin/sse/push"),
    ("POST", "/admin/subscribe-query"),
    ("PUT", "/storage/upload"),
    ("DELETE", "/storage/files"),
    ("POST", "/storage/signed-upload-url"),
    ("PUT", "/storage/:upload-id/consume-upload-url"),
    ("GET", "/storage/signed-download-url"),
    ("GET", "/runtime/:app_id/.well-known/openid-configuration"),
];

/// Process health projected into the public `/health/system` contract.
pub type WalHealthFuture<'a> = Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;

/// Shared WAL-health source used by the fleet-wide readiness contract.
pub trait WalHealthSource: Send + Sync + 'static {
    fn wal_healthy(&self) -> WalHealthFuture<'_>;
}

#[derive(Clone)]
pub struct HealthState {
    wal_healthy: Arc<AtomicBool>,
    shared_wal_health: Option<Arc<dyn WalHealthSource>>,
    version: ServerVersion,
}

/// Build metadata served by `GET /health/version`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ServerVersion {
    pub version: String,
    pub git_sha: String,
    pub migration_version: u64,
}

impl ServerVersion {
    #[must_use]
    pub fn new(
        version: impl Into<String>,
        git_sha: impl Into<String>,
        migration_version: u64,
    ) -> Self {
        Self {
            version: version.into(),
            git_sha: git_sha.into(),
            migration_version,
        }
    }
}

impl Default for ServerVersion {
    fn default() -> Self {
        // Generic routers used outside the production assembly do not embed
        // migration metadata; the binary overrides this with its build values.
        Self::new(env!("CARGO_PKG_VERSION"), "unknown", 0)
    }
}

impl std::fmt::Debug for HealthState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HealthState")
            .field("wal_healthy", &self.wal_healthy())
            .field("has_shared_wal_health", &self.shared_wal_health.is_some())
            .field("version", &self.version)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponsePolicySnapshot {
    pub disable_preflight_caching: bool,
    pub cors_max_age: String,
    pub filtered_security_headers: Vec<String>,
}

impl Default for HttpResponsePolicySnapshot {
    fn default() -> Self {
        Self {
            disable_preflight_caching: false,
            cors_max_age: "600".to_owned(),
            filtered_security_headers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DynamicHttpResponsePolicy {
    inner: Arc<RwLock<HttpResponsePolicySnapshot>>,
}

impl DynamicHttpResponsePolicy {
    #[must_use]
    pub fn new(snapshot: HttpResponsePolicySnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> HttpResponsePolicySnapshot {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn replace(&self, snapshot: HttpResponsePolicySnapshot) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }
}

#[derive(Clone)]
struct HttpResponseMiddlewareState {
    dashboard_origin: String,
    policy: DynamicHttpResponsePolicy,
}

impl HealthState {
    #[must_use]
    pub fn new(wal_healthy: bool) -> Self {
        Self {
            wal_healthy: Arc::new(AtomicBool::new(wal_healthy)),
            shared_wal_health: None,
            version: ServerVersion::default(),
        }
    }

    #[must_use]
    pub fn with_shared_wal_health(mut self, source: Arc<dyn WalHealthSource>) -> Self {
        self.shared_wal_health = Some(source);
        self
    }

    #[must_use]
    pub fn with_version(mut self, version: ServerVersion) -> Self {
        self.version = version;
        self
    }

    pub fn set_wal_healthy(&self, healthy: bool) {
        self.wal_healthy.store(healthy, Ordering::Release);
    }

    #[must_use]
    pub fn wal_healthy(&self) -> bool {
        self.wal_healthy.load(Ordering::Acquire)
    }

    async fn effective_wal_healthy(&self) -> bool {
        if !self.wal_healthy() {
            return false;
        }
        match &self.shared_wal_health {
            Some(source) => source.wal_healthy().await.unwrap_or(false),
            None => true,
        }
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new(true)
    }
}

#[derive(Deserialize, Serialize)]
struct WalHealth {
    wal: &'static str,
}

/// Creates the common router with exact root, health, fallback, and security contracts.
pub fn app_router(state: HealthState) -> Router {
    apply_http_response_policy(base_router(state), "", DynamicHttpResponsePolicy::default())
}

/// Creates the common routes and runtime-auth group under the same middleware stack.
pub fn app_router_with_runtime_auth<B>(state: HealthState, backend: Arc<B>) -> Router
where
    B: RuntimeAuthBackend,
{
    apply_http_response_policy(
        base_router(state).merge(runtime_auth_router(backend)),
        "",
        DynamicHttpResponsePolicy::default(),
    )
}

/// Creates the common routes, runtime auth, and primary admin data routes.
pub fn app_router_with_backends<A, D>(
    state: HealthState,
    auth_backend: Arc<A>,
    data_backend: Arc<D>,
) -> Router
where
    A: RuntimeAuthBackend,
    D: AdminDataBackend,
{
    apply_http_response_policy(
        base_router(state)
            .merge(runtime_auth_router(auth_backend))
            .merge(admin_data_router(data_backend)),
        "",
        DynamicHttpResponsePolicy::default(),
    )
}

/// Backends for the currently ported public route groups.
pub struct AllBackends<R, O, W, D, A, M, P, S, E, T, F, G, I, J> {
    pub server_origin: String,
    pub dashboard_gateway: Option<crate::DashboardGatewayToken>,
    pub runtime_auth: Arc<R>,
    pub runtime_oauth: Arc<O>,
    pub runtime_realtime: Arc<W>,
    pub admin_data: Arc<D>,
    pub admin_auth: Arc<A>,
    pub admin_metadata: Arc<M>,
    pub admin_presence: Arc<P>,
    pub admin_storage: Arc<S>,
    pub admin_realtime: Arc<E>,
    pub runtime_storage: Arc<T>,
    pub platform: Arc<F>,
    pub platform_oauth: Arc<G>,
    pub superadmin: Arc<I>,
    pub dashboard: Arc<J>,
}

/// Creates the common routes with every currently ported backend group.
#[allow(clippy::type_complexity)]
pub fn app_router_with_all_backends<R, O, W, D, A, M, P, S, E, T, F, G, I, J>(
    state: HealthState,
    backends: AllBackends<R, O, W, D, A, M, P, S, E, T, F, G, I, J>,
) -> Router
where
    R: RuntimeAuthBackend,
    O: RuntimeOAuthBackend,
    W: RuntimeRealtimeBackend,
    D: AdminDataBackend + RuntimeFrameworkBackend,
    A: AdminAuthBackend,
    M: AdminMetadataBackend,
    P: AdminPresenceBackend,
    S: AdminStorageBackend,
    E: AdminRealtimeBackend,
    T: RuntimeStorageBackend,
    F: PlatformBackend,
    G: PlatformOAuthBackend,
    I: SuperadminBackend,
    J: DashboardBackend,
{
    app_router_with_all_backends_and_response_policy(
        state,
        backends,
        "",
        DynamicHttpResponsePolicy::default(),
    )
}

#[allow(clippy::type_complexity, clippy::too_many_lines)]
pub fn app_router_with_all_backends_and_response_policy<R, O, W, D, A, M, P, S, E, T, F, G, I, J>(
    state: HealthState,
    backends: AllBackends<R, O, W, D, A, M, P, S, E, T, F, G, I, J>,
    dashboard_origin: impl Into<String>,
    policy: DynamicHttpResponsePolicy,
) -> Router
where
    R: RuntimeAuthBackend,
    O: RuntimeOAuthBackend,
    W: RuntimeRealtimeBackend,
    D: AdminDataBackend + RuntimeFrameworkBackend,
    A: AdminAuthBackend,
    M: AdminMetadataBackend,
    P: AdminPresenceBackend,
    S: AdminStorageBackend,
    E: AdminRealtimeBackend,
    T: RuntimeStorageBackend,
    F: PlatformBackend,
    G: PlatformOAuthBackend,
    I: SuperadminBackend,
    J: DashboardBackend,
{
    let management = crate::dashboard_gateway::protect_dashboard_routes(
        platform_oauth_router(backends.platform_oauth)
            .merge(superadmin_router(backends.superadmin))
            .merge(dashboard_router(backends.dashboard)),
        backends.dashboard_gateway,
    );
    let router = base_router(state)
        .merge(runtime_auth_router(backends.runtime_auth))
        .merge(runtime_oauth_router(backends.runtime_oauth))
        .merge(runtime_realtime_router(backends.runtime_realtime))
        .merge(runtime_framework_router(Arc::clone(&backends.admin_data)))
        .merge(admin_data_router(backends.admin_data))
        .merge(admin_auth_router(backends.admin_auth))
        .merge(admin_metadata_router(backends.admin_metadata))
        .merge(admin_presence_router(backends.admin_presence))
        .merge(admin_storage_router(backends.admin_storage))
        .merge(admin_realtime_router(backends.admin_realtime))
        .merge(runtime_storage_router(backends.runtime_storage))
        .merge(runtime_openid_router(backends.server_origin))
        .merge(platform_router(backends.platform))
        .merge(management);
    apply_http_response_policy(
        with_request_timeout(router, REQUEST_TIMEOUT),
        dashboard_origin,
        policy,
    )
}

/// Overall request deadline for routes that do not stream or upgrade.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy)]
struct RequestTimeoutState {
    duration: std::time::Duration,
}

fn with_request_timeout(router: Router, duration: std::time::Duration) -> Router {
    router.layer(from_fn_with_state(
        RequestTimeoutState { duration },
        request_timeout,
    ))
}

async fn request_timeout(
    State(state): State<RequestTimeoutState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if is_streaming_route(request.uri().path()) {
        return next.run(request).await;
    }
    match tokio::time::timeout(state.duration, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            ApiErrorType::Timeout,
            format!(
                "Request timed out after {} seconds",
                state.duration.as_secs()
            ),
        )
        .with_status(StatusCode::REQUEST_TIMEOUT)
        .into_response(),
    }
}

fn is_streaming_route(path: &str) -> bool {
    matches!(
        path,
        "/runtime/session"
            | "/runtime/sse"
            | "/admin/sse"
            | "/admin/subscribe-query"
            | "/dash/session_counts"
            | "/dash/ws_playground"
            | "/dash/restores/zip"
            | "/admin/storage/upload"
    ) || path.starts_with("/storage/")
        || path.starts_with("/admin/storage/signed-")
        || (path.starts_with("/dash/apps/") && path.ends_with("/storage/upload"))
}

fn base_router(state: HealthState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health/system", get(health))
        .route("/health/version", get(health_version))
        .merge(demo_router())
        .fallback(not_found)
        .with_state(state)
}

/// Mirrors the two JVM OAuth CORS exceptions; all other routes allow CORS.
#[must_use]
pub fn allow_cors_origin(path: &str, origin: Option<&str>, dashboard_origin: &str) -> bool {
    match path {
        "/platform/oauth/start" | "/platform/oauth/grant" => false,
        "/platform/oauth/claim" => origin == Some(dashboard_origin),
        _ => true,
    }
}

async fn root() -> Html<&'static str> {
    Html("<code>Welcome to Instant's Backend!</code>")
}

async fn health(State(state): State<HealthState>) -> Response {
    let healthy = state.effective_wal_healthy().await;
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let wal = if healthy { "ok" } else { "error" };
    (status, Json(WalHealth { wal })).into_response()
}

async fn health_version(State(state): State<HealthState>) -> Json<ServerVersion> {
    Json(state.version)
}

async fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "message": "Oops! We couldn't match this route."
        })),
    )
}

pub fn apply_http_response_policy(
    router: Router,
    dashboard_origin: impl Into<String>,
    policy: DynamicHttpResponsePolicy,
) -> Router {
    router.layer(from_fn_with_state(
        HttpResponseMiddlewareState {
            dashboard_origin: dashboard_origin.into(),
            policy,
        },
        response_headers,
    ))
}

async fn response_headers(
    State(state): State<HttpResponseMiddlewareState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let csp_demo = request.uri().path() == "/demo/csp";
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let origin = request.headers().get(header::ORIGIN).cloned();
    let requested_method = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Method::from_bytes(value.as_bytes()).ok());
    let requested_headers = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned();
    let preflight = method == Method::OPTIONS;
    let cors_method = if preflight {
        requested_method.as_ref()
    } else {
        Some(&method)
    };
    let cors_allowed = origin.is_some()
        && allow_cors_origin(
            &path,
            origin.as_ref().and_then(|value| value.to_str().ok()),
            &state.dashboard_origin,
        )
        && cors_method.is_some_and(allowed_cors_method);
    let mut response = if preflight && cors_allowed {
        Response::new(Body::from("preflight complete"))
    } else {
        next.run(request).await
    };
    let policy = state.policy.snapshot();
    let headers = response.headers_mut();
    if cors_allowed {
        add_cors_headers(
            headers,
            origin.as_ref(),
            preflight.then_some(requested_headers.as_ref()).flatten(),
        );
        if preflight && !policy.disable_preflight_caching {
            headers.insert(
                header::VARY,
                HeaderValue::from_static("origin, Access-Control-Request-Headers"),
            );
            if let Ok(value) = HeaderValue::from_str(&policy.cors_max_age) {
                headers.insert(header::ACCESS_CONTROL_MAX_AGE, value);
            }
            if let Ok(value) =
                HeaderValue::from_str(&format!("public, max-age={}", policy.cors_max_age))
            {
                headers.insert(header::CACHE_CONTROL, value);
            }
        } else if preflight {
            headers.remove(header::VARY);
            headers.remove(header::ACCESS_CONTROL_MAX_AGE);
            headers.remove(header::CACHE_CONTROL);
        }
    }
    let content_security_policy = if csp_demo {
        "default-src 'none'; script-src 'self' 'sha256-yC+Lxzy8GEAXIWLTR7eqO1JY49aqduKqltQP1FCf0y0='; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"
    } else {
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"
    };
    for (name, canonical_name, value) in [
        (
            header::HeaderName::from_static("x-frame-options"),
            "X-Frame-Options",
            "DENY",
        ),
        (header::REFERRER_POLICY, "Referrer-Policy", "strict-origin"),
        (
            header::CONTENT_SECURITY_POLICY,
            "Content-Security-Policy",
            content_security_policy,
        ),
        (
            header::HeaderName::from_static("permissions-policy"),
            "Permissions-Policy",
            "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()",
        ),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            "X-Content-Type-Options",
            "nosniff",
        ),
    ] {
        if policy
            .filtered_security_headers
            .iter()
            .any(|filtered| filtered == canonical_name)
        {
            headers.remove(&name);
        } else {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
    response
}

fn allowed_cors_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::PUT | Method::POST | Method::DELETE
    )
}

fn add_cors_headers(
    headers: &mut HeaderMap,
    origin: Option<&HeaderValue>,
    requested_headers: Option<&HeaderValue>,
) {
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("DELETE, GET, POST, PUT"),
    );
    if let Some(requested) = requested_headers {
        let mut names = requested
            .to_str()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(canonical_header_name)
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        if let Ok(value) = HeaderValue::from_str(&names.join(", ")) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, value);
        }
    }
}

fn canonical_header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect()
            })
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        AdminCreateTokenRequest, AdminMagicCodeRequest, AdminMetadataBackend,
        AdminPresenceResponse, AdminQueryPermissionsRequest, AdminQueryRequest,
        AdminRealtimeBackend, AdminRequestContext, AdminSchemaResponse, AdminSignInGuestRequest,
        AdminSignOutRequest, AdminSseOpenRequest, AdminSsePushRequest, AdminSseSession,
        AdminStorageBackend, AdminTransactPermissionsRequest, AdminTransactRequest,
        AdminTransactResponse, AdminUser, AdminUserSelector, AdminVerifyMagicCodeRequest, ApiError,
        DASHBOARD_ROUTES, DashboardRequest, DashboardResponse, OAuthCallbackRequest,
        OAuthCallbackResponse, OAuthIdTokenRequest, OAuthSessionResponse, OAuthStartRequest,
        OAuthStartResponse, OAuthTokenRequest, PlatformOAuthClaimRequest,
        PlatformOAuthDecisionRequest, PlatformOAuthStartRequest, PlatformOAuthStartResponse,
        PlatformOAuthTokenRequest, RuntimeAuthSession, RuntimeFrameworkRequest,
        RuntimeFrameworkResponse, RuntimeRealtimeSession, RuntimeSsePushRequest,
        RuntimeStorageContext, RuntimeTransport, RuntimeUser, SendMagicCodeRequest,
        SignInGuestRequest, SignOutRequest, SoftDeletedAttributesResponse, SuperadminContext,
        SuperadminOperation, VerifyMagicCodeRequest, VerifyRefreshTokenRequest,
        WebhookPayloadRequest, WebhookPayloadResponse,
    };

    #[derive(Default)]
    struct ProbeBackend {
        calls: AtomicUsize,
    }

    impl ProbeBackend {
        fn reject<T: Send>(&self) -> impl Future<Output = Result<T, ApiError>> + Send {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Err(ApiError::new(
                crate::ApiErrorType::Unknown,
                "route probe backend must not perform provider work",
            )))
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl RuntimeAuthBackend for ProbeBackend {
        fn send_magic_code(
            &self,
            _request: SendMagicCodeRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }

        fn verify_magic_code(
            &self,
            _request: VerifyMagicCodeRequest,
        ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
            self.reject()
        }

        fn verify_refresh_token(
            &self,
            _request: VerifyRefreshTokenRequest,
        ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
            self.reject()
        }

        fn sign_in_guest(
            &self,
            _request: SignInGuestRequest,
        ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
            self.reject()
        }

        fn sign_out(
            &self,
            _request: SignOutRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }
    }

    impl RuntimeOAuthBackend for ProbeBackend {
        fn start(
            &self,
            _request: OAuthStartRequest,
        ) -> impl Future<Output = Result<OAuthStartResponse, ApiError>> + Send {
            self.reject()
        }

        fn callback(
            &self,
            _request: OAuthCallbackRequest,
        ) -> impl Future<Output = Result<OAuthCallbackResponse, ApiError>> + Send {
            self.reject()
        }

        fn exchange_code(
            &self,
            _request: OAuthTokenRequest,
        ) -> impl Future<Output = Result<OAuthSessionResponse, ApiError>> + Send {
            self.reject()
        }

        fn exchange_id_token(
            &self,
            _request: OAuthIdTokenRequest,
        ) -> impl Future<Output = Result<OAuthSessionResponse, ApiError>> + Send {
            self.reject()
        }
    }

    impl RuntimeRealtimeBackend for ProbeBackend {
        fn open(
            &self,
            _app_id: Option<uuid::Uuid>,
            _transport: RuntimeTransport,
        ) -> impl Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
            self.reject()
        }

        fn message(
            &self,
            _session_id: uuid::Uuid,
            _message: serde_json::Value,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }

        fn push(
            &self,
            _request: RuntimeSsePushRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }
    }

    impl AdminDataBackend for ProbeBackend {
        type PreparedQueryPermissions = ();

        fn query(
            &self,
            _context: AdminRequestContext,
            _request: AdminQueryRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn transact(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactRequest,
        ) -> impl Future<Output = Result<AdminTransactResponse, ApiError>> + Send {
            self.reject()
        }

        fn prepare_query_permissions(
            &self,
            _context: AdminRequestContext,
            _rules_override: Option<serde_json::Value>,
        ) -> impl Future<Output = Result<Self::PreparedQueryPermissions, ApiError>> + Send {
            self.reject()
        }

        fn query_permissions(
            &self,
            _prepared: Self::PreparedQueryPermissions,
            _request: AdminQueryPermissionsRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn transact_permissions(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactPermissionsRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }
    }

    impl RuntimeFrameworkBackend for ProbeBackend {
        fn query(
            &self,
            _request: RuntimeFrameworkRequest,
        ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send {
            self.reject()
        }
    }

    impl AdminAuthBackend for ProbeBackend {
        fn get_user(
            &self,
            _context: AdminRequestContext,
            _selector: AdminUserSelector,
        ) -> impl Future<Output = Result<Option<AdminUser>, ApiError>> + Send {
            self.reject()
        }

        fn delete_user(
            &self,
            _context: AdminRequestContext,
            _selector: AdminUserSelector,
        ) -> impl Future<Output = Result<Option<AdminUser>, ApiError>> + Send {
            self.reject()
        }

        fn create_token(
            &self,
            _context: AdminRequestContext,
            _request: AdminCreateTokenRequest,
        ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
            self.reject()
        }

        fn sign_out(
            &self,
            _context: AdminRequestContext,
            _request: AdminSignOutRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }

        fn generate_magic_code(
            &self,
            _context: AdminRequestContext,
            _request: AdminMagicCodeRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.reject()
        }

        fn send_magic_code(
            &self,
            _context: AdminRequestContext,
            _request: AdminMagicCodeRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.reject()
        }

        fn verify_magic_code(
            &self,
            _context: AdminRequestContext,
            _request: AdminVerifyMagicCodeRequest,
        ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
            self.reject()
        }

        fn sign_in_guest(
            &self,
            _context: AdminRequestContext,
            _request: AdminSignInGuestRequest,
        ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
            self.reject()
        }
    }

    impl AdminMetadataBackend for ProbeBackend {
        fn schema(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<AdminSchemaResponse, ApiError>> + Send {
            self.reject()
        }

        fn soft_deleted_attributes(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<SoftDeletedAttributesResponse, ApiError>> + Send {
            self.reject()
        }
    }

    impl AdminPresenceBackend for ProbeBackend {
        fn presence(
            &self,
            _context: AdminRequestContext,
            _room_type: String,
            _room_id: String,
        ) -> impl Future<Output = Result<AdminPresenceResponse, ApiError>> + Send {
            self.reject()
        }
    }

    impl AdminStorageBackend for ProbeBackend {
        fn upload(
            &self,
            _context: AdminRequestContext,
            _path: String,
            _content_type: Option<String>,
            _content_disposition: Option<String>,
            _content_length: Option<u64>,
            _body: Body,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn delete_file(
            &self,
            _context: AdminRequestContext,
            _path: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn delete_files(
            &self,
            _context: AdminRequestContext,
            _paths: Vec<String>,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn list_files(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn create_upload_url(
            &self,
            _context: AdminRequestContext,
            _path: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn create_download_url(
            &self,
            _context: AdminRequestContext,
            _path: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }
    }

    impl AdminRealtimeBackend for ProbeBackend {
        fn open(
            &self,
            _context: AdminRequestContext,
            _request: AdminSseOpenRequest,
        ) -> impl Future<Output = Result<AdminSseSession, ApiError>> + Send {
            self.reject()
        }

        fn push(
            &self,
            _request: AdminSsePushRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }
    }

    impl RuntimeStorageBackend for ProbeBackend {
        fn upload(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
            _content_type: Option<String>,
            _content_disposition: Option<String>,
            _content_length: Option<u64>,
            _body: Body,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn delete_file(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn create_upload_url(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn consume_upload_url(
            &self,
            _upload_id: uuid::Uuid,
            _content_type: String,
            _content_length: Option<u64>,
            _body: Body,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn create_download_url(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }
    }

    impl PlatformBackend for ProbeBackend {
        fn webhook_jwks(&self) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn webhook_payload(
            &self,
            _request: WebhookPayloadRequest,
        ) -> impl Future<Output = Result<WebhookPayloadResponse, ApiError>> + Send {
            self.reject()
        }
    }

    impl PlatformOAuthBackend for ProbeBackend {
        fn start(
            &self,
            _request: PlatformOAuthStartRequest,
        ) -> impl Future<Output = Result<PlatformOAuthStartResponse, ApiError>> + Send {
            self.reject()
        }

        fn claim(
            &self,
            _request: PlatformOAuthClaimRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn grant(
            &self,
            _request: PlatformOAuthDecisionRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.reject()
        }

        fn deny(
            &self,
            _request: PlatformOAuthDecisionRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.reject()
        }

        fn token(
            &self,
            _request: PlatformOAuthTokenRequest,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn token_info(
            &self,
            _access_token: String,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }

        fn revoke(&self, _token: String) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.reject()
        }
    }

    impl SuperadminBackend for ProbeBackend {
        fn execute(
            &self,
            _context: SuperadminContext,
            _operation: SuperadminOperation,
        ) -> impl Future<Output = Result<serde_json::Value, ApiError>> + Send {
            self.reject()
        }
    }

    impl DashboardBackend for ProbeBackend {
        fn handle(
            &self,
            _request: DashboardRequest,
        ) -> impl Future<Output = Result<DashboardResponse, ApiError>> + Send {
            self.reject()
        }
    }

    struct TestWalHealth(Result<bool, String>);

    impl WalHealthSource for TestWalHealth {
        fn wal_healthy(&self) -> WalHealthFuture<'_> {
            Box::pin(std::future::ready(self.0.clone()))
        }
    }

    #[test]
    fn inventory_accounts_for_every_authoritative_route() {
        assert_eq!(
            ROUTE_GROUPS.iter().map(|group| group.routes).sum::<usize>(),
            208
        );
        assert_eq!(IMPLEMENTED_ROUTES.len(), 74);
        assert_eq!(
            IMPLEMENTED_ROUTES.len() + crate::DASHBOARD_ROUTES.len(),
            208
        );
        let unique = IMPLEMENTED_ROUTES
            .iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), IMPLEMENTED_ROUTES.len());
    }

    fn concrete_path(source_path: &str) -> String {
        source_path
            .split('/')
            .map(|part| {
                if part.starts_with(':') {
                    "00000000-0000-0000-0000-000000000001"
                } else if part == "*" {
                    "nested/path"
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn probe_dispatches(method: &str, source_path: &str) -> bool {
        // These declarations are intentionally public/backend-owned. Every
        // other non-WebSocket declaration must reject missing auth or malformed
        // input before any provider boundary is reached.
        matches!(
            (method, source_path),
            (
                "GET",
                "/.well-known/webhooks/jwks.json" | "/runtime/oauth/callback"
            ) | ("PUT", "/storage/:upload-id/consume-upload-url")
        ) || (source_path == "/dash" || source_path.starts_with("/dash/"))
            && !matches!(
                source_path,
                "/dash/session_counts" | "/dash/ws_playground" | "/dash/restores/zip"
            )
    }

    fn probe_is_public_local(source_path: &str) -> bool {
        matches!(
            source_path,
            "/" | "/health/system"
                | "/demo/csp"
                | "/runtime/:app_id/.well-known/openid-configuration"
        )
    }

    fn probe_router(
        backend: &Arc<ProbeBackend>,
        dashboard_gateway: Option<crate::DashboardGatewayToken>,
    ) -> Router {
        app_router_with_all_backends(
            HealthState::default(),
            AllBackends {
                server_origin: "https://api.example.test".to_owned(),
                dashboard_gateway,
                runtime_auth: Arc::clone(&backend),
                runtime_oauth: Arc::clone(&backend),
                runtime_realtime: Arc::clone(&backend),
                admin_data: Arc::clone(&backend),
                admin_auth: Arc::clone(&backend),
                admin_metadata: Arc::clone(&backend),
                admin_presence: Arc::clone(&backend),
                admin_storage: Arc::clone(&backend),
                admin_realtime: Arc::clone(&backend),
                runtime_storage: Arc::clone(&backend),
                platform: Arc::clone(&backend),
                platform_oauth: Arc::clone(&backend),
                superadmin: Arc::clone(&backend),
                dashboard: Arc::clone(&backend),
            },
        )
    }

    #[tokio::test]
    async fn private_gateway_covers_every_management_declaration_without_changing_public_routes() {
        let backend = Arc::new(ProbeBackend::default());
        let guarded = probe_router(
            &backend,
            crate::DashboardGatewayToken::new("unit-test-only-origin-gateway-secret".to_owned()),
        );
        let unguarded = probe_router(&backend, None);
        for (method, path) in IMPLEMENTED_ROUTES.iter().copied().chain(
            DASHBOARD_ROUTES
                .iter()
                .map(|route| (route.method, route.source_path)),
        ) {
            let calls_before = backend.calls();
            let request = || {
                Request::builder()
                    .method(method)
                    .uri(concrete_path(path))
                    .body(Body::empty())
                    .unwrap()
            };
            let response = guarded.clone().oneshot(request()).await.unwrap();
            let management = path == "/dash"
                || path.starts_with("/dash/")
                || path.starts_with("/superadmin/")
                || path.starts_with("/platform/oauth/");
            if management {
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "unprotected {method} {path}"
                );
                assert_eq!(
                    backend.calls(),
                    calls_before,
                    "dispatched unprotected {method} {path}"
                );
            } else {
                assert_eq!(
                    response.status(),
                    unguarded.clone().oneshot(request()).await.unwrap().status(),
                    "public route changed: {method} {path}"
                );
            }
        }
        for path in ["/%64ash/me", "/dash%2Fme", "/dashboard/me", "/dash/unknown"] {
            assert_eq!(
                guarded
                    .clone()
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn every_authoritative_declaration_has_a_method_and_fail_closed_boundary() {
        let backend = Arc::new(ProbeBackend::default());
        let router = probe_router(&backend, None);
        let declarations = IMPLEMENTED_ROUTES.iter().copied().chain(
            DASHBOARD_ROUTES
                .iter()
                .map(|route| (route.method, route.source_path)),
        );
        let mut covered = 0;

        for (method, source_path) in declarations {
            let path = concrete_path(source_path);
            let calls_before = backend.calls();
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(&path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                !matches!(
                    response.status(),
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "declared route is not mounted: {method} {source_path}"
            );
            if !probe_is_public_local(source_path) && !probe_dispatches(method, source_path) {
                assert!(
                    response.status().is_client_error(),
                    "malformed or unauthenticated probe did not fail closed: {method} {source_path}"
                );
            }
            assert_eq!(
                router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(Method::PATCH)
                            .uri(&path)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "declared route accepts an undeclared method: {method} {source_path}"
            );
            let expected_calls = calls_before + usize::from(probe_dispatches(method, source_path));
            assert_eq!(
                backend.calls(),
                expected_calls,
                "missing auth or malformed input crossed a backend boundary: {method} {source_path}"
            );
            covered += 1;
        }

        assert_eq!(covered, 208);
        assert_eq!(backend.calls(), 134);
    }

    #[test]
    fn cors_exceptions_match_the_jvm() {
        assert!(!allow_cors_origin(
            "/platform/oauth/start",
            None,
            "https://dash"
        ));
        assert!(!allow_cors_origin(
            "/platform/oauth/claim",
            Some("https://other"),
            "https://dash"
        ));
        assert!(allow_cors_origin(
            "/platform/oauth/claim",
            Some("https://dash"),
            "https://dash"
        ));
        assert!(allow_cors_origin("/runtime/session", None, "https://dash"));
    }

    #[tokio::test]
    async fn health_fallback_and_security_headers_match_contract() {
        let state = HealthState::new(false);
        let router = app_router(state.clone());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"wal":"error"}"#
        );

        state.set_wal_healthy(true);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"message":"Oops! We couldn't match this route."}"#
        );
    }

    #[tokio::test]
    async fn health_version_reports_build_metadata_without_authentication() {
        let state = HealthState::new(false).with_version(ServerVersion::new(
            "v2026.11.1",
            "0123456789ab",
            151,
        ));
        let response = app_router(state)
            .oneshot(
                Request::builder()
                    .uri("/health/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"version":"v2026.11.1","git_sha":"0123456789ab","migration_version":151}"#
        );
    }

    #[tokio::test]
    async fn shared_wal_health_is_fail_closed_and_preserves_exact_envelope() {
        for (shared, expected_status, expected_body) in [
            (Ok(true), StatusCode::OK, r#"{"wal":"ok"}"#),
            (
                Ok(false),
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"wal":"error"}"#,
            ),
            (
                Err("database unavailable".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"wal":"error"}"#,
            ),
        ] {
            let state =
                HealthState::new(true).with_shared_wal_health(Arc::new(TestWalHealth(shared)));
            let response = app_router(state)
                .oneshot(
                    Request::builder()
                        .uri("/health/system")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                expected_body
            );
        }
    }

    #[tokio::test]
    async fn csp_demo_whitelists_only_its_declared_inline_script() {
        let response = app_router(HealthState::default())
            .oneshot(Request::get("/demo/csp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'; script-src 'self' 'sha256-yC+Lxzy8GEAXIWLTR7eqO1JY49aqduKqltQP1FCf0y0='; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.windows(b"box-a".len()).any(|part| part == b"box-a"));
    }

    #[tokio::test]
    async fn preflight_cors_and_live_response_flags_match_the_jvm() {
        let policy = DynamicHttpResponsePolicy::new(HttpResponsePolicySnapshot {
            cors_max_age: "321".to_owned(),
            filtered_security_headers: vec!["X-Frame-Options".to_owned()],
            ..HttpResponsePolicySnapshot::default()
        });
        let router = apply_http_response_policy(
            base_router(HealthState::default()),
            "https://dash.example",
            policy.clone(),
        );
        let preflight = || {
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/runtime/session")
                .header(header::ORIGIN, "https://app.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "x-instant-app-id, content-type",
                )
                .body(Body::empty())
                .unwrap()
        };
        let response = router.clone().oneshot(preflight()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example"
        );
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
            "DELETE, GET, POST, PUT"
        );
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
            "Content-Type, X-Instant-App-Id"
        );
        assert_eq!(response.headers()[header::ACCESS_CONTROL_MAX_AGE], "321");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=321"
        );
        assert!(!response.headers().contains_key("x-frame-options"));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "preflight complete"
        );

        policy.replace(HttpResponsePolicySnapshot {
            disable_preflight_caching: true,
            ..HttpResponsePolicySnapshot::default()
        });
        let response = router.clone().oneshot(preflight()).await.unwrap();
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_MAX_AGE)
        );
        assert!(!response.headers().contains_key(header::CACHE_CONTROL));
        assert_eq!(response.headers()["x-frame-options"], "DENY");

        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/platform/oauth/claim")
                    .header(header::ORIGIN, "https://other.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
    }

    async fn slow_handler() -> &'static str {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        "late"
    }

    #[tokio::test]
    async fn request_timeout_returns_a_json_408_for_slow_handlers() {
        let router = with_request_timeout(
            Router::new().route("/slow", get(slow_handler)),
            std::time::Duration::from_millis(20),
        );

        let response = router
            .oneshot(Request::get("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "timeout");
        assert_eq!(body["message"], "Request timed out after 0 seconds");
    }

    #[tokio::test]
    async fn streaming_routes_are_not_subject_to_the_request_timeout() {
        let router = with_request_timeout(
            Router::new().route("/runtime/sse", get(slow_handler)),
            std::time::Duration::from_millis(20),
        );

        let response = router
            .oneshot(Request::get("/runtime/sse").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), "late");
    }
}
