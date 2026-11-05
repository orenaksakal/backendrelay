use std::collections::{BTreeMap, BTreeSet, HashMap};

use instant_datalog::InvalidationTopic;
use instant_wal::{Isn, Topic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{QueryTopic, topics_intersect};

/// Metadata retained for one connected client.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Session {
    pub id: Uuid,
    pub app_id: Option<Uuid>,
    #[serde(default)]
    pub auth: Value,
    #[serde(default)]
    pub creator: Value,
    #[serde(default)]
    pub versions: BTreeMap<String, String>,
    pub attrs_hash: Option<u64>,
    #[serde(default)]
    pub inference: bool,
}

impl Session {
    #[must_use]
    pub fn new(id: Uuid) -> Self {
        Self {
            id,
            app_id: None,
            auth: Value::Null,
            creator: Value::Null,
            versions: BTreeMap::new(),
            attrs_hash: None,
            inference: false,
        }
    }
}

/// Result envelope requested by the client query.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReturnType {
    Tree,
    JoinRows,
}

/// Public state for a live `InstaQL` query.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct QueryRecord {
    pub session_id: Uuid,
    pub query: Value,
    pub stale: bool,
    pub version: u64,
    pub result_hash: Option<u64>,
    pub return_type: ReturnType,
}

/// Context carried while an `InstaQL` execution records its Datalog dependencies.
#[derive(Clone, Debug)]
pub struct SubscriptionContext<'a> {
    pub app_id: Uuid,
    pub session_id: Uuid,
    pub instaql_query: &'a Value,
    pub version: u64,
}

/// Outcome of storing a completed `InstaQL` result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryCompletion {
    pub query_present: bool,
    pub result_changed: bool,
}

/// Queries and sessions affected by one WAL invalidation batch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InvalidationOutcome {
    pub invalidated_datalog_queries: usize,
    pub stale_sessions: BTreeSet<Uuid>,
}

/// Atomic snapshot of stale queries claimed for one refresh pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RefreshPlan {
    pub processed_tx_id: Option<i64>,
    pub processed_isn: Option<Isn>,
    pub queries: Vec<QueryRecord>,
}

/// In-memory state shared by realtime connections.
#[derive(Debug, Default)]
pub struct RealtimeStore {
    sessions: HashMap<Uuid, Session>,
    apps: HashMap<Uuid, AppState>,
}

