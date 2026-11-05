use std::io::{Cursor, Read};

use chrono::{DateTime, Utc};
use instant_wal::{Isn, Lsn, WalAction, WalColumn, WalEntry, WalRecord};
use serde_json::{Map, Number, Value};
use thiserror::Error;
use uuid::Uuid;

const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_COLLECTION_ITEMS: usize = 1_000_000;
const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug)]
enum NippyValue {
    Nil,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Keyword(String),
    Vector(Vec<Self>),
    Map(Vec<(Self, Self)>),
    Uuid(Uuid),
    Instant(DateTime<Utc>),
    Bytes(Vec<u8>),
    Lsn(Lsn),
    Isn(Isn),
    Column(WalColumn),
    Entry(WalEntry),
    Record(Box<WalRecord>),
}

#[derive(Debug, Error)]
pub enum LegacyNippyError {
    #[error("legacy WAL history zstd payload is invalid: {0}")]
    Compression(#[from] std::io::Error),
    #[error("legacy WAL history exceeds the {MAX_DECOMPRESSED_BYTES}-byte decoded limit")]
    TooLarge,
    #[error("legacy Nippy payload ended unexpectedly")]
    Truncated,
    #[error("legacy Nippy payload has an invalid {kind} length: {value}")]
    InvalidLength { kind: &'static str, value: i64 },
    #[error("legacy Nippy payload exceeds its nesting limit")]
    TooDeep,
    #[error("legacy Nippy payload contains unsupported type id {0}")]
    UnsupportedType(i8),
    #[error("legacy Nippy payload contains invalid UTF-8")]
    Utf8,
    #[error("legacy Nippy cache reference {0} has no value")]
    MissingCache(usize),
    #[error("legacy Nippy WAL record has an invalid {0}")]
    InvalidField(&'static str),
    #[error("legacy Nippy payload has trailing bytes")]
    TrailingBytes,
}

/// Decodes the exact `Zstd.compress(nippy/fast-freeze(wal-record))` payload
/// written by the JVM history service using Nippy 3.5.0.
///
/// # Errors
///
/// Returns a bounded codec error for malformed, oversized, truncated, or
/// unsupported legacy data.
pub fn decode_legacy_wal_record(payload: &[u8]) -> Result<WalRecord, LegacyNippyError> {
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(payload))?;
    let mut limited = decoder.take(MAX_DECOMPRESSED_BYTES + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(LegacyNippyError::TooLarge);
    }
    let mut decoder = Decoder::new(&bytes);
    let value = decoder.value(0)?;
    if decoder.position != bytes.len() {
        return Err(LegacyNippyError::TrailingBytes);
    }
    match value {
        NippyValue::Record(record) => Ok(*record),
        _ => Err(LegacyNippyError::InvalidField("root record")),
    }
}

struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
    cache: Vec<NippyValue>,
}

impl<'a> Decoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            position: 0,
            cache: Vec::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn value(&mut self, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        if depth > MAX_DEPTH {
            return Err(LegacyNippyError::TooDeep);
        }
        let type_id = self.i8()?;
        match type_id {
            -1 => Ok(NippyValue::Lsn(Lsn::from_u64(self.u64()?))),
            -2 => Ok(NippyValue::Isn(Isn::new(
                self.i32()?,
                Lsn::from_u64(self.u64()?),
            ))),
            -10 => self.column(depth + 1),
            -11 => self.entry(depth + 1),
            -12 => self.record(depth + 1),
            0 => Ok(NippyValue::Integer(0)),
            2 => self.bytes_i32("byte array").map(NippyValue::Bytes),
            3 => Ok(NippyValue::Nil),
            7 => self.bytes_i8("byte array").map(NippyValue::Bytes),
            8 => Ok(NippyValue::Bool(true)),
            9 => Ok(NippyValue::Bool(false)),
            13 => self.string_i32("string").map(NippyValue::String),
            15 => self.bytes_i16("byte array").map(NippyValue::Bytes),
            16 => self.string_i16("string").map(NippyValue::String),
            17 | 18 | 19 | 35 | 37 | 53 => Ok(match type_id {
                19 => NippyValue::Map(Vec::new()),
                53 => NippyValue::Bytes(Vec::new()),
                _ => NippyValue::Vector(Vec::new()),
            }),
            20 | 21 | 23 | 24 | 26 | 28 => {
                let count = self.count_i32("collection")?;
                self.vector(count, depth + 1)
            }
            30 | 31 => {
                let count = self.count_i32("map")?;
                self.map(count, depth + 1)
            }
            32 | 54 | 69 => {
                let count = self.count_i16("collection")?;
                self.vector(count, depth + 1)
            }
            33 => {
                let count = self.count_i16("map")?;
                self.map(count, depth + 1)
            }
            34 => Ok(NippyValue::String(String::new())),
            36 | 38 => {
                let count = self.count_i8("collection")?;
                self.vector(count, depth + 1)
            }
            40 | 100 => Ok(NippyValue::Integer(i64::from(self.i8()?))),
            41 | 101 => Ok(NippyValue::Integer(i64::from(self.i16()?))),
            42 | 102 => Ok(NippyValue::Integer(i64::from(self.i32()?))),
            43 => Ok(NippyValue::Integer(self.i64()?)),
            55 => Ok(NippyValue::Float(0.0)),
            56 => self.string_i8("symbol").map(NippyValue::String),
            59 | 63..=66 | 72..=74 => self.cached_fixed(type_id, depth + 1),
            60 => Ok(NippyValue::Float(f64::from(self.f32()?))),
            61 => Ok(NippyValue::Float(self.f64()?)),
            67 => {
                let index = self.count_i8("cache index")?;
                self.cached(index, depth + 1)
            }
            68 => {
                let index = self.count_i16("cache index")?;
                self.cached(index, depth + 1)
            }
            79 => {
                let seconds = self.i64()?;
                let nanos = self.i32()?;
                let nanos = u32::try_from(nanos)
                    .ok()
                    .filter(|value| *value < 1_000_000_000)
                    .ok_or(LegacyNippyError::InvalidField("instant"))?;
                let instant = DateTime::from_timestamp(seconds, nanos)
                    .ok_or(LegacyNippyError::InvalidField("instant"))?;
                Ok(NippyValue::Instant(instant))
            }
            85 => self.string_i16("keyword").map(NippyValue::Keyword),
            87 => Ok(NippyValue::Integer(
                i64::from(self.i8()?) - i64::from(i8::MIN),
            )),
            88 => Ok(NippyValue::Integer(
                i64::from(self.i16()?) - i64::from(i16::MIN),
            )),
            89 => Ok(NippyValue::Integer(
                i64::from(self.i32()?) - i64::from(i32::MIN),
            )),
            91 => Ok(NippyValue::Uuid(self.uuid()?)),
            93 => Ok(NippyValue::Integer(
                -(i64::from(self.i8()?) - i64::from(i8::MIN)),
            )),
            94 => Ok(NippyValue::Integer(
                -(i64::from(self.i16()?) - i64::from(i16::MIN)),
            )),
            95 => Ok(NippyValue::Integer(
                -(i64::from(self.i32()?) - i64::from(i32::MIN)),
            )),
            96 => {
                let count = usize::from(self.u8()?);
                self.string(count).map(NippyValue::String)
            }
            97 | 98 => {
                let count = usize::from(self.u8()?);
                self.vector(count, depth + 1)
            }
            99 => {
                let count = usize::from(self.u8()?);
                self.map(count, depth + 1)
            }
            105 => self.string_i8("string").map(NippyValue::String),
            106 => self.string_i8("keyword").map(NippyValue::Keyword),
            110..=112 => {
                let count = self.count_i8("collection")?;
                if type_id == 112 {
                    self.map(count, depth + 1)
                } else {
                    self.vector(count, depth + 1)
                }
            }
            113 => self.vector(2, depth + 1),
            114 => self.vector(3, depth + 1),
            other => Err(LegacyNippyError::UnsupportedType(other)),
        }
    }

