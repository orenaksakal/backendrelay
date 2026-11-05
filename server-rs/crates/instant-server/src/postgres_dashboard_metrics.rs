use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use chrono::{Datelike, Duration, NaiveDate};
use crc32fast::Hasher;
use flate2::{Compression, write::ZlibEncoder};
use instant_http::{ApiError, ApiErrorType};
use instant_instaql::system_attr_id;
use serde_json::{Value, json};
use tokio_postgres::Client;
use tokio_postgres::types::Json;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DashboardControlHookOperation, DashboardControlHooks, DatabaseQueryThroughput,
    INSTANT_CONFIG_APP_ID, PostgresPool, RuntimeSessionReporter,
};

const DEFAULT_SUMMARY_TTL_MILLIS: i64 = 5_000;

pub type SessionReportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'a>>;
pub type SessionTotalsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(usize, usize), ApiError>> + Send + 'a>>;
pub type ProxiedConnectionsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Value>, ApiError>> + Send + 'a>>;

/// Cross-machine fanout boundary for realtime dashboard summaries.
pub trait RuntimeSessionReportCollector: Send + Sync + 'static {
    fn collect_session_reports(
        &self,
        machine_id: Uuid,
        local_report: Value,
    ) -> SessionReportFuture<'_>;

    fn collect_totals(
        &self,
        machine_id: Uuid,
        local_sessions: usize,
        local_queries: usize,
    ) -> SessionTotalsFuture<'_>;

    fn collect_proxied_connections(&self) -> ProxiedConnectionsFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalRuntimeSessionReportCollector;

impl RuntimeSessionReportCollector for LocalRuntimeSessionReportCollector {
    fn collect_session_reports(
        &self,
        machine_id: Uuid,
        local_report: Value,
    ) -> SessionReportFuture<'_> {
        Box::pin(std::future::ready(Ok(
            json!({machine_id.to_string(): local_report}),
        )))
    }

    fn collect_totals(
        &self,
        _machine_id: Uuid,
        local_sessions: usize,
        local_queries: usize,
    ) -> SessionTotalsFuture<'_> {
        Box::pin(std::future::ready(Ok((local_sessions, local_queries))))
    }

    fn collect_proxied_connections(&self) -> ProxiedConnectionsFuture<'_> {
        Box::pin(std::future::ready(Ok(None)))
    }
}

/// Cross-process session-report collector using the native coordination table.
/// Session reports honor the live config-app TTL flag; totals and proxy state
/// use the JVM-compatible fixed five-second summary window.
pub struct PostgresRuntimeSessionReportCollector {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresRuntimeSessionReportCollector {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    async fn session_report_ttl_millis(&self, client: &Client) -> Result<i64, ApiError> {
        let value = client
            .query_opt(
                "SELECT flag_value.value
                 FROM attrs setting_attr
                 JOIN triples flag_setting
                   ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                  AND value_attr.label = 'value'
                 JOIN triples flag_value
                   ON flag_value.app_id = $1
                  AND flag_value.entity_id = flag_setting.entity_id
                  AND flag_value.attr_id = value_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                   AND setting_attr.label = 'setting'
                   AND flag_setting.value = to_jsonb('session-reports-cache-ttl'::text)
                 LIMIT 1",
                &[&self.config_app_id],
            )
            .await
            .map_err(session_report_database_error)?
            .map(|row| row.get::<_, Value>(0));
        Ok(parse_ttl_millis(value.as_ref()).unwrap_or(DEFAULT_SUMMARY_TTL_MILLIS))
    }

    /// Records the connection-proxy view for this machine. A future native
    /// proxy router can call this without changing the dashboard collector.
    ///
    /// # Errors
    ///
    /// Returns an internal API error when PostgreSQL rejects the update.
    pub async fn record_proxied_connections(
        &self,
        machine_id: Uuid,
        connections: Value,
    ) -> Result<(), ApiError> {
        self.client
            .acquire()
            .await
            .map_err(session_report_database_error)?
            .execute(
                "INSERT INTO rust_runtime_session_reports
                   (machine_id, proxied_connections, proxy_updated_at)
                 VALUES ($1, $2, now())
                 ON CONFLICT (machine_id) DO UPDATE SET
                   proxied_connections = EXCLUDED.proxied_connections,
                   proxy_updated_at = EXCLUDED.proxy_updated_at",
                &[&machine_id, &Json(&connections)],
            )
            .await
            .map_err(session_report_database_error)?;
        Ok(())
    }

