use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasher;
use std::time::Duration;

use instant_datalog::DataType;
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, DeleteAction,
    SYSTEM_CATALOG_APP_ID, SystemCatalogError, is_system_catalog_identity,
};
use instant_wal::{Isn, LsnError};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{Json, Type};
use tokio_postgres::{Client, Transaction};
use uuid::Uuid;

use crate::{
    AttributeDefinition, AttributeUpdate, EntityLookup, Identity, LookupRef, PreparedLookup,
    PreparedTransaction, TransactionMode, TransactionOperation, TransactionStep, TripleStep,
    optimistic_attribute_from_update,
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransactionOptions {
    pub allow_system_catalog_updates: bool,
    /// Allows admin writes to protected system fields other than `$files`/`$streams`.
    pub admin: bool,
    /// Internal coordinator-only capability for `$files` storage metadata.
    pub allow_file_metadata_updates: bool,
    /// Internal stream coordinator-only capability for `$streams` metadata.
    pub allow_stream_metadata_updates: bool,
    pub overwrite_created_at: bool,
    pub invalidator_slot_num: i32,
    pub wal_entity_log: WalEntityLog,
}

/// Controls the authoritative app-status check at the base write boundary.
///
/// Normal callers use [`Default::default`] and therefore enforce status. The
/// skip variant exists only for the few JVM operations that must keep working
/// while an app is disabled, such as refresh-token revocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppStatusWriteCheck {
    Enforce,
    Skip,
}

