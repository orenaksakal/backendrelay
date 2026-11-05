use instant_datalog::{
    BindingTree, NestedSymbolMap, RawPattern, Symbol, merge_or_symbol_maps, nested_join_predicates,
    or_gather_predicate,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct OrFixtures {
    nested_join: NestedJoinFixture,
    gather_join: GatherJoinFixture,
}

#[derive(Debug, Deserialize)]
struct NestedJoinFixture {
    name: String,
    source: String,
    join_symbol: Symbol,
    pattern_index: usize,
    pattern: RawPattern,
    branches: Vec<Vec<BindingTree>>,
    expected: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct GatherJoinFixture {
    name: String,
    source: String,
    join_symbol: Symbol,
    destination: Vec<BindingTree>,
    branches: Vec<Vec<BindingTree>>,
    expected: Value,
}

fn symbol_map(symbol: &Symbol, bindings: Vec<BindingTree>) -> NestedSymbolMap {
    [(symbol.clone(), bindings)].into_iter().collect()
}

#[test]
fn nested_or_joins_match_the_clojure_path_algebra() {
    let fixtures: OrFixtures =
        serde_json::from_str(include_str!("fixtures/or_joins.json")).unwrap();
    let fixture = fixtures.nested_join;
    let branch_maps: Vec<_> = fixture
        .branches
        .into_iter()
        .map(|bindings| symbol_map(&fixture.join_symbol, bindings))
        .collect();
    let merged = merge_or_symbol_maps(&branch_maps);
    let pattern = fixture.pattern.normalize().unwrap();
    let actual: Vec<_> = nested_join_predicates("match-", fixture.pattern_index, &merged, &pattern)
        .into_iter()
        .map(|predicate| predicate.to_honey_sql_json())
        .collect();

    assert_eq!(
        actual, fixture.expected,
        "fixture `{}` derived from {}",
        fixture.name, fixture.source
    );
}

#[test]
fn gather_joins_match_the_clojure_path_algebra() {
    let fixtures: OrFixtures =
        serde_json::from_str(include_str!("fixtures/or_joins.json")).unwrap();
    let fixture = fixtures.gather_join;
    let destination = symbol_map(&fixture.join_symbol, fixture.destination);
    let branch_maps: Vec<_> = fixture
        .branches
        .into_iter()
        .map(|bindings| symbol_map(&fixture.join_symbol, bindings))
        .collect();
    let actual = or_gather_predicate("match-", &destination, &branch_maps, &fixture.join_symbol)
        .unwrap()
        .to_honey_sql_json();

    assert_eq!(
        actual, fixture.expected,
        "fixture `{}` derived from {}",
        fixture.name, fixture.source
    );
}

#[test]
fn gather_returns_none_without_a_shared_join_symbol() {
    let symbol = Symbol::new("?missing").unwrap();
    assert!(or_gather_predicate("match-", &NestedSymbolMap::new(), &[], &symbol).is_none());
}
