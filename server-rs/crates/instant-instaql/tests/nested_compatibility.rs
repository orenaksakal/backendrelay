use std::collections::{HashMap, HashSet};

use instant_datalog::{
    DatalogResult, DatalogTriple, DatalogValue, MatchCteOptions, NormalizedClause, PlannedCte,
    Term, render_nested_match_query,
};
use instant_instaql::{
    AttributeCatalog, ExecutedEntity, ExecutedNode, InstaqlError, NestedPatternGroup,
    QueryConstraint, WhereCondition, WhereValue, collect_nodes, compile_query,
    compile_query_with_constraints, plan_nested_query, shape_response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct ResultShapeFixture {
    oracle: String,
    cases: Vec<ResultShapeCase>,
}

#[derive(Deserialize)]
struct ResultShapeCase {
    name: String,
    query: Value,
    parent_entity: Uuid,
    child_entity: Uuid,
    child_id_attribute: Uuid,
    child_name_attribute: Uuid,
    child_name: String,
    expected: Value,
}

#[derive(Deserialize)]
struct NumericSortFixture {
    oracle: String,
    cases: Vec<NumericSortCase>,
}

#[derive(Deserialize)]
struct NumericSortCase {
    name: String,
    query: Value,
    entities: Vec<NumericSortEntity>,
    expected_ids: Vec<Uuid>,
}

#[derive(Deserialize)]
struct NumericSortEntity {
    id: Uuid,
    order: Value,
}

#[derive(Deserialize)]
struct StringSortFixture {
    oracle: String,
    cases: Vec<StringSortCase>,
}

#[derive(Deserialize)]
struct StringSortCase {
    name: String,
    query: Value,
    entities: Vec<StringSortEntity>,
    expected_ids: Vec<Uuid>,
}

#[derive(Deserialize)]
struct StringSortEntity {
    id: Uuid,
    title: String,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

#[test]
fn nested_forms_compile_into_selector_fetch_and_child_groups() {
    let catalog = catalog();
    let query = compile_query(
        &json!({
            "users": {
                "$": {"where": {"handle": "alex"}, "fields": ["handle"]},
                "bookshelves": {"$": {"where": {"name": "Nonfiction"}}}
            }
        }),
        &catalog,
        false,
    )
    .unwrap();

    let root = &query.roots[0];
    assert_eq!(root.key, "users");
    assert_eq!(root.fetch_attr_ids.len(), 2, "id is always fetched");
    assert_eq!(root.children.len(), 1);
    let child = &root.children[0];
    let relationship = child.relationship.as_ref().unwrap();
    assert!(relationship.forward);
    assert_eq!(
        relationship.attr_id,
        uuid("44444444-4444-4444-4444-444444444444")
    );
    assert_eq!(
        child.selector_clauses.len(),
        2,
        "relationship precedes where"
    );

    let groups = query.pattern_groups().unwrap();
    assert_eq!(groups.len(), 1);
    let root_fetch = only_fetch(&groups[0]);
    let root_fetch_pattern = pattern(&root_fetch.clauses[0]);
    assert!(matches!(&root_fetch_pattern.attribute, Term::Constants(values) if values.len() == 2));
    let child_selector = &root_fetch.children.as_ref().unwrap().groups[0];
    let relation_pattern = pattern(&child_selector.clauses[0]);
    assert_eq!(relation_pattern.index.key.to_string(), "vae");
    let where_pattern = pattern(&child_selector.clauses[1]);
    assert!(
        matches!(&where_pattern.entity, Term::Variable(symbol) if symbol == &relationship.child_symbol),
        "child filters must bind the level-1 relationship symbol"
    );
    let child_fetch = only_fetch(child_selector);
    assert!(
        matches!(&pattern(&child_fetch.clauses[0]).attribute, Term::Constants(values) if values.len() == 3)
    );
}

#[test]
fn permission_constraints_apply_to_every_matching_root_and_nested_form() {
    let constraints = HashMap::from([
        (
            "users".to_owned(),
            QueryConstraint::Where(vec![WhereCondition::Condition {
                path: vec!["handle".to_owned()],
                value: WhereValue::Scalar(json!("permitted")),
            }]),
        ),
        ("bookshelves".to_owned(), QueryConstraint::DenyAll),
    ]);
    let query = compile_query_with_constraints(
        &json!({
            "users": {"bookshelves": {}},
            "bookshelves": {}
        }),
        &catalog(),
        false,
        &constraints,
    )
    .unwrap();

    assert_eq!(query.roots.len(), 2);
    let users = query
        .roots
        .iter()
        .find(|root| root.etype == "users")
        .unwrap();
    assert_eq!(users.options.where_clause.len(), 1);
    assert_eq!(users.children.len(), 1);
    assert!(!users.children[0].missing);
    assert!(users.children[0].children.is_empty());
    assert!(
        users.children[0]
            .options
            .where_clause
            .iter()
            .any(|condition| {
                matches!(
                    condition,
                    WhereCondition::Condition {
                        path,
                        value: WhereValue::Scalar(Value::String(value)),
                    } if path == &["id"] && Uuid::parse_str(value).is_ok()
                )
            })
    );
    let shelves = query
        .roots
        .iter()
        .find(|root| root.etype == "bookshelves")
        .unwrap();
    assert!(!shelves.missing);
    assert!(shelves.options.where_clause.iter().any(|condition| {
        matches!(
            condition,
            WhereCondition::Condition {
                path,
                value: WhereValue::Scalar(Value::String(value)),
            } if path == &["id"] && Uuid::parse_str(value).is_ok()
        )
    }));
}

#[test]
fn reverse_children_compile_with_child_entity_on_the_reference_entity_side() {
    let query = compile_query(&json!({"bookshelves": {"owner": {}}}), &catalog(), false).unwrap();
    let child = &query.roots[0].children[0];
    let relationship = child.relationship.as_ref().unwrap();
    assert!(!relationship.forward);
    assert_eq!(child.etype, "users");
    let relation = pattern(&child.selector_clauses[0]);
    assert!(
        matches!(&relation.entity, Term::Variable(symbol) if symbol == &relationship.child_symbol)
    );
    assert!(
        matches!(&relation.value, Term::Variable(symbol) if symbol == &relationship.parent_symbol)
    );
}

#[test]
fn existing_scalar_children_remain_validation_errors() {
    let error = compile_query(&json!({"users": {"handle": {}}}), &catalog(), false).unwrap_err();
    assert!(error.to_string().contains("needs to be a link"));
}

#[test]
fn nested_relationship_compilation_stops_at_the_first_error() {
    let InstaqlError::Validation(errors) = compile_query(
        &json!({"users": {"handle": {}, "age": {}}}),
        &catalog(),
        false,
    )
    .unwrap_err() else {
        panic!("expected nested validation errors");
    };

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].expected, "link?");
    assert_eq!(errors[0].path, ["users", "age"]);
}

