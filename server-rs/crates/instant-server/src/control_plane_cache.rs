//! Process-local control-plane snapshots invalidated by this machine's WAL
//! replication stream.
//!
//! Upstream keeps rules, attributes and app rows in model caches that its WAL
//! listener evicts, and resolves a session's auth once at init. Reading them
//! from PostgreSQL on every operation cost several statements and pool
//! checkouts per client operation. Every snapshot records the invalidation
//! generation observed before its load; evictions advance that generation
//! first, so a load that raced a committed change is never served afterwards.
//! A short maximum age bounds staleness if replication itself stalls.
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use instant_instaql::{AttributeCatalog, SYSTEM_CATALOG_APP_ID, system_attr_id};
use instant_permissions::RuleSet;
use instant_wal::{WalCacheEviction, WalEntry, WalRecord};
use serde_json::Value;
use uuid::Uuid;

use crate::{AppStatusSnapshot, ReceiveHandlerTimeouts};

/// Longest a snapshot is served when its eviction may have been lost.
const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(5);
/// Session auth is also evicted through the user and token rows it read, so
/// this bound only matters when an eviction was lost; a longer window removes
/// the residual periodic reload without weakening row-level invalidation.
const AUTH_MAX_AGE: Duration = Duration::from_secs(120);
const MAX_SNAPSHOTS: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ControlScope {
    /// One app's `rules` row.
    Rules,
    /// One app's attributes plus the shared system catalog.
    Attributes,
    /// One app's `apps` row: status and external source.
    App,
    /// Entities of the instant config app: flags, timeouts and limits.
    Config,
    /// Sessions' resolved users, projected through the app's attributes.
    /// Writes to their own rows are evicted individually.
    Users,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Generation {
    scope: u64,
    app: u64,
}

#[derive(Default)]
struct Generations {
    scopes: HashMap<ControlScope, u64>,
    apps: HashMap<(ControlScope, Uuid), u64>,
}

impl Generations {
    fn current(&self, scope: ControlScope, app: Uuid) -> Generation {
        Generation {
            scope: self.scopes.get(&scope).copied().unwrap_or_default(),
            app: self.apps.get(&(scope, app)).copied().unwrap_or_default(),
        }
    }

    fn advance(&mut self, scope: ControlScope, app: Option<Uuid>) {
        let generation = match app {
            Some(app) => self.apps.entry((scope, app)).or_default(),
            None => self.scopes.entry(scope).or_default(),
        };
        *generation = generation.wrapping_add(1);
    }
}

struct Snapshot<V> {
    app: Uuid,
    generation: Generation,
    loaded_at: Instant,
    value: V,
}

struct Snapshots<K, V> {
    scope: ControlScope,
    entries: Mutex<HashMap<K, Snapshot<V>>>,
}

impl<K: Eq + Hash, V: Clone> Snapshots<K, V> {
    fn new(scope: ControlScope) -> Self {
        Self {
            scope,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<K, Snapshot<V>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn get(&self, generations: &Mutex<Generations>, key: &K) -> Option<V> {
        let entries = self.entries();
        let snapshot = entries.get(key)?;
        (snapshot.loaded_at.elapsed() < SNAPSHOT_MAX_AGE
            && lock(generations).current(self.scope, snapshot.app) == snapshot.generation)
            .then(|| snapshot.value.clone())
    }

    fn insert(&self, key: K, app: Uuid, generation: Generation, value: V) {
        let mut entries = self.entries();
        if entries.len() >= MAX_SNAPSHOTS {
            entries.retain(|_, snapshot| snapshot.loaded_at.elapsed() < SNAPSHOT_MAX_AGE);
            if entries.len() >= MAX_SNAPSHOTS {
                entries.clear();
            }
        }
        entries.insert(
            key,
            Snapshot {
                app,
                generation,
                loaded_at: Instant::now(),
                value,
            },
        );
    }
}

/// Identifies the credential a session's auth was resolved from.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AuthKey {
    RefreshToken(Uuid),
    Email(String),
}

struct AuthSnapshot {
    generation: Generation,
    loaded_at: Instant,
    auth: Value,
    entities: Vec<Uuid>,
}

#[derive(Default)]
struct AuthSnapshots {
    entries: HashMap<(Uuid, AuthKey), AuthSnapshot>,
    /// Snapshots to drop when a WAL record touches one of their entities.
    watchers: HashMap<(Uuid, Uuid), Vec<AuthKey>>,
    /// Advanced per app whenever a built-in user or token attribute changes,
    /// so a load that overlapped such a change is served but not retained.
    user_writes: HashMap<Uuid, u64>,
}

impl AuthSnapshots {
    fn forget(&mut self, app: Uuid, key: &AuthKey) {
        if let Some(snapshot) = self.entries.remove(&(app, key.clone())) {
            for entity in snapshot.entities {
                if let Some(keys) = self.watchers.get_mut(&(app, entity)) {
                    keys.retain(|watched| watched != key);
                    if keys.is_empty() {
                        self.watchers.remove(&(app, entity));
                    }
                }
            }
        }
    }

    fn prune(&mut self) {
        self.entries
            .retain(|_, snapshot| snapshot.loaded_at.elapsed() < AUTH_MAX_AGE);
        if self.entries.len() >= MAX_SNAPSHOTS {
            self.entries.clear();
        }
        self.watchers.clear();
        for ((app, key), snapshot) in &self.entries {
            for entity in &snapshot.entities {
                self.watchers
                    .entry((*app, *entity))
                    .or_default()
                    .push(key.clone());
            }
        }
    }
}

/// Built-in attributes whose writes can change any session's resolved user.
static USER_ATTRIBUTES: LazyLock<Vec<Uuid>> = LazyLock::new(|| {
    [
        ("$users", "id"),
        ("$users", "email"),
        ("$users", "type"),
        ("$users", "imageURL"),
        ("$users", "linkedPrimaryUser"),
        ("$userRefreshTokens", "id"),
        ("$userRefreshTokens", "hashedToken"),
        ("$userRefreshTokens", "$user"),
    ]
    .into_iter()
    .filter_map(|(etype, label)| system_attr_id(etype, label).ok())
    .collect()
});

/// Read-through snapshots of control-plane rows shared by every request on this
/// machine. Construct once and register as a replication invalidation consumer
/// ahead of the runtime delivery queue.
pub struct ControlPlaneCache {
    config_app_id: Uuid,
    generations: Mutex<Generations>,
    rules: Snapshots<Uuid, Arc<RuleSet>>,
    catalogs: Snapshots<Uuid, Arc<AttributeCatalog>>,
    external_sources: Snapshots<Uuid, bool>,
    statuses: Snapshots<Uuid, AppStatusSnapshot>,
    receive_timeouts: Snapshots<Option<Uuid>, ReceiveHandlerTimeouts>,
    cancel_wait: Snapshots<(), Duration>,
    rate_limits: Snapshots<Vec<Uuid>, bool>,
    pool_pressure: Snapshots<(), (bool, i64)>,
    auth: Mutex<AuthSnapshots>,
}

impl ControlPlaneCache {
    #[must_use]
    pub fn new(config_app_id: Uuid) -> Self {
        Self {
            config_app_id,
            generations: Mutex::new(Generations::default()),
            rules: Snapshots::new(ControlScope::Rules),
            catalogs: Snapshots::new(ControlScope::Attributes),
            external_sources: Snapshots::new(ControlScope::App),
            statuses: Snapshots::new(ControlScope::App),
            receive_timeouts: Snapshots::new(ControlScope::Config),
            cancel_wait: Snapshots::new(ControlScope::Config),
            rate_limits: Snapshots::new(ControlScope::Config),
            pool_pressure: Snapshots::new(ControlScope::Config),
            auth: Mutex::new(AuthSnapshots::default()),
        }
    }

    pub(crate) fn matches_config_app(&self, config_app_id: Uuid) -> bool {
        self.config_app_id == config_app_id
    }

    async fn load_through<K, V, E, F, Fut>(
        &self,
        snapshots: &Snapshots<K, V>,
        key: K,
        app: Uuid,
        load: F,
    ) -> Result<V, E>
    where
        K: Eq + Hash,
        V: Clone,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        if let Some(value) = snapshots.get(&self.generations, &key) {
            return Ok(value);
        }
        // Capture before reading: an eviction during the load invalidates it.
        let generation = lock(&self.generations).current(snapshots.scope, app);
        let value = load().await?;
        snapshots.insert(key, app, generation, value.clone());
        Ok(value)
    }

    pub(crate) fn cached_rules(&self, app_id: Uuid) -> Option<Arc<RuleSet>> {
        self.rules.get(&self.generations, &app_id)
    }

    pub(crate) async fn rules<E, F, Fut>(&self, app_id: Uuid, load: F) -> Result<Arc<RuleSet>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<RuleSet>, E>>,
    {
        self.load_through(&self.rules, app_id, app_id, load).await
    }

    /// Captures the rules generation before a read whose parse happens after
    /// the database lease is released. Retain it only if no eviction raced.
    pub(crate) fn begin_rules_read(&self, app_id: Uuid) -> ControlRead {
        ControlRead(lock(&self.generations).current(ControlScope::Rules, app_id))
    }

    pub(crate) fn finish_rules_read(&self, app_id: Uuid, read: ControlRead, rules: Arc<RuleSet>) {
        self.rules.insert(app_id, app_id, read.0, rules);
    }

    pub(crate) async fn catalog<E, F, Fut>(
        &self,
        app_id: Uuid,
        load: F,
    ) -> Result<Arc<AttributeCatalog>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<AttributeCatalog>, E>>,
    {
        self.load_through(&self.catalogs, app_id, app_id, load)
            .await
    }

    pub(crate) fn cached_external_source(&self, app_id: Uuid) -> Option<bool> {
        self.external_sources.get(&self.generations, &app_id)
    }

    /// Records whether `app_id` routes to an external database. Callers pass
    /// the generation captured before their routing read.
    pub(crate) fn begin_external_source_read(&self, app_id: Uuid) -> ControlRead {
        ControlRead(lock(&self.generations).current(ControlScope::App, app_id))
    }

    pub(crate) fn finish_external_source_read(
        &self,
        app_id: Uuid,
        read: ControlRead,
        external: bool,
    ) {
        self.external_sources
            .insert(app_id, app_id, read.0, external);
    }

    pub(crate) async fn app_status<E, F, Fut>(
        &self,
        app_id: Uuid,
        load: F,
    ) -> Result<AppStatusSnapshot, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<AppStatusSnapshot, E>>,
    {
        self.load_through(&self.statuses, app_id, app_id, load)
            .await
    }

    pub(crate) async fn receive_timeouts<E, F, Fut>(
        &self,
        app_id: Option<Uuid>,
        load: F,
    ) -> Result<ReceiveHandlerTimeouts, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ReceiveHandlerTimeouts, E>>,
    {
        self.load_through(&self.receive_timeouts, app_id, self.config_app_id, load)
            .await
    }

    /// The config app's `statement-cancel-wait-ms` flag, which bounds how long a
    /// cancelled statement may keep running after its client disconnects.
    ///
    /// Upstream reads this through its config-app flag subscription, and the
    /// reproduction profile showed the per-cancellation read costing more planner
    /// time than any other statement. The value is process-wide, so one snapshot
    /// serves every session; any config-app commit evicts it.
    pub(crate) async fn cancel_wait<E, F, Fut>(&self, load: F) -> Result<Duration, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Duration, E>>,
    {
        self.load_through(&self.cancel_wait, (), self.config_app_id, load)
            .await
    }

    pub(crate) async fn rate_limited<E, F, Fut>(&self, app_ids: &[Uuid], load: F) -> Result<bool, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<bool, E>>,
    {
        let mut key = app_ids.to_vec();
        key.sort_unstable();
        key.dedup();
        self.load_through(&self.rate_limits, key, self.config_app_id, load)
            .await
    }

    /// Transaction pool-pressure rollout controls: enabled and buffer size.
    pub(crate) async fn pool_pressure_controls<E, F, Fut>(&self, load: F) -> Result<(bool, i64), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(bool, i64), E>>,
    {
        self.load_through(&self.pool_pressure, (), self.config_app_id, load)
            .await
    }

    pub(crate) fn cached_auth(&self, app_id: Uuid, key: &AuthKey) -> Option<Value> {
        let users = lock(&self.generations).current(ControlScope::Users, app_id);
        let auth = lock(&self.auth);
        let snapshot = auth.entries.get(&(app_id, key.clone()))?;
        (snapshot.loaded_at.elapsed() < AUTH_MAX_AGE && snapshot.generation == users)
            .then(|| snapshot.auth.clone())
    }

    pub(crate) fn begin_auth_read(&self, app_id: Uuid) -> AuthRead {
        let generation = lock(&self.generations).current(ControlScope::Users, app_id);
        let user_writes = lock(&self.auth)
            .user_writes
            .get(&app_id)
            .copied()
            .unwrap_or_default();
        AuthRead {
            generation,
            user_writes,
        }
    }

    /// Retains a successfully resolved user. `entities` are the user and token
    /// rows it was read from; a later write to any of them drops the snapshot.
    pub(crate) fn finish_auth_read(
        &self,
        app_id: Uuid,
        key: AuthKey,
        read: AuthRead,
        auth: Value,
        entities: Vec<Uuid>,
    ) {
        let mut state = lock(&self.auth);
        if state.user_writes.get(&app_id).copied().unwrap_or_default() != read.user_writes {
            return;
        }
        if state.entries.len() >= MAX_SNAPSHOTS {
            state.prune();
        }
        state.forget(app_id, &key);
        for entity in &entities {
            state
                .watchers
                .entry((app_id, *entity))
                .or_default()
                .push(key.clone());
        }
        state.entries.insert(
            (app_id, key),
            AuthSnapshot {
                generation: read.generation,
                loaded_at: Instant::now(),
                auth,
                entities,
            },
        );
    }

    fn advance(&self, scope: ControlScope, app: Option<Uuid>) {
        lock(&self.generations).advance(scope, app);
    }

    /// Applies the row-level eviction the JVM listener performs for its models.
    pub fn evict(&self, eviction: &WalCacheEviction) {
        match eviction {
            WalCacheEviction::Rules { app_id } => self.advance(ControlScope::Rules, *app_id),
            WalCacheEviction::Attribute { app_id, .. } => {
                let app = app_id.filter(|app| *app != SYSTEM_CATALOG_APP_ID);
                self.advance(ControlScope::Attributes, app);
                // Custom `$users` fields are projected through attribute labels.
                self.advance(ControlScope::Users, app);
                if app_id.is_none_or(|app| app == self.config_app_id) {
                    self.advance(ControlScope::Config, None);
                }
            }
            WalCacheEviction::App { app_id, .. } => {
                self.advance(ControlScope::App, *app_id);
                if app_id.is_none_or(|app| app == self.config_app_id) {
                    self.advance(ControlScope::Config, None);
                }
            }
            WalCacheEviction::InstantUser { .. }
            | WalCacheEviction::TransactionMaxSeen { .. }
            | WalCacheEviction::StreamMachine { .. }
            | WalCacheEviction::Webhook { .. } => {}
        }
    }

    /// Applies one committed application transaction.
    pub fn observe_record(&self, record: &WalRecord) {
        let app_id = record.app_id;
        if app_id == self.config_app_id {
            self.advance(ControlScope::Config, None);
        }
        if !record.attr_changes.is_empty() || !record.ident_changes.is_empty() {
            self.advance(ControlScope::Attributes, Some(app_id));
            self.advance(ControlScope::Users, Some(app_id));
        }
        let mut state = lock(&self.auth);
        let mut user_write = false;
        let mut touched = Vec::new();
        for change in &record.triple_changes {
            if triple_uuid(change, "attr_id").is_some_and(|attr| USER_ATTRIBUTES.contains(&attr)) {
                user_write = true;
            }
            if let Some(entity) = triple_uuid(change, "entity_id")
                && let Some(keys) = state.watchers.remove(&(app_id, entity))
            {
                touched.extend(keys);
            }
        }
        if user_write {
            let writes = state.user_writes.entry(app_id).or_default();
            *writes = writes.wrapping_add(1);
        }
        for key in touched {
            state.forget(app_id, &key);
        }
    }

    /// Drops every snapshot, as after an unknown replication gap.
    pub fn invalidate_all(&self) {
        let mut generations = lock(&self.generations);
        for scope in [
            ControlScope::Rules,
            ControlScope::Attributes,
            ControlScope::App,
            ControlScope::Config,
            ControlScope::Users,
        ] {
            generations.advance(scope, None);
        }
    }
}

/// Generation captured before an uncached routing read.
#[derive(Clone, Copy)]
pub(crate) struct ControlRead(Generation);

/// State captured before an uncached user resolution.
pub(crate) struct AuthRead {
    generation: Generation,
    user_writes: u64,
}

fn triple_uuid(change: &WalEntry, column: &str) -> Option<Uuid> {
    change
        .column(column)
        .or_else(|| change.identity_column(column))
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl crate::RealtimeInvalidationConsumer for ControlPlaneCache {
    fn requires_local_delivery(&self) -> bool {
        false
    }

    async fn stage(
        &self,
        outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), String> {
        if let Some(record) = &outcome.record {
            self.observe_record(record);
        }
        Ok(())
    }

    async fn deliver(
        &self,
        _outcome: &instant_realtime::ReplicationPipelineOutcome,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn evict_cache(&self, eviction: &WalCacheEviction) -> Result<(), String> {
        self.evict(eviction);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use instant_wal::{Isn, Lsn, WalAction, WalColumn};
    use serde_json::json;

    use super::*;

    const CONFIG_APP: Uuid = Uuid::from_u128(0xc0f1);

    fn record(app_id: Uuid, triple_changes: Vec<WalEntry>) -> WalRecord {
        WalRecord {
            app_id,
            tx_id: 1,
            isn: Isn::new(1, Lsn::from_u64(1)),
            previous_isn: None,
            tx_created_at: None,
            tx_bytes: 0,
            next_lsn: Lsn::from_u64(1),
            attr_changes: Vec::new(),
            ident_changes: Vec::new(),
            triple_changes,
            messages: Vec::new(),
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }

    fn triple(entity: Uuid, attr: Uuid) -> WalEntry {
        WalEntry {
            action: WalAction::Update,
            tx_bytes: 0,
            table: Some("triples".to_owned()),
            columns: vec![
                WalColumn {
                    name: "entity_id".to_owned(),
                    value: json!(entity.to_string()),
                },
                WalColumn {
                    name: "attr_id".to_owned(),
                    value: json!(attr.to_string()),
                },
            ],
            identity: Vec::new(),
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn rules(code: Value) -> Arc<RuleSet> {
        Arc::new(RuleSet::new(code))
    }

    async fn counted_rules(
        cache: &ControlPlaneCache,
        app: Uuid,
        loads: &AtomicUsize,
        code: Value,
    ) -> Arc<RuleSet> {
        cache
            .rules(app, || async {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(rules(code))
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rules_are_reused_until_their_app_is_evicted() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let (app, other) = (Uuid::new_v4(), Uuid::new_v4());
        let loads = AtomicUsize::new(0);
        counted_rules(&cache, app, &loads, json!({"v": 1})).await;
        counted_rules(&cache, app, &loads, json!({"v": 2})).await;
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        cache.evict(&WalCacheEviction::Rules {
            app_id: Some(other),
        });
        assert_eq!(cache.cached_rules(app).unwrap().code(), &json!({"v": 1}));
        cache.evict(&WalCacheEviction::Rules { app_id: Some(app) });
        assert!(cache.cached_rules(app).is_none());
        let fresh = counted_rules(&cache, app, &loads, json!({"v": 3})).await;
        assert_eq!(fresh.code(), &json!({"v": 3}));
        cache.evict(&WalCacheEviction::Rules { app_id: None });
        assert!(cache.cached_rules(app).is_none());
    }

    #[tokio::test]
    async fn a_load_that_overlaps_its_eviction_is_never_served_later() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let loaded = cache
            .rules(app, || async {
                // The rules row changes and its WAL eviction arrives while
                // this read still returns the previous version.
                cache.evict(&WalCacheEviction::Rules { app_id: Some(app) });
                Ok::<_, ()>(rules(json!({"stale": true})))
            })
            .await
            .unwrap();
        assert_eq!(loaded.code(), &json!({"stale": true}));
        assert!(cache.cached_rules(app).is_none());
    }

    #[test]
    fn rules_parsed_after_an_eviction_are_not_served() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let read = cache.begin_rules_read(app);
        // The row changes while the caller parses the code it just read.
        cache.evict(&WalCacheEviction::Rules { app_id: Some(app) });
        cache.finish_rules_read(app, read, rules(json!({"stale": true})));
        assert!(cache.cached_rules(app).is_none());

        let read = cache.begin_rules_read(app);
        cache.finish_rules_read(app, read, rules(json!({"v": 1})));
        assert_eq!(cache.cached_rules(app).unwrap().code(), &json!({"v": 1}));
    }

    #[tokio::test]
    async fn system_catalog_and_config_changes_reach_every_dependent_snapshot() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let load_catalog = || async { Ok::<_, ()>(Arc::new(AttributeCatalog::default())) };
        let loads = AtomicUsize::new(0);
        let timeouts = || async {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(ReceiveHandlerTimeouts {
                timeout: Duration::from_secs(5),
                cancel_wait: Duration::from_secs(1),
            })
        };
        cache.catalog(app, load_catalog).await.unwrap();
        cache.receive_timeouts(Some(app), timeouts).await.unwrap();
        cache.receive_timeouts(Some(app), timeouts).await.unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert!(cache.catalogs.get(&cache.generations, &app).is_some());

        cache.evict(&WalCacheEviction::Attribute {
            app_id: Some(SYSTEM_CATALOG_APP_ID),
            attr_id: None,
        });
        assert!(cache.catalogs.get(&cache.generations, &app).is_none());

        // Any committed config-app transaction can change a flag value.
        cache.observe_record(&record(CONFIG_APP, Vec::new()));
        cache.receive_timeouts(Some(app), timeouts).await.unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        cache.observe_record(&record(app, Vec::new()));
        cache.receive_timeouts(Some(app), timeouts).await.unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn the_cancel_wait_flag_is_shared_until_a_config_write() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let loads = AtomicUsize::new(0);
        let load = || async {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(Duration::from_millis(750))
        };
        assert_eq!(cache.cancel_wait(load).await.unwrap().as_millis(), 750);
        assert_eq!(cache.cancel_wait(load).await.unwrap().as_millis(), 750);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        // Application writes never touch the config app's flag entity.
        cache.observe_record(&record(app, Vec::new()));
        assert_eq!(cache.cancel_wait(load).await.unwrap().as_millis(), 750);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        // Any committed config-app transaction can change a flag value.
        cache.observe_record(&record(CONFIG_APP, Vec::new()));
        assert_eq!(cache.cancel_wait(load).await.unwrap().as_millis(), 750);
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        // So can a catalog change on the config app.
        cache.evict(&WalCacheEviction::Attribute {
            app_id: Some(CONFIG_APP),
            attr_id: None,
        });
        assert_eq!(cache.cancel_wait(load).await.unwrap().as_millis(), 750);
        assert_eq!(loads.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn app_row_changes_evict_status_and_routing() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let read = cache.begin_external_source_read(app);
        cache.finish_external_source_read(app, read, false);
        assert_eq!(cache.cached_external_source(app), Some(false));
        cache.evict(&WalCacheEviction::App {
            app_id: Some(app),
            changed_status: None,
        });
        assert_eq!(cache.cached_external_source(app), None);
    }

    #[test]
    fn session_auth_is_dropped_when_its_user_or_token_rows_change() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let (user, token_entity, stranger) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let custom_attr = Uuid::new_v4();
        let key = AuthKey::RefreshToken(Uuid::new_v4());
        let other_key = AuthKey::Email("other@example.com".to_owned());
        let remember = |key: &AuthKey, entities: Vec<Uuid>| {
            let read = cache.begin_auth_read(app);
            cache.finish_auth_read(app, key.clone(), read, json!({"id": user}), entities);
        };
        remember(&key, vec![user, token_entity]);
        remember(&other_key, vec![stranger]);
        assert!(cache.cached_auth(app, &key).is_some());

        // Unrelated entities and other apps leave the snapshot in place.
        cache.observe_record(&record(app, vec![triple(Uuid::new_v4(), custom_attr)]));
        cache.observe_record(&record(Uuid::new_v4(), vec![triple(user, custom_attr)]));
        assert!(cache.cached_auth(app, &key).is_some());

        // A custom `$users` field on this user evicts only this session's user.
        cache.observe_record(&record(app, vec![triple(user, custom_attr)]));
        assert!(cache.cached_auth(app, &key).is_none());
        assert!(cache.cached_auth(app, &other_key).is_some());

        remember(&key, vec![user, token_entity]);
        let revoked = system_attr_id("$userRefreshTokens", "hashedToken").unwrap();
        cache.observe_record(&record(app, vec![triple(token_entity, revoked)]));
        assert!(cache.cached_auth(app, &key).is_none());

        // Projected labels come from the catalog, so attribute changes evict.
        remember(&key, vec![user, token_entity]);
        cache.evict(&WalCacheEviction::Attribute {
            app_id: Some(app),
            attr_id: None,
        });
        assert!(cache.cached_auth(app, &key).is_none());
    }

    #[test]
    fn a_user_resolution_that_overlaps_a_user_write_is_not_retained() {
        let cache = ControlPlaneCache::new(CONFIG_APP);
        let app = Uuid::new_v4();
        let key = AuthKey::RefreshToken(Uuid::new_v4());
        let read = cache.begin_auth_read(app);
        // Another token is created while this resolution is in flight.
        let created = system_attr_id("$userRefreshTokens", "$user").unwrap();
        cache.observe_record(&record(app, vec![triple(Uuid::new_v4(), created)]));
        cache.finish_auth_read(app, key.clone(), read, json!({}), vec![Uuid::new_v4()]);
        assert!(cache.cached_auth(app, &key).is_none());

        let read = cache.begin_auth_read(app);
        cache.finish_auth_read(app, key.clone(), read, json!({}), vec![Uuid::new_v4()]);
        assert!(cache.cached_auth(app, &key).is_some());
    }
}
