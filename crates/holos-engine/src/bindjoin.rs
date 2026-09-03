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
use oxrdf::NamedNode;
use rustc_hash::FxHashMap;
use spareval::{CancellationToken, QueryableDataset};
use spargebra::algebra::{Expression, Function, GraphPattern, PropertyPathExpression};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern, Variable};
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
    /// How many times the todo list has been exhausted — a *match*, whether or not the row
    /// that followed survived `DISTINCT` or `OFFSET`.
    ///
    /// This is what an `OPTIONAL` reads to decide whether it matched. Counting surviving rows
    /// instead would make a duplicate look like a failure to match, and hand back unbound
    /// variables for a match that happened.
    completions: usize,
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

/// What an [`OptionalItem`] does with a match.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OnMatch {
    /// `OPTIONAL`: a match extends the row, and no match leaves the variables unbound.
    Extend,
    /// `MINUS`: a match *removes* the row, and no match keeps it.
    ///
    /// The same traversal answers both — "did the right side match this left row" — so they
    /// share a structure and differ in what they do with the answer.
    Remove,
}

/// One `OPTIONAL` or `MINUS`, kept whole rather than flattened into the surrounding
/// conjunction.
///
/// Neither commutes with a join, so their items cannot join the todo list and be reordered
/// among the required ones: `(A ⟕ B) ⋈ C` and `(A ⋈ C) ⟕ B` are different queries in general.
/// Keeping one as a single entry, evaluated only once everything required is bound, is what
/// makes the two agree — together with the check in [`well_designed`], which refuses the
/// query outright when they would not.
///
/// The two share a structure because they ask the same question — did the right side match
/// this left row — and differ only in what they do with the answer. See [`OnMatch`].
pub struct OptionalItem {
    /// Whether a match extends the row or removes it.
    mode: OnMatch,
    /// What the optional matches, ordered by cost among themselves like any other items.
    items: Vec<Item>,
    /// Indices into the plan's filters: those written inside the optional, plus the left
    /// join's own condition, which is part of the match rather than a test applied after it.
    filters: Vec<usize>,
    /// Variables this optional binds that nothing outside it does.
    ///
    /// These are the ones that come back unbound when it does not match, and the ones
    /// [`well_designed`] checks nothing else reads.
    fresh: Vec<Variable>,
}

/// Which graph a pattern matches against.
///
/// Carried per pattern rather than per plan, because `GRAPH` scopes a *block* and a query may
/// hold several — including patterns outside any of them, which stay on the default graph.
#[derive(Clone, PartialEq, Eq)]
pub enum Scope {
    /// Outside any `GRAPH`: the dataset's default graph.
    Default,
    /// `GRAPH <g> { .. }` — one named graph, known before the scan.
    Named(NamedNode),
    /// `GRAPH ?g { .. }` — every named graph, binding `?g` to whichever matched.
    ///
    /// Once `?g` is bound, a later pattern in the same block resolves it and scans that one
    /// graph instead of all of them, which is the bind join's whole trick applied to the
    /// graph position.
    Variable(Variable),
}

/// A triple pattern together with the graph it is matched in.
pub struct PatternItem {
    triple: TriplePattern,
    scope: Scope,
}

