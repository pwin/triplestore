//! Refusing a query that cannot finish, before it starts.
//!
//! # The gap this fills
//!
//! [`crate::memory`]'s ceiling stops a runaway query from taking the process down, and it
//! does so by refusing a read once the damage is nearly done — several gigabytes and
//! however many minutes in. That is the right backstop and the wrong first answer. A query
//! whose `ORDER BY` has to buffer six hundred million rows was never going to finish, and
//! saying so in a millisecond is worth more than discovering it slowly.
//!
//! This is the lesson stream processing learned first. An engine survives unbounded input by
//! making the *operator* bounded — windows, punctuation, incremental aggregation — and by
//! saying so when an operator cannot be bounded at all. A store answering ad-hoc queries
//! cannot impose a window, but it can recognise the same shape and decline.
//!
//! # Which operators, and why only these
//!
//! Almost all of SPARQL pipelines: a scan feeds a join feeds a filter feeds the serialiser,
//! and memory stays flat however much data goes through. Four operators break that by having
//! to see every row before they can emit the first:
//!
//! | operator | why it buffers |
//! |---|---|
//! | `ORDER BY` | the last row read may sort first |
//! | `DISTINCT` / `REDUCED` | a duplicate may arrive at any time |
//! | `GROUP BY` | a group may gain a member at any time |
//! | the build side of a hash join | probing cannot start until the table is built |
//!
//! Two of those are no longer refused for their size, because they no longer buffer their
//! input: a `DISTINCT` spills ([`crate::spill::Distinct`]), and an `ORDER BY` is answered
//! from a heap when it has a `LIMIT` ([`crate::topk`]) and from a sort-spill-merge when it
//! does not ([`crate::spill::Sorted`]). What is measured for those is the pattern *under*
//! the operator, which can still block.
//!
//! `COUNT(*)` is the instructive contrast: `spareval` answers it with a `u64` counter and
//! streams, while `COUNT(DISTINCT *)` holds every distinct solution and does not. The
//! difference is not the aggregate, it is the `DISTINCT`.
//!
//! # What a `LIMIT` does and does not fix
//!
//! It is tempting to answer every refusal with "add a `LIMIT`", and for `DISTINCT` that
//! genuinely helps: an engine may stop once it has *k* distinct rows. For `ORDER BY` it
//! helps only because [`crate::topk`] exists: `spareval` sorts every row before applying
//! the slice, so `ORDER BY ... LIMIT 10` over a billion rows would buffer a billion rows,
//! and the heap answers `SELECT … ORDER BY … LIMIT` from the ten instead.
//!
//! Without a `LIMIT` the sort is bounded by a budget rather than by *k*, which needs
//! `--spill-bytes` to be set; it is by default. So a sort is measured here by what is
//! *under* it whenever either operator will take it — the body still runs in full, and a
//! blocking operator inside it still blocks — and by its input only when neither will,
//! which means a shape they do not recognise, such as a `DISTINCT` between the sort and
//! the slice.
//!
//! # Estimates, and being wrong in the safe direction
//!
//! The numbers come from [`holos_stats`], whose characteristic-set estimates were measured
//! at a q-error of 1.1 against the reused optimiser's 2×10⁸. That is good enough to tell a
//! thousand rows from a billion, which is the only distinction being made here.
//!
//! It is not good enough to be trusted near the boundary, and the cost of the two errors is
//! not symmetric: refusing a query that would have worked is a bug the user cannot work
//! around, while admitting one that should have been refused merely falls back to the
//! ceiling. So the budget defaults high — [`DEFAULT_BLOCKING_ROWS`] — and the refusal
//! carries the estimate so that a wrong one is visible rather than mysterious.

use holos_stats::{Pattern, Statistics};
use holos_store::Store;
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

/// How many rows a blocking operator may be estimated to buffer before it is refused.
///
/// Ten million solutions is already gigabytes once each carries its terms, and it is far
/// above any query this store is *for*. The budget exists to catch a mistake, not to shape
/// ordinary work — the same reasoning, and the same posture, as the bind join's row budget.
pub const DEFAULT_BLOCKING_ROWS: u64 = 10_000_000;

