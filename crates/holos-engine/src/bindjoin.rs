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
//! Handled: `SELECT` in the default graph over basic graph patterns, `JOIN`, `UNION` and
//! `VALUES`, optionally wrapped in `FILTER`, `DISTINCT`, `LIMIT` and `OFFSET`, with a
//! projection.
//!
//! Refused: `OPTIONAL`, `GRAPH`, `MINUS`, `BIND`, aggregation, `ORDER BY`, property paths,
//! subqueries, `ASK`/`CONSTRUCT`/`DESCRIBE`, `FROM`, and any pattern mentioning a blank node.
//! Every one of those is a *silent* wrong answer if guessed at.
//!
//! # Why `JOIN`, `UNION` and `VALUES` are here
//!
//! Not for their own sake. §17's topology rewrite turns `?f geo:sfWithin <window>` into a
//! geometry lookup joined in as a *union* of ordinary patterns, and the spatial index then
//! joins a `VALUES` of candidate geometries onto that. So the shape a routed GeoSPARQL query
//! actually reaches the planner as is
//!
//! ```text
//! Filter(Join(Join(Bgp, Union(Bgp, Union(Bgp, Bgp))), Values))
//! ```
//!
//! and every one of those four node kinds has to be in the fragment before any of it can use
//! an index nested-loop join. `FILTER` alone was not enough, which is what the previous round
//! of this work established the hard way.
//!
//! # How they evaluate
//!
//! The plan is a flat list of [`Item`]s rather than a tree, because evaluation is a nested
//! loop: entering a `UNION` branch simply replaces that entry with the branch's own patterns
//! and carries on. Ordering is still chosen per step, so a `VALUES` of four candidate
//! geometries sorts ahead of a scan of fifty thousand, which is the entire point of the
//! spatial index.
//!
//! `UNION` is a **multiset** union: a solution satisfying two branches is produced twice.
//! That is not untidiness to be deduplicated — the geometry lookup depends on it for a
//! resource carrying both `geo:hasGeometry` and `geo:hasDefaultGeometry`.
//!
//! # Filters, and why they are not reimplemented
//!
//! A filter is applied **as soon as the last variable it mentions is bound**, not after the
//! join. That is most of its value: a predicate applied after the join has already paid for
//! the join, while one applied at depth one prunes every branch below it. Conjunctions are
//! split for the same reason — `A && B` can only be applied when the later of the two is
//! ready, so the halves are pushed separately.
//!
//! The predicate itself is evaluated by **`spareval`'s own expression evaluator**, through
//! this engine's function registry, so `FILTER` semantics here are the evaluator's rather
//! than a second implementation of them. That matters more than it sounds: SPARQL's
//! comparison rules involve datatype-aware value equality and numeric promotion, and an
//! independent implementation of `=` that got `"1"^^xsd:integer` versus `"1.0"^^xsd:decimal`
//! wrong would be a silent wrong answer of exactly the kind this module exists to avoid.
//!
//! Two classes of expression are refused rather than approximated, because the borrowed
//! evaluator runs against an empty dataset and without an evaluation context:
//!
//! * **`EXISTS`** reads the dataset. It would quietly answer `false` for every solution.
//! * **`NOW`, `RAND`, `UUID`, `STRUUID`, `BNODE`** need a context this path does not build.
//!   `NOW` in particular must be constant across a query, which is a property of the context
//!   and not of the function.
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
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{GroundTerm, TermPattern, TriplePattern, Variable};
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
    /// The evaluator whose expression rules the filters borrow. Built once per query, not
    /// once per row: constructing one registers the whole custom-function table.
    evaluator: spareval::QueryEvaluator,
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

/// One `FILTER`, ready to evaluate.
struct Filter {
    /// `sparopt`'s form, because that is what the evaluator's public entry point takes.
    /// Converted once when the plan is built rather than once per row.
    expression: sparopt::algebra::Expression,
    /// The variables it mentions, every one of which the patterns bind. Used to decide the
    /// depth at which the filter becomes evaluable, and to decode only what it needs.
    needs: Vec<Variable>,
}

