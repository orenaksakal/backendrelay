use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::admin::{query_params, request_context};
use crate::{AdminRequestContext, ApiError};

/// Origin of a persisted attribute definition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AttributeCatalogKind {
    User,
    System,
}

/// Persisted attribute shape exposed by admin schema APIs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors the persisted JVM attribute flags.
pub struct AdminAttribute {
    pub id: Uuid,
    #[serde(rename = "forward-identity")]
    pub forward_identity: (Uuid, String, String),
    #[serde(rename = "reverse-identity", skip_serializing_if = "Option::is_none")]
    pub reverse_identity: Option<(Uuid, String, String)>,
    #[serde(rename = "index?")]
    pub indexed: bool,
    #[serde(rename = "unique?")]
    pub unique: bool,
    #[serde(rename = "required?")]
    pub required: bool,
    pub cardinality: String,
    #[serde(rename = "value-type")]
    pub value_type: String,
    #[serde(rename = "inferred-types")]
    pub inferred_types: Option<Vec<String>>,
    pub catalog: AttributeCatalogKind,
    #[serde(rename = "checked-data-type", skip_serializing_if = "Option::is_none")]
    pub checked_data_type: Option<String>,
    #[serde(rename = "on-delete", skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(rename = "on-delete-reverse", skip_serializing_if = "Option::is_none")]
    pub on_delete_reverse: Option<String>,
    #[serde(rename = "checking-data-type?", skip_serializing_if = "is_false")]
    pub checking_data_type: bool,
    #[serde(rename = "indexing?", skip_serializing_if = "is_false")]
    pub indexing: bool,
    #[serde(rename = "setting-unique?", skip_serializing_if = "is_false")]
    pub setting_unique: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(rename = "deletion-marked-at", skip_serializing_if = "Option::is_none")]
    pub deletion_marked_at: Option<DateTime<Utc>>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // Serde skip predicates receive references.
fn is_false(value: &bool) -> bool {
    !value
}

/// Reference and scalar maps returned by `/admin/schema`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AdminSchema {
    pub refs: BTreeMap<String, AdminAttribute>,
    pub blobs: BTreeMap<String, BTreeMap<String, AdminAttribute>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AdminSchemaResponse {
    pub schema: AdminSchema,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SoftDeletedAttributesResponse {
    pub attrs: Vec<AdminAttribute>,
    #[serde(rename = "grace-period-days")]
    pub grace_period_days: u32,
}

/// Metadata reads behind the two admin schema endpoints.
pub trait AdminMetadataBackend: Send + Sync + 'static {
    fn schema(
        &self,
        context: AdminRequestContext,
    ) -> impl Future<Output = Result<AdminSchemaResponse, ApiError>> + Send;

    fn soft_deleted_attributes(
        &self,
        context: AdminRequestContext,
    ) -> impl Future<Output = Result<SoftDeletedAttributesResponse, ApiError>> + Send;
}

/// Builds `/admin/schema` and `/admin/soft_deleted_attrs`.
pub fn admin_metadata_router<B>(backend: Arc<B>) -> Router
where
    B: AdminMetadataBackend,
{
    crate::body_limit::limit_body(
        Router::new()
            .route("/admin/schema", get(schema::<B>))
            .route(
                "/admin/soft_deleted_attrs",
                get(soft_deleted_attributes::<B>),
            ),
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Admin request body exceeds the 1 MiB limit",
    )
    .with_state(backend)
}

async fn schema<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<AdminSchemaResponse>, ApiError>
where
    B: AdminMetadataBackend,
{
    backend.schema(context(&headers, params)?).await.map(Json)
}

async fn soft_deleted_attributes<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<SoftDeletedAttributesResponse>, ApiError>
where
    B: AdminMetadataBackend,
{
    backend
        .soft_deleted_attributes(context(&headers, params)?)
        .await
        .map(Json)
}

