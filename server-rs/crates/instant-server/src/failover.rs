use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use instant_datalog::MatchCteOptions;
use instant_http::{ApiError, ApiErrorType};
use instant_instaql::normalized_query_hash;
use instant_transact::{TransactionOptions, compile_transaction, execute_transaction};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::{internal_error, query_validation_error};
use crate::postgres_pool::{PostgresClientSource, PostgresPoolError};
use crate::{
    CombineTransactsFuture, CombineTransactsPolicy, INSTANT_CONFIG_APP_ID, PostgresPool,
    PostgresQuerySetting, PreparedInstantQuery, QueryModifierPolicy, QueryModifierPolicyFuture,
    QueryPlannerPolicy, QueryPlannerPolicyFuture, QueryPreparationPolicy,
    QueryPreparationPolicyFuture, QuerySettingsPolicy, QuerySettingsPolicyFuture,
    ReplicaReadPolicy, ReplicaReadPolicyFuture,
};

const PLANNER_FLAG_SETTINGS: &[&str] = &[
    "always-materialize-attr-ids",
    "row-hint-threshold",
    "disable-hint-query-hashes",
    "use-hint-query-hashes",
    "enable-row-hints-query-hashes",
    "disable-row-hints-query-hashes",
    "disable-row-hints-apps",
];

const RUNTIME_FLAG_SETTINGS: &[&str] = &[
    "coarse-topics-apps",
    "refresh-throttled-apps",
    "refresh-throttle-ms",
];

const PLANNER_TOGGLE_SETTINGS: &[&str] = &[
    "disable-pg-hints",
    "pg-hints-by-default",
    "enable-row-hints-globally",
    "disable-is-null-fix",
    "disable-hint-plan-av-fix",
    "disable-like-count-optimization",
    "fix-is-null-index-cost",
    "disable-cap-symbol-map-by-page-limit",
    "ensure-empty-symbol-values-for-topics",
    "test-pg-hint-plan",
    "hint-plan-optional-number-order-scan",
    "forward-is-null-exists",
    "hint-plan-small-page-jit-off",
    "disable-candidate-entity-scans",
    "disable-permission-attribute-projection",
    "disable-shared-root-frames",
];

// One statement gives modifiers and their original-hash controls a consistent
// live snapshot. Ordering preserves modifier and PostgreSQL setting precedence.
// Materialize attribute/value pairs once: planning repeated eight-way joins
// dominated runtime and serialized every refresh behind the control connection.
const QUERY_CONTROL_VALUES_SQL: &str = "WITH control_values AS MATERIALIZED (
    SELECT a.etype, a.label, t.entity_id, t.value
    FROM attrs a JOIN triples t ON t.app_id = a.app_id AND t.attr_id = a.id
    WHERE a.app_id = $1 AND a.etype IN ('flags','toggles','query-modifiers','query-flags')
)
SELECT 'flag' AS kind, setting.entity_id,
       jsonb_build_array(setting.value, val.value) AS data
FROM control_values setting JOIN control_values val
  ON val.etype='flags' AND val.label='value' AND val.entity_id=setting.entity_id
WHERE setting.etype='flags' AND setting.label='setting' AND setting.value #>> '{}' = ANY($4)
UNION ALL
SELECT 'toggle', setting.entity_id, jsonb_build_array(setting.value, val.value)
FROM control_values setting JOIN control_values val
  ON val.etype='toggles' AND val.label='toggled' AND val.entity_id=setting.entity_id
WHERE setting.etype='toggles' AND setting.label='setting' AND setting.value #>> '{}' = ANY($5)
UNION ALL
SELECT 'modifier', app.entity_id, jsonb_build_array(app.value, hash.value, etype.value, params.value)
FROM control_values app
JOIN control_values hash ON hash.etype='query-modifiers' AND hash.label='query-hash' AND hash.entity_id=app.entity_id
JOIN control_values etype ON etype.etype='query-modifiers' AND etype.label='etype' AND etype.entity_id=app.entity_id
JOIN control_values params ON params.etype='query-modifiers' AND params.label='dollar-params' AND params.entity_id=app.entity_id
WHERE app.etype='query-modifiers' AND app.label='app-id' AND app.value=to_jsonb($2::text) AND hash.value=to_jsonb($3::int)
UNION ALL
SELECT 'setting', hash.entity_id, jsonb_build_array(setting.value, val.value)
FROM control_values hash
JOIN control_values setting ON setting.etype='query-flags' AND setting.label='setting' AND setting.entity_id=hash.entity_id
JOIN control_values val ON val.etype='query-flags' AND val.label='value' AND val.entity_id=hash.entity_id
WHERE hash.etype='query-flags' AND hash.label='query-hash' AND hash.value=to_jsonb($3::int)
ORDER BY kind, entity_id";

const IS_TOGGLED_SQL: &str = "SELECT toggle_value.value
                 FROM attrs setting_attr
                 JOIN triples toggle_setting
                   ON toggle_setting.app_id = $1 AND toggle_setting.attr_id = setting_attr.id
                 JOIN attrs toggled_attr
                   ON toggled_attr.app_id = $1 AND toggled_attr.etype = 'toggles'
                  AND toggled_attr.label = 'toggled'
                 JOIN triples toggle_value
                   ON toggle_value.app_id = $1
                  AND toggle_value.entity_id = toggle_setting.entity_id
                  AND toggle_value.attr_id = toggled_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'toggles'
                   AND setting_attr.label = 'setting'
                   AND toggle_setting.value = to_jsonb($2::text)
                 LIMIT 1";
const FLAG_VALUE_SQL: &str = "SELECT flag_value.value
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
                   AND flag_setting.value = to_jsonb($2::text)
                 LIMIT 1";
const FLAG_VALUES_SQL: &str = "SELECT flag_setting.value #>> '{}', flag_value.value
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
                   AND flag_setting.value #>> '{}' = ANY($2)";
const PLANNER_CONFIG_VALUES_SQL: &str =
    "SELECT 'flag', flag_setting.value #>> '{}', flag_value.value
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
                   AND flag_setting.value #>> '{}' = ANY($2)
                 UNION ALL
                 SELECT 'toggle', toggle_setting.value #>> '{}', toggle_value.value
                 FROM attrs setting_attr
                 JOIN triples toggle_setting
                   ON toggle_setting.app_id = $1 AND toggle_setting.attr_id = setting_attr.id
                 JOIN attrs toggled_attr
                   ON toggled_attr.app_id = $1 AND toggled_attr.etype = 'toggles'
                  AND toggled_attr.label = 'toggled'
                 JOIN triples toggle_value
                   ON toggle_value.app_id = $1
                  AND toggle_value.entity_id = toggle_setting.entity_id
                  AND toggle_value.attr_id = toggled_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'toggles'
                   AND setting_attr.label = 'setting'
                   AND toggle_setting.value #>> '{}' = ANY($3)";
const QUERY_MODIFIERS_SQL: &str =
    "SELECT app_value.value, hash_value.value, etype_value.value, params_value.value
                 FROM attrs app_attr
                 JOIN triples app_value
                   ON app_value.app_id = $1 AND app_value.attr_id = app_attr.id
                 JOIN attrs hash_attr
                   ON hash_attr.app_id = $1 AND hash_attr.etype = 'query-modifiers'
                  AND hash_attr.label = 'query-hash'
                 JOIN triples hash_value
                   ON hash_value.app_id = $1 AND hash_value.entity_id = app_value.entity_id
                  AND hash_value.attr_id = hash_attr.id
                 JOIN attrs etype_attr
                   ON etype_attr.app_id = $1 AND etype_attr.etype = 'query-modifiers'
                  AND etype_attr.label = 'etype'
                 JOIN triples etype_value
                   ON etype_value.app_id = $1 AND etype_value.entity_id = app_value.entity_id
                  AND etype_value.attr_id = etype_attr.id
                 JOIN attrs params_attr
                   ON params_attr.app_id = $1 AND params_attr.etype = 'query-modifiers'
                  AND params_attr.label = 'dollar-params'
                 JOIN triples params_value
                   ON params_value.app_id = $1 AND params_value.entity_id = app_value.entity_id
                  AND params_value.attr_id = params_attr.id
                 WHERE app_attr.app_id = $1 AND app_attr.etype = 'query-modifiers'
                   AND app_attr.label = 'app-id'
                   AND app_value.value = to_jsonb($2::text)
                   AND hash_value.value = to_jsonb($3::int)
                 ORDER BY app_value.entity_id";
const QUERY_SETTINGS_SQL: &str = "SELECT setting_value.value, value_value.value
                 FROM attrs hash_attr
                 JOIN triples hash_value
                   ON hash_value.app_id = $1 AND hash_value.attr_id = hash_attr.id
                 JOIN attrs setting_attr
                   ON setting_attr.app_id = $1 AND setting_attr.etype = 'query-flags'
                  AND setting_attr.label = 'setting'
                 JOIN triples setting_value
                   ON setting_value.app_id = $1 AND setting_value.entity_id = hash_value.entity_id
                  AND setting_value.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = $1 AND value_attr.etype = 'query-flags'
                  AND value_attr.label = 'value'
                 JOIN triples value_value
                   ON value_value.app_id = $1 AND value_value.entity_id = hash_value.entity_id
                  AND value_value.attr_id = value_attr.id
                 WHERE hash_attr.app_id = $1 AND hash_attr.etype = 'query-flags'
                   AND hash_attr.label = 'query-hash'
                   AND hash_value.value = to_jsonb($2::int)
                 ORDER BY hash_value.entity_id";

// The config app is immutable for a gate instance. Exposing that UUID to the
// planner avoids repeated custom planning when most rows belong to other apps.
// All mutable policy values and request inputs remain execution-time parameters.
struct ControlQueries {
    is_toggled: String,
    flag_value: String,
    flag_values: String,
    planner_config_values: String,
    query_modifiers: String,
    query_settings: String,
    query_control_values: String,
}

