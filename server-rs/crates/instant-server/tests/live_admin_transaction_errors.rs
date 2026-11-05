use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use instant_datalog::DataType;
use instant_http::{AdminRequestContext, ApiError, admin_data_router};
use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};
use instant_server::{AdminDataAuthorizer, AdminDataScope, AdminDataService, migrate_database};
use instant_transact::PathSegment;
use serde_json::{Value, json};
use tokio_postgres::NoTls;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct UpdateModeFixture {
    oracle: String,
    cases: Vec<UpdateModeCase>,
}

#[derive(serde::Deserialize)]
struct UpdateModeCase {
    name: String,
    input: Value,
    outcome: String,
}

#[derive(serde::Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(serde::Deserialize)]
struct PreprocessingFixture {
    source: String,
    cases: Vec<PreprocessingCase>,
}

#[derive(serde::Deserialize)]
struct PreprocessingCase {
    name: String,
    input: Value,
    path: Vec<PathSegment>,
    message: String,
}

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

fn attribute(
    id: Uuid,
    label: &str,
    unique: bool,
    checked_data_type: Option<DataType>,
) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: "posts".to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique,
        setting_unique: false,
        indexed: unique,
        required: false,
        checked_data_type,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}

fn reference_attribute(id: Uuid, label: &str) -> Attribute {
    Attribute {
        value_type: AttributeValueType::Ref,
        ..attribute(id, label, false, None)
    }
}

