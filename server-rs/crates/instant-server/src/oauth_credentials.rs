use std::collections::HashMap;
use std::future::Future;

use base64::Engine as _;
use instant_auth::oauth_store::{OAuthClientRecord, OAuthProvider};
use instant_http::{ApiError, ApiErrorType};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::db_error::internal_error;
use crate::{
    DashboardControlHookOperation, DashboardControlHooks, OAuthCredentialProvider,
    ResolvedOAuthCredentials, SharedOAuthClientConfig,
};

const TINK_AES_GCM_TYPE: &str = "type.googleapis.com/google.crypto.tink.AesGcmKey";
const TINK_PREFIX_SIZE: usize = 5;
const NONCE_SIZE: usize = 12;

#[derive(Debug, Error)]
pub enum OAuthCredentialError {
    #[error("invalid Tink AEAD keyset JSON: {0}")]
    InvalidTinkJson(#[source] serde_json::Error),
    #[error("Tink AEAD keyset has no enabled TINK AES-GCM primary key")]
    MissingTinkPrimary,
    #[error("Tink AEAD key {key_id} is invalid")]
    InvalidTinkKey { key_id: u32 },
    #[error("OAuth secret encryption failed")]
    Encryption,
    #[error("OAuth secret ciphertext is invalid")]
    InvalidCiphertext,
}

#[derive(Clone)]
struct AesKey {
    id: u32,
    bytes: Vec<u8>,
}

/// JVM-compatible Tink AES-GCM envelope for app OAuth client secrets.
#[derive(Clone)]
pub struct OAuthSecretCipher {
    primary_id: u32,
    keys: Vec<AesKey>,
}

impl OAuthSecretCipher {
    /// Imports a cleartext Tink JSON keyset. KMS decryption stays at the process
    /// secret-manager boundary, matching the existing JVM startup behavior.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, unsupported key metadata, invalid
    /// AES material, duplicate key IDs, or a missing enabled primary key.
    pub fn from_tink_json(keyset_json: &str) -> Result<Self, OAuthCredentialError> {
        let keyset: TinkKeyset =
            serde_json::from_str(keyset_json).map_err(OAuthCredentialError::InvalidTinkJson)?;
        let mut keys = Vec::new();
        for key in keyset.keys {
            if key.status != "ENABLED" {
                continue;
            }
            if key.key_data.type_url != TINK_AES_GCM_TYPE
                || key.key_data.key_material_type != "SYMMETRIC"
                || key.output_prefix_type != "TINK"
            {
                return Err(OAuthCredentialError::InvalidTinkKey { key_id: key.key_id });
            }
            let encoded = base64::engine::general_purpose::STANDARD
                .decode(key.key_data.value)
                .map_err(|_| OAuthCredentialError::InvalidTinkKey { key_id: key.key_id })?;
            let bytes = decode_aes_gcm_key(&encoded)
                .ok_or(OAuthCredentialError::InvalidTinkKey { key_id: key.key_id })?;
            algorithm(&bytes).ok_or(OAuthCredentialError::InvalidTinkKey { key_id: key.key_id })?;
            if keys
                .iter()
                .any(|existing: &AesKey| existing.id == key.key_id)
            {
                return Err(OAuthCredentialError::InvalidTinkKey { key_id: key.key_id });
            }
            keys.push(AesKey {
                id: key.key_id,
                bytes,
            });
        }
        if !keys.iter().any(|key| key.id == keyset.primary_key_id) {
            return Err(OAuthCredentialError::MissingTinkPrimary);
        }
        Ok(Self {
            primary_id: keyset.primary_key_id,
            keys,
        })
    }

    /// Encrypts a secret using the client UUID bytes as associated data.
    ///
    /// # Errors
    ///
    /// Returns an error when the primary key or secure randomness is unavailable.
    pub fn encrypt(
        &self,
        client_id: Uuid,
        plaintext: &str,
    ) -> Result<String, OAuthCredentialError> {
        self.encrypt_bytes(client_id, plaintext)
            .map(|envelope| hex_encode(&envelope))
    }

