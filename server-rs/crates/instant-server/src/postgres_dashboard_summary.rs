use std::sync::Arc;

use chrono::{DateTime, Utc};
use instant_http::ApiError;
use serde_json::{Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardSummaryProvider, DashboardUser,
    INSTANT_CONFIG_APP_ID, PostgresPool,
};

const FREE_TEAMS_CUTOFF: &str = "2026-03-01T12:00:00Z";

/// PostgreSQL implementation of the compound JVM `GET /dash` payload.
pub struct PostgresDashboardSummaryProvider<H> {
    client: PostgresClientSource,
    hooks: Arc<H>,
    superuser_email: Option<String>,
}

impl<H> PostgresDashboardSummaryProvider<H> {
    #[must_use]
    pub fn new(client: Client, hooks: Arc<H>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            hooks,
            superuser_email: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, hooks: Arc<H>) -> Self {
        Self {
            client: PostgresClientSource::pooled_read_only(pool),
            hooks,
            superuser_email: None,
        }
    }

    /// Treat the configured deployment operator as the human superuser. The
    /// config app stays owned by the system catalog user, so its creator is not
    /// a usable operator identity.
    #[must_use]
    pub fn with_superuser_email(mut self, email: Option<String>) -> Self {
        self.superuser_email = email.map(|value| value.trim().to_ascii_lowercase());
        self
    }

    async fn enrich_apps(
        &self,
        user_id: Uuid,
        raw_apps: Vec<(Uuid, Value)>,
    ) -> Result<Vec<Value>, ApiError>
    where
        H: DashboardControlHooks,
    {
        let mut apps = Vec::with_capacity(raw_apps.len());
        for (app_id, app) in raw_apps {
            apps.push(
                self.hooks
                    .execute(DashboardControlHookOperation::EnrichApp {
                        app_id,
                        user_id: Some(user_id),
                        app,
                    })
                    .await?,
            );
        }
        Ok(apps)
    }
}

impl<H> DashboardSummaryProvider for PostgresDashboardSummaryProvider<H>
where
    H: DashboardControlHooks,
{
    async fn summary(&self, user: DashboardUser) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let cutoff = FREE_TEAMS_CUTOFF
            .parse::<DateTime<Utc>>()
            .expect("the fixed JVM free-team cutoff is valid");
        let app_rows = client
            .query(
                "SELECT DISTINCT app.id, app.creator_id, app.org_id, app.title,
                        app.created_at, coalesce(app.status, 'active'),
                        app.magic_code_expiry_minutes, token.token,
                        coalesce(app.creator_id = $1
                         OR coalesce(member.member_role, '') IN ('admin', 'owner'),
                         false) AS can_see_admin_token
                 FROM apps app
                 JOIN app_admin_tokens token ON token.app_id = app.id
                 LEFT JOIN app_members member
                   ON member.app_id = app.id AND member.user_id = $1
                 LEFT JOIN instant_subscriptions subscription
                   ON subscription.id = app.subscription_id
                 WHERE app.deletion_marked_at IS NULL AND app.org_id IS NULL
                   AND (app.creator_id = $1 OR (
                     member.user_id IS NOT NULL AND (
                       $2 OR subscription.subscription_type_id = 2 OR member.created_at < $3
                     )
                   ))
                 ORDER BY app.created_at, app.id",
                &[&user.id, &false, &cutoff],
            )
            .await
            .map_err(database_error)?;
        let mut raw_apps = Vec::with_capacity(app_rows.len());
        for row in app_rows {
            let app_id = row.get::<_, Uuid>(0);
            let own_status = row.get::<_, String>(5);
            let mut app = json!({
                "id": app_id,
                "creator_id": row.get::<_, Option<Uuid>>(1),
                "org_id": row.get::<_, Option<Uuid>>(2),
                "title": row.get::<_, String>(3),
                "created_at": row.get::<_, chrono::NaiveDateTime>(4),
                "status": own_status,
                "effective_status": own_status,
                "magic_code_expiry_minutes": row.get::<_, Option<i32>>(6),
            });
            if row.get::<_, bool>(8) {
                app["admin_token"] = json!(row.get::<_, Uuid>(7));
            }
            raw_apps.push((app_id, app));
        }
        let orgs = load_orgs(&client, user.id, false, cutoff).await?;
        let profile = client
            .query_opt(
                "SELECT id, meta, created_at FROM instant_profiles WHERE id = $1",
                &[&user.id],
            )
            .await
            .map_err(database_error)?
            .map(|row| {
                json!({
                    "id": row.get::<_, Uuid>(0),
                    "meta": row.get::<_, Value>(1),
                    "created_at": row.get::<_, chrono::NaiveDateTime>(2),
                })
            });
        let invites = load_pending_invites(&client, &user.email).await?;
        let superuser = match self.superuser_email.as_deref() {
            Some(email) => email.eq_ignore_ascii_case(user.email.trim()),
            None => client
                .query_opt(
                    "SELECT 1 FROM apps WHERE id = $1 AND creator_id = $2",
                    &[&INSTANT_CONFIG_APP_ID, &user.id],
                )
                .await
                .map_err(database_error)?
                .is_some(),
        };
        drop(client);
        let apps = self.enrich_apps(user.id, raw_apps).await?;
        let mut response = json!({
            "apps": apps,
            "superuser": superuser,
            "orgs": orgs,
            "profile": profile,
            "invites": invites,
            "user": {"id": user.id, "email": user.email},
        });
        if superuser {
            response["instant_config_app_id"] = json!(INSTANT_CONFIG_APP_ID);
        }
        Ok(response)
    }
}

