//! An index nested-loop join for the query shapes that suffer most without one.
//!
//! # The gap this closes
//!
//! `spareval` joins by building a hash table from the left input and **scanning the right
//! input in full**. Knowing `?s` is bound to two hundred values does not help, because there
//! is no operator that can use it. `BENCHMARKS.md` measures what that costs on a three-
//! pattern star query:
//!
//! | | 750k quads | 7.5M quads |
//! |---|---:|---:|
//! | Evaluator, already reordered | 43.9 ms | 370.2 ms |
//! | Hand-written bind join | **0.072 ms** | **0.076 ms** |
//! | | **611×** | **4,846×** |
//!
//! The second row is the point: constant in the size of the store, because the work is
//! proportional to the *answer*. The multiple rises with the data and will keep rising.
//!
//! It is also what stopped the spatial index paying off — narrowing fifty thousand
//! geometries to four changes nothing if the join scans all fifty thousand regardless. One
//! missing operator, hit from two directions.
//!
//! # What this handles, and what it refuses
//!
//! A deliberately small fragment, because a fast path that is wrong is worse than no fast
//! path. [`plan`] returns `None` for anything outside it and the caller falls back to
//! `spareval`, which is why the fragment can grow later without risk.
//!
//! Handled: `SELECT` over a single basic graph pattern in the default graph, optionally
//! wrapped in `DISTINCT`, `LIMIT` and `OFFSET`, with a projection.
//!
//! Refused: `FILTER`, `OPTIONAL`, `UNION`, `GRAPH`, `MINUS`, `VALUES`, `BIND`, aggregation,
//! `ORDER BY`, property paths, subqueries, `ASK`/`CONSTRUCT`/`DESCRIBE`, and any pattern
//! mentioning a blank node. Every one of those is a *silent* wrong answer if guessed at.
//!
//! # Why it can give up half way
//!
//! This operator **materialises** its answer; the evaluator it bypasses streams. For the
//! shapes it is meant for that is unimportant, because the answer is small — that is the
//! premise of the whole fragment. For a shape that slipped through and is *not* small it is
//! the difference between a slow query and an exhausted machine: `SELECT * WHERE { ?a ?b ?c
//! . ?d ?e ?f }` over 20,000 triples is a 400-million-row cross product, and materialising
//! it costs about 19 GB.
//!
//! So [`Plan::evaluate`] runs under [`Limits`] and returns `None` on hitting them — a row
//! budget, or a tripped cancellation token. `None` means *use the evaluator*, never *there
//! are no answers*: the rows built so far are discarded rather than returned, and the caller
//! re-evaluates from scratch. That wastes the work done so far, bounded by the budget, in
//! exchange for a guarantee that this path can never be the reason a query exhausts memory
//! or outlives its timeout.
//!
//! # Policy
//!
//! Scans go through [`DatasetView`]'s `QueryableDataset` implementation — the same call
//! `spareval` makes, applying `decide_quad` to every quad. Policy is therefore enforced by
//! construction rather than by remembering to, and the fast path cannot become a way around
//! §14's guarantee.

use crate::view::{DatasetView, ViewError};
use holos_core::TermId;
use holos_stats::Statistics;
use rustc_hash::FxHashMap;
use spareval::{CancellationToken, QueryableDataset};
use spargebra::algebra::GraphPattern;
use spargebra::term::{TermPattern, TriplePattern, Variable};
use spargebra::Query;

/// The two conditions under which the fast path gives up and defers to the evaluator.
///
/// Both mean the same thing to the caller — *evaluate this the ordinary way* — because
/// neither leaves a partial result that could be returned. Neither is a tuning knob for
/// speed; together they are what keeps a wrongly-admitted query from being unbounded.
#[derive(Clone, Copy, Default)]
pub struct Limits<'a> {
    /// Rows to materialise before giving up. `None` means no cap, which is only safe when
    /// the caller already knows the answer is bounded.
    pub rows: Option<usize>,
    /// Checked while scanning, so a query that has run out of time stops here too. Without
    /// it this path would sail past its own deadline: the token is consulted by the
    /// evaluator, and the evaluator is exactly what has been skipped.
    pub token: Option<&'a CancellationToken>,
}

