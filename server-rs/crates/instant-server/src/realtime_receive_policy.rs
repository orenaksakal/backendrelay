//! Dynamic receive-executor rollout and gauges pinned to upstream
//! `flags.clj:506-514`, `reactive/session.clj:58-61,1558-1571`, and
//! `grouped_queue.clj:181-200` at commit `4e0a1bd8`.

use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::realtime_receive_dispatch::{
    GroupedReceiveDispatcher, GroupedReceiveMetrics, ReceiveExecutorClass,
};
use crate::{GaugeProvider, INSTANT_CONFIG_APP_ID, PostgresPool, ServerEnvironment};

const MORE_VFUTURES_FLAG: &str = "more-vfutures-instances";
const RECEIVE_GAUGE_PATH: &str = "instant.reactive.session.receive-q";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeReceiveExecutorFallback {
    Database,
    MalformedFlag,
}

impl RealtimeReceiveExecutorFallback {
    const fn encoded(self) -> u8 {
        match self {
            Self::Database => 1,
            Self::MalformedFlag => 2,
        }
    }

    fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Database),
            2 => Some(Self::MalformedFlag),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealtimeReceiveExecutorSelection {
    pub class: ReceiveExecutorClass,
    pub worker_limit: usize,
    pub fallback: Option<RealtimeReceiveExecutorFallback>,
}

impl RealtimeReceiveExecutorSelection {
    #[must_use]
    pub fn for_class(class: ReceiveExecutorClass, logical_cpus: NonZeroUsize) -> Self {
        Self {
            class,
            worker_limit: class.worker_limit(logical_cpus.get()),
            fallback: None,
        }
    }

    fn fail_closed(logical_cpus: NonZeroUsize, fallback: RealtimeReceiveExecutorFallback) -> Self {
        Self {
            fallback: Some(fallback),
            ..Self::for_class(ReceiveExecutorClass::Standard, logical_cpus)
        }
    }
}

pub trait RealtimeReceiveExecutorPolicy: Send + Sync + 'static {
    /// Returns the locally cached executor decision without I/O or yielding.
    fn current_selection(&self) -> RealtimeReceiveExecutorSelection;

    /// Refreshes the cached decision outside message ingress.
    fn refresh<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = RealtimeReceiveExecutorSelection> + Send + 'a>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticRealtimeReceiveExecutorPolicy {
    selection: RealtimeReceiveExecutorSelection,
}

impl StaticRealtimeReceiveExecutorPolicy {
    #[must_use]
    pub const fn new(selection: RealtimeReceiveExecutorSelection) -> Self {
        Self { selection }
    }

    #[must_use]
    pub fn for_environment(environment: ServerEnvironment) -> Self {
        let class = if environment == ServerEnvironment::Test {
            ReceiveExecutorClass::Expanded
        } else {
            ReceiveExecutorClass::Standard
        };
        Self::new(RealtimeReceiveExecutorSelection::for_class(
            class,
            logical_cpu_count(),
        ))
    }
}

impl Default for StaticRealtimeReceiveExecutorPolicy {
    fn default() -> Self {
        Self::for_environment(ServerEnvironment::Development)
    }
}

impl RealtimeReceiveExecutorPolicy for StaticRealtimeReceiveExecutorPolicy {
    fn current_selection(&self) -> RealtimeReceiveExecutorSelection {
        self.selection
    }

    fn refresh<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = RealtimeReceiveExecutorSelection> + Send + 'a>> {
        Box::pin(std::future::ready(self.selection))
    }
}

pub struct PostgresRealtimeReceiveExecutorPolicy {
    client: PostgresClientSource,
    environment: ServerEnvironment,
    instance_key: Option<String>,
    logical_cpus: NonZeroUsize,
    config_app_id: Uuid,
    current_class: AtomicU8,
    current_fallback: AtomicU8,
    refresh_generation: AtomicU64,
    refresh_lock: tokio::sync::Mutex<()>,
}

