use std::sync::Arc;

use instant_http::ApiErrorType;
use instant_permissions::RateLimitConsumer;
use instant_server::{
    MagicCodeRateLimiter, PostgresMagicCodeRateLimiter, PostgresRateLimitConsumer,
};
use serde_json::{Value, json};
use tokio_postgres::{Client, NoTls, types::Json};
use uuid::Uuid;

async fn client(database_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[derive(Clone, Copy)]
struct ConfigRows {
    app: Uuid,
    flag_entity: Uuid,
    flag_value_attr: Uuid,
    daily_entity: Uuid,
    ip_entity: Uuid,
    toggle_entity: Uuid,
    toggle_value_attr: Uuid,
}

async fn create_schema(setup: &Client, schema: &str) -> ConfigRows {
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.rate_limit_keys (
               key uuid PRIMARY KEY, value bytea NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );"
        ))
        .await
        .unwrap();
    let rows = ConfigRows {
        app: Uuid::new_v4(),
        flag_entity: Uuid::new_v4(),
        flag_value_attr: Uuid::new_v4(),
        daily_entity: Uuid::new_v4(),
        ip_entity: Uuid::new_v4(),
        toggle_entity: Uuid::new_v4(),
        toggle_value_attr: Uuid::new_v4(),
    };
    let flag_setting_attr = Uuid::new_v4();
    let toggle_setting_attr = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.attrs (id, app_id, etype, label) VALUES
                   ($1, $5, 'flags', 'setting'), ($2, $5, 'flags', 'value'),
                   ($3, $5, 'toggles', 'setting'), ($4, $5, 'toggles', 'toggled')"
            ),
            &[
                &flag_setting_attr,
                &rows.flag_value_attr,
                &toggle_setting_attr,
                &rows.toggle_value_attr,
                &rows.app,
            ],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                   ($1, $2, $3, $4), ($1, $2, $5, $6),
                   ($1, $7, $3, $8), ($1, $7, $5, $9),
                   ($1, $10, $3, $11), ($1, $10, $5, $12),
                   ($1, $13, $14, $15), ($1, $13, $16, $17)"
            ),
            &[
                &rows.app,
                &rows.flag_entity,
                &flag_setting_attr,
                &Json(json!("magic-code-rate-limit-per-hour")),
                &rows.flag_value_attr,
                &Json(json!(2)),
                &rows.daily_entity,
                &Json(json!("magic-code-daily-limit-per-app")),
                &Json(json!(4)),
                &rows.ip_entity,
                &Json(json!("magic-code-rate-limit-per-ip-hour")),
                &Json(json!(2)),
                &rows.toggle_entity,
                &toggle_setting_attr,
                &Json(json!("use-bucket4j")),
                &rows.toggle_value_attr,
                &Json(json!(true)),
            ],
        )
        .await
        .unwrap();
    rows
}

