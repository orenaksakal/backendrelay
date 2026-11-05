//! Per-app cache of user-independent root frames.
//!
//! A frame is keyed by every input that can change the SQL, the file transform
//! or the permission entity loads, never by a session, actor or rule decision.
//! A data WAL retires only the frames whose recorded topics it intersects. A
//! load already in flight is detached from new joiners and publishes only when
//! the app's frame data/security epoch proves the WAL cannot affect it. Security
//! changes still clear the whole app.
//!
//! Frames are retired before the session registry is taken (a WAL's frames must
//! be gone before any refresh it selects can run) and are fenced by the frame
//! epoch plus each cache's own state lock. Scans clone the app's cache handle
//! and release the registry mutex first, so one app's retirement never blocks
//! another app's lookups.
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use instant_datalog::MatchCteOptions;
use instant_http::ApiError;
use instant_instaql::{AttributeCatalog, CompiledQuery};
use instant_permissions::RuleSet;
use instant_realtime::{
    AppInvalidationEpochs, PublishPermit, PublishToken, QueryCache, QueryCacheBudget,
    QueryCacheCleanup, QueryCacheOptions, QueryCachePublisher, ResultCacheBudget,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::PostgresQuerySetting;
pub(crate) use crate::admin_data::root_frames::RootFrame;

tokio::task_local! {
    // Coarse-triggered refreshes carry their selection topics only as a coarse
    // hint, while frames retire on precise topics. Disable shared reuse for the
    // duration of such a batch so it can neither read nor populate a frame the
    // triggering WAL may not have retired.
    static DISABLE_SHARED_ROOT_FRAME_REUSE: bool;
}

/// Runs `future` without reading or populating the shared root-frame cache.
pub(crate) async fn without_shared_root_frame_reuse<F: Future>(future: F) -> F::Output {
    DISABLE_SHARED_ROOT_FRAME_REUSE.scope(true, future).await
}

/// Whether the current task must fall back to the per-request frame path.
#[must_use]
pub(crate) fn shared_root_frame_reuse_disabled() -> bool {
    DISABLE_SHARED_ROOT_FRAME_REUSE
        .try_with(|disabled| *disabled)
        .unwrap_or(false)
}

/// Hit, miss and load counters plus the current ready entry count for one app.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RootFrameCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub loads: u64,
    pub entries: usize,
    /// Approximate ready bytes these frames retain, the same estimate charged to
    /// the process-wide result-cache budget.
    pub ready_bytes: u64,
    pub retire_scanned: u64,
    pub retire_retired: u64,
    pub retire_nanos: u64,
    pub expired_pruned: u64,
}

/// Every input that determines one root frame's content.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RootFrameKey {
    app_id: Uuid,
    catalog_digest: [u8; 32],
    external: bool,
    rules_digest: [u8; 32],
    /// Canonically serialized single-root compiled query after permission
    /// pushdown, so auth-dependent roots are automatically separate keys.
    compiled_root: Arc<[u8]>,
    planner_options: Arc<[u8]>,
    postgres_settings: Arc<[u8]>,
    permission_attribute_projection: bool,
}

