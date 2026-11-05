use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use instant_wal::{MatchSet, Topic, TopicValue};
use serde_json::Value;
use uuid::Uuid;

use crate::{QueryTopic, QueryTopicPart, QueryValue};

// Bound reverse-index expansion; large or complex dependencies use the slow path.
const MAX_ROUTES_PER_SESSION: usize = 256;
const MAX_INDEXED_STRING_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Route {
    Fallback,
    Entity(Uuid),
    Attribute(Uuid),
    Value(ValueKey),
    AttributeValue(Uuid, ValueKey),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ValueKey {
    Reference(Uuid),
    String(String),
    Number(u64),
    Boolean(bool),
    Null,
    Date(DateTime<Utc>),
}

#[derive(Default)]
struct AppIndex {
    sessions: BTreeMap<Uuid, BTreeSet<Route>>,
    routes: BTreeMap<Route, BTreeSet<Uuid>>,
}

/// Conservative candidate routing for session query dependencies.
///
/// Candidates must still pass `topics_intersect` and any entity predicates.
/// Finite entity, attribute and scalar value sets are indexed; unknown dependencies
/// and large sets fall back to scanning. No results or authorization are cached here.
#[derive(Default)]
pub struct SessionTopicIndex {
    apps: BTreeMap<Uuid, AppIndex>,
}

impl SessionTopicIndex {
    /// Atomically replaces every dependency owned by one session.
    /// An empty iterator removes the session; `None` is an unknown dependency.
    pub fn replace<'a>(
        &mut self,
        app_id: Uuid,
        session_id: Uuid,
        queries: impl IntoIterator<Item = Option<&'a [QueryTopic]>>,
    ) {
        let mut routes = BTreeSet::new();
        let mut has_queries = false;
        for topics in queries {
            has_queries = true;
            let Some(topics) = topics else {
                routes = BTreeSet::from([Route::Fallback]);
                break;
            };
            for topic in topics {
                if !add_routes(&mut routes, topic) || routes.len() > MAX_ROUTES_PER_SESSION {
                    routes = BTreeSet::from([Route::Fallback]);
                    break;
                }
            }
            if routes.contains(&Route::Fallback) {
                break;
            }
        }
        if !has_queries {
            self.remove(app_id, session_id);
            return;
        }
        let app = self.apps.entry(app_id).or_default();
        if app.sessions.get(&session_id) == Some(&routes) {
            // Most refreshes update values while retaining identical topic
            // dependencies. Do not tear down and reallocate every route bucket.
            return;
        }
        let previous = app
            .sessions
            .insert(session_id, routes.clone())
            .unwrap_or_default();
        for route in previous.difference(&routes) {
            if let Some(sessions) = app.routes.get_mut(route) {
                sessions.remove(&session_id);
                if sessions.is_empty() {
                    app.routes.remove(route);
                }
            }
        }
        for route in routes.difference(&previous) {
            app.routes
                .entry(route.clone())
                .or_default()
                .insert(session_id);
        }
    }

    /// Removes owned routes, including empty buckets and empty app indexes.
    pub fn remove(&mut self, app_id: Uuid, session_id: Uuid) {
        let Some(app) = self.apps.get_mut(&app_id) else {
            return;
        };
        if let Some(routes) = app.sessions.remove(&session_id) {
            for route in routes {
                if let Some(sessions) = app.routes.get_mut(&route) {
                    sessions.remove(&session_id);
                    if sessions.is_empty() {
                        app.routes.remove(&route);
                    }
                }
            }
        }
        if app.sessions.is_empty() {
            self.apps.remove(&app_id);
        }
    }

    /// Returns a superset of matching sessions scoped to this app.
    /// Wildcard WAL components deliberately bypass the index.
    #[must_use]
    pub fn candidates(&self, app_id: Uuid, topics: &[Topic]) -> BTreeSet<Uuid> {
        let Some(app) = self.apps.get(&app_id) else {
            return BTreeSet::new();
        };
        let mut candidates = app
            .routes
            .get(&Route::Fallback)
            .cloned()
            .unwrap_or_default();
        for topic in topics {
            let (
                MatchSet::Values(entities),
                MatchSet::Values(attributes),
                MatchSet::Values(values),
            ) = (&topic.entities, &topic.attributes, &topic.values)
            else {
                return app.sessions.keys().copied().collect();
            };
            for value in values {
                for key in wal_value_keys(value) {
                    if let Some(sessions) = app.routes.get(&Route::Value(key.clone())) {
                        candidates.extend(sessions);
                    }
                    for attribute in attributes {
                        if let Some(sessions) = app
                            .routes
                            .get(&Route::AttributeValue(*attribute, key.clone()))
                        {
                            candidates.extend(sessions);
                        }
                    }
                }
            }
            for route in entities
                .iter()
                .copied()
                .map(Route::Entity)
                .chain(attributes.iter().copied().map(Route::Attribute))
            {
                if let Some(sessions) = app.routes.get(&route) {
                    candidates.extend(sessions);
                }
            }
        }
        candidates
    }
}