/// One step of a plan: something that binds variables.
pub enum Item {
    /// A single triple pattern, probed against the store's indexes.
    Pattern(PatternItem),
    /// `UNION`, flattened to a list of alternatives, each a conjunction of patterns.
    Union(Vec<Vec<PatternItem>>),
    /// `VALUES`: rows of terms bound directly rather than looked up.
    Values(ValuesItem),
    /// `OPTIONAL { .. }`, evaluated after everything required.
    Optional(Box<OptionalItem>),
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
    Pattern(&'p PatternItem),
    Union(&'p [Vec<PatternItem>]),
    Values(&'p ValuesItem),
    Optional(&'p OptionalItem),
}

impl<'p> From<&'p Item> for Todo<'p> {
    fn from(item: &'p Item) -> Self {
        match item {
            Item::Pattern(triple) => Todo::Pattern(triple),
            Item::Union(branches) => Todo::Union(branches),
            Item::Values(values) => Todo::Values(values),
            Item::Optional(optional) => Todo::Optional(optional),
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
                let bound = collect(inner, &Scope::Default, &mut items, &mut filters)?;
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
    scope: &Scope,
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
                let triple = without_blank_nodes(triple);
                collect_variables(&triple, &mut bound.certain);
                if let Scope::Variable(g) = scope {
                    if !bound.certain.contains(g) {
                        bound.certain.push(g.clone());
                    }
                }
                items.push(Item::Pattern(PatternItem {
                    triple,
                    scope: scope.clone(),
                }));
            }
            bound.possible.clone_from(&bound.certain);
            Some(bound)
        }

        // A subquery. SPARQL evaluates one bottom-up and joins its *projected* solutions,
        // so flattening its patterns into the surrounding conjunction is only the same thing
        // when the projection hides nothing: a variable the subquery binds and does not
        // project is invisible outside it, and flattening would expose it to join against an
        // outer variable that happens to share the name.
        //
        // So a subquery projecting everything it binds is spliced in — which is what
        // `{ SELECT * WHERE { .. } }` always is, and what braces used for readability
        // produce — and one that hides a variable is refused.
        //
        // Widening this means renaming the hidden variables to something unique before
        // splicing, at which point a collision cannot arise. That is a real fix rather than
        // a wider guess, and it is not done here.
        //
        // A subquery carrying its own `LIMIT`, `DISTINCT` or `ORDER BY` arrives wrapped in a
        // `Slice`, `Distinct` or `OrderBy` rather than a bare `Project`, and falls through to
        // the refusal below: those change how many solutions there are, which no join does.
        GraphPattern::Project { inner, variables } => {
            let mut inner_items = Vec::new();
            let mut inner_filters = Vec::new();
            let bound = collect(inner, scope, &mut inner_items, &mut inner_filters)?;
            if !bound.possible.iter().all(|v| variables.contains(v)) {
                return None;
            }
            items.append(&mut inner_items);
            filters.append(&mut inner_filters);
            Some(bound)
        }

        // `MINUS`. The traversal is the optional's — does the right side match this left row
        // — and only the verdict differs: a match removes the row instead of extending it.
        //
        // SPARQL removes a solution only when a compatible one exists on the right *and the
        // two share a variable*: with disjoint domains `MINUS` never removes anything. That
        // is decided statically here, and a right side sharing nothing certainly bound on the
        // left is refused rather than reasoned about — the shared variable might be one an
        // optional left unbound, and then whether the row is removed depends on the data.
        GraphPattern::Minus { left, right } => {
            let outer = collect(left, scope, items, filters)?;

            let mut inner_items = Vec::new();
            let mut inner_filters = Vec::new();
            let inner = collect(right, scope, &mut inner_items, &mut inner_filters)?;

            if !inner.possible.iter().any(|v| outer.certain.contains(v)) {
                return None;
            }

            let first = filters.len();
            filters.extend(inner_filters);
            let indices: Vec<usize> = (first..filters.len()).collect();

            let fresh: Vec<Variable> = inner
                .possible
                .iter()
                .filter(|v| !outer.possible.contains(v))
                .cloned()
                .collect();

            items.push(Item::Optional(Box::new(OptionalItem {
                mode: OnMatch::Remove,
                items: inner_items,
                filters: indices,
                fresh,
            })));

            // A `MINUS` binds nothing: it only takes rows away. Its right side's variables
            // stay out of the outer scope entirely, which is what the specification means by
            // the two being evaluated over separate domains.
            //
            // The consequence is a refusal rather than a wrong answer: a query projecting a
            // variable only the `MINUS` mentions finds it missing from `possible` and goes to
            // the evaluator, which returns it unbound. Reporting it as possible would let
            // this operator answer that query — also correctly, since the projection would
            // find it unbound too — but it would be saying something false about scope to buy
            // a fast path for a query nobody writes.
            Some(outer)
        }

        // An alternative property path is a union of the branches, which this already has a
        // shape for. The closure paths — `*`, `+`, `?` — and a negated set are refused: they
        // need a traversal to a fixpoint that this operator does not have, and approximating
        // one would answer a different query.
        //
        // Sequence and inverse paths never arrive here: the parser desugars `:p/:q` into a
        // BGP joined on a blank node and `^:p` into a swapped pattern, so both are ordinary
        // patterns by the time this sees them.
        GraphPattern::Path {
            subject,
            path,
            object,
        } => {
            let mut branches = Vec::new();
            path_alternatives(path, subject, object, scope, &mut branches)?;
            let mut bound = Bound::default();
            for branch in &branches {
                let mut here = Bound::default();
                for pattern in branch {
                    collect_variables(&pattern.triple, &mut here.certain);
                }
                here.possible.clone_from(&here.certain);
                bound = if bound.possible.is_empty() {
                    here
                } else {
                    bound.alternated(here)
                };
            }
            items.push(Item::Union(branches));
            Some(bound)
        }

        // `GRAPH`. The block's patterns are tagged with the graph and then join the flat
        // todo list like any others, so they can still be ordered against patterns outside
        // the block — which is the point, since a selective pattern in the default graph
        // should be able to drive a scan of a named one.
        //
        // A nested `GRAPH` re-scopes its block, and this refuses rather than implementing
        // that: it is rare, and getting the inner-overrides-outer rule subtly wrong would
        // answer against the wrong graph, which is the one failure this operator must not
        // have.
        GraphPattern::Graph { name, inner } => {
            if !matches!(scope, Scope::Default) {
                return None;
            }
            let inner_scope = match name {
                NamedNodePattern::NamedNode(g) => Scope::Named(g.clone()),
                NamedNodePattern::Variable(v) => Scope::Variable(v.clone()),
            };
            collect(inner, &inner_scope, items, filters)
        }

        // A join of conjunctions is a conjunction, so the two sides simply concatenate. The
        // nested loop then orders the whole lot together rather than respecting a tree shape
        // that carries no information about cost.
        GraphPattern::Join { left, right } => {
            let a = collect(left, scope, items, filters)?;
            let b = collect(right, scope, items, filters)?;
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
            let bound = collect(inner, scope, items, filters)?;
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
            let bound = union_branches(pattern, scope, &mut branches)?;
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
            let _ = scope;
            items.push(Item::Values(ValuesItem {
                variables: variables.clone(),
                rows: bindings.clone(),
            }));
            Some(bound)
        }

        // `OPTIONAL`. The left side joins the surrounding conjunction as usual; the right
        // is kept whole, because a left join does not commute with a join.
        //
        // The right side's own filters and the left join's condition are the same thing to
        // this operator — both decide whether the optional *matched*, and neither may be
        // applied after the fact. A filter that failed after the match would drop the row
        // instead of leaving the optional's variables unbound, which is a different answer.
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            let outer = collect(left, scope, items, filters)?;

            let mut inner_items = Vec::new();
            let mut inner_filters = Vec::new();
            let inner = collect(right, scope, &mut inner_items, &mut inner_filters)?;

            if let Some(expression) = expression {
                let mut conjuncts = Vec::new();
                flatten_conjunction(expression, &mut conjuncts);
                for conjunct in conjuncts {
                    let mut needs = Vec::new();
                    if !inspect_expression(conjunct, &mut needs) {
                        return None;
                    }
                    // The condition may read the left side as well as the right, so what it
                    // needs is bound by the pair rather than by the optional alone.
                    if !needs
                        .iter()
                        .all(|v| inner.certain.contains(v) || outer.certain.contains(v))
                    {
                        return None;
                    }
                    inner_filters.push(Filter {
                        expression: to_sparopt(conjunct)?,
                        needs,
                    });
                }
            }

            let fresh: Vec<Variable> = inner
                .possible
                .iter()
                .filter(|v| !outer.possible.contains(v))
                .cloned()
                .collect();

            let first = filters.len();
            filters.extend(inner_filters);
            let indices: Vec<usize> = (first..filters.len()).collect();

            items.push(Item::Optional(Box::new(OptionalItem {
                mode: OnMatch::Extend,
                items: inner_items,
                filters: indices,
                fresh,
            })));

            // Nothing the optional binds is *certain*: that is the whole meaning of the
            // word, and it is what stops an outer filter naming one of its variables from
            // being hoisted above it.
            let mut bound = outer;
            for v in inner.possible {
                if !bound.possible.contains(&v) {
                    bound.possible.push(v);
                }
            }
            Some(bound)
        }

        _ => None,
    }
}

/// Whether hoisting every `OPTIONAL` to the end preserves the query's meaning.
///
/// The nested loop orders items by cost and evaluates optionals last, which turns
/// `(A ⟕ B) ⋈ C` into `(A ⋈ C) ⟕ B`. Those agree exactly when nothing outside an optional
/// reads a variable only that optional binds — the well-designedness condition, and the
/// reason `OPTIONAL` is famous for not composing.
///
/// `{ ?a :p ?b OPTIONAL { ?a :q ?c } . ?c :r ?d }` is the counter-example: `?c` escapes the
/// optional into a required pattern, so evaluating the optional last would join against a
/// binding the original query never had. Refused rather than answered differently.
fn well_designed(items: &[Item], filters: &[Filter]) -> bool {
    let mut optionals = Vec::new();
    gather_optionals(items, &mut optionals);

    for (position, optional) in optionals.iter().enumerate() {
        // Everything else in the query: the other items, and every filter that is not this
        // optional's own.
        let mut elsewhere = Vec::new();
        for (other, item) in optionals.iter().enumerate() {
            if other != position {
                variables_of_items(&item.items, &mut elsewhere);
            }
        }
        collect_outside(items, optional, &mut elsewhere);
        for (index, filter) in filters.iter().enumerate() {
            if !optional.filters.contains(&index) {
                elsewhere.extend(filter.needs.iter().cloned());
            }
        }
        if optional.fresh.iter().any(|v| elsewhere.contains(v)) {
            return false;
        }
    }
    true
}

fn gather_optionals<'a>(items: &'a [Item], out: &mut Vec<&'a OptionalItem>) {
    for item in items {
        if let Item::Optional(optional) = item {
            out.push(optional);
            gather_optionals(&optional.items, out);
        }
    }
}

