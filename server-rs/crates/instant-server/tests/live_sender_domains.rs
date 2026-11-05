use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use instant_http::{ApiError, ApiErrorType, DashboardRequest, DashboardResponse};
use instant_server::{
    CloudflareEmailError, DashboardControlHookOperation, DashboardControlHooks,
    DashboardControlService, DashboardSubsystem, EmailDomainStatus, EmailDomainVerifier,
    PostgresSenderDomainRecheck,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Default)]
struct MockVerifier {
    enabled: AtomicBool,
}

impl MockVerifier {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }
}

impl EmailDomainVerifier for MockVerifier {
    fn verify_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EmailDomainStatus, CloudflareEmailError>> + Send + 'a>>
    {
        assert_eq!(domain, "mail.example.com");
        let enabled = self.enabled.load(Ordering::SeqCst);
        Box::pin(ready(Ok(EmailDomainStatus {
            zone_id: Some("zone-mail".to_owned()),
            enabled,
        })))
    }
}

#[derive(Clone, Default)]
struct Hooks {
    code: Arc<Mutex<Option<String>>>,
    verifier: Arc<MockVerifier>,
}

impl DashboardControlHooks for Hooks {
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let verifier = Arc::clone(&self.verifier);
        let code = Arc::clone(&self.code);
        async move {
            let result = match operation {
                DashboardControlHookOperation::DefaultEmailTemplate => json!({
                    "sender-email": "hello@example.com",
                    "subject": "{code} for {app_title}",
                    "body": "Use {code}.",
                    "verification-code-expiry-minutes": 1440,
                }),
                DashboardControlHookOperation::AuthorizeEmailSend { .. } => Value::Bool(true),
                DashboardControlHookOperation::DeliverDashboardEmail { subject, .. } => {
                    *code.lock().unwrap() = subject.split_whitespace().next().map(str::to_owned);
                    Value::Bool(true)
                }
                DashboardControlHookOperation::VerifyEmailDomain { domain, .. } => {
                    let status = verifier
                        .verify_domain(&domain)
                        .await
                        .map_err(|error| ApiError::new(ApiErrorType::Unknown, error.to_string()))?;
                    json!({
                        "enabled": status.enabled,
                        "zone-id": status.zone_id,
                        "error": Value::Null,
                    })
                }
                _ => Value::Null,
            };
            Ok(result)
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn request(method: Method, path: &str, token: &str, value: Value) -> DashboardRequest {
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
        body: Bytes::from(serde_json::to_vec(&value).unwrap()),
        staged_body_path: None,
        remote_addr: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn body(response: DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the whole verification flow in one database session.
async fn cloudflare_sender_is_used_only_while_the_domain_is_enabled() {
    let Some(database_url) = instant_test_support::test_database_url(
        "cloudflare_sender_is_used_only_while_the_domain_is_enabled",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TEMP TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL UNIQUE);
             CREATE TEMP TABLE instant_personal_access_tokens (
               id uuid PRIMARY KEY, user_id uuid NOT NULL, name text NOT NULL,
               lookup_key bytea NOT NULL UNIQUE, created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY, creator_id uuid REFERENCES instant_users(id),
               org_id uuid, title text NOT NULL, created_at timestamp NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'active', magic_code_expiry_minutes integer,
               deletion_marked_at timestamp
             );
             CREATE TEMP TABLE app_members (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, user_id uuid NOT NULL,
               member_role text NOT NULL
             );
             CREATE TEMP TABLE app_email_senders (
               id uuid PRIMARY KEY, postmark_id integer, email text NOT NULL UNIQUE,
               name text NOT NULL, email_provider text NOT NULL DEFAULT 'postmark'
             );
             CREATE TEMP TABLE app_email_templates (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, email_type text NOT NULL,
               sender_id uuid, body text NOT NULL, subject text NOT NULL, name text,
               UNIQUE (app_id, email_type)
             );
             CREATE TEMP TABLE app_email_verifications (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, sender_id uuid NOT NULL,
               verified boolean NOT NULL DEFAULT false, UNIQUE (app_id, sender_id)
             );
             CREATE TEMP TABLE app_email_verification_codes (
               id uuid PRIMARY KEY, code text NOT NULL, app_id uuid NOT NULL,
               verification_id uuid NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE app_sender_domains (
               app_id uuid NOT NULL, domain text NOT NULL, cloudflare_zone_id text,
               verified_at timestamp with time zone,
               ownership_verified_at timestamp with time zone,
               last_checked_at timestamp with time zone NOT NULL DEFAULT now(),
               last_error text, PRIMARY KEY (app_id, domain)
             );",
        )
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let token = format!("per_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'owner@example.com')",
            &[&user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_personal_access_tokens (id, user_id, name, lookup_key)
             VALUES ($1, $2, 'test', $3)",
            &[
                &Uuid::new_v4(),
                &user_id,
                &Sha256::digest(token.as_bytes()).to_vec(),
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'Sender Test')",
            &[&app_id, &user_id],
        )
        .await
        .unwrap();

    let hooks = Hooks::default();
    let verifier = Arc::clone(&hooks.verifier);
    let code = Arc::clone(&hooks.code);
    let service = DashboardControlService::new(client, hooks);

    let template = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/email_templates"),
                &token,
                json!({
                    "email-type": "magic-code",
                    "subject": "{code} for {app_title}",
                    "body": "Use {code}.",
                    "sender-email": "custom@mail.example.com",
                    "sender-name": "Sender Test"
                }),
            ))
            .await
            .unwrap(),
    );
    assert!(template["id"].as_str().is_some());

    let status = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/sender-verification"),
                &token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(status["domain"]["domain"], "mail.example.com");
    assert_eq!(status["domain"]["cloudflare_enabled"], false);
    assert_eq!(status["domain"]["ownership_verified"], false);
    assert_eq!(status["domain"]["verified"], false);

    verifier.set_enabled(false);
    send_code(&service, app_id, &token).await;
    let submitted = code.lock().unwrap().clone().unwrap();
    let verification = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/sender-verification/verify-magic-code"),
                &token,
                json!({"code": submitted}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(verification["verified"], false);
    assert_eq!(verification["cloudflare-enabled"], false);
    let status = sender_status(&service, app_id, &token).await;
    assert_eq!(status["ownership_verified"], true);
    assert_eq!(status["cloudflare_enabled"], false);
    assert_eq!(status["verified"], false);

    verifier.set_enabled(true);
    send_code(&service, app_id, &token).await;
    let submitted = code.lock().unwrap().clone().unwrap();
    let verification = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/sender-verification/verify-magic-code"),
                &token,
                json!({"code": submitted}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(verification["verified"], true);
    assert_eq!(verification["cloudflare-enabled"], true);
    let status = sender_status(&service, app_id, &token).await;
    assert_eq!(status["ownership_verified"], true);
    assert_eq!(status["cloudflare_enabled"], true);
    assert_eq!(status["verified"], true);

    // Changing the sender address invalidates ownership until it is verified again.
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/email_templates"),
            &token,
            json!({
                "email-type": "magic-code",
                "subject": "{code} for {app_title}",
                "body": "Use {code}.",
                "sender-email": "other@mail.example.com",
                "sender-name": "Sender Test"
            }),
        ))
        .await
        .unwrap();
    let status = sender_status(&service, app_id, &token).await;
    assert_eq!(status["ownership_verified"], false);
    assert_eq!(status["verified"], false);

    drop(service);
    connection_task.await.unwrap().unwrap();
}

async fn send_code(service: &DashboardControlService<Hooks>, app_id: Uuid, token: &str) {
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/sender-verification/send-magic-code"),
            token,
            json!({}),
        ))
        .await
        .unwrap();
}