    fn column(&mut self, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        let name = self.value(depth)?.string_field("column name")?;
        let value = self.value(depth)?.json()?;
        Ok(NippyValue::Column(WalColumn { name, value }))
    }

    fn entry(&mut self, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        let action = self.value(depth)?.action()?;
        let tx_bytes = self.value(depth)?.usize_field("entry byte count")?;
        let table = self.value(depth)?.optional_string("entry table")?;
        let columns = self.value(depth)?.columns("entry columns")?;
        let identity = self.value(depth)?.columns("entry identity")?;
        let prefix = self.value(depth)?.optional_string("entry prefix")?;
        let content = self.value(depth)?.optional_string("entry content")?;
        let lsn = self.value(depth)?.optional_lsn("entry lsn")?;
        let nextlsn = self.value(depth)?.optional_lsn("entry nextlsn")?;
        Ok(NippyValue::Entry(WalEntry {
            action,
            tx_bytes,
            table,
            columns,
            identity,
            prefix,
            content,
            lsn,
            nextlsn,
        }))
    }

    fn record(&mut self, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        // The custom JVM writer starts a fresh cache scope inside WalRecord.
        self.cache.clear();
        let app_id = self.uuid()?;
        let tx_id = self.value(depth)?.i64_field("transaction id")?;
        let isn = self.value(depth)?.isn("isn")?;
        let previous_isn = self.value(depth)?.optional_isn("previous isn")?;
        let tx_created_at = self.value(depth)?.optional_instant("creation time")?;
        let tx_bytes = self.value(depth)?.usize_field("transaction byte count")?;
        let next_lsn = self.value(depth)?.lsn("next lsn")?;
        let attr_changes = self.value(depth)?.entries("attribute changes")?;
        let ident_changes = self.value(depth)?.entries("ident changes")?;
        let triple_changes = self.value(depth)?.entries("triple changes")?;
        let messages = self.value(depth)?.entries("messages")?;
        let wal_logs = self.value(depth)?.entries("WAL logs")?;
        Ok(NippyValue::Record(Box::new(WalRecord {
            app_id,
            tx_id,
            isn,
            previous_isn,
            tx_created_at,
            tx_bytes,
            next_lsn,
            attr_changes,
            ident_changes,
            triple_changes,
            messages,
            wal_logs,
            combined_count: 1,
        })))
    }

