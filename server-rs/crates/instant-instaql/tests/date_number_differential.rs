use instant_datalog::{FunctionClause, NormalizedClause, Term};
use instant_instaql::{AttributeCatalog, compile_roots, parse_date_millis};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const DATE_ATTR_ID: Uuid = Uuid::from_u128(0x55555555_5555_5555_5555_555555555555);

#[derive(Deserialize)]
struct Fixture {
    catalog: AttributeCatalog,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    expected_values: Vec<Value>,
}

/// Pinned to `attr_pat.clj:coerce-value-data-value!` and
/// `triple.clj:parse-date-value` at upstream 4e0a1bd8. The former accepts every
/// Clojure `number?`; the latter invokes `Instant/ofEpochMilli(long)`, whose
/// primitive conversion truncates fractions toward zero and accepts the full
/// signed-long epoch range. Its `ZonedDateTime` parser also accepts ISO text
/// through `LocalDate`'s signed 999,999,999-year boundary and gives an explicit
/// offset precedence over its bracketed region ID.
#[test]
fn fractional_date_filters_match_the_clojure_numeric_coercion_boundary() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/date_number_differential.json")).unwrap();

    for case in fixture.cases {
        let roots = compile_roots(&case.query, &fixture.catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let mut values = Vec::new();
        collect_date_values(&roots[0].clauses, &mut values);
        assert_eq!(values, case.expected_values, "{}", case.name);
    }

    // The same decoder feeds the optional realtime entity predicate, so the
    // compile and invalidation paths cannot disagree about a fractional date.
    assert_eq!(parse_date_millis(&serde_json::json!(42.75), 0), Some(42));
    assert_eq!(parse_date_millis(&serde_json::json!(-42.75), 0), Some(-42));
}

fn collect_date_values(clauses: &[NormalizedClause], output: &mut Vec<Value>) {
    for clause in clauses {
        match clause {
            NormalizedClause::Pattern { pattern } => {
                let Term::Constants(attributes) = &pattern.attribute else {
                    continue;
                };
                if !attributes
                    .iter()
                    .any(|value| value.as_str() == Some(&DATE_ATTR_ID.to_string()))
                {
                    continue;
                }
                match &pattern.value {
                    Term::Constants(values) => output.extend(values.iter().cloned()),
                    Term::Function(FunctionClause::Not(value)) => output.push(value.clone()),
                    Term::Function(FunctionClause::Comparator(comparator)) => {
                        output.push(comparator.value.clone());
                    }
                    Term::Any
                    | Term::Variable(_)
                    | Term::Function(
                        FunctionClause::IsNull(_) | FunctionClause::EntityIdStartsWith(_),
                    ) => {}
                }
            }
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                collect_date_values(clauses, output);
            }
        }
    }
}
