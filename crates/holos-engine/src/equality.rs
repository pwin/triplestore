//! A `FILTER` that names a constant, used as one.
//!
//! # What goes wrong without it
//!
//! ```sparql
//! ?place gn:featureClass gn:P ;
//!        gn:countryCode ?code ;
//!        gn:alternateName ?name .
//! FILTER (?code = "GB")
//! FILTER (lang(?name) = "en" && str(?name) = "Glasgow")
//! ```
//!
//! Both filters say exactly which term a variable must be, and neither planner can use that:
//! the bind join orders patterns by what is *bound*, and `sparopt` by the same, and a variable
//! that a filter pins is not bound until a pattern has bound it. So the query scans every
//! `gn:countryCode` and every `gn:alternateName` in the store and keeps one row in a hundred
//! thousand. Measured on `GeoNames` — 13.4 million country codes, 16.3 million alternate names
//! — that was **306 s** through the evaluator and **131 s** through the bind join, for one row.
//!
//! The rewrite gives the planner the constant as a binding:
//!
//! ```text
//! Filter(e, P)   ->   Filter(e, Join(VALUES ?code { "GB" }, P))
//! ```
//!
//! A one-row `VALUES` is the cheapest thing in any plan, so `?code` is bound before any pattern
//! that mentions it runs, and `?place gn:countryCode "GB"` becomes a lookup.
//!
//! On the same store, with `--reorder`: **0.25 s**, against 86 s for the same binary without
//! this rewrite. Without statistics it is 97 s, and that is not this module's to fix: once the
//! constants are bound, `gn:featureClass gn:P`, `gn:countryCode "GB"` and
//! `gn:alternateName "Glasgow"@en` each have one free position, the bind join cannot tell five
//! million rows from three by counting positions, and source order picks the first.
//!
//! # Why the answer cannot change
//!
//! Three conditions, each of which is what makes the join and the filter agree:
//!
//! 1. **The candidates are every term the conjunct accepts.** `?v = <iri>` holds only for
//!    that IRI, and `?v = "s"` only for the plain string `"s"`: SPARQL's `=` on a plain string
//!    compares strings, and against a language-tagged string, an IRI or any other datatype it
//!    is false or an error, which a filter treats alike. That is why a number or a date is not
//!    taken — `?v = 1` also holds for `"01"^^xsd:integer` and `1.0`, and the candidates would
//!    be unbounded. `sameTerm` is exact for any constant.
//! 2. **The variable is certainly bound, and by the store.** Joining with `VALUES` keeps a
//!    solution whose `?v` is *unbound*, where the filter would have rejected it, so `?v` must
//!    be bound on every path through `P` — and bound by a triple or path pattern, which only
//!    ever binds terms the store holds. That second half is what allows a candidate the store
//!    does not hold to be dropped rather than sent: it cannot match, and the bind join abandons
//!    a plan whose `VALUES` names an unknown term.
//! 3. **The filter stays.** The join only narrows; the filter still decides. So a candidate
//!    set that were ever too generous would cost speed, not rows.
//!
//! Distinct candidates match a solution at most once each, so multiplicity is preserved too.
//!
//! # The label idiom
//!
//! `lang(?x) = "en" && str(?x) = "Glasgow"` is how a query names a label, and neither half pins
//! anything alone. Together they do: a term whose language is `"en"` is a language-tagged
//! string tagged exactly `en` — the comparison is a string comparison, so case matters — and
//! RDF 1.2 allows it a base direction, so the candidates are `"Glasgow"@en`,
//! `"Glasgow"@en--ltr` and `"Glasgow"@en--rtl`. The tag is used exactly as the query wrote it
//! and not normalised, because the store compares terms by their stored form, and a tag the
//! query spelled `EN` must match a stored `EN` and nothing else. An empty tag is not taken:
//! `lang(?x) = ""` admits a string of every datatype.
//!
//! # What it does not reach
//!
//! A pin inside `OPTIONAL`'s condition, `EXISTS`, or a subquery's projection; a variable bound
//! only by `VALUES`, `BIND` or a `SERVICE`; and any comparison other than the ones above. Each
//! is left exactly as written.

use oxrdf::{BaseDirection, Literal, TermRef};
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, Variable};
use std::collections::BTreeSet;