    async fn proxied_connections_enabled(&self, client: &Client) -> Result<bool, ApiError> {
        let value = client
            .query_opt(
                "SELECT flag_value.value
                 FROM attrs setting_attr
                 JOIN triples flag_setting
                   ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                  AND value_attr.label = 'value'
                 JOIN triples flag_value
                   ON flag_value.app_id = $1
                  AND flag_value.entity_id = flag_setting.entity_id
                  AND flag_value.attr_id = value_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                   AND setting_attr.label = 'setting'
                   AND flag_setting.value = to_jsonb(
                     'proxied-connections-overview-enabled'::text)
                 LIMIT 1",
                &[&self.config_app_id],
            )
            .await
            .map_err(session_report_database_error)?
            .map(|row| row.get::<_, Value>(0));
        Ok(value.as_ref().is_some_and(clojure_truthy))
    }
}

impl RuntimeSessionReportCollector for PostgresRuntimeSessionReportCollector {
    fn collect_session_reports(
        &self,
        machine_id: Uuid,
        local_report: Value,
    ) -> SessionReportFuture<'_> {
        Box::pin(async move {
            let client = self
                .client
                .acquire()
                .await
                .map_err(session_report_database_error)?;
            let ttl_millis = self.session_report_ttl_millis(&client).await?;
            client
                .execute(
                    "INSERT INTO rust_runtime_session_reports
                       (machine_id, report, report_updated_at)
                     VALUES ($1, $2, now())
                     ON CONFLICT (machine_id) DO UPDATE SET
                       report = EXCLUDED.report,
                       report_updated_at = EXCLUDED.report_updated_at",
                    &[&machine_id, &Json(&local_report)],
                )
                .await
                .map_err(session_report_database_error)?;
            let reports: Json<Value> = client
                .query_one(
                    "SELECT coalesce(
                       jsonb_object_agg(machine_id::text, report ORDER BY machine_id),
                       '{}'::jsonb)
                     FROM rust_runtime_session_reports
                     WHERE report IS NOT NULL
                       AND (machine_id = $2 OR
                            report_updated_at >= now() - ($1::bigint * interval '1 millisecond'))",
                    &[&ttl_millis, &machine_id],
                )
                .await
                .map_err(session_report_database_error)?
                .get(0);
            Ok(reports.0)
        })
    }

    fn collect_totals(
        &self,
        machine_id: Uuid,
        local_sessions: usize,
        local_queries: usize,
    ) -> SessionTotalsFuture<'_> {
        Box::pin(async move {
            let sessions = i64::try_from(local_sessions).map_err(|_| {
                ApiError::new(
                    ApiErrorType::Unknown,
                    "Local session count exceeds PostgreSQL bigint",
                )
            })?;
            let queries = i64::try_from(local_queries).map_err(|_| {
                ApiError::new(
                    ApiErrorType::Unknown,
                    "Local query count exceeds PostgreSQL bigint",
                )
            })?;
            let ttl_millis = DEFAULT_SUMMARY_TTL_MILLIS;
            let client = self
                .client
                .acquire()
                .await
                .map_err(session_report_database_error)?;
            client
                .execute(
                    "INSERT INTO rust_runtime_session_reports
                       (machine_id, session_count, query_count, totals_updated_at)
                     VALUES ($1, $2, $3, now())
                     ON CONFLICT (machine_id) DO UPDATE SET
                       session_count = EXCLUDED.session_count,
                       query_count = EXCLUDED.query_count,
                       totals_updated_at = EXCLUDED.totals_updated_at",
                    &[&machine_id, &sessions, &queries],
                )
                .await
                .map_err(session_report_database_error)?;
            let row = client
                .query_one(
                    "SELECT coalesce(sum(session_count), 0)::bigint,
                            coalesce(sum(query_count), 0)::bigint
                     FROM rust_runtime_session_reports
                     WHERE totals_updated_at >= now() - ($1::bigint * interval '1 millisecond')",
                    &[&ttl_millis],
                )
                .await
                .map_err(session_report_database_error)?;
            let sessions = usize::try_from(row.get::<_, i64>(0)).map_err(|_| {
                ApiError::new(ApiErrorType::Unknown, "Cluster session count is invalid")
            })?;
            let queries = usize::try_from(row.get::<_, i64>(1)).map_err(|_| {
                ApiError::new(ApiErrorType::Unknown, "Cluster query count is invalid")
            })?;
            Ok((sessions, queries))
        })
    }

    fn collect_proxied_connections(&self) -> ProxiedConnectionsFuture<'_> {
        Box::pin(async move {
            let ttl_millis = DEFAULT_SUMMARY_TTL_MILLIS;
            let client = self
                .client
                .acquire()
                .await
                .map_err(session_report_database_error)?;
            if !self.proxied_connections_enabled(&client).await? {
                return Ok(None);
            }
            let connections = client
                .query_one(
                    "SELECT jsonb_object_agg(
                       machine_id::text, proxied_connections ORDER BY machine_id)
                     FROM rust_runtime_session_reports
                     WHERE proxied_connections IS NOT NULL
                       AND proxy_updated_at >= now() - ($1::bigint * interval '1 millisecond')",
                    &[&ttl_millis],
                )
                .await
                .map_err(session_report_database_error)?
                .get::<_, Option<Json<Value>>>(0)
                .map(|value| value.0);
            Ok(connections)
        })
    }
}

