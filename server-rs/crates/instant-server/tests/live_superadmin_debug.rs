use std::sync::Arc;

use instant_http::{
    AdminActor, ApiError, SuperadminBackend, SuperadminContext, SuperadminOperation,
};
use instant_instaql::system_attr_id;
use instant_server::{
    AdminDataService, PostgresAdminDataAuthorizer, PostgresSuperadminAuthorizer,
    ProductionAdminData, SuperadminPrincipal, SuperadminService, SuperadminStore, migrate_database,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use tokio_postgres::types::Json;
use uuid::Uuid;

#[tokio::test]
async fn superadmin_explain_plans_instaql_for_an_impersonated_user_read_only() {
    let Some(database_url) = instant_test_support::test_database_url(
        "superadmin_explain_plans_instaql_for_an_impersonated_user_read_only",
    ) else {
        return;
    };
    let (mut setup, setup_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    migrate_database(&mut setup).await.unwrap();

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'person@example.invalid')",
            &[&user_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'explain')",
            &[&app_id, &user_id],
        )
        .await
        .unwrap();
    seed_goal_title_attribute(&setup, app_id).await;
    seed_app_user_email(&setup, app_id, user_id, "person@example.invalid").await;

    let (authorizer_client, authorizer_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let authorizer_task = tokio::spawn(authorizer_connection);
    let (data_client, data_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let data_task = tokio::spawn(data_connection);
    let authorizer = Arc::new(PostgresAdminDataAuthorizer::new(authorizer_client));
    let data: ProductionAdminData = AdminDataService::new(data_client, authorizer);

    let analysis = data
        .explain_instaql_as_user(
            app_id,
            AdminActor::Email("person@example.invalid".to_owned()),
            json!({"goals": {"$": {"where": {"title": "Ship"}}}})
                .as_object()
                .unwrap()
                .clone(),
        )
        .await
        .unwrap();
    assert!(analysis["time_ms"].as_f64().unwrap() >= 0.0);
    assert!(analysis["execution_time_ms"].as_f64().unwrap() >= 0.0);
    assert!(analysis["planning_time_ms"].as_f64().unwrap() >= 0.0);
    assert!(analysis["indexes"].is_object());

    // EXPLAIN ANALYZE ran against a read-only transaction, so the seed row is
    // untouched and no explain-shaped result was returned.
    let count: i64 = setup
        .query_one(
            "SELECT count(*) FROM instant_users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);

    drop(data);
    drop(setup);
    setup_task.await.unwrap().unwrap();
    authorizer_task.await.unwrap().unwrap();
    data_task.await.unwrap().unwrap();
}

async fn seed_goal_title_attribute(client: &tokio_postgres::Client, app_id: Uuid) {
    let attr_id = Uuid::new_v4();
    let forward_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
                forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
                inferred_types)
             VALUES ($1, $2, 'blob', 'one', false, true, false,
                     $3, 'goals', 'title', NULL, NULL, NULL, NULL)",
            &[&attr_id, &app_id, &forward_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label)
             VALUES ($1, $2, $3, 'goals', 'title')",
            &[&forward_id, &app_id, &attr_id],
        )
        .await
        .unwrap();
}

async fn seed_app_user_email(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    user_id: Uuid,
    email: &str,
) {
    let attr_id = system_attr_id("$users", "email").unwrap();
    client
        .execute(
            "INSERT INTO triples
               (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae)
             VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), false, false, true, false, false)",
            &[&app_id, &user_id, &attr_id, &Json(json!(email))],
        )
        .await
        .unwrap();
}

struct PanicStore;

impl SuperadminStore for PanicStore {
    async fn execute(
        &self,
        _principal: SuperadminPrincipal,
        _context: SuperadminContext,
        _operation: SuperadminOperation,
    ) -> Result<Value, ApiError> {
        panic!("authorization must reject before any operator diagnostic runs");
    }
}

#[tokio::test]
async fn superadmin_debug_operations_reject_users_without_app_membership() {
    let Some(database_url) = instant_test_support::test_database_url(
        "superadmin_debug_operations_reject_users_without_app_membership",
    ) else {
        return;
    };
    let (mut setup, setup_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    migrate_database(&mut setup).await.unwrap();

    let owner_id = Uuid::new_v4();
    let outsider_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'owner@example.invalid'), ($2, 'outsider@example.invalid')",
            &[&owner_id, &outsider_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'private')",
            &[&app_id, &owner_id],
        )
        .await
        .unwrap();
    let token = "per_outsider";
    setup
        .execute(
            "INSERT INTO instant_personal_access_tokens (id, name, lookup_key, user_id)
             VALUES ($1, 'outsider', $2, $3)",
            &[
                &Uuid::new_v4(),
                &Sha256::digest(token.as_bytes()).to_vec(),
                &outsider_id,
            ],
        )
        .await
        .unwrap();

    let (authorizer_client, authorizer_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let authorizer_task = tokio::spawn(authorizer_connection);
    let service = SuperadminService::new(
        PostgresSuperadminAuthorizer::new(authorizer_client),
        PanicStore,
    );
    for operation in [
        SuperadminOperation::Runtime { app_id },
        SuperadminOperation::Explain {
            app_id,
            actor: AdminActor::Guest,
            query: json!({"goals": {}}).as_object().unwrap().clone(),
        },
    ] {
        let error = service
            .execute(
                SuperadminContext {
                    bearer_token: token.to_owned(),
                },
                operation,
            )
            .await
            .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    }

    drop(setup);
    drop(service);
    setup_task.await.unwrap().unwrap();
    authorizer_task.await.unwrap().unwrap();
}
