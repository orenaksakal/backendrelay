use instant_datalog::{MatchCteOptions, PatternMetadataNode, Symbol};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, PlannedNestedGroup,
    compile_query, plan_nested_query, shape_response,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

mod support;

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

#[test]
fn chunked_nested_envelope_reconstructs_scoped_entity_tree() {
    let app_id = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let user_id = uuid("11111111-1111-1111-1111-111111111111");
    let shelf_id = uuid("22222222-2222-2222-2222-222222222222");
    let user_id_attr = uuid("aaaaaaaa-1111-1111-1111-111111111111");
    let shelves_attr = uuid("aaaaaaaa-2222-2222-2222-222222222222");
    let shelf_id_attr = uuid("bbbbbbbb-1111-1111-1111-111111111111");
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(user_id_attr, "users", "id", AttributeValueType::Blob),
            Attribute {
                cardinality: Cardinality::Many,
                reverse_etype: Some("shelves".to_owned()),
                reverse_label: Some("owner".to_owned()),
                ..attribute(shelves_attr, "users", "shelves", AttributeValueType::Ref)
            },
            attribute(shelf_id_attr, "shelves", "id", AttributeValueType::Blob),
        ],
    };
    let compiled = compile_query(&json!({"users": {"shelves": {}}}), &catalog, false).unwrap();
    let plan = plan_nested_query(&compiled, app_id, &MatchCteOptions::default()).unwrap();

    let root = &plan.groups[0];
    let root_fetch = only_group(root);
    let child_selector = only_group(root_fetch);
    let child_fetch = only_group(child_selector);
    let mut envelope = Map::new();
    envelope.insert(
        root.table.clone().unwrap(),
        Value::Array(vec![row(
            root,
            user_id,
            user_id_attr,
            json!(user_id),
            false,
            10,
        )]),
    );
    envelope.insert(
        root_fetch.table.clone().unwrap(),
        Value::Array(vec![row(
            root_fetch,
            user_id,
            user_id_attr,
            json!(user_id),
            false,
            10,
        )]),
    );
    envelope.insert(
        child_selector.table.clone().unwrap(),
        Value::Array(vec![row(
            child_selector,
            user_id,
            shelves_attr,
            json!(shelf_id),
            true,
            11,
        )]),
    );
    envelope.insert(
        child_fetch.table.clone().unwrap(),
        Value::Array(vec![row(
            child_fetch,
            shelf_id,
            shelf_id_attr,
            json!(shelf_id),
            false,
            20,
        )]),
    );

    let executed =
        support::decode_envelope(&plan, &Value::Array(vec![Value::Object(envelope)])).unwrap();
    assert_eq!(executed[0].entities.len(), 1);
    assert_eq!(executed[0].entities[0].children[0].entities.len(), 1);
    assert!(
        !executed[0].entities[0]
            .fetch
            .symbol_values
            .contains_key(&Symbol::new("?users-0").unwrap())
    );
    assert!(
        !executed[0].entities[0].children[0].entities[0]
            .fetch
            .symbol_values
            .contains_key(&Symbol::new("?shelves-1").unwrap())
    );
    let response = shape_response(&compiled, &executed, &catalog, true).unwrap();
    assert_eq!(response.data["users"][0]["id"], user_id.to_string());
    assert_eq!(
        response.data["users"][0]["shelves"][0]["id"],
        shelf_id.to_string()
    );
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
    reference: bool,
    created_at: i64,
) -> Value {
    let PatternMetadataNode::Pattern(metadata) = &group.metadata[0] else {
        panic!("fixture groups use one direct pattern");
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
        (reference_column.clone(), json!(reference)),
        (created_column.clone(), json!(created_at)),
    ]))
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
