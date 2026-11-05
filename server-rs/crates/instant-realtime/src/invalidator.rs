use std::collections::BTreeSet;

use instant_wal::{Isn, Lsn, TopicError, WalAction, WalRecord, topics_for_record};
use uuid::Uuid;

use crate::RealtimeStore;

/// Delivery work produced after one application WAL record is applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DispatchOutcome {
    pub refresh_sessions: BTreeSet<Uuid>,
    pub invalidated_datalog_queries: usize,
    pub topic_count: usize,
    pub schema_refresh: bool,
    pub continuity_gap: Option<ContinuityGap>,
    pub durable_lsn: Lsn,
}

/// Discontinuity between the last observed record and the producer's predecessor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContinuityGap {
    pub last_seen: Isn,
    pub record_previous: Option<Isn>,
    pub record_current: Isn,
}

/// Ordered bridge from transformed WAL records to realtime invalidation state.
#[derive(Debug, Default)]
pub struct InvalidationCoordinator {
    latest_isn: Option<Isn>,
}

impl InvalidationCoordinator {
    #[must_use]
    pub const fn latest_isn(&self) -> Option<Isn> {
        self.latest_isn
    }

    /// Applies one record, preserving monotonic continuity tracking and returning
    /// the exact sessions that need refresh events.
    ///
    /// # Errors
    ///
    /// Returns [`TopicError`] if a supported WAL row cannot be converted to a typed
    /// invalidation topic. The store is not modified in that case.
    pub fn dispatch(
        &mut self,
        store: &mut RealtimeStore,
        record: &WalRecord,
    ) -> Result<DispatchOutcome, TopicError> {
        let topics = topics_for_record(record)?;
        let prior = self.latest_isn;
        let continuity_gap = prior
            .filter(|last_seen| record.previous_isn != Some(*last_seen))
            .map(|last_seen| ContinuityGap {
                last_seen,
                record_previous: record.previous_isn,
                record_current: record.isn,
            });
        self.latest_isn = Some(prior.map_or(record.isn, |old| old.max(record.isn)));

        let invalidation = store.invalidate(record.app_id, record.tx_id, record.isn, &topics);
        let schema_refresh = record.attr_changes.iter().any(|change| {
            matches!(
                change.action,
                WalAction::Insert | WalAction::Delete | WalAction::Truncate
            )
        }) || !record.ident_changes.is_empty()
            || record
                .triple_changes
                .iter()
                .any(|change| change.action == WalAction::Truncate);
        let mut refresh_sessions = invalidation.stale_sessions;
        if schema_refresh {
            refresh_sessions.extend(store.session_ids_for_app(record.app_id));
        }
        Ok(DispatchOutcome {
            refresh_sessions,
            invalidated_datalog_queries: invalidation.invalidated_datalog_queries,
            topic_count: topics.len(),
            schema_refresh,
            continuity_gap,
            durable_lsn: record.next_lsn,
        })
    }
}

#[cfg(test)]
mod tests {
    use instant_wal::{Lsn, WalColumn, WalEntry};
    use serde_json::{Value, json};

    use super::*;
    use crate::Session;

