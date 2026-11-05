//! Runtime authentication primitives compatible with the JVM server.

pub mod oauth;
pub mod oauth_origins;
pub mod oauth_store;

use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;

use instant_instaql::{AttributeCatalog, SystemCatalogError, system_attr_id};
use instant_transact::{
    AppStatusWriteCheck, DeleteEntityStep, EntityLookup, ExecutionError, TransactionOptions,
    TransactionStep, TripleOptions, TripleStep, execute_transaction, execute_transaction_in,
    execute_transaction_with_app_status_write_check,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, GenericClient};
use uuid::Uuid;

pub const PLATFORM_REFRESH_TOKEN_PREFIX: &str = "prt_";
pub const PLATFORM_ACCESS_TOKEN_PREFIX: &str = "pat_";
pub const PERSONAL_ACCESS_TOKEN_PREFIX: &str = "per_";

/// A parsed bearer token whose debug/display output never exposes its value.
#[derive(Clone, Eq, PartialEq)]
pub enum AuthToken {
    PlatformAccess(String),
    PersonalAccess(String),
    Jwt(String),
    RuntimeRefresh(Uuid),
}

impl AuthToken {
    /// Applies the same prefix-first coercion as `instant.util.token`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::InvalidToken`] for an unrecognized non-UUID token.
    pub fn parse(value: &str) -> Result<Self, AuthError> {
        let value = value.trim();
        if value.starts_with(PLATFORM_ACCESS_TOKEN_PREFIX) {
            Ok(Self::PlatformAccess(value.to_owned()))
        } else if value.starts_with(PERSONAL_ACCESS_TOKEN_PREFIX) {
            Ok(Self::PersonalAccess(value.to_owned()))
        } else if value.starts_with("eyJ") {
            Ok(Self::Jwt(value.to_owned()))
        } else {
            value
                .parse()
                .map(Self::RuntimeRefresh)
                .map_err(|_| AuthError::InvalidToken)
        }
    }

    #[must_use]
    pub fn secret(&self) -> String {
        match self {
            Self::PlatformAccess(value) | Self::PersonalAccess(value) | Self::Jwt(value) => {
                value.clone()
            }
            Self::RuntimeRefresh(value) => value.to_string(),
        }
    }
}

impl Debug for AuthToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<secret>")
    }
}

impl Display for AuthToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<secret>")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AppUser {
    pub id: Uuid,
    pub email: Option<String>,
    pub user_type: Option<String>,
    pub image_url: Option<String>,
    pub linked_primary_user: Option<Uuid>,
    pub extra_fields: BTreeMap<String, JsonValue>,
}

