//! GeoSPARQL's topology vocabulary, as query rewriting.
//!
//! GeoSPARQL says the same spatial relation twice, in two grammatical positions:
//!
//! * as a **function** — `FILTER(geof:sfContains(?wktA, ?wktB))`
//! * as a **property** — `?a geo:sfContains ?b`
//!
//! `spargeo` supplies the functions. The properties are not data: nothing asserts
//! `geo:sfContains` triples, so a store that treats them as an ordinary lookup answers
//! every such pattern with nothing — silently, which is the worst way to be wrong. That was
//! the behaviour here until this module existed.
//!
//! The specification's own remedy is the **Query Rewrite Extension**: a topology property
//! is *defined* as shorthand for fetching both geometries and calling the matching
//! function. So this rewrites the algebra before evaluation, turning
//!
//! ```sparql
//! ?a geo:sfContains ?b
//! ```
//!
//! into the equivalent of
//!
//! ```sparql
//! ?a (geo:hasDefaultGeometry|geo:hasGeometry)?/geo:asWKT ?_topo_wkt_0 .
//! ?b (geo:hasDefaultGeometry|geo:hasGeometry)?/geo:asWKT ?_topo_wkt_1 .
//! FILTER(geof:sfContains(?_topo_wkt_0, ?_topo_wkt_1))
//! ```
//!
//! # Why the optional hop
//!
//! The specification defines the rewrite over **features**, reaching their geometry through
//! `geo:hasDefaultGeometry`. But a query may just as well relate two **geometries**, which
//! carry `geo:asWKT` themselves — and the GeoSPARQL example everyone tests against does
//! exactly that. `?` makes the hop optional, so one rewrite serves both without the caller
//! having to know which kind of resource it named.
//!
//! `geo:hasGeometry` is accepted alongside `geo:hasDefaultGeometry` because the entailment
//! that would relate an application's own property (`my:hasExactGeometry rdfs:subPropertyOf
//! geo:hasDefaultGeometry`) to either of them is L4 work that does not exist yet. Without
//! RDFS, a feature linked by a subproperty is still not reachable — [`rewrite`] cannot
//! invent the entailment, and this is documented rather than papered over.
//!
//! # What this is not
//!
//! It is not a spatial index. Every rewritten pattern becomes a join over the geometries
//! that satisfy the surrounding query, and the function runs on each pair — so a topology
//! property between two unbound variables is a cross product. §17's R-tree is what would
//! fix that, and it is not built. Correct answers first; the planner can come later.

use spargebra::algebra::{Expression, Function, GraphPattern, PropertyPathExpression};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern, Variable};
use spargebra::Query;

/// The GeoSPARQL vocabulary namespace, where the *properties* live.
const GEO: &str = "http://www.opengis.net/ont/geosparql#";
/// The GeoSPARQL function namespace, where their *function* counterparts live.
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";

/// Prefix for the variables this rewrite introduces.
///
/// Leading underscore so it cannot collide with anything a query could have written: SPARQL
/// permits `?_x`, but a name in this shape appearing in a user's query and meaning something
/// else would require them to have chosen the same private prefix *and* the same counter.
const TEMP: &str = "_topo_wkt_";

/// The 24 topology relations, each paired with the function that computes it.
///
/// Three families, and all three are in the vocabulary: Simple Features, Egenhofer and
/// RCC8. The local names are identical on both sides, which is what makes the table a list
/// of names rather than a map — but it is written out in full so that a name present in one
/// namespace and absent from the other cannot be rewritten into a function that does not
/// exist.
const RELATIONS: [&str; 24] = [
    // Simple Features
    "sfEquals",
    "sfDisjoint",
    "sfIntersects",
    "sfTouches",
    "sfCrosses",
    "sfWithin",
    "sfContains",
    "sfOverlaps",
    // Egenhofer
    "ehEquals",
    "ehDisjoint",
    "ehMeet",
    "ehOverlap",
    "ehCovers",
    "ehCoveredBy",
    "ehInside",
    "ehContains",
    // RCC8
    "rcc8eq",
    "rcc8dc",
    "rcc8ec",
    "rcc8po",
    "rcc8tppi",
    "rcc8tpp",
    "rcc8ntpp",
    "rcc8ntppi",
];

