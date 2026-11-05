use crate::common::traits::{Container, Indexer, Iterable, Sizer, Zeroer};
use crate::common::types::{CelBool, CelDouble, CelInt, CelString, CelUInt, Kind, Type};
use crate::common::value::Val;
use crate::common::{traits, types};
use crate::ExecutionError;
use crate::ExecutionError::NoSuchOverload;
use std::borrow::{Borrow, Cow};
use std::cmp::Ordering;
use std::collections::hash_map::Keys;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct DefaultMap(HashMap<Key, Box<dyn Val>>);

impl DefaultMap {
    pub fn into_inner(self) -> HashMap<Key, Box<dyn Val>> {
        self.0
    }

    pub fn inner(&self) -> &HashMap<Key, Box<dyn Val>> {
        &self.0
    }

    fn find_key(&self, key: &dyn Val) -> Result<Option<&Key>, ExecutionError> {
        let lookup = |key_ref: &dyn AsKeyRef| {
            self.0
                .get_key_value(key_ref)
                .map(|(stored_key, _)| stored_key)
        };

        if let Some(value) = key.downcast_ref::<CelString>() {
            return Ok(lookup(value));
        }
        if let Some(value) = key.downcast_ref::<CelBool>() {
            return Ok(lookup(value));
        }
        if let Some(value) = key.downcast_ref::<CelInt>() {
            if let Some(found) = lookup(value) {
                return Ok(Some(found));
            }
            return Ok(u64::try_from(*value.inner())
                .ok()
                .and_then(|value| lookup(&KeyRef::Uint(value))));
        }
        if let Some(value) = key.downcast_ref::<CelUInt>() {
            if let Some(found) = lookup(value) {
                return Ok(Some(found));
            }
            return Ok(i64::try_from(*value.inner())
                .ok()
                .and_then(|value| lookup(&KeyRef::Int(value))));
        }
        if let Some(value) = key.downcast_ref::<CelDouble>() {
            let value = *value.inner();
            if let Some(value) = double_to_i64_lossless(value) {
                if let Some(found) = lookup(&KeyRef::Int(value)) {
                    return Ok(Some(found));
                }
            }
            return Ok(double_to_u64_lossless(value).and_then(|value| lookup(&KeyRef::Uint(value))));
        }
        Err(NoSuchOverload)
    }

    fn find_value(&self, key: &dyn Val) -> Result<Option<&dyn Val>, ExecutionError> {
        self.find_key(key)
            .map(|key| key.map(|key| self.0[key].as_ref()))
    }
}

fn double_to_i64_lossless(value: f64) -> Option<i64> {
    const I64_MAX_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    if value < i64::MIN as f64 || value >= I64_MAX_EXCLUSIVE {
        return None;
    }
    let integer = value as i64;
    (integer as f64 == value).then_some(integer)
}

fn double_to_u64_lossless(value: f64) -> Option<u64> {
    // cel-java v0.11.1 retries through UnsignedLong.longValue(), whose high
    // bit is negative when promoted back to double for the lossless check.
    const SIGNED_LONG_MAX_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    if value < 0.0 || value >= SIGNED_LONG_MAX_EXCLUSIVE {
        return None;
    }
    let integer = value as u64;
    (integer as f64 == value).then_some(integer)
}

fn key_string(key: &dyn Val) -> Result<String, ExecutionError> {
    if let Some(value) = key.downcast_ref::<CelString>() {
        Ok(value.inner().to_owned())
    } else if let Some(value) = key.downcast_ref::<CelBool>() {
        Ok(value.inner().to_string())
    } else if let Some(value) = key.downcast_ref::<CelInt>() {
        Ok(value.inner().to_string())
    } else if let Some(value) = key.downcast_ref::<CelUInt>() {
        Ok(value.inner().to_string())
    } else if let Some(value) = key.downcast_ref::<CelDouble>() {
        Ok(value.inner().to_string())
    } else {
        Err(NoSuchOverload)
    }
}

