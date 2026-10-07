//! `SELECT DISTINCT ?p WHERE { ?s ?p ?o }`, from the predicates the store already knows.
//!
//! # The gap
//!
//! The commonest first question anyone asks of an unfamiliar store — what predicates does it
//! use? — is a scan of every triple in it, kept for the few dozen distinct values it produces.
//! The admission check sees a `DISTINCT` over the whole store, which it estimates at the whole
//! store, so the query is either refused or sent down the spilling path, which is the slow one
//! by design. On `GeoNames` it took over a minute and left the server holding 13.7 GB.
//!
//! But the store keeps an exact, persisted count of every predicate, maintained on every write
//! (`Store::predicate_histogram`). The list of predicates is already there.
//!
//! # Why the counts are not simply returned
//!
//! Two reasons, and either alone would make a predicate list read from the counts wrong:
//!
//! * **The counts cover every graph.** The query asks about the default graph, and a predicate
//!   used only in a named graph is not in its answer.
//! * **The counts ignore policy.** A predicate used only in triples this session may not read
//!   is not in its answer either — and naming it would disclose exactly what the policy hides.
//!   `Storage::predicate_count` says as much about itself.
//!
//! So the counts supply *candidates*, and each is confirmed by asking the session's own view
//! for one visible quad with that predicate in the default graph — the same call, through the
//! same policy and graph filter, that the evaluator makes when it scans. That is one seek per
//! predicate rather than one read per triple. Under a restrictive policy a seek may have to
//! pass denied quads before finding a visible one, which costs time, never correctness.
//!
//! The confirmed predicates replace the pattern as a `VALUES` table, and the evaluator does the
//! `DISTINCT`, any `ORDER BY ?p` and any `LIMIT` as it always did.
//!
//! # The shape
//!
//! `DISTINCT` (or `REDUCED`) over a projection of exactly the predicate variable, over one
//! triple pattern whose subject and object are two other variables or blank nodes. Optionally
//! ordered by expressions over the predicate alone, and sliced. Anything else — a constant in
//! the pattern, the same variable twice, a second pattern, a filter — is left as it was: each
//! makes the answer depend on more than whether a predicate is used at all.

use crate::view::{DatasetView, ViewError};
use spareval::QueryableDataset;
use spargebra::algebra::{GraphPattern, OrderExpression};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, Variable};
use spargebra::Query;

/// `query` with its single pattern replaced by the predicates visible in the default graph, or
/// `None` where the query is not the shape this answers.
///
/// # Errors
///
/// What the store or the policy returns while confirming a candidate.
pub fn materialised(view: &DatasetView<'_>, query: &Query) -> Result<Option<Query>, ViewError> {
    let Query::Select { pattern, .. } = query else {
        return Ok(None);
    };
    let Some(predicate) = shape(pattern) else {
        return Ok(None);
    };

    let dataset = &view;
    let mut bindings: Vec<Vec<Option<GroundTerm>>> = Vec::new();
    for (id, _) in view.store().predicate_histogram() {
        let Some(first) = dataset
            .internal_quads_for_pattern(None, Some(&id), None, Some(None))
            .next()
        else {
            continue;
        };
        first?;
        let Some(term) = view.decode_term(id)? else {
            continue;
        };
        let Ok(term) = GroundTerm::try_from(term) else {
            continue;
        };
        bindings.push(vec![Some(term)]);
    }
    let table = GraphPattern::Values {
        variables: vec![predicate.clone()],
        bindings,
    };
    Ok(Some(crate::boundjoin::with_pattern(
        query,
        replace_pattern(pattern, &table),
    )))
}

/// The predicate variable, if `pattern` is the shape described in the module comment.
fn shape(pattern: &GraphPattern) -> Option<&Variable> {
    let inner = match pattern {
        GraphPattern::Slice { inner, .. } => inner,
        other => other,
    };
    let (GraphPattern::Distinct { inner } | GraphPattern::Reduced { inner }) = inner else {
        return None;
    };
    let GraphPattern::Project { inner, variables } = inner.as_ref() else {
        return None;
    };
    let [projected] = variables.as_slice() else {
        return None;
    };
    let inner = match inner.as_ref() {
        GraphPattern::OrderBy { inner, expression }
            if expression.iter().all(|e| mentions_only(e, projected)) =>
        {
            inner.as_ref()
        }
        other => other,
    };
    let GraphPattern::Bgp { patterns } = inner else {
        return None;
    };
    let [triple] = patterns.as_slice() else {
        return None;
    };
    let NamedNodePattern::Variable(predicate) = &triple.predicate else {
        return None;
    };
    if predicate != projected {
        return None;
    }
    // Subject and object free, distinct from each other and from the predicate: then the
    // pattern has a solution with predicate `p` exactly when some quad uses `p`.
    let free = |term: &TermPattern| match term {
        TermPattern::Variable(v) => Some(format!("?{}", v.as_str())),
        TermPattern::BlankNode(b) => Some(format!("_:{}", b.as_str())),
        _ => None,
    };
    let (subject, object) = (free(&triple.subject)?, free(&triple.object)?);
    let predicate_key = format!("?{}", predicate.as_str());
    if subject == object || subject == predicate_key || object == predicate_key {
        return None;
    }
    Some(predicate)
}

fn mentions_only(order: &OrderExpression, variable: &Variable) -> bool {
    let (OrderExpression::Asc(e) | OrderExpression::Desc(e)) = order;
    let mut found = Vec::new();
    crate::bindjoin::inspect_expression(e, &mut found) && found.iter().all(|v| v == variable)
}

/// `pattern` with its one basic graph pattern replaced by `table`.
fn replace_pattern(pattern: &GraphPattern, table: &GraphPattern) -> GraphPattern {
    let down = |inner: &GraphPattern| Box::new(replace_pattern(inner, table));
    match pattern {
        GraphPattern::Bgp { .. } => table.clone(),
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: down(inner),
            start: *start,
            length: *length,
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct { inner: down(inner) },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced { inner: down(inner) },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: down(inner),
            variables: variables.clone(),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: down(inner),
            expression: expression.clone(),
        },
        // `shape` accepted this path, so nothing else is reached.
        other => other.clone(),
    }
}
