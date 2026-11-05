//! Bound independent scans without changing the observable Datalog row layout.
use std::collections::{HashMap, HashSet};

use instant_datalog::{
    DataType, FromTable, IndexKey, MatchCteOptions, MatchQueryPlan, Materialization,
    NormalizedClause, Pattern, PatternError, PatternMetadataNode, PatternWhereOptions, PlannedCte,
    PredicateOperator, QueryHint, SelectCte, SelectExpression, SelectQuery, SelectSource,
    SelectStatement, StatementProjection, Symbol, Term, WhereColumn, WhereExpression,
    WherePredicate, independent_equality_index_hint, parameterize_keyed_entity_scan,
    pattern_row_estimate, pattern_where_plan,
};
use uuid::Uuid;

pub(super) fn bound_independent_scans(
    plan: &mut MatchQueryPlan,
    clauses: &[NormalizedClause],
    app_id: Uuid,
    options: &MatchCteOptions,
) -> Result<(), PatternError> {
    if options.candidate_entity_scans != Some(true) {
        return Ok(());
    }
    let mut seeds = Vec::new();
    required_equalities(clauses, &mut seeds);
    // Prefer a small estimated result set (a unique lookup has an exact bound).
    // Never use a predicate inside OR as a globally required restriction.
    seeds.sort_by_key(|pattern| seed_rows(pattern, options).unwrap_or(u64::MAX));
    let patterns = pattern_lookup(&plan.pattern_metadata);
    let optional_drivers = optional_order_drivers(plan);
    let mut seeded = Vec::<Symbol>::new();
    for seed in seeds {
        // Avoid extra materialization/probes for a known broad equality. This
        // threshold selects a physical plan only; it never truncates a result.
        let estimated_rows = seed_rows(seed, options);
        if estimated_rows.is_some_and(|rows| rows > 1_024) {
            continue;
        }
        let Term::Variable(entity) = &seed.entity else {
            continue;
        };
        if seeded.contains(entity) {
            continue;
        }
        // The optional-order gather attaches its relation to the seed match with
        // a LEFT JOIN. When that driver is this candidate's own seed, the extra
        // membership restriction cannot remove any row the join would keep.
        let seed_tables = plan
            .ctes
            .iter()
            .filter_map(|cte| {
                let PlannedCte::Match(cte) = cte else {
                    return None;
                };
                let pattern = patterns.get(&format!("{}_entity_id", cte.name.replace('-', "_")))?;
                (*pattern == seed).then(|| cte.name.clone())
            })
            .collect::<HashSet<_>>();
        let targets = plan
            .ctes
            .iter()
            .enumerate()
            .filter_map(|(index, cte)| {
                let PlannedCte::Match(cte) = cte else {
                    return None;
                };
                let pattern = patterns.get(&format!("{}_entity_id", cte.name.replace('-', "_")))?;
                // Joined scans already have a binding; constrain only the otherwise
                // independent OR arms and optional-order relations. No ref rewrites.
                (cte.from.len() == 1
                    && cte.from[0].table == "triples"
                    && pattern.entity == seed.entity
                    && *pattern != seed
                    && matches!(
                        pattern.index.key,
                        IndexKey::Ea | IndexKey::Av | IndexKey::Ave
                    ))
                .then(|| (index, cte.name.clone()))
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            continue;
        }
        let name = format!("{}-candidate-{}", plan.final_table, seeded.len());
        let alias = format!("{name}-seed");
        let seed_where = pattern_where_plan(
            PatternWhereOptions {
                remove_unnecessary_index: false,
                predicate_options: options.predicate_options,
            },
            app_id,
            &alias,
            seed,
            Vec::new(),
        )?;
        let predicate = seed_where.predicate;
        if options.keyed_small_page_scans == Some(true)
            && estimated_rows.is_some()
            && options.pg_hints_enabled != Some(false)
        {
            // Cost the independent indexed equality for its new seed alias;
            // no data value or result is cached and no seed rows are truncated.
            plan.hints.extend(seed_where.hints);
            plan.hints
                .extend(independent_equality_index_hint(seed, options, &alias));
        }
        let mut keyed_targets = Vec::new();
        for (index, target_name) in targets {
            let PlannedCte::Match(cte) = &mut plan.ctes[index] else {
                unreachable!();
            };
            let target = cte.from[0]
                .alias
                .clone()
                .expect("match relation has an alias");
            let small_numeric_candidate = estimated_rows.is_some()
                && patterns
                    .get(&format!("{}_entity_id", cte.name.replace('-', "_")))
                    .is_some_and(|pattern| {
                        pattern.index.key == IndexKey::Ave
                            && pattern.index.data_type == Some(DataType::Number)
                            && matches!(&pattern.attribute, Term::Constants(values) if values.len() == 1)
                    });
            // The optional order relation is already constrained by its left join
            // to the seed match. Repeating membership makes PostgreSQL rescan the
            // materialized candidate CTE once per room message (N squared). Keep
            // the entity-bound primary-key probe; only the redundant predicate goes.
            // A materialized relation is evaluated alone, so it keeps membership
            // rather than reading the attribute's whole app partition.
            let optional_seed_binding = small_numeric_candidate
                && options.optional_order_seed_binding == Some(true)
                && cte.materialization == Materialization::NotMaterialized
                && optional_drivers
                    .get(&target_name)
                    .is_some_and(|driver| seed_tables.contains(driver));
            let entity_column = WhereExpression::qualified_column(&target, WhereColumn::EntityId);
            let binding = if optional_seed_binding {
                None
            } else if small_numeric_candidate {
                // The seed contains distinct entity IDs, so membership keeps
                // the old join's multiplicity without projecting another copy
                // of the candidate relation into every inlined order/filter
                // join. That extra copy can cause N squared numeric point probes.
                Some(WherePredicate::In {
                    negated: false,
                    expression: entity_column,
                    query: SelectQuery {
                        select: vec![WhereExpression::NamedColumn("bound-entity-id".into())],
                        table: name.clone(),
                        alias: name.clone(),
                        predicate: Box::new(WherePredicate::And(Vec::new())),
                    },
                })
            } else {
                cte.from.push(FromTable {
                    table: name.clone(),
                    alias: None,
                });
                if options.keyed_small_page_scans == Some(true) && estimated_rows.is_some() {
                    keyed_targets.push(index);
                }
                Some(WherePredicate::Compare {
                    operator: PredicateOperator::Equal,
                    left: entity_column,
                    right: WhereExpression::NamedColumn(format!("{name}.bound-entity-id")),
                })
            };
            if let Some(binding) = binding {
                cte.predicate = WherePredicate::And(vec![cte.predicate.clone(), binding]);
            }
            // The old hint was costed as a whole-attribute scan. Keep the normal
            // choice for unknown/broad seeds and other data types. For a known
            // small numeric candidate set, app/entity/attribute point probes
            // avoid rescanning the typed range/order index for each candidate.
            plan.hints.retain(|hint| {
                !matches!(hint,
                QueryHint::IndexScan { table_alias, .. } | QueryHint::BitmapScan { table_alias, .. }
                if table_alias == &target)
            });
            if small_numeric_candidate && options.pg_hints_enabled != Some(false) {
                plan.hints.push(QueryHint::IndexScan {
                    table_alias: target,
                    index: "triples_pkey".into(),
                });
            }
        }
        if estimated_rows.is_some() && options.pg_hints_enabled != Some(false) {
            // The required equality constrains this entity symbol throughout
            // the group, including null-check CTEs joined to a bound identity
            // scan. Avoid a whole-attribute nested probe for each candidate.
            for cte in &plan.ctes {
                let PlannedCte::Match(cte) = cte else {
                    continue;
                };
                if patterns
                    .get(&format!("{}_entity_id", cte.name.replace('-', "_")))
                    .is_some_and(|pattern| pattern.entity == seed.entity)
                {
                    add_forward_null_point_hints(&cte.predicate, app_id, &mut plan.hints);
                }
            }
        }
        // Keep Match metadata/columns and the full original predicate. Convert
        // only the otherwise-independent, now candidate-bound ordinary scans;
        // numeric membership and unknown/broad seeds retain their old path.
        for index in keyed_targets {
            let PlannedCte::Match(cte) = &plan.ctes[index] else {
                continue;
            };
            let mut statement = SelectStatement {
                select: cte.select.iter().cloned().map(Into::into).collect(),
                from: cte.from.iter().cloned().map(SelectSource::Table).collect(),
                predicate: Some(cte.predicate.clone()),
                ..SelectStatement::default()
            };
            if parameterize_keyed_entity_scan(&mut statement, app_id) {
                plan.ctes[index] = PlannedCte::Select(SelectCte {
                    name: cte.name.clone(),
                    statement,
                    materialization: cte.materialization,
                });
            }
        }
        let entity_column = WhereExpression::qualified_column(&alias, WhereColumn::EntityId);
        plan.ctes.insert(
            0,
            PlannedCte::Select(SelectCte {
                name,
                statement: SelectStatement {
                    distinct_on: vec![entity_column.clone()],
                    select: vec![StatementProjection {
                        expression: SelectExpression::Expression(entity_column),
                        alias: Some("bound-entity-id".into()),
                    }],
                    from: vec![SelectSource::Table(FromTable {
                        table: "triples".into(),
                        alias: Some(alias.clone()),
                    })],
                    predicate: Some(predicate),
                    ..SelectStatement::default()
                },
                materialization: Materialization::Materialized,
            }),
        );
        seeded.push(entity.clone());
    }
    Ok(())
}

/// Maps each optional page-order relation to the table its LEFT JOIN drives.
/// Only relations attached by the page planner's `-optional-source` gather are
/// included, so OR arms and ordinary scans never match.
fn optional_order_drivers(plan: &MatchQueryPlan) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for cte in &plan.ctes {
        let PlannedCte::OrGather(gather) = cte else {
            continue;
        };
        if !gather.name.ends_with("-optional-source") {
            continue;
        }
        for join in &gather.left_joins {
            result.insert(join.table.clone(), gather.from.table.clone());
        }
    }
    result
}

