use chrono::{DateTime, NaiveDateTime, Utc};
use instant_http::{ApiError, ApiErrorType, SuperadminContext, SuperadminOperation};
use instant_permissions::RuleSet;
use serde_json::{Value, json};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, Row};
use uuid::Uuid;

use crate::PostgresPool;
use crate::dashboard_control::{
    DashboardControlHookOperation, DashboardControlHooks, apply_schema_plan,
    attributes_to_dashboard_schema, load_dashboard_attributes, schema_push_plan,
};
use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::superadmin::{SuperadminPrincipal, SuperadminStore};

/// Durable PostgreSQL implementation of all fourteen durable superadmin operations.
///
/// Production supplies a dedicated bounded pool: several operations release
/// their persistence lease before invoking hooks that may use the shared
/// request pool, and dropped persistence connections are recreated instead of
/// terminating the process.
pub struct PostgresSuperadminStore<H> {
    client: PostgresClientSource,
    hooks: H,
}

impl<H> PostgresSuperadminStore<H> {
    #[must_use]
    pub fn new(client: Client, hooks: H) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            hooks,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, hooks: H) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            hooks,
        }
    }
}

impl<H> SuperadminStore for PostgresSuperadminStore<H>
where
    H: DashboardControlHooks,
{
    async fn execute(
        &self,
        principal: SuperadminPrincipal,
        _context: SuperadminContext,
        operation: SuperadminOperation,
    ) -> Result<Value, ApiError> {
        match operation {
            SuperadminOperation::ListApps { include } => {
                self.list_apps(required_user(principal)?, None, &include)
                    .await
            }
            SuperadminOperation::ListOrgs => self.list_orgs(required_user(principal)?).await,
            SuperadminOperation::ListOrgApps { org_id, include } => {
                self.list_apps(required_user(principal)?, Some(org_id), &include)
                    .await
            }
            SuperadminOperation::CreateApp { body } => {
                self.create_app(required_user(principal)?, body).await
            }
            SuperadminOperation::GetApp { app_id } => self.get_app(principal, app_id).await,
            SuperadminOperation::UpdateApp { app_id, body } => self.update_app(app_id, &body).await,
            SuperadminOperation::SendTransfer { app_id, body } => {
                self.send_transfer(required_user(principal)?, app_id, &body)
                    .await
            }
            SuperadminOperation::RevokeTransfer { app_id, body } => {
                self.revoke_transfer(required_user(principal)?, app_id, &body)
                    .await
            }
            SuperadminOperation::GetSchema { app_id } => self.get_schema(app_id).await,
            SuperadminOperation::PlanSchema { app_id, body } => {
                self.schema_plan(app_id, &body, false).await
            }
            SuperadminOperation::ApplySchema { app_id, body } => {
                self.schema_plan(app_id, &body, true).await
            }
            SuperadminOperation::GetPerms { app_id } => self.get_permissions(app_id).await,
            SuperadminOperation::PutPerms { app_id, body } => {
                self.put_permissions(app_id, &body).await
            }
            SuperadminOperation::DeleteApp { app_id } => {
                self.delete_app(required_user(principal)?, app_id).await
            }
            SuperadminOperation::Runtime { .. } | SuperadminOperation::Explain { .. } => {
                Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "Operator diagnostics are not part of the durable store",
                ))
            }
        }
    }
}

fn required_user(principal: SuperadminPrincipal) -> Result<Uuid, ApiError> {
    principal
        .user_id
        .ok_or_else(|| ApiError::new(ApiErrorType::PermissionDenied, "User token required"))
}

