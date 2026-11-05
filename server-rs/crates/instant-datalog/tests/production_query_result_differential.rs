use std::collections::{BTreeMap, HashMap};

use instant_datalog::{
    Clause, DatalogResult, DatalogValue, IndexKey, MatchCteOptions, MatchQueryContext,
    PatternMetadataNode, PlannedCte, QueryCell, QueryRow, TopicPart, WhereColumn, WhereExpression,
    WherePredicate, decode_json_query_rows, decode_query_rows, plan_match_query,
    render_match_query,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    app_id: Uuid,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    clauses: Vec<Clause>,
    expected_planned_indexes: Vec<IndexKey>,
    expected_metadata_indexes: Vec<IndexKey>,
    expected_json_uuid_join_count: usize,
    #[serde(default)]
    expected_entity_lookup: bool,
    rows: Vec<FixtureRow>,
    expected: Value,
}

#[derive(Deserialize)]
struct FixtureRow {
    triples: Vec<FixtureTriple>,
}

#[derive(Deserialize)]
struct FixtureTriple {
    entity: Uuid,
    attribute: Uuid,
    value: Value,
    reference: bool,
    created_at: i64,
}

#[test]
fn production_queries_match_pinned_compile_and_result_oracle() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "fixtures/production_query_result_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let clauses = case
            .clauses
            .iter()
            .map(Clause::normalize)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "production-",
                app_id: fixture.app_id,
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &MatchCteOptions::default(),
            },
            &clauses,
        )
        .unwrap();

        let planned_indexes = plan
            .ctes
            .iter()
            .filter_map(|cte| match cte {
                PlannedCte::Match(cte) => index_guard(&cte.predicate),
                PlannedCte::OrGather(_) | PlannedCte::Select(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            planned_indexes, case.expected_planned_indexes,
            "{}",
            case.name
        );
        let metadata_indexes = plan
            .pattern_metadata
            .iter()
            .map(|node| match node {
                PatternMetadataNode::Pattern(metadata) => metadata.pattern.index.key,
                PatternMetadataNode::Or(_) => panic!("fixture is linear"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            metadata_indexes, case.expected_metadata_indexes,
            "{}",
            case.name
        );

        let rendered = render_match_query(&plan).unwrap();
        assert_eq!(
            rendered.sql.matches("json_uuid_to_uuid").count(),
            case.expected_json_uuid_join_count,
            "{}",
            case.name
        );
        assert_eq!(
            rendered.sql.contains("_lookup\""),
            case.expected_entity_lookup,
            "{} entity lookup planning",
            case.name
        );

        let (direct_rows, json_rows) = rows_for_plan(&plan.pattern_metadata, &case.rows);
        let direct = decode_query_rows(&direct_rows, &plan.pattern_metadata).unwrap();
        let batched = decode_json_query_rows(&json_rows, &plan.pattern_metadata).unwrap();
        assert_eq!(direct, batched, "{} direct/batch drift", case.name);
        assert_eq!(canonical_result(&direct), case.expected, "{}", case.name);
    }
}

fn index_guard(predicate: &WherePredicate) -> Option<IndexKey> {
    match predicate {
        WherePredicate::Compare {
            left: WhereExpression::Column(column),
            right: WhereExpression::Literal(Value::Bool(true)),
            ..
        } => match column.column {
            WhereColumn::Index(index) => Some(index),
            _ => None,
        },
        WherePredicate::And(predicates) | WherePredicate::Or(predicates) => {
            predicates.iter().find_map(index_guard)
        }
        WherePredicate::Compare { .. }
        | WherePredicate::In { .. }
        | WherePredicate::Exists { .. }
        | WherePredicate::ExistsProbe { .. }
        | WherePredicate::Truthy(_) => None,
    }
}

fn rows_for_plan(metadata: &[PatternMetadataNode], rows: &[FixtureRow]) -> (Vec<QueryRow>, Value) {
    let metadata = metadata
        .iter()
        .map(|node| match node {
            PatternMetadataNode::Pattern(metadata) => metadata,
            PatternMetadataNode::Or(_) => panic!("fixture is linear"),
        })
        .collect::<Vec<_>>();
    let mut direct = Vec::with_capacity(rows.len());
    let mut batched = Vec::with_capacity(rows.len());
    for row in rows {
        assert_eq!(row.triples.len(), metadata.len());
        let mut cells = BTreeMap::new();
        let mut object = serde_json::Map::new();
        for (triple, pattern) in row.triples.iter().zip(&metadata) {
            let [entity, attribute, value, reference, created_at] = pattern.cte_columns.as_slice()
            else {
                panic!("planner metadata must expose five columns");
            };
            cells.insert(entity.clone(), QueryCell::Uuid(Some(triple.entity)));
            cells.insert(attribute.clone(), QueryCell::Uuid(Some(triple.attribute)));
            cells.insert(value.clone(), QueryCell::Json(Some(triple.value.clone())));
            cells.insert(
                reference.clone(),
                QueryCell::Boolean(Some(triple.reference)),
            );
            cells.insert(
                created_at.clone(),
                QueryCell::Integer(Some(triple.created_at)),
            );
            object.insert(entity.clone(), json!(triple.entity));
            object.insert(attribute.clone(), json!(triple.attribute));
            object.insert(value.clone(), triple.value.clone());
            object.insert(reference.clone(), json!(triple.reference));
            object.insert(created_at.clone(), json!(triple.created_at));
        }
        direct.push(QueryRow { cells });
        batched.push(Value::Object(object));
    }
    (direct, Value::Array(batched))
}

fn canonical_result(result: &DatalogResult) -> Value {
    let symbols = result
        .symbol_values
        .iter()
        .map(|(symbol, values)| {
            (
                symbol.as_str().to_owned(),
                Value::Array(values.iter().map(canonical_value).collect()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    json!({
        "topics": result.topics.iter().map(|topic| json!({
            "indexes": topic.indexes,
            "entity": canonical_topic_part(&topic.entity),
            "attribute": canonical_topic_part(&topic.attribute),
            "value": canonical_topic_part(&topic.value),
        })).collect::<Vec<_>>(),
        "symbol_values": symbols,
        "join_rows": result.join_rows.iter().map(|row| row.iter().map(|triple| json!({
            "entity": triple.entity_id,
            "attribute": triple.attr_id,
            "value": canonical_value(&triple.value),
            "created_at": triple.created_at,
        })).collect::<Vec<_>>()).collect::<Vec<_>>(),
    })
}

fn canonical_topic_part(part: &TopicPart) -> Value {
    match part {
        TopicPart::Any => json!({"kind": "any"}),
        TopicPart::Values(values) => json!({
            "kind": "values",
            "values": values.iter().map(canonical_value).collect::<Vec<_>>()
        }),
        TopicPart::Not(value) => json!({"kind": "not", "value": canonical_value(value)}),
    }
}

fn canonical_value(value: &DatalogValue) -> Value {
    match value {
        DatalogValue::Uuid(value) => json!({"kind": "uuid", "value": value}),
        DatalogValue::Json(value) => json!({"kind": "json", "value": value}),
    }
}
