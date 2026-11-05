use instant_transact::{TransactionOptions, compile_transaction, execute_transaction};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::admin_metadata::load_attribute_catalog;
use crate::postgres_dashboard_policy::INSTANT_CONFIG_APP_ID;

/// Seeded deployment-level apps that must never be created through an
/// application or dashboard API.
pub const INSTANT_SYSTEM_CATALOG_APP_ID: Uuid =
    Uuid::from_u128(0xa111_1111_1111_1111_1111_1111_1111_1ca7);
pub const INSTANT_HOMEPAGE_APP_ID: Uuid =
    Uuid::from_u128(0xfc5a_4977_910a_43d9_ac28_39c7_837c_1eb5);

const FLAGS_ETYPE: &str = "flags";

/// Metadata for a config-app flag the server reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownConfigFlag {
    pub name: &'static str,
    pub value_type: &'static str,
    pub description: &'static str,
}

/// Flags documented in `docs/configuration.md` plus the live controls read by
/// the realtime, backup, rate-limit and storage modules.
pub const KNOWN_CONFIG_FLAGS: &[KnownConfigFlag] = &[
    KnownConfigFlag {
        name: "always-materialize-attr-ids",
        value_type: "array",
        description: "Attribute UUIDs or [uuid, label] pairs that are materialized during query planning.",
    },
    KnownConfigFlag {
        name: "row-hint-threshold",
        value_type: "number",
        description: "Row-count threshold above which PostgreSQL planner hints are applied.",
    },
    KnownConfigFlag {
        name: "disable-hint-query-hashes",
        value_type: "array",
        description: "Query hashes that must not use planner hints.",
    },
    KnownConfigFlag {
        name: "use-hint-query-hashes",
        value_type: "array",
        description: "Query hashes that always use planner hints.",
    },
    KnownConfigFlag {
        name: "enable-row-hints-query-hashes",
        value_type: "array",
        description: "Query hashes that enable row-count hints.",
    },
    KnownConfigFlag {
        name: "disable-row-hints-query-hashes",
        value_type: "array",
        description: "Query hashes that disable row-count hints.",
    },
    KnownConfigFlag {
        name: "disable-row-hints-apps",
        value_type: "array",
        description: "App UUIDs that disable row-count hints.",
    },
    KnownConfigFlag {
        name: "coarse-topics-apps",
        value_type: "array",
        description: "App UUIDs that use coarse realtime topics.",
    },
    KnownConfigFlag {
        name: "refresh-throttled-apps",
        value_type: "array",
        description: "App UUIDs subject to the refresh throttle.",
    },
    KnownConfigFlag {
        name: "refresh-throttle-ms",
        value_type: "number",
        description: "Refresh throttle window in milliseconds.",
    },
    KnownConfigFlag {
        name: "combine-transacts",
        value_type: "boolean",
        description: "Combine concurrent transactions for the same app.",
    },
    KnownConfigFlag {
        name: "team-emails",
        value_type: "array",
        description: "Array of {\"email\": \"...\"} objects that grant dashboard operator rights.",
    },
    KnownConfigFlag {
        name: "dashboard-signups",
        value_type: "object",
        description: "Signup policy: {\"mode\": \"open\"|\"restricted\"|\"closed\", \"allowedEmails\": [...]}.",
    },
    KnownConfigFlag {
        name: "ephemeral-apps-enabled",
        value_type: "boolean",
        description: "Enables unauthenticated ephemeral app creation.",
    },
    KnownConfigFlag {
        name: "magic-code-rate-limit-per-hour",
        value_type: "integer",
        description: "Hourly magic-code send limit per app and email.",
    },
    KnownConfigFlag {
        name: "magic-code-daily-limit-per-app",
        value_type: "integer",
        description: "Daily magic-code send cap for every app.",
    },
    KnownConfigFlag {
        name: "magic-code-rate-limit-per-ip-hour",
        value_type: "integer",
        description: "Hourly magic-code send cap per client IP on the public runtime route.",
    },
    KnownConfigFlag {
        name: "magic-code-verify-limit-per-ip-hour",
        value_type: "integer",
        description: "Hourly magic-code verify cap per client IP on the public runtime route.",
    },
    KnownConfigFlag {
        name: "default-magic-code-expiry-minutes",
        value_type: "integer",
        description: "Default magic-code expiry in minutes; per-app overrides cap at 60.",
    },
    KnownConfigFlag {
        name: "on-demand-backup-max-triples",
        value_type: "integer",
        description: "Maximum triples for an on-demand backup; 0 removes the limit.",
    },
    KnownConfigFlag {
        name: "on-demand-backup-app-rate-limit",
        value_type: "object",
        description: "Rate-limit policy JSON for on-demand backups per app, or null.",
    },
    KnownConfigFlag {
        name: "on-demand-backup-ip-rate-limit",
        value_type: "object",
        description: "Rate-limit policy JSON for on-demand backups per client IP, or null.",
    },
    KnownConfigFlag {
        name: "enable-wal-entity-log-apps-map",
        value_type: "object",
        description: "Object mapping app UUIDs to true to enable the WAL entity log.",
    },
    KnownConfigFlag {
        name: "backup-skip-app-ids",
        value_type: "array",
        description: "App UUIDs excluded from nightly backups.",
    },
    KnownConfigFlag {
        name: "smokescreen-whitelist-ips",
        value_type: "array",
        description: "IP addresses or CIDRs allowed past the webhook SSRF guard.",
    },
    KnownConfigFlag {
        name: "storage-sweeper-max-loops",
        value_type: "integer",
        description: "Storage sweep loops per run.",
    },
    KnownConfigFlag {
        name: "storage-quota-bytes",
        value_type: "integer",
        description: "Deployment-wide soft storage quota in bytes; unset means unlimited.",
    },
    KnownConfigFlag {
        name: "storage-quota-bytes-by-app",
        value_type: "object",
        description: "Object mapping app UUIDs to soft storage quotas in bytes.",
    },
    KnownConfigFlag {
        name: "max-live-sessions-per-ip",
        value_type: "integer",
        description: "Maximum concurrent live sessions from one client IP.",
    },
    KnownConfigFlag {
        name: "max-sessions-per-app",
        value_type: "integer",
        description: "Maximum concurrent live sessions per app.",
    },
    KnownConfigFlag {
        name: "max-subscriptions-per-session",
        value_type: "integer",
        description: "Maximum subscriptions per realtime session.",
    },
    KnownConfigFlag {
        name: "max-instaql-query-bytes",
        value_type: "integer",
        description: "Maximum InstaQL query JSON size in bytes.",
    },
    KnownConfigFlag {
        name: "disable-triples-size-collection",
        value_type: "boolean",
        description: "Disables the triples-size collector.",
    },
    KnownConfigFlag {
        name: "triples-size-collection-batch-size",
        value_type: "integer",
        description: "Rows the triples-size collector processes per batch.",
    },
    KnownConfigFlag {
        name: "triples-size-collection-max-loops",
        value_type: "integer",
        description: "Maximum triples-size collector loops per run.",
    },
    KnownConfigFlag {
        name: "triples-size-collection-loop-limit-alert-threshold",
        value_type: "integer",
        description: "Loop count that raises a triples-size collector alert.",
    },
    KnownConfigFlag {
        name: "more-vfutures-instances",
        value_type: "array",
        description: "Instance keys that use the expanded receive executor.",
    },
    KnownConfigFlag {
        name: "enable-admin-transact-queue-apps",
        value_type: "array",
        description: "App UUIDs that serialize admin transactions through the queue.",
    },
    KnownConfigFlag {
        name: "admin-rate-limit-for-app",
        value_type: "object",
        description: "Object mapping app UUIDs to admin request rate-limit policies.",
    },
    KnownConfigFlag {
        name: "global-admin-rate-limit",
        value_type: "object",
        description: "Deployment-wide admin request rate-limit policy.",
    },
    KnownConfigFlag {
        name: "rate-limit-tx-based-on-conn-pool-buffer",
        value_type: "integer",
        description: "Connection-pool buffer below which permissioned transactions are rate limited.",
    },
];

