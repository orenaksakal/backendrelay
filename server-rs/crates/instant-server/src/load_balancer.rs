use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tokio::task::{Id as TaskId, JoinError, JoinSet};
use tokio_postgres::Client;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool};

pub const DEFAULT_DEREGISTER_TARGETS_DRAIN: Duration = Duration::from_secs(270);
pub const DEREGISTER_TARGETS_DUPLICATE_WINDOW: Duration = Duration::from_secs(120);
pub const DEREGISTER_TARGETS_MAX_GAP: Duration = Duration::from_secs(1);

pub type LoadBalancerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ConnectionDrainFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadBalancerMessage {
    pub body: String,
    pub receipt_handle: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum LoadBalancerDrainError {
    #[error("load-balancer event transport failed: {0}")]
    Transport(String),
    #[error("load-balancer drain policy failed: {0}")]
    Policy(String),
    #[error("load-balancer event body is not valid JSON: {0}")]
    InvalidEvent(String),
}

pub trait LoadBalancerEventSource: Send + Sync + 'static {
    fn subscribe(
        &self,
    ) -> LoadBalancerFuture<
        '_,
        Result<Box<dyn LoadBalancerEventSubscription>, LoadBalancerDrainError>,
    >;
}

pub trait LoadBalancerEventSubscription: Send + 'static {
    fn receive(
        &mut self,
    ) -> LoadBalancerFuture<'_, Result<Vec<LoadBalancerMessage>, LoadBalancerDrainError>>;

    fn acknowledge(
        &mut self,
        receipt_handle: &str,
    ) -> LoadBalancerFuture<'_, Result<(), LoadBalancerDrainError>>;

    /// Removes the process-owned subscription and queue. Implementations must
    /// attempt every cleanup step even when an earlier step fails.
    fn shutdown(&mut self) -> LoadBalancerFuture<'_, Result<(), LoadBalancerDrainError>>;
}

pub trait DeregisterTargetsDrainPolicy: Send + Sync + 'static {
    fn drain_duration(&self) -> LoadBalancerFuture<'_, Result<Duration, LoadBalancerDrainError>>;
}

pub trait ConnectionDrainer: Send + Sync + 'static {
    fn drain(&self, total: Duration, max_gap: Duration) -> ConnectionDrainFuture;
}

/// Reads the live config-app flag for every accepted deregistration event.
pub struct PostgresDeregisterTargetsDrainPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresDeregisterTargetsDrainPolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled_read_only(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }
}

impl DeregisterTargetsDrainPolicy for PostgresDeregisterTargetsDrainPolicy {
    fn drain_duration(&self) -> LoadBalancerFuture<'_, Result<Duration, LoadBalancerDrainError>> {
        Box::pin(async move {
            let client = self
                .client
                .acquire()
                .await
                .map_err(|error| LoadBalancerDrainError::Policy(error.to_string()))?;
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
                       AND flag_setting.value = to_jsonb($2::text)
                     LIMIT 1",
                    &[&self.config_app_id, &"deregister-targets-drain-ms"],
                )
                .await
                .map_err(|error| LoadBalancerDrainError::Policy(error.to_string()))?
                .map(|row| row.get::<_, Value>(0));
            deregister_targets_drain_duration(value.as_ref())
        })
    }
}

fn deregister_targets_drain_duration(
    value: Option<&Value>,
) -> Result<Duration, LoadBalancerDrainError> {
    value.map_or(Ok(DEFAULT_DEREGISTER_TARGETS_DRAIN), |value| {
        value.as_u64().map(Duration::from_millis).ok_or_else(|| {
            LoadBalancerDrainError::Policy(
                "config flag `deregister-targets-drain-ms` must be a non-negative integer"
                    .to_owned(),
            )
        })
    })
}

#[must_use]
pub fn is_deregister_targets_for_instance(event: &Value, instance_id: &str) -> bool {
    event.pointer("/detail/eventName").and_then(Value::as_str) == Some("DeregisterTargets")
        && event
            .pointer("/detail/requestParameters/targets")
            .and_then(Value::as_array)
            .is_some_and(|targets| {
                targets
                    .iter()
                    .any(|target| target.get("id").and_then(Value::as_str) == Some(instance_id))
            })
}

#[derive(Default)]
struct DuplicateDrainGuard {
    active_started_at: HashMap<TaskId, Instant>,
}

