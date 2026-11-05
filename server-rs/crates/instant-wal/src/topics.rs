use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use instant_datalog::IndexKey;
use instant_instaql::{AttributeCatalog, Cardinality};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{WalAction, WalEntry, WalRecord};

const ALL_INDEXES: [IndexKey; 5] = [
    IndexKey::Ea,
    IndexKey::Eav,
    IndexKey::Av,
    IndexKey::Ave,
    IndexKey::Vae,
];

/// Entity type → entity ID → cardinality-one attribute values.
pub type EntitySnapshot = HashMap<String, HashMap<Uuid, HashMap<Uuid, Value>>>;

/// A wildcard or finite set in one component of an invalidation topic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatchSet<T> {
    Any,
    Values(Vec<T>),
}

impl<T: PartialEq> MatchSet<T> {
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

/// Typed values used by reactive query topics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopicValue {
    Json(Value),
    Reference(Uuid),
    Date(DateTime<Utc>),
}

/// The `[indexes e a v]` tuple used by Instant's reactive query invalidator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Topic {
    pub indexes: Vec<IndexKey>,
    pub entities: MatchSet<Uuid>,
    pub attributes: MatchSet<Uuid>,
    pub values: MatchSet<TopicValue>,
}

/// Produces the union of triple, ident, and attribute topics for a WAL record.
///
/// # Errors
///
/// Returns [`TopicError`] when a supported WAL entry is missing a required column or
/// contains a value that cannot be decoded according to its physical index metadata.
pub fn topics_for_record(record: &WalRecord) -> Result<Vec<Topic>, TopicError> {
    let mut topics = Vec::new();
    for change in &record.triple_changes {
        for topic in topics_for_triple_change(change)? {
            push_unique(&mut topics, topic);
        }
    }
    for change in &record.ident_changes {
        if let Some(topic) = topic_for_ident_change(change)? {
            push_unique(&mut topics, topic);
        }
    }
    for change in &record.attr_changes {
        for topic in topics_for_attr_change(change)? {
            push_unique(&mut topics, topic);
        }
    }
    Ok(topics)
}

/// Reconstructs the post-transaction entity snapshots emitted by PostgreSQL triggers.
///
/// Logical messages are applied first; persisted `wal_logs` rows are then applied in
/// creation order, matching the JVM fallback transport.
///
/// # Errors
///
/// Returns [`TopicError::InvalidEntityLog`] if a watched payload is malformed.
pub fn extract_entities_after(record: &WalRecord) -> Result<EntitySnapshot, TopicError> {
    let mut snapshot = EntitySnapshot::new();
    for message in &record.messages {
        if message.prefix.as_deref().is_some_and(watched_prefix) {
            apply_entity_payload(
                &mut snapshot,
                message
                    .content
                    .as_deref()
                    .ok_or(TopicError::InvalidEntityLog)?,
            )?;
        }
    }

    let mut table_messages = record
        .wal_logs
        .iter()
        .filter(|entry| entry.action == WalAction::Insert)
        .filter_map(|entry| {
            let prefix = entry.column("prefix")?.as_str()?;
            let content = entry.column("content")?.as_str()?;
            let created_at = entry.column("created_at")?;
            Some((prefix, content, created_at))
        })
        .map(|(prefix, content, created_at)| {
            parse_date_value(created_at).map(|created_at| (created_at, prefix, content))
        })
        .collect::<Result<Vec<_>, _>>()?;
    table_messages.sort_by_key(|(created_at, _, _)| *created_at);
    for (_, prefix, content) in table_messages {
        if watched_prefix(prefix) {
            apply_entity_payload(&mut snapshot, content)?;
        }
    }
    Ok(snapshot)
}