#[derive(Debug, Error)]
pub enum ConfigAppError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("the config app is not seeded; run `instant-server migrate up`")]
    MissingApp,
    #[error("config app catalog error: {0}")]
    Catalog(String),
    #[error("invalid config app transaction: {0}")]
    Compile(String),
    #[error("config app transaction failed: {0}")]
    Execute(String),
}

/// Returns true for the config app, the system catalog, the seeded homepage
/// app, and any extra `INSTANT_ON_INSTANT_APP_ID` configured for this process.
#[must_use]
pub fn is_reserved_app_id(app_id: Uuid) -> bool {
    app_id == INSTANT_CONFIG_APP_ID
        || app_id == INSTANT_SYSTEM_CATALOG_APP_ID
        || app_id == INSTANT_HOMEPAGE_APP_ID
        || configured_instant_on_instant_app_id() == Some(app_id)
}

fn configured_instant_on_instant_app_id() -> Option<Uuid> {
    std::env::var("INSTANT_ON_INSTANT_APP_ID")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .and_then(|value| Uuid::parse_str(&value).ok())
}

/// Lists the config app's `flags` entities as sorted setting/value pairs.
///
/// # Errors
///
/// Returns a database error when the live flags cannot be read.
pub async fn list_config_flags(client: &Client) -> Result<Vec<(String, Value)>, ConfigAppError> {
    let rows = client
        .query(
            "SELECT flag_setting.value #>> '{}', flag_value.value
             FROM attrs setting_attr
             JOIN triples flag_setting
               ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
             JOIN attrs value_attr
               ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
              AND value_attr.label = 'value'
             JOIN triples flag_value
               ON flag_value.app_id = $1
              AND flag_value.entity_id = flag_setting.entity_id
              AND flag_value.attr_id = value_attr.id
             WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
               AND setting_attr.label = 'setting'
             ORDER BY 1",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect())
}

/// Reads one config app `flags` value.
///
/// # Errors
///
/// Returns a database error when the live flag cannot be read.
pub async fn get_config_flag(
    client: &Client,
    setting: &str,
) -> Result<Option<Value>, ConfigAppError> {
    Ok(client
        .query_opt(
            "SELECT flag_value.value
             FROM attrs setting_attr
             JOIN triples flag_setting
               ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
             JOIN attrs value_attr
               ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
              AND value_attr.label = 'value'
             JOIN triples flag_value
               ON flag_value.app_id = $1
              AND flag_value.entity_id = flag_setting.entity_id
              AND flag_value.attr_id = value_attr.id
             WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
               AND setting_attr.label = 'setting'
               AND flag_setting.value = to_jsonb($2::text)
             LIMIT 1",
            &[&INSTANT_CONFIG_APP_ID, &setting],
        )
        .await?
        .map(|row| row.get(0)))
}

/// Creates or replaces a config app `flags` value through the native
/// transaction path, so WAL consumers invalidate their caches.
///
/// # Errors
///
/// Returns a database, schema or transaction error.
pub async fn set_config_flag(
    client: &mut Client,
    setting: &str,
    value: Value,
) -> Result<(), ConfigAppError> {
    ensure_config_app(client).await?;
    let catalog = load_catalog(client).await?;
    let (_id_attr, setting_attr, value_attr, mut raw_steps) = flags_attributes(&catalog);
    if let Some(entity_id) = flag_entity(client, setting, setting_attr).await? {
        raw_steps.push(json!(["add-triple", entity_id, value_attr, value]));
    } else {
        let entity_id = Uuid::new_v4();
        raw_steps.push(json!(["add-triple", entity_id, setting_attr, setting]));
        raw_steps.push(json!(["add-triple", entity_id, value_attr, value]));
    }
    execute_steps(client, &catalog, &raw_steps).await
}

/// Deletes one config app `flags` entity.
///
/// Returns `true` when the setting existed and was deleted.
///
/// # Errors
///
/// Returns a database or transaction error.
pub async fn unset_config_flag(client: &mut Client, setting: &str) -> Result<bool, ConfigAppError> {
    ensure_config_app(client).await?;
    let catalog = load_catalog(client).await?;
    let Some(setting_attr) = catalog.forward(FLAGS_ETYPE, "setting") else {
        return Ok(false);
    };
    let Some(entity_id) = flag_entity(client, setting, setting_attr.id).await? else {
        return Ok(false);
    };
    let raw_steps = [json!(["delete-entity", entity_id, FLAGS_ETYPE])];
    execute_steps(client, &catalog, &raw_steps).await?;
    Ok(true)
}

async fn ensure_config_app(client: &Client) -> Result<(), ConfigAppError> {
    if client
        .query_opt(
            "SELECT 1 FROM apps WHERE id = $1 AND deletion_marked_at IS NULL",
            &[&INSTANT_CONFIG_APP_ID],
        )
        .await?
        .is_some()
    {
        Ok(())
    } else {
        Err(ConfigAppError::MissingApp)
    }
}

async fn load_catalog(
    client: &Client,
) -> Result<instant_instaql::AttributeCatalog, ConfigAppError> {
    load_attribute_catalog(client, INSTANT_CONFIG_APP_ID)
        .await
        .map_err(|error| ConfigAppError::Catalog(format!("{error:?}")))
}

fn flags_attributes(catalog: &instant_instaql::AttributeCatalog) -> (Uuid, Uuid, Uuid, Vec<Value>) {
    let mut steps = Vec::new();
    let mut attribute = |label: &str, unique: bool| {
        if let Some(existing) = catalog.forward(FLAGS_ETYPE, label) {
            return existing.id;
        }
        let id = Uuid::new_v4();
        steps.push(json!(["add-attr", {
            "id": id,
            "forward-identity": [Uuid::new_v4(), FLAGS_ETYPE, label],
            "value-type": "blob",
            "cardinality": "one",
            "unique?": unique,
            "index?": unique,
        }]));
        id
    };
    let id = attribute("id", true);
    let setting = attribute("setting", true);
    let value = attribute("value", false);
    (id, setting, value, steps)
}

async fn flag_entity(
    client: &Client,
    setting: &str,
    setting_attr: Uuid,
) -> Result<Option<Uuid>, ConfigAppError> {
    Ok(client
        .query_opt(
            "SELECT entity_id
             FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = to_jsonb($3::text)
             LIMIT 1",
            &[&INSTANT_CONFIG_APP_ID, &setting_attr, &setting],
        )
        .await?
        .map(|row| row.get(0)))
}

async fn execute_steps(
    client: &mut Client,
    catalog: &instant_instaql::AttributeCatalog,
    raw_steps: &[Value],
) -> Result<(), ConfigAppError> {
    let raw_steps = Value::Array(raw_steps.to_vec());
    let steps = compile_transaction(&raw_steps, catalog)
        .map_err(|error| ConfigAppError::Compile(error.to_string()))?;
    execute_transaction(
        client,
        INSTANT_CONFIG_APP_ID,
        catalog,
        &steps,
        TransactionOptions::default(),
    )
    .await
    .map_err(|error| ConfigAppError::Execute(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn known_flags_have_unique_names_and_metadata() {
        let mut names = HashSet::new();
        for flag in KNOWN_CONFIG_FLAGS {
            assert!(names.insert(flag.name), "duplicate flag {}", flag.name);
            assert!(!flag.value_type.is_empty(), "{} has no type", flag.name);
            assert!(
                !flag.description.is_empty(),
                "{} has no description",
                flag.name
            );
        }
        assert!(names.contains("team-emails"));
        assert!(names.contains("ephemeral-apps-enabled"));
    }

    #[test]
    fn seeded_app_ids_are_reserved() {
        for app_id in [
            INSTANT_CONFIG_APP_ID,
            INSTANT_SYSTEM_CATALOG_APP_ID,
            INSTANT_HOMEPAGE_APP_ID,
        ] {
            assert!(is_reserved_app_id(app_id));
        }
        assert_eq!(
            INSTANT_SYSTEM_CATALOG_APP_ID.to_string(),
            "a1111111-1111-1111-1111-111111111ca7"
        );
        assert_eq!(
            INSTANT_HOMEPAGE_APP_ID.to_string(),
            "fc5a4977-910a-43d9-ac28-39c7837c1eb5"
        );
        assert!(!is_reserved_app_id(Uuid::from_u128(1)));
    }
}
