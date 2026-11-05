use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use instant_datalog::{
    ComparatorClause, ComparatorOperation, DataType, DatalogValue, FunctionClause,
    InvalidationTopic, TopicPart,
};
use instant_wal::{MatchSet, Topic, TopicValue};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// A query-side topic normalized for fast comparison with WAL invalidations.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryTopic {
    pub indexes: Vec<instant_datalog::IndexKey>,
    pub entities: QueryTopicPart,
    pub attributes: QueryTopicPart,
    pub values: QueryTopicPart,
}

/// One component of a query topic.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryTopicPart {
    Any,
    Values(Vec<QueryValue>),
    Comparator(Comparator),
    Not(QueryValue),
    StartsWith(String),
}

/// A value that can appear in an entity, attribute, or value topic component.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryValue {
    Uuid(Uuid),
    Json(Value),
}

/// Typed comparison recorded for a Datalog predicate.
#[derive(Clone, Debug, PartialEq)]
pub struct Comparator {
    pub kind: ComparatorKind,
    pub data_type: DataType,
    pub value: Value,
}

/// Comparison operation with names independent of a wire encoding.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComparatorKind {
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Like,
    CaseInsensitiveLike,
}

impl From<ComparatorOperation> for ComparatorKind {
    fn from(value: ComparatorOperation) -> Self {
        match value {
            ComparatorOperation::GreaterThan => Self::GreaterThan,
            ComparatorOperation::GreaterThanOrEqual => Self::GreaterThanOrEqual,
            ComparatorOperation::LessThan => Self::LessThan,
            ComparatorOperation::LessThanOrEqual => Self::LessThanOrEqual,
            ComparatorOperation::Like => Self::Like,
            ComparatorOperation::CaseInsensitiveLike => Self::CaseInsensitiveLike,
        }
    }
}

impl From<&InvalidationTopic> for QueryTopic {
    fn from(topic: &InvalidationTopic) -> Self {
        Self {
            indexes: topic.indexes.clone(),
            entities: query_part(&topic.entity),
            attributes: query_part(&topic.attribute),
            values: query_part(&topic.value),
        }
    }
}

fn query_part(part: &TopicPart) -> QueryTopicPart {
    let values = match part {
        TopicPart::Any => return QueryTopicPart::Any,
        TopicPart::Not(value) => return QueryTopicPart::Not(query_value(value)),
        TopicPart::Values(values) => values,
    };
    if let [DatalogValue::Json(value)] = values.as_slice()
        && let Ok(function) = serde_json::from_value::<FunctionClause>(value.clone())
    {
        return match function {
            FunctionClause::Comparator(ComparatorClause {
                operation,
                data_type,
                value,
            }) => QueryTopicPart::Comparator(Comparator {
                kind: operation.into(),
                data_type,
                value,
            }),
            FunctionClause::Not(value) => QueryTopicPart::Not(QueryValue::Json(value)),
            FunctionClause::EntityIdStartsWith(prefix) => QueryTopicPart::StartsWith(prefix),
            // `$isNull` produces its own broad EA topics in instant-datalog.
            FunctionClause::IsNull(_) => QueryTopicPart::Any,
        };
    }
    QueryTopicPart::Values(values.iter().map(query_value).collect())
}

fn query_value(value: &DatalogValue) -> QueryValue {
    match value {
        DatalogValue::Uuid(value) => QueryValue::Uuid(*value),
        DatalogValue::Json(value) => QueryValue::Json(value.clone()),
    }
}

/// Returns whether any WAL topic intersects any query dependency topic.
#[must_use]
pub fn topics_intersect(invalidations: &[Topic], queries: &[QueryTopic]) -> bool {
    invalidations.iter().any(|invalidation| {
        queries
            .iter()
            .any(|query| topic_matches(invalidation, query))
    })
}

fn topic_matches(invalidation: &Topic, query: &QueryTopic) -> bool {
    sets_intersect(&invalidation.indexes, &query.indexes)
        && match_uuid_part(&invalidation.entities, &query.entities)
        && match_uuid_part(&invalidation.attributes, &query.attributes)
        && match_value_part(&invalidation.values, &query.values)
}

fn sets_intersect<T: PartialEq>(left: &[T], right: &[T]) -> bool {
    let (small, large) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    small.iter().any(|value| large.contains(value))
}

