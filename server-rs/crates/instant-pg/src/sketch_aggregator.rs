//! WAL-to-sketch delta transformation for the native attribute-sketch aggregator.

use std::collections::HashMap;

use futures_util::{StreamExt as _, pin_mut};
use instant_instaql::parse_date_millis;
use instant_wal::{Lsn, RawByopWalTransaction, WalAction, WalColumn};
use serde_json::Value;
use thiserror::Error;
use tokio_postgres::types::Json;
use tokio_postgres::{Client, IsolationLevel, Row};
use uuid::Uuid;

use crate::ExportedSnapshotSlot;

const MAX_QUERYABLE_JSON_STRING_UTF16_UNITS: usize = 20_000_000;
use crate::{
    AttrSketch, AttrSketchDecodeError, AttrSketchEncodeError, AttrSketchInput,
    AttrSketchMutationError, decode_attr_sketch_bins, encode_attr_sketch_bins,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SketchKey {
    pub app_id: Uuid,
    pub attr_id: Uuid,
}

/// Owned, hashable form of one sketch input. Floats are stored as IEEE bits so
/// batches can combine identical values without weakening their hash semantics.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SketchDatum {
    Nil,
    Long(i64),
    Double(u64),
    String(String),
    Boolean(bool),
    BigInteger(Vec<u8>),
    Unbinned,
}