impl AppUser {
    #[must_use]
    pub fn is_guest(&self) -> bool {
        self.user_type.as_deref() == Some("guest")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MagicCode {
    pub entity_id: Uuid,
    pub email: String,
    pub code: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshToken {
    pub entity_id: Uuid,
    pub token: Uuid,
    pub user_id: Uuid,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthSession {
    pub user: AppUser,
    pub refresh_token: Uuid,
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateUser {
    pub id: Uuid,
    pub email: Option<String>,
    pub user_type: Option<String>,
    pub image_url: Option<String>,
    pub extra_fields: BTreeMap<String, JsonValue>,
}

/// SHA-256 hash of the UUID's two JVM big-endian longs, encoded as lowercase hex.
#[must_use]
pub fn hash_refresh_token(token: Uuid) -> String {
    hex_digest(Sha256::digest(token.as_bytes()))
}

/// SHA-256 of the platform-default string bytes. The server runs UTF-8, which
/// is also Rust's native string encoding.
#[must_use]
pub fn hash_magic_code(code: &str) -> String {
    hex_digest(Sha256::digest(code.as_bytes()))
}

/// Generates a six-digit magic code from the full decimal alphabet.
#[must_use]
pub fn generate_magic_code() -> String {
    (0..6)
        .map(|_| char::from(b'0' + rand::random_range(0..10_u8)))
        .collect()
}

#[must_use]
pub fn generate_platform_access_token() -> String {
    generate_prefixed_secret(PLATFORM_ACCESS_TOKEN_PREFIX)
}

#[must_use]
pub fn generate_platform_refresh_token() -> String {
    generate_prefixed_secret(PLATFORM_REFRESH_TOKEN_PREFIX)
}

#[must_use]
pub fn generate_personal_access_token() -> String {
    generate_prefixed_secret(PERSONAL_ACCESS_TOKEN_PREFIX)
}

/// Constant-time comparison used for static test-user magic codes.
#[must_use]
pub fn constant_time_code_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left = left.get(index).copied().unwrap_or_default();
        let right = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

/// Constructs system-catalog transaction steps for an application user.
///
/// # Errors
///
/// Returns a system-catalog error if a built-in field ID is unavailable.
pub fn create_user_steps(
    user: &CreateUser,
    catalog: &AttributeCatalog,
) -> Result<Vec<TransactionStep>, AuthError> {
    let mut steps = vec![add(
        user.id,
        "$users",
        system_attr_id("$users", "id")?,
        json!(user.id),
    )];
    if let Some(email) = &user.email {
        steps.push(add(
            user.id,
            "$users",
            system_attr_id("$users", "email")?,
            json!(email),
        ));
    }
    if let Some(user_type) = &user.user_type {
        steps.push(add(
            user.id,
            "$users",
            system_attr_id("$users", "type")?,
            json!(user_type),
        ));
    }
    if let Some(image_url) = &user.image_url {
        steps.push(add(
            user.id,
            "$users",
            system_attr_id("$users", "imageURL")?,
            json!(image_url),
        ));
    }
    for (label, value) in &user.extra_fields {
        if matches!(
            label.as_str(),
            "id" | "email" | "type" | "imageURL" | "linkedPrimaryUser" | "linkedGuestUsers"
        ) {
            return Err(AuthError::SystemUserField(label.clone()));
        }
        let attribute = catalog
            .forward("$users", label)
            .ok_or_else(|| AuthError::UnknownUserField(label.clone()))?;
        steps.push(add(user.id, "$users", attribute.id, value.clone()));
    }
    Ok(steps)
}

/// Creates an application user through the normal transaction/WAL path.
///
/// # Errors
///
/// Returns field validation, catalog, transaction, or PostgreSQL errors.
pub async fn create_user(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user: &CreateUser,
) -> Result<AppUser, AuthError> {
    let steps = create_user_steps(user, catalog)?;
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    get_user(client, app_id, catalog, user.id)
        .await?
        .ok_or(AuthError::MissingCreatedUser(user.id))
}

/// Creates a one-time magic code entity and returns its plaintext code.
///
/// Any unused codes for the same email are invalidated in the same transaction,
/// so only the newest link can complete a sign-in.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn create_magic_code(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
    code: Option<&str>,
) -> Result<MagicCode, AuthError> {
    let code = code.map_or_else(generate_magic_code, str::to_owned);
    let entity_id = Uuid::new_v4();
    let transaction = client.transaction().await?;
    let mut steps = magic_code_delete_steps(&transaction, app_id, email).await?;
    steps.extend(magic_code_steps(entity_id, email, &code)?);
    execute_transaction_in(
        &transaction,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    transaction.commit().await?;
    Ok(MagicCode {
        entity_id,
        email: email.to_owned(),
        code,
    })
}

/// Deletes every unused magic code for an email. Used when a replacement code
/// is sent and when repeated failures lock out further guesses.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn invalidate_magic_codes(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
) -> Result<(), AuthError> {
    let steps = magic_code_delete_steps(client, app_id, email).await?;
    if steps.is_empty() {
        return Ok(());
    }
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

async fn magic_code_delete_steps<C>(
    client: &C,
    app_id: Uuid,
    email: &str,
) -> Result<Vec<TransactionStep>, AuthError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT code.entity_id
             FROM triples code
             JOIN triples email
               ON email.app_id = code.app_id AND email.entity_id = code.entity_id
             WHERE code.app_id = $1
               AND code.attr_id = $2
               AND email.attr_id = $3 AND email.value = $4::jsonb",
            &[
                &app_id,
                &system_attr_id("$magicCodes", "codeHash")?,
                &system_attr_id("$magicCodes", "email")?,
                &json!(email),
            ],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Id(row.get(0)),
                etype: Some("$magicCodes".to_owned()),
            })
        })
        .collect())
}

/// Creates a hashed refresh-token entity while returning the legacy plaintext UUID.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn create_refresh_token(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_id: Uuid,
    token: Option<Uuid>,
) -> Result<RefreshToken, AuthError> {
    let token = token.unwrap_or_else(Uuid::new_v4);
    let entity_id = Uuid::new_v4();
    let steps = refresh_token_steps(entity_id, token, user_id)?;
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(RefreshToken {
        entity_id,
        token,
        user_id,
    })
}

// Match both the predicate and expression of av_index. These built-in fields
// are unique strings; plain value equality scans every token/email in the app.
const UNIQUE_ATTRIBUTE_LOOKUP_SQL: &str = "SELECT entity_id FROM triples
 WHERE av AND app_id = $1 AND attr_id = $2 AND json_null_to_null(value) = $3::jsonb
 LIMIT 1";

