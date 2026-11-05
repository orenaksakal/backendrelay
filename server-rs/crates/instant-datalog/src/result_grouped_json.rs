//! Query-local UUID row grouping. No result or authorization survives a decode.
use super::*;

/// A validated, borrowed JSON table indexed by one nested UUID join symbol.
///
/// Preparing a table once avoids rescanning and cloning all its JSON rows for
/// every parent. Row order and duplicate/OR membership match
/// [`filter_json_query_rows_by_uuid_symbol`]. Malformed or aliased inputs retain
/// the original eager decoder's error precedence and fallback behavior.
pub struct UuidGroupedJsonRows<'a> {
    rows: &'a [Value],
    maps: Option<Vec<&'a serde_json::Map<String, Value>>>,
    positions: HashMap<Uuid, Vec<usize>>,
}

impl<'a> UuidGroupedJsonRows<'a> {
    /// Validates one fresh table and indexes its UUID bindings.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as filtering this table by `symbol`.
    pub fn new(
        rows: &'a Value,
        metadata: &[PatternMetadataNode],
        symbol: &Symbol,
    ) -> Result<Self, ResultDecodeError> {
        let rows = rows.as_array().ok_or(ResultDecodeError::InvalidBatchRows)?;
        if direct_json_columns(metadata, None) {
            let maps: Option<Vec<_>> = rows.iter().map(Value::as_object).collect();
            if let Some(maps) = maps
                && let Ok(positions) = index_uuid_rows(&maps, metadata, symbol)
            {
                return Ok(Self {
                    rows,
                    maps: Some(maps),
                    positions,
                });
            }
        }
        // The reference conversion checks scalar cells row-major before the
        // filter checks triples/symbols. Do not replace its first error with a
        // speculative borrowed-path error, including for absent OR branches.
        let decoded = json_query_rows_from_slice(rows, metadata)?;
        let positions = index_uuid_rows(&decoded, metadata, symbol)?;
        Ok(Self {
            rows,
            maps: None,
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
        if direct_json_columns(metadata, None)
            && let Some(maps) = self.selected_maps(parent)
            && let Ok(result) = decode_result_rows(&maps, metadata, options)
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
        if direct_json_columns(metadata, Some(page))
            && let Some(maps) = self.selected_maps(parent)
            && maps.iter().all(|row| {
                batch_uuid(row, &page.fallback_entity_column).is_ok()
                    && batch_integer(row, &page.fallback_created_at_column).is_ok()
            })
            && let Ok(result) =
                decode_paged_result_rows(&maps, metadata, page, has_next, has_previous, options)
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

    fn selected_maps(&self, parent: Uuid) -> Option<Vec<&'a serde_json::Map<String, Value>>> {
        let maps = self.maps.as_ref()?;
        Some(
            self.selected_positions(parent)
                .iter()
                .map(|position| maps[*position])
                .collect(),
        )
    }

    fn selected_json(&self, parent: Uuid) -> Value {
        Value::Array(
            self.selected_positions(parent)
                .iter()
                .map(|position| self.rows[*position].clone())
                .collect(),
        )
    }
}

pub(super) fn index_uuid_rows<R: ResultRow>(
    rows: &[R],
    metadata: &[PatternMetadataNode],
    symbol: &Symbol,
) -> Result<HashMap<Uuid, Vec<usize>>, ResultDecodeError> {
    let patterns = flattened_pattern_metadata(metadata);
    let mut positions = HashMap::<Uuid, Vec<usize>>::new();
    let mut seen = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        seen.clear();
        for pattern in &patterns {
            let Some(triple) = row.triple(&pattern.cte_columns)? else {
                continue;
            };
            for (index, field) in &pattern.symbol_fields {
                if &field.symbol != symbol {
                    continue;
                }
                match triple_component(&triple, *index)? {
                    DatalogValue::Uuid(value) => {
                        // A row can bind the symbol in several OR branches or
                        // triple positions. Keep it once per matching parent.
                        if !seen.contains(&value) {
                            seen.push(value);
                            positions.entry(value).or_default().push(position);
                        }
                    }
                    DatalogValue::Json(value) => {
                        return Err(ResultDecodeError::InvalidNestedJoinValue {
                            symbol: symbol.as_str().to_owned(),
                            value: value.to_string(),
                        });
                    }
                }
            }
        }
    }
    Ok(positions)
}
