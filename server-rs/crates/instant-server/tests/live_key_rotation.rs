use std::collections::HashMap;

use instant_server::{
    OAuthSecretCipher, ensure_system_catalog, generate_self_hosted_secrets_env,
    reencrypt_stored_secrets, rotate_self_hosted_secrets_env,
};
use serde_json::Value;
use tokio_postgres::NoTls;
use uuid::Uuid;

const SYSTEM_CATALOG_USER: &str = "e1111111-1111-1111-1111-111111111ca7";

fn env_values(env: &str) -> HashMap<String, String> {
    env.lines()
        .map(|line| {
            let (name, value) = line.split_once('=').unwrap();
            (name.to_owned(), value.trim_matches('\'').to_owned())
        })
        .collect()
}

fn hex_key_id(hex: &str) -> u32 {
    assert_eq!(&hex[..2], "01", "missing Tink output prefix");
    u32::from_str_radix(&hex[2..10], 16).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps one end-to-end rotation flow and its assertions together.
async fn rotation_reencrypts_old_rows_and_new_writes_use_the_new_primary() {
    let Some(database_url) =
        instant_test_support::test_database_url("rotation_reencrypts_old_rows")
    else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let original_env = generate_self_hosted_secrets_env().unwrap();
    let rotated_env = rotate_self_hosted_secrets_env(&original_env).unwrap();
    let original = env_values(&original_env);
    let rotated = env_values(&rotated_env);
    let old_cipher =
        OAuthSecretCipher::from_tink_json(&original["INSTANT_AEAD_KEYSET_JSON"]).unwrap();
    let new_cipher =
        OAuthSecretCipher::from_tink_json(&rotated["INSTANT_AEAD_KEYSET_JSON"]).unwrap();
    assert_ne!(old_cipher.primary_key_id(), new_cipher.primary_key_id());

    let app_id = Uuid::new_v4();
    let client_record_id = Uuid::new_v4();
    let byop_url = "postgres://byop.example/tenant";
    let byop_ciphertext = old_cipher.encrypt_bytes(app_id, byop_url).unwrap();
    let oauth_secret = old_cipher
        .encrypt(client_record_id, "oauth-client-secret")
        .unwrap();

    // The rotated keyset still decrypts ciphertext written by the old primary.
    assert_eq!(
        new_cipher.decrypt_bytes(app_id, &byop_ciphertext).unwrap(),
        byop_url
    );
    assert_eq!(
        new_cipher.decrypt(client_record_id, &oauth_secret).unwrap(),
        "oauth-client-secret"
    );
    assert_eq!(
        OAuthSecretCipher::ciphertext_key_id(&byop_ciphertext),
        Some(old_cipher.primary_key_id())
    );
    let fresh = new_cipher.encrypt(client_record_id, "fresh").unwrap();
    assert_eq!(hex_key_id(&fresh), new_cipher.primary_key_id());

    let system_user = Uuid::parse_str(SYSTEM_CATALOG_USER).unwrap();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title, connection_string)
             VALUES ($1, $2, 'key rotation fixture', $3)",
            &[&app_id, &system_user, &byop_ciphertext],
        )
        .await
        .unwrap();
    ensure_system_catalog(&mut client).await.unwrap();
    let attr_id =
        instant_instaql::system_attr_id("$oauthClients", "encryptedClientSecret").unwrap();
    client
        .execute(
            "INSERT INTO triples
               (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae)
             VALUES ($1, $2, $3, to_jsonb($4::text), md5(to_jsonb($4::text)::text),
                     true, false, false, false, false)",
            &[&app_id, &client_record_id, &attr_id, &oauth_secret],
        )
        .await
        .unwrap();

    let dry_run = reencrypt_stored_secrets(&mut client, &new_cipher, true)
        .await
        .unwrap();
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.reencrypted(), 2);
    let unchanged: Vec<u8> = client
        .query_one(
            "SELECT connection_string FROM apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        OAuthSecretCipher::ciphertext_key_id(&unchanged),
        Some(old_cipher.primary_key_id())
    );

    let report = reencrypt_stored_secrets(&mut client, &new_cipher, false)
        .await
        .unwrap();
    assert!(!report.dry_run);
    assert_eq!(report.reencrypted(), 2);

    let byop_after: Vec<u8> = client
        .query_one(
            "SELECT connection_string FROM apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        OAuthSecretCipher::ciphertext_key_id(&byop_after),
        Some(new_cipher.primary_key_id())
    );
    assert_eq!(
        new_cipher.decrypt_bytes(app_id, &byop_after).unwrap(),
        byop_url
    );

    let oauth_after: Value = client
        .query_one(
            "SELECT value FROM triples
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &client_record_id, &attr_id],
        )
        .await
        .unwrap()
        .get(0);
    let oauth_hex = oauth_after.as_str().unwrap();
    assert_eq!(hex_key_id(oauth_hex), new_cipher.primary_key_id());
    assert_eq!(
        new_cipher.decrypt(client_record_id, oauth_hex).unwrap(),
        "oauth-client-secret"
    );

    let idempotent = reencrypt_stored_secrets(&mut client, &new_cipher, false)
        .await
        .unwrap();
    assert_eq!(idempotent.reencrypted(), 0);

    client
        .execute("DELETE FROM apps WHERE id = $1", &[&app_id])
        .await
        .unwrap();
}
