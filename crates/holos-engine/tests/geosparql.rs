//! GeoSPARQL runs through the ordinary query path.
//!
//! The functions come from `spargeo`; what is tested here is that they are registered on
//! the evaluator HOLOS actually uses, that geometry literals survive the term encoding,
//! and that a spatial predicate composes with the rest of SPARQL — including the access
//! policy, since a geometry is data like any other and §14 does not get to make an
//! exception for it.

use holos_engine::Engine;
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use oxrdf::{NamedNode, Term};
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
fn distance_works_for_geometries_that_are_not_points() {
    // `spargeo`'s `geof:distance` reads both operands as points and returns unbound for
    // anything else. Against the OGC example dataset that was every Polygon and every
    // LineString: a perfectly ordinary `geof:distance(?point, ?polygon, uom:metre)` produced
    // no binding at all rather than a distance, which is the quietest kind of wrong.
    let mut engine = Engine::new();
    engine
        .bulk_load(
            br#"@prefix ex:  <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:point   geo:asWKT "POINT(0 0)"^^geo:wktLiteral .
ex:far     geo:asWKT "POINT(1 0)"^^geo:wktLiteral .
ex:square  geo:asWKT "POLYGON((1 -1, 2 -1, 2 1, 1 1, 1 -1))"^^geo:wktLiteral .
ex:through geo:asWKT "POLYGON((-1 -1, 1 -1, 1 1, -1 1, -1 -1))"^^geo:wktLiteral .
ex:line    geo:asWKT "LINESTRING(1 -5, 1 5)"^^geo:wktLiteral .
"#
            .as_ref(),
            RdfFormat::Turtle,
            None,
        )
        .expect("load");
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);

    let distance = |a: &str, b: &str| -> Option<f64> {
        let sparql = format!(
            "PREFIX ex: <http://example.com/> \
             PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/> \
             PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/> \
             SELECT ?d WHERE {{ ex:{a} geo:asWKT ?x . ex:{b} geo:asWKT ?y . \
             BIND(geof:distance(?x, ?y, uom:metre) AS ?d) }}"
        );
        let (results, _) = Engine::query(&view, &sparql, None)
            .map(|r| (r, ()))
            .expect("query");
        match results {
            QueryResults::Solutions(iter) => iter
                .map(|s| s.expect("solution").get("d").cloned())
                .next()
                .flatten()
                .and_then(|t| match t {
                    Term::Literal(l) => l.value().parse::<f64>().ok(),
                    _ => None,
                }),
            _ => panic!("expected solutions"),
        }
    };

    // Point to polygon: the shortest distance is to the polygon's near edge at x = 1, which
    // is the same great-circle distance as the point (1 0) itself.
    let to_square = distance("point", "square").expect("a distance to a polygon");
    let to_far = distance("point", "far").expect("a distance to a point");
    assert!(
        (to_square - to_far).abs() < 1.0,
        "the nearest point of the square lies on x = 1, so this should match the distance to \
         POINT(1 0): {to_square} against {to_far}"
    );

    // Point to line: same edge, same answer.
    let to_line = distance("point", "line").expect("a distance to a linestring");
    assert!(
        (to_line - to_far).abs() < 1.0,
        "the line crosses x = 1: {to_line} against {to_far}"
    );

    // A point inside a polygon is zero away from it, not unbound.
    assert_eq!(
        distance("point", "through"),
        Some(0.0),
        "a point inside a polygon is not at an undefined distance from it"
    );

    // Polygon to polygon, neither of them a point anywhere in the call.
    assert!(
        distance("through", "square").is_some(),
        "two polygons must have a distance"
    );

    // And the case that already worked must return exactly what it returned before, which is
    // why the replacement reuses the same Haversine rather than its own arithmetic.
    assert!(
        (to_far - 111_195.079_734_0).abs() < 0.01,
        "point-to-point changed: {to_far}"
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

// ---------------------------------------------------------------------------------
// coordinate snapping after a set operation
// ---------------------------------------------------------------------------------
//
// `geo`'s boolean operations go through `i_overlay`, which works on an integer grid and
// converts back on the way out. Coordinates that are not exactly representable come back
// shifted by about 1e-10: `-83.2` becomes `-83.20000000009313`.
//
// That is 0.01 mm and harmless for measuring. It is not harmless for the exact topological
// predicates, which turn on whether two boundaries coincide — so `sfTouches` silently
// stopped composing with any computed geometry. `geo_ext` wraps the four set operations to
// snap their output back onto the coordinates that went in.

const WKT: &str = r#"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"#;

/// The single value of a one-row, one-column query.
fn value(expression: &str) -> String {
    let rows = rows(&format!(
        "{PREFIXES} SELECT ({expression} AS ?r) WHERE {{}}"
    ));
    assert_eq!(rows.len(), 1, "expected exactly one row from {expression}");
    rows[0]
        .split_once('=')
        .map(|(_, v)| v.to_owned())
        .unwrap_or_default()
}

#[test]
fn a_set_operation_returns_its_inputs_coordinates_exactly() {
    // -83.2 and 34.1 are in the inputs, so they must be in the output unchanged. Before the
    // snapping wrapper this produced -83.20000000009313.
    let result = value(&format!(
        r#"geof:union("Polygon((-83.6 34.1, -83.2 34.1, -83.2 34.5, -83.6 34.5, -83.6 34.1))"{WKT},
                      "Polygon((-83.3 34.0, -83.1 34.0, -83.1 34.2, -83.3 34.2, -83.3 34.0))"{WKT})"#
    ));
    assert!(
        result.contains("-83.6 34.1"),
        "an input coordinate came back perturbed: {result}"
    );
    assert!(
        !result.contains("34.10000000"),
        "the i_overlay perturbation is still present: {result}"
    );
}