fn match_uuid_part(invalidation: &MatchSet<Uuid>, query: &QueryTopicPart) -> bool {
    match (invalidation, query) {
        (MatchSet::Any, _)
        | (_, QueryTopicPart::Any)
        | (MatchSet::Values(_), QueryTopicPart::Comparator(_)) => true,
        (MatchSet::Values(invalidation), QueryTopicPart::Values(query)) => invalidation
            .iter()
            .any(|value| query.contains(&QueryValue::Uuid(*value))),
        (MatchSet::Values(invalidation), QueryTopicPart::Not(excluded)) => invalidation
            .iter()
            .any(|value| excluded != &QueryValue::Uuid(*value)),
        (MatchSet::Values(invalidation), QueryTopicPart::StartsWith(prefix)) => invalidation
            .iter()
            .any(|value| value.to_string().starts_with(prefix)),
        // Comparator predicates are only meaningful for triple values. The first arm
        // conservatively invalidates if one appears in an identity component.
    }
}

fn match_value_part(invalidation: &MatchSet<TopicValue>, query: &QueryTopicPart) -> bool {
    match (invalidation, query) {
        (MatchSet::Any, _) | (_, QueryTopicPart::Any) => true,
        (MatchSet::Values(invalidation), QueryTopicPart::Values(query)) => invalidation
            .iter()
            .any(|value| query.iter().any(|query| value_equals(value, query))),
        (MatchSet::Values(invalidation), QueryTopicPart::Not(excluded)) => invalidation
            .iter()
            .any(|value| !value_equals(value, excluded)),
        (MatchSet::Values(invalidation), QueryTopicPart::Comparator(comparator)) => invalidation
            .iter()
            .any(|value| comparator_matches(value, comparator)),
        (MatchSet::Values(invalidation), QueryTopicPart::StartsWith(prefix)) => invalidation
            .iter()
            .any(|value| topic_text(value).is_some_and(|value| value.starts_with(prefix))),
    }
}

fn value_equals(invalidation: &TopicValue, query: &QueryValue) -> bool {
    match (invalidation, query) {
        (TopicValue::Reference(left), QueryValue::Uuid(right)) => left == right,
        (TopicValue::Json(left), QueryValue::Json(right)) => left == right,
        (TopicValue::Date(left), QueryValue::Json(Value::String(right))) => right
            .parse::<DateTime<Utc>>()
            .is_ok_and(|right| left == &right),
        (TopicValue::Json(Value::String(left)), QueryValue::Uuid(right)) => {
            Uuid::parse_str(left).is_ok_and(|left| &left == right)
        }
        _ => false,
    }
}

fn comparator_matches(value: &TopicValue, comparator: &Comparator) -> bool {
    let Some(ordering) = compare(value, comparator) else {
        return matches!(
            (topic_json(value), comparator.kind),
            (
                Some(Value::Null),
                ComparatorKind::LessThan | ComparatorKind::LessThanOrEqual
            )
        );
    };
    match comparator.kind {
        ComparatorKind::GreaterThan => ordering.is_gt(),
        ComparatorKind::GreaterThanOrEqual => ordering.is_ge(),
        ComparatorKind::LessThan => ordering.is_lt(),
        ComparatorKind::LessThanOrEqual => ordering.is_le(),
        ComparatorKind::Like | ComparatorKind::CaseInsensitiveLike => {
            let (Some(text), Some(pattern)) = (topic_text(value), comparator.value.as_str()) else {
                return false;
            };
            sql_like(
                text,
                pattern,
                comparator.kind == ComparatorKind::CaseInsensitiveLike,
            )
        }
    }
}

fn compare(value: &TopicValue, comparator: &Comparator) -> Option<Ordering> {
    match comparator.data_type {
        DataType::Number => {
            let left = topic_json(value)?.as_f64()?;
            let right = comparator.value.as_f64()?;
            left.partial_cmp(&right)
        }
        DataType::Boolean => {
            let left = topic_json(value)?.as_bool()?;
            let right = comparator.value.as_bool()?;
            Some(left.cmp(&right))
        }
        DataType::String => Some(topic_text(value)?.cmp(comparator.value.as_str()?)),
        DataType::Date => {
            let left = topic_date(value)?;
            let right = comparator.value.as_str()?.parse::<DateTime<Utc>>().ok()?;
            Some(left.cmp(&right))
        }
    }
}

fn topic_json(value: &TopicValue) -> Option<&Value> {
    match value {
        TopicValue::Json(value) => Some(value),
        TopicValue::Reference(_) | TopicValue::Date(_) => None,
    }
}

fn topic_text(value: &TopicValue) -> Option<&str> {
    match value {
        TopicValue::Json(Value::String(value)) => Some(value),
        TopicValue::Json(_) | TopicValue::Reference(_) | TopicValue::Date(_) => None,
    }
}

fn topic_date(value: &TopicValue) -> Option<DateTime<Utc>> {
    match value {
        TopicValue::Date(value) => Some(*value),
        TopicValue::Json(Value::String(value)) => value.parse().ok(),
        TopicValue::Json(_) | TopicValue::Reference(_) => None,
    }
}

