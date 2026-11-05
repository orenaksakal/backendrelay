use std::collections::HashSet;

use chrono::Utc;
use instant_datalog::{
    ComparatorClause, ComparatorOperation, DataType, FunctionClause, Index, IndexKey, IsNullClause,
    NormalizedClause, RawPattern, RawTerm, Symbol,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[cfg(test)]
use uuid::Uuid;

use super::{
    Attribute, AttributeCatalog, AttributeValueType, Form, QueryPathSegment, ValidationError,
    appended, clojure_map_entries, coerce_public_uuid, spec_validation, validation,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhereComparator {
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Like,
    CaseInsensitiveLike,
}

impl From<WhereComparator> for ComparatorOperation {
    fn from(value: WhereComparator) -> Self {
        match value {
            WhereComparator::GreaterThan => Self::GreaterThan,
            WhereComparator::GreaterThanOrEqual => Self::GreaterThanOrEqual,
            WhereComparator::LessThan => Self::LessThan,
            WhereComparator::LessThanOrEqual => Self::LessThanOrEqual,
            WhereComparator::Like => Self::Like,
            WhereComparator::CaseInsensitiveLike => Self::CaseInsensitiveLike,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum WhereValue {
    Scalar(Value),
    In(Vec<Value>),
    Not(Value),
    IsNull(bool),
    Comparator {
        operation: WhereComparator,
        value: Value,
    },
    EntityIdStartsWith(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WhereCondition {
    Condition {
        path: Vec<String>,
        value: WhereValue,
    },
    And {
        conditions: Vec<WhereCondition>,
    },
    Or {
        conditions: Vec<WhereCondition>,
    },
}

pub(crate) fn coerce_where(
    value: &Value,
    path: &[String],
    etype: &str,
    catalog: &AttributeCatalog,
    spec_path: &[QueryPathSegment],
) -> Result<Vec<WhereCondition>, ValidationError> {
    let object = value.as_object().ok_or_else(|| {
        validation(
            "map?",
            path,
            format!("Expected an object for `where`, got {value}."),
        )
    })?;
    let mut conditions = Vec::with_capacity(object.len());
    let mut spec_errors = Vec::new();
    for (index, (key, value)) in clojure_map_entries(object).into_iter().enumerate() {
        let condition_spec_path = spec_path
            .iter()
            .cloned()
            .chain([QueryPathSegment::Index(index)])
            .collect::<Vec<_>>();
        match coerce_entry(key, value, path, etype, catalog, &condition_spec_path) {
            Ok(condition) => conditions.push(condition),
            Err(error) if error.message.is_empty() => spec_errors.push(error),
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = spec_errors
        .into_iter()
        .max_by_key(|error| (error.path.len(), error.path.last().cloned()))
    {
        Err(error)
    } else {
        Ok(conditions)
    }
}

fn coerce_entry(
    key: &str,
    value: &Value,
    path: &[String],
    etype: &str,
    catalog: &AttributeCatalog,
    spec_path: &[QueryPathSegment],
) -> Result<WhereCondition, ValidationError> {
    if matches!(key, "and" | "or") {
        let values = value.as_array().ok_or_else(|| {
            validation(
                "non-empty-list?",
                &appended(path, key.to_owned()),
                format!("The list of `{key}` conditions must be an array."),
            )
        })?;
        if values.is_empty() {
            return Err(validation(
                "non-empty-list?",
                &appended(path, key.to_owned()),
                format!("The list of `{key}` conditions can't be empty."),
            ));
        }
        let groups = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let nested_spec_path = spec_path
                    .iter()
                    .cloned()
                    .chain([
                        QueryPathSegment::Field(key.to_owned()),
                        QueryPathSegment::Index(index),
                    ])
                    .collect::<Vec<_>>();
                coerce_where(value, path, etype, catalog, &nested_spec_path)
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(if key == "and" {
            collapse_group(true, groups.into_iter().flatten().collect())
        } else {
            // Each object in an `or` array is one branch. Multiple fields in
            // that object are conjunctive, rather than independent OR arms.
            let branches = groups
                .into_iter()
                .map(|conditions| collapse_group(true, conditions))
                .collect();
            collapse_group(false, branches)
        });
    }
    let field_path = split_where_path(key);
    let coerced = if field_path
        .last()
        .is_some_and(|field| field == "$entityIdStartsWith")
    {
        CoercedWhereValue {
            value: WhereValue::EntityIdStartsWith(
                value
                    .as_str()
                    .ok_or_else(|| {
                        validation(
                            "string?",
                            &appended(path, key.to_owned()),
                            "The `$entityIdStartsWith` value must be a string.",
                        )
                    })?
                    .to_owned(),
            ),
            missing_inclusive: false,
        }
    } else {
        coerce_value(
            value,
            &appended(path, key.to_owned()),
            &spec_path
                .iter()
                .cloned()
                .chain([QueryPathSegment::Index(1)])
                .collect::<Vec<_>>(),
        )?
    };
    let condition = WhereCondition::Condition {
        path: field_path,
        value: coerced.value,
    };
    if coerced.missing_inclusive {
        Ok(include_missing_branches(condition, etype, catalog))
    } else {
        Ok(condition)
    }
}

fn include_missing_branches(
    condition: WhereCondition,
    etype: &str,
    catalog: &AttributeCatalog,
) -> WhereCondition {
    let WhereCondition::Condition { path, .. } = &condition else {
        unreachable!()
    };
    let path = path.clone();
    let final_is_indexed = path_attribute(&path, etype, catalog)
        .is_some_and(|attribute| attribute.indexed && !attribute.indexing);
    let prefix_count = path.len().saturating_sub(usize::from(final_is_indexed));
    let mut conditions = vec![condition];
    conditions.extend((1..=prefix_count).map(|length| WhereCondition::Condition {
        path: path[..length].to_vec(),
        value: WhereValue::IsNull(true),
    }));
    dedupe_and_collapse_or(conditions)
}

fn dedupe_and_collapse_or(conditions: Vec<WhereCondition>) -> WhereCondition {
    let mut deduped = Vec::with_capacity(conditions.len());
    for condition in conditions {
        if !deduped.contains(&condition) {
            deduped.push(condition);
        }
    }
    collapse_group(false, deduped)
}

/// Matches `clojure.string/split` with its default zero limit: empty fields at
/// the end of a non-empty input are discarded, while leading and interior
/// empty fields remain part of the path. An entirely empty input has no regex
/// match and therefore retains its one empty field.
fn split_where_path(key: &str) -> Vec<String> {
    let mut fields = key.split('.').map(str::to_owned).collect::<Vec<_>>();
    if !key.is_empty() {
        while fields.last().is_some_and(String::is_empty) {
            fields.pop();
        }
    }
    fields
}

fn collapse_group(and: bool, conditions: Vec<WhereCondition>) -> WhereCondition {
    let mut collapsed = Vec::new();
    for condition in conditions {
        match condition {
            WhereCondition::And { conditions } if and => collapsed.extend(conditions),
            WhereCondition::Or { conditions } if !and => collapsed.extend(conditions),
            condition => collapsed.push(condition),
        }
    }
    if collapsed.len() == 1 {
        collapsed.pop().expect("one condition")
    } else if and {
        WhereCondition::And {
            conditions: collapsed,
        }
    } else {
        WhereCondition::Or {
            conditions: collapsed,
        }
    }
}

struct CoercedWhereValue {
    value: WhereValue,
    missing_inclusive: bool,
}

fn coerce_value(
    value: &Value,
    path: &[String],
    spec_path: &[QueryPathSegment],
) -> Result<CoercedWhereValue, ValidationError> {
    let Some(arguments) = value.as_object() else {
        if scalar(value, false) {
            return Ok(CoercedWhereValue {
                value: WhereValue::Scalar(value.clone()),
                missing_inclusive: false,
            });
        }
        return Err(validation(
            "where-value?",
            path,
            format!("Unsupported where value {value}."),
        ));
    };
    if arguments.is_empty() {
        return Err(validation(
            "single-operator?",
            path,
            "A where argument map must contain a supported operator.",
        ));
    }

    // `normalize-ne-to-not` runs before the upstream Clojure spec. `$ne`
    // therefore overwrites a simultaneously supplied `$not`, while every
    // other entry is still conformed even though downstream lowering consumes
    // only the first map entry.
    let has_missing_branch = arguments.contains_key("$not") || arguments.contains_key("$ne");
    let mut normalized = arguments.clone();
    if let Some(argument) = normalized.remove("$ne") {
        normalized.insert("$not".to_owned(), argument);
    }
    let supported_operator = |operator: &str| {
        matches!(
            operator,
            "in" | "$in"
                | "$not"
                | "$isNull"
                | "$gt"
                | "$gte"
                | "$lt"
                | "$lte"
                | "$like"
                | "$ilike"
                | "$entityId"
        )
    };
    let mut spec_errors = Vec::new();
    if normalized
        .keys()
        .any(|operator| !supported_operator(operator))
    {
        spec_errors.push(spec_validation(
            "instant.db.instaql/where-value-valid-keys?",
            spec_path.to_vec(),
        ));
    }
    let mut selected = None;
    for (operator, argument) in clojure_map_entries(&normalized) {
        if !supported_operator(operator) {
            continue;
        }
        let operator_spec_path = spec_path
            .iter()
            .cloned()
            .chain([QueryPathSegment::Field(operator.clone())])
            .collect::<Vec<_>>();
        match coerce_operator(operator, argument, path, operator_spec_path) {
            Ok(value) if selected.is_none() => selected = Some(value),
            Ok(_) => {}
            Err(error) if error.message.is_empty() => spec_errors.push(error),
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = spec_errors
        .into_iter()
        .max_by_key(|error| (error.path.len(), error.path.last().cloned()))
    {
        return Err(error);
    }
    let selected =
        selected.expect("a non-empty argument map remains non-empty after normalization");
    let missing_inclusive = has_missing_branch && !matches!(&selected, WhereValue::Not(_));
    Ok(CoercedWhereValue {
        value: selected,
        missing_inclusive,
    })
}

fn coerce_operator(
    operator: &str,
    argument: &Value,
    path: &[String],
    spec_path: Vec<QueryPathSegment>,
) -> Result<WhereValue, ValidationError> {
    match operator {
        "in" | "$in" => {
            let values = argument
                .as_array()
                .ok_or_else(|| spec_validation("vector?", spec_path.clone()))?;
            if values.iter().all(|value| scalar(value, true)) {
                Ok(WhereValue::In(values.clone()))
            } else {
                let invalid_index = values
                    .iter()
                    .position(|value| !scalar(value, true))
                    .expect("the invalid branch contains an unsupported value");
                Err(spec_validation(
                    "instant.db.instaql/in-value-valid?",
                    spec_path
                        .into_iter()
                        .chain([QueryPathSegment::Index(invalid_index)])
                        .collect(),
                ))
            }
        }
        "$not" | "$ne" if scalar(argument, false) => Ok(WhereValue::Not(argument.clone())),
        "$not" | "$ne" => Err(spec_validation(
            "instant.db.instaql/where-value-valid?",
            spec_path,
        )),
        "$isNull" => argument
            .as_bool()
            .map(WhereValue::IsNull)
            .ok_or_else(|| spec_validation("boolean?", spec_path.clone())),
        "$gt" | "$gte" | "$lt" | "$lte" | "$like" | "$ilike" if scalar(argument, false) => {
            Ok(WhereValue::Comparator {
                operation: match operator {
                    "$gt" => WhereComparator::GreaterThan,
                    "$gte" => WhereComparator::GreaterThanOrEqual,
                    "$lt" => WhereComparator::LessThan,
                    "$lte" => WhereComparator::LessThanOrEqual,
                    "$like" => WhereComparator::Like,
                    "$ilike" => WhereComparator::CaseInsensitiveLike,
                    _ => unreachable!(),
                },
                value: argument.clone(),
            })
        }
        "$gt" | "$gte" | "$lt" | "$lte" | "$like" | "$ilike" => {
            Err(spec_validation("boolean?", spec_path))
        }
        // The pinned spec's valid-keys predicate admits `$entityId`, but its
        // open `s/keys` declaration does not attach a value predicate or
        // conformer to that key. Downstream Clojure therefore retains the
        // singleton argument map as an ordinary equality value.
        "$entityId" => Ok(WhereValue::Scalar(Value::Object(Map::from_iter([(
            operator.to_owned(),
            argument.clone(),
        )])))),
        _ => Err(validation(
            "supported-where-operator?",
            &appended(path, operator.to_owned()),
            format!("Unsupported where operator `{operator}`."),
        )),
    }
}

fn scalar(value: &Value, allow_null: bool) -> bool {
    allow_null && value.is_null() || value.is_string() || value.is_number() || value.is_boolean()
}

/// Compiles typed `where` conditions into recursive Datalog clauses.
///
/// # Errors
///
/// Returns a validation error for missing attributes, non-link path segments,
/// invalid reference UUIDs, or values incompatible with enforced attribute types.
pub fn compile_where(
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    dedupe_conditions(&form.options.where_clause)
        .iter()
        .enumerate()
        .map(|(index, condition)| {
            compile_condition(condition, form, catalog, root_symbol, &mut vec![index])
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|groups| groups.into_iter().flatten().collect())
}

fn dedupe_conditions(conditions: &[WhereCondition]) -> Vec<WhereCondition> {
    let mut deduped = Vec::new();
    for condition in conditions {
        let condition = match condition {
            WhereCondition::Condition { .. } => condition.clone(),
            WhereCondition::And { conditions } => WhereCondition::And {
                conditions: dedupe_conditions(conditions),
            },
            WhereCondition::Or { conditions } => WhereCondition::Or {
                conditions: dedupe_conditions(conditions),
            },
        };
        if !deduped.contains(&condition) {
            deduped.push(condition);
        }
    }
    deduped
}

fn compile_condition(
    condition: &WhereCondition,
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
    namespace: &mut Vec<usize>,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    match condition {
        WhereCondition::Condition { path, value } => match value {
            WhereValue::Not(_) => compile_not(path, value, form, catalog, root_symbol, namespace),
            WhereValue::IsNull(true) if path.len() > 1 => {
                compile_null_prefixes(path, form, catalog, root_symbol, namespace)
            }
            _ => compile_path(path, value, form, catalog, root_symbol, namespace),
        },
        WhereCondition::And { conditions } => {
            let clauses = compile_branches(conditions, form, catalog, root_symbol, namespace)?;
            Ok(vec![NormalizedClause::And { clauses }])
        }
        WhereCondition::Or { conditions } => {
            let conditions = combine_or_conditions(conditions, form, catalog);
            let clauses = conditions
                .iter()
                .enumerate()
                .map(|(index, condition)| {
                    namespace.push(index);
                    let result =
                        compile_condition(condition, form, catalog, root_symbol, namespace);
                    namespace.pop();
                    result.map(|clauses| match clauses.as_slice() {
                        [clause] => clause.clone(),
                        _ => NormalizedClause::And { clauses },
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(vec![NormalizedClause::Or {
                join_symbol: root_symbol.clone(),
                clauses,
            }])
        }
    }
}

fn combine_or_conditions(
    conditions: &[WhereCondition],
    form: &Form,
    catalog: &AttributeCatalog,
) -> Vec<WhereCondition> {
    let mut uncombined = Vec::new();
    let mut grouped = Vec::<(Vec<String>, Vec<Value>)>::new();
    for condition in conditions {
        let WhereCondition::Condition { path, value } = condition else {
            uncombined.push(condition.clone());
            continue;
        };
        let value = match value {
            WhereValue::Scalar(value)
                if matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_)) =>
            {
                value.clone()
            }
            WhereValue::IsNull(true)
                if path_attribute(path, &form.etype, catalog)
                    .is_some_and(|attribute| attribute.indexed && !attribute.indexing) =>
            {
                Value::Null
            }
            _ => {
                uncombined.push(condition.clone());
                continue;
            }
        };
        if path.first().is_some_and(|segment| segment.starts_with('$')) {
            uncombined.push(condition.clone());
            continue;
        }
        if let Some((_, values)) = grouped
            .iter_mut()
            .find(|(group_path, _)| group_path == path)
        {
            values.push(value);
        } else {
            grouped.push((path.clone(), vec![value]));
        }
    }
    uncombined.extend(
        grouped
            .into_iter()
            .map(|(path, values)| WhereCondition::Condition {
                path,
                value: WhereValue::In(values),
            }),
    );
    uncombined
}

fn compile_not(
    path: &[String],
    value: &WhereValue,
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
    namespace: &mut Vec<usize>,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    let mut branch_paths = vec![(path.to_vec(), value.clone())];
    let final_attribute = path_attribute(path, &form.etype, catalog)
        .ok_or_else(|| missing_attribute(&form.etype, &path.join("."), form, path))?;
    let null_prefix_count = path.len().saturating_sub(usize::from(
        final_attribute.indexed && !final_attribute.indexing,
    ));
    branch_paths.extend(
        (1..=null_prefix_count).map(|length| (path[..length].to_vec(), WhereValue::IsNull(true))),
    );
    compile_path_branches(&branch_paths, form, catalog, root_symbol, namespace)
}

fn compile_null_prefixes(
    path: &[String],
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
    namespace: &mut Vec<usize>,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    let branches = (1..=path.len())
        .map(|length| (path[..length].to_vec(), WhereValue::IsNull(true)))
        .collect::<Vec<_>>();
    compile_path_branches(&branches, form, catalog, root_symbol, namespace)
}

fn compile_path_branches(
    branches: &[(Vec<String>, WhereValue)],
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
    namespace: &mut Vec<usize>,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    if let [(path, value)] = branches {
        return compile_path(path, value, form, catalog, root_symbol, namespace);
    }
    let clauses = branches
        .iter()
        .enumerate()
        .map(|(index, (path, value))| {
            namespace.push(index);
            let result = compile_path(path, value, form, catalog, root_symbol, namespace);
            namespace.pop();
            result.map(|clauses| match clauses.as_slice() {
                [clause] => clause.clone(),
                _ => NormalizedClause::And { clauses },
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(vec![NormalizedClause::Or {
        join_symbol: root_symbol.clone(),
        clauses,
    }])
}

fn compile_branches(
    conditions: &[WhereCondition],
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
    namespace: &mut Vec<usize>,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    conditions
        .iter()
        .enumerate()
        .map(|(index, condition)| {
            namespace.push(index);
            let result = compile_condition(condition, form, catalog, root_symbol, namespace);
            namespace.pop();
            result
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|groups| groups.into_iter().flatten().collect())
}

#[derive(Clone)]
struct AttrPattern<'a> {
    entity: RawTerm,
    attribute: &'a Attribute,
    value: RawTerm,
}

#[allow(clippy::too_many_lines)] // Keeps relationship traversal and terminal pattern lowering together.
fn compile_path(
    path: &[String],
    value: &WhereValue,
    form: &Form,
    catalog: &AttributeCatalog,
    root_symbol: &Symbol,
    namespace: &[usize],
) -> Result<Vec<NormalizedClause>, ValidationError> {
    let (value_label, refs) = path
        .split_last()
        .ok_or_else(|| missing_attribute(&form.etype, "", form, path))?;
    let mut current_etype = form.etype.as_str();
    let mut current_level = form.level;
    let mut current_symbol = root_symbol.clone();
    let mut attr_patterns = Vec::new();
    for label in refs {
        let (attribute, forward) = resolve_attribute(catalog, current_etype, label)
            .ok_or_else(|| missing_attribute(current_etype, label, form, path))?;
        if attribute.value_type != AttributeValueType::Ref {
            return Err(validation(
                "link?",
                &where_path(form, path),
                format!("{current_etype}.{label} needs to be a link"),
            ));
        }
        let next_etype = if forward {
            attribute.reverse_etype.as_deref()
        } else {
            Some(attribute.etype.as_str())
        }
        .ok_or_else(|| {
            validation(
                "link?",
                &where_path(form, path),
                "Link identity is incomplete.",
            )
        })?;
        current_level += 1;
        let next_symbol = namespaced_symbol(next_etype, current_level, namespace, root_symbol)?;
        attr_patterns.push(if forward {
            AttrPattern {
                entity: RawTerm::Variable(current_symbol),
                attribute,
                value: RawTerm::Variable(next_symbol.clone()),
            }
        } else {
            AttrPattern {
                entity: RawTerm::Variable(next_symbol.clone()),
                attribute,
                value: RawTerm::Variable(current_symbol),
            }
        });
        current_etype = next_etype;
        current_symbol = next_symbol;
    }
    if value_label == "$entityIdStartsWith" {
        append_entity_prefix(
            &mut attr_patterns,
            value,
            (current_etype, current_symbol),
            form,
            catalog,
            path,
        )?;
        optimize_attr_patterns(&mut attr_patterns);
        return attr_patterns_to_clauses(attr_patterns);
    }
    let (attribute, forward) = resolve_attribute(catalog, current_etype, value_label)
        .ok_or_else(|| missing_attribute(current_etype, value_label, form, path))?;
    if matches!(value, WhereValue::IsNull(_)) {
        let id_attr = catalog
            .forward(current_etype, "id")
            .ok_or_else(|| missing_attribute(current_etype, "id", form, path))?;
        let WhereValue::IsNull(nil) = value else {
            unreachable!();
        };
        attr_patterns.push(AttrPattern {
            entity: RawTerm::Variable(current_symbol),
            attribute: id_attr,
            value: RawTerm::Function(FunctionClause::IsNull(IsNullClause {
                attr_id: attribute.id,
                nil: *nil,
                reference: attribute.value_type == AttributeValueType::Ref,
                reverse: !forward,
                indexed: attribute.indexed && !attribute.indexing,
                indexed_checked_type: (attribute.indexed && !attribute.indexing)
                    .then_some(attribute.checked_data_type)
                    .flatten(),
            })),
        });
    } else {
        let term = value_term(value, attribute, form, path)?;
        attr_patterns.push(
            if attribute.value_type == AttributeValueType::Ref && !forward {
                AttrPattern {
                    entity: term,
                    attribute,
                    value: RawTerm::Variable(current_symbol),
                }
            } else {
                AttrPattern {
                    entity: RawTerm::Variable(current_symbol),
                    attribute,
                    value: term,
                }
            },
        );
    }
    optimize_attr_patterns(&mut attr_patterns);
    attr_patterns_to_clauses(attr_patterns)
}

fn append_entity_prefix<'a>(
    patterns: &mut Vec<AttrPattern<'a>>,
    value: &WhereValue,
    target: (&str, Symbol),
    form: &Form,
    catalog: &'a AttributeCatalog,
    path: &[String],
) -> Result<(), ValidationError> {
    let (etype, entity) = target;
    let WhereValue::EntityIdStartsWith(prefix) = value else {
        unreachable!("entity prefix is coerced by terminal path segment");
    };
    let id_attr = catalog
        .forward(etype, "id")
        .ok_or_else(|| missing_attribute(etype, "id", form, path))?;
    patterns.push(AttrPattern {
        entity: RawTerm::Variable(entity),
        attribute: id_attr,
        value: RawTerm::Function(FunctionClause::EntityIdStartsWith(prefix.clone())),
    });
    Ok(())
}

fn resolve_attribute<'a>(
    catalog: &'a AttributeCatalog,
    etype: &str,
    label: &str,
) -> Option<(&'a Attribute, bool)> {
    catalog
        .forward(etype, label)
        .map(|attribute| (attribute, true))
        .or_else(|| {
            catalog
                .reverse(etype, label)
                .map(|attribute| (attribute, false))
        })
}

fn path_attribute<'a>(
    path: &[String],
    initial_etype: &str,
    catalog: &'a AttributeCatalog,
) -> Option<&'a Attribute> {
    let (last, refs) = path.split_last()?;
    let mut etype = initial_etype;
    for label in refs {
        let (attribute, forward) = resolve_attribute(catalog, etype, label)?;
        if attribute.value_type != AttributeValueType::Ref {
            return None;
        }
        etype = if forward {
            attribute.reverse_etype.as_deref()?
        } else {
            &attribute.etype
        };
    }
    resolve_attribute(catalog, etype, last).map(|(attribute, _)| attribute)
}

fn value_term(
    value: &WhereValue,
    attribute: &Attribute,
    form: &Form,
    path: &[String],
) -> Result<RawTerm, ValidationError> {
    if attribute.value_type == AttributeValueType::Ref {
        return reference_term(value, attribute, form, path);
    }
    if let WhereValue::In(values) = value
        && values.iter().any(Value::is_null)
        && (!attribute.indexed || attribute.indexing)
    {
        return Err(validation(
            "indexed?",
            &where_path(form, path),
            format!(
                "Only indexed attrs can check for `nil` in `$in`. {}.{} is not indexed.",
                attribute.etype, attribute.label
            ),
        ));
    }
    match value {
        WhereValue::Scalar(value) => Ok(RawTerm::Constant(checked_value(
            value, attribute, None, form, path,
        )?)),
        WhereValue::In(values) => Ok(RawTerm::Constants(
            values
                .iter()
                .map(|value| checked_value(value, attribute, None, form, path))
                .collect::<Result<_, _>>()?,
        )),
        WhereValue::Not(value) => Ok(RawTerm::Function(FunctionClause::Not(checked_value(
            value, attribute, None, form, path,
        )?))),
        WhereValue::Comparator { operation, value } => {
            validate_comparator(attribute, *operation, value, form, path)?;
            Ok(RawTerm::Function(FunctionClause::Comparator(
                ComparatorClause {
                    operation: (*operation).into(),
                    data_type: attribute
                        .checked_data_type
                        .expect("comparison validation requires a type"),
                    value: checked_value(value, attribute, Some(*operation), form, path)?,
                },
            )))
        }
        WhereValue::IsNull(_) | WhereValue::EntityIdStartsWith(_) => unreachable!(),
    }
}

fn reference_term(
    value: &WhereValue,
    attribute: &Attribute,
    form: &Form,
    path: &[String],
) -> Result<RawTerm, ValidationError> {
    let uuid_value = |value: &Value| {
        value
            .as_str()
            .and_then(coerce_public_uuid)
            .map(|value| Value::String(value.to_string()))
            .ok_or_else(|| {
                validation(
                    "uuid?",
                    &where_path(form, path),
                    format!("Expected {} to be a uuid, got {value}", attribute.label),
                )
            })
    };
    match value {
        WhereValue::Scalar(value) => Ok(RawTerm::Constant(uuid_value(value)?)),
        WhereValue::In(values) => Ok(RawTerm::Constants(
            values.iter().map(uuid_value).collect::<Result<_, _>>()?,
        )),
        WhereValue::Not(value) => Ok(RawTerm::Function(FunctionClause::Not(uuid_value(value)?))),
        WhereValue::IsNull(_)
        | WhereValue::Comparator { .. }
        | WhereValue::EntityIdStartsWith(_) => Err(validation(
            "supported-ref-operator?",
            &where_path(form, path),
            "Reference attributes support equality, `$in`, `$not`, and `$isNull`.",
        )),
    }
}

fn checked_value(
    value: &Value,
    attribute: &Attribute,
    comparator: Option<WhereComparator>,
    form: &Form,
    path: &[String],
) -> Result<Value, ValidationError> {
    let Some(data_type) = attribute.checked_data_type else {
        return Ok(value.clone());
    };
    if attribute.checking_data_type {
        return Ok(value.clone());
    }
    if value.is_null() {
        return Ok(Value::Null);
    }
    let valid = match data_type {
        DataType::String => value.is_string(),
        DataType::Number => value.is_number(),
        DataType::Boolean => value.is_boolean(),
        DataType::Date => value.is_number() || value.is_string(),
    };
    let condition_path = comparator.map_or_else(
        || where_path(form, path),
        |operation| {
            appended(
                &where_path(form, path),
                comparator_name(operation).to_owned(),
            )
        },
    );
    if !valid {
        return Err(validation(
            format!("{}?", data_type_name(data_type)),
            &condition_path,
            format!(
                "The data type of `{}.{}` is `{}`, but the query got the value `{value}` of type `{}`.",
                attribute.etype,
                attribute.label,
                data_type_name(data_type),
                json_type(value)
            ),
        ));
    }
    if data_type == DataType::Date && (attribute.indexed || comparator.is_some()) {
        // The storage/date decoder accepts relative words for legacy values,
        // but the public query contract deliberately rejects them because the
        // client and server cannot agree on one stable comparison instant.
        let relative = matches!(
            value.as_str(),
            Some("tomorrow" | "now" | "yesterday" | "today")
        );
        return (!relative)
            .then(|| date_value(value))
            .flatten()
            .ok_or_else(|| {
            validation(
                if value.is_string() {
                    "date-string?"
                } else {
                    "timestamp?"
                },
                &condition_path,
                format!(
                    "The data type of `{}.{}` is `date`, but the query got value `{value}` of type `{}`.",
                    attribute.etype,
                    attribute.label,
                    json_type(value)
                ),
            )
        });
    }
    Ok(value.clone())
}

fn validate_comparator(
    attribute: &Attribute,
    operation: WhereComparator,
    value: &Value,
    form: &Form,
    path: &[String],
) -> Result<(), ValidationError> {
    let condition_path = where_path(form, path);
    let friendly = format!("{}.{}", attribute.etype, attribute.label);
    let error = if attribute.checking_data_type {
        Some((
            "checked-data-type?",
            format!(
                "The `{friendly}` attribute is still in the process of checking its data type. It must finish before using comparison operators."
            ),
        ))
    } else if attribute.indexing {
        Some((
            "indexed?",
            format!(
                "The `{friendly}` attribute is still in the process of indexing. It must finish before using comparison operators."
            ),
        ))
    } else if !attribute.indexed {
        Some((
            "indexed?",
            format!("The `{friendly}` attribute must be indexed to use comparison operators."),
        ))
    } else if attribute.checked_data_type.is_none() {
        Some((
            "checked-data-type?",
            format!(
                "The `{friendly}` attribute must have an enforced type to use comparison operators."
            ),
        ))
    } else {
        None
    };
    if let Some((expected, message)) = error {
        return Err(validation(expected, &condition_path, message));
    }
    if matches!(
        operation,
        WhereComparator::Like | WhereComparator::CaseInsensitiveLike
    ) && !value.is_string()
    {
        return Err(validation(
            "string?",
            &appended(&condition_path, comparator_name(operation).to_owned()),
            format!(
                "The $like value for `{friendly}` must be a string, but the query got the value `{value}` of type `{}`.",
                json_type(value)
            ),
        ));
    }
    Ok(())
}

const fn comparator_name(operation: WhereComparator) -> &'static str {
    match operation {
        WhereComparator::GreaterThan => "$gt",
        WhereComparator::GreaterThanOrEqual => "$gte",
        WhereComparator::LessThan => "$lt",
        WhereComparator::LessThanOrEqual => "$lte",
        WhereComparator::Like => "$like",
        WhereComparator::CaseInsensitiveLike => "$ilike",
    }
}

fn optimize_attr_patterns(patterns: &mut [AttrPattern<'_>]) {
    if patterns.len() <= 1 || actualized(&patterns[0].entity) || actualized(&patterns[0].value) {
        return;
    }
    if let Some(last) = patterns.last()
        && (actualized(&last.entity) || actualized(&last.value))
    {
        patterns.reverse();
    }
}

fn attr_patterns_to_clauses(
    patterns: Vec<AttrPattern<'_>>,
) -> Result<Vec<NormalizedClause>, ValidationError> {
    let mut seen = HashSet::new();
    patterns
        .into_iter()
        .map(|pattern| {
            let value_actualized = term_actualized(&pattern.value, &seen);
            let index = best_index(pattern.attribute, value_actualized);
            for term in [&pattern.entity, &pattern.value] {
                if let RawTerm::Variable(symbol) = term {
                    seen.insert(symbol.clone());
                }
            }
            RawPattern::new(
                index,
                vec![
                    pattern.entity,
                    RawTerm::Constant(Value::String(pattern.attribute.id.to_string())),
                    pattern.value,
                ],
            )
            .normalize()
            .map(|pattern| NormalizedClause::Pattern { pattern })
            .map_err(|error| validation("valid-pattern?", &[], error.to_string()))
        })
        .collect()
}

fn best_index(attribute: &Attribute, value_actualized: bool) -> Index {
    if !value_actualized {
        return Index::new(if attribute.value_type == AttributeValueType::Ref {
            IndexKey::Vae
        } else {
            IndexKey::Ea
        });
    }
    if attribute.value_type == AttributeValueType::Ref {
        Index::new(IndexKey::Vae)
    } else if attribute.indexed
        && !attribute.indexing
        && !attribute.checking_data_type
        && attribute.checked_data_type.is_some()
    {
        Index::checked(
            IndexKey::Ave,
            attribute.checked_data_type.expect("checked above"),
        )
    } else if attribute.unique && !attribute.setting_unique {
        Index::new(IndexKey::Av)
    } else if attribute.indexed && !attribute.indexing {
        Index::new(IndexKey::Ave)
    } else {
        Index::new(IndexKey::Ea)
    }
}

fn actualized(term: &RawTerm) -> bool {
    !matches!(term, RawTerm::Any | RawTerm::Variable(_))
        && !matches!(term, RawTerm::Function(FunctionClause::IsNull(_)))
}

fn term_actualized(term: &RawTerm, seen: &HashSet<Symbol>) -> bool {
    actualized(term) || matches!(term, RawTerm::Variable(symbol) if seen.contains(symbol))
}

fn namespaced_symbol(
    etype: &str,
    level: usize,
    namespace: &[usize],
    root_symbol: &Symbol,
) -> Result<Symbol, ValidationError> {
    let base = format!("?{etype}-{level}");
    if base == root_symbol.as_str() || namespace.is_empty() {
        return Symbol::new(base)
            .map_err(|error| validation("valid-symbol?", &[], error.to_string()));
    }
    let suffix = namespace
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join("-");
    Symbol::new(format!("{base}-{suffix}"))
        .map_err(|error| validation("valid-symbol?", &[], error.to_string()))
}

fn date_value(value: &Value) -> Option<Value> {
    match crate::dates::parse_date_value(value, Utc::now())? {
        crate::dates::DateValue::Finite(instant) => {
            Some(Value::String(crate::dates::render_instant(instant)))
        }
        crate::dates::DateValue::NegativeInfinity => Some(Value::String("-infinity".to_owned())),
        crate::dates::DateValue::PositiveInfinity => Some(Value::String("infinity".to_owned())),
    }
}

const fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::String => "string",
        DataType::Number => "number",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn missing_attribute(etype: &str, label: &str, form: &Form, path: &[String]) -> ValidationError {
    validation(
        "known-attribute?",
        &where_path(form, path),
        format!("There is no `{etype}.{label}` attribute."),
    )
}

fn where_path(form: &Form, path: &[String]) -> Vec<String> {
    [form.key.clone(), "$".to_owned(), "where".to_owned()]
        .into_iter()
        .chain(path.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_attribute(label: &str, indexed: bool) -> Attribute {
        Attribute {
            id: Uuid::new_v4(),
            wire: None,
            etype: "users".to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: super::super::Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    #[test]
    fn coerces_boolean_groups_and_operator_aliases() {
        let catalog = AttributeCatalog { attributes: vec![] };
        let conditions = coerce_where(
            &serde_json::json!({
                "and": [
                    {"handle": {"$ne": "alex"}},
                    {"or": [{"age": {"$gte": 18}}, {"age": null}]}
                ]
            }),
            &["users".to_owned(), "$".to_owned(), "where".to_owned()],
            "users",
            &catalog,
            &[],
        );
        assert!(
            conditions.is_err(),
            "plain null is not a valid equality value"
        );

        let conditions = coerce_where(
            &serde_json::json!({"handle": {"$in": ["alex", null]}}),
            &[],
            "users",
            &catalog,
            &[],
        )
        .unwrap();
        assert!(matches!(
            conditions.as_slice(),
            [WhereCondition::Condition {
                value: WhereValue::In(values),
                ..
            }] if values.len() == 2
        ));
    }

    #[test]
    fn combines_scalar_or_branches_into_in_conditions_like_the_jvm() {
        let catalog = AttributeCatalog {
            attributes: vec![test_attribute("age", true), test_attribute("name", false)],
        };
        let form = Form {
            key: "users".to_owned(),
            etype: "users".to_owned(),
            level: 0,
            options: super::super::FormOptions::default(),
            children: Vec::new(),
        };
        let conditions = vec![
            WhereCondition::Condition {
                path: vec!["age".to_owned()],
                value: WhereValue::Scalar(serde_json::json!(1)),
            },
            WhereCondition::Condition {
                path: vec!["age".to_owned()],
                value: WhereValue::IsNull(true),
            },
            WhereCondition::Condition {
                path: vec!["name".to_owned()],
                value: WhereValue::IsNull(true),
            },
        ];
        assert_eq!(
            combine_or_conditions(&conditions, &form, &catalog),
            vec![
                WhereCondition::Condition {
                    path: vec!["name".to_owned()],
                    value: WhereValue::IsNull(true),
                },
                WhereCondition::Condition {
                    path: vec!["age".to_owned()],
                    value: WhereValue::In(vec![serde_json::json!(1), Value::Null]),
                },
            ]
        );
    }

    #[test]
    fn removes_duplicate_conditions_before_symbol_namespacing() {
        let condition = WhereCondition::Condition {
            path: vec!["handle".to_owned()],
            value: WhereValue::Scalar(serde_json::json!("alex")),
        };
        assert_eq!(
            dedupe_conditions(&[
                condition.clone(),
                condition.clone(),
                WhereCondition::And {
                    conditions: vec![condition.clone(), condition.clone()],
                },
            ]),
            vec![
                condition.clone(),
                WhereCondition::And {
                    conditions: vec![condition],
                },
            ]
        );
    }
}
