use std::collections::{HashMap, HashSet};

use instant_instaql::{
    AttributeValueType, Cardinality, DeleteAction, SYSTEM_CATALOG_APP_ID, SystemCatalogAttribute,
    SystemCatalogError, system_catalog_attributes,
};
use thiserror::Error;
use tokio_postgres::{Client, IsolationLevel, Row, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemCatalogMigrationOutcome {
    pub created_attribute_ids: Vec<Uuid>,
    pub mismatched_attribute_ids: Vec<Uuid>,
    pub extra_attribute_ids: Vec<Uuid>,
}

#[derive(Debug, Error)]
pub enum SystemCatalogMigrationError {
    #[error(transparent)]
    Catalog(#[from] SystemCatalogError),
    #[error(
        "system catalog identity {etype}.{label} is owned by unexpected attribute {actual}; automatic migration cannot safely repair incompatible catalog rows, so restore the expected identity manually before retrying `instant-server migrate up`"
    )]
    IdentityConflict {
        etype: String,
        label: String,
        actual: Uuid,
    },
    #[error(
        "system catalog contains incompatible attributes {mismatched_attribute_ids:?}; automatic migration preserves existing catalog rows and cannot safely repair them, so restore the expected definitions manually before retrying `instant-server migrate up`"
    )]
    IncompatibleAttributes { mismatched_attribute_ids: Vec<Uuid> },
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
}

#[derive(Debug, Error)]
pub enum SystemCatalogVerificationError {
    #[error(transparent)]
    Catalog(#[from] SystemCatalogError),
    #[error(
        "system catalog is incomplete (missing attributes: {missing_attribute_ids:?}, missing identities: {missing_identity_ids:?}); run `instant-server migrate up` explicitly"
    )]
    Missing {
        missing_attribute_ids: Vec<Uuid>,
        missing_identity_ids: Vec<Uuid>,
    },
    #[error(
        "system catalog contains incompatible rows (mismatched attributes: {mismatched_attribute_ids:?}, mismatched identities: {mismatched_identity_ids:?}); automatic migration cannot safely repair existing catalog rows, so restore the expected definitions manually before starting the server"
    )]
    Incompatible {
        mismatched_attribute_ids: Vec<Uuid>,
        mismatched_identity_ids: Vec<Uuid>,
    },
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
}

/// Verifies the deterministic system catalog from a repeatable-read,
/// read-only snapshot. Serving must never create or reconcile catalog rows;
/// operators perform that mutation explicitly through `migrate up`.
///
/// # Errors
///
/// Returns when required attributes or identities are absent/incompatible, or
/// when catalog generation or PostgreSQL verification fails.
pub async fn verify_system_catalog(
    client: &mut Client,
) -> Result<(), SystemCatalogVerificationError> {
    let expected = system_catalog_attributes()?;
    let transaction = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await?;
    let rows = transaction
        .query(
            "SELECT id, value_type, cardinality, is_unique, is_indexed,
                    coalesce(is_required, false), forward_ident, etype, label,
                    reverse_ident, reverse_etype, reverse_label,
                    on_delete::text, on_delete_reverse::text, checked_data_type::text,
                    coalesce(setting_unique, false), coalesce(indexing, false),
                    coalesce(checking_data_type, false)
             FROM attrs WHERE app_id = $1 AND deletion_marked_at IS NULL",
            &[&SYSTEM_CATALOG_APP_ID],
        )
        .await?;
    let by_name = rows
        .iter()
        .map(|row| ((row.get::<_, String>(7), row.get::<_, String>(8)), row))
        .collect::<HashMap<_, _>>();
    let identity_rows = transaction
        .query(
            "SELECT id, attr_id, etype, label
             FROM idents WHERE app_id = $1",
            &[&SYSTEM_CATALOG_APP_ID],
        )
        .await?;
    let identities_by_name = identity_rows
        .iter()
        .map(|row| ((row.get::<_, String>(2), row.get::<_, String>(3)), row))
        .collect::<HashMap<_, _>>();

    let mut missing_attribute_ids = Vec::new();
    let mut mismatched_attribute_ids = Vec::new();
    let mut missing_identity_ids = Vec::new();
    let mut mismatched_identity_ids = Vec::new();
    for definition in &expected {
        let attribute = &definition.attribute;
        match by_name.get(&(attribute.etype.clone(), attribute.label.clone())) {
            Some(row) if attribute_matches(row, definition) => {}
            Some(_) => mismatched_attribute_ids.push(attribute.id),
            None => missing_attribute_ids.push(attribute.id),
        }
        for identity in std::iter::once(&definition.forward_identity)
            .chain(definition.reverse_identity.as_ref())
        {
            match identities_by_name.get(&(identity.etype.clone(), identity.label.clone())) {
                Some(row)
                    if row.get::<_, Uuid>(0) == identity.id
                        && row.get::<_, Uuid>(1) == attribute.id => {}
                Some(_) => mismatched_identity_ids.push(identity.id),
                None => missing_identity_ids.push(identity.id),
            }
        }
    }
    missing_attribute_ids.sort_unstable();
    mismatched_attribute_ids.sort_unstable();
    missing_identity_ids.sort_unstable();
    mismatched_identity_ids.sort_unstable();
    if !mismatched_attribute_ids.is_empty() || !mismatched_identity_ids.is_empty() {
        return Err(SystemCatalogVerificationError::Incompatible {
            mismatched_attribute_ids,
            mismatched_identity_ids,
        });
    }
    if !missing_attribute_ids.is_empty() || !missing_identity_ids.is_empty() {
        return Err(SystemCatalogVerificationError::Missing {
            missing_attribute_ids,
            missing_identity_ids,
        });
    }
    transaction.commit().await?;
    Ok(())
}

