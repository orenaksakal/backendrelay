use instant_instaql::{
    AttributeCatalog, InstaqlError, ValidationError, WhereCondition, WhereValue, coerce_forms,
};
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
    error: Option<ValidationError>,
    where_value: Option<Value>,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

#[test]
fn generic_post_coercion_spec_errors_match_pinned_predicates_and_paths() {
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/generic_spec_validation_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        if let Some(expected) = case.error {
            let error = coerce_forms(&case.query, &catalog()).expect_err(&case.name);
            let InstaqlError::Validation(errors) = error else {
                panic!("{}: expected validation error, got {error}", case.name);
            };
            assert_eq!(errors, [expected], "{}", case.name);
        } else {
            let forms = coerce_forms(&case.query, &catalog()).unwrap_or_else(|error| {
                panic!(
                    "{}: expected the pinned spec to accept the query: {error}",
                    case.name
                )
            });
            if let Some(expected) = case.where_value {
                let WhereCondition::Condition {
                    value: WhereValue::Scalar(actual),
                    ..
                } = &forms[0].options.where_clause[0]
                else {
                    panic!("{}: expected a scalar equality condition", case.name);
                };
                assert_eq!(*actual, expected, "{}", case.name);
            }
        }
    }
}