// One fresh statement/snapshot instead of three sequential network exchanges.
// Keep the indexed hash lookup and app scope at every step. A LEFT JOIN retains
// malformed/missing user references so Rust preserves the previous error/None
// distinction rather than turning malformed persisted values into SQL errors.
const REFRESH_TOKEN_USER_SQL: &str = "WITH token AS MATERIALIZED (
 SELECT entity_id FROM triples
 WHERE av AND app_id = $1 AND attr_id = $2 AND json_null_to_null(value) = $3::jsonb
 LIMIT 1
), user_ref AS MATERIALIZED (
 SELECT link.value, link.entity_id FROM triples link JOIN token ON link.entity_id = token.entity_id
 WHERE link.app_id = $1 AND link.attr_id = $4 LIMIT 1
)
SELECT user_ref.value, u.attr_id, u.value, user_ref.entity_id
FROM user_ref LEFT JOIN triples u ON u.app_id = $1 AND u.entity_id =
 CASE WHEN pg_input_is_valid(user_ref.value #>> '{}', 'uuid')
      THEN (user_ref.value #>> '{}')::uuid END";

/// Looks up a user through the hash-only refresh-token storage model.
///
/// # Errors
///
/// Returns PostgreSQL, catalog, or malformed persisted-value errors.
pub async fn user_by_refresh_token<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    token: Uuid,
) -> Result<Option<AppUser>, AuthError>
where
    C: GenericClient + Sync,
{
    Ok(
        user_and_token_by_refresh_token(client, app_id, catalog, token)
            .await?
            .map(|(user, _)| user),
    )
}

/// Looks up a user and the `$userRefreshTokens` entity it was resolved through.
///
/// # Errors
///
/// Returns PostgreSQL, catalog, or malformed persisted-value errors.
pub async fn user_and_token_by_refresh_token<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    token: Uuid,
) -> Result<Option<(AppUser, Uuid)>, AuthError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query_typed(
            REFRESH_TOKEN_USER_SQL,
            &[
                (&app_id, Type::UUID),
                (
                    &system_attr_id("$userRefreshTokens", "hashedToken")?,
                    Type::UUID,
                ),
                (&json!(hash_refresh_token(token)), Type::JSONB),
                (&system_attr_id("$userRefreshTokens", "$user")?, Type::UUID),
            ],
        )
        .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let value = first.get::<_, tokio_postgres::types::Json<JsonValue>>(0).0;
    let user_id = json_uuid(&value)?;
    if first.get::<_, Option<Uuid>>(1).is_none() {
        return Ok(None);
    }
    let token_entity = first.get::<_, Uuid>(3);
    project_user(
        catalog,
        user_id,
        rows.into_iter().map(|row| {
            (
                row.get::<_, Uuid>(1),
                row.get::<_, tokio_postgres::types::Json<JsonValue>>(2).0,
            )
        }),
    )
    .map(|user| Some((user, token_entity)))
}

/// Looks up the unique `$users.email` value.
///
/// # Errors
///
/// Returns PostgreSQL, catalog, or malformed persisted-value errors.
pub async fn user_by_email<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
) -> Result<Option<AppUser>, AuthError>
where
    C: GenericClient + Sync,
{
    let row = client
        .query_typed_opt(
            UNIQUE_ATTRIBUTE_LOOKUP_SQL,
            &[
                (&app_id, Type::UUID),
                (&system_attr_id("$users", "email")?, Type::UUID),
                (&json!(email), Type::JSONB),
            ],
        )
        .await?;
    match row {
        Some(row) => get_user(client, app_id, catalog, row.get(0)).await,
        None => Ok(None),
    }
}

/// Fetches a complete built-in user projection by entity ID.
///
/// # Errors
///
/// Returns PostgreSQL, catalog, or malformed persisted-value errors.
pub async fn get_user<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_id: Uuid,
) -> Result<Option<AppUser>, AuthError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query_typed(
            "SELECT attr_id, value FROM triples
             WHERE app_id = $1 AND entity_id = $2",
            &[(&app_id, Type::UUID), (&user_id, Type::UUID)],
        )
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    project_user(
        catalog,
        user_id,
        rows.into_iter().map(|row| {
            (
                row.get::<_, Uuid>(0),
                row.get::<_, tokio_postgres::types::Json<JsonValue>>(1).0,
            )
        }),
    )
    .map(Some)
}

fn project_user(
    catalog: &AttributeCatalog,
    user_id: Uuid,
    values: impl IntoIterator<Item = (Uuid, JsonValue)>,
) -> Result<AppUser, AuthError> {
    let email_attr = system_attr_id("$users", "email")?;
    let type_attr = system_attr_id("$users", "type")?;
    let image_attr = system_attr_id("$users", "imageURL")?;
    let linked_attr = system_attr_id("$users", "linkedPrimaryUser")?;
    let mut user = AppUser {
        id: user_id,
        email: None,
        user_type: None,
        image_url: None,
        linked_primary_user: None,
        extra_fields: BTreeMap::new(),
    };
    for (attr_id, value) in values {
        if attr_id == email_attr {
            user.email = json_string(&value)?;
        } else if attr_id == type_attr {
            user.user_type = json_string(&value)?;
        } else if attr_id == image_attr {
            user.image_url = json_string(&value)?;
        } else if attr_id == linked_attr {
            user.linked_primary_user = Some(json_uuid(&value)?);
        } else if let Some(attribute) = catalog.by_id(attr_id)
            && attribute.etype == "$users"
            && attribute.label != "id"
        {
            user.extra_fields.insert(attribute.label.clone(), value);
        }
    }
    Ok(user)
}

