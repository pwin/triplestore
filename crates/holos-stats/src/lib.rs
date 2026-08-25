//! HOLOS L3 — characteristic sets and cardinality estimation.
//!
//! `DESIGN.md` §7: *"The highest-leverage estimator for RDF is characteristic sets
//! (Neumann & Moerkotte): the set of distinct predicate-sets occurring on subjects. They
//! give accurate cardinality estimates for star patterns, which is what RDF queries mostly
//! are, and they are what separates engines that plan well from engines that guess."*
//!
//! # What this is for
//!
//! The reused evaluator's optimiser reorders joins using an estimator that is a **fixed
//! lookup table**: a pattern `?s <p> ?o` is estimated at 10,000 rows whether that predicate
//! occurs three times or three million. It has no access to the data at all. That is not a
//! criticism of it — an optimiser that ships without a store has nothing else to go on —
//! but it is precisely the gap a store-aware estimator fills.
//!
//! There is no injection point: `Optimizer::optimize_graph_pattern` is a free function and
//! spareval calls it internally. So this crate does not *replace* the planner. It builds
//! the statistics a planner needs and makes the case for one **measurable**: see
//! `examples/estimator_accuracy.rs`, which puts this estimator, the constants, and the true
//! cardinality side by side. §13 Q2 asks whether the hypertrie is worth it given a good
//! optimiser; that question cannot be answered without first knowing how good the estimates
//! can get.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// Cardinality estimation *is* turning integer counts into floating-point estimates. Above
// 2^53 the cast loses precision, and at that point the estimate is already an approximation
// by many orders of magnitude more than the cast costs.
#![allow(clippy::cast_precision_loss)]

pub mod baseline;
pub mod characteristic;
pub mod reorder;

pub use characteristic::{CharacteristicSet, Statistics};
pub use reorder::reorder_query;

use holos_core::TermId;

/// A triple pattern, as far as estimation is concerned.
///
/// Only which positions are bound, and to what, matters — variable *names* matter for
/// joins, which [`Statistics::estimate_bgp`] handles separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pattern {
    /// The subject, if bound.
    pub subject: Option<TermId>,
    /// The predicate, if bound.
    pub predicate: Option<TermId>,
    /// The object, if bound.
    pub object: Option<TermId>,
    /// The variable in subject position, if any. Patterns sharing one form a star.
    pub subject_var: Option<u32>,
}

impl Pattern {
    /// A pattern with a bound predicate and a subject variable — the common shape.
    #[must_use]
    pub fn star(subject_var: u32, predicate: TermId, object: Option<TermId>) -> Self {
        Self {
            subject: None,
            predicate: Some(predicate),
            object,
            subject_var: Some(subject_var),
        }
    }

    /// A pattern with no variables shared with anything.
    #[must_use]
    pub fn single(
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
    ) -> Self {
        Self {
            subject,
            predicate,
            object,
            subject_var: None,
        }
    }
}
