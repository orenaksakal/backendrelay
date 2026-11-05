use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

use crate::Lsn;

/// Normalized wal2json v2 action code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalAction {
    Begin,
    Insert,
    Update,
    Delete,
    Truncate,
    Message,
    Close,
    Unknown(String),
}

impl WalAction {
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "B" => Self::Begin,
            "I" => Self::Insert,
            "U" => Self::Update,
            "D" => Self::Delete,
            "T" => Self::Truncate,
            "M" => Self::Message,
            "C" => Self::Close,
            other => Self::Unknown(other.to_owned()),
        }
    }

    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Begin => "B",
            Self::Insert => "I",
            Self::Update => "U",
            Self::Delete => "D",
            Self::Truncate => "T",
            Self::Message => "M",
            Self::Close => "C",
            Self::Unknown(value) => value,
        }
    }
}

impl Serialize for WalAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for WalAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_wire(&String::deserialize(deserializer)?))
    }
}

/// A name/value pair from a wal2json `columns` or `identity` array.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WalColumn {
    pub name: String,
    pub value: Value,
}

/// One independently-delivered wal2json v2 entry.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WalEntry {
    pub action: WalAction,
    #[serde(default, skip_deserializing)]
    pub tx_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<WalColumn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity: Vec<WalColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsn: Option<Lsn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nextlsn: Option<Lsn>,
}

impl WalEntry {
    /// Parses one complete wal2json v2 record and records its exact byte size.
    ///
    /// # Errors
    ///
    /// Returns [`WalEntryError::Json`] if the bytes are not a valid entry.
    pub fn parse(bytes: &[u8]) -> Result<Self, WalEntryError> {
        let mut entry: Self = serde_json::from_slice(bytes)?;
        entry.tx_bytes = bytes.len();
        Ok(entry)
    }

    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Value> {
        self.columns
            .iter()
            .find(|column| column.name == name)
            .map(|column| &column.value)
    }

    #[must_use]
    pub fn identity_column(&self, name: &str) -> Option<&Value> {
        self.identity
            .iter()
            .find(|column| column.name == name)
            .map(|column| &column.value)
    }
}

#[derive(Debug, Error)]
pub enum WalEntryError {
    #[error("invalid wal2json v2 entry: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_each_action_and_preserves_unknown_codes() {
        for (wire, action) in [
            ("B", WalAction::Begin),
            ("I", WalAction::Insert),
            ("U", WalAction::Update),
            ("D", WalAction::Delete),
            ("T", WalAction::Truncate),
            ("M", WalAction::Message),
            ("C", WalAction::Close),
            ("future", WalAction::Unknown("future".to_owned())),
        ] {
            let json = format!(r#"{{"action":"{wire}"}}"#);
            assert_eq!(WalEntry::parse(json.as_bytes()).unwrap().action, action);
        }
    }

    #[test]
    fn parses_wal2json_v2_entry_and_uses_source_byte_length() {
        let json = br#"{"action":"I","schema":"public","table":"triples","columns":[{"name":"entity_id","type":"uuid","value":"7c6b379b-d841-46e1-8970-2da7e0cbc490"},{"name":"ea","type":"boolean","value":true}],"unknown":42}"#;
        let entry = WalEntry::parse(json).unwrap();

        assert_eq!(entry.action, WalAction::Insert);
        assert_eq!(entry.table.as_deref(), Some("triples"));
        assert_eq!(entry.tx_bytes, json.len());
        assert_eq!(
            entry.column("entity_id"),
            Some(&Value::String(
                "7c6b379b-d841-46e1-8970-2da7e0cbc490".to_owned()
            ))
        );
        assert_eq!(entry.column("ea"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parses_commit_lsns() {
        let entry =
            WalEntry::parse(br#"{"action":"C","lsn":"2D4/71E9E610","nextlsn":"2D4/71E9E720"}"#)
                .unwrap();

        assert_eq!(entry.lsn.unwrap().to_string(), "2D4/71E9E610");
        assert_eq!(entry.nextlsn.unwrap().to_string(), "2D4/71E9E720");
    }
}
