//! OAuth 2.0 and `OpenID` Connect protocol primitives.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use base64::{
    Engine as _,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use futures_util::StreamExt as _;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::{Host, Url};

const GOOGLE_ISSUER: &str = "https://accounts.google.com";
const MAX_HTTP_BODY_BYTES: usize = 5 * 1_024 * 1_024;
const APPLE_ISSUERS: [&str; 2] = ["https://account.apple.com", "https://appleid.apple.com"];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DiscoveryDocument {
    pub authorization_endpoint: Option<Url>,
    pub token_endpoint: Option<Url>,
    pub userinfo_endpoint: Option<Url>,
    pub jwks_uri: Option<Url>,
    pub issuer: Option<String>,
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Vec<String>,
}

impl DiscoveryDocument {
    /// Returns the JVM-compatible signing-algorithm default.
    #[must_use]
    pub fn signing_algorithms(&self) -> BTreeSet<String> {
        if self.id_token_signing_alg_values_supported.is_empty() {
            ["HS256".to_owned(), "RS256".to_owned()].into()
        } else {
            self.id_token_signing_alg_values_supported
                .iter()
                .cloned()
                .collect()
        }
    }

    /// Validates every endpoint the server may contact. Domain endpoints must
    /// also pass [`validate_resolved_target`] immediately before connection.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthError::UnsafeUrl`] for unsupported schemes, credentials,
    /// missing hosts, or unsafe literal addresses.
    pub fn validate_server_endpoints(&self) -> Result<(), OAuthError> {
        for endpoint in [
            self.token_endpoint.as_ref(),
            self.userinfo_endpoint.as_ref(),
            self.jwks_uri.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_safe_url(endpoint)?;
        }
        Ok(())
    }
}

/// Fetches and validates an untrusted OIDC discovery document through an
/// address-pinned SSRF-safe HTTP client.
///
/// # Errors
///
/// Returns URL, DNS, HTTP, size, JSON, or unsafe-endpoint errors.
pub async fn fetch_discovery(endpoint: &Url) -> Result<DiscoveryDocument, OAuthError> {
    let body = safe_get(
        endpoint,
        Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15"),
    )
    .await?;
    let document = serde_json::from_slice::<DiscoveryDocument>(&body)?;
    document.validate_server_endpoints()?;
    Ok(document)
}

/// Fetches a bounded JWKS through the same address-pinned HTTP boundary. Only
/// the first 100 keys are retained, matching the JVM implementation.
///
/// # Errors
///
/// Returns URL, DNS, HTTP, size, or JSON errors.
#[tracing::instrument(name = "jwt/get-keys", skip_all, fields(jwks.uri = %endpoint))]
pub async fn fetch_jwks(endpoint: &Url) -> Result<JwkSet, OAuthError> {
    let body = safe_get(endpoint, None).await?;
    let mut keys = serde_json::from_slice::<JwkSet>(&body)?;
    keys.keys.truncate(100);
    Ok(keys)
}

/// Parses a user-controlled server URL and applies the non-DNS SSRF checks.
///
/// # Errors
///
/// Returns a parse error or [`OAuthError::UnsafeUrl`].
pub fn parse_safe_url(value: &str) -> Result<Url, OAuthError> {
    let url = Url::parse(value)?;
    validate_safe_url(&url)?;
    Ok(url)
}

/// Rejects URLs that cannot be safely used as outbound HTTP targets. A domain
/// is not fully validated until all of its resolved addresses have passed
/// [`validate_resolved_target`] at connection time.
///
/// # Errors
///
/// Returns [`OAuthError::UnsafeUrl`] when the target is not HTTP(S), embeds
/// credentials, has no host, or contains an unsafe literal IP address.
pub fn validate_safe_url(url: &Url) -> Result<(), OAuthError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(OAuthError::UnsafeUrl("only HTTP(S) URLs are allowed"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(OAuthError::UnsafeUrl("URL credentials are not allowed"));
    }
    match url.host() {
        Some(Host::Domain(_)) => Ok(()),
        Some(Host::Ipv4(address)) if is_public_ipv4(address) => Ok(()),
        Some(Host::Ipv6(address)) if is_public_ipv6(address) => Ok(()),
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => {
            Err(OAuthError::UnsafeUrl("private or special IP address"))
        }
        None => Err(OAuthError::UnsafeUrl("URL has no host")),
    }
}

/// Checks the addresses produced by DNS resolution. The checked addresses must
/// be the same addresses offered to the HTTP connector; resolving again would
/// reintroduce DNS-rebinding risk.
///
/// # Errors
///
/// Returns [`OAuthError::UnsafeUrl`] for an empty result or any non-public IP.
pub fn validate_resolved_target(
    addresses: impl IntoIterator<Item = IpAddr>,
) -> Result<Vec<IpAddr>, OAuthError> {
    let addresses = addresses.into_iter().collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(OAuthError::UnsafeUrl("host resolved to no addresses"));
    }
    if addresses.iter().any(|address| !is_public_ip(*address)) {
        return Err(OAuthError::UnsafeUrl(
            "host resolved to a private or special IP address",
        ));
    }
    Ok(addresses)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PkceMethod {
    Plain,
    S256,
}

impl PkceMethod {
    fn parse(value: &str) -> Result<Self, OAuthError> {
        match value {
            "plain" => Ok(Self::Plain),
            "S256" => Ok(Self::S256),
            _ => Err(OAuthError::UnknownPkceMethod),
        }
    }
}

