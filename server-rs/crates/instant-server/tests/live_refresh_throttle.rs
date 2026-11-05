use instant_server::{PostgresRefreshThrottlePolicy, RefreshThrottlePolicy};
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

#[tokio::test]
async fn refresh_throttle_policy_follows_live_config_flags() {
    let Some(database_url) = instant_test_support::test_database_url(
        "refresh_throttle_policy_follows_live_config_flags",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("refresh_throttle_{}", Uuid::new_v4().simple());
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
    let throttled_app_id = Uuid::new_v4();
    let setting_attr_id = Uuid::new_v4();
    let value_attr_id = Uuid::new_v4();
    let apps_entity_id = Uuid::new_v4();
    let delay_entity_id = Uuid::new_v4();
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
                   ($1, $2, $3, $4), ($1, $2, $5, $6),
                   ($1, $7, $3, $8), ($1, $7, $5, $9)"
            ),
            &[
                &config_app_id,
                &apps_entity_id,
                &setting_attr_id,
                &Json(json!("refresh-throttled-apps")),
                &value_attr_id,
                &Json(json!([throttled_app_id.to_string()])),
                &delay_entity_id,
                &Json(json!("refresh-throttle-ms")),
                &Json(json!(25)),
            ],
        )
        .await
        .unwrap();
    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policy =
        PostgresRefreshThrottlePolicy::new(policy_client).with_config_app_id(config_app_id);

    assert_eq!(
        policy.delay_after_noop(throttled_app_id).await.unwrap(),
        Some(std::time::Duration::from_millis(25))
    );
    assert_eq!(policy.delay_after_noop(Uuid::new_v4()).await.unwrap(), None);

    set_value(
        &setup,
        &schema,
        config_app_id,
        delay_entity_id,
        value_attr_id,
        json!(40),
    )
    .await;
    assert_eq!(
        policy.delay_after_noop(throttled_app_id).await.unwrap(),
        Some(std::time::Duration::from_millis(40))
    );

    drop(policy);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
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
