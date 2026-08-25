//! GeoSPARQL runs through the ordinary query path.
//!
//! The functions come from `spargeo`; what is tested here is that they are registered on
//! the evaluator HOLOS actually uses, that geometry literals survive the term encoding,
//! and that a spatial predicate composes with the rest of SPARQL — including the access
//! policy, since a geometry is data like any other and §14 does not get to make an
//! exception for it.

use holos_engine::Engine;
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use oxrdf::NamedNode;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const GEO: &str = r#"
@prefix ex:   <http://example.com/> .
@prefix geo:  <http://www.opengis.net/ont/geosparql#> .

ex:london  a ex:City ; ex:name "London"  ;
    geo:asWKT "POINT(-0.1276 51.5072)"^^geo:wktLiteral .
ex:paris   a ex:City ; ex:name "Paris"   ;
    geo:asWKT "POINT(2.3522 48.8566)"^^geo:wktLiteral .
ex:sydney  a ex:City ; ex:name "Sydney"  ;
    geo:asWKT "POINT(151.2093 -33.8688)"^^geo:wktLiteral .

ex:westernEurope a ex:Region ;
    geo:asWKT "POLYGON((-10 40, 20 40, 20 60, -10 60, -10 40))"^^geo:wktLiteral .
"#;

const PREFIXES: &str = r"
PREFIX ex:   <http://example.com/>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
";

fn engine() -> Engine {
    let mut e = Engine::new();
    e.bulk_load(GEO.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    e
}

fn rows(query: &str) -> Vec<String> {
    let e = engine();
    let session = Session::unrestricted(e.store()).expect("session");
    let view = e.view(&session);
    // Bound before the match: a match scrutinee temporary outlives the arm, and `view`
    // would be dropped first.
    let results = Engine::query(&view, query, None).expect("query");
    match results {
        QueryResults::Solutions(iter) => {
            let mut out: Vec<String> = iter
                .map(|s| {
                    s.expect("solution")
                        .iter()
                        .map(|(v, t)| format!("{}={t}", v.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            out.sort();
            out
        }
        _ => panic!("expected a solutions result"),
    }
}

#[test]
fn a_topological_relation_filters_by_geometry() {
    // The canonical GeoSPARQL query: which points fall inside a polygon.
    let out = rows(&format!(
        "{PREFIXES}
         SELECT ?name WHERE {{
           ?city a ex:City ; ex:name ?name ; geo:asWKT ?point .
           ex:westernEurope geo:asWKT ?region .
           FILTER(geof:sfWithin(?point, ?region))
         }}"
    ));
    assert_eq!(
        out,
        vec![r#"name="London""#, r#"name="Paris""#],
        "Sydney is not in western Europe: {out:?}"
    );
}

#[test]
fn distance_is_computed_and_orderable() {
    let out = rows(&format!(
        "{PREFIXES}
         PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/>
         SELECT ?name WHERE {{
           ?city a ex:City ; ex:name ?name ; geo:asWKT ?point .
           ex:london geo:asWKT ?london .
           BIND(geof:distance(?point, ?london, uom:metre) AS ?d)
         }}
         ORDER BY ?d"
    ));
    assert_eq!(out.len(), 3);
    assert!(
        out.iter().any(|r| r.contains("London")),
        "every city should still be returned: {out:?}"
    );
}

#[test]
fn geometry_constructors_work() {
    // A non-topological function that builds a new geometry rather than testing one.
    let out = rows(&format!(
        "{PREFIXES}
         SELECT ?env WHERE {{
           ex:westernEurope geo:asWKT ?region .
           BIND(geof:envelope(?region) AS ?env)
         }}"
    ));
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("POLYGON"), "{out:?}");
}

#[test]
fn a_geometry_literal_survives_the_term_encoding() {
    // WKT literals are typed literals like any other, so they take the dictionary path
    // rather than an inline tag. Round-tripping matters because a mangled lexical form
    // would silently change what a spatial predicate computes.
    let out = rows(&format!(
        "{PREFIXES} SELECT ?wkt WHERE {{ ex:london geo:asWKT ?wkt }}"
    ));
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("POINT(-0.1276 51.5072)"), "{out:?}");
}

#[test]
fn access_policy_still_applies_to_geometry() {
    // A geometry is data. Hiding geo:asWKT must hide it from a spatial filter too —
    // otherwise the §14 property ("the answer over the visible sub-dataset") would have a
    // geospatial hole in it.
    let e = engine();
    let policy = Policy::permit_all().with_rule(Rule::deny(
        Modes::READ,
        Scope::Predicate(NamedNode::new_unchecked(
            "http://www.opengis.net/ont/geosparql#asWKT",
        )),
        PrincipalMatch::Everyone,
    ));
    let session = Session::open(e.store(), Principal::anonymous(), policy).expect("session");
    let view = e.view(&session);
    let results = Engine::query(
        &view,
        &format!(
            "{PREFIXES}
             SELECT ?name WHERE {{
               ?city a ex:City ; ex:name ?name ; geo:asWKT ?point .
               ex:westernEurope geo:asWKT ?region .
               FILTER(geof:sfWithin(?point, ?region))
             }}"
        ),
        None,
    )
    .expect("query");
    match results {
        QueryResults::Solutions(iter) => assert_eq!(
            iter.count(),
            0,
            "the geometry predicate is denied, so the spatial join must find nothing"
        ),
        _ => panic!("expected a solutions result"),
    }
}
