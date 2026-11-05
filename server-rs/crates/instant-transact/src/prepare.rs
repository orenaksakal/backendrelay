use std::collections::{HashMap, HashSet};

use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType};
use serde_json::{Value, json};
use tokio_postgres::Transaction;
use uuid::Uuid;

use crate::{
    DeleteEntityStep, EntityLookup, ExecutionError, LookupRef, RuleParamsStep, TransactionMode,
    TransactionStep, TripleOptions, TripleStep,
};

/// A unique lookup triple that must be inserted as part of a prepared write.
///
/// These triples participate in permission projection but are deliberately not
/// reported as user-requested mutations in the transaction receipt.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedLookup {
    pub entity_id: Uuid,
    pub lookup: LookupRef,
    /// Operation group that first creates this lookup entity.
    pub operation: crate::TransactionOperation,
}

/// Transaction steps whose entity and value lookups have been resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedTransaction {
    pub original_steps: Vec<TransactionStep>,
    pub steps: Vec<TransactionStep>,
    pub source_steps: Vec<TransactionStep>,
    pub implicit_lookups: Vec<PreparedLookup>,
    /// Distinct entity lookup refs for each add-triple group, retaining the
    /// original triple order for the public lookup-collision envelope.
    pub add_group_lookup_refs: Vec<Vec<LookupRef>>,
    pub catalog: AttributeCatalog,
}

impl PreparedTransaction {
    /// Returns the resolved user steps plus synthetic lookup triples for
    /// permission projection and modified-field calculation.
    #[must_use]
    pub fn permission_steps(&self) -> Vec<TransactionStep> {
        let mut steps = Vec::with_capacity(self.implicit_lookups.len() + self.steps.len());
        steps.extend(self.implicit_lookups.iter().filter_map(|prepared| {
            let attribute = self.catalog.by_id(prepared.lookup.attr_id)?;
            Some(TransactionStep::AddTriple(TripleStep {
                entity: EntityLookup::Id(prepared.entity_id),
                etype: Some(attribute.etype.clone()),
                attr_id: prepared.lookup.attr_id,
                reverse_etype: attribute.reverse_etype.clone(),
                value: prepared.lookup.value.clone(),
                options: TripleOptions::default(),
            }))
        }));
        steps.extend(self.source_steps.clone());
        steps
    }
}

#[derive(Clone, Debug)]
struct LookupUse {
    lookup: LookupRef,
    creates_entity: bool,
    requires_unique: bool,
    validates_attribute: bool,
    lock_key: String,
}

