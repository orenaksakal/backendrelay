//! Parameterized SQL planning and execution for Bring Your Own Postgres `InstaQL`.

use std::collections::{BTreeMap, HashMap};

use instant_datalog::OrderDirection;
use instant_instaql::{
    Aggregate, Cursor, Form, InstaqlError, QueryConstraint, WhereComparator, WhereCondition,
    WhereValue, coerce_forms,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio_postgres::types::Json;

use crate::{
    ByopField, ByopRelation, ByopSchema, ByopTable, ByopTopic, ByopTopicError,
    CancelableGenericClient, PgQueryError, topics_for_byop_query,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RenderedByopQuery {
    pub sql: String,
    pub parameters: Vec<Value>,
}

#[derive(Debug, Error)]
pub enum ByopQueryError {
    #[error("unknown BYOP table `{0}`")]
    UnknownTable(String),
    #[error("unknown BYOP field `{field}` on table `{table}`")]
    UnknownField { table: String, field: String },
    #[error("unknown BYOP relation `{relation}` on table `{table}`")]
    UnknownRelation { table: String, relation: String },
    #[error("BYOP ordering requires an indexed field: `{table}.{field}`")]
    UnindexedOrder { table: String, field: String },
    #[error("cursor attribute {0} is not a field of BYOP table `{1}`")]
    UnknownCursorAttribute(uuid::Uuid, String),
    #[error("cursor orders by `{cursor_field}`, but the query orders by `{order_field}`")]
    CursorOrderMismatch {
        cursor_field: String,
        order_field: String,
    },
    #[error("four-item cursors cannot be used for Bring Your Own Postgres queries")]
    NormalCursor,
    #[error("aggregates are currently only available for admin queries")]
    AggregateRequiresAdmin,
    #[error("aggregates cannot contain child queries")]
    AggregateChildren,
    #[error("BYOP query returned {0} rows instead of one")]
    UnexpectedRowCount(usize),
    #[error("invalid BYOP result envelope at {0}")]
    InvalidEnvelope(String),
    #[error(transparent)]
    Instaql(#[from] InstaqlError),
    #[error(transparent)]
    Topic(#[from] ByopTopicError),
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
    #[error(transparent)]
    Query(#[from] PgQueryError),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ByopResponse {
    pub data: BTreeMap<String, Value>,
    pub page_info: BTreeMap<String, ByopPageInfo>,
    pub aggregates: BTreeMap<String, BTreeMap<String, i64>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ByopPageInfo {
    pub start_cursor: Option<Vec<Value>>,
    pub end_cursor: Option<Vec<Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ByopExecution {
    pub response: ByopResponse,
    pub topics: Vec<ByopTopic>,
    pub raw: Value,
}

struct RenderContext<'a> {
    namespace: &'a str,
    schema: &'a ByopSchema,
    parameters: Vec<Value>,
    next_alias: usize,
    admin: bool,
    constraints: &'a HashMap<String, QueryConstraint>,
}

impl RenderContext<'_> {
    fn bind(&mut self, value: Value) -> String {
        self.parameters.push(value);
        format!("${}::jsonb", self.parameters.len())
    }

    fn bind_text(&mut self, value: impl Into<String>) -> String {
        let parameter = self.bind(Value::String(value.into()));
        format!("({parameter} #>> '{{}}')")
    }

    fn alias(&mut self) -> String {
        let alias = format!("byop_{}", self.next_alias);
        self.next_alias += 1;
        alias
    }
}

/// Renders typed `InstaQL` forms as one parameterized PostgreSQL statement against
/// an introspected namespace. User-controlled table, relation, field, and type names
/// must exist in [`ByopSchema`] and are quoted; all values are driver parameters.
///
/// # Errors
///
/// Returns a validation error for unknown catalog objects, invalid ordering/cursors,
/// or unauthorized aggregates.
pub fn render_byop_query(
    namespace: &str,
    schema: &ByopSchema,
    forms: &[Form],
    admin: bool,
) -> Result<RenderedByopQuery, ByopQueryError> {
    render_byop_query_with_constraints(namespace, schema, forms, admin, &HashMap::new())
}

/// Renders BYOP forms with permission-derived restrictions applied at every
/// matching root and nested entity type.
///
/// # Errors
///
/// Returns the same catalog, cursor, and aggregate errors as [`render_byop_query`].
#[allow(clippy::implicit_hasher)]
pub fn render_byop_query_with_constraints(
    namespace: &str,
    schema: &ByopSchema,
    forms: &[Form],
    admin: bool,
    constraints: &HashMap<String, QueryConstraint>,
) -> Result<RenderedByopQuery, ByopQueryError> {
    let mut context = RenderContext {
        namespace,
        schema,
        parameters: Vec::new(),
        next_alias: 0,
        admin,
        constraints,
    };
    let roots = forms
        .iter()
        .map(|form| render_form(&mut context, form, None))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RenderedByopQuery {
        sql: format!("SELECT jsonb_build_array({}) AS result", roots.join(", ")),
        parameters: context.parameters,
    })
}

/// Executes a rendered BYOP query and returns the reference-shaped JSON envelope.
///
/// # Errors
///
/// Returns PostgreSQL, row-count, or result-decoding errors.
pub async fn query_byop<C>(
    client: &C,
    rendered: &RenderedByopQuery,
) -> Result<Value, ByopQueryError>
where
    C: CancelableGenericClient,
{
    let query = instant_datalog::RenderedQuery {
        sql: rendered.sql.clone(),
        parameters: rendered
            .parameters
            .iter()
            .cloned()
            .map(instant_datalog::SqlParameter::Json)
            .collect(),
    };
    let rows = crate::query(client, &query).await?;
    if rows.len() != 1 {
        return Err(ByopQueryError::UnexpectedRowCount(rows.len()));
    }
    Ok(rows[0].try_get::<_, Json<Value>>("result")?.0)
}

/// Renders one CEL `data.ref(path)` lookup against an introspected BYOP schema.
/// The root entity value remains a bound JSON parameter and every identifier is
/// sourced from the catalog.
///
/// # Errors
///
/// Returns a catalog error when the root table or any path segment is unknown.
pub fn render_byop_reference(
    namespace: &str,
    schema: &ByopSchema,
    etype: &str,
    entity_id: Value,
    path: &str,
) -> Result<RenderedByopQuery, ByopQueryError> {
    let root = schema
        .tables
        .get(etype)
        .ok_or_else(|| ByopQueryError::UnknownTable(etype.to_owned()))?;
    let segments = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Ok(RenderedByopQuery {
            sql: "SELECT NULL::jsonb AS value WHERE false".to_owned(),
            parameters: Vec::new(),
        });
    }

    let mut current_name = etype;
    let mut current = root;
    let mut current_alias = "byop_ref_0".to_owned();
    let mut joins = Vec::new();
    let mut selected = None;
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        if let Some(relation) = current.relations.get(*segment) {
            let next = schema
                .tables
                .get(&relation.other_table)
                .ok_or_else(|| ByopQueryError::UnknownTable(relation.other_table.clone()))?;
            let next_alias = format!("byop_ref_{}", index + 1);
            joins.push(format!(
                "JOIN {}.{} AS {} ON {}.{} = {}.{}",
                quote_identifier(namespace),
                quote_identifier(&relation.other_table),
                quote_identifier(&next_alias),
                quote_identifier(&next_alias),
                quote_identifier(&relation.other_table_field),
                quote_identifier(&current_alias),
                quote_identifier(&relation.table_field),
            ));
            current_name = &relation.other_table;
            current = next;
            current_alias = next_alias;
            if last {
                selected = Some(format!(
                    "{}.{}",
                    quote_identifier(&current_alias),
                    quote_identifier(&current.primary_key.field)
                ));
            }
        } else if current.fields.contains_key(*segment) {
            if !last {
                return Err(ByopQueryError::UnknownRelation {
                    table: current_name.to_owned(),
                    relation: (*segment).to_owned(),
                });
            }
            selected = Some(format!(
                "{}.{}",
                quote_identifier(&current_alias),
                quote_identifier(segment)
            ));
        } else {
            return Err(ByopQueryError::UnknownField {
                table: current_name.to_owned(),
                field: (*segment).to_owned(),
            });
        }
    }
    let Some(selected) = selected else {
        return Err(ByopQueryError::UnknownField {
            table: current_name.to_owned(),
            field: path.to_owned(),
        });
    };
    let parameter = format!(
        "(jsonb_populate_record(NULL::{}.{}, jsonb_build_object(($2::jsonb #>> '{{}}'), $1::jsonb))).{}",
        quote_identifier(namespace),
        quote_identifier(etype),
        quote_identifier(&root.primary_key.field),
    );
    Ok(RenderedByopQuery {
        sql: format!(
            "SELECT to_jsonb({selected}) AS value FROM {}.{} AS {} {} WHERE {}.{} = {parameter} ORDER BY {selected}",
            quote_identifier(namespace),
            quote_identifier(etype),
            quote_identifier("byop_ref_0"),
            joins.join(" "),
            quote_identifier("byop_ref_0"),
            quote_identifier(&root.primary_key.field),
        ),
        parameters: vec![entity_id, Value::String(root.primary_key.field.clone())],
    })
}

/// Executes one BYOP permission-reference lookup.
///
/// # Errors
///
/// Returns catalog, binding, PostgreSQL, or row-decoding errors.
pub async fn query_byop_reference<C>(
    client: &C,
    namespace: &str,
    schema: &ByopSchema,
    etype: &str,
    entity_id: Value,
    path: &str,
) -> Result<Vec<Value>, ByopQueryError>
where
    C: CancelableGenericClient,
{
    let rendered = render_byop_reference(namespace, schema, etype, entity_id, path)?;
    let query = instant_datalog::RenderedQuery {
        sql: rendered.sql,
        parameters: rendered
            .parameters
            .into_iter()
            .map(instant_datalog::SqlParameter::Json)
            .collect(),
    };
    crate::query(client, &query)
        .await?
        .into_iter()
        .map(|row| {
            row.try_get::<_, Json<Value>>("value")
                .map(|value| value.0)
                .map_err(ByopQueryError::from)
        })
        .collect()
}

/// Coerces, validates, renders, executes, shapes, and records dependencies for one
/// complete BYOP `InstaQL` request.
///
/// # Errors
///
/// Returns coercion, catalog, SQL, execution, shaping, or topic errors.
pub async fn execute_byop_instaql<C>(
    client: &C,
    namespace: &str,
    schema: &ByopSchema,
    query: &Value,
    admin: bool,
    inference: bool,
) -> Result<ByopExecution, ByopQueryError>
where
    C: CancelableGenericClient,
{
    execute_byop_instaql_with_constraints(
        client,
        namespace,
        schema,
        query,
        admin,
        inference,
        &HashMap::new(),
    )
    .await
}

/// Executes a BYOP query with symbolic permission constraints pushed into SQL.
/// A defensive shaped-entity permission pass remains the caller's responsibility.
///
/// # Errors
///
/// Returns coercion, catalog, SQL, execution, shaping, or topic errors.
#[allow(clippy::implicit_hasher)]
pub async fn execute_byop_instaql_with_constraints<C>(
    client: &C,
    namespace: &str,
    schema: &ByopSchema,
    query: &Value,
    admin: bool,
    inference: bool,
    constraints: &HashMap<String, QueryConstraint>,
) -> Result<ByopExecution, ByopQueryError>
where
    C: CancelableGenericClient,
{
    let forms = coerce_forms(query, &schema.catalog)?;
    let rendered =
        render_byop_query_with_constraints(namespace, schema, &forms, admin, constraints)?;
    let raw = query_byop(client, &rendered).await?;
    let response = shape_byop_response(schema, &raw, inference)?;
    let topics = topics_for_byop_query(schema, &forms, &raw)?;
    Ok(ByopExecution {
        response,
        topics,
        raw,
    })
}

/// Shapes the reference SQL envelope into public entity objects, root page cursors,
/// and aggregate metadata. BYOP primary keys remain arbitrary JSON values.
///
/// # Errors
///
/// Returns an envelope error when PostgreSQL returned a shape inconsistent with the
/// rendered query or introspected schema.
pub fn shape_byop_response(
    schema: &ByopSchema,
    result: &Value,
    inference: bool,
) -> Result<ByopResponse, ByopQueryError> {
    let roots = result
        .as_array()
        .ok_or_else(|| invalid_envelope("root array"))?;
    let mut response = ByopResponse::default();
    for root in roots {
        let object = root
            .as_object()
            .ok_or_else(|| invalid_envelope("root object"))?;
        let key = string_member(object, "k", "root key")?;
        let etype = string_member(object, "etype", "root entity type")?;
        let data = object
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_envelope("root data"))?;
        let rows = shape_byop_rows(schema, etype, data, inference)?;
        response.data.insert(key.to_owned(), Value::Array(rows));
        if let Some(aggregate) = data.get("aggregate").and_then(Value::as_object) {
            let count = aggregate
                .get("count")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid_envelope("count aggregate"))?;
            response.aggregates.insert(
                key.to_owned(),
                BTreeMap::from([("count".to_owned(), count)]),
            );
        }
        if let Some(page) = shape_page_info(schema, etype, data)? {
            response.page_info.insert(key.to_owned(), page);
        }
    }
    Ok(response)
}

