use serde::Serialize;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;
use uuid::Uuid;

use crate::config_app::{INSTANT_HOMEPAGE_APP_ID, INSTANT_SYSTEM_CATALOG_APP_ID};
use crate::postgres_dashboard_policy::INSTANT_CONFIG_APP_ID;

const EPHEMERAL_CREATOR_EMAILS: [&str; 2] = [
    "ephemeral-apps-dev@backendrelay.invalid",
    "ephemeral-apps@backendrelay.invalid",
];
const STALE_EPHEMERAL_DAYS: i32 = 14;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditSeverity {
    Error,
    Warn,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuditFinding {
    pub severity: AuditSeverity,
    pub check: &'static str,
    pub app_id: Option<Uuid>,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditReport {
    pub findings: Vec<AuditFinding>,
}

impl AuditReport {
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == AuditSeverity::Error)
            .count()
    }

    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == AuditSeverity::Warn)
            .count()
    }

    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(self.error_count() > 0)
    }

    #[must_use]
    pub fn summary_json(&self) -> Value {
        json!({
            "severity": "info",
            "check": "summary",
            "errors": self.error_count(),
            "warnings": self.warning_count(),
        })
    }
}

/// Reads the deployment's app rules and ownership state and reports hygiene
/// findings without changing anything.
///
/// # Errors
///
/// Returns a database error when the audited tables cannot be read.
pub async fn audit_database<C>(client: &C) -> Result<AuditReport, tokio_postgres::Error>
where
    C: GenericClient + Sync,
{
    let mut findings = Vec::new();
    audit_config_app(client, &mut findings).await?;
    audit_app_rules(client, &mut findings).await?;
    audit_app_owners(client, &mut findings).await?;
    audit_stale_ephemeral_apps(client, &mut findings).await?;
    Ok(AuditReport { findings })
}

