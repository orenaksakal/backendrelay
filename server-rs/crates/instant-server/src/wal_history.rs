use std::future::Future;
use std::io::Cursor;
use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use instant_realtime::ReplicationPipelineOutcome;
use instant_wal::{Isn, WalAction, WalRecord};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tokio_postgres::{Client, types::Json};
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{
    DurableInvalidationSink, NoopWebhookLatencyRecorder, PostgresPool, PostgresPoolError,
    WebhookLatencyRecorder,
};

const HISTORY_MAGIC: &[u8] = b"instant-rust-wal\0\x01";
const PARTITION_BUCKET_COUNT: i64 = 13;
const MAX_PERSIST_BATCH_RECORDS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredWebhookEventKey {
    pub webhook_id: Uuid,
    pub isn: Isn,
    pub partition_bucket: i32,
}

/// Wakeup boundary for newly inserted webhook queue rows. The queue remains
/// durable when notification is lost, so periodic workers may safely recover.
pub trait WebhookEventNotifier: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn notify(
        &self,
        events: &[StoredWebhookEventKey],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopWebhookEventNotifier;

impl WebhookEventNotifier for NoopWebhookEventNotifier {
    type Error = std::convert::Infallible;

    fn notify(
        &self,
        _events: &[StoredWebhookEventKey],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Non-blocking notification channel used to wake the native webhook worker
/// after durable queue rows commit. A closed receiver deliberately degrades to
/// the periodic queue scan instead of preventing WAL acknowledgement.
#[derive(Clone, Debug)]
pub struct WebhookEventWakeupNotifier {
    wakeups: mpsc::UnboundedSender<()>,
}

impl WebhookEventWakeupNotifier {
    #[must_use]
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (wakeups, receiver) = mpsc::unbounded_channel();
        (Self { wakeups }, receiver)
    }
}

impl WebhookEventNotifier for WebhookEventWakeupNotifier {
    type Error = std::convert::Infallible;

    fn notify(
        &self,
        events: &[StoredWebhookEventKey],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        if !events.is_empty() {
            let _receiver_may_have_stopped = self.wakeups.send(());
        }
        std::future::ready(Ok(()))
    }
}

#[derive(Debug, Error)]
pub enum WalHistoryError {
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("WAL record has no transaction creation timestamp")]
    MissingCreatedAt,
    #[error("WAL triple change has no valid attr_id")]
    InvalidAttributeId,
    #[error("stored WAL record is not a supported Rust history payload")]
    UnsupportedPayload,
    #[error("WAL history serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("WAL history compression failed: {0}")]
    Compression(#[from] std::io::Error),
    #[error("legacy WAL history decoding failed: {0}")]
    Legacy(#[from] crate::LegacyNippyError),
    #[error("webhook event notification failed: {0}")]
    Notification(String),
    #[error("stored webhook ISN is invalid: {0}")]
    InvalidStoredIsn(#[from] instant_wal::IsnError),
}

/// PostgreSQL implementation of the replication acknowledgement gate. History
/// and matching webhook queue rows commit atomically before the slot may flush.
pub struct PostgresDurableInvalidationSink<N> {
    client: PostgresClientSource,
    notifier: Arc<N>,
    webhook_latency: Arc<dyn WebhookLatencyRecorder>,
    cluster_machine: Option<Uuid>,
}

impl<N> PostgresDurableInvalidationSink<N>
where
    N: WebhookEventNotifier,
{
    #[must_use]
    pub fn new(client: Client, notifier: Arc<N>) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), notifier)
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>, notifier: Arc<N>) -> Self {
        Self::from_source(PostgresClientSource::fixed_shared(client), notifier)
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, notifier: Arc<N>) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool), notifier)
    }

    /// Uses a dedicated single-connection pool for WAL history, webhook and
    /// cluster publication. Keeping it separate from the realtime room writer
    /// prevents presence or broadcast writes from queueing a WAL sink refresh.
    #[must_use]
    pub fn from_cluster_writer(pool: PostgresPool, notifier: Arc<N>, machine_id: Uuid) -> Self {
        let mut sink = Self::from_pool(pool, notifier);
        sink.cluster_machine = Some(machine_id);
        sink
    }

    fn from_source(client: PostgresClientSource, notifier: Arc<N>) -> Self {
        Self {
            client,
            notifier,
            webhook_latency: Arc::new(NoopWebhookLatencyRecorder),
            cluster_machine: None,
        }
    }

    #[must_use]
    pub fn with_webhook_latency_recorder(
        mut self,
        recorder: Arc<dyn WebhookLatencyRecorder>,
    ) -> Self {
        self.webhook_latency = recorder;
        self
    }

    /// Persists one record and its exact webhook matches in a single transaction.
    /// Records without entity-log messages are skipped like the JVM logger because
    /// they cannot produce replay payloads.
    ///
    /// # Errors
    ///
    /// Returns a database, record-shape, codec, or notification failure.
    pub async fn persist_record(
        &self,
        record: &WalRecord,
    ) -> Result<Vec<StoredWebhookEventKey>, WalHistoryError> {
        if record.messages.is_empty() && record.wal_logs.is_empty() {
            if let Some(machine_id) = self.cluster_machine {
                let client = self.client.acquire().await?;
                let statement = client
                    .prepare_cached(crate::postgres_realtime_cluster::PUBLISH_WAL_SQL)
                    .await?;
                client
                    .execute(
                        &statement,
                        &[
                            &machine_id,
                            &record.app_id,
                            &record.isn.to_string(),
                            &Json(record),
                        ],
                    )
                    .await?;
            }
            return Ok(Vec::new());
        }
        let started = tokio::time::Instant::now();
        let created_at = record
            .tx_created_at
            .ok_or(WalHistoryError::MissingCreatedAt)?;
        let partition_bucket = partition_bucket(created_at);
        let topics = topic_filter(record)?;
        let content = encode_wal_record(record)?;
        let [created_attrs, updated_attrs, deleted_attrs] = webhook_change_attrs(record);
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        let acquire_started = tokio::time::Instant::now();
        let client = self.client.acquire().await?;
        let acquire_ms = acquire_started.elapsed().as_secs_f64() * 1000.0;
        let sql_started = tokio::time::Instant::now();
        let sql = if self.cluster_machine.is_some() {
            PERSIST_AND_PUBLISH_SQL.as_str()
        } else {
            PERSIST_RECORD_SQL
        };
        let statement = client.prepare_cached(sql).await?;
        let isn = record.isn.to_postgres_composite();
        let room_id = record.isn.to_string();
        let payload = Json(record);
        let mut parameters: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![
            &isn,
            &record.app_id,
            &topics,
            &content,
            &partition_bucket,
            &created_attrs,
            &updated_attrs,
            &deleted_attrs,
        ];
        if let Some(machine_id) = self.cluster_machine.as_ref() {
            parameters.extend([
                machine_id as &(dyn tokio_postgres::types::ToSql + Sync),
                &room_id,
                &payload,
            ]);
        }
        let inserted = client
            .query(&statement, &parameters)
            .await?
            .into_iter()
            .filter_map(|row| {
                row.get::<_, Option<Uuid>>(0)
                    .map(|webhook_id| StoredWebhookEventKey {
                        webhook_id,
                        isn: record.isn,
                        partition_bucket,
                    })
            })
            .collect::<Vec<_>>();
        drop(client);
        let sql_ms = sql_started.elapsed().as_secs_f64() * 1000.0;
        tracing::debug!(
            target: "instant_server::realtime_latency",
            encode_ms, acquire_ms, sql_ms,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            webhook_count = inserted.len(),
            "realtime durable history persisted"
        );
        if !inserted.is_empty() {
            self.notifier
                .notify(&inserted)
                .await
                .map_err(|error| WalHistoryError::Notification(error.to_string()))?;
        }
        Ok(inserted)
    }

    /// Persists original WAL records in bounded commits, retaining each replay
    /// identity. Cluster writers commit up to 32 records together; compatibility
    /// history-only sinks keep their existing per-record behavior.
    ///
    /// # Errors
    ///
    /// Returns a database, record-shape, codec, or notification failure.
    pub async fn persist_records(
        &self,
        records: &[&WalRecord],
    ) -> Result<Vec<StoredWebhookEventKey>, WalHistoryError> {
        let mut inserted = Vec::new();
        for chunk in records.chunks(MAX_PERSIST_BATCH_RECORDS) {
            if let Some(machine_id) = self.cluster_machine.filter(|_| chunk.len() > 1) {
                inserted.extend(self.persist_cluster_batch(chunk, machine_id).await?);
            } else {
                for record in chunk {
                    inserted.extend(self.persist_record(record).await?);
                }
            }
        }
        Ok(inserted)
    }

    async fn persist_cluster_batch(
        &self,
        records: &[&WalRecord],
        machine_id: Uuid,
    ) -> Result<Vec<StoredWebhookEventKey>, WalHistoryError> {
        let started = tokio::time::Instant::now();
        let input = records
            .iter()
            .enumerate()
            .map(|(ordinal, record)| BatchHistoryRecord::new(ordinal, record))
            .collect::<Result<Vec<_>, _>>()?;
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        let acquire_started = tokio::time::Instant::now();
        let client = self.client.acquire().await?;
        let acquire_ms = acquire_started.elapsed().as_secs_f64() * 1000.0;
        let sql_started = tokio::time::Instant::now();
        let statement = client.prepare_cached(PERSIST_BATCH_SQL).await?;
        let rows = client
            .query(&statement, &[&machine_id, &Json(&input)])
            .await?;
        drop(client);
        let sql_ms = sql_started.elapsed().as_secs_f64() * 1000.0;
        let mut inserted = Vec::new();
        for row in rows {
            if let Some(webhook_id) = row.get::<_, Option<Uuid>>(0) {
                inserted.push(StoredWebhookEventKey {
                    webhook_id,
                    isn: Isn::from_postgres_composite(&row.get::<_, String>(1))?,
                    partition_bucket: row.get(2),
                });
            }
        }
        tracing::debug!(
            target: "instant_server::realtime_latency",
            record_count = records.len(), encode_ms, acquire_ms, sql_ms,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            webhook_count = inserted.len(),
            "realtime durable batch persisted"
        );
        if !inserted.is_empty() {
            self.notifier
                .notify(&inserted)
                .await
                .map_err(|error| WalHistoryError::Notification(error.to_string()))?;
        }
        Ok(inserted)
    }
}

