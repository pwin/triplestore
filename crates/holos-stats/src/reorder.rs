//! Reordering a basic graph pattern before evaluation.
//!
//! # Why this can work at all
//!
//! `sparopt` has no injection point — `Optimizer::optimize_graph_pattern` is a free
//! function the evaluator calls internally — so statistics cannot be handed to the planner.
//! But `examples/does_order_matter.rs` established something that makes a detour possible:
//! **written order survives into the plan**. The same query with its selective pattern
//! written last cost 3× more, which it could not have done if the optimiser were fully
//! reordering.
//!
//! So the statistics are applied *before* the query reaches the evaluator, by rewriting the
//! algebra. The evaluator then optimises an already-good order instead of a arbitrary one.
//! This is not a planner — it does not choose join algorithms, and it cannot see what the
//! evaluator will do afterwards. It is the one lever available from outside.
//!
//! # The ordering
//!
//! Greedy, and connectivity-aware:
//!
//! 1. Start from the pattern with the smallest estimated cardinality.
//! 2. Repeatedly take the smallest-estimate pattern that **shares a variable** with what is
//!    already chosen.
//! 3. Only when nothing connects, take the smallest remaining and start a new component.
//!
//! Step 2 is what matters. Ordering purely by size would happily put two unrelated patterns
//! next to each other and build a cross product between them, which is worse than any
//! ordering mistake step 1 could make.

use crate::{Pattern, Statistics};
use holos_core::TermId;
use holos_store::Store;
use rustc_hash::FxHashSet;
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

/// Rewrites every basic graph pattern in a query into estimated-cardinality order.
///
/// Purely a reordering: the same patterns, the same variables, the same answers. SPARQL
/// gives a BGP set semantics, so permuting it cannot change the result — only the work
/// done to reach it.
#[must_use]
pub fn reorder_query(
    query: &spargebra::Query,
    stats: &Statistics,
    store: &Store,
) -> spargebra::Query {
    let ctx = Context { stats, store };
    match query {
        spargebra::Query::Select {
            dataset,
            pattern,
            base_iri,
        } => spargebra::Query::Select {
            dataset: dataset.clone(),
            pattern: ctx.rewrite(pattern),
            base_iri: base_iri.clone(),
        },
        spargebra::Query::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => spargebra::Query::Construct {
            template: template.clone(),
            dataset: dataset.clone(),
            pattern: ctx.rewrite(pattern),
            base_iri: base_iri.clone(),
        },
        spargebra::Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => spargebra::Query::Describe {
            dataset: dataset.clone(),
            pattern: ctx.rewrite(pattern),
            base_iri: base_iri.clone(),
        },
        spargebra::Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => spargebra::Query::Ask {
            dataset: dataset.clone(),
            pattern: ctx.rewrite(pattern),
            base_iri: base_iri.clone(),
        },
    }
}

struct Context<'a> {
    stats: &'a Statistics,
    store: &'a Store,
}

