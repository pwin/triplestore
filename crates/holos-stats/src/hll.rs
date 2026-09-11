//! Counting distinct things without remembering them.
//!
//! `DESIGN.md` §7 specifies `HyperLogLog` sketches for the distinct-subject and
//! distinct-object counts, and this is them. The counts were exact until now, collected into
//! a `FxHashSet` per predicate — correct, unremarkable on a small store, and on a
//! 653.8-million-triple one the largest thing the statistics build held, every byte of it
//! existing to be asked its `len` at the end.
//!
//! # What this is worth, and what it is not
//!
//! Be careful with the credit. Removing the *subject* sets took the build's peak from
//! **12,984 MiB to 5,207 MiB**. Sketching the objects on top of that changed the peak on the
//! same store by **nothing measurable** — it came back at 5,206 MiB, because what remains at
//! that point is the cost of having a 20.65 GB store open and scanning it, not the object
//! sets. The estimate that objects were "about a third" of the original was mine, and it was
//! wrong.
//!
//! So this is not a measured win on that dataset. It is a bound: the sets grew with distinct
//! objects per predicate and nothing capped them, and this is 16 KiB per predicate whatever
//! the data does. That dataset happened not to exercise the case; another will.
//!
//! Distinct *subjects* got a better answer than a sketch: the scan is in `spo` order, so a
//! subject's quads arrive together and the count is an increment as each subject closes,
//! exact and free. Objects arrive scattered in that order and have no such trick, so they get
//! this: **16 KiB per predicate, whatever the cardinality**.
//!
//! # Why an approximation is safe here
//!
//! These counts feed cardinality *estimates*, and the estimator they feed was measured at a
//! q-error of 1.1 — about 10%. This sketch was measured too, by `tests::measure`, and the
//! worst it did across cardinalities from 1 to 5,000,000 was **2.3%**:
//!
//! | distinct | estimate | error |
//! |---|---|---|
//! | 1, 2, 5, 10, 50 | exact | 0% |
//! | 100 | 99 | −1.0% |
//! | 1,000 | 995 | −0.5% |
//! | 10,000 | 9,929 | −0.7% |
//! | 100,000 | 101,347 | +1.3% |
//! | 1,000,000 | 1,022,694 | +2.3% |
//! | 5,000,000 | 5,002,116 | +0.04% |
//!
//! Two disjoint id ranges at 1,000,000 gave +2.3% and +0.6%, which says that figure is
//! variance rather than bias — the standard error at this precision is 0.81%, and these are
//! draws from it. Four times inside the error the estimator already carries, on numbers used
//! only as ratio denominators (`triples / objects`) that are guarded against zero anyway.
//!
//! The docstring on `Statistics::build` used to argue against a sketch, and it was right at
//! the time: these counts were about to be used to measure how accurate estimation *can* be,
//! and an approximation in the yardstick would have muddied the measurement it existed to
//! support. That measurement has since been made.
//!
//! # How it works
//!
//! Hash each value to 64 bits. The top [`PRECISION`] bits pick a register; the number of
//! leading zeros in what remains, plus one, is a *rank*. A rank of `k` turns up about once in
//! `2^k` values, so the largest rank a register has seen is evidence of how many distinct
//! values hashed to it, and the harmonic mean across registers turns that evidence into a
//! count. Nothing about the value itself is kept, which is the whole point: the same value
//! twice produces the same register and the same rank, and leaves no trace that it came
//! twice.
//!
//! Small cardinalities are where the raw estimator is worst, and where a store's data
//! frequently sits — a predicate with two objects is ordinary. So below the usual threshold
//! this switches to *linear counting* over the empty registers, which is near-exact for the
//! small counts: two distinct objects come back as two, not as an approximation of two.
//!
//! 64-bit hashes make the large-range correction unnecessary. It exists in the original paper
//! to undo 32-bit collisions, which begin to matter around 2^32 values; at 64 bits the
//! collision term is negligible against the 1% the sketch already carries.

use holos_core::TermId;