impl<H> PostgresSuperadminStore<H>
where
    H: DashboardControlHooks,
{
    async fn list_apps(
        &self,
        user_id: Uuid,
        org_id: Option<Uuid>,
        include: &[String],
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let rows = if let Some(org_id) = org_id {
            client
                .query(
                    "SELECT a.id, a.creator_id, a.org_id, a.title, a.created_at,
                            a.status, a.magic_code_expiry_minutes, t.token,
                            coalesce(a.creator_id = $2
                             OR EXISTS (SELECT 1 FROM app_members m
                                        WHERE m.app_id = a.id AND m.user_id = $2
                                          AND m.member_role IN ('admin', 'owner'))
                             OR EXISTS (SELECT 1 FROM org_members m
                                        WHERE m.org_id = a.org_id AND m.user_id = $2
                                          AND m.role IN ('admin', 'owner', 'creator')),
                             false) AS can_see_admin_token
                     FROM apps a
                     JOIN app_admin_tokens t ON t.app_id = a.id
                     WHERE a.org_id = $1 AND a.deletion_marked_at IS NULL
                       AND (EXISTS (SELECT 1 FROM org_members m
                                    WHERE m.org_id = a.org_id AND m.user_id = $2)
                            OR EXISTS (SELECT 1 FROM app_members m
                                       WHERE m.app_id = a.id AND m.user_id = $2))
                     ORDER BY a.created_at, a.id",
                    &[&org_id, &user_id],
                )
                .await
        } else {
            client
                .query(
                    "SELECT a.id, a.creator_id, a.org_id, a.title, a.created_at,
                            a.status, a.magic_code_expiry_minutes, t.token
                     FROM apps a
                     JOIN app_admin_tokens t ON t.app_id = a.id
                     WHERE a.creator_id = $1 AND a.deletion_marked_at IS NULL
                     ORDER BY a.created_at, a.id",
                    &[&user_id],
                )
                .await
        }
        .map_err(database_error)?;
        let mut apps = Vec::with_capacity(rows.len());
        for row in rows {
            let app_id = row.get(0);
            let mut app = if org_id.is_some() && !row.get::<_, bool>(8) {
                app_json_without_token(&row)
            } else {
                app_json(&row)
            };
            enrich_app(&client, app_id, include, &mut app).await?;
            apps.push(app);
        }
        Ok(json!({"apps": apps}))
    }

    async fn list_orgs(&self, user_id: Uuid) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let orgs = client
            .query(
                "SELECT o.id, o.title, o.created_at, o.updated_at, m.role
                 FROM orgs o JOIN org_members m ON m.org_id = o.id
                 WHERE m.user_id = $1 ORDER BY o.created_at, o.id",
                &[&user_id],
            )
            .await
            .map_err(database_error)?
            .iter()
            .map(|row| {
                json!({
                    "id": row.get::<_, Uuid>(0),
                    "title": row.get::<_, String>(1),
                    "created_at": row.get::<_, DateTime<Utc>>(2),
                    "updated_at": row.get::<_, DateTime<Utc>>(3),
                    "role": row.get::<_, String>(4),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"orgs": orgs}))
    }

    async fn create_app(&self, user_id: Uuid, body: Value) -> Result<Value, ApiError> {
        let title = required_string(&body, "title")?;
        let org_id = optional_uuid(&body, "org_id")?;
        let rules = body.get("perms").cloned();
        if let Some(rules) = &rules {
            validate_rules(rules)?;
        }
        let app_id = Uuid::new_v4();
        if crate::is_reserved_app_id(app_id) {
            return Err(ApiError::new(
                instant_http::ApiErrorType::ValidationFailed,
                "This app ID is reserved for deployment configuration",
            ));
        }
        let admin_token = Uuid::new_v4();
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        if let Some(org_id) = org_id {
            require_org_admin(&transaction, user_id, org_id).await?;
        }
        let creator_id = org_id.is_none().then_some(user_id);
        let row = transaction
            .query_one(
                "INSERT INTO apps (id, creator_id, org_id, title)
                 VALUES ($1, $2, $3, $4)
                 RETURNING id, creator_id, org_id, title, created_at, status,
                           magic_code_expiry_minutes",
                &[&app_id, &creator_id, &org_id, &title],
            )
            .await
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
                &[&admin_token, &app_id],
            )
            .await
            .map_err(database_error)?;
        if let Some(rules) = &rules {
            transaction
                .execute(
                    "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
                    &[&app_id, &Json(rules)],
                )
                .await
                .map_err(database_error)?;
        }
        let mut app = app_json_without_token(&row);
        app["admin_token"] = json!(admin_token);
        transaction.commit().await.map_err(database_error)?;

        if let Some(schema) = body.get("schema") {
            let attributes = load_dashboard_attributes(&*client, app_id).await?;
            let plan = schema_push_plan(&attributes, schema, true, false)?;
            let steps = plan["steps"]
                .as_array()
                .expect("schema plan steps are an array");
            let (_, jobs) =
                apply_schema_plan(&mut client, app_id, &attributes, steps, false).await?;
            drop(client);
            self.enqueue_jobs(jobs).await?;
        } else {
            drop(client);
        }
        self.hooks
            .execute(DashboardControlHookOperation::AfterCreate {
                app_id,
                body: body.clone(),
            })
            .await?;
        let schema = self.schema_value(app_id).await?;
        Ok(json!({
            "app": merge_fields(app, &json!({"perms": rules, "schema": schema}))
        }))
    }

    async fn get_app(
        &self,
        principal: SuperadminPrincipal,
        app_id: Uuid,
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = load_app(&client, app_id).await?;
        let app = if can_see_admin_token(&client, principal, app_id).await? {
            app_json(&row)
        } else {
            app_json_without_token(&row)
        };
        Ok(json!({"app": app}))
    }

    async fn update_app(&self, app_id: Uuid, body: &Value) -> Result<Value, ApiError> {
        let title = required_string(body, "title")?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_opt(
                "UPDATE apps SET title = $1
                 WHERE id = $2 AND deletion_marked_at IS NULL
                 RETURNING id, creator_id, org_id, title, created_at, status,
                           magic_code_expiry_minutes,
                           (SELECT token FROM app_admin_tokens WHERE app_id = $2)",
                &[&title, &app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("app"))?;
        Ok(json!({"app": app_json(&row)}))
    }

    async fn delete_app(&self, user_id: Uuid, app_id: Uuid) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = load_app(&client, app_id).await?;
        let creator_id: Option<Uuid> = row.get(1);
        if creator_id.is_some() && creator_id != Some(user_id) {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Only the owner can delete a personally owned app",
            ));
        }
        let deleted = client
            .query_one(
                "UPDATE apps SET deletion_marked_at = now() WHERE id = $1
                 RETURNING id, creator_id, org_id, title, created_at, status,
                           magic_code_expiry_minutes,
                           (SELECT token FROM app_admin_tokens WHERE app_id = $1)",
                &[&app_id],
            )
            .await
            .map_err(database_error)?;
        drop(client);
        Ok(json!({"app": app_json(&deleted)}))
    }

    async fn send_transfer(
        &self,
        inviter_id: Uuid,
        app_id: Uuid,
        body: &Value,
    ) -> Result<Value, ApiError> {
        let email = required_string(body, "dest_email")?.to_lowercase();
        let record_id = Uuid::new_v4();
        let client = self.client.acquire().await.map_err(database_error)?;
        let context = client
            .query_one(
                "SELECT users.email, apps.title
                 FROM instant_users users CROSS JOIN apps
                 WHERE users.id = $1 AND apps.id = $2",
                &[&inviter_id, &app_id],
            )
            .await
            .map_err(database_error)?;
        let inviter_email: String = context.get(0);
        let title: String = context.get(1);
        let row = client
            .query_one(
                "INSERT INTO app_member_invites
                   (id, app_id, inviter_id, invitee_role, invitee_email, status, sent_at)
                 VALUES ($1, $2, $3, 'creator', $4, 'pending', now())
                 ON CONFLICT (app_id, invitee_email) DO UPDATE
                 SET inviter_id = EXCLUDED.inviter_id, invitee_role = 'creator',
                     status = 'pending', sent_at = now()
                 RETURNING id",
                &[&record_id, &app_id, &inviter_id, &email],
            )
            .await
            .map_err(database_error)?;
        let id: Uuid = row.get(0);
        drop(client);
        self.hooks
            .execute(DashboardControlHookOperation::DeliverInvite {
                target: "app",
                foreign_key: app_id,
                inviter_id,
                inviter_email,
                invitee_email: email,
                title,
                role: "creator".to_owned(),
            })
            .await?;
        Ok(json!({"id": id}))
    }

    async fn revoke_transfer(
        &self,
        inviter_id: Uuid,
        app_id: Uuid,
        body: &Value,
    ) -> Result<Value, ApiError> {
        let email = required_string(body, "dest_email")?.to_lowercase();
        let client = self.client.acquire().await.map_err(database_error)?;
        let count = client
            .execute(
                "UPDATE app_member_invites SET status = 'revoked'
                 WHERE app_id = $1 AND inviter_id = $2 AND invitee_email = $3
                   AND invitee_role = 'creator' AND status = 'pending'",
                &[&app_id, &inviter_id, &email],
            )
            .await
            .map_err(database_error)?;
        Ok(json!({"count": count}))
    }

    async fn get_schema(&self, app_id: Uuid) -> Result<Value, ApiError> {
        Ok(json!({"schema": self.schema_value(app_id).await?}))
    }

    async fn schema_value(&self, app_id: Uuid) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let attributes = load_dashboard_attributes(&*client, app_id).await?;
        Ok(attributes_to_dashboard_schema(&attributes))
    }

    async fn schema_plan(
        &self,
        app_id: Uuid,
        body: &Value,
        apply: bool,
    ) -> Result<Value, ApiError> {
        let schema = body
            .get("schema")
            .ok_or_else(|| missing_parameter("schema"))?;
        let check_types = clojure_truthy(body.get("check_types"));
        let background_updates = clojure_truthy(body.get("supports_background_updates"));
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let attributes = load_dashboard_attributes(&*client, app_id).await?;
        let mut plan = schema_push_plan(&attributes, schema, check_types, background_updates)?;
        if !apply {
            return Ok(plan);
        }
        let steps = plan["steps"]
            .as_array()
            .cloned()
            .expect("schema plan steps are an array");
        let (result, jobs) =
            apply_schema_plan(&mut client, app_id, &attributes, &steps, false).await?;
        plan.as_object_mut()
            .expect("schema plans are objects")
            .extend(
                result
                    .as_object()
                    .expect("apply result is an object")
                    .clone(),
            );
        drop(client);
        self.enqueue_jobs(jobs).await?;
        Ok(plan)
    }

    async fn enqueue_jobs(&self, jobs: Vec<Uuid>) -> Result<(), ApiError> {
        for job_id in jobs {
            self.hooks
                .execute(DashboardControlHookOperation::EnqueueIndexingJob { job_id })
                .await?;
        }
        Ok(())
    }

    async fn get_permissions(&self, app_id: Uuid) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let code = client
            .query_opt("SELECT code FROM rules WHERE app_id = $1", &[&app_id])
            .await
            .map_err(database_error)?
            .map(|row| row.get::<_, Json<Value>>(0).0);
        Ok(json!({"perms": code}))
    }

    async fn put_permissions(&self, app_id: Uuid, body: &Value) -> Result<Value, ApiError> {
        let code = body
            .get("code")
            .cloned()
            .ok_or_else(|| missing_parameter("code"))?;
        validate_rules(&code)?;
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_one(
                "INSERT INTO rules (app_id, code) VALUES ($1, $2)
                 ON CONFLICT (app_id) DO UPDATE
                 SET code = EXCLUDED.code, version = rules.version + 1
                 RETURNING app_id, code, version",
                &[&app_id, &Json(&code)],
            )
            .await
            .map_err(database_error)?;
        drop(client);
        Ok(json!({
            "rules": {
                "app_id": row.get::<_, Uuid>(0),
                "code": row.get::<_, Json<Value>>(1).0,
                "version": row.get::<_, i32>(2),
            }
        }))
    }
}

