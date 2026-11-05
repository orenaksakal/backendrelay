use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use instant_http::RuntimeSsePushRequest;
use instant_pg::ByopWalRecord;
use instant_realtime::{BroadcastPayload, PeerPresence, PeerUser, RoomKey};
use instant_wal::WalRecord;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, watch};
use tokio_postgres::types::Json;
use tokio_postgres::{AsyncMessage, Client, GenericClient, Row, Transaction};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) const PUBLISH_WAL_SQL: &str = "WITH event AS (
                   INSERT INTO rust_realtime_room_events
                   (source_machine_id, kind, app_id, room_id, payload)
                  VALUES ($1, 'wal', $2, $3, $4)
                  ON CONFLICT (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal')
                  DO NOTHING
                  RETURNING id
                 )
                 SELECT pg_notify('rust_realtime_room_events', '') FROM event";

use crate::postgres_pool::PostgresClientSource;
use crate::runtime_stream::RuntimeStreamAppendResult;
use crate::{
    INSTANT_CONFIG_APP_ID, PostgresPool, PostgresPoolError, RealtimeClusterConsumerKind,
    RealtimeClusterFuture, RuntimeRealtimeCluster, RuntimeRealtimeClusterDelivery,
    RuntimeRealtimeClusterEvent, RuntimeRoomSnapshot,
};

const MACHINE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Machine state and WAL-failure markers remain authoritative for two missed
/// heartbeats before a surviving process ignores them.
pub(crate) const MACHINE_HEALTH_GRACE: Duration =
    Duration::from_secs(MACHINE_HEARTBEAT_INTERVAL.as_secs() * 2);
const DEFAULT_STREAM_CREDITS: usize = 1_024;
const EVENT_NOTIFICATION_CHANNEL: &str = "rust_realtime_room_events";
const EVENT_LISTENER_RETRY: Duration = Duration::from_millis(250);
const EVENT_LISTENER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_SAFETY_POLL: Duration = Duration::from_secs(1);
const EVENT_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
const EVENT_LISTENER_APPLICATION_PREFIX: &str = "instant-realtime-listener-";

#[derive(Clone, Copy)]
struct EventWakeup {
    generation: u64,
    listener_available: bool,
}

/// PostgreSQL implementation of native cross-machine room state and broadcasts.
pub struct PostgresRealtimeCluster {
    machine_id: Uuid,
    writer: Arc<PostgresClientSource>,
    reader: Arc<PostgresClientSource>,
    acknowledgement_writer: Arc<PostgresClientSource>,
    events:
        BTreeMap<RealtimeClusterConsumerKind, broadcast::Sender<RuntimeRealtimeClusterDelivery>>,
    polling: Arc<StdMutex<BTreeSet<RealtimeClusterConsumerKind>>>,
    acknowledgements: BTreeMap<RealtimeClusterConsumerKind, watch::Sender<i64>>,
    runtime_delivery_window: usize,
    listener_cancellation: CancellationToken,
    wakeups: Option<watch::Receiver<EventWakeup>>,
}

impl PostgresRealtimeCluster {
    /// Starts the durable event poller for embedders that own fixed clients.
    /// This compatibility constructor intentionally retains short polling because
    /// it has no database URL from which to establish a dedicated listener.
    ///
    /// # Errors
    ///
    /// Returns when the initial event cursor cannot be read.
    pub async fn new(machine_id: Uuid, writer: Client, reader: Client) -> Result<Self, String> {
        Self::from_sources(
            machine_id,
            PostgresClientSource::fixed(writer),
            PostgresClientSource::fixed(reader),
        )
        .await
    }

    /// Starts the durable event poller with independently reconnecting writer
    /// and reader pools. This compatibility constructor intentionally retains
    /// short polling for external consumers without listener configuration.
    ///
    /// # Errors
    ///
    /// Returns when either startup lease cannot be acquired or the initial
    /// event cursor cannot be initialized.
    pub async fn from_pools(
        machine_id: Uuid,
        writer: PostgresPool,
        reader: PostgresPool,
    ) -> Result<Self, String> {
        Self::from_sources(
            machine_id,
            PostgresClientSource::pooled(writer),
            PostgresClientSource::pooled(reader),
        )
        .await
    }

    /// Starts the cluster with one dedicated, reconnecting PostgreSQL listener.
    /// The listener only supplies wakeup hints; event delivery remains cursor-based.
    ///
    /// # Errors
    ///
    /// Returns when the writer or reader startup lease cannot be acquired, or
    /// the durable event cursor cannot be initialized.
    pub async fn from_pools_with_listener(
        machine_id: Uuid,
        writer: PostgresPool,
        reader: PostgresPool,
        listener_database_url: &str,
    ) -> Result<Self, String> {
        Self::from_sources_with_listener(
            machine_id,
            PostgresClientSource::pooled(writer),
            PostgresClientSource::pooled(reader),
            Some(listener_database_url.to_owned()),
        )
        .await
    }

    async fn from_sources(
        machine_id: Uuid,
        writer: PostgresClientSource,
        reader: PostgresClientSource,
    ) -> Result<Self, String> {
        Self::from_sources_with_listener(machine_id, writer, reader, None).await
    }

