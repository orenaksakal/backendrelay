//! Brief reuse of planner-only attribute sketch snapshots.
//!
//! Sketches feed cardinality estimates that choose a physical plan; they never
//! change which rows a query returns. `attr_sketches` is itself maintained by a
//! lagging aggregator, so retaining a snapshot for a moment adds no meaningful
//! staleness while removing one dynamically rendered and planned statement per
//! query shape. Every entry is keyed by the exact app and attribute sets the
//! statement was bounded by, and expires quickly.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::PlannerAttrSketch;

const MAX_ENTRIES: usize = 256;
const MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SketchSnapshotKey {
    app_id: Uuid,
    total: Vec<Uuid>,
    forward: Vec<Uuid>,
    reverse: Vec<Uuid>,
}

impl SketchSnapshotKey {
    /// Sets are canonicalized because their iteration order is not stable and
    /// the same query shape must address the same entry.
    pub(crate) fn new(app_id: Uuid, total: &[Uuid], forward: &[Uuid], reverse: &[Uuid]) -> Self {
        fn canonical(ids: &[Uuid]) -> Vec<Uuid> {
            let mut ids = ids.to_vec();
            ids.sort_unstable();
            ids.dedup();
            ids
        }
        Self {
            app_id,
            total: canonical(total),
            forward: canonical(forward),
            reverse: canonical(reverse),
        }
    }
}

/// Totals and decoded sketches exactly as the snapshot statement returned them.
#[derive(Clone, Default)]
pub(crate) struct SketchSnapshot {
    pub totals: Vec<(Uuid, u64)>,
    pub forward: HashMap<Uuid, PlannerAttrSketch>,
    pub reverse: HashMap<Uuid, PlannerAttrSketch>,
}

struct Entry {
    snapshot: SketchSnapshot,
    loaded_at: Instant,
}

static SNAPSHOTS: LazyLock<Mutex<HashMap<SketchSnapshotKey, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn get(key: &SketchSnapshotKey) -> Option<SketchSnapshot> {
    get_at(key, Instant::now())
}

fn get_at(key: &SketchSnapshotKey, now: Instant) -> Option<SketchSnapshot> {
    let mut snapshots = lock();
    let entry = snapshots.get(key)?;
    if now.saturating_duration_since(entry.loaded_at) >= MAX_AGE {
        snapshots.remove(key);
        return None;
    }
    Some(entry.snapshot.clone())
}

pub(crate) fn insert(key: SketchSnapshotKey, snapshot: SketchSnapshot) {
    insert_at(key, snapshot, Instant::now());
}

fn insert_at(key: SketchSnapshotKey, snapshot: SketchSnapshot, now: Instant) {
    let mut snapshots = lock();
    if snapshots.len() >= MAX_ENTRIES {
        snapshots.retain(|_, entry| now.saturating_duration_since(entry.loaded_at) < MAX_AGE);
        if snapshots.len() >= MAX_ENTRIES {
            snapshots.clear();
        }
    }
    snapshots.insert(
        key,
        Entry {
            snapshot,
            loaded_at: now,
        },
    );
}

#[cfg(test)]
pub(crate) fn clear() {
    lock().clear();
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<SketchSnapshotKey, Entry>> {
    SNAPSHOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(app_id: Uuid, total: &[Uuid]) -> SketchSnapshotKey {
        SketchSnapshotKey::new(app_id, total, &[], &[])
    }

    #[test]
    fn identical_query_shapes_reuse_a_snapshot_until_it_expires() {
        let app = Uuid::new_v4();
        let attribute = Uuid::new_v4();
        let loaded = Instant::now();
        let key = key(app, &[attribute]);
        insert_at(
            key.clone(),
            SketchSnapshot {
                totals: vec![(attribute, 7)],
                ..SketchSnapshot::default()
            },
            loaded,
        );
        assert_eq!(
            get_at(&key, loaded + Duration::from_millis(500))
                .unwrap()
                .totals,
            vec![(attribute, 7)]
        );
        assert!(get_at(&key, loaded + MAX_AGE).is_none());
        assert!(get_at(&key, loaded + MAX_AGE).is_none());
    }

    #[test]
    fn app_and_requested_sets_are_part_of_the_key() {
        let app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let snapshot = SketchSnapshot {
            totals: vec![(first, 1), (second, 2)],
            ..SketchSnapshot::default()
        };
        insert_at(key(app, &[second, first]), snapshot, Instant::now());
        assert!(get(&key(app, &[first, second])).is_some());
        assert!(get(&key(Uuid::new_v4(), &[first, second])).is_none());
        assert!(get(&key(app, &[first])).is_none());
    }
}
