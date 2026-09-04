//! Data published in one reference system, queried against data published in another.
//!
//! This is the case GeoSPARQL does not cover and real datasets always present. The
//! specification puts the reference system inside the literal and then provides no way to
//! change it, on the assumption that a dataset is internally consistent — which stops being
//! true the moment two of them are joined. Ordnance Survey open data is EPSG:27700, a phone
//! emits WGS 84, a tile server speaks EPSG:3857, and a query that wants all three has, until
//! now, had nothing to say.
//!
//! What is asserted here is not the arithmetic — `holos_engine::crs` owns that and checks it
//! against PROJ — but that the arithmetic is reachable from SPARQL: through the functions
//! this crate implements, through the 43 it borrows from `spargeo`, through the spatial
//! index, and *without* going around the access policy on the way.

use holos_engine::spatial::SpatialIndex;
use holos_engine::{Engine, QueryOptions};
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use oxrdf::{NamedNode, Term};
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use std::sync::Arc;

/// Five places, four reference systems, and one system this engine cannot transform.
///
/// The coordinates are the same three places written five ways, so any test that gets the
/// systems wrong gets an answer that is wrong by tens of kilometres rather than by a
/// rounding error. That is deliberate: a CRS bug that shows up as a small discrepancy is a
/// CRS bug nobody finds.
const PLACES: &str = r#"
@prefix ex:   <http://example.com/> .
@prefix geo:  <http://www.opengis.net/ont/geosparql#> .

# Ordnance Survey open data: British National Grid, eastings and northings in metres.
ex:cardiff a ex:Place ; ex:name "Cardiff Castle" ;
    geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/27700> POINT(318086.06 176511.05)"^^geo:wktLiteral .

# A GPS trace: CRS84, longitude then latitude, no prefix because that is the default.
ex:bristol a ex:Place ; ex:name "Bristol" ;
    geo:asWKT "POINT(-2.5879 51.4545)"^^geo:wktLiteral .

# A tile server: Web Mercator.
ex:norwich a ex:Place ; ex:name "Norwich Castle" ;
    geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/3857> POINT(144203.2684 6914647.3107)"^^geo:wktLiteral .

# An EPSG:4326 publisher, which means latitude first.
ex:sydney a ex:Place ; ex:name "Sydney" ;
    geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(-33.8688 151.2093)"^^geo:wktLiteral .

# The French national grid, which this engine has no transformation for.
ex:paris a ex:Place ; ex:name "Paris" ;
    geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/2154> POINT(652000 6862000)"^^geo:wktLiteral .

# A search area around Cardiff, in degrees.
ex:southWales a ex:Area ;
    geo:asWKT "POLYGON((-3.4 51.3, -3.0 51.3, -3.0 51.7, -3.4 51.7, -3.4 51.3))"^^geo:wktLiteral .

# The same area, on the grid.
ex:southWalesGrid a ex:Area ;
    geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/27700> POLYGON((303000 168000, 331000 168000, 331000 213000, 303000 213000, 303000 168000))"^^geo:wktLiteral .
"#;