/// A blocking operator, and what it was estimated to have to hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocking {
    /// The operator, as a user would write it.
    pub operator: &'static str,
    /// Estimated rows reaching it.
    pub rows: u64,
}

impl std::fmt::Display for Blocking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} over an estimated {} rows", self.operator, self.rows)
    }
}

/// The blocking operator with the largest estimated input, if any exceeds `budget`.
///
/// `None` means the query either has no blocking operator or is estimated to fit. The
/// largest rather than the first, so the message names the one that will actually hurt.
#[must_use]
pub fn over_budget(
    query: &spargebra::Query,
    stats: &Statistics,
    store: &Store,
    budget: u64,
    spilling: bool,
) -> Option<Blocking> {
    let ctx = Context { stats, store };
    let mut found = Vec::new();
    // A sort answered in bounded memory holds its heap or its budget, whatever its input;
    // what can still block is inside it.
    let root = crate::topk::sorted_body(query, spilling).unwrap_or_else(|| body(query));
    ctx.walk(root, &mut found);
    found
        .into_iter()
        .filter(|b| b.rows > budget)
        .max_by_key(|b| b.rows)
}

/// The graph pattern a query evaluates, whatever its result form.
fn body(query: &spargebra::Query) -> &GraphPattern {
    match query {
        spargebra::Query::Select { pattern, .. }
        | spargebra::Query::Construct { pattern, .. }
        | spargebra::Query::Describe { pattern, .. }
        | spargebra::Query::Ask { pattern, .. } => pattern,
    }
}

struct Context<'a> {
    stats: &'a Statistics,
    store: &'a Store,
}

impl Context<'_> {
    /// Collects every blocking operator, with the rows estimated to reach it.
    fn walk(&self, pattern: &GraphPattern, found: &mut Vec<Blocking>) {
        let mut note = |operator: &'static str, inner: &GraphPattern| {
            found.push(Blocking {
                operator,
                rows: self.rows(inner),
            });
        };
        match pattern {
            GraphPattern::OrderBy { inner, .. } => note("ORDER BY", inner),
            GraphPattern::Distinct { inner } => note("DISTINCT", inner),
            GraphPattern::Reduced { inner } => note("REDUCED", inner),
            GraphPattern::Group {
                inner,
                variables,
                aggregates,
            } if buffers(variables, aggregates) => note("GROUP BY", inner),
            _ => {}
        }
        for child in children(pattern) {
            self.walk(child, found);
        }
    }

    /// Rows a pattern is estimated to produce.
    ///
    /// Deliberately crude away from the one case that matters. A basic graph pattern gets
    /// the real estimator; everything above it gets an arithmetic that errs *downward*,
    /// because an over-estimate here refuses a query that would have worked.
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "a new spargebra pattern must compile here, and estimating it as zero                   admits the query rather than refusing one nothing understands"
    )]
    fn rows(&self, pattern: &GraphPattern) -> u64 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a row estimate, saturating at u64::MAX by construction"
        )]
        fn to_rows(estimate: f64) -> u64 {
            if estimate.is_finite() && estimate > 0.0 {
                estimate.min(u64::MAX as f64) as u64
            } else {
                0
            }
        }

        match pattern {
            GraphPattern::Bgp { patterns } => {
                to_rows(self.stats.estimate_bgp(&self.encode(patterns)))
            }

            // A slice bounds what leaves it, and for `DISTINCT` it bounds the work too: an
            // engine may stop once it has k distinct rows. It does *not* bound an `ORDER BY`
            // under some other operator — only the shape `crate::topk` answers is spared,
            // and `over_budget` steps past that one before walking.
            GraphPattern::Slice { inner, length, .. } => match length {
                Some(limit) => self.rows(inner).min(*limit as u64),
                None => self.rows(inner),
            },

            // A join can be anywhere between the smaller side and the product. Taking the
            // larger side is the compromise: it never under-states a scan, and it does not
            // invent a cross product that the estimator has no basis for.
            GraphPattern::Join { left, right } => self.rows(left).max(self.rows(right)),
            // Both can only shrink the left side, never grow it.
            GraphPattern::LeftJoin { left, .. } | GraphPattern::Minus { left, .. } => {
                self.rows(left)
            }
            GraphPattern::Union { left, right } => self.rows(left).saturating_add(self.rows(right)),

            // A filter can only remove rows, and by how much is not knowable from these
            // statistics. Passing the input through unchanged keeps the estimate an upper
            // bound on what the operator above has to hold, which is the question asked.
            GraphPattern::Filter { inner, .. }
            | GraphPattern::Graph { inner, .. }
            | GraphPattern::Extend { inner, .. }
            | GraphPattern::OrderBy { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Group { inner, .. }
            | GraphPattern::Service { inner, .. } => self.rows(inner),

            // `VALUES` is its own row count.
            GraphPattern::Values { bindings, .. } => bindings.len() as u64,
            // A property path, and whatever spargebra adds next, are not things these
            // statistics describe. Estimating zero admits the query, which is the safe
            // direction: an over-estimate refuses work that would have succeeded, and the
            // memory ceiling is still behind this.
            _ => 0,
        }
    }

    /// The statistics' view of a basic graph pattern.
    fn encode(&self, patterns: &[TriplePattern]) -> Vec<Pattern> {
        let resolve = |t: &TermPattern| match t {
            TermPattern::NamedNode(n) => self
                .store
                .lookup_term(oxrdf::TermRef::NamedNode(n.as_ref()))
                .ok()
                .flatten(),
            TermPattern::Literal(l) => self
                .store
                .lookup_term(oxrdf::TermRef::Literal(l.as_ref()))
                .ok()
                .flatten(),
            _ => None,
        };
        patterns
            .iter()
            .map(|p| {
                let predicate = match &p.predicate {
                    NamedNodePattern::NamedNode(n) => {
                        self.store.lookup_term(n.as_ref().into()).ok().flatten()
                    }
                    NamedNodePattern::Variable(_) => None,
                };
                Pattern::single(resolve(&p.subject), predicate, resolve(&p.object))
            })
            .collect()
    }
}

