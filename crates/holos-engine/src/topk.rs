//! `ORDER BY … LIMIT k`, answered from k rows rather than all of them.
//!
//! # The query this exists for
//!
//! ```sparql
//! SELECT ?o WHERE { ?s <http://schema.org/deathDate> ?o } ORDER BY DESC(?o) LIMIT 4
//! ```
//!
//! Over a 654-million-quad store this ran for hours, at one core, and could not be stopped.
//! `spareval` evaluates `ORDER BY` by collecting every solution, sorting the lot, and only
//! then applying the slice — and its comparator externalises both sides of every comparison,
//! so a sort of *n* rows costs `2·n·log₂n` dictionary reads, each a `RocksDB` point read and a
//! literal parse. The rows were held as term ids, gigabytes of them; the memory ceiling
//! could not fire because the query had stopped reading, and neither could the timeout.
//!
//! # What this does instead
//!
//! The k smallest rows of a stream can be kept in a heap of k as the stream goes past —
//! the *top-k* operator every database has, and what [`crate::admit`] used to say this
//! engine lacked. The body under the `ORDER BY` is evaluated as its own query, projected to
//! the variables the output and the sort keys need, and each row is offered to a heap
//! holding `OFFSET + LIMIT` entries. A row that sorts after the heap's worst is dropped on
//! arrival. Each sort key is decoded once, as the row arrives; the comparisons happen on the
//! decoded value. So the query costs *n* decodes of the key, `n·log₂k` in-memory
//! comparisons, and *k* rows of memory — and because the body is a streaming evaluation,
//! the deadline and the memory ceiling are consulted the whole way through.
//!
//! Where the body is one [`crate::bindjoin`] accepts — a scan, a star, an optional, a
//! filter — its rows arrive as ids, streamed rather than held, and only the key is decoded
//! per row; the columns of the rows that survive are decoded at the end. Where it is not,
//! the evaluator's rows arrive decoded in full. On the query above — 48.4 million rows on
//! the predicate, over a 654-million-quad store on a USB disk — the evaluator's rows took
//! 264 s before the view's decode cache and 43 s with it; the id path takes 42 s. With `?s`
//! projected as well, the evaluator's rows could not finish inside a five-minute limit and
//! the id path took 41 s, since `?s` is decoded three times rather than 48 million.
//!
//! How much of that 42 s is the disk is **not** established. `SELECT (COUNT(*) …)` over the
//! same predicate takes 21 s, which is tempting to subtract and wrong to: a `Group` is not
//! in [`crate::bindjoin`]'s fragment, so that query is scanned by the evaluator and this one
//! by the join, and the two are different paths over different index orders. Attributing the
//! rest to the operator would need a profile of one of them, not a subtraction across both.
//!
//! # Ordering
//!
//! SPARQL's `ORDER BY` order (§15.1: unbound, then blank nodes, then IRIs, then literals;
//! literals by value where `<` is defined, and otherwise by lexical form and datatype) is
//! implemented in `spareval` as a private function. [`cmp_terms`] is the same function
//! written here, over the public [`ExpressionTerm`], and the tests sort a mixed bag of terms
//! both ways to hold the two to the same answer. Ties keep arrival order.
//!
//! # What it declines
//!
//! Only the exact shape `Slice(Project(OrderBy(body)))` with a `LIMIT` — `SELECT … ORDER BY
//! … LIMIT`, with or without `OFFSET`. A `DISTINCT` between the sort and the slice is
//! another operator, a query without a `LIMIT` wants every row anyway, and a slice larger
//! than [`MAX_HELD`] would hold more decoded rows than the evaluator holds ids. A sort key
//! that cannot be evaluated outside the evaluator — `EXISTS`, `RAND()`, `NOW()` — declines
//! for the reasons [`crate::bindjoin`] gives. Everything declined goes to `spareval`
//! unchanged, which is the discipline every fast path here keeps.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use holos_core::TermId;
use oxrdf::{NamedOrBlankNode, Term};
use spareval::{ExpressionTerm, ExpressionTriple, QueryEvaluator, QuerySolutionIter};
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern, OrderExpression};
use spargebra::term::Variable;

use crate::{DatasetView, EngineError, ViewError};

/// The most rows a slice may ask for and still be answered here.
///
/// The heap holds decoded rows, which are heavier than the ids `spareval` sorts, so past
/// this the ordinary path is the one with the smaller footprint. A million rows of a few
/// terms each is a few hundred megabytes, and far above what a `LIMIT` is written for.
pub const MAX_HELD: usize = 1 << 20;

/// Where one sort key comes from.
enum Source {
    /// The key is a variable's binding, read straight from the row.
    Variable(Variable),
    /// The key is an expression over the row, evaluated by the evaluator's public entry
    /// point with the variables it mentions substituted in.
    Expression {
        expression: sparopt::algebra::Expression,
        needs: Vec<Variable>,
    },
}

/// One `ORDER BY` condition.
struct Key {
    source: Source,
    descending: bool,
}

/// A recognised `ORDER BY … LIMIT` query, ready to evaluate.
pub struct Plan {
    /// The body under the `ORDER BY`, projected to what the output and the keys need, as a
    /// query the evaluator can run on its own.
    pub inner: Query,
    /// What the query projects, in order.
    pub projected: Vec<Variable>,
    keys: Vec<Key>,
    /// The slice's `OFFSET`, zero where there is no slice.
    pub start: usize,
    /// The slice's `LIMIT`, `None` where there is none.
    pub length: Option<usize>,
}

