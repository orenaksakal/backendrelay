use instant_datalog::{ComparatorOperation, FunctionClause, NormalizedClause, Term};
use instant_instaql::{
    AttributeCatalog, InstaqlError, WhereComparator, WhereCondition, WhereValue, coerce_forms,
    compile_query,
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
    expected_kind: Option<String>,
    expected_value: Option<Value>,
    missing_inclusive: Option<bool>,
    expected_error: Option<String>,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

#[test]
fn multi_operator_maps_match_clojure_normalization_validation_and_precedence() {
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/multi_operator_where_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));
    let catalog = catalog();

    for case in fixture.cases {
        let result = coerce_forms(&case.query, &catalog);
        if let Some(expected_error) = case.expected_error {
            let InstaqlError::Validation(errors) = result.unwrap_err() else {
                panic!("{}: expected validation error", case.name);
            };
            assert_eq!(errors[0].expected, expected_error, "{}", case.name);
            continue;
        }

        let forms = result.unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let (value, missing_inclusive) =
            selected_value(&forms[0].options.where_clause[0], &case.name);
        assert_eq!(
            Some(missing_inclusive),
            case.missing_inclusive,
            "{}",
            case.name
        );
        assert_eq!(
            value_kind(value),
            case.expected_kind.as_deref(),
            "{}",
            case.name
        );
        assert_eq!(value_argument(value), case.expected_value, "{}", case.name);

        let compiled = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{} compile: {error}", case.name));
        assert!(
            contains_selected_operation(&compiled.roots[0].selector_clauses, value),
            "{}: selected operator did not reach the Datalog graph",
            case.name
        );
    }
}

fn selected_value<'a>(condition: &'a WhereCondition, name: &str) -> (&'a WhereValue, bool) {
    match condition {
        WhereCondition::Condition { value, .. } => (value, false),
        WhereCondition::Or { conditions } => {
            let Some(WhereCondition::Condition { value, .. }) = conditions.first() else {
                panic!("{name}: expected selected condition as the first OR branch");
            };
            assert!(
                conditions[1..].iter().all(|condition| matches!(
                    condition,
                    WhereCondition::Condition {
                        value: WhereValue::IsNull(true),
                        ..
                    }
                )),
                "{name}: missing envelope must contain only null-path branches"
            );
            (value, conditions.len() > 1)
        }
        WhereCondition::And { .. } => panic!("{name}: expected one normalized condition"),
    }
}

fn value_kind(value: &WhereValue) -> Option<&'static str> {
    match value {
        WhereValue::In(_) => Some("in"),
        WhereValue::Not(_) => Some("not"),
        WhereValue::IsNull(_) => Some("is-null"),
        WhereValue::Comparator { operation, .. } => Some(match operation {
            WhereComparator::GreaterThan => "gt",
            WhereComparator::GreaterThanOrEqual => "gte",
            WhereComparator::LessThan => "lt",
            WhereComparator::LessThanOrEqual => "lte",
            WhereComparator::Like => "like",
            WhereComparator::CaseInsensitiveLike => "ilike",
        }),
        WhereValue::Scalar(_) | WhereValue::EntityIdStartsWith(_) => None,
    }
}

fn value_argument(value: &WhereValue) -> Option<Value> {
    match value {
        WhereValue::In(values) => values.first().cloned(),
        WhereValue::Not(value) | WhereValue::Comparator { value, .. } => Some(value.clone()),
        WhereValue::IsNull(value) => Some(Value::Bool(*value)),
        WhereValue::Scalar(_) | WhereValue::EntityIdStartsWith(_) => None,
    }
}

fn contains_selected_operation(clauses: &[NormalizedClause], selected: &WhereValue) -> bool {
    clauses.iter().any(|clause| match clause {
        NormalizedClause::Pattern { pattern } => match selected {
            WhereValue::In(_) => matches!(pattern.value, Term::Constants(_)),
            WhereValue::Not(_) => {
                matches!(pattern.value, Term::Function(FunctionClause::Not(_)))
            }
            WhereValue::IsNull(_) => {
                matches!(pattern.value, Term::Function(FunctionClause::IsNull(_)))
            }
            WhereValue::Comparator { operation, .. } => {
                let expected = ComparatorOperation::from(*operation);
                matches!(
                    &pattern.value,
                    Term::Function(FunctionClause::Comparator(comparator))
                        if comparator.operation == expected
                )
            }
            WhereValue::Scalar(_) | WhereValue::EntityIdStartsWith(_) => false,
        },
        NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
            contains_selected_operation(clauses, selected)
        }
    })
}
