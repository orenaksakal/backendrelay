use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType};
use instant_transact::{
    DEFAULT_TRANSACTION_STATEMENT_TIMEOUT, ExecutionError, TransactionOptions, TransactionReceipt,
    TransactionStep, TripleStep, configure_transaction_statement_timeout,
    execute_prepared_transaction_in, group_transaction_steps, optimistic_catalog,
    prepare_transaction_lookups_with_source_order, resolve_delete_lookups,
    validate_transaction_modes,
};
use serde_json::{Map, Value as JsonValue, json};
use uuid::Uuid;

use crate::{
    EvaluationBindings, EvaluationItem, PermissionAction, PermissionEntityKey, PermissionError,
    RefCache, RefRequest, RuleProgram, RuleSet, TransactionEntityMaps, evaluate_batch_async,
    transaction_modified_fields,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PermissionCheckPhase {
    PreWrite,
    PostCreate,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PermissionCheckScope {
    Object,
    Attribute,
}

#[derive(Debug)]
pub struct PlannedPermissionCheck {
    pub phase: PermissionCheckPhase,
    pub scope: PermissionCheckScope,
    pub action: PermissionAction,
    pub etype: String,
    pub entity_id: Option<Uuid>,
    pub program: Option<RuleProgram>,
    pub constant: Option<bool>,
    pub bindings: EvaluationBindings,
}

#[derive(Debug, Default)]
pub struct TransactionPermissionPlan {
    pub pre_write: Vec<PlannedPermissionCheck>,
    pub post_create: Vec<PlannedPermissionCheck>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionCheckResult {
    pub phase: PermissionCheckPhase,
    pub scope: PermissionCheckScope,
    pub action: PermissionAction,
    pub etype: String,
    pub entity_id: Option<Uuid>,
    pub program: Option<crate::PermissionProgramInfo>,
    pub allowed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PermissionedTransactionOptions {
    pub transaction: TransactionOptions,
    pub admin: bool,
    pub admin_check: bool,
    pub admin_dry_run: bool,
    pub rate_limit_consumer: Option<Arc<dyn crate::RateLimitConsumer>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionedTransactionReceipt {
    pub transaction: TransactionReceipt,
    pub checks: Vec<PermissionCheckResult>,
    pub all_checks_allowed: bool,
    pub committed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionedTransactionError {
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
}

pub type PermissionLoadFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PermissionError>> + Send + 'a>>;

/// Supplies complete entity objects and resolved CEL reference paths using the
/// same PostgreSQL transaction as the write.
pub trait PermissionDataLoader {
    fn load_entities<'a>(
        &'a self,
        transaction: &'a tokio_postgres::Transaction<'_>,
        entities: Vec<PermissionEntityKey>,
    ) -> PermissionLoadFuture<'a, std::collections::HashMap<PermissionEntityKey, JsonValue>>;

    fn load_refs<'a>(
        &'a self,
        transaction: &'a tokio_postgres::Transaction<'_>,
        refs: Vec<RefRequest>,
    ) -> PermissionLoadFuture<'a, RefCache>;
}

/// Runs permission checks and writes in one PostgreSQL transaction.
///
/// Normal requests roll back immediately on a denied pre-write or post-create
/// check. Admin-check mode returns every decision and rolls back only for a dry
/// run or when any check is denied, matching the JVM audit behavior. Admin mode
/// bypasses application rules.
///
/// # Errors
///
/// Returns permission, transaction-execution, loader, begin/commit, or rollback
/// errors. Any failure before commit attempts to roll back the transaction.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn execute_permissioned_transaction<L: PermissionDataLoader>(
    client: &mut tokio_postgres::Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    rules: &RuleSet,
    steps: &[TransactionStep],
    base_bindings: &EvaluationBindings,
    options: PermissionedTransactionOptions,
    loader: &L,
) -> Result<PermissionedTransactionReceipt, PermissionedTransactionError> {
    execute_permissioned_transaction_with_source_order(
        client,
        app_id,
        catalog,
        rules,
        steps,
        steps,
        base_bindings,
        options,
        loader,
    )
    .await
}

/// Executes grouped writes while preserving source order for permission
/// coercion, pre-check planning, denial selection, and audit results.
///
/// # Errors
///
/// Returns permission, transaction-execution, loader, PostgreSQL transaction,
/// or rollback errors from either ordered view.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn execute_permissioned_transaction_with_source_order<L: PermissionDataLoader>(
    client: &mut tokio_postgres::Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    rules: &RuleSet,
    grouped_steps: &[TransactionStep],
    source_steps: &[TransactionStep],
    base_bindings: &EvaluationBindings,
    options: PermissionedTransactionOptions,
    loader: &L,
) -> Result<PermissionedTransactionReceipt, PermissionedTransactionError> {
    let normalized_source_steps;
    let normalized_grouped_steps;
    let (initial_grouped_steps, initial_source_steps) = if options.admin {
        (grouped_steps, source_steps)
    } else {
        normalized_source_steps = coerce_permission_link_values(source_steps)?;
        normalized_grouped_steps = group_transaction_steps(&normalized_source_steps);
        (&normalized_grouped_steps[..], &normalized_source_steps[..])
    };
    let transaction = client.transaction().await?;
    configure_transaction_statement_timeout(&transaction, DEFAULT_TRANSACTION_STATEMENT_TIMEOUT)
        .await?;
    let preprocessed_source_steps;
    let preprocessed_grouped_steps;
    let (grouped_steps, source_steps) = if options.admin {
        (initial_grouped_steps, initial_source_steps)
    } else {
        preprocessed_source_steps =
            match preprocess_permission_delete_lookups(&transaction, app_id, initial_source_steps)
                .await
            {
                Ok(steps) => steps,
                Err(error) => {
                    transaction.rollback().await?;
                    return Err(error.into());
                }
            };
        let validation_catalog = optimistic_catalog(catalog, &preprocessed_source_steps);
        if let Err(error) =
            validate_permission_lookup_etypes(&validation_catalog, &preprocessed_source_steps)
        {
            transaction.rollback().await?;
            return Err(error.into());
        }
        preprocessed_grouped_steps = group_transaction_steps(&preprocessed_source_steps);
        (
            &preprocessed_grouped_steps[..],
            &preprocessed_source_steps[..],
        )
    };
    if let Err(error) =
        validate_transaction_modes(&transaction, app_id, catalog, grouped_steps).await
    {
        transaction.rollback().await?;
        return Err(error.into());
    }
    let prepared = match prepare_transaction_lookups_with_source_order(
        &transaction,
        app_id,
        catalog,
        grouped_steps,
        source_steps,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error.into());
        }
    };
    if options.admin {
        let receipt = match execute_prepared_transaction_in(
            &transaction,
            app_id,
            catalog,
            &prepared,
            options.transaction,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                transaction.rollback().await?;
                return Err(error.into());
            }
        };
        transaction.commit().await?;
        return Ok(PermissionedTransactionReceipt {
            transaction: receipt,
            checks: Vec::new(),
            all_checks_allowed: true,
            committed: true,
        });
    }
    let mut effective_bindings = base_bindings.clone();
    if let Some(consumer) = &options.rate_limit_consumer
        && let Err(error) =
            rules.configure_rate_limits(app_id, &mut effective_bindings, Arc::clone(consumer))
    {
        transaction.rollback().await?;
        return Err(error.into());
    }
    let permission_steps = prepared.permission_steps();
    let permission_catalog = &prepared.catalog;
    let entity_keys = transaction_permission_entity_keys(permission_catalog, &permission_steps)?;
    let original = match loader.load_entities(&transaction, entity_keys).await {
        Ok(entities) => entities,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error.into());
        }
    };
    let maps = match crate::project_transaction_entities(
        permission_catalog,
        &permission_steps,
        original,
    ) {
        Ok(maps) => maps,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error.into());
        }
    };
    let source_plan = match plan_transaction_permission_checks(
        rules,
        permission_catalog,
        &prepared.source_steps,
        &maps,
        &effective_bindings,
        false,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error.into());
        }
    };
    let execution_plan = match plan_transaction_permission_checks(
        rules,
        permission_catalog,
        &prepared.steps,
        &maps,
        &effective_bindings,
        false,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error.into());
        }
    };
    let plan = TransactionPermissionPlan {
        pre_write: source_plan.pre_write,
        post_create: execution_plan.post_create,
    };
    if options.rate_limit_consumer.is_none()
        && plan
            .pre_write
            .iter()
            .chain(&plan.post_create)
            .filter_map(|check| check.program.as_ref())
            .any(|program| !program.rate_limit_uses().is_empty())
    {
        transaction.rollback().await?;
        return Err(PermissionError::RateLimitUnavailable.into());
    }

    let mut refs = RefCache::new();
    let pre_results =
        match evaluate_transaction_checks_async(&plan.pre_write, &mut refs, |requests| {
            loader.load_refs(&transaction, requests)
        })
        .await
        {
            Ok(results) => results,
            Err(error) => {
                transaction.rollback().await?;
                return Err(error.into());
            }
        };
    if !options.admin_check
        && let Err(error) = require_transaction_permissions(&pre_results)
    {
        transaction.rollback().await?;
        return Err(error.into());
    }

    let receipt = match execute_prepared_transaction_in(
        &transaction,
        app_id,
        catalog,
        &prepared,
        options.transaction,
    )
    .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error.into());
        }
    };

    refs.clear();
    let post_results =
        match evaluate_transaction_checks_async(&plan.post_create, &mut refs, |requests| {
            loader.load_refs(&transaction, requests)
        })
        .await
        {
            Ok(results) => results,
            Err(error) => {
                transaction.rollback().await?;
                return Err(error.into());
            }
        };
    if !options.admin_check
        && let Err(error) = require_transaction_permissions(&post_results)
    {
        transaction.rollback().await?;
        return Err(error.into());
    }

    let checks = pre_results
        .into_iter()
        .chain(post_results)
        .collect::<Vec<_>>();
    let all_checks_allowed = checks.iter().all(|check| check.allowed);
    let rollback = options.admin_check && (options.admin_dry_run || !all_checks_allowed);
    if rollback {
        transaction.rollback().await?;
    } else {
        transaction.commit().await?;
    }
    Ok(PermissionedTransactionReceipt {
        transaction: receipt,
        checks,
        all_checks_allowed,
        committed: !rollback,
    })
}

