use std::collections::HashMap;

use instant_datalog::{
    DatalogValue, MatchCteOptions, ResultDecodeOptions, TopicPart,
    decode_paged_query_rows_with_options,
};
use instant_instaql::{
    AttributeCatalog, QueryConstraint, WhereCondition, WhereValue, coarse_invalidation_topics,
    compile_query_with_constraints, plan_nested_query,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    denied_forms: DeniedForms,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct DeniedForms {
    query: Value,
    denied_etype: String,
    id_attribute: Uuid,
    relationship_attribute: Uuid,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    constraints: Vec<Constraint>,
    expected: Vec<ExpectedDependency>,
    page: Option<ExpectedPage>,
}

#[derive(Deserialize)]
struct Constraint {
    etype: String,
    conditions: Vec<WhereCondition>,
}

#[derive(Deserialize)]
struct ExpectedDependency {
    path: Vec<String>,
    attribute: Uuid,
    value: Value,
}

#[derive(Deserialize)]
struct ExpectedPage {
    attribute: Uuid,
    reverse: bool,
    empty_has_previous: bool,
}

fn contains_condition(conditions: &[WhereCondition], path: &[String], value: &Value) -> bool {
    conditions.iter().any(|condition| match condition {
        WhereCondition::Condition {
            path: candidate,
            value: WhereValue::Scalar(candidate_value),
        } => candidate == path && candidate_value == value,
        WhereCondition::And { conditions } | WhereCondition::Or { conditions } => {
            contains_condition(conditions, path, value)
        }
        WhereCondition::Condition { .. } => false,
    })
}

#[test]
fn relationship_paths_include_permission_derived_topic_dependencies() {
    let catalog =
        serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
            .unwrap()
            .catalog;
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/permission_topic_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let constraints = case
            .constraints
            .into_iter()
            .map(|constraint| {
                (
                    constraint.etype,
                    QueryConstraint::Where(constraint.conditions),
                )
            })
            .collect::<HashMap<_, _>>();
        let query = compile_query_with_constraints(&case.query, &catalog, false, &constraints)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let root = query.roots.first().expect("fixture has one root");
        let topics = coarse_invalidation_topics(&query)
            .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));

        for expected in &case.expected {
            assert!(
                contains_condition(&root.options.where_clause, &expected.path, &expected.value),
                "{}: missing prefixed permission condition {:?}",
                case.name,
                expected.path
            );
            assert!(
                topics.iter().any(|topic| {
                    topic.attribute
                        == TopicPart::Values(vec![DatalogValue::Uuid(expected.attribute)])
                        && topic_part_contains_json(&topic.value, &expected.value)
                }),
                "{}: missing permission-derived invalidation dependency for {:?}",
                case.name,
                expected.path
            );
        }

        if let Some(expected_page) = case.page {
            let options = MatchCteOptions {
                ensure_empty_symbol_values_for_topics: Some(true),
                ..MatchCteOptions::default()
            };
            let planned = plan_nested_query(
                &query,
                Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
                &options,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            let group = &planned.groups[0];
            let page = group.page.as_ref().expect("fixture expects a page plan");
            assert_eq!(page.attr_id, expected_page.attribute, "{}", case.name);
            assert_eq!(page.reverse_results, expected_page.reverse, "{}", case.name);
            assert_eq!(
                page.empty_page_has_previous, expected_page.empty_has_previous,
                "{}",
                case.name
            );
            let result = decode_paged_query_rows_with_options(
                &[],
                &group.metadata,
                page,
                false,
                false,
                ResultDecodeOptions {
                    ensure_empty_symbol_values_for_topics: true,
                },
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            assert_eq!(
                result.page_info.unwrap().has_previous_page,
                expected_page.empty_has_previous,
                "{}",
                case.name
            );
            for expected in &case.expected {
                assert!(
                    result.topics.iter().any(|topic| {
                        topic.attribute
                            == TopicPart::Values(vec![DatalogValue::Uuid(expected.attribute)])
                            && topic_part_contains_json(&topic.value, &expected.value)
                    }),
                    "{}: precise empty-page topics lost permission dependency {:?}",
                    case.name,
                    expected.path
                );
            }
        }
    }
}

