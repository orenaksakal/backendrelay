use base64::Engine as _;
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("secure random generation failed")]
pub struct SelfHostedSecretGenerationError;

#[derive(Debug, Error)]
pub enum SelfHostedSecretRotationError {
    #[error("{0} is missing from the secrets env file")]
    Missing(&'static str),
    #[error("{name} is not valid Tink keyset JSON")]
    InvalidJson {
        name: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0} does not contain a Tink `key` array")]
    InvalidKeyset(&'static str),
    #[error(transparent)]
    Random(#[from] SelfHostedSecretGenerationError),
}

/// Generates a shell-sourceable, stable secret file for a new self-hosted
/// deployment. Callers must persist the output and restrict its permissions.
///
/// # Errors
///
/// Returns an error if the operating system's secure random source fails.
pub fn generate_self_hosted_secrets_env() -> Result<String, SelfHostedSecretGenerationError> {
    let random = SystemRandom::new();
    let mut aes_key = [0_u8; 32];
    let mut webhook_seed = [0_u8; 32];
    let mut rate_limit_key = [0_u8; 32];
    let mut id_bytes = [0_u8; 4];
    for bytes in [
        aes_key.as_mut_slice(),
        webhook_seed.as_mut_slice(),
        rate_limit_key.as_mut_slice(),
        id_bytes.as_mut_slice(),
    ] {
        random
            .fill(bytes)
            .map_err(|_| SelfHostedSecretGenerationError)?;
    }
    let key_id = u32::from_be_bytes(id_bytes).max(1);
    let webhook = Ed25519KeyPair::from_seed_unchecked(&webhook_seed)
        .map_err(|_| SelfHostedSecretGenerationError)?;

    let mut aes_proto = Vec::with_capacity(34);
    aes_proto.extend_from_slice(&[0x1a, 0x20]);
    aes_proto.extend_from_slice(&aes_key);
    let mut public_proto = Vec::with_capacity(34);
    public_proto.extend_from_slice(&[0x12, 0x20]);
    public_proto.extend_from_slice(webhook.public_key().as_ref());
    let mut webhook_proto = Vec::with_capacity(68);
    webhook_proto.extend_from_slice(&[0x12, 0x20]);
    webhook_proto.extend_from_slice(&webhook_seed);
    webhook_proto.extend_from_slice(&[0x1a, 0x22]);
    webhook_proto.extend_from_slice(&public_proto);

    let base64 = base64::engine::general_purpose::STANDARD;
    let oauth = json!({
        "primaryKeyId": key_id,
        "key": [{
            "keyData": {
                "typeUrl": "type.googleapis.com/google.crypto.tink.AesGcmKey",
                "value": base64.encode(aes_proto),
                "keyMaterialType": "SYMMETRIC"
            },
            "status": "ENABLED",
            "keyId": key_id,
            "outputPrefixType": "TINK"
        }]
    });
    let webhook = json!({
        "primaryKeyId": key_id,
        "key": [{
            "keyData": {
                "typeUrl": "type.googleapis.com/google.crypto.tink.Ed25519PrivateKey",
                "value": base64.encode(webhook_proto),
                "keyMaterialType": "ASYMMETRIC_PRIVATE"
            },
            "status": "ENABLED",
            "keyId": key_id,
            "outputPrefixType": "RAW"
        }]
    });
    Ok(format!(
        "INSTANT_AEAD_KEYSET_JSON='{oauth}'\n\
         INSTANT_WEBHOOK_KEYSET_JSON='{webhook}'\n\
         INSTANT_RATE_LIMIT_HMAC_KEY={}\n",
        base64.encode(rate_limit_key)
    ))
}

/// Rotates the AEAD and webhook keysets in a generated secrets env file.
///
/// A fresh key becomes the primary key of each keyset while every existing key
/// stays enabled for decryption and webhook signature verification. All other
/// lines, including the rate-limit HMAC key, are returned unchanged. Callers
/// print the result and redirect it over the original file.
///
/// # Errors
///
/// Returns an error when either keyset is missing, malformed, or lacks a Tink
/// `key` array, or when secure randomness is unavailable.
pub fn rotate_self_hosted_secrets_env(
    input: &str,
) -> Result<String, SelfHostedSecretRotationError> {
    let mut aead = None;
    let mut webhook = None;
    for line in input.lines() {
        let Some((name, raw)) = line.split_once('=') else {
            continue;
        };
        match env_name(name) {
            "INSTANT_AEAD_KEYSET_JSON" => aead = Some(env_value(raw)),
            "INSTANT_WEBHOOK_KEYSET_JSON" => webhook = Some(env_value(raw)),
            _ => {}
        }
    }
    let aead = aead.ok_or(SelfHostedSecretRotationError::Missing(
        "INSTANT_AEAD_KEYSET_JSON",
    ))?;
    let webhook = webhook.ok_or(SelfHostedSecretRotationError::Missing(
        "INSTANT_WEBHOOK_KEYSET_JSON",
    ))?;
    let aead = rotate_aead_keyset(aead)?;
    let webhook = rotate_webhook_keyset(webhook)?;

    let mut output = String::with_capacity(input.len() + 512);
    for segment in input.split_inclusive('\n') {
        let (line, ending) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        match line.split_once('=') {
            Some((name, _)) if env_name(name) == "INSTANT_AEAD_KEYSET_JSON" => {
                output.push_str(name);
                output.push_str("='");
                output.push_str(&aead);
                output.push('\'');
            }
            Some((name, _)) if env_name(name) == "INSTANT_WEBHOOK_KEYSET_JSON" => {
                output.push_str(name);
                output.push_str("='");
                output.push_str(&webhook);
                output.push('\'');
            }
            _ => output.push_str(line),
        }
        output.push_str(ending);
    }
    Ok(output)
}

fn env_name(name: &str) -> &str {
    let name = name.trim();
    name.strip_prefix("export ")
        .map_or(name, |name| name.trim_start())
}

fn env_value(raw: &str) -> &str {
    let value = raw.trim();
    let value = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .unwrap_or(value);
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

fn rotate_aead_keyset(json: &str) -> Result<String, SelfHostedSecretRotationError> {
    const NAME: &str = "INSTANT_AEAD_KEYSET_JSON";
    let (keyset, existing_ids) = parse_keyset(json, NAME)?;
    let key_id = fresh_key_id(&existing_ids)?;
    let mut aes_key = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut aes_key)
        .map_err(|_| SelfHostedSecretGenerationError)?;
    let mut aes_proto = Vec::with_capacity(34);
    aes_proto.extend_from_slice(&[0x1a, 0x20]);
    aes_proto.extend_from_slice(&aes_key);
    let key = json!({
        "keyData": {
            "typeUrl": "type.googleapis.com/google.crypto.tink.AesGcmKey",
            "value": base64::engine::general_purpose::STANDARD.encode(aes_proto),
            "keyMaterialType": "SYMMETRIC"
        },
        "status": "ENABLED",
        "keyId": key_id,
        "outputPrefixType": "TINK"
    });
    append_primary_key(keyset, key, key_id, NAME)
}

fn rotate_webhook_keyset(json: &str) -> Result<String, SelfHostedSecretRotationError> {
    const NAME: &str = "INSTANT_WEBHOOK_KEYSET_JSON";
    let (keyset, existing_ids) = parse_keyset(json, NAME)?;
    let key_id = fresh_key_id(&existing_ids)?;
    let mut webhook_seed = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut webhook_seed)
        .map_err(|_| SelfHostedSecretGenerationError)?;
    let webhook = Ed25519KeyPair::from_seed_unchecked(&webhook_seed)
        .map_err(|_| SelfHostedSecretGenerationError)?;
    let mut public_proto = Vec::with_capacity(34);
    public_proto.extend_from_slice(&[0x12, 0x20]);
    public_proto.extend_from_slice(webhook.public_key().as_ref());
    let mut webhook_proto = Vec::with_capacity(68);
    webhook_proto.extend_from_slice(&[0x12, 0x20]);
    webhook_proto.extend_from_slice(&webhook_seed);
    webhook_proto.extend_from_slice(&[0x1a, 0x22]);
    webhook_proto.extend_from_slice(&public_proto);
    let key = json!({
        "keyData": {
            "typeUrl": "type.googleapis.com/google.crypto.tink.Ed25519PrivateKey",
            "value": base64::engine::general_purpose::STANDARD.encode(webhook_proto),
            "keyMaterialType": "ASYMMETRIC_PRIVATE"
        },
        "status": "ENABLED",
        "keyId": key_id,
        "outputPrefixType": "RAW"
    });
    append_primary_key(keyset, key, key_id, NAME)
}

fn parse_keyset(
    json: &str,
    name: &'static str,
) -> Result<(serde_json::Value, Vec<u32>), SelfHostedSecretRotationError> {
    let keyset: serde_json::Value = serde_json::from_str(json)
        .map_err(|source| SelfHostedSecretRotationError::InvalidJson { name, source })?;
    let keys = keyset
        .get("key")
        .and_then(serde_json::Value::as_array)
        .ok_or(SelfHostedSecretRotationError::InvalidKeyset(name))?;
    let existing_ids = keys
        .iter()
        .filter_map(|key| key.get("keyId"))
        .filter_map(serde_json::Value::as_u64)
        .filter_map(|key_id| u32::try_from(key_id).ok())
        .collect();
    Ok((keyset, existing_ids))
}

fn append_primary_key(
    mut keyset: serde_json::Value,
    key: serde_json::Value,
    key_id: u32,
    name: &'static str,
) -> Result<String, SelfHostedSecretRotationError> {
    let keys = keyset
        .get_mut("key")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or(SelfHostedSecretRotationError::InvalidKeyset(name))?;
    keys.push(key);
    keyset["primaryKeyId"] = json!(key_id);
    serde_json::to_string(&keyset)
        .map_err(|source| SelfHostedSecretRotationError::InvalidJson { name, source })
}

fn fresh_key_id(existing_ids: &[u32]) -> Result<u32, SelfHostedSecretGenerationError> {
    let random = SystemRandom::new();
    loop {
        let mut id_bytes = [0_u8; 4];
        random
            .fill(&mut id_bytes)
            .map_err(|_| SelfHostedSecretGenerationError)?;
        let key_id = u32::from_be_bytes(id_bytes).max(1);
        if !existing_ids.contains(&key_id) {
            return Ok(key_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ed25519WebhookKeys, OAuthSecretCipher};

    #[test]
    fn generated_env_contains_valid_distinct_keysets_without_shell_metacharacters() {
        let first = generate_self_hosted_secrets_env().unwrap();
        let second = generate_self_hosted_secrets_env().unwrap();
        assert_ne!(first, second);
        let values = first
            .lines()
            .map(|line| {
                let (name, value) = line.split_once('=').unwrap();
                (name, value.trim_matches('\''))
            })
            .collect::<std::collections::HashMap<_, _>>();
        OAuthSecretCipher::from_tink_json(values["INSTANT_AEAD_KEYSET_JSON"]).unwrap();
        Ed25519WebhookKeys::from_tink_json(
            "http://localhost:8888",
            values["INSTANT_WEBHOOK_KEYSET_JSON"],
        )
        .unwrap();
        assert!(values["INSTANT_RATE_LIMIT_HMAC_KEY"].len() >= 43);
        assert!(!first.contains(['`', '$', '\\']));
    }

    #[test]
    fn rotation_adds_primary_keys_and_leaves_other_lines_untouched() {
        let original = generate_self_hosted_secrets_env().unwrap();
        let rotated = rotate_self_hosted_secrets_env(&original).unwrap();

        assert_eq!(original.lines().count(), rotated.lines().count());
        for (before, after) in original.lines().zip(rotated.lines()) {
            if before.starts_with("INSTANT_AEAD_KEYSET_JSON=")
                || before.starts_with("INSTANT_WEBHOOK_KEYSET_JSON=")
            {
                continue;
            }
            assert_eq!(before, after);
        }
        assert!(rotated.ends_with('\n'));

        let values = |env: &str| {
            env.lines()
                .map(|line| {
                    let (name, value) = line.split_once('=').unwrap();
                    (name.to_owned(), value.trim_matches('\'').to_owned())
                })
                .collect::<std::collections::HashMap<_, _>>()
        };
        let original_values = values(&original);
        let rotated_values = values(&rotated);

        let original_aead: serde_json::Value =
            serde_json::from_str(&original_values["INSTANT_AEAD_KEYSET_JSON"]).unwrap();
        let rotated_aead: serde_json::Value =
            serde_json::from_str(&rotated_values["INSTANT_AEAD_KEYSET_JSON"]).unwrap();
        let original_webhook: serde_json::Value =
            serde_json::from_str(&original_values["INSTANT_WEBHOOK_KEYSET_JSON"]).unwrap();
        let rotated_webhook: serde_json::Value =
            serde_json::from_str(&rotated_values["INSTANT_WEBHOOK_KEYSET_JSON"]).unwrap();

        assert_eq!(original_aead["key"].as_array().unwrap().len(), 1);
        assert_eq!(rotated_aead["key"].as_array().unwrap().len(), 2);
        assert_eq!(original_webhook["key"].as_array().unwrap().len(), 1);
        assert_eq!(rotated_webhook["key"].as_array().unwrap().len(), 2);
        assert_ne!(original_aead["primaryKeyId"], rotated_aead["primaryKeyId"]);
        assert_ne!(
            original_webhook["primaryKeyId"],
            rotated_webhook["primaryKeyId"]
        );
        assert_eq!(
            rotated_aead["key"][0]["keyId"],
            original_aead["primaryKeyId"]
        );
        assert_eq!(
            rotated_webhook["key"][0]["keyId"],
            original_webhook["primaryKeyId"]
        );
        assert_eq!(rotated_aead["key"][0]["status"], "ENABLED");
        assert_eq!(rotated_webhook["key"][0]["status"], "ENABLED");
        assert_eq!(
            rotated_aead["primaryKeyId"],
            rotated_aead["key"][1]["keyId"]
        );
        assert_eq!(
            rotated_webhook["primaryKeyId"],
            rotated_webhook["key"][1]["keyId"]
        );

        let old_cipher =
            OAuthSecretCipher::from_tink_json(&original_values["INSTANT_AEAD_KEYSET_JSON"])
                .unwrap();
        let new_cipher =
            OAuthSecretCipher::from_tink_json(&rotated_values["INSTANT_AEAD_KEYSET_JSON"]).unwrap();
        let client_id = uuid::Uuid::new_v4();
        let stored = old_cipher.encrypt(client_id, "pre-rotation").unwrap();
        assert_eq!(
            new_cipher.decrypt(client_id, &stored).unwrap(),
            "pre-rotation"
        );
        let fresh = new_cipher.encrypt(client_id, "post-rotation").unwrap();
        assert_eq!(
            u32::from_str_radix(&fresh[2..10], 16).unwrap(),
            new_cipher.primary_key_id()
        );
        assert_eq!(
            OAuthSecretCipher::ciphertext_key_id(
                &crate::oauth_credentials::hex_decode(&fresh).unwrap()
            ),
            Some(new_cipher.primary_key_id())
        );
    }
}