/// One step of a plan: something that binds variables.
pub enum Item {
    /// A single triple pattern, probed against the store's indexes.
    Pattern(TriplePattern),
    /// `UNION`, flattened to a list of alternatives, each a conjunction of patterns.
    Union(Vec<Vec<TriplePattern>>),
    /// `VALUES`: rows of terms bound directly rather than looked up.
    Values(ValuesItem),
}

/// The rows of a `VALUES` clause.
pub struct ValuesItem {
    variables: Vec<Variable>,
    /// `None` is `UNDEF`, which binds nothing for that variable in that row.
    rows: Vec<Vec<Option<GroundTerm>>>,
}

/// What is left to evaluate, as borrowed pieces of the plan.
///
/// A list rather than a tree: entering a `UNION` branch replaces one entry with that
/// branch's patterns and leaves the rest alone, so the remaining work is always "these
/// things, in whatever order turns out cheapest".
#[derive(Clone, Copy)]
enum Todo<'p> {
    Pattern(&'p TriplePattern),
    Union(&'p [Vec<TriplePattern>]),
    Values(&'p ValuesItem),
}

impl<'p> From<&'p Item> for Todo<'p> {
    fn from(item: &'p Item) -> Self {
        match item {
            Item::Pattern(triple) => Todo::Pattern(triple),
            Item::Union(branches) => Todo::Union(branches),
            Item::Values(values) => Todo::Values(values),
        }
    }
}

/// The variables a subtree binds.
#[derive(Default, Clone)]
struct Bound {
    /// Bound on *every* path through the subtree. The only variables a hoisted `FILTER` may
    /// reference, because they are the ones whose value does not depend on which `UNION`
    /// branch was taken.
    certain: Vec<Variable>,
    /// Bound on at least one path. What a projection may name; the rest come back unbound.
    possible: Vec<Variable>,
}

impl Bound {
    /// Both subtrees run, so everything either binds is bound.
    fn joined(mut self, other: Self) -> Self {
        for v in other.certain {
            if !self.certain.contains(&v) {
                self.certain.push(v);
            }
        }
        for v in other.possible {
            if !self.possible.contains(&v) {
                self.possible.push(v);
            }
        }
        self
    }

    /// One branch or the other runs, so only what *both* bind is certain.
    fn alternated(mut self, other: Self) -> Self {
        self.certain.retain(|v| other.certain.contains(v));
        for v in other.possible {
            if !self.possible.contains(&v) {
                self.possible.push(v);
            }
        }
        self
    }
}

/// A query this module can answer, reduced to what evaluation needs.
pub struct Plan {
    items: Vec<Item>,
    filters: Vec<Filter>,
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
                let mut items = Vec::new();
                let mut filters = Vec::new();
                let bound = collect(inner, &mut items, &mut filters)?;
                return finish(
                    items,
                    filters,
                    &bound,
                    variables.clone(),
                    distinct,
                    offset,
                    limit,
                );
            }
            _ => return None,
        }
    }
}