    /// Encrypts a secret into the raw Tink-prefixed representation used by
    /// PostgreSQL `bytea` columns such as an app's BYOP connection string.
    ///
    /// # Errors
    ///
    /// Returns an error when the primary key or secure randomness is unavailable.
    pub fn encrypt_bytes(
        &self,
        client_id: Uuid,
        plaintext: &str,
    ) -> Result<Vec<u8>, OAuthCredentialError> {
        let key = self
            .keys
            .iter()
            .find(|key| key.id == self.primary_id)
            .ok_or(OAuthCredentialError::MissingTinkPrimary)?;
        let cipher = less_safe_key(key)?;
        let mut nonce_bytes = [0_u8; NONCE_SIZE];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| OAuthCredentialError::Encryption)?;
        let mut ciphertext = plaintext.as_bytes().to_vec();
        cipher
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(client_id.as_bytes().as_slice()),
                &mut ciphertext,
            )
            .map_err(|_| OAuthCredentialError::Encryption)?;
        let mut envelope = Vec::with_capacity(TINK_PREFIX_SIZE + NONCE_SIZE + ciphertext.len());
        envelope.push(1);
        envelope.extend_from_slice(&key.id.to_be_bytes());
        envelope.extend_from_slice(&nonce_bytes);
        envelope.extend_from_slice(&ciphertext);
        Ok(envelope)
    }

    /// Returns the key id the ciphertext was encrypted with, if the Tink prefix
    /// is present.
    #[must_use]
    pub fn ciphertext_key_id(ciphertext: &[u8]) -> Option<u32> {
        let prefix = ciphertext.get(..TINK_PREFIX_SIZE)?;
        if prefix[0] != 1 {
            return None;
        }
        Some(u32::from_be_bytes(prefix[1..].try_into().ok()?))
    }

    /// Returns the primary key id, which new ciphertexts are encrypted with.
    #[must_use]
    pub const fn primary_key_id(&self) -> u32 {
        self.primary_id
    }

    /// Returns true when the ciphertext was not produced by the primary key, so
    /// a rotation should re-encrypt it.
    #[must_use]
    pub fn needs_reencryption(&self, ciphertext: &[u8]) -> bool {
        Self::ciphertext_key_id(ciphertext) != Some(self.primary_id)
    }

    /// Decrypts a Tink-prefixed hexadecimal secret for the given client UUID.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, unknown-key, unauthenticated, or non-UTF-8 ciphertext.
    pub fn decrypt(
        &self,
        client_id: Uuid,
        ciphertext_hex: &str,
    ) -> Result<String, OAuthCredentialError> {
        let envelope = hex_decode(ciphertext_hex)?;
        self.decrypt_bytes(client_id, &envelope)
    }

    /// Decrypts a raw Tink-prefixed ciphertext, as stored in PostgreSQL `bytea`
    /// columns such as an app's BYOP connection string.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, unknown-key, unauthenticated, or non-UTF-8 ciphertext.
    pub fn decrypt_bytes(
        &self,
        client_id: Uuid,
        ciphertext: &[u8],
    ) -> Result<String, OAuthCredentialError> {
        let mut envelope = ciphertext.to_vec();
        if envelope.len() < TINK_PREFIX_SIZE + NONCE_SIZE + aead::MAX_TAG_LEN || envelope[0] != 1 {
            return Err(OAuthCredentialError::InvalidCiphertext);
        }
        let key_id = u32::from_be_bytes(
            envelope[1..TINK_PREFIX_SIZE]
                .try_into()
                .map_err(|_| OAuthCredentialError::InvalidCiphertext)?,
        );
        let key = self
            .keys
            .iter()
            .find(|key| key.id == key_id)
            .ok_or(OAuthCredentialError::InvalidCiphertext)?;
        let nonce_bytes: [u8; NONCE_SIZE] = envelope
            [TINK_PREFIX_SIZE..TINK_PREFIX_SIZE + NONCE_SIZE]
            .try_into()
            .map_err(|_| OAuthCredentialError::InvalidCiphertext)?;
        let cipher = less_safe_key(key)?;
        let plaintext = cipher
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(client_id.as_bytes().as_slice()),
                &mut envelope[TINK_PREFIX_SIZE + NONCE_SIZE..],
            )
            .map_err(|_| OAuthCredentialError::InvalidCiphertext)?;
        String::from_utf8(plaintext.to_vec()).map_err(|_| OAuthCredentialError::InvalidCiphertext)
    }
}

fn less_safe_key(key: &AesKey) -> Result<LessSafeKey, OAuthCredentialError> {
    let algorithm =
        algorithm(&key.bytes).ok_or(OAuthCredentialError::InvalidTinkKey { key_id: key.id })?;
    UnboundKey::new(algorithm, &key.bytes)
        .map(LessSafeKey::new)
        .map_err(|_| OAuthCredentialError::InvalidTinkKey { key_id: key.id })
}

