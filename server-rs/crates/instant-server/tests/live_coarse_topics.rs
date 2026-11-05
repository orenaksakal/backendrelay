use instant_server::{CoarseTopicsPolicy, PostgresCoarseTopicsPolicy};
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
async fn coarse_topics_policy_follows_live_app_membership() {
    let Some(database_url) =
        instant_test_support::test_database_url("coarse_topics_policy_follows_live_app_membership")
    else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("coarse_topics_{}", Uuid::new_v4().simple());
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
    let coarse_app_id = Uuid::new_v4();
    let setting_attr_id = Uuid::new_v4();
    let value_attr_id = Uuid::new_v4();
    let flag_entity_id = Uuid::new_v4();
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
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                   ($1, $2, $3, $4), ($1, $2, $5, $6)"
            ),
            &[
                &config_app_id,
                &flag_entity_id,
                &setting_attr_id,
                &Json(json!("coarse-topics-apps")),
                &value_attr_id,
                &Json(json!([coarse_app_id.to_string()])),
            ],
        )
        .await
        .unwrap();
    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policy = PostgresCoarseTopicsPolicy::new(policy_client).with_config_app_id(config_app_id);

    assert!(policy.use_coarse_topics(coarse_app_id).await.unwrap());
    assert!(!policy.use_coarse_topics(Uuid::new_v4()).await.unwrap());

    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[
                &config_app_id,
                &flag_entity_id,
                &value_attr_id,
                &Json(json!([])),
            ],
        )
        .await
        .unwrap();
    assert!(!policy.use_coarse_topics(coarse_app_id).await.unwrap());

    drop(policy);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
