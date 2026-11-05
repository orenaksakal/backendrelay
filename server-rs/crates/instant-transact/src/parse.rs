use instant_datalog::DataType;
use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality, DeleteAction};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{
    AttributeDefinition, AttributeUpdate, DeleteEntityStep, EntityLookup, Identity, LookupRef,
    PathSegment, RuleParamsStep, TransactionError, TransactionMode, TransactionStep, TripleOptions,
    TripleStep, optimistic_attribute_from_update,
};

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledTransaction {
    pub source_steps: Vec<TransactionStep>,
    pub grouped_steps: Vec<TransactionStep>,
}

/// Coerces, validates, expands, and mapifies public transaction steps.
///
/// Schema mutations are applied optimistically before object-form expansion,
/// matching the JVM's ability to add an attribute and use it in the same transaction.
///
/// # Errors
///
/// Returns [`TransactionError`] with a public-input path for malformed steps,
/// invalid UUIDs, unknown object-form fields, invalid ID lookups, and reference
/// lookup namespace mismatches.
pub fn compile_transaction(
    input: &Value,
    catalog: &AttributeCatalog,
) -> Result<Vec<TransactionStep>, TransactionError> {
    compile_transaction_all_errors(input, catalog).map_err(|errors| {
        errors
            .into_iter()
            .next()
            .unwrap_or_else(|| error([], "transaction validation failed"))
    })
}

/// Compiles one transaction into source-ordered permission steps and
/// operation-grouped execution steps in a single optimistic-schema pass.
///
/// # Errors
///
/// Returns the first validation issue in public input order.
pub fn compile_transaction_with_order(
    input: &Value,
    catalog: &AttributeCatalog,
) -> Result<CompiledTransaction, TransactionError> {
    compile_transaction_with_order_all_errors(input, catalog).map_err(|errors| {
        errors
            .into_iter()
            .next()
            .unwrap_or_else(|| error([], "transaction validation failed"))
    })
}

/// Coerces, validates, expands, and mapifies public transaction steps while
/// retaining every independently discoverable validation issue.
///
/// Issues retain input order and exact nested paths. This mirrors the useful
/// multi-problem surface of Clojure spec's `explain-data`, while
/// [`compile_transaction`] remains the backwards-compatible first-error API.
///
/// # Errors
///
/// Returns every malformed predicate discovered without attempting database
/// I/O. Issues are ordered first by input step, then by tuple position or the
/// stable attribute-schema field order used by the validator.
pub fn compile_transaction_all_errors(
    input: &Value,
    catalog: &AttributeCatalog,
) -> Result<Vec<TransactionStep>, Vec<TransactionError>> {
    compile_transaction_with_order_all_errors(input, catalog).map(|compiled| compiled.grouped_steps)
}

fn compile_transaction_with_order_all_errors(
    input: &Value,
    catalog: &AttributeCatalog,
) -> Result<CompiledTransaction, Vec<TransactionError>> {
    let raw_steps = input.as_array().ok_or_else(|| vec![error([], "coll?")])?;
    let mut optimistic = catalog.clone();
    let mut schema_errors = vec![Vec::new(); raw_steps.len()];
    for (index, raw) in raw_steps.iter().enumerate() {
        if let Some(step) = raw.as_array()
            && is_schema_mutation(step)
        {
            let issues = validate_step_all(step, index, &optimistic);
            if issues.is_empty() {
                // Validation just proved this parse succeeds. Keep this guard so
                // a future parser/validator drift is still reported safely.
                if let Err(issue) = apply_optimistic_schema(step, index, &mut optimistic) {
                    schema_errors[index].push(issue);
                }
            } else {
                schema_errors[index] = issues;
            }
        }
    }
    let mut compiled = Vec::new();
    let mut errors = Vec::new();
    for (index, raw) in raw_steps.iter().enumerate() {
        if !schema_errors[index].is_empty() {
            errors.append(&mut schema_errors[index]);
            continue;
        }
        let (issues, result) = if let Some(object) = raw.as_object() {
            let issues = validate_object_all(object, index, &optimistic);
            let result = issues
                .is_empty()
                .then(|| expand_object(object, index, &optimistic))
                .transpose();
            (issues, result.map(Option::unwrap_or_default))
        } else {
            match raw.as_array() {
                Some(step) => {
                    let issues = validate_step_all(step, index, &optimistic);
                    let result = issues
                        .is_empty()
                        .then(|| parse_step(step, index, &optimistic).map(|step| vec![step]))
                        .transpose();
                    (issues, result.map(Option::unwrap_or_default))
                }
                None => (vec![error([index.into()], "coll?")], Ok(Vec::new())),
            }
        };
        if issues.is_empty() {
            match result {
                Ok(steps) => compiled.extend(steps),
                Err(issue) => errors.push(issue),
            }
        } else {
            errors.extend(issues);
        }
    }
    if errors.is_empty() {
        Ok(CompiledTransaction {
            grouped_steps: group_transaction_steps(&compiled),
            source_steps: compiled,
        })
    } else {
        Err(errors)
    }
}