#[derive(Debug, Default)]
struct AppState {
    processed_tx_id: Option<i64>,
    processed_isn: Option<Isn>,
    queries: HashMap<QueryKey, QueryRecord>,
    datalog_queries: HashMap<String, DatalogQuery>,
    subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct QueryKey {
    session_id: Uuid,
    canonical_query: String,
}

#[derive(Debug)]
struct DatalogQuery {
    topics: Option<Vec<QueryTopic>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Subscription {
    session_id: Uuid,
    instaql_key: QueryKey,
    instaql_version: u64,
    datalog_key: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RealtimeError {
    #[error("session {0} is not registered")]
    SessionNotFound(Uuid),
    #[error("session {session_id} is not attached to app {app_id}")]
    SessionAppMismatch { session_id: Uuid, app_id: Uuid },
    #[error("InstaQL query is not registered for session {0}")]
    QueryNotFound(Uuid),
}

impl RealtimeStore {
    /// Inserts or replaces the metadata for a client session.
    pub fn upsert_session(&mut self, session: Session) {
        self.sessions.insert(session.id, session);
    }

    #[must_use]
    pub fn session(&self, session_id: Uuid) -> Option<&Session> {
        self.sessions.get(&session_id)
    }

    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    #[must_use]
    pub fn app_ids_with_sessions(&self) -> BTreeSet<Uuid> {
        self.sessions
            .values()
            .filter_map(|session| session.app_id)
            .collect()
    }

    #[must_use]
    pub fn session_ids_for_app(&self, app_id: Uuid) -> BTreeSet<Uuid> {
        self.sessions
            .values()
            .filter(|session| session.app_id == Some(app_id))
            .map(|session| session.id)
            .collect()
    }

    /// Starts a query computation, creating version 1 or bumping the existing version.
    ///
    /// The new version is immediately non-stale, matching the reference store's
    /// `bump-instaql-version!` transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or belongs to another app.
    pub fn begin_query(
        &mut self,
        app_id: Uuid,
        session_id: Uuid,
        query: Value,
        return_type: ReturnType,
    ) -> Result<QueryRecord, RealtimeError> {
        self.ensure_session_app(session_id, app_id)?;
        let key = QueryKey::new(session_id, &query);
        let state = self.apps.entry(app_id).or_default();
        let record = state
            .queries
            .entry(key)
            .and_modify(|record| {
                record.version = record.version.saturating_add(1);
                record.stale = false;
            })
            .or_insert(QueryRecord {
                session_id,
                query,
                stale: false,
                version: 1,
                result_hash: None,
                return_type,
            });
        Ok(record.clone())
    }

    /// Records a result hash and retires dependency subscriptions from older versions.
    pub fn complete_query(
        &mut self,
        app_id: Uuid,
        session_id: Uuid,
        query: &Value,
        version: u64,
        result_hash: Option<u64>,
    ) -> QueryCompletion {
        let key = QueryKey::new(session_id, query);
        let Some(state) = self.apps.get_mut(&app_id) else {
            return QueryCompletion {
                query_present: false,
                result_changed: result_hash.is_none(),
            };
        };
        let Some(current_version) = state.queries.get(&key).map(|record| record.version) else {
            state.clean_orphan_datalog_queries();
            return QueryCompletion {
                query_present: false,
                result_changed: result_hash.is_none(),
            };
        };
        if current_version != version {
            return QueryCompletion {
                query_present: true,
                result_changed: false,
            };
        }
        state.subscriptions.retain(|subscription| {
            subscription.instaql_key != key || subscription.instaql_version >= version
        });
        let Some(record) = state.queries.get_mut(&key) else {
            return QueryCompletion {
                query_present: false,
                result_changed: result_hash.is_none(),
            };
        };
        let previous = record.result_hash;
        record.result_hash = result_hash;
        QueryCompletion {
            query_present: true,
            result_changed: previous != result_hash
                || (previous.is_none() && result_hash.is_none()),
        }
    }

    /// Removes a query and all of its subscriptions.
    pub fn remove_query(&mut self, app_id: Uuid, session_id: Uuid, query: &Value) -> bool {
        let key = QueryKey::new(session_id, query);
        let Some(state) = self.apps.get_mut(&app_id) else {
            return false;
        };
        let removed = state.queries.remove(&key).is_some();
        state
            .subscriptions
            .retain(|subscription| subscription.instaql_key != key);
        state.clean_orphan_datalog_queries();
        removed
    }

    /// Removes all resources owned by a session and drops the app store when its
    /// final session leaves.
    pub fn remove_session(&mut self, session_id: Uuid) -> Option<Session> {
        let session = self.sessions.remove(&session_id)?;
        if let Some(app_id) = session.app_id {
            if let Some(state) = self.apps.get_mut(&app_id) {
                state.queries.retain(|key, _| key.session_id != session_id);
                state
                    .subscriptions
                    .retain(|subscription| subscription.session_id != session_id);
                state.clean_orphan_datalog_queries();
            }
            if !self
                .sessions
                .values()
                .any(|candidate| candidate.app_id == Some(app_id))
            {
                self.apps.remove(&app_id);
            }
        }
        Some(session)
    }

    #[must_use]
    pub fn query(&self, app_id: Uuid, session_id: Uuid, query: &Value) -> Option<&QueryRecord> {
        self.apps
            .get(&app_id)?
            .queries
            .get(&QueryKey::new(session_id, query))
    }

    #[must_use]
    pub fn stale_queries(&self, app_id: Uuid, session_id: Uuid) -> Vec<QueryRecord> {
        self.apps.get(&app_id).map_or_else(Vec::new, |state| {
            let mut queries = state
                .queries
                .values()
                .filter(|query| query.session_id == session_id && query.stale)
                .cloned()
                .collect::<Vec<_>>();
            queries.sort_by_key(|query| canonical_json(&query.query));
            queries
        })
    }

    /// Claims every currently stale query for recomputation.
    ///
    /// Each claimed query receives a new version and becomes non-stale before it
    /// leaves the store. A later invalidation can therefore mark the new execution
    /// stale again without being overwritten by this refresh pass.
    #[must_use]
    pub fn begin_refresh(&mut self, app_id: Uuid, session_id: Uuid) -> RefreshPlan {
        let Some(state) = self.apps.get_mut(&app_id) else {
            return RefreshPlan::default();
        };
        let mut queries = state
            .queries
            .values_mut()
            .filter(|query| query.session_id == session_id && query.stale)
            .map(|query| {
                query.version = query.version.saturating_add(1);
                query.stale = false;
                query.clone()
            })
            .collect::<Vec<_>>();
        queries.sort_by_key(|query| canonical_json(&query.query));
        RefreshPlan {
            processed_tx_id: state.processed_tx_id,
            processed_isn: state.processed_isn,
            queries,
        }
    }

    /// Ensures a shared Datalog query exists and subscribes the current `InstaQL` version.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not belong to the app or the parent
    /// query was removed while its execution was starting.
    pub fn record_datalog_query_start(
        &mut self,
        context: &SubscriptionContext<'_>,
        datalog_query: &Value,
        coarse_topics: &[InvalidationTopic],
    ) -> Result<(), RealtimeError> {
        self.ensure_session_app(context.session_id, context.app_id)?;
        let instaql_key = QueryKey::new(context.session_id, context.instaql_query);
        let state = self.apps.entry(context.app_id).or_default();
        if !state.queries.contains_key(&instaql_key) {
            return Err(RealtimeError::QueryNotFound(context.session_id));
        }
        let datalog_key = canonical_json(datalog_query);
        state
            .datalog_queries
            .entry(datalog_key.clone())
            .or_insert_with(|| DatalogQuery {
                topics: Some(coarse_topics.iter().map(QueryTopic::from).collect()),
            });
        let subscription = Subscription {
            session_id: context.session_id,
            instaql_key,
            instaql_version: context.version,
            datalog_key,
        };
        if !state.subscriptions.contains(&subscription) {
            state.subscriptions.push(subscription);
        }
        Ok(())
    }

    /// Replaces coarse dependencies with the precise topics returned by Datalog.
    /// A completion racing with removal is intentionally ignored.
    pub fn record_datalog_query_finish(
        &mut self,
        app_id: Uuid,
        datalog_query: &Value,
        topics: &[InvalidationTopic],
    ) {
        if let Some(query) = self.apps.get_mut(&app_id).and_then(|state| {
            state
                .datalog_queries
                .get_mut(&canonical_json(datalog_query))
        }) {
            query.topics = Some(topics.iter().map(QueryTopic::from).collect());
        }
    }

    /// Invalidates matching Datalog cache entries and marks every dependent
    /// `InstaQL` query stale. Processed transaction metadata advances monotonically.
    pub fn invalidate(
        &mut self,
        app_id: Uuid,
        tx_id: i64,
        isn: Isn,
        topics: &[Topic],
    ) -> InvalidationOutcome {
        let state = self.apps.entry(app_id).or_default();
        state.processed_tx_id = Some(state.processed_tx_id.map_or(tx_id, |old| old.max(tx_id)));
        state.processed_isn = Some(state.processed_isn.map_or(isn, |old| old.max(isn)));

        let invalidated = state
            .datalog_queries
            .iter()
            .filter(|(_, query)| {
                query
                    .topics
                    .as_deref()
                    .is_some_and(|query_topics| topics_intersect(topics, query_topics))
            })
            .map(|(key, _)| key.clone())
            .collect::<BTreeSet<_>>();
        let mut stale_sessions = BTreeSet::new();
        for subscription in &state.subscriptions {
            if invalidated.contains(&subscription.datalog_key)
                && let Some(query) = state.queries.get_mut(&subscription.instaql_key)
            {
                query.stale = true;
                stale_sessions.insert(subscription.session_id);
            }
        }
        state
            .subscriptions
            .retain(|subscription| !invalidated.contains(&subscription.datalog_key));
        state
            .datalog_queries
            .retain(|key, _| !invalidated.contains(key));
        InvalidationOutcome {
            invalidated_datalog_queries: invalidated.len(),
            stale_sessions,
        }
    }

    #[must_use]
    pub fn processed_tx_id(&self, app_id: Uuid) -> Option<i64> {
        self.apps.get(&app_id)?.processed_tx_id
    }

    #[must_use]
    pub fn processed_isn(&self, app_id: Uuid) -> Option<Isn> {
        self.apps.get(&app_id)?.processed_isn
    }

    fn ensure_session_app(&self, session_id: Uuid, app_id: Uuid) -> Result<(), RealtimeError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(RealtimeError::SessionNotFound(session_id))?;
        if session.app_id != Some(app_id) {
            return Err(RealtimeError::SessionAppMismatch { session_id, app_id });
        }
        Ok(())
    }
}

impl AppState {
    fn clean_orphan_datalog_queries(&mut self) {
        self.datalog_queries.retain(|key, _| {
            self.subscriptions
                .iter()
                .any(|subscription| &subscription.datalog_key == key)
        });
    }
}

impl QueryKey {
    fn new(session_id: Uuid, query: &Value) -> Self {
        Self {
            session_id,
            canonical_query: canonical_json(query),
        }
    }
}

fn canonical_json(value: &Value) -> String {
    fn normalize(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.iter().map(normalize).collect()),
            value => value.clone(),
        }
    }
    serde_json::to_string(&normalize(value)).expect("JSON values are always serializable")
}