/// Locks and resolves every entity/value lookup without writing database rows.
///
/// Missing entity lookups used by create/upsert adds are assigned UUIDs and
/// returned as implicit triples. Their rows are inserted by the prepared
/// executor only after the transaction record, preserving WAL ordering.
///
/// # Errors
///
/// Returns transaction validation or PostgreSQL errors for invalid attributes,
/// non-unique lookup attributes on creating operations, missing value lookups,
/// and missing update targets. Retract-only non-unique lookups intentionally
/// resolve through ordinary triples, like upstream `delete-multi!`.
pub async fn prepare_transaction_lookups(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<PreparedTransaction, ExecutionError> {
    prepare_transaction_lookups_with_source_order(transaction, app_id, catalog, steps, steps).await
}

/// Resolves one source-ordered permission view and one grouped execution view
/// with the same locks, lookup IDs, optimistic catalog, and cascade snapshot.
///
/// # Errors
///
/// Returns lookup, mode-preparation, cascade, or PostgreSQL errors from either
/// transaction view.
#[allow(clippy::too_many_lines)] // One lookup allocation pass must construct both parity views.
pub async fn prepare_transaction_lookups_with_source_order(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    source_steps: &[TransactionStep],
) -> Result<PreparedTransaction, ExecutionError> {
    let original_steps = steps.to_vec();
    let add_group_lookup_refs = collect_add_group_lookup_refs(steps);
    let restored_attributes = crate::execute::load_restored_attributes(transaction, app_id, steps)
        .await?
        .into_values();
    let catalog_with_restores =
        crate::execute::catalog_with_restored_attributes(catalog, restored_attributes);
    let effective_catalog = crate::execute::optimistic_catalog(&catalog_with_restores, steps);
    let catalog = &effective_catalog;
    let mut uses = collect_lookup_uses(app_id, catalog, steps)?;
    uses.sort_by(|left, right| left.lock_key.cmp(&right.lock_key));
    let mut deduplicated = Vec::<LookupUse>::with_capacity(uses.len());
    for lookup_use in uses {
        if let Some(previous) = deduplicated.last_mut()
            && previous.lookup == lookup_use.lookup
        {
            previous.creates_entity |= lookup_use.creates_entity;
            previous.requires_unique |= lookup_use.requires_unique;
            previous.validates_attribute |= lookup_use.validates_attribute;
        } else {
            deduplicated.push(lookup_use);
        }
    }
    let uses = deduplicated;

    for lookup_use in &uses {
        transaction
            .query_one(
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
                &[&lookup_use.lock_key],
            )
            .await?;
    }

    let mut resolved = Vec::<(LookupRef, Uuid)>::new();
    let mut implicit_lookups = Vec::new();
    for lookup_use in &uses {
        let attribute = if lookup_use.validates_attribute && lookup_use.requires_unique {
            Some(lookup_attribute(catalog, &lookup_use.lookup)?)
        } else if lookup_use.validates_attribute {
            Some(known_lookup_attribute(catalog, &lookup_use.lookup)?)
        } else {
            catalog.by_id(lookup_use.lookup.attr_id)
        };
        if let Some(entity_id) = lookup_entity(
            transaction,
            app_id,
            &lookup_use.lookup,
            lookup_use.requires_unique,
        )
        .await?
        {
            resolved.push((lookup_use.lookup.clone(), entity_id));
        } else if let Some(entity_id) =
            earlier_same_add_group_id_value_lookup(catalog, steps, &lookup_use.lookup)
        {
            resolved.push((lookup_use.lookup.clone(), entity_id));
        } else if lookup_use.creates_entity {
            let attribute =
                attribute.ok_or(ExecutionError::UnknownAttribute(lookup_use.lookup.attr_id))?;
            let entity_id = allocate_lookup_entity_id(attribute, &lookup_use.lookup)?;
            let Some(operation) = lookup_creation_operation(catalog, steps, &lookup_use.lookup)
            else {
                continue;
            };
            resolved.push((lookup_use.lookup.clone(), entity_id));
            implicit_lookups.push(PreparedLookup {
                entity_id,
                lookup: lookup_use.lookup.clone(),
                operation,
            });
        }
    }

    let mut rewritten_source = Vec::with_capacity(source_steps.len());
    let mut missing_source_updates = Vec::new();
    let source_order = source_steps
        .iter()
        .filter(|step| !is_lookup_delete(step))
        .chain(source_steps.iter().filter(|step| is_lookup_delete(step)));
    for step in source_order {
        rewrite_step(
            catalog,
            step,
            &resolved,
            &mut rewritten_source,
            &mut missing_source_updates,
        )?;
    }
    if !missing_source_updates.is_empty() {
        return Err(ExecutionError::UpdateMissing(missing_source_updates));
    }

    let mut rewritten = Vec::with_capacity(steps.len());
    let mut missing_updates = Vec::new();
    for step in steps {
        rewrite_step(
            catalog,
            step,
            &resolved,
            &mut rewritten,
            &mut missing_updates,
        )?;
    }
    if !missing_updates.is_empty() {
        return Err(ExecutionError::UpdateMissing(missing_updates));
    }
    let source_steps =
        resolve_untyped_deletes(transaction, app_id, catalog, rewritten_source, true).await?;
    let source_steps =
        expand_delete_cascades(transaction, app_id, catalog, source_steps, false).await?;
    let steps = resolve_untyped_deletes(transaction, app_id, catalog, rewritten, false).await?;
    let steps = expand_delete_cascades(transaction, app_id, catalog, steps, true).await?;
    Ok(PreparedTransaction {
        original_steps,
        steps,
        source_steps,
        implicit_lookups,
        add_group_lookup_refs,
        catalog: effective_catalog,
    })
}

fn earlier_same_add_group_id_value_lookup(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    lookup: &LookupRef,
) -> Option<Uuid> {
    let lookup_attribute = catalog.by_id(lookup.attr_id)?;
    if lookup_attribute.label != "id" {
        return None;
    }

    let mut requested_entity = None;
    for step in steps {
        let TransactionStep::AddTriple(triple) = step else {
            requested_entity = None;
            continue;
        };
        let target = catalog.by_id(triple.attr_id)?;
        if (target.value_type == AttributeValueType::Ref || target.label == "id")
            && value_lookup(&triple.value).as_ref() == Some(lookup)
        {
            return requested_entity;
        }
        if triple.attr_id == lookup.attr_id
            && triple.value == lookup.value
            && let EntityLookup::Id(entity_id) = &triple.entity
            && lookup.value == json!(entity_id)
        {
            requested_entity = Some(*entity_id);
        }
    }
    None
}

fn collect_add_group_lookup_refs(steps: &[TransactionStep]) -> Vec<Vec<LookupRef>> {
    let mut groups = Vec::new();
    let mut current = None::<Vec<LookupRef>>;
    for step in steps {
        if let TransactionStep::AddTriple(triple) = step {
            let lookups = current.get_or_insert_with(Vec::new);
            if let EntityLookup::Lookup(lookup) = &triple.entity
                && !lookups.contains(lookup)
            {
                lookups.push(lookup.clone());
            }
        } else if let Some(lookups) = current.take() {
            groups.push(lookups);
        }
    }
    if let Some(lookups) = current {
        groups.push(lookups);
    }
    groups
}

fn lookup_creation_operation(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    lookup: &LookupRef,
) -> Option<crate::TransactionOperation> {
    let mut operation_order = Vec::new();
    for step in steps {
        if !operation_order.contains(&step.operation()) {
            operation_order.push(step.operation());
        }
    }
    for operation in operation_order {
        let mut used = false;
        let mut creates = false;
        for step in steps.iter().filter(|step| step.operation() == operation) {
            let (step_uses, step_creates) = lookup_use_in_step(catalog, step, lookup);
            used |= step_uses;
            creates |= step_creates;
        }
        if used {
            if creates {
                return Some(operation);
            }
            if operation != crate::TransactionOperation::RuleParams {
                return None;
            }
        }
    }
    None
}

fn lookup_use_in_step(
    catalog: &AttributeCatalog,
    step: &TransactionStep,
    lookup: &LookupRef,
) -> (bool, bool) {
    let (triple, creates_entity) = match step {
        TransactionStep::AddTriple(triple) | TransactionStep::DeepMergeTriple(triple) => {
            (triple, triple.options.mode != Some(TransactionMode::Update))
        }
        TransactionStep::RetractTriple(triple) => (triple, false),
        TransactionStep::DeleteEntity(delete) => {
            let used =
                matches!(&delete.entity, EntityLookup::Lookup(candidate) if candidate == lookup);
            return (used, false);
        }
        TransactionStep::RuleParams(params) => {
            let used =
                matches!(&params.entity, EntityLookup::Lookup(candidate) if candidate == lookup);
            return (used, false);
        }
        _ => return (false, false),
    };
    let entity_use =
        matches!(&triple.entity, EntityLookup::Lookup(candidate) if candidate == lookup);
    let value_use = catalog.by_id(triple.attr_id).is_some_and(|attribute| {
        (attribute.value_type == AttributeValueType::Ref || attribute.label == "id")
            && value_lookup(&triple.value).as_ref() == Some(lookup)
    });
    (entity_use || value_use, entity_use && creates_entity)
}

/// Expands cascaded entity deletes from the transaction's pre-write snapshot.
///
/// The JVM performs this expansion during preprocessing, before any requested
/// triple is written.  Keeping the expansion here is significant: a link added
/// earlier in the same transaction must not unexpectedly turn its target into
/// a cascade delete.  Synthetic deletes inherit the root's rule parameters so
/// permission checks cover every entity that execution will remove.
async fn expand_delete_cascades(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: Vec<TransactionStep>,
    group_operations: bool,
) -> Result<Vec<TransactionStep>, ExecutionError> {
    let parent_rule_params = steps
        .iter()
        .filter_map(|step| {
            let TransactionStep::RuleParams(params) = step else {
                return None;
            };
            let EntityLookup::Id(entity_id) = params.entity else {
                return None;
            };
            Some((entity_id, params.params.clone()))
        })
        .collect::<HashMap<_, _>>();
    let roots = steps
        .iter()
        .filter_map(|step| {
            let TransactionStep::DeleteEntity(delete) = step else {
                return None;
            };
            let (EntityLookup::Id(entity_id), Some(etype)) = (&delete.entity, &delete.etype) else {
                return None;
            };
            Some((*entity_id, etype.clone()))
        })
        .collect::<Vec<_>>();

    let mut expanded = steps.clone();
    for (parent_id, parent_etype) in roots {
        let entities = crate::execute::expand_delete_cascades(
            transaction,
            app_id,
            catalog,
            &[(parent_id, parent_etype.clone())],
        )
        .await?;
        for (entity_id, etype) in entities {
            if entity_id == parent_id && etype == parent_etype {
                continue;
            }
            let delete = TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Id(entity_id),
                etype: Some(etype.clone()),
            });
            if !expanded.contains(&delete) {
                expanded.push(delete);
            }
            if let Some(params) = parent_rule_params.get(&parent_id) {
                let rule_params = TransactionStep::RuleParams(RuleParamsStep {
                    entity: EntityLookup::Id(entity_id),
                    etype: Some(etype),
                    params: params.clone(),
                });
                if !expanded.contains(&rule_params) {
                    expanded.push(rule_params);
                }
            }
        }
    }

    if group_operations {
        Ok(crate::group_transaction_steps(&expanded))
    } else {
        Ok(expanded)
    }
}

