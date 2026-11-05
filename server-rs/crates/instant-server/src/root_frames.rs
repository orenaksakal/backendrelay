//! User-independent root frames for the native framework query path.
//!
//! One [`RootFrame`] holds the pre-permission output of a single top-level root
//! after the file-URL transform, plus the permission entity data that root's
//! view/field checks need. Frames contain no authorization decisions, session
//! bindings, shaped response or signed output; every session still evaluates the
//! complete CEL filter itself.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;

use instant_datalog::IndexKey;
use instant_http::AdminActor;
use instant_instaql::{CompiledQuery, ExecutedNode};
use instant_permissions::{
    PermissionEntityKey, PermissionEntityLoad, PermissionError, RuleSet,
    query_permission_entity_loads,
};
use instant_realtime::{QueryTopic, QueryTopicPart, QueryValue, topics_intersect};
use instant_wal::{MatchSet, Topic};
use serde_json::Value;
use uuid::Uuid;

const ALL_INDEXES: [IndexKey; 5] = [
    IndexKey::Ea,
    IndexKey::Eav,
    IndexKey::Av,
    IndexKey::Ave,
    IndexKey::Vae,
];

/// One top-level root's pre-permission executed nodes and permission entities.
#[derive(Clone, Debug, Default)]
pub(crate) struct RootFrame {
    /// The root's executed node(s), already passed through the file transform.
    pub(crate) nodes: Vec<ExecutedNode>,
    /// Entity records required by this root's permission checks.
    pub(crate) entity_data: HashMap<PermissionEntityKey, Value>,
    /// WAL topics this frame depends on, or `None` when they could not be
    /// computed. `None` retires the frame on every data WAL.
    pub(crate) topics: Option<FrameTopics>,
}

/// A frame's invalidation topics plus a cheap summary used to reject unrelated
/// WALs before the linear `topics_intersect` scan.
#[derive(Clone, Debug)]
pub(crate) struct FrameTopics {
    topics: Vec<QueryTopic>,
    summary: TopicSummary,
}

/// Which UUIDs a frame's topics can share with a WAL topic.
///
/// `any` means at least one topic has neither a `Values` attribute part nor a
/// `Values` entity part, so no UUID pre-filter is sound for it.
#[derive(Clone, Debug, Default)]
struct TopicSummary {
    any: bool,
    attributes: HashSet<Uuid>,
    entities: HashSet<Uuid>,
}

impl FrameTopics {
    pub(crate) fn new(topics: Vec<QueryTopic>) -> Self {
        let mut summary = TopicSummary::default();
        for topic in &topics {
            // Soundness follows `match_uuid_part`: an invalidation's UUID set can
            // match a query `Values` part only by containing one of its UUIDs.
            // A matching query topic with `Values` attributes therefore shares
            // an attribute UUID; one with non-`Values` attributes but `Values`
            // entities shares an entity UUID. Everything else (Any, Not,
            // StartsWith, Comparator, or `Values` entries that are not UUIDs)
            // can match without a shared UUID and sets `any`.
            if let QueryTopicPart::Values(values) = &topic.attributes {
                summary
                    .attributes
                    .extend(values.iter().filter_map(|value| match value {
                        QueryValue::Uuid(value) => Some(*value),
                        QueryValue::Json(_) => None,
                    }));
            } else if let QueryTopicPart::Values(values) = &topic.entities {
                summary
                    .entities
                    .extend(values.iter().filter_map(|value| match value {
                        QueryValue::Uuid(value) => Some(*value),
                        QueryValue::Json(_) => None,
                    }));
            } else {
                summary.any = true;
            }
        }
        Self { topics, summary }
    }

    /// Whether an invalidation might match before running the full comparison.
    ///
    /// Rejecting here is sound because a real intersection must either use a
    /// wildcard UUID part (Any), share an attribute UUID from a query `Values`
    /// attribute part, or share an entity UUID from a query `Values` entity part.
    fn could_intersect(&self, wal: &WalTopics<'_>) -> bool {
        self.summary.any
            || wal.any_attribute
            || wal.any_entity
            || shares_uuid(&wal.attributes, &self.summary.attributes)
            || shares_uuid(&wal.entities, &self.summary.entities)
    }
}

