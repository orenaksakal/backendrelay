use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use instant_realtime::{DispatchOutcome, ReplicationPipelineOutcome};
use instant_wal::{WalCacheEviction, combine_wal_records};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, RealtimeInvalidationConsumer};

const DEFAULT_BATCH_LIMIT: usize = 500;
const DEFAULT_DROP_LATENCY_MS: u64 = 30_000;
const DEFAULT_MAX_WORKERS: usize = 8;
// Bound each app lane independently. Producers wait for capacity; dropping
// remains controlled exclusively by the existing opt-in latency policy.
const MAX_PENDING_BATCHES_PER_APP: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidationQueueConfig {
    pub batch_limit: usize,
    pub drop_after: Option<Duration>,
}

impl Default for InvalidationQueueConfig {
    fn default() -> Self {
        Self {
            batch_limit: DEFAULT_BATCH_LIMIT,
            drop_after: None,
        }
    }
}

pub type InvalidationQueuePolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<InvalidationQueueConfig, String>> + Send + 'a>>;

pub trait InvalidationQueuePolicy: Send + Sync + 'static {
    fn config(&self, app_id: Uuid) -> InvalidationQueuePolicyFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultInvalidationQueuePolicy;

impl InvalidationQueuePolicy for DefaultInvalidationQueuePolicy {
    fn config(&self, _app_id: Uuid) -> InvalidationQueuePolicyFuture<'_> {
        Box::pin(std::future::ready(Ok(InvalidationQueueConfig::default())))
    }
}

/// Live config-app reader for invalidator batching and emergency dropping.
pub struct PostgresInvalidationQueuePolicy {
    client: PostgresClientSource,
    controls_sql: String,
}

impl PostgresInvalidationQueuePolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            controls_sql: queue_controls_sql(INSTANT_CONFIG_APP_ID),
        }
    }

    /// Uses reconnecting pooled leases for the short config lookup performed
    /// before each invalidation enqueue.
    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            controls_sql: queue_controls_sql(INSTANT_CONFIG_APP_ID),
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.controls_sql = queue_controls_sql(config_app_id);
        self
    }
}

// Only typed configuration identity and fixed setting names enter the SQL.
// Values remain live while PostgreSQL reuses the partition-pruned plan.
fn queue_controls_sql(config_app_id: Uuid) -> String {
    format!("SELECT flag_setting.value #>> '{{}}', flag_value.value
                     FROM attrs setting_attr
                     JOIN triples flag_setting
                       ON flag_setting.app_id = '{config_app_id}'::uuid AND flag_setting.attr_id = setting_attr.id
                     JOIN attrs value_attr
                       ON value_attr.app_id = '{config_app_id}'::uuid AND value_attr.etype = 'flags'
                      AND value_attr.label = 'value'
                     JOIN triples flag_value
                       ON flag_value.app_id = '{config_app_id}'::uuid
                      AND flag_value.entity_id = flag_setting.entity_id
                      AND flag_value.attr_id = value_attr.id
                     WHERE setting_attr.app_id = '{config_app_id}'::uuid AND setting_attr.etype = 'flags'
                       AND setting_attr.label = 'setting'
                       AND flag_setting.value #>> '{{}}' = ANY(ARRAY['invalidator-batch-limit', 'invalidator-drop-backpressure-apps', 'invalidator-drop-tx-latency-ms']::text[])")
}

impl InvalidationQueuePolicy for PostgresInvalidationQueuePolicy {
    fn config(&self, app_id: Uuid) -> InvalidationQueuePolicyFuture<'_> {
        Box::pin(async move {
            let client = self
                .client
                .acquire()
                .await
                .map_err(|error| error.to_string())?;
            let statement = client
                .prepare_cached(&self.controls_sql)
                .await
                .map_err(|error| error.to_string())?;
            let rows = client
                .query(&statement, &[])
                .await
                .map_err(|error| error.to_string())?;
            let mut batch_limit = DEFAULT_BATCH_LIMIT;
            let mut drop_enabled = false;
            let mut drop_latency_value = None;
            for row in rows {
                let setting = row.get::<_, String>(0);
                let value = row.get::<_, Value>(1);
                match setting.as_str() {
                    "invalidator-batch-limit" => {
                        let limit = value.as_u64().ok_or_else(|| {
                            "config flag `invalidator-batch-limit` must be a non-negative integer"
                                .to_owned()
                        })?;
                        batch_limit = usize::try_from(limit).unwrap_or(usize::MAX);
                    }
                    "invalidator-drop-backpressure-apps" => {
                        drop_enabled = value.as_array().is_some_and(|apps| {
                            apps.iter().any(|value| {
                                value.as_str().and_then(|value| Uuid::parse_str(value).ok())
                                    == Some(app_id)
                            })
                        });
                    }
                    "invalidator-drop-tx-latency-ms" => {
                        drop_latency_value = Some(value);
                    }
                    _ => {}
                }
            }
            let drop_after = if drop_enabled {
                let millis = drop_latency_value.map_or(Ok(DEFAULT_DROP_LATENCY_MS), |value| {
                    value.as_u64().ok_or_else(|| {
                        "config flag `invalidator-drop-tx-latency-ms` must be a non-negative integer"
                            .to_owned()
                    })
                })?;
                Some(Duration::from_millis(millis))
            } else {
                None
            };
            Ok(InvalidationQueueConfig {
                batch_limit,
                drop_after,
            })
        })
    }
}