fn add_forward_null_point_hints(
    predicate: &WherePredicate,
    app_id: Uuid,
    hints: &mut Vec<QueryHint>,
) {
    match predicate {
        WherePredicate::And(values) | WherePredicate::Or(values) => {
            for value in values {
                add_forward_null_point_hints(value, app_id, hints);
            }
        }
        WherePredicate::Exists { query, .. } => {
            if query.table != "triples" {
                return;
            }
            let Some(outer_alias) = query
                .alias
                .strip_suffix("-subquery")
                .filter(|alias| !alias.is_empty())
            else {
                return;
            };
            let entity = WhereExpression::qualified_column(&query.alias, WhereColumn::EntityId);
            if query.select != [entity.clone()] {
                return;
            }
            let WherePredicate::And(values) = query.predicate.as_ref() else {
                return;
            };
            let fixed_app = values.iter().any(|value| {
                matches!(value, WherePredicate::Compare {
                    operator: PredicateOperator::Equal, left, right: WhereExpression::Uuid(id)
                } if *left == WhereExpression::qualified_column(&query.alias, WhereColumn::AppId)
                    && *id == app_id)
            });
            let fixed_attribute = values.iter().any(|value| {
                matches!(value, WherePredicate::Compare {
                    operator: PredicateOperator::Equal, left, right: WhereExpression::Uuid(_)
                } if *left == WhereExpression::qualified_column(&query.alias, WhereColumn::AttrId))
            });
            let correlated_entity = values.iter().any(|value| {
                matches!(value, WherePredicate::Compare {
                    operator: PredicateOperator::Equal, left, right: WhereExpression::Column(right)
                } if *left == entity && right.column == WhereColumn::EntityId
                    && right.qualifier.as_deref() == Some(outer_alias))
            });
            if fixed_app
                && fixed_attribute
                && correlated_entity
                && !hints.iter().any(|hint| {
                    matches!(hint, QueryHint::IndexScan {table_alias, ..} | QueryHint::BitmapScan {table_alias, ..}
                        if table_alias == &query.alias)
                })
            {
                hints.push(QueryHint::IndexScan {
                    table_alias: query.alias.clone(),
                    index: "triples_pkey".into(),
                });
            }
        }
        _ => {}
    }
}