async fn load_orgs(
    client: &Client,
    user_id: Uuid,
    paid_features_free: bool,
    cutoff: DateTime<Utc>,
) -> Result<Vec<Value>, ApiError> {
    client
        .query(
            "WITH direct AS (
               SELECT org.id, org.title, org.created_at, org.updated_at,
                      member.role, coalesce(subscription.subscription_type_id = 3, false) paid
               FROM orgs org
               JOIN org_members member ON member.org_id = org.id AND member.user_id = $1
               LEFT JOIN instant_subscriptions subscription ON subscription.id = org.subscription_id
             ), app_membered AS (
               SELECT DISTINCT ON (org.id)
                      org.id, org.title, org.created_at, org.updated_at,
                      'app-member'::text role,
                      coalesce(org_subscription.subscription_type_id = 3, false) paid
               FROM orgs org
               JOIN apps app ON app.org_id = org.id AND app.deletion_marked_at IS NULL
               JOIN app_members member ON member.app_id = app.id AND member.user_id = $1
               LEFT JOIN instant_subscriptions org_subscription ON org_subscription.id = org.subscription_id
               LEFT JOIN instant_subscriptions app_subscription ON app_subscription.id = app.subscription_id
               WHERE $2 OR member.created_at < $3
                  OR org_subscription.subscription_type_id = 3
                  OR app_subscription.subscription_type_id = 2
             )
             SELECT * FROM direct
             UNION ALL
             SELECT * FROM app_membered WHERE id NOT IN (SELECT id FROM direct)
             ORDER BY created_at, id",
            &[&user_id, &paid_features_free, &cutoff],
        )
        .await
        .map_err(database_error)
        .map(|rows| {
            rows.into_iter()
                .map(|row| {
                    json!({
                        "id": row.get::<_, Uuid>(0),
                        "title": row.get::<_, String>(1),
                        "created_at": row.get::<_, DateTime<Utc>>(2),
                        "updated_at": row.get::<_, DateTime<Utc>>(3),
                        "role": row.get::<_, String>(4),
                        "paid": row.get::<_, bool>(5),
                    })
                })
                .collect()
        })
}

async fn load_pending_invites(client: &Client, email: &str) -> Result<Vec<Value>, ApiError> {
    client
        .query(
            "SELECT 'app', invite.id, invite.invitee_role, app.id, app.title, users.email
             FROM app_member_invites invite
             JOIN apps app ON app.id = invite.app_id
             JOIN instant_users users ON users.id = invite.inviter_id
             WHERE invite.invitee_email = $1 AND invite.status = 'pending'
               AND app.deletion_marked_at IS NULL
               AND invite.sent_at >= now() - interval '3 days'
             UNION ALL
             SELECT 'org', invite.id, invite.invitee_role, org.id, org.title, users.email
             FROM org_member_invites invite
             JOIN orgs org ON org.id = invite.org_id
             JOIN instant_users users ON users.id = invite.inviter_id
             WHERE invite.invitee_email = $1 AND invite.status = 'pending'
               AND invite.sent_at >= now() - interval '3 days'",
            &[&email],
        )
        .await
        .map_err(database_error)
        .map(|rows| {
            rows.into_iter()
                .map(|row| {
                    json!({
                        "type": row.get::<_, String>(0),
                        "id": row.get::<_, Uuid>(1),
                        "invitee_role": row.get::<_, String>(2),
                        "foreign_key": row.get::<_, Uuid>(3),
                        "title": row.get::<_, String>(4),
                        "inviter_email": row.get::<_, String>(5),
                    })
                })
                .collect()
        })
}
