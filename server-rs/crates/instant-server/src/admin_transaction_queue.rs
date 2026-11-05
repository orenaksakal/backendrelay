use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use instant_transact::TransactionStep;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool};

const ADMIN_TRANSACTION_QUEUE_FLAG: &str = "enable-admin-transact-queue-apps";
pub const ADMIN_TRANSACTION_QUEUE_TIMEOUT: Duration = Duration::from_secs(35);

pub type AdminTransactionQueuePolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>>;

pub trait AdminTransactionQueuePolicy: Send + Sync + 'static {
    fn enabled(&self, app_id: Uuid) -> AdminTransactionQueuePolicyFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DisabledAdminTransactionQueue;

impl AdminTransactionQueuePolicy for DisabledAdminTransactionQueue {
    fn enabled(&self, _app_id: Uuid) -> AdminTransactionQueuePolicyFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

pub struct PostgresAdminTransactionQueuePolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresAdminTransactionQueuePolicy {
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
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }
}

impl AdminTransactionQueuePolicy for PostgresAdminTransactionQueuePolicy {
    fn enabled(&self, app_id: Uuid) -> AdminTransactionQueuePolicyFuture<'_> {
        Box::pin(async move {
            let client = self.client.acquire().await.map_err(database_error)?;
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
                    &[&self.config_app_id, &ADMIN_TRANSACTION_QUEUE_FLAG],
                )
                .await
                .map_err(database_error)?
                .map(|row| row.get::<_, serde_json::Value>(0));
            Ok(app_collection_contains(value.as_ref(), app_id))
        })
    }
}

fn app_collection_contains(value: Option<&serde_json::Value>, app_id: Uuid) -> bool {
    value
        .and_then(serde_json::Value::as_array)
        .is_some_and(|values| {
            values.iter().any(|value| {
                value.as_str().and_then(|value| Uuid::parse_str(value).ok()) == Some(app_id)
            })
        })
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TransactionGroup {
    app_id: Uuid,
    attr_id: Uuid,
}

struct GroupRegistration<'a> {
    queue: &'a AdminTransactionQueue,
    group: TransactionGroup,
    lock: Arc<AsyncMutex<()>>,
}

impl Drop for GroupRegistration<'_> {
    fn drop(&mut self) {
        self.queue.remove_idle_group(self.group, &self.lock);
    }
}

/// Native FIFO scheduler for the optional admin transaction rollout.
pub struct AdminTransactionQueue {
    groups: Mutex<HashMap<TransactionGroup, Weak<AsyncMutex<()>>>>,
    workers: Arc<Semaphore>,
    timeout: Duration,
}

impl Default for AdminTransactionQueue {
    fn default() -> Self {
        let workers = std::thread::available_parallelism()
            .map_or(100, std::num::NonZeroUsize::get)
            .saturating_mul(100)
            .max(1);
        Self::new(workers, ADMIN_TRANSACTION_QUEUE_TIMEOUT)
    }
}

impl AdminTransactionQueue {
    #[must_use]
    pub fn new(max_workers: usize, timeout: Duration) -> Self {
        Self {
            groups: Mutex::new(HashMap::new()),
            workers: Arc::new(Semaphore::new(max_workers.max(1))),
            timeout,
        }
    }

    /// Runs a prepared admin transaction under the reference grouping,
    /// concurrency, and total wait deadline.
    ///
    /// # Errors
    ///
    /// Returns the operation's error, a worker failure, or the JVM-compatible
    /// public timeout error when the total queue and execution deadline expires.
    pub async fn run<F, T>(
        &self,
        app_id: Uuid,
        steps: &[TransactionStep],
        operation: F,
    ) -> Result<T, ApiError>
    where
        F: Future<Output = Result<T, ApiError>>,
    {
        let group =
            first_add_triple_attr(steps).map(|attr_id| TransactionGroup { app_id, attr_id });
        let result = tokio::time::timeout(self.timeout, async {
            let group_registration = group.map(|group| GroupRegistration {
                queue: self,
                group,
                lock: self.group_lock(group),
            });
            let _group_guard = if let Some(registration) = &group_registration {
                Some(registration.lock.lock().await)
            } else {
                None
            };
            let _worker = self
                .workers
                .acquire()
                .await
                .map_err(|error| database_error(error.to_string()))?;
            operation.await
        })
        .await;
        result.unwrap_or_else(|_| {
            Err(ApiError::new(
                ApiErrorType::Timeout,
                "The query took too long to complete.",
            ))
        })
    }