impl SketchDatum {
    #[must_use]
    pub fn as_input(&self) -> Option<AttrSketchInput<'_>> {
        match self {
            Self::Nil => Some(AttrSketchInput::Nil),
            Self::Long(value) => Some(AttrSketchInput::Long(*value)),
            Self::Double(bits) => Some(AttrSketchInput::Double(f64::from_bits(*bits))),
            Self::String(value) => Some(AttrSketchInput::String(value)),
            Self::Boolean(value) => Some(AttrSketchInput::Boolean(*value)),
            Self::BigInteger(value) => Some(AttrSketchInput::BigInteger(value)),
            Self::Unbinned => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttrSketchDeltas {
    pub records: HashMap<SketchDatum, i64>,
    pub reverse_records: HashMap<SketchDatum, i64>,
    pub max_lsn: Lsn,
}

impl AttrSketchDeltas {
    /// Applies the combined transaction deltas to forward and reverse sketches.
    ///
    /// # Errors
    ///
    /// Returns a mutation error if a count would overflow or underflow.
    pub fn apply(
        &self,
        forward: &mut AttrSketch,
        reverse: &mut AttrSketch,
    ) -> Result<(), AttrSketchMutationError> {
        let mut next_forward = forward.clone();
        let mut next_reverse = reverse.clone();
        self.apply_in_place(&mut next_forward, &mut next_reverse)?;
        *forward = next_forward;
        *reverse = next_reverse;
        Ok(())
    }

    // Only for owned temporary sketches inside a database transaction. Any
    // failure discards these arrays and rolls back every write/checkpoint.
    fn apply_in_place(
        &self,
        forward: &mut AttrSketch,
        reverse: &mut AttrSketch,
    ) -> Result<(), AttrSketchMutationError> {
        for (datum, delta) in &self.records {
            forward.apply_delta(datum.as_input(), *delta)?;
        }
        for (datum, delta) in &self.reverse_records {
            reverse.apply_delta(datum.as_input(), *delta)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SketchDeltaBatch {
    pub changes: HashMap<SketchKey, AttrSketchDeltas>,
    pub lsn: Lsn,
    pub next_lsn: Lsn,
    pub tx_bytes: usize,
}

#[derive(Debug, Error)]
pub enum SketchWalError {
    #[error("triples WAL record is missing `{0}`")]
    MissingColumn(&'static str),
    #[error("triples WAL column `{column}` is not a valid UUID: {value}")]
    InvalidUuid { column: &'static str, value: Value },
    #[error("triples WAL value is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("checked data type `{checked_type}` does not match value {value}")]
    InvalidCheckedValue { checked_type: String, value: Value },
    #[error("triples WAL date value is invalid: {0}")]
    InvalidDate(Value),
    #[error("triples WAL action {0:?} cannot update a sketch")]
    InvalidAction(WalAction),
}

#[derive(Debug, Error)]
pub enum SketchPersistenceError {
    #[error("PostgreSQL sketch persistence failed: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("could not decode persisted attribute sketch: {0}")]
    Decode(#[from] AttrSketchDecodeError),
    #[error("could not encode attribute sketch: {0}")]
    Encode(#[from] AttrSketchEncodeError),
    #[error("could not apply attribute-sketch WAL delta: {0}")]
    Mutation(#[from] AttrSketchMutationError),
    #[error("persisted attribute sketch column `{0}` is outside its Rust range")]
    InvalidStoredCount(&'static str),
    #[error("WAL aggregator status for slot `{0}` is already initialized")]
    AlreadyInitialized(String),
    #[error(
        "WAL aggregator checkpoint for slot `{slot_name}` did not equal {expected}; slot ownership may have changed"
    )]
    CheckpointConflict { slot_name: String, expected: Lsn },
    #[error("PostgreSQL returned an unsafe exported snapshot identifier")]
    InvalidSnapshotName,
    #[error("sketch persistence batches must advance in strictly increasing commit order")]
    InvalidBatchSequence,
    #[error(transparent)]
    Wal(#[from] SketchWalError),
}

/// Streams the `triples` table from an exported replication-slot snapshot,
/// builds forward/reverse sketches in native Rust, and initializes the durable
/// slot checkpoint only after all snapshot rows have been persisted.
///
/// The `snapshot_slot` guard keeps PostgreSQL's exported snapshot alive until
/// the read transaction imports it. Existing partial bootstrap rows are safely
/// overwritten from the same authoritative snapshot.
///
/// # Errors
///
/// Returns [`SketchPersistenceError`] for an unsafe snapshot identifier,
/// PostgreSQL failure, malformed triple value, sketch mutation, or codec error.
pub async fn bootstrap_attr_sketches(
    snapshot_client: &mut Client,
    write_client: &Client,
    snapshot_slot: &ExportedSnapshotSlot,
    slot_name: &str,
    process_id: &str,
) -> Result<usize, SketchPersistenceError> {
    if snapshot_slot.snapshot_name.is_empty()
        || !snapshot_slot
            .snapshot_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(SketchPersistenceError::InvalidSnapshotName);
    }
    let transaction = snapshot_client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await?;
    transaction
        .batch_execute(&format!(
            "SET TRANSACTION SNAPSHOT '{}'",
            snapshot_slot.snapshot_name
        ))
        .await?;
    let rows = transaction
        .query_raw(
            "SELECT app_id, attr_id, entity_id, value, created_at,
                    checked_data_type::text, eav, ea
               FROM triples
              ORDER BY app_id, attr_id",
            std::iter::empty::<&str>(),
        )
        .await?;
    pin_mut!(rows);

    let mut current: Option<InitialSketch> = None;
    let mut sketch_count = 0_usize;
    while let Some(row) = rows.next().await {
        let row = row?;
        let key = SketchKey {
            app_id: row.get("app_id"),
            attr_id: row.get("attr_id"),
        };
        if current.as_ref().is_some_and(|sketch| sketch.key != key)
            && let Some(completed) = current.take()
        {
            persist_initial_sketch(write_client, completed, snapshot_slot.consistent_point).await?;
            sketch_count = sketch_count.saturating_add(1);
        }
        let sketch = current.get_or_insert_with(|| InitialSketch::new(key));
        let value = row.get::<_, Json<Value>>("value").0;
        let checked_type = row.get::<_, Option<String>>("checked_data_type");
        let datum = sketch_datum(&value, checked_type.as_deref(), Some(row.get("created_at")))?;
        sketch.forward.apply_delta(datum.as_input(), 1)?;
        if row.get::<_, bool>("eav") && !row.get::<_, bool>("ea") {
            let reverse = sketch.reverse.get_or_insert_with(AttrSketch::empty_default);
            let entity_id = row.get::<_, Uuid>("entity_id").to_string();
            reverse.apply_delta(Some(AttrSketchInput::String(&entity_id)), 1)?;
        }
    }
    if let Some(sketch) = current {
        persist_initial_sketch(write_client, sketch, snapshot_slot.consistent_point).await?;
        sketch_count = sketch_count.saturating_add(1);
    }
    transaction.commit().await?;
    initialize_sketch_aggregator_status(
        write_client,
        slot_name,
        process_id,
        snapshot_slot.consistent_point,
    )
    .await?;
    Ok(sketch_count)
}

struct InitialSketch {
    key: SketchKey,
    forward: AttrSketch,
    reverse: Option<AttrSketch>,
}

impl InitialSketch {
    fn new(key: SketchKey) -> Self {
        Self {
            key,
            forward: AttrSketch::empty_default(),
            reverse: None,
        }
    }
}

async fn persist_initial_sketch(
    client: &Client,
    sketch: InitialSketch,
    lsn: Lsn,
) -> Result<(), SketchPersistenceError> {
    let forward_bins = encode_attr_sketch_bins(&sketch.forward)?;
    let reverse_width = sketch
        .reverse
        .as_ref()
        .map(|value| usize_to_i32(value.width, "reverse_width"))
        .transpose()?;
    let reverse_depth = sketch
        .reverse
        .as_ref()
        .map(|value| usize_to_i32(value.depth, "reverse_depth"))
        .transpose()?;
    let reverse_total = sketch
        .reverse
        .as_ref()
        .map(|value| u64_to_i64(value.total, "reverse_total"))
        .transpose()?;
    let reverse_bins = sketch
        .reverse
        .as_ref()
        .map(encode_attr_sketch_bins)
        .transpose()?;
    let lsn = lsn.to_string();
    client
        .execute(
            "INSERT INTO attr_sketches
               (id, app_id, attr_id, width, depth, total, total_not_binned, bins,
                max_lsn, reverse_width, reverse_depth, reverse_total, reverse_bins)
             SELECT gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7,
                    $8::text::pg_lsn, $9, $10, $11, $12
               FROM apps JOIN attrs ON attrs.id = $2
              WHERE apps.id = $1
             ON CONFLICT (app_id, attr_id) DO UPDATE
             SET width = EXCLUDED.width, depth = EXCLUDED.depth,
                 total = EXCLUDED.total, total_not_binned = EXCLUDED.total_not_binned,
                 bins = EXCLUDED.bins, max_lsn = EXCLUDED.max_lsn,
                 reverse_width = EXCLUDED.reverse_width,
                 reverse_depth = EXCLUDED.reverse_depth,
                 reverse_total = EXCLUDED.reverse_total,
                 reverse_bins = EXCLUDED.reverse_bins",
            &[
                &sketch.key.app_id,
                &sketch.key.attr_id,
                &usize_to_i32(sketch.forward.width, "width")?,
                &usize_to_i32(sketch.forward.depth, "depth")?,
                &u64_to_i64(sketch.forward.total, "total")?,
                &u64_to_i64(sketch.forward.total_not_binned, "total_not_binned")?,
                &forward_bins,
                &lsn,
                &reverse_width,
                &reverse_depth,
                &reverse_total,
                &reverse_bins,
            ],
        )
        .await?;
    Ok(())
}

/// Reads the durable sketch-aggregator checkpoint for a replication slot.
///
/// # Errors
///
/// Returns [`SketchPersistenceError::Postgres`] when the query fails or the
/// persisted `pg_lsn` is malformed.
pub async fn sketch_aggregator_checkpoint(
    client: &Client,
    slot_name: &str,
) -> Result<Option<Lsn>, SketchPersistenceError> {
    let row = client
        .query_opt(
            "SELECT lsn::text FROM wal_aggregator_status WHERE slot_name = $1",
            &[&slot_name],
        )
        .await?;
    row.map(|row| row.get::<_, String>(0).parse())
        .transpose()
        .map_err(|_| SketchPersistenceError::InvalidStoredCount("lsn"))
}

/// Initializes the durable checkpoint after the replication slot snapshot has
/// been bootstrapped.
///
/// # Errors
///
/// Returns [`SketchPersistenceError::AlreadyInitialized`] if the slot already
/// has a checkpoint, or a PostgreSQL error if insertion fails.
pub async fn initialize_sketch_aggregator_status(
    client: &Client,
    slot_name: &str,
    process_id: &str,
    lsn: Lsn,
) -> Result<(), SketchPersistenceError> {
    let lsn = lsn.to_string();
    let inserted = client
        .execute(
            "INSERT INTO wal_aggregator_status (slot_name, process_id, lsn)
             VALUES ($1, $2, $3::text::pg_lsn)
             ON CONFLICT (slot_name) DO NOTHING",
            &[&slot_name, &process_id, &lsn],
        )
        .await?;
    if inserted != 1 {
        return Err(SketchPersistenceError::AlreadyInitialized(
            slot_name.to_owned(),
        ));
    }
    Ok(())
}

/// Applies a combined WAL batch and advances its durable checkpoint in one
/// PostgreSQL transaction. Missing sketch rows are created only while both the
/// app and attribute still exist. The checkpoint update uses a compare-and-swap
/// guard so a process that lost slot ownership cannot commit stale work.
///
/// # Errors
///
/// Returns [`SketchPersistenceError`] for database, codec, mutation, stored-range,
/// or checkpoint ownership failures. PostgreSQL rolls back all sketch writes on
/// every error.
pub async fn persist_sketch_delta_batch(
    client: &mut Client,
    slot_name: &str,
    process_id: &str,
    previous_lsn: Lsn,
    batch: &SketchDeltaBatch,
) -> Result<(), SketchPersistenceError> {
    persist_sketch_delta_batches(
        client,
        slot_name,
        process_id,
        previous_lsn,
        std::slice::from_ref(batch),
    )
    .await
}

/// Persists consecutive committed transactions atomically, decoding and encoding
/// each changed attribute once. Deltas are applied in original commit order,
/// never netted across transactions: an invalid intermediate count still fails.
/// The checkpoint advances only with all sketch writes in the same SQL commit.
/// Attribute creation, ordered locking and updates are each one bulk statement,
/// independent of the number of changed attributes in the persistence group.
///
/// # Errors
///
/// Returns on invalid commit order, database, codec, mutation or ownership error.
/// Every error rolls back the full group; no checkpoint may acknowledge a prefix.
pub async fn persist_sketch_delta_batches(
    client: &mut Client,
    slot_name: &str,
    process_id: &str,
    previous_lsn: Lsn,
    batches: &[SketchDeltaBatch],
) -> Result<(), SketchPersistenceError> {
    let Some(last) = batches.last() else {
        return Ok(());
    };
    let mut preceding = previous_lsn;
    let mut by_attribute: HashMap<SketchKey, Vec<&AttrSketchDeltas>> = HashMap::new();
    for batch in batches {
        if batch.lsn <= preceding {
            return Err(SketchPersistenceError::InvalidBatchSequence);
        }
        preceding = batch.lsn;
        for (key, deltas) in &batch.changes {
            by_attribute.entry(*key).or_default().push(deltas);
        }
    }
    let transaction = client.transaction().await?;
    let mut keys = by_attribute.keys().copied().collect::<Vec<_>>();
    keys.sort_by_key(|key| (key.app_id, key.attr_id));
    let app_ids = keys.iter().map(|key| key.app_id).collect::<Vec<_>>();
    let attr_ids = keys.iter().map(|key| key.attr_id).collect::<Vec<_>>();
    let rows = if keys.is_empty() {
        Vec::new()
    } else {
        let empty = AttrSketch::empty_default();
        let empty_bins = encode_attr_sketch_bins(&empty)?;
        transaction
            .execute(
                "INSERT INTO attr_sketches
                   (id, app_id, attr_id, width, depth, total, total_not_binned, bins)
                 SELECT gen_random_uuid(), keys.app_id, keys.attr_id, $3, $4, 0, 0, $5
                   FROM unnest($1::uuid[], $2::uuid[]) AS keys(app_id, attr_id)
                   JOIN apps ON apps.id = keys.app_id
                   JOIN attrs ON attrs.id = keys.attr_id AND attrs.app_id = keys.app_id
                  ORDER BY keys.app_id, keys.attr_id
                  ON CONFLICT (app_id, attr_id) DO NOTHING",
                &[
                    &app_ids,
                    &attr_ids,
                    &usize_to_i32(empty.width, "width")?,
                    &usize_to_i32(empty.depth, "depth")?,
                    &empty_bins,
                ],
            )
            .await?;
        transaction
            .query(
                "SELECT s.id, s.app_id, s.attr_id, s.width, s.depth, s.total,
                        s.total_not_binned, s.bins, s.reverse_width, s.reverse_depth,
                        s.reverse_total, s.reverse_bins
                   FROM attr_sketches AS s
                   JOIN unnest($1::uuid[], $2::uuid[]) AS keys(app_id, attr_id)
                     ON s.app_id = keys.app_id AND s.attr_id = keys.attr_id
                  ORDER BY s.app_id, s.attr_id
                  FOR UPDATE OF s",
                &[&app_ids, &attr_ids],
            )
            .await?
    };
    let mut writes = SketchWrites::default();
    for row in rows {
        let key = SketchKey {
            app_id: row.get("app_id"),
            attr_id: row.get("attr_id"),
        };
        let deltas = by_attribute
            .remove(&key)
            .expect("locked sketch belongs to the requested persistence group");
        let id: Uuid = row.get("id");
        let mut forward = decode_forward_sketch(&row)?;
        let existing_reverse = decode_reverse_sketch(&row)?;
        let persist_reverse = existing_reverse.is_some()
            || deltas.iter().any(|delta| !delta.reverse_records.is_empty());
        let mut reverse = existing_reverse.unwrap_or_else(AttrSketch::empty_default);
        for delta in &deltas {
            delta.apply_in_place(&mut forward, &mut reverse)?;
        }
        // Ordered application and decode validation above remain unconditional.
        // A direction untouched by every commit needs only the row's new LSN,
        // not a freshly allocated/recompressed/toasted copy of the same bins.
        let forward_bins = deltas
            .iter()
            .any(|delta| !delta.records.is_empty())
            .then(|| encode_attr_sketch_bins(&forward))
            .transpose()?;
        let reverse_width = persist_reverse
            .then(|| usize_to_i32(reverse.width, "reverse_width"))
            .transpose()?;
        let reverse_depth = persist_reverse
            .then(|| usize_to_i32(reverse.depth, "reverse_depth"))
            .transpose()?;
        let reverse_total = persist_reverse
            .then(|| u64_to_i64(reverse.total, "reverse_total"))
            .transpose()?;
        let reverse_bins = deltas
            .iter()
            .any(|delta| !delta.reverse_records.is_empty())
            .then(|| encode_attr_sketch_bins(&reverse))
            .transpose()?;
        let max_lsn = deltas
            .iter()
            .map(|delta| delta.max_lsn)
            .max()
            .unwrap_or_default()
            .to_string();
        writes.ids.push(id);
        writes.widths.push(usize_to_i32(forward.width, "width")?);
        writes.depths.push(usize_to_i32(forward.depth, "depth")?);
        writes.totals.push(u64_to_i64(forward.total, "total")?);
        writes
            .unbinned
            .push(u64_to_i64(forward.total_not_binned, "total_not_binned")?);
        writes.bins.push(forward_bins);
        writes.max_lsns.push(max_lsn);
        writes.reverse_widths.push(reverse_width);
        writes.reverse_depths.push(reverse_depth);
        writes.reverse_totals.push(reverse_total);
        writes.reverse_bins.push(reverse_bins);
    }
    if !writes.ids.is_empty() {
        transaction.execute(
                "UPDATE attr_sketches AS s
                    SET width = u.width, depth = u.depth, total = u.total,
                        total_not_binned = u.total_not_binned,
                        bins = CASE
                            WHEN u.bins IS NOT NULL AND s.bins IS DISTINCT FROM u.bins
                            THEN u.bins ELSE s.bins END,
                        max_lsn = u.max_lsn::pg_lsn,
                        reverse_width = u.reverse_width, reverse_depth = u.reverse_depth,
                        reverse_total = u.reverse_total,
                        reverse_bins = CASE
                            WHEN u.reverse_bins IS NOT NULL AND s.reverse_bins IS DISTINCT FROM u.reverse_bins
                            THEN u.reverse_bins ELSE s.reverse_bins END
                   FROM unnest($1::uuid[], $2::int[], $3::int[], $4::bigint[],
                               $5::bigint[], $6::bytea[], $7::text[], $8::int[],
                               $9::int[], $10::bigint[], $11::bytea[])
                     AS u(id, width, depth, total, total_not_binned, bins, max_lsn,
                          reverse_width, reverse_depth, reverse_total, reverse_bins)
                  WHERE s.id = u.id",
                &[
                    &writes.ids, &writes.widths, &writes.depths, &writes.totals,
                    &writes.unbinned, &writes.bins, &writes.max_lsns,
                    &writes.reverse_widths, &writes.reverse_depths,
                    &writes.reverse_totals, &writes.reverse_bins,
                ],
            )
            .await?;
    }

    let previous_lsn_text = previous_lsn.to_string();
    let next_lsn_text = last.lsn.to_string();
    let updated = transaction
        .execute(
            "UPDATE wal_aggregator_status
                SET lsn = $2::text::pg_lsn, process_id = $3
              WHERE slot_name = $1 AND lsn = $4::text::pg_lsn",
            &[&slot_name, &next_lsn_text, &process_id, &previous_lsn_text],
        )
        .await?;
    if updated != 1 {
        return Err(SketchPersistenceError::CheckpointConflict {
            slot_name: slot_name.to_owned(),
            expected: previous_lsn,
        });
    }
    transaction.commit().await?;
    Ok(())
}

#[derive(Default)]
struct SketchWrites {
    ids: Vec<Uuid>,
    widths: Vec<i32>,
    depths: Vec<i32>,
    totals: Vec<i64>,
    unbinned: Vec<i64>,
    bins: Vec<Option<Vec<u8>>>,
    max_lsns: Vec<String>,
    reverse_widths: Vec<Option<i32>>,
    reverse_depths: Vec<Option<i32>>,
    reverse_totals: Vec<Option<i64>>,
    reverse_bins: Vec<Option<Vec<u8>>>,
}

fn decode_forward_sketch(row: &Row) -> Result<AttrSketch, SketchPersistenceError> {
    let width = positive_usize(row.get::<_, i32>("width"), "width")?;
    let depth = positive_usize(row.get::<_, i32>("depth"), "depth")?;
    let bins: Vec<u8> = row.get("bins");
    Ok(AttrSketch {
        width,
        depth,
        bins: decode_attr_sketch_bins(width, depth, &bins)?,
        total: nonnegative_u64(row.get::<_, i64>("total"), "total")?,
        total_not_binned: nonnegative_u64(
            row.get::<_, i64>("total_not_binned"),
            "total_not_binned",
        )?,
    })
}

fn decode_reverse_sketch(row: &Row) -> Result<Option<AttrSketch>, SketchPersistenceError> {
    let Some(width) = row.get::<_, Option<i32>>("reverse_width") else {
        return Ok(None);
    };
    let depth = row
        .get::<_, Option<i32>>("reverse_depth")
        .ok_or(SketchPersistenceError::InvalidStoredCount("reverse_depth"))?;
    let total = row
        .get::<_, Option<i64>>("reverse_total")
        .ok_or(SketchPersistenceError::InvalidStoredCount("reverse_total"))?;
    let bins = row
        .get::<_, Option<Vec<u8>>>("reverse_bins")
        .ok_or(SketchPersistenceError::InvalidStoredCount("reverse_bins"))?;
    let width = positive_usize(width, "reverse_width")?;
    let depth = positive_usize(depth, "reverse_depth")?;
    Ok(Some(AttrSketch {
        width,
        depth,
        bins: decode_attr_sketch_bins(width, depth, &bins)?,
        total: nonnegative_u64(total, "reverse_total")?,
        total_not_binned: 0,
    }))
}

fn positive_usize(value: i32, column: &'static str) -> Result<usize, SketchPersistenceError> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(SketchPersistenceError::InvalidStoredCount(column))
}

fn nonnegative_u64(value: i64, column: &'static str) -> Result<u64, SketchPersistenceError> {
    u64::try_from(value).map_err(|_| SketchPersistenceError::InvalidStoredCount(column))
}

fn usize_to_i32(value: usize, column: &'static str) -> Result<i32, SketchPersistenceError> {
    i32::try_from(value).map_err(|_| SketchPersistenceError::InvalidStoredCount(column))
}

fn u64_to_i64(value: u64, column: &'static str) -> Result<i64, SketchPersistenceError> {
    i64::try_from(value).map_err(|_| SketchPersistenceError::InvalidStoredCount(column))
}

#[derive(Clone, Debug)]
struct TripleDatum {
    key: SketchKey,
    entity_id: Uuid,
    value: SketchDatum,
    reverse: bool,
}

/// Converts one committed arbitrary PostgreSQL transaction into combined sketch
/// deltas. Transactions at or below `start_lsn` are omitted after a restart.
/// Non-`triples` changes produce a checkpoint-only batch after 16 MiB of WAL.
/// Persisting that batch before acknowledgement bounds replay without checkpoint
/// writes feeding themselves indefinitely. Smaller filtered gaps remain unacknowledged.
///
/// # Errors
///
/// Returns [`SketchWalError`] for malformed triple rows or unsupported actions.
pub fn aggregate_sketch_transaction(
    start_lsn: Lsn,
    transaction: &RawByopWalTransaction,
) -> Result<Option<SketchDeltaBatch>, SketchWalError> {
    if transaction.lsn <= start_lsn {
        return Ok(None);
    }
    let mut batch = SketchDeltaBatch {
        changes: HashMap::new(),
        lsn: transaction.lsn,
        next_lsn: transaction.next_lsn,
        tx_bytes: transaction.tx_bytes,
    };
    for entry in transaction
        .changes
        .iter()
        .filter(|entry| entry.table.as_deref() == Some("triples"))
    {
        match entry.action {
            WalAction::Insert => add_entry(&mut batch, parse_triple(&entry.columns, None)?, 1),
            WalAction::Delete => add_entry(&mut batch, parse_triple(&entry.identity, None)?, -1),
            WalAction::Update => {
                let old = parse_triple(&entry.identity, None)?;
                let new = parse_triple(&entry.columns, Some(&entry.identity))?;
                add_entry(&mut batch, old, -1);
                add_entry(&mut batch, new, 1);
            }
            _ => return Err(SketchWalError::InvalidAction(entry.action.clone())),
        }
    }
    if batch.changes.is_empty()
        && transaction.lsn.as_u64().saturating_sub(start_lsn.as_u64()) < 16 * 1024 * 1024
    {
        Ok(None)
    } else {
        Ok(Some(batch))
    }
}

fn add_entry(batch: &mut SketchDeltaBatch, triple: TripleDatum, delta: i64) {
    let changes = batch.changes.entry(triple.key).or_default();
    changes.max_lsn = changes.max_lsn.max(batch.lsn);
    add_count(&mut changes.records, triple.value, delta);
    if triple.reverse {
        add_count(
            &mut changes.reverse_records,
            SketchDatum::String(triple.entity_id.to_string()),
            delta,
        );
    }
}

fn add_count(records: &mut HashMap<SketchDatum, i64>, datum: SketchDatum, delta: i64) {
    let next = records.get(&datum).copied().unwrap_or_default() + delta;
    if next == 0 {
        records.remove(&datum);
    } else {
        records.insert(datum, next);
    }
}

fn parse_triple(
    columns: &[WalColumn],
    identity_fallback: Option<&[WalColumn]>,
) -> Result<TripleDatum, SketchWalError> {
    let app_id = uuid_column(columns, "app_id")?;
    let attr_id = uuid_column(columns, "attr_id")?;
    let entity_id = uuid_column(columns, "entity_id")?;
    let raw_value = column(columns, "value")
        .or_else(|| identity_fallback.and_then(|identity| column(identity, "value")))
        .ok_or(SketchWalError::MissingColumn("value"))?;
    let value = decode_jsonb(raw_value)?;
    let checked_type = string_column(columns, "checked_data_type").or_else(|| {
        identity_fallback.and_then(|identity| string_column(identity, "checked_data_type"))
    });
    let created_at = column(columns, "created_at")
        .or_else(|| identity_fallback.and_then(|identity| column(identity, "created_at")))
        .and_then(Value::as_i64);
    let value = sketch_datum(&value, checked_type, created_at)?;
    Ok(TripleDatum {
        key: SketchKey { app_id, attr_id },
        entity_id,
        value,
        reverse: bool_column(columns, "eav") && !bool_column(columns, "ea"),
    })
}

fn column<'a>(columns: &'a [WalColumn], name: &str) -> Option<&'a Value> {
    columns
        .iter()
        .find(|column| column.name == name)
        .map(|column| &column.value)
}

fn uuid_column(columns: &[WalColumn], name: &'static str) -> Result<Uuid, SketchWalError> {
    let value = column(columns, name).ok_or(SketchWalError::MissingColumn(name))?;
    value
        .as_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| SketchWalError::InvalidUuid {
            column: name,
            value: value.clone(),
        })
}

fn string_column<'a>(columns: &'a [WalColumn], name: &str) -> Option<&'a str> {
    column(columns, name).and_then(Value::as_str)
}

fn bool_column(columns: &[WalColumn], name: &str) -> bool {
    column(columns, name).and_then(Value::as_bool) == Some(true)
}

fn decode_jsonb(raw: &Value) -> Result<Value, SketchWalError> {
    match raw {
        Value::String(value) => Ok(serde_json::from_str(value)?),
        value => Ok(value.clone()),
    }
}

fn sketch_datum(
    value: &Value,
    checked_type: Option<&str>,
    created_at: Option<i64>,
) -> Result<SketchDatum, SketchWalError> {
    if value.is_null() {
        return Ok(SketchDatum::Nil);
    }
    if value.as_str().is_some_and(|value| {
        value
            .encode_utf16()
            .take(MAX_QUERYABLE_JSON_STRING_UTF16_UNITS + 1)
            .count()
            > MAX_QUERYABLE_JSON_STRING_UTF16_UNITS
    }) {
        // The reference's inner JSON parser rejects strings beyond Jackson's
        // default 20M-character limit. It retains them in `total` but cannot bin
        // them; mirroring that avoids hashing hundreds of megabytes per WAL row.
        return Ok(SketchDatum::Unbinned);
    }
    match checked_type {
        Some("number") => number_datum(value),
        Some("string") => value
            .as_str()
            .map(|value| SketchDatum::String(value.to_owned()))
            .ok_or_else(|| invalid_checked("string", value)),
        Some("boolean") => value
            .as_bool()
            .map(SketchDatum::Boolean)
            .ok_or_else(|| invalid_checked("boolean", value)),
        Some("date") => parse_date_millis(
            value,
            created_at.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        )
        .map(SketchDatum::Long)
        .ok_or_else(|| SketchWalError::InvalidDate(value.clone())),
        _ => match value {
            Value::String(value) => Ok(SketchDatum::String(value.clone())),
            Value::Bool(value) => Ok(SketchDatum::Boolean(*value)),
            Value::Number(_) => number_datum(value),
            Value::Array(_) | Value::Object(_) => Ok(SketchDatum::Unbinned),
            Value::Null => Ok(SketchDatum::Nil),
        },
    }
}

fn number_datum(value: &Value) -> Result<SketchDatum, SketchWalError> {
    let Some(number) = value.as_number() else {
        return Err(invalid_checked("number", value));
    };
    if let Some(value) = number.as_i64() {
        return Ok(SketchDatum::Long(value));
    }
    if let Some(value) = number.as_u64() {
        return Ok(SketchDatum::BigInteger(positive_big_integer_bytes(
            value.to_be_bytes().to_vec(),
        )));
    }
    let lexical = number.to_string();
    if lexical
        .strip_prefix('-')
        .unwrap_or(&lexical)
        .bytes()
        .all(|byte| byte.is_ascii_digit())
    {
        return decimal_big_integer_bytes(&lexical)
            .map(SketchDatum::BigInteger)
            .ok_or_else(|| invalid_checked("number", value));
    }
    number.as_f64().map_or_else(
        || Err(invalid_checked("number", value)),
        |value| Ok(SketchDatum::Double(value.to_bits())),
    )
}

fn decimal_big_integer_bytes(value: &str) -> Option<Vec<u8>> {
    let (negative, digits) = value
        .strip_prefix('-')
        .map_or((false, value), |digits| (true, digits));
    if digits.is_empty() {
        return None;
    }
    let mut magnitude = vec![0_u8];
    for digit in digits.bytes() {
        let digit = digit.checked_sub(b'0').filter(|digit| *digit <= 9)?;
        let mut carry = u16::from(digit);
        for byte in magnitude.iter_mut().rev() {
            let next = u16::from(*byte) * 10 + carry;
            *byte = u8::try_from(next & 0xff).ok()?;
            carry = next >> 8;
        }
        if carry != 0 {
            magnitude.insert(0, u8::try_from(carry).ok()?);
        }
    }
    while magnitude.len() > 1 && magnitude[0] == 0 {
        magnitude.remove(0);
    }
    if !negative {
        return Some(positive_big_integer_bytes(magnitude));
    }
    for byte in &mut magnitude {
        *byte = !*byte;
    }
    let mut carry = 1_u16;
    for byte in magnitude.iter_mut().rev() {
        let next = u16::from(*byte) + carry;
        *byte = u8::try_from(next & 0xff).ok()?;
        carry = next >> 8;
    }
    if magnitude[0] & 0x80 == 0 {
        magnitude.insert(0, u8::MAX);
    }
    while magnitude.len() > 1 && magnitude[0] == u8::MAX && magnitude[1] & 0x80 != 0 {
        magnitude.remove(0);
    }
    Some(magnitude)
}

fn positive_big_integer_bytes(mut magnitude: Vec<u8>) -> Vec<u8> {
    while magnitude.len() > 1 && magnitude[0] == 0 {
        magnitude.remove(0);
    }
    if magnitude[0] & 0x80 != 0 {
        magnitude.insert(0, 0);
    }
    magnitude
}

fn invalid_checked(checked_type: &str, value: &Value) -> SketchWalError {
    SketchWalError::InvalidCheckedValue {
        checked_type: checked_type.to_owned(),
        value: value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use instant_wal::WalEntry;
    use serde_json::json;

    use super::*;

    const APP: Uuid = Uuid::from_u128(1);
    const ATTR: Uuid = Uuid::from_u128(2);
    const ENTITY: Uuid = Uuid::from_u128(3);

    fn columns(value: &Value, checked_type: Option<&str>, eav: bool, ea: bool) -> Vec<WalColumn> {
        let mut columns = vec![
            wal_column("app_id", json!(APP)),
            wal_column("attr_id", json!(ATTR)),
            wal_column("entity_id", json!(ENTITY)),
            wal_column("value", Value::String(value.to_string())),
            wal_column("eav", json!(eav)),
            wal_column("ea", json!(ea)),
            wal_column("created_at", json!(1_700_000_000_000_i64)),
        ];
        if let Some(checked_type) = checked_type {
            columns.push(wal_column("checked_data_type", json!(checked_type)));
        }
        columns
    }

    fn wal_column(name: &str, value: Value) -> WalColumn {
        WalColumn {
            name: name.to_owned(),
            value,
        }
    }

    fn transaction(changes: Vec<WalEntry>) -> RawByopWalTransaction {
        RawByopWalTransaction {
            changes,
            lsn: Lsn::from_u64(20),
            next_lsn: Lsn::from_u64(21),
            tx_bytes: 123,
        }
    }

    fn entry(action: WalAction, columns: Vec<WalColumn>, identity: Vec<WalColumn>) -> WalEntry {
        WalEntry {
            action,
            tx_bytes: 0,
            table: Some("triples".to_owned()),
            columns,
            identity,
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    #[test]
    fn filtered_checkpoint_batches_bound_wal_without_self_feedback() {
        let checkpoint = Lsn::from_u64(32 * 1024 * 1024);
        let boundary = checkpoint.as_u64() + 16 * 1024 * 1024;
        for lsn in [0, checkpoint.as_u64(), boundary - 1] {
            let transaction = RawByopWalTransaction {
                changes: vec![],
                lsn: Lsn::from_u64(lsn),
                next_lsn: Lsn::from_u64(lsn + 100),
                tx_bytes: 0,
            };
            assert!(
                aggregate_sketch_transaction(checkpoint, &transaction)
                    .unwrap()
                    .is_none()
            );
        }
        let transaction = RawByopWalTransaction {
            changes: vec![],
            lsn: Lsn::from_u64(boundary),
            next_lsn: Lsn::from_u64(boundary + 100),
            tx_bytes: 0,
        };
        let batch = aggregate_sketch_transaction(checkpoint, &transaction)
            .unwrap()
            .unwrap();
        assert!(batch.changes.is_empty());
        assert_eq!(batch.lsn, transaction.lsn);
        let own_write = RawByopWalTransaction {
            changes: vec![],
            lsn: Lsn::from_u64(boundary + 1024),
            next_lsn: Lsn::from_u64(boundary + 1124),
            tx_bytes: 0,
        };
        assert!(
            aggregate_sketch_transaction(batch.lsn, &own_write)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn combines_insert_delete_and_reverse_ref_deltas() {
        let inserted = columns(&json!("target"), None, true, false);
        let deleted = columns(&json!("old"), None, false, false);
        let tx = transaction(vec![
            entry(WalAction::Insert, inserted, vec![]),
            entry(WalAction::Delete, vec![], deleted),
        ]);
        let batch = aggregate_sketch_transaction(Lsn::from_u64(10), &tx)
            .unwrap()
            .unwrap();
        let deltas = &batch.changes[&SketchKey {
            app_id: APP,
            attr_id: ATTR,
        }];
        assert_eq!(deltas.records[&SketchDatum::String("target".into())], 1);
        assert_eq!(deltas.records[&SketchDatum::String("old".into())], -1);
        assert_eq!(
            deltas.reverse_records[&SketchDatum::String(ENTITY.to_string())],
            1
        );
        assert_eq!(batch.next_lsn, Lsn::from_u64(21));
        assert_eq!(batch.tx_bytes, 123);
    }

    #[test]
    fn update_uses_identity_value_when_toast_omits_the_new_value() {
        let identity = columns(&json!("same"), Some("string"), false, false);
        let mut updated = identity.clone();
        updated.retain(|column| column.name != "value");
        let tx = transaction(vec![entry(WalAction::Update, updated, identity)]);
        let batch = aggregate_sketch_transaction(Lsn::from_u64(10), &tx)
            .unwrap()
            .unwrap();
        assert!(
            batch
                .changes
                .values()
                .all(|change| change.records.is_empty())
        );
    }

    #[test]
    fn normalizes_dates_and_leaves_collections_unbinned() {
        assert_eq!(
            sketch_datum(&json!("2025-01-02T00:00:00-08"), Some("date"), None).unwrap(),
            SketchDatum::Long(1_735_804_800_000)
        );
        assert_eq!(
            sketch_datum(&json!({"large": "object"}), None, None).unwrap(),
            SketchDatum::Unbinned
        );
    }

    #[test]
    fn preserves_arbitrary_precision_json_integers_as_java_big_integer_bytes() {
        let positive: Value = serde_json::from_str("9223372036854775808").unwrap();
        assert_eq!(
            sketch_datum(&positive, Some("number"), None).unwrap(),
            SketchDatum::BigInteger(vec![0, 0x80, 0, 0, 0, 0, 0, 0, 0])
        );
        let negative: Value = serde_json::from_str("-9223372036854775809").unwrap();
        assert_eq!(
            sketch_datum(&negative, Some("number"), None).unwrap(),
            SketchDatum::BigInteger(vec![0xff, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
        );
    }

    #[test]
    fn skips_already_committed_transactions() {
        let tx = transaction(vec![]);
        assert!(
            aggregate_sketch_transaction(Lsn::from_u64(20), &tx)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn skips_transactions_without_triple_changes() {
        let tx = transaction(vec![]);
        assert!(
            aggregate_sketch_transaction(Lsn::from_u64(10), &tx)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn applies_a_batch_atomically_to_both_sketches() {
        let mut deltas = AttrSketchDeltas::default();
        deltas.records.insert(SketchDatum::Long(7), 1);
        deltas
            .reverse_records
            .insert(SketchDatum::String(ENTITY.to_string()), 1);
        let mut forward = AttrSketch::empty(257, 4).unwrap();
        let mut reverse = AttrSketch::empty(257, 4).unwrap();
        deltas.apply(&mut forward, &mut reverse).unwrap();
        assert_eq!(forward.estimate(AttrSketchInput::Long(7)), 1);
        assert_eq!(
            reverse.estimate(AttrSketchInput::String(&ENTITY.to_string())),
            1
        );
    }
}
