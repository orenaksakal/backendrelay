use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use chrono::Utc;
use instant_pg::{
    ByopSchema, ByopTopic, ByopTopicError, ByopWalRecord, ReplicationCancellationToken,
    ReplicationTransportError, SketchDeltaBatch, SketchPersistenceError, SketchWalError,
    Wal2JsonReplicationConnection, aggregate_sketch_transaction, persist_sketch_delta_batches,
    topics_for_byop_record, transform_raw_byop_transaction,
};
use instant_realtime::{
    RealtimeStore, ReplicationPipelineError, ReplicationPipelineOutcome, WalInvalidationPipeline,
};
use instant_wal::{
    ByopWal2JsonReplicationDecoder, Isn, LogicalReplicationOptions, Lsn, ReplicationProtocolError,
    ReplicationStreamError, SlotPurpose, WalCacheEviction,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{PostgresPool, PostgresPoolError};

#[derive(Debug, Error)]
pub enum InvalidationRuntimeError {
    #[error("replication status interval must be greater than zero")]
    InvalidStatusInterval,
    #[error(transparent)]
    Transport(#[from] ReplicationTransportError),
    #[error(transparent)]
    Pipeline(#[from] ReplicationPipelineError),
    #[error(transparent)]
    ByopTopic(#[from] ByopTopicError),
    #[error("durable invalidation sink failed: {0}")]
    Downstream(String),
    #[error("application WAL record has no durable commit LSN")]
    MissingDurableLsn,
}

/// Persists history/webhook work and schedules refresh delivery before WAL is flushed.
pub trait DurableInvalidationSink: Send {
    type Error: std::error::Error + Send + Sync + 'static;

    fn persist(
        &mut self,
        outcome: &ReplicationPipelineOutcome,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Maximum number of original committed records accepted by `persist_batch`.
    /// Serial sinks retain their existing delivery and acknowledgement behavior.
    fn max_batch_records(&self) -> usize {
        1
    }

    /// Persists an ordered group without replacing its transaction identities.
    /// A successful return covers every record; callers must not acknowledge any
    /// part of the group on error, including when this serial default made progress.
    fn persist_batch(
        &mut self,
        outcomes: &[ReplicationPipelineOutcome],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move {
            for outcome in outcomes {
                self.persist(outcome).await?;
            }
            Ok(())
        }
    }

    /// Identifies atomic cluster publication performed by every successful
    /// `persist` call. The default requires the consumer to publish during staging.
    fn published_cluster_machine(&self) -> Option<Uuid> {
        None
    }

    fn persist_byop(
        &mut self,
        _app_id: Uuid,
        _record: &ByopWalRecord,
        _topics: &[ByopTopic],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    /// Applies raw-row cache work immediately without writing WAL history.
    fn evict_cache(
        &mut self,
        _eviction: &WalCacheEviction,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    /// Advances the process-global ISN used to seed newly-created app state.
    fn observe_max_seen_isn(
        &mut self,
        _isn: Isn,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Delivers a durably persisted WAL outcome into a live runtime coordinator.
pub trait RealtimeInvalidationConsumer: Send + Sync {
    /// Whether WAL delivery has local work after durable staging. Cluster-backed
    /// consumers may return false when their ordered cluster lane owns delivery.
    /// This never skips staging, cache eviction, or replication acknowledgments.
    fn requires_local_delivery(&self) -> bool {
        true
    }

    /// Durably stages cross-process delivery before the replication position is
    /// allowed to advance. Implementations that only mutate local memory can
    /// keep the default no-op.
    fn stage(
        &self,
        _outcome: &ReplicationPipelineOutcome,
    ) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Ok(()))
    }

    /// Runs only after durable persistence succeeds. A matching publisher lets
    /// a cluster consumer release writers without publishing the same event twice.
    fn stage_persisted(
        &self,
        outcome: &ReplicationPipelineOutcome,
        _published_machine: Option<Uuid>,
    ) -> impl Future<Output = Result<(), String>> + Send {
        self.stage(outcome)
    }

    fn deliver(
        &self,
        outcome: &ReplicationPipelineOutcome,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Durably stages an external-database invalidation before its replication
    /// position is acknowledged.
    fn stage_byop(
        &self,
        _app_id: Uuid,
        _record: &ByopWalRecord,
        _topics: &[ByopTopic],
    ) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Ok(()))
    }

    fn deliver_byop(
        &self,
        _app_id: Uuid,
        _record: &ByopWalRecord,
        _topics: &[ByopTopic],
    ) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Ok(()))
    }

    fn evict_cache(
        &self,
        _eviction: &WalCacheEviction,
    ) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Ok(()))
    }

    fn observe_max_seen_isn(&self, _isn: Isn) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Ok(()))
    }
}

impl<T> RealtimeInvalidationConsumer for std::sync::Arc<T>
where
    T: RealtimeInvalidationConsumer,
{
    fn requires_local_delivery(&self) -> bool {
        T::requires_local_delivery(self)
    }

    async fn stage(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        T::stage(self, outcome).await
    }

    async fn stage_persisted(
        &self,
        outcome: &ReplicationPipelineOutcome,
        published_machine: Option<Uuid>,
    ) -> Result<(), String> {
        T::stage_persisted(self, outcome, published_machine).await
    }

    async fn deliver(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        T::deliver(self, outcome).await
    }

    async fn deliver_byop(
        &self,
        app_id: Uuid,
        record: &ByopWalRecord,
        topics: &[ByopTopic],
    ) -> Result<(), String> {
        T::deliver_byop(self, app_id, record, topics).await
    }

    async fn stage_byop(
        &self,
        app_id: Uuid,
        record: &ByopWalRecord,
        topics: &[ByopTopic],
    ) -> Result<(), String> {
        T::stage_byop(self, app_id, record, topics).await
    }

    async fn evict_cache(&self, eviction: &WalCacheEviction) -> Result<(), String> {
        T::evict_cache(self, eviction).await
    }

    async fn observe_max_seen_isn(&self, isn: Isn) -> Result<(), String> {
        T::observe_max_seen_isn(self, isn).await
    }
}

impl<A, B> RealtimeInvalidationConsumer for (A, B)
where
    A: RealtimeInvalidationConsumer,
    B: RealtimeInvalidationConsumer,
{
    fn requires_local_delivery(&self) -> bool {
        self.0.requires_local_delivery() || self.1.requires_local_delivery()
    }

    async fn stage(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        let first = self.0.stage(outcome).await;
        let second = self.1.stage(outcome).await;
        first.and(second)
    }

    async fn stage_persisted(
        &self,
        outcome: &ReplicationPipelineOutcome,
        published_machine: Option<Uuid>,
    ) -> Result<(), String> {
        let first = self.0.stage_persisted(outcome, published_machine).await;
        let second = self.1.stage_persisted(outcome, published_machine).await;
        first.and(second)
    }

    async fn deliver(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
        let first = self.0.deliver(outcome).await;
        let second = self.1.deliver(outcome).await;
        first.and(second)
    }

    async fn deliver_byop(
        &self,
        app_id: Uuid,
        record: &ByopWalRecord,
        topics: &[ByopTopic],
    ) -> Result<(), String> {
        let first = self.0.deliver_byop(app_id, record, topics).await;
        let second = self.1.deliver_byop(app_id, record, topics).await;
        first.and(second)
    }

    async fn stage_byop(
        &self,
        app_id: Uuid,
        record: &ByopWalRecord,
        topics: &[ByopTopic],
    ) -> Result<(), String> {
        let first = self.0.stage_byop(app_id, record, topics).await;
        let second = self.1.stage_byop(app_id, record, topics).await;
        first.and(second)
    }

    async fn evict_cache(&self, eviction: &WalCacheEviction) -> Result<(), String> {
        let first = self.0.evict_cache(eviction).await;
        let second = self.1.evict_cache(eviction).await;
        first.and(second)
    }

    async fn observe_max_seen_isn(&self, isn: Isn) -> Result<(), String> {
        let first = self.0.observe_max_seen_isn(isn).await;
        let second = self.1.observe_max_seen_isn(isn).await;
        first.and(second)
    }
}

/// Orders durable history/webhook persistence before runtime realtime delivery.
pub struct RealtimeInvalidationSink<S, C> {
    durable: S,
    consumer: C,
}

impl<S, C> RealtimeInvalidationSink<S, C> {
    #[must_use]
    pub const fn new(durable: S, consumer: C) -> Self {
        Self { durable, consumer }
    }

    #[must_use]
    pub fn into_parts(self) -> (S, C) {
        (self.durable, self.consumer)
    }
}

#[derive(Debug, Error)]
pub enum RealtimeInvalidationSinkError {
    #[error("durable invalidation sink failed: {0}")]
    Durable(String),
    #[error("runtime realtime delivery failed: {0}")]
    Realtime(String),
}

impl<S, C> DurableInvalidationSink for RealtimeInvalidationSink<S, C>
where
    S: DurableInvalidationSink + Send,
    C: RealtimeInvalidationConsumer,
{
    type Error = RealtimeInvalidationSinkError;

    async fn persist(&mut self, outcome: &ReplicationPipelineOutcome) -> Result<(), Self::Error> {
        self.durable
            .persist(outcome)
            .await
            .map_err(|error| RealtimeInvalidationSinkError::Durable(error.to_string()))?;
        self.consumer
            .stage_persisted(outcome, self.durable.published_cluster_machine())
            .await
            .map_err(RealtimeInvalidationSinkError::Realtime)?;
        self.consumer
            .deliver(outcome)
            .await
            .map_err(RealtimeInvalidationSinkError::Realtime)
    }

    fn max_batch_records(&self) -> usize {
        if self.consumer.requires_local_delivery() {
            1
        } else {
            self.durable.max_batch_records()
        }
    }

    async fn persist_batch(
        &mut self,
        outcomes: &[ReplicationPipelineOutcome],
    ) -> Result<(), Self::Error> {
        self.durable
            .persist_batch(outcomes)
            .await
            .map_err(|error| RealtimeInvalidationSinkError::Durable(error.to_string()))?;
        for outcome in outcomes {
            self.consumer
                .stage_persisted(outcome, self.durable.published_cluster_machine())
                .await
                .map_err(RealtimeInvalidationSinkError::Realtime)?;
            self.consumer
                .deliver(outcome)
                .await
                .map_err(RealtimeInvalidationSinkError::Realtime)?;
        }
        Ok(())
    }

    async fn persist_byop(
        &mut self,
        app_id: Uuid,
        record: &ByopWalRecord,
        topics: &[ByopTopic],
    ) -> Result<(), Self::Error> {
        self.consumer
            .stage_byop(app_id, record, topics)
            .await
            .map_err(RealtimeInvalidationSinkError::Realtime)?;
        self.consumer
            .deliver_byop(app_id, record, topics)
            .await
            .map_err(RealtimeInvalidationSinkError::Realtime)
    }

    async fn evict_cache(&mut self, eviction: &WalCacheEviction) -> Result<(), Self::Error> {
        self.durable
            .evict_cache(eviction)
            .await
            .map_err(|error| RealtimeInvalidationSinkError::Durable(error.to_string()))?;
        self.consumer
            .evict_cache(eviction)
            .await
            .map_err(RealtimeInvalidationSinkError::Realtime)
    }

    async fn observe_max_seen_isn(&mut self, isn: Isn) -> Result<(), Self::Error> {
        self.durable
            .observe_max_seen_isn(isn)
            .await
            .map_err(|error| RealtimeInvalidationSinkError::Durable(error.to_string()))?;
        self.consumer
            .observe_max_seen_isn(isn)
            .await
            .map_err(RealtimeInvalidationSinkError::Realtime)
    }
}

/// A second decoder over the invalidator's raw WAL stream for Instant-on-Instant.
/// Its durable position advances only after the configured application's live
/// queries have accepted the committed BYOP transaction.
#[derive(Debug)]
pub struct ByopInvalidationPipeline {
    app_id: Uuid,
    schema: ByopSchema,
    decoder: ByopWal2JsonReplicationDecoder,
}