/// Walks the pattern tree into a flat list of items, hoisting filters as it goes.
///
/// Returns the variables the subtree binds, or `None` if anything in it is outside the
/// fragment. The return value is what makes filter hoisting safe — see below.
fn collect(
    pattern: &GraphPattern,
    items: &mut Vec<Item>,
    filters: &mut Vec<Filter>,
) -> Option<Bound> {
    match pattern {
        GraphPattern::Bgp { patterns } => {
            let mut bound = Bound::default();
            for triple in patterns {
                if !usable_pattern(triple) {
                    return None;
                }
                collect_variables(triple, &mut bound.certain);
                items.push(Item::Pattern(triple.clone()));
            }
            bound.possible.clone_from(&bound.certain);
            Some(bound)
        }

        // A join of conjunctions is a conjunction, so the two sides simply concatenate. The
        // nested loop then orders the whole lot together rather than respecting a tree shape
        // that carries no information about cost.
        GraphPattern::Join { left, right } => {
            let a = collect(left, items, filters)?;
            let b = collect(right, items, filters)?;
            Some(a.joined(b))
        }

        // Hoisting a filter out of a join is only sound when every variable it names is
        // bound *inside the subtree it was written against*. `{ ?a ?b ?c FILTER(?d = 1) }
        // { ?d ?e ?f }` is the counter-example: `?d` is unbound where the filter sits, so
        // the filter errors and eliminates everything, while the same filter applied after
        // the join would see `?d` bound and could pass. Requiring the variables to be
        // certainly bound in the subtree makes the two orders agree, because a compatible
        // join cannot change the value of a variable the subtree already fixed.
        GraphPattern::Filter { expr, inner } => {
            let bound = collect(inner, items, filters)?;
            let mut conjuncts = Vec::new();
            flatten_conjunction(expr, &mut conjuncts);
            for conjunct in conjuncts {
                let mut needs = Vec::new();
                if !inspect_expression(conjunct, &mut needs) {
                    return None;
                }
                if !needs.iter().all(|v| bound.certain.contains(v)) {
                    return None;
                }
                let expression = to_sparopt(conjunct)?;
                filters.push(Filter { expression, needs });
            }
            Some(bound)
        }

        GraphPattern::Union { .. } => {
            let mut branches = Vec::new();
            let bound = union_branches(pattern, &mut branches)?;
            items.push(Item::Union(branches));
            Some(bound)
        }

        GraphPattern::Values {
            variables,
            bindings,
        } => {
            let mut bound = Bound {
                certain: Vec::new(),
                possible: variables.clone(),
            };
            // `UNDEF` in even one row makes the variable only *possibly* bound, which is
            // what stops a filter naming it from being hoisted above the `VALUES`.
            for (column, variable) in variables.iter().enumerate() {
                if bindings
                    .iter()
                    .all(|row| row.get(column).is_some_and(Option::is_some))
                {
                    bound.certain.push(variable.clone());
                }
            }
            items.push(Item::Values(ValuesItem {
                variables: variables.clone(),
                rows: bindings.clone(),
            }));
            Some(bound)
        }

        _ => None,
    }
}

/// Flattens nested `UNION`s into a list of alternatives, each of which must be a plain BGP.
///
/// A branch containing anything else — a filter, a nested join — ends the fragment. That is
/// narrower than SPARQL allows and wide enough for what produces unions here.
fn union_branches(pattern: &GraphPattern, out: &mut Vec<Vec<TriplePattern>>) -> Option<Bound> {
    match pattern {
        GraphPattern::Union { left, right } => {
            let a = union_branches(left, out)?;
            let b = union_branches(right, out)?;
            Some(a.alternated(b))
        }
        GraphPattern::Bgp { patterns } => {
            if patterns.is_empty() {
                return None;
            }
            let mut bound = Bound::default();
            for triple in patterns {
                if !usable_pattern(triple) {
                    return None;
                }
                collect_variables(triple, &mut bound.certain);
            }
            bound.possible.clone_from(&bound.certain);
            out.push(patterns.clone());
            Some(bound)
        }
        _ => None,
    }
}

/// Whether a triple pattern is one this path can probe with.
///
/// A blank node behaves as a variable that cannot be projected, and an RDF 1.2 triple term
/// needs the term encoding's side table. Neither is modelled here, and both are rare in the
/// shapes this exists for.
fn usable_pattern(triple: &TriplePattern) -> bool {
    for term in [&triple.subject, &triple.object] {
        if matches!(term, TermPattern::BlankNode(_) | TermPattern::Triple(_)) {
            return false;
        }
    }
    true
}

/// Splits `A && B` into its conjuncts.
///
/// A conjunction can only be applied once the later of its two halves is ready, so leaving
/// it whole delays the half that was ready earlier. Splitting is what lets each half be
/// pushed to the depth it actually belongs at.
fn flatten_conjunction<'e>(expr: &'e Expression, out: &mut Vec<&'e Expression>) {
    if let Expression::And(left, right) = expr {
        flatten_conjunction(left, out);
        flatten_conjunction(right, out);
    } else {
        out.push(expr);
    }
}

