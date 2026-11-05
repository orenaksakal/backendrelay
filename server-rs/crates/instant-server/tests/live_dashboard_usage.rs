use std::future::{Future, ready};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use instant_http::{ApiError, DashboardRequest, DashboardResponse};
use instant_instaql::system_attr_id;
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardControlService,
    DashboardSubsystem,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Clone)]
struct Hooks {
    live_sessions: Value,
}

impl DashboardControlHooks for Hooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let result = match operation {
            DashboardControlHookOperation::AppSessionCounts => self.live_sessions.clone(),
            _ => Value::Null,
        };
        ready(Ok(result))
    }
}

fn request(method: Method, path: &str, query: Option<&str>, token: &str) -> DashboardRequest {
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
        body: Bytes::new(),
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

#[test]
fn usage_endpoints_report_three_apps_and_honour_workspace_access() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_usage_endpoints());
}

#[allow(clippy::too_many_lines)]
async fn run_usage_endpoints() {
    let Some(url) = instant_test_support::test_database_url("run_usage_endpoints") else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TYPE pg_temp.isn AS (slot_num int, lsn pg_lsn);
             CREATE TEMP TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE
             );
             CREATE TEMP TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               last_used_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE orgs (
               id uuid PRIMARY KEY,
               title text NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE org_members (
               id uuid PRIMARY KEY,
               org_id uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               role text NOT NULL,
               UNIQUE (org_id, user_id)
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid REFERENCES instant_users(id),
               org_id uuid REFERENCES orgs(id),
               title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'active',
               magic_code_expiry_minutes integer,
               deletion_marked_at timestamp,
               subscription_id uuid
             );
             CREATE TEMP TABLE app_members (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               member_role text NOT NULL,
               UNIQUE (app_id, user_id)
             );
             CREATE TEMP TABLE triples_size_aggregate (
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               pg_size bigint NOT NULL,
               files_size bigint,
               PRIMARY KEY (app_id, attr_id)
             );
             CREATE TEMP TABLE attr_sketches (
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               total bigint NOT NULL
             );
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE daily_app_transactions (
               date date NOT NULL,
               app_id uuid NOT NULL,
               active_date timestamp,
               is_active boolean NOT NULL DEFAULT true,
               count bigint,
               UNIQUE (date, app_id, is_active)
             );
             CREATE TEMP TABLE app_backups (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               isn isn,
               backup_at timestamptz NOT NULL,
               storage_prefix text NOT NULL,
               files_size bigint,
               db_size bigint,
               uncompressed_size bigint,
               description text,
               expires_at timestamptz,
               deletion_marked_at timestamptz
             );",
        )
        .await
        .unwrap();

    let owner_id = Uuid::new_v4();
    let outsider_id = Uuid::new_v4();
    let member_id = Uuid::new_v4();
    let owner_refresh = Uuid::new_v4();
    let outsider_refresh = Uuid::new_v4();
    let member_refresh = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'owner@example.com'), ($2, 'outsider@example.com'),
               ($3, 'member@example.com')",
            &[&owner_id, &outsider_id, &member_id],
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
                &refresh_digest(outsider_refresh),
                &outsider_id,
                &Uuid::new_v4(),
                &refresh_digest(member_refresh),
                &member_id,
            ],
        )
        .await
        .unwrap();

    let app_one = Uuid::new_v4();
    let app_two = Uuid::new_v4();
    let app_three = Uuid::new_v4();
    let deleted_app = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title, created_at) VALUES
               ($1, $4, 'Todo', '2026-01-01 00:00:00'),
               ($2, $4, 'Chat', '2026-01-02 00:00:00'),
               ($3, $4, 'Wiki', '2026-01-03 00:00:00'),
               ($5, $4, 'Gone', '2026-01-04 00:00:00')",
            &[&app_one, &app_two, &app_three, &owner_id, &deleted_app],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE apps SET deletion_marked_at = now() WHERE id = $1",
            &[&deleted_app],
        )
        .await
        .unwrap();

    let files_size_attr = system_attr_id("$files", "size").unwrap();
    let other_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO triples_size_aggregate (app_id, attr_id, pg_size, files_size) VALUES
               ($1, $3, 100, NULL), ($1, $4, 20, 4096),
               ($2, $3, 200, NULL)",
            &[&app_one, &app_two, &files_size_attr, &other_attr],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO attr_sketches (app_id, attr_id, total) VALUES ($1, $2, 3)",
            &[&app_one, &files_size_attr],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO transactions (app_id, created_at) VALUES
               ($1, now()), ($1, now()),
               ($1, now() - interval '2 days'),
               ($1, now() - interval '10 days'),
               ($2, now() - interval '3 days')",
            &[&app_one, &app_two],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO daily_app_transactions (date, app_id, count, is_active)
             SELECT current_date - day, $1, 1, true
               FROM generate_series(0, 13) AS day",
            &[&app_one],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO daily_app_transactions (date, app_id, count, is_active) VALUES
               (current_date, $1, 2, false),
               (current_date - 20, $1, 99, true)",
            &[&app_one],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_backups
               (id, app_id, backup_at, storage_prefix, expires_at)
             VALUES ($1, $2, now() - interval '1 hour', 'app-prefix/backup',
                     now() + interval '1 day')",
            &[&Uuid::new_v4(), &app_one],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_backups
               (id, app_id, backup_at, storage_prefix, expires_at)
             VALUES ($1, $2, now() - interval '2 days', 'app-prefix/expired',
                     now() - interval '1 day')",
            &[&Uuid::new_v4(), &app_two],
        )
        .await
        .unwrap();

    let org_id = Uuid::new_v4();
    let org_app_one = Uuid::new_v4();
    let org_app_two = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO orgs (id, title) VALUES ($1, 'Shared org')",
            &[&org_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_members (id, org_id, user_id, role)
             VALUES ($1, $2, $3, 'collaborator')",
            &[&Uuid::new_v4(), &org_id, &owner_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, org_id, title, created_at) VALUES
               ($1, $3, 'Org one', '2026-02-01 00:00:00'),
               ($2, $3, 'Org two', '2026-02-02 00:00:00')",
            &[&org_app_one, &org_app_two, &org_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_members (id, app_id, user_id, member_role)
             VALUES ($1, $2, $3, 'collaborator')",
            &[&Uuid::new_v4(), &org_app_one, &member_id],
        )
        .await
        .unwrap();

    let empty_org = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO orgs (id, title) VALUES ($1, 'Empty org')",
            &[&empty_org],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_members (id, org_id, user_id, role)
             VALUES ($1, $2, $3, 'collaborator')",
            &[&Uuid::new_v4(), &empty_org, &owner_id],
        )
        .await
        .unwrap();

    let mut live_sessions = serde_json::Map::new();
    live_sessions.insert(app_one.to_string(), json!(2));
    live_sessions.insert(app_two.to_string(), json!(0));
    live_sessions.insert(app_three.to_string(), json!(5));
    live_sessions.insert(org_app_one.to_string(), json!(1));
    let hooks = Hooks {
        live_sessions: Value::Object(live_sessions),
    };
    let service = DashboardControlService::new(client, hooks);
    let owner = owner_refresh.to_string();
    let outsider = outsider_refresh.to_string();
    let member = member_refresh.to_string();

    let usage = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_one}/usage"),
                None,
                &owner,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(usage["appBytes"], 120);
    assert_eq!(usage["storageBytes"], 4096);
    assert_eq!(usage["fileCount"], 3);
    assert_eq!(usage["liveSessions"], 2);
    assert_eq!(usage["transactions"]["last24h"], 2);
    assert_eq!(usage["transactions"]["last7d"], 3);
    let daily = usage["transactions"]["daily"].as_array().unwrap();
    assert_eq!(daily.len(), 14);
    assert_eq!(daily[0]["count"], 1);
    assert_eq!(daily[12]["count"], 1);
    assert_eq!(daily[13]["count"], 3);
    assert!(
        daily
            .windows(2)
            .all(|pair| pair[0]["date"].as_str() < pair[1]["date"].as_str())
    );

    let app_two_usage = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_two}/usage"),
                None,
                &owner,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(app_two_usage["appBytes"], 200);
    assert_eq!(app_two_usage["storageBytes"], 0);
    assert_eq!(app_two_usage["fileCount"], Value::Null);
    assert_eq!(app_two_usage["liveSessions"], 0);
    assert_eq!(app_two_usage["transactions"]["last24h"], 0);
    assert_eq!(app_two_usage["transactions"]["last7d"], 1);

    let app_three_usage = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_three}/usage"),
                None,
                &owner,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(app_three_usage["appBytes"], 0);
    assert_eq!(app_three_usage["storageBytes"], 0);
    assert_eq!(app_three_usage["fileCount"], Value::Null);
    assert_eq!(app_three_usage["liveSessions"], 5);
    assert_eq!(app_three_usage["transactions"]["last7d"], 0);

    let denied = service
        .handle(request(
            Method::GET,
            &format!("/dash/apps/{app_one}/usage"),
            None,
            &outsider,
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let personal = body(
        service
            .handle(request(
                Method::GET,
                "/dash/apps-summary",
                Some("workspace=personal"),
                &owner,
            ))
            .await
            .unwrap(),
    );
    let personal_apps = personal["apps"].as_array().unwrap();
    assert_eq!(personal_apps.len(), 3);
    assert_eq!(personal_apps[0]["id"], json!(app_one));
    assert_eq!(personal_apps[0]["liveSessions"], 2);
    assert_eq!(personal_apps[0]["appBytes"], 120);
    assert_eq!(personal_apps[0]["storageBytes"], 4096);
    assert!(personal_apps[0]["lastWriteAt"].is_string());
    assert!(personal_apps[0]["lastBackupAt"].is_string());
    assert_eq!(personal_apps[1]["id"], json!(app_two));
    assert_eq!(personal_apps[1]["appBytes"], 200);
    assert!(personal_apps[1]["lastWriteAt"].is_string());
    assert_eq!(personal_apps[1]["lastBackupAt"], Value::Null);
    assert_eq!(personal_apps[2]["id"], json!(app_three));
    assert_eq!(personal_apps[2]["appBytes"], 0);
    assert_eq!(personal_apps[2]["liveSessions"], 5);
    assert_eq!(personal_apps[2]["lastWriteAt"], Value::Null);
    assert_eq!(personal_apps[2]["lastBackupAt"], Value::Null);

    let org_summary = body(
        service
            .handle(request(
                Method::GET,
                "/dash/apps-summary",
                Some(&format!("workspace={org_id}")),
                &owner,
            ))
            .await
            .unwrap(),
    );
    let org_apps = org_summary["apps"].as_array().unwrap();
    assert_eq!(
        org_apps
            .iter()
            .map(|app| app["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(org_app_one), json!(org_app_two)]
    );
    assert_eq!(org_apps[0]["liveSessions"], 1);
    assert_eq!(org_apps[1]["liveSessions"], 0);

    let member_summary = body(
        service
            .handle(request(
                Method::GET,
                "/dash/apps-summary",
                Some(&format!("workspace={org_id}")),
                &member,
            ))
            .await
            .unwrap(),
    );
    let member_apps = member_summary["apps"].as_array().unwrap();
    assert_eq!(member_apps.len(), 1);
    assert_eq!(member_apps[0]["id"], json!(org_app_one));

    let outsider_summary = service
        .handle(request(
            Method::GET,
            "/dash/apps-summary",
            Some(&format!("workspace={org_id}")),
            &outsider,
        ))
        .await
        .unwrap_err();
    assert_eq!(outsider_summary.status(), StatusCode::UNAUTHORIZED);

    let bad_workspace = service
        .handle(request(
            Method::GET,
            "/dash/apps-summary",
            Some("workspace=nowhere"),
            &owner,
        ))
        .await
        .unwrap_err();
    assert_eq!(bad_workspace.status(), StatusCode::BAD_REQUEST);

    let missing_workspace = service
        .handle(request(Method::GET, "/dash/apps-summary", None, &owner))
        .await
        .unwrap_err();
    assert_eq!(missing_workspace.status(), StatusCode::BAD_REQUEST);

    let empty_summary = body(
        service
            .handle(request(
                Method::GET,
                "/dash/apps-summary",
                Some(&format!("workspace={empty_org}")),
                &owner,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(empty_summary["apps"].as_array().unwrap().len(), 0);
}