impl Deref for DefaultMap {
    type Target = HashMap<Key, Box<dyn Val>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Val for DefaultMap {
    fn get_type(&self) -> &Type {
        &types::MAP_TYPE
    }

    fn as_container(&self) -> Option<&dyn Container> {
        Some(self)
    }

    fn as_indexer(&self) -> Option<&dyn Indexer> {
        Some(self)
    }

    fn into_indexer(self: Box<Self>) -> Option<Box<dyn Indexer>> {
        Some(self)
    }

    fn as_iterable(&self) -> Option<&dyn Iterable> {
        Some(self)
    }

    fn as_sizer(&self) -> Option<&dyn Sizer> {
        Some(self)
    }

    fn as_zeroer(&self) -> Option<&dyn Zeroer> {
        Some(self)
    }

    fn equals(&self, other: &dyn Val) -> bool {
        let Some(other) = other.downcast_ref::<Self>() else {
            return false;
        };
        self.0.len() == other.0.len()
            && self.0.iter().all(|(key, value)| {
                other
                    .find_value(key.inner())
                    .ok()
                    .flatten()
                    .is_some_and(|other_value| value.equals(other_value))
            })
    }

    fn clone_as_boxed(&self) -> Box<dyn Val> {
        let mut map = HashMap::with_capacity(self.0.len());
        for (k, v) in self.0.iter() {
            map.insert(k.clone(), v.clone_as_boxed());
        }
        Box::new(Self(map))
    }
}

impl Container for DefaultMap {
    fn contains(&self, key: &dyn Val) -> Result<bool, ExecutionError> {
        self.find_key(key).map(|key| key.is_some())
    }
}

impl Indexer for DefaultMap {
    fn get<'a>(&'a self, key: &dyn Val) -> Result<Cow<'a, dyn Val>, ExecutionError> {
        let value = self.find_value(key)?;
        let key = key_string(key)?;
        value
            .map(Cow::Borrowed)
            .ok_or_else(|| ExecutionError::NoSuchKey(Arc::new(key)))
    }

    fn steal(self: Box<Self>, key: &dyn Val) -> Result<Box<dyn Val>, ExecutionError> {
        let mut map = self;
        let resolved = map.find_key(key)?.cloned();
        let key = key_string(key)?;
        resolved
            .and_then(|resolved| map.0.remove(&resolved))
            .ok_or_else(|| ExecutionError::NoSuchKey(Arc::new(key)))
    }
}

impl Iterable for DefaultMap {
    fn iter<'a>(&'a self) -> Box<dyn super::traits::Iterator<'a> + 'a> {
        Box::new(MapKeyIterator::new(self.0.keys()))
    }
}

impl Sizer for DefaultMap {
    fn size(&self) -> CelInt {
        (self.inner().len() as i64).into()
    }
}

impl Zeroer for DefaultMap {
    fn is_zero_value(&self) -> bool {
        self.inner().is_empty()
    }
}

impl From<HashMap<Key, Box<dyn Val>>> for DefaultMap {
    fn from(value: HashMap<Key, Box<dyn Val>>) -> Self {
        Self(value)
    }
}

#[derive(Debug, Eq, Clone)]
pub enum Key {
    Bool(CelBool),
    Int(CelInt),
    String(CelString),
    UInt(CelUInt),
}

impl Hash for Key {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_keyref().hash(state);
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_keyref() == other.as_keyref()
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_keyref().cmp(&other.as_keyref())
    }
}

impl Key {
    pub fn inner(&self) -> &dyn Val {
        match self {
            Key::Bool(b) => b,
            Key::Int(i) => i,
            Key::String(s) => s,
            Key::UInt(u) => u,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum KeyRef<'a> {
    Int(i64),
    Uint(u64),
    Bool(bool),
    String(&'a str),
}

/// Trait for converting to a borrowed [`KeyRef`] for efficient lookups.
pub trait AsKeyRef {
    fn as_keyref(&self) -> KeyRef<'_>;
}

impl AsKeyRef for Key {
    fn as_keyref(&self) -> KeyRef<'_> {
        match self {
            Key::Int(i) => KeyRef::Int(*i.inner()),
            Key::UInt(u) => KeyRef::Uint(*u.inner()),
            Key::Bool(b) => KeyRef::Bool(*b.inner()),
            Key::String(s) => KeyRef::String(s.inner()),
        }
    }
}

impl AsKeyRef for CelString {
    fn as_keyref(&self) -> KeyRef<'_> {
        KeyRef::String(self.inner())
    }
}

impl AsKeyRef for CelInt {
    fn as_keyref(&self) -> KeyRef<'_> {
        KeyRef::Int(*self.inner())
    }
}