pub(super) fn required_equalities<'a>(
    clauses: &'a [NormalizedClause],
    result: &mut Vec<&'a Pattern>,
) {
    for clause in clauses {
        match clause {
            NormalizedClause::And { clauses } => required_equalities(clauses, result),
            NormalizedClause::Pattern { pattern }
                if matches!(pattern.index.key, IndexKey::Av | IndexKey::Ave)
                    && matches!(&pattern.entity, Term::Variable(_))
                    && matches!(&pattern.attribute, Term::Constants(values) if values.len() == 1)
                    && matches!(&pattern.value, Term::Constants(values) if !values.is_empty() && values.len() <= 1024) =>
            {
                result.push(pattern)
            }
            _ => {}
        }
    }
}

pub(super) fn pattern_lookup(metadata: &[PatternMetadataNode]) -> HashMap<String, &Pattern> {
    fn collect<'a>(metadata: &'a [PatternMetadataNode], result: &mut HashMap<String, &'a Pattern>) {
        for node in metadata {
            match node {
                PatternMetadataNode::Pattern(meta) => {
                    if let Some(column) = meta.cte_columns.first() {
                        result.insert(column.clone(), &meta.pattern);
                    }
                }
                PatternMetadataNode::Or(branches) => {
                    for branch in branches {
                        collect(branch, result);
                    }
                }
            }
        }
    }
    let mut result = HashMap::new();
    collect(metadata, &mut result);
    result
}