/// Verifies the optional PKCE tuple with the JVM server's four-state contract.
///
/// # Errors
///
/// Returns a precise PKCE validation error for incomplete, malformed, unknown,
/// or mismatched challenges.
pub fn verify_pkce(
    challenge_method: Option<&str>,
    challenge: Option<&str>,
    verifier: Option<&str>,
) -> Result<(), OAuthError> {
    let (challenge, verifier) = match (challenge, verifier) {
        (None, None) => return Ok(()),
        (None, Some(_)) => return Err(OAuthError::VerifierWithoutChallenge),
        (Some(_), None) => return Err(OAuthError::ChallengeWithoutVerifier),
        (Some(challenge), Some(verifier)) => (challenge, verifier),
    };
    match challenge_method
        .map(PkceMethod::parse)
        .transpose()?
        .ok_or(OAuthError::UnknownPkceMethod)?
    {
        PkceMethod::Plain if constant_time_eq(verifier.as_bytes(), challenge.as_bytes()) => Ok(()),
        PkceMethod::Plain => Err(OAuthError::PkceMismatch),
        PkceMethod::S256 => {
            let expected = URL_SAFE_NO_PAD
                .decode(challenge)
                .or_else(|_| URL_SAFE.decode(challenge))
                .map_err(|_| OAuthError::InvalidPkceEncoding)?;
            if constant_time_eq(Sha256::digest(verifier.as_bytes()).as_ref(), &expected) {
                Ok(())
            } else {
                Err(OAuthError::PkceMismatch)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdTokenOptions<'a> {
    pub issuer: &'a str,
    /// The client ID used to mint the token plus any extra native-app IDs
    /// registered for the OAuth client. At least one must appear in `aud`.
    pub accepted_audiences: &'a [String],
    pub supported_algorithms: &'a BTreeSet<String>,
    pub nonce: Option<&'a str>,
    pub allow_unverified_email: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn intersects(&self, expected: &[String]) -> bool {
        match self {
            Self::One(value) => expected.iter().any(|expected| value == expected),
            Self::Many(values) => values
                .iter()
                .any(|value| expected.iter().any(|expected| value == expected)),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(rename = "exp")]
    _exp: u64,
    iss: String,
    aud: Audience,
    azp: Option<String>,
    sub: Option<String>,
    email: Option<String>,
    email_verified: Option<JsonValue>,
    picture: Option<String>,
    nonce: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthUserInfo {
    pub subject: String,
    pub email: Option<String>,
    pub image_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthExchangeResult {
    Success(OAuthUserInfo),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct GenericCodeExchange<'a> {
    pub token_endpoint: &'a Url,
    pub userinfo_endpoint: Option<&'a Url>,
    pub client_id: &'a str,
    /// The effective secret. Apple callers pass the short-lived client-secret
    /// JWT rather than the stored private key.
    pub client_secret: &'a str,
    pub code: &'a str,
    pub redirect_uri: &'a str,
}

#[derive(Serialize)]
struct AppleClientSecretClaims<'a> {
    iss: &'a str,
    iat: u64,
    exp: u64,
    aud: [&'static str; 1],
    sub: &'a str,
}

/// Signs Apple's short-lived OAuth client-secret JWT from the stored PKCS#8 EC
/// private key.
///
/// # Errors
///
/// Returns [`OAuthError::AppleClientSecret`] for an invalid private key or a
/// signing failure.
pub fn apple_client_secret(
    client_id: &str,
    team_id: &str,
    key_id: &str,
    private_key_pem: &str,
    now_epoch_seconds: u64,
) -> Result<String, OAuthError> {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id.to_owned());
    let claims = AppleClientSecretClaims {
        iss: team_id,
        iat: now_epoch_seconds,
        exp: now_epoch_seconds.saturating_add(120),
        aud: ["https://appleid.apple.com"],
        sub: client_id,
    };
    let key = EncodingKey::from_ec_pem(private_key_pem.as_bytes())
        .map_err(OAuthError::AppleClientSecret)?;
    encode(&header, &claims, &key).map_err(OAuthError::AppleClientSecret)
}

/// Exchanges an authorization code with a generic OIDC provider. This mirrors
/// the JVM callback path: the code exchange's ID-token payload is decoded but
/// not signature-verified because it arrived directly from the TLS-protected
/// token endpoint; user-supplied ID tokens use [`verify_id_token`] instead.
///
/// # Errors
///
/// Returns URL, DNS, HTTP, size, or JSON transport errors. Provider-declared
/// OAuth failures are returned as [`OAuthExchangeResult::Error`].
pub async fn exchange_generic_code(
    exchange: &GenericCodeExchange<'_>,
) -> Result<OAuthExchangeResult, OAuthError> {
    exchange_generic_code_with(&PinnedHttp, exchange).await
}

async fn exchange_generic_code_with<H: ProviderHttp>(
    http: &H,
    exchange: &GenericCodeExchange<'_>,
) -> Result<OAuthExchangeResult, OAuthError> {
    let response = http
        .post_form(
            exchange.token_endpoint,
            &[
                ("client_id", exchange.client_id),
                ("client_secret", exchange.client_secret),
                ("code", exchange.code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", exchange.redirect_uri),
            ],
            &[],
        )
        .await?;
    let token_body = serde_json::from_slice::<JsonValue>(&response.body).ok();
    if !response.success() {
        return Ok(OAuthExchangeResult::Error(
            token_body
                .as_ref()
                .and_then(|body| body.get("error_description"))
                .and_then(JsonValue::as_str)
                .unwrap_or("Error exchanging code for token.")
                .to_owned(),
        ));
    }
    let Some(token_body) = token_body else {
        return Ok(OAuthExchangeResult::Error(
            "Invalid token exchanging code for token.".to_owned(),
        ));
    };
    let mut user_info = token_body
        .get("id_token")
        .and_then(JsonValue::as_str)
        .and_then(decode_unverified_jwt_payload);
    if user_info.is_none()
        && let (Some(access_token), Some(userinfo_endpoint)) = (
            token_body.get("access_token").and_then(JsonValue::as_str),
            exchange.userinfo_endpoint,
        )
    {
        let authorization = format!("Bearer {access_token}");
        if let Ok(response) = http
            .get(
                userinfo_endpoint,
                &[(AUTHORIZATION, authorization.as_str())],
            )
            .await
            && response.success()
        {
            user_info = serde_json::from_slice(&response.body).ok();
        }
    }
    let Some(user_info) = user_info else {
        return Ok(OAuthExchangeResult::Error(
            "Invalid token exchanging code for token.".to_owned(),
        ));
    };
    Ok(generic_provider_user_info(&user_info).map_or_else(
        || OAuthExchangeResult::Error("Missing user info".to_owned()),
        OAuthExchangeResult::Success,
    ))
}

/// Exchanges a GitHub OAuth code and loads its user and primary verified email.
///
/// # Errors
///
/// Returns URL, DNS, HTTP, size, or JSON transport errors. Provider-declared
/// OAuth failures are returned as [`OAuthExchangeResult::Error`].
pub async fn exchange_github_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<OAuthExchangeResult, OAuthError> {
    exchange_github_code_with(&PinnedHttp, client_id, client_secret, code, redirect_uri).await
}

async fn exchange_github_code_with<H: ProviderHttp>(
    http: &H,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<OAuthExchangeResult, OAuthError> {
    let token_endpoint = Url::parse("https://github.com/login/oauth/access_token")?;
    let user_endpoint = Url::parse("https://api.github.com/user")?;
    let emails_endpoint = Url::parse("https://api.github.com/user/emails")?;
    let common_headers = [
        (ACCEPT, "application/json"),
        (USER_AGENT, "InstantDB OAuth"),
    ];
    let token_response = http
        .post_form(
            &token_endpoint,
            &[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("code", code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
            ],
            &common_headers,
        )
        .await?;
    let token_body = serde_json::from_slice::<JsonValue>(&token_response.body).ok();
    if !token_response.success() {
        return Ok(OAuthExchangeResult::Error(
            token_body
                .as_ref()
                .and_then(|body| body.get("error_description"))
                .and_then(JsonValue::as_str)
                .unwrap_or("Error exchanging code for token.")
                .to_owned(),
        ));
    }
    let Some(access_token) = token_body
        .as_ref()
        .and_then(|body| body.get("access_token"))
        .and_then(JsonValue::as_str)
    else {
        return Ok(OAuthExchangeResult::Error(
            "No access token received from GitHub.".to_owned(),
        ));
    };
    let authorization = format!("Bearer {access_token}");
    let user_headers = [
        (ACCEPT, "application/json"),
        (USER_AGENT, "InstantDB OAuth"),
        (AUTHORIZATION, authorization.as_str()),
    ];
    let user_response = http.get(&user_endpoint, &user_headers).await?;
    let user_body = serde_json::from_slice::<JsonValue>(&user_response.body).ok();
    if !user_response.success() {
        return Ok(OAuthExchangeResult::Error(
            user_body
                .as_ref()
                .and_then(|body| body.get("message"))
                .and_then(JsonValue::as_str)
                .unwrap_or("Failed to fetch user info from GitHub.")
                .to_owned(),
        ));
    }
    let email_response = http.get(&emails_endpoint, &user_headers).await?;
    let email = if email_response.success() {
        serde_json::from_slice::<Vec<JsonValue>>(&email_response.body)
            .ok()
            .and_then(|emails| github_primary_email(&emails))
    } else {
        None
    };
    let Some(user_body) = user_body else {
        return Ok(OAuthExchangeResult::Error(
            "Missing user ID from GitHub".to_owned(),
        ));
    };
    let Some(subject) = user_body.get("id").and_then(json_scalar_string) else {
        return Ok(OAuthExchangeResult::Error(
            "Missing user ID from GitHub".to_owned(),
        ));
    };
    Ok(OAuthExchangeResult::Success(OAuthUserInfo {
        subject,
        email,
        image_url: user_body
            .get("avatar_url")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
    }))
}

/// Verifies an OIDC ID token against an already fetched, SSRF-validated JWKS.
/// At most the first 100 keys are considered, matching the JVM cache bound.
///
/// # Errors
///
/// Returns an OAuth validation error for malformed headers/claims, unsupported
/// algorithms, missing keys, invalid signatures/expiry, or OIDC claim mismatch.
pub fn verify_id_token(
    token: &str,
    jwks: &JwkSet,
    options: &IdTokenOptions<'_>,
) -> Result<OAuthUserInfo, OAuthError> {
    let header = decode_header(token).map_err(OAuthError::InvalidJwt)?;
    let algorithm_name = match header.alg {
        Algorithm::RS256 => "RS256",
        Algorithm::ES256 => "ES256",
        _ => return Err(OAuthError::UnsupportedSigningAlgorithm),
    };
    if !options.supported_algorithms.contains(algorithm_name) {
        return Err(OAuthError::UnsupportedIdTokenAlgorithm);
    }
    let kid = header.kid.as_deref().ok_or(OAuthError::MissingSigningKey)?;
    let jwk = jwks
        .keys
        .iter()
        .take(100)
        .find(|key| key.common.key_id.as_deref() == Some(kid))
        .ok_or(OAuthError::MissingSigningKey)?;
    let key = DecodingKey::from_jwk(jwk).map_err(OAuthError::InvalidJwt)?;
    let mut validation = Validation::new(header.alg);
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    validation.validate_aud = false;
    let claims = decode::<IdTokenClaims>(token, &key, &validation)
        .map_err(OAuthError::InvalidJwt)?
        .claims;
    if !issuers_match(options.issuer, &claims.iss) {
        return Err(OAuthError::IssuerMismatch);
    }
    let authorized_party_matches = claims.azp.as_deref().is_none_or(|value| {
        options
            .accepted_audiences
            .iter()
            .any(|audience| audience == value)
    });
    let multiple_audiences_have_authorized_party =
        !matches!(&claims.aud, Audience::Many(values) if values.len() > 1) || claims.azp.is_some();
    if !claims.aud.intersects(options.accepted_audiences)
        || !authorized_party_matches
        || !multiple_audiences_have_authorized_party
    {
        return Err(OAuthError::AudienceMismatch);
    }
    validate_nonce(options.issuer, options.nonce, claims.nonce.as_deref())?;
    let subject = claims.sub.ok_or(OAuthError::MissingSubject)?;
    let email_verified = claims.email_verified.as_ref().and_then(JsonValue::as_bool) == Some(true);
    Ok(OAuthUserInfo {
        subject,
        email: (options.allow_unverified_email || email_verified)
            .then_some(claims.email)
            .flatten(),
        image_url: claims.picture,
    })
}

/// Builds the generic OIDC authorization URL. Only Google's `hd` extension is
/// copied from caller-controlled extra parameters.
#[must_use]
pub fn generic_authorization_url(
    endpoint: &Url,
    state: &str,
    redirect_uri: &str,
    client_id: &str,
    hosted_domain: Option<&str>,
) -> Url {
    let mut result = endpoint.clone();
    result.query_pairs_mut().extend_pairs([
        ("scope", "email openid"),
        ("response_type", "code"),
        ("response_mode", "form_post"),
        ("state", state),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
    ]);
    if let Some(hosted_domain) = hosted_domain {
        result.query_pairs_mut().append_pair("hd", hosted_domain);
    }
    result
}

fn validate_nonce(
    issuer: &str,
    requested: Option<&str>,
    token_nonce: Option<&str>,
) -> Result<(), OAuthError> {
    let requested = requested.filter(|value| !value.trim().is_empty());
    if issuer == GOOGLE_ISSUER && requested.is_none() {
        return Ok(());
    }
    let token_nonce = token_nonce.filter(|value| !value.trim().is_empty());
    match (requested, token_nonce) {
        (None, None) => Ok(()),
        (Some(requested), Some(actual))
            if constant_time_eq(requested.as_bytes(), actual.as_bytes()) =>
        {
            Ok(())
        }
        (Some(requested), Some(actual)) => {
            let hashed = super::hex_digest(Sha256::digest(requested.as_bytes()));
            if constant_time_eq(hashed.as_bytes(), actual.as_bytes()) {
                Ok(())
            } else {
                Err(OAuthError::NonceMismatch)
            }
        }
        (Some(_), None) => Err(OAuthError::MissingNonce),
        (None, Some(_)) => Err(OAuthError::RequestOmittedNonce),
    }
}

async fn safe_get(endpoint: &Url, user_agent: Option<&str>) -> Result<Vec<u8>, OAuthError> {
    let headers = user_agent.map_or_else(Vec::new, |value| vec![(USER_AGENT, value)]);
    let response = safe_get_response(endpoint, &headers).await?;
    if response.success() {
        Ok(response.body)
    } else {
        Err(OAuthError::HttpStatus(response.status))
    }
}

struct SafeHttpResponse {
    status: u16,
    body: Vec<u8>,
}

trait ProviderHttp {
    async fn get(
        &self,
        endpoint: &Url,
        headers: &[(reqwest::header::HeaderName, &str)],
    ) -> Result<SafeHttpResponse, OAuthError>;

    async fn post_form(
        &self,
        endpoint: &Url,
        form: &[(&str, &str)],
        headers: &[(reqwest::header::HeaderName, &str)],
    ) -> Result<SafeHttpResponse, OAuthError>;
}

struct PinnedHttp;

impl ProviderHttp for PinnedHttp {
    async fn get(
        &self,
        endpoint: &Url,
        headers: &[(reqwest::header::HeaderName, &str)],
    ) -> Result<SafeHttpResponse, OAuthError> {
        safe_get_response(endpoint, headers).await
    }

    async fn post_form(
        &self,
        endpoint: &Url,
        form: &[(&str, &str)],
        headers: &[(reqwest::header::HeaderName, &str)],
    ) -> Result<SafeHttpResponse, OAuthError> {
        safe_post_form(endpoint, form, headers).await
    }
}

impl SafeHttpResponse {
    fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

async fn safe_get_response(
    endpoint: &Url,
    headers: &[(reqwest::header::HeaderName, &str)],
) -> Result<SafeHttpResponse, OAuthError> {
    validate_safe_url(endpoint)?;
    let client = pinned_client(endpoint).await?;
    let mut request = client.get(endpoint.clone());
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    read_response(request.send().await?).await
}

async fn safe_post_form(
    endpoint: &Url,
    form: &[(&str, &str)],
    headers: &[(reqwest::header::HeaderName, &str)],
) -> Result<SafeHttpResponse, OAuthError> {
    validate_safe_url(endpoint)?;
    let client = pinned_client(endpoint).await?;
    let mut request = client.post(endpoint.clone()).form(form);
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    read_response(request.send().await?).await
}

async fn read_response(response: reqwest::Response) -> Result<SafeHttpResponse, OAuthError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_BODY_BYTES as u64)
    {
        return Err(OAuthError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
            return Err(OAuthError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(SafeHttpResponse { status, body })
}

async fn pinned_client(endpoint: &Url) -> Result<reqwest::Client, OAuthError> {
    let host = endpoint
        .host()
        .ok_or(OAuthError::UnsafeUrl("URL has no host"))?;
    let port = endpoint
        .port_or_known_default()
        .ok_or(OAuthError::UnsafeUrl("URL has no known port"))?;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20));
    if let Host::Domain(domain) = host {
        let resolved = tokio::net::lookup_host((domain, port))
            .await?
            .map(|address| address.ip())
            .collect::<Vec<_>>();
        let validated = validate_resolved_target(resolved)?;
        let addresses = validated
            .into_iter()
            .map(|address| SocketAddr::new(address, port))
            .collect::<Vec<_>>();
        builder = builder.resolve_to_addrs(domain, &addresses);
    }
    Ok(builder.build()?)
}

fn decode_unverified_jwt_payload(token: &str) -> Option<JsonValue> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn generic_provider_user_info(value: &JsonValue) -> Option<OAuthUserInfo> {
    let subject = value.get("sub")?.as_str()?.to_owned();
    let verified = value.get("email_verified").is_some_and(clojure_truthy);
    let email = verified
        .then(|| {
            value
                .get("email")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        })
        .flatten()?;
    Some(OAuthUserInfo {
        subject,
        email: Some(email),
        image_url: value
            .get("picture")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
    })
}

fn clojure_truthy(value: &JsonValue) -> bool {
    !matches!(value, JsonValue::Null | JsonValue::Bool(false))
}

fn json_scalar_string(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) => Some(value.clone()),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn github_primary_email(emails: &[JsonValue]) -> Option<String> {
    emails.iter().find_map(|value| {
        (value.get("verified").and_then(JsonValue::as_bool) == Some(true)
            && value.get("primary").and_then(JsonValue::as_bool) == Some(true))
        .then(|| {
            value
                .get("email")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        })
        .flatten()
    })
}

fn issuers_match(configured: &str, actual: &str) -> bool {
    configured == actual || (APPLE_ISSUERS.contains(&configured) && APPLE_ISSUERS.contains(&actual))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_unspecified()
        || a == 0
        || a == 100 && (64..=127).contains(&b)
        || a == 192 && b == 0 && c == 0
        || a == 192 && b == 0 && c == 2
        || a == 192 && b == 88 && c == 99
        || a == 198 && b == 18 && c <= 19
        || a == 198 && b == 51 && c == 100
        || a == 203 && b == 0 && c == 113
        || a >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || segments[0] == 0x2001 && segments[1] == 0x0db8
        || address
            .to_ipv4()
            .is_some_and(|embedded| !is_public_ipv4(embedded)))
}

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("unsafe outbound URL: {0}")]
    UnsafeUrl(&'static str),
    #[error("DNS resolution failed: {0}")]
    Dns(#[from] std::io::Error),
    #[error("OAuth HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OAuth endpoint returned HTTP {0}")]
    HttpStatus(u16),
    #[error("OAuth endpoint response exceeded the size limit")]
    ResponseTooLarge,
    #[error("invalid OAuth JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("the code_verifier was provided, but no code_challenge was provided")]
    VerifierWithoutChallenge,
    #[error("the code_challenge was provided, but no code_verifier was provided")]
    ChallengeWithoutVerifier,
    #[error("unknown code challenge method")]
    UnknownPkceMethod,
    #[error("invalid code_verifier; expected a URL-safe Base64 string")]
    InvalidPkceEncoding,
    #[error("the code_challenge and code_verifier do not match")]
    PkceMismatch,
    #[error("unsupported JWT signing algorithm")]
    UnsupportedSigningAlgorithm,
    #[error("the id_token used an unsupported algorithm")]
    UnsupportedIdTokenAlgorithm,
    #[error("could not find the JWT signing key")]
    MissingSigningKey,
    #[error("invalid JWT: {0}")]
    InvalidJwt(#[source] jsonwebtoken::errors::Error),
    #[error("could not create Apple client secret: {0}")]
    AppleClientSecret(#[source] jsonwebtoken::errors::Error),
    #[error("the id_token issuer does not match the OAuth provider")]
    IssuerMismatch,
    #[error("the id_token was generated for the wrong OAuth client")]
    AudienceMismatch,
    #[error("the id_token had no subject")]
    MissingSubject,
    #[error("the id_token is missing a nonce")]
    MissingNonce,
    #[error("the nonce parameter was not provided in the request")]
    RequestOmittedNonce,
    #[error("the nonces do not match")]
    NonceMismatch,
}

impl OAuthError {
    /// Returns whether the error text describes the caller's own request.
    ///
    /// Protocol and token validation failures explain the app developer's
    /// OAuth integration and stay in the response. Transport, discovery, and
    /// signing-key failures can carry provider URLs or infrastructure detail,
    /// so callers must log the scrubbed cause and return a fixed message.
    #[must_use]
    pub fn is_client_facing(&self) -> bool {
        match self {
            Self::VerifierWithoutChallenge
            | Self::ChallengeWithoutVerifier
            | Self::UnknownPkceMethod
            | Self::InvalidPkceEncoding
            | Self::PkceMismatch
            | Self::UnsupportedIdTokenAlgorithm
            | Self::IssuerMismatch
            | Self::AudienceMismatch
            | Self::MissingSubject
            | Self::MissingNonce
            | Self::RequestOmittedNonce
            | Self::NonceMismatch
            | Self::InvalidJwt(_) => true,
            Self::InvalidUrl(_)
            | Self::UnsafeUrl(_)
            | Self::Dns(_)
            | Self::Http(_)
            | Self::HttpStatus(_)
            | Self::ResponseTooLarge
            | Self::InvalidJson(_)
            | Self::UnsupportedSigningAlgorithm
            | Self::MissingSigningKey
            | Self::AppleClientSecret(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr};

    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use serde_json::json;

    use super::*;

    struct FakeHttp {
        responses: RefCell<VecDeque<SafeHttpResponse>>,
        requests: RefCell<Vec<String>>,
    }

    impl FakeHttp {
        fn new(responses: impl IntoIterator<Item = (u16, JsonValue)>) -> Self {
            Self {
                responses: RefCell::new(
                    responses
                        .into_iter()
                        .map(|(status, body)| SafeHttpResponse {
                            status,
                            body: serde_json::to_vec(&body).unwrap(),
                        })
                        .collect(),
                ),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn response(&self) -> SafeHttpResponse {
            self.responses
                .borrow_mut()
                .pop_front()
                .expect("fake response")
        }
    }

    impl ProviderHttp for FakeHttp {
        fn get(
            &self,
            endpoint: &Url,
            headers: &[(reqwest::header::HeaderName, &str)],
        ) -> impl Future<Output = Result<SafeHttpResponse, OAuthError>> {
            self.requests.borrow_mut().push(format!(
                "GET {endpoint} {}",
                headers
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            std::future::ready(Ok(self.response()))
        }

        fn post_form(
            &self,
            endpoint: &Url,
            form: &[(&str, &str)],
            _headers: &[(reqwest::header::HeaderName, &str)],
        ) -> impl Future<Output = Result<SafeHttpResponse, OAuthError>> {
            self.requests.borrow_mut().push(format!(
                "POST {endpoint} {}",
                form.iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            std::future::ready(Ok(self.response()))
        }
    }

    #[test]
    fn ssrf_guard_rejects_non_http_and_special_addresses() {
        for value in [
            "not-a-url",
            "ftp://safe.example.com/a",
            "file:///etc/passwd",
            "http://127.0.0.1/a",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/a",
            "https://user:pass@safe.example.com/a",
        ] {
            assert!(parse_safe_url(value).is_err(), "accepted {value}");
        }
        assert!(parse_safe_url("https://safe.example.com/oauth").is_ok());
        assert!(validate_resolved_target([IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]).is_ok());
        assert!(validate_resolved_target([IpAddr::V4(Ipv4Addr::LOCALHOST)]).is_err());
    }

    #[test]
    fn discovery_checks_every_server_fetched_endpoint() {
        let safe = DiscoveryDocument {
            authorization_endpoint: None,
            token_endpoint: Some(Url::parse("https://safe.example.com/token").unwrap()),
            userinfo_endpoint: Some(Url::parse("https://safe.example.com/user").unwrap()),
            jwks_uri: Some(Url::parse("https://safe.example.com/keys").unwrap()),
            issuer: Some("https://safe.example.com".to_owned()),
            id_token_signing_alg_values_supported: Vec::new(),
        };
        assert!(safe.validate_server_endpoints().is_ok());
        assert_eq!(
            safe.signing_algorithms(),
            ["HS256".to_owned(), "RS256".to_owned()].into()
        );
        for field in 0..3 {
            let mut unsafe_document = safe.clone();
            let unsafe_url = Url::parse("http://169.254.169.254/metadata").unwrap();
            match field {
                0 => unsafe_document.token_endpoint = Some(unsafe_url),
                1 => unsafe_document.userinfo_endpoint = Some(unsafe_url),
                _ => unsafe_document.jwks_uri = Some(unsafe_url),
            }
            assert!(unsafe_document.validate_server_endpoints().is_err());
        }
    }

    #[test]
    fn pkce_matches_plain_and_s256_contract() {
        assert!(verify_pkce(None, None, None).is_ok());
        assert!(matches!(
            verify_pkce(None, None, Some("verifier")),
            Err(OAuthError::VerifierWithoutChallenge)
        ));
        assert!(matches!(
            verify_pkce(Some("plain"), Some("verifier"), None),
            Err(OAuthError::ChallengeWithoutVerifier)
        ));
        assert!(verify_pkce(Some("plain"), Some("verifier"), Some("verifier")).is_ok());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(b"verifier"));
        assert!(verify_pkce(Some("S256"), Some(&challenge), Some("verifier")).is_ok());
        assert!(matches!(
            verify_pkce(Some("S256"), Some("***"), Some("verifier")),
            Err(OAuthError::InvalidPkceEncoding)
        ));
    }

    #[test]
    fn authorization_url_has_exact_generic_parameters() {
        let endpoint = Url::parse("https://accounts.example.com/authorize?existing=yes").unwrap();
        let result = generic_authorization_url(
            &endpoint,
            "state",
            "https://app.example.com/callback",
            "client",
            Some("example.com"),
        );
        let pairs = result.query_pairs().collect::<Vec<_>>();
        assert!(pairs.contains(&("scope".into(), "email openid".into())));
        assert!(pairs.contains(&("response_mode".into(), "form_post".into())));
        assert!(pairs.contains(&("hd".into(), "example.com".into())));
        assert!(pairs.contains(&("existing".into(), "yes".into())));
    }

    #[test]
    fn nonces_are_checked_for_every_issuer_when_the_client_supplied_one() {
        assert!(issuers_match(APPLE_ISSUERS[0], APPLE_ISSUERS[1]));
        assert!(validate_nonce(GOOGLE_ISSUER, Some("a"), Some("a")).is_ok());
        assert!(matches!(
            validate_nonce(GOOGLE_ISSUER, Some("a"), Some("b")),
            Err(OAuthError::NonceMismatch)
        ));
        assert!(matches!(
            validate_nonce(GOOGLE_ISSUER, Some("a"), None),
            Err(OAuthError::MissingNonce)
        ));
        assert!(validate_nonce(GOOGLE_ISSUER, None, Some("b")).is_ok());
        assert!(validate_nonce(GOOGLE_ISSUER, None, None).is_ok());
        assert!(matches!(
            validate_nonce(APPLE_ISSUERS[0], None, Some("b")),
            Err(OAuthError::RequestOmittedNonce)
        ));
        assert!(validate_nonce(APPLE_ISSUERS[0], None, None).is_ok());
        let hashed = super::super::hex_digest(Sha256::digest(b"nonce"));
        assert!(validate_nonce(APPLE_ISSUERS[0], Some("nonce"), Some(&hashed)).is_ok());
    }

    #[test]
    fn provider_exchange_payload_parsing_matches_jvm_truthiness() {
        let payload = json!({
            "sub": "subject",
            "email": "user@example.com",
            "email_verified": "false",
            "picture": "https://example.com/avatar.png"
        });
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        assert_eq!(decode_unverified_jwt_payload(&token), Some(payload.clone()));
        assert_eq!(
            generic_provider_user_info(&payload),
            Some(OAuthUserInfo {
                subject: "subject".to_owned(),
                email: Some("user@example.com".to_owned()),
                image_url: Some("https://example.com/avatar.png".to_owned()),
            })
        );
        let mut unverified = payload;
        unverified["email_verified"] = JsonValue::Bool(false);
        assert_eq!(generic_provider_user_info(&unverified), None);
    }

    #[test]
    fn github_primary_email_requires_both_flags() {
        let emails = json!([
            {"email": "primary-unverified@example.com", "primary": true, "verified": false},
            {"email": "verified-secondary@example.com", "primary": false, "verified": true},
            {"email": "chosen@example.com", "primary": true, "verified": true}
        ]);
        assert_eq!(
            github_primary_email(emails.as_array().unwrap()).as_deref(),
            Some("chosen@example.com")
        );
        assert_eq!(json_scalar_string(&json!(42)).as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn generic_exchange_posts_exact_fields_and_falls_back_to_userinfo() {
        let http = FakeHttp::new([
            (200, json!({"access_token": "access"})),
            (
                200,
                json!({
                    "sub": "subject",
                    "email": "user@example.com",
                    "email_verified": true,
                    "picture": "https://example.com/avatar.png"
                }),
            ),
        ]);
        let token_endpoint = Url::parse("https://issuer.example.com/token").unwrap();
        let userinfo_endpoint = Url::parse("https://issuer.example.com/userinfo").unwrap();
        let result = exchange_generic_code_with(
            &http,
            &GenericCodeExchange {
                token_endpoint: &token_endpoint,
                userinfo_endpoint: Some(&userinfo_endpoint),
                client_id: "client",
                client_secret: "secret",
                code: "code",
                redirect_uri: "https://app.example.com/callback",
            },
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            OAuthExchangeResult::Success(OAuthUserInfo {
                subject: "subject".to_owned(),
                email: Some("user@example.com".to_owned()),
                image_url: Some("https://example.com/avatar.png".to_owned()),
            })
        );
        let requests = http.requests.borrow();
        assert!(requests[0].contains("client_secret=secret"));
        assert!(requests[0].contains("grant_type=authorization_code"));
        assert!(requests[1].contains("authorization=Bearer access"));
    }

    #[tokio::test]
    async fn linkedin_exchange_accepts_its_oidc_id_token_claims() {
        let claims = json!({
            "iss": "https://www.linkedin.com/oauth",
            "aud": "linkedin-client",
            "sub": "linkedin-subject",
            "email": "person@example.com",
            "email_verified": true,
            "picture": "https://example.com/linkedin.png"
        });
        let id_token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let http = FakeHttp::new([(
            200,
            json!({"access_token": "linkedin-access", "id_token": id_token}),
        )]);
        let token_endpoint = Url::parse("https://www.linkedin.com/oauth/v2/accessToken").unwrap();
        let result = exchange_generic_code_with(
            &http,
            &GenericCodeExchange {
                token_endpoint: &token_endpoint,
                userinfo_endpoint: None,
                client_id: "linkedin-client",
                client_secret: "secret",
                code: "code",
                redirect_uri: "https://api.example.com/runtime/oauth/callback",
            },
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            OAuthExchangeResult::Success(OAuthUserInfo {
                subject: "linkedin-subject".to_owned(),
                email: Some("person@example.com".to_owned()),
                image_url: Some("https://example.com/linkedin.png".to_owned()),
            })
        );
    }

    #[tokio::test]
    async fn github_exchange_requires_primary_verified_email_but_not_email_presence() {
        let http = FakeHttp::new([
            (200, json!({"access_token": "github-token"})),
            (
                200,
                json!({"id": 42, "avatar_url": "https://example.com/avatar.png"}),
            ),
            (
                200,
                json!([
                    {"email": "secondary@example.com", "primary": false, "verified": true},
                    {"email": "primary@example.com", "primary": true, "verified": true}
                ]),
            ),
        ]);
        assert_eq!(
            exchange_github_code_with(
                &http,
                "client",
                "secret",
                "code",
                "https://app.example.com/callback"
            )
            .await
            .unwrap(),
            OAuthExchangeResult::Success(OAuthUserInfo {
                subject: "42".to_owned(),
                email: Some("primary@example.com".to_owned()),
                image_url: Some("https://example.com/avatar.png".to_owned()),
            })
        );
        let requests = http.requests.borrow();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("authorization=Bearer github-token"));
        assert!(requests[2].contains("authorization=Bearer github-token"));
    }

    #[test]
    fn es256_id_tokens_are_verified_before_claims_are_trusted() {
        let private_key = STANDARD
            .decode(concat!(
                "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtgejda/UoPuAPpAh",
                "TlcRSN8/MrRU9cE7XcbxSWANqvahRANCAASApwdGtLB7wbI/DW8xPJ2fJczvirtP",
                "Lhh4Zj23x8h6VsNggHAQ+yZM/4iyJGouweDSrZ/9mLKnI+bnRz2TAtZi"
            ))
            .unwrap();
        let jwks = serde_json::from_value::<JwkSet>(json!({
            "keys": [{
                "kty": "EC",
                "crv": "P-256",
                "kid": "test-key",
                "use": "sig",
                "alg": "ES256",
                "x": "gKcHRrSwe8GyPw1vMTydnyXM74q7Ty4YeGY9t8fIelY",
                "y": "w2CAcBD7Jkz_iLIkai7B4NKtn_2Ysqcj5udHPZMC1mI"
            }]
        }))
        .unwrap();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("test-key".to_owned());
        let mut claims = json!({
            "exp": jsonwebtoken::get_current_timestamp() + 3_600,
            "iss": APPLE_ISSUERS[1],
            "aud": ["client", "another-client"],
            "azp": "client",
            "sub": "subject",
            "email": "user@example.com",
            "email_verified": true,
            "picture": "https://example.com/image.png",
            "nonce": super::super::hex_digest(Sha256::digest(b"nonce"))
        });
        let token = encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        let supported = ["ES256".to_owned()].into();
        let accepted = ["client".to_owned()];
        let options = IdTokenOptions {
            issuer: APPLE_ISSUERS[0],
            accepted_audiences: &accepted,
            supported_algorithms: &supported,
            nonce: Some("nonce"),
            allow_unverified_email: false,
        };
        assert_eq!(
            verify_id_token(&token, &jwks, &options).unwrap(),
            OAuthUserInfo {
                subject: "subject".to_owned(),
                email: Some("user@example.com".to_owned()),
                image_url: Some("https://example.com/image.png".to_owned()),
            }
        );

        claims["aud"] = json!(["client"]);
        claims.as_object_mut().unwrap().remove("azp");
        let single_audience =
            encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        assert!(verify_id_token(&single_audience, &jwks, &options).is_ok());

        claims["aud"] = json!(["client", "another-client"]);
        let missing_azp =
            encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        assert!(matches!(
            verify_id_token(&missing_azp, &jwks, &options),
            Err(OAuthError::AudienceMismatch)
        ));

        claims["azp"] = json!("another-client");
        let conflicting_azp =
            encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        assert!(matches!(
            verify_id_token(&conflicting_azp, &jwks, &options),
            Err(OAuthError::AudienceMismatch)
        ));

        claims["aud"] = json!("foreign-client");
        claims.as_object_mut().unwrap().remove("azp");
        let foreign_audience =
            encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        assert!(matches!(
            verify_id_token(&foreign_audience, &jwks, &options),
            Err(OAuthError::AudienceMismatch)
        ));

        claims["aud"] = json!("native-client");
        let extra_audience =
            encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        let accepted_with_native = ["client".to_owned(), "native-client".to_owned()];
        let options_with_native = IdTokenOptions {
            accepted_audiences: &accepted_with_native,
            ..options.clone()
        };
        assert!(verify_id_token(&extra_audience, &jwks, &options_with_native).is_ok());

        claims.as_object_mut().unwrap().remove("aud");
        let missing_audience =
            encode(&header, &claims, &EncodingKey::from_ec_der(&private_key)).unwrap();
        assert!(verify_id_token(&missing_audience, &jwks, &options).is_err());

        let mut tampered = token.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        assert!(verify_id_token(std::str::from_utf8(&tampered).unwrap(), &jwks, &options).is_err());
    }

    #[test]
    fn apple_client_secret_has_the_exact_short_lived_claim_contract() {
        let private_key = concat!(
            "-----BEGIN PRIVATE KEY-----\n",
            "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtgejda/UoPuAPpAh\n",
            "TlcRSN8/MrRU9cE7XcbxSWANqvahRANCAASApwdGtLB7wbI/DW8xPJ2fJczvirtP\n",
            "Lhh4Zj23x8h6VsNggHAQ+yZM/4iyJGouweDSrZ/9mLKnI+bnRz2TAtZi\n",
            "-----END PRIVATE KEY-----\n"
        );
        let now = jsonwebtoken::get_current_timestamp();
        let token = apple_client_secret("client", "team", "key-id", private_key, now).unwrap();
        assert_eq!(
            decode_header(&token).unwrap().kid.as_deref(),
            Some("key-id")
        );
        let jwk = serde_json::from_value::<jsonwebtoken::jwk::Jwk>(json!({
            "kty": "EC",
            "crv": "P-256",
            "kid": "key-id",
            "use": "sig",
            "alg": "ES256",
            "x": "gKcHRrSwe8GyPw1vMTydnyXM74q7Ty4YeGY9t8fIelY",
            "y": "w2CAcBD7Jkz_iLIkai7B4NKtn_2Ysqcj5udHPZMC1mI"
        }))
        .unwrap();
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&["team"]);
        validation.set_audience(&["https://appleid.apple.com"]);
        let claims =
            decode::<JsonValue>(&token, &DecodingKey::from_jwk(&jwk).unwrap(), &validation)
                .unwrap()
                .claims;
        assert_eq!(claims["sub"], "client");
        assert_eq!(claims["iat"], now);
        assert_eq!(claims["exp"], now + 120);
    }

    #[test]
    fn oauth_error_classification_covers_every_variant() {
        let parse_error = "http://[".parse::<Url>().unwrap_err();
        let json_error = serde_json::from_str::<JsonValue>("{").unwrap_err();
        let http_error = reqwest::Client::new()
            .get("http://[")
            .build()
            .expect_err("invalid URL is rejected");
        let signing_error =
            || jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidToken);
        let cases = [
            (OAuthError::InvalidUrl(parse_error), false),
            (
                OAuthError::UnsafeUrl("private or special IP address"),
                false,
            ),
            (
                OAuthError::Dns(std::io::Error::other("lookup failed")),
                false,
            ),
            (OAuthError::Http(http_error), false),
            (OAuthError::HttpStatus(502), false),
            (OAuthError::ResponseTooLarge, false),
            (OAuthError::InvalidJson(json_error), false),
            (OAuthError::VerifierWithoutChallenge, true),
            (OAuthError::ChallengeWithoutVerifier, true),
            (OAuthError::UnknownPkceMethod, true),
            (OAuthError::InvalidPkceEncoding, true),
            (OAuthError::PkceMismatch, true),
            (OAuthError::UnsupportedSigningAlgorithm, false),
            (OAuthError::UnsupportedIdTokenAlgorithm, true),
            (OAuthError::MissingSigningKey, false),
            (OAuthError::InvalidJwt(signing_error()), true),
            (OAuthError::AppleClientSecret(signing_error()), false),
            (OAuthError::IssuerMismatch, true),
            (OAuthError::AudienceMismatch, true),
            (OAuthError::MissingSubject, true),
            (OAuthError::MissingNonce, true),
            (OAuthError::RequestOmittedNonce, true),
            (OAuthError::NonceMismatch, true),
        ];
        for (error, client_facing) in cases {
            assert_eq!(error.is_client_facing(), client_facing, "{error}");
        }
    }
}
