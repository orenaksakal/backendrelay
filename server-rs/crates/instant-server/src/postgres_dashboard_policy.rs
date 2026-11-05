use std::collections::HashSet;

use instant_http::{ApiError, ApiErrorType};
use instant_transact::{TransactionOptions, compile_transaction, execute_transaction_in};
use serde_json::{Value, json};
use tokio_postgres::{Client, GenericClient};
use uuid::Uuid;

use crate::admin_metadata::load_attribute_catalog;
use crate::db_error::database_error;
use crate::postgres_dashboard_identity::{is_listed_email, is_superuser_email};
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardPublicConfigValues,
    DashboardSignupMode, PostgresPool, ServerEnvironment,
};

pub const INSTANT_CONFIG_APP_ID: Uuid = Uuid::from_u128(0x24a4_d71b_7bb2_4630_9aee_0114_6af2_6239);

const EPHEMERAL_DEVELOPMENT_EMAIL: &str = "ephemeral-apps-dev@backendrelay.invalid";
const EPHEMERAL_HOSTED_EMAIL: &str = "ephemeral-apps@backendrelay.invalid";
const GET_A_DB_EMAIL: &str = "get-a-db@backendrelay.invalid";
const SHARED_OAUTH_USER_LIMIT: i64 = 100;
const DEFAULT_ON_DEMAND_BACKUP_MAX_TRIPLES: i64 = 25_000_000;
const WEBHOOK_CONFIG_LOCK_NAMESPACE: i32 = 2;
const WEBHOOK_LOG_FLAG: &str = "enable-wal-entity-log-apps-map";
/// PostgreSQL-backed feature policy for special apps and dashboard signups.
/// Flags are read from the same config-app `flags` entities used by the JVM.
pub struct PostgresDashboardPolicyHooks<D> {
    client: PostgresClientSource,
    inner: D,
    environment: ServerEnvironment,
    config_app_id: Uuid,
    shared_oauth_providers: HashSet<String>,
    shared_oauth_user_limit: i64,
    self_hosted_plan_enabled: bool,
    superuser_email: Option<String>,
    dashboard_allowed_emails: Option<HashSet<String>>,
    dashboard_signup_open: bool,
    ephemeral_apps_enabled: bool,
}

