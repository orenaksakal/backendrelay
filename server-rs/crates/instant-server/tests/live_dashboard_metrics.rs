use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration, Utc};
use instant_instaql::system_attr_id;
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DatabaseQueryThroughput,
    DatabaseQueryThroughputFuture, LocalRuntimeSessionReportCollector, NoopDashboardControlHooks,
    PostgresDashboardMetricsHooks, PostgresRuntimeSessionReportCollector,
    RuntimeLocalAppSessionReport, RuntimeSessionReportCollector, RuntimeSessionReporter,
};
use serde_json::{Value, json};
use tokio_postgres::NoTls;
use uuid::Uuid;

struct SessionReporter {
    machine_id: Uuid,
    app_id: Uuid,
}

struct QueryThroughput;

impl DatabaseQueryThroughput for QueryThroughput {
    fn total_queries(&self) -> DatabaseQueryThroughputFuture<'_> {
        Box::pin(std::future::ready(19))
    }
}

impl RuntimeSessionReporter for SessionReporter {
    fn machine_id(&self) -> Uuid {
        self.machine_id
    }

    fn local_session_report(&self) -> HashMap<Uuid, RuntimeLocalAppSessionReport> {
        HashMap::from([(
            self.app_id,
            RuntimeLocalAppSessionReport {
                count: 2,
                origins: HashMap::from([("https://app.example.com".to_owned(), 2)]),
            },
        )])
    }

    fn local_session_count(&self) -> usize {
        2
    }

    fn local_query_count(&self) -> usize {
        7
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn operator_table_metrics_match_recent_top_and_storage_contracts() {
    let Some(database_url) = instant_test_support::test_database_url(
        "operator_table_metrics_match_recent_top_and_storage_contracts",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let schema = format!("dashboard_metrics_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL, created_at timestamptz NOT NULL);
             CREATE TABLE instant_profiles (id uuid PRIMARY KEY, meta jsonb, created_at timestamptz);
             CREATE TABLE orgs (id uuid PRIMARY KEY, title text);
             CREATE TABLE org_members (org_id uuid, user_id uuid, role text, created_at timestamptz);
             CREATE TABLE apps (id uuid PRIMARY KEY, title text, creator_id uuid, org_id uuid, created_at timestamptz NOT NULL);
             CREATE TABLE daily_app_transactions (app_id uuid, date timestamptz, count bigint, is_active boolean NOT NULL DEFAULT true);
             CREATE TABLE attr_sketches (app_id uuid, attr_id uuid, total bigint);
             CREATE TABLE triples_size_aggregate (app_id uuid, attr_id uuid, files_size bigint, pg_size bigint);"
        ))
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    let excluded_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let excluded_app = Uuid::new_v4();
    let now = Utc::now();
    client
        .execute(
            "INSERT INTO instant_users VALUES ($1, 'user@example.com', $3), ($2, 'internal@example.com', $3)",
            &[&user_id, &excluded_id, &now],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO daily_app_transactions (app_id, date, count, is_active)
             SELECT $1, day, 1, true
             FROM generate_series(
               date_trunc('day', now()) - interval '75 days',
               date_trunc('day', now()) - interval '1 day',
               interval '1 day'
             ) day",
            &[&app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_profiles VALUES ($1, $2, $3)",
            &[&user_id, &json!({"source": "sdk"}), &now],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps VALUES
             ($1, 'User app', $3, NULL, $5), ($2, 'Internal app', $4, NULL, $5)",
            &[&app_id, &excluded_app, &user_id, &excluded_id, &now],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO daily_app_transactions (app_id, date, count)
             VALUES ($1, $3, 12), ($2, $3, 99)",
            &[&app_id, &excluded_app, &(now - Duration::days(1))],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO daily_app_transactions (app_id, date, count, is_active)
             SELECT $1, day, 1, true
             FROM generate_series(
               '2025-01-01 00:00:00+00'::timestamptz,
               '2025-02-14 00:00:00+00'::timestamptz,
               interval '1 day'
             ) day",
            &[&app_id],
        )
        .await
        .unwrap();
    let size_attr = system_attr_id("$files", "size").unwrap();
    client
        .execute(
            "INSERT INTO attr_sketches VALUES ($1, $2, 3)",
            &[&app_id, &size_attr],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO triples_size_aggregate VALUES ($1, $2, 4096, 1024)",
            &[&app_id, &size_attr],
        )
        .await
        .unwrap();

    let machine_id = Uuid::new_v4();
    let hooks = PostgresDashboardMetricsHooks::new(
        client,
        NoopDashboardControlHooks,
        vec!["internal@example.com".to_owned()],
    )
    .with_session_metrics(
        Arc::new(SessionReporter { machine_id, app_id }),
        Arc::new(LocalRuntimeSessionReportCollector),
    )
    .with_database_query_throughput(Arc::new(QueryThroughput));
    let recent = hooks
        .execute(DashboardControlHookOperation::DashboardAdminRecentUsers)
        .await
        .unwrap();
    assert_eq!(recent.as_array().unwrap().len(), 2);
    let user = recent
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["email"] == "user@example.com")
        .unwrap();
    assert_eq!(user["app_title"], "User app");
    assert_eq!(user["meta"]["source"], "sdk");

    let top = hooks
        .execute(DashboardControlHookOperation::DashboardAdminTopUsers { days: 7 })
        .await
        .unwrap();
    assert_eq!(top.as_array().unwrap().len(), 1);
    assert_eq!(top[0]["user_email"], "user@example.com");
    assert_eq!(top[0]["total_transactions"], 18);

    let storage = hooks
        .execute(DashboardControlHookOperation::DashboardAdminStorageMetrics)
        .await
        .unwrap();
    assert_eq!(storage.as_array().unwrap().len(), 1);
    assert_eq!(storage[0]["total_byte_size"], 4096);
    assert_eq!(storage[0]["total_file_count"], 3);

    let daily = hooks
        .execute(DashboardControlHookOperation::DashboardAdminOverviewDaily)
        .await
        .unwrap();
    assert_eq!(daily["charts"].as_object().unwrap().len(), 4);
    assert_eq!(
        daily["data-points"]["rolling-monthly-stats"]
            .as_array()
            .unwrap()
            .len(),
        31
    );

    let minute = hooks
        .execute(DashboardControlHookOperation::DashboardAdminOverviewMinute)
        .await
        .unwrap();
    let machine = &minute["session-reports"][machine_id.to_string()];
    assert_eq!(machine[app_id.to_string()]["app-title"], "User app");
    assert_eq!(
        machine[app_id.to_string()]["creator-email"],
        "user@example.com"
    );
    assert_eq!(minute["proxied-connections"], Value::Null);

    let session_counts = hooks
        .execute(DashboardControlHookOperation::SessionCounts)
        .await
        .unwrap();
    assert_eq!(session_counts[app_id.to_string()]["app-title"], "User app");
    assert_eq!(
        session_counts[app_id.to_string()]["creator-email"],
        "user@example.com"
    );
    assert_eq!(session_counts[app_id.to_string()]["count"], 2);
    assert!(session_counts[app_id.to_string()].get("origins").is_none());

    let app_stats = hooks
        .execute(DashboardControlHookOperation::AppSessionStats { app_id })
        .await
        .unwrap();
    assert_eq!(app_stats["count"], 2);
    assert_eq!(app_stats["origins"]["https://app.example.com"], 2);

    let app_counts = hooks
        .execute(DashboardControlHookOperation::AppSessionCounts)
        .await
        .unwrap();
    assert_eq!(app_counts[app_id.to_string()], 2);

    let active = hooks
        .execute(DashboardControlHookOperation::ActiveSessionStats)
        .await
        .unwrap();
    assert_eq!(active, json!({"total-count": 2, "total-queries": 19}));
}

#[tokio::test]
async fn proxied_connection_overview_requires_the_live_config_flag() {
    let Some(database_url) = instant_test_support::test_database_url(
        "proxied_connection_overview_requires_the_live_config_flag",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let schema = format!("dashboard_proxy_flag_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
             );
             CREATE TABLE triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb
             );
             CREATE TABLE rust_runtime_session_reports (
               machine_id uuid PRIMARY KEY,
               proxied_connections jsonb,
               proxy_updated_at timestamptz
             );"
        ))
        .await
        .unwrap();

