//! The bound join: sending a `SERVICE` the keys it will be joined against.
//!
//! Not to be confused with [`crate::bindjoin`], which is the local join strategy. This is the
//! federation optimisation of the same family, on a different problem: what to put *in* the
//! query that crosses the network.
//!
//! # What goes wrong without it
//!
//! ```sparql
//! ?town owl:sameAs ?dbp .
//! SERVICE <https://dbpedia.org/sparql> { ?dbp dbo:populationTotal ?population }
//! ```
//!
//! `?dbp` is bound out here -- three book towns -- and the endpoint is asked
//! `SELECT ?dbp ?population WHERE { ?dbp dbo:populationTotal ?population }`, which is every
//! population `DBpedia` holds. It caps an answer at ten thousand rows, the three towns are not
//! among the ten thousand it chose, and the join finds nothing. The query is right, the engine
//! is right, and the answer is empty.
//!
//! A bound join sends the keys with the question:
//!
//! ```sparql
//! SELECT ?dbp ?population WHERE {
//!   VALUES (?dbp) { (dbr:Hay-on-Wye) (dbr:Sedbergh) (dbr:Wigtown) }
//!   ?dbp dbo:populationTotal ?population
//! }
//! ```
//!
//! which is three lookups rather than a scan, and comes back in one round trip.
//!
//! # Why it is a rewrite and not a handler
//!
//! `spareval` evaluates `SERVICE` as a nested loop: it calls the handler once per solution
//! arriving from the left, and joins each answer with that solution. So the bindings exist at
//! the moment of the call -- but `DefaultServiceHandler::handle` is given the endpoint, the
//! pattern and the base IRI, and not the solution. The handler cannot see what it is being
//! joined against, and every one of those calls therefore sends the identical unrestricted
//! query.
//!
//! Which leaves rewriting the query before evaluation. The keys have to come from somewhere, so
//! this runs the query's local part first with every `SERVICE` replaced by the empty pattern,
//! and reads them out of that. Two evaluations instead of one, which is what a bound join costs
//! wherever it is implemented.
//!
//! # Why a superset of the keys is safe and a subset is not
//!
//! The probe is deliberately generous. Neutralising a `SERVICE` removes a constraint, so the
//! probe can report keys the real evaluation would never ask about -- and pushing a key that is
//! not needed only means the endpoint returns a row the local join then discards. Pushing *too
//! few* would drop rows from the answer, so every rule here errs towards sending more:
//!
//! * A variable is pushed only when the probe bound it in **every** row. An unbound join
//!   variable means that solution joins against the whole remote pattern, so restricting it
//!   would lose rows.
//! * A variable whose keys include a blank node is not pushed. Blank-node labels are
//!   document-scoped, so the label cannot name anything at the far end -- and a `VALUES` block
//!   cannot carry one anyway.
//! * Above [`MAX_PUSHED_BINDINGS`] distinct tuples, nothing is pushed. A `VALUES` block with
//!   tens of thousands of rows is a request most endpoints refuse outright, and an unrestricted
//!   query that might be answered beats a bound one that will not be accepted.
//!
//! # What it does not reach
//!
//! A `SERVICE` whose keys come from *another* `SERVICE`. The probe neutralises all of them at
//! once, so a chained clause sees no keys and is sent unrestricted, exactly as before. Binding
//! those would mean probing once per clause with the other clauses live, which is a different
//! and much larger design -- and the host-fetched loop already re-runs the query as answers
//! arrive, so a chain still terminates without it.

use crate::service::pattern_variables;
use oxrdf::Variable;
use spargebra::Query;
use spargebra::algebra::GraphPattern;
use spargebra::term::GroundTerm;
use std::collections::{BTreeMap, BTreeSet};

/// How many distinct key tuples may be pushed into one `SERVICE` clause.
///
/// Chosen to sit well below what a public endpoint accepts in a POST body rather than from
/// measurement of any particular one. A query needing more keys than this is not the shape this
/// optimisation is for: at that point the remote scan it was avoiding is probably the cheaper
/// of the two.
pub const MAX_PUSHED_BINDINGS: usize = 1024;