/// Rewinds post-transaction snapshots through triple changes to recover before-images.
///
/// Only cardinality-one attributes participate, exactly as in the trigger payload.
///
/// # Errors
///
/// Returns [`TopicError`] when a supported triple change has malformed identity data.
pub fn extract_entities_before(
    catalog: &AttributeCatalog,
    mut after: EntitySnapshot,
    record: &WalRecord,
) -> Result<EntitySnapshot, TopicError> {
    for change in record.triple_changes.iter().rev() {
        let columns = if change.action == WalAction::Insert {
            &change.columns
        } else if matches!(change.action, WalAction::Update | WalAction::Delete) {
            &change.identity
        } else {
            continue;
        };
        let entity = uuid_from_columns(columns, "entity_id")?;
        let attr_id = uuid_from_columns(columns, "attr_id")?;
        let Some(attribute) = catalog
            .attributes
            .iter()
            .find(|attribute| attribute.id == attr_id && attribute.cardinality == Cardinality::One)
        else {
            continue;
        };

        if change.action == WalAction::Insert {
            remove_snapshot_attribute(&mut after, &attribute.etype, entity, attr_id);
        } else {
            let value =
                decode_jsonb(column(columns, "value").ok_or(TopicError::MissingColumn("value"))?)?;
            after
                .entry(attribute.etype.clone())
                .or_default()
                .entry(entity)
                .or_default()
                .insert(attr_id, value);
        }
    }
    Ok(after)
}

fn topics_for_triple_change(change: &WalEntry) -> Result<Vec<Topic>, TopicError> {
    match change.action {
        WalAction::Insert => Ok(vec![triple_topic(&change.columns, None)?]),
        WalAction::Delete => Ok(vec![triple_topic(&change.identity, None)?]),
        WalAction::Update => topics_for_triple_update(change),
        WalAction::Truncate => Ok(vec![Topic {
            indexes: ALL_INDEXES.to_vec(),
            entities: MatchSet::Any,
            attributes: MatchSet::Any,
            values: MatchSet::Any,
        }]),
        _ => Ok(Vec::new()),
    }
}

fn topics_for_triple_update(change: &WalEntry) -> Result<Vec<Topic>, TopicError> {
    let entity = uuid_from_columns(&change.columns, "entity_id")?;
    let attribute = uuid_from_columns(&change.columns, "attr_id")?;
    let old_entity = uuid_from_columns(&change.identity, "entity_id")?;
    let old_attribute = uuid_from_columns(&change.identity, "attr_id")?;
    let old_value = topic_value(&change.identity)?;
    let new_value = optional_topic_value(&change.columns)?;

    if entity == old_entity
        && attribute == old_attribute
        && new_value.as_ref().is_none_or(|value| value == &old_value)
    {
        return Ok(Vec::new());
    }

    let indexes = indexes_from_columns(&change.columns);
    let Some(new_value) = new_value else {
        return Err(TopicError::MissingColumn("value"));
    };
    if entity == old_entity && attribute == old_attribute {
        return Ok(vec![Topic {
            indexes,
            entities: MatchSet::values([entity]),
            attributes: MatchSet::values([attribute]),
            values: MatchSet::values([new_value, old_value]),
        }]);
    }

    Ok(vec![
        Topic {
            indexes: indexes.clone(),
            entities: MatchSet::values([entity]),
            attributes: MatchSet::values([attribute]),
            values: MatchSet::values([new_value]),
        },
        Topic {
            indexes,
            entities: MatchSet::values([old_entity]),
            attributes: MatchSet::values([old_attribute]),
            values: MatchSet::values([old_value]),
        },
    ])
}

fn triple_topic(
    columns: &[crate::WalColumn],
    value: Option<TopicValue>,
) -> Result<Topic, TopicError> {
    Ok(Topic {
        indexes: indexes_from_columns(columns),
        entities: MatchSet::values([uuid_from_columns(columns, "entity_id")?]),
        attributes: MatchSet::values([uuid_from_columns(columns, "attr_id")?]),
        values: MatchSet::values([match value {
            Some(value) => value,
            None => topic_value(columns)?,
        }]),
    })
}

fn topic_for_ident_change(change: &WalEntry) -> Result<Option<Topic>, TopicError> {
    if !matches!(change.action, WalAction::Insert | WalAction::Update) {
        return Ok(None);
    }
    Ok(Some(schema_topic(uuid_from_columns(
        &change.columns,
        "attr_id",
    )?)))
}

