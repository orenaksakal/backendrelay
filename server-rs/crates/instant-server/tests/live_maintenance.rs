use instant_server::{PostgresEphemeralAppSweeper, PostgresPool, PostgresRateLimitSweeper};
use uuid::Uuid;

#[tokio::test]
async fn rate_limit_sweep_honors_age_batches_and_loop_bounds() {
    let Some(url) = instant_test_support::test_database_url(
        "rate_limit_sweep_honors_age_batches_and_loop_bounds",
    ) else {
        return;
    };
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let client = pool.acquire().await.unwrap();
    client
        .batch_execute(
            "CREATE TEMP TABLE rate_limit_keys (
               key uuid PRIMARY KEY,
               value bytea NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();

    let keys = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    client
        .execute(
            "INSERT INTO rate_limit_keys (key, value, updated_at) VALUES
               ($1, '\\x01', now() - interval '3 days'),
               ($2, '\\x02', now() - interval '2 days 1 minute'),
               ($3, '\\x03', now() - interval '1 day')",
            &[&keys[0], &keys[1], &keys[2]],
        )
        .await
        .unwrap();
    drop(client);
    let rate_sweeper = PostgresRateLimitSweeper::from_pool(pool.clone());
    let limited = rate_sweeper.sweep(1, 1).await.unwrap();
    assert_eq!(limited.deleted, 1);
    assert_eq!(limited.loops, 1);
    assert!(limited.reached_loop_limit);
    let drained = rate_sweeper.sweep(10, 1).await.unwrap();
    assert_eq!(drained.deleted, 1);
    assert_eq!(drained.loops, 2);
    assert!(!drained.reached_loop_limit);
    let client = pool.acquire().await.unwrap();
    let remaining: Vec<Uuid> = client
        .query("SELECT key FROM rate_limit_keys", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(remaining, vec![keys[2]]);
}

#[tokio::test]
async fn ephemeral_sweep_honors_creator_age_soft_delete_and_cascade() {
    let Some(url) = instant_test_support::test_database_url(
        "ephemeral_sweep_honors_creator_age_soft_delete_and_cascade",
    ) else {
        return;
    };
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let client = pool.acquire().await.unwrap();
    client
        .batch_execute(
            "CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid,
               created_at timestamp NOT NULL DEFAULT now(),
               deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_children (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE
             );",
        )
        .await
        .unwrap();
    let ephemeral_creator = Uuid::new_v4();
    let other_creator = Uuid::new_v4();
    let apps = [
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    ];
    client
        .execute(
            "INSERT INTO apps (id, creator_id, created_at, deletion_marked_at) VALUES
               ($1, $5, now() - interval '16 days', NULL),
               ($2, $5, now() - interval '14 days 23 hours', NULL),
               ($3, $5, now() - interval '16 days', now()),
               ($4, $6, now() - interval '16 days', NULL)",
            &[
                &apps[0],
                &apps[1],
                &apps[2],
                &apps[3],
                &ephemeral_creator,
                &other_creator,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_children (id, app_id) VALUES ($1, $2)",
            &[&Uuid::new_v4(), &apps[0]],
        )
        .await
        .unwrap();
    drop(client);
    let ephemeral_sweeper = PostgresEphemeralAppSweeper::from_pool(pool.clone());
    let outcome = ephemeral_sweeper
        .sweep_expired(ephemeral_creator)
        .await
        .unwrap();
    assert_eq!(outcome.deleted_app_ids, vec![apps[0]]);
    let client = pool.acquire().await.unwrap();
    let remaining: i64 = client
        .query_one("SELECT count(*) FROM apps", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(remaining, 3);
    let children: i64 = client
        .query_one("SELECT count(*) FROM app_children", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(children, 0);
}
