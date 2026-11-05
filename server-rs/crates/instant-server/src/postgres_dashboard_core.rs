use instant_http::ApiError;
use instant_instaql::SYSTEM_CATALOG_APP_ID;
use serde_json::{Value, json};
use tokio_postgres::{Client, GenericClient};
use uuid::Uuid;

use crate::admin_metadata::{ATTRIBUTE_COLUMNS, row_to_attribute};
use crate::db_error::{database_error, internal_error};
use crate::postgres_pool::PostgresClientSource;
use crate::{DashboardControlHookOperation, DashboardControlHooks, PostgresPool};

/// PostgreSQL implementations for dashboard hooks that mutate Instant's core
/// application data, composed around the remaining provider hooks.
pub struct PostgresDashboardCoreHooks<D> {
    client: PostgresClientSource,
    inner: D,
}

impl<D> PostgresDashboardCoreHooks<D> {
    #[must_use]
    pub fn new(client: Client, inner: D) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), inner)
    }

    /// Uses a reconnecting pool for dashboard core reads and mutations.
    #[must_use]
    pub fn from_pool(pool: PostgresPool, inner: D) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool), inner)
    }

    pub(crate) fn from_source(client: PostgresClientSource, inner: D) -> Self {
        Self { client, inner }
    }

    async fn clear_app(&self, app_id: uuid::Uuid) -> Result<Value, ApiError> {
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        transaction
            .execute("INSERT INTO transactions (app_id) VALUES ($1)", &[&app_id])
            .await
            .map_err(database_error)?;
        transaction
            .query(
                "WITH target_attrs AS (
                   SELECT app_id, id, etype
                   FROM attrs
                   WHERE app_id = $1 AND deletion_marked_at IS NULL
                 ), snapshots AS (
                   SELECT target.app_id, target.id,
                          (SELECT id_attr.id
                           FROM attrs id_attr
                           WHERE (id_attr.app_id = target.app_id OR id_attr.app_id = $2)
                             AND id_attr.etype = target.etype
                             AND id_attr.label = 'id'
                           LIMIT 1) AS id_attr_id
                   FROM target_attrs target
                 ), deleted_attrs AS (
                   UPDATE attrs AS attr
                   SET deletion_marked_at = now(),
                       is_indexed = false,
                       is_required = false,
                       metadata = coalesce(attr.metadata, '{}'::jsonb) || jsonb_build_object(
                         'soft_delete_snapshot', jsonb_build_object(
                           'is_indexed', attr.is_indexed,
                           'is_required', attr.is_required,
                           'id_attr_id', snapshots.id_attr_id
                         )
                       ),
                       etype = attr.id::text || '_deleted$' || attr.etype,
                       label = attr.id::text || '_deleted$' || attr.label,
                       reverse_etype = CASE WHEN attr.reverse_etype IS NULL THEN NULL
                         ELSE attr.id::text || '_deleted$' || attr.reverse_etype END,
                       reverse_label = CASE WHEN attr.reverse_label IS NULL THEN NULL
                         ELSE attr.id::text || '_deleted$' || attr.reverse_label END
                   FROM snapshots
                   WHERE attr.app_id = snapshots.app_id AND attr.id = snapshots.id
                     AND attr.deletion_marked_at IS NULL
                   RETURNING attr.forward_ident, attr.reverse_ident,
                             attr.etype, attr.label, attr.reverse_etype, attr.reverse_label
                 ), forward_idents AS (
                   UPDATE idents AS ident
                   SET etype = attr.etype, label = attr.label
                   FROM deleted_attrs attr
                   WHERE ident.app_id = $1 AND ident.id = attr.forward_ident
                   RETURNING ident.id
                 ), reverse_idents AS (
                   UPDATE idents AS ident
                   SET etype = attr.reverse_etype, label = attr.reverse_label
                   FROM deleted_attrs attr
                   WHERE ident.app_id = $1 AND ident.id = attr.reverse_ident
                   RETURNING ident.id
                 )
                 SELECT (SELECT count(*) FROM deleted_attrs)::bigint,
                        (SELECT count(*) FROM forward_idents)::bigint,
                        (SELECT count(*) FROM reverse_idents)::bigint",
                &[&app_id, &SYSTEM_CATALOG_APP_ID],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO rules (app_id, code) VALUES ($1, $2)
                 ON CONFLICT (app_id) DO UPDATE SET code = excluded.code",
                &[&app_id, &json!({})],
            )
            .await
            .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(Value::Bool(true))
    }

    async fn soft_deleted_attributes(&self, app_id: Uuid) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let statement = format!(
            "SELECT {ATTRIBUTE_COLUMNS}
             FROM attrs
             WHERE app_id = $1 AND deletion_marked_at IS NOT NULL
             ORDER BY id"
        );
        let attributes = client
            .query(&statement, &[&app_id])
            .await
            .map_err(database_error)?
            .iter()
            .map(row_to_attribute)
            .collect::<Result<Vec<_>, _>>()?;
        serde_json::to_value(attributes).map_err(attribute_encoding_error)
    }

    async fn enrich_app(
        &self,
        app_id: Uuid,
        user_id: Option<Uuid>,
        app: Value,
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        enrich_app(&*client, app_id, user_id, app).await
    }

    #[allow(clippy::too_many_lines)] // One org projection query and its JSON shape.
    async fn org_details(
        &self,
        org_id: Uuid,
        user_id: Uuid,
        mut org: Value,
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let access = client
            .query_one(
                "SELECT coalesce(member.role, 'app-member'),
                        coalesce(subscription.subscription_type_id = 3, false),
                        member.user_id IS NOT NULL
                 FROM orgs org
                 LEFT JOIN org_members member
                   ON member.org_id = org.id AND member.user_id = $2
                 LEFT JOIN instant_subscriptions subscription ON subscription.id = org.subscription_id
                 WHERE org.id = $1 AND (
                   member.user_id IS NOT NULL OR EXISTS (
                     SELECT 1 FROM apps app
                     JOIN app_members app_member ON app_member.app_id = app.id
                     WHERE app.org_id = org.id AND app.deletion_marked_at IS NULL
                       AND app_member.user_id = $2
                   )
                 )",
                &[&org_id, &user_id],
            )
            .await
            .map_err(database_error)?;
        let direct_org_member = access.get::<_, bool>(2);
        let org_role = access.get::<_, String>(0);
        let org_admin = matches!(org_role.as_str(), "admin" | "owner" | "creator");
        merge_object(
            &mut org,
            &json!({
                "role": org_role,
                "paid": access.get::<_, bool>(1),
            }),
        )?;
        let rows = client
            .query(
                "SELECT app.id, app.creator_id, app.org_id, app.title, app.created_at,
                         coalesce(app.status, 'active'), app.magic_code_expiry_minutes, token.token,
                         coalesce(app.creator_id = $2
                          OR coalesce(member.member_role, '') IN ('admin', 'owner')
                          OR $4, false) AS can_see_admin_token
                  FROM apps app
                  JOIN app_admin_tokens token ON token.app_id = app.id
                  LEFT JOIN app_members member
                    ON member.app_id = app.id AND member.user_id = $2
                  WHERE app.org_id = $1 AND app.deletion_marked_at IS NULL
                    AND ($3 OR member.user_id IS NOT NULL)
                  ORDER BY app.created_at, app.id",
                &[&org_id, &user_id, &direct_org_member, &org_admin],
            )
            .await
            .map_err(database_error)?;
        let mut apps = Vec::with_capacity(rows.len());
        for row in rows {
            let app_id = row.get::<_, Uuid>(0);
            let mut app = json!({
                "id": app_id,
                "creator_id": row.get::<_, Option<Uuid>>(1),
                "org_id": row.get::<_, Option<Uuid>>(2),
                "title": row.get::<_, String>(3),
                "created_at": row.get::<_, chrono::NaiveDateTime>(4),
                "status": row.get::<_, String>(5),
                "magic_code_expiry_minutes": row.get::<_, Option<i32>>(6),
            });
            if row.get::<_, bool>(8) {
                app["admin_token"] = json!(row.get::<_, Uuid>(7));
            }
            apps.push(enrich_app(&*client, app_id, Some(user_id), app).await?);
        }
        let members = if direct_org_member {
            query_json(
                &*client,
                "SELECT coalesce(jsonb_agg(jsonb_build_object(
                       'id', member.id, 'email', users.email, 'role', member.role,
                       'created_at', member.created_at
                      ) ORDER BY member.id), '[]'::jsonb)
              FROM org_members member
              JOIN instant_users users ON users.id = member.user_id
              WHERE member.org_id = $1",
                org_id,
            )
            .await?
        } else {
            json!([])
        };
        let invites = if direct_org_member {
            query_json(
                &*client,
                "SELECT coalesce(jsonb_agg(jsonb_build_object(
                       'id', invite.id, 'email', invite.invitee_email,
                        'role', invite.invitee_role, 'status', invite.status,
                        'sent_at', invite.sent_at,
                       'expired', invite.sent_at < now() - interval '3 days'
                     ) ORDER BY invite.id), '[]'::jsonb)
              FROM org_member_invites invite
              WHERE invite.org_id = $1",
                org_id,
            )
            .await?
        } else {
            json!([])
        };
        Ok(json!({"org": org, "apps": apps, "members": members, "invites": invites}))
    }
}

