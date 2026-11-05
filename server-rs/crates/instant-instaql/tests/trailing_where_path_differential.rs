use instant_instaql::{AttributeCatalog, compile_query};
use serde::Deserialize;
use serde_json::Value;

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
    query: Value,
    canonical: Option<Value>,
    missing: bool,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

/// Clojure's default `string/split` limit discards every trailing empty
/// segment. This affects the public query graph before attribute resolution,
/// including pseudo-fields and the null branches synthesized for `$not`.
#[test]
fn dotted_where_paths_match_clojure_trailing_empty_segment_semantics() {
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/trailing_where_path_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));
    let catalog = catalog();

    for case in fixture.cases {
        let compiled = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_eq!(compiled.roots[0].missing, case.missing, "{}", case.name);

        if let Some(canonical) = case.canonical {
            let expected = compile_query(&canonical, &catalog, false)
                .unwrap_or_else(|error| panic!("{} canonical: {error}", case.name));
            assert_eq!(
                compiled.roots[0].selector_clauses, expected.roots[0].selector_clauses,
                "{}",
                case.name
            );
        }
    }
}
