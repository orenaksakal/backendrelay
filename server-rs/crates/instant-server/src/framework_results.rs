//! Exact framework wire projection without intermediate owned JSON copies.
use std::collections::BTreeMap;

use instant_datalog::{DatalogResult, DatalogTriple, DatalogValue, PageCursorResult};
use instant_instaql::{CompiledNode, ExecutedNode};
use serde::ser::{Serialize, SerializeTuple, Serializer};
use serde_json::{Map, Value, json};

struct FrameworkValue<'a>(&'a DatalogValue);

impl Serialize for FrameworkValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            DatalogValue::Uuid(value) => value.serialize(serializer),
            DatalogValue::Json(value) => value.serialize(serializer),
        }
    }
}

struct FrameworkTriple<'a>(&'a DatalogTriple);

impl Serialize for FrameworkTriple<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_tuple(4)?;
        row.serialize_element(&self.0.entity_id)?;
        row.serialize_element(&self.0.attr_id)?;
        row.serialize_element(&FrameworkValue(&self.0.value))?;
        row.serialize_element(&self.0.created_at)?;
        row.end()
    }
}

pub(super) fn framework_result(roots: &[CompiledNode], results: &[ExecutedNode]) -> Value {
    let mut triples = BTreeMap::new();
    let mut page_info = Map::new();
    let mut aggregates = Map::new();
    for (root, result) in roots.iter().zip(results) {
        collect_framework_result(root, result, &mut triples);
        if let Some(info) = &result.selector.page_info {
            page_info.insert(
                root.key.clone(),
                json!({
                    "start-cursor": info.start_cursor.as_ref().map(framework_cursor),
                    "end-cursor": info.end_cursor.as_ref().map(framework_cursor),
                    "has-next-page?": info.has_next_page,
                    "has-previous-page?": info.has_previous_page,
                }),
            );
        }
        if let Some(count) = result.aggregate {
            aggregates.insert(root.key.clone(), json!({"count": count}));
        }
    }
    // Keep the exact old serialized-triple key, ordering and last-entry
    // precedence. Construct owned rows only after deduplication. No query,
    // database value, authorization decision or response is cached.
    let rows = triples
        .into_values()
        .map(|triple| {
            serde_json::to_value(FrameworkTriple(triple))
                .expect("framework triples are JSON serializable")
        })
        .collect();
    let mut data = Map::from_iter([(
        "datalog-result".to_owned(),
        Value::Object(Map::from_iter([(
            "join-rows".to_owned(),
            Value::Array(vec![Value::Array(rows)]),
        )])),
    )]);
    if !page_info.is_empty() {
        data.insert("page-info".to_owned(), Value::Object(page_info));
    }
    if !aggregates.is_empty() {
        data.insert("aggregate".to_owned(), Value::Object(aggregates));
    }
    Value::Array(vec![Value::Object(Map::from_iter([
        ("data".to_owned(), Value::Object(data)),
        ("child-nodes".to_owned(), Value::Array(Vec::new())),
    ]))])
}

fn collect_framework_result<'a>(
    node: &CompiledNode,
    result: &'a ExecutedNode,
    triples: &mut BTreeMap<String, &'a DatalogTriple>,
) {
    collect_datalog_result(&result.selector, triples);
    for entity in &result.entities {
        collect_datalog_result(&entity.fetch, triples);
        for (child, child_result) in node.children.iter().zip(&entity.children) {
            collect_framework_result(child, child_result, triples);
        }
    }
}

fn collect_datalog_result<'a>(
    result: &'a DatalogResult,
    triples: &mut BTreeMap<String, &'a DatalogTriple>,
) {
    for triple in result.join_rows.iter().flatten() {
        let key = serde_json::to_string(&FrameworkTriple(triple))
            .expect("framework triples are JSON serializable");
        triples.insert(key, triple);
    }
}

pub(super) fn framework_cursor(cursor: &PageCursorResult) -> Value {
    json!([
        cursor.entity_id,
        cursor.attr_id,
        cursor.value.as_ref().map(|value| match value {
            DatalogValue::Uuid(value) => json!(value),
            DatalogValue::Json(value) => value.clone(),
        }),
        cursor.created_at,
    ])
}

#[cfg(test)]
#[path = "framework_results/tests.rs"]
mod tests;