/// Rows this path will hold in memory before handing the query back to the evaluator.
///
/// A row of three bound variables costs roughly a hundred bytes once the `Vec` and its
/// contents are counted, so a million rows is a ceiling of a few hundred megabytes rather
/// than the tens of gigabytes an uncapped cross product reaches. It is deliberately far
/// above any answer this operator is *for*: the budget exists to bound a mistake, not to
/// shape ordinary queries.
pub const DEFAULT_ROW_BUDGET: usize = 1_000_000;

/// Candidate quads examined between two reads of the cancellation token.
///
/// Reading it every time costs a relaxed atomic load on the hot path; never reading it is
/// how a timeout gets missed. A few thousand quads sits well inside the 50 ms granularity
/// the watchdog already has, so the coarser check gives up nothing that matters.
const TOKEN_CHECK_INTERVAL: u64 = 4096;

/// What the recursion accumulates, kept together so `step` takes one argument rather than
/// four.
struct Run<'a> {
    out: Vec<Vec<Option<TermId>>>,
    seen: rustc_hash::FxHashSet<Vec<Option<TermId>>>,
    /// Rows consumed by `OFFSET` so far.
    skipped: usize,
    limits: Limits<'a>,
    /// Set once a limit is hit; every frame unwinds on it.
    abandoned: bool,
    /// Candidate quads examined since the token was last read.
    since_check: u64,
}

impl Run<'_> {
    /// Whether a limit has been hit and the recursion should unwind.
    ///
    /// Called both on entry to each frame and once per candidate quad. The second is not
    /// redundant: a quad that fails to bind never reaches a frame, so a scan that rejects
    /// everything it sees would otherwise run to its end unchecked.
    fn done(&mut self) -> bool {
        if self.abandoned {
            return true;
        }
        if let Some(cap) = self.limits.rows {
            // Both structures, not just `out`. Under `DISTINCT` every row seen is retained
            // in `seen` whether or not it reaches `out`, so a large `OFFSET` grows one and
            // not the other: `SELECT DISTINCT .. OFFSET 100000000` would hold every distinct
            // row of a cross product while `out` stayed empty and the budget stayed unspent.
            if self.out.len() + self.seen.len() > cap {
                self.abandoned = true;
                return true;
            }
        }
        self.since_check += 1;
        if self.since_check >= TOKEN_CHECK_INTERVAL {
            self.since_check = 0;
            if self
                .limits
                .token
                .is_some_and(CancellationToken::is_cancelled)
            {
                self.abandoned = true;
                return true;
            }
        }
        false
    }
}

/// A query this module can answer, reduced to what evaluation needs.
pub struct Plan {
    patterns: Vec<TriplePattern>,
    /// Variables the query projects, in order.
    variables: Vec<Variable>,
    distinct: bool,
    offset: usize,
    limit: Option<usize>,
}

/// Recognises a query this module can answer, or returns `None`.
///
/// Conservative by design: anything not explicitly handled is refused so the caller falls
/// back. The cost of refusing something answerable is a slow query; the cost of accepting
/// something unanswerable is a wrong one.
#[must_use]
pub fn plan(query: &Query) -> Option<Plan> {
    let Query::Select {
        pattern, dataset, ..
    } = query
    else {
        return None;
    };

    // `FROM` and `FROM NAMED` live here rather than in the pattern, so a rewrite that only
    // reads the pattern sees a query it can answer and answers a different one — over the
    // store's default graph instead of the graphs the query named. Scanning is hard-wired to
    // the default graph in `step`, so the only safe response is to refuse.
    if dataset.is_some() {
        return None;
    }

    let mut distinct = false;
    let mut offset = 0usize;
    let mut limit = None;
    let mut node = pattern;

    // Peel the wrappers this fragment allows. Their *order* is part of the meaning: this
    // evaluates as project, then distinct, then offset and limit, which is what
    // `Slice(Distinct(Project(..)))` means. Seeing them the other way round would be
    // `DISTINCT` applied after `LIMIT` — a different query — so it is refused rather than
    // flattened.
    loop {
        match node {
            GraphPattern::Distinct { inner } => {
                distinct = true;
                node = inner;
            }
            GraphPattern::Slice {
                inner,
                start,
                length,
            } => {
                // Nested slices would compose rather than replace; refuse rather than get
                // the arithmetic subtly wrong.
                if offset != 0 || limit.is_some() {
                    return None;
                }
                // A slice *inside* a distinct is `DISTINCT(SLICE(..))`, which deduplicates
                // what the slice already cut down. SPARQL's own grammar cannot produce it,
                // but this fragment should not depend on the parser to stay correct.
                if distinct {
                    return None;
                }
                offset = *start;
                limit = *length;
                node = inner;
            }
            GraphPattern::Project { inner, variables } => {
                let patterns = as_bgp(inner)?;
                return finish(patterns, variables.clone(), distinct, offset, limit);
            }
            _ => return None,
        }
    }
}