/// Whether a `Group` has to hold rows, or can accumulate as they go past.
///
/// Not every aggregate blocks, and `COUNT(*)` is the case that proves it: with no grouping
/// key there is exactly one group, and `spareval` answers it with a `u64` counter that never
/// grows. Treating every `Group` as blocking refused it — a query that streams in constant
/// memory, declined for being over a memory budget.
///
/// Two things make a group buffer:
///
/// - **A grouping key.** One accumulator per distinct value of it, so the memory is the
///   number of distinct keys. That is bounded by the input, which is what gets estimated —
///   an over-estimate, and deliberately so, since the alternative is not noticing.
/// - **A `DISTINCT` aggregate**, with or without a key. `COUNT(DISTINCT ?x)` has to remember
///   every value it has seen to know whether the next one is new, which is the same
///   structure `SELECT DISTINCT` builds and the same reason it cannot stream.
fn buffers(
    variables: &[spargebra::term::Variable],
    aggregates: &[(
        spargebra::term::Variable,
        spargebra::algebra::AggregateExpression,
    )],
) -> bool {
    use spargebra::algebra::AggregateExpression;
    if !variables.is_empty() {
        return true;
    }
    aggregates.iter().any(|(_, aggregate)| match aggregate {
        AggregateExpression::CountSolutions { distinct }
        | AggregateExpression::FunctionCall { distinct, .. } => *distinct,
    })
}

/// Every sub-pattern of a pattern, so the walk reaches operators at any depth.
fn children(pattern: &GraphPattern) -> Vec<&GraphPattern> {
    match pattern {
        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right }
        | GraphPattern::LeftJoin { left, right, .. } => vec![left, right],
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Group { inner, .. }
        | GraphPattern::Service { inner, .. } => vec![inner],
        _ => Vec::new(),
    }
}
