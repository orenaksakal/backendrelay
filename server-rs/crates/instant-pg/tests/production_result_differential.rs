use std::collections::HashMap;

use instant_datalog::{
    BatchResultKind, Clause, DatalogValue, MatchCteOptions, MatchQueryContext, MatchQueryPlan,
    NormalizedClause, PagePlanMetadata, PatternMetadata, PatternMetadataNode, QueryCell, QueryRow,
    decode_paged_query_rows, decode_query_rows, plan_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, PlannedNestedGroup,
    compile_query, plan_nested_query, shape_response,
};
use instant_pg::{
    MixedDatalogBatchResult, decode_datalog_batch_envelope, decode_mixed_datalog_batch_envelope,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

mod support;

#[derive(Clone, Copy, Deserialize)]
struct Fixture {
    entity_count: usize,
    team_count: usize,
    page_size: usize,
    missing_handle_modulo: usize,
    null_handle_modulo: usize,
    duplicate_row_modulo: usize,
}

const APP_ID: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa);
const ID_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_1111_1111_111111111111);
const HANDLE_ATTR: Uuid = Uuid::from_u128(0x22222222_2222_2222_2222_222222222222);
const TEAM_ATTR: Uuid = Uuid::from_u128(0x33333333_3333_3333_3333_333333333333);
const TEAM_ID_ATTR: Uuid = Uuid::from_u128(0x44444444_4444_4444_4444_444444444444);
const TEAM_NAME_ATTR: Uuid = Uuid::from_u128(0x55555555_5555_5555_5555_555555555555);

#[test]
fn production_rows_match_direct_batch_paged_and_count_decoders() {
    let fixture = fixture();
    let plan = production_plan("production-");
    let (direct_rows, json_rows) = production_rows(&plan, fixture, fixture.entity_count);

    let direct = decode_query_rows(&direct_rows, &plan.pattern_metadata).unwrap();
    assert_eq!(direct.join_rows.len(), fixture.entity_count);
    assert_eq!(
        direct.join_rows.iter().map(Vec::len).sum::<usize>(),
        fixture.entity_count * 3 - missing_count(fixture, fixture.entity_count)
    );
    assert_eq!(
        direct.symbol_values[&instant_datalog::Symbol::new("?entity").unwrap()].len(),
        fixture.entity_count
    );
    assert!(direct.join_rows.iter().any(|row| {
        row.iter()
            .any(|triple| triple.value == DatalogValue::Json(Value::Null))
    }));
    assert!(direct.join_rows.iter().any(|row| {
        row.iter()
            .any(|triple| matches!(triple.value, DatalogValue::Uuid(_)))
    }));

    let batch_envelope = json!({plan.final_table.clone(): json_rows.clone()});
    let batch = decode_datalog_batch_envelope(&batch_envelope, std::slice::from_ref(&plan))
        .unwrap()
        .remove(0);
    assert_eq!(direct, batch);

    let count_plan = production_plan("count-");
    let mixed_envelope = json!({
        plan.final_table.clone(): json_rows,
        count_plan.final_table.clone(): fixture.entity_count
    });
    let mixed = decode_mixed_datalog_batch_envelope(
        &mixed_envelope,
        &[plan.clone(), count_plan],
        &[BatchResultKind::Rows, BatchResultKind::Count],
    )
    .unwrap();
    assert_eq!(mixed[0], MixedDatalogBatchResult::Rows(Box::new(direct)));
    assert_eq!(
        mixed[1],
        MixedDatalogBatchResult::Count(i64::try_from(fixture.entity_count).unwrap())
    );

    assert_paged_envelopes_match(fixture);
}

