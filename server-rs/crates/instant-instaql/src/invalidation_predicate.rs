use std::collections::HashMap;

use chrono::Utc;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    Attribute, AttributeCatalog, Cardinality, DataType, Form, WhereCondition, WhereValue,
    parse_date_millis,
};

/// One entity image extracted from a WAL record for query-level invalidation filtering.
#[derive(Clone, Debug, PartialEq)]
pub struct InvalidationEntity {
    pub etype: String,
    pub attrs: HashMap<Uuid, Value>,
}

/// Why a query cannot use the conservative entity predicate optimization.
///
/// Unsupported queries must retain their normal Datalog-topic invalidation; they
/// are not errors and must never suppress a refresh.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("InstaQL invalidation predicate does not support {0}")]
pub struct InvalidationPredicateUnsupported(pub &'static str);

/// A fail-open entity predicate equivalent to the JVM `instaql-topic` program.
#[derive(Clone, Debug, PartialEq)]
pub struct InvalidationPredicate {
    roots: Vec<PredicateRoot>,
}

/// Result of evaluating the optional entity-level invalidation predicate.
///
/// `FailOpen` is deliberately distinct from `Match`: both must refresh the
/// query, but production observability needs to identify conversion failures
/// without weakening the JVM-compatible fail-open boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidationPredicateOutcome {
    Match,
    NoMatch,
    FailOpen,
}

#[derive(Clone, Debug, PartialEq)]
struct PredicateRoot {
    etype: String,
    checks: Vec<PredicateCheck>,
    descendant_etypes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum PredicateCheck {
    Equal { attr_id: Uuid, value: Value },
    DateEqual { attr_id: Uuid, epoch_millis: i64 },
    IsNull { attr_id: Uuid, expected: bool },
}

impl InvalidationPredicate {
    /// Returns whether a before/after WAL entity can affect this query.
    ///
    /// Runtime conversion failures deliberately return `true`, matching the JVM
    /// program's fail-open exception boundary.
    #[must_use]
    pub fn matches(&self, entity: &InvalidationEntity) -> bool {
        self.match_outcome(entity) != InvalidationPredicateOutcome::NoMatch
    }

    /// Evaluates the predicate while preserving whether a refresh was caused
    /// by the reference runtime's fail-open conversion-error behavior.
    #[must_use]
    pub fn match_outcome(&self, entity: &InvalidationEntity) -> InvalidationPredicateOutcome {
        match self.try_matches(entity) {
            Ok(true) => InvalidationPredicateOutcome::Match,
            Ok(false) => InvalidationPredicateOutcome::NoMatch,
            Err(()) => InvalidationPredicateOutcome::FailOpen,
        }
    }

    fn try_matches(&self, entity: &InvalidationEntity) -> Result<bool, ()> {
        for root in &self.roots {
            if root.descendant_etypes.contains(&entity.etype) {
                return Ok(true);
            }
            if root.etype != entity.etype {
                continue;
            }
            let mut matched = true;
            for check in &root.checks {
                matched &= match check {
                    PredicateCheck::Equal { attr_id, value } => {
                        entity.attrs.get(attr_id) == Some(value)
                    }
                    PredicateCheck::DateEqual {
                        attr_id,
                        epoch_millis,
                    } => {
                        let Some(value) = entity.attrs.get(attr_id) else {
                            matched = false;
                            break;
                        };
                        if value.is_null() {
                            matched = false;
                            break;
                        }
                        normalized_date_millis(value).ok_or(())? == *epoch_millis
                    }
                    PredicateCheck::IsNull { attr_id, expected } => {
                        let is_null = entity.attrs.get(attr_id).is_none_or(Value::is_null);
                        is_null == *expected
                    }
                };
                if !matched {
                    break;
                }
            }
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Compiles the optional entity-level invalidation predicate used to discard
/// WAL records that cannot change a query result.
///
/// # Errors
///
/// Returns a stable unsupported reason for query shapes that cannot be proven
/// safe. Callers must fall back to ordinary Datalog topics on error.
pub fn compile_invalidation_predicate(
    forms: &[Form],
    catalog: &AttributeCatalog,
) -> Result<InvalidationPredicate, InvalidationPredicateUnsupported> {
    let roots = forms
        .iter()
        .map(|form| compile_root(form, catalog))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(InvalidationPredicate { roots })
}

fn compile_root(
    form: &Form,
    catalog: &AttributeCatalog,
) -> Result<PredicateRoot, InvalidationPredicateUnsupported> {
    let checks = form
        .options
        .where_clause
        .iter()
        .map(|condition| compile_check(condition, &form.etype, catalog))
        .collect::<Result<Vec<_>, _>>()?;
    let mut descendant_etypes = Vec::new();
    for child in &form.children {
        validate_child(child, catalog, &mut descendant_etypes)?;
    }
    Ok(PredicateRoot {
        etype: form.etype.clone(),
        checks,
        descendant_etypes,
    })
}

fn validate_child(
    form: &Form,
    catalog: &AttributeCatalog,
    etypes: &mut Vec<String>,
) -> Result<(), InvalidationPredicateUnsupported> {
    for condition in &form.options.where_clause {
        compile_check(condition, &form.etype, catalog)?;
    }
    if !etypes.contains(&form.etype) {
        etypes.push(form.etype.clone());
    }
    for child in &form.children {
        validate_child(child, catalog, etypes)?;
    }
    Ok(())
}

fn compile_check(
    condition: &WhereCondition,
    etype: &str,
    catalog: &AttributeCatalog,
) -> Result<PredicateCheck, InvalidationPredicateUnsupported> {
    let WhereCondition::Condition { path, value } = condition else {
        return Err(InvalidationPredicateUnsupported("boolean-where-condition"));
    };
    if path.len() != 1 {
        return Err(InvalidationPredicateUnsupported("multi-part-path"));
    }
    let label = &path[0];
    if catalog.reverse(etype, label).is_some() {
        return Err(InvalidationPredicateUnsupported("reverse-attribute"));
    }
    let attribute = catalog
        .forward(etype, label)
        .ok_or(InvalidationPredicateUnsupported("unknown-attribute"))?;
    if attribute.cardinality != Cardinality::One {
        return Err(InvalidationPredicateUnsupported("cardinality-many"));
    }
    match value {
        WhereValue::Scalar(value) => compile_scalar(attribute, value),
        WhereValue::IsNull(expected) => Ok(PredicateCheck::IsNull {
            attr_id: attribute.id,
            expected: *expected,
        }),
        WhereValue::In(_)
        | WhereValue::Not(_)
        | WhereValue::Comparator { .. }
        | WhereValue::EntityIdStartsWith(_) => {
            Err(InvalidationPredicateUnsupported("complex-value-type"))
        }
    }
}

fn compile_scalar(
    attribute: &Attribute,
    value: &Value,
) -> Result<PredicateCheck, InvalidationPredicateUnsupported> {
    if attribute.checked_data_type == Some(DataType::Date) {
        let epoch_millis = normalized_date_millis(value)
            .ok_or(InvalidationPredicateUnsupported("invalid-date-literal"))?;
        Ok(PredicateCheck::DateEqual {
            attr_id: attribute.id,
            epoch_millis,
        })
    } else {
        Ok(PredicateCheck::Equal {
            attr_id: attribute.id,
            value: value.clone(),
        })
    }
}

fn normalized_date_millis(value: &Value) -> Option<i64> {
    if matches!(
        value.as_str(),
        Some("tomorrow" | "now" | "yesterday" | "today")
    ) {
        return None;
    }
    parse_date_millis(value, Utc::now().timestamp_millis())
}