/// One WAL's topics with their attribute and entity UUIDs collected once.
///
/// A retirement scan checks every cached frame of the app against the same WAL.
/// The raw topics repeat a few UUIDs once per changed triple and index, so the
/// pre-filter dedupes them here instead of once per frame.
pub(crate) struct WalTopics<'a> {
    topics: &'a [Topic],
    any_attribute: bool,
    any_entity: bool,
    /// Sorted and deduplicated.
    attributes: Vec<Uuid>,
    /// Sorted and deduplicated.
    entities: Vec<Uuid>,
}

impl<'a> WalTopics<'a> {
    pub(crate) fn new(topics: &'a [Topic]) -> Self {
        let mut wal = Self {
            topics,
            any_attribute: false,
            any_entity: false,
            attributes: Vec::new(),
            entities: Vec::new(),
        };
        for topic in topics {
            match &topic.attributes {
                MatchSet::Any => wal.any_attribute = true,
                MatchSet::Values(values) => wal.attributes.extend(values),
            }
            match &topic.entities {
                MatchSet::Any => wal.any_entity = true,
                MatchSet::Values(values) => wal.entities.extend(values),
            }
        }
        for ids in [&mut wal.attributes, &mut wal.entities] {
            ids.sort_unstable();
            ids.dedup();
        }
        wal
    }
}

fn shares_uuid(wal: &[Uuid], known: &HashSet<Uuid>) -> bool {
    if known.len() < wal.len() {
        known.iter().any(|value| wal.binary_search(value).is_ok())
    } else {
        wal.iter().any(|value| known.contains(value))
    }
}

impl RootFrame {
    /// Whether a data WAL carrying `invalidations` can change this frame.
    ///
    /// Conservative by construction: an uncomputable topic set always retires.
    #[must_use]
    pub(crate) fn wal_intersects(&self, invalidations: &[instant_wal::Topic]) -> bool {
        self.wal_intersects_with(&WalTopics::new(invalidations))
    }

    /// [`Self::wal_intersects`] for a WAL prepared once per retirement scan.
    #[must_use]
    pub(crate) fn wal_intersects_with(&self, wal: &WalTopics<'_>) -> bool {
        self.topics.as_ref().is_none_or(|frame| {
            frame.could_intersect(wal) && topics_intersect(wal.topics, &frame.topics)
        })
    }

    /// Concatenates the frames in root order and unions their entity data.
    #[must_use]
    pub(crate) fn assemble(
        frames: Vec<Self>,
    ) -> (Vec<ExecutedNode>, HashMap<PermissionEntityKey, Value>) {
        let mut nodes = Vec::new();
        let mut entity_data = HashMap::new();
        for frame in frames {
            nodes.extend(frame.nodes);
            entity_data.extend(frame.entity_data);
        }
        (nodes, entity_data)
    }
}

/// Invalidation topics for one cached root frame.
///
/// Topics come from the frame's UNFILTERED executed node, exactly the precise
/// set the session path stores in `InstantQueryTopics`. Entity-level topics
/// cover every permission entity the frame carries: a rule can read an attribute
/// the projected query never selected, and any change to that entity can change
/// a later decision.
///
/// Coarse topics are deliberately not unioned in. They drop bound entity and
/// value terms (for example a root's id selector becomes `any entity`), which
/// would retire every room on any insert. Instead, a frame with no precise
/// topics (for example an empty result whose variables never bound) returns
/// `None` and is retired by every data WAL.
#[must_use]
pub(crate) fn invalidation_topics(
    compiled: &CompiledQuery,
    executed: &[ExecutedNode],
    entity_data: &HashMap<PermissionEntityKey, Value>,
) -> Option<FrameTopics> {
    let topics = instant_instaql::collect_query_invalidation_topics(compiled, executed).ok()?;
    let mut combined = Vec::with_capacity(topics.len() + entity_data.len());
    for topic in &topics {
        let topic = QueryTopic::from(topic);
        if !combined.contains(&topic) {
            combined.push(topic);
        }
    }
    for key in entity_data.keys() {
        let topic = QueryTopic {
            indexes: ALL_INDEXES.to_vec(),
            entities: QueryTopicPart::Values(vec![QueryValue::Uuid(key.entity_id)]),
            attributes: QueryTopicPart::Any,
            values: QueryTopicPart::Any,
        };
        if !combined.contains(&topic) {
            combined.push(topic);
        }
    }
    (!combined.is_empty()).then(|| FrameTopics::new(combined))
}