/// `pattern` with every pinning filter given its constants as a `VALUES` join.
///
/// `known` answers whether the store holds a term. A candidate it rejects is dropped, which is
/// sound only because a pinned variable is bound by the store (see the module comment).
#[must_use]
pub fn rewrite(pattern: &GraphPattern, known: &dyn Fn(TermRef<'_>) -> bool) -> GraphPattern {
    let go = |p: &GraphPattern| Box::new(rewrite(p, known));
    match pattern {
        GraphPattern::Filter { expr, inner } => {
            let inner = rewrite(inner, known);
            let pinned = pins_within(expr, &inner);
            let mut out = inner;
            for (variable, candidates) in pinned {
                let mut rows: Vec<Vec<Option<GroundTerm>>> = Vec::new();
                for candidate in candidates {
                    let term: oxrdf::Term = candidate.clone().into();
                    if known(term.as_ref()) {
                        rows.push(vec![Some(candidate)]);
                    }
                }
                out = GraphPattern::Join {
                    left: Box::new(GraphPattern::Values {
                        variables: vec![variable],
                        bindings: rows,
                    }),
                    right: Box::new(out),
                };
            }
            GraphPattern::Filter {
                expr: expr.clone(),
                inner: Box::new(out),
            }
        }
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {
            pattern.clone()
        }
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: go(left),
            right: go(right),
        },
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => GraphPattern::LeftJoin {
            left: go(left),
            right: go(right),
            expression: expression.clone(),
        },
        GraphPattern::Union { left, right } => GraphPattern::Union {
            left: go(left),
            right: go(right),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: go(left),
            right: go(right),
        },
        GraphPattern::Graph { name, inner } => GraphPattern::Graph {
            name: name.clone(),
            inner: go(inner),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: go(inner),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: go(inner),
            expression: expression.clone(),
        },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: go(inner),
            variables: variables.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct { inner: go(inner) },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced { inner: go(inner) },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: go(inner),
            start: *start,
            length: *length,
        },
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: go(inner),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },
        // Left alone: what a remote endpoint holds is not what `known` answers for.
        GraphPattern::Service { .. } => pattern.clone(),
    }
}

/// Each variable `expr` pins that `inner` binds from the store, with every term the pin admits.
///
/// The rewrite's rule, shared with [`crate::admit`] so that what is estimated as bound is what
/// the planner will be given as bound.
pub(crate) fn pins_within(
    expr: &Expression,
    inner: &GraphPattern,
) -> Vec<(Variable, Vec<GroundTerm>)> {
    let bound = store_bound(inner);
    pins(expr)
        .into_iter()
        .filter(|(variable, _)| bound.contains(variable))
        .collect()
}

/// The variables bound on every path through `pattern` by a triple or path pattern.
///
/// Deliberately narrower than "certainly bound": `VALUES`, `BIND` and `SERVICE` bind terms the
/// store need not hold, and a subquery's projection is not followed. Narrower only ever means
/// a pin is not used.
fn store_bound(pattern: &GraphPattern) -> BTreeSet<Variable> {
    let mut out = BTreeSet::new();
    match pattern {
        GraphPattern::Bgp { patterns } => {
            for triple in patterns {
                term_variable(&triple.subject, &mut out);
                if let NamedNodePattern::Variable(v) = &triple.predicate {
                    out.insert(v.clone());
                }
                term_variable(&triple.object, &mut out);
            }
        }
        GraphPattern::Path {
            subject, object, ..
        } => {
            term_variable(subject, &mut out);
            term_variable(object, &mut out);
        }
        GraphPattern::Join { left, right } => {
            out = store_bound(left);
            out.extend(store_bound(right));
        }
        GraphPattern::LeftJoin { left, .. } => out = store_bound(left),
        GraphPattern::Union { left, right } => {
            let right = store_bound(right);
            out = store_bound(left)
                .into_iter()
                .filter(|v| right.contains(v))
                .collect();
        }
        GraphPattern::Filter { inner, .. } | GraphPattern::Extend { inner, .. } => {
            out = store_bound(inner);
        }
        GraphPattern::Graph { name, inner } => {
            out = store_bound(inner);
            // `GRAPH ?g` binds ?g to a graph the store has, but only where the inner pattern
            // matched something in it, which `inner` being store-bound already guarantees.
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.clone());
            }
        }
        _ => {}
    }
    out
}

/// A variable in a top-level position. A variable inside a quoted triple pattern is bound too,
/// but the bind join does not reach those, so nothing is gained by counting it.
fn term_variable(term: &TermPattern, out: &mut BTreeSet<Variable>) {
    if let TermPattern::Variable(v) = term {
        out.insert(v.clone());
    }
}