/// The patterns of a bare BGP, or `None` if it is anything else.
fn as_bgp(pattern: &GraphPattern) -> Option<&[TriplePattern]> {
    match pattern {
        GraphPattern::Bgp { patterns } => Some(patterns),
        _ => None,
    }
}

fn finish(
    patterns: &[TriplePattern],
    variables: Vec<Variable>,
    distinct: bool,
    offset: usize,
    limit: Option<usize>,
) -> Option<Plan> {
    if patterns.is_empty() {
        return None;
    }
    // A blank node in a pattern behaves as a variable that cannot be projected. Rather than
    // model that, refuse: it is rare in the shapes this exists for.
    for triple in patterns {
        for term in [&triple.subject, &triple.object] {
            if matches!(term, TermPattern::BlankNode(_)) {
                return None;
            }
        }
        if matches!(triple.subject, TermPattern::Triple(_))
            || matches!(triple.object, TermPattern::Triple(_))
        {
            // RDF 1.2 triple terms in a pattern need the term encoding's side table, which
            // this path does not reach into.
            return None;
        }
    }
    // Every projected variable must be bound by the patterns, or the result would need
    // unbound columns this path does not produce.
    let mut bound = Vec::new();
    for triple in patterns {
        collect_variables(triple, &mut bound);
    }
    if !variables.iter().all(|v| bound.contains(v)) {
        return None;
    }
    Some(Plan {
        patterns: patterns.to_vec(),
        variables,
        distinct,
        offset,
        limit,
    })
}

fn collect_variables(triple: &TriplePattern, out: &mut Vec<Variable>) {
    for term in [&triple.subject, &triple.object] {
        if let TermPattern::Variable(v) = term {
            if !out.contains(v) {
                out.push(v.clone());
            }
        }
    }
    if let spargebra::term::NamedNodePattern::Variable(v) = &triple.predicate {
        if !out.contains(v) {
            out.push(v.clone());
        }
    }
}

impl Plan {
    /// Evaluates the plan, returning solutions as term-id tuples in projection order.
    ///
    /// # Errors
    ///
    /// Propagates view failures, which includes a policy refusal under Fail semantics.
    pub fn evaluate(
        &self,
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        limits: Limits<'_>,
    ) -> Result<Option<Vec<Vec<Option<TermId>>>>, ViewError> {
        let mut bindings: FxHashMap<&Variable, TermId> = FxHashMap::default();
        let mut run = Run {
            out: Vec::new(),
            seen: rustc_hash::FxHashSet::default(),
            skipped: 0,
            limits,
            abandoned: false,
            since_check: 0,
        };

        let remaining: Vec<usize> = (0..self.patterns.len()).collect();
        self.step(view, stats, &remaining, &mut bindings, &mut run)?;

        // Discarded, not truncated. A partial answer returned as a whole one is precisely
        // the failure this operator must not introduce.
        if run.abandoned {
            return Ok(None);
        }
        Ok(Some(run.out))
    }

