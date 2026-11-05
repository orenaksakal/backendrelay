use serde_json::Value;
use thiserror::Error;

use crate::{Isn, Lsn, WalAction, WalEntry};

pub const WATCHED_MESSAGE_PREFIXES: [&str; 2] = ["update_ents", "delete_ents"];

/// A complete application transaction assembled from individual wal2json v2 entries.
#[derive(Clone, Debug, PartialEq)]
pub struct RawWalTransaction {
    pub changes: Vec<WalEntry>,
    pub messages: Vec<WalEntry>,
    pub lsn: Lsn,
    pub next_lsn: Lsn,
    pub isn: Isn,
    pub previous_isn: Option<Isn>,
    pub tx_bytes: usize,
    pub tx_id: Option<i64>,
}

/// A committed BYOP transaction. Unlike Instant's triple-store transactions,
/// arbitrary PostgreSQL transactions have no leading `transactions` row or ISN.
#[derive(Clone, Debug, PartialEq)]
pub struct RawByopWalTransaction {
    pub changes: Vec<WalEntry>,
    pub lsn: Lsn,
    pub next_lsn: Lsn,
    pub tx_bytes: usize,
}

/// Collects every row change between wal2json begin/close records for BYOP.
#[derive(Debug, Default)]
pub struct ByopWalAssembler {
    collecting: bool,
    changes: Vec<WalEntry>,
    tx_bytes: usize,
}

impl ByopWalAssembler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            collecting: false,
            changes: Vec::new(),
            tx_bytes: 0,
        }
    }

    /// Consumes one decoded wal2json entry and emits every committed BYOP row
    /// transaction without requiring Instant's application transaction marker.
    ///
    /// # Errors
    ///
    /// Returns an error when a close entry lacks its commit LSN fields.
    pub fn push(
        &mut self,
        entry: WalEntry,
    ) -> Result<Option<RawByopWalTransaction>, WalAssemblyError> {
        let entry_bytes = entry.tx_bytes;
        match entry.action {
            WalAction::Begin => {
                self.reset();
                self.collecting = true;
                self.tx_bytes = entry_bytes;
                Ok(None)
            }
            WalAction::Insert | WalAction::Update | WalAction::Delete | WalAction::Truncate
                if self.collecting =>
            {
                self.changes.push(entry);
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
                Ok(None)
            }
            WalAction::Close if self.collecting => {
                let lsn = entry.lsn.ok_or(WalAssemblyError::MissingCommitLsn)?;
                let next_lsn = entry.nextlsn.ok_or(WalAssemblyError::MissingNextLsn)?;
                let transaction = RawByopWalTransaction {
                    changes: std::mem::take(&mut self.changes),
                    lsn,
                    next_lsn,
                    tx_bytes: self.tx_bytes,
                };
                self.reset();
                Ok(Some(transaction))
            }
            _ => {
                if self.collecting {
                    self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
                }
                Ok(None)
            }
        }
    }

    fn reset(&mut self) {
        self.collecting = false;
        self.changes.clear();
        self.tx_bytes = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    AwaitBegin,
    AwaitTransaction,
    Collect,
    Ignore,
}

/// Stateful equivalent of `instant.jdbc.wal/produce` without stream I/O.
#[derive(Debug)]
pub struct WalAssembler {
    slot_num: i32,
    previous_isn: Option<Isn>,
    phase: Phase,
    changes: Vec<WalEntry>,
    messages: Vec<WalEntry>,
    tx_bytes: usize,
    tx_id: Option<i64>,
}

impl WalAssembler {
    #[must_use]
    pub const fn new(slot_num: i32, previous_isn: Option<Isn>) -> Self {
        Self {
            slot_num,
            previous_isn,
            phase: Phase::AwaitBegin,
            changes: Vec::new(),
            messages: Vec::new(),
            tx_bytes: 0,
            tx_id: None,
        }
    }

    #[must_use]
    pub const fn previous_isn(&self) -> Option<Isn> {
        self.previous_isn
    }

    /// Reports whether a begin record has opened a transaction that has not closed.
    #[must_use]
    pub(crate) const fn has_open_transaction(&self) -> bool {
        !matches!(self.phase, Phase::AwaitBegin)
    }

    /// Consumes one decoded WAL entry and emits an application transaction at commit.
    ///
    /// Transactions whose first row change is not an insert into `transactions` are
    /// intentionally ignored, matching the production JVM invalidator.
    ///
    /// # Errors
    ///
    /// Returns an error if an accepted transaction has a malformed transaction ID or
    /// its close entry does not carry both PostgreSQL LSN fields.
    pub fn push(&mut self, entry: WalEntry) -> Result<Option<RawWalTransaction>, WalAssemblyError> {
        let entry_bytes = entry.tx_bytes;
        match self.phase {
            Phase::AwaitBegin => {
                if entry.action == WalAction::Begin {
                    self.reset();
                    self.phase = Phase::AwaitTransaction;
                    self.tx_bytes = entry_bytes;
                }
                Ok(None)
            }
            Phase::AwaitTransaction => self.push_awaiting_transaction(entry, entry_bytes),
            Phase::Collect => self.push_collecting(entry, entry_bytes),
            Phase::Ignore => {
                if entry.action == WalAction::Close {
                    self.reset();
                } else if entry.action == WalAction::Begin {
                    self.reset_for_begin(entry_bytes);
                } else {
                    self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
                }
                Ok(None)
            }
        }
    }

