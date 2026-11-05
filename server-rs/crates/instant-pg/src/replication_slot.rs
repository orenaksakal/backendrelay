use std::time::Duration;

use instant_wal::{LogicalReplicationOptions, Lsn, ReplicationProtocolError, SlotPurpose};
use thiserror::Error;
use tokio_postgres::GenericClient;

const OUTPUT_PLUGIN: &str = "wal2json";

/// Control-plane state for one PostgreSQL logical replication slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalReplicationSlot {
    pub name: String,
    pub plugin: String,
    pub confirmed_flush_lsn: Lsn,
    pub active: bool,
    pub active_pid: Option<i32>,
    pub temporary: bool,
}

/// Result returned when PostgreSQL creates a logical slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedLogicalSlot {
    pub name: String,
    pub consistent_point: Lsn,
}

#[derive(Debug, Error)]
pub enum ReplicationSlotError {
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
    #[error(transparent)]
    Protocol(#[from] ReplicationProtocolError),
    #[error("replication slot query returned an invalid LSN `{0}`")]
    InvalidLsn(String),
    #[error("replication slot query returned no creation row")]
    MissingCreationRow,
    #[error("replication slot {slot_name} uses plugin {plugin}, expected wal2json")]
    WrongPlugin { slot_name: String, plugin: String },
    #[error("replication slot {0} is temporary and cannot survive process reconnects")]
    TemporarySlot(String),
    #[error(
        "slot {slot_name} is already confirmed at {confirmed}, later than requested start {requested}"
    )]
    ConfirmedPastRequestedStart {
        slot_name: String,
        requested: Lsn,
        confirmed: Lsn,
    },
}

/// Looks up a logical slot by name.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for PostgreSQL or LSN decoding failures.
pub async fn get_logical_replication_slot<C>(
    client: &C,
    slot_name: &str,
) -> Result<Option<LogicalReplicationSlot>, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    validate_slot_name(slot_name)?;
    let row = client
        .query_opt(
            "SELECT slot_name::text, plugin, confirmed_flush_lsn::text, active, active_pid, temporary \
             FROM pg_replication_slots WHERE slot_name = $1 AND slot_type = 'logical'",
            &[&slot_name],
        )
        .await?;
    row.map(|row| {
        let lsn = row.get::<_, String>("confirmed_flush_lsn");
        Ok(LogicalReplicationSlot {
            name: row.get("slot_name"),
            plugin: row.get("plugin"),
            confirmed_flush_lsn: lsn
                .parse()
                .map_err(|_| ReplicationSlotError::InvalidLsn(lsn))?,
            active: row.get("active"),
            active_pid: row.get("active_pid"),
            temporary: row.get("temporary"),
        })
    })
    .transpose()
}

/// Creates a permanent or session-temporary wal2json slot.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for an invalid name, PostgreSQL failure, missing
/// creation row, or malformed consistent point.
pub async fn create_logical_replication_slot<C>(
    client: &C,
    slot_name: &str,
    temporary: bool,
) -> Result<CreatedLogicalSlot, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    validate_slot_name(slot_name)?;
    let row = client
        .query_opt(
            "SELECT slot_name::text, lsn::text FROM pg_create_logical_replication_slot($1, $2, $3)",
            &[&slot_name, &OUTPUT_PLUGIN, &temporary],
        )
        .await?
        .ok_or(ReplicationSlotError::MissingCreationRow)?;
    let consistent_point = row.get::<_, String>("lsn");
    Ok(CreatedLogicalSlot {
        name: row.get("slot_name"),
        consistent_point: consistent_point
            .parse()
            .map_err(|_| ReplicationSlotError::InvalidLsn(consistent_point))?,
    })
}

/// Returns the existing slot or creates it if absent.
///
/// A duplicate-object race is resolved by reading the winner's slot after the
/// failed create, so multiple process starters converge without replacing a slot.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] if lookup or creation fails for a reason other
/// than another process winning the create race.
pub async fn ensure_logical_replication_slot<C>(
    client: &C,
    slot_name: &str,
) -> Result<LogicalReplicationSlot, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    if let Some(slot) = get_logical_replication_slot(client, slot_name).await? {
        return validate_wal2json_slot(slot);
    }
    match create_logical_replication_slot(client, slot_name, false).await {
        Ok(created) => validate_wal2json_slot(LogicalReplicationSlot {
            name: created.name,
            plugin: OUTPUT_PLUGIN.to_owned(),
            confirmed_flush_lsn: created.consistent_point,
            active: false,
            active_pid: None,
            temporary: false,
        }),
        Err(ReplicationSlotError::Postgres(error))
            if error.as_db_error().is_some_and(|db| {
                db.code() == &tokio_postgres::error::SqlState::DUPLICATE_OBJECT
            }) =>
        {
            let slot = get_logical_replication_slot(client, slot_name)
                .await?
                .ok_or(ReplicationSlotError::Postgres(error))?;
            validate_wal2json_slot(slot)
        }
        Err(error) => Err(error),
    }
}