/// Keys to join into a `SERVICE` clause, in the shape `GraphPattern::Values` wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    /// The variables the tuples are over, in the tuples' own order.
    pub variables: Vec<Variable>,
    /// One tuple per row. Always `Some`, because an unbound key is not pushed at all.
    pub rows: Vec<Vec<Option<GroundTerm>>>,
}

/// The `SERVICE` clauses in `pattern`, each with the variables it could receive.
///
/// In the order [`with_pushed_keys`] consumes them, which is what lets a caller collect
/// candidates on one pass and answer with keys on the next.
#[must_use]
pub fn service_candidates(pattern: &GraphPattern) -> Vec<Vec<Variable>> {
    let mut out = Vec::new();
    let mut collect = |candidates: &[Variable]| -> Option<Keys> {
        out.push(candidates.to_vec());
        None
    };
    rewrite_services(pattern, &BTreeSet::new(), &mut collect);
    out
}

/// `pattern` with keys joined into each `SERVICE` that has any.
///
/// `keys` is positional and matches [`service_candidates`]: one entry per clause, `None` to
/// leave that clause as it was.
#[must_use]
pub fn with_pushed_keys(pattern: &GraphPattern, keys: Vec<Option<Keys>>) -> GraphPattern {
    let mut supply = keys.into_iter();
    let mut next = |_: &[Variable]| -> Option<Keys> { supply.next().flatten() };
    rewrite_services(pattern, &BTreeSet::new(), &mut next)
}

/// Walks `pattern`, asking `supply` what to push into each `SERVICE`.
///
/// `bound` is the variables a pattern this one joins with may bind, which is what makes a
/// variable a candidate. It grows going *down* the tree, and only across a join: the operands of
/// a `UNION` do not see each other's variables, and neither does the mandatory side of an
/// `OPTIONAL` see the optional side's -- a row there survives without a match, so restricting it
/// to the other side's keys would lose rows.
///
/// `supply` is called once per clause, in pre-order. Both public entry points go through here so
/// the order cannot drift between the pass that collects and the pass that fills.
fn rewrite_services(
    pattern: &GraphPattern,
    bound: &BTreeSet<Variable>,
    supply: &mut impl FnMut(&[Variable]) -> Option<Keys>,
) -> GraphPattern {
    match pattern {
        GraphPattern::Service {
            name,
            inner,
            silent,
        } => {
            let mut candidates: Vec<Variable> = pattern_variables(inner)
                .into_iter()
                .filter(|v| bound.contains(v))
                .collect();
            // Sorted so the pushed block, and the query string a host caches on, does not
            // depend on the order the pattern happened to mention its variables in.
            candidates.sort_by(|a, b| a.as_str().cmp(b.as_str()));

            let inner = match supply(&candidates) {
                Some(keys) if !keys.variables.is_empty() && !keys.rows.is_empty() => {
                    Box::new(GraphPattern::Join {
                        left: Box::new(GraphPattern::Values {
                            variables: keys.variables,
                            bindings: keys.rows,
                        }),
                        right: inner.clone(),
                    })
                }
                _ => inner.clone(),
            };
            GraphPattern::Service {
                name: name.clone(),
                inner,
                silent: *silent,
            }
        }
        GraphPattern::Join { left, right } => {
            let for_left = with_variables(bound, right);
            let for_right = with_variables(bound, left);
            GraphPattern::Join {
                left: Box::new(rewrite_services(left, &for_left, supply)),
                right: Box::new(rewrite_services(right, &for_right, supply)),
            }
        }
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            let for_right = with_variables(bound, left);
            GraphPattern::LeftJoin {
                left: Box::new(rewrite_services(left, bound, supply)),
                right: Box::new(rewrite_services(right, &for_right, supply)),
                expression: expression.clone(),
            }
        }
        GraphPattern::Union { left, right } => GraphPattern::Union {
            left: Box::new(rewrite_services(left, bound, supply)),
            right: Box::new(rewrite_services(right, bound, supply)),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: Box::new(rewrite_services(left, bound, supply)),
            right: Box::new(rewrite_services(right, bound, supply)),
        },
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr: expr.clone(),
            inner: Box::new(rewrite_services(inner, bound, supply)),
        },
        GraphPattern::Graph { name, inner } => GraphPattern::Graph {
            name: name.clone(),
            inner: Box::new(rewrite_services(inner, bound, supply)),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: Box::new(rewrite_services(inner, bound, supply)),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: Box::new(rewrite_services(inner, bound, supply)),
            expression: expression.clone(),
        },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: Box::new(rewrite_services(inner, bound, supply)),
            variables: variables.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct {
            inner: Box::new(rewrite_services(inner, bound, supply)),
        },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced {
            inner: Box::new(rewrite_services(inner, bound, supply)),
        },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: Box::new(rewrite_services(inner, bound, supply)),
            start: *start,
            length: *length,
        },
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: Box::new(rewrite_services(inner, bound, supply)),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {
            pattern.clone()
        }
    }
}