/// Fetches complete user projections for the requested entity IDs.
///
/// Missing users are omitted and the input order is retained, matching the JVM
/// system-catalog model operation.
///
/// # Errors
///
/// Returns PostgreSQL, catalog, or malformed persisted-value errors.
pub async fn get_users<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_ids: &[Uuid],
) -> Result<Vec<AppUser>, AuthError>
where
    C: GenericClient + Sync,
{
    let mut users = Vec::with_capacity(user_ids.len());
    for user_id in user_ids {
        if let Some(user) = get_user(client, app_id, catalog, *user_id).await? {
            users.push(user);
        }
    }
    Ok(users)
}

/// Performs the JVM model's bounded `users-at-least?` existence check.
///
/// The inner limit prevents a full application-user count for large apps.
///
/// # Errors
///
/// Returns catalog or PostgreSQL errors.
pub async fn users_at_least<C>(client: &C, app_id: Uuid, count: u64) -> Result<bool, AuthError>
where
    C: GenericClient + Sync,
{
    if count == 0 {
        return Ok(true);
    }
    let count = i64::try_from(count).unwrap_or(i64::MAX);
    let row = client
        .query_one(
            "SELECT count(*)
             FROM (
                 SELECT 1 FROM triples
                 WHERE app_id = $1 AND attr_id = $2
                 LIMIT $3
             ) users",
            &[&app_id, &system_attr_id("$users", "id")?, &count],
        )
        .await?;
    Ok(row.get::<_, i64>(0) >= count)
}

/// Replaces a user's unique email through the normal transaction/WAL path.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn update_user_email(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_id: Uuid,
    email: &str,
) -> Result<Option<AppUser>, AuthError> {
    execute_transaction(
        client,
        app_id,
        catalog,
        &[add(
            user_id,
            "$users",
            system_attr_id("$users", "email")?,
            json!(email),
        )],
        system_transaction_options(),
    )
    .await?;
    get_user(client, app_id, catalog, user_id).await
}

/// Replaces the profile image URL through the normal transaction/WAL path.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn update_user_image_url(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_id: Uuid,
    image_url: &str,
) -> Result<(), AuthError> {
    execute_transaction(
        client,
        app_id,
        catalog,
        &[add(
            user_id,
            "$users",
            system_attr_id("$users", "imageURL")?,
            json!(image_url),
        )],
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

/// Marks a guest as linked to its primary user through the catalog reference.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn link_guest_user(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    primary_user_id: Uuid,
    guest_user_id: Uuid,
) -> Result<(), AuthError> {
    execute_transaction(
        client,
        app_id,
        catalog,
        &[add(
            guest_user_id,
            "$users",
            system_attr_id("$users", "linkedPrimaryUser")?,
            json!(primary_user_id),
        )],
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

/// Deletes one refresh token by its legacy plaintext UUID.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn delete_refresh_token(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    token: Uuid,
) -> Result<(), AuthError> {
    delete_entities_by_value(
        client,
        app_id,
        catalog,
        "$userRefreshTokens",
        system_attr_id("$userRefreshTokens", "hashedToken")?,
        json!(hash_refresh_token(token)),
    )
    .await
}

/// Deletes every refresh token linked to one user (sign out everywhere).
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn delete_user_refresh_tokens(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_id: Uuid,
) -> Result<(), AuthError> {
    delete_entities_by_value(
        client,
        app_id,
        catalog,
        "$userRefreshTokens",
        system_attr_id("$userRefreshTokens", "$user")?,
        json!(user_id),
    )
    .await
}

/// Deletes an application user through the normal transaction/WAL path.
///
/// The pre-delete projection is returned to match the JVM model. Catalog cascade
/// metadata controls removal of linked refresh tokens and other dependent entities.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn delete_user(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    user_id: Uuid,
) -> Result<Option<AppUser>, AuthError> {
    let Some(user) = get_user(client, app_id, catalog, user_id).await? else {
        return Ok(None);
    };
    execute_transaction(
        client,
        app_id,
        catalog,
        &[TransactionStep::DeleteEntity(DeleteEntityStep {
            entity: EntityLookup::Id(user_id),
            etype: Some("$users".to_owned()),
        })],
        system_transaction_options(),
    )
    .await?;
    Ok(Some(user))
}

/// Deletes the user selected by its unique email value.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn delete_user_by_email(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
) -> Result<Option<AppUser>, AuthError> {
    let Some(user) = user_by_email(client, app_id, catalog, email).await? else {
        return Ok(None);
    };
    delete_user(client, app_id, catalog, user.id).await
}