/// Whether the frame path may run before any SQL is executed.
///
/// The check is deliberately conservative: an unprovable rule or field program
/// keeps the existing path (and its error timing). BYOP queries already returned
/// before this point.
#[must_use]
pub(crate) fn eligible(
    native_only: bool,
    actor: &AdminActor,
    rate_limits_empty: bool,
    executes_sql: bool,
    rules: &RuleSet,
    compiled: &CompiledQuery,
) -> bool {
    native_only
        && !matches!(actor, AdminActor::Admin)
        && rate_limits_empty
        && executes_sql
        && instant_permissions::query_programs_have_no_database_callbacks(rules, compiled)
}

/// Splits an executed query into one frame per root in root order.
///
/// All roots were already executed by the caller in one statement inside the
/// open snapshot. This splits the transformed result, computes each root's
/// entity loads from a single-root compiled query, and loads the union once with
/// the existing batched loader.
pub(crate) async fn load<F, Fut>(
    rules: &RuleSet,
    compiled: &CompiledQuery,
    transformed: Vec<ExecutedNode>,
    catalog: &instant_instaql::AttributeCatalog,
    project_attributes: bool,
    load_entities: F,
) -> Result<Vec<RootFrame>, PermissionError>
where
    F: FnOnce(Vec<PermissionEntityLoad>) -> Fut,
    Fut: Future<Output = Result<HashMap<PermissionEntityKey, Value>, PermissionError>>,
{
    if compiled.roots.len() != transformed.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: compiled.roots.len(),
            actual: transformed.len(),
        });
    }
    let mut frames = Vec::with_capacity(compiled.roots.len());
    let mut per_frame_loads = Vec::with_capacity(compiled.roots.len());
    let mut all_loads = Vec::new();
    for (root, node) in compiled.roots.iter().zip(transformed) {
        let single_root = CompiledQuery {
            roots: vec![root.clone()],
        };
        let loads = query_permission_entity_loads(
            rules,
            &single_root,
            std::slice::from_ref(&node),
            catalog,
            project_attributes,
        )?;
        all_loads.extend(loads.iter().cloned());
        per_frame_loads.push(loads);
        frames.push(RootFrame {
            nodes: vec![node],
            entity_data: HashMap::new(),
            // The batch path is never cached; topics are recorded by the
            // per-root shared loader only.
            topics: None,
        });
    }
    let loaded = load_entities(merge_entity_loads(all_loads)).await?;
    for (frame, loads) in frames.iter_mut().zip(per_frame_loads) {
        for load in loads {
            for key in load.entities {
                let value = loaded
                    .get(&key)
                    .ok_or_else(|| PermissionError::MissingEntityData(key.clone()))?;
                frame.entity_data.insert(key, value.clone());
            }
        }
    }
    Ok(frames)
}

/// Loads one root's permission entity data around an already executed node.
///
/// The shared frame path calls this once per root against that root's own
/// single-root compiled query. No two roots' entity data is ever merged here.
pub(crate) async fn frame_from_node<F, Fut>(
    rules: &RuleSet,
    single_root: &CompiledQuery,
    node: ExecutedNode,
    catalog: &instant_instaql::AttributeCatalog,
    project_attributes: bool,
    load_entities: F,
) -> Result<RootFrame, PermissionError>
where
    F: FnOnce(Vec<PermissionEntityLoad>) -> Fut,
    Fut: Future<Output = Result<HashMap<PermissionEntityKey, Value>, PermissionError>>,
{
    let loads = query_permission_entity_loads(
        rules,
        single_root,
        std::slice::from_ref(&node),
        catalog,
        project_attributes,
    )?;
    let loaded = load_entities(merge_entity_loads(loads)).await?;
    Ok(RootFrame {
        nodes: vec![node],
        entity_data: loaded,
        topics: None,
    })
}