/// The relation a predicate names, if it is one.
fn relation_of(iri: &str) -> Option<&'static str> {
    let local = iri.strip_prefix(GEO)?;
    RELATIONS.iter().copied().find(|name| *name == local)
}

/// Whether a query mentions any topology property at all.
///
/// Checked first so that the overwhelming majority of queries — which mention none — are
/// returned untouched rather than rebuilt node by node.
#[must_use]
pub fn mentions_topology(query: &Query) -> bool {
    match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => pattern_mentions(pattern),
    }
}

fn pattern_mentions(pattern: &GraphPattern) -> bool {
    let mut found = false;
    visit(pattern, &mut |p| {
        if let GraphPattern::Bgp { patterns } = p {
            found |= patterns.iter().any(|t| {
                matches!(&t.predicate, NamedNodePattern::NamedNode(n)
                    if relation_of(n.as_str()).is_some())
            });
        }
    });
    found
}

/// Calls `f` on every pattern in the tree, parents before children.
fn visit(pattern: &GraphPattern, f: &mut impl FnMut(&GraphPattern)) {
    f(pattern);
    match pattern {
        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            visit(left, f);
            visit(right, f);
        }
        GraphPattern::LeftJoin { left, right, .. } => {
            visit(left, f);
            visit(right, f);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Group { inner, .. } => visit(inner, f),
        GraphPattern::Service { inner, .. } => visit(inner, f),
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {}
    }
}

/// Rewrites every topology property in a query into geometry lookups and a filter.
///
/// Returns the query unchanged when it mentions none, which is nearly always.
#[must_use]
pub fn rewrite(query: &Query) -> Query {
    if !mentions_topology(query) {
        return query.clone();
    }
    let mut counter = 0usize;
    match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut counter),
            base_iri: base_iri.clone(),
        },
        Query::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Query::Construct {
            template: template.clone(),
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut counter),
            base_iri: base_iri.clone(),
        },
        Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => Query::Describe {
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut counter),
            base_iri: base_iri.clone(),
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut counter),
            base_iri: base_iri.clone(),
        },
    }
}

fn rewrite_pattern(pattern: &GraphPattern, counter: &mut usize) -> GraphPattern {
    match pattern {
        GraphPattern::Bgp { patterns } => rewrite_bgp(patterns, counter),
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: Box::new(rewrite_pattern(left, counter)),
            right: Box::new(rewrite_pattern(right, counter)),
        },
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => GraphPattern::LeftJoin {
            left: Box::new(rewrite_pattern(left, counter)),
            right: Box::new(rewrite_pattern(right, counter)),
            expression: expression.clone(),
        },
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr: expr.clone(),
            inner: Box::new(rewrite_pattern(inner, counter)),
        },
        GraphPattern::Union { left, right } => GraphPattern::Union {
            left: Box::new(rewrite_pattern(left, counter)),
            right: Box::new(rewrite_pattern(right, counter)),
        },
        GraphPattern::Graph { name, inner } => GraphPattern::Graph {
            name: name.clone(),
            inner: Box::new(rewrite_pattern(inner, counter)),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: Box::new(rewrite_pattern(inner, counter)),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: Box::new(rewrite_pattern(left, counter)),
            right: Box::new(rewrite_pattern(right, counter)),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: Box::new(rewrite_pattern(inner, counter)),
            expression: expression.clone(),
        },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: Box::new(rewrite_pattern(inner, counter)),
            variables: variables.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct {
            inner: Box::new(rewrite_pattern(inner, counter)),
        },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced {
            inner: Box::new(rewrite_pattern(inner, counter)),
        },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: Box::new(rewrite_pattern(inner, counter)),
            start: *start,
            length: *length,
        },
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: Box::new(rewrite_pattern(inner, counter)),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },
        GraphPattern::Service {
            name,
            inner,
            silent,
        } => GraphPattern::Service {
            name: name.clone(),
            // A remote endpoint answers its own topology properties, or does not. Rewriting
            // inside SERVICE would send it geometry lookups it never asked for and may not
            // be able to answer, so the pattern crosses the boundary as written.
            inner: inner.clone(),
            silent: *silent,
        },
        other => other.clone(),
    }
}