fn sql_like(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let mut expression = String::from("^");
    let mut literal = String::new();
    for character in pattern.chars() {
        match character {
            '%' | '_' => {
                expression.push_str(&regex::escape(&literal));
                literal.clear();
                expression.push_str(if character == '%' { ".*" } else { "." });
            }
            character => literal.push(character),
        }
    }
    expression.push_str(&regex::escape(&literal));
    expression.push('$');
    RegexBuilder::new(&expression)
        .dot_matches_new_line(true)
        .case_insensitive(case_insensitive)
        .build()
        .is_ok_and(|regex| regex.is_match(text))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use instant_datalog::{IndexKey, TopicPart};
    use serde_json::json;

    use super::*;

    fn comparator(kind: ComparatorKind, data_type: DataType, value: Value) -> QueryTopicPart {
        QueryTopicPart::Comparator(Comparator {
            kind,
            data_type,
            value,
        })
    }

    fn values(values: impl IntoIterator<Item = Value>) -> MatchSet<TopicValue> {
        MatchSet::Values(values.into_iter().map(TopicValue::Json).collect())
    }

    #[allow(clippy::needless_pass_by_value)]
    fn matches(values: MatchSet<TopicValue>, query: QueryTopicPart) -> bool {
        match_value_part(&values, &query)
    }

    #[test]
    fn wildcard_and_literal_parts_match() {
        assert!(matches(MatchSet::Any, QueryTopicPart::Any));
        assert!(matches(values([json!(1)]), QueryTopicPart::Any));
        assert!(matches(
            values([json!(1)]),
            QueryTopicPart::Values(vec![QueryValue::Json(json!(1))])
        ));
        assert!(!matches(
            values([json!(1)]),
            QueryTopicPart::Values(vec![QueryValue::Json(json!(0))])
        ));
    }

    #[test]
    fn number_comparators_match_boundaries() {
        assert!(matches(
            values([json!(1)]),
            comparator(
                ComparatorKind::GreaterThanOrEqual,
                DataType::Number,
                json!(1)
            )
        ));
        assert!(!matches(
            values([json!(1)]),
            comparator(ComparatorKind::GreaterThan, DataType::Number, json!(1))
        ));
        assert!(matches(
            values([json!(-1)]),
            comparator(ComparatorKind::LessThan, DataType::Number, json!(1))
        ));
        assert!(!matches(
            values([json!(-1)]),
            comparator(ComparatorKind::LessThanOrEqual, DataType::Number, json!(-2))
        ));
    }

    #[test]
    fn string_comparators_and_sql_wildcards_match() {
        assert!(matches(
            values([json!("b")]),
            comparator(
                ComparatorKind::GreaterThanOrEqual,
                DataType::String,
                json!("b")
            )
        ));
        assert!(!matches(
            values([json!("a")]),
            comparator(
                ComparatorKind::GreaterThanOrEqual,
                DataType::String,
                json!("b")
            )
        ));
        for (text, pattern) in [
            ("hello", "he%"),
            ("hello world", "%w%r_d"),
            ("[[Daily/2025-08-15]]", "%[[Daily/2025-08-15]]%"),
            ("a\nb", "a%b"),
            ("a\nb", "a_b"),
            ("x\ny", "%"),
            ("a\r\nb", "a__b"),
        ] {
            assert!(matches(
                values([json!(text)]),
                comparator(ComparatorKind::Like, DataType::String, json!(pattern))
            ));
        }
        assert!(!matches(
            values([json!("a\r\nb")]),
            comparator(ComparatorKind::Like, DataType::String, json!("a_b"))
        ));
        assert!(!matches(
            values([json!("abc\n")]),
            comparator(ComparatorKind::Like, DataType::String, json!("abc"))
        ));
        assert!(matches(
            values([json!("A\nB")]),
            comparator(
                ComparatorKind::CaseInsensitiveLike,
                DataType::String,
                json!("a%b")
            )
        ));
    }

    #[test]
    fn booleans_dates_and_nulls_match_reference_ordering() {
        assert!(matches(
            values([json!(true)]),
            comparator(ComparatorKind::GreaterThan, DataType::Boolean, json!(false))
        ));
        assert!(!matches(
            values([json!(true)]),
            comparator(
                ComparatorKind::LessThanOrEqual,
                DataType::Boolean,
                json!(false)
            )
        ));

        let three = Utc.with_ymd_and_hms(2025, 2, 17, 3, 0, 0).unwrap();
        let two = Utc.with_ymd_and_hms(2025, 2, 17, 2, 0, 0).unwrap();
        assert!(matches(
            MatchSet::Values(vec![TopicValue::Date(three)]),
            comparator(
                ComparatorKind::GreaterThan,
                DataType::Date,
                json!(two.to_rfc3339())
            )
        ));
        for kind in [ComparatorKind::LessThan, ComparatorKind::LessThanOrEqual] {
            assert!(matches(
                values([Value::Null]),
                comparator(kind, DataType::Number, json!(1))
            ));
        }
        for kind in [
            ComparatorKind::GreaterThan,
            ComparatorKind::GreaterThanOrEqual,
            ComparatorKind::Like,
            ComparatorKind::CaseInsensitiveLike,
        ] {
            assert!(!matches(
                values([Value::Null]),
                comparator(kind, DataType::Number, json!(1))
            ));
        }
    }

    #[test]
    fn not_matches_when_any_invalidated_value_differs() {
        assert!(matches(
            values([json!(1)]),
            QueryTopicPart::Not(QueryValue::Json(json!(2)))
        ));
        assert!(!matches(
            values([json!(1)]),
            QueryTopicPart::Not(QueryValue::Json(json!(1)))
        ));
        assert!(matches(
            values([json!(1), json!(2)]),
            QueryTopicPart::Not(QueryValue::Json(json!(1)))
        ));
    }

    #[test]
    fn typed_reference_not_excludes_uuid_entities_and_values() {
        let attr = Uuid::new_v4();
        let excluded = Uuid::new_v4();
        let other = Uuid::new_v4();
        let entity_query = QueryTopic::from(&InvalidationTopic {
            indexes: vec![IndexKey::Vae],
            entity: TopicPart::Not(DatalogValue::Uuid(excluded)),
            attribute: TopicPart::Values(vec![DatalogValue::Uuid(attr)]),
            value: TopicPart::Any,
        });
        let entity_change = |entity| Topic {
            indexes: vec![IndexKey::Vae],
            entities: MatchSet::Values(vec![entity]),
            attributes: MatchSet::Values(vec![attr]),
            values: MatchSet::Any,
        };
        assert!(!topics_intersect(
            &[entity_change(excluded)],
            std::slice::from_ref(&entity_query)
        ));
        assert!(topics_intersect(&[entity_change(other)], &[entity_query]));

        let value_topic = InvalidationTopic {
            indexes: vec![IndexKey::Vae],
            entity: TopicPart::Any,
            attribute: TopicPart::Values(vec![DatalogValue::Uuid(attr)]),
            value: TopicPart::Not(DatalogValue::Uuid(excluded)),
        };
        let value_topic: InvalidationTopic =
            serde_json::from_value(serde_json::to_value(value_topic).unwrap()).unwrap();
        let value_query = QueryTopic::from(&value_topic);
        let value_change = |value| Topic {
            indexes: vec![IndexKey::Vae],
            entities: MatchSet::Any,
            attributes: MatchSet::Values(vec![attr]),
            values: MatchSet::Values(vec![TopicValue::Reference(value)]),
        };
        assert!(!topics_intersect(
            &[value_change(excluded)],
            std::slice::from_ref(&value_query)
        ));
        assert!(topics_intersect(&[value_change(other)], &[value_query]));
    }

    #[test]
    fn json_not_function_keeps_json_semantics_for_uuid_shaped_strings() {
        let excluded = Uuid::new_v4();
        let encoded =
            DatalogValue::Json(serde_json::to_value(FunctionClause::Not(json!(excluded))).unwrap());
        let query = query_part(&TopicPart::Values(vec![encoded]));
        assert_eq!(
            query,
            QueryTopicPart::Not(QueryValue::Json(json!(excluded)))
        );
        assert!(!matches(
            values([json!(excluded)]),
            QueryTopicPart::Not(QueryValue::Json(json!(excluded)))
        ));
    }

    #[test]
    fn datalog_function_encoding_is_normalized_and_topics_intersect() {
        let attr = Uuid::new_v4();
        let entity = Uuid::new_v4();
        let function = FunctionClause::Comparator(ComparatorClause {
            operation: ComparatorOperation::GreaterThanOrEqual,
            data_type: DataType::Number,
            value: json!(10),
        });
        let query = QueryTopic::from(&InvalidationTopic {
            indexes: vec![IndexKey::Ave],
            entity: TopicPart::Any,
            attribute: TopicPart::Values(vec![DatalogValue::Uuid(attr)]),
            value: TopicPart::Values(vec![DatalogValue::Json(
                serde_json::to_value(function).unwrap(),
            )]),
        });
        let invalidation = Topic {
            indexes: vec![IndexKey::Ave],
            entities: MatchSet::Values(vec![entity]),
            attributes: MatchSet::Values(vec![attr]),
            values: values([json!(11)]),
        };
        assert!(topics_intersect(&[invalidation], &[query]));
    }
}
