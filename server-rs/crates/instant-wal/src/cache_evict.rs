use instant_instaql::system_attr_id;
use serde_json::Value;
use uuid::Uuid;

use crate::{WalAction, WalEntry};

/// Cache-side work carried by one raw wal2json row.
///
/// These intents are emitted before application transaction assembly because several
/// reference caches are backed by tables whose writes do not produce realtime query
/// records. Optional identifiers preserve the reference listener's best-effort behavior
/// for incomplete row images without misclassifying unsupported actions.
#[derive(Clone, Debug, PartialEq)]
pub enum WalCacheEviction {
    Attribute {
        app_id: Option<Uuid>,
        attr_id: Option<Uuid>,
    },
    Rules {
        app_id: Option<Uuid>,
    },
    App {
        app_id: Option<Uuid>,
        changed_status: Option<Value>,
    },
    InstantUser {
        user_id: Option<Uuid>,
    },
    TransactionMaxSeen {
        tx_id: Option<i64>,
    },
    StreamMachine {
        stream_id: Uuid,
        machine_id: Uuid,
    },
    Webhook {
        app_id: Option<Uuid>,
        webhook_id: Option<Uuid>,
    },
}

/// Classifies the raw row-level cache work performed by the JVM WAL listener.
///
/// Insert, update, and delete are the only supported cache actions. In particular,
/// truncate, logical messages, transaction boundaries, and future wal2json actions do
/// not accidentally turn into cache events.
#[must_use]
pub fn cache_eviction_for_entry(entry: &WalEntry) -> Option<WalCacheEviction> {
    if !matches!(
        entry.action,
        WalAction::Insert | WalAction::Update | WalAction::Delete
    ) {
        return None;
    }

    match entry.table.as_deref()? {
        "attrs" => Some(WalCacheEviction::Attribute {
            app_id: uuid_from_row(entry, "app_id"),
            attr_id: uuid_from_row(entry, "id"),
        }),
        "rules" => Some(WalCacheEviction::Rules {
            app_id: uuid_from_row(entry, "app_id"),
        }),
        "apps" => {
            let changed_status = (entry.action == WalAction::Update)
                .then(|| entry.column("status"))
                .flatten()
                .filter(|new_status| entry.identity_column("status") != Some(*new_status))
                .cloned();
            Some(WalCacheEviction::App {
                app_id: uuid_from_row(entry, "id"),
                changed_status,
            })
        }
        "instant_users" => Some(WalCacheEviction::InstantUser {
            user_id: uuid_from_row(entry, "id"),
        }),
        "transactions" if entry.action == WalAction::Insert => {
            Some(WalCacheEviction::TransactionMaxSeen {
                tx_id: entry.column("id").and_then(parse_i64),
            })
        }
        "triples" if entry.action == WalAction::Update => stream_machine_eviction(entry),
        "webhooks" => Some(WalCacheEviction::Webhook {
            app_id: uuid_from_row(entry, "app_id"),
            webhook_id: uuid_from_row(entry, "id"),
        }),
        _ => None,
    }
}

fn uuid_from_row(entry: &WalEntry, name: &str) -> Option<Uuid> {
    entry
        .column(name)
        .or_else(|| entry.identity_column(name))
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn parse_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn stream_machine_eviction(entry: &WalEntry) -> Option<WalCacheEviction> {
    let machine_attr = system_attr_id("$streams", "machineId").ok()?;
    if uuid_from_columns(&entry.identity, "attr_id")? != machine_attr {
        return None;
    }
    let stream_id = uuid_from_columns(&entry.columns, "entity_id")?;
    let encoded_machine_id = entry.column("value")?.as_str()?;
    let machine_id = serde_json::from_str::<String>(encoded_machine_id)
        .ok()?
        .parse()
        .ok()?;
    Some(WalCacheEviction::StreamMachine {
        stream_id,
        machine_id,
    })
}

fn uuid_from_columns(columns: &[crate::WalColumn], name: &str) -> Option<Uuid> {
    columns
        .iter()
        .find(|column| column.name == name)?
        .value
        .as_str()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::WalColumn;

    fn column(name: &str, value: Value) -> WalColumn {
        WalColumn {
            name: name.to_owned(),
            value,
        }
    }

    fn entry(action: WalAction, table: &str) -> WalEntry {
        WalEntry {
            action,
            tx_bytes: 0,
            table: Some(table.to_owned()),
            columns: Vec::new(),
            identity: Vec::new(),
            prefix: None,
            content: None,
            lsn: None,
            nextlsn: None,
        }
    }

    #[test]
    fn update_and_delete_identifiers_fall_back_to_the_old_row_image() {
        let app_id = Uuid::new_v4();
        let attr_id = Uuid::new_v4();
        let mut attr = entry(WalAction::Delete, "attrs");
        attr.identity = vec![
            column("app_id", json!(app_id)),
            column("id", json!(attr_id)),
        ];
        assert_eq!(
            cache_eviction_for_entry(&attr),
            Some(WalCacheEviction::Attribute {
                app_id: Some(app_id),
                attr_id: Some(attr_id),
            })
        );

        let webhook_id = Uuid::new_v4();
        let mut webhook = entry(WalAction::Update, "webhooks");
        webhook.identity = vec![
            column("app_id", json!(app_id)),
            column("id", json!(webhook_id)),
        ];
        assert_eq!(
            cache_eviction_for_entry(&webhook),
            Some(WalCacheEviction::Webhook {
                app_id: Some(app_id),
                webhook_id: Some(webhook_id),
            })
        );
    }

    #[test]
    fn app_status_notification_is_only_attached_to_a_changed_update() {
        let app_id = Uuid::new_v4();
        let mut app = entry(WalAction::Update, "apps");
        app.columns = vec![
            column("id", json!(app_id)),
            column("status", json!("active")),
        ];
        app.identity = vec![column("status", json!("paused"))];
        assert_eq!(
            cache_eviction_for_entry(&app),
            Some(WalCacheEviction::App {
                app_id: Some(app_id),
                changed_status: Some(json!("active")),
            })
        );

        app.identity = vec![column("status", json!("active"))];
        assert_eq!(
            cache_eviction_for_entry(&app),
            Some(WalCacheEviction::App {
                app_id: Some(app_id),
                changed_status: None,
            })
        );
    }

    #[test]
    fn unsupported_actions_and_tables_do_not_emit_cache_work() {
        for action in [
            WalAction::Begin,
            WalAction::Truncate,
            WalAction::Message,
            WalAction::Close,
            WalAction::Unknown("future".to_owned()),
        ] {
            assert_eq!(cache_eviction_for_entry(&entry(action, "attrs")), None);
        }
        assert_eq!(
            cache_eviction_for_entry(&entry(WalAction::Insert, "idents")),
            None
        );
        assert_eq!(
            cache_eviction_for_entry(&entry(WalAction::Update, "transactions")),
            None
        );
    }

    #[test]
    fn stream_machine_update_decodes_the_jsonb_uuid() {
        let stream_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let machine_attr = system_attr_id("$streams", "machineId").unwrap();
        let mut triple = entry(WalAction::Update, "triples");
        triple.identity = vec![column("attr_id", json!(machine_attr))];
        triple.columns = vec![
            column("entity_id", json!(stream_id)),
            column("value", json!(json!(machine_id).to_string())),
        ];
        assert_eq!(
            cache_eviction_for_entry(&triple),
            Some(WalCacheEviction::StreamMachine {
                stream_id,
                machine_id,
            })
        );
    }
}