#[test]
fn nested_tree_lowers_to_one_monotonic_postgres_cte_graph() {
    let query = compile_query(
        &json!({
            "users": {
                "$": {"where": {"handle": "alex"}},
                "bookshelves": {"$": {"where": {"name": "Nonfiction"}}}
            }
        }),
        &catalog(),
        false,
    )
    .unwrap();
    let plan = plan_nested_query(
        &query,
        uuid("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee"),
        &MatchCteOptions::default(),
    )
    .unwrap();
    let names = plan
        .ctes
        .iter()
        .map(|cte| match cte {
            PlannedCte::Match(cte) => cte.name.as_str(),
            PlannedCte::OrGather(cte) => cte.name.as_str(),
            PlannedCte::Select(cte) => cte.name.as_str(),
        })
        .collect::<Vec<_>>();
    assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len());
    assert_eq!(plan.sql_plan().result_tables.len(), 4);
    let dependency_ctes = plan
        .ctes
        .iter()
        .filter_map(|cte| match cte {
            PlannedCte::Select(cte)
                if cte.statement.select.len() == 1
                    && cte.statement.select[0]
                        .alias
                        .as_ref()
                        .is_some_and(|alias| alias == &format!("{}-entity-id", cte.name)) =>
            {
                Some(cte)
            }
            PlannedCte::Match(_) | PlannedCte::OrGather(_) | PlannedCte::Select(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dependency_ctes.len(), 3);

    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert!(rendered.sql.contains("json_build_array(json_build_object("));
    assert!(rendered.sql.contains("json_uuid_to_uuid"));
    assert_eq!(rendered.sql.matches("json_agg(row_to_json(").count(), 4);
}

#[test]
fn nested_count_uses_a_scalar_result_table() {
    let query = compile_query(
        &json!({"users": {"$": {"aggregate": "count"}}}),
        &catalog(),
        true,
    )
    .unwrap();
    let plan = plan_nested_query(
        &query,
        uuid("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee"),
        &MatchCteOptions::default(),
    )
    .unwrap();
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert!(rendered.sql.contains("SELECT count(*) FROM"));
    assert!(!rendered.sql.contains("json_agg(row_to_json("));
}

#[test]
fn executed_nested_results_shape_into_public_objects_and_reference_nodes() {
    let catalog = catalog();
    let query = compile_query(
        &json!({"users": {"bookshelves": {"$": {"order": {"order": "desc"}}}}}),
        &catalog,
        false,
    )
    .unwrap();
    let user = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let shelf_one = uuid("cccccccc-cccc-cccc-cccc-cccccccccccc");
    let shelf_two = uuid("dddddddd-dddd-dddd-dddd-dddddddddddd");
    let results = vec![ExecutedNode {
        selector: DatalogResult::default(),
        aggregate: None,
        entities: vec![ExecutedEntity {
            fetch: datalog_result(vec![
                triple(
                    user,
                    "11111111-1111-1111-1111-111111111111",
                    json!(user),
                    10,
                ),
                triple(
                    user,
                    "22222222-2222-2222-2222-222222222222",
                    json!("alex"),
                    11,
                ),
            ]),
            children: vec![ExecutedNode {
                selector: DatalogResult::default(),
                aggregate: None,
                entities: vec![
                    ExecutedEntity {
                        fetch: datalog_result(vec![
                            triple(
                                shelf_one,
                                "55555555-5555-5555-5555-555555555555",
                                json!(shelf_one),
                                20,
                            ),
                            triple(
                                shelf_one,
                                "aaaaaaaa-1111-1111-1111-111111111111",
                                json!("One"),
                                21,
                            ),
                            triple(
                                shelf_one,
                                "aaaaaaaa-2222-2222-2222-222222222222",
                                json!(1),
                                22,
                            ),
                        ]),
                        children: Vec::new(),
                    },
                    ExecutedEntity {
                        fetch: datalog_result(vec![
                            triple(
                                shelf_two,
                                "55555555-5555-5555-5555-555555555555",
                                json!(shelf_two),
                                30,
                            ),
                            triple(
                                shelf_two,
                                "aaaaaaaa-1111-1111-1111-111111111111",
                                json!("Two"),
                                31,
                            ),
                            triple(
                                shelf_two,
                                "aaaaaaaa-2222-2222-2222-222222222222",
                                json!(2),
                                32,
                            ),
                        ]),
                        children: Vec::new(),
                    },
                ],
            }],
        }],
    }];

    let nodes = collect_nodes(&query, results.clone()).unwrap();
    assert_eq!(nodes[0].child_nodes.len(), 1);
    assert_eq!(
        nodes[0].child_nodes[0].child_nodes[0].data.key.as_deref(),
        Some("bookshelves")
    );

    let response = shape_response(&query, &results, &catalog, true).unwrap();
    let users = response.data["users"].as_array().unwrap();
    let shelves = users[0]["bookshelves"].as_array().unwrap();
    assert_eq!(shelves[0]["order"], 2);
    assert_eq!(shelves[1]["order"], 1);
    assert_eq!(users[0]["handle"], "alex");
    assert!(users[0].get("$serverCreatedAt").is_none());
}

#[test]
fn relationship_only_entities_shape_like_the_clojure_merge_contract() {
    let catalog = catalog();
    let fixture = serde_json::from_str::<ResultShapeFixture>(include_str!(
        "fixtures/result_shaping_edge_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let results = vec![ExecutedNode {
            selector: DatalogResult::default(),
            aggregate: None,
            entities: vec![ExecutedEntity {
                // Permission/field filtering can leave the parent with only
                // relationship children. The JVM merges those children into
                // an initially nil scalar map instead of rejecting the node.
                fetch: DatalogResult::default(),
                children: vec![ExecutedNode {
                    selector: DatalogResult::default(),
                    aggregate: None,
                    entities: vec![ExecutedEntity {
                        fetch: datalog_result(vec![
                            DatalogTriple {
                                entity_id: case.child_entity,
                                attr_id: case.child_id_attribute,
                                value: DatalogValue::Json(json!(case.child_entity)),
                                created_at: 20,
                            },
                            DatalogTriple {
                                entity_id: case.child_entity,
                                attr_id: case.child_name_attribute,
                                value: DatalogValue::Json(json!(case.child_name)),
                                created_at: 21,
                            },
                        ]),
                        children: Vec::new(),
                    }],
                }],
            }],
        }];
        assert_ne!(case.parent_entity, case.child_entity, "{}", case.name);
        let response = shape_response(&query, &results, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_eq!(
            serde_json::to_value(response.data).unwrap(),
            case.expected,
            "{}",
            case.name
        );
    }
}

#[test]
fn numeric_result_order_preserves_integral_precision_like_clojure_compare() {
    let catalog = catalog();
    let fixture = serde_json::from_str::<NumericSortFixture>(include_str!(
        "fixtures/numeric_sort_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let results = vec![ExecutedNode {
            selector: DatalogResult::default(),
            aggregate: None,
            entities: case
                .entities
                .into_iter()
                .enumerate()
                .map(|(index, entity)| ExecutedEntity {
                    fetch: datalog_result(vec![
                        triple(
                            entity.id,
                            "55555555-5555-5555-5555-555555555555",
                            json!(entity.id),
                            i64::try_from(index).unwrap(),
                        ),
                        triple(
                            entity.id,
                            "aaaaaaaa-2222-2222-2222-222222222222",
                            entity.order,
                            i64::try_from(index).unwrap(),
                        ),
                    ]),
                    children: Vec::new(),
                })
                .collect(),
        }];

        let response = shape_response(&query, &results, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let actual_ids = response.data["bookshelves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entity| Uuid::parse_str(entity["id"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual_ids, case.expected_ids, "{}", case.name);
    }
}

#[test]
fn string_result_order_uses_the_jvms_utf16_code_unit_contract() {
    let catalog = catalog();
    let fixture = serde_json::from_str::<StringSortFixture>(include_str!(
        "fixtures/string_sort_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let results = vec![ExecutedNode {
            selector: DatalogResult::default(),
            aggregate: None,
            entities: case
                .entities
                .into_iter()
                .enumerate()
                .map(|(index, entity)| ExecutedEntity {
                    fetch: datalog_result(vec![
                        triple(
                            entity.id,
                            "77777777-7777-7777-7777-777777777777",
                            json!(entity.id),
                            i64::try_from(index).unwrap(),
                        ),
                        triple(
                            entity.id,
                            "88888888-8888-8888-8888-888888888888",
                            json!(entity.title),
                            i64::try_from(index).unwrap(),
                        ),
                    ]),
                    children: Vec::new(),
                })
                .collect(),
        }];

        let response = shape_response(&query, &results, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let actual_ids = response.data["books"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entity| Uuid::parse_str(entity["id"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual_ids, case.expected_ids, "{}", case.name);
    }
}

fn only_fetch(group: &NestedPatternGroup) -> &NestedPatternGroup {
    let children = group.children.as_ref().expect("selector has fetch child");
    assert_eq!(children.groups.len(), 1);
    &children.groups[0]
}

fn pattern(clause: &NormalizedClause) -> &instant_datalog::Pattern {
    let NormalizedClause::Pattern { pattern } = clause else {
        panic!("expected pattern clause");
    };
    pattern
}

fn triple(entity_id: Uuid, attr_id: &str, value: Value, created_at: i64) -> DatalogTriple {
    DatalogTriple {
        entity_id,
        attr_id: uuid(attr_id),
        value: DatalogValue::Json(value),
        created_at,
    }
}

fn datalog_result(triples: Vec<DatalogTriple>) -> DatalogResult {
    DatalogResult {
        join_rows: vec![triples],
        ..DatalogResult::default()
    }
}