    fn vector(&mut self, count: usize, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        self.validate_count("collection", count)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.value(depth)?);
        }
        Ok(NippyValue::Vector(values))
    }

    fn map(&mut self, count: usize, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        self.validate_count("map", count)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push((self.value(depth)?, self.value(depth)?));
        }
        Ok(NippyValue::Map(values))
    }

    fn cached_fixed(&mut self, type_id: i8, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        let index = match type_id {
            59 => 0,
            63 => 1,
            64 => 2,
            65 => 3,
            66 => 4,
            72 => 5,
            73 => 6,
            74 => 7,
            _ => unreachable!(),
        };
        self.cached(index, depth)
    }

    fn cached(&mut self, index: usize, depth: usize) -> Result<NippyValue, LegacyNippyError> {
        if let Some(value) = self.cache.get(index) {
            return Ok(value.clone());
        }
        if index != self.cache.len() {
            return Err(LegacyNippyError::MissingCache(index));
        }
        let value = self.value(depth)?;
        self.cache.push(value.clone());
        Ok(value)
    }

    fn uuid(&mut self) -> Result<Uuid, LegacyNippyError> {
        let bytes: [u8; 16] = self.take(16)?.try_into().expect("length checked");
        Ok(Uuid::from_bytes(bytes))
    }

    fn string(&mut self, count: usize) -> Result<String, LegacyNippyError> {
        self.validate_count("string", count)?;
        String::from_utf8(self.take(count)?.to_vec()).map_err(|_| LegacyNippyError::Utf8)
    }

    fn bytes(&mut self, count: usize) -> Result<Vec<u8>, LegacyNippyError> {
        self.validate_count("byte array", count)?;
        Ok(self.take(count)?.to_vec())
    }

    fn string_i8(&mut self, kind: &'static str) -> Result<String, LegacyNippyError> {
        let count = self.count_i8(kind)?;
        self.string(count)
    }

    fn string_i16(&mut self, kind: &'static str) -> Result<String, LegacyNippyError> {
        let count = self.count_i16(kind)?;
        self.string(count)
    }

    fn string_i32(&mut self, kind: &'static str) -> Result<String, LegacyNippyError> {
        let count = self.count_i32(kind)?;
        self.string(count)
    }

    fn bytes_i8(&mut self, kind: &'static str) -> Result<Vec<u8>, LegacyNippyError> {
        let count = self.count_i8(kind)?;
        self.bytes(count)
    }

    fn bytes_i16(&mut self, kind: &'static str) -> Result<Vec<u8>, LegacyNippyError> {
        let count = self.count_i16(kind)?;
        self.bytes(count)
    }

    fn bytes_i32(&mut self, kind: &'static str) -> Result<Vec<u8>, LegacyNippyError> {
        let count = self.count_i32(kind)?;
        self.bytes(count)
    }

    fn validate_count(&self, kind: &'static str, count: usize) -> Result<(), LegacyNippyError> {
        if count > MAX_COLLECTION_ITEMS || count > self.input.len().saturating_sub(self.position) {
            return Err(LegacyNippyError::InvalidLength {
                kind,
                value: i64::try_from(count).unwrap_or(i64::MAX),
            });
        }
        Ok(())
    }

    fn count_i8(&mut self, kind: &'static str) -> Result<usize, LegacyNippyError> {
        checked_count(kind, i64::from(self.i8()?))
    }

    fn count_i16(&mut self, kind: &'static str) -> Result<usize, LegacyNippyError> {
        checked_count(kind, i64::from(self.i16()?))
    }

    fn count_i32(&mut self, kind: &'static str) -> Result<usize, LegacyNippyError> {
        checked_count(kind, i64::from(self.i32()?))
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], LegacyNippyError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(LegacyNippyError::Truncated)?;
        let bytes = self
            .input
            .get(self.position..end)
            .ok_or(LegacyNippyError::Truncated)?;
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, LegacyNippyError> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8, LegacyNippyError> {
        Ok(i8::from_be_bytes([self.u8()?]))
    }

    fn i16(&mut self) -> Result<i16, LegacyNippyError> {
        Ok(i16::from_be_bytes(
            self.take(2)?.try_into().expect("length checked"),
        ))
    }

    fn i32(&mut self) -> Result<i32, LegacyNippyError> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("length checked"),
        ))
    }

    fn i64(&mut self) -> Result<i64, LegacyNippyError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        ))
    }

    fn u64(&mut self) -> Result<u64, LegacyNippyError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        ))
    }

    fn f32(&mut self) -> Result<f32, LegacyNippyError> {
        Ok(f32::from_bits(u32::from_be_bytes(
            self.take(4)?.try_into().expect("length checked"),
        )))
    }

    fn f64(&mut self) -> Result<f64, LegacyNippyError> {
        Ok(f64::from_bits(u64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        )))
    }
}

