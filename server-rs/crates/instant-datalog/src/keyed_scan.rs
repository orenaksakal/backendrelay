use uuid::Uuid;

use crate::{
    FromTable, PredicateOperator, SelectExpression, SelectSource, SelectStatement,
    StatementProjection, WhereColumn, WhereExpression, WherePredicate,
};

/// Parameterizes a triple scan by a required entity equality to an outer table.
///
/// The original projection, predicate, multiplicity and page controls remain
/// untouched. A lateral subquery exposes the complete app/entity key before any
/// other filter; OFFSET 0 prevents pull-up without limiting or skipping rows.
/// Only an equality required by the full conjunction is eligible, never one
/// inside OR/NOT/EXISTS or a self-reference. Callers own small-page admission.
/// Returns false without mutation when no supported binding exists.
pub fn parameterize_keyed_entity_scan(statement: &mut SelectStatement, app_id: Uuid) -> bool {
    let Some(SelectSource::Table(triples)) = statement.from.first() else {
        return false;
    };
    let Some(alias) = triples
        .alias
        .as_deref()
        .filter(|_| triples.table == "triples")
    else {
        return false;
    };
    let outer_tables = statement.from[1..]
        .iter()
        .map(|source| match source {
            SelectSource::Table(table) => Some(table.alias.as_ref().unwrap_or(&table.table)),
            SelectSource::Subquery { .. } | SelectSource::Lateral { .. } => None,
        })
        .collect::<Option<Vec<_>>>();
    let Some(outer_tables) = outer_tables.filter(|tables| !tables.is_empty()) else {
        return false;
    };
    let Some(binding) = statement
        .predicate
        .as_ref()
        .and_then(|predicate| required_entity_binding(predicate, alias, &outer_tables))
    else {
        return false;
    };
    let alias = alias.to_owned();
    let keyed_alias = format!("{alias}-keyed");
    let query = SelectStatement {
        select: vec![StatementProjection {
            expression: SelectExpression::Expression(WhereExpression::Wildcard(None)),
            alias: None,
        }],
        from: vec![SelectSource::Table(FromTable {
            table: "triples".to_owned(),
            alias: Some(keyed_alias.clone()),
        })],
        predicate: Some(WherePredicate::And(vec![
            WherePredicate::equal(
                WhereExpression::qualified_column(&keyed_alias, WhereColumn::AppId),
                WhereExpression::Uuid(app_id),
            ),
            WherePredicate::equal(
                WhereExpression::qualified_column(&keyed_alias, WhereColumn::EntityId),
                binding.clone(),
            ),
        ])),
        offset: Some(0),
        ..SelectStatement::default()
    };
    statement.from.remove(0);
    statement.from.push(SelectSource::Lateral {
        query: Box::new(query),
        alias,
    });
    true
}

