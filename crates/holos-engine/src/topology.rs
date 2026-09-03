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
//! # Routing through the spatial index
//!
//! When one operand is a **constant** geometry and a [`SpatialIndex`] is supplied, the
//! rewrite also emits a `VALUES` clause restricting the other side to the geometries whose
//! bounding boxes overlap the constant. *Find everything inside this polygon* stops being a
//! scan of every geometry in the store.
//!
//! Three conditions, all of which must hold, and each of which is a correctness boundary
//! rather than a tuning knob:
//!
//! 1. **The relation must be box-filterable.** [`SpatialIndex::can_filter`] excludes
//!    disjointness, whose answers lie mostly *outside* any probe.
//! 2. **The index must be current for the store.** A stale index is missing whatever was
//!    written after it was built, and `VALUES` would then omit rows — silently.
//!    [`SpatialIndex::is_current_for`] gates this, and failing it costs a full scan.
//! 3. **One operand must be a constant.** There is nothing to probe with otherwise.
//!
//! ## Why this cannot change an answer
//!
//! `VALUES` **restricts and never adds**. The candidate set is a superset of the geometries
//! that can satisfy the relation, so restricting to it removes only rows that would have
//! failed the filter anyway. The exact `geof:` predicate still runs, the geometry lookup
//! still goes through the policy-filtered view, and a principal still cannot bind a quad it
//! may not read. Every way the index could be wrong — stale, over-broad, empty — either
//! costs time or is caught by the two gates above.
//!
//! ## What it does not help
//!
//! A relation between two unbound variables. There is no constant to probe with, so it
//! remains a cross product; §17's note about needing a planner still applies to that shape.

use crate::spatial::{can_filter, SpatialIndex};
use holos_store::Store;
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern, Variable};
use spargebra::Query;

/// The spatial index and the store to decode candidates against.
///
/// Both are needed together: the index answers in term ids, and `VALUES` needs the literals
/// they stand for.
#[derive(Clone, Copy)]
pub struct Routing<'a> {
    /// The index to probe.
    pub index: &'a SpatialIndex,
    /// The store the index describes, used to decode candidates and to check staleness.
    pub store: &'a Store,
}

impl Routing<'_> {
    /// Whether this routing may be used at all.
    ///
    /// A stale index omits rows rather than slowing things down, so this is checked before
    /// any probe rather than trusted.
    fn usable(&self) -> bool {
        self.index.is_current_for(self.store)
    }

    /// The `VALUES` clause restricting `variable` to what could satisfy `relation` with
    /// `constant`, or `None` if the index cannot help.
    fn restrict(
        &self,
        relation: &str,
        constant: &oxrdf::Literal,
        variable: &Variable,
    ) -> Option<GraphPattern> {
        if !can_filter(relation) || !self.usable() {
            return None;
        }
        let geometry = crate::geo_ext::geometry_of(&constant.clone().into())?;
        let candidates = self.index.candidates(&geometry);

        // A restriction that restricts almost nothing is not worth emitting.
        //
        // `spareval` joins by building the left side and scanning the right in full, so a
        // `VALUES` does not stop the scan happening — it only adds a relation to join
        // against. When it narrows to a handful that is a fair trade; when it names most of
        // the store it is pure overhead, and a query with a continent-sized window would be
        // made *slower* by the index meant to help it.
        //
        // The threshold is deliberately aggressive. Until the evaluator can bind-join, the
        // only case that genuinely pays is the one where the candidate set is small or
        // empty, and an empty one short-circuits the whole pattern.
        if candidates.len() * 4 > self.index.len() {
            return None;
        }

        let mut bindings = Vec::with_capacity(candidates.len());
        for term in candidates {
            // A candidate that will not decode is dropped rather than failing the query: the
            // result is a smaller VALUES, which can only lose a row that could not have been
            // returned anyway.
            if let Ok(Some(oxrdf::Term::Literal(literal))) = self.store.decode_term(term) {
                bindings.push(vec![Some(GroundTerm::Literal(literal))]);
            }
        }
        Some(GraphPattern::Values {
            variables: vec![variable.clone()],
            bindings,
        })
    }
}

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
pub fn rewrite(query: &Query, routing: Option<Routing<'_>>) -> Query {
    if !mentions_topology(query) {
        return query.clone();
    }
    let mut counter = 0usize;
    let context = Context {
        counter: &mut counter,
        routing,
    };
    let mut context = context;
    match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut context),
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
            pattern: rewrite_pattern(pattern, &mut context),
            base_iri: base_iri.clone(),
        },
        Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => Query::Describe {
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut context),
            base_iri: base_iri.clone(),
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset: dataset.clone(),
            pattern: rewrite_pattern(pattern, &mut context),
            base_iri: base_iri.clone(),
        },
    }
}

