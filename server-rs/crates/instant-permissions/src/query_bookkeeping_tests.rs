//! Frozen pre-optimization bookkeeping, independent of the current helpers.
use super::*;
use instant_datalog::{
    DatalogResult, DatalogTriple, DatalogValue, PageCursorResult, PageResultInfo,
};
use instant_instaql::{Attribute, AttributeValueType, Cardinality, ExecutedEntity, compile_query};
use serde_json::json;

fn legacy_subjects(
    nodes: &[CompiledNode],
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    subjects: &mut HashMap<PermissionEntityKey, HashSet<String>>,
) -> Result<(), PermissionError> {
    if nodes.len() != results.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: nodes.len(),
            actual: results.len(),
        });
    }
    for (node, result) in nodes.iter().zip(results) {
        for entity in &result.entities {
            let entity_id = result_entity_id(&entity.fetch)?;
            let key = PermissionEntityKey {
                etype: node.etype.clone(),
                entity_id,
            };
            let labels = subjects.entry(key).or_default();
            for triple in entity.fetch.join_rows.iter().flatten() {
                let attribute = catalog
                    .by_id(triple.attr_id)
                    .ok_or(PermissionError::UnknownAttribute(triple.attr_id))?;
                labels.insert(attribute.label.clone());
            }
            legacy_subjects(&node.children, &entity.children, catalog, subjects)?;
        }
    }
    Ok(())
}

fn legacy_filter(
    nodes: &[CompiledNode],
    results: &mut [ExecutedNode],
    catalog: &AttributeCatalog,
    decisions: &HashMap<DecisionKey, bool>,
) -> Result<(), PermissionError> {
    if nodes.len() != results.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: nodes.len(),
            actual: results.len(),
        });
    }
    for (node, result) in nodes.iter().zip(results) {
        let mut retained = Vec::new();
        let mut allowed_ids = HashSet::new();
        for mut entity in std::mem::take(&mut result.entities) {
            let entity_id = result_entity_id(&entity.fetch)?;
            let key = PermissionEntityKey {
                etype: node.etype.clone(),
                entity_id,
            };
            if !decisions
                .get(&DecisionKey::View(key.clone()))
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            entity.fetch.join_rows.retain(|row| {
                row.iter().all(|triple| {
                    catalog.by_id(triple.attr_id).is_some_and(|attribute| {
                        decisions
                            .get(&DecisionKey::Field(key.clone(), attribute.label.clone()))
                            .copied()
                            .unwrap_or(true)
                    })
                })
            });
            if entity.fetch.join_rows.is_empty() {
                continue;
            }
            legacy_filter(&node.children, &mut entity.children, catalog, decisions)?;
            allowed_ids.insert(entity_id);
            retained.push(entity);
        }
        result.entities = retained;
        result.selector.join_rows.retain(|row| {
            row.first()
                .is_some_and(|triple| allowed_ids.contains(&triple.entity_id))
        });
        if let Some(page_info) = &mut result.selector.page_info {
            let cursor_attr_id = page_info
                .start_cursor
                .as_ref()
                .or(page_info.end_cursor.as_ref())
                .map(|cursor| cursor.attr_id);
            if let Some(attr_id) = cursor_attr_id {
                page_info.start_cursor = result
                    .selector
                    .join_rows
                    .first()
                    .and_then(|row| page_cursor_from_row(row, attr_id));
                page_info.end_cursor = result
                    .selector
                    .join_rows
                    .last()
                    .and_then(|row| page_cursor_from_row(row, attr_id));
            }
        }
    }
    Ok(())
}