fn algorithm(bytes: &[u8]) -> Option<&'static aead::Algorithm> {
    match bytes.len() {
        16 => Some(&aead::AES_128_GCM),
        32 => Some(&aead::AES_256_GCM),
        _ => None,
    }
}

/// Owns dashboard OAuth-secret encryption and delegates all other operations.
pub struct TinkOAuthSettingsHooks<D> {
    cipher: OAuthSecretCipher,
    inner: D,
}

impl<D> TinkOAuthSettingsHooks<D> {
    #[must_use]
    pub const fn new(cipher: OAuthSecretCipher, inner: D) -> Self {
        Self { cipher, inner }
    }
}

impl<D> DashboardControlHooks for TinkOAuthSettingsHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::EncryptOAuthSecret {
                client_id,
                plaintext,
            } => self
                .cipher
                .encrypt(client_id, &plaintext)
                .map(Value::String)
                .map_err(|error| credential_error(&error)),
            other => self.inner.execute(other).await,
        }
    }
}

/// Resolves custom encrypted credentials or environment-provided shared ones.
pub struct ConfiguredOAuthCredentialProvider {
    cipher: OAuthSecretCipher,
    shared: HashMap<String, (String, String)>,
}

impl ConfiguredOAuthCredentialProvider {
    #[must_use]
    pub fn new(cipher: OAuthSecretCipher, shared: &[SharedOAuthClientConfig]) -> Self {
        Self {
            cipher,
            shared: shared
                .iter()
                .map(|client| {
                    (
                        client.provider_name().to_owned(),
                        (
                            client.client_id().to_owned(),
                            client.client_secret().to_owned(),
                        ),
                    )
                })
                .collect(),
        }
    }
}

impl OAuthCredentialProvider for ConfiguredOAuthCredentialProvider {
    fn resolve(
        &self,
        _app_id: Uuid,
        client: &OAuthClientRecord,
        provider: &OAuthProvider,
    ) -> impl Future<Output = Result<ResolvedOAuthCredentials, ApiError>> + Send {
        let result = if client.use_shared_credentials {
            self.shared
                .get(&provider.name)
                .map(|(client_id, client_secret)| ResolvedOAuthCredentials {
                    client_id: client_id.clone(),
                    client_secret: Some(client_secret.clone()),
                })
                .ok_or_else(|| {
                    ApiError::new(
                        ApiErrorType::RecordNotFound,
                        "Record not found: shared-oauth-client",
                    )
                })
        } else {
            client
                .client_id
                .clone()
                .ok_or_else(|| ApiError::new(ApiErrorType::OauthError, "Missing OAuth client_id."))
                .and_then(|client_id| {
                    let client_secret = client
                        .encrypted_client_secret
                        .as_deref()
                        .map(|secret| self.cipher.decrypt(client.id, secret))
                        .transpose()
                        .map_err(|error| credential_error(&error))?;
                    Ok(ResolvedOAuthCredentials {
                        client_id,
                        client_secret,
                    })
                })
        };
        std::future::ready(result)
    }
}