async fn enrich_app(
    client: &Client,
    app_id: Uuid,
    include: &[String],
    app: &mut Value,
) -> Result<(), ApiError> {
    if include.iter().any(|item| item == "schema") {
        let attributes = load_dashboard_attributes(client, app_id).await?;
        app["schema"] = attributes_to_dashboard_schema(&attributes);
    }
    if include.iter().any(|item| item == "perms") {
        app["perms"] = client
            .query_opt("SELECT code FROM rules WHERE app_id = $1", &[&app_id])
            .await
            .map_err(database_error)?
            .map_or(Value::Null, |row| row.get::<_, Json<Value>>(0).0);
    }
    Ok(())
}

async fn load_app(client: &Client, app_id: Uuid) -> Result<Row, ApiError> {
    client
        .query_opt(
            "SELECT a.id, a.creator_id, a.org_id, a.title, a.created_at,
                    a.status, a.magic_code_expiry_minutes, t.token
             FROM apps a JOIN app_admin_tokens t ON t.app_id = a.id
             WHERE a.id = $1 AND a.deletion_marked_at IS NULL",
            &[&app_id],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("app"))
}

fn app_json(row: &Row) -> Value {
    let mut app = app_json_without_token(row);
    app["admin_token"] = json!(row.get::<_, Uuid>(7));
    app
}