/// Splits a BGP into its ordinary patterns and its topology properties.
///
/// The ordinary ones stay in the BGP. Each topology property contributes two geometry paths
/// and one filter, which are layered around it.
fn rewrite_bgp(patterns: &[TriplePattern], counter: &mut usize) -> GraphPattern {
    let mut plain = Vec::new();
    let mut relations = Vec::new();
    for triple in patterns {
        match &triple.predicate {
            NamedNodePattern::NamedNode(n) => match relation_of(n.as_str()) {
                Some(relation) => relations.push((triple.clone(), relation)),
                None => plain.push(triple.clone()),
            },
            NamedNodePattern::Variable(_) => plain.push(triple.clone()),
        }
    }
    if relations.is_empty() {
        return GraphPattern::Bgp {
            patterns: plain.to_vec(),
        };
    }

    let mut result = GraphPattern::Bgp { patterns: plain };
    for (triple, relation) in relations {
        // Each operand becomes an expression. A resource needs its geometry fetched and
        // joined in; a literal *is* the geometry and needs nothing.
        let (left, joined) = operand(&triple.subject, result, counter);
        let (right, joined) = operand(&triple.object, joined, counter);
        result = GraphPattern::Filter {
            expr: call(relation, left, right),
            inner: Box::new(joined),
        };
    }
    result
}

/// Turns one side of a topology property into an expression the function can take.
///
/// A literal operand is used as written. `?g geo:sfWithin "POLYGON(...)"^^geo:wktLiteral` is
/// a natural thing to write and the obvious reading is the right one — but looking up
/// `geo:asWKT` on a literal finds nothing, so without this case the pattern silently matches
/// nothing, which is the failure this whole module exists to remove.
///
/// Anything else is a resource: its geometry is fetched and joined onto what is already
/// bound, which is what makes a topology property behave like a pattern rather than a
/// post-filter.
fn operand(
    term: &TermPattern,
    inner: GraphPattern,
    counter: &mut usize,
) -> (Expression, GraphPattern) {
    match term {
        TermPattern::Literal(literal) => (Expression::Literal(literal.clone()), inner),
        resource => {
            let variable = fresh(counter);
            let joined = GraphPattern::Join {
                left: Box::new(inner),
                right: Box::new(geometry_path(resource, &variable)),
            };
            (Expression::Variable(variable), joined)
        }
    }
}

/// `?resource (geo:hasDefaultGeometry|geo:hasGeometry)?/geo:asWKT ?target`.
fn geometry_path(resource: &TermPattern, target: &Variable) -> GraphPattern {
    let hop = PropertyPathExpression::ZeroOrOne(Box::new(PropertyPathExpression::Alternative(
        Box::new(PropertyPathExpression::NamedNode(iri(GEO, "hasDefaultGeometry"))),
        Box::new(PropertyPathExpression::NamedNode(iri(GEO, "hasGeometry"))),
    )));
    GraphPattern::Path {
        subject: resource.clone(),
        path: PropertyPathExpression::Sequence(
            Box::new(hop),
            Box::new(PropertyPathExpression::NamedNode(iri(GEO, "asWKT"))),
        ),
        object: TermPattern::Variable(target.clone()),
    }
}

/// `geof:<relation>(left, right)`, over whatever each operand resolved to.
fn call(relation: &str, left: Expression, right: Expression) -> Expression {
    Expression::FunctionCall(Function::Custom(iri(GEOF, relation)), vec![left, right])
}

fn fresh(counter: &mut usize) -> Variable {
    let v = Variable::new_unchecked(format!("{TEMP}{counter}"));
    *counter += 1;
    v
}