#[cfg(test)]
mod tests {
    use instant_datalog::{DatalogValue, IndexKey, TopicPart};
    use instant_wal::{Lsn, MatchSet, TopicValue};
    use serde_json::json;

    use super::*;

    fn session(id: Uuid, app_id: Uuid) -> Session {
        Session {
            app_id: Some(app_id),
            ..Session::new(id)
        }
    }

    fn topic(attr: Uuid, value: i64) -> Topic {
        Topic {
            indexes: vec![IndexKey::Ave],
            entities: MatchSet::Any,
            attributes: MatchSet::Values(vec![attr]),
            values: MatchSet::Values(vec![TopicValue::Json(json!(value))]),
        }
    }

    fn dependency(attr: Uuid) -> InvalidationTopic {
        InvalidationTopic {
            indexes: vec![IndexKey::Ave],
            entity: TopicPart::Any,
            attribute: TopicPart::Values(vec![DatalogValue::Uuid(attr)]),
            value: TopicPart::Any,
        }
    }

    #[test]
    fn query_identity_ignores_object_key_order_and_versions_bump() {
        let app = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let mut store = RealtimeStore::default();
        store.upsert_session(session(session_id, app));
        let first = store
            .begin_query(
                app,
                session_id,
                json!({"users": {"$": {"limit": 1, "where": {"name": "Ada"}}}}),
                ReturnType::Tree,
            )
            .unwrap();
        let second_query = json!({"users": {"$": {"where": {"name": "Ada"}, "limit": 1}}});
        let second = store
            .begin_query(app, session_id, second_query.clone(), ReturnType::Tree)
            .unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(second.version, 2);
        assert_eq!(
            store.query(app, session_id, &second_query).unwrap().version,
            2
        );
    }

