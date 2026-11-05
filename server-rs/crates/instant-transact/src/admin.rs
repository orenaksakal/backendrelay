use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    CompiledTransaction, PathSegment, TransactionError, TransactionStep,
    compile_transaction_with_order,
};

/// Lowers the object-oriented Admin SDK transaction tuples into the internal
/// attribute-addressed transaction protocol.
///
/// # Errors
///
/// Returns a path-aware validation error for malformed tuples, invalid entity
/// lookups, or fields missing while strict schema mode is enabled.
pub fn compile_admin_transaction(
    input: &Value,
    catalog: &AttributeCatalog,
    throw_on_missing_attrs: bool,
) -> Result<Vec<TransactionStep>, TransactionError> {
    compile_admin_transaction_with_order(input, catalog, throw_on_missing_attrs)
        .map(|compiled| compiled.grouped_steps)
}

/// Lowers Admin SDK tuples once and retains source and execution ordering.
///
/// # Errors
///
/// Returns the same path-aware validation errors as [`compile_admin_transaction`].
pub fn compile_admin_transaction_with_order(
    input: &Value,
    catalog: &AttributeCatalog,
    throw_on_missing_attrs: bool,
) -> Result<CompiledTransaction, TransactionError> {
    let raw_steps = input
        .as_array()
        .ok_or_else(|| admin_error([], "expected a collection"))?;
    let mut working = catalog.clone();
    let mut lowered = Vec::new();
    precreate_lookup_attributes(
        raw_steps,
        &mut working,
        throw_on_missing_attrs,
        &mut lowered,
    )?;

    for (index, raw) in raw_steps.iter().enumerate() {
        let Some(step) = raw.as_array() else {
            lowered.push(raw.clone());
            continue;
        };
        match step.first().and_then(Value::as_str) {
            Some("create") => lower_create(
                step,
                index,
                &mut working,
                throw_on_missing_attrs,
                &mut lowered,
            )?,
            Some("update") => lower_update(
                step,
                index,
                &mut working,
                throw_on_missing_attrs,
                &mut lowered,
            )?,
            Some("merge") => lower_merge(
                step,
                index,
                &mut working,
                throw_on_missing_attrs,
                &mut lowered,
            )?,
            Some("link") => lower_link(
                step,
                index,
                &mut working,
                throw_on_missing_attrs,
                &mut lowered,
            )?,
            Some("unlink") => lower_unlink(
                step,
                index,
                &mut working,
                throw_on_missing_attrs,
                &mut lowered,
            )?,
            Some("delete") => lower_delete(
                step,
                index,
                &mut working,
                throw_on_missing_attrs,
                &mut lowered,
            )?,
            Some("ruleParams") => lower_rule_params(step, index, &working, &mut lowered)?,
            _ => lowered.push(raw.clone()),
        }
    }

    compile_transaction_with_order(&Value::Array(lowered), &working)
}

fn precreate_lookup_attributes(
    raw_steps: &[Value],
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    for (index, raw) in raw_steps.iter().enumerate() {
        let Some(step) = raw.as_array() else {
            continue;
        };
        let Some(action) = step.first().and_then(Value::as_str) else {
            continue;
        };
        let Some(etype) = step.get(1).and_then(Value::as_str) else {
            continue;
        };
        if matches!(
            action,
            "create" | "update" | "merge" | "link" | "unlink" | "delete"
        ) && let Some(entity) = step.get(2).filter(|entity| looks_like_admin_lookup(entity))
        {
            let _ = lower_entity(
                entity,
                etype,
                catalog,
                index,
                2,
                throw_on_missing_attrs,
                lowered,
            )?;
        }
        if action != "link" {
            continue;
        }
        let Some(links) = step.get(3).and_then(Value::as_object) else {
            continue;
        };
        for (label, targets) in links {
            for target in link_targets(targets)
                .into_iter()
                .filter(|target| looks_like_admin_lookup(target))
            {
                let relationship = if let Some(attribute) = catalog
                    .forward(etype, label)
                    .or_else(|| catalog.reverse(etype, label))
                {
                    attribute.clone()
                } else {
                    ensure_lookup_link_attribute(
                        catalog,
                        etype,
                        label,
                        index,
                        throw_on_missing_attrs,
                        lowered,
                    )?
                };
                let forward = relationship.etype == etype && relationship.label == *label;
                let target_etype = if forward {
                    relationship.reverse_etype.as_deref().unwrap_or(label)
                } else {
                    relationship.etype.as_str()
                };
                let _ = lower_entity(
                    target,
                    target_etype,
                    catalog,
                    index,
                    3,
                    throw_on_missing_attrs,
                    lowered,
                )?;
            }
        }
    }
    Ok(())
}

