//! Typed transaction preprocessing for `InstantDB`'s public write protocol.

use instant_instaql::{Attribute, AttributeValueType, Cardinality, DeleteAction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

mod admin;
mod execute;
mod parse;
mod prepare;
mod resolve;

pub use admin::{compile_admin_transaction, compile_admin_transaction_with_order};
pub use execute::{
    AppStatusWriteCheck, DEFAULT_TRANSACTION_STATEMENT_TIMEOUT, ExecutionError,
    MissingRequiredAttribute, Mutation, SchemaMutation, TransactionOptions, TransactionReceipt,
    WalEntityLog, configure_transaction_statement_timeout, execute_prepared_transaction_in,
    execute_prepared_transaction_in_with_app_status_write_check, execute_transaction,
    execute_transaction_in, execute_transaction_in_with_app_status_write_check,
    execute_transaction_with_app_status_write_check, optimistic_catalog,
    validate_reserved_identity_names, validate_transaction_modes,
};
pub use parse::{
    CompiledTransaction, compile_transaction, compile_transaction_all_errors,
    compile_transaction_with_order, group_transaction_steps,
};
pub use prepare::{
    PreparedLookup, PreparedTransaction, prepare_transaction_lookups,
    prepare_transaction_lookups_with_source_order,
};
pub use resolve::{resolve_delete_etypes, resolve_delete_lookups};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PathSegment {
    Index(usize),
    Field(String),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("transaction validation failed at {path:?}: {message}")]
pub struct TransactionError {
    pub path: Vec<PathSegment>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LookupRef {
    pub attr_id: Uuid,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum EntityLookup {
    Id(Uuid),
    Lookup(LookupRef),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionMode {
    Create,
    Update,
    Upsert,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TripleOptions {
    pub mode: Option<TransactionMode>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TripleStep {
    pub entity: EntityLookup,
    pub etype: Option<String>,
    pub attr_id: Uuid,
    pub reverse_etype: Option<String>,
    pub value: Value,
    pub options: TripleOptions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeleteEntityStep {
    pub entity: EntityLookup,
    pub etype: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuleParamsStep {
    pub entity: EntityLookup,
    pub etype: Option<String>,
    pub params: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Identity {
    pub id: Uuid,
    pub etype: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttributeDefinition {
    pub attribute: Attribute,
    pub forward_identity: Identity,
    pub reverse_identity: Option<Identity>,
    pub on_delete: Option<DeleteAction>,
    pub on_delete_reverse: Option<DeleteAction>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttributeUpdate {
    pub id: Uuid,
    pub forward_identity: Option<Identity>,
    pub reverse_identity: Option<Option<Identity>>,
    pub unique: Option<bool>,
    pub indexed: Option<bool>,
    pub cardinality: Option<Cardinality>,
    pub required: Option<bool>,
    pub on_delete: Option<Option<DeleteAction>>,
    pub on_delete_reverse: Option<Option<DeleteAction>>,
}

fn optimistic_attribute_from_update(update: &AttributeUpdate) -> Attribute {
    let (etype, label) = update.forward_identity.as_ref().map_or_else(
        || (String::new(), String::new()),
        |identity| (identity.etype.clone(), identity.label.clone()),
    );
    let reverse = update.reverse_identity.as_ref().and_then(Option::as_ref);
    Attribute {
        id: update.id,
        wire: update.forward_identity.as_ref().map(|forward| {
            instant_instaql::AttributeWireMetadata {
                forward_identity_id: forward.id,
                reverse_identity_id: reverse.map(|identity| identity.id),
                inferred_types: None,
            }
        }),
        etype,
        label,
        value_type: AttributeValueType::Blob,
        cardinality: update.cardinality.unwrap_or(Cardinality::One),
        unique: update.unique.unwrap_or(false),
        setting_unique: false,
        indexed: update.indexed.unwrap_or(false),
        required: update.required.unwrap_or(false),
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: reverse.map(|identity| identity.etype.clone()),
        reverse_label: reverse.map(|identity| identity.label.clone()),
        on_delete: update.on_delete.flatten(),
        on_delete_reverse: update.on_delete_reverse.flatten(),
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", content = "step", rename_all = "kebab-case")]
pub enum TransactionStep {
    AddTriple(TripleStep),
    DeepMergeTriple(TripleStep),
    RetractTriple(TripleStep),
    AddAttr(AttributeDefinition),
    UpdateAttr(AttributeUpdate),
    DeleteAttr(Uuid),
    RestoreAttr(Uuid),
    DeleteEntity(DeleteEntityStep),
    RuleParams(RuleParamsStep),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransactionOperation {
    AddTriple,
    DeepMergeTriple,
    RetractTriple,
    AddAttr,
    UpdateAttr,
    DeleteAttr,
    RestoreAttr,
    DeleteEntity,
    RuleParams,
}

impl TransactionStep {
    #[must_use]
    pub const fn operation(&self) -> TransactionOperation {
        match self {
            Self::AddTriple(_) => TransactionOperation::AddTriple,
            Self::DeepMergeTriple(_) => TransactionOperation::DeepMergeTriple,
            Self::RetractTriple(_) => TransactionOperation::RetractTriple,
            Self::AddAttr(_) => TransactionOperation::AddAttr,
            Self::UpdateAttr(_) => TransactionOperation::UpdateAttr,
            Self::DeleteAttr(_) => TransactionOperation::DeleteAttr,
            Self::RestoreAttr(_) => TransactionOperation::RestoreAttr,
            Self::DeleteEntity(_) => TransactionOperation::DeleteEntity,
            Self::RuleParams(_) => TransactionOperation::RuleParams,
        }
    }
}

impl AttributeDefinition {
    #[must_use]
    pub fn query_attribute(&self) -> &Attribute {
        &self.attribute
    }
}