fn topics_for_attr_change(change: &WalEntry) -> Result<Vec<Topic>, TopicError> {
    match change.action {
        WalAction::Insert | WalAction::Update => {
            let attr_id = uuid_from_columns(&change.columns, "id")?;
            let mut topics = vec![schema_topic(attr_id)];
            let object_attribute = string_from_columns(&change.columns, "value_type")? != "ref";
            let restoration = column(&change.identity, "deletion_marked_at")
                .is_some_and(|value| !value.is_null())
                && column(&change.columns, "deletion_marked_at").is_some_and(Value::is_null);
            if object_attribute && restoration {
                topics.push(Topic {
                    indexes: vec![IndexKey::Ea],
                    entities: MatchSet::Any,
                    attributes: MatchSet::Any,
                    values: MatchSet::Any,
                });
            }
            Ok(topics)
        }
        WalAction::Delete => Ok(vec![schema_topic(uuid_from_columns(
            &change.identity,
            "id",
        )?)]),
        _ => Ok(Vec::new()),
    }
}

fn schema_topic(attr_id: Uuid) -> Topic {
    Topic {
        indexes: ALL_INDEXES.to_vec(),
        entities: MatchSet::Any,
        attributes: MatchSet::values([attr_id]),
        values: MatchSet::Any,
    }
}

fn indexes_from_columns(columns: &[crate::WalColumn]) -> Vec<IndexKey> {
    ALL_INDEXES
        .into_iter()
        .filter(|index| {
            column(columns, index.to_string().as_str()).and_then(Value::as_bool) == Some(true)
        })
        .collect()
}

fn optional_topic_value(columns: &[crate::WalColumn]) -> Result<Option<TopicValue>, TopicError> {
    if column(columns, "value").is_none() {
        return Ok(None);
    }
    topic_value(columns).map(Some)
}

fn topic_value(columns: &[crate::WalColumn]) -> Result<TopicValue, TopicError> {
    let raw = column(columns, "value").ok_or(TopicError::MissingColumn("value"))?;
    let value = decode_jsonb(raw)?;
    if bool_from_columns(columns, "eav") {
        let value = value.as_str().ok_or(TopicError::InvalidReferenceValue)?;
        return value
            .parse()
            .map(TopicValue::Reference)
            .map_err(|_| TopicError::InvalidReferenceValue);
    }
    if column(columns, "checked_data_type").and_then(Value::as_str) == Some("date") {
        return parse_date_value(&value).map(TopicValue::Date);
    }
    Ok(TopicValue::Json(value))
}

fn decode_jsonb(raw: &Value) -> Result<Value, TopicError> {
    match raw {
        Value::String(value) => serde_json::from_str(value).map_err(TopicError::InvalidJsonValue),
        value => Ok(value.clone()),
    }
}

fn parse_date_value(value: &Value) -> Result<DateTime<Utc>, TopicError> {
    if let Some(epoch_millis) = value.as_i64() {
        return DateTime::from_timestamp_millis(epoch_millis).ok_or(TopicError::InvalidDateValue);
    }
    let value = value.as_str().ok_or(TopicError::InvalidDateValue)?;

    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    if value.len() >= 3 {
        let offset = &value[value.len() - 3..];
        if (offset.starts_with('+') || offset.starts_with('-'))
            && offset[1..].bytes().all(|byte| byte.is_ascii_digit())
        {
            let normalized = format!("{value}:00");
            if let Ok(timestamp) = DateTime::parse_from_rfc3339(&normalized) {
                return Ok(timestamp.with_timezone(&Utc));
            }
        }
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc2822(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.fZ",
        "%Y-%-m-%-dT%H:%M:%S%.fZ",
        "%m/%d/%Y, %I:%M:%S %p",
    ] {
        if let Ok(timestamp) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(timestamp.and_utc());
        }
    }
    for format in ["%Y-%m-%d", "%m/%d/%Y", "%a %b %d %Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(value, format) {
            return date
                .and_hms_opt(0, 0, 0)
                .map(|timestamp| timestamp.and_utc())
                .ok_or(TopicError::InvalidDateValue);
        }
    }
    Err(TopicError::InvalidDateValue)
}

fn watched_prefix(prefix: &str) -> bool {
    matches!(prefix, "update_ents" | "delete_ents")
}

fn apply_entity_payload(snapshot: &mut EntitySnapshot, content: &str) -> Result<(), TopicError> {
    let entities: Vec<(String, Uuid, HashMap<Uuid, Value>)> =
        serde_json::from_str(content).map_err(|_| TopicError::InvalidEntityLog)?;
    for (etype, entity_id, attributes) in entities {
        snapshot
            .entry(etype)
            .or_default()
            .insert(entity_id, attributes);
    }
    Ok(())
}