/// Register-index bits. 2^14 registers, **16 KiB per sketch**, 0.81% standard error.
///
/// The error goes as `1.04 / sqrt(m)`, so precision is bought by the byte: 16 would be 64 KiB
/// and 0.41%, and measuring both showed exactly that — a worst case of 2.3% against 1.0%
/// across the same cardinalities.
///
/// 14 is the choice because the accuracy 16 buys cannot be spent. The consumer tolerates 10%,
/// so both sit comfortably inside it, while the memory is per *predicate* and therefore
/// unbounded in something the dataset controls: a store with Wikidata's eleven thousand
/// properties spends 176 MB here and 704 MB at 16. Replacing a structure that grew without
/// limit in the data with one that grows without limit in the schema, for precision nothing
/// downstream can use, would be the same mistake in a new place.
const PRECISION: u32 = 14;

/// Registers per sketch.
const REGISTERS: usize = 1 << PRECISION;

/// The largest rank the remaining bits can express, once `PRECISION` are taken for the index.
const MAX_RANK: u32 = 64 - PRECISION + 1;

/// A distinct-count sketch over `TermId`s.
#[derive(Clone)]
pub struct DistinctCount {
    /// Largest rank seen per register. Boxed because 16 KiB inline would move on every
    /// `FxHashMap` resize.
    registers: Box<[u8]>,
}

impl Default for DistinctCount {
    fn default() -> Self {
        Self {
            registers: vec![0u8; REGISTERS].into_boxed_slice(),
        }
    }
}

impl std::fmt::Debug for DistinctCount {
    /// Prints the estimate rather than sixteen kibibytes of registers.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DistinctCount(~{})", self.estimate())
    }
}

impl DistinctCount {
    /// Records one value. Recording it again changes nothing.
    pub fn add(&mut self, id: TermId) {
        let hash = mix(id.to_raw());
        // Top bits pick the register, so the index and the counted bits never overlap.
        let index = (hash >> (64 - PRECISION)) as usize;
        // Shifting the index bits off the top leaves the rest left-aligned, so counting
        // leading zeros counts them within the remaining width and nothing else.
        let rank = ((hash << PRECISION).leading_zeros() + 1).min(MAX_RANK);
        let register = &mut self.registers[index];
        #[allow(
            clippy::cast_possible_truncation,
            reason = "rank is clamped to MAX_RANK, which is 51"
        )]
        let rank = rank as u8;
        if rank > *register {
            *register = rank;
        }
    }

    /// The estimated number of distinct values recorded.
    #[must_use]
    pub fn estimate(&self) -> u64 {
        let m = REGISTERS as f64;
        let mut harmonic = 0.0;
        let mut empty = 0u32;
        for &register in &self.registers {
            harmonic += 2.0_f64.powi(-i32::from(register));
            if register == 0 {
                empty += 1;
            }
        }
        // The bias constant for m >= 128, from the paper.
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let raw = alpha * m * m / harmonic;

        // Below this the raw estimator is biased and linear counting is better — and, for the
        // handful-of-values case that real predicates often are, effectively exact. It needs
        // an empty register to take a logarithm of; with none left it does not apply.
        if raw <= 2.5 * m && empty > 0 {
            let linear = m * (m / f64::from(empty)).ln();
            return round(linear);
        }
        round(raw)
    }
}

/// Rounds a non-negative estimate to a count, saturating rather than wrapping.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "guarded: negatives clamp to zero and the cast saturates at u64::MAX"
)]
fn round(estimate: f64) -> u64 {
    if estimate <= 0.0 {
        return 0;
    }
    estimate.round() as u64
}

