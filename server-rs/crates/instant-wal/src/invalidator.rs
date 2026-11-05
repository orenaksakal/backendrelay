use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{Isn, Lsn, RawWalTransaction, WalEntry};

const WAL_LOG_TABLES: [&str; 9] = [
    "wal_logs",
    "wal_logs_0",
    "wal_logs_1",
    "wal_logs_2",
    "wal_logs_3",
    "wal_logs_4",
    "wal_logs_5",
    "wal_logs_6",
    "wal_logs_7",
];

/// Application-scoped invalidation payload derived from one committed transaction.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WalRecord {
    pub app_id: Uuid,
    pub tx_id: i64,
    pub isn: Isn,
    pub previous_isn: Option<Isn>,
    pub tx_created_at: Option<DateTime<Utc>>,
    pub tx_bytes: usize,
    pub next_lsn: Lsn,
    pub attr_changes: Vec<WalEntry>,
    pub ident_changes: Vec<WalEntry>,
    pub triple_changes: Vec<WalEntry>,
    pub messages: Vec<WalEntry>,
    pub wal_logs: Vec<WalEntry>,
    pub combined_count: usize,
}

/// Groups supported tables and attaches application identity to a raw WAL transaction.
///
/// Returns `Ok(None)` for commits with no supported application changes, which is the
/// filtering behavior of the JVM invalidator transducer.
///
/// # Errors
///
/// Returns an error when the transaction row contains malformed required identifiers or
/// a malformed creation timestamp.
pub fn transform_wal_transaction(
    raw: RawWalTransaction,
) -> Result<Option<WalRecord>, WalTransformError> {
    let mut attr_changes = Vec::new();
    let mut ident_changes = Vec::new();
    let mut triple_changes = Vec::new();
    let mut transaction_changes = Vec::new();
    let mut wal_logs = Vec::new();

    for change in raw.changes {
        match change.table.as_deref() {
            Some("attrs") => attr_changes.push(change),
            Some("idents") => ident_changes.push(change),
            Some("triples") => triple_changes.push(change),
            Some("transactions") => transaction_changes.push(change),
            Some(table) if WAL_LOG_TABLES.contains(&table) => wal_logs.push(change),
            _ => {}
        }
    }

    let has_supported_changes = !attr_changes.is_empty()
        || !ident_changes.is_empty()
        || !triple_changes.is_empty()
        || !transaction_changes.is_empty();
    if !has_supported_changes {
        return Ok(None);
    }

    let Some(transaction) = transaction_changes.first() else {
        return Ok(None);
    };
    let app_id = parse_uuid_column(transaction, "app_id")?;
    let tx_id = parse_i64_column(transaction, "id")?;
    let tx_created_at = transaction
        .column("created_at")
        .map(parse_timestamp)
        .transpose()?;

    Ok(Some(WalRecord {
        app_id,
        tx_id,
        isn: raw.isn,
        previous_isn: raw.previous_isn,
        tx_created_at,
        tx_bytes: raw.tx_bytes,
        next_lsn: raw.next_lsn,
        attr_changes,
        ident_changes,
        triple_changes,
        messages: raw.messages,
        wal_logs,
        combined_count: 1,
    }))
}

/// Coalesces consecutive records for one app up to the configured batch limit.
///
/// The older creation timestamp and `previous_isn` are retained for latency and
/// continuity checks; transaction identity and WAL positions advance to the newer record.
///
/// # Errors
///
/// Returns [`WalTransformError::AppIdMismatch`] if two records under the limit belong
/// to different applications.
pub fn combine_wal_records(
    mut older: WalRecord,
    mut newer: WalRecord,
    batch_limit: usize,
) -> Result<Option<WalRecord>, WalTransformError> {
    if older.combined_count >= batch_limit {
        return Ok(None);
    }
    if older.app_id != newer.app_id {
        return Err(WalTransformError::AppIdMismatch {
            older: older.app_id,
            newer: newer.app_id,
        });
    }

    older.attr_changes.append(&mut newer.attr_changes);
    older.ident_changes.append(&mut newer.ident_changes);
    older.triple_changes.append(&mut newer.triple_changes);
    older.messages.append(&mut newer.messages);
    older.wal_logs.append(&mut newer.wal_logs);
    older.tx_bytes = older.tx_bytes.saturating_add(newer.tx_bytes);
    older.tx_id = newer.tx_id;
    older.isn = newer.isn;
    older.next_lsn = newer.next_lsn;
    older.combined_count = older.combined_count.saturating_add(newer.combined_count);
    Ok(Some(older))
}

fn parse_uuid_column(entry: &WalEntry, name: &'static str) -> Result<Uuid, WalTransformError> {
    let value = entry
        .column(name)
        .ok_or(WalTransformError::MissingColumn(name))?;
    let value = value
        .as_str()
        .ok_or(WalTransformError::InvalidColumn(name))?;
    value
        .parse()
        .map_err(|_| WalTransformError::InvalidColumn(name))
}

