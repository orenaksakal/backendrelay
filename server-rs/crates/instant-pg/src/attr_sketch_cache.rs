//! Bounded reuse of a pure decode, never a database snapshot. Callers must read
//! the row afresh and supply its exact bytes and dimensions on every invocation.
use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::{AttrSketchDecodeError, decode_attr_sketch_bins};

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 128 * 1024 * 1024;

static DECODED_BINS: LazyLock<DecodedBinsCache> =
    LazyLock::new(|| DecodedBinsCache::new(MAX_ENTRIES, MAX_BYTES));

pub(crate) fn decode_shared_bins(
    width: usize,
    depth: usize,
    compressed: &[u8],
) -> Result<Arc<[u64]>, AttrSketchDecodeError> {
    DECODED_BINS.decode(width, depth, compressed)
}

struct Entry {
    key: (usize, usize, u64),
    compressed: Box<[u8]>,
    bins: Arc<[u64]>,
    weight: usize,
}

#[derive(Default)]
struct CacheState {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl CacheState {
    fn get(&self, key: (usize, usize, u64), compressed: &[u8]) -> Option<Arc<[u64]>> {
        self.entries
            .iter()
            // The digest only accelerates lookup. Exact input equality, not a
            // hash, app/attribute ID, LSN, or timestamp, establishes a hit.
            .find(|entry| entry.key == key && entry.compressed.as_ref() == compressed)
            .map(|entry| entry.bins.clone())
    }
}

struct DecodedBinsCache {
    state: Mutex<CacheState>,
    max_entries: usize,
    max_bytes: usize,
}

impl DecodedBinsCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            max_entries,
            max_bytes,
        }
    }

    fn decode(
        &self,
        width: usize,
        depth: usize,
        compressed: &[u8],
    ) -> Result<Arc<[u64]>, AttrSketchDecodeError> {
        let key = (width, depth, xxh3_64_with_seed(compressed, 0));
        if let Some(bins) = self.state.lock().unwrap().get(key, compressed) {
            return Ok(bins);
        }
        // Never decompress under the lock, cache errors, or let one tenant's
        // cold decode block unrelated warm readers. Concurrent misses can do
        // duplicate work; insertion below retains only one identical result.
        let bins: Arc<[u64]> = decode_attr_sketch_bins(width, depth, compressed)?.into();
        let weight = bins
            .len()
            .checked_mul(size_of::<u64>())
            .and_then(|bytes| bytes.checked_add(compressed.len()))
            .and_then(|bytes| bytes.checked_add(size_of::<Entry>() + 2 * size_of::<usize>()));
        let Some(weight) = weight.filter(|weight| *weight <= self.max_bytes) else {
            return Ok(bins);
        };
        if self.max_entries == 0 {
            return Ok(bins);
        }
        let entry = Entry {
            key,
            compressed: compressed.into(),
            bins: bins.clone(),
            weight,
        };
        let mut evicted = Vec::new();
        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.get(key, compressed) {
            return Ok(existing);
        }
        while state.entries.len() >= self.max_entries || state.bytes > self.max_bytes - weight {
            let old = state.entries.pop_front().expect("nonempty bounded cache");
            state.bytes -= old.weight;
            evicted.push(old);
        }
        state.bytes += weight;
        state.entries.push_back(entry);
        drop(state);
        // Large bin buffers are freed outside the shared lock. Existing planner
        // snapshots retain their immutable Arc even after eviction.
        drop(evicted);
        Ok(bins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttrSketch, AttrSketchInput, encode_attr_sketch_bins};

    fn payload(width: usize, depth: usize, value: u64) -> Vec<u8> {
        let mut sketch = AttrSketch::empty(width, depth).unwrap();
        sketch.bins.fill(value);
        encode_attr_sketch_bins(&sketch).unwrap()
    }

    #[test]
    fn identical_bytes_share_bins_but_changes_and_dimensions_do_not() {
        let cache = DecodedBinsCache::new(10, 4096);
        let raw = payload(3, 2, 7);
        let first = cache.decode(3, 2, &raw).unwrap();
        assert!(Arc::ptr_eq(&first, &cache.decode(3, 2, &raw).unwrap()));
        let reshaped = cache.decode(2, 3, &raw).unwrap();
        assert!(!Arc::ptr_eq(&first, &reshaped));
        let changed = cache.decode(3, 2, &payload(3, 2, 9)).unwrap();
        assert_eq!(first.as_ref(), &[7; 6]);
        assert_eq!(changed.as_ref(), &[9; 6]);
        assert!(cache.decode(4, 2, &raw).is_err());
        assert_eq!(cache.state.lock().unwrap().entries.len(), 3);
    }

    #[test]
    fn digest_collisions_require_exact_payload_equality() {
        let cache = DecodedBinsCache::new(10, 4096);
        let a = payload(3, 2, 7);
        let b = payload(3, 2, 9);
        cache.decode(3, 2, &a).unwrap();
        cache.state.lock().unwrap().entries[0].key.2 = xxh3_64_with_seed(&b, 0);
        assert_eq!(cache.decode(3, 2, &b).unwrap().as_ref(), &[9; 6]);
        assert_eq!(cache.state.lock().unwrap().entries.len(), 2);
    }

    #[test]
    fn both_formats_decode_and_corruption_is_never_cached() {
        let cache = DecodedBinsCache::new(10, 4096);
        let current_words = [64_u64, 1, 0xaaaa_aaaa_aaaa_aaaa];
        let bytes: Vec<_> = current_words
            .into_iter()
            .flat_map(u64::to_be_bytes)
            .collect();
        let current = zstd::bulk::compress(&bytes, 1).unwrap();
        let decoded = cache.decode(8, 8, &current).unwrap();
        assert_eq!(*decoded, (0..64).map(|index| index % 2).collect::<Vec<_>>());
        assert!(Arc::ptr_eq(
            &decoded,
            &cache.decode(8, 8, &current).unwrap()
        ));
        let mut legacy = AttrSketch::empty(8, 8).unwrap();
        legacy.bins.copy_from_slice(&decoded);
        assert_eq!(
            *cache
                .decode(8, 8, &encode_attr_sketch_bins(&legacy).unwrap())
                .unwrap(),
            legacy.bins
        );
        for _ in 0..2 {
            assert!(cache.decode(8, 8, &[0xff]).is_err());
            assert!(matches!(
                cache.decode(usize::MAX, 2, &current),
                Err(AttrSketchDecodeError::DimensionsOverflow)
            ));
        }
        assert_eq!(cache.state.lock().unwrap().entries.len(), 2);
    }

    #[test]
    fn retained_bytes_and_entry_count_are_bounded_and_eviction_keeps_readers_valid() {
        let raw = payload(3, 2, 7);
        let weight = 6 * size_of::<u64>() + raw.len() + size_of::<Entry>() + 2 * size_of::<usize>();
        let cache = DecodedBinsCache::new(10, weight);
        let first = cache.decode(3, 2, &raw).unwrap();
        cache.decode(2, 3, &raw).unwrap();
        assert_eq!(cache.state.lock().unwrap().entries.len(), 1);
        assert_eq!(cache.state.lock().unwrap().bytes, weight);
        assert_eq!(first.as_ref(), &[7; 6]);
        assert!(!Arc::ptr_eq(&first, &cache.decode(3, 2, &raw).unwrap()));
        let small = DecodedBinsCache::new(10, weight - 1);
        small.decode(3, 2, &raw).unwrap();
        assert_eq!(small.state.lock().unwrap().bytes, 0);
        let count = DecodedBinsCache::new(1, 4096);
        count.decode(3, 2, &raw).unwrap();
        count.decode(2, 3, &raw).unwrap();
        assert_eq!(count.state.lock().unwrap().entries.len(), 1);
        let disabled = DecodedBinsCache::new(0, 4096);
        disabled.decode(3, 2, &raw).unwrap();
        assert_eq!(disabled.state.lock().unwrap().bytes, 0);
    }

    #[test]
    fn concurrent_misses_converge_and_owned_mutation_cannot_change_shared_bins() {
        let cache = DecodedBinsCache::new(MAX_ENTRIES, MAX_BYTES);
        let raw = payload(20_000, 10, 7);
        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| cache.decode(20_000, 10, &raw).unwrap()))
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(results.iter().all(|bins| Arc::ptr_eq(bins, &results[0])));
        assert_eq!(cache.state.lock().unwrap().entries.len(), 1);
        let mut owned = AttrSketch::empty_default();
        owned.bins.copy_from_slice(&results[0]);
        owned
            .apply_delta(Some(AttrSketchInput::Boolean(true)), 1)
            .unwrap();
        assert_eq!(results[0].as_ref(), vec![7; 200_000]);
    }

    #[test]
    #[ignore = "synthetic production-dimension decode benchmark; run explicitly in release mode"]
    fn production_dimension_decode_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        let mut fixtures = Vec::new();
        for attribute in 0..24 {
            let mut sketch = AttrSketch::empty_default();
            for value in 0..2000 {
                sketch
                    .apply_delta(Some(AttrSketchInput::Long(value * 31 + attribute)), 1)
                    .unwrap();
            }
            fixtures.push(encode_attr_sketch_bins(&sketch).unwrap());
        }
        let iterations = 2400;
        let start = Instant::now();
        for index in 0..iterations {
            black_box(
                decode_attr_sketch_bins(20_000, 10, black_box(&fixtures[index % fixtures.len()]))
                    .unwrap(),
            );
        }
        let uncached = start.elapsed();
        let cache = DecodedBinsCache::new(MAX_ENTRIES, MAX_BYTES);
        let start = Instant::now();
        for index in 0..iterations {
            black_box(
                cache
                    .decode(20_000, 10, black_box(&fixtures[index % fixtures.len()]))
                    .unwrap(),
            );
        }
        let reused = start.elapsed();
        println!(
            "production-sketch-decode: iterations={iterations} fixtures={} bins=200000 uncached_us={} reused_us={} retained_bytes={}",
            fixtures.len(),
            uncached.as_micros(),
            reused.as_micros(),
            cache.state.lock().unwrap().bytes
        );
    }
}