    /// The cheapest remaining pattern, given what is already bound.
    ///
    /// Chosen at each step rather than once up front. After the first pattern binds `?s`,
    /// the others stop being "predicate scans" and become "subject-and-predicate lookups",
    /// which is a different — and far smaller — estimate. Ordering once, before anything is
    /// bound, would miss exactly the effect the operator exists to exploit.
    fn cheapest(
        &self,
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        remaining: &[usize],
        bindings: &FxHashMap<&Variable, TermId>,
    ) -> Option<usize> {
        remaining.iter().copied().min_by(|a, b| {
            let ca = estimate(&self.patterns[*a], view, bindings, stats);
            let cb = estimate(&self.patterns[*b], view, bindings, stats);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    fn step<'p>(
        &'p self,
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        remaining: &[usize],
        bindings: &mut FxHashMap<&'p Variable, TermId>,
        run: &mut Run<'_>,
    ) -> Result<(), ViewError> {
        if run.done() {
            return Ok(());
        }
        if let Some(limit) = self.limit {
            if run.out.len() >= limit {
                // Stopping here is what makes the work proportional to the answer rather
                // than to the store. It is the whole reason for the operator.
                return Ok(());
            }
        }
        let Some(index) = self.cheapest(view, stats, remaining, bindings) else {
            let row = self.project(bindings);
            if self.distinct && !run.seen.insert(row.clone()) {
                return Ok(());
            }
            if run.skipped < self.offset {
                run.skipped += 1;
                return Ok(());
            }
            run.out.push(row);
            return Ok(());
        };

        let triple = &self.patterns[index];
        // A constant absent from the dictionary kills the branch before any scan.
        let Some((subject, predicate, object)) = self.resolve(triple, view, bindings)? else {
            return Ok(());
        };

        // The probe. Where the previous depth bound the subject, this is a prefix lookup
        // rather than a scan — which is the entire operator.
        for quad in view.internal_quads_for_pattern(
            subject.as_ref(),
            predicate.as_ref(),
            object.as_ref(),
            Some(None),
        ) {
            let quad = quad?;
            // Checked here as well as on entry to the next frame, because a quad that fails
            // to bind never reaches one. `?s ?p ?s` over a large store rejects almost every
            // quad it sees, and without this that scan would run to its end no matter what
            // the token said.
            if run.done() {
                break;
            }
            let mut added = Vec::new();
            if !bind(&triple.subject, quad.subject, bindings, &mut added)
                || !bind_predicate(&triple.predicate, quad.predicate, bindings, &mut added)
                || !bind(&triple.object, quad.object, bindings, &mut added)
            {
                for variable in added {
                    bindings.remove(variable);
                }
                continue;
            }
            let rest: Vec<usize> = remaining.iter().copied().filter(|i| *i != index).collect();
            self.step(view, stats, &rest, bindings, run)?;
            for variable in added {
                bindings.remove(variable);
            }
            if run.abandoned {
                break;
            }
            if let Some(limit) = self.limit {
                if run.out.len() >= limit {
                    break;
                }
            }
        }
        Ok(())
    }

    /// The concrete term ids a pattern is constrained to, given what is already bound.
    ///
    /// `None` for the whole triple means a constant in it is absent from the dictionary, so
    /// no quad can match and the branch is dead. That is a common and cheap win: a query
    /// naming a predicate the store has never seen returns immediately.
    fn resolve(
        &self,
        triple: &TriplePattern,
        view: &DatasetView<'_>,
        bindings: &FxHashMap<&Variable, TermId>,
    ) -> Result<Option<(Option<TermId>, Option<TermId>, Option<TermId>)>, ViewError> {
        let subject = match slot(&triple.subject, view, bindings)? {
            Slot::Missing => return Ok(None),
            Slot::Any => None,
            Slot::Fixed(id) => Some(id),
        };
        let predicate = match predicate_slot(&triple.predicate, view, bindings)? {
            Slot::Missing => return Ok(None),
            Slot::Any => None,
            Slot::Fixed(id) => Some(id),
        };
        let object = match slot(&triple.object, view, bindings)? {
            Slot::Missing => return Ok(None),
            Slot::Any => None,
            Slot::Fixed(id) => Some(id),
        };
        Ok(Some((subject, predicate, object)))
    }

    fn project(&self, bindings: &FxHashMap<&Variable, TermId>) -> Vec<Option<TermId>> {
        self.variables
            .iter()
            .map(|v| bindings.get(v).copied())
            .collect()
    }

    /// The variables this plan projects, in order.
    #[must_use]
    pub fn variables(&self) -> &[Variable] {
        &self.variables
    }
}

/// What a pattern position resolves to.
enum Slot {
    /// A constant or an already-bound variable.
    Fixed(TermId),
    /// An unbound variable: anything matches.
    Any,
    /// A constant that is not in the store's dictionary, so nothing can match.
    Missing,
}

fn slot(
    term: &TermPattern,
    view: &DatasetView<'_>,
    bindings: &FxHashMap<&Variable, TermId>,
) -> Result<Slot, ViewError> {
    Ok(match term {
        TermPattern::Variable(v) => match bindings.get(v) {
            Some(id) => Slot::Fixed(*id),
            None => Slot::Any,
        },
        TermPattern::NamedNode(n) => lookup(view, n.as_ref().into())?,
        TermPattern::Literal(l) => lookup(view, l.as_ref().into())?,
        // Refused by `plan`.
        TermPattern::BlankNode(_) | TermPattern::Triple(_) => Slot::Any,
    })
}

fn predicate_slot(
    predicate: &spargebra::term::NamedNodePattern,
    view: &DatasetView<'_>,
    bindings: &FxHashMap<&Variable, TermId>,
) -> Result<Slot, ViewError> {
    Ok(match predicate {
        spargebra::term::NamedNodePattern::Variable(v) => match bindings.get(v) {
            Some(id) => Slot::Fixed(*id),
            None => Slot::Any,
        },
        spargebra::term::NamedNodePattern::NamedNode(n) => lookup(view, n.as_ref().into())?,
    })
}

fn lookup(view: &DatasetView<'_>, term: oxrdf::TermRef<'_>) -> Result<Slot, ViewError> {
    Ok(match view.store().lookup_term(term)? {
        Some(id) => Slot::Fixed(id),
        None => Slot::Missing,
    })
}

/// Binds a pattern position against a scanned term, or reports a conflict.
///
/// A conflict is how a repeated variable within one pattern (`?s ?p ?s`) is enforced: the
/// second occurrence finds the first already bound to something else and rejects the row.
fn bind<'p>(
    term: &'p TermPattern,
    actual: TermId,
    bindings: &mut FxHashMap<&'p Variable, TermId>,
    added: &mut Vec<&'p Variable>,
) -> bool {
    let TermPattern::Variable(v) = term else {
        return true;
    };
    match bindings.get(v) {
        Some(existing) => *existing == actual,
        None => {
            bindings.insert(v, actual);
            added.push(v);
            true
        }
    }
}

fn bind_predicate<'p>(
    predicate: &'p spargebra::term::NamedNodePattern,
    actual: TermId,
    bindings: &mut FxHashMap<&'p Variable, TermId>,
    added: &mut Vec<&'p Variable>,
) -> bool {
    let spargebra::term::NamedNodePattern::Variable(v) = predicate else {
        return true;
    };
    match bindings.get(v) {
        Some(existing) => *existing == actual,
        None => {
            bindings.insert(v, actual);
            added.push(v);
            true
        }
    }
}

