use std::future::Future;
use std::sync::Arc;

use instant_auth::{AuthError, CreateUser};
use instant_http::ApiError;
use instant_instaql::{AttributeCatalog, system_attr_id};
use instant_permissions::{
    EvaluationBindings, EvaluationItem, InMemoryRateLimiter, PermissionAction,
    PermissionDataLoader, RateLimitConsumer, RefCache, evaluate_batch_async,
};
use serde_json::{Map, Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::admin_data::{internal_error, load_rules};
use crate::admin_metadata::load_attribute_catalog;
use crate::postgres_pool::PostgresClientSource;
use crate::runtime_auth::RuntimeAuthPolicy;
use crate::runtime_oauth::RuntimeOAuthPolicy;

const SHARED_OAUTH_USER_LIMIT: i64 = 100;

/// Production runtime OAuth policy backed by current PostgreSQL schema/rules.
pub struct PostgresRuntimeOAuthPolicy {
    client: PostgresClientSource,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
    shared_oauth_user_limit: i64,
}

impl PostgresRuntimeOAuthPolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            rate_limit_consumer: Arc::new(InMemoryRateLimiter::default()),
            shared_oauth_user_limit: SHARED_OAUTH_USER_LIMIT,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            rate_limit_consumer: Arc::new(InMemoryRateLimiter::default()),
            shared_oauth_user_limit: SHARED_OAUTH_USER_LIMIT,
        }
    }

    #[must_use]
    pub fn with_rate_limit_consumer(mut self, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        self.rate_limit_consumer = consumer;
        self
    }

    #[must_use]
    pub const fn with_shared_oauth_user_limit(mut self, limit: i64) -> Self {
        self.shared_oauth_user_limit = limit;
        self
    }

    async fn authorize_user(
        &self,
        app_id: Uuid,
        user: &CreateUser,
        origin: Option<&str>,
    ) -> Result<(), AuthError> {
        let mut client = self
            .client
            .acquire()
            .await
            .map_err(|error| policy_evaluation(&error))?;
        let catalog = load_attribute_catalog(&client, app_id)
            .await
            .map_err(|error| policy_api_error(&error))?;
        let rules = load_rules(&client, app_id, None)
            .await
            .map_err(|error| policy_api_error(&error))?;
        let program = rules
            .program("$users", PermissionAction::Create)
            .map_err(|error| policy_evaluation(&error))?
            .expect("$users create always has a system fallback");
        let mut bindings = signup_bindings(user, origin);
        rules
            .configure_rate_limits(app_id, &mut bindings, Arc::clone(&self.rate_limit_consumer))
            .map_err(|error| policy_evaluation(&error))?;
        let transaction = client.transaction().await.map_err(AuthError::Postgres)?;
        let loader = instant_permissions::PostgresPermissionDataLoader {
            app_id,
            catalog: &catalog,
        };
        let allowed = evaluate_batch_async(
            &[EvaluationItem {
                program: &program,
                bindings: &bindings,
            }],
            &mut RefCache::new(),
            |requests| loader.load_refs(&transaction, requests),
        )
        .await
        .map_err(|error| policy_evaluation(&error))?[0];
        transaction.rollback().await.map_err(AuthError::Postgres)?;
        if allowed {
            Ok(())
        } else {
            Err(AuthError::SignupPermissionDenied)
        }
    }

    async fn authorize_shared(&self, app_id: Uuid) -> Result<(), AuthError> {
        let id_attr = system_attr_id("$users", "id")?;
        let client = self
            .client
            .acquire()
            .await
            .map_err(|error| policy_evaluation(&error))?;
        let at_limit = client
            .query_one(
                "SELECT count(*) >= $3
                   FROM (
                     SELECT 1
                       FROM triples
                      WHERE app_id = $1 AND attr_id = $2
                      LIMIT $3
                   ) users",
                &[&app_id, &id_attr, &self.shared_oauth_user_limit],
            )
            .await?
            .get::<_, bool>(0);
        if at_limit {
            let limit = u64::try_from(self.shared_oauth_user_limit).unwrap_or_default();
            Err(AuthError::SharedOAuthUserLimit(limit))
        } else {
            Ok(())
        }
    }
}

