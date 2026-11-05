use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use instant_http::{DashboardRequest, DashboardResponse};
use instant_server::{DashboardControlService, DashboardSubsystem, NoopDashboardControlHooks};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[allow(clippy::needless_pass_by_value)]
fn request(method: Method, path: &str, token: &str, body: Value) -> DashboardRequest {
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

fn refresh_digest(token: Uuid) -> Vec<u8> {
    Sha256::digest(token.to_string().as_bytes()).to_vec()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end rules validation journey.
async fn rules_validation_reports_errors_without_writing() {
    let Some(url) =
        instant_test_support::test_database_url("rules_validation_reports_errors_without_writing")
    else {
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
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid REFERENCES instant_users(id),
               org_id uuid,
               title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'active',
               magic_code_expiry_minutes integer,
               deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_members (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               member_role text NOT NULL,
               UNIQUE (app_id, user_id)
             );
             CREATE TEMP TABLE rules (
               app_id uuid PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
               code jsonb NOT NULL,
               version integer NOT NULL DEFAULT 0
             );",
        )
        .await
        .unwrap();

    let owner_id = Uuid::new_v4();
    let collaborator_id = Uuid::new_v4();
    let outsider_id = Uuid::new_v4();
    let owner_refresh = Uuid::new_v4();
    let collaborator_refresh = Uuid::new_v4();
    let outsider_refresh = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'owner@example.com'), ($2, 'collaborator@example.com'),
               ($3, 'outsider@example.com')",
            &[&owner_id, &collaborator_id, &outsider_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id)
             VALUES ($1, $2, $3), ($4, $5, $6), ($7, $8, $9)",
            &[
                &Uuid::new_v4(),
                &refresh_digest(owner_refresh),
                &owner_id,
                &Uuid::new_v4(),
                &refresh_digest(collaborator_refresh),
                &collaborator_id,
                &Uuid::new_v4(),
                &refresh_digest(outsider_refresh),
                &outsider_id,
            ],
        )
        .await
        .unwrap();

    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'Rules app')",
            &[&app_id, &owner_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_members (id, app_id, user_id, member_role)
             VALUES ($1, $2, $3, 'collaborator')",
            &[&Uuid::new_v4(), &app_id, &collaborator_id],
        )
        .await
        .unwrap();

    let service = DashboardControlService::new(client, NoopDashboardControlHooks);
    let validate_path = format!("/dash/apps/{app_id}/rules/validate");
    let owner = owner_refresh.to_string();
    let collaborator = collaborator_refresh.to_string();

    let valid = service
        .handle(request(
            Method::POST,
            &validate_path,
            &owner,
            json!({"code": {"goals": {"allow": {"view": "true"}}}}),
        ))
        .await
        .unwrap();
    assert_eq!(valid.status, StatusCode::OK);
    assert_eq!(body(valid), json!({"ok": true}));

    let invalid = service
        .handle(request(
            Method::POST,
            &validate_path,
            &collaborator,
            json!({"code": {"goals": {"allow": {"view": "("}}}}),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status, StatusCode::OK);
    let invalid = body(invalid);
    assert_eq!(invalid["ok"], false);
    let errors = invalid["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["in"], json!(["goals", "allow", "view"]));
    assert!(
        errors[0]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );

    let denied = service
        .handle(request(
            Method::POST,
            &validate_path,
            &outsider_refresh.to_string(),
            json!({"code": {"goals": {"allow": {"view": "true"}}}}),
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    // Validation never writes the rules row: the same read that backs the
    // dashboard's rules editor still finds nothing saved.
    let pulled = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/perms/pull"),
                &owner,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(pulled["perms"], Value::Null);
}