/// How many rows a pattern is expected to yield, for ordering.
///
/// Reuses `holos_stats`'s own estimator rather than inventing a second one: it already reads
/// the characteristic-set counts, and two estimators that disagree would make the ordering
/// depend on which code path asked.
///
/// A pattern position already *bound* by an earlier pattern counts as a constant here, which
/// is the whole point — it is why a star query orders itself into one scan and then probes.
fn estimate(
    triple: &TriplePattern,
    view: &DatasetView<'_>,
    bindings: &FxHashMap<&Variable, TermId>,
    stats: Option<&Statistics>,
) -> f64 {
    let resolved = |term: &TermPattern| -> Option<TermId> {
        match term {
            TermPattern::Variable(v) => bindings.get(v).copied(),
            TermPattern::NamedNode(n) => view.store().lookup_term(n.as_ref().into()).ok().flatten(),
            TermPattern::Literal(l) => view.store().lookup_term(l.as_ref().into()).ok().flatten(),
            _ => None,
        }
    };
    let predicate = match &triple.predicate {
        spargebra::term::NamedNodePattern::Variable(v) => bindings.get(v).copied(),
        spargebra::term::NamedNodePattern::NamedNode(n) => {
            view.store().lookup_term(n.as_ref().into()).ok().flatten()
        }
    };
    let pattern = holos_stats::Pattern {
        subject: resolved(&triple.subject),
        predicate,
        object: resolved(&triple.object),
        subject_var: None,
    };
    match stats {
        Some(stats) => stats.estimate_pattern(&pattern),
        // Without statistics, fewer free positions means fewer rows. Crude, and enough to
        // put a pattern with two constants ahead of one with none.
        None => {
            let free = usize::from(pattern.subject.is_none())
                + usize::from(pattern.predicate.is_none())
                + usize::from(pattern.object.is_none());
            10f64.powi(free as i32 * 2)
        }
    }
}
