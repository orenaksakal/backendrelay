//! Re-encrypts stored secrets after an AEAD keyset rotation.

use instant_instaql::system_attr_id;
use thiserror::Error;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::oauth_credentials::hex_decode;
use crate::{OAuthCredentialError, OAuthSecretCipher};

const REENCRYPT_BATCH_SIZE: usize = 500;
const OAUTH_CLIENT_SECRET_ETYPE: &str = "$oauthClients";
const OAUTH_CLIENT_SECRET_LABEL: &str = "encryptedClientSecret";

/// Counts for one table whose stored ciphertexts were inspected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretReencryptionCount {
    pub table: &'static str,
    pub scanned: u64,
    pub reencrypted: u64,
}

/// Outcome of a full re-encryption walk. `dry_run` reports the same counts
/// without writing any rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretReencryptionReport {
    pub dry_run: bool,
    pub tables: Vec<SecretReencryptionCount>,
}

impl SecretReencryptionReport {
    #[must_use]
    pub fn reencrypted(&self) -> u64 {
        self.tables.iter().map(|table| table.reencrypted).sum()
    }
}

#[derive(Debug, Error)]
pub enum SecretReencryptionError {
    #[error("could not re-encrypt {table}: {source}")]
    Database {
        table: &'static str,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("could not re-encrypt {table} row {row_id}: {source}")]
    Cipher {
        table: &'static str,
        row_id: Uuid,
        #[source]
        source: OAuthCredentialError,
    },
    #[error("stored secret in {table} row {row_id} is not a string")]
    InvalidValue { table: &'static str, row_id: Uuid },
    #[error("the OAuth client secret system attribute is unavailable")]
    SystemCatalog(#[source] instant_instaql::SystemCatalogError),
}

/// Walks every ciphertext encrypted with the AEAD keyset and re-encrypts rows
/// that were not written with the current primary key. The walk is idempotent
/// and commits one transaction per batch.
///
/// # Errors
///
/// Returns an error for database failures or stored values that the loaded
/// keyset cannot decrypt.
pub async fn reencrypt_stored_secrets(
    client: &mut Client,
    cipher: &OAuthSecretCipher,
    dry_run: bool,
) -> Result<SecretReencryptionReport, SecretReencryptionError> {
    let byop = reencrypt_byop_connection_strings(client, cipher, dry_run).await?;
    let oauth = reencrypt_oauth_client_secrets(client, cipher, dry_run).await?;
    Ok(SecretReencryptionReport {
        dry_run,
        tables: vec![byop, oauth],
    })
}

async fn reencrypt_byop_connection_strings(
    client: &mut Client,
    cipher: &OAuthSecretCipher,
    dry_run: bool,
) -> Result<SecretReencryptionCount, SecretReencryptionError> {
    const TABLE: &str = "apps.connection_string";
    let batch_size = i64::try_from(REENCRYPT_BATCH_SIZE).unwrap_or(i64::MAX);
    let mut count = SecretReencryptionCount {
        table: TABLE,
        scanned: 0,
        reencrypted: 0,
    };
    let mut after: Option<Uuid> = None;
    loop {
        let transaction =
            client
                .transaction()
                .await
                .map_err(|source| SecretReencryptionError::Database {
                    table: TABLE,
                    source,
                })?;
        let rows = transaction
            .query(
                "SELECT id, connection_string FROM apps
                 WHERE connection_string IS NOT NULL
                   AND ($1::uuid IS NULL OR id > $1)
                 ORDER BY id
                 LIMIT $2",
                &[&after, &batch_size],
            )
            .await
            .map_err(|source| SecretReencryptionError::Database {
                table: TABLE,
                source,
            })?;
        if rows.is_empty() {
            transaction
                .commit()
                .await
                .map_err(|source| SecretReencryptionError::Database {
                    table: TABLE,
                    source,
                })?;
            break;
        }
        for row in &rows {
            let app_id: Uuid = row.get(0);
            let encrypted: Vec<u8> = row.get(1);
            count.scanned += 1;
            if !cipher.needs_reencryption(&encrypted) {
                continue;
            }
            let plaintext = cipher.decrypt_bytes(app_id, &encrypted).map_err(|source| {
                SecretReencryptionError::Cipher {
                    table: TABLE,
                    row_id: app_id,
                    source,
                }
            })?;
            let reencrypted = cipher.encrypt_bytes(app_id, &plaintext).map_err(|source| {
                SecretReencryptionError::Cipher {
                    table: TABLE,
                    row_id: app_id,
                    source,
                }
            })?;
            count.reencrypted += 1;
            if !dry_run {
                transaction
                    .execute(
                        "UPDATE apps SET connection_string = $2 WHERE id = $1",
                        &[&app_id, &reencrypted],
                    )
                    .await
                    .map_err(|source| SecretReencryptionError::Database {
                        table: TABLE,
                        source,
                    })?;
            }
        }
        let last = rows.last().map(|row| row.get::<_, Uuid>(0));
        transaction
            .commit()
            .await
            .map_err(|source| SecretReencryptionError::Database {
                table: TABLE,
                source,
            })?;
        after = last;
        if rows.len() < REENCRYPT_BATCH_SIZE {
            break;
        }
    }
    Ok(count)
}

async fn reencrypt_oauth_client_secrets(
    client: &mut Client,
    cipher: &OAuthSecretCipher,
    dry_run: bool,
) -> Result<SecretReencryptionCount, SecretReencryptionError> {
    const TABLE: &str = "triples.$oauthClients.encryptedClientSecret";
    let batch_size = i64::try_from(REENCRYPT_BATCH_SIZE).unwrap_or(i64::MAX);
    let attr_id = system_attr_id(OAUTH_CLIENT_SECRET_ETYPE, OAUTH_CLIENT_SECRET_LABEL)
        .map_err(SecretReencryptionError::SystemCatalog)?;
    let mut count = SecretReencryptionCount {
        table: TABLE,
        scanned: 0,
        reencrypted: 0,
    };
    let mut after: Option<(Uuid, Uuid)> = None;
    loop {
        let transaction =
            client
                .transaction()
                .await
                .map_err(|source| SecretReencryptionError::Database {
                    table: TABLE,
                    source,
                })?;
        let rows = transaction
            .query(
                "SELECT app_id, entity_id, value FROM triples
                 WHERE attr_id = $1
                   AND ($2::uuid IS NULL OR (app_id, entity_id) > ($2, $3))
                 ORDER BY app_id, entity_id
                 LIMIT $4",
                &[
                    &attr_id,
                    &after.map(|after| after.0),
                    &after.map(|after| after.1),
                    &batch_size,
                ],
            )
            .await
            .map_err(|source| SecretReencryptionError::Database {
                table: TABLE,
                source,
            })?;
        if rows.is_empty() {
            transaction
                .commit()
                .await
                .map_err(|source| SecretReencryptionError::Database {
                    table: TABLE,
                    source,
                })?;
            break;
        }
        for row in &rows {
            let app_id: Uuid = row.get(0);
            let entity_id: Uuid = row.get(1);
            let value: serde_json::Value = row.get(2);
            count.scanned += 1;
            let Some(reencrypted) = reencrypted_oauth_secret(cipher, entity_id, &value)? else {
                continue;
            };
            count.reencrypted += 1;
            if !dry_run {
                transaction
                    .execute(
                        "UPDATE triples
                         SET value = to_jsonb($4::text),
                             value_md5 = md5(to_jsonb($4::text)::text)
                         WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3
                           AND value = $5",
                        &[&app_id, &entity_id, &attr_id, &reencrypted, &value],
                    )
                    .await
                    .map_err(|source| SecretReencryptionError::Database {
                        table: TABLE,
                        source,
                    })?;
            }
        }
        let last = rows
            .last()
            .map(|row| (row.get::<_, Uuid>(0), row.get::<_, Uuid>(1)));
        transaction
            .commit()
            .await
            .map_err(|source| SecretReencryptionError::Database {
                table: TABLE,
                source,
            })?;
        after = last;
        if rows.len() < REENCRYPT_BATCH_SIZE {
            break;
        }
    }
    Ok(count)
}

fn reencrypted_oauth_secret(
    cipher: &OAuthSecretCipher,
    entity_id: Uuid,
    value: &serde_json::Value,
) -> Result<Option<String>, SecretReencryptionError> {
    const TABLE: &str = "triples.$oauthClients.encryptedClientSecret";
    let Some(secret_hex) = value.as_str() else {
        return Err(SecretReencryptionError::InvalidValue {
            table: TABLE,
            row_id: entity_id,
        });
    };
    let envelope = hex_decode(secret_hex).map_err(|source| SecretReencryptionError::Cipher {
        table: TABLE,
        row_id: entity_id,
        source,
    })?;
    if !cipher.needs_reencryption(&envelope) {
        return Ok(None);
    }
    let plaintext = cipher.decrypt(entity_id, secret_hex).map_err(|source| {
        SecretReencryptionError::Cipher {
            table: TABLE,
            row_id: entity_id,
            source,
        }
    })?;
    cipher
        .encrypt(entity_id, &plaintext)
        .map(Some)
        .map_err(|source| SecretReencryptionError::Cipher {
            table: TABLE,
            row_id: entity_id,
            source,
        })
}