impl<D> DashboardControlHooks for PostgresDashboardCoreHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::ClearApp { app_id } => self.clear_app(app_id).await,
            DashboardControlHookOperation::SoftDeletedAttributes { app_id } => {
                self.soft_deleted_attributes(app_id).await
            }
            DashboardControlHookOperation::EnrichApp {
                app_id,
                user_id,
                app,
            } => self.enrich_app(app_id, user_id, app).await,
            DashboardControlHookOperation::OrgDetails {
                org_id,
                user_id,
                org,
            } => self.org_details(org_id, user_id, org).await,
            other => self.inner.execute(other).await,
        }
    }
}

async fn enrich_app<C>(
    client: &C,
    app_id: Uuid,
    user_id: Option<Uuid>,
    mut app: Value,
) -> Result<Value, ApiError>
where
    C: GenericClient + Sync,
{
    let row = client
        .query_one(
            "SELECT jsonb_build_object(
               'rules', rules.code,
               'rules_version', rules.version,
               'org', CASE WHEN org.id IS NULL THEN NULL ELSE jsonb_build_object('id', org.id, 'title', org.title) END,
               'pro', coalesce(subscription.subscription_type_id = 2, false),
               'user_app_role', CASE
                 WHEN app.creator_id = $2 THEN 'owner'
                 ELSE (SELECT member_role FROM app_members WHERE app_id = app.id AND user_id = $2)
               END,
               'members', coalesce((
                 SELECT jsonb_agg(jsonb_build_object('id', member.id, 'email', users.email, 'role', member.member_role) ORDER BY member.id)
                 FROM app_members member JOIN instant_users users ON users.id = member.user_id
                 WHERE member.app_id = app.id
               ), '[]'::jsonb),
               'invites', coalesce((
                 SELECT jsonb_agg(jsonb_build_object(
                   'id', invite.id, 'email', invite.invitee_email, 'role', invite.invitee_role,
                   'status', invite.status, 'sent_at', invite.sent_at,
                   'expired', invite.sent_at < now() - interval '3 days'
                 ) ORDER BY invite.id)
                 FROM app_member_invites invite WHERE invite.app_id = app.id
               ), '[]'::jsonb),
               'webhooks', coalesce((
                 SELECT jsonb_agg(jsonb_build_object(
                   'id', webhook.id, 'sink', webhook.sink,
                   'namespaces', coalesce((SELECT jsonb_agg(DISTINCT attr.etype ORDER BY attr.etype)
                     FROM attrs attr WHERE (attr.app_id = webhook.app_id OR attr.app_id = $3)
                       AND attr.id = ANY(webhook.id_attr_ids)), '[]'::jsonb),
                   'actions', to_jsonb(webhook.actions), 'status', webhook.status,
                   'disabled_reason', webhook.disabled_reason,
                   'created_at', webhook.created_at, 'updated_at', webhook.updated_at
                 ) ORDER BY webhook.id)
                 FROM webhooks webhook WHERE webhook.app_id = app.id
               ), '[]'::jsonb),
               'magic_code_email_template', CASE WHEN template.id IS NULL THEN NULL ELSE jsonb_build_object(
                 'id', template.id, 'subject', template.subject, 'body', template.body,
                 'name', template.name, 'email', sender.email
               ) END
             ),
             (SELECT org_member.role
              FROM org_members org_member
              WHERE org_member.org_id = app.org_id AND org_member.user_id = $2)
             FROM apps app
             LEFT JOIN rules ON rules.app_id = app.id
             LEFT JOIN orgs org ON org.id = app.org_id
             LEFT JOIN instant_subscriptions subscription ON subscription.id = app.subscription_id
             LEFT JOIN app_email_templates template ON template.app_id = app.id AND template.email_type = 'magic-code'
             LEFT JOIN app_email_senders sender ON sender.id = template.sender_id
             WHERE app.id = $1",
            &[&app_id, &user_id, &SYSTEM_CATALOG_APP_ID],
        )
        .await
        .map_err(database_error)?;
    let enrichment: Value = row.get(0);
    let organization_role: Option<String> = row.get(1);
    merge_object(&mut app, &enrichment)?;
    let role = effective_app_role(
        app.get("user_app_role").and_then(Value::as_str),
        organization_role.as_deref(),
    );
    app["role"] = json!(role);
    Ok(app)
}

