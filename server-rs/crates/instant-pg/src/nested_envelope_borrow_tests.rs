use super::*;
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, PlannedNestedGroup,
    compile_query, plan_nested_query,
};
use serde_json::json;

#[test]
fn merged_tables_borrow_original_values_and_keep_last_chunk_precedence() {
    let input = json!([
        {"rows": [{"value": "superseded"}], "count": 2},
        {"other": [{"value": "retained"}]},
        {"rows": [{"value": "latest", "nested": [1, null, true]}]}
    ]);
    let before = input.clone();
    let borrowed = merge_nested_envelope(&input).unwrap();
    let mut legacy = serde_json::Map::new();
    for chunk in input.as_array().unwrap() {
        legacy.extend(chunk.as_object().unwrap().clone());
    }
    assert_eq!(borrowed.len(), legacy.len());
    for (key, table) in &borrowed {
        let NestedTable::Value(value) = table else {
            panic!("value envelopes must borrow the original table values");
        };
        assert_eq!(**value, legacy[key.as_ref()]);
    }
    assert!(
        matches!(borrowed["rows"], NestedTable::Value(value) if std::ptr::eq(value, &input[2]["rows"]))
    );
    assert!(
        matches!(borrowed["other"], NestedTable::Value(value) if std::ptr::eq(value, &input[1]["other"]))
    );
    assert_eq!(input, before);
}

#[test]
fn all_chunks_are_validated_before_any_nested_decode() {
    for input in [json!(null), json!({}), json!([17]), json!([{}, null])] {
        assert!(matches!(
            merge_nested_envelope(&input),
            Err(PgQueryError::InvalidNestedEnvelope)
        ));
    }
    assert!(merge_nested_envelope(&json!([])).unwrap().is_empty());
}

#[test]
fn borrowed_page_flags_have_identical_values_and_errors() {
    for value in [
        json!([{"exists": true}]),
        json!([{"exists": false}]),
        json!([]),
        json!(null),
        json!([{"exists": null}]),
        json!([{"exists": "true"}]),
        json!([17]),
    ] {
        let input = json!([{"page": value}]);
        let borrowed = merge_nested_envelope(&input).unwrap();
        let legacy = input[0].as_object().unwrap();
        for table in ["page", "missing"] {
            let old = batch_page_flag(legacy, table).map_err(|error| error.to_string());
            let new = nested_page_flag(&borrowed, table).map_err(|error| error.to_string());
            assert_eq!(old, new);
        }
    }
}