fn parse_ttl_millis(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::String(value)) => value.parse::<i64>().ok().filter(|ttl| *ttl >= 0),
        Some(Value::Number(value)) => value.as_i64().filter(|ttl| *ttl >= 0),
        _ => None,
    }
}

fn clojure_truthy(value: &Value) -> bool {
    !value.is_null() && value != &Value::Bool(false)
}

fn session_report_database_error(error: impl std::fmt::Display) -> ApiError {
    database_error(error)
}

/// PostgreSQL-backed operator metrics that are derived from Instant's own
/// tables. Billing, chart rendering, and distributed session reports remain
/// independently composable provider operations.
pub struct PostgresDashboardMetricsHooks<D> {
    client: PostgresClientSource,
    inner: D,
    excluded_emails: Vec<String>,
    session_metrics: Option<(
        Arc<dyn RuntimeSessionReporter>,
        Arc<dyn RuntimeSessionReportCollector>,
    )>,
    database_query_throughput: Option<Arc<dyn DatabaseQueryThroughput>>,
}

impl<D> PostgresDashboardMetricsHooks<D> {
    #[must_use]
    pub fn new(client: Client, inner: D, excluded_emails: Vec<String>) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), inner, excluded_emails)
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, inner: D, excluded_emails: Vec<String>) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool), inner, excluded_emails)
    }

    pub(crate) fn from_source(
        client: PostgresClientSource,
        inner: D,
        excluded_emails: Vec<String>,
    ) -> Self {
        Self {
            client,
            inner,
            excluded_emails,
            session_metrics: None,
            database_query_throughput: None,
        }
    }

    #[must_use]
    pub fn with_session_metrics(
        mut self,
        reporter: Arc<dyn RuntimeSessionReporter>,
        collector: Arc<dyn RuntimeSessionReportCollector>,
    ) -> Self {
        self.session_metrics = Some((reporter, collector));
        self
    }

    #[must_use]
    pub fn with_database_query_throughput(
        mut self,
        throughput: Arc<dyn DatabaseQueryThroughput>,
    ) -> Self {
        self.database_query_throughput = Some(throughput);
        self
    }

    async fn recent_users(&self) -> Result<Value, ApiError> {
        self.query_json(
            "SELECT coalesce(jsonb_agg(to_jsonb(recent) ORDER BY recent.user_created_at DESC), '[]'::jsonb)
             FROM (
               SELECT users.id, users.email, users.created_at AS user_created_at,
                      profiles.meta, profiles.created_at AS profile_created_at,
                      first_app.app_created_at, app.title AS app_title
               FROM instant_users users
               LEFT JOIN instant_profiles profiles ON users.id = profiles.id
               LEFT JOIN (
                 SELECT creator_id, min(created_at) AS app_created_at
                 FROM apps GROUP BY creator_id
               ) first_app ON first_app.creator_id = users.id
               LEFT JOIN apps app
                 ON app.creator_id = first_app.creator_id
                AND app.created_at = first_app.app_created_at
               ORDER BY users.created_at DESC
               LIMIT 50
             ) recent",
            &[],
        )
        .await
    }

    async fn top_users(&self, days: i32) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_one(
                "WITH app_attribution AS (
                   SELECT app.id AS app_id, app.title AS app_title,
                          coalesce(
                            creator.email,
                            (SELECT owner.email
                             FROM org_members member
                             JOIN instant_users owner ON owner.id = member.user_id
                             WHERE member.org_id = app.org_id AND member.role = 'owner'
                             ORDER BY member.created_at ASC LIMIT 1)
                          ) AS user_email
                   FROM apps app
                   LEFT JOIN instant_users creator ON creator.id = app.creator_id
                 ), top_users AS (
                   SELECT attribution.user_email, attribution.app_title,
                          sum(tx.count)::bigint AS total_transactions
                   FROM daily_app_transactions tx
                   JOIN app_attribution attribution ON attribution.app_id = tx.app_id
                   WHERE attribution.user_email IS NOT NULL
                     AND NOT (attribution.user_email = ANY($1::text[]))
                     AND tx.date::date BETWEEN now() - ($2::int * interval '1 day') AND now()
                   GROUP BY attribution.user_email, attribution.app_title
                   ORDER BY total_transactions DESC
                 )
                 SELECT coalesce(jsonb_agg(to_jsonb(top_users)), '[]'::jsonb) FROM top_users",
                &[&self.excluded_emails, &days],
            )
            .await
            .map_err(database_error)?;
        Ok(row.get(0))
    }

    async fn storage_metrics(&self) -> Result<Value, ApiError> {
        let files_size_attr = system_attr_id("$files", "size").map_err(database_error)?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_one(
                "WITH storage AS (
                   SELECT sketch.app_id, app.title, creator.email AS creator_email,
                          aggregate.files_size::bigint AS total_byte_size,
                          sketch.total::bigint AS total_file_count
                   FROM apps app
                   JOIN instant_users creator ON creator.id = app.creator_id
                   JOIN attr_sketches sketch
                     ON sketch.app_id = app.id AND sketch.attr_id = $1
                   JOIN triples_size_aggregate aggregate
                     ON aggregate.app_id = app.id AND aggregate.attr_id = $1
                   ORDER BY total_byte_size DESC
                 )
                 SELECT coalesce(jsonb_agg(to_jsonb(storage)), '[]'::jsonb) FROM storage",
                &[&files_size_attr],
            )
            .await
            .map_err(database_error)?;
        Ok(row.get(0))
    }

    async fn overview_daily(&self) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let target_date = client
            .query_one(
                "SELECT max(date)::date AS date FROM daily_app_transactions",
                &[],
            )
            .await
            .map_err(database_error)?
            .get::<_, Option<NaiveDate>>("date")
            .ok_or_else(no_metrics_data)?;
        let rolling = rolling_active_metrics(
            &client,
            target_date - Duration::days(30),
            target_date,
            30,
            &self.excluded_emails,
        )
        .await?;
        let month_start = target_date.with_day(1).ok_or_else(no_metrics_data)?;
        let month_to_date =
            month_to_date_metrics(&client, month_start, target_date, &self.excluded_emails).await?;
        let previous_month_end = month_start - Duration::days(1);
        let previous_month_start = previous_month_end.with_day(1).ok_or_else(no_metrics_data)?;
        let previous_month = monthly_active_summary(
            &client,
            previous_month_start,
            previous_month_end,
            &self.excluded_emails,
        )
        .await?;
        if rolling.is_empty() || month_to_date.is_empty() {
            return Err(no_metrics_data());
        }
        let daily_signups = signup_counts(&client, false, &self.excluded_emails).await?;
        let weekly_signups = signup_counts(&client, true, &self.excluded_emails).await?;
        drop(client);
        let rolling_apps = rolling
            .iter()
            .map(|metric| metric.distinct_apps)
            .collect::<Vec<_>>();
        let month_to_date_apps = month_to_date
            .iter()
            .map(|metric| metric.distinct_apps)
            .collect::<Vec<_>>();
        let month_goal = previous_month.distinct_apps.saturating_mul(12) / 10;
        Ok(json!({
            "date": target_date.format("%Y-%m-%d").to_string(),
            "data-points": {
                "rolling-monthly-stats": rolling.iter().map(DatedMetric::as_json).collect::<Vec<_>>(),
            },
            "charts": {
                "rolling-monthly-active-apps": line_chart_data_uri(&rolling_apps)?,
                "month-to-date-active-apps": line_chart_with_goal_data_uri(&month_to_date_apps, month_goal)?,
                "daily-signups": bar_chart_data_uri(&daily_signups)?,
                "weekly-signups": bar_chart_data_uri(&weekly_signups)?,
            },
        }))
    }

    async fn local_session_report(&self, extended: bool) -> Result<Value, ApiError> {
        let (reporter, _) = self
            .session_metrics
            .as_ref()
            .ok_or_else(session_metrics_unavailable)?;
        let local = reporter.local_session_report();
        let app_ids = local.keys().copied().collect::<Vec<_>>();
        let metadata = if app_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let client = self.client.acquire().await.map_err(database_error)?;
            client
                .query(
                    "SELECT apps.id, apps.title, users.email AS creator_email
                     FROM apps
                     LEFT JOIN instant_users users ON users.id = apps.creator_id
                     WHERE apps.id = ANY($1::uuid[])",
                    &[&app_ids],
                )
                .await
                .map_err(database_error)?
                .into_iter()
                .map(|row| {
                    (
                        row.get::<_, Uuid>("id"),
                        (
                            row.get::<_, Option<String>>("title"),
                            row.get::<_, Option<String>>("creator_email"),
                        ),
                    )
                })
                .collect()
        };
        Ok(Value::Object(
            local
                .into_iter()
                .map(|(app_id, report)| {
                    let (title, creator_email) =
                        metadata.get(&app_id).cloned().unwrap_or((None, None));
                    let mut value = json!({
                        "app-id": app_id,
                        "app-title": title,
                        "creator-email": creator_email,
                        "count": report.count,
                        "origins": report.origins,
                    });
                    if !extended {
                        let object = value.as_object_mut().expect("session report is an object");
                        object.remove("app-id");
                        object.remove("origins");
                    }
                    (app_id.to_string(), value)
                })
                .collect::<serde_json::Map<_, _>>(),
        ))
    }

    async fn session_reports(&self) -> Result<Value, ApiError> {
        let (reporter, collector) = self
            .session_metrics
            .as_ref()
            .ok_or_else(session_metrics_unavailable)?;
        let local = self.local_session_report(true).await?;
        collector
            .collect_session_reports(reporter.machine_id(), local)
            .await
    }

    async fn app_session_stats(&self, app_id: Uuid) -> Result<Value, ApiError> {
        let reports = self.session_reports().await?;
        let mut count = 0_i64;
        let mut origins = serde_json::Map::new();
        for report in reports
            .as_object()
            .into_iter()
            .flat_map(serde_json::Map::values)
            .filter_map(Value::as_object)
            .filter_map(|machine| machine.get(&app_id.to_string()))
        {
            count += report.get("count").and_then(Value::as_i64).unwrap_or(0);
            for (origin, origin_count) in report
                .get("origins")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(serde_json::Map::iter)
            {
                let total = origins.get(origin).and_then(Value::as_i64).unwrap_or(0)
                    + origin_count.as_i64().unwrap_or(0);
                origins.insert(origin.clone(), json!(total));
            }
        }
        Ok(json!({"count": count, "origins": origins}))
    }

    async fn active_session_stats(&self) -> Result<Value, ApiError> {
        let (reporter, collector) = self
            .session_metrics
            .as_ref()
            .ok_or_else(session_metrics_unavailable)?;
        let (sessions, local_queries) = collector
            .collect_totals(
                reporter.machine_id(),
                reporter.local_session_count(),
                reporter.local_query_count(),
            )
            .await?;
        let queries =
            database_query_count(self.database_query_throughput.as_ref(), local_queries).await;
        Ok(json!({"total-count": sessions, "total-queries": queries}))
    }

    async fn app_session_counts(&self) -> Result<Value, ApiError> {
        let reports = self.session_reports().await?;
        let mut counts = serde_json::Map::new();
        for machine in reports
            .as_object()
            .into_iter()
            .flat_map(serde_json::Map::values)
            .filter_map(Value::as_object)
        {
            for (app_id, report) in machine {
                let count = report.get("count").and_then(Value::as_i64).unwrap_or(0);
                let total = counts.get(app_id).and_then(Value::as_i64).unwrap_or(0);
                counts.insert(app_id.clone(), json!(total + count));
            }
        }
        Ok(Value::Object(counts))
    }

    async fn overview_minute(&self) -> Result<Value, ApiError> {
        let (_, collector) = self
            .session_metrics
            .as_ref()
            .ok_or_else(session_metrics_unavailable)?;
        let session_reports = self.session_reports().await?;
        let proxied_connections = collector.collect_proxied_connections().await?;
        Ok(json!({
            "session-reports": session_reports,
            "proxied-connections": proxied_connections,
        }))
    }

    async fn query_json(
        &self,
        sql: &str,
        parameters: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        client
            .query_one(sql, parameters)
            .await
            .map(|row| row.get(0))
            .map_err(database_error)
    }
}