async fn sender_status(
    service: &DashboardControlService<Hooks>,
    app_id: Uuid,
    token: &str,
) -> Value {
    body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/sender-verification"),
                token,
                json!({}),
            ))
            .await
            .unwrap(),
    )["domain"]
        .clone()
}

#[tokio::test]
async fn daily_recheck_clears_the_verification_when_cloudflare_disables_the_domain() {
    let Some(database_url) = instant_test_support::test_database_url(
        "daily_recheck_clears_the_verification_when_cloudflare_disables_the_domain",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let schema = format!("sender_domains_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             SET search_path TO {schema};
             CREATE TABLE apps (id uuid PRIMARY KEY);
             CREATE TABLE app_sender_domains (
               app_id uuid NOT NULL, domain text NOT NULL, cloudflare_zone_id text,
               verified_at timestamp with time zone,
               ownership_verified_at timestamp with time zone,
               last_checked_at timestamp with time zone NOT NULL DEFAULT now(),
               last_error text, PRIMARY KEY (app_id, domain)
             );"
        ))
        .await
        .unwrap();
    let owned_app = Uuid::new_v4();
    let unowned_app = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO apps (id) VALUES ($1), ($2)",
            &[&owned_app, &unowned_app],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO app_sender_domains
               (app_id, domain, verified_at, ownership_verified_at)
             VALUES ($1, 'mail.example.com', now(), now()),
                    ($2, 'mail.example.com', now(), NULL)",
            &[&owned_app, &unowned_app],
        )
        .await
        .unwrap();

    let mock = Arc::new(MockVerifier::default());
    mock.set_enabled(true);
    let verifier: Arc<dyn EmailDomainVerifier> = mock.clone();
    let recheck_client = connect_with_schema(&database_url, &schema).await;
    let outcomes = PostgresSenderDomainRecheck::new(recheck_client, Arc::clone(&verifier))
        .run()
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|outcome| outcome.error.is_none()));
    let rows = setup
        .query(
            "SELECT app_id, verified_at IS NOT NULL, cloudflare_zone_id
             FROM app_sender_domains ORDER BY app_id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let app_id: Uuid = row.get(0);
        assert_eq!(row.get::<_, String>(2), "zone-mail");
        assert_eq!(row.get::<_, bool>(1), app_id == owned_app);
    }

    mock.set_enabled(false);
    let recheck_client = connect_with_schema(&database_url, &schema).await;
    let outcomes = PostgresSenderDomainRecheck::new(recheck_client, Arc::clone(&verifier))
        .run()
        .await
        .unwrap();
    assert!(outcomes.iter().all(|outcome| !outcome.verified));
    let cleared: i64 = setup
        .query_one(
            "SELECT count(*) FROM app_sender_domains WHERE verified_at IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(cleared, 0);

    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(setup);
    connection_task.await.unwrap().unwrap();
}

async fn connect_with_schema(database_url: &str, schema: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    client
}