fn shape_byop_rows(
    schema: &ByopSchema,
    etype: &str,
    data: &Map<String, Value>,
    inference: bool,
) -> Result<Vec<Value>, ByopQueryError> {
    let table = schema
        .tables
        .get(etype)
        .ok_or_else(|| ByopQueryError::UnknownTable(etype.to_owned()))?;
    let rows = match data.get("rows") {
        None => return Ok(Vec::new()),
        Some(Value::Array(rows)) => rows,
        Some(_) => return Err(invalid_envelope("data rows")),
    };
    rows.iter()
        .map(|entry| {
            let entry = entry
                .as_object()
                .ok_or_else(|| invalid_envelope("row entry"))?;
            let mut row = entry
                .get("row")
                .and_then(Value::as_object)
                .cloned()
                .ok_or_else(|| invalid_envelope("row object"))?;
            if let Some(children) = entry.get("children") {
                let children = children
                    .as_array()
                    .ok_or_else(|| invalid_envelope("children array"))?;
                for child in children {
                    let child = child
                        .as_object()
                        .ok_or_else(|| invalid_envelope("child object"))?;
                    let child_key = string_member(child, "k", "child key")?;
                    let child_etype = string_member(child, "etype", "child entity type")?;
                    let child_data = child
                        .get("data")
                        .and_then(Value::as_object)
                        .ok_or_else(|| invalid_envelope("child data"))?;
                    let values = shape_byop_rows(schema, child_etype, child_data, inference)?;
                    let relation = table.relations.get(child_key).ok_or_else(|| {
                        ByopQueryError::UnknownRelation {
                            table: etype.to_owned(),
                            relation: child_key.to_owned(),
                        }
                    })?;
                    let value = if inference && relation_is_singular(schema, relation) {
                        values.into_iter().next().unwrap_or(Value::Null)
                    } else {
                        Value::Array(values)
                    };
                    row.insert(child_key.to_owned(), value);
                }
            }
            Ok(Value::Object(row))
        })
        .collect()
}