    async fn from_sources_with_listener(
        machine_id: Uuid,
        writer: PostgresClientSource,
        reader: PostgresClientSource,
        listener_database_url: Option<String>,
    ) -> Result<Self, String> {
        let mut startup = writer.acquire().await.map_err(pool_error)?;
        let transaction = startup.transaction().await.map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO rust_realtime_machines (machine_id)
                 VALUES ($1)
                 ON CONFLICT (machine_id) DO UPDATE SET updated_at = now()",
                &[&machine_id],
            )
            .await
            .map_err(database_error)?;
        let event_head = transaction
            .query_one(
                "SELECT coalesce(max(id), 0) FROM rust_realtime_room_events",
                &[],
            )
            .await
            .map_err(database_error)?
            .get::<_, i64>(0);
        for consumer in [
            RealtimeClusterConsumerKind::Runtime,
            RealtimeClusterConsumerKind::Admin,
        ] {
            transaction
                .execute(
                    "INSERT INTO rust_realtime_event_consumers
                       (machine_id, consumer_kind, event_cursor)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (machine_id, consumer_kind) DO NOTHING",
                    &[&machine_id, &consumer.as_str(), &event_head],
                )
                .await
                .map_err(database_error)?;
        }
        transaction.commit().await.map_err(database_error)?;
        let stream_credits = load_stream_credits(&startup)
            .await
            .map_err(database_error)?;
        let events = [
            RealtimeClusterConsumerKind::Runtime,
            RealtimeClusterConsumerKind::Admin,
        ]
        .into_iter()
        .map(|kind| (kind, broadcast::channel(stream_credits).0))
        .collect();
        let acknowledgements = [
            RealtimeClusterConsumerKind::Runtime,
            RealtimeClusterConsumerKind::Admin,
        ]
        .into_iter()
        .map(|kind| (kind, watch::channel(0).0))
        .collect();
        drop(startup);
        let writer = Arc::new(writer);
        let reader = Arc::new(reader);
        tokio::spawn(maintain_machine(machine_id, Arc::downgrade(&writer)));
        let listener_cancellation = CancellationToken::new();
        let wakeups = listener_database_url.map(|database_url| {
            let (wakeups, receiver) = watch::channel(EventWakeup {
                generation: 0,
                listener_available: false,
            });
            tokio::spawn(maintain_event_listener(
                database_url,
                machine_id,
                wakeups,
                listener_cancellation.clone(),
            ));
            receiver
        });
        Ok(Self {
            machine_id,
            acknowledgement_writer: Arc::clone(&writer),
            writer,
            reader,
            events,
            polling: Arc::new(StdMutex::new(BTreeSet::new())),
            acknowledgements,
            runtime_delivery_window: stream_credits
                .min(crate::runtime_realtime::MAX_WAL_REFRESH_WINDOWS),
            listener_cancellation,
            wakeups,
        })
    }

    /// Keeps durable consumer cursor writes off the event publication connection.
    /// Event inserts still share one writer, preserving their existing ordering.
    /// Configure this before sharing or subscribing to the cluster.
    #[must_use]
    pub fn with_acknowledgement_pool(mut self, pool: PostgresPool) -> Self {
        self.acknowledgement_writer = Arc::new(PostgresClientSource::pooled(pool));
        self
    }

    /// Removes sessions owned by machines whose heartbeat has expired and
    /// publishes authoritative presence snapshots for every affected room.
    ///
    /// # Errors
    ///
    /// Returns when the maintenance transaction cannot be completed.
    pub async fn reap_stale_machines(&self) -> Result<u64, String> {
        let mut writer = self.writer.acquire().await.map_err(pool_error)?;
        reap_stale_machine_state(&mut writer).await
    }

    /// Deletes expired events only through the acknowledgement floor of every
    /// live machine. Stale machine leases do not pin the shared event log.
    ///
    /// # Errors
    ///
    /// Returns when the retention statement cannot be completed.
    pub async fn prune_acknowledged_events(&self) -> Result<u64, String> {
        let writer = self.writer.acquire().await.map_err(pool_error)?;
        prune_acknowledged_events(&*writer).await
    }

    async fn mutate_join(
        &self,
        room: &RoomKey,
        peer: &PeerPresence,
    ) -> Result<RuntimeRoomSnapshot, String> {
        let mut client = self.writer.acquire().await.map_err(pool_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        lock_room(&transaction, room).await?;
        transaction
            .execute(
                "INSERT INTO rust_realtime_room_members
                   (app_id, room_id, session_id, machine_id, instance_id, user_id, data)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (app_id, room_id, session_id) DO UPDATE SET
                   machine_id = EXCLUDED.machine_id,
                   instance_id = EXCLUDED.instance_id,
                   user_id = EXCLUDED.user_id,
                   data = CASE
                     WHEN rust_realtime_room_members.data <> 'null'::jsonb
                       AND rust_realtime_room_members.data <> 'false'::jsonb
                       THEN rust_realtime_room_members.data
                     WHEN EXCLUDED.data <> 'null'::jsonb
                       AND EXCLUDED.data <> 'false'::jsonb
                       THEN EXCLUDED.data
                     ELSE '{}'::jsonb
                   END",
                &[
                    &room.app_id,
                    &room.room_id,
                    &peer.peer_id,
                    &self.machine_id,
                    &peer.instance_id,
                    &peer.user.map(|user| user.id),
                    &Json(&peer.data),
                ],
            )
            .await
            .map_err(database_error)?;
        let peers = load_snapshot(&transaction, room).await?;
        let revision = insert_presence_event(&transaction, self.machine_id, room).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(RuntimeRoomSnapshot { revision, peers })
    }

    async fn mutate_presence(
        &self,
        room: &RoomKey,
        session_id: Uuid,
        data: &Value,
    ) -> Result<RuntimeRoomSnapshot, String> {
        let mut client = self.writer.acquire().await.map_err(pool_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        lock_room(&transaction, room).await?;
        let updated = transaction
            .execute(
                "UPDATE rust_realtime_room_members SET data = $5
                 WHERE app_id = $1 AND room_id = $2 AND session_id = $3
                   AND machine_id = $4",
                &[
                    &room.app_id,
                    &room.room_id,
                    &session_id,
                    &self.machine_id,
                    &Json(data),
                ],
            )
            .await
            .map_err(database_error)?;
        if updated == 0 {
            return Err(format!(
                "session {session_id} is not owned by machine {}",
                self.machine_id
            ));
        }
        let peers = load_snapshot(&transaction, room).await?;
        let revision = insert_presence_event(&transaction, self.machine_id, room).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(RuntimeRoomSnapshot { revision, peers })
    }

    async fn mutate_leave(
        &self,
        room: &RoomKey,
        session_id: Uuid,
    ) -> Result<RuntimeRoomSnapshot, String> {
        let mut client = self.writer.acquire().await.map_err(pool_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        lock_room(&transaction, room).await?;
        transaction
            .execute(
                "DELETE FROM rust_realtime_room_members
                 WHERE app_id = $1 AND room_id = $2 AND session_id = $3
                   AND machine_id = $4",
                &[&room.app_id, &room.room_id, &session_id, &self.machine_id],
            )
            .await
            .map_err(database_error)?;
        let peers = load_snapshot(&transaction, room).await?;
        let revision = insert_presence_event(&transaction, self.machine_id, room).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(RuntimeRoomSnapshot { revision, peers })
    }

    async fn insert_broadcast(
        &self,
        room: &RoomKey,
        payload: &BroadcastPayload,
    ) -> Result<(), String> {
        self.writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "WITH event AS (
                   INSERT INTO rust_realtime_room_events
                   (source_machine_id, kind, app_id, room_id, payload)
                  VALUES ($1, 'broadcast', $2, $3, $4)
                  RETURNING id
                 )
                 SELECT pg_notify('rust_realtime_room_events', '') FROM event",
                &[
                    &self.machine_id,
                    &room.app_id,
                    &room.room_id,
                    &Json(payload),
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn insert_stream_append(
        &self,
        app_id: Uuid,
        append: &RuntimeStreamAppendResult,
    ) -> Result<(), String> {
        self.writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "WITH event AS (
                   INSERT INTO rust_realtime_room_events
                   (source_machine_id, kind, app_id, room_id, payload)
                  VALUES ($1, 'stream-append', $2, $3, $4)
                  RETURNING id
                 )
                 SELECT pg_notify('rust_realtime_room_events', '') FROM event",
                &[
                    &self.machine_id,
                    &app_id,
                    &append.stream_id.to_string(),
                    &Json(append),
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn insert_stream_disconnect(&self, app_id: Uuid, stream_id: Uuid) -> Result<(), String> {
        self.writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "WITH event AS (
                   INSERT INTO rust_realtime_room_events
                   (source_machine_id, kind, app_id, room_id)
                  VALUES ($1, 'stream-disconnect', $2, $3)
                  RETURNING id
                 )
                 SELECT pg_notify('rust_realtime_room_events', '') FROM event",
                &[&self.machine_id, &app_id, &stream_id.to_string()],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn upsert_sse_session(
        &self,
        app_id: Uuid,
        session_id: Uuid,
        token_hash: &[u8; 32],
    ) -> Result<(), String> {
        self.writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "INSERT INTO rust_realtime_sse_sessions
                   (session_id, app_id, machine_id, token_hash)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (session_id) DO UPDATE SET
                   app_id = EXCLUDED.app_id,
                   machine_id = EXCLUDED.machine_id,
                   token_hash = EXCLUDED.token_hash",
                &[
                    &session_id,
                    &app_id,
                    &self.machine_id,
                    &token_hash.as_slice(),
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn delete_sse_session(&self, session_id: Uuid) -> Result<(), String> {
        self.writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "DELETE FROM rust_realtime_sse_sessions
                 WHERE session_id = $1 AND machine_id = $2",
                &[&session_id, &self.machine_id],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn insert_sse_push(&self, request: &RuntimeSsePushRequest) -> Result<bool, String> {
        let token_hash = Sha256::digest(request.sse_token.as_bytes());
        let inserted = self
            .writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "WITH event AS (
                   INSERT INTO rust_realtime_room_events
                   (source_machine_id, kind, app_id, room_id, payload)
                  SELECT $1, 'sse-push', $2, $3, $4
                 WHERE EXISTS (
                   SELECT 1 FROM rust_realtime_sse_sessions
                   WHERE session_id = $5 AND app_id = $2 AND machine_id = $6
                      AND token_hash = $7
                  )
                  RETURNING id
                 )
                 SELECT pg_notify('rust_realtime_room_events', '') FROM event",
                &[
                    &self.machine_id,
                    &request.app_id,
                    &request.session_id.to_string(),
                    &Json(json!({
                        "target-machine-id": request.machine_id,
                        "messages": request.messages,
                    })),
                    &request.session_id,
                    &request.machine_id,
                    &token_hash.as_slice(),
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(inserted == 1)
    }

    async fn insert_wal(&self, record: &WalRecord) -> Result<(), String> {
        let client = self.writer.acquire().await.map_err(pool_error)?;
        let statement = client
            .prepare_cached(PUBLISH_WAL_SQL)
            .await
            .map_err(database_error)?;
        client
            .execute(
                &statement,
                &[
                    &self.machine_id,
                    &record.app_id,
                    &record.isn.to_string(),
                    &Json(record),
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn insert_byop_wal(
        &self,
        app_id: Uuid,
        record: &ByopWalRecord,
        topics: &[instant_pg::ByopTopic],
    ) -> Result<(), String> {
        self.writer
            .acquire()
            .await
            .map_err(pool_error)?
            .execute(
                "WITH event AS (
                   INSERT INTO rust_realtime_room_events
                   (source_machine_id, kind, app_id, room_id, payload)
                  VALUES ($1, 'byop-wal', $2, $3, $4)
                  ON CONFLICT (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal')
                  DO NOTHING
                  RETURNING id
                 )
                 SELECT pg_notify('rust_realtime_room_events', '') FROM event",
                &[
                    &self.machine_id,
                    &app_id,
                    &record.tx_id.to_string(),
                    &Json(json!({"record": record, "topics": topics})),
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }
}

impl Drop for PostgresRealtimeCluster {
    fn drop(&mut self) {
        self.listener_cancellation.cancel();
    }
}

async fn load_stream_credits(client: &Client) -> Result<usize, tokio_postgres::Error> {
    let value = client
        .query_opt(
            "SELECT flag_value.value
             FROM attrs setting_attr
             JOIN triples flag_setting
               ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
             JOIN attrs value_attr
               ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
              AND value_attr.label = 'value'
             JOIN triples flag_value
               ON flag_value.app_id = $1
              AND flag_value.entity_id = flag_setting.entity_id
              AND flag_value.attr_id = value_attr.id
             WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
               AND setting_attr.label = 'setting'
               AND flag_setting.value = '\"instant-stream-credits\"'::jsonb
             LIMIT 1",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?
        .map(|row| row.get::<_, Value>(0));
    Ok(parse_stream_credits(value.as_ref()))
}

fn parse_stream_credits(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_STREAM_CREDITS)
        .max(1)
}

impl RuntimeRealtimeCluster for PostgresRealtimeCluster {
    fn join<'a>(
        &'a self,
        room: &'a RoomKey,
        peer: PeerPresence,
    ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
        Box::pin(async move { self.mutate_join(room, &peer).await })
    }

    fn set_presence<'a>(
        &'a self,
        room: &'a RoomKey,
        session_id: Uuid,
        data: Value,
    ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
        Box::pin(async move { self.mutate_presence(room, session_id, &data).await })
    }

    fn leave<'a>(
        &'a self,
        room: &'a RoomKey,
        session_id: Uuid,
    ) -> RealtimeClusterFuture<'a, RuntimeRoomSnapshot> {
        Box::pin(async move { self.mutate_leave(room, session_id).await })
    }

    fn publish_broadcast<'a>(
        &'a self,
        room: &'a RoomKey,
        payload: BroadcastPayload,
    ) -> RealtimeClusterFuture<'a, ()> {
        Box::pin(async move { self.insert_broadcast(room, &payload).await })
    }

    fn publish_stream_append(
        &self,
        app_id: Uuid,
        append: RuntimeStreamAppendResult,
    ) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move { self.insert_stream_append(app_id, &append).await })
    }

    fn publish_stream_disconnect(
        &self,
        app_id: Uuid,
        stream_id: Uuid,
    ) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move { self.insert_stream_disconnect(app_id, stream_id).await })
    }

    fn register_sse_session(
        &self,
        app_id: Uuid,
        session_id: Uuid,
        token_hash: [u8; 32],
    ) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move {
            self.upsert_sse_session(app_id, session_id, &token_hash)
                .await
        })
    }

    fn unregister_sse_session(&self, session_id: Uuid) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move { self.delete_sse_session(session_id).await })
    }

    fn forward_sse_push(&self, request: RuntimeSsePushRequest) -> RealtimeClusterFuture<'_, bool> {
        Box::pin(async move { self.insert_sse_push(&request).await })
    }

    fn publish_wal(&self, record: WalRecord) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move { self.insert_wal(&record).await })
    }

    fn publish_byop_wal(
        &self,
        app_id: Uuid,
        record: ByopWalRecord,
        topics: Vec<instant_pg::ByopTopic>,
    ) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move { self.insert_byop_wal(app_id, &record, &topics).await })
    }

    fn subscribe(
        &self,
        consumer: RealtimeClusterConsumerKind,
    ) -> RealtimeClusterFuture<'_, broadcast::Receiver<RuntimeRealtimeClusterDelivery>> {
        Box::pin(async move {
            let events = self
                .events
                .get(&consumer)
                .expect("all realtime consumer kinds have a delivery channel")
                .clone();
            if events.receiver_count() != 0 {
                return Err(format!(
                    "realtime {consumer:?} consumer is already subscribed on machine {}",
                    self.machine_id
                ));
            }
            let receiver = events.subscribe();
            let writer = self.writer.acquire().await.map_err(pool_error)?;
            let cursor = writer
                .query_one(
                    "UPDATE rust_realtime_event_consumers
                     SET updated_at = now()
                     WHERE machine_id = $1 AND consumer_kind = $2
                     RETURNING event_cursor",
                    &[&self.machine_id, &consumer.as_str()],
                )
                .await
                .map_err(database_error)?
                .get::<_, i64>(0);
            let start_poller = self
                .polling
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(consumer);
            if start_poller {
                tokio::spawn(poll_events(
                    Arc::clone(&self.reader),
                    self.machine_id,
                    consumer,
                    cursor,
                    events,
                    Arc::clone(&self.polling),
                    self.wakeups.clone(),
                    self.acknowledgements[&consumer].subscribe(),
                    self.runtime_delivery_window,
                ));
            }
            Ok(receiver)
        })
    }

    fn acknowledge(
        &self,
        consumer: RealtimeClusterConsumerKind,
        event_id: i64,
    ) -> RealtimeClusterFuture<'_, ()> {
        Box::pin(async move {
            let writer = self
                .acknowledgement_writer
                .acquire()
                .await
                .map_err(pool_error)?;
            acknowledge_event(&writer, self.machine_id, consumer, event_id).await?;
            // Publish only after the durable cursor write commits. The separate
            // consumer channels cannot release each other's ordered delivery.
            self.acknowledgements[&consumer].send_if_modified(|cursor| {
                if event_id > *cursor {
                    *cursor = event_id;
                    true
                } else {
                    false
                }
            });
            Ok(())
        })
    }
}