/// Merges per-root entity loads into one batch that is a superset of every
/// root's needs.
///
/// A key needed by more than one root, or by a complete load anywhere, is loaded
/// complete so no root's CEL input is narrowed by another root's projection.
pub(crate) fn merge_entity_loads(
    loads: impl IntoIterator<Item = PermissionEntityLoad>,
) -> Vec<PermissionEntityLoad> {
    let loads = loads.into_iter().collect::<Vec<_>>();
    let mut occurrences = HashMap::<PermissionEntityKey, usize>::new();
    for load in &loads {
        for key in &load.entities {
            *occurrences.entry(key.clone()).or_default() += 1;
        }
    }
    let mut complete = Vec::new();
    let mut projected = BTreeMap::<Vec<Uuid>, Vec<PermissionEntityKey>>::new();
    for load in loads {
        let PermissionEntityLoad {
            entities,
            attributes,
        } = load;
        for key in entities {
            let repeated = occurrences.get(&key).copied().unwrap_or(0) > 1;
            match (attributes.as_ref(), repeated) {
                (Some(attributes), false) => {
                    projected.entry(attributes.clone()).or_default().push(key)
                }
                _ => complete.push(key),
            }
        }
    }
    complete.sort();
    complete.dedup();
    let mut merged = Vec::with_capacity(1 + projected.len());
    if !complete.is_empty() {
        merged.push(PermissionEntityLoad {
            entities: complete,
            attributes: None,
        });
    }
    merged.extend(projected.into_iter().map(|(attributes, mut entities)| {
        entities.sort();
        entities.dedup();
        PermissionEntityLoad {
            entities,
            attributes: Some(attributes),
        }
    }));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_datalog::DataType;
    use instant_instaql::{
        Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query,
    };
    use instant_realtime::{Comparator, ComparatorKind};
    use instant_wal::TopicValue;
    use serde_json::json;

    fn attribute(id: u128, etype: &str, label: &str, reference: bool) -> Attribute {
        Attribute {
            id: Uuid::from_u128(id),
            wire: None,
            etype: etype.to_owned(),
            label: label.to_owned(),
            value_type: if reference {
                AttributeValueType::Ref
            } else {
                AttributeValueType::Blob
            },
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed: label == "id",
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: reference.then(|| "owners".to_owned()),
            reverse_label: reference.then(|| "posts".to_owned()),
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    fn catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![
                attribute(1, "posts", "id", false),
                attribute(2, "posts", "title", false),
                attribute(3, "posts", "secret", false),
                attribute(4, "posts", "ownerId", false),
                attribute(5, "posts", "owner", true),
                attribute(6, "comments", "id", false),
            ],
        }
    }

    fn compiled() -> CompiledQuery {
        compile_query(&json!({"posts": {}}), &catalog(), false).unwrap()
    }

    #[test]
    fn eligible_requires_every_frame_precondition() {
        let compiled = compiled();
        let pure = RuleSet::new(json!({"posts": {"allow": {"view": "auth.id == data.ownerId"}}}));
        assert!(eligible(
            true,
            &AdminActor::Guest,
            true,
            true,
            &pure,
            &compiled
        ));
        assert!(eligible(
            true,
            &AdminActor::RefreshToken(Uuid::new_v4()),
            true,
            true,
            &pure,
            &compiled
        ));
        assert!(!eligible(
            false,
            &AdminActor::Guest,
            true,
            true,
            &pure,
            &compiled
        ));
        assert!(!eligible(
            true,
            &AdminActor::Admin,
            true,
            true,
            &pure,
            &compiled
        ));
        assert!(!eligible(
            true,
            &AdminActor::Guest,
            false,
            true,
            &pure,
            &compiled
        ));
        assert!(!eligible(
            true,
            &AdminActor::Guest,
            true,
            false,
            &pure,
            &compiled
        ));
        let mut missing = compiled.clone();
        missing.roots[0].missing = true;
        assert!(!eligible(
            true,
            &AdminActor::Guest,
            true,
            false,
            &pure,
            &missing
        ));
    }

    #[test]
    fn callback_rules_and_fields_keep_the_existing_path() {
        let compiled = compiled();
        for rules in [
            json!({"posts": {"allow": {"view": "data.ref('owner.id') == []"}}}),
            json!({"posts": {"allow": {"view": "true || data.ref('owner.id') == []"}}}),
            json!({"posts": {"allow": {"view": "true"}, "fields": {"secret": "rateLimit.bucket.limit(auth.id)"}}}),
            // Even an unfetched field is classified conservatively.
            json!({"posts": {"allow": {"view": "true"}, "fields": {"unfetched": "data.ref('owner.id') == []"}}}),
        ] {
            let rules = RuleSet::new(rules);
            assert!(!eligible(
                true,
                &AdminActor::Guest,
                true,
                true,
                &rules,
                &compiled
            ));
        }
    }

    #[test]
    fn rules_for_unqueried_etypes_and_malformed_rules_are_handled() {
        let compiled = compiled();
        let other_etype = RuleSet::new(json!({
            "posts": {"allow": {"view": "true"}},
            "comments": {"allow": {"view": "data.ref('post.id') == []"}}
        }));
        assert!(eligible(
            true,
            &AdminActor::Guest,
            true,
            true,
            &other_etype,
            &compiled
        ));
        let malformed_field = RuleSet::new(json!({
            "posts": {"allow": {"view": "true"}, "fields": {"secret": "auth."}}
        }));
        assert!(!eligible(
            true,
            &AdminActor::Guest,
            true,
            true,
            &malformed_field,
            &compiled
        ));
    }

    #[test]
    fn merged_loads_keep_single_root_projections_and_promote_shared_entities() {
        let key = |etype: &str, id: u128| PermissionEntityKey {
            etype: etype.to_owned(),
            entity_id: Uuid::from_u128(id),
        };
        let merged = merge_entity_loads([
            PermissionEntityLoad {
                entities: vec![key("posts", 1), key("posts", 2)],
                attributes: Some(vec![Uuid::from_u128(1)]),
            },
            PermissionEntityLoad {
                entities: vec![key("posts", 2), key("posts", 3)],
                attributes: Some(vec![Uuid::from_u128(2)]),
            },
            PermissionEntityLoad {
                entities: vec![key("posts", 4)],
                attributes: None,
            },
        ]);
        assert_eq!(
            merged,
            vec![
                PermissionEntityLoad {
                    entities: vec![key("posts", 2), key("posts", 4)],
                    attributes: None,
                },
                PermissionEntityLoad {
                    entities: vec![key("posts", 1)],
                    attributes: Some(vec![Uuid::from_u128(1)]),
                },
                PermissionEntityLoad {
                    entities: vec![key("posts", 3)],
                    attributes: Some(vec![Uuid::from_u128(2)]),
                },
            ]
        );
    }

    fn frame(topics: Vec<QueryTopic>) -> RootFrame {
        RootFrame {
            topics: Some(FrameTopics::new(topics)),
            ..RootFrame::default()
        }
    }

    fn wal_topic(
        indexes: Vec<IndexKey>,
        entities: MatchSet<Uuid>,
        attributes: MatchSet<Uuid>,
    ) -> Topic {
        Topic {
            indexes,
            entities,
            attributes,
            values: MatchSet::Any,
        }
    }

    fn topic(entities: QueryTopicPart, attributes: QueryTopicPart) -> QueryTopic {
        QueryTopic {
            indexes: vec![IndexKey::Ea],
            entities,
            attributes,
            values: QueryTopicPart::Any,
        }
    }

    #[test]
    fn topic_prefilter_keeps_explicit_intersections() {
        let attribute = Uuid::from_u128(1);
        let entity = Uuid::from_u128(2);
        let other = Uuid::from_u128(3);
        let query_uuid = |value| QueryTopicPart::Values(vec![QueryValue::Uuid(value)]);
        let wal_uuid = |value| MatchSet::Values(vec![value]);

        // Attribute-level dependency: only a shared attribute UUID intersects.
        let attribute_frame = frame(vec![topic(QueryTopicPart::Any, query_uuid(attribute))]);
        assert!(attribute_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            MatchSet::Any,
            wal_uuid(attribute),
        )]));
        assert!(!attribute_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            MatchSet::Any,
            wal_uuid(other),
        )]));

        // Entity-level permission topic: a shared entity UUID intersects even
        // though the frame records `Any` attributes for that entity.
        let entity_frame = frame(vec![topic(query_uuid(entity), QueryTopicPart::Any)]);
        assert!(entity_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            wal_uuid(entity),
            wal_uuid(other),
        )]));

        // A wildcard WAL topic always passes the pre-filter.
        assert!(entity_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            MatchSet::Any,
            MatchSet::Any,
        )]));

        // A frame topic with `Any` on both UUID parts is summarized as `any`.
        let any_frame = frame(vec![topic(QueryTopicPart::Any, QueryTopicPart::Any)]);
        assert!(any_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            wal_uuid(other),
            wal_uuid(other),
        )]));

        // Non-`Values` query parts (Not, StartsWith, Comparator) rely on the
        // other UUID part being a plain `Values` set for the pre-filter.
        let not_entity_frame = frame(vec![topic(
            QueryTopicPart::Not(QueryValue::Uuid(other)),
            query_uuid(attribute),
        )]);
        assert!(not_entity_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            MatchSet::Any,
            wal_uuid(attribute),
        )]));
        let starts_with_frame = frame(vec![topic(
            QueryTopicPart::StartsWith(entity.to_string()),
            query_uuid(attribute),
        )]);
        assert!(starts_with_frame.wal_intersects(&[wal_topic(
            vec![IndexKey::Ea],
            wal_uuid(entity),
            wal_uuid(attribute),
        )]));
    }

    struct TestRng(u64);

    impl TestRng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, limit: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(limit).unwrap()).unwrap()
        }

        fn uuid(&mut self) -> Uuid {
            Uuid::from_u128(1 + u128::from(self.next() % 6))
        }

        fn indexes(&mut self) -> Vec<IndexKey> {
            const ALL: [IndexKey; 5] = [
                IndexKey::Ea,
                IndexKey::Eav,
                IndexKey::Av,
                IndexKey::Ave,
                IndexKey::Vae,
            ];
            let mut indexes = Vec::new();
            for _ in 0..=self.below(ALL.len()) {
                let index = ALL[self.below(ALL.len())];
                if !indexes.contains(&index) {
                    indexes.push(index);
                }
            }
            indexes
        }

        fn query_part(&mut self) -> QueryTopicPart {
            match self.below(10) {
                0 => QueryTopicPart::Any,
                1 => QueryTopicPart::Not(QueryValue::Uuid(self.uuid())),
                2 => QueryTopicPart::StartsWith(self.uuid().to_string()[..4].to_owned()),
                3 => QueryTopicPart::Comparator(Comparator {
                    kind: ComparatorKind::GreaterThan,
                    data_type: DataType::Number,
                    value: json!(3),
                }),
                _ => QueryTopicPart::Values(
                    (0..self.below(3))
                        .map(|_| {
                            if self.below(5) == 0 {
                                QueryValue::Json(json!("not-a-uuid"))
                            } else {
                                QueryValue::Uuid(self.uuid())
                            }
                        })
                        .collect(),
                ),
            }
        }

        fn wal_uuids(&mut self) -> MatchSet<Uuid> {
            if self.below(4) == 0 {
                MatchSet::Any
            } else {
                MatchSet::Values((0..self.below(3)).map(|_| self.uuid()).collect())
            }
        }

        fn wal_values(&mut self) -> MatchSet<TopicValue> {
            if self.below(3) == 0 {
                MatchSet::Any
            } else {
                MatchSet::Values(
                    (0..self.below(3))
                        .map(|_| {
                            if self.below(2) == 0 {
                                TopicValue::Json(json!(self.below(4)))
                            } else {
                                TopicValue::Reference(self.uuid())
                            }
                        })
                        .collect(),
                )
            }
        }

        fn frame_topics(&mut self) -> Vec<QueryTopic> {
            (0..1 + self.below(3))
                .map(|_| QueryTopic {
                    indexes: self.indexes(),
                    entities: self.query_part(),
                    attributes: self.query_part(),
                    values: self.query_part(),
                })
                .collect()
        }

        fn wal_topics(&mut self) -> Vec<Topic> {
            (0..1 + self.below(3))
                .map(|_| Topic {
                    indexes: self.indexes(),
                    entities: self.wal_uuids(),
                    attributes: self.wal_uuids(),
                    values: self.wal_values(),
                })
                .collect()
        }
    }

    #[test]
    fn topic_prefilter_never_rejects_a_real_intersection() {
        let mut rng = TestRng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..4000 {
            let frame_topics = rng.frame_topics();
            let wal = rng.wal_topics();
            let expected = topics_intersect(&wal, &frame_topics);
            let frame = frame(frame_topics.clone());
            if !frame
                .topics
                .as_ref()
                .unwrap()
                .could_intersect(&WalTopics::new(&wal))
            {
                assert!(
                    !expected,
                    "pre-filter rejected a real intersection: frame={frame_topics:?} wal={wal:?}"
                );
            }
            assert_eq!(
                frame.wal_intersects(&wal),
                expected,
                "pre-filtered result diverged: frame={frame_topics:?} wal={wal:?}"
            );
        }
    }
}
