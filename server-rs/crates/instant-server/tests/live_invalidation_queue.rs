use std::time::Duration;

use instant_server::{InvalidationQueuePolicy, PostgresInvalidationQueuePolicy, PostgresPool};
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

fn scoped_url(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}%2Cpublic")
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn invalidation_queue_policy_reloads_batch_and_drop_flags() {
    let Some(database_url) = instant_test_support::test_database_url(
        "invalidation_queue_policy_reloads_batch_and_drop_flags",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("invalidation_queue_{}", Uuid::new_v4().simple());
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
    let dropped_app_id = Uuid::new_v4();
    let setting_attr_id = Uuid::new_v4();
    let value_attr_id = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.attrs (id, app_id, etype, label) VALUES
                   ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')"
            ),
            &[&setting_attr_id, &value_attr_id, &config_app_id],
        )
        .await
        .unwrap();
    let batch_entity = insert_flag(
        &setup,
        &schema,
        config_app_id,
        setting_attr_id,
        value_attr_id,
        "invalidator-batch-limit",
        json!(7),
    )
    .await;
    insert_flag(
        &setup,
        &schema,
        config_app_id,
        setting_attr_id,
        value_attr_id,
        "invalidator-drop-backpressure-apps",
        json!([dropped_app_id.to_string()]),
    )
    .await;
    let latency_entity = insert_flag(
        &setup,
        &schema,
        config_app_id,
        setting_attr_id,
        value_attr_id,
        "invalidator-drop-tx-latency-ms",
        json!(25),
    )
    .await;

    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policy =
        PostgresInvalidationQueuePolicy::new(policy_client).with_config_app_id(config_app_id);
    let selected = policy.config(dropped_app_id).await.unwrap();
    assert_eq!(selected.batch_limit, 7);
    assert_eq!(selected.drop_after, Some(Duration::from_millis(25)));
    let ordinary = policy.config(Uuid::new_v4()).await.unwrap();
    assert_eq!(ordinary.batch_limit, 7);
    assert_eq!(ordinary.drop_after, None);

    set_value(
        &setup,
        &schema,
        config_app_id,
        batch_entity,
        value_attr_id,
        json!(11),
    )
    .await;
    set_value(
        &setup,
        &schema,
        config_app_id,
        latency_entity,
        value_attr_id,
        json!(40),
    )
    .await;
    let updated = policy.config(dropped_app_id).await.unwrap();
    assert_eq!(updated.batch_limit, 11);
    assert_eq!(updated.drop_after, Some(Duration::from_millis(40)));

    drop(policy);
    let pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
        .await
        .unwrap();
    let mut pooled_policy =
        PostgresInvalidationQueuePolicy::from_pool(pool.clone()).with_config_app_id(config_app_id);
    let pooled = pooled_policy.config(dropped_app_id).await.unwrap();
    assert_eq!(pooled.batch_limit, 11);
    assert_eq!(pooled.drop_after, Some(Duration::from_millis(40)));
    assert_eq!(pool.status().available, 1);

    for _ in 0..8 {
        assert_eq!(pooled_policy.config(dropped_app_id).await.unwrap(), pooled);
    }
    let lease = pool.acquire().await.unwrap();
    let prepared: i64 = lease.query_one(
        "SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE 'SELECT flag_setting.value%'",
        &[],
    ).await.unwrap().get(0);
    assert_eq!(
        prepared, 1,
        "queue config must reuse its physical connection plan"
    );
    drop(lease);
    set_value(
        &setup,
        &schema,
        config_app_id,
        batch_entity,
        value_attr_id,
        json!(19),
    )
    .await;
    assert_eq!(
        pooled_policy
            .config(dropped_app_id)
            .await
            .unwrap()
            .batch_limit,
        19
    );
    assert_eq!(
        pooled_policy
            .config(Uuid::new_v4())
            .await
            .unwrap()
            .drop_after,
        None
    );
    pooled_policy = pooled_policy.with_config_app_id(Uuid::new_v4());
    assert_eq!(
        pooled_policy.config(dropped_app_id).await.unwrap(),
        instant_server::InvalidationQueueConfig::default()
    );
    pooled_policy = pooled_policy.with_config_app_id(config_app_id);
    let lease = pool.acquire().await.unwrap();
    drop(deadpool_postgres::Object::take(lease));
    assert_eq!(
        pooled_policy
            .config(dropped_app_id)
            .await
            .unwrap()
            .batch_limit,
        19
    );
    set_value(
        &setup,
        &schema,
        config_app_id,
        batch_entity,
        value_attr_id,
        json!("invalid"),
    )
    .await;
    assert!(pooled_policy.config(dropped_app_id).await.is_err());

    drop(pooled_policy);
    drop(pool);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_flag(
    client: &Client,
    schema: &str,
    app_id: Uuid,
    setting_attr_id: Uuid,
    value_attr_id: Uuid,
    setting: &str,
    value: Value,
) -> Uuid {
    let entity_id = Uuid::new_v4();
    client
        .execute(
            &format!(
                "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                   ($1, $2, $3, $4), ($1, $2, $5, $6)"
            ),
            &[
                &app_id,
                &entity_id,
                &setting_attr_id,
                &Json(json!(setting)),
                &value_attr_id,
                &Json(value),
            ],
        )
        .await
        .unwrap();
    entity_id
}

async fn set_value(
    client: &Client,
    schema: &str,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
) {
    client
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[&app_id, &entity_id, &attr_id, &Json(value)],
        )
        .await
        .unwrap();
}
