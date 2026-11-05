//! Provider-specific smoke tests for `signInWithIdToken`.
//!
//! Clerk, `LinkedIn` and Firebase all sign their ID tokens with RS256 in
//! production. These tests sign with the repository's EC fixture instead: the
//! verifier's issuer, audience, nonce and claim handling is identical for both
//! algorithms, and this keeps the test free of a second private key. Everything
//! runs against an in-memory JWKS, so no network or provider account is needed.

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use instant_auth::oauth::{IdTokenOptions, OAuthError, OAuthUserInfo, verify_id_token};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value as JsonValue, json};

const PRIVATE_KEY_DER: &str = concat!(
    "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtgejda/UoPuAPpAh",
    "TlcRSN8/MrRU9cE7XcbxSWANqvahRANCAASApwdGtLB7wbI/DW8xPJ2fJczvirtP",
    "Lhh4Zj23x8h6VsNggHAQ+yZM/4iyJGouweDSrZ/9mLKnI+bnRz2TAtZi"
);

fn jwks() -> JwkSet {
    serde_json::from_value(json!({
        "keys": [{
            "kty": "EC",
            "crv": "P-256",
            "kid": "smoke-test-key",
            "use": "sig",
            "alg": "ES256",
            "x": "gKcHRrSwe8GyPw1vMTydnyXM74q7Ty4YeGY9t8fIelY",
            "y": "w2CAcBD7Jkz_iLIkai7B4NKtn_2Ysqcj5udHPZMC1mI"
        }]
    }))
    .unwrap()
}

fn token(claims: &JsonValue) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("smoke-test-key".to_owned());
    let key = EncodingKey::from_ec_der(&STANDARD.decode(PRIVATE_KEY_DER).unwrap());
    encode(&header, claims, &key).unwrap()
}

fn supported_algorithms() -> BTreeSet<String> {
    ["ES256".to_owned()].into()
}

fn options<'a>(
    issuer: &'a str,
    accepted_audiences: &'a [String],
    algorithms: &'a BTreeSet<String>,
) -> IdTokenOptions<'a> {
    IdTokenOptions {
        issuer,
        accepted_audiences,
        supported_algorithms: algorithms,
        nonce: None,
        allow_unverified_email: false,
    }
}

#[test]
fn clerk_session_tokens_need_an_audience_and_accept_the_requesting_origin() {
    let issuer = "https://example.clerk.accounts.dev";
    let client_id = ["client_abc".to_owned()];
    let algorithms = supported_algorithms();
    let jwks = jwks();
    let mut claims = json!({
        "exp": jsonwebtoken::get_current_timestamp() + 3_600,
        "iss": issuer,
        "sub": "user_2abc",
        "sid": "sess_123",
        "email": "person@example.com",
        "email_verified": true,
        "picture": "https://example.com/avatar.png"
    });

    // The default Clerk session token has no `aud` claim, so it is rejected
    // before the audience comparison runs.
    assert!(matches!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        ),
        Err(OAuthError::InvalidJwt(_))
    ));

    // A customized Clerk session token with `aud` equal to the OAuth client's
    // `client_id` signs in.
    claims["aud"] = json!("client_abc");
    assert_eq!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        )
        .unwrap(),
        OAuthUserInfo {
            subject: "user_2abc".to_owned(),
            email: Some("person@example.com".to_owned()),
            image_url: Some("https://example.com/avatar.png".to_owned()),
        }
    );

    // Clerk also sets `azp` to the requesting origin. A present `azp` must be
    // accepted too, so the origin has to be listed in `extra_audiences`.
    claims["azp"] = json!("https://app.example.com");
    assert!(matches!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        ),
        Err(OAuthError::AudienceMismatch)
    ));
    let with_origin = [
        "client_abc".to_owned(),
        "https://app.example.com".to_owned(),
    ];
    assert!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &with_origin, &algorithms)
        )
        .is_ok()
    );
}

#[test]
fn linkedin_id_tokens_carry_the_client_id_audience() {
    let issuer = "https://www.linkedin.com/oauth";
    let client_id = ["linkedin-client".to_owned()];
    let algorithms = supported_algorithms();
    let jwks = jwks();
    let mut claims = json!({
        "exp": jsonwebtoken::get_current_timestamp() + 3_600,
        "iss": issuer,
        "aud": "linkedin-client",
        "sub": "linkedin-subject",
        "email": "person@example.com",
        "email_verified": true,
        "picture": "https://example.com/linkedin.png"
    });
    assert_eq!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        )
        .unwrap(),
        OAuthUserInfo {
            subject: "linkedin-subject".to_owned(),
            email: Some("person@example.com".to_owned()),
            image_url: Some("https://example.com/linkedin.png".to_owned()),
        }
    );

    // An unverified LinkedIn email is dropped unless the OAuth client opts in.
    claims["email_verified"] = json!(false);
    assert_eq!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        )
        .unwrap()
        .email,
        None
    );

    // A token minted for another LinkedIn application is rejected.
    claims["aud"] = json!("another-linkedin-client");
    assert!(matches!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        ),
        Err(OAuthError::AudienceMismatch)
    ));
}

#[test]
fn firebase_id_tokens_use_the_project_id_as_the_audience() {
    let project_id = "backendrelay-test";
    let issuer = "https://securetoken.google.com/backendrelay-test";
    let client_id = [project_id.to_owned()];
    let algorithms = supported_algorithms();
    let jwks = jwks();
    let claims = json!({
        "exp": jsonwebtoken::get_current_timestamp() + 3_600,
        "iss": issuer,
        "aud": project_id,
        "sub": "firebase-subject",
        "user_id": "firebase-subject",
        "auth_time": jsonwebtoken::get_current_timestamp(),
        "email": "person@example.com",
        "email_verified": true,
        "picture": "https://example.com/firebase.png",
        "firebase": {
            "sign_in_provider": "password",
            "identities": {}
        }
    });
    assert_eq!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &client_id, &algorithms)
        )
        .unwrap(),
        OAuthUserInfo {
            subject: "firebase-subject".to_owned(),
            email: Some("person@example.com".to_owned()),
            image_url: Some("https://example.com/firebase.png".to_owned()),
        }
    );

    // A Firebase OAuth client without a client ID has no accepted audience, so
    // every Firebase token is rejected. The project ID must be configured as
    // the client's `client_id`.
    let no_audience: [String; 0] = [];
    assert!(matches!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &no_audience, &algorithms)
        ),
        Err(OAuthError::AudienceMismatch)
    ));

    // A token from a different Firebase project is rejected.
    let other_project = ["another-project".to_owned()];
    assert!(matches!(
        verify_id_token(
            &token(&claims),
            &jwks,
            &options(issuer, &other_project, &algorithms)
        ),
        Err(OAuthError::AudienceMismatch)
    ));
}