impl AsKeyRef for CelUInt {
    fn as_keyref(&self) -> KeyRef<'_> {
        KeyRef::Uint(*self.inner())
    }
}

impl AsKeyRef for CelBool {
    fn as_keyref(&self) -> KeyRef<'_> {
        KeyRef::Bool(*self.inner())
    }
}

impl<'a> AsKeyRef for KeyRef<'a> {
    fn as_keyref(&self) -> KeyRef<'a> {
        *self
    }
}

/// Trait object implementations for `dyn AsKeyRef` to enable hashing and comparison.
impl<'a> PartialEq for dyn AsKeyRef + 'a {
    fn eq(&self, other: &Self) -> bool {
        self.as_keyref().eq(&other.as_keyref())
    }
}

impl<'a> Eq for dyn AsKeyRef + 'a {}

impl<'a> Hash for dyn AsKeyRef + 'a {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_keyref().hash(state)
    }
}

impl<'a> PartialOrd for dyn AsKeyRef + 'a {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for dyn AsKeyRef + 'a {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_keyref().cmp(&other.as_keyref())
    }
}

/// Implement `Borrow<dyn AsKeyRef>` for `Key` to enable efficient lookups.
impl<'a> Borrow<dyn AsKeyRef + 'a> for Key {
    fn borrow(&self) -> &(dyn AsKeyRef + 'a) {
        self
    }
}

impl From<bool> for Key {
    fn from(value: bool) -> Self {
        Key::Bool(value.into())
    }
}

impl From<i64> for Key {
    fn from(value: i64) -> Self {
        Key::Int(value.into())
    }
}

impl From<String> for Key {
    fn from(value: String) -> Self {
        Key::String(value.into())
    }
}

impl From<&str> for Key {
    fn from(value: &str) -> Self {
        Key::String(value.into())
    }
}

impl From<u64> for Key {
    fn from(value: u64) -> Self {
        Key::UInt(value.into())
    }
}

impl TryFrom<Box<dyn Val>> for Key {
    type Error = ExecutionError;

    fn try_from(value: Box<dyn Val>) -> Result<Self, Self::Error> {
        let key = match value.get_type().kind() {
            Kind::Boolean => value
                .downcast_ref::<CelBool>()
                .copied()
                .map(Key::Bool)
                .ok_or_else(|| {
                    ExecutionError::UnsupportedKeyType(
                        value.as_ref().try_into().expect("Can't convert key!"),
                    )
                })?,
            Kind::Int => value
                .downcast_ref::<CelInt>()
                .copied()
                .map(Key::Int)
                .ok_or_else(|| {
                    ExecutionError::UnsupportedKeyType(
                        value.as_ref().try_into().expect("Can't convert key!"),
                    )
                })?,
            Kind::String => {
                let s = super::cast_boxed::<CelString>(value).map_err(|v| {
                    ExecutionError::UnsupportedKeyType(
                        v.as_ref().try_into().expect("Can't convert key!"),
                    )
                })?;
                Key::String(s.into_inner().into())
            }
            Kind::UInt => value
                .downcast_ref::<CelUInt>()
                .copied()
                .map(Key::UInt)
                .ok_or_else(|| {
                    ExecutionError::UnsupportedKeyType(
                        value.as_ref().try_into().expect("Can't convert key!"),
                    )
                })?,
            _ => {
                return Err(ExecutionError::UnsupportedKeyType(
                    value.as_ref().try_into().expect("Can't convert key!"),
                ))
            }
        };
        Ok(key)
    }
}

pub struct MapKeyIterator<'a> {
    keys: Keys<'a, Key, Box<dyn Val>>,
}

impl<'a> MapKeyIterator<'a> {
    fn new(keys: Keys<'a, Key, Box<dyn Val>>) -> Self {
        Self { keys }
    }
}

impl<'a> traits::Iterator<'a> for MapKeyIterator<'a> {
    fn next(&mut self) -> Option<&'a dyn Val> {
        self.keys.next().map(|k| k.inner())
    }
}