impl RuntimeOAuthPolicy for PostgresRuntimeOAuthPolicy {
    async fn load_catalog(&self, app_id: Uuid) -> Result<AttributeCatalog, ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        load_attribute_catalog(&client, app_id).await
    }

    fn authorize_signup(
        &self,
        app_id: Uuid,
        user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        let user = user.clone();
        async move { self.authorize_user(app_id, &user, None).await }
    }

    fn authorize_shared_credentials(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        self.authorize_shared(app_id)
    }
}

impl RuntimeAuthPolicy for PostgresRuntimeOAuthPolicy {
    async fn load_catalog(&self, app_id: Uuid) -> Result<AttributeCatalog, ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        load_attribute_catalog(&client, app_id).await
    }

    fn authorize_signup(
        &self,
        app_id: Uuid,
        user: &CreateUser,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        let user = user.clone();
        async move { self.authorize_user(app_id, &user, None).await }
    }
    async fn authorize_signup_with_origin(
        &self,
        app_id: Uuid,
        user: &CreateUser,
        origin: Option<&str>,
    ) -> Result<(), AuthError> {
        self.authorize_user(app_id, user, origin).await
    }
}

fn signup_bindings(user: &CreateUser, origin: Option<&str>) -> EvaluationBindings {
    let data = user_data(user);
    EvaluationBindings {
        data: data.clone(),
        data_etype: Some("$users".to_owned()),
        auth: data.clone(),
        new_data: Some(data),
        request: instant_permissions::RequestBinding {
            origin: origin.map(str::to_owned),
            ..instant_permissions::RequestBinding::default()
        },
        ..EvaluationBindings::default()
    }
}

fn user_data(user: &CreateUser) -> Value {
    let mut data = Map::new();
    data.insert("id".to_owned(), json!(user.id));
    if let Some(email) = &user.email {
        data.insert("email".to_owned(), Value::String(email.clone()));
    }
    data.extend(user.extra_fields.clone());
    Value::Object(data)
}

fn policy_api_error(error: &ApiError) -> AuthError {
    AuthError::SignupPermissionEvaluation(error.body().message.clone())
}

fn policy_evaluation(error: &impl std::fmt::Display) -> AuthError {
    AuthError::SignupPermissionEvaluation(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_permissions::RuleSet;
    use std::collections::BTreeMap;

    #[test]
    fn signup_origin_allowlist_receives_the_http_origin() {
        let user = CreateUser {
            id: Uuid::new_v4(),
            email: Some("person@example.com".into()),
            user_type: Some("user".into()),
            image_url: None,
            extra_fields: BTreeMap::new(),
        };
        let rule = instant_permissions::RuleProgram::compile(PermissionAction::Create,
            "data.email != null && request.origin in ['https://example.com', 'http://localhost:3107']").unwrap();
        let bindings = signup_bindings(&user, Some("http://localhost:3107"));
        assert!(rule.evaluate_bool(&bindings).unwrap());
        assert!(
            !rule
                .evaluate_bool(&signup_bindings(&user, Some("https://untrusted.example")))
                .unwrap()
        );
        assert!(rule.evaluate_bool(&signup_bindings(&user, None)).is_err());
    }

    #[test]
    fn candidate_binding_matches_the_jvm_signup_shape() {
        let id = Uuid::new_v4();
        let value = user_data(&CreateUser {
            id,
            email: Some("person@example.com".to_owned()),
            user_type: Some("user".to_owned()),
            image_url: Some("https://example.com/avatar.png".to_owned()),
            extra_fields: BTreeMap::from([("handle".to_owned(), json!("instant"))]),
        });
        assert_eq!(
            value,
            json!({"id": id, "email": "person@example.com", "handle": "instant"})
        );
    }

    #[test]
    fn policy_errors_have_stable_auth_classification() {
        let rules = RuleSet::new(json!({"$users": {"allow": {"create": "false"}}}));
        let program = rules
            .program("$users", PermissionAction::Create)
            .unwrap()
            .unwrap();
        let bindings = EvaluationBindings::default();
        assert!(!program.evaluate_bool(&bindings).unwrap());
    }
}
