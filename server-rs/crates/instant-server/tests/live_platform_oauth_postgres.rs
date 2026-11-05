use base64::Engine as _;
use instant_http::{
    ApiError, ApiErrorType, PlatformOAuthBackend, PlatformOAuthClaimRequest,
    PlatformOAuthDecisionRequest, PlatformOAuthStartRequest, PlatformOAuthTokenRequest,
};
use instant_server::{PlatformOAuthService, PostgresPlatformOAuthStore, PostgresPool};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use tokio_postgres::NoTls;
use url::Url;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn postgres_oauth_store_runs_confidential_pkce_refresh_info_and_revoke_flows() {
    let Some(url) = instant_test_support::test_database_url(
        "postgres_oauth_store_runs_confidential_pkce_refresh_info_and_revoke_flows",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("platform_oauth_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path = {schema}, public"
        ))
        .await
        .unwrap();
    client
        .batch_execute(
            "CREATE TYPE instant_oauth_app_redirect_status AS ENUM ('init', 'claimed', 'granted');
             CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL,
               last_used_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE apps (id uuid PRIMARY KEY, creator_id uuid, org_id uuid);
             CREATE TABLE app_members (app_id uuid, user_id uuid);
             CREATE TABLE org_members (org_id uuid, user_id uuid);
             CREATE TABLE instant_oauth_apps (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, app_name text NOT NULL,
               granted_scopes text[] NOT NULL, is_public boolean NOT NULL,
               support_email text, app_privacy_policy_link text, app_logo bytea,
               app_tos_link text, app_home_page text
             );
             CREATE TABLE instant_oauth_app_clients (
               client_id uuid PRIMARY KEY, oauth_app_id uuid NOT NULL,
               client_name text NOT NULL, authorized_redirect_urls text[]
             );
             CREATE TABLE instant_oauth_app_client_secrets (
               id uuid PRIMARY KEY, client_id uuid NOT NULL, hashed_secret bytea NOT NULL,
               first_four text NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE instant_oauth_app_redirects (
               lookup_key bytea PRIMARY KEY, client_id uuid NOT NULL, state text NOT NULL,
               cookie uuid NOT NULL, redirect_uri text NOT NULL, scopes text[] NOT NULL,
               code_challenge_method text, code_challenge text,
               status instant_oauth_app_redirect_status NOT NULL, user_id uuid,
               grant_token uuid, expires_at timestamptz NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE instant_oauth_app_codes (
               hashed_code bytea PRIMARY KEY, client_id uuid NOT NULL,
               redirect_uri text NOT NULL, user_id uuid NOT NULL, scopes text[] NOT NULL,
               code_challenge_method text, code_challenge text,
               expires_at timestamptz NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE instant_user_oauth_refresh_tokens (
               lookup_key bytea PRIMARY KEY, client_id uuid NOT NULL, user_id uuid NOT NULL,
               scopes text[] NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE instant_user_oauth_access_tokens (
               lookup_key bytea PRIMARY KEY, refresh_token_lookup_key bytea,
               client_id uuid NOT NULL, user_id uuid NOT NULL, scopes text[] NOT NULL,
               expires_at timestamptz NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    let refresh_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let oauth_app_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let client_secret = "0123456789abcdef";
    let other_client_id = Uuid::new_v4();
    let other_client_secret = "fedcba9876543210";
    client
        .execute(
            "INSERT INTO instant_users VALUES ($1, 'owner@example.com')",
            &[&user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_oauth_app_clients
               (client_id, oauth_app_id, client_name, authorized_redirect_urls)
             VALUES ($1, $2, 'Other Web', ARRAY['https://tool.example.com/callback'])",
            &[&other_client_id, &oauth_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_oauth_app_client_secrets
               (id, client_id, hashed_secret, first_four) VALUES ($1, $2, $3, 'fedc')",
            &[
                &Uuid::new_v4(),
                &other_client_id,
                &Sha256::digest(other_client_secret.as_bytes()).to_vec(),
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id) VALUES ($1, $2, $3)",
            &[
                &refresh_id,
                &Sha256::digest(refresh_id.to_string().as_bytes()).to_vec(),
                &user_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps VALUES ($1, $2, NULL)",
            &[&app_id, &user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_oauth_apps
               (id, app_id, app_name, granted_scopes, is_public, support_email)
             VALUES ($1, $2, 'Tool', ARRAY['apps-read'], true, 'support@example.com')",
            &[&oauth_app_id, &app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_oauth_app_clients
               (client_id, oauth_app_id, client_name, authorized_redirect_urls)
             VALUES ($1, $2, 'Web', ARRAY['https://tool.example.com/callback'])",
            &[&client_id, &oauth_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_oauth_app_client_secrets
               (id, client_id, hashed_secret, first_four) VALUES ($1, $2, $3, '0123')",
            &[
                &Uuid::new_v4(),
                &client_id,
                &Sha256::digest(client_secret.as_bytes()).to_vec(),
            ],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&scoped_url(&url, &schema), 4)
        .await
        .unwrap();
    let service = PlatformOAuthService::new(
        PostgresPlatformOAuthStore::from_pool(pool),
        "https://dash.example.com",
        true,
    )
    .unwrap();
    let (code, cookie) = authorize(&service, client_id, refresh_id, None).await;
    let tokens = service
        .token(PlatformOAuthTokenRequest {
            grant_type: "authorization_code".to_owned(),
            client_id,
            client_secret: Some(client_secret.to_owned()),
            redirect_uri: Some("https://tool.example.com/callback".to_owned()),
            code: Some(code),
            code_verifier: None,
            refresh_token: None,
        })
        .await
        .unwrap();
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("pat_"));
    assert!(refresh.starts_with("prt_"));
    assert_eq!(
        service.token_info(access.clone()).await.unwrap()["token_type"],
        "Bearer"
    );
    let refreshed = service
        .token(PlatformOAuthTokenRequest {
            grant_type: "refresh_token".to_owned(),
            client_id,
            client_secret: Some(client_secret.to_owned()),
            redirect_uri: None,
            code: None,
            code_verifier: None,
            refresh_token: Some(refresh.clone()),
        })
        .await
        .unwrap();
    assert!(
        refreshed["access_token"]
            .as_str()
            .unwrap()
            .starts_with("pat_")
    );
    service.revoke(access.clone()).await.unwrap();
    assert!(service.token_info(access).await.is_err());
    service.revoke(refresh).await.unwrap();

    let verifier = "pkce-verifier";
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let (pkce_code, _) = authorize(
        &service,
        client_id,
        refresh_id,
        Some(("S256".to_owned(), challenge)),
    )
    .await;
    let pkce = service
        .token(PlatformOAuthTokenRequest {
            grant_type: "authorization_code".to_owned(),
            client_id,
            client_secret: None,
            redirect_uri: Some("https://tool.example.com/callback".to_owned()),
            code: Some(pkce_code),
            code_verifier: Some(verifier.to_owned()),
            refresh_token: None,
        })
        .await
        .unwrap();
    assert!(pkce["refresh_token"].is_null());
    assert_ne!(cookie, Uuid::nil());

    assert_invalid_redirect_attempts_burn_artifacts(&service, &client, client_id, refresh_id).await;
    assert_invalid_code_attempts_burn_artifacts(
        &service,
        client_id,
        client_secret,
        other_client_id,
        other_client_secret,
        refresh_id,
    )
    .await;
    assert_concurrent_consumers_have_one_winner(&service, client_id, client_secret, refresh_id)
        .await;

    drop(service);
    client
        .batch_execute(&format!("RESET search_path; DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

fn scoped_url(database_url: &str, schema: &str) -> String {
    let mut url = Url::parse(database_url).expect("TEST_DATABASE_URL must be a PostgreSQL URL");
    let mut pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut options = pairs
        .iter()
        .filter(|(key, _)| key == "options")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !options.is_empty() {
        options.push(' ');
    }
    write!(options, "-csearch_path={schema},public").unwrap();
    pairs.retain(|(key, _)| key != "options");
    pairs.push(("options".to_owned(), options));
    url.query_pairs_mut().clear().extend_pairs(pairs);
    url.into()
}

async fn assert_invalid_redirect_attempts_burn_artifacts(
    service: &PlatformOAuthService<PostgresPlatformOAuthStore>,
    setup: &tokio_postgres::Client,
    client_id: Uuid,
    refresh_id: Uuid,
) {
    let (request, _) = claimed_redirect(service, client_id, refresh_id, None).await;
    let valid_request = request.clone();
    let mut invalid_grant = request;
    invalid_grant.grant_token = Uuid::new_v4();
    assert_eq!(
        service.grant(invalid_grant).await.unwrap_err(),
        record_not_found("oauth-app-redirect")
    );
    assert_eq!(
        service.grant(valid_request).await.unwrap_err(),
        record_not_found("oauth-app-redirect")
    );

    let (request, _) = claimed_redirect(service, client_id, refresh_id, None).await;
    setup
        .execute(
            "UPDATE instant_oauth_app_redirects SET expires_at = now() - interval '1 second'
             WHERE lookup_key = $1",
            &[&Sha256::digest(request.redirect_id.as_bytes()).to_vec()],
        )
        .await
        .unwrap();
    assert_eq!(
        service.grant(request.clone()).await.unwrap_err(),
        ApiError::new(ApiErrorType::RecordExpired, "Expired oauth-app-redirect")
    );
    assert_eq!(
        service.grant(request).await.unwrap_err(),
        record_not_found("oauth-app-redirect")
    );

    let (request, _) = claimed_redirect(service, client_id, refresh_id, None).await;
    let valid_request = request.clone();
    let mut invalid_cookie = request;
    invalid_cookie.cookie = Some(Uuid::new_v4());
    assert_grant_error_redirect(
        &service.grant(invalid_cookie).await.unwrap(),
        "invalid_request",
        "Invalid cookie.",
    );
    assert_eq!(
        service.grant(valid_request).await.unwrap_err(),
        record_not_found("oauth-app-redirect")
    );

    let (mut request, _) = claimed_redirect(service, client_id, refresh_id, None).await;
    request.cookie = None;
    assert_grant_error_redirect(
        &service.grant(request).await.unwrap(),
        "invalid_request",
        "Missing cookie.",
    );

    let (request, _) = claimed_redirect(service, client_id, refresh_id, None).await;
    let valid_request = request.clone();
    let mut invalid_cookie = request;
    invalid_cookie.cookie = Some(Uuid::new_v4());
    assert_eq!(
        service.deny(invalid_cookie).await.unwrap_err(),
        ApiError::new(ApiErrorType::ParamMissing, "Invalid cookie")
    );
    assert_eq!(
        service.deny(valid_request).await.unwrap_err(),
        record_not_found("oauth-app-redirect")
    );
}

#[allow(clippy::too_many_arguments)]
async fn assert_invalid_code_attempts_burn_artifacts(
    service: &PlatformOAuthService<PostgresPlatformOAuthStore>,
    client_id: Uuid,
    client_secret: &str,
    other_client_id: Uuid,
    other_client_secret: &str,
    refresh_id: Uuid,
) {
    let (code, _) = authorize(service, client_id, refresh_id, None).await;
    let valid = confidential_token_request(client_id, client_secret, code);
    let invalid_secret = confidential_token_request(client_id, "wrong-secret", code);
    assert_eq!(
        service.token(invalid_secret).await.unwrap_err(),
        record_not_found("oauth-app-client")
    );
    assert!(service.token(valid).await.is_ok());

    let (code, _) = authorize(service, client_id, refresh_id, None).await;
    let valid = confidential_token_request(client_id, client_secret, code);
    let mut invalid_client = confidential_token_request(other_client_id, other_client_secret, code);
    invalid_client.redirect_uri = valid.redirect_uri.clone();
    assert_eq!(
        service.token(invalid_client).await.unwrap_err(),
        ApiError::new(ApiErrorType::ParamMalformed, "Invalid client_id parameter")
    );
    assert_eq!(
        service.token(valid).await.unwrap_err(),
        record_not_found("oauth-code")
    );

    let (code, _) = authorize(service, client_id, refresh_id, None).await;
    let valid = confidential_token_request(client_id, client_secret, code);
    let mut invalid_redirect = valid.clone();
    invalid_redirect.redirect_uri = Some("https://wrong.example.com/callback".to_owned());
    assert_eq!(
        service.token(invalid_redirect).await.unwrap_err(),
        ApiError::new(
            ApiErrorType::ParamMalformed,
            "Invalid redirect_uri parameter"
        )
    );
    assert_eq!(
        service.token(valid).await.unwrap_err(),
        record_not_found("oauth-code")
    );

    let verifier = "burn-after-bad-pkce";
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let (code, _) = authorize(
        service,
        client_id,
        refresh_id,
        Some(("S256".to_owned(), challenge)),
    )
    .await;
    let valid = pkce_token_request(client_id, code, verifier);
    let mut invalid_pkce = valid.clone();
    invalid_pkce.code_verifier = Some("wrong-verifier".to_owned());
    assert_eq!(
        service.token(invalid_pkce).await.unwrap_err(),
        ApiError::new(ApiErrorType::ParamMalformed, "Invalid code_verifier")
    );
    assert_eq!(
        service.token(valid).await.unwrap_err(),
        record_not_found("oauth-code")
    );
}

fn assert_grant_error_redirect(destination: &str, error: &str, description: &str) {
    let destination = Url::parse(destination).unwrap();
    assert_eq!(
        destination.as_str().split('?').next().unwrap(),
        "https://tool.example.com/callback"
    );
    assert_eq!(
        destination.query_pairs().collect::<Vec<_>>(),
        vec![
            ("error".into(), error.into()),
            ("error_description".into(), description.into()),
            ("state".into(), "opaque-state".into()),
        ]
    );
}

async fn assert_concurrent_consumers_have_one_winner(
    service: &PlatformOAuthService<PostgresPlatformOAuthStore>,
    client_id: Uuid,
    client_secret: &str,
    refresh_id: Uuid,
) {
    let (redirect, _) = claimed_redirect(service, client_id, refresh_id, None).await;
    let (left, right) = tokio::join!(service.grant(redirect.clone()), service.grant(redirect));
    assert_one_winner(left, right, "oauth-app-redirect");

    let (code, _) = authorize(service, client_id, refresh_id, None).await;
    let request = confidential_token_request(client_id, client_secret, code);
    let (left, right) = tokio::join!(service.token(request.clone()), service.token(request));
    assert_one_winner(left, right, "oauth-code");
}

fn assert_one_winner<T: std::fmt::Debug>(
    left: Result<T, ApiError>,
    right: Result<T, ApiError>,
    artifact: &'static str,
) {
    match (left, right) {
        (Ok(_), Err(error)) | (Err(error), Ok(_)) => {
            assert_eq!(error, record_not_found(artifact));
        }
        outcomes => panic!("expected exactly one successful consumer, got {outcomes:?}"),
    }
}

fn confidential_token_request(
    client_id: Uuid,
    client_secret: &str,
    code: Uuid,
) -> PlatformOAuthTokenRequest {
    PlatformOAuthTokenRequest {
        grant_type: "authorization_code".to_owned(),
        client_id,
        client_secret: Some(client_secret.to_owned()),
        redirect_uri: Some("https://tool.example.com/callback".to_owned()),
        code: Some(code),
        code_verifier: None,
        refresh_token: None,
    }
}

fn pkce_token_request(client_id: Uuid, code: Uuid, verifier: &str) -> PlatformOAuthTokenRequest {
    PlatformOAuthTokenRequest {
        grant_type: "authorization_code".to_owned(),
        client_id,
        client_secret: None,
        redirect_uri: Some("https://tool.example.com/callback".to_owned()),
        code: Some(code),
        code_verifier: Some(verifier.to_owned()),
        refresh_token: None,
    }
}

fn record_not_found(artifact: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Record not found: {artifact}"),
    )
}

async fn authorize(
    service: &PlatformOAuthService<PostgresPlatformOAuthStore>,
    client_id: Uuid,
    refresh_id: Uuid,
    pkce: Option<(String, String)>,
) -> (Uuid, Uuid) {
    let (request, cookie) = claimed_redirect(service, client_id, refresh_id, pkce).await;
    let destination = service.grant(request).await.unwrap();
    let code = Url::parse(&destination)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "code")
        .and_then(|(_, value)| Uuid::parse_str(&value).ok())
        .unwrap();
    (code, cookie)
}

async fn claimed_redirect(
    service: &PlatformOAuthService<PostgresPlatformOAuthStore>,
    client_id: Uuid,
    refresh_id: Uuid,
    pkce: Option<(String, String)>,
) -> (PlatformOAuthDecisionRequest, Uuid) {
    let started = service
        .start(PlatformOAuthStartRequest {
            client_id,
            redirect_uri: "https://tool.example.com/callback".to_owned(),
            scopes: vec!["apps-read".to_owned()],
            state: "opaque-state".to_owned(),
            code_challenge_method: pkce.as_ref().map(|value| value.0.clone()),
            code_challenge: pkce.map(|value| value.1),
        })
        .await
        .unwrap();
    let redirect_id = Url::parse(&started.dashboard_url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "redirect-id")
        .and_then(|(_, value)| Uuid::parse_str(&value).ok())
        .unwrap();
    let claimed = service
        .claim(PlatformOAuthClaimRequest {
            bearer_token: refresh_id.to_string(),
            redirect_id,
        })
        .await
        .unwrap();
    let grant_token = Uuid::parse_str(claimed["grantToken"].as_str().unwrap()).unwrap();
    (
        PlatformOAuthDecisionRequest {
            redirect_id,
            grant_token,
            cookie: Some(started.cookie),
        },
        started.cookie,
    )
}