fn relation_is_singular(schema: &ByopSchema, relation: &ByopRelation) -> bool {
    if relation.direction == crate::ByopRelationDirection::Forward {
        return true;
    }
    // A reverse edge is singular only when the referencing field is unique.
    // Introspection records that constraint on the source table's field.
    schema
        .tables
        .get(&relation.other_table)
        .is_some_and(|table| {
            table
                .fields
                .get(&relation.other_table_field)
                .is_some_and(|field| field.unique)
        })
}

fn shape_page_info(
    schema: &ByopSchema,
    etype: &str,
    data: &Map<String, Value>,
) -> Result<Option<ByopPageInfo>, ByopQueryError> {
    let Some(page) = data.get("page-info").and_then(Value::as_object) else {
        return Ok(None);
    };
    let sort_field = string_member(page, "sort-field", "page sort field")?;
    let reversed = page
        .get("reversed")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid_envelope("page reversed flag"))?;
    let table = schema
        .tables
        .get(etype)
        .ok_or_else(|| ByopQueryError::UnknownTable(etype.to_owned()))?;
    let field = table
        .fields
        .get(sort_field)
        .ok_or_else(|| ByopQueryError::UnknownField {
            table: etype.to_owned(),
            field: sort_field.to_owned(),
        })?;
    let rows = data
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_envelope("page rows"))?;
    let first = rows
        .first()
        .map(|row| row_cursor(row, table, sort_field, field));
    let last = rows
        .last()
        .map(|row| row_cursor(row, table, sort_field, field));
    let (start_cursor, end_cursor) = if reversed {
        (last, first)
    } else {
        (first, last)
    };
    Ok(Some(ByopPageInfo {
        start_cursor: start_cursor.transpose()?,
        end_cursor: end_cursor.transpose()?,
    }))
}

