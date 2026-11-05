use std::collections::BTreeMap;
use std::hash::BuildHasher;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use uuid::Uuid;

use crate::{
    BindingPath, BindingTree, Component, CteJoin, DataType, FromTable, MatchCteContext,
    MatchQueryContext, MatchQueryPlan, Materialization, NullOrder, OrGatherCte, OrderBy,
    OrderDirection, Pattern, PatternError, PatternMetadata, PatternMetadataNode, PgType,
    PlannedCte, PredicateOperator, SelectCte, SelectExpression, SelectProjection, SelectSource,
    SelectStatement, SqlFunction, StatementProjection, Symbol, Term, WhereColumn, WhereExpression,
    WherePredicate, build_match_cte, joined_row_estimate,
};

const ORDER_VALUE: &str = "order-val";
const ORDER_ENTITY: &str = "order-eid";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PageOrderType {
    CreatedAtTimestamp,
    Checked(DataType),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PageCursor {
    pub entity_id: Uuid,
    pub order_value: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors InstantDB's public page-info contract.
pub struct PageInfo {
    pub limit: Option<u64>,
    pub last: bool,
    pub offset: Option<u64>,
    pub direction: OrderDirection,
    pub order_symbol: Symbol,
    pub order_type: PageOrderType,
    pub order_required: bool,
    pub attr_id: Uuid,
    pub pattern: Pattern,
    pub before: Option<PageCursor>,
    pub before_inclusive: bool,
    pub after: Option<PageCursor>,
    pub after_inclusive: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PagePlanMetadata {
    pub table: String,
    pub has_next_table: String,
    pub has_previous_table: String,
    pub cursor_columns: Vec<String>,
    pub fallback_entity_column: String,
    pub fallback_created_at_column: String,
    pub attr_id: Uuid,
    pub reverse_results: bool,
    pub empty_page_has_previous: bool,
}

#[derive(Clone, Copy)]
enum OrderComponent {
    Entity,
    Attribute,
    Value,
    CreatedAt,
}

#[derive(Clone)]
struct PageRowExpressions {
    entity: WhereExpression,
    order: WhereExpression,
}

impl OrderComponent {
    const fn column(self) -> WhereColumn {
        match self {
            Self::Entity => WhereColumn::EntityId,
            Self::Attribute => WhereColumn::AttrId,
            Self::Value => WhereColumn::Value,
            Self::CreatedAt => WhereColumn::CreatedAt,
        }
    }

    const fn suffix(self) -> &'static str {
        match self {
            Self::Entity => "entity-id",
            Self::Attribute => "attr-id",
            Self::Value => "value",
            Self::CreatedAt => "created-at",
        }
    }
}

/// Adds the reference server's ordered overfetch and page-boundary CTEs to a match plan.
///
/// # Errors
///
/// Returns [`PatternError`] when the ordering symbol is absent, the page pattern is
/// invalid, or `limit + 1` overflows.
#[allow(clippy::too_many_lines)] // The six CTEs share state and mirror one JVM planner pass.
pub fn add_page_info<S: BuildHasher>(
    plan: &mut MatchQueryPlan,
    context: &MatchQueryContext<'_, S>,
    page: &PageInfo,
) -> Result<(), PatternError> {
    let page_index = plan.next_index;
    let order_table = indexed_table(context.prefix, page_index);
    let component = order_component(&page.pattern, &page.order_symbol).ok_or_else(|| {
        PatternError::MissingPageOrderSymbol(page.order_symbol.as_str().to_owned())
    })?;
    let mut options = context.options.clone();
    options.page_info = true;
    let (row_estimate, propagated_estimate, suppress_row_hint) =
        joined_row_estimate(&page.pattern, &options, &plan.symbol_cardinalities);
    // Capture the selector's estimate before page-limit propagation below. A
    // page limit is not a bound on the number of matching entities to order.
    let bind_created_at_order = options.candidate_entity_scans == Some(true)
        && page.order_required
        && page.order_type == PageOrderType::CreatedAtTimestamp
        && page.limit.is_some_and(|limit| (1..=100).contains(&limit))
        && context.additional_joins.is_empty()
        && narrow_created_at_selector(plan, page);
    let mut match_plan = build_match_cte(
        &MatchCteContext {
            prefix: context.prefix,
            app_id: context.app_id,
            pattern_index: page_index,
            // Optional order attributes must be scanned independently and attached
            // with a left join below. A normal linear match would drop entities
            // that do not have an order triple.
            start_of_group: !page.order_required,
            symbol_map: &plan.symbol_map,
            symbol_cardinalities: &plan.symbol_cardinalities,
            additional_joins: context.additional_joins,
            options: &options,
            row_estimate,
            suppress_row_hint,
        },
        &page.pattern,
    )?;
    if options.optional_number_order_scan_hints == Some(false)
        && !page.order_required
        && page.order_type == PageOrderType::Checked(DataType::Number)
        && context.additional_joins.is_empty()
    {
        // The optional order relation is attached by LEFT JOIN below, not by
        // the entity-bound match which the ordinary index cost model assumes.
        // Forcing ea_index can cause one heap probe per historical room match
        // instead of a cheaper typed-number scan/hash join. Keep every SQL
        // predicate, parameter, projection, page boundary and other hint intact.
        let order_alias = match_plan
            .cte
            .from
            .first()
            .and_then(|table| table.alias.as_deref());
        match_plan.hints.retain(|hint| match hint {
            crate::QueryHint::IndexScan { table_alias, .. }
            | crate::QueryHint::BitmapScan { table_alias, .. } => {
                Some(table_alias.as_str()) != order_alias
            }
            crate::QueryHint::Rows { .. } | crate::QueryHint::DisableJit => true,
        });
    }
    if bind_created_at_order {
        // The page-order hint was costed before the filtered join was fenced.
        // In the missing-estimate fallback it can force a full created-at scan
        // for every selected entity. Let PostgreSQL choose entity-bound probes
        // inside the fence; do not retain the old whole-partition row hint.
        let order_alias = match_plan
            .cte
            .from
            .first()
            .and_then(|table| table.alias.as_deref());
        match_plan.hints.retain(|hint| match hint {
            crate::QueryHint::IndexScan { table_alias, .. }
            | crate::QueryHint::BitmapScan { table_alias, .. } => {
                Some(table_alias.as_str()) != order_alias
            }
            crate::QueryHint::Rows { tables, .. } => !tables
                .iter()
                .any(|table| Some(table.as_str()) == order_alias),
            crate::QueryHint::DisableJit => true,
        });
    }
    plan.hints.extend(match_plan.hints);
    if let Some(propagated_estimate) = propagated_estimate {
        let capped_estimate = if options.cap_symbol_map_by_page_limit.unwrap_or(true) {
            page.limit
                .map_or(propagated_estimate, |limit| propagated_estimate.min(limit))
        } else {
            propagated_estimate
        };
        for term in [&page.pattern.entity, &page.pattern.value] {
            if let Term::Variable(symbol) = term {
                plan.symbol_cardinalities
                    .entry(symbol.clone())
                    .and_modify(|estimate| *estimate = (*estimate).min(capped_estimate))
                    .or_insert(capped_estimate);
            }
        }
    }

    let (mut base, table, cursor_table, consumed_indices, row_expressions) = if page.order_required
    {
        (
            match_statement(match_plan.cte),
            order_table.clone(),
            order_table.clone(),
            1,
            PageRowExpressions {
                entity: WhereExpression::column(WhereColumn::EntityId),
                order: WhereExpression::column(component.column()),
            },
        )
    } else {
        optional_order_statement(
            plan,
            context.prefix,
            match_plan.cte,
            &page.pattern,
            page.order_type,
            component,
        )?
    };
    if bind_created_at_order
        && options.keyed_small_page_scans == Some(true)
        && known_narrow_required_equality(plan, page, &options)
    {
        crate::parameterize_keyed_entity_scan(&mut base, context.app_id);
    }
    let raw_order = row_expressions.order.clone();
    let order_value = extraction_function(page.order_type).map_or_else(
        || raw_order.clone(),
        |function| WhereExpression::function(function, raw_order.clone()),
    );
    if page.order_required
        && let PageOrderType::Checked(data_type) = page.order_type
    {
        base.predicate = Some(and(
            base.predicate.take(),
            WherePredicate::compare(
                PredicateOperator::Equal,
                WhereExpression::column(WhereColumn::CheckedDataType),
                WhereExpression::Cast {
                    expression: Box::new(WhereExpression::Inline(
                        data_type_name(data_type).to_owned(),
                    )),
                    pg_type: PgType::CheckedDataType,
                },
            ),
        ));
    }
    let original_select = std::mem::take(&mut base.select);
    base.select = vec![
        projection(order_value.clone(), Some(ORDER_VALUE)),
        projection(row_expressions.entity.clone(), Some(ORDER_ENTITY)),
    ];
    base.select.extend(original_select);
    base.distinct_on = vec![
        WhereExpression::NamedColumn(ORDER_VALUE.to_owned()),
        WhereExpression::NamedColumn(ORDER_ENTITY.to_owned()),
    ];

    let effective_direction = if page.last {
        page.direction.reverse()
    } else {
        page.direction
    };
    base.order_by = page_order(
        effective_direction,
        extraction_function(page.order_type).is_some() || !page.order_required,
    );
    // Boundary probes start from the unconstrained match query and add their own
    // first/last-row cursor, matching the JVM planner. Reusing the paged query
    // would retain the public before/after predicate and hide rows on the other
    // side of that cursor (most visibly `has-previous-page` after an `after`).
    let probe_base = base.clone();
    base.limit = page
        .limit
        .map(|limit| limit.checked_add(1).ok_or(PatternError::PageLimitOverflow))
        .transpose()?;
    base.offset = page.offset;
    if let Some(cursor) = &page.after {
        add_cursor(
            &mut base,
            page,
            cursor_values(cursor),
            CursorSide::After,
            page.after_inclusive,
            &row_expressions,
        );
    }
    if let Some(cursor) = &page.before {
        add_cursor(
            &mut base,
            page,
            cursor_values(cursor),
            CursorSide::Before,
            page.before_inclusive,
            &row_expressions,
        );
    }

    let with_next = format!("{table}-with-next");
    let first = format!("{table}-first");
    let last = format!("{table}-last");
    let has_next = format!("{table}-has-next");
    let has_previous = format!("{table}-has-prev");
    if bind_created_at_order {
        // Without this fence PostgreSQL may walk the entire app/attribute
        // created-at index in order, joining each row to a tiny selector until
        // it finds enough matches. Order only the complete filtered join.
        // Keep DISTINCT, sorting, lookahead, limit/offset and all metadata in
        // their original outer stage. No estimate truncates candidate rows.
        let source = format!("{table}-bound-order");
        let statement = SelectStatement {
            select: std::mem::take(&mut base.select),
            from: std::mem::take(&mut base.from),
            predicate: base.predicate.take(),
            ..SelectStatement::default()
        };
        plan.ctes.push(select_cte(
            source.clone(),
            statement,
            Materialization::Materialized,
        ));
        base.select = vec![projection(WhereExpression::Wildcard(None), None)];
        base.from = vec![table_source(&source, None)];
    }
    plan.ctes.push(select_cte(
        with_next.clone(),
        base.clone(),
        Materialization::Materialized,
    ));
    plan.ctes.push(select_cte(
        table.clone(),
        SelectStatement {
            select: vec![projection(WhereExpression::Wildcard(None), None)],
            from: vec![table_source(&with_next, None)],
            limit: page.limit,
            ..SelectStatement::default()
        },
        Materialization::Materialized,
    ));

    let raw_cursor_column = format!("{cursor_table}-{}", component.suffix());
    plan.ctes.push(select_cte(
        first.clone(),
        cursor_row_statement(&table, &raw_cursor_column),
        Materialization::NotMaterialized,
    ));
    plan.ctes.push(select_cte(
        last.clone(),
        last_cursor_row_statement(&table, &raw_cursor_column),
        Materialization::NotMaterialized,
    ));

    let next_probe = page_probe(
        &probe_base,
        page,
        &with_next,
        &first,
        &last,
        &row_expressions,
        CursorSide::After,
    );
    let previous_probe = page_probe(
        &probe_base,
        page,
        &with_next,
        &first,
        &last,
        &row_expressions,
        CursorSide::Before,
    );
    plan.ctes.push(exists_cte(&has_next, next_probe));
    plan.ctes.push(exists_cte(&has_previous, previous_probe));

    let cursor_columns = metadata_columns(&cursor_table);
    let fallback_created_at_column = last_created_at_column(&plan.pattern_metadata)
        .ok_or(PatternError::MissingPageCreatedAtColumn)?;
    let fallback_entity_column = ORDER_ENTITY.replace('-', "_");
    plan.pattern_metadata
        .push(PatternMetadataNode::Pattern(PatternMetadata {
            cte_columns: cursor_columns.clone(),
            symbol_fields: BTreeMap::new(),
            pattern: page.pattern.clone(),
        }));
    plan.next_index += consumed_indices;
    plan.final_table.clone_from(&table);
    plan.page = Some(PagePlanMetadata {
        table,
        has_next_table: has_next,
        has_previous_table: has_previous,
        cursor_columns,
        fallback_entity_column,
        fallback_created_at_column,
        attr_id: page.attr_id,
        reverse_results: page.last,
        empty_page_has_previous: page.offset.unwrap_or_default() > 0 || page.after.is_some(),
    });
    Ok(())
}

fn known_narrow_required_equality(
    plan: &MatchQueryPlan,
    page: &PageInfo,
    options: &crate::MatchCteOptions,
) -> bool {
    // A small estimate inherited from one OR arm is not a bound on its union.
    // Require a top-level/conjunctive indexed equality on this entity instead.
    plan.pattern_metadata.iter().any(|node| {
        let PatternMetadataNode::Pattern(metadata) = node else {
            return false;
        };
        let pattern = &metadata.pattern;
        if pattern.entity != page.pattern.entity
            || !matches!(
                pattern.index.key,
                crate::IndexKey::Av | crate::IndexKey::Ave
            )
            || !matches!(&pattern.attribute, Term::Constants(values) if values.len() == 1)
        {
            return false;
        }
        let Term::Constants(values) = &pattern.value else {
            return false;
        };
        if values.is_empty() || values.len() > 1_024 {
            return false;
        }
        if pattern.index.key == crate::IndexKey::Av {
            return true;
        }
        crate::pattern_row_estimate(pattern, options).is_some_and(|rows| rows <= 1_024)
    })
}

fn narrow_created_at_selector(plan: &MatchQueryPlan, page: &PageInfo) -> bool {
    let Term::Variable(entity) = &page.pattern.entity else {
        return false;
    };
    if let Some(rows) = plan.symbol_cardinalities.get(entity) {
        return *rows <= 1_024;
    }
    // Missing estimates keep the existing candidate planner's selective-index
    // fallback. Only top-level/conjunctive equalities are required globally;
    // an equality inside an OR arm cannot constrain the complete selector.
    plan.pattern_metadata.iter().any(|node| matches!(node,
        PatternMetadataNode::Pattern(metadata)
            if metadata.pattern.entity == page.pattern.entity
                && matches!(metadata.pattern.index.key, crate::IndexKey::Av | crate::IndexKey::Ave)
                && matches!(&metadata.pattern.attribute, Term::Constants(values) if values.len() == 1)
                && matches!(&metadata.pattern.value, Term::Constants(values) if !values.is_empty() && values.len() <= 1_024)))
}

fn last_created_at_column(metadata: &[PatternMetadataNode]) -> Option<String> {
    metadata.iter().rev().find_map(|node| match node {
        PatternMetadataNode::Pattern(metadata) => metadata.cte_columns.last().cloned(),
        PatternMetadataNode::Or(branches) => branches
            .iter()
            .rev()
            .find_map(|branch| last_created_at_column(branch)),
    })
}

fn optional_order_statement(
    plan: &mut MatchQueryPlan,
    prefix: &str,
    mut order_match: crate::MatchCte,
    pattern: &Pattern,
    order_type: PageOrderType,
    component: OrderComponent,
) -> Result<(SelectStatement, String, String, usize, PageRowExpressions), PatternError> {
    if let PageOrderType::Checked(data_type) = order_type {
        order_match.predicate = and(
            Some(order_match.predicate),
            WherePredicate::compare(
                PredicateOperator::Equal,
                WhereExpression::column(WhereColumn::CheckedDataType),
                WhereExpression::Cast {
                    expression: Box::new(WhereExpression::Inline(
                        data_type_name(data_type).to_owned(),
                    )),
                    pg_type: PgType::CheckedDataType,
                },
            ),
        );
    }

    let Term::Variable(entity_symbol) = &pattern.entity else {
        return Err(PatternError::MissingPageEntityBinding(
            "<constant>".to_owned(),
        ));
    };
    let bindings = plan
        .symbol_map
        .get(entity_symbol)
        .ok_or_else(|| PatternError::MissingPageEntityBinding(entity_symbol.as_str().to_owned()))?;
    let previous_table = plan.final_table.clone();
    let order_table = order_match.name.clone();
    let table = indexed_table(prefix, plan.next_index + 1);
    let gather_table = format!("{table}-optional-source");
    let previous_entity = qualified_symbol_expression(&previous_table, prefix, bindings)
        .ok_or_else(|| PatternError::MissingPageEntityBinding(entity_symbol.as_str().to_owned()))?;
    let order_entity =
        WhereExpression::NamedColumn(format!("{order_table}.{order_table}-entity-id"));

    plan.ctes.push(PlannedCte::Match(order_match));
    plan.ctes.push(PlannedCte::OrGather(OrGatherCte {
        name: gather_table.clone(),
        select: vec![
            SelectProjection {
                expression: WhereExpression::Wildcard(Some(previous_table.clone())),
                alias: None,
            },
            SelectProjection {
                expression: WhereExpression::Wildcard(Some(order_table.clone())),
                alias: None,
            },
        ],
        from: FromTable {
            table: previous_table,
            alias: None,
        },
        full_joins: Vec::new(),
        left_joins: vec![CteJoin {
            table: order_table.clone(),
            predicate: Some(WherePredicate::compare(
                PredicateOperator::Equal,
                previous_entity,
                order_entity,
            )),
        }],
        predicate: None,
        materialization: Materialization::NotMaterialized,
    }));

    let row_expressions = PageRowExpressions {
        entity: qualified_symbol_expression(&gather_table, prefix, bindings).ok_or_else(|| {
            PatternError::MissingPageEntityBinding(entity_symbol.as_str().to_owned())
        })?,
        order: WhereExpression::NamedColumn(format!(
            "{gather_table}.{order_table}-{}",
            component.suffix()
        )),
    };
    Ok((
        SelectStatement {
            select: vec![projection(
                WhereExpression::Wildcard(Some(gather_table.clone())),
                None,
            )],
            from: vec![table_source(&gather_table, None)],
            ..SelectStatement::default()
        },
        table,
        order_table,
        2,
        row_expressions,
    ))
}

fn qualified_symbol_expression(
    source_table: &str,
    prefix: &str,
    bindings: &[BindingTree],
) -> Option<WhereExpression> {
    fn collect(
        expressions: &mut Vec<WhereExpression>,
        source_table: &str,
        prefix: &str,
        tree: &BindingTree,
    ) {
        match tree {
            BindingTree::Path(path) => {
                expressions.push(qualified_binding_expression(source_table, prefix, path));
            }
            BindingTree::And(trees) | BindingTree::Or(trees) => {
                for tree in trees {
                    collect(expressions, source_table, prefix, tree);
                }
            }
        }
    }

    let mut expressions = Vec::new();
    for binding in bindings {
        collect(&mut expressions, source_table, prefix, binding);
    }
    match expressions.len() {
        0 => None,
        1 => expressions.pop(),
        _ => Some(WhereExpression::Coalesce(expressions)),
    }
}

fn qualified_binding_expression(
    source_table: &str,
    prefix: &str,
    binding: &BindingPath,
) -> WhereExpression {
    let suffix = match binding.component {
        Component::Entity => "entity-id",
        Component::Attribute => "attr-id",
        Component::Value => "value",
    };
    let binding_table = indexed_table(prefix, binding.pattern_index);
    let expression =
        WhereExpression::NamedColumn(format!("{source_table}.{binding_table}-{suffix}"));
    if binding.reference_value {
        WhereExpression::function(SqlFunction::JsonUuidToUuid, expression)
    } else {
        expression
    }
}

fn match_statement(cte: crate::MatchCte) -> SelectStatement {
    SelectStatement {
        select: cte.select.into_iter().map(Into::into).collect(),
        from: cte.from.into_iter().map(SelectSource::Table).collect(),
        predicate: Some(cte.predicate),
        ..SelectStatement::default()
    }
}

fn order_component(pattern: &Pattern, symbol: &Symbol) -> Option<OrderComponent> {
    [
        (OrderComponent::Entity, &pattern.entity),
        (OrderComponent::Attribute, &pattern.attribute),
        (OrderComponent::Value, &pattern.value),
        (OrderComponent::CreatedAt, &pattern.created_at),
    ]
    .into_iter()
    .find_map(|(component, term)| match term {
        Term::Variable(candidate) if candidate == symbol => Some(component),
        _ => None,
    })
}

const fn extraction_function(order_type: PageOrderType) -> Option<SqlFunction> {
    match order_type {
        PageOrderType::CreatedAtTimestamp | PageOrderType::Checked(DataType::String) => None,
        PageOrderType::Checked(DataType::Date) => Some(SqlFunction::ExtractDateValue),
        PageOrderType::Checked(DataType::Number) => Some(SqlFunction::ExtractNumberValue),
        PageOrderType::Checked(DataType::Boolean) => Some(SqlFunction::ExtractBooleanValue),
    }
}

const fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::String => "string",
        DataType::Number => "number",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
    }
}

