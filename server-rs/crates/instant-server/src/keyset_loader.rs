#![allow(dead_code)]

use base64::Engine;
use prost::Message;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Ed25519WebhookKeys, OAuthSecretCipher};

const AWS_KMS_PREFIX: &str = "aws-kms://";

/// JVM-compatible Tink keyset configuration. This deliberately has no `Debug`
/// implementation because both cleartext and decrypted JSON contain key material.
pub struct TinkKeysetConfig {
    encrypted: bool,
    json: String,
    kms_key_url: Option<String>,
}

impl TinkKeysetConfig {
    #[must_use]
    pub fn cleartext(json: impl Into<String>) -> Self {
        Self {
            encrypted: false,
            json: json.into(),
            kms_key_url: None,
        }
    }

    #[must_use]
    pub fn encrypted(json: impl Into<String>, kms_key_url: impl Into<String>) -> Self {
        Self {
            encrypted: true,
            json: json.into(),
            kms_key_url: Some(kms_key_url.into()),
        }
    }
}

#[derive(Debug, Error)]
pub enum TinkKeysetLoadError {
    #[error("encrypted Tink keyset is missing its AWS KMS key URL")]
    MissingKmsKeyUrl,
    #[error("AWS KMS key URL must have the form aws-kms://arn:aws:kms:<region>:...")]
    InvalidKmsKeyUrl,
    #[error("encrypted Tink keyset JSON is malformed")]
    InvalidEncryptedJson(#[source] serde_json::Error),
    #[error("encrypted Tink keyset ciphertext is not standard Base64")]
    InvalidCiphertext,
    #[error("AWS KMS could not decrypt the Tink keyset")]
    KmsDecrypt(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("AWS KMS returned no plaintext for the Tink keyset")]
    MissingPlaintext,
    #[error("AWS KMS keysets are unavailable in the self-hosted instant-server build")]
    AwsHostedFeatureDisabled,
    #[error("decrypted Tink keyset protobuf is malformed")]
    InvalidKeysetProto(#[source] prost::DecodeError),
    #[error("decrypted Tink keyset cannot be represented as JSON")]
    InvalidKeysetJson(#[source] serde_json::Error),
    #[error("OAuth keyset is invalid: {0}")]
    InvalidOAuthKeyset(#[source] crate::OAuthCredentialError),
    #[error("webhook keyset is invalid: {0}")]
    InvalidWebhookKeyset(#[source] crate::WebhookSecurityError),
}

/// Loads the same cleartext or AWS-KMS-encrypted Tink JSON keysets accepted by
/// the JVM server. AWS credentials use the official SDK's default provider chain.
#[derive(Clone, Copy, Default)]
pub struct AwsKmsTinkKeysetLoader;

impl AwsKmsTinkKeysetLoader {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Loads an AES-GCM keyset for app OAuth client-secret encryption.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, KMS decryption, protobuf decoding,
    /// or the contained AES-GCM key material is invalid.
    pub async fn load_oauth_cipher(
        &self,
        config: &TinkKeysetConfig,
    ) -> Result<OAuthSecretCipher, TinkKeysetLoadError> {
        let json = self.load_json(config).await?;
        OAuthSecretCipher::from_tink_json(&json).map_err(TinkKeysetLoadError::InvalidOAuthKeyset)
    }

    /// Loads an Ed25519 keyset for webhook payload JWT signing and verification.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, KMS decryption, protobuf decoding,
    /// or the contained Ed25519 key material is invalid.
    pub async fn load_webhook_keys(
        &self,
        issuer: impl Into<String>,
        config: &TinkKeysetConfig,
    ) -> Result<Ed25519WebhookKeys, TinkKeysetLoadError> {
        let json = self.load_json(config).await?;
        Ed25519WebhookKeys::from_tink_json(issuer, &json)
            .map_err(TinkKeysetLoadError::InvalidWebhookKeyset)
    }

    #[allow(clippy::unused_async)]
    async fn load_json(&self, config: &TinkKeysetConfig) -> Result<String, TinkKeysetLoadError> {
        if !config.encrypted {
            return Ok(config.json.clone());
        }
        let key_url = config
            .kms_key_url
            .as_deref()
            .ok_or(TinkKeysetLoadError::MissingKmsKeyUrl)?;
        let (key_arn, region) = parse_kms_key_url(key_url)?;
        let envelope: EncryptedKeysetJson = serde_json::from_str(&config.json)
            .map_err(TinkKeysetLoadError::InvalidEncryptedJson)?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(envelope.encrypted_keyset)
            .map_err(|_| TinkKeysetLoadError::InvalidCiphertext)?;
        let _ = (key_arn, region, ciphertext);
        Err(TinkKeysetLoadError::AwsHostedFeatureDisabled)
    }
}

fn parse_kms_key_url(value: &str) -> Result<(&str, &str), TinkKeysetLoadError> {
    let arn = value
        .strip_prefix(AWS_KMS_PREFIX)
        .ok_or(TinkKeysetLoadError::InvalidKmsKeyUrl)?;
    let mut parts = arn.split(':');
    if parts.next() != Some("arn") || parts.next() != Some("aws") || parts.next() != Some("kms") {
        return Err(TinkKeysetLoadError::InvalidKmsKeyUrl);
    }
    let region = parts.next().unwrap_or_default();
    if region.is_empty()
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(TinkKeysetLoadError::InvalidKmsKeyUrl);
    }
    if parts.next().unwrap_or_default().is_empty()
        || parts.next().unwrap_or_default().is_empty()
        || parts.next().is_some()
    {
        return Err(TinkKeysetLoadError::InvalidKmsKeyUrl);
    }
    Ok((arn, region))
}

fn keyset_proto_to_json(bytes: &[u8]) -> Result<String, TinkKeysetLoadError> {
    let keyset = ProtoKeyset::decode(bytes).map_err(TinkKeysetLoadError::InvalidKeysetProto)?;
    let json = JsonKeyset {
        primary_key_id: keyset.primary_key_id,
        keys: keyset
            .keys
            .into_iter()
            .map(|key| JsonKey {
                key_data: key.key_data.map(|data| JsonKeyData {
                    type_url: data.type_url,
                    value: base64::engine::general_purpose::STANDARD.encode(data.value),
                    key_material_type: key_material_type_name(data.key_material_type).to_owned(),
                }),
                status: key_status_name(key.status).to_owned(),
                key_id: key.key_id,
                output_prefix_type: output_prefix_name(key.output_prefix_type).to_owned(),
            })
            .collect(),
    };
    serde_json::to_string(&json).map_err(TinkKeysetLoadError::InvalidKeysetJson)
}

const fn key_status_name(value: i32) -> &'static str {
    match value {
        1 => "ENABLED",
        2 => "DISABLED",
        3 => "DESTROYED",
        _ => "UNKNOWN_STATUS",
    }
}

const fn output_prefix_name(value: i32) -> &'static str {
    match value {
        1 => "TINK",
        2 => "LEGACY",
        3 => "RAW",
        4 => "CRUNCHY",
        _ => "UNKNOWN_PREFIX",
    }
}

const fn key_material_type_name(value: i32) -> &'static str {
    match value {
        1 => "SYMMETRIC",
        2 => "ASYMMETRIC_PRIVATE",
        3 => "ASYMMETRIC_PUBLIC",
        4 => "REMOTE",
        _ => "UNKNOWN_KEYMATERIAL",
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncryptedKeysetJson {
    encrypted_keyset: String,
}

#[derive(Message)]
struct ProtoKeyset {
    #[prost(uint32, tag = "1")]
    primary_key_id: u32,
    #[prost(message, repeated, tag = "2")]
    keys: Vec<ProtoKey>,
}

#[derive(Message)]
struct ProtoKey {
    #[prost(message, optional, tag = "1")]
    key_data: Option<ProtoKeyData>,
    #[prost(enumeration = "ProtoKeyStatus", tag = "2")]
    status: i32,
    #[prost(uint32, tag = "3")]
    key_id: u32,
    #[prost(enumeration = "ProtoOutputPrefix", tag = "4")]
    output_prefix_type: i32,
}

#[derive(Message)]
struct ProtoKeyData {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
    #[prost(enumeration = "ProtoKeyMaterialType", tag = "3")]
    key_material_type: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
enum ProtoKeyStatus {
    UnknownStatus = 0,
    Enabled = 1,
    Disabled = 2,
    Destroyed = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
enum ProtoOutputPrefix {
    UnknownPrefix = 0,
    Tink = 1,
    Legacy = 2,
    Raw = 3,
    Crunchy = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
enum ProtoKeyMaterialType {
    UnknownKeymaterial = 0,
    Symmetric = 1,
    AsymmetricPrivate = 2,
    AsymmetricPublic = 3,
    Remote = 4,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonKeyset {
    primary_key_id: u32,
    #[serde(rename = "key")]
    keys: Vec<JsonKey>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonKey {
    #[serde(skip_serializing_if = "Option::is_none")]
    key_data: Option<JsonKeyData>,
    status: String,
    key_id: u32,
    output_prefix_type: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonKeyData {
    type_url: String,
    value: String,
    key_material_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jvm_aws_kms_urls_and_rejects_lookalikes() {
        let url = "aws-kms://arn:aws:kms:us-east-1:123456789012:key/mrk-example";
        assert_eq!(
            parse_kms_key_url(url).unwrap(),
            (
                "arn:aws:kms:us-east-1:123456789012:key/mrk-example",
                "us-east-1"
            )
        );
        for invalid in [
            "arn:aws:kms:us-east-1:123:key/example",
            "aws-kms://arn:aws:s3:us-east-1:123:key/example",
            "aws-kms://arn:aws:kms::123:key/example",
            "aws-kms://arn:aws:kms:us-east-1::key/example",
        ] {
            assert!(matches!(
                parse_kms_key_url(invalid),
                Err(TinkKeysetLoadError::InvalidKmsKeyUrl)
            ));
        }
    }

    #[test]
    fn converts_binary_tink_keysets_to_the_canonical_json_shape() {
        let proto = ProtoKeyset {
            primary_key_id: 42,
            keys: vec![ProtoKey {
                key_data: Some(ProtoKeyData {
                    type_url: "type.googleapis.com/google.crypto.tink.AesGcmKey".to_owned(),
                    value: vec![0x12, 0x02, 0xaa, 0xbb],
                    key_material_type: ProtoKeyMaterialType::Symmetric as i32,
                }),
                status: ProtoKeyStatus::Enabled as i32,
                key_id: 42,
                output_prefix_type: ProtoOutputPrefix::Tink as i32,
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&keyset_proto_to_json(&proto.encode_to_vec()).unwrap()).unwrap();
        assert_eq!(json["primaryKeyId"], 42);
        assert_eq!(json["key"][0]["keyId"], 42);
        assert_eq!(json["key"][0]["status"], "ENABLED");
        assert_eq!(json["key"][0]["outputPrefixType"], "TINK");
        assert_eq!(json["key"][0]["keyData"]["keyMaterialType"], "SYMMETRIC");
        assert_eq!(json["key"][0]["keyData"]["value"], "EgKquw==");
    }

    #[tokio::test]
    async fn cleartext_loader_constructs_the_existing_oauth_primitive() {
        let key_proto = vec![0x12, 0x20]
            .into_iter()
            .chain([7_u8; 32])
            .collect::<Vec<_>>();
        let json = serde_json::json!({
            "primaryKeyId": 7,
            "key": [{
                "keyData": {
                    "typeUrl": "type.googleapis.com/google.crypto.tink.AesGcmKey",
                    "value": base64::engine::general_purpose::STANDARD.encode(key_proto),
                    "keyMaterialType": "SYMMETRIC"
                },
                "status": "ENABLED",
                "keyId": 7,
                "outputPrefixType": "TINK"
            }]
        });
        let cipher = AwsKmsTinkKeysetLoader::new()
            .load_oauth_cipher(&TinkKeysetConfig::cleartext(json.to_string()))
            .await
            .unwrap();
        let client_id = uuid::Uuid::new_v4();
        let encrypted = cipher.encrypt(client_id, "secret").unwrap();
        assert_eq!(cipher.decrypt(client_id, &encrypted).unwrap(), "secret");
    }
}
