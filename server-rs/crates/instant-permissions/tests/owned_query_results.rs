use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use instant_datalog::{DatalogResult, DatalogTriple, DatalogValue};
use instant_instaql::{
    AsyncFileUrlBatchFuture, AsyncFileUrlSigner, Attribute, AttributeCatalog, AttributeValueType,
    Cardinality, CompiledQuery, ExecutedEntity, ExecutedNode, compile_query,
    transform_file_results_async, transform_file_results_async_owned,
};
use instant_permissions::{
    EvaluationBindings, PermissionEntityKey, PermissionEntityLoad, PermissionError,
    QueryPermissionOptions, RefCache, RuleSet, filter_executed_query, filter_executed_query_owned,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn attribute(id: u128, etype: &str, label: &str) -> Attribute {
    Attribute {
        id: Uuid::from_u128(id),
        wire: None,
        etype: etype.into(),
        label: label.into(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: label == "id",
        setting_unique: false,
        indexed: label == "id",
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

fn fixture() -> (
    AttributeCatalog,
    CompiledQuery,
    Vec<ExecutedNode>,
    HashMap<PermissionEntityKey, Value>,
) {
    let mut link = attribute(22, "comments", "post");
    link.value_type = AttributeValueType::Ref;
    link.reverse_etype = Some("posts".into());
    link.reverse_label = Some("comments".into());
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(10, "posts", "id"),
            attribute(11, "posts", "title"),
            attribute(12, "posts", "secret"),
            attribute(13, "posts", "ownerId"),
            attribute(20, "comments", "id"),
            attribute(21, "comments", "text"),
            link,
        ],
    };
    let query = compile_query(&json!({"posts": {"comments": {}}}), &catalog, false).unwrap();
    let entity = |id: u128, fields: Vec<(u128, Value)>, children| ExecutedEntity {
        fetch: DatalogResult {
            join_rows: fields
                .into_iter()
                .map(|(attr, value)| {
                    vec![DatalogTriple {
                        entity_id: Uuid::from_u128(id),
                        attr_id: Uuid::from_u128(attr),
                        value: DatalogValue::Json(value),
                        created_at: 5,
                    }]
                })
                .collect(),
            ..DatalogResult::default()
        },
        children,
    };
    let mut data = HashMap::new();
    let mut entities = Vec::new();
    for id in 1..=20 {
        let child_id = 100 + id;
        data.insert(
            PermissionEntityKey {
                etype: "posts".into(),
                entity_id: Uuid::from_u128(id),
            },
            json!({"ownerId": if id % 2 == 0 { "other" } else { "me" }, "title": "visible", "secret": "private"}),
        );
        data.insert(
            PermissionEntityKey {
                etype: "comments".into(),
                entity_id: Uuid::from_u128(child_id),
            },
            json!({"text": "comment", "post": Uuid::from_u128(id)}),
        );
        let child = ExecutedNode {
            selector: DatalogResult::default(),
            aggregate: None,
            entities: vec![entity(
                child_id,
                vec![
                    (20, json!(Uuid::from_u128(child_id))),
                    (21, json!("comment")),
                ],
                vec![],
            )],
        };
        entities.push(entity(
            id,
            vec![
                (10, json!(Uuid::from_u128(id))),
                (11, json!("visible")),
                (12, json!("private")),
            ],
            vec![child],
        ));
    }
    let selector = DatalogResult {
        join_rows: entities
            .iter()
            .map(|e| e.fetch.join_rows[0].clone())
            .collect(),
        ..DatalogResult::default()
    };
    (
        catalog,
        query,
        vec![ExecutedNode {
            selector,
            entities,
            aggregate: None,
        }],
        data,
    )
}

async fn filtered(
    owned: bool,
    catalog: &AttributeCatalog,
    query: &CompiledQuery,
    input: Vec<ExecutedNode>,
    code: Value,
    data: HashMap<PermissionEntityKey, Value>,
) -> (
    Result<Vec<ExecutedNode>, PermissionError>,
    Vec<PermissionEntityLoad>,
) {
    let loaded = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&loaded);
    let load = move |loads| {
        *observed.lock().unwrap() = loads;
        async move { Ok(data) }
    };
    let rules = RuleSet::new(code);
    let bindings = EvaluationBindings {
        auth: json!({"id": "me", "admin": false}),
        ..EvaluationBindings::default()
    };
    let mut refs = RefCache::new();
    let result = if owned {
        filter_executed_query_owned(
            &rules,
            query,
            input,
            catalog,
            &bindings,
            QueryPermissionOptions::default(),
            &mut refs,
            load,
            |_| async { Ok(RefCache::new()) },
        )
        .await
    } else {
        filter_executed_query(
            &rules,
            query,
            &input,
            catalog,
            &bindings,
            QueryPermissionOptions::default(),
            &mut refs,
            load,
            |_| async { Ok(RefCache::new()) },
        )
        .await
    };
    let keys = loaded.lock().unwrap().clone();
    (result, keys)
}

#[tokio::test]
async fn owned_permissions_match_legacy_decisions_loads_and_descendant_removal() {
    let (catalog, query, input, data) = fixture();
    let code = json!({"posts": {"allow": {"view": "auth.id == data.ownerId"},
        "fields": {"secret": "auth.admin"}}, "comments": {"allow": {"view": "false"}}});
    let legacy = filtered(
        false,
        &catalog,
        &query,
        input.clone(),
        code.clone(),
        data.clone(),
    )
    .await;
    let pointer = input.as_ptr();
    let row_pointer = input[0].entities[0].fetch.join_rows[0].as_ptr();
    let owned = filtered(true, &catalog, &query, input, code, data).await;
    assert_eq!(legacy.1, owned.1, "fresh entity loads must not be elided");
    let (legacy, owned) = (legacy.0.unwrap(), owned.0.unwrap());
    assert_eq!(legacy, owned);
    assert_eq!(owned.as_ptr(), pointer);
    assert_eq!(
        owned[0].entities[0].fetch.join_rows[0].as_ptr(),
        row_pointer
    );
    assert_eq!(owned[0].entities.len(), 10);
    assert_eq!(owned[0].selector.join_rows.len(), 10);
    assert!(
        owned[0]
            .entities
            .iter()
            .all(|e| e.children[0].entities.is_empty())
    );
    assert!(
        owned[0]
            .entities
            .iter()
            .flat_map(|e| &e.fetch.join_rows)
            .flatten()
            .all(|t| t.attr_id != Uuid::from_u128(12))
    );
}

#[tokio::test]
async fn owned_permissions_preserve_validation_loader_errors_and_fresh_rule_changes() {
    let (catalog, query, input, data) = fixture();
    for invalid in [
        vec![],
        vec![ExecutedNode {
            entities: vec![ExecutedEntity {
                fetch: DatalogResult::default(),
                children: vec![],
            }],
            selector: DatalogResult::default(),
            aggregate: None,
        }],
    ] {
        let a = filtered(
            false,
            &catalog,
            &query,
            invalid.clone(),
            json!({}),
            data.clone(),
        )
        .await;
        let b = filtered(true, &catalog, &query, invalid, json!({}), data.clone()).await;
        assert_eq!(a.0.unwrap_err().to_string(), b.0.unwrap_err().to_string());
        assert_eq!(a.1, b.1);
    }
    let needs_data = json!({"posts": {"allow": {"view": "true"}}});
    for owned in [false, true] {
        let missing = filtered(
            owned,
            &catalog,
            &query,
            input.clone(),
            needs_data.clone(),
            HashMap::new(),
        )
        .await;
        assert!(matches!(
            missing.0,
            Err(PermissionError::MissingEntityData(_))
        ));
        for allow in ["true", "false", "true"] {
            let code = json!({"posts": {"allow": {"view": allow}}});
            let result = filtered(owned, &catalog, &query, input.clone(), code, data.clone())
                .await
                .0
                .unwrap();
            assert_eq!(
                result[0].entities.len(),
                if allow == "true" { 20 } else { 0 }
            );
        }
    }
}

#[tokio::test]
#[ignore = "manual result ownership benchmark; not a capacity or timing gate"]
async fn owned_result_pipeline_benchmark() {
    struct NoFilesSigner;
    impl AsyncFileUrlSigner for NoFilesSigner {
        fn signed_download_urls(&self, _: Uuid, _: Vec<String>) -> AsyncFileUrlBatchFuture<'_> {
            panic!("a non-file query must not call its file signer")
        }
    }
    let (catalog, query, input, data) = fixture();
    let code = json!({"posts": {"allow": {"view": "auth.id == data.ownerId"}}});
    for owned in [false, true] {
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..2000 {
            let source = input.clone();
            let data = data.clone();
            let code = code.clone();
            let started = std::time::Instant::now();
            if owned {
                let transformed = transform_file_results_async_owned(
                    &query,
                    source,
                    &catalog,
                    Uuid::nil(),
                    &NoFilesSigner,
                )
                .await
                .unwrap();
                std::hint::black_box(
                    filtered(true, &catalog, &query, transformed, code, data)
                        .await
                        .0
                        .unwrap(),
                );
            } else {
                let transformed = transform_file_results_async(
                    &query,
                    &source,
                    &catalog,
                    Uuid::nil(),
                    &NoFilesSigner,
                )
                .await
                .unwrap();
                std::hint::black_box(
                    filtered(false, &catalog, &query, transformed, code, data)
                        .await
                        .0
                        .unwrap(),
                );
            }
            elapsed += started.elapsed();
        }
        println!(
            "query-result-ownership owned={owned} iterations=2000 root_entities=20 elapsed_us={}",
            elapsed.as_micros()
        );
    }
}
