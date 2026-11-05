use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use instant_http::{
    ApiError, ApiErrorType, DashboardRequest, RuntimeAuthBackend, SendMagicCodeRequest,
    VerifyMagicCodeRequest,
};
use instant_instaql::system_attribute_catalog;
use instant_server::{
    AllowAllSignups, DashboardControlService, DashboardSubsystem, MagicCodeDelivery,
    MagicCodeRateLimitFuture, MagicCodeRateLimiter, MagicCodeVerifyFailureFuture,
    NoopDashboardControlHooks, PostgresPool, RuntimeAuthConfig, RuntimeAuthService,
    StaticRuntimeAuthPolicy, ensure_system_catalog, migrate_database,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Debug, Default)]
struct RecordingDelivery {
    codes: Mutex<Vec<String>>,
}

impl MagicCodeDelivery for RecordingDelivery {
    fn deliver(
        &self,
        _app_id: Uuid,
        _email: &str,
        code: &str,
        _expiry_minutes: u64,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        self.codes.lock().unwrap().push(code.to_owned());
        std::future::ready(Ok(()))
    }
}

fn dashboard_request(app_id: Uuid, token: Uuid, email: &str, code: &str) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    DashboardRequest {
        method: Method::POST,
        path: format!("/dash/apps/{app_id}/test_users"),
        query: None,
        headers,
        body: Bytes::from(serde_json::to_vec(&json!({"email": email, "code": code})).unwrap()),
        staged_body_path: None,
        remote_addr: None,
    }
}

fn verify_request(app_id: Uuid, email: &str, code: &str) -> VerifyMagicCodeRequest {
    VerifyMagicCodeRequest {
        app_id,
        email: email.to_owned(),
        code: code.to_owned(),
        refresh_token: None,
        extra_fields: BTreeMap::new(),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn migrated_dashboard_test_users_are_honored_by_runtime_auth() {
    let Some(database_url) = instant_test_support::test_database_url(
        "migrated_dashboard_test_users_are_honored_by_runtime_auth",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    ensure_system_catalog(&mut setup).await.unwrap();

    let creator_id = Uuid::new_v4();
    let refresh_token = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id) VALUES ($1, $2, $3)",
            &[
                &refresh_token,
                &Sha256::digest(refresh_token.to_string().as_bytes()).to_vec(),
                &creator_id,
            ],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'test-user-auth')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
    let dashboard = DashboardControlService::from_pool(pool.clone(), NoopDashboardControlHooks);
    let created = dashboard
        .handle(dashboard_request(
            app_id,
            refresh_token,
            " Static@Example.com ",
            "424242",
        ))
        .await
        .unwrap();
    let created: Value = serde_json::from_slice(created.body.buffered().unwrap()).unwrap();
    assert_eq!(created["test-user"]["email"], "static@example.com");

    let duplicate = dashboard
        .handle(dashboard_request(
            app_id,
            refresh_token,
            "STATIC@example.com",
            "111111",
        ))
        .await
        .unwrap_err();
    assert_eq!(duplicate.status().as_u16(), 400);
    assert_eq!(duplicate.body().kind, ApiErrorType::RecordNotUnique);
    assert_eq!(
        serde_json::to_value(duplicate.body()).unwrap(),
        json!({
            "type": "record-not-unique",
            "message": "Record not unique: app-test-user",
            "hint": {"record-type": "app-test-user"}
        })
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM app_test_users WHERE app_id = $1 AND email = $2",
                &[&app_id, &"static@example.com"],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );

    let delivery = Arc::new(RecordingDelivery::default());
    let runtime = RuntimeAuthService::from_pool(
        pool.clone(),
        Arc::clone(&delivery),
        StaticRuntimeAuthPolicy::new(system_attribute_catalog().unwrap(), AllowAllSignups),
        RuntimeAuthConfig::default(),
    );
    let session = runtime
        .verify_magic_code(verify_request(app_id, "static@example.com", "424242"))
        .await
        .unwrap();
    assert_eq!(session.user.email.as_deref(), Some("static@example.com"));
    let wrong = runtime
        .verify_magic_code(verify_request(app_id, "static@example.com", "000000"))
        .await
        .unwrap_err();
    assert_eq!(wrong.status().as_u16(), 400);
    assert_eq!(wrong.body().kind, ApiErrorType::RecordNotFound);

    runtime
        .send_magic_code(SendMagicCodeRequest {
            app_id,
            email: "static@example.com".to_owned(),
        })
        .await
        .unwrap();
    let generated_code = delivery.codes.lock().unwrap()[0].clone();
    let generated = runtime
        .verify_magic_code(verify_request(
            app_id,
            "static@example.com",
            &generated_code,
        ))
        .await
        .unwrap();
    assert_eq!(generated.user.id, session.user.id);

    setup
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await
        .unwrap();
    drop(runtime);
    drop(dashboard);
    drop(pool);
    drop(setup);
    connection_task.await.unwrap().unwrap();
}

#[derive(Debug, Default)]
struct LockoutLimiter {
    failures: Mutex<u32>,
}

impl MagicCodeRateLimiter for LockoutLimiter {
    fn check_send<'a>(&'a self, _app_id: Uuid, _email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn check_verify<'a>(&'a self, _app_id: Uuid, _email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn record_verify_failure<'a>(
        &'a self,
        _app_id: Uuid,
        _email: &'a str,
    ) -> MagicCodeVerifyFailureFuture<'a> {
        Box::pin(std::future::ready(Ok({
            let mut failures = self.failures.lock().unwrap();
            *failures = failures.saturating_add(1);
            *failures >= 5
        })))
    }

    fn reset_verify_failures<'a>(
        &'a self,
        _app_id: Uuid,
        _email: &'a str,
    ) -> MagicCodeRateLimitFuture<'a> {
        *self.failures.lock().unwrap() = 0;
        Box::pin(std::future::ready(Ok(())))
    }

    fn check_custom_sender<'a>(&'a self, _email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn newer_magic_codes_replace_older_ones_and_failures_invalidate_the_code() {
    let Some(database_url) = instant_test_support::test_database_url(
        "newer_magic_codes_replace_older_ones_and_failures_invalidate_the_code",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    ensure_system_catalog(&mut setup).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'magic-code-lockout')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
    let delivery = Arc::new(RecordingDelivery::default());
    let limiter = Arc::new(LockoutLimiter::default());
    let runtime = RuntimeAuthService::from_pool(
        pool.clone(),
        Arc::clone(&delivery),
        StaticRuntimeAuthPolicy::new(system_attribute_catalog().unwrap(), AllowAllSignups),
        RuntimeAuthConfig::default(),
    )
    .with_magic_code_rate_limiter(limiter.clone());
    let email = "rotating@example.com";

    runtime
        .send_magic_code(SendMagicCodeRequest {
            app_id,
            email: email.to_owned(),
        })
        .await
        .unwrap();
    runtime
        .send_magic_code(SendMagicCodeRequest {
            app_id,
            email: email.to_owned(),
        })
        .await
        .unwrap();
    let (first, second) = {
        let codes = delivery.codes.lock().unwrap();
        (codes[0].clone(), codes[1].clone())
    };
    let stale = runtime
        .verify_magic_code(verify_request(app_id, email, &first))
        .await
        .unwrap_err();
    assert_eq!(stale.body().kind, ApiErrorType::RecordNotFound);
    runtime
        .verify_magic_code(verify_request(app_id, email, &second))
        .await
        .unwrap();
    assert_eq!(
        *limiter.failures.lock().unwrap(),
        0,
        "a successful verify resets the failure counter"
    );

    runtime
        .send_magic_code(SendMagicCodeRequest {
            app_id,
            email: email.to_owned(),
        })
        .await
        .unwrap();
    let code = delivery.codes.lock().unwrap()[2].clone();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        let error = runtime
            .verify_magic_code(verify_request(app_id, email, wrong))
            .await
            .unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::RecordNotFound);
    }
    let locked = runtime
        .verify_magic_code(verify_request(app_id, email, &code))
        .await
        .unwrap_err();
    assert_eq!(locked.body().kind, ApiErrorType::RecordNotFound);

    drop(runtime);
    drop(pool);
    setup
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await
        .unwrap();
    drop(setup);
    connection_task.await.unwrap().unwrap();
}

