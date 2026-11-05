use instant_datalog::{
    Component, Index, PredicateOptions, QueryHint, Term, constant_predicate, function_predicates,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const APP_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

#[derive(Debug, Deserialize)]
struct PredicateFixtures {
    constant_cases: Vec<ConstantFixture>,
    function_cases: Vec<FunctionFixture>,
}

#[derive(Debug, Deserialize)]
struct ConstantFixture {
    name: String,
    source: String,
    index: Index,
    component: Component,
    values: Vec<Value>,
    expected: Value,
    expected_hint: Option<ExpectedHint>,
}

#[derive(Debug, Deserialize)]
struct ExpectedHint {
    table_alias: String,
    index: String,
}

#[derive(Debug, Deserialize)]
struct FunctionFixture {
    name: String,
    source: String,
    index: Index,
    entity: Term,
    value: Term,
    #[serde(default)]
    options: PredicateOptions,
    expected: Vec<Value>,
}

fn fixtures() -> PredicateFixtures {
    serde_json::from_str(include_str!("fixtures/predicate_planning.json")).unwrap()
}

#[test]
fn constants_match_the_clojure_predicate_contract() {
    let app_id = Uuid::parse_str(APP_ID).unwrap();
    for fixture in fixtures().constant_cases {
        let plan = constant_predicate(
            "t0",
            fixture.index,
            app_id,
            fixture.component,
            &fixture.values,
        )
        .unwrap();
        assert_eq!(
            plan.predicate.to_honey_sql_json(),
            fixture.expected,
            "fixture `{}` derived from {}",
            fixture.name,
            fixture.source
        );

        match fixture.expected_hint {
            Some(expected) => assert_eq!(
                plan.hints,
                vec![QueryHint::IndexScan {
                    table_alias: expected.table_alias,
                    index: expected.index,
                }],
                "fixture `{}` derived from {}",
                fixture.name,
                fixture.source
            ),
            None => assert!(plan.hints.is_empty()),
        }
    }
}

#[test]
fn functions_match_the_clojure_predicate_contract() {
    let app_id = Uuid::parse_str(APP_ID).unwrap();
    for fixture in fixtures().function_cases {
        let actual: Vec<_> = function_predicates(
            fixture.options,
            app_id,
            "t0",
            fixture.index,
            &fixture.entity,
            &fixture.value,
        )
        .unwrap()
        .into_iter()
        .map(|predicate| predicate.to_honey_sql_json())
        .collect();
        assert_eq!(
            actual, fixture.expected,
            "fixture `{}` derived from {}",
            fixture.name, fixture.source
        );
    }
}
