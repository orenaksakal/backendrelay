use std::sync::Arc;
use std::time::Duration;

use instant_permissions::{
    RateLimit, RateLimitBucketConfig, RateLimitConsumer, RateLimitDecision, RateLimitRequest,
    RefillType,
};
use instant_server::{PostgresPool, PostgresRateLimitConsumer};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

async fn client(database_url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_rate_limit_is_atomic_and_shared_across_server_instances() {
    let Some(url) = instant_test_support::test_database_url(
        "postgres_rate_limit_is_atomic_and_shared_across_server_instances",
    ) else {
        return;
    };
    let first = client(&url).await;
    if !first
        .query_one("SELECT to_regclass('rate_limit_keys') IS NOT NULL", &[])
        .await
        .unwrap()
        .get::<_, bool>(0)
    {
        return;
    }
    drop(first);
    let first_pool = PostgresPool::connect(&url, 1).await.unwrap();
    let second_pool = PostgresPool::connect(&url, 1).await.unwrap();
    let key: Arc<[u8]> = Arc::from(b"live-test-rate-limit-secret".as_slice());
    let first: Arc<dyn RateLimitConsumer> = Arc::new(PostgresRateLimitConsumer::from_pool(
        first_pool,
        Arc::clone(&key),
    ));
    let second: Arc<dyn RateLimitConsumer> =
        Arc::new(PostgresRateLimitConsumer::from_pool(second_pool, key));
    let request = RateLimitRequest {
        app_id: Uuid::new_v4(),
        bucket_name: "cross-process-contract".to_owned(),
        config: RateLimitBucketConfig {
            limits: vec![RateLimit {
                capacity: 1,
                refill_amount: 1,
                refill_period: Duration::from_hours(24),
                refill_type: RefillType::Interval,
            }],
        },
        bucket_key: json!({"actor": Uuid::new_v4()}),
        tokens: 1,
    };

    let first_request = request.clone();
    let first_consumer = Arc::clone(&first);
    let first_attempt = tokio::task::spawn_blocking(move || first_consumer.consume(&first_request));
    let second_request = request.clone();
    let second_consumer = Arc::clone(&second);
    let second_attempt =
        tokio::task::spawn_blocking(move || second_consumer.consume(&second_request));
    let decisions = [
        first_attempt.await.unwrap().unwrap(),
        second_attempt.await.unwrap().unwrap(),
    ];
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| matches!(decision, RateLimitDecision::Allowed { .. }))
            .count(),
        1
    );
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| matches!(decision, RateLimitDecision::Denied { .. }))
            .count(),
        1
    );

    assert!(matches!(
        second.consume(&request).unwrap(),
        RateLimitDecision::Denied {
            remaining_tokens: 0,
            ..
        }
    ));

    let fresh = RateLimitRequest {
        app_id: Uuid::new_v4(),
        bucket_name: "atomic-batch-fresh".to_owned(),
        bucket_key: json!(null),
        ..request.clone()
    };
    let exhausted = RateLimitRequest {
        app_id: Uuid::new_v4(),
        bucket_name: "atomic-batch-exhausted".to_owned(),
        bucket_key: json!(null),
        ..request
    };
    assert!(matches!(
        first.consume(&exhausted).unwrap(),
        RateLimitDecision::Allowed { .. }
    ));
    let decisions = first.consume_all(&[fresh.clone(), exhausted]).unwrap();
    assert!(
        decisions
            .iter()
            .any(|decision| matches!(decision, RateLimitDecision::Denied { .. }))
    );
    assert!(matches!(
        first.consume(&fresh).unwrap(),
        RateLimitDecision::Allowed { .. }
    ));
}