fn page_order(direction: OrderDirection, nullable: bool) -> Vec<OrderBy> {
    vec![
        OrderBy {
            expression: WhereExpression::NamedColumn(ORDER_VALUE.to_owned()),
            direction,
            nulls: nullable.then_some(match direction {
                OrderDirection::Asc => NullOrder::First,
                OrderDirection::Desc => NullOrder::Last,
            }),
        },
        OrderBy {
            expression: WhereExpression::NamedColumn(ORDER_ENTITY.to_owned()),
            direction,
            nulls: None,
        },
    ]
}

fn projection(expression: WhereExpression, alias: Option<&str>) -> StatementProjection {
    StatementProjection {
        expression: SelectExpression::Expression(expression),
        alias: alias.map(str::to_owned),
    }
}

fn table_source(table: &str, alias: Option<&str>) -> SelectSource {
    SelectSource::Table(FromTable {
        table: table.to_owned(),
        alias: alias.map(str::to_owned),
    })
}

fn select_cte(
    name: String,
    statement: SelectStatement,
    materialization: Materialization,
) -> PlannedCte {
    PlannedCte::Select(SelectCte {
        name,
        statement,
        materialization,
    })
}

fn cursor_row_statement(table: &str, raw_cursor_column: &str) -> SelectStatement {
    SelectStatement {
        select: vec![
            projection(
                WhereExpression::NamedColumn(ORDER_ENTITY.to_owned()),
                Some("e"),
            ),
            projection(
                WhereExpression::NamedColumn(format!("{table}.{raw_cursor_column}")),
                Some("sym"),
            ),
        ],
        from: vec![table_source(table, None)],
        limit: Some(1),
        ..SelectStatement::default()
    }
}

