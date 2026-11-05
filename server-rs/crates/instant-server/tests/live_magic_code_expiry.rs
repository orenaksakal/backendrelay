use instant_http::ApiErrorType;
use instant_server::{MagicCodeExpiryPolicy, PostgresMagicCodeExpiryPolicy};
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
async fn magic_code_expiry_uses_live_app_override_and_config_fallback() {
    let Some(database_url) = instant_test_support::test_database_url(
        "magic_code_expiry_uses_live_app_override_and_config_fallback",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("magic_code_expiry_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.apps (
               id uuid PRIMARY KEY,
               magic_code_expiry_minutes integer,
               deletion_marked_at timestamptz
             );
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
    let override_app_id = Uuid::new_v4();
    let fallback_app_id = Uuid::new_v4();
    let deleted_app_id = Uuid::new_v4();
    let setting_attr_id = Uuid::new_v4();
    let value_attr_id = Uuid::new_v4();
    let flag_entity_id = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.apps VALUES
                   ($1, 7, NULL), ($2, NULL, NULL), ($3, NULL, now())"
            ),
            &[&override_app_id, &fallback_app_id, &deleted_app_id],
        )
        .await
        .unwrap();
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
                &Json(json!("default-magic-code-expiry-minutes")),
                &value_attr_id,
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
        PostgresMagicCodeExpiryPolicy::new(policy_client).with_config_app_id(config_app_id);

    assert_eq!(policy.expiry_minutes(override_app_id).await.unwrap(), 7);
    assert_eq!(policy.expiry_minutes(fallback_app_id).await.unwrap(), 25);
    let missing = policy.expiry_minutes(Uuid::new_v4()).await.unwrap_err();
    assert_eq!(missing.body().kind, ApiErrorType::RecordNotFound);
    let deleted = policy.expiry_minutes(deleted_app_id).await.unwrap_err();
    assert_eq!(deleted.body().kind, ApiErrorType::RecordNotFound);

    setup
        .execute(
            &format!(
                "UPDATE {schema}.apps SET magic_code_expiry_minutes = 120
                 WHERE id = $1"
            ),
            &[&override_app_id],
        )
        .await
        .unwrap();
    assert_eq!(
        policy.expiry_minutes(override_app_id).await.unwrap(),
        60,
        "per-app overrides above one hour are clamped"
    );

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
                &Json(json!(40)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(policy.expiry_minutes(fallback_app_id).await.unwrap(), 40);
    assert_eq!(policy.expiry_minutes(override_app_id).await.unwrap(), 60);

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
                &Json(json!(90)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        policy.expiry_minutes(fallback_app_id).await.unwrap(),
        60,
        "config fallbacks above one hour are clamped"
    );

    setup
        .execute(
            &format!(
                "DELETE FROM {schema}.triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[&config_app_id, &flag_entity_id, &value_attr_id],
        )
        .await
        .unwrap();
    assert_eq!(
        policy.expiry_minutes(fallback_app_id).await.unwrap(),
        15,
        "the default magic-code lifetime is fifteen minutes"
    );

    drop(policy);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
