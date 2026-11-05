use std::collections::{BTreeSet, HashMap};

use instant_datalog::{
    DatalogValue, MatchCteOptions, OrderDirection, PatternMetadataNode, Term, TopicPart,
};
use instant_instaql::{
    AttributeCatalog, ExecutedNode, PlannedNestedGroup, collect_query_invalidation_topics,
    compile_query, plan_nested_query, shape_response,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

mod support;

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    app_id: Uuid,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    root_order: Option<OrderDirection>,
    child_order: OrderDirection,
    users: Vec<User>,
    expected_topic_attributes: Vec<Uuid>,
    expected: Value,
}

#[derive(Deserialize)]
struct User {
    id: Uuid,
    handle: String,
    age: i64,
    created_at: i64,
    shelves: Vec<Shelf>,
}

#[derive(Deserialize)]
struct Shelf {
    id: Uuid,
    name: String,
    order: i64,
    created_at: i64,
}

#[derive(Clone)]
struct TripleSeed {
    entity: Uuid,
    attribute: Uuid,
    value: Value,
    reference: bool,
    created_at: i64,
}

#[test]
fn boolean_relationship_filters_nested_pages_results_and_topics_match_clojure() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "fixtures/nested_page_boolean_topic_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));
    let catalog = catalog();

    for case in fixture.cases {
        let compiled = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let plan = plan_nested_query(
            &compiled,
            fixture.app_id,
            &MatchCteOptions {
                ensure_empty_symbol_values_for_topics: Some(true),
                ..MatchCteOptions::default()
            },
        )
        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_plan_pages(&case, &compiled, &plan.groups[0], &catalog);
        let envelope = build_envelope(&plan.groups[0], &case.users, &catalog);
        let executed =
            support::decode_envelope(&plan, &Value::Array(vec![Value::Object(envelope)]))
                .unwrap_or_else(|error| panic!("{}: {error}", case.name));

        assert_page_metadata(&case, &executed);
        let topics = collect_query_invalidation_topics(&compiled, &executed)
            .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
        let actual_topic_attributes = topics
            .iter()
            .flat_map(|topic| match &topic.attribute {
                TopicPart::Values(values) => values
                    .iter()
                    .filter_map(|value| match value {
                        DatalogValue::Uuid(attribute) => Some(*attribute),
                        DatalogValue::Json(_) => None,
                    })
                    .collect::<Vec<_>>(),
                TopicPart::Any | TopicPart::Not(_) => Vec::new(),
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual_topic_attributes,
            case.expected_topic_attributes.into_iter().collect(),
            "{}: exact selector and generated-fetch topic attributes",
            case.name
        );

        let response = shape_response(&compiled, &executed, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_eq!(
            serde_json::to_value(response.data).unwrap(),
            case.expected,
            "{}",
            case.name
        );
    }
}

fn assert_plan_pages(
    case: &Case,
    compiled: &instant_instaql::CompiledQuery,
    root: &PlannedNestedGroup,
    catalog: &AttributeCatalog,
) {
    let compiled_root = &compiled.roots[0];
    assert_eq!(
        compiled_root.page.as_ref().map(|page| page.direction),
        case.root_order,
        "{}: root planner direction",
        case.name
    );
    assert_eq!(
        root.page.as_ref().map(|page| page.attr_id),
        case.root_order.map(|_| attr(catalog, "users", "handle")),
        "{}: root page metadata",
        case.name
    );
    let child = &compiled_root.children[0];
    assert_eq!(
        child.options.order.as_ref().map(|order| order.direction),
        Some(case.child_order),
        "{}: child shaping direction",
        case.name
    );
    assert!(
        child.page.is_none(),
        "{}: nested order does not create top-level-only page metadata",
        case.name
    );
    let child_group = only_group(only_group(root));
    assert!(child_group.page.is_none(), "{}", case.name);
}

fn build_envelope(
    root_selector: &PlannedNestedGroup,
    users: &[User],
    catalog: &AttributeCatalog,
) -> Map<String, Value> {
    let root_fetch = only_group(root_selector);
    let shelf_selector = only_group(root_fetch);
    let shelf_fetch = only_group(shelf_selector);
    let mut envelope = Map::new();

    insert_rows(
        &mut envelope,
        root_selector,
        users
            .iter()
            .map(|user| selector_row(root_selector, &root_seeds(user, catalog))),
    );
    insert_rows(
        &mut envelope,
        root_fetch,
        users
            .iter()
            .flat_map(|user| root_fetch_rows(root_fetch, user, catalog)),
    );
    insert_rows(
        &mut envelope,
        shelf_selector,
        users.iter().flat_map(|user| {
            user.shelves.iter().map(|shelf| {
                selector_row(shelf_selector, &shelf_selector_seeds(user, shelf, catalog))
            })
        }),
    );
    insert_rows(
        &mut envelope,
        shelf_fetch,
        users.iter().flat_map(|user| {
            user.shelves
                .iter()
                .flat_map(|shelf| shelf_fetch_rows(shelf_fetch, shelf, catalog))
        }),
    );
    insert_page_flags(&mut envelope, root_selector);
    insert_page_flags(&mut envelope, shelf_selector);
    envelope
}

fn root_seeds(user: &User, catalog: &AttributeCatalog) -> HashMap<Uuid, TripleSeed> {
    let page_shelf = user
        .shelves
        .iter()
        .find(|shelf| shelf.order == 0)
        .or_else(|| user.shelves.first())
        .expect("fixture users have shelves");
    HashMap::from([
        seed(
            user.id,
            attr(catalog, "users", "id"),
            json!(user.id),
            false,
            user.created_at,
        ),
        seed(
            user.id,
            attr(catalog, "users", "handle"),
            json!(user.handle),
            false,
            user.created_at + 1,
        ),
        seed(
            user.id,
            attr(catalog, "users", "age"),
            json!(user.age),
            false,
            user.created_at + 2,
        ),
        seed(
            user.id,
            attr(catalog, "users", "bookshelves"),
            json!(page_shelf.id),
            true,
            page_shelf.created_at,
        ),
        seed(
            page_shelf.id,
            attr(catalog, "bookshelves", "order"),
            json!(page_shelf.order),
            false,
            page_shelf.created_at + 1,
        ),
    ])
}

fn shelf_selector_seeds(
    user: &User,
    shelf: &Shelf,
    catalog: &AttributeCatalog,
) -> HashMap<Uuid, TripleSeed> {
    HashMap::from([
        seed(
            user.id,
            attr(catalog, "users", "bookshelves"),
            json!(shelf.id),
            true,
            shelf.created_at,
        ),
        seed(
            shelf.id,
            attr(catalog, "bookshelves", "order"),
            json!(shelf.order),
            false,
            shelf.created_at + 1,
        ),
    ])
}

fn root_fetch_rows(
    group: &PlannedNestedGroup,
    user: &User,
    catalog: &AttributeCatalog,
) -> Vec<Value> {
    [
        (attr(catalog, "users", "id"), json!(user.id)),
        (attr(catalog, "users", "handle"), json!(user.handle)),
        (attr(catalog, "users", "age"), json!(user.age)),
    ]
    .into_iter()
    .filter(|(attribute, _)| fetches_attribute(group, *attribute))
    .map(|(attribute, value)| row(group, user.id, attribute, value, false, user.created_at))
    .collect()
}

fn shelf_fetch_rows(
    group: &PlannedNestedGroup,
    shelf: &Shelf,
    catalog: &AttributeCatalog,
) -> Vec<Value> {
    [
        (attr(catalog, "bookshelves", "id"), json!(shelf.id)),
        (attr(catalog, "bookshelves", "name"), json!(shelf.name)),
        (attr(catalog, "bookshelves", "order"), json!(shelf.order)),
    ]
    .into_iter()
    .filter(|(attribute, _)| fetches_attribute(group, *attribute))
    .map(|(attribute, value)| row(group, shelf.id, attribute, value, false, shelf.created_at))
    .collect()
}

fn fetches_attribute(group: &PlannedNestedGroup, attribute: Uuid) -> bool {
    let PatternMetadataNode::Pattern(metadata) = &group.metadata[0] else {
        panic!("fetch group uses one direct pattern");
    };
    let attribute = attribute.to_string();
    matches!(
        &metadata.pattern.attribute,
        Term::Constants(values)
            if values.iter().any(|value| value.as_str() == Some(attribute.as_str()))
    )
}

fn selector_row(group: &PlannedNestedGroup, seeds: &HashMap<Uuid, TripleSeed>) -> Value {
    let mut object = Map::new();
    for metadata in &group.metadata {
        insert_metadata(metadata, seeds, &mut object);
    }
    if let Some(page) = &group.page {
        let order = &seeds[&page.attr_id];
        object.insert(page.fallback_entity_column.clone(), json!(order.entity));
        object.insert(
            page.fallback_created_at_column.clone(),
            json!(order.created_at),
        );
    }
    Value::Object(object)
}

fn insert_metadata(
    node: &PatternMetadataNode,
    seeds: &HashMap<Uuid, TripleSeed>,
    object: &mut Map<String, Value>,
) {
    match node {
        PatternMetadataNode::Pattern(metadata) => {
            let attribute = match &metadata.pattern.attribute {
                Term::Constants(values) => values
                    .iter()
                    .find_map(|value| value.as_str()?.parse().ok())
                    .expect("selector attribute is a UUID constant"),
                Term::Any | Term::Variable(_) | Term::Function(_) => {
                    panic!("selector attribute is constant")
                }
            };
            let seed = &seeds[&attribute];
            insert_cells(object, &metadata.cte_columns, seed);
        }
        PatternMetadataNode::Or(branches) => {
            for branch in branches {
                for metadata in branch {
                    insert_metadata(metadata, seeds, object);
                }
            }
        }
    }
}

fn row(
    group: &PlannedNestedGroup,
    entity: Uuid,
    attribute: Uuid,
    value: Value,
    reference: bool,
    created_at: i64,
) -> Value {
    let PatternMetadataNode::Pattern(metadata) = &group.metadata[0] else {
        panic!("fetch group uses one direct pattern");
    };
    let seed = TripleSeed {
        entity,
        attribute,
        value,
        reference,
        created_at,
    };
    let mut object = Map::new();
    insert_cells(&mut object, &metadata.cte_columns, &seed);
    Value::Object(object)
}

fn insert_cells(object: &mut Map<String, Value>, columns: &[String], seed: &TripleSeed) {
    let [entity, attribute, value, reference, created_at] = columns else {
        panic!("pattern metadata exposes five columns");
    };
    object.insert(entity.clone(), json!(seed.entity));
    object.insert(attribute.clone(), json!(seed.attribute));
    object.insert(value.clone(), seed.value.clone());
    object.insert(reference.clone(), json!(seed.reference));
    object.insert(created_at.clone(), json!(seed.created_at));
}

fn insert_rows(
    envelope: &mut Map<String, Value>,
    group: &PlannedNestedGroup,
    rows: impl IntoIterator<Item = Value>,
) {
    envelope.insert(
        group.table.clone().expect("fixture group has a table"),
        Value::Array(rows.into_iter().collect()),
    );
}

fn insert_page_flags(envelope: &mut Map<String, Value>, group: &PlannedNestedGroup) {
    if let Some(page) = &group.page {
        envelope.insert(page.has_next_table.clone(), json!([{"exists": false}]));
        envelope.insert(page.has_previous_table.clone(), json!([{"exists": false}]));
    }
}

fn assert_page_metadata(case: &Case, executed: &[ExecutedNode]) {
    let name = &case.name;
    assert_eq!(
        executed[0].selector.page_info.is_some(),
        case.root_order.is_some(),
        "{name}: decoded root page presence"
    );
    if let Some(root_page) = &executed[0].selector.page_info {
        assert!(!root_page.has_next_page, "{name}");
        assert!(!root_page.has_previous_page, "{name}");
        assert_cursor(
            root_page.start_cursor.as_ref(),
            case.users.first().map(|user| (user.id, json!(user.handle))),
            name,
        );
        assert_cursor(
            root_page.end_cursor.as_ref(),
            case.users.last().map(|user| (user.id, json!(user.handle))),
            name,
        );
    }
    for (entity, user) in executed[0].entities.iter().zip(&case.users) {
        assert_eq!(
            entity.children[0].entities.len(),
            user.shelves.len(),
            "{name}"
        );
        assert!(
            entity.children[0].selector.page_info.is_none(),
            "{name}: nested order is shaping-only"
        );
    }
}

fn assert_cursor(
    actual: Option<&instant_datalog::PageCursorResult>,
    expected: Option<(Uuid, Value)>,
    name: &str,
) {
    let actual = actual.expect("nonempty fixture page has a cursor");
    let (entity, value) = expected.expect("nonempty fixture page has expected cursor data");
    assert_eq!(actual.entity_id, entity, "{name}");
    assert_eq!(actual.value, Some(DatalogValue::Json(value)), "{name}");
}

fn seed(
    entity: Uuid,
    attribute: Uuid,
    value: Value,
    reference: bool,
    created_at: i64,
) -> (Uuid, TripleSeed) {
    (
        attribute,
        TripleSeed {
            entity,
            attribute,
            value,
            reference,
            created_at,
        },
    )
}

fn only_group(group: &PlannedNestedGroup) -> &PlannedNestedGroup {
    let children = group.children.as_ref().expect("fixture group has children");
    assert_eq!(children.groups.len(), 1);
    &children.groups[0]
}

fn attr(catalog: &AttributeCatalog, etype: &str, label: &str) -> Uuid {
    catalog
        .attributes
        .iter()
        .find(|attribute| attribute.etype == etype && attribute.label == label)
        .expect("fixture attribute exists")
        .id
}

fn catalog() -> AttributeCatalog {
    serde_json::from_value(
        serde_json::from_str::<Value>(include_str!(
            "../../instant-instaql/tests/fixtures/where_compilation.json"
        ))
        .unwrap()["catalog"]
            .clone(),
    )
    .unwrap()
}