/// Creates an anonymous guest user and its first refresh token atomically.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn sign_in_guest(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
) -> Result<AuthSession, AuthError> {
    sign_in_guest_with_fields(client, app_id, catalog, BTreeMap::new(), |_| async {
        Ok(())
    })
    .await
}

/// Creates a guest with custom fields after running signup authorization.
///
/// The authorization callback runs before any write, matching the runtime route's
/// `$users` create-rule ordering.
///
/// # Errors
///
/// Returns authorization, field validation, catalog, transaction, or PostgreSQL errors.
pub async fn sign_in_guest_with_fields<F, Fut>(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    extra_fields: BTreeMap<String, JsonValue>,
    authorize_signup: F,
) -> Result<AuthSession, AuthError>
where
    F: FnOnce(&CreateUser) -> Fut,
    Fut: Future<Output = Result<(), AuthError>>,
{
    let user_id = Uuid::new_v4();
    let token = Uuid::new_v4();
    let token_entity = Uuid::new_v4();
    let user = CreateUser {
        id: user_id,
        email: None,
        user_type: Some("guest".to_owned()),
        image_url: None,
        extra_fields,
    };
    let mut steps = create_user_steps(&user, catalog)?;
    authorize_signup(&user).await?;
    steps.extend(refresh_token_steps(token_entity, token, user_id)?);
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(AuthSession {
        user: get_user(client, app_id, catalog, user_id)
            .await?
            .ok_or(AuthError::MissingCreatedUser(user_id))?,
        refresh_token: token,
        created: true,
    })
}

/// Verifies a magic code, creates/upgrades/links a user, and issues a refresh
/// token. The authorization callback runs before the one-time code is consumed.
///
/// # Errors
///
/// Returns authorization callback errors, invalid/expired codes, field validation,
/// catalog, transaction, or PostgreSQL errors.
#[allow(clippy::too_many_arguments)]
pub async fn verify_magic_code<F, Fut>(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
    code: &str,
    guest_user_id: Option<Uuid>,
    extra_fields: BTreeMap<String, JsonValue>,
    static_test_code: Option<&str>,
    now_ms: i64,
    expiry_minutes: u64,
    authorize_signup: F,
) -> Result<AuthSession, AuthError>
where
    F: FnOnce(&CreateUser) -> Fut,
    Fut: Future<Output = Result<(), AuthError>>,
{
    let existing = user_by_email(client, app_id, catalog, email).await?;
    let created = existing.is_none();
    let user_id = existing.as_ref().map_or_else(
        || guest_user_id.unwrap_or_else(Uuid::new_v4),
        |user| user.id,
    );
    let create = CreateUser {
        id: user_id,
        email: Some(email.to_owned()),
        user_type: Some("user".to_owned()),
        image_url: None,
        extra_fields,
    };
    if created {
        create_user_steps(&create, catalog)?;
        authorize_signup(&create).await?;
    }
    let static_matches = static_test_code
        .is_some_and(|expected| code.len() == 6 && constant_time_code_eq(expected, code));
    match consume_magic_code(client, app_id, catalog, email, code, now_ms, expiry_minutes).await {
        Ok(()) => {}
        Err(AuthError::InvalidMagicCode | AuthError::ExpiredMagicCode) if static_matches => {}
        Err(error) => return Err(error),
    }

    let refresh_token = Uuid::new_v4();
    let token_entity = Uuid::new_v4();
    let mut steps = if created {
        create_user_steps(&create, catalog)?
    } else {
        Vec::new()
    };
    if let Some(guest_id) = guest_user_id
        && guest_id != user_id
    {
        steps.push(add(
            guest_id,
            "$users",
            system_attr_id("$users", "linkedPrimaryUser")?,
            json!(user_id),
        ));
    }
    steps.extend(refresh_token_steps(token_entity, refresh_token, user_id)?);
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(AuthSession {
        user: get_user(client, app_id, catalog, user_id)
            .await?
            .ok_or(AuthError::MissingCreatedUser(user_id))?,
        refresh_token,
        created,
    })
}

async fn delete_entities_by_value(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    etype: &str,
    attr_id: Uuid,
    value: JsonValue,
) -> Result<(), AuthError> {
    let rows = client
        .query(
            "SELECT entity_id FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3::jsonb",
            &[&app_id, &attr_id, &value],
        )
        .await?;
    let steps = rows
        .into_iter()
        .map(|row| {
            TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Id(row.get(0)),
                etype: Some(etype.to_owned()),
            })
        })
        .collect::<Vec<_>>();
    if !steps.is_empty() {
        // Sign-out must remain available while an app is read-only or
        // disabled (pinned app_user_refresh_token.clj exemption).
        execute_transaction_with_app_status_write_check(
            client,
            app_id,
            catalog,
            &steps,
            system_transaction_options(),
            AppStatusWriteCheck::Skip,
        )
        .await?;
    }
    Ok(())
}

