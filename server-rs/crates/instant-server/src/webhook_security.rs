use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use futures_util::StreamExt as _;
use instant_auth::oauth::{OAuthError, parse_safe_url, validate_resolved_target};
use instant_http::{ApiError, ApiErrorType, WebhookPayloadRequest};
use instant_wal::Isn;
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Host;
use uuid::Uuid;

use crate::{
    Ed25519PublicJwk, WebhookAttempt, WebhookDeliveryRequest, WebhookPayloadAuthorizer,
    WebhookPayloadTokenIssuer, WebhookTransport,
};

const PAYLOAD_TOKEN_LIFETIME_SECONDS: i64 = 3_600;
const MAX_RESPONSE_PREVIEW_BYTES: usize = 256;

#[derive(Debug, Error)]
pub enum WebhookSecurityError {
    #[error("webhook issuer must not be empty")]
    EmptyIssuer,
    #[error("webhook key ID must not be empty")]
    EmptyKeyId,
    #[error("invalid Ed25519 webhook key: {0}")]
    InvalidKey(#[source] jsonwebtoken::errors::Error),
    #[error("webhook signing failed: {0}")]
    Signing(#[source] jsonwebtoken::errors::Error),
    #[error("webhook signature encoding is invalid")]
    InvalidSignatureEncoding,
    #[error("webhook HTTP client could not be built: {0}")]
    HttpClient(#[source] reqwest::Error),
    #[error("invalid Tink webhook keyset JSON: {0}")]
    InvalidTinkJson(#[source] serde_json::Error),
    #[error("Tink webhook keyset has no enabled RAW Ed25519 primary key")]
    MissingTinkPrimary,
    #[error("Tink webhook key {key_id} is invalid: {reason}")]
    InvalidTinkKey { key_id: u32, reason: &'static str },
    #[error("webhook target policy could not be loaded: {0}")]
    TargetPolicy(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PayloadClaims {
    iss: String,
    sub: String,
    exp: u64,
    #[serde(rename = "app-id")]
    app_id: Uuid,
    #[serde(rename = "webhook-id")]
    webhook_id: Uuid,
    isn: String,
}

/// Ed25519 material shared by webhook payload JWTs, delivery signatures, and JWKS.
#[derive(Clone)]
pub struct Ed25519WebhookKeys {
    issuer: String,
    key_id: String,
    encoding_key: EncodingKey,
    decoding_keys: HashMap<String, DecodingKey>,
    primary_public_jwk: Ed25519PublicJwk,
    public_jwks: Vec<Ed25519PublicJwk>,
}

impl Ed25519WebhookKeys {
    /// Loads a PKCS#8 private key and `SubjectPublicKeyInfo` public key from PEM.
    ///
    /// # Errors
    ///
    /// Returns an error for empty identity fields or malformed/non-Ed25519 keys.
    pub fn from_pem(
        issuer: impl Into<String>,
        key_id: impl Into<String>,
        private_key_pem: &[u8],
        public_key_pem: &[u8],
    ) -> Result<Self, WebhookSecurityError> {
        let issuer = issuer.into();
        if issuer.is_empty() {
            return Err(WebhookSecurityError::EmptyIssuer);
        }
        let key_id = key_id.into();
        if key_id.is_empty() {
            return Err(WebhookSecurityError::EmptyKeyId);
        }
        let encoding_key =
            EncodingKey::from_ed_pem(private_key_pem).map_err(WebhookSecurityError::InvalidKey)?;
        let decoding_key =
            DecodingKey::from_ed_pem(public_key_pem).map_err(WebhookSecurityError::InvalidKey)?;
        let public_jwk = public_jwk(&key_id, &decoding_key)?;
        Ok(Self {
            issuer,
            key_id: key_id.clone(),
            encoding_key,
            decoding_keys: HashMap::from([(key_id, decoding_key)]),
            primary_public_jwk: public_jwk.clone(),
            public_jwks: vec![public_jwk],
        })
    }

    /// Imports the cleartext JSON emitted by Tink's `serializeKeyset`, including
    /// every enabled verification key and the declared primary signing key.
    /// KMS-encrypted keysets must be decrypted before this boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON/protobuf, non-RAW keys, unsupported
    /// key material, or a missing/disabled primary key.
    pub fn from_tink_json(
        issuer: impl Into<String>,
        keyset_json: &str,
    ) -> Result<Self, WebhookSecurityError> {
        let issuer = issuer.into();
        if issuer.is_empty() {
            return Err(WebhookSecurityError::EmptyIssuer);
        }
        let keyset = serde_json::from_str::<TinkKeyset>(keyset_json)
            .map_err(WebhookSecurityError::InvalidTinkJson)?;
        let mut primary = None;
        let mut decoding_keys = HashMap::new();
        let mut public_jwks = Vec::new();
        for key in keyset.keys {
            if key.status != "ENABLED" {
                continue;
            }
            validate_tink_key_metadata(&key)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&key.key_data.value)
                .map_err(|_| WebhookSecurityError::InvalidTinkKey {
                    key_id: key.key_id,
                    reason: "key data is not standard Base64",
                })?;
            let (private_seed, public_bytes) = decode_tink_ed25519_private(&bytes).ok_or(
                WebhookSecurityError::InvalidTinkKey {
                    key_id: key.key_id,
                    reason: "key data is not an Ed25519PrivateKey protobuf",
                },
            )?;
            let encoding_key = EncodingKey::from_ed_der(&ed25519_private_der(&private_seed));
            let decoding_key = DecodingKey::from_ed_der(&public_bytes);
            let key_id = key.key_id.to_string();
            public_jwks.push(public_jwk(&key_id, &decoding_key)?);
            if decoding_keys.insert(key_id.clone(), decoding_key).is_some() {
                return Err(WebhookSecurityError::InvalidTinkKey {
                    key_id: key.key_id,
                    reason: "key ID is duplicated",
                });
            }
            validate_key_pair(key.key_id, &encoding_key, &decoding_keys[&key_id])?;
            if key.key_id == keyset.primary_key_id {
                primary = Some((key_id, encoding_key));
            }
        }
        let Some((key_id, encoding_key)) = primary else {
            return Err(WebhookSecurityError::MissingTinkPrimary);
        };
        public_jwks.sort_by(|left, right| left.kid.cmp(&right.kid));
        let primary_public_jwk = public_jwks
            .iter()
            .find(|jwk| jwk.kid == key_id)
            .cloned()
            .ok_or(WebhookSecurityError::MissingTinkPrimary)?;
        Ok(Self {
            issuer,
            key_id,
            encoding_key,
            decoding_keys,
            primary_public_jwk,
            public_jwks,
        })
    }

    #[must_use]
    pub fn public_jwk(&self) -> Ed25519PublicJwk {
        self.primary_public_jwk.clone()
    }

    #[must_use]
    pub fn public_jwks(&self) -> Vec<Ed25519PublicJwk> {
        self.public_jwks.clone()
    }

    fn issue_at(
        &self,
        app_id: Uuid,
        webhook_id: Uuid,
        isn: Isn,
        now: DateTime<Utc>,
    ) -> Result<String, WebhookSecurityError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("JWT".to_owned());
        header.kid = Some(self.key_id.clone());
        let claims = PayloadClaims {
            iss: self.issuer.clone(),
            sub: app_id.to_string(),
            exp: (now + TimeDelta::seconds(PAYLOAD_TOKEN_LIFETIME_SECONDS))
                .timestamp()
                .try_into()
                .map_err(|_| {
                    WebhookSecurityError::Signing(
                        jsonwebtoken::errors::ErrorKind::InvalidToken.into(),
                    )
                })?,
            app_id,
            webhook_id,
            isn: isn.to_string(),
        };
        encode(&header, &claims, &self.encoding_key).map_err(WebhookSecurityError::Signing)
    }

    fn authorize_payload(&self, request: &WebhookPayloadRequest) -> Result<(), ApiError> {
        if !request.bearer_token.starts_with("eyJ") {
            return Err(permission_denied("Invalid webhook payload token"));
        }
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "iss", "sub"]);
        let header = jsonwebtoken::decode_header(&request.bearer_token)
            .map_err(|_| validation_error("Invalid JWT"))?;
        let key_id = header
            .kid
            .as_deref()
            .ok_or_else(|| validation_error("Invalid JWT"))?;
        let decoding_key = self
            .decoding_keys
            .get(key_id)
            .ok_or_else(|| validation_error("Invalid JWT"))?;
        let token = decode::<PayloadClaims>(&request.bearer_token, decoding_key, &validation)
            .map_err(|_| validation_error("Invalid JWT"))?;
        let expected_isn = request
            .isn
            .parse::<Isn>()
            .map_err(|_| validation_error("Malformed Instant sequence number"))?;
        let claims = token.claims;
        if claims.sub != request.app_id.to_string()
            || claims.app_id != request.app_id
            || claims.webhook_id != request.webhook_id
            || claims.isn != expected_isn.to_string()
        {
            return Err(validation_error("Webhook payload JWT claims do not match"));
        }
        Ok(())
    }

    fn delivery_signature(
        &self,
        timestamp: i64,
        body: &[u8],
    ) -> Result<String, WebhookSecurityError> {
        let mut input = timestamp.to_string().into_bytes();
        input.push(b'.');
        input.extend_from_slice(body);
        let encoded = jsonwebtoken::crypto::sign(&input, &self.encoding_key, Algorithm::EdDSA)
            .map_err(WebhookSecurityError::Signing)?;
        let signature = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| WebhookSecurityError::InvalidSignatureEncoding)?;
        let mut hex = String::with_capacity(signature.len() * 2);
        for byte in signature {
            write!(hex, "{byte:02x}").expect("writing hexadecimal to a string cannot fail");
        }
        Ok(format!("t={timestamp},kid={},v1={hex}", self.key_id))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TinkKeyset {
    primary_key_id: u32,
    #[serde(rename = "key")]
    keys: Vec<TinkKey>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TinkKey {
    key_data: TinkKeyData,
    status: String,
    key_id: u32,
    output_prefix_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TinkKeyData {
    type_url: String,
    value: String,
    key_material_type: String,
}

fn validate_tink_key_metadata(key: &TinkKey) -> Result<(), WebhookSecurityError> {
    let reason =
        if key.key_data.type_url != "type.googleapis.com/google.crypto.tink.Ed25519PrivateKey" {
            Some("key type is not Ed25519PrivateKey")
        } else if key.key_data.key_material_type != "ASYMMETRIC_PRIVATE" {
            Some("key material is not asymmetric private material")
        } else if key.output_prefix_type != "RAW" {
            Some("output prefix type is not RAW")
        } else {
            None
        };
    reason.map_or(Ok(()), |reason| {
        Err(WebhookSecurityError::InvalidTinkKey {
            key_id: key.key_id,
            reason,
        })
    })
}

fn public_jwk(
    key_id: &str,
    decoding_key: &DecodingKey,
) -> Result<Ed25519PublicJwk, WebhookSecurityError> {
    let jwk = Jwk::from_decoding_key(decoding_key, Some(Algorithm::EdDSA))
        .map_err(WebhookSecurityError::InvalidKey)?;
    let AlgorithmParameters::OctetKeyPair(parameters) = jwk.algorithm else {
        return Err(WebhookSecurityError::InvalidKey(
            jsonwebtoken::errors::ErrorKind::InvalidEddsaKey.into(),
        ));
    };
    Ok(Ed25519PublicJwk {
        kid: key_id.to_owned(),
        x: parameters.x,
    })
}

fn validate_key_pair(
    key_id: u32,
    encoding_key: &EncodingKey,
    decoding_key: &DecodingKey,
) -> Result<(), WebhookSecurityError> {
    let private_jwk = Jwk::from_encoding_key(encoding_key, Algorithm::EdDSA)
        .map_err(WebhookSecurityError::InvalidKey)?;
    let public_jwk = Jwk::from_decoding_key(decoding_key, Some(Algorithm::EdDSA))
        .map_err(WebhookSecurityError::InvalidKey)?;
    if private_jwk.algorithm != public_jwk.algorithm {
        return Err(WebhookSecurityError::InvalidTinkKey {
            key_id,
            reason: "private and public key bytes do not match",
        });
    }
    Ok(())
}

enum ProtoValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn parse_protobuf(input: &[u8]) -> Option<Vec<(u32, ProtoValue<'_>)>> {
    let mut offset = 0;
    let mut fields = Vec::new();
    while offset < input.len() {
        let tag = read_varint(input, &mut offset)?;
        let field = u32::try_from(tag >> 3).ok()?;
        if field == 0 {
            return None;
        }
        match tag & 7 {
            0 => fields.push((field, ProtoValue::Varint(read_varint(input, &mut offset)?))),
            2 => {
                let length = usize::try_from(read_varint(input, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let value = input.get(offset..end)?;
                offset = end;
                fields.push((field, ProtoValue::Bytes(value)));
            }
            _ => return None,
        }
    }
    Some(fields)
}

fn read_varint(input: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..=63).step_by(7) {
        let byte = *input.get(*offset)?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn decode_tink_ed25519_private(input: &[u8]) -> Option<([u8; 32], [u8; 32])> {
    let fields = parse_protobuf(input)?;
    let mut version = 0;
    let mut private_seed = None;
    let mut public_key = None;
    for (field, value) in fields {
        match (field, value) {
            (1, ProtoValue::Varint(value)) => version = value,
            (2, ProtoValue::Bytes(value)) if private_seed.is_none() => {
                private_seed = value.try_into().ok();
            }
            (3, ProtoValue::Bytes(value)) if public_key.is_none() => {
                public_key = decode_tink_ed25519_public(value);
            }
            _ => {}
        }
    }
    (version == 0).then_some((private_seed?, public_key?))
}

fn decode_tink_ed25519_public(input: &[u8]) -> Option<[u8; 32]> {
    let fields = parse_protobuf(input)?;
    let mut version = 0;
    let mut public_key = None;
    for (field, value) in fields {
        match (field, value) {
            (1, ProtoValue::Varint(value)) => version = value,
            (2, ProtoValue::Bytes(value)) if public_key.is_none() => {
                public_key = value.try_into().ok();
            }
            _ => {}
        }
    }
    (version == 0).then_some(public_key?)
}

fn ed25519_private_der(seed: &[u8; 32]) -> [u8; 48] {
    let mut der = [0; 48];
    der[..16].copy_from_slice(&[
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ]);
    der[16..].copy_from_slice(seed);
    der
}

impl WebhookPayloadTokenIssuer for Ed25519WebhookKeys {
    type Error = WebhookSecurityError;

    fn issue(&self, app_id: Uuid, webhook_id: Uuid, isn: Isn) -> Result<String, Self::Error> {
        self.issue_at(app_id, webhook_id, isn, Utc::now())
    }
}

/// Payload replay authorizer for the scoped JWT embedded in webhook deliveries.
#[derive(Clone)]
pub struct JwtWebhookPayloadAuthorizer {
    keys: Arc<Ed25519WebhookKeys>,
}

impl JwtWebhookPayloadAuthorizer {
    #[must_use]
    pub const fn new(keys: Arc<Ed25519WebhookKeys>) -> Self {
        Self { keys }
    }
}

impl WebhookPayloadAuthorizer for JwtWebhookPayloadAuthorizer {
    fn authorize(
        &self,
        request: &WebhookPayloadRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        std::future::ready(self.keys.authorize_payload(request))
    }
}

/// Existing app-access authorization used for non-JWT replay credentials.
pub trait WebhookBearerAuthorizer: Send + Sync {
    fn authorize(
        &self,
        app_id: Uuid,
        bearer_token: &str,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

/// JVM-compatible replay authorization: scoped JWT or normal app read access.
#[derive(Clone)]
pub struct JwtOrBearerWebhookPayloadAuthorizer<A> {
    keys: Arc<Ed25519WebhookKeys>,
    bearer_authorizer: Arc<A>,
}

impl<A> JwtOrBearerWebhookPayloadAuthorizer<A> {
    #[must_use]
    pub const fn new(keys: Arc<Ed25519WebhookKeys>, bearer_authorizer: Arc<A>) -> Self {
        Self {
            keys,
            bearer_authorizer,
        }
    }
}

impl<A> WebhookPayloadAuthorizer for JwtOrBearerWebhookPayloadAuthorizer<A>
where
    A: WebhookBearerAuthorizer,
{
    async fn authorize(&self, request: &WebhookPayloadRequest) -> Result<(), ApiError> {
        if request.bearer_token.starts_with("eyJ") {
            self.keys.authorize_payload(request)
        } else {
            self.bearer_authorizer
                .authorize(request.app_id, &request.bearer_token)
                .await
        }
    }
}

pub type WebhookIpWhitelistFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HashSet<IpAddr>, String>> + Send + 'a>>;

/// Supplies the live operator-approved addresses that may bypass webhook SSRF
/// filtering. Implementations must return an error rather than an empty set
/// when their backing policy cannot be read so delivery fails closed.
pub trait WebhookIpWhitelist: Send + Sync + 'static {
    fn addresses(&self) -> WebhookIpWhitelistFuture<'_>;
}

#[derive(Debug, Default)]
struct EmptyWebhookIpWhitelist;

impl WebhookIpWhitelist for EmptyWebhookIpWhitelist {
    fn addresses(&self) -> WebhookIpWhitelistFuture<'_> {
        Box::pin(std::future::ready(Ok(HashSet::new())))
    }
}

impl WebhookIpWhitelist for crate::PostgresFailoverGate {
    fn addresses(&self) -> WebhookIpWhitelistFuture<'_> {
        Box::pin(async move {
            self.flag_value("smokescreen-whitelist-ips")
                .await
                .map(|value| parse_ip_whitelist(value.as_ref()))
                .map_err(|error| error.to_string())
        })
    }
}

/// Redirect-free, address-pinned HTTP delivery with Instant's Ed25519 headers.
#[derive(Clone)]
pub struct GuardedWebhookHttpTransport {
    keys: Arc<Ed25519WebhookKeys>,
    ip_whitelist: Arc<dyn WebhookIpWhitelist>,
}

impl GuardedWebhookHttpTransport {
    #[must_use]
    pub fn new(keys: Arc<Ed25519WebhookKeys>) -> Self {
        Self {
            keys,
            ip_whitelist: Arc::new(EmptyWebhookIpWhitelist),
        }
    }

    #[must_use]
    pub fn with_ip_whitelist(mut self, ip_whitelist: Arc<dyn WebhookIpWhitelist>) -> Self {
        self.ip_whitelist = ip_whitelist;
        self
    }

    /// Resolves and pins a target using the same SSRF policy as delivery,
    /// without sending an HTTP request.
    pub async fn validate_target(&self, url: &str) -> bool {
        let Ok(whitelist) = self.ip_whitelist.addresses().await else {
            return false;
        };
        let Ok(endpoint) = parse_webhook_url(url, &whitelist) else {
            return false;
        };
        pinned_client(&endpoint, &whitelist).await.is_ok()
    }

    async fn deliver_inner(
        &self,
        request: WebhookDeliveryRequest,
    ) -> Result<WebhookAttempt, WebhookSecurityError> {
        let started_at = Utc::now();
        let timer = Instant::now();
        let timestamp = started_at.timestamp();
        let signature = self.keys.delivery_signature(timestamp, &request.body)?;
        let whitelist = match self.ip_whitelist.addresses().await {
            Ok(whitelist) => whitelist,
            Err(error) => return Err(WebhookSecurityError::TargetPolicy(error)),
        };
        let endpoint = match parse_webhook_url(&request.url, &whitelist) {
            Ok(endpoint) => endpoint,
            Err(error) => return Ok(failed_attempt(started_at, timer, classify_oauth(&error))),
        };
        let client = match pinned_client(&endpoint, &whitelist).await {
            Ok(client) => client,
            Err(PinnedClientError::Guard(error)) => {
                return Ok(failed_attempt(started_at, timer, classify_oauth(&error)));
            }
            Err(PinnedClientError::Http(error)) => {
                return Ok(failed_attempt(started_at, timer, classify_reqwest(&error)));
            }
        };
        let response = match client
            .post(endpoint)
            .header("User-Agent", "InstantDB Webhook Sender")
            .header("Instant-Signature", signature)
            .header("Idempotency-Key", request.idempotency_key.to_string())
            .header("Content-Type", "application/json; charset=utf-8")
            .body(request.body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return Ok(failed_attempt(started_at, timer, classify_reqwest(&error))),
        };
        let status = i32::from(response.status().as_u16());
        let success = response.status().is_success();
        let mut preview = Vec::new();
        let mut chunks = response.bytes_stream();
        while preview.len() < MAX_RESPONSE_PREVIEW_BYTES {
            let Some(chunk) = chunks.next().await else {
                break;
            };
            match chunk {
                Ok(chunk) => {
                    let remaining = MAX_RESPONSE_PREVIEW_BYTES - preview.len();
                    preview.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                Err(error) => {
                    return Ok(failed_attempt(started_at, timer, classify_reqwest(&error)));
                }
            }
        }
        Ok(WebhookAttempt {
            attempted_at: started_at,
            duration_millis: elapsed_millis(timer),
            success,
            status_code: Some(status),
            response_text: Some(String::from_utf8_lossy(&preview).into_owned()),
            error_type: None,
            error_message: None,
        })
    }
}

impl WebhookTransport for GuardedWebhookHttpTransport {
    type Error = WebhookSecurityError;

    fn deliver(
        &self,
        request: WebhookDeliveryRequest,
    ) -> impl Future<Output = Result<WebhookAttempt, Self::Error>> + Send {
        self.deliver_inner(request)
    }
}

#[derive(Debug, Error)]
enum PinnedClientError {
    #[error(transparent)]
    Guard(#[from] OAuthError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

fn parse_webhook_url(value: &str, whitelist: &HashSet<IpAddr>) -> Result<url::Url, OAuthError> {
    match parse_safe_url(value) {
        Ok(endpoint) => Ok(endpoint),
        Err(OAuthError::UnsafeUrl("private or special IP address")) => {
            let endpoint = url::Url::parse(value)?;
            let address = match endpoint.host() {
                Some(Host::Ipv4(address)) => IpAddr::V4(address),
                Some(Host::Ipv6(address)) => IpAddr::V6(address),
                _ => return Err(OAuthError::UnsafeUrl("private or special IP address")),
            };
            whitelist
                .contains(&address)
                .then_some(endpoint)
                .ok_or(OAuthError::UnsafeUrl("private or special IP address"))
        }
        Err(error) => Err(error),
    }
}

async fn pinned_client(
    endpoint: &url::Url,
    whitelist: &HashSet<IpAddr>,
) -> Result<reqwest::Client, PinnedClientError> {
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
            .await
            .map_err(OAuthError::Dns)?
            .map(|address| address.ip())
            .collect::<Vec<_>>();
        let addresses = validate_resolved_target_with_whitelist(resolved, whitelist)?
            .into_iter()
            .map(|address| SocketAddr::new(address, port))
            .collect::<Vec<_>>();
        builder = builder.resolve_to_addrs(domain, &addresses);
    }
    Ok(builder.build()?)
}

fn validate_resolved_target_with_whitelist(
    addresses: impl IntoIterator<Item = IpAddr>,
    whitelist: &HashSet<IpAddr>,
) -> Result<Vec<IpAddr>, OAuthError> {
    let addresses = addresses.into_iter().collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(OAuthError::UnsafeUrl("host resolved to no addresses"));
    }
    if addresses.iter().any(|address| {
        !whitelist.contains(address) && validate_resolved_target([*address]).is_err()
    }) {
        return Err(OAuthError::UnsafeUrl(
            "host resolved to a private or special IP address",
        ));
    }
    Ok(addresses)
}

fn parse_ip_whitelist(value: Option<&serde_json::Value>) -> HashSet<IpAddr> {
    value
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter_map(|value| value.parse().ok())
        .collect()
}

fn failed_attempt(
    attempted_at: DateTime<Utc>,
    timer: Instant,
    (error_type, error_message): (&'static str, &'static str),
) -> WebhookAttempt {
    WebhookAttempt {
        attempted_at,
        duration_millis: elapsed_millis(timer),
        success: false,
        status_code: None,
        response_text: None,
        error_type: Some(error_type.to_owned()),
        error_message: Some(error_message.to_owned()),
    }
}

fn elapsed_millis(timer: Instant) -> i32 {
    i32::try_from(timer.elapsed().as_millis()).unwrap_or(i32::MAX)
}

fn classify_oauth(error: &OAuthError) -> (&'static str, &'static str) {
    match error {
        OAuthError::Dns(_) | OAuthError::UnsafeUrl(_) | OAuthError::InvalidUrl(_) => {
            ("dns", "Could not resolve hostname.")
        }
        _ => ("network", "Network error."),
    }
}

fn classify_reqwest(error: &reqwest::Error) -> (&'static str, &'static str) {
    if error.is_timeout() {
        ("timeout", "Request timed out.")
    } else if error.is_connect() {
        ("connect", "Could not connect to server.")
    } else if error.is_builder() {
        ("protocol", "Invalid HTTP response.")
    } else {
        ("network", "Network error.")
    }
}

fn validation_error(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn permission_denied(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::PermissionDenied, message).unauthorized()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use jsonwebtoken::{DecodingKey, decode_header};

    const PRIVATE_KEY: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIGrD/e7uKYqSY4twDEsRfMMuLSrODf14dpTiTK6K1YI0\n-----END PRIVATE KEY-----\n";
    const PUBLIC_KEY: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA2+Jj2UvNCvQiUPNYRgSi0cJSPiJI6Rs6D0UTeEpQVj8=\n-----END PUBLIC KEY-----\n";
    const TINK_KEYSET: &str = r#"{"primaryKeyId":1783962448,"key":[{"keyData":{"typeUrl":"type.googleapis.com/google.crypto.tink.Ed25519PrivateKey","value":"EiB14wGTmxdOWZjBKmM4NjfVvbLFk8WgAkt0nPQZv0qfZRoiEiDXMNu0AcfM4rk4lIJgrwFvn8Zeifb0I+AVJCvNIB1vYQ==","keyMaterialType":"ASYMMETRIC_PRIVATE"},"status":"ENABLED","keyId":1783962448,"outputPrefixType":"RAW"}]}"#;

    fn keys() -> Ed25519WebhookKeys {
        Ed25519WebhookKeys::from_pem("https://instant.test", "1234", PRIVATE_KEY, PUBLIC_KEY)
            .unwrap()
    }

    #[derive(Debug)]
    struct FixedWhitelist(Result<HashSet<IpAddr>, &'static str>);

    impl WebhookIpWhitelist for FixedWhitelist {
        fn addresses(&self) -> WebhookIpWhitelistFuture<'_> {
            let result = self.0.clone().map_err(ToOwned::to_owned);
            Box::pin(std::future::ready(result))
        }
    }

    #[test]
    fn payload_jwt_binds_every_route_claim_and_exposes_matching_jwk() {
        let keys = keys();
        let app_id = Uuid::from_u128(1);
        let webhook_id = Uuid::from_u128(2);
        let isn = "0/328/48953748".parse().unwrap();
        let token = keys.issue(app_id, webhook_id, isn).unwrap();
        assert_eq!(decode_header(&token).unwrap().kid.as_deref(), Some("1234"));
        assert_eq!(
            keys.public_jwk().x,
            "2-Jj2UvNCvQiUPNYRgSi0cJSPiJI6Rs6D0UTeEpQVj8"
        );
        keys.authorize_payload(&WebhookPayloadRequest {
            app_id,
            webhook_id,
            isn: isn.to_string(),
            bearer_token: token.clone(),
        })
        .unwrap();
        assert!(
            keys.authorize_payload(&WebhookPayloadRequest {
                app_id,
                webhook_id: Uuid::from_u128(3),
                isn: isn.to_string(),
                bearer_token: token,
            })
            .is_err()
        );
    }

    #[test]
    fn imports_authoritative_jvm_tink_keyset_and_preserves_rotation_ids() {
        let keys = Ed25519WebhookKeys::from_tink_json("https://instant.test", TINK_KEYSET).unwrap();
        assert_eq!(keys.public_jwk().kid, "1783962448");
        assert_eq!(
            keys.public_jwk().x,
            "1zDbtAHHzOK5OJSCYK8Bb5_GXon29CPgFSQrzSAdb2E"
        );

        let mut rotated = serde_json::from_str::<serde_json::Value>(TINK_KEYSET).unwrap();
        let mut replacement = rotated["key"][0].clone();
        replacement["keyId"] = serde_json::json!(7);
        rotated["primaryKeyId"] = serde_json::json!(7);
        rotated["key"].as_array_mut().unwrap().push(replacement);
        let keys = Ed25519WebhookKeys::from_tink_json(
            "https://instant.test",
            &serde_json::to_string(&rotated).unwrap(),
        )
        .unwrap();
        assert_eq!(keys.public_jwks().len(), 2);
        let token = keys
            .issue(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                "0/328/48953748".parse().unwrap(),
            )
            .unwrap();
        assert_eq!(decode_header(&token).unwrap().kid.as_deref(), Some("7"));
    }

    #[test]
    fn tink_import_rejects_prefixed_or_mismatched_material() {
        let mut prefixed = serde_json::from_str::<serde_json::Value>(TINK_KEYSET).unwrap();
        prefixed["key"][0]["outputPrefixType"] = serde_json::json!("TINK");
        assert!(
            Ed25519WebhookKeys::from_tink_json(
                "https://instant.test",
                &serde_json::to_string(&prefixed).unwrap(),
            )
            .is_err()
        );

        let mut mismatched = serde_json::from_str::<serde_json::Value>(TINK_KEYSET).unwrap();
        let encoded = mismatched["key"][0]["keyData"]["value"].as_str().unwrap();
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        mismatched["key"][0]["keyData"]["value"] =
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        assert!(
            Ed25519WebhookKeys::from_tink_json(
                "https://instant.test",
                &serde_json::to_string(&mismatched).unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn delivery_signature_is_hex_ed25519_over_timestamp_dot_body() {
        let keys = keys();
        let body = br#"{"payload":true}"#;
        let header = keys.delivery_signature(1_700_000_000, body).unwrap();
        let signature = header.rsplit("v1=").next().unwrap();
        assert_eq!(signature.len(), 128);
        assert!(signature.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let signature_bytes = (0..signature.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&signature[offset..offset + 2], 16).unwrap())
            .collect::<Vec<_>>();
        let mut input = b"1700000000.".to_vec();
        input.extend_from_slice(body);
        assert!(
            jsonwebtoken::crypto::verify(
                &URL_SAFE_NO_PAD.encode(signature_bytes),
                &input,
                &DecodingKey::from_ed_pem(PUBLIC_KEY).unwrap(),
                Algorithm::EdDSA,
            )
            .unwrap()
        );
        assert_eq!(
            header,
            keys.delivery_signature(1_700_000_000, body).unwrap()
        );
        assert_ne!(
            header,
            keys.delivery_signature(1_700_000_001, body).unwrap()
        );
    }

    struct BearerAccess {
        calls: AtomicUsize,
    }

    impl WebhookBearerAuthorizer for BearerAccess {
        fn authorize(
            &self,
            _app_id: Uuid,
            bearer_token: &str,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(if bearer_token == "per_valid" {
                Ok(())
            } else {
                Err(permission_denied("denied"))
            })
        }
    }

    #[tokio::test]
    async fn composite_authorizer_delegates_only_non_jwt_credentials() {
        let keys = Arc::new(keys());
        let access = Arc::new(BearerAccess {
            calls: AtomicUsize::new(0),
        });
        let authorizer = JwtOrBearerWebhookPayloadAuthorizer::new(keys, access.clone());
        authorizer
            .authorize(&WebhookPayloadRequest {
                app_id: Uuid::from_u128(1),
                webhook_id: Uuid::from_u128(2),
                isn: "0/328/48953748".to_owned(),
                bearer_token: "per_valid".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(access.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn transport_rejects_private_targets_without_connecting() {
        let transport = GuardedWebhookHttpTransport::new(Arc::new(keys()));
        let attempt = transport
            .deliver(WebhookDeliveryRequest {
                url: "http://127.0.0.1/webhook".to_owned(),
                idempotency_key: Uuid::nil(),
                queue_latency: Duration::ZERO,
                body: b"{}".to_vec(),
            })
            .await
            .unwrap();
        assert!(!attempt.success);
        assert_eq!(attempt.error_type.as_deref(), Some("dns"));
        assert_eq!(attempt.status_code, None);
    }

    #[tokio::test]
    async fn live_whitelist_allows_only_the_operator_approved_private_literal() {
        let whitelist = HashSet::from(["127.0.0.1".parse().unwrap()]);
        let transport = GuardedWebhookHttpTransport::new(Arc::new(keys()))
            .with_ip_whitelist(Arc::new(FixedWhitelist(Ok(whitelist))));

        assert!(transport.validate_target("http://127.0.0.1/webhook").await);
        assert!(!transport.validate_target("http://10.0.0.1/webhook").await);
        assert!(!transport.validate_target("http://[::1]/webhook").await);
    }

    #[tokio::test]
    async fn whitelist_policy_errors_fail_closed() {
        let transport = GuardedWebhookHttpTransport::new(Arc::new(keys()))
            .with_ip_whitelist(Arc::new(FixedWhitelist(Err("config app unavailable"))));
        assert!(
            !transport
                .validate_target("https://example.com/webhook")
                .await
        );

        let error = transport
            .deliver(WebhookDeliveryRequest {
                url: "https://example.com/webhook".to_owned(),
                idempotency_key: Uuid::nil(),
                queue_latency: Duration::ZERO,
                body: b"{}".to_vec(),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, WebhookSecurityError::TargetPolicy(message) if message == "config app unavailable")
        );
    }

    #[test]
    fn whitelist_parser_ignores_malformed_values_and_dns_validation_stays_all_or_nothing() {
        let whitelist = parse_ip_whitelist(Some(&serde_json::json!([
            "127.0.0.1",
            "10.0.0.1",
            "not-an-ip",
            42
        ])));
        assert_eq!(whitelist.len(), 2);
        assert!(whitelist.contains(&"127.0.0.1".parse().unwrap()));
        assert!(
            validate_resolved_target_with_whitelist(
                [
                    "127.0.0.1".parse().unwrap(),
                    "93.184.216.34".parse().unwrap()
                ],
                &whitelist
            )
            .is_ok()
        );
        assert!(
            validate_resolved_target_with_whitelist(
                ["127.0.0.1".parse().unwrap(), "192.168.0.1".parse().unwrap()],
                &whitelist
            )
            .is_err()
        );
    }
}