impl PostgresRealtimeReceiveExecutorPolicy {
    #[must_use]
    pub fn new(
        client: Client,
        environment: ServerEnvironment,
        hostname: Option<String>,
        instance_id: Option<String>,
    ) -> Self {
        Self::with_client(
            PostgresClientSource::fixed(client),
            environment,
            hostname,
            instance_id,
        )
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        environment: ServerEnvironment,
        hostname: Option<String>,
        instance_id: Option<String>,
    ) -> Self {
        Self::with_client(
            PostgresClientSource::pooled(pool),
            environment,
            hostname,
            instance_id,
        )
    }

    fn with_client(
        client: PostgresClientSource,
        environment: ServerEnvironment,
        hostname: Option<String>,
        instance_id: Option<String>,
    ) -> Self {
        let initial_class = if environment == ServerEnvironment::Test {
            ReceiveExecutorClass::Expanded
        } else {
            ReceiveExecutorClass::Standard
        };
        Self {
            client,
            environment,
            instance_key: instance_key(environment, hostname, instance_id),
            logical_cpus: logical_cpu_count(),
            config_app_id: INSTANT_CONFIG_APP_ID,
            current_class: AtomicU8::new(executor_class_encoded(initial_class)),
            current_fallback: AtomicU8::new(0),
            refresh_generation: AtomicU64::new(0),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    #[cfg(test)]
    fn with_logical_cpus(mut self, logical_cpus: NonZeroUsize) -> Self {
        self.logical_cpus = logical_cpus;
        self
    }

    /// Refreshes the cached decision once. Lifecycle wiring should call this
    /// from one bounded/coalesced background owner, never from message ingress.
    pub async fn refresh(&self) -> RealtimeReceiveExecutorSelection {
        let observed_generation = self.refresh_generation.load(Ordering::Acquire);
        let _refresh = self.refresh_lock.lock().await;
        if self.refresh_generation.load(Ordering::Acquire) != observed_generation {
            return self.current_selection();
        }
        let use_more = if self.environment == ServerEnvironment::Test {
            Ok(true)
        } else if let Some(instance_key) = &self.instance_key {
            let client = match self.client.acquire().await {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        flag = MORE_VFUTURES_FLAG,
                        "receive executor policy database read failed; using standard workers"
                    );
                    return self.update_selection(RealtimeReceiveExecutorSelection::fail_closed(
                        self.logical_cpus,
                        RealtimeReceiveExecutorFallback::Database,
                    ));
                }
            };
            match client
                .query_opt(
                    "SELECT flag_value.value
                         FROM attrs setting_attr
                         JOIN triples flag_setting
                           ON flag_setting.app_id = $1
                          AND flag_setting.attr_id = setting_attr.id
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
                    &[&self.config_app_id, &MORE_VFUTURES_FLAG],
                )
                .await
            {
                Ok(row) => {
                    let value = row.map(|row| row.get::<_, Value>(0));
                    use_more_vfutures(value.as_ref(), instance_key)
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        flag = MORE_VFUTURES_FLAG,
                        "receive executor policy database query failed; using standard workers"
                    );
                    Err(RealtimeReceiveExecutorFallback::Database)
                }
            }
        } else {
            Ok(false)
        };
        let use_more = match use_more {
            Ok(use_more) => use_more,
            Err(fallback) => {
                if fallback == RealtimeReceiveExecutorFallback::MalformedFlag {
                    tracing::warn!(
                        flag = MORE_VFUTURES_FLAG,
                        "receive executor policy flag is malformed; using standard workers"
                    );
                }
                return self.update_selection(RealtimeReceiveExecutorSelection::fail_closed(
                    self.logical_cpus,
                    fallback,
                ));
            }
        };
        let class = if use_more {
            ReceiveExecutorClass::Expanded
        } else {
            ReceiveExecutorClass::Standard
        };
        self.update_selection(RealtimeReceiveExecutorSelection::for_class(
            class,
            self.logical_cpus,
        ))
    }

    fn update_selection(
        &self,
        selection: RealtimeReceiveExecutorSelection,
    ) -> RealtimeReceiveExecutorSelection {
        self.current_fallback.store(
            selection
                .fallback
                .map_or(0, RealtimeReceiveExecutorFallback::encoded),
            Ordering::Release,
        );
        self.current_class
            .store(executor_class_encoded(selection.class), Ordering::Release);
        self.refresh_generation.fetch_add(1, Ordering::AcqRel);
        selection
    }
}

