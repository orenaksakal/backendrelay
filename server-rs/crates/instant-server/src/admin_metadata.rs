use std::sync::Arc;

use instant_datalog::DataType;
use instant_http::{
    AdminAttribute, AdminMetadataBackend, AdminRequestContext, AdminSchemaResponse, ApiError,
    ApiErrorType, AttributeCatalogKind, SoftDeletedAttributesResponse, attributes_to_admin_schema,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, AttributeWireMetadata, Cardinality,
    DeleteAction, SYSTEM_CATALOG_APP_ID,
};
use serde_json::Value;
use tokio_postgres::types::{Json, Type};
use tokio_postgres::{Client, Row};
use uuid::Uuid;

use crate::admin_data::{AdminDataAuthorizer, AdminDataScope, require_admin};

pub(crate) const ATTRIBUTE_COLUMNS: &str = "
    id, app_id, value_type, cardinality, is_unique, is_indexed,
    is_required, forward_ident, etype, label, reverse_ident,
    reverse_etype, reverse_label,
    CASE WHEN inferred_types IS NULL THEN NULL ELSE inferred_types::bigint END,
    on_delete::text, on_delete_reverse::text, checked_data_type::text,
    checking_data_type, indexing, setting_unique, metadata, deletion_marked_at";

pub(crate) async fn load_attribute_catalog(
    client: &Client,
    app_id: Uuid,
) -> Result<AttributeCatalog, ApiError> {
    let statement = format!(
        "SELECT {ATTRIBUTE_COLUMNS}
         FROM attrs
         WHERE (app_id = $1 OR app_id = $2)
           AND deletion_marked_at IS NULL
         ORDER BY id"
    );
    let rows = client
        .query_typed(
            &statement,
            &[(&app_id, Type::UUID), (&SYSTEM_CATALOG_APP_ID, Type::UUID)],
        )
        .await
        .map_err(super::admin_data::internal_error)?;
    let attributes = rows
        .iter()
        .map(row_to_attribute)
        .collect::<Result<Vec<_>, _>>()?;
    attributes_to_catalog(&attributes)
}

pub(crate) fn attributes_to_catalog(
    attributes: &[AdminAttribute],
) -> Result<AttributeCatalog, ApiError> {
    let mut catalog = AttributeCatalog::default();
    for attribute in attributes {
        let value_type = match attribute.value_type.as_str() {
            "blob" => AttributeValueType::Blob,
            "ref" => AttributeValueType::Ref,
            other => return Err(invalid_catalog_value("value type", other)),
        };
        let cardinality = match attribute.cardinality.as_str() {
            "one" => Cardinality::One,
            "many" => Cardinality::Many,
            other => return Err(invalid_catalog_value("cardinality", other)),
        };
        let checked_data_type = match attribute.checked_data_type.as_deref() {
            None => None,
            Some("string") => Some(DataType::String),
            Some("number") => Some(DataType::Number),
            Some("boolean") => Some(DataType::Boolean),
            Some("date") => Some(DataType::Date),
            Some(other) => return Err(invalid_catalog_value("checked data type", other)),
        };
        catalog.attributes.push(Attribute {
            id: attribute.id,
            wire: Some(AttributeWireMetadata {
                forward_identity_id: attribute.forward_identity.0,
                reverse_identity_id: attribute
                    .reverse_identity
                    .as_ref()
                    .map(|identity| identity.0),
                inferred_types: attribute.inferred_types.clone(),
            }),
            etype: attribute.forward_identity.1.clone(),
            label: attribute.forward_identity.2.clone(),
            value_type,
            cardinality,
            unique: attribute.unique,
            setting_unique: attribute.setting_unique,
            indexed: attribute.indexed,
            required: attribute.required,
            checked_data_type,
            indexing: attribute.indexing,
            checking_data_type: attribute.checking_data_type,
            reverse_etype: attribute
                .reverse_identity
                .as_ref()
                .map(|identity| identity.1.clone()),
            reverse_label: attribute
                .reverse_identity
                .as_ref()
                .map(|identity| identity.2.clone()),
            on_delete: attribute.on_delete.as_ref().map(|_| DeleteAction::Cascade),
            on_delete_reverse: attribute
                .on_delete_reverse
                .as_ref()
                .map(|_| DeleteAction::Cascade),
        });
    }
    Ok(catalog)
}