/// Every variable the items mention, except inside `skip`.
fn collect_outside(items: &[Item], skip: &OptionalItem, out: &mut Vec<Variable>) {
    for item in items {
        match item {
            Item::Optional(optional) => {
                if !std::ptr::eq(optional.as_ref(), skip) {
                    collect_outside(&optional.items, skip, out);
                }
            }
            other => variables_of_items(std::slice::from_ref(other), out),
        }
    }
}

fn variables_of_items(items: &[Item], out: &mut Vec<Variable>) {
    for item in items {
        match item {
            Item::Pattern(pattern) => variables_of_pattern(pattern, out),
            Item::Union(branches) => {
                for branch in branches {
                    for pattern in branch {
                        variables_of_pattern(pattern, out);
                    }
                }
            }
            Item::Values(values) => out.extend(values.variables.iter().cloned()),
            Item::Optional(optional) => variables_of_items(&optional.items, out),
        }
    }
}

/// Flattens an alternative property path into union branches.
///
/// Only `a|b` and a bare predicate. Everything else — `*`, `+`, `?`, a negated set — needs a
/// fixpoint traversal, and is refused rather than approximated.
fn path_alternatives(
    path: &PropertyPathExpression,
    subject: &TermPattern,
    object: &TermPattern,
    scope: &Scope,
    out: &mut Vec<Vec<PatternItem>>,
) -> Option<()> {
    match path {
        PropertyPathExpression::NamedNode(p) => {
            let triple = TriplePattern {
                subject: match subject {
                    TermPattern::Variable(v) => v.clone().into(),
                    TermPattern::NamedNode(n) => n.clone().into(),
                    TermPattern::BlankNode(b) => blank_as_variable(b.as_str()).into(),
                    TermPattern::Literal(_) | TermPattern::Triple(_) => return None,
                },
                predicate: NamedNodePattern::NamedNode(p.clone()),
                object: match object {
                    TermPattern::BlankNode(b) => {
                        TermPattern::Variable(blank_as_variable(b.as_str()))
                    }
                    TermPattern::Triple(_) => return None,
                    other => other.clone(),
                },
            };
            out.push(vec![PatternItem {
                triple,
                scope: scope.clone(),
            }]);
            Some(())
        }
        PropertyPathExpression::Alternative(a, b) => {
            path_alternatives(a, subject, object, scope, out)?;
            path_alternatives(b, subject, object, scope, out)
        }
        _ => None,
    }
}