impl RootFrameKey {
    pub(crate) fn new(
        app_id: Uuid,
        catalog_digest: [u8; 32],
        external: bool,
        rules_digest: [u8; 32],
        compiled: &CompiledQuery,
        options: &MatchCteOptions,
        settings: &[PostgresQuerySetting],
        permission_attribute_projection: bool,
    ) -> Self {
        Self {
            app_id,
            catalog_digest,
            external,
            rules_digest,
            compiled_root: canonical_bytes(compiled),
            planner_options: canonical_bytes(options),
            postgres_settings: canonical_bytes(settings),
            permission_attribute_projection,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(app_id: Uuid, tag: u64) -> Self {
        Self {
            app_id,
            catalog_digest: [0; 32],
            external: false,
            rules_digest: [0; 32],
            compiled_root: Arc::from(tag.to_le_bytes()),
            planner_options: Arc::from([]),
            postgres_settings: Arc::from([]),
            permission_attribute_projection: true,
        }
    }
}

/// Catalog identity including the client-visible attribute encoding. The
/// external flag separates native catalogs from BYOP/client catalogs.
pub(crate) fn catalog_digest(catalog: &AttributeCatalog) -> [u8; 32] {
    Sha256::digest(canonical_bytes(&crate::attribute_wire::FrameworkCatalog(
        catalog,
    )))
    .into()
}

/// Identity of the rule code. Any rule edit changes every frame's key, in
/// addition to the security clear the WAL path performs.
pub(crate) fn rules_digest(rules: &RuleSet) -> [u8; 32] {
    Sha256::digest(canonical_bytes(rules.code())).into()
}

fn canonical_bytes<T: Serialize + ?Sized>(value: &T) -> Arc<[u8]> {
    let mut value =
        serde_json::to_value(value).expect("root frame key parts are JSON serializable");
    sort_json_keys(&mut value);
    Arc::from(serde_json::to_vec(&value).expect("sorted JSON serializes"))
}

fn sort_json_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let sorted = std::mem::take(map).into_iter().collect::<BTreeMap<_, _>>();
            map.extend(sorted);
            for value in map.values_mut() {
                sort_json_keys(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                sort_json_keys(value);
            }
        }
        _ => {}
    }
}

/// Cheap structural weight: no serialization or deep copy while holding the
/// cache lock. Rows and entity objects dominate; exact bytes are not required.
pub(crate) fn root_frame_weight(frame: &RootFrame, size_multiple: u64) -> u64 {
    fn node_weight(node: &instant_instaql::ExecutedNode) -> u64 {
        let rows = node.selector.join_rows.len() as u64
            + node
                .selector
                .symbol_values
                .values()
                .map(|values| values.len() as u64)
                .sum::<u64>();
        let mut weight = 64_u64.saturating_add(rows.saturating_mul(64));
        for entity in &node.entities {
            weight = weight
                .saturating_add(64)
                .saturating_add((entity.fetch.join_rows.len() as u64).saturating_mul(64));
            weight = weight.saturating_add(entity.children.iter().map(node_weight).sum::<u64>());
        }
        weight
    }
    let nodes = frame.nodes.iter().map(node_weight).sum::<u64>();
    let entities = frame
        .entity_data
        .values()
        .map(|value| {
            let fields = value.as_object().map_or(1, serde_json::Map::len) as u64;
            64_u64.saturating_add(fields.saturating_mul(32))
        })
        .sum::<u64>();
    nodes.saturating_add(entities).saturating_mul(size_multiple)
}

#[derive(Default)]
struct RootFrameCountersState {
    hits: AtomicU64,
    misses: AtomicU64,
    loads: AtomicU64,
    retire_scanned: AtomicU64,
    retire_retired: AtomicU64,
    retire_nanos: AtomicU64,
    expired_pruned: AtomicU64,
}

/// Conditional publication for one app's frames read from the shared epochs.
struct RootFramePublisher {
    app_id: Uuid,
    epochs: Arc<Mutex<Option<Arc<AppInvalidationEpochs>>>>,
}

impl RootFramePublisher {
    fn epoch(&self) -> Option<Arc<instant_realtime::InvalidationEpoch>> {
        let epochs = self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()?;
        Some(epochs.get_or_create(self.app_id))
    }
}

impl QueryCachePublisher<RootFrame> for RootFramePublisher {
    fn begin(&self) -> PublishToken {
        self.epoch()
            .map_or_else(PublishToken::default, |epoch| epoch.snapshot().token())
    }

    fn authorize<'a>(
        &'a self,
        token: PublishToken,
        frame: &RootFrame,
    ) -> Option<PublishPermit<'a>> {
        let Some(epoch) = self.epoch() else {
            // Standalone registries (unit and integration tests) have no
            // realtime epoch; retain exactly as before.
            return Some(PublishPermit::unconditional());
        };
        epoch
            .authorize(token, |topics| frame.wal_intersects(topics))
            .map(instant_realtime::EpochPermit::into_publish_permit)
    }
}

pub(crate) struct RootFrameCounters {
    state: RootFrameCountersState,
}

