use instant_datalog::DataType;
use instant_instaql::{
    AttributeCatalog, Cardinality, InstaqlError, ValidationError, compile_query, compile_roots,
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
    admin: bool,
    query: Value,
    age_lifecycle: Option<Lifecycle>,
    errors: Option<Vec<ValidationError>>,
    roots_errors: Option<Vec<ValidationError>>,
    query_errors: Option<Vec<ValidationError>>,
}

#[derive(Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors independent persisted lifecycle flags.
struct Lifecycle {
    checking_data_type: bool,
    indexing: bool,
    indexed: bool,
    typed: bool,
    cardinality: Cardinality,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

#[test]
fn order_and_aggregate_validation_matches_pinned_clojure_error_accumulation() {
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/order_aggregate_validation_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let mut catalog = catalog();
        if let Some(lifecycle) = case.age_lifecycle {
            let age = catalog
                .attributes
                .iter_mut()
                .find(|attribute| attribute.etype == "users" && attribute.label == "age")
                .unwrap();
            age.checking_data_type = lifecycle.checking_data_type;
            age.indexing = lifecycle.indexing;
            age.indexed = lifecycle.indexed;
            age.checked_data_type = lifecycle.typed.then_some(DataType::Number);
            age.cardinality = lifecycle.cardinality;
        }

        let expected_roots = case
            .roots_errors
            .as_ref()
            .or(case.errors.as_ref())
            .expect("fixture supplies root errors");
        let roots = validation_errors(compile_roots(&case.query, &catalog, case.admin), &case.name);
        assert_eq!(&roots, expected_roots, "{} compile_roots", case.name);

        let expected_query = case
            .query_errors
            .as_ref()
            .or(case.errors.as_ref())
            .expect("fixture supplies query errors");
        let query = validation_errors(compile_query(&case.query, &catalog, case.admin), &case.name);
        assert_eq!(&query, expected_query, "{} compile_query", case.name);
    }
}

fn validation_errors<T>(result: Result<T, InstaqlError>, name: &str) -> Vec<ValidationError> {
    let Err(error) = result else {
        panic!("{name}: expected validation errors");
    };
    let InstaqlError::Validation(errors) = error else {
        panic!("{name}: expected validation errors, got {error}");
    };
    errors
}