async fn preprocess_permission_delete_lookups(
    transaction: &tokio_postgres::Transaction<'_>,
    app_id: Uuid,
    steps: &[TransactionStep],
) -> Result<Vec<TransactionStep>, ExecutionError> {
    let mut resolved = Vec::new();
    for lookup in steps.iter().filter_map(|step| match step {
        TransactionStep::DeleteEntity(delete) => match &delete.entity {
            instant_transact::EntityLookup::Lookup(lookup) => Some(lookup),
            instant_transact::EntityLookup::Id(_) => None,
        },
        _ => None,
    }) {
        if resolved
            .iter()
            .any(|(candidate, _): &(instant_transact::LookupRef, Uuid)| candidate == lookup)
        {
            continue;
        }
        let entity_id = transaction
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
            .await?
            .map(|row| row.get::<_, Uuid>(0));
        if let Some(entity_id) = entity_id {
            resolved.push((lookup.clone(), entity_id));
        }
    }
    Ok(resolve_delete_lookups(steps, &resolved))
}

fn coerce_permission_link_values(
    steps: &[TransactionStep],
) -> Result<Vec<TransactionStep>, PermissionError> {
    steps
        .iter()
        .cloned()
        .map(|step| match step {
            TransactionStep::AddTriple(triple) => {
                coerce_permission_link_value("add-triple", triple, TransactionStep::AddTriple)
            }
            TransactionStep::DeepMergeTriple(triple) => coerce_permission_link_value(
                "deep-merge-triple",
                triple,
                TransactionStep::DeepMergeTriple,
            ),
            TransactionStep::RetractTriple(triple) => coerce_permission_link_value(
                "retract-triple",
                triple,
                TransactionStep::RetractTriple,
            ),
            step => Ok(step),
        })
        .collect()
}

fn coerce_permission_link_value(
    operation: &str,
    mut triple: TripleStep,
    wrap: impl FnOnce(TripleStep) -> TransactionStep,
) -> Result<TransactionStep, PermissionError> {
    if triple.reverse_etype.is_none() || triple.value.is_array() {
        return Ok(wrap(triple));
    }
    let value = triple.value.clone();
    let parsed = value.as_str().and_then(coerce_java_uuid);
    let Some(parsed) = parsed else {
        return Err(PermissionError::InvalidTransactionLinkValue {
            value: value.clone(),
            tx_step: triple_wire_step(operation, &triple, &value),
        });
    };
    triple.value = json!(parsed);
    Ok(wrap(triple))
}

fn validate_permission_lookup_etypes(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<(), PermissionError> {
    for step in steps {
        let candidate = match step {
            TransactionStep::AddTriple(triple) => {
                lookup_validation_step("add-triple", triple, triple.etype.as_deref())
            }
            TransactionStep::DeepMergeTriple(triple) => {
                lookup_validation_step("deep-merge-triple", triple, triple.etype.as_deref())
            }
            TransactionStep::RetractTriple(triple) => {
                lookup_validation_step("retract-triple", triple, triple.etype.as_deref())
            }
            TransactionStep::RuleParams(params) => {
                let instant_transact::EntityLookup::Lookup(lookup) = &params.entity else {
                    continue;
                };
                Some((
                    lookup,
                    params.etype.as_deref(),
                    json!([
                        "rule-params",
                        [lookup.attr_id, lookup.value],
                        params.etype,
                        params.params
                    ]),
                ))
            }
            // Upstream resolves delete lookups before this validation pass.
            TransactionStep::DeleteEntity(_)
            | TransactionStep::AddAttr(_)
            | TransactionStep::UpdateAttr(_)
            | TransactionStep::DeleteAttr(_)
            | TransactionStep::RestoreAttr(_) => continue,
        };
        let Some((lookup, target_etype, tx_step)) = candidate else {
            continue;
        };
        let lookup_value = json!([lookup.attr_id, lookup.value]);
        let Some(attribute) = catalog.by_id(lookup.attr_id) else {
            let message =
                "Invalid lookup. Could not determine namespace from lookup attribute.".to_owned();
            return Err(PermissionError::InvalidTransactionLookupType {
                data_type: "lookup".to_owned(),
                input: lookup_value,
                message,
                tx_step,
                include_tx_step_in_error: true,
            });
        };
        if target_etype != Some(attribute.etype.as_str()) {
            let message = "Invalid transaction. The namespace in the lookup attribute is different from the namespace of the attribute that is being set".to_owned();
            return Err(PermissionError::InvalidTransactionLookupType {
                data_type: "tx-step".to_owned(),
                input: tx_step.clone(),
                message,
                tx_step,
                include_tx_step_in_error: false,
            });
        }
    }
    Ok(())
}

fn lookup_validation_step<'a>(
    operation: &str,
    triple: &'a TripleStep,
    target_etype: Option<&'a str>,
) -> Option<(&'a instant_transact::LookupRef, Option<&'a str>, JsonValue)> {
    let instant_transact::EntityLookup::Lookup(lookup) = &triple.entity else {
        return None;
    };
    Some((
        lookup,
        target_etype,
        triple_wire_step(operation, triple, &triple.value),
    ))
}