#[derive(Debug)]
struct BlockingDelivery {
    blocked_app: Uuid,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl MagicCodeDelivery for BlockingDelivery {
    fn deliver(
        &self,
        app_id: Uuid,
        _email: &str,
        _code: &str,
        _expiry_minutes: u64,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        let blocked = app_id == self.blocked_app;
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        async move {
            if blocked {
                entered.notify_one();
                release.notified().await;
            }
            Ok(())
        }
    }
}

#[tokio::test]
async fn runtime_send_releases_its_connection_before_delivery() {
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_send_releases_its_connection_before_delivery",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    ensure_system_catalog(&mut setup).await.unwrap();

    let creator_id = Uuid::new_v4();
    let blocked_app = Uuid::new_v4();
    let other_app = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    for app_id in [blocked_app, other_app] {
        setup
            .execute(
                "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'delivery-pool')",
                &[&app_id, &creator_id],
            )
            .await
            .unwrap();
    }

    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let delivery = Arc::new(BlockingDelivery {
        blocked_app,
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let runtime = Arc::new(RuntimeAuthService::from_pool(
        pool,
        Arc::clone(&delivery),
        StaticRuntimeAuthPolicy::new(system_attribute_catalog().unwrap(), AllowAllSignups),
        RuntimeAuthConfig::default(),
    ));

    let first = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move {
            runtime
                .send_magic_code(SendMagicCodeRequest {
                    app_id: blocked_app,
                    email: "blocked@example.com".to_owned(),
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), delivery.entered.notified())
        .await
        .expect("the blocking delivery never started");
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.send_magic_code(SendMagicCodeRequest {
            app_id: other_app,
            email: "other@example.com".to_owned(),
        }),
    )
    .await
    .expect("runtime auth held the only pool connection across delivery");
    second.unwrap();
    delivery.release.notify_one();
    first.await.unwrap().unwrap();

    setup
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await
        .unwrap();
    drop(runtime);
    drop(setup);
    connection_task.await.unwrap().unwrap();
}