fn invalid_catalog_value(kind: &str, value: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ValidationFailed,
        format!("Unknown attribute {kind} `{value}`"),
    )
}

/// PostgreSQL-backed admin schema and soft-deletion metadata service.
pub struct AdminMetadataService<A> {
    client: crate::postgres_pool::PostgresClientSource,
    authorizer: Arc<A>,
}

impl<A> AdminMetadataService<A>
where
    A: AdminDataAuthorizer,
{
    #[must_use]
    pub fn new(client: Client, authorizer: Arc<A>) -> Self {
        Self {
            client: crate::postgres_pool::PostgresClientSource::fixed(client),
            authorizer,
        }
    }

    #[must_use]
    pub fn from_pool(pool: crate::PostgresPool, authorizer: Arc<A>) -> Self {
        Self {
            client: crate::postgres_pool::PostgresClientSource::pooled(pool),
            authorizer,
        }
    }

    async fn authorize(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<Uuid, instant_http::ApiError> {
        require_admin(&context)?;
        let app_id = context.app_id;
        self.authorizer.authorize(context, scope).await?;
        Ok(app_id)
    }
}

impl<A> AdminMetadataBackend for AdminMetadataService<A>
where
    A: AdminDataAuthorizer,
{
    async fn schema(
        &self,
        context: AdminRequestContext,
    ) -> Result<AdminSchemaResponse, instant_http::ApiError> {
        let app_id = self.authorize(context, AdminDataScope::AppsRead).await?;
        let client = self
            .client
            .acquire()
            .await
            .map_err(super::admin_data::internal_error)?;
        let statement = format!(
            "SELECT {ATTRIBUTE_COLUMNS}
             FROM attrs
             WHERE (app_id = $1 OR app_id = $2)
               AND deletion_marked_at IS NULL
             ORDER BY id"
        );
        let rows = client
            .query(&statement, &[&app_id, &SYSTEM_CATALOG_APP_ID])
            .await
            .map_err(super::admin_data::internal_error)?;
        let attributes = rows
            .iter()
            .map(row_to_attribute)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AdminSchemaResponse {
            schema: attributes_to_admin_schema(&attributes),
        })
    }

    async fn soft_deleted_attributes(
        &self,
        context: AdminRequestContext,
    ) -> Result<SoftDeletedAttributesResponse, instant_http::ApiError> {
        let app_id = self.authorize(context, AdminDataScope::Read).await?;
        let client = self
            .client
            .acquire()
            .await
            .map_err(super::admin_data::internal_error)?;
        let statement = format!(
            "SELECT {ATTRIBUTE_COLUMNS}
             FROM attrs
             WHERE app_id = $1 AND deletion_marked_at IS NOT NULL
             ORDER BY id"
        );
        let rows = client
            .query(&statement, &[&app_id])
            .await
            .map_err(super::admin_data::internal_error)?;
        let attrs = rows
            .iter()
            .map(row_to_attribute)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SoftDeletedAttributesResponse {
            attrs,
            grace_period_days: 2,
        })
    }
}