fn triple_wire_step(operation: &str, triple: &TripleStep, value: &JsonValue) -> JsonValue {
    let entity = match &triple.entity {
        instant_transact::EntityLookup::Id(entity_id) => json!(entity_id),
        instant_transact::EntityLookup::Lookup(lookup) => {
            json!([lookup.attr_id, lookup.value])
        }
    };
    let options = triple
        .options
        .mode
        .map_or(JsonValue::Null, |mode| json!({"mode": mode}));
    json!([operation, entity, triple.attr_id, value, options])
}

fn coerce_java_uuid(value: &str) -> Option<Uuid> {
    let value = value.trim_matches(|character| character <= '\u{20}');
    if value.len() != 36 {
        return None;
    }
    let mut parts = value.split('-');
    let time_low = i64::from_str_radix(parts.next()?, 16).ok()?.cast_unsigned();
    let time_mid = i64::from_str_radix(parts.next()?, 16).ok()?.cast_unsigned();
    let version = i64::from_str_radix(parts.next()?, 16).ok()?.cast_unsigned();
    let variant = i64::from_str_radix(parts.next()?, 16).ok()?.cast_unsigned();
    let node = i64::from_str_radix(parts.next()?, 16).ok()?.cast_unsigned();
    if parts.next().is_some() {
        return None;
    }
    let most = ((time_low & 0xffff_ffff) << 32) | ((time_mid & 0xffff) << 16) | (version & 0xffff);
    let least = ((variant & 0xffff) << 48) | (node & 0xffff_ffff_ffff);
    Some(Uuid::from_u128(
        (u128::from(most) << 64) | u128::from(least),
    ))
}

/// Collects every forward and reverse entity that permission planning must
/// preload. Transaction lookups must already be resolved to UUIDs.
///
/// # Errors
///
/// Returns an error for unknown attributes, unresolved lookups, incomplete
/// relationship metadata, or malformed reference UUIDs.
pub fn transaction_permission_entity_keys(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<Vec<PermissionEntityKey>, PermissionError> {
    let mut keys = HashSet::new();
    for step in steps {
        match step {
            TransactionStep::AddTriple(triple)
            | TransactionStep::DeepMergeTriple(triple)
            | TransactionStep::RetractTriple(triple) => {
                let attribute = attribute(catalog, triple)?;
                keys.insert(entity_key(&attribute.etype, entity_id(&triple.entity)?));
                if attribute.value_type == AttributeValueType::Ref {
                    keys.insert(relationship_context(attribute, triple)?.reverse_key);
                }
            }
            TransactionStep::DeleteEntity(delete) => {
                let etype = delete
                    .etype
                    .as_deref()
                    .ok_or(PermissionError::MissingTransactionEntityType)?;
                keys.insert(entity_key(etype, entity_id(&delete.entity)?));
            }
            TransactionStep::RuleParams(params) => {
                let etype = params
                    .etype
                    .as_deref()
                    .ok_or(PermissionError::MissingTransactionEntityType)?;
                keys.insert(entity_key(etype, entity_id(&params.entity)?));
            }
            TransactionStep::AddAttr(_)
            | TransactionStep::UpdateAttr(_)
            | TransactionStep::DeleteAttr(_)
            | TransactionStep::RestoreAttr(_) => {}
        }
    }
    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort();
    Ok(keys)
}

/// Builds the JVM permissioned-transaction pre-write and post-create check sets
/// from resolved transaction steps and projected entity maps.
///
/// Link and unlink rules are checked on both relationship directions. When no
/// specialized relationship rule exists, the JVM update/view/create fallbacks
/// are emitted. Repeated equivalent checks are evaluated once.
///
/// # Errors
///
/// Returns an error for unresolved lookups, malformed reference values,
/// incomplete reference metadata, invalid rules, or unknown attributes.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn plan_transaction_permission_checks(
    rules: &RuleSet,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    maps: &TransactionEntityMaps,
    base_bindings: &EvaluationBindings,
    admin: bool,
) -> Result<TransactionPermissionPlan, PermissionError> {
    let mut plan = TransactionPermissionPlan::default();
    let mut pre_seen = HashSet::new();
    let mut post_seen = HashSet::new();

    for step in steps {
        match step {
            TransactionStep::UpdateAttr(_) => push_constant(
                &mut plan.pre_write,
                &mut pre_seen,
                PermissionCheckPhase::PreWrite,
                PermissionAction::Update,
                PermissionCheckScope::Attribute,
                "attrs",
                None,
                admin,
            ),
            TransactionStep::DeleteAttr(_) => push_constant(
                &mut plan.pre_write,
                &mut pre_seen,
                PermissionCheckPhase::PreWrite,
                PermissionAction::Delete,
                PermissionCheckScope::Attribute,
                "attrs",
                None,
                admin,
            ),
            TransactionStep::RestoreAttr(_) => push_constant(
                &mut plan.pre_write,
                &mut pre_seen,
                PermissionCheckPhase::PreWrite,
                PermissionAction::Restore,
                PermissionCheckScope::Attribute,
                "attrs",
                None,
                admin,
            ),
            TransactionStep::AddAttr(definition) => {
                let data = serde_json::to_value(definition)
                    .map_err(|error| PermissionError::Binding(error.to_string()))?;
                let mut bindings = base_bindings.clone();
                bindings.data = data;
                bindings.data_etype = Some("attrs".to_owned());
                push_rule(
                    &mut plan.post_create,
                    &mut post_seen,
                    PlannedPermissionCheck {
                        phase: PermissionCheckPhase::PostCreate,
                        scope: PermissionCheckScope::Attribute,
                        action: PermissionAction::Create,
                        etype: "attrs".to_owned(),
                        entity_id: None,
                        program: rules.program("attrs", PermissionAction::Create)?,
                        constant: None,
                        bindings,
                    },
                );
            }
            TransactionStep::AddTriple(triple) => {
                plan_add_checks(
                    rules,
                    catalog,
                    steps,
                    maps,
                    base_bindings,
                    triple,
                    &mut plan,
                    &mut pre_seen,
                    &mut post_seen,
                )?;
            }
            TransactionStep::DeepMergeTriple(triple) => {
                plan_scalar_write_checks(
                    rules,
                    catalog,
                    steps,
                    maps,
                    base_bindings,
                    triple,
                    &mut plan,
                    &mut pre_seen,
                    &mut post_seen,
                )?;
            }
            TransactionStep::RetractTriple(triple) => {
                plan_retract_checks(
                    rules,
                    catalog,
                    steps,
                    maps,
                    base_bindings,
                    triple,
                    &mut plan.pre_write,
                    &mut pre_seen,
                )?;
            }
            TransactionStep::DeleteEntity(delete) => {
                let entity_id = entity_id(&delete.entity)?;
                let etype = delete
                    .etype
                    .as_deref()
                    .ok_or(PermissionError::MissingTransactionEntityType)?;
                let key = entity_key(etype, entity_id);
                let bindings = object_bindings(base_bindings, maps, &key, false, None, None);
                push_rule(
                    &mut plan.pre_write,
                    &mut pre_seen,
                    PlannedPermissionCheck {
                        phase: PermissionCheckPhase::PreWrite,
                        scope: PermissionCheckScope::Object,
                        action: PermissionAction::Delete,
                        etype: etype.to_owned(),
                        entity_id: Some(entity_id),
                        program: rules.program(etype, PermissionAction::Delete)?,
                        constant: None,
                        bindings,
                    },
                );
            }
            TransactionStep::RuleParams(_) => {}
        }
    }
    Ok(plan)
}

