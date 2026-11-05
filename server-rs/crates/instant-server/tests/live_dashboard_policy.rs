use std::collections::HashSet;

use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, NoopDashboardControlHooks,
    PostgresDashboardPolicyHooks, ServerEnvironment,
};
use serde_json::{Value, json};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dashboard_policy_preserves_admin_backup_and_signup_entitlements() {
    let Some(database_url) = instant_test_support::test_database_url(
        "dashboard_policy_preserves_admin_backup_and_signup_entitlements",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("dashboard_policy_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public;
             CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE app_users (id uuid PRIMARY KEY, app_id uuid NOT NULL, email text NOT NULL);
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
             CREATE TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let shared_app_id = Uuid::new_v4();
    let capped_shared_app_id = Uuid::new_v4();
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
    let ephemeral_creator = Uuid::new_v4();
    let get_a_db_creator = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users VALUES
             ($1, 'ephemeral-apps-dev@backendrelay.invalid'),
             ($2, 'get-a-db@backendrelay.invalid')",
            &[&ephemeral_creator, &get_a_db_creator],
        )
        .await
        .unwrap();
    for (setting, value) in [
        ("ephemeral-apps-enabled", json!(false)),
        ("team-emails", json!([{"email": "admin@example.com"}])),
        (
            "dashboard-signups",
            json!({"mode": "restricted", "allowedEmails": ["oauth@example.com"]}),
        ),
    ] {
        insert_flag(
            &client,
            config_app_id,
            setting_attr,
            value_attr,
            setting,
            value,
        )
        .await;
    }
    client
        .execute(
            "INSERT INTO app_users VALUES
             ($1, $3, 'one@example.com'), ($2, $3, 'two@example.com')",
            &[&Uuid::new_v4(), &Uuid::new_v4(), &capped_shared_app_id],
        )
        .await
        .unwrap();

    let hooks = PostgresDashboardPolicyHooks::new(
        client,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    )
    .with_config_app_id(config_app_id)
    .with_shared_oauth_providers(["github"])
    .with_shared_oauth_user_limit(2)
    .with_self_hosted_plan_enabled(true)
    .with_superuser_email(Some("root@example.com"));

    for email in ["ADMIN@example.com", "ROOT@example.com"] {
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeDashboardAdmin {
                    email: email.to_owned(),
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
    }
    assert_eq!(
        hooks
            .execute(DashboardControlHookOperation::AuthorizeDashboardAdmin {
                email: "user@example.com".to_owned(),
            })
            .await
            .unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        hooks
            .execute(DashboardControlHookOperation::AuthorizeBackup {
                app_id: Uuid::new_v4(),
                creator_id: Some(ephemeral_creator),
                work_estimate: 1,
                client_ip: None,
            })
            .await
            .unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        hooks
            .execute(DashboardControlHookOperation::AuthorizeBackup {
                app_id: Uuid::new_v4(),
                creator_id: Some(get_a_db_creator),
                work_estimate: 24_999_999,
                client_ip: None,
            })
            .await
            .unwrap(),
        Value::Bool(true)
    );
    let config = hooks
        .execute(DashboardControlHookOperation::SpecialAppConfiguration { kind: "ephemeral" })
        .await
        .unwrap();
    assert_eq!(config["creator_id"], ephemeral_creator.to_string());
    assert_eq!(config["enabled"], false);
    assert_eq!(
        authorize_claim(&hooks, ephemeral_creator, "claimant@example.com").await,
        Value::Bool(true)
    );
    assert_eq!(
        authorize_claim(&hooks, get_a_db_creator, "claimant@example.com").await,
        Value::Bool(true)
    );
    assert_eq!(
        hooks
            .execute(
                DashboardControlHookOperation::AuthorizeDashboardOAuthSignup {
                    email: "OAuth@Example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        hooks
            .execute(
                DashboardControlHookOperation::AuthorizeDashboardOAuthSignup {
                    email: "blocked@example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        hooks
            .execute(
                DashboardControlHookOperation::AuthorizeSharedOAuthCredentials {
                    app_id: shared_app_id,
                    provider_name: "github".to_owned(),
                }
            )
            .await
            .unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        hooks
            .execute(
                DashboardControlHookOperation::AuthorizeSharedOAuthCredentials {
                    app_id: capped_shared_app_id,
                    provider_name: "github".to_owned(),
                }
            )
            .await
            .unwrap_err()
            .body()
            .kind,
        instant_http::ApiErrorType::ValidationFailed
    );
}

#[tokio::test]
async fn dashboard_signup_and_ephemeral_apps_default_to_closed() {
    let Some(database_url) = instant_test_support::test_database_url(
        "dashboard_signup_and_ephemeral_apps_default_to_closed",
    ) else {
        return;
    };
    let client = connect_schema(&database_url, None).await;
    let (schema, config_app_id, _, _) = create_policy_schema(&client).await;
    client
        .execute(
            "INSERT INTO instant_users VALUES
             ($1, 'allowed@example.com'),
             ($2, 'other@example.com'),
             ($3, 'root@example.com'),
             ($4, 'ephemeral-apps-dev@backendrelay.invalid')",
            &[
                &Uuid::new_v4(),
                &Uuid::new_v4(),
                &Uuid::new_v4(),
                &Uuid::new_v4(),
            ],
        )
        .await
        .unwrap();

    let closed = PostgresDashboardPolicyHooks::new(
        client,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    )
    .with_config_app_id(config_app_id)
    .with_dashboard_allowed_emails(Some(HashSet::from(["allowed@example.com".to_owned()])))
    .with_superuser_email(Some("root@example.com"));

    assert_eq!(
        signup(&closed, "ALLOWED@example.com").await,
        Value::Bool(true)
    );
    assert_eq!(signup(&closed, "ROOT@example.com").await, Value::Bool(true));
    assert_eq!(
        signup(&closed, "other@example.com").await,
        Value::Bool(false)
    );
    assert_eq!(ephemeral_enabled(&closed).await, Value::Bool(false));

    let open = PostgresDashboardPolicyHooks::new(
        connect_schema(&database_url, Some(&schema)).await,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    )
    .with_config_app_id(config_app_id)
    .with_dashboard_signup_open(true);
    assert_eq!(signup(&open, "other@example.com").await, Value::Bool(true));

    let enabled = PostgresDashboardPolicyHooks::new(
        connect_schema(&database_url, Some(&schema)).await,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    )
    .with_config_app_id(config_app_id)
    .with_ephemeral_apps_enabled(true);
    assert_eq!(ephemeral_enabled(&enabled).await, Value::Bool(true));
}

async fn connect_schema(database_url: &str, schema: Option<&str>) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    if let Some(schema) = schema {
        client
            .batch_execute(&format!("SET search_path TO {schema}, public"))
            .await
            .unwrap();
    }
    client
}

async fn create_policy_schema(client: &Client) -> (String, Uuid, Uuid, Uuid) {
    let schema = format!("dashboard_policy_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public;
             CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE app_users (id uuid PRIMARY KEY, app_id uuid NOT NULL, email text NOT NULL);
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
    (schema, config_app_id, setting_attr, value_attr)
}

async fn signup<D: DashboardControlHooks>(hooks: &D, email: &str) -> Value {
    hooks
        .execute(
            DashboardControlHookOperation::AuthorizeDashboardOAuthSignup {
                email: email.to_owned(),
            },
        )
        .await
        .unwrap()
}

async fn ephemeral_enabled<D: DashboardControlHooks>(hooks: &D) -> Value {
    hooks
        .execute(DashboardControlHookOperation::SpecialAppConfiguration { kind: "ephemeral" })
        .await
        .unwrap()
        .get("enabled")
        .cloned()
        .unwrap_or(Value::Null)
}

async fn insert_flag(
    client: &Client,
    app_id: Uuid,
    setting_attr: Uuid,
    value_attr: Uuid,
    setting: &str,
    value: Value,
) {
    let entity_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO triples VALUES ($1, $2, $3, to_jsonb($5::text)), ($1, $2, $4, $6)",
            &[
                &app_id,
                &entity_id,
                &setting_attr,
                &value_attr,
                &setting,
                &value,
            ],
        )
        .await
        .unwrap();
}

async fn authorize_claim<D: DashboardControlHooks>(
    hooks: &D,
    creator_id: Uuid,
    claimant_email: &str,
) -> Value {
    hooks
        .execute(DashboardControlHookOperation::AuthorizeClaim {
            app_id: Uuid::new_v4(),
            current_creator_id: Some(creator_id),
            claimant_id: Uuid::new_v4(),
            claimant_email: claimant_email.to_owned(),
        })
        .await
        .unwrap()
}