async fn audit_config_app<C>(
    client: &C,
    findings: &mut Vec<AuditFinding>,
) -> Result<(), tokio_postgres::Error>
where
    C: GenericClient + Sync,
{
    let app = client
        .query_opt(
            "SELECT rules.code
             FROM apps
             LEFT JOIN rules ON rules.app_id = apps.id
             WHERE apps.id = $1 AND apps.deletion_marked_at IS NULL",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?;
    let Some(app) = app else {
        findings.push(AuditFinding {
            severity: AuditSeverity::Error,
            check: "config-app-missing",
            app_id: Some(INSTANT_CONFIG_APP_ID),
            detail: "the deployment config app is missing or soft-deleted".to_owned(),
        });
        return Ok(());
    };

    let admin_tokens: i64 = client
        .query_one(
            "SELECT count(*) FROM app_admin_tokens WHERE app_id = $1",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?
        .get(0);
    if admin_tokens > 0 {
        findings.push(AuditFinding {
            severity: AuditSeverity::Error,
            check: "config-app-admin-tokens",
            app_id: Some(INSTANT_CONFIG_APP_ID),
            detail: format!("the config app still has {admin_tokens} admin token(s)"),
        });
    }

    let members: i64 = client
        .query_one(
            "SELECT count(*) FROM app_members WHERE app_id = $1",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?
        .get(0);
    if members > 0 {
        findings.push(AuditFinding {
            severity: AuditSeverity::Error,
            check: "config-app-members",
            app_id: Some(INSTANT_CONFIG_APP_ID),
            detail: format!("the config app still has {members} member(s)"),
        });
    }

    let invites: i64 = client
        .query_one(
            "SELECT count(*) FROM app_member_invites WHERE app_id = $1",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?
        .get(0);
    if invites > 0 {
        findings.push(AuditFinding {
            severity: AuditSeverity::Error,
            check: "config-app-invites",
            app_id: Some(INSTANT_CONFIG_APP_ID),
            detail: format!("the config app still has {invites} invite(s)"),
        });
    }

    let deny_all = json!({"$default": {"allow": {"$default": "false"}}});
    let rules: Option<Value> = app.get(0);
    if rules.as_ref() != Some(&deny_all) {
        findings.push(AuditFinding {
            severity: AuditSeverity::Error,
            check: "config-app-rules",
            app_id: Some(INSTANT_CONFIG_APP_ID),
            detail: "the config app rules are not the deny-all rules from migration 146".to_owned(),
        });
    }
    Ok(())
}

async fn audit_app_rules<C>(
    client: &C,
    findings: &mut Vec<AuditFinding>,
) -> Result<(), tokio_postgres::Error>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT apps.id, rules.code
             FROM apps
             LEFT JOIN rules ON rules.app_id = apps.id
             WHERE apps.deletion_marked_at IS NULL
             ORDER BY apps.id",
            &[],
        )
        .await?;
    for row in rows {
        let app_id: Uuid = row.get(0);
        if is_system_app(app_id) {
            continue;
        }
        let rules: Option<Value> = row.get(1);
        match rules {
            None => findings.push(AuditFinding {
                severity: AuditSeverity::Warn,
                check: "app-no-rules",
                app_id: Some(app_id),
                detail: "the app has no rules row".to_owned(),
            }),
            Some(rules) if rules == json!({}) => findings.push(AuditFinding {
                severity: AuditSeverity::Warn,
                check: "app-empty-rules",
                app_id: Some(app_id),
                detail: "the app rules are empty (`{}`)".to_owned(),
            }),
            Some(rules) if default_rule_allows_everything(&rules) => {
                findings.push(AuditFinding {
                    severity: AuditSeverity::Error,
                    check: "app-default-allows",
                    app_id: Some(app_id),
                    detail: "the app's `$default` rules allow everything".to_owned(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

async fn audit_app_owners<C>(
    client: &C,
    findings: &mut Vec<AuditFinding>,
) -> Result<(), tokio_postgres::Error>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT apps.id
             FROM apps
             WHERE apps.deletion_marked_at IS NULL
               AND apps.creator_id IS NULL
               AND NOT EXISTS (
                 SELECT 1 FROM app_members
                 WHERE app_members.app_id = apps.id
                   AND lower(app_members.member_role) = 'owner'
               )
             ORDER BY apps.id",
            &[],
        )
        .await?;
    for row in rows {
        let app_id: Uuid = row.get(0);
        if is_system_app(app_id) {
            continue;
        }
        findings.push(AuditFinding {
            severity: AuditSeverity::Warn,
            check: "app-no-owner",
            app_id: Some(app_id),
            detail: "the app has no creator and no Owner member".to_owned(),
        });
    }
    Ok(())
}

async fn audit_stale_ephemeral_apps<C>(
    client: &C,
    findings: &mut Vec<AuditFinding>,
) -> Result<(), tokio_postgres::Error>
where
    C: GenericClient + Sync,
{
    let [development_email, hosted_email] = EPHEMERAL_CREATOR_EMAILS;
    let rows = client
        .query(
            "SELECT apps.id
             FROM apps
             JOIN instant_users ON instant_users.id = apps.creator_id
             WHERE apps.deletion_marked_at IS NULL
               AND instant_users.email IN ($1, $2)
               AND apps.created_at < now() - make_interval(days => $3)
             ORDER BY apps.id",
            &[&development_email, &hosted_email, &STALE_EPHEMERAL_DAYS],
        )
        .await?;
    for row in rows {
        let app_id: Uuid = row.get(0);
        if is_system_app(app_id) {
            continue;
        }
        findings.push(AuditFinding {
            severity: AuditSeverity::Warn,
            check: "app-stale-ephemeral",
            app_id: Some(app_id),
            detail: format!(
                "the ephemeral app is older than {STALE_EPHEMERAL_DAYS} days and should have been swept"
            ),
        });
    }
    Ok(())
}

fn is_system_app(app_id: Uuid) -> bool {
    app_id == INSTANT_SYSTEM_CATALOG_APP_ID || app_id == INSTANT_HOMEPAGE_APP_ID
}

fn default_rule_allows_everything(rules: &Value) -> bool {
    let Some(allow) = rules
        .get("$default")
        .and_then(|default| default.get("allow"))
    else {
        return false;
    };
    match allow {
        Value::Bool(allowed) => *allowed,
        Value::String(rule) => rule == "true",
        Value::Object(actions) => actions.values().any(rule_allows_everything),
        _ => false,
    }
}

fn rule_allows_everything(rule: &Value) -> bool {
    match rule {
        Value::Bool(allowed) => *allowed,
        Value::String(rule) => rule == "true",
        _ => false,
    }
}
