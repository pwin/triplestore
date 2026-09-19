//! "Has this load interned that term before?", answered without touching the disk.
//!
//! # Why a bulk load needs to ask
//!
//! Every term a load meets is either new — allocate an id — or already interned — reuse
//! one. The term cache in front of the dictionary answers that exactly, but it is cleared at
//! every mid-load flush to keep memory flat (`flush_dictionary` says why), so after the first
//! flush the cache no longer knows what it has forgotten. From then on **every new term** is
//! looked up in `str2id` on disk before it can be allocated, to prove it is new.
//!
//! Measured on a 3-million-quad load forced through sixty flushes: 1,268,458 of those reads
//! against 100,287 that found anything, at **3.9 µs each** where an empty store answered in
//! 0.4 µs. They were the whole of the flush penalty. A bloom filter on `str2id` itself took
//! that to 2.8 µs and no lower, because a miss pays a check per ingested file whatever the
//! filter says, and a load makes many files.
//!
//! # What this is
//!
//! A bloom filter over the serialised bytes of every term the load has interned, kept in
//! memory for the load's lifetime. It says "never seen" with certainty and "possibly seen"
//! with a small false-positive rate; a term it has never seen skips the disk entirely, and a
//! term it might have seen goes to `resolve` exactly as before. So a false positive costs one
//! read that was going to happen anyway, and there is no false negative to cost anything.
//!
//! # When it must not be used
//!
//! Only when the load began on an **empty dictionary**. A term interned before the load
//! started is not in this filter, and skipping the lookup for it would allocate a second id
//! for the same term — two ids that do not join, which is silent corruption. `BulkState`
//! creates the filter only when `dictionary_len() == 0` at `begin_bulk_load`; a load into a
//! populated store runs exactly as it did before this existed. Seeding the filter from the
//! existing dictionary would lift that restriction, and is the obvious next step.
//!
//! # Size
//!
//! Fixed at [`DEFAULT_BYTES`] rather than sized from the data, because the data's size is
//! not known until the load is over. At 256 MiB that is 10.8 bits per key for the
//! 199-million-term store this was written against, about 0.8% false positives with seven
//! hashes; a store with five times the terms would see a filter five times fuller and a
//! false-positive rate in the tens of percent — worse, never wrong. The memory is the same
//! order as the dictionary buffer the load already holds.

use rustc_hash::FxHasher;
use std::hash::Hasher;

/// The filter's size. See the module docs for what this buys at what scale.
pub(super) const DEFAULT_BYTES: usize = 256 << 20;

/// Hash functions per key. Seven is the optimum for around ten bits per key, and it is
/// close to optimal over the whole range a load is likely to land in.
const HASHES: u32 = 7;

/// A bloom filter over byte strings.
pub(super) struct Seen {
    words: Vec<u64>,
    /// How many bits the filter has. Not required to be a power of two: an index is taken
    /// by multiply-and-shift rather than by mask, so the filter is exactly the size asked
    /// for. The first version rounded down to a power of two and a test caught it — a
    /// request for ten bits per key got five, and the false-positive rate was 11.8% where
    /// the theory for five bits says 11.8%.
    bits: u64,
    /// How many keys have been inserted, for the report at the end.
    inserted: u64,
}

impl Seen {
    /// A filter of `bytes` bytes, exactly (to the nearest word).
    pub(super) fn with_bytes(bytes: usize) -> Self {
        let words = (bytes / 8).max(1);
        Self {
            words: vec![0; words],
            bits: words as u64 * 64,
            inserted: 0,
        }
    }

    /// Maps a 64-bit hash onto `0..bits` uniformly, without a modulus.
    ///
    /// Lemire's fast range: the top 64 bits of the 128-bit product. Uses the hash's high
    /// bits, which is why `h1 + i * h2` below is fine as an input — the sum's high bits
    /// move with `i` as surely as its low bits do.
    fn index(&self, hash: u64) -> u64 {
        ((u128::from(hash) * u128::from(self.bits)) >> 64) as u64
    }

    /// Two 64-bit hashes of the key, from which the others are derived.
    ///
    /// Kirsch–Mitzenmacher: `h1 + i * h2` for `i` in `0..HASHES` is as good as `HASHES`
    /// independent functions for a bloom filter, and costs one hash of the key rather than
    /// seven. `h2` is `h1` put through a mixer so the two are not correlated.
    fn hashes(key: &[u8]) -> (u64, u64) {
        let mut hasher = FxHasher::default();
        hasher.write(key);
        let h1 = hasher.finish();
        let mut z = h1.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        // An even `h2` would make every other probe land on an even bit; forcing it odd
        // keeps the sequence covering the whole table.
        (h1, (z ^ (z >> 31)) | 1)
    }