impl ByopInvalidationPipeline {
    #[must_use]
    pub fn new(app_id: Uuid, schema: ByopSchema, confirmed_lsn: Lsn) -> Self {
        Self {
            app_id,
            schema,
            decoder: ByopWal2JsonReplicationDecoder::new(confirmed_lsn),
        }
    }

    fn push(
        &mut self,
        payload: &[u8],
        reply_at: chrono::DateTime<Utc>,
    ) -> Result<(Option<ByopWalRecord>, bool), ReplicationStreamError> {
        let outcome = self.decoder.push(payload, reply_at)?;
        Ok((
            outcome.transaction.and_then(transform_raw_byop_transaction),
            outcome.feedback.is_some(),
        ))
    }

    fn acknowledge(&mut self, durable_lsn: Lsn) {
        self.decoder.acknowledge(durable_lsn);
    }

    fn restart_after_disconnect(&mut self) {
        self.decoder.restart_after_disconnect();
    }

    fn feedback_state(&self) -> instant_wal::FeedbackState {
        self.decoder.feedback_state()
    }
}

/// Sink for deployments that intentionally need only local in-memory invalidation.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopInvalidationSink;

impl DurableInvalidationSink for NoopInvalidationSink {
    type Error = std::convert::Infallible;

    fn persist(
        &mut self,
        _outcome: &ReplicationPipelineOutcome,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

#[derive(Debug, Error)]
pub enum InvalidationSupervisorError {
    #[error(transparent)]
    Runtime(#[from] InvalidationRuntimeError),
    #[error(transparent)]
    Options(#[from] ReplicationProtocolError),
    #[error("replication connection worker failed: {0}")]
    ConnectionWorker(#[from] tokio::task::JoinError),
    #[error("sketch aggregator stream failed: {0}")]
    Aggregator(String),
}

#[derive(Debug, Error)]
pub enum SketchAggregatorRuntimeError {
    #[error("replication status interval must be greater than zero")]
    InvalidStatusInterval,
    #[error("invalid derived-statistics persistence group bounds")]
    InvalidBatchPolicy,
    #[error(transparent)]
    Transport(#[from] ReplicationTransportError),
    #[error(transparent)]
    Stream(#[from] ReplicationStreamError),
    #[error(transparent)]
    Wal(#[from] SketchWalError),
    #[error(transparent)]
    Persistence(#[from] SketchPersistenceError),
}

#[derive(Debug, Error)]
pub enum SketchAggregatorSupervisorError {
    #[error(transparent)]
    Runtime(#[from] SketchAggregatorRuntimeError),
    #[error(transparent)]
    Options(#[from] ReplicationProtocolError),
    #[error("replication connection worker failed: {0}")]
    ConnectionWorker(#[from] tokio::task::JoinError),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
}

/// Bounded exponential reconnect delays for transient transport failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconnectPolicy {
    initial: Duration,
    maximum: Duration,
}

impl ReconnectPolicy {
    #[must_use]
    pub const fn new(initial: Duration, maximum: Duration) -> Self {
        Self { initial, maximum }
    }

    #[must_use]
    pub fn delay(self, retry: u32) -> Duration {
        self.initial
            .saturating_mul(1_u32.checked_shl(retry.min(31)).unwrap_or(u32::MAX))
            .min(self.maximum)
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), Duration::from_secs(30))
    }
}

/// Best-effort readiness observer for the primary invalidation stream.
pub trait InvalidationHealthObserver: Send + Sync {
    fn set_healthy(&self, healthy: bool);

    fn graceful_shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.set_healthy(true);
        Box::pin(std::future::ready(()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidationHealthTransitions {
    restarts: [Option<Instant>; STREAM_RESTARTS_PER_WINDOW + 1],
    next_restart: usize,
    published: Option<bool>,
}

const STREAM_RESTART_WINDOW: Duration = Duration::from_mins(10);
const STREAM_RESTARTS_PER_WINDOW: usize = 3;
const HEALTH_EVALUATION_INTERVAL: Duration = Duration::from_secs(5);

impl Default for InvalidationHealthTransitions {
    fn default() -> Self {
        Self {
            restarts: [None; STREAM_RESTARTS_PER_WINDOW + 1],
            next_restart: 0,
            // Unknown until the supervisor observes the stream. The first
            // evaluation publishes the current state, which also clears a
            // marker left behind by an earlier supervisor run in this process.
            published: None,
        }
    }
}

impl InvalidationHealthTransitions {
    #[must_use]
    pub const fn disconnected(&self) -> bool {
        false
    }

    /// Records a producer restart without publishing readiness.
    pub fn record_restart(&mut self, now: Instant) {
        self.restarts[self.next_restart] = Some(now);
        self.next_restart = (self.next_restart + 1) % self.restarts.len();
    }

    #[must_use]
    pub fn restarts_in_window(&self, now: Instant) -> usize {
        let window_start = now.checked_sub(STREAM_RESTART_WINDOW);
        self.restarts
            .iter()
            .flatten()
            .filter(|restart| window_start.is_none_or(|start| **restart > start))
            .count()
    }

    /// More than three producer restarts in the rolling window means the stream
    /// is flapping and readiness must stay withheld.
    #[must_use]
    pub fn flapping_at(&self, now: Instant) -> bool {
        self.restarts_in_window(now) > STREAM_RESTARTS_PER_WINDOW
    }

    /// Health for a connected stream. Up to three restarts in the rolling
    /// window clear immediately on reconnect; a fourth withholds readiness
    /// until the oldest of them ages out of the window. Restarts that keep
    /// coming keep the count above the limit, so no separate stability period
    /// is needed, and the periodic evaluation clears health without waiting
    /// for another reconnect.
    #[must_use]
    pub fn healthy_at(&self, now: Instant) -> bool {
        !self.flapping_at(now)
    }

    /// A connection-stage failure is not a producer restart and therefore does
    /// not withhold producer readiness.
    #[must_use]
    pub const fn connected_after_connect_failure(&self) -> bool {
        true
    }

    #[must_use]
    pub const fn graceful_shutdown(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct NoopInvalidationHealthObserver;

impl InvalidationHealthObserver for NoopInvalidationHealthObserver {
    fn set_healthy(&self, _healthy: bool) {}
}

fn publish_health(
    transitions: &mut InvalidationHealthTransitions,
    observer: &dyn InvalidationHealthObserver,
    healthy: bool,
) {
    if transitions.published == Some(healthy) {
        return;
    }
    transitions.published = Some(healthy);
    crate::metrics::set_invalidation_health(healthy);
    observer.set_healthy(healthy);
}

fn observe_stream_disconnect(
    transitions: &mut InvalidationHealthTransitions,
    observer: &dyn InvalidationHealthObserver,
) {
    let healthy = transitions.disconnected();
    publish_health(transitions, observer, healthy);
}

fn observe_connect_stage_recovery(
    transitions: &mut InvalidationHealthTransitions,
    observer: &dyn InvalidationHealthObserver,
    slot_name: &str,
    attempt: u32,
) {
    let healthy = transitions.connected_after_connect_failure();
    publish_health(transitions, observer, healthy);
    tracing::info!(
        purpose = "invalidator",
        slot = %slot_name,
        attempt,
        "invalidation stream connected after a connect-stage failure"
    );
}

fn observe_restart(
    transitions: &mut InvalidationHealthTransitions,
    observer: &dyn InvalidationHealthObserver,
    slot_name: &str,
    now: Instant,
    attempt: u32,
) {
    transitions.record_restart(now);
    crate::metrics::record_invalidation_stream_restart();
    let restarts = transitions.restarts_in_window(now);
    if transitions.flapping_at(now) {
        tracing::warn!(
            purpose = "invalidator",
            slot = %slot_name,
            restarts,
            "invalidation health withheld while the stream is flapping"
        );
    }
    let healthy = transitions.healthy_at(now);
    publish_health(transitions, observer, healthy);
    tracing::info!(
        purpose = "invalidator",
        slot = %slot_name,
        attempt,
        "invalidation stream reconnected"
    );
}

fn log_stream_retry(
    slot_name: &str,
    error: &ReplicationTransportError,
    delay: Duration,
    connecting: bool,
) {
    let message = if connecting {
        "invalidation stream connect failed; retrying"
    } else {
        "invalidation stream failed; reconnecting"
    };
    tracing::warn!(
        purpose = "invalidator",
        slot = %slot_name,
        error_kind = transport_error_kind(error),
        retry_after_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        message,
    );
}

#[allow(clippy::too_many_arguments)] // Mirrors the inputs of the supervised stream.
async fn run_stream_with_health_ticks<S>(
    connection: &mut Wal2JsonReplicationConnection,
    pipeline: &mut WalInvalidationPipeline,
    byop: Option<&mut ByopInvalidationPipeline>,
    store: &mut RealtimeStore,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    sink: &mut S,
    health_transitions: &mut InvalidationHealthTransitions,
    health: &dyn InvalidationHealthObserver,
    slot_name: &str,
) -> Result<(), InvalidationRuntimeError>
where
    S: DurableInvalidationSink,
{
    let stream = run_invalidation_stream_inner(
        connection,
        pipeline,
        byop,
        store,
        cancellation,
        status_interval,
        sink,
    );
    tokio::pin!(stream);
    let mut health_timer = tokio::time::interval_at(
        tokio::time::Instant::now() + HEALTH_EVALUATION_INTERVAL,
        HEALTH_EVALUATION_INTERVAL,
    );
    health_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            result = &mut stream => return result,
            _ = health_timer.tick() => observe_health_tick(
                health_transitions,
                health,
                slot_name,
                Instant::now(),
            ),
        }
    }
}

async fn publish_graceful_shutdown(
    transitions: &mut InvalidationHealthTransitions,
    health: &dyn InvalidationHealthObserver,
) {
    debug_assert!(transitions.graceful_shutdown());
    transitions.published = Some(true);
    crate::metrics::set_invalidation_health(true);
    health.graceful_shutdown().await;
}

fn observe_health_tick(
    transitions: &mut InvalidationHealthTransitions,
    observer: &dyn InvalidationHealthObserver,
    slot_name: &str,
    now: Instant,
) {
    let was_healthy = transitions.published;
    let healthy = transitions.healthy_at(now);
    publish_health(transitions, observer, healthy);
    if was_healthy == Some(false) && healthy {
        tracing::info!(
            purpose = "invalidator",
            slot = %slot_name,
            "invalidation health cleared after the flapping window"
        );
    }
}

/// Drives one live wal2json slot until cancellation or failure.
///
/// The durable sink runs after each application commit mutates the realtime store and
/// before that commit is acknowledged. Periodic and requested standby feedback always
/// uses the pipeline's durable position; receiving bytes alone never advances the
/// slot's flush boundary.
///
/// # Errors
///
/// Returns on transport, decoding, transformation, or invalidation failure. Callers
/// should reconnect from the slot's confirmed flush LSN and recreate the pipeline.
pub async fn run_invalidation_stream<S>(
    connection: &mut Wal2JsonReplicationConnection,
    pipeline: &mut WalInvalidationPipeline,
    store: &mut RealtimeStore,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    sink: &mut S,
) -> Result<(), InvalidationRuntimeError>
where
    S: DurableInvalidationSink,
{
    run_invalidation_stream_inner(
        connection,
        pipeline,
        None,
        store,
        cancellation,
        status_interval,
        sink,
    )
    .await
}

async fn run_invalidation_stream_inner<S>(
    connection: &mut Wal2JsonReplicationConnection,
    pipeline: &mut WalInvalidationPipeline,
    mut byop: Option<&mut ByopInvalidationPipeline>,
    store: &mut RealtimeStore,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    sink: &mut S,
) -> Result<(), InvalidationRuntimeError>
where
    S: DurableInvalidationSink,
{
    if status_interval.is_zero() {
        return Err(InvalidationRuntimeError::InvalidStatusInterval);
    }
    let mut feedback_timer = ReplicationFeedbackTimer::new(status_interval);
    feedback_timer.advance();
    let batch_enabled = byop.is_none() && sink.max_batch_records() > 1;
    let mut deferred_frame = None;
    let mut stalled_replies = StalledReplyGuard::new("invalidator");

    loop {
        match next_replication_stream_event(&mut feedback_timer, cancellation, None, async {
            match deferred_frame.take() {
                Some(frame) => Ok(frame),
                None => connection.recv(cancellation).await,
            }
        })
        .await
        {
            ReplicationStreamEvent::Cancelled => return Ok(()),
            ReplicationStreamEvent::Feedback => {
                connection
                    .send_feedback(pipeline.feedback_state(), false)
                    .await?;
            }
            ReplicationStreamEvent::Flush => unreachable!("invalidation has no flush deadline"),
            ReplicationStreamEvent::Frame(frame) => {
                let frame = frame?;
                if batch_enabled {
                    let drained = drain_ready_invalidation_frames(
                        frame,
                        || connection.recv_ready(cancellation),
                        pipeline,
                        store,
                        sink,
                        cancellation,
                    );
                    let drained = tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Ok(()),
                        result = drained => result?,
                    };
                    let Some(drained) = drained else {
                        return Ok(());
                    };
                    deferred_frame = drained.deferred_frame;
                    if drained.reply_requested {
                        stalled_replies
                            .answer(connection, pipeline.feedback_state())
                            .await?;
                    }
                    continue;
                }
                let now = Utc::now();
                let byop_record = byop
                    .as_deref_mut()
                    .map(|pipeline| pipeline.push(&frame, now))
                    .transpose()
                    .map_err(ReplicationPipelineError::Stream)?
                    .and_then(|(record, _reply_requested)| record);
                let outcome = pipeline.push(store, &frame, now)?;
                let reply_requested = outcome.feedback.is_some();
                let delivery = Box::pin(deliver_invalidation_outcome(sink, &outcome));
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Ok(()),
                    result = delivery => result?,
                }
                if let Some(record) = &byop_record {
                    let pipeline = byop.as_deref().expect("BYOP record requires a pipeline");
                    let topics = topics_for_byop_record(&pipeline.schema, record)?;
                    let delivery = Box::pin(sink.persist_byop(pipeline.app_id, record, &topics));
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Ok(()),
                        result = delivery => {
                            result.map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
                        }
                    }
                }
                if let Some(durable_lsn) = outcome.durable_lsn {
                    pipeline.acknowledge(durable_lsn);
                }
                if let Some(record) = byop_record
                    && let Some(pipeline) = byop.as_deref_mut()
                {
                    pipeline.acknowledge(record.next_lsn);
                }
                if reply_requested {
                    stalled_replies
                        .answer(connection, pipeline.feedback_state())
                        .await?;
                }
            }
        }
    }
}

async fn deliver_invalidation_outcome<S>(
    sink: &mut S,
    outcome: &ReplicationPipelineOutcome,
) -> Result<(), InvalidationRuntimeError>
where
    S: DurableInvalidationSink,
{
    if let Some(isn) = outcome.max_seen_isn {
        sink.observe_max_seen_isn(isn)
            .await
            .map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
    }
    if let Some(eviction) = &outcome.cache_eviction {
        sink.evict_cache(eviction)
            .await
            .map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
    }
    if outcome.record.is_some() {
        sink.persist(outcome)
            .await
            .map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
    }
    Ok(())
}

const MAX_DURABLE_BATCH_RECORDS: usize = 32;
const MAX_DURABLE_BATCH_FRAMES: usize = 1_024;
const MAX_DURABLE_BATCH_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct PendingInvalidationBatch {
    outcomes: Vec<ReplicationPipelineOutcome>,
    durable_lsn: Option<Lsn>,
}

impl PendingInvalidationBatch {
    /// No acknowledgement escapes this gate before persistence and staging of
    /// the entire prefix succeed. A cancelled or failed commit is replayed.
    async fn flush<S>(
        &mut self,
        pipeline: &mut WalInvalidationPipeline,
        sink: &mut S,
        cancellation: &ReplicationCancellationToken,
    ) -> Result<bool, InvalidationRuntimeError>
    where
        S: DurableInvalidationSink,
    {
        if self.outcomes.is_empty() {
            return Ok(!cancellation.is_cancelled());
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(false),
            result = sink.persist_batch(&self.outcomes) => {
                result.map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
            }
        }
        if let Some(durable_lsn) = self.durable_lsn {
            pipeline.acknowledge(durable_lsn);
        }
        self.outcomes.clear();
        self.durable_lsn = None;
        Ok(true)
    }
}

struct ReadyInvalidationDrain {
    deferred_frame: Option<Vec<u8>>,
    reply_requested: bool,
}

/// Drains only frames already supplied by the transport. The callback must not
/// wait, so a quiet stream's first record commits without a batching delay.
/// BYOP and local-delivery sinks do not enter this path.
async fn drain_ready_invalidation_frames<S, F>(
    first_frame: Vec<u8>,
    mut ready_frame: F,
    pipeline: &mut WalInvalidationPipeline,
    store: &mut RealtimeStore,
    sink: &mut S,
    cancellation: &ReplicationCancellationToken,
) -> Result<Option<ReadyInvalidationDrain>, InvalidationRuntimeError>
where
    S: DurableInvalidationSink,
    F: FnMut() -> Option<Result<Vec<u8>, ReplicationTransportError>>,
{
    let record_limit = sink.max_batch_records().clamp(1, MAX_DURABLE_BATCH_RECORDS);
    let mut batch = PendingInvalidationBatch::default();
    let mut frame = first_frame;
    let mut frame_count = 0_usize;
    let mut byte_count = 0_usize;
    let mut result = ReadyInvalidationDrain {
        deferred_frame: None,
        reply_requested: false,
    };
    loop {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        frame_count += 1;
        byte_count = byte_count.saturating_add(frame.len());
        let outcome = pipeline.push(store, &frame, Utc::now())?;
        // Filtered commits and keepalives fence pending application records.
        // Their boundary cannot skip an earlier unfinished durable publication.
        let fence = outcome.record.is_none()
            && (outcome.durable_lsn.is_some() || outcome.feedback.is_some());
        if fence && !batch.flush(pipeline, sink, cancellation).await? {
            return Ok(None);
        }
        if let Some(isn) = outcome.max_seen_isn {
            sink.observe_max_seen_isn(isn)
                .await
                .map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
        }
        if let Some(eviction) = &outcome.cache_eviction {
            sink.evict_cache(eviction)
                .await
                .map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
        }
        result.reply_requested |= outcome.feedback.is_some();
        if outcome.record.is_some() {
            batch.durable_lsn = outcome.durable_lsn;
            batch.outcomes.push(outcome);
        } else if let Some(durable_lsn) = outcome.durable_lsn {
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            pipeline.acknowledge(durable_lsn);
        }
        if fence
            || batch.outcomes.len() >= record_limit
            || frame_count >= MAX_DURABLE_BATCH_FRAMES
            || byte_count >= MAX_DURABLE_BATCH_BYTES
        {
            break;
        }
        match ready_frame() {
            Some(Ok(next)) => {
                if byte_count.saturating_add(next.len()) > MAX_DURABLE_BATCH_BYTES {
                    // Keep this exact frame for the next drain, before parsing it.
                    // A single oversized valid frame retains serial-path semantics.
                    result.deferred_frame = Some(next);
                    break;
                }
                frame = next;
            }
            Some(Err(error)) => {
                // A reconnect retains the decoder's latest completed ISN. Finish
                // its decoded prefix first so replay starts from a matching LSN.
                if !batch.flush(pipeline, sink, cancellation).await? {
                    return Ok(None);
                }
                return Err(error.into());
            }
            None => break,
        }
    }
    if !batch.flush(pipeline, sink, cancellation).await? {
        return Ok(None);
    }
    Ok(Some(result))
}

/// Drives one external BYOP wal2json stream until cancellation or failure.
///
/// Unlike Instant's primary invalidator, an arbitrary PostgreSQL database has no
/// Instant transaction marker or WAL history sink. A commit is acknowledged only
/// after its app-scoped realtime invalidation has been accepted downstream.
///
/// # Errors
///
/// Returns on transport, decoding, transformation, topic, or delivery failure.
pub async fn run_byop_invalidation_stream<S>(
    connection: &mut Wal2JsonReplicationConnection,
    pipeline: &mut ByopInvalidationPipeline,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    sink: &mut S,
) -> Result<(), InvalidationRuntimeError>
where
    S: DurableInvalidationSink,
{
    if status_interval.is_zero() {
        return Err(InvalidationRuntimeError::InvalidStatusInterval);
    }
    let mut feedback_timer = ReplicationFeedbackTimer::new(status_interval);
    feedback_timer.advance();
    let mut stalled_replies = StalledReplyGuard::new("byop");

    loop {
        match next_replication_stream_event(
            &mut feedback_timer,
            cancellation,
            None,
            connection.recv(cancellation),
        )
        .await
        {
            ReplicationStreamEvent::Cancelled => return Ok(()),
            ReplicationStreamEvent::Feedback => {
                connection
                    .send_feedback(pipeline.feedback_state(), false)
                    .await?;
            }
            ReplicationStreamEvent::Flush => {
                unreachable!("BYOP invalidation has no flush deadline")
            }
            ReplicationStreamEvent::Frame(frame) => {
                let frame = frame?;
                let (record, reply_requested) = pipeline
                    .push(&frame, Utc::now())
                    .map_err(ReplicationPipelineError::Stream)?;
                if let Some(record) = record {
                    let topics = topics_for_byop_record(&pipeline.schema, &record)?;
                    let delivery = Box::pin(sink.persist_byop(pipeline.app_id, &record, &topics));
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Ok(()),
                        result = delivery => {
                            result.map_err(|error| InvalidationRuntimeError::Downstream(error.to_string()))?;
                        }
                    }
                    pipeline.acknowledge(record.next_lsn);
                }
                if reply_requested {
                    stalled_replies
                        .answer(connection, pipeline.feedback_state())
                        .await?;
                }
            }
        }
    }
}

/// Connects and supervises a dedicated external BYOP replication slot.
///
/// Reconnects resume at the last downstream-acknowledged LSN and reset partial
/// transaction assembly. The schema snapshot determines wal2json's table filter;
/// callers replace the supervisor when a refreshed schema changes that set.
///
/// # Errors
///
/// Returns a permanent connection, worker, configuration, or pipeline error.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_byop_invalidation_stream<S>(
    connection_string: &str,
    slot_name: &str,
    app_id: Uuid,
    namespace: &str,
    schema: ByopSchema,
    confirmed_lsn: Lsn,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    reconnect: ReconnectPolicy,
    sink: &mut S,
) -> Result<(), InvalidationSupervisorError>
where
    S: DurableInvalidationSink,
{
    let mut pipeline = ByopInvalidationPipeline::new(app_id, schema, confirmed_lsn);
    let tables = pipeline.schema.tables.keys().cloned().collect::<Vec<_>>();
    let mut retry = 0_u32;

    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let options = LogicalReplicationOptions::new_byop(
            slot_name,
            pipeline.feedback_state().flushed(),
            namespace,
            &tables,
        )?;
        let connection_string = connection_string.to_owned();
        let connected = tokio::task::spawn_blocking(move || {
            Wal2JsonReplicationConnection::connect(&connection_string, &options)
        })
        .await?;
        let mut connection = match connected {
            Ok(connection) => connection,
            Err(error) if retryable_transport(&error) => {
                wait_to_retry(cancellation, reconnect.delay(retry)).await;
                retry = retry.saturating_add(1);
                continue;
            }
            Err(error) => return Err(InvalidationRuntimeError::Transport(error).into()),
        };
        retry = 0;
        match run_byop_invalidation_stream(
            &mut connection,
            &mut pipeline,
            cancellation,
            status_interval,
            sink,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(InvalidationRuntimeError::Transport(error)) if retryable_transport(&error) => {
                pipeline.restart_after_disconnect();
                wait_to_retry(cancellation, reconnect.delay(retry)).await;
                retry = retry.saturating_add(1);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Connects and supervises a live invalidator, retrying transient transport failures.
///
/// Each reconnect starts at the last flushed LSN and resets partial wal2json assembly.
/// Protocol, transform, and topic failures are terminal. Slot creation and validation
/// remain a control-plane responsibility performed before this loop starts.
///
/// # Errors
///
/// Returns a permanent connection, worker, configuration, or pipeline error.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_invalidation_stream<S>(
    connection_string: &str,
    slot_name: &str,
    slot_num: i32,
    purpose: SlotPurpose,
    previous_isn: Option<Isn>,
    confirmed_lsn: Lsn,
    store: &mut RealtimeStore,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    reconnect: ReconnectPolicy,
    byop: Option<(Uuid, ByopSchema)>,
    sink: &mut S,
) -> Result<(), InvalidationSupervisorError>
where
    S: DurableInvalidationSink,
{
    supervise_invalidation_stream_with_health(
        connection_string,
        slot_name,
        slot_num,
        purpose,
        previous_isn,
        confirmed_lsn,
        store,
        cancellation,
        status_interval,
        reconnect,
        byop,
        sink,
        &NoopInvalidationHealthObserver,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
/// Supervises the primary invalidation stream and publishes fleet-visible WAL health.
///
/// # Errors
///
/// Returns terminal replication setup, connection, stream, or persistence failures.
pub async fn supervise_invalidation_stream_with_health<S>(
    connection_string: &str,
    slot_name: &str,
    slot_num: i32,
    purpose: SlotPurpose,
    previous_isn: Option<Isn>,
    confirmed_lsn: Lsn,
    store: &mut RealtimeStore,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    reconnect: ReconnectPolicy,
    byop: Option<(Uuid, ByopSchema)>,
    sink: &mut S,
    health: &dyn InvalidationHealthObserver,
) -> Result<(), InvalidationSupervisorError>
where
    S: DurableInvalidationSink,
{
    let mut pipeline = WalInvalidationPipeline::new(slot_num, previous_isn, confirmed_lsn);
    let mut byop =
        byop.map(|(app_id, schema)| ByopInvalidationPipeline::new(app_id, schema, confirmed_lsn));
    let mut retry = 0_u32;
    let mut health_transitions = InvalidationHealthTransitions::default();
    let mut stream_has_failed = false;
    let mut initial_connect_failed = false;

    loop {
        if cancellation.is_cancelled() {
            publish_graceful_shutdown(&mut health_transitions, health).await;
            return Ok(());
        }
        let options =
            LogicalReplicationOptions::new(slot_name, pipeline.feedback_state().flushed(), purpose)
                .inspect_err(|_| observe_stream_disconnect(&mut health_transitions, health))?;
        let connection_string = connection_string.to_owned();
        let connected = tokio::task::spawn_blocking(move || {
            Wal2JsonReplicationConnection::connect(&connection_string, &options)
        })
        .await
        .inspect_err(|_| observe_stream_disconnect(&mut health_transitions, health))?;
        let mut connection = match connected {
            Ok(connection) => connection,
            Err(error) if retryable_transport(&error) => {
                observe_stream_disconnect(&mut health_transitions, health);
                if !stream_has_failed {
                    initial_connect_failed = true;
                }
                let delay = reconnect.delay(retry);
                log_stream_retry(slot_name, &error, delay, true);
                wait_to_retry(cancellation, delay).await;
                retry = retry.saturating_add(1);
                continue;
            }
            Err(error) => {
                observe_stream_disconnect(&mut health_transitions, health);
                return Err(InvalidationRuntimeError::Transport(error).into());
            }
        };
        if stream_has_failed {
            observe_restart(
                &mut health_transitions,
                health,
                slot_name,
                Instant::now(),
                retry,
            );
        } else if initial_connect_failed {
            observe_connect_stage_recovery(&mut health_transitions, health, slot_name, retry);
        }
        retry = 0;
        let stream_result = run_stream_with_health_ticks(
            &mut connection,
            &mut pipeline,
            byop.as_mut(),
            store,
            cancellation,
            status_interval,
            sink,
            &mut health_transitions,
            health,
            slot_name,
        )
        .await;
        match stream_result {
            Ok(()) => {
                publish_graceful_shutdown(&mut health_transitions, health).await;
                return Ok(());
            }
            Err(InvalidationRuntimeError::Transport(error)) if retryable_transport(&error) => {
                observe_stream_disconnect(&mut health_transitions, health);
                stream_has_failed = true;
                pipeline.restart_after_disconnect();
                if let Some(byop) = &mut byop {
                    byop.restart_after_disconnect();
                }
                let delay = reconnect.delay(retry);
                log_stream_retry(slot_name, &error, delay, false);
                wait_to_retry(cancellation, delay).await;
                retry = retry.saturating_add(1);
            }
            Err(error) => {
                observe_stream_disconnect(&mut health_transitions, health);
                return Err(error.into());
            }
        }
    }
}

enum ReplicationStreamEvent<T> {
    Cancelled,
    Feedback,
    Flush,
    Frame(T),
}

const SKETCH_BATCH_MAX_TRANSACTIONS: usize = 64;
const SKETCH_BATCH_MAX_WAL_BYTES: usize = 1024 * 1024;
const SKETCH_BATCH_MAX_WAIT: Duration = Duration::from_millis(50);

/// Bounds derived-statistics persistence, not application transaction durability.
/// Feedback remains at the last committed checkpoint until the whole group commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SketchBatchPolicy {
    pub max_transactions: usize,
    pub max_wal_bytes: usize,
    pub max_wait: Duration,
}

impl Default for SketchBatchPolicy {
    fn default() -> Self {
        Self {
            max_transactions: SKETCH_BATCH_MAX_TRANSACTIONS,
            max_wal_bytes: SKETCH_BATCH_MAX_WAL_BYTES,
            max_wait: SKETCH_BATCH_MAX_WAIT,
        }
    }
}

impl SketchBatchPolicy {
    #[must_use]
    pub fn valid(self) -> bool {
        (1..=65_536).contains(&self.max_transactions)
            && (1..=64 * 1024 * 1024).contains(&self.max_wal_bytes)
            && !self.max_wait.is_zero()
            && self.max_wait <= Duration::from_secs(10)
    }
}

// Check logical deadlines before polling a ready WAL future. A newly registered
// Tokio sleep/interval can still return Pending until the timer driver catches
// up, even when its deadline is already due. Under a ready backlog that must not
// postpone feedback or the bounded persistence flush.
struct ReplicationFeedbackTimer {
    next: tokio::time::Instant,
    period: Duration,
    wait: Pin<Box<tokio::time::Sleep>>,
}

impl ReplicationFeedbackTimer {
    fn new(period: Duration) -> Self {
        let next = tokio::time::Instant::now();
        Self {
            next,
            period,
            wait: Box::pin(tokio::time::sleep_until(next)),
        }
    }

    fn advance(&mut self) {
        // Delay missed ticks instead of replaying a burst of obsolete feedback.
        self.next = tokio::time::Instant::now() + self.period;
        self.wait.as_mut().reset(self.next);
    }
}

async fn next_replication_stream_event<F: Future>(
    feedback_timer: &mut ReplicationFeedbackTimer,
    cancellation: &ReplicationCancellationToken,
    flush_at: Option<tokio::time::Instant>,
    frame: F,
) -> ReplicationStreamEvent<F::Output> {
    if cancellation.is_cancelled() {
        return ReplicationStreamEvent::Cancelled;
    }
    let now = tokio::time::Instant::now();
    if now >= feedback_timer.next {
        feedback_timer.advance();
        return ReplicationStreamEvent::Feedback;
    }
    if flush_at.is_some_and(|deadline| now >= deadline) {
        return ReplicationStreamEvent::Flush;
    }
    let event = tokio::select! {
        biased;
        () = cancellation.cancelled() => ReplicationStreamEvent::Cancelled,
        // A ready WAL backlog must not starve status feedback indefinitely.
        // PostgreSQL's sender timeout applies even while we persist progress.
        // Reuse the timer registration instead of creating/dropping one for
        // every immediately-ready WAL frame.
        () = feedback_timer.wait.as_mut() => ReplicationStreamEvent::Feedback,
        () = async {
            match flush_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        } => ReplicationStreamEvent::Flush,
        frame = frame => ReplicationStreamEvent::Frame(frame),
    };
    if matches!(event, ReplicationStreamEvent::Feedback) {
        feedback_timer.advance();
    }
    event
}

/// Drives the dedicated `public.triples` aggregation stream. Every committed
/// transaction updates sketches and the checkpoint before its LSN is acknowledged
/// to PostgreSQL. Up to 64 transactions / 1 MiB / 50 ms share one persistence
/// group; oversized single transactions retain atomicity. Filtered transactions
/// retain their existing checkpoint budget. Feedback never passes a durable commit.
///
/// # Errors
///
/// Returns on transport, decoding, WAL transformation, or durable persistence
/// failure. No failed batch advances replication feedback.
#[allow(clippy::too_many_arguments)]
pub async fn run_sketch_aggregation_stream(
    connection: &mut Wal2JsonReplicationConnection,
    decoder: &mut ByopWal2JsonReplicationDecoder,
    database: &mut tokio_postgres::Client,
    slot_name: &str,
    process_id: &str,
    checkpoint: &mut Lsn,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
) -> Result<(), SketchAggregatorRuntimeError> {
    run_sketch_aggregation_stream_with_policy(
        connection,
        decoder,
        database,
        slot_name,
        process_id,
        checkpoint,
        cancellation,
        status_interval,
        SketchBatchPolicy::default(),
    )
    .await
}

/// Runs ordered, replay-safe aggregation with an explicitly bounded persistence
/// policy. Larger groups reduce derived-statistics WAL without changing app commits.
#[allow(clippy::too_many_arguments)]
pub async fn run_sketch_aggregation_stream_with_policy(
    connection: &mut Wal2JsonReplicationConnection,
    decoder: &mut ByopWal2JsonReplicationDecoder,
    database: &mut tokio_postgres::Client,
    slot_name: &str,
    process_id: &str,
    checkpoint: &mut Lsn,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    policy: SketchBatchPolicy,
) -> Result<(), SketchAggregatorRuntimeError> {
    if status_interval.is_zero() {
        return Err(SketchAggregatorRuntimeError::InvalidStatusInterval);
    }
    if !policy.valid() {
        return Err(SketchAggregatorRuntimeError::InvalidBatchPolicy);
    }
    let mut feedback_timer = ReplicationFeedbackTimer::new(status_interval);
    feedback_timer.advance();
    let mut pending = Vec::new();
    let mut pending_bytes = 0_usize;
    let mut flush_at = None;
    let mut stalled_replies = StalledReplyGuard::new("aggregator");

    loop {
        match next_replication_stream_event(
            &mut feedback_timer,
            cancellation,
            flush_at,
            connection.recv(cancellation),
        )
        .await
        {
            ReplicationStreamEvent::Cancelled => return Ok(()),
            ReplicationStreamEvent::Feedback => {
                connection
                    .send_feedback(decoder.feedback_state(), false)
                    .await?;
            }
            ReplicationStreamEvent::Flush => {
                persist_pending_sketches(
                    database,
                    slot_name,
                    process_id,
                    checkpoint,
                    &mut pending,
                    decoder,
                )
                .await?;
                pending_bytes = 0;
                flush_at = None;
                connection
                    .send_feedback(decoder.feedback_state(), false)
                    .await?;
            }
            ReplicationStreamEvent::Frame(frame) => {
                let frame = frame?;
                let outcome = decoder.push(&frame, Utc::now())?;
                let reply_requested = outcome.feedback.is_some();
                if let Some(transaction) = outcome.transaction {
                    let buffered_lsn = pending
                        .last()
                        .map_or(*checkpoint, |batch: &SketchDeltaBatch| batch.lsn);
                    if let Some(batch) = aggregate_sketch_transaction(buffered_lsn, &transaction)? {
                        // A large incoming transaction cannot be split, but must
                        // not push an already buffered group over its byte cap.
                        if !pending.is_empty()
                            && pending_bytes.saturating_add(batch.tx_bytes) > policy.max_wal_bytes
                        {
                            persist_pending_sketches(
                                database,
                                slot_name,
                                process_id,
                                checkpoint,
                                &mut pending,
                                decoder,
                            )
                            .await?;
                            pending_bytes = 0;
                            flush_at = None;
                        }
                        if pending.is_empty() {
                            flush_at = Some(tokio::time::Instant::now() + policy.max_wait);
                        }
                        pending_bytes = pending_bytes.saturating_add(batch.tx_bytes);
                        pending.push(batch);
                        if pending.len() >= policy.max_transactions
                            || pending_bytes >= policy.max_wal_bytes
                        {
                            persist_pending_sketches(
                                database,
                                slot_name,
                                process_id,
                                checkpoint,
                                &mut pending,
                                decoder,
                            )
                            .await?;
                            pending_bytes = 0;
                            flush_at = None;
                            connection
                                .send_feedback(decoder.feedback_state(), false)
                                .await?;
                        }
                    } else {
                        // Filtered or replayed boundaries cannot confirm progress
                        // that the restart guard cannot recover from durable state.
                        decoder.acknowledge(*checkpoint);
                    }
                }
                if reply_requested {
                    stalled_replies
                        .answer(connection, decoder.feedback_state())
                        .await?;
                }
            }
        }
    }
}

async fn persist_pending_sketches(
    database: &mut tokio_postgres::Client,
    slot_name: &str,
    process_id: &str,
    checkpoint: &mut Lsn,
    pending: &mut Vec<SketchDeltaBatch>,
    decoder: &mut ByopWal2JsonReplicationDecoder,
) -> Result<(), SketchPersistenceError> {
    let Some(last_lsn) = pending.last().map(|batch| batch.lsn) else {
        return Ok(());
    };
    persist_sketch_delta_batches(database, slot_name, process_id, *checkpoint, pending).await?;
    // Do not change either in-memory restart state or feedback until COMMIT.
    *checkpoint = last_lsn;
    decoder.acknowledge(last_lsn);
    pending.clear();
    Ok(())
}

/// Connects and supervises the singleton native sketch aggregator. Active-slot
/// contention and transient transport failures retry with bounded exponential
/// backoff; decoding and persistence failures remain terminal.
///
/// # Errors
///
/// Returns a permanent connection, worker, configuration, WAL, or persistence
/// error.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_sketch_aggregation_stream(
    connection_string: &str,
    slot_name: &str,
    process_id: &str,
    database: PostgresPool,
    checkpoint: Lsn,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    reconnect: ReconnectPolicy,
) -> Result<(), SketchAggregatorSupervisorError> {
    supervise_sketch_aggregation_stream_with_policy(
        connection_string,
        slot_name,
        process_id,
        database,
        checkpoint,
        cancellation,
        status_interval,
        reconnect,
        SketchBatchPolicy::default(),
    )
    .await
}

/// Supervises aggregation using the validated deployment-specific group budget.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_sketch_aggregation_stream_with_policy(
    connection_string: &str,
    slot_name: &str,
    process_id: &str,
    database: PostgresPool,
    checkpoint: Lsn,
    cancellation: &ReplicationCancellationToken,
    status_interval: Duration,
    reconnect: ReconnectPolicy,
    policy: SketchBatchPolicy,
) -> Result<(), SketchAggregatorSupervisorError> {
    let mut checkpoint = checkpoint;
    let mut decoder = ByopWal2JsonReplicationDecoder::new(checkpoint);
    let mut retry = 0_u32;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let options = LogicalReplicationOptions::new(
            slot_name,
            decoder.feedback_state().flushed(),
            SlotPurpose::Aggregator,
        )?;
        let connection_string = connection_string.to_owned();
        let connected = tokio::task::spawn_blocking(move || {
            Wal2JsonReplicationConnection::connect(&connection_string, &options)
        })
        .await?;
        let mut connection = match connected {
            Ok(connection) => connection,
            Err(error) if retryable_transport(&error) => {
                wait_to_retry(cancellation, reconnect.delay(retry)).await;
                retry = retry.saturating_add(1);
                continue;
            }
            Err(error) => {
                return Err(SketchAggregatorRuntimeError::Transport(error).into());
            }
        };
        retry = 0;
        let mut database_client = database.acquire().await?;
        match run_sketch_aggregation_stream_with_policy(
            &mut connection,
            &mut decoder,
            &mut database_client,
            slot_name,
            process_id,
            &mut checkpoint,
            cancellation,
            status_interval,
            policy,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if retryable_sketch_runtime(&error) => {
                decoder.restart_after_disconnect();
                wait_to_retry(cancellation, reconnect.delay(retry)).await;
                retry = retry.saturating_add(1);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn retryable_sketch_runtime(error: &SketchAggregatorRuntimeError) -> bool {
    match error {
        SketchAggregatorRuntimeError::Transport(error) => retryable_transport(error),
        SketchAggregatorRuntimeError::Persistence(SketchPersistenceError::Postgres(error)) => {
            error.is_closed()
                || error.as_db_error().is_some_and(|error| {
                    let code = error.code().code();
                    code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P03")
                })
        }
        _ => false,
    }
}

fn retryable_transport(error: &ReplicationTransportError) -> bool {
    error.is_transient() || retryable_slot_contention(error) || retryable_postgres_sqlstate(error)
}

/// Stable label for logs; never include the driver message, which can contain
/// connection strings or SQL identifiers.
fn transport_error_kind(error: &ReplicationTransportError) -> &'static str {
    match error {
        ReplicationTransportError::Protocol(_) => "protocol",
        ReplicationTransportError::Buffer(_) => "buffer",
        ReplicationTransportError::TransientConnection(_) => "transient_connection",
        ReplicationTransportError::PermanentConnection(_) => "permanent_connection",
        ReplicationTransportError::ReplicationConnection(_) => "replication_connection",
        ReplicationTransportError::Authentication(_) => "authentication",
        ReplicationTransportError::ReplicationSlot(_) => "replication_slot",
        ReplicationTransportError::Timeout(_) => "timeout",
        ReplicationTransportError::Cancelled(_) => "cancelled",
        ReplicationTransportError::Config(_) => "config",
        ReplicationTransportError::Io(_) => "io",
        ReplicationTransportError::StringConversion(_) => "string_conversion",
        ReplicationTransportError::Generic(_) => "generic",
        ReplicationTransportError::Deserialize(_) => "deserialize",
        ReplicationTransportError::Backend(_) => "backend",
        ReplicationTransportError::StreamStopped(_) => "stream_stopped",
    }
}

fn retryable_slot_contention(error: &ReplicationTransportError) -> bool {
    match error {
        // The native transport reports START_REPLICATION ownership contention
        // as PostgreSQL's object-in-use SQLSTATE.
        ReplicationTransportError::Protocol(message) => message.contains("(SQLSTATE 55006)"),
        // Preserve compatibility with transports that expose a typed slot error.
        ReplicationTransportError::ReplicationSlot(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("is active")
                || message.contains("already active")
                || message.contains("in use")
        }
        _ => false,
    }
}

pub(crate) fn unrecoverable_slot_loss(error: &ReplicationTransportError) -> bool {
    match error {
        ReplicationTransportError::ReplicationSlot(_) => !retryable_slot_contention(error),
        ReplicationTransportError::Protocol(message) => {
            let message_lower = message.to_ascii_lowercase();
            message_lower.contains("replication slot")
                && (message.contains("(SQLSTATE 42704)") || message.contains("(SQLSTATE 55000)"))
        }
        _ => false,
    }
}

const RETRYABLE_SQLSTATE_PREFIXES: [&str; 4] = ["08", "57P01", "57P02", "57P03"];

fn retryable_postgres_sqlstate(error: &ReplicationTransportError) -> bool {
    match error {
        ReplicationTransportError::Protocol(message) => RETRYABLE_SQLSTATE_PREFIXES
            .iter()
            .any(|code| message.contains(&format!("(SQLSTATE {code}"))),
        // The native transport reports any FATAL during connection startup as
        // an authentication or permanent connection error, including
        // PostgreSQL's own "shutting down" / "starting up" (57P03) during a
        // restart. Only trust a SQLSTATE that terminates a FATAL message: a
        // BYOP server's quoted detail elsewhere in the text must not turn a
        // real authentication failure into an endless retry.
        ReplicationTransportError::Authentication(message)
        | ReplicationTransportError::PermanentConnection(message) => {
            message.contains("FATAL:")
                && message
                    .trim_end()
                    .strip_suffix(')')
                    .and_then(|rest| rest.rsplit_once("(SQLSTATE "))
                    .is_some_and(|(_, code)| {
                        code.len() == 5
                            && RETRYABLE_SQLSTATE_PREFIXES
                                .iter()
                                .any(|prefix| code.starts_with(prefix))
                    })
        }
        _ => false,
    }
}

/// PostgreSQL asks a logical replication client for a standby reply about once
/// per half `wal_sender_timeout`. During a shutdown it instead waits for the
/// client to confirm everything it has sent, asking again as soon as each reply
/// arrives. A client that cannot confirm that position (its durable state lives
/// in the database that is shutting down, or it only confirms persisted
/// progress, like the sketch aggregator) would answer forever: the walsender
/// and the shutdown stay open, and both sides spin at full speed. Found by the
/// P3.12 pg-restart drill (19 million replies in two minutes).
///
/// After this many reply requests inside one window without any change in the
/// confirmed flush position, the stream disconnects and reconnects with
/// backoff from its confirmed position, which lets PostgreSQL finish.
const STALLED_REPLY_REQUESTS: u32 = 64;
const STALLED_REPLY_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct StalledReplyGuard {
    stream: &'static str,
    window_start: Option<Instant>,
    flushed: Option<Lsn>,
    count: u32,
}

impl StalledReplyGuard {
    const fn new(stream: &'static str) -> Self {
        Self {
            stream,
            window_start: None,
            flushed: None,
            count: 0,
        }
    }

    /// Answers a server reply request, or disconnects when the server is
    /// spinning on replies this stream cannot satisfy.
    async fn answer(
        &mut self,
        connection: &mut Wal2JsonReplicationConnection,
        feedback: instant_wal::FeedbackState,
    ) -> Result<(), ReplicationTransportError> {
        if self.reply_requested(feedback.flushed(), Instant::now()) {
            return Err(stalled_reply_error(self.stream));
        }
        connection.send_feedback(feedback, false).await
    }

    /// Records one server reply request; true means the stream should
    /// disconnect instead of answering.
    fn reply_requested(&mut self, flushed: Lsn, now: Instant) -> bool {
        let window_expired = self
            .window_start
            .is_none_or(|start| now.saturating_duration_since(start) > STALLED_REPLY_WINDOW);
        if window_expired || self.flushed != Some(flushed) {
            self.window_start = Some(now);
            self.flushed = Some(flushed);
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count >= STALLED_REPLY_REQUESTS
    }
}

fn stalled_reply_error(stream: &str) -> ReplicationTransportError {
    tracing::warn!(
        stream,
        "PostgreSQL keeps requesting standby replies without confirmable progress (likely shutting down); disconnecting so it can finish"
    );
    ReplicationTransportError::TransientConnection(
        "server kept requesting standby replies without confirmable progress".to_owned(),
    )
}

async fn wait_to_retry(cancellation: &ReplicationCancellationToken, delay: Duration) {
    tokio::select! {
        () = cancellation.cancelled() => {},
        () = tokio::time::sleep(delay) => {},
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use futures_util::FutureExt;
    use tokio::sync::Semaphore;
    use tokio_postgres::NoTls;

    use super::*;

    #[tokio::test]
    async fn due_sketch_feedback_precedes_ready_wal_and_cancellation_precedes_both() {
        let cancellation = ReplicationCancellationToken::new();
        let mut timer = ReplicationFeedbackTimer::new(Duration::from_secs(3600));
        // The initial timer tick and incoming data are both immediately ready.
        // A busy stream must service the due feedback before accepting more WAL.
        assert!(matches!(
            next_replication_stream_event(&mut timer, &cancellation, None, std::future::ready(7))
                .await,
            ReplicationStreamEvent::Feedback
        ));
        assert!(matches!(
            next_replication_stream_event(&mut timer, &cancellation, None, std::future::ready(7))
                .await,
            ReplicationStreamEvent::Frame(7)
        ));
        assert!(
            matches!(
                next_replication_stream_event(
                    &mut timer,
                    &cancellation,
                    Some(tokio::time::Instant::now()),
                    std::future::ready(7)
                )
                .await,
                ReplicationStreamEvent::Flush
            ),
            "a due persistence flush must not starve behind a ready WAL backlog"
        );
        let mut due = ReplicationFeedbackTimer::new(Duration::from_secs(3600));
        cancellation.cancel();
        assert!(matches!(
            next_replication_stream_event(&mut due, &cancellation, None, std::future::ready(7))
                .await,
            ReplicationStreamEvent::Cancelled
        ));
    }

    #[tokio::test]
    async fn overdue_feedback_and_flush_do_not_poll_a_ready_wal_backlog() {
        let cancellation = ReplicationCancellationToken::new();
        let mut timer = ReplicationFeedbackTimer::new(Duration::from_secs(3600));
        timer.next = tokio::time::Instant::now() - Duration::from_secs(10);
        let frame = || async { panic!("WAL must not be polled ahead of a due deadline") };
        assert!(matches!(
            next_replication_stream_event(&mut timer, &cancellation, None, frame()).await,
            ReplicationStreamEvent::Feedback
        ));
        assert!(timer.next > tokio::time::Instant::now());
        assert!(matches!(
            next_replication_stream_event(
                &mut timer,
                &cancellation,
                Some(tokio::time::Instant::now()),
                frame()
            )
            .await,
            ReplicationStreamEvent::Flush
        ));
        // A delayed heartbeat schedules just one next tick, not catch-up bursts.
        assert!(matches!(
            next_replication_stream_event(&mut timer, &cancellation, None, std::future::ready(7))
                .await,
            ReplicationStreamEvent::Frame(7)
        ));
    }

    #[tokio::test]
    async fn sketch_aggregator_pool_replaces_a_retryable_database_disconnect() {
        let Some(url) = instant_test_support::test_database_url(
            "sketch_aggregator_pool_replaces_a_retryable_database_disconnect",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let doomed = pool.acquire().await.unwrap();
        let doomed_pid = doomed
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert!(
            setup
                .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        let error = doomed.query_one("SELECT 1", &[]).await.unwrap_err();
        assert!(retryable_sketch_runtime(
            &SketchAggregatorRuntimeError::Persistence(SketchPersistenceError::Postgres(error))
        ));
        drop(doomed);
        let replacement = pool.acquire().await.unwrap();
        assert_ne!(
            replacement
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            doomed_pid
        );
    }

    fn xlog(wal_start: u64, wal_end: u64, value: &serde_json::Value) -> Vec<u8> {
        let mut frame = vec![b'w'];
        frame.extend_from_slice(&wal_start.to_be_bytes());
        frame.extend_from_slice(&wal_end.to_be_bytes());
        frame.extend_from_slice(&0_i64.to_be_bytes());
        frame.extend_from_slice(&serde_json::to_vec(value).unwrap());
        frame
    }

    #[derive(Default)]
    struct BatchTestState {
        calls: Mutex<Vec<Vec<i64>>>,
        staged: Mutex<Vec<i64>>,
        evictions: Mutex<Vec<WalCacheEviction>>,
    }

    struct BatchTestSink {
        state: Arc<BatchTestState>,
        gate: Option<Arc<Semaphore>>,
        fail: bool,
    }

    impl DurableInvalidationSink for BatchTestSink {
        type Error = std::io::Error;

        async fn persist(&mut self, _: &ReplicationPipelineOutcome) -> Result<(), Self::Error> {
            panic!("batch-capable stream must use persist_batch");
        }

        fn max_batch_records(&self) -> usize {
            MAX_DURABLE_BATCH_RECORDS
        }

        async fn evict_cache(&mut self, eviction: &WalCacheEviction) -> Result<(), Self::Error> {
            self.state.evictions.lock().unwrap().push(eviction.clone());
            Ok(())
        }

        async fn persist_batch(
            &mut self,
            outcomes: &[ReplicationPipelineOutcome],
        ) -> Result<(), Self::Error> {
            self.state.calls.lock().unwrap().push(
                outcomes
                    .iter()
                    .map(|outcome| outcome.record.as_ref().unwrap().tx_id)
                    .collect(),
            );
            if let Some(gate) = &self.gate {
                gate.acquire().await.unwrap().forget();
            }
            if self.fail {
                return Err(std::io::Error::other("batch commit failed"));
            }
            Ok(())
        }
    }

    struct BatchTestConsumer {
        state: Arc<BatchTestState>,
        fail_tx: Option<i64>,
    }

    impl RealtimeInvalidationConsumer for BatchTestConsumer {
        fn requires_local_delivery(&self) -> bool {
            false
        }

        async fn stage(&self, outcome: &ReplicationPipelineOutcome) -> Result<(), String> {
            let tx_id = outcome.record.as_ref().unwrap().tx_id;
            if self.fail_tx == Some(tx_id) {
                return Err("staging failed".to_owned());
            }
            self.state.staged.lock().unwrap().push(tx_id);
            Ok(())
        }

        async fn deliver(&self, _: &ReplicationPipelineOutcome) -> Result<(), String> {
            Ok(())
        }
    }

    fn batch_transaction_frames(tx_id: u64) -> Vec<Vec<u8>> {
        let app_id = Uuid::from_u128(42);
        let commit = tx_id * 10 + 3;
        [
            serde_json::json!({"action": "B"}),
            serde_json::json!({"action": "I", "table": "transactions", "columns": [
                {"name": "id", "value": tx_id}, {"name": "app_id", "value": app_id}
            ]}),
            serde_json::json!({"action": "C", "lsn": Lsn::from_u64(commit).to_string(),
                "nextlsn": Lsn::from_u64(commit).to_string()}),
        ]
        .iter()
        .enumerate()
        .map(|(n, entry)| {
            let position = tx_id * 10 + u64::try_from(n).unwrap() + 1;
            xlog(position, position, entry)
        })
        .collect()
    }

    #[tokio::test]
    async fn ready_burst_commits_once_and_stages_every_original_transaction_after_commit() {
        let state = Arc::new(BatchTestState::default());
        let gate = Arc::new(Semaphore::new(0));
        let mut sink = RealtimeInvalidationSink::new(
            BatchTestSink {
                state: Arc::clone(&state),
                gate: Some(Arc::clone(&gate)),
                fail: false,
            },
            BatchTestConsumer {
                state: Arc::clone(&state),
                fail_tx: None,
            },
        );
        let mut frames: VecDeque<_> = (1..=32).flat_map(batch_transaction_frames).collect();
        let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
        let mut store = RealtimeStore::default();
        let cancellation = ReplicationCancellationToken::new();
        {
            let drain = drain_ready_invalidation_frames(
                frames.pop_front().unwrap(),
                || frames.pop_front().map(Ok),
                &mut pipeline,
                &mut store,
                &mut sink,
                &cancellation,
            );
            tokio::pin!(drain);
            assert!(drain.as_mut().now_or_never().is_none());
            assert_eq!(
                *state.calls.lock().unwrap(),
                vec![(1..=32).collect::<Vec<_>>()]
            );
            assert!(state.staged.lock().unwrap().is_empty());
            assert_eq!(
                *state.evictions.lock().unwrap(),
                (1..=32)
                    .map(|tx_id| WalCacheEviction::TransactionMaxSeen { tx_id: Some(tx_id) })
                    .collect::<Vec<_>>()
            );
            gate.add_permits(1);
            assert!(drain.await.unwrap().is_some());
        }
        assert_eq!(*state.staged.lock().unwrap(), (1..=32).collect::<Vec<_>>());
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(323));
    }

    #[tokio::test]
    async fn dropped_or_cancelled_batch_never_acknowledges_an_unfinished_prefix() {
        for cancel in [false, true] {
            let state = Arc::new(BatchTestState::default());
            let gate = Arc::new(Semaphore::new(0));
            let mut sink = RealtimeInvalidationSink::new(
                BatchTestSink {
                    state: Arc::clone(&state),
                    gate: Some(gate),
                    fail: !cancel,
                },
                BatchTestConsumer {
                    state: Arc::clone(&state),
                    fail_tx: None,
                },
            );
            let mut frames: VecDeque<_> = (1..=2).flat_map(batch_transaction_frames).collect();
            let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
            let mut store = RealtimeStore::default();
            let cancellation = ReplicationCancellationToken::new();
            {
                let drain = drain_ready_invalidation_frames(
                    frames.pop_front().unwrap(),
                    || frames.pop_front().map(Ok),
                    &mut pipeline,
                    &mut store,
                    &mut sink,
                    &cancellation,
                );
                tokio::pin!(drain);
                assert!(drain.as_mut().now_or_never().is_none());
                if cancel {
                    cancellation.cancel();
                    assert!(drain.await.unwrap().is_none());
                }
                // The scope drops the owning future while its commit is pending
                // when cancellation was not requested.
            }
            assert_eq!(pipeline.feedback_state().flushed(), Lsn::default());
            assert!(state.staged.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn durable_or_staging_failure_does_not_acknowledge_even_a_successful_stage_prefix() {
        for durable_fails in [true, false] {
            let state = Arc::new(BatchTestState::default());
            let mut sink = RealtimeInvalidationSink::new(
                BatchTestSink {
                    state: Arc::clone(&state),
                    gate: None,
                    fail: durable_fails,
                },
                BatchTestConsumer {
                    state: Arc::clone(&state),
                    fail_tx: Some(2),
                },
            );
            let mut frames: VecDeque<_> = (1..=2).flat_map(batch_transaction_frames).collect();
            let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
            let mut store = RealtimeStore::default();
            let result = drain_ready_invalidation_frames(
                frames.pop_front().unwrap(),
                || frames.pop_front().map(Ok),
                &mut pipeline,
                &mut store,
                &mut sink,
                &ReplicationCancellationToken::new(),
            )
            .await;
            assert!(result.is_err());
            assert_eq!(pipeline.feedback_state().flushed(), Lsn::default());
            assert_eq!(
                *state.staged.lock().unwrap(),
                if durable_fails { vec![] } else { vec![1] }
            );
        }
    }

    #[tokio::test]
    async fn filtered_commits_and_keepalives_fence_ready_application_batches() {
        for keepalive in [false, true] {
            let state = Arc::new(BatchTestState::default());
            let mut sink = BatchTestSink {
                state: Arc::clone(&state),
                gate: None,
                fail: false,
            };
            let mut frames: VecDeque<_> = batch_transaction_frames(1).into();
            if keepalive {
                let mut frame = vec![b'k'];
                frame.extend(30_u64.to_be_bytes());
                frame.extend(0_i64.to_be_bytes());
                frame.push(1);
                frames.push_back(frame);
            } else {
                frames.push_back(xlog(20, 20, &serde_json::json!({"action": "B"})));
                frames.push_back(xlog(
                    23,
                    23,
                    &serde_json::json!({"action": "C", "lsn": "0/17", "nextlsn": "0/17"}),
                ));
            }
            frames.extend(batch_transaction_frames(4));
            let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
            let mut store = RealtimeStore::default();
            let drained = drain_ready_invalidation_frames(
                frames.pop_front().unwrap(),
                || frames.pop_front().map(Ok),
                &mut pipeline,
                &mut store,
                &mut sink,
                &ReplicationCancellationToken::new(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(*state.calls.lock().unwrap(), vec![vec![1]]);
            assert_eq!(
                frames.len(),
                3,
                "next transaction must remain behind the fence"
            );
            assert_eq!(drained.reply_requested, keepalive);
            assert_eq!(
                pipeline.feedback_state().flushed(),
                Lsn::from_u64(if keepalive { 13 } else { 23 })
            );
        }
    }

    #[tokio::test]
    async fn ready_drain_bounds_frames_and_defers_bytes_without_consuming_the_next_frame() {
        for byte_limit in [false, true] {
            let state = Arc::new(BatchTestState::default());
            let mut sink = BatchTestSink {
                state: Arc::clone(&state),
                gate: None,
                fail: false,
            };
            let mut frames: VecDeque<_> = batch_transaction_frames(1).into();
            // Uncommitted row frames must not let the drain grow unbounded while
            // waiting for another complete application record.
            let entry = if byte_limit {
                serde_json::json!({"action": "M", "prefix": "ignored", "content": "x".repeat(MAX_DURABLE_BATCH_BYTES)})
            } else {
                serde_json::json!({"action": "B"})
            };
            let next = xlog(20, 20, &entry);
            let limit = if byte_limit {
                1
            } else {
                MAX_DURABLE_BATCH_FRAMES
            };
            frames.extend((0..limit).map(|_| next.clone()));
            let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
            let mut store = RealtimeStore::default();
            let drained = drain_ready_invalidation_frames(
                frames.pop_front().unwrap(),
                || frames.pop_front().map(Ok),
                &mut pipeline,
                &mut store,
                &mut sink,
                &ReplicationCancellationToken::new(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(*state.calls.lock().unwrap(), vec![vec![1]]);
            assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(13));
            if byte_limit {
                assert_eq!(drained.deferred_frame, Some(next));
                assert_eq!(pipeline.feedback_state().written(), Lsn::from_u64(13));
            } else {
                assert!(drained.deferred_frame.is_none());
                assert_eq!(frames.len(), 3);
            }
        }
    }

    #[tokio::test]
    async fn ready_drain_commits_a_quiet_first_record_without_waiting_for_a_full_batch() {
        let state = Arc::new(BatchTestState::default());
        let mut sink = BatchTestSink {
            state: Arc::clone(&state),
            gate: None,
            fail: false,
        };
        let mut frames: VecDeque<_> = batch_transaction_frames(1).into();
        let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
        let mut store = RealtimeStore::default();
        let cancellation = ReplicationCancellationToken::new();
        let drain = drain_ready_invalidation_frames(
            frames.pop_front().unwrap(),
            || frames.pop_front().map(Ok),
            &mut pipeline,
            &mut store,
            &mut sink,
            &cancellation,
        );
        assert!(drain.now_or_never().unwrap().unwrap().is_some());
        assert_eq!(*state.calls.lock().unwrap(), vec![vec![1]]);
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(13));
    }

    #[tokio::test]
    async fn ready_transport_failure_finishes_decoded_prefix_before_reconnect() {
        let state = Arc::new(BatchTestState::default());
        let mut sink = BatchTestSink {
            state: Arc::clone(&state),
            gate: None,
            fail: false,
        };
        let mut frames: VecDeque<_> = batch_transaction_frames(1).into();
        let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
        let mut store = RealtimeStore::default();
        let result = drain_ready_invalidation_frames(
            frames.pop_front().unwrap(),
            || {
                Some(frames.pop_front().map_or_else(
                    || {
                        Err(ReplicationTransportError::transient_connection(
                            "disconnected",
                        ))
                    },
                    Ok,
                ))
            },
            &mut pipeline,
            &mut store,
            &mut sink,
            &ReplicationCancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(InvalidationRuntimeError::Transport(_))
        ));
        assert_eq!(*state.calls.lock().unwrap(), vec![vec![1]]);
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(13));
        pipeline.restart_after_disconnect();
        let mut last = None;
        for frame in batch_transaction_frames(2) {
            last = Some(pipeline.push(&mut store, &frame, Utc::now()).unwrap());
        }
        assert_eq!(
            last.unwrap().record.unwrap().previous_isn,
            Some(Isn::new(1, Lsn::from_u64(13)))
        );
    }

    struct RecordingDurable(Arc<Mutex<Vec<&'static str>>>);

    impl DurableInvalidationSink for RecordingDurable {
        type Error = Infallible;

        fn persist(
            &mut self,
            _outcome: &ReplicationPipelineOutcome,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.lock().unwrap().push("durable");
            std::future::ready(Ok(()))
        }

        fn evict_cache(
            &mut self,
            _eviction: &WalCacheEviction,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.lock().unwrap().push("cache-sink");
            std::future::ready(Ok(()))
        }

        fn observe_max_seen_isn(
            &mut self,
            _isn: Isn,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.0.lock().unwrap().push("isn-sink");
            std::future::ready(Ok(()))
        }
    }

    struct RecordingRealtime(Arc<Mutex<Vec<&'static str>>>);

    impl RealtimeInvalidationConsumer for RecordingRealtime {
        fn stage(
            &self,
            _outcome: &ReplicationPipelineOutcome,
        ) -> impl Future<Output = Result<(), String>> + Send {
            self.0.lock().unwrap().push("outbox");
            std::future::ready(Ok(()))
        }

        fn deliver(
            &self,
            _outcome: &ReplicationPipelineOutcome,
        ) -> impl Future<Output = Result<(), String>> + Send {
            self.0.lock().unwrap().push("realtime");
            std::future::ready(Ok(()))
        }

        fn deliver_byop(
            &self,
            _app_id: Uuid,
            _record: &ByopWalRecord,
            _topics: &[ByopTopic],
        ) -> impl Future<Output = Result<(), String>> + Send {
            self.0.lock().unwrap().push("byop");
            std::future::ready(Ok(()))
        }

        fn stage_byop(
            &self,
            _app_id: Uuid,
            _record: &ByopWalRecord,
            _topics: &[ByopTopic],
        ) -> impl Future<Output = Result<(), String>> + Send {
            self.0.lock().unwrap().push("byop-outbox");
            std::future::ready(Ok(()))
        }

        fn evict_cache(
            &self,
            _eviction: &WalCacheEviction,
        ) -> impl Future<Output = Result<(), String>> + Send {
            self.0.lock().unwrap().push("cache");
            std::future::ready(Ok(()))
        }

        fn observe_max_seen_isn(
            &self,
            _isn: Isn,
        ) -> impl Future<Output = Result<(), String>> + Send {
            self.0.lock().unwrap().push("isn");
            std::future::ready(Ok(()))
        }
    }

    #[test]
    fn reconnect_delay_is_exponential_and_bounded() {
        let policy = ReconnectPolicy::new(Duration::from_millis(100), Duration::from_secs(1));
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(200));
        assert_eq!(policy.delay(3), Duration::from_millis(800));
        assert_eq!(policy.delay(4), Duration::from_secs(1));
        assert_eq!(policy.delay(u32::MAX), Duration::from_secs(1));
    }

    #[test]
    fn an_active_singleton_slot_is_a_retryable_ownership_race() {
        assert!(retryable_transport(
            &ReplicationTransportError::ReplicationSlot(
                "replication slot is active for PID 42".to_owned()
            )
        ));
        assert!(retryable_transport(&ReplicationTransportError::Protocol(
            "START_REPLICATION failed: replication slot is active (SQLSTATE 55006)".to_owned()
        )));
        assert!(!retryable_transport(
            &ReplicationTransportError::ReplicationSlot(
                "replication slot does not exist".to_owned()
            )
        ));
        assert!(!retryable_transport(&ReplicationTransportError::Protocol(
            "START_REPLICATION failed: replication slot does not exist (SQLSTATE 42704)".to_owned()
        )));
        assert!(!retryable_transport(
            &ReplicationTransportError::Authentication("invalid password".to_owned())
        ));
    }

    #[test]
    fn missing_or_invalidated_slots_are_unrecoverable_but_active_slots_are_not() {
        assert!(unrecoverable_slot_loss(
            &ReplicationTransportError::ReplicationSlot("slot not found".to_owned())
        ));
        assert!(unrecoverable_slot_loss(
            &ReplicationTransportError::Protocol(
                "replication slot does not exist (SQLSTATE 42704)".to_owned()
            )
        ));
        assert!(unrecoverable_slot_loss(
            &ReplicationTransportError::Protocol(
                "can no longer get changes from replication slot (SQLSTATE 55000)".to_owned()
            )
        ));
        assert!(!unrecoverable_slot_loss(
            &ReplicationTransportError::Protocol(
                "replication slot is active (SQLSTATE 55006)".to_owned()
            )
        ));
    }

    #[test]
    fn postgres_disconnect_sqlstates_are_retryable_but_protocol_errors_are_not() {
        for code in ["08006", "57P01", "57P02", "57P03"] {
            assert!(retryable_transport(&ReplicationTransportError::Protocol(
                format!("server error during replication: FATAL: disconnected (SQLSTATE {code})")
            )));
        }
        assert!(!retryable_transport(&ReplicationTransportError::Protocol(
            "server error during replication: ERROR: invalid option (SQLSTATE 22023)".to_owned()
        )));
        assert!(!retryable_transport(
            &ReplicationTransportError::Authentication(
                "quoted server detail (SQLSTATE 57P01)".to_owned()
            )
        ));
    }

    #[test]
    fn stalled_reply_guard_trips_only_on_a_tight_loop_without_flush_progress() {
        let start = Instant::now();
        let lsn = Lsn::from_u64(100);

        // PostgreSQL's shutdown wait: replies requested back to back while the
        // confirmed position cannot move.
        let mut guard = StalledReplyGuard::new("test");
        let tripped = (0..STALLED_REPLY_REQUESTS)
            .map(|i| guard.reply_requested(lsn, start + Duration::from_micros(u64::from(i))))
            .collect::<Vec<_>>();
        assert!(tripped[..tripped.len() - 1].iter().all(|tripped| !tripped));
        assert_eq!(tripped.last(), Some(&true));

        // Normal keepalive cadence on an idle stream never trips.
        let mut guard = StalledReplyGuard::new("test");
        for i in 0..1_000 {
            assert!(!guard.reply_requested(lsn, start + Duration::from_secs(30 * i)));
        }

        // A busy stream that keeps confirming progress never trips.
        let mut guard = StalledReplyGuard::new("test");
        for i in 0..1_000_u64 {
            assert!(
                !guard.reply_requested(Lsn::from_u64(100 + i), start + Duration::from_micros(i))
            );
        }
    }

    #[test]
    fn fatal_startup_errors_during_a_postgres_restart_are_retryable() {
        // Observed in the P3.12 pg-restart drill: the aggregator reconnected
        // while PostgreSQL was shutting down and the whole process exited.
        for message in [
            "Authentication failed: FATAL: the database system is shutting down (SQLSTATE 57P03)",
            "Authentication failed: FATAL: the database system is starting up (SQLSTATE 57P03)",
            "FATAL: terminating connection due to administrator command (SQLSTATE 57P01)",
        ] {
            assert!(retryable_transport(
                &ReplicationTransportError::Authentication(message.to_owned())
            ));
            assert!(retryable_transport(
                &ReplicationTransportError::PermanentConnection(message.to_owned())
            ));
        }
        for message in [
            "Authentication failed: FATAL: password authentication failed for user \"instant\" (SQLSTATE 28P01)",
            "FATAL: role \"x (SQLSTATE 57P03)\" does not exist (SQLSTATE 28000)",
            "FATAL: quoted detail (SQLSTATE 57P03) trailing text",
            "no fatal prefix (SQLSTATE 57P03)",
        ] {
            assert!(!retryable_transport(
                &ReplicationTransportError::Authentication(message.to_owned())
            ));
        }
    }

    struct AtomicDurable {
        machine_id: Uuid,
        fail: bool,
    }

    impl DurableInvalidationSink for AtomicDurable {
        type Error = std::io::Error;
        fn max_batch_records(&self) -> usize {
            MAX_DURABLE_BATCH_RECORDS
        }
        async fn persist(&mut self, _: &ReplicationPipelineOutcome) -> Result<(), Self::Error> {
            if self.fail {
                return Err(std::io::Error::other("commit failed"));
            }
            Ok(())
        }
        fn published_cluster_machine(&self) -> Option<Uuid> {
            Some(self.machine_id)
        }
    }

    struct AtomicConsumer(Arc<Mutex<Vec<Uuid>>>);
    impl RealtimeInvalidationConsumer for AtomicConsumer {
        fn requires_local_delivery(&self) -> bool {
            false
        }
        async fn stage_persisted(
            &self,
            _: &ReplicationPipelineOutcome,
            machine_id: Option<Uuid>,
        ) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(machine_id.expect("publisher receipt forwarded"));
            Ok(())
        }
        async fn deliver(&self, _: &ReplicationPipelineOutcome) -> Result<(), String> {
            panic!("cluster-only pair must not enqueue local delivery");
        }
    }

    #[tokio::test]
    async fn atomic_receipt_crosses_queue_arc_and_pair_only_after_commit() {
        let machine_id = Uuid::new_v4();
        let stages = Arc::new(Mutex::new(Vec::new()));
        let consumer = crate::QueuedRealtimeInvalidationConsumer::new(
            (
                Arc::new(AtomicConsumer(Arc::clone(&stages))),
                Arc::new(AtomicConsumer(Arc::clone(&stages))),
            ),
            Arc::new(crate::DefaultInvalidationQueuePolicy),
        );
        let mut sink = RealtimeInvalidationSink::new(
            AtomicDurable {
                machine_id,
                fail: true,
            },
            consumer,
        );
        assert_eq!(sink.max_batch_records(), MAX_DURABLE_BATCH_RECORDS);
        assert!(
            sink.persist(&ReplicationPipelineOutcome::default())
                .await
                .is_err()
        );
        assert!(stages.lock().unwrap().is_empty());
        sink.durable.fail = false;
        sink.persist(&ReplicationPipelineOutcome::default())
            .await
            .unwrap();
        assert_eq!(*stages.lock().unwrap(), [machine_id, machine_id]);
    }

    #[tokio::test]
    async fn realtime_delivery_runs_after_durable_persistence() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RealtimeInvalidationSink::new(
            RecordingDurable(Arc::clone(&events)),
            RecordingRealtime(Arc::clone(&events)),
        );

        sink.persist(&ReplicationPipelineOutcome::default())
            .await
            .unwrap();

        assert_eq!(*events.lock().unwrap(), ["durable", "outbox", "realtime"]);
    }

    #[tokio::test]
    async fn cache_only_outcome_is_delivered_before_its_commit_is_acknowledged() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RealtimeInvalidationSink::new(
            RecordingDurable(Arc::clone(&events)),
            RecordingRealtime(Arc::clone(&events)),
        );
        let mut pipeline = WalInvalidationPipeline::new(2, None, Lsn::default());
        let mut store = RealtimeStore::default();
        let now = Utc::now();
        pipeline
            .push(
                &mut store,
                &xlog(1, 1, &serde_json::json!({"action": "B"})),
                now,
            )
            .unwrap();
        let eviction = pipeline
            .push(
                &mut store,
                &xlog(
                    2,
                    2,
                    &serde_json::json!({
                        "action": "U",
                        "table": "rules",
                        "identity": [{"name": "app_id", "value": Uuid::new_v4()}]
                    }),
                ),
                now,
            )
            .unwrap();

        deliver_invalidation_outcome(&mut sink, &eviction)
            .await
            .unwrap();
        assert_eq!(*events.lock().unwrap(), ["cache-sink", "cache"]);
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::default());

        let close = pipeline
            .push(
                &mut store,
                &xlog(
                    3,
                    3,
                    &serde_json::json!({"action": "C", "lsn": "0/2", "nextlsn": "0/3"}),
                ),
                now,
            )
            .unwrap();
        deliver_invalidation_outcome(&mut sink, &close)
            .await
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["cache-sink", "cache", "isn-sink", "isn"]
        );
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::default());
        pipeline.acknowledge(close.durable_lsn.unwrap());
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(3));
    }

    #[test]
    fn byop_decoder_retains_arbitrary_tables_until_explicit_acknowledgement() {
        fn xlog(wal_start: u64, wal_end: u64, value: &serde_json::Value) -> Vec<u8> {
            let mut frame = vec![b'w'];
            frame.extend_from_slice(&wal_start.to_be_bytes());
            frame.extend_from_slice(&wal_end.to_be_bytes());
            frame.extend_from_slice(&0_i64.to_be_bytes());
            frame.extend_from_slice(&serde_json::to_vec(value).unwrap());
            frame
        }

        let app_id = Uuid::new_v4();
        let mut pipeline =
            ByopInvalidationPipeline::new(app_id, ByopSchema::default(), Lsn::from_u64(5));
        let now = Utc::now();
        for (index, value) in [
            serde_json::json!({"action": "B"}),
            serde_json::json!({
                "action": "I",
                "table": "customer_posts",
                "columns": [{"name": "slug", "value": "native-rust"}]
            }),
        ]
        .iter()
        .enumerate()
        {
            assert!(
                pipeline
                    .push(&xlog(10 + index as u64, 20 + index as u64, value), now)
                    .unwrap()
                    .0
                    .is_none()
            );
        }
        let record = pipeline
            .push(
                &xlog(
                    12,
                    22,
                    &serde_json::json!({"action": "C", "lsn": "0/63", "nextlsn": "0/64"}),
                ),
                now,
            )
            .unwrap()
            .0
            .unwrap();
        assert_eq!(pipeline.app_id, app_id);
        assert_eq!(record.changes[0].table.as_deref(), Some("customer_posts"));
        assert_eq!(
            pipeline.decoder.feedback_state().flushed(),
            Lsn::from_u64(5)
        );
        pipeline.acknowledge(record.next_lsn);
        assert_eq!(
            pipeline.decoder.feedback_state().flushed(),
            Lsn::from_u64(22)
        );
    }

    #[tokio::test]
    async fn byop_delivery_uses_the_realtime_side_without_wal_history_persistence() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RealtimeInvalidationSink::new(
            RecordingDurable(Arc::clone(&events)),
            RecordingRealtime(Arc::clone(&events)),
        );
        let record = ByopWalRecord {
            tx_id: 10,
            next_lsn: Lsn::from_u64(10),
            changes: Vec::new(),
        };

        sink.persist_byop(Uuid::new_v4(), &record, &[])
            .await
            .unwrap();

        assert_eq!(*events.lock().unwrap(), ["byop-outbox", "byop"]);
    }

    struct RecordingHealth(Arc<Mutex<Vec<bool>>>);
    impl InvalidationHealthObserver for RecordingHealth {
        fn set_healthy(&self, healthy: bool) {
            self.0.lock().unwrap().push(healthy);
        }
    }

    #[test]
    fn up_to_three_restarts_in_the_window_clear_health_on_reconnect() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observer = RecordingHealth(Arc::clone(&events));
        let mut transitions = InvalidationHealthTransitions::default();
        let start = Instant::now();
        for offset in [0, 1, 2] {
            observe_stream_disconnect(&mut transitions, &observer);
            observe_restart(
                &mut transitions,
                &observer,
                "slot",
                start + Duration::from_secs(offset),
                0,
            );
        }
        assert_eq!(
            *events.lock().unwrap(),
            [false, true, false, true, false, true]
        );
    }

    #[test]
    fn flapping_health_clears_on_its_own_once_the_burst_ages_out() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observer = RecordingHealth(Arc::clone(&events));
        let mut transitions = InvalidationHealthTransitions::default();
        let start = Instant::now();
        for offset in [0, 1, 2, 3] {
            observe_stream_disconnect(&mut transitions, &observer);
            observe_restart(
                &mut transitions,
                &observer,
                "slot",
                start + Duration::from_secs(offset),
                0,
            );
        }
        assert!(transitions.flapping_at(start + Duration::from_secs(3)));
        assert_eq!(
            *events.lock().unwrap(),
            [false, true, false, true, false, true, false]
        );

        // Still inside the window: the periodic evaluation keeps it withheld.
        observe_health_tick(
            &mut transitions,
            &observer,
            "slot",
            start + STREAM_RESTART_WINDOW.saturating_sub(Duration::from_secs(1)),
        );
        assert_eq!(events.lock().unwrap().last(), Some(&false));
        assert_eq!(events.lock().unwrap().len(), 7);

        // No reconnect is needed: once the oldest restart ages out, the
        // periodic evaluation alone clears health.
        let aged_out = start + STREAM_RESTART_WINDOW;
        assert_eq!(transitions.restarts_in_window(aged_out), 3);
        observe_health_tick(&mut transitions, &observer, "slot", aged_out);
        assert_eq!(events.lock().unwrap().last(), Some(&true));
        assert_eq!(events.lock().unwrap().len(), 8);
    }

    #[test]
    fn restarts_that_keep_coming_keep_health_withheld() {
        let mut transitions = InvalidationHealthTransitions::default();
        let start = Instant::now();
        for minute in 0..30 {
            transitions.record_restart(start + Duration::from_mins(minute));
        }
        // The last four restarts are at minutes 26..=29; health returns only
        // when the one at minute 26 leaves the ten-minute window.
        assert!(!transitions.healthy_at(start + Duration::from_mins(29)));
        assert!(!transitions.healthy_at(start + Duration::from_secs(36 * 60 - 1)));
        assert!(transitions.healthy_at(start + Duration::from_mins(36)));
    }

    #[tokio::test]
    async fn a_disconnect_always_withholds_health_and_shutdown_publishes_it() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observer = RecordingHealth(Arc::clone(&events));
        let mut transitions = InvalidationHealthTransitions::default();
        let start = Instant::now();
        assert!(transitions.healthy_at(start));
        observe_stream_disconnect(&mut transitions, &observer);
        assert!(!transitions.disconnected());
        assert_eq!(*events.lock().unwrap(), [false]);
        assert!(transitions.graceful_shutdown());
        observer.graceful_shutdown().await;
        assert_eq!(*events.lock().unwrap(), [false, true]);
    }

    #[test]
    fn connect_stage_recovery_does_not_count_as_a_producer_restart() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observer = RecordingHealth(Arc::clone(&events));
        let mut transitions = InvalidationHealthTransitions::default();
        let start = Instant::now();
        observe_stream_disconnect(&mut transitions, &observer);
        observe_connect_stage_recovery(&mut transitions, &observer, "slot", 0);
        assert_eq!(transitions.restarts_in_window(start), 0);
        assert!(!transitions.flapping_at(start));
        assert!(transitions.healthy_at(start));
        assert_eq!(*events.lock().unwrap(), [false, true]);
    }
}