/// Adds missing deterministic system attributes and identities after SQL
/// migrations. Existing incompatible attributes are preserved and cause an
/// actionable failure instead of being reported as a successful migration.
///
/// # Errors
///
/// Returns when built-in catalog generation fails, PostgreSQL rejects a write,
/// or an existing identity name points at a different attribute.
pub async fn ensure_system_catalog(
    client: &mut Client,
) -> Result<SystemCatalogMigrationOutcome, SystemCatalogMigrationError> {
    let expected = system_catalog_attributes()?;
    let transaction = client.transaction().await?;
    transaction
        .batch_execute(
            "SET LOCAL instant.allow_system_catalog_app_attr_update = 'true';
             SET LOCAL instant.allow_system_catalog_app_ident_update = 'true';",
        )
        .await?;
    let rows = transaction
        .query(
            "SELECT id, value_type, cardinality, is_unique, is_indexed,
                    coalesce(is_required, false), forward_ident, etype, label,
                    reverse_ident, reverse_etype, reverse_label,
                    on_delete::text, on_delete_reverse::text, checked_data_type::text,
                    coalesce(setting_unique, false), coalesce(indexing, false),
                    coalesce(checking_data_type, false)
             FROM attrs WHERE app_id = $1 AND deletion_marked_at IS NULL",
            &[&SYSTEM_CATALOG_APP_ID],
        )
        .await?;
    let by_name = rows
        .iter()
        .map(|row| ((row.get::<_, String>(7), row.get::<_, String>(8)), row))
        .collect::<HashMap<_, _>>();
    let expected_ids = expected
        .iter()
        .map(|definition| definition.attribute.id)
        .collect::<HashSet<_>>();
    let mut outcome = SystemCatalogMigrationOutcome {
        extra_attribute_ids: rows
            .iter()
            .map(|row| row.get::<_, Uuid>(0))
            .filter(|id| !expected_ids.contains(id))
            .collect(),
        ..SystemCatalogMigrationOutcome::default()
    };

    for definition in &expected {
        let attribute = &definition.attribute;
        if let Some(row) = by_name.get(&(attribute.etype.clone(), attribute.label.clone())) {
            if !attribute_matches(row, definition) {
                outcome.mismatched_attribute_ids.push(attribute.id);
            }
        } else {
            insert_attribute(&transaction, definition).await?;
            outcome.created_attribute_ids.push(attribute.id);
        }
        if by_name
            .get(&(attribute.etype.clone(), attribute.label.clone()))
            .is_none_or(|row| row.get::<_, Uuid>(0) == attribute.id)
        {
            ensure_identity(
                &transaction,
                attribute.id,
                definition.forward_identity.id,
                &definition.forward_identity.etype,
                &definition.forward_identity.label,
            )
            .await?;
            if let Some(identity) = &definition.reverse_identity {
                ensure_identity(
                    &transaction,
                    attribute.id,
                    identity.id,
                    &identity.etype,
                    &identity.label,
                )
                .await?;
            }
        }
    }
    outcome.created_attribute_ids.sort_unstable();
    outcome.mismatched_attribute_ids.sort_unstable();
    outcome.extra_attribute_ids.sort_unstable();
    if !outcome.mismatched_attribute_ids.is_empty() {
        return Err(SystemCatalogMigrationError::IncompatibleAttributes {
            mismatched_attribute_ids: outcome.mismatched_attribute_ids,
        });
    }
    transaction.commit().await?;
    Ok(outcome)
}