async fn lock_room(transaction: &Transaction<'_>, room: &RoomKey) -> Result<(), String> {
    transaction
        .query_one(
            "SELECT pg_advisory_xact_lock(
               hashtextextended($1::uuid::text || ':' || $2, 0))",
            &[&room.app_id, &room.room_id],
        )
        .await
        .map_err(database_error)?;
    Ok(())
}

async fn insert_presence_event(
    transaction: &Transaction<'_>,
    machine_id: Uuid,
    room: &RoomKey,
) -> Result<i64, String> {
    transaction
        .query_one(
            "WITH event AS (
               INSERT INTO rust_realtime_room_events
               (source_machine_id, kind, app_id, room_id)
              VALUES ($1, 'presence', $2, $3)
              RETURNING id
             )
             SELECT id, pg_notify('rust_realtime_room_events', '') FROM event",
            &[&machine_id, &room.app_id, &room.room_id],
        )
        .await
        .map_err(database_error)
        .map(|row| row.get(0))
}

async fn load_snapshot<C>(
    client: &C,
    room: &RoomKey,
) -> Result<BTreeMap<Uuid, PeerPresence>, String>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT session_id, instance_id, user_id, data
             FROM rust_realtime_room_members
             WHERE app_id = $1 AND room_id = $2
             ORDER BY session_id",
            &[&room.app_id, &room.room_id],
        )
        .await
        .map_err(database_error)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let peer_id = row.get::<_, Uuid>(0);
            (
                peer_id,
                PeerPresence {
                    peer_id,
                    instance_id: row.get(1),
                    user: row.get::<_, Option<Uuid>>(2).map(|id| PeerUser { id }),
                    data: row.get::<_, Json<Value>>(3).0,
                },
            )
        })
        .collect())
}