/// Per-app asynchronous invalidation lanes. Durable persistence occurs before
/// this consumer is called, so enqueue acceptance—not client recomputation—is
/// the replication acknowledgement boundary.
pub struct QueuedRealtimeInvalidationConsumer<C> {
    inner: Arc<QueueInner<C>>,
}

impl<C> Clone for QueuedRealtimeInvalidationConsumer<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct QueueInner<C> {
    consumer: C,
    policy: Arc<dyn InvalidationQueuePolicy>,
    state: Mutex<QueueState>,
    workers: Arc<Semaphore>,
    space_available: Notify,
}

#[derive(Default)]
struct QueueState {
    groups: HashMap<Uuid, AppGroup>,
}

#[derive(Default)]
struct AppGroup {
    pending: VecDeque<QueuedOutcome>,
    running: bool,
}

struct QueuedOutcome {
    outcome: ReplicationPipelineOutcome,
    enqueued_at: Instant,
    batch_limit: usize,
}

enum EnqueueDecision {
    Wait { drop_at: Option<Instant> },
    Drop,
    Enqueued { start_worker: bool },
}

impl<C> QueuedRealtimeInvalidationConsumer<C>
where
    C: RealtimeInvalidationConsumer + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(consumer: C, policy: Arc<dyn InvalidationQueuePolicy>) -> Self {
        Self::with_max_workers(consumer, policy, DEFAULT_MAX_WORKERS)
    }

    #[must_use]
    pub fn with_max_workers(
        consumer: C,
        policy: Arc<dyn InvalidationQueuePolicy>,
        max_workers: usize,
    ) -> Self {
        Self {
            inner: Arc::new(QueueInner {
                consumer,
                policy,
                state: Mutex::new(QueueState::default()),
                workers: Arc::new(Semaphore::new(max_workers.max(1))),
                space_available: Notify::new(),
            }),
        }
    }

    async fn enqueue(&self, outcome: ReplicationPipelineOutcome) -> Result<(), String> {
        let Some(record) = outcome.record.as_ref() else {
            return self.inner.consumer.deliver(&outcome).await;
        };
        let app_id = record.app_id;
        let config = self.inner.policy.config(app_id).await?;
        let capacity = config
            .batch_limit
            .max(1)
            .saturating_mul(MAX_PENDING_BATCHES_PER_APP);
        let mut outcome = Some(outcome);
        let start_worker = loop {
            let notified = self.inner.space_available.notified();
            let decision = {
                let mut state = queue_state(&self.inner.state);
                let group = state.groups.entry(app_id).or_default();
                let now = Instant::now();
                let latency = group
                    .pending
                    .front()
                    .map_or(Duration::ZERO, |head| now.duration_since(head.enqueued_at));
                if config
                    .drop_after
                    .is_some_and(|drop_after| latency.as_millis() > drop_after.as_millis())
                {
                    EnqueueDecision::Drop
                } else if group.pending.len() >= capacity {
                    let drop_at = config.drop_after.and_then(|drop_after| {
                        group
                            .pending
                            .front()
                            .map(|head| head.enqueued_at + drop_after + Duration::from_millis(1))
                    });
                    EnqueueDecision::Wait { drop_at }
                } else {
                    let Some(outcome) = outcome.take() else {
                        return Err("invalidation outcome was already enqueued".to_owned());
                    };
                    group.pending.push_back(QueuedOutcome {
                        outcome,
                        enqueued_at: now,
                        batch_limit: config.batch_limit,
                    });
                    let start_worker = !group.running;
                    group.running = true;
                    EnqueueDecision::Enqueued { start_worker }
                }
            };
            match decision {
                EnqueueDecision::Wait {
                    drop_at: Some(drop_at),
                } => {
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep_until(drop_at) => {}
                    }
                }
                EnqueueDecision::Wait { drop_at: None } => notified.await,
                EnqueueDecision::Drop => return Ok(()),
                EnqueueDecision::Enqueued { start_worker } => break start_worker,
            }
        };
        if start_worker {
            let inner = Arc::clone(&self.inner);
            std::mem::drop(tokio::spawn(async move {
                run_app_group(inner, app_id).await;
            }));
        }
        Ok(())
    }
}

