//! Per-app data/security epochs and the bounded WAL topic log.
//!
//! Invalidation records an epoch, then retires reusable entries under the cache
//! lock. A conditional publisher compares the epoch version under that same lock
//! before inserting a completed load. A WAL therefore cannot slip between the
//! check and the insert: either the insert precedes the retirement scan (and is
//! retired if affected), or the version changed and the insert is refused.
//!
//! There is one [`AppInvalidationEpochs`] registry per cache protocol. Session
//! query results use the service's `query_epochs` and record under the session
//! registry lock. Shared root frames use a separate `frame_epochs` registry and
//! record *before* that lock is taken, because a WAL's frames must be retired
//! before any refresh it selects runs, while frame correctness is already fenced
//! by the frame epoch and the frame cache's own state lock.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use instant_wal::Topic;

use crate::cache::{PublishPermit, PublishToken, QueryCachePublisher};

/// About three seconds of WAL at 10k-user burst write rates; an older window is
/// answered conservatively, exactly as every window was before this log.
pub const DATA_INVALIDATION_LOG_LIMIT: usize = 1024;

/// One logged invalidation: its data epoch and, when precise, the WAL topics.
pub type DataInvalidationLog = VecDeque<(u64, Option<Arc<[Topic]>>)>;

/// Current data and security epochs. `version` changes on every invalidation,
/// including ones that do not affect a particular query.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EpochSnapshot {
    pub version: u64,
    pub data: u64,
    pub security: u64,
}

impl EpochSnapshot {
    #[must_use]
    pub const fn token(self) -> PublishToken {
        PublishToken {
            version: self.version,
            data: self.data,
            security: self.security,
        }
    }
}

#[derive(Debug, Default)]
struct EpochState {
    data: u64,
    security: u64,
    log: DataInvalidationLog,
}

/// One app's invalidation epoch and bounded topic history.
#[derive(Debug)]
pub struct InvalidationEpoch {
    version: AtomicU64,
    state: Mutex<EpochState>,
    log_limit: usize,
}

impl InvalidationEpoch {
    #[must_use]
    pub fn new(log_limit: usize) -> Self {
        Self {
            version: AtomicU64::new(0),
            state: Mutex::new(EpochState::default()),
            log_limit,
        }
    }

    /// Snapshot for a load that is about to start.
    #[must_use]
    pub fn snapshot(&self) -> EpochSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        EpochSnapshot {
            version: self.version.load(Ordering::Acquire),
            data: state.data,
            security: state.security,
        }
    }

    #[must_use]
    pub fn current(&self) -> EpochSnapshot {
        self.snapshot()
    }

    /// Records one invalidation and returns its new snapshot.
    ///
    /// The caller must retire ready cache entries after this returns, so an
    /// affected insertion that raced the version bump is removed by that scan.
    pub fn record(&self, security: bool, routing: Option<Arc<[Topic]>>) -> EpochSnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.data = state.data.wrapping_add(1);
        if security {
            state.security = state.security.wrapping_add(1);
        }
        if state.log.len() == self.log_limit {
            state.log.pop_front();
        }
        let data = state.data;
        state.log.push_back((data, routing));
        let version = self.version.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        EpochSnapshot {
            version,
            data: state.data,
            security: state.security,
        }
    }

    /// Whether a data invalidation in `(since, until]` may affect `affects`.
    ///
    /// An imprecise routing, a missing app epoch, or a window older than the
    /// bounded log answers true.
    pub fn data_invalidated_since(
        &self,
        since: u64,
        until: u64,
        affects: impl Fn(&[Topic]) -> bool,
    ) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        logged_window_affects(&state.log, since, until, affects)
    }

    /// Authorizes publication of a value loaded after `token`, or rejects it.
    ///
    /// Security changes always reject. Data invalidations reject when their
    /// routing was imprecise, intersected the value, or fell off the bounded
    /// log. The caller confirms the returned permit under the cache lock so a
    /// new invalidation cannot interleave with insertion.
    #[must_use]
    pub fn authorize(
        self: &Arc<Self>,
        token: PublishToken,
        affects: impl Fn(&[Topic]) -> bool,
    ) -> Option<EpochPermit> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.security != token.security {
            return None;
        }
        let version = self.version.load(Ordering::Acquire);
        if version != token.version
            && logged_window_affects(&state.log, token.data, state.data, affects)
        {
            return None;
        }
        Some(EpochPermit {
            epoch: Arc::clone(self),
            version,
        })
    }
}