#[test]
#[allow(clippy::too_many_lines)] // One corpus constructs and verifies every nested transport layer.
fn production_nested_envelope_shapes_all_parents_children_nulls_and_count() {
    let fixture = fixture();
    let catalog = catalog();
    let compiled = compile_query(&json!({"users": {"team": {}}}), &catalog, false).unwrap();
    let plan = plan_nested_query(&compiled, APP_ID, &MatchCteOptions::default()).unwrap();
    let root = &plan.groups[0];
    let root_fetch = only_group(root);
    let child_selector = only_group(root_fetch);
    let child_fetch = only_group(child_selector);
    let mut envelope = Map::new();
    envelope.insert(
        root.table.clone().unwrap(),
        Value::Array(
            (0..fixture.entity_count)
                .map(|index| {
                    nested_row(
                        root,
                        entity_id(index),
                        ID_ATTR,
                        json!(entity_id(index)),
                        false,
                        index,
                    )
                })
                .collect(),
        ),
    );
    let mut fetch_rows = Vec::new();
    for index in 0..fixture.entity_count {
        fetch_rows.push(nested_row(
            root_fetch,
            entity_id(index),
            ID_ATTR,
            json!(entity_id(index)),
            false,
            index,
        ));
        if index % fixture.missing_handle_modulo != 0 {
            fetch_rows.push(nested_row(
                root_fetch,
                entity_id(index),
                HANDLE_ATTR,
                handle_value(index, fixture),
                false,
                10_000 + index,
            ));
        }
    }
    envelope.insert(root_fetch.table.clone().unwrap(), Value::Array(fetch_rows));
    envelope.insert(
        child_selector.table.clone().unwrap(),
        Value::Array(
            (0..fixture.entity_count)
                .map(|index| {
                    nested_row(
                        child_selector,
                        entity_id(index),
                        TEAM_ATTR,
                        json!(team_id(index % fixture.team_count)),
                        true,
                        20_000 + index,
                    )
                })
                .collect(),
        ),
    );
    let mut team_rows = Vec::new();
    for index in 0..fixture.team_count {
        team_rows.push(nested_row(
            child_fetch,
            team_id(index),
            TEAM_ID_ATTR,
            json!(team_id(index)),
            false,
            30_000 + index,
        ));
        team_rows.push(nested_row(
            child_fetch,
            team_id(index),
            TEAM_NAME_ATTR,
            json!(format!("team-{index:02}")),
            false,
            40_000 + index,
        ));
    }
    envelope.insert(child_fetch.table.clone().unwrap(), Value::Array(team_rows));

    let chunks = chunk_envelope(envelope, 2);
    let executed = support::decode_envelope(&plan, &chunks).unwrap();
    let response = shape_response(&compiled, &executed, &catalog, true).unwrap();
    let users = response.data["users"].as_array().unwrap();
    assert_eq!(users.len(), fixture.entity_count);
    for index in 0..fixture.entity_count {
        let user = users
            .iter()
            .find(|user| user["id"] == entity_id(index).to_string())
            .unwrap();
        if index.is_multiple_of(fixture.missing_handle_modulo) {
            assert!(user.get("handle").is_none());
        } else {
            assert_eq!(user["handle"], handle_value(index, fixture));
        }
        assert_eq!(
            user["team"]["id"],
            team_id(index % fixture.team_count).to_string()
        );
        assert_eq!(
            user["team"]["name"],
            format!("team-{:02}", index % fixture.team_count)
        );
    }

    let aggregate = compile_query(
        &json!({"teams": {"$": {"aggregate": "count"}}}),
        &catalog,
        true,
    )
    .unwrap();
    let aggregate_plan =
        plan_nested_query(&aggregate, APP_ID, &MatchCteOptions::default()).unwrap();
    let table = aggregate_plan.groups[0].table.clone().unwrap();
    let aggregate_executed =
        support::decode_envelope(&aggregate_plan, &json!([{table: fixture.team_count}])).unwrap();
    assert_eq!(
        aggregate_executed[0].aggregate,
        Some(i64::try_from(fixture.team_count).unwrap())
    );
}