fn collect_lookup_uses(
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<Vec<LookupUse>, ExecutionError> {
    let mut uses = Vec::new();
    for step in steps {
        match step {
            TransactionStep::AddTriple(triple) | TransactionStep::DeepMergeTriple(triple) => {
                let attribute = target_attribute(catalog, triple)?;
                collect_entity_lookup(
                    app_id,
                    catalog,
                    triple,
                    triple.options.mode != Some(TransactionMode::Update),
                    true,
                    &mut uses,
                )?;
                collect_value_lookup(app_id, attribute, &triple.value, true, &mut uses);
            }
            TransactionStep::RetractTriple(triple) => {
                let attribute = target_attribute(catalog, triple)?;
                collect_entity_lookup(app_id, catalog, triple, false, false, &mut uses)?;
                collect_value_lookup(app_id, attribute, &triple.value, false, &mut uses);
            }
            TransactionStep::DeleteEntity(delete) => {
                if let EntityLookup::Lookup(lookup) = &delete.entity {
                    uses.push(unvalidated_lookup_use(app_id, lookup));
                }
            }
            TransactionStep::RuleParams(params) => {
                if let EntityLookup::Lookup(lookup) = &params.entity {
                    uses.push(unvalidated_lookup_use(app_id, lookup));
                }
            }
            TransactionStep::AddAttr(_)
            | TransactionStep::UpdateAttr(_)
            | TransactionStep::DeleteAttr(_)
            | TransactionStep::RestoreAttr(_) => {}
        }
    }
    Ok(uses)
}

fn collect_entity_lookup(
    app_id: Uuid,
    catalog: &AttributeCatalog,
    triple: &TripleStep,
    creates_entity: bool,
    requires_unique: bool,
    uses: &mut Vec<LookupUse>,
) -> Result<(), ExecutionError> {
    let EntityLookup::Lookup(lookup) = &triple.entity else {
        return Ok(());
    };
    let lookup_attribute = if requires_unique {
        lookup_attribute(catalog, lookup)?
    } else {
        known_lookup_attribute(catalog, lookup)?
    };
    let target = target_attribute(catalog, triple)?;
    if lookup_attribute.etype != target.etype {
        return Err(ExecutionError::LookupEntityTypeMismatch {
            lookup_etype: lookup_attribute.etype.clone(),
            target_etype: target.etype.clone(),
        });
    }
    uses.push(lookup_use(app_id, lookup, creates_entity, requires_unique));
    Ok(())
}

fn collect_value_lookup(
    app_id: Uuid,
    attribute: &Attribute,
    value: &Value,
    requires_unique: bool,
    uses: &mut Vec<LookupUse>,
) {
    if (attribute.value_type == AttributeValueType::Ref || attribute.label == "id")
        && let Some(lookup) = value_lookup(value)
    {
        uses.push(lookup_use(app_id, &lookup, false, requires_unique));
    }
}

fn lookup_use(
    app_id: Uuid,
    lookup: &LookupRef,
    creates_entity: bool,
    requires_unique: bool,
) -> LookupUse {
    LookupUse {
        lookup: lookup.clone(),
        creates_entity,
        requires_unique,
        validates_attribute: true,
        lock_key: format!(
            "instantdb-lookup:{app_id}:{}:{}",
            lookup.attr_id, lookup.value
        ),
    }
}

fn unvalidated_lookup_use(app_id: Uuid, lookup: &LookupRef) -> LookupUse {
    let mut lookup_use = lookup_use(app_id, lookup, false, true);
    lookup_use.validates_attribute = false;
    lookup_use
}

fn rewrite_step(
    catalog: &AttributeCatalog,
    step: &TransactionStep,
    resolved: &[(LookupRef, Uuid)],
    rewritten: &mut Vec<TransactionStep>,
    missing_updates: &mut Vec<TransactionStep>,
) -> Result<(), ExecutionError> {
    match step {
        TransactionStep::AddTriple(triple) | TransactionStep::DeepMergeTriple(triple) => {
            let mut triple = triple.clone();
            if let EntityLookup::Lookup(lookup) = &triple.entity {
                let Some(entity_id) = resolved_lookup(resolved, lookup) else {
                    if triple.options.mode == Some(TransactionMode::Update) {
                        missing_updates.push(match step {
                            TransactionStep::AddTriple(_) => TransactionStep::AddTriple(triple),
                            TransactionStep::DeepMergeTriple(_) => {
                                TransactionStep::DeepMergeTriple(triple)
                            }
                            _ => unreachable!(),
                        });
                        return Ok(());
                    }
                    return Err(ExecutionError::MissingLookup(lookup.clone()));
                };
                triple.entity = EntityLookup::Id(entity_id);
            }
            materialize_resolved_value(catalog, &mut triple, resolved)?;
            rewritten.push(match step {
                TransactionStep::AddTriple(_) => TransactionStep::AddTriple(triple),
                TransactionStep::DeepMergeTriple(_) => TransactionStep::DeepMergeTriple(triple),
                _ => unreachable!(),
            });
        }
        TransactionStep::RetractTriple(triple) => {
            let mut triple = triple.clone();
            if let EntityLookup::Lookup(lookup) = &triple.entity {
                let Some(entity_id) = resolved_lookup(resolved, lookup) else {
                    return Ok(());
                };
                triple.entity = EntityLookup::Id(entity_id);
            }
            if !materialize_resolved_retract_value(catalog, &mut triple, resolved)? {
                return Ok(());
            }
            rewritten.push(TransactionStep::RetractTriple(triple));
        }
        TransactionStep::DeleteEntity(delete) => {
            let Some(entity) = resolved_entity(&delete.entity, resolved) else {
                return Ok(());
            };
            rewritten.push(TransactionStep::DeleteEntity(DeleteEntityStep {
                entity,
                etype: delete.etype.clone(),
            }));
        }
        TransactionStep::RuleParams(params) => {
            let Some(entity) = resolved_entity(&params.entity, resolved) else {
                return Ok(());
            };
            rewritten.push(TransactionStep::RuleParams(RuleParamsStep {
                entity,
                etype: params.etype.clone(),
                params: params.params.clone(),
            }));
        }
        _ => rewritten.push(step.clone()),
    }
    Ok(())
}

fn materialize_resolved_retract_value(
    catalog: &AttributeCatalog,
    triple: &mut TripleStep,
    resolved: &[(LookupRef, Uuid)],
) -> Result<bool, ExecutionError> {
    let attribute = target_attribute(catalog, triple)?;
    let lookupable = attribute.value_type == AttributeValueType::Ref || attribute.label == "id";
    if lookupable && let Some(lookup) = value_lookup(&triple.value) {
        let Some(entity_id) = resolved_lookup(resolved, &lookup) else {
            // The JVM's scalar lookup subquery yields SQL NULL here, so the
            // retract matches no row instead of turning idempotent cleanup into
            // a missing-lookup validation failure.
            return Ok(false);
        };
        triple.value = json!(entity_id);
    }
    // Unlike inserts, triple.clj/delete-multi! hashes arbitrary public values
    // without validating a reference UUID. An invalid direct ref is therefore
    // another no-match retract, not InvalidReferenceValue.
    Ok(true)
}

fn materialize_resolved_value(
    catalog: &AttributeCatalog,
    triple: &mut TripleStep,
    resolved: &[(LookupRef, Uuid)],
) -> Result<(), ExecutionError> {
    let attribute = target_attribute(catalog, triple)?;
    let lookupable = attribute.value_type == AttributeValueType::Ref || attribute.label == "id";
    if lookupable && let Some(lookup) = value_lookup(&triple.value) {
        let entity_id = resolved_lookup(resolved, &lookup)
            .ok_or_else(|| ExecutionError::MissingLookup(lookup.clone()))?;
        triple.value = json!(entity_id);
    }
    if attribute.value_type == AttributeValueType::Ref
        && triple
            .value
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .is_none()
    {
        return Err(ExecutionError::InvalidReferenceValue {
            attr_id: attribute.id,
            value: triple.value.clone(),
            entity_id: match &triple.entity {
                EntityLookup::Id(entity_id) => Some(*entity_id),
                EntityLookup::Lookup(_) => None,
            },
        });
    }
    Ok(())
}

async fn resolve_untyped_deletes(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    mut steps: Vec<TransactionStep>,
    append_untyped: bool,
) -> Result<Vec<TransactionStep>, ExecutionError> {
    if append_untyped {
        steps.sort_by_key(|step| usize::from(is_untyped_delete(step)));
    }
    let mut result = Vec::with_capacity(steps.len());
    for step in steps {
        let TransactionStep::DeleteEntity(delete) = &step else {
            result.push(step);
            continue;
        };
        if delete.etype.is_some() {
            result.push(step);
            continue;
        }
        let EntityLookup::Id(entity_id) = delete.entity else {
            unreachable!("prepared entity lookups are UUIDs");
        };
        let rows = transaction
            .query(
                "SELECT DISTINCT attr_id FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &entity_id],
            )
            .await?;
        let mut etypes = HashSet::new();
        for row in rows {
            let attr_id: Uuid = row.get("attr_id");
            if let Some(attribute) = catalog.by_id(attr_id) {
                etypes.insert(attribute.etype.clone());
            }
        }
        let mut etypes = etypes.into_iter().collect::<Vec<_>>();
        etypes.sort();
        result.extend(etypes.into_iter().map(|etype| {
            TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Id(entity_id),
                etype: Some(etype),
            })
        }));
    }
    Ok(result)
}