/// Atomically consumes a matching magic code. The entity is deleted before
/// expiration is reported, preserving one-time behavior for expired codes.
///
/// # Errors
///
/// Returns invalid/expired code, catalog, transaction, or PostgreSQL errors.
pub async fn consume_magic_code(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    email: &str,
    code: &str,
    now_ms: i64,
    expiry_minutes: u64,
) -> Result<(), AuthError> {
    let transaction = client.transaction().await?;
    let row = transaction
        .query_opt(
            "SELECT code.entity_id, code.created_at
             FROM triples code
             JOIN triples email
               ON email.app_id = code.app_id AND email.entity_id = code.entity_id
             WHERE code.app_id = $1
               AND code.attr_id = $2 AND code.value = $3::jsonb
               AND email.attr_id = $4 AND email.value = $5::jsonb
             LIMIT 1
             FOR UPDATE OF code",
            &[
                &app_id,
                &system_attr_id("$magicCodes", "codeHash")?,
                &json!(hash_magic_code(code)),
                &system_attr_id("$magicCodes", "email")?,
                &json!(email),
            ],
        )
        .await?;
    let Some(row) = row else {
        return Err(AuthError::InvalidMagicCode);
    };
    let entity_id = row.get::<_, Uuid>(0);
    let created_at_ms = row.get::<_, i64>(1);
    execute_transaction_in(
        &transaction,
        app_id,
        catalog,
        &[TransactionStep::DeleteEntity(DeleteEntityStep {
            entity: EntityLookup::Id(entity_id),
            etype: Some("$magicCodes".to_owned()),
        })],
        system_transaction_options(),
    )
    .await?;
    transaction.commit().await?;
    let ttl_ms = expiry_minutes
        .checked_mul(60_000)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(AuthError::ExpiryOverflow)?;
    if created_at_ms.saturating_add(ttl_ms) < now_ms {
        Err(AuthError::ExpiredMagicCode)
    } else {
        Ok(())
    }
}

fn magic_code_steps(
    entity_id: Uuid,
    email: &str,
    code: &str,
) -> Result<Vec<TransactionStep>, AuthError> {
    Ok(vec![
        add(
            entity_id,
            "$magicCodes",
            system_attr_id("$magicCodes", "id")?,
            json!(entity_id),
        ),
        add(
            entity_id,
            "$magicCodes",
            system_attr_id("$magicCodes", "codeHash")?,
            json!(hash_magic_code(code)),
        ),
        add(
            entity_id,
            "$magicCodes",
            system_attr_id("$magicCodes", "email")?,
            json!(email),
        ),
    ])
}

fn refresh_token_steps(
    entity_id: Uuid,
    token: Uuid,
    user_id: Uuid,
) -> Result<Vec<TransactionStep>, AuthError> {
    Ok(vec![
        add(
            entity_id,
            "$userRefreshTokens",
            system_attr_id("$userRefreshTokens", "id")?,
            json!(entity_id),
        ),
        add(
            entity_id,
            "$userRefreshTokens",
            system_attr_id("$userRefreshTokens", "hashedToken")?,
            json!(hash_refresh_token(token)),
        ),
        add(
            entity_id,
            "$userRefreshTokens",
            system_attr_id("$userRefreshTokens", "$user")?,
            json!(user_id),
        ),
    ])
}

fn add(entity_id: Uuid, etype: &str, attr_id: Uuid, value: JsonValue) -> TransactionStep {
    TransactionStep::AddTriple(TripleStep {
        entity: EntityLookup::Id(entity_id),
        etype: Some(etype.to_owned()),
        attr_id,
        reverse_etype: None,
        value,
        options: TripleOptions::default(),
    })
}

const fn system_transaction_options() -> TransactionOptions {
    TransactionOptions {
        allow_system_catalog_updates: true,
        admin: true,
        allow_file_metadata_updates: false,
        allow_stream_metadata_updates: false,
        overwrite_created_at: false,
        invalidator_slot_num: 0,
        wal_entity_log: instant_transact::WalEntityLog::Disabled,
    }
}

fn json_uuid(value: &JsonValue) -> Result<Uuid, AuthError> {
    value
        .as_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AuthError::InvalidPersistedValue(value.clone()))
}

fn json_string(value: &JsonValue) -> Result<Option<String>, AuthError> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| AuthError::InvalidPersistedValue(value.clone()))
    }
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;
    bytes
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