#[derive(Debug, serde::Serialize)]
struct BatchHistoryRecord<'a> {
    ordinal: usize,
    isn: String,
    app_id: Uuid,
    room_id: String,
    payload: &'a WalRecord,
    content: Option<String>,
    topics: i64,
    partition_bucket: i32,
    created_attrs: Vec<Uuid>,
    updated_attrs: Vec<Uuid>,
    deleted_attrs: Vec<Uuid>,
}

impl<'a> BatchHistoryRecord<'a> {
    fn new(ordinal: usize, record: &'a WalRecord) -> Result<Self, WalHistoryError> {
        let has_history = !record.messages.is_empty() || !record.wal_logs.is_empty();
        let (content, topics, bucket, attrs) = if has_history {
            let created_at = record
                .tx_created_at
                .ok_or(WalHistoryError::MissingCreatedAt)?;
            (
                Some(base64::engine::general_purpose::STANDARD.encode(encode_wal_record(record)?)),
                topic_filter(record)?,
                partition_bucket(created_at),
                webhook_change_attrs(record),
            )
        } else {
            (None, 0, 0, Default::default())
        };
        let [created_attrs, updated_attrs, deleted_attrs] = attrs;
        Ok(Self {
            ordinal,
            isn: record.isn.to_postgres_composite(),
            app_id: record.app_id,
            room_id: record.isn.to_string(),
            payload: record,
            content,
            topics,
            partition_bucket: bucket,
            created_attrs,
            updated_attrs,
            deleted_attrs,
        })
    }
}

