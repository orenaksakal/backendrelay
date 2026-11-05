use std::collections::HashSet;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use instant_permissions::{
    RateLimit, RateLimitConsumer, RateLimitDecision, RateLimitRequest, RefillType,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::Sha256;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::mpsc::{Sender, channel};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::postgres_pool::PostgresClientSource;

const TOKEN_SCALE: u128 = 1_000_000_000;
const STATE_VERSION: u8 = 1;
const QUEUE_CAPACITY: usize = 64;
const MAX_BATCH_REQUESTS: usize = 128;
// One connection per worker: a single slow bucket transaction must not
// serialize every other app's rate limit check behind it.
const WORKER_CONNECTIONS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const DEADLINE_ERROR: &str = "PostgreSQL rate limit deadline exceeded or caller abandoned; outcome may be unknown, do not automatically retry";

type HmacSha256 = Hmac<Sha256>;

struct ConsumeCommand {
    kind: CommandKind,
    requests: Vec<RateLimitRequest>,
    reply: mpsc::SyncSender<Result<Vec<RateLimitDecision>, String>>,
    deadline: Instant,
    abandoned: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandKind {
    Consume,
    Reset,
}

impl ConsumeCommand {
    fn is_stale(&self) -> bool {
        Instant::now() >= self.deadline || self.abandoned.load(Ordering::Acquire)
    }
}

/// Durable, cross-process permission rate limiter backed by `rate_limit_keys`.
///
/// A small local worker pool owns the asynchronous PostgreSQL clients; each
/// worker checks out its own connection for one batch. Each bucket update also
/// takes a transaction-scoped advisory lock derived from its HMAC key, so
/// independent Rust servers serialize creation and consumption without relying
/// on a process-local cache.
#[derive(Clone)]
pub struct PostgresRateLimitConsumer {
    sender: Sender<ConsumeCommand>,
}

impl Debug for PostgresRateLimitConsumer {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresRateLimitConsumer")
            .finish_non_exhaustive()
    }
}

impl PostgresRateLimitConsumer {
    /// Starts a database worker on the current Tokio runtime.
    #[must_use]
    pub fn new(client: Client, hmac_key: impl Into<Arc<[u8]>>) -> Self {
        let hmac_key = hmac_key.into();
        Self::from_source(&PostgresClientSource::fixed(client), &hmac_key)
    }

    /// Starts a worker that checks out a fresh lease for each atomic batch.
    /// A dedicated production pool preserves the JVM's transaction-independent
    /// Bucket4j semantics while replacing closed PostgreSQL connections.
    #[must_use]
    pub fn from_pool(pool: PostgresPool, hmac_key: impl Into<Arc<[u8]>>) -> Self {
        let hmac_key = hmac_key.into();
        Self::from_source(&PostgresClientSource::pooled(pool), &hmac_key)
    }

    fn from_source(source: &PostgresClientSource, hmac_key: &Arc<[u8]>) -> Self {
        let (sender, receiver) = channel(QUEUE_CAPACITY);
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
        for _ in 0..WORKER_CONNECTIONS {
            tokio::spawn(run_shared_worker(
                source.clone(),
                Arc::clone(hmac_key),
                Arc::clone(&receiver),
            ));
        }
        Self { sender }
    }

    fn send_command(
        &self,
        kind: CommandKind,
        requests: &[RateLimitRequest],
    ) -> Result<Vec<RateLimitDecision>, String> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let handle = Handle::try_current().ok();
        if handle
            .as_ref()
            .is_some_and(|handle| handle.runtime_flavor() != RuntimeFlavor::MultiThread)
        {
            return Err(
                "PostgreSQL rate limit consumption requires a multi-thread Tokio runtime"
                    .to_owned(),
            );
        }
        if requests.len() > MAX_BATCH_REQUESTS {
            return Err("PostgreSQL rate limit batch is too large".to_owned());
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        if requests.iter().any(|request| request.tokens == 0) {
            return Err("rate limit tokens must be a positive integer".to_owned());
        }
        let (reply, response) = mpsc::sync_channel(1);
        let abandoned = Arc::new(AtomicBool::new(false));
        let permit = self.sender.try_reserve().map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(()) => {
                "PostgreSQL rate limit queue is full".to_owned()
            }
            tokio::sync::mpsc::error::TrySendError::Closed(()) => {
                "PostgreSQL rate limit worker has stopped".to_owned()
            }
        })?;
        permit.send(ConsumeCommand {
            kind,
            requests: requests.to_vec(),
            reply,
            deadline,
            abandoned: Arc::clone(&abandoned),
        });

        let receive = || {
            let result = response.recv_timeout(deadline.saturating_duration_since(Instant::now()));
            abandoned.store(true, Ordering::Release);
            result.map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => DEADLINE_ERROR.to_owned(),
                mpsc::RecvTimeoutError::Disconnected => "PostgreSQL rate limit worker dropped its response; outcome may be unknown, do not automatically retry".to_owned(),
            })?
        };
        if handle.is_some() {
            tokio::task::block_in_place(receive)
        } else {
            receive()
        }
    }
}