/// Each variable `expr` pins, with every term the pin admits.
fn pins(expr: &Expression) -> Vec<(Variable, Vec<GroundTerm>)> {
    let mut conjuncts = Vec::new();
    flatten(expr, &mut conjuncts);

    let mut out = Vec::new();
    let mut strings: Vec<(&Variable, &str)> = Vec::new();
    let mut languages: Vec<(&Variable, &str)> = Vec::new();
    for conjunct in conjuncts {
        match conjunct {
            Expression::Equal(a, b) => {
                if let Some((v, term)) = variable_and_constant(a, b) {
                    if let Some(term) = equal_admits(term) {
                        out.push((v.clone(), vec![term]));
                    }
                } else if let Some((v, s)) = function_and_string(a, b, &Function::Str) {
                    strings.push((v, s));
                } else if let Some((v, s)) = function_and_string(a, b, &Function::Lang) {
                    languages.push((v, s));
                }
            }
            Expression::SameTerm(a, b) => {
                if let Some((v, term)) = variable_and_constant(a, b) {
                    out.push((v.clone(), vec![term]));
                }
            }
            _ => {}
        }
    }
    for (variable, value) in &strings {
        for (other, language) in &languages {
            if variable != other || language.is_empty() {
                continue;
            }
            let candidates = [None, Some(BaseDirection::Ltr), Some(BaseDirection::Rtl)]
                .into_iter()
                .map(|direction| {
                    GroundTerm::Literal(match direction {
                        None => Literal::new_language_tagged_literal_unchecked(*value, *language),
                        Some(d) => Literal::new_directional_language_tagged_literal_unchecked(
                            *value, *language, d,
                        ),
                    })
                })
                .collect();
            out.push(((*variable).clone(), candidates));
        }
    }
    out
}

fn flatten<'e>(expr: &'e Expression, out: &mut Vec<&'e Expression>) {
    if let Expression::And(a, b) = expr {
        flatten(a, out);
        flatten(b, out);
    } else {
        out.push(expr);
    }
}

/// `?v` and a constant, in either order.
fn variable_and_constant<'e>(
    a: &'e Expression,
    b: &'e Expression,
) -> Option<(&'e Variable, GroundTerm)> {
    let constant = |e: &Expression| match e {
        Expression::NamedNode(n) => Some(GroundTerm::NamedNode(n.clone())),
        Expression::Literal(l) => Some(GroundTerm::Literal(l.clone())),
        _ => None,
    };
    match (a, b) {
        (Expression::Variable(v), other) | (other, Expression::Variable(v)) => {
            constant(other).map(|c| (v, c))
        }
        _ => None,
    }
}

/// The one term `?v = term` admits, where there is exactly one: an IRI, or a plain string.
fn equal_admits(term: GroundTerm) -> Option<GroundTerm> {
    match &term {
        GroundTerm::NamedNode(_) => Some(term),
        GroundTerm::Literal(l) if is_plain(l) => Some(term),
        _ => None,
    }
}

fn is_plain(literal: &Literal) -> bool {
    literal.datatype() == oxrdf::vocab::xsd::STRING
}

