use instant_http::ApiErrorType;
use instant_server::{PostgresWebhookLimitPolicy, WebhookLimitPolicy};
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
#[allow(clippy::too_many_lines)]
async fn webhook_limit_policy_follows_the_live_config_flag() {
    let Some(database_url) = instant_test_support::test_database_url(
        "webhook_limit_policy_follows_the_live_config_flag",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("webhook_limit_{}", Uuid::new_v4().simple());
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

    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policy = PostgresWebhookLimitPolicy::new(policy_client).with_config_app_id(config_app_id);
    assert_eq!(policy.maximum_active_webhooks().await.unwrap(), 100);

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
                &Json(json!("maximum-active-webhooks")),
                &value_attr_id,
                &Json(json!(7)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(policy.maximum_active_webhooks().await.unwrap(), 7);

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
                &Json(json!(12)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(policy.maximum_active_webhooks().await.unwrap(), 12);

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
                &Json(json!("invalid")),
            ],
        )
        .await
        .unwrap();
    let invalid = policy.maximum_active_webhooks().await.unwrap_err();
    assert_eq!(invalid.body().kind, ApiErrorType::Unknown);
    assert_eq!(
        invalid.body().message,
        "config flag `maximum-active-webhooks` must be an integer"
    );

    drop(policy);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