fn row_cursor(
    entry: &Value,
    table: &ByopTable,
    sort_field: &str,
    field: &ByopField,
) -> Result<Vec<Value>, ByopQueryError> {
    let row = entry
        .get("row")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_envelope("page row object"))?;
    let entity = row
        .get(&table.primary_key.field)
        .cloned()
        .ok_or_else(|| invalid_envelope("page primary key"))?;
    let value = row
        .get(sort_field)
        .cloned()
        .ok_or_else(|| invalid_envelope("page sort value"))?;
    Ok(vec![
        entity,
        Value::String(field.attribute_id.to_string()),
        value,
    ])
}

fn string_member<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, ByopQueryError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_envelope(context))
}

fn invalid_envelope(context: &str) -> ByopQueryError {
    ByopQueryError::InvalidEnvelope(context.to_owned())
}

fn render_form(
    context: &mut RenderContext<'_>,
    form: &Form,
    correlation: Option<(&str, &ByopRelation)>,
) -> Result<String, ByopQueryError> {
    let table = context
        .schema
        .tables
        .get(&form.etype)
        .ok_or_else(|| ByopQueryError::UnknownTable(form.etype.clone()))?;
    if form.options.aggregate.is_some() && !context.admin {
        return Err(ByopQueryError::AggregateRequiresAdmin);
    }
    if form.options.aggregate.is_some() && !form.children.is_empty() {
        return Err(ByopQueryError::AggregateChildren);
    }
    let alias = context.alias();
    let data = if form.options.aggregate == Some(Aggregate::Count) {
        render_aggregate(context, form, table, &alias, correlation)?
    } else {
        render_rows(context, form, table, &alias, correlation)?
    };
    let key = context.bind_text(form.key.clone());
    let etype = context.bind_text(form.etype.clone());
    Ok(format!(
        "jsonb_build_object('k', {key}, 'etype', {etype}, 'data', {data})"
    ))
}

fn render_aggregate(
    context: &mut RenderContext<'_>,
    form: &Form,
    table: &ByopTable,
    alias: &str,
    correlation: Option<(&str, &ByopRelation)>,
) -> Result<String, ByopQueryError> {
    let predicate = render_predicate(context, form, table, alias, correlation)?;
    Ok(format!(
        "(SELECT jsonb_build_object('aggregate', jsonb_build_object('count', count(*))) \
           FROM {}.{} AS {}{})",
        quote_identifier(context.namespace),
        quote_identifier(&form.etype),
        quote_identifier(alias),
        where_sql(predicate.as_deref())
    ))
}

fn render_rows(
    context: &mut RenderContext<'_>,
    form: &Form,
    table: &ByopTable,
    alias: &str,
    correlation: Option<(&str, &ByopRelation)>,
) -> Result<String, ByopQueryError> {
    let predicate = render_predicate(context, form, table, alias, correlation)?;
    let order = order_plan(form, table)?;
    let cursor = render_cursor(context, form, table, alias, &order)?;
    let predicate = combine_predicates(predicate, cursor);
    let children =
        form.children
            .iter()
            .map(|child| {
                let relation = table.relations.get(&child.key).ok_or_else(|| {
                    ByopQueryError::UnknownRelation {
                        table: form.etype.clone(),
                        relation: child.key.clone(),
                    }
                })?;
                render_form(context, child, Some((alias, relation)))
            })
            .collect::<Result<Vec<_>, _>>()?;
    let row_object = render_row_object(table, alias, &children);
    let source = format!(
        "SELECT {}.* FROM {}.{} AS {}{}{}{}{}",
        quote_identifier(alias),
        quote_identifier(context.namespace),
        quote_identifier(&form.etype),
        quote_identifier(alias),
        where_sql(predicate.as_deref()),
        order_sql(alias, &order, &table.primary_key.field),
        limit_sql(form),
        offset_sql(form)
    );
    let page_info = order.field.as_ref().map_or_else(
        || "NULL".to_owned(),
        |field| {
            format!(
                "jsonb_build_object('sort-field', {}, 'reversed', {})",
                sql_string(field),
                order.reversed
            )
        },
    );
    Ok(format!(
        "(SELECT jsonb_build_object(\
             'rows', COALESCE(jsonb_agg({row_object}), '[]'::jsonb), \
             'page-info', {page_info}) \
           FROM ({source}) AS {})",
        quote_identifier(alias)
    ))
}

fn render_row_object(table: &ByopTable, alias: &str, children: &[String]) -> String {
    let mut entries = vec![format!("'row', to_jsonb({})", quote_identifier(alias))];
    if let Some(field) = &table.created_at_field {
        entries.push(format!(
            "'t', (extract(epoch FROM {}.{}) * 1000)::bigint",
            quote_identifier(alias),
            quote_identifier(field)
        ));
    }
    if !children.is_empty() {
        entries.push(format!(
            "'children', jsonb_build_array({})",
            children.join(", ")
        ));
    }
    format!("jsonb_build_object({})", entries.join(", "))
}

#[derive(Default)]
struct OrderPlan {
    field: Option<String>,
    direction: Option<OrderDirection>,
    reversed: bool,
}

fn order_plan(form: &Form, table: &ByopTable) -> Result<OrderPlan, ByopQueryError> {
    let page_requested = form.options.limit.is_some()
        || form.options.first.is_some()
        || form.options.last.is_some()
        || form.options.offset.is_some()
        || form.options.before.is_some()
        || form.options.after.is_some();
    let (field, mut direction) = if let Some(order) = &form.options.order {
        let field = table
            .fields
            .get(&order.field)
            .ok_or_else(|| ByopQueryError::UnknownField {
                table: form.etype.clone(),
                field: order.field.clone(),
            })?;
        if !field.indexed {
            return Err(ByopQueryError::UnindexedOrder {
                table: form.etype.clone(),
                field: order.field.clone(),
            });
        }
        (Some(order.field.clone()), Some(order.direction))
    } else if page_requested {
        (
            Some(table.primary_key.field.clone()),
            Some(OrderDirection::Asc),
        )
    } else {
        (None, None)
    };
    let reversed = form.options.last.is_some();
    if reversed {
        direction = direction.map(reverse_direction);
    }
    Ok(OrderPlan {
        field,
        direction,
        reversed,
    })
}

