//! Guards the compose files against blank defaults that the server rejects.
//!
//! Compose passes optional settings through as `NAME: ${NAME:-}`. Every one of
//! those becomes an empty string inside the container, so the server must treat
//! a blank value exactly like an unset variable.

use std::path::{Path, PathBuf};

use instant_server::{ProductionServerSecrets, ServerConfig};
use regex::Regex;

// `docker-compose.test.yml` is excluded on purpose: its acceptance scripts always
// set `S3_PUBLIC_ENDPOINT`, which is validated exactly and must not be blank.
const COMPOSE_FILES: [&str; 3] = [
    "self-hosting/docker-compose.yml",
    "self-hosting/docker-compose.with-caddy.yml",
    "self-hosting/docker-compose.local.yml",
];

const REQUIRED_SECRETS: [(&str, &str); 3] = [
    ("INSTANT_AEAD_KEYSET_JSON", "compose-blank-defaults-oauth"),
    (
        "INSTANT_WEBHOOK_KEYSET_JSON",
        "compose-blank-defaults-webhook",
    ),
    (
        "INSTANT_RATE_LIMIT_HMAC_KEY",
        "compose-blank-defaults-rate-limit",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn blank_default_server_env(contents: &str) -> Vec<String> {
    // Rust's regex engine has no backreferences, so compare both captures.
    let service = Regex::new(r"^  ([a-z][a-z0-9_-]*):\s*$").expect("valid compose service regex");
    let entry = Regex::new(r"^\s+([A-Z][A-Z0-9_]*): \$\{([A-Z][A-Z0-9_]*):-\}\s*$")
        .expect("valid compose environment regex");
    let mut current_service = String::new();
    let mut names = Vec::new();
    for line in contents.lines() {
        if let Some(captures) = service.captures(line) {
            captures[1].clone_into(&mut current_service);
            continue;
        }
        if current_service != "server" {
            continue;
        }
        let Some(captures) = entry.captures(line) else {
            continue;
        };
        assert_eq!(
            &captures[1], &captures[2],
            "environment default must reference its own name: {line}"
        );
        names.push(captures[1].to_owned());
    }
    names
}

#[test]
fn compose_blank_defaults_are_accepted_by_startup_config() {
    for file in COMPOSE_FILES {
        let path = repo_root().join(file);
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let blank_names = blank_default_server_env(&contents);
        assert!(
            !blank_names.is_empty(),
            "{file}: found no blank-default server environment entries"
        );

        let blank_pairs = blank_names.iter().map(|name| (name.as_str(), ""));
        ServerConfig::from_pairs(blank_pairs).unwrap_or_else(|error| {
            panic!("{file}: blank server environment rejected by ServerConfig: {error}")
        });

        let secret_pairs = REQUIRED_SECRETS
            .into_iter()
            .chain(blank_names.iter().map(|name| (name.as_str(), "")));
        ProductionServerSecrets::from_pairs(secret_pairs).unwrap_or_else(|error| {
            panic!("{file}: blank server environment rejected by ProductionServerSecrets: {error}")
        });
    }
}