impl ControlQueries {
    fn new(config_app_id: Uuid) -> Self {
        fn specialize(sql: &str, app_id: Uuid) -> String {
            // Only a typed UUID is interpolated, never caller-provided SQL/text.
            let mut sql = sql.replace("$1", &format!("'{app_id}'::uuid"));
            for parameter in 2..=5 {
                sql = sql.replace(&format!("${parameter}"), &format!("${}", parameter - 1));
            }
            sql
        }
        Self {
            is_toggled: specialize(IS_TOGGLED_SQL, config_app_id),
            flag_value: specialize(FLAG_VALUE_SQL, config_app_id),
            flag_values: specialize(FLAG_VALUES_SQL, config_app_id),
            planner_config_values: specialize(PLANNER_CONFIG_VALUES_SQL, config_app_id),
            query_modifiers: specialize(QUERY_MODIFIERS_SQL, config_app_id),
            query_settings: specialize(QUERY_SETTINGS_SQL, config_app_id),
            query_control_values: specialize(QUERY_CONTROL_VALUES_SQL, config_app_id),
        }
    }
}

#[derive(Debug, Error)]
pub enum FailoverGateError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
    #[error("invalid config-app flag transaction: {0}")]
    Config(String),
    #[error("config-app flag transaction failed: {0}")]
    Transaction(String),
}

/// Reads operational toggles directly from the config app. Production uses an
/// separate reconnecting control pool so a dropped gate connection does not
/// terminate the process. Control reads still share that pool with each other. The
/// fixed-client constructor remains available to tests and one-shot operators.
/// Critical gates always execute fresh reads. Production query preparation can
/// opt into a WAL-invalidated planner-only snapshot; standalone constructors
/// continue to read fresh values on every call.
pub struct PostgresFailoverGate {
    client: PostgresClientSource,
    config_app_id: Uuid,
    query_sql: ControlQueries,
    query_controls: Option<Arc<QueryControlCache>>,
}

impl PostgresFailoverGate {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            query_sql: ControlQueries::new(INSTANT_CONFIG_APP_ID),
            query_controls: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            query_sql: ControlQueries::new(INSTANT_CONFIG_APP_ID),
            query_controls: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self.query_sql = ControlQueries::new(config_app_id);
        // Never reuse a snapshot belonging to a different config app.
        self.query_controls = None;
        self
    }

    pub(crate) fn with_query_control_cache(mut self, cache: Arc<QueryControlCache>) -> Self {
        assert_eq!(self.config_app_id, cache.config_app_id);
        self.query_controls = Some(cache);
        self
    }

    /// Matches Clojure toggle truthiness: only a missing value, JSON null, or
    /// boolean false is off.
    ///
    /// # Errors
    ///
    /// Returns a database error rather than allowing guarded writes without a
    /// current decision.
    pub async fn is_toggled(&self, setting: &str) -> Result<bool, FailoverGateError> {
        let client = self.client.acquire().await?;
        let statement = client.prepare_cached(&self.query_sql.is_toggled).await?;
        let value = client
            .query_opt(&statement, &[&setting])
            .await?
            .map(|row| row.get::<_, Value>(0));
        Ok(value.as_ref().is_some_and(toggle_truthy))
    }

    /// # Errors
    ///
    /// Returns a database error when the failover toggle cannot be read.
    pub async fn is_failing_over(&self) -> Result<bool, FailoverGateError> {
        self.is_toggled("failing-over").await
    }

    /// Reads a value from the config app's `flags` namespace.
    ///
    /// # Errors
    ///
    /// Returns a database error if the live flag cannot be queried.
    pub async fn flag_value(&self, setting: &str) -> Result<Option<Value>, FailoverGateError> {
        let client = self.client.acquire().await?;
        let statement = client.prepare_cached(&self.query_sql.flag_value).await?;
        Ok(client
            .query_opt(&statement, &[&setting])
            .await?
            .map(|row| row.get(0)))
    }

    /// Upserts a config-app flag through the native transaction kernel so the
    /// write participates in normal indexing, transaction, and WAL behavior.
    ///
    /// # Errors
    ///
    /// Returns a pool, schema, compilation, or transaction execution error.
    pub async fn set_flag_value(
        &self,
        setting: &str,
        value: Value,
    ) -> Result<(), FailoverGateError> {
        let _invalidate = self.query_controls.as_deref().map(|cache| {
            cache.invalidate();
            QueryControlWriteGuard(cache)
        });
        let mut client = self.client.acquire().await?;
        let catalog = crate::admin_metadata::load_attribute_catalog(&client, self.config_app_id)
            .await
            .map_err(|error| FailoverGateError::Config(format!("{error:?}")))?;
        let id_attr = catalog
            .forward("flags", "id")
            .ok_or_else(|| FailoverGateError::Config("missing flags.id attribute".to_owned()))?;
        let setting_attr = catalog.forward("flags", "setting").ok_or_else(|| {
            FailoverGateError::Config("missing flags.setting attribute".to_owned())
        })?;
        let value_attr = catalog
            .forward("flags", "value")
            .ok_or_else(|| FailoverGateError::Config("missing flags.value attribute".to_owned()))?;
        let lookup = json!([setting_attr.id, setting]);
        let raw_steps = json!([
            ["add-triple", lookup, id_attr.id, lookup],
            ["add-triple", lookup, value_attr.id, value]
        ]);
        let steps = compile_transaction(&raw_steps, &catalog)
            .map_err(|error| FailoverGateError::Config(error.to_string()))?;
        execute_transaction(
            &mut client,
            self.config_app_id,
            &catalog,
            &steps,
            TransactionOptions::default(),
        )
        .await
        .map_err(|error| FailoverGateError::Transaction(error.to_string()))?;
        Ok(())
    }

    /// Reads several config-app flag values in one consistent database statement.
    /// Missing settings are omitted from the returned map.
    ///
    /// # Errors
    ///
    /// Returns a database error if the live flags cannot be queried.
    pub async fn flag_values(
        &self,
        settings: &[&str],
    ) -> Result<HashMap<String, Value>, FailoverGateError> {
        if settings.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.client.acquire().await?;
        let statement = client.prepare_cached(&self.query_sql.flag_values).await?;
        Ok(client
            .query(&statement, &[&settings])
            .await?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    async fn planner_config_values(
        &self,
        flag_settings: &[&str],
        toggle_settings: &[&str],
    ) -> Result<(HashMap<String, Value>, HashMap<String, Value>), FailoverGateError> {
        if let Some(cache) = &self.query_controls {
            let snapshot = cache.snapshot().await?;
            return Ok((
                snapshot
                    .flags
                    .iter()
                    .filter(|(key, _)| flag_settings.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                snapshot
                    .toggles
                    .iter()
                    .filter(|(key, _)| toggle_settings.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ));
        }
        let client = self.client.acquire().await?;
        let statement = client
            .prepare_cached(&self.query_sql.planner_config_values)
            .await?;
        let rows = client
            .query(&statement, &[&flag_settings, &toggle_settings])
            .await?;
        let mut flags = HashMap::new();
        let mut toggles = HashMap::new();
        for row in rows {
            let namespace: String = row.get(0);
            let setting = row.get(1);
            let value = row.get(2);
            if namespace == "flag" {
                flags.insert(setting, value);
            } else {
                toggles.insert(setting, value);
            }
        }
        Ok((flags, toggles))
    }

    /// Uses the opt-in planner snapshot in production, or one fresh statement
    /// for callers that do not participate in WAL invalidation.
    async fn query_control_values(
        &self,
        app_id: Uuid,
        original_hash: i32,
    ) -> Result<QueryControlValues, FailoverGateError> {
        if let Some(cache) = &self.query_controls {
            return Ok(cache.snapshot().await?.for_query(app_id, original_hash));
        }
        let client = self.client.acquire().await?;
        let statement = client
            .prepare_cached(&self.query_sql.query_control_values)
            .await?;
        let rows = client
            .query(
                &statement,
                &[
                    &app_id.to_string(),
                    &original_hash,
                    &PLANNER_FLAG_SETTINGS,
                    &PLANNER_TOGGLE_SETTINGS,
                ],
            )
            .await?;
        let mut controls = QueryControlValues::default();
        for row in rows {
            let kind: &str = row.get(0);
            let data: Value = row.get(2);
            match kind {
                "flag" | "toggle" => {
                    let setting = data[0].as_str().ok_or_else(|| {
                        FailoverGateError::Config("query control name must be a string".to_owned())
                    })?;
                    let values = if kind == "flag" {
                        &mut controls.flags
                    } else {
                        &mut controls.toggles
                    };
                    Arc::make_mut(values).insert(setting.to_owned(), data[1].clone());
                }
                "modifier" => {
                    if let Some(modifier) = decode_query_modifier(
                        &data[0],
                        &data[1],
                        &data[2],
                        &data[3],
                        app_id,
                        original_hash,
                    ) {
                        controls.modifiers.push(modifier);
                    }
                }
                "setting" => controls.settings.push((data[0].clone(), data[1].clone())),
                _ => {
                    return Err(FailoverGateError::Config(
                        "unknown query control namespace".to_owned(),
                    ));
                }
            }
        }
        Ok(controls)
    }

    async fn query_modifiers(
        &self,
        app_id: Uuid,
        query_hash: i32,
    ) -> Result<Vec<QueryModifier>, FailoverGateError> {
        if let Some(cache) = &self.query_controls {
            return Ok(cache
                .snapshot()
                .await?
                .for_query(app_id, query_hash)
                .modifiers);
        }
        let client = self.client.acquire().await?;
        let statement = client
            .prepare_cached(&self.query_sql.query_modifiers)
            .await?;
        let rows = client
            .query(&statement, &[&app_id.to_string(), &query_hash])
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                decode_query_modifier(
                    &row.get(0),
                    &row.get(1),
                    &row.get(2),
                    &row.get(3),
                    app_id,
                    query_hash,
                )
            })
            .collect())
    }

    async fn query_settings(
        &self,
        query_hash: i32,
    ) -> Result<Vec<PostgresQuerySetting>, FailoverGateError> {
        if let Some(cache) = &self.query_controls {
            let snapshot = cache.snapshot().await?;
            return decode_query_settings(
                snapshot
                    .settings
                    .get(&query_hash)
                    .map_or(&[], Vec::as_slice),
            );
        }
        let client = self.client.acquire().await?;
        let statement = client
            .prepare_cached(&self.query_sql.query_settings)
            .await?;
        let rows = client.query(&statement, &[&query_hash]).await?;
        let rows = rows
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect::<Vec<_>>();
        decode_query_settings(&rows)
    }
}

