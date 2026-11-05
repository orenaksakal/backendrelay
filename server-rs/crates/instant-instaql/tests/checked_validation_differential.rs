use instant_instaql::{AttributeCatalog, InstaqlError, compile_roots};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    catalog: AttributeCatalog,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    expected: String,
    path: Vec<String>,
    message: String,
}

#[test]
fn checked_value_errors_match_the_authoritative_clojure_corpus() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "fixtures/checked_validation_differential.json"
    ))
    .unwrap();

    for case in fixture.cases {
        let InstaqlError::Validation(errors) =
            compile_roots(&case.query, &fixture.catalog, false).unwrap_err()
        else {
            panic!("{}: expected a validation error", case.name);
        };
        let error = errors
            .first()
            .unwrap_or_else(|| panic!("{}: empty validation error", case.name));
        assert_eq!(error.expected, case.expected, "{}", case.name);
        assert_eq!(error.path, case.path, "{}", case.name);
        assert_eq!(error.message, case.message, "{}", case.name);
    }
}