#[test]
fn touching_survives_a_union() {
    // The failure this fixes, stated as the invariant it broke: C shares an edge with A at
    // longitude -83.2, so it touches A, and it must still touch a union that A is part of.
    let a = r#""Polygon((-83.6 34.1, -83.2 34.1, -83.2 34.5, -83.6 34.5, -83.6 34.1))""#;
    let d = r#""Polygon((-83.3 34.0, -83.1 34.0, -83.1 34.2, -83.3 34.2, -83.3 34.0))""#;
    let c = r#""Polygon((-83.2 34.3, -83.0 34.3, -83.0 34.5, -83.2 34.5, -83.2 34.3))""#;

    assert_eq!(
        value(&format!("geof:sfTouches({c}{WKT}, {a}{WKT})")),
        value(&format!(
            "geof:sfTouches({c}{WKT}, geof:union({a}{WKT}, {d}{WKT}))"
        )),
        "touching A but not touching a union containing A is a contradiction"
    );
}

#[test]
fn a_computed_vertex_is_not_snapped() {
    // The reason this is snapping rather than rounding. Rounding every coordinate to the
    // inputs' decimal places would move genuinely new vertices; here the triangle
    // 2x + 3y <= 6 clipped to the 0..2 square produces a vertex at y = 2/3, which matches no
    // input and must survive as computed.
    let result = value(&format!(
        r#"geof:intersection("Polygon((0 0, 3 0, 0 2, 0 0))"{WKT},
                             "Polygon((0 0, 2 0, 2 2, 0 2, 0 0))"{WKT})"#
    ));
    assert!(
        result.contains("2 0.66"),
        "the computed vertex at y = 2/3 was lost or snapped away: {result}"
    );
}

#[test]
fn every_set_operation_is_wrapped() {
    // A wrapper on three of the four would be worse than none: the exception would be found
    // by whoever eventually used it, in production.
    for operation in ["union", "intersection", "difference", "symDifference"] {
        let result = value(&format!(
            r#"geof:{operation}("Polygon((0.1 0.1, 0.4 0.1, 0.4 0.4, 0.1 0.4, 0.1 0.1))"{WKT},
                                "Polygon((0.3 0.3, 0.6 0.3, 0.6 0.6, 0.3 0.6, 0.3 0.3))"{WKT})"#
        ));
        assert!(
            result.contains("0.1 0.1") || result.contains("0.3 0.3") || result.contains("0.4 0.4"),
            "{operation} did not return an input coordinate exactly: {result}"
        );
        assert!(
            !result.contains("0.10000000") && !result.contains("0.30000000"),
            "{operation} still perturbs its inputs: {result}"
        );
    }
}