fn generate_prefixed_secret(prefix: &str) -> String {
    let mut bytes = [0_u8; 32];
    rand::fill(&mut bytes);
    format!("{prefix}{}", hex_digest(bytes))
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid authentication token")]
    InvalidToken,
    #[error("invalid magic code")]
    InvalidMagicCode,
    #[error("magic code has expired")]
    ExpiredMagicCode,
    #[error("magic-code expiry is too large")]
    ExpiryOverflow,
    #[error("invalid OAuth authorization code")]
    InvalidOAuthCode,
    #[error("OAuth authorization code has expired")]
    ExpiredOAuthCode,
    #[error("invalid OAuth redirect state")]
    InvalidOAuthRedirect,
    #[error("OAuth redirect state has expired")]
    ExpiredOAuthRedirect,
    #[error("unknown $users field `{0}`")]
    UnknownUserField(String),
    #[error("cannot set system $users field `{0}` as an extra field")]
    SystemUserField(String),
    #[error("permission denied for $users create")]
    SignupPermissionDenied,
    #[error("could not evaluate $users create permission: {0}")]
    SignupPermissionEvaluation(String),
    #[error("shared OAuth credentials are limited to {0} users")]
    SharedOAuthUserLimit(u64),
    #[error("created user {0} could not be loaded")]
    MissingCreatedUser(Uuid),
    #[error("OAuth link {0} points to a missing user")]
    MissingOAuthLinkUser(Uuid),
    #[error("invalid persisted authentication value: {0}")]
    InvalidPersistedValue(JsonValue),
    #[error(transparent)]
    OAuth(#[from] oauth::OAuthError),
    #[error(transparent)]
    SystemCatalog(#[from] SystemCatalogError),
    #[error(transparent)]
    Transaction(#[from] ExecutionError),
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
}

#[cfg(test)]
mod tests {
    use instant_instaql::{Attribute, AttributeValueType, Cardinality, system_attribute_catalog};

    use super::*;

    #[tokio::test]
    async fn unique_auth_lookup_does_not_scan_other_users() {
        let Some(url) =
            instant_test_support::test_database_url("unique_auth_lookup_does_not_scan_other_users")
        else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let schema = format!("auth_lookup_{}", uuid::Uuid::new_v4().simple());
        client
            .batch_execute(&format!(
                "BEGIN; CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}, public"
            ))
            .await
            .unwrap();
        client.batch_execute("CREATE FUNCTION json_null_to_null(jsonb) RETURNS jsonb
          LANGUAGE SQL IMMUTABLE PARALLEL SAFE AS 'SELECT CASE WHEN $1 = ''null''::jsonb THEN NULL ELSE $1 END';
          CREATE TEMP TABLE triples (app_id uuid, entity_id uuid, attr_id uuid, value jsonb, av boolean);
          CREATE UNIQUE INDEX auth_lookup_av ON triples (app_id, attr_id, json_null_to_null(value)) INCLUDE (entity_id) WHERE av;").await.unwrap();
        let app = uuid::Uuid::new_v4();
        let attr = uuid::Uuid::new_v4();
        client.execute("INSERT INTO triples SELECT $1, gen_random_uuid(), $2, to_jsonb('token-' || n), true FROM generate_series(1, 5000) n", &[&app, &attr]).await.unwrap();
        client.batch_execute("ANALYZE triples").await.unwrap();
        let query = format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) {}",
            super::UNIQUE_ATTRIBUTE_LOOKUP_SQL
        );
        let plan: serde_json::Value = client
            .query_one(&query, &[&app, &attr, &serde_json::json!("missing-token")])
            .await
            .unwrap()
            .get(0);
        let lookup = &plan[0]["Plan"]["Plans"][0];
        assert_eq!(
            lookup["Index Name"], "auth_lookup_av",
            "lookup must use the unique value index: {plan}"
        );
        assert!(
            lookup["Rows Removed by Filter"].as_u64().unwrap_or(0) < 10,
            "lookup must not scan other users: {plan}"
        );
        // A hit must still return its owner and remain scoped to the app.
        assert!(
            client
                .query_opt(
                    super::UNIQUE_ATTRIBUTE_LOOKUP_SQL,
                    &[&app, &attr, &serde_json::json!("token-4999")]
                )
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            client
                .query_opt(
                    super::UNIQUE_ATTRIBUTE_LOOKUP_SQL,
                    &[
                        &uuid::Uuid::new_v4(),
                        &attr,
                        &serde_json::json!("token-4999")
                    ]
                )
                .await
                .unwrap()
                .is_none()
        );
        client
            .batch_execute("CREATE INDEX auth_lookup_entity ON triples(app_id, entity_id)")
            .await
            .unwrap();
        let token_entity: Uuid = client
            .query_one(
                UNIQUE_ATTRIBUTE_LOOKUP_SQL,
                &[&app, &attr, &json!("token-4999")],
            )
            .await
            .unwrap()
            .get(0);
        let user_id = Uuid::new_v4();
        let reference_attr = Uuid::new_v4();
        let projection_attr = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO triples VALUES ($1,$2,$3,$4,false),($1,$5,$6,$7,false)",
                &[
                    &app,
                    &token_entity,
                    &reference_attr,
                    &json!(user_id),
                    &user_id,
                    &projection_attr,
                    &json!("synthetic"),
                ],
            )
            .await
            .unwrap();
        let plan: JsonValue = client
            .query_one(
                &format!("EXPLAIN (ANALYZE, FORMAT JSON) {REFRESH_TOKEN_USER_SQL}"),
                &[&app, &attr, &json!("token-4999"), &reference_attr],
            )
            .await
            .unwrap()
            .get(0);
        let encoded_plan = plan.to_string();
        assert!(
            encoded_plan.contains("auth_lookup_av"),
            "combined token lookup lost the indexed hash path: {plan}"
        );
        assert!(
            encoded_plan.contains("auth_lookup_entity"),
            "combined user projection must use entity lookup indexes: {plan}"
        );
        assert_eq!(plan[0]["Plan"]["Actual Rows"].as_f64(), Some(1.0));
        client.batch_execute("ROLLBACK").await.unwrap();
        drop(client);
        driver.await.unwrap().unwrap();
    }

    #[test]
    fn token_coercion_and_secret_rendering_match_jvm_prefix_rules() {
        let uuid = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        for (value, expected) in [
            (
                "pat_secret",
                AuthToken::PlatformAccess("pat_secret".to_owned()),
            ),
            (
                "per_secret",
                AuthToken::PersonalAccess("per_secret".to_owned()),
            ),
            (
                "eyJheader.payload.sig",
                AuthToken::Jwt("eyJheader.payload.sig".to_owned()),
            ),
            (&uuid.to_string(), AuthToken::RuntimeRefresh(uuid)),
        ] {
            let token = AuthToken::parse(value).unwrap();
            assert_eq!(token, expected);
            assert_eq!(format!("{token}"), "<secret>");
            assert_eq!(format!("{token:?}"), "<secret>");
        }
        assert!(matches!(
            AuthToken::parse("prt_secret"),
            Err(AuthError::InvalidToken)
        ));
    }

    #[test]
    fn hashes_match_jvm_sha256_byte_contracts() {
        let token = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        assert_eq!(
            hash_refresh_token(token),
            "a8faed6abbf35c12a4b26e40f6feb19d736d90045c83b9f9a31f638d323e6811"
        );
        assert_eq!(
            hash_magic_code("123456"),
            "8d969eef6ecad3c29a3a629280e686cf0c3f5d5a86aff3ca12020c923adc6c92"
        );
    }

    #[test]
    fn magic_codes_use_six_decimal_digits_and_constant_time_comparison() {
        let mut seen = [false; 10];
        for _ in 0..1_000 {
            let code = generate_magic_code();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
            for byte in code.bytes() {
                seen[usize::from(byte - b'0')] = true;
            }
        }
        assert!(seen.iter().all(|digit| *digit));
        assert!(constant_time_code_eq("424242", "424242"));
        assert!(!constant_time_code_eq("424242", "424243"));
        assert!(!constant_time_code_eq("42", "420"));
        for (token, prefix) in [
            (
                generate_platform_access_token(),
                PLATFORM_ACCESS_TOKEN_PREFIX,
            ),
            (
                generate_platform_refresh_token(),
                PLATFORM_REFRESH_TOKEN_PREFIX,
            ),
            (
                generate_personal_access_token(),
                PERSONAL_ACCESS_TOKEN_PREFIX,
            ),
        ] {
            assert!(token.starts_with(prefix));
            assert_eq!(token.len(), prefix.len() + 64);
            assert!(
                token[prefix.len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            );
        }
    }

    #[test]
    fn user_steps_validate_and_write_custom_schema_fields() {
        let mut catalog = system_attribute_catalog().unwrap();
        let nickname = Attribute {
            id: Uuid::new_v4(),
            wire: None,
            etype: "$users".to_owned(),
            label: "nickname".to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        };
        catalog.attributes.push(nickname.clone());
        let user = CreateUser {
            id: Uuid::new_v4(),
            email: Some("a@example.com".to_owned()),
            user_type: Some("user".to_owned()),
            image_url: None,
            extra_fields: BTreeMap::from([("nickname".to_owned(), json!("A"))]),
        };
        let steps = create_user_steps(&user, &catalog).unwrap();
        assert!(steps.iter().any(
            |step| matches!(step, TransactionStep::AddTriple(step) if step.attr_id == nickname.id)
        ));

        let mut unknown = user.clone();
        unknown
            .extra_fields
            .insert("missing".to_owned(), json!(true));
        assert!(matches!(
            create_user_steps(&unknown, &catalog),
            Err(AuthError::UnknownUserField(field)) if field == "missing"
        ));
        let mut system = user;
        system.extra_fields.insert("email".to_owned(), json!("b"));
        assert!(matches!(
            create_user_steps(&system, &catalog),
            Err(AuthError::SystemUserField(field)) if field == "email"
        ));
    }
}
