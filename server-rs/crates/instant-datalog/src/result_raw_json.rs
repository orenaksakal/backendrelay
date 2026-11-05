//! Raw JSON text row decoding. Row envelopes are borrowed as text instead of
//! being materialized as an owned [`Value`] tree before decoding.
use std::borrow::Cow;

use super::grouped_json::index_uuid_rows;
use super::*;

/// One borrowed `row_to_json(...)` object from a JSON result table.
///
/// Fields point at the row text; only the values the decoder actually owns
/// (triple payloads) are parsed into a [`Value`]. Duplicate keys keep their last
/// occurrence, matching `serde_json::Map` insertion order.
struct RawJsonRow<'a> {
    fields: Vec<(Cow<'a, str>, &'a str)>,
}

impl<'a> RawJsonRow<'a> {
    /// Splits the JSON object starting at `start` into borrowed key/value
    /// slices, returning the row and the index just past its closing brace.
    ///
    /// The caller has already validated the enclosing document, so this only
    /// has to find top-level members; it never re-validates JSON syntax.
    fn split(text: &'a str, start: usize) -> Option<(Self, usize)> {
        let bytes = text.as_bytes();
        if bytes.get(start) != Some(&b'{') {
            return None;
        }
        let mut fields = Vec::new();
        let mut index = skip_whitespace(bytes, start + 1);
        if bytes.get(index) == Some(&b'}') {
            return Some((Self { fields }, index + 1));
        }
        loop {
            if bytes.get(index) != Some(&b'"') {
                return None;
            }
            let (inner_start, inner_end, next) = scan_string(text, index)?;
            let inner = &text[inner_start..inner_end];
            let key = if inner.contains('\\') {
                // Escaped member names still decode to the same column name.
                Cow::Owned(serde_json::from_str::<String>(&text[index..next]).ok()?)
            } else {
                Cow::Borrowed(inner)
            };
            index = skip_whitespace(bytes, next);
            if bytes.get(index) != Some(&b':') {
                return None;
            }
            let value_start = skip_whitespace(bytes, index + 1);
            let value_end = scan_value(bytes, value_start)?;
            fields.push((key, &text[value_start..value_end]));
            index = skip_whitespace(bytes, value_end);
            match bytes.get(index) {
                Some(b',') => index = skip_whitespace(bytes, index + 1),
                Some(b'}') => return Some((Self { fields }, index + 1)),
                _ => return None,
            }
        }
    }

    fn field(&self, column: &str) -> Option<&'a str> {
        self.fields
            .iter()
            .rev()
            .find(|(key, _)| key == column)
            .map(|(_, value)| *value)
    }

    fn to_value(&self) -> Value {
        Value::Object(
            self.fields
                .iter()
                .map(|(key, value)| (key.to_string(), parse_json_text(value)))
                .collect(),
        )
    }
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        index += 1;
    }
    index
}

/// Returns the contents range of the string at `start` plus the index just past
/// its closing quote. Escape sequences are left untouched.
fn scan_string(text: &str, start: usize) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some((start + 1, index, index + 1)),
            _ => index += 1,
        }
    }
    None
}

/// Returns the index just past the JSON value starting at `start`.
fn scan_value(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'"' => {
            let mut index = start + 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index += 2,
                    b'"' => return Some(index + 1),
                    _ => index += 1,
                }
            }
            None
        }
        b'{' | b'[' => {
            let mut depth = 0_usize;
            let mut index = start;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index += 1;
                        while index < bytes.len() {
                            match bytes[index] {
                                b'\\' => index += 2,
                                b'"' => break,
                                _ => index += 1,
                            }
                        }
                        index += 1;
                    }
                    b'{' | b'[' => {
                        depth += 1;
                        index += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        index += 1;
                        if depth == 0 {
                            return Some(index);
                        }
                    }
                    _ => index += 1,
                }
            }
            None
        }
        _ => {
            let mut index = start;
            while index < bytes.len()
                && !matches!(
                    bytes[index],
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
                )
            {
                index += 1;
            }
            Some(index)
        }
    }
}

