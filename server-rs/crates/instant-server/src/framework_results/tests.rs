use std::hint::black_box;
use std::time::Instant;

use instant_datalog::PageResultInfo;
use instant_instaql::ExecutedEntity;
use uuid::Uuid;

use super::*;

// Kept literally equivalent to the previous owned-JSON projection as an oracle.
fn legacy(roots: &[CompiledNode], results: &[ExecutedNode]) -> Value {
    fn collect(node: &CompiledNode, result: &ExecutedNode, rows: &mut BTreeMap<String, Value>) {
        fn add(result: &DatalogResult, rows: &mut BTreeMap<String, Value>) {
            for triple in result.join_rows.iter().flatten() {
                let value = json!([
                    triple.entity_id,
                    triple.attr_id,
                    match &triple.value {
                        DatalogValue::Uuid(value) => json!(value),
                        DatalogValue::Json(value) => value.clone(),
                    },
                    triple.created_at,
                ]);
                rows.insert(value.to_string(), value);
            }
        }
        add(&result.selector, rows);
        for entity in &result.entities {
            add(&entity.fetch, rows);
            for (child, child_result) in node.children.iter().zip(&entity.children) {
                collect(child, child_result, rows);
            }
        }
    }
    let mut triples = BTreeMap::new();
    let mut page_info = Map::new();
    let mut aggregates = Map::new();
    for (root, result) in roots.iter().zip(results) {
        collect(root, result, &mut triples);
        if let Some(info) = &result.selector.page_info {
            page_info.insert(
                root.key.clone(),
                json!({
                    "start-cursor": info.start_cursor.as_ref().map(framework_cursor),
                    "end-cursor": info.end_cursor.as_ref().map(framework_cursor),
                    "has-next-page?": info.has_next_page,
                    "has-previous-page?": info.has_previous_page,
                }),
            );
        }
        if let Some(count) = result.aggregate {
            aggregates.insert(root.key.clone(), json!({"count": count}));
        }
    }
    let mut data = Map::from_iter([(
        "datalog-result".to_owned(),
        json!({"join-rows": [triples.into_values().collect::<Vec<_>>()]}),
    )]);
    if !page_info.is_empty() {
        data.insert("page-info".to_owned(), Value::Object(page_info));
    }
    if !aggregates.is_empty() {
        data.insert("aggregate".to_owned(), Value::Object(aggregates));
    }
    json!([{"data": data, "child-nodes": []}])
}

fn node(key: &str) -> CompiledNode {
    CompiledNode {
        key: key.to_owned(),
        etype: key.to_owned(),
        level: 0,
        options: Default::default(),
        selector_clauses: Vec::new(),
        missing: false,
        page: None,
        aggregate: None,
        fetch_attr_ids: Vec::new(),
        relationship: None,
        children: Vec::new(),
    }
}

fn triple(value: DatalogValue, created_at: i64) -> DatalogTriple {
    DatalogTriple {
        entity_id: Uuid::from_u128(17),
        attr_id: Uuid::from_u128(29),
        value,
        created_at,
    }
}

fn values() -> Vec<DatalogValue> {
    let mut values = vec![DatalogValue::Uuid(Uuid::from_u128(7))];
    values.extend(
        [
            json!(null),
            json!(false),
            json!(true),
            json!(0),
            json!(-1),
            json!(1.0),
            json!("quote\" slash\\ newline\n unicode 🦀"),
            json!(Uuid::from_u128(7).to_string()),
            json!([]),
            json!([1, null, {"nested": [false, "value"]}]),
            json!({"b": [3, 2], "a": {"escaped": "\u{0000}"}}),
            serde_json::from_str("18446744073709551616000000000000000001").unwrap(),
        ]
        .into_iter()
        .map(DatalogValue::Json),
    );
    values
}

#[test]
fn borrowed_triple_keys_and_rows_match_the_old_json_bytes() {
    for value in values() {
        for created_at in [i64::MIN, -100, -10, -1, 0, 1, 10, 100, i64::MAX] {
            let triple = triple(value.clone(), created_at);
            let old = json!([
                triple.entity_id,
                triple.attr_id,
                match &triple.value {
                    DatalogValue::Uuid(value) => json!(value),
                    DatalogValue::Json(value) => value.clone(),
                },
                triple.created_at,
            ]);
            assert_eq!(
                serde_json::to_string(&FrameworkTriple(&triple)).unwrap(),
                old.to_string()
            );
            assert_eq!(serde_json::to_value(FrameworkTriple(&triple)).unwrap(), old);
        }
    }
}

