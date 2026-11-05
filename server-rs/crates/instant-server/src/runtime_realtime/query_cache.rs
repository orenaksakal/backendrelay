//! Per-app query reuse, admission and invalidation epochs.
//!
//! Session result lock order remains sessions -> epochs/cache registry -> cache
//! state. The caller must hold the session registry while changing a session
//! result reuse generation.
//!
//! Shared root frames have their own epoch registry and are retired before the
//! session registry is taken. A WAL's frames must be gone before a refresh it
//! selected can run; selection marks sessions under the session lock, so
//! retiring frames first preserves that order. Frame correctness is fenced by
//! the frame epoch and the frame cache's own state lock, not the session lock
//! (see `instant_realtime::epoch`).
use std::collections::HashMap;
use std::sync::{Arc, MutexGuard};

use instant_http::ApiError;
use instant_realtime::{
    AppInvalidationEpochs, QueryCache, QueryCacheBudget, QueryCacheCleanup, QueryCacheOptions,
    ResultCacheBudget,
};
use uuid::Uuid;

use super::{
    DatalogCachePolicy, DatalogCacheSettings, RealtimeInner, SessionQueryResult, SharedQueryKey,
    sessions,
};

pub(super) type RuntimeQueryCache = QueryCache<SharedQueryKey, SessionQueryResult, ApiError>;
type RuntimeCacheCleanup = Option<QueryCacheCleanup<SharedQueryKey, SessionQueryResult>>;
type RootFrameCacheCleanup = Option<crate::root_frame_cache::RootFrameCacheCleanup>;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(super) struct QueryInvalidationEpoch {
    pub(super) data: u64,
    pub(super) security: u64,
}

pub(super) fn query_caches(
    inner: &RealtimeInner,
) -> MutexGuard<'_, HashMap<Uuid, RuntimeQueryCache>> {
    inner
        .query_caches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) async fn app_query_cache(
    inner: &RealtimeInner,
    app_id: Uuid,
    policy: &dyn DatalogCachePolicy,
) -> Result<RuntimeQueryCache, ApiError> {
    if let Some(cache) = query_caches(inner).get(&app_id).cloned() {
        return Ok(cache);
    }
    let settings = policy.settings().await?;
    let multiple = settings.size_multiple;
    let budget = query_cache_budget(inner, settings);
    let result_budget = result_cache_budget(inner);
    let candidate = QueryCache::with_options_and_budget(
        QueryCacheOptions {
            max_weight: settings.max_weight,
            ttl: settings.ttl,
            max_in_flight: settings.max_in_flight,
        },
        move |result: &SessionQueryResult| session_query_result_weight(result, multiple),
        budget,
    )
    .with_result_cache_budget(&result_budget);
    Ok(query_caches(inner)
        .entry(app_id)
        .or_insert(candidate)
        .clone())
}

/// The process-wide ready-byte budget shared by every app's session results.
///
/// One cache may retain at most its configured weight; this budget also caps
/// what all of them retain together. The limit is resolved on first use:
/// `INSTANT_RESULT_CACHE_MAX_BYTES`, otherwise a quarter of the container
/// memory limit, capped at 4 GiB.
pub(super) fn result_cache_budget(inner: &RealtimeInner) -> Arc<ResultCacheBudget> {
    let mut budget = inner
        .result_cache_budget
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(budget.get_or_insert_with(|| {
        Arc::new(ResultCacheBudget::new(
            crate::process_memory::result_cache_max_bytes(),
        ))
    }))
}

pub(super) fn query_cache_budget(
    inner: &RealtimeInner,
    settings: DatalogCacheSettings,
) -> Option<Arc<QueryCacheBudget>> {
    if settings.process_max_in_flight == 0 {
        return None;
    }
    let mut budget = inner
        .query_cache_budget
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(Arc::clone(budget.get_or_insert_with(|| {
        Arc::new(QueryCacheBudget::new(settings.process_max_in_flight))
    })))
}

pub(super) fn session_query_result_weight(result: &SessionQueryResult, size_multiple: u64) -> u64 {
    // Count precisely the same encoded bytes without allocating an intermediate
    // Vec for either payload. Saturation preserves the previous overflow policy.
    #[derive(Default)]
    struct ByteCounter(u64);
    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = ByteCounter::default();
    if serde_json::to_writer(&mut counter, &result.result).is_err()
        || serde_json::to_writer(&mut counter, &result.result_meta).is_err()
    {
        return u64::MAX.saturating_mul(size_multiple);
    }
    counter.0.saturating_mul(size_multiple)
}

pub(super) fn query_epochs(inner: &RealtimeInner) -> &AppInvalidationEpochs {
    &inner.query_epochs
}

/// The independent epoch registry that fences shared root frames.
pub(super) fn frame_epochs(inner: &RealtimeInner) -> &AppInvalidationEpochs {
    &inner.frame_epochs
}