/// What the rewrite carries down the tree.
struct Context<'a, 'r> {
    counter: &'a mut usize,
    routing: Option<Routing<'r>>,
}

fn rewrite_pattern(pattern: &GraphPattern, context: &mut Context<'_, '_>) -> GraphPattern {
    match pattern {
        GraphPattern::Bgp { patterns } => rewrite_bgp(patterns, context),
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: Box::new(rewrite_pattern(left, context)),
            right: Box::new(rewrite_pattern(right, context)),
        },
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => GraphPattern::LeftJoin {
            left: Box::new(rewrite_pattern(left, context)),
            right: Box::new(rewrite_pattern(right, context)),
            expression: expression.clone(),
        },
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr: expr.clone(),
            inner: Box::new(rewrite_pattern(inner, context)),
        },
        GraphPattern::Union { left, right } => GraphPattern::Union {
            left: Box::new(rewrite_pattern(left, context)),
            right: Box::new(rewrite_pattern(right, context)),
        },
        GraphPattern::Graph { name, inner } => GraphPattern::Graph {
            name: name.clone(),
            inner: Box::new(rewrite_pattern(inner, context)),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: Box::new(rewrite_pattern(inner, context)),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: Box::new(rewrite_pattern(left, context)),
            right: Box::new(rewrite_pattern(right, context)),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: Box::new(rewrite_pattern(inner, context)),
            expression: expression.clone(),
        },
        GraphPattern::Project { inner, variables } => GraphPattern::Project {
            inner: Box::new(rewrite_pattern(inner, context)),
            variables: variables.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct {
            inner: Box::new(rewrite_pattern(inner, context)),
        },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced {
            inner: Box::new(rewrite_pattern(inner, context)),
        },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: Box::new(rewrite_pattern(inner, context)),
            start: *start,
            length: *length,
        },
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: Box::new(rewrite_pattern(inner, context)),
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
fn rewrite_bgp(patterns: &[TriplePattern], context: &mut Context<'_, '_>) -> GraphPattern {
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
        // Variables are allocated before anything is joined, so a restriction can be built
        // for one of them and joined in *ahead* of the geometry lookup it constrains.
        //
        // Order matters more than it looks. A `VALUES` joined *after* the lookup filters
        // rows the scan has already produced, which saves nothing — measured at 50,000
        // geometries it was a 1x "speed-up". Joined *before*, it binds the variable first
        // and the lookup starts from four candidates instead of fifty thousand.
        let subject_variable = match &triple.subject {
            TermPattern::Literal(_) => None,
            _ => Some(fresh(context.counter)),
        };
        let object_variable = match &triple.object {
            TermPattern::Literal(_) => None,
            _ => Some(fresh(context.counter)),
        };

        // The restriction, if one side is constant and the index can help.
        if let (TermPattern::Literal(constant), Some(variable)) =
            (&triple.subject, &object_variable)
        {
            result = restrict(context.routing, relation, constant, variable, result);
        }
        if let (Some(variable), TermPattern::Literal(constant)) =
            (&subject_variable, &triple.object)
        {
            result = restrict(context.routing, relation, constant, variable, result);
        }

        // Then the geometry lookups, which now join onto a bound variable where the index
        // applied and onto the whole store where it did not.
        let (left, joined) = side(&triple.subject, subject_variable, result, context.counter);
        let (right, joined) = side(&triple.object, object_variable, joined, context.counter);

        result = GraphPattern::Filter {
            expr: call(relation, left, right),
            inner: Box::new(joined),
        };
    }
    result
}

/// Joins the index's restriction onto `inner`, or returns it unchanged.
fn restrict(
    routing: Option<Routing<'_>>,
    relation: &str,
    constant: &oxrdf::Literal,
    variable: &Variable,
    inner: GraphPattern,
) -> GraphPattern {
    let Some(values) = routing.and_then(|r| r.restrict(relation, constant, variable)) else {
        return inner;
    };
    GraphPattern::Join {
        left: Box::new(inner),
        right: Box::new(values),
    }
}

/// One side of a topology property: an expression, and the pattern it needs joined in.
///
/// A literal *is* the geometry and needs nothing joined. Anything else is a resource whose
/// geometry has to be fetched.
fn side(
    term: &TermPattern,
    variable: Option<Variable>,
    inner: GraphPattern,
    counter: &mut usize,
) -> (Expression, GraphPattern) {
    match (term, variable) {
        (TermPattern::Literal(literal), _) => (Expression::Literal(literal.clone()), inner),
        (resource, Some(variable)) => {
            let joined = GraphPattern::Join {
                left: Box::new(inner),
                right: Box::new(geometry_path(resource, &variable, counter)),
            };
            (Expression::Variable(variable), joined)
        }
        // A non-literal always gets a variable above; this arm cannot be reached.
        (resource, None) => {
            let variable = Variable::new_unchecked(format!("{TEMP}unreachable"));
            let joined = GraphPattern::Join {
                left: Box::new(inner),
                right: Box::new(geometry_path(resource, &variable, counter)),
            };
            (Expression::Variable(variable), joined)
        }
    }
}

