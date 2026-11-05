//! HTTP protocol, middleware, and route assembly for the Rust `InstantDB` server.

mod admin;
mod admin_auth;
mod admin_metadata;
mod admin_presence;
mod admin_realtime;
mod admin_storage;
mod body_limit;
mod client_ip;
mod dashboard;
mod dashboard_gateway;
mod error;
mod platform;
mod platform_oauth;
pub mod request_id;
mod router;
mod runtime_auth;
mod runtime_framework;
mod runtime_oauth;
mod runtime_openid;
mod runtime_realtime;
mod runtime_storage;
mod superadmin;

pub use admin::{
    AdminActor, AdminDataBackend, AdminQueryPermissionsRequest, AdminQueryRequest,
    AdminRequestContext, AdminTransactPermissionsRequest, AdminTransactRequest,
    AdminTransactResponse, admin_data_router,
};
pub use admin_auth::{
    AdminAuthBackend, AdminCreateTokenRequest, AdminMagicCodeRequest, AdminSignInGuestRequest,
    AdminSignOutRequest, AdminUser, AdminUserSelector, AdminVerifyMagicCodeRequest,
    admin_auth_router,
};
pub use admin_metadata::{
    AdminAttribute, AdminMetadataBackend, AdminSchema, AdminSchemaResponse, AttributeCatalogKind,
    SoftDeletedAttributesResponse, admin_metadata_router, attributes_to_admin_schema,
};
pub use admin_presence::{AdminPresenceBackend, AdminPresenceResponse, admin_presence_router};
pub use admin_realtime::{
    AdminRealtimeBackend, AdminSseOpenRequest, AdminSsePushRequest, AdminSseSession,
    admin_realtime_router,
};
pub use admin_storage::{AdminStorageBackend, admin_storage_router};
pub use client_ip::{ResolvedClientIp, resolve_client_ip};
pub use dashboard::{
    DASHBOARD_ROUTES, DashboardBackend, DashboardRequest, DashboardResponse, DashboardResponseBody,
    DashboardRoute, dashboard_router,
};
pub use dashboard_gateway::{DASHBOARD_GATEWAY_HEADER, DashboardGatewayToken};
pub use error::{ApiError, ApiErrorBody, ApiErrorType};
pub use platform::{
    PlatformBackend, WebhookPayloadRequest, WebhookPayloadResponse, demo_router, platform_router,
};
pub use platform_oauth::{
    PlatformOAuthBackend, PlatformOAuthClaimRequest, PlatformOAuthDecisionRequest,
    PlatformOAuthStartRequest, PlatformOAuthStartResponse, PlatformOAuthTokenRequest,
    platform_oauth_router,
};
pub use router::{
    AllBackends, DynamicHttpResponsePolicy, HealthState, HttpResponsePolicySnapshot,
    IMPLEMENTED_ROUTES, ROUTE_GROUPS, RouteGroup, ServerVersion, WalHealthFuture, WalHealthSource,
    allow_cors_origin, app_router, app_router_with_all_backends,
    app_router_with_all_backends_and_response_policy, app_router_with_backends,
    app_router_with_runtime_auth, apply_http_response_policy,
};
pub use runtime_auth::{
    RuntimeAuthBackend, RuntimeAuthSession, RuntimeUser, SendMagicCodeRequest, SignInGuestRequest,
    SignOutRequest, VerifyMagicCodeRequest, VerifyRefreshTokenRequest, runtime_auth_router,
};
pub use runtime_framework::{
    RuntimeFrameworkBackend, RuntimeFrameworkQueryTopics, RuntimeFrameworkRequest,
    RuntimeFrameworkResponse, runtime_framework_router,
};
pub use runtime_oauth::{
    OAuthCallbackRequest, OAuthCallbackResponse, OAuthIdTokenRequest, OAuthSessionResponse,
    OAuthStartRequest, OAuthStartResponse, OAuthTokenRequest, RuntimeOAuthBackend,
    runtime_oauth_router,
};
pub use runtime_openid::runtime_openid_router;
pub use runtime_realtime::{
    RuntimeDrainSignal, RuntimeRealtimeBackend, RuntimeRealtimeSession, RuntimeSsePushRequest,
    RuntimeTransport, runtime_realtime_router, runtime_websocket_exit_metrics,
};
pub use runtime_storage::{RuntimeStorageBackend, RuntimeStorageContext, runtime_storage_router};
pub use superadmin::{
    SuperadminBackend, SuperadminContext, SuperadminOperation, superadmin_router,
};
