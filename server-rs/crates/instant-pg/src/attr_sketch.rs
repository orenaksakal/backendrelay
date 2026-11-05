//! Native attribute count-min sketches compatible with rows written by the legacy server.
//!
//! Newer rows contain big-endian `JavaFastPFOR` `LongCompressor` words inside a zstd
//! frame. Older rows contain the uncompressed big-endian bins inside zstd. Keeping
//! this implementation here lets a Rust-only deployment read either representation and
//! persist the stable legacy representation without involving a JVM.

use thiserror::Error;
use xxhash_rust::xxh3::xxh3_64_with_seed;

const VALUES_PER_BLOCK: usize = 64;
const BLOCKS_PER_GROUP: usize = 8;
pub const DEFAULT_ATTR_SKETCH_WIDTH: usize = 20_000;
pub const DEFAULT_ATTR_SKETCH_DEPTH: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AttrSketchInput<'a> {
    Nil,
    Long(i64),
    Double(f64),
    String(&'a str),
    Boolean(bool),
    BigInteger(&'a [u8]),
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A mutable owned sketch by default, or an immutable planner view when `B`
/// supplies shared bins. Only the owned representation supports WAL mutation.
pub struct AttrSketch<B = Vec<u64>> {
    pub width: usize,
    pub depth: usize,
    pub bins: B,
    pub total: u64,
    pub total_not_binned: u64,
}

impl AttrSketch {
    /// Creates the production-sized empty sketch used by `InstantDB`.
    #[must_use]
    pub fn empty_default() -> Self {
        Self {
            width: DEFAULT_ATTR_SKETCH_WIDTH,
            depth: DEFAULT_ATTR_SKETCH_DEPTH,
            bins: vec![0; DEFAULT_ATTR_SKETCH_WIDTH * DEFAULT_ATTR_SKETCH_DEPTH],
            total: 0,
            total_not_binned: 0,
        }
    }

    /// Creates an empty sketch with explicit dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`AttrSketchMutationError::DimensionsOverflow`] when the bin count
    /// cannot be represented by `usize`.
    pub fn empty(width: usize, depth: usize) -> Result<Self, AttrSketchMutationError> {
        let bin_count = width
            .checked_mul(depth)
            .ok_or(AttrSketchMutationError::DimensionsOverflow)?;
        Ok(Self {
            width,
            depth,
            bins: vec![0; bin_count],
            total: 0,
            total_not_binned: 0,
        })
    }

    /// Applies a signed WAL delta. `None` represents a JSON value that cannot be
    /// queried (objects and arrays): it contributes to both totals but not bins.
    ///
    /// # Errors
    ///
    /// Returns [`AttrSketchMutationError`] when dimensions are inconsistent or a
    /// count would overflow/underflow. The sketch is unchanged on error.
    pub fn apply_delta(
        &mut self,
        input: Option<AttrSketchInput<'_>>,
        delta: i64,
    ) -> Result<(), AttrSketchMutationError> {
        if self.width == 0
            || self.depth == 0
            || self.width.checked_mul(self.depth) != Some(self.bins.len())
        {
            return Err(AttrSketchMutationError::InvalidDimensions);
        }
        let total = apply_signed(self.total, delta).ok_or(AttrSketchMutationError::TotalRange)?;
        let total_not_binned = if input.is_none() {
            apply_signed(self.total_not_binned, delta)
                .ok_or(AttrSketchMutationError::UnbinnedRange)?
        } else {
            self.total_not_binned
        };

        let bin_indexes = input.map(|input| {
            let initial_seed = hash_input(u64::MAX, input);
            (0..self.depth)
                .map(|row| {
                    let hash = hash_input(initial_seed.wrapping_add(row as u64), input);
                    let column = usize::try_from(hash % self.width as u64).unwrap_or_default();
                    row * self.width + column
                })
                .collect::<Vec<_>>()
        });
        if let Some(bin_indexes) = &bin_indexes
            && bin_indexes
                .iter()
                .any(|index| apply_signed(self.bins[*index], delta).is_none())
        {
            return Err(AttrSketchMutationError::BinRange);
        }

        self.total = total;
        self.total_not_binned = total_not_binned;
        if let Some(bin_indexes) = bin_indexes {
            for index in bin_indexes {
                self.bins[index] = if delta >= 0 {
                    self.bins[index].wrapping_add(delta.unsigned_abs())
                } else {
                    self.bins[index].wrapping_sub(delta.unsigned_abs())
                };
            }
        }
        Ok(())
    }
}

// Planner snapshots may share immutable decoded bins; mutable WAL sketches
// retain the default owned Vec and the same mutation/encoding API.
impl<B: AsRef<[u64]>> AttrSketch<B> {
    /// Estimates how many binned values equal `input` using the same seeded XXH3
    /// sequence as the JVM count-min sketch.
    #[must_use]
    pub fn estimate(&self, input: AttrSketchInput<'_>) -> u64 {
        let bins = self.bins.as_ref();
        if self.width == 0
            || self.depth == 0
            || self.width.checked_mul(self.depth) != Some(bins.len())
        {
            return 0;
        }
        let initial_seed = hash_input(u64::MAX, input);
        (0..self.depth)
            .map(|row| {
                let hash = hash_input(initial_seed.wrapping_add(row as u64), input);
                let column = usize::try_from(hash % self.width as u64).unwrap_or_default();
                bins[row * self.width + column]
            })
            .min()
            .unwrap_or(0)
    }
}

fn apply_signed(value: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        value.checked_add(delta.unsigned_abs())
    } else {
        value.checked_sub(delta.unsigned_abs())
    }
}

fn hash_input(seed: u64, input: AttrSketchInput<'_>) -> u64 {
    match input {
        AttrSketchInput::Nil => xxh3_64_with_seed(&0_u16.to_le_bytes(), seed),
        AttrSketchInput::Long(value) => xxh3_64_with_seed(&value.to_le_bytes(), seed),
        AttrSketchInput::Double(value) => xxh3_64_with_seed(&value.to_bits().to_le_bytes(), seed),
        AttrSketchInput::String(value) => {
            let utf16_le = value
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            xxh3_64_with_seed(&utf16_le, seed)
        }
        AttrSketchInput::Boolean(value) => xxh3_64_with_seed(&[u8::from(value)], seed),
        AttrSketchInput::BigInteger(twos_complement_big_endian) => {
            xxh3_64_with_seed(twos_complement_big_endian, seed)
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttrSketchDecodeError {
    #[error("attribute sketch dimensions overflow")]
    DimensionsOverflow,
    #[error("attribute sketch zstd payload is invalid: {0}")]
    Zstd(String),
    #[error("attribute sketch payload length {0} is not a multiple of 8 bytes")]
    InvalidWordBytes(usize),
    #[error("attribute sketch payload is empty")]
    Empty,
    #[error("attribute sketch declares {declared} bins, expected {expected}")]
    WrongBinCount { declared: u64, expected: usize },
    #[error("attribute sketch compressed payload ended unexpectedly")]
    Truncated,
    #[error("attribute sketch bit width {0} is greater than 64")]
    InvalidBitWidth(u64),
    #[error("attribute sketch variable-byte value is malformed")]
    InvalidVariableByte,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttrSketchMutationError {
    #[error("attribute sketch dimensions overflow")]
    DimensionsOverflow,
    #[error("attribute sketch dimensions do not match its bins")]
    InvalidDimensions,
    #[error("attribute sketch total would overflow or underflow")]
    TotalRange,
    #[error("attribute sketch unbinned total would overflow or underflow")]
    UnbinnedRange,
    #[error("attribute sketch bin would overflow or underflow")]
    BinRange,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttrSketchEncodeError {
    #[error("attribute sketch dimensions overflow")]
    DimensionsOverflow,
    #[error("attribute sketch declares {actual} bins, expected {expected}")]
    WrongBinCount { actual: usize, expected: usize },
    #[error("attribute sketch zstd compression failed: {0}")]
    Zstd(String),
}

/// Encodes bins in the stable raw big-endian representation understood by both
/// the JVM and Rust readers, wrapped in zstd.
///
/// # Errors
///
/// Returns [`AttrSketchEncodeError`] for inconsistent dimensions or zstd errors.
pub fn encode_attr_sketch_bins(sketch: &AttrSketch) -> Result<Vec<u8>, AttrSketchEncodeError> {
    let expected = sketch
        .width
        .checked_mul(sketch.depth)
        .ok_or(AttrSketchEncodeError::DimensionsOverflow)?;
    if sketch.bins.len() != expected {
        return Err(AttrSketchEncodeError::WrongBinCount {
            actual: sketch.bins.len(),
            expected,
        });
    }
    let bytes = sketch
        .bins
        .iter()
        .flat_map(|bin| bin.to_be_bytes())
        .collect::<Vec<_>>();
    zstd::bulk::compress(&bytes, 1).map_err(|error| AttrSketchEncodeError::Zstd(error.to_string()))
}

/// Decodes an `attr_sketches.bins` value produced by either Instant's old or
/// current Clojure implementation.
///
/// # Errors
///
/// Returns [`AttrSketchDecodeError`] for invalid dimensions, zstd data, word
/// alignment, compressed lengths, bit widths, or variable-byte values.
pub fn decode_attr_sketch_bins(
    width: usize,
    depth: usize,
    compressed: &[u8],
) -> Result<Vec<u64>, AttrSketchDecodeError> {
    let expected = width
        .checked_mul(depth)
        .ok_or(AttrSketchDecodeError::DimensionsOverflow)?;
    let packed_count = expected / VALUES_PER_BLOCK * VALUES_PER_BLOCK;
    let block_count = packed_count / VALUES_PER_BLOCK;
    let header_words = block_count / BLOCKS_PER_GROUP + block_count % BLOCKS_PER_GROUP;
    let tail_bytes = (expected - packed_count)
        .checked_mul(10)
        .ok_or(AttrSketchDecodeError::DimensionsOverflow)?;
    let tail_words = tail_bytes
        .checked_add(7)
        .ok_or(AttrSketchDecodeError::DimensionsOverflow)?
        / 8;
    let pfor_words = 1_usize
        .checked_add(header_words)
        .and_then(|value| value.checked_add(packed_count))
        .and_then(|value| value.checked_add(usize::from(packed_count == 0)))
        .and_then(|value| value.checked_add(tail_words))
        .ok_or(AttrSketchDecodeError::DimensionsOverflow)?;
    let max_words = expected.max(pfor_words);
    let max_bytes = max_words
        .checked_mul(size_of::<u64>())
        .ok_or(AttrSketchDecodeError::DimensionsOverflow)?;
    let bytes = zstd::bulk::decompress(compressed, max_bytes)
        .map_err(|error| AttrSketchDecodeError::Zstd(error.to_string()))?;
    if bytes.len() % size_of::<u64>() != 0 {
        return Err(AttrSketchDecodeError::InvalidWordBytes(bytes.len()));
    }
    let (word_bytes, remainder) = bytes.as_chunks::<8>();
    debug_assert!(remainder.is_empty());
    let words = word_bytes
        .iter()
        .map(|chunk| u64::from_be_bytes(*chunk))
        .collect::<Vec<_>>();
    if words.len() == expected {
        return Ok(words);
    }
    decode_long_compressor(&words, expected)
}

fn decode_long_compressor(
    words: &[u64],
    expected: usize,
) -> Result<Vec<u64>, AttrSketchDecodeError> {
    let (&declared, payload) = words.split_first().ok_or(AttrSketchDecodeError::Empty)?;
    if usize::try_from(declared).ok() != Some(expected) {
        return Err(AttrSketchDecodeError::WrongBinCount { declared, expected });
    }

    let packed_count = expected / VALUES_PER_BLOCK * VALUES_PER_BLOCK;
    let block_count = packed_count / VALUES_PER_BLOCK;
    let mut input = 0;
    let mut output = Vec::with_capacity(expected);

    let grouped_blocks = block_count / BLOCKS_PER_GROUP * BLOCKS_PER_GROUP;
    for _ in (0..grouped_blocks).step_by(BLOCKS_PER_GROUP) {
        let header = *payload.get(input).ok_or(AttrSketchDecodeError::Truncated)?;
        input += 1;
        for header_shift in (0..BLOCKS_PER_GROUP).rev().map(|index| index * 8) {
            let bit_width = (header >> header_shift) & 0xff;
            unpack_block(payload, &mut input, bit_width, &mut output)?;
        }
    }
    for _ in grouped_blocks..block_count {
        let bit_width = *payload.get(input).ok_or(AttrSketchDecodeError::Truncated)?;
        input += 1;
        unpack_block(payload, &mut input, bit_width, &mut output)?;
    }

    // A composition with no bit-packed blocks writes one zero marker word.
    if packed_count == 0 {
        if payload.get(input) != Some(&0) {
            return Err(AttrSketchDecodeError::Truncated);
        }
        input += 1;
    }
    decode_variable_byte_tail(payload, input, expected - packed_count, &mut output)?;
    if output.len() != expected {
        return Err(AttrSketchDecodeError::Truncated);
    }
    Ok(output)
}

fn unpack_block(
    input: &[u64],
    input_position: &mut usize,
    bit_width: u64,
    output: &mut Vec<u64>,
) -> Result<(), AttrSketchDecodeError> {
    let bit_width = usize::try_from(bit_width)
        .map_err(|_| AttrSketchDecodeError::InvalidBitWidth(bit_width))?;
    if bit_width > u64::BITS as usize {
        return Err(AttrSketchDecodeError::InvalidBitWidth(bit_width as u64));
    }
    if bit_width == 0 {
        output.resize(output.len() + VALUES_PER_BLOCK, 0);
        return Ok(());
    }
    let end = input_position
        .checked_add(bit_width)
        .ok_or(AttrSketchDecodeError::Truncated)?;
    let packed = input
        .get(*input_position..end)
        .ok_or(AttrSketchDecodeError::Truncated)?;
    if bit_width == u64::BITS as usize {
        output.extend_from_slice(packed);
        *input_position = end;
        return Ok(());
    }

    let mask = (1_u64 << bit_width) - 1;
    for value_index in 0..VALUES_PER_BLOCK {
        let bit_position = value_index * bit_width;
        let word_index = bit_position / u64::BITS as usize;
        let shift = bit_position % u64::BITS as usize;
        let mut value = packed[word_index] >> shift;
        if shift + bit_width > u64::BITS as usize {
            value |= packed[word_index + 1] << (u64::BITS as usize - shift);
        }
        output.push(value & mask);
    }
    *input_position = end;
    Ok(())
}

fn decode_variable_byte_tail(
    input: &[u64],
    input_position: usize,
    count: usize,
    output: &mut Vec<u64>,
) -> Result<(), AttrSketchDecodeError> {
    let mut bytes = input[input_position..]
        .iter()
        .flat_map(|word| word.to_le_bytes());
    for _ in 0..count {
        let mut value = 0_u64;
        for byte_index in 0..10 {
            let byte = bytes.next().ok_or(AttrSketchDecodeError::Truncated)?;
            let payload = u64::from(byte & 0x7f);
            if byte_index == 9 && payload > 1 {
                return Err(AttrSketchDecodeError::InvalidVariableByte);
            }
            value |= payload << (byte_index * 7);
            if byte & 0x80 != 0 {
                output.push(value);
                break;
            }
            if byte_index == 9 {
                return Err(AttrSketchDecodeError::InvalidVariableByte);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zstd_words(words: &[u64]) -> Vec<u8> {
        let bytes = words
            .iter()
            .flat_map(|word| word.to_be_bytes())
            .collect::<Vec<_>>();
        zstd::bulk::compress(&bytes, 1).expect("compress fixture")
    }

    #[test]
    fn decodes_legacy_zstd_only_bins() {
        let bins = [0, 1, u64::MAX, 42, 9, 8];
        assert_eq!(
            decode_attr_sketch_bins(3, 2, &zstd_words(&bins)),
            Ok(bins.to_vec())
        );
    }

    #[test]
    fn empty_default_matches_the_jvm_production_dimensions() {
        let sketch = AttrSketch::empty_default();
        assert_eq!(sketch.width, 20_000);
        assert_eq!(sketch.depth, 10);
        assert_eq!(sketch.bins.len(), 200_000);
    }

    #[test]
    fn applies_insert_delete_and_unbinned_deltas() {
        let mut sketch = AttrSketch::empty(257, 4).unwrap();
        sketch
            .apply_delta(Some(AttrSketchInput::String("value")), 2)
            .unwrap();
        sketch.apply_delta(None, 3).unwrap();
        assert_eq!(sketch.total, 5);
        assert_eq!(sketch.total_not_binned, 3);
        assert_eq!(sketch.estimate(AttrSketchInput::String("value")), 2);

        sketch
            .apply_delta(Some(AttrSketchInput::String("value")), -1)
            .unwrap();
        sketch.apply_delta(None, -2).unwrap();
        assert_eq!(sketch.total, 2);
        assert_eq!(sketch.total_not_binned, 1);
        assert_eq!(sketch.estimate(AttrSketchInput::String("value")), 1);
    }

    #[test]
    fn rejects_underflow_without_partially_mutating() {
        let mut sketch = AttrSketch::empty(8, 2).unwrap();
        let before = sketch.clone();
        assert_eq!(
            sketch.apply_delta(Some(AttrSketchInput::Boolean(true)), -1),
            Err(AttrSketchMutationError::TotalRange)
        );
        assert_eq!(sketch, before);
    }

    #[test]
    fn encoded_bins_round_trip_through_the_legacy_format() {
        let mut sketch = AttrSketch::empty(257, 2).unwrap();
        sketch
            .apply_delta(Some(AttrSketchInput::Long(42)), 3)
            .unwrap();
        let encoded = encode_attr_sketch_bins(&sketch).unwrap();
        assert_eq!(
            decode_attr_sketch_bins(sketch.width, sketch.depth, &encoded),
            Ok(sketch.bins)
        );
    }

    #[test]
    fn decodes_bit_packed_long_compressor_block() {
        // JavaFastPFOR layout: original length, one bit-width header, then one
        // packed word for 64 alternating zero/one values.
        let words = [64, 1, 0xaaaa_aaaa_aaaa_aaaa];
        let expected = (0..64).map(|index| index % 2).collect::<Vec<_>>();
        assert_eq!(
            decode_attr_sketch_bins(8, 8, &zstd_words(&words)),
            Ok(expected)
        );
    }

    #[test]
    fn decodes_grouped_headers_and_variable_byte_tail() {
        let mut words = vec![514, 0];
        // Eight zero-width block headers are packed into the second word.
        // The two-value tail is little-endian variable-byte data: 1, 300.
        words.push(u64::from_le_bytes([0x81, 0x2c, 0x82, 0, 0, 0, 0, 0]));
        let mut expected = vec![0; 512];
        expected.extend([1, 300]);
        assert_eq!(
            decode_attr_sketch_bins(257, 2, &zstd_words(&words)),
            Ok(expected)
        );
    }

    #[test]
    fn rejects_declared_length_mismatch() {
        let error = decode_attr_sketch_bins(8, 8, &zstd_words(&[63, 0])).unwrap_err();
        assert_eq!(
            error,
            AttrSketchDecodeError::WrongBinCount {
                declared: 63,
                expected: 64
            }
        );
    }

    #[test]
    fn rust_xxh3_matches_the_upstream_openhft_seeded_fixture() {
        let bytes = (0_u8..8).collect::<Vec<_>>();
        assert_eq!(
            xxh3_64_with_seed(&bytes, 42).cast_signed(),
            5_997_435_870_397_431_563
        );
    }

    #[test]
    fn estimates_each_jvm_hash_input_from_the_minimum_bin() {
        let inputs = [
            AttrSketchInput::Nil,
            AttrSketchInput::Long(-123),
            AttrSketchInput::Double(12.5),
            AttrSketchInput::String("Instant 🚀"),
            AttrSketchInput::Boolean(true),
            AttrSketchInput::BigInteger(&[0, 0x80, 0, 0, 0, 0, 0, 0, 0]),
        ];
        for input in inputs {
            let width = 257;
            let depth = 4;
            let initial_seed = hash_input(u64::MAX, input);
            let mut bins = vec![99; width * depth];
            for row in 0..depth {
                let hash = hash_input(initial_seed.wrapping_add(row as u64), input);
                let column = usize::try_from(hash % width as u64).unwrap();
                bins[row * width + column] = 7 + row as u64;
            }
            let sketch = AttrSketch {
                width,
                depth,
                bins,
                total: 20,
                total_not_binned: 0,
            };
            assert_eq!(sketch.estimate(input), 7);
        }
    }
}