fn looks_like_admin_lookup(value: &Value) -> bool {
    value.is_array()
        || value.is_object()
        || value
            .as_str()
            .is_some_and(|value| value.starts_with("lookup__"))
}

fn lower_create(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 4, 4)?;
    let options = json!({"mode": "create"});
    lower_fields(
        step,
        index,
        catalog,
        throw_on_missing_attrs,
        "add-triple",
        Some(&options),
        lowered,
    )
}

fn lower_update(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 4, 5)?;
    let options = update_options(step.get(4), index)?;
    lower_fields(
        step,
        index,
        catalog,
        throw_on_missing_attrs,
        "add-triple",
        options.as_ref(),
        lowered,
    )
}

fn lower_merge(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 4, 5)?;
    let options = update_options(step.get(4), index)?;
    lower_fields(
        step,
        index,
        catalog,
        throw_on_missing_attrs,
        "deep-merge-triple",
        options.as_ref(),
        lowered,
    )
}

#[allow(clippy::too_many_arguments)]
fn lower_fields(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    field_operation: &str,
    options: Option<&Value>,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 4, 5)?;
    let etype = admin_string(&step[1], index, 1, "entity type")?;
    let fields = admin_object(&step[3], index, 3, "fields")?;
    let id_attr = ensure_blob_attribute(
        catalog,
        etype,
        "id",
        true,
        index,
        throw_on_missing_attrs,
        lowered,
    )?;
    let entity = lower_entity(
        &step[2],
        etype,
        catalog,
        index,
        2,
        throw_on_missing_attrs,
        lowered,
    )?;

    push_triple(
        lowered,
        "add-triple",
        entity.clone(),
        id_attr.id,
        entity.clone(),
        options.cloned(),
    );
    for (label, value) in fields {
        if label == "id" {
            continue;
        }
        let attribute = ensure_blob_attribute(
            catalog,
            etype,
            label,
            false,
            index,
            throw_on_missing_attrs,
            lowered,
        )?;
        push_triple(
            lowered,
            field_operation,
            entity.clone(),
            attribute.id,
            value.clone(),
            options.cloned(),
        );
    }
    Ok(())
}

fn lower_link(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    lower_link_operation(
        step,
        index,
        catalog,
        throw_on_missing_attrs,
        "add-triple",
        lowered,
    )
}

fn lower_unlink(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    lower_link_operation(
        step,
        index,
        catalog,
        throw_on_missing_attrs,
        "retract-triple",
        lowered,
    )
}

#[allow(clippy::too_many_arguments)]
fn lower_link_operation(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    operation: &str,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 4, 4)?;
    let etype = admin_string(&step[1], index, 1, "entity type")?;
    let links = admin_object(&step[3], index, 3, "links")?;
    let id_attribute = ensure_blob_attribute(
        catalog,
        etype,
        "id",
        true,
        index,
        throw_on_missing_attrs,
        lowered,
    )?;
    let source = lower_entity(
        &step[2],
        etype,
        catalog,
        index,
        2,
        throw_on_missing_attrs,
        lowered,
    )?;
    if source.is_array() {
        push_triple(
            lowered,
            "add-triple",
            source.clone(),
            id_attribute.id,
            source.clone(),
            None,
        );
    }

    for (label, value) in links {
        let attribute = ensure_link_attribute(
            catalog,
            etype,
            label,
            index,
            throw_on_missing_attrs,
            lowered,
        )?;
        let forward = attribute.etype == etype && attribute.label == *label;
        let target_etype = if forward {
            attribute.reverse_etype.as_deref().unwrap_or(label)
        } else {
            attribute.etype.as_str()
        };
        for target in link_targets(value) {
            let target = if operation == "add-triple" {
                lower_entity(
                    target,
                    target_etype,
                    catalog,
                    index,
                    3,
                    throw_on_missing_attrs,
                    lowered,
                )?
            } else {
                lower_existing_entity(target, target_etype, catalog, index, 3)?
            };
            let (entity, linked) = if forward {
                (source.clone(), target)
            } else {
                (target, source.clone())
            };
            push_triple(lowered, operation, entity, attribute.id, linked, None);
        }
    }
    Ok(())
}