fn iri(namespace: &str, local: &str) -> oxrdf::NamedNode {
    oxrdf::NamedNode::new_unchecked(format!("{namespace}{local}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    fn parse(q: &str) -> Query {
        SparqlParser::new().parse_query(q).expect("parses")
    }

    const PREFIXES: &str = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
         PREFIX my: <http://example.org/ApplicationSchema#>";

    #[test]
    fn every_relation_maps_to_a_function_of_the_same_name() {
        // The two namespaces use identical local names, which is the whole reason a list
        // suffices. If that ever stops being true this is where it shows.
        for name in RELATIONS {
            assert_eq!(relation_of(&format!("{GEO}{name}")), Some(name));
        }
        assert_eq!(RELATIONS.len(), 24, "8 Simple Features + 8 Egenhofer + 8 RCC8");
    }

    #[test]
    fn a_predicate_outside_the_vocabulary_is_left_alone() {
        assert_eq!(relation_of("http://example.org/sfContains"), None);
        // `asWKT` is in the right namespace and is not a relation.
        assert_eq!(relation_of(&format!("{GEO}asWKT")), None);
    }

    #[test]
    fn a_query_without_topology_is_returned_unchanged() {
        let q = parse("SELECT ?s WHERE { ?s ?p ?o }");
        assert!(!mentions_topology(&q));
        assert_eq!(rewrite(&q).to_string(), q.to_string());
    }

    #[test]
    fn a_topology_property_becomes_geometry_lookups_and_a_filter() {
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ ?a geo:sfContains ?f }}"
        ));
        assert!(mentions_topology(&q));
        let text = rewrite(&q).to_string();

        // The property is gone as a pattern...
        assert!(
            !text.contains("geosparql#sfContains"),
            "the property survived the rewrite: {text}"
        );
        // ...and the function has taken its place.
        assert!(
            text.contains("geosparql/sfContains"),
            "no function call was produced: {text}"
        );
        assert!(text.contains("asWKT"), "no geometry lookup: {text}");
    }

    #[test]
    fn ordinary_patterns_in_the_same_bgp_are_kept() {
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ ?f my:hasExactGeometry ?g . ?a geo:sfWithin ?g }}"
        ));
        let text = rewrite(&q).to_string();
        assert!(
            text.contains("hasExactGeometry"),
            "the ordinary pattern was dropped: {text}"
        );
    }

    #[test]
    fn all_three_families_are_rewritten() {
        for relation in ["sfTouches", "ehMeet", "rcc8ec"] {
            let q = parse(&format!(
                "{PREFIXES} SELECT ?f WHERE {{ ?a geo:{relation} ?f }}"
            ));
            let text = rewrite(&q).to_string();
            assert!(
                text.contains(&format!("geosparql/{relation}")),
                "{relation} was not rewritten: {text}"
            );
        }
    }

    #[test]
    fn two_relations_in_one_query_get_distinct_variables() {
        // Sharing a temporary between two relations would silently constrain them to the
        // same geometry, which is a wrong answer rather than an error.
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ ?a geo:sfContains ?f . ?b geo:sfWithin ?f }}"
        ));
        let text = rewrite(&q).to_string();
        for n in 0..4 {
            assert!(
                text.contains(&format!("{TEMP}{n}")),
                "expected {TEMP}{n} among the temporaries: {text}"
            );
        }
    }

    #[test]
    fn a_literal_operand_is_used_directly() {
        // No geometry to fetch: the WKT is written in the query. Looking up `geo:asWKT` on
        // a literal would find nothing and match nothing.
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ ?f geo:sfWithin \"POLYGON((0 0,1 0,1 1,0 0))\"^^geo:wktLiteral }}"
        ));
        let text = rewrite(&q).to_string();
        assert!(
            text.contains("geosparql/sfWithin"),
            "not rewritten: {text}"
        );
        assert!(
            text.contains("POLYGON((0 0,1 0,1 1,0 0))"),
            "the literal was lost: {text}"
        );
        // One operand is a resource and one is a literal, so exactly one temporary.
        assert!(text.contains(&format!("{TEMP}0")), "{text}");
        assert!(!text.contains(&format!("{TEMP}1")), "a temporary was made for the literal: {text}");
    }

    #[test]
    fn a_relation_inside_service_is_left_for_the_remote_endpoint() {
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ SERVICE <http://example.org/sparql> {{ ?a geo:sfContains ?f }} }}"
        ));
        let text = rewrite(&q).to_string();
        assert!(
            text.contains("geosparql#sfContains"),
            "the pattern should cross the SERVICE boundary as written: {text}"
        );
    }

    #[test]
    fn the_rewrite_survives_a_round_trip_through_the_parser() {
        // The rewritten algebra is printed and re-parsed by nothing in production, but a
        // form that cannot be printed is a form that cannot be debugged.
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ ?a geo:sfOverlaps ?f }}"
        ));
        let text = rewrite(&q).to_string();
        SparqlParser::new()
            .parse_query(&text)
            .unwrap_or_else(|e| panic!("the rewritten query does not parse: {e}\n{text}"));
    }
}
