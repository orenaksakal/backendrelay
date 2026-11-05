use chrono::{DateTime, TimeZone, Utc};
use thiserror::Error;

use crate::{
    ByopWalAssembler, Isn, Lsn, RawByopWalTransaction, RawWalTransaction, WalAssembler,
    WalAssemblyError, WalCacheEviction, WalEntry, WalEntryError, cache_eviction_for_entry,
};

const POSTGRES_EPOCH_UNIX_SECONDS: i64 = 946_684_800;
const XLOG_DATA_HEADER_LEN: usize = 25;
const KEEPALIVE_LEN: usize = 18;
const STANDBY_STATUS_LEN: usize = 34;

/// One server-to-client payload carried inside a PostgreSQL `CopyData` message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplicationFrame<'a> {
    XLogData {
        wal_start: Lsn,
        wal_end: Lsn,
        sent_at: DateTime<Utc>,
        data: &'a [u8],
    },
    KeepAlive {
        wal_end: Lsn,
        sent_at: DateTime<Utc>,
        reply_requested: bool,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ReplicationProtocolError {
    #[error("replication frame is empty")]
    EmptyFrame,
    #[error("replication frame {tag:#x} has {actual} bytes, expected at least {minimum}")]
    TruncatedFrame {
        tag: u8,
        actual: usize,
        minimum: usize,
    },
    #[error("unknown replication frame tag {0:#x}")]
    UnknownFrame(u8),
    #[error("replication frame reply flag must be 0 or 1, got {0}")]
    InvalidReplyFlag(u8),
    #[error("PostgreSQL replication timestamp is outside chrono's range: {0}")]
    InvalidTimestamp(i64),
    #[error("invalid replication slot name `{0}`")]
    InvalidSlotName(String),
    #[error("invalid qualified table name `{0}`")]
    InvalidTableName(String),
}

/// Decodes the payload of a server `CopyData` replication message.
///
/// # Errors
///
/// Returns [`ReplicationProtocolError`] for a truncated, unknown, or malformed frame.
pub fn decode_replication_frame(
    data: &[u8],
) -> Result<ReplicationFrame<'_>, ReplicationProtocolError> {
    let tag = *data.first().ok_or(ReplicationProtocolError::EmptyFrame)?;
    match tag {
        b'w' => {
            require_len(data, tag, XLOG_DATA_HEADER_LEN)?;
            Ok(ReplicationFrame::XLogData {
                wal_start: Lsn::from_u64(read_u64(data, 1)),
                wal_end: Lsn::from_u64(read_u64(data, 9)),
                sent_at: postgres_timestamp(read_i64(data, 17))?,
                data: &data[XLOG_DATA_HEADER_LEN..],
            })
        }
        b'k' => {
            require_len(data, tag, KEEPALIVE_LEN)?;
            let reply_requested = match data[17] {
                0 => false,
                1 => true,
                value => return Err(ReplicationProtocolError::InvalidReplyFlag(value)),
            };
            Ok(ReplicationFrame::KeepAlive {
                wal_end: Lsn::from_u64(read_u64(data, 1)),
                sent_at: postgres_timestamp(read_i64(data, 9))?,
                reply_requested,
            })
        }
        tag => Err(ReplicationProtocolError::UnknownFrame(tag)),
    }
}

/// Encodes a frontend standby-status payload for a PostgreSQL `CopyData` message.
#[must_use]
pub fn encode_standby_status(
    written: Lsn,
    flushed: Lsn,
    applied: Lsn,
    sent_at: DateTime<Utc>,
    reply_requested: bool,
) -> [u8; STANDBY_STATUS_LEN] {
    let mut message = [0_u8; STANDBY_STATUS_LEN];
    message[0] = b'r';
    message[1..9].copy_from_slice(&written.as_u64().to_be_bytes());
    message[9..17].copy_from_slice(&flushed.as_u64().to_be_bytes());
    message[17..25].copy_from_slice(&applied.as_u64().to_be_bytes());
    message[25..33].copy_from_slice(&postgres_microseconds(sent_at).to_be_bytes());
    message[33] = u8::from(reply_requested);
    message
}