    fn change(action: WalAction, table: &str, columns: Vec<(&str, Value)>) -> WalEntry {
        WalEntry {
            action,
            tx_bytes: 0,
            table: Some(table.to_owned()),
            columns: columns
                .into_iter()
                .map(|(name, value)| WalColumn {
                    name: name.to_owned(),
                    value,
                })
                .collect(),
            identity: Vec::new(),
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    fn record(app_id: Uuid, isn: Isn, previous_isn: Option<Isn>) -> WalRecord {
        WalRecord {
            app_id,
            tx_id: 10,
            isn,
            previous_isn,
            tx_created_at: None,
            tx_bytes: 100,
            next_lsn: isn.lsn(),
            attr_changes: Vec::new(),
            ident_changes: vec![change(
                WalAction::Insert,
                "idents",
                vec![("attr_id", json!(Uuid::new_v4()))],
            )],
            triple_changes: Vec::new(),
            messages: Vec::new(),
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }

    #[test]
    fn schema_changes_refresh_all_app_sessions_and_advance_metadata() {
        let app = Uuid::new_v4();
        let other_app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let mut store = RealtimeStore::default();
        for (session_id, app_id) in [(first, app), (second, app), (unrelated, other_app)] {
            store.upsert_session(Session {
                app_id: Some(app_id),
                ..Session::new(session_id)
            });
        }
        let isn = Isn::new(2, Lsn::from_u64(100));
        let outcome = InvalidationCoordinator::default()
            .dispatch(&mut store, &record(app, isn, None))
            .unwrap();
        assert!(outcome.schema_refresh);
        assert_eq!(outcome.refresh_sessions, BTreeSet::from([first, second]));
        assert_eq!(outcome.topic_count, 1);
        assert_eq!(outcome.durable_lsn, Lsn::from_u64(100));
        assert_eq!(store.processed_tx_id(app), Some(10));
        assert_eq!(store.processed_isn(app), Some(isn));
    }

    #[test]
    fn attribute_truncation_refreshes_every_app_session() {
        let app = Uuid::new_v4();
        let other_app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let mut store = RealtimeStore::default();
        for (session_id, app_id) in [(first, app), (second, app), (unrelated, other_app)] {
            store.upsert_session(Session {
                app_id: Some(app_id),
                ..Session::new(session_id)
            });
        }

        let isn = Isn::new(2, Lsn::from_u64(100));
        let mut record = record(app, isn, None);
        record.ident_changes.clear();
        record
            .attr_changes
            .push(change(WalAction::Truncate, "attrs", Vec::new()));

        let outcome = InvalidationCoordinator::default()
            .dispatch(&mut store, &record)
            .unwrap();
        assert!(outcome.schema_refresh);
        assert_eq!(outcome.refresh_sessions, BTreeSet::from([first, second]));
        assert_eq!(outcome.topic_count, 0);
        assert_eq!(store.processed_tx_id(app), Some(10));
        assert_eq!(store.processed_isn(app), Some(isn));
    }

    #[test]
    fn triple_truncation_refreshes_sessions_without_topic_dependencies() {
        let app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut store = RealtimeStore::default();
        for session_id in [first, second] {
            store.upsert_session(Session {
                app_id: Some(app),
                ..Session::new(session_id)
            });
        }

        let isn = Isn::new(2, Lsn::from_u64(101));
        let mut record = record(app, isn, None);
        record.ident_changes.clear();
        record
            .triple_changes
            .push(change(WalAction::Truncate, "triples", Vec::new()));

        let outcome = InvalidationCoordinator::default()
            .dispatch(&mut store, &record)
            .unwrap();
        assert!(outcome.schema_refresh);
        assert_eq!(outcome.refresh_sessions, BTreeSet::from([first, second]));
        assert_eq!(outcome.topic_count, 1);
    }

    #[test]
    fn continuity_is_monotonic_and_reports_missing_predecessors() {
        let app = Uuid::new_v4();
        let first = Isn::new(2, Lsn::from_u64(100));
        let second = Isn::new(2, Lsn::from_u64(200));
        let stale = Isn::new(2, Lsn::from_u64(50));
        let mut store = RealtimeStore::default();
        let mut coordinator = InvalidationCoordinator::default();
        assert!(
            coordinator
                .dispatch(&mut store, &record(app, first, None))
                .unwrap()
                .continuity_gap
                .is_none()
        );
        let gap = coordinator
            .dispatch(&mut store, &record(app, second, Some(stale)))
            .unwrap()
            .continuity_gap
            .unwrap();
        assert_eq!(gap.last_seen, first);
        assert_eq!(gap.record_previous, Some(stale));
        assert_eq!(coordinator.latest_isn(), Some(second));
        coordinator
            .dispatch(&mut store, &record(app, stale, Some(first)))
            .unwrap();
        assert_eq!(coordinator.latest_isn(), Some(second));
    }
}