/// Collects the variables an expression mentions, and reports whether it can be evaluated
/// here at all.
///
/// Returns `false` for the two classes described in the module documentation: `EXISTS`,
/// which reads a dataset the borrowed evaluator does not have, and the context-dependent
/// builtins. Both would otherwise fail quietly — `EXISTS` by answering `false` everywhere —
/// which is worse than refusing the query.
fn inspect_expression(expr: &Expression, out: &mut Vec<Variable>) -> bool {
    match expr {
        Expression::NamedNode(_) | Expression::Literal(_) => true,
        Expression::Variable(v) | Expression::Bound(v) => {
            if !out.contains(v) {
                out.push(v.clone());
            }
            true
        }
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => inspect_expression(a, out) && inspect_expression(b, out),
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            inspect_expression(a, out)
        }
        Expression::If(a, b, c) => {
            inspect_expression(a, out) && inspect_expression(b, out) && inspect_expression(c, out)
        }
        Expression::In(a, list) => {
            inspect_expression(a, out) && list.iter().all(|e| inspect_expression(e, out))
        }
        Expression::Coalesce(list) => list.iter().all(|e| inspect_expression(e, out)),
        Expression::FunctionCall(function, args) => {
            // Custom functions — `geof:sfWithin` and the rest of §17 — are deliberately
            // allowed: they are registered on the same evaluator, so they behave here
            // exactly as they do anywhere else.
            if matches!(
                function,
                Function::Now
                    | Function::Rand
                    | Function::Uuid
                    | Function::StrUuid
                    | Function::BNode
            ) {
                return false;
            }
            args.iter().all(|e| inspect_expression(e, out))
        }
        Expression::Exists(_) => false,
    }
}

/// Converts one expression into the form the evaluator's public entry point takes.
///
/// There is no public conversion for a bare expression, so it travels inside a `FILTER` and
/// is taken out the other side. Roundabout, but it is upstream's own conversion rather than
/// a second copy of it, which is the property worth having.
fn to_sparopt(expr: &Expression) -> Option<sparopt::algebra::Expression> {
    let wrapper = GraphPattern::Filter {
        expr: expr.clone(),
        inner: Box::new(GraphPattern::Bgp {
            patterns: vec![TriplePattern {
                subject: TermPattern::Variable(Variable::new_unchecked("s")),
                predicate: spargebra::term::NamedNodePattern::Variable(Variable::new_unchecked(
                    "p",
                )),
                object: TermPattern::Variable(Variable::new_unchecked("o")),
            }],
        }),
    };
    match sparopt::algebra::GraphPattern::from(&wrapper) {
        sparopt::algebra::GraphPattern::Filter { expression, .. } => Some(expression),
        _ => None,
    }
}

