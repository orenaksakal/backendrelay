use instant_http::{AdminActor, AdminRequestContext};
use instant_instaql::{AttributeValueType, DeleteAction, SYSTEM_CATALOG_APP_ID};
use instant_server::{AdminDataAuthorizer, AdminDataScope, PostgresAdminDataAuthorizer};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the migrations-shaped fixture beside its assertions.
async fn postgres_authorizer_validates_app_tokens_and_loads_the_full_catalog() {
    let Some(database_url) = instant_test_support::test_database_url(
        "postgres_authorizer_validates_app_tokens_and_loads_the_full_catalog",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TEMP TABLE app_admin_tokens (
               token uuid PRIMARY KEY,
               app_id uuid NOT NULL UNIQUE
             );
             CREATE TEMP TABLE attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL,
               is_required boolean NOT NULL,
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
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid NOT NULL,
               deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE instant_personal_access_tokens (
               lookup_key bytea PRIMARY KEY,
               user_id uuid NOT NULL
             );
             CREATE TEMP TABLE instant_user_oauth_access_tokens (
               lookup_key bytea PRIMARY KEY,
               user_id uuid NOT NULL,
               scopes text[] NOT NULL,
               expires_at timestamptz NOT NULL
             );",
        )
        .await
        .unwrap();

    let app_id = Uuid::new_v4();
    let creator_id = Uuid::new_v4();
    let token = Uuid::new_v4();
    let ref_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&token, &app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id) VALUES ($1, $2)",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();
    let personal_token = "per_personal-secret";
    let data_oauth_token = "pat_data-secret";
    let apps_oauth_token = "pat_apps-secret";
    let storage_oauth_token = "pat_storage-secret";
    client
        .execute(
            "INSERT INTO instant_personal_access_tokens (lookup_key, user_id) VALUES ($1, $2)",
            &[
                &Sha256::digest(personal_token.as_bytes()).to_vec(),
                &creator_id,
            ],
        )
        .await
        .unwrap();
    for (value, scopes) in [
        (data_oauth_token, vec!["data-write"]),
        (apps_oauth_token, vec!["apps-write"]),
        (storage_oauth_token, vec!["storage-write"]),
    ] {
        client
            .execute(
                "INSERT INTO instant_user_oauth_access_tokens
                   (lookup_key, user_id, scopes, expires_at)
                 VALUES ($1, $2, $3, now() + interval '1 hour')",
                &[
                    &Sha256::digest(value.as_bytes()).to_vec(),
                    &creator_id,
                    &scopes,
                ],
            )
            .await
            .unwrap();
    }
    client
        .execute(
            "INSERT INTO attrs (
               id, app_id, value_type, cardinality, is_unique, is_indexed,
               is_required, forward_ident, etype, label, reverse_ident,
               reverse_etype, reverse_label, on_delete, setting_unique
             ) VALUES (
               $1, $2, 'ref', 'many', true, true, true, $3, 'posts',
               'author', $4, 'users', 'posts', 'cascade', true
             )",
            &[&ref_id, &app_id, &Uuid::new_v4(), &Uuid::new_v4()],
        )
        .await
        .unwrap();
    let system_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (
               id, app_id, value_type, cardinality, is_unique, is_indexed,
               is_required, forward_ident, etype, label, checked_data_type,
               checking_data_type, indexing, setting_unique
             ) VALUES (
               $1, $2, 'blob', 'one', false, false, false, $3,
               '$users', 'createdAt', 'date', false, false, false
             )",
            &[&system_id, &SYSTEM_CATALOG_APP_ID, &Uuid::new_v4()],
        )
        .await
        .unwrap();

    let authorizer = PostgresAdminDataAuthorizer::new(client);
    let context = AdminRequestContext {
        app_id,
        bearer_token: Some(token.to_string()),
        actor: AdminActor::Email("person@example.com".to_owned()),
        client_ip: None,
    };
    let catalog = authorizer
        .authorize(context.clone(), AdminDataScope::Write)
        .await
        .unwrap();
    let link = catalog.by_id(ref_id).unwrap();
    assert_eq!(link.value_type, AttributeValueType::Ref);
    assert_eq!(link.reverse_etype.as_deref(), Some("users"));
    assert_eq!(link.reverse_label.as_deref(), Some("posts"));
    assert_eq!(link.on_delete, Some(DeleteAction::Cascade));
    assert!(link.setting_unique);
    assert!(catalog.by_id(system_id).is_some());

    for (bearer_token, scope) in [
        (personal_token, AdminDataScope::Write),
        (personal_token, AdminDataScope::AppsWrite),
        (personal_token, AdminDataScope::StorageWrite),
        (data_oauth_token, AdminDataScope::Read),
        (apps_oauth_token, AdminDataScope::AppsRead),
        (storage_oauth_token, AdminDataScope::StorageRead),
    ] {
        let platform_catalog = authorizer
            .authorize(
                AdminRequestContext {
                    app_id,
                    bearer_token: Some(bearer_token.to_owned()),
                    actor: AdminActor::Admin,
                    client_ip: None,
                },
                scope,
            )
            .await
            .unwrap();
        assert!(platform_catalog.by_id(ref_id).is_some());
    }
    for (bearer_token, required, required_name) in [
        (apps_oauth_token, AdminDataScope::Read, "data-read"),
        (
            data_oauth_token,
            AdminDataScope::StorageWrite,
            "storage-write",
        ),
        (storage_oauth_token, AdminDataScope::AppsRead, "apps-read"),
    ] {
        let missing_scope = authorizer
            .authorize(
                AdminRequestContext {
                    app_id,
                    bearer_token: Some(bearer_token.to_owned()),
                    actor: AdminActor::Admin,
                    client_ip: None,
                },
                required,
            )
            .await
            .unwrap_err();
        assert_eq!(
            missing_scope.body().message,
            format!("You are missing the {required_name} scope")
        );
        assert_eq!(
            missing_scope.body().hint,
            Some(serde_json::json!({"required-scope": required_name}))
        );
    }

    let error = authorizer
        .authorize(
            AdminRequestContext {
                bearer_token: Some(Uuid::new_v4().to_string()),
                ..context
            },
            AdminDataScope::Read,
        )
        .await
        .unwrap_err();
    assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        error.body().message,
        "This admin token may be expired or invalid. Or you may have provided an incorrect app ID."
    );

    let unauthenticated_catalog = authorizer.load_catalog(app_id).await.unwrap();
    assert_eq!(unauthenticated_catalog.attributes.len(), 2);

    drop(authorizer);
    connection_task.await.unwrap().unwrap();
}