fn remove_snapshot_attribute(
    snapshot: &mut EntitySnapshot,
    etype: &str,
    entity_id: Uuid,
    attr_id: Uuid,
) {
    let remove_entity = snapshot
        .get_mut(etype)
        .and_then(|entities| entities.get_mut(&entity_id))
        .is_some_and(|attributes| {
            attributes.remove(&attr_id);
            attributes.is_empty()
        });
    if remove_entity {
        let remove_etype = snapshot.get_mut(etype).is_some_and(|entities| {
            entities.remove(&entity_id);
            entities.is_empty()
        });
        if remove_etype {
            snapshot.remove(etype);
        }
    }
}

fn bool_from_columns(columns: &[crate::WalColumn], name: &str) -> bool {
    column(columns, name).and_then(Value::as_bool) == Some(true)
}

fn string_from_columns<'a>(
    columns: &'a [crate::WalColumn],
    name: &'static str,
) -> Result<&'a str, TopicError> {
    column(columns, name)
        .and_then(Value::as_str)
        .ok_or(TopicError::InvalidColumn(name))
}

fn uuid_from_columns(columns: &[crate::WalColumn], name: &'static str) -> Result<Uuid, TopicError> {
    string_from_columns(columns, name)?
        .parse()
        .map_err(|_| TopicError::InvalidColumn(name))
}

fn column<'a>(columns: &'a [crate::WalColumn], name: &str) -> Option<&'a Value> {
    columns
        .iter()
        .find(|column| column.name == name)
        .map(|column| &column.value)
}

fn push_unique(topics: &mut Vec<Topic>, topic: Topic) {
    if !topics.contains(&topic) {
        topics.push(topic);
    }
}

