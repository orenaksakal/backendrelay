use std::collections::HashSet;

use instant_server::{
    AuditReport, AuditSeverity, INSTANT_CONFIG_APP_ID, INSTANT_HOMEPAGE_APP_ID,
    INSTANT_SYSTEM_CATALOG_APP_ID, audit_database, migrate_database,
};
use serde_json::{Value, json};
use tokio_postgres::{NoTls, Transaction};
use uuid::Uuid;

#[path = "../src/native_allocator.rs"]
mod native_allocator;

async fn connect_and_migrate() -> Option<String> {
    let database_url =
        instant_test_support::test_database_url("audit_reports_rules_hygiene_findings")?;
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    migrate_database(&mut client).await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
    Some(database_url)
}

fn checks_for(report: &AuditReport, app_id: Uuid) -> HashSet<&'static str> {
    report
        .findings
        .iter()
        .filter(|finding| finding.app_id == Some(app_id))
        .map(|finding| finding.check)
        .collect()
}

async fn insert_rules(transaction: &Transaction<'_>, app_id: Uuid, rules: Value) {
    transaction
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[&app_id, &rules],
        )
        .await
        .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn audit_reports_rules_hygiene_findings_and_exit_status() {
    let Some(database_url) = connect_and_migrate().await else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let transaction = client.transaction().await.unwrap();
    transaction
        .execute("DELETE FROM apps WHERE id = $1", &[&INSTANT_CONFIG_APP_ID])
        .await
        .unwrap();
    let report = audit_database(&transaction).await.unwrap();
    assert!(
        checks_for(&report, INSTANT_CONFIG_APP_ID).contains("config-app-missing"),
        "missing config app must be an error: {:?}",
        report.findings
    );
    assert_eq!(report.exit_code(), 1);
    transaction.rollback().await.unwrap();

    let transaction = client.transaction().await.unwrap();
    let config_member = Uuid::new_v4();
    transaction
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[
                &config_member,
                &format!("config-member-{config_member}@example.com"),
            ],
        )
        .await
        .unwrap();
    transaction
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&Uuid::new_v4(), &INSTANT_CONFIG_APP_ID],
        )
        .await
        .unwrap();
    transaction
        .execute(
            "INSERT INTO app_members (id, app_id, user_id, member_role)
             VALUES ($1, $2, $3, 'owner')",
            &[&Uuid::new_v4(), &INSTANT_CONFIG_APP_ID, &config_member],
        )
        .await
        .unwrap();
    transaction
        .execute(
            "INSERT INTO app_member_invites
               (id, app_id, inviter_id, invitee_role, invitee_email, status)
             VALUES ($1, $2, $3, 'collaborator', $4, 'pending')",
            &[
                &Uuid::new_v4(),
                &INSTANT_CONFIG_APP_ID,
                &config_member,
                &"invitee@example.com",
            ],
        )
        .await
        .unwrap();
    transaction
        .execute(
            "UPDATE rules SET code = '{}'::jsonb WHERE app_id = $1",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await
        .unwrap();
    let report = audit_database(&transaction).await.unwrap();
    let config_checks = checks_for(&report, INSTANT_CONFIG_APP_ID);
    assert!(config_checks.contains("config-app-admin-tokens"));
    assert!(config_checks.contains("config-app-members"));
    assert!(config_checks.contains("config-app-invites"));
    assert!(config_checks.contains("config-app-rules"));
    assert_eq!(report.exit_code(), 1);
    transaction.rollback().await.unwrap();

    let transaction = client.transaction().await.unwrap();
    let creator = Uuid::new_v4();
    let org = Uuid::new_v4();
    transaction
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator, &format!("audit-{creator}@example.com")],
        )
        .await
        .unwrap();
    let ephemeral_creator: Uuid = transaction
        .query_one(
            "SELECT id FROM instant_users WHERE email = $1",
            &[&"ephemeral-apps@backendrelay.invalid"],
        )
        .await
        .unwrap()
        .get(0);
    transaction
        .execute(
            "INSERT INTO orgs (id, title) VALUES ($1, 'audit org')",
            &[&org],
        )
        .await
        .unwrap();

    let deny_all = json!({"$default": {"allow": {"$default": "false"}}});
    let clean = Uuid::new_v4();
    let no_rules = Uuid::new_v4();
    let empty_rules = Uuid::new_v4();
    let open_default = Uuid::new_v4();
    let no_owner = Uuid::new_v4();
    let stale_ephemeral = Uuid::new_v4();
    for (app_id, creator_id, org_id, title) in [
        (clean, Some(creator), None, "clean"),
        (no_rules, Some(creator), None, "no rules"),
        (empty_rules, Some(creator), None, "empty rules"),
        (open_default, Some(creator), None, "open default"),
        (no_owner, None, Some(org), "no owner"),
    ] {
        transaction
            .execute(
                "INSERT INTO apps (id, creator_id, org_id, title) VALUES ($1, $2, $3, $4)",
                &[&app_id, &creator_id, &org_id, &title],
            )
            .await
            .unwrap();
    }
    transaction
        .execute(
            "INSERT INTO apps (id, creator_id, org_id, title, created_at)
             VALUES ($1, $2, NULL, 'stale ephemeral', now() - interval '15 days')",
            &[&stale_ephemeral, &ephemeral_creator],
        )
        .await
        .unwrap();

    insert_rules(&transaction, clean, deny_all.clone()).await;
    insert_rules(&transaction, empty_rules, json!({})).await;
    insert_rules(
        &transaction,
        open_default,
        json!({"$default": {"allow": {"$default": "true"}}}),
    )
    .await;
    insert_rules(&transaction, no_owner, deny_all.clone()).await;
    insert_rules(&transaction, stale_ephemeral, deny_all).await;

    let report = audit_database(&transaction).await.unwrap();
    assert!(
        checks_for(&report, clean).is_empty(),
        "clean app must not be reported: {:?}",
        report.findings
    );
    assert!(checks_for(&report, no_rules).contains("app-no-rules"));
    assert!(checks_for(&report, empty_rules).contains("app-empty-rules"));
    assert!(checks_for(&report, open_default).contains("app-default-allows"));
    assert!(checks_for(&report, no_owner).contains("app-no-owner"));
    assert!(checks_for(&report, stale_ephemeral).contains("app-stale-ephemeral"));
    for app_id in [INSTANT_SYSTEM_CATALOG_APP_ID, INSTANT_HOMEPAGE_APP_ID] {
        assert!(
            checks_for(&report, app_id).is_empty(),
            "reserved app {app_id} must not be reported"
        );
    }
    assert!(report.error_count() >= 1);
    assert!(report.warning_count() >= 4);
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.summary_json()["errors"], report.error_count());

    let wire_finding = report
        .findings
        .iter()
        .find(|finding| finding.app_id == Some(no_rules))
        .map(|finding| serde_json::to_value(finding).unwrap())
        .unwrap();
    assert_eq!(wire_finding["severity"], "warn");
    assert_eq!(wire_finding["check"], "app-no-rules");
    assert_eq!(wire_finding["app_id"], no_rules.to_string());
    assert!(wire_finding["detail"].is_string());

    let warning_only = AuditReport {
        findings: report
            .findings
            .iter()
            .filter(|finding| finding.severity == AuditSeverity::Warn)
            .cloned()
            .collect(),
    };
    assert_eq!(warning_only.exit_code(), 0);
    transaction.rollback().await.unwrap();
}