impl DuplicateDrainGuard {
    fn is_duplicate(&self, now: Instant) -> bool {
        self.active_started_at.values().any(|started_at| {
            now.saturating_duration_since(*started_at) < DEREGISTER_TARGETS_DUPLICATE_WINDOW
        })
    }

    fn record_start(&mut self, task_id: TaskId, now: Instant) {
        self.active_started_at.insert(task_id, now);
    }

    fn record_completion(&mut self, task_id: TaskId) {
        self.active_started_at.remove(&task_id);
    }
}

fn record_finished_drain(
    duplicate_guard: &mut DuplicateDrainGuard,
    result: Result<(TaskId, ()), JoinError>,
) {
    match result {
        Ok((task_id, ())) => duplicate_guard.record_completion(task_id),
        Err(error) => {
            duplicate_guard.record_completion(error.id());
            tracing::warn!(error = %error, "load-balancer drain task failed");
        }
    }
}

fn reap_finished_drains(duplicate_guard: &mut DuplicateDrainGuard, drains: &mut JoinSet<()>) {
    while let Some(result) = drains.try_join_next_with_id() {
        record_finished_drain(duplicate_guard, result);
    }
}

pub struct LoadBalancerDrainListener {
    instance_id: Arc<str>,
    source: Arc<dyn LoadBalancerEventSource>,
    policy: Arc<dyn DeregisterTargetsDrainPolicy>,
    drainer: Arc<dyn ConnectionDrainer>,
}

impl LoadBalancerDrainListener {
    #[must_use]
    pub fn new(
        instance_id: impl Into<Arc<str>>,
        source: Arc<dyn LoadBalancerEventSource>,
        policy: Arc<dyn DeregisterTargetsDrainPolicy>,
        drainer: Arc<dyn ConnectionDrainer>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            source,
            policy,
            drainer,
        }
    }

    /// Polls until cancellation, then aborts every in-flight paced drain and
    /// waits for the transport to remove its process-owned AWS resources.
    ///
    /// # Errors
    ///
    /// Returns an error when initial transport provisioning fails. Poll,
    /// message, and cleanup errors are recorded and retried or ignored like
    /// the hosted JVM listener.
    pub async fn run(&self, cancellation: CancellationToken) -> Result<(), LoadBalancerDrainError> {
        let mut subscription = self.source.subscribe().await?;
        let mut duplicate_guard = DuplicateDrainGuard::default();
        let mut drains = JoinSet::new();
        let mut backoff_index = 0;

        loop {
            reap_finished_drains(&mut duplicate_guard, &mut drains);
            let messages = tokio::select! {
                () = cancellation.cancelled() => break,
                result = subscription.receive() => result,
            };
            match messages {
                Ok(messages) => {
                    let mut batch_failed = false;
                    for message in messages {
                        let result = tokio::select! {
                            () = cancellation.cancelled() => break,
                            result = self.handle_message(
                                &message,
                                &mut duplicate_guard,
                                &mut drains,
                                &mut *subscription,
                            ) => result,
                        };
                        if let Err(error) = result {
                            batch_failed = true;
                            tracing::warn!(error = %error, "load-balancer message handling failed");
                        }
                    }
                    if batch_failed {
                        if cancellable_backoff(backoff_index, &cancellation).await {
                            break;
                        }
                        backoff_index = backoff_index.saturating_add(1);
                    } else {
                        backoff_index = 0;
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "load-balancer event receive failed");
                    if cancellable_backoff(backoff_index, &cancellation).await {
                        break;
                    }
                    backoff_index = backoff_index.saturating_add(1);
                }
            }
        }

        drains.abort_all();
        while drains.join_next().await.is_some() {}
        if let Err(error) = subscription.shutdown().await {
            tracing::warn!(error = %error, "load-balancer event subscription cleanup failed");
        }
        Ok(())
    }

    async fn handle_message(
        &self,
        message: &LoadBalancerMessage,
        duplicate_guard: &mut DuplicateDrainGuard,
        drains: &mut JoinSet<()>,
        subscription: &mut dyn LoadBalancerEventSubscription,
    ) -> Result<(), LoadBalancerDrainError> {
        reap_finished_drains(duplicate_guard, drains);
        let event = serde_json::from_str::<Value>(&message.body)
            .map_err(|error| LoadBalancerDrainError::InvalidEvent(error.to_string()))?;
        if is_deregister_targets_for_instance(&event, &self.instance_id) {
            let now = Instant::now();
            if !duplicate_guard.is_duplicate(now) {
                let duration = self.policy.drain_duration().await?;
                let task = drains.spawn(self.drainer.drain(duration, DEREGISTER_TARGETS_MAX_GAP));
                duplicate_guard.record_start(task.id(), now);
            }
        }
        subscription.acknowledge(&message.receipt_handle).await
    }
}

