use std::collections::HashMap;

use instant_datalog::{
    BindingPath, IndexKey, MatchCteContext, MatchCteOptions, NestedSymbolMap, RawPattern, Symbol,
    build_match_cte,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const APP_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

#[derive(Debug, Deserialize)]
struct CteFixture {
    name: String,
    source: String,
    pattern_index: usize,
    start_of_group: bool,
    #[serde(default)]
    prior_patterns: Vec<PriorPattern>,
    pattern: RawPattern,
    #[serde(default)]
    options: MatchCteOptions,
    #[serde(default)]
    additional_joins: HashMap<Symbol, BindingPath>,
    expected_index: IndexKey,
    expected: Value,
}

#[derive(Debug, Deserialize)]
struct PriorPattern {
    pattern_index: usize,
    pattern: RawPattern,
}

#[test]
fn match_ctes_follow_the_clojure_planner_contract() {
    let fixtures: Vec<CteFixture> =
        serde_json::from_str(include_str!("fixtures/cte_planning.json")).unwrap();
    let app_id = Uuid::parse_str(APP_ID).unwrap();

    for fixture in fixtures {
        let mut symbol_map = NestedSymbolMap::new();
        for prior in fixture.prior_patterns {
            let pattern = prior.pattern.normalize().unwrap();
            for (symbol, bindings) in pattern.nested_symbol_map(prior.pattern_index) {
                symbol_map.entry(symbol).or_default().extend(bindings);
            }
        }
        let pattern = fixture.pattern.normalize().unwrap();
        let plan = build_match_cte(
            &MatchCteContext {
                prefix: "match-",
                app_id,
                pattern_index: fixture.pattern_index,
                start_of_group: fixture.start_of_group,
                symbol_map: &symbol_map,
                symbol_cardinalities: &HashMap::new(),
                additional_joins: &fixture.additional_joins,
                options: &fixture.options,
                row_estimate: None,
                suppress_row_hint: false,
            },
            &pattern,
        )
        .unwrap();

        assert_eq!(
            plan.pattern.index.key, fixture.expected_index,
            "fixture {} derived from {}",
            fixture.name, fixture.source
        );
        assert_eq!(
            plan.cte.to_honey_sql_json(),
            fixture.expected,
            "fixture {} derived from {}",
            fixture.name,
            fixture.source
        );
    }
}