impl<C> RealtimeInvalidationConsumer for QueuedRealtimeInvalidationConsumer<C>
where
    C: RealtimeInvalidationConsumer + Send + Sync + 'static,
{
    fn requires_local_delivery(&self) -> bool {
        self.inner.consumer.requires_local_delivery()
    }

    async fn stage(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        self.inner.consumer.stage(outcome).await
    }

    async fn stage_persisted(
        &self,
        outcome: &ReplicationPipelineOutcome,
        published_machine: Option<Uuid>,
    ) -> Result<(), String> {
        self.inner
            .consumer
            .stage_persisted(outcome, published_machine)
            .await
    }

    async fn deliver(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        // Cluster-backed consumers already staged the durable event. Their
        // separate ordered lane owns delivery, so there is no local work to queue.
        if !self.inner.consumer.requires_local_delivery() {
            return Ok(());
        }
        self.enqueue(outcome.clone()).await
    }

    async fn deliver_byop(
        &self,
        app_id: Uuid,
        record: &instant_pg::ByopWalRecord,
        topics: &[instant_pg::ByopTopic],
    ) -> Result<(), String> {
        self.inner
            .consumer
            .deliver_byop(app_id, record, topics)
            .await
    }

    async fn stage_byop(
        &self,
        app_id: Uuid,
        record: &instant_pg::ByopWalRecord,
        topics: &[instant_pg::ByopTopic],
    ) -> Result<(), String> {
        self.inner.consumer.stage_byop(app_id, record, topics).await
    }

    async fn evict_cache(&self, eviction: &WalCacheEviction) -> Result<(), String> {
        self.inner.consumer.evict_cache(eviction).await
    }

    async fn observe_max_seen_isn(&self, isn: instant_wal::Isn) -> Result<(), String> {
        self.inner.consumer.observe_max_seen_isn(isn).await
    }
}

async fn run_app_group<C>(inner: Arc<QueueInner<C>>, app_id: Uuid)
where
    C: RealtimeInvalidationConsumer + Send + Sync + 'static,
{
    let Ok(_permit) = Arc::clone(&inner.workers).acquire_owned().await else {
        return;
    };
    loop {
        let Some(outcome) = take_combined(&inner, app_id) else {
            return;
        };
        let _isolated_delivery_failure = inner.consumer.deliver(&outcome).await;
    }
}