fn assert_paged_envelopes_match(fixture: Fixture) {
    let mut plan = production_plan("page-");
    let metadata = pattern_metadata(&plan.pattern_metadata[1]);
    let page = PagePlanMetadata {
        table: plan.final_table.clone(),
        has_next_table: "page-has-next".to_owned(),
        has_previous_table: "page-has-prev".to_owned(),
        cursor_columns: metadata.cte_columns.clone(),
        fallback_entity_column: "order_eid".to_owned(),
        fallback_created_at_column: "fallback_created_at".to_owned(),
        attr_id: HANDLE_ATTR,
        reverse_results: true,
        empty_page_has_previous: true,
    };
    plan.page = Some(page.clone());
    // The page CTE uses DISTINCT ON before LIMIT, so its transport rows have
    // already removed duplicate join projections.
    let page_fixture = Fixture {
        duplicate_row_modulo: usize::MAX,
        ..fixture
    };
    let (mut direct_rows, mut json_rows) = production_rows(&plan, page_fixture, fixture.page_size);
    for (index, row) in direct_rows.iter_mut().enumerate() {
        row.cells.insert(
            "order_eid".to_owned(),
            QueryCell::Uuid(Some(entity_id(index))),
        );
        row.cells.insert(
            "fallback_created_at".to_owned(),
            QueryCell::Integer(Some(i64::try_from(50_000 + index).unwrap())),
        );
    }
    for (index, row) in json_rows.iter_mut().enumerate() {
        let row = row.as_object_mut().unwrap();
        row.insert("order_eid".to_owned(), json!(entity_id(index)));
        row.insert("fallback_created_at".to_owned(), json!(50_000 + index));
    }
    let direct =
        decode_paged_query_rows(&direct_rows, &plan.pattern_metadata, &page, true, false).unwrap();
    let envelope = json!({
        plan.final_table.clone(): json_rows,
        page.has_next_table.clone(): [{"exists": true}],
        page.has_previous_table.clone(): [{"exists": false}]
    });
    let batch = decode_datalog_batch_envelope(&envelope, &[plan])
        .unwrap()
        .remove(0);
    assert_eq!(direct, batch);
    let info = batch.page_info.unwrap();
    assert!(info.has_next_page);
    assert!(!info.has_previous_page);
    assert_eq!(
        info.start_cursor.unwrap().entity_id,
        entity_id(fixture.page_size - 1)
    );
    assert_eq!(info.end_cursor.unwrap().entity_id, entity_id(0));
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/production_result_differential.json")).unwrap()
}

fn production_plan(prefix: &str) -> MatchQueryPlan {
    let clauses: Vec<Clause> = serde_json::from_value(json!([
        {"kind":"pattern","pattern":{"index":{"key":"ea"},"components":[{"kind":"variable","value":"?entity"},{"kind":"constant","value":ID_ATTR},{"kind":"variable","value":"?id"}]}},
        {"kind":"pattern","pattern":{"index":{"key":"ea"},"components":[{"kind":"variable","value":"?entity"},{"kind":"constant","value":HANDLE_ATTR},{"kind":"variable","value":"?handle"}]}},
        {"kind":"pattern","pattern":{"index":{"key":"ea"},"components":[{"kind":"variable","value":"?entity"},{"kind":"constant","value":TEAM_ATTR},{"kind":"variable","value":"?team"}]}}
    ])).unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<NormalizedClause>, _>>()
        .unwrap();
    plan_match_query(
        &MatchQueryContext {
            prefix,
            app_id: APP_ID,
            additional_joins: &HashMap::new(),
            initial_symbol_cardinalities: &HashMap::new(),
            options: &MatchCteOptions::default(),
        },
        &clauses,
    )
    .unwrap()
}

fn production_rows(
    plan: &MatchQueryPlan,
    fixture: Fixture,
    count: usize,
) -> (Vec<QueryRow>, Vec<Value>) {
    let metadata = plan
        .pattern_metadata
        .iter()
        .map(pattern_metadata)
        .collect::<Vec<_>>();
    let mut direct = Vec::new();
    let mut json_rows = Vec::new();
    for index in 0..count {
        let entity = entity_id(index);
        let team = team_id(index % fixture.team_count);
        let mut direct_row = QueryRow::default();
        let mut json_row = Map::new();
        add_cells(
            &mut direct_row,
            &mut json_row,
            metadata[0],
            Some(entity),
            Some(ID_ATTR),
            Some(json!(entity)),
            Some(false),
            Some(i64::try_from(index).unwrap()),
        );
        if index.is_multiple_of(fixture.missing_handle_modulo) {
            add_cells(
                &mut direct_row,
                &mut json_row,
                metadata[1],
                None,
                None,
                None,
                None,
                None,
            );
        } else {
            add_cells(
                &mut direct_row,
                &mut json_row,
                metadata[1],
                Some(entity),
                Some(HANDLE_ATTR),
                Some(handle_value(index, fixture)),
                Some(false),
                Some(i64::try_from(10_000 + index).unwrap()),
            );
        }
        add_cells(
            &mut direct_row,
            &mut json_row,
            metadata[2],
            Some(entity),
            Some(TEAM_ATTR),
            Some(json!(team)),
            Some(true),
            Some(i64::try_from(20_000 + index).unwrap()),
        );
        direct.push(direct_row.clone());
        json_rows.push(Value::Object(json_row.clone()));
        if index > 0 && index.is_multiple_of(fixture.duplicate_row_modulo) {
            direct.push(direct_row);
            json_rows.push(Value::Object(json_row));
        }
    }
    (direct, json_rows)
}