    fn push_awaiting_transaction(
        &mut self,
        entry: WalEntry,
        entry_bytes: usize,
    ) -> Result<Option<RawWalTransaction>, WalAssemblyError> {
        match entry.action {
            WalAction::Insert if entry.table.as_deref() == Some("transactions") => {
                self.tx_id = Some(transaction_id(&entry)?);
                self.changes.push(entry);
                self.phase = Phase::Collect;
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
            }
            WalAction::Insert | WalAction::Update | WalAction::Delete => {
                self.phase = Phase::Ignore;
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
            }
            WalAction::Message if watched_message(&entry) => {
                self.messages.push(entry);
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
            }
            WalAction::Close => self.reset(),
            WalAction::Begin => self.reset_for_begin(entry_bytes),
            _ => self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes),
        }
        Ok(None)
    }

    fn push_collecting(
        &mut self,
        entry: WalEntry,
        entry_bytes: usize,
    ) -> Result<Option<RawWalTransaction>, WalAssemblyError> {
        match entry.action {
            WalAction::Insert | WalAction::Update | WalAction::Delete | WalAction::Truncate => {
                self.changes.push(entry);
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
                Ok(None)
            }
            WalAction::Message if watched_message(&entry) => {
                self.messages.push(entry);
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
                Ok(None)
            }
            WalAction::Close => self.finish(&entry),
            WalAction::Begin => {
                self.reset_for_begin(entry_bytes);
                Ok(None)
            }
            _ => {
                self.tx_bytes = self.tx_bytes.saturating_add(entry_bytes);
                Ok(None)
            }
        }
    }

    fn finish(&mut self, close: &WalEntry) -> Result<Option<RawWalTransaction>, WalAssemblyError> {
        let lsn = close.lsn.ok_or(WalAssemblyError::MissingCommitLsn)?;
        let next_lsn = close.nextlsn.ok_or(WalAssemblyError::MissingNextLsn)?;
        let isn = Isn::new(self.slot_num, next_lsn);
        let transaction = RawWalTransaction {
            changes: std::mem::take(&mut self.changes),
            messages: std::mem::take(&mut self.messages),
            lsn,
            next_lsn,
            isn,
            previous_isn: self.previous_isn,
            tx_bytes: self.tx_bytes,
            tx_id: self.tx_id.take(),
        };
        self.previous_isn = Some(isn);
        self.reset();
        Ok(Some(transaction))
    }

    fn reset_for_begin(&mut self, entry_bytes: usize) {
        self.reset();
        self.phase = Phase::AwaitTransaction;
        self.tx_bytes = entry_bytes;
    }

    fn reset(&mut self) {
        self.phase = Phase::AwaitBegin;
        self.changes.clear();
        self.messages.clear();
        self.tx_bytes = 0;
        self.tx_id = None;
    }
}

fn watched_message(entry: &WalEntry) -> bool {
    entry
        .prefix
        .as_deref()
        .is_some_and(|prefix| WATCHED_MESSAGE_PREFIXES.contains(&prefix))
}

