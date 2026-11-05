use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use instant_datalog::MAX_BATCH_MATCH_QUERIES;
use tokio::sync::oneshot;

type ExecutionFuture<O, E> = Pin<Box<dyn Future<Output = Result<Vec<O>, E>> + Send>>;
type Executor<I, O, E> = dyn Fn(Vec<I>) -> ExecutionFuture<O, E> + Send + Sync;

/// Error returned to an individual request submitted through [`BatchLoader`].
#[derive(Debug)]
pub enum BatchLoadError<E> {
    Executor(Arc<E>),
    ResultCount { expected: usize, actual: usize },
    WorkerDropped,
}

impl<E> Clone for BatchLoadError<E> {
    fn clone(&self) -> Self {
        match self {
            Self::Executor(error) => Self::Executor(Arc::clone(error)),
            Self::ResultCount { expected, actual } => Self::ResultCount {
                expected: *expected,
                actual: *actual,
            },
            Self::WorkerDropped => Self::WorkerDropped,
        }
    }
}

impl<E: fmt::Display> fmt::Display for BatchLoadError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Executor(error) => write!(formatter, "batch execution failed: {error}"),
            Self::ResultCount { expected, actual } => write!(
                formatter,
                "batch executor returned {actual} results for {expected} requests"
            ),
            Self::WorkerDropped => {
                formatter.write_str("batch worker stopped before delivering a result")
            }
        }
    }
}

impl<E: Error + 'static> Error for BatchLoadError<E> {}

struct Pending<I, O, E> {
    input: I,
    result: oneshot::Sender<Result<O, BatchLoadError<E>>>,
}

struct Inner<I, O, E> {
    pending: Mutex<VecDeque<Pending<I, O, E>>>,
    executor: Arc<Executor<I, O, E>>,
}

/// Cancellation-safe async equivalent of `InstantDB`'s per-pool Datalog loader.
///
/// Concurrent requests are drained FIFO in batches of at most 50. The first
/// request in each tranche schedules a detached worker, so dropping any caller
/// cannot strand the other waiters. Each request receives exactly the output at
/// its input position.
pub struct BatchLoader<I, O, E> {
    inner: Arc<Inner<I, O, E>>,
}

impl<I, O, E> Clone for BatchLoader<I, O, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<I, O, E> BatchLoader<I, O, E>
where
    I: Send + 'static,
    O: Send + 'static,
    E: Error + Send + Sync + 'static,
{
    #[must_use]
    pub fn new<F, Fut>(executor: F) -> Self
    where
        F: Fn(Vec<I>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<O>, E>> + Send + 'static,
    {
        let executor = Arc::new(move |inputs| Box::pin(executor(inputs)) as ExecutionFuture<O, E>);
        Self {
            inner: Arc::new(Inner {
                pending: Mutex::new(VecDeque::new()),
                executor,
            }),
        }
    }

    /// Queues one request and waits for its position-matched result.
    ///
    /// # Errors
    ///
    /// Returns [`BatchLoadError::Executor`] to every request in a failed batch,
    /// [`BatchLoadError::ResultCount`] when the executor violates its cardinality
    /// contract, or [`BatchLoadError::WorkerDropped`] if a worker panics.
    pub async fn load(&self, input: I) -> Result<O, BatchLoadError<E>> {
        let (sender, receiver) = oneshot::channel();
        let should_dispatch = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.push_back(Pending {
                input,
                result: sender,
            });
            pending.len() % MAX_BATCH_MATCH_QUERIES == 1
        };
        if should_dispatch {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                dispatch(inner).await;
            });
        }
        receiver.await.unwrap_or(Err(BatchLoadError::WorkerDropped))
    }

    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

