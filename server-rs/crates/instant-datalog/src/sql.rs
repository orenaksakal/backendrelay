use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    CteJoin, FromTable, MatchCte, MatchQueryPlan, Materialization, NullOrder, OrGatherCte, PgType,
    PlannedCte, PredicateOperator, QueryHint, SelectCte, SelectExpression, SelectProjection,
    SelectQuery, SelectSource, SelectStatement, StatementProjection, WhereColumn, WhereExpression,
    WherePredicate,
};

/// PostgreSQL's `json_build_object` accepts at most 100 arguments: one key and
/// one aggregated value for each match query.
pub const MAX_BATCH_MATCH_QUERIES: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BatchResultKind {
    Rows,
    Count,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NestedResultKind {
    Rows,
    Count,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NestedResultTable {
    pub table: String,
    pub kind: NestedResultKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NestedSqlPlan {
    pub ctes: Vec<PlannedCte>,
    pub hints: Vec<QueryHint>,
    pub result_tables: Vec<NestedResultTable>,
}

/// A PostgreSQL parameter together with the type information needed by the executor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum SqlParameter {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Json(Value),
    Uuid(Uuid),
    Timestamp(DateTime<Utc>),
    Array {
        values: Vec<Value>,
        element_type: PgType,
    },
}

/// Executable SQL and its ordered, non-interpolated parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderedQuery {
    pub sql: String,
    pub parameters: Vec<SqlParameter>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SqlRenderError {
    #[error("invalid SQL identifier: {0}")]
    InvalidIdentifier(String),
    #[error("integer parameter is outside PostgreSQL bigint range: {0}")]
    IntegerOutOfRange(u64),
    #[error("value {value} cannot be bound as PostgreSQL type {pg_type:?}")]
    InvalidTypedParameter { value: String, pg_type: PgType },
    #[error("a SELECT query must project at least one expression")]
    EmptySelect,
    #[error("a match query must contain at least one CTE")]
    EmptyCteGraph,
    #[error("a batch query must contain at least one match query")]
    EmptyBatch,
    #[error("a batch query supports at most 50 match queries, got {0}")]
    BatchTooLarge(usize),
    #[error("batch query contains duplicate CTE name {0}")]
    DuplicateCte(String),
    #[error("batch query has {plans} plans but {kinds} result kinds")]
    BatchResultKindCount { plans: usize, kinds: usize },
    #[error("count result {0} cannot include pagination tables")]
    CountResultHasPage(String),
}

/// Renders a complete Datalog match plan as one PostgreSQL query.
///
/// Values are represented only by positional parameters. Identifiers originate
/// from the planner, are validated, normalized from kebab-case to `snake_case`,
/// and are quoted before being emitted.
///
/// # Errors
///
/// Returns [`SqlRenderError`] for malformed identifiers, empty query structures,
/// or JSON integers that cannot be represented as PostgreSQL `bigint` values.
pub fn render_match_query(plan: &MatchQueryPlan) -> Result<RenderedQuery, SqlRenderError> {
    if plan.ctes.is_empty() {
        return Err(SqlRenderError::EmptyCteGraph);
    }
    let mut renderer = Renderer::default();
    let mut sql = String::from("WITH ");
    for (index, cte) in plan.ctes.iter().enumerate() {
        if index != 0 {
            sql.push_str(", ");
        }
        renderer.render_cte(&mut sql, cte)?;
    }
    sql.push_str(" SELECT ");
    Renderer::render_hints(&mut sql, &plan.hints)?;
    Renderer::render_wildcard(&mut sql, Some(&plan.final_table))?;
    sql.push_str(" FROM ");
    Renderer::render_identifier(&mut sql, &plan.final_table)?;
    Ok(RenderedQuery {
        sql,
        parameters: renderer.parameters,
    })
}

/// Renders the reference aggregate shape for a count-only Datalog plan.
///
/// # Errors
///
/// Returns [`SqlRenderError`] for an empty CTE graph or malformed identifiers.
pub fn render_count_query(plan: &MatchQueryPlan) -> Result<RenderedQuery, SqlRenderError> {
    if plan.ctes.is_empty() {
        return Err(SqlRenderError::EmptyCteGraph);
    }
    let mut renderer = Renderer::default();
    let mut sql = String::from("WITH ");
    for (index, cte) in plan.ctes.iter().enumerate() {
        if index != 0 {
            sql.push_str(", ");
        }
        renderer.render_cte(&mut sql, cte)?;
    }
    sql.push_str(" SELECT count(*) AS \"count\" FROM ");
    Renderer::render_identifier(&mut sql, &plan.final_table)?;
    Ok(RenderedQuery {
        sql,
        parameters: renderer.parameters,
    })
}

/// Renders up to 50 independent match plans into the reference server's JSON batch envelope.
///
/// Every plan must use a distinct prefix. Parameters are numbered globally in CTE
/// order and results are keyed by each plan's unnormalized final-table name.
///
/// # Errors
///
/// Returns [`SqlRenderError`] for an empty/oversized batch, empty CTE graph,
/// duplicate CTE names, malformed identifiers, or invalid typed parameters.
pub fn render_batch_match_query(plans: &[MatchQueryPlan]) -> Result<RenderedQuery, SqlRenderError> {
    let (mut renderer, mut sql) = render_batch_ctes(plans)?;
    if plans.iter().any(|plan| plan.page.is_some()) {
        sql.push_str(" SELECT ");
        let mut result_index = 0;
        for plan in plans {
            for table in std::iter::once(&plan.final_table).chain(
                plan.page
                    .iter()
                    .flat_map(|page| [&page.has_next_table, &page.has_previous_table]),
            ) {
                if result_index != 0 {
                    sql.push_str(" || ");
                }
                render_batch_jsonb_rows_object(&mut sql, table)?;
                result_index += 1;
            }
        }
    } else {
        sql.push_str(" SELECT json_build_object(");
        for (index, plan) in plans.iter().enumerate() {
            if index != 0 {
                sql.push_str(", ");
            }
            Renderer::render_string_literal(&mut sql, &plan.final_table);
            sql.push_str(", COALESCE((SELECT json_agg(row_to_json(");
            Renderer::render_identifier(&mut sql, &plan.final_table)?;
            sql.push_str(")) FROM ");
            Renderer::render_identifier(&mut sql, &plan.final_table)?;
            sql.push_str("), '[]'::json)");
        }
        sql.push(')');
    }
    Ok(RenderedQuery {
        sql,
        parameters: std::mem::take(&mut renderer.parameters),
    })
}

/// Renders a heterogeneous Datalog loader tranche containing row and count results.
///
/// Each plan's final table is encoded under its unnormalized table name. Row
/// results are arrays, count results are scalar integers, and row pagination
/// flag tables are included in the same JSONB envelope.
///
/// # Errors
///
/// Returns [`SqlRenderError`] for invalid batches and when the result-kind list
/// does not have exactly one entry per plan.
pub fn render_mixed_batch_match_query(
    plans: &[MatchQueryPlan],
    kinds: &[BatchResultKind],
) -> Result<RenderedQuery, SqlRenderError> {
    if plans.len() != kinds.len() {
        return Err(SqlRenderError::BatchResultKindCount {
            plans: plans.len(),
            kinds: kinds.len(),
        });
    }
    if let Some((plan, _)) = plans
        .iter()
        .zip(kinds)
        .find(|(plan, kind)| **kind == BatchResultKind::Count && plan.page.is_some())
    {
        return Err(SqlRenderError::CountResultHasPage(plan.final_table.clone()));
    }
    let (mut renderer, mut sql) = render_batch_ctes(plans)?;
    sql.push_str(" SELECT ");
    for (index, (plan, kind)) in plans.iter().zip(kinds).enumerate() {
        if index != 0 {
            sql.push_str(" || ");
        }
        match kind {
            BatchResultKind::Rows => {
                render_batch_jsonb_rows_object(&mut sql, &plan.final_table)?;
                if let Some(page) = &plan.page {
                    for table in [&page.has_next_table, &page.has_previous_table] {
                        sql.push_str(" || ");
                        render_batch_jsonb_rows_object(&mut sql, table)?;
                    }
                }
            }
            BatchResultKind::Count => {
                sql.push_str("jsonb_build_object(");
                Renderer::render_string_literal(&mut sql, &plan.final_table);
                sql.push_str(", (SELECT count(*) FROM ");
                Renderer::render_identifier(&mut sql, &plan.final_table)?;
                sql.push_str("))");
            }
        }
    }
    Ok(RenderedQuery {
        sql,
        parameters: std::mem::take(&mut renderer.parameters),
    })
}

fn render_batch_ctes(plans: &[MatchQueryPlan]) -> Result<(Renderer, String), SqlRenderError> {
    if plans.is_empty() {
        return Err(SqlRenderError::EmptyBatch);
    }
    if plans.len() > MAX_BATCH_MATCH_QUERIES {
        return Err(SqlRenderError::BatchTooLarge(plans.len()));
    }
    let mut names = HashSet::new();
    for plan in plans {
        if plan.ctes.is_empty() {
            return Err(SqlRenderError::EmptyCteGraph);
        }
        for cte in &plan.ctes {
            let name = match cte {
                PlannedCte::Match(cte) => &cte.name,
                PlannedCte::OrGather(cte) => &cte.name,
                PlannedCte::Select(cte) => &cte.name,
            };
            if !names.insert(name) {
                return Err(SqlRenderError::DuplicateCte(name.clone()));
            }
        }
    }
    let mut renderer = Renderer::default();
    let mut sql = String::from("WITH ");
    let mut cte_index = 0;
    for plan in plans {
        for cte in &plan.ctes {
            if cte_index != 0 {
                sql.push_str(", ");
            }
            renderer.render_cte(&mut sql, cte)?;
            cte_index += 1;
        }
    }
    Ok((renderer, sql))
}

fn render_batch_jsonb_rows_object(sql: &mut String, table: &str) -> Result<(), SqlRenderError> {
    sql.push_str("jsonb_build_object(");
    Renderer::render_string_literal(sql, table);
    sql.push_str(", COALESCE((SELECT jsonb_agg(to_jsonb(");
    Renderer::render_identifier(sql, table)?;
    sql.push_str(")) FROM ");
    Renderer::render_identifier(sql, table)?;
    sql.push_str("), '[]'::jsonb))");
    Ok(())
}

/// Renders the JVM-compatible single-round-trip envelope for a recursive query tree.
///
/// PostgreSQL limits `json_build_object` to 100 arguments, so result tables are
/// emitted as an array of objects containing at most 50 table/value pairs each.
/// Row tables contain JSON arrays and count tables contain scalar integers.
///
/// # Errors
///
/// Returns [`SqlRenderError`] for an empty CTE/result graph, duplicate CTE names,
/// malformed identifiers, or invalid typed parameters.
pub fn render_nested_match_query(plan: &NestedSqlPlan) -> Result<RenderedQuery, SqlRenderError> {
    render_nested_query(plan, false)
}

/// Renders native schema/index constants while retaining all data values as binds.
///
/// Only UUIDs compared with app/attribute columns, UUID attribute arrays, and
/// booleans compared with physical index flags may be literals. This lets a
/// connection-local prepared plan prove partial-index predicates without fixing
/// an actor, entity, cursor, permission value, or result. Ordinary renderers keep
/// their existing fully parameterized SQL.
///
/// # Errors
///
/// Returns the same validation errors as [`render_nested_match_query`].
pub fn render_nested_match_query_with_structural_constants(
    plan: &NestedSqlPlan,
) -> Result<RenderedQuery, SqlRenderError> {
    render_nested_query(plan, true)
}

fn render_nested_query(
    plan: &NestedSqlPlan,
    structural_constants: bool,
) -> Result<RenderedQuery, SqlRenderError> {
    if plan.ctes.is_empty() {
        return Err(SqlRenderError::EmptyCteGraph);
    }
    if plan.result_tables.is_empty() {
        return Err(SqlRenderError::EmptyBatch);
    }
    let mut names = HashSet::new();
    for cte in &plan.ctes {
        let name = match cte {
            PlannedCte::Match(cte) => &cte.name,
            PlannedCte::OrGather(cte) => &cte.name,
            PlannedCte::Select(cte) => &cte.name,
        };
        if !names.insert(name) {
            return Err(SqlRenderError::DuplicateCte(name.clone()));
        }
    }

    let mut renderer = Renderer {
        structural_constants,
        ..Renderer::default()
    };
    let mut sql = String::from("WITH ");
    for (index, cte) in plan.ctes.iter().enumerate() {
        if index != 0 {
            sql.push_str(", ");
        }
        renderer.render_cte(&mut sql, cte)?;
    }
    sql.push_str(" SELECT ");
    Renderer::render_hints(&mut sql, &plan.hints)?;
    sql.push_str("json_build_array(");
    for (chunk_index, chunk) in plan
        .result_tables
        .chunks(MAX_BATCH_MATCH_QUERIES)
        .enumerate()
    {
        if chunk_index != 0 {
            sql.push_str(", ");
        }
        sql.push_str("json_build_object(");
        for (index, result) in chunk.iter().enumerate() {
            if index != 0 {
                sql.push_str(", ");
            }
            Renderer::render_string_literal(&mut sql, &result.table);
            match result.kind {
                NestedResultKind::Rows => {
                    sql.push_str(", COALESCE((SELECT json_agg(row_to_json(");
                    Renderer::render_identifier(&mut sql, &result.table)?;
                    sql.push_str(")) FROM ");
                    Renderer::render_identifier(&mut sql, &result.table)?;
                    sql.push_str("), '[]'::json)");
                }
                NestedResultKind::Count => {
                    sql.push_str(", COALESCE((SELECT count(*) FROM ");
                    Renderer::render_identifier(&mut sql, &result.table)?;
                    sql.push_str("), 0)");
                }
            }
        }
        sql.push(')');
    }
    sql.push(')');
    Ok(RenderedQuery {
        sql,
        parameters: renderer.parameters,
    })
}

/// Renders a standalone scalar subquery-shaped SELECT.
///
/// # Errors
///
/// Returns [`SqlRenderError`] when an identifier or parameter cannot be rendered.
pub fn render_select_query(query: &SelectQuery) -> Result<RenderedQuery, SqlRenderError> {
    let mut renderer = Renderer::default();
    let mut sql = String::new();
    renderer.render_select(&mut sql, query)?;
    Ok(RenderedQuery {
        sql,
        parameters: renderer.parameters,
    })
}

#[derive(Default)]
struct Renderer {
    parameters: Vec<SqlParameter>,
    structural_constants: bool,
}

impl Renderer {
    fn parameter(&mut self, parameter: SqlParameter) -> String {
        self.parameters.push(parameter);
        format!("${}", self.parameters.len())
    }

    fn render_cte(&mut self, sql: &mut String, cte: &PlannedCte) -> Result<(), SqlRenderError> {
        match cte {
            PlannedCte::Match(cte) => self.render_match_cte(sql, cte),
            PlannedCte::OrGather(cte) => self.render_or_gather_cte(sql, cte),
            PlannedCte::Select(cte) => self.render_select_cte(sql, cte),
        }
    }

    fn render_select_cte(
        &mut self,
        sql: &mut String,
        cte: &SelectCte,
    ) -> Result<(), SqlRenderError> {
        Self::render_cte_header(sql, &cte.name, cte.materialization)?;
        self.render_statement(sql, &cte.statement)?;
        sql.push(')');
        Ok(())
    }

    fn render_statement(
        &mut self,
        sql: &mut String,
        statement: &SelectStatement,
    ) -> Result<(), SqlRenderError> {
        if statement.select.is_empty() {
            return Err(SqlRenderError::EmptySelect);
        }
        sql.push_str("SELECT ");
        if !statement.distinct_on.is_empty() {
            sql.push_str("DISTINCT ON (");
            for (index, expression) in statement.distinct_on.iter().enumerate() {
                if index != 0 {
                    sql.push_str(", ");
                }
                self.render_expression(sql, expression)?;
            }
            sql.push_str(") ");
        }
        self.render_statement_projections(sql, &statement.select)?;
        if !statement.from.is_empty() {
            sql.push_str(" FROM ");
            for (index, source) in statement.from.iter().enumerate() {
                if index != 0 {
                    sql.push_str(", ");
                }
                match source {
                    SelectSource::Table(table) => Self::render_from_table(sql, table)?,
                    SelectSource::Subquery { query, alias } => {
                        sql.push('(');
                        self.render_statement(sql, query)?;
                        sql.push_str(") AS ");
                        Self::render_identifier(sql, alias)?;
                    }
                    SelectSource::Lateral { query, alias } => {
                        sql.push_str("LATERAL (");
                        self.render_statement(sql, query)?;
                        sql.push_str(") AS ");
                        Self::render_identifier(sql, alias)?;
                    }
                }
            }
        }
        if let Some(predicate) = &statement.predicate {
            sql.push_str(" WHERE ");
            self.render_predicate(sql, predicate)?;
        }
        if !statement.order_by.is_empty() {
            sql.push_str(" ORDER BY ");
            for (index, order) in statement.order_by.iter().enumerate() {
                if index != 0 {
                    sql.push_str(", ");
                }
                self.render_expression(sql, &order.expression)?;
                match order.direction {
                    crate::OrderDirection::Asc => sql.push_str(" ASC"),
                    crate::OrderDirection::Desc => sql.push_str(" DESC"),
                }
                if let Some(nulls) = order.nulls {
                    match nulls {
                        NullOrder::First => sql.push_str(" NULLS FIRST"),
                        NullOrder::Last => sql.push_str(" NULLS LAST"),
                    }
                }
            }
        }
        if let Some(limit) = statement.limit {
            sql.push_str(" LIMIT ");
            sql.push_str(&limit.to_string());
        }
        if let Some(offset) = statement.offset {
            sql.push_str(" OFFSET ");
            sql.push_str(&offset.to_string());
        }
        Ok(())
    }

    fn render_statement_projections(
        &mut self,
        sql: &mut String,
        projections: &[StatementProjection],
    ) -> Result<(), SqlRenderError> {
        for (index, projection) in projections.iter().enumerate() {
            if index != 0 {
                sql.push_str(", ");
            }
            match &projection.expression {
                SelectExpression::Expression(expression) => {
                    self.render_expression(sql, expression)?;
                }
                SelectExpression::Exists(statement) => {
                    sql.push_str("EXISTS (");
                    self.render_statement(sql, statement)?;
                    sql.push(')');
                }
                SelectExpression::RowNumber => sql.push_str("ROW_NUMBER() OVER ()"),
            }
            if let Some(alias) = &projection.alias {
                sql.push_str(" AS ");
                Self::render_identifier(sql, alias)?;
            }
        }
        Ok(())
    }

    fn render_match_cte(&mut self, sql: &mut String, cte: &MatchCte) -> Result<(), SqlRenderError> {
        Self::render_cte_header(sql, &cte.name, cte.materialization)?;
        sql.push_str("SELECT ");
        self.render_projections(sql, &cte.select)?;
        sql.push_str(" FROM ");
        for (index, table) in cte.from.iter().enumerate() {
            if index != 0 {
                sql.push_str(", ");
            }
            Self::render_from_table(sql, table)?;
        }
        sql.push_str(" WHERE ");
        self.render_predicate(sql, &cte.predicate)?;
        sql.push(')');
        Ok(())
    }

    fn render_or_gather_cte(
        &mut self,
        sql: &mut String,
        cte: &OrGatherCte,
    ) -> Result<(), SqlRenderError> {
        Self::render_cte_header(sql, &cte.name, cte.materialization)?;
        sql.push_str("SELECT ");
        self.render_projections(sql, &cte.select)?;
        sql.push_str(" FROM ");
        Self::render_from_table(sql, &cte.from)?;
        for join in &cte.full_joins {
            self.render_join(sql, " FULL JOIN ", join)?;
        }
        for join in &cte.left_joins {
            self.render_join(sql, " LEFT JOIN ", join)?;
        }
        if let Some(predicate) = &cte.predicate {
            sql.push_str(" WHERE ");
            self.render_predicate(sql, predicate)?;
        }
        sql.push(')');
        Ok(())
    }

    fn render_cte_header(
        sql: &mut String,
        name: &str,
        materialization: Materialization,
    ) -> Result<(), SqlRenderError> {
        Self::render_identifier(sql, name)?;
        match materialization {
            Materialization::Materialized => sql.push_str(" AS MATERIALIZED ("),
            Materialization::NotMaterialized => sql.push_str(" AS NOT MATERIALIZED ("),
        }
        Ok(())
    }

    fn render_join(
        &mut self,
        sql: &mut String,
        join_type: &str,
        join: &CteJoin,
    ) -> Result<(), SqlRenderError> {
        sql.push_str(join_type);
        Self::render_identifier(sql, &join.table)?;
        sql.push_str(" ON ");
        if let Some(predicate) = &join.predicate {
            self.render_predicate(sql, predicate)
        } else {
            sql.push_str("TRUE");
            Ok(())
        }
    }

    fn render_projections(
        &mut self,
        sql: &mut String,
        projections: &[SelectProjection],
    ) -> Result<(), SqlRenderError> {
        if projections.is_empty() {
            return Err(SqlRenderError::EmptySelect);
        }
        for (index, projection) in projections.iter().enumerate() {
            if index != 0 {
                sql.push_str(", ");
            }
            self.render_expression(sql, &projection.expression)?;
            if let Some(alias) = &projection.alias {
                sql.push_str(" AS ");
                Self::render_identifier(sql, alias)?;
            }
        }
        Ok(())
    }

    fn render_from_table(sql: &mut String, from: &FromTable) -> Result<(), SqlRenderError> {
        Self::render_identifier(sql, &from.table)?;
        if let Some(alias) = &from.alias {
            sql.push_str(" AS ");
            Self::render_identifier(sql, alias)?;
        }
        Ok(())
    }

    fn render_select(
        &mut self,
        sql: &mut String,
        query: &SelectQuery,
    ) -> Result<(), SqlRenderError> {
        if query.select.is_empty() {
            return Err(SqlRenderError::EmptySelect);
        }
        sql.push_str("SELECT ");
        for (index, expression) in query.select.iter().enumerate() {
            if index != 0 {
                sql.push_str(", ");
            }
            self.render_expression(sql, expression)?;
        }
        sql.push_str(" FROM ");
        Self::render_identifier(sql, &query.table)?;
        sql.push_str(" AS ");
        Self::render_identifier(sql, &query.alias)?;
        sql.push_str(" WHERE ");
        self.render_predicate(sql, &query.predicate)
    }

    fn render_predicate(
        &mut self,
        sql: &mut String,
        predicate: &WherePredicate,
    ) -> Result<(), SqlRenderError> {
        match predicate {
            WherePredicate::Compare {
                operator,
                left,
                right,
            } => self.render_comparison(sql, *operator, left, right),
            WherePredicate::And(predicates) => {
                self.render_boolean_group(sql, "AND", "TRUE", predicates)
            }
            WherePredicate::Or(predicates) => {
                self.render_boolean_group(sql, "OR", "FALSE", predicates)
            }
            WherePredicate::In {
                negated,
                expression,
                query,
            } => {
                sql.push('(');
                self.render_expression(sql, expression)?;
                if *negated {
                    sql.push_str(" NOT IN (");
                } else {
                    sql.push_str(" IN (");
                }
                self.render_select(sql, query)?;
                sql.push_str("))");
                Ok(())
            }
            WherePredicate::Exists { negated, query } => {
                sql.push_str(if *negated {
                    "(NOT EXISTS ("
                } else {
                    "(EXISTS ("
                });
                self.render_select(sql, query)?;
                sql.push_str("))");
                Ok(())
            }
            WherePredicate::ExistsProbe { query } => {
                sql.push_str("(EXISTS (");
                self.render_select(sql, query)?;
                // Fixed compiler syntax, not a user value or a row limit.
                sql.push_str(" OFFSET 0))");
                Ok(())
            }
            WherePredicate::Truthy(expression) => self.render_expression(sql, expression),
        }
    }

    fn render_comparison(
        &mut self,
        sql: &mut String,
        operator: PredicateOperator,
        left: &WhereExpression,
        right: &WhereExpression,
    ) -> Result<(), SqlRenderError> {
        let left_null = matches!(left, WhereExpression::Literal(Value::Null));
        let right_null = matches!(right, WhereExpression::Literal(Value::Null));
        if (left_null || right_null)
            && matches!(
                operator,
                PredicateOperator::Equal | PredicateOperator::NotEqual
            )
        {
            sql.push('(');
            self.render_expression(sql, if left_null { right } else { left })?;
            if operator == PredicateOperator::Equal {
                sql.push_str(" IS NULL)");
            } else {
                sql.push_str(" IS NOT NULL)");
            }
            return Ok(());
        }

        sql.push('(');
        self.render_expression(sql, left)?;
        sql.push(' ');
        sql.push_str(match operator {
            PredicateOperator::Equal => "=",
            PredicateOperator::NotEqual => "<>",
            PredicateOperator::IsDistinctFrom => "IS DISTINCT FROM",
            PredicateOperator::GreaterThan => ">",
            PredicateOperator::GreaterThanOrEqual => ">=",
            PredicateOperator::LessThan => "<",
            PredicateOperator::LessThanOrEqual => "<=",
            PredicateOperator::Like => "LIKE",
            PredicateOperator::CaseInsensitiveLike => "ILIKE",
        });
        sql.push(' ');
        if !self.structural_constants || !render_structural_constant(sql, left, right) {
            self.render_expression_with_type(sql, right, expression_type(left))?;
        }
        sql.push(')');
        Ok(())
    }

    fn render_boolean_group(
        &mut self,
        sql: &mut String,
        operator: &str,
        identity: &str,
        predicates: &[WherePredicate],
    ) -> Result<(), SqlRenderError> {
        if predicates.is_empty() {
            sql.push_str(identity);
            return Ok(());
        }
        sql.push('(');
        for (index, predicate) in predicates.iter().enumerate() {
            if index != 0 {
                sql.push(' ');
                sql.push_str(operator);
                sql.push(' ');
            }
            self.render_predicate(sql, predicate)?;
        }
        sql.push(')');
        Ok(())
    }

    fn render_expression(
        &mut self,
        sql: &mut String,
        expression: &WhereExpression,
    ) -> Result<(), SqlRenderError> {
        match expression {
            WhereExpression::Column(column) => {
                if let Some(qualifier) = &column.qualifier {
                    Self::render_identifier(sql, qualifier)?;
                    sql.push('.');
                }
                Self::render_identifier(sql, column.column.as_str())
            }
            WhereExpression::NamedColumn(name)
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) =>
            {
                sql.push_str(name);
                Ok(())
            }
            WhereExpression::NamedColumn(name) => Self::render_identifier_path(sql, name),
            WhereExpression::Literal(value) => self.render_literal(sql, value),
            WhereExpression::Uuid(value) => {
                let parameter = self.parameter(SqlParameter::Uuid(*value));
                sql.push_str(&parameter);
                Ok(())
            }
            WhereExpression::Wildcard(qualifier) => {
                Self::render_wildcard(sql, qualifier.as_deref())
            }
            WhereExpression::Inline(value) => {
                sql.push('\'');
                sql.push_str(&value.replace('\'', "''"));
                sql.push('\'');
                Ok(())
            }
            WhereExpression::Cast {
                expression,
                pg_type,
            } => {
                sql.push('(');
                self.render_expression_with_type(sql, expression, Some(*pg_type))?;
                sql.push_str(")::");
                sql.push_str(pg_type.as_str());
                Ok(())
            }
            WhereExpression::Function {
                function,
                arguments,
            } => {
                sql.push_str(function.as_str());
                sql.push('(');
                for (index, argument) in arguments.iter().enumerate() {
                    if index != 0 {
                        sql.push_str(", ");
                    }
                    self.render_expression(sql, argument)?;
                }
                sql.push(')');
                Ok(())
            }
            WhereExpression::Coalesce(expressions) => {
                sql.push_str("COALESCE(");
                for (index, expression) in expressions.iter().enumerate() {
                    if index != 0 {
                        sql.push_str(", ");
                    }
                    self.render_expression(sql, expression)?;
                }
                sql.push(')');
                Ok(())
            }
            WhereExpression::Any {
                values,
                element_type,
            } => {
                let parameter = self.parameter(SqlParameter::Array {
                    values: values.clone(),
                    element_type: *element_type,
                });
                sql.push_str("ANY(");
                sql.push_str(&parameter);
                sql.push_str("::");
                sql.push_str(pg_type_array_name(*element_type).as_str());
                sql.push(')');
                Ok(())
            }
            WhereExpression::Subquery(query) => {
                sql.push('(');
                self.render_select(sql, query)?;
                sql.push(')');
                Ok(())
            }
        }
    }

    fn render_expression_with_type(
        &mut self,
        sql: &mut String,
        expression: &WhereExpression,
        pg_type: Option<PgType>,
    ) -> Result<(), SqlRenderError> {
        match (expression, pg_type) {
            (WhereExpression::Literal(Value::Null), Some(PgType::Jsonb)) => {
                sql.push_str(&self.parameter(SqlParameter::Json(Value::Null)));
                Ok(())
            }
            (WhereExpression::Literal(Value::Null), Some(_)) => {
                sql.push_str("NULL");
                Ok(())
            }
            (WhereExpression::Literal(value), Some(pg_type)) => {
                let parameter = typed_parameter(value, pg_type)?;
                sql.push_str(&self.parameter(parameter));
                Ok(())
            }
            _ => self.render_expression(sql, expression),
        }
    }

    fn render_literal(&mut self, sql: &mut String, value: &Value) -> Result<(), SqlRenderError> {
        let parameter = match value {
            Value::Null => {
                sql.push_str("NULL");
                return Ok(());
            }
            Value::Bool(value) => SqlParameter::Boolean(*value),
            Value::Number(value) => {
                if let Some(value) = value.as_i64() {
                    SqlParameter::Integer(value)
                } else if let Some(value) = value.as_u64() {
                    let value = i64::try_from(value)
                        .map_err(|_| SqlRenderError::IntegerOutOfRange(value))?;
                    SqlParameter::Integer(value)
                } else {
                    SqlParameter::Float(value.as_f64().expect("JSON number must be finite"))
                }
            }
            Value::String(value) => SqlParameter::Text(value.clone()),
            Value::Array(_) | Value::Object(_) => SqlParameter::Json(value.clone()),
        };
        sql.push_str(&self.parameter(parameter));
        Ok(())
    }

    fn render_hints(sql: &mut String, hints: &[QueryHint]) -> Result<(), SqlRenderError> {
        if hints.is_empty() {
            return Ok(());
        }
        sql.push_str("/*+\n");
        let mut jit_disabled = false;
        for hint in hints {
            match hint {
                QueryHint::DisableJit => {
                    if !jit_disabled {
                        sql.push_str("Set(jit off)\n");
                        jit_disabled = true;
                    }
                }
                QueryHint::IndexScan { table_alias, index } => {
                    sql.push_str("IndexScan(");
                    sql.push_str(&normalize_identifier(table_alias)?);
                    sql.push(' ');
                    sql.push_str(&normalize_identifier(index)?);
                    sql.push_str(")\n");
                }
                QueryHint::BitmapScan { table_alias, index } => {
                    sql.push_str("BitmapScan(");
                    sql.push_str(&normalize_identifier(table_alias)?);
                    sql.push(' ');
                    sql.push_str(&normalize_identifier(index)?);
                    sql.push_str(")\n");
                }
                QueryHint::Rows { tables, estimate } => {
                    if tables.is_empty() {
                        return Err(SqlRenderError::InvalidIdentifier("Rows()".to_owned()));
                    }
                    sql.push_str("Rows(");
                    for (index, table) in tables.iter().enumerate() {
                        if index != 0 {
                            sql.push(' ');
                        }
                        sql.push_str(&normalize_identifier(table)?);
                    }
                    // pg_hint_plan requires # for an absolute row estimate.
                    sql.push_str(" #");
                    sql.push_str(&estimate.to_string());
                    sql.push_str(")\n");
                }
            }
        }
        sql.push_str("*/ ");
        Ok(())
    }

    fn render_wildcard(sql: &mut String, qualifier: Option<&str>) -> Result<(), SqlRenderError> {
        if let Some(qualifier) = qualifier {
            Self::render_identifier(sql, qualifier)?;
            sql.push('.');
        }
        sql.push('*');
        Ok(())
    }

    fn render_identifier_path(sql: &mut String, path: &str) -> Result<(), SqlRenderError> {
        for (index, segment) in path.split('.').enumerate() {
            if index != 0 {
                sql.push('.');
            }
            Self::render_identifier(sql, segment)?;
        }
        Ok(())
    }

    fn render_identifier(sql: &mut String, value: &str) -> Result<(), SqlRenderError> {
        let normalized = normalize_identifier(value)?;
        sql.push('"');
        sql.push_str(&normalized);
        sql.push('"');
        Ok(())
    }

    fn render_string_literal(sql: &mut String, value: &str) {
        sql.push('\'');
        sql.push_str(&value.replace('\'', "''"));
        sql.push('\'');
    }
}

// Match typed planner nodes, never SQL text or arbitrary strings. All rendered
// literal characters come from bool/Uuid formatting; no user text is escaped or
// interpolated here. Entity IDs and UUID-valued data deliberately stay bound.
fn render_structural_constant(
    sql: &mut String,
    left: &WhereExpression,
    right: &WhereExpression,
) -> bool {
    let WhereExpression::Column(column) = left else {
        return false;
    };
    match (&column.column, right) {
        (WhereColumn::AppId | WhereColumn::AttrId, WhereExpression::Uuid(value)) => {
            sql.push('\'');
            sql.push_str(&value.to_string());
            sql.push_str("'::uuid");
        }
        (
            WhereColumn::AppId | WhereColumn::AttrId,
            WhereExpression::Cast {
                expression,
                pg_type: PgType::Uuid,
            },
        ) => {
            let WhereExpression::Literal(Value::String(text)) = expression.as_ref() else {
                return false;
            };
            let Ok(id) = Uuid::parse_str(text) else {
                return false;
            };
            sql.push('\'');
            sql.push_str(&id.to_string());
            sql.push_str("'::uuid");
        }
        (WhereColumn::Index(_), WhereExpression::Literal(Value::Bool(value))) => {
            sql.push_str(if *value { "TRUE" } else { "FALSE" });
        }
        (
            WhereColumn::AttrId,
            WhereExpression::Any {
                values,
                element_type: PgType::Uuid,
            },
        ) => {
            let Some(ids) = values
                .iter()
                .map(|value| value.as_str().and_then(|text| Uuid::parse_str(text).ok()))
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            sql.push_str("ANY(ARRAY[");
            for (index, id) in ids.into_iter().enumerate() {
                if index != 0 {
                    sql.push_str(", ");
                }
                sql.push('\'');
                sql.push_str(&id.to_string());
                sql.push('\'');
            }
            sql.push_str("]::uuid[])");
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
#[path = "structural_constants_tests.rs"]
mod structural_constants_tests;

fn normalize_identifier(value: &str) -> Result<String, SqlRenderError> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(SqlRenderError::InvalidIdentifier(value.to_owned()));
    }
    Ok(value.replace('-', "_"))
}

fn pg_type_array_name(pg_type: PgType) -> String {
    format!("{}[]", pg_type.as_str())
}

fn expression_type(expression: &WhereExpression) -> Option<PgType> {
    use crate::{SqlFunction, WhereColumn};

    match expression {
        WhereExpression::Column(column) => Some(match column.column {
            WhereColumn::AppId | WhereColumn::EntityId | WhereColumn::AttrId => PgType::Uuid,
            WhereColumn::Value => PgType::Jsonb,
            WhereColumn::CreatedAt => PgType::Bigint,
            WhereColumn::CheckedDataType => PgType::CheckedDataType,
            WhereColumn::Index(_) => PgType::Boolean,
        }),
        WhereExpression::Cast { pg_type, .. }
        | WhereExpression::Any {
            element_type: pg_type,
            ..
        } => Some(*pg_type),
        WhereExpression::Function { function, .. } => Some(match function {
            SqlFunction::JsonNullToNull => PgType::Jsonb,
            SqlFunction::JsonUuidToUuid => PgType::Uuid,
            SqlFunction::ExtractDateValue => PgType::TimestampWithTimeZone,
            SqlFunction::ExtractNumberValue => PgType::Float8,
            SqlFunction::ExtractStringValue => PgType::Text,
            SqlFunction::ExtractBooleanValue => PgType::Boolean,
        }),
        _ => None,
    }
}

fn typed_parameter(value: &Value, pg_type: PgType) -> Result<SqlParameter, SqlRenderError> {
    let invalid = || SqlRenderError::InvalidTypedParameter {
        value: value.to_string(),
        pg_type,
    };
    match pg_type {
        PgType::Bigint => value
            .as_i64()
            .map(SqlParameter::Integer)
            .ok_or_else(invalid),
        PgType::Jsonb => Ok(SqlParameter::Json(value.clone())),
        PgType::Uuid => value
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(SqlParameter::Uuid)
            .ok_or_else(invalid),
        PgType::TimestampWithTimeZone => value
            .as_str()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| SqlParameter::Timestamp(value.with_timezone(&Utc)))
            .ok_or_else(invalid),
        PgType::Float8 => value.as_f64().map(SqlParameter::Float).ok_or_else(invalid),
        PgType::Text => value
            .as_str()
            .map(|value| SqlParameter::Text(value.to_owned()))
            .ok_or_else(invalid),
        PgType::Boolean => value
            .as_bool()
            .map(SqlParameter::Boolean)
            .ok_or_else(invalid),
        PgType::CheckedDataType => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        IndexKey, Materialization, PlannedCte, SelectCte, SelectExpression, SelectStatement,
        SqlFunction, StatementProjection, WhereColumn, WhereColumnRef,
    };
    use serde_json::json;

    #[test]
    fn renders_null_semantics_and_typed_json_without_interpolation() {
        let query = SelectQuery {
            select: vec![WhereExpression::Wildcard(Some("t-lookup".to_owned()))],
            table: "triples".to_owned(),
            alias: "t-lookup".to_owned(),
            predicate: Box::new(WherePredicate::And(vec![
                WherePredicate::Compare {
                    operator: PredicateOperator::Equal,
                    left: WhereExpression::Literal(Value::Null),
                    right: WhereExpression::Column(WhereColumnRef::unqualified(
                        WhereColumn::CreatedAt,
                    )),
                },
                WherePredicate::Compare {
                    operator: PredicateOperator::Equal,
                    left: WhereExpression::Column(WhereColumnRef::unqualified(WhereColumn::Value)),
                    right: WhereExpression::Cast {
                        expression: Box::new(WhereExpression::Literal(json!("Robert'); DROP"))),
                        pg_type: PgType::Jsonb,
                    },
                },
            ])),
        };

        let rendered = render_select_query(&query).unwrap();
        assert_eq!(
            rendered.sql,
            "SELECT \"t_lookup\".* FROM \"triples\" AS \"t_lookup\" WHERE ((\"created_at\" IS NULL) AND (\"value\" = ($1)::jsonb))"
        );
        assert_eq!(
            rendered.parameters,
            vec![SqlParameter::Json(json!("Robert'); DROP"))]
        );
        assert!(!rendered.sql.contains("DROP"));
    }

    #[test]
    fn correlated_exists_fence_binds_values_and_preserves_reference_exists() {
        let probe = SelectQuery {
            select: vec![WhereExpression::NamedColumn("1".into())],
            table: "triples".into(),
            alias: "probe".into(),
            predicate: Box::new(WherePredicate::Compare {
                operator: PredicateOperator::Equal,
                left: WhereExpression::column(WhereColumn::Value),
                right: WhereExpression::Literal(json!("never-inline-probe-value")),
            }),
        };
        let predicate = WherePredicate::ExistsProbe {
            query: probe.clone(),
        };
        assert_eq!(predicate.to_honey_sql_json()[1][":offset"], json!(0));
        let mut outer = SelectQuery {
            select: vec![WhereExpression::NamedColumn("1".into())],
            table: "triples".into(),
            alias: "outer".into(),
            predicate: Box::new(predicate),
        };
        let fenced = render_select_query(&outer).unwrap();
        assert!(fenced.sql.ends_with(" OFFSET 0))"));
        assert!(!fenced.sql.contains("never-inline"));
        outer.predicate = Box::new(WherePredicate::Exists {
            negated: false,
            query: probe,
        });
        let reference = render_select_query(&outer).unwrap();
        assert_eq!(fenced.sql.replace(" OFFSET 0", ""), reference.sql);
        assert_eq!(fenced.parameters, reference.parameters);
    }

    #[test]
    fn renders_typed_any_arrays() {
        let query = SelectQuery {
            select: vec![WhereExpression::column(WhereColumn::EntityId)],
            table: "triples".to_owned(),
            alias: "t".to_owned(),
            predicate: Box::new(WherePredicate::Compare {
                operator: PredicateOperator::Equal,
                left: WhereExpression::column(WhereColumn::AttrId),
                right: WhereExpression::Any {
                    values: vec![json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")],
                    element_type: PgType::Uuid,
                },
            }),
        };

        let rendered = render_select_query(&query).unwrap();
        assert_eq!(
            rendered.sql,
            "SELECT \"entity_id\" FROM \"triples\" AS \"t\" WHERE (\"attr_id\" = ANY($1::uuid[]))"
        );
        assert!(matches!(
            rendered.parameters.as_slice(),
            [SqlParameter::Array {
                element_type: PgType::Uuid,
                ..
            }]
        ));
    }

    #[test]
    fn nested_results_are_partitioned_into_fifty_table_objects() {
        let plan = NestedSqlPlan {
            ctes: vec![PlannedCte::Select(SelectCte {
                name: "result-0".to_owned(),
                statement: SelectStatement {
                    select: vec![StatementProjection {
                        expression: SelectExpression::Expression(WhereExpression::Literal(json!(
                            1
                        ))),
                        alias: Some("value".to_owned()),
                    }],
                    ..SelectStatement::default()
                },
                materialization: Materialization::Materialized,
            })],
            hints: Vec::new(),
            result_tables: (0..51)
                .map(|_| NestedResultTable {
                    table: "result-0".to_owned(),
                    kind: NestedResultKind::Rows,
                })
                .collect(),
        };
        let rendered = render_nested_match_query(&plan).unwrap();
        assert_eq!(rendered.sql.matches("json_build_object(").count(), 2);
        assert_eq!(rendered.sql.matches("json_agg(row_to_json(").count(), 51);
    }

    #[test]
    fn binds_casts_and_extraction_comparisons_as_native_driver_types() {
        let entity_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let query = SelectQuery {
            select: vec![WhereExpression::Wildcard(None)],
            table: "triples".to_owned(),
            alias: "t".to_owned(),
            predicate: Box::new(WherePredicate::And(vec![
                WherePredicate::Compare {
                    operator: PredicateOperator::Equal,
                    left: WhereExpression::column(WhereColumn::EntityId),
                    right: WhereExpression::Cast {
                        expression: Box::new(WhereExpression::Literal(json!(entity_id))),
                        pg_type: PgType::Uuid,
                    },
                },
                WherePredicate::Compare {
                    operator: PredicateOperator::GreaterThanOrEqual,
                    left: WhereExpression::Function {
                        function: SqlFunction::ExtractNumberValue,
                        arguments: vec![WhereExpression::column(WhereColumn::Value)],
                    },
                    right: WhereExpression::Literal(json!(18)),
                },
                WherePredicate::Compare {
                    operator: PredicateOperator::LessThan,
                    left: WhereExpression::Function {
                        function: SqlFunction::ExtractDateValue,
                        arguments: vec![WhereExpression::column(WhereColumn::Value)],
                    },
                    right: WhereExpression::Literal(json!("2026-08-27T15:30:00-06:00")),
                },
            ])),
        };

        let rendered = render_select_query(&query).unwrap();
        assert_eq!(rendered.parameters[0], SqlParameter::Uuid(entity_id));
        assert_eq!(rendered.parameters[1], SqlParameter::Float(18.0));
        assert!(matches!(rendered.parameters[2], SqlParameter::Timestamp(_)));
    }

    #[test]
    fn rejects_identifier_injection() {
        let query = SelectQuery {
            select: vec![WhereExpression::column(WhereColumn::Index(IndexKey::Ea))],
            table: "triples; DROP TABLE triples".to_owned(),
            alias: "t".to_owned(),
            predicate: Box::new(WherePredicate::And(Vec::new())),
        };
        assert!(matches!(
            render_select_query(&query),
            Err(SqlRenderError::InvalidIdentifier(_))
        ));
    }

    #[test]
    fn renders_reference_index_hints_in_pg_hint_plan_shape() {
        let mut sql = String::from("SELECT ");
        Renderer::render_hints(
            &mut sql,
            &[QueryHint::IndexScan {
                table_alias: "t0-lookup".to_owned(),
                index: "av_index".to_owned(),
            }],
        )
        .unwrap();
        sql.push('*');
        assert_eq!(sql, "SELECT /*+\nIndexScan(t0_lookup av_index)\n*/ *");
    }

    #[test]
    fn renders_bitmap_index_hints_in_pg_hint_plan_shape() {
        let mut sql = String::from("SELECT ");
        Renderer::render_hints(
            &mut sql,
            &[QueryHint::BitmapScan {
                table_alias: "t2".to_owned(),
                index: "triples_pkey".to_owned(),
            }],
        )
        .unwrap();
        sql.push('*');
        assert_eq!(sql, "SELECT /*+\nBitmapScan(t2 triples_pkey)\n*/ *");
    }

    #[test]
    fn renders_row_count_hints_in_pg_hint_plan_shape() {
        let mut sql = String::from("SELECT ");
        Renderer::render_hints(
            &mut sql,
            &[QueryHint::Rows {
                tables: vec!["m-0".to_owned(), "t1".to_owned()],
                estimate: 42,
            }],
        )
        .unwrap();
        sql.push('*');
        assert_eq!(sql, "SELECT /*+\nRows(m_0 t1 #42)\n*/ *");
    }
}
