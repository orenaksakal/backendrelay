//! App membership and session entries share one lock and one mutation boundary.
//! App-scoped WAL admission and disconnect cleanup must not scan every tenant.
use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::ops::Deref;

use uuid::Uuid;

use super::RuntimeSessionEntry;

#[derive(Default)]
pub(super) struct SessionRegistry {
    entries: HashMap<Uuid, RuntimeSessionEntry>,
    apps: HashMap<Uuid, BTreeSet<Uuid>>,
    sessions_by_ip: HashMap<IpAddr, usize>,
}

impl Deref for SessionRegistry {
    type Target = HashMap<Uuid, RuntimeSessionEntry>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl SessionRegistry {
    pub(super) fn insert(
        &mut self,
        session_id: Uuid,
        entry: RuntimeSessionEntry,
    ) -> Option<RuntimeSessionEntry> {
        let previous = self.remove(&session_id);
        if let Some(app_id) = entry.app_id {
            self.apps.entry(app_id).or_default().insert(session_id);
        }
        if let Some(client_ip) = entry.client_ip {
            *self.sessions_by_ip.entry(client_ip).or_default() += 1;
        }
        self.entries.insert(session_id, entry);
        previous
    }

    pub(super) fn remove(&mut self, session_id: &Uuid) -> Option<RuntimeSessionEntry> {
        let entry = self.entries.remove(session_id)?;
        if let Some(app_id) = entry.app_id {
            self.remove_membership(app_id, session_id);
        }
        if let Some(client_ip) = entry.client_ip
            && let Some(count) = self.sessions_by_ip.get_mut(&client_ip)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.sessions_by_ip.remove(&client_ip);
            }
        }
        Some(entry)
    }

    pub(super) fn count_for_ip(&self, client_ip: IpAddr) -> usize {
        self.sessions_by_ip
            .get(&client_ip)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn count_for_app(&self, app_id: Uuid) -> usize {
        self.apps
            .get(&app_id)
            .map_or(0, std::collections::BTreeSet::len)
    }

    pub(super) fn get_mut(&mut self, session_id: &Uuid) -> Option<&mut RuntimeSessionEntry> {
        self.entries.get_mut(session_id)
    }

    // This is the only place that changes an existing entry's app membership.
    // Other mutable access is for queries, worker state and authenticated fields.
    pub(super) fn attach_app(
        &mut self,
        session_id: Uuid,
        app_id: Uuid,
    ) -> Option<&mut RuntimeSessionEntry> {
        let previous = self.entries.get(&session_id)?.app_id;
        if previous != Some(app_id) {
            if let Some(previous) = previous {
                self.remove_membership(previous, &session_id);
            }
            self.apps.entry(app_id).or_default().insert(session_id);
        }
        let entry = self.entries.get_mut(&session_id)?;
        entry.app_id = Some(app_id);
        Some(entry)
    }

    pub(super) fn has_app(&self, app_id: Uuid) -> bool {
        self.apps.contains_key(&app_id)
    }

    pub(super) fn app_ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.apps.keys().copied()
    }

    pub(super) fn ids_for_app(&self, app_id: Uuid) -> impl Iterator<Item = Uuid> + '_ {
        self.apps
            .get(&app_id)
            .into_iter()
            .flat_map(BTreeSet::iter)
            .copied()
    }

    fn remove_membership(&mut self, app_id: Uuid, session_id: &Uuid) {
        if let Some(sessions) = self.apps.get_mut(&app_id) {
            sessions.remove(session_id);
            if sessions.is_empty() {
                self.apps.remove(&app_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    fn entry(app_id: Option<Uuid>) -> RuntimeSessionEntry {
        RuntimeSessionEntry {
            app_id,
            origin: None,
            client_ip: None,
            actor: AdminActor::Guest,
            bearer_token: None,
            versions: None,
            client_features: ClientFeatures::default(),
            token_hash: None,
            user_id: None,
            sender: RuntimeSessionSender {
                sender: mpsc::unbounded_channel().0,
                state: Arc::new(OutboundQueueState::default()),
                presence: Arc::new(Mutex::new(OutboundPresenceState::default())),
                disconnect: Arc::new(|| {}),
            },
            drain: RuntimeDrainSignal::default(),
            queries: HashMap::new(),
            refresh_not_before: None,
            transact_sender: mpsc::channel(1).0,
            transact_worker: Arc::new(TransactionWorkerControl::default()),
            refresh_sender: mpsc::channel(1).0,
            refresh_worker: Arc::new(TransactionWorkerControl::default()),
            next_refresh_generation: 0,
            refresh_attrs_generation: 0,
            refresh_force_generation: 0,
            wal_sink_consecutive_timeouts: 0,
            external_catalog: false,
            catalog_digest: None,
            active_handlers: Arc::new(ActiveStatementHandlers::default()),
        }
    }

    fn assert_consistent(registry: &SessionRegistry) {
        let mut expected: HashMap<Uuid, BTreeSet<Uuid>> = HashMap::new();
        for (id, entry) in registry.iter() {
            if let Some(app) = entry.app_id {
                expected.entry(app).or_default().insert(*id);
            }
        }
        assert_eq!(registry.apps, expected);
    }

    #[test]
    fn membership_tracks_init_replacement_reassignment_and_final_disconnect() {
        let mut registry = SessionRegistry::default();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let session = Uuid::new_v4();
        registry.insert(session, entry(None));
        assert!(!registry.has_app(first));
        registry.attach_app(session, first).unwrap();
        registry.attach_app(session, first).unwrap();
        assert_eq!(
            registry.ids_for_app(first).collect::<Vec<_>>(),
            vec![session]
        );
        assert_eq!(registry.count_for_app(first), 1);
        assert_eq!(registry.count_for_app(second), 0);
        assert_consistent(&registry);

        registry.attach_app(session, second).unwrap();
        assert!(!registry.has_app(first));
        assert_eq!(registry.count_for_app(first), 0);
        assert_eq!(registry.count_for_app(second), 1);
        assert_consistent(&registry);
        assert_eq!(
            registry.insert(session, entry(Some(first))).unwrap().app_id,
            Some(second)
        );
        assert!(!registry.has_app(second));
        assert_consistent(&registry);

        registry.insert(session, entry(None));
        assert!(!registry.has_app(first));
        assert_consistent(&registry);
        registry.remove(&session).unwrap();
        assert!(registry.remove(&session).is_none());
        assert!(registry.attach_app(session, first).is_none());
        assert!(registry.apps.is_empty());
        assert!(registry.is_empty());
    }

    #[test]
    fn ten_thousand_sessions_keep_app_scoped_selection_and_cleanup_exact() {
        let mut registry = SessionRegistry::default();
        let apps = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let mut ids = Vec::new();
        for index in 0..10_000 {
            let id = Uuid::new_v4();
            registry.insert(id, entry(Some(apps[index % apps.len()])));
            ids.push(id);
        }
        assert_eq!(registry.len(), 10_000);
        assert_consistent(&registry);
        for app in apps {
            assert_eq!(registry.ids_for_app(app).count(), 2_000);
        }
        for id in ids.iter().step_by(apps.len()) {
            registry.remove(id).unwrap();
        }
        assert!(!registry.has_app(apps[0]));
        assert_eq!(registry.len(), 8_000);
        assert_eq!(registry.app_ids().count(), 4);
        assert_consistent(&registry);
        // Concentrating the remaining connections in one app must remain exact.
        for id in ids
            .iter()
            .filter(|id| registry.contains_key(id))
            .copied()
            .collect::<Vec<_>>()
        {
            registry.attach_app(id, apps[0]).unwrap();
        }
        assert_eq!(registry.app_ids().count(), 1);
        assert_eq!(registry.ids_for_app(apps[0]).count(), 8_000);
        assert_consistent(&registry);
        for id in ids {
            registry.remove(&id);
        }
        assert!(registry.apps.is_empty());
        assert!(registry.is_empty());
    }
}