/// App details omit the admin token for collaborators because it grants full
/// data-plane access. Callers that administer the app, or that already hold its
/// admin token, still receive it.
async fn can_see_admin_token(
    client: &Client,
    principal: SuperadminPrincipal,
    app_id: Uuid,
) -> Result<bool, ApiError> {
    let Some(user_id) = principal.user_id else {
        return Ok(principal.admin_app_id == Some(app_id));
    };
    let row = client
        .query_opt(
            "SELECT coalesce(a.creator_id = $2
                    OR EXISTS (SELECT 1 FROM app_members m
                               WHERE m.app_id = a.id AND m.user_id = $2
                                 AND m.member_role IN ('admin', 'owner'))
                    OR EXISTS (SELECT 1 FROM org_members m
                               WHERE m.org_id = a.org_id AND m.user_id = $2
                                 AND m.role IN ('admin', 'owner', 'creator')),
                    false)
               FROM apps a
              WHERE a.id = $1 AND a.deletion_marked_at IS NULL",
            &[&app_id, &user_id],
        )
        .await
        .map_err(database_error)?;
    Ok(row.is_some_and(|row| row.get(0)))
}

fn app_json_without_token(row: &Row) -> Value {
    json!({
        "id": row.get::<_, Uuid>(0),
        "creator_id": row.get::<_, Option<Uuid>>(1),
        "org_id": row.get::<_, Option<Uuid>>(2),
        "title": row.get::<_, String>(3),
        "created_at": row.get::<_, NaiveDateTime>(4),
        "status": row.get::<_, String>(5),
        "magic_code_expiry_minutes": row.get::<_, Option<i32>>(6),
    })
}

