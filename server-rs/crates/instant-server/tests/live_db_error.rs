use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use instant_http::superadmin_router;
use instant_server::{
    PostgresSuperadminAuthorizer, SuperadminPrincipal, SuperadminService, SuperadminStore,
    trace_http_request,
};
use serde_json::Value;
use tokio_postgres::NoTls;
use tower::ServiceExt as _;
use uuid::Uuid;

struct UnreachableStore;

impl SuperadminStore for UnreachableStore {
    async fn execute(
        &self,
        _principal: SuperadminPrincipal,
        _context: instant_http::SuperadminContext,
        _operation: instant_http::SuperadminOperation,
    ) -> Result<Value, instant_http::ApiError> {
        panic!("authorization failures must short-circuit before the store");
    }
}

#[tokio::test]
async fn superadmin_database_failure_is_generic_and_reports_the_request_id() {
    let Some(database_url) = instant_test_support::test_database_url(
        "superadmin_database_failure_is_generic_and_reports_the_request_id",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let schema = format!("db_error_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}"
        ))
        .await
        .unwrap();
    let service =
        SuperadminService::new(PostgresSuperadminAuthorizer::new(client), UnreachableStore);
    let router =
        superadmin_router(Arc::new(service)).layer(axum::middleware::from_fn(trace_http_request));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/superadmin/apps")
                .header("authorization", "Bearer not-a-real-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], "unknown");
    assert_eq!(body["message"], "Database error");
    assert_eq!(body["trace-id"], request_id);
}