fn finish(
    items: Vec<Item>,
    filters: Vec<Filter>,
    bound: &Bound,
    variables: Vec<Variable>,
    distinct: bool,
    offset: usize,
    limit: Option<usize>,
) -> Option<Plan> {
    if items.is_empty() {
        return None;
    }
    // A projected variable no part of the plan binds would come back unbound on every row.
    // That is what SPARQL says should happen, and it is also a sign the query was not
    // understood, so it goes back to the evaluator rather than being answered here.
    if !variables.iter().all(|v| bound.possible.contains(v)) {
        return None;
    }
    Some(Plan {
        items,
        filters,
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
            evaluator: crate::Engine::evaluator(),
            out: Vec::new(),
            seen: rustc_hash::FxHashSet::default(),
            skipped: 0,
            limits,
            abandoned: false,
            since_check: 0,
        };

        let todo: Vec<Todo<'_>> = self.items.iter().map(Todo::from).collect();
        let pending: Vec<usize> = (0..self.filters.len()).collect();
        self.step(view, stats, &todo, &pending, &mut bindings, &mut run)?;

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
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        todo: &[Todo<'_>],
        bindings: &FxHashMap<&Variable, TermId>,
    ) -> Option<usize> {
        (0..todo.len()).min_by(|a, b| {
            let ca = estimate_todo(&todo[*a], view, bindings, stats);
            let cb = estimate_todo(&todo[*b], view, bindings, stats);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn step<'p>(
        &'p self,
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        todo: &[Todo<'p>],
        pending: &[usize],
        bindings: &mut FxHashMap<&'p Variable, TermId>,
        run: &mut Run<'_>,
    ) -> Result<(), ViewError> {
        if run.done() {
            return Ok(());
        }

        // Any filter whose variables are all bound is applied now, before the remaining
        // patterns are scanned. Failing one prunes this branch entirely, which is where the
        // saving is — a filter applied after the join has already paid for the join.
        //
        // At the deepest frame every variable is bound, so `still_pending` is empty there
        // and no filter can reach the row that follows unapplied.
        let mut still_pending = Vec::with_capacity(pending.len());
        for &index in pending {
            let filter = &self.filters[index];
            if filter.needs.iter().all(|v| bindings.contains_key(v)) {
                if !Self::passes(filter, bindings, view, &run.evaluator)? {
                    return Ok(());
                }
            } else {
                still_pending.push(index);
            }
        }
        let pending = &still_pending[..];
        if let Some(limit) = self.limit {
            if run.out.len() >= limit {
                // Stopping here is what makes the work proportional to the answer rather
                // than to the store. It is the whole reason for the operator.
                return Ok(());
            }
        }
        let Some(index) = Self::cheapest(view, stats, todo, bindings) else {
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

        // Everything except the chosen entry, which is what the next depth still has to do.
        let rest: Vec<Todo<'p>> = todo
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != index)
            .map(|(_, entry)| *entry)
            .collect();

        match todo[index] {
            Todo::Pattern(triple) => {
                self.probe(view, stats, triple, &rest, pending, bindings, run)?;
            }

            // Each alternative is tried in turn, with its own patterns prepended to whatever
            // was left. Bindings made inside a branch are undone by the frames that made
            // them, so the next branch starts from the same place this one did — which is
            // what makes this a union rather than a join.
            Todo::Union(branches) => {
                for branch in branches {
                    let mut next: Vec<Todo<'p>> = branch.iter().map(Todo::Pattern).collect();
                    next.extend_from_slice(&rest);
                    self.step(view, stats, &next, pending, bindings, run)?;
                    if run.abandoned {
                        break;
                    }
                    if let Some(limit) = self.limit {
                        if run.out.len() >= limit {
                            break;
                        }
                    }
                }
            }

            Todo::Values(values) => {
                self.rows_of(view, stats, values, &rest, pending, bindings, run)?;
            }
        }
        Ok(())
    }

    /// One triple pattern: resolve what is already known, then scan what is not.
    #[allow(clippy::too_many_arguments)]
    fn probe<'p>(
        &'p self,
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        triple: &'p TriplePattern,
        rest: &[Todo<'p>],
        pending: &[usize],
        bindings: &mut FxHashMap<&'p Variable, TermId>,
        run: &mut Run<'_>,
    ) -> Result<(), ViewError> {
        // A constant absent from the dictionary kills the branch before any scan.
        let Some((subject, predicate, object)) = Self::resolve(triple, view, bindings)? else {
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
            self.step(view, stats, rest, pending, bindings, run)?;
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

    /// One `VALUES` clause: each row is a set of bindings to try.
    ///
    /// A row whose term the dictionary has never seen is the awkward case. Such a term still
    /// *binds* — `VALUES ?x { <urn:absent> }` is one solution, not none — but representing it
    /// needs a term id that does not exist, and interning one would be a write in the middle
    /// of a read. So the whole query goes back to the evaluator instead. It costs nothing
    /// where this matters: the `VALUES` §17 generates holds geometries read out of the store,
    /// which are interned by construction.
    #[allow(clippy::too_many_arguments)]
    fn rows_of<'p>(
        &'p self,
        view: &DatasetView<'_>,
        stats: Option<&Statistics>,
        values: &'p ValuesItem,
        rest: &[Todo<'p>],
        pending: &[usize],
        bindings: &mut FxHashMap<&'p Variable, TermId>,
        run: &mut Run<'_>,
    ) -> Result<(), ViewError> {
        for row in &values.rows {
            if run.done() {
                break;
            }
            let mut added: Vec<&'p Variable> = Vec::new();
            let mut compatible = true;
            for (variable, term) in values.variables.iter().zip(row) {
                // `UNDEF` binds nothing, which is exactly what leaving it out does.
                let Some(term) = term else { continue };
                let term: oxrdf::Term = term.clone().into();
                match lookup(view, term.as_ref())? {
                    Slot::Fixed(id) => {
                        if !bind_variable(variable, id, bindings, &mut added) {
                            compatible = false;
                            break;
                        }
                    }
                    Slot::Missing => {
                        for variable in added {
                            bindings.remove(variable);
                        }
                        run.abandoned = true;
                        return Ok(());
                    }
                    Slot::Any => unreachable!("a ground term is never a wildcard"),
                }
            }
            if compatible {
                self.step(view, stats, rest, pending, bindings, run)?;
            }
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

    /// Whether a solution satisfies one filter.
    ///
    /// The expression is evaluated by `spareval`, so the answer is the evaluator's answer. Only
    /// the variables the filter mentions are decoded — a dictionary lookup per variable per row
    /// is the cost of this feature, and there is no reason to pay it for columns the predicate
    /// never reads.
    ///
    /// `None` from the evaluator means the expression raised an error or has no effective
    /// boolean value. SPARQL's rule for both is that the solution is eliminated, not that the
    /// query fails, so both become `false`.
    fn passes(
        filter: &Filter,
        bindings: &FxHashMap<&Variable, TermId>,
        view: &DatasetView<'_>,
        evaluator: &spareval::QueryEvaluator,
    ) -> Result<bool, ViewError> {
        let mut substitutions = Vec::with_capacity(filter.needs.len());
        for variable in &filter.needs {
            let Some(id) = bindings.get(variable) else {
                return Ok(false);
            };
            let Some(term) = view.store().decode_term(*id)? else {
                return Ok(false);
            };
            substitutions.push((variable, term));
        }
        Ok(evaluator
            .evaluate_effective_boolean_value_expression(&filter.expression, substitutions)
            .unwrap_or(false))
    }

    /// The concrete term ids a pattern is constrained to, given what is already bound.
    ///
    /// `None` for the whole triple means a constant in it is absent from the dictionary, so
    /// no quad can match and the branch is dead. That is a common and cheap win: a query
    /// naming a predicate the store has never seen returns immediately.
    fn resolve(
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

/// Binds a variable to a term id, or reports that it is already bound to something else.
///
/// The second case is what makes a repeated variable mean equality rather than two
/// independent bindings.
fn bind_variable<'p>(
    variable: &'p Variable,
    id: TermId,
    bindings: &mut FxHashMap<&'p Variable, TermId>,
    added: &mut Vec<&'p Variable>,
) -> bool {
    match bindings.get(variable) {
        Some(existing) => *existing == id,
        None => {
            bindings.insert(variable, id);
            added.push(variable);
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
/// What one entry of the todo list is likely to cost, given what is already bound.
///
/// Rough on purpose: this decides an order, and a wrong order is slow rather than wrong. A
/// `VALUES` is the one exact number here, which is why a handful of candidate geometries
/// reliably sorts ahead of a scan — the behaviour the spatial index exists to produce.
fn estimate_todo(
    todo: &Todo<'_>,
    view: &DatasetView<'_>,
    bindings: &FxHashMap<&Variable, TermId>,
    stats: Option<&Statistics>,
) -> f64 {
    match todo {
        Todo::Pattern(triple) => estimate(triple, view, bindings, stats),
        // Each branch yields at most what its most selective pattern does, and the union
        // yields the sum of the branches.
        Todo::Union(branches) => branches
            .iter()
            .map(|branch| {
                branch
                    .iter()
                    .map(|triple| estimate(triple, view, bindings, stats))
                    .fold(f64::INFINITY, f64::min)
            })
            .sum(),
        Todo::Values(values) => values.rows.len() as f64,
    }
}

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