    /// Records a key.
    pub(super) fn insert(&mut self, key: &[u8]) {
        let (h1, h2) = Self::hashes(key);
        for i in 0..u64::from(HASHES) {
            let bit = self.index(h1.wrapping_add(i.wrapping_mul(h2)));
            self.words[(bit / 64) as usize] |= 1 << (bit % 64);
        }
        self.inserted += 1;
    }

    /// `false` means the key was never inserted. `true` means it may have been.
    pub(super) fn may_contain(&self, key: &[u8]) -> bool {
        let (h1, h2) = Self::hashes(key);
        (0..u64::from(HASHES)).all(|i| {
            let bit = self.index(h1.wrapping_add(i.wrapping_mul(h2)));
            self.words[(bit / 64) as usize] & (1 << (bit % 64)) != 0
        })
    }

}

impl std::fmt::Debug for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Seen({} keys in {} MiB)",
            self.inserted,
            self.words.len() * 8 / (1 << 20)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property everything rests on: no false negatives, ever. A key that went in must
    /// come back as possibly present, or the load would allocate it a second id.
    #[test]
    fn everything_inserted_is_reported_present() {
        let mut seen = Seen::with_bytes(1 << 20);
        let keys: Vec<Vec<u8>> = (0..100_000u32).map(|i| format!("term-{i}").into_bytes()).collect();
        for key in &keys {
            seen.insert(key);
        }
        for key in &keys {
            assert!(seen.may_contain(key), "{key:?} was inserted and must be found");
        }
        assert_eq!(seen.inserted, 100_000);
    }

    /// The property that makes it worth having: a key that never went in is almost always
    /// reported absent. At 1 MiB and 100,000 keys — 84 bits per key, far roomier than a real
    /// load — the false-positive rate should be far below one in a thousand.
    #[test]
    fn keys_never_inserted_are_almost_always_reported_absent() {
        let mut seen = Seen::with_bytes(1 << 20);
        for i in 0..100_000u32 {
            seen.insert(format!("term-{i}").as_bytes());
        }
        let false_positives = (0..100_000u32)
            .filter(|i| seen.may_contain(format!("other-{i}").as_bytes()))
            .count();
        assert!(
            false_positives < 100,
            "{false_positives} of 100,000 absent keys reported present"
        );
    }

    /// At the load's own density — ten bits per key — the rate is about one in a hundred,
    /// and a filter that came out an order of magnitude worse would mean the hashing had
    /// gone wrong rather than the theory.
    #[test]
    fn at_ten_bits_per_key_the_false_positive_rate_is_around_one_percent() {
        let keys = 200_000u32;
        let mut seen = Seen::with_bytes(keys as usize * 10 / 8);
        for i in 0..keys {
            seen.insert(format!("k{i}").as_bytes());
        }
        let false_positives = (0..keys)
            .filter(|i| seen.may_contain(format!("absent{i}").as_bytes()))
            .count();
        let rate = false_positives as f64 / f64::from(keys);
        assert!(
            (0.002..0.03).contains(&rate),
            "false-positive rate {rate:.4} is not the ~1% ten bits per key should give"
        );
    }

    #[test]
    fn an_empty_filter_contains_nothing() {
        let seen = Seen::with_bytes(4096);
        assert!(!seen.may_contain(b"anything"));
        assert_eq!(seen.inserted, 0);
    }

    /// A request is honoured to the word, and a tiny one still yields a working filter.
    #[test]
    fn sizes_are_honoured_and_never_zero() {
        for (bytes, words) in [(0usize, 1usize), (1, 1), (7, 1), (8, 1), (9, 1), (100, 12), (250_000, 31_250)] {
            let seen = Seen::with_bytes(bytes);
            assert_eq!(seen.words.len(), words, "{bytes} bytes");
            assert_eq!(seen.bits, words as u64 * 64);
        }
    }

    /// Every index the filter computes lands inside it, for a size that is not a power of
    /// two — the case a mask would silently get wrong.
    #[test]
    fn indices_stay_in_range_for_an_odd_size() {
        let mut seen = Seen::with_bytes(1_000);
        for i in 0..50_000u32 {
            seen.insert(format!("k{i}").as_bytes());
            assert!(seen.may_contain(format!("k{i}").as_bytes()));
        }
    }
}