impl RealtimeReceiveExecutorPolicy for PostgresRealtimeReceiveExecutorPolicy {
    fn current_selection(&self) -> RealtimeReceiveExecutorSelection {
        let class = executor_class_decoded(self.current_class.load(Ordering::Acquire));
        RealtimeReceiveExecutorSelection {
            class,
            worker_limit: class.worker_limit(self.logical_cpus.get()),
            fallback: RealtimeReceiveExecutorFallback::decode(
                self.current_fallback.load(Ordering::Acquire),
            ),
        }
    }

    fn refresh<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = RealtimeReceiveExecutorSelection> + Send + 'a>> {
        Box::pin(PostgresRealtimeReceiveExecutorPolicy::refresh(self))
    }
}

const fn executor_class_encoded(class: ReceiveExecutorClass) -> u8 {
    match class {
        ReceiveExecutorClass::Standard => 0,
        ReceiveExecutorClass::Expanded => 1,
    }
}

fn executor_class_decoded(value: u8) -> ReceiveExecutorClass {
    if value == 1 {
        ReceiveExecutorClass::Expanded
    } else {
        ReceiveExecutorClass::Standard
    }
}

fn instance_key(
    environment: ServerEnvironment,
    hostname: Option<String>,
    instance_id: Option<String>,
) -> Option<String> {
    match environment {
        ServerEnvironment::Development => hostname,
        ServerEnvironment::Test => None,
        ServerEnvironment::Staging | ServerEnvironment::Production => instance_id,
    }
    .filter(|value| !value.trim().is_empty())
}

fn use_more_vfutures(
    value: Option<&Value>,
    instance_key: &str,
) -> Result<bool, RealtimeReceiveExecutorFallback> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Array(values)) => Ok(values
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == instance_key)),
        Some(_) => Err(RealtimeReceiveExecutorFallback::MalformedFlag),
    }
}

fn logical_cpu_count() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

pub trait RealtimeReceiveMetricsSource: Send + Sync + 'static {
    /// Returns one dispatcher snapshot. As in the upstream get-and-set gauge,
    /// the dispatcher's `puts` field is reset by snapshot collection.
    fn snapshot(&self) -> GroupedReceiveMetrics;
}

impl<K, V, R, E> RealtimeReceiveMetricsSource for GroupedReceiveDispatcher<K, V, R, E>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + 'static,
    R: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    fn snapshot(&self) -> GroupedReceiveMetrics {
        self.metrics_snapshot()
    }
}

pub struct RealtimeReceiveGaugeProvider {
    source: Arc<dyn RealtimeReceiveMetricsSource>,
}

impl RealtimeReceiveGaugeProvider {
    #[must_use]
    pub fn new(source: Arc<dyn RealtimeReceiveMetricsSource>) -> Self {
        Self { source }
    }
}