impl RootFrameCounters {
    pub(crate) fn hit(&self) {
        self.state.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn miss(&self) {
        self.state.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn load(&self) {
        self.state.loads.fetch_add(1, Ordering::Relaxed);
    }

    /// Folds a removed cache's cumulative scan counters into the app totals.
    fn accumulate(&self, metrics: instant_realtime::QueryCacheMetrics) {
        self.state
            .retire_scanned
            .fetch_add(metrics.retire_scanned, Ordering::Relaxed);
        self.state
            .retire_retired
            .fetch_add(metrics.retire_retired, Ordering::Relaxed);
        self.state
            .retire_nanos
            .fetch_add(metrics.retire_nanos, Ordering::Relaxed);
        self.state
            .expired_pruned
            .fetch_add(metrics.expired_pruned, Ordering::Relaxed);
    }

    fn snapshot(&self, entries: usize) -> RootFrameCacheStats {
        RootFrameCacheStats {
            hits: self.state.hits.load(Ordering::Relaxed),
            misses: self.state.misses.load(Ordering::Relaxed),
            loads: self.state.loads.load(Ordering::Relaxed),
            entries,
            // Filled from the live cache by the registry's stats/totals reads.
            ready_bytes: 0,
            retire_scanned: self.state.retire_scanned.load(Ordering::Relaxed),
            retire_retired: self.state.retire_retired.load(Ordering::Relaxed),
            retire_nanos: self.state.retire_nanos.load(Ordering::Relaxed),
            expired_pruned: self.state.expired_pruned.load(Ordering::Relaxed),
        }
    }
}

/// Process registry of per-app root frame caches. Both the framework query
/// implementation and realtime WAL invalidation hold this handle.
pub struct RootFrameCaches {
    policy: Arc<dyn crate::DatalogCachePolicy>,
    caches: Mutex<HashMap<Uuid, RootFrameCache>>,
    budget: Mutex<Option<Arc<QueryCacheBudget>>>,
    // Shared with the per-app session result caches, so root frames are charged
    // to the same process-wide ready-byte budget. Set during assembly: frames
    // created before it is attached stay outside the budget.
    result_cache_budget: Mutex<Option<Arc<ResultCacheBudget>>>,
    counters: Mutex<HashMap<Uuid, Arc<RootFrameCounters>>>,
    // Set by the realtime service during assembly. Caches may be created before
    // this point, so the publisher reads the slot on every load instead of
    // capturing it when the cache is built.
    epochs: Arc<Mutex<Option<Arc<AppInvalidationEpochs>>>>,
    // Last digested catalog per app. Equality with the incoming catalog is
    // checked before reuse, so this only skips re-encoding identical metadata.
    // The catalog is shared so a cache hit never deep-clones it.
    catalog_digests: Mutex<HashMap<Uuid, (Arc<AttributeCatalog>, [u8; 32])>>,
    // Test-only hook run after the registry guard is released and immediately
    // before a retirement scan, so a test can assert which locks remain held.
    #[cfg(test)]
    retire_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Upper bound on a frame's lifetime. Precise topics come from returned rows, so
/// a change they cannot see (for example a sort-key update that moves an
/// out-of-page entity onto a page) would otherwise keep a frame stale until the
/// policy TTL. Sharing happens within one write's refresh fan-out, which settles
/// well inside this window.
const ROOT_FRAME_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) type RootFrameCache = QueryCache<RootFrameKey, RootFrame, ApiError>;
pub(crate) type RootFrameCacheCleanup = QueryCacheCleanup<RootFrameKey, RootFrame>;

impl RootFrameCaches {
    #[must_use]
    pub fn new(policy: Arc<dyn crate::DatalogCachePolicy>) -> Self {
        Self {
            policy,
            caches: Mutex::new(HashMap::new()),
            budget: Mutex::new(None),
            result_cache_budget: Mutex::new(None),
            counters: Mutex::new(HashMap::new()),
            epochs: Arc::new(Mutex::new(None)),
            catalog_digests: Mutex::new(HashMap::new()),
            #[cfg(test)]
            retire_probe: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_retire_probe(&self, probe: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self
            .retire_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = probe;
    }

    #[cfg(test)]
    fn run_retire_probe(&self) {
        let probe = self
            .retire_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(probe) = probe {
            probe();
        }
    }

    /// Shares the frame data/security epochs with this registry.
    ///
    /// This must be the frame-specific registry, not the session result cache's
    /// epochs. Publication of a completed frame load is conditional on no frame
    /// invalidation since its start having been imprecise, intersecting the
    /// frame's topics, or older than the bounded log.
    pub(crate) fn set_invalidation_epochs(&self, epochs: Arc<AppInvalidationEpochs>) {
        *self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(epochs);
    }

    /// Shares the process-wide ready-byte budget with every frame cache created
    /// afterwards.
    ///
    /// Frames then evict their least recently used entries when the shared
    /// budget overflows, exactly as session result caches do. Call this during
    /// assembly, before any frame is loaded.
    pub(crate) fn set_result_cache_budget(&self, budget: Arc<ResultCacheBudget>) {
        *self
            .result_cache_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(budget);
    }

    /// Catalog identity for one app. Reuses the digest while the catalog is
    /// pointer- or value-equal; the first fresh catalog after a schema change is
    /// rehashed.
    pub(crate) fn catalog_identity(
        &self,
        app_id: Uuid,
        catalog: &Arc<AttributeCatalog>,
    ) -> [u8; 32] {
        let mut digests = self
            .catalog_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((known, digest)) = digests.get(&app_id)
            && (Arc::ptr_eq(known, catalog) || known.as_ref() == catalog.as_ref())
        {
            return *digest;
        }
        let digest = catalog_digest(catalog);
        digests.insert(app_id, (Arc::clone(catalog), digest));
        digest
    }

    fn caches(&self) -> MutexGuard<'_, HashMap<Uuid, RootFrameCache>> {
        self.caches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns the app's cache, creating it from the current live cache policy.
    pub(crate) async fn cache(&self, app_id: Uuid) -> Result<RootFrameCache, ApiError> {
        if let Some(cache) = self.caches().get(&app_id).cloned() {
            return Ok(cache);
        }
        let settings = self.policy.settings().await?;
        let multiple = settings.size_multiple;
        let budget = if settings.process_max_in_flight == 0 {
            None
        } else {
            let mut budget = self
                .budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Some(Arc::clone(budget.get_or_insert_with(|| {
                Arc::new(QueryCacheBudget::new(settings.process_max_in_flight))
            })))
        };
        let result_budget = self
            .result_cache_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let candidate = QueryCache::with_options_and_budget(
            QueryCacheOptions {
                max_weight: settings.max_weight,
                ttl: settings.ttl.min(ROOT_FRAME_MAX_AGE),
                max_in_flight: settings.max_in_flight,
            },
            move |frame: &RootFrame| root_frame_weight(frame, multiple),
            budget,
        )
        .with_publisher(Arc::new(RootFramePublisher {
            app_id,
            epochs: Arc::clone(&self.epochs),
        }));
        let candidate = match &result_budget {
            Some(result_budget) => candidate.with_result_cache_budget(result_budget),
            None => candidate,
        };
        Ok(self.caches().entry(app_id).or_insert(candidate).clone())
    }

    pub(crate) fn counters(&self, app_id: Uuid) -> Arc<RootFrameCounters> {
        Arc::clone(
            self.counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(app_id)
                .or_insert_with(|| {
                    Arc::new(RootFrameCounters {
                        state: RootFrameCountersState::default(),
                    })
                }),
        )
    }

    /// Drops every ready frame without cancelling in-flight loads.
    ///
    /// Conditional publication rejects a load whose snapshot predates the
    /// invalidation; existing waiters still receive their value. The registry
    /// guard is released before the scan so other apps are not blocked.
    pub(crate) fn retire_deferred(&self, app_id: Uuid) -> Option<RootFrameCacheCleanup> {
        let cache = self.caches().get(&app_id).cloned()?;
        #[cfg(test)]
        self.run_retire_probe();
        Some(cache.retire_matching(|_, _| true))
    }

    /// Retires frames whose topics intersect this WAL without cancelling
    /// in-flight loads.
    ///
    /// Unrelated frames stay reusable because a topic miss means no row or entity
    /// the frame carries can have changed. A pending load is published only if
    /// the app epoch proves this WAL cannot affect it. The registry guard is
    /// released before the scan so other apps are not blocked.
    pub(crate) fn retire_wal_deferred(
        &self,
        app_id: Uuid,
        topics: &[instant_wal::Topic],
    ) -> Option<RootFrameCacheCleanup> {
        let cache = self.caches().get(&app_id).cloned()?;
        #[cfg(test)]
        self.run_retire_probe();
        let wal = crate::admin_data::root_frames::WalTopics::new(topics);
        Some(cache.retire_matching(|_, frame| frame.wal_intersects_with(&wal)))
    }

    /// Cancels in-flight loads and drops every entry. The registry guard is
    /// released before the scan so other apps are not blocked.
    pub(crate) fn clear_deferred(&self, app_id: Uuid) -> Option<RootFrameCacheCleanup> {
        let cache = self.caches().get(&app_id).cloned()?;
        #[cfg(test)]
        self.run_retire_probe();
        Some(cache.clear_deferred())
    }

    /// Removes the app's cache and cached catalog digest entirely.
    ///
    /// Cumulative hit/miss/load and retirement counters are retained so an
    /// operator can observe a run after its last session disconnected. No keys,
    /// values or payloads survive.
    pub(crate) fn remove(&self, app_id: Uuid) -> Option<RootFrameCacheCleanup> {
        self.catalog_digests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&app_id);
        let cache = self.caches().remove(&app_id)?;
        self.counters(app_id).accumulate(cache.metrics_snapshot());
        Some(cache.clear_deferred())
    }

    /// Public whole-app retirement used by integration tests and operators that
    /// want to simulate the WAL data-invalidation path.
    pub fn retire(&self, app_id: Uuid) {
        drop(self.retire_deferred(app_id));
    }

    /// Public topic-precise retirement used by integration tests and operators
    /// that want to simulate a data WAL with routing topics.
    pub fn retire_wal(&self, app_id: Uuid, topics: &[instant_wal::Topic]) {
        drop(self.retire_wal_deferred(app_id, topics));
    }

    /// Public whole-app security clear used by integration tests and operators.
    pub fn clear(&self, app_id: Uuid) {
        drop(self.clear_deferred(app_id));
    }

    #[must_use]
    pub fn stats(&self, app_id: Uuid) -> RootFrameCacheStats {
        let metrics = self
            .caches()
            .get(&app_id)
            .map_or_else(Default::default, |cache| cache.metrics_snapshot());
        let mut stats = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&app_id)
            .map_or_else(
                || RootFrameCacheStats {
                    entries: metrics.entries,
                    ..RootFrameCacheStats::default()
                },
                |counters| counters.snapshot(metrics.entries),
            );
        stats.ready_bytes = metrics.ready_weight;
        stats.retire_scanned = stats.retire_scanned.saturating_add(metrics.retire_scanned);
        stats.retire_retired = stats.retire_retired.saturating_add(metrics.retire_retired);
        stats.retire_nanos = stats.retire_nanos.saturating_add(metrics.retire_nanos);
        stats.expired_pruned = stats.expired_pruned.saturating_add(metrics.expired_pruned);
        stats
    }

    /// Process aggregate over every app's frame cache. Read-only.
    #[must_use]
    pub fn totals(&self) -> RootFrameCacheStats {
        let mut totals = RootFrameCacheStats::default();
        for cache in self.caches().values() {
            let metrics = cache.metrics_snapshot();
            totals.entries = totals.entries.saturating_add(metrics.entries);
            totals.ready_bytes = totals.ready_bytes.saturating_add(metrics.ready_weight);
            totals.retire_scanned = totals.retire_scanned.saturating_add(metrics.retire_scanned);
            totals.retire_retired = totals.retire_retired.saturating_add(metrics.retire_retired);
            totals.retire_nanos = totals.retire_nanos.saturating_add(metrics.retire_nanos);
            totals.expired_pruned = totals.expired_pruned.saturating_add(metrics.expired_pruned);
        }
        for counters in self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            let snapshot = counters.snapshot(0);
            totals.hits = totals.hits.saturating_add(snapshot.hits);
            totals.misses = totals.misses.saturating_add(snapshot.misses);
            totals.loads = totals.loads.saturating_add(snapshot.loads);
            totals.retire_scanned = totals
                .retire_scanned
                .saturating_add(snapshot.retire_scanned);
            totals.retire_retired = totals
                .retire_retired
                .saturating_add(snapshot.retire_retired);
            totals.retire_nanos = totals.retire_nanos.saturating_add(snapshot.retire_nanos);
            totals.expired_pruned = totals
                .expired_pruned
                .saturating_add(snapshot.expired_pruned);
        }
        totals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_instaql::{Attribute, AttributeValueType, Cardinality, compile_query};
    use serde_json::json;
    use std::time::Duration;

    fn catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![Attribute {
                id: Uuid::from_u128(1),
                wire: None,
                etype: "posts".to_owned(),
                label: "id".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: true,
                setting_unique: false,
                indexed: true,
                required: false,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            }],
        }
    }