fn credential_error(error: &OAuthCredentialError) -> ApiError {
    internal_error(error)
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

fn decode_aes_gcm_key(input: &[u8]) -> Option<Vec<u8>> {
    let mut offset = 0;
    let mut version = 0;
    let mut key = None;
    while offset < input.len() {
        let tag = read_varint(input, &mut offset)?;
        match (tag >> 3, tag & 7) {
            (1, 0) => version = read_varint(input, &mut offset)?,
            (2 | 3, 2) => {
                let length = usize::try_from(read_varint(input, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                key = Some(input.get(offset..end)?.to_vec());
                offset = end;
            }
            (_, 0) => {
                read_varint(input, &mut offset)?;
            }
            (_, 2) => {
                let length = usize::try_from(read_varint(input, &mut offset)?).ok()?;
                offset = offset.checked_add(length)?;
                input.get(..offset)?;
            }
            _ => return None,
        }
    }
    (version == 0).then_some(key?)
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

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing hexadecimal to a string cannot fail");
    }
    output
}

pub(crate) fn hex_decode(value: &str) -> Result<Vec<u8>, OAuthCredentialError> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(OAuthCredentialError::InvalidCiphertext);
    }
    pairs
        .iter()
        .map(|pair| {
            let text =
                std::str::from_utf8(pair).map_err(|_| OAuthCredentialError::InvalidCiphertext)?;
            u8::from_str_radix(text, 16).map_err(|_| OAuthCredentialError::InvalidCiphertext)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopDashboardControlHooks;
    use serde_json::json;

    fn cipher_with_key_field(field_tag: u8) -> OAuthSecretCipher {
        let mut proto = vec![0x08, 0x00, field_tag, 0x20];
        proto.extend(0_u8..32);
        let keyset = json!({
            "primaryKeyId": 42,
            "key": [{
                "keyData": {
                    "typeUrl": TINK_AES_GCM_TYPE,
                    "value": base64::engine::general_purpose::STANDARD.encode(proto),
                    "keyMaterialType": "SYMMETRIC"
                },
                "status": "ENABLED",
                "keyId": 42,
                "outputPrefixType": "TINK"
            }]
        });
        OAuthSecretCipher::from_tink_json(&keyset.to_string()).unwrap()
    }

    fn cipher() -> OAuthSecretCipher {
        cipher_with_key_field(0x1a)
    }

    #[test]
    fn imports_legacy_rust_field_two_aes_key_protobufs() {
        let cipher = cipher_with_key_field(0x12);
        let client_id = Uuid::new_v4();
        let encrypted = cipher.encrypt(client_id, "legacy").unwrap();
        assert_eq!(cipher.decrypt(client_id, &encrypted).unwrap(), "legacy");
    }

    #[test]
    fn tink_envelope_round_trips_and_binds_client_id_as_aad() {
        let cipher = cipher();
        let client_id = Uuid::new_v4();
        let encrypted = cipher.encrypt(client_id, "super-secret").unwrap();
        let encrypted_bytes = cipher.encrypt_bytes(client_id, "raw-secret").unwrap();
        assert!(encrypted.starts_with("010000002a"));
        assert_eq!(
            cipher.decrypt(client_id, &encrypted).unwrap(),
            "super-secret"
        );
        assert_eq!(
            cipher
                .decrypt_bytes(client_id, &hex_decode(&encrypted).unwrap())
                .unwrap(),
            "super-secret"
        );
        assert!(cipher.decrypt(Uuid::new_v4(), &encrypted).is_err());
        assert_eq!(
            cipher.decrypt_bytes(client_id, &encrypted_bytes).unwrap(),
            "raw-secret"
        );
    }

    #[tokio::test]
    async fn dashboard_hook_encrypts_secrets() {
        let client_id = Uuid::new_v4();
        let cipher = cipher();
        let hooks = TinkOAuthSettingsHooks::new(cipher.clone(), NoopDashboardControlHooks);
        let encrypted = hooks
            .execute(DashboardControlHookOperation::EncryptOAuthSecret {
                client_id,
                plaintext: "secret".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            cipher
                .decrypt(client_id, encrypted.as_str().unwrap())
                .unwrap(),
            "secret"
        );
    }

    #[tokio::test]
    async fn credential_provider_resolves_shared_and_decrypts_custom_clients() {
        let cipher = cipher();
        let config = crate::ServerConfig::from_pairs([(
            "INSTANT_SHARED_OAUTH_CLIENTS",
            r#"[{"provider_name":"google","client_id":"shared-id","client_secret":"shared-secret"}]"#,
        )])
        .unwrap();
        let provider =
            ConfiguredOAuthCredentialProvider::new(cipher.clone(), config.shared_oauth_clients());
        let provider_record = OAuthProvider {
            id: Uuid::new_v4(),
            name: "google".to_owned(),
        };
        let client_id = Uuid::new_v4();
        let mut client = OAuthClientRecord {
            id: client_id,
            provider_id: provider_record.id,
            name: "web".to_owned(),
            client_id: Some("custom-id".to_owned()),
            encrypted_client_secret: Some(cipher.encrypt(client_id, "custom-secret").unwrap()),
            extra_audiences: Vec::new(),
            discovery_endpoint: None,
            meta: None,
            redirect_to: None,
            use_shared_credentials: false,
        };
        assert_eq!(
            provider
                .resolve(Uuid::new_v4(), &client, &provider_record)
                .await
                .unwrap(),
            ResolvedOAuthCredentials {
                client_id: "custom-id".to_owned(),
                client_secret: Some("custom-secret".to_owned()),
            }
        );
        client.use_shared_credentials = true;
        assert_eq!(
            provider
                .resolve(Uuid::new_v4(), &client, &provider_record)
                .await
                .unwrap(),
            ResolvedOAuthCredentials {
                client_id: "shared-id".to_owned(),
                client_secret: Some("shared-secret".to_owned()),
            }
        );
    }
}