/// Parses the JSON array text of one result table into borrowed row objects.
///
/// # Errors
///
/// Returns [`ResultDecodeError::InvalidBatchRows`] when the text is not a JSON
/// array and [`ResultDecodeError::InvalidBatchRow`] for a non-object row,
/// matching the eager `Value` envelope's validation order.
fn parse_raw_json_rows(rows: &str) -> Result<Vec<RawJsonRow<'_>>, ResultDecodeError> {
    let bytes = rows.as_bytes();
    let mut parsed = Vec::new();
    let mut index = skip_whitespace(bytes, 0);
    if bytes.get(index) != Some(&b'[') {
        return Err(ResultDecodeError::InvalidBatchRows);
    }
    index = skip_whitespace(bytes, index + 1);
    if bytes.get(index) != Some(&b']') {
        loop {
            // Array elements and their member spans are found in one pass.
            let (row, end) = RawJsonRow::split(rows, index)
                .ok_or(ResultDecodeError::InvalidBatchRow(parsed.len()))?;
            parsed.push(row);
            index = skip_whitespace(bytes, end);
            match bytes.get(index) {
                Some(b',') => index = skip_whitespace(bytes, index + 1),
                Some(b']') => {
                    index += 1;
                    break;
                }
                _ => return Err(ResultDecodeError::InvalidBatchRows),
            }
        }
    } else {
        index += 1;
    }
    if skip_whitespace(bytes, index) != bytes.len() {
        return Err(ResultDecodeError::InvalidBatchRows);
    }
    Ok(parsed)
}

fn raw_scalar(value: &str) -> &str {
    value.trim()
}

fn parse_json_text(value: &str) -> Value {
    // The slice was already validated as JSON by the envelope parser.
    serde_json::from_str(value).unwrap_or(Value::Null)
}

fn raw_display(value: &str) -> String {
    parse_json_text(value).to_string()
}

fn quoted_text(value: &str) -> Option<&str> {
    value.trim().strip_prefix('"')?.strip_suffix('"')
}

/// Borrows the contents of a raw JSON string, unescaping only when required.
fn raw_string(value: &str) -> Option<Cow<'_, str>> {
    let value = value.trim();
    let inner = quoted_text(value)?;
    if inner.contains('\\') {
        serde_json::from_str(value).ok().map(Cow::Owned)
    } else {
        Some(Cow::Borrowed(inner))
    }
}

fn raw_field<'a>(row: &RawJsonRow<'a>, column: &str) -> Result<&'a str, ResultDecodeError> {
    row.field(column)
        .ok_or_else(|| ResultDecodeError::MissingColumn(column.to_owned()))
}

fn raw_uuid(row: &RawJsonRow<'_>, column: &str) -> Result<Option<Uuid>, ResultDecodeError> {
    let value = raw_field(row, column)?;
    // UUID columns are plain unescaped strings in every planned statement, so
    // parse the borrowed slice first and unescape only for exotic payloads.
    if let Some(inner) = quoted_text(value)
        && let Ok(uuid) = Uuid::parse_str(inner)
    {
        return Ok(Some(uuid));
    }
    if raw_scalar(value) == "null" {
        return Ok(None);
    }
    raw_string(value)
        .and_then(|text| Uuid::parse_str(&text).ok())
        .map(Some)
        .ok_or_else(|| ResultDecodeError::InvalidBatchColumn {
            column: column.to_owned(),
            expected: "a UUID string or null",
            value: raw_display(value),
        })
}

fn raw_boolean(row: &RawJsonRow<'_>, column: &str) -> Result<Option<bool>, ResultDecodeError> {
    let value = raw_field(row, column)?;
    match raw_scalar(value) {
        "null" => Ok(None),
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => Err(ResultDecodeError::InvalidBatchColumn {
            column: column.to_owned(),
            expected: "a boolean or null",
            value: raw_display(value),
        }),
    }
}

fn raw_integer(row: &RawJsonRow<'_>, column: &str) -> Result<Option<i64>, ResultDecodeError> {
    let value = raw_field(row, column)?;
    if raw_scalar(value) == "null" {
        return Ok(None);
    }
    raw_scalar(value)
        .parse::<i64>()
        .map(Some)
        .map_err(|_| ResultDecodeError::InvalidBatchColumn {
            column: column.to_owned(),
            expected: "an integer or null",
            value: raw_display(value),
        })
}