impl Default for AppStatusWriteCheck {
    fn default() -> Self {
        Self::Enforce
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WalEntityLog {
    #[default]
    Disabled,
    LogicalMessage,
    Table,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mutation {
    pub operation: TransactionOperation,
    pub entity_id: Uuid,
    pub attr_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionReceipt {
    pub id: i64,
    pub created_at_ms: i64,
    pub fallback_isn: Isn,
    pub mutations: Vec<Mutation>,
    pub schema_mutations: Vec<SchemaMutation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaMutation {
    pub operation: TransactionOperation,
    pub attr_id: Uuid,
    pub identity_ids: Vec<Uuid>,
    pub affected_entity_ids: Vec<Uuid>,
    pub changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingRequiredAttribute {
    pub entity_id: Uuid,
    pub etype: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
struct NewEntityType {
    entity: EntityLookup,
    etype: String,
}

struct AttributeStorageState {
    value_type: String,
    cardinality: String,
    unique: bool,
    indexed: bool,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("you can't make updates to the system catalog app")]
    SystemCatalogProtected,
    #[error("transaction references unknown attribute {0}")]
    UnknownAttribute(Uuid),
    #[error("lookup attribute {0} is not unique")]
    LookupAttributeNotUnique(Uuid),
    #[error("the entity for lookup {0:?} does not exist")]
    MissingLookup(LookupRef),
    #[error(
        "Updates with lookups can only update the lookup attribute if an entity with the unique attribute value already exists."
    )]
    LookupUpdateCollision(Vec<LookupRef>),
    #[error("reference attribute {attr_id} requires a UUID value, got {value}")]
    InvalidReferenceValue {
        attr_id: Uuid,
        value: Value,
        entity_id: Option<Uuid>,
    },
    #[error("deep merge is not supported for link attribute {0}")]
    DeepMergeReference(Uuid),
    #[error("deep merge requires cardinality-one attribute {0}")]
    DeepMergeMany(Uuid),
    #[error("creating entities that exist: {0:?}")]
    CreateExisting(Vec<TransactionStep>),
    #[error("updating entities that don't exist: {0:?}")]
    UpdateMissing(Vec<TransactionStep>),
    #[error(
        "lookup entity namespace {lookup_etype} does not match target namespace {target_etype}"
    )]
    LookupEntityTypeMismatch {
        lookup_etype: String,
        target_etype: String,
    },
    #[error("missing required attributes: {0:?}")]
    MissingRequired(Vec<MissingRequiredAttribute>),
    #[error("attribute {0} conflicts with its persisted definition")]
    AttributeConflict(Uuid),
    #[error("identity {0} conflicts with its persisted definition")]
    IdentityConflict(Uuid),
    #[error("{etype}.{label} is reserved by the system catalog")]
    ReservedIdentity { etype: String, label: String },
    #[error("$ is reserved for system namespaces; cannot create {0}")]
    ReservedSystemNamespace(String),
    #[error("required system attributes cannot be created for {0}")]
    RequiredSystemAttribute(String),
    #[error("{etype}.{label} is a protected system field")]
    ProtectedSystemField { etype: String, label: String },
    #[error("system entity {0} cannot be deleted directly")]
    ProtectedSystemEntity(String),
    #[error("stream file paths cannot be edited")]
    ProtectedStreamFilePath,
    #[error("required attribute {etype}.{label} cannot be added after entities exist")]
    RequiredAttributeOnExistingEntities { etype: String, label: String },
    #[error("required attribute {etype}.{label} cannot be enabled while entities are missing it")]
    RequiredAttributeUpdateOnExistingEntities {
        attr_id: Uuid,
        etype: String,
        label: String,
    },
    #[error(transparent)]
    SystemCatalog(#[from] SystemCatalogError),
    #[error(transparent)]
    InvalidWalLsn(#[from] LsnError),
    #[error("PostgreSQL transaction statement exceeded its execution timeout")]
    QueryTimeout,
    #[error("This app is in read-only mode.")]
    AppReadOnly,
    #[error("This app is currently disabled.")]
    AppDisabled,
    #[error("there are too many parameters in the transaction")]
    ParameterLimitExceeded,
    #[error("app status `{0}` is not active, read-only, or disabled")]
    InvalidAppStatus(String),
    #[error(transparent)]
    Postgres(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for ExecutionError {
    fn from(error: tokio_postgres::Error) -> Self {
        if error.code() == Some(&SqlState::QUERY_CANCELED)
            && error
                .as_db_error()
                .is_some_and(|error| error.message().contains("statement timeout"))
        {
            Self::QueryTimeout
        } else {
            Self::Postgres(error)
        }
    }
}

pub const DEFAULT_TRANSACTION_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const SOURCE_PROVEN_ADD_TRIPLE_LIMIT: usize = 100_000;

/// Stable config-app identity used by the JVM deployment and native schema.
/// Applies a transaction-local PostgreSQL statement timeout.
///
/// The setting automatically disappears on commit or rollback and therefore
/// cannot leak into the next user of a pooled connection.
///
/// # Errors
///
/// Returns a PostgreSQL error if the local setting cannot be applied.
pub async fn configure_transaction_statement_timeout(
    transaction: &Transaction<'_>,
    timeout: Duration,
) -> Result<(), ExecutionError> {
    let milliseconds = timeout.as_millis().max(1);
    transaction
        .query_one(
            "SELECT set_config('statement_timeout', $1, true)",
            &[&format!("{milliseconds}ms")],
        )
        .await?;
    Ok(())
}

/// Executes compiled transaction steps atomically against the existing schema.
///
/// The transaction row is the first write, preserving WAL invalidator ordering.
/// Reads used for create/update mode validation occur before that write in the
/// same database transaction.
///
/// # Errors
///
/// Returns [`ExecutionError`] for validation, unsupported schema mutations,
/// lookup resolution, PostgreSQL constraint, or commit failures. Any error rolls
/// back the transaction and its transaction record.
pub async fn execute_transaction(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    options: TransactionOptions,
) -> Result<TransactionReceipt, ExecutionError> {
    execute_transaction_with_app_status_write_check(
        client,
        app_id,
        catalog,
        steps,
        options,
        AppStatusWriteCheck::default(),
    )
    .await
}

/// Executes compiled transaction steps with an explicit app-status policy.
///
/// This is intentionally verbose at call sites: using [`AppStatusWriteCheck::Skip`]
/// is an auditable capability, not a general-purpose admin bypass.
///
/// # Errors
///
/// Returns [`ExecutionError`] under the same conditions as
/// [`execute_transaction`], including an app-status rejection before the
/// transaction record or triples can be written.
pub async fn execute_transaction_with_app_status_write_check(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    options: TransactionOptions,
    app_status_write_check: AppStatusWriteCheck,
) -> Result<TransactionReceipt, ExecutionError> {
    if app_id == SYSTEM_CATALOG_APP_ID && !options.allow_system_catalog_updates {
        return Err(ExecutionError::SystemCatalogProtected);
    }
    let transaction = client.transaction().await?;
    let result = execute_transaction_in_with_app_status_write_check(
        &transaction,
        app_id,
        catalog,
        steps,
        options,
        app_status_write_check,
    )
    .await;
    match result {
        Ok(receipt) => {
            transaction.commit().await?;
            Ok(receipt)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

#[must_use]
pub fn optimistic_catalog(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> AttributeCatalog {
    let mut catalog = catalog.clone();
    for step in steps {
        match step {
            TransactionStep::AddAttr(definition) => {
                catalog
                    .attributes
                    .retain(|attribute| attribute.id != definition.attribute.id);
                catalog.attributes.push(definition.attribute.clone());
            }
            TransactionStep::UpdateAttr(update) => {
                catalog
                    .attributes
                    .retain(|attribute| attribute.id != update.id);
                catalog
                    .attributes
                    .push(optimistic_attribute_from_update(update));
            }
            TransactionStep::DeleteAttr(attr_id) => {
                catalog
                    .attributes
                    .retain(|attribute| attribute.id != *attr_id);
            }
            _ => {}
        }
    }
    catalog
}

fn apply_persisted_schema_step(catalog: &mut AttributeCatalog, step: &TransactionStep) {
    match step {
        TransactionStep::AddAttr(definition) => {
            replace_catalog_attribute(catalog, definition.attribute.clone());
        }
        TransactionStep::UpdateAttr(update) => {
            if let Some(attribute) = catalog
                .attributes
                .iter_mut()
                .find(|attribute| attribute.id == update.id)
            {
                if let Some(identity) = &update.forward_identity {
                    attribute.etype.clone_from(&identity.etype);
                    attribute.label.clone_from(&identity.label);
                }
                if let Some(Some(identity)) = &update.reverse_identity {
                    attribute.reverse_etype = Some(identity.etype.clone());
                    attribute.reverse_label = Some(identity.label.clone());
                }
                if let Some(unique) = update.unique {
                    attribute.unique = unique;
                }
                if let Some(indexed) = update.indexed {
                    attribute.indexed = indexed;
                }
                if let Some(cardinality) = update.cardinality {
                    attribute.cardinality = cardinality;
                }
                if let Some(required) = update.required {
                    attribute.required = required;
                }
                if let Some(on_delete) = update.on_delete {
                    attribute.on_delete = on_delete;
                }
                if let Some(on_delete_reverse) = update.on_delete_reverse {
                    attribute.on_delete_reverse = on_delete_reverse;
                }
            }
        }
        TransactionStep::DeleteAttr(attr_id) => {
            catalog
                .attributes
                .retain(|attribute| attribute.id != *attr_id);
        }
        _ => {}
    }
}

fn replace_catalog_attribute(catalog: &mut AttributeCatalog, attribute: Attribute) {
    catalog
        .attributes
        .retain(|candidate| candidate.id != attribute.id);
    catalog.attributes.push(attribute);
}

fn restored_catalog_attribute(mut attribute: Attribute) -> Attribute {
    attribute.indexed = false;
    attribute.required = false;
    attribute
}

pub(crate) fn catalog_with_restored_attributes(
    catalog: &AttributeCatalog,
    restored: impl IntoIterator<Item = Attribute>,
) -> AttributeCatalog {
    let mut catalog = catalog.clone();
    for attribute in restored {
        replace_catalog_attribute(&mut catalog, attribute);
    }
    catalog
}

fn restored_attribute_from_row(row: &tokio_postgres::Row) -> Result<Attribute, ExecutionError> {
    let id = row.get::<_, Uuid>("id");
    let value_type = match row.get::<_, String>("value_type").as_str() {
        "blob" => AttributeValueType::Blob,
        "ref" => AttributeValueType::Ref,
        _ => return Err(ExecutionError::AttributeConflict(id)),
    };
    let cardinality = match row.get::<_, String>("cardinality").as_str() {
        "one" => Cardinality::One,
        "many" => Cardinality::Many,
        _ => return Err(ExecutionError::AttributeConflict(id)),
    };
    let checked_data_type = match row.get::<_, Option<String>>("checked_data_type").as_deref() {
        None => None,
        Some("string") => Some(DataType::String),
        Some("number") => Some(DataType::Number),
        Some("boolean") => Some(DataType::Boolean),
        Some("date") => Some(DataType::Date),
        Some(_) => return Err(ExecutionError::AttributeConflict(id)),
    };
    let decode_delete_action = |field| match row.get::<_, Option<String>>(field).as_deref() {
        None => Ok(None),
        Some("cascade") => Ok(Some(DeleteAction::Cascade)),
        Some(_) => Err(ExecutionError::AttributeConflict(id)),
    };
    Ok(Attribute {
        id,
        wire: None,
        etype: row.get("etype"),
        label: row.get("label"),
        value_type,
        cardinality,
        unique: row.get("is_unique"),
        setting_unique: row.get("setting_unique"),
        indexed: false,
        required: false,
        checked_data_type,
        indexing: row.get("indexing"),
        checking_data_type: row.get("checking_data_type"),
        reverse_etype: row.get("reverse_etype"),
        reverse_label: row.get("reverse_label"),
        on_delete: decode_delete_action("on_delete")?,
        on_delete_reverse: decode_delete_action("on_delete_reverse")?,
    })
}

pub(crate) async fn load_restored_attributes(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    steps: &[TransactionStep],
) -> Result<HashMap<Uuid, Attribute>, ExecutionError> {
    let attr_ids = steps
        .iter()
        .filter_map(|step| match step {
            TransactionStep::RestoreAttr(attr_id) => Some(*attr_id),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if attr_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = transaction
        .query(
            "SELECT id, value_type::text AS value_type,
                    cardinality::text AS cardinality, is_unique,
                    coalesce(setting_unique, false) AS setting_unique,
                    coalesce(indexing, false) AS indexing,
                    coalesce(checking_data_type, false) AS checking_data_type,
                    checked_data_type::text AS checked_data_type,
                    on_delete::text AS on_delete,
                    on_delete_reverse::text AS on_delete_reverse,
                    substring(etype FROM position('$' IN etype) + 1) AS etype,
                    substring(label FROM position('$' IN etype) + 1) AS label,
                    CASE WHEN reverse_etype IS NULL THEN NULL
                      ELSE substring(reverse_etype FROM position('$' IN etype) + 1) END
                      AS reverse_etype,
                    CASE WHEN reverse_label IS NULL THEN NULL
                      ELSE substring(reverse_label FROM position('$' IN etype) + 1) END
                      AS reverse_label
             FROM attrs
             WHERE app_id = $1 AND id = ANY($2) AND deletion_marked_at IS NOT NULL",
            &[&app_id, &attr_ids],
        )
        .await?;
    rows.iter()
        .map(restored_attribute_from_row)
        .map(|result| result.map(|attribute| (attribute.id, attribute)))
        .collect()
}

/// Enforces the app's write status inside the caller's transaction.
///
/// This read is deliberately not served from the WAL-invalidated control-plane
/// snapshot. A status flip must reject the very next transaction, including one
/// that began before the invalidation was delivered, so the check has to use the
/// transaction's own statement snapshot: WAL delivery is asynchronous and cannot
/// linearize a read-only/disabled flip ahead of a write that has already started.
/// The statement is a one-row primary-key lookup; its cost is planning, not I/O.
async fn assert_app_status_write_allowed(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    check: AppStatusWriteCheck,
) -> Result<(), ExecutionError> {
    let AppStatusWriteCheck::Enforce = check else {
        return Ok(());
    };
    let row = transaction
        .query_one(
            "SELECT COALESCE(
                       (SELECT app.status
                          FROM apps app
                         WHERE app.id = $1
                           AND app.deletion_marked_at IS NULL),
                       'active'
                     )",
            &[&app_id],
        )
        .await?;
    let own = row.get::<_, String>(0);
    match own.as_str() {
        "active" => Ok(()),
        "read-only" => Err(ExecutionError::AppReadOnly),
        "disabled" => Err(ExecutionError::AppDisabled),
        invalid => Err(ExecutionError::InvalidAppStatus(invalid.to_owned())),
    }
}

/// Executes compiled steps inside a caller-owned PostgreSQL transaction.
///
/// This entry point deliberately does not commit or roll back. Permissioned
/// transaction orchestration uses it after pre-write checks, then reads its own
/// writes for create checks before choosing whether to commit.
///
/// # Errors
///
/// Returns [`ExecutionError`] under the same conditions as
/// [`execute_transaction`]. The caller remains responsible for ending the
/// PostgreSQL transaction after an error.
pub async fn execute_transaction_in(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    options: TransactionOptions,
) -> Result<TransactionReceipt, ExecutionError> {
    execute_transaction_in_with_app_status_write_check(
        transaction,
        app_id,
        catalog,
        steps,
        options,
        AppStatusWriteCheck::default(),
    )
    .await
}

/// Executes compiled steps in a caller-owned transaction with an explicit
/// app-status policy.
///
/// # Errors
///
/// Returns [`ExecutionError`] under the same conditions as
/// [`execute_transaction_in`].
pub async fn execute_transaction_in_with_app_status_write_check(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    options: TransactionOptions,
    app_status_write_check: AppStatusWriteCheck,
) -> Result<TransactionReceipt, ExecutionError> {
    if app_id == SYSTEM_CATALOG_APP_ID && !options.allow_system_catalog_updates {
        return Err(ExecutionError::SystemCatalogProtected);
    }
    assert_app_status_write_allowed(transaction, app_id, app_status_write_check).await?;
    configure_transaction_statement_timeout(transaction, DEFAULT_TRANSACTION_STATEMENT_TIMEOUT)
        .await?;
    validate_transaction_modes(transaction, app_id, catalog, steps).await?;
    let prepared = crate::prepare_transaction_lookups(transaction, app_id, catalog, steps).await?;
    validate_reference_parameter_limit(steps)?;
    execute_transaction_steps_in(
        transaction,
        app_id,
        catalog,
        &prepared.steps,
        &prepared.implicit_lookups,
        &prepared.add_group_lookup_refs,
        options,
    )
    .await
}

/// Executes a lookup-prepared transaction inside a caller-owned PostgreSQL
/// transaction.
///
/// Implicit unique lookup triples are inserted after the transaction record,
/// immediately before their first consuming operation group, and are
/// intentionally omitted from the returned mutation receipt.
///
/// # Errors
///
/// Returns [`ExecutionError`] under the same conditions as
/// [`execute_transaction_in`].
pub async fn execute_prepared_transaction_in(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    prepared: &PreparedTransaction,
    options: TransactionOptions,
) -> Result<TransactionReceipt, ExecutionError> {
    execute_prepared_transaction_in_with_app_status_write_check(
        transaction,
        app_id,
        catalog,
        prepared,
        options,
        AppStatusWriteCheck::default(),
    )
    .await
}

/// Executes lookup-prepared steps with an explicit app-status policy.
///
/// # Errors
///
/// Returns [`ExecutionError`] under the same conditions as
/// [`execute_prepared_transaction_in`].
pub async fn execute_prepared_transaction_in_with_app_status_write_check(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    prepared: &PreparedTransaction,
    options: TransactionOptions,
    app_status_write_check: AppStatusWriteCheck,
) -> Result<TransactionReceipt, ExecutionError> {
    if app_id == SYSTEM_CATALOG_APP_ID && !options.allow_system_catalog_updates {
        return Err(ExecutionError::SystemCatalogProtected);
    }
    assert_app_status_write_allowed(transaction, app_id, app_status_write_check).await?;
    configure_transaction_statement_timeout(transaction, DEFAULT_TRANSACTION_STATEMENT_TIMEOUT)
        .await?;
    validate_transaction_modes(transaction, app_id, catalog, &prepared.original_steps).await?;
    validate_reference_parameter_limit(&prepared.original_steps)?;
    execute_transaction_steps_in(
        transaction,
        app_id,
        catalog,
        &prepared.steps,
        &prepared.implicit_lookups,
        &prepared.add_group_lookup_refs,
        options,
    )
    .await
}

fn validate_reference_parameter_limit(steps: &[TransactionStep]) -> Result<(), ExecutionError> {
    // The pinned JVM corpus proves that 100,000 grouped add triples exceed the
    // JDBC parameter limit. Rust sends one JSON payload instead, so preserve
    // that demonstrated boundary without guessing at lower untested counts.
    let add_triples = steps
        .iter()
        .filter(|step| matches!(step, TransactionStep::AddTriple(_)))
        .count();
    if add_triples >= SOURCE_PROVEN_ADD_TRIPLE_LIMIT {
        Err(ExecutionError::ParameterLimitExceeded)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_lines)] // The ordered write dispatch mirrors the public transaction protocol.
async fn execute_transaction_steps_in(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    implicit_lookups: &[PreparedLookup],
    add_group_lookup_refs: &[Vec<LookupRef>],
    options: TransactionOptions,
) -> Result<TransactionReceipt, ExecutionError> {
    configure_transaction_statement_timeout(transaction, DEFAULT_TRANSACTION_STATEMENT_TIMEOUT)
        .await?;
    if app_id == SYSTEM_CATALOG_APP_ID && !options.allow_system_catalog_updates {
        return Err(ExecutionError::SystemCatalogProtected);
    }
    configure_system_catalog_writes(transaction, app_id, options).await?;
    let mut restored_attributes = load_restored_attributes(transaction, app_id, steps).await?;
    for attr_id in steps.iter().filter_map(|step| match step {
        TransactionStep::RestoreAttr(attr_id) => Some(attr_id),
        _ => None,
    }) {
        if let Some(attribute) = catalog.by_id(*attr_id) {
            restored_attributes
                .entry(*attr_id)
                .or_insert_with(|| restored_catalog_attribute(attribute.clone()));
        }
    }
    let catalog_with_restores =
        catalog_with_restored_attributes(catalog, restored_attributes.values().cloned());
    let validation_catalog = optimistic_catalog(&catalog_with_restores, steps);
    validate_system_entity_writes(&validation_catalog, steps, options)?;
    validate_modes(transaction, app_id, &validation_catalog, steps).await?;
    let mut write_catalog = catalog.clone();
    let mut deleted_attributes = HashMap::new();
    let inserted = insert_transaction_record(transaction, app_id, options).await?;
    let mut receipt = TransactionReceipt {
        id: inserted.id,
        created_at_ms: inserted.created_at_ms,
        fallback_isn: inserted.fallback_isn,
        mutations: Vec::new(),
        schema_mutations: Vec::new(),
    };
    let mut inserted_lookups = HashSet::new();
    let mut add_group_index = 0;
    let mut index = 0;
    while index < steps.len() {
        let operation = steps[index].operation();
        let mut add_group_lookups = Vec::new();
        for (lookup_index, prepared) in implicit_lookups.iter().enumerate() {
            if prepared.operation != operation || !inserted_lookups.insert(lookup_index) {
                continue;
            }
            if operation == TransactionOperation::AddTriple {
                add_group_lookups.push(prepared.clone());
                continue;
            }
            let attribute = lookup_attribute(&write_catalog, &prepared.lookup)?;
            insert_lookup_triple(
                transaction,
                app_id,
                prepared.entity_id,
                attribute,
                &prepared.lookup.value,
            )
            .await?;
        }
        match &steps[index] {
            TransactionStep::AddTriple(_) => {
                let group_start = index;
                while index < steps.len() && matches!(steps[index], TransactionStep::AddTriple(_)) {
                    index += 1;
                }
                let group = &steps[group_start..index];
                let collision_lookups = add_group_lookup_refs
                    .get(add_group_index)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                add_group_index += 1;
                let lookups = AddGroupLookups {
                    materialized: &add_group_lookups,
                    collision: collision_lookups,
                };
                execute_add_group(
                    transaction,
                    app_id,
                    &write_catalog,
                    group,
                    lookups,
                    options,
                    &mut receipt.mutations,
                )
                .await?;
            }
            TransactionStep::DeepMergeTriple(_) => {
                let group_start = index;
                while index < steps.len()
                    && matches!(steps[index], TransactionStep::DeepMergeTriple(_))
                {
                    index += 1;
                }
                execute_deep_merge_group(
                    transaction,
                    app_id,
                    &write_catalog,
                    &steps[group_start..index],
                    options.overwrite_created_at,
                    &mut receipt.mutations,
                )
                .await?;
            }
            TransactionStep::RetractTriple(step) => {
                execute_retract(
                    transaction,
                    app_id,
                    &write_catalog,
                    step,
                    &mut receipt.mutations,
                )
                .await?;
                index += 1;
            }
            TransactionStep::DeleteEntity(step) => {
                execute_delete_entity(
                    transaction,
                    app_id,
                    &write_catalog,
                    step,
                    &mut receipt.mutations,
                )
                .await?;
                index += 1;
            }
            TransactionStep::RuleParams(_) => index += 1,
            TransactionStep::AddAttr(definition) => {
                let schema_mutation = execute_add_attribute(
                    transaction,
                    app_id,
                    definition,
                    app_id == SYSTEM_CATALOG_APP_ID && options.allow_system_catalog_updates,
                )
                .await?;
                receipt.schema_mutations.push(schema_mutation);
                apply_persisted_schema_step(&mut write_catalog, &steps[index]);
                index += 1;
            }
            TransactionStep::UpdateAttr(update) => {
                let schema_mutation = execute_update_attribute(transaction, app_id, update).await?;
                receipt.schema_mutations.push(schema_mutation);
                apply_persisted_schema_step(&mut write_catalog, &steps[index]);
                index += 1;
            }
            TransactionStep::DeleteAttr(attr_id) => {
                let deleted_attribute = write_catalog
                    .by_id(*attr_id)
                    .cloned()
                    .map(restored_catalog_attribute);
                let schema_mutation =
                    execute_delete_attribute(transaction, app_id, *attr_id).await?;
                if schema_mutation.changed
                    && let Some(attribute) = deleted_attribute
                {
                    deleted_attributes.insert(*attr_id, attribute);
                }
                receipt.schema_mutations.push(schema_mutation);
                apply_persisted_schema_step(&mut write_catalog, &steps[index]);
                index += 1;
            }
            TransactionStep::RestoreAttr(attr_id) => {
                let schema_mutation =
                    execute_restore_attribute(transaction, app_id, *attr_id).await?;
                if schema_mutation.changed {
                    let attribute = deleted_attributes
                        .remove(attr_id)
                        .or_else(|| restored_attributes.get(attr_id).cloned());
                    if let Some(attribute) = attribute {
                        replace_catalog_attribute(&mut write_catalog, attribute);
                    }
                }
                receipt.schema_mutations.push(schema_mutation);
                index += 1;
            }
        }
    }
    validate_required_attributes(transaction, app_id, &write_catalog, &receipt.mutations).await?;
    validate_required_attribute_updates(transaction, app_id, &write_catalog, steps).await?;
    Ok(receipt)
}

fn validate_system_entity_writes(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    options: TransactionOptions,
) -> Result<(), ExecutionError> {
    for step in steps {
        match step {
            TransactionStep::AddTriple(triple)
            | TransactionStep::DeepMergeTriple(triple)
            | TransactionStep::RetractTriple(triple) => {
                let attribute = catalog
                    .by_id(triple.attr_id)
                    .ok_or(ExecutionError::UnknownAttribute(triple.attr_id))?;
                let etype = attribute.etype.as_str();
                let label = attribute.label.as_str();
                if !etype.starts_with('$') || !is_system_catalog_identity(etype, label)? {
                    continue;
                }
                let editable = matches!(
                    (etype, label),
                    ("$users" | "$streams", "id") | ("$files", "id" | "path")
                );
                let internal_file = etype == "$files" && options.allow_file_metadata_updates;
                let internal_stream = etype == "$streams" && options.allow_stream_metadata_updates;
                let admin = options.admin && !matches!(etype, "$files" | "$streams");
                if !(editable || internal_file || internal_stream || admin) {
                    return Err(ExecutionError::ProtectedSystemField {
                        etype: etype.to_owned(),
                        label: label.to_owned(),
                    });
                }
                if etype == "$files"
                    && label == "path"
                    && !options.allow_file_metadata_updates
                    && triple
                        .value
                        .as_str()
                        .is_some_and(|path| path.starts_with("$stream/"))
                {
                    return Err(ExecutionError::ProtectedStreamFilePath);
                }
            }
            TransactionStep::DeleteEntity(delete) => {
                let Some(etype) = delete.etype.as_deref() else {
                    continue;
                };
                if !etype.starts_with('$') {
                    continue;
                }
                let allowed = etype == "$files"
                    || (etype == "$streams" && options.allow_stream_metadata_updates)
                    || (options.admin && !matches!(etype, "$files" | "$streams"));
                if !allowed {
                    return Err(ExecutionError::ProtectedSystemEntity(etype.to_owned()));
                }
            }
            TransactionStep::AddAttr(_)
            | TransactionStep::UpdateAttr(_)
            | TransactionStep::DeleteAttr(_)
            | TransactionStep::RestoreAttr(_)
            | TransactionStep::RuleParams(_) => {}
        }
    }
    Ok(())
}

struct InsertedTransaction {
    id: i64,
    created_at_ms: i64,
    fallback_isn: Isn,
}

async fn insert_transaction_record(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    options: TransactionOptions,
) -> Result<InsertedTransaction, ExecutionError> {
    let wal_message_app_id = app_id.to_string();
    let log_to_wal_table = matches!(options.wal_entity_log, WalEntityLog::Table).to_string();
    let row = if options.wal_entity_log == WalEntityLog::Disabled {
        transaction
            .query_typed_one(
                "WITH tx AS (
                   INSERT INTO transactions (app_id) VALUES ($1) RETURNING *
                 )
                 SELECT tx.id,
                        (extract(epoch from tx.created_at) * 1000)::bigint AS created_at_ms,
                        pg_current_wal_insert_lsn()::text AS wal_insert_lsn
                   FROM tx",
                &[(&app_id, Type::UUID)],
            )
            .await?
    } else {
        transaction
            .query_typed_one(
                "WITH setting AS MATERIALIZED (
                   SELECT set_config('instant.wal_msg_app_id', $1::text, true) AS app_id_config,
                          set_config('instant.wal_msg_log_to_table', $2::text, true) AS log_to_table_config
                 ), tx AS (
                   INSERT INTO transactions (app_id) VALUES ($3) RETURNING *
                 )
                 SELECT tx.id,
                        (extract(epoch from tx.created_at) * 1000)::bigint AS created_at_ms,
                        pg_current_wal_insert_lsn()::text AS wal_insert_lsn,
                        setting.app_id_config AS _,
                        setting.log_to_table_config AS __
                   FROM tx CROSS JOIN setting",
                &[
                    (&wal_message_app_id, Type::TEXT),
                    (&log_to_wal_table, Type::TEXT),
                    (&app_id, Type::UUID),
                ],
            )
            .await?
    };
    let wal_insert_lsn: String = row.get("wal_insert_lsn");
    Ok(InsertedTransaction {
        id: row.get("id"),
        created_at_ms: row.get("created_at_ms"),
        fallback_isn: Isn::new(options.invalidator_slot_num, wal_insert_lsn.parse()?),
    })
}

async fn configure_system_catalog_writes(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    options: TransactionOptions,
) -> Result<(), ExecutionError> {
    if app_id == SYSTEM_CATALOG_APP_ID && options.allow_system_catalog_updates {
        transaction
            .batch_execute(
                "SET LOCAL instant.allow_system_catalog_app_attr_update = 'true';
                 SET LOCAL instant.allow_system_catalog_app_ident_update = 'true';",
            )
            .await?;
    }
    Ok(())
}

async fn execute_add_attribute(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    definition: &AttributeDefinition,
    allow_reserved_names: bool,
) -> Result<SchemaMutation, ExecutionError> {
    validate_attribute_definition(definition, allow_reserved_names)?;
    let attribute = &definition.attribute;
    if attribute.required
        && transaction
            .query_one(
                "SELECT EXISTS(
                   SELECT 1 FROM triples
                   JOIN attrs ON attrs.id = triples.attr_id
                   WHERE attrs.app_id = $1 AND attrs.etype = $2 AND triples.app_id = $1
                 )",
                &[&app_id, &attribute.etype],
            )
            .await?
            .get::<_, bool>(0)
    {
        return Err(ExecutionError::RequiredAttributeOnExistingEntities {
            etype: attribute.etype.clone(),
            label: attribute.label.clone(),
        });
    }

    let value_type = attribute_value_type_name(attribute.value_type);
    let cardinality = cardinality_name(attribute.cardinality);
    let reverse_identity_id = definition
        .reverse_identity
        .as_ref()
        .map(|identity| identity.id);
    let on_delete = delete_action_name(definition.on_delete);
    let on_delete_reverse = delete_action_name(definition.on_delete_reverse);
    let checked_data_type = attribute
        .checked_data_type
        .map(|data_type| format!("{data_type:?}").to_lowercase());
    let inserted = !transaction
        .query(
            "INSERT INTO attrs
             (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
              forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
              on_delete, on_delete_reverse, checked_data_type, setting_unique, indexing,
              checking_data_type)
             VALUES
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
              $14::text::attr_on_delete, $15::text::attr_on_delete,
              $16::text::checked_data_type, $17, $18, $19)
             ON CONFLICT (id) DO NOTHING
             RETURNING id",
            &[
                &attribute.id,
                &app_id,
                &value_type,
                &cardinality,
                &attribute.unique,
                &attribute.indexed,
                &attribute.required,
                &definition.forward_identity.id,
                &attribute.etype,
                &attribute.label,
                &reverse_identity_id,
                &attribute.reverse_etype,
                &attribute.reverse_label,
                &on_delete,
                &on_delete_reverse,
                &checked_data_type,
                &attribute.setting_unique,
                &attribute.indexing,
                &attribute.checking_data_type,
            ],
        )
        .await?
        .is_empty();
    if !inserted {
        ensure_attribute_matches(transaction, app_id, definition).await?;
    }

    let mut identity_ids = Vec::new();
    for identity in
        std::iter::once(&definition.forward_identity).chain(definition.reverse_identity.iter())
    {
        if insert_identity(transaction, app_id, attribute.id, identity).await? {
            identity_ids.push(identity.id);
        }
    }
    let affected_entity_ids = if inserted {
        backfill_indexed_nulls_for_attribute(transaction, app_id, attribute).await?
    } else {
        Vec::new()
    };
    let changed = inserted || !identity_ids.is_empty() || !affected_entity_ids.is_empty();
    Ok(SchemaMutation {
        operation: TransactionOperation::AddAttr,
        attr_id: attribute.id,
        identity_ids,
        affected_entity_ids,
        changed,
    })
}

fn validate_attribute_definition(
    definition: &AttributeDefinition,
    allow_reserved_names: bool,
) -> Result<(), ExecutionError> {
    if allow_reserved_names {
        return Ok(());
    }
    validate_identity_name(&definition.forward_identity)?;
    if let Some(identity) = &definition.reverse_identity {
        validate_identity_name(identity)?;
    }
    if definition.attribute.required && definition.attribute.etype.starts_with('$') {
        return Err(ExecutionError::RequiredSystemAttribute(
            definition.attribute.etype.clone(),
        ));
    }
    Ok(())
}

/// Rejects attribute identities pre-reserved by the live config-app flag.
///
/// Static system identities are still checked during execution; this additional
/// set protects names staged for a rolling system-catalog deployment.
///
/// # Errors
///
/// Returns [`ExecutionError::ReservedIdentity`] for the first reserved forward
/// or reverse identity in transaction order.
pub fn validate_reserved_identity_names<S: BuildHasher>(
    steps: &[TransactionStep],
    reserved: &HashSet<(String, String), S>,
) -> Result<(), ExecutionError> {
    for step in steps {
        let TransactionStep::AddAttr(definition) = step else {
            continue;
        };
        for identity in
            std::iter::once(&definition.forward_identity).chain(definition.reverse_identity.iter())
        {
            if reserved.contains(&(identity.etype.clone(), identity.label.clone())) {
                return Err(ExecutionError::ReservedIdentity {
                    etype: identity.etype.clone(),
                    label: identity.label.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_identity_name(identity: &Identity) -> Result<(), ExecutionError> {
    if is_system_catalog_identity(&identity.etype, &identity.label)? {
        return Err(ExecutionError::ReservedIdentity {
            etype: identity.etype.clone(),
            label: identity.label.clone(),
        });
    }
    if identity.etype.starts_with('$')
        && !matches!(identity.etype.as_str(), "$users" | "$files" | "$streams")
    {
        return Err(ExecutionError::ReservedSystemNamespace(
            identity.etype.clone(),
        ));
    }
    Ok(())
}

async fn ensure_attribute_matches(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    definition: &AttributeDefinition,
) -> Result<(), ExecutionError> {
    let attribute = &definition.attribute;
    let row = transaction
        .query_one(
            "SELECT app_id, value_type, cardinality, is_unique, is_indexed,
                    coalesce(is_required, false) AS is_required,
                    forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
                    on_delete::text AS on_delete,
                    on_delete_reverse::text AS on_delete_reverse,
                    checked_data_type::text AS checked_data_type,
                    coalesce(setting_unique, false) AS setting_unique,
                    coalesce(indexing, false) AS indexing,
                    coalesce(checking_data_type, false) AS checking_data_type
             FROM attrs WHERE id = $1",
            &[&attribute.id],
        )
        .await?;
    let reverse_identity_id = definition
        .reverse_identity
        .as_ref()
        .map(|identity| identity.id);
    let matches = row.get::<_, Uuid>("app_id") == app_id
        && row.get::<_, String>("value_type") == attribute_value_type_name(attribute.value_type)
        && row.get::<_, String>("cardinality") == cardinality_name(attribute.cardinality)
        && row.get::<_, bool>("is_unique") == attribute.unique
        && row.get::<_, bool>("is_indexed") == attribute.indexed
        && row.get::<_, bool>("is_required") == attribute.required
        && row.get::<_, Uuid>("forward_ident") == definition.forward_identity.id
        && row.get::<_, String>("etype") == attribute.etype
        && row.get::<_, String>("label") == attribute.label
        && row.get::<_, Option<Uuid>>("reverse_ident") == reverse_identity_id
        && row.get::<_, Option<String>>("reverse_etype") == attribute.reverse_etype
        && row.get::<_, Option<String>>("reverse_label") == attribute.reverse_label
        && row.get::<_, Option<String>>("on_delete") == delete_action_name(definition.on_delete)
        && row.get::<_, Option<String>>("on_delete_reverse")
            == delete_action_name(definition.on_delete_reverse)
        && row.get::<_, Option<String>>("checked_data_type")
            == attribute
                .checked_data_type
                .map(|data_type| format!("{data_type:?}").to_lowercase())
        && row.get::<_, bool>("setting_unique") == attribute.setting_unique
        && row.get::<_, bool>("indexing") == attribute.indexing
        && row.get::<_, bool>("checking_data_type") == attribute.checking_data_type;
    if matches {
        Ok(())
    } else {
        Err(ExecutionError::AttributeConflict(attribute.id))
    }
}

async fn insert_identity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    attr_id: Uuid,
    identity: &Identity,
) -> Result<bool, ExecutionError> {
    let inserted = !transaction
        .query(
            "INSERT INTO idents (id, app_id, attr_id, etype, label)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO NOTHING
             RETURNING id",
            &[
                &identity.id,
                &app_id,
                &attr_id,
                &identity.etype,
                &identity.label,
            ],
        )
        .await?
        .is_empty();
    if !inserted {
        let row = transaction
            .query_one(
                "SELECT app_id, attr_id, etype, label FROM idents WHERE id = $1",
                &[&identity.id],
            )
            .await?;
        if row.get::<_, Uuid>("app_id") != app_id
            || row.get::<_, Uuid>("attr_id") != attr_id
            || row.get::<_, String>("etype") != identity.etype
            || row.get::<_, String>("label") != identity.label
        {
            return Err(ExecutionError::IdentityConflict(identity.id));
        }
    }
    Ok(inserted)
}

async fn backfill_indexed_nulls_for_attribute(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    attribute: &Attribute,
) -> Result<Vec<Uuid>, ExecutionError> {
    if !attribute.indexed || attribute.value_type != AttributeValueType::Blob {
        return Ok(Vec::new());
    }
    let rows = transaction
        .query(
            "SELECT DISTINCT triples.entity_id
             FROM triples
             JOIN attrs id_attr ON id_attr.id = triples.attr_id
             WHERE triples.app_id = $1
               AND id_attr.app_id = $1
               AND id_attr.etype = $2
               AND id_attr.label = 'id'
               AND NOT EXISTS (
                 SELECT 1 FROM triples existing
                 WHERE existing.app_id = $1
                   AND existing.entity_id = triples.entity_id
                   AND existing.attr_id = $3
               )",
            &[&app_id, &attribute.etype, &attribute.id],
        )
        .await?;
    let mut affected = Vec::new();
    let null = Value::Null;
    for row in rows {
        let entity_id = row.get("entity_id");
        if insert_triple(
            transaction,
            app_id,
            entity_id,
            attribute,
            &null,
            false,
            false,
        )
        .await?
        {
            affected.push(entity_id);
        }
    }
    Ok(affected)
}

const fn attribute_value_type_name(value_type: AttributeValueType) -> &'static str {
    match value_type {
        AttributeValueType::Blob => "blob",
        AttributeValueType::Ref => "ref",
    }
}

const fn cardinality_name(cardinality: Cardinality) -> &'static str {
    match cardinality {
        Cardinality::One => "one",
        Cardinality::Many => "many",
    }
}

fn delete_action_name(action: Option<DeleteAction>) -> Option<String> {
    action.map(|DeleteAction::Cascade| "cascade".to_owned())
}

#[allow(clippy::too_many_lines)] // Mirrors the persisted multi-column JVM attribute update.
async fn execute_update_attribute(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    update: &AttributeUpdate,
) -> Result<SchemaMutation, ExecutionError> {
    if let Some(identity) = &update.forward_identity {
        validate_identity_name(identity)?;
    }
    if let Some(Some(identity)) = &update.reverse_identity {
        validate_identity_name(identity)?;
    }
    let Some(old) = transaction
        .query_opt(
            "SELECT value_type, cardinality, is_unique, is_indexed
             FROM attrs WHERE app_id = $1 AND id = $2",
            &[&app_id, &update.id],
        )
        .await?
    else {
        return Ok(SchemaMutation {
            operation: TransactionOperation::UpdateAttr,
            attr_id: update.id,
            identity_ids: Vec::new(),
            affected_entity_ids: Vec::new(),
            changed: false,
        });
    };
    let old = AttributeStorageState {
        value_type: old.get("value_type"),
        cardinality: old.get("cardinality"),
        unique: old.get("is_unique"),
        indexed: old.get("is_indexed"),
    };
    let forward_etype = update
        .forward_identity
        .as_ref()
        .map(|identity| identity.etype.as_str());
    let forward_label = update
        .forward_identity
        .as_ref()
        .map(|identity| identity.label.as_str());
    let reverse_identity = update
        .reverse_identity
        .as_ref()
        .and_then(|identity| identity.as_ref());
    let reverse_etype = reverse_identity.map(|identity| identity.etype.as_str());
    let reverse_label = reverse_identity.map(|identity| identity.label.as_str());
    let cardinality = update.cardinality.map(cardinality_name);
    let on_delete_present = update.on_delete.is_some();
    let on_delete = update.on_delete.flatten().map(|_| "cascade");
    let on_delete_reverse_present = update.on_delete_reverse.is_some();
    let on_delete_reverse = update.on_delete_reverse.flatten().map(|_| "cascade");
    transaction
        .execute(
            "UPDATE attrs
             SET etype = coalesce($3::text, etype),
                 label = coalesce($4::text, label),
                 reverse_etype = coalesce($5::text, reverse_etype),
                 reverse_label = coalesce($6::text, reverse_label),
                 cardinality = coalesce($7::text, cardinality),
                 is_unique = coalesce($8::boolean, is_unique),
                 is_indexed = coalesce($9::boolean, is_indexed),
                 is_required = coalesce($10::boolean, is_required),
                 on_delete = CASE WHEN $11 THEN $12::text::attr_on_delete ELSE on_delete END,
                 on_delete_reverse = CASE WHEN $13 THEN $14::text::attr_on_delete ELSE on_delete_reverse END
             WHERE app_id = $1 AND id = $2",
            &[
                &app_id,
                &update.id,
                &forward_etype,
                &forward_label,
                &reverse_etype,
                &reverse_label,
                &cardinality,
                &update.unique,
                &update.indexed,
                &update.required,
                &on_delete_present,
                &on_delete,
                &on_delete_reverse_present,
                &on_delete_reverse,
            ],
        )
        .await?;

    let affected_entity_ids = propagate_attribute_flags(transaction, app_id, update, &old).await?;
    let identity_ids =
        update_attribute_identities(transaction, app_id, update, reverse_identity).await?;
    Ok(SchemaMutation {
        operation: TransactionOperation::UpdateAttr,
        attr_id: update.id,
        identity_ids,
        affected_entity_ids,
        changed: true,
    })
}

async fn propagate_attribute_flags(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    update: &AttributeUpdate,
    old: &AttributeStorageState,
) -> Result<Vec<Uuid>, ExecutionError> {
    let new_cardinality = match update.cardinality {
        Some(cardinality) => cardinality_name(cardinality),
        None => old.cardinality.as_str(),
    };
    let new_unique = update.unique.unwrap_or(old.unique);
    let new_indexed = update.indexed.unwrap_or(old.indexed);
    if new_cardinality == old.cardinality && new_unique == old.unique && new_indexed == old.indexed
    {
        return Ok(Vec::new());
    }
    let ea = new_cardinality == "one";
    let reference = old.value_type == "ref";
    let rows = transaction
        .query(
            "UPDATE triples
             SET ea = $3, eav = $4, av = $5, ave = $6, vae = $4
             WHERE app_id = $1 AND attr_id = $2
             RETURNING entity_id",
            &[
                &app_id,
                &update.id,
                &ea,
                &reference,
                &new_unique,
                &new_indexed,
            ],
        )
        .await?;
    let mut seen = HashSet::new();
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let entity_id = row.get("entity_id");
            seen.insert(entity_id).then_some(entity_id)
        })
        .collect())
}

async fn update_attribute_identities(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    update: &AttributeUpdate,
    reverse_identity: Option<&Identity>,
) -> Result<Vec<Uuid>, ExecutionError> {
    let mut identity_ids = Vec::new();
    for identity in update.forward_identity.iter().chain(reverse_identity) {
        if transaction
            .execute(
                "UPDATE idents SET etype = $4, label = $5
                 WHERE id = $1 AND app_id = $2 AND attr_id = $3",
                &[
                    &identity.id,
                    &app_id,
                    &update.id,
                    &identity.etype,
                    &identity.label,
                ],
            )
            .await?
            > 0
        {
            identity_ids.push(identity.id);
        }
    }
    Ok(identity_ids)
}

async fn execute_delete_attribute(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    attr_id: Uuid,
) -> Result<SchemaMutation, ExecutionError> {
    let row = transaction
        .query_opt(
            "UPDATE attrs AS target
             SET deletion_marked_at = now(),
                 is_indexed = false,
                 is_required = false,
                 metadata = coalesce(target.metadata, '{}'::jsonb) || jsonb_build_object(
                   'soft_delete_snapshot', jsonb_build_object(
                     'is_indexed', target.is_indexed,
                     'is_required', target.is_required,
                     'id_attr_id', (
                       SELECT id_attr.id FROM attrs AS id_attr
                       WHERE (id_attr.app_id = target.app_id OR id_attr.app_id = $3)
                         AND id_attr.etype = target.etype
                         AND id_attr.label = 'id'
                       LIMIT 1
                     )
                   )
                 ),
                 etype = target.id::text || '_deleted$' || target.etype,
                 label = target.id::text || '_deleted$' || target.label,
                 reverse_etype = CASE WHEN target.reverse_etype IS NULL THEN NULL
                   ELSE target.id::text || '_deleted$' || target.reverse_etype END,
                 reverse_label = CASE WHEN target.reverse_label IS NULL THEN NULL
                   ELSE target.id::text || '_deleted$' || target.reverse_label END
             WHERE target.app_id = $1 AND target.id = $2
               AND target.deletion_marked_at IS NULL
             RETURNING target.forward_ident, target.reverse_ident,
                       target.etype, target.label, target.reverse_etype, target.reverse_label",
            &[&app_id, &attr_id, &SYSTEM_CATALOG_APP_ID],
        )
        .await?;
    schema_lifecycle_result(
        transaction,
        app_id,
        attr_id,
        TransactionOperation::DeleteAttr,
        row,
    )
    .await
}

async fn execute_restore_attribute(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    attr_id: Uuid,
) -> Result<SchemaMutation, ExecutionError> {
    let row = transaction
        .query_opt(
            "UPDATE attrs
             SET deletion_marked_at = NULL,
                 is_indexed = false,
                 is_required = false,
                 metadata = metadata - 'soft_delete_snapshot',
                 etype = substring(etype FROM position('$' IN etype) + 1),
                 label = substring(label FROM position('$' IN etype) + 1),
                 reverse_etype = CASE WHEN reverse_etype IS NULL THEN NULL
                   ELSE substring(reverse_etype FROM position('$' IN etype) + 1) END,
                 reverse_label = CASE WHEN reverse_label IS NULL THEN NULL
                   ELSE substring(reverse_label FROM position('$' IN etype) + 1) END
             WHERE app_id = $1 AND id = $2 AND deletion_marked_at IS NOT NULL
             RETURNING forward_ident, reverse_ident, etype, label, reverse_etype, reverse_label",
            &[&app_id, &attr_id],
        )
        .await?;
    schema_lifecycle_result(
        transaction,
        app_id,
        attr_id,
        TransactionOperation::RestoreAttr,
        row,
    )
    .await
}

async fn schema_lifecycle_result(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    attr_id: Uuid,
    operation: TransactionOperation,
    row: Option<tokio_postgres::Row>,
) -> Result<SchemaMutation, ExecutionError> {
    let Some(row) = row else {
        return Ok(SchemaMutation {
            operation,
            attr_id,
            identity_ids: Vec::new(),
            affected_entity_ids: Vec::new(),
            changed: false,
        });
    };
    let forward_ident = row.get::<_, Uuid>("forward_ident");
    let reverse_ident = row.get::<_, Option<Uuid>>("reverse_ident");
    let mut identity_ids = Vec::new();
    let identities = [
        (
            Some(forward_ident),
            Some(row.get::<_, String>("etype")),
            Some(row.get::<_, String>("label")),
        ),
        (
            reverse_ident,
            row.get::<_, Option<String>>("reverse_etype"),
            row.get::<_, Option<String>>("reverse_label"),
        ),
    ];
    for (identity_id, etype, label) in identities {
        let (Some(identity_id), Some(etype), Some(label)) = (identity_id, etype, label) else {
            continue;
        };
        if transaction
            .execute(
                "UPDATE idents SET etype = $4, label = $5
                 WHERE id = $1 AND app_id = $2 AND attr_id = $3",
                &[&identity_id, &app_id, &attr_id, &etype, &label],
            )
            .await?
            > 0
        {
            identity_ids.push(identity_id);
        }
    }
    Ok(SchemaMutation {
        operation,
        attr_id,
        identity_ids,
        affected_entity_ids: Vec::new(),
        changed: true,
    })
}

async fn execute_add_group(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    group: &[TransactionStep],
    lookups: AddGroupLookups<'_>,
    options: TransactionOptions,
    mutations: &mut Vec<Mutation>,
) -> Result<(), ExecutionError> {
    let inferred_types = inferred_type_payload(group);
    let group = collapse_cardinality_one_adds(catalog, group)?;
    let new_entity_types = find_new_id_lookup_types(transaction, app_id, catalog, &group).await?;
    let mut resolved = Vec::with_capacity(group.len());
    for step in &group {
        let TransactionStep::AddTriple(step) = step else {
            unreachable!("the add group was bounded by its operation")
        };
        let attribute = catalog
            .by_id(step.attr_id)
            .ok_or(ExecutionError::UnknownAttribute(step.attr_id))?;
        validate_entity_lookup_type(catalog, &step.entity, attribute)?;
        let entity_id = resolve_entity(transaction, app_id, catalog, &step.entity, true).await?;
        let value = materialize_value(
            transaction,
            app_id,
            catalog,
            attribute,
            entity_id,
            &step.value,
        )
        .await?;
        resolved.push(ResolvedAddTriple {
            entity_id,
            attr_id: attribute.id,
            value,
        });
    }
    let indexed_nulls = collect_indexed_null_candidates(
        transaction,
        app_id,
        catalog,
        &group,
        &resolved,
        &new_entity_types,
        lookups.materialized,
    )
    .await?;

    let rows = insert_add_group(
        transaction,
        app_id,
        &resolved,
        &inferred_types,
        &indexed_nulls,
        lookups,
        options.overwrite_created_at,
    )
    .await?;
    mutations.extend(rows.into_iter().map(|(entity_id, attr_id)| Mutation {
        operation: TransactionOperation::AddTriple,
        entity_id,
        attr_id,
    }));
    Ok(())
}

#[derive(Debug, PartialEq)]
struct ResolvedAddTriple {
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
}

#[derive(Debug, PartialEq)]
struct IndexedNullCandidate {
    entity: Uuid,
    attr: Uuid,
    trigger_id_attr_id: Option<Uuid>,
}

#[derive(Clone, Copy)]
struct AddGroupLookups<'a> {
    materialized: &'a [PreparedLookup],
    collision: &'a [LookupRef],
}

fn lookup_payload(lookups: &[PreparedLookup]) -> Value {
    Value::Array(
        lookups
            .iter()
            .map(|prepared| {
                json!({
                    "entity_id": prepared.entity_id,
                    "attr_id": prepared.lookup.attr_id,
                    "value": prepared.lookup.value,
                })
            })
            .collect(),
    )
}

fn add_group_payload(group: &[ResolvedAddTriple]) -> Value {
    Value::Array(
        group
            .iter()
            .map(|triple| {
                json!({
                    "entity_id": triple.entity_id,
                    "attr_id": triple.attr_id,
                    "value": triple.value,
                })
            })
            .collect(),
    )
}

fn indexed_null_payload(candidates: &[IndexedNullCandidate]) -> Value {
    Value::Array(
        candidates
            .iter()
            .map(|candidate| {
                json!({
                    "entity_id": candidate.entity,
                    "attr_id": candidate.attr,
                    "trigger_id_attr_id": candidate.trigger_id_attr_id,
                })
            })
            .collect(),
    )
}

fn inferred_type_payload(group: &[TransactionStep]) -> Value {
    let mut inferred = Vec::<(Uuid, i32)>::new();
    for step in group {
        let (TransactionStep::AddTriple(triple) | TransactionStep::DeepMergeTriple(triple)) = step
        else {
            continue;
        };
        let Some(type_bit) = inferred_type_bit(&triple.value) else {
            continue;
        };
        if let Some((_, mask)) = inferred
            .iter_mut()
            .find(|(attr_id, _)| *attr_id == triple.attr_id)
        {
            *mask |= type_bit;
        } else {
            inferred.push((triple.attr_id, type_bit));
        }
    }
    Value::Array(
        inferred
            .into_iter()
            .map(|(attr_id, mask)| json!({"attr_id": attr_id, "mask": mask}))
            .collect(),
    )
}

async fn insert_add_group(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    group: &[ResolvedAddTriple],
    inferred_types: &Value,
    indexed_nulls: &[IndexedNullCandidate],
    lookups: AddGroupLookups<'_>,
    overwrite_created_at: bool,
) -> Result<Vec<(Uuid, Uuid)>, ExecutionError> {
    if group.is_empty() {
        return Ok(Vec::new());
    }
    let payload = add_group_payload(group);
    let indexed_nulls = indexed_null_payload(indexed_nulls);
    let lookups_payload = lookup_payload(lookups.materialized);
    let rows = transaction
        // Every input has a built-in type fixed by this SQL contract. Parse,
        // bind and execute can share one protocol exchange without retaining a
        // prepared plan or changing the statement, values or write ordering.
        .query_typed(
            INSERT_ADD_GROUP,
            &[
                (&app_id, Type::UUID),
                (&Json(&payload), Type::JSONB),
                (&Json(inferred_types), Type::JSONB),
                (&SYSTEM_CATALOG_APP_ID, Type::UUID),
                (&overwrite_created_at, Type::BOOL),
                (&Json(&indexed_nulls), Type::JSONB),
                (&Json(&lookups_payload), Type::JSONB),
            ],
        )
        .await
        .map_err(|error| {
            if !lookups.collision.is_empty()
                && error.as_db_error().is_some_and(|database| {
                    database.message()
                        == "ON CONFLICT DO UPDATE command cannot affect row a second time"
                        || (database.code() == &SqlState::UNIQUE_VIOLATION
                            && matches!(database.constraint(), Some("tx_ea_index" | "ea_index")))
                })
            {
                ExecutionError::LookupUpdateCollision(lookups.collision.to_vec())
            } else {
                ExecutionError::from(error)
            }
        })?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("entity_id"), row.get("attr_id")))
        .collect())
}

#[derive(Debug, PartialEq)]
struct DeepMergePatches {
    entity: EntityLookup,
    attr_id: Uuid,
    patches: Vec<Value>,
}

#[derive(Debug, PartialEq)]
struct ResolvedDeepMergePatches {
    entity_id: Uuid,
    attr_id: Uuid,
    patches: Vec<Value>,
}

fn group_deep_merge_patches(
    catalog: &AttributeCatalog,
    group: &[TransactionStep],
) -> Result<Vec<DeepMergePatches>, ExecutionError> {
    let mut grouped = Vec::<DeepMergePatches>::new();
    for step in group {
        let TransactionStep::DeepMergeTriple(triple) = step else {
            unreachable!("the deep-merge group was bounded by its operation")
        };
        let attribute = catalog
            .by_id(triple.attr_id)
            .ok_or(ExecutionError::UnknownAttribute(triple.attr_id))?;
        if attribute.value_type == AttributeValueType::Ref {
            return Err(ExecutionError::DeepMergeReference(attribute.id));
        }
        if attribute.cardinality != Cardinality::One {
            return Err(ExecutionError::DeepMergeMany(attribute.id));
        }
        validate_entity_lookup_type(catalog, &triple.entity, attribute)?;
        if let Some(existing) = grouped.iter_mut().find(|candidate| {
            candidate.entity == triple.entity && candidate.attr_id == triple.attr_id
        }) {
            existing.patches.push(triple.value.clone());
        } else {
            grouped.push(DeepMergePatches {
                entity: triple.entity.clone(),
                attr_id: triple.attr_id,
                patches: vec![triple.value.clone()],
            });
        }
    }
    Ok(grouped)
}

async fn execute_deep_merge_group(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    group: &[TransactionStep],
    overwrite_created_at: bool,
    mutations: &mut Vec<Mutation>,
) -> Result<(), ExecutionError> {
    let grouped = group_deep_merge_patches(catalog, group)?;
    for step in group {
        let TransactionStep::DeepMergeTriple(triple) = step else {
            unreachable!("validated above")
        };
        update_inferred_type(transaction, app_id, triple.attr_id, &triple.value).await?;
    }
    let mut resolved = Vec::with_capacity(grouped.len());
    for merged in grouped {
        let entity_id = resolve_entity(transaction, app_id, catalog, &merged.entity, true).await?;
        resolved.push(ResolvedDeepMergePatches {
            entity_id,
            attr_id: merged.attr_id,
            patches: merged.patches,
        });
    }
    let rows =
        insert_deep_merge_groups(transaction, app_id, &resolved, overwrite_created_at).await?;
    mutations.extend(rows.into_iter().map(|(entity_id, attr_id)| Mutation {
        operation: TransactionOperation::DeepMergeTriple,
        entity_id,
        attr_id,
    }));
    Ok(())
}

fn collapse_cardinality_one_adds(
    catalog: &AttributeCatalog,
    group: &[TransactionStep],
) -> Result<Vec<TransactionStep>, ExecutionError> {
    for step in group {
        let TransactionStep::AddTriple(triple) = step else {
            unreachable!("the add group was bounded by its operation")
        };
        catalog
            .by_id(triple.attr_id)
            .ok_or(ExecutionError::UnknownAttribute(triple.attr_id))?;
    }
    let mut seen = Vec::<(EntityLookup, Uuid)>::new();
    let mut selected = Vec::with_capacity(group.len());
    for step in group.iter().rev() {
        let TransactionStep::AddTriple(triple) = step else {
            unreachable!("validated above")
        };
        let attribute = catalog
            .by_id(triple.attr_id)
            .expect("attributes were validated above");
        if attribute.cardinality == Cardinality::One {
            let duplicate = seen
                .iter()
                .any(|(entity, attr_id)| entity == &triple.entity && *attr_id == triple.attr_id);
            if duplicate {
                continue;
            }
            seen.push((triple.entity.clone(), triple.attr_id));
        }
        selected.push(step.clone());
    }
    selected.reverse();
    Ok(selected)
}

async fn update_inferred_type(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    attr_id: Uuid,
    value: &Value,
) -> Result<(), ExecutionError> {
    let Some(type_bit) = inferred_type_bit(value) else {
        return Ok(());
    };
    transaction
        .execute(
            "UPDATE attrs
             SET inferred_types = coalesce(inferred_types, 0::bit(32)) | $3::integer::bit(32)
             WHERE app_id = $1 AND id = $2
               AND inferred_types IS DISTINCT FROM
                   (coalesce(inferred_types, 0::bit(32)) | $3::integer::bit(32))",
            &[&app_id, &attr_id, &type_bit],
        )
        .await?;
    Ok(())
}

const fn inferred_type_bit(value: &Value) -> Option<i32> {
    match value {
        Value::Null => None,
        Value::Number(_) => Some(1),
        Value::String(_) => Some(2),
        Value::Bool(_) => Some(4),
        Value::Array(_) | Value::Object(_) => Some(8),
    }
}

async fn execute_retract(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    step: &TripleStep,
    mutations: &mut Vec<Mutation>,
) -> Result<(), ExecutionError> {
    let attribute = catalog
        .by_id(step.attr_id)
        .ok_or(ExecutionError::UnknownAttribute(step.attr_id))?;
    validate_entity_lookup_type(catalog, &step.entity, attribute)?;
    let Some(entity_id) =
        resolve_existing_entity(transaction, app_id, catalog, &step.entity).await?
    else {
        return Ok(());
    };
    let rows = transaction
        .query(
            "DELETE FROM triples
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3 AND value = $4
             RETURNING entity_id, attr_id",
            &[&app_id, &entity_id, &attribute.id, &Json(&step.value)],
        )
        .await?;
    mutations.extend(rows.into_iter().map(|row| Mutation {
        operation: TransactionOperation::RetractTriple,
        entity_id: row.get("entity_id"),
        attr_id: row.get("attr_id"),
    }));
    Ok(())
}

async fn execute_delete_entity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    step: &crate::DeleteEntityStep,
    mutations: &mut Vec<Mutation>,
) -> Result<(), ExecutionError> {
    let Some(entity_id) =
        resolve_existing_entity(transaction, app_id, catalog, &step.entity).await?
    else {
        return Ok(());
    };
    let Some(etype) = step.etype.as_deref() else {
        return Ok(());
    };
    delete_typed_entity(transaction, app_id, catalog, entity_id, etype, mutations).await?;
    Ok(())
}

pub(crate) async fn expand_delete_cascades(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    roots: &[(Uuid, String)],
) -> Result<Vec<(Uuid, String)>, ExecutionError> {
    let mut visited = HashSet::new();
    let mut queue = roots.iter().cloned().collect::<VecDeque<_>>();
    let mut result = Vec::new();
    while let Some((entity_id, etype)) = queue.pop_front() {
        if !visited.insert((entity_id, etype.clone())) {
            continue;
        }
        result.push((entity_id, etype.clone()));
        let reference = json!(entity_id);
        for attribute in catalog.attributes.iter().filter(|attribute| {
            attribute.value_type == AttributeValueType::Ref
                && attribute.on_delete == Some(DeleteAction::Cascade)
                && attribute.reverse_etype.as_deref() == Some(etype.as_str())
        }) {
            let rows = transaction
                .query(
                    "SELECT DISTINCT entity_id FROM triples
                     WHERE app_id = $1 AND attr_id = $2 AND vae AND value = $3
                       AND public.json_uuid_to_uuid(value) = $4",
                    &[&app_id, &attribute.id, &Json(&reference), &entity_id],
                )
                .await?;
            queue.extend(
                rows.into_iter()
                    .map(|row| (row.get("entity_id"), attribute.etype.clone())),
            );
        }
        for attribute in catalog.attributes.iter().filter(|attribute| {
            attribute.value_type == AttributeValueType::Ref
                && attribute.on_delete_reverse == Some(DeleteAction::Cascade)
                && attribute.etype == etype
        }) {
            let Some(reverse_etype) = &attribute.reverse_etype else {
                continue;
            };
            let rows = transaction
                .query(
                    "SELECT DISTINCT value FROM triples
                     WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3 AND eav",
                    &[&app_id, &entity_id, &attribute.id],
                )
                .await?;
            for row in rows {
                let value = row.get::<_, Json<Value>>("value").0;
                let Some(child_id) = value.as_str().and_then(|value| Uuid::parse_str(value).ok())
                else {
                    continue;
                };
                queue.push_back((child_id, reverse_etype.clone()));
            }
        }
    }
    Ok(result)
}

async fn delete_typed_entity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entity_id: Uuid,
    etype: &str,
    mutations: &mut Vec<Mutation>,
) -> Result<(), ExecutionError> {
    let forward = catalog
        .attributes
        .iter()
        .filter(|attribute| attribute.etype == etype)
        .map(|attribute| attribute.id)
        .collect::<Vec<_>>();
    let reverse = catalog
        .attributes
        .iter()
        .filter(|attribute| {
            attribute.value_type == AttributeValueType::Ref
                && attribute.reverse_etype.as_deref() == Some(etype)
        })
        .map(|attribute| attribute.id)
        .collect::<Vec<_>>();
    let reference = json!(entity_id);
    let rows = transaction
        .query(
            "DELETE FROM triples
             WHERE app_id = $1
               AND ((entity_id = $2 AND attr_id = ANY($3))
                    OR (vae AND value = $4 AND public.json_uuid_to_uuid(value) = $2
                        AND attr_id = ANY($5)))
             RETURNING entity_id, attr_id",
            &[&app_id, &entity_id, &forward, &Json(&reference), &reverse],
        )
        .await?;
    mutations.extend(rows.into_iter().map(|row| Mutation {
        operation: TransactionOperation::DeleteEntity,
        entity_id: row.get("entity_id"),
        attr_id: row.get("attr_id"),
    }));
    Ok(())
}

async fn insert_triple(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    entity_id: Uuid,
    attribute: &Attribute,
    value: &Value,
    deep_merge: bool,
    overwrite_created_at: bool,
) -> Result<bool, ExecutionError> {
    let ea = attribute.cardinality == Cardinality::One;
    let reference = attribute.value_type == AttributeValueType::Ref;
    let checked_data_type = attribute
        .checked_data_type
        .map(|data_type| format!("{data_type:?}").to_lowercase());
    let sql = match (ea, deep_merge, overwrite_created_at) {
        (true, true, true) => INSERT_EA_DEEP_MERGE_OVERWRITE_TIME,
        (true, true, false) => INSERT_EA_DEEP_MERGE,
        (true, false, true) => INSERT_EA_OVERWRITE_TIME,
        (true, false, false) => INSERT_EA,
        (false, _, _) => INSERT_MANY,
    };
    let rows = transaction
        .query(
            sql,
            &[
                &app_id,
                &entity_id,
                &attribute.id,
                &Json(value),
                &ea,
                &reference,
                &attribute.unique,
                &attribute.indexed,
                &reference,
                &checked_data_type,
            ],
        )
        .await?;
    Ok(!rows.is_empty())
}

fn deep_merge_group_payload(groups: &[ResolvedDeepMergePatches]) -> Value {
    Value::Array(
        groups
            .iter()
            .map(|group| {
                json!({
                    "entity_id": group.entity_id,
                    "attr_id": group.attr_id,
                    "patches": group.patches,
                })
            })
            .collect(),
    )
}

async fn insert_deep_merge_groups(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    groups: &[ResolvedDeepMergePatches],
    overwrite_created_at: bool,
) -> Result<Vec<(Uuid, Uuid)>, ExecutionError> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let payload = deep_merge_group_payload(groups);
    let sql = if overwrite_created_at {
        INSERT_EA_DEEP_MERGE_GROUPS_OVERWRITE_TIME
    } else {
        INSERT_EA_DEEP_MERGE_GROUPS
    };
    let rows = transaction
        .query_typed(
            sql,
            &[
                (&app_id, Type::UUID),
                (&Json(&payload), Type::JSONB),
                (&SYSTEM_CATALOG_APP_ID, Type::UUID),
            ],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("entity_id"), row.get("attr_id")))
        .collect())
}