    let config_app_id = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    let flag_entity = Uuid::new_v4();
    let machine_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (id, app_id, etype, label) VALUES
             ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')",
            &[&setting_attr, &value_attr, &config_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO triples (app_id, entity_id, attr_id, value) VALUES
             ($1, $2, $3, to_jsonb('proxied-connections-overview-enabled'::text)),
             ($1, $2, $4, 'false'::jsonb)",
            &[&config_app_id, &flag_entity, &setting_attr, &value_attr],
        )
        .await
        .unwrap();

    let collector =
        PostgresRuntimeSessionReportCollector::new(client).with_config_app_id(config_app_id);
    collector
        .record_proxied_connections(machine_id, json!({"upstream": 3}))
        .await
        .unwrap();
    assert_eq!(collector.collect_proxied_connections().await.unwrap(), None);

    let client = collector_test_client(&database_url, &schema).await;
    client
        .execute(
            "UPDATE triples SET value = 'true'::jsonb
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&config_app_id, &flag_entity, &value_attr],
        )
        .await
        .unwrap();
    let enabled = collector
        .collect_proxied_connections()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(enabled[machine_id.to_string()]["upstream"], 3);

    client
        .execute(
            "UPDATE triples SET value = 'null'::jsonb
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&config_app_id, &flag_entity, &value_attr],
        )
        .await
        .unwrap();
    assert_eq!(collector.collect_proxied_connections().await.unwrap(), None);
}

async fn collector_test_client(database_url: &str, schema: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    client
}
