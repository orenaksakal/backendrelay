use std::time::Duration;

use instant_server::{DatalogCachePolicy, DatalogCacheSettings, PostgresDatalogCachePolicy};
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
async fn datalog_cache_policy_reads_one_live_config_snapshot() {
    let Some(database_url) = instant_test_support::test_database_url(
        "datalog_cache_policy_reads_one_live_config_snapshot",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("datalog_cache_{}", Uuid::new_v4().simple());
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

    let mut entities = Vec::new();
    for (name, value) in [
        ("datalog-cache-max-weight", json!(12_345)),
        ("datalog-cache-ttl", json!(2_500)),
        ("datalog-cache-size-multiple", json!(7)),
        ("datalog-cache-process-max-in-flight", json!(12)),
        ("datalog-cache-max-in-flight", json!(3)),
    ] {
        let entity_id = Uuid::new_v4();
        entities.push(entity_id);
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                       ($1, $2, $3, $4), ($1, $2, $5, $6)"
                ),
                &[
                    &config_app_id,
                    &entity_id,
                    &setting_attr_id,
                    &Json(json!(name)),
                    &value_attr_id,
                    &Json(value),
                ],
            )
            .await
            .unwrap();
    }

    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policy = PostgresDatalogCachePolicy::new(policy_client).with_config_app_id(config_app_id);
    assert_eq!(
        policy.settings().await.unwrap(),
        DatalogCacheSettings {
            max_weight: 12_345,
            ttl: Duration::from_millis(2_500),
            size_multiple: 7,
            process_max_in_flight: 12,
            max_in_flight: 3,
        }
    );

    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[
                &config_app_id,
                &entities[1],
                &value_attr_id,
                &Json(json!(4_000)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        policy.settings().await.unwrap().ttl,
        Duration::from_millis(4_000)
    );

    drop(policy);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
