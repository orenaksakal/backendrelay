use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use instant_http::{
    AdminDataBackend, AdminQueryRequest, AdminRealtimeBackend, AdminRequestContext,
    AdminSseOpenRequest, AdminSsePushRequest, AdminSseSession, AdminTransactRequest, ApiError,
    ApiErrorType, RuntimeSsePushRequest,
};
use instant_pg::{ByopTopic, ByopWalRecord};
use instant_realtime::{
    BroadcastPayload, PeerPresence, PeerUser, PresenceUpdate, ReplicationPipelineOutcome, RoomKey,
    RoomStore, sdk_session,
};
use instant_wal::{Isn, WalCacheEviction, WalRecord};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::db_error::{database_error, internal_error};
use crate::runtime_realtime::{
    ClusterDeliveryTracker, DEFAULT_TRANSACTION_ISN_WAIT, ProcessedWatermark,
    RealtimeClusterConsumerKind, RealtimeWatermarkSeed, RuntimeRealtimeCluster,
    RuntimeRealtimeClusterDelivery, RuntimeRealtimeClusterEvent, RuntimeRealtimeIdentityBackend,
    RuntimeRealtimeTransactionBackend, RuntimeSyncBackend, RuntimeSyncHistory,
    RuntimeSyncSnapshotRequest, RuntimeSyncSubscriptionCreate, RuntimeTreeQueryBackend,
    RuntimeTreeQueryResponse, TransactionIsnTracker, await_cluster_while_owned, client_versions,
    note_sdk_versions, sync_changes_from_wal, validate_sync_query,
};
use crate::runtime_stream::{
    InMemoryRuntimeStreamBackend, RuntimeStreamAppendRequest, RuntimeStreamAppendResult,
    RuntimeStreamBackend, RuntimeStreamLoadRequest, RuntimeStreamStartRequest,
};
use crate::{AdminDataAuthorizer, AdminDataScope, RealtimeInvalidationConsumer};

const CRITICAL_STREAM_EVENT_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const SYNC_FRAME_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const CLUSTER_SUBSCRIBE_RETRY_MIN: Duration = Duration::from_millis(250);
const CLUSTER_SUBSCRIBE_RETRY_MAX: Duration = Duration::from_secs(5);

/// Live admin SSE sessions plus query/mutation message processing.
pub struct AdminRealtimeService<A, D> {
    authorizer: Arc<A>,
    data: Arc<D>,
    stream_backend: Arc<dyn RuntimeStreamBackend>,
    app_status: Option<Arc<dyn crate::EffectiveAppStatusPolicy>>,
    transaction_isn_wait: Duration,
    cluster: Option<Arc<dyn RuntimeRealtimeCluster>>,
    inner: Arc<AdminRealtimeInner>,
}

impl<A, D> Clone for AdminRealtimeService<A, D> {
    fn clone(&self) -> Self {
        Self {
            authorizer: Arc::clone(&self.authorizer),
            data: Arc::clone(&self.data),
            stream_backend: Arc::clone(&self.stream_backend),
            app_status: self.app_status.clone(),
            transaction_isn_wait: self.transaction_isn_wait,
            cluster: self.cluster.clone(),
            inner: Arc::clone(&self.inner),
        }
    }
}

struct AdminRealtimeInner {
    machine_id: Uuid,
    sessions: Mutex<HashMap<Uuid, SessionEntry>>,
    processed: Mutex<HashMap<Uuid, ProcessedWatermark>>,
    watermark_seed: Arc<RealtimeWatermarkSeed>,
    streams: Mutex<AdminStreamState>,
    sync: Mutex<AdminSyncState>,
    sync_history: Arc<RuntimeSyncHistory>,
    rooms: Mutex<RoomStore>,
    local_broadcasts: Mutex<HashMap<Uuid, usize>>,
    transaction_isns: TransactionIsnTracker,
    cluster_deliveries: ClusterDeliveryTracker,
}

#[derive(Default)]
struct AdminStreamState {
    writers: HashMap<Uuid, Uuid>,
    readers: HashMap<(Uuid, String), AdminStreamReader>,
}

#[derive(Default)]
struct AdminSyncState {
    active: HashMap<(Uuid, Uuid), AdminActiveSyncSubscription>,
    stale_subscriptions: HashMap<Uuid, Uuid>,
}

#[derive(Clone)]
struct AdminActiveSyncSubscription {
    app: Uuid,
    subscription: Uuid,
    sent_tx: i64,
    attribute_ids: Vec<Uuid>,
    ready: bool,
    draining: bool,
}

struct AdminStartSyncGuard<D: RuntimeSyncBackend> {
    inner: Arc<AdminRealtimeInner>,
    data: Arc<D>,
    context: AdminRequestContext,
    session_id: Uuid,
    subscription_id: Uuid,
    create_task: Option<tokio::task::JoinHandle<Result<(), ApiError>>>,
    durable: bool,
    armed: bool,
}

impl<D: RuntimeSyncBackend> AdminStartSyncGuard<D> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<D: RuntimeSyncBackend> Drop for AdminStartSyncGuard<D> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        sync_state(&self.inner)
            .active
            .remove(&(self.session_id, self.subscription_id));
        let data = Arc::clone(&self.data);
        let context = self.context.clone();
        let subscription_id = self.subscription_id;
        let create_task = self.create_task.take();
        let durable = self.durable;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let should_delete = if let Some(create_task) = create_task {
                    matches!(create_task.await, Ok(Ok(())))
                } else {
                    durable
                };
                if should_delete {
                    let _ = RuntimeSyncBackend::delete_sync_subscription(
                        data.as_ref(),
                        context,
                        subscription_id,
                    )
                    .await;
                }
            });
        }
    }
}

#[derive(Clone)]
struct AdminStreamReader {
    session: Uuid,
    stream: Uuid,
    event: String,
}

#[derive(Clone)]
struct SessionEntry {
    app_id: Uuid,
    context: AdminRequestContext,
    inference: bool,
    versions: Option<Value>,
    token_hash: [u8; 32],
    sender: mpsc::Sender<Value>,
    queries: HashMap<String, RegisteredAdminQuery>,
    attributes: Value,
    user_id: Option<Uuid>,
}

#[derive(Clone)]
struct RegisteredAdminQuery {
    request: AdminQueryRequest,
    return_tree: bool,
    last_result: Option<RuntimeTreeQueryResponse>,
}

