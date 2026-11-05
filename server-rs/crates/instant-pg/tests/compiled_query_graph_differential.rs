use std::collections::HashSet;

use instant_datalog::{MatchCteOptions, NestedResultKind, PlannedCte};
use instant_instaql::{AttributeCatalog, compile_query, plan_nested_query};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    source: String,
    app_id: Uuid,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    #[serde(default)]
    admin: bool,
    expected_cte_names: Vec<String>,
    expected_cte_kinds: Vec<String>,
    expected_result_kinds: Vec<String>,
}

#[test]
fn adapted_upstream_query_shapes_lower_to_stable_rust_cte_graphs() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "fixtures/compiled_query_graph_differential.json"
    ))
    .unwrap();
    assert!(fixture.source.contains("45d3ffbb"));
    let catalog = catalog();

    for case in fixture.cases {
        let compiled = compile_query(&case.query, &catalog, case.admin)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let plan = plan_nested_query(&compiled, fixture.app_id, &MatchCteOptions::default())
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let cte_names = plan
            .ctes
            .iter()
            .map(|cte| match cte {
                PlannedCte::Match(cte) => cte.name.clone(),
                PlannedCte::OrGather(cte) => cte.name.clone(),
                PlannedCte::Select(cte) => cte.name.clone(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cte_names.len(),
            cte_names.iter().collect::<HashSet<_>>().len(),
            "{}: CTE names are globally unique",
            case.name
        );
        assert_eq!(cte_names, case.expected_cte_names, "{}", case.name);
        let kinds = plan
            .ctes
            .iter()
            .map(|cte| match cte {
                PlannedCte::Match(_) => "match",
                PlannedCte::OrGather(_) => "gather",
                PlannedCte::Select(_) => "select",
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(kinds, case.expected_cte_kinds, "{}", case.name);
        let result_kinds = plan
            .sql_plan()
            .result_tables
            .into_iter()
            .map(|table| match table.kind {
                NestedResultKind::Rows => "rows",
                NestedResultKind::Count => "count",
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(result_kinds, case.expected_result_kinds, "{}", case.name);
    }
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
