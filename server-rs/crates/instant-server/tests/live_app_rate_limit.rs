use instant_http::ApiErrorType;
use instant_server::{AppRateLimitPolicy, PostgresAppRateLimitPolicy};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
async fn app_rate_limit_policy_reads_the_live_config_collection() {
    let Some(database_url) = instant_test_support::test_database_url(
        "app_rate_limit_policy_reads_the_live_config_collection",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let schema = format!("app_rate_limit_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
             CREATE TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let limited_app = Uuid::new_v4();
    let allowed_app = Uuid::new_v4();
    let app_id_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs VALUES ($1, $2, 'rate-limited-apps', 'appId')",
            &[&app_id_attr, &config_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO triples VALUES ($1, $2, $3, $4)",
            &[
                &config_app_id,
                &Uuid::new_v4(),
                &app_id_attr,
                &json!(limited_app.to_string()),
            ],
        )
        .await
        .unwrap();

    let (policy_client, policy_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { policy_connection.await.unwrap() });
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let hosted =
        PostgresAppRateLimitPolicy::new(policy_client, None).with_config_app_id(config_app_id);
    hosted.check_app_ids(&[allowed_app]).await.unwrap();
    let error = hosted
        .check_app_ids(&[allowed_app, limited_app])
        .await
        .unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::RateLimited);
    assert_eq!(error.status().as_u16(), 429);
    assert!(error.body().message.contains("operator of this deployment"));

    client
        .execute(
            "DELETE FROM triples WHERE app_id = $1 AND attr_id = $2",
            &[&config_app_id, &app_id_attr],
        )
        .await
        .unwrap();
    hosted.check_app_ids(&[limited_app]).await.unwrap();
}