fn take_combined<C>(inner: &QueueInner<C>, app_id: Uuid) -> Option<ReplicationPipelineOutcome> {
    let mut state = queue_state(&inner.state);
    let group = state.groups.get_mut(&app_id)?;
    let Some(mut queued) = group.pending.pop_front() else {
        state.groups.remove(&app_id);
        return None;
    };
    while queued
        .outcome
        .record
        .as_ref()
        .is_some_and(|record| record.combined_count < queued.batch_limit)
    {
        let Some(next) = group.pending.pop_front() else {
            break;
        };
        queued.outcome = combine_outcomes(queued.outcome, next.outcome);
    }
    drop(state);
    inner.space_available.notify_waiters();
    Some(queued.outcome)
}

fn combine_outcomes(
    mut older: ReplicationPipelineOutcome,
    newer: ReplicationPipelineOutcome,
) -> ReplicationPipelineOutcome {
    if let (Some(older_record), Some(newer_record)) = (older.record.take(), newer.record) {
        older.record = combine_wal_records(older_record, newer_record, usize::MAX)
            .expect("per-app invalidation lane cannot mix app ids");
    }
    older.dispatch = combine_dispatch(older.dispatch.take(), newer.dispatch);
    older.max_seen_isn = newer.max_seen_isn.or(older.max_seen_isn);
    older.durable_lsn = newer.durable_lsn.or(older.durable_lsn);
    older.feedback = newer.feedback.or(older.feedback);
    older
}

fn combine_dispatch(
    older: Option<DispatchOutcome>,
    newer: Option<DispatchOutcome>,
) -> Option<DispatchOutcome> {
    match (older, newer) {
        (Some(mut older), Some(newer)) => {
            older.refresh_sessions.extend(newer.refresh_sessions);
            older.invalidated_datalog_queries = older
                .invalidated_datalog_queries
                .saturating_add(newer.invalidated_datalog_queries);
            older.topic_count = older.topic_count.saturating_add(newer.topic_count);
            older.schema_refresh |= newer.schema_refresh;
            if older.continuity_gap.is_none() {
                older.continuity_gap = newer.continuity_gap;
            }
            older.durable_lsn = newer.durable_lsn;
            Some(older)
        }
        (older @ Some(_), None) => older,
        (None, newer) => newer,
    }
}