    #[test]
    fn completion_deduplicates_results_and_cleans_old_subscriptions() {
        let app = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let query = json!({"users": {}});
        let datalog = json!([["ave", "?e", attr, "?v"]]);
        let mut store = RealtimeStore::default();
        store.upsert_session(session(session_id, app));
        let record = store
            .begin_query(app, session_id, query.clone(), ReturnType::Tree)
            .unwrap();
        store
            .record_datalog_query_start(
                &SubscriptionContext {
                    app_id: app,
                    session_id,
                    instaql_query: &query,
                    version: record.version,
                },
                &datalog,
                &[dependency(attr)],
            )
            .unwrap();
        assert!(
            store
                .complete_query(app, session_id, &query, 1, Some(7))
                .result_changed
        );
        assert!(
            !store
                .complete_query(app, session_id, &query, 1, Some(7))
                .result_changed
        );
        assert!(
            store
                .complete_query(app, session_id, &query, 1, Some(8))
                .result_changed
        );
    }

    #[test]
    fn stale_completion_cannot_overwrite_the_current_result() {
        let app = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let query = json!({"users": {}});
        let mut store = RealtimeStore::default();
        store.upsert_session(session(session_id, app));
        store
            .begin_query(app, session_id, query.clone(), ReturnType::Tree)
            .unwrap();
        store
            .begin_query(app, session_id, query.clone(), ReturnType::Tree)
            .unwrap();

        store.complete_query(app, session_id, &query, 2, Some(20));
        let stale = store.complete_query(app, session_id, &query, 1, Some(10));

        assert!(!stale.result_changed);
        let record = store.query(app, session_id, &query).unwrap();
        assert_eq!(record.version, 2);
        assert_eq!(record.result_hash, Some(20));
    }