/// Flattens nested `UNION`s into a list of alternatives, each of which must be a plain BGP.
///
/// A branch containing anything else — a filter, a nested join — ends the fragment. That is
/// narrower than SPARQL allows and wide enough for what produces unions here.
fn union_branches(
    pattern: &GraphPattern,
    scope: &Scope,
    out: &mut Vec<Vec<PatternItem>>,
) -> Option<Bound> {
    match pattern {
        GraphPattern::Union { left, right } => {
            let a = union_branches(left, scope, out)?;
            let b = union_branches(right, scope, out)?;
            Some(a.alternated(b))
        }
        // A `GRAPH` block as a union branch re-scopes that branch, which is common enough to
        // be worth having: `{ GRAPH <a> { .. } } UNION { GRAPH <b> { .. } }`.
        GraphPattern::Graph { name, inner } => {
            if !matches!(scope, Scope::Default) {
                return None;
            }
            let inner_scope = match name {
                NamedNodePattern::NamedNode(g) => Scope::Named(g.clone()),
                NamedNodePattern::Variable(v) => Scope::Variable(v.clone()),
            };
            union_branches(inner, &inner_scope, out)
        }
        GraphPattern::Bgp { patterns } => {
            if patterns.is_empty() {
                return None;
            }
            let mut bound = Bound::default();
            let renamed: Vec<TriplePattern> = patterns
                .iter()
                .map(|triple| usable_pattern(triple).then(|| without_blank_nodes(triple)))
                .collect::<Option<_>>()?;
            for triple in &renamed {
                collect_variables(triple, &mut bound.certain);
            }
            if let Scope::Variable(g) = scope {
                if !bound.certain.contains(g) {
                    bound.certain.push(g.clone());
                }
            }
            bound.possible.clone_from(&bound.certain);
            out.push(
                renamed
                    .into_iter()
                    .map(|triple| PatternItem {
                        triple,
                        scope: scope.clone(),
                    })
                    .collect(),
            );
            Some(bound)
        }
        _ => None,
    }
}