/// Authorization returned by [`InvalidationEpoch::authorize`].
pub struct EpochPermit {
    epoch: Arc<InvalidationEpoch>,
    version: u64,
}

impl EpochPermit {
    /// Confirms under the cache lock that no invalidation interleaved.
    #[must_use]
    pub fn confirm(&self) -> bool {
        self.epoch.version.load(Ordering::Acquire) == self.version
    }

    /// Converts this epoch authorization into the cache's publication permit.
    #[must_use]
    pub fn into_publish_permit(self) -> PublishPermit<'static> {
        let epoch = Arc::clone(&self.epoch);
        let version = self.version;
        PublishPermit::guarded(Box::new(Arc::clone(&self.epoch)), move || {
            epoch.version.load(Ordering::Acquire) == version
        })
    }
}

impl std::fmt::Debug for EpochPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EpochPermit")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Whether a logged invalidation window may affect a query.
pub fn logged_window_affects(
    log: &DataInvalidationLog,
    since: u64,
    until: u64,
    affects: impl Fn(&[Topic]) -> bool,
) -> bool {
    let missed = until.wrapping_sub(since);
    let mut checked = 0_u64;
    for (data, topics) in log {
        // Epochs wrap; position within the window is measured from `since`.
        if data.wrapping_sub(since).wrapping_sub(1) >= missed {
            continue;
        }
        checked += 1;
        match topics {
            Some(topics) if !affects(topics) => {}
            _ => return true,
        }
    }
    checked != missed
}

/// Process registry of independently versioned per-app epochs.
///
/// Each cache protocol owns its own registry so one protocol's invalidation
/// cannot record into another's history (see the module docs).
#[derive(Debug)]
pub struct AppInvalidationEpochs {
    epochs: Mutex<HashMap<uuid::Uuid, Arc<InvalidationEpoch>>>,
    log_limit: usize,
}

impl AppInvalidationEpochs {
    #[must_use]
    pub fn new(log_limit: usize) -> Self {
        Self {
            epochs: Mutex::new(HashMap::new()),
            log_limit,
        }
    }

    #[must_use]
    pub fn get(&self, app_id: uuid::Uuid) -> Option<Arc<InvalidationEpoch>> {
        self.epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&app_id)
            .cloned()
    }

    #[must_use]
    pub fn get_or_create(&self, app_id: uuid::Uuid) -> Arc<InvalidationEpoch> {
        Arc::clone(
            self.epochs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(app_id)
                .or_insert_with(|| Arc::new(InvalidationEpoch::new(self.log_limit))),
        )
    }

    pub fn remove(&self, app_id: uuid::Uuid) -> Option<Arc<InvalidationEpoch>> {
        self.epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&app_id)
    }
}

impl Default for AppInvalidationEpochs {
    fn default() -> Self {
        Self::new(DATA_INVALIDATION_LOG_LIMIT)
    }
}

/// Adapter that publishes cache values only while a data/security epoch allows.
///
/// `affects` receives each invalidation's topics and decides whether the value
/// depends on them. A value with no precise topics should answer true.
pub struct EpochPublisher<V> {
    epoch: Arc<InvalidationEpoch>,
    affects: Arc<dyn Fn(&V, &[Topic]) -> bool + Send + Sync>,
}

impl<V> EpochPublisher<V> {
    #[must_use]
    pub fn new(
        epoch: Arc<InvalidationEpoch>,
        affects: impl Fn(&V, &[Topic]) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            epoch,
            affects: Arc::new(affects),
        }
    }
}

