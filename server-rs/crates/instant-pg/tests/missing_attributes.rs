use instant_datalog::{IndexKey, MatchCteOptions, PatternMetadataNode};
use instant_instaql::{
    AttributeCatalog, PlannedNestedGroup, compile_query, plan_nested_query, shape_response,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

mod support;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!(
        "../../instant-instaql/tests/fixtures/where_compilation.json"
    ))
    .unwrap()
    .catalog
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

#[test]
fn missing_roots_and_where_attributes_decode_without_postgres() {
    let catalog = catalog();
    let compiled = compile_query(
        &json!({
            "zippy": {},
            "users": {"$": {"where": {"missing": "alex"}}}
        }),
        &catalog,
        false,
    )
    .unwrap();
    assert!(compiled.roots.iter().all(|root| root.missing));
    let plan = plan_nested_query(
        &compiled,
        uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        &MatchCteOptions::default(),
    )
    .unwrap();
    assert!(plan.ctes.is_empty());
    assert!(plan.sql_plan().result_tables.is_empty());

    let results = support::decode_envelope(&plan, &json!([{}])).unwrap();
    assert_eq!(results.len(), 2);
    for result in &results {
        assert!(result.entities.is_empty());
        assert_eq!(result.selector.topics.len(), 2);
        assert_eq!(result.selector.topics[0].indexes, vec![IndexKey::Ea]);
        assert_eq!(result.selector.topics[1].indexes, vec![IndexKey::Eav]);
        assert!(result.selector.symbol_values.values().all(Vec::is_empty));
    }
    let response = shape_response(&compiled, &results, &catalog, true).unwrap();
    assert_eq!(response.data["zippy"], json!([]));
    assert_eq!(response.data["users"], json!([]));
}

#[test]
fn missing_child_and_root_do_not_suppress_a_valid_parent() {
    let catalog = catalog();
    let compiled = compile_query(
        &json!({"users": {"zippy": {}}, "zippy": {}}),
        &catalog,
        false,
    )
    .unwrap();
    let plan = plan_nested_query(
        &compiled,
        uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        &MatchCteOptions::default(),
    )
    .unwrap();
    let user_index = compiled
        .roots
        .iter()
        .position(|root| root.key == "users")
        .unwrap();
    let user_group = &plan.groups[user_index];
    let user_fetch = only_group(user_group);
    let missing_child = only_group(user_fetch);
    assert!(missing_child.table.is_none());
    assert!(missing_child.missing_clauses.is_some());
    assert_eq!(plan.sql_plan().result_tables.len(), 2);

    let user_id = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let user_id_attr = catalog.forward("users", "id").unwrap().id;
    let mut envelope = Map::new();
    envelope.insert(
        user_group.table.clone().unwrap(),
        Value::Array(vec![row(
            user_group,
            user_id,
            user_id_attr,
            json!(user_id),
            10,
        )]),
    );
    envelope.insert(
        user_fetch.table.clone().unwrap(),
        Value::Array(vec![row(
            user_fetch,
            user_id,
            user_id_attr,
            json!(user_id),
            10,
        )]),
    );
    let results =
        support::decode_envelope(&plan, &Value::Array(vec![Value::Object(envelope)])).unwrap();
    assert_eq!(results[user_index].entities.len(), 1);
    let missing_result = &results[user_index].entities[0].children[0];
    assert!(missing_result.entities.is_empty());
    assert_eq!(missing_result.selector.topics.len(), 2);

    let response = shape_response(&compiled, &results, &catalog, true).unwrap();
    assert_eq!(response.data["users"][0]["id"], user_id.to_string());
    assert_eq!(response.data["users"][0]["zippy"], json!([]));
    assert_eq!(response.data["zippy"], json!([]));
}

#[test]
fn missing_count_query_does_not_fabricate_an_aggregate() {
    let catalog = catalog();
    let compiled = compile_query(
        &json!({"zippy": {"$": {"aggregate": "count"}}}),
        &catalog,
        true,
    )
    .unwrap();
    let plan = plan_nested_query(
        &compiled,
        uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        &MatchCteOptions::default(),
    )
    .unwrap();
    let results = support::decode_envelope(&plan, &json!([{}])).unwrap();
    assert_eq!(results[0].aggregate, None);
    let response = shape_response(&compiled, &results, &catalog, true).unwrap();
    assert_eq!(response.data["zippy"], json!([]));
    assert!(!response.aggregates.contains_key("zippy"));
}

fn only_group(group: &PlannedNestedGroup) -> &PlannedNestedGroup {
    let children = group.children.as_ref().unwrap();
    assert_eq!(children.groups.len(), 1);
    &children.groups[0]
}

fn row(
    group: &PlannedNestedGroup,
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
    created_at: i64,
) -> Value {
    let PatternMetadataNode::Pattern(metadata) = &group.metadata[0] else {
        panic!("fixture group should use one direct pattern");
    };
    let [
        entity,
        attribute,
        value_column,
        reference_column,
        created_column,
    ] = metadata.cte_columns.as_slice()
    else {
        panic!("pattern metadata should expose five columns");
    };
    Value::Object(Map::from_iter([
        (entity.clone(), json!(entity_id)),
        (attribute.clone(), json!(attr_id)),
        (value_column.clone(), value),
        (reference_column.clone(), json!(false)),
        (created_column.clone(), json!(created_at)),
    ]))
}