/// `pattern` with every `SERVICE` replaced by the empty pattern, for the probe.
///
/// The empty pattern, not nothing: a `Bgp` with no triples matches once and binds nothing, which
/// is the identity of a join. So the clause stops constraining and stops contributing, and what
/// is left is the part of the query this process can answer by itself.
#[must_use]
pub fn without_services(pattern: &GraphPattern) -> GraphPattern {
    match pattern {
        GraphPattern::Service { .. } => GraphPattern::Bgp {
            patterns: Vec::new(),
        },
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: Box::new(without_services(left)),
            right: Box::new(without_services(right)),
        },
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => GraphPattern::LeftJoin {
            left: Box::new(without_services(left)),
            right: Box::new(without_services(right)),
            expression: expression.clone(),
        },
        GraphPattern::Union { left, right } => GraphPattern::Union {
            left: Box::new(without_services(left)),
            right: Box::new(without_services(right)),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: Box::new(without_services(left)),
            right: Box::new(without_services(right)),
        },
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr: expr.clone(),
            inner: Box::new(without_services(inner)),
        },
        GraphPattern::Graph { name, inner } => GraphPattern::Graph {
            name: name.clone(),
            inner: Box::new(without_services(inner)),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: Box::new(without_services(inner)),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: Box::new(without_services(inner)),
            expression: expression.clone(),
        },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: Box::new(without_services(inner)),
            variables: variables.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct {
            inner: Box::new(without_services(inner)),
        },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced {
            inner: Box::new(without_services(inner)),
        },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: Box::new(without_services(inner)),
            start: *start,
            length: *length,
        },
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: Box::new(without_services(inner)),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {
            pattern.clone()
        }
    }
}

/// `bound` plus every variable `sibling` makes visible.
fn with_variables(bound: &BTreeSet<Variable>, sibling: &GraphPattern) -> BTreeSet<Variable> {
    let mut out = bound.clone();
    out.extend(pattern_variables(sibling));
    out
}

/// The query's own pattern.
///
/// Every query form has one, and which form it is does not matter to any of this. Matching on it
/// in three separate places is how a `DESCRIBE` ends up silently unoptimised.
#[must_use]
pub fn pattern_of(query: &Query) -> &GraphPattern {
    match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => pattern,
    }
}

/// `query` with `pattern` in place of its own.
#[must_use]
pub fn with_pattern(query: &Query, pattern: GraphPattern) -> Query {
    match query {
        Query::Select {
            dataset, base_iri, ..
        } => Query::Select {
            dataset: dataset.clone(),
            pattern,
            base_iri: base_iri.clone(),
        },
        Query::Construct {
            template,
            dataset,
            base_iri,
            ..
        } => Query::Construct {
            template: template.clone(),
            dataset: dataset.clone(),
            pattern,
            base_iri: base_iri.clone(),
        },
        Query::Describe {
            dataset, base_iri, ..
        } => Query::Describe {
            dataset: dataset.clone(),
            pattern,
            base_iri: base_iri.clone(),
        },
        Query::Ask {
            dataset, base_iri, ..
        } => Query::Ask {
            dataset: dataset.clone(),
            pattern,
            base_iri: base_iri.clone(),
        },
    }
}

