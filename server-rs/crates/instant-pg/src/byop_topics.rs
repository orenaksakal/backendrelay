//! Query dependencies and WAL invalidations for Bring Your Own Postgres.

use instant_instaql::{Form, WhereCondition};
use instant_wal::{Lsn, RawByopWalTransaction, WalAction, WalEntry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{ByopSchema, ByopTable};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ByopMatchSet<T> {
    Any,
    Values(Vec<T>),
}

impl<T: PartialEq> ByopMatchSet<T> {
    fn values(values: impl IntoIterator<Item = T>) -> Self {
        let mut unique = Vec::new();
        for value in values {
            if !unique.contains(&value) {
                unique.push(value);
            }
        }
        Self::Values(unique)
    }
}

/// BYOP's conservative `[ea entity attribute value]` dependency tuple.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByopTopic {
    pub entities: ByopMatchSet<Value>,
    pub attributes: ByopMatchSet<Uuid>,
    pub values: ByopMatchSet<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ByopWalRecord {
    pub tx_id: u64,
    pub next_lsn: Lsn,
    pub changes: Vec<WalEntry>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ByopTopicError {
    #[error("BYOP WAL change is missing its table")]
    MissingTable,
    #[error("BYOP table `{0}` has no primary-key value in the WAL change")]
    MissingPrimaryKey(String),
    #[error("invalid BYOP result envelope at {0}")]
    InvalidEnvelope(&'static str),
}

/// Filters a committed logical-WAL transaction to BYOP row changes and uses the
/// commit LSN as its monotonically increasing transaction identity.
#[must_use]
pub fn transform_byop_wal_record(changes: Vec<WalEntry>, next_lsn: Lsn) -> Option<ByopWalRecord> {
    let changes = changes
        .into_iter()
        .filter(|change| {
            matches!(
                change.action,
                WalAction::Insert | WalAction::Update | WalAction::Delete | WalAction::Truncate
            )
        })
        .collect::<Vec<_>>();
    (!changes.is_empty()).then(|| ByopWalRecord {
        tx_id: next_lsn.as_u64(),
        next_lsn,
        changes,
    })
}

/// Converts the output of [`instant_wal::ByopWalAssembler`] into a query
/// invalidation record.
#[must_use]
pub fn transform_raw_byop_transaction(raw: RawByopWalTransaction) -> Option<ByopWalRecord> {
    transform_byop_wal_record(raw.changes, raw.next_lsn)
}

/// Converts BYOP row changes into the same conservative dependencies emitted by
/// the JVM path: inserts include new values, while updates and deletes wildcard
/// values because wal2json does not reliably provide their before-images.
///
/// # Errors
///
/// Returns an error when a known table change lacks its primary key.
pub fn topics_for_byop_record(
    schema: &ByopSchema,
    record: &ByopWalRecord,
) -> Result<Vec<ByopTopic>, ByopTopicError> {
    let mut topics = Vec::new();
    for change in &record.changes {
        let table_name = change
            .table
            .as_deref()
            .ok_or(ByopTopicError::MissingTable)?;
        let Some(table) = schema.tables.get(table_name) else {
            push_unique(
                &mut topics,
                ByopTopic {
                    entities: ByopMatchSet::Any,
                    attributes: ByopMatchSet::Any,
                    values: ByopMatchSet::Any,
                },
            );
            continue;
        };
        if change.action == WalAction::Truncate {
            push_unique(
                &mut topics,
                ByopTopic {
                    entities: ByopMatchSet::Any,
                    attributes: ByopMatchSet::Any,
                    values: ByopMatchSet::Any,
                },
            );
            continue;
        }
        let columns = if change.action == WalAction::Delete {
            &change.identity
        } else {
            &change.columns
        };
        let entity = columns
            .iter()
            .find(|column| column.name == table.primary_key.field)
            .or_else(|| {
                change
                    .identity
                    .iter()
                    .find(|column| column.name == table.primary_key.field)
            })
            .map(|column| column.value.clone())
            .ok_or_else(|| ByopTopicError::MissingPrimaryKey(table_name.to_owned()))?;
        if change.action == WalAction::Delete {
            // DELETE identity payloads commonly contain only the primary key;
            // invalidate every relation edge because the old FK value is not
            // available to wal2json.
            for relation in table.relations.values() {
                push_unique(
                    &mut topics,
                    ByopTopic {
                        entities: ByopMatchSet::Any,
                        attributes: ByopMatchSet::values([relation.attribute_id]),
                        values: ByopMatchSet::Any,
                    },
                );
            }
        }
        for column in columns {
            let Some(field) = table.fields.get(&column.name) else {
                continue;
            };
            push_unique(
                &mut topics,
                ByopTopic {
                    entities: ByopMatchSet::values([entity.clone()]),
                    attributes: ByopMatchSet::values([field.attribute_id]),
                    values: if change.action == WalAction::Insert {
                        ByopMatchSet::values([column.value.clone()])
                    } else {
                        ByopMatchSet::Any
                    },
                },
            );
            for relation in table.relations.values() {
                if relation.table_field == column.name {
                    push_unique(
                        &mut topics,
                        ByopTopic {
                            entities: ByopMatchSet::values([entity.clone()]),
                            attributes: ByopMatchSet::values([relation.attribute_id]),
                            values: ByopMatchSet::Any,
                        },
                    );
                }
            }
        }
    }
    Ok(topics)
}

/// Computes query dependencies from both the typed forms and concrete result rows.
/// This includes coarse where attributes, primary-key catchalls, returned entity
/// catchalls, and relation attributes used by nested children.
///
/// # Errors
///
/// Returns an error for a malformed result envelope.
pub fn topics_for_byop_query(
    schema: &ByopSchema,
    forms: &[Form],
    result: &Value,
) -> Result<Vec<ByopTopic>, ByopTopicError> {
    let mut topics = Vec::new();
    for form in forms {
        collect_form_topics(schema, form, &mut topics);
    }
    let roots = result
        .as_array()
        .ok_or(ByopTopicError::InvalidEnvelope("root array"))?;
    for root in roots {
        collect_result_topics(schema, root, &mut topics)?;
    }
    Ok(topics)
}

/// Returns true when any concrete WAL invalidation intersects any query dependency.
#[must_use]
pub fn byop_topics_intersect(changes: &[ByopTopic], queries: &[ByopTopic]) -> bool {
    changes.iter().any(|change| {
        queries.iter().any(|query| {
            match_sets_intersect(&change.entities, &query.entities)
                && match_sets_intersect(&change.attributes, &query.attributes)
                && match_sets_intersect(&change.values, &query.values)
        })
    })
}

fn collect_form_topics(schema: &ByopSchema, form: &Form, topics: &mut Vec<ByopTopic>) {
    let Some(table) = schema.tables.get(&form.etype) else {
        return;
    };
    for condition in &form.options.where_clause {
        collect_condition_topics(schema, table, condition, topics);
    }
    // Ordering is part of the result dependency: an insert or update to a
    // sort field can move a row into a limited page even when it matches no
    // where clause and is not currently returned.
    if let Some(order) = &form.options.order
        && let Some(field) = table.fields.get(&order.field)
    {
        push_unique(
            topics,
            ByopTopic {
                entities: ByopMatchSet::Any,
                attributes: ByopMatchSet::values([field.attribute_id]),
                values: ByopMatchSet::Any,
            },
        );
    }
    for child in &form.children {
        if let Some(relation) = table.relations.get(&child.key) {
            push_unique(
                topics,
                ByopTopic {
                    entities: ByopMatchSet::Any,
                    attributes: ByopMatchSet::values([relation.attribute_id]),
                    values: ByopMatchSet::Any,
                },
            );
        }
        collect_form_topics(schema, child, topics);
    }
}

fn collect_condition_topics(
    schema: &ByopSchema,
    table: &ByopTable,
    condition: &WhereCondition,
    topics: &mut Vec<ByopTopic>,
) {
    match condition {
        WhereCondition::And { conditions } | WhereCondition::Or { conditions } => {
            for condition in conditions {
                collect_condition_topics(schema, table, condition, topics);
            }
        }
        WhereCondition::Condition { path, .. } => {
            let mut current = table;
            for relation_name in path.iter().take(path.len().saturating_sub(1)) {
                let Some(relation) = current.relations.get(relation_name) else {
                    return;
                };
                push_unique(
                    topics,
                    ByopTopic {
                        entities: ByopMatchSet::Any,
                        attributes: ByopMatchSet::values([relation.attribute_id]),
                        values: ByopMatchSet::Any,
                    },
                );
                let Some(next) = schema.tables.get(&relation.other_table) else {
                    return;
                };
                current = next;
            }
            let field_name = path
                .last()
                .map_or(current.primary_key.field.as_str(), String::as_str);
            let attribute_id = if field_name == "$entityIdStartsWith" {
                current.primary_key.attribute_id
            } else if let Some(field) = current.fields.get(field_name) {
                field.attribute_id
            } else {
                return;
            };
            push_unique(
                topics,
                ByopTopic {
                    entities: ByopMatchSet::Any,
                    attributes: ByopMatchSet::values([attribute_id]),
                    values: ByopMatchSet::Any,
                },
            );
        }
    }
}

fn collect_result_topics(
    schema: &ByopSchema,
    root: &Value,
    topics: &mut Vec<ByopTopic>,
) -> Result<(), ByopTopicError> {
    let etype = root
        .get("etype")
        .and_then(Value::as_str)
        .ok_or(ByopTopicError::InvalidEnvelope("entity type"))?;
    let Some(table) = schema.tables.get(etype) else {
        return Ok(());
    };
    push_unique(
        topics,
        ByopTopic {
            entities: ByopMatchSet::Any,
            attributes: ByopMatchSet::values([table.primary_key.attribute_id]),
            values: ByopMatchSet::Any,
        },
    );
    let Some(rows) = root.pointer("/data/rows").and_then(Value::as_array) else {
        return Ok(());
    };
    for entry in rows {
        let row = entry
            .get("row")
            .and_then(Value::as_object)
            .ok_or(ByopTopicError::InvalidEnvelope("row"))?;
        let entity = row
            .get(&table.primary_key.field)
            .cloned()
            .ok_or(ByopTopicError::InvalidEnvelope("row primary key"))?;
        push_unique(
            topics,
            ByopTopic {
                entities: ByopMatchSet::values([entity]),
                attributes: ByopMatchSet::Any,
                values: ByopMatchSet::Any,
            },
        );
        if let Some(children) = entry.get("children").and_then(Value::as_array) {
            for child in children {
                let child_key = child
                    .get("k")
                    .and_then(Value::as_str)
                    .ok_or(ByopTopicError::InvalidEnvelope("child key"))?;
                if let Some(relation) = table.relations.get(child_key) {
                    push_unique(
                        topics,
                        ByopTopic {
                            entities: ByopMatchSet::Any,
                            attributes: ByopMatchSet::values([relation.attribute_id]),
                            values: ByopMatchSet::Any,
                        },
                    );
                }
                collect_result_topics(schema, child, topics)?;
            }
        }
    }
    Ok(())
}

fn match_sets_intersect<T: PartialEq>(left: &ByopMatchSet<T>, right: &ByopMatchSet<T>) -> bool {
    match (left, right) {
        (ByopMatchSet::Any, _) | (_, ByopMatchSet::Any) => true,
        (ByopMatchSet::Values(left), ByopMatchSet::Values(right)) => {
            left.iter().any(|value| right.contains(value))
        }
    }
}

fn push_unique(topics: &mut Vec<ByopTopic>, topic: ByopTopic) {
    if !topics.contains(&topic) {
        topics.push(topic);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use instant_datalog::OrderDirection;
    use instant_instaql::{AttributeCatalog, FormOptions, Order, WhereValue};
    use instant_wal::WalColumn;
    use serde_json::json;

    use super::*;
    use crate::{ByopField, ByopPrimaryKey, ByopRelation};

    fn schema() -> ByopSchema {
        let id = Uuid::from_u64_pair(1, 1);
        let name = Uuid::from_u64_pair(1, 2);
        ByopSchema {
            catalog: AttributeCatalog::default(),
            tables: BTreeMap::from([(
                "users".to_owned(),
                ByopTable {
                    fields: BTreeMap::from([
                        (
                            "id".to_owned(),
                            ByopField {
                                database_type: "int8".to_owned(),
                                attribute_id: id,
                                indexed: true,
                                unique: false,
                            },
                        ),
                        (
                            "name".to_owned(),
                            ByopField {
                                database_type: "text".to_owned(),
                                attribute_id: name,
                                indexed: false,
                                unique: false,
                            },
                        ),
                    ]),
                    attribute_id_to_field: BTreeMap::from([
                        (id, "id".to_owned()),
                        (name, "name".to_owned()),
                    ]),
                    relations: BTreeMap::from([(
                        "friends".to_owned(),
                        ByopRelation {
                            direction: crate::ByopRelationDirection::Forward,
                            attribute_id: Uuid::from_u64_pair(1, 3),
                            table: "users".to_owned(),
                            table_field: "id".to_owned(),
                            other_table: "users".to_owned(),
                            other_table_field: "id".to_owned(),
                        },
                    )]),
                    primary_key: ByopPrimaryKey {
                        field: "id".to_owned(),
                        attribute_id: id,
                    },
                    created_at_field: None,
                },
            )]),
        }
    }

    fn entry(action: WalAction, columns: Vec<WalColumn>, identity: Vec<WalColumn>) -> WalEntry {
        WalEntry {
            action,
            tx_bytes: 0,
            table: Some("users".to_owned()),
            columns,
            identity,
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    #[test]
    fn insert_update_and_delete_topics_match_the_reference_wildcards() {
        let schema = schema();
        let columns = vec![
            WalColumn {
                name: "id".to_owned(),
                value: json!(7),
            },
            WalColumn {
                name: "name".to_owned(),
                value: json!("Oren"),
            },
        ];
        let record = transform_byop_wal_record(
            vec![
                entry(WalAction::Insert, columns.clone(), Vec::new()),
                entry(WalAction::Update, columns.clone(), Vec::new()),
                entry(WalAction::Delete, Vec::new(), columns),
            ],
            Lsn::from_u64(42),
        )
        .unwrap();
        assert_eq!(record.tx_id, 42);
        let topics = topics_for_byop_record(&schema, &record).unwrap();
        assert!(topics.iter().any(|topic| {
            topic.attributes
                == ByopMatchSet::values([schema.tables["users"].fields["name"].attribute_id])
                && topic.values == ByopMatchSet::values([json!("Oren")])
        }));
        assert!(topics.iter().any(|topic| topic.values == ByopMatchSet::Any));
        let relation_id = schema.tables["users"].relations["friends"].attribute_id;
        assert!(topics.iter().any(|topic| {
            topic.attributes == ByopMatchSet::values([relation_id])
                && topic.entities == ByopMatchSet::values([json!(7)])
        }));
        assert!(topics.iter().any(|topic| {
            topic.attributes == ByopMatchSet::values([relation_id])
                && topic.entities == ByopMatchSet::Any
        }));

        let identity_only_key = ByopWalRecord {
            tx_id: 2,
            next_lsn: Lsn::from_u64(2),
            changes: vec![entry(
                WalAction::Update,
                vec![WalColumn {
                    name: "name".to_owned(),
                    value: json!("Renamed"),
                }],
                vec![WalColumn {
                    name: "id".to_owned(),
                    value: json!(7),
                }],
            )],
        };
        assert!(topics_for_byop_record(&schema, &identity_only_key).is_ok());
    }

    #[test]
    fn tables_added_after_introspection_invalidate_conservatively() {
        let mut change = entry(
            WalAction::Insert,
            vec![WalColumn {
                name: "id".to_owned(),
                value: json!(1),
            }],
            Vec::new(),
        );
        change.table = Some("new_table".to_owned());
        let record = transform_byop_wal_record(vec![change], Lsn::from_u64(43)).unwrap();

        assert_eq!(
            topics_for_byop_record(&schema(), &record).unwrap(),
            vec![ByopTopic {
                entities: ByopMatchSet::Any,
                attributes: ByopMatchSet::Any,
                values: ByopMatchSet::Any,
            }]
        );
    }

    #[test]
    fn truncate_wal_events_invalidate_every_dependency() {
        let record = transform_byop_wal_record(
            vec![entry(WalAction::Truncate, Vec::new(), Vec::new())],
            Lsn::from_u64(44),
        )
        .expect("truncate is a meaningful BYOP change");
        let topics = topics_for_byop_record(&schema(), &record).unwrap();
        assert_eq!(
            topics,
            vec![ByopTopic {
                entities: ByopMatchSet::Any,
                attributes: ByopMatchSet::Any,
                values: ByopMatchSet::Any,
            }]
        );
    }

    #[test]
    fn query_and_change_topics_intersect_on_returned_entities_and_where_fields() {
        let schema = schema();
        let form = Form {
            key: "users".to_owned(),
            etype: "users".to_owned(),
            level: 0,
            options: FormOptions {
                where_clause: vec![WhereCondition::Condition {
                    path: vec!["name".to_owned()],
                    value: WhereValue::Scalar(json!("Oren")),
                }],
                ..FormOptions::default()
            },
            children: Vec::new(),
        };
        let query_topics = topics_for_byop_query(
            &schema,
            &[form],
            &json!([{"k":"users", "etype":"users", "data":{"rows":[{"row":{"id":7,"name":"Oren"}}]}}]),
        )
        .unwrap();
        let record = ByopWalRecord {
            tx_id: 1,
            next_lsn: Lsn::from_u64(1),
            changes: vec![entry(
                WalAction::Update,
                vec![
                    WalColumn {
                        name: "id".to_owned(),
                        value: json!(7),
                    },
                    WalColumn {
                        name: "name".to_owned(),
                        value: json!("Other"),
                    },
                ],
                Vec::new(),
            )],
        };
        let changes = topics_for_byop_record(&schema, &record).unwrap();
        assert!(byop_topics_intersect(&changes, &query_topics));
    }

    #[test]
    fn ordered_pages_invalidate_when_an_unreturned_sort_value_changes() {
        let schema = schema();
        let form = Form {
            key: "users".to_owned(),
            etype: "users".to_owned(),
            level: 0,
            options: FormOptions {
                order: Some(Order {
                    field: "name".to_owned(),
                    direction: OrderDirection::Asc,
                }),
                limit: Some(1),
                ..FormOptions::default()
            },
            children: Vec::new(),
        };
        let query_topics = topics_for_byop_query(
            &schema,
            &[form],
            &json!([{"k":"users", "etype":"users", "data":{"rows":[{"row":{"id":7,"name":"Oren"}}]}}]),
        )
        .unwrap();
        let inserted = ByopWalRecord {
            tx_id: 2,
            next_lsn: Lsn::from_u64(2),
            changes: vec![entry(
                WalAction::Insert,
                vec![
                    WalColumn {
                        name: "id".to_owned(),
                        value: json!(8),
                    },
                    WalColumn {
                        name: "name".to_owned(),
                        value: json!("Aaron"),
                    },
                ],
                Vec::new(),
            )],
        };
        let changes = topics_for_byop_record(&schema, &inserted).unwrap();
        assert!(byop_topics_intersect(&changes, &query_topics));
    }

    #[test]
    fn nested_queries_depend_on_relation_keys_even_without_child_rows() {
        let schema = schema();
        let relation_id = schema.tables["users"].relations["friends"].attribute_id;
        let query_topics = topics_for_byop_query(
            &schema,
            &[Form {
                key: "users".to_owned(),
                etype: "users".to_owned(),
                level: 0,
                options: FormOptions::default(),
                children: vec![Form {
                    key: "friends".to_owned(),
                    etype: "users".to_owned(),
                    level: 1,
                    options: FormOptions::default(),
                    children: Vec::new(),
                }],
            }],
            &json!([{"k":"users", "etype":"users", "data":{"rows":[{"row":{"id":7}}]}}]),
        )
        .unwrap();
        assert!(query_topics.iter().any(|topic| {
            topic.attributes == ByopMatchSet::values([relation_id])
                && topic.entities == ByopMatchSet::Any
        }));
    }

    #[test]
    fn relation_filters_depend_on_the_relation_edge() {
        let schema = schema();
        let relation_id = schema.tables["users"].relations["friends"].attribute_id;
        let form = Form {
            key: "users".to_owned(),
            etype: "users".to_owned(),
            level: 0,
            options: FormOptions {
                where_clause: vec![WhereCondition::Condition {
                    path: vec!["friends".to_owned(), "name".to_owned()],
                    value: WhereValue::Scalar(json!("Oren")),
                }],
                ..FormOptions::default()
            },
            children: Vec::new(),
        };
        let topics = topics_for_byop_query(
            &schema,
            &[form],
            &json!([{"k":"users", "etype":"users", "data":{"rows":[]}}]),
        )
        .unwrap();
        assert!(topics.iter().any(|topic| {
            topic.attributes == ByopMatchSet::values([relation_id])
                && topic.entities == ByopMatchSet::Any
        }));
    }
}
