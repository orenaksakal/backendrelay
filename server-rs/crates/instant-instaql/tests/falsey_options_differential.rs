use instant_instaql::{AttributeCatalog, compile_query};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    options: Map<String, Value>,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

#[test]
fn falsey_public_options_match_clojure_when_let_coercion() {
    let catalog = catalog();
    let fixture =
        serde_json::from_str::<Fixture>(include_str!("fixtures/falsey_options_differential.json"))
            .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    let baseline = compile_query(&json!({"users": {}}), &catalog, false).unwrap();
    for case in fixture.cases {
        let query = json!({"users": {"$": case.options}});
        let compiled = compile_query(&query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_eq!(compiled, baseline, "{}", case.name);
    }
}