fn parse_i64_column(entry: &WalEntry, name: &'static str) -> Result<i64, WalTransformError> {
    let value = entry
        .column(name)
        .ok_or(WalTransformError::MissingColumn(name))?;
    match value {
        Value::Number(value) => value.as_i64().ok_or(WalTransformError::InvalidColumn(name)),
        Value::String(value) => value
            .parse()
            .map_err(|_| WalTransformError::InvalidColumn(name)),
        _ => Err(WalTransformError::InvalidColumn(name)),
    }
}

fn parse_timestamp(value: &Value) -> Result<DateTime<Utc>, WalTransformError> {
    let value = value
        .as_str()
        .ok_or(WalTransformError::InvalidColumn("created_at"))?;
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(timestamp) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(timestamp.and_utc());
        }
    }
    Err(WalTransformError::InvalidColumn("created_at"))
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WalTransformError {
    #[error("WAL transaction row is missing required column {0}")]
    MissingColumn(&'static str),
    #[error("WAL transaction column {0} has an invalid value")]
    InvalidColumn(&'static str),
    #[error("cannot combine WAL records for different apps: {older} and {newer}")]
    AppIdMismatch { older: Uuid, newer: Uuid },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{WalAction, WalColumn};

    fn change(table: &str, columns: Vec<(&str, Value)>) -> WalEntry {
        WalEntry {
            action: WalAction::Insert,
            tx_bytes: 0,
            table: Some(table.to_owned()),
            columns: columns
                .into_iter()
                .map(|(name, value)| WalColumn {
                    name: name.to_owned(),
                    value,
                })
                .collect(),
            identity: Vec::new(),
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn raw(app_id: Uuid, tx_id: i64, next: u64) -> RawWalTransaction {
        RawWalTransaction {
            changes: vec![
                change(
                    "transactions",
                    vec![
                        ("id", json!(tx_id)),
                        ("app_id", json!(app_id)),
                        ("created_at", json!("2026-08-27 12:34:56.123456")),
                    ],
                ),
                change("attrs", Vec::new()),
                change("idents", Vec::new()),
                change("triples", Vec::new()),
                change("wal_logs_3", Vec::new()),
                change("unrelated", Vec::new()),
            ],
            messages: vec![change("message-placeholder", Vec::new())],
            lsn: Lsn::from_u64(next - 1),
            next_lsn: Lsn::from_u64(next),
            isn: Isn::new(2, Lsn::from_u64(next)),
            previous_isn: Some(Isn::new(2, Lsn::from_u64(next - 10))),
            tx_bytes: 900,
            tx_id: Some(tx_id),
        }
    }

    #[test]
    fn transforms_supported_tables_and_transaction_metadata() {
        let app_id = Uuid::new_v4();
        let record = transform_wal_transaction(raw(app_id, 42, 100))
            .unwrap()
            .unwrap();

        assert_eq!(record.app_id, app_id);
        assert_eq!(record.tx_id, 42);
        assert_eq!(record.attr_changes.len(), 1);
        assert_eq!(record.ident_changes.len(), 1);
        assert_eq!(record.triple_changes.len(), 1);
        assert_eq!(record.wal_logs.len(), 1);
        assert_eq!(record.messages.len(), 1);
        assert_eq!(
            record.tx_created_at.unwrap().timestamp_subsec_micros(),
            123_456
        );
    }

    #[test]
    fn filters_records_without_supported_application_changes() {
        let raw = RawWalTransaction {
            changes: vec![change("webhooks", Vec::new())],
            messages: Vec::new(),
            lsn: Lsn::from_u64(1),
            next_lsn: Lsn::from_u64(2),
            isn: Isn::new(0, Lsn::from_u64(2)),
            previous_isn: None,
            tx_bytes: 10,
            tx_id: None,
        };
        assert!(transform_wal_transaction(raw).unwrap().is_none());
    }

    #[test]
    fn combines_same_app_and_preserves_oldest_continuity_metadata() {
        let app_id = Uuid::new_v4();
        let older = transform_wal_transaction(raw(app_id, 1, 100))
            .unwrap()
            .unwrap();
        let original_created_at = older.tx_created_at;
        let original_previous_isn = older.previous_isn;
        let newer = transform_wal_transaction(raw(app_id, 2, 200))
            .unwrap()
            .unwrap();

        let combined = combine_wal_records(older, newer, 500).unwrap().unwrap();
        assert_eq!(combined.tx_id, 2);
        assert_eq!(combined.next_lsn, Lsn::from_u64(200));
        assert_eq!(combined.tx_bytes, 1_800);
        assert_eq!(combined.combined_count, 2);
        assert_eq!(combined.tx_created_at, original_created_at);
        assert_eq!(combined.previous_isn, original_previous_isn);
        assert_eq!(combined.attr_changes.len(), 2);
    }
}