fn validate_wal2json_slot(
    slot: LogicalReplicationSlot,
) -> Result<LogicalReplicationSlot, ReplicationSlotError> {
    if slot.plugin != OUTPUT_PLUGIN {
        return Err(ReplicationSlotError::WrongPlugin {
            slot_name: slot.name,
            plugin: slot.plugin,
        });
    }
    if slot.temporary {
        return Err(ReplicationSlotError::TemporarySlot(slot.name));
    }
    Ok(slot)
}

/// Drops an inactive logical replication slot.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for invalid names or PostgreSQL failures. An
/// active slot is rejected by PostgreSQL and is never force-terminated here.
pub async fn drop_logical_replication_slot<C>(
    client: &C,
    slot_name: &str,
) -> Result<(), ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    validate_slot_name(slot_name)?;
    client
        .execute("SELECT pg_drop_replication_slot($1)", &[&slot_name])
        .await?;
    Ok(())
}

/// Reports whether PostgreSQL has invalidated a logical slot's retained WAL.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for invalid names or PostgreSQL failures.
pub async fn replication_slot_is_lost<C>(
    client: &C,
    slot_name: &str,
) -> Result<bool, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    validate_slot_name(slot_name)?;
    Ok(client
        .query_opt(
            "SELECT wal_status = 'lost' AS lost FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot_name],
        )
        .await?
        .is_some_and(|row| row.get("lost")))
}

/// Drops an invalidated slot and creates a fresh wal2json slot.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for invalid names or PostgreSQL failures.
pub async fn recreate_invalidated_slot<C>(
    client: &C,
    slot_name: &str,
) -> Result<CreatedLogicalSlot, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    if get_logical_replication_slot(client, slot_name)
        .await?
        .is_some()
    {
        drop_logical_replication_slot(client, slot_name).await?;
    }
    create_logical_replication_slot(client, slot_name, false).await
}

/// Computes retained WAL bytes for a slot using PostgreSQL's LSN arithmetic.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for invalid names or PostgreSQL failures.
pub async fn replication_latency_bytes<C>(
    client: &C,
    slot_name: &str,
) -> Result<Option<i64>, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    validate_slot_name(slot_name)?;
    Ok(client
        .query_opt(
            "SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)::bigint AS bytes \
             FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot_name],
        )
        .await?
        .map(|row| row.get("bytes")))
}

/// Lists inactive permanent wal2json slots owned by per-machine invalidators.
///
/// The returned names are a snapshot. Callers should wait for a grace period
/// and pass that snapshot to [`cleanup_inactive_replication_slots`], which
/// atomically rechecks inactivity before dropping anything.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for PostgreSQL failures.
pub async fn inactive_invalidator_slots<C>(client: &C) -> Result<Vec<String>, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    Ok(client
        .query(
            "SELECT slot_name::text
             FROM pg_replication_slots
             WHERE active = false
               AND temporary = false
               AND plugin = 'wal2json'
               AND slot_name LIKE 'invalidator\\_%' ESCAPE '\\'
             ORDER BY slot_name",
            &[],
        )
        .await?
        .into_iter()
        .map(|row| row.get("slot_name"))
        .collect())
}

/// Lists inactive invalidator slots whose owning machine has stopped heartbeating.
///
/// A slot name still has to encode a UUID; malformed names are treated like a
/// machine without a row so they cannot pin WAL forever. Callers pass the
/// returned names to [`cleanup_inactive_replication_slots`].
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for PostgreSQL failures.
pub async fn stale_invalidator_slots<C>(
    client: &C,
    stale_after: Duration,
) -> Result<Vec<String>, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    let seconds = i64::try_from(stale_after.as_secs()).unwrap_or(i64::MAX);
    Ok(client
        .query(
            "SELECT slot.slot_name::text
             FROM pg_replication_slots AS slot
             LEFT JOIN rust_realtime_machines AS machine
               ON machine.machine_id = CASE
                    WHEN pg_input_is_valid(
                           regexp_replace(slot.slot_name, '^invalidator_', ''), 'uuid')
                      THEN regexp_replace(slot.slot_name, '^invalidator_', '')::uuid
                  END
             WHERE slot.active = false
               AND slot.temporary = false
               AND slot.plugin = 'wal2json'
               AND slot.slot_name LIKE 'invalidator\\_%' ESCAPE '\\'
               AND (machine.machine_id IS NULL
                    OR machine.updated_at <= now() - $1::bigint * interval '1 second')
             ORDER BY slot.slot_name",
            &[&seconds],
        )
        .await?
        .into_iter()
        .map(|row| row.get("slot_name"))
        .collect())
}