fn attribute_matches(row: &Row, definition: &SystemCatalogAttribute) -> bool {
    let attribute = &definition.attribute;
    row.get::<_, Uuid>(0) == attribute.id
        && row.get::<_, String>(1) == value_type_name(attribute.value_type)
        && row.get::<_, String>(2) == cardinality_name(attribute.cardinality)
        && row.get::<_, bool>(3) == attribute.unique
        && row.get::<_, bool>(4) == attribute.indexed
        && row.get::<_, bool>(5) == attribute.required
        && row.get::<_, Uuid>(6) == definition.forward_identity.id
        && row.get::<_, Option<Uuid>>(9)
            == definition
                .reverse_identity
                .as_ref()
                .map(|identity| identity.id)
        && row.get::<_, Option<String>>(10) == attribute.reverse_etype
        && row.get::<_, Option<String>>(11) == attribute.reverse_label
        && row.get::<_, Option<String>>(12).as_deref() == delete_action_name(definition.on_delete)
        && row.get::<_, Option<String>>(13).as_deref()
            == delete_action_name(definition.on_delete_reverse)
        && row.get::<_, Option<String>>(14)
            == attribute
                .checked_data_type
                .map(|value| format!("{value:?}").to_lowercase())
        && row.get::<_, bool>(15) == attribute.setting_unique
        && row.get::<_, bool>(16) == attribute.indexing
        && row.get::<_, bool>(17) == attribute.checking_data_type
}

async fn insert_attribute(
    transaction: &Transaction<'_>,
    definition: &SystemCatalogAttribute,
) -> Result<(), tokio_postgres::Error> {
    let attribute = &definition.attribute;
    let reverse_ident = definition
        .reverse_identity
        .as_ref()
        .map(|identity| identity.id);
    let checked_data_type = attribute
        .checked_data_type
        .map(|value| format!("{value:?}").to_lowercase());
    let inferred_type = if attribute.label == "meta" {
        8_i32
    } else {
        2_i32
    };
    transaction
        .execute(
            "INSERT INTO attrs
             (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
              forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
              on_delete, on_delete_reverse, checked_data_type, setting_unique, indexing,
              checking_data_type, inferred_types)
             VALUES
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
              $14::text::attr_on_delete, $15::text::attr_on_delete,
              $16::text::checked_data_type, $17, $18, $19, $20::integer::bit(32))",
            &[
                &attribute.id,
                &SYSTEM_CATALOG_APP_ID,
                &value_type_name(attribute.value_type),
                &cardinality_name(attribute.cardinality),
                &attribute.unique,
                &attribute.indexed,
                &attribute.required,
                &definition.forward_identity.id,
                &attribute.etype,
                &attribute.label,
                &reverse_ident,
                &attribute.reverse_etype,
                &attribute.reverse_label,
                &delete_action_name(definition.on_delete),
                &delete_action_name(definition.on_delete_reverse),
                &checked_data_type,
                &attribute.setting_unique,
                &attribute.indexing,
                &attribute.checking_data_type,
                &inferred_type,
            ],
        )
        .await?;
    Ok(())
}

async fn ensure_identity(
    transaction: &Transaction<'_>,
    attr_id: Uuid,
    identity_id: Uuid,
    etype: &str,
    label: &str,
) -> Result<(), SystemCatalogMigrationError> {
    if transaction
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING",
            &[
                &identity_id,
                &SYSTEM_CATALOG_APP_ID,
                &attr_id,
                &etype,
                &label,
            ],
        )
        .await?
        == 1
    {
        return Ok(());
    }
    let row = transaction
        .query_one(
            "SELECT app_id, attr_id, etype, label FROM idents WHERE id = $1",
            &[&identity_id],
        )
        .await?;
    let actual = row.get::<_, Uuid>(1);
    if row.get::<_, Uuid>(0) != SYSTEM_CATALOG_APP_ID
        || actual != attr_id
        || row.get::<_, String>(2) != etype
        || row.get::<_, String>(3) != label
    {
        return Err(SystemCatalogMigrationError::IdentityConflict {
            etype: etype.to_owned(),
            label: label.to_owned(),
            actual,
        });
    }
    Ok(())
}

const fn value_type_name(value: AttributeValueType) -> &'static str {
    match value {
        AttributeValueType::Blob => "blob",
        AttributeValueType::Ref => "ref",
    }
}

const fn cardinality_name(value: Cardinality) -> &'static str {
    match value {
        Cardinality::One => "one",
        Cardinality::Many => "many",
    }
}

const fn delete_action_name(value: Option<DeleteAction>) -> Option<&'static str> {
    match value {
        Some(DeleteAction::Cascade) => Some("cascade"),
        None => None,
    }
}