async fn database_query_count(
    throughput: Option<&Arc<dyn DatabaseQueryThroughput>>,
    local_queries: usize,
) -> usize {
    match throughput {
        Some(throughput) => throughput.total_queries().await,
        None => local_queries,
    }
}

impl<D> DashboardControlHooks for PostgresDashboardMetricsHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::DashboardAdminRecentUsers => self.recent_users().await,
            DashboardControlHookOperation::DashboardAdminTopUsers { days } => {
                self.top_users(days).await
            }
            DashboardControlHookOperation::DashboardAdminStorageMetrics => {
                self.storage_metrics().await
            }
            DashboardControlHookOperation::DashboardAdminOverviewDaily => {
                self.overview_daily().await
            }
            DashboardControlHookOperation::DashboardAdminOverviewMinute => {
                self.overview_minute().await
            }
            DashboardControlHookOperation::SessionCounts => self.local_session_report(false).await,
            DashboardControlHookOperation::AppSessionStats { app_id } => {
                self.app_session_stats(app_id).await
            }
            DashboardControlHookOperation::AppSessionCounts => self.app_session_counts().await,
            DashboardControlHookOperation::ActiveSessionStats => self.active_session_stats().await,
            other => self.inner.execute(other).await,
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveMetric {
    distinct_apps: i64,
}