pub(super) fn query_epoch(inner: &RealtimeInner, app_id: Uuid) -> QueryInvalidationEpoch {
    let snapshot = query_epochs(inner).get_or_create(app_id).current();
    QueryInvalidationEpoch {
        data: snapshot.data,
        security: snapshot.security,
    }
}

/// Whether a data invalidation after `since`, up to and including `until`,
/// may have changed a query that was executing while it was routed.
///
/// The caller holds the session registry, so no invalidation can enter the
/// window concurrently. `affects` receives each invalidation's WAL topics.
/// Imprecise routing or a window older than the retained log answers true.
pub(super) fn data_invalidated_since(
    inner: &RealtimeInner,
    app_id: Uuid,
    since: u64,
    until: u64,
    affects: impl Fn(&[instant_wal::Topic]) -> bool,
) -> bool {
    query_epochs(inner)
        .get(app_id)
        .is_none_or(|epoch| epoch.data_invalidated_since(since, until, affects))
}

// Caller holds the session registry, serializing session-result invalidation
// with initial query registration. Lock order is sessions -> epochs/cache ->
// cache state. Return only unreachable payload ownership. The caller must
// release its session lock before dropping this handle, while retaining the same
// epoch/state fence. `routing` carries the WAL topics used to select refreshed
// queries, if precise. Root frames are handled separately by
// `invalidate_app_root_frames` before this lock is taken.
pub(super) fn invalidate_app_query_reuse(
    inner: &RealtimeInner,
    app_id: Uuid,
    security: bool,
    routing: Option<Arc<[instant_wal::Topic]>>,
) -> RuntimeCacheCleanup {
    // Record before retiring so a publisher that inserts after this point
    // either observes the new version and withholds an affected result, or is
    // scanned by the retirement below. Only an epoch that already exists is
    // recorded: session readers create it through `query_epoch` under the
    // session lock, so an invalidation that found no epoch committed before any
    // such reader started. Creating one here would retain a bounded WAL log for
    // every app with WAL traffic but no realtime reader.
    if let Some(epoch) = query_epochs(inner).get(app_id) {
        let _recorded = epoch.record(security, routing);
    }
    let cache = query_caches(inner).get(&app_id).cloned();
    cache.map(|cache| {
        if security {
            cache.clear_deferred()
        } else {
            // Permission-filtered topics omit hidden dependencies, so every
            // result loses reuse eligibility. Existing waiters may finish at
            // their original watermark; newer stale generations remain queued.
            cache.retire_deferred()
        }
    })
}

/// Records and retires this app's shared root frames.
///
/// Call this before taking the session registry: refreshes are dispatched only
/// after WAL selection marks sessions under that lock, so retiring a WAL's
/// frames first guarantees a selected refresh cannot read them. Frames are
/// pre-permission, so a topic miss proves this WAL cannot change the rows or
/// entity data a frame carries. Retire only intersecting ready frames; in-flight
/// loads publish conditionally against the frame epoch recorded here. An
/// unprecise routing or security change still retires or clears the app.
///
/// Drop the returned payload ownership after the session lock (if any) is
/// released, as with session results.
pub(super) fn invalidate_app_root_frames(
    inner: &RealtimeInner,
    app_id: Uuid,
    security: bool,
    routing: Option<Arc<[instant_wal::Topic]>>,
) -> RootFrameCacheCleanup {
    // Same "record only into an existing epoch" rule as session results: the
    // frame publisher creates the epoch before its load opens a snapshot, so an
    // invalidation that found none committed before any such load started.
    // Creating one here would retain a bounded WAL log for every app with WAL
    // traffic but no frame reader.
    if let Some(epoch) = frame_epochs(inner).get(app_id) {
        let _recorded = epoch.record(security, routing.clone());
    }
    inner.root_frame_caches.as_ref().and_then(|caches| {
        if security {
            caches.clear_deferred(app_id)
        } else if let Some(topics) = routing.as_deref() {
            caches.retire_wal_deferred(app_id, topics)
        } else {
            caches.retire_deferred(app_id)
        }
    })
}

pub(super) fn clear_app_query_cache(inner: &RealtimeInner, app_id: Uuid) {
    // Frames first, as on the WAL path: a refresh cannot be selected until the
    // session lock below is taken.
    let root_frames = invalidate_app_root_frames(inner, app_id, true, None);
    let cleanup;
    {
        let _entries = sessions(inner);
        cleanup = invalidate_app_query_reuse(inner, app_id, true, None);
    }
    drop(cleanup);
    drop(root_frames);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_epoch_is_default_until_the_first_invalidation() {
        let epochs = AppInvalidationEpochs::default();
        assert!(epochs.get(Uuid::new_v4()).is_none());
    }
}