fn required_entity_binding<'a>(
    predicate: &'a WherePredicate,
    alias: &str,
    outer_tables: &[&String],
) -> Option<&'a WhereExpression> {
    fn entity(expression: &WhereExpression, alias: &str) -> bool {
        matches!(expression, WhereExpression::Column(column)
            if column.column == WhereColumn::EntityId
                && column.qualifier.as_deref().is_none_or(|qualifier| qualifier == alias))
    }
    fn outer(expression: &WhereExpression, tables: &[&String]) -> bool {
        match expression {
            WhereExpression::NamedColumn(name) => {
                (name.ends_with("-entity-id") || name.ends_with("_entity_id"))
                    && tables.iter().any(|table| {
                        name.starts_with(&format!("{table}."))
                            || name.starts_with(&format!("{table}-"))
                            || name.starts_with(&format!("{}_", table.replace('-', "_")))
                    })
            }
            WhereExpression::Column(column) => {
                column.column == WhereColumn::EntityId
                    && column
                        .qualifier
                        .as_ref()
                        .is_some_and(|qualifier| tables.contains(&qualifier))
            }
            _ => false,
        }
    }
    match predicate {
        WherePredicate::And(values) => values
            .iter()
            .find_map(|value| required_entity_binding(value, alias, outer_tables)),
        WherePredicate::Compare {
            operator: PredicateOperator::Equal,
            left,
            right,
        } if entity(left, alias) && outer(right, outer_tables) => Some(right),
        WherePredicate::Compare {
            operator: PredicateOperator::Equal,
            left,
            right,
        } if entity(right, alias) && outer(left, outer_tables) => Some(left),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Materialization, NestedResultKind, NestedResultTable, NestedSqlPlan, PlannedCte, SelectCte,
        render_nested_match_query, render_nested_match_query_with_structural_constants,
    };
    use serde_json::json;

    fn statement() -> SelectStatement {
        SelectStatement {
            select: vec![StatementProjection {
                expression: SelectExpression::Expression(WhereExpression::Wildcard(None)),
                alias: None,
            }],
            from: vec![
                SelectSource::Table(FromTable {
                    table: "triples".into(),
                    alias: Some("t0".into()),
                }),
                SelectSource::Table(FromTable {
                    table: "m-0".into(),
                    alias: None,
                }),
            ],
            predicate: Some(WherePredicate::And(vec![
                WherePredicate::equal(
                    WhereExpression::column(WhereColumn::AppId),
                    WhereExpression::Uuid(Uuid::from_u128(100)),
                ),
                WherePredicate::equal(
                    WhereExpression::column(WhereColumn::EntityId),
                    WhereExpression::NamedColumn("m-0-entity-id".into()),
                ),
                WherePredicate::equal(
                    WhereExpression::column(WhereColumn::Value),
                    WhereExpression::Literal(json!("actor-' λ")),
                ),
            ])),
            limit: Some(11),
            offset: Some(3),
            ..Default::default()
        }
    }

    #[test]
    fn keyed_scan_keeps_all_outer_controls_and_uses_only_an_app_entity_fence() {
        let mut query = statement();
        let before = query.clone();
        assert!(parameterize_keyed_entity_scan(
            &mut query,
            Uuid::from_u128(100)
        ));
        assert_eq!(query.select, before.select);
        assert_eq!(query.predicate, before.predicate);
        assert_eq!((query.limit, query.offset), (Some(11), Some(3)));
        assert_eq!(query.from[0], before.from[1]);
        let SelectSource::Lateral {
            query: inner,
            alias,
        } = &query.from[1]
        else {
            panic!("missing keyed source")
        };
        assert_eq!(alias, "t0");
        assert_eq!(inner.offset, Some(0));
        assert_eq!(inner.limit, None);
        let plan = NestedSqlPlan {
            ctes: vec![PlannedCte::Select(SelectCte {
                name: "result".into(),
                statement: query,
                materialization: Materialization::Materialized,
            })],
            hints: Vec::new(),
            result_tables: vec![NestedResultTable {
                table: "result".into(),
                kind: NestedResultKind::Rows,
            }],
        };
        for rendered in [
            render_nested_match_query(&plan).unwrap(),
            render_nested_match_query_with_structural_constants(&plan).unwrap(),
        ] {
            assert!(
                rendered
                    .sql
                    .contains("LATERAL (SELECT * FROM \"triples\" AS \"t0_keyed\"")
            );
            assert!(rendered.sql.contains(" OFFSET 0) AS \"t0\""));
            assert!(rendered.sql.contains(" LIMIT 11 OFFSET 3"));
            assert!(!rendered.sql.contains("actor-'"));
            assert!(
                rendered
                    .parameters
                    .iter()
                    .any(|p| matches!(p,crate::SqlParameter::Json(v) if *v==json!("actor-' λ")))
            );
        }
    }

    #[test]
    fn disjunctive_or_constant_or_self_bindings_do_not_parameterize_a_scan() {
        for predicate in [
            WherePredicate::Or(vec![WherePredicate::equal(
                WhereExpression::column(WhereColumn::EntityId),
                WhereExpression::NamedColumn("m-0-entity-id".into()),
            )]),
            WherePredicate::equal(
                WhereExpression::column(WhereColumn::EntityId),
                WhereExpression::Uuid(Uuid::from_u128(1)),
            ),
            WherePredicate::equal(
                WhereExpression::column(WhereColumn::EntityId),
                WhereExpression::NamedColumn("t0-entity-id".into()),
            ),
        ] {
            let mut query = statement();
            query.predicate = Some(predicate);
            let before = query.clone();
            assert!(!parameterize_keyed_entity_scan(&mut query, Uuid::nil()));
            assert_eq!(query, before);
        }
    }

    #[test]
    fn required_equality_can_be_reversed_and_nested_under_and_but_not_under_or() {
        let mut query = statement();
        let required = WherePredicate::equal(
            WhereExpression::NamedColumn("m-0.bound-entity-id".into()),
            WhereExpression::qualified_column("t0", WhereColumn::EntityId),
        );
        query.predicate = Some(WherePredicate::And(vec![
            WherePredicate::Or(vec![required.clone()]),
            WherePredicate::And(vec![required]),
        ]));
        assert!(parameterize_keyed_entity_scan(&mut query, Uuid::nil()));
        let SelectSource::Lateral { query, .. } = &query.from[1] else {
            unreachable!()
        };
        assert!(matches!(&query.predicate,Some(WherePredicate::And(values)) if values.len()==2));
    }

    #[test]
    fn unsupported_or_unjoined_sources_are_unchanged() {
        for remove_outer in [true, false] {
            let mut query = statement();
            if remove_outer {
                query.from.pop();
            } else {
                query.from[0] = SelectSource::Subquery {
                    query: Box::new(SelectStatement::default()),
                    alias: "t0".into(),
                };
            }
            let before = query.clone();
            assert!(!parameterize_keyed_entity_scan(&mut query, Uuid::nil()));
            assert_eq!(query, before);
        }
    }
}
