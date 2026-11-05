use instant_server::{
    AppRole, PostgresSuperadminAuthorizer, SuperadminAccess, SuperadminAuthorizer, SuperadminScope,
};
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the complete authorization matrix visible together.
async fn postgres_superadmin_authorizer_enforces_scopes_and_strongest_membership() {
    let Some(database_url) = instant_test_support::test_database_url(
        "postgres_superadmin_authorizer_enforces_scopes_and_strongest_membership",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    let org_app_id = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    let admin_token = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let collaborator_id = Uuid::new_v4();
    let org_admin_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, org_id) VALUES
             ($1, $2, NULL), ($3, NULL, $4)",
            &[&app_id, &owner_id, &org_app_id, &org_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_members (app_id, user_id, member_role)
             VALUES ($1, $2, 'collaborator')",
            &[&app_id, &collaborator_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_members (org_id, user_id, role)
             VALUES ($1, $2, 'admin')",
            &[&org_id, &org_admin_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&admin_token, &app_id],
        )
        .await
        .unwrap();

    let personal = "per_owner";
    insert_personal(&client, personal, owner_id).await;
    let collaborator = "pat_collaborator";
    insert_oauth(&client, collaborator, collaborator_id, &["apps-write"]).await;
    let read_only = "pat_read-only";
    insert_oauth(&client, read_only, collaborator_id, &["apps-read"]).await;
    let org_admin = "per_org-admin";
    insert_personal(&client, org_admin, org_admin_id).await;

    let authorizer = PostgresSuperadminAuthorizer::new(client);
    let owner = authorizer
        .authorize(
            personal.to_owned(),
            access(
                SuperadminScope::AppsTransfer,
                Some((app_id, AppRole::Owner)),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(owner.user_id, Some(owner_id));
    assert_eq!(owner.admin_app_id, None);
    let app_admin = authorizer
        .authorize(
            admin_token.to_string(),
            access_with_app_admin(
                SuperadminScope::AppsWrite,
                Some((app_id, AppRole::Collaborator)),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(app_admin.user_id, None);
    assert_eq!(app_admin.admin_app_id, Some(app_id));
    authorizer
        .authorize(
            collaborator.to_owned(),
            access(
                SuperadminScope::AppsWrite,
                Some((app_id, AppRole::Collaborator)),
                None,
            ),
        )
        .await
        .unwrap();
    let insufficient_role = authorizer
        .authorize(
            collaborator.to_owned(),
            access(
                SuperadminScope::AppsWrite,
                Some((app_id, AppRole::Admin)),
                None,
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(
        insufficient_role.body().message,
        "Insufficient app member role"
    );
    let missing_scope = authorizer
        .authorize(
            read_only.to_owned(),
            access(SuperadminScope::AppsWrite, None, None),
        )
        .await
        .unwrap_err();
    assert_eq!(
        missing_scope.body().hint,
        Some(serde_json::json!({"required-scope": "apps-write"}))
    );
    authorizer
        .authorize(
            org_admin.to_owned(),
            access(
                SuperadminScope::AppsRead,
                Some((org_app_id, AppRole::Admin)),
                Some(org_id),
            ),
        )
        .await
        .unwrap();

    drop(authorizer);
    connection_task.await.unwrap().unwrap();
}

const fn access(
    scope: SuperadminScope,
    app: Option<(Uuid, AppRole)>,
    org_id: Option<Uuid>,
) -> SuperadminAccess {
    SuperadminAccess {
        scope,
        app,
        org_id,
        allow_app_admin: false,
    }
}

const fn access_with_app_admin(
    scope: SuperadminScope,
    app: Option<(Uuid, AppRole)>,
    org_id: Option<Uuid>,
) -> SuperadminAccess {
    SuperadminAccess {
        scope,
        app,
        org_id,
        allow_app_admin: true,
    }
}

async fn install_schema(client: &Client) {
    client
        .batch_execute(
            "CREATE TEMP TABLE instant_personal_access_tokens (
               lookup_key bytea PRIMARY KEY, user_id uuid NOT NULL
             );
             CREATE TEMP TABLE instant_user_oauth_access_tokens (
               lookup_key bytea PRIMARY KEY, user_id uuid NOT NULL,
               scopes text[] NOT NULL, expires_at timestamptz NOT NULL
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY, creator_id uuid, org_id uuid,
               deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_members (
               app_id uuid NOT NULL, user_id uuid NOT NULL, member_role text NOT NULL
             );
             CREATE TEMP TABLE app_admin_tokens (
               token uuid PRIMARY KEY, app_id uuid NOT NULL
             );
             CREATE TEMP TABLE org_members (
               org_id uuid NOT NULL, user_id uuid NOT NULL, role text NOT NULL
             );",
        )
        .await
        .unwrap();
}

async fn insert_personal(client: &Client, token: &str, user_id: Uuid) {
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    client
        .execute(
            "INSERT INTO instant_personal_access_tokens (lookup_key, user_id) VALUES ($1, $2)",
            &[&digest, &user_id],
        )
        .await
        .unwrap();
}

async fn insert_oauth(client: &Client, token: &str, user_id: Uuid, scopes: &[&str]) {
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    client
        .execute(
            "INSERT INTO instant_user_oauth_access_tokens
               (lookup_key, user_id, scopes, expires_at)
             VALUES ($1, $2, $3, now() + interval '1 hour')",
            &[&digest, &user_id, &scopes],
        )
        .await
        .unwrap();
}