pub(crate) fn stdlib(env: &mut crate::Env) {
    env.add_overload(
        "size",
        "size_map",
        vec![super::MAP_TYPE],
        traits::adapter::sizer_size,
    )
    .expect("Must be unique id");
    env.add_member_overload(
        "size",
        "map_size",
        super::MAP_TYPE,
        vec![],
        traits::adapter::sizer_size,
    )
    .expect("Must be unique id");
}

#[cfg(test)]
mod tests {
    use super::{DefaultMap, Key};
    use crate::common::traits::{Container, Indexer};
    use crate::common::types::{CelDouble, CelInt, CelList, CelString, CelUInt};
    use crate::common::value::Val;
    use crate::ExecutionError;
    use std::collections::HashMap;

    fn map(entries: impl IntoIterator<Item = (Key, i64)>) -> DefaultMap {
        entries
            .into_iter()
            .map(|(key, value)| (key, Box::new(CelInt::from(value)) as Box<dyn Val>))
            .collect::<HashMap<_, _>>()
            .into()
    }

    fn assert_int(value: &dyn Val, expected: i64) {
        assert_eq!(
            value.downcast_ref::<CelInt>().map(CelInt::inner),
            Some(&expected)
        );
    }

    #[test]
    fn indexes_negative_integral_double_as_int() {
        let map = map([(Key::from(-42_i64), 7)]);
        let key = CelDouble::from(-42.0);

        assert_eq!(map.contains(&key), Ok(true));
        assert_int(map.get(&key).unwrap().as_ref(), 7);
    }

    #[test]
    fn indexes_both_signed_double_zeroes_as_int_zero() {
        let map = map([(Key::from(0_i64), 11)]);

        for key in [CelDouble::from(0.0), CelDouble::from(-0.0)] {
            assert_eq!(map.contains(&key), Ok(true));
            assert_int(map.get(&key).unwrap().as_ref(), 11);
        }
    }

    #[test]
    fn indexes_integer_boundaries_without_lossy_double_coercion() {
        let map = map([
            (Key::from(i64::MIN), 1),
            (Key::from(i64::MAX), 2),
            (Key::from(u64::MAX), 3),
        ]);

        assert_int(
            map.get(&CelDouble::from(i64::MIN as f64)).unwrap().as_ref(),
            1,
        );
        assert_int(
            map.get(&CelUInt::from(i64::MAX as u64)).unwrap().as_ref(),
            2,
        );
        assert_int(map.get(&CelInt::from(i64::MAX)).unwrap().as_ref(), 2);
        assert_int(map.get(&CelUInt::from(u64::MAX)).unwrap().as_ref(), 3);

        // `i64::MAX as f64` rounds to 2^63 and must not alias the max-int key.
        assert_eq!(map.contains(&CelDouble::from(i64::MAX as f64)), Ok(false));
        // cel-java does not retry double lookup through uint values with the
        // high bit set, so this must not alias the max-uint key either.
        assert_eq!(map.contains(&CelDouble::from(u64::MAX as f64)), Ok(false));
    }

    #[test]
    fn rejects_fractional_and_non_finite_double_keys() {
        let map = map([(Key::from(1_i64), 1)]);

        for key in [
            CelDouble::from(1.5),
            CelDouble::from(f64::NAN),
            CelDouble::from(f64::INFINITY),
            CelDouble::from(f64::NEG_INFINITY),
        ] {
            assert_eq!(map.contains(&key), Ok(false));
            assert!(matches!(map.get(&key), Err(ExecutionError::NoSuchKey(_))));
        }
    }

    #[test]
    fn distinguishes_missing_supported_keys_from_unsupported_index_types() {
        let map = map([(Key::from("present"), 1)]);

        assert!(matches!(
            map.get(&CelString::from("missing")),
            Err(ExecutionError::NoSuchKey(_))
        ));
        let unsupported = CelList::from(vec![Box::new(CelInt::from(1)) as Box<dyn Val>]);
        assert!(matches!(
            map.get(&unsupported),
            Err(ExecutionError::NoSuchOverload)
        ));
    }
}