impl<D> PostgresDashboardPolicyHooks<D> {
    #[must_use]
    pub fn new(client: Client, environment: ServerEnvironment, inner: D) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), environment, inner)
    }

    /// Uses a reconnecting bounded pool for dashboard policy reads and writes.
    #[must_use]
    pub fn from_pool(pool: PostgresPool, environment: ServerEnvironment, inner: D) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool), environment, inner)
    }

    pub(crate) fn from_source(
        client: PostgresClientSource,
        environment: ServerEnvironment,
        inner: D,
    ) -> Self {
        Self {
            client,
            inner,
            environment,
            config_app_id: INSTANT_CONFIG_APP_ID,
            shared_oauth_providers: HashSet::new(),
            shared_oauth_user_limit: SHARED_OAUTH_USER_LIMIT,
            self_hosted_plan_enabled: false,
            superuser_email: None,
            dashboard_allowed_emails: None,
            dashboard_signup_open: false,
            ephemeral_apps_enabled: false,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    #[must_use]
    pub fn with_shared_oauth_providers<I, S>(mut self, providers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.shared_oauth_providers = providers.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub const fn with_shared_oauth_user_limit(mut self, limit: i64) -> Self {
        self.shared_oauth_user_limit = limit;
        self
    }

    #[must_use]
    pub const fn with_self_hosted_plan_enabled(mut self, enabled: bool) -> Self {
        self.self_hosted_plan_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_superuser_email(mut self, email: Option<&str>) -> Self {
        self.superuser_email = email.map(str::to_ascii_lowercase);
        self
    }

    #[must_use]
    pub fn with_dashboard_allowed_emails(
        mut self,
        allowed_emails: Option<HashSet<String>>,
    ) -> Self {
        self.dashboard_allowed_emails = allowed_emails;
        self
    }

    #[must_use]
    pub const fn with_dashboard_signup_open(mut self, open: bool) -> Self {
        self.dashboard_signup_open = open;
        self
    }

    #[must_use]
    pub const fn with_ephemeral_apps_enabled(mut self, enabled: bool) -> Self {
        self.ephemeral_apps_enabled = enabled;
        self
    }

    async fn authorize_dashboard_admin(&self, email: &str) -> Result<Value, ApiError> {
        let normalized = email.trim().to_ascii_lowercase();
        if self.superuser_email.as_deref() == Some(normalized.as_str()) {
            return Ok(Value::Bool(true));
        }
        let client = self.client.acquire().await.map_err(database_error)?;
        let allowed = self
            .flag(&*client, "team-emails")
            .await?
            .and_then(|value| value.as_array().cloned())
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("email")
                        .and_then(Value::as_str)
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&normalized))
                })
            });
        Ok(Value::Bool(allowed))
    }

    async fn authorize_backup(
        &self,
        creator_id: Option<Uuid>,
        work_estimate: i64,
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let ephemeral_creator = self
            .creator_id(&*client, self.ephemeral_creator_email())
            .await?;
        let max_triples = self
            .flag(&*client, "on-demand-backup-max-triples")
            .await?
            .and_then(|value| value.as_i64())
            .unwrap_or_else(|| default_backup_limit(self.self_hosted_plan_enabled));
        let allowed = creator_id != Some(ephemeral_creator)
            && (max_triples <= 0 || work_estimate < max_triples);
        Ok(Value::Bool(allowed))
    }

    async fn backup_rate_limit_configuration(&self) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let app = self
            .flag(&*client, "on-demand-backup-app-rate-limit")
            .await?
            .unwrap_or(Value::Null);
        let ip = self
            .flag(&*client, "on-demand-backup-ip-rate-limit")
            .await?
            .unwrap_or(Value::Null);
        Ok(json!({"app": app, "ip": ip}))
    }

    fn ephemeral_creator_email(&self) -> &'static str {
        if self.environment == ServerEnvironment::Development {
            EPHEMERAL_DEVELOPMENT_EMAIL
        } else {
            EPHEMERAL_HOSTED_EMAIL
        }
    }

    async fn creator_id<C>(&self, client: &C, email: &str) -> Result<Uuid, ApiError>
    where
        C: GenericClient + Sync,
    {
        client
            .query_opt(
                "SELECT id FROM instant_users WHERE lower(email) = lower($1) LIMIT 1",
                &[&email],
            )
            .await
            .map_err(database_error)?
            .map(|row| row.get(0))
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorType::Unknown,
                    format!("Special app creator {email} is unavailable"),
                )
            })
    }

    async fn flag<C>(&self, client: &C, setting: &str) -> Result<Option<Value>, ApiError>
    where
        C: GenericClient + Sync,
    {
        client
            .query_opt(
                "SELECT flag_value.value
                 FROM attrs setting_attr
                 JOIN triples flag_setting
                   ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                  AND value_attr.label = 'value'
                 JOIN triples flag_value
                   ON flag_value.app_id = $1
                  AND flag_value.entity_id = flag_setting.entity_id
                  AND flag_value.attr_id = value_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                   AND setting_attr.label = 'setting'
                   AND flag_setting.value = to_jsonb($2::text)
                 LIMIT 1",
                &[&self.config_app_id, &setting],
            )
            .await
            .map_err(database_error)
            .map(|row| row.map(|row| row.get(0)))
    }

    /// Reads the public signup and ephemeral-app switches for `/dash/config`.
    ///
    /// # Errors
    ///
    /// Returns the PostgreSQL error when the dashboard flags cannot be read.
    pub async fn public_config_values(&self) -> Result<DashboardPublicConfigValues, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let signup = if self.dashboard_allowed_emails.is_some() {
            DashboardSignupMode::Restricted
        } else {
            let configuration = self.flag(&*client, "dashboard-signups").await?;
            let mode = configuration
                .as_ref()
                .and_then(|value| value.get("mode"))
                .and_then(Value::as_str)
                .map(str::trim);
            match mode {
                Some(mode) if mode.eq_ignore_ascii_case("open") => DashboardSignupMode::Open,
                Some(mode) if mode.eq_ignore_ascii_case("restricted") => {
                    DashboardSignupMode::Restricted
                }
                Some(mode) if mode.eq_ignore_ascii_case("closed") => DashboardSignupMode::Closed,
                _ if self.dashboard_signup_open => DashboardSignupMode::Open,
                _ => DashboardSignupMode::Closed,
            }
        };
        let ephemeral_apps = self
            .flag(&*client, "ephemeral-apps-enabled")
            .await?
            .and_then(|value| value.as_bool())
            .unwrap_or(self.ephemeral_apps_enabled);
        Ok(DashboardPublicConfigValues {
            signup,
            ephemeral_apps,
        })
    }

    async fn special_configuration(&self, kind: &str) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let (email, enabled) = match kind {
            "ephemeral" => (
                self.ephemeral_creator_email(),
                self.flag(&*client, "ephemeral-apps-enabled")
                    .await?
                    .and_then(|value| value.as_bool())
                    .unwrap_or(self.ephemeral_apps_enabled),
            ),
            "get-a-db" => (GET_A_DB_EMAIL, true),
            _ => {
                return Err(ApiError::new(
                    ApiErrorType::ParamMalformed,
                    "Unknown special app kind",
                ));
            }
        };
        let creator_id = self.creator_id(&*client, email).await?;
        Ok(json!({"creator_id": creator_id, "enabled": enabled}))
    }

    async fn app_creation_allowed<C>(&self, client: &C, email: &str) -> Result<bool, ApiError>
    where
        C: GenericClient + Sync,
    {
        let _ = (client, email);
        Ok(true)
    }

    async fn enable_webhook_logs(&self, app_id: Uuid) -> Result<Value, ApiError> {
        let mut client = self.client.acquire().await.map_err(database_error)?;
        let catalog = load_attribute_catalog(&client, self.config_app_id).await?;
        let Some((id_attr, setting_attr, value_attr)) = catalog
            .forward("flags", "id")
            .zip(catalog.forward("flags", "setting"))
            .zip(catalog.forward("flags", "value"))
            .map(|((id, setting), value)| (id, setting, value))
        else {
            return if matches!(
                self.environment,
                ServerEnvironment::Development | ServerEnvironment::Test
            ) {
                Ok(Value::Null)
            } else {
                Err(flags_schema_error())
            };
        };
        let raw_steps = webhook_log_raw_steps(app_id, id_attr.id, setting_attr.id, value_attr.id);
        let steps = compile_transaction(&raw_steps, &catalog).map_err(|error| {
            ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
                .with_hint(json!({"in": error.path, "original-input": raw_steps}))
        })?;
        let transaction = client.transaction().await.map_err(database_error)?;
        transaction
            .query_one(
                "SELECT pg_advisory_xact_lock($1::int4, 0::int4)",
                &[&WEBHOOK_CONFIG_LOCK_NAMESPACE],
            )
            .await
            .map_err(database_error)?;
        execute_transaction_in(
            &transaction,
            self.config_app_id,
            &catalog,
            &steps,
            TransactionOptions::default(),
        )
        .await
        .map_err(|error| {
            crate::admin_data::transaction_execution_error_with_catalog(error, Some(&catalog))
        })?;
        transaction.commit().await.map_err(database_error)?;
        Ok(Value::Null)
    }

    async fn authorize_claim(
        &self,
        current_creator_id: Option<Uuid>,
        claimant_email: &str,
    ) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let ephemeral = self
            .creator_id(&*client, self.ephemeral_creator_email())
            .await?;
        let get_a_db = self.creator_id(&*client, GET_A_DB_EMAIL).await?;
        let claimable = current_creator_id.is_some_and(|id| id == ephemeral || id == get_a_db);
        let allowed = claimable && self.app_creation_allowed(&*client, claimant_email).await?;
        Ok(Value::Bool(allowed))
    }

    async fn authorize_dashboard_signup(&self, email: &str) -> Result<Value, ApiError> {
        let normalized = email.trim().to_ascii_lowercase();
        if is_superuser_email(self.superuser_email.as_deref(), &normalized)
            || is_listed_email(self.dashboard_allowed_emails.as_ref(), &normalized)
        {
            return Ok(Value::Bool(true));
        }
        if self
            .dashboard_allowed_emails
            .as_ref()
            .is_some_and(|emails| !emails.contains(&normalized))
        {
            return Ok(Value::Bool(false));
        }
        let client = self.client.acquire().await.map_err(database_error)?;
        let configuration = self.flag(&*client, "dashboard-signups").await?;
        let mode = configuration
            .as_ref()
            .and_then(|value| value.get("mode"))
            .and_then(Value::as_str)
            .map(str::trim);
        let allowed = match mode {
            Some(mode) if mode.eq_ignore_ascii_case("closed") => false,
            Some(mode) if mode.eq_ignore_ascii_case("restricted") => configuration
                .as_ref()
                .and_then(|value| value.get("allowedEmails"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|allowed| allowed.trim().eq_ignore_ascii_case(email.trim())),
            Some(mode) if mode.eq_ignore_ascii_case("open") => true,
            _ => self.dashboard_signup_open,
        };
        Ok(Value::Bool(allowed))
    }

    async fn authorize_shared_oauth(
        &self,
        app_id: Uuid,
        provider_name: &str,
    ) -> Result<Value, ApiError> {
        if !self.shared_oauth_providers.contains(provider_name) {
            return Err(ApiError::new(
                ApiErrorType::RecordNotFound,
                "Record not found: shared-oauth-client",
            )
            .with_hint(json!({
                "record-type": "shared-oauth-client",
                "provider-name": provider_name,
            })));
        }
        let client = self.client.acquire().await.map_err(database_error)?;
        let at_limit = client
            .query_one(
                "SELECT count(*) >= $2 FROM app_users WHERE app_id = $1",
                &[&app_id, &self.shared_oauth_user_limit],
            )
            .await
            .map_err(database_error)?
            .get::<_, bool>(0);
        if at_limit {
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                format!(
                    "Shared dev credentials are limited to {} users. Please add your own client_id and client_secret in the dashboard.",
                    self.shared_oauth_user_limit
                ),
            ));
        }
        Ok(Value::Bool(true))
    }
}