fn queue_state(state: &Mutex<QueueState>) -> MutexGuard<'_, QueueState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use instant_wal::{Isn, Lsn, WalRecord};
    use tokio::sync::{Notify, mpsc};

    use super::*;

    struct StaticPolicy(InvalidationQueueConfig);

    impl InvalidationQueuePolicy for StaticPolicy {
        fn config(&self, _app_id: Uuid) -> InvalidationQueuePolicyFuture<'_> {
            Box::pin(std::future::ready(Ok(self.0)))
        }
    }

    #[derive(Clone)]
    struct BlockingConsumer {
        calls: Arc<AtomicUsize>,
        first_entered: Arc<Notify>,
        release_first: Arc<Notify>,
        delivered: mpsc::UnboundedSender<ReplicationPipelineOutcome>,
    }

    impl RealtimeInvalidationConsumer for BlockingConsumer {
        async fn deliver(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_entered.notify_one();
                self.release_first.notified().await;
            }
            self.delivered
                .send(outcome.clone())
                .map_err(|error| error.to_string())
        }
    }

    #[derive(Clone)]
    struct StagedConsumer(mpsc::UnboundedSender<&'static str>);

    impl RealtimeInvalidationConsumer for StagedConsumer {
        fn stage(
            &self,
            _outcome: &ReplicationPipelineOutcome,
        ) -> impl Future<Output = Result<(), String>> + Send {
            std::future::ready(self.0.send("stage").map_err(|error| error.to_string()))
        }

        fn deliver(
            &self,
            _outcome: &ReplicationPipelineOutcome,
        ) -> impl Future<Output = Result<(), String>> + Send {
            std::future::ready(self.0.send("apply").map_err(|error| error.to_string()))
        }
    }

    struct ClusterOnlyConsumer(mpsc::UnboundedSender<&'static str>);

    impl RealtimeInvalidationConsumer for ClusterOnlyConsumer {
        fn requires_local_delivery(&self) -> bool {
            false
        }

        async fn stage(&self, _: &ReplicationPipelineOutcome) -> Result<(), String> {
            self.0.send("stage").map_err(|error| error.to_string())
        }

        async fn deliver(&self, _: &ReplicationPipelineOutcome) -> Result<(), String> {
            Err("cluster delivery must use its durable lane".to_owned())
        }
    }

    struct UnavailablePolicy;

    impl InvalidationQueuePolicy for UnavailablePolicy {
        fn config(&self, _: Uuid) -> InvalidationQueuePolicyFuture<'_> {
            Box::pin(std::future::ready(
                Err("unnecessary policy read".to_owned()),
            ))
        }
    }

    #[tokio::test]
    async fn cluster_only_pair_stages_without_consulting_local_queue_policy() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            (
                Arc::new(ClusterOnlyConsumer(events.clone())),
                Arc::new(ClusterOnlyConsumer(events)),
            ),
            Arc::new(UnavailablePolicy),
        );
        let outcome = outcome(Uuid::new_v4(), 1);
        queue.stage(&outcome).await.unwrap();
        assert_eq!(receiver.recv().await, Some("stage"));
        assert_eq!(receiver.recv().await, Some("stage"));
        queue
            .deliver(&outcome)
            .await
            .expect("staged cluster delivery needs no local queue");
        assert!(queue_state(&queue.inner.state).groups.is_empty());
    }

    #[tokio::test]
    async fn mixed_cluster_and_local_pair_still_requires_queue_policy() {
        let (events, _) = mpsc::unbounded_channel();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            (
                Arc::new(ClusterOnlyConsumer(events.clone())),
                Arc::new(StagedConsumer(events)),
            ),
            Arc::new(UnavailablePolicy),
        );
        assert_eq!(
            queue
                .deliver(&outcome(Uuid::new_v4(), 1))
                .await
                .unwrap_err(),
            "unnecessary policy read"
        );
    }

    #[tokio::test]
    async fn queue_forwards_durable_stage_before_async_application() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            StagedConsumer(events),
            Arc::new(StaticPolicy(InvalidationQueueConfig::default())),
        );
        let outcome = outcome(Uuid::new_v4(), 1);

        queue.stage(&outcome).await.unwrap();
        assert_eq!(receiver.recv().await, Some("stage"));
        queue.deliver(&outcome).await.unwrap();
        assert_eq!(receiver.recv().await, Some("apply"));
    }

    #[tokio::test]
    async fn app_lane_coalesces_adjacent_records_after_the_active_delivery() {
        let app_id = Uuid::new_v4();
        let (consumer, mut delivered, first_entered, release_first) = blocking_consumer();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            consumer,
            Arc::new(StaticPolicy(InvalidationQueueConfig {
                batch_limit: 500,
                drop_after: None,
            })),
        );

        queue.deliver(&outcome(app_id, 1)).await.unwrap();
        first_entered.notified().await;
        queue.deliver(&outcome(app_id, 2)).await.unwrap();
        queue.deliver(&outcome(app_id, 3)).await.unwrap();
        release_first.notify_one();

        assert_eq!(delivered.recv().await.unwrap().record.unwrap().tx_id, 1);
        let combined = delivered.recv().await.unwrap().record.unwrap();
        assert_eq!(combined.tx_id, 3);
        assert_eq!(combined.combined_count, 2);
        assert_eq!(combined.tx_bytes, 5);
        assert_eq!(combined.previous_isn, Some(Isn::new(1, Lsn::from_u64(1))));
        assert_eq!(combined.next_lsn, Lsn::from_u64(3));
    }

    #[tokio::test]
    async fn backpressure_drops_only_new_records_behind_an_overdue_pending_head() {
        let app_id = Uuid::new_v4();
        let (consumer, mut delivered, first_entered, release_first) = blocking_consumer();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            consumer,
            Arc::new(StaticPolicy(InvalidationQueueConfig {
                batch_limit: 1,
                drop_after: Some(Duration::from_millis(5)),
            })),
        );

        queue.deliver(&outcome(app_id, 1)).await.unwrap();
        first_entered.notified().await;
        queue.deliver(&outcome(app_id, 2)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        queue.deliver(&outcome(app_id, 3)).await.unwrap();
        release_first.notify_one();

        assert_eq!(delivered.recv().await.unwrap().record.unwrap().tx_id, 1);
        assert_eq!(delivered.recv().await.unwrap().record.unwrap().tx_id, 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), delivered.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sustained_backpressure_keeps_a_blocked_app_lane_bounded_without_dropping() {
        let app_id = Uuid::new_v4();
        let (consumer, mut delivered, first_entered, release_first) = blocking_consumer();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            consumer,
            Arc::new(StaticPolicy(InvalidationQueueConfig {
                batch_limit: 1,
                drop_after: None,
            })),
        );

        queue.deliver(&outcome(app_id, 1)).await.unwrap();
        first_entered.notified().await;
        let producer_queue = queue.clone();
        let producer = tokio::spawn(async move {
            for tx_id in 2..=4_097 {
                producer_queue
                    .deliver(&outcome(app_id, tx_id))
                    .await
                    .unwrap();
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(
            queue_state(&queue.inner.state).groups[&app_id]
                .pending
                .len(),
            MAX_PENDING_BATCHES_PER_APP
        );

        release_first.notify_one();
        producer.await.unwrap();
        for expected_tx_id in 1..=4_097 {
            assert_eq!(
                delivered.recv().await.unwrap().record.unwrap().tx_id,
                expected_tx_id
            );
        }
    }

    #[tokio::test]
    async fn a_full_lane_still_applies_the_configured_drop_deadline() {
        let app_id = Uuid::new_v4();
        let (consumer, mut delivered, first_entered, release_first) = blocking_consumer();
        let queue = QueuedRealtimeInvalidationConsumer::new(
            consumer,
            Arc::new(StaticPolicy(InvalidationQueueConfig {
                batch_limit: 1,
                drop_after: Some(Duration::from_millis(5)),
            })),
        );

        queue.deliver(&outcome(app_id, 1)).await.unwrap();
        first_entered.notified().await;
        for tx_id in 2..=9 {
            queue.deliver(&outcome(app_id, tx_id)).await.unwrap();
        }
        tokio::time::timeout(
            Duration::from_millis(30),
            queue.deliver(&outcome(app_id, 10)),
        )
        .await
        .expect("the latency deadline must wake a full lane")
        .unwrap();

        release_first.notify_one();
        for expected_tx_id in 1..=9 {
            assert_eq!(
                delivered.recv().await.unwrap().record.unwrap().tx_id,
                expected_tx_id
            );
        }
    }

    fn blocking_consumer() -> (
        BlockingConsumer,
        mpsc::UnboundedReceiver<ReplicationPipelineOutcome>,
        Arc<Notify>,
        Arc<Notify>,
    ) {
        let (delivered, receiver) = mpsc::unbounded_channel();
        let first_entered = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        (
            BlockingConsumer {
                calls: Arc::new(AtomicUsize::new(0)),
                first_entered: Arc::clone(&first_entered),
                release_first: Arc::clone(&release_first),
                delivered,
            },
            receiver,
            first_entered,
            release_first,
        )
    }

    fn outcome(app_id: Uuid, tx_id: i64) -> ReplicationPipelineOutcome {
        let lsn = Lsn::from_u64(u64::try_from(tx_id).unwrap());
        ReplicationPipelineOutcome {
            cache_eviction: None,
            record: Some(WalRecord {
                app_id,
                tx_id,
                isn: Isn::new(1, lsn),
                previous_isn: Some(Isn::new(
                    1,
                    Lsn::from_u64(u64::try_from(tx_id - 1).unwrap()),
                )),
                tx_created_at: None,
                tx_bytes: usize::try_from(tx_id).unwrap(),
                next_lsn: lsn,
                attr_changes: Vec::new(),
                ident_changes: Vec::new(),
                triple_changes: Vec::new(),
                messages: Vec::new(),
                wal_logs: Vec::new(),
                combined_count: 1,
            }),
            dispatch: None,
            max_seen_isn: Some(Isn::new(1, lsn)),
            durable_lsn: Some(lsn),
            feedback: None,
        }
    }
}
