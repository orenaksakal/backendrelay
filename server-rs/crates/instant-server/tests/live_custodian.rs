use std::sync::Arc;

use chrono::{Duration, Utc};
use instant_pg::{create_logical_replication_slot, drop_logical_replication_slot};
use instant_server::{CustodianTickOutcome, PostgresCustodian, PostgresPool};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use uuid::Uuid;

fn scoped_url(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}%2Cpublic")
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn custodian_executes_dependency_chains_and_aborts_restored_plans() {
    let Some(url) = instant_test_support::test_database_url(
        "custodian_executes_dependency_chains_and_aborts_restored_plans",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("custodian_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public"
        ))
        .await
        .unwrap();
    client
        .batch_execute(
            "CREATE TABLE apps (
               id uuid PRIMARY KEY, deletion_marked_at timestamptz
             );
             CREATE TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               deletion_marked_at timestamptz
             );
             CREATE TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE
             );
             CREATE TABLE triples (
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               attr_id uuid NOT NULL REFERENCES attrs(id) ON DELETE CASCADE,
               entity_id uuid NOT NULL, created_at bigint NOT NULL
             );
             CREATE TABLE custodian (
               id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               attr_id uuid REFERENCES attrs(id) ON DELETE CASCADE,
               type text NOT NULL,
               depends_on uuid REFERENCES custodian(id) ON DELETE SET NULL,
               worker_id text, status text NOT NULL DEFAULT 'waiting',
               attempts integer NOT NULL DEFAULT 0, error text,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               UNIQUE NULLS NOT DISTINCT (app_id, type, attr_id)
             );",
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let attr_a = Uuid::new_v4();
    let attr_b = Uuid::new_v4();
    client
        .execute("INSERT INTO apps VALUES ($1, NULL)", &[&app_id])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO attrs VALUES
               ($1, $3, now() - interval '3 days'), ($2, $3, NULL)",
            &[&attr_a, &attr_b, &app_id],
        )
        .await
        .unwrap();
    for index in 0..5_i64 {
        client
            .execute(
                "INSERT INTO triples VALUES ($1, $2, $3, $4)",
                &[&app_id, &attr_a, &Uuid::new_v4(), &index],
            )
            .await
            .unwrap();
    }
    let client = Arc::new(Mutex::new(client));
    let pool = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let doomed = pool.acquire().await.unwrap();
    let doomed_pid = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    {
        let database = client.lock().await;
        assert!(
            database
                .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
    }
    drop(doomed);
    let custodian = PostgresCustodian::from_pool(pool.clone(), "custodian-test").with_limits(2, 1);
    assert!(
        custodian
            .enqueue_attr_deletion(app_id, attr_a)
            .await
            .unwrap()
    );
    assert!(
        !custodian
            .enqueue_attr_deletion(app_id, attr_a)
            .await
            .unwrap()
    );
    assert!(matches!(
        custodian.tick(i64::MAX).await.unwrap(),
        CustodianTickOutcome::CompletedStep { ref kind, .. } if kind == "triples"
    ));
    assert!(matches!(
        custodian.tick(i64::MAX).await.unwrap(),
        CustodianTickOutcome::CompletedStep { ref kind, .. } if kind == "attr"
    ));
    assert_eq!(
        custodian.tick(i64::MAX).await.unwrap(),
        CustodianTickOutcome::Idle
    );

    let restored_attr = Uuid::new_v4();
    {
        let database = client.lock().await;
        database
            .execute(
                "INSERT INTO attrs VALUES ($1, $2, now() - interval '3 days')",
                &[&restored_attr, &app_id],
            )
            .await
            .unwrap();
    }
    assert!(
        custodian
            .enqueue_attr_deletion(app_id, restored_attr)
            .await
            .unwrap()
    );
    {
        let database = client.lock().await;
        database
            .execute(
                "UPDATE attrs SET deletion_marked_at = NULL WHERE id = $1",
                &[&restored_attr],
            )
            .await
            .unwrap();
    }
    assert_eq!(
        custodian.tick(i64::MAX).await.unwrap(),
        CustodianTickOutcome::AbortedRestoredPlan {
            app_id,
            attr_id: Some(restored_attr)
        }
    );

    {
        let database = client.lock().await;
        database
            .execute(
                "UPDATE apps SET deletion_marked_at = now() - interval '3 days' WHERE id = $1",
                &[&app_id],
            )
            .await
            .unwrap();
        for _ in 0..3 {
            database
                .execute("INSERT INTO transactions (app_id) VALUES ($1)", &[&app_id])
                .await
                .unwrap();
        }
        for index in 0..3_i64 {
            database
                .execute(
                    "INSERT INTO triples VALUES ($1, $2, $3, $4)",
                    &[&app_id, &attr_b, &Uuid::new_v4(), &index],
                )
                .await
                .unwrap();
        }
    }
    assert!(custodian.enqueue_app_deletion(app_id).await.unwrap());
    for kind in ["triples", "transactions", "attrs", "app"] {
        assert!(matches!(
            custodian.tick(i64::MAX).await.unwrap(),
            CustodianTickOutcome::CompletedStep { kind: actual, .. } if actual == kind
        ));
    }
    assert_eq!(
        custodian.tick(i64::MAX).await.unwrap(),
        CustodianTickOutcome::Idle
    );

    let reaper_app = Uuid::new_v4();
    let reaper_attr = Uuid::new_v4();
    {
        let database = client.lock().await;
        database
            .execute(
                "INSERT INTO apps VALUES ($1, now() - interval '3 days')",
                &[&reaper_app],
            )
            .await
            .unwrap();
        database
            .execute(
                "INSERT INTO attrs VALUES ($1, $2, now() - interval '3 days')",
                &[&reaper_attr, &reaper_app],
            )
            .await
            .unwrap();
    }
    let enqueued = custodian
        .enqueue_due_deletions(Utc::now(), 2)
        .await
        .unwrap();
    assert_eq!(enqueued.attrs_enqueued, 1);
    assert_eq!(enqueued.apps_enqueued, 1);
    let stale = custodian
        .reap_stuck(Utc::now() + Duration::minutes(3))
        .await
        .unwrap();
    assert!(stale.is_empty());
    let replacement = pool.acquire().await.unwrap();
    let row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(row.get::<_, i32>(0), doomed_pid);
    assert_eq!(row.get::<_, String>(1), schema);
    drop(replacement);
    drop(custodian);
    drop(pool);
    client
        .lock()
        .await
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn lag_counts_only_active_or_live_machine_invalidator_slots() {
    let Some(url) = instant_test_support::test_database_url(
        "lag_counts_only_active_or_live_machine_invalidator_slots",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let machine_id = Uuid::new_v4();
    let slot_name = format!("invalidator_{}", machine_id.simple());
    client
        .execute(
            "INSERT INTO rust_realtime_machines (machine_id, updated_at)
             VALUES ($1, now() - interval '2 minutes')",
            &[&machine_id],
        )
        .await
        .unwrap();
    create_logical_replication_slot(&client, &slot_name, false)
        .await
        .unwrap();
    client
        .query_one(
            "SELECT pg_logical_emit_message(true, 'custodian-lag-test', repeat('x', 4096))",
            &[],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let custodian = PostgresCustodian::from_pool(pool.clone(), "custodian-lag-test");
    assert_eq!(custodian.max_replication_lag_bytes().await.unwrap(), 0);

    client
        .execute(
            "UPDATE rust_realtime_machines SET updated_at = now() WHERE machine_id = $1",
            &[&machine_id],
        )
        .await
        .unwrap();
    assert!(custodian.max_replication_lag_bytes().await.unwrap() > 0);

    drop(custodian);
    drop(pool);
    drop_logical_replication_slot(&client, &slot_name)
        .await
        .unwrap();
    client
        .execute(
            "DELETE FROM rust_realtime_machines WHERE machine_id = $1",
            &[&machine_id],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn prune_transactions_deletes_only_rows_beyond_retention_in_batches() {
    let Some(url) = instant_test_support::test_database_url(
        "prune_transactions_deletes_only_rows_beyond_retention_in_batches",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("custodian_prune_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public;
             CREATE TABLE apps (id uuid PRIMARY KEY);
             CREATE TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               created_at timestamptz NOT NULL DEFAULT now()
             );"
        ))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    client
        .execute("INSERT INTO apps VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    for age_days in [45_i64, 40, 31] {
        client
            .execute(
                "INSERT INTO transactions (app_id, created_at)
                 VALUES ($1, now() - $2::bigint * interval '1 day')",
                &[&app_id, &age_days],
            )
            .await
            .unwrap();
    }
    for age_days in [5_i64, 1] {
        client
            .execute(
                "INSERT INTO transactions (app_id, created_at)
                 VALUES ($1, now() - $2::bigint * interval '1 day')",
                &[&app_id, &age_days],
            )
            .await
            .unwrap();
    }
    // An idle app keeps its newest marker even when every row is past the
    // window, so its sync and startup watermark never resets to zero.
    let idle_app = Uuid::new_v4();
    client
        .execute("INSERT INTO apps VALUES ($1)", &[&idle_app])
        .await
        .unwrap();
    for age_days in [60_i64, 50] {
        client
            .execute(
                "INSERT INTO transactions (app_id, created_at)
                 VALUES ($1, now() - $2::bigint * interval '1 day')",
                &[&idle_app, &age_days],
            )
            .await
            .unwrap();
    }
    let pool = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let custodian = PostgresCustodian::from_pool(pool.clone(), "custodian-prune-test");
    let now = Utc::now();
    assert_eq!(
        custodian.prune_transactions(now, 30, 1).await.unwrap(),
        4,
        "each batch should stop at the requested size"
    );
    assert_eq!(custodian.prune_transactions(now, 30, 1).await.unwrap(), 0);
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        3
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&idle_app],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
}
