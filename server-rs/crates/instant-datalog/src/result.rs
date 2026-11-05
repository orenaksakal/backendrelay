use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    Component, FunctionClause, IndexKey, NormalizedClause, PagePlanMetadata, Pattern,
    PatternMetadata, PatternMetadataNode, Symbol, Term,
};

#[path = "result_grouped_json.rs"]
mod grouped_json;
#[path = "result_raw_json.rs"]
mod raw_json;
pub use grouped_json::UuidGroupedJsonRows;
pub use raw_json::{
    UuidGroupedRawJsonRows, decode_raw_json_paged_query_rows_with_options,
    decode_raw_json_query_rows_with_options,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum DatalogValue {
    Uuid(Uuid),
    Json(Value),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DatalogTriple {
    pub entity_id: Uuid,
    pub attr_id: Uuid,
    pub value: DatalogValue,
    pub created_at: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "values", rename_all = "kebab-case")]
pub enum TopicPart {
    Any,
    Values(Vec<DatalogValue>),
    /// A typed exclusion retained separately from the generic serialized
    /// function envelope. Reference UUIDs must remain UUIDs through realtime
    /// intersection rather than becoming indistinguishable JSON strings.
    Not(DatalogValue),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InvalidationTopic {
    pub indexes: Vec<IndexKey>,
    pub entity: TopicPart,
    pub attribute: TopicPart,
    pub value: TopicPart,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct DatalogResult {
    pub topics: Vec<InvalidationTopic>,
    pub symbol_values: HashMap<Symbol, Vec<DatalogValue>>,
    pub join_rows: Vec<Vec<DatalogTriple>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_info: Option<PageResultInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<BTreeMap<String, i64>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PageCursorResult {
    pub entity_id: Uuid,
    pub attr_id: Uuid,
    pub value: Option<DatalogValue>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PageResultInfo {
    pub start_cursor: Option<PageCursorResult>,
    pub end_cursor: Option<PageCursorResult>,
    pub has_next_page: bool,
    pub has_previous_page: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum QueryCell {
    Uuid(Option<Uuid>),
    Json(Option<Value>),
    Boolean(Option<bool>),
    Integer(Option<i64>),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct QueryRow {
    pub cells: BTreeMap<String, QueryCell>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResultDecodeOptions {
    /// Retains known non-reference variables as empty topic bindings when a
    /// pattern returns no rows, preventing them from widening to wildcards in
    /// later invalidation topics.
    pub ensure_empty_symbol_values_for_topics: bool,
}

/// Constructs the reference result returned when schema lookup fails while
/// compiling an otherwise well-formed query.
///
/// No SQL is executed. Variables mentioned by the sentinel clauses are retained
/// with empty value lists and the two broad EA/EAV topics ensure a later schema or
/// data change can invalidate the cached empty result.
#[must_use]
pub fn missing_attribute_result(clauses: &[NormalizedClause]) -> DatalogResult {
    let mut symbol_values = HashMap::new();
    collect_clause_symbols(clauses, &mut symbol_values);
    DatalogResult {
        topics: vec![
            InvalidationTopic {
                indexes: vec![IndexKey::Ea],
                entity: TopicPart::Any,
                attribute: TopicPart::Any,
                value: TopicPart::Any,
            },
            InvalidationTopic {
                indexes: vec![IndexKey::Eav],
                entity: TopicPart::Any,
                attribute: TopicPart::Any,
                value: TopicPart::Any,
            },
        ],
        symbol_values,
        ..DatalogResult::default()
    }
}

fn collect_clause_symbols(
    clauses: &[NormalizedClause],
    symbols: &mut HashMap<Symbol, Vec<DatalogValue>>,
) {
    for clause in clauses {
        match clause {
            NormalizedClause::Pattern { pattern } => {
                for term in [
                    &pattern.entity,
                    &pattern.attribute,
                    &pattern.value,
                    &pattern.created_at,
                ] {
                    if let Term::Variable(symbol) = term {
                        symbols.entry(symbol.clone()).or_default();
                    }
                }
            }
            NormalizedClause::And { clauses }
            | NormalizedClause::Or {
                clauses,
                join_symbol: _,
            } => collect_clause_symbols(clauses, symbols),
        }
    }
}

impl QueryRow {
    #[must_use]
    pub fn with_cell(mut self, name: impl Into<String>, cell: QueryCell) -> Self {
        self.cells.insert(name.into(), cell);
        self
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ResultDecodeError {
    #[error("pattern metadata must name exactly five CTE columns, got {0}")]
    InvalidMetadataColumns(usize),
    #[error("query row is missing column {0}")]
    MissingColumn(String),
    #[error("query column {column} has type {actual}, expected {expected}")]
    InvalidColumnType {
        column: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("query column {column} contains invalid reference UUID {value}")]
    InvalidReferenceUuid { column: String, value: String },
    #[error("query row has an entity but a null attribute in column {0}")]
    NullAttribute(String),
    #[error("query row has an entity but a null value in column {0}")]
    NullValue(String),
    #[error("query row has an entity but a null created-at value in column {0}")]
    NullCreatedAt(String),
    #[error("pattern symbol field index {0} is outside the triple")]
    InvalidSymbolField(usize),
    #[error("batched query rows must be a JSON array")]
    InvalidBatchRows,
    #[error("batched query row at index {0} must be a JSON object")]
    InvalidBatchRow(usize),
    #[error("batched query column {column} must be {expected}, got {value}")]
    InvalidBatchColumn {
        column: String,
        expected: &'static str,
        value: String,
    },
    #[error("nested join symbol {symbol} resolved to a non-UUID value {value}")]
    InvalidNestedJoinValue { symbol: String, value: String },
}

/// Decodes SQL rows using the recursive metadata emitted by [`crate::plan_match_query`].
///
/// This follows the reference server's `sql-result->result` state machine: OR
/// branches share output rows but accumulate symbols from the same pre-branch
/// snapshot, missing branch triples are discarded, and topics only use bindings
/// established by earlier patterns.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] when row columns do not match planner metadata or
/// a reference JSON value is not a UUID.
pub fn decode_query_rows(
    rows: &[QueryRow],
    metadata: &[PatternMetadataNode],
) -> Result<DatalogResult, ResultDecodeError> {
    decode_query_rows_with_options(rows, metadata, ResultDecodeOptions::default())
}

/// Decodes SQL rows with live result-compatibility controls.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] under the same conditions as [`decode_query_rows`].
pub fn decode_query_rows_with_options(
    rows: &[QueryRow],
    metadata: &[PatternMetadataNode],
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    decode_result_rows(rows, metadata, options)
}

fn decode_result_rows<R: ResultRow>(
    rows: &[R],
    metadata: &[PatternMetadataNode],
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    let mut accumulator = Accumulator {
        join_rows: vec![Vec::new(); rows.len()],
        ensure_empty_symbol_values_for_topics: options.ensure_empty_symbol_values_for_topics,
        ..Accumulator::default()
    };
    accumulate_nodes(&mut accumulator, rows, metadata)?;
    deduplicate(&mut accumulator.join_rows);
    Ok(DatalogResult {
        topics: accumulator.topics,
        symbol_values: accumulator.symbol_values,
        join_rows: accumulator.join_rows,
        page_info: None,
        aggregate: None,
    })
}

/// Decodes the `json_agg(row_to_json(...))` rows returned by a batched query.
///
/// UUID and scalar column types are reconstructed from planner metadata before
/// delegating to [`decode_query_rows`]. A JSON `null` value is preserved for an
/// existing triple; absent OR branches are identified by their null entity column.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] for a malformed batch envelope or incompatible cell.
pub fn decode_json_query_rows(
    rows: &Value,
    metadata: &[PatternMetadataNode],
) -> Result<DatalogResult, ResultDecodeError> {
    decode_json_query_rows_with_options(rows, metadata, ResultDecodeOptions::default())
}

/// JSON-envelope equivalent of [`decode_query_rows_with_options`].
///
/// # Errors
///
/// Returns [`ResultDecodeError`] for a malformed batch envelope or incompatible cell.
pub fn decode_json_query_rows_with_options(
    rows: &Value,
    metadata: &[PatternMetadataNode],
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    if direct_json_columns(metadata, None)
        && let Ok(rows) = borrowed_json_rows(rows)
        && let Ok(result) = decode_result_rows(&rows, metadata, options)
    {
        return Ok(result);
    }
    // Eager conversion defines error precedence: row-major cell validation,
    // then pattern-major accumulation. Retain it for malformed inputs and
    // unusual aliased metadata, not just equivalent error categories.
    let query_rows = json_query_rows(rows, metadata)?;
    decode_query_rows_with_options(&query_rows, metadata, options)
}

/// Keeps only JSON-envelope rows whose decoded symbol bindings contain `value`.
///
/// `InstaQL` nesting joins entity identifiers, so a non-UUID binding is rejected
/// instead of being stringified into an ambiguous grouping key.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] when the JSON rows are malformed, cannot be
/// decoded with `metadata`, or the requested symbol has a non-UUID value.
pub fn filter_json_query_rows_by_uuid_symbol(
    rows: &Value,
    metadata: &[PatternMetadataNode],
    symbol: &Symbol,
    value: Uuid,
) -> Result<Value, ResultDecodeError> {
    let rows = rows.as_array().ok_or(ResultDecodeError::InvalidBatchRows)?;
    if direct_json_columns(metadata, None) {
        let borrowed: Option<Vec<_>> = rows.iter().map(Value::as_object).collect();
        if let Some(borrowed) = borrowed
            && let Ok(result) = filter_result_rows(rows, &borrowed, metadata, symbol, value)
        {
            return Ok(result);
        }
    }
    let decoded_rows = json_query_rows_from_slice(rows, metadata)?;
    filter_result_rows(rows, &decoded_rows, metadata, symbol, value)
}

fn filter_result_rows<R: ResultRow>(
    rows: &[Value],
    decoded_rows: &[R],
    metadata: &[PatternMetadataNode],
    symbol: &Symbol,
    value: Uuid,
) -> Result<Value, ResultDecodeError> {
    let patterns = flattened_pattern_metadata(metadata);
    let mut filtered = Vec::new();
    for (row, decoded) in rows.iter().zip(decoded_rows) {
        let mut matches = false;
        for pattern in &patterns {
            let Some(triple) = decoded.triple(&pattern.cte_columns)? else {
                continue;
            };
            for (index, field) in &pattern.symbol_fields {
                if &field.symbol == symbol {
                    match triple_component(&triple, *index)? {
                        DatalogValue::Uuid(candidate) => matches |= candidate == value,
                        DatalogValue::Json(candidate) => {
                            return Err(ResultDecodeError::InvalidNestedJoinValue {
                                symbol: symbol.as_str().to_owned(),
                                value: candidate.to_string(),
                            });
                        }
                    }
                }
            }
        }
        if matches {
            filtered.push(row.clone());
        }
    }
    Ok(Value::Array(filtered))
}

/// Clones query metadata with one entity join symbol replaced by a UUID constant.
///
/// This mirrors the JVM's scoped nested-result metadata: the inherited parent
/// binding is removed from `symbol-values` and invalidation topics are qualified
/// with the concrete entity ID.
#[must_use]
pub fn bind_pattern_metadata_uuid_symbol(
    metadata: &[PatternMetadataNode],
    symbol: &Symbol,
    value: Uuid,
) -> Vec<PatternMetadataNode> {
    metadata
        .iter()
        .map(|node| bind_metadata_node(node, symbol, value))
        .collect()
}

fn bind_metadata_node(
    node: &PatternMetadataNode,
    symbol: &Symbol,
    value: Uuid,
) -> PatternMetadataNode {
    match node {
        PatternMetadataNode::Pattern(metadata) => {
            let mut metadata = metadata.clone();
            for term in [
                &mut metadata.pattern.entity,
                &mut metadata.pattern.attribute,
                &mut metadata.pattern.value,
                &mut metadata.pattern.created_at,
            ] {
                if matches!(term, Term::Variable(candidate) if candidate == symbol) {
                    *term = Term::constants([Value::String(value.to_string())]);
                }
            }
            metadata
                .symbol_fields
                .retain(|_, field| &field.symbol != symbol);
            PatternMetadataNode::Pattern(metadata)
        }
        PatternMetadataNode::Or(branches) => PatternMetadataNode::Or(
            branches
                .iter()
                .map(|branch| {
                    branch
                        .iter()
                        .map(|node| bind_metadata_node(node, symbol, value))
                        .collect()
                })
                .collect(),
        ),
    }
}

/// Decodes page rows and attaches stable cursors and page-boundary flags.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] when the page cursor columns are missing or malformed.
pub fn decode_paged_query_rows(
    rows: &[QueryRow],
    metadata: &[PatternMetadataNode],
    page: &PagePlanMetadata,
    has_next_page: bool,
    has_previous_page: bool,
) -> Result<DatalogResult, ResultDecodeError> {
    decode_paged_query_rows_with_options(
        rows,
        metadata,
        page,
        has_next_page,
        has_previous_page,
        ResultDecodeOptions::default(),
    )
}

/// Decodes page rows with live result-compatibility controls.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] under the same conditions as [`decode_paged_query_rows`].
pub fn decode_paged_query_rows_with_options(
    rows: &[QueryRow],
    metadata: &[PatternMetadataNode],
    page: &PagePlanMetadata,
    has_next_page: bool,
    has_previous_page: bool,
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    decode_paged_result_rows(
        rows,
        metadata,
        page,
        has_next_page,
        has_previous_page,
        options,
    )
}

fn decode_paged_result_rows<R: ResultRow>(
    rows: &[R],
    metadata: &[PatternMetadataNode],
    page: &PagePlanMetadata,
    has_next_page: bool,
    has_previous_page: bool,
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    let mut result = decode_result_rows(rows, metadata, options)?;
    let mut cursors = rows
        .iter()
        .filter_map(|row| match row.triple(&page.cursor_columns) {
            Ok(Some(triple)) => Some(Ok(PageCursorResult {
                entity_id: triple.entity_id,
                attr_id: triple.attr_id,
                value: Some(triple.value),
                created_at: triple.created_at,
            })),
            Ok(None) => match missing_value_page_cursor(row, page) {
                Ok(cursor) => cursor.map(Ok),
                Err(error) => Some(Err(error)),
            },
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if page.reverse_results {
        cursors.reverse();
        result.join_rows.reverse();
    }
    result.page_info = Some(PageResultInfo {
        start_cursor: cursors.first().cloned(),
        end_cursor: cursors.last().cloned(),
        has_next_page,
        has_previous_page: has_previous_page || (rows.is_empty() && page.empty_page_has_previous),
    });
    Ok(result)
}

/// JSON-envelope equivalent of [`decode_paged_query_rows`].
///
/// # Errors
///
/// Returns [`ResultDecodeError`] for malformed JSON rows or cursor columns.
pub fn decode_json_paged_query_rows(
    rows: &Value,
    metadata: &[PatternMetadataNode],
    page: &PagePlanMetadata,
    has_next_page: bool,
    has_previous_page: bool,
) -> Result<DatalogResult, ResultDecodeError> {
    decode_json_paged_query_rows_with_options(
        rows,
        metadata,
        page,
        has_next_page,
        has_previous_page,
        ResultDecodeOptions::default(),
    )
}

/// JSON-envelope equivalent of [`decode_paged_query_rows_with_options`].
///
/// # Errors
///
/// Returns [`ResultDecodeError`] for malformed JSON rows or cursor columns.
pub fn decode_json_paged_query_rows_with_options(
    rows: &Value,
    metadata: &[PatternMetadataNode],
    page: &PagePlanMetadata,
    has_next_page: bool,
    has_previous_page: bool,
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    if direct_json_columns(metadata, Some(page))
        && let Ok(rows) = borrowed_json_rows(rows)
    {
        // Validate fallback columns on EVERY row, even if a present cursor
        // triple means they will not actually be needed for that row.
        let fallback_valid = rows.iter().all(|row| {
            batch_uuid(row, &page.fallback_entity_column).is_ok()
                && batch_integer(row, &page.fallback_created_at_column).is_ok()
        });
        if fallback_valid
            && let Ok(result) = decode_paged_result_rows(
                &rows,
                metadata,
                page,
                has_next_page,
                has_previous_page,
                options,
            )
        {
            return Ok(result);
        }
    }
    let mut query_rows = json_query_rows(rows, metadata)?;
    let raw_rows = rows.as_array().ok_or(ResultDecodeError::InvalidBatchRows)?;
    for (raw_row, query_row) in raw_rows.iter().zip(&mut query_rows) {
        let raw_row = raw_row
            .as_object()
            .ok_or(ResultDecodeError::InvalidBatchRows)?;
        query_row.cells.insert(
            page.fallback_entity_column.clone(),
            QueryCell::Uuid(batch_uuid(raw_row, &page.fallback_entity_column)?),
        );
        query_row.cells.insert(
            page.fallback_created_at_column.clone(),
            QueryCell::Integer(batch_integer(raw_row, &page.fallback_created_at_column)?),
        );
    }
    decode_paged_query_rows_with_options(
        &query_rows,
        metadata,
        page,
        has_next_page,
        has_previous_page,
        options,
    )
}

fn missing_value_page_cursor<R: ResultRow>(
    row: &R,
    page: &PagePlanMetadata,
) -> Result<Option<PageCursorResult>, ResultDecodeError> {
    let Some(entity_id) = row.uuid(&page.fallback_entity_column)? else {
        return Ok(None);
    };
    let created_at = row
        .integer(&page.fallback_created_at_column)?
        .ok_or_else(|| ResultDecodeError::NullCreatedAt(page.fallback_created_at_column.clone()))?;
    Ok(Some(PageCursorResult {
        entity_id,
        attr_id: page.attr_id,
        value: None,
        created_at,
    }))
}

fn json_query_rows(
    rows: &Value,
    metadata: &[PatternMetadataNode],
) -> Result<Vec<QueryRow>, ResultDecodeError> {
    let rows = rows.as_array().ok_or(ResultDecodeError::InvalidBatchRows)?;
    json_query_rows_from_slice(rows, metadata)
}

fn json_query_rows_from_slice(
    rows: &[Value],
    metadata: &[PatternMetadataNode],
) -> Result<Vec<QueryRow>, ResultDecodeError> {
    let patterns = flattened_pattern_metadata(metadata);
    let query_rows = rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let row = row
                .as_object()
                .ok_or(ResultDecodeError::InvalidBatchRow(row_index))?;
            let mut query_row = QueryRow::default();
            for pattern in &patterns {
                let [entity, attribute, value, reference, created_at] =
                    pattern.cte_columns.as_slice()
                else {
                    return Err(ResultDecodeError::InvalidMetadataColumns(
                        pattern.cte_columns.len(),
                    ));
                };
                query_row
                    .cells
                    .insert(entity.clone(), QueryCell::Uuid(batch_uuid(row, entity)?));
                query_row.cells.insert(
                    attribute.clone(),
                    QueryCell::Uuid(batch_uuid(row, attribute)?),
                );
                let json_value = batch_value(row, value)?;
                query_row
                    .cells
                    .insert(value.clone(), QueryCell::Json(Some(json_value.clone())));
                query_row.cells.insert(
                    reference.clone(),
                    QueryCell::Boolean(batch_boolean(row, reference)?),
                );
                query_row.cells.insert(
                    created_at.clone(),
                    QueryCell::Integer(batch_integer(row, created_at)?),
                );
            }
            Ok(query_row)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(query_rows)
}

#[derive(Clone, Debug, Default)]
struct Accumulator {
    join_rows: Vec<Vec<DatalogTriple>>,
    symbol_values: HashMap<Symbol, Vec<DatalogValue>>,
    topic_symbol_values: HashMap<Symbol, Vec<DatalogValue>>,
    topics: Vec<InvalidationTopic>,
    ensure_empty_symbol_values_for_topics: bool,
}

// Successful JSON-envelope reads borrow the fresh row rather than allocating
// five string-keyed QueryCells per pattern per row. The owned decoder remains
// the reference/error path and all accumulation semantics are shared.
trait ResultRow {
    fn triple(&self, columns: &[String]) -> Result<Option<DatalogTriple>, ResultDecodeError>;
    fn uuid(&self, column: &str) -> Result<Option<Uuid>, ResultDecodeError>;
    fn integer(&self, column: &str) -> Result<Option<i64>, ResultDecodeError>;
}

impl ResultRow for QueryRow {
    fn triple(&self, columns: &[String]) -> Result<Option<DatalogTriple>, ResultDecodeError> {
        decode_triple(self, columns)
    }

    fn uuid(&self, column: &str) -> Result<Option<Uuid>, ResultDecodeError> {
        uuid_cell(self, column)
    }

    fn integer(&self, column: &str) -> Result<Option<i64>, ResultDecodeError> {
        integer_cell(self, column)
    }
}

impl ResultRow for &serde_json::Map<String, Value> {
    fn triple(&self, columns: &[String]) -> Result<Option<DatalogTriple>, ResultDecodeError> {
        let [entity, attribute, value, reference, created] = columns else {
            return Err(ResultDecodeError::InvalidMetadataColumns(columns.len()));
        };
        let entity_id = batch_uuid(self, entity)?;
        let attr_id = batch_uuid(self, attribute)?;
        let json_value = batch_value(self, value)?;
        let reference = batch_boolean(self, reference)?.unwrap_or(false);
        let created_at = batch_integer(self, created)?;
        // Even an absent OR branch must validate every scalar column, just as
        // the eager conversion does before discarding its null-entity triple.
        let Some(entity_id) = entity_id else {
            return Ok(None);
        };
        let attr_id = attr_id.ok_or_else(|| ResultDecodeError::NullAttribute(attribute.clone()))?;
        let value = if reference {
            let encoded =
                json_value
                    .as_str()
                    .ok_or_else(|| ResultDecodeError::InvalidReferenceUuid {
                        column: value.clone(),
                        value: json_value.to_string(),
                    })?;
            DatalogValue::Uuid(Uuid::parse_str(encoded).map_err(|_| {
                ResultDecodeError::InvalidReferenceUuid {
                    column: value.clone(),
                    value: encoded.to_owned(),
                }
            })?)
        } else {
            DatalogValue::Json(json_value.clone())
        };
        let created_at =
            created_at.ok_or_else(|| ResultDecodeError::NullCreatedAt(created.clone()))?;
        Ok(Some(DatalogTriple {
            entity_id,
            attr_id,
            value,
            created_at,
        }))
    }

    fn uuid(&self, column: &str) -> Result<Option<Uuid>, ResultDecodeError> {
        batch_uuid(self, column)
    }

    fn integer(&self, column: &str) -> Result<Option<i64>, ResultDecodeError> {
        batch_integer(self, column)
    }
}

fn borrowed_json_rows(
    rows: &Value,
) -> Result<Vec<&serde_json::Map<String, Value>>, ResultDecodeError> {
    rows.as_array()
        .ok_or(ResultDecodeError::InvalidBatchRows)?
        .iter()
        .enumerate()
        .map(|(index, row)| {
            row.as_object()
                .ok_or(ResultDecodeError::InvalidBatchRow(index))
        })
        .collect()
}

fn direct_json_columns(metadata: &[PatternMetadataNode], page: Option<&PagePlanMetadata>) -> bool {
    let mut columns = std::collections::BTreeSet::new();
    let patterns = flattened_pattern_metadata(metadata);
    for pattern in &patterns {
        if pattern.cte_columns.len() != 5 {
            return false;
        }
        for column in &pattern.cte_columns {
            if !columns.insert(column.as_str()) {
                return false; // Retain the original map's overwrite semantics.
            }
        }
    }
    page.is_none_or(|page| {
        page.cursor_columns.len() == 5
            && page
                .cursor_columns
                .iter()
                .enumerate()
                .all(|(index, column)| {
                    patterns
                        .iter()
                        .any(|pattern| pattern.cte_columns[index] == *column)
                })
            && !columns.contains(page.fallback_entity_column.as_str())
            && !columns.contains(page.fallback_created_at_column.as_str())
            && page.fallback_entity_column != page.fallback_created_at_column
    })
}

fn accumulate_nodes<R: ResultRow>(
    accumulator: &mut Accumulator,
    rows: &[R],
    metadata: &[PatternMetadataNode],
) -> Result<(), ResultDecodeError> {
    for node in metadata {
        match node {
            PatternMetadataNode::Pattern(pattern) => {
                accumulate_pattern(accumulator, rows, pattern)?;
            }
            PatternMetadataNode::Or(branches) => {
                accumulate_or(accumulator, rows, branches)?;
            }
        }
    }
    Ok(())
}

fn flattened_pattern_metadata(metadata: &[PatternMetadataNode]) -> Vec<&PatternMetadata> {
    let mut patterns = Vec::new();
    for node in metadata {
        match node {
            PatternMetadataNode::Pattern(pattern) => patterns.push(pattern),
            PatternMetadataNode::Or(branches) => {
                for branch in branches {
                    patterns.extend(flattened_pattern_metadata(branch));
                }
            }
        }
    }
    patterns
}

fn accumulate_or<R: ResultRow>(
    accumulator: &mut Accumulator,
    rows: &[R],
    branches: &[Vec<PatternMetadataNode>],
) -> Result<(), ResultDecodeError> {
    let baseline_symbols = accumulator.symbol_values.clone();
    let baseline_topic_symbols = accumulator.topic_symbol_values.clone();
    let mut branch_symbols = Vec::with_capacity(branches.len());
    let mut branch_topic_symbols = Vec::with_capacity(branches.len());

    for branch in branches {
        accumulator.symbol_values.clone_from(&baseline_symbols);
        accumulator
            .topic_symbol_values
            .clone_from(&baseline_topic_symbols);
        accumulate_nodes(accumulator, rows, branch)?;
        branch_symbols.push(accumulator.symbol_values.clone());
        branch_topic_symbols.push(accumulator.topic_symbol_values.clone());
    }

    accumulator.symbol_values = baseline_symbols;
    accumulator.topic_symbol_values = baseline_topic_symbols;
    for symbols in branch_symbols {
        merge_symbol_values(&mut accumulator.symbol_values, symbols);
    }
    for symbols in branch_topic_symbols {
        merge_symbol_values(&mut accumulator.topic_symbol_values, symbols);
    }
    Ok(())
}

fn accumulate_pattern<R: ResultRow>(
    accumulator: &mut Accumulator,
    rows: &[R],
    metadata: &PatternMetadata,
) -> Result<(), ResultDecodeError> {
    accumulator.topics.extend(pattern_topics(
        &metadata.pattern,
        &accumulator.topic_symbol_values,
    ));

    for (row_index, row) in rows.iter().enumerate() {
        let Some(triple) = row.triple(&metadata.cte_columns)? else {
            continue;
        };
        for (triple_index, field) in &metadata.symbol_fields {
            let value = triple_component(&triple, *triple_index)?;
            push_unique(
                accumulator
                    .symbol_values
                    .entry(field.symbol.clone())
                    .or_default(),
                value.clone(),
            );
            if !field.reference_value {
                push_unique(
                    accumulator
                        .topic_symbol_values
                        .entry(field.symbol.clone())
                        .or_default(),
                    value,
                );
            }
        }
        accumulator.join_rows[row_index].push(triple);
    }
    for field in metadata.symbol_fields.values() {
        accumulator
            .symbol_values
            .entry(field.symbol.clone())
            .or_default();
        if accumulator.ensure_empty_symbol_values_for_topics && !field.reference_value {
            accumulator
                .topic_symbol_values
                .entry(field.symbol.clone())
                .or_default();
        }
    }
    Ok(())
}

fn decode_triple(
    row: &QueryRow,
    columns: &[String],
) -> Result<Option<DatalogTriple>, ResultDecodeError> {
    let [
        entity_column,
        attribute_column,
        value_column,
        reference_column,
        created_column,
    ] = columns
    else {
        return Err(ResultDecodeError::InvalidMetadataColumns(columns.len()));
    };
    let Some(entity_id) = uuid_cell(row, entity_column)? else {
        return Ok(None);
    };
    let attr_id = uuid_cell(row, attribute_column)?
        .ok_or_else(|| ResultDecodeError::NullAttribute(attribute_column.clone()))?;
    let json_value = json_cell(row, value_column)?
        .ok_or_else(|| ResultDecodeError::NullValue(value_column.clone()))?;
    let reference = boolean_cell(row, reference_column)?.unwrap_or(false);
    let value = if reference {
        let encoded =
            json_value
                .as_str()
                .ok_or_else(|| ResultDecodeError::InvalidReferenceUuid {
                    column: value_column.clone(),
                    value: json_value.to_string(),
                })?;
        DatalogValue::Uuid(Uuid::parse_str(encoded).map_err(|_| {
            ResultDecodeError::InvalidReferenceUuid {
                column: value_column.clone(),
                value: encoded.to_owned(),
            }
        })?)
    } else {
        DatalogValue::Json(json_value)
    };
    let created_at = integer_cell(row, created_column)?
        .ok_or_else(|| ResultDecodeError::NullCreatedAt(created_column.clone()))?;
    Ok(Some(DatalogTriple {
        entity_id,
        attr_id,
        value,
        created_at,
    }))
}

fn triple_component(
    triple: &DatalogTriple,
    index: usize,
) -> Result<DatalogValue, ResultDecodeError> {
    match index {
        0 => Ok(DatalogValue::Uuid(triple.entity_id)),
        1 => Ok(DatalogValue::Uuid(triple.attr_id)),
        2 => Ok(triple.value.clone()),
        index => Err(ResultDecodeError::InvalidSymbolField(index)),
    }
}

fn pattern_topics(
    pattern: &Pattern,
    symbols: &HashMap<Symbol, Vec<DatalogValue>>,
) -> Vec<InvalidationTopic> {
    if let Term::Function(FunctionClause::IsNull(clause)) = &pattern.value {
        return vec![
            InvalidationTopic {
                indexes: vec![IndexKey::Ea],
                entity: term_topic_part(&pattern.entity, Component::Entity, false, symbols),
                attribute: term_topic_part(
                    &pattern.attribute,
                    Component::Attribute,
                    false,
                    symbols,
                ),
                value: TopicPart::Any,
            },
            InvalidationTopic {
                indexes: vec![IndexKey::Ea],
                entity: TopicPart::Any,
                attribute: TopicPart::Values(vec![DatalogValue::Uuid(clause.attr_id)]),
                value: TopicPart::Any,
            },
        ];
    }
    vec![InvalidationTopic {
        indexes: vec![pattern.index.key],
        entity: term_topic_part(&pattern.entity, Component::Entity, false, symbols),
        attribute: term_topic_part(&pattern.attribute, Component::Attribute, false, symbols),
        value: term_topic_part(
            &pattern.value,
            Component::Value,
            pattern.index.key.value_is_reference(),
            symbols,
        ),
    }]
}

fn term_topic_part(
    term: &Term,
    component: Component,
    reference_value: bool,
    symbols: &HashMap<Symbol, Vec<DatalogValue>>,
) -> TopicPart {
    match term {
        Term::Any => TopicPart::Any,
        Term::Variable(symbol) => symbols
            .get(symbol)
            .cloned()
            .map_or(TopicPart::Any, TopicPart::Values),
        Term::Constants(values) => TopicPart::Values(
            values
                .iter()
                .cloned()
                .map(|value| component_value(component, reference_value, value))
                .collect(),
        ),
        Term::Function(FunctionClause::Not(value))
            if (matches!(component, Component::Entity) || reference_value)
                && value
                    .as_str()
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .is_some() =>
        {
            TopicPart::Not(DatalogValue::Uuid(
                Uuid::parse_str(value.as_str().expect("checked above")).expect("checked above"),
            ))
        }
        Term::Function(function) => TopicPart::Values(vec![DatalogValue::Json(
            serde_json::to_value(function).unwrap_or(Value::Null),
        )]),
    }
}

fn component_value(component: Component, reference_value: bool, value: Value) -> DatalogValue {
    if (matches!(component, Component::Entity | Component::Attribute) || reference_value)
        && let Some(uuid) = value.as_str().and_then(|value| Uuid::parse_str(value).ok())
    {
        return DatalogValue::Uuid(uuid);
    }
    DatalogValue::Json(value)
}

fn merge_symbol_values(
    destination: &mut HashMap<Symbol, Vec<DatalogValue>>,
    source: HashMap<Symbol, Vec<DatalogValue>>,
) {
    for (symbol, values) in source {
        let destination_values = destination.entry(symbol).or_default();
        for value in values {
            push_unique(destination_values, value);
        }
    }
}

fn push_unique(values: &mut Vec<DatalogValue>, value: DatalogValue) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn deduplicate<T: PartialEq>(values: &mut Vec<T>) {
    let mut index = 0;
    while index < values.len() {
        if values[..index].contains(&values[index]) {
            values.remove(index);
        } else {
            index += 1;
        }
    }
}

fn cell<'a>(row: &'a QueryRow, column: &str) -> Result<&'a QueryCell, ResultDecodeError> {
    row.cells
        .get(column)
        .ok_or_else(|| ResultDecodeError::MissingColumn(column.to_owned()))
}

fn cell_name(cell: &QueryCell) -> &'static str {
    match cell {
        QueryCell::Uuid(_) => "uuid",
        QueryCell::Json(_) => "json",
        QueryCell::Boolean(_) => "boolean",
        QueryCell::Integer(_) => "integer",
    }
}

macro_rules! cell_reader {
    ($name:ident, $variant:ident, $type:ty, $expected:literal) => {
        fn $name(row: &QueryRow, column: &str) -> Result<Option<$type>, ResultDecodeError> {
            let value = cell(row, column)?;
            if let QueryCell::$variant(value) = value {
                Ok(value.clone())
            } else {
                Err(ResultDecodeError::InvalidColumnType {
                    column: column.to_owned(),
                    expected: $expected,
                    actual: cell_name(value),
                })
            }
        }
    };
}

cell_reader!(uuid_cell, Uuid, Uuid, "uuid");
cell_reader!(json_cell, Json, Value, "json");
cell_reader!(boolean_cell, Boolean, bool, "boolean");
cell_reader!(integer_cell, Integer, i64, "integer");

fn batch_value<'a>(
    row: &'a serde_json::Map<String, Value>,
    column: &str,
) -> Result<&'a Value, ResultDecodeError> {
    row.get(column)
        .ok_or_else(|| ResultDecodeError::MissingColumn(column.to_owned()))
}

fn batch_uuid(
    row: &serde_json::Map<String, Value>,
    column: &str,
) -> Result<Option<Uuid>, ResultDecodeError> {
    let value = batch_value(row, column)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .map(Some)
        .ok_or_else(|| ResultDecodeError::InvalidBatchColumn {
            column: column.to_owned(),
            expected: "a UUID string or null",
            value: value.to_string(),
        })
}

fn batch_boolean(
    row: &serde_json::Map<String, Value>,
    column: &str,
) -> Result<Option<bool>, ResultDecodeError> {
    let value = batch_value(row, column)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| ResultDecodeError::InvalidBatchColumn {
            column: column.to_owned(),
            expected: "a boolean or null",
            value: value.to_string(),
        })
}

fn batch_integer(
    row: &serde_json::Map<String, Value>,
    column: &str,
) -> Result<Option<i64>, ResultDecodeError> {
    let value = batch_value(row, column)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_i64()
        .map(Some)
        .ok_or_else(|| ResultDecodeError::InvalidBatchColumn {
            column: column.to_owned(),
            expected: "an integer or null",
            value: value.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::{Clause, MatchCteOptions, MatchQueryContext, plan_match_query};

    const APP_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    fn plan(clauses: Value) -> crate::MatchQueryPlan {
        let clauses: Vec<Clause> = serde_json::from_value(clauses).unwrap();
        let clauses = clauses
            .iter()
            .map(Clause::normalize)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        plan_match_query(
            &MatchQueryContext {
                prefix: "match-",
                app_id: uuid(APP_ID),
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &MatchCteOptions::default(),
            },
            &clauses,
        )
        .unwrap()
    }

    fn cells(
        prefix: &str,
        entity: Option<Uuid>,
        attr: Option<Uuid>,
        value: Option<Value>,
        reference: Option<bool>,
        created_at: Option<i64>,
    ) -> [(String, QueryCell); 5] {
        [
            (format!("{prefix}_entity_id"), QueryCell::Uuid(entity)),
            (format!("{prefix}_attr_id"), QueryCell::Uuid(attr)),
            (format!("{prefix}_value"), QueryCell::Json(value)),
            (
                format!("{prefix}_is_ref_val"),
                QueryCell::Boolean(reference),
            ),
            (
                format!("{prefix}_created_at"),
                QueryCell::Integer(created_at),
            ),
        ]
    }

    fn row(groups: impl IntoIterator<Item = [(String, QueryCell); 5]>) -> QueryRow {
        QueryRow {
            cells: groups.into_iter().flatten().collect(),
        }
    }

    #[test]
    fn decodes_linear_rows_symbols_and_bound_topics() {
        let entity = uuid("11111111-1111-1111-1111-111111111111");
        let year_attr = uuid("22222222-2222-2222-2222-222222222222");
        let title_attr = uuid("33333333-3333-3333-3333-333333333333");
        let plan = plan(json!([
            {
                "kind": "pattern",
                "pattern": {
                    "index": {"key": "ea"},
                    "components": [
                        {"kind": "variable", "value": "?e"},
                        {"kind": "constant", "value": year_attr},
                        {"kind": "constant", "value": 1987}
                    ]
                }
            },
            {
                "kind": "pattern",
                "pattern": {
                    "index": {"key": "ea"},
                    "components": [
                        {"kind": "variable", "value": "?e"},
                        {"kind": "constant", "value": title_attr},
                        {"kind": "variable", "value": "?title"}
                    ]
                }
            }
        ]));
        let rows = [row([
            cells(
                "match_0",
                Some(entity),
                Some(year_attr),
                Some(json!(1987)),
                Some(false),
                Some(10),
            ),
            cells(
                "match_1",
                Some(entity),
                Some(title_attr),
                Some(json!("RoboCop")),
                Some(false),
                Some(11),
            ),
        ])];

        let result = decode_query_rows(&rows, &plan.pattern_metadata).unwrap();
        assert_eq!(result.join_rows.len(), 1);
        assert_eq!(result.join_rows[0].len(), 2);
        assert_eq!(
            result.symbol_values[&Symbol::new("?e").unwrap()],
            vec![DatalogValue::Uuid(entity)]
        );
        assert_eq!(
            result.symbol_values[&Symbol::new("?title").unwrap()],
            vec![DatalogValue::Json(json!("RoboCop"))]
        );
        assert_eq!(result.topics[0].entity, TopicPart::Any);
        assert_eq!(
            result.topics[1].entity,
            TopicPart::Values(vec![DatalogValue::Uuid(entity)])
        );
    }

    #[test]
    fn missing_attributes_return_empty_symbols_and_broad_schema_topics() {
        let symbol = Symbol::new("?missing-0").unwrap();
        let clauses = vec![NormalizedClause::Pattern {
            pattern: crate::RawPattern::new(
                crate::Index::new(IndexKey::Ea),
                vec![crate::RawTerm::Variable(symbol.clone())],
            )
            .normalize()
            .unwrap(),
        }];
        let result = missing_attribute_result(&clauses);
        assert!(result.join_rows.is_empty());
        assert_eq!(result.symbol_values[&symbol], Vec::<DatalogValue>::new());
        assert_eq!(result.topics.len(), 2);
        assert_eq!(result.topics[0].indexes, vec![IndexKey::Ea]);
        assert_eq!(result.topics[1].indexes, vec![IndexKey::Eav]);
        assert!(result.topics.iter().all(|topic| {
            topic.entity == TopicPart::Any
                && topic.attribute == TopicPart::Any
                && topic.value == TopicPart::Any
        }));
    }

    #[test]
    fn or_branches_drop_null_triples_and_merge_symbol_values() {
        let first = uuid("11111111-1111-1111-1111-111111111111");
        let second = uuid("22222222-2222-2222-2222-222222222222");
        let first_attr = uuid("aaaaaaaa-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        let second_attr = uuid("cccccccc-cccc-cccc-cccc-cccccccccccc");
        let plan = plan(json!([{
            "kind": "or",
            "join_symbol": "?e",
            "clauses": [
                {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                    {"kind": "variable", "value": "?e"}, {"kind": "constant", "value": first_attr}
                ]}},
                {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                    {"kind": "variable", "value": "?e"}, {"kind": "constant", "value": second_attr}
                ]}}
            ]
        }]));
        let rows = [
            row([
                cells(
                    "match_0",
                    Some(first),
                    Some(first_attr),
                    Some(json!("one")),
                    Some(false),
                    Some(1),
                ),
                cells("match_1", None, None, None, None, None),
            ]),
            row([
                cells("match_0", None, None, None, None, None),
                cells(
                    "match_1",
                    Some(second),
                    Some(second_attr),
                    Some(json!("two")),
                    Some(false),
                    Some(2),
                ),
            ]),
        ];

        let result = decode_query_rows(&rows, &plan.pattern_metadata).unwrap();
        assert_eq!(
            result.join_rows.iter().map(Vec::len).collect::<Vec<_>>(),
            [1, 1]
        );
        assert_eq!(
            result.symbol_values[&Symbol::new("?e").unwrap()],
            vec![DatalogValue::Uuid(first), DatalogValue::Uuid(second)]
        );
    }

    #[test]
    fn reference_values_become_uuids_and_do_not_bind_later_topics() {
        let movie = uuid("11111111-1111-1111-1111-111111111111");
        let director = uuid("22222222-2222-2222-2222-222222222222");
        let director_attr = uuid("33333333-3333-3333-3333-333333333333");
        let name_attr = uuid("44444444-4444-4444-4444-444444444444");
        let plan = plan(json!([
            {"kind": "pattern", "pattern": {"index": {"key": "vae"}, "components": [
                {"kind": "variable", "value": "?movie"},
                {"kind": "constant", "value": director_attr},
                {"kind": "variable", "value": "?director"}
            ]}},
            {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                {"kind": "variable", "value": "?director"},
                {"kind": "constant", "value": name_attr},
                {"kind": "variable", "value": "?name"}
            ]}}
        ]));
        let rows = [row([
            cells(
                "match_0",
                Some(movie),
                Some(director_attr),
                Some(json!(director)),
                Some(true),
                Some(1),
            ),
            cells(
                "match_1",
                Some(director),
                Some(name_attr),
                Some(json!("Jane")),
                Some(false),
                Some(2),
            ),
        ])];

        let result = decode_query_rows(&rows, &plan.pattern_metadata).unwrap();
        assert_eq!(
            result.symbol_values[&Symbol::new("?director").unwrap()],
            vec![DatalogValue::Uuid(director)]
        );
        assert_eq!(result.topics[1].entity, TopicPart::Any);
    }

    #[test]
    fn reference_constants_remain_typed_uuids_in_precise_topics() {
        let entity = uuid("11111111-1111-1111-1111-111111111111");
        let relationship = uuid("22222222-2222-2222-2222-222222222222");
        let target = uuid("33333333-3333-3333-3333-333333333333");
        for index in ["vae", "eav"] {
            let plan = plan(json!([{
                "kind": "pattern",
                "pattern": {"index": {"key": index}, "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": relationship},
                    {"kind": "constant", "value": target}
                ]}
            }]));
            let rows = [row([cells(
                "match_0",
                Some(entity),
                Some(relationship),
                Some(json!(target)),
                Some(true),
                Some(1),
            )])];

            let result = decode_query_rows(&rows, &plan.pattern_metadata).unwrap();
            assert_eq!(
                result.topics[0].value,
                TopicPart::Values(vec![DatalogValue::Uuid(target)]),
                "{index} reference constants must intersect typed WAL references"
            );
        }
    }

    #[test]
    fn reference_not_topics_are_typed_without_retyping_json_predicates() {
        let relationship = uuid("22222222-2222-2222-2222-222222222222");
        let excluded = uuid("33333333-3333-3333-3333-333333333333");
        for index in ["vae", "eav"] {
            let plan = plan(json!([{
                "kind": "pattern",
                "pattern": {"index": {"key": index}, "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": relationship},
                    {"kind": "function", "value": {
                        "function": "not",
                        "argument": excluded
                    }}
                ]}
            }]));
            let result = decode_query_rows(&[], &plan.pattern_metadata).unwrap();
            assert_eq!(
                result.topics[0].value,
                TopicPart::Not(DatalogValue::Uuid(excluded)),
                "{index} reference exclusions must retain UUID semantics"
            );
        }

        let entity_not = plan(json!([{
            "kind": "pattern",
            "pattern": {"index": {"key": "vae"}, "components": [
                {"kind": "function", "value": {
                    "function": "not",
                    "argument": excluded
                }},
                {"kind": "constant", "value": relationship},
                {"kind": "any"}
            ]}
        }]));
        let result = decode_query_rows(&[], &entity_not.pattern_metadata).unwrap();
        assert_eq!(
            result.topics[0].entity,
            TopicPart::Not(DatalogValue::Uuid(excluded))
        );

        let json_not = plan(json!([{
            "kind": "pattern",
            "pattern": {"index": {"key": "ea"}, "components": [
                {"kind": "variable", "value": "?entity"},
                {"kind": "constant", "value": relationship},
                {"kind": "function", "value": {
                    "function": "not",
                    "argument": excluded
                }}
            ]}
        }]));
        let result = decode_query_rows(&[], &json_not.pattern_metadata).unwrap();
        assert_eq!(
            result.topics[0].value,
            TopicPart::Values(vec![DatalogValue::Json(json!({
                "function": "not",
                "argument": excluded
            }))]),
            "a UUID-shaped blob remains a JSON `$not` outside reference indexes"
        );
    }

    #[test]
    fn optional_empty_topic_symbols_prevent_wildcards_but_skip_reference_values() {
        let first_attr = uuid("11111111-1111-1111-1111-111111111111");
        let second_attr = uuid("22222222-2222-2222-2222-222222222222");
        let empty_plan = plan(json!([
            {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                {"kind": "variable", "value": "?entity"},
                {"kind": "constant", "value": first_attr},
                {"kind": "any"}
            ]}},
            {"kind": "pattern", "pattern": {"index": {"key": "eav"}, "components": [
                {"kind": "variable", "value": "?entity"},
                {"kind": "constant", "value": second_attr},
                {"kind": "any"}
            ]}}
        ]));
        let default = decode_query_rows(&[], &empty_plan.pattern_metadata).unwrap();
        assert_eq!(default.topics[1].entity, TopicPart::Any);
        let guarded = decode_query_rows_with_options(
            &[],
            &empty_plan.pattern_metadata,
            ResultDecodeOptions {
                ensure_empty_symbol_values_for_topics: true,
            },
        )
        .unwrap();
        assert_eq!(guarded.topics[0].entity, TopicPart::Any);
        assert_eq!(guarded.topics[1].entity, TopicPart::Values(Vec::new()));

        let reference_plan = plan(json!([
            {"kind": "pattern", "pattern": {"index": {"key": "vae"}, "components": [
                {"kind": "any"},
                {"kind": "constant", "value": first_attr},
                {"kind": "variable", "value": "?reference"}
            ]}},
            {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                {"kind": "variable", "value": "?reference"},
                {"kind": "constant", "value": second_attr},
                {"kind": "any"}
            ]}}
        ]));
        let reference = decode_query_rows_with_options(
            &[],
            &reference_plan.pattern_metadata,
            ResultDecodeOptions {
                ensure_empty_symbol_values_for_topics: true,
            },
        )
        .unwrap();
        assert_eq!(reference.topics[1].entity, TopicPart::Any);
    }

    #[test]
    fn decodes_batched_row_json_and_preserves_json_null() {
        let entity = uuid("11111111-1111-1111-1111-111111111111");
        let attr = uuid("22222222-2222-2222-2222-222222222222");
        let plan = plan(json!([{
            "kind": "pattern",
            "pattern": {
                "index": {"key": "ea"},
                "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": attr},
                    {"kind": "variable", "value": "?value"}
                ]
            }
        }]));
        let rows = json!([{
            "match_0_entity_id": entity,
            "match_0_attr_id": attr,
            "match_0_value": null,
            "match_0_is_ref_val": false,
            "match_0_created_at": 99
        }]);

        let result = decode_json_query_rows(&rows, &plan.pattern_metadata).unwrap();
        assert_eq!(
            result.join_rows[0][0].value,
            DatalogValue::Json(Value::Null)
        );
        assert_eq!(
            result.symbol_values[&Symbol::new("?entity").unwrap()],
            vec![DatalogValue::Uuid(entity)]
        );
    }

    #[test]
    fn direct_and_json_envelopes_have_identical_datalog_results() {
        // Keep the two PostgreSQL projection paths locked together. The
        // production executor uses QueryCell rows for a direct query and
        // row_to_json objects for a batch; either path must preserve typed
        // references, JSON nulls, symbol bindings, and topic values exactly.
        let entity = uuid("11111111-1111-1111-1111-111111111111");
        let attr = uuid("22222222-2222-2222-2222-222222222222");
        let reference_attr = uuid("33333333-3333-3333-3333-333333333333");
        let referenced = uuid("44444444-4444-4444-4444-444444444444");
        let plan = plan(json!([
            {
                "kind": "pattern",
                "pattern": {
                    "index": {"key": "ea"},
                    "components": [
                        {"kind": "variable", "value": "?entity"},
                        {"kind": "constant", "value": attr},
                        {"kind": "variable", "value": "?value"}
                    ]
                }
            },
            {
                "kind": "pattern",
                "pattern": {
                    "index": {"key": "ea"},
                    "components": [
                        {"kind": "variable", "value": "?entity"},
                        {"kind": "constant", "value": reference_attr},
                        {"kind": "variable", "value": "?ref"}
                    ]
                }
            }
        ]));
        let direct = vec![row([
            cells(
                "match_0",
                Some(entity),
                Some(attr),
                Some(Value::Null),
                Some(false),
                Some(7),
            ),
            cells(
                "match_1",
                Some(entity),
                Some(reference_attr),
                Some(json!(referenced.to_string())),
                Some(true),
                Some(8),
            ),
        ])];
        let json_rows = json!([{
            "match_0_entity_id": entity,
            "match_0_attr_id": attr,
            "match_0_value": null,
            "match_0_is_ref_val": false,
            "match_0_created_at": 7,
            "match_1_entity_id": entity,
            "match_1_attr_id": reference_attr,
            "match_1_value": referenced.to_string(),
            "match_1_is_ref_val": true,
            "match_1_created_at": 8
        }]);

        let direct_result = decode_query_rows(&direct, &plan.pattern_metadata).unwrap();
        let json_result = decode_json_query_rows(&json_rows, &plan.pattern_metadata).unwrap();
        assert_eq!(direct_result, json_result);
        assert_eq!(
            direct_result.symbol_values[&Symbol::new("?ref").unwrap()],
            vec![DatalogValue::Uuid(referenced)]
        );
        assert_eq!(
            direct_result.join_rows[0][0].value,
            DatalogValue::Json(Value::Null)
        );
    }

    #[test]
    fn direct_and_json_envelopes_match_for_nested_or_branches() {
        // Batch rows flatten every match CTE, while direct rows retain typed cells.
        // Keep recursive metadata decoding identical across those two projections.
        let first = uuid("11111111-1111-1111-1111-111111111111");
        let second = uuid("22222222-2222-2222-2222-222222222222");
        let third = uuid("33333333-3333-3333-3333-333333333333");
        let first_attr = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let second_attr = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        let third_attr = uuid("cccccccc-cccc-cccc-cccc-cccccccccccc");
        let plan = plan(json!([{
            "kind": "or",
            "join_symbol": "?entity",
            "clauses": [
                {
                    "kind": "or",
                    "join_symbol": "?entity",
                    "clauses": [
                        {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                            {"kind": "variable", "value": "?entity"}, {"kind": "constant", "value": first_attr}
                        ]}},
                        {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                            {"kind": "variable", "value": "?entity"}, {"kind": "constant", "value": second_attr}
                        ]}}
                    ]
                },
                {"kind": "pattern", "pattern": {"index": {"key": "ea"}, "components": [
                    {"kind": "variable", "value": "?entity"}, {"kind": "constant", "value": third_attr}
                ]}}
            ]
        }]));
        let direct = vec![
            row([
                cells(
                    "match_0",
                    Some(first),
                    Some(first_attr),
                    Some(json!("first")),
                    Some(false),
                    Some(1),
                ),
                cells("match_1", None, None, None, None, None),
                cells("match_3", None, None, None, None, None),
            ]),
            row([
                cells("match_0", None, None, None, None, None),
                cells(
                    "match_1",
                    Some(second),
                    Some(second_attr),
                    Some(json!("second")),
                    Some(false),
                    Some(2),
                ),
                cells("match_3", None, None, None, None, None),
            ]),
            row([
                cells("match_0", None, None, None, None, None),
                cells("match_1", None, None, None, None, None),
                cells(
                    "match_3",
                    Some(third),
                    Some(third_attr),
                    Some(json!("third")),
                    Some(false),
                    Some(3),
                ),
            ]),
        ];
        let json_rows = json!([
            {
                "match_0_entity_id": first, "match_0_attr_id": first_attr, "match_0_value": "first", "match_0_is_ref_val": false, "match_0_created_at": 1,
                "match_1_entity_id": null, "match_1_attr_id": null, "match_1_value": null, "match_1_is_ref_val": null, "match_1_created_at": null,
                "match_3_entity_id": null, "match_3_attr_id": null, "match_3_value": null, "match_3_is_ref_val": null, "match_3_created_at": null
            },
            {
                "match_0_entity_id": null, "match_0_attr_id": null, "match_0_value": null, "match_0_is_ref_val": null, "match_0_created_at": null,
                "match_1_entity_id": second, "match_1_attr_id": second_attr, "match_1_value": "second", "match_1_is_ref_val": false, "match_1_created_at": 2,
                "match_3_entity_id": null, "match_3_attr_id": null, "match_3_value": null, "match_3_is_ref_val": null, "match_3_created_at": null
            },
            {
                "match_0_entity_id": null, "match_0_attr_id": null, "match_0_value": null, "match_0_is_ref_val": null, "match_0_created_at": null,
                "match_1_entity_id": null, "match_1_attr_id": null, "match_1_value": null, "match_1_is_ref_val": null, "match_1_created_at": null,
                "match_3_entity_id": third, "match_3_attr_id": third_attr, "match_3_value": "third", "match_3_is_ref_val": false, "match_3_created_at": 3
            }
        ]);

        let direct_result = decode_query_rows(&direct, &plan.pattern_metadata).unwrap();
        let json_result = decode_json_query_rows(&json_rows, &plan.pattern_metadata).unwrap();
        assert_eq!(direct_result, json_result);
        assert_eq!(
            direct_result.symbol_values[&Symbol::new("?entity").unwrap()],
            vec![
                DatalogValue::Uuid(first),
                DatalogValue::Uuid(second),
                DatalogValue::Uuid(third),
            ]
        );
    }

    #[test]
    fn nested_uuid_scoping_filters_rows_and_binds_topic_metadata() {
        let first = uuid("11111111-1111-1111-1111-111111111111");
        let second = uuid("22222222-2222-2222-2222-222222222222");
        let attr = uuid("33333333-3333-3333-3333-333333333333");
        let plan = plan(json!([{
            "kind": "pattern",
            "pattern": {
                "index": {"key": "ea"},
                "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": attr}
                ]
            }
        }]));
        let rows = json!([
            {
                "match_0_entity_id": first,
                "match_0_attr_id": attr,
                "match_0_value": "first",
                "match_0_is_ref_val": false,
                "match_0_created_at": 1
            },
            {
                "match_0_entity_id": second,
                "match_0_attr_id": attr,
                "match_0_value": "second",
                "match_0_is_ref_val": false,
                "match_0_created_at": 2
            }
        ]);
        let symbol = Symbol::new("?entity").unwrap();
        let filtered =
            filter_json_query_rows_by_uuid_symbol(&rows, &plan.pattern_metadata, &symbol, first)
                .unwrap();
        let metadata = bind_pattern_metadata_uuid_symbol(&plan.pattern_metadata, &symbol, first);
        let result = decode_json_query_rows(&filtered, &metadata).unwrap();

        assert_eq!(result.join_rows.len(), 1);
        assert_eq!(result.join_rows[0][0].entity_id, first);
        assert!(!result.symbol_values.contains_key(&symbol));
        assert_eq!(
            result.topics[0].entity,
            TopicPart::Values(vec![DatalogValue::Uuid(first)])
        );
    }

    #[test]
    fn nested_uuid_scoping_handles_reference_symbols_and_rejects_json_values() {
        let entity = uuid("11111111-1111-1111-1111-111111111111");
        let parent = uuid("22222222-2222-2222-2222-222222222222");
        let attr = uuid("33333333-3333-3333-3333-333333333333");
        let plan = plan(json!([{
            "kind": "pattern",
            "pattern": {
                "index": {"key": "vae"},
                "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": attr},
                    {"kind": "variable", "value": "?parent"}
                ]
            }
        }]));
        let rows = json!([{
            "match_0_entity_id": entity,
            "match_0_attr_id": attr,
            "match_0_value": parent,
            "match_0_is_ref_val": true,
            "match_0_created_at": 1
        }]);
        let symbol = Symbol::new("?parent").unwrap();
        assert_eq!(
            filter_json_query_rows_by_uuid_symbol(&rows, &plan.pattern_metadata, &symbol, parent)
                .unwrap(),
            rows
        );
        let bound = bind_pattern_metadata_uuid_symbol(&plan.pattern_metadata, &symbol, parent);
        let legacy = decode_json_query_rows(&rows, &bound).unwrap();
        assert_eq!(
            UuidGroupedJsonRows::new(&rows, &plan.pattern_metadata, &symbol)
                .unwrap()
                .decode_with_options(parent, &bound, ResultDecodeOptions::default())
                .unwrap(),
            legacy
        );

        let json_rows = json!([{
            "match_0_entity_id": entity,
            "match_0_attr_id": attr,
            "match_0_value": parent,
            "match_0_is_ref_val": false,
            "match_0_created_at": 1
        }]);
        assert!(matches!(
            filter_json_query_rows_by_uuid_symbol(
                &json_rows,
                &plan.pattern_metadata,
                &symbol,
                parent
            ),
            Err(ResultDecodeError::InvalidNestedJoinValue { .. })
        ));
        assert!(matches!(
            UuidGroupedJsonRows::new(&json_rows, &plan.pattern_metadata, &symbol),
            Err(ResultDecodeError::InvalidNestedJoinValue { .. })
        ));
    }

    #[test]
    fn decodes_and_reverses_page_cursors_with_empty_page_fallback() {
        let first = uuid("11111111-1111-1111-1111-111111111111");
        let second = uuid("22222222-2222-2222-2222-222222222222");
        let attr = uuid("33333333-3333-3333-3333-333333333333");
        let plan = plan(json!([{
            "kind": "pattern",
            "pattern": {
                "index": {"key": "ea"},
                "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": attr}
                ]
            }
        }]));
        let page = PagePlanMetadata {
            table: "match-0".to_owned(),
            has_next_table: "match-0-has-next".to_owned(),
            has_previous_table: "match-0-has-prev".to_owned(),
            cursor_columns: plan
                .pattern_metadata
                .first()
                .and_then(|node| match node {
                    PatternMetadataNode::Pattern(metadata) => Some(metadata.cte_columns.clone()),
                    PatternMetadataNode::Or(_) => None,
                })
                .unwrap(),
            fallback_entity_column: "order_eid".to_owned(),
            fallback_created_at_column: "fallback_created_at".to_owned(),
            attr_id: attr,
            reverse_results: true,
            empty_page_has_previous: true,
        };
        let rows = [
            row([cells(
                "match_0",
                Some(second),
                Some(attr),
                Some(json!("second")),
                Some(false),
                Some(2),
            )]),
            row([cells(
                "match_0",
                Some(first),
                Some(attr),
                Some(json!("first")),
                Some(false),
                Some(1),
            )])
            .with_cell("order_eid", QueryCell::Uuid(Some(first)))
            .with_cell("fallback_created_at", QueryCell::Integer(Some(1))),
        ];

        let result =
            decode_paged_query_rows(&rows, &plan.pattern_metadata, &page, false, true).unwrap();
        let info = result.page_info.unwrap();
        assert_eq!(info.start_cursor.unwrap().entity_id, first);
        assert_eq!(info.end_cursor.unwrap().entity_id, second);
        assert!(info.has_previous_page);

        let empty = decode_paged_query_rows(&[], &plan.pattern_metadata, &page, false, false)
            .unwrap()
            .page_info
            .unwrap();
        assert!(empty.has_previous_page);
    }

    #[test]
    fn reconstructs_missing_order_value_cursors_from_page_fallback_columns() {
        let entity = uuid("11111111-1111-1111-1111-111111111111");
        let attr = uuid("33333333-3333-3333-3333-333333333333");
        let plan = plan(json!([{
            "kind": "pattern",
            "pattern": {
                "index": {"key": "ea"},
                "components": [
                    {"kind": "variable", "value": "?entity"},
                    {"kind": "constant", "value": attr}
                ]
            }
        }]));
        let cursor_columns = match &plan.pattern_metadata[0] {
            PatternMetadataNode::Pattern(metadata) => metadata.cte_columns.clone(),
            PatternMetadataNode::Or(_) => panic!("expected pattern metadata"),
        };
        let page = PagePlanMetadata {
            table: "match-0".to_owned(),
            has_next_table: "match-0-has-next".to_owned(),
            has_previous_table: "match-0-has-prev".to_owned(),
            cursor_columns,
            fallback_entity_column: "order_eid".to_owned(),
            fallback_created_at_column: "fallback_created_at".to_owned(),
            attr_id: attr,
            reverse_results: false,
            empty_page_has_previous: false,
        };
        let missing = row([cells("match_0", None, None, None, None, None)])
            .with_cell("order_eid", QueryCell::Uuid(Some(entity)))
            .with_cell("fallback_created_at", QueryCell::Integer(Some(42)));

        let direct =
            decode_paged_query_rows(&[missing], &plan.pattern_metadata, &page, false, false)
                .unwrap();
        let cursor = direct.page_info.unwrap().start_cursor.unwrap();
        assert_eq!(cursor.entity_id, entity);
        assert_eq!(cursor.attr_id, attr);
        assert_eq!(cursor.value, None);
        assert_eq!(cursor.created_at, 42);

        let json_result = decode_json_paged_query_rows(
            &json!([{
                "match_0_entity_id": null,
                "match_0_attr_id": null,
                "match_0_value": null,
                "match_0_is_ref_val": null,
                "match_0_created_at": null,
                "order_eid": entity,
                "fallback_created_at": 42
            }]),
            &plan.pattern_metadata,
            &page,
            false,
            false,
        )
        .unwrap();
        assert_eq!(json_result.page_info.unwrap().end_cursor.unwrap(), cursor);
    }

    fn json_decode_fixture(
        pattern_count: usize,
        row_count: usize,
        or: bool,
    ) -> (Vec<PatternMetadataNode>, Value) {
        let clauses: Vec<_> = (0..pattern_count)
            .map(|index| {
                json!({
                    "kind":"pattern", "pattern":{"index":{"key":"ea"},"components":[
                        {"kind":"variable","value":"?entity"},
                        {"kind":"constant","value":Uuid::from_u128(100 + index as u128)},
                        {"kind":"variable","value":format!("?value-{index}")}
                    ]}
                })
            })
            .collect();
        let clauses = if or {
            json!([{"kind":"or","join_symbol":"?entity","clauses":clauses}])
        } else {
            json!(clauses)
        };
        let metadata = plan(clauses).pattern_metadata;
        let patterns = flattened_pattern_metadata(&metadata);
        let rows: Vec<_> = (0..row_count)
            .map(|index| {
                let mut row = serde_json::Map::new();
                for (pattern_index, pattern) in patterns.iter().enumerate() {
                    let absent = or && index % patterns.len() != pattern_index;
                    let reference = (index + pattern_index) % 3 == 1;
                    let value = if reference {
                        json!(Uuid::from_u128(999))
                    } else {
                        match index % 4 {
                            0 => Value::Null,
                            1 => serde_json::from_str("184467440737095516170").unwrap(),
                            2 => json!({"nested":[false, null, "synthetic payload"]}),
                            _ => json!("row value"),
                        }
                    };
                    let values = if absent {
                        vec![Value::Null; 5]
                    } else {
                        vec![
                            json!(Uuid::from_u128(1 + index as u128)),
                            json!(Uuid::from_u128(100 + pattern_index as u128)),
                            value,
                            json!(reference),
                            json!(index as i64),
                        ]
                    };
                    row.extend(pattern.cte_columns.iter().cloned().zip(values));
                }
                row.insert("unprojected".into(), json!({"kept":"by nested scoping"}));
                Value::Object(row)
            })
            .collect();
        (metadata, Value::Array(rows))
    }

    fn assert_json_decode_parity(rows: &Value, metadata: &[PatternMetadataNode]) {
        let text = serde_json::to_string(rows).unwrap();
        for ensure_empty_symbol_values_for_topics in [false, true] {
            let options = ResultDecodeOptions {
                ensure_empty_symbol_values_for_topics,
            };
            let expected = json_query_rows(rows, metadata)
                .and_then(|rows| decode_query_rows_with_options(&rows, metadata, options));
            assert_eq!(
                decode_json_query_rows_with_options(rows, metadata, options),
                expected
            );
            assert_eq!(
                decode_raw_json_query_rows_with_options(&text, metadata, options),
                expected
            );
        }
        let expected = rows
            .as_array()
            .ok_or(ResultDecodeError::InvalidBatchRows)
            .and_then(|raw| {
                let decoded = json_query_rows_from_slice(raw, metadata)?;
                filter_result_rows(
                    raw,
                    &decoded,
                    metadata,
                    &Symbol::new("?entity").unwrap(),
                    Uuid::from_u128(1),
                )
            });
        assert_eq!(
            filter_json_query_rows_by_uuid_symbol(
                rows,
                metadata,
                &Symbol::new("?entity").unwrap(),
                Uuid::from_u128(1)
            ),
            expected
        );
        let symbol = Symbol::new("?entity").unwrap();
        for parent in [
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(9000),
        ] {
            let bound = bind_pattern_metadata_uuid_symbol(metadata, &symbol, parent);
            for ensure_empty_symbol_values_for_topics in [false, true] {
                let options = ResultDecodeOptions {
                    ensure_empty_symbol_values_for_topics,
                };
                let legacy = filter_json_query_rows_by_uuid_symbol(rows, metadata, &symbol, parent)
                    .and_then(|rows| decode_json_query_rows_with_options(&rows, &bound, options));
                let indexed = UuidGroupedJsonRows::new(rows, metadata, &symbol)
                    .and_then(|rows| rows.decode_with_options(parent, &bound, options));
                assert_eq!(indexed, legacy);
                assert_eq!(
                    UuidGroupedRawJsonRows::new(
                        &serde_json::to_string(rows).unwrap(),
                        metadata,
                        &symbol
                    )
                    .and_then(|rows| rows.decode_with_options(parent, &bound, options)),
                    legacy
                );
            }
        }
    }

    #[test]
    fn borrowed_json_decoding_matches_eager_results_topics_nested_scoping_and_errors() {
        for or in [false, true] {
            let (metadata, rows) = json_decode_fixture(3, 4, or);
            assert!(direct_json_columns(&metadata, None));
            let borrowed = borrowed_json_rows(&rows).unwrap();
            assert!(
                decode_result_rows(&borrowed, &metadata, ResultDecodeOptions::default()).is_ok()
            );
            assert_json_decode_parity(&rows, &metadata);
            assert_json_decode_parity(&json!([]), &metadata);
            assert_json_decode_parity(&json!([null]), &metadata);
            assert_json_decode_parity(&json!({}), &metadata);
            // Every field, including absent OR-branch scalars, must be checked.
            for row_index in 0..4 {
                for pattern in flattened_pattern_metadata(&metadata) {
                    for column in &pattern.cte_columns {
                        let mut missing = rows.clone();
                        missing[row_index].as_object_mut().unwrap().remove(column);
                        assert_json_decode_parity(&missing, &metadata);
                        for replacement in [
                            json!("invalid-uuid-or-scalar"),
                            Value::Null,
                            json!({}),
                            json!(true),
                        ] {
                            let mut changed = rows.clone();
                            changed[row_index][column] = replacement;
                            assert_json_decode_parity(&changed, &metadata);
                        }
                    }
                }
            }
            // Multiple errors must retain the eager decoder's exact priority.
            let mut corrupt = rows.clone();
            let patterns = flattened_pattern_metadata(&metadata);
            corrupt[0][&patterns[0].cte_columns[2]] = json!("invalid-reference");
            corrupt[0][&patterns[0].cte_columns[3]] = json!(true);
            corrupt[3]
                .as_object_mut()
                .unwrap()
                .remove(&patterns[2].cte_columns[4]);
            assert_json_decode_parity(&corrupt, &metadata);
            let mut aliased = metadata.clone();
            aliased.extend(metadata.clone());
            assert!(!direct_json_columns(&aliased, None));
            assert_json_decode_parity(&rows, &aliased);
        }
    }

    fn eager_json_page(
        rows: &Value,
        metadata: &[PatternMetadataNode],
        page: &PagePlanMetadata,
    ) -> Result<DatalogResult, ResultDecodeError> {
        let mut decoded = json_query_rows(rows, metadata)?;
        for (raw, row) in rows.as_array().unwrap().iter().zip(&mut decoded) {
            let raw = raw.as_object().unwrap();
            row.cells.insert(
                page.fallback_entity_column.clone(),
                QueryCell::Uuid(batch_uuid(raw, &page.fallback_entity_column)?),
            );
            row.cells.insert(
                page.fallback_created_at_column.clone(),
                QueryCell::Integer(batch_integer(raw, &page.fallback_created_at_column)?),
            );
        }
        decode_paged_query_rows(&decoded, metadata, page, true, false)
    }

    fn assert_raw_page_parity(
        rows: &Value,
        metadata: &[PatternMetadataNode],
        page: &PagePlanMetadata,
    ) {
        assert_eq!(
            decode_raw_json_paged_query_rows_with_options(
                &serde_json::to_string(rows).unwrap(),
                metadata,
                page,
                true,
                false,
                ResultDecodeOptions::default(),
            ),
            eager_json_page(rows, metadata, page)
        );
    }

    fn assert_grouped_page_parity(
        rows: &Value,
        metadata: &[PatternMetadataNode],
        page: &PagePlanMetadata,
    ) {
        let symbol = Symbol::new("?entity").unwrap();
        for parent in [
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(9000),
        ] {
            let bound = bind_pattern_metadata_uuid_symbol(metadata, &symbol, parent);
            for has_next in [false, true] {
                for has_previous in [false, true] {
                    let options = ResultDecodeOptions::default();
                    let legacy =
                        filter_json_query_rows_by_uuid_symbol(rows, metadata, &symbol, parent)
                            .and_then(|rows| {
                                decode_json_paged_query_rows_with_options(
                                    &rows,
                                    &bound,
                                    page,
                                    has_next,
                                    has_previous,
                                    options,
                                )
                            });
                    let indexed =
                        UuidGroupedJsonRows::new(rows, metadata, &symbol).and_then(|rows| {
                            rows.decode_paged_with_options(
                                parent,
                                &bound,
                                page,
                                has_next,
                                has_previous,
                                options,
                            )
                        });
                    assert_eq!(indexed, legacy);
                    let raw = UuidGroupedRawJsonRows::new(
                        &serde_json::to_string(rows).unwrap(),
                        metadata,
                        &symbol,
                    )
                    .and_then(|rows| {
                        rows.decode_paged_with_options(
                            parent,
                            &bound,
                            page,
                            has_next,
                            has_previous,
                            options,
                        )
                    });
                    assert_eq!(raw, legacy);
                }
            }
        }
    }

    #[test]
    fn borrowed_json_pages_preserve_fallbacks_reversal_flags_and_malformed_columns() {
        let (metadata, mut rows) = json_decode_fixture(1, 4, false);
        let columns = flattened_pattern_metadata(&metadata)[0].cte_columns.clone();
        let mut page = PagePlanMetadata {
            table: "page".into(),
            has_next_table: "next".into(),
            has_previous_table: "previous".into(),
            cursor_columns: columns.clone(),
            fallback_entity_column: "fallback_entity".into(),
            fallback_created_at_column: "fallback_created".into(),
            attr_id: Uuid::from_u128(100),
            reverse_results: false,
            empty_page_has_previous: true,
        };
        for (index, row) in rows.as_array_mut().unwrap().iter_mut().enumerate() {
            row[&page.fallback_entity_column] = json!(Uuid::from_u128(1 + index as u128));
            row[&page.fallback_created_at_column] = json!(index as i64);
        }
        for column in &columns {
            rows[1][column] = Value::Null;
        }
        assert!(direct_json_columns(&metadata, Some(&page)));
        for reverse in [false, true] {
            page.reverse_results = reverse;
            for raw in [&rows, &json!([])] {
                assert_eq!(
                    decode_json_paged_query_rows(raw, &metadata, &page, true, false),
                    eager_json_page(raw, &metadata, &page)
                );
                assert_raw_page_parity(raw, &metadata, &page);
                assert_grouped_page_parity(raw, &metadata, &page);
            }
            for column in columns.iter().chain([
                &page.fallback_entity_column,
                &page.fallback_created_at_column,
            ]) {
                let mut invalid = rows.clone();
                invalid[0].as_object_mut().unwrap().remove(column);
                assert_eq!(
                    decode_json_paged_query_rows(&invalid, &metadata, &page, true, false),
                    eager_json_page(&invalid, &metadata, &page)
                );
                assert_raw_page_parity(&invalid, &metadata, &page);
                assert_grouped_page_parity(&invalid, &metadata, &page);
                invalid[0][column] = json!("invalid scalar");
                assert_eq!(
                    decode_json_paged_query_rows(&invalid, &metadata, &page, true, false),
                    eager_json_page(&invalid, &metadata, &page)
                );
                assert_raw_page_parity(&invalid, &metadata, &page);
                assert_grouped_page_parity(&invalid, &metadata, &page);
            }
        }
        page.fallback_entity_column = columns[0].clone();
        assert!(!direct_json_columns(&metadata, Some(&page)));
        assert_eq!(
            decode_json_paged_query_rows(&rows, &metadata, &page, true, false),
            eager_json_page(&rows, &metadata, &page)
        );
        assert_raw_page_parity(&rows, &metadata, &page);
        assert_grouped_page_parity(&rows, &metadata, &page);
    }

    #[test]
    fn grouped_rows_share_payloads_but_never_reuse_a_previous_response() {
        let (metadata, mut rows) = json_decode_fixture(2, 4, true);
        let symbol = Symbol::new("?entity").unwrap();
        let first = Uuid::from_u128(1);
        let bound = bind_pattern_metadata_uuid_symbol(&metadata, &symbol, first);
        let result = UuidGroupedJsonRows::new(&rows, &metadata, &symbol)
            .unwrap()
            .decode_with_options(first, &bound, ResultDecodeOptions::default())
            .unwrap();
        assert!(!result.join_rows.is_empty());
        for pattern in flattened_pattern_metadata(&metadata) {
            rows[0][&pattern.cte_columns[0]] = Value::Null;
        }
        let next = UuidGroupedJsonRows::new(&rows, &metadata, &symbol)
            .unwrap()
            .decode_with_options(first, &bound, ResultDecodeOptions::default())
            .unwrap();
        assert!(next.join_rows.is_empty());
    }

    #[test]
    fn grouped_uuid_rows_keep_multiple_bindings_and_one_row_per_parent() {
        let (mut metadata, mut rows) = json_decode_fixture(1, 3, false);
        let symbol = Symbol::new("?entity").unwrap();
        let PatternMetadataNode::Pattern(pattern) = &mut metadata[0] else {
            unreachable!()
        };
        pattern.symbol_fields.insert(
            2,
            crate::SymbolField {
                symbol: symbol.clone(),
                reference_value: true,
            },
        );
        for (index, row) in rows.as_array_mut().unwrap().iter_mut().enumerate() {
            row[&pattern.cte_columns[0]] = json!(Uuid::from_u128(if index < 2 { 1 } else { 3 }));
            row[&pattern.cte_columns[2]] = json!(Uuid::from_u128(if index == 0 { 2 } else { 1 }));
            row[&pattern.cte_columns[3]] = json!(true);
        }
        let grouped = UuidGroupedJsonRows::new(&rows, &metadata, &symbol).unwrap();
        for parent in [1, 2, 3, 999].map(Uuid::from_u128) {
            let bound = bind_pattern_metadata_uuid_symbol(&metadata, &symbol, parent);
            let filtered =
                filter_json_query_rows_by_uuid_symbol(&rows, &metadata, &symbol, parent).unwrap();
            assert_eq!(
                grouped
                    .decode_with_options(parent, &bound, ResultDecodeOptions::default())
                    .unwrap(),
                decode_json_query_rows(&filtered, &bound).unwrap()
            );
        }
    }

    /// Compares raw-text decoding with the `Value` decoder for one table text.
    fn assert_raw_text_parity(text: &str, metadata: &[PatternMetadataNode]) {
        for ensure_empty_symbol_values_for_topics in [false, true] {
            let options = ResultDecodeOptions {
                ensure_empty_symbol_values_for_topics,
            };
            let expected = serde_json::from_str::<Value>(text)
                .map_err(|_| ResultDecodeError::InvalidBatchRows)
                .and_then(|rows| decode_json_query_rows_with_options(&rows, metadata, options));
            assert_eq!(
                decode_raw_json_query_rows_with_options(text, metadata, options),
                expected,
                "{text}"
            );
        }
    }

    fn row_text(fields: &[(String, String)]) -> String {
        let fields = fields
            .iter()
            .map(|(key, value)| format!("{}:{value}", serde_json::to_string(key).unwrap()))
            .collect::<Vec<_>>()
            .join(",");
        format!("[{{{fields}}}]")
    }

    #[test]
    fn raw_json_text_decoding_matches_value_decoding_for_adversarial_rows() {
        let (metadata, _) = json_decode_fixture(2, 3, true);
        let pattern = flattened_pattern_metadata(&metadata)[0];
        let [entity, attr, value, reference, created] =
            pattern.cte_columns.clone().try_into().unwrap();
        let id = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);
        let base = |entity_value: &str| {
            vec![
                (entity.clone(), entity_value.to_owned()),
                (attr.clone(), format!("\"{other}\"")),
                (
                    value.clone(),
                    format!(
                        r#"{{"nested":[[1,null,"é"],{{"deep":{}}}]}}"#,
                        "[".repeat(20) + &"]".repeat(20)
                    ),
                ),
                (reference.clone(), "false".to_owned()),
                (created.clone(), "184467440737095516170".to_owned()),
            ]
        };
        // Well-formed rows, JSON null scalars, big integers, unicode values.
        assert_raw_text_parity(&row_text(&base(&format!("\"{id}\""))), &metadata);
        assert_raw_text_parity(&row_text(&base("null")), &metadata);
        assert_raw_text_parity("[]", &metadata);
        assert_raw_text_parity("[{}]", &metadata);
        assert_raw_text_parity(
            &row_text(&[
                (entity.clone(), "null".into()),
                (attr.clone(), "null".into()),
                (value.clone(), "null".into()),
                (reference.clone(), "null".into()),
                (created.clone(), "null".into()),
            ]),
            &metadata,
        );
        // Escaped keys decode to the same column names as unescaped ones.
        let escaped = row_text(&base(&format!("\"{id}\""))).replace(
            &format!("\"{entity}\""),
            &format!("\"{}\"", entity.replace('_', "\\u005f")),
        );
        assert_raw_text_parity(&escaped, &metadata);
        // Duplicate keys keep their last occurrence, like `serde_json::Map`.
        let mut duplicate = base(&format!("\"{id}\""));
        duplicate.insert(0, (entity.clone(), "null".into()));
        duplicate.push((entity.clone(), format!("\"{other}\"")));
        assert_raw_text_parity(&row_text(&duplicate), &metadata);
        // Non-column keys, including unicode and escaped keys, are ignored.
        let mut extra = base(&format!("\"{id}\""));
        extra.push(("clé".into(), r#""v""#.into()));
        extra.push(("path/to".into(), r#"{"k":true}"#.into()));
        assert_raw_text_parity(&row_text(&extra), &metadata);
        // Non-object rows and malformed documents keep the array-level errors.
        for (text, expected) in [
            ("1", ResultDecodeError::InvalidBatchRows),
            ("{}", ResultDecodeError::InvalidBatchRows),
            ("", ResultDecodeError::InvalidBatchRows),
            ("[1]", ResultDecodeError::InvalidBatchRow(0)),
            ("[{} , 2]", ResultDecodeError::InvalidBatchRow(1)),
            ("[{}] trailing", ResultDecodeError::InvalidBatchRows),
        ] {
            assert_eq!(
                decode_raw_json_query_rows_with_options(
                    text,
                    &metadata,
                    ResultDecodeOptions::default()
                ),
                Err(expected),
                "{text}"
            );
        }
    }

    #[test]
    #[ignore = "manual parent regrouping benchmark; not a capacity or timing gate"]
    fn grouped_nested_json_decode_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;
        let (metadata, mut rows) = json_decode_fixture(3, 100, false);
        for (index, row) in rows.as_array_mut().unwrap().iter_mut().enumerate() {
            for pattern in flattened_pattern_metadata(&metadata) {
                row[&pattern.cte_columns[0]] = json!(Uuid::from_u128(1 + index as u128 / 10));
            }
        }
        let symbol = Symbol::new("?entity").unwrap();
        for indexed in [false, true] {
            let started = Instant::now();
            for _ in 0..200 {
                let grouped = indexed.then(|| {
                    UuidGroupedJsonRows::new(black_box(&rows), &metadata, &symbol).unwrap()
                });
                for parent in (1..=10).map(Uuid::from_u128) {
                    let bound = bind_pattern_metadata_uuid_symbol(&metadata, &symbol, parent);
                    let result = if let Some(grouped) = &grouped {
                        grouped
                            .decode_with_options(parent, &bound, ResultDecodeOptions::default())
                            .unwrap()
                    } else {
                        let filtered = filter_json_query_rows_by_uuid_symbol(
                            &rows, &metadata, &symbol, parent,
                        )
                        .unwrap();
                        decode_json_query_rows(&filtered, &bound).unwrap()
                    };
                    black_box(result);
                }
            }
            println!(
                "nested-regroup indexed={indexed} iterations=200 rows=100 parents=10 patterns=3 elapsed_us={}",
                started.elapsed().as_micros()
            );
        }
    }

    #[test]
    #[ignore = "synthetic allocation/decode benchmark; run explicitly in release mode"]
    fn borrowed_json_decode_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;
        let (metadata, rows) = json_decode_fixture(8, 20, false);
        let iterations = 2000;
        let start = Instant::now();
        for _ in 0..iterations {
            let converted = json_query_rows(black_box(&rows), &metadata).unwrap();
            black_box(decode_query_rows(&converted, &metadata).unwrap());
        }
        let eager = start.elapsed();
        let start = Instant::now();
        for _ in 0..iterations {
            black_box(decode_json_query_rows(black_box(&rows), &metadata).unwrap());
        }
        let borrowed = start.elapsed();
        println!(
            "json-result-decode: iterations={iterations} rows=20 patterns=8 eager_us={} borrowed_us={}",
            eager.as_micros(),
            borrowed.as_micros()
        );
    }
}