async fn resolve_entity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entity: &EntityLookup,
    create: bool,
) -> Result<Uuid, ExecutionError> {
    match entity {
        EntityLookup::Id(id) => Ok(*id),
        EntityLookup::Lookup(lookup) => {
            if let Some(id) = lookup_entity(transaction, app_id, catalog, lookup).await? {
                return Ok(id);
            }
            if !create {
                return Err(ExecutionError::MissingLookup(lookup.clone()));
            }
            let attribute = lookup_attribute(catalog, lookup)?;
            let id = if attribute.label == "id" {
                lookup
                    .value
                    .as_str()
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .ok_or_else(|| ExecutionError::InvalidReferenceValue {
                        attr_id: attribute.id,
                        value: lookup.value.clone(),
                        entity_id: None,
                    })?
            } else {
                Uuid::new_v4()
            };
            insert_lookup_triple(transaction, app_id, id, attribute, &lookup.value).await?;
            lookup_entity(transaction, app_id, catalog, lookup)
                .await?
                .ok_or_else(|| ExecutionError::MissingLookup(lookup.clone()))
        }
    }
}

async fn insert_lookup_triple(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    entity_id: Uuid,
    attribute: &Attribute,
    value: &Value,
) -> Result<(), ExecutionError> {
    let ea = attribute.cardinality == Cardinality::One;
    let reference = attribute.value_type == AttributeValueType::Ref;
    let checked_data_type = attribute
        .checked_data_type
        .map(|data_type| format!("{data_type:?}").to_lowercase());
    transaction
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
             VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), $5, $6, true, $7, $6, $8::text::checked_data_type)
             ON CONFLICT (app_id, attr_id, public.json_null_to_null(value)) WHERE av
             DO NOTHING",
            &[
                &app_id,
                &entity_id,
                &attribute.id,
                &Json(value),
                &ea,
                &reference,
                &attribute.indexed,
                &checked_data_type,
            ],
        )
        .await?;
    Ok(())
}

