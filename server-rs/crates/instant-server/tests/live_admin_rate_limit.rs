use std::future::Future;
use std::sync::Arc;

use instant_http::{
    AdminActor, AdminAuthBackend, AdminMagicCodeRequest, AdminRequestContext, ApiError,
    ApiErrorType,
};
use instant_permissions::RateLimitConsumer;
use instant_server::{
    AdminAuthService, MagicCodeDelivery, PostgresAdminDataAuthorizer, PostgresMagicCodeRateLimiter,
    PostgresPool, PostgresRateLimitConsumer, RuntimeAuthConfig, ensure_system_catalog,
    migrate_database,
};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Debug, Default)]
struct RecordingDelivery;

impl MagicCodeDelivery for RecordingDelivery {
    fn deliver(
        &self,
        _app_id: Uuid,
        _email: &str,
        _code: &str,
        _expiry_minutes: u64,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        std::future::ready(Ok(()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn admin_send_magic_code_is_rate_limited() {
    let Some(database_url) =
        instant_test_support::test_database_url("admin_send_magic_code_is_rate_limited")
    else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    ensure_system_catalog(&mut setup).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let admin_token = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'admin-rate-limit')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&admin_token, &app_id],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&database_url, 4).await.unwrap();
    let rate_limit_pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let hmac_key: Arc<[u8]> = Arc::from(b"admin-magic-code-live-test".as_slice());
    let consumer: Arc<dyn RateLimitConsumer> = Arc::new(PostgresRateLimitConsumer::from_pool(
        rate_limit_pool,
        hmac_key,
    ));
    let limiter = Arc::new(PostgresMagicCodeRateLimiter::from_pool(
        pool.clone(),
        consumer,
    ));
    let service = AdminAuthService::from_pool(
        pool.clone(),
        Arc::new(PostgresAdminDataAuthorizer::from_pool(pool.clone())),
        RecordingDelivery,
        RuntimeAuthConfig::default(),
    )
    .with_magic_code_rate_limiter(limiter);

    let context = AdminRequestContext {
        app_id,
        bearer_token: Some(admin_token.to_string()),
        actor: AdminActor::Admin,
        client_ip: None,
    };
    let request = || AdminMagicCodeRequest {
        email: "limited@example.com".to_owned(),
    };
    for _ in 0..20 {
        service
            .send_magic_code(context.clone(), request())
            .await
            .unwrap();
    }
    let error = service
        .send_magic_code(context, request())
        .await
        .unwrap_err();
    assert_eq!(error.status().as_u16(), 429);
    assert_eq!(error.body().kind, ApiErrorType::RateLimited);

    setup
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await
        .unwrap();
    drop(service);
    drop(pool);
    drop(setup);
    connection_task.await.unwrap().unwrap();
}