fn context(
    headers: &HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<AdminRequestContext, ApiError> {
    let params = query_params(params)?;
    request_context(headers, &params)
}

/// Converts full persisted attributes into the exact JVM schema grouping.
#[must_use]
pub fn attributes_to_admin_schema(attributes: &[AdminAttribute]) -> AdminSchema {
    let mut schema = AdminSchema::default();
    for source in attributes.iter().filter(|attribute| visible(attribute)) {
        let mut attribute = source.clone();
        let (_, forward_etype, forward_label) = &attribute.forward_identity;
        if forward_etype == "$files" && forward_label == "url" {
            attribute.required = true;
        }
        if attribute.value_type == "ref" {
            if let Some((_, reverse_etype, reverse_label)) = &attribute.reverse_identity {
                let key =
                    format!("{forward_etype}-{forward_label}-{reverse_etype}-{reverse_label}");
                schema.refs.insert(key, attribute);
            }
        } else if attribute.value_type == "blob" {
            schema
                .blobs
                .entry(forward_etype.clone())
                .or_default()
                .insert(forward_label.clone(), attribute);
        }
    }
    schema
}

fn visible(attribute: &AdminAttribute) -> bool {
    let (_, etype, label) = &attribute.forward_identity;
    if attribute.catalog == AttributeCatalogKind::System
        && !matches!(etype.as_str(), "$users" | "$files")
    {
        return false;
    }
    !(etype == "$files"
        && matches!(
            label.as_str(),
            "content-type" | "content-disposition" | "size" | "location-id" | "key-version"
        ))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;

    fn attribute(
        id: u128,
        etype: &str,
        label: &str,
        value_type: &str,
        catalog: AttributeCatalogKind,
    ) -> AdminAttribute {
        AdminAttribute {
            id: Uuid::from_u128(id),
            forward_identity: (
                Uuid::from_u128(id + 100),
                etype.to_owned(),
                label.to_owned(),
            ),
            reverse_identity: None,
            indexed: false,
            unique: label == "id",
            required: false,
            cardinality: "one".to_owned(),
            value_type: value_type.to_owned(),
            inferred_types: None,
            catalog,
            checked_data_type: None,
            on_delete: None,
            on_delete_reverse: None,
            checking_data_type: false,
            indexing: false,
            setting_unique: false,
            metadata: None,
            deletion_marked_at: None,
        }
    }

    #[test]
    fn schema_projection_matches_hidden_system_and_reference_contracts() {
        let mut owner = attribute(3, "posts", "owner", "ref", AttributeCatalogKind::User);
        owner.reverse_identity = Some((Uuid::from_u128(103), "users".into(), "posts".into()));
        let schema = attributes_to_admin_schema(&[
            attribute(1, "posts", "id", "blob", AttributeCatalogKind::User),
            owner.clone(),
            attribute(
                4,
                "$oauthClients",
                "id",
                "blob",
                AttributeCatalogKind::System,
            ),
            attribute(
                5,
                "$files",
                "location-id",
                "blob",
                AttributeCatalogKind::System,
            ),
            attribute(6, "$files", "url", "blob", AttributeCatalogKind::System),
        ]);
        assert!(schema.blobs["posts"].contains_key("id"));
        assert_eq!(schema.refs["posts-owner-users-posts"], owner);
        assert!(!schema.blobs.contains_key("$oauthClients"));
        assert!(!schema.blobs["$files"].contains_key("location-id"));
        assert!(schema.blobs["$files"]["url"].required);
    }

    #[derive(Default)]
    struct FakeBackend;

    impl AdminMetadataBackend for FakeBackend {
        fn schema(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<AdminSchemaResponse, ApiError>> + Send {
            std::future::ready(Ok(AdminSchemaResponse {
                schema: AdminSchema::default(),
            }))
        }

        fn soft_deleted_attributes(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<SoftDeletedAttributesResponse, ApiError>> + Send {
            std::future::ready(Ok(SoftDeletedAttributesResponse {
                attrs: Vec::new(),
                grace_period_days: 2,
            }))
        }
    }

    #[tokio::test]
    async fn metadata_routes_preserve_response_keys() {
        let app_id = Uuid::new_v4();
        for (path, expected) in [
            ("/admin/schema", r#"{"schema":{"refs":{},"blobs":{}}}"#),
            (
                "/admin/soft_deleted_attrs",
                r#"{"attrs":[],"grace-period-days":2}"#,
            ),
        ] {
            let response = admin_metadata_router(Arc::new(FakeBackend))
                .oneshot(
                    Request::builder()
                        .uri(format!("{path}?app_id={app_id}"))
                        .header(header::AUTHORIZATION, "Bearer admin-secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn metadata_routes_reject_missing_admin_authorization_with_jvm_error() {
        let app_id = Uuid::new_v4();
        let response = admin_metadata_router(Arc::new(FakeBackend))
            .oneshot(
                Request::builder()
                    .uri(format!("/admin/schema?app_id={app_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"type":"param-missing","message":"Missing parameter: [headers authorization]"}"#
        );
    }
}