fn topic_part_contains_json(part: &TopicPart, expected: &Value) -> bool {
    matches!(part, TopicPart::Values(values) if values.contains(&DatalogValue::Json(expected.clone())))
}

#[test]
fn denied_relationship_paths_receive_the_upstream_impossible_id_guard() {
    let catalog =
        serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
            .unwrap()
            .catalog;
    let constraints = HashMap::from([("bookshelves".to_owned(), QueryConstraint::DenyAll)]);
    let query = compile_query_with_constraints(
        &serde_json::json!({"users":{"$":{"where":{"bookshelves.name":"requested"}}}}),
        &catalog,
        false,
        &constraints,
    )
    .unwrap();
    let root = &query.roots[0];
    let guard = root
        .options
        .where_clause
        .iter()
        .find_map(|condition| find_scalar(condition, &["bookshelves", "id"]))
        .expect("denied related type has a prefixed id guard");
    let guard = guard.as_str().expect("id guard is a UUID string");
    Uuid::parse_str(guard).expect("id guard uses the upstream random UUID shape");

    let id_attr = Uuid::parse_str("55555555-5555-5555-5555-555555555555").unwrap();
    let topics = coarse_invalidation_topics(&query).unwrap();
    assert!(topics.iter().any(|topic| {
        topic.attribute == TopicPart::Values(vec![DatalogValue::Uuid(id_attr)])
            && topic.value
                == TopicPart::Values(vec![DatalogValue::Json(Value::String(guard.to_owned()))])
    }));
}

#[test]
fn explicitly_requested_denied_forms_keep_normal_precise_dependencies() {
    let catalog =
        serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
            .unwrap()
            .catalog;
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/permission_topic_differential.json"
    ))
    .unwrap();
    let denied_fixture = fixture.denied_forms;
    let constraints = HashMap::from([(
        denied_fixture.denied_etype.clone(),
        QueryConstraint::DenyAll,
    )]);
    let query =
        compile_query_with_constraints(&denied_fixture.query, &catalog, false, &constraints)
            .unwrap();
    let users = query
        .roots
        .iter()
        .find(|root| root.etype == "users")
        .unwrap();
    let denied = [
        &users.children[0],
        query
            .roots
            .iter()
            .find(|root| root.etype == denied_fixture.denied_etype)
            .unwrap(),
    ];
    assert!(denied.iter().all(|node| !node.missing));
    let guards = denied
        .iter()
        .map(|node| {
            node.options
                .where_clause
                .iter()
                .find_map(|condition| find_scalar(condition, &["id"]))
                .and_then(Value::as_str)
                .expect("explicit denied form has an id guard")
        })
        .collect::<Vec<_>>();
    assert!(guards.iter().all(|guard| Uuid::parse_str(guard).is_ok()));

    let topics = coarse_invalidation_topics(&query).unwrap();
    assert!(topics.iter().any(|topic| {
        topic.attribute
            == TopicPart::Values(vec![DatalogValue::Uuid(
                denied_fixture.relationship_attribute,
            )])
    }));
    for guard in guards {
        assert!(topics.iter().any(|topic| {
            topic.attribute
                == TopicPart::Values(vec![DatalogValue::Uuid(denied_fixture.id_attribute)])
                && topic_part_contains_json(&topic.value, &Value::String(guard.to_owned()))
        }));
    }
}

fn find_scalar<'a>(condition: &'a WhereCondition, path: &[&str]) -> Option<&'a Value> {
    match condition {
        WhereCondition::Condition {
            path: candidate,
            value: WhereValue::Scalar(value),
        } if candidate
            .iter()
            .map(String::as_str)
            .eq(path.iter().copied()) =>
        {
            Some(value)
        }
        WhereCondition::And { conditions } | WhereCondition::Or { conditions } => conditions
            .iter()
            .find_map(|condition| find_scalar(condition, path)),
        WhereCondition::Condition { .. } => None,
    }
}
