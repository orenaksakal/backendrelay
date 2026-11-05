use std::sync::Arc;

use instant_http::{
    AdminActor, AdminDataBackend, AdminRequestContext, AdminTransactRequest, ApiErrorType,
};
use instant_server::{
    AdminDataService, DashboardControlHooks, NoopDashboardControlHooks,
    PostgresAdminDataAuthorizer, PostgresDashboardPolicyHooks, PostgresPool, ServerEnvironment,
    get_config_flag, list_config_flags, migrate_database, set_config_flag, unset_config_flag,
};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[path = "../src/native_allocator.rs"]
mod native_allocator;

async fn connect_and_migrate() -> Option<String> {
    let database_url = instant_test_support::test_database_url("live_config_app")?;
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    migrate_database(&mut client).await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
    Some(database_url)
}

#[tokio::test]
async fn migrated_config_app_denies_guest_writes() {
    let Some(database_url) = connect_and_migrate().await else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let attr_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed,
                is_required, forward_ident, etype, label)
             VALUES ($1, $2, 'blob', 'one', false, false, false, $3, 'notes', 'body')",
            &[
                &attr_id,
                &instant_server::INSTANT_CONFIG_APP_ID,
                &Uuid::new_v4(),
            ],
        )
        .await
        .unwrap();
    drop(client);
    driver.await.unwrap().unwrap();

    let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
    let service = AdminDataService::from_pool(
        pool.clone(),
        Arc::new(PostgresAdminDataAuthorizer::from_pool(pool)),
    );
    let error = service
        .transact(
            AdminRequestContext {
                app_id: instant_server::INSTANT_CONFIG_APP_ID,
                bearer_token: None,
                actor: AdminActor::Guest,
                client_ip: None,
            },
            AdminTransactRequest {
                steps: vec![json!(["add-triple", Uuid::new_v4(), attr_id, "denied"])],
                throw_on_missing_attrs: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::PermissionDenied);
}

#[tokio::test]
async fn operator_flags_round_trip_and_dashboard_policy_reads_them() {
    let Some(database_url) = connect_and_migrate().await else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let value = json!([{"email": "ops@example.com"}]);
    set_config_flag(&mut client, "team-emails", value.clone())
        .await
        .unwrap();
    assert_eq!(
        get_config_flag(&client, "team-emails").await.unwrap(),
        Some(value.clone())
    );
    assert_eq!(
        list_config_flags(&client)
            .await
            .unwrap()
            .into_iter()
            .find(|(setting, _)| setting == "team-emails"),
        Some(("team-emails".to_owned(), value.clone()))
    );

    let policy_client = {
        let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
        tokio::spawn(connection);
        client
    };
    let policy = PostgresDashboardPolicyHooks::new(
        policy_client,
        ServerEnvironment::Development,
        NoopDashboardControlHooks,
    );
    assert_eq!(
        policy
            .execute(
                instant_server::DashboardControlHookOperation::AuthorizeDashboardAdmin {
                    email: "OPS@example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        json!(true)
    );
    assert_eq!(
        policy
            .execute(
                instant_server::DashboardControlHookOperation::AuthorizeDashboardAdmin {
                    email: "other@example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        json!(false)
    );

    let replacement = json!([{"email": "replacement@example.com"}]);
    set_config_flag(&mut client, "team-emails", replacement)
        .await
        .unwrap();
    assert_eq!(
        policy
            .execute(
                instant_server::DashboardControlHookOperation::AuthorizeDashboardAdmin {
                    email: "ops@example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        json!(false)
    );
    assert_eq!(
        policy
            .execute(
                instant_server::DashboardControlHookOperation::AuthorizeDashboardAdmin {
                    email: "replacement@example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        json!(true)
    );

    assert!(unset_config_flag(&mut client, "team-emails").await.unwrap());
    assert_eq!(get_config_flag(&client, "team-emails").await.unwrap(), None);
    assert_eq!(
        policy
            .execute(
                instant_server::DashboardControlHookOperation::AuthorizeDashboardAdmin {
                    email: "replacement@example.com".to_owned(),
                }
            )
            .await
            .unwrap(),
        json!(false)
    );

    drop(client);
    driver.await.unwrap().unwrap();
}