fn is_schema_mutation(step: &[Value]) -> bool {
    step.first()
        .and_then(Value::as_str)
        .map(normalize_op)
        .is_some_and(|op| matches!(op.as_str(), "add-attr" | "update-attr" | "delete-attr"))
}

fn push_issue<T>(result: Result<T, TransactionError>, errors: &mut Vec<TransactionError>) {
    if let Err(issue) = result {
        errors.push(issue);
    }
}

fn validate_step_all(
    step: &[Value],
    index: usize,
    catalog: &AttributeCatalog,
) -> Vec<TransactionError> {
    if step.is_empty() {
        return vec![error([index.into()], "#{:restore-attr}")];
    }
    let Some(op) = step.first().and_then(Value::as_str).map(normalize_op) else {
        return vec![error(
            [index.into(), 0.into()],
            "expected a transaction operation",
        )];
    };
    let bounds = match op.as_str() {
        "add-triple" | "deep-merge-triple" => (4, 5),
        "retract-triple" => (4, 4),
        // `coerce-attr-args` rebuilds add/update tuples from their first two
        // elements, intentionally dropping forward-compatible trailing input.
        "add-attr" | "update-attr" => (2, usize::MAX),
        "delete-attr" | "restore-attr" => (2, 2),
        "delete-entity" => (2, 3),
        "rule-params" => (3, 4),
        _ => {
            return vec![error([index.into(), 0.into()], "#{:restore-attr}")];
        }
    };
    // These are the exact predicates selected by the JVM's `best-problem`
    // heuristic for the generic spec failures pinned in transaction_test.clj.
    if matches!(
        op.as_str(),
        "add-triple" | "deep-merge-triple" | "retract-triple"
    ) && step.len() < 4
    {
        let predicate = match step.len() {
            1 => ":instant.db.model.triple/lookup",
            2 => ":instant.db.model.triple/attr-id",
            3 => ":instant.db.model.triple/value",
            _ => unreachable!("the operation occupies the first tuple position"),
        };
        return vec![error([index.into()], predicate)];
    }
    if op == "add-attr" && step.len() == 1 {
        return vec![error(
            [index.into(), 1.into()],
            "instant.db.model.attr/value-type",
        )];
    }
    if op == "update-attr" && step.len() == 1 {
        return vec![error([index.into(), 1.into()], "map?")];
    }
    if require_len(step, index, bounds.0, bounds.1).is_err() {
        let predicate = if op == "restore-attr" {
            "#{:delete-attr}"
        } else {
            "#{:restore-attr}"
        };
        return vec![error([index.into(), 0.into()], predicate)];
    }

    let mut errors = Vec::new();
    match op.as_str() {
        "add-triple" | "deep-merge-triple" | "retract-triple" => {
            push_issue(entity_at(&step[1], index, 1, catalog), &mut errors);
            let attr_id = uuid_at(&step[2], [index.into(), 2.into()]);
            let attribute = attr_id.as_ref().ok().and_then(|id| catalog.by_id(*id));
            if let Err(issue) = attr_id {
                errors.push(issue);
            }
            push_issue(
                validate_value_lookup(&step[3], attribute, index, catalog),
                &mut errors,
            );
            if op != "retract-triple"
                && let Some(options) = step.get(4)
            {
                push_issue(parse_options(options, index), &mut errors);
            }
        }
        "add-attr" => errors.extend(validate_attribute_all(&step[1], index)),
        "update-attr" => errors.extend(validate_attribute_update_all(&step[1], index)),
        "delete-attr" | "restore-attr" => {
            push_issue(uuid_at(&step[1], [index.into(), 1.into()]), &mut errors);
        }
        "delete-entity" => {
            push_issue(entity_at(&step[1], index, 1, catalog), &mut errors);
            if let Some(etype) = step.get(2) {
                push_issue(string_at(etype, index, 2), &mut errors);
            }
        }
        "rule-params" => {
            push_issue(rule_lookup_at(&step[1], index, 1), &mut errors);
            let params_index = if step.len() == 4 {
                push_issue(string_at(&step[2], index, 2), &mut errors);
                3
            } else {
                2
            };
            if !step[params_index].is_object() {
                errors.push(error([index.into(), params_index.into()], "map?"));
            }
        }
        _ => unreachable!("operation was exhaustively classified above"),
    }
    errors
}