async fn post_transaction(
    router: axum::Router,
    app_id: Uuid,
    steps: Value,
) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/transact")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header(header::AUTHORIZATION, "Bearer admin-secret")
                .body(Body::from(json!({"steps": steps}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn response_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

fn replace_string(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(string) if string == from => to.clone_into(string),
        Value::Array(values) => {
            for value in values {
                replace_string(value, from, to);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                replace_string(value, from, to);
            }
        }
        _ => {}
    }
}

fn mode_entity_display(entity: &Value) -> String {
    entity
        .as_str()
        .map_or_else(|| entity.to_string(), str::to_owned)
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the migrated HTTP request and rollback proof together.
async fn admin_http_postgres_failures_keep_exact_envelopes_and_roll_back_every_step() {
    const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:812-1156; server/src/instant/util/exception.clj";
    assert!(ORACLE.contains("4e0a1bd8"));
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_http_postgres_failures_keep_exact_envelopes_and_roll_back_every_step",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut client).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'transaction-errors')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let handle_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let score_attr = Uuid::new_v4();
    let author_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(handle_attr, "handle", true, None),
            attribute(title_attr, "title", false, None),
            attribute(score_attr, "score", false, Some(DataType::Number)),
            reference_attribute(author_attr, "author"),
        ],
    };
    for attribute in &catalog.attributes {
        let ident_id = Uuid::new_v4();
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        client
            .execute(
                "INSERT INTO attrs
                   (id, app_id, value_type, cardinality, is_unique, is_indexed,
                    forward_ident, etype, label, checked_data_type)
                 VALUES ($1, $2, $7, 'one', $3, $3, $4, 'posts', $5,
                         $6::text::checked_data_type)",
                &[
                    &attribute.id,
                    &app_id,
                    &attribute.unique,
                    &ident_id,
                    &attribute.label,
                    &attribute
                        .checked_data_type
                        .map(|data_type| format!("{data_type:?}").to_lowercase()),
                    &value_type,
                ],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO idents (id, app_id, attr_id, etype, label)
                 VALUES ($1, $2, $3, 'posts', $4)",
                &[&ident_id, &app_id, &attribute.id, &attribute.label],
            )
            .await
            .unwrap();
    }

    let first_entity = Uuid::new_v4();
    let failed_entity = Uuid::new_v4();
    let router = admin_data_router(Arc::new(AdminDataService::new(
        client,
        Arc::new(FixtureAuthorizer { catalog }),
    )));
    let seeded = post_transaction(
        router.clone(),
        app_id,
        json!([["add-triple", first_entity, handle_attr, "taken"]]),
    )
    .await;
    assert_eq!(seeded.status(), StatusCode::OK);

    let failed = post_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", failed_entity, title_attr, "must-roll-back"],
            ["add-triple", failed_entity, handle_attr, "taken"]
        ]),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&to_bytes(failed.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        body,
        json!({
            "type": "record-not-unique",
            "message": "`handle` is a unique attribute on `posts` and an entity already exists with `posts.handle` = \"taken\"",
            "hint": {
                "record-type": "triple",
                "attr-id": handle_attr,
                "value": "\"taken\"",
                "etype": "posts",
                "label": "handle"
            }
        })
    );

    let checked_entity = Uuid::new_v4();
    let checked = post_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", checked_entity, title_attr, "must-roll-back"],
            ["add-triple", checked_entity, score_attr, "forty-two"]
        ]),
    )
    .await;
    assert_eq!(checked.status(), StatusCode::BAD_REQUEST);
    let checked_body: Value =
        serde_json::from_slice(&to_bytes(checked.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        checked_body,
        json!({
            "type": "validation-failed",
            "message": "Invalid value type for posts.score. Value must be a number but the provided value type is string.",
            "hint": {
                "value": "forty-two",
                "checked-data-type": "number",
                "attr-id": score_attr,
                "entity-id": checked_entity,
                "namespace": "posts",
                "attribute": "score"
            }
        })
    );

    let invalid_reference_entity = Uuid::new_v4();
    let invalid_reference = post_transaction(
        router.clone(),
        app_id,
        json!([
            [
                "add-triple",
                invalid_reference_entity,
                title_attr,
                "must-roll-back"
            ],
            [
                "add-triple",
                invalid_reference_entity,
                author_attr,
                {"foo": "bar"}
            ]
        ]),
    )
    .await;
    assert_eq!(invalid_reference.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_body(invalid_reference).await,
        json!({
            "type": "validation-failed",
            "message": "Linked value must be a valid uuid.",
            "hint": {
                "value": {"foo": "bar"},
                "checked-data-type": null,
                "attr-id": author_attr,
                "entity-id": invalid_reference_entity,
                "namespace": "posts",
                "attribute": "author"
            }
        })
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/model/triple.clj:769-850,1120-1156 and
    // server/test/instant/db/transaction_test.clj:3437-3505.
    let collision_lookup = json!([handle_attr, "lookup-collision"]);
    let lookup_collision = post_transaction(
        router.clone(),
        app_id,
        json!([
            [
                "add-triple",
                [handle_attr, "taken"],
                title_attr,
                "must-roll-back"
            ],
            ["add-triple", collision_lookup, handle_attr, "lookup-moved"]
        ]),
    )
    .await;
    assert_eq!(lookup_collision.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_body(lookup_collision).await,
        json!({
            "type": "validation-failed",
            "message": "Validation failed for lookup: Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists.",
            "hint": {
                "data-type": "lookup",
                "input": [[handle_attr, "taken"], collision_lookup],
                "errors": [{
                    "message": "Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists."
                }]
            }
        })
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/test/instant/db/transaction_test.clj:5805-5815 and
    // server/src/instant/util/exception.clj:743-751.
    let null_byte_entity = Uuid::new_v4();
    let null_byte = post_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", null_byte_entity, title_attr, "must-roll-back"],
            [
                "add-triple",
                null_byte_entity,
                title_attr,
                "Hello\u{0000}World"
            ]
        ]),
    )
    .await;
    assert_eq!(null_byte.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_body(null_byte).await,
        json!({
            "type": "validation-failed",
            "message": "A value in the transaction contains null bytes, which cannot be stored.",
            "hint": {
                "condition": "untranslatable-character"
            }
        })
    );

    let oversized_entity = Uuid::new_v4();
    let mut oversized_value = String::new();
    for index in 0..400 {
        write!(&mut oversized_value, "{index:04x}").unwrap();
    }
    let oversized = post_transaction(
        router,
        app_id,
        json!([
            ["add-triple", oversized_entity, title_attr, "must-roll-back"],
            ["add-triple", oversized_entity, handle_attr, oversized_value]
        ]),
    )
    .await;
    assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);
    let oversized_body: Value =
        serde_json::from_slice(&to_bytes(oversized.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let oversized_hint = format!("\"{}...", &oversized_value[..63]);
    assert_eq!(
        oversized_body,
        json!({
            "type": "validation-failed",
            "message": "Value is too large for a unique attribute.",
            "hint": {
                "value": oversized_hint,
                "checked-data-type": null,
                "attr-id": handle_attr,
                "entity-id": oversized_entity,
                "namespace": "posts",
                "attribute": "handle",
                "value-too-large?": true
            }
        })
    );

    let (verifier, verifier_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let verifier_task = tokio::spawn(verifier_connection);
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "all six failed requests must roll back their transaction records"
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &failed_entity],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "the valid earlier step must roll back with the unique failure"
    );
    for (entity_id, family) in [
        (checked_entity, "checked-type"),
        (invalid_reference_entity, "invalid-reference"),
        (null_byte_entity, "null-byte"),
        (oversized_entity, "oversized-indexed-value"),
    ] {
        assert_eq!(
            verifier
                .query_one(
                    "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                    &[&app_id, &entity_id],
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0,
            "the valid earlier step must roll back with the {family} failure"
        );
    }
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND attr_id = $2 AND value IN ($3, $4)",
                &[
                    &app_id,
                    &handle_attr,
                    &tokio_postgres::types::Json(json!("lookup-collision")),
                    &tokio_postgres::types::Json(json!("lookup-moved")),
                ],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "lookup collision rolls back its implicit and requested values"
    );

    drop(verifier);
    verifier_task.await.unwrap().unwrap();
    connection_task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One source-pinned matrix proves the public route and rollback boundary.
async fn admin_http_update_modes_match_the_source_pinned_execution_matrix() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_http_update_modes_match_the_source_pinned_execution_matrix",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut client).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'transaction-modes')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let fixture_book_id_attr = "77777777-7777-7777-7777-777777777777";
    let fixture_book_title_attr = "88888888-8888-8888-8888-888888888888";
    let book_id_attr = Uuid::new_v4();
    let book_title_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            Attribute {
                id: book_id_attr,
                wire: None,
                etype: "books".to_owned(),
                label: "id".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: true,
                setting_unique: false,
                indexed: true,
                required: true,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            },
            Attribute {
                id: book_title_attr,
                wire: None,
                etype: "books".to_owned(),
                label: "title".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: false,
                setting_unique: false,
                indexed: false,
                required: false,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            },
        ],
    };
    for attribute in &catalog.attributes {
        let ident_id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO attrs
                   (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
                    forward_ident, etype, label)
                 VALUES ($1, $2, 'blob', 'one', $3, $4, $5, $6, 'books', $7)",
                &[
                    &attribute.id,
                    &app_id,
                    &attribute.unique,
                    &attribute.indexed,
                    &attribute.required,
                    &ident_id,
                    &attribute.label,
                ],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO idents (id, app_id, attr_id, etype, label)
                 VALUES ($1, $2, $3, 'books', $4)",
                &[&ident_id, &app_id, &attribute.id, &attribute.label],
            )
            .await
            .unwrap();
    }

    let existing_book = Uuid::parse_str("aaaaaaaa-1000-0000-0000-000000000300").unwrap();
    let router = admin_data_router(Arc::new(AdminDataService::new(
        client,
        Arc::new(FixtureAuthorizer {
            catalog: catalog.clone(),
        }),
    )));
    let seeded = post_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", existing_book, book_id_attr, existing_book],
            [
                "add-triple",
                existing_book,
                book_title_attr,
                "mode-existing"
            ]
        ]),
    )
    .await;
    assert_eq!(seeded.status(), StatusCode::OK);
    assert_eq!(response_body(seeded).await.as_object().unwrap().len(), 1);

    let (verifier, verifier_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let verifier_task = tokio::spawn(verifier_connection);
    let fixture: UpdateModeFixture = serde_json::from_str(include_str!(
        "../../instant-transact/tests/fixtures/update_modes.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));
    assert_eq!(fixture.cases.len(), 16);

    for mut case in fixture.cases {
        replace_string(
            &mut case.input,
            fixture_book_id_attr,
            &book_id_attr.to_string(),
        );
        replace_string(
            &mut case.input,
            fixture_book_title_attr,
            &book_title_attr.to_string(),
        );
        let tuple = case.input.as_array().unwrap()[0].as_array().unwrap();
        let requested_entity = tuple[1].clone();
        let expected_value = tuple[3].clone();
        let transactions_before = verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        let existing_title = verifier
            .query_opt(
                "SELECT value FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &existing_book, &book_title_attr],
            )
            .await
            .unwrap()
            .map(|row| row.get::<_, tokio_postgres::types::Json<Value>>(0).0);
        let response = post_transaction(router.clone(), app_id, case.input.clone()).await;

        if case.outcome == "ok" {
            assert_eq!(response.status(), StatusCode::OK, "{}", case.name);
            let body = response_body(response).await;
            let transaction_id = body["tx-id"].as_i64().unwrap();
            assert_eq!(body, json!({"tx-id": transaction_id}), "{}", case.name);
            assert_eq!(
                verifier
                    .query_one(
                        "SELECT count(*) FROM transactions WHERE app_id = $1 AND id = $2",
                        &[&app_id, &transaction_id],
                    )
                    .await
                    .unwrap()
                    .get::<_, i64>(0),
                1,
                "{}",
                case.name
            );
            let entity_id = if let Some(entity_id) = requested_entity.as_str() {
                Uuid::parse_str(entity_id).unwrap()
            } else {
                let lookup = requested_entity.as_array().unwrap();
                verifier
                    .query_one(
                        "SELECT entity_id FROM triples
                         WHERE app_id = $1 AND attr_id = $2 AND value = $3 AND av",
                        &[
                            &app_id,
                            &Uuid::parse_str(lookup[0].as_str().unwrap()).unwrap(),
                            &tokio_postgres::types::Json(lookup[1].clone()),
                        ],
                    )
                    .await
                    .unwrap()
                    .get(0)
            };
            assert_eq!(
                verifier
                    .query_one(
                        "SELECT value FROM triples
                         WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                        &[&app_id, &entity_id, &book_title_attr],
                    )
                    .await
                    .unwrap()
                    .get::<_, tokio_postgres::types::Json<Value>>(0)
                    .0,
                expected_value,
                "{}",
                case.name
            );
            assert_eq!(
                verifier
                    .query_one(
                        "SELECT count(*) FROM transactions WHERE app_id = $1",
                        &[&app_id],
                    )
                    .await
                    .unwrap()
                    .get::<_, i64>(0),
                transactions_before + 1,
                "{}",
                case.name
            );
            continue;
        }

        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", case.name);
        let entity_display = mode_entity_display(&requested_entity);
        let detail = match case.outcome.as_str() {
            "create-existing" => format!("Creating entities that exist: {entity_display}"),
            "update-missing" => format!("Updating entities that don't exist: {entity_display}"),
            outcome => panic!("{} has unknown fixture outcome {outcome}", case.name),
        };
        let requested_step = case.input[0].as_array().unwrap();
        assert_eq!(
            response_body(response).await,
            json!({
                "type": "validation-failed",
                "message": format!("Validation failed for tx-step: {detail}"),
                "hint": {
                    "data-type": "tx-step",
                    "input": [{
                        "op": requested_step[0],
                        "eid": requested_step[1],
                        "etype": "books",
                        "aid": requested_step[2],
                        "value": requested_step[3],
                        "rev-etype": null,
                        "opts": requested_step[4],
                    }],
                    "errors": [{"message": detail}],
                },
            }),
            "{}",
            case.name
        );
        assert_eq!(
            verifier
                .query_one(
                    "SELECT count(*) FROM transactions WHERE app_id = $1",
                    &[&app_id],
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            transactions_before,
            "{}",
            case.name
        );
        if let Some(entity_id) = requested_entity.as_str() {
            let entity_id = Uuid::parse_str(entity_id).unwrap();
            if case.outcome == "update-missing" {
                assert_eq!(
                    verifier
                        .query_one(
                            "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                            &[&app_id, &entity_id],
                        )
                        .await
                        .unwrap()
                        .get::<_, i64>(0),
                    0,
                    "{}",
                    case.name
                );
            } else {
                assert_eq!(
                    verifier
                        .query_one(
                            "SELECT value FROM triples
                             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                            &[&app_id, &entity_id, &book_title_attr],
                        )
                        .await
                        .unwrap()
                        .get::<_, tokio_postgres::types::Json<Value>>(0)
                        .0,
                    existing_title.unwrap(),
                    "{}",
                    case.name
                );
            }
        } else {
            let lookup = requested_entity.as_array().unwrap();
            let lookup_value = tokio_postgres::types::Json(lookup[1].clone());
            let residue = verifier
                .query_one(
                    "SELECT count(*) FROM triples
                     WHERE app_id = $1 AND attr_id = $2 AND value = $3",
                    &[&app_id, &book_id_attr, &lookup_value],
                )
                .await
                .unwrap()
                .get::<_, i64>(0);
            assert_eq!(
                residue,
                i64::from(case.outcome == "create-existing"),
                "{} implicit lookup residue",
                case.name
            );
            if case.outcome == "create-existing" {
                assert_eq!(
                    verifier
                        .query_one(
                            "SELECT value FROM triples
                             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                            &[&app_id, &existing_book, &book_title_attr],
                        )
                        .await
                        .unwrap()
                        .get::<_, tokio_postgres::types::Json<Value>>(0)
                        .0,
                    existing_title.unwrap(),
                    "{}",
                    case.name
                );
            }
        }
    }

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/transaction.clj:283-358 validate-mode checks the
    // complete create set before the complete update set.
    let missing_book = Uuid::new_v4();
    let existing_title = verifier
        .query_one(
            "SELECT value FROM triples
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &existing_book, &book_title_attr],
        )
        .await
        .unwrap()
        .get::<_, tokio_postgres::types::Json<Value>>(0)
        .0;
    let transactions_before_precedence = verifier
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    let precedence = post_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", missing_book, book_title_attr, "missing-update", {"mode": "update"}],
            ["add-triple", existing_book, book_title_attr, "existing-create", {"mode": "create"}]
        ]),
    )
    .await;
    assert_eq!(precedence.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_body(precedence).await,
        json!({
            "type": "validation-failed",
            "message": format!("Validation failed for tx-step: Creating entities that exist: {existing_book}"),
            "hint": {
                "data-type": "tx-step",
                "input": [{
                    "op": "add-triple",
                    "eid": existing_book,
                    "etype": "books",
                    "aid": book_title_attr,
                    "value": "existing-create",
                    "rev-etype": null,
                    "opts": {"mode": "create"},
                }],
                "errors": [{
                    "message": format!("Creating entities that exist: {existing_book}")
                }],
            },
        })
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_precedence
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &missing_book],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &existing_book, &book_title_attr],
            )
            .await
            .unwrap()
            .get::<_, tokio_postgres::types::Json<Value>>(0)
            .0,
        existing_title
    );

    let incomplete_book = Uuid::new_v4();
    let seeded_incomplete = post_transaction(
        router.clone(),
        app_id,
        json!([["add-triple", incomplete_book, book_id_attr, incomplete_book]]),
    )
    .await;
    assert_eq!(seeded_incomplete.status(), StatusCode::OK);
    let transactions_before_required = verifier
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    let invalid_required = post_transaction(
        router.clone(),
        app_id,
        json!([["update-attr", {"id": book_title_attr, "required?": true}]]),
    )
    .await;
    assert_eq!(invalid_required.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_body(invalid_required).await,
        json!({
            "type": "validation-failed",
            "message": "Validation failed for attributes: Can't update attribute `title` to required because `books` already have entities without it",
            "hint": {
                "data-type": "attributes",
                "input": {"id": book_title_attr},
                "errors": [{
                    "message": "Can't update attribute `title` to required because `books` already have entities without it"
                }]
            }
        })
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_required
    );
    assert!(
        !verifier
            .query_one(
                "SELECT is_required FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &book_title_attr],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );

    let repaired_required = post_transaction(
        router.clone(),
        app_id,
        json!([
            ["update-attr", {"id": book_title_attr, "required?": true}],
            [
                "add-triple",
                incomplete_book,
                book_title_attr,
                "same-transaction repair"
            ]
        ]),
    )
    .await;
    assert_eq!(repaired_required.status(), StatusCode::OK);
    assert!(response_body(repaired_required).await["tx-id"].is_i64());
    assert!(
        verifier
            .query_one(
                "SELECT is_required FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &book_title_attr],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &incomplete_book, &book_title_attr],
            )
            .await
            .unwrap()
            .get::<_, tokio_postgres::types::Json<Value>>(0)
            .0,
        json!("same-transaction repair")
    );

    drop(router);
    drop(verifier);
    verifier_task.await.unwrap().unwrap();
    connection_task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One source-pinned matrix proves the public validation boundary.
async fn admin_http_preprocessing_errors_match_the_source_pinned_validation_matrix() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_http_preprocessing_errors_match_the_source_pinned_validation_matrix",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    migrate_database(&mut client).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'transaction-validation')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let catalog: CatalogFixture = serde_json::from_str(include_str!(
        "../../instant-instaql/tests/fixtures/where_compilation.json"
    ))
    .unwrap();
    let router = admin_data_router(Arc::new(AdminDataService::new(
        client,
        Arc::new(FixtureAuthorizer {
            catalog: catalog.catalog,
        }),
    )));
    let (verifier, verifier_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let verifier_task = tokio::spawn(verifier_connection);
    let fixture: PreprocessingFixture = serde_json::from_str(include_str!(
        "../../instant-transact/tests/fixtures/preprocessing.json"
    ))
    .unwrap();
    assert!(fixture.source.contains("instantdb/instant@4e0a1bd8"));
    assert_eq!(fixture.cases.len(), 19);

    for case in fixture.cases {
        let transactions_before = verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        let response = post_transaction(router.clone(), app_id, case.input.clone()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", case.name);
        let expected = if case.name == "root-not-collection" {
            json!({
                "type": "param-malformed",
                "message": "Malformed parameter: [body steps]",
                "hint": {
                    "in": "[body steps]",
                    "original-input": case.input,
                },
            })
        } else {
            json!({
                "type": "validation-failed",
                "message": format!("Validation failed for steps: {}", case.message),
                "hint": {
                    "data-type": "steps",
                    "input": case.input,
                    "errors": [{
                        "message": case.message,
                        "in": case.path,
                    }],
                },
            })
        };
        assert_eq!(response_body(response).await, expected, "{}", case.name);
        assert_eq!(
            verifier
                .query_one(
                    "SELECT count(*) FROM transactions WHERE app_id = $1",
                    &[&app_id],
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            transactions_before,
            "{}",
            case.name
        );
        assert_eq!(
            verifier
                .query_one("SELECT count(*) FROM attrs WHERE app_id = $1", &[&app_id],)
                .await
                .unwrap()
                .get::<_, i64>(0),
            0,
            "{}",
            case.name
        );
        assert_eq!(
            verifier
                .query_one("SELECT count(*) FROM triples WHERE app_id = $1", &[&app_id],)
                .await
                .unwrap()
                .get::<_, i64>(0),
            0,
            "{}",
            case.name
        );
    }

    drop(router);
    drop(verifier);
    verifier_task.await.unwrap().unwrap();
    connection_task.abort();
}