impl<V> QueryCachePublisher<V> for EpochPublisher<V>
where
    V: Send + Sync + 'static,
{
    fn begin(&self) -> PublishToken {
        self.epoch.snapshot().token()
    }

    fn authorize<'a>(&'a self, token: PublishToken, value: &V) -> Option<PublishPermit<'a>> {
        let affects = &self.affects;
        self.epoch
            .authorize(token, |topics| affects(value, topics))
            .map(EpochPermit::into_publish_permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_wal::{MatchSet, Topic};
    use uuid::Uuid;

    fn topics(attribute: u128) -> Option<Arc<[Topic]>> {
        Some(Arc::from(vec![Topic {
            indexes: Vec::new(),
            entities: MatchSet::Any,
            attributes: MatchSet::Values(vec![Uuid::from_u128(attribute)]),
            values: MatchSet::Any,
        }]))
    }

    fn touches(attribute: u128) -> impl Fn(&[Topic]) -> bool {
        move |topics| {
            topics.iter().any(|topic| {
                matches!(&topic.attributes, MatchSet::Values(values)
                    if values.contains(&Uuid::from_u128(attribute)))
            })
        }
    }

    #[test]
    fn logged_window_checks_only_invalidations_after_the_start() {
        let log: DataInvalidationLog = [(1, topics(9)), (2, topics(1)), (3, topics(2))].into();
        assert!(!logged_window_affects(&log, 1, 3, touches(9)));
        assert!(logged_window_affects(&log, 1, 3, touches(2)));
        assert!(logged_window_affects(&log, 0, 3, touches(9)));
        assert!(!logged_window_affects(&log, 3, 3, |_| true));
    }

    #[test]
    fn logged_window_is_conservative_for_imprecise_or_evicted_entries() {
        let log: DataInvalidationLog = [(5, topics(1)), (6, None), (7, topics(1))].into();
        assert!(logged_window_affects(&log, 5, 7, touches(9)));
        assert!(!logged_window_affects(&log, 6, 7, touches(9)));
        // Epoch 4 was evicted from the log, so the window cannot be verified.
        assert!(logged_window_affects(&log, 3, 5, touches(9)));
    }

    #[test]
    fn logged_window_survives_epoch_wraparound() {
        let log: DataInvalidationLog =
            [(u64::MAX, topics(1)), (0, topics(2)), (1, topics(3))].into();
        assert!(!logged_window_affects(&log, u64::MAX - 1, 1, touches(9)));
        assert!(logged_window_affects(&log, u64::MAX, 1, touches(3)));
        assert!(!logged_window_affects(&log, u64::MAX, 1, touches(1)));
    }

    #[test]
    fn authorize_allows_unrelated_invalidations_and_blocks_affected_ones() {
        let epoch = Arc::new(InvalidationEpoch::new(8));
        let token = epoch.snapshot().token();
        epoch.record(false, topics(1));
        assert!(epoch.authorize(token, touches(9)).is_some());
        assert!(epoch.authorize(token, touches(1)).is_none());
        // Imprecise routing always blocks.
        epoch.record(false, None);
        assert!(epoch.authorize(token, touches(9)).is_none());
        // Security always blocks.
        let epoch = Arc::new(InvalidationEpoch::new(8));
        let token = epoch.snapshot().token();
        epoch.record(true, topics(1));
        assert!(epoch.authorize(token, touches(9)).is_none());
    }

    #[test]
    fn authorize_blocks_when_the_window_falls_off_the_bounded_log() {
        let epoch = Arc::new(InvalidationEpoch::new(1));
        let token = epoch.snapshot().token();
        epoch.record(false, topics(1));
        epoch.record(false, topics(2));
        assert!(epoch.authorize(token, touches(9)).is_none());
    }

    #[test]
    fn permit_confirmation_fails_after_another_invalidation() {
        let epoch = Arc::new(InvalidationEpoch::new(8));
        let token = epoch.snapshot().token();
        let permit = epoch.authorize(token, touches(1)).unwrap();
        assert!(permit.confirm());
        epoch.record(false, topics(9));
        assert!(!permit.confirm());
    }
}
