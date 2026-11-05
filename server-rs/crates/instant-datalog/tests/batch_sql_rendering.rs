use std::collections::HashMap;

use instant_datalog::{
    BatchResultKind, Clause, MatchCteOptions, MatchQueryContext, SqlRenderError, plan_match_query,
    render_batch_match_query, render_mixed_batch_match_query,
};
use serde_json::json;
use uuid::Uuid;

fn plan(prefix: &str, app_id: Uuid, attr_id: Uuid) -> instant_datalog::MatchQueryPlan {
    let clauses: Vec<Clause> = serde_json::from_value(json!([{
        "kind": "pattern",
        "pattern": {
            "index": {"key": "ea"},
            "components": [
                {"kind": "variable", "value": "?entity"},
                {"kind": "constant", "value": attr_id},
                {"kind": "variable", "value": "?value"}
            ]
        }
    }]))
    .unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    plan_match_query(
        &MatchQueryContext {
            prefix,
            app_id,
            additional_joins: &HashMap::new(),
            initial_symbol_cardinalities: &HashMap::new(),
            options: &MatchCteOptions::default(),
        },
        &clauses,
    )
    .unwrap()
}

#[test]
fn batches_ctes_parameters_and_json_results_in_request_order() {
    let first = plan(
        "match-0-",
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
    );
    let second = plan(
        "match-1-",
        Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap(),
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
    );

    let rendered = render_batch_match_query(&[first, second]).unwrap();
    assert_eq!(rendered.parameters.len(), 6);
    assert!(rendered.sql.contains("$1"));
    assert!(rendered.sql.contains("$6"));
    assert!(rendered.sql.contains(
        "SELECT json_build_object('match-0-0', COALESCE((SELECT json_agg(row_to_json(\"match_0_0\")) FROM \"match_0_0\"), '[]'::json), 'match-1-0'"
    ));
}

#[test]
fn rejects_colliding_or_oversized_batches() {
    let plan = plan(
        "match-0-",
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
    );
    assert!(matches!(
        render_batch_match_query(&[plan.clone(), plan.clone()]),
        Err(SqlRenderError::DuplicateCte(_))
    ));
    assert_eq!(
        render_batch_match_query(&vec![plan; 51]),
        Err(SqlRenderError::BatchTooLarge(51))
    );
}

#[test]
fn renders_rows_and_count_in_one_position_keyed_envelope() {
    let first = plan(
        "match-0-",
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
    );
    let second = plan(
        "match-1-",
        Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap(),
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
    );

    let rendered = render_mixed_batch_match_query(
        &[first, second],
        &[BatchResultKind::Rows, BatchResultKind::Count],
    )
    .unwrap();

    assert_eq!(rendered.parameters.len(), 6);
    assert!(rendered.sql.contains(
        "jsonb_build_object('match-0-0', COALESCE((SELECT jsonb_agg(to_jsonb(\"match_0_0\")) FROM \"match_0_0\"), '[]'::jsonb))"
    ));
    assert!(
        rendered
            .sql
            .contains("jsonb_build_object('match-1-0', (SELECT count(*) FROM \"match_1_0\"))")
    );
}

#[test]
fn mixed_batches_require_one_result_kind_per_plan() {
    let plan = plan(
        "match-0-",
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
    );

    assert_eq!(
        render_mixed_batch_match_query(&[plan], &[]),
        Err(SqlRenderError::BatchResultKindCount { plans: 1, kinds: 0 })
    );
}