impl<D> DashboardControlHooks for PostgresDashboardPolicyHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::SpecialAppConfiguration { kind } => {
                self.special_configuration(kind).await
            }
            DashboardControlHookOperation::AuthorizeClaim {
                current_creator_id,
                claimant_email,
                ..
            } => {
                self.authorize_claim(current_creator_id, &claimant_email)
                    .await
            }
            DashboardControlHookOperation::AuthorizeDashboardOAuthSignup { email } => {
                self.authorize_dashboard_signup(&email).await
            }
            DashboardControlHookOperation::AuthorizeDashboardAdmin { email } => {
                self.authorize_dashboard_admin(&email).await
            }
            DashboardControlHookOperation::AuthorizeBackup {
                creator_id,
                work_estimate,
                ..
            } => self.authorize_backup(creator_id, work_estimate).await,
            DashboardControlHookOperation::BackupRateLimitConfiguration => {
                self.backup_rate_limit_configuration().await
            }
            DashboardControlHookOperation::AuthorizeSharedOAuthCredentials {
                app_id,
                provider_name,
            } => self.authorize_shared_oauth(app_id, &provider_name).await,
            DashboardControlHookOperation::EnableWebhookLogs { app_id } => {
                self.enable_webhook_logs(app_id).await
            }
            other => self.inner.execute(other).await,
        }
    }
}

