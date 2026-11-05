//! PostgreSQL logical-WAL compatibility primitives for `InstantDB`.
//!
//! This crate owns the wire-level contracts shared by the transaction writer and
//! reactive invalidator: PostgreSQL LSNs, Instant sequence numbers, wal2json v2
//! entries, transaction assembly, and invalidation topics.

mod assembler;
mod cache_evict;
mod entry;
mod invalidator;
mod lsn;
mod replication;
mod topics;

pub use assembler::{
    ByopWalAssembler, RawByopWalTransaction, RawWalTransaction, WalAssembler, WalAssemblyError,
};
pub use cache_evict::{WalCacheEviction, cache_eviction_for_entry};
pub use entry::{WalAction, WalColumn, WalEntry, WalEntryError};
pub use invalidator::{
    WalRecord, WalTransformError, combine_wal_records, transform_wal_transaction,
};
pub use lsn::{Isn, IsnError, Lsn, LsnError, MaxSeenIsn};
pub use replication::{
    ByopReplicationStreamOutcome, ByopWal2JsonReplicationDecoder, FeedbackState,
    LogicalReplicationOptions, ReplicationFrame, ReplicationProtocolError, ReplicationStreamError,
    ReplicationStreamOutcome, SlotPurpose, Wal2JsonReplicationDecoder, decode_replication_frame,
    encode_standby_status,
};
pub use topics::{
    EntitySnapshot, MatchSet, Topic, TopicError, TopicValue, extract_entities_after,
    extract_entities_before, topics_for_record,
};