#[derive(Default)]
struct QueryControlValues {
    flags: Arc<HashMap<String, Value>>,
    toggles: Arc<HashMap<String, Value>>,
    modifiers: Vec<QueryModifier>,
    settings: Vec<(Value, Value)>,
}

// Upstream flags.clj/flags_impl.clj (b97a1164) keep query controls in a shared
// WAL-refreshed snapshot, not one database checkout per query. Only planner and
// topic/throttle controls are cached here: failover, authorization, rate limits,
// rules and app status deliberately keep their existing fresh reads.
//
// The short lease is a backstop for an interrupted/lagging WAL stream, not the
// primary invalidation mechanism. An expired or invalidated snapshot is never
// returned on a reload error. Constructors used without WAL wiring stay uncached.
const QUERY_CONTROL_MAX_AGE: Duration = Duration::from_secs(1);

/// Operational snapshot owned by the production invalidation sink. Critical
/// policy readers never consult it; construction is part of production wiring.
pub struct QueryControlCache {
    client: PostgresClientSource,
    config_app_id: Uuid,
    sql: String,
    state: RwLock<QueryControlCacheState>,
    loading: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct QueryControlCacheState {
    generation: u64,
    loaded: Option<(Instant, Arc<QueryControlSnapshot>)>,
}

struct QueryControlWriteGuard<'a>(&'a QueryControlCache);

impl Drop for QueryControlWriteGuard<'_> {
    fn drop(&mut self) {
        // Also evict on cancellation or an ambiguous transaction error, matching
        // upstream's before/after cache invalidation around local writes.
        self.0.invalidate();
    }
}

#[derive(Default)]
struct QueryControlSnapshot {
    flags: Arc<HashMap<String, Value>>,
    toggles: Arc<HashMap<String, Value>>,
    modifiers: HashMap<(String, i32), Vec<QueryModifier>>,
    settings: HashMap<i32, Vec<(Value, Value)>>,
}

impl QueryControlSnapshot {
    fn from_entities(entities: BTreeMap<(String, Uuid), Map<String, Value>>) -> Self {
        let mut snapshot = Self::default();
        // Entity UUID ordering matches the uncached control query's precedence.
        for ((namespace, _), entity) in entities {
            match namespace.as_str() {
                "flags" | "toggles" => {
                    let Some(setting) = entity.get("setting").and_then(Value::as_str) else {
                        continue;
                    };
                    let (values, allowed, label) = if namespace == "flags" {
                        (&mut snapshot.flags, PLANNER_FLAG_SETTINGS, "value")
                    } else {
                        (&mut snapshot.toggles, PLANNER_TOGGLE_SETTINGS, "toggled")
                    };
                    if (allowed.contains(&setting)
                        || (namespace == "flags" && RUNTIME_FLAG_SETTINGS.contains(&setting)))
                        && let Some(value) = entity.get(label)
                    {
                        Arc::make_mut(values).insert(setting.to_owned(), value.clone());
                    }
                }
                "query-modifiers" => {
                    let Some(app) = entity.get("app-id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(hash) = entity
                        .get("query-hash")
                        .and_then(Value::as_i64)
                        .and_then(|hash| i32::try_from(hash).ok())
                    else {
                        continue;
                    };
                    let Some(modifier) = Uuid::parse_str(app).ok().and_then(|app_id| {
                        decode_query_modifier(
                            &entity["app-id"],
                            &entity["query-hash"],
                            entity.get("etype")?,
                            entity.get("dollar-params")?,
                            app_id,
                            hash,
                        )
                    }) else {
                        continue;
                    };
                    snapshot
                        .modifiers
                        .entry((app.to_owned(), hash))
                        .or_default()
                        .push(modifier);
                }
                "query-flags" => {
                    let Some(hash) = entity
                        .get("query-hash")
                        .and_then(Value::as_i64)
                        .and_then(|hash| i32::try_from(hash).ok())
                    else {
                        continue;
                    };
                    if let (Some(setting), Some(value)) =
                        (entity.get("setting"), entity.get("value"))
                    {
                        snapshot
                            .settings
                            .entry(hash)
                            .or_default()
                            .push((setting.clone(), value.clone()));
                    }
                }
                _ => {}
            }
        }
        snapshot
    }

    fn for_query(&self, app_id: Uuid, hash: i32) -> QueryControlValues {
        QueryControlValues {
            flags: Arc::clone(&self.flags),
            toggles: Arc::clone(&self.toggles),
            modifiers: self
                .modifiers
                .get(&(app_id.to_string(), hash))
                .cloned()
                .unwrap_or_default(),
            settings: self.settings.get(&hash).cloned().unwrap_or_default(),
        }
    }
}

impl QueryControlCache {
    pub(crate) fn matches_config_app(&self, app_id: Uuid) -> bool {
        self.config_app_id == app_id
    }

    pub(crate) async fn runtime_flags(&self) -> Result<Arc<HashMap<String, Value>>, ApiError> {
        self.snapshot()
            .await
            .map(|snapshot| Arc::clone(&snapshot.flags))
            .map_err(internal_error)
    }
    pub(crate) fn from_pool(pool: PostgresPool) -> Self {
        Self::new(pool, INSTANT_CONFIG_APP_ID)
    }

    pub(crate) fn new(pool: PostgresPool, config_app_id: Uuid) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id,
            // As on the uncached path, expose only the immutable typed config
            // identity to the planner. No application query/value is interpolated.
            sql: format!(
                "SELECT a.etype, t.entity_id, a.label, t.value
                FROM attrs a JOIN triples t ON t.app_id=a.app_id AND t.attr_id=a.id
                WHERE a.app_id='{config_app_id}'::uuid
                  AND a.etype IN ('flags','toggles','query-modifiers','query-flags')
                ORDER BY a.etype, t.entity_id, a.label"
            ),
            state: RwLock::new(QueryControlCacheState::default()),
            loading: tokio::sync::Mutex::new(()),
        }
    }

    fn invalidate(&self) {
        let retired = {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.generation = state.generation.wrapping_add(1);
            state.loaded.take()
        };
        drop(retired);
    }

    fn cached(&self) -> Option<Arc<QueryControlSnapshot>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .loaded
            .as_ref()
            .filter(|(at, _)| at.elapsed() < QUERY_CONTROL_MAX_AGE)
            .map(|(_, snapshot)| Arc::clone(snapshot))
    }

    fn try_cached(&self) -> Option<Arc<QueryControlSnapshot>> {
        // A held data lease must not queue behind snapshot writers or reloads.
        // Invalidation clears loaded; the existing one-second age cap remains.
        self.state
            .try_read()
            .ok()?
            .loaded
            .as_ref()
            .filter(|(at, _)| at.elapsed() < QUERY_CONTROL_MAX_AGE)
            .map(|(_, snapshot)| Arc::clone(snapshot))
    }

    async fn snapshot(&self) -> Result<Arc<QueryControlSnapshot>, FailoverGateError> {
        if let Some(snapshot) = self.cached() {
            return Ok(snapshot);
        }
        // Coalesce concurrent reloads without holding a database lease while
        // waiting for the loader. Cancellation releases this guard for a retry.
        let _loading = self.loading.lock().await;
        loop {
            if let Some(snapshot) = self.cached() {
                return Ok(snapshot);
            }
            let generation = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generation;
            let started = Instant::now();
            let snapshot = Arc::new(self.load().await?);
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.generation == generation {
                state.loaded = Some((started, Arc::clone(&snapshot)));
                return Ok(snapshot);
            }
            // WAL arrived during the read: never publish an older generation.
            // Retry against a new database statement snapshot instead.
        }
    }

    async fn load(&self) -> Result<QueryControlSnapshot, FailoverGateError> {
        let rows = {
            let client = self.client.acquire().await?;
            let statement = client.prepare_cached(&self.sql).await?;
            client.query(&statement, &[]).await?
        };
        let mut entities = BTreeMap::<(String, Uuid), Map<String, Value>>::new();
        for row in rows {
            entities
                .entry((row.get(0), row.get(1)))
                .or_default()
                .insert(row.get(2), row.get(3));
        }
        Ok(QueryControlSnapshot::from_entities(entities))
    }
}

impl crate::RealtimeInvalidationConsumer for QueryControlCache {
    fn requires_local_delivery(&self) -> bool {
        false
    }

    async fn stage(
        &self,
        outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), String> {
        if outcome
            .record
            .as_ref()
            .is_some_and(|record| record.app_id == self.config_app_id)
        {
            self.invalidate();
        }
        Ok(())
    }

