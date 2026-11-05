use std::future::{Future, ready};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use instant_http::{ApiError, DashboardRequest};
use instant_server::{
    AllowDashboardIdentity, DashboardIdentityPolicy, DashboardIdentityService,
    DashboardMagicCodeDelivery, DashboardSubsystem, DashboardSummaryProvider, DashboardUser,
    INSTANT_CONFIG_APP_ID, PostgresDashboardIdentityPolicy,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

#[derive(Clone, Default)]
struct Delivery(Arc<Mutex<Vec<(String, String)>>>);

impl DashboardMagicCodeDelivery for Delivery {
    fn deliver(
        &self,
        email: String,
        code: String,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        self.0.lock().unwrap().push((email, code));
        ready(Ok(()))
    }
}

struct Summaries;

impl DashboardSummaryProvider for Summaries {
    fn summary(&self, user: DashboardUser) -> impl Future<Output = Result<Value, ApiError>> + Send {
        ready(Ok(json!({
            "apps": [],
            "orgs": [],
            "profile": null,
            "invites": [],
            "superuser": false,
            "user": {"id": user.id, "email": user.email}
        })))
    }
}

#[allow(clippy::needless_pass_by_value)]
fn request(method: Method, path: &str, token: Option<Uuid>, body: Value) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
    }
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
fn body(response: instant_http::DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dashboard_identity_flows_are_one_use_hashed_and_user_scoped() {
    let Some(url) = instant_test_support::test_database_url(
        "dashboard_identity_flows_are_one_use_hashed_and_user_scoped",
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
               email text NOT NULL UNIQUE,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               created_at timestamp NOT NULL DEFAULT now(),
               last_used_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_user_magic_codes (
               id uuid PRIMARY KEY,
               code text NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE user_flags (
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               flag_name text NOT NULL
             );
             CREATE TEMP TABLE instant_profiles (
               id uuid PRIMARY KEY REFERENCES instant_users(id) ON DELETE CASCADE,
               meta jsonb NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_cli_logins (
               id uuid PRIMARY KEY,
               secret bytea NOT NULL,
               used boolean NOT NULL DEFAULT false,
               created_at timestamp NOT NULL DEFAULT now(),
               user_id uuid REFERENCES instant_users(id) ON DELETE CASCADE
             );
             CREATE TEMP TABLE instant_personal_access_tokens (
               id uuid PRIMARY KEY,
               created_at timestamp NOT NULL DEFAULT now(),
               name text NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               lookup_key bytea NOT NULL
             );",
        )
        .await
        .unwrap();

    let delivery = Delivery::default();
    let delivered = Arc::clone(&delivery.0);
    let service =
        DashboardIdentityService::new(client, delivery, AllowDashboardIdentity, Summaries);

    let sent = service
        .handle(request(
            Method::POST,
            "/dash/auth/send_magic_code",
            None,
            json!({"email": "Test@Example.com"}),
        ))
        .await
        .unwrap();
    assert_eq!(body(sent), json!({"sent": true}));
    let code = delivered.lock().unwrap()[0].1.clone();

    let verified = body(
        service
            .handle(request(
                Method::POST,
                "/dash/auth/verify_magic_code",
                None,
                json!({"email": "test@example.com", "code": code}),
            ))
            .await
            .unwrap(),
    );
    let token = Uuid::parse_str(verified["token"].as_str().unwrap()).unwrap();
    assert_eq!(verified["user"]["email"], "test@example.com");

    let profile = body(
        service
            .handle(request(
                Method::POST,
                "/dash/profiles",
                Some(token),
                json!({"meta": {"role": "developer"}}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(profile["profile"]["meta"]["role"], "developer");

    let personal = body(
        service
            .handle(request(
                Method::POST,
                "/dash/personal_access_tokens",
                Some(token),
                json!({"name": "CI"}),
            ))
            .await
            .unwrap(),
    );
    assert!(
        personal["data"]["token"]
            .as_str()
            .unwrap()
            .starts_with("per_")
    );
    let personal_id = personal["data"]["id"].as_str().unwrap();
    let listed = body(
        service
            .handle(request(
                Method::GET,
                "/dash/personal_access_tokens",
                Some(token),
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert!(listed["data"][0].get("token").is_none());
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/personal_access_tokens/{personal_id}"),
            Some(token),
            json!({}),
        ))
        .await
        .unwrap();

    let registered = body(
        service
            .handle(request(
                Method::POST,
                "/dash/cli/auth/register",
                None,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    let ticket = registered["ticket"].as_str().unwrap();
    service
        .handle(request(
            Method::POST,
            "/dash/cli/auth/claim",
            Some(token),
            json!({"ticket": ticket}),
        ))
        .await
        .unwrap();
    let checked = body(
        service
            .handle(request(
                Method::POST,
                "/dash/cli/auth/check",
                None,
                json!({"secret": registered["secret"]}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(checked["email"], "test@example.com");
    assert!(
        service
            .handle(request(
                Method::POST,
                "/dash/cli/auth/check",
                None,
                json!({"secret": registered["secret"]}),
            ))
            .await
            .is_err()
    );

    service
        .handle(request(
            Method::POST,
            "/dash/signout",
            Some(token),
            json!({}),
        ))
        .await
        .unwrap();
    assert!(
        service
            .handle(request(Method::GET, "/dash/me", Some(token), json!({}),))
            .await
            .is_err()
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dashboard_refresh_tokens_expire_after_thirty_days_of_disuse() {
    let Some(database_url) = instant_test_support::test_database_url(
        "dashboard_refresh_tokens_expire_after_thirty_days_of_disuse",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("dashboard_refresh_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public;
             CREATE TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               created_at timestamp NOT NULL DEFAULT now(),
               last_used_at timestamptz NOT NULL DEFAULT now()
             );"
        ))
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'refresh@example.com')",
            &[&user_id],
        )
        .await
        .unwrap();
    let stale = Uuid::new_v4();
    let quiet = Uuid::new_v4();
    let active = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id, last_used_at)
             VALUES ($1, $2, $4, now() - interval '31 days'),
                    ($3, $5, $4, now() - interval '2 hours'),
                    ($6, $7, $4, now() - interval '10 minutes')",
            &[
                &stale,
                &Sha256::digest(stale.to_string().as_bytes()).to_vec(),
                &quiet,
                &user_id,
                &Sha256::digest(quiet.to_string().as_bytes()).to_vec(),
                &active,
                &Sha256::digest(active.to_string().as_bytes()).to_vec(),
            ],
        )
        .await
        .unwrap();

    let service = DashboardIdentityService::new(
        connect_schema(&database_url, &schema).await,
        Delivery::default(),
        AllowDashboardIdentity,
        Summaries,
    );
    assert!(
        service
            .handle(request(Method::GET, "/dash/me", Some(stale), json!({})))
            .await
            .is_err(),
        "a token unused for more than 30 days must be rejected"
    );
    let quiet_session = body(
        service
            .handle(request(Method::GET, "/dash/me", Some(quiet), json!({})))
            .await
            .unwrap(),
    );
    assert_eq!(quiet_session["user"]["email"], "refresh@example.com");
    assert!(
        setup
            .query_one(
                "SELECT last_used_at > now() - interval '1 minute'
                   FROM instant_user_refresh_tokens WHERE id = $1",
                &[&quiet],
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "using a token inside the sliding window refreshes last_used_at"
    );
    service
        .handle(request(Method::GET, "/dash/me", Some(active), json!({})))
        .await
        .unwrap();
    assert!(
        setup
            .query_one(
                "SELECT last_used_at < now() - interval '9 minutes'
                   FROM instant_user_refresh_tokens WHERE id = $1",
                &[&active],
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "last_used_at is written at most once per hour"
    );
}

#[tokio::test]
async fn operator_signup_default_applies_before_refresh_and_flag_wins() {
    let Some(database_url) = instant_test_support::test_database_url(
        "operator_signup_default_applies_before_refresh_and_flag_wins",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("dashboard_identity_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public;
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
             CREATE TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);"
        ))
        .await
        .unwrap();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO attrs VALUES
             ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')",
            &[&setting_attr, &value_attr, &INSTANT_CONFIG_APP_ID],
        )
        .await
        .unwrap();

    // The constructor must not freeze the closed default before the operator
    // config is applied: no refresh() here.
    let open_signups =
        PostgresDashboardIdentityPolicy::load(connect_schema(&database_url, &schema).await)
            .await
            .unwrap()
            .with_signup_open(true);
    open_signups.allow_signup("new@example.test").unwrap();

    let closed_signups =
        PostgresDashboardIdentityPolicy::load(connect_schema(&database_url, &schema).await)
            .await
            .unwrap()
            .with_signup_open(false);
    assert!(closed_signups.allow_signup("new@example.test").is_err());

    let flagged =
        PostgresDashboardIdentityPolicy::load(connect_schema(&database_url, &schema).await)
            .await
            .unwrap()
            .with_signup_open(true);
    insert_signup_flag(&setup, setting_attr, value_attr, json!({"mode": "closed"})).await;
    flagged.refresh().await.unwrap();
    assert!(flagged.allow_signup("new@example.test").is_err());

    setup
        .execute(
            "UPDATE triples SET value = $2 WHERE app_id = $1 AND attr_id = $3",
            &[
                &INSTANT_CONFIG_APP_ID,
                &json!({"mode": "bogus"}),
                &value_attr,
            ],
        )
        .await
        .unwrap();
    flagged.refresh().await.unwrap();
    flagged.allow_signup("new@example.test").unwrap();
}

async fn connect_schema(database_url: &str, schema: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(&format!("SET search_path TO {schema}, public"))
        .await
        .unwrap();
    client
}

async fn insert_signup_flag(client: &Client, setting_attr: Uuid, value_attr: Uuid, value: Value) {
    let setting = "dashboard-signups";
    let entity_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO triples VALUES ($1, $2, $3, to_jsonb($5::text)), ($1, $2, $4, $6)",
            &[
                &INSTANT_CONFIG_APP_ID,
                &entity_id,
                &setting_attr,
                &value_attr,
                &setting,
                &value,
            ],
        )
        .await
        .unwrap();
}
