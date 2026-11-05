use instant_datalog::{
    DataType, FunctionClause, Index, IndexKey, IsNullClause, PredicateOptions, SelectQuery, Term,
    WhereExpression, WherePredicate, function_predicates, render_select_query,
};
use serde_json::json;
use uuid::Uuid;

fn predicate(options: PredicateOptions, clause: IsNullClause) -> WherePredicate {
    let mut predicates = function_predicates(
        options,
        Uuid::from_u128(1),
        "t0",
        Index::new(IndexKey::Ea),
        &Term::Any,
        &Term::Function(FunctionClause::IsNull(clause)),
    )
    .unwrap();
    assert_eq!(predicates.len(), 1);
    predicates.remove(0)
}

fn clause(nil: bool) -> IsNullClause {
    IsNullClause {
        attr_id: Uuid::from_u128(2),
        nil,
        reference: false,
        reverse: false,
        indexed: false,
        indexed_checked_type: None,
    }
}

#[test]
fn forward_null_exists_preserves_the_correlated_subquery_and_parameters() {
    for nil in [false, true] {
        for reference in [false, true] {
            for checked in [
                None,
                Some(DataType::Number),
                Some(DataType::Boolean),
                Some(DataType::Date),
                Some(DataType::String),
            ] {
                let clause = IsNullClause {
                    reference,
                    indexed: checked.is_some(),
                    indexed_checked_type: checked,
                    ..clause(nil)
                };
                let original = predicate(PredicateOptions::default(), clause.clone());
                let candidate = predicate(
                    PredicateOptions {
                        forward_is_null_exists: true,
                        ..PredicateOptions::default()
                    },
                    clause,
                );
                let WherePredicate::In { negated, query, .. } = original else {
                    panic!("reference must use IN")
                };
                assert_eq!(
                    candidate,
                    WherePredicate::Exists {
                        negated,
                        query: query.clone()
                    }
                );
                // The existing correlation is retained: EXISTS changes only
                // the membership operator, not app/attr/value/index/type guards.
                let statement = |predicate| SelectQuery {
                    select: vec![WhereExpression::Inline("1".into())],
                    table: "triples".into(),
                    alias: "t0".into(),
                    predicate: Box::new(predicate),
                };
                let legacy = render_select_query(&statement(WherePredicate::In {
                    negated,
                    expression: WhereExpression::column(instant_datalog::WhereColumn::EntityId),
                    query,
                }))
                .unwrap();
                let optimized = render_select_query(&statement(candidate)).unwrap();
                let expected = legacy.sql.replace(
                    if nil {
                        "(\"entity_id\" NOT IN ("
                    } else {
                        "(\"entity_id\" IN ("
                    },
                    if nil { "(NOT EXISTS (" } else { "(EXISTS (" },
                );
                assert_ne!(expected, legacy.sql);
                assert_eq!(optimized.sql, expected);
                assert_eq!(optimized.parameters, legacy.parameters);
            }
        }
    }
}

#[test]
fn reverse_reference_and_legacy_fix_rollback_keep_the_original_predicate() {
    for nil in [false, true] {
        for disable_is_null_fix in [false, true] {
            for reverse in [false, true] {
                if !disable_is_null_fix && !reverse {
                    continue;
                }
                let clause = IsNullClause {
                    reference: true,
                    reverse,
                    ..clause(nil)
                };
                let original = PredicateOptions {
                    disable_is_null_fix,
                    ..PredicateOptions::default()
                };
                assert_eq!(
                    predicate(original, clause.clone()),
                    predicate(
                        PredicateOptions {
                            forward_is_null_exists: true,
                            ..original
                        },
                        clause
                    )
                );
            }
        }
    }
}

#[test]
fn older_predicate_options_deserialize_without_changing_reference_defaults() {
    assert_eq!(
        serde_json::from_value::<PredicateOptions>(json!({"disable_is_null_fix":false})).unwrap(),
        PredicateOptions::default()
    );
}
