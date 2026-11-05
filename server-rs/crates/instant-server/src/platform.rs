use std::future::{Future, ready};
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use instant_http::{
    ApiError, ApiErrorType, PlatformBackend, WebhookPayloadRequest, WebhookPayloadResponse,
};
use instant_wal::Isn;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Authorization and history lookup boundary for webhook payload replay.
pub trait WebhookPayloadStore: Send + Sync + 'static {
    fn load(
        &self,
        request: WebhookPayloadRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

/// Public Ed25519 material exposed as an RFC 7517/8037 JWK.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ed25519PublicJwk {
    pub kid: String,
    pub x: String,
}

/// Production coordinator for platform webhooks and replay payloads.
pub struct PlatformService<W> {
    webhook_payloads: W,
    webhook_jwks: Value,
}

impl<W> PlatformService<W> {
    /// Builds a service and rejects invalid webhook verification material.
    ///
    /// # Errors
    ///
    /// Returns an error when key IDs or Ed25519 keys are invalid.
    pub fn new(webhook_payloads: W, webhook_keys: Vec<Ed25519PublicJwk>) -> Result<Self, String> {
        for key in &webhook_keys {
            if key.kid.is_empty() {
                return Err("Webhook JWK key ID must not be empty".to_owned());
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(&key.x)
                .map_err(|_| "Webhook JWK x must be unpadded base64url".to_owned())?;
            if bytes.len() != 32 {
                return Err("Webhook Ed25519 public keys must be 32 bytes".to_owned());
            }
        }
        let webhook_jwks = serde_json::json!({
            "keys": webhook_keys
                .into_iter()
                .map(|key| serde_json::json!({
                    "kty": "OKP",
                    "crv": "Ed25519",
                    "alg": "EdDSA",
                    "use": "sig",
                    "kid": key.kid,
                    "x": key.x,
                }))
                .collect::<Vec<_>>()
        });
        Ok(Self {
            webhook_payloads,
            webhook_jwks,
        })
    }
}

impl<W> PlatformBackend for PlatformService<W>
where
    W: WebhookPayloadStore,
{
    fn webhook_jwks(&self) -> impl Future<Output = Result<Value, ApiError>> + Send {
        ready(Ok(self.webhook_jwks.clone()))
    }

    async fn webhook_payload(
        &self,
        request: WebhookPayloadRequest,
    ) -> Result<WebhookPayloadResponse, ApiError> {
        let isn = Isn::from_str(&request.isn)
            .map_err(|_| validation("Malformed Instant sequence number"))?;
        let webhook_id = request.webhook_id;
        let data = self.webhook_payloads.load(request).await?;
        Ok(WebhookPayloadResponse {
            data,
            idempotency_key: payload_idempotency_key(webhook_id, isn),
        })
    }
}

fn validation(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

/// JVM-compatible SHA-256/UUID idempotency key for a webhook payload.
#[must_use]
pub fn payload_idempotency_key(webhook_id: Uuid, isn: Isn) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(webhook_id.as_bytes());
    digest.update(isn.to_bytes());
    let bytes: [u8; 32] = digest.finalize().into();
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&bytes[..16]);
    Uuid::from_bytes(uuid_bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Sink(Mutex<Vec<Value>>);

    impl WebhookPayloadStore for Sink {
        fn load(
            &self,
            request: WebhookPayloadRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.0.lock().unwrap().push(serde_json::json!({
                "app-id": request.app_id,
                "token": request.bearer_token,
            }));
            ready(Ok(serde_json::json!([{"action": "create"}])))
        }
    }

    #[test]
    fn payload_keys_match_reference_contracts() {
        let isn: Isn = "0/328/48953748".parse().unwrap();
        let webhook = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        assert_eq!(
            payload_idempotency_key(webhook, isn),
            Uuid::parse_str("c97d2a4d-8ad4-f52a-5b2a-d3e3040ca80d").unwrap()
        );
    }

    #[test]
    fn jwks_configuration_rejects_non_ed25519_material() {
        let result = PlatformService::new(
            Sink::default(),
            vec![Ed25519PublicJwk {
                kid: "1".to_owned(),
                x: URL_SAFE_NO_PAD.encode([0_u8; 31]),
            }],
        );
        assert!(result.is_err());
    }
}