fn transaction_id(entry: &WalEntry) -> Result<i64, WalAssemblyError> {
    let value = entry
        .columns
        .first()
        .map(|column| &column.value)
        .ok_or(WalAssemblyError::MissingTransactionId)?;
    match value {
        Value::Number(value) => value.as_i64().ok_or(WalAssemblyError::InvalidTransactionId),
        Value::String(value) => value
            .parse()
            .map_err(|_| WalAssemblyError::InvalidTransactionId),
        _ => Err(WalAssemblyError::InvalidTransactionId),
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WalAssemblyError {
    #[error("transactions WAL insert has no id column")]
    MissingTransactionId,
    #[error("transactions WAL id is not a signed 64-bit integer")]
    InvalidTransactionId,
    #[error("wal2json close entry has no lsn")]
    MissingCommitLsn,
    #[error("wal2json close entry has no nextlsn")]
    MissingNextLsn,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WalColumn;

    fn entry(action: WalAction, bytes: usize) -> WalEntry {
        WalEntry {
            action,
            tx_bytes: bytes,
            table: None,
            columns: Vec::new(),
            identity: Vec::new(),
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn transaction_insert(tx_id: i64, bytes: usize) -> WalEntry {
        let mut entry = entry(WalAction::Insert, bytes);
        entry.table = Some("transactions".to_owned());
        entry.columns.push(WalColumn {
            name: "id".to_owned(),
            value: Value::from(tx_id),
        });
        entry
    }

    #[test]
    fn assembles_application_transaction_and_advances_previous_isn() {
        let prior = Isn::new(7, Lsn::from_u64(100));
        let mut assembler = WalAssembler::new(7, Some(prior));
        assembler.push(entry(WalAction::Begin, 10)).unwrap();

        let mut message = entry(WalAction::Message, 20);
        message.prefix = Some("update_ents".to_owned());
        assembler.push(message).unwrap();
        assembler.push(transaction_insert(42, 30)).unwrap();

        let mut triple = entry(WalAction::Insert, 40);
        triple.table = Some("triples".to_owned());
        assembler.push(triple).unwrap();

        let mut close = entry(WalAction::Close, 50);
        close.lsn = Some(Lsn::from_u64(199));
        close.nextlsn = Some(Lsn::from_u64(200));
        let transaction = assembler.push(close).unwrap().unwrap();

        assert_eq!(transaction.tx_id, Some(42));
        assert_eq!(transaction.changes.len(), 2);
        assert_eq!(transaction.messages.len(), 1);
        assert_eq!(
            transaction.tx_bytes, 100,
            "commit bytes are excluded on the JVM"
        );
        assert_eq!(transaction.previous_isn, Some(prior));
        assert_eq!(transaction.isn, Isn::new(7, Lsn::from_u64(200)));
        assert_eq!(assembler.previous_isn(), Some(transaction.isn));
    }

    #[test]
    fn ignores_transactions_without_transaction_row_first() {
        let mut assembler = WalAssembler::new(1, None);
        assembler.push(entry(WalAction::Begin, 10)).unwrap();
        let mut triple = entry(WalAction::Insert, 20);
        triple.table = Some("triples".to_owned());
        assembler.push(triple).unwrap();
        assert!(
            assembler
                .push(entry(WalAction::Close, 30))
                .unwrap()
                .is_none()
        );
        assert_eq!(assembler.previous_isn(), None);
    }

    #[test]
    fn ignores_unwatched_messages() {
        let mut assembler = WalAssembler::new(1, None);
        assembler.push(entry(WalAction::Begin, 1)).unwrap();
        let mut message = entry(WalAction::Message, 2);
        message.prefix = Some("other".to_owned());
        assembler.push(message).unwrap();
        assembler.push(transaction_insert(1, 3)).unwrap();
        let mut close = entry(WalAction::Close, 4);
        close.lsn = Some(Lsn::from_u64(9));
        close.nextlsn = Some(Lsn::from_u64(10));
        assert!(assembler.push(close).unwrap().unwrap().messages.is_empty());
    }

    #[test]
    fn byop_assembler_collects_transactions_without_an_instant_marker() {
        let mut assembler = ByopWalAssembler::new();
        assembler.push(entry(WalAction::Begin, 10)).unwrap();
        let mut user = entry(WalAction::Insert, 20);
        user.table = Some("users".to_owned());
        assembler.push(user).unwrap();
        let mut close = entry(WalAction::Close, 30);
        close.lsn = Some(Lsn::from_u64(99));
        close.nextlsn = Some(Lsn::from_u64(100));
        let transaction = assembler.push(close).unwrap().unwrap();
        assert_eq!(transaction.changes.len(), 1);
        assert_eq!(transaction.lsn, Lsn::from_u64(99));
        assert_eq!(transaction.next_lsn, Lsn::from_u64(100));
        assert_eq!(transaction.tx_bytes, 30, "commit bytes are excluded");
    }

    #[test]
    fn byop_assembler_preserves_truncate_changes() {
        let mut assembler = ByopWalAssembler::new();
        assembler.push(entry(WalAction::Begin, 1)).unwrap();
        let mut truncate = entry(WalAction::Truncate, 2);
        truncate.table = Some("users".to_owned());
        assembler.push(truncate).unwrap();
        let mut close = entry(WalAction::Close, 3);
        close.lsn = Some(Lsn::from_u64(11));
        close.nextlsn = Some(Lsn::from_u64(12));
        let transaction = assembler.push(close).unwrap().unwrap();
        assert_eq!(transaction.changes.len(), 1);
        assert_eq!(transaction.changes[0].action, WalAction::Truncate);
        assert_eq!(transaction.changes[0].table.as_deref(), Some("users"));
    }

    #[test]
    fn application_assembler_preserves_truncate_after_transaction_marker() {
        let mut assembler = WalAssembler::new(1, None);
        assembler.push(entry(WalAction::Begin, 1)).unwrap();
        assembler.push(transaction_insert(7, 2)).unwrap();
        let mut truncate = entry(WalAction::Truncate, 3);
        truncate.table = Some("triples".to_owned());
        assembler.push(truncate).unwrap();
        let mut close = entry(WalAction::Close, 4);
        close.lsn = Some(Lsn::from_u64(21));
        close.nextlsn = Some(Lsn::from_u64(22));

        let transaction = assembler.push(close).unwrap().unwrap();
        assert_eq!(transaction.changes.len(), 2);
        assert_eq!(transaction.changes[1].action, WalAction::Truncate);
        assert_eq!(transaction.changes[1].table.as_deref(), Some("triples"));
        assert_eq!(transaction.tx_bytes, 6, "commit bytes are excluded");
    }
}
