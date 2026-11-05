use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};

pub type DatabaseQueryThroughputFuture<'a> = Pin<Box<dyn Future<Output = usize> + Send + 'a>>;

/// Supplies the recent database-query throughput shown by the operator
/// active-session endpoint.
pub trait DatabaseQueryThroughput: Send + Sync + 'static {
    fn total_queries(&self) -> DatabaseQueryThroughputFuture<'_>;
}

/// Non-blocking observation boundary for the JVM-compatible `CloudWatch`
/// webhook-history latency metric.
pub trait WebhookLatencyRecorder: Send + Sync + 'static {
    fn record(&self, tx_created_at: DateTime<Utc>);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopWebhookLatencyRecorder;

impl WebhookLatencyRecorder for NoopWebhookLatencyRecorder {
    fn record(&self, _tx_created_at: DateTime<Utc>) {}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AwsWebhookLatencyRecorder;

impl AwsWebhookLatencyRecorder {
    #[must_use]
    pub fn new(_config: &()) -> Self {
        Self
    }
}

impl WebhookLatencyRecorder for AwsWebhookLatencyRecorder {
    fn record(&self, _tx_created_at: DateTime<Utc>) {}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AwsDatabaseQueryThroughput;

impl AwsDatabaseQueryThroughput {
    #[must_use]
    pub fn new(_config: &()) -> Self {
        Self
    }
}

impl DatabaseQueryThroughput for AwsDatabaseQueryThroughput {
    fn total_queries(&self) -> DatabaseQueryThroughputFuture<'_> {
        Box::pin(std::future::ready(0))
    }
}
