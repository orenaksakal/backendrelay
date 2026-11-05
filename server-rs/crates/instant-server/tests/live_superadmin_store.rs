use instant_http::{SuperadminContext, SuperadminOperation};
use instant_server::{
    NoopDashboardControlHooks, PostgresSuperadminStore, SuperadminPrincipal, SuperadminStore,
};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)] // Exercises the complete fourteen-operation store surface.
async fn postgres_superadmin_store_executes_the_complete_route_surface() {
    let Some(database_url) = instant_test_support::test_database_url(
        "postgres_superadmin_store_executes_the_complete_route_surface",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'owner@example.com')",
            &[&user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO orgs (id, title) VALUES ($1, 'Team')",
            &[&org_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&org_id, &user_id],
        )
        .await
        .unwrap();
    seed_app(&client, app_id, Some(user_id), None).await;
    let collaborator_id = Uuid::new_v4();
    let org_app_id = Uuid::new_v4();
    seed_app(&client, org_app_id, None, Some(org_id)).await;
    client
        .execute(
            "INSERT INTO app_members (app_id, user_id, member_role)
             VALUES ($1, $2, 'collaborator')",
            &[&org_app_id, &collaborator_id],
        )
        .await
        .unwrap();

    let principal = SuperadminPrincipal {
        user_id: Some(user_id),
        admin_app_id: None,
    };
    let store = PostgresSuperadminStore::new(client, NoopDashboardControlHooks);
    assert_eq!(
        execute(
            &store,
            principal,
            SuperadminOperation::ListApps {
                include: vec!["schema".to_owned(), "perms".to_owned()]
            }
        )
        .await["apps"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        execute(&store, principal, SuperadminOperation::ListOrgs).await["orgs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Collaborators see org apps but never the admin token that would bypass
    // their role; the org owner still receives it.
    // Collaborators see org apps but never the admin token that would bypass
    // their role; the org owner still receives it.
    let collaborator = SuperadminPrincipal {
        user_id: Some(collaborator_id),
        admin_app_id: None,
    };
    let owner_org_apps = execute(
        &store,
        principal,
        SuperadminOperation::ListOrgApps {
            org_id,
            include: vec![],
        },
    )
    .await;
    assert_eq!(owner_org_apps["apps"].as_array().unwrap().len(), 1);
    assert!(owner_org_apps["apps"][0].get("admin_token").is_some());
    let collaborator_org_apps = execute(
        &store,
        collaborator,
        SuperadminOperation::ListOrgApps {
            org_id,
            include: vec![],
        },
    )
    .await;
    assert_eq!(collaborator_org_apps["apps"].as_array().unwrap().len(), 1);
    assert!(
        collaborator_org_apps["apps"][0]
            .get("admin_token")
            .is_none(),
        "collaborators must not receive the admin token"
    );
    let collaborator_app = execute(
        &store,
        collaborator,
        SuperadminOperation::GetApp { app_id: org_app_id },
    )
    .await;
    assert!(collaborator_app["app"].get("admin_token").is_none());

    let created = execute(
        &store,
        principal,
        SuperadminOperation::CreateApp {
            body: serde_json::json!({
                "title": "Created",
                "org_id": org_id,
                "schema": {},
                "perms": {}
            }),
        },
    )
    .await;
    let created_id: Uuid = serde_json::from_value(created["app"]["id"].clone()).unwrap();
    assert_eq!(created["app"]["title"], "Created");
    assert_eq!(
        execute(&store, principal, SuperadminOperation::GetApp { app_id }).await["app"]["id"],
        app_id.to_string()
    );
    assert_eq!(
        execute(
            &store,
            principal,
            SuperadminOperation::UpdateApp {
                app_id,
                body: serde_json::json!({"title": "Renamed"})
            }
        )
        .await["app"]["title"],
        "Renamed"
    );

    let invite = execute(
        &store,
        principal,
        SuperadminOperation::SendTransfer {
            app_id,
            body: serde_json::json!({"dest_email": "NEW@EXAMPLE.COM"}),
        },
    )
    .await;
    assert!(invite["id"].is_string());
    assert_eq!(
        execute(
            &store,
            principal,
            SuperadminOperation::RevokeTransfer {
                app_id,
                body: serde_json::json!({"dest_email": "new@example.com"})
            }
        )
        .await["count"],
        1
    );
    assert!(
        execute(&store, principal, SuperadminOperation::GetSchema { app_id }).await["schema"]
            .is_object()
    );
    for operation in [
        SuperadminOperation::PlanSchema {
            app_id,
            body: serde_json::json!({"schema": {}}),
        },
        SuperadminOperation::ApplySchema {
            app_id,
            body: serde_json::json!({"schema": {}}),
        },
    ] {
        assert!(execute(&store, principal, operation).await["steps"].is_array());
    }
    assert!(
        execute(&store, principal, SuperadminOperation::GetPerms { app_id }).await["perms"]
            .is_null()
    );
    assert_eq!(
        execute(
            &store,
            principal,
            SuperadminOperation::PutPerms {
                app_id,
                body: serde_json::json!({"code": {}})
            }
        )
        .await["rules"]["code"],
        serde_json::json!({})
    );
    assert_eq!(
        execute(&store, principal, SuperadminOperation::DeleteApp { app_id }).await["app"]["id"],
        app_id.to_string()
    );
    assert_ne!(created_id, app_id);

    drop(store);
    connection_task.await.unwrap().unwrap();
}

async fn execute(
    store: &PostgresSuperadminStore<NoopDashboardControlHooks>,
    principal: SuperadminPrincipal,
    operation: SuperadminOperation,
) -> serde_json::Value {
    store
        .execute(
            principal,
            SuperadminContext {
                bearer_token: "personal-token".to_owned(),
            },
            operation,
        )
        .await
        .unwrap()
}

async fn seed_app(client: &Client, app_id: Uuid, creator_id: Option<Uuid>, org_id: Option<Uuid>) {
    client
        .execute(
            "INSERT INTO apps (id, creator_id, org_id, title) VALUES ($1, $2, $3, 'Original')",
            &[&app_id, &creator_id, &org_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&Uuid::new_v4(), &app_id],
        )
        .await
        .unwrap();
}

async fn install_schema(client: &Client) {
    client
        .batch_execute(
            "CREATE TEMP TABLE instant_users (
               id uuid PRIMARY KEY, email text NOT NULL
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY, creator_id uuid, org_id uuid, title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(), status text NOT NULL DEFAULT 'active',
               magic_code_expiry_minutes integer, deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_admin_tokens (
               token uuid PRIMARY KEY, app_id uuid NOT NULL UNIQUE
             );
             CREATE TEMP TABLE orgs (
               id uuid PRIMARY KEY, title text NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE org_members (
               org_id uuid NOT NULL, user_id uuid NOT NULL, role text NOT NULL
             );
             CREATE TEMP TABLE app_members (
               app_id uuid NOT NULL, user_id uuid NOT NULL, member_role text NOT NULL
             );
             CREATE TEMP TABLE app_member_invites (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, inviter_id uuid NOT NULL,
               invitee_role text NOT NULL, invitee_email text NOT NULL, status text NOT NULL,
               sent_at timestamp NOT NULL DEFAULT now(), UNIQUE (app_id, invitee_email)
             );
             CREATE TEMP TABLE rules (
               app_id uuid PRIMARY KEY, code jsonb NOT NULL, version integer NOT NULL DEFAULT 1
             );
             CREATE TEMP TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, value_type text NOT NULL,
               cardinality text NOT NULL, is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL, is_required boolean NOT NULL,
               forward_ident uuid NOT NULL, etype text NOT NULL, label text NOT NULL,
               reverse_ident uuid, reverse_etype text, reverse_label text,
               inferred_types bit(32), on_delete text, on_delete_reverse text,
               checked_data_type text, checking_data_type boolean, indexing boolean,
               setting_unique boolean, metadata jsonb, deletion_marked_at timestamptz
             );",
        )
        .await
        .unwrap();
}
