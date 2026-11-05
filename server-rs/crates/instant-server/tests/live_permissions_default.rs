//! D7.2: does a user namespace with no rule for an action allow or deny?
//!
//! The permissions editor's state chip says "No rule — allows everyone", and
//! that wording is only correct if the server's rule lookup treats an absent
//! user-namespace action as allowed. `RuleSet::program` returns `None` when no
//! expression matches, and the runtime paths treat `None` as permitted; this
//! live check proves the same thing through the guest transaction and query
//! routes, including a control case with an explicit deny.

use std::future::Future;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use instant_http::{AdminRequestContext, ApiError, admin_data_router};
use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};
use instant_server::{AdminDataAuthorizer, AdminDataScope, AdminDataService, migrate_database};
use serde_json::{Value, json};
use tokio_postgres::{NoTls, types::Json};
use tower::ServiceExt as _;
use uuid::Uuid;

#[derive(Clone)]
struct FixtureAuthorizer {
    catalog: AttributeCatalog,
}

impl AdminDataAuthorizer for FixtureAuthorizer {
    fn authorize(
        &self,
        _context: AdminRequestContext,
        _scope: AdminDataScope,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(self.catalog.clone()))
    }

    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(self.catalog.clone()))
    }
}

fn attribute(id: Uuid, etype: &str, label: &str, unique: bool) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique,
        setting_unique: false,
        indexed: unique,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}

async fn insert_attribute(client: &tokio_postgres::Client, app_id: Uuid, attribute: &Attribute) {
    let value_type = match attribute.value_type {
        AttributeValueType::Blob => "blob",
        AttributeValueType::Ref => "ref",
    };
    let cardinality = match attribute.cardinality {
        Cardinality::One => "one",
        Cardinality::Many => "many",
    };
    let forward_ident = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs
             (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
              forward_ident, etype, label, reverse_etype, reverse_label)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            &[
                &attribute.id,
                &app_id,
                &value_type,
                &cardinality,
                &attribute.unique,
                &attribute.indexed,
                &attribute.required,
                &forward_ident,
                &attribute.etype,
                &attribute.label,
                &attribute.reverse_etype,
                &attribute.reverse_label,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label)
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &forward_ident,
                &app_id,
                &attribute.id,
                &attribute.etype,
                &attribute.label,
            ],
        )
        .await
        .unwrap();
}

async fn guest_post(
    router: axum::Router,
    path: &str,
    app_id: Uuid,
    body: Value,
) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header("as-guest", "true")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One migration-backed fixture proves allow, read and deny together.
async fn user_namespace_without_a_rule_allows_guest_writes() {
    let Some(database_url) = instant_test_support::test_database_url(
        "user_namespace_without_a_rule_allows_guest_writes",
    ) else {
        return;
    };
    let (mut setup, setup_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    migrate_database(&mut setup).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'permissions-default')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(id_attr, "todos", "id", true),
            attribute(title_attr, "todos", "title", false),
        ],
    };
    for attribute in &catalog.attributes {
        insert_attribute(&setup, app_id, attribute).await;
    }
    // Deliberately no `rules` row: the namespace has no rule for any action.

    let (verifier, verifier_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let verifier_task = tokio::spawn(verifier_connection);

    let router = admin_data_router(Arc::new(AdminDataService::new(
        setup,
        Arc::new(FixtureAuthorizer {
            catalog: catalog.clone(),
        }),
    )));

    let entity_id = Uuid::new_v4();
    let created = guest_post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [
            ["add-triple", entity_id, id_attr, entity_id],
            ["add-triple", entity_id, title_attr, "no rule needed"]
        ]}),
    )
    .await;
    assert_eq!(
        created.status(),
        StatusCode::OK,
        "a user namespace without a create rule must allow the write"
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &entity_id]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );

    let queried = guest_post(
        router.clone(),
        "/admin/query",
        app_id,
        json!({"query": {"todos": {}}}),
    )
    .await;
    assert_eq!(
        queried.status(),
        StatusCode::OK,
        "a user namespace without a view rule must allow the read"
    );
    let queried = response_json(queried).await;
    assert_eq!(
        queried["todos"],
        json!([{"id": entity_id, "title": "no rule needed"}])
    );

    // Control: the allow above comes from the missing rule, not from a guest
    // write bypass. An explicit deny on the same namespace is enforced.
    verifier
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({"todos": {"allow": {"create": "false"}}})),
            ],
        )
        .await
        .unwrap();

    let denied_entity = Uuid::new_v4();
    let denied = guest_post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [
            ["add-triple", denied_entity, id_attr, denied_entity],
            ["add-triple", denied_entity, title_attr, "denied"]
        ]}),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &denied_entity]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    drop(router);
    drop(verifier);
    verifier_task.await.unwrap().unwrap();
    setup_task.await.unwrap().unwrap();
}