fn finite_uuids(part: &QueryTopicPart) -> Option<&[QueryValue]> {
    match part {
        QueryTopicPart::Values(values)
            if values.len() <= MAX_ROUTES_PER_SESSION
                && values
                    .iter()
                    .all(|value| matches!(value, QueryValue::Uuid(_))) =>
        {
            Some(values)
        }
        _ => None,
    }
}

fn add_routes(routes: &mut BTreeSet<Route>, topic: &QueryTopic) -> bool {
    if let Some(values) = finite_uuids(&topic.entities) {
        add_uuid_routes(routes, values, Route::Entity);
        return true;
    }
    if let Some(keys) = query_value_keys(&topic.values) {
        if let Some(attributes) = finite_uuids(&topic.attributes) {
            if attributes.len().saturating_mul(keys.len()) > MAX_ROUTES_PER_SESSION {
                return false;
            }
            for attribute in attributes {
                if let QueryValue::Uuid(attribute) = attribute {
                    routes.extend(
                        keys.iter()
                            .cloned()
                            .map(|key| Route::AttributeValue(*attribute, key)),
                    );
                }
            }
        } else {
            routes.extend(keys.into_iter().map(Route::Value));
        }
        return true;
    }
    if let Some(values) = finite_uuids(&topic.attributes) {
        add_uuid_routes(routes, values, Route::Attribute);
        return true;
    }
    false
}

fn add_uuid_routes(routes: &mut BTreeSet<Route>, values: &[QueryValue], route: fn(Uuid) -> Route) {
    for value in values {
        if let QueryValue::Uuid(id) = value {
            routes.insert(route(*id));
        }
    }
}

