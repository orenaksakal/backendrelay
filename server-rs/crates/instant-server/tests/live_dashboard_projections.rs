use std::sync::Arc;

use chrono::Utc;
use instant_instaql::SYSTEM_CATALOG_APP_ID;
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardSummaryProvider, DashboardUser,
    NoopDashboardControlHooks, PostgresDashboardCoreHooks, PostgresDashboardSummaryProvider,
};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn app_and_org_compound_projections_match_dashboard_shapes() {
    let Some(database_url) = instant_test_support::test_database_url(
        "app_and_org_compound_projections_match_dashboard_shapes",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    let schema = format!("dashboard_projection_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE instant_profiles (
               id uuid PRIMARY KEY, meta jsonb NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE instant_subscriptions (
               id uuid PRIMARY KEY, subscription_type_id smallint NOT NULL,
               stripe_customer_id text, stripe_subscription_id text
             );
             CREATE TABLE orgs (
               id uuid PRIMARY KEY, title text NOT NULL, subscription_id uuid,
               created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE apps (
               id uuid PRIMARY KEY, creator_id uuid, org_id uuid, title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(), status text NOT NULL DEFAULT 'active',
               magic_code_expiry_minutes integer, deletion_marked_at timestamp, subscription_id uuid
             );
             CREATE TABLE app_admin_tokens (token uuid PRIMARY KEY, app_id uuid UNIQUE NOT NULL);
             CREATE TABLE rules (app_id uuid PRIMARY KEY, code jsonb NOT NULL, version integer NOT NULL);
             CREATE TABLE app_members (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, user_id uuid NOT NULL,
               member_role text NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE app_member_invites (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, inviter_id uuid NOT NULL,
               invitee_email text NOT NULL,
               invitee_role text NOT NULL, status text NOT NULL, sent_at timestamptz NOT NULL
             );
             CREATE TABLE org_members (
               id uuid PRIMARY KEY, org_id uuid NOT NULL, user_id uuid NOT NULL, role text NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE org_member_invites (
               id uuid PRIMARY KEY, org_id uuid NOT NULL, inviter_id uuid NOT NULL,
               invitee_email text NOT NULL,
               invitee_role text NOT NULL, status text NOT NULL, sent_at timestamptz NOT NULL
             );
             CREATE TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL);
             CREATE TABLE webhooks (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, sink jsonb NOT NULL,
               id_attr_ids uuid[] NOT NULL, actions text[] NOT NULL, status text NOT NULL,
               disabled_reason text, created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE app_email_senders (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE app_email_templates (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, email_type text NOT NULL,
               sender_id uuid, subject text NOT NULL, body text NOT NULL, name text
             );"
        ))
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    let member_id = Uuid::new_v4();
    let app_member_only_id = Uuid::new_v4();
    let app_admin_id = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let member_app_id = Uuid::new_v4();
    let personal_app_id = Uuid::new_v4();
    let subscription_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let sender_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users VALUES
               ($1, 'owner@example.com'), ($2, 'member@example.com'),
               ($3, 'app-member@example.com'), ($4, 'app-admin@example.com')",
            &[&user_id, &member_id, &app_member_only_id, &app_admin_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_subscriptions VALUES ($1, 3, 'cus_org', 'sub_org')",
            &[&subscription_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO orgs (id, title, subscription_id) VALUES ($1, 'Team', $2)",
            &[&org_id, &subscription_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, org_id, title) VALUES
               ($1, NULL, $3, 'Alpha'), ($2, NULL, $3, 'Member only'),
               ($4, $5, NULL, 'Personal')",
            &[&app_id, &member_app_id, &org_id, &personal_app_id, &user_id],
        )
        .await
        .unwrap();
    let admin_token = Uuid::new_v4();
    let member_admin_token = Uuid::new_v4();
    let personal_admin_token = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO app_admin_tokens VALUES ($1, $3), ($2, $4), ($5, $6)",
            &[
                &admin_token,
                &member_admin_token,
                &app_id,
                &member_app_id,
                &personal_admin_token,
                &personal_app_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rules VALUES ($1, $2, 7)",
            &[&app_id, &json!({"attrs": {"allow": {"view": "true"}}})],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_members VALUES
               ($1, $2, $3, 'owner'), ($4, $2, $5, 'admin'),
               ($6, $2, $7, 'collaborator')",
            &[
                &Uuid::new_v4(),
                &org_id,
                &user_id,
                &Uuid::new_v4(),
                &member_id,
                &Uuid::new_v4(),
                &app_admin_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_members VALUES
               ($1, $3, $4, 'collaborator', now()),
               ($2, $5, $6, 'collaborator', now()),
               ($7, $3, $8, 'admin', now())",
            &[
                &Uuid::new_v4(),
                &Uuid::new_v4(),
                &app_id,
                &member_id,
                &member_app_id,
                &app_member_only_id,
                &Uuid::new_v4(),
                &app_admin_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_member_invites VALUES ($1, $2, $3, 'invite@example.com', 'admin', 'pending', now() - interval '4 days')",
            &[&Uuid::new_v4(), &app_id, &user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_member_invites VALUES ($1, $2, $3, 'org-invite@example.com', 'collaborator', 'pending', now())",
            &[&Uuid::new_v4(), &org_id, &user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO attrs VALUES ($1, $2, 'widgets')",
            &[&id_attr, &app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO webhooks (id, app_id, sink, id_attr_ids, actions, status)
             VALUES ($3, $2, $4, ARRAY[$1::uuid]::uuid[],
                     ARRAY['create', 'update'], 'active')",
            &[
                &id_attr,
                &app_id,
                &Uuid::new_v4(),
                &json!({"url": "https://hook.example"}),
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_email_senders VALUES ($1, 'auth@example.com')",
            &[&sender_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_email_templates VALUES ($1, $2, 'magic-code', $3, 'Code {code}', 'Body {code}', 'Auth')",
            &[&Uuid::new_v4(), &app_id, &sender_id],
        )
        .await
        .unwrap();

    let hooks = PostgresDashboardCoreHooks::new(client, NoopDashboardControlHooks);
    let app = hooks
        .execute(DashboardControlHookOperation::EnrichApp {
            app_id,
            user_id: Some(member_id),
            app: json!({"id": app_id, "title": "Alpha", "admin_token": admin_token}),
        })
        .await
        .unwrap();
    assert_eq!(app["rules_version"], 7);
    assert_eq!(app["org"]["id"], org_id.to_string());
    assert_eq!(app["user_app_role"], "collaborator");
    assert_eq!(
        app["role"], "admin",
        "the org admin role outranks the app role"
    );
    let members = app["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "both app members are listed");
    assert!(
        members
            .iter()
            .any(|member| member["email"] == "member@example.com"
                && member["role"] == "collaborator"),
        "the collaborator member is projected with their role"
    );
    assert_eq!(app["invites"][0]["expired"], true);
    assert_eq!(app["webhooks"][0]["namespaces"], json!(["widgets"]));
    assert_eq!(
        app["magic_code_email_template"]["email"],
        "auth@example.com"
    );

    let app_admin = hooks
        .execute(DashboardControlHookOperation::EnrichApp {
            app_id,
            user_id: Some(app_admin_id),
            app: json!({"id": app_id, "title": "Alpha"}),
        })
        .await
        .unwrap();
    assert_eq!(
        app_admin["role"], "admin",
        "the app admin role outranks the org collaborator role"
    );

    let org_owner = hooks
        .execute(DashboardControlHookOperation::EnrichApp {
            app_id,
            user_id: Some(user_id),
            app: json!({"id": app_id, "title": "Alpha"}),
        })
        .await
        .unwrap();
    assert_eq!(
        org_owner["role"], "owner",
        "an org owner has owner access to its apps"
    );

    let personal = hooks
        .execute(DashboardControlHookOperation::EnrichApp {
            app_id: personal_app_id,
            user_id: Some(user_id),
            app: json!({"id": personal_app_id, "title": "Personal"}),
        })
        .await
        .unwrap();
    assert_eq!(personal["role"], "owner", "the app creator owns the app");

    let outsider = hooks
        .execute(DashboardControlHookOperation::EnrichApp {
            app_id,
            user_id: Some(Uuid::new_v4()),
            app: json!({"id": app_id, "title": "Alpha"}),
        })
        .await
        .unwrap();
    assert_eq!(outsider["role"], "app-member");

    let details = hooks
        .execute(DashboardControlHookOperation::OrgDetails {
            org_id,
            user_id,
            org: json!({"id": org_id, "title": "Team"}),
        })
        .await
        .unwrap();
    assert_eq!(details["org"]["role"], "owner");
    assert_eq!(details["org"]["paid"], true);
    assert_eq!(details["apps"].as_array().unwrap().len(), 2);
    assert!(
        details["apps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|app| app["id"] == app_id.to_string())
    );
    assert!(
        details["apps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|app| app["role"] == "owner"),
        "org owners see owner on every org app"
    );
    assert!(
        details["apps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|app| app.get("admin_token").is_some()),
        "org owners still receive app admin tokens"
    );
    assert_eq!(details["members"].as_array().unwrap().len(), 3);
    assert!(
        details["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|member| member["created_at"].is_string()),
        "org members expose their join time for the members table"
    );
    assert_eq!(details["invites"][0]["email"], "org-invite@example.com");

    let admin_details = hooks
        .execute(DashboardControlHookOperation::OrgDetails {
            org_id,
            user_id: member_id,
            org: json!({"id": org_id, "title": "Team"}),
        })
        .await
        .unwrap();
    assert_eq!(admin_details["org"]["role"], "admin");
    assert!(
        admin_details["apps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|app| app["role"] == "admin")
    );

    let app_member_details = hooks
        .execute(DashboardControlHookOperation::OrgDetails {
            org_id,
            user_id: app_member_only_id,
            org: json!({"id": org_id, "title": "Team"}),
        })
        .await
        .unwrap();
    assert_eq!(app_member_details["org"]["role"], "app-member");
    assert_eq!(app_member_details["apps"].as_array().unwrap().len(), 1);
    assert_eq!(
        app_member_details["apps"][0]["id"],
        member_app_id.to_string()
    );
    assert_eq!(app_member_details["apps"][0]["role"], "collaborator");
    assert!(
        app_member_details["apps"][0].get("admin_token").is_none(),
        "collaborators must not receive the admin token"
    );
    assert!(app_member_details["members"].as_array().unwrap().is_empty());
    assert!(app_member_details["invites"].as_array().unwrap().is_empty());

    let summary_client = schema_client(&database_url, &schema).await;
    let summary = PostgresDashboardSummaryProvider::new(summary_client, Arc::new(hooks))
        .summary(DashboardUser {
            id: user_id,
            email: "owner@example.com".to_owned(),
            created_at: Utc::now().naive_utc(),
        })
        .await
        .unwrap();
    let summary_apps = summary["apps"].as_array().unwrap();
    assert_eq!(summary_apps.len(), 1, "GET /dash lists only personal apps");
    assert_eq!(summary_apps[0]["id"], personal_app_id.to_string());
    assert_eq!(summary_apps[0]["role"], "owner");

    // The namespace query includes system attributes when webhook ID attributes reference them.
    assert_ne!(SYSTEM_CATALOG_APP_ID, app_id);
}

async fn schema_client(database_url: &str, schema: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    client
}