fn validate_object_all(
    object: &Map<String, Value>,
    index: usize,
    catalog: &AttributeCatalog,
) -> Vec<TransactionError> {
    let mut errors = Vec::new();
    let etype = if let Some(etype) = object.get("etype").and_then(Value::as_str) {
        Some(etype)
    } else {
        errors.push(error(
            [index.into(), "etype".into()],
            "expected an entity type",
        ));
        None
    };
    match object.get("id") {
        Some(entity) => push_issue(entity_at(entity, index, 1, catalog), &mut errors),
        None => errors.push(error([index.into(), "id".into()], "expected an entity ID")),
    }
    let Some(etype) = etype else {
        return errors;
    };
    for (label, value) in object.iter().filter(|(label, _)| label.as_str() != "etype") {
        let Some(attr_id) = parse_uuid(label)
            .or_else(|| object_attribute(catalog, etype, label).map(|attribute| attribute.id))
        else {
            errors.push(error(
                [index.into(), label.clone().into()],
                format!("unknown attribute `{etype}.{label}`"),
            ));
            continue;
        };
        push_issue(
            validate_value_lookup(value, catalog.by_id(attr_id), index, catalog),
            &mut errors,
        );
    }
    errors
}

#[must_use]
pub fn group_transaction_steps(steps: &[TransactionStep]) -> Vec<TransactionStep> {
    let mut order = Vec::new();
    for step in steps {
        if !order.contains(&step.operation()) {
            order.push(step.operation());
        }
    }
    order
        .into_iter()
        .flat_map(|operation| {
            steps
                .iter()
                .filter(move |step| step.operation() == operation)
                .cloned()
        })
        .collect()
}

