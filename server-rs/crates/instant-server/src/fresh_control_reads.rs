//! Bounded fresh-read batching, not a policy-value or in-flight-result cache.
//!
//! Collect after checkout so requests queued during pool contention can share a
//! new statement. Once execution starts, later requests belong to a later read.
use std::future::Future;
use std::sync::{Arc, Mutex};

use instant_http::{ApiError, ApiErrorType};
use tokio::sync::{mpsc, oneshot};

use crate::postgres_pool::{PostgresClientGuard, PostgresClientSource};

const QUEUE_CAPACITY: usize = 256;
const MAX_BATCH: usize = instant_datalog::MAX_BATCH_MATCH_QUERIES;

pub(crate) trait FreshControlQuery: Send + Sync + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;

    fn execute(
        &self,
        client: &PostgresClientGuard<'_>,
        inputs: Vec<Self::Input>,
    ) -> impl Future<Output = Result<Vec<Self::Output>, ApiError>> + Send;
}

struct Request<Q: FreshControlQuery> {
    input: Option<Q::Input>,
    reply: oneshot::Sender<Result<Q::Output, ApiError>>,
}

pub(crate) struct FreshControlReads<Q: FreshControlQuery> {
    sender: mpsc::Sender<Request<Q>>,
}

impl<Q: FreshControlQuery> FreshControlReads<Q> {
    pub(crate) fn new(source: PostgresClientSource, query: Q) -> Self {
        // At most two existing leases per reader, not an extra connection pool.
        // Fixed/singleton clients retain serial execution.
        let concurrency = match &source {
            PostgresClientSource::Fixed(_) => 1,
            PostgresClientSource::Pool { pool, .. } => pool.max_size().clamp(1, 2),
        };
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(run(source, query, receiver, concurrency));
        Self { sender }
    }

    pub(crate) async fn load(&self, input: Q::Input) -> Result<Q::Output, ApiError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Request {
                input: Some(input),
                reply,
            })
            .await
            .map_err(|_| unavailable("Fresh control reader stopped"))?;
        response
            .await
            .map_err(|_| unavailable("Fresh control reader dropped its reply"))?
    }
}

async fn run<Q: FreshControlQuery>(
    source: PostgresClientSource,
    query: Q,
    receiver: mpsc::Receiver<Request<Q>>,
    concurrency: usize,
) {
    let source = Arc::new(source);
    let query = Arc::new(query);
    let receiver = Arc::new(Mutex::new(receiver));
    let mut batches = tokio::task::JoinSet::new();
    loop {
        if batches.len() == concurrency {
            let _ = batches.join_next().await;
            continue;
        }
        // Only this dispatcher polls recv. The mutex covers one poll/try_recv,
        // never an await, so an idle receiver cannot block batch collection.
        let next = std::future::poll_fn(|cx| {
            receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .poll_recv(cx)
        });
        tokio::select! {
            biased;
            Some(_) = batches.join_next(), if !batches.is_empty() => {}
            first = next => {
                let Some(first) = first else { break };
                if first.reply.is_closed() { continue; }
                batches.spawn(execute_batch(Arc::clone(&source), Arc::clone(&query), Arc::clone(&receiver), first));
            }
        }
    }
    // Closing the request channel does not abandon accepted batches or their
    // connection-disposal guards. A panicked batch fails its own replies only.
    while batches.join_next().await.is_some() {}
}