fn last_cursor_row_statement(table: &str, raw_cursor_column: &str) -> SelectStatement {
    let subquery = SelectStatement {
        select: vec![
            projection(WhereExpression::Wildcard(Some(table.to_owned())), None),
            StatementProjection {
                expression: SelectExpression::RowNumber,
                alias: Some("sort-id".to_owned()),
            },
        ],
        from: vec![table_source(table, None)],
        ..SelectStatement::default()
    };
    SelectStatement {
        select: vec![
            projection(
                WhereExpression::NamedColumn(ORDER_ENTITY.to_owned()),
                Some("e"),
            ),
            projection(
                WhereExpression::NamedColumn(raw_cursor_column.to_owned()),
                Some("sym"),
            ),
        ],
        from: vec![SelectSource::Subquery {
            query: Box::new(subquery),
            alias: "subquery".to_owned(),
        }],
        order_by: vec![OrderBy {
            expression: WhereExpression::NamedColumn("sort-id".to_owned()),
            direction: OrderDirection::Desc,
            nulls: None,
        }],
        limit: Some(1),
        ..SelectStatement::default()
    }
}

#[derive(Clone, Copy)]
enum CursorSide {
    Before,
    After,
}

fn cursor_values(cursor: &PageCursor) -> (WhereExpression, WhereExpression) {
    (
        WhereExpression::Uuid(cursor.entity_id),
        WhereExpression::Literal(cursor.order_value.clone()),
    )
}

