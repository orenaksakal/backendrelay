//! System-triple persistence for runtime OAuth flows.

use std::collections::BTreeMap;
use std::future::Future;

use instant_instaql::{AttributeCatalog, system_attr_id};
use instant_transact::{
    DeleteEntityStep, EntityLookup, TransactionStep, execute_transaction, execute_transaction_in,
};
use serde_json::{Value as JsonValue, json};
use tokio_postgres::{Client, GenericClient, Transaction};
use uuid::Uuid;

use super::oauth::verify_pkce;
use super::{
    AppUser, AuthError, CreateUser, add, create_user_steps, get_user, hash_refresh_token,
    json_string, json_uuid, refresh_token_steps, system_transaction_options, user_by_email,
};

#[derive(Clone, Debug, PartialEq)]
pub struct OAuthCode {
    pub entity_id: Uuid,
    pub client_id: Uuid,
    pub user_info: JsonValue,
    pub code_challenge_method: Option<String>,
    pub code_challenge: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthRedirect {
    pub entity_id: Uuid,
    pub cookie_hash: Vec<u8>,
    pub redirect_url: String,
    pub redirect_to: Option<String>,
    pub client_id: Uuid,
    pub code_challenge_method: Option<String>,
    pub code_challenge: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthProvider {
    pub id: Uuid,
    pub name: String,
}

/// Persisted OAuth client configuration. Secret encryption/decryption belongs
/// to the server key-management boundary; this model only handles ciphertext.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthClientRecord {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub name: String,
    pub client_id: Option<String>,
    pub encrypted_client_secret: Option<String>,
    /// Extra native-app audience IDs accepted by this client in addition to
    /// the resolved `client_id`.
    pub extra_audiences: Vec<String>,
    pub discovery_endpoint: Option<String>,
    pub meta: Option<JsonValue>,
    pub redirect_to: Option<String>,
    pub use_shared_credentials: bool,
}

/// Presence-aware changes for a persisted OAuth client. An outer `None`
/// leaves a field unchanged; `Some(None)` clears a nullable field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthClientPatch {
    pub client_id: Option<Option<String>>,
    pub encrypted_client_secret: Option<Option<String>>,
    /// `Some` replaces the list; an empty list clears it.
    pub extra_audiences: Option<Vec<String>>,
    pub discovery_endpoint: Option<Option<String>>,
    pub meta: Option<Option<JsonValue>>,
    pub redirect_to: Option<Option<String>>,
    pub use_shared_credentials: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthUserLink {
    pub id: Uuid,
    pub subject: String,
    pub user_id: Uuid,
    pub provider_id: Uuid,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OAuthSignIn {
    pub provider_id: Uuid,
    pub subject: String,
    pub email: Option<String>,
    pub image_url: Option<String>,
    pub guest_user_id: Option<Uuid>,
    pub extra_fields: BTreeMap<String, JsonValue>,
    pub use_shared_credentials: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OAuthSession {
    pub user: AppUser,
    pub link: OAuthUserLink,
    pub refresh_token: Uuid,
    pub created: bool,
}

/// Resolves or creates an OAuth user, updates/reassigns its provider link,
/// upgrades or links a guest, and issues a refresh token atomically.
///
/// The shared-credential limit and `$users` create-rule callbacks run only for
/// a new user and before any write, matching the runtime route contract.
///
/// # Errors
///
/// Returns callback, validation, malformed storage, catalog, transaction, or
/// PostgreSQL errors.
#[allow(clippy::too_many_lines)]
pub async fn sign_in_with_oauth<F, G, FFut, GFut>(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    sign_in: &OAuthSignIn,
    authorize_signup: F,
    authorize_shared_credentials: G,
) -> Result<OAuthSession, AuthError>
where
    F: FnOnce(&CreateUser) -> FFut,
    FFut: Future<Output = Result<(), AuthError>>,
    G: FnOnce() -> GFut,
    GFut: Future<Output = Result<(), AuthError>>,
{
    sign_in_with_oauth_reusing_refresh(
        client,
        app_id,
        catalog,
        sign_in,
        None,
        authorize_signup,
        authorize_shared_credentials,
    )
    .await
}

/// Performs OAuth sign-in while retaining a supplied refresh token when it
/// already belongs to the selected user. This is the native ID-token route's
/// JVM contract and avoids issuing a replacement token for the same account.
///
/// # Errors
///
/// Returns callback, validation, malformed storage, catalog, transaction, or
/// PostgreSQL errors.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn sign_in_with_oauth_reusing_refresh<F, G, FFut, GFut>(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    sign_in: &OAuthSignIn,
    reusable_refresh: Option<(Uuid, Uuid)>,
    authorize_signup: F,
    authorize_shared_credentials: G,
) -> Result<OAuthSession, AuthError>
where
    F: FnOnce(&CreateUser) -> FFut,
    FFut: Future<Output = Result<(), AuthError>>,
    G: FnOnce() -> GFut,
    GFut: Future<Output = Result<(), AuthError>>,
{
    let email_user = match sign_in.email.as_deref() {
        Some(email) => user_by_email(client, app_id, catalog, email).await?,
        None => None,
    };
    let existing_link =
        oauth_user_link(client, app_id, &sign_in.subject, sign_in.provider_id).await?;
    let link_user = match &existing_link {
        Some(link) => Some(
            get_user(client, app_id, catalog, link.user_id)
                .await?
                .ok_or(AuthError::MissingOAuthLinkUser(link.id))?,
        ),
        None => None,
    };
    let selected_user = match (&email_user, &link_user) {
        (Some(email_user), Some(link_user)) if email_user.id != link_user.id => {
            // Apple may reveal a real email after first returning a relay email.
            // As on the JVM, the unique email match wins and the link moves.
            Some(email_user.clone())
        }
        (Some(email_user), _) => Some(email_user.clone()),
        (None, Some(link_user)) => Some(link_user.clone()),
        (None, None) => None,
    };
    let (user_id, link, created, mut steps) = if let Some(user) = selected_user {
        let mut steps = Vec::new();
        if let Some(guest_user_id) = sign_in.guest_user_id
            && guest_user_id != user.id
        {
            steps.push(add(
                guest_user_id,
                "$users",
                system_attr_id("$users", "linkedPrimaryUser")?,
                json!(user.id),
            ));
        }
        if let Some(image_url) = &sign_in.image_url
            && user.image_url.as_ref() != Some(image_url)
        {
            steps.push(add(
                user.id,
                "$users",
                system_attr_id("$users", "imageURL")?,
                json!(image_url),
            ));
        }
        if let Some(email) = &sign_in.email
            && user.email.as_ref() != Some(email)
        {
            steps.push(add(
                user.id,
                "$users",
                system_attr_id("$users", "email")?,
                json!(email),
            ));
        }
        let link = if let Some(mut link) = existing_link {
            if link.user_id != user.id {
                steps.push(add(
                    link.id,
                    "$oauthUserLinks",
                    system_attr_id("$oauthUserLinks", "$user")?,
                    json!(user.id),
                ));
                link.user_id = user.id;
            }
            link
        } else {
            let link = OAuthUserLink {
                id: Uuid::new_v4(),
                subject: sign_in.subject.clone(),
                user_id: user.id,
                provider_id: sign_in.provider_id,
            };
            steps.extend(oauth_user_link_steps(&link)?);
            link
        };
        (user.id, link, false, steps)
    } else {
        if sign_in.use_shared_credentials {
            authorize_shared_credentials().await?;
        }
        let user_id = sign_in.guest_user_id.unwrap_or_else(Uuid::new_v4);
        let create = CreateUser {
            id: user_id,
            email: sign_in.email.clone(),
            user_type: Some("user".to_owned()),
            image_url: sign_in.image_url.clone(),
            extra_fields: sign_in.extra_fields.clone(),
        };
        let mut steps = create_user_steps(&create, catalog)?;
        authorize_signup(&create).await?;
        let link = OAuthUserLink {
            id: Uuid::new_v4(),
            subject: sign_in.subject.clone(),
            user_id,
            provider_id: sign_in.provider_id,
        };
        steps.extend(oauth_user_link_steps(&link)?);
        (user_id, link, true, steps)
    };
    let reused_refresh = reusable_refresh.filter(|(_, token_user_id)| *token_user_id == user_id);
    let refresh_token = reused_refresh.map_or_else(Uuid::new_v4, |(token, _)| token);
    if reused_refresh.is_none() {
        steps.extend(refresh_token_steps(Uuid::new_v4(), refresh_token, user_id)?);
    }
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(OAuthSession {
        user: get_user(client, app_id, catalog, user_id)
            .await?
            .ok_or(AuthError::MissingCreatedUser(user_id))?,
        link,
        refresh_token,
        created,
    })
}

/// Creates a one-time, hash-only OAuth authorization code record.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
#[allow(clippy::too_many_arguments)]
pub async fn create_oauth_code(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    code: Uuid,
    oauth_client_id: Uuid,
    user_info: JsonValue,
    code_challenge_method: Option<&str>,
    code_challenge: Option<&str>,
) -> Result<Uuid, AuthError> {
    let entity_id = Uuid::new_v4();
    let mut steps = vec![
        add(
            entity_id,
            "$oauthCodes",
            system_attr_id("$oauthCodes", "id")?,
            json!(entity_id),
        ),
        add(
            entity_id,
            "$oauthCodes",
            system_attr_id("$oauthCodes", "codeHash")?,
            json!(hash_refresh_token(code)),
        ),
        add(
            entity_id,
            "$oauthCodes",
            system_attr_id("$oauthCodes", "$oauthClient")?,
            json!(oauth_client_id),
        ),
        add(
            entity_id,
            "$oauthCodes",
            system_attr_id("$oauthCodes", "userInfo")?,
            user_info,
        ),
    ];
    push_optional_string(
        &mut steps,
        entity_id,
        "$oauthCodes",
        "codeChallengeMethod",
        code_challenge_method,
    )?;
    push_optional_string(
        &mut steps,
        entity_id,
        "$oauthCodes",
        "codeChallenge",
        code_challenge,
    )?;
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(entity_id)
}

/// Atomically deletes, expires, and PKCE-verifies an OAuth code.
///
/// # Errors
///
/// Returns invalid/expired code, PKCE, catalog, transaction, or PostgreSQL
/// errors. Expired and PKCE-invalid records remain consumed.
pub async fn consume_oauth_code(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    code: Uuid,
    verifier: Option<&str>,
    now_ms: i64,
) -> Result<OAuthCode, AuthError> {
    let transaction = client.transaction().await?;
    let row = transaction
        .query_opt(
            "SELECT entity_id, created_at FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3::jsonb
             LIMIT 1 FOR UPDATE",
            &[
                &app_id,
                &system_attr_id("$oauthCodes", "codeHash")?,
                &json!(hash_refresh_token(code)),
            ],
        )
        .await?;
    let Some(row) = row else {
        return Err(AuthError::InvalidOAuthCode);
    };
    let entity_id = row.get::<_, Uuid>(0);
    let created_at_ms = row.get::<_, i64>(1);
    let values = entity_values(&transaction, app_id, entity_id).await?;
    let record = OAuthCode {
        entity_id,
        client_id: required_uuid(&values, "$oauthCodes", "$oauthClient")?,
        user_info: required_value(&values, "$oauthCodes", "userInfo")?.clone(),
        code_challenge_method: optional_string(&values, "$oauthCodes", "codeChallengeMethod")?,
        code_challenge: optional_string(&values, "$oauthCodes", "codeChallenge")?,
        created_at_ms,
    };
    delete_in_transaction(&transaction, app_id, catalog, entity_id, "$oauthCodes").await?;
    transaction.commit().await?;
    if whole_minutes_since(created_at_ms, now_ms) > 5 {
        return Err(AuthError::ExpiredOAuthCode);
    }
    verify_pkce(
        record.code_challenge_method.as_deref(),
        record.code_challenge.as_deref(),
        verifier,
    )?;
    Ok(record)
}

/// Creates a one-time OAuth redirect state and stores only state/cookie hashes.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
#[allow(clippy::too_many_arguments)]
pub async fn create_oauth_redirect(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    state: Uuid,
    cookie: Uuid,
    redirect_url: &str,
    redirect_to: Option<&str>,
    oauth_client_id: Uuid,
    code_challenge_method: Option<&str>,
    code_challenge: Option<&str>,
) -> Result<Uuid, AuthError> {
    let entity_id = Uuid::new_v4();
    let mut steps = vec![
        add(
            entity_id,
            "$oauthRedirects",
            system_attr_id("$oauthRedirects", "id")?,
            json!(entity_id),
        ),
        add(
            entity_id,
            "$oauthRedirects",
            system_attr_id("$oauthRedirects", "stateHash")?,
            json!(hash_refresh_token(state)),
        ),
        add(
            entity_id,
            "$oauthRedirects",
            system_attr_id("$oauthRedirects", "cookieHash")?,
            json!(hash_refresh_token(cookie)),
        ),
        add(
            entity_id,
            "$oauthRedirects",
            system_attr_id("$oauthRedirects", "redirectUrl")?,
            json!(redirect_url),
        ),
        add(
            entity_id,
            "$oauthRedirects",
            system_attr_id("$oauthRedirects", "$oauthClient")?,
            json!(oauth_client_id),
        ),
    ];
    push_optional_string(
        &mut steps,
        entity_id,
        "$oauthRedirects",
        "redirectTo",
        redirect_to,
    )?;
    push_optional_string(
        &mut steps,
        entity_id,
        "$oauthRedirects",
        "codeChallengeMethod",
        code_challenge_method,
    )?;
    push_optional_string(
        &mut steps,
        entity_id,
        "$oauthRedirects",
        "codeChallenge",
        code_challenge,
    )?;
    execute_transaction(
        client,
        app_id,
        catalog,
        &steps,
        system_transaction_options(),
    )
    .await?;
    Ok(entity_id)
}

/// Atomically consumes an OAuth redirect state.
///
/// # Errors
///
/// Returns invalid/expired state, malformed storage, catalog, transaction, or
/// PostgreSQL errors. Expired records remain consumed.
pub async fn consume_oauth_redirect(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    state: Uuid,
    now_ms: i64,
) -> Result<OAuthRedirect, AuthError> {
    let transaction = client.transaction().await?;
    let row = transaction
        .query_opt(
            "SELECT entity_id, created_at FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3::jsonb
             LIMIT 1 FOR UPDATE",
            &[
                &app_id,
                &system_attr_id("$oauthRedirects", "stateHash")?,
                &json!(hash_refresh_token(state)),
            ],
        )
        .await?;
    let Some(row) = row else {
        return Err(AuthError::InvalidOAuthRedirect);
    };
    let entity_id = row.get::<_, Uuid>(0);
    let created_at_ms = row.get::<_, i64>(1);
    let values = entity_values(&transaction, app_id, entity_id).await?;
    let record = OAuthRedirect {
        entity_id,
        cookie_hash: decode_hex(&required_string(&values, "$oauthRedirects", "cookieHash")?)?,
        redirect_url: required_string(&values, "$oauthRedirects", "redirectUrl")?,
        redirect_to: optional_string(&values, "$oauthRedirects", "redirectTo")?,
        client_id: required_uuid(&values, "$oauthRedirects", "$oauthClient")?,
        code_challenge_method: optional_string(&values, "$oauthRedirects", "codeChallengeMethod")?,
        code_challenge: optional_string(&values, "$oauthRedirects", "codeChallenge")?,
        created_at_ms,
    };
    delete_in_transaction(&transaction, app_id, catalog, entity_id, "$oauthRedirects").await?;
    transaction.commit().await?;
    if whole_minutes_since(created_at_ms, now_ms) > 10 {
        Err(AuthError::ExpiredOAuthRedirect)
    } else {
        Ok(record)
    }
}

/// Creates an OAuth service-provider entity.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn create_oauth_provider(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    provider: &OAuthProvider,
) -> Result<(), AuthError> {
    execute_transaction(
        client,
        app_id,
        catalog,
        &[
            add(
                provider.id,
                "$oauthProviders",
                system_attr_id("$oauthProviders", "id")?,
                json!(provider.id),
            ),
            add(
                provider.id,
                "$oauthProviders",
                system_attr_id("$oauthProviders", "name")?,
                json!(provider.name),
            ),
        ],
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

/// Loads an OAuth provider by ID.
///
/// # Errors
///
/// Returns malformed storage, catalog, or PostgreSQL errors.
pub async fn oauth_provider_by_id<C>(
    client: &C,
    app_id: Uuid,
    provider_id: Uuid,
) -> Result<Option<OAuthProvider>, AuthError>
where
    C: GenericClient + Sync,
{
    let values = entity_values(client, app_id, provider_id).await?;
    if values.is_empty() {
        return Ok(None);
    }
    Ok(Some(OAuthProvider {
        id: provider_id,
        name: required_string(&values, "$oauthProviders", "name")?,
    }))
}

/// Loads an OAuth provider by its unique application-local name.
///
/// # Errors
///
/// Returns malformed storage, catalog, or PostgreSQL errors.
pub async fn oauth_provider_by_name<C>(
    client: &C,
    app_id: Uuid,
    name: &str,
) -> Result<Option<OAuthProvider>, AuthError>
where
    C: GenericClient + Sync,
{
    let Some(entity_id) = entity_by_unique_value(
        client,
        app_id,
        system_attr_id("$oauthProviders", "name")?,
        json!(name),
    )
    .await?
    else {
        return Ok(None);
    };
    oauth_provider_by_id(client, app_id, entity_id).await
}

/// Creates an OAuth client record using an already-encrypted client secret.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn create_oauth_client(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    oauth_client: &OAuthClientRecord,
) -> Result<(), AuthError> {
    let mut steps = vec![
        add(
            oauth_client.id,
            "$oauthClients",
            system_attr_id("$oauthClients", "id")?,
            json!(oauth_client.id),
        ),
        add(
            oauth_client.id,
            "$oauthClients",
            system_attr_id("$oauthClients", "$oauthProvider")?,
            json!(oauth_client.provider_id),
        ),
        add(
            oauth_client.id,
            "$oauthClients",
            system_attr_id("$oauthClients", "name")?,
            json!(oauth_client.name),
        ),
        add(
            oauth_client.id,
            "$oauthClients",
            system_attr_id("$oauthClients", "useSharedCredentials")?,
            json!(oauth_client.use_shared_credentials),
        ),
        add(
            oauth_client.id,
            "$oauthClients",
            system_attr_id("$oauthClients", "extraAudiences")?,
            json!(oauth_client.extra_audiences),
        ),
    ];
    for (label, value) in [
        ("clientId", oauth_client.client_id.as_deref()),
        (
            "encryptedClientSecret",
            oauth_client.encrypted_client_secret.as_deref(),
        ),
        (
            "discoveryEndpoint",
            oauth_client.discovery_endpoint.as_deref(),
        ),
        ("redirectTo", oauth_client.redirect_to.as_deref()),
    ] {
        push_optional_string(&mut steps, oauth_client.id, "$oauthClients", label, value)?;
    }
    if let Some(meta) = &oauth_client.meta {
        steps.push(add(
            oauth_client.id,
            "$oauthClients",
            system_attr_id("$oauthClients", "meta")?,
            meta.clone(),
        ));
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

/// Loads an OAuth client by ID.
///
/// # Errors
///
/// Returns malformed storage, catalog, or PostgreSQL errors.
pub async fn oauth_client_by_id<C>(
    client: &C,
    app_id: Uuid,
    client_id: Uuid,
) -> Result<Option<OAuthClientRecord>, AuthError>
where
    C: GenericClient + Sync,
{
    let values = entity_values(client, app_id, client_id).await?;
    if values.is_empty() {
        return Ok(None);
    }
    Ok(Some(OAuthClientRecord {
        id: client_id,
        provider_id: required_uuid(&values, "$oauthClients", "$oauthProvider")?,
        name: required_string(&values, "$oauthClients", "name")?,
        client_id: optional_string(&values, "$oauthClients", "clientId")?,
        encrypted_client_secret: optional_string(
            &values,
            "$oauthClients",
            "encryptedClientSecret",
        )?,
        extra_audiences: optional_string_array(&values, "$oauthClients", "extraAudiences")?,
        discovery_endpoint: optional_string(&values, "$oauthClients", "discoveryEndpoint")?,
        meta: optional_value(&values, "$oauthClients", "meta")?,
        redirect_to: optional_string(&values, "$oauthClients", "redirectTo")?,
        use_shared_credentials: optional_bool(&values, "$oauthClients", "useSharedCredentials")?
            .unwrap_or(false),
    }))
}

/// Loads an OAuth client by its unique application-local name.
///
/// # Errors
///
/// Returns malformed storage, catalog, or PostgreSQL errors.
pub async fn oauth_client_by_name<C>(
    client: &C,
    app_id: Uuid,
    name: &str,
) -> Result<Option<OAuthClientRecord>, AuthError>
where
    C: GenericClient + Sync,
{
    let Some(entity_id) = entity_by_unique_value(
        client,
        app_id,
        system_attr_id("$oauthClients", "name")?,
        json!(name),
    )
    .await?
    else {
        return Ok(None);
    };
    oauth_client_by_id(client, app_id, entity_id).await
}

/// Applies a presence-aware OAuth-client patch through the system-triple
/// transaction path.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn update_oauth_client(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    client_id: Uuid,
    patch: &OAuthClientPatch,
) -> Result<(), AuthError> {
    let mut steps = vec![add(
        client_id,
        "$oauthClients",
        system_attr_id("$oauthClients", "id")?,
        json!(client_id),
    )];
    for (label, value) in [
        ("clientId", &patch.client_id),
        ("encryptedClientSecret", &patch.encrypted_client_secret),
        ("discoveryEndpoint", &patch.discovery_endpoint),
        ("redirectTo", &patch.redirect_to),
    ] {
        if let Some(value) = value {
            steps.push(add(
                client_id,
                "$oauthClients",
                system_attr_id("$oauthClients", label)?,
                value.as_ref().map_or(JsonValue::Null, |value| json!(value)),
            ));
        }
    }
    if let Some(extra_audiences) = &patch.extra_audiences {
        steps.push(add(
            client_id,
            "$oauthClients",
            system_attr_id("$oauthClients", "extraAudiences")?,
            json!(extra_audiences),
        ));
    }
    if let Some(meta) = &patch.meta {
        let step = add(
            client_id,
            "$oauthClients",
            system_attr_id("$oauthClients", "meta")?,
            meta.clone().unwrap_or(JsonValue::Null),
        );
        let TransactionStep::AddTriple(step) = step else {
            unreachable!("add always constructs an add-triple step")
        };
        steps.push(TransactionStep::DeepMergeTriple(step));
    }
    if let Some(shared) = patch.use_shared_credentials {
        steps.push(add(
            client_id,
            "$oauthClients",
            system_attr_id("$oauthClients", "useSharedCredentials")?,
            json!(shared),
        ));
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

/// Deletes an OAuth-client entity through the system-triple transaction path.
/// Returns `false` when the client does not exist in the application.
///
/// # Errors
///
/// Returns catalog, transaction, malformed storage, or PostgreSQL errors.
pub async fn delete_oauth_client(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    client_id: Uuid,
) -> Result<bool, AuthError> {
    if oauth_client_by_id(client, app_id, client_id)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    execute_transaction(
        client,
        app_id,
        catalog,
        &[TransactionStep::DeleteEntity(DeleteEntityStep {
            entity: EntityLookup::Id(client_id),
            etype: Some("$oauthClients".to_owned()),
        })],
        system_transaction_options(),
    )
    .await?;
    Ok(true)
}

/// Creates a unique `(subject, provider)` OAuth-to-user link.
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn create_oauth_user_link(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    link: &OAuthUserLink,
) -> Result<(), AuthError> {
    execute_transaction(
        client,
        app_id,
        catalog,
        &oauth_user_link_steps(link)?,
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

/// Updates the user side of an existing OAuth link (guest upgrade/linking).
///
/// # Errors
///
/// Returns catalog, transaction, or PostgreSQL errors.
pub async fn update_oauth_link_user(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    link_id: Uuid,
    user_id: Uuid,
) -> Result<(), AuthError> {
    execute_transaction(
        client,
        app_id,
        catalog,
        &[add(
            link_id,
            "$oauthUserLinks",
            system_attr_id("$oauthUserLinks", "$user")?,
            json!(user_id),
        )],
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

/// Looks up an OAuth user link by its collision-free composite identity.
///
/// # Errors
///
/// Returns malformed storage, catalog, or PostgreSQL errors.
pub async fn oauth_user_link<C>(
    client: &C,
    app_id: Uuid,
    subject: &str,
    provider_id: Uuid,
) -> Result<Option<OAuthUserLink>, AuthError>
where
    C: GenericClient + Sync,
{
    let row = client
        .query_opt(
            "SELECT entity_id FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3::jsonb LIMIT 1",
            &[
                &app_id,
                &system_attr_id("$oauthUserLinks", "sub+$oauthProvider")?,
                &json!(composite_subject_provider(subject, provider_id)),
            ],
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let entity_id = row.get(0);
    let values = entity_values(client, app_id, entity_id).await?;
    Ok(Some(OAuthUserLink {
        id: entity_id,
        subject: required_string(&values, "$oauthUserLinks", "sub")?,
        user_id: required_uuid(&values, "$oauthUserLinks", "$user")?,
        provider_id: required_uuid(&values, "$oauthUserLinks", "$oauthProvider")?,
    }))
}

fn oauth_user_link_steps(link: &OAuthUserLink) -> Result<Vec<TransactionStep>, AuthError> {
    Ok(vec![
        add(
            link.id,
            "$oauthUserLinks",
            system_attr_id("$oauthUserLinks", "id")?,
            json!(link.id),
        ),
        add(
            link.id,
            "$oauthUserLinks",
            system_attr_id("$oauthUserLinks", "sub")?,
            json!(link.subject),
        ),
        add(
            link.id,
            "$oauthUserLinks",
            system_attr_id("$oauthUserLinks", "$user")?,
            json!(link.user_id),
        ),
        add(
            link.id,
            "$oauthUserLinks",
            system_attr_id("$oauthUserLinks", "$oauthProvider")?,
            json!(link.provider_id),
        ),
        add(
            link.id,
            "$oauthUserLinks",
            system_attr_id("$oauthUserLinks", "sub+$oauthProvider")?,
            json!(composite_subject_provider(&link.subject, link.provider_id)),
        ),
    ])
}

fn composite_subject_provider(subject: &str, provider_id: Uuid) -> String {
    format!("{subject}+{provider_id}")
}

async fn entity_values<C>(
    client: &C,
    app_id: Uuid,
    entity_id: Uuid,
) -> Result<BTreeMap<Uuid, JsonValue>, AuthError>
where
    C: GenericClient + Sync,
{
    let rows = client
        .query(
            "SELECT attr_id, value FROM triples WHERE app_id = $1 AND entity_id = $2",
            &[&app_id, &entity_id],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get(0),
                row.get::<_, tokio_postgres::types::Json<JsonValue>>(1).0,
            )
        })
        .collect())
}

async fn entity_by_unique_value<C>(
    client: &C,
    app_id: Uuid,
    attr_id: Uuid,
    value: JsonValue,
) -> Result<Option<Uuid>, AuthError>
where
    C: GenericClient + Sync,
{
    Ok(client
        .query_opt(
            "SELECT entity_id FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3::jsonb LIMIT 1",
            &[&app_id, &attr_id, &value],
        )
        .await?
        .map(|row| row.get(0)))
}

async fn delete_in_transaction(
    client: &Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entity_id: Uuid,
    etype: &str,
) -> Result<(), AuthError> {
    execute_transaction_in(
        client,
        app_id,
        catalog,
        &[TransactionStep::DeleteEntity(DeleteEntityStep {
            entity: EntityLookup::Id(entity_id),
            etype: Some(etype.to_owned()),
        })],
        system_transaction_options(),
    )
    .await?;
    Ok(())
}

fn push_optional_string(
    steps: &mut Vec<TransactionStep>,
    entity_id: Uuid,
    etype: &str,
    label: &str,
    value: Option<&str>,
) -> Result<(), AuthError> {
    if let Some(value) = value {
        steps.push(add(
            entity_id,
            etype,
            system_attr_id(etype, label)?,
            json!(value),
        ));
    }
    Ok(())
}

fn required_value<'a>(
    values: &'a BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<&'a JsonValue, AuthError> {
    values
        .get(&system_attr_id(etype, label)?)
        .ok_or_else(|| AuthError::InvalidPersistedValue(JsonValue::Null))
}

fn required_uuid(
    values: &BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<Uuid, AuthError> {
    json_uuid(required_value(values, etype, label)?)
}

fn required_string(
    values: &BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<String, AuthError> {
    json_string(required_value(values, etype, label)?)?
        .ok_or_else(|| AuthError::InvalidPersistedValue(JsonValue::Null))
}

fn optional_string(
    values: &BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<Option<String>, AuthError> {
    values
        .get(&system_attr_id(etype, label)?)
        .map(json_string)
        .transpose()
        .map(Option::flatten)
}

fn optional_value(
    values: &BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<Option<JsonValue>, AuthError> {
    Ok(values.get(&system_attr_id(etype, label)?).cloned())
}

fn optional_string_array(
    values: &BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<Vec<String>, AuthError> {
    let Some(value) = values.get(&system_attr_id(etype, label)?) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| AuthError::InvalidPersistedValue(value.clone()))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| AuthError::InvalidPersistedValue(item.clone()))
        })
        .collect()
}

fn optional_bool(
    values: &BTreeMap<Uuid, JsonValue>,
    etype: &str,
    label: &str,
) -> Result<Option<bool>, AuthError> {
    values
        .get(&system_attr_id(etype, label)?)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| AuthError::InvalidPersistedValue(value.clone()))
        })
        .transpose()
}

fn whole_minutes_since(created_at_ms: i64, now_ms: i64) -> i64 {
    now_ms.saturating_sub(created_at_ms).div_euclid(60_000)
}

fn decode_hex(value: &str) -> Result<Vec<u8>, AuthError> {
    if !value.len().is_multiple_of(2) {
        return Err(AuthError::InvalidPersistedValue(json!(value)));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            let pair = &value.as_bytes()[index..index + 2];
            let pair = std::str::from_utf8(pair)
                .map_err(|_| AuthError::InvalidPersistedValue(json!(value)))?;
            u8::from_str_radix(pair, 16).map_err(|_| AuthError::InvalidPersistedValue(json!(value)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_identity_and_expiry_match_jvm_contract() {
        let provider = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        assert_eq!(
            composite_subject_provider("subject", provider),
            "subject+00112233-4455-6677-8899-aabbccddeeff"
        );
        assert_eq!(whole_minutes_since(0, 5 * 60_000 + 59_999), 5);
        assert_eq!(whole_minutes_since(0, 6 * 60_000), 6);
        assert_eq!(decode_hex("00aaff").unwrap(), [0, 170, 255]);
    }
}