impl RateLimitConsumer for PostgresRateLimitConsumer {
    fn consume(&self, request: &RateLimitRequest) -> Result<RateLimitDecision, String> {
        self.consume_all(std::slice::from_ref(request))?
            .into_iter()
            .next()
            .ok_or_else(|| "PostgreSQL rate limit worker returned no decision".to_owned())
    }

    fn consume_all(&self, requests: &[RateLimitRequest]) -> Result<Vec<RateLimitDecision>, String> {
        self.send_command(CommandKind::Consume, requests)
    }

    fn reset(&self, request: &RateLimitRequest) -> Result<(), String> {
        self.send_command(CommandKind::Reset, std::slice::from_ref(request))?;
        Ok(())
    }
}

async fn run_shared_worker(
    source: PostgresClientSource,
    hmac_key: Arc<[u8]>,
    receiver: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ConsumeCommand>>>,
) {
    loop {
        // Hold the receive lock only until a command is dequeued so the other
        // workers can take their own commands concurrently.
        let command = receiver.lock().await.recv().await;
        let Some(command) = command else {
            return;
        };
        if handle_command(&source, &hmac_key, command).await {
            return;
        }
    }
}

#[cfg(test)]
async fn run_worker(
    source: PostgresClientSource,
    hmac_key: Arc<[u8]>,
    mut receiver: tokio::sync::mpsc::Receiver<ConsumeCommand>,
) {
    while let Some(command) = receiver.recv().await {
        if handle_command(&source, &hmac_key, command).await {
            return;
        }
    }
}

/// Handles one command. Returns `true` when this worker must stop because its
/// fixed connection cannot be replaced after a timeout.
async fn handle_command(
    source: &PostgresClientSource,
    hmac_key: &[u8],
    command: ConsumeCommand,
) -> bool {
    if command.is_stale() {
        let _ = command.reply.send(Err(DEADLINE_ERROR.to_owned()));
        return false;
    }
    let deadline = tokio::time::Instant::from_std(command.deadline);
    let mut pooled = None;
    let work = async {
        let mut fixed;
        // Own pooled leases here so a timed-out transaction can never be recycled.
        let client: &mut Client = match source {
            PostgresClientSource::Fixed(shared) => {
                fixed = shared.lock().await;
                &mut fixed
            }
            PostgresClientSource::Pool { pool, .. } => {
                pooled = Some(pool.acquire().await.map_err(|error| error.to_string())?);
                pooled.as_mut().unwrap()
            }
        };
        if command.is_stale() {
            return Err(DEADLINE_ERROR.to_owned());
        }
        match command.kind {
            CommandKind::Consume => consume_postgres_batch(client, hmac_key, &command)
                .await
                .map_err(|error| error.to_string()),
            CommandKind::Reset => reset_postgres_batch(client, hmac_key, &command)
                .await
                .map_err(|error| error.to_string()),
        }
    };
    let result = tokio::time::timeout_at(deadline, work).await;
    let timed_out = result.is_err();
    let result = result.unwrap_or_else(|_| Err(DEADLINE_ERROR.to_owned()));
    if timed_out {
        // Dropping a query future only queues rollback; it does not cancel SQL.
        // Discard the connection and never replay an ambiguous COMMIT.
        if let Some(client) = pooled.take() {
            drop(deadpool_postgres::Object::take(client));
        }
    }
    let _ = command.reply.send(result);
    // A fixed connection cannot be replaced safely after cancellation.
    timed_out && matches!(source, PostgresClientSource::Fixed(_))
}