    async fn deliver(
        &self,
        _outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn evict_cache(&self, eviction: &instant_wal::WalCacheEviction) -> Result<(), String> {
        if let instant_wal::WalCacheEviction::Attribute { app_id, .. }
        | instant_wal::WalCacheEviction::App { app_id, .. } = eviction
            && app_id.is_none_or(|app| app == self.config_app_id)
        {
            self.invalidate();
        }
        Ok(())
    }
}

fn decode_query_setting(
    setting: &Value,
    value: &Value,
) -> Result<PostgresQuerySetting, FailoverGateError> {
    let setting = setting.as_str().ok_or_else(|| {
        FailoverGateError::Config("query-flags setting must be a string".to_owned())
    })?;
    let value = value.as_str().ok_or_else(|| {
        FailoverGateError::Config("query-flags value must be a string".to_owned())
    })?;
    Ok(PostgresQuerySetting {
        setting: setting.to_owned(),
        value: value.to_owned(),
    })
}

fn decode_query_settings(
    rows: &[(Value, Value)],
) -> Result<Vec<PostgresQuerySetting>, FailoverGateError> {
    rows.iter()
        .map(|(setting, value)| decode_query_setting(setting, value))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueryModifier {
    etype: String,
    params: Map<String, Value>,
}

fn decode_query_modifier(
    app_id: &Value,
    query_hash: &Value,
    etype: &Value,
    params: &Value,
    expected_app_id: Uuid,
    expected_query_hash: i32,
) -> Option<QueryModifier> {
    if app_id
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        != Some(expected_app_id)
        || query_hash.as_i64() != Some(i64::from(expected_query_hash))
    {
        return None;
    }
    Some(QueryModifier {
        etype: etype.as_str()?.to_owned(),
        params: params.as_object()?.clone(),
    })
}

/// Pinned to `instant.db.instaql/add-query-modifiers` at oracle 4e0a1bd8:
/// `(update-in o [etype :$] merge params)` for each config result in order.
fn add_query_modifiers(query: &Value, modifiers: &[QueryModifier]) -> Value {
    let mut query = query.clone();
    let Some(root) = query.as_object_mut() else {
        return query;
    };
    for modifier in modifiers {
        let entity = root
            .entry(modifier.etype.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(entity) = entity.as_object_mut() else {
            continue;
        };
        let dollar = entity
            .entry("$".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if dollar.is_null() {
            *dollar = Value::Object(Map::new());
        }
        let Some(dollar) = dollar.as_object_mut() else {
            continue;
        };
        dollar.extend(modifier.params.clone());
    }
    query
}

impl ReplicaReadPolicy for PostgresFailoverGate {
    fn enabled(&self) -> ReplicaReadPolicyFuture<'_> {
        Box::pin(async move {
            self.is_toggled("refresh-from-replica")
                .await
                .map_err(internal_error)
        })
    }
}

impl CombineTransactsPolicy for PostgresFailoverGate {
    fn combine_transacts(&self, _app_id: Uuid) -> CombineTransactsFuture<'_> {
        Box::pin(async move {
            let value = self
                .flag_value("combine-transacts")
                .await
                .map_err(internal_error)?;
            Ok(combine_transacts_flag(value.as_ref()))
        })
    }
}

impl QueryModifierPolicy for PostgresFailoverGate {
    fn apply<'a>(
        &'a self,
        app_id: Uuid,
        original_query_hash: i32,
        query: &'a Value,
    ) -> QueryModifierPolicyFuture<'a> {
        Box::pin(async move {
            let modifiers = self
                .query_modifiers(app_id, original_query_hash)
                .await
                .map_err(internal_error)?;
            Ok(add_query_modifiers(query, &modifiers))
        })
    }
}

impl QuerySettingsPolicy for PostgresFailoverGate {
    fn settings(&self, original_query_hash: i32) -> QuerySettingsPolicyFuture<'_> {
        Box::pin(async move {
            self.query_settings(original_query_hash)
                .await
                .map_err(internal_error)
        })
    }
}

impl QueryPlannerPolicy for PostgresFailoverGate {
    fn options<'a>(&'a self, query: &'a Value) -> QueryPlannerPolicyFuture<'a> {
        Box::pin(async move {
            let (flags, toggles) = self
                .planner_config_values(PLANNER_FLAG_SETTINGS, PLANNER_TOGGLE_SETTINGS)
                .await
                .map_err(internal_error)?;
            let query_hash = normalized_query_hash(query).map_err(query_validation_error)?;
            query_planner_options_from_config(&flags, &toggles, query_hash)
        })
    }
}

impl QueryPreparationPolicy for PostgresFailoverGate {
    fn try_prepare_no_io(
        &self,
        app_id: Uuid,
        original: &Value,
    ) -> Option<Result<PreparedInstantQuery, ApiError>> {
        let snapshot = self.query_controls.as_ref()?.try_cached()?;
        let hash = match normalized_query_hash(original) {
            Ok(hash) => hash,
            Err(error) => return Some(Err(query_validation_error(error))),
        };
        Some(prepare_query_controls(
            app_id,
            original,
            hash,
            snapshot.for_query(app_id, hash),
        ))
    }

    fn may_overlap_authorization(&self) -> bool {
        // The live overlap candidate increased measured control-pool pressure
        // without an end-to-end latency win. Keep production admission
        // sequential; the explicit opt-in remains available to custom policies.
        false
    }

    fn prepare<'a>(
        &'a self,
        app_id: Uuid,
        original: &'a Value,
    ) -> QueryPreparationPolicyFuture<'a> {
        Box::pin(async move {
            let original_hash = normalized_query_hash(original).map_err(query_validation_error)?;
            let controls = self
                .query_control_values(app_id, original_hash)
                .await
                .map_err(internal_error)?;
            prepare_query_controls(app_id, original, original_hash, controls)
        })
    }
}

fn prepare_query_controls(
    app_id: Uuid,
    original: &Value,
    original_hash: i32,
    controls: QueryControlValues,
) -> Result<PreparedInstantQuery, ApiError> {
    // Both paths preserve the same error/override ordering and decoding.
    let postgres_settings = decode_query_settings(&controls.settings).map_err(internal_error)?;
    let mut planner_options =
        query_planner_options_from_config(&controls.flags, &controls.toggles, original_hash)?;
    apply_app_row_hint_policy(&mut planner_options, &controls.flags, app_id);
    Ok(PreparedInstantQuery {
        effective: add_query_modifiers(original, &controls.modifiers),
        original_hash,
        planner_options,
        postgres_settings,
        // Permission attribute projection is on by default; the control toggle
        // is a rollback switch for query/refresh view checks only.
        permission_attribute_projection: !toggle_with_default(
            &controls.toggles,
            "disable-permission-attribute-projection",
            false,
        ),
        // Root frame sharing is on by default; the toggle only disables
        // cross-session reuse, keeping the single-snapshot frame path.
        shared_root_frames: !toggle_with_default(
            &controls.toggles,
            "disable-shared-root-frames",
            false,
        ),
    }
    .with_explicit_postgres_settings_precedence())
}

fn apply_app_row_hint_policy(
    options: &mut MatchCteOptions,
    flags: &HashMap<String, Value>,
    app_id: Uuid,
) {
    if flags
        .get("disable-row-hints-apps")
        .and_then(Value::as_array)
        .is_some_and(|apps| {
            apps.iter()
                .filter_map(Value::as_str)
                .filter_map(|value| Uuid::parse_str(value).ok())
                .any(|id| id == app_id)
        })
    {
        options.row_hints_enabled = Some(false);
    }
}

fn query_planner_options_from_config(
    flags: &HashMap<String, Value>,
    toggles: &HashMap<String, Value>,
    query_hash: i32,
) -> Result<MatchCteOptions, ApiError> {
    let always_materialize_attr_ids = flags
        .get("always-materialize-attr-ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| Uuid::parse_str(value).ok())
        .collect();
    let row_hint_threshold = match flags.get("row-hint-threshold") {
        None | Some(Value::Null) => 10_000,
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_u64() {
                value
            } else if number.as_i64().is_some_and(|value| value <= 0) {
                0
            } else if let Some(value) = number.as_f64() {
                if value <= 0.0 {
                    0
                } else {
                    value.ceil().to_string().parse::<u64>().unwrap_or(u64::MAX)
                }
            } else {
                return Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "config flag `row-hint-threshold` is outside the supported numeric range",
                ));
            }
        }
        Some(_) => {
            return Err(ApiError::new(
                ApiErrorType::Unknown,
                "config flag `row-hint-threshold` must be numeric",
            ));
        }
    };
    let disable_pg_hints = toggle_with_default(toggles, "disable-pg-hints", false)
        || hash_flag_contains(flags.get("disable-hint-query-hashes"), query_hash);
    let enable_pg_hints = toggle_with_default(toggles, "pg-hints-by-default", true)
        || hash_flag_contains(flags.get("use-hint-query-hashes"), query_hash);
    let row_hints_enabled = (toggle_with_default(toggles, "enable-row-hints-globally", true)
        || hash_flag_contains(flags.get("enable-row-hints-query-hashes"), query_hash))
        && !hash_flag_contains(flags.get("disable-row-hints-query-hashes"), query_hash);
    let disable_is_null_fix = toggle_with_default(toggles, "disable-is-null-fix", false);
    let hint_plan_av_fix = !toggle_with_default(toggles, "disable-hint-plan-av-fix", false);
    let like_count_optimization =
        !toggle_with_default(toggles, "disable-like-count-optimization", false);
    let fix_is_null_index_cost = toggle_with_default(toggles, "fix-is-null-index-cost", true);
    let cap_symbol_map_by_page_limit =
        !toggle_with_default(toggles, "disable-cap-symbol-map-by-page-limit", false);
    let ensure_empty_symbol_values_for_topics =
        toggle_with_default(toggles, "ensure-empty-symbol-values-for-topics", false);
    let test_pg_hint_plan = toggle_with_default(toggles, "test-pg-hint-plan", false);
    Ok(MatchCteOptions {
        candidate_entity_scans: Some(!toggle_with_default(
            toggles,
            "disable-candidate-entity-scans",
            false,
        )),
        optional_order_seed_binding: Some(!toggle_with_default(
            toggles,
            "disable-optional-order-seed-binding",
            false,
        )),
        always_materialize_attr_ids,
        row_hint_threshold: Some(row_hint_threshold),
        pg_hints_enabled: Some(enable_pg_hints && !disable_pg_hints),
        row_hints_enabled: Some(row_hints_enabled),
        predicate_options: instant_datalog::PredicateOptions {
            disable_is_null_fix,
            forward_is_null_exists: toggle_with_default(toggles, "forward-is-null-exists", true),
        },
        hint_plan_av_fix: Some(hint_plan_av_fix),
        like_count_optimization: Some(like_count_optimization),
        fix_is_null_index_cost: Some(fix_is_null_index_cost),
        cap_symbol_map_by_page_limit: Some(cap_symbol_map_by_page_limit),
        ensure_empty_symbol_values_for_topics: Some(ensure_empty_symbol_values_for_topics),
        test_pg_hint_plan: Some(test_pg_hint_plan),
        optional_number_order_scan_hints: Some(toggle_with_default(
            toggles,
            "hint-plan-optional-number-order-scan",
            false,
        )),
        small_page_jit_off: Some(toggle_with_default(
            toggles,
            "hint-plan-small-page-jit-off",
            true,
        )),
        ..MatchCteOptions::default()
    })
}