fn is_lookup_delete(step: &TransactionStep) -> bool {
    matches!(
        step,
        TransactionStep::DeleteEntity(DeleteEntityStep {
            entity: EntityLookup::Lookup(_),
            ..
        })
    )
}

fn is_untyped_delete(step: &TransactionStep) -> bool {
    matches!(
        step,
        TransactionStep::DeleteEntity(DeleteEntityStep { etype: None, .. })
    )
}

fn target_attribute<'a>(
    catalog: &'a AttributeCatalog,
    triple: &TripleStep,
) -> Result<&'a Attribute, ExecutionError> {
    catalog
        .by_id(triple.attr_id)
        .ok_or(ExecutionError::UnknownAttribute(triple.attr_id))
}

fn lookup_attribute<'a>(
    catalog: &'a AttributeCatalog,
    lookup: &LookupRef,
) -> Result<&'a Attribute, ExecutionError> {
    let attribute = known_lookup_attribute(catalog, lookup)?;
    if !attribute.unique {
        return Err(ExecutionError::LookupAttributeNotUnique(attribute.id));
    }
    Ok(attribute)
}

fn known_lookup_attribute<'a>(
    catalog: &'a AttributeCatalog,
    lookup: &LookupRef,
) -> Result<&'a Attribute, ExecutionError> {
    catalog
        .by_id(lookup.attr_id)
        .ok_or(ExecutionError::UnknownAttribute(lookup.attr_id))
}