fn add_cursor(
    statement: &mut SelectStatement,
    page: &PageInfo,
    cursor: (WhereExpression, WhereExpression),
    side: CursorSide,
    inclusive: bool,
    row: &PageRowExpressions,
) {
    let predicate = cursor_predicate(page, cursor, side, inclusive, row);
    statement.predicate = Some(and(statement.predicate.take(), predicate));
}

fn cursor_predicate(
    page: &PageInfo,
    (cursor_entity, cursor_value): (WhereExpression, WhereExpression),
    side: CursorSide,
    inclusive: bool,
    row: &PageRowExpressions,
) -> WherePredicate {
    let operator =
        match (side, page.direction) {
            (CursorSide::Before, OrderDirection::Asc)
            | (CursorSide::After, OrderDirection::Desc) => PredicateOperator::LessThan,
            (CursorSide::Before, OrderDirection::Desc)
            | (CursorSide::After, OrderDirection::Asc) => PredicateOperator::GreaterThan,
        };
    let inclusive_operator = inclusive_comparison(operator);
    let entity_operator = if inclusive {
        inclusive_operator
    } else {
        operator
    };
    let raw_order = row.order.clone();
    let order = extraction_function(page.order_type).map_or_else(
        || raw_order.clone(),
        |function| WhereExpression::function(function, raw_order.clone()),
    );
    let cursor_order = typed_cursor_value(cursor_value, page.order_type);
    let strict = WherePredicate::compare(operator, order.clone(), cursor_order.clone());
    let equal_then_entity = WherePredicate::And(vec![
        WherePredicate::compare(
            PredicateOperator::Equal,
            order.clone(),
            cursor_order.clone(),
        ),
        WherePredicate::compare(
            entity_operator,
            row.entity.clone(),
            WhereExpression::Cast {
                expression: Box::new(cursor_entity),
                pg_type: PgType::Uuid,
            },
        ),
    ]);
    if page.order_required {
        WherePredicate::And(vec![
            WherePredicate::compare(inclusive_operator, order, cursor_order),
            WherePredicate::Or(vec![strict, equal_then_entity]),
        ])
    } else {
        let null_side = match operator {
            PredicateOperator::GreaterThan | PredicateOperator::GreaterThanOrEqual => {
                WherePredicate::And(vec![
                    WherePredicate::compare(
                        PredicateOperator::NotEqual,
                        WhereExpression::Literal(Value::Null),
                        order.clone(),
                    ),
                    WherePredicate::compare(
                        PredicateOperator::Equal,
                        WhereExpression::Literal(Value::Null),
                        cursor_order.clone(),
                    ),
                ])
            }
            _ => WherePredicate::And(vec![
                WherePredicate::compare(
                    PredicateOperator::Equal,
                    WhereExpression::Literal(Value::Null),
                    order.clone(),
                ),
                WherePredicate::compare(
                    PredicateOperator::NotEqual,
                    WhereExpression::Literal(Value::Null),
                    cursor_order.clone(),
                ),
            ]),
        };
        let same_value = WherePredicate::Or(vec![
            WherePredicate::And(vec![
                WherePredicate::compare(
                    PredicateOperator::Equal,
                    order.clone(),
                    WhereExpression::Literal(Value::Null),
                ),
                WherePredicate::compare(
                    PredicateOperator::Equal,
                    cursor_order.clone(),
                    WhereExpression::Literal(Value::Null),
                ),
            ]),
            WherePredicate::compare(PredicateOperator::Equal, order, cursor_order),
        ]);
        WherePredicate::Or(vec![
            WherePredicate::Or(vec![strict, null_side]),
            WherePredicate::And(vec![same_value, equal_then_entity]),
        ])
    }
}

