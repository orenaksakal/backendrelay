//! Ordered first-page candidate windows for broad, single-entity blob filters.
//!
//! Walk the typed order index and probe every complete filter before applying
//! the lookahead limit. Missing/JSON-null order values are preserved through a
//! separate null tail, evaluated only if the non-null prefix is insufficient.
//! Correlated EXISTS fences prevent whole-partition semi joins ahead of LIMIT;
//! bounded selector/order relations are materialized once for the outer page.
use instant_datalog::{
    CteJoin, DataType, FromTable, FunctionClause, IndexKey, MatchCteOptions, MatchQueryPlan,
    Materialization, NormalizedClause, NullOrder, OrGatherCte, OrderBy, OrderDirection, PageInfo,
    PageOrderType, Pattern, PatternError, PatternWhereOptions, PgType, PlannedCte,
    PredicateOperator, QueryHint, SelectCte, SelectExpression, SelectProjection, SelectQuery,
    SelectSource, SelectStatement, SqlFunction, StatementProjection, Term, WhereColumn,
    WhereExpression, WherePredicate, pattern_where_plan,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::candidate_entities::{pattern_lookup, required_equalities, seed_rows};

pub(super) fn bound_ordered_page(
    plan: &mut MatchQueryPlan,
    clauses: &[NormalizedClause],
    page: Option<&PageInfo>,
    app_id: Uuid,
    options: &MatchCteOptions,
    root: bool,
) -> Result<bool, PatternError> {
    let Some(page) = page else {
        return Ok(false);
    };
    let Some(limit) = page.limit.filter(|limit| (1..=100).contains(limit)) else {
        return Ok(false);
    };
    if !root
        || options.candidate_entity_scans != Some(true)
        || page.order_type != PageOrderType::Checked(DataType::Number)
        || page.direction != OrderDirection::Desc
        || page.last
        || page.offset.is_some()
        || page.before.is_some()
        || page.after.is_some()
        || !compatible(clauses, &page.pattern.entity)
    {
        return Ok(false);
    }
    let mut seeds = Vec::new();
    required_equalities(clauses, &mut seeds);
    seeds.sort_by_key(|seed| seed_rows(seed, options).unwrap_or(u64::MAX));
    let Some(seed) = seeds
        .first()
        .filter(|seed| seed_rows(seed, options).is_some_and(|rows| rows > 1024))
    else {
        return Ok(false);
    };
    let count = limit + 1;
    let prefix = format!("{}-ordered", plan.final_table);
    let nonnull = format!("{prefix}-nonnull");
    let enough = format!("{prefix}-enough");
    let nulls = format!("{prefix}-nulls");
    let order_alias = format!("{prefix}-order");
    let seed_alias = format!("{prefix}-seed");
    let value = WhereExpression::function(
        SqlFunction::ExtractNumberValue,
        WhereExpression::qualified_column(&order_alias, WhereColumn::Value),
    );
    let order_entity = WhereExpression::qualified_column(&order_alias, WhereColumn::EntityId);
    let seed_entity = WhereExpression::qualified_column(&seed_alias, WhereColumn::EntityId);
    let mut order_bounds = vec![order_predicate(
        &page.pattern,
        &order_alias,
        app_id,
        options,
    )?];
    required_order_bounds(
        clauses,
        &page.pattern,
        &order_alias,
        app_id,
        options,
        &mut order_bounds,
    )?;
    order_bounds.push(compare(
        PredicateOperator::NotEqual,
        value.clone(),
        WhereExpression::Literal(Value::Null),
    ));
    order_bounds.push(filters(
        clauses,
        &order_entity,
        &format!("{prefix}-filter"),
        app_id,
        options,
        true,
    )?);
    let dense = SelectStatement {
        select: vec![projection(order_entity.clone(), "bound-entity-id")],
        from: vec![source("triples", Some(&order_alias))],
        predicate: Some(WherePredicate::And(order_bounds)),
        order_by: vec![
            OrderBy {
                expression: value,
                direction: OrderDirection::Desc,
                nulls: Some(NullOrder::Last),
            },
            OrderBy {
                expression: order_entity,
                direction: OrderDirection::Desc,
                nulls: None,
            },
        ],
        limit: Some(count),
        ..Default::default()
    };
    let nonnull_order_exists = order_exists(
        &page.pattern,
        &seed_entity,
        &format!("{prefix}-null-check"),
        app_id,
        options,
        true,
    )?;
    let mut null_predicates = vec![
        pattern_where_plan(
            PatternWhereOptions {
                remove_unnecessary_index: false,
                predicate_options: options.predicate_options,
            },
            app_id,
            &seed_alias,
            seed,
            Vec::new(),
        )?
        .predicate,
        // This uncorrelated check becomes a one-time filter: do not scan a broad
        // room for missing timestamps when the indexed prefix fills the page.
        WherePredicate::Exists {
            negated: true,
            query: SelectQuery {
                select: vec![WhereExpression::NamedColumn("1".into())],
                table: enough.clone(),
                alias: format!("{prefix}-full"),
                predicate: Box::new(truth()),
            },
        },
        WherePredicate::Exists {
            negated: true,
            query: nonnull_order_exists,
        },
        filters(
            clauses,
            &seed_entity,
            &format!("{prefix}-null-filter"),
            app_id,
            options,
            false,
        )?,
    ];
    if page.order_required {
        null_predicates.push(WherePredicate::Exists {
            negated: false,
            query: order_exists(
                &page.pattern,
                &seed_entity,
                &format!("{prefix}-required"),
                app_id,
                options,
                false,
            )?,
        });
    }
    let null_tail = SelectStatement {
        distinct_on: vec![seed_entity.clone()],
        select: vec![projection(seed_entity.clone(), "bound-entity-id")],
        from: vec![source("triples", Some(&seed_alias))],
        predicate: Some(WherePredicate::And(null_predicates)),
        order_by: vec![OrderBy {
            expression: seed_entity,
            direction: OrderDirection::Desc,
            nulls: None,
        }],
        limit: Some(count),
        ..Default::default()
    };
    let candidates = PlannedCte::OrGather(OrGatherCte {
        name: prefix.clone(),
        select: vec![SelectProjection {
            expression: WhereExpression::Coalesce(vec![
                WhereExpression::NamedColumn(format!("{nonnull}.bound-entity-id")),
                WhereExpression::NamedColumn(format!("{nulls}.bound-entity-id")),
            ]),
            alias: Some("bound-entity-id".into()),
        }],
        from: FromTable {
            table: nonnull.clone(),
            alias: None,
        },
        full_joins: vec![CteJoin {
            table: nulls.clone(),
            predicate: Some(WherePredicate::Truthy(WhereExpression::Literal(json!(
                false
            )))),
        }],
        left_joins: Vec::new(),
        predicate: None,
        materialization: Materialization::Materialized,
    });
    let metadata = pattern_lookup(&plan.pattern_metadata);
    for cte in &mut plan.ctes {
        let PlannedCte::Match(cte) = cte else {
            continue;
        };
        let Some(pattern) = metadata.get(&format!("{}_entity_id", cte.name.replace('-', "_")))
        else {
            continue;
        };
        if pattern.entity != page.pattern.entity {
            continue;
        }
        let Some(target) = cte
            .from
            .first()
            .filter(|from| from.table == "triples")
            .and_then(|from| from.alias.clone())
        else {
            continue;
        };
        cte.from.push(FromTable {
            table: prefix.clone(),
            alias: None,
        });
        cte.predicate = WherePredicate::And(vec![
            cte.predicate.clone(),
            compare(
                PredicateOperator::Equal,
                WhereExpression::qualified_column(&target, WhereColumn::EntityId),
                WhereExpression::NamedColumn(format!("{prefix}.bound-entity-id")),
            ),
        ]);
        // The optional-order relation can otherwise inline across the outer
        // join and probe every candidate once per selected row (N squared).
        // The complete candidate-bound relation is small; materialize it once.
        cte.materialization = Materialization::Materialized;
        plan.hints.retain(|hint| match hint {
            QueryHint::IndexScan { table_alias, .. }
            | QueryHint::BitmapScan { table_alias, .. } => table_alias != &target,
            QueryHint::Rows { tables, .. } => !tables.contains(&target),
            QueryHint::DisableJit => true,
        });
        if options.pg_hints_enabled != Some(false) {
            // These scans now have at most limit+1 entity bindings. Re-costing
            // a broad AV equality can still choose a whole-room hash build;
            // the primary key supports app/entity/attribute-bound probes.
            plan.hints.push(QueryHint::IndexScan {
                table_alias: target,
                index: "triples_pkey".into(),
            });
        }
    }
    plan.ctes.splice(
        0..0,
        [
            select(nonnull.clone(), dense),
            select(
                enough,
                SelectStatement {
                    select: vec![StatementProjection {
                        expression: SelectExpression::Expression(WhereExpression::Wildcard(None)),
                        alias: None,
                    }],
                    from: vec![source(&nonnull, None)],
                    limit: Some(1),
                    offset: Some(count - 1),
                    ..Default::default()
                },
            ),
            select(nulls, null_tail),
            candidates,
        ],
    );
    Ok(true)
}

fn compatible(clauses: &[NormalizedClause], entity: &Term) -> bool {
    !clauses.is_empty() && clauses.iter().all(|clause|match clause {
        NormalizedClause::Pattern {pattern}=>pattern.entity==*entity
            && matches!(pattern.index.key,IndexKey::Ea|IndexKey::Av|IndexKey::Ave)
            && matches!(&pattern.attribute,Term::Constants(values) if values.len()==1)
            && !matches!(pattern.value,Term::Variable(_))
            && !matches!(&pattern.value,Term::Function(FunctionClause::IsNull(clause)) if clause.reference)
            && matches!(pattern.created_at,Term::Any),
        NormalizedClause::And {clauses}|NormalizedClause::Or {clauses,..}=>compatible(clauses,entity),
    })
}

// Required comparisons on the cardinality-one order attribute are safe index
// bounds on the driving row itself. Leaving them only in a correlated EXISTS
// makes an older-page query walk every newer timestamp before reaching its key.
// Keep the complete EXISTS filter as well; never push an individual OR arm.
fn required_order_bounds(
    clauses: &[NormalizedClause],
    order: &Pattern,
    alias: &str,
    app_id: Uuid,
    options: &MatchCteOptions,
    predicates: &mut Vec<WherePredicate>,
) -> Result<(), PatternError> {
    for clause in clauses {
        match clause {
            NormalizedClause::And { clauses } => {
                required_order_bounds(clauses, order, alias, app_id, options, predicates)?
            }
            NormalizedClause::Pattern { pattern }
                if pattern.entity == order.entity
                    && pattern.attribute == order.attribute
                    && pattern.index == order.index
                    && matches!(
                        pattern.value,
                        Term::Constants(_) | Term::Function(FunctionClause::Comparator(_))
                    ) =>
            {
                predicates.push(
                    pattern_where_plan(
                        PatternWhereOptions {
                            remove_unnecessary_index: false,
                            predicate_options: options.predicate_options,
                        },
                        app_id,
                        alias,
                        pattern,
                        Vec::new(),
                    )?
                    .predicate,
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn filters(
    clauses: &[NormalizedClause],
    entity: &WhereExpression,
    prefix: &str,
    app_id: Uuid,
    options: &MatchCteOptions,
    correlated_probe: bool,
) -> Result<WherePredicate, PatternError> {
    let mut predicates = Vec::new();
    for (index, clause) in clauses.iter().enumerate() {
        let alias = format!("{prefix}-{index}");
        predicates.push(match clause {
            NormalizedClause::Pattern { pattern } => {
                let query = SelectQuery {
                    select: vec![WhereExpression::NamedColumn("1".into())],
                    table: "triples".into(),
                    alias: alias.clone(),
                    predicate: Box::new(
                        pattern_where_plan(
                            PatternWhereOptions {
                                remove_unnecessary_index: false,
                                predicate_options: options.predicate_options,
                            },
                            app_id,
                            &alias,
                            pattern,
                            vec![compare(
                                PredicateOperator::Equal,
                                WhereExpression::qualified_column(&alias, WhereColumn::EntityId),
                                entity.clone(),
                            )],
                        )?
                        .predicate,
                    ),
                };
                if correlated_probe {
                    WherePredicate::ExistsProbe { query }
                } else {
                    WherePredicate::Exists {
                        negated: false,
                        query,
                    }
                }
            }
            NormalizedClause::And { clauses } => {
                filters(clauses, entity, &alias, app_id, options, correlated_probe)?
            }
            NormalizedClause::Or { clauses, .. } => {
                // Every arm is recursively compiled as its own full conjunction.
                let mut arms = Vec::new();
                for (n, clause) in clauses.iter().enumerate() {
                    arms.push(filters(
                        std::slice::from_ref(clause),
                        entity,
                        &format!("{alias}-{n}"),
                        app_id,
                        options,
                        correlated_probe,
                    )?);
                }
                WherePredicate::Or(arms)
            }
        });
    }
    Ok(WherePredicate::And(predicates))
}

fn order_predicate(
    pattern: &Pattern,
    alias: &str,
    app_id: Uuid,
    options: &MatchCteOptions,
) -> Result<WherePredicate, PatternError> {
    let mut predicate = pattern_where_plan(
        PatternWhereOptions {
            remove_unnecessary_index: false,
            predicate_options: options.predicate_options,
        },
        app_id,
        alias,
        pattern,
        Vec::new(),
    )?
    .predicate;
    predicate = WherePredicate::And(vec![
        predicate,
        compare(
            PredicateOperator::Equal,
            WhereExpression::qualified_column(alias, WhereColumn::CheckedDataType),
            WhereExpression::Cast {
                expression: Box::new(WhereExpression::Inline("number".into())),
                pg_type: PgType::CheckedDataType,
            },
        ),
    ]);
    Ok(predicate)
}

fn order_exists(
    pattern: &Pattern,
    entity: &WhereExpression,
    alias: &str,
    app_id: Uuid,
    options: &MatchCteOptions,
    nonnull: bool,
) -> Result<SelectQuery, PatternError> {
    let mut predicates = vec![
        order_predicate(pattern, alias, app_id, options)?,
        compare(
            PredicateOperator::Equal,
            WhereExpression::qualified_column(alias, WhereColumn::EntityId),
            entity.clone(),
        ),
    ];
    if nonnull {
        predicates.push(compare(
            PredicateOperator::NotEqual,
            WhereExpression::function(
                SqlFunction::ExtractNumberValue,
                WhereExpression::qualified_column(alias, WhereColumn::Value),
            ),
            WhereExpression::Literal(Value::Null),
        ));
    }
    Ok(SelectQuery {
        select: vec![WhereExpression::NamedColumn("1".into())],
        table: "triples".into(),
        alias: alias.into(),
        predicate: Box::new(WherePredicate::And(predicates)),
    })
}

fn compare(
    operator: PredicateOperator,
    left: WhereExpression,
    right: WhereExpression,
) -> WherePredicate {
    WherePredicate::Compare {
        operator,
        left,
        right,
    }
}
fn truth() -> WherePredicate {
    WherePredicate::Truthy(WhereExpression::Literal(json!(true)))
}
fn projection(expression: WhereExpression, alias: &str) -> StatementProjection {
    StatementProjection {
        expression: SelectExpression::Expression(expression),
        alias: Some(alias.into()),
    }
}
fn source(table: &str, alias: Option<&str>) -> SelectSource {
    SelectSource::Table(FromTable {
        table: table.into(),
        alias: alias.map(str::to_owned),
    })
}
fn select(name: String, statement: SelectStatement) -> PlannedCte {
    PlannedCte::Select(SelectCte {
        name,
        statement,
        materialization: Materialization::Materialized,
    })
}
