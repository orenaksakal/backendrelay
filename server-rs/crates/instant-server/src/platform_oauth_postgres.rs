use base64::Engine as _;
use chrono::{DateTime, Utc};
use instant_auth::{generate_platform_access_token, generate_platform_refresh_token};
use instant_http::{
    ApiError, ApiErrorType, PlatformOAuthClaimRequest, PlatformOAuthDecisionRequest,
    PlatformOAuthStartRequest, PlatformOAuthTokenRequest,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, GenericClient, Row, Transaction};
use url::Url;
use uuid::Uuid;

use crate::PostgresPool;
use crate::dashboard_refresh_token::{LOAD_USER_SQL, hash_token};
use crate::db_error::database_error_pg as database_error;
use crate::platform_oauth::PlatformOAuthStore;
use crate::postgres_pool::PostgresClientSource;

pub struct PostgresPlatformOAuthStore {
    client: PostgresClientSource,
}

impl PostgresPlatformOAuthStore {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
        }
    }
}

impl PlatformOAuthStore for PostgresPlatformOAuthStore {
    async fn create_redirect(
        &self,
        redirect_id: Uuid,
        cookie: Uuid,
        request: PlatformOAuthStartRequest,
    ) -> Result<(), ApiError> {
        let client = self.client.acquire().await.map_err(pool_error)?;
        let row = client
            .query_opt(
                "SELECT c.authorized_redirect_urls, a.is_public, a.granted_scopes
                 FROM instant_oauth_app_clients c
                 JOIN instant_oauth_apps a ON a.id = c.oauth_app_id
                 WHERE c.client_id = $1",
                &[&request.client_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-app-client"))?;
        let redirects = row.get::<_, Option<Vec<String>>>(0).unwrap_or_default();
        if !redirects.contains(&request.redirect_uri) {
            return Err(validation(
                "The redirect_uri does not appear in the authorized redirect URIs.",
            ));
        }
        let is_public: bool = row.get(1);
        if is_public {
            let granted = row.get::<_, Vec<String>>(2);
            if request.scopes.iter().any(|scope| !granted.contains(scope)) {
                return Err(validation(
                    "This OAuth app has not been granted all requested scopes.",
                ));
            }
        }
        validate_redirect(&request.redirect_uri, !is_public)?;
        client
            .execute(
                "INSERT INTO instant_oauth_app_redirects
                   (lookup_key, client_id, state, cookie, redirect_uri, scopes,
                    code_challenge_method, code_challenge, status, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                         'init'::instant_oauth_app_redirect_status,
                         now() + interval '10 minutes')",
                &[
                    &hash_uuid(redirect_id),
                    &request.client_id,
                    &request.state,
                    &cookie,
                    &request.redirect_uri,
                    &request.scopes,
                    &request.code_challenge_method,
                    &request.code_challenge,
                ],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }

    async fn claim_redirect(&self, request: PlatformOAuthClaimRequest) -> Result<Value, ApiError> {
        let token = Uuid::parse_str(&request.bearer_token)
            .map_err(|_| record_not_found("token").unauthorized())?;
        let client = self.client.acquire().await.map_err(pool_error)?;
        let user = client
            .query_opt(LOAD_USER_SQL, &[&hash_token(token)])
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("token").unauthorized())?;
        let user_id: Uuid = user.get(0);
        let grant_token = Uuid::new_v4();
        let redirect = client
            .query_opt(
                "UPDATE instant_oauth_app_redirects SET
                   user_id = $2, status = 'claimed', grant_token = $3
                 WHERE lookup_key = $1 AND user_id IS NULL AND status = 'init'
                 RETURNING client_id, redirect_uri, scopes, grant_token, expires_at",
                &[&hash_uuid(request.redirect_id), &user_id, &grant_token],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-app-redirect"))?;
        ensure_not_expired(&redirect, 4, "oauth-app-redirect")?;
        let client_id: Uuid = redirect.get(0);
        let app = client
            .query_one(
                "SELECT a.id, a.app_id, a.app_name, a.is_public, a.support_email,
                        a.app_privacy_policy_link, a.app_logo, a.app_tos_link,
                        a.app_home_page
                 FROM instant_oauth_app_clients c
                 JOIN instant_oauth_apps a ON a.id = c.oauth_app_id
                 WHERE c.client_id = $1",
                &[&client_id],
            )
            .await
            .map_err(database_error)?;
        let redirect_uri: String = redirect.get(1);
        let host = Url::parse(&redirect_uri)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned));
        let private_or_local = !app.get::<_, bool>(3) || host.as_deref() == Some("localhost");
        if private_or_local && !is_app_member(&*client, app.get(1), user_id).await? {
            client
                .execute(
                    "DELETE FROM instant_oauth_app_redirects WHERE lookup_key = $1",
                    &[&hash_uuid(request.redirect_id)],
                )
                .await
                .map_err(database_error)?;
            return Err(permission(
                "Only members of this app may use this OAuth redirect.",
            ));
        }
        let logo = app
            .get::<_, Option<Vec<u8>>>(6)
            .as_deref()
            .map(encode_logo)
            .transpose()?;
        Ok(json!({
            "appName": app.get::<_, String>(2),
            "userEmail": user.get::<_, String>(1),
            "supportEmail": app.get::<_, Option<String>>(4),
            "appPrivacyPolicyLink": app.get::<_, Option<String>>(5),
            "appLogo": logo,
            "appTosLink": app.get::<_, Option<String>>(7),
            "appHomePage": app.get::<_, Option<String>>(8),
            "redirectOrigin": host,
            "scopes": redirect.get::<_, Vec<String>>(2),
            "grantToken": redirect.get::<_, Uuid>(3),
        }))
    }

    async fn grant_redirect(
        &self,
        request: PlatformOAuthDecisionRequest,
        code: Uuid,
    ) -> Result<String, ApiError> {
        let mut client = self.client.acquire().await.map_err(pool_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let redirect = consume_redirect(&transaction, request.redirect_id).await?;
        transaction.commit().await.map_err(database_error)?;
        if redirect.get::<_, Option<Uuid>>(9) != Some(request.grant_token) {
            return Err(record_not_found("oauth-app-redirect"));
        }
        ensure_not_expired(&redirect, 8, "oauth-app-redirect")?;

        let result = async {
            let cookie = request.cookie.ok_or_else(|| missing("Missing cookie."))?;
            if redirect.get::<_, Uuid>(2) != cookie {
                return Err(missing("Invalid cookie."));
            }
            let transaction = client.transaction().await.map_err(database_error)?;
            transaction
                .execute(
                    "INSERT INTO instant_oauth_app_codes
                       (hashed_code, client_id, redirect_uri, user_id, scopes,
                        code_challenge_method, code_challenge, expires_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, now() + interval '10 minutes')",
                    &[
                        &hash_uuid(code),
                        &redirect.get::<_, Uuid>(0),
                        &redirect.get::<_, String>(3),
                        &redirect.get::<_, Uuid>(5),
                        &redirect.get::<_, Vec<String>>(4),
                        &redirect.get::<_, Option<String>>(6),
                        &redirect.get::<_, Option<String>>(7),
                    ],
                )
                .await
                .map_err(database_error)?;
            let url = oauth_redirect_url(
                &redirect.get::<_, String>(3),
                &[
                    ("code", code.to_string()),
                    ("state", redirect.get::<_, String>(1)),
                    ("scope", redirect.get::<_, Vec<String>>(4).join(" ")),
                ],
            )?;
            transaction.commit().await.map_err(database_error)?;
            Ok(url)
        }
        .await;

        match result {
            Ok(url) => Ok(url),
            Err(error) => oauth_grant_error_redirect(
                &redirect.get::<_, String>(3),
                &redirect.get::<_, String>(1),
                &error,
            ),
        }
    }

    async fn deny_redirect(
        &self,
        request: PlatformOAuthDecisionRequest,
    ) -> Result<String, ApiError> {
        let mut client = self.client.acquire().await.map_err(pool_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let redirect = consume_redirect(&transaction, request.redirect_id).await?;
        transaction.commit().await.map_err(database_error)?;
        let cookie = request.cookie.ok_or_else(|| missing("cookie"))?;
        if redirect.get::<_, Uuid>(2) != cookie {
            return Err(missing("Invalid cookie"));
        }
        let url = oauth_redirect_url(
            &redirect.get::<_, String>(3),
            &[
                ("error", "access_denied".to_owned()),
                ("state", redirect.get::<_, String>(1)),
            ],
        )?;
        Ok(url)
    }

    async fn exchange_token(&self, request: PlatformOAuthTokenRequest) -> Result<Value, ApiError> {
        let mut client = self.client.acquire().await.map_err(pool_error)?;
        let transaction = client.transaction().await.map_err(database_error)?;
        let confidential = if let Some(secret) = request.client_secret.as_deref() {
            authenticate_client(&transaction, request.client_id, secret).await?;
            true
        } else {
            false
        };
        let response = if request.grant_type == "refresh_token" {
            let refresh = request
                .refresh_token
                .as_deref()
                .ok_or_else(|| missing("refresh_token"))?;
            let row = transaction
                .query_opt(
                    "SELECT lookup_key, user_id, scopes FROM instant_user_oauth_refresh_tokens
                     WHERE lookup_key = $1 AND client_id = $2",
                    &[&hash_string(refresh), &request.client_id],
                )
                .await
                .map_err(database_error)?
                .ok_or_else(|| record_not_found("oauth-refresh-token"))?;
            let mut response = issue_access_token(
                &transaction,
                request.client_id,
                row.get(1),
                row.get(2),
                Some(row.get(0)),
            )
            .await?;
            let scope = response
                .as_object_mut()
                .and_then(|object| object.remove("scopes"))
                .unwrap_or(Value::String(String::new()));
            response["scope"] = scope;
            response
        } else {
            let code = request.code.ok_or_else(|| missing("code"))?;
            let row = transaction
                .query_opt(
                    "DELETE FROM instant_oauth_app_codes WHERE hashed_code = $1
                     RETURNING client_id, redirect_uri, user_id, scopes,
                               code_challenge_method, code_challenge, expires_at",
                    &[&hash_uuid(code)],
                )
                .await
                .map_err(database_error)?
                .ok_or_else(|| record_not_found("oauth-code"))?;
            transaction.commit().await.map_err(database_error)?;
            ensure_not_expired(&row, 6, "oauth-code")?;
            if row.get::<_, Uuid>(0) != request.client_id {
                return Err(malformed("Invalid client_id parameter"));
            }
            if row.get::<_, String>(1) != request.redirect_uri.clone().unwrap_or_default() {
                return Err(malformed("Invalid redirect_uri parameter"));
            }
            let transaction = client.transaction().await.map_err(database_error)?;
            if confidential {
                let response =
                    issue_token_pair(&transaction, request.client_id, row.get(2), row.get(3))
                        .await?;
                transaction.commit().await.map_err(database_error)?;
                return Ok(response);
            }
            verify_pkce(
                row.get::<_, Option<String>>(4).as_deref(),
                row.get::<_, Option<String>>(5).as_deref(),
                request.code_verifier.as_deref(),
            )?;
            let response = issue_access_token(
                &transaction,
                request.client_id,
                row.get(2),
                row.get(3),
                None,
            )
            .await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(response);
        };
        transaction.commit().await.map_err(database_error)?;
        Ok(response)
    }

    async fn access_token_info(&self, access_token: String) -> Result<Value, ApiError> {
        let client = self.client.acquire().await.map_err(pool_error)?;
        let row = client
            .query_opt(
                "SELECT scopes, expires_at FROM instant_user_oauth_access_tokens
                 WHERE lookup_key = $1",
                &[&hash_string(&access_token)],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| record_not_found("oauth-access-token"))?;
        ensure_not_expired(&row, 1, "oauth-access-token")?;
        Ok(token_info_json(&row.get::<_, Vec<String>>(0), row.get(1)))
    }

    async fn revoke_token(&self, token: String) -> Result<(), ApiError> {
        let client = self.client.acquire().await.map_err(pool_error)?;
        let table = if token.starts_with("prt_") {
            "instant_user_oauth_refresh_tokens"
        } else {
            "instant_user_oauth_access_tokens"
        };
        client
            .execute(
                &format!("DELETE FROM {table} WHERE lookup_key = $1"),
                &[&hash_string(&token)],
            )
            .await
            .map_err(database_error)?;
        Ok(())
    }
}

async fn consume_redirect(
    transaction: &Transaction<'_>,
    redirect_id: Uuid,
) -> Result<Row, ApiError> {
    let row = transaction
        .query_opt(
            "DELETE FROM instant_oauth_app_redirects WHERE lookup_key = $1
             RETURNING client_id, state, cookie, redirect_uri, scopes, user_id,
                       code_challenge_method, code_challenge, expires_at, grant_token",
            &[&hash_uuid(redirect_id)],
        )
        .await
        .map_err(database_error)?
        .ok_or_else(|| record_not_found("oauth-app-redirect"))?;
    Ok(row)
}

async fn authenticate_client(
    client: &Transaction<'_>,
    client_id: Uuid,
    secret: &str,
) -> Result<(), ApiError> {
    if client
        .query_opt(
            "SELECT 1 FROM instant_oauth_app_clients c
             JOIN instant_oauth_app_client_secrets s ON s.client_id = c.client_id
             WHERE c.client_id = $1 AND s.hashed_secret = $2",
            &[&client_id, &hash_string(secret)],
        )
        .await
        .map_err(database_error)?
        .is_some()
    {
        Ok(())
    } else {
        Err(record_not_found("oauth-app-client"))
    }
}

async fn issue_token_pair(
    client: &Transaction<'_>,
    client_id: Uuid,
    user_id: Uuid,
    scopes: Vec<String>,
) -> Result<Value, ApiError> {
    let refresh = generate_platform_refresh_token();
    let refresh_hash = hash_string(&refresh);
    client
        .execute(
            "INSERT INTO instant_user_oauth_refresh_tokens
               (lookup_key, client_id, user_id, scopes) VALUES ($1, $2, $3, $4)",
            &[&refresh_hash, &client_id, &user_id, &scopes],
        )
        .await
        .map_err(database_error)?;
    let mut value =
        issue_access_token(client, client_id, user_id, scopes, Some(refresh_hash)).await?;
    value["refresh_token"] = Value::String(refresh);
    client
        .execute(
            "DELETE FROM instant_user_oauth_refresh_tokens WHERE lookup_key IN (
               SELECT lookup_key FROM instant_user_oauth_refresh_tokens
               WHERE client_id = $1 AND user_id = $2
               ORDER BY created_at DESC OFFSET 5
             )",
            &[&client_id, &user_id],
        )
        .await
        .map_err(database_error)?;
    Ok(value)
}

async fn issue_access_token<C>(
    client: &C,
    client_id: Uuid,
    user_id: Uuid,
    scopes: Vec<String>,
    refresh_hash: Option<Vec<u8>>,
) -> Result<Value, ApiError>
where
    C: GenericClient + Sync,
{
    let access = generate_platform_access_token();
    let expires_at = Utc::now() + chrono::Duration::weeks(2);
    client
        .execute(
            "INSERT INTO instant_user_oauth_access_tokens
               (lookup_key, refresh_token_lookup_key, client_id, user_id, scopes, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &hash_string(&access),
                &refresh_hash,
                &client_id,
                &user_id,
                &scopes,
                &expires_at,
            ],
        )
        .await
        .map_err(database_error)?;
    Ok(json!({
        "access_token": access,
        "expires_in": 14 * 24 * 60 * 60,
        "token_type": "Bearer",
        "scopes": scopes.join(" "),
    }))
}

async fn is_app_member<C>(client: &C, app_id: Uuid, user_id: Uuid) -> Result<bool, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_opt(
            "SELECT 1 FROM apps a WHERE a.id = $1 AND (
               a.creator_id = $2
               OR EXISTS (SELECT 1 FROM app_members m WHERE m.app_id = a.id AND m.user_id = $2)
               OR EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = a.org_id AND m.user_id = $2)
             )",
            &[&app_id, &user_id],
        )
        .await
        .map(|row| row.is_some())
        .map_err(database_error)
}