const fn inclusive_comparison(operator: PredicateOperator) -> PredicateOperator {
    match operator {
        PredicateOperator::GreaterThan | PredicateOperator::GreaterThanOrEqual => {
            PredicateOperator::GreaterThanOrEqual
        }
        PredicateOperator::LessThan | PredicateOperator::LessThanOrEqual => {
            PredicateOperator::LessThanOrEqual
        }
        other => other,
    }
}

fn typed_cursor_value(expression: WhereExpression, order_type: PageOrderType) -> WhereExpression {
    // Internal boundary probes read the raw JSONB order triple. Unlike a
    // public literal cursor, that column can contain JSON null, which cannot
    // be cast directly to float8. Preserve SQL NULL for a missing order triple
    // and normalize only JSON null; non-null numbers retain the same cast.
    let expression = if order_type == PageOrderType::Checked(DataType::Number)
        && matches!(&expression, WhereExpression::NamedColumn(_))
    {
        WhereExpression::function(SqlFunction::JsonNullToNull, expression)
    } else {
        expression
    };
    WhereExpression::Cast {
        expression: Box::new(expression),
        pg_type: match order_type {
            PageOrderType::CreatedAtTimestamp => PgType::Bigint,
            PageOrderType::Checked(DataType::Date) => PgType::TimestampWithTimeZone,
            PageOrderType::Checked(DataType::Number) => PgType::Float8,
            PageOrderType::Checked(DataType::String) => PgType::Jsonb,
            PageOrderType::Checked(DataType::Boolean) => PgType::Boolean,
        },
    }
}

