use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use instant_http::{ApiError, DashboardRequest, DashboardResponse};
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardControlService,
    DashboardSubsystem,
};
use serde_json::{Value, json};
use tokio_postgres::NoTls;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Default)]
struct OAuthHooks {
    allow_signup: Arc<AtomicBool>,
    exchanges: Arc<Mutex<Vec<(String, String)>>>,
}

impl DashboardControlHooks for OAuthHooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let result = match operation {
            DashboardControlHookOperation::DashboardOAuthConfiguration { .. } => json!({
                "authorization_endpoint": "https://accounts.example.com/o/oauth2/v2/auth",
                "client_id": "dashboard-client",
                "callback_url": "https://api.example.com/dash/oauth/callback",
                "dashboard_origin": "https://dashboard.example.com",
                "dev_dashboard_origin": "http://localhost:3000",
                "secure_cookie": true
            }),
            DashboardControlHookOperation::DashboardOAuthExchangeCode { code, redirect_uri } => {
                self.exchanges.lock().unwrap().push((code, redirect_uri));
                json!({
                    "email": "new.user@example.com",
                    "subject": "google-subject-1",
                    "email_verified": true
                })
            }
            DashboardControlHookOperation::AuthorizeDashboardOAuthSignup { email } => {
                assert_eq!(email, "new.user@example.com");
                Value::Bool(self.allow_signup.load(Ordering::SeqCst))
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
    query: Option<String>,
    cookie: Option<String>,
    body: Value,
) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    if let Some(cookie) = cookie {
        headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
    }
    DashboardRequest {
        method,
        path: path.to_owned(),
        query,
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
async fn dashboard_oauth_start_callback_and_one_use_token_flow_are_persisted() {
    let Some(url) = instant_test_support::test_database_url(
        "dashboard_oauth_start_callback_and_one_use_token_flow_are_persisted",
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
               created_at timestamp NOT NULL DEFAULT now(),
               google_sub text UNIQUE
             );
             CREATE TEMP TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               last_used_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_oauth_redirects (
               lookup_key bytea PRIMARY KEY,
               state uuid NOT NULL,
               cookie uuid NOT NULL,
               service text NOT NULL,
               redirect_path text NOT NULL,
               redirect_to_dev boolean,
               ticket uuid,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_oauth_codes (
               lookup_key bytea PRIMARY KEY,
               redirect_path text NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               created_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();

    let hooks = OAuthHooks::default();
    hooks.allow_signup.store(true, Ordering::SeqCst);
    let service = DashboardControlService::new(client, hooks.clone());
    let ticket = Uuid::new_v4();
    let started = service
        .handle(request(
            Method::GET,
            "/dash/oauth/start",
            Some(format!(
                "redirect_path=projects&redirect_to_dev=true&ticket={ticket}"
            )),
            None,
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(started.status, StatusCode::FOUND);
    let authorization_url =
        Url::parse(started.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(authorization_url.host_str(), Some("accounts.example.com"));
    let params = authorization_url.query_pairs().collect::<Vec<_>>();
    assert!(
        params
            .iter()
            .any(|(key, value)| key == "scope" && value == "email")
    );
    assert!(
        params
            .iter()
            .any(|(key, value)| key == "client_id" && value == "dashboard-client")
    );
    let state = params
        .iter()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.to_string())
        .unwrap();
    let cookie = started.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(
        started.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Secure")
    );

    let callback = service
        .handle(request(
            Method::GET,
            "/dash/oauth/callback",
            Some(format!("state={state}&code=provider-code")),
            Some(cookie.clone()),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(callback.status, StatusCode::FOUND);
    let callback_url = Url::parse(callback.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(
        callback_url.origin().ascii_serialization(),
        "http://localhost:3000"
    );
    assert_eq!(callback_url.path(), "/dash/oauth/callback");
    assert_eq!(
        callback_url
            .query_pairs()
            .find(|(key, _)| key == "ticket")
            .map(|(_, value)| value.to_string()),
        Some(ticket.to_string())
    );
    let code = callback_url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| Uuid::parse_str(&value).unwrap())
        .unwrap();
    assert_eq!(
        hooks.exchanges.lock().unwrap().as_slice(),
        &[(
            "provider-code".to_owned(),
            "https://api.example.com/dash/oauth/callback".to_owned()
        )]
    );

    let session = service
        .handle(request(
            Method::POST,
            "/dash/oauth/token",
            None,
            None,
            json!({"code": code}),
        ))
        .await
        .unwrap();
    let session = body(session);
    assert_eq!(session["redirect_path"], "/projects");
    assert_eq!(session["user"]["email"], "new.user@example.com");
    assert!(Uuid::parse_str(session["token"].as_str().unwrap()).is_ok());
    assert!(
        service
            .handle(request(
                Method::POST,
                "/dash/oauth/token",
                None,
                None,
                json!({"code": code})
            ))
            .await
            .is_err()
    );

    let replay = service
        .handle(request(
            Method::GET,
            "/dash/oauth/callback",
            Some(format!("state={state}&code=provider-code")),
            Some(cookie),
            json!({}),
        ))
        .await
        .unwrap();
    let replay_url = Url::parse(replay.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert!(
        replay_url
            .query_pairs()
            .find(|(key, _)| key == "error")
            .is_some()
    );
}
