use uuid::Uuid;

use crate::{DeleteEntityStep, EntityLookup, LookupRef, TransactionStep};

/// Applies database lookup results to delete and rule-parameter operations.
///
/// Unresolved lookup deletes disappear, matching the JVM's no-op behavior.
/// Rule parameters are rewritten when their matching lookup resolves.
#[must_use]
pub fn resolve_delete_lookups(
    steps: &[TransactionStep],
    resolved: &[(LookupRef, Uuid)],
) -> Vec<TransactionStep> {
    let mut lookup_deletes = Vec::new();
    let mut rest = Vec::new();
    for step in steps {
        match step {
            TransactionStep::DeleteEntity(delete)
                if matches!(delete.entity, EntityLookup::Lookup(_)) =>
            {
                lookup_deletes.push(delete);
            }
            TransactionStep::RuleParams(params) => {
                let mut params = params.clone();
                if let EntityLookup::Lookup(lookup) = &params.entity
                    && let Some((_, entity_id)) =
                        resolved.iter().find(|(candidate, _)| candidate == lookup)
                {
                    params.entity = EntityLookup::Id(*entity_id);
                }
                rest.push(TransactionStep::RuleParams(params));
            }
            _ => rest.push(step.clone()),
        }
    }
    rest.extend(lookup_deletes.into_iter().filter_map(|delete| {
        let EntityLookup::Lookup(lookup) = &delete.entity else {
            return None;
        };
        resolved
            .iter()
            .find(|(candidate, _)| candidate == lookup)
            .map(|(_, entity_id)| {
                TransactionStep::DeleteEntity(DeleteEntityStep {
                    entity: EntityLookup::Id(*entity_id),
                    etype: delete.etype.clone(),
                })
            })
    }));
    rest
}

/// Expands untyped UUID deletes into one operation per resolved entity type.
///
/// A missing entity/type row retains one untyped delete, matching the JVM's
/// `[nil]` fallback and allowing the SQL layer to make it a no-op.
#[must_use]
pub fn resolve_delete_etypes(
    steps: &[TransactionStep],
    resolved: &[(Uuid, Vec<String>)],
) -> Vec<TransactionStep> {
    let mut untyped = Vec::new();
    let mut rest = Vec::new();
    for step in steps {
        match step {
            TransactionStep::DeleteEntity(delete)
                if delete.etype.is_none() && matches!(delete.entity, EntityLookup::Id(_)) =>
            {
                untyped.push(delete);
            }
            _ => rest.push(step.clone()),
        }
    }
    for delete in untyped {
        let EntityLookup::Id(entity_id) = delete.entity else {
            continue;
        };
        let etypes = resolved
            .iter()
            .find(|(candidate, _)| *candidate == entity_id)
            .map_or(&[][..], |(_, etypes)| etypes.as_slice());
        if etypes.is_empty() {
            rest.push(TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Id(entity_id),
                etype: None,
            }));
        } else {
            rest.extend(etypes.iter().map(|etype| {
                TransactionStep::DeleteEntity(DeleteEntityStep {
                    entity: EntityLookup::Id(entity_id),
                    etype: Some(etype.clone()),
                })
            }));
        }
    }
    rest
}