async fn lookup_entity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    lookup: &LookupRef,
    requires_unique: bool,
) -> Result<Option<Uuid>, ExecutionError> {
    if requires_unique {
        transaction
            .query_opt(
                "SELECT entity_id FROM triples
                 WHERE app_id = $1 AND attr_id = $2 AND av AND value = $3
                 LIMIT 1",
                &[
                    &app_id,
                    &lookup.attr_id,
                    &tokio_postgres::types::Json(&lookup.value),
                ],
            )
            .await
            .map(|row| row.map(|row| row.get("entity_id")))
            .map_err(ExecutionError::from)
    } else {
        // Preserve the scalar subquery used by upstream delete-multi!: a
        // single ordinary (non-AV) triple resolves, no row yields null, and
        // ambiguous non-unique matches raise PostgreSQL cardinality violation.
        transaction
            .query_one(
                "SELECT (SELECT entity_id FROM triples
                         WHERE app_id = $1 AND attr_id = $2 AND value = $3) AS entity_id",
                &[
                    &app_id,
                    &lookup.attr_id,
                    &tokio_postgres::types::Json(&lookup.value),
                ],
            )
            .await
            .map(|row| row.get::<_, Option<Uuid>>("entity_id"))
            .map_err(ExecutionError::from)
    }
}