#[derive(Debug, Error)]
pub enum TopicError {
    #[error("WAL change is missing required column {0}")]
    MissingColumn(&'static str),
    #[error("WAL change column {0} has an invalid value")]
    InvalidColumn(&'static str),
    #[error("WAL triple value is not valid JSON: {0}")]
    InvalidJsonValue(serde_json::Error),
    #[error("WAL reference triple does not contain a UUID JSON string")]
    InvalidReferenceValue,
    #[error("WAL checked date triple cannot be parsed")]
    InvalidDateValue,
    #[error("WAL entity-log payload is malformed")]
    InvalidEntityLog,
}

#[cfg(test)]
mod tests {
    use instant_instaql::{Attribute, AttributeValueType};
    use serde_json::json;

    use super::*;
    use crate::{Isn, Lsn, WalColumn};

    fn columns(values: Vec<(&str, Value)>) -> Vec<WalColumn> {
        values
            .into_iter()
            .map(|(name, value)| WalColumn {
                name: name.to_owned(),
                value,
            })
            .collect()
    }

    fn change(
        action: WalAction,
        table: &str,
        columns: Vec<WalColumn>,
        identity: Vec<WalColumn>,
    ) -> WalEntry {
        WalEntry {
            action,
            tx_bytes: 0,
            table: Some(table.to_owned()),
            columns,
            identity,
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn record() -> WalRecord {
        WalRecord {
            app_id: Uuid::nil(),
            tx_id: 1,
            isn: Isn::default(),
            previous_isn: None,
            tx_created_at: None,
            tx_bytes: 0,
            next_lsn: Lsn::default(),
            attr_changes: Vec::new(),
            ident_changes: Vec::new(),
            triple_changes: Vec::new(),
            messages: Vec::new(),
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }

    fn triple_columns(entity: Uuid, attr: Uuid, value: &str) -> Vec<WalColumn> {
        columns(vec![
            ("entity_id", json!(entity)),
            ("attr_id", json!(attr)),
            ("value", json!(value)),
            ("ea", json!(true)),
            ("eav", json!(false)),
            ("av", json!(false)),
            ("ave", json!(false)),
            ("vae", json!(false)),
        ])
    }

    fn entity_payload(entity: Uuid, attr: Uuid, value: &str) -> String {
        let attributes = serde_json::Map::from_iter([(attr.to_string(), json!(value))]);
        json!([["users", entity, Value::Object(attributes)]]).to_string()
    }

    #[test]
    fn triple_insert_decodes_json_and_index_flags() {
        let entity = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let mut record = record();
        record.triple_changes.push(change(
            WalAction::Insert,
            "triples",
            triple_columns(entity, attr, r#""New Movie""#),
            Vec::new(),
        ));

        assert_eq!(
            topics_for_record(&record).unwrap(),
            vec![Topic {
                indexes: vec![IndexKey::Ea],
                entities: MatchSet::Values(vec![entity]),
                attributes: MatchSet::Values(vec![attr]),
                values: MatchSet::Values(vec![TopicValue::Json(json!("New Movie"))]),
            }]
        );
    }

    #[test]
    fn triple_truncate_invalidates_every_query_shape() {
        let mut record = record();
        record.triple_changes.push(change(
            WalAction::Truncate,
            "triples",
            Vec::new(),
            Vec::new(),
        ));

        assert_eq!(
            topics_for_record(&record).unwrap(),
            vec![Topic {
                indexes: ALL_INDEXES.to_vec(),
                entities: MatchSet::Any,
                attributes: MatchSet::Any,
                values: MatchSet::Any,
            }]
        );
    }

    #[test]
    fn triple_update_unions_old_and_new_values_and_ignores_toasted_value() {
        let entity = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let old = triple_columns(entity, attr, r#""Old Movie""#);
        let new = triple_columns(entity, attr, r#""New Movie""#);
        let mut record = record();
        record
            .triple_changes
            .push(change(WalAction::Update, "triples", new, old.clone()));
        let topics = topics_for_record(&record).unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(
            topics[0].values,
            MatchSet::Values(vec![
                TopicValue::Json(json!("New Movie")),
                TopicValue::Json(json!("Old Movie")),
            ])
        );

        let mut toasted = triple_columns(entity, attr, "null");
        toasted.retain(|column| column.name != "value");
        record.triple_changes[0] = change(WalAction::Update, "triples", toasted, old);
        assert!(topics_for_record(&record).unwrap().is_empty());
    }

    #[test]
    fn reference_and_checked_date_values_are_typed() {
        let entity = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let target = Uuid::new_v4();
        let mut reference = triple_columns(entity, attr, &json!(target).to_string());
        *reference
            .iter_mut()
            .find(|column| column.name == "eav")
            .unwrap() = WalColumn {
            name: "eav".to_owned(),
            value: json!(true),
        };
        let mut record = record();
        record
            .triple_changes
            .push(change(WalAction::Insert, "triples", reference, Vec::new()));
        assert_eq!(
            topics_for_record(&record).unwrap()[0].values,
            MatchSet::Values(vec![TopicValue::Reference(target)])
        );

        let mut date = triple_columns(entity, attr, r#""2026-08-27T12:00:00Z""#);
        date.push(WalColumn {
            name: "checked_data_type".to_owned(),
            value: json!("date"),
        });
        record.triple_changes[0] = change(WalAction::Insert, "triples", date, Vec::new());
        assert!(matches!(
            topics_for_record(&record).unwrap()[0].values,
            MatchSet::Values(ref values) if matches!(values[0], TopicValue::Date(_))
        ));
    }

    #[test]
    fn schema_topics_dedupe_and_restore_adds_object_wildcard() {
        let attr = Uuid::new_v4();
        let mut record = record();
        record.ident_changes.push(change(
            WalAction::Update,
            "idents",
            columns(vec![("attr_id", json!(attr))]),
            Vec::new(),
        ));
        record.attr_changes.push(change(
            WalAction::Update,
            "attrs",
            columns(vec![
                ("id", json!(attr)),
                ("value_type", json!("blob")),
                ("deletion_marked_at", Value::Null),
            ]),
            columns(vec![(
                "deletion_marked_at",
                json!("2025-08-01 09:16:58.402186-07"),
            )]),
        ));

        let topics = topics_for_record(&record).unwrap();
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0], schema_topic(attr));
        assert_eq!(
            topics[1],
            Topic {
                indexes: vec![IndexKey::Ea],
                entities: MatchSet::Any,
                attributes: MatchSet::Any,
                values: MatchSet::Any,
            }
        );
    }

    #[test]
    fn entity_logs_reconstruct_after_and_triples_rewind_before() {
        let entity = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let mut record = record();

        let mut message = change(WalAction::Message, "", Vec::new(), Vec::new());
        message.prefix = Some("update_ents".to_owned());
        message.content = Some(entity_payload(entity, attr, "from-message"));
        record.messages.push(message);

        for (created_at, value) in [
            ("2026-08-27T12:00:02Z", "latest"),
            ("2026-08-27T12:00:01Z", "earlier"),
        ] {
            record.wal_logs.push(change(
                WalAction::Insert,
                "wal_logs_3",
                columns(vec![
                    ("prefix", json!("update_ents")),
                    ("content", json!(entity_payload(entity, attr, value))),
                    ("created_at", json!(created_at)),
                ]),
                Vec::new(),
            ));
        }

        record.triple_changes.push(change(
            WalAction::Update,
            "triples",
            triple_columns(entity, attr, r#""latest""#),
            triple_columns(entity, attr, r#""before""#),
        ));

        let catalog = AttributeCatalog {
            attributes: vec![Attribute {
                id: attr,
                wire: None,
                etype: "users".to_owned(),
                label: "handle".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: false,
                setting_unique: false,
                indexed: false,
                required: false,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        };

        let after = extract_entities_after(&record).unwrap();
        assert_eq!(after["users"][&entity][&attr], json!("latest"));
        let before = extract_entities_before(&catalog, after, &record).unwrap();
        assert_eq!(before["users"][&entity][&attr], json!("before"));
    }

    #[test]
    fn entity_log_transport_precedence_preserves_unrelated_before_image_fields() {
        let entity = Uuid::new_v4();
        let handle_attr = Uuid::new_v4();
        let role_attr = Uuid::new_v4();
        let mut record = record();
        let payload = |handle: &str, role: &str| {
            json!([[
                "users",
                entity,
                {
                    handle_attr.to_string(): handle,
                    role_attr.to_string(): role,
                }
            ]])
            .to_string()
        };

        let mut logical = change(WalAction::Message, "", Vec::new(), Vec::new());
        logical.prefix = Some("update_ents".to_owned());
        logical.content = Some(payload("logical", "member"));
        record.messages.push(logical);
        let mut ignored_message = change(WalAction::Message, "", Vec::new(), Vec::new());
        ignored_message.prefix = Some("other_prefix".to_owned());
        record.messages.push(ignored_message);

        record.wal_logs.push(change(
            WalAction::Insert,
            "wal_logs_7",
            columns(vec![
                ("prefix", json!("update_ents")),
                ("content", json!(payload("earlier-table", "editor"))),
                ("created_at", json!("2026-08-27T12:00:01Z")),
            ]),
            Vec::new(),
        ));
        record.wal_logs.push(change(
            WalAction::Insert,
            "wal_logs",
            columns(vec![
                ("prefix", json!("delete_ents")),
                ("content", json!(payload("latest-table", "admin"))),
                ("created_at", json!("2026-08-27T12:00:02Z")),
            ]),
            Vec::new(),
        ));
        record.wal_logs.push(change(
            WalAction::Delete,
            "wal_logs",
            Vec::new(),
            Vec::new(),
        ));
        record.triple_changes.push(change(
            WalAction::Insert,
            "triples",
            triple_columns(entity, handle_attr, r#""latest-table""#),
            Vec::new(),
        ));

        let catalog = AttributeCatalog {
            attributes: [(handle_attr, "handle"), (role_attr, "role")]
                .into_iter()
                .map(|(id, label)| Attribute {
                    id,
                    wire: None,
                    etype: "users".to_owned(),
                    label: label.to_owned(),
                    value_type: AttributeValueType::Blob,
                    cardinality: Cardinality::One,
                    unique: false,
                    setting_unique: false,
                    indexed: false,
                    required: false,
                    checked_data_type: None,
                    indexing: false,
                    checking_data_type: false,
                    reverse_etype: None,
                    reverse_label: None,
                    on_delete: None,
                    on_delete_reverse: None,
                })
                .collect(),
        };

        let after = extract_entities_after(&record).unwrap();
        assert_eq!(after["users"][&entity][&handle_attr], json!("latest-table"));
        assert_eq!(after["users"][&entity][&role_attr], json!("admin"));
        let before = extract_entities_before(&catalog, after, &record).unwrap();
        assert!(
            !before["users"][&entity].contains_key(&handle_attr),
            "rewinding an inserted cardinality-one triple removes only that attribute"
        );
        assert_eq!(before["users"][&entity][&role_attr], json!("admin"));
    }
}
