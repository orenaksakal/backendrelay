use std::collections::HashMap;

use instant_datalog::{
    BindingTree, MatchCteContext, MatchCteOptions, QueryHint, RawPattern, Symbol, build_match_cte,
};
use serde::Deserialize;
use uuid::Uuid;

const APP_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

#[derive(Debug, Deserialize)]
struct CostFixture {
    name: String,
    source: String,
    pattern: RawPattern,
    #[serde(default)]
    options: MatchCteOptions,
    #[serde(default)]
    symbol_cardinalities: HashMap<Symbol, u64>,
    expected_index: String,
}

#[test]
fn index_cost_choices_match_the_clojure_planner_corpus() {
    let fixtures: Vec<CostFixture> =
        serde_json::from_str(include_str!("fixtures/cost_planning.json")).unwrap();
    let app_id = Uuid::parse_str(APP_ID).unwrap();

    for fixture in fixtures {
        let pattern = fixture.pattern.normalize().unwrap();
        let plan = build_match_cte(
            &MatchCteContext {
                prefix: "match-",
                app_id,
                pattern_index: 0,
                start_of_group: true,
                symbol_map: &HashMap::<Symbol, Vec<BindingTree>>::new(),
                symbol_cardinalities: &fixture.symbol_cardinalities,
                additional_joins: &HashMap::new(),
                options: &fixture.options,
                row_estimate: None,
                suppress_row_hint: false,
            },
            &pattern,
        )
        .unwrap();
        let selected = plan.hints.iter().find_map(|hint| match hint {
            QueryHint::IndexScan { index, .. } | QueryHint::BitmapScan { index, .. } => {
                Some(index.as_str())
            }
            QueryHint::Rows { .. } | QueryHint::DisableJit => None,
        });

        assert_eq!(
            selected,
            Some(fixture.expected_index.as_str()),
            "fixture {} derived from {}",
            fixture.name,
            fixture.source
        );
    }
}
