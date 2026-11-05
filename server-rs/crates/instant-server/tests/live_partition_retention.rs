use chrono::{TimeZone, Utc};
use instant_server::{
    PostgresPartitionRetention, PostgresPool, history_partition_bucket,
    history_partitions_to_truncate, wal_partitions_to_truncate,
};

fn partition_fixture_sql() -> String {
    let mut sql = String::from(
        "CREATE TEMP TABLE wal_logs (bucket integer NOT NULL) PARTITION BY RANGE (bucket);",
    );
    for partition in 0..8 {
        let _ = write!(
            sql,
            "CREATE TEMP TABLE wal_logs_{partition} PARTITION OF wal_logs \
             FOR VALUES FROM ({partition}) TO ({});",
            partition + 1
        );
    }
    sql.push_str(
        "CREATE TEMP TABLE history (bucket integer NOT NULL) PARTITION BY RANGE (bucket);
         CREATE TEMP TABLE webhook_events (bucket integer NOT NULL) PARTITION BY RANGE (bucket);",
    );
    for partition in 0..13 {
        let _ = write!(
            sql,
            "CREATE TEMP TABLE history_{partition} PARTITION OF history \
             FOR VALUES FROM ({partition}) TO ({}); \
             CREATE TEMP TABLE webhook_events_{partition} PARTITION OF webhook_events \
             FOR VALUES FROM ({partition}) TO ({});",
            partition + 1,
            partition + 1
        );
    }
    sql.push_str(
        "INSERT INTO wal_logs SELECT generate_series(0, 7);
         INSERT INTO history SELECT generate_series(0, 12);
         INSERT INTO webhook_events SELECT generate_series(0, 12);",
    );
    sql
}

#[tokio::test]
async fn retention_worker_truncates_only_the_selected_physical_partitions() {
    let Some(database_url) = instant_test_support::test_database_url(
        "retention_worker_truncates_only_the_selected_physical_partitions",
    ) else {
        return;
    };
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let client = pool.acquire().await.unwrap();
    client
        .batch_execute(&partition_fixture_sql())
        .await
        .unwrap();
    drop(client);

    let now = Utc.with_ymd_and_hms(2026, 8, 28, 5, 0, 0).unwrap();
    let expected_wal = wal_partitions_to_truncate(now);
    let expected_history = history_partitions_to_truncate(now, 120);
    let worker =
        PostgresPartitionRetention::from_pool(pool.clone()).with_history_retention_days(120);
    let wal = worker.truncate_wal(now).await.unwrap();
    assert_eq!(wal.wal_partitions, expected_wal);
    let history = worker.truncate_history(now).await.unwrap();
    assert_eq!(history.history_partitions, expected_history);

    let client = pool.acquire().await.unwrap();
    let wal_buckets = client
        .query("SELECT bucket FROM wal_logs ORDER BY bucket", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<_, i32>(0))
        .collect::<Vec<_>>();
    assert_eq!(wal_buckets.len(), 6);
    for partition in expected_wal {
        assert!(!wal_buckets.contains(&i32::from(partition)));
    }

    let history_buckets = client
        .query("SELECT bucket FROM history ORDER BY bucket", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<_, i32>(0))
        .collect::<Vec<_>>();
    let webhook_buckets = client
        .query("SELECT bucket FROM webhook_events ORDER BY bucket", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<_, i32>(0))
        .collect::<Vec<_>>();
    assert_eq!(history_buckets, webhook_buckets);
    assert_eq!(history_buckets.len(), 4);
    let current = history_partition_bucket(now);
    let expected_kept = [
        current,
        (current + 10) % 13,
        (current + 11) % 13,
        (current + 12) % 13,
    ];
    for partition in expected_kept {
        assert!(history_buckets.contains(&i32::from(partition)));
    }
}
use std::fmt::Write as _;