impl<A, D> AdminRealtimeService<A, D>
where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend
        + RuntimeSyncBackend
        + RuntimeTreeQueryBackend,
{
    #[must_use]
    pub fn new(authorizer: Arc<A>, data: Arc<D>, machine_id: Uuid) -> Self {
        Self {
            authorizer,
            data,
            stream_backend: Arc::new(InMemoryRuntimeStreamBackend::default()),
            app_status: None,
            transaction_isn_wait: DEFAULT_TRANSACTION_ISN_WAIT,
            cluster: None,
            inner: Arc::new(AdminRealtimeInner {
                machine_id,
                sessions: Mutex::new(HashMap::new()),
                processed: Mutex::new(HashMap::new()),
                watermark_seed: Arc::new(RealtimeWatermarkSeed::default()),
                streams: Mutex::new(AdminStreamState::default()),
                sync: Mutex::new(AdminSyncState::default()),
                sync_history: Arc::new(RuntimeSyncHistory::default()),
                rooms: Mutex::new(RoomStore::default()),
                local_broadcasts: Mutex::new(HashMap::new()),
                transaction_isns: TransactionIsnTracker::default(),
                cluster_deliveries: ClusterDeliveryTracker::default(),
            }),
        }
    }

    /// Shares startup/global transaction and ISN maxima with runtime sessions.
    ///
    /// # Panics
    ///
    /// Panics when called after the service's internal state has been cloned.
    #[must_use]
    pub fn with_watermark_seed(mut self, seed: Arc<RealtimeWatermarkSeed>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("watermark seed must be configured before cloning the service")
            .watermark_seed = seed;
        self
    }

    #[must_use]
    pub fn with_transaction_isn_wait(mut self, duration: Duration) -> Self {
        self.transaction_isn_wait = duration;
        self
    }

    #[must_use]
    /// Shares process-lifetime sync history with runtime sessions.
    ///
    /// # Panics
    ///
    /// Panics when called after the service's internal state has been cloned.
    pub fn with_sync_history(mut self, history: Arc<RuntimeSyncHistory>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("sync history must be configured before cloning the service")
            .sync_history = history;
        self
    }

    #[must_use]
    pub fn with_effective_app_status_policy(
        mut self,
        policy: Arc<dyn crate::EffectiveAppStatusPolicy>,
    ) -> Self {
        self.app_status = Some(policy);
        self
    }

    /// Subscribes admin SSE sessions to WAL committed by replication workers on
    /// other machines in the realtime cluster.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn with_cluster(mut self, cluster: &Arc<dyn RuntimeRealtimeCluster>) -> Self {
        self.cluster = Some(Arc::clone(cluster));
        let cluster = Arc::clone(cluster);
        let owner = Arc::downgrade(&self.inner);
        let authorizer = Arc::clone(&self.authorizer);
        let data = Arc::clone(&self.data);
        let stream_backend = Arc::clone(&self.stream_backend);
        let app_status = self.app_status.clone();
        let transaction_isn_wait = self.transaction_isn_wait;
        tokio::spawn(async move {
            let mut retry_delay = CLUSTER_SUBSCRIBE_RETRY_MIN;
            loop {
                if owner.upgrade().is_none() {
                    break;
                }
                let Some(subscription) = await_cluster_while_owned(
                    &owner,
                    cluster.subscribe(RealtimeClusterConsumerKind::Admin),
                )
                .await
                else {
                    return;
                };
                let Ok(mut events) = subscription else {
                    if await_cluster_while_owned(&owner, tokio::time::sleep(retry_delay))
                        .await
                        .is_none()
                    {
                        return;
                    }
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
                    continue;
                };
                loop {
                    let Some(received) = await_cluster_while_owned(&owner, events.recv()).await
                    else {
                        return;
                    };
                    let delivery = match received {
                        Ok(delivery) => Some(delivery),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            if await_cluster_while_owned(&owner, tokio::time::sleep(retry_delay))
                                .await
                                .is_none()
                            {
                                return;
                            }
                            retry_delay = retry_delay
                                .saturating_mul(2)
                                .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
                            break;
                        }
                    };
                    let Some(inner) = owner.upgrade() else {
                        return;
                    };
                    let service = AdminRealtimeService {
                        authorizer: Arc::clone(&authorizer),
                        data: Arc::clone(&data),
                        stream_backend: Arc::clone(&stream_backend),
                        app_status: app_status.clone(),
                        transaction_isn_wait,
                        cluster: Some(Arc::clone(&cluster)),
                        inner,
                    };
                    if let Some(delivery) = delivery {
                        let event_id = delivery.event_id;
                        deliver_cluster_delivery(&service, delivery).await;
                        drop(service);
                        loop {
                            let Some(acknowledgement) = await_cluster_while_owned(
                                &owner,
                                cluster.acknowledge(RealtimeClusterConsumerKind::Admin, event_id),
                            )
                            .await
                            else {
                                return;
                            };
                            if acknowledgement.is_ok() {
                                break;
                            }
                            if await_cluster_while_owned(
                                &owner,
                                tokio::time::sleep(CLUSTER_SUBSCRIBE_RETRY_MIN),
                            )
                            .await
                            .is_none()
                            {
                                return;
                            }
                        }
                    } else {
                        service.recover_cluster_lag().await;
                    }
                    retry_delay = CLUSTER_SUBSCRIBE_RETRY_MIN;
                }
            }
        });
        self
    }

    async fn apply_wal_record(&self, record: &WalRecord) -> Result<(), String> {
        let tx_id = u64::try_from(record.tx_id)
            .map_err(|_| "committed tx-id cannot be negative".to_owned())?;
        self.inner
            .transaction_isns
            .deliver(record.tx_id, record.isn);
        self.publish_sync_transaction(record.app_id, record.tx_id, sync_changes_from_wal(record))
            .await;
        let catalog = self
            .authorizer
            .load_catalog(record.app_id)
            .await
            .map_err(|error| error.body().message.clone())?;
        let attributes = Value::Array(
            catalog
                .attributes
                .iter()
                .map(crate::admin_data::framework_attr)
                .collect(),
        );
        self.refresh_app(record.app_id, tx_id, Some(record.isn), Some(attributes))
            .await;
        Ok(())
    }

    async fn apply_byop_record(&self, app_id: Uuid, record: &ByopWalRecord) {
        self.refresh_app(app_id, record.tx_id, None, None).await;
    }

    async fn recover_cluster_lag(&self) {
        let app_ids = sessions(&self.inner)
            .values()
            .map(|entry| entry.app_id)
            .collect::<std::collections::BTreeSet<_>>();
        for app_id in app_ids {
            let entries = sessions(&self.inner)
                .iter()
                .filter(|(_, entry)| entry.app_id == app_id)
                .map(|(session_id, entry)| (*session_id, entry.clone()))
                .collect::<Vec<_>>();
            let attributes = match self.authorizer.load_catalog(app_id).await {
                Ok(catalog) => Some(Value::Array(
                    catalog
                        .attributes
                        .iter()
                        .map(crate::admin_data::framework_attr)
                        .collect(),
                )),
                Err(error) => {
                    for (_, entry) in &entries {
                        send_error(&entry.sender, None, &error);
                    }
                    None
                }
            };
            self.refresh_entries(entries, None, attributes).await;
        }
    }

    #[must_use]
    pub fn with_stream_backend(mut self, backend: Arc<dyn RuntimeStreamBackend>) -> Self {
        self.stream_backend = backend;
        self
    }

    #[must_use]
    pub fn machine_id(&self) -> Uuid {
        self.inner.machine_id
    }

    #[must_use]
    pub fn session_count(&self) -> usize {
        sessions(&self.inner).len()
    }

    /// Live admin SSE sessions per SDK family and normalized version.
    #[must_use]
    pub fn sdk_sessions(&self) -> Vec<(String, String, u64)> {
        let entries = sessions(&self.inner);
        let mut counts = BTreeMap::new();
        for entry in entries.values() {
            let (sdk, version) = sdk_session(&client_versions(entry.versions.as_ref()));
            *counts.entry((sdk.to_owned(), version)).or_insert(0_u64) += 1;
        }
        counts
            .into_iter()
            .map(|((sdk, version), count)| (sdk, version, count))
            .collect()
    }

    /// Gradually closes every local admin SSE session during process drain.
    pub async fn drain_all_connections(&self, total: Duration, max_gap: Duration) -> usize {
        let session_ids = sessions(&self.inner).keys().copied().collect::<Vec<_>>();
        let count = session_ids.len();
        let gap = u32::try_from(count)
            .ok()
            .filter(|count| *count > 0)
            .map_or(Duration::ZERO, |count| (total / count).min(max_gap));
        for session_id in session_ids {
            if let Some(entry) = sessions(&self.inner).remove(&session_id) {
                let app_id = entry.app_id;
                drop(entry);
                let cleanup = self.clone();
                tokio::spawn(async move {
                    cleanup.disconnect_removed(session_id, app_id).await;
                });
            }
            tokio::time::sleep(gap).await;
        }
        count
    }

    fn apply_cache_eviction(&self, eviction: &WalCacheEviction) {
        match eviction {
            WalCacheEviction::StreamMachine {
                stream_id,
                machine_id,
            } if *machine_id != self.inner.machine_id => {
                // Keep readers subscribed for delivery from the replacement machine,
                // but fence the stale local writer before it can append again.
                stream_state(&self.inner).writers.remove(stream_id);
            }
            WalCacheEviction::TransactionMaxSeen { tx_id } => {
                if let Some(tx_id) = tx_id {
                    self.inner.watermark_seed.observe_tx_id(*tx_id);
                }
            }
            // Runtime owns app/query cache effects. Users and webhooks have no
            // process-local Rust model cache.
            WalCacheEviction::Attribute { .. }
            | WalCacheEviction::Rules { .. }
            | WalCacheEviction::App { .. }
            | WalCacheEviction::InstantUser { .. }
            | WalCacheEviction::StreamMachine { .. }
            | WalCacheEviction::Webhook { .. } => {}
        }
    }

    /// Recomputes all registered queries for sessions selected by WAL invalidation.
    pub async fn refresh_sessions(&self, session_ids: &[Uuid]) {
        let entries = {
            let sessions = sessions(&self.inner);
            session_ids
                .iter()
                .filter_map(|session_id| {
                    sessions
                        .get(session_id)
                        .cloned()
                        .map(|entry| (*session_id, entry))
                })
                .collect::<Vec<_>>()
        };
        self.refresh_entries(entries, None, None).await;
    }

    async fn refresh_app(
        &self,
        app_id: Uuid,
        tx_id: u64,
        isn: Option<Isn>,
        attributes: Option<Value>,
    ) {
        let processed = advance_processed(&self.inner, app_id, tx_id, isn);
        let entries = sessions(&self.inner)
            .iter()
            .filter(|(_, entry)| entry.app_id == app_id)
            .map(|(session_id, entry)| (*session_id, entry.clone()))
            .collect::<Vec<_>>();
        self.refresh_entries(entries, Some(processed), attributes)
            .await;
    }

    async fn refresh_entries(
        &self,
        entries: Vec<(Uuid, SessionEntry)>,
        processed: Option<ProcessedWatermark>,
        attributes: Option<Value>,
    ) {
        for (session_id, entry) in entries {
            let attributes_changed = attributes
                .as_ref()
                .is_some_and(|attributes| attributes != &entry.attributes);
            let mut computations = Vec::new();
            for (key, registered) in &entry.queries {
                match self.execute_query(entry.context.clone(), registered).await {
                    Ok(result) => {
                        if !attributes_changed && registered.last_result.as_ref() == Some(&result) {
                            continue;
                        }
                        if let Some(current) = sessions(&self.inner).get_mut(&session_id)
                            && let Some(query) = current.queries.get_mut(key)
                        {
                            query.last_result = Some(result.clone());
                        }
                        computations.push(json!({
                            "instaql-query": registered.request.query,
                            "instaql-result": result.result,
                            "result-meta": result.result_meta,
                        }));
                    }
                    Err(error) => {
                        send_error(&entry.sender, None, &error);
                    }
                }
            }
            if computations.is_empty() && !attributes_changed {
                continue;
            }
            if attributes_changed && let Some(current) = sessions(&self.inner).get_mut(&session_id)
            {
                current.attributes = attributes.clone().expect("attributes are present");
            }
            let mut event = json!({
                "op": "refresh-ok",
                "computations": computations,
                "processed-tx-id": processed.and_then(|watermark| watermark.tx_id),
                "processed-isn": processed.and_then(|watermark| watermark.isn),
            });
            if let Some(attributes) = &attributes {
                event["attrs"] = attributes.clone();
            }
            send(&entry.sender, event);
        }
    }

    async fn execute_query(
        &self,
        context: AdminRequestContext,
        registered: &RegisteredAdminQuery,
    ) -> Result<RuntimeTreeQueryResponse, ApiError> {
        if registered.return_tree {
            return self
                .data
                .query_tree(context, registered.request.clone())
                .await;
        }
        self.data
            .query_raw(context, registered.request.clone())
            .await
            .map(|result| RuntimeTreeQueryResponse {
                result,
                result_meta: Value::Null,
                instant_topics: None,
            })
    }

    async fn join_room(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        if let Some(policy) = &self.app_status {
            policy
                .status(entry.app_id)
                .await?
                .effective_status
                .assert_read_allowed()?;
        }
        let room_id = required_room_id(message)?;
        let mut data = message.get("data").cloned().unwrap_or_else(|| json!({}));
        if matches!(data, Value::Null | Value::Bool(false)) {
            data = json!({});
        }
        let update = if let Some(cluster) = &self.cluster {
            let room = RoomKey::new(entry.app_id, room_id);
            rooms(&self.inner).register_local_session(room.clone(), session_id);
            let snapshot = cluster
                .join(
                    &room,
                    PeerPresence {
                        peer_id: session_id,
                        instance_id: self.inner.machine_id.to_string(),
                        user: entry.user_id.map(|id| PeerUser { id }),
                        data,
                    },
                )
                .await
                .map_err(cluster_error);
            match snapshot {
                Ok(snapshot) => rooms(&self.inner).apply_versioned_remote_snapshot(
                    &room,
                    snapshot.revision,
                    snapshot.peers,
                ),
                Err(error) => {
                    rooms(&self.inner).unregister_local_session(&room, session_id);
                    return Err(error);
                }
            }
        } else {
            rooms(&self.inner)
                .join(
                    entry.app_id,
                    room_id,
                    session_id,
                    self.inner.machine_id.to_string(),
                    entry.user_id,
                    data,
                )
                .map_err(room_error)?
        };
        self.deliver_presence(update).await?;
        send_sync_frame(
            &entry.sender,
            event_with_client_id(
                json!({"op": "join-room-ok", "room-id": room_id}),
                client_event_id,
            ),
        )
        .await?;
        Ok(())
    }

    async fn leave_room(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let room_id = required_room_id(message)?;
        let update = if let Some(cluster) = &self.cluster {
            let room = RoomKey::new(entry.app_id, room_id);
            let snapshot = cluster
                .leave(&room, session_id)
                .await
                .map_err(cluster_error)?;
            let update = rooms(&self.inner).apply_versioned_remote_snapshot(
                &room,
                snapshot.revision,
                snapshot.peers,
            );
            rooms(&self.inner).unregister_local_session(&room, session_id);
            update
        } else {
            rooms(&self.inner)
                .leave(entry.app_id, room_id, session_id)
                .map_err(room_error)?
        };
        self.deliver_presence(update).await?;
        send_sync_frame(
            &entry.sender,
            event_with_client_id(
                json!({"op": "leave-room-ok", "room-id": room_id}),
                client_event_id,
            ),
        )
        .await?;
        Ok(())
    }

    async fn set_presence(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let room_id = required_room_id(message)?;
        let data = message.get("data").cloned().unwrap_or(Value::Null);
        let update = if let Some(cluster) = &self.cluster {
            if !rooms(&self.inner).in_room(entry.app_id, room_id, session_id) {
                return Err(room_error(instant_realtime::RoomError::NotInRoom {
                    session_id,
                    room_id: room_id.to_owned(),
                }));
            }
            let room = RoomKey::new(entry.app_id, room_id);
            let snapshot = cluster
                .set_presence(&room, session_id, data)
                .await
                .map_err(cluster_error)?;
            rooms(&self.inner).apply_versioned_remote_snapshot(
                &room,
                snapshot.revision,
                snapshot.peers,
            )
        } else {
            rooms(&self.inner)
                .set_presence(entry.app_id, room_id, session_id, data)
                .map_err(room_error)?
        };
        self.deliver_presence(update).await?;
        send_sync_frame(
            &entry.sender,
            event_with_client_id(
                json!({"op": "set-presence-ok", "room-id": room_id}),
                client_event_id,
            ),
        )
        .await?;
        Ok(())
    }

    async fn broadcast(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let room_id = required_room_id(message)?;
        let plan = rooms(&self.inner)
            .broadcast(
                entry.app_id,
                room_id,
                session_id,
                entry.user_id,
                message.get("topic").cloned().unwrap_or(Value::Null),
                message.get("data").cloned().unwrap_or(Value::Null),
            )
            .map_err(room_error)?;
        deliver_broadcast_to_admin_sessions(
            &self.inner,
            &RoomKey::new(entry.app_id, room_id),
            &plan.recipient_payload,
        )
        .await?;
        if let Some(cluster) = &self.cluster {
            record_local_broadcast(&self.inner, session_id);
            if let Err(error) = cluster
                .publish_broadcast(
                    &RoomKey::new(entry.app_id, room_id),
                    plan.recipient_payload.clone(),
                )
                .await
            {
                consume_local_broadcast(&self.inner, session_id);
                return Err(cluster_error(error));
            }
        }
        send_sync_frame(
            &entry.sender,
            event_with_client_id(
                json!({
                    "op": "client-broadcast-ok",
                    "room-id": plan.sender_ack.room_id,
                    "topic": plan.sender_ack.topic,
                    "data": plan.sender_ack.data,
                }),
                client_event_id,
            ),
        )
        .await?;
        Ok(())
    }

    async fn deliver_presence(&self, update: PresenceUpdate) -> Result<(), ApiError> {
        for delivery in update.deliveries {
            let sender = sessions(&self.inner)
                .get(&delivery.session_id)
                .map(|entry| entry.sender.clone());
            if let Some(sender) = sender {
                send_sync_frame(
                    &sender,
                    json!({
                        "op": "refresh-presence",
                        "room-id": delivery.room.room_id,
                        "data": delivery.data,
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_message(&self, session_id: Uuid, entry: SessionEntry, message: Value) {
        let Some(object) = message.as_object() else {
            send_protocol_error(&entry.sender, None, "Invalid message");
            return;
        };
        let client_event_id = object.get("client-event-id").cloned();
        match object.get("op").and_then(Value::as_str) {
            Some("add-query") => {
                let Some(query) = object.get("q").and_then(Value::as_object).cloned() else {
                    send_protocol_event_error(
                        &entry.sender,
                        client_event_id,
                        object,
                        "Query can not be null.",
                    );
                    return;
                };
                let request = AdminQueryRequest {
                    query,
                    inference: entry.inference,
                    versions: entry.versions.clone(),
                };
                let mut registered = RegisteredAdminQuery {
                    request: request.clone(),
                    return_tree: object.get("return-type").and_then(Value::as_str) == Some("tree"),
                    last_result: None,
                };
                let key = canonical_query(&request.query);
                if entry.queries.contains_key(&key) {
                    send(
                        &entry.sender,
                        event_with_client_id(
                            json!({"op": "add-query-exists", "q": request.query}),
                            client_event_id,
                        ),
                    );
                    return;
                }
                let processed = processed_watermark(&self.inner, entry.app_id);
                match self.execute_query(entry.context.clone(), &registered).await {
                    Ok(result) => {
                        registered.last_result = Some(result.clone());
                        if let Some(current) = sessions(&self.inner).get_mut(&session_id) {
                            current.queries.insert(key, registered);
                        }
                        send(
                            &entry.sender,
                            event_with_client_id(
                                json!({
                                    "op": "add-query-ok",
                                    "q": request.query,
                                    "result": result.result,
                                    "result-meta": result.result_meta,
                                    "processed-tx-id": processed.tx_id,
                                    "processed-isn": processed.isn,
                                }),
                                client_event_id,
                            ),
                        );
                    }
                    Err(error) => send_stream_error(&entry.sender, client_event_id, object, &error),
                }
            }
            Some("remove-query") => {
                let query = object.get("q").cloned().unwrap_or(Value::Null);
                if let Some(query_object) = query.as_object()
                    && let Some(current) = sessions(&self.inner).get_mut(&session_id)
                {
                    current.queries.remove(&canonical_query(query_object));
                }
                send(
                    &entry.sender,
                    event_with_client_id(
                        json!({"op": "remove-query-ok", "q": query}),
                        client_event_id,
                    ),
                );
            }
            Some("transact") => {
                let Some(steps) = object.get("tx-steps").and_then(Value::as_array).cloned() else {
                    send_protocol_event_error(
                        &entry.sender,
                        client_event_id,
                        object,
                        "Invalid transaction steps",
                    );
                    return;
                };
                match self
                    .data
                    .transact_realtime(
                        entry.context.clone(),
                        AdminTransactRequest {
                            steps,
                            throw_on_missing_attrs: false,
                        },
                    )
                    .await
                {
                    Ok(transaction) => {
                        let isn = self
                            .inner
                            .transaction_isns
                            .wait(transaction.tx_id, self.transaction_isn_wait)
                            .await
                            .unwrap_or(transaction.fallback_isn);
                        send(
                            &entry.sender,
                            event_with_client_id(
                                json!({
                                    "op": "transact-ok",
                                    "tx-id": transaction.tx_id,
                                    "isn": isn,
                                }),
                                client_event_id,
                            ),
                        );
                    }
                    Err(error) => send_stream_error(&entry.sender, client_event_id, object, &error),
                }
            }
            Some("start-sync") => {
                if let Err(error) = self
                    .start_sync(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("remove-sync") => {
                if let Err(error) = self.remove_sync(session_id, &entry, object).await {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("refresh-sync-table") => {
                if let Err(error) = self.refresh_sync_table(session_id, object).await {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("resync-table") => {
                if let Err(error) = Self::resync_table(session_id, &entry, object) {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("join-room") => {
                if let Err(error) = self
                    .join_room(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("leave-room") => {
                if let Err(error) = self
                    .leave_room(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("set-presence") => {
                if let Err(error) = self
                    .set_presence(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("client-broadcast") => {
                if let Err(error) = self
                    .broadcast(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("start-stream") => {
                if let Err(error) = self
                    .start_stream(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("append-stream") => {
                if let Err(error) = self.append_stream(session_id, &entry, object).await {
                    let event = stream_error_event(client_event_id, object, &error);
                    let _ = send_critical_stream_event(&entry.sender, event).await;
                }
            }
            Some("subscribe-stream") => {
                if let Err(error) = self
                    .subscribe_stream(session_id, &entry, object, client_event_id.clone())
                    .await
                {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            Some("unsubscribe-stream") => {
                if let Err(error) = self.unsubscribe_stream(session_id, object) {
                    send_stream_error(&entry.sender, client_event_id, object, &error);
                }
            }
            _ => {
                let error = ApiError::new(ApiErrorType::ParamMalformed, "Invalid op")
                    .with_hint(json!({"op": object.get("op")}));
                send_stream_error(&entry.sender, client_event_id, object, &error);
            }
        }
    }

    async fn start_stream(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let client_id = required_string(message, "client-id")?.to_owned();
        let reconnect_token = required_uuid(message, "reconnect-token")?;
        let record = self
            .stream_backend
            .start_stream(RuntimeStreamStartRequest {
                context: entry.context.clone(),
                client_id,
                reconnect_token,
                machine_id: self.inner.machine_id,
            })
            .await?;
        stream_state(&self.inner)
            .writers
            .insert(record.stream_id, session_id);
        send(
            &entry.sender,
            event_with_client_id(
                json!({
                    "op": "start-stream-ok",
                    "client-id": record.client_id,
                    "stream-id": record.stream_id,
                    "offset": record.size,
                }),
                client_event_id,
            ),
        );
        Ok(())
    }

    async fn append_stream(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        let stream_id = required_uuid(message, "stream-id")?;
        if stream_state(&self.inner).writers.get(&stream_id) != Some(&session_id) {
            return Err(protocol_error("Stream updated from a different machine."));
        }
        let appended = self
            .stream_backend
            .append_stream(RuntimeStreamAppendRequest {
                context: entry.context.clone(),
                stream_id,
                machine_id: self.inner.machine_id,
                expected_offset: required_non_negative_u64(message, "offset")?,
                chunks: required_string_array(message, "chunks")?,
                done: optional_bool(message, "done")?.unwrap_or(false),
                abort_reason: message.get("abort-reason").map(json_string).transpose()?,
            })
            .await?;
        let readers = stream_state(&self.inner)
            .readers
            .values()
            .filter(|reader| reader.stream == stream_id)
            .cloned()
            .collect::<Vec<_>>();
        futures_util::future::join_all(
            readers
                .iter()
                .map(|reader| self.deliver_stream_append(reader, &appended)),
        )
        .await;
        if let Some(flushed_offset) = appended.flushed_offset {
            let event = json!({
                "op": "stream-flushed",
                "stream-id": stream_id,
                "offset": flushed_offset,
                "done": appended.done,
            });
            if appended.done {
                send_critical_stream_event(&entry.sender, event).await?;
            } else {
                send(&entry.sender, event);
            }
        } else if appended.done {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "Completed stream did not return a durable flush acknowledgement.",
            ));
        }
        if appended.done {
            stream_state(&self.inner).writers.remove(&stream_id);
        }
        Ok(())
    }

    async fn deliver_stream_append(
        &self,
        reader: &AdminStreamReader,
        appended: &RuntimeStreamAppendResult,
    ) {
        let Some(sender) = sessions(&self.inner)
            .get(&reader.session)
            .map(|session| session.sender.clone())
        else {
            return;
        };
        let delivery = async {
            if !appended.content.is_empty() {
                let event = json!({
                    "op": "stream-append",
                    "client-event-id": reader.event,
                    "stream-id": appended.stream_id,
                    "client-id": appended.client_id,
                    "offset": appended.offset,
                    "content": appended.content,
                });
                if appended.done {
                    send_critical_stream_event(&sender, event).await?;
                } else {
                    send(&sender, event);
                }
            }
            if appended.done {
                send_critical_stream_event(
                    &sender,
                    json!({
                        "op": "stream-append",
                        "client-event-id": reader.event,
                        "stream-id": appended.stream_id,
                        "client-id": appended.client_id,
                        "offset": appended.new_offset,
                        "done": true,
                        "abort-reason": appended.abort_reason,
                    }),
                )
                .await?;
            }
            Ok::<(), ApiError>(())
        }
        .await;
        if let Err(error) = delivery {
            tracing::warn!(
                stream_id = %appended.stream_id,
                reader_session_id = %reader.session,
                reader_event_id = %reader.event,
                error = %error.body().message,
                "retaining resumable-stream reader after terminal event delivery failed"
            );
        } else if appended.done {
            stream_state(&self.inner)
                .readers
                .remove(&(reader.session, reader.event.clone()));
        }
    }

    async fn subscribe_stream(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let stream_id = optional_uuid(message, "stream-id")?;
        let client_id = optional_non_empty_string(message, "client-id")?;
        if stream_id.is_none() && client_id.is_none() {
            return Err(protocol_error(
                "Must provide either a stream-id or a client-id",
            ));
        }
        let event_id = client_event_id
            .as_ref()
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| protocol_error("Invalid client-event-id"))?
            .to_owned();
        let record = self
            .stream_backend
            .load_stream(RuntimeStreamLoadRequest {
                context: entry.context.clone(),
                stream_id,
                client_id,
                requested_offset: optional_non_negative_u64(message, "offset")?.unwrap_or(0),
            })
            .await?;
        if !record.done {
            stream_state(&self.inner).readers.insert(
                (session_id, event_id.clone()),
                AdminStreamReader {
                    session: session_id,
                    stream: record.stream_id,
                    event: event_id.clone(),
                },
            );
        }
        let files = record
            .files
            .iter()
            .map(|file| json!({"id": file.id, "size": file.size, "url": file.url}))
            .collect::<Vec<_>>();
        send(
            &entry.sender,
            json!({
                "op": "stream-append",
                "client-event-id": event_id,
                "stream-id": record.stream_id,
                "client-id": record.client_id,
                "offset": record.replay_offset,
                "files": files,
                "content": record.content,
                "done": record.done,
                "abort-reason": record.abort_reason,
            }),
        );
        Ok(())
    }

    fn unsubscribe_stream(
        &self,
        session_id: Uuid,
        message: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        let event_id = required_string(message, "subscribe-event-id")?;
        if stream_state(&self.inner)
            .readers
            .remove(&(session_id, event_id.to_owned()))
            .is_none()
        {
            return Err(protocol_error("Stream subscription is missing."));
        }
        Ok(())
    }

    async fn start_sync(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
        client_event_id: Option<Value>,
    ) -> Result<(), ApiError> {
        let query = validated_start_sync_query(entry, message)?;
        let snapshot = RuntimeSyncBackend::sync_snapshot(
            self.data.as_ref(),
            RuntimeSyncSnapshotRequest {
                context: entry.context.clone(),
                query: query.clone(),
            },
        )
        .await?;
        let subscription_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        sync_state(&self.inner).active.insert(
            (session_id, subscription_id),
            AdminActiveSyncSubscription {
                app: entry.app_id,
                subscription: subscription_id,
                sent_tx: snapshot.tx_id,
                attribute_ids: snapshot.attribute_ids.clone(),
                ready: false,
                draining: true,
            },
        );
        let mut ownership = AdminStartSyncGuard {
            inner: Arc::clone(&self.inner),
            data: Arc::clone(&self.data),
            context: entry.context.clone(),
            session_id,
            subscription_id,
            create_task: None,
            durable: false,
            armed: true,
        };
        send_sync_frame(
            &entry.sender,
            event_with_client_id(
                json!({
                    "op": "start-sync-ok",
                    "q": query,
                    "subscription-id": subscription_id,
                    "token": token,
                }),
                client_event_id,
            ),
        )
        .await?;
        for batch in snapshot.join_rows.chunks(100) {
            send_sync_frame(
                &entry.sender,
                json!({
                    "op": "sync-load-batch",
                    "subscription-id": subscription_id,
                    "join-rows": batch,
                }),
            )
            .await?;
        }
        let data = Arc::clone(&self.data);
        let create_request = RuntimeSyncSubscriptionCreate {
            context: entry.context.clone(),
            subscription_id,
            token,
            query,
            attribute_ids: snapshot.attribute_ids,
        };
        ownership.create_task = Some(tokio::spawn(async move {
            RuntimeSyncBackend::create_sync_subscription(data.as_ref(), create_request).await
        }));
        let create_result = ownership
            .create_task
            .as_mut()
            .expect("sync create task was installed")
            .await
            .map_err(internal_error)?;
        ownership.create_task = None;
        create_result?;
        ownership.durable = true;
        send_sync_frame(
            &entry.sender,
            json!({
                "op": "sync-init-finish",
                "subscription-id": subscription_id,
                "tx-id": snapshot.tx_id,
            }),
        )
        .await?;
        self.drain_sync_transactions(session_id, subscription_id, true)
            .await?;
        ownership.disarm();
        Ok(())
    }

    async fn remove_sync(
        &self,
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        let subscription_id = required_uuid(message, "subscription-id")?;
        let (removed, stale) = {
            let mut sync = sync_state(&self.inner);
            let removed = sync
                .active
                .remove(&(session_id, subscription_id))
                .is_some_and(|subscription| subscription.app == entry.app_id);
            let stale = if sync.stale_subscriptions.get(&subscription_id) == Some(&entry.app_id) {
                sync.stale_subscriptions.remove(&subscription_id);
                true
            } else {
                false
            };
            (removed, stale)
        };
        let keep = message
            .get("keep-subscription")
            .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)));
        if stale || (removed && !keep) {
            RuntimeSyncBackend::delete_sync_subscription(
                self.data.as_ref(),
                entry.context.clone(),
                subscription_id,
            )
            .await?;
        }
        Ok(())
    }

    async fn refresh_sync_table(
        &self,
        session_id: Uuid,
        message: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        let Some(subscription_id) = message
            .get("subscription-id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<Uuid>().ok())
        else {
            return Ok(());
        };
        let claimed = {
            let mut state = sync_state(&self.inner);
            let Some(subscription) = state.active.get_mut(&(session_id, subscription_id)) else {
                return Ok(());
            };
            if !subscription.ready || subscription.draining {
                false
            } else {
                subscription.draining = true;
                true
            }
        };
        if claimed
            && let Err(error) = self
                .drain_sync_transactions(session_id, subscription_id, false)
                .await
        {
            self.mark_sync_stale(session_id, subscription_id);
            return Err(error);
        }
        Ok(())
    }

    fn resync_table(
        session_id: Uuid,
        entry: &SessionEntry,
        message: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        let _ = (session_id, entry);
        required_i64(message, "tx-id")?;
        required_uuid(message, "subscription-id")?;
        required_uuid(message, "token")?;
        Err(ApiError::new(
            ApiErrorType::Unknown,
            "Sync subscriptions restart when reconnecting.",
        ))
    }

    async fn drain_sync_transactions(
        &self,
        session_id: Uuid,
        subscription_id: Uuid,
        initializing: bool,
    ) -> Result<(), ApiError> {
        let sender = sessions(&self.inner)
            .get(&session_id)
            .map(|entry| entry.sender.clone())
            .ok_or_else(|| session_missing(session_id))?;
        let subscription = sync_state(&self.inner)
            .active
            .get(&(session_id, subscription_id))
            .cloned()
            .ok_or_else(|| session_missing(session_id))?;
        let (latest_tx, _) = self
            .inner
            .sync_history
            .transactions_after(
                subscription.app,
                subscription.sent_tx,
                &subscription.attribute_ids,
            )
            .map_err(|()| stale_sync_error())?;
        let Some(_) = latest_tx else {
            if let Some(active) = sync_state(&self.inner)
                .active
                .get_mut(&(session_id, subscription_id))
            {
                active.ready |= initializing;
                active.draining = false;
            }
            return Ok(());
        };
        sync_state(&self.inner)
            .active
            .remove(&(session_id, subscription_id));
        sync_state(&self.inner)
            .stale_subscriptions
            .insert(subscription_id, subscription.app);
        send_sync_frame(
            &sender,
            json!({
                "op": "error",
                "status": 500,
                "client-event-id": Value::Null,
                "original-event": {
                    "op": "resync-table",
                    "subscription-id": subscription_id,
                },
                "type": ApiErrorType::Unknown,
                "message": "Sync data changed; restarting the sync table.",
                "hint": Value::Null,
            }),
        )
        .await?;
        Ok(())
    }

    async fn publish_sync_transaction(&self, app_id: Uuid, tx_id: i64, changes: Vec<Value>) {
        self.inner.sync_history.record(app_id, tx_id, changes);
        let (ready, stale) = {
            let mut sync = sync_state(&self.inner);
            let mut ready = Vec::new();
            let mut expired = Vec::new();
            for (key, subscription) in &sync.active {
                if subscription.app != app_id {
                    continue;
                }
                if self
                    .inner
                    .sync_history
                    .transactions_after(app_id, subscription.sent_tx, &subscription.attribute_ids)
                    .is_err()
                {
                    expired.push((*key, subscription.clone()));
                } else if subscription.ready && !subscription.draining {
                    ready.push(*key);
                }
            }
            for key in &ready {
                if let Some(subscription) = sync.active.get_mut(key) {
                    subscription.draining = true;
                }
            }
            for (key, subscription) in &expired {
                sync.active.remove(key);
                sync.stale_subscriptions
                    .insert(subscription.subscription, subscription.app);
            }
            (ready, expired)
        };
        for ((session_id, _subscription_id), _) in stale {
            if let Some(sender) = sessions(&self.inner)
                .get(&session_id)
                .map(|entry| entry.sender.clone())
            {
                send_error(&sender, None, &stale_sync_error());
            }
        }
        for (session_id, subscription_id) in ready {
            if let Err(error) = self
                .drain_sync_transactions(session_id, subscription_id, false)
                .await
            {
                self.mark_sync_stale(session_id, subscription_id);
                if let Some(sender) = sessions(&self.inner)
                    .get(&session_id)
                    .map(|entry| entry.sender.clone())
                {
                    send_error(&sender, None, &error);
                }
            }
        }
    }

    fn mark_sync_stale(&self, session_id: Uuid, subscription_id: Uuid) {
        let mut sync = sync_state(&self.inner);
        if let Some(subscription) = sync.active.remove(&(session_id, subscription_id)) {
            sync.stale_subscriptions
                .insert(subscription.subscription, subscription.app);
        }
    }

    async fn disconnect_removed(&self, session_id: Uuid, app_id: Uuid) {
        sync_state(&self.inner)
            .active
            .retain(|(owner_session, _), _| *owner_session != session_id);
        {
            let mut streams = stream_state(&self.inner);
            streams
                .readers
                .retain(|_, reader| reader.session != session_id);
            streams.writers.retain(|_, writer| *writer != session_id);
        }
        if let Some(cluster) = &self.cluster {
            let joined_rooms = rooms(&self.inner).local_rooms_for_session(session_id);
            for room in joined_rooms {
                rooms(&self.inner).unregister_local_session(&room, session_id);
                let mut retry_delay = CLUSTER_SUBSCRIBE_RETRY_MIN;
                let snapshot = loop {
                    if let Ok(snapshot) = cluster.leave(&room, session_id).await {
                        break snapshot;
                    }
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
                };
                let update = rooms(&self.inner).apply_versioned_remote_snapshot(
                    &room,
                    snapshot.revision,
                    snapshot.peers,
                );
                let _ = self.deliver_presence(update).await;
            }
            let mut retry_delay = CLUSTER_SUBSCRIBE_RETRY_MIN;
            while cluster.unregister_sse_session(session_id).await.is_err() {
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay
                    .saturating_mul(2)
                    .min(CLUSTER_SUBSCRIBE_RETRY_MAX);
            }
        } else {
            let updates = rooms(&self.inner).leave_all(session_id);
            for update in updates {
                let _ = self.deliver_presence(update).await;
            }
        }
        tracing::debug!(%app_id, %session_id, "admin realtime session disconnected");
    }
}

async fn deliver_cluster_event<A, D>(
    service: &AdminRealtimeService<A, D>,
    event: RuntimeRealtimeClusterEvent,
) where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend
        + RuntimeSyncBackend
        + RuntimeTreeQueryBackend,
{
    match event {
        RuntimeRealtimeClusterEvent::Presence {
            revision,
            room,
            peers,
            ..
        } => {
            let update =
                rooms(&service.inner).apply_versioned_remote_snapshot(&room, revision, peers);
            let _ = service.deliver_presence(update).await;
        }
        RuntimeRealtimeClusterEvent::Broadcast {
            source_machine_id,
            room,
            payload,
        } => {
            let local_admin_echo = source_machine_id == service.inner.machine_id
                && consume_local_broadcast(&service.inner, payload.data.peer_id);
            if !local_admin_echo {
                let _ = deliver_broadcast_to_admin_sessions(&service.inner, &room, &payload).await;
            }
        }
        RuntimeRealtimeClusterEvent::SsePush {
            target_machine_id,
            app_id,
            session_id,
            messages,
        } if target_machine_id == service.inner.machine_id => {
            for message in messages {
                let entry = sessions(&service.inner)
                    .get(&session_id)
                    .filter(|entry| entry.app_id == app_id)
                    .cloned();
                if let Some(entry) = entry {
                    service.handle_message(session_id, entry, message).await;
                }
            }
        }
        RuntimeRealtimeClusterEvent::Wal { record, .. } => {
            let has_sessions = sessions(&service.inner)
                .values()
                .any(|entry| entry.app_id == record.app_id);
            if has_sessions {
                while service.apply_wal_record(&record).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            } else {
                service
                    .publish_sync_transaction(
                        record.app_id,
                        record.tx_id,
                        sync_changes_from_wal(&record),
                    )
                    .await;
            }
        }
        RuntimeRealtimeClusterEvent::WalBatch { records } => {
            for record in records {
                if !sessions(&service.inner)
                    .values()
                    .any(|entry| entry.app_id == record.app_id)
                {
                    service
                        .publish_sync_transaction(
                            record.app_id,
                            record.tx_id,
                            sync_changes_from_wal(&record),
                        )
                        .await;
                    continue;
                }
                while service.apply_wal_record(&record).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        RuntimeRealtimeClusterEvent::ByopWal { app_id, record, .. }
            if sessions(&service.inner)
                .values()
                .any(|entry| entry.app_id == app_id) =>
        {
            service.apply_byop_record(app_id, &record).await;
        }
        _ => {}
    }
}

async fn deliver_cluster_delivery<A, D>(
    service: &AdminRealtimeService<A, D>,
    delivery: RuntimeRealtimeClusterDelivery,
) where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend
        + RuntimeSyncBackend
        + RuntimeTreeQueryBackend,
{
    if !service
        .inner
        .cluster_deliveries
        .should_apply(delivery.event_id)
    {
        return;
    }
    deliver_cluster_event(service, delivery.event).await;
    service
        .inner
        .cluster_deliveries
        .mark_applied(delivery.event_id);
}

impl<A, D> RealtimeInvalidationConsumer for AdminRealtimeService<A, D>
where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend
        + RuntimeSyncBackend
        + RuntimeTreeQueryBackend,
{
    fn requires_local_delivery(&self) -> bool {
        self.cluster.is_none()
    }

    async fn deliver(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        if self.cluster.is_some() {
            return Ok(());
        }
        let Some(record) = &outcome.record else {
            return Ok(());
        };
        self.apply_wal_record(record).await
    }

    async fn deliver_byop(
        &self,
        app_id: Uuid,
        record: &ByopWalRecord,
        _topics: &[ByopTopic],
    ) -> Result<(), String> {
        if self.cluster.is_some() {
            return Ok(());
        }
        self.apply_byop_record(app_id, record).await;
        Ok(())
    }

    fn evict_cache(
        &self,
        eviction: &WalCacheEviction,
    ) -> impl Future<Output = Result<(), String>> + Send {
        self.apply_cache_eviction(eviction);
        std::future::ready(Ok(()))
    }

    fn observe_max_seen_isn(
        &self,
        isn: Isn,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        self.inner.watermark_seed.observe_isn(isn);
        std::future::ready(Ok(()))
    }
}

impl<A, D> AdminRealtimeBackend for AdminRealtimeService<A, D>
where
    A: AdminDataAuthorizer,
    D: AdminDataBackend
        + RuntimeRealtimeIdentityBackend
        + RuntimeRealtimeTransactionBackend
        + RuntimeSyncBackend
        + RuntimeTreeQueryBackend,
{
    async fn open(
        &self,
        context: AdminRequestContext,
        request: AdminSseOpenRequest,
    ) -> Result<AdminSseSession, ApiError> {
        note_sdk_versions(request.versions.as_ref());
        let catalog = self
            .authorizer
            .authorize(context.clone(), AdminDataScope::Read)
            .await?;
        let user_id = self.data.resolve_realtime_user_id(context.clone()).await?;
        let attributes = Value::Array(
            catalog
                .attributes
                .iter()
                .map(crate::admin_data::framework_attr)
                .collect(),
        );
        let initial_query = request.query.map(|query| AdminQueryRequest {
            query,
            inference: request.inference,
            versions: request.versions.clone(),
        });
        let processed = processed_watermark(&self.inner, context.app_id);
        let mut initial_registered = initial_query.clone().map(|request| RegisteredAdminQuery {
            request,
            return_tree: true,
            last_result: None,
        });
        let initial_result = if let Some(query) = initial_registered.as_ref() {
            Some(self.execute_query(context.clone(), query).await?)
        } else {
            None
        };
        if let (Some(query), Some(result)) = (&mut initial_registered, &initial_result) {
            query.last_result = Some(result.clone());
        }
        let session_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let token_hash = token_hash(token);
        if let Some(cluster) = &self.cluster {
            cluster
                .register_sse_session(context.app_id, session_id, token_hash)
                .await
                .map_err(cluster_error)?;
        }
        let (sender, receiver) = mpsc::channel(64);
        let mut queries = HashMap::new();
        if let Some(query) = initial_registered.as_ref() {
            queries.insert(canonical_query(&query.request.query), query.clone());
        }
        sessions(&self.inner).insert(
            session_id,
            SessionEntry {
                app_id: context.app_id,
                context,
                inference: request.inference,
                versions: request.versions,
                token_hash,
                sender: sender.clone(),
                queries,
                attributes,
                user_id,
            },
        );
        send(
            &sender,
            json!({
                "op": "sse-init",
                "session-id": session_id,
                "machine-id": self.inner.machine_id,
                "sse-token": token,
            }),
        );
        if let (Some(request), Some(result)) = (initial_query, initial_result) {
            send(
                &sender,
                json!({
                    "op": "add-query-ok",
                    "q": request.query,
                    "result": result.result,
                    "result-meta": result.result_meta,
                    "processed-tx-id": processed.tx_id,
                    "processed-isn": processed.isn,
                    "client-event-id": Uuid::new_v4(),
                }),
            );
        }
        let service = self.clone();
        Ok(AdminSseSession::new(receiver, move || {
            let entry = { sessions(&service.inner).remove(&session_id) };
            if let Some(entry) = entry {
                let app_id = entry.app_id;
                drop(entry);
                let cleanup = service.clone();
                tokio::spawn(async move {
                    cleanup.disconnect_removed(session_id, app_id).await;
                });
            }
        }))
    }

    async fn push(&self, request: AdminSsePushRequest) -> Result<(), ApiError> {
        if request.machine_id != self.inner.machine_id {
            if let Some(cluster) = &self.cluster {
                if cluster
                    .forward_sse_push(RuntimeSsePushRequest {
                        machine_id: request.machine_id,
                        app_id: request.app_id,
                        session_id: request.session_id,
                        sse_token: request.sse_token,
                        messages: request.messages.clone(),
                    })
                    .await
                    .map_err(cluster_error)?
                {
                    return Ok(());
                }
                return Err(session_missing(request.session_id));
            }
            return Err(ApiError::new(
                ApiErrorType::MemberMissing,
                format!("Machine {} is not available", request.machine_id),
            ));
        }
        let entry = sessions(&self.inner)
            .get(&request.session_id)
            .cloned()
            .filter(|entry| entry.app_id == request.app_id)
            .ok_or_else(|| session_missing(request.session_id))?;
        if !constant_time_eq(&entry.token_hash, &token_hash(request.sse_token)) {
            return Err(session_missing(request.session_id));
        }
        for message in request.messages {
            let current = sessions(&self.inner)
                .get(&request.session_id)
                .cloned()
                .ok_or_else(|| session_missing(request.session_id))?;
            self.handle_message(request.session_id, current, message)
                .await;
        }
        Ok(())
    }
}

fn sessions(inner: &AdminRealtimeInner) -> std::sync::MutexGuard<'_, HashMap<Uuid, SessionEntry>> {
    inner
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn processed_watermark(inner: &AdminRealtimeInner, app_id: Uuid) -> ProcessedWatermark {
    let mut processed = inner
        .processed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *processed
        .entry(app_id)
        .or_insert_with(|| inner.watermark_seed.snapshot())
}

fn advance_processed(
    inner: &AdminRealtimeInner,
    app_id: Uuid,
    tx_id: u64,
    isn: Option<Isn>,
) -> ProcessedWatermark {
    let mut processed = inner
        .processed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let watermark = processed
        .entry(app_id)
        .or_insert_with(|| inner.watermark_seed.snapshot());
    watermark.advance(tx_id, isn);
    *watermark
}

fn canonical_query(query: &Map<String, Value>) -> String {
    serde_json::to_string(query).expect("JSON values are serializable")
}

fn stream_state(inner: &AdminRealtimeInner) -> std::sync::MutexGuard<'_, AdminStreamState> {
    inner
        .streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rooms(inner: &AdminRealtimeInner) -> std::sync::MutexGuard<'_, RoomStore> {
    inner
        .rooms
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sync_state(inner: &AdminRealtimeInner) -> std::sync::MutexGuard<'_, AdminSyncState> {
    inner
        .sync
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record_local_broadcast(inner: &AdminRealtimeInner, session_id: Uuid) {
    let mut broadcasts = inner
        .local_broadcasts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *broadcasts.entry(session_id).or_default() += 1;
}

fn consume_local_broadcast(inner: &AdminRealtimeInner, session_id: Uuid) -> bool {
    let mut broadcasts = inner
        .local_broadcasts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(count) = broadcasts.get_mut(&session_id) else {
        return false;
    };
    *count -= 1;
    if *count == 0 {
        broadcasts.remove(&session_id);
    }
    true
}

fn required_room_id(message: &Map<String, Value>) -> Result<&str, ApiError> {
    message
        .get("room-id")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("Invalid room-id"))
}

fn validated_start_sync_query(
    entry: &SessionEntry,
    message: &Map<String, Value>,
) -> Result<Map<String, Value>, ApiError> {
    if entry.context.actor != instant_http::AdminActor::Admin {
        return Err(ApiError::new(
            ApiErrorType::ValidationFailed,
            "start-sync is currently supported for admins only.",
        ));
    }
    let query = message
        .get("q")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| ApiError::new(ApiErrorType::ValidationFailed, "Query can not be null."))?;
    validate_sync_query(&query)?;
    Ok(query)
}

async fn deliver_broadcast_to_admin_sessions(
    inner: &AdminRealtimeInner,
    room: &RoomKey,
    payload: &BroadcastPayload,
) -> Result<(), ApiError> {
    let recipients = rooms(inner).local_session_ids(room);
    for recipient in recipients {
        if recipient == payload.data.peer_id {
            continue;
        }
        let sender = sessions(inner)
            .get(&recipient)
            .map(|entry| entry.sender.clone());
        if let Some(sender) = sender {
            send_sync_frame(
                &sender,
                json!({
                    "op": "server-broadcast",
                    "room-id": payload.room_id,
                    "topic": payload.topic,
                    "data": payload.data,
                }),
            )
            .await?;
        }
    }
    Ok(())
}

fn required_string<'a>(message: &'a Map<String, Value>, key: &str) -> Result<&'a str, ApiError> {
    message
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| protocol_error(&format!("Invalid {key}")))
}

fn required_uuid(message: &Map<String, Value>, key: &str) -> Result<Uuid, ApiError> {
    required_string(message, key).and_then(|value| {
        Uuid::parse_str(value).map_err(|_| protocol_error(&format!("Invalid {key}")))
    })
}

fn optional_uuid(message: &Map<String, Value>, key: &str) -> Result<Option<Uuid>, ApiError> {
    message
        .get(key)
        .map(|_| required_uuid(message, key))
        .transpose()
}

fn optional_non_empty_string(
    message: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ApiError> {
    message
        .get(key)
        .map(|_| required_string(message, key).map(str::to_owned))
        .transpose()
}

fn required_non_negative_u64(message: &Map<String, Value>, key: &str) -> Result<u64, ApiError> {
    message
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol_error(&format!("Invalid {key}")))
}

fn required_i64(message: &Map<String, Value>, key: &str) -> Result<i64, ApiError> {
    message
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| protocol_error(&format!("Invalid {key}")))
}

fn optional_non_negative_u64(
    message: &Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, ApiError> {
    message
        .get(key)
        .map(|_| required_non_negative_u64(message, key))
        .transpose()
}

fn optional_bool(message: &Map<String, Value>, key: &str) -> Result<Option<bool>, ApiError> {
    message
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| protocol_error(&format!("Invalid {key}")))
        })
        .transpose()
}

fn required_string_array(message: &Map<String, Value>, key: &str) -> Result<Vec<String>, ApiError> {
    message
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error(&format!("Invalid {key}")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| protocol_error(&format!("Invalid {key}")))
        })
        .collect()
}

fn json_string(value: &Value) -> Result<String, ApiError> {
    if let Some(value) = value.as_str() {
        if value.is_empty() {
            return Err(protocol_error("Invalid abort-reason"));
        }
        Ok(value.to_owned())
    } else {
        serde_json::to_string(value).map_err(|error| protocol_error(&error.to_string()))
    }
}

fn protocol_error(message: &str) -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, message)
}

fn room_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
}

fn cluster_error(error: impl std::fmt::Display) -> ApiError {
    database_error(error)
}

fn token_hash(token: Uuid) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn event_with_client_id(mut event: Value, client_event_id: Option<Value>) -> Value {
    if let (Some(object), Some(client_event_id)) = (event.as_object_mut(), client_event_id) {
        object.insert("client-event-id".to_owned(), client_event_id);
    }
    event
}

fn send(sender: &mpsc::Sender<Value>, event: Value) {
    let _ = sender.try_send(event);
}

async fn send_critical_stream_event(
    sender: &mpsc::Sender<Value>,
    event: Value,
) -> Result<(), ApiError> {
    match tokio::time::timeout(CRITICAL_STREAM_EVENT_SEND_TIMEOUT, sender.send(event)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(ApiError::new(
            ApiErrorType::ConnectionClosed,
            "Admin realtime connection closed before stream completion was delivered.",
        )),
        Err(_) => Err(ApiError::new(
            ApiErrorType::OperationTimedOut,
            "Timed out delivering stream completion to the admin realtime connection.",
        )),
    }
}

async fn send_sync_frame(sender: &mpsc::Sender<Value>, event: Value) -> Result<(), ApiError> {
    match tokio::time::timeout(SYNC_FRAME_SEND_TIMEOUT, sender.send(event)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(ApiError::new(
            ApiErrorType::ConnectionClosed,
            "Admin realtime connection closed during sync delivery.",
        )),
        Err(_) => Err(ApiError::new(
            ApiErrorType::OperationTimedOut,
            "Timed out delivering an admin sync frame.",
        )),
    }
}

fn stale_sync_error() -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        "Sync subscription missed realtime updates and must restart.",
    )
}

fn send_error(sender: &mpsc::Sender<Value>, client_event_id: Option<Value>, error: &ApiError) {
    let mut event = serde_json::to_value(error.body()).unwrap_or_else(
        |_| json!({"type": "unknown", "message": "Unable to serialize server error"}),
    );
    if let Some(object) = event.as_object_mut() {
        object.insert("op".to_owned(), json!("error"));
        object.insert("status".to_owned(), json!(error.status().as_u16()));
    }
    send(sender, event_with_client_id(event, client_event_id));
}

fn send_stream_error(
    sender: &mpsc::Sender<Value>,
    client_event_id: Option<Value>,
    original_event: &Map<String, Value>,
    error: &ApiError,
) {
    send(
        sender,
        stream_error_event(client_event_id, original_event, error),
    );
}

fn stream_error_event(
    client_event_id: Option<Value>,
    original_event: &Map<String, Value>,
    error: &ApiError,
) -> Value {
    let mut event = serde_json::to_value(error.body()).unwrap_or_else(
        |_| json!({"type": "unknown", "message": "Unable to serialize server error"}),
    );
    if let Some(object) = event.as_object_mut() {
        object.insert("op".to_owned(), json!("error"));
        object.insert("status".to_owned(), json!(error.status().as_u16()));
        if error.body().hint.is_none() && error.body().kind == ApiErrorType::ValidationFailed {
            object.insert(
                "hint".to_owned(),
                json!({"errors": [{"message": error.body().message}]}),
            );
        }
        object.insert(
            "original-event".to_owned(),
            Value::Object(original_event.clone()),
        );
    }
    event_with_client_id(event, client_event_id)
}

fn send_protocol_error(
    sender: &mpsc::Sender<Value>,
    client_event_id: Option<Value>,
    message: &str,
) {
    send(
        sender,
        event_with_client_id(
            json!({
                "op": "error",
                "status": 400,
                "type": "param-malformed",
                "message": message,
            }),
            client_event_id,
        ),
    );
}

fn send_protocol_event_error(
    sender: &mpsc::Sender<Value>,
    client_event_id: Option<Value>,
    original_event: &Map<String, Value>,
    message: &str,
) {
    send_stream_error(
        sender,
        client_event_id,
        original_event,
        &ApiError::new(ApiErrorType::ParamMalformed, message),
    );
}

fn session_missing(session_id: Uuid) -> ApiError {
    ApiError::new(
        ApiErrorType::SessionMissing,
        format!("Session {session_id} is not available"),
    )
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::body::{Body, Bytes};
    use axum::http::{Request, StatusCode};
    use futures_util::{Stream, StreamExt};
    use instant_http::{
        AdminActor, AdminQueryPermissionsRequest, AdminTransactPermissionsRequest,
        admin_realtime_router,
    };
    use instant_instaql::{AttributeCatalog, system_attribute_catalog};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn cluster_coordination_errors_hide_the_store_cause() {
        let error = cluster_error("connection to server at 10.0.0.5 failed");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Database error");
    }

    #[derive(Default)]
    struct TestAuthorizer {
        schema_changed: AtomicBool,
    }

    impl TestAuthorizer {
        fn catalog(&self) -> AttributeCatalog {
            let mut catalog = system_attribute_catalog().unwrap();
            if self.schema_changed.load(Ordering::Relaxed) {
                let mut extra = catalog.attributes[0].clone();
                extra.id = Uuid::from_u128(0xfeed);
                extra.etype = "goals".to_owned();
                extra.label = "extra".to_owned();
                extra.reverse_etype = None;
                extra.reverse_label = None;
                catalog.attributes.push(extra);
            }
            catalog
        }
    }

    async fn next_sse_values(
        stream: &mut (impl Stream<Item = Result<Bytes, axum::Error>> + Unpin),
    ) -> Vec<Value> {
        loop {
            let bytes = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("SSE event timed out")
                .expect("SSE stream closed")
                .expect("SSE body failed");
            let values = String::from_utf8(bytes.to_vec())
                .expect("SSE data is UTF-8")
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|data| serde_json::from_str(data).expect("SSE data is JSON"))
                .collect::<Vec<_>>();
            if !values.is_empty() {
                return values;
            }
        }
    }

    async fn push_over_http(
        router: axum::Router,
        app_id: Uuid,
        machine_id: Uuid,
        session_id: Uuid,
        sse_token: Uuid,
        messages: Vec<Value>,
    ) -> StatusCode {
        router
            .oneshot(
                Request::post(format!("/admin/sse/push?app_id={app_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "machine_id": machine_id,
                            "session_id": session_id,
                            "sse_token": sse_token,
                            "messages": messages,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    impl AdminDataAuthorizer for TestAuthorizer {
        fn authorize(
            &self,
            _context: AdminRequestContext,
            _scope: AdminDataScope,
        ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(self.catalog()))
        }

        fn load_catalog(
            &self,
            _app_id: Uuid,
        ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
            std::future::ready(Ok(self.catalog()))
        }
    }

    #[derive(Default)]
    struct TestData {
        query_count: AtomicUsize,
        query_version: AtomicUsize,
        transact_count: AtomicUsize,
    }

    impl AdminDataBackend for TestData {
        type PreparedQueryPermissions = AdminRequestContext;

        fn query(
            &self,
            _context: AdminRequestContext,
            request: AdminQueryRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.query_count.fetch_add(1, Ordering::Relaxed);
            let version = self.query_version.load(Ordering::Relaxed);
            std::future::ready(Ok(json!({"echo": request.query, "version": version})))
        }

        fn transact(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactRequest,
        ) -> impl Future<Output = Result<instant_http::AdminTransactResponse, ApiError>> + Send
        {
            self.transact_count.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(instant_http::AdminTransactResponse { tx_id: 9 }))
        }

        fn prepare_query_permissions(
            &self,
            context: AdminRequestContext,
            _rules_override: Option<Value>,
        ) -> impl Future<Output = Result<Self::PreparedQueryPermissions, ApiError>> + Send {
            std::future::ready(Ok(context))
        }

        fn query_permissions(
            &self,
            _prepared: Self::PreparedQueryPermissions,
            _request: AdminQueryPermissionsRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Err(ApiError::new(ApiErrorType::Unknown, "unused")))
        }

        fn transact_permissions(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactPermissionsRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Err(ApiError::new(ApiErrorType::Unknown, "unused")))
        }
    }

    impl RuntimeRealtimeTransactionBackend for TestData {
        fn transact_realtime(
            &self,
            _context: AdminRequestContext,
            _request: AdminTransactRequest,
        ) -> impl Future<Output = Result<crate::RuntimeRealtimeTransaction, ApiError>> + Send
        {
            self.transact_count.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(crate::RuntimeRealtimeTransaction {
                tx_id: 42,
                fallback_isn: Isn::new(6, instant_wal::Lsn::from_u64(0x41)),
            }))
        }
    }

    impl RuntimeRealtimeIdentityBackend for TestData {
        fn resolve_realtime_user_id(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<Option<Uuid>, ApiError>> + Send {
            std::future::ready(Ok(None))
        }
    }

    impl RuntimeSyncBackend for TestData {
        fn sync_snapshot(
            &self,
            _request: RuntimeSyncSnapshotRequest,
        ) -> impl Future<Output = Result<crate::RuntimeSyncSnapshot, ApiError>> + Send {
            std::future::ready(Ok(crate::RuntimeSyncSnapshot {
                join_rows: vec![json!([["entity", "attribute", "value", 1]])],
                tx_id: 7,
                attribute_ids: vec![Uuid::nil()],
            }))
        }

        fn create_sync_subscription(
            &self,
            _request: RuntimeSyncSubscriptionCreate,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Ok(()))
        }

        fn delete_sync_subscription(
            &self,
            _context: AdminRequestContext,
            _subscription_id: Uuid,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Ok(()))
        }

        fn resume_sync_subscription(
            &self,
            request: crate::RuntimeSyncSubscriptionResume,
        ) -> impl Future<Output = Result<crate::RuntimeSyncSubscription, ApiError>> + Send {
            std::future::ready(Ok(crate::RuntimeSyncSubscription {
                app_id: request.context.app_id,
                query: serde_json::Map::new(),
                attribute_ids: vec![Uuid::nil()],
            }))
        }
    }

    impl RuntimeTreeQueryBackend for TestData {
        fn query_tree(
            &self,
            context: AdminRequestContext,
            request: AdminQueryRequest,
        ) -> impl Future<Output = Result<RuntimeTreeQueryResponse, ApiError>> + Send {
            let result = AdminDataBackend::query(self, context, request);
            async move {
                Ok(RuntimeTreeQueryResponse {
                    result: result.await?,
                    result_meta: json!({
                        "page-info": {
                            "goals": {
                                "has-next-page?": true,
                                "has-previous-page?": false,
                            }
                        },
                        "aggregate": {"goals": {"count": 2}},
                    }),
                    instant_topics: None,
                })
            }
        }
    }

    fn context(app_id: Uuid) -> AdminRequestContext {
        AdminRequestContext {
            app_id,
            bearer_token: Some("secret".to_owned()),
            actor: AdminActor::Admin,
            client_ip: None,
        }
    }

    async fn push_message(
        service: &AdminRealtimeService<TestAuthorizer, TestData>,
        app_id: Uuid,
        machine_id: Uuid,
        session_id: Uuid,
        sse_token: Uuid,
        message: Value,
    ) {
        service
            .push(AdminSsePushRequest {
                app_id,
                machine_id,
                session_id,
                sse_token,
                messages: vec![message],
            })
            .await
            .unwrap();
    }

    fn wal_record(app_id: Uuid) -> instant_wal::WalRecord {
        let lsn = instant_wal::Lsn::from_u64(0x42);
        instant_wal::WalRecord {
            app_id,
            tx_id: 42,
            isn: Isn::new(7, lsn),
            previous_isn: None,
            tx_created_at: None,
            tx_bytes: 0,
            next_lsn: lsn,
            attr_changes: Vec::new(),
            ident_changes: Vec::new(),
            triple_changes: Vec::new(),
            messages: Vec::new(),
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }

    async fn assert_invalid_token_is_rejected(
        service: &AdminRealtimeService<TestAuthorizer, TestData>,
        app_id: Uuid,
        machine_id: Uuid,
        session_id: Uuid,
    ) {
        let error = service
            .push(AdminSsePushRequest {
                app_id,
                machine_id,
                session_id,
                sse_token: Uuid::new_v4(),
                messages: Vec::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::SessionMissing);
    }

    fn assert_tree_metadata(result_meta: &Value) {
        assert_eq!(result_meta["page-info"]["goals"]["has-next-page?"], true);
        assert_eq!(
            result_meta["page-info"]["goals"]["has-previous-page?"],
            false
        );
        assert_eq!(result_meta["aggregate"]["goals"]["count"], 2);
    }

    async fn deliver_wal(
        service: &AdminRealtimeService<TestAuthorizer, TestData>,
        record: instant_wal::WalRecord,
    ) {
        RealtimeInvalidationConsumer::deliver(
            service,
            &ReplicationPipelineOutcome {
                record: Some(record),
                ..ReplicationPipelineOutcome::default()
            },
        )
        .await
        .unwrap();
    }

    async fn assert_unchanged_then_schema_refresh(
        service: &AdminRealtimeService<TestAuthorizer, TestData>,
        session: &mut AdminSseSession,
        authorizer: &TestAuthorizer,
        record: &instant_wal::WalRecord,
    ) -> instant_wal::WalRecord {
        let mut unchanged = record.clone();
        unchanged.tx_id = 43;
        unchanged.isn = Isn::new(8, instant_wal::Lsn::from_u64(0x43));
        deliver_wal(service, unchanged.clone()).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), session.recv())
                .await
                .is_err()
        );

        authorizer.schema_changed.store(true, Ordering::Relaxed);
        let mut schema_change = unchanged;
        schema_change.tx_id = 44;
        schema_change.isn = Isn::new(9, instant_wal::Lsn::from_u64(0x44));
        deliver_wal(service, schema_change.clone()).await;
        let schema_refresh = session.recv().await.unwrap();
        assert_eq!(schema_refresh["op"], "refresh-ok");
        assert_eq!(schema_refresh["computations"].as_array().unwrap().len(), 2);
        for computation in schema_refresh["computations"].as_array().unwrap() {
            assert_tree_metadata(&computation["result-meta"]);
        }
        assert_eq!(
            schema_refresh["attrs"].as_array().unwrap().len(),
            system_attribute_catalog().unwrap().attributes.len() + 1
        );
        schema_change
    }

    async fn assert_transaction_uses_delivered_isn(
        service: &AdminRealtimeService<TestAuthorizer, TestData>,
        session: &mut AdminSseSession,
        connection: (Uuid, Uuid, Uuid, Uuid),
        record: &instant_wal::WalRecord,
    ) {
        let (app_id, machine_id, session_id, token) = connection;
        push_message(
            service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({"op": "transact", "client-event-id": "event-2", "tx-steps": []}),
        )
        .await;
        let transacted = session.recv().await.unwrap();
        assert_eq!(transacted["op"], "transact-ok");
        assert_eq!(transacted["tx-id"], record.tx_id);
        assert_eq!(transacted["isn"], json!(record.isn));
    }

    #[tokio::test]
    async fn sessions_authenticate_pushes_process_messages_and_refresh_queries() {
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let authorizer = Arc::new(TestAuthorizer::default());
        let service =
            AdminRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&data), machine_id);
        let mut session = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: Some(serde_json::from_value(json!({"goals": {}})).unwrap()),
                    inference: true,
                    versions: Some(json!({"sdk": "1"})),
                },
            )
            .await
            .unwrap();
        let init = session.recv().await.unwrap();
        let session_id = Uuid::parse_str(init["session-id"].as_str().unwrap()).unwrap();
        let token = Uuid::parse_str(init["sse-token"].as_str().unwrap()).unwrap();
        assert_eq!(init["machine-id"], json!(machine_id));
        let initial_query = session.recv().await.unwrap();
        assert_eq!(initial_query["op"], "add-query-ok");
        assert_eq!(initial_query["processed-tx-id"], Value::Null);
        assert_tree_metadata(&initial_query["result-meta"]);
        assert_eq!(service.session_count(), 1);

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "add-query",
                "client-event-id": "event-1",
                "q": {"todos": {}},
                "return-type": "tree"
            }),
        )
        .await;
        let added = session.recv().await.unwrap();
        assert_eq!(added["op"], "add-query-ok");
        assert_eq!(added["client-event-id"], "event-1");
        assert_tree_metadata(&added["result-meta"]);

        data.query_version.store(1, Ordering::Relaxed);
        let record = wal_record(app_id);
        deliver_wal(&service, record.clone()).await;
        let refreshed = session.recv().await.unwrap();
        assert_eq!(refreshed["op"], "refresh-ok");
        assert_eq!(refreshed["computations"].as_array().unwrap().len(), 2);
        for computation in refreshed["computations"].as_array().unwrap() {
            assert_tree_metadata(&computation["result-meta"]);
        }
        assert_eq!(refreshed["processed-tx-id"], record.tx_id);
        assert_eq!(refreshed["processed-isn"], json!(record.isn));
        assert!(
            refreshed["attrs"]
                .as_array()
                .is_some_and(|attrs| !attrs.is_empty())
        );

        let schema_change = assert_unchanged_then_schema_refresh(
            &service,
            &mut session,
            authorizer.as_ref(),
            &record,
        )
        .await;

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "add-query",
                "client-event-id": "event-after-wal",
                "q": {"afterWal": {}}
            }),
        )
        .await;
        let after_wal = session.recv().await.unwrap();
        assert_eq!(after_wal["processed-tx-id"], schema_change.tx_id);
        assert_eq!(after_wal["processed-isn"], json!(schema_change.isn));

        assert_transaction_uses_delivered_isn(
            &service,
            &mut session,
            (app_id, machine_id, session_id, token),
            &record,
        )
        .await;

        assert_invalid_token_is_rejected(&service, app_id, machine_id, session_id).await;
        assert_eq!(data.query_count.load(Ordering::Relaxed), 9);
        assert_eq!(data.transact_count.load(Ordering::Relaxed), 1);

        drop(session);
        assert_eq!(service.session_count(), 0);
    }

    #[tokio::test]
    async fn admin_sse_rooms_presence_and_broadcast_match_the_shared_operation_table() {
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            machine_id,
        );
        let mut first = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let first_init = first.recv().await.unwrap();
        let first_id = Uuid::parse_str(first_init["session-id"].as_str().unwrap()).unwrap();
        let first_token = Uuid::parse_str(first_init["sse-token"].as_str().unwrap()).unwrap();
        let mut second = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let second_init = second.recv().await.unwrap();
        let second_id = Uuid::parse_str(second_init["session-id"].as_str().unwrap()).unwrap();
        let second_token = Uuid::parse_str(second_init["sse-token"].as_str().unwrap()).unwrap();

        push_message(
            &service,
            app_id,
            machine_id,
            first_id,
            first_token,
            json!({"op": "join-room", "room-id": "", "data": {"cursor": 1}, "client-event-id": "join-1"}),
        )
        .await;
        assert_eq!(first.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(first.recv().await.unwrap()["op"], "join-room-ok");

        push_message(
            &service,
            app_id,
            machine_id,
            second_id,
            second_token,
            json!({"op": "join-room", "room-id": "", "data": {"cursor": 2}, "client-event-id": "join-2"}),
        )
        .await;
        let first_presence = first.recv().await.unwrap();
        assert_eq!(first_presence["op"], "refresh-presence");
        assert_eq!(first_presence["data"].as_object().unwrap().len(), 2);
        assert_eq!(second.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(second.recv().await.unwrap()["op"], "join-room-ok");

        push_message(
            &service,
            app_id,
            machine_id,
            first_id,
            first_token,
            json!({"op": "client-broadcast", "room-id": "", "topic": "cursor", "data": {"x": 10}, "client-event-id": "broadcast-1"}),
        )
        .await;
        let received = second.recv().await.unwrap();
        assert_eq!(received["op"], "server-broadcast");
        assert_eq!(received["data"]["peer-id"], json!(first_id));
        let acknowledged = first.recv().await.unwrap();
        assert_eq!(acknowledged["op"], "client-broadcast-ok");
        assert_eq!(acknowledged["client-event-id"], "broadcast-1");

        push_message(
            &service,
            app_id,
            machine_id,
            first_id,
            first_token,
            json!({"op": "leave-room", "room-id": "", "client-event-id": "leave-1"}),
        )
        .await;
        assert_eq!(second.recv().await.unwrap()["op"], "refresh-presence");
        assert_eq!(first.recv().await.unwrap()["op"], "leave-room-ok");

        push_message(
            &service,
            app_id,
            machine_id,
            first_id,
            first_token,
            json!({"op": "client-broadcast", "room-id": "", "topic": "cursor", "data": null, "client-event-id": "not-in-room"}),
        )
        .await;
        let error = first.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["type"], "validation-failed");
        assert_eq!(error["original-event"]["client-event-id"], "not-in-room");
    }

    #[tokio::test]
    async fn admin_sse_sync_restarts_after_updates_and_resume_attempts() {
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            machine_id,
        );
        let mut session = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let init = session.recv().await.unwrap();
        let session_id = Uuid::parse_str(init["session-id"].as_str().unwrap()).unwrap();
        let sse_token = Uuid::parse_str(init["sse-token"].as_str().unwrap()).unwrap();
        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            sse_token,
            json!({"op": "start-sync", "q": {"goals": {}}, "client-event-id": "sync-1"}),
        )
        .await;
        let started = session.recv().await.unwrap();
        assert_eq!(started["op"], "start-sync-ok");
        assert_eq!(started["client-event-id"], "sync-1");
        let subscription_id =
            Uuid::parse_str(started["subscription-id"].as_str().unwrap()).unwrap();
        let token = Uuid::parse_str(started["token"].as_str().unwrap()).unwrap();
        let loaded = session.recv().await.unwrap();
        assert_eq!(loaded["op"], "sync-load-batch");
        assert_eq!(loaded["join-rows"].as_array().unwrap().len(), 1);
        let finished = session.recv().await.unwrap();
        assert_eq!(finished["op"], "sync-init-finish");
        assert_eq!(finished["tx-id"], 7);

        service
            .publish_sync_transaction(
                app_id,
                8,
                vec![json!({
                    "action": "added",
                    "triple": [Uuid::nil(), Uuid::nil(), "updated", 2]
                })],
            )
            .await;
        let update = session.recv().await.unwrap();
        assert_eq!(update["op"], "error");
        assert_eq!(update["original-event"]["op"], "resync-table");

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            sse_token,
            json!({"op": "remove-sync", "subscription-id": subscription_id, "keep-subscription": false}),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), session.recv())
                .await
                .is_err(),
            "remove-sync has no acknowledgement"
        );
        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            sse_token,
            json!({"op": "resync-table", "subscription-id": subscription_id, "tx-id": 8, "token": token}),
        )
        .await;
        let resumed = session.recv().await.unwrap();
        assert_eq!(resumed["op"], "error");
        assert_eq!(resumed["original-event"]["op"], "resync-table");
    }

    #[tokio::test]
    async fn remote_cluster_wal_and_byop_refresh_admin_sessions() {
        let app_id = Uuid::new_v4();
        let local_machine = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::clone(&data),
            local_machine,
        );
        let mut session = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: Some(serde_json::from_value(json!({"goals": {}})).unwrap()),
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "sse-init");
        assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");

        data.query_version.store(1, Ordering::Relaxed);
        let record = wal_record(app_id);
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: Uuid::new_v4(),
                record: record.clone(),
            },
        )
        .await;
        let wal_refresh = session.recv().await.unwrap();
        assert_eq!(wal_refresh["op"], "refresh-ok");
        assert_eq!(wal_refresh["processed-tx-id"], record.tx_id);
        assert_eq!(wal_refresh["processed-isn"], json!(record.isn));

        data.query_version.store(2, Ordering::Relaxed);
        let byop_record = ByopWalRecord {
            tx_id: 43,
            next_lsn: instant_wal::Lsn::from_u64(0x43),
            changes: Vec::new(),
        };
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::ByopWal {
                source_machine_id: Uuid::new_v4(),
                app_id,
                record: byop_record,
                topics: Vec::new(),
            },
        )
        .await;
        let byop_refresh = session.recv().await.unwrap();
        assert_eq!(byop_refresh["op"], "refresh-ok");
        assert_eq!(byop_refresh["processed-tx-id"], 43);
        assert_eq!(byop_refresh["processed-isn"], json!(record.isn));

        data.query_version.store(3, Ordering::Relaxed);
        deliver_cluster_event(
            &service,
            RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: local_machine,
                record: record.clone(),
            },
        )
        .await;
        let local_refresh = session.recv().await.unwrap();
        assert_eq!(local_refresh["op"], "refresh-ok");
        assert_eq!(local_refresh["processed-tx-id"], 43);
        assert_eq!(local_refresh["processed-isn"], json!(record.isn));

        data.query_version.store(4, Ordering::Relaxed);
        service.recover_cluster_lag().await;
        let recovered = session.recv().await.unwrap();
        assert_eq!(recovered["op"], "refresh-ok");
        assert_eq!(recovered["processed-tx-id"], Value::Null);
        assert_eq!(recovered["processed-isn"], Value::Null);
        assert_eq!(recovered["computations"].as_array().unwrap().len(), 1);
        assert!(recovered["attrs"].is_array());
    }

    #[tokio::test]
    async fn admin_cluster_consumer_retries_failed_subscription_then_delivers_and_acks() {
        let app_id = Uuid::new_v4();
        let local_machine = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let cluster = Arc::new(
            crate::runtime_realtime::cluster_test_support::FailFirstSubscribeCluster::new(),
        );
        let cluster_boundary = Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>;
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::clone(&data),
            local_machine,
        )
        .with_cluster(&cluster_boundary);
        let mut session = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: Some(serde_json::from_value(json!({"goals": {}})).unwrap()),
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap()["op"], "sse-init");
        assert_eq!(session.recv().await.unwrap()["op"], "add-query-ok");

        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin cluster consumer did not retry subscription");

        data.query_version.store(1, Ordering::Relaxed);
        let event_id = 81;
        cluster.send(RuntimeRealtimeClusterDelivery {
            event_id,
            event: RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: Uuid::new_v4(),
                record: wal_record(app_id),
            },
        });
        let refresh = tokio::time::timeout(Duration::from_secs(2), session.recv())
            .await
            .expect("admin cluster delivery timed out")
            .expect("admin SSE session closed");
        assert_eq!(refresh["op"], "refresh-ok");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cluster.acknowledged(RealtimeClusterConsumerKind::Admin, event_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin cluster delivery was not acknowledged");
    }

    #[tokio::test]
    async fn admin_cluster_consumer_stops_while_idle_after_service_drop() {
        let cluster = Arc::new(
            crate::runtime_realtime::cluster_test_support::FailFirstSubscribeCluster::new(),
        );
        let cluster_boundary = Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>;
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(&cluster_boundary);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin cluster consumer did not become idle");

        drop(service);
        drop(cluster_boundary);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&cluster) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idle admin cluster consumer retained the cluster");
    }

    #[tokio::test]
    async fn admin_cluster_consumer_stops_during_persistent_ack_failure() {
        let app_id = Uuid::new_v4();
        let cluster = Arc::new(
            crate::runtime_realtime::cluster_test_support::FailFirstSubscribeCluster::new(),
        );
        let cluster_boundary = Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>;
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(&cluster_boundary);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin cluster consumer did not subscribe");
        cluster.fail_acknowledgements();
        let event_id = 82;
        cluster.send(RuntimeRealtimeClusterDelivery {
            event_id,
            event: RuntimeRealtimeClusterEvent::Wal {
                source_machine_id: Uuid::new_v4(),
                record: wal_record(app_id),
            },
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cluster.acknowledged(RealtimeClusterConsumerKind::Admin, event_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin acknowledgement was not attempted");

        drop(service);
        drop(cluster_boundary);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&cluster) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin acknowledgement retry retained the cluster");
    }

    #[tokio::test]
    async fn admin_cluster_consumer_backs_off_repeated_closed_receivers() {
        let cluster = Arc::new(
            crate::runtime_realtime::cluster_test_support::FailFirstSubscribeCluster::closed(),
        );
        let cluster_boundary = Arc::clone(&cluster) as Arc<dyn RuntimeRealtimeCluster>;
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            Uuid::new_v4(),
        )
        .with_cluster(&cluster_boundary);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cluster.attempts() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin cluster consumer did not retry closed receivers");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(cluster.attempts(), 3);
        drop(service);
        drop(cluster_boundary);
    }

    #[tokio::test]
    async fn transaction_uses_fallback_isn_when_invalidation_does_not_arrive() {
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let data = Arc::new(TestData::default());
        let service =
            AdminRealtimeService::new(Arc::new(TestAuthorizer::default()), data, machine_id)
                .with_transaction_isn_wait(Duration::ZERO);
        let mut session = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let init = session.recv().await.unwrap();
        let session_id = Uuid::parse_str(init["session-id"].as_str().unwrap()).unwrap();
        let token = Uuid::parse_str(init["sse-token"].as_str().unwrap()).unwrap();
        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({"op": "transact", "tx-steps": []}),
        )
        .await;
        let response = session.recv().await.unwrap();
        assert_eq!(response["tx-id"], 42);
        assert_eq!(
            response["isn"],
            json!(Isn::new(6, instant_wal::Lsn::from_u64(0x41)))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One HTTP flow deliberately fills and drains the SSE queue.
    async fn http_sse_terminal_stream_events_survive_channel_backpressure() {
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let service = Arc::new(AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            machine_id,
        ));
        let router = admin_realtime_router(Arc::clone(&service));
        let response = router
            .clone()
            .oneshot(
                Request::post(format!("/admin/sse?app_id={app_id}"))
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut events = response.into_body().into_data_stream();
        let init = next_sse_values(&mut events).await.remove(0);
        assert_eq!(init["op"], "sse-init");
        let session_id = Uuid::parse_str(init["session-id"].as_str().unwrap()).unwrap();
        let sse_token = Uuid::parse_str(init["sse-token"].as_str().unwrap()).unwrap();

        assert_eq!(
            push_over_http(
                router.clone(),
                app_id,
                machine_id,
                session_id,
                sse_token,
                vec![json!({
                    "op": "start-stream",
                    "client-event-id": "start",
                    "client-id": "backpressure-stream",
                    "reconnect-token": Uuid::new_v4(),
                })],
            )
            .await,
            StatusCode::OK
        );
        let started = next_sse_values(&mut events).await.remove(0);
        assert_eq!(started["op"], "start-stream-ok");
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();

        assert_eq!(
            push_over_http(
                router.clone(),
                app_id,
                machine_id,
                session_id,
                sse_token,
                vec![json!({
                    "op": "subscribe-stream",
                    "client-event-id": "reader",
                    "stream-id": stream_id,
                })],
            )
            .await,
            StatusCode::OK
        );
        let subscribed = next_sse_values(&mut events).await.remove(0);
        assert_eq!(subscribed["op"], "stream-append");
        assert_eq!(subscribed["done"], false);

        let terminal_events = tokio::spawn(async move {
            let mut reader_done = false;
            let mut writer_done = false;
            while !reader_done || !writer_done {
                for event in next_sse_values(&mut events).await {
                    reader_done |= event["op"] == "stream-append"
                        && event["client-event-id"] == "reader"
                        && event["done"] == true;
                    writer_done |= event["op"] == "stream-flushed"
                        && event["stream-id"] == json!(stream_id)
                        && event["done"] == true;
                }
            }
            (reader_done, writer_done)
        });

        let mut messages = (0..64)
            .map(|index| {
                json!({
                    "op": "invalid",
                    "client-event-id": format!("fill-{index}"),
                })
            })
            .collect::<Vec<_>>();
        messages.push(json!({
            "op": "append-stream",
            "client-event-id": "complete",
            "stream-id": stream_id,
            "offset": 0,
            "chunks": [],
            "done": true,
        }));
        assert_eq!(
            push_over_http(router, app_id, machine_id, session_id, sse_token, messages,).await,
            StatusCode::OK
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), terminal_events)
                .await
                .expect("terminal SSE events timed out")
                .expect("terminal SSE consumer failed"),
            (true, true)
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn admin_sse_streams_acknowledge_fan_out_complete_and_replay() {
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            machine_id,
        );
        let mut session = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let init = session.recv().await.unwrap();
        let session_id = Uuid::parse_str(init["session-id"].as_str().unwrap()).unwrap();
        let token = Uuid::parse_str(init["sse-token"].as_str().unwrap()).unwrap();
        let reconnect_token = Uuid::new_v4();

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "start-stream",
                "client-event-id": "start",
                "client-id": "official-resumable-stream",
                "reconnect-token": reconnect_token,
            }),
        )
        .await;
        let started = session.recv().await.unwrap();
        assert_eq!(started["op"], "start-stream-ok");
        assert_eq!(started["client-event-id"], "start");
        assert_eq!(started["offset"], 0);
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "subscribe-stream",
                "client-event-id": "reader",
                "client-id": "official-resumable-stream",
            }),
        )
        .await;
        let initial = session.recv().await.unwrap();
        assert_eq!(initial["op"], "stream-append");
        assert_eq!(initial["client-event-id"], "reader");
        assert_eq!(initial["stream-id"], json!(stream_id));
        assert_eq!(initial["content"], "");
        assert_eq!(initial["done"], false);

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "append-stream",
                "client-event-id": "append",
                "stream-id": stream_id,
                "offset": 0,
                "chunks": ["hello"],
                "done": false,
            }),
        )
        .await;
        let appended = session.recv().await.unwrap();
        assert_eq!(appended["op"], "stream-append");
        assert_eq!(appended["client-event-id"], "reader");
        assert_eq!(appended["offset"], 0);
        assert_eq!(appended["content"], "hello");

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "append-stream",
                "client-event-id": "complete",
                "stream-id": stream_id,
                "offset": 5,
                "chunks": [],
                "done": true,
            }),
        )
        .await;
        let completed_reader = session.recv().await.unwrap();
        assert_eq!(completed_reader["op"], "stream-append");
        assert_eq!(completed_reader["client-event-id"], "reader");
        assert_eq!(completed_reader["offset"], 5);
        assert_eq!(completed_reader["done"], true);
        let flushed = session.recv().await.unwrap();
        assert_eq!(flushed["op"], "stream-flushed");
        assert_eq!(flushed["offset"], 5);
        assert_eq!(flushed["done"], true);

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "subscribe-stream",
                "client-event-id": "replay",
                "client-id": "official-resumable-stream",
            }),
        )
        .await;
        let replay = session.recv().await.unwrap();
        assert_eq!(replay["client-event-id"], "replay");
        assert_eq!(replay["content"], "hello");
        assert_eq!(replay["done"], true);

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "start-stream",
                "client-event-id": "duplicate",
                "client-id": "official-resumable-stream",
                "reconnect-token": Uuid::new_v4(),
            }),
        )
        .await;
        let duplicate = session.recv().await.unwrap();
        assert_eq!(duplicate["op"], "error");
        assert_eq!(duplicate["client-event-id"], "duplicate");
        assert_eq!(duplicate["original-event"]["op"], "start-stream");

        push_message(
            &service,
            app_id,
            machine_id,
            session_id,
            token,
            json!({
                "op": "subscribe-stream",
                "client-event-id": "missing",
                "client-id": "does-not-exist",
            }),
        )
        .await;
        let missing = session.recv().await.unwrap();
        assert_eq!(missing["original-event"]["op"], "subscribe-stream");
        assert_eq!(
            missing["hint"]["errors"][0]["message"],
            "Stream is missing."
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One flow proves writer fencing and reader retention.
    async fn stream_machine_eviction_fences_only_a_displaced_admin_writer() {
        let app_id = Uuid::new_v4();
        let local_machine = Uuid::new_v4();
        let service = AdminRealtimeService::new(
            Arc::new(TestAuthorizer::default()),
            Arc::new(TestData::default()),
            local_machine,
        );
        let mut writer = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let writer_init = writer.recv().await.unwrap();
        let writer_id = Uuid::parse_str(writer_init["session-id"].as_str().unwrap()).unwrap();
        let writer_token = Uuid::parse_str(writer_init["sse-token"].as_str().unwrap()).unwrap();
        let mut reader = service
            .open(
                context(app_id),
                AdminSseOpenRequest {
                    query: None,
                    inference: false,
                    versions: None,
                },
            )
            .await
            .unwrap();
        let reader_init = reader.recv().await.unwrap();
        let reader_id = Uuid::parse_str(reader_init["session-id"].as_str().unwrap()).unwrap();
        let reader_token = Uuid::parse_str(reader_init["sse-token"].as_str().unwrap()).unwrap();

        push_message(
            &service,
            app_id,
            local_machine,
            writer_id,
            writer_token,
            json!({
                "op": "start-stream",
                "client-event-id": "start",
                "client-id": "admin-ownership-fence",
                "reconnect-token": Uuid::new_v4(),
            }),
        )
        .await;
        let started = writer.recv().await.unwrap();
        let stream_id = Uuid::parse_str(started["stream-id"].as_str().unwrap()).unwrap();
        push_message(
            &service,
            app_id,
            local_machine,
            reader_id,
            reader_token,
            json!({
                "op": "subscribe-stream",
                "client-event-id": "reader",
                "stream-id": stream_id,
            }),
        )
        .await;
        reader.recv().await.unwrap();

        service.apply_cache_eviction(&WalCacheEviction::StreamMachine {
            stream_id,
            machine_id: local_machine,
        });
        assert_eq!(
            stream_state(&service.inner).writers.get(&stream_id),
            Some(&writer_id)
        );

        service.apply_cache_eviction(&WalCacheEviction::StreamMachine {
            stream_id,
            machine_id: Uuid::new_v4(),
        });
        {
            let state = stream_state(&service.inner);
            assert!(!state.writers.contains_key(&stream_id));
            assert!(
                state
                    .readers
                    .values()
                    .any(|entry| entry.stream == stream_id)
            );
        }

        push_message(
            &service,
            app_id,
            local_machine,
            writer_id,
            writer_token,
            json!({
                "op": "append-stream",
                "client-event-id": "stale-append",
                "stream-id": stream_id,
                "offset": 0,
                "chunks": ["stale"],
            }),
        )
        .await;
        let error = writer.recv().await.unwrap();
        assert_eq!(error["op"], "error");
        assert_eq!(error["message"], "Stream updated from a different machine.");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reader.recv())
                .await
                .is_err()
        );
        assert!(
            stream_state(&service.inner)
                .readers
                .values()
                .any(|entry| entry.stream == stream_id)
        );
    }
}
