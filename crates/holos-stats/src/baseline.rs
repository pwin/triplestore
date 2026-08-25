//! The estimator the reused optimiser uses, reimplemented for comparison.
//!
//! `sparopt` reorders joins with a cardinality estimator that is a fixed lookup table
//! keyed only on which positions are bound. It is reproduced here — same constants, same
//! shape — so the two estimators can be measured against the same queries and the same
//! ground truth, rather than compared by assertion.
//!
//! This is **not** a criticism of that optimiser. It ships without a store, so it has
//! nothing to consult; a constant table is the only honest thing to do in that position.
//! The point of reproducing it is to find out what having a store to consult is actually
//! worth, which is the question §13 Q2 turns on.

use crate::Pattern;

/// The constants `sparopt` estimates a triple pattern at.
#[must_use]
pub fn estimate_pattern(pattern: &Pattern) -> f64 {
    let bound = (
        pattern.subject.is_some(),
        pattern.predicate.is_some(),
        pattern.object.is_some(),
    );
    let rows: u64 = match bound {
        (true, true, true) => 1,
        (true, true, false) => 10,
        (true, false, true) => 2,
        (false, true, true) => 10_000,
        (true, false, false) => 100,
        (false, false, false) => 1_000_000_000,
        (false, true, false) => 1_000_000,
        (false, false, true) => 100_000,
    };
    rows as f64
}

/// The constants applied to a whole basic graph pattern.
///
/// `sparopt` joins with `left × right / 1000^keys`, which is a fixed selectivity per join
/// key rather than anything derived from the data.
#[must_use]
pub fn estimate_bgp(patterns: &[Pattern]) -> f64 {
    let mut estimate = 0.0_f64;
    for (i, pattern) in patterns.iter().enumerate() {
        let rows = estimate_pattern(pattern);
        if i == 0 {
            estimate = rows;
            continue;
        }
        // One shared join key is the common case for a star.
        estimate = (estimate * rows) / 1_000.0;
    }
    estimate
}

#[cfg(test)]
mod tests {
    // These compare estimates that are exact by construction (0.0 for "no subject
    // has both predicates", a table constant), so an epsilon would weaken the test.
    #![allow(clippy::float_cmp)]
    // The helper mirrors the Option-shaped fields of a Pattern.
    #![allow(clippy::unnecessary_wraps)]
    use super::*;
    use holos_core::{Tag, TermId};

    fn some() -> Option<TermId> {
        Some(TermId::new(Tag::Iri, 1))
    }

    #[test]
    fn the_table_does_not_look_at_the_data() {
        // The whole point: two predicates with wildly different frequencies estimate the
        // same, because the estimator has no way to tell them apart.
        let a = Pattern::single(None, some(), None);
        let b = Pattern::single(None, Some(TermId::new(Tag::Iri, 2)), None);
        assert_eq!(estimate_pattern(&a), estimate_pattern(&b));
        assert!((estimate_pattern(&a) - 1_000_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn binding_a_position_narrows_it() {
        assert!(
            estimate_pattern(&Pattern::single(some(), some(), None))
                < estimate_pattern(&Pattern::single(None, some(), None))
        );
    }
}