fn toggle_with_default(toggles: &HashMap<String, Value>, setting: &str, default: bool) -> bool {
    toggles.get(setting).map_or(default, toggle_truthy)
}

fn hash_flag_contains(value: Option<&Value>, query_hash: i32) -> bool {
    value.and_then(Value::as_array).is_some_and(|hashes| {
        hashes.iter().any(|value| {
            value.as_i64() == Some(i64::from(query_hash))
                || value.as_u64() == u64::try_from(query_hash).ok()
                || value.as_f64() == Some(f64::from(query_hash))
        })
    })
}

fn toggle_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn combine_transacts_flag(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(value) => toggle_truthy(value),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    async fn query_control_fixture() -> Option<(PostgresFailoverGate, PostgresPool)> {
        let Some(database_url) = instant_test_support::test_database_url("query_control_fixture")
        else {
            return None;
        };
        let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
        let client = pool.acquire().await.unwrap();
        client
            .batch_execute(
                "CREATE TEMP TABLE attrs (
                   id uuid PRIMARY KEY, app_id uuid NOT NULL,
                   etype text NOT NULL, label text NOT NULL
                 );
                 CREATE TEMP TABLE triples (
                   app_id uuid NOT NULL, entity_id uuid NOT NULL,
                   attr_id uuid NOT NULL, value jsonb NOT NULL
                 );",
            )
            .await
            .unwrap();
        drop(client);
        let gate = PostgresFailoverGate::from_pool(pool.clone()).with_config_app_id(Uuid::new_v4());
        Some((gate, pool))
    }

    async fn insert_control_attrs(client: &Client, config_app_id: Uuid) {
        for (namespace, labels) in [
            ("flags", &["setting", "value"][..]),
            ("toggles", &["setting", "toggled"][..]),
            (
                "query-modifiers",
                &["app-id", "query-hash", "etype", "dollar-params"][..],
            ),
            ("query-flags", &["query-hash", "setting", "value"][..]),
        ] {
            for label in labels {
                client
                    .execute(
                        "INSERT INTO attrs (id, app_id, etype, label) VALUES ($1, $2, $3, $4)",
                        &[&Uuid::new_v4(), &config_app_id, &namespace, label],
                    )
                    .await
                    .unwrap();
            }
        }
    }

    async fn insert_control_row(
        client: &Client,
        config_app_id: Uuid,
        namespace: &str,
        entity_id: Uuid,
        values: &[(&str, Value)],
    ) {
        for (label, value) in values {
            let inserted = client
                .execute(
                    "INSERT INTO triples (app_id, entity_id, attr_id, value)
                     SELECT $1, $2, id, $3 FROM attrs
                     WHERE app_id = $1 AND etype = $4 AND label = $5",
                    &[&config_app_id, &entity_id, value, &namespace, label],
                )
                .await
                .unwrap();
            assert_eq!(inserted, 1);
        }
    }

    #[test]
    fn planner_snapshot_preserves_scope_precedence_and_per_query_errors() {
        let app = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut entities = BTreeMap::new();
        let mut add = |namespace: &str, id: u128, value: Value| {
            entities.insert(
                (namespace.to_owned(), Uuid::from_u128(id)),
                value.as_object().unwrap().clone(),
            );
        };
        add(
            "flags",
            1,
            json!({"setting":"row-hint-threshold","value":10}),
        );
        add(
            "flags",
            2,
            json!({"setting":"row-hint-threshold","value":20}),
        );
        add("flags", 3, json!({"setting":"failing-over","value":true}));
        add(
            "toggles",
            4,
            json!({"setting":"pg-hints-by-default","toggled":false}),
        );
        add(
            "query-modifiers",
            5,
            json!({"app-id":app,"query-hash":7,"etype":"users","dollar-params":{"limit":1}}),
        );
        add(
            "query-modifiers",
            6,
            json!({"app-id":app,"query-hash":7,"etype":"users","dollar-params":{"limit":2}}),
        );
        add(
            "query-modifiers",
            7,
            json!({"app-id":other,"query-hash":7,"etype":"users","dollar-params":{"limit":3}}),
        );
        add(
            "query-modifiers",
            8,
            json!({"app-id":app,"query-hash":8,"etype":"users","dollar-params":{"limit":4}}),
        );
        add(
            "query-flags",
            9,
            json!({"query-hash":7,"setting":"jit","value":"on"}),
        );
        // Do not let malformed controls for an unrelated hash poison this one.
        add(
            "query-flags",
            10,
            json!({"query-hash":8,"setting":"jit","value":false}),
        );
        let snapshot = QueryControlSnapshot::from_entities(entities);
        let controls = snapshot.for_query(app, 7);
        assert_eq!(controls.flags["row-hint-threshold"], json!(20));
        assert!(!controls.flags.contains_key("failing-over"));
        assert_eq!(controls.toggles["pg-hints-by-default"], json!(false));
        assert_eq!(
            add_query_modifiers(&json!({"users":{}}), &controls.modifiers),
            json!({"users":{"$":{"limit":2}}})
        );
        assert_eq!(
            decode_query_settings(&controls.settings).unwrap()[0].value,
            "on"
        );
        assert_eq!(
            snapshot.for_query(other, 7).modifiers[0].params["limit"],
            json!(3)
        );
        assert_eq!(
            snapshot.for_query(app, 8).modifiers[0].params["limit"],
            json!(4)
        );
        assert!(decode_query_settings(&snapshot.for_query(app, 8).settings).is_err());
        assert!(snapshot.for_query(app, 9).modifiers.is_empty());
    }

    #[tokio::test]
    async fn planner_snapshot_coalesces_reads_and_keeps_failover_checks_live() {
        use crate::RealtimeInvalidationConsumer as _;
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "toggles",
            Uuid::new_v4(),
            &[
                ("setting", json!("hint-plan-small-page-jit-off")),
                ("toggled", json!(false)),
            ],
        )
        .await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "toggles",
            Uuid::new_v4(),
            &[("setting", json!("failing-over")), ("toggled", json!(true))],
        )
        .await;
        drop(client);
        let cache = Arc::new(QueryControlCache::new(pool.clone(), gate.config_app_id));
        let gate = gate.with_query_control_cache(Arc::clone(&cache));
        let app = Uuid::new_v4();
        let query = json!({"users":{"$":{"limit":10}}});
        let before = pool.acquisition_count();
        let (a, b, c, d) = tokio::join!(
            gate.prepare(app, &query),
            gate.prepare(app, &query),
            gate.options(&query),
            gate.settings(7)
        );
        assert_eq!(a.unwrap().planner_options.small_page_jit_off, Some(false));
        assert_eq!(b.unwrap().planner_options.small_page_jit_off, Some(false));
        c.unwrap();
        d.unwrap();
        assert_eq!(pool.acquisition_count() - before, 1);
        assert!(gate.is_failing_over().await.unwrap());
        assert_eq!(
            pool.acquisition_count() - before,
            2,
            "critical failover reads must not use the planner snapshot"
        );
        let client = pool.acquire().await.unwrap();
        client.execute("UPDATE triples SET value='true'::jsonb WHERE app_id=$1 AND attr_id=(SELECT id FROM attrs WHERE app_id=$1 AND etype='toggles' AND label='toggled')", &[&gate.config_app_id]).await.unwrap();
        drop(client);
        cache
            .evict_cache(&instant_wal::WalCacheEviction::Attribute {
                app_id: Some(gate.config_app_id),
                attr_id: None,
            })
            .await
            .unwrap();
        let after = pool.acquisition_count();
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .small_page_jit_off,
            Some(true)
        );
        assert_eq!(pool.acquisition_count() - after, 1);
    }

    #[tokio::test]
    async fn planner_snapshot_invalidation_during_reload_cannot_publish_old_generation() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let cache = Arc::new(QueryControlCache::new(pool.clone(), gate.config_app_id));
        let held = pool.acquire().await.unwrap();
        let before = pool.acquisition_count();
        let loader = Arc::clone(&cache);
        let task = tokio::spawn(async move {
            loader.snapshot().await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool.status().waiting == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cache.invalidate();
        drop(held);
        task.await.unwrap();
        assert_eq!(
            pool.acquisition_count() - before,
            2,
            "the invalidated loader must repeat its statement snapshot"
        );
        assert!(cache.cached().is_some());
    }

    #[tokio::test]
    async fn planner_snapshot_expiry_reloads_and_errors_never_return_stale_controls() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let mut cache = QueryControlCache::new(pool.clone(), gate.config_app_id);
        cache.snapshot().await.unwrap();
        cache.state.write().unwrap().loaded.as_mut().unwrap().0 =
            Instant::now() - QUERY_CONTROL_MAX_AGE;
        let before = pool.acquisition_count();
        cache.snapshot().await.unwrap();
        assert_eq!(
            pool.acquisition_count() - before,
            1,
            "expired snapshots must be reloaded even without WAL"
        );
        cache.invalidate();
        cache.sql = "SELECT missing_planner_snapshot_column".to_owned();
        assert!(cache.snapshot().await.is_err());
        assert!(cache.cached().is_none());
    }

    #[tokio::test]
    async fn planner_snapshot_cancelled_reload_releases_singleflight_lock() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let cache = QueryControlCache::new(pool.clone(), gate.config_app_id);
        let held = pool.acquire().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), cache.snapshot())
                .await
                .is_err()
        );
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), cache.snapshot())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn runtime_topic_and_throttle_controls_share_immutable_snapshot_but_not_critical_gates() {
        use crate::{CoarseTopicsPolicy as _, RefreshThrottlePolicy as _};
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app = Uuid::new_v4();
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        for (setting, value) in [
            ("coarse-topics-apps", json!([app])),
            ("refresh-throttled-apps", json!([app])),
            ("refresh-throttle-ms", json!(25)),
        ] {
            insert_control_row(
                &client,
                gate.config_app_id,
                "flags",
                Uuid::new_v4(),
                &[("setting", json!(setting)), ("value", value)],
            )
            .await;
        }
        drop(client);
        let cache = Arc::new(QueryControlCache::new(pool.clone(), gate.config_app_id));
        let coarse = crate::PostgresCoarseTopicsPolicy::from_pool(pool.clone())
            .with_config_app_id(gate.config_app_id)
            .with_query_control_cache(Arc::clone(&cache));
        let throttle = crate::PostgresRefreshThrottlePolicy::from_pool(pool.clone())
            .with_config_app_id(gate.config_app_id)
            .with_query_control_cache(Arc::clone(&cache));
        let gate = gate.with_query_control_cache(Arc::clone(&cache));
        let before = pool.acquisition_count();
        let query = json!({"users":{}});
        let (topics, delay, prepared) = tokio::join!(
            coarse.use_coarse_topics(app),
            throttle.delay_after_noop(app),
            gate.prepare(app, &query)
        );
        assert!(topics.unwrap());
        assert_eq!(delay.unwrap(), Some(Duration::from_millis(25)));
        prepared.unwrap();
        assert_eq!(
            pool.acquisition_count() - before,
            1,
            "operational readers should share one reload"
        );
        assert!(!coarse.use_coarse_topics(Uuid::new_v4()).await.unwrap());
        assert_eq!(
            throttle.delay_after_noop(Uuid::new_v4()).await.unwrap(),
            None
        );
        let a = gate.query_control_values(app, 1).await.unwrap();
        let b = gate.query_control_values(app, 2).await.unwrap();
        assert!(
            Arc::ptr_eq(&a.flags, &b.flags),
            "do not deep-copy global flag values per query"
        );
        assert!(!gate.is_failing_over().await.unwrap());
        assert_eq!(
            pool.acquisition_count() - before,
            2,
            "critical gates remain fresh SQL reads"
        );
    }

    #[test]
    fn candidate_scan_rollback_does_not_disable_other_planner_features() {
        let options =
            query_planner_options_from_config(&HashMap::new(), &HashMap::new(), 1).unwrap();
        assert_eq!(options.candidate_entity_scans, Some(true));
        assert_eq!(options.optional_order_seed_binding, Some(true));
        let rollback = query_planner_options_from_config(
            &HashMap::new(),
            &HashMap::from([("disable-candidate-entity-scans".into(), json!(true))]),
            1,
        )
        .unwrap();
        assert_eq!(rollback.candidate_entity_scans, Some(false));
        let order_rollback = query_planner_options_from_config(
            &HashMap::new(),
            &HashMap::from([("disable-optional-order-seed-binding".into(), json!(true))]),
            1,
        )
        .unwrap();
        assert_eq!(order_rollback.optional_order_seed_binding, Some(false));
        assert_eq!(
            order_rollback.candidate_entity_scans,
            options.candidate_entity_scans
        );
        assert_eq!(options.pg_hints_enabled, rollback.pg_hints_enabled);
        assert_eq!(options.small_page_jit_off, rollback.small_page_jit_off);
    }

    #[tokio::test]
    async fn planner_snapshot_wal_staging_is_scoped_and_precedes_delivery() {
        use crate::RealtimeInvalidationConsumer as _;
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let cache = QueryControlCache::new(pool, gate.config_app_id);
        cache.snapshot().await.unwrap();
        assert!(
            !cache.requires_local_delivery(),
            "planner caching must not disable WAL batching"
        );
        let record = instant_wal::WalRecord {
            app_id: Uuid::new_v4(),
            tx_id: 1,
            isn: instant_wal::Isn::default(),
            previous_isn: None,
            tx_created_at: None,
            tx_bytes: 0,
            next_lsn: instant_wal::Lsn::default(),
            attr_changes: vec![],
            ident_changes: vec![],
            triple_changes: vec![],
            messages: vec![],
            wal_logs: vec![],
            combined_count: 1,
        };
        let mut outcome = instant_realtime::ReplicationPipelineOutcome {
            record: Some(record),
            ..Default::default()
        };
        cache.stage_persisted(&outcome, None).await.unwrap();
        assert!(
            cache.cached().is_some(),
            "unrelated app WAL must not evict the config snapshot"
        );
        outcome.record.as_mut().unwrap().app_id = gate.config_app_id;
        cache.stage_persisted(&outcome, None).await.unwrap();
        assert!(
            cache.cached().is_none(),
            "WAL staging must evict before delivery is queued"
        );
        cache.snapshot().await.unwrap();
        cache
            .evict_cache(&instant_wal::WalCacheEviction::Attribute {
                app_id: None,
                attr_id: None,
            })
            .await
            .unwrap();
        assert!(
            cache.cached().is_none(),
            "incomplete metadata images require conservative eviction"
        );
    }

    #[tokio::test]
    async fn explicit_jit_query_controls_override_the_hint_freshly_in_both_policy_paths() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app = Uuid::new_v4();
        let original = json!({"users":{"$":{"limit":20}}});
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        let entity = Uuid::new_v4();
        insert_control_row(
            &client,
            gate.config_app_id,
            "query-flags",
            entity,
            &[
                (
                    "query-hash",
                    json!(normalized_query_hash(&original).unwrap()),
                ),
                ("setting", json!("jit")),
                ("value", json!("on")),
            ],
        )
        .await;
        drop(client);
        let before = pool.acquisition_count();
        let combined = gate.prepare(app, &original).await.unwrap();
        assert_eq!(
            pool.acquisition_count(),
            before + 1,
            "precedence guard must not add a checkout"
        );
        assert_eq!(combined.planner_options.small_page_jit_off, Some(false));
        assert_eq!(
            combined.postgres_settings,
            vec![PostgresQuerySetting {
                setting: "jit".into(),
                value: "on".into()
            }]
        );
        let separate =
            crate::admin_data::prepare_instant_query(None, &gate, &gate, &gate, app, &original)
                .await
                .unwrap();
        assert_eq!(separate.planner_options, combined.planner_options);
        assert_eq!(separate.postgres_settings, combined.postgres_settings);
        let client = pool.acquire().await.unwrap();
        client
            .execute(
                "DELETE FROM triples WHERE app_id=$1 AND entity_id=$2",
                &[&gate.config_app_id, &entity],
            )
            .await
            .unwrap();
        drop(client);
        assert_eq!(
            gate.prepare(app, &original)
                .await
                .unwrap()
                .planner_options
                .small_page_jit_off,
            Some(true)
        );
    }

    #[tokio::test]
    async fn small_page_jit_rollback_is_a_fresh_control_read() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app = Uuid::new_v4();
        let query = json!({"users":{"$":{"limit":20}}});
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .small_page_jit_off,
            Some(true)
        );
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "toggles",
            Uuid::new_v4(),
            &[
                ("setting", json!("hint-plan-small-page-jit-off")),
                ("toggled", json!(false)),
            ],
        )
        .await;
        drop(client);
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .small_page_jit_off,
            Some(false)
        );
        let client = pool.acquire().await.unwrap();
        client.execute("UPDATE triples SET value='true'::jsonb WHERE app_id=$1 AND attr_id=(SELECT id FROM attrs WHERE app_id=$1 AND etype='toggles' AND label='toggled')", &[&gate.config_app_id]).await.unwrap();
        drop(client);
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .small_page_jit_off,
            Some(true)
        );
    }

    #[tokio::test]
    async fn forward_null_exists_rollback_is_a_fresh_control_read() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app = Uuid::new_v4();
        let query = json!({"messages":{"$":{"where":{"revision":{"$ne":"fresh"}}}}});
        assert!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .predicate_options
                .forward_is_null_exists
        );
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "toggles",
            Uuid::new_v4(),
            &[
                ("setting", json!("forward-is-null-exists")),
                ("toggled", json!(false)),
            ],
        )
        .await;
        drop(client);
        assert!(
            !gate
                .prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .predicate_options
                .forward_is_null_exists
        );
        let client = pool.acquire().await.unwrap();
        client.execute("UPDATE triples SET value='true'::jsonb WHERE app_id=$1 AND attr_id=(SELECT id FROM attrs WHERE app_id=$1 AND etype='toggles' AND label='toggled')", &[&gate.config_app_id]).await.unwrap();
        drop(client);
        assert!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .predicate_options
                .forward_is_null_exists
        );
    }

    #[tokio::test]
    async fn optional_numeric_order_hint_rollback_is_a_fresh_control_read() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app = Uuid::new_v4();
        let query = json!({"messages":{"$":{"order":{"updatedAt":"desc"},"limit":20}}});
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .optional_number_order_scan_hints,
            Some(false)
        );
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "toggles",
            Uuid::new_v4(),
            &[
                ("setting", json!("hint-plan-optional-number-order-scan")),
                ("toggled", json!(true)),
            ],
        )
        .await;
        drop(client);
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .optional_number_order_scan_hints,
            Some(true)
        );
        let client = pool.acquire().await.unwrap();
        client.execute("UPDATE triples SET value='false'::jsonb WHERE app_id=$1 AND attr_id=(SELECT id FROM attrs WHERE app_id=$1 AND etype='toggles' AND label='toggled')", &[&gate.config_app_id]).await.unwrap();
        drop(client);
        assert_eq!(
            gate.prepare(app, &query)
                .await
                .unwrap()
                .planner_options
                .optional_number_order_scan_hints,
            Some(false)
        );
    }

    #[tokio::test]
    async fn live_query_controls_use_one_pool_lease() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        assert!(
            !gate.may_overlap_authorization(),
            "production controls must not re-enable the withdrawn overlap"
        );
        let app_id = Uuid::new_v4();
        let original = json!({"todos": {}});
        let original_hash = normalized_query_hash(&original).unwrap();
        let before = pool.acquisition_count();

        let prepared = crate::admin_data::prepare_instant_query(
            Some(&gate),
            &gate,
            &gate,
            &gate,
            app_id,
            &original,
        )
        .await
        .unwrap();

        assert_eq!(prepared.effective, original);
        assert_eq!(prepared.original_hash, original_hash);
        assert!(prepared.postgres_settings.is_empty());
        assert_eq!(prepared.planner_options.row_hint_threshold, Some(10_000));
        assert_eq!(pool.acquisition_count() - before, 1);
    }

    #[tokio::test]
    async fn no_io_preparation_requires_a_valid_unlocked_snapshot_and_never_reloads() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app = Uuid::new_v4();
        let query = json!({"todos": {}});
        assert!(gate.try_prepare_no_io(app, &query).is_none());
        let cache = Arc::new(QueryControlCache::new(pool.clone(), gate.config_app_id));
        let gate = gate.with_query_control_cache(Arc::clone(&cache));
        let before = pool.acquisition_count();
        assert!(gate.try_prepare_no_io(app, &query).is_none());
        assert_eq!(
            pool.acquisition_count(),
            before,
            "a cold snapshot must not reload here"
        );
        let fresh = gate.prepare(app, &query).await.unwrap();
        let loaded = pool.acquisition_count();
        let ready = gate.try_prepare_no_io(app, &query).unwrap().unwrap();
        assert_eq!(ready.effective, fresh.effective);
        assert_eq!(ready.original_hash, fresh.original_hash);
        assert_eq!(ready.planner_options, fresh.planner_options);
        assert_eq!(ready.postgres_settings, fresh.postgres_settings);
        assert_eq!(pool.acquisition_count(), loaded);
        assert!(
            !gate.may_overlap_authorization(),
            "the withdrawn overlap stays disabled"
        );
        {
            let _writer = cache.state.write().unwrap();
            assert!(
                gate.try_prepare_no_io(app, &query).is_none(),
                "do not wait for a snapshot writer while holding a data lease"
            );
        }
        {
            let mut state = cache.state.write().unwrap();
            state.loaded.as_mut().unwrap().0 = Instant::now() - QUERY_CONTROL_MAX_AGE;
        }
        assert!(
            gate.try_prepare_no_io(app, &query).is_none(),
            "never use expired operational controls"
        );
        gate.prepare(app, &query).await.unwrap();
        cache.invalidate();
        let invalidated = pool.acquisition_count();
        assert!(
            gate.try_prepare_no_io(app, &query).is_none(),
            "WAL invalidation must fence the fast path"
        );
        assert_eq!(pool.acquisition_count(), invalidated);
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "One fixture compares combined controls with the separate-policy path and a live update"
    )]
    async fn live_query_controls_preserve_order_scope_original_hash_and_freshness() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app_id = Uuid::new_v4();
        let other_app = Uuid::new_v4();
        let other_config_app = Uuid::new_v4();
        let original = json!({"todos": {}});
        let original_hash = normalized_query_hash(&original).unwrap();
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        insert_control_attrs(&client, other_config_app).await;
        for (config_app, target_app, entity, params) in [
            (
                gate.config_app_id,
                app_id,
                1,
                json!({"limit": 50, "offset": 2}),
            ),
            (gate.config_app_id, app_id, 2, json!({"limit": 10})),
            (gate.config_app_id, app_id, 3, json!(false)),
            (gate.config_app_id, other_app, 4, json!({"limit": 1})),
            (other_config_app, app_id, 5, json!({"limit": 99})),
        ]
        .into_iter()
        .rev()
        {
            insert_control_row(
                &client,
                config_app,
                "query-modifiers",
                Uuid::from_u128(entity),
                &[
                    ("app-id", json!(target_app)),
                    ("query-hash", json!(original_hash)),
                    ("etype", json!("todos")),
                    ("dollar-params", params),
                ],
            )
            .await;
        }
        for (entity, value) in [(7, "8MB"), (6, "4MB")] {
            insert_control_row(
                &client,
                gate.config_app_id,
                "query-flags",
                Uuid::from_u128(entity),
                &[
                    ("query-hash", json!(original_hash)),
                    ("setting", json!("work_mem")),
                    ("value", json!(value)),
                ],
            )
            .await;
        }
        for (config_app, value) in [(gate.config_app_id, 321), (other_config_app, 999)] {
            insert_control_row(
                &client,
                config_app,
                "flags",
                Uuid::new_v4(),
                &[
                    ("setting", json!("row-hint-threshold")),
                    ("value", json!(value)),
                ],
            )
            .await;
        }
        insert_control_row(
            &client,
            gate.config_app_id,
            "flags",
            Uuid::new_v4(),
            &[
                ("setting", json!("disable-hint-query-hashes")),
                ("value", json!([original_hash])),
            ],
        )
        .await;
        drop(client);

        let before = pool.acquisition_count();
        let prepared = crate::admin_data::prepare_instant_query(
            Some(&gate),
            &gate,
            &gate,
            &gate,
            app_id,
            &original,
        )
        .await
        .unwrap();
        assert_eq!(pool.acquisition_count() - before, 1);
        assert_eq!(
            prepared.effective,
            json!({"todos": {"$": {"limit": 10, "offset": 2}}})
        );
        assert_ne!(
            normalized_query_hash(&prepared.effective).unwrap(),
            original_hash
        );
        assert_eq!(prepared.original_hash, original_hash);
        assert_eq!(prepared.planner_options.row_hint_threshold, Some(321));
        assert_eq!(prepared.planner_options.pg_hints_enabled, Some(false));
        assert_eq!(
            prepared
                .postgres_settings
                .iter()
                .map(|s| s.value.as_str())
                .collect::<Vec<_>>(),
            ["4MB", "8MB"]
        );
        let separate =
            crate::admin_data::prepare_instant_query(None, &gate, &gate, &gate, app_id, &original)
                .await
                .unwrap();
        assert_eq!(prepared.effective, separate.effective);
        assert_eq!(prepared.postgres_settings, separate.postgres_settings);
        assert_eq!(prepared.planner_options, separate.planner_options);

        let client = pool.acquire().await.unwrap();
        client
            .execute(
                "UPDATE triples SET value = '456'::jsonb
             WHERE app_id = $1 AND attr_id IN (
               SELECT id FROM attrs WHERE app_id = $1 AND etype = 'flags' AND label = 'value'
             ) AND value = '321'::jsonb",
                &[&gate.config_app_id],
            )
            .await
            .unwrap();
        drop(client);
        let refreshed = gate.prepare(app_id, &original).await.unwrap();
        assert_eq!(refreshed.planner_options.row_hint_threshold, Some(456));
        let other = gate.prepare(other_app, &original).await.unwrap();
        assert_eq!(other.effective, json!({"todos": {"$": {"limit": 1}}}));
        assert_eq!(other.postgres_settings, prepared.postgres_settings);
    }

    #[tokio::test]
    async fn live_query_controls_keep_null_toggles_and_fail_closed_on_invalid_values() {
        let Some((gate, pool)) = query_control_fixture().await else {
            return;
        };
        let app_id = Uuid::new_v4();
        let original = json!({"todos": {}});
        let client = pool.acquire().await.unwrap();
        insert_control_attrs(&client, gate.config_app_id).await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "toggles",
            Uuid::new_v4(),
            &[
                ("setting", json!("pg-hints-by-default")),
                ("toggled", Value::Null),
            ],
        )
        .await;
        drop(client);
        assert_eq!(
            gate.prepare(app_id, &original)
                .await
                .unwrap()
                .planner_options
                .pg_hints_enabled,
            Some(false)
        );

        let client = pool.acquire().await.unwrap();
        insert_control_row(
            &client,
            gate.config_app_id,
            "query-flags",
            Uuid::new_v4(),
            &[
                (
                    "query-hash",
                    json!(normalized_query_hash(&original).unwrap()),
                ),
                ("setting", json!("work_mem")),
                ("value", json!(false)),
            ],
        )
        .await;
        insert_control_row(
            &client,
            gate.config_app_id,
            "flags",
            Uuid::new_v4(),
            &[
                ("setting", json!("row-hint-threshold")),
                ("value", json!("invalid")),
            ],
        )
        .await;
        drop(client);
        let error = gate.prepare(app_id, &original).await.unwrap_err();
        // Control-plane causes stay in the server log; clients only see that the
        // query failed closed.
        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");

        let client = pool.acquire().await.unwrap();
        client
            .execute(
                "DELETE FROM triples WHERE app_id = $1 AND attr_id IN (
               SELECT id FROM attrs WHERE app_id = $1 AND etype = 'query-flags'
             )",
                &[&gate.config_app_id],
            )
            .await
            .unwrap();
        drop(client);
        let error = gate.prepare(app_id, &original).await.unwrap_err();
        assert!(error.body().message.contains("row-hint-threshold"));
    }

    #[test]
    fn app_row_hint_opt_out_preserves_other_apps_and_index_hints() {
        let app = Uuid::new_v4();
        let flags = HashMap::from([("disable-row-hints-apps".to_owned(), json!([app]))]);
        let base = query_planner_options_from_config(&HashMap::new(), &HashMap::new(), 1).unwrap();
        let mut other = base.clone();
        apply_app_row_hint_policy(&mut other, &flags, Uuid::new_v4());
        assert_eq!(other, base);
        let mut opted_out = base.clone();
        apply_app_row_hint_policy(&mut opted_out, &flags, app);
        assert_eq!(opted_out.row_hints_enabled, Some(false));
        assert_eq!(opted_out.pg_hints_enabled, base.pg_hints_enabled);
    }

    #[test]
    fn toggle_truthiness_matches_clojure() {
        assert!(!toggle_truthy(&Value::Null));
        assert!(!toggle_truthy(&json!(false)));
        assert!(toggle_truthy(&json!(true)));
        assert!(toggle_truthy(&json!(0)));
        assert!(toggle_truthy(&json!("false")));
        assert!(toggle_truthy(&json!([])));
    }

    #[test]
    fn combine_transacts_defaults_true_and_honors_live_false() {
        assert!(combine_transacts_flag(None));
        assert!(combine_transacts_flag(Some(&Value::Null)));
        assert!(combine_transacts_flag(Some(&json!(true))));
        assert!(combine_transacts_flag(Some(&json!("enabled"))));
        assert!(!combine_transacts_flag(Some(&json!(false))));
    }

    #[test]
    fn query_modifiers_match_ordered_upstream_update_in_merge_fixture() {
        // Oracle: instant.db.instaql/add-query-modifiers at 4e0a1bd8.
        let query = json!({
            "users": {"$": {"limit": 100, "where": {"active": true}}, "posts": {}},
            "teams": {}
        });
        let modifiers = vec![
            QueryModifier {
                etype: "users".to_owned(),
                params: serde_json::from_value(json!({"limit": 50, "offset": 2})).unwrap(),
            },
            QueryModifier {
                etype: "users".to_owned(),
                params: serde_json::from_value(json!({"limit": 10})).unwrap(),
            },
            QueryModifier {
                etype: "missing".to_owned(),
                params: serde_json::from_value(json!({"limit": 1})).unwrap(),
            },
        ];
        assert_eq!(
            add_query_modifiers(&query, &modifiers),
            json!({
                "users": {"$": {"limit": 10, "offset": 2, "where": {"active": true}}, "posts": {}},
                "teams": {},
                "missing": {"$": {"limit": 1}}
            })
        );
    }

    #[test]
    fn malformed_and_non_matching_query_modifier_rows_are_ignored() {
        let app_id = Uuid::new_v4();
        let hash = -42;
        assert!(
            decode_query_modifier(
                &json!(Uuid::new_v4()),
                &json!(hash),
                &json!("users"),
                &json!({"limit": 1}),
                app_id,
                hash
            )
            .is_none()
        );
        assert!(
            decode_query_modifier(
                &json!(app_id),
                &json!(hash + 1),
                &json!("users"),
                &json!({"limit": 1}),
                app_id,
                hash
            )
            .is_none()
        );
        assert!(
            decode_query_modifier(
                &json!(app_id),
                &json!(hash),
                &json!("users"),
                &json!(["bad"]),
                app_id,
                hash
            )
            .is_none()
        );
        assert!(
            decode_query_modifier(
                &json!(app_id),
                &json!(hash),
                &json!(null),
                &json!({}),
                app_id,
                hash
            )
            .is_none()
        );
    }

    #[test]
    fn query_setting_decoder_matches_source_string_contract_and_fails_closed() {
        // Oracle: flags.clj 189-193 and jdbc/sql.clj 540-564 at 4e0a1bd8.
        assert_eq!(
            decode_query_settings(&[
                (json!("enable_nestloop"), json!("off")),
                (json!("enable_nestloop"), json!("on")),
                (json!("work_mem"), json!("64MB")),
            ])
            .unwrap(),
            vec![
                PostgresQuerySetting {
                    setting: "enable_nestloop".to_owned(),
                    value: "off".to_owned(),
                },
                PostgresQuerySetting {
                    setting: "enable_nestloop".to_owned(),
                    value: "on".to_owned(),
                },
                PostgresQuerySetting {
                    setting: "work_mem".to_owned(),
                    value: "64MB".to_owned(),
                },
            ]
        );
        assert!(decode_query_setting(&json!(false), &json!("off")).is_err());
        assert!(decode_query_setting(&json!("enable_nestloop"), &json!(false)).is_err());
    }

    #[test]
    fn query_planner_flags_preserve_defaults_uuid_filtering_and_numeric_thresholds() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let defaults =
            query_planner_options_from_config(&HashMap::new(), &HashMap::new(), 17).unwrap();
        assert_eq!(defaults.row_hint_threshold, Some(10_000));
        assert!(defaults.always_materialize_attr_ids.is_empty());
        assert_eq!(defaults.pg_hints_enabled, Some(true));
        assert_eq!(defaults.row_hints_enabled, Some(true));
        assert!(!defaults.predicate_options.disable_is_null_fix);
        assert!(defaults.predicate_options.forward_is_null_exists);
        assert_eq!(defaults.hint_plan_av_fix, Some(true));
        assert_eq!(defaults.like_count_optimization, Some(true));
        assert_eq!(defaults.fix_is_null_index_cost, Some(true));
        assert_eq!(defaults.cap_symbol_map_by_page_limit, Some(true));
        assert_eq!(defaults.ensure_empty_symbol_values_for_topics, Some(false));
        assert_eq!(defaults.test_pg_hint_plan, Some(false));
        assert_eq!(defaults.optional_number_order_scan_hints, Some(false));
        assert_eq!(defaults.small_page_jit_off, Some(true));

        let values = HashMap::from([
            (
                "always-materialize-attr-ids".to_owned(),
                json!([first, "invalid", second]),
            ),
            ("row-hint-threshold".to_owned(), json!(37.5)),
        ]);
        let options = query_planner_options_from_config(&values, &HashMap::new(), 17).unwrap();
        assert_eq!(options.always_materialize_attr_ids, vec![first, second]);
        assert_eq!(options.row_hint_threshold, Some(38));

        for value in [json!(0), json!(-1), json!(-0.5)] {
            let values = HashMap::from([("row-hint-threshold".to_owned(), value)]);
            assert_eq!(
                query_planner_options_from_config(&values, &HashMap::new(), 17)
                    .unwrap()
                    .row_hint_threshold,
                Some(0)
            );
        }
        let values = HashMap::from([("row-hint-threshold".to_owned(), json!("100"))]);
        assert!(query_planner_options_from_config(&values, &HashMap::new(), 17).is_err());
    }

    #[test]
    fn query_hash_controls_match_clojure_precedence() {
        let query_hash = -123;
        let flags = HashMap::from([
            ("use-hint-query-hashes".to_owned(), json!([query_hash])),
            (
                "enable-row-hints-query-hashes".to_owned(),
                json!([query_hash]),
            ),
        ]);
        let disabled_defaults = HashMap::from([
            ("pg-hints-by-default".to_owned(), json!(false)),
            ("enable-row-hints-globally".to_owned(), json!(false)),
        ]);
        let options =
            query_planner_options_from_config(&flags, &disabled_defaults, query_hash).unwrap();
        assert_eq!(options.pg_hints_enabled, Some(true));
        assert_eq!(options.row_hints_enabled, Some(true));

        let deny_flags = HashMap::from([
            ("disable-hint-query-hashes".to_owned(), json!([query_hash])),
            (
                "disable-row-hints-query-hashes".to_owned(),
                json!([query_hash]),
            ),
        ]);
        let options =
            query_planner_options_from_config(&deny_flags, &HashMap::new(), query_hash).unwrap();
        assert_eq!(options.pg_hints_enabled, Some(false));
        assert_eq!(options.row_hints_enabled, Some(false));

        let disabled_globally = HashMap::from([("disable-pg-hints".to_owned(), json!("truthy"))]);
        let options =
            query_planner_options_from_config(&flags, &disabled_globally, query_hash).unwrap();
        assert_eq!(options.pg_hints_enabled, Some(false));
    }

    #[test]
    fn shared_root_frames_toggle_rolls_back_sharing_only() {
        let query = json!({"posts": {}});
        let app_id = Uuid::new_v4();
        let enabled =
            prepare_query_controls(app_id, &query, 17, QueryControlValues::default()).unwrap();
        assert!(enabled.shared_root_frames);
        assert!(enabled.permission_attribute_projection);

        let mut controls = QueryControlValues::default();
        controls.toggles = Arc::new(HashMap::from([(
            "disable-shared-root-frames".to_owned(),
            json!(true),
        )]));
        let disabled = prepare_query_controls(app_id, &query, 17, controls).unwrap();
        assert!(!disabled.shared_root_frames);
        assert!(disabled.permission_attribute_projection);
    }

    #[test]
    fn planner_rollback_toggles_remain_independent() {
        let toggles = HashMap::from([
            ("disable-is-null-fix".to_owned(), json!(true)),
            ("disable-hint-plan-av-fix".to_owned(), json!(true)),
            (
                "disable-like-count-optimization".to_owned(),
                json!("truthy"),
            ),
            ("fix-is-null-index-cost".to_owned(), json!(false)),
            (
                "disable-cap-symbol-map-by-page-limit".to_owned(),
                json!(true),
            ),
            (
                "ensure-empty-symbol-values-for-topics".to_owned(),
                json!(true),
            ),
            (
                "hint-plan-optional-number-order-scan".to_owned(),
                json!(true),
            ),
            ("hint-plan-small-page-jit-off".to_owned(), json!(false)),
        ]);
        let options = query_planner_options_from_config(&HashMap::new(), &toggles, 17).unwrap();
        assert!(options.predicate_options.disable_is_null_fix);
        assert_eq!(options.hint_plan_av_fix, Some(false));
        assert_eq!(options.like_count_optimization, Some(false));
        assert_eq!(options.fix_is_null_index_cost, Some(false));
        assert_eq!(options.cap_symbol_map_by_page_limit, Some(false));
        assert_eq!(options.ensure_empty_symbol_values_for_topics, Some(true));
        assert_eq!(options.optional_number_order_scan_hints, Some(true));
        assert_eq!(options.small_page_jit_off, Some(false));
        assert_eq!(options.pg_hints_enabled, Some(true));
        assert_eq!(options.row_hints_enabled, Some(true));
    }
}