impl<N> DurableInvalidationSink for PostgresDurableInvalidationSink<N>
where
    N: WebhookEventNotifier,
{
    type Error = WalHistoryError;

    fn published_cluster_machine(&self) -> Option<Uuid> {
        self.cluster_machine
    }

    fn max_batch_records(&self) -> usize {
        if self.cluster_machine.is_some() {
            MAX_PERSIST_BATCH_RECORDS
        } else {
            1
        }
    }

    async fn persist_batch(
        &mut self,
        outcomes: &[ReplicationPipelineOutcome],
    ) -> Result<(), Self::Error> {
        let records = outcomes
            .iter()
            .filter_map(|outcome| outcome.record.as_ref())
            .collect::<Vec<_>>();
        self.persist_records(&records).await?;
        for record in records {
            if let Some(created_at) = record.tx_created_at {
                self.webhook_latency.record(created_at);
            }
        }
        Ok(())
    }

    async fn persist(&mut self, outcome: &ReplicationPipelineOutcome) -> Result<(), Self::Error> {
        if let Some(record) = &outcome.record {
            self.persist_record(record).await?;
            if let Some(tx_created_at) = record.tx_created_at {
                self.webhook_latency.record(tx_created_at);
            }
        }
        Ok(())
    }
}

pub(crate) fn encode_wal_record(record: &WalRecord) -> Result<Vec<u8>, WalHistoryError> {
    let json = serde_json::to_vec(record)?;
    let compressed = zstd::stream::encode_all(Cursor::new(json), 3)?;
    let mut payload = Vec::with_capacity(HISTORY_MAGIC.len() + compressed.len());
    payload.extend_from_slice(HISTORY_MAGIC);
    payload.extend_from_slice(&compressed);
    Ok(payload)
}