async fn execute_batch<Q: FreshControlQuery>(
    source: Arc<PostgresClientSource>,
    query: Arc<Q>,
    receiver: Arc<Mutex<mpsc::Receiver<Request<Q>>>>,
    first: Request<Q>,
) {
    if first.reply.is_closed() {
        return;
    }
    let mut batch = vec![first];
    // Shared work gets its own bounded cancellation/disposal scope. Dropping
    // one waiter cannot cancel a different caller's fresh policy lookup.
    let tracker = crate::StatementTracker::default();
    let result = tokio::time::timeout(
        instant_pg::DEFAULT_QUERY_TIMEOUT,
        tracker.scope(async {
            let client = source
                .acquire()
                .await
                .map_err(|error| unavailable(error.to_string()))?;
            {
                let mut receiver = receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while batch.len() < MAX_BATCH {
                    match receiver.try_recv() {
                        Ok(request) if !request.reply.is_closed() => batch.push(request),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            batch.retain(|request| !request.reply.is_closed());
            if batch.is_empty() {
                return Ok(Vec::new());
            }
            let inputs = batch
                .iter_mut()
                .map(|request| {
                    request
                        .input
                        .take()
                        .expect("a queued control request is dispatched once")
                })
                .collect();
            query.execute(&client, inputs).await
        }),
    )
    .await
    .unwrap_or_else(|_| {
        Err(unavailable(
            "Fresh control read exceeded database query deadline",
        ))
    });
    match result {
        Ok(outputs) if outputs.len() == batch.len() => {
            for (request, output) in batch.into_iter().zip(outputs) {
                let _ = request.reply.send(Ok(output));
            }
        }
        result => {
            let error = match result {
                Err(error) => error,
                Ok(_) => unavailable("Fresh control reader returned an invalid result count"),
            };
            for request in batch {
                let _ = request.reply.send(Err(error.clone()));
            }
        }
    }
}

fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorType::Unknown, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct GatedQuery {
        calls: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    }

    impl FreshControlQuery for GatedQuery {
        type Input = usize;
        type Output = (usize, usize);

        async fn execute(
            &self,
            client: &PostgresClientGuard<'_>,
            inputs: Vec<usize>,
        ) -> Result<Vec<Self::Output>, ApiError> {
            let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            client
                .query_one("SELECT 1", &[])
                .await
                .map_err(|error| unavailable(error.to_string()))?;
            if inputs.contains(&1) {
                self.started.add_permits(1);
                self.release.acquire().await.unwrap().forget();
            }
            Ok(inputs.into_iter().map(|input| (input, call)).collect())
        }
    }

    #[tokio::test]
    async fn fresh_batches_overlap_without_reusing_inflight_results_or_cancelling_peers() {
        let Some(url) = instant_test_support::test_database_url(
            "fresh_batches_overlap_without_reusing_inflight_results_or_cancelling_peers",
        ) else {
            return;
        };
        let pool = crate::PostgresPool::connect(&url, 2).await.unwrap();
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(FreshControlReads::new(
            PostgresClientSource::pooled_read_only(pool.clone()),
            GatedQuery {
                calls: Arc::clone(&calls),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            },
        ));
        let first_reads = Arc::clone(&reads);
        let first = tokio::spawn(async move { first_reads.load(1).await });
        started.acquire().await.unwrap().forget();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        // Arrives after the first statement started; must execute a new fresh
        // batch, and must not wait for that unrelated batch's blocked response.
        let second = tokio::time::timeout(Duration::from_secs(2), reads.load(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, (2, 2));
        assert_eq!(pool.max_size(), 2);
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        release.add_permits(1);
        let third = tokio::time::timeout(Duration::from_secs(2), reads.load(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third.0, 3);
        assert!(
            third.1 > second.1,
            "completed and in-flight policy values are never cached"
        );
        drop(reads);
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool.status().available != pool.status().size {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelling_one_waiter_keeps_the_other_waiter_in_the_same_fresh_batch() {
        let Some(url) = instant_test_support::test_database_url(
            "cancelling_one_waiter_keeps_the_other_waiter_in_the_same_fresh_batch",
        ) else {
            return;
        };
        let pool = crate::PostgresPool::connect(&url, 1).await.unwrap();
        let held = pool.acquire().await.unwrap();
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(FreshControlReads::new(
            PostgresClientSource::pooled_read_only(pool.clone()),
            GatedQuery {
                calls: Arc::clone(&calls),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            },
        ));
        let first_reads = Arc::clone(&reads);
        let first = tokio::spawn(async move { first_reads.load(1).await });
        // The singleton reader has consumed the first request but cannot start
        // SQL yet, so the second request joins that same new statement.
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool.status().waiting == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second_reads = Arc::clone(&reads);
        let second = tokio::spawn(async move { second_reads.load(2).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while reads.sender.capacity() == QUEUE_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(held);
        started.acquire().await.unwrap().forget();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        release.add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), second)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            (2, 1)
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }
}