async fn resolve_existing_entity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entity: &EntityLookup,
) -> Result<Option<Uuid>, ExecutionError> {
    match entity {
        EntityLookup::Id(id) => Ok(Some(*id)),
        EntityLookup::Lookup(lookup) => lookup_entity(transaction, app_id, catalog, lookup).await,
    }
}

async fn lookup_entity(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    lookup: &LookupRef,
) -> Result<Option<Uuid>, ExecutionError> {
    let attribute = lookup_attribute(catalog, lookup)?;
    transaction
        .query_opt(
            "SELECT entity_id FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND av AND value = $3
             LIMIT 1",
            &[&app_id, &attribute.id, &Json(&lookup.value)],
        )
        .await
        .map(|row| row.map(|row| row.get("entity_id")))
        .map_err(ExecutionError::from)
}

fn lookup_attribute<'a>(
    catalog: &'a AttributeCatalog,
    lookup: &LookupRef,
) -> Result<&'a Attribute, ExecutionError> {
    let attribute = catalog
        .by_id(lookup.attr_id)
        .ok_or(ExecutionError::UnknownAttribute(lookup.attr_id))?;
    if !attribute.unique {
        return Err(ExecutionError::LookupAttributeNotUnique(attribute.id));
    }
    Ok(attribute)
}