    fn compiled() -> CompiledQuery {
        compile_query(&json!({"posts": {}}), &catalog(), false).unwrap()
    }

    fn key(
        catalog_digest: [u8; 32],
        rules_digest: [u8; 32],
        options: &MatchCteOptions,
        settings: &[PostgresQuerySetting],
        projection: bool,
    ) -> RootFrameKey {
        RootFrameKey::new(
            Uuid::from_u128(7),
            catalog_digest,
            false,
            rules_digest,
            &compiled(),
            options,
            settings,
            projection,
        )
    }

    #[test]
    fn every_frame_input_changes_the_key() {
        let base = key([1; 32], [2; 32], &MatchCteOptions::default(), &[], true);
        assert_ne!(
            base,
            key([9; 32], [2; 32], &MatchCteOptions::default(), &[], true)
        );
        assert_ne!(
            base,
            key([1; 32], [9; 32], &MatchCteOptions::default(), &[], true)
        );
        assert_ne!(
            base,
            key(
                [1; 32],
                [2; 32],
                &MatchCteOptions {
                    page_info: true,
                    ..MatchCteOptions::default()
                },
                &[],
                true
            )
        );
        assert_ne!(
            base,
            key(
                [1; 32],
                [2; 32],
                &MatchCteOptions::default(),
                &[PostgresQuerySetting {
                    setting: "statement_timeout".to_owned(),
                    value: "1000".to_owned(),
                }],
                true
            )
        );
        assert_ne!(
            base,
            key([1; 32], [2; 32], &MatchCteOptions::default(), &[], false)
        );
        assert_ne!(
            base,
            RootFrameKey::new(
                Uuid::from_u128(8),
                [1; 32],
                false,
                [2; 32],
                &compiled(),
                &MatchCteOptions::default(),
                &[],
                true,
            )
        );
        let mut other_query = compiled();
        other_query.roots[0].missing = true;
        assert_ne!(
            base,
            RootFrameKey::new(
                Uuid::from_u128(7),
                [1; 32],
                false,
                [2; 32],
                &other_query,
                &MatchCteOptions::default(),
                &[],
                true,
            )
        );
        assert_ne!(
            base,
            RootFrameKey::new(
                Uuid::from_u128(7),
                [1; 32],
                true,
                [2; 32],
                &compiled(),
                &MatchCteOptions::default(),
                &[],
                true,
            )
        );
    }