/// Monotonic receive/durability positions used to generate standby feedback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FeedbackState {
    written: Lsn,
    flushed: Lsn,
    applied: Lsn,
}

/// Output from consuming one server replication frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplicationStreamOutcome {
    pub transaction: Option<RawWalTransaction>,
    /// Raw cache work emitted before transaction filtering/assembly.
    pub cache_eviction: Option<WalCacheEviction>,
    /// Commit boundary for an open transaction that produced no assembled record.
    pub filtered_commit_lsn: Option<Lsn>,
    /// Immediate standby reply requested by a primary keepalive.
    pub feedback: Option<[u8; STANDBY_STATUS_LEN]>,
}

/// Output from consuming one BYOP server replication frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ByopReplicationStreamOutcome {
    pub transaction: Option<RawByopWalTransaction>,
    pub feedback: Option<[u8; STANDBY_STATUS_LEN]>,
}

#[derive(Debug, Error)]
pub enum ReplicationStreamError {
    #[error(transparent)]
    Protocol(#[from] ReplicationProtocolError),
    #[error(transparent)]
    Entry(#[from] WalEntryError),
    #[error(transparent)]
    Assembly(#[from] WalAssemblyError),
}

/// Stateful wal2json frame decoder with explicit downstream acknowledgement.
#[derive(Debug)]
pub struct Wal2JsonReplicationDecoder {
    slot_num: i32,
    assembler: WalAssembler,
    feedback: FeedbackState,
}

impl Wal2JsonReplicationDecoder {
    #[must_use]
    pub fn new(slot_num: i32, previous_isn: Option<Isn>, confirmed_lsn: Lsn) -> Self {
        Self {
            slot_num,
            assembler: WalAssembler::new(slot_num, previous_isn),
            feedback: FeedbackState::new(confirmed_lsn, confirmed_lsn, confirmed_lsn),
        }
    }

    #[must_use]
    pub const fn previous_isn(&self) -> Option<Isn> {
        self.assembler.previous_isn()
    }

    #[must_use]
    pub const fn feedback_state(&self) -> FeedbackState {
        self.feedback
    }

    /// Consumes a server `CopyData` payload.
    ///
    /// A complete application transaction is returned only after its wal2json close
    /// entry. Receiving does not advance the flushed/applied positions; callers must
    /// invoke [`Self::acknowledge`] only after downstream work is durable.
    ///
    /// # Errors
    ///
    /// Returns [`ReplicationStreamError`] for malformed framing, JSON, or transaction
    /// boundaries.
    pub fn push(
        &mut self,
        payload: &[u8],
        reply_at: DateTime<Utc>,
    ) -> Result<ReplicationStreamOutcome, ReplicationStreamError> {
        match decode_replication_frame(payload)? {
            ReplicationFrame::XLogData { wal_end, data, .. } => {
                self.feedback.observe(wal_end);
                let entry = WalEntry::parse(data)?;
                let cache_eviction = cache_eviction_for_entry(&entry);
                let filtered_commit_lsn = if entry.action == crate::WalAction::Close
                    && self.assembler.has_open_transaction()
                {
                    Some(entry.nextlsn.ok_or(WalAssemblyError::MissingNextLsn)?)
                } else {
                    None
                };
                let transaction = self.assembler.push(entry)?;
                Ok(ReplicationStreamOutcome {
                    filtered_commit_lsn: filtered_commit_lsn.filter(|_| transaction.is_none()),
                    transaction,
                    cache_eviction,
                    feedback: None,
                })
            }
            ReplicationFrame::KeepAlive {
                wal_end,
                reply_requested,
                ..
            } => {
                self.feedback.observe(wal_end);
                Ok(ReplicationStreamOutcome {
                    transaction: None,
                    cache_eviction: None,
                    filtered_commit_lsn: None,
                    feedback: reply_requested.then(|| self.feedback.status(reply_at, false)),
                })
            }
        }
    }

    /// Advances the durable boundary after dispatch/history persistence completes.
    pub fn acknowledge(&mut self, durable_lsn: Lsn) {
        self.feedback.acknowledge(durable_lsn);
    }

    /// Discards any partial transaction and starts a fresh connection at the durable
    /// boundary while retaining committed ISN continuity.
    pub fn restart_after_disconnect(&mut self) {
        let previous_isn = self.assembler.previous_isn();
        let confirmed_lsn = self.feedback.flushed();
        self.assembler = WalAssembler::new(self.slot_num, previous_isn);
        self.feedback = FeedbackState::new(confirmed_lsn, confirmed_lsn, confirmed_lsn);
    }

    #[must_use]
    pub fn periodic_feedback(&self, sent_at: DateTime<Utc>) -> [u8; STANDBY_STATUS_LEN] {
        self.feedback.status(sent_at, false)
    }
}

/// Stateful wal2json decoder for arbitrary BYOP transactions. It shares the same
/// durable feedback gate but does not require Instant's `transactions` marker row.
#[derive(Debug)]
pub struct ByopWal2JsonReplicationDecoder {
    assembler: ByopWalAssembler,
    feedback: FeedbackState,
}

impl ByopWal2JsonReplicationDecoder {
    #[must_use]
    pub fn new(confirmed_lsn: Lsn) -> Self {
        Self {
            assembler: ByopWalAssembler::new(),
            feedback: FeedbackState::new(confirmed_lsn, confirmed_lsn, confirmed_lsn),
        }
    }

    #[must_use]
    pub const fn feedback_state(&self) -> FeedbackState {
        self.feedback
    }

    /// Consumes one PostgreSQL replication `CopyData` payload.
    ///
    /// # Errors
    ///
    /// Returns framing, JSON, or transaction-boundary errors.
    pub fn push(
        &mut self,
        payload: &[u8],
        reply_at: DateTime<Utc>,
    ) -> Result<ByopReplicationStreamOutcome, ReplicationStreamError> {
        match decode_replication_frame(payload)? {
            ReplicationFrame::XLogData { wal_end, data, .. } => {
                self.feedback.observe(wal_end);
                Ok(ByopReplicationStreamOutcome {
                    transaction: self.assembler.push(WalEntry::parse(data)?)?,
                    feedback: None,
                })
            }
            ReplicationFrame::KeepAlive {
                wal_end,
                reply_requested,
                ..
            } => {
                self.feedback.observe(wal_end);
                Ok(ByopReplicationStreamOutcome {
                    transaction: None,
                    feedback: reply_requested.then(|| self.feedback.status(reply_at, false)),
                })
            }
        }
    }

    pub fn acknowledge(&mut self, durable_lsn: Lsn) {
        self.feedback.acknowledge(durable_lsn);
    }

    pub fn restart_after_disconnect(&mut self) {
        let confirmed_lsn = self.feedback.flushed();
        self.assembler = ByopWalAssembler::new();
        self.feedback = FeedbackState::new(confirmed_lsn, confirmed_lsn, confirmed_lsn);
    }

    #[must_use]
    pub fn periodic_feedback(&self, sent_at: DateTime<Utc>) -> [u8; STANDBY_STATUS_LEN] {
        self.feedback.status(sent_at, false)
    }
}

impl FeedbackState {
    #[must_use]
    pub const fn new(written: Lsn, flushed: Lsn, applied: Lsn) -> Self {
        Self {
            written,
            flushed,
            applied,
        }
    }

    pub fn observe(&mut self, received: Lsn) {
        self.written = self.written.max(received);
    }

    /// Advances the durable boundary after downstream processing succeeds.
    pub fn acknowledge(&mut self, durable: Lsn) {
        // A caller can only acknowledge bytes that this connection has actually
        // received.  In particular, a durable sink may hand us a transaction's
        // `nextlsn` while the final CopyData frame advertised a smaller `wal_end`.
        // Never let that advance feedback past the receive boundary: PostgreSQL
        // interprets flushed/applied beyond written as a false replication claim.
        let durable = durable.min(self.written);
        self.flushed = self.flushed.max(durable).min(self.written);
        self.applied = self.applied.max(durable).min(self.flushed);
    }

    #[must_use]
    pub const fn written(self) -> Lsn {
        self.written
    }

    #[must_use]
    pub const fn flushed(self) -> Lsn {
        self.flushed
    }

    #[must_use]
    pub const fn applied(self) -> Lsn {
        self.applied
    }

    #[must_use]
    pub fn status(self, sent_at: DateTime<Utc>, reply_requested: bool) -> [u8; STANDBY_STATUS_LEN] {
        encode_standby_status(
            self.written,
            self.flushed,
            self.applied,
            sent_at,
            reply_requested,
        )
    }
}

/// Reference slot roles and their wal2json table filters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotPurpose {
    Invalidator,
    Aggregator,
    Byop,
}

impl SlotPurpose {
    #[must_use]
    pub const fn status_interval_seconds(self) -> u64 {
        match self {
            Self::Invalidator | Self::Byop => 1,
            Self::Aggregator => 30,
        }
    }

    #[must_use]
    pub const fn tables(self) -> &'static [&'static str] {
        match self {
            Self::Invalidator => &[
                "public.idents",
                "public.triples",
                "public.attrs",
                "public.transactions",
                "public.rules",
                "public.apps",
                "public.instant_users",
                "public.wal_logs",
                "public.wal_logs_0",
                "public.wal_logs_1",
                "public.wal_logs_2",
                "public.wal_logs_3",
                "public.wal_logs_4",
                "public.wal_logs_5",
                "public.wal_logs_6",
                "public.wal_logs_7",
                "public.webhooks",
            ],
            Self::Aggregator => &["public.triples"],
            Self::Byop => &[],
        }
    }
}

/// Validated wal2json logical-replication command options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalReplicationOptions {
    slot_name: String,
    start_lsn: Lsn,
    purpose: SlotPurpose,
    tables: Vec<String>,
}