/// `function(?v) = "s"`, in either order, with `"s"` a plain string.
fn function_and_string<'e>(
    a: &'e Expression,
    b: &'e Expression,
    function: &Function,
) -> Option<(&'e Variable, &'e str)> {
    let call = |e: &'e Expression| match e {
        Expression::FunctionCall(f, args) if f == function => match args.as_slice() {
            [Expression::Variable(v)] => Some(v),
            _ => None,
        },
        _ => None,
    };
    let string = |e: &'e Expression| match e {
        Expression::Literal(l) if is_plain(l) => Some(l.value()),
        _ => None,
    };
    match (call(a), string(b)) {
        (Some(v), Some(s)) => Some((v, s)),
        _ => call(b).zip(string(a)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::{Query, SparqlParser};

    fn pattern(query: &str) -> GraphPattern {
        let query = SparqlParser::new()
            .parse_query(&format!("PREFIX ex: <http://example.com/> {query}"))
            .expect("parse");
        let Query::Select { pattern, .. } = query else {
            panic!("select")
        };
        pattern
    }

    /// Every `VALUES` the rewrite added, as `?v=term,term` — not counting the query's own.
    fn pinned(query: &str) -> Vec<String> {
        let original = pattern(query);
        let mut before = Vec::new();
        collect(&original, &mut before);
        let mut out = Vec::new();
        collect(&rewrite(&original, &|_| true), &mut out);
        for written in before {
            let at = out.iter().position(|v| *v == written).expect("kept");
            out.remove(at);
        }
        out.sort();
        out
    }

    fn assert_unpinned(query: &str) {
        assert_eq!(pinned(query), Vec::<String>::new(), "{query}");
    }

    fn collect(pattern: &GraphPattern, out: &mut Vec<String>) {
        match pattern {
            GraphPattern::Values {
                variables,
                bindings,
            } => {
                let terms: Vec<String> = bindings
                    .iter()
                    .map(|row| row[0].as_ref().map_or("UNDEF".into(), ToString::to_string))
                    .collect();
                out.push(format!("{}={}", variables[0], terms.join(",")));
            }
            GraphPattern::Join { left, right }
            | GraphPattern::LeftJoin { left, right, .. }
            | GraphPattern::Union { left, right }
            | GraphPattern::Minus { left, right } => {
                collect(left, out);
                collect(right, out);
            }
            GraphPattern::Filter { inner, .. }
            | GraphPattern::Graph { inner, .. }
            | GraphPattern::Extend { inner, .. }
            | GraphPattern::OrderBy { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. }
            | GraphPattern::Group { inner, .. }
            | GraphPattern::Service { inner, .. } => collect(inner, out),
            GraphPattern::Bgp { .. } | GraphPattern::Path { .. } => {}
        }
    }

    #[test]
    fn a_plain_string_and_an_iri_are_pinned() {
        assert_eq!(
            pinned(r#"SELECT * { ?s ex:code ?c ; ex:kind ?k FILTER(?c = "GB" && ex:A = ?k) }"#),
            vec![r#"?c="GB""#, "?k=<http://example.com/A>"]
        );
    }

    #[test]
    fn a_number_a_date_and_a_tagged_string_are_not() {
        // Each of these is `=` to terms other than itself.
        for constant in ["1", r#""2020-01-01"^^xsd:date"#, r#""x"@en"#] {
            let q = format!(
                "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT * {{ ?s ex:p ?o FILTER(?o = {constant}) }}"
            );
            assert_eq!(pinned(&q), Vec::<String>::new(), "{constant}");
        }
    }

    #[test]
    fn same_term_pins_any_constant() {
        assert_eq!(
            pinned("SELECT * { ?s ex:p ?o FILTER(sameTerm(?o, 1)) }"),
            vec![r#"?o="1"^^<http://www.w3.org/2001/XMLSchema#integer>"#]
        );
    }

    #[test]
    fn the_label_idiom_pins_three_terms() {
        assert_eq!(
            pinned(
                r#"SELECT * { ?s ex:name ?n FILTER(lang(?n) = "en") FILTER("Glasgow" = str(?n)) }"#
            ),
            vec![r#"?n="Glasgow"@en,"Glasgow"@en--ltr,"Glasgow"@en--rtl"#]
        );
    }

    #[test]
    fn half_the_label_idiom_pins_nothing() {
        assert_unpinned(r#"SELECT * { ?s ex:name ?n FILTER(str(?n) = "Glasgow") }"#);
        assert_unpinned(r#"SELECT * { ?s ex:name ?n FILTER(lang(?n) = "en") }"#);
        assert_unpinned(r#"SELECT * { ?s ex:name ?n FILTER(lang(?n) = "" && str(?n) = "x") }"#);
    }

    #[test]
    fn a_variable_the_store_may_not_bind_is_not_pinned() {
        // Only in OPTIONAL: unbound in some solutions, which the filter rejects and a join would keep.
        assert_unpinned(r#"SELECT * { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } FILTER(?v = "a") }"#);
        // Bound by VALUES or BIND: the term need not be in the store.
        assert_unpinned(r#"SELECT * { VALUES ?v { "a" } ?s ex:p ?o FILTER(?v = "a") }"#);
        assert_unpinned(r#"SELECT * { ?s ex:p ?o BIND("a" AS ?v) FILTER(?v = "a") }"#);
        // One UNION branch only.
        assert_unpinned(r#"SELECT * { { ?s ex:p ?v } UNION { ?s ex:q ?o } FILTER(?v = "a") }"#);
        // Not a conjunct.
        assert_unpinned(r#"SELECT * { ?s ex:p ?v FILTER(?v = "a" || ?v = "b") }"#);
    }

    #[test]
    fn both_union_branches_binding_it_is_enough() {
        assert_eq!(
            pinned(r#"SELECT * { { ?s ex:p ?v } UNION { ?s ex:q ?v } FILTER(?v = "a") }"#),
            vec![r#"?v="a""#]
        );
    }

    #[test]
    fn a_term_the_store_lacks_is_dropped_not_sent() {
        let rewritten = rewrite(
            &pattern(r#"SELECT * { ?s ex:p ?v FILTER(?v = "absent") }"#),
            &|_| false,
        );
        let mut out = Vec::new();
        collect(&rewritten, &mut out);
        assert_eq!(out, vec!["?v=".to_string()]);
    }
}