const PREFIXES: &str = r"
PREFIX ex:    <http://example.com/>
PREFIX geo:   <http://www.opengis.net/ont/geosparql#>
PREFIX geof:  <http://www.opengis.net/def/function/geosparql/>
PREFIX uom:   <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX holos: <https://holos.dev/ns#>
PREFIX xsd:   <http://www.w3.org/2001/XMLSchema#>
";

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(PLACES.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

fn rows(query: &str) -> Vec<String> {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    solutions(&view, &format!("{PREFIXES}{query}"), &QueryOptions::new())
}

fn solutions(
    view: &holos_engine::view::DatasetView<'_>,
    query: &str,
    options: &QueryOptions,
) -> Vec<String> {
    let (results, _) = Engine::query_with(view, query, options).expect("query");
    let QueryResults::Solutions(iter) = results else {
        panic!("expected solutions");
    };
    let mut out: Vec<String> = iter
        .map(|solution| {
            solution
                .expect("solution")
                .iter()
                .map(|(variable, term)| format!("{}={term}", variable.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

/// The headline case: a polygon in degrees selecting a point published on the grid.
///
/// Before reference systems were supported this returned nothing at all — not an error, an
/// empty result, which is the failure mode that gets deployed.
#[test]
fn a_polygon_in_degrees_selects_a_point_on_the_british_national_grid() {
    let found = rows(
        "SELECT ?name WHERE {
           ex:southWales geo:asWKT ?area .
           ?place ex:name ?name ; geo:asWKT ?point .
           FILTER(geof:sfWithin(?point, ?area))
         }",
    );
    assert_eq!(found, vec![r#"name="Cardiff Castle""#]);
}

/// And the other way round: an area on the grid selecting a point in degrees.
#[test]
fn a_polygon_on_the_grid_selects_a_point_in_degrees() {
    let found = rows(
        "SELECT ?name WHERE {
           ex:southWalesGrid geo:asWKT ?area .
           ?place ex:name ?name ; geo:asWKT ?point .
           FILTER(geof:sfWithin(?point, ?area))
         }",
    );
    assert_eq!(found, vec![r#"name="Cardiff Castle""#]);
}

/// `geof:distance` between two systems, in metres.
///
/// Cardiff to Bristol is 41.2 km. Reading the grid reference as degrees instead — the
/// failure this transformation exists to prevent — would put Cardiff several thousand
/// kilometres away, so the assertion does not need to be tight to be decisive.
#[test]
fn distance_measures_between_two_reference_systems() {
    let found = rows(
        "SELECT ?km WHERE {
           ex:cardiff geo:asWKT ?a .
           ex:bristol geo:asWKT ?b .
           BIND(geof:distance(?a, ?b, uom:kilometre) AS ?km)
         }",
    );
    assert_eq!(found.len(), 1);
    let km: f64 = found[0]
        .trim_start_matches("km=\"")
        .split('"')
        .next()
        .expect("a number")
        .parse()
        .expect("a number");
    assert!(
        (41.0..41.4).contains(&km),
        "Cardiff to Bristol came to {km} km"
    );
}

/// A metre buffer around a grid geometry, tested by what it then contains.
///
/// The buffer is computed in CRS84 after the transformation, so this is also a check that
/// the transformation happens *before* the projection `geof:buffer` does internally, and
/// not twice.
#[test]
fn a_metre_buffer_around_a_grid_point_reaches_the_right_distance() {
    // 50 km from Cardiff reaches Bristol, at 41 km. 30 km does not.
    for (radius, expected) in [(50_000, 1), (30_000, 0)] {
        let found = rows(&format!(
            "SELECT ?name WHERE {{
               ex:cardiff geo:asWKT ?centre .
               ex:bristol ex:name ?name ; geo:asWKT ?point .
               FILTER(geof:sfWithin(?point, geof:buffer(?centre, {radius}, uom:metre)))
             }}"
        ));
        assert_eq!(found.len(), expected, "{radius} m reached {found:?}");
    }
}

/// One of the 43 functions this crate does not implement, working on grid data.
///
/// `geof:envelope` comes from `spargeo`, which refuses anything that is not CRS84. It works
/// here only because every borrowed function is registered through `geo_ext::crs_aware`, so
/// this test fails the moment that wrapper is dropped.
#[test]
fn a_borrowed_spargeo_function_reads_the_grid_too() {
    let found = rows(
        "SELECT ?wkt WHERE {
           ex:southWalesGrid geo:asWKT ?area .
           BIND(STR(geof:envelope(?area)) AS ?wkt)
         }",
    );
    assert_eq!(found.len(), 1);
    // The envelope of the grid polygon, expressed in degrees, spans roughly -3.4 to -3.0.
    assert!(found[0].contains("-3.4"), "{}", found[0]);
    assert!(found[0].contains("POLYGON"), "{}", found[0]);
}

/// The four set operations take the same route, and need their own test because they take
/// a different one *inside*.
///
/// `geof:union` and its three siblings are `spargeo`'s implementations wrapped in `geo_ext`
/// so their output can be snapped back onto their inputs (§17). That wrapper sits inside
/// the registration, so `crs_aware` never sees these calls and `set_operation` has to
/// rewrite the arguments itself. Two ways in, two things to get wrong.
#[test]
fn a_grid_geometry_survives_a_set_operation() {
    let found = rows(
        "SELECT ?area WHERE {
           ex:southWalesGrid geo:asWKT ?grid .
           ex:southWales geo:asWKT ?degrees .
           BIND(geof:area(geof:union(?grid, ?degrees), uom:square_metre) AS ?area)
         }",
    );
    assert_eq!(found.len(), 1);
    assert!(
        !found[0].is_empty(),
        "the union came back unbound: {found:?}"
    );
    // The two areas are the same patch of south Wales described twice — 28 km by 45 km —
    // so their union is about one of them rather than two: 1.6 million square kilometres
    // would mean the grid polygon had been read as degrees and unioned with a continent.
    let area: f64 = found[0]
        .trim_start_matches("area=\"")
        .split('"')
        .next()
        .expect("a number")
        .parse()
        .expect("a number");
    assert!(
        (1.2e9..2.0e9).contains(&area),
        "the union came to {area} square metres"
    );
}

/// `geof:getSRID` reports what each literal declares, which is the only useful answer and
/// the one `spargeo` cannot give: its version returns CRS84 unconditionally.
#[test]
fn get_srid_reports_each_publishers_own_system() {
    let found = rows(
        "SELECT ?name ?srid WHERE {
           ?place ex:name ?name ; geo:asWKT ?geometry .
           BIND(geof:getSRID(?geometry) AS ?srid)
         }",
    );
    let systems: Vec<&str> = found.iter().map(String::as_str).collect();
    assert!(systems
        .iter()
        .any(|row| row.contains("Cardiff") && row.contains("EPSG/0/27700")));
    assert!(systems
        .iter()
        .any(|row| row.contains("Bristol") && row.contains("OGC/1.3/CRS84")));
    assert!(systems
        .iter()
        .any(|row| row.contains("Norwich") && row.contains("EPSG/0/3857")));
    assert!(systems
        .iter()
        .any(|row| row.contains("Sydney") && row.contains("EPSG/0/4326")));
    // Paris is in a system this engine cannot read, so it has no SRID to report and the
    // BIND leaves the variable unbound rather than guessing.
    assert!(
        systems.iter().any(|row| row.contains("Paris")),
        "Paris should still be a row: {found:?}"
    );
    assert!(
        !systems.iter().any(|row| row.contains("EPSG/0/2154")),
        "a system with no transformation must not be reported as readable: {found:?}"
    );
}

/// The answer usually has to go back out as a grid reference, which is what
/// `holos:transform` is for. It is the only function whose output is not CRS84.
#[test]
fn transform_hands_the_answer_back_as_a_grid_reference() {
    let found = rows(
        "SELECT ?grid WHERE {
           ex:bristol geo:asWKT ?point .
           BIND(STR(holos:transform(?point, \"http://www.opengis.net/def/crs/EPSG/0/27700\"^^xsd:anyURI)) AS ?grid)
         }",
    );
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("EPSG/0/27700"), "{}", found[0]);
    // Bristol is around easting 359000, northing 172500 on the National Grid.
    assert!(found[0].contains("POINT(359"), "{}", found[0]);
}

/// A system with no transformation here comes back unbound, not wrong.
///
/// This is the assertion that matters most in the whole file. Every other failure mode is
/// loud; treating an unrecognised CRS as CRS84 is silent, and it turns 652000 metres east
/// of the French origin into 652000 degrees of longitude — a geometry that will happily
/// take part in comparisons and be absent from every answer it should be in.
#[test]
fn an_unsupported_system_yields_no_geometry_rather_than_a_wrong_one() {
    let found = rows(
        "SELECT ?name WHERE {
           ?place ex:name ?name ; geo:asWKT ?point .
           FILTER(geof:sfWithin(?point,
             \"POLYGON((-180 -90, 180 -90, 180 90, -180 90, -180 -90))\"^^geo:wktLiteral))
         }",
    );
    // Everything on earth is inside the whole earth. Paris is not, because its literal
    // cannot be read at all.
    assert_eq!(
        found,
        vec![
            r#"name="Bristol""#,
            r#"name="Cardiff Castle""#,
            r#"name="Norwich Castle""#,
            r#"name="Sydney""#,
        ]
    );

    // The sharper version of the same claim. Read as CRS84, EPSG:2154's 652000 metres east
    // becomes 652000 degrees of longitude, which falls outside every polygon and so passes
    // the assertion above without anything being right. A distance does not have that
    // escape: it either refuses the literal or returns a number, and a number here would
    // be the wrong answer given confidently.
    let measured = rows(
        "SELECT ?d WHERE {
           ex:paris   geo:asWKT ?a .
           ex:bristol geo:asWKT ?b .
           BIND(geof:distance(?a, ?b, uom:metre) AS ?d)
         }",
    );
    assert_eq!(
        measured,
        vec![""],
        "an unreadable geometry produced a distance: {measured:?}"
    );

    // And it is not in the spatial index either, so no probe can propose it.
    let engine = engine();
    let index = SpatialIndex::build(engine.store()).expect("build");
    assert_eq!(index.len(), 6, "Paris should have no index entry");
}

/// The two search areas as inline literals, which is what the index router needs.
///
/// `topology::restrict` only fires when the probe is a constant, so an area bound from a
/// triple pattern would leave both sides of the comparison below on the unindexed path —
/// two identical runs, agreeing about nothing.
const AREAS: [(&str, &str); 2] = [
    (
        "degrees",
        r#""POLYGON((-3.4 51.3, -3.0 51.3, -3.0 51.7, -3.4 51.7, -3.4 51.3))"^^geo:wktLiteral"#,
    ),
    (
        "grid",
        r#""<http://www.opengis.net/def/crs/EPSG/0/27700> POLYGON((303000 168000, 331000 168000, 331000 213000, 303000 213000, 303000 168000))"^^geo:wktLiteral"#,
    ),
];

/// The index builds its boxes by decoding each geometry literal through the same path a
/// query uses, so it must understand reference systems or it stops proposing the geometries
/// that use them.
///
/// Asserted against the index directly, because it is the sharp version of the claim: a
/// grid easting of 318086 taken raw would become a bounding box 318086 degrees east, and
/// `candidates` for a probe over Wales would come back without Cardiff in it.
#[test]
fn the_spatial_index_proposes_geometries_whatever_system_they_use() {
    let engine = engine();
    let index = SpatialIndex::build(engine.store()).expect("build");

    // Four of the five places have a readable geometry; Paris does not, and an index entry
    // for it would mean something had guessed at EPSG:2154.
    assert_eq!(
        index.len(),
        6,
        "five places and two areas, less unreadable Paris"
    );

    let cardiff =
        holos_engine::geo_ext::geometry_of(&Term::Literal(oxrdf::Literal::new_typed_literal(
            "<http://www.opengis.net/def/crs/EPSG/0/27700> POINT(318086.06 176511.05)",
            oxrdf::NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
        )))
        .expect("Cardiff reads");
    let proposed = index.candidates(&cardiff);
    assert!(
        !proposed.is_empty(),
        "a probe at Cardiff proposed nothing, so the grid geometry is not in the index"
    );
    assert!(
        proposed.len() < index.len(),
        "the index proposed everything, which is not an index"
    );
}

/// And routed queries must give the same answer as unrouted ones, either way round.
///
/// An index can only go wrong by omitting rows, and an omission is invisible unless
/// something holds the two answers side by side.
#[test]
fn the_spatial_index_and_a_full_scan_agree_across_reference_systems() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let index = Arc::new(SpatialIndex::build(engine.store()).expect("build"));

    for (label, area) in AREAS {
        let query = format!(
            "{PREFIXES} SELECT ?name WHERE {{
               ?place ex:name ?name ; geo:asWKT ?point .
               FILTER(geof:sfWithin(?point, {area}))
             }}"
        );
        let scanned = solutions(&view, &query, &QueryOptions::new());
        let indexed = solutions(
            &view,
            &query,
            &QueryOptions::new().with_spatial(Arc::clone(&index)),
        );
        assert_eq!(scanned, indexed, "{label} disagreed");
        assert_eq!(scanned, vec![r#"name="Cardiff Castle""#], "{label}");
    }
}

/// §14 does not make an exception for coordinates.
///
/// A transformation is arithmetic on a literal the session already holds, so it cannot
/// widen what is visible — but "cannot" is worth asserting rather than assuming, because
/// the whole value of enforcing policy at the scan is that no operator has another route
/// to the indexes, and a geometry function is an operator like any other.
#[test]
fn policy_still_decides_which_geometries_a_session_can_transform() {
    let engine = engine();
    let policy = Policy::permit_all().with_rule(Rule::deny(
        Modes::READ,
        Scope::Predicate(NamedNode::new_unchecked(
            "http://www.opengis.net/ont/geosparql#asWKT",
        )),
        PrincipalMatch::Everyone,
    ));
    let session = Session::open(engine.store(), Principal::anonymous(), policy).expect("session");
    let view = engine.view(&session);

    for query in [
        "SELECT ?srid WHERE { ?s geo:asWKT ?g . BIND(geof:getSRID(?g) AS ?srid) }",
        "SELECT ?grid WHERE { ?s geo:asWKT ?g .
           BIND(holos:transform(?g, \"http://www.opengis.net/def/crs/EPSG/0/27700\"^^xsd:anyURI) AS ?grid) }",
        "SELECT ?d WHERE { ex:cardiff geo:asWKT ?a . ex:bristol geo:asWKT ?b .
           BIND(geof:distance(?a, ?b, uom:metre) AS ?d) }",
    ] {
        let found = solutions(&view, &format!("{PREFIXES}{query}"), &QueryOptions::new());
        assert!(
            found.is_empty(),
            "a session denied the geometry predicate got {found:?}"
        );
    }

    // And the denial is of the geometries, not of the whole dataset: the names still read.
    let names = solutions(
        &view,
        &format!("{PREFIXES} SELECT ?name WHERE {{ ?s ex:name ?name }}"),
        &QueryOptions::new(),
    );
    assert_eq!(names.len(), 5, "{names:?}");
}

/// Every system the engine advertises is one it can actually read from a literal.
///
/// A list that says more than the code does is worse than no list, because the first thing
/// anyone does with it is write a query against it.
#[test]
fn every_advertised_system_can_be_read_and_written() {
    use holos_engine::crs::Crs;
    for crs in Crs::all() {
        let found = rows(&format!(
            "SELECT ?back WHERE {{
               ex:bristol geo:asWKT ?point .
               BIND(holos:transform(?point, \"{}\"^^xsd:anyURI) AS ?there)
               BIND(geof:getSRID(?there) AS ?back)
             }}",
            crs.uri()
        ));
        assert_eq!(found.len(), 1, "{crs:?}");
        assert!(found[0].contains(crs.uri()), "{crs:?}: {}", found[0]);
    }
}

/// A term that is not a geometry is not a geometry in any reference system.
#[test]
fn the_new_functions_leave_non_geometries_unbound() {
    let found = rows(
        "SELECT ?srid ?grid WHERE {
           ?s ex:name ?name .
           BIND(geof:getSRID(?name) AS ?srid)
           BIND(holos:transform(?name, \"http://www.opengis.net/def/crs/EPSG/0/27700\"^^xsd:anyURI) AS ?grid)
         }",
    );
    assert_eq!(found, vec![""; 5], "a name is not a geometry: {found:?}");
}

/// The IRIs are what the documentation says they are.
#[test]
fn the_registered_function_iris_include_the_new_ones() {
    let iris: Vec<String> = holos_engine::geo_ext::function_iris()
        .iter()
        .map(|iri| iri.as_str().to_owned())
        .collect();
    for expected in [
        "https://holos.dev/ns#transform",
        "http://www.opengis.net/def/function/geosparql/getSRID",
    ] {
        assert!(
            iris.iter().any(|iri| iri == expected),
            "{expected}: {iris:?}"
        );
    }
    // And they are registered, not merely listed.
    let _ = Term::from(NamedNode::new_unchecked("https://holos.dev/ns#transform"));
    assert!(!rows("SELECT ?x WHERE { BIND(holos:transform(\"POINT(0 0)\"^^geo:wktLiteral, \"http://www.opengis.net/def/crs/EPSG/0/3857\"^^xsd:anyURI) AS ?x) }")[0].is_empty());
}