fn and(existing: Option<WherePredicate>, predicate: WherePredicate) -> WherePredicate {
    existing.map_or(predicate.clone(), |existing| {
        WherePredicate::And(vec![existing, predicate])
    })
}

fn page_probe(
    base: &SelectStatement,
    page: &PageInfo,
    with_next: &str,
    first: &str,
    last: &str,
    row: &PageRowExpressions,
    side: CursorSide,
) -> SelectStatement {
    let impossible = match side {
        CursorSide::After => page.limit.is_none() && page.before.is_none(),
        CursorSide::Before => {
            page.after.is_none() && page.offset.unwrap_or_default() == 0 && !page.last
        }
    };
    if impossible {
        return SelectStatement {
            select: vec![projection(WhereExpression::Inline("1".to_owned()), None)],
            predicate: Some(WherePredicate::Or(Vec::new())),
            ..SelectStatement::default()
        };
    }
    let overfetch = match side {
        CursorSide::After => page.limit.is_some() && page.before.is_none() && !page.last,
        CursorSide::Before => page.last && page.limit.is_some() && page.after.is_none(),
    };
    if overfetch {
        return SelectStatement {
            select: vec![projection(WhereExpression::Wildcard(None), None)],
            from: vec![table_source(with_next, None)],
            offset: page.limit,
            ..SelectStatement::default()
        };
    }
    let cursor_table = match (side, page.last) {
        (CursorSide::After, false) | (CursorSide::Before, true) => last,
        (CursorSide::After, true) | (CursorSide::Before, false) => first,
    };
    let mut probe = base.clone();
    probe.limit = Some(1);
    probe.offset = None;
    probe
        .from
        .insert(0, table_source(cursor_table, Some("cursor-row")));
    add_cursor(
        &mut probe,
        page,
        (
            WhereExpression::NamedColumn("cursor-row.e".to_owned()),
            WhereExpression::NamedColumn("cursor-row.sym".to_owned()),
        ),
        side,
        false,
        row,
    );
    probe
}

fn exists_cte(name: &str, query: SelectStatement) -> PlannedCte {
    select_cte(
        name.to_owned(),
        SelectStatement {
            select: vec![StatementProjection {
                expression: SelectExpression::Exists(Box::new(query)),
                alias: Some("exists".to_owned()),
            }],
            ..SelectStatement::default()
        },
        Materialization::NotMaterialized,
    )
}

fn indexed_table(prefix: &str, index: usize) -> String {
    format!("{}{index}", prefix.trim_start_matches(':'))
}