impl Plan {
    /// Rows the heap would hold, or `None` for a sort it cannot answer.
    ///
    /// The heap needs a `LIMIT`: without one every row is wanted, and holding `k` of them
    /// when `k` is all of them is just a sort with extra steps. It also needs the slice to
    /// be small enough to hold, which [`MAX_HELD`] decides. Everything else goes to
    /// [`crate::spill::Sorted`], which bounds memory by a budget instead of by `k`.
    #[must_use]
    pub fn held(&self) -> Option<usize> {
        let held = self.start.checked_add(self.length?)?;
        (held > 0 && held <= MAX_HELD).then_some(held)
    }
}

/// The query taken apart, borrowed: what [`plan`] builds from and [`sorted_body`] reports.
struct Shape<'q> {
    body: &'q GraphPattern,
    variables: &'q [Variable],
    expression: &'q [OrderExpression],
    start: usize,
    length: Option<usize>,
    dataset: &'q Option<spargebra::algebra::QueryDataset>,
    base_iri: &'q Option<oxiri::Iri<String>>,
}

/// Takes the query apart if it has the shape, without yet looking at the keys.
fn shape(query: &Query) -> Option<Shape<'_>> {
    let Query::Select {
        pattern,
        dataset,
        base_iri,
    } = query
    else {
        return None;
    };
    // A slice is optional: with one the heap may answer this, without one the sort is the
    // whole of it, and either way the shape underneath must be a projection over a sort.
    let (projection, start, length) = match pattern {
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => (inner.as_ref(), *start, *length),
        other => (other, 0, None),
    };
    let GraphPattern::Project { inner, variables } = projection else {
        return None;
    };
    let GraphPattern::OrderBy {
        inner: body,
        expression,
    } = inner.as_ref()
    else {
        return None;
    };
    // A `LIMIT 0` answers nothing and is not worth a special path.
    if length == Some(0) {
        return None;
    }
    Some(Shape {
        body,
        variables,
        expression,
        start,
        length,
        dataset,
        base_iri,
    })
}

/// The sort conditions as keys, and the variables they need the body to project, or
/// `None` for a condition the evaluator's public entry point cannot answer.
fn keys(expression: &[OrderExpression], needed: &mut Vec<Variable>) -> Option<Vec<Key>> {
    let mut keys = Vec::with_capacity(expression.len());
    for order in expression {
        let (expression, descending) = match order {
            OrderExpression::Asc(e) => (e, false),
            OrderExpression::Desc(e) => (e, true),
        };
        let mut mentions = Vec::new();
        if !crate::bindjoin::inspect_expression(expression, &mut mentions) {
            return None;
        }
        let source = match expression {
            Expression::Variable(v) => Source::Variable(v.clone()),
            other => Source::Expression {
                expression: crate::bindjoin::to_sparopt(other)?,
                needs: mentions.clone(),
            },
        };
        for variable in mentions {
            if !needed.contains(&variable) {
                needed.push(variable);
            }
        }
        keys.push(Key { source, descending });
    }
    Some(keys)
}

/// Recognises the shape, or declines.
#[must_use]
pub fn plan(query: &Query) -> Option<Plan> {
    let shape = shape(query)?;
    let mut needed = shape.variables.to_vec();
    let keys = keys(shape.expression, &mut needed)?;
    Some(Plan {
        inner: Query::Select {
            dataset: shape.dataset.clone(),
            base_iri: shape.base_iri.clone(),
            pattern: GraphPattern::Project {
                inner: Box::new(shape.body.clone()),
                variables: needed,
            },
        },
        projected: shape.variables.to_vec(),
        keys,
        start: shape.start,
        length: shape.length,
    })
}

/// The pattern under a sort that will be answered in bounded memory, or `None`.
///
/// What [`crate::admit`] measures instead of the sort: the body is still evaluated in full,
/// and a blocking operator inside it still blocks, but the sort itself holds `OFFSET +
/// LIMIT` rows in the heap, or `spilling` bytes in [`crate::spill::Sorted`] — never its
/// input. A sort neither can take is measured by its input, as it always was.
#[must_use]
pub fn sorted_body(query: &Query, spilling: bool) -> Option<&GraphPattern> {
    let shape = shape(query)?;
    let mut needed = shape.variables.to_vec();
    let keys = keys(shape.expression, &mut needed)?;
    let bounded = spilling
        || Plan {
            inner: query.clone(),
            projected: Vec::new(),
            keys,
            start: shape.start,
            length: shape.length,
        }
        .held()
        .is_some();
    bounded.then_some(shape.body)
}

/// A row in the heap: its keys, whatever the caller keeps of the row, and when it arrived.
struct Entry<R> {
    keys: Vec<Option<ExpressionTerm>>,
    row: R,
    /// Arrival order, which breaks ties so that the heap's order is total and equal keys
    /// come out in the order they went in.
    seq: u64,
    /// Which keys sort descending. Shared, so an entry costs one pointer for it.
    descending: Arc<[bool]>,
}

impl<R> Entry<R> {
    /// Where this row sorts relative to another, in output order.
    fn output_cmp(&self, other: &Self) -> Ordering {
        cmp_keys(&self.keys, &other.keys, &self.descending).then(self.seq.cmp(&other.seq))
    }
}

impl<R> PartialEq for Entry<R> {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}

impl<R> Eq for Entry<R> {}

impl<R> PartialOrd for Entry<R> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<R> Ord for Entry<R> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.output_cmp(other)
    }
}

