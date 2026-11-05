//! Indexed deduplication and tenant-local reads with the original global FIFO
//! retention budget. Transaction IDs need not arrive in increasing order.
use std::collections::{HashMap, HashSet, VecDeque};

use uuid::Uuid;

use super::SyncTransaction;

#[derive(Default)]
pub(super) struct SyncHistoryState {
    order: VecDeque<(Uuid, i64)>,
    identities: HashSet<(Uuid, i64)>,
    apps: HashMap<Uuid, VecDeque<SyncTransaction>>,
    pub(super) evicted_through: HashMap<Uuid, i64>,
}

impl SyncHistoryState {
    pub(super) fn record(&mut self, transaction: SyncTransaction, limit: usize) {
        let key = (transaction.app_id, transaction.tx_id);
        if !self.identities.insert(key) {
            return;
        }
        self.order.push_back(key);
        self.apps.entry(key.0).or_default().push_back(transaction);
        while self.order.len() > limit {
            let (app, tx) = self.order.pop_front().expect("history exceeds its limit");
            self.identities.remove(&(app, tx));
            let transactions = self.apps.get_mut(&app).expect("retained app exists");
            let expired = transactions
                .pop_front()
                .expect("retained transaction exists");
            debug_assert_eq!(expired.tx_id, tx);
            if transactions.is_empty() {
                self.apps.remove(&app);
            }
            self.evicted_through
                .entry(app)
                .and_modify(|evicted| *evicted = (*evicted).max(tx))
                .or_insert(tx);
        }
    }

    /// Preserve arrival order, including out-of-order transaction IDs and replay
    /// after eviction. No rows belonging to another app are visited.
    pub(super) fn after(&self, app: Uuid, sent_tx: i64) -> impl Iterator<Item = &SyncTransaction> {
        self.for_app(app)
            .filter(move |transaction| transaction.tx_id > sent_tx)
    }

    pub(super) fn for_app(&self, app: Uuid) -> impl Iterator<Item = &SyncTransaction> {
        self.apps
            .get(&app)
            .into_iter()
            .flat_map(|transactions| transactions.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transaction(app: Uuid, tx: i64, value: u64) -> SyncTransaction {
        SyncTransaction {
            app_id: app,
            tx_id: tx,
            changes: vec![json!({"value": value})],
        }
    }

    fn assert_consistent(history: &SyncHistoryState) {
        let keys = history.order.iter().copied().collect::<HashSet<_>>();
        assert_eq!(keys, history.identities);
        assert_eq!(keys.len(), history.order.len());
        assert_eq!(
            history.apps.values().map(VecDeque::len).sum::<usize>(),
            keys.len()
        );
        for (app, transactions) in &history.apps {
            assert!(!transactions.is_empty());
            let ordered = history
                .order
                .iter()
                .filter(|(owner, _)| owner == app)
                .copied()
                .collect::<Vec<_>>();
            let indexed = transactions
                .iter()
                .map(|transaction| {
                    assert_eq!(transaction.app_id, *app);
                    (*app, transaction.tx_id)
                })
                .collect::<Vec<_>>();
            assert_eq!(ordered, indexed);
        }
    }

    #[test]
    fn fifo_replay_and_duplicate_payload_semantics_are_unchanged() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut history = SyncHistoryState::default();
        history.record(transaction(a, 9, 1), 3);
        history.record(transaction(b, 9, 2), 3);
        history.record(transaction(a, 2, 3), 3);
        history.record(transaction(a, 9, 99), 3);
        assert_eq!(
            history.after(a, 0).map(|tx| tx.tx_id).collect::<Vec<_>>(),
            vec![9, 2]
        );
        assert_eq!(
            history.after(a, 0).next().unwrap().changes,
            vec![json!({"value": 1})]
        );
        assert!(
            history.evicted_through.is_empty(),
            "duplicates never move FIFO positions"
        );
        history.record(transaction(b, 3, 4), 3);
        assert_eq!(history.evicted_through.get(&a), Some(&9));
        history.record(transaction(a, 9, 5), 3);
        assert_eq!(history.evicted_through.get(&b), Some(&9));
        assert_eq!(
            history.after(a, 0).map(|tx| tx.tx_id).collect::<Vec<_>>(),
            vec![2, 9]
        );
        assert_eq!(
            history.after(a, 2).next().unwrap().changes,
            vec![json!({"value": 5})]
        );
        assert_consistent(&history);
    }

    #[test]
    fn indexed_history_matches_the_scan_based_reference_under_replay_and_eviction() {
        let apps = (0..7).map(Uuid::from_u128).collect::<Vec<_>>();
        let mut history = SyncHistoryState::default();
        let mut reference = VecDeque::<SyncTransaction>::new();
        let mut evicted = HashMap::<Uuid, i64>::new();
        let mut seed = 7_u64;
        for step in 0..2_000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let app = apps[(seed as usize) % apps.len()];
            let tx = i64::try_from((seed >> 32) % 31).unwrap() - 5;
            let limit = if step % 113 == 0 { 0 } else { 23 };
            if !reference
                .iter()
                .any(|entry| entry.app_id == app && entry.tx_id == tx)
            {
                reference.push_back(transaction(app, tx, step));
                while reference.len() > limit {
                    let expired = reference.pop_front().unwrap();
                    evicted
                        .entry(expired.app_id)
                        .and_modify(|tx| *tx = (*tx).max(expired.tx_id))
                        .or_insert(expired.tx_id);
                }
            }
            history.record(transaction(app, tx, step), limit);
            assert_eq!(history.evicted_through, evicted);
            for app in &apps {
                for sent in [-6, 0, 15, i64::MAX] {
                    let expected = reference
                        .iter()
                        .filter(|entry| entry.app_id == *app && entry.tx_id > sent)
                        .map(|entry| (entry.tx_id, &entry.changes))
                        .collect::<Vec<_>>();
                    let actual = history
                        .after(*app, sent)
                        .map(|entry| (entry.tx_id, &entry.changes))
                        .collect::<Vec<_>>();
                    assert_eq!(actual, expected);
                }
            }
            assert_consistent(&history);
        }
    }

    #[test]
    fn ten_thousand_tenants_share_one_budget_and_empty_app_buckets_are_removed() {
        let mut history = SyncHistoryState::default();
        for app in 0..10_000 {
            history.record(transaction(Uuid::from_u128(app), 1, 0), 10_000);
        }
        let app = Uuid::from_u128(9_999);
        assert_eq!(history.after(app, 0).count(), 1);
        assert_eq!(history.apps[&app].len(), 1);
        for app in 10_000..20_000 {
            history.record(transaction(Uuid::from_u128(app), 2, 0), 10_000);
        }
        assert!(history.after(app, 0).next().is_none());
        assert!(!history.apps.contains_key(&app));
        assert_eq!(history.apps.len(), 10_000);
        assert_eq!(history.identities.len(), 10_000);
        assert_eq!(history.order.len(), 10_000);
        history.record(transaction(Uuid::nil(), 3, 0), 0);
        assert!(
            history.apps.is_empty() && history.identities.is_empty() && history.order.is_empty()
        );
    }
}