/// The three ways a resource can reach a geometry literal, as a `UNION` of plain patterns.
///
/// Semantically this is `?resource (geo:hasDefaultGeometry|geo:hasGeometry)?/geo:asWKT
/// ?target`, and that is how it was first written. A property path is the obvious spelling
/// and the wrong one here:
///
/// **`spareval` evaluates a property path by traversal, not by index lookup.** With the
/// target variable already bound — which is exactly what the spatial index arranges — the
/// path ignores the binding and walks the store anyway. Measured at 50,000 geometries, the
/// index narrowed fifty thousand candidates to four and the query still took 480 ms, because
/// the path never used them.
///
/// Written out as a union of ordinary triple patterns, each branch is a BGP the evaluator can
/// probe by object, so a bound `?target` becomes a lookup. The three branches are the three
/// alternatives the path expression stood for — the zero-length hop, and each of the two
/// properties — so the answer is unchanged, including the duplicate a resource carrying both
/// spellings produces.
fn geometry_path(resource: &TermPattern, target: &Variable, counter: &mut usize) -> GraphPattern {
    let direct = GraphPattern::Bgp {
        patterns: vec![TriplePattern {
            subject: resource.clone(),
            predicate: NamedNodePattern::NamedNode(iri(GEO, "asWKT")),
            object: TermPattern::Variable(target.clone()),
        }],
    };

    let through = |property: &str, counter: &mut usize| {
        let intermediate = fresh(counter);
        GraphPattern::Bgp {
            patterns: vec![
                TriplePattern {
                    subject: resource.clone(),
                    predicate: NamedNodePattern::NamedNode(iri(GEO, property)),
                    object: TermPattern::Variable(intermediate.clone()),
                },
                TriplePattern {
                    subject: TermPattern::Variable(intermediate),
                    predicate: NamedNodePattern::NamedNode(iri(GEO, "asWKT")),
                    object: TermPattern::Variable(target.clone()),
                },
            ],
        }
    };

    GraphPattern::Union {
        left: Box::new(direct),
        right: Box::new(GraphPattern::Union {
            left: Box::new(through("hasDefaultGeometry", counter)),
            right: Box::new(through("hasGeometry", counter)),
        }),
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
        assert_eq!(
            RELATIONS.len(),
            24,
            "8 Simple Features + 8 Egenhofer + 8 RCC8"
        );
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
        assert_eq!(rewrite(&q, None).to_string(), q.to_string());
    }

    #[test]
    fn a_topology_property_becomes_geometry_lookups_and_a_filter() {
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ ?a geo:sfContains ?f }}"
        ));
        assert!(mentions_topology(&q));
        let text = rewrite(&q, None).to_string();

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
        let text = rewrite(&q, None).to_string();
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
            let text = rewrite(&q, None).to_string();
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
        let text = rewrite(&q, None).to_string();
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
        let text = rewrite(&q, None).to_string();
        assert!(text.contains("geosparql/sfWithin"), "not rewritten: {text}");
        assert!(
            text.contains("POLYGON((0 0,1 0,1 1,0 0))"),
            "the literal was lost: {text}"
        );
        // The literal goes straight into the function call rather than through a variable.
        // (The resource side still allocates temporaries — one for the WKT target and one
        // per `hasGeometry` branch — which is what `geometry_path` is for.)
        assert!(
            text.contains(&format!("{TEMP}0")),
            "no WKT target variable: {text}"
        );
        assert!(
            text.contains("sfWithin>(?_topo_wkt_0, \"POLYGON"),
            "the literal was routed through a variable instead of used directly: {text}"
        );
    }

    #[test]
    fn a_relation_inside_service_is_left_for_the_remote_endpoint() {
        let q = parse(&format!(
            "{PREFIXES} SELECT ?f WHERE {{ SERVICE <http://example.org/sparql> {{ ?a geo:sfContains ?f }} }}"
        ));
        let text = rewrite(&q, None).to_string();
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
        let text = rewrite(&q, None).to_string();
        SparqlParser::new()
            .parse_query(&text)
            .unwrap_or_else(|e| panic!("the rewritten query does not parse: {e}\n{text}"));
    }
}