fn grouped_fixture() -> (PlannedNestedQuery, Value) {
    use instant_instaql::{
        Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query,
    };
    let attribute = Attribute {
        id: Uuid::from_u128(90),
        wire: None,
        etype: "users".into(),
        label: "id".into(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: true,
        setting_unique: false,
        indexed: true,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    };
    let catalog = AttributeCatalog {
        attributes: vec![attribute],
    };
    let compiled = compile_query(&json!({"users": {}}), &catalog, false).unwrap();
    let plan =
        plan_nested_query(&compiled, Uuid::from_u128(99), &MatchCteOptions::default()).unwrap();
    let selector = &plan.groups[0];
    let fetch = &selector.children.as_ref().unwrap().groups[0];
    let mut envelope = serde_json::Map::new();
    for group in [selector, fetch] {
        let rows = (1..=3)
            .map(|id| {
                let mut row = serde_json::Map::new();
                for pattern in flattened_metadata(&group.metadata) {
                    let values = [
                        json!(Uuid::from_u128(id)),
                        json!(Uuid::from_u128(90)),
                        json!(Uuid::from_u128(id)),
                        json!(false),
                        json!(id as i64),
                    ];
                    row.extend(pattern.cte_columns.iter().cloned().zip(values));
                }
                Value::Object(row)
            })
            .collect();
        envelope.insert(group.table.clone().unwrap(), Value::Array(rows));
    }
    (plan, Value::Array(vec![Value::Object(envelope)]))
}

#[test]
fn parent_rows_are_prepared_once_per_exact_group_and_never_reused_across_responses() {
    let (plan, mut input) = grouped_fixture();
    let envelope = merge_nested_envelope(&input).unwrap();
    let mut indexed = HashMap::new();
    let decoded = decode_nested_groups(&plan.groups, &envelope, None, &mut indexed).unwrap();
    assert_eq!(
        indexed.len(),
        1,
        "three parents must share one row partition, not three scans"
    );
    assert_eq!(decoded[0].children.len(), 3);
    assert!(
        decoded[0]
            .children
            .iter()
            .all(|groups| groups[0].result.join_rows.len() == 1)
    );
    let before = decode_planned_instaql_envelope(&plan, &input).unwrap();
    drop(indexed);
    drop(envelope);
    let fetch = &plan.groups[0].children.as_ref().unwrap().groups[0];
    let created = &flattened_metadata(&fetch.metadata)[0].cte_columns[4];
    input[0][fetch.table.as_ref().unwrap()][0][created] = json!(999);
    let after = decode_planned_instaql_envelope(&plan, &input).unwrap();
    assert_ne!(before, after);
    assert_eq!(after[0].entities[0].fetch.join_rows[0][0].created_at, 999);
}

#[test]
fn the_same_table_with_different_group_metadata_cannot_share_a_partition() {
    let (mut plan, input) = grouped_fixture();
    let mut alternate = plan.groups[0].clone();
    let children = alternate.children.as_mut().unwrap();
    let symbol = children.join_symbol.clone();
    for node in &mut children.groups[0].metadata {
        let PatternMetadataNode::Pattern(pattern) = node else {
            unreachable!()
        };
        pattern
            .symbol_fields
            .retain(|_, field| field.symbol != symbol);
    }
    plan.groups.push(alternate);
    let envelope = merge_nested_envelope(&input).unwrap();
    let mut indexed = HashMap::new();
    let decoded = decode_nested_groups(&plan.groups, &envelope, None, &mut indexed).unwrap();
    assert_eq!(indexed.len(), 2);
    assert!(
        decoded[0]
            .children
            .iter()
            .all(|groups| !groups[0].result.join_rows.is_empty())
    );
    assert!(
        decoded[1]
            .children
            .iter()
            .all(|groups| groups[0].result.join_rows.is_empty())
    );
}

#[test]
#[ignore = "manual envelope-copy benchmark; not end-to-end capacity acceptance"]
fn nested_envelope_merge_benchmark() {
    use std::hint::black_box;
    use std::time::Instant;

    let rows = Value::Array(
        (0..100)
            .map(|n| {
                Value::Object(
                    (0..10)
                        .map(|field| (format!("field-{field}"), json!(format!("row-{n}-payload"))))
                        .collect(),
                )
            })
            .collect(),
    );
    let input = json!([{"rows": rows}]);
    for borrowed in [false, true] {
        let started = Instant::now();
        for _ in 0..2000 {
            if borrowed {
                black_box(merge_nested_envelope(black_box(&input)).unwrap());
            } else {
                let mut merged = serde_json::Map::new();
                for chunk in black_box(&input).as_array().unwrap() {
                    merged.extend(chunk.as_object().unwrap().clone());
                }
                black_box(merged);
            }
        }
        println!(
            "nested-envelope borrowed={borrowed} iterations=2000 rows=100 fields=10 elapsed_us={}",
            started.elapsed().as_micros()
        );
    }
}

fn catalog() -> AttributeCatalog {
    let mut catalog: AttributeCatalog = serde_json::from_value(
        serde_json::from_str::<Value>(include_str!(
            "../../instant-instaql/tests/fixtures/where_compilation.json"
        ))
        .unwrap()["catalog"]
            .clone(),
    )
    .unwrap();
    // The shared fixture catalog covers `users` only; add a two-root `teams`
    // entity for the multi-root and count fixtures.
    for (index, label) in ["id", "name"].into_iter().enumerate() {
        catalog.attributes.push(Attribute {
            id: Uuid::from_u128(0x7e00 + index as u128),
            wire: None,
            etype: "teams".into(),
            label: label.into(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed: true,
            required: label == "id",
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        });
    }
    catalog
}

fn plan_for(query: &Value, options: &MatchCteOptions) -> PlannedNestedQuery {
    let compiled = compile_query(query, &catalog(), false).unwrap();
    plan_nested_query(&compiled, Uuid::from_u128(99), options).unwrap()
}

fn admin_plan_for(query: &Value, options: &MatchCteOptions) -> PlannedNestedQuery {
    let compiled = compile_query(query, &catalog(), true).unwrap();
    plan_nested_query(&compiled, Uuid::from_u128(99), options).unwrap()
}

/// Asserts both decoders agree on one envelope document, including error variants.
fn assert_text_envelope_parity(plan: &PlannedNestedQuery, text: &str) {
    let Ok(envelope) = serde_json::from_str::<Value>(text) else {
        assert!(
            matches!(
                decode_planned_instaql_envelope_text(plan, text),
                Err(PgQueryError::InvalidNestedEnvelope)
            ),
            "{text}"
        );
        return;
    };
    assert_eq!(
        decode_planned_instaql_envelope_text(plan, text).map_err(|error| format!("{error:?}")),
        decode_planned_instaql_envelope(plan, &envelope).map_err(|error| format!("{error:?}")),
        "{text}"
    );
}

fn assert_envelope_parity(plan: &PlannedNestedQuery, envelope: &Value) {
    assert_text_envelope_parity(plan, &serde_json::to_string(envelope).unwrap());
}

#[test]
fn raw_text_envelopes_match_value_envelopes_for_groups_chunks_and_malformed_rows() {
    let (plan, envelope) = grouped_fixture();
    assert_envelope_parity(&plan, &envelope);
    assert_eq!(
        decode_planned_instaql_envelope_text(&plan, &serde_json::to_string(&envelope).unwrap())
            .unwrap()[0]
            .entities
            .len(),
        3,
        "the fixture must exercise real entities, not just empty tables"
    );
    for text in ["[]", "[{}]", "null", "{}", "[1]", "[{}, 2]", ""] {
        assert_text_envelope_parity(&plan, text);
    }

    // Escape the table keys: the decoder must match unescaped names exactly.
    let table_name = plan.groups[0].table.clone().unwrap();
    let first = table_name.chars().next().unwrap();
    let escaped_name = format!("\\u{:04x}{}", first as u32, &table_name[first.len_utf8()..]);
    let escaped = serde_json::to_string(&envelope)
        .unwrap()
        .replace(&format!("\"{table_name}\""), &format!("\"{escaped_name}\""));
    assert_ne!(escaped, serde_json::to_string(&envelope).unwrap());
    assert_text_envelope_parity(&plan, &escaped);

    let table = plan.groups[0].table.clone().unwrap();
    let fetch = &plan.groups[0].children.as_ref().unwrap().groups[0];
    let fetch_table = fetch.table.clone().unwrap();
    assert_envelope_parity(
        &plan,
        &json!([{table.clone(): [], fetch_table.clone(): []}]),
    );
    // Chunked envelopes keep last-chunk-wins precedence for repeated tables.
    let rows = envelope[0][&table].clone();
    assert_envelope_parity(
        &plan,
        &json!([{table.clone(): []}, {table.clone(): rows.clone()}]),
    );
    assert_envelope_parity(&plan, &json!([{table.clone(): rows}, {table.clone(): []}]));
    assert_text_envelope_parity(
        &plan,
        &format!(
            "[{{\"{table}\":[],\"{table}\":{}}}]",
            serde_json::to_string(&envelope[0][&table]).unwrap()
        ),
    );

    // Escaped strings, unicode, deep nesting, big integers, and JSON nulls keep
    // exactly the same triple payloads and errors.
    let pattern = flattened_metadata(&plan.groups[0].metadata)[0];
    let value_column = pattern.cte_columns[2].clone();
    let created_column = pattern.cte_columns[4].clone();
    let entity_column = pattern.cte_columns[0].clone();
    for replacement in [
        json!({"escaped": "é\n\u{0}\"", "nested": [[[{"deep": [1, null, true]}]]]}),
        json!(184467440737095516170_u128),
        json!(-0.0),
        json!("unexpected \u{1f600} payload"),
        Value::Null,
    ] {
        let mut changed = envelope.clone();
        changed[0][&table][0][&value_column] = replacement;
        assert_envelope_parity(&plan, &changed);
    }
    for (column, replacement) in [
        (created_column, json!("not-an-integer")),
        (entity_column.clone(), json!(true)),
    ] {
        let mut changed = envelope.clone();
        changed[0][&table][0][&column] = replacement;
        assert_envelope_parity(&plan, &changed);
    }
    let mut missing = envelope.clone();
    missing[0][&table][0]
        .as_object_mut()
        .unwrap()
        .remove(&value_column);
    assert_envelope_parity(&plan, &missing);
    let mut absent = envelope.clone();
    absent[0][&table][0][&entity_column] = Value::Null;
    absent[0][&table][0][&value_column] = Value::Null;
    assert_envelope_parity(&plan, &absent);
}

#[test]
fn raw_text_envelopes_match_value_envelopes_for_pages_and_counts() {
    let plan = plan_for(
        &json!({"users": {"$": {"fields": ["handle"], "order": {"handle": "desc"}}}}),
        &MatchCteOptions::default(),
    );
    let root = &plan.groups[0];
    let page = root.page.clone().expect("root order selects a page plan");
    // The paged CTE projects both the selector triple and its ordering triple.
    let mut patterns: Vec<&PatternMetadata> = flattened_metadata(&root.metadata);
    for fetch in &root.children.as_ref().unwrap().groups {
        patterns.extend(flattened_metadata(&fetch.metadata));
    }
    let mut rows = Vec::new();
    for index in 0..3 {
        let mut row = serde_json::Map::new();
        for pattern in &patterns {
            let values = [
                json!(Uuid::from_u128(1 + index)),
                json!(page.attr_id),
                json!(format!("handle-{index}")),
                json!(false),
                json!(index as i64),
            ];
            row.extend(pattern.cte_columns.iter().cloned().zip(values));
        }
        row.insert(
            page.fallback_entity_column.clone(),
            json!(Uuid::from_u128(1 + index)),
        );
        row.insert(page.fallback_created_at_column.clone(), json!(index as i64));
        rows.push(Value::Object(row));
    }
    let fetch_table = root.children.as_ref().unwrap().groups[0]
        .table
        .clone()
        .unwrap();
    let root_table = root.table.clone().unwrap();
    let envelope = json!([{
        root_table.clone(): rows,
        fetch_table.clone(): [],
        page.has_next_table.clone(): [{"exists": true}],
        page.has_previous_table.clone(): [{"exists": false}],
    }]);
    assert_envelope_parity(&plan, &envelope);
    assert_eq!(
        decode_planned_instaql_envelope_text(&plan, &serde_json::to_string(&envelope).unwrap())
            .unwrap()[0]
            .entities
            .len(),
        3,
        "the fixture must exercise page cursors, not just empty tables"
    );
    assert_envelope_parity(
        &plan,
        &json!([{root_table.clone(): [], fetch_table.clone(): []}]),
    );
    for flag in [
        json!([]),
        json!([{}]),
        json!([{"exists": null}]),
        json!([{"exists": "true"}]),
        json!([{"exists": true}, {"exists": false}]),
        json!({"exists": true}),
        json!(17),
    ] {
        let changed = json!([{
            root_table.clone(): envelope[0][&root_table].clone(),
            fetch_table.clone(): [],
            page.has_next_table.clone(): flag.clone(),
            page.has_previous_table.clone(): [{"exists": false}],
        }]);
        assert_envelope_parity(&plan, &changed);
        let changed = json!([{
            root_table.clone(): envelope[0][&root_table].clone(),
            fetch_table.clone(): [],
            page.has_next_table.clone(): [{"exists": true}],
            page.has_previous_table.clone(): flag,
        }]);
        assert_envelope_parity(&plan, &changed);
    }
    // Missing fallback columns take the eager cursor path with identical errors.
    let mut without_fallbacks = envelope.clone();
    for row in without_fallbacks[0][&root_table].as_array_mut().unwrap() {
        row.as_object_mut()
            .unwrap()
            .remove(&page.fallback_entity_column);
    }
    assert_envelope_parity(&plan, &without_fallbacks);

    let count_plan = admin_plan_for(
        &json!({"teams": {"$": {"aggregate": "count"}}}),
        &MatchCteOptions::default(),
    );
    assert!(count_plan.groups[0].aggregate.is_some());
    let count_table = count_plan.groups[0].table.clone().unwrap();
    for text in [
        format!("[{{\"{count_table}\":5}}]"),
        format!("[{{\"{count_table}\":\"5\"}}]"),
        format!("[{{\"{count_table}\":5.0}}]"),
        format!("[{{\"{count_table}\":null}}]"),
        format!("[{{\"{count_table}\":{{}}}}]"),
        "[{}]".to_owned(),
        "[]".to_owned(),
    ] {
        assert_text_envelope_parity(&count_plan, &text);
    }

    let missing_plan = plan_for(&json!({"zippy": {}}), &MatchCteOptions::default());
    assert_text_envelope_parity(&missing_plan, "[{}]");
}

#[test]
fn raw_json_cells_accept_both_postgres_json_formats() {
    assert_eq!(
        <RawJsonText as FromSql>::from_sql(&Type::JSON, br#"[{"exists":true}]"#)
            .unwrap()
            .text(),
        "[{\"exists\":true}]"
    );
    assert_eq!(
        <RawJsonText as FromSql>::from_sql(&Type::JSONB, b"\x01[1, null]")
            .unwrap()
            .text(),
        "[1, null]"
    );
    assert!(<RawJsonText as FromSql>::from_sql(&Type::JSONB, b"\x02[]").is_err());
    assert!(<RawJsonText as FromSql>::from_sql(&Type::JSONB, b"").is_err());
    assert!(<RawJsonText as FromSql>::from_sql(&Type::TEXT, b"[]").is_err());
    assert_eq!(
        <RawJsonText as FromSql>::from_sql(&Type::JSON, b"\xff")
            .unwrap_err()
            .to_string(),
        "invalid utf-8 sequence of 1 bytes from index 0"
    );
}

fn flat_group(group: &PlannedNestedGroup, rows: u128) -> Value {
    Value::Array(
        (1..=rows)
            .map(|id| {
                let mut row = serde_json::Map::new();
                for pattern in flattened_metadata(&group.metadata) {
                    let values = [
                        json!(Uuid::from_u128(id)),
                        json!(Uuid::from_u128(90 + id)),
                        json!(format!("row-{id}")),
                        json!(false),
                        json!(id as i64),
                    ];
                    row.extend(pattern.cte_columns.iter().cloned().zip(values));
                }
                Value::Object(row)
            })
            .collect(),
    )
}

fn benchmark_fixture() -> (PlannedNestedQuery, String) {
    let plan = plan_for(
        &json!({
            "users": {"$": {"fields": ["handle"], "where": {"or": [
                {"handle": "alex"}, {"age": 18}, {"nickname": "al"}
            ]}}},
            "teams": {"$": {"fields": ["name"]}}
        }),
        &MatchCteOptions::default(),
    );
    assert_eq!(plan.groups.len(), 2);
    let mut envelope = serde_json::Map::new();
    for group in &plan.groups {
        envelope.insert(group.table.clone().unwrap(), flat_group(group, 15));
        for fetch in &group.children.as_ref().unwrap().groups {
            envelope.insert(fetch.table.clone().unwrap(), flat_group(fetch, 15));
        }
    }
    (plan, serde_json::to_string(&json!([envelope])).unwrap())
}

#[test]
#[ignore = "manual raw-envelope decode benchmark; run explicitly in release mode"]
fn raw_nested_envelope_decode_benchmark() {
    use std::hint::black_box;
    use std::time::Instant;
    let (plan, text) = benchmark_fixture();
    let envelope: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        decode_planned_instaql_envelope_text(&plan, &text).unwrap(),
        decode_planned_instaql_envelope(&plan, &envelope).unwrap()
    );
    let iterations = 2000;
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(decode_planned_instaql_envelope(black_box(&plan), black_box(&envelope)).unwrap());
    }
    let value_decode = start.elapsed();
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(serde_json::from_str::<Value>(black_box(&text)).unwrap());
    }
    let parse_only = start.elapsed();
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(
            decode_planned_instaql_envelope_text(black_box(&plan), black_box(&text)).unwrap(),
        );
    }
    let raw_total = start.elapsed();
    println!(
        "nested-envelope-decode: iterations={iterations} roots=2 root_rows=30 total_rows=60 \
         value_parse_plus_decode_us={} value_decode_only_us={} value_parse_only_us={} \
         raw_total_us={}",
        (parse_only + value_decode).as_micros(),
        value_decode.as_micros(),
        parse_only.as_micros(),
        raw_total.as_micros()
    );
}