fn metadata_columns(table: &str) -> Vec<String> {
    ["entity-id", "attr-id", "value", "is-ref-val", "created-at"]
        .into_iter()
        .map(|suffix| format!("{table}-{suffix}").replace('-', "_"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::{Index, IndexKey, MatchCteOptions, RawPattern, RawTerm, plan_match_query};

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap()
    }

    fn page_plan(
        order_required: bool,
        after: Option<PageCursor>,
        cap_symbol_map_by_page_limit: Option<bool>,
    ) -> MatchQueryPlan {
        page_plan_with_scan_options(
            order_required,
            after,
            cap_symbol_map_by_page_limit,
            None,
            false,
        )
    }

    fn page_plan_with_scan_options(
        order_required: bool,
        after: Option<PageCursor>,
        cap_symbol_map_by_page_limit: Option<bool>,
        optional_number_order_scan_hints: Option<bool>,
        nested: bool,
    ) -> MatchQueryPlan {
        let app_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let attr_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let entity = symbol("?e");
        let base_pattern = RawPattern::new(
            Index::new(IndexKey::Ea),
            vec![
                RawTerm::Variable(entity.clone()),
                RawTerm::Constant(json!(attr_id)),
            ],
        )
        .normalize()
        .unwrap();
        let order = symbol("?order");
        let page_pattern = RawPattern::new(
            Index::checked(IndexKey::Ave, DataType::Number),
            vec![
                RawTerm::Variable(entity),
                RawTerm::Constant(json!(attr_id)),
                RawTerm::Variable(order.clone()),
            ],
        )
        .normalize()
        .unwrap();
        let additional = if nested {
            HashMap::from([(
                symbol("?e"),
                BindingPath::new(99, Component::Entity, IndexKey::Ea),
            )])
        } else {
            HashMap::new()
        };
        let options = MatchCteOptions {
            attribute_cardinalities: BTreeMap::from([(attr_id, 100)]),
            cap_symbol_map_by_page_limit,
            optional_number_order_scan_hints,
            ..MatchCteOptions::default()
        };
        let context = MatchQueryContext {
            prefix: "page-",
            app_id,
            additional_joins: &additional,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        };
        let mut plan = plan_match_query(
            &context,
            &[crate::NormalizedClause::Pattern {
                pattern: base_pattern,
            }],
        )
        .unwrap();
        let page = PageInfo {
            limit: Some(2),
            last: false,
            offset: None,
            direction: OrderDirection::Asc,
            order_symbol: order,
            order_type: PageOrderType::Checked(DataType::Number),
            order_required,
            attr_id,
            pattern: page_pattern,
            before: None,
            before_inclusive: false,
            after,
            after_inclusive: false,
        };
        add_page_info(&mut plan, &context, &page).unwrap();
        plan
    }

    #[test]
    fn optional_number_root_scan_relaxation_changes_only_the_order_scan_hint() {
        let legacy = page_plan(false, None, None);
        let relaxed = page_plan_with_scan_options(false, None, None, Some(false), false);
        assert_eq!(legacy.ctes, relaxed.ctes);
        assert_eq!(legacy.page, relaxed.page);
        assert_eq!(legacy.pattern_metadata, relaxed.pattern_metadata);
        assert_eq!(legacy.symbol_cardinalities, relaxed.symbol_cardinalities);
        let removed = legacy
            .hints
            .iter()
            .filter(|hint| !relaxed.hints.contains(hint))
            .collect::<Vec<_>>();
        assert!(!removed.is_empty());
        assert!(removed.iter().all(|hint| matches!(hint, crate::QueryHint::IndexScan { table_alias, .. } | crate::QueryHint::BitmapScan { table_alias, .. } if table_alias == "t1")));
        assert!(relaxed.hints.iter().all(|hint| legacy.hints.contains(hint)));
        assert_eq!(
            crate::render_match_query(&legacy).unwrap().parameters,
            crate::render_match_query(&relaxed).unwrap().parameters
        );
        assert_eq!(
            legacy.hints,
            page_plan_with_scan_options(false, None, None, Some(true), false).hints
        );
    }

    #[test]
    fn required_and_nested_order_scan_hints_keep_the_legacy_path() {
        for (required, nested) in [(true, false), (false, true), (true, true)] {
            let legacy = page_plan_with_scan_options(required, None, None, None, nested);
            let relaxed = page_plan_with_scan_options(required, None, None, Some(false), nested);
            assert_eq!(legacy, relaxed);
        }
        assert!(
            serde_json::to_value(MatchCteOptions::default())
                .unwrap()
                .get("optional_number_order_scan_hints")
                .is_none()
        );
    }

    #[test]
    fn plans_ordered_overfetch_and_boundary_probes() {
        let plan = page_plan(true, None, None);

        assert_eq!(plan.final_table, "page-1");
        assert_eq!(plan.next_index, 2);
        assert_eq!(plan.ctes.len(), 7);
        assert_eq!(
            plan.page.as_ref().unwrap().has_next_table,
            "page-1-has-next"
        );
        assert_eq!(
            plan.page.as_ref().unwrap().fallback_entity_column,
            "order_eid"
        );
        assert_eq!(
            plan.page.as_ref().unwrap().fallback_created_at_column,
            "page_0_created_at"
        );
        let rendered = crate::render_match_query(&plan).unwrap();
        assert!(
            rendered
                .sql
                .contains("DISTINCT ON (\"order_val\", \"order_eid\")")
        );
        assert!(rendered.sql.contains("LIMIT 3"));
        assert!(rendered.sql.contains("ROW_NUMBER() OVER ()"));
        assert!(
            rendered
                .sql
                .contains("EXISTS (SELECT * FROM \"page_1_with_next\" OFFSET 2)")
        );
        assert!(rendered.sql.contains("EXISTS (SELECT '1' WHERE FALSE)"));
        assert!(
            !rendered
                .parameters
                .iter()
                .any(|parameter| parameter == &crate::SqlParameter::Integer(1))
        );
    }

    #[test]
    fn page_limit_caps_propagated_symbol_cardinalities() {
        let plan = page_plan(true, None, None);

        assert_eq!(plan.symbol_cardinalities[&symbol("?e")], 2);
        assert_eq!(plan.symbol_cardinalities[&symbol("?order")], 2);
        assert!(plan.hints.contains(&crate::QueryHint::Rows {
            tables: vec!["page-0".to_owned(), "t1".to_owned()],
            estimate: 100,
        }));

        let uncapped = page_plan(true, None, Some(false));
        assert_eq!(uncapped.symbol_cardinalities[&symbol("?e")], 100);
        assert_eq!(uncapped.symbol_cardinalities[&symbol("?order")], 100);
    }

    #[test]
    fn renders_nullable_page_cursor_comparisons() {
        let entity_id = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();
        let plan = page_plan(
            false,
            Some(PageCursor {
                entity_id,
                order_value: Value::Null,
            }),
            None,
        );
        let rendered = crate::render_match_query(&plan).unwrap();

        assert_eq!(plan.final_table, "page-2");
        assert_eq!(plan.next_index, 3);
        assert_eq!(plan.ctes.len(), 9);
        assert_eq!(
            plan.page.as_ref().unwrap().cursor_columns,
            metadata_columns("page-1")
        );
        assert!(rendered.sql.contains("LEFT JOIN \"page_1\""));
        assert!(rendered.sql.contains("\"page_2_optional_source\""));
        assert!(rendered.sql.contains("NULLS FIRST"));
        assert!(rendered.sql.contains("(NULL)::float8"));
        assert!(rendered.sql.contains("IS NULL"));
        assert!(rendered.sql.contains("IS NOT NULL"));
        assert!(rendered.parameters.iter().any(
            |parameter| matches!(parameter, crate::SqlParameter::Uuid(id) if *id == entity_id)
        ));
    }

    #[test]
    fn checked_order_types_match_clojure_extraction_and_cast_contract() {
        let cases = [
            (
                DataType::Date,
                Some(SqlFunction::ExtractDateValue),
                PgType::TimestampWithTimeZone,
            ),
            (
                DataType::Number,
                Some(SqlFunction::ExtractNumberValue),
                PgType::Float8,
            ),
            (DataType::String, None, PgType::Jsonb),
            (
                DataType::Boolean,
                Some(SqlFunction::ExtractBooleanValue),
                PgType::Boolean,
            ),
        ];

        for (data_type, expected_function, expected_type) in cases {
            let order_type = PageOrderType::Checked(data_type);
            assert_eq!(extraction_function(order_type), expected_function);
            assert!(matches!(
                typed_cursor_value(WhereExpression::Literal(Value::Null), order_type),
                WhereExpression::Cast { pg_type, .. } if pg_type == expected_type
            ));
        }
    }

    #[test]
    fn numeric_boundary_columns_normalize_json_null_without_changing_public_cursor_casts() {
        let column = WhereExpression::NamedColumn("cursor-row.sym".into());
        let number = PageOrderType::Checked(DataType::Number);
        assert_eq!(
            typed_cursor_value(column.clone(), number),
            WhereExpression::Cast {
                expression: Box::new(WhereExpression::function(
                    SqlFunction::JsonNullToNull,
                    column.clone(),
                )),
                pg_type: PgType::Float8,
            },
            "an internal JSON-null numeric boundary must become SQL NULL before casting"
        );
        for value in [Value::Null, json!(7.5)] {
            let literal = WhereExpression::Literal(value);
            assert_eq!(
                typed_cursor_value(literal.clone(), number),
                WhereExpression::Cast {
                    expression: Box::new(literal),
                    pg_type: PgType::Float8,
                },
                "public cursor literals keep their original typed parameter path"
            );
        }
        for (order, pg_type) in [
            (PageOrderType::CreatedAtTimestamp, PgType::Bigint),
            (PageOrderType::Checked(DataType::String), PgType::Jsonb),
            (PageOrderType::Checked(DataType::Boolean), PgType::Boolean),
            (
                PageOrderType::Checked(DataType::Date),
                PgType::TimestampWithTimeZone,
            ),
        ] {
            assert_eq!(
                typed_cursor_value(column.clone(), order),
                WhereExpression::Cast {
                    expression: Box::new(column.clone()),
                    pg_type,
                },
                "do not broaden the numeric boundary repair to other types"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Eight exact Clojure-derived cursor forms stay readable together.
    fn cursor_direction_and_inclusivity_match_clojure_forms() {
        let entity_id = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();
        let order = json!([":triples_extract_number_value", ":value"]);
        let cursor_value = json!([":cast", 20, ":float8"]);
        let cursor_entity = json!([":cast", entity_id.to_string(), ":uuid"]);
        let cases = [
            (
                CursorSide::Before,
                OrderDirection::Asc,
                false,
                "<",
                "<",
                "<=",
            ),
            (
                CursorSide::Before,
                OrderDirection::Asc,
                true,
                "<",
                "<=",
                "<=",
            ),
            (
                CursorSide::After,
                OrderDirection::Asc,
                false,
                ">",
                ">",
                ">=",
            ),
            (
                CursorSide::After,
                OrderDirection::Asc,
                true,
                ">",
                ">=",
                ">=",
            ),
            (
                CursorSide::Before,
                OrderDirection::Desc,
                false,
                ">",
                ">",
                ">=",
            ),
            (
                CursorSide::Before,
                OrderDirection::Desc,
                true,
                ">",
                ">=",
                ">=",
            ),
            (
                CursorSide::After,
                OrderDirection::Desc,
                false,
                "<",
                "<",
                "<=",
            ),
            (
                CursorSide::After,
                OrderDirection::Desc,
                true,
                "<",
                "<=",
                "<=",
            ),
        ];

        for (side, direction, inclusive, strict, entity_op, inclusive_op) in cases {
            let mut page = page_info(direction);
            page.order_required = true;
            let actual = cursor_predicate(
                &page,
                (
                    WhereExpression::Uuid(entity_id),
                    WhereExpression::Literal(json!(20)),
                ),
                side,
                inclusive,
                &PageRowExpressions {
                    entity: WhereExpression::column(WhereColumn::EntityId),
                    order: WhereExpression::column(WhereColumn::Value),
                },
            )
            .to_honey_sql_json();
            assert_eq!(
                actual,
                json!([
                    ":and",
                    [format!(":{inclusive_op}"), order, cursor_value],
                    [
                        ":or",
                        [format!(":{strict}"), order, cursor_value],
                        [
                            ":and",
                            [":=", order, cursor_value],
                            [format!(":{entity_op}"), ":entity-id", cursor_entity]
                        ]
                    ]
                ])
            );
        }
    }

    fn page_info(direction: OrderDirection) -> PageInfo {
        let attr_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let order_symbol = symbol("?order");
        PageInfo {
            limit: Some(2),
            last: false,
            offset: None,
            direction,
            order_symbol: order_symbol.clone(),
            order_type: PageOrderType::Checked(DataType::Number),
            order_required: true,
            attr_id,
            pattern: RawPattern::new(
                Index::checked(IndexKey::Ave, DataType::Number),
                vec![
                    RawTerm::Variable(symbol("?e")),
                    RawTerm::Constant(json!(attr_id)),
                    RawTerm::Variable(order_symbol),
                ],
            )
            .normalize()
            .unwrap(),
            before: None,
            before_inclusive: false,
            after: None,
            after_inclusive: false,
        }
    }
}
