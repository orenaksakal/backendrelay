use std::collections::BTreeSet;

use instant_datalog::{DataType, DatalogValue, MatchCteOptions, PatternMetadataNode, Term};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, PlannedNestedGroup,
    collect_query_invalidation_topics, compile_query, plan_nested_query,
};
use instant_realtime::{QueryTopic, SessionTopicIndex, topics_intersect};
use instant_wal::{MatchSet, Topic, TopicValue};
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: [
            (1, "id", None),
            (2, "ownerId", Some(DataType::String)),
            (3, "createdAt", Some(DataType::Number)),
            (4, "title", Some(DataType::String)),
            (5, "completed", Some(DataType::Boolean)),
        ]
        .into_iter()
        .map(|(id, label, checked)| Attribute {
            id: Uuid::from_u128(id),
            wire: None,
            etype: "todos".to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed: matches!(label, "id" | "ownerId" | "createdAt"),
            required: false,
            checked_data_type: checked,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        })
        .collect(),
    }
}

fn owner_query_topics(app: Uuid, owner: Uuid, entities: &[Uuid]) -> Vec<QueryTopic> {
    let compiled = compile_query(
        &json!({"todos":{"$":{"where":{"ownerId":owner.to_string()},"order":{"createdAt":"desc"},"limit":50}}}),
        &catalog(),
        false,
    ).unwrap();
    let plan = plan_nested_query(&compiled, app, &MatchCteOptions::default()).unwrap();
    let mut envelope = Map::new();
    for group in &plan.groups {
        add_group_rows(group, owner, entities, &mut envelope);
    }
    let decoded = instant_pg::decode_planned_instaql_envelope(&plan, &json!([envelope])).unwrap();
    let topics = collect_query_invalidation_topics(&compiled, &decoded).unwrap();
    assert!(topics.iter().any(|topic| matches!(
        &topic.value,
        instant_datalog::TopicPart::Values(values)
            if values == &[DatalogValue::Json(json!(owner.to_string()))]
    )));
    topics.iter().map(QueryTopic::from).collect()
}

fn add_group_rows(
    group: &PlannedNestedGroup,
    owner: Uuid,
    entities: &[Uuid],
    envelope: &mut Map<String, Value>,
) {
    let mut rows = Vec::new();
    for entity in entities {
        // The root joins its owner selector and ordering triple. The child
        // fetch has one row per selected attribute, as returned by PostgreSQL.
        let fetch_attrs = if group.page.is_some() {
            vec![None]
        } else {
            (1..=5).map(Some).collect()
        };
        for fetch_attr in fetch_attrs {
            let mut row = Map::new();
            for metadata in &group.metadata {
                let PatternMetadataNode::Pattern(metadata) = metadata else {
                    panic!("owner query has no OR branches");
                };
                let Term::Constants(attributes) = &metadata.pattern.attribute else {
                    panic!("owner/page/fetch attributes are constant");
                };
                let attr = fetch_attr.map_or_else(
                    || Uuid::parse_str(attributes[0].as_str().unwrap()).unwrap(),
                    Uuid::from_u128,
                );
                let value = match attr.as_u128() {
                    1 => json!(entity.to_string()),
                    2 => json!(owner.to_string()),
                    3 => json!(1_700_000_000_000_u64),
                    4 => json!("todo title"),
                    5 => json!(false),
                    _ => panic!("unexpected attribute"),
                };
                let columns = &metadata.cte_columns;
                row.insert(columns[0].clone(), json!(entity));
                row.insert(columns[1].clone(), json!(attr));
                row.insert(columns[2].clone(), value);
                row.insert(columns[3].clone(), json!(false));
                row.insert(columns[4].clone(), json!(1));
            }
            if let Some(page) = &group.page {
                row.insert(page.fallback_entity_column.clone(), json!(entity));
                row.insert(page.fallback_created_at_column.clone(), json!(1));
            }
            rows.push(Value::Object(row));
        }
    }
    envelope.insert(group.table.clone().unwrap(), Value::Array(rows));
    if let Some(page) = &group.page {
        envelope.insert(page.has_next_table.clone(), json!([{"exists":false}]));
        envelope.insert(page.has_previous_table.clone(), json!([{"exists":false}]));
    }
    if let Some(children) = &group.children {
        for child in &children.groups {
            add_group_rows(child, owner, entities, envelope);
        }
    }
}

fn write(entity: Uuid, attr: u128, value: Value) -> Topic {
    Topic {
        indexes: vec![
            instant_datalog::IndexKey::Ea,
            instant_datalog::IndexKey::Ave,
        ],
        entities: MatchSet::Values(vec![entity]),
        attributes: MatchSet::Values(vec![Uuid::from_u128(attr)]),
        values: MatchSet::Values(vec![TopicValue::Json(value)]),
    }
}

#[test]
fn compiled_paginated_owner_queries_route_only_the_owner_and_observer() {
    let app = Uuid::from_u128(5000);
    let mut index = SessionTopicIndex::default();
    let mut subscriptions = Vec::new();
    for id in 100..1100 {
        let owner = Uuid::from_u128(id);
        let entities = (0..20)
            .map(|row| Uuid::from_u128(100_000 + id * 20 + row))
            .collect::<Vec<_>>();
        let topics = owner_query_topics(app, owner, &entities);
        index.replace(app, owner, [Some(topics.as_slice())]);
        if id % 10 == 0 {
            let observer = Uuid::from_u128(10_000 + id);
            index.replace(app, observer, [Some(topics.as_slice())]);
            subscriptions.push((observer, topics.clone()));
        }
        subscriptions.push((owner, topics));
    }
    assert_eq!(subscriptions.len(), 1100);
    let owner = Uuid::from_u128(100);
    let expected = BTreeSet::from([owner, Uuid::from_u128(10_100)]);
    let created = write(Uuid::from_u128(900_000), 2, json!(owner.to_string()));
    let reordered = write(Uuid::from_u128(102_000), 3, json!(1_700_000_000_001_u64));
    let edited = write(Uuid::from_u128(102_000), 4, json!("updated title"));
    let toggled = write(Uuid::from_u128(102_000), 5, json!(true));
    let deleted_owner = write(Uuid::from_u128(102_000), 2, json!(owner.to_string()));
    for topic in [created, reordered, edited, toggled, deleted_owner] {
        let candidates = index.candidates(app, std::slice::from_ref(&topic));
        assert_eq!(
            candidates.len(),
            2,
            "unexpected candidate count for attribute {:?}",
            topic.attributes
        );
        assert_eq!(candidates, expected);
        let exact = subscriptions
            .iter()
            .filter(|(_, query)| topics_intersect(std::slice::from_ref(&topic), query))
            .map(|(session, _)| *session)
            .collect::<BTreeSet<_>>();
        assert_eq!(candidates, exact);
    }

    let next_owner = Uuid::from_u128(110);
    let mut transferred = write(Uuid::from_u128(102_000), 2, json!(next_owner.to_string()));
    transferred.values = MatchSet::Values(vec![
        TopicValue::Json(json!(owner.to_string())),
        TopicValue::Json(json!(next_owner.to_string())),
    ]);
    let candidates = index.candidates(app, &[transferred]);
    assert_eq!(candidates.len(), 4);
    assert_eq!(
        candidates,
        BTreeSet::from([
            owner,
            Uuid::from_u128(10_100),
            next_owner,
            Uuid::from_u128(10_110)
        ])
    );
}
