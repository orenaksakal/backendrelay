use std::collections::HashSet;
use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use instant_http::{ApiError, ApiErrorType, DashboardRequest, DashboardResponse};
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardControlService,
    DashboardSubsystem,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Clone, Default)]
struct AdminHooks {
    allow: Arc<AtomicBool>,
    top_days: Arc<Mutex<Vec<i32>>>,
}

impl DashboardControlHooks for AdminHooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let result = match operation {
            DashboardControlHookOperation::AuthorizeDashboardAdmin { email } => {
                Value::Bool(self.allow.load(Ordering::SeqCst) && email == "operator@example.com")
            }
            DashboardControlHookOperation::ActiveSessionStats => {
                json!({"total-count": 3, "total-queries": 9})
            }
            DashboardControlHookOperation::DashboardAdminRecentUsers => {
                json!([{"email": "new@example.com"}])
            }
            DashboardControlHookOperation::DashboardAdminTopUsers { days } => {
                self.top_days.lock().unwrap().push(days);
                json!([{"user_email": "top@example.com", "total_transactions": 8}])
            }
            DashboardControlHookOperation::DashboardAdminStorageMetrics => {
                json!([{"app_id": Uuid::nil(), "usage": 10}])
            }
            DashboardControlHookOperation::DashboardAdminOverviewDaily => {
                json!({"charts": {}})
            }
            DashboardControlHookOperation::DashboardAdminOverviewMinute => {
                json!({"session-reports": {"node": {}}, "proxied-connections": null})
            }
            _ => Value::Null,
        };
        ready(Ok(result))
    }
}

#[allow(clippy::needless_pass_by_value)]
fn request(
    method: Method,
    path: &str,
    query: Option<&str>,
    token: Uuid,
    body: Value,
) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    DashboardRequest {
        method,
        path: path.to_owned(),
        query: query.map(str::to_owned),
        headers,
        body: Bytes::from(serde_json::to_vec(&body).unwrap()),
        staged_body_path: None,
        remote_addr: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn body(response: DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dashboard_admin_routes_enforce_operator_auth_and_preserve_response_contracts() {
    let Some(url) = instant_test_support::test_database_url(
        "dashboard_admin_routes_enforce_operator_auth_and_preserve_response_contracts",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TEMP TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE
             );
             CREATE TEMP TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               last_used_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    let viewer_id = Uuid::new_v4();
    let token = Uuid::new_v4();
    let viewer_token = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'operator@example.com'), ($2, 'viewer@example.com')",
            &[&user_id, &viewer_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id)
             VALUES ($1, $2, $3), ($4, $5, $6)",
            &[
                &token,
                &Sha256::digest(token.to_string().as_bytes()).to_vec(),
                &user_id,
                &viewer_token,
                &Sha256::digest(viewer_token.to_string().as_bytes()).to_vec(),
                &viewer_id,
            ],
        )
        .await
        .unwrap();

    let hooks = AdminHooks::default();
    hooks.allow.store(true, Ordering::SeqCst);
    let service = DashboardControlService::new(client, hooks.clone()).with_dashboard_email_policy(
        Some(HashSet::from(["viewer@example.com".to_owned()])),
        Some("operator@example.com".to_owned()),
    );

    let admin = body(
        service
            .handle(request(Method::GET, "/dash/admin", None, token, json!({})))
            .await
            .unwrap(),
    );
    assert_eq!(admin["users"][0]["email"], "new@example.com");
    assert_eq!(
        body(
            service
                .handle(request(
                    Method::GET,
                    "/dash/check-admin",
                    None,
                    token,
                    json!({})
                ))
                .await
                .unwrap()
        )["ok"],
        true
    );
    assert_eq!(
        body(
            service
                .handle(request(
                    Method::GET,
                    "/dash/top",
                    Some("n=30"),
                    token,
                    json!({})
                ))
                .await
                .unwrap()
        )["users"][0]["total_transactions"],
        8
    );
    assert_eq!(hooks.top_days.lock().unwrap().as_slice(), &[30]);
    assert_eq!(
        body(
            service
                .handle(request(
                    Method::GET,
                    "/dash/storage",
                    None,
                    token,
                    json!({})
                ))
                .await
                .unwrap()
        )["apps"][0]["usage"],
        10
    );
    assert_eq!(
        body(
            service
                .handle(request(
                    Method::GET,
                    "/dash/overview/daily",
                    None,
                    token,
                    json!({})
                ))
                .await
                .unwrap()
        )["charts"],
        json!({})
    );
    assert!(
        body(
            service
                .handle(request(
                    Method::GET,
                    "/dash/overview/minute",
                    None,
                    token,
                    json!({})
                ))
                .await
                .unwrap()
        )["proxied-connections"]
            .is_null()
    );
    let active_sessions = body(
        service
            .handle(request(
                Method::GET,
                "/dash/stats/active_sessions",
                None,
                token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(active_sessions["total-count"], 3);
    assert_eq!(active_sessions["total-queries"], 9);
    let viewer_denied = service
        .handle(request(
            Method::GET,
            "/dash/stats/active_sessions",
            None,
            viewer_token,
            json!({}),
        ))
        .await
        .unwrap_err();
    assert_eq!(viewer_denied.body().kind, ApiErrorType::PermissionDenied);
    assert_eq!(viewer_denied.status(), StatusCode::UNAUTHORIZED);
    hooks.allow.store(false, Ordering::SeqCst);
    assert!(
        service
            .handle(request(Method::GET, "/dash/top", None, token, json!({})))
            .await
            .is_err()
    );
    assert!(
        service
            .handle(request(
                Method::GET,
                "/dash/check-admin",
                None,
                token,
                json!({})
            ))
            .await
            .is_err()
    );
}