fn render_cursor(
    context: &mut RenderContext<'_>,
    form: &Form,
    table: &ByopTable,
    alias: &str,
    order: &OrderPlan,
) -> Result<Option<String>, ByopQueryError> {
    let Some((cursor, before)) = form
        .options
        .before
        .as_ref()
        .map(|cursor| (cursor, true))
        .or_else(|| form.options.after.as_ref().map(|cursor| (cursor, false)))
    else {
        return Ok(None);
    };
    let Cursor::Byop {
        attr_id,
        value,
        entity_id,
    } = cursor
    else {
        return Err(ByopQueryError::NormalCursor);
    };
    let cursor_field =
        table
            .attribute_id_to_field
            .get(attr_id)
            .ok_or(ByopQueryError::UnknownCursorAttribute(
                *attr_id,
                form.etype.clone(),
            ))?;
    let order_field = order
        .field
        .as_ref()
        .expect("a cursor always requests ordering");
    if cursor_field != order_field {
        return Err(ByopQueryError::CursorOrderMismatch {
            cursor_field: cursor_field.clone(),
            order_field: order_field.clone(),
        });
    }
    let direction = order.direction.expect("a cursor always has direction");
    let operator = match (before, direction) {
        (true, OrderDirection::Asc) | (false, OrderDirection::Desc) => "<",
        (true, OrderDirection::Desc) | (false, OrderDirection::Asc) => ">",
    };
    // PostgreSQL's ordinary ORDER BY places NULLs last for ASC and first for
    // DESC. A SQL comparison with a NULL cursor is UNKNOWN, so express the
    // boundary crossing explicitly instead of returning an empty page.
    if value.is_null() {
        let crosses_null_partition = matches!(
            (before, direction),
            (true, OrderDirection::Asc) | (false, OrderDirection::Desc)
        );
        return Ok(Some(if crosses_null_partition {
            let primary_key = typed_parameter(
                context,
                entity_id.clone(),
                &form.etype,
                &table.primary_key.field,
            );
            format!(
                "({}.{} IS NOT NULL OR ({}.{} IS NULL AND {}.{} {operator} {}))",
                quote_identifier(alias),
                quote_identifier(order_field),
                quote_identifier(alias),
                quote_identifier(order_field),
                quote_identifier(alias),
                quote_identifier(&table.primary_key.field),
                primary_key,
            )
        } else {
            "FALSE".to_owned()
        }));
    }
    let parameter = typed_parameter(context, value.clone(), &form.etype, order_field);
    let entity = typed_parameter(
        context,
        entity_id.clone(),
        &form.etype,
        &table.primary_key.field,
    );
    let null_partition = (operator == ">").then(|| {
        format!(
            "{}.{} IS NULL OR ",
            quote_identifier(alias),
            quote_identifier(order_field),
        )
    });
    Ok(Some(format!(
        "({}{}.{} {operator} {parameter} OR ({}.{} = {parameter} AND {}.{} {operator} {entity}))",
        null_partition.unwrap_or_default(),
        quote_identifier(alias),
        quote_identifier(order_field),
        quote_identifier(alias),
        quote_identifier(order_field),
        quote_identifier(alias),
        quote_identifier(&table.primary_key.field),
    )))
}

