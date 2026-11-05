use instant_server::{PostgresStorageAccessPolicy, ServerEnvironment, StorageAccessPolicy};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps one schema, one policy and every flag read together.
async fn production_storage_policy_reads_the_config_app_block_list() {
    let Some(database_url) = instant_test_support::test_database_url(
        "production_storage_policy_reads_the_config_app_block_list",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let schema = format!("storage_policy_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
             CREATE TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let blocked_app = Uuid::new_v4();
    let allowed_app = Uuid::new_v4();
    let app_attr = Uuid::new_v4();
    let disabled_attr = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    let flag_entity_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs VALUES
             ($1, $3, 'storage-block-list', 'appId'),
             ($2, $3, 'storage-block-list', 'isDisabled')",
            &[&app_attr, &disabled_attr, &config_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO attrs VALUES
             ($1, $3, 'flags', 'setting'),
             ($2, $3, 'flags', 'value')",
            &[&setting_attr, &value_attr, &config_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO triples VALUES
             ($1, $2, $3, $5), ($1, $2, $4, 'true'::jsonb)",
            &[
                &config_app_id,
                &entity_id,
                &app_attr,
                &disabled_attr,
                &json!(blocked_app.to_string()),
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO triples VALUES
             ($1, $2, $3, '\"tika-enabled-apps\"'::jsonb),
             ($1, $2, $4, $5)",
            &[
                &config_app_id,
                &flag_entity_id,
                &setting_attr,
                &value_attr,
                &json!(["invalid", allowed_app.to_string()]),
            ],
        )
        .await
        .unwrap();

    let quota_entity_id = Uuid::new_v4();
    let quota_map_entity_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO triples VALUES
             ($1, $2, $3, '\"storage-quota-bytes\"'::jsonb),
             ($1, $2, $4, '1000'::jsonb),
             ($1, $5, $3, '\"storage-quota-bytes-by-app\"'::jsonb),
             ($1, $5, $4, $6)",
            &[
                &config_app_id,
                &quota_entity_id,
                &setting_attr,
                &value_attr,
                &quota_map_entity_id,
                &json!({allowed_app.to_string(): 5_000}),
            ],
        )
        .await
        .unwrap();

    let policy = PostgresStorageAccessPolicy::new(client, ServerEnvironment::Production)
        .with_config_app_id(config_app_id);
    let error = policy.authorize(blocked_app).await.unwrap_err();
    assert_eq!(error.body().message, "Storage is disabled for this app");
    policy.authorize(allowed_app).await.unwrap();
    assert!(policy.detect_content_type(allowed_app).await.unwrap());
    assert!(!policy.detect_content_type(blocked_app).await.unwrap());
    assert_eq!(
        policy.storage_quota_bytes(allowed_app).await.unwrap(),
        Some(5_000)
    );
    assert_eq!(
        policy.storage_quota_bytes(blocked_app).await.unwrap(),
        Some(1_000)
    );

    let (development_client, connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    development_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let development =
        PostgresStorageAccessPolicy::new(development_client, ServerEnvironment::Development)
            .with_config_app_id(config_app_id);
    development.authorize(blocked_app).await.unwrap();
    assert!(development.detect_content_type(allowed_app).await.unwrap());
}