    #[test]
    fn canonical_key_ignores_json_object_insertion_order() {
        let mut left = json!({"a": {"x": 1, "y": 2}});
        let mut right = json!({"a": {"y": 2, "x": 1}});
        sort_json_keys(&mut left);
        sort_json_keys(&mut right);
        assert_eq!(
            serde_json::to_vec(&left).unwrap(),
            serde_json::to_vec(&right).unwrap()
        );
    }

    fn policy() -> Arc<dyn crate::DatalogCachePolicy> {
        Arc::new(crate::StaticDatalogCachePolicy(
            crate::DatalogCacheSettings::default(),
        ))
    }

    /// A frame with a non-zero estimate: one entity record carrying `fields`
    /// JSON fields, so its weight is the default multiple times that record.
    fn weighted_frame(fields: usize) -> RootFrame {
        let mut frame = RootFrame::default();
        frame.entity_data.insert(
            instant_permissions::PermissionEntityKey {
                etype: "posts".to_owned(),
                entity_id: Uuid::from_u128(1),
            },
            Value::Object(
                (0..fields)
                    .map(|index| (format!("field{index}"), Value::from(index)))
                    .collect(),
            ),
        );
        frame
    }

    fn weighted_frame_bytes() -> u64 {
        root_frame_weight(
            &weighted_frame(3),
            crate::DatalogCacheSettings::default().size_multiple,
        )
    }