#[derive(Clone, Debug)]
struct DatedMetric {
    analysis_date: NaiveDate,
    distinct_users: i64,
    distinct_apps: i64,
}

impl DatedMetric {
    fn as_json(&self) -> Value {
        json!({
            "analysis_date": self.analysis_date.format("%Y-%m-%d").to_string(),
            "distinct_users": self.distinct_users,
            "distinct_apps": self.distinct_apps,
        })
    }
}

async fn rolling_active_metrics(
    client: &Client,
    start_date: NaiveDate,
    end_date: NaiveDate,
    window_days: i32,
    excluded_emails: &[String],
) -> Result<Vec<DatedMetric>, ApiError> {
    dated_active_metrics(
        client,
        "dat.date::date BETWEEN series.analysis_date - ($3::int * interval '1 day') AND series.analysis_date",
        start_date,
        end_date,
        window_days,
        excluded_emails,
    )
    .await
}

async fn month_to_date_metrics(
    client: &Client,
    start_date: NaiveDate,
    end_date: NaiveDate,
    excluded_emails: &[String],
) -> Result<Vec<DatedMetric>, ApiError> {
    dated_active_metrics(
        client,
        "dat.date::date BETWEEN date_trunc('month', series.analysis_date)::date AND series.analysis_date",
        start_date,
        end_date,
        0,
        excluded_emails,
    )
    .await
}