impl LogicalReplicationOptions {
    /// Validates the slot name and constructs the reference wal2json configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ReplicationProtocolError::InvalidSlotName`] unless the name follows
    /// PostgreSQL's lowercase replication-slot naming contract.
    pub fn new(
        slot_name: impl Into<String>,
        start_lsn: Lsn,
        purpose: SlotPurpose,
    ) -> Result<Self, ReplicationProtocolError> {
        let slot_name = slot_name.into();
        if !valid_slot_name(&slot_name) {
            return Err(ReplicationProtocolError::InvalidSlotName(slot_name));
        }
        Ok(Self {
            slot_name,
            start_lsn,
            purpose,
            tables: purpose.tables().iter().map(ToString::to_string).collect(),
        })
    }

    /// Constructs a BYOP stream over the introspected tables in one PostgreSQL schema.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an invalid slot, schema, table name, or empty
    /// table set. Restricting names here prevents the wal2json option string from
    /// becoming an injection boundary.
    pub fn new_byop(
        slot_name: impl Into<String>,
        start_lsn: Lsn,
        schema: &str,
        tables: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, ReplicationProtocolError> {
        let mut options = Self::new(slot_name, start_lsn, SlotPurpose::Byop)?;
        options.tables = tables
            .into_iter()
            .map(|table| format!("{schema}.{}", table.as_ref()))
            .collect();
        if options.tables.is_empty() {
            return Err(ReplicationProtocolError::InvalidTableName(format!(
                "{schema}.<empty>"
            )));
        }
        options.validate_tables()?;
        Ok(options)
    }

    #[must_use]
    pub fn slot_name(&self) -> &str {
        &self.slot_name
    }

    #[must_use]
    pub const fn start_lsn(&self) -> Lsn {
        self.start_lsn
    }

    #[must_use]
    pub const fn purpose(&self) -> SlotPurpose {
        self.purpose
    }

    #[must_use]
    pub fn tables(&self) -> &[String] {
        &self.tables
    }

    /// Renders the replication-mode command sent after startup authentication.
    ///
    /// # Errors
    ///
    /// Returns [`ReplicationProtocolError::InvalidTableName`] if a configured table
    /// filter is not a safe two-part SQL identifier.
    pub fn start_command(&self) -> Result<String, ReplicationProtocolError> {
        self.validate_tables()?;
        Ok(format!(
            "START_REPLICATION SLOT {} LOGICAL {} (\"include-lsn\" 'true', \"format-version\" '2', \"add-tables\" '{}')",
            self.slot_name,
            self.start_lsn,
            self.tables.join(",")
        ))
    }

    fn validate_tables(&self) -> Result<(), ReplicationProtocolError> {
        for table in &self.tables {
            if !valid_table_name(table) {
                return Err(ReplicationProtocolError::InvalidTableName(table.clone()));
            }
        }
        Ok(())
    }
}

fn require_len(data: &[u8], tag: u8, minimum: usize) -> Result<(), ReplicationProtocolError> {
    if data.len() < minimum {
        Err(ReplicationProtocolError::TruncatedFrame {
            tag,
            actual: data.len(),
            minimum,
        })
    } else {
        Ok(())
    }
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        data[offset..offset + 8]
            .try_into()
            .expect("length was checked"),
    )
}

