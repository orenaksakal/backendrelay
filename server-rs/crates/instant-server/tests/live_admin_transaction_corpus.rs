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

async fn post(
    router: axum::Router,
    path: &str,
    app_id: Uuid,
    body: Value,
    guest: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("app-id", app_id.to_string())
        .header(header::AUTHORIZATION, format!("Bearer {app_id}"));
    if guest {
        request = request.header("as-guest", "true");
    }
    router
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // The source-pinned route corpus shares one migration-backed fixture.
async fn admin_http_transaction_corpus_covers_operations_errors_and_permissioned_variants() {
    const ORACLE: &str = "instantdb/instant@45d3ffbb server/src/instant/admin/routes.clj:250-375; server/test/instant/db/transaction_test.clj";
    assert!(ORACLE.contains("45d3ffbb"));
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_http_transaction_corpus_covers_operations_errors_and_permissioned_variants",
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
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'admin-transaction-corpus')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let user_id_attr = Uuid::new_v4();
    let user_name_attr = Uuid::new_v4();
    let profile_id_attr = Uuid::new_v4();
    let profile_name_attr = Uuid::new_v4();
    let post_id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let meta_attr = Uuid::new_v4();
    let archived_attr = Uuid::new_v4();
    let author_attr = Uuid::new_v4();
    let mut author = attribute(author_attr, "posts", "author", false);
    author.value_type = AttributeValueType::Ref;
    author.reverse_etype = Some("users".to_owned());
    author.reverse_label = Some("posts".to_owned());
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(user_id_attr, "users", "id", true),
            attribute(user_name_attr, "users", "name", false),
            attribute(profile_id_attr, "profiles", "id", true),
            attribute(profile_name_attr, "profiles", "name", true),
            attribute(post_id_attr, "posts", "id", true),
            attribute(title_attr, "posts", "title", false),
            attribute(meta_attr, "posts", "meta", false),
            attribute(archived_attr, "posts", "archived", false),
            author,
        ],
    };
    for attribute in &catalog.attributes {
        insert_attribute(&setup, app_id, attribute).await;
    }
    setup
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({"posts": {"allow": {"create": "false"}}})),
            ],
        )
        .await
        .unwrap();

    let (verifier, verifier_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let verifier_task = tokio::spawn(verifier_connection);
    let router = admin_data_router(Arc::new(AdminDataService::new(
        setup,
        Arc::new(FixtureAuthorizer {
            catalog: catalog.clone(),
        }),
    )));

    // Oracle: instantdb/instant@45d3ffbb
    // server/test/instant/db/transaction_test.clj:518-545.
    let shared_entity_id = Uuid::new_v4();
    for (id_attr, name_attr, name) in [
        (user_id_attr, user_name_attr, "user"),
        (profile_id_attr, profile_name_attr, "profile"),
    ] {
        let response = post(
            router.clone(),
            "/admin/transact",
            app_id,
            json!({"steps": [
                ["add-triple", shared_entity_id, id_attr, shared_entity_id, {"mode": "create"}],
                ["add-triple", shared_entity_id, name_attr, name, {"mode": "create"}]
            ]}),
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "creating {name}");
    }
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &shared_entity_id]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        4,
        "create mode scopes entity existence to the attribute's entity type"
    );

    let note_attr = Uuid::new_v4();
    let note_entity = Uuid::new_v4();
    let schema_and_write = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [
            ["add-attr", {
                "id": note_attr,
                "forward-identity": [Uuid::new_v4(), "notes", "body"],
                "value-type": "blob",
                "cardinality": "one",
                "unique?": false,
                "index?": false
            }],
            ["add-triple", note_entity, note_attr, "written with its schema"]
        ]}),
        false,
    )
    .await;
    assert_eq!(schema_and_write.status(), StatusCode::OK);
    assert!(response_json(schema_and_write).await["tx-id"].is_i64());
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &note_entity, &note_attr]
            )
            .await
            .unwrap()
            .get::<_, Json<Value>>(0)
            .0,
        json!("written with its schema")
    );

    let raw_entity = Uuid::new_v4();
    let raw = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [
            ["add-triple", raw_entity, post_id_attr, raw_entity],
            ["add-triple", raw_entity, title_attr, "before retract"],
            ["add-triple", raw_entity, meta_attr, {"a": 1, "nested": {"keep": true}}],
            ["deep-merge-triple", raw_entity, meta_attr, {"b": 2, "nested": {"add": true}}],
            ["retract-triple", raw_entity, title_attr, "before retract"],
            ["update-attr", {"id": title_attr, "index?": true}]
        ]}),
        false,
    )
    .await;
    assert_eq!(raw.status(), StatusCode::OK);
    assert!(response_json(raw).await["tx-id"].is_i64());
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &raw_entity, &meta_attr]
            )
            .await
            .unwrap()
            .get::<_, Json<Value>>(0)
            .0,
        json!({"a": 1, "b": 2, "nested": {"keep": true, "add": true}})
    );
    assert_eq!(
        verifier
            .query_one("SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3", &[&app_id, &raw_entity, &title_attr])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert!(
        verifier
            .query_one(
                "SELECT is_indexed FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &title_attr]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let raw_deleted = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [["delete-entity", raw_entity, "posts"]]}),
        false,
    )
    .await;
    assert_eq!(raw_deleted.status(), StatusCode::OK);
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &raw_entity]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let archived = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [["delete-attr", archived_attr]]}),
        false,
    )
    .await;
    assert_eq!(archived.status(), StatusCode::OK);
    assert!(
        verifier
            .query_one(
                "SELECT deletion_marked_at IS NOT NULL FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &archived_attr]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let restored = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [["restore-attr", archived_attr]]}),
        false,
    )
    .await;
    assert_eq!(restored.status(), StatusCode::OK);
    assert!(
        !verifier
            .query_one(
                "SELECT deletion_marked_at IS NOT NULL FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &archived_attr]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );

    let user = Uuid::new_v4();
    let object_post = Uuid::new_v4();
    let object_ops = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [
            ["create", "users", user, {}],
            ["create", "posts", object_post, {"title": "object title", "meta": {"one": true}}],
            ["link", "posts", object_post, {"author": user}],
            ["merge", "posts", object_post, {"meta": {"two": true}}],
            ["unlink", "posts", object_post, {"author": user}]
        ]}),
        false,
    )
    .await;
    assert_eq!(object_ops.status(), StatusCode::OK);
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &object_post, &meta_attr]
            )
            .await
            .unwrap()
            .get::<_, Json<Value>>(0)
            .0,
        json!({"one": true, "two": true})
    );
    assert_eq!(
        verifier
            .query_one("SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3", &[&app_id, &object_post, &author_attr])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    let object_update = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [["update", "posts", object_post, {"title": "object updated"}]]}),
        false,
    )
    .await;
    assert_eq!(object_update.status(), StatusCode::OK);
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &object_post, &title_attr]
            )
            .await
            .unwrap()
            .get::<_, Json<Value>>(0)
            .0,
        json!("object updated")
    );
    let deleted = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [["delete", "posts", object_post]]}),
        false,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &object_post]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let transactions_before_error = verifier
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    let strict_error_entity = Uuid::new_v4();
    let strict_error = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"throw-on-missing-attrs?": true, "steps": [["update", "posts", strict_error_entity, {"unknown": true}]]}),
        false,
    )
    .await;
    assert_eq!(strict_error.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(strict_error).await,
        json!({
            "type": "validation-failed",
            "message": "Validation failed for steps: Attributes are missing in your schema: posts.unknown",
            "hint": {
                "data-type": "steps",
                "input": [["update", "posts", strict_error_entity, {"unknown": true}]],
                "errors": [{
                    "message": "Attributes are missing in your schema: posts.unknown",
                    "in": [0, 3, "unknown"]
                }]
            }
        })
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_error
    );

    let denied_entity = Uuid::new_v4();
    let denied = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({"steps": [["add-triple", denied_entity, title_attr, "denied"]]}),
        true,
    )
    .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(denied).await,
        json!({
            "type": "permission-denied",
            "message": "Permission denied: not perms-pass?",
            "hint": {"input": ["posts", "object"], "expected": "perms-pass?"}
        })
    );
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

    let dry_run_entity = Uuid::new_v4();
    let dry_run = post(
        router.clone(),
        "/admin/transact_perms_check",
        app_id,
        json!({
            "rules-override": {"posts": {"allow": {"create": "false"}}},
            "steps": [["add-triple", dry_run_entity, title_attr, "dry run"]]
        }),
        true,
    )
    .await;
    assert_eq!(dry_run.status(), StatusCode::OK);
    let dry_run = response_json(dry_run).await;
    assert_eq!(dry_run["all-checks-ok?"], false);
    assert_eq!(dry_run["committed?"], false);
    assert_eq!(dry_run["check-results"][0]["action"], "create");
    assert_eq!(dry_run["check-results"][0]["check-pass?"], false);
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &dry_run_entity]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let committed_entity = Uuid::new_v4();
    let committed = post(
        router.clone(),
        "/admin/transact_perms_check",
        app_id,
        json!({
            "rules-override": {"posts": {"allow": {"create": "true"}}},
            "dangerously-commit-tx": true,
            "steps": [
                ["rule-params", committed_entity, "posts", {"source": "corpus"}],
                ["add-triple", committed_entity, title_attr, "committed"]
            ]
        }),
        true,
    )
    .await;
    assert_eq!(committed.status(), StatusCode::OK);
    let committed = response_json(committed).await;
    assert_eq!(committed["all-checks-ok?"], true);
    assert_eq!(committed["committed?"], true);
    assert_eq!(committed["check-results"][0]["action"], "create");
    assert_eq!(committed["check-results"][0]["check-pass?"], true);
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &committed_entity, &title_attr]
            )
            .await
            .unwrap()
            .get::<_, Json<Value>>(0)
            .0,
        json!("committed")
    );

    let rejected_entity = Uuid::new_v4();
    let rejected = post(
        router.clone(),
        "/admin/transact",
        app_id,
        json!({
            "steps": [["add-triple", rejected_entity, title_attr, "must not write"]],
            "enqueue": [{"task":"notify","input":{}}]
        }),
        false,
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id=$1 AND entity_id=$2",
                &[&app_id, &rejected_entity],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(router);
    drop(verifier);
    verifier_task.await.unwrap().unwrap();
    setup_task.abort();
}