async fn dated_active_metrics(
    client: &Client,
    date_condition: &str,
    start_date: NaiveDate,
    end_date: NaiveDate,
    window_days: i32,
    excluded_emails: &[String],
) -> Result<Vec<DatedMetric>, ApiError> {
    let sql = format!(
        "SELECT series.analysis_date::date AS analysis_date,
                count(DISTINCT apps.id)::bigint AS distinct_apps,
                count(DISTINCT users.id)::bigint AS distinct_users
         FROM generate_series($1::date, $2::date, interval '1 day')
              AS series(analysis_date)
         LEFT JOIN daily_app_transactions dat ON {date_condition}
         LEFT JOIN apps ON dat.app_id = apps.id
         LEFT JOIN instant_users users ON apps.creator_id = users.id
         WHERE dat.is_active AND $3::int IS NOT NULL
           AND (apps.org_id IS NULL OR apps.org_id != '301bf1e5-9872-44c9-888e-e78c525a0e70'::uuid)
           AND ((users.email IS NULL AND apps.org_id IS NOT NULL)
                OR users.email <> ALL($4::text[]))
         GROUP BY series.analysis_date
         ORDER BY series.analysis_date"
    );
    client
        .query(
            &sql,
            &[&start_date, &end_date, &window_days, &excluded_emails],
        )
        .await
        .map_err(database_error)
        .map(rows_to_dated_metrics)
}

async fn monthly_active_summary(
    client: &Client,
    start_date: NaiveDate,
    end_date: NaiveDate,
    excluded_emails: &[String],
) -> Result<ActiveMetric, ApiError> {
    client
        .query_opt(
            "SELECT count(DISTINCT apps.id)::bigint AS distinct_apps
             FROM daily_app_transactions dat
             JOIN apps ON dat.app_id = apps.id
             LEFT JOIN instant_users users ON apps.creator_id = users.id
             WHERE dat.is_active AND dat.date::date BETWEEN $1 AND $2
               AND (apps.org_id IS NULL OR apps.org_id != '301bf1e5-9872-44c9-888e-e78c525a0e70'::uuid)
               AND ((users.email IS NULL AND apps.org_id IS NOT NULL)
                    OR users.email <> ALL($3::text[]))",
            &[&start_date, &end_date, &excluded_emails],
        )
        .await
        .map_err(database_error)?
        .map(|row| ActiveMetric {
            distinct_apps: row.get("distinct_apps"),
        })
        .ok_or_else(no_metrics_data)
}

fn rows_to_dated_metrics(rows: Vec<tokio_postgres::Row>) -> Vec<DatedMetric> {
    rows.into_iter()
        .map(|row| DatedMetric {
            analysis_date: row.get("analysis_date"),
            distinct_users: row.get("distinct_users"),
            distinct_apps: row.get("distinct_apps"),
        })
        .collect()
}

async fn signup_counts(
    client: &Client,
    weekly: bool,
    excluded_emails: &[String],
) -> Result<Vec<i64>, ApiError> {
    let (period, lower_bound) = if weekly {
        (
            "week",
            "((now() AT TIME ZONE 'America/Los_Angeles')::date
              - extract(dow FROM now() AT TIME ZONE 'America/Los_Angeles')::int
              - 6 - 84)::timestamp AT TIME ZONE 'America/Los_Angeles'",
        )
    } else {
        (
            "day",
            "((now() AT TIME ZONE 'America/Los_Angeles')::date - 14)::timestamp
              AT TIME ZONE 'America/Los_Angeles'",
        )
    };
    let sql = format!(
        "SELECT date_trunc(
                  '{period}',
                  (users.created_at AT TIME ZONE 'UTC') AT TIME ZONE 'America/Los_Angeles'
                ) AS signup_period,
                count(users.id)::bigint AS signup_count
         FROM instant_users users
         WHERE users.created_at >= {lower_bound}
           AND users.created_at < now()
           AND users.email <> ALL($1::text[])
         GROUP BY signup_period
         ORDER BY signup_period"
    );
    client
        .query(&sql, &[&excluded_emails])
        .await
        .map_err(database_error)
        .map(|rows| {
            rows.into_iter()
                .map(|row| row.get("signup_count"))
                .collect()
        })
}