fn validate_entity_lookup_type(
    catalog: &AttributeCatalog,
    entity: &EntityLookup,
    target_attribute: &Attribute,
) -> Result<(), ExecutionError> {
    let EntityLookup::Lookup(lookup) = entity else {
        return Ok(());
    };
    let lookup_attribute = lookup_attribute(catalog, lookup)?;
    if lookup_attribute.etype != target_attribute.etype {
        return Err(ExecutionError::LookupEntityTypeMismatch {
            lookup_etype: lookup_attribute.etype.clone(),
            target_etype: target_attribute.etype.clone(),
        });
    }
    Ok(())
}

async fn materialize_value(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    attribute: &Attribute,
    entity_id: Uuid,
    value: &Value,
) -> Result<Value, ExecutionError> {
    let lookupable = attribute.value_type == AttributeValueType::Ref || attribute.label == "id";
    let value = if lookupable {
        if let Some(lookup) = value_lookup(value) {
            let entity_id = lookup_entity(transaction, app_id, catalog, &lookup)
                .await?
                .ok_or_else(|| ExecutionError::MissingLookup(lookup.clone()))?;
            json!(entity_id)
        } else {
            value.clone()
        }
    } else {
        value.clone()
    };
    if attribute.value_type == AttributeValueType::Ref
        && value
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .is_none()
    {
        return Err(ExecutionError::InvalidReferenceValue {
            attr_id: attribute.id,
            value,
            entity_id: Some(entity_id),
        });
    }
    Ok(value)
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

/// Reads whether a create/update target already exists for mode validation.
/// The transaction is the only production reader; tests substitute a counter
/// to prove the per-entity and per-lookup memoization.
trait ModeExistence {
    fn entity_exists(
        &self,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        entity_id: Uuid,
        etype: &str,
    ) -> impl std::future::Future<Output = Result<bool, ExecutionError>>;

    fn lookup_exists(
        &self,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        lookup: &LookupRef,
    ) -> impl std::future::Future<Output = Result<bool, ExecutionError>>;
}

impl ModeExistence for Transaction<'_> {
    fn entity_exists(
        &self,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        entity_id: Uuid,
        etype: &str,
    ) -> impl std::future::Future<Output = Result<bool, ExecutionError>> {
        entity_exists_in_etype(self, app_id, catalog, entity_id, etype)
    }

    fn lookup_exists(
        &self,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        lookup: &LookupRef,
    ) -> impl std::future::Future<Output = Result<bool, ExecutionError>> {
        async move {
            lookup_entity(self, app_id, catalog, lookup)
                .await
                .map(|entity| entity.is_some())
        }
    }
}

