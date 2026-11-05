use axum::http::StatusCode;
use instant_http::{ApiError, ApiErrorType};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::superadmin::{
    AppRole, SuperadminAccess, SuperadminAuthorizer, SuperadminPrincipal, SuperadminScope,
};

/// PostgreSQL implementation of personal/platform-token authorization for the
/// superadmin API, including OAuth scopes and app/org role checks.
pub struct PostgresSuperadminAuthorizer {
    client: PostgresClientSource,
}

impl PostgresSuperadminAuthorizer {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled_read_only(pool),
        }
    }
}

impl SuperadminAuthorizer for PostgresSuperadminAuthorizer {
    async fn authorize(
        &self,
        bearer_token: String,
        access: SuperadminAccess,
    ) -> Result<SuperadminPrincipal, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        if access.allow_app_admin
            && let Some(admin_app_id) = authenticate_app_admin(&client, &bearer_token).await?
        {
            let requested_app_id = access.app.map(|(app_id, _)| app_id).ok_or_else(|| {
                ApiError::new(ApiErrorType::PermissionDenied, "User token required")
            })?;
            if admin_app_id != requested_app_id {
                return Err(ApiError::new(
                    ApiErrorType::ValidationFailed,
                    "Admin token does not belong to the requested app",
                ));
            }
            return Ok(SuperadminPrincipal {
                user_id: None,
                admin_app_id: Some(admin_app_id),
            });
        }
        let user_id = authenticate(&client, &bearer_token, access.scope).await?;
        if let Some((app_id, required_role)) = access.app {
            authorize_app_role(&client, user_id, app_id, required_role).await?;
        }
        if let Some(org_id) = access.org_id {
            authorize_org_membership(&client, user_id, org_id).await?;
        }
        Ok(SuperadminPrincipal {
            user_id: Some(user_id),
            admin_app_id: None,
        })
    }
}

async fn authenticate_app_admin(
    client: &Client,
    bearer_token: &str,
) -> Result<Option<Uuid>, ApiError> {
    let Ok(token) = Uuid::parse_str(bearer_token) else {
        return Ok(None);
    };
    client
        .query_opt(
            "SELECT app_id FROM app_admin_tokens WHERE token = $1",
            &[&token],
        )
        .await
        .map_err(database_error)
        .map(|row| row.map(|row| row.get(0)))
}

async fn authenticate(
    client: &Client,
    bearer_token: &str,
    required_scope: SuperadminScope,
) -> Result<Uuid, ApiError> {
    let digest = Sha256::digest(bearer_token.as_bytes()).to_vec();
    if bearer_token.starts_with("pat_") {
        let row = client
            .query_opt(
                "SELECT user_id, scopes FROM instant_user_oauth_access_tokens
                 WHERE lookup_key = $1 AND expires_at > now()",
                &[&digest],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(invalid_token)?;
        let scopes: Vec<String> = row.get(1);
        if !satisfies_scope(&scopes, required_scope) {
            let required = scope_name(required_scope);
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                format!("You are missing the {required} scope"),
            )
            .with_hint(json!({"required-scope": required})));
        }
        return Ok(row.get(0));
    }

    client
        .query_opt(
            "SELECT user_id FROM instant_personal_access_tokens WHERE lookup_key = $1",
            &[&digest],
        )
        .await
        .map_err(database_error)?
        .map(|row| row.get(0))
        .ok_or_else(invalid_token)
}

async fn authorize_app_role(
    client: &Client,
    user_id: Uuid,
    app_id: Uuid,
    required: AppRole,
) -> Result<(), ApiError> {
    let app = client
        .query_opt(
            "SELECT creator_id, org_id FROM apps
             WHERE id = $1 AND deletion_marked_at IS NULL",
            &[&app_id],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("app"))?;
    let creator_id: Option<Uuid> = app.get(0);
    let org_id: Option<Uuid> = app.get(1);
    let mut actual = (creator_id == Some(user_id)).then_some(AppRole::Owner);
    if let Some(row) = client
        .query_opt(
            "SELECT member_role FROM app_members WHERE app_id = $1 AND user_id = $2",
            &[&app_id, &user_id],
        )
        .await
        .map_err(database_error)?
    {
        actual = strongest(actual, parse_role(row.get::<_, String>(0).as_str()));
    }
    if let Some(org_id) = org_id
        && let Some(row) = client
            .query_opt(
                "SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2",
                &[&org_id, &user_id],
            )
            .await
            .map_err(database_error)?
    {
        actual = strongest(actual, parse_role(row.get::<_, String>(0).as_str()));
    }
    if actual.is_some_and(|actual| role_rank(actual) >= role_rank(required)) {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorType::PermissionDenied,
            "Insufficient app member role",
        )
        .with_status(StatusCode::FORBIDDEN))
    }
}

async fn authorize_org_membership(
    client: &Client,
    user_id: Uuid,
    org_id: Uuid,
) -> Result<(), ApiError> {
    if client
        .query_opt(
            "SELECT 1 FROM org_members WHERE org_id = $1 AND user_id = $2",
            &[&org_id, &user_id],
        )
        .await
        .map_err(database_error)?
        .is_some()
    {
        Ok(())
    } else {
        Err(record_not_found("org"))
    }
}

const fn scope_name(scope: SuperadminScope) -> &'static str {
    match scope {
        SuperadminScope::AppsRead => "apps-read",
        SuperadminScope::AppsWrite => "apps-write",
        SuperadminScope::AppsTransfer => "apps-transfer",
    }
}

fn satisfies_scope(scopes: &[String], required: SuperadminScope) -> bool {
    scopes.iter().any(|scope| {
        scope == scope_name(required)
            || (required == SuperadminScope::AppsRead
                && scope == scope_name(SuperadminScope::AppsWrite))
    })
}

fn parse_role(role: &str) -> Option<AppRole> {
    match role {
        "collaborator" => Some(AppRole::Collaborator),
        "admin" => Some(AppRole::Admin),
        "owner" | "creator" => Some(AppRole::Owner),
        _ => None,
    }
}

fn strongest(left: Option<AppRole>, right: Option<AppRole>) -> Option<AppRole> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if role_rank(left) >= role_rank(right) {
            left
        } else {
            right
        }),
        (role, None) | (None, role) => role,
    }
}

const fn role_rank(role: AppRole) -> u8 {
    match role {
        AppRole::Collaborator => 0,
        AppRole::Admin => 1,
        AppRole::Owner => 2,
    }
}

fn invalid_token() -> ApiError {
    record_not_found("token").unauthorized()
}

fn record_not_found(record: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Record not found: {record}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_scope_and_role_order_match_the_jvm_contract() {
        assert!(satisfies_scope(
            &["apps-write".to_owned()],
            SuperadminScope::AppsRead
        ));
        assert!(!satisfies_scope(
            &["apps-read".to_owned()],
            SuperadminScope::AppsWrite
        ));
        assert!(!satisfies_scope(
            &["apps-write".to_owned()],
            SuperadminScope::AppsTransfer
        ));
        assert_eq!(parse_role("creator"), Some(AppRole::Owner));
        assert_eq!(
            strongest(Some(AppRole::Collaborator), Some(AppRole::Admin)),
            Some(AppRole::Admin)
        );
    }
}