fn apply_optimistic_schema(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
) -> Result<(), TransactionError> {
    let Some(op) = step.first().and_then(Value::as_str).map(normalize_op) else {
        return Ok(());
    };
    match op.as_str() {
        "add-attr" => {
            if let Some(value) = step.get(1) {
                let definition = parse_attribute(value, index)?;
                catalog
                    .attributes
                    .retain(|attribute| attribute.id != definition.attribute.id);
                catalog.attributes.push(definition.attribute);
            }
        }
        "update-attr" => {
            if let Some(value) = step.get(1) {
                let update = parse_attribute_update(value, index)?;
                // The JVM's `optimistic-attrs` uses `assoc!`, so a partial
                // update replaces (rather than merges with) the prior catalog
                // entry for preprocessing. Preserve only fields carried by the
                // update; the persisted update step remains partial.
                catalog
                    .attributes
                    .retain(|attribute| attribute.id != update.id);
                catalog
                    .attributes
                    .push(optimistic_attribute_from_update(&update));
            }
        }
        "delete-attr" => {
            if let Some(id) = step.get(1).and_then(Value::as_str).and_then(parse_uuid) {
                catalog.attributes.retain(|attribute| attribute.id != id);
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_step(
    step: &[Value],
    index: usize,
    catalog: &AttributeCatalog,
) -> Result<TransactionStep, TransactionError> {
    let op = step
        .first()
        .and_then(Value::as_str)
        .map(normalize_op)
        .ok_or_else(|| error([index.into(), 0.into()], "expected a transaction operation"))?;
    match op.as_str() {
        "add-triple" | "deep-merge-triple" => {
            require_len(step, index, 4, 5)?;
            let triple = parse_triple(step, index, catalog, true)?;
            Ok(if op == "add-triple" {
                TransactionStep::AddTriple(triple)
            } else {
                TransactionStep::DeepMergeTriple(triple)
            })
        }
        "retract-triple" => {
            require_len(step, index, 4, 4)?;
            Ok(TransactionStep::RetractTriple(parse_triple(
                step, index, catalog, false,
            )?))
        }
        "add-attr" => {
            require_len(step, index, 2, usize::MAX)?;
            Ok(TransactionStep::AddAttr(parse_attribute(&step[1], index)?))
        }
        "update-attr" => {
            require_len(step, index, 2, usize::MAX)?;
            Ok(TransactionStep::UpdateAttr(parse_attribute_update(
                &step[1], index,
            )?))
        }
        "delete-attr" | "restore-attr" => {
            require_len(step, index, 2, 2)?;
            let id = uuid_at(&step[1], [index.into(), 1.into()])?;
            Ok(if op == "delete-attr" {
                TransactionStep::DeleteAttr(id)
            } else {
                TransactionStep::RestoreAttr(id)
            })
        }
        "delete-entity" => {
            require_len(step, index, 2, 3)?;
            Ok(TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: entity_at(&step[1], index, 1, catalog)?,
                etype: step
                    .get(2)
                    .map(|value| string_at(value, index, 2))
                    .transpose()?,
            }))
        }
        "rule-params" => parse_rule_params(step, index).map(TransactionStep::RuleParams),
        _ => Err(error(
            [index.into(), 0.into()],
            format!("unknown transaction operation `{op}`"),
        )),
    }
}

fn parse_triple(
    step: &[Value],
    index: usize,
    catalog: &AttributeCatalog,
    options_allowed: bool,
) -> Result<TripleStep, TransactionError> {
    let entity = entity_at(&step[1], index, 1, catalog)?;
    let attr_id = uuid_at(&step[2], [index.into(), 2.into()])?;
    let attribute = catalog.by_id(attr_id);
    validate_value_lookup(&step[3], attribute, index, catalog)?;
    let options = if options_allowed {
        step.get(4)
            .map(|value| parse_options(value, index))
            .transpose()?
            .unwrap_or_default()
    } else {
        TripleOptions::default()
    };
    Ok(TripleStep {
        entity,
        // A partial optimistic update is a replacement map upstream, so a
        // missing forward identity maps to nil metadata rather than the prior
        // catalog namespace.
        etype: attribute
            .and_then(|attribute| (!attribute.etype.is_empty()).then(|| attribute.etype.clone())),
        attr_id,
        reverse_etype: attribute.and_then(|attribute| attribute.reverse_etype.clone()),
        value: step[3].clone(),
        options,
    })
}

fn parse_rule_params(step: &[Value], index: usize) -> Result<RuleParamsStep, TransactionError> {
    require_len(step, index, 3, 4)?;
    let (etype, params_index) = if step.len() == 4 {
        (Some(string_at(&step[2], index, 2)?), 3)
    } else {
        (None, 2)
    };
    let params = step[params_index]
        .as_object()
        .cloned()
        .ok_or_else(|| error([index.into(), params_index.into()], "map?"))?;
    Ok(RuleParamsStep {
        entity: rule_lookup_at(&step[1], index, 1)?,
        etype,
        params,
    })
}

fn expand_object(
    object: &Map<String, Value>,
    index: usize,
    catalog: &AttributeCatalog,
) -> Result<Vec<TransactionStep>, TransactionError> {
    let etype = object
        .get("etype")
        .and_then(Value::as_str)
        .ok_or_else(|| error([index.into(), "etype".into()], "expected an entity type"))?;
    let entity_value = object
        .get("id")
        .ok_or_else(|| error([index.into(), "id".into()], "expected an entity ID"))?;
    let entity = entity_at(entity_value, index, 1, catalog)?;
    object
        .iter()
        .filter(|(label, _)| label.as_str() != "etype")
        .map(|(label, value)| {
            let attr_id = parse_uuid(label)
                .or_else(|| object_attribute(catalog, etype, label).map(|attribute| attribute.id))
                .ok_or_else(|| {
                    error(
                        [index.into(), label.clone().into()],
                        format!("unknown attribute `{etype}.{label}`"),
                    )
                })?;
            let attribute = catalog.by_id(attr_id);
            validate_value_lookup(value, attribute, index, catalog)?;
            Ok(TransactionStep::AddTriple(TripleStep {
                entity: entity.clone(),
                etype: attribute.map(|attribute| attribute.etype.clone()),
                attr_id,
                reverse_etype: attribute.and_then(|attribute| attribute.reverse_etype.clone()),
                value: value.clone(),
                options: TripleOptions::default(),
            }))
        })
        .collect()
}

fn object_attribute<'a>(
    catalog: &'a AttributeCatalog,
    etype: &str,
    label: &str,
) -> Option<&'a Attribute> {
    // `attr-model/resolve-attr-id` checks the forward identity first and then
    // the reverse identity. Keep validation and expansion on this shared path
    // so a collision cannot resolve differently between the two passes.
    catalog
        .forward(etype, label)
        .or_else(|| catalog.reverse(etype, label))
}

fn parse_options(value: &Value, index: usize) -> Result<TripleOptions, TransactionError> {
    if value.is_null() {
        return Ok(TripleOptions::default());
    }
    let object = value
        .as_object()
        .ok_or_else(|| error([index.into(), 4.into()], "map?"))?;
    let mode = object
        .get("mode")
        .map(|mode| {
            let mode = mode.as_str().map(normalize_op).ok_or_else(|| {
                error(
                    [index.into(), 4.into(), "mode".into()],
                    "#{:create :update :upsert}",
                )
            })?;
            match mode.as_str() {
                "create" => Ok(TransactionMode::Create),
                "update" => Ok(TransactionMode::Update),
                "upsert" => Ok(TransactionMode::Upsert),
                _ => Err(error(
                    [index.into(), 4.into(), "mode".into()],
                    "#{:create :update :upsert}",
                )),
            }
        })
        .transpose()?;
    Ok(TripleOptions { mode })
}