fn allocate_lookup_entity_id(
    attribute: &Attribute,
    lookup: &LookupRef,
) -> Result<Uuid, ExecutionError> {
    if attribute.label == "id" {
        return lookup
            .value
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| ExecutionError::InvalidReferenceValue {
                attr_id: attribute.id,
                value: lookup.value.clone(),
                entity_id: None,
            });
    }
    Ok(Uuid::new_v4())
}

fn resolved_entity(entity: &EntityLookup, resolved: &[(LookupRef, Uuid)]) -> Option<EntityLookup> {
    match entity {
        EntityLookup::Id(entity_id) => Some(EntityLookup::Id(*entity_id)),
        EntityLookup::Lookup(lookup) => resolved_lookup(resolved, lookup).map(EntityLookup::Id),
    }
}

fn resolved_lookup(resolved: &[(LookupRef, Uuid)], lookup: &LookupRef) -> Option<Uuid> {
    resolved
        .iter()
        .find(|(candidate, _)| candidate == lookup)
        .map(|(_, entity_id)| *entity_id)
}

fn value_lookup(value: &Value) -> Option<LookupRef> {
    let [attr_id, value] = value.as_array()?.as_slice() else {
        return None;
    };
    Some(LookupRef {
        attr_id: Uuid::parse_str(attr_id.as_str()?).ok()?,
        value: value.clone(),
    })
}