/// A `SELECT DISTINCT` over the query's local part, projecting `variables`.
///
/// Always a `SELECT`, whatever `query` is: a `CONSTRUCT` probe would answer with a graph, and
/// what the caller needs is rows. The dataset clauses come across because `FROM` and `FROM NAMED`
/// decide what the local part can see, and a probe over a different dataset would report keys the
/// real evaluation will not.
#[must_use]
pub fn probe_query(query: &Query, variables: Vec<Variable>) -> Query {
    let body = without_services(query_body(pattern_of(query)));
    let (dataset, base_iri) = match query {
        Query::Select {
            dataset, base_iri, ..
        }
        | Query::Construct {
            dataset, base_iri, ..
        }
        | Query::Describe {
            dataset, base_iri, ..
        }
        | Query::Ask {
            dataset, base_iri, ..
        } => (dataset.clone(), base_iri.clone()),
    };
    Query::Select {
        dataset,
        pattern: GraphPattern::Distinct {
            inner: Box::new(GraphPattern::Project {
                inner: Box::new(body),
                variables,
            }),
        },
        base_iri,
    }
}

/// The query body, with the solution modifiers taken off the top.
///
/// The probe must not inherit them. A `LIMIT` would cut the keys short, and which keys survived
/// would depend on evaluation order -- so the probe would sometimes report a *subset*, which is
/// the one thing that loses rows. `Project` has to go for the same reason: a query's own
/// projection need not include its join variables at all.
///
/// `Group` is left in place, and so is `Extend`. Stopping at an aggregate means the probe
/// aggregates too and the join variables come back unbound, so nothing is pushed -- the safe
/// outcome, reached by doing less rather than by reasoning about aggregation.
#[must_use]
pub fn query_body(pattern: &GraphPattern) -> &GraphPattern {
    match pattern {
        GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. } => query_body(inner),
        other => other,
    }
}

/// Keys from the probe's rows, or `None` when pushing them would be wrong or useless.
///
/// `rows` is the probe's answer: one entry per solution, each a value per candidate variable in
/// the same order, `None` where the probe left it unbound or the value could not be sent.
#[must_use]
pub fn keys_from_rows(candidates: &[Variable], rows: &[Vec<Option<GroundTerm>>]) -> Option<Keys> {
    if candidates.is_empty() || rows.is_empty() {
        return None;
    }

    // A variable is pushed only if every row bound it: one unbound row means that solution
    // joins against the whole remote pattern, and a `VALUES` restricting it would lose rows.
    let keep: Vec<usize> = (0..candidates.len())
        .filter(|i| rows.iter().all(|row| row.get(*i).is_some_and(Option::is_some)))
        .collect();
    if keep.is_empty() {
        return None;
    }

    // Distinct, because the probe has one row per local solution and many of them agree on the
    // join variables -- three towns across thirty shops.
    //
    // Keyed on each term's written form rather than on the term, because `GroundTerm` is not
    // `Ord`. That buys the ordering as well as the deduplication: a `BTreeMap` fixes the order
    // of the block, so the query string a host caches on is the same for the same set of keys
    // however the probe happened to produce them. The written form distinguishes any two terms
    // that differ, which is all a key has to do.
    let mut distinct: BTreeMap<String, Vec<GroundTerm>> = BTreeMap::new();
    for row in rows {
        let mut tuple = Vec::with_capacity(keep.len());
        for i in &keep {
            match row.get(*i).and_then(Clone::clone) {
                Some(term) => tuple.push(term),
                // Unreachable while `keep` is derived as above. Giving up rather than emitting
                // an `UNDEF` keeps a bug here from quietly widening the remote query back out
                // to the scan this exists to avoid.
                None => return None,
            }
        }
        let key = tuple
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        distinct.insert(key, tuple);
        if distinct.len() > MAX_PUSHED_BINDINGS {
            return None;
        }
    }

    Some(Keys {
        variables: keep.iter().map(|i| candidates[*i].clone()).collect(),
        rows: distinct
            .into_values()
            .map(|tuple| tuple.into_iter().map(Some).collect())
            .collect(),
    })
}
