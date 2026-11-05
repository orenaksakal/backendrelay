use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::hash::BuildHasher;
use std::pin::Pin;

use instant_datalog::{
    BindingPath, BindingTree, CoarseTerm, Component, DatalogResult, DatalogTriple, DatalogValue,
    FromTable, FunctionClause, Index, IndexKey, InvalidationTopic, MatchCteOptions,
    MatchQueryContext, Materialization, NestedResultKind, NestedResultTable, NestedSqlPlan,
    NormalizedClause, PageInfo, PagePlanMetadata, PageResultInfo, PatternError,
    PatternMetadataNode, PlannedCte, QueryHint, RawPattern, RawTerm, SelectCte, SelectExpression,
    SelectSource, SelectStatement, SqlFunction, StatementProjection, Symbol, TopicPart,
    WhereExpression, add_page_info, plan_match_query_from_index,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use super::{
    Aggregate, AttributeCatalog, AttributeValueType, Cardinality, FILES_ETYPE, Form, FormOptions,
    InstaqlError, ValidationError, coerce_forms, compile_root, datalog_validation, level_symbol,
    missing_root, validate_aggregate, validation,
};
use crate::dates::{DateValue, parse_date_value};
use crate::{WhereCondition, WhereValue};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompiledRelationship {
    pub attr_id: Uuid,
    pub forward: bool,
    pub cardinality: Cardinality,
    pub unique: bool,
    pub parent_symbol: Symbol,
    pub child_symbol: Symbol,
}

impl CompiledRelationship {
    #[must_use]
    pub const fn singular(&self) -> bool {
        if self.forward {
            matches!(self.cardinality, Cardinality::One)
        } else {
            self.unique
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompiledNode {
    pub key: String,
    pub etype: String,
    pub level: usize,
    pub options: FormOptions,
    pub selector_clauses: Vec<NormalizedClause>,
    pub missing: bool,
    pub page: Option<PageInfo>,
    pub aggregate: Option<Aggregate>,
    pub fetch_attr_ids: Vec<Uuid>,
    pub relationship: Option<CompiledRelationship>,
    pub children: Vec<CompiledNode>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompiledQuery {
    pub roots: Vec<CompiledNode>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryConstraint {
    Where(Vec<super::WhereCondition>),
    DenyAll,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NestedPatternGroup {
    pub clauses: Vec<NormalizedClause>,
    pub missing: bool,
    pub page: Option<PageInfo>,
    pub aggregate: Option<Aggregate>,
    pub children: Option<NestedPatternChildren>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NestedPatternChildren {
    pub join_symbol: Symbol,
    pub groups: Vec<NestedPatternGroup>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlannedNestedQuery {
    pub ctes: Vec<PlannedCte>,
    pub hints: Vec<QueryHint>,
    pub groups: Vec<PlannedNestedGroup>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlannedNestedGroup {
    pub table: Option<String>,
    pub metadata: Vec<PatternMetadataNode>,
    pub missing_clauses: Option<Vec<NormalizedClause>>,
    pub page: Option<PagePlanMetadata>,
    pub aggregate: Option<Aggregate>,
    pub children: Option<PlannedNestedChildren>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ensure_empty_symbol_values_for_topics: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlannedNestedChildren {
    pub join_symbol: Symbol,
    pub groups: Vec<PlannedNestedGroup>,
}

impl PlannedNestedQuery {
    #[must_use]
    pub fn sql_plan(&self) -> NestedSqlPlan {
        let mut result_tables = Vec::new();
        collect_result_tables(&self.groups, &mut result_tables);
        NestedSqlPlan {
            ctes: self.ctes.clone(),
            hints: self.hints.clone(),
            result_tables,
        }
    }
}

fn collect_result_tables(groups: &[PlannedNestedGroup], tables: &mut Vec<NestedResultTable>) {
    for group in groups {
        if let Some(table) = &group.table {
            tables.push(NestedResultTable {
                table: table.clone(),
                kind: if group.aggregate.is_some() {
                    NestedResultKind::Count
                } else {
                    NestedResultKind::Rows
                },
            });
            if let Some(page) = &group.page {
                tables.extend(
                    [&page.has_next_table, &page.has_previous_table].map(|table| {
                        NestedResultTable {
                            table: table.clone(),
                            kind: NestedResultKind::Rows,
                        }
                    }),
                );
            }
        }
        if let Some(children) = &group.children {
            collect_result_tables(&children.groups, tables);
        }
    }
}

/// Lowers every nested pattern group into one globally indexed CTE graph.
///
/// Child groups are constrained through materialized CTEs containing the
/// distinct parent entity bindings, matching `accumulate-nested-match-query`.
///
/// # Errors
///
/// Returns [`InstaqlError`] when generated pattern validation, pattern/page
/// planning, or a child join binding fails.
pub fn plan_nested_query(
    query: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
) -> Result<PlannedNestedQuery, InstaqlError> {
    let groups = query
        .pattern_groups()
        .map_err(|error| InstaqlError::Validation(vec![error]))?;
    let mut state = NestedPlannerState::default();
    let planned_groups = plan_group_level(
        &groups,
        &mut state,
        "m-",
        app_id,
        options,
        &HashMap::new(),
        &HashMap::new(),
    )?;
    if options.small_page_jit_off == Some(true)
        && options.pg_hints_enabled.unwrap_or(true)
        && small_flat_pages(query)
    {
        // pg_hint_plan applies this only while planning this statement. It
        // adds no checkout, SET roundtrip, session state or result cache. Keep
        // unbounded, bulk, aggregate, nested/ref and large-offset work on the
        // existing planner path, where JIT can amortize its compilation cost.
        state.hints.push(QueryHint::DisableJit);
    }
    Ok(PlannedNestedQuery {
        ctes: state.ctes,
        hints: state.hints,
        groups: planned_groups,
    })
}

fn small_flat_pages(query: &CompiledQuery) -> bool {
    (1..=2).contains(&query.roots.len())
        && query.roots.iter().all(|root| {
            !root.missing
                && root.aggregate.is_none()
                && root.children.is_empty()
                && root.page.as_ref().is_some_and(|page| {
                    page.limit.is_some_and(|limit| (1..=100).contains(&limit))
                        && page.offset.unwrap_or_default() <= 100
                })
                && flat_blob_pattern_count(&root.selector_clauses).is_some_and(|count| count <= 8)
        })
}

fn flat_blob_pattern_count(clauses: &[NormalizedClause]) -> Option<usize> {
    clauses.iter().try_fold(0_usize, |total, clause| {
        let count = match clause {
            NormalizedClause::Pattern { pattern } => {
                if !matches!(pattern.index.key, IndexKey::Ea | IndexKey::Av | IndexKey::Ave)
                    || matches!(&pattern.value, instant_datalog::Term::Function(FunctionClause::IsNull(clause)) if clause.reference)
                {
                    return None;
                }
                1
            }
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                flat_blob_pattern_count(clauses)?
            }
        };
        total.checked_add(count).filter(|count| *count <= 8)
    })
}

#[derive(Default)]
struct NestedPlannerState {
    next_index: usize,
    ctes: Vec<PlannedCte>,
    hints: Vec<QueryHint>,
}

fn plan_group_level(
    groups: &[NestedPatternGroup],
    state: &mut NestedPlannerState,
    prefix: &str,
    app_id: Uuid,
    options: &MatchCteOptions,
    additional_joins: &HashMap<Symbol, BindingPath>,
    initial_symbol_cardinalities: &HashMap<Symbol, u64>,
) -> Result<Vec<PlannedNestedGroup>, PatternError> {
    groups
        .iter()
        .map(|group| {
            if group.missing {
                return Ok(PlannedNestedGroup {
                    table: None,
                    metadata: Vec::new(),
                    missing_clauses: Some(group.clauses.clone()),
                    page: None,
                    aggregate: group.aggregate,
                    children: None,
                    ensure_empty_symbol_values_for_topics: options
                        .ensure_empty_symbol_values_for_topics,
                });
            }
            let context = MatchQueryContext {
                prefix,
                app_id,
                additional_joins,
                initial_symbol_cardinalities,
                options,
            };
            let mut plan = plan_match_query_from_index(&context, &group.clauses, state.next_index)?;
            if let Some(page) = &group.page {
                add_page_info(&mut plan, &context, page)?;
            }
            if !crate::ordered_candidates::bound_ordered_page(
                &mut plan,
                &group.clauses,
                group.page.as_ref(),
                app_id,
                options,
                additional_joins.is_empty(),
            )? {
                crate::candidate_entities::bound_independent_scans(
                    &mut plan,
                    &group.clauses,
                    app_id,
                    options,
                )?;
            }
            materialize_result_table(&mut plan.ctes, &plan.final_table);
            state.next_index = plan.next_index;
            state.ctes.extend(plan.ctes);
            state.hints.extend(plan.hints);

            let children = group
                .children
                .as_ref()
                .map(|children| {
                    let join_index = state.next_index;
                    let join_table = indexed_table(prefix, join_index);
                    let expression = join_binding_expression(
                        &plan.symbol_map,
                        &children.join_symbol,
                        prefix,
                        &plan.final_table,
                    )?;
                    state.ctes.push(PlannedCte::Select(SelectCte {
                        name: join_table,
                        statement: SelectStatement {
                            distinct_on: vec![expression.clone()],
                            select: vec![StatementProjection {
                                expression: SelectExpression::Expression(expression),
                                alias: Some(format!(
                                    "{}-entity-id",
                                    indexed_table(prefix, join_index)
                                )),
                            }],
                            from: vec![SelectSource::Table(FromTable {
                                table: plan.final_table.clone(),
                                alias: None,
                            })],
                            ..SelectStatement::default()
                        },
                        materialization: Materialization::Materialized,
                    }));
                    state.next_index += 1;
                    let child_joins = HashMap::from([(
                        children.join_symbol.clone(),
                        BindingPath::new(join_index, Component::Entity, IndexKey::Ea),
                    )]);
                    let child_cardinalities = plan
                        .symbol_cardinalities
                        .get(&children.join_symbol)
                        .copied()
                        .map(|estimate| HashMap::from([(children.join_symbol.clone(), estimate)]))
                        .unwrap_or_default();
                    Ok::<_, PatternError>(PlannedNestedChildren {
                        join_symbol: children.join_symbol.clone(),
                        groups: plan_group_level(
                            &children.groups,
                            state,
                            prefix,
                            app_id,
                            options,
                            &child_joins,
                            &child_cardinalities,
                        )?,
                    })
                })
                .transpose()?;

            Ok(PlannedNestedGroup {
                table: Some(plan.final_table),
                metadata: plan.pattern_metadata,
                missing_clauses: None,
                page: plan.page,
                aggregate: group.aggregate,
                children,
                ensure_empty_symbol_values_for_topics: plan.ensure_empty_symbol_values_for_topics,
            })
        })
        .collect()
}

fn materialize_result_table(ctes: &mut [PlannedCte], table: &str) {
    for cte in ctes {
        match cte {
            PlannedCte::Match(cte) if cte.name == table => {
                cte.materialization = Materialization::Materialized;
            }
            PlannedCte::OrGather(cte) if cte.name == table => {
                cte.materialization = Materialization::Materialized;
            }
            PlannedCte::Select(cte) if cte.name == table => {
                cte.materialization = Materialization::Materialized;
            }
            PlannedCte::Match(_) | PlannedCte::OrGather(_) | PlannedCte::Select(_) => {}
        }
    }
}

fn join_binding_expression(
    symbol_map: &HashMap<Symbol, Vec<BindingTree>>,
    symbol: &Symbol,
    prefix: &str,
    result_table: &str,
) -> Result<WhereExpression, PatternError> {
    let paths = symbol_map
        .get(symbol)
        .into_iter()
        .flatten()
        .flat_map(flatten_binding_tree)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(PatternError::MissingNestedJoinSymbol(
            symbol.as_str().to_owned(),
        ));
    }
    let expressions = paths
        .into_iter()
        .map(|path| {
            let suffix = match path.component {
                Component::Entity => "entity-id",
                Component::Attribute => "attr-id",
                Component::Value => "value",
            };
            let expression = WhereExpression::NamedColumn(format!(
                "{result_table}.{}-{suffix}",
                indexed_table(prefix, path.pattern_index)
            ));
            if path.reference_value {
                WhereExpression::function(SqlFunction::JsonUuidToUuid, expression)
            } else {
                expression
            }
        })
        .collect::<Vec<_>>();
    Ok(if let [expression] = expressions.as_slice() {
        expression.clone()
    } else {
        WhereExpression::Coalesce(expressions)
    })
}

fn flatten_binding_tree(tree: &BindingTree) -> Vec<&BindingPath> {
    match tree {
        BindingTree::Path(path) => vec![path],
        BindingTree::And(trees) | BindingTree::Or(trees) => {
            trees.iter().flat_map(flatten_binding_tree).collect()
        }
    }
}

fn indexed_table(prefix: &str, index: usize) -> String {
    format!("{}{index}", prefix.trim_start_matches(':'))
}

impl CompiledQuery {
    /// Returns the recursive selector/fetch pattern groups consumed by nested Datalog execution.
    ///
    /// # Errors
    ///
    /// Returns a validation error if a compiled symbol or fetch pattern is invalid.
    pub fn pattern_groups(&self) -> Result<Vec<NestedPatternGroup>, ValidationError> {
        self.roots.iter().map(CompiledNode::pattern_group).collect()
    }
}

impl CompiledNode {
    /// Builds this form's selector/fetch pattern hierarchy.
    ///
    /// # Errors
    ///
    /// Returns a validation error if its symbol or generated fetch pattern is invalid.
    pub fn pattern_group(&self) -> Result<NestedPatternGroup, ValidationError> {
        if self.missing {
            return Ok(NestedPatternGroup {
                clauses: self.selector_clauses.clone(),
                missing: true,
                page: None,
                aggregate: self.aggregate,
                children: None,
            });
        }
        let children = if self.aggregate.is_none() {
            let join_symbol = self.entity_symbol()?;
            Some(NestedPatternChildren {
                join_symbol: join_symbol.clone(),
                groups: vec![NestedPatternGroup {
                    clauses: vec![self.fetch_clause()?],
                    missing: false,
                    page: None,
                    aggregate: None,
                    children: if self.children.is_empty() {
                        None
                    } else {
                        Some(NestedPatternChildren {
                            join_symbol,
                            groups: self
                                .children
                                .iter()
                                .map(Self::pattern_group)
                                .collect::<Result<_, _>>()?,
                        })
                    },
                }],
            })
        } else {
            None
        };
        Ok(NestedPatternGroup {
            clauses: self.selector_clauses.clone(),
            missing: false,
            page: self.page.clone(),
            aggregate: self.aggregate,
            children,
        })
    }

    fn entity_symbol(&self) -> Result<Symbol, ValidationError> {
        level_symbol(&self.etype, self.level)
    }

    fn fetch_clause(&self) -> Result<NormalizedClause, ValidationError> {
        let pattern = RawPattern::new(
            Index::new(IndexKey::Ea),
            vec![
                RawTerm::Variable(self.entity_symbol()?),
                RawTerm::Constants(
                    self.fetch_attr_ids
                        .iter()
                        .map(ToString::to_string)
                        .map(Value::String)
                        .collect(),
                ),
            ],
        )
        .normalize()
        .map_err(|error| datalog_validation(&error))?;
        Ok(NormalizedClause::Pattern { pattern })
    }
}

/// Compiles all roots, parent field fetches, relationship selectors, and descendants.
///
/// # Errors
///
/// Returns [`InstaqlError`] for form validation, missing/non-reference child links,
/// or invalid generated Datalog patterns.
pub fn compile_query(
    query: &Value,
    catalog: &AttributeCatalog,
    admin: bool,
) -> Result<CompiledQuery, InstaqlError> {
    compile_query_with_constraints(query, catalog, admin, &HashMap::new())
}

/// Compiles a query while applying permission-derived constraints to every
/// root or nested form of the matching entity type.
///
/// # Errors
///
/// Returns the same validation and Datalog errors as [`compile_query`],
/// including errors in the injected where conditions.
pub fn compile_query_with_constraints<S: BuildHasher>(
    query: &Value,
    catalog: &AttributeCatalog,
    admin: bool,
    constraints: &HashMap<String, QueryConstraint, S>,
) -> Result<CompiledQuery, InstaqlError> {
    let forms = coerce_forms(query, catalog)?;
    let mut roots = Vec::new();
    for form in forms {
        let form_path = vec![form.key.clone()];
        let root = compile_node(form, catalog, admin, None, constraints, &form_path)
            .map_err(InstaqlError::Validation)?;
        roots.push(root);
    }
    Ok(CompiledQuery { roots })
}

fn compile_node<S: BuildHasher>(
    mut form: Form,
    catalog: &AttributeCatalog,
    admin: bool,
    relationship: Option<(CompiledRelationship, NormalizedClause)>,
    constraints: &HashMap<String, QueryConstraint, S>,
    form_path: &[String],
) -> Result<CompiledNode, Vec<ValidationError>> {
    form.options.where_clause = extend_where_with_reference_constraints(
        &form.etype,
        form.options.where_clause,
        catalog,
        constraints,
    );
    match constraints.get(&form.etype) {
        Some(QueryConstraint::Where(conditions)) => {
            form.options.where_clause.extend(conditions.clone());
        }
        Some(QueryConstraint::DenyAll) => {
            form.options
                .where_clause
                .push(impossible_id_condition(Vec::new()));
        }
        None => {}
    }
    let child_forms = form.children.clone();
    let had_where = !form.options.where_clause.is_empty();
    let mut compiled = compile_root(form, catalog)?;
    let compiled_relationship = relationship
        .as_ref()
        .map(|(relationship, _)| relationship.clone());
    if !compiled.missing
        && let Some((_, relation_clause)) = relationship
    {
        if had_where {
            compiled.clauses.insert(0, relation_clause);
        } else {
            compiled.clauses = vec![relation_clause];
        }
    }
    let children = if compiled.missing {
        Vec::new()
    } else {
        let parent_symbol =
            level_symbol(&compiled.form.etype, compiled.form.level).map_err(|error| vec![error])?;
        let mut children = Vec::new();
        for child in child_forms {
            let child_path = super::appended(form_path, child.key.clone());
            let result = match compile_relationship(&compiled.form, &child, catalog, &parent_symbol)
            {
                Ok(Some(relation)) => compile_node(
                    child,
                    catalog,
                    admin,
                    Some(relation),
                    constraints,
                    &child_path,
                ),
                Ok(None) => {
                    compile_missing_node(child, admin, &child_path).map_err(|error| vec![error])
                }
                Err(error) => Err(vec![error]),
            };
            children.push(result?);
        }
        children
    };
    validate_aggregate(&compiled.form, admin, form_path).map_err(|error| vec![error])?;
    Ok(CompiledNode {
        key: compiled.form.key,
        etype: compiled.form.etype,
        level: compiled.form.level,
        options: compiled.form.options,
        selector_clauses: compiled.clauses,
        missing: compiled.missing,
        page: compiled.page,
        aggregate: compiled.aggregate,
        fetch_attr_ids: compiled.selected_attr_ids,
        relationship: compiled_relationship,
        children,
    })
}

fn impossible_id_condition(prefix: Vec<String>) -> WhereCondition {
    WhereCondition::Condition {
        path: prefix
            .into_iter()
            .chain(std::iter::once("id".to_owned()))
            .collect(),
        value: WhereValue::Scalar(Value::String(Uuid::new_v4().to_string())),
    }
}

fn extend_where_with_reference_constraints<S: BuildHasher>(
    etype: &str,
    conditions: Vec<WhereCondition>,
    catalog: &AttributeCatalog,
    constraints: &HashMap<String, QueryConstraint, S>,
) -> Vec<WhereCondition> {
    // Mirrors `extend-where-with-rule-refs`: a filter that crosses an entity
    // boundary must also satisfy that related entity's rule-derived where.
    // Keeping the injection inside each boolean branch is important both for
    // query semantics and for the invalidation topics compiled from the branch.
    conditions
        .into_iter()
        .map(|condition| {
            extend_condition_with_reference_constraints(etype, condition, catalog, constraints)
        })
        .collect()
}

fn extend_condition_with_reference_constraints<S: BuildHasher>(
    etype: &str,
    condition: WhereCondition,
    catalog: &AttributeCatalog,
    constraints: &HashMap<String, QueryConstraint, S>,
) -> WhereCondition {
    match condition {
        WhereCondition::And { conditions } => WhereCondition::And {
            conditions: extend_where_with_reference_constraints(
                etype,
                conditions,
                catalog,
                constraints,
            ),
        },
        WhereCondition::Or { conditions } => WhereCondition::Or {
            conditions: extend_where_with_reference_constraints(
                etype,
                conditions,
                catalog,
                constraints,
            ),
        },
        WhereCondition::Condition { path, value } => {
            let mut current_etype = etype;
            let mut prefix = Vec::new();
            let mut injected = Vec::new();
            for segment in &path {
                prefix.push(segment.clone());
                let Some(next_etype) = catalog.link_etype(current_etype, segment) else {
                    break;
                };
                current_etype = next_etype;
                match constraints.get(current_etype) {
                    Some(QueryConstraint::Where(conditions)) => {
                        injected.extend(
                            conditions
                                .iter()
                                .cloned()
                                .map(|condition| prefix_condition_paths(&prefix, condition)),
                        );
                    }
                    Some(QueryConstraint::DenyAll) => {
                        injected.push(impossible_id_condition(prefix.clone()));
                    }
                    None => {}
                }
            }
            let condition = WhereCondition::Condition { path, value };
            if injected.is_empty() {
                condition
            } else {
                WhereCondition::And {
                    conditions: std::iter::once(condition).chain(injected).collect(),
                }
            }
        }
    }
}

fn prefix_condition_paths(prefix: &[String], condition: WhereCondition) -> WhereCondition {
    match condition {
        WhereCondition::Condition { path, value } => WhereCondition::Condition {
            path: prefix.iter().cloned().chain(path).collect(),
            value,
        },
        WhereCondition::And { conditions } => WhereCondition::And {
            conditions: conditions
                .into_iter()
                .map(|condition| prefix_condition_paths(prefix, condition))
                .collect(),
        },
        WhereCondition::Or { conditions } => WhereCondition::Or {
            conditions: conditions
                .into_iter()
                .map(|condition| prefix_condition_paths(prefix, condition))
                .collect(),
        },
    }
}

fn compile_missing_node(
    form: Form,
    admin: bool,
    form_path: &[String],
) -> Result<CompiledNode, ValidationError> {
    validate_aggregate(&form, admin, form_path)?;
    let entity_symbol = level_symbol(&form.etype, form.level)?;
    let compiled = missing_root(form, entity_symbol)?;
    Ok(CompiledNode {
        key: compiled.form.key,
        etype: compiled.form.etype,
        level: compiled.form.level,
        options: compiled.form.options,
        selector_clauses: compiled.clauses,
        missing: true,
        page: None,
        aggregate: compiled.aggregate,
        fetch_attr_ids: Vec::new(),
        relationship: None,
        children: Vec::new(),
    })
}

fn compile_relationship(
    parent: &Form,
    child: &Form,
    catalog: &AttributeCatalog,
    parent_symbol: &Symbol,
) -> Result<Option<(CompiledRelationship, NormalizedClause)>, ValidationError> {
    let relationship = catalog
        .forward(&parent.etype, &child.key)
        .map(|attribute| (attribute, true))
        .or_else(|| {
            catalog
                .reverse(&parent.etype, &child.key)
                .map(|attribute| (attribute, false))
        });
    let Some((attribute, forward)) = relationship else {
        return Ok(None);
    };
    if attribute.value_type != AttributeValueType::Ref {
        return Err(validation(
            "link?",
            &[parent.key.clone(), child.key.clone()],
            format!("{}.{} needs to be a link", parent.etype, child.key),
        ));
    }
    let child_symbol = level_symbol(&child.etype, child.level)?;
    let components = if forward {
        vec![
            RawTerm::Variable(parent_symbol.clone()),
            RawTerm::Constant(Value::String(attribute.id.to_string())),
            RawTerm::Variable(child_symbol.clone()),
        ]
    } else {
        vec![
            RawTerm::Variable(child_symbol.clone()),
            RawTerm::Constant(Value::String(attribute.id.to_string())),
            RawTerm::Variable(parent_symbol.clone()),
        ]
    };
    let pattern = RawPattern::new(Index::new(IndexKey::Vae), components)
        .normalize()
        .map_err(|error| datalog_validation(&error))?;
    Ok(Some((
        CompiledRelationship {
            attr_id: attribute.id,
            forward,
            cardinality: attribute.cardinality,
            unique: attribute.unique,
            parent_symbol: parent_symbol.clone(),
            child_symbol,
        },
        NormalizedClause::Pattern { pattern },
    )))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExecutedNode {
    pub selector: DatalogResult,
    pub aggregate: Option<i64>,
    pub entities: Vec<ExecutedEntity>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExecutedEntity {
    pub fetch: DatalogResult,
    pub children: Vec<ExecutedNode>,
}

/// Collects every precise dependency topic produced while executing a nested query.
#[must_use]
pub fn collect_invalidation_topics(results: &[ExecutedNode]) -> Vec<InvalidationTopic> {
    fn push_unique(topics: &mut Vec<InvalidationTopic>, topic: &InvalidationTopic) {
        if !topics.contains(topic) {
            topics.push(topic.clone());
        }
    }

    fn collect(nodes: &[ExecutedNode], topics: &mut Vec<InvalidationTopic>) {
        for node in nodes {
            for topic in &node.selector.topics {
                push_unique(topics, topic);
            }
            for entity in &node.entities {
                for topic in &entity.fetch.topics {
                    push_unique(topics, topic);
                }
                collect(&entity.children, topics);
            }
        }
    }

    let mut topics = Vec::new();
    collect(results, &mut topics);
    topics
}

/// Collects invalidation topics, restoring the coarse dependency lost by
/// aggregate result envelopes. Aggregate groups intentionally carry no rows,
/// so their decoder cannot emit precise topics; the upstream implementation
/// used the aggregate group's original patterns for this case.
///
/// # Errors
///
/// Returns a validation error when an aggregate node's original query patterns
/// cannot be converted into conservative invalidation topics.
pub fn collect_query_invalidation_topics(
    query: &CompiledQuery,
    results: &[ExecutedNode],
) -> Result<Vec<InvalidationTopic>, ValidationError> {
    fn push_unique(topics: &mut Vec<InvalidationTopic>, topic: InvalidationTopic) {
        if !topics.contains(&topic) {
            topics.push(topic);
        }
    }

    fn collect(
        nodes: &[CompiledNode],
        results: &[ExecutedNode],
        topics: &mut Vec<InvalidationTopic>,
    ) -> Result<(), ValidationError> {
        for (node, result) in nodes.iter().zip(results) {
            if result.aggregate.is_some() {
                let aggregate_query = CompiledQuery {
                    roots: vec![node.clone()],
                };
                for topic in coarse_invalidation_topics(&aggregate_query)? {
                    push_unique(topics, topic);
                }
            }
            for entity in &result.entities {
                collect(&node.children, &entity.children, topics)?;
            }
        }
        Ok(())
    }

    let mut topics = collect_invalidation_topics(results);
    collect(&query.roots, results, &mut topics)?;
    Ok(topics)
}

/// Builds the conservative dependency topics for every selector and fetch pattern.
///
/// # Errors
///
/// Returns a validation error if the compiled query cannot reconstruct its nested
/// pattern hierarchy.
pub fn coarse_invalidation_topics(
    query: &CompiledQuery,
) -> Result<Vec<InvalidationTopic>, ValidationError> {
    fn part(term: &CoarseTerm, uuid_component: bool) -> TopicPart {
        let CoarseTerm::Constants(values) = term else {
            return TopicPart::Any;
        };
        if uuid_component
            && let [value] = values.as_slice()
            && let Ok(FunctionClause::Not(value)) = serde_json::from_value(value.clone())
            && let Some(value) = value.as_str().and_then(|value| Uuid::parse_str(value).ok())
        {
            return TopicPart::Not(DatalogValue::Uuid(value));
        }
        TopicPart::Values(
            values
                .iter()
                .map(|value| {
                    if uuid_component
                        && let Some(value) = value.as_str()
                        && let Ok(value) = Uuid::parse_str(value)
                    {
                        return DatalogValue::Uuid(value);
                    }
                    DatalogValue::Json(value.clone())
                })
                .collect(),
        )
    }

    fn collect_clause(clause: &NormalizedClause, topics: &mut Vec<InvalidationTopic>) {
        match clause {
            NormalizedClause::Pattern { pattern } => {
                let coarse = pattern.coarse_topic();
                let topic = InvalidationTopic {
                    indexes: coarse.indexes,
                    entity: part(&coarse.entity, true),
                    attribute: part(&coarse.attribute, true),
                    value: part(&coarse.value, pattern.index.key.value_is_reference()),
                };
                if !topics.contains(&topic) {
                    topics.push(topic);
                }
            }
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                for clause in clauses {
                    collect_clause(clause, topics);
                }
            }
        }
    }

    fn collect_group(group: &NestedPatternGroup, topics: &mut Vec<InvalidationTopic>) {
        for clause in &group.clauses {
            collect_clause(clause, topics);
        }
        if let Some(children) = &group.children {
            for group in &children.groups {
                collect_group(group, topics);
            }
        }
    }

    let mut topics = Vec::new();
    for group in query.pattern_groups()? {
        collect_group(&group, &mut topics);
    }
    Ok(topics)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InstaqlNode {
    pub data: InstaqlNodeData,
    pub child_nodes: Vec<InstaqlNode>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InstaqlNodeData {
    pub key: Option<String>,
    pub etype: String,
    pub options: FormOptions,
    pub datalog_result: DatalogResult,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ResultShapeError {
    #[error("expected {expected} {scope} results, got {actual}")]
    Cardinality {
        scope: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("result references unknown attribute {0}")]
    UnknownAttribute(Uuid),
    #[error("the $files system catalog is missing its {0} attribute")]
    MissingFileAttribute(&'static str),
    #[error("$files location-id must be a string, got {0}")]
    InvalidFileLocation(String),
    #[error("could not sign $files URL: {0}")]
    FileUrlSigning(String),
    #[error("unable to parse checked date sort value {0}")]
    InvalidDateSortValue(String),
}

/// Signs a public download URL for a persisted `$files.location-id`.
pub trait FileUrlSigner {
    /// Produces the externally visible URL for one stored object.
    ///
    /// # Errors
    ///
    /// Returns a provider-specific message if URL generation fails.
    fn signed_download_url(&self, app_id: Uuid, location_id: &str) -> Result<String, String>;
}

pub type AsyncFileUrlBatchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>>;

/// Asynchronously signs every file location needed by one query response.
///
/// A batch boundary lets production storage resolve live rollout policy once
/// per response instead of once per file.
pub trait AsyncFileUrlSigner: Send + Sync {
    fn signed_download_urls(
        &self,
        app_id: Uuid,
        location_ids: Vec<String>,
    ) -> AsyncFileUrlBatchFuture<'_>;
}

impl<F> FileUrlSigner for F
where
    F: Fn(Uuid, &str) -> Result<String, String>,
{
    fn signed_download_url(&self, app_id: Uuid, location_id: &str) -> Result<String, String> {
        self(app_id, location_id)
    }
}

/// Synthesizes virtual `$files.url` triples and removes hidden support fields.
///
/// The source execution tree is left unchanged. This allows callers to retry a
/// transient signing failure without re-running the database query.
///
/// # Errors
///
/// Returns [`ResultShapeError`] for a malformed result tree, an invalid
/// `location-id`, a missing system attribute, or a signing failure.
pub fn transform_file_results<S: FileUrlSigner>(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    app_id: Uuid,
    signer: &S,
) -> Result<Vec<ExecutedNode>, ResultShapeError> {
    let plans = plan_file_results(query, results, catalog)?;
    let signed_urls = plans
        .iter()
        .filter_map(|plan| plan.location.as_deref())
        .map(|location| {
            signer
                .signed_download_url(app_id, location)
                .map_err(ResultShapeError::FileUrlSigning)
        })
        .collect::<Result<Vec<_>, _>>()?;
    apply_file_results(query, results, plans, signed_urls)
}

#[derive(Clone)]
struct FileTransformPlan {
    url_attr_id: Uuid,
    location_attr_id: Uuid,
    exposes_location: bool,
    anchor: Option<DatalogTriple>,
    location: Option<String>,
}

/// Applies file virtual-field semantics through an asynchronous batch signer.
///
/// The source execution tree is left unchanged, and signing occurs only after
/// the complete result tree has been validated.
///
/// # Errors
///
/// Returns an error for malformed result trees, invalid locations, missing
/// system attributes, signing failures, or a signer result-count mismatch.
pub async fn transform_file_results_async<S: AsyncFileUrlSigner + ?Sized>(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    app_id: Uuid,
    signer: &S,
) -> Result<Vec<ExecutedNode>, ResultShapeError> {
    transform_file_results_async_impl(query, Cow::Borrowed(results), catalog, app_id, signer).await
}

/// Applies the same validated file transformation without copying an owned tree.
///
/// Use [`transform_file_results_async`] when the source must remain available
/// for retries. This consuming variant preserves validation, signing order,
/// hidden-field removal and errors, but does not retain the original tree.
///
/// # Errors
///
/// Returns the same errors as [`transform_file_results_async`].
pub async fn transform_file_results_async_owned<S: AsyncFileUrlSigner + ?Sized>(
    query: &CompiledQuery,
    results: Vec<ExecutedNode>,
    catalog: &AttributeCatalog,
    app_id: Uuid,
    signer: &S,
) -> Result<Vec<ExecutedNode>, ResultShapeError> {
    transform_file_results_async_impl(query, Cow::Owned(results), catalog, app_id, signer).await
}

async fn transform_file_results_async_impl<S: AsyncFileUrlSigner + ?Sized>(
    query: &CompiledQuery,
    results: Cow<'_, [ExecutedNode]>,
    catalog: &AttributeCatalog,
    app_id: Uuid,
    signer: &S,
) -> Result<Vec<ExecutedNode>, ResultShapeError> {
    let plans = plan_file_results(query, &results, catalog)?;
    let locations = plans
        .iter()
        .filter_map(|plan| plan.location.clone())
        .collect::<Vec<_>>();
    let expected = locations.len();
    let signed_urls = if locations.is_empty() {
        Vec::new()
    } else {
        signer
            .signed_download_urls(app_id, locations)
            .await
            .map_err(ResultShapeError::FileUrlSigning)?
    };
    if signed_urls.len() != expected {
        return Err(ResultShapeError::FileUrlSigning(format!(
            "file URL signer returned {} URLs for {expected} locations",
            signed_urls.len()
        )));
    }

    apply_file_results_owned(query, results.into_owned(), plans, signed_urls)
}

fn plan_file_results(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
) -> Result<Vec<FileTransformPlan>, ResultShapeError> {
    let mut plans = Vec::new();
    plan_file_level(&query.roots, results, catalog, &mut plans)?;
    Ok(plans)
}

fn apply_file_results(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    plans: Vec<FileTransformPlan>,
    signed_urls: Vec<String>,
) -> Result<Vec<ExecutedNode>, ResultShapeError> {
    apply_file_results_owned(query, results.to_vec(), plans, signed_urls)
}

fn apply_file_results_owned(
    query: &CompiledQuery,
    mut transformed: Vec<ExecutedNode>,
    plans: Vec<FileTransformPlan>,
    signed_urls: Vec<String>,
) -> Result<Vec<ExecutedNode>, ResultShapeError> {
    let mut plans = plans.into_iter();
    let mut signed_urls = signed_urls.into_iter();
    apply_file_level(&query.roots, &mut transformed, &mut plans, &mut signed_urls)?;
    if plans.next().is_some() || signed_urls.next().is_some() {
        return Err(ResultShapeError::FileUrlSigning(
            "file URL transformation left unused plan entries".to_owned(),
        ));
    }
    Ok(transformed)
}

fn plan_file_level(
    compiled: &[CompiledNode],
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    plans: &mut Vec<FileTransformPlan>,
) -> Result<(), ResultShapeError> {
    if compiled.len() != results.len() {
        return Err(ResultShapeError::Cardinality {
            scope: "form",
            expected: compiled.len(),
            actual: results.len(),
        });
    }
    for (node, result) in compiled.iter().zip(results) {
        for entity in &result.entities {
            if node.etype == FILES_ETYPE {
                plans.push(plan_file_fetch(node, &entity.fetch, catalog)?);
            }
            plan_file_level(&node.children, &entity.children, catalog, plans)?;
        }
    }
    Ok(())
}

fn plan_file_fetch(
    node: &CompiledNode,
    result: &DatalogResult,
    catalog: &AttributeCatalog,
) -> Result<FileTransformPlan, ResultShapeError> {
    let url_attr = catalog
        .forward(FILES_ETYPE, "url")
        .ok_or(ResultShapeError::MissingFileAttribute("url"))?;
    let location_attr = catalog
        .forward(FILES_ETYPE, "location-id")
        .ok_or(ResultShapeError::MissingFileAttribute("location-id"))?;
    let asks_for_url = node
        .options
        .fields
        .as_ref()
        .is_none_or(|fields| fields.iter().any(|field| field.as_str() == Some("url")));
    let exposes_location = node.options.fields.as_ref().is_none_or(|fields| {
        fields
            .iter()
            .any(|field| field.as_str() == Some("location-id"))
    });
    let anchor = asks_for_url
        .then(|| result.join_rows.iter().flatten().next().cloned())
        .flatten();
    let location = if anchor.is_some() {
        result
            .join_rows
            .iter()
            .flatten()
            .find(|triple| triple.attr_id == location_attr.id)
            .map(|triple| match &triple.value {
                DatalogValue::Json(Value::String(location)) => Ok(location.clone()),
                value => Err(ResultShapeError::InvalidFileLocation(format!("{value:?}"))),
            })
            .transpose()?
    } else {
        None
    };
    Ok(FileTransformPlan {
        url_attr_id: url_attr.id,
        location_attr_id: location_attr.id,
        exposes_location,
        anchor,
        location,
    })
}

fn apply_file_level(
    compiled: &[CompiledNode],
    results: &mut [ExecutedNode],
    plans: &mut impl Iterator<Item = FileTransformPlan>,
    signed_urls: &mut impl Iterator<Item = String>,
) -> Result<(), ResultShapeError> {
    if compiled.len() != results.len() {
        return Err(ResultShapeError::Cardinality {
            scope: "form",
            expected: compiled.len(),
            actual: results.len(),
        });
    }
    for (node, result) in compiled.iter().zip(results) {
        for entity in &mut result.entities {
            if node.etype == FILES_ETYPE {
                let plan = plans.next().ok_or_else(|| {
                    ResultShapeError::FileUrlSigning(
                        "file URL transformation plan ended early".to_owned(),
                    )
                })?;
                if let (Some(anchor), Some(_location)) = (plan.anchor, plan.location) {
                    let url = signed_urls.next().ok_or_else(|| {
                        ResultShapeError::FileUrlSigning(
                            "file URL signer result ended early".to_owned(),
                        )
                    })?;
                    entity.fetch.join_rows.push(vec![DatalogTriple {
                        entity_id: anchor.entity_id,
                        attr_id: plan.url_attr_id,
                        value: DatalogValue::Json(Value::String(url)),
                        created_at: anchor.created_at,
                    }]);
                }
                if !plan.exposes_location {
                    entity.fetch.join_rows.retain(|row| {
                        row.first()
                            .is_none_or(|triple| triple.attr_id != plan.location_attr_id)
                    });
                }
            }
            apply_file_level(&node.children, &mut entity.children, plans, signed_urls)?;
        }
    }
    Ok(())
}

/// Reconstructs the reference server's selector/entity `InstaQL` node hierarchy.
///
/// # Errors
///
/// Returns [`ResultShapeError`] when root or child result counts do not align with
/// their compiled forms.
pub fn collect_nodes(
    query: &CompiledQuery,
    results: Vec<ExecutedNode>,
) -> Result<Vec<InstaqlNode>, ResultShapeError> {
    collect_node_level(&query.roots, results)
}

fn collect_node_level(
    compiled: &[CompiledNode],
    results: Vec<ExecutedNode>,
) -> Result<Vec<InstaqlNode>, ResultShapeError> {
    if compiled.len() != results.len() {
        return Err(ResultShapeError::Cardinality {
            scope: "form",
            expected: compiled.len(),
            actual: results.len(),
        });
    }
    compiled
        .iter()
        .zip(results)
        .map(|(compiled, mut result)| {
            let child_nodes = result
                .entities
                .into_iter()
                .map(|entity| {
                    Ok(InstaqlNode {
                        data: InstaqlNodeData {
                            key: None,
                            etype: compiled.etype.clone(),
                            options: compiled.options.clone(),
                            datalog_result: entity.fetch,
                        },
                        child_nodes: collect_node_level(&compiled.children, entity.children)?,
                    })
                })
                .collect::<Result<Vec<_>, ResultShapeError>>()?;
            if let Some(count) = result.aggregate {
                result.selector.aggregate = Some(BTreeMap::from([("count".to_owned(), count)]));
            }
            Ok(InstaqlNode {
                data: InstaqlNodeData {
                    key: Some(compiled.key.clone()),
                    etype: compiled.etype.clone(),
                    options: compiled.options.clone(),
                    datalog_result: result.selector,
                },
                child_nodes,
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct InstaqlResponse {
    pub data: BTreeMap<String, Value>,
    pub page_info: BTreeMap<String, PageResultInfo>,
    pub aggregates: BTreeMap<String, BTreeMap<String, i64>>,
}

/// Converts executed nested groups into public entity objects plus root metadata.
///
/// # Errors
///
/// Returns [`ResultShapeError`] for mismatched result trees or unknown attributes.
pub fn shape_response(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    inference: bool,
) -> Result<InstaqlResponse, ResultShapeError> {
    if query.roots.len() != results.len() {
        return Err(ResultShapeError::Cardinality {
            scope: "root",
            expected: query.roots.len(),
            actual: results.len(),
        });
    }
    let mut response = InstaqlResponse::default();
    for (node, result) in query.roots.iter().zip(results) {
        response.data.insert(
            node.key.clone(),
            Value::Array(shape_entities(node, result, catalog, inference)?),
        );
        if let Some(page_info) = &result.selector.page_info {
            response
                .page_info
                .insert(node.key.clone(), page_info.clone());
        }
        if let Some(count) = result.aggregate {
            response.aggregates.insert(
                node.key.clone(),
                BTreeMap::from([("count".to_owned(), count)]),
            );
        }
    }
    Ok(response)
}

/// Applies `$files` virtual-field semantics and shapes the public response.
///
/// # Errors
///
/// Returns [`ResultShapeError`] for malformed result trees, URL signing
/// failures, or unknown attributes.
pub fn shape_response_with_file_urls<S: FileUrlSigner>(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    app_id: Uuid,
    signer: &S,
    inference: bool,
) -> Result<InstaqlResponse, ResultShapeError> {
    let transformed = transform_file_results(query, results, catalog, app_id, signer)?;
    shape_response(query, &transformed, catalog, inference)
}

/// Applies asynchronous batched file URL signing and shapes the public response.
///
/// # Errors
///
/// Returns an error for malformed result trees, URL signing failures, or unknown attributes.
pub async fn shape_response_with_file_urls_async<S: AsyncFileUrlSigner + ?Sized>(
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    app_id: Uuid,
    signer: &S,
    inference: bool,
) -> Result<InstaqlResponse, ResultShapeError> {
    let transformed = transform_file_results_async(query, results, catalog, app_id, signer).await?;
    shape_response(query, &transformed, catalog, inference)
}

fn shape_entities(
    node: &CompiledNode,
    result: &ExecutedNode,
    catalog: &AttributeCatalog,
    inference: bool,
) -> Result<Vec<Value>, ResultShapeError> {
    let entries = result
        .entities
        .iter()
        .map(|entity| {
            if node.children.len() != entity.children.len() {
                return Err(ResultShapeError::Cardinality {
                    scope: "child",
                    expected: node.children.len(),
                    actual: entity.children.len(),
                });
            }
            let mut object = entity_object(&entity.fetch, catalog)?;
            for (child, child_result) in node.children.iter().zip(&entity.children) {
                let values = shape_entities(child, child_result, catalog, inference)?;
                let value = if inference
                    && child
                        .relationship
                        .as_ref()
                        .is_some_and(CompiledRelationship::singular)
                {
                    values.into_iter().next().unwrap_or(Value::Null)
                } else {
                    Value::Array(values)
                };
                object.insert(child.key.clone(), value);
            }
            Ok(Value::Object(object))
        })
        .collect::<Result<Vec<_>, _>>()?;
    sort_entries(entries, node, catalog)
}

fn entity_object(
    result: &DatalogResult,
    catalog: &AttributeCatalog,
) -> Result<Map<String, Value>, ResultShapeError> {
    let mut object = Map::new();
    for triple in result.join_rows.iter().flatten() {
        let attribute = catalog
            .by_id(triple.attr_id)
            .ok_or(ResultShapeError::UnknownAttribute(triple.attr_id))?;
        object.insert(attribute.label.clone(), datalog_value(&triple.value));
        if attribute.label == "id" {
            object.insert(
                "$serverCreatedAt".to_owned(),
                Value::from(triple.created_at),
            );
        }
    }
    Ok(object)
}

fn datalog_value(value: &DatalogValue) -> Value {
    match value {
        DatalogValue::Uuid(value) => Value::String(value.to_string()),
        DatalogValue::Json(value) => value.clone(),
    }
}

fn sort_entries(
    entries: Vec<Value>,
    node: &CompiledNode,
    catalog: &AttributeCatalog,
) -> Result<Vec<Value>, ResultShapeError> {
    let (field, descending, date_order) = sort_spec(node, catalog);
    let now = chrono::Utc::now();
    let mut keyed = entries
        .into_iter()
        .map(|entry| {
            let object = entry.as_object().expect("entity entries are objects");
            let date = date_order
                .then(|| {
                    let value = object.get(field).unwrap_or(&Value::Null);
                    if value.is_null() {
                        Ok(None)
                    } else {
                        parse_date_value(value, now).map(Some).ok_or_else(|| {
                            ResultShapeError::InvalidDateSortValue(value.to_string())
                        })
                    }
                })
                .transpose()?
                .flatten();
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .unwrap_or_else(Uuid::new_v4);
            Ok((entry, date, id))
        })
        .collect::<Result<Vec<_>, ResultShapeError>>()?;
    keyed.sort_by(
        |(left, left_date, left_id), (right, right_date, right_id)| {
            let left_object = left.as_object().expect("entity entries are objects");
            let right_object = right.as_object().expect("entity entries are objects");
            let order = if date_order {
                compare_dates(*left_date, *right_date)
            } else {
                compare_json(left_object.get(field), right_object.get(field))
            }
            .then_with(|| left_id.as_bytes().cmp(right_id.as_bytes()));
            if descending { order.reverse() } else { order }
        },
    );
    let mut sorted: Vec<Value> = Vec::with_capacity(keyed.len());
    let mut previous_key: Option<(Option<DateValue>, Value, Uuid)> = None;
    for (mut entry, date, id) in keyed {
        let field_value = entry
            .as_object()
            .expect("entity entries are objects")
            .get(field)
            .cloned()
            .unwrap_or(Value::Null);
        let duplicate =
            previous_key
                .as_ref()
                .is_some_and(|(previous_date, previous, previous_id)| {
                    let fields_equal = if date_order {
                        previous_date == &date
                    } else {
                        compare_json(Some(previous), Some(&field_value)) == Ordering::Equal
                    };
                    fields_equal && previous_id == &id
                });
        previous_key = Some((date, field_value, id));
        let duplicate_public_entry = duplicate
            && sorted.last().is_some_and(|previous| {
                let previous = previous.as_object().expect("entity entries are objects");
                let current = entry.as_object().expect("entity entries are objects");
                previous.get("id") == current.get("id")
            });
        entry
            .as_object_mut()
            .expect("entity entries are objects")
            .remove("$serverCreatedAt");
        if duplicate_public_entry {
            *sorted.last_mut().expect("duplicate has a predecessor") = entry;
        } else {
            sorted.push(entry);
        }
    }
    Ok(sorted)
}

fn sort_spec<'a>(node: &'a CompiledNode, catalog: &AttributeCatalog) -> (&'a str, bool, bool) {
    let order = node.options.order.as_ref();
    let field = order.map_or("$serverCreatedAt", |order| {
        if order.field == "serverCreatedAt" {
            "$serverCreatedAt"
        } else {
            &order.field
        }
    });
    let descending =
        order.is_some_and(|order| matches!(order.direction, instant_datalog::OrderDirection::Desc));
    let date_order = order
        .filter(|order| order.field != "serverCreatedAt")
        .and_then(|order| catalog.forward(&node.etype, &order.field))
        .is_some_and(|attribute| {
            attribute.checked_data_type == Some(instant_datalog::DataType::Date)
        });
    (field, descending, date_order)
}

fn compare_dates(left: Option<DateValue>, right: Option<DateValue>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => left.cmp(&right),
    }
}

fn compare_json(left: Option<&Value>, right: Option<&Value>) -> Ordering {
    match (left, right) {
        (None | Some(Value::Null), None | Some(Value::Null)) => Ordering::Equal,
        (None | Some(Value::Null), _) => Ordering::Less,
        (_, None | Some(Value::Null)) => Ordering::Greater,
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left.cmp(right),
        (Some(Value::Number(left)), Some(Value::Number(right))) => {
            compare_json_numbers(left, right)
        }
        (Some(Value::String(left)), Some(Value::String(right))) => {
            left.encode_utf16().cmp(right.encode_utf16())
        }
        (Some(left), Some(right)) => left.to_string().cmp(&right.to_string()),
    }
}

fn compare_json_numbers(left: &serde_json::Number, right: &serde_json::Number) -> Ordering {
    match (left.as_i64(), left.as_u64(), right.as_i64(), right.as_u64()) {
        (Some(left), _, Some(right), _) => return left.cmp(&right),
        (Some(left), _, None, Some(right)) => {
            return if left < 0 {
                Ordering::Less
            } else {
                left.cast_unsigned().cmp(&right)
            };
        }
        (None, Some(left), Some(right), _) => {
            return if right < 0 {
                Ordering::Greater
            } else {
                left.cmp(&right.cast_unsigned())
            };
        }
        (None, Some(left), None, Some(right)) => return left.cmp(&right),
        _ => {}
    }

    // `serde_json` retains arbitrary-precision number text in this workspace.
    // Compare integral values from that text so values outside u64 (and, most
    // importantly, ordinary 64-bit values above f64's exact range) retain the
    // same ordering as Clojure's numeric `compare`.
    let left_text = left.to_string();
    let right_text = right.to_string();
    if let (Some(left), Some(right)) = (
        json_integer_parts(&left_text),
        json_integer_parts(&right_text),
    ) {
        return compare_integer_parts(left, right);
    }

    left.as_f64()
        .partial_cmp(&right.as_f64())
        .unwrap_or(Ordering::Equal)
}

fn json_integer_parts(value: &str) -> Option<(bool, &str)> {
    let (negative, digits) = value
        .strip_prefix('-')
        .map_or((false, value), |digits| (true, digits));
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    Some((negative && !digits.is_empty(), digits))
}

fn compare_integer_parts(left: (bool, &str), right: (bool, &str)) -> Ordering {
    let (left_negative, left_digits) = left;
    let (right_negative, right_digits) = right;
    match (left_negative, right_negative) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left_digits
            .len()
            .cmp(&right_digits.len())
            .then_with(|| left_digits.cmp(right_digits)),
        (true, true) => right_digits
            .len()
            .cmp(&left_digits.len())
            .then_with(|| right_digits.cmp(left_digits)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relationship_singularity_matches_forward_cardinality_and_reverse_uniqueness() {
        let relationship = |forward, cardinality, unique| CompiledRelationship {
            attr_id: Uuid::nil(),
            forward,
            cardinality,
            unique,
            parent_symbol: Symbol::new("?parent").unwrap(),
            child_symbol: Symbol::new("?child").unwrap(),
        };
        assert!(relationship(true, Cardinality::One, false).singular());
        assert!(!relationship(true, Cardinality::Many, true).singular());
        assert!(relationship(false, Cardinality::Many, true).singular());
    }

    #[test]
    fn nested_children_inherit_parent_join_cardinality() {
        let app_id = Uuid::new_v4();
        let parent_attr = Uuid::new_v4();
        let child_attr = Uuid::new_v4();
        let parent = Symbol::new("?parent").unwrap();
        let child = Symbol::new("?child").unwrap();
        let pattern = |entity: Symbol, attr_id: Uuid, value: RawTerm| NormalizedClause::Pattern {
            pattern: RawPattern::new(
                Index::new(IndexKey::Eav),
                vec![
                    RawTerm::Variable(entity),
                    RawTerm::Constant(Value::String(attr_id.to_string())),
                    value,
                ],
            )
            .normalize()
            .unwrap(),
        };
        let groups = vec![NestedPatternGroup {
            clauses: vec![pattern(parent.clone(), parent_attr, RawTerm::Any)],
            missing: false,
            page: None,
            aggregate: None,
            children: Some(NestedPatternChildren {
                join_symbol: parent.clone(),
                groups: vec![NestedPatternGroup {
                    clauses: vec![pattern(parent, child_attr, RawTerm::Variable(child))],
                    missing: false,
                    page: None,
                    aggregate: None,
                    children: None,
                }],
            }),
        }];
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(parent_attr, 5), (child_attr, 100)]),
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        };
        let mut state = NestedPlannerState::default();

        let planned = plan_group_level(
            &groups,
            &mut state,
            "m-",
            app_id,
            &options,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(planned[0].ensure_empty_symbol_values_for_topics, Some(true));
        assert_eq!(
            planned[0].children.as_ref().unwrap().groups[0].ensure_empty_symbol_values_for_topics,
            Some(true)
        );
        assert!(state.hints.contains(&QueryHint::Rows {
            tables: vec!["m-1".to_owned(), "t2".to_owned()],
            estimate: 5,
        }));
    }

    #[test]
    fn checked_dates_sort_by_instant_then_postgres_uuid_and_deduplicate_keys() {
        let date_attr_id = Uuid::new_v4();
        let catalog = AttributeCatalog {
            attributes: vec![super::super::Attribute {
                id: date_attr_id,
                wire: None,
                etype: "events".to_owned(),
                label: "happenedAt".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: false,
                setting_unique: false,
                indexed: true,
                required: false,
                checked_data_type: Some(instant_datalog::DataType::Date),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        };
        let node = CompiledNode {
            key: "events".to_owned(),
            etype: "events".to_owned(),
            level: 0,
            options: FormOptions {
                order: Some(super::super::Order {
                    field: "happenedAt".to_owned(),
                    direction: instant_datalog::OrderDirection::Asc,
                }),
                ..FormOptions::default()
            },
            selector_clauses: Vec::new(),
            missing: false,
            page: None,
            aggregate: None,
            fetch_attr_ids: vec![date_attr_id],
            relationship: None,
            children: Vec::new(),
        };
        let low_id = "00000000-0000-0000-0000-000000000001";
        let high_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let entries = vec![
            json!({"id": high_id, "happenedAt": "2025-01-02T00:00:00-08", "$serverCreatedAt": 3}),
            json!({"id": high_id, "happenedAt": "2025-01-02T04:00:00Z", "$serverCreatedAt": 2, "version": 1}),
            json!({"id": low_id, "happenedAt": "2025-01-02T04:00:00Z", "$serverCreatedAt": 1}),
            json!({"id": high_id, "happenedAt": "2025-01-02T04:00:00Z", "$serverCreatedAt": 4, "version": 2}),
        ];

        let sorted = sort_entries(entries, &node, &catalog).unwrap();

        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0]["id"], low_id);
        assert_eq!(sorted[1]["version"], 2, "the last duplicate sort key wins");
        assert_eq!(sorted[2]["happenedAt"], "2025-01-02T00:00:00-08");
        assert!(
            sorted
                .iter()
                .all(|entry| entry.get("$serverCreatedAt").is_none())
        );
    }

    #[test]
    fn dependency_helpers_collect_recursive_precise_topics_and_generated_fetch_topics() {
        let selector_attr = Uuid::new_v4();
        let fetch_attr = Uuid::new_v4();
        let precise = InvalidationTopic {
            indexes: vec![IndexKey::Eav],
            entity: TopicPart::Any,
            attribute: TopicPart::Values(vec![DatalogValue::Uuid(selector_attr)]),
            value: TopicPart::Any,
        };
        let results = vec![ExecutedNode {
            selector: DatalogResult {
                topics: vec![precise.clone()],
                ..DatalogResult::default()
            },
            entities: vec![ExecutedEntity {
                fetch: DatalogResult {
                    topics: vec![precise.clone()],
                    ..DatalogResult::default()
                },
                children: vec![ExecutedNode {
                    selector: DatalogResult {
                        topics: vec![precise.clone()],
                        ..DatalogResult::default()
                    },
                    ..ExecutedNode::default()
                }],
            }],
            ..ExecutedNode::default()
        }];
        assert_eq!(collect_invalidation_topics(&results), vec![precise]);

        let query = CompiledQuery {
            roots: vec![CompiledNode {
                key: "posts".to_owned(),
                etype: "posts".to_owned(),
                level: 0,
                options: FormOptions::default(),
                selector_clauses: Vec::new(),
                missing: false,
                page: None,
                aggregate: None,
                fetch_attr_ids: vec![fetch_attr],
                relationship: None,
                children: Vec::new(),
            }],
        };
        let coarse = coarse_invalidation_topics(&query).unwrap();
        assert_eq!(coarse.len(), 1);
        assert_eq!(coarse[0].indexes, vec![IndexKey::Ea]);
        assert_eq!(
            coarse[0].attribute,
            TopicPart::Values(vec![DatalogValue::Uuid(fetch_attr)])
        );
    }

    #[test]
    fn aggregate_results_retain_pattern_dependencies_without_rows() {
        let attr = Uuid::new_v4();
        let clause = NormalizedClause::Pattern {
            pattern: RawPattern::new(
                Index::new(IndexKey::Ea),
                vec![
                    RawTerm::Variable(Symbol::new("?entity").unwrap()),
                    RawTerm::Constant(Value::String(attr.to_string())),
                    RawTerm::Constant(Value::String("pending".to_owned())),
                ],
            )
            .normalize()
            .unwrap(),
        };
        let query = CompiledQuery {
            roots: vec![CompiledNode {
                key: "tasks".to_owned(),
                etype: "tasks".to_owned(),
                level: 0,
                options: FormOptions::default(),
                selector_clauses: vec![clause],
                missing: false,
                page: None,
                aggregate: Some(Aggregate::Count),
                fetch_attr_ids: Vec::new(),
                relationship: None,
                children: Vec::new(),
            }],
        };
        let results = vec![ExecutedNode {
            aggregate: Some(0),
            ..ExecutedNode::default()
        }];

        let topics = collect_query_invalidation_topics(&query, &results).unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].indexes, vec![IndexKey::Ea]);
        assert_eq!(
            topics[0].attribute,
            TopicPart::Values(vec![DatalogValue::Uuid(attr)])
        );
        assert_eq!(
            topics[0].value,
            TopicPart::Values(vec![DatalogValue::Json(Value::String(
                "pending".to_owned()
            ))])
        );
    }

    #[test]
    fn nested_coarse_topics_include_selector_and_generated_fetch_dependencies() {
        let root_attr = Uuid::new_v4();
        let child_attr = Uuid::new_v4();
        let root_clause = NormalizedClause::Pattern {
            pattern: RawPattern::new(
                Index::new(IndexKey::Eav),
                vec![
                    RawTerm::Variable(Symbol::new("?users-0").unwrap()),
                    RawTerm::Constant(Value::String(root_attr.to_string())),
                    RawTerm::Constant(Value::String("active".to_owned())),
                ],
            )
            .normalize()
            .unwrap(),
        };
        let child_clause = NormalizedClause::Pattern {
            pattern: RawPattern::new(
                Index::new(IndexKey::Eav),
                vec![
                    RawTerm::Variable(Symbol::new("?posts-1").unwrap()),
                    RawTerm::Constant(Value::String(child_attr.to_string())),
                    RawTerm::Constant(Value::String("published".to_owned())),
                ],
            )
            .normalize()
            .unwrap(),
        };
        let query = CompiledQuery {
            roots: vec![CompiledNode {
                key: "users".to_owned(),
                etype: "users".to_owned(),
                level: 0,
                options: FormOptions::default(),
                selector_clauses: vec![root_clause],
                missing: false,
                page: None,
                aggregate: None,
                fetch_attr_ids: vec![root_attr],
                relationship: None,
                children: vec![CompiledNode {
                    key: "posts".to_owned(),
                    etype: "posts".to_owned(),
                    level: 1,
                    options: FormOptions::default(),
                    selector_clauses: vec![child_clause],
                    missing: false,
                    page: None,
                    aggregate: None,
                    fetch_attr_ids: vec![child_attr],
                    relationship: None,
                    children: Vec::new(),
                }],
            }],
        };

        let topics = coarse_invalidation_topics(&query).unwrap();
        assert_eq!(topics.len(), 4);
        assert_eq!(topics[0].indexes, vec![IndexKey::Eav]);
        assert_eq!(
            topics[0].attribute,
            TopicPart::Values(vec![DatalogValue::Uuid(root_attr)])
        );
        assert_eq!(
            topics[0].value,
            TopicPart::Values(vec![DatalogValue::Json(Value::String("active".to_owned()))])
        );
        assert_eq!(topics[1].indexes, vec![IndexKey::Ea]);
        assert_eq!(
            topics[1].attribute,
            TopicPart::Values(vec![DatalogValue::Uuid(root_attr)])
        );
        assert_eq!(topics[2].indexes, vec![IndexKey::Eav]);
        assert_eq!(
            topics[2].attribute,
            TopicPart::Values(vec![DatalogValue::Uuid(child_attr)])
        );
        assert_eq!(
            topics[2].value,
            TopicPart::Values(vec![DatalogValue::Json(Value::String(
                "published".to_owned()
            ))])
        );
        assert_eq!(topics[3].indexes, vec![IndexKey::Ea]);
        assert_eq!(
            topics[3].attribute,
            TopicPart::Values(vec![DatalogValue::Uuid(child_attr)])
        );
    }
}