async fn dispatch<I, O, E>(inner: Arc<Inner<I, O, E>>)
where
    I: Send + 'static,
    O: Send + 'static,
    E: Error + Send + Sync + 'static,
{
    tokio::task::yield_now().await;
    let items = {
        let mut pending = inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = pending.len().min(MAX_BATCH_MATCH_QUERIES);
        pending.drain(..count).collect::<Vec<_>>()
    };
    if items.is_empty() {
        return;
    }
    let expected = items.len();
    let mut owned_inputs = Vec::with_capacity(expected);
    let mut senders = Vec::with_capacity(expected);
    for Pending { input, result } in items {
        owned_inputs.push(input);
        senders.push(result);
    }
    match (inner.executor)(owned_inputs).await {
        Ok(outputs) if outputs.len() == expected => {
            for (sender, output) in senders.into_iter().zip(outputs) {
                let _ = sender.send(Ok(output));
            }
        }
        Ok(outputs) => {
            let error = BatchLoadError::ResultCount {
                expected,
                actual: outputs.len(),
            };
            for sender in senders {
                let _ = sender.send(Err(error.clone()));
            }
        }
        Err(error) => {
            let error = BatchLoadError::Executor(Arc::new(error));
            for sender in senders {
                let _ = sender.send(Err(error.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestError {}

    #[tokio::test]
    async fn coalesces_fifo_requests_into_fifty_item_batches() {
        let batch_sizes = Arc::new(Mutex::new(Vec::new()));
        let recorded_sizes = Arc::clone(&batch_sizes);
        let loader = BatchLoader::new(move |inputs: Vec<usize>| {
            let recorded_sizes = Arc::clone(&recorded_sizes);
            async move {
                recorded_sizes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(inputs.len());
                Ok::<_, TestError>(inputs.into_iter().map(|value| value * 2).collect())
            }
        });
        let tasks: Vec<_> = (0..125)
            .map(|value| {
                let loader = loader.clone();
                tokio::spawn(async move { loader.load(value).await.unwrap() })
            })
            .collect();
        let mut outputs = Vec::new();
        for task in tasks {
            outputs.push(task.await.unwrap());
        }
        assert_eq!(outputs, (0..125).map(|value| value * 2).collect::<Vec<_>>());
        let mut sizes = batch_sizes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        sizes.sort_unstable();
        assert_eq!(sizes, [25, 50, 50]);
    }

    #[tokio::test]
    async fn broadcasts_one_executor_error_to_every_waiter() {
        let loader = BatchLoader::new(|_inputs: Vec<usize>| async {
            Err::<Vec<usize>, _>(TestError("database unavailable"))
        });
        let tasks: Vec<_> = (0..3)
            .map(|value| {
                let loader = loader.clone();
                tokio::spawn(async move { loader.load(value).await.unwrap_err() })
            })
            .collect();
        let mut errors = Vec::new();
        for task in tasks {
            errors.push(task.await.unwrap());
        }
        let BatchLoadError::Executor(first) = &errors[0] else {
            panic!("expected executor error");
        };
        assert!(errors.iter().all(|error| {
            matches!(error, BatchLoadError::Executor(next) if Arc::ptr_eq(first, next))
        }));
    }

    #[tokio::test]
    async fn rejects_executor_result_count_mismatches_without_stranding_waiters() {
        let loader = BatchLoader::new(|_inputs: Vec<usize>| async {
            Ok::<_, TestError>(Vec::<usize>::new())
        });
        let tasks: Vec<_> = (0..4)
            .map(|value| {
                let loader = loader.clone();
                tokio::spawn(async move { loader.load(value).await.unwrap_err() })
            })
            .collect();
        for task in tasks {
            assert!(matches!(
                task.await.unwrap(),
                BatchLoadError::ResultCount {
                    expected: 4,
                    actual: 0
                }
            ));
        }
    }

    #[tokio::test]
    async fn dispatch_survives_cancellation_of_the_scheduling_caller() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completions = Arc::new(AtomicUsize::new(0));
        let loader = BatchLoader::new({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let completions = Arc::clone(&completions);
            move |inputs: Vec<usize>| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let completions = Arc::clone(&completions);
                async move {
                    started.notify_one();
                    release.notified().await;
                    completions.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, TestError>(inputs)
                }
            }
        });
        let caller = {
            let loader = loader.clone();
            tokio::spawn(async move { loader.load(1).await })
        };
        started.notified().await;
        caller.abort();
        release.notify_one();
        while completions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }

        let second = {
            let loader = loader.clone();
            tokio::spawn(async move { loader.load(2).await.unwrap() })
        };
        started.notified().await;
        release.notify_one();
        assert_eq!(second.await.unwrap(), 2);
    }
}