fn entity_at(
    value: &Value,
    index: usize,
    position: usize,
    catalog: &AttributeCatalog,
) -> Result<EntityLookup, TransactionError> {
    if let Some(id) = value.as_str().and_then(parse_uuid) {
        return Ok(EntityLookup::Id(id));
    }
    if let Some(values) = value.as_array() {
        // `check-for-invalid-entity-ids!` runs before spec validation and
        // checks any sequential value as a possible id lookup, even when its
        // tuple arity is malformed.
        if let Some(attr_id) = values.first().and_then(Value::as_str).and_then(parse_uuid)
            && let Some(attribute) = catalog
                .by_id(attr_id)
                .filter(|attribute| attribute.label == "id")
            && values
                .get(1)
                .and_then(Value::as_str)
                .and_then(parse_uuid)
                .is_none()
        {
            return Err(error(
                [index.into(), position.into()],
                format!(
                    "Invalid lookup '{}'. The lookup attribute is '{}.{}', but the lookup value is not a valid UUID.",
                    clojure_value_display(value),
                    attribute.etype,
                    attribute.label
                ),
            ));
        }
        return lookup_values_at(values, index, position);
    }
    Err(error(
        [index.into(), position.into()],
        format!(
            "Invalid entity ID '{}'. Entity IDs must be UUIDs. Use id() or lookup() to generate a valid UUID.",
            display_value(value)
        ),
    ))
}

fn rule_lookup_at(
    value: &Value,
    index: usize,
    position: usize,
) -> Result<EntityLookup, TransactionError> {
    if let Some(id) = value.as_str().and_then(parse_uuid) {
        return Ok(EntityLookup::Id(id));
    }
    let Some(values) = value.as_array() else {
        return Err(error([index.into(), position.into()], "vector?"));
    };
    lookup_values_at(values, index, position)
}

fn lookup_values_at(
    values: &[Value],
    index: usize,
    position: usize,
) -> Result<EntityLookup, TransactionError> {
    let [attr_id, lookup_value] = values else {
        return Err(error([index.into(), position.into()], "(= (count %) 2)"));
    };
    let attr_id = attr_id
        .as_str()
        .and_then(parse_uuid)
        .ok_or_else(|| error([index.into(), position.into(), 0.into()], "uuid?"))?;
    Ok(EntityLookup::Lookup(LookupRef {
        attr_id,
        value: lookup_value.clone(),
    }))
}

fn validate_value_lookup(
    value: &Value,
    attribute: Option<&Attribute>,
    index: usize,
    catalog: &AttributeCatalog,
) -> Result<(), TransactionError> {
    let Some(lookup) = lookup_ref(value) else {
        return Ok(());
    };
    validate_id_lookup(&lookup, catalog, index, 3)?;
    let Some(reverse_etype) = attribute.and_then(|attribute| attribute.reverse_etype.as_deref())
    else {
        return Ok(());
    };
    let Some(lookup_attribute) = catalog.by_id(lookup.attr_id) else {
        return Err(error(
            [index.into(), 3.into()],
            "Invalid lookup. Could not determine namespace from lookup attribute.",
        ));
    };
    let lookup_etype = lookup_attribute.etype.as_str();
    if lookup_etype != reverse_etype {
        return Err(error(
            [index.into(), 3.into()],
            "Invalid transaction. The namespace in the lookup attribute is different from the namespace of the attribute that is being set",
        ));
    }
    Ok(())
}

fn validate_id_lookup(
    lookup: &LookupRef,
    catalog: &AttributeCatalog,
    index: usize,
    position: usize,
) -> Result<(), TransactionError> {
    if catalog
        .by_id(lookup.attr_id)
        .is_some_and(|attribute| attribute.label == "id")
        && lookup.value.as_str().and_then(parse_uuid).is_none()
    {
        let attribute = catalog.by_id(lookup.attr_id).expect("checked above");
        return Err(error(
            [index.into(), position.into()],
            format!(
                "Invalid lookup '{}'. The lookup attribute is '{}.{}', but the lookup value is not a valid UUID.",
                clojure_lookup_display(lookup),
                attribute.etype,
                attribute.label
            ),
        ));
    }
    Ok(())
}

fn lookup_ref(value: &Value) -> Option<LookupRef> {
    let values = value.as_array()?;
    let [attr_id, lookup_value] = values.as_slice() else {
        return None;
    };
    Some(LookupRef {
        attr_id: attr_id.as_str().and_then(parse_uuid)?,
        value: lookup_value.clone(),
    })
}

