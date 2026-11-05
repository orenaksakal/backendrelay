use std::collections::BTreeMap;

use instant_auth::{AuthError, CreateUser};
use instant_instaql::system_attr_id;
use instant_server::{PostgresRuntimeOAuthPolicy, RuntimeOAuthPolicy};
use serde_json::json;
use tokio_postgres::{NoTls, types::Json};
use uuid::Uuid;

#[tokio::test]
async fn runtime_oauth_policy_enforces_user_rules_and_bounded_shared_quota() {
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_oauth_policy_enforces_user_rules_and_bounded_shared_quota",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(
            "CREATE TEMP TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, value_type text NOT NULL,
               cardinality text NOT NULL, is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL, is_required boolean NOT NULL DEFAULT false,
               forward_ident uuid NOT NULL, etype text NOT NULL, label text NOT NULL,
               reverse_ident uuid, reverse_etype text, reverse_label text,
               inferred_types bit(32), on_delete text, on_delete_reverse text,
               checked_data_type text, checking_data_type boolean, indexing boolean,
               setting_unique boolean, metadata jsonb, deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE rules (app_id uuid PRIMARY KEY, code jsonb NOT NULL);
             CREATE TEMP TABLE triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();
    let allowed_app = Uuid::new_v4();
    let denied_app = Uuid::new_v4();
    let origin_app = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO rules VALUES ($1, $2), ($3, $4)",
            &[
                &allowed_app,
                &Json(json!({"$users": {"allow": {"create": "data.invite == 'yes'"}}})),
                &denied_app,
                &Json(json!({"$users": {"allow": {"create": "false"}}})),
            ],
        )
        .await
        .unwrap();
    client.execute("INSERT INTO rules VALUES ($1, $2)", &[
        &origin_app, &Json(json!({"$users":{"allow":{"create":
            "data.email != null && (data.isPro == null || data.isPro == false) && (data.isBanned == null || data.isBanned == false) && (data.role == null || data.role == 'user') && data.stripeCustomerId == null && data.stripeCheckoutSessionId == null && data.stripePaymentIntentId == null && request.origin in ['https://www.example.com', 'https://example.com', 'http://localhost:3000', 'http://localhost:3107']"
        }}}))
    ]).await.unwrap();
    let id_attr = system_attr_id("$users", "id").unwrap();
    for _ in 0..2 {
        let user_id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO triples
                 VALUES ($1, $2::uuid, $3, to_jsonb($2::uuid::text), now())",
                &[&allowed_app, &user_id, &id_attr],
            )
            .await
            .unwrap();
    }

    let policy = PostgresRuntimeOAuthPolicy::new(client).with_shared_oauth_user_limit(2);
    let candidate = CreateUser {
        id: Uuid::new_v4(),
        email: Some("person@example.com".to_owned()),
        user_type: Some("user".to_owned()),
        image_url: None,
        extra_fields: BTreeMap::from([("invite".to_owned(), json!("yes"))]),
    };
    instant_server::RuntimeAuthPolicy::authorize_signup_with_origin(
        &policy,
        origin_app,
        &candidate,
        Some("http://localhost:3107"),
    )
    .await
    .unwrap();
    assert!(matches!(
        instant_server::RuntimeAuthPolicy::authorize_signup_with_origin(
            &policy,
            origin_app,
            &candidate,
            Some("https://untrusted.example")
        )
        .await,
        Err(AuthError::SignupPermissionDenied)
    ));
    assert!(
        instant_server::RuntimeAuthPolicy::authorize_signup_with_origin(
            &policy, origin_app, &candidate, None
        )
        .await
        .is_err()
    );
    policy
        .authorize_signup(allowed_app, &candidate)
        .await
        .unwrap();
    assert_eq!(
        policy
            .authorize_signup(denied_app, &candidate)
            .await
            .unwrap_err()
            .to_string(),
        AuthError::SignupPermissionDenied.to_string()
    );
    assert!(matches!(
        policy.authorize_shared_credentials(allowed_app).await,
        Err(AuthError::SharedOAuthUserLimit(2))
    ));
    policy
        .authorize_shared_credentials(denied_app)
        .await
        .unwrap();
}