/// A blank node in a query pattern, as the variable it behaves like.
///
/// SPARQL gives a blank node in a pattern the same role as a variable, minus the ability to
/// be projected — and since the only thing this operator does with a variable is bind it and
/// look it up, that is a rename rather than a feature.
///
/// It matters more than it sounds. The parser desugars a *sequence path* into a BGP joined on
/// an anonymous blank node, so `?s :p/:q ?o` was being refused for having a blank node in it
/// rather than for being a path. Renaming brings sequence paths in with it, along with the
/// `[ ]` syntax that means the same thing.
///
/// The name is deliberately not valid SPARQL — a space cannot appear in a variable — so it
/// cannot collide with one the query wrote. `?s :p _:x . ?y :q ?x` is the case that needs it:
/// `_:x` and `?x` are different things, and a rename that mapped them together would join
/// them.
fn blank_as_variable(label: &str) -> Variable {
    Variable::new_unchecked(format!("bnode {label}"))
}

/// A pattern with its blank nodes renamed to variables.
fn without_blank_nodes(triple: &TriplePattern) -> TriplePattern {
    let rename = |term: &TermPattern| -> TermPattern {
        match term {
            TermPattern::BlankNode(b) => TermPattern::Variable(blank_as_variable(b.as_str())),
            other => other.clone(),
        }
    };
    TriplePattern {
        subject: match rename(&triple.subject) {
            TermPattern::Variable(v) => v.into(),
            TermPattern::NamedNode(n) => n.into(),
            // Neither a literal nor a triple term can be a subject, and `usable_pattern` has
            // already refused the latter.
            other => unreachable!("{other} cannot be a subject"),
        },
        predicate: triple.predicate.clone(),
        object: rename(&triple.object),
    }
}