pub(crate) fn decode_wal_record(payload: &[u8]) -> Result<WalRecord, WalHistoryError> {
    let compressed = payload
        .strip_prefix(HISTORY_MAGIC)
        .ok_or(WalHistoryError::UnsupportedPayload)?;
    let json = zstd::stream::decode_all(Cursor::new(compressed))?;
    Ok(serde_json::from_slice(&json)?)
}

// All changes are bound values. One PostgreSQL statement commits replay history
// and its exact webhook matches together, including when no webhook matches.
// The count dependency finishes history writes before webhook inserts while
// still considering webhook matches on replay when no new history row returns.
const PERSIST_RECORD_SQL: &str = "WITH persisted_history AS (
    INSERT INTO history (isn, app_id, topics, storage, content, partition_bucket)
    VALUES ($1::text::isn, $2, $3, 'pg'::history_storage, $4, $5)
    ON CONFLICT (isn, partition_bucket) DO NOTHING
    RETURNING isn
)
INSERT INTO webhook_events (webhook_id, isn, app_id, status, partition_bucket)
SELECT id, $1::text::isn, $2, 'pending'::webhook_event_status, $5
FROM webhooks
WHERE (SELECT count(*) FROM persisted_history) >= 0
  AND app_id = $2 AND status = 'active'::webhook_status AND (topics & $3) <> 0
  AND (('create'::webhook_action = ANY(actions) AND id_attr_ids && $6::uuid[])
    OR ('update'::webhook_action = ANY(actions) AND id_attr_ids && $7::uuid[])
    OR ('delete'::webhook_action = ANY(actions) AND id_attr_ids && $8::uuid[]))
ON CONFLICT (webhook_id, isn, partition_bucket) DO NOTHING
RETURNING webhook_id";