fn verify_pkce(
    method: Option<&str>,
    challenge: Option<&str>,
    verifier: Option<&str>,
) -> Result<(), ApiError> {
    let method = method.ok_or_else(|| missing("code_challenge_method"))?;
    let challenge = challenge.ok_or_else(|| missing("code_challenge"))?;
    let verifier = verifier.ok_or_else(|| missing("code_verifier"))?;
    let actual = if method == "S256" {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    } else {
        verifier.to_owned()
    };
    if constant_eq(actual.as_bytes(), challenge.as_bytes()) {
        Ok(())
    } else {
        Err(malformed("Invalid code_verifier"))
    }
}

fn validate_redirect(input: &str, allow_localhost: bool) -> Result<(), ApiError> {
    let url = Url::parse(input).map_err(|_| malformed("Invalid redirect_uri"))?;
    let host = url.host_str().unwrap_or_default();
    let localhost = matches!(host, "localhost" | "127.0.0.1");
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path().contains("/..")
        || url.path().contains("\\.")
        || (localhost && !allow_localhost)
        || (!localhost && url.scheme() != "https")
        || (localhost && !matches!(url.scheme(), "http" | "https"))
    {
        Err(validation("Invalid redirect_uri"))
    } else {
        Ok(())
    }
}

fn oauth_redirect_url(base: &str, values: &[(&str, String)]) -> Result<String, ApiError> {
    let mut url = Url::parse(base).map_err(|_| malformed("Invalid redirect_uri"))?;
    url.query_pairs_mut().extend_pairs(values.iter().cloned());
    Ok(url.into())
}