fn validate_identity_all(value: &Value, index: usize, field: &str) -> Vec<TransactionError> {
    let field_path = || vec![index.into(), 1.into(), field.to_owned().into()];
    if value.is_null() {
        return vec![error(field_path(), "uuid?")];
    }
    let Some(values) = value.as_array() else {
        return vec![error(
            field_path(),
            "(fn [%] (or (nil? %) (sequential? %)))",
        )];
    };
    let [id, etype, label] = match values.as_slice() {
        [] => return vec![error(field_path(), "uuid?")],
        [_] => return vec![error(field_path(), ":instant.db.model.attr/etype")],
        [_, _] => return vec![error(field_path(), ":instant.db.model.attr/label")],
        [id, etype, label] => [id, etype, label],
        [_, _, _, ..] => {
            return vec![error(
                [index.into(), 1.into(), field.to_owned().into(), 3.into()],
                "(clojure.spec.alpha/cat :id uuid? :etype :instant.db.model.attr/etype :label :instant.db.model.attr/label)",
            )];
        }
    };
    let mut errors = Vec::new();
    if id.as_str().and_then(parse_uuid).is_none() {
        errors.push(error(
            [index.into(), 1.into(), field.to_owned().into(), 0.into()],
            "uuid?",
        ));
    }
    if let Some(etype) = etype.as_str() {
        if etype.trim().is_empty() {
            errors.push(error(
                [index.into(), 1.into(), field.to_owned().into(), 1.into()],
                "(complement clojure.string/blank?)",
            ));
        }
    } else {
        errors.push(error(
            [index.into(), 1.into(), field.to_owned().into(), 1.into()],
            "string?",
        ));
    }
    if let Some(label) = label.as_str() {
        if label.trim().is_empty() {
            errors.push(error(
                [index.into(), 1.into(), field.to_owned().into(), 2.into()],
                "(complement clojure.string/blank?)",
            ));
        }
    } else {
        errors.push(error(
            [index.into(), 1.into(), field.to_owned().into(), 2.into()],
            "string?",
        ));
    }
    errors
}

