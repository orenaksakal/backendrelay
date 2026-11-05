use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{NaiveDate, TimeZone as _, Utc};
use instant_server::{
    DailyMetricsFailoverGate, DailyMetricsGateFuture, DailyMetricsRun, PostgresDailyMetricsWorker,
};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

struct ToggleGate {
    failing_over: AtomicBool,
}

impl DailyMetricsFailoverGate for ToggleGate {
    fn is_failing_over(&self) -> DailyMetricsGateFuture<'_> {
        Box::pin(std::future::ready(Ok(self
            .failing_over
            .load(Ordering::Relaxed))))
    }
}

async fn connect(database_url: &str) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, task)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn daily_metrics_gates_backfills_and_deduplicates() {
    let Some(database_url) =
        instant_test_support::test_database_url("daily_metrics_gates_backfills_and_deduplicates")
    else {
        return;
    };
    let (setup, setup_task) = connect(&database_url).await;
    let schema = format!("daily_metrics_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.grabs (id text PRIMARY KEY);
             CREATE TABLE {schema}.transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL, created_at timestamp NOT NULL
             );
             CREATE TABLE {schema}.daily_app_transactions (
               date date, app_id uuid, active_date timestamp, is_active boolean, count bigint,
               UNIQUE (date, app_id, is_active)
             );"
        ))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.transactions (app_id, created_at)
                 SELECT $1, day
                 FROM generate_series(
                   current_date - interval '75 days',
                   current_date - interval '1 day',
                   interval '1 day'
                 ) day"
            ),
            &[&app_id],
        )
        .await
        .unwrap();
    let database_date: NaiveDate = setup
        .query_one("SELECT current_date", &[])
        .await
        .unwrap()
        .get(0);
    let local_run = database_date.and_hms_opt(9, 0, 0).unwrap();
    let run_at = chrono_tz::US::Pacific
        .from_local_datetime(&local_run)
        .single()
        .unwrap()
        .with_timezone(&Utc);

    let (worker_client, worker_task) = connect(&database_url).await;
    worker_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let gate = Arc::new(ToggleGate {
        failing_over: AtomicBool::new(true),
    });
    let worker = PostgresDailyMetricsWorker::new(worker_client, gate.clone());

    assert_eq!(
        worker.run_at(run_at).await.unwrap(),
        DailyMetricsRun::SkippedFailover
    );
    gate.failing_over.store(false, Ordering::Relaxed);
    let previous_run_at = run_at.checked_sub_days(chrono::Days::new(1)).unwrap();
    assert_eq!(
        worker.run_at(previous_run_at).await.unwrap(),
        DailyMetricsRun::Completed {
            inserted_activity_rows: 75,
        }
    );
    assert_eq!(
        worker.run_at(run_at).await.unwrap(),
        DailyMetricsRun::Completed {
            inserted_activity_rows: 0,
        }
    );
    assert_eq!(
        worker.run_at(run_at).await.unwrap(),
        DailyMetricsRun::AlreadyClaimed
    );

    let activity = setup
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS total,
                        count(*) FILTER (WHERE NOT is_active)::bigint AS inactive,
                        min(active_date)::date AS first_active
                 FROM {schema}.daily_app_transactions"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(activity.get::<_, i64>("total"), 75);
    assert_eq!(activity.get::<_, i64>("inactive"), 8);
    assert_eq!(
        activity.get::<_, NaiveDate>("first_active"),
        database_date
            .checked_sub_days(chrono::Days::new(68))
            .unwrap()
    );

    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    worker_task.abort();
    setup_task.abort();
}
