use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use instant_http::{
    AdminActor, AdminAuthBackend, AdminCreateTokenRequest, AdminMagicCodeRequest,
    AdminRequestContext, AdminSignInGuestRequest, AdminSignOutRequest, AdminUserSelector,
    AdminVerifyMagicCodeRequest, ApiError, ApiErrorType, RuntimeAuthBackend, SendMagicCodeRequest,
    VerifyMagicCodeRequest,
};
use instant_instaql::{
    AttributeCatalog, AttributeValueType, Cardinality, system_attribute_catalog,
};
use instant_server::{
    AdminAuthService, AdminDataAuthorizer, AdminDataScope, AllowAllSignups, MagicCodeDelivery,
    RuntimeAuthConfig, RuntimeAuthService, StaticRuntimeAuthPolicy,
};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Debug)]
struct TestAuthorizer;

impl AdminDataAuthorizer for TestAuthorizer {
    fn authorize(
        &self,
        context: AdminRequestContext,
        _scope: AdminDataScope,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(if context.bearer_token.as_deref() == Some("admin-secret") {
            Ok(system_attribute_catalog().unwrap())
        } else {
            Err(ApiError::new(ApiErrorType::PermissionDenied, "Invalid admin token").unauthorized())
        })
    }

    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(
            system_attribute_catalog()
                .map_err(|error| ApiError::new(ApiErrorType::Unknown, error.to_string())),
        )
    }
}

#[derive(Debug, Default)]
struct TestDelivery {
    messages: Mutex<Vec<(Uuid, String, String, u64)>>,
}

impl MagicCodeDelivery for TestDelivery {
    fn deliver(
        &self,
        app_id: Uuid,
        email: &str,
        code: &str,
        expiry_minutes: u64,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        self.messages.lock().unwrap().push((
            app_id,
            email.to_owned(),
            code.to_owned(),
            expiry_minutes,
        ));
        std::future::ready(Ok(()))
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

async fn install_schema(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "BEGIN;
             SELECT pg_advisory_xact_lock(7843311947);
             CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE app_test_users (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               email text NOT NULL,
               code text NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               UNIQUE (app_id, email)
             );
             CREATE TEMP TABLE attrs (
               id uuid NOT NULL,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL,
               is_required boolean,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               on_delete attr_on_delete,
               on_delete_reverse attr_on_delete,
               checked_data_type checked_data_type,
               setting_unique boolean,
               indexing boolean,
               checking_data_type boolean,
               inferred_types bit(32),
               metadata jsonb,
               deletion_marked_at timestamptz,
               PRIMARY KEY (app_id, id)
             );
             CREATE UNIQUE INDEX server_auth_attrs_id ON attrs(id);
             CREATE TEMP TABLE triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL,
               value_md5 text NOT NULL,
               ea boolean NOT NULL,
               eav boolean NOT NULL,
               av boolean NOT NULL,
               ave boolean NOT NULL,
               vae boolean NOT NULL,
               checked_data_type checked_data_type,
               created_at bigint NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::bigint),
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE UNIQUE INDEX server_auth_ea ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX server_auth_av
               ON triples(app_id, attr_id, public.json_null_to_null(value)) WHERE av;
             CREATE OR REPLACE FUNCTION public.jsonb_deep_merge(obj1 jsonb, obj2 jsonb)
             RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $$
             DECLARE key text; item jsonb;
             BEGIN
               IF obj1 IS NULL OR obj2 IS NULL OR jsonb_typeof(obj1) != 'object' OR jsonb_typeof(obj2) != 'object'
               THEN RETURN obj2; END IF;
               FOR key, item IN SELECT * FROM jsonb_each(obj2) LOOP
                 obj1 := CASE
                   WHEN obj1 ? key AND jsonb_typeof(obj1->key) = 'object' AND jsonb_typeof(item) = 'object'
                     THEN jsonb_set(obj1, ARRAY[key], public.jsonb_deep_merge(obj1->key, item))
                   WHEN jsonb_typeof(item) = 'null' THEN obj1 - key
                   ELSE jsonb_set(obj1, ARRAY[key], item)
                 END;
               END LOOP;
               RETURN obj1;
              END $$;
              COMMIT;",
        )
        .await
        .unwrap();
}