async fn maintain_machine(machine_id: Uuid, writer: std::sync::Weak<PostgresClientSource>) {
    let mut interval = tokio::time::interval(MACHINE_HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(writer) = writer.upgrade() else {
            break;
        };
        let Ok(mut client) = writer.acquire().await else {
            continue;
        };
        if client
            .execute(
                "INSERT INTO rust_realtime_machines (machine_id)
                 VALUES ($1)
                 ON CONFLICT (machine_id) DO UPDATE SET updated_at = now()",
                &[&machine_id],
            )
            .await
            .is_ok()
        {
            let _ = reap_stale_machine_state(&mut client).await;
        }
    }
}

async fn reap_stale_machine_state(client: &mut Client) -> Result<u64, String> {
    let transaction = client.transaction().await.map_err(database_error)?;
    let row = transaction
        .query_one(
            "WITH stale AS MATERIALIZED (
               SELECT machine_id
               FROM rust_realtime_machines
               WHERE updated_at < now() - interval '30 seconds'
               FOR UPDATE
             ),
             deleted_members AS (
               DELETE FROM rust_realtime_room_members AS member
               USING stale
               WHERE member.machine_id = stale.machine_id
               RETURNING member.app_id, member.room_id
             ),
             affected_rooms AS (
               SELECT DISTINCT app_id, room_id FROM deleted_members
             ),
             emitted AS (
               INSERT INTO rust_realtime_room_events
                 (source_machine_id, kind, app_id, room_id)
               SELECT $1, 'presence', app_id, room_id FROM affected_rooms
               RETURNING id
             ),
             deleted_sse AS (
               DELETE FROM rust_realtime_sse_sessions AS session
               USING stale
               WHERE session.machine_id = stale.machine_id
             ),
             deleted_machines AS (
               DELETE FROM rust_realtime_machines AS machine
               USING stale
               WHERE machine.machine_id = stale.machine_id
             ),
             cleared_wal_errors AS (
               UPDATE config AS wal
               SET v = (wal.v::jsonb - cleared.keys)::json
               FROM (
                 SELECT array_agg(entry.key) AS keys
                 FROM config AS wal_config
                 CROSS JOIN LATERAL jsonb_each(wal_config.v::jsonb) AS entry
                 WHERE wal_config.k = 'wal-errors'
                   AND (
                     EXISTS (
                       SELECT 1 FROM stale
                       WHERE stale.machine_id = CASE
                         WHEN pg_input_is_valid(entry.key, 'uuid')
                           THEN entry.key::uuid
                       END
                     )
                     OR (
                       NOT EXISTS (
                         SELECT 1 FROM rust_realtime_machines AS machine
                         WHERE machine.machine_id = CASE
                           WHEN pg_input_is_valid(entry.key, 'uuid')
                             THEN entry.key::uuid
                         END
                       )
                       -- CASE guards the cast (AND order is not guaranteed);
                       -- unparseable orphan markers are cleared, not kept forever.
                       AND CASE
                         WHEN pg_input_is_valid(entry.value #>> '{}', 'timestamptz')
                           THEN (entry.value #>> '{}')::timestamptz
                             < now() - interval '10 minutes'
                         ELSE true
                       END
                     )
                   )
               ) AS cleared
               WHERE wal.k = 'wal-errors' AND cleared.keys IS NOT NULL
             )
              SELECT count(*)::bigint,
                     (SELECT count(*)::bigint FROM emitted)
              FROM deleted_members",
            &[&Uuid::nil()],
        )
        .await
        .map_err(database_error)?;
    let deleted = row.get::<_, i64>(0);
    let emitted = row.get::<_, i64>(1);
    if emitted > 0 {
        transaction
            .execute("SELECT pg_notify('rust_realtime_room_events', '')", &[])
            .await
            .map_err(database_error)?;
    }
    transaction.commit().await.map_err(database_error)?;
    u64::try_from(deleted).map_err(|error| error.to_string())
}

#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "Delivery, acknowledgement, wakeup, and cursor progression must remain ordered together."
)]
async fn poll_events(
    reader: Arc<PostgresClientSource>,
    machine_id: Uuid,
    consumer: RealtimeClusterConsumerKind,
    mut cursor: i64,
    events: broadcast::Sender<RuntimeRealtimeClusterDelivery>,
    polling: Arc<StdMutex<BTreeSet<RealtimeClusterConsumerKind>>>,
    mut wakeups: Option<watch::Receiver<EventWakeup>>,
    mut acknowledgements: watch::Receiver<i64>,
    runtime_delivery_window: usize,
) {
    let mut next_prune = tokio::time::Instant::now() + EVENT_PRUNE_INTERVAL;
    // `cursor` is only the in-memory dispatch position. Restart reads the
    // durable consumer cursor, which advances through completed windows only.
    let mut outstanding = VecDeque::new();
    loop {
        if stop_poller_if_idle(&events, &polling, consumer) {
            return;
        }
        let listener_available = if let Some(wakeups) = &mut wakeups {
            // Establish the observed generation before scanning. A notification
            // committed during the scan then makes `changed` return immediately.
            wakeups.borrow_and_update().listener_available
        } else {
            false
        };
        let rows = if let Ok(client) = reader.acquire().await {
            client
                .query(
                    "SELECT id, source_machine_id, kind, app_id, room_id, payload
                 FROM rust_realtime_room_events
                 WHERE id > $1 ORDER BY id LIMIT 1000",
                    &[&cursor],
                )
                .await
        } else {
            if stop_poller_if_idle(&events, &polling, consumer) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };
        let mut stalled = false;
        let has_rows = if let Ok(rows) = rows {
            let has_rows = !rows.is_empty();
            let mut made_progress = false;
            let mut rows = rows.into_iter().peekable();
            while let Some(row) = rows.next() {
                let mut event_id = row.get(0);
                let Ok(client) = reader.acquire().await else {
                    stalled = true;
                    break;
                };
                let event = decode_event(&client, &row).await;
                let Ok(mut event) = event else {
                    stalled = true;
                    break;
                };
                if consumer == RealtimeClusterConsumerKind::Runtime
                    && let Some(RuntimeRealtimeClusterEvent::Wal { record, .. }) = &event
                {
                    let mut records = vec![record.clone()];
                    while records.len() < crate::runtime_realtime::MAX_WAL_REFRESH_BATCH {
                        let Some(next) = rows.peek() else {
                            break;
                        };
                        if next.get::<_, String>(2) != "wal" {
                            break;
                        }
                        let Ok(Some(RuntimeRealtimeClusterEvent::Wal { record, .. })) =
                            decode_event(&client, next).await
                        else {
                            break;
                        };
                        event_id = next.get(0);
                        records.push(record);
                        rows.next();
                    }
                    if records.len() > 1 {
                        event = Some(RuntimeRealtimeClusterEvent::WalBatch { records });
                    }
                }
                drop(client);
                let pipeline_wal = consumer == RealtimeClusterConsumerKind::Runtime
                    && matches!(
                        &event,
                        Some(
                            RuntimeRealtimeClusterEvent::Wal { .. }
                                | RuntimeRealtimeClusterEvent::WalBatch { .. }
                        )
                    );
                retire_acknowledged_deliveries(
                    &mut outstanding,
                    *acknowledgements.borrow_and_update(),
                );
                // Only ordinary runtime WAL may be read ahead. Skipped rows and
                // other event types are fences so their cursor cannot acknowledge
                // earlier work that is still running.
                let wait_for = if !pipeline_wal {
                    outstanding.back().copied()
                } else if outstanding.len() >= runtime_delivery_window {
                    outstanding.front().copied()
                } else {
                    None
                };
                if let Some(event_id) = wait_for {
                    let Some(acknowledged) = wait_for_event_acknowledgement(
                        &reader,
                        machine_id,
                        consumer,
                        event_id,
                        &events,
                        &polling,
                        &mut acknowledgements,
                    )
                    .await
                    else {
                        return;
                    };
                    retire_acknowledged_deliveries(&mut outstanding, acknowledged);
                }
                if let Some(event) = event {
                    while events
                        .send(RuntimeRealtimeClusterDelivery {
                            event_id,
                            event: event.clone(),
                        })
                        .is_err()
                    {
                        if stop_poller_if_idle(&events, &polling, consumer) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    outstanding.push_back(event_id);
                    if !pipeline_wal {
                        let Some(acknowledged) = wait_for_event_acknowledgement(
                            &reader,
                            machine_id,
                            consumer,
                            event_id,
                            &events,
                            &polling,
                            &mut acknowledgements,
                        )
                        .await
                        else {
                            return;
                        };
                        retire_acknowledged_deliveries(&mut outstanding, acknowledged);
                    }
                } else {
                    loop {
                        let acknowledged = match reader.acquire().await {
                            Ok(client) => {
                                acknowledge_event(&client, machine_id, consumer, event_id).await
                            }
                            Err(error) => Err(pool_error(error)),
                        };
                        if acknowledged.is_ok() {
                            break;
                        }
                        if stop_poller_if_idle(&events, &polling, consumer) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
                cursor = event_id;
                made_progress = true;
            }
            has_rows && made_progress && !stalled
        } else {
            if stop_poller_if_idle(&events, &polling, consumer) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };
        if tokio::time::Instant::now() >= next_prune {
            if let Ok(client) = reader.acquire().await {
                let _ = prune_acknowledged_events(&*client).await;
            }
            next_prune = tokio::time::Instant::now() + EVENT_PRUNE_INTERVAL;
        }
        if has_rows {
            // A nonempty batch means durable work may remain; avoid the idle delay.
            continue;
        }
        if stalled {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        if listener_available && let Some(listener_wakeups) = &mut wakeups {
            tokio::select! {
                _ = listener_wakeups.changed() => {},
                () = tokio::time::sleep(EVENT_SAFETY_POLL) => {},
            }
        } else {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn retire_acknowledged_deliveries(outstanding: &mut VecDeque<i64>, cursor: i64) {
    while outstanding
        .front()
        .is_some_and(|event_id| *event_id <= cursor)
    {
        outstanding.pop_front();
    }
}

async fn wait_for_event_acknowledgement(
    reader: &PostgresClientSource,
    machine_id: Uuid,
    consumer: RealtimeClusterConsumerKind,
    event_id: i64,
    events: &broadcast::Sender<RuntimeRealtimeClusterDelivery>,
    polling: &StdMutex<BTreeSet<RealtimeClusterConsumerKind>>,
    acknowledgements: &mut watch::Receiver<i64>,
) -> Option<i64> {
    loop {
        let cursor = *acknowledgements.borrow_and_update();
        if cursor >= event_id {
            return Some(cursor);
        }
        let cursor = match reader.acquire().await {
            Ok(client) => load_consumer_cursor(&client, machine_id, consumer).await,
            Err(_) => None,
        };
        if cursor.is_some_and(|cursor| cursor >= event_id) {
            return cursor;
        }
        if stop_poller_if_idle(events, polling, consumer) {
            return None;
        }
        // The local watch is the fast path. Database rereads are only a bounded
        // fallback, avoiding a cursor query every 25 ms while refreshes run.
        tokio::select! {
            result = acknowledgements.changed() => {
                if result.is_err() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            },
            () = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
}

async fn maintain_event_listener(
    database_url: String,
    machine_id: Uuid,
    wakeups: watch::Sender<EventWakeup>,
    cancellation: CancellationToken,
) {
    loop {
        let Ok(mut configuration) = database_url.parse::<tokio_postgres::Config>() else {
            return;
        };
        let application_name = listener_application_name(machine_id);
        configuration.application_name(&application_name);
        let connection = tokio::select! {
            () = cancellation.cancelled() => return,
            connection = tokio::time::timeout(
                EVENT_LISTENER_CONNECT_TIMEOUT,
                configuration.connect(MakeRustlsConnect::with_webpki_roots()),
            ) => connection,
        };
        let Ok(Ok((client, mut connection))) = connection else {
            wakeups.send_modify(|state| state.listener_available = false);
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = tokio::time::sleep(EVENT_LISTENER_RETRY) => continue,
            }
        };
        let listener_wakeups = wakeups.clone();
        let mut driver = tokio::spawn(async move {
            loop {
                match futures_util::future::poll_fn(|context| connection.poll_message(context))
                    .await
                {
                    Some(Ok(AsyncMessage::Notification(_))) => {
                        listener_wakeups.send_modify(|state| {
                            state.generation = state.generation.wrapping_add(1);
                        });
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        listener_wakeups.send_modify(|state| state.listener_available = false);
                        return;
                    }
                }
            }
        });
        let listen = format!("LISTEN {EVENT_NOTIFICATION_CHANNEL}");
        let listener_ready = tokio::select! {
            () = cancellation.cancelled() => {
                driver.abort();
                let _ = driver.await;
                wakeups.send_modify(|state| state.listener_available = false);
                return;
            }
            result = tokio::time::timeout(
                Duration::from_secs(10),
                client.batch_execute(&listen),
            ) => result.is_ok_and(|result| result.is_ok()),
        };
        if listener_ready {
            // A new listener is a wakeup: pollers catch up from their durable cursor.
            wakeups.send_modify(|state| {
                state.generation = state.generation.wrapping_add(1);
                state.listener_available = true;
            });
            tokio::select! {
                () = cancellation.cancelled() => {
                    driver.abort();
                    let _ = driver.await;
                    return;
                }
                _ = &mut driver => {}
            }
        } else {
            driver.abort();
            let _ = driver.await;
        }
        wakeups.send_modify(|state| state.listener_available = false);
        tokio::select! {
            () = cancellation.cancelled() => return,
            () = tokio::time::sleep(EVENT_LISTENER_RETRY) => {}
        }
    }
}

fn listener_application_name(machine_id: Uuid) -> String {
    format!("{EVENT_LISTENER_APPLICATION_PREFIX}{machine_id}")
}

/// Stops an inactive poller only while holding the same marker lock used when
/// subscribing. A receiver that arrives during shutdown therefore starts a new
/// poller from the durable cursor instead of being stranded behind a stale marker.
fn stop_poller_if_idle(
    events: &broadcast::Sender<RuntimeRealtimeClusterDelivery>,
    polling: &StdMutex<BTreeSet<RealtimeClusterConsumerKind>>,
    consumer: RealtimeClusterConsumerKind,
) -> bool {
    let mut polling = polling
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if events.receiver_count() == 0 {
        polling.remove(&consumer);
        true
    } else {
        false
    }
}

async fn acknowledge_event(
    client: &Client,
    machine_id: Uuid,
    consumer: RealtimeClusterConsumerKind,
    event_id: i64,
) -> Result<(), String> {
    let updated = client
        .execute(
            "WITH machine AS (
               INSERT INTO rust_realtime_machines (machine_id)
               VALUES ($1)
               ON CONFLICT (machine_id) DO UPDATE SET updated_at = now()
               RETURNING machine_id
             )
             INSERT INTO rust_realtime_event_consumers
               (machine_id, consumer_kind, event_cursor)
             SELECT machine_id, $3, $2 FROM machine
             ON CONFLICT (machine_id, consumer_kind) DO UPDATE SET
               event_cursor = greatest(
                 rust_realtime_event_consumers.event_cursor,
                 excluded.event_cursor
               ),
               updated_at = now()",
            &[&machine_id, &event_id, &consumer.as_str()],
        )
        .await
        .map_err(database_error)?;
    if updated == 1 {
        Ok(())
    } else {
        Err(format!(
            "realtime {consumer:?} consumer on machine {machine_id} could not acknowledge event {event_id}"
        ))
    }
}

async fn load_consumer_cursor(
    client: &Client,
    machine_id: Uuid,
    consumer: RealtimeClusterConsumerKind,
) -> Option<i64> {
    client
        .query_opt(
            "SELECT event_cursor FROM rust_realtime_event_consumers
             WHERE machine_id = $1 AND consumer_kind = $2",
            &[&machine_id, &consumer.as_str()],
        )
        .await
        .ok()
        .flatten()
        .map(|row| row.get(0))
}

async fn prune_acknowledged_events<C>(client: &C) -> Result<u64, String>
where
    C: GenericClient + Sync,
{
    client
        .execute(
            "WITH retention AS (
               SELECT coalesce(
                 min(consumer.event_cursor),
                 (SELECT coalesce(max(id), 0) FROM rust_realtime_room_events)
               ) AS cursor
               FROM rust_realtime_event_consumers AS consumer
               JOIN rust_realtime_machines AS machine USING (machine_id)
               WHERE machine.updated_at >= now() - interval '30 seconds'
             )
             DELETE FROM rust_realtime_room_events AS event
             USING retention
             WHERE event.created_at < now() - interval '10 minutes'
               AND event.id <= retention.cursor",
            &[],
        )
        .await
        .map_err(database_error)
}

#[allow(clippy::too_many_lines)] // Keeps every persisted event kind's skip/retry policy together.
async fn decode_event(
    reader: &Client,
    row: &Row,
) -> Result<Option<RuntimeRealtimeClusterEvent>, String> {
    let source_machine_id = row.get(1);
    let kind = row.get::<_, String>(2);
    let app_id = row.get(3);
    let room_id = row.get::<_, String>(4);
    match kind.as_str() {
        "presence" => {
            let room = RoomKey::new(app_id, room_id);
            Ok(Some(RuntimeRealtimeClusterEvent::Presence {
                source_machine_id,
                revision: row.get(0),
                peers: load_snapshot(reader, &room).await?,
                room,
            }))
        }
        "broadcast" => Ok(Some(RuntimeRealtimeClusterEvent::Broadcast {
            source_machine_id,
            room: RoomKey::new(app_id, room_id),
            payload: match row.get::<_, Option<Json<Value>>>(5) {
                Some(payload) => match serde_json::from_value(payload.0) {
                    Ok(payload) => payload,
                    Err(_) => return Ok(None),
                },
                None => return Ok(None),
            },
        })),
        "stream-append" => Ok(
            serde_json::from_value(match row.get::<_, Option<Json<Value>>>(5) {
                Some(payload) => payload.0,
                None => return Ok(None),
            })
            .ok()
            .map(|append| RuntimeRealtimeClusterEvent::StreamAppend {
                source_machine_id,
                app_id,
                append,
            }),
        ),
        "stream-disconnect" => Ok(Uuid::parse_str(&room_id).ok().map(|stream_id| {
            RuntimeRealtimeClusterEvent::StreamWriterDisconnected {
                source_machine_id,
                app_id,
                stream_id,
            }
        })),
        "sse-push" => {
            let Some(payload) = row.get::<_, Option<Json<Value>>>(5).map(|value| value.0) else {
                return Ok(None);
            };
            let Some(target_machine_id) = payload
                .get("target-machine-id")
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                return Ok(None);
            };
            let Some(session_id) = Uuid::parse_str(&room_id).ok() else {
                return Ok(None);
            };
            let Some(messages) = payload
                .get("messages")
                .cloned()
                .and_then(|messages| serde_json::from_value(messages).ok())
            else {
                return Ok(None);
            };
            Ok(Some(RuntimeRealtimeClusterEvent::SsePush {
                target_machine_id,
                app_id,
                session_id,
                messages,
            }))
        }
        "wal" => Ok(row
            .get::<_, Option<Json<Value>>>(5)
            .and_then(|payload| serde_json::from_value(payload.0).ok())
            .map(|record| RuntimeRealtimeClusterEvent::Wal {
                source_machine_id,
                record,
            })),
        "byop-wal" => {
            let Some(payload) = row.get::<_, Option<Json<Value>>>(5).map(|value| value.0) else {
                return Ok(None);
            };
            let Some(record) = payload
                .get("record")
                .cloned()
                .and_then(|record| serde_json::from_value(record).ok())
            else {
                return Ok(None);
            };
            let Some(topics) = payload
                .get("topics")
                .cloned()
                .and_then(|topics| serde_json::from_value(topics).ok())
            else {
                return Ok(None);
            };
            Ok(Some(RuntimeRealtimeClusterEvent::ByopWal {
                source_machine_id,
                app_id,
                record,
                topics,
            }))
        }
        _ => Ok(None),
    }
}

fn database_error(error: tokio_postgres::Error) -> String {
    let message = error.to_string();
    drop(error);
    crate::db_error::log_database_error(&message);
    crate::db_error::scrub_database_error(&message)
}

fn pool_error(error: PostgresPoolError) -> String {
    let message = error.to_string();
    drop(error);
    crate::db_error::log_database_error(&message);
    crate::db_error::scrub_database_error(&message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn stream_credit_window_preserves_default_floor_and_valid_override() {
        assert_eq!(parse_stream_credits(None), 1_024);
        assert_eq!(parse_stream_credits(Some(&json!(0))), 1);
        assert_eq!(parse_stream_credits(Some(&json!(17))), 17);
        for invalid in [json!(-1), json!(1.5), json!("20"), Value::Null] {
            assert_eq!(parse_stream_credits(Some(&invalid)), 1_024);
        }
    }

    #[test]
    fn listener_application_name_is_machine_specific() {
        let machine_id = Uuid::new_v4();
        assert_eq!(
            listener_application_name(machine_id),
            format!("{EVENT_LISTENER_APPLICATION_PREFIX}{machine_id}")
        );
    }

    #[test]
    fn idle_poller_removes_its_marker_and_a_new_receiver_keeps_it_active() {
        let consumer = RealtimeClusterConsumerKind::Runtime;
        let (events, _) = broadcast::channel(1);
        let polling = StdMutex::new(BTreeSet::from([consumer]));

        assert!(stop_poller_if_idle(&events, &polling, consumer));
        assert!(!polling.lock().unwrap().contains(&consumer));

        polling.lock().unwrap().insert(consumer);
        let receiver = events.subscribe();
        assert!(!stop_poller_if_idle(&events, &polling, consumer));
        assert!(polling.lock().unwrap().contains(&consumer));
        drop(receiver);
    }
}