impl Context<'_> {
    /// Walks the algebra, reordering every `Bgp` and leaving everything else alone.
    fn rewrite(&self, pattern: &GraphPattern) -> GraphPattern {
        let go = |p: &GraphPattern| Box::new(self.rewrite(p));
        match pattern {
            GraphPattern::Bgp { patterns } => GraphPattern::Bgp {
                patterns: self.order(patterns),
            },

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
            GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
                expr: expr.clone(),
                inner: go(inner),
            },
            GraphPattern::Union { left, right } => GraphPattern::Union {
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
            GraphPattern::Minus { left, right } => GraphPattern::Minus {
                left: go(left),
                right: go(right),
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

            // Paths, VALUES and SERVICE hold no BGP to reorder. Cloning rather than
            // matching exhaustively would be shorter, but this way a new variant added
            // upstream fails to compile instead of being silently skipped.
            other @ (GraphPattern::Path { .. }
            | GraphPattern::Values { .. }
            | GraphPattern::Service { .. }) => other.clone(),
        }
    }

    /// Greedy, connectivity-aware ordering.
    fn order(&self, patterns: &[TriplePattern]) -> Vec<TriplePattern> {
        if patterns.len() < 2 {
            return patterns.to_vec();
        }

        let estimates: Vec<f64> = patterns.iter().map(|p| self.estimate(p)).collect();
        let variables: Vec<FxHashSet<String>> = patterns.iter().map(variables_of).collect();

        let mut remaining: Vec<usize> = (0..patterns.len()).collect();
        let mut chosen: Vec<usize> = Vec::with_capacity(patterns.len());
        let mut bound: FxHashSet<String> = FxHashSet::default();

        while !remaining.is_empty() {
            // Among those that share a variable with what is already chosen, the smallest.
            // Falling back to the smallest overall only when nothing connects is what stops
            // a cheap-but-unrelated pattern being pulled in ahead of a join.
            let connected: Vec<&usize> = remaining
                .iter()
                .filter(|i| !variables[**i].is_disjoint(&bound))
                .collect();

            let pick = if chosen.is_empty() || connected.is_empty() {
                *remaining
                    .iter()
                    .min_by(|a, b| estimates[**a].total_cmp(&estimates[**b]))
                    .expect("remaining is non-empty")
            } else {
                **connected
                    .iter()
                    .min_by(|a, b| estimates[***a].total_cmp(&estimates[***b]))
                    .expect("connected is non-empty")
            };

            remaining.retain(|i| *i != pick);
            bound.extend(variables[pick].iter().cloned());
            chosen.push(pick);
        }

        chosen.into_iter().map(|i| patterns[i].clone()).collect()
    }

    /// Estimated rows for one triple pattern, using the characteristic sets.
    fn estimate(&self, pattern: &TriplePattern) -> f64 {
        let resolve = |t: &TermPattern| -> Option<TermId> {
            let term = match t {
                TermPattern::NamedNode(n) => oxrdf::TermRef::NamedNode(n.as_ref()),
                TermPattern::Literal(l) => oxrdf::TermRef::Literal(l.as_ref()),
                // A blank node in a query is a variable in all but name, and a triple term
                // pattern is not something the statistics describe.
                _ => return None,
            };
            self.store.lookup_term(term).ok().flatten()
        };
        let predicate = match &pattern.predicate {
            NamedNodePattern::NamedNode(n) => {
                self.store.lookup_term(n.as_ref().into()).ok().flatten()
            }
            NamedNodePattern::Variable(_) => None,
        };

        self.stats.estimate_pattern(&Pattern::single(
            resolve(&pattern.subject),
            predicate,
            resolve(&pattern.object),
        ))
    }
}