async fn install_catalog(client: &tokio_postgres::Client, app_id: Uuid) {
    let catalog = system_attribute_catalog().unwrap();
    for attribute in &catalog.attributes {
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        client
            .execute(
                "INSERT INTO attrs
                 (id, app_id, value_type, cardinality, is_unique, is_indexed,
                  forward_ident, etype, label)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &attribute.id,
                    &app_id,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &Uuid::new_v4(),
                    &attribute.etype,
                    &attribute.label,
                ],
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn admin_auth_service_uses_system_triples_and_checks_admin_before_writes() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_auth_service_uses_system_triples_and_checks_admin_before_writes",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    install_catalog(&client, app_id).await;

    let delivery = Arc::new(TestDelivery::default());
    let service = AdminAuthService::new(
        client,
        Arc::new(TestAuthorizer),
        Arc::clone(&delivery),
        RuntimeAuthConfig {
            magic_code_expiry_minutes: 15,
            static_test_code: Some("123456".to_owned()),
        },
    );

    let first = service
        .create_token(
            context(app_id),
            AdminCreateTokenRequest {
                selector: AdminUserSelector::Email("admin-created@example.com".to_owned()),
                extra_fields: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.created, Some(true));
    assert_eq!(
        first.user.email.as_deref(),
        Some("admin-created@example.com")
    );

    let returning = service
        .create_token(
            context(app_id),
            AdminCreateTokenRequest {
                selector: AdminUserSelector::Email("admin-created@example.com".to_owned()),
                extra_fields: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(returning.created, Some(false));
    assert_eq!(returning.user.id, first.user.id);
    assert_ne!(returning.user.refresh_token, first.user.refresh_token);
    let found = service
        .get_user(
            context(app_id),
            AdminUserSelector::RefreshToken(returning.user.refresh_token),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, first.user.id);

    let generated = service
        .generate_magic_code(
            context(app_id),
            AdminMagicCodeRequest {
                email: "generated@example.com".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(generated.len(), 6);

    let invalid_refresh_email = "invalid-refresh@example.com";
    let invalid_refresh_code = service
        .generate_magic_code(
            context(app_id),
            AdminMagicCodeRequest {
                email: invalid_refresh_email.to_owned(),
            },
        )
        .await
        .unwrap();
    let invalid_refresh_token = Uuid::new_v4();
    let error = service
        .verify_magic_code(
            context(app_id),
            AdminVerifyMagicCodeRequest {
                email: invalid_refresh_email.to_owned(),
                code: invalid_refresh_code.clone(),
                refresh_token: Some(invalid_refresh_token),
                extra_fields: BTreeMap::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        serde_json::to_value(error.body()).unwrap(),
        serde_json::json!({
            "type": "record-not-found",
            "message": "Record not found: app-user",
            "hint": {
                "args": [{
                    "app-id": app_id,
                    "refresh-token": invalid_refresh_token,
                }],
                "record-type": "app-user"
            }
        })
    );
    assert!(
        service
            .get_user(
                context(app_id),
                AdminUserSelector::Email(invalid_refresh_email.to_owned()),
            )
            .await
            .unwrap()
            .is_none()
    );
    let retry = service
        .verify_magic_code(
            context(app_id),
            AdminVerifyMagicCodeRequest {
                email: invalid_refresh_email.to_owned(),
                code: invalid_refresh_code,
                refresh_token: None,
                extra_fields: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(retry.created, Some(true));
    let sent = service
        .send_magic_code(
            context(app_id),
            AdminMagicCodeRequest {
                email: "sent@example.com".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(sent.len(), 6);
    assert_eq!(delivery.messages.lock().unwrap()[0].3, 15);

    let verified = service
        .verify_magic_code(
            context(app_id),
            AdminVerifyMagicCodeRequest {
                email: "verified@example.com".to_owned(),
                code: "123456".to_owned(),
                refresh_token: None,
                extra_fields: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(verified.created, Some(true));
    assert_eq!(verified.user.email.as_deref(), Some("verified@example.com"));

    let guest = service
        .sign_in_guest(context(app_id), AdminSignInGuestRequest::default())
        .await
        .unwrap();
    assert!(guest.is_guest);

    service
        .sign_out(
            context(app_id),
            AdminSignOutRequest {
                selector: AdminUserSelector::RefreshToken(first.user.refresh_token),
            },
        )
        .await
        .unwrap();

    let deleted = service
        .delete_user(context(app_id), AdminUserSelector::Id(first.user.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.id, first.user.id);
    assert!(
        service
            .get_user(context(app_id), AdminUserSelector::Id(first.user.id))
            .await
            .unwrap()
            .is_none()
    );

    let error = service
        .sign_in_guest(
            AdminRequestContext {
                app_id,
                bearer_token: None,
                actor: AdminActor::Guest,
                client_ip: None,
            },
            AdminSignInGuestRequest::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status().as_u16(), 401);

    drop(service);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn runtime_magic_code_rejects_missing_supplied_refresh_before_consuming_code() {
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_magic_code_rejects_missing_supplied_refresh_before_consuming_code",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    install_catalog(&client, app_id).await;
    let delivery = Arc::new(TestDelivery::default());
    let service = RuntimeAuthService::new(
        client,
        Arc::clone(&delivery),
        StaticRuntimeAuthPolicy::new(system_attribute_catalog().unwrap(), AllowAllSignups),
        RuntimeAuthConfig {
            magic_code_expiry_minutes: 15,
            static_test_code: None,
        },
    );
    let email = "runtime-invalid-refresh@example.com";
    service
        .send_magic_code(SendMagicCodeRequest {
            app_id,
            email: email.to_owned(),
        })
        .await
        .unwrap();
    let code = delivery.messages.lock().unwrap()[0].2.clone();

    let invalid_refresh_token = Uuid::new_v4();
    let error = service
        .verify_magic_code(VerifyMagicCodeRequest {
            app_id,
            email: email.to_owned(),
            code: code.clone(),
            refresh_token: Some(invalid_refresh_token),
            extra_fields: BTreeMap::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        serde_json::to_value(error.body()).unwrap(),
        serde_json::json!({
            "type": "record-not-found",
            "message": "Record not found: app-user",
            "hint": {
                "args": [{
                    "app-id": app_id,
                    "refresh-token": invalid_refresh_token,
                }],
                "record-type": "app-user"
            }
        })
    );

    let retry = service
        .verify_magic_code(VerifyMagicCodeRequest {
            app_id,
            email: email.to_owned(),
            code,
            refresh_token: None,
            extra_fields: BTreeMap::new(),
        })
        .await
        .unwrap();
    assert_eq!(retry.created, Some(true));
    assert_eq!(retry.user.email.as_deref(), Some(email));

    drop(service);
    connection_task.await.unwrap().unwrap();
}