async fn validate_modes(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<(), ExecutionError> {
    validate_modes_with(transaction, app_id, catalog, steps).await
}

async fn validate_modes_with(
    existence: &impl ModeExistence,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<(), ExecutionError> {
    let mut existing_creates = Vec::new();
    let mut missing_updates = Vec::new();
    // Every step reads the same pre-write snapshot, and an update or create
    // sends one step per attribute. Ask once per entity type or lookup.
    let mut known_entities = HashMap::<(Uuid, &str), bool>::new();
    let mut known_lookups = Vec::<(&LookupRef, bool)>::new();
    for transaction_step in steps {
        let (step, mode) = match transaction_step {
            TransactionStep::AddTriple(step) | TransactionStep::DeepMergeTriple(step) => {
                (step, step.options.mode)
            }
            _ => continue,
        };
        let Some(mode @ (TransactionMode::Create | TransactionMode::Update)) = mode else {
            continue;
        };
        let attribute = catalog
            .by_id(step.attr_id)
            .ok_or(ExecutionError::UnknownAttribute(step.attr_id))?;
        validate_entity_lookup_type(catalog, &step.entity, attribute)?;
        let exists = match &step.entity {
            EntityLookup::Lookup(lookup) => {
                match known_lookups.iter().find(|(known, _)| *known == lookup) {
                    Some((_, exists)) => *exists,
                    None => {
                        let exists = existence.lookup_exists(app_id, catalog, lookup).await?;
                        known_lookups.push((lookup, exists));
                        exists
                    }
                }
            }
            EntityLookup::Id(entity_id) => {
                match known_entities.get(&(*entity_id, attribute.etype.as_str())) {
                    Some(exists) => *exists,
                    None => {
                        let exists = existence
                            .entity_exists(app_id, catalog, *entity_id, &attribute.etype)
                            .await?;
                        known_entities.insert((*entity_id, attribute.etype.as_str()), exists);
                        exists
                    }
                }
            }
        };
        match (mode, exists) {
            (TransactionMode::Create, true) => existing_creates.push(transaction_step.clone()),
            (TransactionMode::Update, false) => missing_updates.push(transaction_step.clone()),
            _ => {}
        }
    }
    if !existing_creates.is_empty() {
        return Err(ExecutionError::CreateExisting(existing_creates));
    }
    if !missing_updates.is_empty() {
        return Err(ExecutionError::UpdateMissing(missing_updates));
    }
    Ok(())
}

/// Validates create/update mode constraints against the pre-write snapshot.
///
/// # Errors
///
/// Returns create-existing, update-missing, lookup, or database errors before
/// lookup preparation or permission loading can change their precedence.
pub async fn validate_transaction_modes(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<(), ExecutionError> {
    let validation_catalog = optimistic_catalog(catalog, steps);
    validate_modes(transaction, app_id, &validation_catalog, steps).await
}

async fn entity_exists_in_etype(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entity_id: Uuid,
    etype: &str,
) -> Result<bool, ExecutionError> {
    let attr_ids = catalog
        .attributes
        .iter()
        .filter(|attribute| attribute.etype == etype)
        .map(|attribute| attribute.id)
        .collect::<Vec<_>>();
    transaction
        .query_one(
            "SELECT EXISTS(
               SELECT 1 FROM triples
               WHERE app_id = $1 AND entity_id = $2 AND attr_id = ANY($3)
             )",
            &[&app_id, &entity_id, &attr_ids],
        )
        .await
        .map(|row| row.get(0))
        .map_err(ExecutionError::from)
}

async fn find_new_id_lookup_types(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<Vec<NewEntityType>, ExecutionError> {
    let mut result = Vec::new();
    for step in steps {
        let TransactionStep::AddTriple(step) = step else {
            continue;
        };
        let target_attribute = catalog
            .by_id(step.attr_id)
            .ok_or(ExecutionError::UnknownAttribute(step.attr_id))?;
        validate_entity_lookup_type(catalog, &step.entity, target_attribute)?;
        let EntityLookup::Lookup(lookup) = &step.entity else {
            continue;
        };
        let lookup_attribute = lookup_attribute(catalog, lookup)?;
        if lookup_attribute.label != "id"
            || lookup_entity(transaction, app_id, catalog, lookup)
                .await?
                .is_some()
        {
            continue;
        }
        let candidate = NewEntityType {
            entity: step.entity.clone(),
            etype: lookup_attribute.etype.clone(),
        };
        if !result.contains(&candidate) {
            result.push(candidate);
        }
    }
    Ok(result)
}

async fn collect_indexed_null_candidates(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    resolved: &[ResolvedAddTriple],
    new_entity_types: &[NewEntityType],
    lookups: &[PreparedLookup],
) -> Result<Vec<IndexedNullCandidate>, ExecutionError> {
    let mut entity_types = Vec::<(Uuid, String, Option<Uuid>)>::new();
    for new_entity_type in new_entity_types {
        let Some(entity_id) =
            resolve_existing_entity(transaction, app_id, catalog, &new_entity_type.entity).await?
        else {
            continue;
        };
        entity_types.push((entity_id, new_entity_type.etype.clone(), None));
    }
    for prepared in lookups {
        let attribute = lookup_attribute(catalog, &prepared.lookup)?;
        if attribute.label == "id" {
            entity_types.push((prepared.entity_id, attribute.etype.clone(), None));
        }
    }
    for (step, triple) in steps.iter().zip(resolved) {
        let TransactionStep::AddTriple(step) = step else {
            continue;
        };
        let attribute = catalog
            .by_id(step.attr_id)
            .ok_or(ExecutionError::UnknownAttribute(step.attr_id))?;
        if attribute.label == "id" {
            entity_types.push((
                triple.entity_id,
                attribute.etype.clone(),
                Some(attribute.id),
            ));
        }
    }

    let mut result = Vec::<IndexedNullCandidate>::new();
    for (entity_id, etype, trigger_id_attr_id) in entity_types {
        for attribute in catalog.attributes.iter().filter(|attribute| {
            attribute.etype == etype
                && attribute.indexed
                && attribute.value_type == AttributeValueType::Blob
        }) {
            let candidate = IndexedNullCandidate {
                entity: entity_id,
                attr: attribute.id,
                trigger_id_attr_id,
            };
            if let Some(existing) = result.iter_mut().find(|existing| {
                existing.entity == candidate.entity && existing.attr == candidate.attr
            }) {
                if candidate.trigger_id_attr_id.is_none() {
                    existing.trigger_id_attr_id = None;
                }
            } else {
                result.push(candidate);
            }
        }
    }
    Ok(result)
}

async fn validate_required_attributes(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    mutations: &[Mutation],
) -> Result<(), ExecutionError> {
    let mut entity_types = Vec::<(Uuid, String)>::new();
    for mutation in mutations {
        let Some(attribute) = catalog.by_id(mutation.attr_id) else {
            continue;
        };
        let pair = (mutation.entity_id, attribute.etype.clone());
        if !entity_types.contains(&pair) {
            entity_types.push(pair);
        }
    }
    let mut missing = Vec::new();
    for (entity_id, etype) in entity_types {
        let etype_attributes = catalog
            .attributes
            .iter()
            .filter(|attribute| attribute.etype == etype)
            .collect::<Vec<_>>();
        if !etype_attributes.iter().any(|attribute| attribute.required) {
            continue;
        }
        let attr_ids = etype_attributes
            .iter()
            .map(|attribute| attribute.id)
            .collect::<Vec<_>>();
        let alive: bool = transaction
            .query_one(
                "SELECT EXISTS(
                   SELECT 1 FROM triples
                   WHERE app_id = $1 AND entity_id = $2 AND attr_id = ANY($3)
                     AND value <> 'null'::jsonb
                 )",
                &[&app_id, &entity_id, &attr_ids],
            )
            .await?
            .get(0);
        if !alive {
            continue;
        }
        for attribute in etype_attributes
            .into_iter()
            .filter(|attribute| attribute.required || attribute.label == "id")
        {
            let present: bool = transaction
                .query_one(
                    "SELECT EXISTS(
                       SELECT 1 FROM triples
                       WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3
                         AND value <> 'null'::jsonb
                     )",
                    &[&app_id, &entity_id, &attribute.id],
                )
                .await?
                .get(0);
            if !present {
                missing.push(MissingRequiredAttribute {
                    entity_id,
                    etype: etype.clone(),
                    label: attribute.label.clone(),
                });
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ExecutionError::MissingRequired(missing))
    }
}

async fn validate_required_attribute_updates(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
) -> Result<(), ExecutionError> {
    let mut checked = HashSet::new();
    for update in steps.iter().filter_map(|step| match step {
        TransactionStep::UpdateAttr(update) if update.required == Some(true) => Some(update),
        _ => None,
    }) {
        if !checked.insert(update.id) {
            continue;
        }
        let Some(attribute) = catalog.by_id(update.id) else {
            continue;
        };
        let all_entities_have_attribute = transaction
            .query_one(
                "SELECT
                   count(DISTINCT entity_id) FILTER (
                     WHERE attr_id = $2 AND value IS NOT NULL AND value <> 'null'
                   ) = count(DISTINCT entity_id) FILTER (
                     WHERE attr_id IN (
                       SELECT id FROM attrs WHERE app_id = $1 AND etype = $3
                     ) AND value IS NOT NULL AND value <> 'null'
                   )
                 FROM triples WHERE app_id = $1",
                &[&app_id, &update.id, &attribute.etype],
            )
            .await?
            .get::<_, bool>(0);
        if !all_entities_have_attribute {
            return Err(ExecutionError::RequiredAttributeUpdateOnExistingEntities {
                attr_id: update.id,
                etype: attribute.etype.clone(),
                label: attribute.label.clone(),
            });
        }
    }
    Ok(())
}

// Pinned oracle: server/src/instant/db/model/triple.clj at 4e0a1bd8,
// insert-multi-new!. Prepared lookup IDs remain native Rust, while lookup
// materialization and final add-group writes share the oracle's ordered CTE.
const INSERT_ADD_GROUP: &str = "WITH input_lookup_refs AS (
       SELECT item.idx,
              (item.value->>'entity_id')::uuid AS entity_id,
              (item.value->>'attr_id')::uuid AS attr_id,
              item.value->'value' AS value
       FROM jsonb_array_elements($7::jsonb) WITH ORDINALITY AS item(value, idx)
     ), enhanced_lookup_refs AS (
       SELECT refs.idx, $1::uuid AS app_id, refs.entity_id,
              CASE WHEN attributes.id IS NOT NULL THEN attributes.id
                   ELSE (raise_exception_message(
                     'We could not find an attribute with id = ''' ||
                     refs.attr_id::text || ''''
                   )::text)::uuid
              END AS attr_id,
              refs.value, md5(refs.value::text) AS value_md5,
              attributes.cardinality = 'one' AS ea,
              attributes.value_type = 'ref' AS eav,
              CASE WHEN attributes.is_unique THEN true
                   ELSE raise_exception_message('attribute is not unique')
              END AS av,
              attributes.is_indexed AS ave,
              attributes.value_type = 'ref' AS vae,
              attributes.checked_data_type AS checked_data_type
       FROM input_lookup_refs AS refs
       LEFT JOIN attrs AS attributes
         ON attributes.is_unique
        AND (attributes.app_id = $1 OR attributes.app_id = $4)
        AND attributes.id = refs.attr_id
        AND attributes.deletion_marked_at IS NULL
     ), lookup_ref_inserts AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
          checked_data_type)
       SELECT app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
              checked_data_type
       FROM enhanced_lookup_refs
       WHERE NOT EXISTS (
         SELECT 1 FROM triples AS stored
         WHERE stored.app_id = enhanced_lookup_refs.app_id
           AND stored.attr_id = enhanced_lookup_refs.attr_id
           AND stored.av
           AND public.json_null_to_null(stored.value) =
               public.json_null_to_null(enhanced_lookup_refs.value)
       )
       ORDER BY app_id, entity_id, attr_id, value_md5
       ON CONFLICT (app_id, attr_id, public.json_null_to_null(value)) WHERE av
       DO UPDATE SET app_id = excluded.app_id
       RETURNING entity_id, attr_id, value_md5
     ), input_triples AS (
       SELECT item.idx,
              (item.value->>'entity_id')::uuid AS entity_id,
              (item.value->>'attr_id')::uuid AS attr_id,
              item.value->'value' AS value
       FROM jsonb_array_elements($2::jsonb) WITH ORDINALITY AS item(value, idx)
     ), enhanced_triples AS (
       SELECT input_triples.idx, $1::uuid AS app_id, input_triples.entity_id,
              CASE WHEN attributes.id IS NOT NULL THEN attributes.id
                   ELSE (raise_exception_message(
                     'We could not find an attribute with id = ''' ||
                     input_triples.attr_id::text || ''''
                   )::text)::uuid
              END AS attr_id,
              input_triples.value,
              md5(input_triples.value::text) AS value_md5,
              attributes.cardinality = 'one' AS ea,
              attributes.value_type = 'ref' AS eav,
              attributes.is_unique AS av, attributes.is_indexed AS ave,
              attributes.value_type = 'ref' AS vae,
              attributes.checked_data_type AS checked_data_type,
              attributes.label = 'id' AS is_id_attr
       FROM input_triples
       LEFT JOIN attrs AS attributes
         ON (attributes.app_id = $1 OR attributes.app_id = $4)
        AND attributes.id = input_triples.attr_id
        AND attributes.deletion_marked_at IS NULL
     ), ea_triples_distinct AS (
       SELECT DISTINCT ON (entity_id, attr_id) *
       FROM enhanced_triples
       WHERE ea
       ORDER BY entity_id DESC, attr_id DESC, idx DESC
     ), remaining_triples_distinct AS (
       SELECT DISTINCT ON (entity_id, attr_id, value_md5) *
       FROM enhanced_triples
       WHERE NOT ea
       ORDER BY entity_id, attr_id, value_md5, idx
     ), ea_non_id_inserts AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
          checked_data_type)
       SELECT app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
              checked_data_type
       FROM ea_triples_distinct
       WHERE NOT is_id_attr
       ORDER BY app_id, entity_id, attr_id, value_md5
       ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
       DO UPDATE SET value = excluded.value, value_md5 = excluded.value_md5,
                     created_at = CASE WHEN $5 THEN current_unix_timestamp_ms()
                                       ELSE triples.created_at END
       WHERE $5 OR triples.value_md5 IS DISTINCT FROM excluded.value_md5
       RETURNING entity_id, attr_id, value_md5
     ), remaining_inserts AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
          checked_data_type)
       SELECT app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
              checked_data_type
       FROM remaining_triples_distinct
       ORDER BY app_id, entity_id, attr_id, value_md5
       ON CONFLICT (app_id, entity_id, attr_id, value_md5) DO NOTHING
       RETURNING entity_id, attr_id, value_md5
     ), changed_entities AS (
       SELECT entity_id FROM ea_non_id_inserts
       UNION ALL
       SELECT entity_id FROM remaining_inserts
     ), ea_id_inserts AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
          checked_data_type)
       SELECT app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
              checked_data_type
       FROM ea_triples_distinct
       WHERE is_id_attr
       ORDER BY app_id, entity_id, attr_id, value_md5
       ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
       DO UPDATE SET value = excluded.value, value_md5 = excluded.value_md5,
                     created_at = CASE WHEN $5 THEN current_unix_timestamp_ms()
                                       ELSE triples.created_at END
       WHERE $5 OR EXISTS (
         SELECT 1 FROM changed_entities
         WHERE changed_entities.entity_id = triples.entity_id
       )
       RETURNING entity_id, attr_id, value_md5
     ), requested_writes AS (
       SELECT entity_id, attr_id FROM ea_non_id_inserts
       UNION ALL
       SELECT entity_id, attr_id FROM remaining_inserts
       UNION ALL
       SELECT entity_id, attr_id FROM ea_id_inserts
       UNION ALL
       SELECT entity_id, attr_id FROM lookup_ref_inserts
     ), indexed_null_candidates AS (
       SELECT item.idx, (item.value->>'entity_id')::uuid AS entity_id,
              (item.value->>'attr_id')::uuid AS attr_id,
              (item.value->>'trigger_id_attr_id')::uuid AS trigger_id_attr_id
       FROM jsonb_array_elements($6::jsonb) WITH ORDINALITY AS item(value, idx)
     ), indexed_null_inserts AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
          checked_data_type)
       SELECT $1, candidates.entity_id, attributes.id, 'null'::jsonb,
              md5(('null'::jsonb)::text), attributes.cardinality = 'one',
              false, attributes.is_unique, attributes.is_indexed, false,
              attributes.checked_data_type
       FROM indexed_null_candidates AS candidates
       JOIN attrs AS attributes
         ON (attributes.app_id = $1 OR attributes.app_id = $4)
        AND attributes.id = candidates.attr_id
        AND attributes.deletion_marked_at IS NULL
       WHERE (candidates.trigger_id_attr_id IS NULL OR EXISTS (
                SELECT 1 FROM ea_id_inserts
                WHERE ea_id_inserts.entity_id = candidates.entity_id
                  AND ea_id_inserts.attr_id = candidates.trigger_id_attr_id
              ))
         AND NOT EXISTS (
               SELECT 1 FROM triples
               WHERE triples.app_id = $1
                 AND triples.entity_id = candidates.entity_id
                 AND triples.attr_id = candidates.attr_id
             )
         AND NOT EXISTS (
               SELECT 1 FROM requested_writes
               WHERE requested_writes.entity_id = candidates.entity_id
                 AND requested_writes.attr_id = candidates.attr_id
             )
       ORDER BY $1, candidates.entity_id, attributes.id,
                md5(('null'::jsonb)::text)
       ON CONFLICT (app_id, entity_id, attr_id, value_md5) DO NOTHING
       RETURNING entity_id, attr_id, value_md5
     ), inferred_type_updates AS (
       SELECT (item.value->>'attr_id')::uuid AS attr_id,
              (item.value->>'mask')::integer::bit(32) AS mask
       FROM jsonb_array_elements($3::jsonb) AS item(value)
     ), attr_inferred_types AS (
       UPDATE attrs
       SET inferred_types = coalesce(attrs.inferred_types, 0::bit(32)) |
                            inferred_type_updates.mask
       FROM inferred_type_updates
       WHERE attrs.app_id = $1
         AND attrs.id = inferred_type_updates.attr_id
         AND attrs.inferred_types IS DISTINCT FROM
             (coalesce(attrs.inferred_types, 0::bit(32)) | inferred_type_updates.mask)
       RETURNING attrs.id
     ), inserted AS (
       SELECT 0 AS kind, candidates.idx, writes.entity_id, writes.attr_id
       FROM ea_non_id_inserts AS writes
       JOIN ea_triples_distinct AS candidates
         ON candidates.entity_id = writes.entity_id
        AND candidates.attr_id = writes.attr_id
        AND candidates.value_md5 = writes.value_md5
       UNION ALL
       SELECT 0 AS kind, candidates.idx, writes.entity_id, writes.attr_id
       FROM remaining_inserts AS writes
       JOIN remaining_triples_distinct AS candidates
         ON candidates.entity_id = writes.entity_id
        AND candidates.attr_id = writes.attr_id
        AND candidates.value_md5 = writes.value_md5
       UNION ALL
       SELECT 0 AS kind, candidates.idx, writes.entity_id, writes.attr_id
       FROM ea_id_inserts AS writes
       JOIN ea_triples_distinct AS candidates
         ON candidates.entity_id = writes.entity_id
        AND candidates.attr_id = writes.attr_id
        AND candidates.value_md5 = writes.value_md5
       UNION ALL
       SELECT 1 AS kind, candidates.idx, writes.entity_id, writes.attr_id
       FROM indexed_null_inserts AS writes
       JOIN indexed_null_candidates AS candidates
        ON candidates.entity_id = writes.entity_id
        AND candidates.attr_id = writes.attr_id
     )
     SELECT inserted.entity_id, inserted.attr_id
     FROM inserted
     CROSS JOIN (SELECT count(*) FROM attr_inferred_types) AS inferred_types_applied
     ORDER BY inserted.kind, inserted.idx";

const INSERT_EA: &str = "INSERT INTO triples
     (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
     VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), $5, $6, $7, $8, $9, $10::text::checked_data_type)
     ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
      DO UPDATE SET value = excluded.value, value_md5 = excluded.value_md5,
                    eav = excluded.eav, av = excluded.av, ave = excluded.ave,
                    vae = excluded.vae, checked_data_type = excluded.checked_data_type
      WHERE triples.value_md5 IS DISTINCT FROM excluded.value_md5
      RETURNING entity_id, attr_id";

const INSERT_EA_OVERWRITE_TIME: &str = "INSERT INTO triples
     (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
     VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), $5, $6, $7, $8, $9, $10::text::checked_data_type)
     ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
      DO UPDATE SET value = excluded.value, value_md5 = excluded.value_md5,
                    eav = excluded.eav, av = excluded.av, ave = excluded.ave,
                    vae = excluded.vae, checked_data_type = excluded.checked_data_type,
                    created_at = current_unix_timestamp_ms()
      RETURNING entity_id, attr_id";

const INSERT_EA_DEEP_MERGE: &str = "INSERT INTO triples
     (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
     VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), $5, $6, $7, $8, $9, $10::text::checked_data_type)
     ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
      DO UPDATE SET value = public.jsonb_deep_merge(triples.value, excluded.value),
                    value_md5 = md5(public.jsonb_deep_merge(triples.value, excluded.value)::text)
      RETURNING entity_id, attr_id";

const INSERT_EA_DEEP_MERGE_OVERWRITE_TIME: &str = "INSERT INTO triples
     (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
     VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), $5, $6, $7, $8, $9, $10::text::checked_data_type)
     ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
      DO UPDATE SET value = public.jsonb_deep_merge(triples.value, excluded.value),
                    value_md5 = md5(public.jsonb_deep_merge(triples.value, excluded.value)::text),
                    created_at = current_unix_timestamp_ms()
      RETURNING entity_id, attr_id";

const INSERT_EA_DEEP_MERGE_GROUPS: &str = "WITH input_groups AS (
       SELECT item.idx,
              (item.value->>'entity_id')::uuid AS entity_id,
              (item.value->>'attr_id')::uuid AS attr_id,
              item.value->'patches' AS patches
       FROM jsonb_array_elements($2::jsonb) WITH ORDINALITY AS item(value, idx)
     ), applied_groups AS (
       SELECT input_groups.idx, input_groups.entity_id, input_groups.attr_id,
              public.jsonb_deep_merge_many(existing.value, input_groups.patches) AS value
       FROM input_groups
       LEFT JOIN triples AS existing
         ON existing.app_id = $1
        AND existing.entity_id = input_groups.entity_id
        AND existing.attr_id = input_groups.attr_id
        AND existing.ea
     ), candidates AS (
       SELECT $1::uuid AS app_id, applied_groups.entity_id, attributes.id AS attr_id,
              applied_groups.value, md5(applied_groups.value::text) AS value_md5,
              true AS ea, false AS eav, attributes.is_unique AS av,
              attributes.is_indexed AS ave, false AS vae,
              attributes.checked_data_type AS checked_data_type
       FROM applied_groups
       JOIN attrs AS attributes
         ON (attributes.app_id = $1 OR attributes.app_id = $3)
        AND attributes.id = applied_groups.attr_id
        AND attributes.deletion_marked_at IS NULL
     ), inserted AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
       SELECT app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
              checked_data_type
       FROM candidates
       ORDER BY app_id, entity_id, attr_id, value_md5
       ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
       DO UPDATE SET value = excluded.value, value_md5 = excluded.value_md5
       RETURNING entity_id, attr_id
     )
     SELECT entity_id, attr_id FROM inserted ORDER BY entity_id, attr_id";

const INSERT_EA_DEEP_MERGE_GROUPS_OVERWRITE_TIME: &str = "WITH input_groups AS (
       SELECT item.idx,
              (item.value->>'entity_id')::uuid AS entity_id,
              (item.value->>'attr_id')::uuid AS attr_id,
              item.value->'patches' AS patches
       FROM jsonb_array_elements($2::jsonb) WITH ORDINALITY AS item(value, idx)
     ), applied_groups AS (
       SELECT input_groups.idx, input_groups.entity_id, input_groups.attr_id,
              public.jsonb_deep_merge_many(existing.value, input_groups.patches) AS value
       FROM input_groups
       LEFT JOIN triples AS existing
         ON existing.app_id = $1
        AND existing.entity_id = input_groups.entity_id
        AND existing.attr_id = input_groups.attr_id
        AND existing.ea
     ), candidates AS (
       SELECT $1::uuid AS app_id, applied_groups.entity_id, attributes.id AS attr_id,
              applied_groups.value, md5(applied_groups.value::text) AS value_md5,
              true AS ea, false AS eav, attributes.is_unique AS av,
              attributes.is_indexed AS ave, false AS vae,
              attributes.checked_data_type AS checked_data_type
       FROM applied_groups
       JOIN attrs AS attributes
         ON (attributes.app_id = $1 OR attributes.app_id = $3)
        AND attributes.id = applied_groups.attr_id
        AND attributes.deletion_marked_at IS NULL
     ), inserted AS (
       INSERT INTO triples
         (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
       SELECT app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
              checked_data_type
       FROM candidates
       ORDER BY app_id, entity_id, attr_id, value_md5
       ON CONFLICT (app_id, entity_id, attr_id) WHERE ea
       DO UPDATE SET value = excluded.value, value_md5 = excluded.value_md5,
                     created_at = current_unix_timestamp_ms()
       RETURNING entity_id, attr_id
     )
     SELECT entity_id, attr_id FROM inserted ORDER BY entity_id, attr_id";

const INSERT_MANY: &str = "INSERT INTO triples
     (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae, checked_data_type)
     VALUES ($1, $2, $3, $4::jsonb, md5(($4::jsonb)::text), $5, $6, $7, $8, $9, $10::text::checked_data_type)
     ON CONFLICT (app_id, entity_id, attr_id, value_md5) DO NOTHING
     RETURNING entity_id, attr_id";

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;

    use instant_instaql::{
        AttributeCatalog, AttributeValueType, Cardinality, system_attribute_catalog,
    };
    use serde_json::{Value, json};

    use super::{
        ExecutionError, INSERT_ADD_GROUP, IndexedNullCandidate, ResolvedAddTriple,
        ResolvedDeepMergePatches, TransactionOptions, add_group_payload,
        apply_persisted_schema_step, catalog_with_restored_attributes,
        collapse_cardinality_one_adds, deep_merge_group_payload, group_deep_merge_patches,
        indexed_null_payload, inferred_type_payload, lookup_payload, optimistic_catalog,
        restored_catalog_attribute, validate_attribute_definition,
        validate_reference_parameter_limit, validate_reserved_identity_names,
        validate_system_entity_writes,
    };
    use crate::{
        DeleteEntityStep, EntityLookup, LookupRef, PreparedLookup, TransactionMode,
        TransactionOperation, TransactionStep, TripleOptions, TripleStep, compile_transaction,
    };

    #[test]
    fn source_proven_add_triple_limit_preserves_the_upstream_failure() {
        const ORACLE: &str =
            "instantdb/instant@45d3ffbb server/test/instant/db/transaction_test.clj:5252-5267";
        assert!(ORACLE.contains("45d3ffbb"));
        let add = TransactionStep::AddTriple(TripleStep {
            entity: EntityLookup::Id(uuid::Uuid::nil()),
            etype: None,
            attr_id: uuid::Uuid::nil(),
            reverse_etype: None,
            value: Value::Null,
            options: TripleOptions::default(),
        });
        assert!(validate_reference_parameter_limit(&vec![add.clone(); 99_999]).is_ok());
        assert!(matches!(
            validate_reference_parameter_limit(&vec![add; 100_000]),
            Err(ExecutionError::ParameterLimitExceeded)
        ));
        let non_add = TransactionStep::DeleteAttr(uuid::Uuid::nil());
        assert!(validate_reference_parameter_limit(&vec![non_add; 65_536]).is_ok());
    }

    #[test]
    fn optimistic_catalog_replaces_update_maps_before_triple_execution() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/transaction.clj:652-666";
        assert!(ORACLE.contains("4e0a1bd8"));
        let attr_id = uuid::Uuid::from_u128(1);
        let forward_id = uuid::Uuid::from_u128(2);
        let reverse_id = uuid::Uuid::from_u128(3);
        let steps = compile_transaction(
            &json!([
              ["add-attr", {
                "id": attr_id,
                "forward-identity": [forward_id, "users", "friend"],
                "reverse-identity": [reverse_id, "users", "friendOf"],
                "value-type": "ref",
                "cardinality": "many",
                "unique?": false,
                "index?": false
              }],
              ["update-attr", {
                "id": attr_id,
                "forward-identity": [forward_id, "users", "friends"],
                "index?": true
              }]
            ]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let catalog = optimistic_catalog(&AttributeCatalog::default(), &steps);
        let attribute = catalog.by_id(attr_id).unwrap();
        assert_eq!(attribute.label, "friends");
        assert!(attribute.indexed);
        assert_eq!(attribute.value_type, AttributeValueType::Blob);
        assert_eq!(attribute.cardinality, Cardinality::One);
        assert_eq!(attribute.reverse_label, None);

        let deleted = compile_transaction(&json!([["delete-attr", attr_id]]), &catalog).unwrap();
        assert!(
            optimistic_catalog(&catalog, &deleted)
                .by_id(attr_id)
                .is_none()
        );
    }

    #[test]
    fn persisted_write_catalog_applies_schema_groups_without_losing_omitted_fields() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/db/transaction.clj:580-614 server/src/instant/model/triple.clj:344-370";
        assert!(ORACLE.contains("4e0a1bd8"));
        let attr_id = uuid::Uuid::from_u128(30);
        let forward_id = uuid::Uuid::from_u128(31);
        let reverse_id = uuid::Uuid::from_u128(32);
        let added = compile_transaction(
            &json!([["add-attr", {
                "id": attr_id,
                "forward-identity": [forward_id, "users", "friends"],
                "reverse-identity": [reverse_id, "users", "friendOf"],
                "value-type": "ref",
                "cardinality": "many",
                "unique?": false,
                "index?": false
            }]]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let mut persisted = AttributeCatalog::default();
        apply_persisted_schema_step(&mut persisted, &added[0]);

        let update = compile_transaction(
            &json!([["update-attr", {"id": attr_id, "index?": true}]]),
            &persisted,
        )
        .unwrap();
        apply_persisted_schema_step(&mut persisted, &update[0]);

        let attribute = persisted.by_id(attr_id).unwrap();
        assert_eq!(attribute.value_type, AttributeValueType::Ref);
        assert_eq!(attribute.cardinality, Cardinality::Many);
        assert_eq!(attribute.reverse_label.as_deref(), Some("friendOf"));
        assert!(attribute.indexed);
    }

    #[test]
    fn restored_attributes_reenter_the_same_request_catalog_with_persisted_metadata() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/db/transaction.clj:580-614 server/src/instant/db/model/attr.clj:657-725 server/src/instant/db/model/triple.clj:570-600";
        assert!(ORACLE.contains("4e0a1bd8"));
        let attr_id = uuid::Uuid::from_u128(40);
        let restored = compile_transaction(
            &json!([["add-attr", {
                "id": attr_id,
                "forward-identity": [uuid::Uuid::from_u128(41), "users", "friends"],
                "reverse-identity": [uuid::Uuid::from_u128(42), "users", "friendOf"],
                "value-type": "ref",
                "cardinality": "many",
                "unique?": false,
                "index?": true,
                "required?": true
            }]]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let TransactionStep::AddAttr(definition) = &restored[0] else {
            panic!("expected add-attr")
        };
        let restored = restored_catalog_attribute(definition.attribute.clone());
        let catalog = catalog_with_restored_attributes(&AttributeCatalog::default(), [restored]);

        let attribute = catalog.by_id(attr_id).unwrap();
        assert_eq!(attribute.value_type, AttributeValueType::Ref);
        assert_eq!(attribute.cardinality, Cardinality::Many);
        assert_eq!(attribute.reverse_label.as_deref(), Some("friendOf"));
        assert!(
            !attribute.indexed,
            "restore always disables indexing upstream"
        );
        assert!(
            !attribute.required,
            "restore always disables required upstream"
        );
    }

    #[test]
    fn cardinality_one_add_groups_keep_only_the_last_entity_attribute_value() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:570-620";
        assert!(ORACLE.contains("4e0a1bd8"));
        let one_attr = uuid::Uuid::from_u128(50);
        let many_attr = uuid::Uuid::from_u128(52);
        let entity_id = uuid::Uuid::from_u128(54);
        let definitions = compile_transaction(
            &json!([
                ["add-attr", {
                    "id": one_attr,
                    "forward-identity": [uuid::Uuid::from_u128(51), "users", "handle"],
                    "value-type": "blob", "cardinality": "one",
                    "unique?": true, "index?": false
                }],
                ["add-attr", {
                    "id": many_attr,
                    "forward-identity": [uuid::Uuid::from_u128(53), "users", "tags"],
                    "value-type": "blob", "cardinality": "many",
                    "unique?": false, "index?": false
                }]
            ]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let mut catalog = AttributeCatalog::default();
        for definition in &definitions {
            apply_persisted_schema_step(&mut catalog, definition);
        }
        let group = compile_transaction(
            &json!([
                ["add-triple", entity_id, one_attr, "superseded"],
                ["add-triple", entity_id, many_attr, "first"],
                ["add-triple", entity_id, one_attr, "final"],
                ["add-triple", entity_id, many_attr, "second"]
            ]),
            &catalog,
        )
        .unwrap();

        let selected = collapse_cardinality_one_adds(&catalog, &group).unwrap();
        let values = selected
            .iter()
            .map(|step| match step {
                TransactionStep::AddTriple(triple) => triple.value.clone(),
                _ => panic!("expected add-triple"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![json!("first"), json!("final"), json!("second")]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One source-pinned contract covers payload and CTE ordering.
    fn add_groups_use_one_ordered_insert_pipeline_and_group_inferred_types() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:812-1156 insert-multi-new!";
        assert!(ORACLE.contains("4e0a1bd8"));

        let first_entity = uuid::Uuid::from_u128(61);
        let second_entity = uuid::Uuid::from_u128(62);
        let first_attr = uuid::Uuid::from_u128(63);
        let second_attr = uuid::Uuid::from_u128(64);
        let payload = add_group_payload(&[
            ResolvedAddTriple {
                entity_id: first_entity,
                attr_id: first_attr,
                value: json!("one"),
            },
            ResolvedAddTriple {
                entity_id: second_entity,
                attr_id: second_attr,
                value: json!({"nested": true}),
            },
        ]);
        assert_eq!(
            payload,
            json!([
                {"entity_id": first_entity, "attr_id": first_attr, "value": "one"},
                {"entity_id": second_entity, "attr_id": second_attr, "value": {"nested": true}}
            ])
        );
        let lookups = lookup_payload(&[PreparedLookup {
            entity_id: first_entity,
            lookup: LookupRef {
                attr_id: first_attr,
                value: json!("new-lookup"),
            },
            operation: TransactionOperation::AddTriple,
        }]);
        assert_eq!(
            lookups,
            json!([{
                "entity_id": first_entity,
                "attr_id": first_attr,
                "value": "new-lookup"
            }])
        );
        let indexed_nulls = indexed_null_payload(&[
            IndexedNullCandidate {
                entity: first_entity,
                attr: second_attr,
                trigger_id_attr_id: Some(first_attr),
            },
            IndexedNullCandidate {
                entity: second_entity,
                attr: first_attr,
                trigger_id_attr_id: None,
            },
        ]);
        assert_eq!(
            indexed_nulls,
            json!([
                {
                    "entity_id": first_entity,
                    "attr_id": second_attr,
                    "trigger_id_attr_id": first_attr
                },
                {
                    "entity_id": second_entity,
                    "attr_id": first_attr,
                    "trigger_id_attr_id": null
                }
            ])
        );

        let group = vec![
            TransactionStep::AddTriple(crate::TripleStep {
                entity: EntityLookup::Id(first_entity),
                attr_id: first_attr,
                value: json!("string"),
                etype: None,
                reverse_etype: None,
                options: crate::TripleOptions::default(),
            }),
            TransactionStep::AddTriple(crate::TripleStep {
                entity: EntityLookup::Id(first_entity),
                attr_id: first_attr,
                value: json!({"object": true}),
                etype: None,
                reverse_etype: None,
                options: crate::TripleOptions::default(),
            }),
            TransactionStep::AddTriple(crate::TripleStep {
                entity: EntityLookup::Id(second_entity),
                attr_id: second_attr,
                value: Value::Null,
                etype: None,
                reverse_etype: None,
                options: crate::TripleOptions::default(),
            }),
        ];
        assert_eq!(
            inferred_type_payload(&group),
            json!([{"attr_id": first_attr, "mask": 10}])
        );

        for stage in [
            "input_lookup_refs",
            "enhanced_lookup_refs",
            "lookup_ref_inserts",
            "input_triples",
            "ea_triples_distinct",
            "remaining_triples_distinct",
            "ea_non_id_inserts",
            "changed_entities",
            "ea_id_inserts",
            "requested_writes",
            "indexed_null_candidates",
            "indexed_null_inserts",
            "attr_inferred_types",
        ] {
            assert!(
                INSERT_ADD_GROUP.contains(stage),
                "missing oracle CTE {stage}"
            );
        }
        assert!(INSERT_ADD_GROUP.contains("ORDER BY app_id, entity_id, attr_id, value_md5"));
        assert!(INSERT_ADD_GROUP.contains("ORDER BY inserted.kind, inserted.idx"));
        assert!(INSERT_ADD_GROUP.contains("LEFT JOIN attrs AS attributes"));
        assert!(INSERT_ADD_GROUP.contains("raise_exception_message"));
        assert!(INSERT_ADD_GROUP.contains("FROM requested_writes"));
        assert!(INSERT_ADD_GROUP.contains("FROM indexed_null_inserts AS writes"));
        assert!(INSERT_ADD_GROUP.contains("DO UPDATE SET app_id = excluded.app_id"));
        assert!(INSERT_ADD_GROUP.contains("jsonb_array_elements($7::jsonb)"));
    }

    #[test]
    fn deep_merge_groups_collect_ordered_patches_per_entity_and_attribute() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:249-405";
        assert!(ORACLE.contains("4e0a1bd8"));
        let attr_id = uuid::Uuid::from_u128(60);
        let definition = compile_transaction(
            &json!([["add-attr", {
                "id": attr_id,
                "forward-identity": [uuid::Uuid::from_u128(61), "users", "profile"],
                "value-type": "blob", "cardinality": "one",
                "unique?": true, "index?": true
            }]]),
            &AttributeCatalog::default(),
        )
        .unwrap();
        let mut catalog = AttributeCatalog::default();
        apply_persisted_schema_step(&mut catalog, &definition[0]);
        let first_entity = uuid::Uuid::from_u128(62);
        let second_entity = uuid::Uuid::from_u128(63);
        let steps = compile_transaction(
            &json!([
                ["deep-merge-triple", first_entity, attr_id, {"nested": {"a": 1}}],
                ["deep-merge-triple", second_entity, attr_id, {"other": true}],
                ["deep-merge-triple", first_entity, attr_id, {"nested": {"b": 2}}]
            ]),
            &catalog,
        )
        .unwrap();

        let grouped = group_deep_merge_patches(&catalog, &steps).unwrap();
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].entity, EntityLookup::Id(first_entity));
        assert_eq!(
            grouped[0].patches,
            vec![json!({"nested": {"a": 1}}), json!({"nested": {"b": 2}})]
        );
        assert_eq!(grouped[1].entity, EntityLookup::Id(second_entity));
        assert_eq!(grouped[1].patches, vec![json!({"other": true})]);
    }

    #[test]
    fn set_based_deep_merge_payload_retains_every_final_group_in_order() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/db/model/triple.clj:249-405";
        assert!(ORACLE.contains("4e0a1bd8"));
        let first_entity = uuid::Uuid::from_u128(70);
        let second_entity = uuid::Uuid::from_u128(71);
        let first_attr = uuid::Uuid::from_u128(72);
        let second_attr = uuid::Uuid::from_u128(73);
        let payload = deep_merge_group_payload(&[
            ResolvedDeepMergePatches {
                entity_id: first_entity,
                attr_id: first_attr,
                patches: vec![json!({"a": 1}), json!({"b": 2})],
            },
            ResolvedDeepMergePatches {
                entity_id: second_entity,
                attr_id: second_attr,
                patches: vec![json!({"c": 3})],
            },
        ]);

        assert_eq!(
            payload,
            json!([
                {"entity_id": first_entity, "attr_id": first_attr,
                 "patches": [{"a": 1}, {"b": 2}]},
                {"entity_id": second_entity, "attr_id": second_attr,
                 "patches": [{"c": 3}]}
            ])
        );
    }

    #[test]
    fn dynamic_reserved_names_cover_forward_and_reverse_identities() {
        let steps = compile_transaction(
            &json!([["add-attr", {
                "id": uuid::Uuid::from_u128(20),
                "forward-identity": [uuid::Uuid::from_u128(21), "users", "futureField"],
                "reverse-identity": [uuid::Uuid::from_u128(22), "profiles", "futureOwner"],
                "value-type": "ref",
                "cardinality": "one",
                "unique?": false,
                "index?": false
            }]]),
            &AttributeCatalog::default(),
        )
        .unwrap();

        validate_reserved_identity_names(&steps, &HashSet::new()).unwrap();
        for (etype, label) in [
            ("users".to_owned(), "futureField".to_owned()),
            ("profiles".to_owned(), "futureOwner".to_owned()),
        ] {
            let error = validate_reserved_identity_names(
                &steps,
                &HashSet::from([(etype.clone(), label.clone())]),
            )
            .unwrap_err();
            assert!(matches!(
                error,
                ExecutionError::ReservedIdentity {
                    etype: actual_etype,
                    label: actual_label,
                } if actual_etype == etype && actual_label == label
            ));
        }
    }

    #[test]
    fn schema_validation_reserves_catalog_names_and_dollar_namespaces() {
        let cases = [
            (
                json!({
                  "id": uuid::Uuid::from_u128(10),
                  "forward-identity": [uuid::Uuid::from_u128(11), "$users", "email"],
                  "value-type": "blob", "cardinality": "one",
                  "unique?": false, "index?": false
                }),
                "identity",
            ),
            (
                json!({
                  "id": uuid::Uuid::from_u128(12),
                  "forward-identity": [uuid::Uuid::from_u128(13), "$private", "id"],
                  "value-type": "blob", "cardinality": "one",
                  "unique?": false, "index?": false
                }),
                "namespace",
            ),
            (
                json!({
                  "id": uuid::Uuid::from_u128(14),
                  "forward-identity": [uuid::Uuid::from_u128(15), "$users", "custom"],
                  "value-type": "blob", "cardinality": "one",
                  "unique?": false, "index?": false, "required?": true
                }),
                "required",
            ),
        ];
        for (definition, expected) in cases {
            let steps = compile_transaction(
                &json!([["add-attr", definition]]),
                &AttributeCatalog::default(),
            )
            .unwrap();
            let TransactionStep::AddAttr(definition) = &steps[0] else {
                panic!("expected add-attr")
            };
            let error = validate_attribute_definition(definition, false).unwrap_err();
            match expected {
                "identity" => assert!(matches!(error, ExecutionError::ReservedIdentity { .. })),
                "namespace" => {
                    assert!(matches!(error, ExecutionError::ReservedSystemNamespace(_)));
                }
                "required" => {
                    assert!(matches!(error, ExecutionError::RequiredSystemAttribute(_)));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn protected_system_fields_match_admin_and_internal_coordinator_rules() {
        let catalog = system_attribute_catalog().unwrap();
        let entity_id = uuid::Uuid::new_v4();
        let path = catalog.forward("$files", "path").unwrap().id;
        let size = catalog.forward("$files", "size").unwrap().id;
        let email = catalog.forward("$users", "email").unwrap().id;
        let path_step = compile_transaction(
            &json!([["add-triple", entity_id, path, "renamed.png"]]),
            &catalog,
        )
        .unwrap();
        validate_system_entity_writes(&catalog, &path_step, TransactionOptions::default()).unwrap();

        let stream_path_step = compile_transaction(
            &json!([["add-triple", entity_id, path, "$stream/stream-id/0"]]),
            &catalog,
        )
        .unwrap();
        assert!(matches!(
            validate_system_entity_writes(
                &catalog,
                &stream_path_step,
                TransactionOptions::default()
            ),
            Err(ExecutionError::ProtectedStreamFilePath)
        ));
        validate_system_entity_writes(
            &catalog,
            &stream_path_step,
            TransactionOptions {
                allow_file_metadata_updates: true,
                ..TransactionOptions::default()
            },
        )
        .unwrap();

        let size_step =
            compile_transaction(&json!([["add-triple", entity_id, size, 10]]), &catalog).unwrap();
        assert!(matches!(
            validate_system_entity_writes(
                &catalog,
                &size_step,
                TransactionOptions {
                    admin: true,
                    ..TransactionOptions::default()
                }
            ),
            Err(ExecutionError::ProtectedSystemField { .. })
        ));
        validate_system_entity_writes(
            &catalog,
            &size_step,
            TransactionOptions {
                allow_file_metadata_updates: true,
                ..TransactionOptions::default()
            },
        )
        .unwrap();

        let user_step = compile_transaction(
            &json!([["add-triple", entity_id, email, "person@example.com"]]),
            &catalog,
        )
        .unwrap();
        assert!(
            validate_system_entity_writes(&catalog, &user_step, TransactionOptions::default())
                .is_err()
        );
        validate_system_entity_writes(
            &catalog,
            &user_step,
            TransactionOptions {
                admin: true,
                ..TransactionOptions::default()
            },
        )
        .unwrap();

        let delete_user = vec![TransactionStep::DeleteEntity(DeleteEntityStep {
            entity: EntityLookup::Id(entity_id),
            etype: Some("$users".to_owned()),
        })];
        assert!(matches!(
            validate_system_entity_writes(&catalog, &delete_user, TransactionOptions::default()),
            Err(ExecutionError::ProtectedSystemEntity(_))
        ));
    }

    #[derive(Default)]
    struct CountingExistence {
        entities: Cell<usize>,
        lookups: Cell<usize>,
    }

    impl super::ModeExistence for CountingExistence {
        fn entity_exists(
            &self,
            _app_id: uuid::Uuid,
            _catalog: &AttributeCatalog,
            _entity_id: uuid::Uuid,
            _etype: &str,
        ) -> impl std::future::Future<Output = Result<bool, ExecutionError>> {
            self.entities.set(self.entities.get() + 1);
            std::future::ready(Ok(false))
        }

        fn lookup_exists(
            &self,
            _app_id: uuid::Uuid,
            _catalog: &AttributeCatalog,
            _lookup: &LookupRef,
        ) -> impl std::future::Future<Output = Result<bool, ExecutionError>> {
            self.lookups.set(self.lookups.get() + 1);
            std::future::ready(Ok(false))
        }
    }

    fn create_step(entity: EntityLookup, attr_id: uuid::Uuid) -> TransactionStep {
        TransactionStep::AddTriple(TripleStep {
            entity,
            etype: Some("$users".to_owned()),
            attr_id,
            reverse_etype: None,
            value: json!("value"),
            options: TripleOptions {
                mode: Some(TransactionMode::Create),
            },
        })
    }

    #[tokio::test]
    async fn mode_validation_checks_one_existence_per_entity_and_lookup() {
        let catalog = system_attribute_catalog().unwrap();
        let user = catalog.forward("$users", "id").unwrap().id;
        let email = catalog.forward("$users", "email").unwrap().id;
        let user_type = catalog.forward("$users", "type").unwrap().id;
        let entity = uuid::Uuid::new_v4();
        let lookup = LookupRef {
            attr_id: email,
            value: json!("mode-validation@example.com"),
        };
        // A create sends one step per attribute and every step reads the same
        // pre-write snapshot, so existence is read once per entity and lookup.
        let steps = vec![
            create_step(EntityLookup::Id(entity), user),
            create_step(EntityLookup::Id(entity), email),
            create_step(EntityLookup::Id(entity), user_type),
            create_step(EntityLookup::Lookup(lookup.clone()), email),
            create_step(EntityLookup::Lookup(lookup), user_type),
        ];
        let existence = CountingExistence::default();
        super::validate_modes_with(&existence, uuid::Uuid::new_v4(), &catalog, &steps)
            .await
            .unwrap();
        assert_eq!(existence.entities.get(), 1);
        assert_eq!(existence.lookups.get(), 1);
    }
}
