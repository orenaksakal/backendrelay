use std::collections::HashMap;

use instant_datalog::{
    BindingPath, BindingTree, Clause, Component, IndexKey, MatchCteOptions, MatchQueryContext,
    MatchQueryPlan, PatternError, PatternMetadataNode, PlannedCte, Symbol, plan_match_query,
    plan_match_query_from_index, render_match_query,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const APP_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

fn plan_json_clauses(value: Value) -> MatchQueryPlan {
    let clauses: Vec<Clause> = serde_json::from_value(value).unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let options = MatchCteOptions::default();
    plan_match_query(
        &MatchQueryContext {
            prefix: "match-",
            app_id: Uuid::parse_str(APP_ID).unwrap(),
            additional_joins: &HashMap::new(),
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        },
        &clauses,
    )
    .unwrap()
}

#[derive(Debug, Deserialize)]
struct QueryFixture {
    name: String,
    source: String,
    clauses: Vec<Clause>,
    expected_next_index: usize,
    expected_final_table: String,
    expected_cte_names: Vec<String>,
    expected_gather: Value,
}

#[test]
fn recursive_query_planning_matches_the_clojure_state_machine() {
    let fixture: QueryFixture =
        serde_json::from_str(include_str!("fixtures/query_planning.json")).unwrap();
    let clauses = fixture
        .clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let additional_joins = HashMap::new();
    let options = MatchCteOptions::default();
    let plan = plan_match_query(
        &MatchQueryContext {
            prefix: "match-",
            app_id: Uuid::parse_str(APP_ID).unwrap(),
            additional_joins: &additional_joins,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        },
        &clauses,
    )
    .unwrap();

    assert_eq!(
        plan.next_index, fixture.expected_next_index,
        "fixture {} derived from {}",
        fixture.name, fixture.source
    );
    assert_eq!(plan.final_table, fixture.expected_final_table);
    let cte_names: Vec<_> = plan
        .ctes
        .iter()
        .map(|cte| match cte {
            PlannedCte::Match(cte) => cte.name.clone(),
            PlannedCte::OrGather(cte) => cte.name.clone(),
            PlannedCte::Select(cte) => cte.name.clone(),
        })
        .collect();
    assert_eq!(cte_names, fixture.expected_cte_names);
    let PlannedCte::OrGather(gather) = &plan.ctes[2] else {
        panic!("third CTE must gather the OR branches");
    };
    assert_eq!(gather.to_honey_sql_json(), fixture.expected_gather);

    let join_symbol = Symbol::new("?e").unwrap();
    let bindings = &plan.symbol_map[&join_symbol];
    assert!(matches!(
        bindings.as_slice(),
        [BindingTree::Or(branches)] if branches.len() == 2
    ));
    assert!(matches!(
        plan.pattern_metadata.as_slice(),
        [PatternMetadataNode::Or(branches)]
            if branches.len() == 2 && branches.iter().all(|branch| branch.len() == 1)
    ));

    let query = plan.to_honey_sql_json();
    assert_eq!(query[":select"], Value::String(":match-2.*".to_owned()));
    assert_eq!(query[":from"], Value::String(":match-2".to_owned()));
    assert_eq!(query[":with"].as_array().map(Vec::len), Some(3));
}

#[test]
fn nonzero_start_index_does_not_imply_an_or_predecessor() {
    let clauses: Vec<Clause> = serde_json::from_value(serde_json::json!([{
        "kind": "or",
        "join_symbol": "?e",
        "clauses": [
            {
                "kind": "pattern",
                "pattern": {
                    "index": { "key": "ea" },
                    "components": [
                        { "kind": "variable", "value": "?e" },
                        { "kind": "constant", "value": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb" }
                    ]
                }
            },
            {
                "kind": "pattern",
                "pattern": {
                    "index": { "key": "ea" },
                    "components": [
                        { "kind": "variable", "value": "?e" },
                        { "kind": "constant", "value": "cccccccc-cccc-cccc-cccc-cccccccccccc" }
                    ]
                }
            }
        ]
    }]))
    .unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let options = MatchCteOptions::default();
    let plan = plan_match_query_from_index(
        &MatchQueryContext {
            prefix: "match-",
            app_id: Uuid::parse_str(APP_ID).unwrap(),
            additional_joins: &HashMap::new(),
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        },
        &clauses,
        10,
    )
    .unwrap();

    assert_eq!(plan.next_index, 13);
    assert_eq!(plan.final_table, "match-12");
    let PlannedCte::OrGather(gather) = &plan.ctes[2] else {
        panic!("third CTE must gather the independent OR");
    };
    assert_eq!(gather.from.table, "match-10");
    assert_eq!(gather.full_joins.len(), 1);
    assert!(gather.left_joins.is_empty());
    let rendered = render_match_query(&plan).unwrap();
    assert!(!rendered.sql.contains("\"match_9\""));
    assert!(
        rendered
            .sql
            .contains("\"match_12\" AS NOT MATERIALIZED (SELECT \"match_10\".*, \"match_11\".*")
    );
}

#[test]
fn or_after_a_prior_pattern_uses_left_joins_and_a_filter() {
    let plan = plan_json_clauses(serde_json::json!([
        {
            "kind": "pattern",
            "pattern": {
                "index": { "key": "ea" },
                "components": [
                    { "kind": "variable", "value": "?e" },
                    {
                        "kind": "constant",
                        "value": "aaaaaaaa-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
                    },
                    { "kind": "constant", "value": "anchor" }
                ]
            }
        },
        {
            "kind": "or",
            "join_symbol": "?e",
            "clauses": [
                {
                    "kind": "pattern",
                    "pattern": {
                        "index": { "key": "ea" },
                        "components": [
                            { "kind": "variable", "value": "?e" },
                            {
                                "kind": "constant",
                                "value": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
                            }
                        ]
                    }
                },
                {
                    "kind": "pattern",
                    "pattern": {
                        "index": { "key": "ea" },
                        "components": [
                            { "kind": "variable", "value": "?e" },
                            {
                                "kind": "constant",
                                "value": "cccccccc-cccc-cccc-cccc-cccccccccccc"
                            }
                        ]
                    }
                }
            ]
        }
    ]));

    assert_eq!(plan.next_index, 4);
    assert_eq!(plan.final_table, "match-3");
    let PlannedCte::OrGather(gather) = &plan.ctes[3] else {
        panic!("fourth CTE must gather the OR branches");
    };
    assert_eq!(gather.from.table, "match-0");
    assert!(gather.full_joins.is_empty());
    assert_eq!(gather.left_joins.len(), 2);
    assert!(
        gather
            .left_joins
            .iter()
            .all(|join| join.predicate.is_some())
    );
    assert!(gather.predicate.is_some());
    assert!(matches!(
        plan.pattern_metadata.as_slice(),
        [PatternMetadataNode::Pattern(_), PatternMetadataNode::Or(_)]
    ));
    let bindings = &plan.symbol_map[&Symbol::new("?e").unwrap()];
    assert_eq!(bindings.len(), 2);
    assert!(matches!(bindings[0], BindingTree::Path(_)));
    assert!(matches!(bindings[1], BindingTree::Or(_)));
}

#[test]
fn nested_or_groups_keep_monotonic_indexes_and_nested_metadata() {
    let clauses: Vec<Clause> = serde_json::from_value(serde_json::json!([
        {
            "kind": "or",
            "join_symbol": "?e",
            "clauses": [
                {
                    "kind": "or",
                    "join_symbol": "?e",
                    "clauses": [
                        {
                            "kind": "pattern",
                            "pattern": {
                                "index": { "key": "ea" },
                                "components": [
                                    { "kind": "variable", "value": "?e" },
                                    {
                                        "kind": "constant",
                                        "value": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                                    }
                                ]
                            }
                        },
                        {
                            "kind": "pattern",
                            "pattern": {
                                "index": { "key": "ea" },
                                "components": [
                                    { "kind": "variable", "value": "?e" },
                                    {
                                        "kind": "constant",
                                        "value": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
                                    }
                                ]
                            }
                        }
                    ]
                },
                {
                    "kind": "pattern",
                    "pattern": {
                        "index": { "key": "ea" },
                        "components": [
                            { "kind": "variable", "value": "?e" },
                            {
                                "kind": "constant",
                                "value": "cccccccc-cccc-cccc-cccc-cccccccccccc"
                            }
                        ]
                    }
                }
            ]
        }
    ]))
    .unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let additional_joins = HashMap::new();
    let options = MatchCteOptions::default();
    let plan = plan_match_query(
        &MatchQueryContext {
            prefix: "match-",
            app_id: Uuid::parse_str(APP_ID).unwrap(),
            additional_joins: &additional_joins,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        },
        &clauses,
    )
    .unwrap();

    assert_eq!(plan.next_index, 5);
    assert_eq!(plan.final_table, "match-4");
    let kinds: Vec<_> = plan
        .ctes
        .iter()
        .map(|cte| match cte {
            PlannedCte::Match(_) => "match",
            PlannedCte::OrGather(_) => "gather",
            PlannedCte::Select(_) => "select",
        })
        .collect();
    assert_eq!(kinds, ["match", "match", "gather", "match", "gather"]);
    assert!(matches!(
        plan.pattern_metadata.as_slice(),
        [PatternMetadataNode::Or(outer)]
            if matches!(
                outer.as_slice(),
                [first, second]
                    if matches!(first.as_slice(), [PatternMetadataNode::Or(_)])
                        && matches!(second.as_slice(), [PatternMetadataNode::Pattern(_)])
            )
    ));
}

#[test]
fn first_nested_or_branch_does_not_reproject_the_outer_predecessor() {
    let plan = plan_json_clauses(serde_json::json!([
        {
            "kind": "pattern",
            "pattern": {
                "index": { "key": "ea" },
                "components": [
                    { "kind": "variable", "value": "?e" },
                    { "kind": "constant", "value": "aaaaaaaa-bbbb-bbbb-bbbb-bbbbbbbbbbbb" }
                ]
            }
        },
        {
            "kind": "or",
            "join_symbol": "?e",
            "clauses": [
                {
                    "kind": "or",
                    "join_symbol": "?e",
                    "clauses": [
                        {
                            "kind": "pattern",
                            "pattern": {
                                "index": { "key": "ea" },
                                "components": [
                                    { "kind": "variable", "value": "?e" },
                                    { "kind": "constant", "value": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb" }
                                ]
                            }
                        },
                        {
                            "kind": "pattern",
                            "pattern": {
                                "index": { "key": "ea" },
                                "components": [
                                    { "kind": "variable", "value": "?e" },
                                    { "kind": "constant", "value": "cccccccc-cccc-cccc-cccc-cccccccccccc" }
                                ]
                            }
                        }
                    ]
                },
                {
                    "kind": "pattern",
                    "pattern": {
                        "index": { "key": "ea" },
                        "components": [
                            { "kind": "variable", "value": "?e" },
                            { "kind": "constant", "value": "dddddddd-dddd-dddd-dddd-dddddddddddd" }
                        ]
                    }
                }
            ]
        },
        {
            "kind": "pattern",
            "pattern": {
                "index": { "key": "ea" },
                "components": [
                    { "kind": "variable", "value": "?e" },
                    { "kind": "constant", "value": "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee" }
                ]
            }
        }
    ]));

    assert_eq!(plan.next_index, 7);
    assert_eq!(plan.final_table, "match-6");
    let PlannedCte::OrGather(inner) = &plan.ctes[3] else {
        panic!("fourth CTE must gather the nested OR");
    };
    assert_eq!(inner.from.table, "match-1");
    assert_eq!(inner.full_joins.len(), 1);
    assert!(inner.left_joins.is_empty());
    let PlannedCte::OrGather(outer) = &plan.ctes[5] else {
        panic!("sixth CTE must gather the outer OR");
    };
    assert_eq!(outer.from.table, "match-0");
    assert_eq!(outer.left_joins.len(), 2);

    let rendered = render_match_query(&plan).unwrap();
    assert!(
        rendered
            .sql
            .contains("\"match_3\" AS NOT MATERIALIZED (SELECT \"match_1\".*, \"match_2\".*")
    );
    assert!(
        !rendered
            .sql
            .contains("\"match_3\" AS NOT MATERIALIZED (SELECT \"match_0\".*, \"match_1\".*")
    );
    assert!(rendered.sql.contains(
        "\"match_6\" AS MATERIALIZED (SELECT \"match_5\".*, \"entity_id\" AS \"match_6_entity_id\""
    ));
}

#[test]
fn parent_join_bindings_anchor_nested_query_plans() {
    let clauses: Vec<Clause> = serde_json::from_value(serde_json::json!([{
        "kind": "pattern",
        "pattern": {
            "index": {"key": "ea"},
            "components": [
                {"kind": "variable", "value": "parent"},
                {"kind": "variable", "value": "attribute"}
            ]
        }
    }]))
    .unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let options = MatchCteOptions::default();
    let app_id = Uuid::parse_str(APP_ID).unwrap();

    assert_eq!(
        plan_match_query(
            &MatchQueryContext {
                prefix: "match-",
                app_id,
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap_err(),
        PatternError::PatternNotJoinable { pattern_index: 0 }
    );

    let parent = Symbol::new("parent").unwrap();
    let additional_joins = HashMap::from([(
        parent,
        BindingPath::new(41, Component::Entity, IndexKey::Ea),
    )]);
    let plan = plan_match_query(
        &MatchQueryContext {
            prefix: "match-",
            app_id,
            additional_joins: &additional_joins,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        },
        &clauses,
    )
    .unwrap();
    assert_eq!(plan.final_table, "match-0");
}
