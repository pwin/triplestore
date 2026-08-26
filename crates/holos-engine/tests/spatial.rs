//! The spatial index, checked against the answer a full scan gives.
//!
//! An index is only worth having if it cannot change an answer, and every way this one could
//! go wrong produces a **missing row** — which nothing notices. So the central test here is
//! differential: for a range of probes, the geometries the index proposes must be a superset
//! of the geometries that actually satisfy the relation.
//!
//! Superset rather than equality is the correct assertion, and the reason is the whole design
//! of a spatial index: a bounding box says a geometry *may* qualify, never that it does. The
//! index narrows the candidates; the exact predicate still decides.

use geo::{Geometry, Rect};
use holos_engine::spatial::{can_filter, SpatialIndex};
use holos_engine::{geo_ext, Engine};
use holos_store::{GraphFilter, Store};
use oxrdfio::RdfFormat;

/// A grid of points, a few polygons, and a line — enough shapes for boxes to overlap in
/// interesting ways rather than all-or-nothing.
fn data() -> String {
    let mut turtle = String::from(
        "@prefix ex:  <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for x in 0..10 {
        for y in 0..10 {
            turtle.push_str(&format!(
                "ex:p{x}_{y} geo:asWKT \"POINT({x}.5 {y}.5)\"^^geo:wktLiteral .\n"
            ));
        }
    }
    turtle.push_str(
        "ex:big  geo:asWKT \"POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))\"^^geo:wktLiteral .\n\
         ex:mid  geo:asWKT \"POLYGON((2 2, 6 2, 6 6, 2 6, 2 2))\"^^geo:wktLiteral .\n\
         ex:far  geo:asWKT \"POLYGON((50 50, 60 50, 60 60, 50 60, 50 50))\"^^geo:wktLiteral .\n\
         ex:line geo:asWKT \"LINESTRING(0 0, 10 10)\"^^geo:wktLiteral .\n\
         ex:name ex:label \"not a geometry\" .\n",
    );
    turtle
}

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(data().as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

/// Every geometry in the store, by scanning — what the index must not disagree with.
fn all_geometries(store: &Store) -> Vec<(holos_core::TermId, Geometry)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for encoded in store.quads_for_pattern(None, None, None, GraphFilter::Any) {
        let object = encoded.expect("scan").object;
        if !seen.insert(object) {
            continue;
        }
        let Some(term) = store.decode_term(object).expect("decode") else {
            continue;
        };
        if let Some(geometry) = geo_ext::geometry_of(&term) {
            out.push((object, geometry));
        }
    }
    out
}

#[test]
fn the_index_holds_every_geometry_and_nothing_else() {
    let engine = engine();
    let store = engine.store();
    let index = SpatialIndex::build(store).expect("build");
    // 100 points + 3 polygons + 1 line. The plain literal is not a geometry and must not be
    // indexed — an index that swallows non-geometries would propose candidates that cannot
    // be parsed downstream.
    assert_eq!(index.len(), 104, "expected 104 geometries, got {}", index.len());
    assert_eq!(index.len(), all_geometries(store).len());
}

#[test]
fn candidates_are_a_superset_of_what_actually_overlaps() {
    // The differential property. For a spread of probe boxes, everything whose bounding box
    // genuinely overlaps the probe must be proposed. A missing candidate is a lost answer,
    // and it is invisible without a test like this one.
    let engine = engine();
    let store = engine.store();
    let index = SpatialIndex::build(store).expect("build");
    let geometries = all_geometries(store);

    let probes = [
        Rect::new((0.0, 0.0), (1.0, 1.0)),
        Rect::new((2.0, 2.0), (6.0, 6.0)),
        Rect::new((-5.0, -5.0), (15.0, 15.0)),
        Rect::new((9.0, 9.0), (9.2, 9.2)),
        Rect::new((55.0, 55.0), (56.0, 56.0)),
        Rect::new((100.0, 100.0), (101.0, 101.0)),
    ];

    for probe in probes {
        let proposed: std::collections::HashSet<_> =
            index.candidates_in(&probe).into_iter().collect();

        for (term, geometry) in &geometries {
            let Some(bounds) = geo::BoundingRect::bounding_rect(geometry) else {
                continue;
            };
            let overlaps = bounds.min().x <= probe.max().x
                && bounds.max().x >= probe.min().x
                && bounds.min().y <= probe.max().y
                && bounds.max().y >= probe.min().y;
            if overlaps {
                assert!(
                    proposed.contains(term),
                    "probe {probe:?} missed a geometry whose box overlaps it"
                );
            }
        }
    }
}

#[test]
fn a_probe_far_from_everything_proposes_nothing() {
    // The point of the index: not merely correct, but actually narrowing. If a distant probe
    // still returned everything, the tree would be doing nothing for its cost.
    let engine = engine();
    let store = engine.store();
    let index = SpatialIndex::build(store).expect("build");
    let empty = Rect::new((1000.0, 1000.0), (1001.0, 1001.0));
    assert!(index.candidates_in(&empty).is_empty());
}

#[test]
fn a_small_probe_narrows_substantially() {
    let engine = engine();
    let store = engine.store();
    let index = SpatialIndex::build(store).expect("build");
    // A one-unit box in a ten-by-ten grid: the point at its centre, plus the shapes that
    // span the whole grid. Nothing like all 104.
    let small = Rect::new((0.4, 0.4), (0.6, 0.6));
    let candidates = index.candidates_in(&small);
    assert!(
        candidates.len() < 10,
        "a small probe proposed {} of {} geometries, which is not narrowing",
        candidates.len(),
        index.len()
    );
    assert!(!candidates.is_empty(), "it should still find the point inside it");
}

#[test]
fn an_empty_store_builds_an_empty_index() {
    let index = SpatialIndex::build(&Store::new()).expect("build");
    assert!(index.is_empty());
}

#[test]
fn disjointness_cannot_be_filtered_by_boxes() {
    // Stated as a property of the data rather than of the table: the answers to a disjoint
    // query lie mostly *outside* the probe, so bounding-box overlap is the wrong filter and
    // `can_filter` must say so.
    let engine = engine();
    let store = engine.store();
    let index = SpatialIndex::build(store).expect("build");
    let probe = Rect::new((0.0, 0.0), (1.0, 1.0));
    let candidates = index.candidates_in(&probe).len();
    assert!(
        candidates < index.len(),
        "the probe should exclude most geometries"
    );
    // ...and those excluded ones are exactly the correct answers to `sfDisjoint`, which is
    // why routing it through the index would be wrong.
    assert!(!can_filter("sfDisjoint"));
}
