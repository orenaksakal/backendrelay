use std::sync::Arc;

use instant_http::ApiErrorType;
use instant_permissions::{InMemoryRateLimiter, RateLimitConsumer};
use instant_server::{
    AdminRequestOperation, AdminRequestRateLimitPolicy, PostgresAdminRequestRateLimitPolicy,
    PostgresPool,
};
use serde_json::{Value, json};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

struct FlagFixture {
    schema: String,
    config_app_id: Uuid,
    global_entity: Uuid,
    value_attr: Uuid,
}

fn limit(capacity: u64) -> Value {
    json!({
        "limits": [{
            "capacity": capacity,
            "refill": {"period": "1 hour", "amount": capacity, "type": "interval"}
        }]
    })
}

fn scoped_url(database_url: &str, schema: &str) -> String {
    let mut url = url::Url::parse(database_url).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    url.to_string()
}

async fn create_flag_fixture(
    client: &Client,
    override_app: Uuid,
    fallback_app: Uuid,
) -> FlagFixture {
    let schema = format!("admin_request_rate_limit_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
             CREATE TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs VALUES
             ($1, $2, 'flags', 'setting'), ($3, $2, 'flags', 'value')",
            &[&setting_attr, &config_app_id, &value_attr],
        )
        .await
        .unwrap();
    let global_entity = Uuid::new_v4();
    let per_app_entity = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO triples VALUES
             ($1, $2, $3, to_jsonb('global-admin-rate-limit'::text)),
             ($1, $2, $4, $5),
             ($1, $6, $3, to_jsonb('admin-rate-limit-for-app'::text)),
             ($1, $6, $4, $7)",
            &[
                &config_app_id,
                &global_entity,
                &setting_attr,
                &value_attr,
                &limit(1),
                &per_app_entity,
                &json!({override_app.to_string(): limit(2), fallback_app.to_string(): false}),
            ],
        )
        .await
        .unwrap();
    FlagFixture {
        schema,
        config_app_id,
        global_entity,
        value_attr,
    }
}

#[tokio::test]
async fn admin_request_limits_load_live_per_app_and_global_flags() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_request_limits_load_live_per_app_and_global_flags",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let global_app = Uuid::new_v4();
    let override_app = Uuid::new_v4();
    let fallback_app = Uuid::new_v4();
    let unlimited_app = Uuid::new_v4();
    let fixture = create_flag_fixture(&client, override_app, fallback_app).await;

    let scoped_database_url = scoped_url(&database_url, &fixture.schema);
    let request_pool = PostgresPool::connect(&scoped_database_url, 1)
        .await
        .unwrap();
    let _held_request = request_pool.acquire().await.unwrap();
    let config_pool = PostgresPool::connect(&scoped_database_url, 1)
        .await
        .unwrap();
    let consumer: Arc<dyn RateLimitConsumer> = Arc::new(InMemoryRateLimiter::default());
    let policy = PostgresAdminRequestRateLimitPolicy::from_pool(config_pool, consumer)
        .with_config_app_id(fixture.config_app_id);

    policy
        .check(global_app, AdminRequestOperation::Query)
        .await
        .unwrap();
    let error = policy
        .check(global_app, AdminRequestOperation::Query)
        .await
        .unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::RateLimited);
    assert_eq!(error.status().as_u16(), 429);
    policy
        .check(global_app, AdminRequestOperation::Transact)
        .await
        .unwrap();

    for _ in 0..2 {
        policy
            .check(override_app, AdminRequestOperation::Query)
            .await
            .unwrap();
    }
    assert_eq!(
        policy
            .check(override_app, AdminRequestOperation::Query)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited
    );

    policy
        .check(fallback_app, AdminRequestOperation::Query)
        .await
        .unwrap();
    assert_eq!(
        policy
            .check(fallback_app, AdminRequestOperation::Query)
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited
    );

    client
        .execute(
            "UPDATE triples SET value = 'null'::jsonb
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[
                &fixture.config_app_id,
                &fixture.global_entity,
                &fixture.value_attr,
            ],
        )
        .await
        .unwrap();
    for _ in 0..3 {
        policy
            .check(unlimited_app, AdminRequestOperation::Query)
            .await
            .unwrap();
    }

    client
        .batch_execute(&format!("DROP SCHEMA {} CASCADE", fixture.schema))
        .await
        .unwrap();
}