// Reuse the exact history/webhook predicates. Explicit CTE dependencies order
// history, webhook insertion and event ID allocation inside the same commit.
// The LEFT JOIN returns a row even without webhook matches, so pg_notify runs.
static PERSIST_AND_PUBLISH_SQL: LazyLock<String> = LazyLock::new(|| {
    let history_and_webhooks = PERSIST_RECORD_SQL.replacen(
        ")\nINSERT INTO webhook_events",
        "), persisted_webhooks AS (\nINSERT INTO webhook_events",
        1,
    );
    format!("{history_and_webhooks}
), published_event AS (
 INSERT INTO rust_realtime_room_events (source_machine_id, kind, app_id, room_id, payload)
 SELECT $9, 'wal', $2, $10, $11
 WHERE (SELECT count(*) FROM persisted_webhooks) >= 0
 ON CONFLICT (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal') DO NOTHING
 RETURNING id
)
SELECT webhook_id, CASE WHEN publication.count > 0 THEN pg_notify('rust_realtime_room_events', '') END
FROM (SELECT count(*) FROM published_event) publication
LEFT JOIN persisted_webhooks ON true")
});

// Materialize bound inputs once. History conflicts must not suppress webhook
// matching on replay. CTE dependencies complete both writes before publishing;
// the final ORDER BY allocates event IDs in original WAL order on the shared
// single-connection writer. Every row and notification commits in one statement.
const PERSIST_BATCH_SQL: &str = "WITH input AS MATERIALIZED (
 SELECT * FROM jsonb_to_recordset($2::jsonb) AS r(
   ordinal bigint, isn text, app_id uuid, room_id text, payload jsonb,
   content text, topics bigint, partition_bucket int,
   created_attrs uuid[], updated_attrs uuid[], deleted_attrs uuid[])
), persisted_history AS (
 INSERT INTO history (isn, app_id, topics, storage, content, partition_bucket)
 SELECT isn::isn, app_id, topics, 'pg'::history_storage,
        decode(content, 'base64'), partition_bucket
 FROM input WHERE content IS NOT NULL ORDER BY ordinal
 ON CONFLICT (isn, partition_bucket) DO NOTHING RETURNING isn
), persisted_webhooks AS (
 INSERT INTO webhook_events (webhook_id, isn, app_id, status, partition_bucket)
 SELECT w.id, r.isn::isn, r.app_id, 'pending'::webhook_event_status, r.partition_bucket
 FROM input r JOIN webhooks w ON w.app_id=r.app_id
 WHERE (SELECT count(*) FROM persisted_history) >= 0
   AND r.content IS NOT NULL AND w.status='active'::webhook_status AND (w.topics & r.topics) <> 0
   AND (('create'::webhook_action=ANY(w.actions) AND w.id_attr_ids && r.created_attrs)
     OR ('update'::webhook_action=ANY(w.actions) AND w.id_attr_ids && r.updated_attrs)
     OR ('delete'::webhook_action=ANY(w.actions) AND w.id_attr_ids && r.deleted_attrs))
 ORDER BY r.ordinal, w.id
 ON CONFLICT (webhook_id, isn, partition_bucket) DO NOTHING
 RETURNING webhook_id, isn, partition_bucket
), published_events AS (
 INSERT INTO rust_realtime_room_events (source_machine_id, kind, app_id, room_id, payload)
 SELECT $1, 'wal', app_id, room_id, payload FROM input
 WHERE (SELECT count(*) FROM persisted_webhooks) >= 0
 ORDER BY ordinal
 ON CONFLICT (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal') DO NOTHING RETURNING id
)
SELECT webhook_id, isn::text, partition_bucket,
       CASE WHEN publication.count > 0 THEN pg_notify('rust_realtime_room_events', '') END
FROM (SELECT count(*) FROM published_events) publication
LEFT JOIN persisted_webhooks ON true";

fn webhook_change_attrs(record: &WalRecord) -> [Vec<Uuid>; 3] {
    let mut attrs: [Vec<Uuid>; 3] = Default::default();
    for change in &record.triple_changes {
        let (action, columns) = match change.action {
            WalAction::Insert => (0, &change.columns),
            WalAction::Update
                if matches!(
                    (change.column("pg_size"), change.identity_column("pg_size")),
                    (Some(current), Some(previous)) if current == previous
                ) =>
            {
                (1, &change.columns)
            }
            WalAction::Delete => (2, &change.identity),
            _ => continue,
        };
        if let Some(id) = attr_id(columns) {
            attrs[action].push(id);
        }
    }
    for ids in &mut attrs {
        ids.sort_unstable();
        ids.dedup();
    }
    attrs
}

#[cfg(test)]
fn webhook_matches(record: &WalRecord, id_attr_ids: &[Uuid], actions: &[&str]) -> bool {
    actions.iter().any(|action| match *action {
        "create" => record.triple_changes.iter().any(|change| {
            change.action == WalAction::Insert
                && attr_id(&change.columns).is_some_and(|id| id_attr_ids.contains(&id))
        }),
        "update" => record.triple_changes.iter().any(|change| {
            change.action == WalAction::Update
                && attr_id(&change.columns).is_some_and(|id| id_attr_ids.contains(&id))
                && matches!(
                    (change.column("pg_size"), change.identity_column("pg_size")),
                    (Some(current), Some(previous)) if current == previous
                )
        }),
        "delete" => record.triple_changes.iter().any(|change| {
            change.action == WalAction::Delete
                && attr_id(&change.identity).is_some_and(|id| id_attr_ids.contains(&id))
        }),
        _ => false,
    })
}

fn topic_filter(record: &WalRecord) -> Result<i64, WalHistoryError> {
    record
        .triple_changes
        .iter()
        .try_fold(0_i64, |mask, change| {
            let id = attr_id(change.identity.iter().chain(&change.columns))
                .ok_or(WalHistoryError::InvalidAttributeId)?;
            Ok(mask | bloom_bit(id))
        })
}

fn attr_id<'a>(columns: impl IntoIterator<Item = &'a instant_wal::WalColumn>) -> Option<Uuid> {
    columns
        .into_iter()
        .find(|column| column.name == "attr_id")?
        .value
        .as_str()?
        .parse()
        .ok()
}