/// Collapses the stored app and organization roles into the dashboard's
/// effective role for an app: the higher of the two.
fn effective_app_role(app_role: Option<&str>, organization_role: Option<&str>) -> &'static str {
    fn rank(role: Option<&str>) -> u8 {
        match role {
            Some("owner" | "creator") => 3,
            Some("admin") => 2,
            Some("collaborator") => 1,
            _ => 0,
        }
    }

    match rank(app_role).max(rank(organization_role)) {
        3 => "owner",
        2 => "admin",
        1 => "collaborator",
        _ => "app-member",
    }
}

async fn query_json<C>(client: &C, sql: &str, id: Uuid) -> Result<Value, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_one(sql, &[&id])
        .await
        .map(|row| row.get(0))
        .map_err(database_error)
}

fn merge_object(target: &mut Value, enrichment: &Value) -> Result<(), ApiError> {
    let target = target
        .as_object_mut()
        .ok_or_else(|| database_error("Dashboard projection target is not an object"))?;
    let enrichment = enrichment
        .as_object()
        .ok_or_else(|| database_error("Dashboard projection is not an object"))?;
    target.extend(enrichment.clone());
    Ok(())
}

fn attribute_encoding_error(error: serde_json::Error) -> ApiError {
    internal_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_encoding_errors_hide_the_serializer_cause() {
        let error = attribute_encoding_error(serde_json::Error::io(std::io::Error::other(
            "failed encoding attribute value",
        )));

        assert_eq!(error.body().kind, instant_http::ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");
    }

    #[test]
    fn effective_app_role_takes_the_higher_app_or_organization_role() {
        let cases = [
            (None, None, "app-member"),
            (Some("collaborator"), None, "collaborator"),
            (Some("admin"), None, "admin"),
            (Some("owner"), None, "owner"),
            (Some("creator"), None, "owner"),
            (None, Some("collaborator"), "collaborator"),
            (None, Some("admin"), "admin"),
            (None, Some("owner"), "owner"),
            (None, Some("creator"), "owner"),
            (Some("collaborator"), Some("collaborator"), "collaborator"),
            (Some("collaborator"), Some("admin"), "admin"),
            (Some("collaborator"), Some("owner"), "owner"),
            (Some("admin"), Some("collaborator"), "admin"),
            (Some("admin"), Some("owner"), "owner"),
            (Some("owner"), Some("admin"), "owner"),
            (Some("unknown"), Some("unknown"), "app-member"),
        ];
        for (app_role, organization_role, expected) in cases {
            assert_eq!(
                effective_app_role(app_role, organization_role),
                expected,
                "app {app_role:?} and organization {organization_role:?}"
            );
        }
    }
}