fn checked_count(kind: &'static str, value: i64) -> Result<usize, LegacyNippyError> {
    usize::try_from(value).map_err(|_| LegacyNippyError::InvalidLength { kind, value })
}

impl NippyValue {
    fn i64_field(self, field: &'static str) -> Result<i64, LegacyNippyError> {
        match self {
            Self::Integer(value) => Ok(value),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn usize_field(self, field: &'static str) -> Result<usize, LegacyNippyError> {
        usize::try_from(self.i64_field(field)?).map_err(|_| LegacyNippyError::InvalidField(field))
    }

    fn string_field(self, field: &'static str) -> Result<String, LegacyNippyError> {
        match self {
            Self::String(value) | Self::Keyword(value) => Ok(value),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn optional_string(self, field: &'static str) -> Result<Option<String>, LegacyNippyError> {
        match self {
            Self::Nil => Ok(None),
            value => value.string_field(field).map(Some),
        }
    }

    fn action(self) -> Result<WalAction, LegacyNippyError> {
        let value = self.string_field("entry action")?;
        Ok(match value.as_str() {
            "begin" => WalAction::Begin,
            "insert" => WalAction::Insert,
            "update" => WalAction::Update,
            "delete" => WalAction::Delete,
            "truncate" => WalAction::Truncate,
            "message" => WalAction::Message,
            "close" => WalAction::Close,
            other => WalAction::Unknown(other.to_owned()),
        })
    }

    fn lsn(self, field: &'static str) -> Result<Lsn, LegacyNippyError> {
        match self {
            Self::Lsn(value) => Ok(value),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn optional_lsn(self, field: &'static str) -> Result<Option<Lsn>, LegacyNippyError> {
        match self {
            Self::Nil => Ok(None),
            value => value.lsn(field).map(Some),
        }
    }

    fn isn(self, field: &'static str) -> Result<Isn, LegacyNippyError> {
        match self {
            Self::Isn(value) => Ok(value),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn optional_isn(self, field: &'static str) -> Result<Option<Isn>, LegacyNippyError> {
        match self {
            Self::Nil => Ok(None),
            value => value.isn(field).map(Some),
        }
    }

    fn optional_instant(
        self,
        field: &'static str,
    ) -> Result<Option<DateTime<Utc>>, LegacyNippyError> {
        match self {
            Self::Nil => Ok(None),
            Self::Instant(value) => Ok(Some(value)),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn columns(self, field: &'static str) -> Result<Vec<WalColumn>, LegacyNippyError> {
        match self {
            Self::Nil => Ok(Vec::new()),
            Self::Vector(values) => values
                .into_iter()
                .map(|value| match value {
                    Self::Column(column) => Ok(column),
                    _ => Err(LegacyNippyError::InvalidField(field)),
                })
                .collect(),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn entries(self, field: &'static str) -> Result<Vec<WalEntry>, LegacyNippyError> {
        match self {
            Self::Nil => Ok(Vec::new()),
            Self::Vector(values) => values
                .into_iter()
                .map(|value| match value {
                    Self::Entry(entry) => Ok(entry),
                    _ => Err(LegacyNippyError::InvalidField(field)),
                })
                .collect(),
            _ => Err(LegacyNippyError::InvalidField(field)),
        }
    }

    fn json(self) -> Result<Value, LegacyNippyError> {
        match self {
            Self::Nil => Ok(Value::Null),
            Self::Bool(value) => Ok(Value::Bool(value)),
            Self::Integer(value) => Ok(Value::Number(Number::from(value))),
            Self::Float(value) => Number::from_f64(value)
                .map(Value::Number)
                .ok_or(LegacyNippyError::InvalidField("JSON number")),
            Self::String(value) | Self::Keyword(value) => Ok(Value::String(value)),
            Self::Uuid(value) => Ok(Value::String(value.to_string())),
            Self::Bytes(value) => Ok(Value::Array(
                value
                    .into_iter()
                    .map(|byte| Value::Number(Number::from(byte)))
                    .collect(),
            )),
            Self::Vector(values) => values
                .into_iter()
                .map(Self::json)
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Self::Map(values) => {
                let mut map = Map::new();
                for (key, value) in values {
                    map.insert(key.string_field("JSON object key")?, value.json()?);
                }
                Ok(Value::Object(map))
            }
            _ => Err(LegacyNippyError::InvalidField("JSON value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_a_nippy_350_fixture_emitted_by_the_jvm_codec() {
        let payload = include_bytes!("../tests/fixtures/jvm-wal-record-nippy.zstd");
        let record = decode_legacy_wal_record(payload).unwrap();

        assert_eq!(
            record.app_id,
            Uuid::parse_str("12345678-1234-5678-9abc-def012345678").unwrap()
        );
        assert_eq!(record.tx_id, 2_147_483_647);
        assert_eq!(record.isn, Isn::new(17, Lsn::from_u64(0x2d4_0000_0010)));
        assert_eq!(
            record.previous_isn,
            Some(Isn::new(16, Lsn::from_u64(0x2d3_ffff_fff0)))
        );
        assert_eq!(
            record.tx_created_at,
            DateTime::from_timestamp(1_787_832_000, 123_456_789)
        );
        assert_eq!(record.tx_bytes, 536);
        assert_eq!(record.next_lsn, Lsn::from_u64(0x2d4_0000_0030));
        assert!(record.attr_changes.is_empty());
        assert!(record.ident_changes.is_empty());
        assert_eq!(record.triple_changes.len(), 1);
        assert_eq!(record.triple_changes[0].action, WalAction::Insert);
        assert_eq!(record.triple_changes[0].table.as_deref(), Some("triples"));
        assert_eq!(
            record.triple_changes[0].column("value"),
            Some(&json!({"nested": [1, true, 2.5, null]}))
        );
        assert_eq!(record.messages.len(), 1);
        assert_eq!(record.messages[0].action, WalAction::Message);
        assert_eq!(record.messages[0].prefix.as_deref(), Some("update_ents"));
        assert_eq!(record.wal_logs.len(), 1);
        assert_eq!(record.wal_logs[0].action, WalAction::Close);
        assert_eq!(record.wal_logs[0].lsn, Some(Lsn::from_u64(0x2d4_0000_0020)));
        assert_eq!(record.combined_count, 1);
    }

    #[test]
    fn rejects_non_zstd_and_trailing_nippy_data() {
        assert!(decode_legacy_wal_record(b"not zstd").is_err());
        let payload = include_bytes!("../tests/fixtures/jvm-wal-record-nippy.zstd");
        let mut decoded = zstd::stream::decode_all(Cursor::new(payload)).unwrap();
        decoded.push(3);
        let payload = zstd::stream::encode_all(Cursor::new(decoded), 3).unwrap();
        assert!(matches!(
            decode_legacy_wal_record(&payload),
            Err(LegacyNippyError::TrailingBytes)
        ));
    }
}
