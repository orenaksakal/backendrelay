use std::collections::BTreeMap;

use instant_auth::oauth::OAuthError;
use instant_auth::oauth_origins::{authorized_origins, is_authorized_origin};
use instant_auth::oauth_store::{
    OAuthClientPatch, OAuthClientRecord, OAuthProvider, OAuthSignIn, OAuthUserLink,
    consume_oauth_code, consume_oauth_redirect, create_oauth_client, create_oauth_code,
    create_oauth_provider, create_oauth_redirect, create_oauth_user_link, delete_oauth_client,
    oauth_client_by_id, oauth_client_by_name, oauth_provider_by_id, oauth_provider_by_name,
    oauth_user_link, sign_in_with_oauth, sign_in_with_oauth_reusing_refresh, update_oauth_client,
    update_oauth_link_user,
};
use instant_auth::{
    AuthError, CreateUser, create_magic_code, create_user, delete_refresh_token,
    delete_user_by_email, get_user, get_users, link_guest_user, sign_in_guest, update_user_email,
    update_user_image_url, user_by_refresh_token, users_at_least, verify_magic_code,
};
use instant_instaql::{AttributeValueType, Cardinality, system_attr_id, system_attribute_catalog};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

async fn install_schema(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE attrs (
               id uuid NOT NULL,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL,
               is_required boolean,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               on_delete attr_on_delete,
               on_delete_reverse attr_on_delete,
               checked_data_type checked_data_type,
               setting_unique boolean,
               indexing boolean,
               checking_data_type boolean,
               inferred_types bit(32),
               metadata jsonb,
               deletion_marked_at timestamptz,
               PRIMARY KEY (app_id, id)
             );
             CREATE UNIQUE INDEX auth_attrs_id ON attrs(id);
             CREATE TEMP TABLE app_authorized_redirect_origins (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               service text NOT NULL,
               params text[] NOT NULL
             );
             CREATE TEMP TABLE triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL,
               value_md5 text NOT NULL,
               ea boolean NOT NULL,
               eav boolean NOT NULL,
               av boolean NOT NULL,
               ave boolean NOT NULL,
               vae boolean NOT NULL,
               checked_data_type checked_data_type,
               created_at bigint NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::bigint),
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE UNIQUE INDEX auth_ea ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX auth_av
               ON triples(app_id, attr_id, public.json_null_to_null(value)) WHERE av;
             CREATE OR REPLACE FUNCTION public.jsonb_deep_merge(obj1 jsonb, obj2 jsonb)
             RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $$
             DECLARE key text; item jsonb;
             BEGIN
               IF obj1 IS NULL OR obj2 IS NULL OR jsonb_typeof(obj1) != 'object' OR jsonb_typeof(obj2) != 'object'
               THEN RETURN obj2; END IF;
               FOR key, item IN SELECT * FROM jsonb_each(obj2) LOOP
                 obj1 := CASE
                   WHEN obj1 ? key AND jsonb_typeof(obj1->key) = 'object' AND jsonb_typeof(item) = 'object'
                     THEN jsonb_set(obj1, ARRAY[key], public.jsonb_deep_merge(obj1->key, item))
                   WHEN jsonb_typeof(item) = 'null' THEN obj1 - key
                   ELSE jsonb_set(obj1, ARRAY[key], item)
                 END;
               END LOOP;
               RETURN obj1;
             END $$;",
        )
        .await
        .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn magic_refresh_guest_and_expiry_flows_use_system_triples() {
    let Some(database_url) = instant_test_support::test_database_url(
        "magic_refresh_guest_and_expiry_flows_use_system_triples",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    let catalog = system_attribute_catalog().unwrap();
    for attribute in &catalog.attributes {
        let forward_ident = Uuid::new_v4();
        let reverse_ident = attribute.reverse_etype.as_ref().map(|_| Uuid::new_v4());
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        let checked_data_type = attribute
            .checked_data_type
            .map(|data_type| format!("{data_type:?}").to_lowercase());
        client
            .execute(
                "INSERT INTO attrs
                 (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
                  forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
                  checked_data_type, setting_unique, indexing, checking_data_type)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                         $14::text::checked_data_type, $15, $16, $17)",
                &[
                    &attribute.id,
                    &app_id,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &attribute.required,
                    &forward_ident,
                    &attribute.etype,
                    &attribute.label,
                    &reverse_ident,
                    &attribute.reverse_etype,
                    &attribute.reverse_label,
                    &checked_data_type,
                    &attribute.setting_unique,
                    &attribute.indexing,
                    &attribute.checking_data_type,
                ],
            )
            .await
            .unwrap();
    }
    client
        .execute(
            "INSERT INTO app_authorized_redirect_origins (id, app_id, service, params)
             VALUES ($1, $2, 'generic', ARRAY['app.example.com'])",
            &[&Uuid::new_v4(), &app_id],
        )
        .await
        .unwrap();
    let origins = authorized_origins(&client, app_id).await.unwrap();
    assert!(is_authorized_origin(
        &origins,
        "https://app.example.com/oauth/callback",
        false
    ));

    create_magic_code(
        &mut client,
        app_id,
        &catalog,
        "first@example.com",
        Some("123456"),
    )
    .await
    .unwrap();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let first = verify_magic_code(
        &mut client,
        app_id,
        &catalog,
        "first@example.com",
        "123456",
        None,
        BTreeMap::new(),
        None,
        now_ms,
        1_440,
        |_| async { Ok(()) },
    )
    .await
    .unwrap();
    assert!(first.created);
    assert_eq!(first.user.email.as_deref(), Some("first@example.com"));
    assert_eq!(first.user.user_type.as_deref(), Some("user"));
    assert_eq!(
        user_by_refresh_token(&client, app_id, &catalog, first.refresh_token)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.user.id
    );
    assert!(
        user_by_refresh_token(&client, Uuid::new_v4(), &catalog, first.refresh_token)
            .await
            .unwrap()
            .is_none(),
        "typed refresh-token lookups must remain app-scoped"
    );
    let lookup_transaction = client.transaction().await.unwrap();
    assert_eq!(
        user_by_refresh_token(&lookup_transaction, app_id, &catalog, first.refresh_token)
            .await
            .unwrap()
            .unwrap(),
        first.user,
        "typed lookups must preserve the complete projection inside transactions"
    );
    lookup_transaction.rollback().await.unwrap();
    let token_entity: Uuid = client
        .query_one(
            "SELECT entity_id FROM triples WHERE app_id=$1 AND attr_id=$2 AND value=$3",
            &[
                &app_id,
                &system_attr_id("$userRefreshTokens", "hashedToken").unwrap(),
                &json!(instant_auth::hash_refresh_token(first.refresh_token)),
            ],
        )
        .await
        .unwrap()
        .get(0);
    let reference_attr = system_attr_id("$userRefreshTokens", "$user").unwrap();
    for invalid in [json!("not-a-uuid"), json!(false), json!(null)] {
        client
            .execute(
                "UPDATE triples SET value=$1 WHERE app_id=$2 AND entity_id=$3 AND attr_id=$4",
                &[&invalid, &app_id, &token_entity, &reference_attr],
            )
            .await
            .unwrap();
        assert!(
            matches!(user_by_refresh_token(&client, app_id, &catalog, first.refresh_token).await,
            Err(AuthError::InvalidPersistedValue(value)) if value == invalid)
        );
    }
    for reference in [
        Uuid::new_v4().to_string(),
        first.user.id.simple().to_string(),
        first.user.id.to_string(),
    ] {
        client
            .execute(
                "UPDATE triples SET value=$1 WHERE app_id=$2 AND entity_id=$3 AND attr_id=$4",
                &[&json!(reference), &app_id, &token_entity, &reference_attr],
            )
            .await
            .unwrap();
        let loaded = user_by_refresh_token(&client, app_id, &catalog, first.refresh_token)
            .await
            .unwrap();
        if reference.parse::<Uuid>().unwrap() == first.user.id {
            assert_eq!(loaded.as_ref(), Some(&first.user));
        } else {
            assert!(
                loaded.is_none(),
                "a valid reference to a missing user remains absent"
            );
        }
    }
    assert!(matches!(
        verify_magic_code(
            &mut client,
            app_id,
            &catalog,
            "first@example.com",
            "123456",
            None,
            BTreeMap::new(),
            None,
            now_ms,
            1_440,
            |_| async { Ok(()) }
        )
        .await,
        Err(AuthError::InvalidMagicCode)
    ));

    let guest = sign_in_guest(&mut client, app_id, &catalog).await.unwrap();
    assert!(guest.user.is_guest());
    create_magic_code(
        &mut client,
        app_id,
        &catalog,
        "first@example.com",
        Some("222222"),
    )
    .await
    .unwrap();
    let linked = verify_magic_code(
        &mut client,
        app_id,
        &catalog,
        "first@example.com",
        "222222",
        Some(guest.user.id),
        BTreeMap::new(),
        None,
        now_ms,
        1_440,
        |_| async { Ok(()) },
    )
    .await
    .unwrap();
    assert!(!linked.created);
    assert_eq!(linked.user.id, first.user.id);
    let linked_value = client
        .query_one(
            "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[
                &app_id,
                &guest.user.id,
                &system_attr_id("$users", "linkedPrimaryUser").unwrap(),
            ],
        )
        .await
        .unwrap()
        .get::<_, tokio_postgres::types::Json<serde_json::Value>>(0)
        .0;
    assert_eq!(linked_value, json!(first.user.id));

    delete_refresh_token(&mut client, app_id, &catalog, first.refresh_token)
        .await
        .unwrap();
    assert!(
        user_by_refresh_token(&client, app_id, &catalog, first.refresh_token)
            .await
            .unwrap()
            .is_none()
    );

    let expired = create_magic_code(
        &mut client,
        app_id,
        &catalog,
        "expired@example.com",
        Some("333333"),
    )
    .await
    .unwrap();
    client
        .execute(
            "UPDATE triples SET created_at = $1 WHERE app_id = $2 AND entity_id = $3",
            &[&(now_ms - 61_000), &app_id, &expired.entity_id],
        )
        .await
        .unwrap();
    assert!(matches!(
        instant_auth::consume_magic_code(
            &mut client,
            app_id,
            &catalog,
            "expired@example.com",
            "333333",
            now_ms,
            1
        )
        .await,
        Err(AuthError::ExpiredMagicCode)
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &expired.entity_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let provider = OAuthProvider {
        id: Uuid::new_v4(),
        name: "test-provider".to_owned(),
    };
    create_oauth_provider(&mut client, app_id, &catalog, &provider)
        .await
        .unwrap();
    assert_eq!(
        oauth_provider_by_id(&client, app_id, provider.id)
            .await
            .unwrap(),
        Some(provider.clone())
    );
    assert_eq!(
        oauth_provider_by_name(&client, app_id, &provider.name)
            .await
            .unwrap(),
        Some(provider.clone())
    );
    let oauth_client = OAuthClientRecord {
        id: Uuid::new_v4(),
        provider_id: provider.id,
        name: "test-client".to_owned(),
        client_id: Some("external-client-id".to_owned()),
        encrypted_client_secret: Some("ciphertext".to_owned()),
        extra_audiences: vec!["com.example.ios".to_owned(), "android-client-id".to_owned()],
        discovery_endpoint: Some(
            "https://issuer.example.com/.well-known/openid-configuration".to_owned(),
        ),
        meta: Some(json!({"teamId": "team"})),
        redirect_to: Some("/dashboard".to_owned()),
        use_shared_credentials: false,
    };
    create_oauth_client(&mut client, app_id, &catalog, &oauth_client)
        .await
        .unwrap();
    assert_eq!(
        oauth_client_by_id(&client, app_id, oauth_client.id)
            .await
            .unwrap(),
        Some(oauth_client.clone())
    );
    assert_eq!(
        oauth_client_by_name(&client, app_id, &oauth_client.name)
            .await
            .unwrap(),
        Some(oauth_client.clone())
    );
    update_oauth_client(
        &mut client,
        app_id,
        &catalog,
        oauth_client.id,
        &OAuthClientPatch {
            client_id: Some(Some("replacement-client-id".to_owned())),
            encrypted_client_secret: Some(None),
            extra_audiences: Some(vec!["replacement-audience".to_owned()]),
            meta: Some(Some(json!({"environment": "test"}))),
            redirect_to: Some(None),
            use_shared_credentials: Some(true),
            ..OAuthClientPatch::default()
        },
    )
    .await
    .unwrap();
    let updated_client = oauth_client_by_id(&client, app_id, oauth_client.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_client.client_id.as_deref(),
        Some("replacement-client-id")
    );
    assert_eq!(updated_client.encrypted_client_secret, None);
    assert_eq!(
        updated_client.extra_audiences,
        vec!["replacement-audience".to_owned()]
    );
    assert_eq!(
        updated_client.meta,
        Some(json!({"teamId": "team", "environment": "test"}))
    );
    assert_eq!(updated_client.redirect_to, None);
    assert!(updated_client.use_shared_credentials);

    let social = sign_in_with_oauth(
        &mut client,
        app_id,
        &catalog,
        &OAuthSignIn {
            provider_id: provider.id,
            subject: "new-social-subject".to_owned(),
            email: Some("social@example.com".to_owned()),
            image_url: Some("https://example.com/first.png".to_owned()),
            guest_user_id: None,
            extra_fields: BTreeMap::new(),
            use_shared_credentials: true,
        },
        |_| async { Ok(()) },
        || async { Ok(()) },
    )
    .await
    .unwrap();
    assert!(social.created);
    assert_eq!(social.user.email.as_deref(), Some("social@example.com"));
    assert_eq!(
        user_by_refresh_token(&client, app_id, &catalog, social.refresh_token)
            .await
            .unwrap()
            .unwrap()
            .id,
        social.user.id
    );
    let returning = sign_in_with_oauth(
        &mut client,
        app_id,
        &catalog,
        &OAuthSignIn {
            provider_id: provider.id,
            subject: "new-social-subject".to_owned(),
            email: None,
            image_url: Some("https://example.com/second.png".to_owned()),
            guest_user_id: None,
            extra_fields: [("ignored".to_owned(), json!(true))].into(),
            use_shared_credentials: true,
        },
        |_| async { panic!("returning user must not run signup authorization") },
        || async { panic!("returning user must not run shared-credential admission") },
    )
    .await
    .unwrap();
    assert!(!returning.created);
    assert_eq!(returning.user.id, social.user.id);
    assert_eq!(
        returning.user.image_url.as_deref(),
        Some("https://example.com/second.png")
    );
    let reused = sign_in_with_oauth_reusing_refresh(
        &mut client,
        app_id,
        &catalog,
        &OAuthSignIn {
            provider_id: provider.id,
            subject: "new-social-subject".to_owned(),
            email: None,
            image_url: None,
            guest_user_id: None,
            extra_fields: BTreeMap::new(),
            use_shared_credentials: true,
        },
        Some((social.refresh_token, social.user.id)),
        |_| async { panic!("returning user must not run signup authorization") },
        || async { panic!("returning user must not run shared-credential admission") },
    )
    .await
    .unwrap();
    assert_eq!(reused.refresh_token, social.refresh_token);

    let relay = sign_in_with_oauth(
        &mut client,
        app_id,
        &catalog,
        &OAuthSignIn {
            provider_id: provider.id,
            subject: "apple-relay-subject".to_owned(),
            email: Some("relay@privaterelay.appleid.com".to_owned()),
            image_url: None,
            guest_user_id: None,
            extra_fields: BTreeMap::new(),
            use_shared_credentials: false,
        },
        |_| async { Ok(()) },
        || async { Ok(()) },
    )
    .await
    .unwrap();
    assert_ne!(relay.user.id, first.user.id);
    let revealed = sign_in_with_oauth(
        &mut client,
        app_id,
        &catalog,
        &OAuthSignIn {
            provider_id: provider.id,
            subject: "apple-relay-subject".to_owned(),
            email: Some("first@example.com".to_owned()),
            image_url: None,
            guest_user_id: None,
            extra_fields: BTreeMap::new(),
            use_shared_credentials: false,
        },
        |_| async { panic!("email disambiguation selects an existing user") },
        || async { Ok(()) },
    )
    .await
    .unwrap();
    assert_eq!(revealed.user.id, first.user.id);
    assert_eq!(revealed.link.id, relay.link.id);
    assert_eq!(revealed.link.user_id, first.user.id);

    let upgrade_guest = sign_in_guest(&mut client, app_id, &catalog).await.unwrap();
    let upgraded = sign_in_with_oauth(
        &mut client,
        app_id,
        &catalog,
        &OAuthSignIn {
            provider_id: provider.id,
            subject: "guest-upgrade-subject".to_owned(),
            email: Some("upgraded@example.com".to_owned()),
            image_url: None,
            guest_user_id: Some(upgrade_guest.user.id),
            extra_fields: BTreeMap::new(),
            use_shared_credentials: false,
        },
        |_| async { Ok(()) },
        || async { Ok(()) },
    )
    .await
    .unwrap();
    assert!(upgraded.created);
    assert_eq!(upgraded.user.id, upgrade_guest.user.id);
    assert_eq!(upgraded.user.user_type.as_deref(), Some("user"));
    let link = OAuthUserLink {
        id: Uuid::new_v4(),
        subject: "provider-subject".to_owned(),
        user_id: first.user.id,
        provider_id: provider.id,
    };
    create_oauth_user_link(&mut client, app_id, &catalog, &link)
        .await
        .unwrap();
    assert_eq!(
        oauth_user_link(&client, app_id, &link.subject, provider.id)
            .await
            .unwrap(),
        Some(link.clone())
    );
    update_oauth_link_user(&mut client, app_id, &catalog, link.id, guest.user.id)
        .await
        .unwrap();
    assert_eq!(
        oauth_user_link(&client, app_id, &link.subject, provider.id)
            .await
            .unwrap()
            .unwrap()
            .user_id,
        guest.user.id
    );

    let oauth_client_id = oauth_client.id;
    let code = Uuid::new_v4();
    create_oauth_code(
        &mut client,
        app_id,
        &catalog,
        code,
        oauth_client_id,
        json!({"sub": "provider-subject"}),
        Some("S256"),
        Some("iMnq5o6zALKXGivsnlom_0F5_WYda32GHkxlV7mq7hQ"),
    )
    .await
    .unwrap();
    let consumed = consume_oauth_code(
        &mut client,
        app_id,
        &catalog,
        code,
        Some("verifier"),
        now_ms,
    )
    .await
    .unwrap();
    assert_eq!(consumed.client_id, oauth_client_id);
    assert_eq!(consumed.user_info, json!({"sub": "provider-subject"}));
    assert!(matches!(
        consume_oauth_code(
            &mut client,
            app_id,
            &catalog,
            code,
            Some("verifier"),
            now_ms
        )
        .await,
        Err(AuthError::InvalidOAuthCode)
    ));

    let mismatched_code = Uuid::new_v4();
    create_oauth_code(
        &mut client,
        app_id,
        &catalog,
        mismatched_code,
        oauth_client_id,
        json!({}),
        Some("plain"),
        Some("expected"),
    )
    .await
    .unwrap();
    assert!(matches!(
        consume_oauth_code(
            &mut client,
            app_id,
            &catalog,
            mismatched_code,
            Some("wrong"),
            now_ms
        )
        .await,
        Err(AuthError::OAuth(OAuthError::PkceMismatch))
    ));
    assert!(matches!(
        consume_oauth_code(
            &mut client,
            app_id,
            &catalog,
            mismatched_code,
            Some("expected"),
            now_ms
        )
        .await,
        Err(AuthError::InvalidOAuthCode)
    ));

    let state = Uuid::new_v4();
    let cookie = Uuid::new_v4();
    create_oauth_redirect(
        &mut client,
        app_id,
        &catalog,
        state,
        cookie,
        "https://app.example.com/callback",
        Some("/dashboard"),
        oauth_client_id,
        Some("plain"),
        Some("challenge"),
    )
    .await
    .unwrap();
    let redirect = consume_oauth_redirect(&mut client, app_id, &catalog, state, now_ms)
        .await
        .unwrap();
    assert_eq!(redirect.cookie_hash.len(), 32);
    assert_eq!(redirect.redirect_to.as_deref(), Some("/dashboard"));
    assert!(matches!(
        consume_oauth_redirect(&mut client, app_id, &catalog, state, now_ms).await,
        Err(AuthError::InvalidOAuthRedirect)
    ));
    assert!(
        delete_oauth_client(&mut client, app_id, &catalog, oauth_client_id)
            .await
            .unwrap()
    );
    assert_eq!(
        oauth_client_by_id(&client, app_id, oauth_client_id)
            .await
            .unwrap(),
        None
    );
    assert!(
        !delete_oauth_client(&mut client, app_id, &catalog, oauth_client_id)
            .await
            .unwrap()
    );

    let model_user_id = Uuid::new_v4();
    let model_user = create_user(
        &mut client,
        app_id,
        &catalog,
        &CreateUser {
            id: model_user_id,
            email: Some("model-old@example.com".to_owned()),
            user_type: Some("user".to_owned()),
            image_url: None,
            extra_fields: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    assert_eq!(model_user.id, model_user_id);
    assert!(users_at_least(&client, app_id, 1).await.unwrap());
    assert!(!users_at_least(&client, app_id, 10_000).await.unwrap());
    let projected = get_users(
        &client,
        app_id,
        &catalog,
        &[model_user_id, Uuid::new_v4(), first.user.id],
    )
    .await
    .unwrap();
    assert_eq!(
        projected.iter().map(|user| user.id).collect::<Vec<_>>(),
        vec![model_user_id, first.user.id]
    );

    let updated = update_user_email(
        &mut client,
        app_id,
        &catalog,
        model_user_id,
        "model-new@example.com",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(updated.email.as_deref(), Some("model-new@example.com"));
    update_user_image_url(
        &mut client,
        app_id,
        &catalog,
        model_user_id,
        "https://example.com/avatar.png",
    )
    .await
    .unwrap();
    assert_eq!(
        get_user(&client, app_id, &catalog, model_user_id)
            .await
            .unwrap()
            .unwrap()
            .image_url
            .as_deref(),
        Some("https://example.com/avatar.png")
    );

    let linked_guest = sign_in_guest(&mut client, app_id, &catalog).await.unwrap();
    link_guest_user(
        &mut client,
        app_id,
        &catalog,
        model_user_id,
        linked_guest.user.id,
    )
    .await
    .unwrap();
    assert_eq!(
        get_user(&client, app_id, &catalog, linked_guest.user.id)
            .await
            .unwrap()
            .unwrap()
            .linked_primary_user,
        Some(model_user_id)
    );
    assert_eq!(
        delete_user_by_email(&mut client, app_id, &catalog, "model-new@example.com")
            .await
            .unwrap()
            .unwrap()
            .id,
        model_user_id
    );
    assert!(
        delete_user_by_email(&mut client, app_id, &catalog, "model-new@example.com")
            .await
            .unwrap()
            .is_none()
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
}