/// Whether a triple pattern is one this path can probe with.
///
/// An RDF 1.2 triple term needs the term encoding's side table, which is not modelled here.
/// Blank nodes are fine: [`without_blank_nodes`] turns them into the variables they behave
/// like before anything else sees them.
fn usable_pattern(triple: &TriplePattern) -> bool {
    for term in [&triple.subject, &triple.object] {
        if matches!(term, TermPattern::Triple(_)) {
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
    // Optionals are evaluated last, which is only sound for a well-designed pattern.
    if !well_designed(&items, &filters) {
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

/// Every variable a pattern mentions, the graph position included.
///
/// `GRAPH ?g` binds `?g` as surely as the triple binds its own variables, and a walk that
/// missed it would let `?g` be projected without anything appearing to bind it.
fn variables_of_pattern(pattern: &PatternItem, out: &mut Vec<Variable>) {
    collect_variables(&pattern.triple, out);
    if let Scope::Variable(g) = &pattern.scope {
        if !out.contains(g) {
            out.push(g.clone());
        }
    }
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
            completions: 0,
            limits,
            abandoned: false,
            since_check: 0,
        };

        let todo: Vec<Todo<'_>> = self.items.iter().map(Todo::from).collect();

        // Every filter *except* those belonging to an optional. Theirs decide whether the
        // optional matched and are added when it runs; starting them pending would apply
        // them to the whole row, and a row failing one would vanish instead of coming back
        // with the optional's variables unbound. `OPTIONAL { ?s :city ?c FILTER(?age < 35) }`
        // is the case: a person over 35 has no city *in this optional*, not no row.
        let owned = self.owned_by_optionals();
        let pending: Vec<usize> = (0..self.filters.len())
            .filter(|i| !owned.contains(i))
            .collect();
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
        // Optionals do not take part in the ordering. Two reasons, and both are about
        // meaning rather than cost:
        //
        // * an optional must see the whole required conjunction bound, because it is a left
        //   join over it and not a conjunct within it;
        // * among themselves they keep source order, because `(A ⟕ B) ⟕ C` and
        //   `(A ⟕ C) ⟕ B` differ once `C` reads what `B` binds.
        //
        // So the first optional runs only when nothing required is left, and cost never
        // enters into it. `estimate_todo` answers `INFINITY` for an optional as a second
        // line of the same defence; neither alone is relied on.
        let required: Vec<usize> = (0..todo.len())
            .filter(|i| !matches!(todo[*i], Todo::Optional(_)))
            .collect();
        if required.is_empty() {
            return (0..todo.len()).next();
        }
        required.into_iter().min_by(|a, b| {
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
            run.completions += 1;
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
            Todo::Pattern(pattern) => {
                self.probe(view, stats, pattern, &rest, pending, bindings, run)?;
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

            // A left join, in the one shape this fragment allows: everything required is
            // already bound, so what remains is to try the optional's own items and, if
            // none of them matched, carry on with its variables left unbound.
            //
            // "Matched" is read off `completions` rather than off the output, because a row
            // the optional produced may still be dropped by `DISTINCT` or `OFFSET` — and a
            // match that was deduplicated is still a match. Getting that backwards would
            // emit the *unbound* row as well as the bound one.
            Todo::Optional(optional) => {
                // `MINUS` asks the same question and acts on the opposite answer, so its right
                // side runs *alone* rather than with the rest appended: a match must not emit
                // anything, only decide. The rest then runs, or does not, on that verdict.
                //
                // Appending the rest anyway would still give the right answer — the truncate
                // below discards whatever it produced, and the fallback re-runs it — so this
                // is about not doing that work twice rather than about correctness. A
                // mutation test cannot tell the two apart, and saying so is better than
                // leaving a reader to assume the guard is load-bearing.
                let mut inner: Vec<Todo<'p>> = optional.items.iter().map(Todo::from).collect();
                if optional.mode == OnMatch::Extend {
                    inner.extend_from_slice(&rest);
                }

                // The optional's filters join the pending set only for its own subtree. They
                // decide whether it matched, so they must not survive into the fallback
                // below, where its variables are unbound and the filter would error.
                let mut inner_pending = pending.to_vec();
                inner_pending.extend(optional.filters.iter().copied());

                let before = run.completions;
                let saved_out = run.out.len();
                self.step(view, stats, &inner, &inner_pending, bindings, run)?;
                let matched = run.completions != before;

                match optional.mode {
                    // No match: carry on with the optional's variables unbound.
                    OnMatch::Extend => {
                        if !matched && !run.abandoned {
                            self.step(view, stats, &rest, pending, bindings, run)?;
                        }
                    }
                    // A `MINUS` decides and emits nothing, so whatever the probe produced is
                    // discarded before the verdict is acted on. It produces rows only because
                    // exhausting a todo list is how this loop signals a match, and the right
                    // side's todo list is exhausted by matching.
                    OnMatch::Remove => {
                        run.out.truncate(saved_out);
                        run.completions = before;
                        if !matched && !run.abandoned {
                            self.step(view, stats, &rest, pending, bindings, run)?;
                        }
                    }
                }
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
        pattern: &'p PatternItem,
        rest: &[Todo<'p>],
        pending: &[usize],
        bindings: &mut FxHashMap<&'p Variable, TermId>,
        run: &mut Run<'_>,
    ) -> Result<(), ViewError> {
        let triple = &pattern.triple;
        // A constant absent from the dictionary kills the branch before any scan.
        let Some((subject, predicate, object)) = Self::resolve(triple, view, bindings)? else {
            return Ok(());
        };

        // Which graph to scan. A `GRAPH ?g` whose variable an earlier pattern already bound
        // becomes a scan of that one graph — the bind join's trick applied to the graph
        // position, and the reason `GRAPH ?g { ?s :p ?o . ?s :q ?r }` does not scan every
        // graph for the second pattern.
        //
        // Purely an optimisation: `bind_variable` below rejects a quad from a different
        // graph anyway, so scanning them all and discarding gives the same answer more
        // slowly. A mutation test cannot tell the two apart, which is why the benchmark
        // exists — correctness is the binding check, and this is the speed.
        let graph: Option<Option<TermId>> = match &pattern.scope {
            Scope::Default => Some(None),
            Scope::Named(g) => match lookup(view, oxrdf::Term::from(g.clone()).as_ref())? {
                // A graph the dictionary never saw holds nothing, so the branch is dead
                // before any scan rather than after a fruitless one.
                Slot::Missing => return Ok(()),
                Slot::Fixed(id) => Some(Some(id)),
                Slot::Any => None,
            },
            Scope::Variable(v) => match bindings.get(v) {
                Some(id) => Some(Some(*id)),
                // `None` is `spareval`'s "any named graph", which is what an unbound `GRAPH
                // ?g` ranges over — never the default graph.
                None => None,
            },
        };

        // The probe. Where the previous depth bound the subject, this is a prefix lookup
        // rather than a scan — which is the entire operator.
        for quad in view.internal_quads_for_pattern(
            subject.as_ref(),
            predicate.as_ref(),
            object.as_ref(),
            graph.as_ref().map(Option::as_ref),
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
            // The graph is bound first, so an incompatible one costs nothing else.
            let graph_ok = match (&pattern.scope, quad.graph_name) {
                (Scope::Variable(v), Some(g)) => bind_variable(v, g, bindings, &mut added),
                // A quad from the default graph cannot satisfy `GRAPH ?g`, and the scan does
                // not produce one; this is the belt to that brace.
                (Scope::Variable(_), None) => false,
                _ => true,
            };
            if !graph_ok
                || !bind(&triple.subject, quad.subject, bindings, &mut added)
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

    /// Filter indices that belong to an optional rather than to the query as a whole.
    fn owned_by_optionals(&self) -> rustc_hash::FxHashSet<usize> {
        fn walk(items: &[Item], out: &mut rustc_hash::FxHashSet<usize>) {
            for item in items {
                if let Item::Optional(optional) = item {
                    out.extend(optional.filters.iter().copied());
                    walk(&optional.items, out);
                }
            }
        }
        let mut out = rustc_hash::FxHashSet::default();
        walk(&self.items, &mut out);
        out
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
        Todo::Pattern(pattern) => estimate(&pattern.triple, view, bindings, stats),
        // Each branch yields at most what its most selective pattern does, and the union
        // yields the sum of the branches.
        Todo::Union(branches) => branches
            .iter()
            .map(|branch| {
                branch
                    .iter()
                    .map(|pattern| estimate(&pattern.triple, view, bindings, stats))
                    .fold(f64::INFINITY, f64::min)
            })
            .sum(),
        Todo::Values(values) => values.rows.len() as f64,
        // `cheapest` filters optionals out before asking, so this is not reached today.
        // Kept, and kept as `INFINITY`, because the two agree: were the filter ever removed
        // the estimate would still sort every optional last and preserve source order among
        // them, turning a mistake there into slow rather than wrong. The filter states the
        // rule; this is the floor under it. A mutation test cannot tell them apart, which is
        // the point of having both.
        Todo::Optional(_) => f64::INFINITY,
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