#[allow(clippy::too_many_arguments)]
fn add_cells(
    direct: &mut QueryRow,
    json_row: &mut Map<String, Value>,
    metadata: &PatternMetadata,
    entity: Option<Uuid>,
    attr: Option<Uuid>,
    value: Option<Value>,
    reference: Option<bool>,
    created_at: Option<i64>,
) {
    let [
        entity_column,
        attr_column,
        value_column,
        reference_column,
        created_column,
    ] = metadata.cte_columns.as_slice()
    else {
        panic!("pattern metadata must expose five columns");
    };
    direct
        .cells
        .insert(entity_column.clone(), QueryCell::Uuid(entity));
    direct
        .cells
        .insert(attr_column.clone(), QueryCell::Uuid(attr));
    direct
        .cells
        .insert(value_column.clone(), QueryCell::Json(value.clone()));
    direct
        .cells
        .insert(reference_column.clone(), QueryCell::Boolean(reference));
    direct
        .cells
        .insert(created_column.clone(), QueryCell::Integer(created_at));
    json_row.insert(entity_column.clone(), json!(entity));
    json_row.insert(attr_column.clone(), json!(attr));
    json_row.insert(value_column.clone(), value.unwrap_or(Value::Null));
    json_row.insert(reference_column.clone(), json!(reference));
    json_row.insert(created_column.clone(), json!(created_at));
}

fn pattern_metadata(node: &PatternMetadataNode) -> &PatternMetadata {
    let PatternMetadataNode::Pattern(metadata) = node else {
        panic!("fixture uses linear pattern metadata");
    };
    metadata
}

fn missing_count(fixture: Fixture, count: usize) -> usize {
    (0..count)
        .filter(|index| index.is_multiple_of(fixture.missing_handle_modulo))
        .count()
}

fn entity_id(index: usize) -> Uuid {
    Uuid::from_u128(0x10000000_0000_0000_0000_000000000000 + index as u128)
}

fn team_id(index: usize) -> Uuid {
    Uuid::from_u128(0x20000000_0000_0000_0000_000000000000 + index as u128)
}

fn handle_value(index: usize, fixture: Fixture) -> Value {
    if index.is_multiple_of(fixture.null_handle_modulo) {
        Value::Null
    } else {
        json!(format!("user-{index:04}"))
    }
}

fn only_group(group: &PlannedNestedGroup) -> &PlannedNestedGroup {
    let children = group.children.as_ref().unwrap();
    assert_eq!(children.groups.len(), 1);
    &children.groups[0]
}

fn nested_row(
    group: &PlannedNestedGroup,
    entity: Uuid,
    attr: Uuid,
    value: Value,
    reference: bool,
    created_at: usize,
) -> Value {
    let metadata = pattern_metadata(&group.metadata[0]);
    let mut direct = QueryRow::default();
    let mut json_row = Map::new();
    add_cells(
        &mut direct,
        &mut json_row,
        metadata,
        Some(entity),
        Some(attr),
        Some(value),
        Some(reference),
        Some(i64::try_from(created_at).unwrap()),
    );
    Value::Object(json_row)
}

fn chunk_envelope(envelope: Map<String, Value>, chunk_size: usize) -> Value {
    Value::Array(
        envelope
            .into_iter()
            .collect::<Vec<_>>()
            .chunks(chunk_size)
            .map(|chunk| Value::Object(Map::from_iter(chunk.iter().cloned())))
            .collect(),
    )
}

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: vec![
            attribute(ID_ATTR, "users", "id", AttributeValueType::Blob),
            attribute(HANDLE_ATTR, "users", "handle", AttributeValueType::Blob),
            Attribute {
                cardinality: Cardinality::One,
                reverse_etype: Some("teams".to_owned()),
                reverse_label: Some("members".to_owned()),
                ..attribute(TEAM_ATTR, "users", "team", AttributeValueType::Ref)
            },
            attribute(TEAM_ID_ATTR, "teams", "id", AttributeValueType::Blob),
            attribute(TEAM_NAME_ATTR, "teams", "name", AttributeValueType::Blob),
        ],
    }
}

fn attribute(id: Uuid, etype: &str, label: &str, value_type: AttributeValueType) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type,
        cardinality: Cardinality::One,
        unique: label == "id",
        setting_unique: false,
        indexed: label == "id",
        required: label == "id",
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}
