use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::BuildHasher;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BindingPath, BindingTree, ComparatorOperation, Component, DataType, FunctionClause, Index,
    IndexKey, JoinCondition, JoinPredicate, NestedSymbolMap, NormalizedClause, PagePlanMetadata,
    Pattern, PatternError, PredicateOperator, PredicateOptions, QueryHint, SqlColumn,
    SqlExpression, SqlFunction, Symbol, Term, WhereColumn, WhereColumnRef, WhereExpression,
    WherePredicate, constant_predicate, function_predicates, join_conditions, merge_or_symbol_maps,
    nested_join_predicates, or_gather_predicate, validate_clause_joinable,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PatternWhereOptions {
    pub remove_unnecessary_index: bool,
    pub predicate_options: PredicateOptions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PatternWherePlan {
    pub predicate: WherePredicate,
    pub hints: Vec<QueryHint>,
}

/// Assembles constants, functions, index guards, and joins for one match CTE.
///
/// # Errors
///
/// Returns `PatternError` when a normalized constant or function operand violates
/// the component contract.
pub fn pattern_where_plan(
    options: PatternWhereOptions,
    app_id: Uuid,
    triples_alias: &str,
    pattern: &Pattern,
    additional_predicates: Vec<WherePredicate>,
) -> Result<PatternWherePlan, PatternError> {
    let mut hints = Vec::new();
    let attribute = if let Term::Constants(values) = &pattern.attribute {
        let plan = constant_predicate(
            triples_alias,
            pattern.index,
            app_id,
            Component::Attribute,
            values,
        )?;
        hints.extend(plan.hints);
        Some(plan.predicate)
    } else {
        None
    };

    let mut rest = Vec::new();
    for (component, term) in [
        (Component::Entity, &pattern.entity),
        (Component::Value, &pattern.value),
    ] {
        if let Term::Constants(values) = term {
            let plan = constant_predicate(triples_alias, pattern.index, app_id, component, values)?;
            rest.push(plan.predicate);
            hints.extend(plan.hints);
        }
    }
    rest.extend(function_predicates(
        options.predicate_options,
        app_id,
        triples_alias,
        pattern.index,
        &pattern.entity,
        &pattern.value,
    )?);
    rest.extend(additional_predicates.into_iter().map(|predicate| {
        if pattern.index.key == IndexKey::Av {
            patch_av_origin_value(predicate)
        } else {
            predicate
        }
    }));

    let mut predicates = vec![WherePredicate::equal(
        WhereExpression::column(WhereColumn::AppId),
        WhereExpression::Uuid(app_id),
    )];
    if !rest.is_empty() || !options.remove_unnecessary_index {
        predicates.push(WherePredicate::equal(
            WhereExpression::column(WhereColumn::Index(pattern.index.key)),
            WhereExpression::Literal(Value::Bool(true)),
        ));
    }
    if let Some(attribute) = attribute {
        predicates.push(attribute);
    }
    predicates.extend(rest);

    Ok(PatternWherePlan {
        predicate: WherePredicate::And(predicates),
        hints,
    })
}

fn patch_av_origin_value(predicate: WherePredicate) -> WherePredicate {
    match predicate {
        WherePredicate::Compare {
            operator,
            left:
                WhereExpression::Column(WhereColumnRef {
                    qualifier: None,
                    column: WhereColumn::Value,
                }),
            right,
        } => WherePredicate::Compare {
            operator,
            left: WhereExpression::function(
                SqlFunction::JsonNullToNull,
                WhereExpression::column(WhereColumn::Value),
            ),
            right,
        },
        predicate => predicate,
    }
}

fn join_predicate_to_where(predicate: JoinPredicate) -> WherePredicate {
    match predicate {
        JoinPredicate::Equals(condition) => join_condition_to_where(condition),
        JoinPredicate::And(predicates) => WherePredicate::And(
            predicates
                .into_iter()
                .map(join_predicate_to_where)
                .collect(),
        ),
        JoinPredicate::Or(predicates) => WherePredicate::Or(
            predicates
                .into_iter()
                .map(join_predicate_to_where)
                .collect(),
        ),
    }
}

fn join_condition_to_where(condition: JoinCondition) -> WherePredicate {
    WherePredicate::compare(
        PredicateOperator::Equal,
        join_expression_to_where(condition.left),
        join_expression_to_where(condition.right),
    )
}

fn join_expression_to_where(expression: SqlExpression) -> WhereExpression {
    match expression {
        SqlExpression::Column(column) => {
            let (where_column, suffix) = match column.column {
                SqlColumn::EntityId => (WhereColumn::EntityId, "entity-id"),
                SqlColumn::AttrId => (WhereColumn::AttrId, "attr-id"),
                SqlColumn::Value => (WhereColumn::Value, "value"),
            };
            match column.qualifier {
                Some(qualifier) => WhereExpression::NamedColumn(format!("{qualifier}-{suffix}")),
                None => WhereExpression::column(where_column),
            }
        }
        SqlExpression::JsonUuidToUuid(expression) => WhereExpression::function(
            SqlFunction::JsonUuidToUuid,
            join_expression_to_where(*expression),
        ),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectProjection {
    pub expression: WhereExpression,
    pub alias: Option<String>,
}

impl SelectProjection {
    fn to_honey_sql_json(&self) -> Value {
        match &self.alias {
            Some(alias) => json!([self.expression.to_honey_sql_json(), format!(":{alias}")]),
            None => self.expression.to_honey_sql_json(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FromTable {
    pub table: String,
    pub alias: Option<String>,
}

impl FromTable {
    fn to_honey_sql_json(&self) -> Value {
        match &self.alias {
            Some(alias) => json!([format!(":{}", self.table), format!(":{alias}")]),
            None => Value::String(format!(":{}", self.table)),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Materialization {
    Materialized,
    NotMaterialized,
}

impl Materialization {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Materialized => "materialized",
            Self::NotMaterialized => "not-materialized",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchCte {
    pub name: String,
    pub select: Vec<SelectProjection>,
    pub from: Vec<FromTable>,
    pub predicate: WherePredicate,
    pub materialization: Materialization,
}

impl MatchCte {
    /// Serializes into the stable JSON analogue used by compatibility fixtures.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        json!([
            format!(":{}", self.name),
            {
                ":select": self.select.iter().map(SelectProjection::to_honey_sql_json).collect::<Vec<_>>(),
                ":from": self.from.iter().map(FromTable::to_honey_sql_json).collect::<Vec<_>>(),
                ":where": self.predicate.to_honey_sql_json()
            },
            format!(":{}", self.materialization.as_str())
        ])
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CteJoin {
    pub table: String,
    pub predicate: Option<WherePredicate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OrGatherCte {
    pub name: String,
    pub select: Vec<SelectProjection>,
    pub from: FromTable,
    pub full_joins: Vec<CteJoin>,
    pub left_joins: Vec<CteJoin>,
    pub predicate: Option<WherePredicate>,
    pub materialization: Materialization,
}

impl OrGatherCte {
    /// Serializes into the stable JSON analogue used by compatibility fixtures.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        let mut query = serde_json::Map::new();
        query.insert(
            ":select".to_owned(),
            Value::Array(
                self.select
                    .iter()
                    .map(SelectProjection::to_honey_sql_json)
                    .collect(),
            ),
        );
        query.insert(":from".to_owned(), self.from.to_honey_sql_json());
        if !self.full_joins.is_empty() {
            query.insert(":full-join".to_owned(), flattened_joins(&self.full_joins));
        }
        if !self.left_joins.is_empty() {
            query.insert(":left-join".to_owned(), flattened_joins(&self.left_joins));
        }
        if let Some(predicate) = &self.predicate {
            query.insert(":where".to_owned(), predicate.to_honey_sql_json());
        }
        Value::Array(vec![
            Value::String(format!(":{}", self.name)),
            Value::Object(query),
            Value::String(format!(":{}", self.materialization.as_str())),
        ])
    }
}

fn flattened_joins(joins: &[CteJoin]) -> Value {
    let mut values = Vec::with_capacity(joins.len() * 2);
    for join in joins {
        values.push(Value::String(format!(":{}", join.table)));
        values.push(
            join.predicate
                .as_ref()
                .map_or(Value::Null, WherePredicate::to_honey_sql_json),
        );
    }
    Value::Array(values)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum PlannedCte {
    Match(MatchCte),
    OrGather(OrGatherCte),
    Select(SelectCte),
}

impl PlannedCte {
    /// Serializes into the stable JSON analogue used by compatibility fixtures.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        match self {
            Self::Match(cte) => cte.to_honey_sql_json(),
            Self::OrGather(cte) => cte.to_honey_sql_json(),
            Self::Select(cte) => cte.to_honey_sql_json(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderDirection {
    Asc,
    Desc,
}

impl OrderDirection {
    #[must_use]
    pub const fn reverse(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NullOrder {
    First,
    Last,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OrderBy {
    pub expression: WhereExpression,
    pub direction: OrderDirection,
    pub nulls: Option<NullOrder>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum SelectExpression {
    Expression(WhereExpression),
    Exists(Box<SelectStatement>),
    RowNumber,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatementProjection {
    pub expression: SelectExpression,
    pub alias: Option<String>,
}

impl From<SelectProjection> for StatementProjection {
    fn from(projection: SelectProjection) -> Self {
        Self {
            expression: SelectExpression::Expression(projection.expression),
            alias: projection.alias,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum SelectSource {
    Table(FromTable),
    Subquery {
        query: Box<SelectStatement>,
        alias: String,
    },
    Lateral {
        query: Box<SelectStatement>,
        alias: String,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectStatement {
    pub distinct_on: Vec<WhereExpression>,
    pub select: Vec<StatementProjection>,
    pub from: Vec<SelectSource>,
    pub predicate: Option<WherePredicate>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectCte {
    pub name: String,
    pub statement: SelectStatement,
    pub materialization: Materialization,
}

impl SelectCte {
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        // Pagination compatibility is asserted at rendered-SQL level. Keeping this
        // representation explicit still makes debug snapshots useful.
        json!([
            format!(":{}", self.name),
            serde_json::to_value(&self.statement).unwrap_or(Value::Null),
            format!(":{}", self.materialization.as_str())
        ])
    }
}

/// Builds the CTE that gathers all branches of one OR clause.
///
/// # Errors
///
/// Returns `PatternError::EmptyOrClause` when no branch CTEs were produced.
pub fn build_or_gather_cte(
    prefix: &str,
    previous_index: Option<usize>,
    gather_index: usize,
    branch_last_indices: &[usize],
    symbol_map: &NestedSymbolMap,
    branch_symbol_maps: &[(usize, NestedSymbolMap)],
    join_symbol: &Symbol,
) -> Result<OrGatherCte, PatternError> {
    let Some(first_branch_index) = branch_last_indices.first().copied() else {
        return Err(PatternError::EmptyOrClause);
    };
    let table_name = indexed_table(prefix, gather_index);
    let previous_table = previous_index.map(|index| indexed_table(prefix, index));
    let branch_tables: Vec<_> = branch_last_indices
        .iter()
        .map(|index| indexed_table(prefix, *index))
        .collect();

    let mut select = Vec::new();
    if let Some(previous_table) = &previous_table {
        select.push(SelectProjection {
            expression: WhereExpression::Wildcard(Some(previous_table.clone())),
            alias: None,
        });
    }
    select.extend(branch_tables.iter().map(|table| SelectProjection {
        expression: WhereExpression::Wildcard(Some(table.clone())),
        alias: None,
    }));
    select.push(SelectProjection {
        expression: WhereExpression::Coalesce(
            branch_tables
                .iter()
                .map(|table| WhereExpression::NamedColumn(format!("{table}.{table}-entity-id")))
                .collect(),
        ),
        alias: Some(format!("{table_name}-entity-id")),
    });

    let false_join = || {
        WherePredicate::equal(
            WhereExpression::NamedColumn("0".to_owned()),
            WhereExpression::NamedColumn("1".to_owned()),
        )
    };
    let (from, full_joins, left_joins, predicate) = if let Some(previous_table) = previous_table {
        let left_joins = branch_symbol_maps
            .iter()
            .map(|(branch_index, branch_symbol_map)| CteJoin {
                table: indexed_table(prefix, *branch_index),
                predicate: or_gather_predicate(
                    prefix,
                    symbol_map,
                    std::slice::from_ref(branch_symbol_map),
                    join_symbol,
                )
                .map(join_predicate_to_where),
            })
            .collect();
        let branch_maps: Vec<_> = branch_symbol_maps
            .iter()
            .map(|(_, symbol_map)| symbol_map.clone())
            .collect();
        (
            FromTable {
                table: previous_table,
                alias: None,
            },
            Vec::new(),
            left_joins,
            or_gather_predicate(prefix, symbol_map, &branch_maps, join_symbol)
                .map(join_predicate_to_where),
        )
    } else {
        (
            FromTable {
                table: indexed_table(prefix, first_branch_index),
                alias: None,
            },
            branch_last_indices
                .iter()
                .skip(1)
                .map(|index| CteJoin {
                    table: indexed_table(prefix, *index),
                    predicate: Some(false_join()),
                })
                .collect(),
            Vec::new(),
            None,
        )
    };

    Ok(OrGatherCte {
        name: table_name,
        select,
        from,
        full_joins,
        left_joins,
        predicate,
        materialization: Materialization::NotMaterialized,
    })
}

fn indexed_table(prefix: &str, index: usize) -> String {
    format!("{}{index}", prefix.trim_start_matches(':'))
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct MatchCteOptions {
    pub page_info: bool,
    pub remove_unnecessary_index: bool,
    pub predicate_options: PredicateOptions,
    pub always_materialize_attr_ids: Vec<Uuid>,
    /// Suppresses cardinality hints at or above the live planner threshold.
    /// `None` preserves the reference default of 10,000 rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_hint_threshold: Option<u64>,
    /// Enables PostgreSQL planner annotations. `None` preserves the reference
    /// default of enabling hints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_hints_enabled: Option<bool>,
    /// Enables cardinality hints independently of index hints. `None`
    /// preserves the reference default of enabling row hints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_hints_enabled: Option<bool>,
    /// Applies the AV index uniqueness correction to value sketch estimates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_plan_av_fix: Option<bool>,
    /// Applies the selective LIKE/ILIKE cardinality heuristic rather than the
    /// legacy half-of-partition estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub like_count_optimization: Option<bool>,
    /// Keeps `$isNull: true` from being treated as an index-driving entity
    /// constraint. This is independent of the predicate SQL compatibility fix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_is_null_index_cost: Option<bool>,
    /// Caps cardinalities propagated to child groups by the public page limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_symbol_map_by_page_limit: Option<bool>,
    /// Forces the legacy scan hint for optional numeric root-page ordering.
    /// `None` preserves that legacy behavior; `Some(false)` leaves only this
    /// order scan to PostgreSQL while retaining filter and cardinality hints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional_number_order_scan_hints: Option<bool>,
    /// Opts small flat, bounded native pages into a statement-only JIT hint.
    /// `None` preserves reference planning and adds no setting hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_page_jit_off: Option<bool>,
    /// Restricts independent filter/order scans to a distinct candidate entity
    /// set obtained from a required indexed equality; narrow required created-at
    /// pages also materialize their filtered order join before sorting. This
    /// changes only the
    /// physical plan, not pattern order, result columns or invalidation topics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_entity_scans: Option<bool>,
    /// Leaves a known-small numeric optional page-order relation to the same
    /// candidate seed its driving relation already left-joins, instead of
    /// repeating the distinct candidate membership inside its scan. `None`
    /// preserves the membership form; this changes only the physical plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional_order_seed_binding: Option<bool>,
    /// Opts bounded native pages into keyed, pull-up-fenced entity probes and
    /// retains the existing selective candidate seed's physical index hints.
    /// `None` preserves reference planning. No fence limits candidate rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyed_small_page_scans: Option<bool>,
    /// Preserves empty non-reference bindings for later invalidation topics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ensure_empty_symbol_values_for_topics: Option<bool>,
    /// Queues this normalized query for native PostgreSQL hint shadow testing.
    /// The planner itself deliberately ignores this operational control.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_pg_hint_plan: Option<bool>,
    pub has_best_index: bool,
    pub attribute_cardinalities: BTreeMap<Uuid, u64>,
    pub attribute_value_cardinalities: BTreeMap<Uuid, BTreeMap<String, u64>>,
    pub attribute_entity_cardinalities: BTreeMap<Uuid, BTreeMap<String, u64>>,
    /// Exact `$isNull` estimates, keyed first by the id attribute used by the
    /// outer pattern and then by the attribute tested by the subquery.
    pub attribute_is_null_cardinalities: BTreeMap<Uuid, BTreeMap<Uuid, IsNullCardinalities>>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IsNullCardinalities {
    pub null: u64,
    pub not_null: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MatchCtePlan {
    pub cte: MatchCte,
    pub hints: Vec<QueryHint>,
    pub pattern: Pattern,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CostColumn {
    Entity,
    Attribute,
    Value,
    CreatedAt,
}

#[derive(Clone, Copy)]
struct IndexConfig {
    name: &'static str,
    columns: &'static [CostColumn],
    unique_columns: &'static [CostColumn],
    index_key: Option<IndexKey>,
    data_type: Option<DataType>,
}

const ENTITY: CostColumn = CostColumn::Entity;
const ATTRIBUTE: CostColumn = CostColumn::Attribute;
const VALUE: CostColumn = CostColumn::Value;
const CREATED_AT: CostColumn = CostColumn::CreatedAt;

const INDEX_CONFIGS: &[IndexConfig] = &[
    IndexConfig {
        name: "ea_index",
        columns: &[ENTITY, ATTRIBUTE],
        unique_columns: &[ENTITY],
        index_key: Some(IndexKey::Ea),
        data_type: None,
    },
    IndexConfig {
        name: "triples_pkey",
        columns: &[ENTITY, ATTRIBUTE],
        unique_columns: &[ENTITY],
        index_key: None,
        data_type: None,
    },
    IndexConfig {
        name: "ave_with_e_index",
        columns: &[ATTRIBUTE, VALUE, ENTITY],
        unique_columns: &[ENTITY],
        index_key: Some(IndexKey::Ave),
        data_type: None,
    },
    IndexConfig {
        name: "eav_uuid_index",
        columns: &[ENTITY, ATTRIBUTE, VALUE],
        unique_columns: &[],
        index_key: Some(IndexKey::Eav),
        data_type: None,
    },
    IndexConfig {
        name: "triples_string_trgm_gist_idx",
        columns: &[ATTRIBUTE, VALUE],
        unique_columns: &[],
        index_key: Some(IndexKey::Ave),
        data_type: Some(DataType::String),
    },
    IndexConfig {
        name: "vae_uuid_index",
        columns: &[VALUE, ATTRIBUTE, ENTITY],
        unique_columns: &[ENTITY],
        index_key: Some(IndexKey::Vae),
        data_type: None,
    },
    IndexConfig {
        name: "triples_created_at_idx",
        columns: &[ATTRIBUTE, CREATED_AT],
        unique_columns: &[],
        index_key: None,
        data_type: None,
    },
    IndexConfig {
        name: "av_index",
        columns: &[ATTRIBUTE, VALUE, ENTITY],
        unique_columns: &[VALUE, ENTITY],
        index_key: Some(IndexKey::Av),
        data_type: None,
    },
    IndexConfig {
        name: "triples_number_type_idx",
        columns: &[ATTRIBUTE, VALUE],
        unique_columns: &[],
        index_key: Some(IndexKey::Ave),
        data_type: Some(DataType::Number),
    },
    IndexConfig {
        name: "triples_boolean_type_idx",
        columns: &[ATTRIBUTE, VALUE],
        unique_columns: &[],
        index_key: Some(IndexKey::Ave),
        data_type: Some(DataType::Boolean),
    },
    IndexConfig {
        name: "triples_date_type_idx",
        columns: &[ATTRIBUTE, VALUE],
        unique_columns: &[],
        index_key: Some(IndexKey::Ave),
        data_type: Some(DataType::Date),
    },
];

#[derive(Clone)]
struct IndexCost {
    config: IndexConfig,
    cost: u128,
    path: Vec<(CostColumn, u64)>,
    known_remaining: HashMap<CostColumn, u64>,
    join_components: HashMap<CostColumn, u64>,
    matching_index_key: bool,
    matching_data_type: bool,
}

#[derive(Clone, Copy)]
pub struct MatchCteContext<'a, S> {
    pub prefix: &'a str,
    pub app_id: Uuid,
    pub pattern_index: usize,
    pub start_of_group: bool,
    pub symbol_map: &'a NestedSymbolMap,
    pub symbol_cardinalities: &'a HashMap<Symbol, u64>,
    pub additional_joins: &'a HashMap<Symbol, BindingPath, S>,
    pub options: &'a MatchCteOptions,
    pub row_estimate: Option<u64>,
    pub suppress_row_hint: bool,
}

/// Builds one CTE in the linear/nested Datalog match plan.
///
/// # Errors
///
/// Returns `PatternError` when constants or function operands are invalid.
#[allow(clippy::too_many_lines)] // Index hints and row hints share the CTE's transformed joins.
pub fn build_match_cte<S: BuildHasher>(
    context: &MatchCteContext<'_, S>,
    pattern: &Pattern,
) -> Result<MatchCtePlan, PatternError> {
    let prefix = context.prefix;
    let app_id = context.app_id;
    let pattern_index = context.pattern_index;
    let start_of_group = context.start_of_group;
    let symbol_map = context.symbol_map;
    let additional_joins = context.additional_joins;
    let options = context.options;
    let pattern = transform_for_ref_joins(symbol_map, pattern);
    let pg_hints_enabled = options.pg_hints_enabled.unwrap_or(true);
    let chosen_index = pg_hints_enabled
        .then(|| best_index(&pattern, options, context.symbol_cardinalities))
        .flatten();
    let table_name = format!("{}{pattern_index}", prefix.trim_start_matches(':'));
    let triples_alias = format!("t{pattern_index}");
    let previous_table = (!start_of_group)
        .then(|| pattern_index.checked_sub(1))
        .flatten()
        .map(|index| format!("{}{index}", prefix.trim_start_matches(':')));

    let mut joins: Vec<WherePredicate> = if start_of_group {
        Vec::new()
    } else {
        nested_join_predicates(prefix, pattern_index, symbol_map, &pattern)
            .into_iter()
            .map(join_predicate_to_where)
            .collect()
    };
    let parent_symbol_map: HashMap<_, Vec<_>> = additional_joins
        .iter()
        .map(|(symbol, path)| (symbol.clone(), vec![path.clone()]))
        .collect();
    joins.extend(
        join_conditions(prefix, pattern_index, &parent_symbol_map, &pattern)
            .into_iter()
            .map(join_condition_to_where),
    );
    let join_count = joins.len();

    let where_plan = pattern_where_plan(
        PatternWhereOptions {
            remove_unnecessary_index: options.remove_unnecessary_index,
            predicate_options: options.predicate_options,
        },
        app_id,
        &triples_alias,
        &pattern,
        joins,
    )?;

    let mut select = Vec::new();
    if let Some(previous_table) = &previous_table {
        select.push(SelectProjection {
            expression: WhereExpression::Wildcard(Some(previous_table.clone())),
            alias: None,
        });
    }
    select.extend(match_table_select(&table_name));

    let parent_froms = parent_from_tables(prefix, &pattern, additional_joins);
    let mut from = vec![FromTable {
        table: "triples".to_owned(),
        alias: Some(triples_alias.clone()),
    }];
    if let Some(previous_table) = &previous_table {
        from.push(FromTable {
            table: previous_table.clone(),
            alias: None,
        });
    }
    from.extend(parent_froms.iter().cloned());

    let mut hints = if pg_hints_enabled {
        where_plan.hints
    } else {
        Vec::new()
    };
    if pg_hints_enabled
        && options.row_hints_enabled.unwrap_or(true)
        && let Some(estimate) = context
            .row_estimate
            .or_else(|| attribute_total_estimate(&pattern, options))
        && estimate > 0
        && estimate < options.row_hint_threshold.unwrap_or(10_000)
        && !context.suppress_row_hint
    {
        let outer_table = previous_table
            .clone()
            .or_else(|| (parent_froms.len() == 1).then(|| parent_froms[0].table.clone()));
        if let Some(outer_table) = outer_table {
            hints.push(QueryHint::Rows {
                tables: vec![outer_table, triples_alias.clone()],
                estimate,
            });
        }
    }
    if let Some(index) = chosen_index {
        // A single OR join is not a single point lookup. A forced value scan
        // can reread the entire room/value partition for every incoming row.
        // Probe each entity alternative through BitmapOr instead, without
        // rewriting its NULL/OR semantics. Keep compound and ref joins alone.
        let entity_bitmap = options.candidate_entity_scans == Some(true)
            && index == "ave_with_e_index"
            && single_entity_disjunction(&pattern, symbol_map, join_count);
        hints.push(if entity_bitmap || join_count >= 2 {
            QueryHint::BitmapScan {
                table_alias: triples_alias.clone(),
                index: if entity_bitmap { "triples_pkey" } else { index }.to_owned(),
            }
        } else {
            QueryHint::IndexScan {
                table_alias: triples_alias.clone(),
                index: index.to_owned(),
            }
        });
    }

    let materialization = if should_materialize(&pattern, options, chosen_index.is_some()) {
        Materialization::Materialized
    } else {
        Materialization::NotMaterialized
    };
    Ok(MatchCtePlan {
        cte: MatchCte {
            name: table_name,
            select,
            from,
            predicate: where_plan.predicate,
            materialization,
        },
        hints,
        pattern,
    })
}

fn single_entity_disjunction(
    pattern: &Pattern,
    symbols: &NestedSymbolMap,
    join_count: usize,
) -> bool {
    fn entity_paths(tree: &BindingTree) -> bool {
        match tree {
            BindingTree::Path(path) => path.component == Component::Entity,
            BindingTree::And(paths) | BindingTree::Or(paths) => {
                !paths.is_empty() && paths.iter().all(entity_paths)
            }
        }
    }
    fn alternatives(tree: &BindingTree) -> bool {
        match tree {
            BindingTree::Path(_) => false,
            BindingTree::Or(paths) if paths.len() > 1 => true,
            BindingTree::And(paths) | BindingTree::Or(paths) => paths.iter().any(alternatives),
        }
    }
    if join_count != 1
        || pattern.index.key != IndexKey::Ave
        || !matches!(&pattern.attribute, Term::Constants(values) if values.len() == 1)
        || !matches!(&pattern.value, Term::Constants(values) if !values.is_empty())
    {
        return false;
    }
    pattern
        .entity
        .variable()
        .and_then(|symbol| symbols.get(symbol))
        .is_some_and(|bindings| {
            bindings.iter().all(entity_paths) && bindings.iter().any(alternatives)
        })
}

/// Approximate matching rows for physical-plan selection only. Never use this
/// estimate to limit results or make permission decisions.
#[must_use]
pub fn pattern_row_estimate(pattern: &Pattern, options: &MatchCteOptions) -> Option<u64> {
    attribute_total_estimate(pattern, options)
}

/// Costs an independent indexed equality using the ordinary fresh-sketch model.
/// Callers may retarget this hint to a materialized candidate seed, but must
/// separately admit a small known seed. This never bounds or reuses its rows.
#[must_use]
pub fn independent_equality_index_hint(
    pattern: &Pattern,
    options: &MatchCteOptions,
    table_alias: &str,
) -> Option<QueryHint> {
    if options.pg_hints_enabled == Some(false)
        || !matches!(pattern.index.key, IndexKey::Av | IndexKey::Ave)
        || !matches!(pattern.entity, Term::Variable(_))
        || !matches!(&pattern.attribute, Term::Constants(values) if values.len() == 1)
        || !matches!(&pattern.value, Term::Constants(values) if !values.is_empty() && values.len() <= 1_024)
    {
        return None;
    }
    best_index(pattern, options, &HashMap::new()).map(|index| QueryHint::IndexScan {
        table_alias: table_alias.to_owned(),
        index: index.to_owned(),
    })
}

fn attribute_total_estimate(pattern: &Pattern, options: &MatchCteOptions) -> Option<u64> {
    let entity_estimate = component_sketch_estimate(
        pattern,
        ENTITY,
        &pattern.entity,
        &options.attribute_entity_cardinalities,
        options,
    );
    let value_estimate = component_sketch_estimate(
        pattern,
        VALUE,
        &pattern.value,
        &options.attribute_value_cardinalities,
        options,
    );
    entity_estimate
        .zip(value_estimate)
        .map(|(entity, value)| entity.min(value))
}

fn pattern_attribute_ids(pattern: &Pattern) -> Option<Vec<Uuid>> {
    let Term::Constants(attribute_values) = &pattern.attribute else {
        return None;
    };
    attribute_values
        .iter()
        .map(|value| value.as_str()?.parse::<Uuid>().ok())
        .collect::<Option<Vec<_>>>()
}

fn attribute_partition_rows(pattern: &Pattern, options: &MatchCteOptions) -> Option<u64> {
    pattern_attribute_ids(pattern)?
        .iter()
        .try_fold(0_u64, |total, attribute_id| {
            total.checked_add(*options.attribute_cardinalities.get(attribute_id)?)
        })
}

fn component_sketch_estimate(
    pattern: &Pattern,
    column: CostColumn,
    term: &Term,
    estimates: &BTreeMap<Uuid, BTreeMap<String, u64>>,
    options: &MatchCteOptions,
) -> Option<u64> {
    let attribute_ids = pattern_attribute_ids(pattern)?;
    match term {
        Term::Constants(values) => attribute_ids.iter().try_fold(0_u64, |total, attribute_id| {
            let estimates = estimates.get(attribute_id)?;
            values.iter().try_fold(total, |total, value| {
                let key = serde_json::to_string(value).ok()?;
                let estimate = *estimates.get(&key)?;
                let estimate = if pattern.index.key == IndexKey::Av
                    && column == VALUE
                    && options.hint_plan_av_fix.unwrap_or(true)
                {
                    estimate.min(1)
                } else {
                    estimate
                };
                total.checked_add(estimate)
            })
        }),
        Term::Function(FunctionClause::Not(value)) => {
            let exact = attribute_ids
                .iter()
                .try_fold(0_u64, |total, attribute_id| {
                    let estimates = estimates.get(attribute_id)?;
                    let key = serde_json::to_string(value).ok()?;
                    total.checked_add(*estimates.get(&key)?)
                })?;
            attribute_partition_rows(pattern, options).map(|total| total.saturating_sub(exact))
        }
        Term::Function(FunctionClause::Comparator(comparator)) => {
            let total = attribute_partition_rows(pattern, options)?;
            Some(match comparator.operation {
                ComparatorOperation::Like | ComparatorOperation::CaseInsensitiveLike => {
                    if options.like_count_optimization.unwrap_or(true) {
                        like_estimate(total, comparator.value.as_str().unwrap_or_default())
                    } else {
                        total / 2
                    }
                }
                ComparatorOperation::GreaterThan
                | ComparatorOperation::GreaterThanOrEqual
                | ComparatorOperation::LessThan
                | ComparatorOperation::LessThanOrEqual => total / 2,
            })
        }
        Term::Function(FunctionClause::IsNull(clause)) => {
            attribute_ids.iter().try_fold(0_u64, |total, id_attr_id| {
                let estimates = options
                    .attribute_is_null_cardinalities
                    .get(id_attr_id)?
                    .get(&clause.attr_id)?;
                total.checked_add(if clause.nil {
                    estimates.null
                } else {
                    estimates.not_null
                })
            })
        }
        Term::Any | Term::Variable(_) | Term::Function(FunctionClause::EntityIdStartsWith(_)) => {
            attribute_partition_rows(pattern, options)
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // Mirrors the JVM's deliberately approximate fractional selectivity heuristic.
fn like_estimate(total: u64, value: &str) -> u64 {
    if total == 0 {
        return 0;
    }
    let prefix_wild = value.starts_with('%');
    let suffix_wild = value.ends_with('%');
    let any_wild = value
        .chars()
        .any(|character| matches!(character, '%' | '_'));
    let denominator = total as f64;
    let selectivity = if value == "%" || (prefix_wild && suffix_wild && value.len() == 2) {
        1.0
    } else if !any_wild {
        1.0 / denominator
    } else if prefix_wild && suffix_wild {
        1.0 / denominator.powf(0.25)
    } else if prefix_wild {
        1.0 / denominator.sqrt()
    } else if suffix_wild {
        1.0 / denominator.powf(0.75)
    } else {
        1.0 / denominator.sqrt()
    };
    ((denominator * selectivity) as u64).max(1)
}

fn best_index(
    pattern: &Pattern,
    options: &MatchCteOptions,
    symbol_cardinalities: &HashMap<Symbol, u64>,
) -> Option<&'static str> {
    let known_components = known_component_costs(pattern, options, symbol_cardinalities);
    if known_components.is_empty() {
        return None;
    }
    let join_components = join_component_costs(pattern, symbol_cardinalities);
    let filter_components = filter_components(pattern, options);
    let partition_rows = attribute_partition_rows(pattern, options)?;
    INDEX_CONFIGS
        .iter()
        .copied()
        .filter(|config| index_candidate_matches(*config, pattern))
        .map(|config| {
            index_cost(
                config,
                pattern,
                &known_components,
                &join_components,
                &filter_components,
                Some(partition_rows),
            )
        })
        .min_by(compare_index_cost)
        .map(|cost| cost.config.name)
}

fn index_candidate_matches(config: IndexConfig, pattern: &Pattern) -> bool {
    if config
        .index_key
        .is_some_and(|index_key| index_key != pattern.index.key)
    {
        return false;
    }
    if let Some(data_type) = config.data_type {
        if pattern.index.data_type != Some(data_type) {
            return false;
        }
        if data_type == DataType::String
            && !matches!(
                &pattern.value,
                Term::Function(FunctionClause::Comparator(comparator))
                    if matches!(
                        comparator.operation,
                        ComparatorOperation::Like | ComparatorOperation::CaseInsensitiveLike
                    )
            )
        {
            return false;
        }
    }
    true
}

fn term_for_column(pattern: &Pattern, column: CostColumn) -> &Term {
    match column {
        CostColumn::Entity => &pattern.entity,
        CostColumn::Attribute => &pattern.attribute,
        CostColumn::Value => &pattern.value,
        CostColumn::CreatedAt => &pattern.created_at,
    }
}

fn known_component_costs(
    pattern: &Pattern,
    options: &MatchCteOptions,
    symbol_cardinalities: &HashMap<Symbol, u64>,
) -> HashMap<CostColumn, u64> {
    let mut known = HashMap::new();
    if let Term::Function(FunctionClause::IsNull(clause)) = &pattern.value
        && (!clause.nil || !options.fix_is_null_index_cost.unwrap_or(true))
        && let Some(estimate) = component_sketch_estimate(
            pattern,
            VALUE,
            &pattern.value,
            &options.attribute_value_cardinalities,
            options,
        )
    {
        known.insert(ENTITY, estimate);
    }
    for column in [ENTITY, ATTRIBUTE, VALUE, CREATED_AT] {
        let term = term_for_column(pattern, column);
        if column == VALUE && matches!(term, Term::Function(FunctionClause::IsNull(_))) {
            continue;
        }
        let estimate = match (column, term) {
            (CostColumn::Attribute | CostColumn::CreatedAt, Term::Constants(values)) => {
                Some(values.len() as u64)
            }
            (CostColumn::Entity, Term::Constants(_) | Term::Function(_)) => {
                component_sketch_estimate(
                    pattern,
                    ENTITY,
                    term,
                    &options.attribute_entity_cardinalities,
                    options,
                )
            }
            (CostColumn::Value, Term::Constants(_) | Term::Function(_)) => {
                component_sketch_estimate(
                    pattern,
                    VALUE,
                    term,
                    &options.attribute_value_cardinalities,
                    options,
                )
            }
            (_, Term::Variable(symbol)) => symbol_cardinalities.get(symbol).copied(),
            (_, Term::Any) | (CostColumn::Attribute | CostColumn::CreatedAt, Term::Function(_)) => {
                None
            }
        };
        if let Some(estimate) = estimate {
            known.insert(column, estimate);
        }
    }
    known
}

fn join_component_costs(
    pattern: &Pattern,
    symbol_cardinalities: &HashMap<Symbol, u64>,
) -> HashMap<CostColumn, u64> {
    [ENTITY, ATTRIBUTE, VALUE, CREATED_AT]
        .into_iter()
        .filter_map(|column| match term_for_column(pattern, column) {
            Term::Variable(symbol) => symbol_cardinalities
                .get(symbol)
                .copied()
                .map(|estimate| (column, estimate)),
            Term::Any | Term::Constants(_) | Term::Function(_) => None,
        })
        .collect()
}

fn filter_components(pattern: &Pattern, options: &MatchCteOptions) -> HashSet<CostColumn> {
    let is_null_entity_filter = matches!(
        &pattern.value,
        Term::Function(FunctionClause::IsNull(clause))
            if !clause.nil || !options.fix_is_null_index_cost.unwrap_or(true)
    );
    [ENTITY, ATTRIBUTE, VALUE, CREATED_AT]
        .into_iter()
        .filter(|column| {
            (*column == ENTITY && is_null_entity_filter)
                || (*column != VALUE
                    || !matches!(&pattern.value, Term::Function(FunctionClause::IsNull(_))))
                    && matches!(
                        term_for_column(pattern, *column),
                        Term::Constants(_) | Term::Function(_)
                    )
        })
        .collect()
}

fn index_cost(
    config: IndexConfig,
    pattern: &Pattern,
    known_components: &HashMap<CostColumn, u64>,
    join_components: &HashMap<CostColumn, u64>,
    filter_components: &HashSet<CostColumn>,
    partition_rows: Option<u64>,
) -> IndexCost {
    let mut known_remaining = known_components.clone();
    let mut join_remaining = join_components.clone();
    let mut path = Vec::new();
    for &column in config.columns {
        let Some(cost) = known_remaining.remove(&column) else {
            break;
        };
        join_remaining.remove(&column);
        path.push((column, cost));
        if matches!(term_for_column(pattern, column), Term::Constants(values) if values.len() > 1) {
            break;
        }
    }
    let walked_columns = path
        .iter()
        .map(|(column, _)| *column)
        .collect::<HashSet<_>>();
    if walked_columns.iter().any(|column| *column != ATTRIBUTE) {
        for column in config.columns {
            if !walked_columns.contains(column) {
                join_remaining.remove(column);
            }
        }
    }
    let path_resolves_expensive_function = path.iter().any(|(column, _)| {
        matches!(
            term_for_column(pattern, *column),
            Term::Function(FunctionClause::Comparator(comparator))
                if matches!(
                    comparator.operation,
                    ComparatorOperation::Like | ComparatorOperation::CaseInsensitiveLike
                )
        )
    });
    let mut scan_cost_from_path = 1_u64;
    for (column, cost) in &path {
        scan_cost_from_path = scan_cost_from_path.saturating_mul(*cost);
        if config.unique_columns.contains(column) {
            break;
        }
    }
    let path_has_selective_column = path.iter().any(|(column, _)| *column != ATTRIBUTE);
    let can_push_down_non_attribute_filter = config
        .columns
        .iter()
        .any(|column| *column != ATTRIBUTE && filter_components.contains(column));
    let scan_cost = if partition_rows.is_some()
        && ((!path.is_empty() && !path_has_selective_column && !can_push_down_non_attribute_filter)
            || (path.is_empty() && filter_components.contains(&ATTRIBUTE)))
    {
        partition_rows.unwrap_or(scan_cost_from_path)
    } else {
        scan_cost_from_path
    };
    let max_filter_remaining = known_remaining
        .iter()
        .filter(|(column, _)| filter_components.contains(column))
        .map(|(_, estimate)| *estimate)
        .max()
        .unwrap_or(0);
    let max_join_remaining = join_remaining.values().copied().max().unwrap_or(0);
    let join_reversal_risk = max_join_remaining > 0
        && scan_cost < max_join_remaining
        && !path_resolves_expensive_function;
    let rows_scanned = if join_reversal_risk {
        max_join_remaining
    } else if path_has_selective_column {
        scan_cost
    } else {
        scan_cost.max(max_join_remaining).max(max_filter_remaining)
    };
    let unresolved_filter_count = known_remaining
        .keys()
        .filter(|column| filter_components.contains(column))
        .count();
    let cost_multiplier = 100_u128
        + 10_u128.saturating_mul(unresolved_filter_count as u128)
        + 15_u128.saturating_mul(join_remaining.len() as u128);
    IndexCost {
        config,
        cost: u128::from(rows_scanned).saturating_mul(cost_multiplier),
        path,
        known_remaining,
        join_components: join_components.clone(),
        matching_index_key: config.index_key == Some(pattern.index.key),
        matching_data_type: config.data_type == pattern.index.data_type,
    }
}

fn compare_index_cost(left: &IndexCost, right: &IndexCost) -> std::cmp::Ordering {
    left.cost
        .cmp(&right.cost)
        .then_with(|| {
            match (
                left.known_remaining.is_empty(),
                right.known_remaining.is_empty(),
            ) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        })
        .then_with(|| right.path.len().cmp(&left.path.len()))
        .then_with(|| match (left.path.is_empty(), right.path.is_empty()) {
            (false, false) => {
                let left_sum = left
                    .path
                    .iter()
                    .map(|(_, cost)| u128::from(*cost))
                    .sum::<u128>();
                let right_sum = right
                    .path
                    .iter()
                    .map(|(_, cost)| u128::from(*cost))
                    .sum::<u128>();
                left_sum
                    .saturating_mul(right.path.len() as u128)
                    .cmp(&right_sum.saturating_mul(left.path.len() as u128))
            }
            _ => std::cmp::Ordering::Equal,
        })
        .then_with(|| {
            let left_only_joins = left.known_remaining == left.join_components;
            let right_only_joins = right.known_remaining == right.join_components;
            match (left_only_joins, right_only_joins) {
                (false, true) => std::cmp::Ordering::Less,
                (true, false) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        })
        .then_with(|| right.matching_index_key.cmp(&left.matching_index_key))
        .then_with(|| right.matching_data_type.cmp(&left.matching_data_type))
}

fn match_table_select(table_name: &str) -> Vec<SelectProjection> {
    [
        (WhereColumn::EntityId, "entity-id"),
        (WhereColumn::AttrId, "attr-id"),
        (WhereColumn::Value, "value"),
        (WhereColumn::Index(IndexKey::Eav), "is-ref-val"),
        (WhereColumn::CreatedAt, "created-at"),
    ]
    .into_iter()
    .map(|(column, suffix)| SelectProjection {
        expression: WhereExpression::column(column),
        alias: Some(format!("{table_name}-{suffix}")),
    })
    .collect()
}

fn parent_from_tables<S: BuildHasher>(
    prefix: &str,
    pattern: &Pattern,
    additional_joins: &HashMap<Symbol, BindingPath, S>,
) -> Vec<FromTable> {
    pattern
        .variables_with_components()
        .filter_map(|(_, symbol)| additional_joins.get(symbol))
        .map(|path| FromTable {
            table: format!(
                "{}{pattern_index}",
                prefix.trim_start_matches(':'),
                pattern_index = path.pattern_index
            ),
            alias: None,
        })
        .collect()
}

fn should_materialize(pattern: &Pattern, options: &MatchCteOptions, has_best_index: bool) -> bool {
    let always_materialize = match &pattern.attribute {
        Term::Constants(values) if values.len() == 1 => values[0]
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .is_some_and(|attr_id| options.always_materialize_attr_ids.contains(&attr_id)),
        _ => false,
    };
    let is_null = matches!(&pattern.value, Term::Function(FunctionClause::IsNull(_)));
    let indexed_constant = !(options.has_best_index || has_best_index)
        && pattern.index.key == IndexKey::Ave
        && matches!(pattern.value, Term::Constants(_));
    always_materialize || !options.page_info || is_null || indexed_constant
}

/// Rewrites VAE to EAV for entity-side reference joins, matching the Clojure planner.
#[must_use]
pub fn transform_for_ref_joins(symbol_map: &NestedSymbolMap, pattern: &Pattern) -> Pattern {
    if pattern.index.key != IndexKey::Vae {
        return pattern.clone();
    }

    let mut join_components = HashSet::new();
    for (component, symbol) in pattern.variables_with_components() {
        if let Some(bindings) = symbol_map.get(symbol) {
            for binding in bindings.iter().flat_map(flatten_binding_tree) {
                join_components.insert((component, binding.component));
            }
        }
    }
    let entity_constant = matches!(pattern.entity, Term::Constants(_));
    let value_constant = matches!(pattern.value, Term::Constants(_));
    let entity_entity = HashSet::from([(Component::Entity, Component::Entity)]);
    let entity_value = HashSet::from([(Component::Entity, Component::Value)]);
    if join_components == entity_entity
        || join_components == entity_value
        || (join_components.is_empty() && entity_constant && !value_constant)
    {
        let mut pattern = pattern.clone();
        pattern.index = Index::new(IndexKey::Eav);
        pattern
    } else {
        pattern.clone()
    }
}

fn flatten_binding_tree(tree: &BindingTree) -> Vec<&BindingPath> {
    match tree {
        BindingTree::Path(path) => vec![path],
        BindingTree::And(trees) | BindingTree::Or(trees) => {
            trees.iter().flat_map(flatten_binding_tree).collect()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolField {
    pub symbol: Symbol,
    pub reference_value: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PatternMetadata {
    pub cte_columns: Vec<String>,
    pub symbol_fields: BTreeMap<usize, SymbolField>,
    pub pattern: Pattern,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum PatternMetadataNode {
    Pattern(PatternMetadata),
    Or(Vec<Vec<PatternMetadataNode>>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MatchQueryPlan {
    pub next_index: usize,
    pub ctes: Vec<PlannedCte>,
    pub hints: Vec<QueryHint>,
    pub pattern_metadata: Vec<PatternMetadataNode>,
    pub symbol_map: NestedSymbolMap,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub symbol_cardinalities: HashMap<Symbol, u64>,
    pub final_table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<PagePlanMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ensure_empty_symbol_values_for_topics: Option<bool>,
}

impl MatchQueryPlan {
    /// Serializes the complete CTE graph and final table selection.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        json!({
            ":with": self.ctes.iter().map(PlannedCte::to_honey_sql_json).collect::<Vec<_>>(),
            ":select": format!(":{}.*", self.final_table),
            ":pg-hints": serde_json::to_value(&self.hints).unwrap_or(Value::Null),
            ":from": format!(":{}", self.final_table)
        })
    }
}

pub struct MatchQueryContext<'a, S> {
    pub prefix: &'a str,
    pub app_id: Uuid,
    pub additional_joins: &'a HashMap<Symbol, BindingPath, S>,
    pub initial_symbol_cardinalities: &'a HashMap<Symbol, u64>,
    pub options: &'a MatchCteOptions,
}

#[derive(Default)]
struct PlannerState {
    next_index: usize,
    group_index: usize,
    ctes: Vec<PlannedCte>,
    hints: Vec<QueryHint>,
    symbol_map: NestedSymbolMap,
    symbol_cardinalities: HashMap<Symbol, u64>,
    pattern_metadata: Vec<PatternMetadataNode>,
    or_indices: HashSet<usize>,
}

/// Plans a complete recursive Datalog CTE graph.
///
/// # Errors
///
/// Returns `PatternError` for an empty query, empty OR branch set, or invalid pattern.
pub fn plan_match_query<S: BuildHasher>(
    context: &MatchQueryContext<'_, S>,
    clauses: &[NormalizedClause],
) -> Result<MatchQueryPlan, PatternError> {
    plan_match_query_from_index(context, clauses, 0)
}

/// Plans a recursive Datalog CTE graph beginning at a caller-owned global index.
///
/// Nested `InstaQL` queries share one CTE namespace. This entry point lets each
/// pattern group continue where the preceding group or dependency CTE stopped,
/// while [`plan_match_query`] retains the standalone zero-based behavior.
///
/// # Errors
///
/// Returns `PatternError` for an empty query, empty OR branch set, or invalid pattern.
pub fn plan_match_query_from_index<S: BuildHasher>(
    context: &MatchQueryContext<'_, S>,
    clauses: &[NormalizedClause],
    start_index: usize,
) -> Result<MatchQueryPlan, PatternError> {
    if clauses.is_empty() {
        return Err(PatternError::EmptyQuery);
    }
    let mut state = PlannerState {
        next_index: start_index,
        symbol_cardinalities: context.initial_symbol_cardinalities.clone(),
        ..PlannerState::default()
    };
    for clause in clauses {
        state = accumulate_clause(context, state, clause)?;
    }
    let anchored_symbols = context.additional_joins.keys().cloned().collect::<Vec<_>>();
    validate_clause_joinable(clauses, &anchored_symbols)?;
    let final_index = state
        .next_index
        .checked_sub(1)
        .ok_or(PatternError::EmptyQuery)?;
    Ok(MatchQueryPlan {
        next_index: state.next_index,
        ctes: state.ctes,
        hints: state.hints,
        pattern_metadata: state.pattern_metadata,
        symbol_map: state.symbol_map,
        symbol_cardinalities: state.symbol_cardinalities,
        final_table: indexed_table(context.prefix, final_index),
        page: None,
        ensure_empty_symbol_values_for_topics: context
            .options
            .ensure_empty_symbol_values_for_topics,
    })
}

fn accumulate_clause<S: BuildHasher>(
    context: &MatchQueryContext<'_, S>,
    mut state: PlannerState,
    clause: &NormalizedClause,
) -> Result<PlannerState, PatternError> {
    match clause {
        NormalizedClause::Pattern { pattern } => {
            let pattern_index = state.next_index;
            let (row_estimate, propagated_estimate, suppress_row_hint) =
                joined_row_estimate(pattern, context.options, &state.symbol_cardinalities);
            let plan = build_match_cte(
                &MatchCteContext {
                    prefix: context.prefix,
                    app_id: context.app_id,
                    pattern_index,
                    start_of_group: state.group_index == 0,
                    symbol_map: &state.symbol_map,
                    symbol_cardinalities: &state.symbol_cardinalities,
                    additional_joins: context.additional_joins,
                    options: context.options,
                    row_estimate,
                    suppress_row_hint,
                },
                pattern,
            )?;
            state.ctes.push(PlannedCte::Match(plan.cte));
            state.hints.extend(plan.hints);
            merge_symbol_maps(
                &mut state.symbol_map,
                pattern.nested_symbol_map(pattern_index),
            );
            if let Some(propagated_estimate) = propagated_estimate {
                for term in [&pattern.entity, &pattern.value] {
                    if let Term::Variable(symbol) = term {
                        state
                            .symbol_cardinalities
                            .entry(symbol.clone())
                            .and_modify(|estimate| *estimate = (*estimate).min(propagated_estimate))
                            .or_insert(propagated_estimate);
                    }
                }
            }
            state
                .pattern_metadata
                .push(PatternMetadataNode::Pattern(pattern_metadata(
                    context.prefix,
                    pattern_index,
                    pattern,
                )));
            state.next_index += 1;
            state.group_index += 1;
            Ok(state)
        }
        NormalizedClause::And { clauses } => {
            for clause in clauses {
                state = accumulate_clause(context, state, clause)?;
            }
            Ok(state)
        }
        NormalizedClause::Or {
            join_symbol,
            clauses,
        } => accumulate_or_clause(context, state, join_symbol, clauses),
    }
}

pub(crate) fn joined_row_estimate(
    pattern: &Pattern,
    options: &MatchCteOptions,
    symbol_cardinalities: &HashMap<Symbol, u64>,
) -> (Option<u64>, Option<u64>, bool) {
    let Some(initial_estimate) = attribute_total_estimate(pattern, options) else {
        return (None, None, false);
    };
    let join_cap = [&pattern.entity, &pattern.value]
        .into_iter()
        .filter_map(|term| match term {
            Term::Variable(symbol) => symbol_cardinalities.get(symbol).copied(),
            Term::Any | Term::Constants(_) | Term::Function(_) => None,
        })
        .min();
    let Some(join_cap) = join_cap else {
        return (Some(initial_estimate), Some(initial_estimate), false);
    };
    let row_estimate = initial_estimate.min(join_cap);
    let entity_is_new = matches!(
        &pattern.entity,
        Term::Variable(symbol) if !symbol_cardinalities.contains_key(symbol)
    );
    let propagated_estimate = if entity_is_new {
        initial_estimate.min(join_cap.saturating_mul(20))
    } else {
        row_estimate
    };
    let suppress_row_hint = join_cap > row_estimate.saturating_mul(11);
    (
        Some(row_estimate),
        Some(propagated_estimate),
        suppress_row_hint,
    )
}

fn accumulate_or_clause<S: BuildHasher>(
    context: &MatchQueryContext<'_, S>,
    state: PlannerState,
    join_symbol: &Symbol,
    branches: &[NormalizedClause],
) -> Result<PlannerState, PatternError> {
    if branches.is_empty() {
        return Err(PatternError::EmptyOrClause);
    }

    let outer_next_index = state.next_index;
    let outer_group_index = state.group_index;
    let outer_or_indices = state.or_indices;
    let mut outer_symbol_map = state.symbol_map;
    let mut outer_symbol_cardinalities = state.symbol_cardinalities;
    let mut ctes = state.ctes;
    let mut hints = state.hints;
    let mut pattern_metadata = state.pattern_metadata;
    let mut next_index = outer_next_index;
    let mut inherited_or_indices = outer_or_indices.clone();
    // A nested OR at the start of a branch must not absorb the CTE preceding
    // this outer OR. The outer gather owns that predecessor and joins every
    // completed branch back to it. Marking it as branch-external also makes OR
    // semantics independent of whether the nested branch appears first.
    if let Some(previous_index) = (outer_group_index > 0)
        .then(|| outer_next_index.checked_sub(1))
        .flatten()
    {
        inherited_or_indices.insert(previous_index);
    }
    let mut branch_last_indices = Vec::with_capacity(branches.len());
    let mut branch_symbol_maps = Vec::with_capacity(branches.len());
    let mut branch_symbol_cardinalities = Vec::with_capacity(branches.len());
    let mut branch_metadata = Vec::with_capacity(branches.len());

    for branch in branches {
        let branch_start = next_index;
        let branch_state = PlannerState {
            next_index,
            group_index: 0,
            ctes: Vec::new(),
            hints: Vec::new(),
            symbol_map: NestedSymbolMap::new(),
            symbol_cardinalities: HashMap::new(),
            pattern_metadata: Vec::new(),
            or_indices: inherited_or_indices.clone(),
        };
        let mut result = accumulate_clause(context, branch_state, branch)?;
        if result.next_index == branch_start {
            return Err(PatternError::EmptyOrClause);
        }
        let branch_last_index = result.next_index - 1;
        next_index = result.next_index;
        inherited_or_indices.insert(branch_last_index);
        ctes.append(&mut result.ctes);
        hints.append(&mut result.hints);
        branch_last_indices.push(branch_last_index);
        branch_symbol_maps.push((branch_last_index, result.symbol_map));
        branch_symbol_cardinalities.push(result.symbol_cardinalities);
        branch_metadata.push(result.pattern_metadata);
    }

    let previous_index = (outer_group_index > 0)
        .then(|| outer_next_index.checked_sub(1))
        .flatten()
        .filter(|index| !outer_or_indices.contains(index));
    let gather = build_or_gather_cte(
        context.prefix,
        previous_index,
        next_index,
        &branch_last_indices,
        &outer_symbol_map,
        &branch_symbol_maps,
        join_symbol,
    )?;
    ctes.push(PlannedCte::OrGather(gather));

    let maps: Vec<_> = branch_symbol_maps
        .iter()
        .map(|(_, symbol_map)| symbol_map.clone())
        .collect();
    merge_symbol_maps(&mut outer_symbol_map, merge_or_symbol_maps(&maps));
    for cardinalities in branch_symbol_cardinalities {
        for (symbol, estimate) in cardinalities {
            outer_symbol_cardinalities
                .entry(symbol)
                .and_modify(|current| *current = (*current).min(estimate))
                .or_insert(estimate);
        }
    }
    pattern_metadata.push(PatternMetadataNode::Or(branch_metadata));
    let mut or_indices = outer_or_indices;
    or_indices.extend(branch_last_indices);
    Ok(PlannerState {
        next_index: next_index + 1,
        group_index: outer_group_index + 1,
        ctes,
        hints,
        symbol_map: outer_symbol_map,
        symbol_cardinalities: outer_symbol_cardinalities,
        pattern_metadata,
        or_indices,
    })
}

fn merge_symbol_maps(destination: &mut NestedSymbolMap, source: NestedSymbolMap) {
    for (symbol, bindings) in source {
        destination.entry(symbol).or_default().extend(bindings);
    }
}

fn pattern_metadata(prefix: &str, pattern_index: usize, pattern: &Pattern) -> PatternMetadata {
    let table = indexed_table(prefix, pattern_index);
    let cte_columns = ["entity-id", "attr-id", "value", "is-ref-val", "created-at"]
        .into_iter()
        .map(|suffix| format!("{table}-{suffix}").replace('-', "_"))
        .collect();
    let mut symbol_fields = BTreeMap::new();
    for (component, symbol) in pattern.variables_with_components() {
        let triple_index = match component {
            Component::Entity => 0,
            Component::Attribute => 1,
            Component::Value => 2,
        };
        symbol_fields.insert(
            triple_index,
            SymbolField {
                symbol: symbol.clone(),
                reference_value: component == Component::Value
                    && pattern.index.key == IndexKey::Vae,
            },
        );
    }
    PatternMetadata {
        cte_columns,
        symbol_fields,
        pattern: pattern.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RawPattern, RawTerm};

    #[test]
    #[allow(clippy::too_many_lines)] // Exercises global, row-only, and threshold hint controls together.
    fn adds_cardinality_row_hint_to_a_joined_attribute_pattern() {
        let app_id = Uuid::new_v4();
        let first_attr = Uuid::new_v4();
        let second_attr = Uuid::new_v4();
        let entity = Symbol::new("?entity").unwrap();
        let clauses = [
            NormalizedClause::Pattern {
                pattern: RawPattern::new(
                    Index::new(IndexKey::Eav),
                    vec![
                        RawTerm::Variable(entity.clone()),
                        RawTerm::Constant(json!(first_attr)),
                        RawTerm::Any,
                    ],
                )
                .normalize()
                .unwrap(),
            },
            NormalizedClause::Pattern {
                pattern: RawPattern::new(
                    Index::new(IndexKey::Eav),
                    vec![
                        RawTerm::Variable(entity),
                        RawTerm::Constant(json!(second_attr)),
                        RawTerm::Any,
                    ],
                )
                .normalize()
                .unwrap(),
            },
        ];
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(first_attr, 5), (second_attr, 42)]),
            ..MatchCteOptions::default()
        };
        let additional_joins = HashMap::new();
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "m-",
                app_id,
                additional_joins: &additional_joins,
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap();

        assert!(plan.hints.contains(&QueryHint::Rows {
            tables: vec!["m-0".to_owned(), "t1".to_owned()],
            estimate: 5,
        }));
        assert_eq!(
            plan.symbol_cardinalities[&Symbol::new("?entity").unwrap()],
            5
        );

        let options = MatchCteOptions {
            row_hint_threshold: Some(5),
            ..options
        };
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "m-",
                app_id,
                additional_joins: &additional_joins,
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap();
        assert!(
            !plan
                .hints
                .iter()
                .any(|hint| matches!(hint, QueryHint::Rows { .. }))
        );

        let options = MatchCteOptions {
            row_hints_enabled: Some(false),
            ..options
        };
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "m-",
                app_id,
                additional_joins: &additional_joins,
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap();
        assert!(
            !plan
                .hints
                .iter()
                .any(|hint| matches!(hint, QueryHint::Rows { .. }))
        );
        assert!(
            plan.hints
                .iter()
                .any(|hint| matches!(hint, QueryHint::IndexScan { .. }))
        );

        let options = MatchCteOptions {
            pg_hints_enabled: Some(false),
            ..options
        };
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "m-",
                app_id,
                additional_joins: &additional_joins,
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap();
        assert!(plan.hints.is_empty());
    }

    #[test]
    fn live_attribute_allowlist_forces_page_cte_materialization() {
        let attribute_id = Uuid::new_v4();
        let pattern = RawPattern::new(
            Index::new(IndexKey::Eav),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Any,
            ],
        )
        .normalize()
        .unwrap();
        let default_options = MatchCteOptions {
            page_info: true,
            has_best_index: true,
            ..MatchCteOptions::default()
        };
        assert!(!should_materialize(&pattern, &default_options, true));
        let live_options = MatchCteOptions {
            always_materialize_attr_ids: vec![attribute_id],
            ..default_options
        };
        assert!(should_materialize(&pattern, &live_options, true));
    }

    #[test]
    fn prefers_value_selective_cardinality_over_attribute_total() {
        let attribute_id = Uuid::new_v4();
        let value = json!("needle");
        let pattern = RawPattern::new(
            Index::new(IndexKey::Ave),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Constant(value.clone()),
            ],
        )
        .normalize()
        .unwrap();
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 9_000)]),
            attribute_value_cardinalities: BTreeMap::from([(
                attribute_id,
                BTreeMap::from([(serde_json::to_string(&value).unwrap(), 3)]),
            )]),
            ..MatchCteOptions::default()
        };

        assert_eq!(attribute_total_estimate(&pattern, &options), Some(3));
    }

    #[test]
    fn planner_cost_rollbacks_are_independent_and_preserve_reference_defaults() {
        let attribute_id = Uuid::new_v4();
        let value = json!("needle");
        let second_value = json!("second");
        let av_pattern = RawPattern::new(
            Index::new(IndexKey::Av),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Constant(value.clone()),
            ],
        )
        .normalize()
        .unwrap();
        let base = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 10_000)]),
            attribute_value_cardinalities: BTreeMap::from([(
                attribute_id,
                BTreeMap::from([
                    (serde_json::to_string(&value).unwrap(), 37),
                    (serde_json::to_string(&second_value).unwrap(), 19),
                ]),
            )]),
            ..MatchCteOptions::default()
        };
        assert_eq!(attribute_total_estimate(&av_pattern, &base), Some(1));
        let legacy_av = MatchCteOptions {
            hint_plan_av_fix: Some(false),
            ..base.clone()
        };
        assert_eq!(attribute_total_estimate(&av_pattern, &legacy_av), Some(37));
        let av_multi_pattern = RawPattern::new(
            Index::new(IndexKey::Av),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Constants(vec![value, second_value]),
            ],
        )
        .normalize()
        .unwrap();
        assert_eq!(attribute_total_estimate(&av_multi_pattern, &base), Some(2));
        assert_eq!(
            attribute_total_estimate(&av_multi_pattern, &legacy_av),
            Some(56)
        );

        let like_pattern = RawPattern::new(
            Index::checked(IndexKey::Ave, DataType::String),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Function(FunctionClause::Comparator(crate::ComparatorClause {
                    operation: ComparatorOperation::Like,
                    data_type: DataType::String,
                    value: json!("%rust%"),
                })),
            ],
        )
        .normalize()
        .unwrap();
        assert_eq!(attribute_total_estimate(&like_pattern, &base), Some(1_000));
        let legacy_like = MatchCteOptions {
            like_count_optimization: Some(false),
            ..base
        };
        assert_eq!(
            attribute_total_estimate(&like_pattern, &legacy_like),
            Some(5_000)
        );
    }

    #[test]
    fn estimates_not_predicates_as_the_forward_sketch_complement() {
        let attribute_id = Uuid::new_v4();
        let excluded = json!("archived");
        let pattern = RawPattern::new(
            Index::new(IndexKey::Ave),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Function(FunctionClause::Not(excluded.clone())),
            ],
        )
        .normalize()
        .unwrap();
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 10)]),
            attribute_value_cardinalities: BTreeMap::from([(
                attribute_id,
                BTreeMap::from([(serde_json::to_string(&excluded).unwrap(), 3)]),
            )]),
            ..MatchCteOptions::default()
        };

        assert_eq!(
            component_sketch_estimate(
                &pattern,
                VALUE,
                &pattern.value,
                &options.attribute_value_cardinalities,
                &options,
            ),
            Some(7)
        );
    }

    #[test]
    fn uses_exact_is_null_cardinality_for_both_predicate_polarities() {
        let id_attr_id = Uuid::new_v4();
        let tested_attr_id = Uuid::new_v4();
        let make_pattern = |nil| {
            RawPattern::new(
                Index::new(IndexKey::Ave),
                vec![
                    RawTerm::Any,
                    RawTerm::Constant(json!(id_attr_id)),
                    RawTerm::Function(FunctionClause::IsNull(crate::IsNullClause {
                        attr_id: tested_attr_id,
                        nil,
                        reference: false,
                        reverse: false,
                        indexed: false,
                        indexed_checked_type: None,
                    })),
                ],
            )
            .normalize()
            .unwrap()
        };
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(id_attr_id, 100)]),
            attribute_is_null_cardinalities: BTreeMap::from([(
                id_attr_id,
                BTreeMap::from([(
                    tested_attr_id,
                    IsNullCardinalities {
                        null: 73,
                        not_null: 27,
                    },
                )]),
            )]),
            ..MatchCteOptions::default()
        };

        assert_eq!(
            attribute_total_estimate(&make_pattern(true), &options),
            Some(73)
        );
        assert_eq!(
            attribute_total_estimate(&make_pattern(false), &options),
            Some(27)
        );
        assert_eq!(
            known_component_costs(&make_pattern(false), &options, &HashMap::new()).get(&ENTITY),
            Some(&27)
        );
        assert!(
            !known_component_costs(&make_pattern(true), &options, &HashMap::new())
                .contains_key(&ENTITY)
        );
        let legacy_cost = MatchCteOptions {
            fix_is_null_index_cost: Some(false),
            ..options.clone()
        };
        assert_eq!(
            known_component_costs(&make_pattern(true), &legacy_cost, &HashMap::new()).get(&ENTITY),
            Some(&73)
        );
        let predicate_rollback_only = MatchCteOptions {
            predicate_options: PredicateOptions {
                disable_is_null_fix: true,
                ..PredicateOptions::default()
            },
            ..options
        };
        assert!(
            !known_component_costs(
                &make_pattern(true),
                &predicate_rollback_only,
                &HashMap::new(),
            )
            .contains_key(&ENTITY)
        );
    }

    #[test]
    fn takes_the_minimum_of_reverse_entity_and_forward_value_estimates() {
        let attribute_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let lookup_attribute_id = Uuid::new_v4();
        let value = json!("status");
        let entity = json!(entity_id);
        let lookup = json!([lookup_attribute_id, "unique-value"]);
        let pattern = RawPattern::new(
            Index::new(IndexKey::Eav),
            vec![
                RawTerm::Constants(vec![entity.clone(), lookup.clone()]),
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Constant(value.clone()),
            ],
        )
        .normalize()
        .unwrap();
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 1_000)]),
            attribute_value_cardinalities: BTreeMap::from([(
                attribute_id,
                BTreeMap::from([(serde_json::to_string(&value).unwrap(), 7)]),
            )]),
            attribute_entity_cardinalities: BTreeMap::from([(
                attribute_id,
                BTreeMap::from([
                    (serde_json::to_string(&entity).unwrap(), 3),
                    (serde_json::to_string(&lookup).unwrap(), 1),
                ]),
            )]),
            ..MatchCteOptions::default()
        };

        assert_eq!(attribute_total_estimate(&pattern, &options), Some(4));
    }

    #[test]
    fn suppresses_a_row_hint_that_would_overcompress_the_outer_binding_set() {
        let app_id = Uuid::new_v4();
        let first_attr = Uuid::new_v4();
        let second_attr = Uuid::new_v4();
        let entity = Symbol::new("?entity").unwrap();
        let make_pattern = |attribute_id, entity: Symbol| NormalizedClause::Pattern {
            pattern: RawPattern::new(
                Index::new(IndexKey::Eav),
                vec![
                    RawTerm::Variable(entity),
                    RawTerm::Constant(json!(attribute_id)),
                    RawTerm::Any,
                ],
            )
            .normalize()
            .unwrap(),
        };
        let clauses = [
            make_pattern(first_attr, entity.clone()),
            make_pattern(second_attr, entity),
        ];
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(first_attr, 1_000), (second_attr, 1)]),
            ..MatchCteOptions::default()
        };
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "m-",
                app_id,
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap();

        assert!(!plan.hints.iter().any(|hint| {
            matches!(hint, QueryHint::Rows { tables, .. } if tables.iter().any(|table| table == "t1"))
        }));
    }

    #[test]
    fn propagates_bounded_fanout_for_a_new_entity_variable() {
        let app_id = Uuid::new_v4();
        let parent_attr = Uuid::new_v4();
        let child_attr = Uuid::new_v4();
        let child_field_attr = Uuid::new_v4();
        let root = Symbol::new("?root").unwrap();
        let parent = Symbol::new("?parent").unwrap();
        let child = Symbol::new("?child").unwrap();
        let clauses = [
            NormalizedClause::Pattern {
                pattern: RawPattern::new(
                    Index::new(IndexKey::Eav),
                    vec![
                        RawTerm::Variable(root),
                        RawTerm::Constant(json!(parent_attr)),
                        RawTerm::Variable(parent.clone()),
                    ],
                )
                .normalize()
                .unwrap(),
            },
            NormalizedClause::Pattern {
                pattern: RawPattern::new(
                    Index::new(IndexKey::Vae),
                    vec![
                        RawTerm::Variable(child.clone()),
                        RawTerm::Constant(json!(child_attr)),
                        RawTerm::Variable(parent),
                    ],
                )
                .normalize()
                .unwrap(),
            },
            NormalizedClause::Pattern {
                pattern: RawPattern::new(
                    Index::new(IndexKey::Eav),
                    vec![
                        RawTerm::Variable(child.clone()),
                        RawTerm::Constant(json!(child_field_attr)),
                        RawTerm::Any,
                    ],
                )
                .normalize()
                .unwrap(),
            },
        ];
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([
                (parent_attr, 2),
                (child_attr, 100),
                (child_field_attr, 1_000),
            ]),
            ..MatchCteOptions::default()
        };
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "m-",
                app_id,
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &options,
            },
            &clauses,
        )
        .unwrap();

        assert_eq!(plan.symbol_cardinalities[&child], 40);
        assert!(plan.hints.contains(&QueryHint::Rows {
            tables: vec!["m-0".to_owned(), "t1".to_owned()],
            estimate: 2,
        }));
        assert!(plan.hints.contains(&QueryHint::Rows {
            tables: vec!["m-1".to_owned(), "t2".to_owned()],
            estimate: 40,
        }));
    }

    #[test]
    fn cost_planner_prefers_entity_probe_over_large_value_scan() {
        let attribute_id = Uuid::new_v4();
        let entity = Symbol::new("?entity").unwrap();
        let pattern = RawPattern::new(
            Index::checked(IndexKey::Ave, DataType::Date),
            vec![
                RawTerm::Variable(entity.clone()),
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Function(FunctionClause::Comparator(crate::ComparatorClause {
                    operation: ComparatorOperation::GreaterThan,
                    data_type: DataType::Date,
                    value: json!("2026-01-01T00:00:00Z"),
                })),
            ],
        )
        .normalize()
        .unwrap();
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 1_000_000)]),
            ..MatchCteOptions::default()
        };

        assert_eq!(
            best_index(&pattern, &options, &HashMap::from([(entity, 12_u64)])),
            Some("triples_pkey")
        );
    }

    #[test]
    fn cost_planner_prefers_matching_checked_value_index() {
        let attribute_id = Uuid::new_v4();
        let pattern = RawPattern::new(
            Index::checked(IndexKey::Ave, DataType::Number),
            vec![
                RawTerm::Any,
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Function(FunctionClause::Comparator(crate::ComparatorClause {
                    operation: ComparatorOperation::GreaterThan,
                    data_type: DataType::Number,
                    value: json!(10),
                })),
            ],
        )
        .normalize()
        .unwrap();
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 100)]),
            ..MatchCteOptions::default()
        };

        assert_eq!(
            best_index(&pattern, &options, &HashMap::new()),
            Some("triples_number_type_idx")
        );
    }

    #[test]
    fn cost_planner_emits_bitmap_scan_for_two_join_conditions() {
        let app_id = Uuid::new_v4();
        let attribute_id = Uuid::new_v4();
        let entity = Symbol::new("?entity").unwrap();
        let value = Symbol::new("?value").unwrap();
        let pattern = RawPattern::new(
            Index::new(IndexKey::Ave),
            vec![
                RawTerm::Variable(entity.clone()),
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Variable(value.clone()),
            ],
        )
        .normalize()
        .unwrap();
        let symbol_map = HashMap::from([
            (
                entity.clone(),
                vec![BindingTree::Path(BindingPath::new(
                    0,
                    Component::Entity,
                    IndexKey::Ea,
                ))],
            ),
            (
                value.clone(),
                vec![BindingTree::Path(BindingPath::new(
                    1,
                    Component::Value,
                    IndexKey::Ave,
                ))],
            ),
        ]);
        let cardinalities = HashMap::from([(entity, 5_u64), (value, 4_u64)]);
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attribute_id, 1_000)]),
            ..MatchCteOptions::default()
        };

        let plan = build_match_cte(
            &MatchCteContext {
                prefix: "m-",
                app_id,
                pattern_index: 2,
                start_of_group: false,
                symbol_map: &symbol_map,
                symbol_cardinalities: &cardinalities,
                additional_joins: &HashMap::new(),
                options: &options,
                row_estimate: Some(4),
                suppress_row_hint: false,
            },
            &pattern,
        )
        .unwrap();

        assert!(plan.hints.contains(&QueryHint::BitmapScan {
            table_alias: "t2".to_owned(),
            index: "triples_pkey".to_owned(),
        }));
    }
}