fn validate_required_identity_all(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Vec<TransactionError> {
    object.get(field).map_or_else(
        || {
            vec![error(
                [index.into(), 1.into()],
                missing_key_predicate(field),
            )]
        },
        |value| validate_identity_all(value, index, field),
    )
}

fn validate_optional_identity_all(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Vec<TransactionError> {
    object
        .get(field)
        .map_or_else(Vec::new, |value| validate_identity_all(value, index, field))
}

fn validate_attribute_all(value: &Value, index: usize) -> Vec<TransactionError> {
    let Ok(object) = object_at(value, index, 1) else {
        return vec![error([index.into(), 1.into()], "expected an attribute map")];
    };
    let mut errors = Vec::new();
    push_issue(required_uuid(object, "id", index), &mut errors);
    errors.extend(validate_required_identity_all(
        object,
        "forward-identity",
        index,
    ));

    let value_type = required_enum::<AttributeValueType>(object, "value-type", index);
    let is_ref = matches!(value_type, Ok(AttributeValueType::Ref));
    push_issue(value_type, &mut errors);
    push_issue(
        required_enum::<Cardinality>(object, "cardinality", index),
        &mut errors,
    );
    push_issue(required_bool(object, "unique?", index), &mut errors);
    push_issue(required_bool(object, "index?", index), &mut errors);
    if is_ref {
        errors.extend(validate_required_identity_all(
            object,
            "reverse-identity",
            index,
        ));
    }
    push_issue(
        optional_enum::<DataType>(object, "checked-data-type", index),
        &mut errors,
    );
    push_issue(optional_bool(object, "required?", index), &mut errors);
    push_issue(
        optional_non_null_enum::<DeleteAction>(object, "on-delete", index),
        &mut errors,
    );
    push_issue(
        optional_non_null_enum::<DeleteAction>(object, "on-delete-reverse", index),
        &mut errors,
    );
    push_issue(optional_bool(object, "setting-unique?", index), &mut errors);
    push_issue(optional_bool(object, "indexing?", index), &mut errors);
    push_issue(
        optional_bool(object, "checking-data-type?", index),
        &mut errors,
    );
    errors
}

fn validate_attribute_update_all(value: &Value, index: usize) -> Vec<TransactionError> {
    let Ok(object) = object_at(value, index, 1) else {
        return vec![error([index.into(), 1.into()], "expected an attribute map")];
    };
    let mut errors = Vec::new();
    push_issue(required_uuid(object, "id", index), &mut errors);
    errors.extend(validate_optional_identity_all(
        object,
        "forward-identity",
        index,
    ));
    errors.extend(validate_optional_identity_all(
        object,
        "reverse-identity",
        index,
    ));
    push_issue(optional_bool(object, "unique?", index), &mut errors);
    push_issue(optional_bool(object, "index?", index), &mut errors);
    push_issue(
        optional_enum::<Cardinality>(object, "cardinality", index),
        &mut errors,
    );
    push_issue(optional_bool(object, "required?", index), &mut errors);
    push_issue(
        optional_nullable_enum::<DeleteAction>(object, "on-delete", index),
        &mut errors,
    );
    push_issue(
        optional_nullable_enum::<DeleteAction>(object, "on-delete-reverse", index),
        &mut errors,
    );
    errors
}

fn parse_attribute(value: &Value, index: usize) -> Result<AttributeDefinition, TransactionError> {
    let object = object_at(value, index, 1)?;
    let id = required_uuid(object, "id", index)?;
    let forward_identity = required_identity(object, "forward-identity", index)?;
    let value_type = required_enum::<AttributeValueType>(object, "value-type", index)?;
    let cardinality = required_enum::<Cardinality>(object, "cardinality", index)?;
    let unique = required_bool(object, "unique?", index)?;
    let indexed = required_bool(object, "index?", index)?;
    let reverse_identity = if value_type == AttributeValueType::Ref {
        Some(required_identity(object, "reverse-identity", index)?)
    } else {
        None
    };
    let checked_data_type = optional_enum(object, "checked-data-type", index)?;
    let required = optional_bool(object, "required?", index)?.unwrap_or(false);
    let on_delete = optional_non_null_enum(object, "on-delete", index)?;
    let on_delete_reverse = optional_non_null_enum(object, "on-delete-reverse", index)?;
    let definition = AttributeDefinition {
        attribute: Attribute {
            id,
            wire: Some(instant_instaql::AttributeWireMetadata {
                forward_identity_id: forward_identity.id,
                reverse_identity_id: reverse_identity.as_ref().map(|identity| identity.id),
                inferred_types: None,
            }),
            etype: forward_identity.etype.clone(),
            label: forward_identity.label.clone(),
            value_type,
            cardinality,
            unique,
            setting_unique: optional_bool(object, "setting-unique?", index)?.unwrap_or(false),
            indexed,
            required,
            checked_data_type,
            indexing: optional_bool(object, "indexing?", index)?.unwrap_or(false),
            checking_data_type: optional_bool(object, "checking-data-type?", index)?
                .unwrap_or(false),
            reverse_etype: reverse_identity
                .as_ref()
                .map(|identity| identity.etype.clone()),
            reverse_label: reverse_identity
                .as_ref()
                .map(|identity| identity.label.clone()),
            on_delete,
            on_delete_reverse,
        },
        forward_identity,
        reverse_identity,
        on_delete,
        on_delete_reverse,
    };
    Ok(definition)
}

fn parse_attribute_update(
    value: &Value,
    index: usize,
) -> Result<AttributeUpdate, TransactionError> {
    let object = object_at(value, index, 1)?;
    Ok(AttributeUpdate {
        id: required_uuid(object, "id", index)?,
        forward_identity: optional_identity(object, "forward-identity", index)?,
        reverse_identity: object
            .get("reverse-identity")
            .map(|value| {
                if value.is_null() {
                    Ok(None)
                } else {
                    parse_identity(value, index, "reverse-identity").map(Some)
                }
            })
            .transpose()?,
        unique: optional_bool(object, "unique?", index)?,
        indexed: optional_bool(object, "index?", index)?,
        cardinality: optional_enum(object, "cardinality", index)?,
        required: optional_bool(object, "required?", index)?,
        on_delete: optional_nullable_enum(object, "on-delete", index)?,
        on_delete_reverse: optional_nullable_enum(object, "on-delete-reverse", index)?,
    })
}

#[allow(clippy::option_option)] // Outer None means absent; inner None means an explicit null/clear.
fn optional_nullable_enum<T>(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Option<Option<T>>, TransactionError>
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(None));
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map(Some)
        .map_err(|_| {
            error(
                [index.into(), 1.into(), field.to_owned().into()],
                format!("invalid `{field}`"),
            )
        })
}

fn parse_identity(value: &Value, index: usize, field: &str) -> Result<Identity, TransactionError> {
    let values = value.as_array().ok_or_else(|| {
        error(
            [index.into(), 1.into(), field.to_owned().into()],
            "expected [id, etype, label]",
        )
    })?;
    let [id, etype, label] = values.as_slice() else {
        return Err(error(
            [index.into(), 1.into(), field.to_owned().into()],
            "expected [id, etype, label]",
        ));
    };
    Ok(Identity {
        id: uuid_at(
            id,
            [index.into(), 1.into(), field.to_owned().into(), 0.into()],
        )?,
        etype: etype
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                error(
                    [index.into(), 1.into(), field.to_owned().into(), 1.into()],
                    "expected a non-blank entity type",
                )
            })?
            .to_owned(),
        label: label
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                error(
                    [index.into(), 1.into(), field.to_owned().into(), 2.into()],
                    "expected a non-blank label",
                )
            })?
            .to_owned(),
    })
}

fn required_identity(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Identity, TransactionError> {
    object
        .get(field)
        .ok_or_else(|| error([index.into(), 1.into()], missing_key_predicate(field)))
        .and_then(|value| parse_identity(value, index, field))
}

fn optional_identity(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Option<Identity>, TransactionError> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| parse_identity(value, index, field))
        .transpose()
}