impl GaugeProvider for RealtimeReceiveGaugeProvider {
    fn gauges(&self) -> Result<Vec<(String, Value)>, String> {
        let snapshot = self.source.snapshot();
        let mut gauges = vec![
            (format!("{RECEIVE_GAUGE_PATH}.size"), json!(snapshot.queued)),
            (
                format!("{RECEIVE_GAUGE_PATH}.executor-class"),
                json!(snapshot.executor_class),
            ),
            (
                format!("{RECEIVE_GAUGE_PATH}.worker-count"),
                json!(snapshot.worker_count),
            ),
            (
                format!("{RECEIVE_GAUGE_PATH}.num-puts"),
                json!(snapshot.puts),
            ),
        ];
        if let Some(longest_wait) = snapshot.longest_wait {
            gauges.push((
                format!("{RECEIVE_GAUGE_PATH}.longest-waiting-ms"),
                json!(u64::try_from(longest_wait.as_millis()).unwrap_or(u64::MAX)),
            ));
        }
        Ok(gauges)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio_postgres::{NoTls, types::Json};

    use super::*;

    async fn postgres_client(database_url: &str) -> Client {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    }

    #[test]
    fn worker_limits_match_the_pinned_twenty_and_hundred_per_cpu() {
        let cpus = NonZeroUsize::new(3).unwrap();
        assert_eq!(
            RealtimeReceiveExecutorSelection::for_class(ReceiveExecutorClass::Standard, cpus)
                .worker_limit,
            60
        );
        assert_eq!(
            RealtimeReceiveExecutorSelection::for_class(ReceiveExecutorClass::Expanded, cpus)
                .worker_limit,
            300
        );
    }

    #[tokio::test]
    async fn trait_object_refreshes_without_changing_static_selection() {
        let selection = RealtimeReceiveExecutorSelection::for_class(
            ReceiveExecutorClass::Expanded,
            NonZeroUsize::new(2).unwrap(),
        );
        let policy: Arc<dyn RealtimeReceiveExecutorPolicy> =
            Arc::new(StaticRealtimeReceiveExecutorPolicy::new(selection));

        assert_eq!(policy.current_selection(), selection);
        assert_eq!(policy.refresh().await, selection);
    }

    #[test]
    fn environment_uses_hostname_in_dev_instance_id_when_hosted_and_test_override() {
        assert_eq!(
            StaticRealtimeReceiveExecutorPolicy::for_environment(ServerEnvironment::Test)
                .selection
                .class,
            ReceiveExecutorClass::Expanded
        );
        assert_eq!(
            StaticRealtimeReceiveExecutorPolicy::for_environment(ServerEnvironment::Production)
                .selection
                .class,
            ReceiveExecutorClass::Standard
        );
        assert_eq!(
            instance_key(
                ServerEnvironment::Development,
                Some("dev-host".to_owned()),
                Some("ignored".to_owned()),
            ),
            Some("dev-host".to_owned())
        );
        assert_eq!(
            instance_key(
                ServerEnvironment::Production,
                Some("ignored".to_owned()),
                Some("i-prod".to_owned()),
            ),
            Some("i-prod".to_owned())
        );
        assert_eq!(
            instance_key(
                ServerEnvironment::Test,
                Some("ignored".to_owned()),
                Some("ignored".to_owned()),
            ),
            None
        );
    }

    #[test]
    fn flag_membership_is_exact_and_malformed_containers_fail_closed() {
        assert!(use_more_vfutures(Some(&json!(["i-1", "i-2"])), "i-2").unwrap());
        assert!(!use_more_vfutures(Some(&json!(["i-1", 42, null])), "i-2").unwrap());
        assert!(!use_more_vfutures(None, "i-1").unwrap());
        assert!(!use_more_vfutures(Some(&Value::Null), "i-1").unwrap());
        assert_eq!(
            use_more_vfutures(Some(&json!({"i-1": true})), "i-1"),
            Err(RealtimeReceiveExecutorFallback::MalformedFlag)
        );
    }

    struct RecordingMetrics(Mutex<Option<GroupedReceiveMetrics>>);

    impl RealtimeReceiveMetricsSource for RecordingMetrics {
        fn snapshot(&self) -> GroupedReceiveMetrics {
            self.0.lock().unwrap().clone().unwrap()
        }
    }

    #[test]
    fn gauge_adapter_emits_the_pinned_receive_queue_paths() {
        let source = Arc::new(RecordingMetrics(Mutex::new(Some(GroupedReceiveMetrics {
            queued: 7,
            active_groups: 3,
            worker_count: 2,
            puts: 11,
            longest_wait: Some(std::time::Duration::from_millis(19)),
            executor_class: "expanded",
        }))));
        let provider = RealtimeReceiveGaugeProvider::new(source);
        let gauges = provider
            .gauges()
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(gauges["instant.reactive.session.receive-q.size"], 7);
        assert_eq!(
            gauges["instant.reactive.session.receive-q.longest-waiting-ms"],
            19
        );
        assert_eq!(
            gauges["instant.reactive.session.receive-q.executor-class"],
            "expanded"
        );
        assert_eq!(gauges["instant.reactive.session.receive-q.worker-count"], 2);
        assert_eq!(gauges["instant.reactive.session.receive-q.num-puts"], 11);
    }

    #[test]
    fn gauge_adapter_omits_longest_wait_when_the_queue_is_empty() {
        let source = Arc::new(RecordingMetrics(Mutex::new(Some(GroupedReceiveMetrics {
            queued: 0,
            active_groups: 0,
            worker_count: 0,
            puts: 0,
            longest_wait: None,
            executor_class: "standard",
        }))));
        let gauges = RealtimeReceiveGaugeProvider::new(source)
            .gauges()
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert!(!gauges.contains_key("instant.reactive.session.receive-q.longest-waiting-ms"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn postgres_policy_tracks_live_membership_and_rejects_malformed_containers() {
        let Some(database_url) = instant_test_support::test_database_url(
            "postgres_policy_tracks_live_membership_and_rejects_malformed_containers",
        ) else {
            return;
        };
        let setup = postgres_client(&database_url).await;
        let schema = format!("receive_executor_policy_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.attrs (
                   id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
                 );
                 CREATE TABLE {schema}.triples (
                   app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
                   value jsonb NOT NULL
                 );"
            ))
            .await
            .unwrap();
        let config_app_id = Uuid::new_v4();
        let setting_attr_id = Uuid::new_v4();
        let value_attr_id = Uuid::new_v4();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.attrs (id, app_id, etype, label) VALUES
                       ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')"
                ),
                &[&setting_attr_id, &value_attr_id, &config_app_id],
            )
            .await
            .unwrap();

        let policy_client = postgres_client(&database_url).await;
        policy_client
            .batch_execute(&format!("SET search_path TO {schema}"))
            .await
            .unwrap();
        let policy = PostgresRealtimeReceiveExecutorPolicy::new(
            policy_client,
            ServerEnvironment::Development,
            Some("dev-worker-3".to_owned()),
            Some("ignored-instance".to_owned()),
        )
        .with_config_app_id(config_app_id)
        .with_logical_cpus(NonZeroUsize::new(2).unwrap());
        assert_eq!(
            policy.current_selection(),
            RealtimeReceiveExecutorSelection {
                class: ReceiveExecutorClass::Standard,
                worker_limit: 40,
                fallback: None,
            }
        );

        let entity_id = Uuid::new_v4();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                       ($1, $2, $3, $4), ($1, $2, $5, $6)"
                ),
                &[
                    &config_app_id,
                    &entity_id,
                    &setting_attr_id,
                    &Json(json!(MORE_VFUTURES_FLAG)),
                    &value_attr_id,
                    &Json(json!(["another-worker", "dev-worker-3"])),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.refresh().await,
            RealtimeReceiveExecutorSelection {
                class: ReceiveExecutorClass::Expanded,
                worker_limit: 200,
                fallback: None,
            }
        );

        setup
            .execute(
                &format!(
                    "UPDATE {schema}.triples SET value = $4
                     WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
                ),
                &[
                    &config_app_id,
                    &entity_id,
                    &value_attr_id,
                    &Json(json!({"dev-worker-3": true})),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.refresh().await,
            RealtimeReceiveExecutorSelection {
                class: ReceiveExecutorClass::Standard,
                worker_limit: 40,
                fallback: Some(RealtimeReceiveExecutorFallback::MalformedFlag),
            }
        );

        let test_policy = PostgresRealtimeReceiveExecutorPolicy::new(
            postgres_client(&database_url).await,
            ServerEnvironment::Test,
            None,
            None,
        )
        .with_logical_cpus(NonZeroUsize::new(2).unwrap());
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        assert_eq!(
            policy.refresh().await,
            RealtimeReceiveExecutorSelection {
                class: ReceiveExecutorClass::Standard,
                worker_limit: 40,
                fallback: Some(RealtimeReceiveExecutorFallback::Database),
            }
        );
        drop(policy);
        assert_eq!(
            test_policy.current_selection(),
            RealtimeReceiveExecutorSelection {
                class: ReceiveExecutorClass::Expanded,
                worker_limit: 200,
                fallback: None,
            }
        );
    }
}