    #[test]
    fn matching_invalidation_stales_dependents_and_advances_metadata() {
        let app = Uuid::new_v4();
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let other_attr = Uuid::new_v4();
        let query = json!({"users": {}});
        let datalog = json!([["ave", "?e", attr, "?v"]]);
        let mut store = RealtimeStore::default();
        for session_id in [session_a, session_b] {
            store.upsert_session(session(session_id, app));
            let record = store
                .begin_query(app, session_id, query.clone(), ReturnType::Tree)
                .unwrap();
            store
                .record_datalog_query_start(
                    &SubscriptionContext {
                        app_id: app,
                        session_id,
                        instaql_query: &query,
                        version: record.version,
                    },
                    &datalog,
                    &[dependency(attr)],
                )
                .unwrap();
        }
        let isn = Isn::new(1, Lsn::from_u64(20));
        let outcome = store.invalidate(app, 10, isn, &[topic(other_attr, 1)]);
        assert_eq!(outcome.invalidated_datalog_queries, 0);
        assert!(store.stale_queries(app, session_a).is_empty());

        let outcome = store.invalidate(app, 8, Isn::default(), &[topic(attr, 1)]);
        assert_eq!(outcome.invalidated_datalog_queries, 1);
        assert_eq!(
            outcome.stale_sessions,
            BTreeSet::from([session_a, session_b])
        );
        assert_eq!(store.stale_queries(app, session_a).len(), 1);
        assert_eq!(store.stale_queries(app, session_b).len(), 1);
        assert_eq!(store.processed_tx_id(app), Some(10));
        assert_eq!(store.processed_isn(app), Some(isn));

        let refresh = store.begin_refresh(app, session_a);
        assert_eq!(refresh.processed_tx_id, Some(10));
        assert_eq!(refresh.processed_isn, Some(isn));
        assert_eq!(refresh.queries.len(), 1);
        assert_eq!(refresh.queries[0].version, 2);
        assert!(!refresh.queries[0].stale);
        assert!(store.stale_queries(app, session_a).is_empty());
    }

    #[test]
    fn invalidation_during_refresh_is_not_lost() {
        let app = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let attr = Uuid::new_v4();
        let query = json!({"users": {}});
        let datalog = json!([["ave", "?e", attr, "?v"]]);
        let mut store = RealtimeStore::default();
        store.upsert_session(session(session_id, app));
        let first = store
            .begin_query(app, session_id, query.clone(), ReturnType::Tree)
            .unwrap();
        store
            .record_datalog_query_start(
                &SubscriptionContext {
                    app_id: app,
                    session_id,
                    instaql_query: &query,
                    version: first.version,
                },
                &datalog,
                &[dependency(attr)],
            )
            .unwrap();
        store.invalidate(app, 1, Isn::default(), &[topic(attr, 1)]);
        let refresh = store.begin_refresh(app, session_id);
        assert_eq!(refresh.queries[0].version, 2);

        // The new execution records its dependency before the old refresh finishes.
        store
            .record_datalog_query_start(
                &SubscriptionContext {
                    app_id: app,
                    session_id,
                    instaql_query: &query,
                    version: 2,
                },
                &datalog,
                &[dependency(attr)],
            )
            .unwrap();
        store.invalidate(app, 2, Isn::new(1, Lsn::from_u64(2)), &[topic(attr, 2)]);
        store.complete_query(app, session_id, &query, 2, Some(10));
        assert_eq!(store.stale_queries(app, session_id).len(), 1);
    }

    #[test]
    fn removing_sessions_cleans_queries_and_final_app_state() {
        let app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let query = json!({"users": {}});
        let mut store = RealtimeStore::default();
        store.upsert_session(session(first, app));
        store.upsert_session(session(second, app));
        store
            .begin_query(app, first, query.clone(), ReturnType::Tree)
            .unwrap();
        store.remove_session(first);
        assert!(store.query(app, first, &query).is_none());
        assert_eq!(store.app_ids_with_sessions(), BTreeSet::from([app]));
        store.invalidate(app, 5, Isn::default(), &[]);
        store.remove_session(second);
        assert!(store.processed_tx_id(app).is_none());
    }
}