fn bloom_bit(id: Uuid) -> i64 {
    let (seed, value) = id.as_u64_pair();
    let hash = xxhash_rust::xxh3::xxh3_64_with_seed(&value.to_ne_bytes(), seed);
    1_i64.wrapping_shl((hash % 64) as u32)
}

fn partition_bucket(created_at: DateTime<Utc>) -> i32 {
    i32::try_from((created_at.timestamp() / 86_400 / 30).rem_euclid(PARTITION_BUCKET_COUNT))
        .expect("history partition is in 0..13")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use instant_wal::{Lsn, WalColumn, WalEntry};
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn webhook_wakeup_notifier_signals_nonempty_commits() {
        let (notifier, mut wakeups) = WebhookEventWakeupNotifier::channel();
        notifier.notify(&[]).await.unwrap();
        assert!(wakeups.try_recv().is_err());

        notifier
            .notify(&[StoredWebhookEventKey {
                webhook_id: Uuid::new_v4(),
                isn: Isn::new(1, Lsn::from_u64(2)),
                partition_bucket: 0,
            }])
            .await
            .unwrap();
        wakeups.recv().await.unwrap();
    }

    fn entry(action: WalAction, attr_id: Uuid, pg_size: i64) -> WalEntry {
        let columns = vec![
            WalColumn {
                name: "attr_id".to_owned(),
                value: json!(attr_id),
            },
            WalColumn {
                name: "pg_size".to_owned(),
                value: json!(pg_size),
            },
        ];
        WalEntry {
            action,
            tx_bytes: 0,
            table: Some("triples".to_owned()),
            identity: columns.clone(),
            columns,
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn record(change: WalEntry) -> WalRecord {
        WalRecord {
            app_id: Uuid::new_v4(),
            tx_id: 1,
            isn: Isn::new(1, Lsn::from_u64(2)),
            previous_isn: None,
            tx_created_at: Some(Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap()),
            tx_bytes: 1,
            next_lsn: Lsn::from_u64(2),
            attr_changes: Vec::new(),
            ident_changes: Vec::new(),
            triple_changes: vec![change],
            messages: vec![WalEntry {
                action: WalAction::Message,
                tx_bytes: 0,
                table: None,
                columns: Vec::new(),
                identity: Vec::new(),
                prefix: Some("update_ents".to_owned()),
                content: Some("[]".to_owned()),
                lsn: None,
                nextlsn: None,
            }],
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }

    #[test]
    fn native_history_codec_round_trips_complete_records() {
        let original = record(entry(WalAction::Insert, Uuid::new_v4(), 10));
        let payload = encode_wal_record(&original).unwrap();
        assert_eq!(decode_wal_record(&payload).unwrap(), original);
        assert!(matches!(
            decode_wal_record(b"legacy"),
            Err(WalHistoryError::UnsupportedPayload)
        ));
    }

    #[test]
    fn exact_webhook_matching_rejects_bloom_false_positives_and_pg_size_updates() {
        let id_attr = Uuid::new_v4();
        let other = Uuid::new_v4();
        assert!(webhook_matches(
            &record(entry(WalAction::Insert, id_attr, 10)),
            &[id_attr],
            &["create"]
        ));
        assert!(!webhook_matches(
            &record(entry(WalAction::Insert, other, 10)),
            &[id_attr],
            &["create"]
        ));
        let mut pg_size_only = entry(WalAction::Update, id_attr, 11);
        pg_size_only.identity[1].value = json!(10);
        assert!(!webhook_matches(
            &record(pg_size_only),
            &[id_attr],
            &["update"]
        ));
    }
}