pub(super) fn fixture() -> (
    AttributeCatalog,
    CompiledQuery,
    Vec<ExecutedNode>,
    HashMap<PermissionEntityKey, JsonValue>,
) {
    let mut attributes = Vec::new();
    for (etype, labels) in [
        ("posts", vec!["id", "title", "secret", "ownerId"]),
        ("comments", vec!["id", "text", "post"]),
    ] {
        for label in labels {
            let reference = label == "post";
            attributes.push(Attribute {
                id: Uuid::from_u128(attributes.len() as u128 + 1),
                wire: None,
                etype: etype.into(),
                label: label.into(),
                value_type: if reference {
                    AttributeValueType::Ref
                } else {
                    AttributeValueType::Blob
                },
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: label == "id",
                required: false,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: reference.then(|| "posts".into()),
                reverse_label: reference.then(|| "comments".into()),
                on_delete: None,
                on_delete_reverse: None,
            });
        }
    }
    let catalog = AttributeCatalog { attributes };
    let query = compile_query(
        &json!({"posts": {"$": {"limit": 12}, "comments": {}}}),
        &catalog,
        false,
    )
    .unwrap();
    let triple = |id, attr, value| DatalogTriple {
        entity_id: Uuid::from_u128(id),
        attr_id: Uuid::from_u128(attr),
        value: DatalogValue::Json(value),
        created_at: 42,
    };
    let mut data = HashMap::new();
    let mut entities = Vec::new();
    for id in 1..=12 {
        // Reusing the UUID in a different etype must not merge decisions.
        let child = ExecutedEntity {
            fetch: DatalogResult {
                join_rows: vec![
                    vec![triple(id, 5, json!(Uuid::from_u128(id)))],
                    vec![triple(id, 6, json!("comment"))],
                ],
                ..Default::default()
            },
            children: vec![],
        };
        let child_selector = DatalogResult {
            join_rows: vec![child.fetch.join_rows[0].clone()],
            ..Default::default()
        };
        let child = ExecutedNode {
            selector: child_selector,
            entities: vec![child],
            aggregate: None,
        };
        entities.push(ExecutedEntity {
            fetch: DatalogResult {
                join_rows: vec![
                    vec![triple(id, 1, json!(Uuid::from_u128(id)))],
                    vec![triple(id, 2, json!("title"))],
                    vec![triple(id, 3, json!({"private": [null, 1, "secret"]}))],
                ],
                ..Default::default()
            },
            children: vec![child],
        });
        data.insert(PermissionEntityKey { etype: "posts".into(), entity_id: Uuid::from_u128(id) },
            json!({"id": Uuid::from_u128(id), "ownerId": if id % 2 == 0 {"me"} else {"other"},
                   "title": "title", "secret": {"private": [null, 1, "secret"]}, "unfetched": "must remain complete"}));
        data.insert(
            PermissionEntityKey {
                etype: "comments".into(),
                entity_id: Uuid::from_u128(id),
            },
            json!({"id": Uuid::from_u128(id), "text": "comment", "post": Uuid::from_u128(id)}),
        );
    }
    let cursor = |id| PageCursorResult {
        entity_id: Uuid::from_u128(id),
        attr_id: Uuid::from_u128(1),
        value: Some(DatalogValue::Json(json!(Uuid::from_u128(id)))),
        created_at: 42,
    };
    let selector = DatalogResult {
        join_rows: entities
            .iter()
            .map(|e| e.fetch.join_rows[0].clone())
            .collect(),
        page_info: Some(PageResultInfo {
            start_cursor: Some(cursor(1)),
            end_cursor: Some(cursor(12)),
            has_next_page: true,
            has_previous_page: false,
        }),
        ..Default::default()
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

fn owned_subjects(
    subjects: HashMap<PermissionEntityKey, HashSet<&str>>,
) -> HashMap<PermissionEntityKey, HashSet<String>> {
    subjects
        .into_iter()
        .map(|(key, labels)| (key, labels.into_iter().map(str::to_owned).collect()))
        .collect()
}

#[test]
fn borrowed_labels_match_frozen_collection_and_first_catalog_entry_without_copies() {
    let (mut catalog, query, mut results, _) = fixture();
    let mut duplicate = catalog.attributes[1].clone();
    duplicate.label = "must-not-shadow-first-entry".into();
    catalog.attributes.push(duplicate);
    let repeated = results[0].entities[0].fetch.join_rows[1].clone();
    results[0].entities[0].fetch.join_rows.push(repeated);
    let mut expected = HashMap::new();
    legacy_subjects(&query.roots, &results, &catalog, &mut expected).unwrap();
    let mut actual = HashMap::new();
    collect_permission_subjects(&query.roots, &results, &catalog, &mut actual).unwrap();
    assert_eq!(actual.len(), 24);
    for labels in actual.values() {
        for label in labels {
            assert!(
                catalog
                    .attributes
                    .iter()
                    .any(|a| std::ptr::eq(a.label.as_str(), *label)),
                "a collected label must borrow this request's catalog"
            );
        }
    }
    assert_eq!(owned_subjects(actual), expected);
}

#[test]
fn collection_keeps_cardinality_empty_entity_unknown_attribute_and_child_error_priority() {
    let (catalog, query, results, _) = fixture();
    let mut cases = vec![Vec::new()];
    let mut empty = results.clone();
    empty[0].entities[0].fetch.join_rows.clear();
    cases.push(empty);
    let mut unknown = results.clone();
    unknown[0].entities[0].fetch.join_rows[0][0].attr_id = Uuid::nil();
    cases.push(unknown);
    let mut children = results.clone();
    children[0].entities[0].children.clear();
    cases.push(children);
    let mut ordered = results.clone();
    ordered[0].entities[0].fetch.join_rows.clear();
    ordered[0].entities[1].fetch.join_rows[0][0].attr_id = Uuid::nil();
    cases.push(ordered);
    for (index, results) in cases.iter().enumerate() {
        let mut expected = HashMap::new();
        let mut actual = HashMap::new();
        let old = legacy_subjects(&query.roots, results, &catalog, &mut expected)
            .unwrap_err()
            .to_string();
        let new = collect_permission_subjects(&query.roots, results, &catalog, &mut actual)
            .unwrap_err()
            .to_string();
        assert_eq!(new, old, "case {index}");
        assert_eq!(owned_subjects(actual), expected, "partial case {index}");
    }
}

fn compare_filter(
    query: &CompiledQuery,
    input: &[ExecutedNode],
    catalog: &AttributeCatalog,
    decisions: &HashMap<DecisionKey, bool>,
) {
    let mut expected = input.to_vec();
    let mut actual = input.to_vec();
    let old = legacy_filter(&query.roots, &mut expected, catalog, decisions);
    let new = filter_executed_level(&query.roots, &mut actual, catalog, decisions);
    assert_eq!(
        old.err().map(|e| e.to_string()),
        new.err().map(|e| e.to_string())
    );
    assert_eq!(
        actual, expected,
        "complete trees, selectors, descendants, topics and cursors"
    );
}

#[test]
fn no_field_decisions_keep_views_unknown_attribute_rejection_descendants_and_page_boundaries() {
    let (catalog, query, mut input, _) = fixture();
    let mut unknown = input[0].entities[0].fetch.join_rows[1].clone();
    unknown[0].attr_id = Uuid::nil();
    input[0].entities[0].fetch.join_rows.push(unknown);
    for seed in 0..32 {
        let mut decisions = HashMap::new();
        for id in 1..=12 {
            if (id + seed) % 3 != 0 {
                decisions.insert(
                    DecisionKey::View(PermissionEntityKey {
                        etype: "posts".into(),
                        entity_id: Uuid::from_u128(id),
                    }),
                    (id + seed) % 2 == 0,
                );
            }
            if (id + seed) % 4 == 0 {
                decisions.insert(
                    DecisionKey::View(PermissionEntityKey {
                        etype: "comments".into(),
                        entity_id: Uuid::from_u128(id),
                    }),
                    false,
                );
            }
        }
        compare_filter(&query, &input, &catalog, &decisions);
    }
    compare_filter(&query, &input, &catalog, &HashMap::new());
}

#[test]
fn any_field_decision_retains_full_per_entity_field_checks_across_levels_and_mixed_rows() {
    let (catalog, query, mut input, _) = fixture();
    let combined = input[0].entities[0].fetch.join_rows[1..]
        .iter()
        .flatten()
        .cloned()
        .collect();
    input[0].entities[0].fetch.join_rows.push(combined);
    for seed in 0..32 {
        let mut decisions = HashMap::new();
        decisions.insert(
            DecisionKey::Field(
                PermissionEntityKey {
                    etype: "unrelated".into(),
                    entity_id: Uuid::nil(),
                },
                "title".into(),
            ),
            false,
        );
        for id in 1..=12 {
            let key = PermissionEntityKey {
                etype: "posts".into(),
                entity_id: Uuid::from_u128(id),
            };
            decisions.insert(DecisionKey::View(key.clone()), (id + seed) % 5 != 0);
            decisions.insert(
                DecisionKey::Field(key, "secret".into()),
                (id + seed) % 2 == 0,
            );
            let key = PermissionEntityKey {
                etype: "comments".into(),
                entity_id: Uuid::from_u128(id),
            };
            decisions.insert(DecisionKey::Field(key, "text".into()), (id + seed) % 3 != 0);
        }
        compare_filter(&query, &input, &catalog, &decisions);
    }
}

#[tokio::test]
async fn public_filter_and_audit_keep_fresh_complete_entity_loads_identity_and_field_decisions() {
    let (catalog, query, input, data) = fixture();
    let rules = RuleSet::new(
        json!({"posts":{"allow":{"view":"auth.id == data.ownerId"},"fields":{"secret":"auth.admin"}},
                                "comments":{"allow":{"view":"true"}}}),
    );
    for (identity, admin) in [("me", false), ("other", false), ("me", true)] {
        let bindings = EvaluationBindings {
            auth: json!({"id":identity,"admin":admin}),
            ..Default::default()
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let loads = Arc::clone(&observed);
        let fresh = data.clone();
        let filtered = filter_executed_query_owned(
            &rules,
            &query,
            input.clone(),
            &catalog,
            &bindings,
            QueryPermissionOptions {
                attribute_projection: false,
            },
            &mut RefCache::new(),
            move |keys| {
                loads.lock().unwrap().push(keys);
                async move { Ok(fresh) }
            },
            |_| async { panic!("these rules have no reference loads") },
        )
        .await
        .unwrap();
        let loads = Arc::clone(&observed);
        let fresh = data.clone();
        let audit = audit_executed_query(
            &rules,
            &query,
            &input,
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            &mut RefCache::new(),
            move |keys| {
                loads.lock().unwrap().push(keys);
                async move { Ok(fresh) }
            },
            |_| async { panic!("these rules have no reference loads") },
        )
        .await
        .unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0], observed[1]);
        assert_eq!(
            observed[0]
                .iter()
                .map(|load| load.entities.len())
                .sum::<usize>(),
            24
        );
        assert_eq!(audit.results, filtered);
        assert_eq!(audit.checks.len(), 36);
        assert!(
            audit
                .checks
                .iter()
                .filter(|c| c.etype == "posts")
                .all(|c| c.record["unfetched"] == "must remain complete")
        );
        let mut decisions = HashMap::new();
        for (key, record) in &data {
            decisions.insert(
                DecisionKey::View(key.clone()),
                key.etype == "comments" || record["ownerId"] == identity,
            );
            if key.etype == "posts" {
                decisions.insert(DecisionKey::Field(key.clone(), "secret".into()), admin);
            }
        }
        let mut expected = input.clone();
        legacy_filter(&query.roots, &mut expected, &catalog, &decisions).unwrap();
        assert_eq!(filtered, expected);
        assert_eq!(filtered[0].entities.len(), 6);
    }
}

#[test]
#[ignore = "manual query-local bookkeeping benchmark; not a capacity or timing gate"]
fn bookkeeping_benchmark() {
    let (catalog, query, input, _) = fixture();
    let decisions = HashMap::new();
    for legacy in [true, false, true, false] {
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..5000 {
            let mut results = input.clone();
            let started = std::time::Instant::now();
            if legacy {
                let mut subjects = HashMap::new();
                legacy_subjects(&query.roots, &results, &catalog, &mut subjects).unwrap();
                legacy_filter(&query.roots, &mut results, &catalog, &decisions).unwrap();
                drop(subjects);
            } else {
                let mut subjects = HashMap::new();
                collect_permission_subjects(&query.roots, &results, &catalog, &mut subjects)
                    .unwrap();
                filter_executed_level(&query.roots, &mut results, &catalog, &decisions).unwrap();
                drop(subjects);
            }
            elapsed += started.elapsed();
            std::hint::black_box(results);
        }
        eprintln!(
            "bookkeeping legacy={legacy}: {elapsed:?} for 5000 synthetic query-local passes; not capacity evidence"
        );
    }
}