fn render_predicate(
    context: &mut RenderContext<'_>,
    form: &Form,
    table: &ByopTable,
    alias: &str,
    correlation: Option<(&str, &ByopRelation)>,
) -> Result<Option<String>, ByopQueryError> {
    let mut predicates = form
        .options
        .where_clause
        .iter()
        .map(|condition| render_condition(context, &form.etype, table, alias, condition))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(constraint) = context.constraints.get(&form.etype) {
        match constraint {
            QueryConstraint::DenyAll => predicates.push("FALSE".to_owned()),
            QueryConstraint::Where(conditions) => predicates.extend(
                conditions
                    .iter()
                    .map(|condition| {
                        render_condition(context, &form.etype, table, alias, condition)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        }
    }
    if let Some((parent_alias, relation)) = correlation {
        predicates.push(format!(
            "{}.{} = {}.{}",
            quote_identifier(alias),
            quote_identifier(&relation.other_table_field),
            quote_identifier(parent_alias),
            quote_identifier(&relation.table_field)
        ));
    }
    Ok((!predicates.is_empty()).then(|| predicates.join(" AND ")))
}

fn render_condition(
    context: &mut RenderContext<'_>,
    table_name: &str,
    table: &ByopTable,
    alias: &str,
    condition: &WhereCondition,
) -> Result<String, ByopQueryError> {
    match condition {
        WhereCondition::And { conditions } | WhereCondition::Or { conditions } => {
            let separator = if matches!(condition, WhereCondition::And { .. }) {
                " AND "
            } else {
                " OR "
            };
            let rendered = conditions
                .iter()
                .map(|condition| render_condition(context, table_name, table, alias, condition))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", rendered.join(separator)))
        }
        WhereCondition::Condition { path, value } => {
            render_path_condition(context, table_name, table, alias, path, value)
        }
    }
}

fn render_path_condition(
    context: &mut RenderContext<'_>,
    table_name: &str,
    table: &ByopTable,
    alias: &str,
    path: &[String],
    value: &WhereValue,
) -> Result<String, ByopQueryError> {
    let Some((head, tail)) = path.split_first() else {
        return Err(ByopQueryError::UnknownField {
            table: table_name.to_owned(),
            field: String::new(),
        });
    };
    if tail.is_empty() {
        if matches!(value, WhereValue::EntityIdStartsWith(_)) {
            return Ok(render_field_value(
                context,
                table_name,
                alias,
                &table.primary_key.field,
                value,
            ));
        }
        if !table.fields.contains_key(head) {
            return Err(ByopQueryError::UnknownField {
                table: table_name.to_owned(),
                field: head.clone(),
            });
        }
        return Ok(render_field_value(context, table_name, alias, head, value));
    }
    let relation = table
        .relations
        .get(head)
        .ok_or_else(|| ByopQueryError::UnknownRelation {
            table: table_name.to_owned(),
            relation: head.clone(),
        })?;
    let other = context
        .schema
        .tables
        .get(&relation.other_table)
        .ok_or_else(|| ByopQueryError::UnknownTable(relation.other_table.clone()))?;
    let other_alias = context.alias();
    let nested = render_path_condition(
        context,
        &relation.other_table,
        other,
        &other_alias,
        tail,
        value,
    )?;
    Ok(format!(
        "EXISTS (SELECT 1 FROM {}.{} AS {} WHERE {}.{} = {}.{} AND {nested})",
        quote_identifier(context.namespace),
        quote_identifier(&relation.other_table),
        quote_identifier(&other_alias),
        quote_identifier(&other_alias),
        quote_identifier(&relation.other_table_field),
        quote_identifier(alias),
        quote_identifier(&relation.table_field)
    ))
}

fn render_field_value(
    context: &mut RenderContext<'_>,
    table_name: &str,
    alias: &str,
    field_name: &str,
    value: &WhereValue,
) -> String {
    let column = format!(
        "{}.{}",
        quote_identifier(alias),
        quote_identifier(field_name)
    );
    match value {
        WhereValue::Scalar(Value::Null) | WhereValue::IsNull(true) => {
            format!("{column} IS NULL")
        }
        WhereValue::Scalar(value) => {
            format!(
                "{column} = {}",
                typed_parameter(context, value.clone(), table_name, field_name)
            )
        }
        WhereValue::In(values) if values.is_empty() => "FALSE".to_owned(),
        WhereValue::In(values) => {
            let comparisons = values
                .iter()
                .map(|value| {
                    if value.is_null() {
                        format!("{column} IS NULL")
                    } else {
                        format!(
                            "{column} = {}",
                            typed_parameter(context, value.clone(), table_name, field_name)
                        )
                    }
                })
                .collect::<Vec<_>>();
            format!("({})", comparisons.join(" OR "))
        }
        WhereValue::Not(Value::Null) | WhereValue::IsNull(false) => {
            format!("{column} IS NOT NULL")
        }
        WhereValue::Not(value) => format!(
            "{column} IS DISTINCT FROM {}",
            typed_parameter(context, value.clone(), table_name, field_name)
        ),
        WhereValue::Comparator { operation, value } => format!(
            "{column} {} {}",
            comparator_sql(*operation),
            typed_parameter(context, value.clone(), table_name, field_name)
        ),
        WhereValue::EntityIdStartsWith(prefix) => {
            let parameter = context.bind_text(format!("{prefix}%"));
            format!("({column})::text LIKE {parameter}")
        }
    }
}

fn typed_parameter(
    context: &mut RenderContext<'_>,
    value: Value,
    table_name: &str,
    field_name: &str,
) -> String {
    let parameter = context.bind(value);
    let key = context.bind_text(field_name);
    format!(
        "(jsonb_populate_record(NULL::{}.{}, jsonb_build_object({key}, {parameter}))).{}",
        quote_identifier(context.namespace),
        quote_identifier(table_name),
        quote_identifier(field_name),
    )
}

const fn comparator_sql(comparator: WhereComparator) -> &'static str {
    match comparator {
        WhereComparator::GreaterThan => ">",
        WhereComparator::GreaterThanOrEqual => ">=",
        WhereComparator::LessThan => "<",
        WhereComparator::LessThanOrEqual => "<=",
        WhereComparator::Like => "LIKE",
        WhereComparator::CaseInsensitiveLike => "ILIKE",
    }
}

const fn reverse_direction(direction: OrderDirection) -> OrderDirection {
    match direction {
        OrderDirection::Asc => OrderDirection::Desc,
        OrderDirection::Desc => OrderDirection::Asc,
    }
}

fn combine_predicates(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("({left}) AND ({right})")),
        (Some(predicate), None) | (None, Some(predicate)) => Some(predicate),
        (None, None) => None,
    }
}

fn where_sql(predicate: Option<&str>) -> String {
    predicate.map_or_else(String::new, |predicate| format!(" WHERE {predicate}"))
}

fn order_sql(alias: &str, order: &OrderPlan, primary_key: &str) -> String {
    match (&order.field, order.direction) {
        (Some(field), Some(direction)) => {
            let direction = match direction {
                OrderDirection::Asc => "ASC",
                OrderDirection::Desc => "DESC",
            };
            let tie_breaker = if field == primary_key {
                String::new()
            } else {
                format!(
                    ", {}.{} {}",
                    quote_identifier(alias),
                    quote_identifier(primary_key),
                    direction
                )
            };
            format!(
                " ORDER BY {}.{} {}{}",
                quote_identifier(alias),
                quote_identifier(field),
                direction,
                tie_breaker
            )
        }
        _ => String::new(),
    }
}

fn limit_sql(form: &Form) -> String {
    form.options
        .limit
        .or(form.options.first)
        .or(form.options.last)
        .map_or_else(String::new, |limit| format!(" LIMIT {limit}"))
}

