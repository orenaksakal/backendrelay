use axum::body::Bytes;
use axum::http::{HeaderMap, Method, header};
use instant_http::{DashboardRequest, DashboardResponse};
use instant_server::{
    DASHBOARD_BUILD_VERSION, DASHBOARD_CONFIG_PATH, DashboardConfigService, DashboardSubsystem,
    NoopDashboardControlHooks, PostgresDashboardPolicyHooks, ServerConfig, ServerEnvironment,
};
use serde_json::{Value, json};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

struct ConfigSchema {
    config_app_id: Uuid,
    setting_attr: Uuid,
    value_attr: Uuid,
}

async fn create_config_schema(client: &Client) -> ConfigSchema {
    let schema = format!("dashboard_config_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public;
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
             CREATE TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs VALUES
             ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')",
            &[&setting_attr, &value_attr, &config_app_id],
        )
        .await
        .unwrap();
    ConfigSchema {
        config_app_id,
        setting_attr,
        value_attr,
    }
}

async fn insert_flag(client: &Client, schema: &ConfigSchema, setting: &str, value: Value) {
    let entity_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO triples VALUES ($1, $2, $3, to_jsonb($6::text)), ($1, $2, $4, $5)",
            &[
                &schema.config_app_id,
                &entity_id,
                &schema.setting_attr,
                &schema.value_attr,
                &value,
                &setting,
            ],
        )
        .await
        .unwrap();
}

fn config_probe() -> DashboardRequest {
    DashboardRequest {
        method: Method::GET,
        path: DASHBOARD_CONFIG_PATH.to_owned(),
        query: None,
        headers: HeaderMap::new(),
        body: Bytes::new(),
        staged_body_path: None,
        remote_addr: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn body(response: DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

#[tokio::test]
async fn public_config_follows_policy_flags_without_user_or_secret_data() {
    let Some(database_url) = instant_test_support::test_database_url(
        "public_config_follows_policy_flags_without_user_or_secret_data",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = create_config_schema(&client).await;
    insert_flag(
        &client,
        &schema,
        "dashboard-signups",
        json!({"mode": "restricted", "allowedEmails": ["allowed@example.test"]}),
    )
    .await;
    insert_flag(&client, &schema, "ephemeral-apps-enabled", json!(true)).await;

    let config = ServerConfig::from_pairs([
        ("INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID", "client-id"),
        (
            "INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET",
            "client-secret",
        ),
    ])
    .unwrap();
    let policy = PostgresDashboardPolicyHooks::new(
        client,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    )
    .with_config_app_id(schema.config_app_id)
    .with_dashboard_signup_open(config.dashboard_signup_open())
    .with_ephemeral_apps_enabled(config.ephemeral_apps_enabled());
    let service = DashboardConfigService::from_config(policy, &config, true);

    let response = service.handle(config_probe()).await.unwrap();
    assert_eq!(
        response.headers[header::CACHE_CONTROL],
        axum::http::HeaderValue::from_static("no-store")
    );
    let body = body(response);
    assert_eq!(body["version"], DASHBOARD_BUILD_VERSION);
    assert_eq!(body["googleLogin"], true);
    assert_eq!(body["signup"], "restricted");
    assert_eq!(body["ephemeralApps"], true);
    assert_eq!(body["private"], true);
    let encoded = body.to_string();
    assert!(!encoded.contains("allowed@example.test"));
    assert!(!encoded.contains("client-id"));
    assert!(!encoded.contains("client-secret"));
}

#[tokio::test]
async fn public_config_defaults_follow_environment_switches() {
    let Some(database_url) = instant_test_support::test_database_url(
        "public_config_defaults_follow_environment_switches",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = create_config_schema(&client).await;
    let config = ServerConfig::from_pairs([("INSTANT_EPHEMERAL_APPS", "true")]).unwrap();
    let policy = PostgresDashboardPolicyHooks::new(
        client,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    )
    .with_config_app_id(schema.config_app_id)
    .with_dashboard_signup_open(false)
    .with_ephemeral_apps_enabled(config.ephemeral_apps_enabled());
    let service = DashboardConfigService::from_config(policy, &config, false);

    let body = body(service.handle(config_probe()).await.unwrap());
    assert_eq!(body["googleLogin"], false);
    assert_eq!(body["signup"], "closed");
    assert_eq!(body["ephemeralApps"], true);
    assert_eq!(body["private"], false);
}