impl ResultRow for RawJsonRow<'_> {
    fn triple(&self, columns: &[String]) -> Result<Option<DatalogTriple>, ResultDecodeError> {
        let [entity, attribute, value, reference, created] = columns else {
            return Err(ResultDecodeError::InvalidMetadataColumns(columns.len()));
        };
        let entity_id = raw_uuid(self, entity)?;
        let attr_id = raw_uuid(self, attribute)?;
        let json_value = raw_field(self, value)?;
        let reference = raw_boolean(self, reference)?.unwrap_or(false);
        let created_at = raw_integer(self, created)?;
        // Even an absent OR branch must validate every scalar column, just as
        // the eager conversion does before discarding its null-entity triple.
        let Some(entity_id) = entity_id else {
            return Ok(None);
        };
        let attr_id = attr_id.ok_or_else(|| ResultDecodeError::NullAttribute(attribute.clone()))?;
        let value = if reference {
            let encoded =
                raw_string(json_value).ok_or_else(|| ResultDecodeError::InvalidReferenceUuid {
                    column: value.clone(),
                    value: raw_display(json_value),
                })?;
            DatalogValue::Uuid(Uuid::parse_str(&encoded).map_err(|_| {
                ResultDecodeError::InvalidReferenceUuid {
                    column: value.clone(),
                    value: encoded.to_string(),
                }
            })?)
        } else {
            DatalogValue::Json(parse_json_text(json_value))
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
        raw_uuid(self, column)
    }

    fn integer(&self, column: &str) -> Result<Option<i64>, ResultDecodeError> {
        raw_integer(self, column)
    }
}

impl ResultRow for &RawJsonRow<'_> {
    fn triple(&self, columns: &[String]) -> Result<Option<DatalogTriple>, ResultDecodeError> {
        (*self).triple(columns)
    }

    fn uuid(&self, column: &str) -> Result<Option<Uuid>, ResultDecodeError> {
        (*self).uuid(column)
    }

    fn integer(&self, column: &str) -> Result<Option<i64>, ResultDecodeError> {
        (*self).integer(column)
    }
}

/// Eager fallback conversion for aliased metadata or failed borrowed decoding.
///
/// This mirrors [`json_query_rows_from_slice`] column for column so malformed
/// rows retain the eager decoder's exact error variant and precedence.
fn raw_json_query_rows(
    rows: &[RawJsonRow<'_>],
    metadata: &[PatternMetadataNode],
) -> Result<Vec<QueryRow>, ResultDecodeError> {
    let patterns = flattened_pattern_metadata(metadata);
    rows.iter()
        .map(|row| {
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
                    .insert(entity.clone(), QueryCell::Uuid(raw_uuid(row, entity)?));
                query_row.cells.insert(
                    attribute.clone(),
                    QueryCell::Uuid(raw_uuid(row, attribute)?),
                );
                let json_value = raw_field(row, value)?;
                query_row.cells.insert(
                    value.clone(),
                    QueryCell::Json(Some(parse_json_text(json_value))),
                );
                query_row.cells.insert(
                    reference.clone(),
                    QueryCell::Boolean(raw_boolean(row, reference)?),
                );
                query_row.cells.insert(
                    created_at.clone(),
                    QueryCell::Integer(raw_integer(row, created_at)?),
                );
            }
            Ok(query_row)
        })
        .collect()
}

/// JSON-envelope equivalent of [`decode_json_query_rows_with_options`] that
/// reads the raw JSON text of one result table.
///
/// # Errors
///
/// Returns [`ResultDecodeError`] for a malformed table or incompatible cell.
pub fn decode_raw_json_query_rows_with_options(
    rows: &str,
    metadata: &[PatternMetadataNode],
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    let rows = parse_raw_json_rows(rows)?;
    if direct_json_columns(metadata, None)
        && let Ok(result) = decode_result_rows(&rows, metadata, options)
    {
        return Ok(result);
    }
    // Eager conversion defines error precedence: row-major cell validation,
    // then pattern-major accumulation. Retain it for malformed inputs and
    // unusual aliased metadata, not just equivalent error categories.
    let query_rows = raw_json_query_rows(&rows, metadata)?;
    decode_query_rows_with_options(&query_rows, metadata, options)
}