fn offset_sql(form: &Form) -> String {
    form.options
        .offset
        .map_or_else(String::new, |offset| format!(" OFFSET {offset}"))
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use instant_instaql::{AttributeCatalog, FormOptions, Order};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{ByopPrimaryKey, ByopRelationDirection};

    fn schema() -> ByopSchema {
        let user_id = Uuid::from_u64_pair(1, 1);
        let user_name = Uuid::from_u64_pair(1, 2);
        let post_id = Uuid::from_u64_pair(2, 1);
        let post_author = Uuid::from_u64_pair(2, 2);
        let relation = Uuid::from_u64_pair(3, 0);
        let mut users = ByopTable {
            fields: BTreeMap::from([
                (
                    "id".to_owned(),
                    ByopField {
                        database_type: "uuid".to_owned(),
                        attribute_id: user_id,
                        indexed: true,
                        unique: false,
                    },
                ),
                (
                    "name".to_owned(),
                    ByopField {
                        database_type: "text".to_owned(),
                        attribute_id: user_name,
                        indexed: true,
                        unique: false,
                    },
                ),
            ]),
            attribute_id_to_field: BTreeMap::from([
                (user_id, "id".to_owned()),
                (user_name, "name".to_owned()),
            ]),
            relations: BTreeMap::new(),
            primary_key: ByopPrimaryKey {
                field: "id".to_owned(),
                attribute_id: user_id,
            },
            created_at_field: None,
        };
        let mut posts = ByopTable {
            fields: BTreeMap::from([
                (
                    "id".to_owned(),
                    ByopField {
                        database_type: "int8".to_owned(),
                        attribute_id: post_id,
                        indexed: true,
                        unique: false,
                    },
                ),
                (
                    "author_id".to_owned(),
                    ByopField {
                        database_type: "uuid".to_owned(),
                        attribute_id: post_author,
                        indexed: true,
                        unique: false,
                    },
                ),
            ]),
            attribute_id_to_field: BTreeMap::from([
                (post_id, "id".to_owned()),
                (post_author, "author_id".to_owned()),
            ]),
            relations: BTreeMap::new(),
            primary_key: ByopPrimaryKey {
                field: "id".to_owned(),
                attribute_id: post_id,
            },
            created_at_field: None,
        };
        posts.relations.insert(
            "users".to_owned(),
            ByopRelation {
                direction: ByopRelationDirection::Forward,
                attribute_id: relation,
                table: "posts".to_owned(),
                table_field: "author_id".to_owned(),
                other_table: "users".to_owned(),
                other_table_field: "id".to_owned(),
            },
        );
        users.relations.insert(
            "posts".to_owned(),
            ByopRelation {
                direction: ByopRelationDirection::Reverse,
                attribute_id: relation,
                table: "users".to_owned(),
                table_field: "id".to_owned(),
                other_table: "posts".to_owned(),
                other_table_field: "author_id".to_owned(),
            },
        );
        ByopSchema {
            catalog: AttributeCatalog::default(),
            tables: BTreeMap::from([("users".to_owned(), users), ("posts".to_owned(), posts)]),
        }
    }

    #[test]
    fn renderer_quotes_catalog_names_and_binds_every_value() {
        let schema = schema();
        let name_attr = schema.tables["users"].fields["name"].attribute_id;
        let form = Form {
            key: "users'); DROP TABLE users; --".to_owned(),
            etype: "users".to_owned(),
            level: 0,
            options: FormOptions {
                where_clause: vec![WhereCondition::Condition {
                    path: vec!["name".to_owned()],
                    value: WhereValue::Scalar(json!("O'Reilly")),
                }],
                order: Some(Order {
                    field: "name".to_owned(),
                    direction: OrderDirection::Asc,
                }),
                after: Some(Cursor::Byop {
                    entity_id: json!("ignored"),
                    attr_id: name_attr,
                    value: json!("A"),
                }),
                limit: Some(10),
                ..FormOptions::default()
            },
            children: Vec::new(),
        };
        let rendered = render_byop_query("public", &schema, &[form], false).unwrap();
        assert!(rendered.sql.contains("\"public\".\"users\""));
        assert!(!rendered.sql.contains("DROP TABLE"));
        assert!(!rendered.sql.contains("O'Reilly"));
        assert_eq!(rendered.parameters[0], json!("O'Reilly"));
        assert!(
            rendered
                .sql
                .contains("ORDER BY \"byop_0\".\"name\" ASC, \"byop_0\".\"id\" ASC LIMIT 10")
        );
        assert!(rendered.sql.contains("\"byop_0\".\"name\" >"));
    }

    #[test]
    fn cursors_cross_the_database_null_partition_in_order() {
        let schema = schema();
        let name_attr = schema.tables["users"].fields["name"].attribute_id;
        let form = |cursor: Cursor, before: bool, direction: OrderDirection| Form {
            key: "users".to_owned(),
            etype: "users".to_owned(),
            level: 0,
            options: FormOptions {
                order: Some(Order {
                    field: "name".to_owned(),
                    direction,
                }),
                before: before.then_some(cursor.clone()),
                after: (!before).then_some(cursor),
                limit: Some(10),
                ..FormOptions::default()
            },
            children: Vec::new(),
        };
        let cursor = Cursor::Byop {
            entity_id: json!(1),
            attr_id: name_attr,
            value: Value::Null,
        };
        let rendered = render_byop_query(
            "public",
            &schema,
            &[form(cursor.clone(), true, OrderDirection::Asc)],
            false,
        )
        .unwrap();
        assert!(rendered.sql.contains("\"byop_0\".\"name\" IS NOT NULL"));
        assert_eq!(
            rendered.parameters,
            vec![json!(1), json!("id"), json!("users"), json!("users")]
        );

        let rendered = render_byop_query(
            "public",
            &schema,
            &[form(cursor, false, OrderDirection::Asc)],
            false,
        )
        .unwrap();
        assert!(rendered.sql.contains("FALSE"));
        assert_eq!(rendered.parameters, vec![json!("users"), json!("users")]);

        let non_null_cursor = Cursor::Byop {
            entity_id: json!(1),
            attr_id: name_attr,
            value: json!("Z"),
        };
        for (before, direction, crosses_nulls) in [
            (true, OrderDirection::Asc, false),
            (false, OrderDirection::Asc, true),
            (true, OrderDirection::Desc, true),
            (false, OrderDirection::Desc, false),
        ] {
            let rendered = render_byop_query(
                "public",
                &schema,
                &[form(non_null_cursor.clone(), before, direction)],
                false,
            )
            .unwrap();
            assert_eq!(
                rendered.sql.contains("\"byop_0\".\"name\" IS NULL OR"),
                crosses_nulls,
                "before={before}, direction={direction:?}",
            );
        }
    }

    #[test]
    fn permission_reference_renderer_traverses_relations_and_binds_root_ids() {
        let schema = schema();
        let rendered =
            render_byop_reference("tenant", &schema, "posts", json!(7), "users.name").unwrap();
        assert!(rendered.sql.contains("\"tenant\".\"posts\""));
        assert!(rendered.sql.contains("JOIN \"tenant\".\"users\""));
        assert!(rendered.sql.contains("\"byop_ref_1\".\"name\""));
        assert!(rendered.sql.contains("jsonb_populate_record"));
        assert!(rendered.sql.contains("NULL::\"tenant\".\"posts\""));
        assert!(rendered.sql.contains("ORDER BY \"byop_ref_1\".\"name\""));
        assert!(!rendered.sql.contains("ORDER BY to_jsonb"));
        assert_eq!(rendered.parameters, vec![json!(7), json!("id")]);

        let malicious = render_byop_reference(
            "tenant\"; DROP SCHEMA public; --",
            &schema,
            "posts",
            json!(7),
            "users.name",
        )
        .unwrap();
        assert!(
            malicious
                .sql
                .contains("\"tenant\"\"; DROP SCHEMA public; --\"")
        );
        assert!(!malicious.sql.contains("DROP SCHEMA public; --.\"posts\""));
    }

    #[test]
    fn permission_constraints_are_applied_to_roots_and_children() {
        let schema = schema();
        let forms = vec![Form {
            key: "posts".to_owned(),
            etype: "posts".to_owned(),
            level: 0,
            options: FormOptions::default(),
            children: vec![Form {
                key: "users".to_owned(),
                etype: "users".to_owned(),
                level: 1,
                options: FormOptions::default(),
                children: Vec::new(),
            }],
        }];
        let constraints = HashMap::from([
            (
                "posts".to_owned(),
                QueryConstraint::Where(vec![WhereCondition::Condition {
                    path: vec!["id".to_owned()],
                    value: WhereValue::Scalar(json!(7)),
                }]),
            ),
            ("users".to_owned(), QueryConstraint::DenyAll),
        ]);
        let rendered =
            render_byop_query_with_constraints("public", &schema, &forms, false, &constraints)
                .unwrap();
        assert!(rendered.sql.contains("FALSE"));
        assert_eq!(rendered.parameters[0], json!(7));
    }

    #[test]
    fn shapes_nested_rows_inference_aggregates_and_three_field_cursors() {
        let schema = schema();
        let name_attr = schema.tables["users"].fields["name"].attribute_id;
        let raw = json!([{
            "k": "users",
            "etype": "users",
            "data": {
                "rows": [{
                    "row": {"id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "name": "Oren"},
                    "children": [{
                        "k": "posts",
                        "etype": "posts",
                        "data": {"rows": [{"row": {"id": 7, "author_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}}]}
                    }]
                }],
                "page-info": {"sort-field": "name", "reversed": false}
            }
        }, {
            "k": "postCount",
            "etype": "posts",
            "data": {"aggregate": {"count": 3}}
        }]);
        let response = shape_byop_response(&schema, &raw, false).unwrap();
        assert_eq!(response.data["users"][0]["posts"][0]["id"], 7);
        assert_eq!(response.aggregates["postCount"]["count"], 3);
        assert_eq!(
            response.page_info["users"].start_cursor,
            Some(vec![
                json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
                json!(name_attr),
                json!("Oren")
            ])
        );

        let inferred = shape_byop_response(
            &schema,
            &json!([{
                "k": "posts",
                "etype": "posts",
                "data": {"rows": [{
                    "row": {"id": 7, "author_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"},
                    "children": [{
                        "k": "users",
                        "etype": "users",
                        "data": {"rows": [{"row": {"id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "name": "Oren"}}]}
                    }]
                }]}
            }]),
            true,
        )
        .unwrap();
        assert_eq!(inferred.data["posts"][0]["users"]["name"], "Oren");

        let mut one_to_one = schema.clone();
        one_to_one
            .tables
            .get_mut("posts")
            .unwrap()
            .fields
            .get_mut("author_id")
            .unwrap()
            .unique = true;
        let reverse_inferred = shape_byop_response(
            &one_to_one,
            &json!([{
                "k": "users",
                "etype": "users",
                "data": {"rows": [{
                    "row": {"id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"},
                    "children": [{
                        "k": "posts",
                        "etype": "posts",
                        "data": {"rows": [{"row": {"id": 7}}]}
                    }]
                }]}
            }]),
            true,
        )
        .unwrap();
        assert_eq!(reverse_inferred.data["users"][0]["posts"]["id"], 7);
    }
}