fn object_at(
    value: &Value,
    index: usize,
    position: usize,
) -> Result<&Map<String, Value>, TransactionError> {
    value
        .as_object()
        .ok_or_else(|| error([index.into(), position.into()], "expected an attribute map"))
}

fn required_uuid(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Uuid, TransactionError> {
    object
        .get(field)
        .ok_or_else(|| error([index.into(), 1.into()], missing_key_predicate(field)))
        .and_then(|value| {
            value
                .as_str()
                .and_then(parse_uuid)
                .ok_or_else(|| error([index.into(), 1.into(), field.to_owned().into()], "uuid?"))
        })
}

fn required_bool(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<bool, TransactionError> {
    optional_bool(object, field, index)?
        .ok_or_else(|| error([index.into(), 1.into()], missing_key_predicate(field)))
}

fn optional_bool(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Option<bool>, TransactionError> {
    object
        .get(field)
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                error(
                    [index.into(), 1.into(), field.to_owned().into()],
                    "boolean?",
                )
            })
        })
        .transpose()
}

fn required_enum<T>(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<T, TransactionError>
where
    T: serde::de::DeserializeOwned,
{
    optional_enum(object, field, index)?.ok_or_else(|| enum_error(field, index, true))
}

fn optional_enum<T>(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Option<T>, TransactionError>
where
    T: serde::de::DeserializeOwned,
{
    object
        .get(field)
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|_| enum_error(field, index, false))
        })
        .transpose()
}

fn enum_error(field: &str, index: usize, missing: bool) -> TransactionError {
    match field {
        "value-type" => error([index.into(), 1.into()], "instant.db.model.attr/value-type"),
        "cardinality" if missing => error([index.into(), 1.into()], missing_key_predicate(field)),
        "cardinality" => error(
            [index.into(), 1.into(), field.to_owned().into()],
            "#{:one :many}",
        ),
        "checked-data-type" => error(
            [index.into(), 1.into(), field.to_owned().into()],
            "instant.db.model.attr/checked-data-types",
        ),
        _ if missing => error([index.into(), 1.into()], missing_key_predicate(field)),
        _ => error(
            [index.into(), 1.into(), field.to_owned().into()],
            format!("invalid `{field}`"),
        ),
    }
}

fn missing_key_predicate(field: &str) -> String {
    format!("(fn [%] (contains? % :{field}))")
}

fn optional_non_null_enum<T>(
    object: &Map<String, Value>,
    field: &str,
    index: usize,
) -> Result<Option<T>, TransactionError>
where
    T: serde::de::DeserializeOwned,
{
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|_| {
                error(
                    [index.into(), 1.into(), field.to_owned().into()],
                    format!("invalid `{field}`"),
                )
            })
        })
        .transpose()
}

fn require_len(
    step: &[Value],
    index: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), TransactionError> {
    if (minimum..=maximum).contains(&step.len()) {
        Ok(())
    } else {
        Err(error(
            [index.into()],
            format!("expected {minimum}..={maximum} transaction step items"),
        ))
    }
}

fn uuid_at(
    value: &Value,
    path: impl IntoIterator<Item = PathSegment>,
) -> Result<Uuid, TransactionError> {
    value
        .as_str()
        .and_then(parse_uuid)
        .ok_or_else(|| error(path, "uuid?"))
}

fn string_at(value: &Value, index: usize, position: usize) -> Result<String, TransactionError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| error([index.into(), position.into()], "string?"))
}

fn parse_uuid(value: &str) -> Option<Uuid> {
    (value.len() == 36)
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}

fn normalize_op(value: &str) -> String {
    value.trim_start_matches(':').to_owned()
}

fn display_value(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn clojure_lookup_display(lookup: &LookupRef) -> String {
    format!(
        "[#uuid \"{}\" {}]",
        lookup.attr_id,
        clojure_value_display(&lookup.value)
    )
}

fn clojure_value_display(value: &Value) -> String {
    match value {
        Value::Null => "nil".to_owned(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::String(value) if parse_uuid(value).is_some() => format!("#uuid \"{value}\""),
        Value::String(value) => serde_json::to_string(value).expect("strings always serialize"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(clojure_value_display)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{} {}",
                    serde_json::to_string(key).expect("map keys always serialize"),
                    clojure_value_display(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn error(
    path: impl IntoIterator<Item = PathSegment>,
    message: impl Into<String>,
) -> TransactionError {
    TransactionError {
        path: path.into_iter().collect(),
        message: message.into(),
    }
}

impl From<usize> for PathSegment {
    fn from(value: usize) -> Self {
        Self::Index(value)
    }
}

impl From<String> for PathSegment {
    fn from(value: String) -> Self {
        Self::Field(value)
    }
}

impl From<&str> for PathSegment {
    fn from(value: &str) -> Self {
        Self::Field(value.to_owned())
    }
}