    #[tokio::test]
    async fn result_cache_budget_accounts_for_root_frame_bytes() {
        let caches = RootFrameCaches::new(policy());
        let budget = Arc::new(ResultCacheBudget::new(1024 * 1024));
        caches.set_result_cache_budget(Arc::clone(&budget));
        let app = Uuid::new_v4();
        let key = RootFrameKey::synthetic(app, 1);
        let bytes = weighted_frame_bytes();
        assert!(bytes > 0);
        let cache = caches.cache(app).await.unwrap();
        cache
            .get_or_load(key.clone(), || async { Ok(weighted_frame(3)) })
            .await
            .unwrap();
        assert_eq!(budget.snapshot().bytes, bytes);
        assert_eq!(caches.stats(app).ready_bytes, bytes);
        assert_eq!(caches.totals().ready_bytes, bytes);

        caches.retire(app);
        assert_eq!(budget.snapshot().bytes, 0);
        assert_eq!(budget.snapshot().evictions, 0);
        assert_eq!(caches.totals().ready_bytes, 0);
    }

    #[tokio::test]
    async fn full_result_cache_budget_evicts_root_frames_instead_of_growing() {
        let caches = RootFrameCaches::new(policy());
        let bytes = weighted_frame_bytes();
        let budget = Arc::new(ResultCacheBudget::new(bytes));
        caches.set_result_cache_budget(Arc::clone(&budget));
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let first_key = RootFrameKey::synthetic(first, 1);
        let second_key = RootFrameKey::synthetic(second, 2);
        caches
            .cache(first)
            .await
            .unwrap()
            .get_or_load(first_key.clone(), || async { Ok(weighted_frame(3)) })
            .await
            .unwrap();
        assert_eq!(budget.snapshot().bytes, bytes);

        // The second app's insert fills an already full shared budget, so the
        // globally oldest frame is evicted rather than retained.
        caches
            .cache(second)
            .await
            .unwrap()
            .get_or_load(second_key.clone(), || async { Ok(weighted_frame(3)) })
            .await
            .unwrap();
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.bytes, bytes);
        assert_eq!(snapshot.evictions, 1);
        assert_eq!(caches.totals().entries, 1);
        assert_eq!(caches.totals().ready_bytes, bytes);
        assert_eq!(caches.stats(first).entries, 0);
        assert_eq!(caches.stats(second).entries, 1);
    }

    #[tokio::test]
    async fn retirement_and_clear_fence_reuse_and_stats_report_loads() {
        let caches = RootFrameCaches::new(policy());
        let app = Uuid::new_v4();
        let cache = caches.cache(app).await.unwrap();
        let counters = caches.counters(app);
        let key = RootFrameKey::synthetic(app, 1);
        assert!(!cache.contains(&key));
        counters.miss();
        cache
            .get_or_load(key.clone(), {
                let counters = Arc::clone(&counters);
                move || async move {
                    counters.load();
                    Ok(RootFrame::default())
                }
            })
            .await
            .unwrap();
        assert!(cache.contains(&key));
        assert_eq!(
            caches.stats(app),
            RootFrameCacheStats {
                hits: 0,
                misses: 1,
                loads: 1,
                entries: 1,
                ..RootFrameCacheStats::default()
            }
        );

        caches.retire(app);
        assert!(!cache.contains(&key));
        assert_eq!(caches.stats(app).entries, 0);
        caches.clear(app);
        assert_eq!(caches.totals().loads, 1);

        let before_remove = caches.stats(app);
        caches.remove(app);
        let after_remove = caches.stats(app);
        assert_eq!(
            after_remove,
            RootFrameCacheStats {
                entries: 0,
                ..before_remove
            },
            "cumulative counters, including retirement work, survive cache cleanup"
        );
        assert_eq!(caches.totals().loads, 1);
        tokio::time::timeout(Duration::from_secs(1), caches.cache(app))
            .await
            .unwrap()
            .unwrap();
    }
}
