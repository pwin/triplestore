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
    assert_eq!(
        index.len(),
        104,
        "expected 104 geometries, got {}",
        index.len()
    );
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
    assert!(
        !candidates.is_empty(),
        "it should still find the point inside it"
    );
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

// ---------------------------------------------------------------------------------
// routing: the index must narrow the scan without changing the answer
// ---------------------------------------------------------------------------------

use holos_engine::QueryOptions;
use holos_security::Session;
use spareval::QueryResults;
use std::sync::Arc;

/// Runs a query with and without the spatial index, and returns both answers.
///
/// Every assertion below compares the two. An index can only go wrong by *omitting* rows,
/// and an omission is invisible unless something holds the two answers side by side.
fn both_ways(engine: &Engine, query: &str) -> (Vec<String>, Vec<String>) {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let index = Arc::new(SpatialIndex::build(engine.store()).expect("build"));

    let plain = run(&view, query, &QueryOptions::new());
    let routed = run(&view, query, &QueryOptions::new().with_spatial(index));
    (plain, routed)
}

fn run(
    view: &holos_engine::view::DatasetView<'_>,
    query: &str,
    options: &QueryOptions,
) -> Vec<String> {
    let (results, _) = Engine::query_with(view, query, options).expect("query");
    let mut rows: Vec<String> = match results {
        QueryResults::Solutions(iter) => iter
            .map(|s| {
                let s = s.expect("solution");
                s.iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect(),
        _ => panic!("expected solutions"),
    };
    rows.sort();
    rows
}

const PREFIXES: &str = "PREFIX ex:  <http://example.com/>\n\
                        PREFIX geo: <http://www.opengis.net/ont/geosparql#>\n";

#[test]
fn routing_does_not_change_the_answer() {
    let engine = engine();
    // A window over part of the grid. The index should narrow it substantially, and must
    // return exactly what the unindexed query returns.
    let query = format!(
        "{PREFIXES} SELECT ?s WHERE {{ ?s geo:sfWithin \
         \"POLYGON((0 0, 3 0, 3 3, 0 3, 0 0))\"^^geo:wktLiteral }}"
    );
    let (plain, routed) = both_ways(&engine, &query);
    assert_eq!(plain, routed, "the spatial index changed the answer");
    assert!(!plain.is_empty(), "the query should match some points");
}

#[test]
fn routing_agrees_across_relations_and_windows() {
    // Several relations and several windows, because a routing bug is easy to have in one
    // relation and not another — `can_filter` is a table, and tables get edited.
    let engine = engine();
    for relation in [
        "sfWithin",
        "sfIntersects",
        "sfContains",
        "sfDisjoint",
        "sfTouches",
    ] {
        for window in [
            "POLYGON((0 0, 3 0, 3 3, 0 3, 0 0))",
            "POLYGON((-10 -10, 20 -10, 20 20, -10 20, -10 -10))",
            "POLYGON((100 100, 101 100, 101 101, 100 101, 100 100))",
        ] {
            let query = format!(
                "{PREFIXES} SELECT ?s WHERE {{ ?s geo:{relation} \"{window}\"^^geo:wktLiteral }}"
            );
            let (plain, routed) = both_ways(&engine, &query);
            assert_eq!(
                plain, routed,
                "{relation} over {window} disagreed between indexed and unindexed"
            );
        }
    }
}

#[test]
fn disjointness_is_answered_correctly_despite_not_being_routed() {
    // The relation the index must not narrow. It still has to give the right answer, and
    // that answer is large — nearly everything is disjoint from a small window.
    let engine = engine();
    let query = format!(
        "{PREFIXES} SELECT ?s WHERE {{ ?s geo:sfDisjoint \
         \"POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))\"^^geo:wktLiteral }}"
    );
    let (plain, routed) = both_ways(&engine, &query);
    assert_eq!(plain, routed);
    assert!(
        plain.len() > 50,
        "most geometries are disjoint from a unit square; got {}",
        plain.len()
    );
}

#[test]
fn a_stale_index_is_refused_rather_than_trusted() {
    // The safety property. An index built before a write is missing what the write added;
    // using it would drop rows silently. It must be detected and ignored.
    let mut engine = engine();
    let index = Arc::new(SpatialIndex::build(engine.store()).expect("build"));
    assert!(index.is_current_for(engine.store()));

    engine
        .bulk_load(
            "@prefix ex: <http://example.com/> .\n\
             @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
             ex:late geo:asWKT \"POINT(0.25 0.25)\"^^geo:wktLiteral .\n"
                .as_bytes(),
            RdfFormat::Turtle,
            None,
        )
        .expect("load");

    assert!(
        !index.is_current_for(engine.store()),
        "the index must notice the store moved under it"
    );

    // And the query must still find the new point, because the stale index is not used.
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!(
        "{PREFIXES} SELECT ?s WHERE {{ ?s geo:sfWithin \
         \"POLYGON((0 0, 0.5 0, 0.5 0.5, 0 0.5, 0 0))\"^^geo:wktLiteral }}"
    );
    let rows = run(&view, &query, &QueryOptions::new().with_spatial(index));
    assert!(
        rows.iter().any(|r| r.contains("late")),
        "a stale index dropped a row that was written after it was built: {rows:?}"
    );
}

#[test]
fn routing_actually_narrows_rather_than_quietly_doing_nothing() {
    // Without this, every differential test above would pass on a routing implementation
    // that never fires — they compare two answers, and two identical no-ops are equal.
    //
    // So this asserts the rewrite *emits* the restriction: the algebra must contain a VALUES
    // clause, and it must be smaller than the index.
    use holos_engine::topology::{rewrite, Routing};
    use spargebra::SparqlParser;

    let engine = engine();
    let index = SpatialIndex::build(engine.store()).expect("build");
    let query = SparqlParser::new()
        .parse_query(&format!(
            "{PREFIXES} SELECT ?s WHERE {{ ?s geo:sfWithin \
             \"POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))\"^^geo:wktLiteral }}"
        ))
        .expect("parse");

    let unrouted = rewrite(&query, None).to_string();
    assert!(
        !unrouted.contains("VALUES"),
        "no index was supplied, so nothing should have been narrowed: {unrouted}"
    );

    let routed = rewrite(
        &query,
        Some(Routing {
            index: &index,
            store: engine.store(),
        }),
    )
    .to_string();
    assert!(
        routed.contains("VALUES"),
        "the index was supplied and applicable, but no restriction was emitted: {routed}"
    );

    // And the restriction has to be a restriction: fewer geometries than the whole index.
    let listed = routed.matches("wktLiteral").count();
    assert!(
        listed < index.len(),
        "the VALUES listed {listed} of {} geometries, which narrows nothing",
        index.len()
    );
}

#[test]
fn disjointness_emits_no_restriction() {
    // The correctness boundary, asserted on the algebra rather than only on `can_filter`:
    // even with an index in hand, a disjoint relation must be left as a full scan.
    use holos_engine::topology::{rewrite, Routing};
    use spargebra::SparqlParser;

    let engine = engine();
    let index = SpatialIndex::build(engine.store()).expect("build");
    let query = SparqlParser::new()
        .parse_query(&format!(
            "{PREFIXES} SELECT ?s WHERE {{ ?s geo:sfDisjoint \
             \"POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))\"^^geo:wktLiteral }}"
        ))
        .expect("parse");

    let routed = rewrite(
        &query,
        Some(Routing {
            index: &index,
            store: engine.store(),
        }),
    )
    .to_string();
    assert!(
        !routed.contains("VALUES"),
        "disjointness was narrowed by bounding boxes, which loses correct answers: {routed}"
    );
}

// ------------------------------------------------------------------- incremental

/// A store holding `n` points on a diagonal, plus some non-geometry noise.
fn points(n: usize, offset: usize) -> String {
    let mut turtle = String::from(
        "@prefix ex:  <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in offset..offset + n {
        turtle.push_str(&format!(
            "ex:p{i} geo:asWKT \"POINT({i} {i})\"^^geo:wktLiteral .\n"
        ));
        turtle.push_str(&format!("ex:p{i} ex:label \"not a geometry {i}\" .\n"));
    }
    turtle
}

/// Both indexes, asked the same question, must answer identically.
fn agree(a: &SpatialIndex, b: &SpatialIndex, rect: &geo::Rect) -> bool {
    let mut left = a.candidates_in(rect);
    let mut right = b.candidates_in(rect);
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

#[test]
fn a_refresh_produces_what_a_rebuild_would() {
    // The property the whole change rests on. A refresh skips the decode and the parse for
    // everything it has seen, and inserts one at a time instead of bulk loading — so the
    // tree it ends up with is *shaped* differently from a rebuilt one. What must not differ
    // is any answer it gives.
    let mut engine = Engine::new();
    engine
        .bulk_load(points(200, 0).as_bytes(), RdfFormat::Turtle, None)
        .expect("load");

    let index = SpatialIndex::build(engine.store()).expect("build");
    assert_eq!(index.len(), 200);

    // Three rounds of writing and refreshing, rather than one, because the interesting
    // failure is state accumulating wrongly across refreshes.
    for round in 1..=3 {
        engine
            .bulk_load(
                points(100, round * 1000).as_bytes(),
                RdfFormat::Turtle,
                None,
            )
            .expect("load more");
        index.refresh(engine.store()).expect("refresh");

        let rebuilt = SpatialIndex::build(engine.store()).expect("rebuild");
        assert_eq!(
            index.len(),
            rebuilt.len(),
            "round {round}: refreshed index holds {} geometries, a rebuild holds {}",
            index.len(),
            rebuilt.len()
        );
        assert!(
            index.is_current_for(engine.store()),
            "round {round}: a refreshed index must not still look stale, or nothing will use it"
        );
        for (lo, hi) in [(-10.0, 10.0), (0.0, 250.0), (999.0, 1105.0), (-1e6, 1e6)] {
            let rect = geo::Rect::new((lo, lo), (hi, hi));
            assert!(
                agree(&index, &rebuilt, &rect),
                "round {round}: refreshed and rebuilt indexes disagree over {lo}..{hi}"
            );
        }
    }
}

#[test]
fn a_refresh_that_changes_nothing_is_a_no_op() {
    let mut engine = Engine::new();
    engine
        .bulk_load(points(50, 0).as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    let index = SpatialIndex::build(engine.store()).expect("build");
    let before = index.len();
    index.refresh(engine.store()).expect("refresh");
    index.refresh(engine.store()).expect("refresh again");
    assert_eq!(index.len(), before, "a no-op refresh changed the index");
}

#[test]
fn a_geometry_added_after_the_build_is_found() {
    // The failure this exists to prevent: an index that silently omits a new geometry
    // returns a `VALUES` without it, and the row it would have produced is simply gone.
    let mut engine = Engine::new();
    engine
        .bulk_load(points(10, 0).as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    let index = SpatialIndex::build(engine.store()).expect("build");

    let far = geo::Rect::new((4995.0, 4995.0), (5005.0, 5005.0));
    assert!(
        index.candidates_in(&far).is_empty(),
        "nothing is out there yet"
    );

    engine
        .bulk_load(points(1, 5000).as_bytes(), RdfFormat::Turtle, None)
        .expect("load one more");
    index.refresh(engine.store()).expect("refresh");
    assert_eq!(
        index.candidates_in(&far).len(),
        1,
        "the geometry added after the build was not picked up"
    );
}

#[test]
fn deleting_quads_rebuilds_rather_than_leaking() {
    // Deletion is not tracked incrementally, because a departed geometry costs space and not
    // correctness — the `VALUES` it appears in simply fails to join. What is tracked is the
    // store getting *smaller*, which triggers a rebuild so the index does not grow for ever.
    use holos_security::Session;
    use oxrdf::NamedNode;

    let mut engine = Engine::new();
    engine
        .bulk_load(points(40, 0).as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    let index = SpatialIndex::build(engine.store()).expect("build");
    assert_eq!(index.len(), 40);

    let mut session = Session::unrestricted(engine.store()).expect("session");
    let update = spargebra::SparqlParser::new()
        .parse_update(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
             DELETE WHERE { ?s geo:asWKT ?g }",
        )
        .expect("parse");
    holos_engine::update::apply(&mut engine, &mut session, &update).expect("delete");
    let _ = NamedNode::new_unchecked("urn:unused");

    index.refresh(engine.store()).expect("refresh");
    assert_eq!(
        index.len(),
        0,
        "the store lost every geometry, so a refresh should have reclaimed them"
    );
    assert!(index.is_current_for(engine.store()));
}

#[test]
fn many_small_refreshes_still_answer_correctly() {
    // Repeated one-at-a-time insertion degrades an R-tree's packing, so the index repacks
    // itself past a threshold. The repack must not change an answer — it is the same
    // entries, rebuilt into a better-shaped tree.
    let mut engine = Engine::new();
    engine
        .bulk_load(points(20, 0).as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    let index = SpatialIndex::build(engine.store()).expect("build");

    for round in 1..=20 {
        engine
            .bulk_load(points(5, round * 100).as_bytes(), RdfFormat::Turtle, None)
            .expect("load");
        index.refresh(engine.store()).expect("refresh");
    }
    let rebuilt = SpatialIndex::build(engine.store()).expect("rebuild");
    assert_eq!(index.len(), rebuilt.len());
    let whole = geo::Rect::new((-1e6, -1e6), (1e6, 1e6));
    assert!(agree(&index, &rebuilt, &whole));
    assert_eq!(index.candidates_in(&whole).len(), 20 + 20 * 5);
}
