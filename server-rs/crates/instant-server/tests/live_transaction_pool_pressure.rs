use instant_http::ApiErrorType;
use instant_server::{
    PostgresPool, PostgresTransactionPoolPressurePolicy, TransactionPoolPressurePolicy,
};
use serde_json::json;
use tokio_postgres::{Client, NoTls, types::Json};
use uuid::Uuid;

async fn client(database_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps one isolated live flag-and-capacity fixture auditable.
async fn permissioned_transaction_pressure_uses_live_controls_and_an_isolated_reader() {
    let Some(database_url) = instant_test_support::test_database_url(
        "permissioned_transaction_pressure_uses_live_controls_and_an_isolated_reader",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("transaction_pool_pressure_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );"
        ))
        .await
        .unwrap();

    let config_app_id = Uuid::new_v4();
    let toggle_setting_attr = Uuid::new_v4();
    let toggled_attr = Uuid::new_v4();
    let flag_setting_attr = Uuid::new_v4();
    let flag_value_attr = Uuid::new_v4();
    let toggle_entity = Uuid::new_v4();
    let flag_entity = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.attrs (id, app_id, etype, label) VALUES
                   ($1, $5, 'toggles', 'setting'), ($2, $5, 'toggles', 'toggled'),
                   ($3, $5, 'flags', 'setting'), ($4, $5, 'flags', 'value')"
            ),
            &[
                &toggle_setting_attr,
                &toggled_attr,
                &flag_setting_attr,
                &flag_value_attr,
                &config_app_id,
            ],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                   ($1, $2, $3, $4), ($1, $2, $5, $6),
                   ($1, $7, $8, $9), ($1, $7, $10, $11)"
            ),
            &[
                &config_app_id,
                &toggle_entity,
                &toggle_setting_attr,
                &Json(json!("rate-limit-tx-based-on-conn-pool?")),
                &toggled_attr,
                &Json(json!(true)),
                &flag_entity,
                &flag_setting_attr,
                &Json(json!("rate-limit-tx-based-on-conn-pool-buffer")),
                &flag_value_attr,
                &Json(json!(2)),
            ],
        )
        .await
        .unwrap();

    let config_client = client(&database_url).await;
    config_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let observed_pool = PostgresPool::connect(&database_url, 3).await.unwrap();
    let policy = PostgresTransactionPoolPressurePolicy::new(
        observed_pool.clone(),
        config_client,
        Some("admin@example.test".to_owned()),
    )
    .with_config_app_id(config_app_id);
    let first = observed_pool.acquire().await.unwrap();
    let second = observed_pool.acquire().await.unwrap();
    let third = observed_pool.acquire().await.unwrap();

    let error = policy.check(Uuid::new_v4()).await.unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::RateLimited);
    assert_eq!(
        error.body().message,
        "You're making too many requests. Contact the deployment administrator at admin@example.test."
    );
    drop(first);
    assert!(policy.check(Uuid::new_v4()).await.is_err());
    drop(second);
    policy.check(Uuid::new_v4()).await.unwrap();

    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = 'false'::jsonb
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[&config_app_id, &toggle_entity, &toggled_attr],
        )
        .await
        .unwrap();
    let replacement = observed_pool.acquire().await.unwrap();
    policy.check(Uuid::new_v4()).await.unwrap();

    drop(replacement);
    drop(third);
    drop(policy);

    let separator = if database_url.contains('?') { '&' } else { '?' };
    let pool_url = format!("{database_url}{separator}options=-csearch_path%3D{schema}");
    let control_pool = PostgresPool::connect(&pool_url, 1).await.unwrap();
    let mut pooled_policy = PostgresTransactionPoolPressurePolicy::from_pools(
        observed_pool.clone(),
        control_pool.clone(),
        Some("admin@example.test".to_owned()),
    )
    .with_config_app_id(config_app_id);
    for _ in 0..8 {
        pooled_policy.check(Uuid::new_v4()).await.unwrap();
    }
    let lease = control_pool.acquire().await.unwrap();
    let prepared: i64 = lease.query_one(
        "SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE 'SELECT ''toggle'', toggle_value.value%'",
        &[],
    ).await.unwrap().get(0);
    assert_eq!(
        prepared, 1,
        "one connection-local policy statement must be reused"
    );
    drop(lease);

    // A cached plan must read current values and remain scoped to this config app.
    setup
        .execute(
            &format!("UPDATE {schema}.triples SET value='true'::jsonb WHERE attr_id=$1"),
            &[&toggled_attr],
        )
        .await
        .unwrap();
    let first = observed_pool.acquire().await.unwrap();
    let second = observed_pool.acquire().await.unwrap();
    assert_eq!(
        pooled_policy
            .check(Uuid::new_v4())
            .await
            .unwrap_err()
            .body()
            .kind,
        ApiErrorType::RateLimited
    );
    pooled_policy = pooled_policy.with_config_app_id(Uuid::new_v4());
    pooled_policy.check(Uuid::new_v4()).await.unwrap();
    pooled_policy = pooled_policy.with_config_app_id(config_app_id);
    assert!(pooled_policy.check(Uuid::new_v4()).await.is_err());
    drop(first);
    drop(second);
    let lease = control_pool.acquire().await.unwrap();
    drop(deadpool_postgres::Object::take(lease));
    pooled_policy.check(Uuid::new_v4()).await.unwrap();
    setup
        .execute(
            &format!("UPDATE {schema}.triples SET value='\"invalid\"'::jsonb WHERE attr_id=$1"),
            &[&flag_value_attr],
        )
        .await
        .unwrap();
    assert!(pooled_policy.check(Uuid::new_v4()).await.is_err());
    drop(pooled_policy);
    drop(control_pool);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