fn oauth_grant_error_redirect(
    redirect_uri: &str,
    state: &str,
    error: &ApiError,
) -> Result<String, ApiError> {
    let error_code = match error.body().kind {
        ApiErrorType::ParamMissing | ApiErrorType::ParamMalformed => "invalid_request",
        _ => "server_error",
    };
    oauth_redirect_url(
        redirect_uri,
        &[
            ("error", error_code.to_owned()),
            ("error_description", error.body().message.clone()),
            ("state", state.to_owned()),
        ],
    )
}

fn encode_logo(bytes: &[u8]) -> Result<String, ApiError> {
    if bytes.len() < 4 {
        return Err(unknown("Invalid stored OAuth app logo"));
    }
    let mime = std::str::from_utf8(&bytes[..4])
        .map_err(|_| unknown("Invalid stored OAuth app logo"))?
        .trim();
    Ok(format!(
        "data:image/{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes[4..])
    ))
}

fn token_info_json(scopes: &[String], expires_at: DateTime<Utc>) -> Value {
    json!({
        "expires_in": (expires_at - Utc::now()).num_seconds().max(0),
        "token_type": "Bearer",
        "scopes": scopes.join(" "),
    })
}

fn ensure_not_expired(row: &Row, index: usize, kind: &'static str) -> Result<(), ApiError> {
    if row.get::<_, DateTime<Utc>>(index) > Utc::now() {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorType::RecordExpired,
            format!("Expired {kind}"),
        ))
    }
}

fn hash_uuid(value: Uuid) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn hash_string(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

#[allow(clippy::needless_pass_by_value)]
fn pool_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::log_database_error(&error.to_string());
    ApiError::new(ApiErrorType::Unknown, "Database unavailable")
}

fn record_not_found(kind: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Record not found: {kind}"),
    )
}

fn malformed(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, message)
}

fn validation(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn permission(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::PermissionDenied, message)
}

fn missing(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ParamMissing, message)
}

fn unknown(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::Unknown, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_errors_match_oauth_callback_contract() {
        assert_eq!(
            oauth_grant_error_redirect(
                "https://client.test/callback?existing=value",
                "opaque state",
                &missing("Missing cookie."),
            )
            .unwrap(),
            "https://client.test/callback?existing=value&error=invalid_request&error_description=Missing+cookie.&state=opaque+state"
        );
        assert_eq!(
            oauth_grant_error_redirect(
                "https://client.test/callback",
                "original-state",
                &unknown("Code issuance failed"),
            )
            .unwrap(),
            "https://client.test/callback?error=server_error&error_description=Code+issuance+failed&state=original-state"
        );
    }
}