#[allow(
    clippy::collapsible_if,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn plan_add_checks(
    rules: &RuleSet,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    maps: &TransactionEntityMaps,
    base: &EvaluationBindings,
    triple: &TripleStep,
    plan: &mut TransactionPermissionPlan,
    pre_seen: &mut HashSet<String>,
    post_seen: &mut HashSet<String>,
) -> Result<(), PermissionError> {
    let attribute = attribute(catalog, triple)?;
    if attribute.value_type != AttributeValueType::Ref {
        return plan_scalar_write_checks(
            rules, catalog, steps, maps, base, triple, plan, pre_seen, post_seen,
        );
    }
    let relationship = relationship_context(attribute, triple)?;
    let forward_link =
        rules.relationship_program(&attribute.etype, PermissionAction::Link, &attribute.label)?;
    let reverse_link = rules.relationship_program(
        &relationship.reverse_key.etype,
        PermissionAction::Link,
        &relationship.reverse_label,
    )?;
    let specialized = forward_link.is_some() || reverse_link.is_some();
    let forward_exists = maps.original.contains_key(&relationship.forward_key);
    let reverse_exists = maps.original.contains_key(&relationship.reverse_key);

    if specialized {
        if forward_exists {
            if let Some(program) = forward_link {
                let bindings = link_bindings(
                    base,
                    maps,
                    &relationship.forward_key,
                    &relationship.reverse_key,
                    false,
                    true,
                    reverse_exists,
                );
                push_relationship_check(
                    &mut plan.pre_write,
                    pre_seen,
                    PermissionCheckPhase::PreWrite,
                    PermissionAction::Link,
                    relationship.forward_key.clone(),
                    program,
                    bindings,
                );
            }
        }
        if reverse_exists {
            if let Some(program) = reverse_link {
                let mut bindings = link_bindings(
                    base,
                    maps,
                    &relationship.reverse_key,
                    &relationship.forward_key,
                    true,
                    true,
                    forward_exists,
                );
                if let Some(data) = bindings.data.as_object_mut() {
                    data.insert("$action".to_owned(), json!("update"));
                }
                push_relationship_check(
                    &mut plan.pre_write,
                    pre_seen,
                    PermissionCheckPhase::PreWrite,
                    PermissionAction::Link,
                    relationship.reverse_key.clone(),
                    program,
                    bindings,
                );
            }
        }
    } else {
        if forward_exists {
            push_object_fallback(
                rules,
                catalog,
                steps,
                maps,
                base,
                &relationship.forward_key,
                Some(&relationship.reverse_key),
                PermissionCheckPhase::PreWrite,
                PermissionAction::Update,
                &mut plan.pre_write,
                pre_seen,
            )?;
        }
        if reverse_exists {
            push_object_fallback(
                rules,
                catalog,
                steps,
                maps,
                base,
                &relationship.reverse_key,
                Some(&relationship.forward_key),
                PermissionCheckPhase::PreWrite,
                PermissionAction::View,
                &mut plan.pre_write,
                pre_seen,
            )?;
        }
    }

    let forward_create = !forward_exists;
    let reverse_create = !reverse_exists && maps.updated.contains_key(&relationship.reverse_key);
    if specialized {
        if forward_create {
            if let Some(program) = rules.relationship_program(
                &attribute.etype,
                PermissionAction::Link,
                &attribute.label,
            )? {
                let bindings = link_bindings(
                    base,
                    maps,
                    &relationship.forward_key,
                    &relationship.reverse_key,
                    false,
                    false,
                    reverse_exists,
                );
                push_relationship_check(
                    &mut plan.post_create,
                    post_seen,
                    PermissionCheckPhase::PostCreate,
                    PermissionAction::Link,
                    relationship.forward_key.clone(),
                    program,
                    bindings,
                );
            }
        }
        if reverse_create {
            if let Some(program) = rules.relationship_program(
                &relationship.reverse_key.etype,
                PermissionAction::Link,
                &relationship.reverse_label,
            )? {
                let bindings = link_bindings(
                    base,
                    maps,
                    &relationship.reverse_key,
                    &relationship.forward_key,
                    true,
                    false,
                    forward_exists,
                );
                push_relationship_check(
                    &mut plan.post_create,
                    post_seen,
                    PermissionCheckPhase::PostCreate,
                    PermissionAction::Link,
                    relationship.reverse_key.clone(),
                    program,
                    bindings,
                );
            }
        }
    } else {
        if forward_create {
            reject_direct_user_create(&relationship.forward_key)?;
            push_object_fallback(
                rules,
                catalog,
                steps,
                maps,
                base,
                &relationship.forward_key,
                Some(&relationship.reverse_key),
                PermissionCheckPhase::PostCreate,
                PermissionAction::Create,
                &mut plan.post_create,
                post_seen,
            )?;
        }
        if reverse_create {
            push_object_fallback(
                rules,
                catalog,
                steps,
                maps,
                base,
                &relationship.reverse_key,
                Some(&relationship.forward_key),
                PermissionCheckPhase::PostCreate,
                PermissionAction::View,
                &mut plan.post_create,
                post_seen,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn plan_scalar_write_checks(
    rules: &RuleSet,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    maps: &TransactionEntityMaps,
    base: &EvaluationBindings,
    triple: &TripleStep,
    plan: &mut TransactionPermissionPlan,
    pre_seen: &mut HashSet<String>,
    post_seen: &mut HashSet<String>,
) -> Result<(), PermissionError> {
    let attribute = attribute(catalog, triple)?;
    let key = entity_key(&attribute.etype, entity_id(&triple.entity)?);
    if maps.original.contains_key(&key) {
        push_object_fallback(
            rules,
            catalog,
            steps,
            maps,
            base,
            &key,
            None,
            PermissionCheckPhase::PreWrite,
            PermissionAction::Update,
            &mut plan.pre_write,
            pre_seen,
        )?;
    } else {
        reject_direct_user_create(&key)?;
        push_object_fallback(
            rules,
            catalog,
            steps,
            maps,
            base,
            &key,
            None,
            PermissionCheckPhase::PostCreate,
            PermissionAction::Create,
            &mut plan.post_create,
            post_seen,
        )?;
    }
    Ok(())
}

#[allow(clippy::collapsible_if, clippy::too_many_arguments)]
fn plan_retract_checks(
    rules: &RuleSet,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    maps: &TransactionEntityMaps,
    base: &EvaluationBindings,
    triple: &TripleStep,
    checks: &mut Vec<PlannedPermissionCheck>,
    seen: &mut HashSet<String>,
) -> Result<(), PermissionError> {
    let attribute = attribute(catalog, triple)?;
    if attribute.value_type != AttributeValueType::Ref {
        return Ok(());
    }
    let relationship = relationship_context(attribute, triple)?;
    let forward =
        rules.relationship_program(&attribute.etype, PermissionAction::Unlink, &attribute.label)?;
    let reverse = rules.relationship_program(
        &relationship.reverse_key.etype,
        PermissionAction::Unlink,
        &relationship.reverse_label,
    )?;
    let specialized = forward.is_some() || reverse.is_some();
    if specialized {
        if maps.original.contains_key(&relationship.forward_key) {
            if let Some(program) = forward {
                let bindings = unlink_bindings(
                    base,
                    maps,
                    &relationship.forward_key,
                    &relationship.reverse_key,
                );
                push_relationship_check(
                    checks,
                    seen,
                    PermissionCheckPhase::PreWrite,
                    PermissionAction::Unlink,
                    relationship.forward_key.clone(),
                    program,
                    bindings,
                );
            }
        }
        if maps.original.contains_key(&relationship.reverse_key) {
            if let Some(program) = reverse {
                let bindings = unlink_bindings(
                    base,
                    maps,
                    &relationship.reverse_key,
                    &relationship.forward_key,
                );
                push_relationship_check(
                    checks,
                    seen,
                    PermissionCheckPhase::PreWrite,
                    PermissionAction::Unlink,
                    relationship.reverse_key.clone(),
                    program,
                    bindings,
                );
            }
        }
    } else {
        if maps.original.contains_key(&relationship.forward_key) {
            push_object_fallback(
                rules,
                catalog,
                steps,
                maps,
                base,
                &relationship.forward_key,
                Some(&relationship.reverse_key),
                PermissionCheckPhase::PreWrite,
                PermissionAction::Update,
                checks,
                seen,
            )?;
        }
        if maps.original.contains_key(&relationship.reverse_key) {
            push_object_fallback(
                rules,
                catalog,
                steps,
                maps,
                base,
                &relationship.reverse_key,
                Some(&relationship.forward_key),
                PermissionCheckPhase::PreWrite,
                PermissionAction::View,
                checks,
                seen,
            )?;
        }
    }
    Ok(())
}

struct RelationshipContext {
    forward_key: PermissionEntityKey,
    reverse_key: PermissionEntityKey,
    reverse_label: String,
}

fn relationship_context(
    attribute: &Attribute,
    triple: &TripleStep,
) -> Result<RelationshipContext, PermissionError> {
    let reverse_etype = attribute
        .reverse_etype
        .as_deref()
        .or(triple.reverse_etype.as_deref())
        .ok_or(PermissionError::IncompleteRelationship)?;
    let reverse_label = attribute
        .reverse_label
        .clone()
        .ok_or(PermissionError::IncompleteRelationship)?;
    Ok(RelationshipContext {
        forward_key: entity_key(&attribute.etype, entity_id(&triple.entity)?),
        reverse_key: entity_key(reverse_etype, reference_id(&triple.value)?),
        reverse_label,
    })
}

fn object_bindings(
    base: &EvaluationBindings,
    maps: &TransactionEntityMaps,
    key: &PermissionEntityKey,
    include_new: bool,
    linked: Option<(&PermissionEntityKey, bool)>,
    actions: Option<JsonValue>,
) -> EvaluationBindings {
    let mut bindings = base.clone();
    bindings.data = maps
        .original
        .get(key)
        .or_else(|| maps.updated.get(key))
        .cloned()
        .unwrap_or(JsonValue::Null);
    bindings.data_etype = Some(key.etype.clone());
    bindings.rule_params = maps
        .rule_params
        .get(key)
        .cloned()
        .unwrap_or_else(|| JsonValue::Object(Map::new()));
    if include_new {
        bindings.new_data = maps.updated.get(key).cloned();
    }
    if let Some((linked_key, updated)) = linked {
        bindings.linked_data = if updated {
            maps.updated.get(linked_key).cloned()
        } else {
            maps.original.get(linked_key).cloned()
        };
        bindings.linked_data_etype = Some(linked_key.etype.clone());
    }
    bindings.actions = actions;
    bindings
}

fn link_bindings(
    base: &EvaluationBindings,
    maps: &TransactionEntityMaps,
    key: &PermissionEntityKey,
    linked_key: &PermissionEntityKey,
    reverse: bool,
    pre_write: bool,
    linked_existed: bool,
) -> EvaluationBindings {
    let mut bindings = object_bindings(
        base,
        maps,
        key,
        true,
        Some((linked_key, reverse || !pre_write)),
        Some(json!({
            "data": if pre_write { "update" } else { "create" },
            "linkedData": if linked_existed { "update" } else { "create" }
        })),
    );
    bindings.rule_params = merge_rule_params(maps, linked_key, key);
    if !pre_write {
        bindings.data = maps.updated.get(key).cloned().unwrap_or(JsonValue::Null);
        bindings.new_data = maps.updated.get(key).cloned();
        synthesize_entity_id(&mut bindings.data, key.entity_id);
        if let Some(new_data) = &mut bindings.new_data {
            synthesize_entity_id(new_data, key.entity_id);
        }
        if let Some(linked_data) = &mut bindings.linked_data {
            synthesize_entity_id(linked_data, linked_key.entity_id);
        }
    }
    bindings
}

fn unlink_bindings(
    base: &EvaluationBindings,
    maps: &TransactionEntityMaps,
    key: &PermissionEntityKey,
    linked_key: &PermissionEntityKey,
) -> EvaluationBindings {
    let mut bindings = object_bindings(base, maps, key, true, Some((linked_key, false)), None);
    bindings.rule_params = merge_rule_params(maps, linked_key, key);
    bindings
}

fn merge_rule_params(
    maps: &TransactionEntityMaps,
    first: &PermissionEntityKey,
    second: &PermissionEntityKey,
) -> JsonValue {
    let mut merged = Map::new();
    for key in [first, second] {
        if let Some(values) = maps.rule_params.get(key).and_then(JsonValue::as_object) {
            merged.extend(values.clone());
        }
    }
    JsonValue::Object(merged)
}

#[allow(clippy::too_many_arguments)]
fn push_object_fallback(
    rules: &RuleSet,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    maps: &TransactionEntityMaps,
    base: &EvaluationBindings,
    key: &PermissionEntityKey,
    related_key: Option<&PermissionEntityKey>,
    phase: PermissionCheckPhase,
    action: PermissionAction,
    checks: &mut Vec<PlannedPermissionCheck>,
    seen: &mut HashSet<String>,
) -> Result<(), PermissionError> {
    let relationship_create = phase == PermissionCheckPhase::PostCreate && related_key.is_some();
    let include_new = matches!(action, PermissionAction::Create | PermissionAction::Update)
        || relationship_create;
    let mut bindings = object_bindings(base, maps, key, include_new, None, None);
    if relationship_create {
        synthesize_entity_id(&mut bindings.data, key.entity_id);
        if let Some(new_data) = &mut bindings.new_data {
            synthesize_entity_id(new_data, key.entity_id);
        }
    }
    if let Some(related_key) = related_key {
        bindings.rule_params = merge_rule_params(maps, related_key, key);
    }
    if matches!(action, PermissionAction::Create | PermissionAction::Update) {
        bindings.request.modified_fields =
            transaction_modified_fields(key.entity_id, steps, catalog)?;
    }
    push_rule(
        checks,
        seen,
        PlannedPermissionCheck {
            phase,
            scope: PermissionCheckScope::Object,
            action,
            etype: key.etype.clone(),
            entity_id: Some(key.entity_id),
            program: rules.program(&key.etype, action)?,
            constant: None,
            bindings,
        },
    );
    Ok(())
}

fn synthesize_entity_id(entity: &mut JsonValue, entity_id: Uuid) {
    if let Some(object) = entity.as_object_mut()
        && object
            .get("id")
            .is_none_or(|value| value.is_null() || value == &JsonValue::Bool(false))
    {
        object.insert("id".to_owned(), json!(entity_id));
    }
}

#[allow(clippy::too_many_arguments)]
fn push_relationship_check(
    checks: &mut Vec<PlannedPermissionCheck>,
    seen: &mut HashSet<String>,
    phase: PermissionCheckPhase,
    action: PermissionAction,
    key: PermissionEntityKey,
    program: RuleProgram,
    bindings: EvaluationBindings,
) {
    push_rule(
        checks,
        seen,
        PlannedPermissionCheck {
            phase,
            scope: PermissionCheckScope::Object,
            action,
            etype: key.etype,
            entity_id: Some(key.entity_id),
            program: Some(program),
            constant: None,
            bindings,
        },
    );
}

fn push_rule(
    checks: &mut Vec<PlannedPermissionCheck>,
    seen: &mut HashSet<String>,
    mut check: PlannedPermissionCheck,
) {
    if check.program.is_none() && check.constant.is_none() {
        check.constant = Some(true);
    }
    if !seen.insert(check_dedup_key(&check)) {
        return;
    }
    checks.push(check);
}

fn check_dedup_key(check: &PlannedPermissionCheck) -> String {
    let bindings = json!({
        "data": &check.bindings.data,
        "data-etype": &check.bindings.data_etype,
        "rule-params": &check.bindings.rule_params,
        "new-data": &check.bindings.new_data,
        "linked-data": &check.bindings.linked_data,
        "linked-data-etype": &check.bindings.linked_data_etype,
        "actions": &check.bindings.actions,
        "modified-fields": &check.bindings.request.modified_fields,
    });
    format!(
        "{:?}:{:?}:{:?}:{}:{:?}:{:?}:{:?}:{}",
        check.phase,
        check.scope,
        check.action,
        check.etype,
        check.entity_id,
        check.program.as_ref().map(RuleProgram::source),
        check.constant,
        bindings
    )
}

#[allow(clippy::too_many_arguments)]
fn push_constant(
    checks: &mut Vec<PlannedPermissionCheck>,
    seen: &mut HashSet<String>,
    phase: PermissionCheckPhase,
    action: PermissionAction,
    scope: PermissionCheckScope,
    etype: &str,
    entity_id: Option<Uuid>,
    allowed: bool,
) {
    push_rule(
        checks,
        seen,
        PlannedPermissionCheck {
            phase,
            scope,
            action,
            etype: etype.to_owned(),
            entity_id,
            program: None,
            constant: Some(allowed),
            bindings: EvaluationBindings::default(),
        },
    );
}

fn attribute<'a>(
    catalog: &'a AttributeCatalog,
    triple: &TripleStep,
) -> Result<&'a Attribute, PermissionError> {
    catalog
        .by_id(triple.attr_id)
        .ok_or(PermissionError::UnknownAttribute(triple.attr_id))
}

fn entity_id(entity: &instant_transact::EntityLookup) -> Result<Uuid, PermissionError> {
    match entity {
        instant_transact::EntityLookup::Id(id) => Ok(*id),
        instant_transact::EntityLookup::Lookup(_) => {
            Err(PermissionError::UnresolvedTransactionLookup)
        }
    }
}

fn reference_id(value: &JsonValue) -> Result<Uuid, PermissionError> {
    value
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| PermissionError::InvalidTransactionReference(value.clone()))
}

fn entity_key(etype: &str, entity_id: Uuid) -> PermissionEntityKey {
    PermissionEntityKey {
        etype: etype.to_owned(),
        entity_id,
    }
}

fn reject_direct_user_create(key: &PermissionEntityKey) -> Result<(), PermissionError> {
    if key.etype == "$users" {
        Err(PermissionError::DirectUserCreate)
    } else {
        Ok(())
    }
}

/// Evaluates one phase's constants and CEL programs in original plan order.
///
/// # Errors
///
/// Propagates CEL and asynchronous reference-loader failures.
pub async fn evaluate_transaction_checks_async<F, Fut>(
    checks: &[PlannedPermissionCheck],
    refs: &mut RefCache,
    load_refs: F,
) -> Result<Vec<PermissionCheckResult>, PermissionError>
where
    F: FnMut(Vec<RefRequest>) -> Fut,
    Fut: Future<Output = Result<RefCache, PermissionError>>,
{
    let dynamic = checks
        .iter()
        .enumerate()
        .filter_map(|(index, check)| check.program.as_ref().map(|program| (index, program)))
        .collect::<Vec<_>>();
    let items = dynamic
        .iter()
        .map(|(index, program)| EvaluationItem {
            program,
            bindings: &checks[*index].bindings,
        })
        .collect::<Vec<_>>();
    let values = evaluate_batch_async(&items, refs, load_refs).await?;
    let mut dynamic_values = dynamic
        .into_iter()
        .map(|(index, _)| index)
        .zip(values)
        .collect::<std::collections::HashMap<_, _>>();
    checks
        .iter()
        .enumerate()
        .map(|(index, check)| {
            let allowed = check
                .constant
                .or_else(|| dynamic_values.remove(&index))
                .ok_or_else(|| {
                    PermissionError::Evaluation(
                        "planned permission check has no decision source".to_owned(),
                    )
                })?;
            Ok(PermissionCheckResult {
                phase: check.phase,
                scope: check.scope,
                action: check.action,
                etype: check.etype.clone(),
                entity_id: check.entity_id,
                program: check
                    .program
                    .as_ref()
                    .map(|program| crate::PermissionProgramInfo {
                        etype: check.etype.clone(),
                        action: program.action(),
                        code: program.source().to_owned(),
                        display_code: program.source().to_owned(),
                    }),
                allowed,
            })
        })
        .collect()
}

/// Enforces evaluated checks for normal (non-admin-audit) transactions.
///
/// # Errors
///
/// Returns the first denied check in deterministic plan order.
pub fn require_transaction_permissions(
    results: &[PermissionCheckResult],
) -> Result<(), PermissionError> {
    if let Some(denied) = results.iter().find(|result| !result.allowed) {
        return Err(PermissionError::PermissionDenied {
            etype: denied.etype.clone(),
            scope: match denied.scope {
                PermissionCheckScope::Object => "object",
                PermissionCheckScope::Attribute => "attribute",
            },
            action: denied.action.as_str(),
            entity_id: denied.entity_id,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use instant_instaql::{Cardinality, DeleteAction};
    use instant_transact::{
        AttributeDefinition, AttributeUpdate, EntityLookup, Identity, LookupRef, RuleParamsStep,
        TransactionStep, TripleOptions, TripleStep,
    };
    use serde_json::json;

    use super::*;
    use crate::project_transaction_entities;

    fn attr(id: u128, etype: &str, label: &str, reverse: Option<(&str, &str)>) -> Attribute {
        Attribute {
            id: Uuid::from_u128(id),
            wire: None,
            etype: etype.to_owned(),
            label: label.to_owned(),
            value_type: if reverse.is_some() {
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
            reverse_etype: reverse.map(|(etype, _)| etype.to_owned()),
            reverse_label: reverse.map(|(_, label)| label.to_owned()),
            on_delete: reverse.is_some().then_some(DeleteAction::Cascade),
            on_delete_reverse: None,
        }
    }

    fn triple(entity_id: Uuid, attribute: &Attribute, value: JsonValue) -> TripleStep {
        TripleStep {
            entity: EntityLookup::Id(entity_id),
            etype: Some(attribute.etype.clone()),
            attr_id: attribute.id,
            reverse_etype: attribute.reverse_etype.clone(),
            value,
            options: TripleOptions::default(),
        }
    }

    #[tokio::test]
    async fn scalar_checks_dedupe_and_carry_projected_new_data_and_modified_fields() {
        let id = attr(1, "posts", "id", None);
        let title = attr(2, "posts", "title", None);
        let catalog = AttributeCatalog {
            attributes: vec![id, title.clone()],
        };
        let entity_id = Uuid::new_v4();
        let key = entity_key("posts", entity_id);
        let steps = vec![
            TransactionStep::AddTriple(triple(entity_id, &title, json!("new"))),
            TransactionStep::AddTriple(triple(entity_id, &title, json!("new"))),
        ];
        let maps = project_transaction_entities(
            &catalog,
            &steps,
            [(key.clone(), json!({"id": entity_id, "title": "old"}))],
        )
        .unwrap();
        let invalid_rules = RuleSet::new(json!({
            "posts": {"allow": {"update":
                "[1, 2].map(value, value + 1)[0] + 'invalid' == 2"
            }}
        }));
        assert_eq!(
            plan_transaction_permission_checks(
                &invalid_rules,
                &catalog,
                &steps,
                &maps,
                &EvaluationBindings::default(),
                false,
            )
            .unwrap_err(),
            PermissionError::StaticType("found no matching overload for '_+_'".to_owned()),
        );
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"update":
                "request.modifiedFields.all(field, field == 'title') && [1, 2].map(value, value + 1)[0] == 2 && newData.title == 'new'"
            }}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();

        assert_eq!(plan.pre_write.len(), 1);
        assert!(plan.post_create.is_empty());
        assert_eq!(
            plan.pre_write[0].bindings.request.modified_fields,
            ["title"]
        );
        let results =
            evaluate_transaction_checks_async(&plan.pre_write, &mut RefCache::new(), |_| async {
                panic!("rule has no refs")
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].allowed);
        require_transaction_permissions(&results).unwrap();
    }

    #[test]
    fn relationship_checks_cover_both_directions_and_schema_denials() {
        let post_id = attr(10, "posts", "id", None);
        let user_id = attr(11, "users", "id", None);
        let author = attr(12, "posts", "author", Some(("users", "posts")));
        let catalog = AttributeCatalog {
            attributes: vec![post_id, user_id, author.clone()],
        };
        let post = Uuid::new_v4();
        let user = Uuid::new_v4();
        let add = TransactionStep::AddTriple(triple(post, &author, json!(user)));
        let unlink = TransactionStep::RetractTriple(triple(post, &author, json!(user)));
        let schema = TransactionStep::UpdateAttr(AttributeUpdate {
            id: author.id,
            ..AttributeUpdate::default()
        });
        let steps = vec![add, unlink, schema];
        let post_key = entity_key("posts", post);
        let user_key = entity_key("users", user);
        let maps = project_transaction_entities(
            &catalog,
            &steps,
            [
                (post_key, json!({"id": post, "author": user})),
                (user_key, json!({"id": user})),
            ],
        )
        .unwrap();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {
                "link": {"author": "actions.data == 'update'"},
                "unlink": {"author": true}
            }},
            "users": {"allow": {
                "link": {"posts": true},
                "unlink": {"posts": true}
            }}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();

        assert_eq!(
            plan.pre_write
                .iter()
                .filter(|check| check.action == PermissionAction::Link)
                .count(),
            2
        );
        assert_eq!(
            plan.pre_write
                .iter()
                .filter(|check| check.action == PermissionAction::Unlink)
                .count(),
            2
        );
        let schema_check = plan
            .pre_write
            .iter()
            .find(|check| check.scope == PermissionCheckScope::Attribute)
            .unwrap();
        assert_eq!(schema_check.constant, Some(false));
        assert!(plan.post_create.is_empty());
    }

    #[test]
    fn relationship_post_create_bindings_synthesize_resolved_ids() {
        let post_title = attr(20, "posts", "title", None);
        let user_email = attr(21, "users", "email", None);
        let author = attr(22, "posts", "author", Some(("users", "posts")));
        let catalog = AttributeCatalog {
            attributes: vec![post_title.clone(), user_email.clone(), author.clone()],
        };
        let post = Uuid::new_v4();
        let user = Uuid::new_v4();
        let steps = vec![
            TransactionStep::AddTriple(triple(user, &user_email, json!("user@example.com"))),
            TransactionStep::AddTriple(triple(post, &post_title, json!("created"))),
            TransactionStep::AddTriple(triple(post, &author, json!(user))),
        ];
        let maps = project_transaction_entities(&catalog, &steps, []).unwrap();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"create": true, "link": {"author": true}}},
            "users": {"allow": {"create": true, "link": {"posts": true}}}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();

        for check in plan
            .post_create
            .iter()
            .filter(|check| check.action == PermissionAction::Link)
        {
            let entity_id = check.entity_id.unwrap();
            assert_eq!(check.bindings.data["id"], json!(entity_id));
            assert_eq!(
                check.bindings.new_data.as_ref().unwrap()["id"],
                json!(entity_id)
            );
            if check.action == PermissionAction::Link {
                let linked_id = if entity_id == post { user } else { post };
                assert_eq!(
                    check.bindings.linked_data.as_ref().unwrap()["id"],
                    json!(linked_id)
                );
            }
        }

        let fallback_rules = RuleSet::new(json!({
            "posts": {"allow": {"create": true}},
            "users": {"allow": {"create": true, "view": true}}
        }));
        let fallback = plan_transaction_permission_checks(
            &fallback_rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();
        let reverse_view = fallback
            .post_create
            .iter()
            .find(|check| check.action == PermissionAction::View)
            .unwrap();
        assert_eq!(reverse_view.bindings.data["id"], json!(user));
        assert_eq!(
            reverse_view.bindings.new_data.as_ref().unwrap()["id"],
            json!(user)
        );
        assert!(reverse_view.bindings.request.modified_fields.is_empty());

        let mut false_id = json!({"id": false});
        synthesize_entity_id(&mut false_id, post);
        assert_eq!(false_id["id"], json!(post));
    }

    #[tokio::test]
    async fn distinct_relationship_labels_between_one_entity_pair_are_all_checked() {
        let author = attr(30, "posts", "author", Some(("users", "authoredPosts")));
        let reviewer = attr(31, "posts", "reviewer", Some(("users", "reviewedPosts")));
        let catalog = AttributeCatalog {
            attributes: vec![author.clone(), reviewer.clone()],
        };
        let post = Uuid::new_v4();
        let user = Uuid::new_v4();
        let steps = vec![
            TransactionStep::AddTriple(triple(post, &author, json!(user))),
            TransactionStep::AddTriple(triple(post, &reviewer, json!(user))),
        ];
        let post_key = entity_key("posts", post);
        let user_key = entity_key("users", user);
        let maps = project_transaction_entities(
            &catalog,
            &steps,
            [
                (post_key, json!({"id": post})),
                (user_key, json!({"id": user})),
            ],
        )
        .unwrap();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"link": {"author": true, "reviewer": false}}}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();
        assert_eq!(plan.pre_write.len(), 2);
        let results =
            evaluate_transaction_checks_async(&plan.pre_write, &mut RefCache::new(), |_| async {
                panic!("rules have no refs")
            })
            .await
            .unwrap();
        assert!(results.iter().any(|result| result.allowed));
        assert!(results.iter().any(|result| !result.allowed));
        assert!(require_transaction_permissions(&results).is_err());
    }

    #[test]
    fn equivalent_specialized_relationship_checks_dedupe_by_effective_check() {
        let author = attr(32, "posts", "author", Some(("users", "authoredPosts")));
        let reviewer = attr(33, "posts", "reviewer", Some(("users", "reviewedPosts")));
        let catalog = AttributeCatalog {
            attributes: vec![author.clone(), reviewer.clone()],
        };
        let post = Uuid::new_v4();
        let user = Uuid::new_v4();
        let steps = vec![
            TransactionStep::AddTriple(triple(post, &author, json!(user))),
            TransactionStep::AddTriple(triple(post, &reviewer, json!(user))),
        ];
        let maps = project_transaction_entities(
            &catalog,
            &steps,
            [
                (entity_key("posts", post), json!({"id": post})),
                (entity_key("users", user), json!({"id": user})),
            ],
        )
        .unwrap();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"link": {"author": true, "reviewer": true}}}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();
        assert_eq!(plan.pre_write.len(), 1);
    }

    #[tokio::test]
    async fn fallback_relationship_checks_keep_distinct_linked_rule_params() {
        let author = attr(40, "posts", "author", Some(("users", "authoredPosts")));
        let reviewer = attr(41, "posts", "reviewer", Some(("users", "reviewedPosts")));
        let catalog = AttributeCatalog {
            attributes: vec![author.clone(), reviewer.clone()],
        };
        let post = Uuid::new_v4();
        let author_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let steps = vec![
            TransactionStep::AddTriple(triple(post, &author, json!(author_id))),
            TransactionStep::AddTriple(triple(post, &reviewer, json!(reviewer_id))),
            TransactionStep::RuleParams(RuleParamsStep {
                entity: EntityLookup::Id(author_id),
                etype: Some("users".to_owned()),
                params: Map::from_iter([("allowed".to_owned(), json!(true))]),
            }),
            TransactionStep::RuleParams(RuleParamsStep {
                entity: EntityLookup::Id(reviewer_id),
                etype: Some("users".to_owned()),
                params: Map::from_iter([("allowed".to_owned(), json!(false))]),
            }),
        ];
        let maps = project_transaction_entities(
            &catalog,
            &steps,
            [
                (entity_key("posts", post), json!({"id": post})),
                (entity_key("users", author_id), json!({"id": author_id})),
                (entity_key("users", reviewer_id), json!({"id": reviewer_id})),
            ],
        )
        .unwrap();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"update": "ruleParams.allowed"}},
            "users": {"allow": {"view": true}}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();
        let results =
            evaluate_transaction_checks_async(&plan.pre_write, &mut RefCache::new(), |_| async {
                panic!("rules have no refs")
            })
            .await
            .unwrap();
        let post_updates = results
            .iter()
            .filter(|result| result.etype == "posts" && result.action == PermissionAction::Update)
            .collect::<Vec<_>>();
        assert_eq!(post_updates.len(), 2);
        assert!(post_updates.iter().any(|result| result.allowed));
        assert!(post_updates.iter().any(|result| !result.allowed));
        assert!(require_transaction_permissions(&results).is_err());
    }

    #[test]
    fn equivalent_fallback_relationship_checks_dedupe_for_one_linked_entity() {
        let author = attr(50, "posts", "author", Some(("users", "authoredPosts")));
        let reviewer = attr(51, "posts", "reviewer", Some(("users", "reviewedPosts")));
        let catalog = AttributeCatalog {
            attributes: vec![author.clone(), reviewer.clone()],
        };
        let post = Uuid::new_v4();
        let user = Uuid::new_v4();
        let steps = vec![
            TransactionStep::AddTriple(triple(post, &author, json!(user))),
            TransactionStep::AddTriple(triple(post, &reviewer, json!(user))),
        ];
        let maps = project_transaction_entities(
            &catalog,
            &steps,
            [
                (entity_key("posts", post), json!({"id": post})),
                (entity_key("users", user), json!({"id": user})),
            ],
        )
        .unwrap();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"update": true}},
            "users": {"allow": {"view": true}}
        }));
        let plan = plan_transaction_permission_checks(
            &rules,
            &catalog,
            &steps,
            &maps,
            &EvaluationBindings::default(),
            false,
        )
        .unwrap();
        assert_eq!(plan.pre_write.len(), 2);
        assert_eq!(
            plan.pre_write
                .iter()
                .filter(|check| check.etype == "posts")
                .count(),
            1
        );
        assert_eq!(
            plan.pre_write
                .iter()
                .filter(|check| check.etype == "users")
                .count(),
            1
        );
    }

    #[test]
    fn permissioned_retracts_retain_the_upstream_link_uuid_validation() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/permissioned_transaction.clj:112-122";
        assert!(ORACLE.contains("4e0a1bd8"));
        let author = attr(12, "posts", "author", Some(("users", "posts")));
        let catalog = AttributeCatalog {
            attributes: vec![author.clone()],
        };
        let invalid = json!("not-a-uuid");
        let steps = [TransactionStep::RetractTriple(triple(
            Uuid::new_v4(),
            &author,
            invalid.clone(),
        ))];

        assert_eq!(
            transaction_permission_entity_keys(&catalog, &steps).unwrap_err(),
            PermissionError::InvalidTransactionReference(invalid),
        );
    }

    #[test]
    fn permission_link_values_use_jvm_uuid_coercion() {
        let author = attr(60, "posts", "author", Some(("users", "posts")));
        let post = Uuid::new_v4();
        let user = Uuid::new_v4();
        let normalized = coerce_permission_link_values(&[TransactionStep::RetractTriple(triple(
            post,
            &author,
            json!(user.to_string().to_uppercase()),
        ))])
        .unwrap();
        let TransactionStep::RetractTriple(normalized) = &normalized[0] else {
            panic!("expected retract triple")
        };
        assert_eq!(normalized.value, json!(user));

        let noncanonical = "0000000-0000-0000-0000-0000000000000";
        assert_eq!(noncanonical.len(), 36);
        let normalized = coerce_permission_link_values(&[TransactionStep::RetractTriple(triple(
            post,
            &author,
            json!(noncanonical),
        ))])
        .unwrap();
        let TransactionStep::RetractTriple(normalized) = &normalized[0] else {
            panic!("expected retract triple")
        };
        assert_eq!(normalized.value, json!(Uuid::nil()));

        let nonzero_overflow = "0000000-10000-0000-0000-000000000000";
        assert_eq!(nonzero_overflow.len(), 36);
        let normalized = coerce_permission_link_values(&[TransactionStep::RetractTriple(triple(
            post,
            &author,
            json!(nonzero_overflow),
        ))])
        .unwrap();
        let TransactionStep::RetractTriple(normalized) = &normalized[0] else {
            panic!("expected retract triple")
        };
        assert_eq!(normalized.value, json!(Uuid::nil()));

        let nbsp_wrapped = json!(format!("\u{a0}{user}\u{a0}"));
        assert!(matches!(
            coerce_permission_link_values(&[TransactionStep::RetractTriple(triple(
                post,
                &author,
                nbsp_wrapped.clone(),
            ))]),
            Err(PermissionError::InvalidTransactionLinkValue { value, .. })
                if value == nbsp_wrapped
        ));

        let compact = json!(user.simple().to_string());
        let compact_error = coerce_permission_link_values(&[TransactionStep::RetractTriple(
            triple(post, &author, compact.clone()),
        )])
        .unwrap_err();
        assert!(matches!(
            compact_error,
            PermissionError::InvalidTransactionLinkValue { value, .. } if value == compact
        ));

        let invalid = json!({"not": "a uuid"});
        let add_error = coerce_permission_link_values(&[TransactionStep::AddTriple(triple(
            post,
            &author,
            invalid.clone(),
        ))])
        .unwrap_err();
        assert!(matches!(
            add_error,
            PermissionError::InvalidTransactionLinkValue { value, .. } if value == invalid
        ));
    }

    #[test]
    fn permission_lookup_validation_uses_the_optimistic_catalog() {
        let mut handle = attr(70, "posts", "handle", None);
        handle.unique = true;
        handle.indexed = true;
        let definition = AttributeDefinition {
            attribute: handle.clone(),
            forward_identity: Identity {
                id: Uuid::new_v4(),
                etype: "posts".to_owned(),
                label: "handle".to_owned(),
            },
            reverse_identity: None,
            on_delete: None,
            on_delete_reverse: None,
        };
        let steps = vec![
            TransactionStep::AddAttr(definition),
            TransactionStep::RuleParams(RuleParamsStep {
                entity: EntityLookup::Lookup(LookupRef {
                    attr_id: handle.id,
                    value: json!("new-post"),
                }),
                etype: Some("posts".to_owned()),
                params: Map::new(),
            }),
        ];
        let catalog = optimistic_catalog(&AttributeCatalog::default(), &steps);
        validate_permission_lookup_etypes(&catalog, &steps).unwrap();

        let TransactionStep::RuleParams(params) = &steps[1] else {
            panic!("expected rule params")
        };
        let mismatched = vec![
            steps[0].clone(),
            TransactionStep::RuleParams(RuleParamsStep {
                etype: Some("comments".to_owned()),
                ..params.clone()
            }),
        ];
        assert!(matches!(
            validate_permission_lookup_etypes(&catalog, &mismatched),
            Err(PermissionError::InvalidTransactionLookupType { data_type, .. })
                if data_type == "tx-step"
        ));
    }
}