#[test]
fn nested_projection_keeps_duplicates_order_cursors_counts_and_fresh_input() {
    let variants = values();
    let rows = (0..200)
        .map(|index| {
            let mut row = triple(
                variants[index % variants.len()].clone(),
                (index % 19) as i64 - 9,
            );
            row.entity_id = Uuid::from_u128((index % 7) as u128);
            row.attr_id = Uuid::from_u128((index % 3) as u128);
            row
        })
        .collect::<Vec<_>>();
    let mut root = node("parent");
    root.children.push(node("children"));
    let mut result = ExecutedNode {
        selector: DatalogResult {
            join_rows: vec![rows.clone(), rows.clone()],
            page_info: Some(PageResultInfo {
                start_cursor: Some(PageCursorResult {
                    entity_id: Uuid::from_u128(13),
                    attr_id: Uuid::from_u128(29),
                    value: Some(DatalogValue::Json(json!([null, 1, "cursor"]))),
                    created_at: i64::MIN,
                }),
                end_cursor: None,
                has_next_page: true,
                has_previous_page: false,
            }),
            ..Default::default()
        },
        aggregate: Some(200),
        entities: vec![ExecutedEntity {
            fetch: DatalogResult {
                join_rows: vec![rows.clone()],
                ..Default::default()
            },
            children: vec![ExecutedNode {
                selector: DatalogResult {
                    join_rows: vec![rows],
                    ..Default::default()
                },
                ..Default::default()
            }],
        }],
    };
    let roots = vec![root, node("empty")];
    let before = result.clone();
    let actual = framework_result(&roots, std::slice::from_ref(&result));
    let expected = legacy(&roots, std::slice::from_ref(&result));
    assert_eq!(actual, expected);
    assert_eq!(actual.to_string(), expected.to_string());
    assert_eq!(
        actual[0]["data"]["page-info"]["parent"],
        json!({
            "start-cursor": [Uuid::from_u128(13), Uuid::from_u128(29), [null, 1, "cursor"], i64::MIN],
            "end-cursor": null,
            "has-next-page?": true,
            "has-previous-page?": false,
        })
    );
    assert_eq!(actual[0]["data"]["aggregate"]["parent"]["count"], 200);
    assert_eq!(result, before);
    result.selector.join_rows[0][0].value = DatalogValue::Json(json!("changed-now"));
    let fresh = framework_result(&roots, std::slice::from_ref(&result));
    assert_ne!(fresh, actual);
    assert_eq!(fresh, legacy(&roots, std::slice::from_ref(&result)));
}

#[test]
fn empty_results_and_uuid_json_aliases_keep_the_original_envelope() {
    assert_eq!(framework_result(&[], &[]), legacy(&[], &[]));
    let root = node("rows");
    let result = ExecutedNode {
        selector: DatalogResult {
            join_rows: vec![vec![
                triple(DatalogValue::Uuid(Uuid::from_u128(7)), 1),
                triple(DatalogValue::Json(json!(Uuid::from_u128(7).to_string())), 1),
            ]],
            ..Default::default()
        },
        aggregate: Some(0),
        ..Default::default()
    };
    let actual = framework_result(std::slice::from_ref(&root), std::slice::from_ref(&result));
    assert_eq!(
        actual[0]["data"]["datalog-result"]["join-rows"][0]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(actual, legacy(&[root], &[result]));
}

#[test]
#[ignore = "manual allocation/CPU characterization, not a timing or capacity gate"]
fn framework_projection_benchmark() {
    let roots = vec![node("todos"), node("messages")];
    let results = (0..2)
        .map(|root| ExecutedNode {
            entities: (0..if root == 0 { 10 } else { 20 })
                .map(|entity| ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![
                            (0..6)
                                .map(|field| DatalogTriple {
                                    entity_id: Uuid::from_u128(root * 100 + entity),
                                    attr_id: Uuid::from_u128(field),
                                    value: DatalogValue::Json(json!(format!(
                                        "synthetic-{entity}-{field}"
                                    ))),
                                    created_at: 17,
                                })
                                .collect(),
                        ],
                        ..Default::default()
                    },
                    children: Vec::new(),
                })
                .collect(),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    assert_eq!(framework_result(&roots, &results), legacy(&roots, &results));
    for (name, format) in [
        (
            "legacy",
            legacy as fn(&[CompiledNode], &[ExecutedNode]) -> Value,
        ),
        ("borrowed", framework_result),
    ] {
        let started = Instant::now();
        for _ in 0..2_000 {
            black_box(format(black_box(&roots), black_box(&results)));
        }
        println!(
            "framework-projection {name}: 2000 iterations, {} ns/projection",
            started.elapsed().as_nanos() / 2_000
        );
    }
}