fn read_i64(data: &[u8], offset: usize) -> i64 {
    i64::from_be_bytes(
        data[offset..offset + 8]
            .try_into()
            .expect("length was checked"),
    )
}

fn postgres_timestamp(microseconds: i64) -> Result<DateTime<Utc>, ReplicationProtocolError> {
    let unix_microseconds =
        i128::from(POSTGRES_EPOCH_UNIX_SECONDS) * 1_000_000 + i128::from(microseconds);
    let seconds = unix_microseconds.div_euclid(1_000_000);
    let micros = unix_microseconds.rem_euclid(1_000_000);
    let seconds = i64::try_from(seconds)
        .map_err(|_| ReplicationProtocolError::InvalidTimestamp(microseconds))?;
    Utc.timestamp_opt(
        seconds,
        u32::try_from(micros).expect("microsecond remainder fits") * 1_000,
    )
    .single()
    .ok_or(ReplicationProtocolError::InvalidTimestamp(microseconds))
}

fn postgres_microseconds(timestamp: DateTime<Utc>) -> i64 {
    timestamp.timestamp_micros() - POSTGRES_EPOCH_UNIX_SECONDS * 1_000_000
}

fn valid_slot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_table_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(schema), Some(table), None) if valid(schema) && valid(table))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn xlog(wal_start: u64, wal_end: u64, json: &[u8]) -> Vec<u8> {
        let mut frame = vec![b'w'];
        frame.extend_from_slice(&wal_start.to_be_bytes());
        frame.extend_from_slice(&wal_end.to_be_bytes());
        frame.extend_from_slice(&0_i64.to_be_bytes());
        frame.extend_from_slice(json);
        frame
    }

    #[test]
    fn decodes_xlog_data_and_keepalive_frames() {
        let mut xlog = vec![b'w'];
        xlog.extend_from_slice(&0x10_u64.to_be_bytes());
        xlog.extend_from_slice(&0x20_u64.to_be_bytes());
        xlog.extend_from_slice(&1_500_000_i64.to_be_bytes());
        xlog.extend_from_slice(br#"{"action":"B"}"#);
        assert_eq!(
            decode_replication_frame(&xlog).unwrap(),
            ReplicationFrame::XLogData {
                wal_start: Lsn::from_u64(0x10),
                wal_end: Lsn::from_u64(0x20),
                sent_at: Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 1).unwrap()
                    + chrono::Duration::milliseconds(500),
                data: br#"{"action":"B"}"#,
            }
        );

        let mut keepalive = vec![b'k'];
        keepalive.extend_from_slice(&0x30_u64.to_be_bytes());
        keepalive.extend_from_slice(&0_i64.to_be_bytes());
        keepalive.push(1);
        assert_eq!(
            decode_replication_frame(&keepalive).unwrap(),
            ReplicationFrame::KeepAlive {
                wal_end: Lsn::from_u64(0x30),
                sent_at: Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(),
                reply_requested: true,
            }
        );
    }

    #[test]
    fn rejects_short_unknown_and_invalid_keepalive_frames() {
        assert_eq!(
            decode_replication_frame(&[]),
            Err(ReplicationProtocolError::EmptyFrame)
        );
        assert_eq!(
            decode_replication_frame(b"w"),
            Err(ReplicationProtocolError::TruncatedFrame {
                tag: b'w',
                actual: 1,
                minimum: XLOG_DATA_HEADER_LEN,
            })
        );
        assert_eq!(
            decode_replication_frame(b"x"),
            Err(ReplicationProtocolError::UnknownFrame(b'x'))
        );
        let mut keepalive = vec![b'k'];
        keepalive.extend_from_slice(&0_u64.to_be_bytes());
        keepalive.extend_from_slice(&0_i64.to_be_bytes());
        keepalive.push(2);
        assert_eq!(
            decode_replication_frame(&keepalive),
            Err(ReplicationProtocolError::InvalidReplyFlag(2))
        );
    }

    #[test]
    fn standby_status_is_big_endian_and_feedback_is_monotonic() {
        let now = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 2).unwrap();
        let mut feedback = FeedbackState::default();
        feedback.observe(Lsn::from_u64(10));
        feedback.observe(Lsn::from_u64(5));
        feedback.acknowledge(Lsn::from_u64(8));
        feedback.acknowledge(Lsn::from_u64(7));
        assert_eq!(feedback.written(), Lsn::from_u64(10));
        assert_eq!(feedback.flushed(), Lsn::from_u64(8));
        assert_eq!(feedback.applied(), Lsn::from_u64(8));
        let encoded = feedback.status(now, false);
        assert_eq!(encoded[0], b'r');
        assert_eq!(read_u64(&encoded, 1), 10);
        assert_eq!(read_u64(&encoded, 9), 8);
        assert_eq!(read_u64(&encoded, 17), 8);
        assert_eq!(read_i64(&encoded, 25), 2_000_000);
        assert_eq!(encoded[33], 0);
    }

    #[test]
    fn acknowledgement_cannot_claim_unreceived_wal() {
        let mut feedback =
            FeedbackState::new(Lsn::from_u64(100), Lsn::from_u64(80), Lsn::from_u64(80));

        // A sink acknowledgement must not make durable feedback outrun the
        // receive position advertised in the same standby status message.
        feedback.acknowledge(Lsn::from_u64(200));
        assert_eq!(feedback.written(), Lsn::from_u64(100));
        assert_eq!(feedback.flushed(), Lsn::from_u64(100));
        assert_eq!(feedback.applied(), Lsn::from_u64(100));

        // Older acknowledgements cannot move either watermark backwards.
        feedback.acknowledge(Lsn::from_u64(90));
        assert_eq!(feedback.flushed(), Lsn::from_u64(100));
        assert_eq!(feedback.applied(), Lsn::from_u64(100));
    }

    #[test]
    fn wal2json_start_commands_match_reference_slot_options() {
        let options = LogicalReplicationOptions::new(
            "invalidator_abc123",
            Lsn::from_u64(0x16_B6C50),
            SlotPurpose::Invalidator,
        )
        .unwrap();
        let command = options.start_command().unwrap();
        assert!(command.starts_with("START_REPLICATION SLOT invalidator_abc123 LOGICAL 0/16B6C50"));
        assert!(command.contains("\"include-lsn\" 'true'"));
        assert!(command.contains("\"format-version\" '2'"));
        assert!(command.contains("public.transactions"));
        assert!(command.contains("public.wal_logs_7"));
        assert_eq!(options.purpose().status_interval_seconds(), 1);

        let aggregate =
            LogicalReplicationOptions::new("aggregator", Lsn::default(), SlotPurpose::Aggregator)
                .unwrap();
        assert!(
            aggregate
                .start_command()
                .unwrap()
                .ends_with("\"add-tables\" 'public.triples')")
        );
        assert_eq!(aggregate.purpose().status_interval_seconds(), 30);
    }

    #[test]
    fn slot_names_are_not_an_injection_boundary() {
        for invalid in [
            "",
            "UPPER",
            "bad-name",
            "x;drop table apps",
            &"a".repeat(64),
        ] {
            assert!(matches!(
                LogicalReplicationOptions::new(invalid, Lsn::default(), SlotPurpose::Invalidator),
                Err(ReplicationProtocolError::InvalidSlotName(_))
            ));
        }
    }

    #[test]
    fn byop_start_commands_use_only_validated_introspected_tables() {
        let options = LogicalReplicationOptions::new_byop(
            "byop_abc123",
            Lsn::from_u64(42),
            "customer_schema",
            ["books", "authors"],
        )
        .unwrap();
        assert_eq!(options.purpose(), SlotPurpose::Byop);
        assert_eq!(options.purpose().status_interval_seconds(), 1);
        assert_eq!(
            options.tables(),
            &["customer_schema.books", "customer_schema.authors"]
        );
        assert!(
            options
                .start_command()
                .unwrap()
                .ends_with("\"add-tables\" 'customer_schema.books,customer_schema.authors')")
        );

        for (schema, tables) in [
            ("bad-schema", vec!["books"]),
            ("public", vec!["books,public.apps"]),
            ("public", Vec::new()),
        ] {
            assert!(matches!(
                LogicalReplicationOptions::new_byop("byop_abc123", Lsn::default(), schema, tables),
                Err(ReplicationProtocolError::InvalidTableName(_))
            ));
        }
    }

    #[test]
    fn stream_decoder_assembles_transactions_and_waits_for_durable_ack() {
        let prior = Isn::new(2, Lsn::from_u64(5));
        let confirmed = Lsn::from_u64(5);
        let mut decoder = Wal2JsonReplicationDecoder::new(2, Some(prior), confirmed);
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        for (index, json) in [
            br#"{"action":"B"}"#.as_slice(),
            br#"{"action":"I","table":"transactions","columns":[{"name":"id","value":42}]}"#,
            br#"{"action":"I","table":"triples","columns":[]}"#,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                decoder
                    .push(&xlog(10 + index as u64, 20 + index as u64, json), now)
                    .unwrap()
                    .transaction
                    .is_none()
            );
        }
        let outcome = decoder
            .push(
                &xlog(13, 23, br#"{"action":"C","lsn":"0/16","nextlsn":"0/17"}"#),
                now,
            )
            .unwrap();
        let transaction = outcome.transaction.unwrap();
        assert_eq!(transaction.tx_id, Some(42));
        assert_eq!(transaction.previous_isn, Some(prior));
        assert_eq!(transaction.next_lsn, Lsn::from_u64(23));
        assert_eq!(decoder.feedback_state().written(), Lsn::from_u64(23));
        assert_eq!(decoder.feedback_state().flushed(), confirmed);
        decoder.acknowledge(transaction.next_lsn);
        assert_eq!(decoder.feedback_state().flushed(), Lsn::from_u64(23));
        assert_eq!(decoder.previous_isn(), Some(Isn::new(2, Lsn::from_u64(23))));
    }

    #[test]
    fn cache_eviction_survives_a_transaction_the_assembler_filters() {
        let app_id = uuid::Uuid::new_v4();
        let attr_id = uuid::Uuid::new_v4();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let mut decoder = Wal2JsonReplicationDecoder::new(2, None, Lsn::default());
        decoder
            .push(&xlog(1, 1, br#"{"action":"B"}"#), now)
            .unwrap();

        // The reference listener evicts on every raw row before deciding that a
        // database transaction lacks Instant's leading `transactions` marker.
        let json = format!(
            r#"{{"action":"D","table":"attrs","identity":[{{"name":"app_id","value":"{app_id}"}},{{"name":"id","value":"{attr_id}"}}]}}"#
        );
        let outcome = decoder.push(&xlog(2, 2, json.as_bytes()), now).unwrap();
        assert_eq!(
            outcome.cache_eviction,
            Some(WalCacheEviction::Attribute {
                app_id: Some(app_id),
                attr_id: Some(attr_id),
            })
        );
        assert!(outcome.transaction.is_none());

        let close = decoder
            .push(
                &xlog(3, 3, br#"{"action":"C","lsn":"0/2","nextlsn":"0/3"}"#),
                now,
            )
            .unwrap();
        assert!(close.cache_eviction.is_none());
        assert!(close.transaction.is_none());
        assert_eq!(close.filtered_commit_lsn, Some(Lsn::from_u64(3)));
    }

    #[test]
    fn empty_commit_exposes_a_boundary_but_a_stray_close_does_not() {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let mut decoder = Wal2JsonReplicationDecoder::new(2, None, Lsn::default());
        decoder
            .push(&xlog(1, 1, br#"{"action":"B"}"#), now)
            .unwrap();
        let empty = decoder
            .push(
                &xlog(2, 2, br#"{"action":"C","lsn":"0/1","nextlsn":"0/2"}"#),
                now,
            )
            .unwrap();
        assert!(empty.transaction.is_none());
        assert_eq!(empty.filtered_commit_lsn, Some(Lsn::from_u64(2)));

        let stray = decoder
            .push(
                &xlog(3, 3, br#"{"action":"C","lsn":"0/2","nextlsn":"0/3"}"#),
                now,
            )
            .unwrap();
        assert!(stray.transaction.is_none());
        assert!(stray.filtered_commit_lsn.is_none());

        let second_stray = decoder
            .push(
                &xlog(4, 4, br#"{"action":"C","lsn":"0/3","nextlsn":"0/4"}"#),
                now,
            )
            .unwrap();
        assert!(second_stray.transaction.is_none());
        assert!(second_stray.filtered_commit_lsn.is_none());
    }

    #[test]
    fn byop_decoder_accepts_transactions_without_an_instant_marker() {
        let confirmed = Lsn::from_u64(5);
        let mut decoder = ByopWal2JsonReplicationDecoder::new(confirmed);
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        for (index, json) in [
            br#"{"action":"B"}"#.as_slice(),
            br#"{"action":"I","table":"users","columns":[{"name":"id","value":7}]}"#,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                decoder
                    .push(&xlog(10 + index as u64, 20 + index as u64, json), now)
                    .unwrap()
                    .transaction
                    .is_none()
            );
        }
        let transaction = decoder
            .push(
                &xlog(12, 22, br#"{"action":"C","lsn":"0/15","nextlsn":"0/16"}"#),
                now,
            )
            .unwrap()
            .transaction
            .unwrap();
        assert_eq!(transaction.changes[0].table.as_deref(), Some("users"));
        assert_eq!(decoder.feedback_state().flushed(), confirmed);
        decoder.acknowledge(transaction.next_lsn);
        assert_eq!(decoder.feedback_state().flushed(), Lsn::from_u64(22));
    }

    #[test]
    fn reply_requested_keepalive_emits_current_feedback() {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let mut decoder = Wal2JsonReplicationDecoder::new(2, None, Lsn::from_u64(5));
        let mut keepalive = vec![b'k'];
        keepalive.extend_from_slice(&10_u64.to_be_bytes());
        keepalive.extend_from_slice(&0_i64.to_be_bytes());
        keepalive.push(1);
        let feedback = decoder.push(&keepalive, now).unwrap().feedback.unwrap();
        assert_eq!(read_u64(&feedback, 1), 10);
        assert_eq!(read_u64(&feedback, 9), 5);
        assert_eq!(read_u64(&feedback, 17), 5);
        assert_eq!(feedback[33], 0);
    }

    #[test]
    fn reconnect_discards_partial_transaction_and_resumes_at_durable_lsn() {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let prior = Isn::new(2, Lsn::from_u64(5));
        let mut decoder = Wal2JsonReplicationDecoder::new(2, Some(prior), prior.lsn());
        decoder
            .push(&xlog(10, 20, br#"{"action":"B"}"#), now)
            .unwrap();
        decoder
            .push(
                &xlog(
                    11,
                    21,
                    br#"{"action":"I","table":"transactions","columns":[{"name":"id","value":99}]}"#,
                ),
                now,
            )
            .unwrap();

        decoder.restart_after_disconnect();
        assert_eq!(decoder.previous_isn(), Some(prior));
        assert_eq!(
            decoder.feedback_state(),
            FeedbackState::new(prior.lsn(), prior.lsn(), prior.lsn())
        );
        assert!(
            decoder
                .push(
                    &xlog(30, 40, br#"{"action":"C","lsn":"0/27","nextlsn":"0/28"}"#),
                    now
                )
                .unwrap()
                .transaction
                .is_none()
        );
    }
}