// Self-hosted operators own capacity; explicit configured limits still apply.
fn default_backup_limit(self_hosted: bool) -> i64 {
    if self_hosted {
        0
    } else {
        DEFAULT_ON_DEMAND_BACKUP_MAX_TRIPLES
    }
}

fn flags_schema_error() -> ApiError {
    ApiError::new(
        ApiErrorType::ValidationFailed,
        "The config app doesn't have the flags namespace set up.",
    )
}

fn webhook_log_raw_steps(
    app_id: Uuid,
    id_attr_id: Uuid,
    setting_attr_id: Uuid,
    value_attr_id: Uuid,
) -> Value {
    let lookup = json!([setting_attr_id, WEBHOOK_LOG_FLAG]);
    json!([
        ["add-triple", lookup, id_attr_id, lookup],
        [
            "deep-merge-triple",
            lookup,
            value_attr_id,
            {(app_id.to_string()): true}
        ]
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_backups_do_not_inherit_hosted_size_limit() {
        assert_eq!(default_backup_limit(true), 0);
        assert_eq!(default_backup_limit(false), 25_000_000);
    }

    #[test]
    fn webhook_log_enablement_matches_the_jvm_lookup_and_deep_merge_steps() {
        let app_id = Uuid::from_u128(1);
        let id_attr_id = Uuid::from_u128(2);
        let setting_attr_id = Uuid::from_u128(3);
        let value_attr_id = Uuid::from_u128(4);
        let lookup = json!([setting_attr_id, "enable-wal-entity-log-apps-map"]);
        assert_eq!(
            webhook_log_raw_steps(app_id, id_attr_id, setting_attr_id, value_attr_id),
            json!([
                ["add-triple", lookup, id_attr_id, lookup],
                [
                    "deep-merge-triple",
                    lookup,
                    value_attr_id,
                    {app_id.to_string(): true}
                ]
            ])
        );
        assert_eq!(WEBHOOK_CONFIG_LOCK_NAMESPACE, 2);
    }
}