/// The heap of `OFFSET + LIMIT` rows, fed one row at a time.
///
/// Generic over what is kept of a row, because the two sources keep different things: the
/// evaluator's rows arrive decoded and the projected terms are kept, the bind join's arrive
/// as ids and the ids are kept, to be decoded for the few rows that survive.
struct Collector<R> {
    heap: BinaryHeap<Entry<R>>,
    held: usize,
    start: usize,
    length: usize,
    descending: Arc<[bool]>,
    seq: u64,
}

impl<R> Collector<R> {
    /// The heap for a plan it can answer.
    ///
    /// `held` is [`Plan::held`], which the caller has already checked: the heap paths are
    /// reached only for a plan that has one, and a plan without one goes to the spilling
    /// sort instead.
    fn new(plan: &Plan, held: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(held + 1),
            held,
            start: plan.start,
            length: held - plan.start,
            descending: plan.keys.iter().map(|k| k.descending).collect(),
            seq: 0,
        }
    }

    /// Offers a row by its keys; `row` is asked for only if the row is kept, which for a
    /// small slice over a large body is almost never.
    fn offer(&mut self, keys: Vec<Option<ExpressionTerm>>, row: impl FnOnce() -> R) {
        let seq = self.seq;
        self.seq += 1;
        let probe = Entry {
            keys,
            row: (),
            seq,
            descending: Arc::clone(&self.descending),
        };
        if self.heap.len() == self.held {
            // The heap's top is the row that sorts last among those held. A newcomer that
            // sorts after it, or ties with it, cannot be in the answer.
            let Some(worst) = self.heap.peek() else {
                return;
            };
            if probe.keys_cmp(worst) != Ordering::Less {
                return;
            }
            self.heap.pop();
        }
        self.heap.push(Entry {
            keys: probe.keys,
            row: row(),
            seq: probe.seq,
            descending: probe.descending,
        });
    }

    /// The slice, in order.
    fn finish(self) -> Vec<R> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .skip(self.start)
            .take(self.length)
            .map(|entry| entry.row)
            .collect()
    }
}

impl Entry<()> {
    /// [`Entry::output_cmp`] against an entry holding a row, for the probe that has none.
    fn keys_cmp<R>(&self, other: &Entry<R>) -> Ordering {
        cmp_keys(&self.keys, &other.keys, &self.descending).then(self.seq.cmp(&other.seq))
    }
}

impl Plan {
    /// Runs the evaluator's rows through the heap and returns the slice, in order.
    ///
    /// `rows` is the evaluation of [`Plan::inner`]; `evaluator` evaluates any sort key that
    /// is an expression rather than a variable. A failure from the rows — a cancellation,
    /// a scan that could not be answered — ends the query, as it would anywhere else.
    pub fn collect(
        &self,
        rows: QuerySolutionIter<'_>,
        evaluator: &QueryEvaluator,
        held: usize,
    ) -> Result<Vec<Vec<Option<Term>>>, EngineError> {
        let variables = rows.variables().to_vec();
        let readers = self.readers(&variables);
        let output = self.output_positions(&variables);
        let mut collector = Collector::new(self, held);
        for row in rows {
            let row = row?;
            let values = row.values();
            let keys = readers
                .iter()
                .map(|reader| reader.key(values, |t| Ok(Some(t.clone())), evaluator))
                .collect::<Result<Vec<_>, ViewError>>()?;
            collector.offer(keys, || {
                output
                    .iter()
                    .map(|at| at.and_then(|i| values.get(i).cloned().flatten()))
                    .collect()
            });
        }
        Ok(collector.finish())
    }

    /// Runs the bind join's rows through the heap, decoding only what the keys need.
    ///
    /// The join hands rows over as ids and never holds them. Each row costs a decode of
    /// its key variables — through the view's cache, so a key that recurs costs a lookup —
    /// and nothing else; the rows the heap keeps are decoded in full at the end. This is
    /// the path for a body with a wide projection over a narrow key, where the evaluator's
    /// path would decode every column of every row.
    ///
    /// `None` if the join abandoned the evaluation on its token: the heap has seen a prefix
    /// of the rows, and a prefix is not an answer.
    pub fn collect_ids(
        &self,
        view: &DatasetView<'_>,
        join: &crate::bindjoin::Plan,
        stats: Option<&holos_stats::Statistics>,
        token: Option<&spareval::CancellationToken>,
        evaluator: &QueryEvaluator,
        held: usize,
    ) -> Result<Option<Vec<Vec<Option<Term>>>>, EngineError> {
        let variables = join.variables().to_vec();
        let readers = self.readers(&variables);
        let output = self.output_positions(&variables);
        let mut collector: Collector<Vec<Option<TermId>>> = Collector::new(self, held);
        let completed = {
            let mut sink = |row: &[Option<TermId>]| -> Result<(), ViewError> {
                let keys = readers
                    .iter()
                    .map(|reader| reader.key(row, |id| view.decode_term(*id), evaluator))
                    .collect::<Result<Vec<_>, ViewError>>()?;
                // Copied only for a row the heap keeps, which for a small slice over a
                // large body is almost none of them.
                collector.offer(keys, || row.to_vec());
                Ok(())
            };
            join.evaluate_each(
                view,
                stats,
                crate::bindjoin::Limits { rows: None, token },
                &mut sink,
            )?
        };
        if !completed {
            return Ok(None);
        }
        let mut rows = Vec::new();
        for ids in collector.finish() {
            let mut row = Vec::with_capacity(output.len());
            for at in &output {
                row.push(match at.and_then(|i| ids.get(i).copied().flatten()) {
                    Some(id) => view.decode_term(id)?,
                    None => None,
                });
            }
            rows.push(row);
        }
        Ok(Some(rows))
    }