/// `SplitMix64`'s finalizer: a bijection with good avalanche, which is all a sketch needs.
///
/// `TermId`s are dense and sequential — the dictionary hands them out in order — so their raw
/// bits are the worst possible input to a sketch that reads the top bits as a register index.
/// Without mixing, a load's terms would land in a handful of registers and the estimate would
/// be nonsense. This is not defending against an adversary, only against structure.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_core::Tag;

    fn id(n: u64) -> TermId {
        TermId::new(Tag::Iri, n)
    }

    /// Prints the error curve in the module docs. Ignored by default because it asserts
    /// nothing and takes a few seconds; run it after changing [`PRECISION`] or [`mix`], and
    /// update the table from what it says rather than from what the theory promises.
    ///
    /// ```text
    /// cargo test --release -p holos-stats --lib hll::tests::measure -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "measurement harness, not an assertion"]
    fn measure() {
        for n in [1u64, 2, 5, 10, 50, 100, 500, 1000, 10_000, 100_000, 1_000_000, 5_000_000] {
            let mut sketch = DistinctCount::default();
            for i in 0..n {
                sketch.add(id(i));
            }
            let e = sketch.estimate();
            println!(
                "n={n:>9}  est={e:>9}  err={:>7.3}%",
                (e as f64 - n as f64) / n as f64 * 100.0
            );
        }
        // And a second, disjoint id range, to tell variance from bias.
        for n in [100_000u64, 1_000_000] {
            let mut sketch = DistinctCount::default();
            for i in 0..n {
                sketch.add(id(i + 900_000_000));
            }
            let e = sketch.estimate();
            println!(
                "offset n={n:>9}  est={e:>9}  err={:>7.3}%",
                (e as f64 - n as f64) / n as f64 * 100.0
            );
        }
    }

    #[test]
    fn an_empty_sketch_counts_nothing() {
        assert_eq!(DistinctCount::default().estimate(), 0);
    }

    /// The case the raw estimator is bad at and real data is full of: a predicate with two
    /// objects, or ten. Linear counting has to make these *exact*, not approximately exact,
    /// because an estimate of 3 where the truth is 2 is a 50% error on a small denominator.
    ///
    /// The ceiling is set where measurement put it, not where it would be convenient.
    /// Exactness lasts while values rarely share a register, which at 16,384 registers means
    /// up to the birthday threshold of about 180; 100 already collides once and comes back
    /// 99. An earlier version of this test asserted exactness at 100 and 1000 and was simply
    /// wrong about the algorithm — the code was right and the assertion was not.
    #[test]
    fn small_counts_are_exact() {
        for n in [1u64, 2, 3, 5, 10, 50] {
            let mut sketch = DistinctCount::default();
            for i in 0..n {
                sketch.add(id(i));
            }
            assert_eq!(sketch.estimate(), n, "exact for {n} distinct values");
        }
    }

    #[test]
    fn repeats_are_not_counted() {
        let mut sketch = DistinctCount::default();
        for _ in 0..10_000 {
            for i in 0..5 {
                sketch.add(id(i));
            }
        }
        assert_eq!(sketch.estimate(), 5, "fifty thousand adds, five values");
    }

    /// Where the sketch earns its place: a million distinct values in 16 KiB, against the
    /// 8 MB an exact set of the ids alone would need.
    ///
    /// The bound is 3% because the worst measured was 2.3% and the standard error is 0.81%,
    /// so this sits at about 3.7 sigma — loose enough not to flake on an id distribution that
    /// draws unluckily, tight enough that a broken mixer or an off-by-one in the rank, which
    /// miss by tens of percent, cannot pass it.
    #[test]
    fn large_counts_are_within_a_few_percent() {
        let mut sketch = DistinctCount::default();
        let n = 1_000_000u64;
        for i in 0..n {
            sketch.add(id(i));
        }
        let estimate = sketch.estimate();
        let error = (estimate as f64 - n as f64).abs() / n as f64;
        assert!(
            error < 0.03,
            "estimated {estimate} for {n}, {:.2}% error",
            error * 100.0
        );
    }

    /// Sequential ids are what this actually gets fed, and they are the input a sketch reading
    /// the top bits of an unmixed key would handle worst. Ids far apart must do no better.
    #[test]
    fn a_sparse_spread_of_ids_counts_the_same_as_a_dense_one() {
        let mut dense = DistinctCount::default();
        let mut sparse = DistinctCount::default();
        for i in 0..100_000u64 {
            dense.add(id(i));
            sparse.add(id(i * 7_919));
        }
        let (d, s) = (dense.estimate(), sparse.estimate());
        let difference = (d as f64 - s as f64).abs() / 100_000.0;
        assert!(
            difference < 0.03,
            "dense {d} against sparse {s} for the same 100,000 values"
        );
    }

    /// A sketch is only worth its error if the error does not grow with the count.
    #[test]
    fn the_error_does_not_grow_with_the_count() {
        for n in [10_000u64, 100_000, 1_000_000, 5_000_000] {
            let mut sketch = DistinctCount::default();
            for i in 0..n {
                sketch.add(id(i));
            }
            let error = (sketch.estimate() as f64 - n as f64).abs() / n as f64;
            assert!(error < 0.03, "{:.2}% error at {n}", error * 100.0);
        }
    }
}
