use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, get, on};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

const RESTORE_UPLOAD_LIMIT: u64 = 1024 * 1024 * 1024;

use crate::{ApiError, ApiErrorType, body_limit};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DashboardRoute {
    pub method: &'static str,
    pub source_path: &'static str,
    pub axum_path: &'static str,
}

/// Exact dashboard declarations extracted from the authoritative Clojure defroutes form.
pub const DASHBOARD_ROUTES: &[DashboardRoute] = &[
    DashboardRoute {
        method: "POST",
        source_path: "/dash/auth/send_magic_code",
        axum_path: "/dash/auth/send_magic_code",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/auth/verify_magic_code",
        axum_path: "/dash/auth/verify_magic_code",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/config",
        axum_path: "/dash/config",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/admin",
        axum_path: "/dash/admin",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/check-admin",
        axum_path: "/dash/check-admin",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/operator/flags",
        axum_path: "/dash/operator/flags",
    },
    DashboardRoute {
        method: "PUT",
        source_path: "/dash/operator/flags/:name",
        axum_path: "/dash/operator/flags/{name}",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/operator/flags/:name",
        axum_path: "/dash/operator/flags/{name}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/top",
        axum_path: "/dash/top",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/storage",
        axum_path: "/dash/storage",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/overview/daily",
        axum_path: "/dash/overview/daily",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/overview/minute",
        axum_path: "/dash/overview/minute",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/restores/zip",
        axum_path: "/dash/restores/zip",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/restore-jobs",
        axum_path: "/dash/restore-jobs",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/restore-jobs/:id",
        axum_path: "/dash/restore-jobs/{id}",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/restore-jobs/:id",
        axum_path: "/dash/restore-jobs/{id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash",
        axum_path: "/dash",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/me",
        axum_path: "/dash/me",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps",
        axum_path: "/dash/apps",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps-summary",
        axum_path: "/dash/apps-summary",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/profiles",
        axum_path: "/dash/profiles",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id",
        axum_path: "/dash/apps/{app_id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/stats",
        axum_path: "/dash/apps/{app_id}/stats",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/usage",
        axum_path: "/dash/apps/{app_id}/usage",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id",
        axum_path: "/dash/apps/{app_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/clear",
        axum_path: "/dash/apps/{app_id}/clear",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/track-import",
        axum_path: "/dash/apps/{app_id}/track-import",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/rules",
        axum_path: "/dash/apps/{app_id}/rules",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/rules/validate",
        axum_path: "/dash/apps/{app_id}/rules/validate",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/rule-versions",
        axum_path: "/dash/apps/{app_id}/rule-versions",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/tokens",
        axum_path: "/dash/apps/{app_id}/tokens",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/tokens/rotate",
        axum_path: "/dash/apps/{app_id}/tokens/rotate",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/soft_deleted_attrs",
        axum_path: "/dash/apps/{app_id}/soft_deleted_attrs",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/ephemeral/:app_id",
        axum_path: "/dash/apps/ephemeral/{app_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/ephemeral",
        axum_path: "/dash/apps/ephemeral",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/ephemeral/:app_id/status",
        axum_path: "/dash/apps/ephemeral/{app_id}/status",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/ephemeral/:app_id/claim",
        axum_path: "/dash/apps/ephemeral/{app_id}/claim",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/claim",
        axum_path: "/dash/apps/{app_id}/claim",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/get_a_db/:app_id",
        axum_path: "/dash/apps/get_a_db/{app_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/get_a_db",
        axum_path: "/dash/apps/get_a_db",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/auth",
        axum_path: "/dash/apps/{app_id}/auth",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/authorized_redirect_origins",
        axum_path: "/dash/apps/{app_id}/authorized_redirect_origins",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/authorized_redirect_origins/:id",
        axum_path: "/dash/apps/{app_id}/authorized_redirect_origins/{id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth_service_providers",
        axum_path: "/dash/apps/{app_id}/oauth_service_providers",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth_clients",
        axum_path: "/dash/apps/{app_id}/oauth_clients",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/oauth_clients/:id",
        axum_path: "/dash/apps/{app_id}/oauth_clients/{id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth_clients/:id",
        axum_path: "/dash/apps/{app_id}/oauth_clients/{id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/oauth/start",
        axum_path: "/dash/oauth/start",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/oauth/callback",
        axum_path: "/dash/oauth/callback",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/oauth/token",
        axum_path: "/dash/oauth/token",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/cli/auth/register",
        axum_path: "/dash/cli/auth/register",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/cli/auth/check",
        axum_path: "/dash/cli/auth/check",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/cli/auth/claim",
        axum_path: "/dash/cli/auth/claim",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/cli/auth/void",
        axum_path: "/dash/cli/auth/void",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/cli/version",
        axum_path: "/dash/cli/version",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/session_counts",
        axum_path: "/dash/session_counts",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/invite/send",
        axum_path: "/dash/apps/{app_id}/invite/send",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/invite/revoke",
        axum_path: "/dash/apps/{app_id}/invite/revoke",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/members/remove",
        axum_path: "/dash/apps/{app_id}/members/remove",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/members/update",
        axum_path: "/dash/apps/{app_id}/members/update",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/default-email-template",
        axum_path: "/dash/default-email-template",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/sender-verification",
        axum_path: "/dash/apps/{app_id}/sender-verification",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/sender-verification/send-magic-code",
        axum_path: "/dash/apps/{app_id}/sender-verification/send-magic-code",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/sender-verification/verify-magic-code",
        axum_path: "/dash/apps/{app_id}/sender-verification/verify-magic-code",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/email_status",
        axum_path: "/dash/apps/{app_id}/email_status",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/email_templates",
        axum_path: "/dash/apps/{app_id}/email_templates",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/send-test-email",
        axum_path: "/dash/apps/{app_id}/send-test-email",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/email_templates/:id",
        axum_path: "/dash/apps/{app_id}/email_templates/{id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/invites/accept",
        axum_path: "/dash/invites/accept",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/invites/decline",
        axum_path: "/dash/invites/decline",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/personal_access_tokens",
        axum_path: "/dash/personal_access_tokens",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/personal_access_tokens",
        axum_path: "/dash/personal_access_tokens",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/personal_access_tokens/:id",
        axum_path: "/dash/personal_access_tokens/{id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/rename",
        axum_path: "/dash/apps/{app_id}/rename",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/status",
        axum_path: "/dash/apps/{app_id}/status",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/transfer_to_org/:org_id",
        axum_path: "/dash/apps/{app_id}/transfer_to_org/{org_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/set-magic-code-expiry",
        axum_path: "/dash/apps/{app_id}/set-magic-code-expiry",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/test_users",
        axum_path: "/dash/apps/{app_id}/test_users",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/test_users",
        axum_path: "/dash/apps/{app_id}/test_users",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/test_users",
        axum_path: "/dash/apps/{app_id}/test_users",
    },
    DashboardRoute {
        method: "PUT",
        source_path: "/dash/apps/:app_id/storage/upload",
        axum_path: "/dash/apps/{app_id}/storage/upload",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/storage/files/delete",
        axum_path: "/dash/apps/{app_id}/storage/files/delete",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/schema/push/plan",
        axum_path: "/dash/apps/{app_id}/schema/push/plan",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/schema/push/apply",
        axum_path: "/dash/apps/{app_id}/schema/push/apply",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/schema/steps/apply",
        axum_path: "/dash/apps/{app_id}/schema/steps/apply",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/schema/pull",
        axum_path: "/dash/apps/{app_id}/schema/pull",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/perms/pull",
        axum_path: "/dash/apps/{app_id}/perms/pull",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/indexing-jobs/:job_id",
        axum_path: "/dash/apps/{app_id}/indexing-jobs/{job_id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/indexing-jobs/group/:group_id",
        axum_path: "/dash/apps/{app_id}/indexing-jobs/group/{group_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/indexing-jobs",
        axum_path: "/dash/apps/{app_id}/indexing-jobs",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backup-schedule",
        axum_path: "/dash/apps/{app_id}/backup-schedule",
    },
    DashboardRoute {
        method: "PUT",
        source_path: "/dash/apps/:app_id/backup-schedule",
        axum_path: "/dash/apps/{app_id}/backup-schedule",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backups",
        axum_path: "/dash/apps/{app_id}/backups",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/backups",
        axum_path: "/dash/apps/{app_id}/backups",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/backups/:backup_id",
        axum_path: "/dash/apps/{app_id}/backups/{backup_id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backup-jobs",
        axum_path: "/dash/apps/{app_id}/backup-jobs",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backup-jobs/:job_id",
        axum_path: "/dash/apps/{app_id}/backup-jobs/{job_id}",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/backup-jobs/:job_id",
        axum_path: "/dash/apps/{app_id}/backup-jobs/{job_id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backups/:backup_id/files",
        axum_path: "/dash/apps/{app_id}/backups/{backup_id}/files",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backups/:backup_id/file-url",
        axum_path: "/dash/apps/{app_id}/backups/{backup_id}/file-url",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/backups/:backup_id/storage-files",
        axum_path: "/dash/apps/{app_id}/backups/{backup_id}/storage-files",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/backups/:backup_id/restore",
        axum_path: "/dash/apps/{app_id}/backups/{backup_id}/restore",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/restore-jobs",
        axum_path: "/dash/apps/{app_id}/restore-jobs",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/webhooks",
        axum_path: "/dash/apps/{app_id}/webhooks",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/webhooks",
        axum_path: "/dash/apps/{app_id}/webhooks",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id/enable",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}/enable",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id/disable",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}/disable",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id/events",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}/events",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id/events/*",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}/events/{*wildcard}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/webhooks/:webhook_id/events/*",
        axum_path: "/dash/apps/{app_id}/webhooks/{webhook_id}/events/{*wildcard}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/orgs",
        axum_path: "/dash/orgs",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/orgs/:org_id",
        axum_path: "/dash/orgs/{org_id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/orgs/:org_id",
        axum_path: "/dash/orgs/{org_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/orgs/:org_id/invite/send",
        axum_path: "/dash/orgs/{org_id}/invite/send",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/orgs/:org_id/invite/revoke",
        axum_path: "/dash/orgs/{org_id}/invite/revoke",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/orgs/:org_id/members/remove",
        axum_path: "/dash/orgs/{org_id}/members/remove",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/orgs/:org_id/members/update",
        axum_path: "/dash/orgs/{org_id}/members/update",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/orgs/:org_id/rename",
        axum_path: "/dash/orgs/{org_id}/rename",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/ws_playground",
        axum_path: "/dash/ws_playground",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/signout",
        axum_path: "/dash/signout",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/stats/active_sessions",
        axum_path: "/dash/stats/active_sessions",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/apps/:app_id/oauth-apps",
        axum_path: "/dash/apps/{app_id}/oauth-apps",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth-apps",
        axum_path: "/dash/apps/{app_id}/oauth-apps",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth-apps/:oauth_app_id",
        axum_path: "/dash/apps/{app_id}/oauth-apps/{oauth_app_id}",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/oauth-apps/:oauth_app_id",
        axum_path: "/dash/apps/{app_id}/oauth-apps/{oauth_app_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth-apps/:oauth_app_id/clients",
        axum_path: "/dash/apps/{app_id}/oauth-apps/{oauth_app_id}/clients",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth-app-clients/:client_id",
        axum_path: "/dash/apps/{app_id}/oauth-app-clients/{client_id}",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/oauth-app-clients/:client_id",
        axum_path: "/dash/apps/{app_id}/oauth-app-clients/{client_id}",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/apps/:app_id/oauth-app-clients/:client_id/client-secrets",
        axum_path: "/dash/apps/{app_id}/oauth-app-clients/{client_id}/client-secrets",
    },
    DashboardRoute {
        method: "DELETE",
        source_path: "/dash/apps/:app_id/oauth-app-client-secrets/:client_secret_id",
        axum_path: "/dash/apps/{app_id}/oauth-app-client-secrets/{client_secret_id}",
    },
    DashboardRoute {
        method: "GET",
        source_path: "/dash/user/oauth_apps",
        axum_path: "/dash/user/oauth_apps",
    },
    DashboardRoute {
        method: "POST",
        source_path: "/dash/user/oauth_apps/revoke_access",
        axum_path: "/dash/user/oauth_apps/revoke_access",
    },
];

#[derive(Debug)]
pub struct DashboardRequest {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// A request body staged on local disk for endpoints whose payloads can be
    /// much larger than the normal dashboard request limit. The path remains
    /// valid only for the duration of [`DashboardBackend::handle`].
    pub staged_body_path: Option<PathBuf>,
    /// Original peer address when the outer server was created with Axum
    /// `ConnectInfo`. Proxies should continue to supply `x-forwarded-for`.
    pub remote_addr: Option<String>,
}

pub enum DashboardResponseBody {
    Buffered(Bytes),
    Streaming(Body),
}

impl std::fmt::Debug for DashboardResponseBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buffered(bytes) => formatter
                .debug_tuple("Buffered")
                .field(&bytes.len())
                .finish(),
            Self::Streaming(_) => formatter.write_str("Streaming(..)"),
        }
    }
}

impl DashboardResponseBody {
    #[must_use]
    pub fn buffered(&self) -> Option<&Bytes> {
        match self {
            Self::Buffered(bytes) => Some(bytes),
            Self::Streaming(_) => None,
        }
    }

    pub fn into_body(self) -> Body {
        match self {
            Self::Buffered(bytes) => Body::from(bytes),
            Self::Streaming(body) => body,
        }
    }

    /// Collects a response body for consumers that require one bounded value,
    /// such as the dashboard session-report WebSocket bridge.
    async fn into_bytes(self, limit: usize) -> Result<Bytes, axum::Error> {
        match self {
            Self::Buffered(bytes) => Ok(bytes),
            Self::Streaming(body) => to_bytes(body, limit).await,
        }
    }
}

impl From<Bytes> for DashboardResponseBody {
    fn from(body: Bytes) -> Self {
        Self::Buffered(body)
    }
}

impl From<Vec<u8>> for DashboardResponseBody {
    fn from(body: Vec<u8>) -> Self {
        Self::Buffered(body.into())
    }
}

impl From<String> for DashboardResponseBody {
    fn from(body: String) -> Self {
        Self::Buffered(body.into())
    }
}

impl From<Body> for DashboardResponseBody {
    fn from(body: Body) -> Self {
        Self::Streaming(body)
    }
}

#[derive(Debug)]
pub struct DashboardResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: DashboardResponseBody,
}

impl DashboardResponse {
    #[must_use]
    pub fn json(body: impl Into<Bytes>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Self {
            status: StatusCode::OK,
            headers,
            body: DashboardResponseBody::Buffered(body.into()),
        }
    }
}

/// Dashboard control-plane boundary. The exact whitelist is owned by this crate while
/// app/org/backup/webhook persistence remains independently replaceable.
pub trait DashboardBackend: Send + Sync + 'static {
    fn authorize_restore_upload(
        &self,
        _request: DashboardRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        std::future::ready(Err(ApiError::new(
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

pub fn dashboard_router<B>(backend: Arc<B>) -> Router
where
    B: DashboardBackend,
{
    let mut limited = Router::new();
    let mut dashboard_upload = Router::new();
    let mut restore_upload = Router::new();
    for route in DASHBOARD_ROUTES {
        if route.axum_path == "/dash/session_counts" {
            limited = limited.route(route.axum_path, get(session_counts::<B>));
            continue;
        }
        if route.axum_path == "/dash/ws_playground" {
            limited = limited.route(route.axum_path, get(ws_playground));
            continue;
        }
        let filter = match route.method {
            "GET" => MethodFilter::GET,
            "POST" => MethodFilter::POST,
            "PUT" => MethodFilter::PUT,
            "DELETE" => MethodFilter::DELETE,
            "PATCH" => MethodFilter::PATCH,
            _ => unreachable!("audited dashboard method"),
        };
        let service = on(filter, dispatch::<B>);
        if route.axum_path == "/dash/restores/zip" {
            restore_upload = restore_upload.route(route.axum_path, service);
        } else if route.axum_path == "/dash/apps/{app_id}/storage/upload" {
            dashboard_upload = dashboard_upload.route(route.axum_path, service);
        } else {
            limited = limited.route(route.axum_path, service);
        }
    }
    body_limit::limit_body(
        limited,
        body_limit::MANAGEMENT_BODY_LIMIT,
        "Dashboard request body exceeds the 1 MiB limit",
    )
    .merge(body_limit::limit_body(
        dashboard_upload,
        64 * 1024 * 1024,
        "Dashboard upload exceeds the 64 MiB limit",
    ))
    .merge(body_limit::limit_body(
        restore_upload,
        usize::try_from(RESTORE_UPLOAD_LIMIT).expect("restore upload limit fits usize"),
        "Dashboard restore upload exceeds the 1 GiB limit",
    ))
    .with_state(backend)
}

async fn session_counts<B>(State(backend): State<Arc<B>>, upgrade: WebSocketUpgrade) -> Response
where
    B: DashboardBackend,
{
    let Some(token) = std::env::var("INSTANT_SESSION_COUNTER_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade.on_upgrade(move |socket| session_counts_loop(backend, socket, token))
}

async fn session_counts_loop<B>(backend: Arc<B>, mut socket: WebSocket, token: String)
where
    B: DashboardBackend,
{
    let mut authorized = false;
    let mut reports = tokio::time::interval(std::time::Duration::from_secs(5));
    reports.tick().await;
    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(Ok(message)) = message else { break };
                match message {
                    Message::Text(text) => {
                        let valid = valid_session_counter_token(&text, &token);
                        if valid {
                            authorized = true;
                            if send_session_report(backend.as_ref(), &mut socket).await.is_err() {
                                break;
                            }
                        } else if socket.send(Message::Text(
                            json!({"op": "error", "message": "Invalid token"}).to_string().into()
                        )).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = reports.tick(), if authorized => {
                if send_session_report(backend.as_ref(), &mut socket).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn valid_session_counter_token(message: &str, expected: &str) -> bool {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|message| {
            message
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|token| token.as_bytes().ct_eq(expected.as_bytes()).into())
}

async fn send_session_report<B>(backend: &B, socket: &mut WebSocket) -> Result<(), ()>
where
    B: DashboardBackend,
{
    let response = backend
        .handle(DashboardRequest {
            method: Method::GET,
            path: "/dash/session_counts".to_owned(),
            query: None,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            staged_body_path: None,
            remote_addr: None,
        })
        .await
        .map_err(|_| ())?;
    let body = response
        .body
        .into_bytes(1024 * 1024)
        .await
        .map_err(|_| ())?;
    let report = serde_json::from_slice::<Value>(&body).map_err(|_| ())?;
    socket
        .send(Message::Text(
            json!({"op": "report", "report": report}).to_string().into(),
        ))
        .await
        .map_err(|_| ())
}

static WS_PLAYGROUND_ID: AtomicU64 = AtomicU64::new(0);

async fn ws_playground(upgrade: WebSocketUpgrade) -> Response {
    let id = WS_PLAYGROUND_ID.fetch_add(1, Ordering::Relaxed) + 1;
    upgrade.on_upgrade(move |socket| ws_playground_loop(id, socket))
}

async fn ws_playground_loop(id: u64, mut socket: WebSocket) {
    let opened = serde_json::to_string(&format!("[{id}] ok")).expect("string serialization");
    if socket.send(Message::Text(opened.into())).await.is_err() {
        return;
    }
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        let text = text.trim();
        if text == "break" || text == "throw-err" {
            let _ = socket.send(Message::Close(None)).await;
            break;
        }
        let reply = serde_json::to_string(&format!("[{id}] received {text}"))
            .expect("string serialization");
        if socket.send(Message::Text(reply.into())).await.is_err() {
            break;
        }
    }
}

async fn dispatch<B>(State(backend): State<Arc<B>>, request: Request) -> Result<Response, ApiError>
where
    B: DashboardBackend,
{
    let (parts, body) = request.into_parts();
    let remote_addr = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|address| address.0.ip().to_string());
    let (body, staged_body) =
        if parts.method == Method::POST && parts.uri.path() == "/dash/restores/zip" {
            let has_bearer = parts
                .headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split_once(' '))
                .is_some_and(|(scheme, token)| {
                    scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty()
                });
            if !has_bearer {
                return Err(ApiError::new(
                    ApiErrorType::ParamMalformed,
                    "Malformed dashboard bearer token",
                )
                .unauthorized());
            }
            backend
                .authorize_restore_upload(DashboardRequest {
                    method: parts.method.clone(),
                    path: parts.uri.path().to_owned(),
                    query: parts.uri.query().map(str::to_owned),
                    headers: parts.headers.clone(),
                    body: Bytes::new(),
                    staged_body_path: None,
                    remote_addr: remote_addr.clone(),
                })
                .await?;
            (Bytes::new(), Some(stage_restore_body(body).await?))
        } else {
            let body = to_bytes(body, 64 * 1024 * 1024).await.map_err(|_| {
                ApiError::new(
                    ApiErrorType::ParameterLimitExceeded,
                    "Dashboard request body exceeds the 64 MiB limit",
                )
            })?;
            (body, None)
        };
    let response = backend
        .handle(DashboardRequest {
            method: parts.method,
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().map(str::to_owned),
            headers: parts.headers,
            body,
            staged_body_path: staged_body.as_ref().map(|body| body.path.clone()),
            remote_addr,
        })
        .await?;
    let mut result = (response.status, response.body.into_body()).into_response();
    result.headers_mut().extend(response.headers);
    Ok(result)
}

#[derive(Debug)]
struct StagedBody {
    path: PathBuf,
}

impl Drop for StagedBody {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn stage_restore_body(body: Body) -> Result<StagedBody, ApiError> {
    let path = std::env::temp_dir().join(format!("instant-restore-upload-{}.zip", Uuid::new_v4()));
    stage_restore_body_at(body, path, RESTORE_UPLOAD_LIMIT).await
}

async fn stage_restore_body_at(
    body: Body,
    path: PathBuf,
    limit: u64,
) -> Result<StagedBody, ApiError> {
    let staged = StagedBody { path };
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged.path)
        .await
        .map_err(staging_error)?;
    let mut stream = body.into_data_stream();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(staging_error)?;
        written = written.saturating_add(chunk.len() as u64);
        if written > limit {
            return Err(ApiError::new(
                ApiErrorType::ParameterLimitExceeded,
                "Restore ZIP exceeds the 1 GiB limit",
            ));
        }
        file.write_all(&chunk).await.map_err(staging_error)?;
    }
    file.flush().await.map_err(staging_error)?;
    drop(file);
    Ok(staged)
}

fn staging_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        format!("Could not stage restore ZIP: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::http::{Request, header};
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct Backend(Mutex<Vec<DashboardRequest>>);

    impl DashboardBackend for Backend {
        fn handle(
            &self,
            request: DashboardRequest,
        ) -> impl Future<Output = Result<DashboardResponse, ApiError>> + Send {
            self.0.lock().unwrap().push(request);
            std::future::ready(Ok(DashboardResponse::json(r#"{"ok":true}"#)))
        }
    }

    #[derive(Default)]
    struct StagingBackend(Mutex<Option<(Vec<u8>, PathBuf)>>);

    impl DashboardBackend for StagingBackend {
        fn authorize_restore_upload(
            &self,
            _request: DashboardRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Ok(()))
        }

        async fn handle(&self, request: DashboardRequest) -> Result<DashboardResponse, ApiError> {
            assert!(request.body.is_empty());
            let path = request
                .staged_body_path
                .expect("restore uploads are staged");
            let bytes = tokio::fs::read(&path).await.map_err(staging_error)?;
            *self.0.lock().unwrap() = Some((bytes, path));
            Ok(DashboardResponse::json(r#"{"ok":true}"#))
        }
    }

    struct StreamingBackend;

    impl DashboardBackend for StreamingBackend {
        fn handle(
            &self,
            _request: DashboardRequest,
        ) -> impl Future<Output = Result<DashboardResponse, ApiError>> + Send {
            let chunks = futures_util::stream::iter([
                Ok::<_, std::convert::Infallible>(Bytes::from_static(b"first\n")),
                Ok(Bytes::from_static(b"second\n")),
            ]);
            std::future::ready(Ok(DashboardResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Body::from_stream(chunks).into(),
            }))
        }
    }

    struct RejectRestoreBackend;

    impl DashboardBackend for RejectRestoreBackend {
        fn authorize_restore_upload(
            &self,
            _request: DashboardRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Invalid dashboard token",
            )
            .unauthorized()))
        }

        fn handle(
            &self,
            _request: DashboardRequest,
        ) -> impl Future<Output = Result<DashboardResponse, ApiError>> + Send {
            std::future::ready(Ok(DashboardResponse::json(r#"{"unexpected":true}"#)))
        }
    }

    #[test]
    fn inventory_is_unique_and_matches_the_authoritative_group_count() {
        // Includes the public config probe, the workspace usage summary, the
        // operator flags API, the rules validation probe, the owner-level
        // backup restore routes, the server-side admin-token rotation route
        // and the native GET and PUT backup-schedule routes.
        assert_eq!(DASHBOARD_ROUTES.len(), 134);
        let unique = DASHBOARD_ROUTES
            .iter()
            .map(|route| (route.method, route.source_path))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), DASHBOARD_ROUTES.len());
    }

    #[test]
    fn session_counter_requires_the_configured_token_message() {
        assert!(valid_session_counter_token(
            r#"{"token":"configured-secret"}"#,
            "configured-secret"
        ));
        assert!(!valid_session_counter_token(
            r#"{"token":"7deea103-7ef8-43f4-aae6-519f51ef33ed"}"#,
            "configured-secret"
        ));
        assert!(!valid_session_counter_token(
            r#"{"token":"wrong"}"#,
            "configured-secret"
        ));
        assert!(!valid_session_counter_token(
            "not-json",
            "configured-secret"
        ));
    }

    #[tokio::test]
    async fn exact_routes_dispatch_and_unlisted_dashboard_paths_do_not() {
        let backend = Arc::new(Backend::default());
        let router = dashboard_router(Arc::clone(&backend));
        let response = router
            .clone()
            .oneshot(
                Request::post("/dash/apps/00000000-0000-0000-0000-000000000001/webhooks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"url":"https://example.test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        {
            let requests = backend.0.lock().unwrap();
            assert_eq!(requests[0].method, Method::POST);
            assert_eq!(
                requests[0].path,
                "/dash/apps/00000000-0000-0000-0000-000000000001/webhooks"
            );
            assert!(requests[0].staged_body_path.is_none());
        }

        let response = router
            .oneshot(
                Request::get("/dash/not-a-real-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn every_dashboard_declaration_is_mounted_with_its_declared_method() {
        let backend = Arc::new(Backend::default());
        let router = dashboard_router(Arc::clone(&backend));
        let parameter = "00000000-0000-0000-0000-000000000001";

        // The two WebSocket declarations have their own upgrade handlers. The
        // restore upload is likewise intentionally staged to disk, and has a
        // dedicated contract below; all other declarations must reach the
        // ordinary dashboard backend boundary with the declared method/path.
        for route in DASHBOARD_ROUTES {
            if matches!(
                route.source_path,
                "/dash/session_counts" | "/dash/ws_playground" | "/dash/restores/zip"
            ) {
                continue;
            }
            let path = route
                .source_path
                .split('/')
                .map(|part| {
                    if part.starts_with(':') {
                        parameter
                    } else if part == "*" {
                        "nested/path"
                    } else {
                        part
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            let method = Method::from_bytes(route.method.as_bytes()).unwrap();
            let request = Request::builder()
                .method(method.clone())
                .uri(&path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "unmounted dashboard declaration: {} {}",
                route.method,
                route.source_path
            );
        }

        let requests = backend.0.lock().unwrap();
        assert_eq!(requests.len(), DASHBOARD_ROUTES.len() - 3);
        for request in requests.iter() {
            assert!(
                DASHBOARD_ROUTES.iter().any(|route| {
                    route.method == request.method.as_str()
                        && route
                            .source_path
                            .split('/')
                            .map(|part| {
                                if part.starts_with(':') {
                                    parameter
                                } else if part == "*" {
                                    "nested/path"
                                } else {
                                    part
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("/")
                            == request.path
                }),
                "backend received undeclared dashboard request: {} {}",
                request.method,
                request.path
            );
        }
    }

    #[tokio::test]
    async fn streaming_dashboard_responses_reach_the_http_body() {
        let router = dashboard_router(Arc::new(StreamingBackend));
        let response = router
            .oneshot(
                Request::get("/dash/apps/00000000-0000-0000-0000-000000000001/backups")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(body, "first\nsecond\n");
    }

    #[tokio::test]
    async fn restore_uploads_are_streamed_to_a_scoped_temporary_file() {
        let backend = Arc::new(StagingBackend::default());
        let router = dashboard_router(Arc::clone(&backend));
        let chunks = futures_util::stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"zip-")),
            Ok(Bytes::from_static(b"payload")),
        ]);
        let response = router
            .oneshot(
                Request::post("/dash/restores/zip")
                    .header(header::AUTHORIZATION, "Bearer test-token")
                    .body(Body::from_stream(chunks))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (bytes, path) = backend.0.lock().unwrap().take().unwrap();
        assert_eq!(bytes, b"zip-payload");
        assert!(!path.exists(), "staged upload is removed after dispatch");
    }

    #[tokio::test]
    async fn failed_and_oversized_restore_uploads_remove_partial_files() {
        let oversized_path = std::env::temp_dir().join(format!(
            "instant-restore-upload-test-{}.zip",
            Uuid::new_v4()
        ));
        let oversized = stage_restore_body_at(
            Body::from(Bytes::from_static(b"too-large")),
            oversized_path.clone(),
            4,
        )
        .await;
        assert!(oversized.is_err());
        assert!(!oversized_path.exists());

        let failed_path = std::env::temp_dir().join(format!(
            "instant-restore-upload-test-{}.zip",
            Uuid::new_v4()
        ));
        let chunks = futures_util::stream::iter([
            Ok(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("stream failed")),
        ]);
        let failed =
            stage_restore_body_at(Body::from_stream(chunks), failed_path.clone(), 64).await;
        assert!(failed.is_err());
        assert!(!failed_path.exists());
    }

    #[tokio::test]
    async fn restore_authentication_precedes_body_reads() {
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stream_polled = Arc::clone(&polled);
        let body = Body::from_stream(futures_util::stream::once(async move {
            stream_polled.store(true, Ordering::Release);
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"zip"))
        }));
        let response = dashboard_router(Arc::new(RejectRestoreBackend))
            .oneshot(
                Request::post("/dash/restores/zip")
                    .header(header::AUTHORIZATION, "Bearer invalid")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!polled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn oversized_dashboard_bodies_are_rejected_before_dispatch() {
        let backend = Arc::new(Backend::default());
        let body = "x".repeat(crate::body_limit::MANAGEMENT_BODY_LIMIT + 1);
        let response = dashboard_router(Arc::clone(&backend))
            .oneshot(
                Request::post(format!("/dash/apps/{}/rename", Uuid::new_v4()))
                    .header(header::CONTENT_LENGTH, body.len())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "parameter-limit-exceeded");
        assert!(backend.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dashboard_file_uploads_keep_their_larger_body_limit() {
        let backend = Arc::new(Backend::default());
        let body = "x".repeat(crate::body_limit::MANAGEMENT_BODY_LIMIT + 1);
        let response = dashboard_router(Arc::clone(&backend))
            .oneshot(
                Request::put(format!("/dash/apps/{}/storage/upload", Uuid::new_v4()))
                    .header(header::CONTENT_LENGTH, body.len())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let requests = backend.0.lock().unwrap();
        assert_eq!(
            requests[0].body.len(),
            crate::body_limit::MANAGEMENT_BODY_LIMIT + 1
        );
    }
}
