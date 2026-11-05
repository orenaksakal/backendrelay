use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// PostgreSQL's unsigned 64-bit log sequence number.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Lsn(u64);

impl Lsn {
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:X}/{:X}",
            self.0 >> 32,
            self.0 & u64::from(u32::MAX)
        )
    }
}

impl FromStr for Lsn {
    type Err = LsnError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (high, low) = value
            .split_once('/')
            .ok_or_else(|| LsnError::InvalidFormat(value.to_owned()))?;
        if high.is_empty() || low.is_empty() || low.contains('/') {
            return Err(LsnError::InvalidFormat(value.to_owned()));
        }

        let high = u32::from_str_radix(high, 16)
            .map_err(|_| LsnError::InvalidComponent(value.to_owned()))?;
        let low = u32::from_str_radix(low, 16)
            .map_err(|_| LsnError::InvalidComponent(value.to_owned()))?;
        Ok(Self((u64::from(high) << 32) | u64::from(low)))
    }
}

impl Serialize for Lsn {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Lsn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum LsnError {
    #[error("invalid PostgreSQL LSN format: {0}")]
    InvalidFormat(String),
    #[error("invalid hexadecimal PostgreSQL LSN component: {0}")]
    InvalidComponent(String),
}

/// Instant's database-generation-aware sequence number.
///
/// Ordering is lexicographic by signed slot number and then PostgreSQL LSN, matching
/// the JVM `ISN.compareTo` implementation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Isn {
    slot_num: i32,
    lsn: Lsn,
}

impl Isn {
    pub const BYTE_LEN: usize = 12;

    #[must_use]
    pub const fn new(slot_num: i32, lsn: Lsn) -> Self {
        Self { slot_num, lsn }
    }

    #[must_use]
    pub const fn slot_num(self) -> i32 {
        self.slot_num
    }

    #[must_use]
    pub const fn lsn(self) -> Lsn {
        self.lsn
    }

    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0_u8; Self::BYTE_LEN];
        bytes[..4].copy_from_slice(&self.slot_num.to_be_bytes());
        bytes[4..].copy_from_slice(&self.lsn.as_u64().to_be_bytes());
        bytes
    }

    /// Decodes the JVM-compatible big-endian binary representation.
    ///
    /// # Errors
    ///
    /// Returns [`IsnError::InvalidByteLength`] unless `bytes` is exactly 12 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IsnError> {
        let bytes: &[u8; Self::BYTE_LEN] = bytes
            .try_into()
            .map_err(|_| IsnError::InvalidByteLength(bytes.len()))?;
        let [s0, s1, s2, s3, l0, l1, l2, l3, l4, l5, l6, l7] = *bytes;
        let slot_num = i32::from_be_bytes([s0, s1, s2, s3]);
        let lsn = u64::from_be_bytes([l0, l1, l2, l3, l4, l5, l6, l7]);
        Ok(Self::new(slot_num, Lsn::from_u64(lsn)))
    }

    /// Encodes the PostgreSQL composite input form for the `isn` type.
    #[must_use]
    pub fn to_postgres_composite(self) -> String {
        format!("({},{})", self.slot_num, self.lsn)
    }

    /// Decodes PostgreSQL's `(slot_num,lsn)` composite output form.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed composite, slot, or LSN values.
    pub fn from_postgres_composite(value: &str) -> Result<Self, IsnError> {
        let inner = value
            .strip_prefix('(')
            .and_then(|value| value.strip_suffix(')'))
            .ok_or_else(|| IsnError::InvalidFormat(value.to_owned()))?;
        let (slot_num, lsn) = inner
            .split_once(',')
            .ok_or_else(|| IsnError::InvalidFormat(value.to_owned()))?;
        if lsn.contains(',') {
            return Err(IsnError::InvalidFormat(value.to_owned()));
        }
        let slot_num = slot_num
            .parse::<i32>()
            .map_err(|_| IsnError::InvalidSlot(value.to_owned()))?;
        Ok(Self::new(slot_num, lsn.parse()?))
    }
}

impl fmt::Display for Isn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:X}/{}",
            self.slot_num.cast_unsigned(),
            self.lsn
        )
    }
}

impl FromStr for Isn {
    type Err = IsnError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (slot_num, lsn) = value
            .split_once('/')
            .ok_or_else(|| IsnError::InvalidFormat(value.to_owned()))?;
        if slot_num.is_empty() {
            return Err(IsnError::InvalidFormat(value.to_owned()));
        }
        let slot_num = u32::from_str_radix(slot_num, 16)
            .map_err(|_| IsnError::InvalidSlot(value.to_owned()))?
            .cast_signed();
        Ok(Self::new(slot_num, lsn.parse()?))
    }
}

impl Serialize for Isn {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Isn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum IsnError {
    #[error("invalid Instant sequence number format: {0}")]
    InvalidFormat(String),
    #[error("invalid Instant sequence number slot: {0}")]
    InvalidSlot(String),
    #[error(transparent)]
    InvalidLsn(#[from] LsnError),
    #[error("invalid Instant sequence number byte length: got {0}, expected 12")]
    InvalidByteLength(usize),
}

/// Thread-safe equivalent of the JVM invalidator's global max-seen ISN cell.
#[derive(Debug, Default)]
pub struct MaxSeenIsn(Mutex<Isn>);

impl MaxSeenIsn {
    #[must_use]
    pub fn get(&self) -> Isn {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn set(&self, isn: Isn) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = isn;
    }

    /// Advances the process watermark without allowing an out-of-order
    /// replication observer to move it backwards.
    pub fn advance(&self, isn: Isn) {
        let mut current = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = (*current).max(isn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_round_trips_postgres_syntax() {
        let lsn: Lsn = "2D4/71E9E720".parse().unwrap();
        assert_eq!(lsn.as_u64(), 0x0000_02D4_71E9_E720);
        assert_eq!(lsn.to_string(), "2D4/71E9E720");
        assert_eq!(serde_json::to_string(&lsn).unwrap(), r#""2D4/71E9E720""#);
        assert_eq!(
            serde_json::from_str::<Lsn>(r#""2D4/71E9E720""#).unwrap(),
            lsn
        );
    }

    #[test]
    fn isn_has_jvm_text_order_and_binary_contract() {
        let isn: Isn = "A/2D4/71E9E720".parse().unwrap();
        assert_eq!(isn.slot_num(), 10);
        assert_eq!(isn.to_string(), "A/2D4/71E9E720");
        assert_eq!(Isn::from_bytes(&isn.to_bytes()).unwrap(), isn);
        assert_eq!(isn.to_bytes().len(), 12);

        let later_slot = Isn::new(11, Lsn::from_u64(0));
        let later_lsn = Isn::new(10, Lsn::from_u64(isn.lsn().as_u64() + 1));
        assert!(later_slot > isn);
        assert!(later_lsn > isn);
        assert_eq!(isn.to_postgres_composite(), "(10,2D4/71E9E720)");
        assert_eq!(
            Isn::from_postgres_composite("(10,2D4/71E9E720)").unwrap(),
            isn
        );
    }

    #[test]
    fn max_seen_isn_advances_monotonically() {
        let max_seen = MaxSeenIsn::default();
        let newer = Isn::new(2, Lsn::from_u64(10));
        max_seen.advance(newer);
        max_seen.advance(Isn::new(1, Lsn::from_u64(999)));
        assert_eq!(max_seen.get(), newer);
    }

    #[test]
    fn isn_validates_binary_length() {
        assert_eq!(
            Isn::from_bytes(&[0_u8; 11]),
            Err(IsnError::InvalidByteLength(11))
        );
    }
}