#[cfg(test)]
mod tests {
    use instant_instaql::AttributeCatalog;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        collect_add_group_lookup_refs, collect_lookup_uses, earlier_same_add_group_id_value_lookup,
        lookup_creation_operation, materialize_resolved_retract_value,
    };
    use crate::{
        EntityLookup, LookupRef, RuleParamsStep, TransactionMode, TransactionOperation,
        TransactionStep, TripleOptions, TripleStep, compile_transaction,
    };

    #[test]
    fn rule_params_before_create_does_not_hide_the_lookup_creation_group() {
        let lookup = LookupRef {
            attr_id: Uuid::from_u128(1),
            value: json!("later@example.com"),
        };
        let steps = vec![
            TransactionStep::RuleParams(RuleParamsStep {
                entity: EntityLookup::Lookup(lookup.clone()),
                etype: Some("users".to_owned()),
                params: serde_json::Map::new(),
            }),
            TransactionStep::AddTriple(TripleStep {
                entity: EntityLookup::Lookup(lookup.clone()),
                etype: Some("users".to_owned()),
                attr_id: Uuid::from_u128(2),
                reverse_etype: None,
                value: json!([lookup.attr_id, lookup.value]),
                options: TripleOptions {
                    mode: Some(TransactionMode::Create),
                },
            }),
        ];
        assert_eq!(
            lookup_creation_operation(&AttributeCatalog::default(), &steps, &lookup),
            Some(TransactionOperation::AddTriple)
        );
    }
    #[test]
    fn retract_values_preserve_the_jvm_idempotent_no_match_contract() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:1262-1322";
        assert!(ORACLE.contains("4e0a1bd8"));
        let ref_attr = Uuid::from_u128(1);
        let lookup_attr = Uuid::from_u128(2);
        let definitions = compile_transaction(
            &json!([
                ["add-attr", {
                    "id": lookup_attr,
                    "forward-identity": [Uuid::from_u128(3), "shelves", "id"],
                    "value-type": "blob",
                    "cardinality": "one",
                    "unique?": true,
                    "index?": false
                }],
                ["add-attr", {
                    "id": ref_attr,
                    "forward-identity": [Uuid::from_u128(4), "users", "shelf"],
                    "reverse-identity": [Uuid::from_u128(5), "shelves", "owner"],
                    "value-type": "ref",
                    "cardinality": "one",
                    "unique?": false,
                    "index?": false
                }]
            ]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let catalog = AttributeCatalog {
            attributes: definitions
                .iter()
                .filter_map(|step| match step {
                    TransactionStep::AddAttr(definition) => Some(definition.attribute.clone()),
                    _ => None,
                })
                .collect(),
        };
        let entity = EntityLookup::Id(Uuid::from_u128(6));
        let triple = |value| TripleStep {
            entity: entity.clone(),
            etype: Some("users".to_owned()),
            attr_id: ref_attr,
            reverse_etype: Some("shelves".to_owned()),
            value,
            options: TripleOptions::default(),
        };

        let mut invalid_direct = triple(json!("not-a-uuid"));
        assert!(materialize_resolved_retract_value(&catalog, &mut invalid_direct, &[]).unwrap());
        assert_eq!(invalid_direct.value, json!("not-a-uuid"));

        let lookup = LookupRef {
            attr_id: lookup_attr,
            value: json!("missing"),
        };
        let mut missing_lookup = triple(json!([lookup_attr, "missing"]));
        assert!(!materialize_resolved_retract_value(&catalog, &mut missing_lookup, &[]).unwrap());

        let resolved_id = Uuid::from_u128(7);
        let mut resolved_lookup = triple(json!([lookup_attr, "missing"]));
        assert!(
            materialize_resolved_retract_value(
                &catalog,
                &mut resolved_lookup,
                &[(lookup, resolved_id)],
            )
            .unwrap()
        );
        assert_eq!(resolved_lookup.value, json!(resolved_id));
    }

    #[test]
    fn retract_only_non_unique_lookups_are_admitted_but_adds_remain_strict() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:1262-1322";
        assert!(ORACLE.contains("4e0a1bd8"));
        let lookup_attr = Uuid::from_u128(20);
        let target_attr = Uuid::from_u128(21);
        let definitions = compile_transaction(
            &json!([
                ["add-attr", {
                    "id": lookup_attr,
                    "forward-identity": [Uuid::from_u128(22), "users", "nickname"],
                    "value-type": "blob",
                    "cardinality": "one",
                    "unique?": false,
                    "index?": false
                }],
                ["add-attr", {
                    "id": target_attr,
                    "forward-identity": [Uuid::from_u128(23), "users", "name"],
                    "value-type": "blob",
                    "cardinality": "one",
                    "unique?": false,
                    "index?": false
                }]
            ]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let catalog = AttributeCatalog {
            attributes: definitions
                .iter()
                .filter_map(|step| match step {
                    TransactionStep::AddAttr(definition) => Some(definition.attribute.clone()),
                    _ => None,
                })
                .collect(),
        };
        let triple = TripleStep {
            entity: EntityLookup::Lookup(LookupRef {
                attr_id: lookup_attr,
                value: json!("alex"),
            }),
            etype: Some("users".to_owned()),
            attr_id: target_attr,
            reverse_etype: None,
            value: json!("Alex"),
            options: TripleOptions::default(),
        };

        let retract_uses = collect_lookup_uses(
            Uuid::from_u128(24),
            &catalog,
            &[TransactionStep::RetractTriple(triple.clone())],
        )
        .unwrap();
        assert_eq!(retract_uses.len(), 1);
        assert!(!retract_uses[0].requires_unique);

        let error = collect_lookup_uses(
            Uuid::from_u128(24),
            &catalog,
            &[TransactionStep::AddTriple(triple)],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            crate::ExecutionError::LookupAttributeNotUnique(id) if id == lookup_attr
        ));
    }

    #[test]
    fn add_group_lookup_refs_retain_original_distinct_order() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:769-781";
        assert!(ORACLE.contains("4e0a1bd8"));
        let first = LookupRef {
            attr_id: Uuid::from_u128(9),
            value: json!("existing"),
        };
        let second = LookupRef {
            attr_id: Uuid::from_u128(1),
            value: json!("missing"),
        };
        let triple = |lookup: LookupRef| {
            TransactionStep::AddTriple(TripleStep {
                entity: EntityLookup::Lookup(lookup),
                etype: Some("users".to_owned()),
                attr_id: Uuid::from_u128(2),
                reverse_etype: None,
                value: json!("value"),
                options: TripleOptions::default(),
            })
        };
        let steps = vec![
            triple(first.clone()),
            triple(second.clone()),
            triple(first.clone()),
            TransactionStep::DeleteAttr(Uuid::from_u128(3)),
            triple(second.clone()),
            triple(first.clone()),
        ];
        assert_eq!(
            collect_add_group_lookup_refs(&steps),
            vec![vec![first.clone(), second.clone()], vec![second, first]]
        );
    }

    #[test]
    fn implicit_lookup_is_scheduled_with_its_first_creating_operation_group() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/db/transaction.clj:580-614 server/src/instant/db/model/triple.clj:440-520";
        assert!(ORACLE.contains("4e0a1bd8"));
        let lookup = LookupRef {
            attr_id: Uuid::from_u128(1),
            value: json!("new-user"),
        };
        let triple = |mode| TripleStep {
            entity: EntityLookup::Lookup(lookup.clone()),
            etype: Some("users".to_owned()),
            attr_id: Uuid::from_u128(2),
            reverse_etype: None,
            value: json!("profile"),
            options: TripleOptions { mode },
        };
        let same_group_steps = [
            TransactionStep::AddTriple(triple(Some(TransactionMode::Update))),
            TransactionStep::AddTriple(triple(None)),
        ];

        assert_eq!(
            lookup_creation_operation(&AttributeCatalog::default(), &same_group_steps, &lookup),
            Some(TransactionOperation::AddTriple),
            "one creating use makes the lookup visible to its entire operation group"
        );

        let target_attr = Uuid::from_u128(2);
        let definition = compile_transaction(
            &json!([["add-attr", {
                "id": target_attr,
                "forward-identity": [Uuid::from_u128(3), "users", "id"],
                "value-type": "blob",
                "cardinality": "one",
                "unique?": true,
                "index?": false
            }]]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let TransactionStep::AddAttr(definition) = &definition[0] else {
            panic!("expected add-attr")
        };
        let catalog = AttributeCatalog {
            attributes: vec![definition.attribute.clone()],
        };
        let earlier_value_use = TransactionStep::AddTriple(TripleStep {
            entity: EntityLookup::Id(Uuid::from_u128(4)),
            etype: Some("users".to_owned()),
            attr_id: target_attr,
            reverse_etype: None,
            value: json!([lookup.attr_id, lookup.value]),
            options: TripleOptions::default(),
        });
        assert_eq!(
            lookup_creation_operation(
                &catalog,
                &[
                    earlier_value_use,
                    TransactionStep::DeepMergeTriple(triple(None))
                ],
                &lookup,
            ),
            None,
            "a non-creating earlier group must fail rather than see a later implicit entity"
        );
    }

    #[test]
    fn id_value_lookup_resolves_only_from_an_earlier_triple_in_the_same_add_group() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:812-1156";
        assert!(ORACLE.contains("4e0a1bd8"));
        let id_attr = Uuid::from_u128(11);
        let link_attr = Uuid::from_u128(12);
        let target = Uuid::from_u128(13);
        let owner = Uuid::from_u128(14);
        let definitions = compile_transaction(
            &json!([
                ["add-attr", {
                    "id": id_attr,
                    "forward-identity": [Uuid::from_u128(21), "shelves", "id"],
                    "value-type": "blob",
                    "cardinality": "one",
                    "unique?": true,
                    "index?": false
                }],
                ["add-attr", {
                    "id": link_attr,
                    "forward-identity": [Uuid::from_u128(22), "users", "shelf"],
                    "reverse-identity": [Uuid::from_u128(23), "shelves", "owner"],
                    "value-type": "ref",
                    "cardinality": "one",
                    "unique?": false,
                    "index?": false
                }]
            ]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let catalog = AttributeCatalog {
            attributes: definitions
                .iter()
                .filter_map(|step| match step {
                    TransactionStep::AddAttr(definition) => Some(definition.attribute.clone()),
                    _ => None,
                })
                .collect(),
        };
        let lookup = LookupRef {
            attr_id: id_attr,
            value: json!(target),
        };
        let source = TransactionStep::AddTriple(TripleStep {
            entity: EntityLookup::Id(target),
            etype: Some("shelves".to_owned()),
            attr_id: id_attr,
            reverse_etype: None,
            value: json!(target),
            options: TripleOptions::default(),
        });
        let consumer = TransactionStep::AddTriple(TripleStep {
            entity: EntityLookup::Id(owner),
            etype: Some("users".to_owned()),
            attr_id: link_attr,
            reverse_etype: Some("shelves".to_owned()),
            value: json!([id_attr, target]),
            options: TripleOptions::default(),
        });

        assert_eq!(
            earlier_same_add_group_id_value_lookup(
                &catalog,
                &[source.clone(), consumer.clone()],
                &lookup,
            ),
            Some(target)
        );
        assert_eq!(
            earlier_same_add_group_id_value_lookup(
                &catalog,
                &[consumer.clone(), source.clone()],
                &lookup,
            ),
            None,
            "a later ID write cannot satisfy an earlier value lookup"
        );
        assert_eq!(
            earlier_same_add_group_id_value_lookup(
                &catalog,
                &[
                    source,
                    TransactionStep::DeleteAttr(Uuid::from_u128(99)),
                    consumer
                ],
                &lookup,
            ),
            None,
            "schema operations separate add groups"
        );
    }
}