const CHART_WIDTH: usize = 500;
const CHART_HEIGHT: usize = 400;

fn line_chart_data_uri(values: &[i64]) -> Result<String, ApiError> {
    chart_data_uri(values, false, None)
}

fn bar_chart_data_uri(values: &[i64]) -> Result<String, ApiError> {
    chart_data_uri(values, true, None)
}

fn line_chart_with_goal_data_uri(values: &[i64], goal: i64) -> Result<String, ApiError> {
    chart_data_uri(values, false, Some(goal))
}

fn chart_data_uri(values: &[i64], bars: bool, goal: Option<i64>) -> Result<String, ApiError> {
    let png = chart_png(values, bars, goal, CHART_WIDTH, CHART_HEIGHT)?;
    Ok(format!(
        "data:image/png;base64, {}",
        base64::engine::general_purpose::STANDARD.encode(png)
    ))
}

fn chart_png(
    values: &[i64],
    bars: bool,
    goal: Option<i64>,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, ApiError> {
    if width <= 72 || height <= 72 {
        return Err(ApiError::new(
            ApiErrorType::ValidationFailed,
            "Chart dimensions are too small",
        ));
    }
    let mut pixels = vec![255_u8; width * height * 3];
    let left = 48;
    let right = width - 24;
    let top = 28;
    let bottom = height - 44;
    draw_line(&mut pixels, width, height, left, top, left, bottom);
    draw_line(&mut pixels, width, height, left, bottom, right, bottom);
    let maximum = values.iter().copied().chain(goal).max().unwrap_or(1).max(1);
    if bars {
        let slot = (right - left).checked_div(values.len().max(1)).unwrap_or(1);
        for (index, value) in values.iter().enumerate() {
            let bar_height = scaled_height(*value, maximum, bottom - top);
            let x0 = left + index * slot + slot / 5;
            let x1 = left + (index + 1) * slot - slot / 5;
            let y0 = bottom.saturating_sub(bar_height);
            draw_rect_outline(&mut pixels, width, height, x0, y0, x1.max(x0 + 1), bottom);
        }
    } else if !values.is_empty() {
        let denominator = values.len().saturating_sub(1).max(1);
        let mut previous = None;
        for (index, value) in values.iter().enumerate() {
            let x = left + index * (right - left) / denominator;
            let y = bottom - scaled_height(*value, maximum, bottom - top);
            if let Some((previous_x, previous_y)) = previous {
                draw_line(&mut pixels, width, height, previous_x, previous_y, x, y);
            }
            draw_filled_square(&mut pixels, width, height, x, y, 2);
            previous = Some((x, y));
        }
        if let Some(goal) = goal {
            let start_y = bottom - scaled_height(values[0], maximum, bottom - top);
            let goal_y = bottom - scaled_height(goal, maximum, bottom - top);
            draw_dashed_line(&mut pixels, width, height, left, start_y, right, goal_y);
        }
    }
    encode_png(&pixels, width, height)
}

fn draw_dashed_line(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) {
    const DASHES: usize = 24;
    for index in (0..DASHES).step_by(2) {
        let start_x = x0 + (x1 - x0) * index / DASHES;
        let start_y = interpolate(y0, y1, index, DASHES);
        let end_index = (index + 1).min(DASHES);
        let end_x = x0 + (x1 - x0) * end_index / DASHES;
        let end_y = interpolate(y0, y1, end_index, DASHES);
        draw_line(pixels, width, height, start_x, start_y, end_x, end_y);
    }
}

fn interpolate(start: usize, end: usize, position: usize, denominator: usize) -> usize {
    if end >= start {
        start + (end - start) * position / denominator
    } else {
        start - (start - end) * position / denominator
    }
}

fn scaled_height(value: i64, maximum: i64, available: usize) -> usize {
    let value = u128::from(u64::try_from(value.max(0)).unwrap_or(0));
    let maximum = u128::from(u64::try_from(maximum.max(1)).unwrap_or(1));
    let available = u128::try_from(available).unwrap_or(0);
    usize::try_from(value * available / maximum).unwrap_or(usize::MAX)
}

fn draw_rect_outline(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) {
    draw_line(pixels, width, height, x0, y0, x1, y0);
    draw_line(pixels, width, height, x1, y0, x1, y1);
    draw_line(pixels, width, height, x1, y1, x0, y1);
    draw_line(pixels, width, height, x0, y1, x0, y0);
}

fn draw_filled_square(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    radius: usize,
) {
    for py in y.saturating_sub(radius)..=(y + radius).min(height - 1) {
        for px in x.saturating_sub(radius)..=(x + radius).min(width - 1) {
            set_black(pixels, width, height, px, py);
        }
    }
}

fn draw_line(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) {
    let (mut x0, mut y0, x1, y1) = (
        x0.cast_signed(),
        y0.cast_signed(),
        x1.cast_signed(),
        y1.cast_signed(),
    );
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 {
            set_black(
                pixels,
                width,
                height,
                x0.cast_unsigned(),
                y0.cast_unsigned(),
            );
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x0 += sx;
        }
        if doubled <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

fn set_black(pixels: &mut [u8], width: usize, height: usize, x: usize, y: usize) {
    if x >= width || y >= height {
        return;
    }
    let offset = (y * width + x) * 3;
    pixels[offset..offset + 3].fill(0);
}

fn encode_png(pixels: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ApiError> {
    let mut scanlines = Vec::with_capacity((width * 3 + 1) * height);
    for row in pixels.chunks_exact(width * 3) {
        scanlines.push(0);
        scanlines.extend_from_slice(row);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&scanlines).map_err(database_error)?;
    let compressed = encoder.finish().map_err(database_error)?;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&u32::try_from(width).map_err(database_error)?.to_be_bytes());
    header.extend_from_slice(&u32::try_from(height).map_err(database_error)?.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut png, *b"IHDR", &header);
    png_chunk(&mut png, *b"IDAT", &compressed);
    png_chunk(&mut png, *b"IEND", &[]);
    Ok(png)
}

fn png_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let length = u32::try_from(data.len()).expect("a bounded chart PNG chunk fits in u32");
    png.extend_from_slice(&length.to_be_bytes());
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let mut hasher = Hasher::new();
    hasher.update(&kind);
    hasher.update(data);
    png.extend_from_slice(&hasher.finalize().to_be_bytes());
}

fn no_metrics_data() -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, "No data found for stats")
}