pub(crate) fn row_to_attribute(row: &Row) -> Result<AdminAttribute, instant_http::ApiError> {
    macro_rules! column {
        ($index:expr) => {
            row.try_get($index)
                .map_err(super::admin_data::internal_error)?
        };
    }
    let id: Uuid = column!(0);
    let app_id: Uuid = column!(1);
    let value_type: String = column!(2);
    let cardinality: String = column!(3);
    let unique: bool = column!(4);
    let indexed: bool = column!(5);
    let required: bool = column!(6);
    let forward_id: Uuid = column!(7);
    let forward_etype: String = column!(8);
    let forward_label: String = column!(9);
    let reverse_id: Option<Uuid> = column!(10);
    let reverse_etype: Option<String> = column!(11);
    let reverse_label: Option<String> = column!(12);
    let inferred_bits: Option<i64> = column!(13);
    let on_delete: Option<String> = column!(14);
    let on_delete_reverse: Option<String> = column!(15);
    let checked_data_type: Option<String> = column!(16);
    let checking_data_type: Option<bool> = column!(17);
    let indexing: Option<bool> = column!(18);
    let setting_unique: Option<bool> = column!(19);
    let metadata: Option<Json<Value>> = column!(20);
    let deletion_marked_at = column!(21);
    Ok(AdminAttribute {
        id,
        forward_identity: (forward_id, forward_etype, forward_label),
        reverse_identity: reverse_id.map(|id| {
            (
                id,
                reverse_etype.unwrap_or_default(),
                reverse_label.unwrap_or_default(),
            )
        }),
        indexed,
        unique,
        required,
        cardinality,
        value_type,
        inferred_types: inferred_bits.map(inferred_type_names),
        catalog: if app_id == SYSTEM_CATALOG_APP_ID {
            AttributeCatalogKind::System
        } else {
            AttributeCatalogKind::User
        },
        on_delete,
        on_delete_reverse,
        checked_data_type,
        checking_data_type: checking_data_type.unwrap_or(false),
        indexing: indexing.unwrap_or(false),
        setting_unique: setting_unique.unwrap_or(false),
        metadata: metadata.map(|Json(value)| value),
        deletion_marked_at,
    })
}

fn inferred_type_names(bits: i64) -> Vec<String> {
    [(1, "number"), (2, "string"), (4, "boolean"), (8, "json")]
        .into_iter()
        .filter(|(mask, _)| bits & mask != 0)
        .map(|(_, name)| name.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inferred_type_bits_match_the_jvm_mapping() {
        assert_eq!(
            inferred_type_names(1 | 4 | 8),
            ["number", "boolean", "json"]
        );
        assert!(inferred_type_names(0).is_empty());
    }

    #[test]
    fn admin_attributes_convert_without_losing_query_semantics() {
        let attribute = AdminAttribute {
            id: Uuid::new_v4(),
            forward_identity: (Uuid::new_v4(), "posts".to_owned(), "author".to_owned()),
            reverse_identity: Some((Uuid::new_v4(), "users".to_owned(), "posts".to_owned())),
            indexed: true,
            unique: true,
            required: true,
            cardinality: "many".to_owned(),
            value_type: "ref".to_owned(),
            inferred_types: Some(vec!["number".to_owned(), "json".to_owned()]),
            catalog: AttributeCatalogKind::User,
            on_delete: Some("cascade".to_owned()),
            on_delete_reverse: Some("cascade".to_owned()),
            checked_data_type: Some("date".to_owned()),
            checking_data_type: true,
            indexing: true,
            setting_unique: true,
            metadata: None,
            deletion_marked_at: None,
        };

        let catalog = attributes_to_catalog(std::slice::from_ref(&attribute)).unwrap();
        let converted = catalog.by_id(attribute.id).unwrap();
        assert_eq!(converted.value_type, AttributeValueType::Ref);
        assert_eq!(converted.cardinality, Cardinality::Many);
        assert_eq!(converted.checked_data_type, Some(DataType::Date));
        assert_eq!(converted.reverse_etype.as_deref(), Some("users"));
        assert_eq!(converted.reverse_label.as_deref(), Some("posts"));
        assert_eq!(
            converted.forward_identity_id(),
            attribute.forward_identity.0
        );
        assert_eq!(
            converted.reverse_identity_id(),
            attribute
                .reverse_identity
                .as_ref()
                .map(|identity| identity.0)
        );
        assert_eq!(
            converted.inferred_types(),
            attribute.inferred_types.as_deref()
        );
        assert_eq!(converted.on_delete, Some(DeleteAction::Cascade));
        assert_eq!(converted.on_delete_reverse, Some(DeleteAction::Cascade));
        assert!(converted.setting_unique);
        assert!(converted.indexing);
        assert!(converted.checking_data_type);

        let mut invalid = attribute;
        invalid.value_type = "mystery".to_owned();
        let error = attributes_to_catalog(&[invalid]).unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ValidationFailed);
    }
}
