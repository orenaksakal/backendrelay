use std::future::{Future, ready};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use instant_http::{ApiError, ApiErrorType, DashboardRequest, DashboardResponse};
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardControlService,
    DashboardSubsystem, migrate_database,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

struct OperatorHooks;

impl DashboardControlHooks for OperatorHooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let result = match operation {
            DashboardControlHookOperation::AuthorizeDashboardAdmin { email } => {
                Value::Bool(email.eq_ignore_ascii_case("operator@example.com"))
            }
            _ => Value::Null,
        };
        ready(Ok(result))
    }
}

async fn connect_and_migrate() -> Option<String> {
    let database_url = instant_test_support::test_database_url("live_dashboard_operator_flags")?;
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    migrate_database(&mut client).await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
    Some(database_url)
}

#[allow(clippy::needless_pass_by_value)]
fn request(method: Method, path: &str, token: Uuid, body: Value) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    DashboardRequest {
        method,
        path: path.to_owned(),
        query: None,
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
#[allow(clippy::too_many_lines)] // One end-to-end operator flag journey.
async fn operator_flags_require_superuser_and_round_trip() {
    let Some(database_url) = connect_and_migrate().await else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(connection);
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
    let operator_id = Uuid::new_v4();
    let viewer_id = Uuid::new_v4();
    let operator_token = Uuid::new_v4();
    let viewer_token = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'operator@example.com'), ($2, 'viewer@example.com')",
            &[&operator_id, &viewer_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id)
             VALUES ($1, $2, $3), ($4, $5, $6)",
            &[
                &Uuid::new_v4(),
                &Sha256::digest(operator_token.to_string().as_bytes()).to_vec(),
                &operator_id,
                &Uuid::new_v4(),
                &Sha256::digest(viewer_token.to_string().as_bytes()).to_vec(),
                &viewer_id,
            ],
        )
        .await
        .unwrap();

    let service = DashboardControlService::new(client, OperatorHooks);
    let flag_name = format!("dashboard-operator-test-{}", Uuid::new_v4());
    let flag_path = format!("/dash/operator/flags/{flag_name}");
    let value = json!({"emails": ["ops@example.com"], "enabled": true});

    let viewer_error = service
        .handle(request(
            Method::GET,
            "/dash/operator/flags",
            viewer_token,
            json!({}),
        ))
        .await
        .unwrap_err();
    assert_eq!(viewer_error.body().kind, ApiErrorType::PermissionDenied);
    assert_eq!(viewer_error.status(), StatusCode::FORBIDDEN);

    let viewer_write = service
        .handle(request(
            Method::PUT,
            &flag_path,
            viewer_token,
            json!({"value": value}),
        ))
        .await
        .unwrap_err();
    assert_eq!(viewer_write.status(), StatusCode::FORBIDDEN);

    let put = service
        .handle(request(
            Method::PUT,
            &flag_path,
            operator_token,
            json!({"value": value}),
        ))
        .await
        .unwrap();
    assert_eq!(put.status, StatusCode::OK);
    assert_eq!(body(put)["value"], value);

    let flags = body(
        service
            .handle(request(
                Method::GET,
                "/dash/operator/flags",
                operator_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(flags["flags"][flag_name.as_str()], value);
    assert!(
        flags["known"]
            .as_array()
            .unwrap()
            .iter()
            .any(|flag| flag["name"] == "combine-transacts")
    );

    let deleted = service
        .handle(request(
            Method::DELETE,
            &flag_path,
            operator_token,
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status, StatusCode::OK);
    assert_eq!(body(deleted)["deleted"], true);

    let flags = body(
        service
            .handle(request(
                Method::GET,
                "/dash/operator/flags",
                operator_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert!(flags["flags"].get(flag_name.as_str()).is_none());
}
