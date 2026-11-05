use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use instant_http::{ApiError, DashboardRequest, DashboardResponse};
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardControlService,
    DashboardSubsystem,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

type PersistedRestore = (Uuid, Uuid, Vec<u8>);

#[derive(Clone, Default)]
struct RestoreHooks {
    allow_admin: Arc<AtomicBool>,
    persisted: Arc<Mutex<Vec<PersistedRestore>>>,
    enqueued: Arc<Mutex<Vec<Uuid>>>,
}

impl DashboardControlHooks for RestoreHooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let result = match operation {
            DashboardControlHookOperation::AuthorizeDashboardAdmin { email } => {
                assert_eq!(email, "operator@example.com");
                Value::Bool(self.allow_admin.load(Ordering::SeqCst))
            }
            DashboardControlHookOperation::PersistRestoreZip {
                job_id,
                app_id,
                body,
                staged_path,
            } => {
                assert!(staged_path.is_none());
                self.persisted.lock().unwrap().push((job_id, app_id, body));
                Value::String(format!("/tmp/instant-restore-{job_id}.zip"))
            }
            DashboardControlHookOperation::EnqueueRestoreJob { job_id } => {
                self.enqueued.lock().unwrap().push(job_id);
                Value::Null
            }
            _ => Value::Null,
        };
        ready(Ok(result))
    }
}

fn request(
    method: Method,
    path: &str,
    query: Option<String>,
    token: Uuid,
    body: &'static [u8],
) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    DashboardRequest {
        method,
        path: path.to_owned(),
        query,
        headers,
        body: Bytes::from_static(body),
        staged_body_path: None,
        remote_addr: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn json_body(response: DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dashboard_restore_routes_authorize_persist_and_track_job_lifecycle() {
    let Some(url) = instant_test_support::test_database_url(
        "dashboard_restore_routes_authorize_persist_and_track_job_lifecycle",
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
             );
             CREATE TEMP TABLE orgs (id uuid PRIMARY KEY);
             CREATE TEMP TABLE apps (id uuid PRIMARY KEY);
             CREATE TEMP TABLE app_restore_jobs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               creator_id uuid REFERENCES instant_users(id) ON DELETE SET NULL,
               org_id uuid REFERENCES orgs(id) ON DELETE SET NULL,
               title text,
               zip_path text NOT NULL,
               job_status text NOT NULL DEFAULT 'waiting',
               progress text,
               error text,
               created_at timestamptz NOT NULL DEFAULT now(),
               done_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();

    let user_id = Uuid::new_v4();
    let token = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'operator@example.com')",
            &[&user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id) VALUES ($1, $2, $3)",
            &[
                &token,
                &Sha256::digest(token.to_string().as_bytes()).to_vec(),
                &user_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute("INSERT INTO orgs (id) VALUES ($1)", &[&org_id])
        .await
        .unwrap();

    let hooks = RestoreHooks::default();
    hooks.allow_admin.store(true, Ordering::SeqCst);
    let service = DashboardControlService::new(client, hooks.clone());

    let app_id = Uuid::new_v4();
    let created = service
        .handle(request(
            Method::POST,
            "/dash/restores/zip",
            Some(format!(
                "app_id={app_id}&org_id={org_id}&title=Recovered+App"
            )),
            token,
            b"PK\x03\x04test-zip",
        ))
        .await
        .unwrap();
    let created = json_body(created)["restore-job"].clone();
    let job_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    assert_eq!(created["app_id"], json!(app_id));
    assert_eq!(created["title"], "Recovered App");
    assert_eq!(created["job_status"], "waiting");
    assert!(created["progress"].is_null());
    assert_eq!(
        hooks.persisted.lock().unwrap().as_slice(),
        &[(job_id, app_id, b"PK\x03\x04test-zip".to_vec())]
    );
    assert_eq!(hooks.enqueued.lock().unwrap().as_slice(), &[job_id]);

    let listed = service
        .handle(request(Method::GET, "/dash/restore-jobs", None, token, b""))
        .await
        .unwrap();
    assert_eq!(json_body(listed)["restore-jobs"][0]["id"], json!(job_id));

    let path = format!("/dash/restore-jobs/{job_id}");
    let fetched = service
        .handle(request(Method::GET, &path, None, token, b""))
        .await
        .unwrap();
    assert_eq!(json_body(fetched)["restore-job"]["id"], json!(job_id));

    let cancelled = service
        .handle(request(Method::DELETE, &path, None, token, b""))
        .await
        .unwrap();
    let cancelled = json_body(cancelled);
    assert_eq!(cancelled["restore-job"]["job_status"], "cancelled");
    assert!(!cancelled["restore-job"]["done_at"].is_null());

    let cancelled_again = service
        .handle(request(Method::DELETE, &path, None, token, b""))
        .await
        .unwrap();
    assert!(json_body(cancelled_again)["restore-job"].is_null());

    let missing = service
        .handle(request(
            Method::GET,
            &format!("/dash/restore-jobs/{}", Uuid::new_v4()),
            None,
            token,
            b"",
        ))
        .await
        .unwrap();
    assert!(json_body(missing)["restore-job"].is_null());

    hooks.allow_admin.store(false, Ordering::SeqCst);
    assert!(
        service
            .handle(request(Method::GET, "/dash/restore-jobs", None, token, b""))
            .await
            .is_err()
    );
}