    /// The keys' readers, with their variables resolved to positions in a row laid out as
    /// `variables`.
    fn readers<'p>(&'p self, variables: &[Variable]) -> Vec<Reader<'p>> {
        let position = |v: &Variable| variables.iter().position(|x| x == v);
        self.keys
            .iter()
            .map(|key| match &key.source {
                Source::Variable(v) => Reader::Variable(position(v)),
                Source::Expression { expression, needs } => Reader::Expression {
                    expression,
                    needs: needs
                        .iter()
                        .filter_map(|v| position(v).map(|at| (v, at)))
                        .collect(),
                },
            })
            .collect()
    }

    /// Where each projected variable sits in a row laid out as `variables`, or `None` for
    /// one the body never binds.
    fn output_positions(&self, variables: &[Variable]) -> Vec<Option<usize>> {
        self.projected
            .iter()
            .map(|v| variables.iter().position(|x| x == v))
            .collect()
    }
}

impl Plan {
    /// Sorts every row of the body in bounded memory and returns the slice, in order.
    ///
    /// For the sorts the heap declines — no `LIMIT`, or one too large to hold. Rows go into
    /// [`crate::spill::Sorted`], which keeps them in memory until `budget` bytes and writes
    /// sorted runs past that, so what an `ORDER BY` costs stops being the size of its input.
    /// The order is [`cmp_keys`], the same as the heap's.
    ///
    /// Returned as an iterator rather than a `Vec`, because the whole point is that the
    /// answer need never be resident: the merge yields one row at a time and the serialiser
    /// writes it out.
    pub fn collect_spilled<'a>(
        &self,
        rows: QuerySolutionIter<'a>,
        evaluator: &QueryEvaluator,
        budget: usize,
        token: Option<spareval::CancellationToken>,
    ) -> Result<QuerySolutionIter<'a>, EngineError> {
        let variables = rows.variables().to_vec();
        let readers = self.readers(&variables);
        let output = self.output_positions(&variables);
        let descending: Arc<[bool]> = self.keys.iter().map(|k| k.descending).collect();
        let io = |e: std::io::Error| EngineError::Evaluation(spareval::QueryEvaluationError::Dataset(Box::new(e)));

        let mut sorted =
            crate::spill::Sorted::new(self.projected.len(), descending, budget).map_err(io)?;
        let mut rows = rows;
        for row in rows.by_ref() {
            let row = row?;
            let values = row.values();
            let keys = readers
                .iter()
                .map(|reader| reader.key(values, |t| Ok(Some(t.clone())), evaluator))
                .collect::<Result<Vec<_>, ViewError>>()?;
            let projected = output
                .iter()
                .map(|at| at.and_then(|i| values.get(i).cloned().flatten()))
                .collect();
            sorted.push(keys, projected).map_err(io)?;
        }

        let mut merged = sorted.merge().map_err(io)?;
        let mut skipped = 0;
        let start = self.start;
        let mut emitted = 0;
        let length = self.length;
        let projected: Arc<[Variable]> = self.projected.clone().into();
        // `rows` is exhausted and is moved in anyway, because the deadline guarding the
        // query lives inside it: dropping it here would stop the watchdog, and the merge
        // below — which can be minutes of a large sort — would then run with no time limit
        // at all. Held to the last row, the token beneath still fires.
        let out = std::iter::from_fn(move || {
            let _guarded = &rows;
            loop {
                if length.is_some_and(|limit| emitted >= limit) {
                    return None;
                }
                if token
                    .as_ref()
                    .is_some_and(spareval::CancellationToken::is_cancelled)
                {
                    return Some(Err(spareval::QueryEvaluationError::Cancelled));
                }
                match merged.next_row() {
                    Ok(None) => return None,
                    Err(e) => {
                        return Some(Err(spareval::QueryEvaluationError::Dataset(Box::new(e))))
                    }
                    Ok(Some(row)) => {
                        if skipped < start {
                            skipped += 1;
                            continue;
                        }
                        emitted += 1;
                        return Some(Ok(row));
                    }
                }
            }
        });
        Ok(QuerySolutionIter::from_tuples(projected, out))
    }
}

/// A sort key's reader, with the variables resolved to positions in the row.
enum Reader<'p> {
    /// `None` when the variable is not in the row at all, which makes the key unbound.
    Variable(Option<usize>),
    Expression {
        expression: &'p sparopt::algebra::Expression,
        needs: Vec<(&'p Variable, usize)>,
    },
}

impl Reader<'_> {
    /// This row's key: the decoded value, or `None` for unbound and for an expression that
    /// raised an error, which SPARQL orders the same way.
    ///
    /// `decode` turns a cell into a term: a clone when the row already holds terms, a read
    /// through the view when it holds ids.
    fn key<C>(
        &self,
        values: &[Option<C>],
        decode: impl Fn(&C) -> Result<Option<Term>, ViewError>,
        evaluator: &QueryEvaluator,
    ) -> Result<Option<ExpressionTerm>, ViewError> {
        let cell = |at: usize| -> Result<Option<Term>, ViewError> {
            match values.get(at).and_then(Option::as_ref) {
                Some(cell) => decode(cell),
                None => Ok(None),
            }
        };
        match self {
            Reader::Variable(None) => Ok(None),
            Reader::Variable(Some(at)) => Ok(cell(*at)?.map(ExpressionTerm::from)),
            Reader::Expression { expression, needs } => {
                let mut substitutions = Vec::with_capacity(needs.len());
                for (variable, at) in needs {
                    if let Some(term) = cell(*at)? {
                        substitutions.push((*variable, term));
                    }
                }
                Ok(evaluator
                    .evaluate_expression(expression, substitutions)
                    .map(ExpressionTerm::from))
            }
        }
    }
}