async fn set_config_value(
    setup: &Client,
    schema: &str,
    app: Uuid,
    entity: Uuid,
    attr: Uuid,
    value: Value,
) {
    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[&app, &entity, &attr, &Json(value)],
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn runtime_magic_code_limits_follow_live_jvm_configuration() {
    let Some(url) = instant_test_support::test_database_url(
        "runtime_magic_code_limits_follow_live_jvm_configuration",
    ) else {
        return;
    };
    let setup = client(&url).await;
    let schema = format!("magic_code_limit_{}", Uuid::new_v4().simple());
    let rows = create_schema(&setup, &schema).await;
    let config_client = client(&url).await;
    let store_client = client(&url).await;
    config_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    store_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let hmac_key: Arc<[u8]> = Arc::from(b"magic-code-live-test-key".as_slice());
    let consumer: Arc<dyn RateLimitConsumer> =
        Arc::new(PostgresRateLimitConsumer::new(store_client, hmac_key));
    let limiter = PostgresMagicCodeRateLimiter::new(config_client, Arc::clone(&consumer))
        .with_config_app_id(rows.app)
        .with_verify_ip_hourly_limit(2);
    let app = Uuid::new_v4();
    let email = " USER@Example.com ";

    limiter.check_send(app, email).await.unwrap();
    limiter.check_send(app, email).await.unwrap();
    let error = limiter.check_send(app, email).await.unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::RateLimited);
    assert_eq!(
        error.body().message,
        "Too many verification codes requested for this email. Please try again later."
    );
    limiter.check_verify(app, email).await.unwrap();
    limiter.check_verify(app, email).await.unwrap();
    assert_eq!(
        limiter
            .check_verify(app, email)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited
    );
    limiter.check_custom_sender(email).await.unwrap();
    limiter.check_custom_sender(email).await.unwrap();
    assert_eq!(
        limiter
            .check_custom_sender(email)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited
    );

    set_config_value(
        &setup,
        &schema,
        rows.app,
        rows.flag_entity,
        rows.flag_value_attr,
        json!(3),
    )
    .await;
    limiter.check_send(app, email).await.unwrap();
    set_config_value(
        &setup,
        &schema,
        rows.app,
        rows.toggle_entity,
        rows.toggle_value_attr,
        json!(false),
    )
    .await;
    let fallback_email = "fallback@example.com";
    for _ in 0..3 {
        limiter.check_send(app, fallback_email).await.unwrap();
    }
    assert_eq!(
        limiter
            .check_send(app, fallback_email)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited
    );
    for _ in 0..5 {
        limiter.check_verify(app, fallback_email).await.unwrap();
        limiter.check_custom_sender(fallback_email).await.unwrap();
    }

    let daily_app = Uuid::new_v4();
    for email in [
        "daily-1@example.com",
        "daily-2@example.com",
        "daily-3@example.com",
        "daily-4@example.com",
    ] {
        limiter.check_send(daily_app, email).await.unwrap();
    }
    assert_eq!(
        limiter
            .check_send(daily_app, "daily-5@example.com")
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited,
        "the per-app daily quota must reject the fifth send"
    );

    let ip_app = Uuid::new_v4();
    let ip = Some("203.0.113.9");
    limiter
        .check_send_from_ip(ip_app, "ip-1@example.com", ip)
        .await
        .unwrap();
    limiter
        .check_send_from_ip(ip_app, "ip-2@example.com", ip)
        .await
        .unwrap();
    assert_eq!(
        limiter
            .check_send_from_ip(ip_app, "ip-3@example.com", ip)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited,
        "the per-client-IP hourly quota must reject the third send"
    );

    set_config_value(
        &setup,
        &schema,
        rows.app,
        rows.toggle_entity,
        rows.toggle_value_attr,
        json!(true),
    )
    .await;
    let verify_ip = Some("203.0.113.10");
    for email in ["verify-ip-1@example.com", "verify-ip-2@example.com"] {
        limiter
            .check_verify_from_ip(ip_app, email, verify_ip)
            .await
            .unwrap();
    }
    assert_eq!(
        limiter
            .check_verify_from_ip(ip_app, "verify-ip-3@example.com", verify_ip)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited,
        "the per-client-IP verify quota must reject the third verify"
    );

    let failure_app = Uuid::new_v4();
    let failure_email = "failures@example.com";
    for _ in 0..4 {
        assert!(
            !limiter
                .record_verify_failure(failure_app, failure_email)
                .await
                .unwrap()
        );
    }
    assert!(
        limiter
            .record_verify_failure(failure_app, failure_email)
            .await
            .unwrap(),
        "the fifth failure locks out the pair"
    );
    assert!(
        limiter
            .record_verify_failure(failure_app, failure_email)
            .await
            .unwrap(),
        "the lockout persists until a reset"
    );
    limiter
        .reset_verify_failures(failure_app, failure_email)
        .await
        .unwrap();
    assert!(
        !limiter
            .record_verify_failure(failure_app, failure_email)
            .await
            .unwrap(),
        "a successful verify or a new send resets the failure counter"
    );

    drop(limiter);
    drop(consumer);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