fn variables_of(pattern: &TriplePattern) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    let mut add_term = |t: &TermPattern| {
        if let TermPattern::Variable(v) = t {
            out.insert(v.as_str().to_owned());
        }
    };
    add_term(&pattern.subject);
    add_term(&pattern.object);
    if let NamedNodePattern::Variable(v) = &pattern.predicate {
        out.insert(v.as_str().to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_store::GraphFilter;
    use oxrdf::vocab::rdf;
    use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
    use spargebra::SparqlParser;

    const EX: &str = "http://example.com/";

    fn ex(name: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("{EX}{name}"))
    }

    /// 500 people, all with a name, one with a badge.
    fn store() -> Store {
        let mut store = Store::new();
        for i in 0..500 {
            let s = ex(&format!("p{i}"));
            for (p, o) in [
                (rdf::TYPE.into_owned(), Term::NamedNode(ex("Person"))),
                (
                    ex("name"),
                    Literal::new_simple_literal(format!("P{i}")).into(),
                ),
                (
                    ex("email"),
                    Literal::new_simple_literal(format!("p{i}@x")).into(),
                ),
            ] {
                store
                    .insert(
                        Quad {
                            subject: s.clone().into(),
                            predicate: p,
                            object: o,
                            graph_name: GraphName::DefaultGraph,
                        }
                        .as_ref(),
                    )
                    .expect("insert");
            }
        }
        store
            .insert(
                Quad {
                    subject: ex("p0").into(),
                    predicate: ex("badge"),
                    object: Literal::new_simple_literal("B0").into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
        store
    }

    fn bgp_of(query: &spargebra::Query) -> Vec<TriplePattern> {
        fn find(p: &GraphPattern) -> Option<Vec<TriplePattern>> {
            match p {
                GraphPattern::Bgp { patterns } => Some(patterns.clone()),
                GraphPattern::Project { inner, .. }
                | GraphPattern::Distinct { inner }
                | GraphPattern::Reduced { inner }
                | GraphPattern::Slice { inner, .. }
                | GraphPattern::Filter { inner, .. }
                | GraphPattern::OrderBy { inner, .. }
                | GraphPattern::Group { inner, .. }
                | GraphPattern::Extend { inner, .. } => find(inner),
                GraphPattern::Join { left, right } => find(left).or_else(|| find(right)),
                _ => None,
            }
        }
        let pattern = match query {
            spargebra::Query::Select { pattern, .. }
            | spargebra::Query::Ask { pattern, .. }
            | spargebra::Query::Construct { pattern, .. }
            | spargebra::Query::Describe { pattern, .. } => pattern,
        };
        find(pattern).unwrap_or_default()
    }

    fn predicates(patterns: &[TriplePattern]) -> Vec<String> {
        patterns
            .iter()
            .map(|p| match &p.predicate {
                NamedNodePattern::NamedNode(n) => n
                    .as_str()
                    .trim_start_matches(EX)
                    .rsplit('#')
                    .next()
                    .unwrap_or("?")
                    .to_owned(),
                NamedNodePattern::Variable(v) => format!("?{}", v.as_str()),
            })
            .collect()
    }

    #[test]
    fn the_selective_pattern_is_moved_first() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).expect("stats");
        let query = SparqlParser::new()
            .parse_query(&format!(
                "PREFIX ex: <{EX}> SELECT * WHERE {{ ?s ex:name ?n . ?s ex:email ?e . ?s ex:badge ?b }}"
            ))
            .expect("parse");

        let before = predicates(&bgp_of(&query));
        assert_eq!(before, vec!["name", "email", "badge"]);

        let after = predicates(&bgp_of(&reorder_query(&query, &stats, &store)));
        assert_eq!(
            after[0], "badge",
            "the 1-row pattern must lead, not the 500-row one: {after:?}"
        );
    }

    #[test]
    fn an_already_good_order_is_left_alone() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).expect("stats");
        let query = SparqlParser::new()
            .parse_query(&format!(
                "PREFIX ex: <{EX}> SELECT * WHERE {{ ?s ex:badge ?b . ?s ex:name ?n }}"
            ))
            .expect("parse");
        let after = predicates(&bgp_of(&reorder_query(&query, &stats, &store)));
        assert_eq!(after, vec!["badge", "name"]);
    }

    #[test]
    fn disconnected_patterns_do_not_become_a_cross_product_early() {
        // ?a and ?b share nothing. Whatever order is chosen, the two patterns about one
        // subject must stay adjacent rather than being split by the unrelated one.
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).expect("stats");
        let query = SparqlParser::new()
            .parse_query(&format!(
                "PREFIX ex: <{EX}> SELECT * WHERE {{ ?a ex:name ?n . ?b ex:email ?e . ?a ex:badge ?x }}"
            ))
            .expect("parse");
        let after = predicates(&bgp_of(&reorder_query(&query, &stats, &store)));
        let badge = after.iter().position(|p| p == "badge").expect("badge");
        let name = after.iter().position(|p| p == "name").expect("name");
        assert!(
            badge.abs_diff(name) == 1,
            "the connected pair must stay together: {after:?}"
        );
    }

    #[test]
    fn a_single_pattern_is_untouched() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).expect("stats");
        let query = SparqlParser::new()
            .parse_query(&format!(
                "PREFIX ex: <{EX}> SELECT * WHERE {{ ?s ex:name ?n }}"
            ))
            .expect("parse");
        assert_eq!(
            predicates(&bgp_of(&reorder_query(&query, &stats, &store))),
            vec!["name"]
        );
    }

    #[test]
    fn reordering_preserves_the_pattern_set() {
        // The safety property. A BGP has set semantics, so permuting it cannot change the
        // answer — but only if it really is a permutation.
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).expect("stats");
        let query = SparqlParser::new()
            .parse_query(&format!(
                "PREFIX ex: <{EX}> SELECT * WHERE {{ ?s ex:name ?n . ?s ex:email ?e . ?s ex:badge ?b . ?s a ex:Person }}"
            ))
            .expect("parse");
        let mut before = bgp_of(&query);
        let mut after = bgp_of(&reorder_query(&query, &stats, &store));
        assert_eq!(before.len(), after.len());
        before.sort_by_key(ToString::to_string);
        after.sort_by_key(ToString::to_string);
        assert_eq!(before, after, "reordering must be a permutation");
    }

    #[test]
    fn nested_patterns_are_reached() {
        // The BGP inside an OPTIONAL is still a BGP, and gets the same treatment.
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).expect("stats");
        let query = SparqlParser::new()
            .parse_query(&format!(
                "PREFIX ex: <{EX}> SELECT * WHERE {{ ?s ex:name ?n \
                 OPTIONAL {{ ?s ex:email ?e . ?s ex:badge ?b }} }}"
            ))
            .expect("parse");
        let rewritten = reorder_query(&query, &stats, &store);
        // Rendering the whole query is the simplest way to see inside the OPTIONAL.
        let text = rewritten.to_string();
        let badge = text.find("badge").expect("badge present");
        let email = text.find("email").expect("email present");
        assert!(
            badge < email,
            "the rarer pattern should lead inside the OPTIONAL: {text}"
        );
    }
}