async fn consume_postgres_batch(
    client: &mut Client,
    hmac_key: &[u8],
    command: &ConsumeCommand,
) -> Result<Vec<RateLimitDecision>, Box<dyn std::error::Error + Send + Sync>> {
    let requests = &command.requests;
    let mut keys = requests
        .iter()
        .map(|request| {
            let key = bucket_uuid(hmac_key, request)?;
            let advisory_key = i64::from_be_bytes(key.as_bytes()[..8].try_into()?);
            Ok((key, advisory_key))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error + Send + Sync>>>()?;
    let mut ordered_locks = keys.iter().map(|(_, lock)| *lock).collect::<Vec<_>>();
    if keys
        .iter()
        .map(|(key, _)| *key)
        .collect::<HashSet<_>>()
        .len()
        != keys.len()
    {
        return Err("duplicate rate limit bucket in atomic batch".into());
    }
    ordered_locks.sort_unstable();
    ordered_locks.dedup();
    let transaction = client.transaction().await?;
    let result = async {
        transaction
            .batch_execute("SET LOCAL lock_timeout = '250ms'; SET LOCAL statement_timeout = '1s'")
            .await?;
        for advisory_key in ordered_locks {
            transaction
                .query_one("SELECT pg_advisory_xact_lock($1)", &[&advisory_key])
                .await?;
        }
        let now_text: String = transaction
            .query_one(
                "SELECT floor(extract(epoch FROM clock_timestamp()) * 1000000000)::text",
                &[],
            )
            .await?
            .get(0);
        let now_nanos = now_text.parse::<u128>()?;
        let mut decisions = Vec::with_capacity(requests.len());
        let mut pending = Vec::with_capacity(requests.len());
        for (request, (key, _)) in requests.iter().zip(keys.drain(..)) {
            let stored = transaction
                .query_opt(
                    "SELECT value FROM rate_limit_keys WHERE key = $1 FOR UPDATE",
                    &[&key],
                )
                .await?
                .map(|row| row.get::<_, Vec<u8>>(0));
            let mut state = match stored {
                Some(bytes) => PersistedBucket::decode(&bytes, request.config.limits.len())?,
                None => PersistedBucket::full(&request.config.limits, now_nanos),
            };
            decisions.push(state.consume(&request.config.limits, request.tokens, now_nanos)?);
            pending.push((key, state));
        }
        if decisions
            .iter()
            .all(|decision| matches!(decision, RateLimitDecision::Allowed { .. }))
        {
            for (key, state) in pending {
                let bytes = serde_json::to_vec(&state)?;
                transaction
                    .execute(
                        "INSERT INTO rate_limit_keys (key, value) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                        &[&key, &bytes],
                    )
                    .await?;
            }
        }
        if command.is_stale() {
            return Err(DEADLINE_ERROR.into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(decisions)
    }
    .await;
    match result {
        Ok(decisions) => {
            transaction.commit().await.map_err(|error| format!("PostgreSQL rate limit commit failed: {error}; outcome may be unknown, do not automatically retry"))?;
            Ok(decisions)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

async fn reset_postgres_batch(
    client: &mut Client,
    hmac_key: &[u8],
    command: &ConsumeCommand,
) -> Result<Vec<RateLimitDecision>, Box<dyn std::error::Error + Send + Sync>> {
    for request in &command.requests {
        let key = bucket_uuid(hmac_key, request)?;
        client
            .execute("DELETE FROM rate_limit_keys WHERE key = $1", &[&key])
            .await?;
    }
    Ok(Vec::new())
}

fn bucket_uuid(hmac_key: &[u8], request: &RateLimitRequest) -> Result<Uuid, String> {
    let canonical = json!({
        "version": STATE_VERSION,
        "kind": "user",
        "appId": request.app_id,
        "bucketName": request.bucket_name,
        "config": request.config.limits.iter().map(limit_json).collect::<Vec<_>>(),
        "bucketKey": request.bucket_key,
    });
    let encoded = serde_json::to_vec(&canonical).map_err(|error| error.to_string())?;
    let mut hmac = HmacSha256::new_from_slice(hmac_key)
        .map_err(|_| "rate limit HMAC key must not be empty".to_owned())?;
    hmac.update(&encoded);
    let digest = hmac.finalize().into_bytes();
    Uuid::from_slice(&digest[..16]).map_err(|error| error.to_string())
}

fn limit_json(limit: &RateLimit) -> JsonValue {
    json!({
        "capacity": limit.capacity,
        "refillAmount": limit.refill_amount,
        "refillPeriodNanos": limit.refill_period.as_nanos().to_string(),
        "refillType": match limit.refill_type {
            RefillType::Greedy => "greedy",
            RefillType::Interval => "interval",
        },
    })
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistedBucket {
    version: u8,
    limits: Vec<PersistedLimit>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedLimit {
    scaled_tokens: String,
    last_refill_unix_nanos: String,
}

impl PersistedBucket {
    fn full(limits: &[RateLimit], now_nanos: u128) -> Self {
        Self {
            version: STATE_VERSION,
            limits: limits
                .iter()
                .map(|limit| PersistedLimit {
                    scaled_tokens: scaled_tokens(limit.capacity).to_string(),
                    last_refill_unix_nanos: now_nanos.to_string(),
                })
                .collect(),
        }
    }

    fn decode(bytes: &[u8], expected_limits: usize) -> Result<Self, String> {
        let state: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid persisted rate limit bucket: {error}"))?;
        if state.version != STATE_VERSION {
            return Err(format!(
                "unsupported persisted rate limit bucket version {}",
                state.version
            ));
        }
        if state.limits.len() != expected_limits {
            return Err("stored rate limit bucket has incompatible bandwidths".to_owned());
        }
        for limit in &state.limits {
            limit.values()?;
        }
        Ok(state)
    }

    fn consume(
        &mut self,
        config: &[RateLimit],
        tokens: u64,
        now_nanos: u128,
    ) -> Result<RateLimitDecision, String> {
        if self.limits.len() != config.len() {
            return Err("stored rate limit bucket has incompatible bandwidths".to_owned());
        }
        let requested = scaled_tokens(tokens);
        let mut current = Vec::with_capacity(self.limits.len());
        for (state, limit) in self.limits.iter_mut().zip(config) {
            current.push(state.refill(limit, now_nanos)?);
        }
        if current.iter().all(|tokens| *tokens >= requested) {
            for (state, tokens) in self.limits.iter_mut().zip(&mut current) {
                *tokens -= requested;
                state.scaled_tokens = tokens.to_string();
            }
            return Ok(RateLimitDecision::Allowed {
                remaining_tokens: minimum_whole_tokens(&current),
            });
        }
        let retry_after = current
            .iter()
            .zip(config)
            .zip(&self.limits)
            .filter(|((tokens, _), _)| **tokens < requested)
            .map(|((tokens, limit), state)| state.retry_after(*tokens, limit, requested, now_nanos))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(Duration::ZERO);
        Ok(RateLimitDecision::Denied {
            retry_after,
            remaining_tokens: minimum_whole_tokens(&current),
        })
    }
}

impl PersistedLimit {
    fn values(&self) -> Result<(u128, u128), String> {
        Ok((
            self.scaled_tokens
                .parse()
                .map_err(|_| "invalid persisted rate limit token count".to_owned())?,
            self.last_refill_unix_nanos
                .parse()
                .map_err(|_| "invalid persisted rate limit refill timestamp".to_owned())?,
        ))
    }

    fn refill(&mut self, limit: &RateLimit, now_nanos: u128) -> Result<u128, String> {
        let (mut tokens, last_refill) = self.values()?;
        let elapsed = now_nanos.saturating_sub(last_refill);
        let period = limit.refill_period.as_nanos();
        match limit.refill_type {
            RefillType::Greedy => {
                let added = elapsed
                    .saturating_mul(u128::from(limit.refill_amount))
                    .saturating_mul(TOKEN_SCALE)
                    / period;
                tokens = tokens
                    .saturating_add(added)
                    .min(scaled_tokens(limit.capacity));
                self.last_refill_unix_nanos = now_nanos.to_string();
            }
            RefillType::Interval => {
                let intervals = elapsed / period;
                if intervals > 0 {
                    tokens = tokens
                        .saturating_add(
                            intervals
                                .saturating_mul(u128::from(limit.refill_amount))
                                .saturating_mul(TOKEN_SCALE),
                        )
                        .min(scaled_tokens(limit.capacity));
                    self.last_refill_unix_nanos = last_refill
                        .saturating_add(period.saturating_mul(intervals))
                        .to_string();
                }
            }
        }
        self.scaled_tokens = tokens.to_string();
        Ok(tokens)
    }

    fn retry_after(
        &self,
        tokens: u128,
        limit: &RateLimit,
        requested: u128,
        now_nanos: u128,
    ) -> Result<Duration, String> {
        let (_, last_refill) = self.values()?;
        let missing = requested.saturating_sub(tokens);
        let nanos = match limit.refill_type {
            RefillType::Greedy => ceil_div(
                missing.saturating_mul(limit.refill_period.as_nanos()),
                u128::from(limit.refill_amount).saturating_mul(TOKEN_SCALE),
            ),
            RefillType::Interval => {
                let intervals = ceil_div(missing, scaled_tokens(limit.refill_amount));
                limit
                    .refill_period
                    .as_nanos()
                    .saturating_mul(intervals)
                    .saturating_sub(now_nanos.saturating_sub(last_refill))
            }
        };
        Ok(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }
}

fn minimum_whole_tokens(tokens: &[u128]) -> u64 {
    tokens
        .iter()
        .map(|tokens| u64::try_from(tokens / TOKEN_SCALE).unwrap_or(u64::MAX))
        .min()
        .unwrap_or(0)
}

fn scaled_tokens(tokens: u64) -> u128 {
    u128::from(tokens).saturating_mul(TOKEN_SCALE)
}

const fn ceil_div(numerator: u128, denominator: u128) -> u128 {
    numerator.saturating_add(denominator.saturating_sub(1)) / denominator
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_permissions::RateLimitBucketConfig;

    fn request() -> RateLimitRequest {
        RateLimitRequest {
            app_id: Uuid::new_v4(),
            bucket_name: "writes".to_owned(),
            config: RateLimitBucketConfig {
                limits: vec![
                    RateLimit {
                        capacity: 2,
                        refill_amount: 2,
                        refill_period: Duration::from_secs(10),
                        refill_type: RefillType::Greedy,
                    },
                    RateLimit {
                        capacity: 10,
                        refill_amount: 10,
                        refill_period: Duration::from_secs(60),
                        refill_type: RefillType::Interval,
                    },
                ],
            },
            bucket_key: json!({"email": "user@example.com"}),
            tokens: 1,
        }
    }

    #[tokio::test]
    async fn current_thread_rejects_before_enqueue() {
        let (sender, mut receiver) = channel(QUEUE_CAPACITY);
        let consumer = PostgresRateLimitConsumer { sender };
        assert!(
            consumer
                .consume(&request())
                .unwrap_err()
                .contains("multi-thread")
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn stalled_worker_does_not_block_caller_indefinitely() {
        let (sender, mut receiver) = channel(QUEUE_CAPACITY);
        let consumer = PostgresRateLimitConsumer { sender };
        let worker = std::thread::spawn(move || {
            let command = receiver.blocking_recv().unwrap();
            std::thread::sleep(Duration::from_secs(3));
            assert!(command.abandoned.load(Ordering::Acquire));
            let _ = command.reply.send(Ok(Vec::new()));
        });
        let started = std::time::Instant::now();
        let result = consumer.consume_all(&[request()]);
        let elapsed = started.elapsed();
        worker.join().unwrap();
        assert!(elapsed < Duration::from_millis(2500), "waited {elapsed:?}");
        assert!(result.unwrap_err().contains("deadline"));
    }

    #[test]
    fn caller_without_runtime_receives_decision_and_closed_worker_rejects() {
        let (sender, mut receiver) = channel(QUEUE_CAPACITY);
        let consumer = PostgresRateLimitConsumer { sender };
        let worker = std::thread::spawn(move || {
            let command = receiver.blocking_recv().unwrap();
            command
                .reply
                .send(Ok(vec![RateLimitDecision::Allowed {
                    remaining_tokens: 1,
                }]))
                .unwrap();
        });
        assert_eq!(
            consumer.consume(&request()).unwrap(),
            RateLimitDecision::Allowed {
                remaining_tokens: 1
            }
        );
        worker.join().unwrap();
        assert!(
            consumer
                .consume(&request())
                .unwrap_err()
                .contains("stopped")
        );
    }

    #[test]
    fn full_queue_rejects_without_waiting() {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let (reply, _response) = mpsc::sync_channel(1);
        sender
            .try_send(ConsumeCommand {
                kind: CommandKind::Consume,
                requests: vec![request()],
                reply,
                deadline: std::time::Instant::now() + REQUEST_TIMEOUT,
                abandoned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
            .unwrap();
        let consumer = PostgresRateLimitConsumer { sender };
        let started = std::time::Instant::now();
        assert!(consumer.consume(&request()).unwrap_err().contains("full"));
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn oversized_batch_rejects_before_enqueue() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let consumer = PostgresRateLimitConsumer { sender };
        assert!(
            consumer
                .consume_all(&vec![request(); MAX_BATCH_REQUESTS + 1])
                .unwrap_err()
                .contains("batch")
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_and_abandoned_commands_skip_database_acquisition() {
        let Some(url) = instant_test_support::test_database_url(
            "stale_and_abandoned_commands_skip_database_acquisition",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let source = PostgresClientSource::fixed(client);
        let PostgresClientSource::Fixed(shared) = &source else {
            unreachable!()
        };
        let held = shared.lock().await;
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let mut responses = Vec::new();
        for abandoned in [false, true] {
            let (reply, response) = mpsc::sync_channel(1);
            responses.push(response);
            sender
                .try_send(ConsumeCommand {
                    kind: CommandKind::Consume,
                    requests: vec![request()],
                    reply,
                    deadline: if abandoned {
                        std::time::Instant::now() + REQUEST_TIMEOUT
                    } else {
                        std::time::Instant::now()
                            .checked_sub(Duration::from_secs(1))
                            .unwrap()
                    },
                    abandoned: Arc::new(std::sync::atomic::AtomicBool::new(abandoned)),
                })
                .unwrap();
        }
        drop(sender);
        tokio::time::timeout(
            Duration::from_millis(100),
            run_worker(source.clone(), Arc::from(b"secret".as_slice()), receiver),
        )
        .await
        .expect("stale commands tried to acquire the held connection");
        for response in responses {
            assert!(
                response
                    .try_recv()
                    .unwrap()
                    .unwrap_err()
                    .contains("deadline")
            );
        }
        drop(held);
    }

    async fn postgres_test_batch() -> Option<(Client, ConsumeCommand)> {
        let Some(url) = instant_test_support::test_database_url("postgres_rate_limit_test_batch")
        else {
            return None;
        };
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        client
            .batch_execute(
                "CREATE TEMP TABLE rate_limit_keys (key uuid PRIMARY KEY, value bytea NOT NULL);
            SET lock_timeout = '9s'; SET statement_timeout = '10s'",
            )
            .await
            .unwrap();
        let (reply, _response) = mpsc::sync_channel(1);
        let command = ConsumeCommand {
            kind: CommandKind::Consume,
            requests: vec![request()],
            reply,
            deadline: Instant::now() + Duration::from_secs(10),
            abandoned: Arc::new(AtomicBool::new(false)),
        };
        client
            .batch_execute(
                "CREATE FUNCTION pg_temp.slow_rate_limit() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN PERFORM pg_sleep(2); RETURN NEW; END $$;
             CREATE TRIGGER slow_rate_limit BEFORE INSERT ON rate_limit_keys
             FOR EACH ROW EXECUTE FUNCTION pg_temp.slow_rate_limit()",
            )
            .await
            .unwrap();
        Some((client, command))
    }

    #[tokio::test]
    async fn postgres_deadlines_rollback_and_restore_session_settings() {
        let Some((mut client, command)) = postgres_test_batch().await else {
            return;
        };
        let url = instant_test_support::test_database_url(
            "postgres_deadlines_rollback_and_restore_session_settings",
        )
        .unwrap();
        let (mut locker, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let key = bucket_uuid(b"secret", &command.requests[0]).unwrap();
        let advisory_key = i64::from_be_bytes(key.as_bytes()[..8].try_into().unwrap());
        let lock = locker.transaction().await.unwrap();
        lock.query_one("SELECT pg_advisory_xact_lock($1)", &[&advisory_key])
            .await
            .unwrap();
        let started = Instant::now();
        let error = consume_postgres_batch(&mut client, b"secret", &command)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<tokio_postgres::Error>()
                .unwrap()
                .code(),
            Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        lock.rollback().await.unwrap();

        let error = consume_postgres_batch(&mut client, b"secret", &command)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<tokio_postgres::Error>()
                .unwrap()
                .code(),
            Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
        );
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM rate_limit_keys", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        assert_eq!(
            client
                .query_one("SHOW lock_timeout", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "9s"
        );
        assert_eq!(
            client
                .query_one("SHOW statement_timeout", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "10s"
        );
    }

    #[tokio::test]
    async fn stale_or_abandoned_batch_rolls_back_before_commit() {
        let Some((mut client, mut command)) = postgres_test_batch().await else {
            return;
        };
        client.batch_execute("CREATE OR REPLACE FUNCTION pg_temp.slow_rate_limit() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN PERFORM pg_sleep(0.1); RETURN NEW; END $$").await.unwrap();
        command.deadline = Instant::now() + Duration::from_millis(20);
        assert!(
            consume_postgres_batch(&mut client, b"secret", &command)
                .await
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM rate_limit_keys", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );

        command.deadline = Instant::now() + REQUEST_TIMEOUT;
        let abandoned = Arc::clone(&command.abandoned);
        let abandon = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            abandoned.store(true, Ordering::Release);
        });
        assert!(
            consume_postgres_batch(&mut client, b"secret", &command)
                .await
                .unwrap_err()
                .to_string()
                .contains("abandoned")
        );
        abandon.await.unwrap();
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM rate_limit_keys", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        command.abandoned.store(false, Ordering::Release);
        assert!(matches!(
            consume_postgres_batch(&mut client, b"secret", &command)
                .await
                .unwrap()[0],
            RateLimitDecision::Allowed { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_transaction_discards_pool_lease_and_worker_recovers() {
        timed_out_lease_recovers(false).await;
    }

    #[tokio::test]
    async fn timed_out_fixed_connection_stops_instead_of_reusing_transaction() {
        let Some((client, mut command)) = postgres_test_batch().await else {
            return;
        };
        let (sender, receiver) = channel(QUEUE_CAPACITY);
        let (reply, response) = mpsc::sync_channel(1);
        command.reply = reply;
        command.deadline = Instant::now() + Duration::from_millis(100);
        sender.try_send(command).unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            run_worker(
                PostgresClientSource::fixed(client),
                Arc::from(b"secret".as_slice()),
                receiver,
            ),
        )
        .await
        .expect("fixed worker did not stop after timeout");
        assert!(
            response
                .try_recv()
                .unwrap()
                .unwrap_err()
                .contains("deadline")
        );
        assert!(sender.is_closed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_commit_is_not_replayed_and_discards_pool_lease() {
        timed_out_lease_recovers(true).await;
    }

    async fn timed_out_lease_recovers(during_commit: bool) {
        let request = request();
        let Some(url) = instant_test_support::test_database_url("timed_out_lease_recovers") else {
            return;
        };
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let client = pool.acquire().await.unwrap();
        let old_pid = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        client
            .batch_execute(
                "CREATE TEMP TABLE rate_limit_keys (key uuid PRIMARY KEY, value bytea NOT NULL);
            CREATE FUNCTION pg_temp.slow_rate_limit() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN PERFORM pg_sleep(0.5); RETURN NEW; END $$",
            )
            .await
            .unwrap();
        let trigger = if during_commit {
            "CREATE CONSTRAINT TRIGGER slow_rate_limit AFTER INSERT ON rate_limit_keys
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION pg_temp.slow_rate_limit()"
        } else {
            "CREATE TRIGGER slow_rate_limit BEFORE INSERT ON rate_limit_keys
             FOR EACH ROW EXECUTE FUNCTION pg_temp.slow_rate_limit()"
        };
        client.batch_execute(trigger).await.unwrap();
        drop(client);
        let (sender, receiver) = channel(QUEUE_CAPACITY);
        let worker = tokio::spawn(run_worker(
            PostgresClientSource::pooled(pool.clone()),
            Arc::from(b"secret".as_slice()),
            receiver,
        ));
        let (reply, response) = mpsc::sync_channel(1);
        sender
            .try_send(ConsumeCommand {
                kind: CommandKind::Consume,
                requests: vec![request.clone()],
                reply,
                deadline: Instant::now() + Duration::from_millis(100),
                abandoned: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        let error = tokio::task::spawn_blocking(move || {
            response
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
        })
        .await
        .unwrap();
        assert!(error.contains("outcome may be unknown"));
        let replacement = pool.acquire().await.unwrap();
        let new_pid = replacement
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert_ne!(old_pid, new_pid, "timed-out transaction was recycled");
        tokio::time::timeout(Duration::from_secs(2), async {
            while replacement
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                    &[&old_pid],
                )
                .await
                .unwrap()
                .get::<_, bool>(0)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed-out backend did not release its transaction");
        assert_eq!(
            pool.acquisition_count(),
            4,
            "worker retried the timed-out command"
        );
        replacement
            .batch_execute(
                "CREATE TEMP TABLE rate_limit_keys (key uuid PRIMARY KEY, value bytea NOT NULL)",
            )
            .await
            .unwrap();
        drop(replacement);
        let consumer = PostgresRateLimitConsumer { sender };
        assert!(matches!(
            consumer.consume(&request).unwrap(),
            RateLimitDecision::Allowed { .. }
        ));
        drop(consumer);
        worker.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_acquisition_deadline_does_not_consume_or_stop_worker() {
        let request = request();
        let Some(url) = instant_test_support::test_database_url(
            "pool_acquisition_deadline_does_not_consume_or_stop_worker",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let held = pool.acquire().await.unwrap();
        held.batch_execute(
            "CREATE TEMP TABLE rate_limit_keys (key uuid PRIMARY KEY, value bytea NOT NULL)",
        )
        .await
        .unwrap();
        let (sender, receiver) = channel(QUEUE_CAPACITY);
        let worker = tokio::spawn(run_worker(
            PostgresClientSource::pooled(pool.clone()),
            Arc::from(b"secret".as_slice()),
            receiver,
        ));
        let (reply, response) = mpsc::sync_channel(1);
        sender
            .try_send(ConsumeCommand {
                kind: CommandKind::Consume,
                requests: vec![request.clone()],
                reply,
                deadline: Instant::now() + Duration::from_millis(50),
                abandoned: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        assert!(
            tokio::task::spawn_blocking(move || response
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err())
            .await
            .unwrap()
            .contains("deadline")
        );
        assert_eq!(pool.status().waiting, 0);
        assert_eq!(
            held.query_one("SELECT count(*) FROM rate_limit_keys", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        drop(held);
        let consumer = PostgresRateLimitConsumer { sender };
        assert!(matches!(
            consumer.consume(&request).unwrap(),
            RateLimitDecision::Allowed {
                remaining_tokens: 1
            }
        ));
        drop(consumer);
        worker.await.unwrap();
    }

    #[test]
    fn durable_state_consumes_refills_and_round_trips() {
        let request = request();
        let mut state = PersistedBucket::full(&request.config.limits, 1_000_000_000);
        assert_eq!(
            state
                .consume(&request.config.limits, 2, 1_000_000_000)
                .unwrap(),
            RateLimitDecision::Allowed {
                remaining_tokens: 0
            }
        );
        let bytes = serde_json::to_vec(&state).unwrap();
        let mut restored = PersistedBucket::decode(&bytes, 2).unwrap();
        assert!(matches!(
            restored
                .consume(&request.config.limits, 1, 1_000_000_000)
                .unwrap(),
            RateLimitDecision::Denied { .. }
        ));
        assert_eq!(
            restored
                .consume(&request.config.limits, 1, 6_000_000_000)
                .unwrap(),
            RateLimitDecision::Allowed {
                remaining_tokens: 0
            }
        );
    }

    #[test]
    fn hmac_keys_are_stable_scoped_and_do_not_expose_bucket_values() {
        let request = request();
        let first = bucket_uuid(b"secret", &request).unwrap();
        assert_eq!(first, bucket_uuid(b"secret", &request).unwrap());
        let mut other = request;
        other.bucket_key = json!({"email": "other@example.com"});
        assert_ne!(first, bucket_uuid(b"secret", &other).unwrap());
        assert_ne!(first, bucket_uuid(b"other-secret", &other).unwrap());
    }
}