    fn group_lock(&self, group: TransactionGroup) -> Arc<AsyncMutex<()>> {
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lock) = groups.get(&group).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        groups.insert(group, Arc::downgrade(&lock));
        lock
    }

    fn remove_idle_group(&self, group: TransactionGroup, lock: &Arc<AsyncMutex<()>>) {
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if groups
            .get(&group)
            .is_some_and(|current| current.ptr_eq(&Arc::downgrade(lock)))
            && Arc::strong_count(lock) == 1
        {
            groups.remove(&group);
        }
    }
}

fn first_add_triple_attr(steps: &[TransactionStep]) -> Option<Uuid> {
    steps.iter().find_map(|step| match step {
        TransactionStep::AddTriple(triple) => Some(triple.attr_id),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use instant_transact::{EntityLookup, TripleOptions, TripleStep};
    use serde_json::json;

    use super::*;

    fn add_triple(attr_id: Uuid) -> TransactionStep {
        TransactionStep::AddTriple(TripleStep {
            entity: EntityLookup::Id(Uuid::new_v4()),
            etype: None,
            attr_id,
            reverse_etype: None,
            value: json!(1),
            options: TripleOptions::default(),
        })
    }

    #[test]
    fn rollout_collection_ignores_malformed_values() {
        let app_id = Uuid::new_v4();
        assert!(app_collection_contains(
            Some(&json!([app_id, "bad"])),
            app_id
        ));
        assert!(!app_collection_contains(Some(&json!(["bad", 1])), app_id));
        assert!(!app_collection_contains(Some(&json!({})), app_id));
        assert!(!app_collection_contains(None, app_id));
    }

    #[test]
    fn grouping_uses_only_the_first_add_triple_attribute() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert_eq!(
            first_add_triple_attr(&[add_triple(first), add_triple(second)]),
            Some(first)
        );
        assert_eq!(first_add_triple_attr(&[]), None);
    }

    #[tokio::test]
    async fn same_group_is_fifo_while_other_attributes_can_run() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let queue = Arc::new(AdminTransactionQueue::new(3, Duration::from_secs(1)));
        let app_id = Uuid::new_v4();
        let shared_attr = Uuid::new_v4();
        let other_attr = Uuid::new_v4();
        let order = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let (first_started, first_started_rx) = tokio::sync::oneshot::channel();
        let first = {
            let queue = Arc::clone(&queue);
            let order = Arc::clone(&order);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            tokio::spawn(async move {
                queue
                    .run(app_id, &[add_triple(shared_attr)], async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        order.lock().unwrap().push(1);
                        let _receiver_was_dropped = first_started.send(());
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                    .unwrap();
            })
        };
        first_started_rx.await.unwrap();

        let spawn = |marker, attr_id, delay| {
            let queue = Arc::clone(&queue);
            let order = Arc::clone(&order);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            tokio::spawn(async move {
                queue
                    .run(app_id, &[add_triple(attr_id)], async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        order.lock().unwrap().push(marker);
                        tokio::time::sleep(delay).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                    .unwrap();
            })
        };
        let second = spawn(2, shared_attr, Duration::ZERO);
        let other = spawn(3, other_attr, Duration::ZERO);
        first.await.unwrap();
        second.await.unwrap();
        other.await.unwrap();

        let order = order.lock().unwrap().clone();
        assert!(
            order.iter().position(|value| *value == 1) < order.iter().position(|value| *value == 2)
        );
        assert!(peak.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn total_queue_deadline_uses_the_public_timeout_contract() {
        let queue = AdminTransactionQueue::new(1, Duration::from_millis(1));
        let steps = [add_triple(Uuid::new_v4())];
        let error = queue
            .run(Uuid::new_v4(), &steps, async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::Timeout);
        assert_eq!(error.body().message, "The query took too long to complete.");
        assert!(queue.groups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn worker_limit_bounds_independent_groups() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let queue = Arc::new(AdminTransactionQueue::new(1, Duration::from_secs(1)));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let queue = Arc::clone(&queue);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                queue
                    .run(Uuid::new_v4(), &[add_triple(Uuid::new_v4())], async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
}