pub(super) fn seed_rows(pattern: &Pattern, options: &MatchCteOptions) -> Option<u64> {
    if pattern.index.key == IndexKey::Av
        && let Term::Constants(values) = &pattern.value
    {
        return u64::try_from(values.len()).ok();
    }
    pattern_row_estimate(pattern, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point_query() -> WherePredicate {
        let alias = "t1-subquery";
        WherePredicate::Exists {
            negated: true,
            query: SelectQuery {
                select: vec![WhereExpression::qualified_column(
                    alias,
                    WhereColumn::EntityId,
                )],
                table: "triples".into(),
                alias: alias.into(),
                predicate: Box::new(WherePredicate::And(vec![
                    WherePredicate::Compare {
                        operator: PredicateOperator::Equal,
                        left: WhereExpression::qualified_column(alias, WhereColumn::AppId),
                        right: WhereExpression::Uuid(Uuid::from_u128(1)),
                    },
                    WherePredicate::Compare {
                        operator: PredicateOperator::Equal,
                        left: WhereExpression::qualified_column(alias, WhereColumn::EntityId),
                        right: WhereExpression::qualified_column("t1", WhereColumn::EntityId),
                    },
                    WherePredicate::Compare {
                        operator: PredicateOperator::Equal,
                        left: WhereExpression::qualified_column(alias, WhereColumn::AttrId),
                        right: WhereExpression::Uuid(Uuid::from_u128(2)),
                    },
                ])),
            },
        }
    }

    #[test]
    fn correlated_forward_exists_and_not_exists_get_only_a_primary_key_hint() {
        for negated in [false, true] {
            let mut predicate = point_query();
            let WherePredicate::Exists { negated: value, .. } = &mut predicate else {
                unreachable!()
            };
            *value = negated;
            let mut hints = Vec::new();
            add_forward_null_point_hints(&predicate, Uuid::from_u128(1), &mut hints);
            assert_eq!(
                hints,
                vec![QueryHint::IndexScan {
                    table_alias: "t1-subquery".into(),
                    index: "triples_pkey".into(),
                }]
            );
        }
    }

    #[test]
    fn legacy_reverse_and_nonpoint_subqueries_are_not_hinted() {
        for variant in 0..8 {
            let mut predicate = point_query();
            let WherePredicate::Exists { query, .. } = &mut predicate else {
                unreachable!()
            };
            let WherePredicate::And(values) = query.predicate.as_mut() else {
                unreachable!()
            };
            match variant {
                0 => query.table = "other".into(),
                1 => query.alias = "other".into(),
                2 => {
                    query.select = vec![WhereExpression::qualified_column(
                        "t1-subquery",
                        WhereColumn::Value,
                    )]
                }
                3 => {
                    values.remove(0);
                }
                4 => {
                    values.remove(2);
                }
                5 => {
                    let WherePredicate::Compare { right, .. } = &mut values[1] else {
                        unreachable!()
                    };
                    *right = WhereExpression::column(WhereColumn::EntityId);
                }
                6 => {
                    let WherePredicate::Compare { left, .. } = &mut values[1] else {
                        unreachable!()
                    };
                    *left = WhereExpression::NamedColumn("reverse-value-expression".into());
                }
                _ => {
                    let WherePredicate::Compare { right, .. } = &mut values[1] else {
                        unreachable!()
                    };
                    *right = WhereExpression::qualified_column("unrelated", WhereColumn::EntityId);
                }
            }
            let mut hints = Vec::new();
            add_forward_null_point_hints(&predicate, Uuid::from_u128(1), &mut hints);
            assert!(hints.is_empty(), "variant {variant}");
        }
        let mut hints = Vec::new();
        add_forward_null_point_hints(&point_query(), Uuid::from_u128(99), &mut hints);
        assert!(hints.is_empty());
    }

    #[test]
    fn nested_predicates_deduplicate_and_respect_existing_scan_hints() {
        let predicate = WherePredicate::And(vec![
            point_query(),
            WherePredicate::Or(vec![point_query(), point_query()]),
        ]);
        let mut hints = Vec::new();
        add_forward_null_point_hints(&predicate, Uuid::from_u128(1), &mut hints);
        assert_eq!(hints.len(), 1);
        let mut existing = vec![QueryHint::BitmapScan {
            table_alias: "t1-subquery".into(),
            index: "existing-index".into(),
        }];
        let original = existing.clone();
        add_forward_null_point_hints(&predicate, Uuid::from_u128(1), &mut existing);
        assert_eq!(existing, original);
    }

    #[test]
    fn point_hints_do_not_rewrite_predicates_projections_or_membership_queries() {
        let predicate = point_query();
        let original = predicate.clone();
        let mut hints = Vec::new();
        add_forward_null_point_hints(&predicate, Uuid::from_u128(1), &mut hints);
        assert_eq!(predicate, original);
        let WherePredicate::Exists { query, .. } = original else {
            unreachable!()
        };
        let membership = WherePredicate::In {
            negated: true,
            expression: WhereExpression::column(WhereColumn::EntityId),
            query,
        };
        let mut hints = Vec::new();
        add_forward_null_point_hints(&membership, Uuid::from_u128(1), &mut hints);
        assert!(hints.is_empty());
    }
}
