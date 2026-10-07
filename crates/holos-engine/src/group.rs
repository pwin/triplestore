//! `GROUP BY` over the bind join.
//!
//! # The gap
//!
//! The bind join takes a basic graph pattern, a join, a filter and the rest of its fragment,
//! and refuses aggregation — deliberately, because SPARQL's aggregates have their own rules
//! for errors, unbound values, `DISTINCT` and numeric promotion, and a second implementation of
//! `SUM` that got one of them wrong would be a silent wrong answer. So a query with a
//! `GROUP BY` went to the evaluator whole, and the evaluator scans the right side of every
//! join in full. On `GeoNames`,
//!
//! ```sparql
//! SELECT ?cc (COUNT(*) AS ?n) WHERE {
//!   ?place gn:featureClass gn:P ; gn:countryCode ?cc ; gn:alternateName ?name .
//!   FILTER(?cc = "GB") FILTER(lang(?name) = "en") FILTER(str(?name) = "Glasgow")
//! } GROUP BY ?cc
//! ```
//!
//! took 49.5 s, for one group over one row; without the `GROUP BY` the same pattern is 0.22 s.
//!
//! # What this does instead
//!
//! The pattern *under* the group is evaluated by the bind join, and its solutions replace it as
//! a `VALUES` table:
//!
//! ```text
//! Group(P, keys, aggregates)   ->   Group(VALUES (?vars) { rows of P }, keys, aggregates)
//! ```
//!
//! and the query then goes to the evaluator as usual. The grouping, the aggregates, `HAVING`,
//! the projection and any `ORDER BY` above are all still the evaluator's, unchanged — only
//! where the group's input comes from has moved. The evaluator reads a `VALUES` table without
//! touching the store, so the scans are gone.
//!
//! # Why the answer cannot change
//!
//! A group's input is evaluated bottom-up, independently of everything above it: SPARQL's
//! algebra gives `P` no access to the solutions around it. So `P`'s solutions computed on their
//! own *are* the group's input, and a `VALUES` table holding exactly those rows — every row,
//! with its multiplicity, and `UNDEF` where `P` left a variable unbound — is the same input.
//!
//! The conditions are the ones that keep "on its own" true:
//!
//! * **Only the query's own group**, reached from the top through the solution modifiers,
//!   `HAVING` and the projected expressions. A group in a subquery under `GRAPH ?g` is
//!   evaluated against a graph this does not know, so subqueries are not reached.
//! * **Only where the bind join takes `P` whole**, and only while it stays under its row
//!   budget. Over the budget, or outside the fragment, the query goes to the evaluator exactly
//!   as before.
//! * **No blank nodes in the rows.** A `VALUES` table cannot hold one, so a row with a blank
//!   node in it sends the query to the evaluator rather than being approximated.
//! * **An empty input under a keyless aggregate is not an empty `VALUES`.** `COUNT(*)` over
//!   nothing is one row saying 0, and `sparopt` folds a `Group` over an empty `VALUES` to no
//!   rows. It gets [`crate::equality::nothing`] instead, which is empty only when evaluated.
//!
//! Only `SELECT`. The same caller conditions as the bind join apply: no dataset clause or
//! substitution, and no request for an explanation.

use crate::bindjoin::{Limits, DEFAULT_ROW_BUDGET};
use crate::view::{DatasetView, ViewError};
use holos_stats::Statistics;
use spareval::CancellationToken;
use spargebra::algebra::GraphPattern;
use spargebra::term::{GroundTerm, Variable};
use spargebra::Query;

/// `query` with its group's input replaced by the bind join's rows, or `None` where that does
/// not apply — outside the shape, outside the fragment, over the budget, or a blank node.
///
/// # Errors
///
/// What the store returns while the bind join reads it.
pub fn materialised(
    view: &DatasetView<'_>,
    query: &Query,
    stats: Option<&Statistics>,
    token: Option<&CancellationToken>,
) -> Result<Option<Query>, ViewError> {
    let Query::Select { pattern, .. } = query else {
        return Ok(None);
    };
    let Some((inner, keyless)) = group_input(pattern) else {
        return Ok(None);
    };

    let mut variables: Vec<Variable> = Vec::new();
    inner.on_in_scope_variable(|v| {
        if !variables.contains(v) {
            variables.push(v.clone());
        }
    });
    let probe = Query::Select {
        dataset: None,
        pattern: GraphPattern::Project {
            inner: Box::new(inner.clone()),
            variables: variables.clone(),
        },
        base_iri: None,
    };
    let Some(plan) = crate::bindjoin::plan(&probe) else {
        return Ok(None);
    };
    let limits = Limits {
        rows: Some(DEFAULT_ROW_BUDGET),
        token,
    };
    let Some(rows) = plan.evaluate(view, stats, limits)? else {
        return Ok(None);
    };
    // No input under a keyless aggregate is one row — `COUNT(*)` is 0 — but `sparopt` folds a
    // `Group` over an empty `VALUES` to no rows at all (`GraphPattern::group`, 0.3.7). So that
    // input is given as a pattern that is empty only when evaluated.
    if rows.is_empty() && keyless {
        let store = view.store();
        let known = |term: oxrdf::TermRef<'_>| matches!(store.lookup_term(term), Ok(Some(_)));
        return Ok(crate::equality::nothing(&known).map(|empty| {
            crate::boundjoin::with_pattern(query, replace_group_input(pattern, empty))
        }));
    }

    let mut bindings: Vec<Vec<Option<GroundTerm>>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(row.len());
        for id in row {
            let term = match id {
                None => None,
                Some(id) => match view.decode_term(id)? {
                    None => None,
                    Some(term) => match GroundTerm::try_from(term) {
                        Ok(ground) => Some(ground),
                        // A blank node: not something a `VALUES` table can carry.
                        Err(()) => return Ok(None),
                    },
                },
            };
            values.push(term);
        }
        bindings.push(values);
    }
    let table = GraphPattern::Values {
        variables: plan.variables().to_vec(),
        bindings,
    };
    Ok(Some(crate::boundjoin::with_pattern(
        query,
        replace_group_input(pattern, table),
    )))
}

/// The input of the query's own group — the first `Group` reached from the top through the
/// operators that sit above one in a query's algebra — and whether that group has no keys.
fn group_input(pattern: &GraphPattern) -> Option<(&GraphPattern, bool)> {
    match pattern {
        GraphPattern::Group {
            inner, variables, ..
        } => Some((inner, variables.is_empty())),
        GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Filter { inner, .. }
        | GraphPattern::Extend { inner, .. } => group_input(inner),
        _ => None,
    }
}

/// `pattern` with the input found by [`group_input`] replaced by `table`.
fn replace_group_input(pattern: &GraphPattern, table: GraphPattern) -> GraphPattern {
    match pattern {
        GraphPattern::Group {
            variables,
            aggregates,
            ..
        } => GraphPattern::Group {
            inner: Box::new(table),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: Box::new(replace_group_input(inner, table)),
            variables: variables.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct {
            inner: Box::new(replace_group_input(inner, table)),
        },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced {
            inner: Box::new(replace_group_input(inner, table)),
        },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: Box::new(replace_group_input(inner, table)),
            start: *start,
            length: *length,
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: Box::new(replace_group_input(inner, table)),
            expression: expression.clone(),
        },
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr: expr.clone(),
            inner: Box::new(replace_group_input(inner, table)),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: Box::new(replace_group_input(inner, table)),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        // `group_input` found a group on this path, so nothing else is reached.
        other => other.clone(),
    }
}
