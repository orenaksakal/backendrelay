use std::time::Duration;

use instant_server::{PostgresStatementCancelWaitPolicy, StatementCancelWaitPolicy};
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
async fn statement_cancel_wait_policy_tracks_the_live_config_flag() {
    let Some(database_url) = instant_test_support::test_database_url(
        "statement_cancel_wait_policy_tracks_the_live_config_flag",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("statement_cancel_policy_{}", Uuid::new_v4().simple());
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

    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policy =
        PostgresStatementCancelWaitPolicy::new(policy_client).with_config_app_id(config_app_id);
    assert_eq!(
        policy.wait_duration().await.unwrap(),
        Duration::from_millis(500)
    );

    let entity_id = Uuid::new_v4();
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
                &Json(json!("statement-cancel-wait-ms")),
                &value_attr_id,
                &Json(json!(25)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        policy.wait_duration().await.unwrap(),
        Duration::from_millis(25)
    );

    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = 'null'::jsonb
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[&config_app_id, &entity_id, &value_attr_id],
        )
        .await
        .unwrap();
    assert!(policy.wait_duration().await.is_err());

    drop(policy);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