/// Raw text equivalent of [`decode_json_paged_query_rows_with_options`].
///
/// # Errors
///
/// Returns [`ResultDecodeError`] for malformed rows or cursor columns.
pub fn decode_raw_json_paged_query_rows_with_options(
    rows: &str,
    metadata: &[PatternMetadataNode],
    page: &PagePlanMetadata,
    has_next_page: bool,
    has_previous_page: bool,
    options: ResultDecodeOptions,
) -> Result<DatalogResult, ResultDecodeError> {
    let rows = parse_raw_json_rows(rows)?;
    if direct_json_columns(metadata, Some(page))
        && rows.iter().all(|row| {
            raw_uuid(row, &page.fallback_entity_column).is_ok()
                && raw_integer(row, &page.fallback_created_at_column).is_ok()
        })
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
    let mut query_rows = raw_json_query_rows(&rows, metadata)?;
    for (raw_row, query_row) in rows.iter().zip(&mut query_rows) {
        query_row.cells.insert(
            page.fallback_entity_column.clone(),
            QueryCell::Uuid(raw_uuid(raw_row, &page.fallback_entity_column)?),
        );
        query_row.cells.insert(
            page.fallback_created_at_column.clone(),
            QueryCell::Integer(raw_integer(raw_row, &page.fallback_created_at_column)?),
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

/// A validated, borrowed JSON table indexed by one nested UUID join symbol,
/// decoded straight from raw JSON text.
///
/// This mirrors [`UuidGroupedJsonRows`] while retaining row objects as borrowed
/// text instead of owned [`Value`] trees.
pub struct UuidGroupedRawJsonRows<'a> {
    rows: Vec<RawJsonRow<'a>>,
    direct: bool,
    positions: HashMap<Uuid, Vec<usize>>,
}

impl<'a> UuidGroupedRawJsonRows<'a> {
    /// Validates one fresh table and indexes its UUID bindings.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as filtering this table by `symbol`.
    pub fn new(
        rows: &'a str,
        metadata: &[PatternMetadataNode],
        symbol: &Symbol,
    ) -> Result<Self, ResultDecodeError> {
        let rows = parse_raw_json_rows(rows)?;
        if direct_json_columns(metadata, None)
            && let Ok(positions) = index_uuid_rows(&rows, metadata, symbol)
        {
            return Ok(Self {
                rows,
                direct: true,
                positions,
            });
        }
        // The reference conversion checks scalar cells row-major before the
        // filter checks triples/symbols. Do not replace its first error with a
        // speculative borrowed-path error, including for absent OR branches.
        let decoded = raw_json_query_rows(&rows, metadata)?;
        let positions = index_uuid_rows(&decoded, metadata, symbol)?;
        Ok(Self {
            rows,
            direct: false,
            positions,
        })
    }

    /// Decodes one parent using its independently bound metadata.
    ///
    /// # Errors
    ///
    /// Returns the same errors as filtering, then decoding with `metadata`.
    pub fn decode_with_options(
        &self,
        parent: Uuid,
        metadata: &[PatternMetadataNode],
        options: ResultDecodeOptions,
    ) -> Result<DatalogResult, ResultDecodeError> {
        if self.direct
            && let Ok(result) = decode_result_rows(&self.selected_rows(parent), metadata, options)
        {
            return Ok(result);
        }
        decode_json_query_rows_with_options(&self.selected_json(parent), metadata, options)
    }

    /// Decodes the same scoped rows with the original cursor/flag semantics.
    ///
    /// # Errors
    ///
    /// Returns the same errors as filtering, then paged JSON decoding.
    pub fn decode_paged_with_options(
        &self,
        parent: Uuid,
        metadata: &[PatternMetadataNode],
        page: &PagePlanMetadata,
        has_next: bool,
        has_previous: bool,
        options: ResultDecodeOptions,
    ) -> Result<DatalogResult, ResultDecodeError> {
        let selected = self.selected_rows(parent);
        if self.direct
            && selected.iter().all(|row| {
                raw_uuid(row, &page.fallback_entity_column).is_ok()
                    && raw_integer(row, &page.fallback_created_at_column).is_ok()
            })
            && let Ok(result) =
                decode_paged_result_rows(&selected, metadata, page, has_next, has_previous, options)
        {
            return Ok(result);
        }
        decode_json_paged_query_rows_with_options(
            &self.selected_json(parent),
            metadata,
            page,
            has_next,
            has_previous,
            options,
        )
    }

    fn selected_positions(&self, parent: Uuid) -> &[usize] {
        self.positions.get(&parent).map_or(&[], Vec::as_slice)
    }

    fn selected_rows(&self, parent: Uuid) -> Vec<&RawJsonRow<'a>> {
        self.selected_positions(parent)
            .iter()
            .map(|position| &self.rows[*position])
            .collect()
    }

    fn selected_json(&self, parent: Uuid) -> Value {
        Value::Array(
            self.selected_positions(parent)
                .iter()
                .map(|position| self.rows[*position].to_value())
                .collect(),
        )
    }
}