fn json_value_key(value: &Value) -> Option<ValueKey> {
    match value {
        Value::String(value) if value.len() <= MAX_INDEXED_STRING_BYTES => {
            Some(ValueKey::String(value.clone()))
        }
        // Normalize signed zero; rounding large integers can only add candidates.
        Value::Number(value) => value
            .as_f64()
            .map(|value| ValueKey::Number(if value == 0.0 { 0 } else { value.to_bits() })),
        Value::Bool(value) => Some(ValueKey::Boolean(*value)),
        Value::Null => Some(ValueKey::Null),
        Value::String(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

fn query_value_keys(part: &QueryTopicPart) -> Option<Vec<ValueKey>> {
    let QueryTopicPart::Values(values) = part else {
        return None;
    };
    if values.len() > MAX_ROUTES_PER_SESSION {
        return None;
    }
    let mut keys = Vec::new();
    for value in values {
        match value {
            QueryValue::Uuid(value) => keys.push(ValueKey::Reference(*value)),
            QueryValue::Json(value) => {
                keys.push(json_value_key(value)?);
                // Date WAL values compare by instant, including alternate RFC3339
                // spellings. Retain the string route for untyped JSON equality too.
                if let Value::String(value) = value
                    && let Ok(date) = value.parse::<DateTime<Utc>>()
                {
                    keys.push(ValueKey::Date(date));
                }
            }
        }
        if keys.len() > MAX_ROUTES_PER_SESSION {
            return None;
        }
    }
    Some(keys)
}

fn wal_value_keys(value: &TopicValue) -> Vec<ValueKey> {
    match value {
        TopicValue::Reference(value) => vec![ValueKey::Reference(*value)],
        TopicValue::Date(value) => vec![ValueKey::Date(*value)],
        TopicValue::Json(value) => {
            let mut keys = json_value_key(value).into_iter().collect::<Vec<_>>();
            if let Value::String(value) = value
                && let Ok(id) = Uuid::parse_str(value)
            {
                keys.push(ValueKey::Reference(id));
            }
            keys
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Comparator, ComparatorKind, topics_intersect};
    use instant_datalog::{DataType, IndexKey};
    use instant_wal::TopicValue;
    use serde_json::json;

    fn dependency(entity: QueryTopicPart, attribute: QueryTopicPart) -> QueryTopic {
        QueryTopic {
            indexes: vec![IndexKey::Ea],
            entities: entity,
            attributes: attribute,
            values: QueryTopicPart::Any,
        }
    }

    fn write(entity: MatchSet<Uuid>, attribute: MatchSet<Uuid>) -> Topic {
        Topic {
            indexes: vec![IndexKey::Ea],
            entities: entity,
            attributes: attribute,
            values: MatchSet::Values(vec![TopicValue::Json(json!(1))]),
        }
    }

    #[test]
    fn candidate_routing_never_excludes_an_exact_match() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let parts = vec![
            QueryTopicPart::Any,
            QueryTopicPart::Values(vec![]),
            QueryTopicPart::Values(vec![QueryValue::Uuid(first)]),
            QueryTopicPart::Values(vec![QueryValue::Uuid(first), QueryValue::Uuid(second)]),
            QueryTopicPart::Values(vec![QueryValue::Json(json!(first.to_string()))]),
            QueryTopicPart::Not(QueryValue::Uuid(first)),
            QueryTopicPart::StartsWith("0000".into()),
            QueryTopicPart::Comparator(Comparator {
                kind: ComparatorKind::GreaterThan,
                data_type: DataType::Number,
                value: json!(0),
            }),
        ];
        let writes = [
            MatchSet::Any,
            MatchSet::Values(vec![]),
            MatchSet::Values(vec![first]),
            MatchSet::Values(vec![second]),
            MatchSet::Values(vec![first, second]),
        ];
        let app = Uuid::from_u128(10);
        let session = Uuid::from_u128(11);
        let mut index = SessionTopicIndex::default();
        for entity in &parts {
            for attribute in &parts {
                for value in &parts {
                    let mut query = dependency(entity.clone(), attribute.clone());
                    query.values = value.clone();
                    index.replace(app, session, [Some(std::slice::from_ref(&query))]);
                    for entities in &writes {
                        for attributes in &writes {
                            for values in [
                                MatchSet::Any,
                                MatchSet::Values(vec![]),
                                MatchSet::Values(vec![TopicValue::Reference(first)]),
                                MatchSet::Values(vec![TopicValue::Reference(second)]),
                                MatchSet::Values(vec![TopicValue::Json(json!(first.to_string()))]),
                                MatchSet::Values(vec![TopicValue::Json(json!(1))]),
                            ] {
                                let mut topic = write(entities.clone(), attributes.clone());
                                topic.values = values;
                                let topics = [topic];
                                if topics_intersect(&topics, std::slice::from_ref(&query)) {
                                    assert!(
                                        index.candidates(app, &topics).contains(&session),
                                        "lost match: {query:?} / {topics:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn incremental_replacement_matches_full_rebuild_during_dependency_churn() {
        let mut index = SessionTopicIndex::default();
        let mut owned = BTreeMap::<(Uuid, Uuid), Option<Vec<QueryTopic>>>::new();
        for step in 0..256_u128 {
            let app = Uuid::from_u128(1000 + step % 2);
            let session = Uuid::from_u128(10 + step % 7);
            let query = match step % 5 {
                0 => None,
                1 => Some(vec![]),
                _ => Some(vec![dependency(
                    QueryTopicPart::Values(vec![QueryValue::Uuid(Uuid::from_u128(100 + step % 3))]),
                    QueryTopicPart::Values(vec![QueryValue::Uuid(Uuid::from_u128(200))]),
                )]),
            };
            if step % 11 == 0 {
                owned.remove(&(app, session));
                index.replace(app, session, []);
            } else {
                index.replace(app, session, [query.as_deref()]);
                // Repeating a dependency update is intentionally a no-op.
                index.replace(app, session, [query.as_deref()]);
                owned.insert((app, session), query);
            }
            let mut rebuilt = SessionTopicIndex::default();
            for ((app, session), query) in &owned {
                rebuilt.replace(*app, *session, [query.as_deref()]);
            }
            assert_eq!(index.apps.len(), rebuilt.apps.len());
            for (app_id, expected) in &rebuilt.apps {
                assert_eq!(index.apps[app_id].sessions, expected.sessions);
                assert_eq!(index.apps[app_id].routes, expected.routes);
                for entity in [
                    MatchSet::Any,
                    MatchSet::Values(vec![Uuid::from_u128(100)]),
                    MatchSet::Values(vec![]),
                ] {
                    let topics = [write(entity, MatchSet::Values(vec![Uuid::from_u128(200)]))];
                    assert_eq!(
                        index.candidates(*app_id, &topics),
                        rebuilt.candidates(*app_id, &topics)
                    );
                }
            }
        }
    }

    #[test]
    fn routes_private_entities_and_replaces_and_removes_owned_dependencies() {
        let app = Uuid::from_u128(1);
        let other_app = Uuid::from_u128(2);
        let attr = Uuid::from_u128(3);
        let mut index = SessionTopicIndex::default();
        for id in 10..1010 {
            let session = Uuid::from_u128(id);
            let query = dependency(
                QueryTopicPart::Values(vec![QueryValue::Uuid(session)]),
                QueryTopicPart::Values(vec![QueryValue::Uuid(attr)]),
            );
            index.replace(app, session, [Some(std::slice::from_ref(&query))]);
            index.replace(other_app, session, [None]);
        }
        let target = Uuid::from_u128(10);
        let topics = [write(
            MatchSet::Values(vec![target]),
            MatchSet::Values(vec![attr]),
        )];
        assert_eq!(index.candidates(app, &topics), BTreeSet::from([target]));
        index.replace(app, target, [None]);
        assert!(index.candidates(app, &[]).contains(&target));
        index.replace(app, target, [Some([].as_slice())]);
        assert!(index.candidates(app, &topics).is_empty());
        for id in 10..1010 {
            index.remove(app, Uuid::from_u128(id));
        }
        assert!(!index.apps.contains_key(&app));
        assert_eq!(index.candidates(other_app, &topics).len(), 1000);
    }

    #[test]
    fn routes_private_owner_references_without_scanning_the_shared_attribute() {
        let app = Uuid::from_u128(1);
        let attr = Uuid::from_u128(2);
        let mut index = SessionTopicIndex::default();
        for id in 10..1010 {
            let owner = Uuid::from_u128(id);
            let mut query = dependency(
                QueryTopicPart::Any,
                QueryTopicPart::Values(vec![QueryValue::Uuid(attr)]),
            );
            query.values = QueryTopicPart::Values(vec![QueryValue::Uuid(owner)]);
            index.replace(app, owner, [Some(std::slice::from_ref(&query))]);
        }
        let owner = Uuid::from_u128(10);
        let mut topic = write(
            MatchSet::Values(vec![Uuid::from_u128(5000)]),
            MatchSet::Values(vec![attr]),
        );
        for value in [
            TopicValue::Reference(owner),
            TopicValue::Json(json!(owner.to_string())),
        ] {
            topic.values = MatchSet::Values(vec![value]);
            assert_eq!(
                index.candidates(app, std::slice::from_ref(&topic)),
                BTreeSet::from([owner])
            );
        }
    }

    #[test]
    fn routes_json_owner_values_without_scanning_the_shared_attribute() {
        let app = Uuid::from_u128(1);
        let attr = Uuid::from_u128(2);
        let mut index = SessionTopicIndex::default();
        for id in 10..1110 {
            let owner = Uuid::from_u128(id);
            // Blob/string filters reach realtime as JSON, even when the string
            // happens to be a UUID. Match the Datalog-to-realtime conversion.
            let topic = instant_datalog::InvalidationTopic {
                indexes: vec![IndexKey::Ave],
                entity: instant_datalog::TopicPart::Any,
                attribute: instant_datalog::TopicPart::Values(vec![
                    instant_datalog::DatalogValue::Uuid(attr),
                ]),
                value: instant_datalog::TopicPart::Values(vec![
                    instant_datalog::DatalogValue::Json(json!(owner.to_string())),
                ]),
            };
            let query = QueryTopic::from(&topic);
            index.replace(app, owner, [Some(std::slice::from_ref(&query))]);
        }
        let owner = Uuid::from_u128(10);
        let topic = Topic {
            indexes: vec![IndexKey::Ave],
            entities: MatchSet::Values(vec![Uuid::from_u128(5000)]),
            attributes: MatchSet::Values(vec![attr]),
            values: MatchSet::Values(vec![TopicValue::Json(json!(owner.to_string()))]),
        };
        let candidates = index.candidates(app, &[topic]);
        assert_eq!(candidates.len(), 1);
        assert!(candidates.contains(&owner));
    }

    #[test]
    fn scalar_routes_preserve_exact_match_semantics() {
        let app = Uuid::from_u128(1);
        let attr = Uuid::from_u128(2);
        let entity = Uuid::from_u128(3);
        let date = "2026-09-11T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let json_values = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(0.0),
            json!(-0.0),
            json!(1),
            json!(1.0),
            json!(-1),
            json!(u64::MAX),
            json!("private-owner"),
            json!("a".repeat(MAX_INDEXED_STRING_BYTES + 1)),
            json!(entity.to_string()),
            json!("2026-09-11T00:00:00Z"),
            json!("2026-09-10T18:00:00-06:00"),
            json!([1]),
            json!({"a":1}),
        ];
        let mut index = SessionTopicIndex::default();
        let mut queries = Vec::new();
        for (id, value) in json_values.iter().enumerate() {
            let session = Uuid::from_u128(id as u128 + 10);
            let mut query = dependency(
                QueryTopicPart::Any,
                QueryTopicPart::Values(vec![QueryValue::Uuid(attr)]),
            );
            query.values = QueryTopicPart::Values(vec![QueryValue::Json(value.clone())]);
            index.replace(app, session, [Some(std::slice::from_ref(&query))]);
            queries.push((session, query));
        }
        let writes = json_values
            .iter()
            .cloned()
            .map(TopicValue::Json)
            .chain([TopicValue::Date(date), TopicValue::Reference(entity)]);
        for value in writes {
            let mut topic = write(MatchSet::Values(vec![entity]), MatchSet::Values(vec![attr]));
            topic.values = MatchSet::Values(vec![value]);
            let candidates = index.candidates(app, std::slice::from_ref(&topic));
            for (session, query) in &queries {
                if topics_intersect(std::slice::from_ref(&topic), std::slice::from_ref(query)) {
                    assert!(
                        candidates.contains(session),
                        "lost scalar match: {query:?} / {topic:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn scalar_routes_are_attribute_scoped_and_removed_on_replacement() {
        let app = Uuid::from_u128(1);
        let session = Uuid::from_u128(2);
        let attr = Uuid::from_u128(3);
        let unrelated_attr = Uuid::from_u128(4);
        let mut query = dependency(
            QueryTopicPart::Any,
            QueryTopicPart::Values(vec![QueryValue::Uuid(attr)]),
        );
        query.values = QueryTopicPart::Values(vec![QueryValue::Json(json!("owner"))]);
        let mut index = SessionTopicIndex::default();
        index.replace(app, session, [Some(std::slice::from_ref(&query))]);
        let mut topic = write(
            MatchSet::Values(vec![Uuid::from_u128(5)]),
            MatchSet::Values(vec![unrelated_attr]),
        );
        topic.values = MatchSet::Values(vec![TopicValue::Json(json!("owner"))]);
        assert!(
            index
                .candidates(app, std::slice::from_ref(&topic))
                .is_empty()
        );
        topic.attributes = MatchSet::Values(vec![attr]);
        assert!(
            index
                .candidates(app, std::slice::from_ref(&topic))
                .contains(&session)
        );
        query.values = QueryTopicPart::Values(vec![QueryValue::Json(json!("another-owner"))]);
        index.replace(app, session, [Some(std::slice::from_ref(&query))]);
        assert!(index.candidates(app, &[topic]).is_empty());
        index.remove(app, session);
        assert!(index.apps.is_empty());
    }

    #[test]
    fn large_and_unknown_dependencies_fall_back_without_unbounded_index_expansion() {
        let mut index = SessionTopicIndex::default();
        let app = Uuid::from_u128(1);
        let session = Uuid::from_u128(2);
        let queries = (0..1000)
            .map(|id| {
                dependency(
                    QueryTopicPart::Values(vec![QueryValue::Uuid(Uuid::from_u128(id))]),
                    QueryTopicPart::Any,
                )
            })
            .collect::<Vec<_>>();
        index.replace(app, session, [Some(queries.as_slice())]);
        assert_eq!(
            index.apps[&app].sessions[&session],
            BTreeSet::from([Route::Fallback])
        );
        assert_eq!(index.apps[&app].routes.len(), 1);
        let topics = [write(
            MatchSet::Values(vec![Uuid::from_u128(2000)]),
            MatchSet::Values(vec![Uuid::from_u128(2001)]),
        )];
        assert!(index.candidates(app, &topics).contains(&session));
        index.replace(app, session, std::iter::empty());
        assert!(index.apps.is_empty());
    }
}
