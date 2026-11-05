use std::collections::HashMap;

use instant_datalog::{
    Clause, MatchCteOptions, MatchQueryContext, SqlParameter, plan_match_query, render_match_query,
};
use serde_json::Value;
use uuid::Uuid;

const APP_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

#[test]
fn recursive_or_plan_renders_as_parameterized_postgres_sql() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/query_planning.json")).unwrap();
    let clauses: Vec<Clause> = serde_json::from_value(fixture["clauses"].clone()).unwrap();
    let clauses = clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let additional_joins = HashMap::new();
    let options = MatchCteOptions::default();
    let app_id = Uuid::parse_str(APP_ID).unwrap();
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

    let rendered = render_match_query(&plan).unwrap();
    assert_eq!(
        rendered.sql,
        include_str!("fixtures/sql_rendering.sql").trim_end()
    );
    assert_eq!(
        rendered.parameters,
        vec![
            SqlParameter::Uuid(app_id),
            SqlParameter::Boolean(true),
            SqlParameter::Uuid(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap()),
            SqlParameter::Uuid(app_id),
            SqlParameter::Boolean(true),
            SqlParameter::Uuid(Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap()),
        ]
    );
}
