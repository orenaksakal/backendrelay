use std::future::Future;
use std::sync::{Arc, Mutex};

use instant_http::{AdminActor, AdminMetadataBackend, AdminRequestContext, ApiError, ApiErrorType};
use instant_instaql::{AttributeCatalog, SYSTEM_CATALOG_APP_ID};
use instant_server::{AdminDataAuthorizer, AdminDataScope, AdminMetadataService};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

#[derive(Default)]
struct TestAuthorizer {
    scopes: Mutex<Vec<AdminDataScope>>,
}

impl AdminDataAuthorizer for TestAuthorizer {
    fn authorize(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        self.scopes.lock().unwrap().push(scope);
        std::future::ready(if context.bearer_token.as_deref() == Some("admin-secret") {
            Ok(AttributeCatalog::default())
        } else {
            Err(ApiError::new(ApiErrorType::PermissionDenied, "Invalid admin token").unauthorized())
        })
    }

    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(AttributeCatalog::default()))
    }
}

fn context(app_id: Uuid) -> AdminRequestContext {
    AdminRequestContext {
        app_id,
        bearer_token: Some("admin-secret".to_owned()),
        actor: AdminActor::Admin,
        client_ip: None,
    }
}

async fn install_schema(client: &Client) {
    client
        .batch_execute(
            "CREATE TEMP TABLE attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL,
               is_required boolean NOT NULL DEFAULT false,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               inferred_types bit(32),
               on_delete text,
               on_delete_reverse text,
               checked_data_type text,
               checking_data_type boolean,
               indexing boolean,
               setting_unique boolean,
               metadata jsonb,
               deletion_marked_at timestamptz
             );",
        )
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_attribute(
    client: &Client,
    id: Uuid,
    app_id: Uuid,
    etype: &str,
    label: &str,
    value_type: &str,
    checked_type: Option<&str>,
    metadata: Option<serde_json::Value>,
    deleted: bool,
) {
    client
        .execute(
            "INSERT INTO attrs (
               id, app_id, value_type, cardinality, is_unique, is_indexed,
               forward_ident, etype, label, checked_data_type, metadata,
               deletion_marked_at
             ) VALUES ($1, $2, $3, 'one', false, true, $4, $5, $6, $7,
                       $8, CASE WHEN $9 THEN now() ELSE NULL END)",
            &[
                &id,
                &app_id,
                &value_type,
                &Uuid::new_v4(),
                &etype,
                &label,
                &checked_type,
                &metadata.map(tokio_postgres::types::Json),
                &deleted,
            ],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn admin_metadata_loads_full_schema_and_soft_deleted_rows() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_metadata_loads_full_schema_and_soft_deleted_rows",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    let active_id = Uuid::new_v4();
    insert_attribute(
        &client,
        active_id,
        app_id,
        "posts",
        "title",
        "blob",
        Some("string"),
        Some(serde_json::json!({"source": "test"})),
        false,
    )
    .await;
    client
        .execute(
            "UPDATE attrs SET inferred_types = B'00000000000000000000000000000011'
             WHERE id = $1",
            &[&active_id],
        )
        .await
        .unwrap();
    insert_attribute(
        &client,
        Uuid::new_v4(),
        app_id,
        "deleted_posts",
        "old-title",
        "blob",
        None,
        None,
        true,
    )
    .await;
    insert_attribute(
        &client,
        Uuid::new_v4(),
        SYSTEM_CATALOG_APP_ID,
        "$oauthClients",
        "id",
        "blob",
        None,
        None,
        false,
    )
    .await;

    let authorizer = Arc::new(TestAuthorizer::default());
    let service = AdminMetadataService::new(client, Arc::clone(&authorizer));
    let schema = service.schema(context(app_id)).await.unwrap().schema;
    let title = &schema.blobs["posts"]["title"];
    assert_eq!(title.checked_data_type.as_deref(), Some("string"));
    assert_eq!(
        title.inferred_types.as_deref(),
        Some(["number".to_owned(), "string".to_owned()].as_slice())
    );
    assert_eq!(title.metadata, Some(serde_json::json!({"source": "test"})));
    assert!(!schema.blobs.contains_key("$oauthClients"));

    let deleted = service
        .soft_deleted_attributes(context(app_id))
        .await
        .unwrap();
    assert_eq!(deleted.grace_period_days, 2);
    assert_eq!(deleted.attrs.len(), 1);
    assert_eq!(deleted.attrs[0].forward_identity.1, "deleted_posts");
    assert!(deleted.attrs[0].deletion_marked_at.is_some());
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::AppsRead, AdminDataScope::Read]
    );

    drop(service);
    connection_task.await.unwrap().unwrap();
}