/// Drops only slots from a prior inactive snapshot that are still inactive.
///
/// Slots that became active during the caller's grace period are retained.
/// The returned names are exactly the slots PostgreSQL dropped.
///
/// # Errors
///
/// Returns [`ReplicationSlotError`] for invalid names or PostgreSQL failures.
pub async fn cleanup_inactive_replication_slots<C>(
    client: &C,
    slot_names: &[String],
) -> Result<Vec<String>, ReplicationSlotError>
where
    C: GenericClient + Sync,
{
    for slot_name in slot_names {
        validate_slot_name(slot_name)?;
    }
    if slot_names.is_empty() {
        return Ok(Vec::new());
    }
    Ok(client
        .query(
            "SELECT slot_name::text, pg_drop_replication_slot(slot_name)
             FROM pg_replication_slots
             WHERE active = false
               AND temporary = false
               AND plugin = 'wal2json'
               AND slot_name = ANY($1::text[])
             ORDER BY slot_name",
            &[&slot_names],
        )
        .await?
        .into_iter()
        .map(|row| row.get("slot_name"))
        .collect())
}

/// Protects the aggregator from silently starting after its requested checkpoint.
///
/// PostgreSQL starts a logical stream at the greater of the requested position and
/// `confirmed_flush_lsn`; a confirmed position ahead of the requested checkpoint
/// would therefore lose aggregation input.
///
/// # Errors
///
/// Returns [`ReplicationSlotError::ConfirmedPastRequestedStart`] when the slot has
/// advanced past the required start.
pub fn validate_replication_start(
    slot: &LogicalReplicationSlot,
    requested: Lsn,
    purpose: SlotPurpose,
) -> Result<(), ReplicationSlotError> {
    if purpose == SlotPurpose::Aggregator && requested < slot.confirmed_flush_lsn {
        return Err(ReplicationSlotError::ConfirmedPastRequestedStart {
            slot_name: slot.name.clone(),
            requested,
            confirmed: slot.confirmed_flush_lsn,
        });
    }
    Ok(())
}

fn validate_slot_name(slot_name: &str) -> Result<(), ReplicationProtocolError> {
    LogicalReplicationOptions::new(slot_name, Lsn::default(), SlotPurpose::Invalidator).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(confirmed: u64) -> LogicalReplicationSlot {
        LogicalReplicationSlot {
            name: "aggregator".to_owned(),
            plugin: OUTPUT_PLUGIN.to_owned(),
            confirmed_flush_lsn: Lsn::from_u64(confirmed),
            active: false,
            active_pid: None,
            temporary: false,
        }
    }

    #[test]
    fn aggregator_rejects_a_slot_ahead_of_its_checkpoint() {
        assert!(matches!(
            validate_replication_start(&slot(20), Lsn::from_u64(10), SlotPurpose::Aggregator),
            Err(ReplicationSlotError::ConfirmedPastRequestedStart { .. })
        ));
        assert!(
            validate_replication_start(&slot(20), Lsn::from_u64(20), SlotPurpose::Aggregator)
                .is_ok()
        );
        assert!(
            validate_replication_start(&slot(20), Lsn::from_u64(10), SlotPurpose::Invalidator)
                .is_ok()
        );
    }

    #[test]
    fn slot_names_share_the_protocol_injection_guard() {
        assert!(validate_slot_name("invalidator_node_1").is_ok());
        assert!(validate_slot_name("invalidator;drop_table").is_err());
    }

    #[test]
    fn permanent_wal2json_slots_are_the_only_compatible_runtime_slots() {
        assert!(validate_wal2json_slot(slot(10)).is_ok());
        let mut wrong_plugin = slot(10);
        wrong_plugin.plugin = "pgoutput".to_owned();
        assert!(matches!(
            validate_wal2json_slot(wrong_plugin),
            Err(ReplicationSlotError::WrongPlugin { .. })
        ));
        let mut temporary = slot(10);
        temporary.temporary = true;
        assert!(matches!(
            validate_wal2json_slot(temporary),
            Err(ReplicationSlotError::TemporarySlot(_))
        ));
    }
}