fn lower_delete(
    step: &[Value],
    index: usize,
    catalog: &mut AttributeCatalog,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 3, usize::MAX)?;
    let etype = admin_string(&step[1], index, 1, "entity type")?;
    let entity = lower_entity(
        &step[2],
        etype,
        catalog,
        index,
        2,
        throw_on_missing_attrs,
        lowered,
    )?;
    lowered.push(json!(["delete-entity", entity, etype]));
    Ok(())
}

fn lower_rule_params(
    step: &[Value],
    index: usize,
    catalog: &AttributeCatalog,
    lowered: &mut Vec<Value>,
) -> Result<(), TransactionError> {
    require_admin_len(step, index, 4, 4)?;
    let etype = admin_string(&step[1], index, 1, "entity type")?;
    let params = admin_object(&step[3], index, 3, "rule params")?;
    let entity = lower_existing_entity(&step[2], etype, catalog, index, 2)?;
    lowered.push(json!(["rule-params", entity, etype, params]));
    Ok(())
}

fn ensure_blob_attribute(
    catalog: &mut AttributeCatalog,
    etype: &str,
    label: &str,
    unique: bool,
    index: usize,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<Attribute, TransactionError> {
    if let Some(attribute) = catalog.forward(etype, label) {
        return Ok(attribute.clone());
    }
    if throw_on_missing_attrs {
        return Err(missing_attribute_error(index, etype, label));
    }
    let attribute = Attribute {
        id: Uuid::new_v4(),
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique,
        setting_unique: false,
        indexed: false,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    };
    let identity_id = Uuid::new_v4();
    lowered.push(json!(["add-attr", {
        "id": attribute.id,
        "forward-identity": [identity_id, etype, label],
        "value-type": "blob",
        "cardinality": "one",
        "unique?": unique,
        "index?": false
    }]));
    catalog.attributes.push(attribute.clone());
    Ok(attribute)
}

fn ensure_link_attribute(
    catalog: &mut AttributeCatalog,
    etype: &str,
    label: &str,
    index: usize,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<Attribute, TransactionError> {
    if let Some(attribute) = catalog
        .forward(etype, label)
        .or_else(|| catalog.reverse(etype, label))
    {
        return Ok(attribute.clone());
    }
    if throw_on_missing_attrs {
        return Err(missing_attribute_error(index, etype, label));
    }
    let attribute = Attribute {
        id: Uuid::new_v4(),
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Ref,
        cardinality: Cardinality::Many,
        unique: false,
        setting_unique: false,
        indexed: false,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: Some(label.to_owned()),
        reverse_label: Some(etype.to_owned()),
        on_delete: None,
        on_delete_reverse: None,
    };
    let forward_identity = Uuid::new_v4();
    let reverse_identity = Uuid::new_v4();
    lowered.push(json!(["add-attr", {
        "id": attribute.id,
        "forward-identity": [forward_identity, etype, label],
        "reverse-identity": [reverse_identity, label, etype],
        "value-type": "ref",
        "cardinality": "many",
        "unique?": false,
        "index?": false
    }]));
    catalog.attributes.push(attribute.clone());
    Ok(attribute)
}

fn ensure_lookup_blob_attribute(
    catalog: &mut AttributeCatalog,
    etype: &str,
    label: &str,
    index: usize,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<Attribute, TransactionError> {
    if throw_on_missing_attrs {
        return Err(missing_attribute_error(index, etype, label));
    }
    let attribute = Attribute {
        id: Uuid::new_v4(),
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: true,
        setting_unique: false,
        indexed: true,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    };
    lowered.push(json!(["add-attr", {
        "id": attribute.id,
        "forward-identity": [Uuid::new_v4(), etype, label],
        "value-type": "blob",
        "cardinality": "one",
        "unique?": true,
        "index?": true
    }]));
    catalog.attributes.push(attribute.clone());
    Ok(attribute)
}

fn ensure_lookup_link_attribute(
    catalog: &mut AttributeCatalog,
    etype: &str,
    label: &str,
    index: usize,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<Attribute, TransactionError> {
    if throw_on_missing_attrs {
        return Err(missing_attribute_error(index, etype, label));
    }
    let attribute = Attribute {
        id: Uuid::new_v4(),
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Ref,
        cardinality: Cardinality::One,
        unique: true,
        setting_unique: false,
        indexed: true,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: Some(label.to_owned()),
        reverse_label: Some(etype.to_owned()),
        on_delete: None,
        on_delete_reverse: None,
    };
    lowered.push(json!(["add-attr", {
        "id": attribute.id,
        "forward-identity": [Uuid::new_v4(), etype, label],
        "reverse-identity": [Uuid::new_v4(), label, etype],
        "value-type": "ref",
        "cardinality": "one",
        "unique?": true,
        "index?": true
    }]));
    catalog.attributes.push(attribute.clone());
    Ok(attribute)
}

fn lower_entity(
    value: &Value,
    etype: &str,
    catalog: &mut AttributeCatalog,
    index: usize,
    position: usize,
    throw_on_missing_attrs: bool,
    lowered: &mut Vec<Value>,
) -> Result<Value, TransactionError> {
    if value
        .as_str()
        .and_then(|value| value.parse::<Uuid>().ok())
        .is_some()
    {
        return Ok(value.clone());
    }
    let (ident_name, lookup_value) = admin_lookup_pair(value, index, position)?;
    let relationship_lookup =
        ident_name.contains('.') && catalog.forward(etype, &ident_name).is_none();
    let label = if relationship_lookup {
        relationship_lookup_label(&ident_name, value, index, position)?
    } else {
        ident_name.clone()
    };
    let attribute = if let Some(attribute) = catalog.forward(etype, &label) {
        attribute.clone()
    } else if relationship_lookup {
        ensure_lookup_link_attribute(
            catalog,
            etype,
            &label,
            index,
            throw_on_missing_attrs,
            lowered,
        )?
    } else {
        ensure_lookup_blob_attribute(
            catalog,
            etype,
            &label,
            index,
            throw_on_missing_attrs,
            lowered,
        )?
    };
    if !attribute.unique {
        return Err(admin_error(
            [index.into(), position.into()],
            format!("{ident_name} is not a unique attribute on {etype}"),
        ));
    }
    Ok(json!([attribute.id, lookup_value]))
}

fn lower_existing_entity(
    value: &Value,
    etype: &str,
    catalog: &AttributeCatalog,
    index: usize,
    position: usize,
) -> Result<Value, TransactionError> {
    if value
        .as_str()
        .and_then(|value| value.parse::<Uuid>().ok())
        .is_some()
    {
        return Ok(value.clone());
    }
    let (ident_name, lookup_value) = admin_lookup_pair(value, index, position)?;
    let relationship_lookup =
        ident_name.contains('.') && catalog.forward(etype, &ident_name).is_none();
    let label = if relationship_lookup {
        relationship_lookup_label(&ident_name, value, index, position)?
    } else {
        ident_name.clone()
    };
    let attribute = catalog.forward(etype, &label).ok_or_else(|| {
        admin_error(
            [index.into(), position.into()],
            format!("unknown lookup attribute `{etype}.{label}`"),
        )
    })?;
    if !attribute.unique {
        return Err(admin_error(
            [index.into(), position.into()],
            format!("{ident_name} is not a unique attribute on {etype}"),
        ));
    }
    Ok(json!([attribute.id, lookup_value]))
}

fn admin_lookup_pair(
    value: &Value,
    index: usize,
    position: usize,
) -> Result<(String, Value), TransactionError> {
    if let Some(encoded) = value
        .as_str()
        .and_then(|value| value.strip_prefix("lookup__"))
    {
        let (label, encoded_value) = encoded.split_once("__").ok_or_else(|| {
            admin_error([index.into(), position.into()], "lookup value is invalid")
        })?;
        let lookup_value = serde_json::from_str(encoded_value)
            .map_err(|_| admin_error([index.into(), position.into()], "lookup value is invalid"))?;
        return Ok((label.to_owned(), lookup_value));
    }
    if let Some(values) = value.as_array() {
        let [label, lookup_value] = values.as_slice() else {
            return Err(admin_error(
                [index.into(), position.into()],
                "lookup must contain one attribute and value",
            ));
        };
        let label = label.as_str().ok_or_else(|| {
            admin_error(
                [index.into(), position.into(), 0.into()],
                "lookup attribute must be a string",
            )
        })?;
        return Ok((label.to_owned(), lookup_value.clone()));
    }
    if let Some(values) = value.as_object() {
        let Some((label, lookup_value)) = values.iter().next().filter(|_| values.len() == 1) else {
            return Err(admin_error(
                [index.into(), position.into()],
                "lookup must be an object with a single unique attr and value",
            ));
        };
        return Ok((label.clone(), lookup_value.clone()));
    }
    Err(admin_error(
        [index.into(), position.into()],
        format!(
            "Invalid entity ID '{}'. Entity IDs must be UUIDs or lookup references.",
            display(value)
        ),
    ))
}

fn relationship_lookup_label(
    ident_name: &str,
    lookup: &Value,
    index: usize,
    position: usize,
) -> Result<String, TransactionError> {
    let parts = ident_name.split('.').collect::<Vec<_>>();
    let [label, "id"] = parts.as_slice() else {
        return Err(admin_error(
            [index.into(), position.into()],
            format!("{ident_name} is not a valid lookup attribute: {lookup}"),
        ));
    };
    Ok((*label).to_owned())
}

fn link_targets(value: &Value) -> Vec<&Value> {
    match value.as_array() {
        Some(values) if !looks_like_lookup(values) => values.iter().collect(),
        _ => vec![value],
    }
}

fn looks_like_lookup(values: &[Value]) -> bool {
    values.len() == 2
        && values[0]
            .as_str()
            .is_some_and(|value| value.parse::<Uuid>().is_err() && !value.starts_with("lookup__"))
}

fn update_options(value: Option<&Value>, index: usize) -> Result<Option<Value>, TransactionError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let options = admin_object(value, index, 4, "transaction options")?;
    let Some(upsert) = options.get("upsert") else {
        return Ok(None);
    };
    if upsert.is_null() {
        return Ok(None);
    }
    let upsert = !matches!(upsert, Value::Bool(false));
    Ok(Some(
        json!({"mode": if upsert { "upsert" } else { "update" }}),
    ))
}

fn push_triple(
    lowered: &mut Vec<Value>,
    operation: &str,
    entity: Value,
    attr_id: Uuid,
    value: Value,
    options: Option<Value>,
) {
    let mut step = vec![json!(operation), entity, json!(attr_id), value];
    if let Some(options) = options {
        step.push(options);
    }
    lowered.push(Value::Array(step));
}

fn missing_attribute_error(index: usize, etype: &str, label: &str) -> TransactionError {
    admin_error(
        [index.into(), 3.into(), label.to_owned().into()],
        format!("Attributes are missing in your schema: {etype}.{label}"),
    )
}

fn require_admin_len(
    step: &[Value],
    index: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), TransactionError> {
    if step.len() < minimum || step.len() > maximum {
        return Err(admin_error(
            [index.into()],
            format!("expected {minimum} to {maximum} transaction fields"),
        ));
    }
    Ok(())
}

fn admin_string<'a>(
    value: &'a Value,
    index: usize,
    position: usize,
    name: &str,
) -> Result<&'a str, TransactionError> {
    value
        .as_str()
        .ok_or_else(|| admin_error([index.into(), position.into()], format!("expected {name}")))
}

fn admin_object<'a>(
    value: &'a Value,
    index: usize,
    position: usize,
    name: &str,
) -> Result<&'a Map<String, Value>, TransactionError> {
    value
        .as_object()
        .ok_or_else(|| admin_error([index.into(), position.into()], format!("expected {name}")))
}

fn display(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn admin_error(
    path: impl IntoIterator<Item = PathSegment>,
    message: impl Into<String>,
) -> TransactionError {
    TransactionError {
        path: path.into_iter().collect(),
        message: message.into(),
    }
}