async fn cancellable_backoff(index: usize, cancellation: &CancellationToken) -> bool {
    const BACKOFFS: [Duration; 4] = [
        Duration::from_secs(2),
        Duration::from_secs(10),
        Duration::from_secs(30),
        Duration::from_secs(60),
    ];
    let delay = BACKOFFS
        .get(index)
        .copied()
        .unwrap_or(Duration::from_secs(60));
    tokio::select! {
        () = cancellation.cancelled() => true,
        () = tokio::time::sleep(delay) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    #[test]
    fn deregistration_filter_requires_event_name_and_exact_instance_id() {
        let event = json!({
            "detail": {
                "eventName": "DeregisterTargets",
                "requestParameters": {
                    "targets": [
                        {"id": null, "port": 8888},
                        {"id": "i-other", "port": 8888},
                        {"id": "i-current", "port": 8888}
                    ]
                }
            }
        });
        assert!(is_deregister_targets_for_instance(&event, "i-current"));
        assert!(!is_deregister_targets_for_instance(&event, "i-missing"));

        let mut wrong_event = event.clone();
        wrong_event["detail"]["eventName"] = json!("RegisterTargets");
        assert!(!is_deregister_targets_for_instance(
            &wrong_event,
            "i-current"
        ));
        assert!(!is_deregister_targets_for_instance(
            &json!({"detail": {"eventName": "DeregisterTargets"}}),
            "i-current"
        ));
    }

    #[test]
    fn drain_duration_preserves_default_override_zero_and_validation() {
        assert_eq!(
            deregister_targets_drain_duration(None).unwrap(),
            Duration::from_secs(270)
        );
        assert_eq!(
            deregister_targets_drain_duration(Some(&json!(125_000))).unwrap(),
            Duration::from_secs(125)
        );
        assert_eq!(
            deregister_targets_drain_duration(Some(&json!(0))).unwrap(),
            Duration::ZERO
        );
        let overflow = serde_json::from_str("18446744073709551616").unwrap();
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("270000"),
            Value::Null,
            overflow,
        ] {
            let error = deregister_targets_drain_duration(Some(&invalid)).unwrap_err();
            assert!(error.to_string().contains("deregister-targets-drain-ms"));
        }
    }

    #[tokio::test]
    async fn duplicate_guard_suppresses_only_active_drains_started_in_the_prior_120_seconds() {
        let start = Instant::now();
        let mut guard = DuplicateDrainGuard::default();
        let mut drains = JoinSet::new();
        assert!(!guard.is_duplicate(start));
        let first = drains.spawn(std::future::pending());
        guard.record_start(first.id(), start);
        assert!(guard.is_duplicate(start + Duration::from_secs(119)));
        assert!(!guard.is_duplicate(start + Duration::from_secs(120)));
        let second = drains.spawn(std::future::pending());
        guard.record_start(second.id(), start + Duration::from_secs(120));
        assert!(guard.is_duplicate(start + Duration::from_secs(239)));
        assert!(!guard.is_duplicate(start + Duration::from_secs(240)));

        drains.abort_all();
        while let Some(result) = drains.join_next_with_id().await {
            record_finished_drain(&mut guard, result);
        }
        assert!(guard.active_started_at.is_empty());
    }

    struct FixedPolicy;

    impl DeregisterTargetsDrainPolicy for FixedPolicy {
        fn drain_duration(
            &self,
        ) -> LoadBalancerFuture<'_, Result<Duration, LoadBalancerDrainError>> {
            Box::pin(std::future::ready(Ok(Duration::from_millis(321))))
        }
    }

    struct FailingPolicy;

    impl DeregisterTargetsDrainPolicy for FailingPolicy {
        fn drain_duration(
            &self,
        ) -> LoadBalancerFuture<'_, Result<Duration, LoadBalancerDrainError>> {
            Box::pin(std::future::ready(Err(LoadBalancerDrainError::Policy(
                "invalid live flag".to_owned(),
            ))))
        }
    }

    struct ZeroPolicy;

    impl DeregisterTargetsDrainPolicy for ZeroPolicy {
        fn drain_duration(
            &self,
        ) -> LoadBalancerFuture<'_, Result<Duration, LoadBalancerDrainError>> {
            Box::pin(std::future::ready(Ok(Duration::ZERO)))
        }
    }

    struct FakeSource {
        messages: Mutex<Option<Vec<LoadBalancerMessage>>>,
        acknowledged: Arc<Mutex<Vec<String>>>,
        cleaned: Arc<AtomicBool>,
    }

    impl LoadBalancerEventSource for FakeSource {
        fn subscribe(
            &self,
        ) -> LoadBalancerFuture<
            '_,
            Result<Box<dyn LoadBalancerEventSubscription>, LoadBalancerDrainError>,
        > {
            let messages = self.messages.lock().unwrap().take().unwrap_or_default();
            let subscription = FakeSubscription {
                messages: VecDeque::from([messages]),
                acknowledged: Arc::clone(&self.acknowledged),
                cleaned: Arc::clone(&self.cleaned),
                wake: Arc::new(Notify::new()),
            };
            let subscription: Box<dyn LoadBalancerEventSubscription> = Box::new(subscription);
            Box::pin(std::future::ready(Ok(subscription)))
        }
    }

    struct FakeSubscription {
        messages: VecDeque<Vec<LoadBalancerMessage>>,
        acknowledged: Arc<Mutex<Vec<String>>>,
        cleaned: Arc<AtomicBool>,
        wake: Arc<Notify>,
    }

    impl LoadBalancerEventSubscription for FakeSubscription {
        fn receive(
            &mut self,
        ) -> LoadBalancerFuture<'_, Result<Vec<LoadBalancerMessage>, LoadBalancerDrainError>>
        {
            if let Some(messages) = self.messages.pop_front() {
                return Box::pin(std::future::ready(Ok(messages)));
            }
            let wake = Arc::clone(&self.wake);
            Box::pin(async move {
                wake.notified().await;
                Ok(Vec::new())
            })
        }

        fn acknowledge(
            &mut self,
            receipt_handle: &str,
        ) -> LoadBalancerFuture<'_, Result<(), LoadBalancerDrainError>> {
            self.acknowledged
                .lock()
                .unwrap()
                .push(receipt_handle.to_owned());
            Box::pin(std::future::ready(Ok(())))
        }

        fn shutdown(&mut self) -> LoadBalancerFuture<'_, Result<(), LoadBalancerDrainError>> {
            self.cleaned.store(true, Ordering::SeqCst);
            Box::pin(std::future::ready(Ok(())))
        }
    }

    struct PendingDrainer {
        calls: Arc<Mutex<Vec<(Duration, Duration)>>>,
        started: Arc<Notify>,
        dropped: Arc<AtomicBool>,
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl ConnectionDrainer for PendingDrainer {
        fn drain(&self, total: Duration, max_gap: Duration) -> ConnectionDrainFuture {
            self.calls.lock().unwrap().push((total, max_gap));
            let started = Arc::clone(&self.started);
            let dropped = Arc::clone(&self.dropped);
            Box::pin(async move {
                let _drop = DropFlag(dropped);
                started.notify_one();
                std::future::pending::<()>().await;
            })
        }
    }

    struct ImmediateDrainer {
        calls: Arc<Mutex<Vec<(Duration, Duration)>>>,
    }

    impl ConnectionDrainer for ImmediateDrainer {
        fn drain(&self, total: Duration, max_gap: Duration) -> ConnectionDrainFuture {
            self.calls.lock().unwrap().push((total, max_gap));
            Box::pin(std::future::ready(()))
        }
    }

    #[tokio::test]
    async fn finished_zero_drain_permits_another_event_inside_the_duplicate_window() {
        let acknowledged = Arc::new(Mutex::new(Vec::new()));
        let mut subscription = FakeSubscription {
            messages: VecDeque::new(),
            acknowledged: Arc::clone(&acknowledged),
            cleaned: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = LoadBalancerDrainListener::new(
            "i-current",
            Arc::new(FakeSource {
                messages: Mutex::new(None),
                acknowledged: Arc::clone(&acknowledged),
                cleaned: Arc::new(AtomicBool::new(false)),
            }),
            Arc::new(ZeroPolicy),
            Arc::new(ImmediateDrainer {
                calls: Arc::clone(&calls),
            }),
        );
        let message = |receipt_handle: &str| LoadBalancerMessage {
            body: json!({
                "detail": {
                    "eventName": "DeregisterTargets",
                    "requestParameters": {"targets": [{"id": "i-current"}]}
                }
            })
            .to_string(),
            receipt_handle: receipt_handle.to_owned(),
        };
        let mut guard = DuplicateDrainGuard::default();
        let mut drains = JoinSet::new();

        listener
            .handle_message(
                &message("first"),
                &mut guard,
                &mut drains,
                &mut subscription,
            )
            .await
            .unwrap();
        let result = drains
            .join_next_with_id()
            .await
            .expect("zero-duration drain was tracked through completion");
        record_finished_drain(&mut guard, result);
        assert!(!guard.is_duplicate(Instant::now()));

        listener
            .handle_message(
                &message("second"),
                &mut guard,
                &mut drains,
                &mut subscription,
            )
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (Duration::ZERO, DEREGISTER_TARGETS_MAX_GAP),
                (Duration::ZERO, DEREGISTER_TARGETS_MAX_GAP)
            ]
        );
        assert_eq!(
            *acknowledged.lock().unwrap(),
            vec!["first".to_owned(), "second".to_owned()]
        );

        drains.abort_all();
        while let Some(result) = drains.join_next_with_id().await {
            record_finished_drain(&mut guard, result);
        }
    }

    #[tokio::test]
    async fn listener_filters_deduplicates_acknowledges_and_cleans_up_on_cancellation() {
        let matching = |receipt_handle: &str| LoadBalancerMessage {
            body: json!({
                "detail": {
                    "eventName": "DeregisterTargets",
                    "requestParameters": {"targets": [{"id": "i-current"}]}
                }
            })
            .to_string(),
            receipt_handle: receipt_handle.to_owned(),
        };
        let wrong_instance = LoadBalancerMessage {
            body: json!({
                "detail": {
                    "eventName": "DeregisterTargets",
                    "requestParameters": {"targets": [{"id": "i-other"}]}
                }
            })
            .to_string(),
            receipt_handle: "wrong".to_owned(),
        };
        let acknowledged = Arc::new(Mutex::new(Vec::new()));
        let cleaned = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakeSource {
            messages: Mutex::new(Some(vec![
                matching("first"),
                matching("duplicate"),
                wrong_instance,
            ])),
            acknowledged: Arc::clone(&acknowledged),
            cleaned: Arc::clone(&cleaned),
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let drainer = Arc::new(PendingDrainer {
            calls: Arc::clone(&calls),
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        });
        let listener = Arc::new(LoadBalancerDrainListener::new(
            "i-current",
            source,
            Arc::new(FixedPolicy),
            drainer,
        ));
        let cancellation = CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { listener.run(run_cancellation).await });
        started.notified().await;
        tokio::task::yield_now().await;

        assert_eq!(
            *calls.lock().unwrap(),
            vec![(Duration::from_millis(321), Duration::from_secs(1))]
        );
        assert_eq!(
            *acknowledged.lock().unwrap(),
            vec!["first", "duplicate", "wrong"]
        );

        cancellation.cancel();
        task.await.unwrap().unwrap();
        assert!(cleaned.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn policy_failure_leaves_the_sqs_receipt_unacknowledged() {
        let acknowledged = Arc::new(Mutex::new(Vec::new()));
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut subscription = FakeSubscription {
            messages: VecDeque::new(),
            acknowledged: Arc::clone(&acknowledged),
            cleaned,
            wake: Arc::new(Notify::new()),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = LoadBalancerDrainListener::new(
            "i-current",
            Arc::new(FakeSource {
                messages: Mutex::new(None),
                acknowledged: Arc::clone(&acknowledged),
                cleaned: Arc::new(AtomicBool::new(false)),
            }),
            Arc::new(FailingPolicy),
            Arc::new(PendingDrainer {
                calls: Arc::clone(&calls),
                started: Arc::new(Notify::new()),
                dropped: Arc::new(AtomicBool::new(false)),
            }),
        );
        let message = LoadBalancerMessage {
            body: json!({
                "detail": {
                    "eventName": "DeregisterTargets",
                    "requestParameters": {"targets": [{"id": "i-current"}]}
                }
            })
            .to_string(),
            receipt_handle: "retry-me".to_owned(),
        };
        let mut guard = DuplicateDrainGuard::default();
        let mut drains = JoinSet::new();

        let error = listener
            .handle_message(&message, &mut guard, &mut drains, &mut subscription)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            LoadBalancerDrainError::Policy("invalid live flag".to_owned())
        );
        assert!(acknowledged.lock().unwrap().is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(!guard.is_duplicate(Instant::now()));
    }

    #[test]
    fn listener_error_type_is_sendable() {
        fn assert_send<T: Send>() {}
        assert_send::<LoadBalancerDrainError>();
    }
}