async fn require_org_admin(
    client: &tokio_postgres::Transaction<'_>,
    user_id: Uuid,
    org_id: Uuid,
) -> Result<(), ApiError> {
    let role = client
        .query_opt(
            "SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2",
            &[&org_id, &user_id],
        )
        .await
        .map_err(database_error)?
        .map(|row| row.get::<_, String>(0));
    if matches!(role.as_deref(), Some("admin" | "owner" | "creator")) {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorType::PermissionDenied,
            "Insufficient organization member role",
        ))
    }
}

fn validate_rules(code: &Value) -> Result<(), ApiError> {
    if let Err(issues) = RuleSet::try_new(code.clone()) {
        return Err(
            ApiError::new(ApiErrorType::ValidationFailed, "Rule validation failed").with_hint(
                json!({
                    "record-type": "rule",
                    "errors": issues.into_iter().map(|issue| json!({
                        "message": issue.message,
                        "path": issue.path,
                    })).collect::<Vec<_>>()
                }),
            ),
        );
    }
    Ok(())
}

fn required_string<'a>(body: &'a Value, key: &'static str) -> Result<&'a str, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_parameter(key))
}

fn optional_uuid(body: &Value, key: &'static str) -> Result<Option<Uuid>, ApiError> {
    body.get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    ApiError::new(ApiErrorType::ParamMalformed, format!("Malformed {key}"))
                })
        })
        .transpose()
}

fn clojure_truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

fn merge_fields(mut left: Value, right: &Value) -> Value {
    left.as_object_mut()
        .expect("app projection is an object")
        .extend(
            right
                .as_object()
                .expect("extra projection is an object")
                .clone(),
        );
    left
}

fn missing_parameter(key: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing {key} parameter"),
    )
}

fn record_not_found(record: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Record not found: {record}"),
    )
}