/// Where one row's sort keys place it against another's, under an `ORDER BY`'s conditions.
///
/// Each key in turn by [`cmp_terms`], reversed where its condition said `DESC`, until one
/// of them decides. `Equal` means the conditions do not separate these two rows, and the
/// caller breaks the tie — by arrival, everywhere in this engine, so that a sort is stable
/// and two runs of one query agree.
///
/// This is the whole of the engine's own ordering, used by the heap in this module and by
/// the spilling sort in [`crate::spill`], so that a query answered by either comes back the
/// same way.
///
/// # Where it differs from the evaluator's, and why that is allowed
///
/// SPARQL §15.1 orders unbound before blank nodes before IRIs before literals, and orders
/// two literals by value where `<` is defined between them. Between the rest — an integer
/// and a string, say — it defines **no** order, and says an implementation may extend it.
/// `spareval` extends it by comparing lexical forms, and this module does the same so that
/// the two agree; the consequence, worth knowing before relying on it, is that the extension
/// is not transitive. `2 < 10` by value, `"1abc" < 2` and `10 < "1abc"` by lexical form, so
/// those three terms form a cycle, and which of them a sort puts first depends on the order
/// they arrived in. Both paths were measured doing exactly that, and disagreeing with each
/// other. Nothing is wrong with either answer — the specification defines no order over that
/// trio — but neither is repeatable, so **a query whose sort key mixes datatypes has no
/// dependable order**, whatever it is sorted by. A key of one datatype, which is what real
/// data has, is a total order and is fully determined.
#[must_use]
pub fn cmp_keys(
    a: &[Option<ExpressionTerm>],
    b: &[Option<ExpressionTerm>],
    descending: &[bool],
) -> Ordering {
    for ((a, b), descending) in a.iter().zip(b).zip(descending) {
        let ordering = cmp_terms(a.as_ref(), b.as_ref());
        let ordering = if *descending {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// SPARQL's `ORDER BY` order over two possibly-unbound terms.
///
/// The same function as `spareval`'s, which is private: unbound first, then blank nodes by
/// label, then IRIs by string, then literals, then triple terms. Two literals compare by
/// value when SPARQL's `<` is defined between them, and by `(lexical form, datatype,
/// language)` when it is not — so `"b"` and `2` have an order, and it is arbitrary but
/// total.
#[must_use]
pub fn cmp_terms(a: Option<&ExpressionTerm>, b: Option<&ExpressionTerm>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => match a {
            ExpressionTerm::BlankNode(a) => match b {
                ExpressionTerm::BlankNode(b) => a.as_str().cmp(b.as_str()),
                _ => Ordering::Less,
            },
            ExpressionTerm::NamedNode(a) => match b {
                ExpressionTerm::BlankNode(_) => Ordering::Greater,
                ExpressionTerm::NamedNode(b) => a.as_str().cmp(b.as_str()),
                _ => Ordering::Less,
            },
            ExpressionTerm::Triple(a) => match b {
                ExpressionTerm::Triple(b) => cmp_triples(a, b),
                _ => Ordering::Greater,
            },
            _ => match b {
                ExpressionTerm::NamedNode(_) | ExpressionTerm::BlankNode(_) => Ordering::Greater,
                ExpressionTerm::Triple(_) => Ordering::Less,
                _ => partial_cmp_literals(a, b).unwrap_or_else(|| lexical_cmp(a, b)),
            },
        },
    }
}

/// Triple terms: subject, then predicate, then object, each as [`cmp_terms`] would.
fn cmp_triples(a: &ExpressionTriple, b: &ExpressionTriple) -> Ordering {
    let subjects = match (&a.subject, &b.subject) {
        (NamedOrBlankNode::BlankNode(a), NamedOrBlankNode::BlankNode(b)) => {
            a.as_str().cmp(b.as_str())
        }
        (NamedOrBlankNode::BlankNode(_), NamedOrBlankNode::NamedNode(_)) => Ordering::Less,
        (NamedOrBlankNode::NamedNode(_), NamedOrBlankNode::BlankNode(_)) => Ordering::Greater,
        (NamedOrBlankNode::NamedNode(a), NamedOrBlankNode::NamedNode(b)) => {
            a.as_str().cmp(b.as_str())
        }
    };
    subjects
        .then_with(|| a.predicate.as_str().cmp(b.predicate.as_str()))
        .then_with(|| cmp_terms(Some(&a.object), Some(&b.object)))
}

/// SPARQL's `<` between two literals, where it is defined.
///
/// Numbers of any of the four numeric types compare with each other; strings with strings,
/// language-tagged strings with the same tag, `xsd:dateTime` with `xsd:dateTime`. A
/// datatype the evaluator does not give a value to — `xsd:date` in this build, and every
/// user-defined type — is not comparable by value, and falls to the lexical order.
fn partial_cmp_literals(a: &ExpressionTerm, b: &ExpressionTerm) -> Option<Ordering> {
    use ExpressionTerm as T;
    use oxsdatatypes::{Decimal, Double, Float};
    match (a, b) {
        (T::StringLiteral(a), T::StringLiteral(b)) => a.partial_cmp(b),
        (
            T::LangStringLiteral {
                value: va,
                language: la,
            },
            T::LangStringLiteral {
                value: vb,
                language: lb,
            },
        ) => (la == lb).then(|| va.cmp(vb)),
        (
            T::DirLangStringLiteral {
                value: va,
                language: la,
                direction: da,
            },
            T::DirLangStringLiteral {
                value: vb,
                language: lb,
                direction: db,
            },
        ) => (la == lb && da == db).then(|| va.cmp(vb)),
        (T::FloatLiteral(a), T::FloatLiteral(b)) => a.partial_cmp(b),
        (T::FloatLiteral(a), T::DoubleLiteral(b)) => Double::from(*a).partial_cmp(b),
        (T::FloatLiteral(a), T::IntegerLiteral(b)) => a.partial_cmp(&Float::from(*b)),
        (T::FloatLiteral(a), T::DecimalLiteral(b)) => a.partial_cmp(&Float::from(*b)),
        (T::DoubleLiteral(a), T::FloatLiteral(b)) => a.partial_cmp(&Double::from(*b)),
        (T::DoubleLiteral(a), T::DoubleLiteral(b)) => a.partial_cmp(b),
        (T::DoubleLiteral(a), T::IntegerLiteral(b)) => a.partial_cmp(&Double::from(*b)),
        (T::DoubleLiteral(a), T::DecimalLiteral(b)) => a.partial_cmp(&Double::from(*b)),
        (T::IntegerLiteral(a), T::FloatLiteral(b)) => Float::from(*a).partial_cmp(b),
        (T::IntegerLiteral(a), T::DoubleLiteral(b)) => Double::from(*a).partial_cmp(b),
        (T::IntegerLiteral(a), T::IntegerLiteral(b)) => a.partial_cmp(b),
        (T::IntegerLiteral(a), T::DecimalLiteral(b)) => Decimal::from(*a).partial_cmp(b),
        (T::DecimalLiteral(a), T::FloatLiteral(b)) => Float::from(*a).partial_cmp(b),
        (T::DecimalLiteral(a), T::DoubleLiteral(b)) => Double::from(*a).partial_cmp(b),
        (T::DecimalLiteral(a), T::IntegerLiteral(b)) => a.partial_cmp(&Decimal::from(*b)),
        (T::DecimalLiteral(a), T::DecimalLiteral(b)) => a.partial_cmp(b),
        (T::DateTimeLiteral(a), T::DateTimeLiteral(b)) => a.partial_cmp(b),
        _ => None,
    }
}

/// The order two literals fall back to when `<` is not defined between them: lexical form,
/// then datatype, then language tag — what `spareval` does, and what makes the order total.
///
/// Two literals of a datatype the evaluator does not value — which is every `xsd:date` in
/// this build, and so every key of the query this module was written for — are compared
/// in place. The general case goes back through `Term`, which canonicalises a valued
/// literal's lexical form the way the evaluator does; it is reached only for a pair of
/// different kinds, which a well-typed sort key seldom produces.
fn lexical_cmp(a: &ExpressionTerm, b: &ExpressionTerm) -> Ordering {
    if let (
        ExpressionTerm::OtherTypedLiteral {
            value: va,
            datatype: da,
        },
        ExpressionTerm::OtherTypedLiteral {
            value: vb,
            datatype: db,
        },
    ) = (a, b)
    {
        return va.cmp(vb).then_with(|| da.as_str().cmp(db.as_str()));
    }
    match (Term::from(a.clone()), Term::from(b.clone())) {
        (Term::Literal(a), Term::Literal(b)) => (a.value(), a.datatype(), a.language()).cmp(&(
            b.value(),
            b.datatype(),
            b.language(),
        )),
        // Both sides are literals here; the arms above have sent every other kind away.
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatasetView, Engine};
    use holos_security::Session;
    use oxrdfio::RdfFormat;
    use spareval::QueryResults;
    use spargebra::SparqlParser;

    const DATA: &str = r#"
        @prefix ex: <http://example.com/> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        ex:a ex:died "1901-02-03"^^xsd:date ; ex:name "Ada" ; ex:rank 3 .
        ex:b ex:died "1999-12-31"^^xsd:date ; ex:name "Bob" ; ex:rank 1 .
        ex:c ex:died "1950-06-15"^^xsd:date ; ex:name "Cy" ; ex:rank 2 .
        ex:d ex:died "2020-01-01"^^xsd:date ; ex:name "Dee" .
        ex:e ex:name "Eve" ; ex:rank 2 .
        ex:f ex:died "1950-06-15"^^xsd:date ; ex:name "Fay" ; ex:rank 5 .
    "#;

    fn engine() -> Engine {
        let mut e = Engine::new();
        e.bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
            .unwrap();
        e
    }

    fn rows(view: &DatasetView<'_>, query: &str) -> Vec<Vec<Option<String>>> {
        match Engine::query(view, query, None).unwrap() {
            QueryResults::Solutions(iter) => iter
                .map(|s| {
                    s.unwrap()
                        .values()
                        .iter()
                        .map(|t| t.as_ref().map(ToString::to_string))
                        .collect()
                })
                .collect(),
            _ => panic!("expected solutions"),
        }
    }

    fn parse(query: &str) -> Query {
        SparqlParser::new().parse_query(query).unwrap()
    }

    #[test]
    fn the_shape_is_recognised_exactly() {
        let p = "PREFIX ex: <http://example.com/> ";
        let heap = |q: &str| plan(&parse(q)).and_then(|plan| plan.held());
        let recognised = |q: &str| plan(&parse(q)).is_some();

        // The heap takes a sort with a LIMIT it can hold.
        assert_eq!(heap(&format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o) LIMIT 4")), Some(4));
        assert_eq!(heap(&format!("{p}SELECT * WHERE {{ ?s ex:died ?o }} ORDER BY ?o OFFSET 2 LIMIT 4")), Some(6));
        assert_eq!(heap(&format!("{p}SELECT ?s WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o) ?s LIMIT 4")), Some(4));
        assert_eq!(heap(&format!("{p}SELECT ?s WHERE {{ ?s ex:rank ?r }} ORDER BY (?r * 2) LIMIT 4")), Some(4));

        // Recognised, but past what the heap will hold, so the spilling sort takes it.
        for query in [
            format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o)"),
            format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o OFFSET 5"),
            format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o LIMIT {}", MAX_HELD + 1),
        ] {
            assert!(recognised(&query), "{query}");
            assert_eq!(heap(&query), None, "{query}");
        }

        // Not a sort this module answers at all, by either route.
        // A DISTINCT between the sort and the slice is another operator.
        assert!(!recognised(&format!("{p}SELECT DISTINCT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o LIMIT 4")));
        // A key the evaluator's public entry point cannot answer.
        assert!(!recognised(&format!("{p}SELECT ?s WHERE {{ ?s ex:died ?o }} ORDER BY RAND() LIMIT 4")));
        assert!(!recognised(&format!("{p}SELECT ?s WHERE {{ ?s ex:died ?o }} ORDER BY EXISTS {{ ?s ex:rank ?r }} LIMIT 4")));
        // Nothing to return.
        assert!(!recognised(&format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o LIMIT 0")));
        // No sort.
        assert!(!recognised(&format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} LIMIT 4")));
    }

    #[test]
    fn the_inner_query_projects_the_keys_as_well() {
        let q = parse(
            "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:died ?o ; ex:rank ?r } ORDER BY DESC(?o) (?r + 1) LIMIT 2",
        );
        let plan = plan(&q).unwrap();
        let Query::Select {
            pattern: GraphPattern::Project { variables, .. },
            ..
        } = &plan.inner
        else {
            panic!("the inner query is a projection");
        };
        let names: Vec<&str> = variables.iter().map(Variable::as_str).collect();
        assert_eq!(names, ["s", "o", "r"]);
        assert_eq!(plan.projected.len(), 1);
        assert_eq!((plan.start, plan.length, plan.held()), (0, Some(2), Some(2)));
    }

    #[test]
    fn the_latest_deaths_come_first() {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let got = rows(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?o WHERE { ?s ex:died ?o } ORDER BY DESC(?o) LIMIT 3",
        );
        let dates: Vec<&str> = got.iter().map(|r| r[0].as_deref().unwrap()).collect();
        assert_eq!(
            dates,
            [
                "\"2020-01-01\"^^<http://www.w3.org/2001/XMLSchema#date>",
                "\"1999-12-31\"^^<http://www.w3.org/2001/XMLSchema#date>",
                "\"1950-06-15\"^^<http://www.w3.org/2001/XMLSchema#date>",
            ]
        );
    }

    #[test]
    fn offset_and_a_key_outside_the_projection() {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        // Ranks: b=1, c=2, e=2, a=3, f=5, d unbound. Unbound sorts first; ties keep
        // arrival order, and the store hands rows back in insertion order.
        let got = rows(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?n WHERE { ?s ex:name ?n . OPTIONAL { ?s ex:rank ?r } } ORDER BY ?r ?n OFFSET 1 LIMIT 3",
        );
        let names: Vec<&str> = got.iter().map(|r| r[0].as_deref().unwrap()).collect();
        assert_eq!(names, ["\"Bob\"", "\"Cy\"", "\"Eve\""]);
    }

    #[test]
    fn an_expression_key_and_two_directions() {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let got = rows(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?n ?r WHERE { ?s ex:name ?n ; ex:rank ?r } ORDER BY DESC(?r * 10) ASC(?n) LIMIT 3",
        );
        let names: Vec<&str> = got.iter().map(|r| r[0].as_deref().unwrap()).collect();
        assert_eq!(names, ["\"Fay\"", "\"Ada\"", "\"Cy\""]);
    }

    #[test]
    fn a_slice_past_the_end_is_empty_and_a_short_body_is_whole() {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let p = "PREFIX ex: <http://example.com/> ";
        assert!(rows(&view, &format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o OFFSET 10 LIMIT 3")).is_empty());
        assert_eq!(rows(&view, &format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o LIMIT 100")).len(), 5);
    }

    /// The terms of every kind the order has a rule for, with pairs that tie by value and
    /// pairs that only the lexical fallback separates. Blank nodes cannot be written in a
    /// `VALUES` block, so the query unions two `BNODE()`s in.
    const MIXED: &str = r#"
        UNDEF
        <http://example.com/z> <http://example.com/a>
        "b" "a" "10" "9"
        "b"@en "a"@en "a"@fr "a"@en--ltr
        true false
        3 "03"^^xsd:integer 2.5 2.50 "2.5"^^xsd:float 2.5e0 -1 1e400 "NaN"^^xsd:double
        "2001-01-01T00:00:00Z"^^xsd:dateTime "1999-06-01T12:00:00"^^xsd:dateTime
        "2001-01-01"^^xsd:date "1999-06-01"^^xsd:date "P1D"^^xsd:duration
        "x"^^<http://example.com/t> "x"^^<http://example.com/s> "w"^^<http://example.com/t>
        <<( <http://example.com/s> <http://example.com/p> "o" )>>
        <<( <http://example.com/s> <http://example.com/p> 1 )>>
        <<( <http://example.com/r> <http://example.com/q> "o" )>>
    "#;

    fn mixed(direction: &str, limit: &str) -> Vec<Option<Term>> {
        let e = Engine::new();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let query = format!(
            "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT ?x WHERE {{ {{ VALUES ?x {{ {MIXED} }} }} UNION {{ BIND(BNODE() AS ?x) }} UNION {{ BIND(BNODE() AS ?x) }} }} ORDER BY {direction}(?x) {limit}"
        );
        let terms = match Engine::query(&view, &query, None).unwrap() {
            QueryResults::Solutions(iter) => iter
                .map(|s| s.unwrap().get(0).cloned())
                .collect(),
            _ => panic!("expected solutions"),
        };
        terms
    }

    /// The order this module implements is held to the evaluator's: the same bag of terms
    /// is sorted by `spareval` (no `LIMIT`, so this path declines) and by the heap (a
    /// `LIMIT` past the end, so nothing is dropped), and the evaluator's sequence must be
    /// non-decreasing under [`cmp_terms`] at every pair, in both directions.
    #[test]
    fn the_order_agrees_with_the_evaluator() {
        for direction in ["ASC", "DESC"] {
            let theirs = mixed(direction, "");
            let ours = mixed(direction, "LIMIT 1000");
            assert_eq!(theirs.len(), ours.len(), "{direction}: same number of rows");
            let key = |t: &Option<Term>| t.clone().map(ExpressionTerm::from);
            // Every pair of the evaluator's output is in this module's order.
            for i in 0..theirs.len() {
                for j in i + 1..theirs.len() {
                    let ordering = cmp_terms(key(&theirs[i]).as_ref(), key(&theirs[j]).as_ref());
                    let expected = if direction == "ASC" {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    };
                    assert_ne!(
                        ordering, expected,
                        "{direction}: the evaluator put {:?} before {:?}, and cmp_terms disagrees",
                        theirs[i], theirs[j]
                    );
                }
            }
            // And this module's output is the same rows, in an order the evaluator's
            // comparator would accept — each adjacent pair non-decreasing. A blank node's
            // label is fresh on every run, so the two are compared with labels erased.
            let show = |t: &Option<Term>| match t {
                Some(Term::BlankNode(_)) => Some("_:".to_owned()),
                other => other.as_ref().map(ToString::to_string),
            };
            let mut a = theirs.iter().map(show).collect::<Vec<_>>();
            let mut b = ours.iter().map(show).collect::<Vec<_>>();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{direction}: the same rows");
            for pair in ours.windows(2) {
                let ordering = cmp_terms(key(&pair[0]).as_ref(), key(&pair[1]).as_ref());
                let wrong = if direction == "ASC" {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
                assert_ne!(ordering, wrong, "{direction}: {:?} before {:?}", pair[0], pair[1]);
            }
        }
    }

    /// The two sources — the bind join's ids and the evaluator's decoded rows — must give
    /// the same slice. Each query below is run with the join and without it.
    #[test]
    fn the_id_path_and_the_evaluator_path_agree() {
        use crate::QueryOptions;
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let p = "PREFIX ex: <http://example.com/> ";
        for query in [
            format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o) LIMIT 3"),
            format!("{p}SELECT ?s ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o ?s OFFSET 1 LIMIT 3"),
            format!("{p}SELECT ?n WHERE {{ ?s ex:name ?n . OPTIONAL {{ ?s ex:rank ?r }} }} ORDER BY ?r ?n OFFSET 1 LIMIT 3"),
            format!("{p}SELECT ?n ?r WHERE {{ ?s ex:name ?n ; ex:rank ?r }} ORDER BY DESC(?r * 10) ASC(?n) LIMIT 3"),
            format!("{p}SELECT ?n WHERE {{ ?s ex:name ?n ; ex:rank ?r FILTER(?r > 1) }} ORDER BY DESC(?r) ?n LIMIT 10"),
        ] {
            let run = |options: &QueryOptions| -> Vec<Vec<Option<String>>> {
                let (results, _) = Engine::query_with(&view, &query, options).unwrap();
                match results {
                    QueryResults::Solutions(iter) => iter
                        .map(|s| {
                            s.unwrap()
                                .values()
                                .iter()
                                .map(|t| t.as_ref().map(ToString::to_string))
                                .collect()
                        })
                        .collect(),
                    _ => panic!("expected solutions"),
                }
            };
            let with_join = run(&QueryOptions::new());
            let without = run(&QueryOptions::new().without_bind_join());
            assert_eq!(with_join, without, "{query}");
            assert!(!with_join.is_empty(), "{query}");
        }
    }

    #[test]
    fn ties_keep_arrival_order() {
        let e = Engine::new();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let got = rows(
            &view,
            "SELECT ?x ?y WHERE { VALUES (?x ?y) { (1 \"first\") (0 \"zero\") (1 \"second\") (1 \"third\") } } ORDER BY ?x LIMIT 3",
        );
        let ys: Vec<&str> = got.iter().map(|r| r[1].as_deref().unwrap()).collect();
        assert_eq!(ys, ["\"zero\"", "\"first\"", "\"second\""]);
    }
}