fn session_metrics_unavailable() -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        "Distributed session metrics are unavailable",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base64::Engine as _;
    use instant_http::ApiErrorType;
    use serde_json::{Value, json};

    use super::{
        DatabaseQueryThroughput, bar_chart_data_uri, clojure_truthy, database_query_count,
        line_chart_data_uri, parse_ttl_millis, session_report_database_error,
    };

    struct StaticQueryThroughput;

    impl DatabaseQueryThroughput for StaticQueryThroughput {
        fn total_queries(&self) -> crate::DatabaseQueryThroughputFuture<'_> {
            Box::pin(std::future::ready(23))
        }
    }

    #[tokio::test]
    async fn active_stats_use_database_throughput_instead_of_subscription_count() {
        let throughput: Arc<dyn DatabaseQueryThroughput> = Arc::new(StaticQueryThroughput);
        assert_eq!(database_query_count(Some(&throughput), 7).await, 23);
        assert_eq!(database_query_count(None, 7).await, 7);
    }

    #[test]
    fn session_report_database_errors_hide_the_driver_message() {
        let error = session_report_database_error("relation \"secret_table\" does not exist");
        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Database error");
        assert!(!error.body().message.contains("secret_table"));
    }

    #[test]
    fn session_report_ttl_accepts_config_strings_and_defaults_invalid_values() {
        assert_eq!(parse_ttl_millis(Some(&json!("5000"))), Some(5_000));
        assert_eq!(parse_ttl_millis(Some(&json!(2500))), Some(2_500));
        assert_eq!(parse_ttl_millis(Some(&json!("-1"))), None);
        assert_eq!(parse_ttl_millis(Some(&json!("later"))), None);
        assert_eq!(parse_ttl_millis(None), None);
    }

    #[test]
    fn proxied_connections_flag_preserves_clojure_truthiness() {
        assert!(!clojure_truthy(&Value::Null));
        assert!(!clojure_truthy(&json!(false)));
        assert!(clojure_truthy(&json!(true)));
        assert!(clojure_truthy(&json!(0)));
        assert!(clojure_truthy(&json!("false")));
        assert!(clojure_truthy(&json!([])));
    }

    #[test]
    fn native_charts_are_jvm_shaped_500_by_400_png_data_uris() {
        for chart in [
            line_chart_data_uri(&[2, 5, 3]).unwrap(),
            bar_chart_data_uri(&[2, 5, 3]).unwrap(),
        ] {
            let payload = chart
                .strip_prefix("data:image/png;base64, ")
                .expect("JVM data URI prefix");
            let png = base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 500);
            assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 400);
            assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        }
    }
}
