//! `geof:buffer` and `geof:boundary`.
//!
//! `spargeo` supplies 43 GeoSPARQL functions, but not these two — a fact this codebase
//! found the hard way, having claimed them in `DESIGN.md` §17 before probing for them.
//! They are the two most commonly reached-for constructive operators after the topological
//! relations, so they are implemented here and registered alongside the rest.
//!
//! Everything about the literal handling matches `spargeo`'s conventions deliberately, so
//! the two sets compose: `geo:wktLiteral` and `geo:geoJSONLiteral` accepted on input,
//! output in whichever of the two the arguments used, and the same OGC unit IRIs. A
//! function that round-tripped literals differently from its neighbours would be worse
//! than one that did not exist.
//!
//! # Reference systems
//!
//! One convention is deliberately *not* matched. `spargeo` accepts CRS84 and refuses every
//! other reference system, which makes a dataset published on the British National Grid
//! unqueryable rather than merely awkward. This module reads any system [`crate::crs`]
//! knows, converts it to CRS84 on the way in, and works in CRS84 from there.
//!
//! That single decision is what lets data in one system be queried against data in
//! another: two operands reach a function already in the same space, so
//! `geof:distance(?ordnance_survey_point, ?gps_point, uom:metre)` is an ordinary distance
//! rather than a category error. It also means the 43 functions this module does *not*
//! implement gain the same behaviour, because [`crs_aware`] rewrites their arguments before
//! `spargeo` ever sees them.
//!
//! Two consequences worth stating rather than discovering:
//!
//! - **Output is always CRS84**, whatever went in. `holos:transform` converts it back.
//! - **`geof:getSRID` reports what the literal says**, not what this module converted it
//!   to, so it is the one function that must not be wrapped.

use crate::crs::Crs;
use geo::algorithm::{Buffer, Centroid};
use geo::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon,
};
use geojson::{GeoJson, Geometry as GeoJsonGeometry};
use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode, NamedNodeRef, Term};
use std::str::FromStr;
use wkt::{ToWkt, TryFromWkt};

/// `geo:wktLiteral`.
const WKT_LITERAL: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral");
/// `geo:geoJSONLiteral`.
const GEO_JSON_LITERAL: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#geoJSONLiteral");
/// The reference system this module works in, and writes.
const CRS84_URI: &str = crate::crs::CRS84_URI;
/// Root of the OGC unit-of-measure IRIs.
const OGC_UOM_PREFIX: &str = "http://www.opengis.net/def/uom/OGC/1.0/";

/// `geof:buffer`.
const BUFFER: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/buffer");
/// `geof:boundary`.
const UNION: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/union");
const INTERSECTION: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/intersection");
const DIFFERENCE: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/difference");
const SYM_DIFFERENCE: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/symDifference");
const BOUNDARY: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/boundary");
const DISTANCE: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/distance");
/// `geof:getSRID`.
const GET_SRID: NamedNodeRef<'_> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/def/function/geosparql/getSRID");
/// `holos:transform`, which GeoSPARQL has no equivalent of.
///
/// The specification has no transform function at all — it assumes every literal in a
/// dataset already shares a reference system, which stops being true the moment two
/// datasets are joined. Putting one in the `geof:` namespace would claim OGC sanction it
/// does not have, so it goes in this project's own.
const TRANSFORM: NamedNodeRef<'_> = NamedNodeRef::new_unchecked("https://holos.dev/ns#transform");

/// The functions this module adds to, or replaces in, `spargeo`'s 43.
pub const EXTRA_GEOSPARQL_FUNCTIONS: [(NamedNodeRef<'static>, fn(&[Term]) -> Option<Term>); 9] = [
    (BUFFER, geof_buffer),
    (BOUNDARY, geof_boundary),
    // Not a geometry operation at all: the one thing GeoSPARQL leaves out.
    (TRANSFORM, holos_transform),
    // A replacement, because `spargeo`'s answers CRS84 unconditionally — true of every
    // literal it could parse, and false of every literal this module added support for.
    (GET_SRID, geof_get_srid),
    // The four set operations are `spargeo`'s, wrapped to snap their output back onto the
    // inputs' coordinates. Registered after `spargeo`'s own so these win; see
    // `snap_to_inputs` for what they are correcting and why it matters.
    (UNION, geof_union),
    (INTERSECTION, geof_intersection),
    (DIFFERENCE, geof_difference),
    (SYM_DIFFERENCE, geof_sym_difference),
    // Also a replacement, and for a plainer reason: `spargeo`'s takes both operands as
    // points and returns unbound for anything else. See `geof_distance`.
    (DISTANCE, geof_distance),
];

// ---------------------------------------------------------------------------------
// literal plumbing — deliberately identical in behaviour to spargeo's private helpers
// ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Wkt,
    GeoJson,
}

/// The GeoSPARQL function namespace.
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";

/// How close an output coordinate must be to an input one to be treated as that input.
///
/// The perturbation being corrected is around 1e-10 in absolute terms at these magnitudes.
/// 1e-9 degrees is roughly **0.1 mm** on the ground, far below any distance RDF geometry
/// data is meaningful at, and far above the error. Wide enough to catch it, narrow enough
/// that a genuinely distinct vertex cannot be swallowed.
const SNAP_EPSILON: f64 = 1e-9;

/// `spargeo`'s implementation of a function, by local name.
///
/// The functions are a public const array of name/pointer pairs, which is what makes it
/// possible to reuse an implementation rather than write a second one that would then have
/// to be kept in agreement with it.
fn spargeo_function(local: &str) -> Option<fn(&[Term]) -> Option<Term>> {
    let iri = format!("{GEOF}{local}");
    spargeo::GEOSPARQL_EXTENSION_FUNCTIONS
        .iter()
        .find(|(name, _)| name.as_str() == iri)
        .map(|(_, function)| *function)
}

/// Every coordinate value appearing in the arguments.
///
/// X and Y are kept apart. Mixing them would let a latitude snap to a longitude, which at
/// these tolerances is unlikely but is not a risk worth taking for nothing.
fn input_coordinates(args: &[Term]) -> (Vec<f64>, Vec<f64>) {
    // `map_coords` takes an `Fn`, so the collection goes through a Cell rather than a
    // captured `&mut`. Cheap, and avoids threading a second traversal helper through the
    // module for one caller.
    let xs = std::cell::RefCell::new(Vec::new());
    let ys = std::cell::RefCell::new(Vec::new());
    for arg in args {
        let Some(geometry) = extract_geometry(arg) else {
            continue;
        };
        map_coords(&geometry, &|c: Coord| {
            xs.borrow_mut().push(c.x);
            ys.borrow_mut().push(c.y);
            c
        });
    }
    (xs.into_inner(), ys.into_inner())
}

/// The input value this output coordinate is a perturbed copy of, if any.
fn nearest(value: f64, candidates: &[f64]) -> Option<f64> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| (candidate - value).abs() <= SNAP_EPSILON)
        .min_by(|a, b| {
            (a - value)
                .abs()
                .partial_cmp(&(b - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Restores coordinates that a boolean operation perturbed.
///
/// # The problem
///
/// `geo`'s boolean operations go through `i_overlay`, which works on an integer grid and
/// converts back on the way out. Coordinates that are exactly representable survive; others
/// come back shifted by about 1e-10. `-83.2` becomes `-83.20000000009313`.
///
/// That is 0.01 mm and harmless for anything measuring distance. It is *not* harmless for
/// the exact topological predicates, which turn on whether two boundaries coincide:
///
/// ```text
/// sfTouches(C, A)             -> true
/// sfTouches(C, union(A, D))   -> false     // same shared edge, now 1e-10 apart
/// ```
///
/// So `sfTouches`, `sfEquals` and `sfCrosses` silently stopped composing with any computed
/// geometry — the answer was wrong, not merely imprecise.
///
/// # The fix, and why it is not rounding
///
/// Rounding every coordinate to the inputs' decimal places would also move genuinely *new*
/// vertices: two integer-coordinate lines crossing at x = 1.5 would be rounded to 2, turning
/// a correct intersection into a wrong one.
///
/// Instead each output coordinate is compared against the coordinates that went in, and
/// replaced only when it is within [`SNAP_EPSILON`] of one of them. A preserved vertex
/// returns to its exact input value; a computed intersection point, which matches no input,
/// is left exactly as the algorithm produced it.
fn snap_to_inputs(result: &Term, args: &[Term]) -> Term {
    let (xs, ys) = input_coordinates(args);
    if xs.is_empty() {
        return result.clone();
    }
    let Some(geometry) = extract_geometry(result) else {
        return result.clone();
    };
    let snapped = map_coords(&geometry, &|c: Coord| Coord {
        x: nearest(c.x, &xs).unwrap_or(c.x),
        y: nearest(c.y, &ys).unwrap_or(c.y),
    });
    Term::Literal(to_literal(&snapped, pick_output_kind(args)))
}

/// Runs one of `spargeo`'s set operations and snaps the result back onto its inputs.
///
/// The arguments are moved into CRS84 first and both steps then see the moved ones, which
/// matters for the second step as much as the first: snapping compares the output against
/// the coordinates that went in, and an easting in metres is never within
/// [`SNAP_EPSILON`] of a degree of longitude. Snapping against the originals would not be
/// wrong so much as inert, and the topological predicates this exists to protect would
/// quietly stop composing again.
fn set_operation(local: &str, args: &[Term]) -> Option<Term> {
    let rewritten = to_crs84(args);
    let args = rewritten.as_deref().unwrap_or(args);
    let result = spargeo_function(local)?(args)?;
    Some(snap_to_inputs(&result, args))
}

fn geof_union(args: &[Term]) -> Option<Term> {
    set_operation("union", args)
}
fn geof_intersection(args: &[Term]) -> Option<Term> {
    set_operation("intersection", args)
}
fn geof_difference(args: &[Term]) -> Option<Term> {
    set_operation("difference", args)
}
fn geof_sym_difference(args: &[Term]) -> Option<Term> {
    set_operation("symDifference", args)
}

fn detect_kind(term: &Term) -> Option<Kind> {
    let Term::Literal(literal) = term else {
        return None;
    };
    if literal.datatype() == WKT_LITERAL {
        Some(Kind::Wkt)
    } else if literal.datatype() == GEO_JSON_LITERAL {
        Some(Kind::GeoJson)
    } else {
        None
    }
}

/// WKT wins if any argument is WKT, matching `spargeo`.
fn pick_output_kind(args: &[Term]) -> Kind {
    let mut seen_geojson = false;
    for term in args {
        match detect_kind(term) {
            Some(Kind::Wkt) => return Kind::Wkt,
            Some(Kind::GeoJson) => seen_geojson = true,
            None => {}
        }
    }
    if seen_geojson {
        Kind::GeoJson
    } else {
        Kind::Wkt
    }
}

/// The geometry a term carries, if it is a GeoSPARQL geometry literal.
///
/// Public so the spatial index can decide what to index without a second copy of the
/// datatype and CRS rules — those live here, and a second copy would drift.
pub fn geometry_of(term: &Term) -> Option<Geometry> {
    extract_geometry(term)
}

fn extract_geometry(term: &Term) -> Option<Geometry> {
    let Term::Literal(literal) = term else {
        return None;
    };
    let value = literal.value().trim();
    if literal.datatype() == WKT_LITERAL {
        parse_wkt_literal(value)
    } else if literal.datatype() == GEO_JSON_LITERAL {
        GeoJson::from_str(value).ok()?.try_into().ok()
    } else {
        None
    }
}

/// A WKT literal may carry a leading `<crs-uri>`; absent one, GeoSPARQL says CRS84.
///
/// An *unrecognised* system is still refused. Falling back to CRS84 would read eastings in
/// metres as degrees of longitude, which does not fail — it silently relocates the geometry
/// several thousand kilometres and answers every subsequent query confidently.
fn declared_crs(value: &str) -> Option<(Crs, &str)> {
    let value = value.trim_start();
    let Some(rest) = value.strip_prefix('<') else {
        return Some((Crs::Crs84, value));
    };
    let (system, rest) = rest.split_once('>')?;
    Some((Crs::from_uri(system)?, rest.trim_start()))
}

/// A WKT literal, parsed and moved into CRS84.
fn parse_wkt_literal(value: &str) -> Option<Geometry> {
    let (crs, wkt) = declared_crs(value)?;
    let geometry = Geometry::try_from_wkt_str(wkt).ok()?;
    reproject(&geometry, crs, Crs::Crs84)
}

/// Every coordinate of a geometry, moved from one reference system to another.
///
/// `None` if any single coordinate cannot be transformed. Partial success is not an option:
/// half a reprojected polygon is a shape that exists nowhere, and returning it would be
/// worse than returning nothing.
fn reproject(geom: &Geometry, from: Crs, to: Crs) -> Option<Geometry> {
    if from == to {
        return Some(geom.clone());
    }
    let failed = std::cell::Cell::new(false);
    let moved = map_coords(geom, &|c| match crate::crs::transform(from, to, c) {
        Some(c) => c,
        None => {
            failed.set(true);
            c
        }
    });
    (!failed.get()).then_some(moved)
}

fn to_literal(geom: &Geometry, kind: Kind) -> Literal {
    match kind {
        Kind::Wkt => {
            Literal::new_typed_literal(format!("<{CRS84_URI}> {}", geom.wkt_string()), WKT_LITERAL)
        }
        Kind::GeoJson => {
            Literal::new_typed_literal(GeoJsonGeometry::from(geom).to_string(), GEO_JSON_LITERAL)
        }
    }
}

fn extract_units_iri(term: &Term) -> Option<&str> {
    match term {
        Term::NamedNode(node) => Some(node.as_str()),
        Term::Literal(literal) if literal.datatype() == xsd::ANY_URI => Some(literal.value()),
        _ => None,
    }
}

fn extract_f64(term: &Term) -> Option<f64> {
    let Term::Literal(literal) = term else {
        return None;
    };
    let dt = literal.datatype();
    if dt == xsd::DOUBLE || dt == xsd::FLOAT || dt == xsd::DECIMAL || dt == xsd::INTEGER {
        literal.value().parse().ok()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------
// geof:buffer
// ---------------------------------------------------------------------------------

/// How a buffer radius was expressed.
enum Radius {
    /// Metres on the ground.
    Metres(f64),
    /// Degrees of arc, which in CRS84 is the coordinate unit itself.
    Degrees(f64),
}

fn radius_for(value: f64, units_iri: &str) -> Option<Radius> {
    let local = units_iri.strip_prefix(OGC_UOM_PREFIX)?;
    match local {
        "metre" | "meter" => Some(Radius::Metres(value)),
        "kilometre" | "kilometer" => Some(Radius::Metres(value * 1000.0)),
        "degree" => Some(Radius::Degrees(value)),
        "radian" => Some(Radius::Degrees(value.to_degrees())),
        _ => None,
    }
}

/// <http://www.opengis.net/def/function/geosparql/buffer>
///
/// `geof:buffer(geom, radius, units)` — the set of points within `radius` of `geom`.
///
/// # The projection, and why it is here
///
/// CRS84 coordinates are degrees, and a metre is not a fixed number of degrees: it is
/// about 1/111320 of a degree of latitude everywhere, but of longitude only at the equator,
/// shrinking by `cos(latitude)` toward the poles. Buffering degrees by a scalar therefore
/// produces a shape that is too wide away from the equator — badly so at high latitudes.
///
/// So a metric buffer is computed in a local equirectangular projection centred on the
/// geometry's centroid: coordinates are converted to metres, buffered there, and converted
/// back. That is accurate to well under a percent for buffers up to tens of kilometres,
/// which is the range these are used over.
///
/// It is **not** a geodesic buffer. Over continental distances, or across a pole, the
/// projection's own distortion dominates and the result should not be trusted. A radius in
/// degrees skips the projection entirely and is exact in coordinate space.
fn geof_buffer(args: &[Term]) -> Option<Term> {
    let args: &[Term; 3] = args.try_into().ok()?;
    let geom = extract_geometry(&args[0])?;
    let value = extract_f64(&args[1])?;
    let radius = radius_for(value, extract_units_iri(&args[2])?)?;

    if !value.is_finite() {
        return None;
    }

    let buffered: MultiPolygon = match radius {
        Radius::Degrees(d) => geom.buffer(d),
        Radius::Metres(m) => {
            let centre = geom.centroid()?;
            let (lon0, lat0) = (centre.x(), centre.y());
            // Metres per degree at this latitude. The latitude figure is the mean
            // meridional degree; the longitude figure narrows with the cosine.
            let m_per_deg_lat = 110_574.0;
            let m_per_deg_lon = 111_320.0 * lat0.to_radians().cos();
            if m_per_deg_lon.abs() < 1.0 {
                // Within a metre of a pole every longitude is the same place, and the
                // projection is singular. Refusing beats returning nonsense.
                return None;
            }
            let forward = |c: Coord| Coord {
                x: (c.x - lon0) * m_per_deg_lon,
                y: (c.y - lat0) * m_per_deg_lat,
            };
            let back = |c: Coord| Coord {
                x: lon0 + c.x / m_per_deg_lon,
                y: lat0 + c.y / m_per_deg_lat,
            };
            let projected = map_coords(&geom, &forward);
            let buffered = projected.buffer(m);
            match map_coords(&Geometry::MultiPolygon(buffered), &back) {
                Geometry::MultiPolygon(mp) => mp,
                _ => return None,
            }
        }
    };

    Some(to_literal(&Geometry::MultiPolygon(buffered), pick_output_kind(args)).into())
}

/// Applies a coordinate transform to every point of a geometry.
///
/// `geo` has `MapCoords`, but it is generic in a way that fights a closure captured by
/// reference across the recursive collection cases; this is shorter than satisfying it.
fn map_coords(geom: &Geometry, f: &impl Fn(Coord) -> Coord) -> Geometry {
    let line = |ls: &LineString| LineString(ls.0.iter().map(|c| f(*c)).collect());
    let poly = |p: &Polygon| {
        Polygon::new(
            line(p.exterior()),
            p.interiors().iter().map(&line).collect::<Vec<_>>(),
        )
    };
    match geom {
        Geometry::Point(p) => Geometry::Point(Point::from(f(p.0))),
        Geometry::MultiPoint(mp) => Geometry::MultiPoint(MultiPoint(
            mp.0.iter().map(|p| Point::from(f(p.0))).collect(),
        )),
        Geometry::Line(l) => Geometry::Line(geo::Line::new(f(l.start), f(l.end))),
        Geometry::LineString(ls) => Geometry::LineString(line(ls)),
        Geometry::MultiLineString(mls) => {
            Geometry::MultiLineString(MultiLineString(mls.0.iter().map(&line).collect()))
        }
        Geometry::Polygon(p) => Geometry::Polygon(poly(p)),
        Geometry::MultiPolygon(mp) => {
            Geometry::MultiPolygon(MultiPolygon(mp.0.iter().map(&poly).collect()))
        }
        Geometry::Rect(r) => Geometry::Polygon(poly(&r.to_polygon())),
        Geometry::Triangle(t) => Geometry::Polygon(poly(&t.to_polygon())),
        Geometry::GeometryCollection(gc) => Geometry::GeometryCollection(GeometryCollection(
            gc.0.iter().map(|g| map_coords(g, f)).collect(),
        )),
    }
}

// ---------------------------------------------------------------------------------
// geof:boundary
// ---------------------------------------------------------------------------------

/// <http://www.opengis.net/def/function/geosparql/boundary>
///
/// `geof:boundary(geom)` — the topological boundary, as OGC Simple Features defines it:
///
/// | Input | Boundary |
/// |---|---|
/// | Point, MultiPoint | empty |
/// | LineString | its two endpoints, or empty when closed |
/// | MultiLineString | endpoints appearing an **odd** number of times (the mod-2 rule) |
/// | Polygon, MultiPolygon | all rings, exterior and interior alike |
/// | GeometryCollection | the boundaries of its members, combined |
///
/// The mod-2 rule is the part worth stating: two lines joined end to end have a shared
/// point that is interior to the union, so it is not on the boundary. Counting occurrences
/// and keeping the odd ones is exactly that rule.
fn geof_boundary(args: &[Term]) -> Option<Term> {
    let args: &[Term; 1] = args.try_into().ok()?;
    let geom = extract_geometry(&args[0])?;
    Some(to_literal(&boundary_of(&geom), pick_output_kind(args)).into())
}

fn boundary_of(geom: &Geometry) -> Geometry {
    fn rings(p: &Polygon) -> Vec<LineString> {
        let mut out = vec![p.exterior().clone()];
        out.extend(p.interiors().iter().cloned());
        out
    }

    match geom {
        // A point has no boundary, and neither does a set of them.
        Geometry::Point(_) | Geometry::MultiPoint(_) => {
            Geometry::GeometryCollection(GeometryCollection(Vec::new()))
        }

        Geometry::Line(l) => {
            Geometry::MultiPoint(MultiPoint(vec![Point::from(l.start), Point::from(l.end)]))
        }

        Geometry::LineString(ls) => Geometry::MultiPoint(MultiPoint(line_endpoints(&[ls.clone()]))),

        Geometry::MultiLineString(mls) => Geometry::MultiPoint(MultiPoint(line_endpoints(&mls.0))),

        Geometry::Polygon(p) => Geometry::MultiLineString(MultiLineString(rings(p))),

        Geometry::MultiPolygon(mp) => {
            Geometry::MultiLineString(MultiLineString(mp.0.iter().flat_map(rings).collect()))
        }

        Geometry::Rect(r) => Geometry::MultiLineString(MultiLineString(rings(&r.to_polygon()))),
        Geometry::Triangle(t) => Geometry::MultiLineString(MultiLineString(rings(&t.to_polygon()))),

        Geometry::GeometryCollection(gc) => {
            Geometry::GeometryCollection(GeometryCollection(gc.0.iter().map(boundary_of).collect()))
        }
    }
}

/// Endpoints of a set of line strings, keeping those that occur an odd number of times.
///
/// Closed rings contribute nothing, because their single endpoint occurs twice.
fn line_endpoints(lines: &[LineString]) -> Vec<Point> {
    let mut counts: Vec<(Coord, usize)> = Vec::new();
    let mut bump = |c: Coord| {
        // Coordinates are f64 and not hashable; the endpoint count here is tiny, so a
        // linear scan with exact comparison is both simpler and faster than interning.
        if let Some(entry) = counts.iter_mut().find(|(k, _)| *k == c) {
            entry.1 += 1;
        } else {
            counts.push((c, 1));
        }
    };
    for ls in lines {
        if ls.0.len() < 2 {
            continue;
        }
        bump(ls.0[0]);
        bump(ls.0[ls.0.len() - 1]);
    }
    counts
        .into_iter()
        .filter(|(_, n)| n % 2 == 1)
        .map(|(c, _)| Point::from(c))
        .collect()
}

// ---------------------------------------------------------------------------------
// distance
// ---------------------------------------------------------------------------------

/// `geof:distance(g1, g2, units)` — the shortest distance between two geometries.
///
/// # Why this replaces `spargeo`'s
///
/// `spargeo`'s implementation reads both operands as *points* and returns `None` — an
/// unbound variable — for anything else. Against the OGC GeoSPARQL example dataset that was
/// every Polygon and every LineString in it: `geof:distance(?point, ?polygon, uom:metre)`
/// silently produced no binding rather than a distance. GeoSPARQL defines the function for
/// any two geometries, as the shortest distance between a point of one and a point of the
/// other.
///
/// # How the shortest distance is found
///
/// 1. Intersecting geometries are zero apart, which is the definition when they meet.
/// 2. Otherwise the minimum is taken over every vertex of each geometry against its closest
///    point on the other, in both directions.
///
/// Step 2 is exact, not a sample. For two straight segments that do not cross, the closest
/// pair is always attained at an endpoint of at least one of them; a polyline or a polygon
/// boundary is a union of segments, so walking every vertex against the whole of the other
/// geometry considers every candidate pair there is.
///
/// The closest pair is located in planar degrees and then measured with the same Haversine
/// formula `spargeo` uses, so a point-to-point call returns exactly the number it returned
/// before this existed. That is asserted by a test rather than assumed.
///
/// # Cost
///
/// Quadratic in the vertex counts: every vertex of each operand is tested against the whole
/// of the other. That is nothing for the geometries GeoSPARQL datasets actually carry — the
/// OGC example's polygons have five vertices each — and it would matter for two detailed
/// coastlines. Narrowing it needs an index reaching *inside* a single geometry, which is a
/// different structure from §17's, and that one indexes whole geometries.
fn geof_distance(args: &[Term]) -> Option<Term> {
    let [left, right, units] = args else {
        return None;
    };
    let factor = meter_factor(units)?;
    let left = extract_geometry(left)?;
    let right = extract_geometry(right)?;
    let meters = shortest_distance(&left, &right)?;
    Some(Literal::from(meters / factor).into())
}

/// The metres one unit of `units` represents, or `None` for an IRI that is not a length.
///
/// Restated rather than borrowed: `spargeo` keeps its unit tables private. The two it
/// supports are the two here, so a query that worked before still works, and one naming a
/// unit neither of us handles still comes back unbound rather than wrong.
fn meter_factor(units: &Term) -> Option<f64> {
    const OGC_UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";
    let Term::NamedNode(iri) = units else {
        return None;
    };
    match iri.as_str().strip_prefix(OGC_UOM)? {
        "metre" | "meter" => Some(1.0),
        "kilometre" | "kilometer" => Some(1000.0),
        _ => None,
    }
}

/// The shortest distance in metres between two geometries.
fn shortest_distance(left: &Geometry, right: &Geometry) -> Option<f64> {
    use geo::algorithm::{ClosestPoint, Intersects};
    use geo::CoordsIter;
    use geo::{Closest, Distance, Haversine};

    if left.intersects(right) {
        return Some(0.0);
    }

    let mut best = f64::INFINITY;
    let mut consider = |from: &Geometry, to: &Geometry| {
        for coord in from.coords_iter() {
            let vertex = Point::from(coord);
            // `Indeterminate` means the geometry is empty, and an empty geometry has no
            // closest point to offer; skipping it leaves `best` untouched.
            let closest = match to.closest_point(&vertex) {
                Closest::Intersection(p) | Closest::SinglePoint(p) => p,
                Closest::Indeterminate => continue,
            };
            let meters = Haversine.distance(vertex, closest);
            if meters < best {
                best = meters;
            }
        }
    };
    consider(left, right);
    consider(right, left);

    best.is_finite().then_some(best)
}

// ---------------------------------------------------------------------------------
// reference systems
// ---------------------------------------------------------------------------------

/// <http://www.opengis.net/def/function/geosparql/getSRID>
///
/// The reference system a geometry literal *declares*, which is the only useful answer and
/// not the one `spargeo` gives: its version returns CRS84 for anything it can parse, which
/// was true when CRS84 was the only thing it could parse.
///
/// A literal with no CRS prefix answers CRS84, because that is what GeoSPARQL says an
/// unprefixed literal means. GeoJSON always answers CRS84, because RFC 7946 allows nothing
/// else.
fn geof_get_srid(args: &[Term]) -> Option<Term> {
    let [term] = args else {
        return None;
    };
    let Term::Literal(literal) = term else {
        return None;
    };
    let crs = if literal.datatype() == WKT_LITERAL {
        // Parsed, not just prefix-matched: a well-formed CRS on malformed WKT is not a
        // geometry, and this function should not be the one place that says otherwise.
        let (crs, wkt) = declared_crs(literal.value().trim())?;
        <Geometry>::try_from_wkt_str(wkt).ok()?;
        crs
    } else if literal.datatype() == GEO_JSON_LITERAL {
        GeoJson::from_str(literal.value().trim()).ok()?;
        Crs::Crs84
    } else {
        return None;
    };
    Some(Literal::new_typed_literal(crs.uri(), xsd::ANY_URI).into())
}

/// <https://holos.dev/ns#transform>
///
/// `holos:transform(geom, crsUri)` — the same geometry, expressed in another reference
/// system.
///
/// This is the only function here whose output is not CRS84, and the reason it exists: a
/// query that has joined British National Grid data against GPS data still has to hand the
/// answer back to something, and that something usually wants grid references.
///
/// ```sparql
/// PREFIX holos: <https://holos.dev/ns#>
/// PREFIX uom:   <http://www.opengis.net/def/uom/OGC/1.0/>
/// SELECT ?site (holos:transform(?buffered, "http://www.opengis.net/def/crs/EPSG/0/27700") AS ?grid)
/// WHERE {
///   ?site :footprint ?shape .
///   BIND(geof:buffer(?shape, 500, uom:metre) AS ?buffered)
/// }
/// ```
///
/// The target may be given as an IRI or as an `xsd:anyURI` literal, because `geof:getSRID`
/// returns the latter and feeding one function's output to another should not need a cast.
/// An unrecognised system returns unbound rather than the geometry unchanged: a caller who
/// asked for EPSG:2154 and silently received CRS84 would have no way to tell.
fn holos_transform(args: &[Term]) -> Option<Term> {
    let [term, target] = args else {
        return None;
    };
    let target = Crs::from_uri(extract_units_iri(target)?)?;
    // The literal arrives in CRS84 whatever it declared, because `extract_geometry` moved
    // it there. So this is one hop, not two, and transforming to CRS84 is a no-op.
    let geom = extract_geometry(term)?;
    let moved = reproject(&geom, Crs::Crs84, target)?;
    let literal = match pick_output_kind(args) {
        // GeoJSON cannot express another reference system: RFC 7946 removed the `crs`
        // member and fixed the format at CRS84. Answering with GeoJSON would produce a
        // document whose numbers are eastings and whose schema says they are degrees.
        Kind::GeoJson if target != Crs::Crs84 => return None,
        Kind::GeoJson => to_literal(&moved, Kind::GeoJson),
        Kind::Wkt => Literal::new_typed_literal(
            format!("<{}> {}", target.uri(), moved.wkt_string()),
            WKT_LITERAL,
        ),
    };
    Some(literal.into())
}

/// A `spargeo` function, taught to read every reference system this module does.
///
/// `spargeo` parses its own literals and refuses anything that is not CRS84, so the 43
/// functions it supplies would be the one part of the surface where British National Grid
/// data did not work. Rewriting each argument to CRS84 first fixes all 43 at once, and
/// leaves `spargeo` doing exactly what it did before.
///
/// A literal that is already CRS84, or is not a geometry at all — a unit IRI, a radius —
/// is passed through untouched, so the wrapper costs a datatype comparison per argument on
/// the common path.
#[must_use]
pub fn crs_aware(
    function: fn(&[Term]) -> Option<Term>,
) -> impl Fn(&[Term]) -> Option<Term> + Send + Sync + 'static {
    move |args: &[Term]| match to_crs84(args) {
        Some(rewritten) => function(&rewritten),
        None => function(args),
    }
}

/// The arguments with every non-CRS84 geometry literal rewritten, or `None` if none needed
/// it — which is the case this is written to keep cheap.
fn to_crs84(args: &[Term]) -> Option<Vec<Term>> {
    let mut rewritten: Option<Vec<Term>> = None;
    for (index, term) in args.iter().enumerate() {
        let Term::Literal(literal) = term else {
            continue;
        };
        if literal.datatype() != WKT_LITERAL {
            continue;
        }
        let value = literal.value().trim();
        // `Some((Crs84, _))` is the common case and needs no work. `None` is a literal
        // neither side can read, and is left alone so `spargeo` refuses it as it always
        // did rather than this wrapper refusing it a step earlier.
        match declared_crs(value) {
            Some((Crs::Crs84, _)) | None => continue,
            Some(_) => {}
        }
        let target = rewritten.get_or_insert_with(|| args.to_vec());
        target[index] = match parse_wkt_literal(value) {
            Some(geom) => to_literal(&geom, Kind::Wkt).into(),
            None => continue,
        };
    }
    rewritten
}

/// The IRIs this module registers, for the documentation and tests to assert against.
#[must_use]
pub fn function_iris() -> Vec<NamedNode> {
    EXTRA_GEOSPARQL_FUNCTIONS
        .iter()
        .map(|(iri, _)| iri.into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkt(s: &str) -> Term {
        Literal::new_typed_literal(s.to_owned(), WKT_LITERAL).into()
    }
    fn units(local: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(format!("{OGC_UOM_PREFIX}{local}")))
    }
    fn number(v: f64) -> Term {
        Literal::new_typed_literal(v.to_string(), xsd::DOUBLE).into()
    }
    fn geometry_of(term: &Term) -> Geometry {
        extract_geometry(term).expect("result parses back")
    }

    #[test]
    fn buffering_a_point_in_degrees_gives_a_polygon_of_that_radius() {
        let out =
            geof_buffer(&[wkt("POINT(0 0)"), number(1.0), units("degree")]).expect("buffered");
        let geom = geometry_of(&out);
        let Geometry::MultiPolygon(mp) = &geom else {
            panic!("expected a multipolygon");
        };
        assert_eq!(mp.0.len(), 1);
        use geo::algorithm::BoundingRect;
        let rect = geom.bounding_rect().expect("has extent");
        // A unit-radius disc around the origin spans -1..1 in both axes.
        assert!((rect.min().x + 1.0).abs() < 0.05, "min x {}", rect.min().x);
        assert!((rect.max().x - 1.0).abs() < 0.05, "max x {}", rect.max().x);
        assert!((rect.min().y + 1.0).abs() < 0.05, "min y {}", rect.min().y);
    }

    #[test]
    fn a_metre_buffer_narrows_in_longitude_away_from_the_equator() {
        use geo::algorithm::BoundingRect;
        // The same 100km buffer at the equator and at 60°N. At 60° a degree of longitude
        // is half as long, so the buffer must span about twice as many degrees of it.
        let at_equator = geometry_of(
            &geof_buffer(&[wkt("POINT(0 0)"), number(100_000.0), units("metre")]).expect("eq"),
        );
        let at_sixty = geometry_of(
            &geof_buffer(&[wkt("POINT(0 60)"), number(100_000.0), units("metre")]).expect("60"),
        );
        let w = |g: &Geometry| {
            let r = g.bounding_rect().expect("extent");
            r.max().x - r.min().x
        };
        let ratio = w(&at_sixty) / w(&at_equator);
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "expected roughly 2x wider in degrees at 60N, got {ratio}"
        );
    }

    #[test]
    fn kilometres_and_metres_agree() {
        use geo::algorithm::BoundingRect;
        let a = geometry_of(
            &geof_buffer(&[wkt("POINT(5 5)"), number(2000.0), units("metre")]).expect("m"),
        );
        let b = geometry_of(
            &geof_buffer(&[wkt("POINT(5 5)"), number(2.0), units("kilometre")]).expect("km"),
        );
        let w = |g: &Geometry| {
            let r = g.bounding_rect().expect("extent");
            r.max().x - r.min().x
        };
        assert!((w(&a) - w(&b)).abs() < 1e-9);
    }

    #[test]
    fn an_unknown_unit_is_refused() {
        assert!(geof_buffer(&[wkt("POINT(0 0)"), number(1.0), units("furlong")]).is_none());
    }

    #[test]
    fn the_boundary_of_a_polygon_is_its_rings() {
        let out = geof_boundary(&[wkt("POLYGON((0 0,0 1,1 1,1 0,0 0))")]).expect("boundary");
        let Geometry::MultiLineString(mls) = geometry_of(&out) else {
            panic!("expected a multilinestring");
        };
        assert_eq!(mls.0.len(), 1);
        assert_eq!(mls.0[0].0.len(), 5);
    }

    #[test]
    fn a_polygon_with_a_hole_has_two_boundary_rings() {
        let out = geof_boundary(&[wkt(
            "POLYGON((0 0,0 10,10 10,10 0,0 0),(2 2,2 4,4 4,4 2,2 2))",
        )])
        .expect("boundary");
        let Geometry::MultiLineString(mls) = geometry_of(&out) else {
            panic!("expected a multilinestring");
        };
        assert_eq!(mls.0.len(), 2, "exterior and interior rings both count");
    }

    #[test]
    fn the_boundary_of_a_line_is_its_endpoints() {
        let out = geof_boundary(&[wkt("LINESTRING(0 0,1 1,2 0)")]).expect("boundary");
        let Geometry::MultiPoint(mp) = geometry_of(&out) else {
            panic!("expected a multipoint");
        };
        assert_eq!(mp.0.len(), 2);
    }

    #[test]
    fn a_closed_line_has_an_empty_boundary() {
        let out = geof_boundary(&[wkt("LINESTRING(0 0,1 0,1 1,0 0)")]).expect("boundary");
        let Geometry::MultiPoint(mp) = geometry_of(&out) else {
            panic!("expected a multipoint");
        };
        assert!(mp.0.is_empty(), "a ring has no endpoints");
    }

    #[test]
    fn joined_lines_drop_the_shared_endpoint() {
        // Two segments meeting at (1 1). That point is interior to the union, so the
        // mod-2 rule must exclude it and leave only the two outer ends.
        let out = geof_boundary(&[wkt("MULTILINESTRING((0 0,1 1),(1 1,2 2))")]).expect("boundary");
        let Geometry::MultiPoint(mp) = geometry_of(&out) else {
            panic!("expected a multipoint");
        };
        assert_eq!(mp.0.len(), 2, "the shared join is not on the boundary");
    }

    #[test]
    fn a_point_has_no_boundary() {
        let out = geof_boundary(&[wkt("POINT(1 1)")]).expect("boundary");
        let Geometry::GeometryCollection(gc) = geometry_of(&out) else {
            panic!("expected an empty collection");
        };
        assert!(gc.0.is_empty());
    }

    // -----------------------------------------------------------------------------
    // reference systems
    // -----------------------------------------------------------------------------

    /// Cardiff Castle, in each of the four systems, to a metre.
    const CARDIFF: [(&str, &str); 4] = [
        (crate::crs::CRS84_URI, "POINT(-3.181 51.4816)"),
        (crate::crs::EPSG_4326_URI, "POINT(51.4816 -3.181)"),
        (crate::crs::EPSG_27700_URI, "POINT(318086.06 176511.05)"),
        (crate::crs::EPSG_3857_URI, "POINT(-354107.3 6706929.42)"),
    ];

    fn crs_wkt(crs: &str, geometry: &str) -> Term {
        wkt(&format!("<{crs}> {geometry}"))
    }

    /// A system with no transformation here is still refused, and that is the point.
    ///
    /// EPSG:2154 is the French national grid: a perfectly ordinary system, in metres, whose
    /// coordinates would read as plausible degrees only if you did not look. Treating an
    /// unknown CRS as CRS84 is the failure mode this guards, and it is silent.
    #[test]
    fn a_reference_system_this_engine_cannot_transform_is_refused() {
        let french = crs_wkt(
            "http://www.opengis.net/def/crs/EPSG/0/2154",
            "POINT(650000 6860000)",
        );
        assert!(geof_boundary(&[french.clone()]).is_none());
        assert!(extract_geometry(&french).is_none());
        assert!(geof_get_srid(&[french]).is_none());
    }

    /// The four spellings of Cardiff Castle must all parse to the same place.
    #[test]
    fn every_supported_system_reads_to_the_same_point() {
        let expected = Coord {
            x: -3.181,
            y: 51.4816,
        };
        for (crs, geometry) in CARDIFF {
            let Geometry::Point(point) = geometry_of(&crs_wkt(crs, geometry)) else {
                panic!("{crs} did not give a point");
            };
            // The fixtures are rounded to a centimetre, so a metre is the honest bound.
            let east = (point.x() - expected.x) * 111_320.0 * expected.y.to_radians().cos();
            let north = (point.y() - expected.y) * 110_574.0;
            let off = (east * east + north * north).sqrt();
            assert!(off < 1.0, "{crs} landed {off:.3} m from Cardiff Castle");
        }
    }

    /// The axis-order trap, at the level a user meets it.
    ///
    /// Both literals below say Cardiff. Read in the wrong order the EPSG:4326 one is at
    /// latitude 51 north of the equator or 3 degrees south of it, which is in the Atlantic
    /// off Gabon — a difference no distance function would flag as suspicious.
    #[test]
    fn epsg4326_puts_latitude_first_and_crs84_does_not() {
        let a = geometry_of(&crs_wkt(crate::crs::CRS84_URI, "POINT(-3.181 51.4816)"));
        let b = geometry_of(&crs_wkt(crate::crs::EPSG_4326_URI, "POINT(51.4816 -3.181)"));
        assert_eq!(a, b);
        let swapped = geometry_of(&crs_wkt(crate::crs::EPSG_4326_URI, "POINT(-3.181 51.4816)"));
        assert_ne!(a, swapped);
    }

    /// `geof:getSRID` answers what the literal declares, not what the engine converted it
    /// to. Wrapping it the way the other 43 are wrapped would make it answer CRS84 always,
    /// which is what `spargeo`'s does and is the reason it is replaced.
    #[test]
    fn get_srid_reports_the_declared_system() {
        for (crs, geometry) in CARDIFF {
            let answer = geof_get_srid(&[crs_wkt(crs, geometry)]).expect("has a system");
            let Term::Literal(literal) = &answer else {
                panic!("expected a literal");
            };
            assert_eq!(literal.value(), crs);
            assert_eq!(literal.datatype(), xsd::ANY_URI);
        }
    }

    /// An unprefixed literal is CRS84, which is what GeoSPARQL says it means.
    #[test]
    fn get_srid_calls_an_unprefixed_literal_crs84() {
        let answer = geof_get_srid(&[wkt("POINT(-3.181 51.4816)")]).expect("has a system");
        let Term::Literal(literal) = &answer else {
            panic!("expected a literal");
        };
        assert_eq!(literal.value(), crate::crs::CRS84_URI);
    }

    /// A well-formed CRS on malformed WKT is not a geometry, and `getSRID` should not be
    /// the one function that says otherwise.
    #[test]
    fn get_srid_refuses_a_literal_that_is_not_a_geometry() {
        assert!(geof_get_srid(&[crs_wkt(crate::crs::CRS84_URI, "POINT(nope)")]).is_none());
    }

    fn crs_iri(uri: &str) -> Term {
        Term::NamedNode(NamedNode::new_unchecked(uri.to_owned()))
    }

    /// `holos:transform` is the only function whose output is not CRS84, because it is the
    /// only one whose job is to leave it.
    #[test]
    fn transform_writes_the_target_system_into_the_literal() {
        let out = holos_transform(&[
            wkt("POINT(-3.181 51.4816)"),
            crs_iri(crate::crs::EPSG_27700_URI),
        ])
        .expect("transformed");
        let Term::Literal(literal) = &out else {
            panic!("expected a literal");
        };
        assert!(
            literal
                .value()
                .starts_with(&format!("<{}>", crate::crs::EPSG_27700_URI)),
            "{}",
            literal.value()
        );
        // And the numbers are the grid reference, not the degrees relabelled.
        let (_, geometry) = declared_crs(literal.value()).expect("declares a system");
        let point = <Geometry>::try_from_wkt_str(geometry).expect("parses");
        let Geometry::Point(point) = point else {
            panic!("expected a point");
        };
        assert!(
            (point.x() - 318_086.06).abs() < 0.1,
            "easting {}",
            point.x()
        );
        assert!(
            (point.y() - 176_511.05).abs() < 0.1,
            "northing {}",
            point.y()
        );
    }

    /// Out and back is the identity, to the millimetre the datum round trip closes to.
    #[test]
    fn transform_round_trips_through_every_system() {
        for (crs, _) in CARDIFF {
            let there = holos_transform(&[wkt("POINT(-3.181 51.4816)"), crs_iri(crs)])
                .expect("transformed");
            let Geometry::Point(back) = geometry_of(&there) else {
                panic!("expected a point");
            };
            // Two millimetres on the ground, the same bound the National Grid round trip
            // closes to in `crate::crs`. Serialising to WKT and back does not add to it.
            let east = (back.x() + 3.181) * 111_320.0 * 51.4816_f64.to_radians().cos();
            let north = (back.y() - 51.4816) * 110_574.0;
            let drift = (east * east + north * north).sqrt();
            assert!(
                drift < 0.002,
                "{crs}: came back {:.4} mm away",
                drift * 1000.0
            );
        }
    }

    /// The target may be an `xsd:anyURI` literal, because that is what `geof:getSRID`
    /// returns and one function's output should feed another without a cast.
    #[test]
    fn transform_accepts_the_target_as_a_uri_literal() {
        let target =
            Literal::new_typed_literal(crate::crs::EPSG_3857_URI.to_owned(), xsd::ANY_URI).into();
        assert!(holos_transform(&[wkt("POINT(0 0)"), target]).is_some());
    }

    /// A target this engine cannot reach comes back unbound rather than unchanged. A caller
    /// who asked for EPSG:2154 and silently received CRS84 would have no way to tell.
    #[test]
    fn transform_refuses_a_system_it_cannot_reach() {
        let target = crs_iri("http://www.opengis.net/def/crs/EPSG/0/2154");
        assert!(holos_transform(&[wkt("POINT(0 0)"), target]).is_none());
    }

    /// RFC 7946 fixed GeoJSON at CRS84 and removed the `crs` member, so there is no honest
    /// way to answer this. A document whose numbers are eastings and whose format says they
    /// are degrees is worse than no answer.
    #[test]
    fn transform_will_not_write_a_projected_system_as_geojson() {
        let gj = Literal::new_typed_literal(
            r#"{"type":"Point","coordinates":[-3.181,51.4816]}"#.to_owned(),
            GEO_JSON_LITERAL,
        )
        .into();
        assert!(holos_transform(&[gj, crs_iri(crate::crs::EPSG_27700_URI)]).is_none());
    }

    /// The whole point of the exercise: two operands in different systems, one answer.
    ///
    /// Cardiff Castle on the British National Grid against Cardiff Castle in degrees is
    /// zero metres apart. Before this, it was unbound.
    #[test]
    fn distance_works_across_two_reference_systems() {
        let grid = crs_wkt(crate::crs::EPSG_27700_URI, "POINT(318086.06 176511.05)");
        let degrees = wkt("POINT(-3.181 51.4816)");
        let metres = Term::NamedNode(NamedNode::new_unchecked(format!("{OGC_UOM_PREFIX}metre")));
        let out = geof_distance(&[grid, degrees, metres]).expect("a distance");
        let Term::Literal(literal) = &out else {
            panic!("expected a literal");
        };
        let apart: f64 = literal.value().parse().expect("a number");
        assert!(apart < 1.0, "{apart} m apart, which is not the same castle");
    }

    /// `spargeo`'s 43 gain the same reach through `crs_aware`, and this is the check that
    /// they do. `geof:envelope` is one of the 43 and is not reimplemented here, so it can
    /// only pass by going through the wrapper.
    #[test]
    fn crs_aware_extends_a_spargeo_function_to_the_other_systems() {
        use geo::algorithm::BoundingRect;
        let envelope = spargeo_function("envelope").expect("spargeo has envelope");
        let grid = crs_wkt(
            crate::crs::EPSG_27700_URI,
            "POLYGON((318000 176000, 318200 176000, 318200 176600, 318000 176600, 318000 176000))",
        );

        // Unwrapped, this is exactly the old behaviour: refused.
        assert!(envelope(std::slice::from_ref(&grid)).is_none());

        let out = crs_aware(envelope)(&[grid]).expect("wrapped, it answers");
        let rect = geometry_of(&out).bounding_rect().expect("has extent");
        assert!(
            (rect.min().x + 3.1824).abs() < 0.001 && (rect.min().y - 51.4770).abs() < 0.001,
            "envelope came back at {:?}",
            rect.min()
        );
    }

    /// The wrapper must leave everything else alone, including the arguments that are not
    /// geometries at all and the CRS84 literals that are already where they need to be.
    #[test]
    fn crs_aware_passes_through_what_needs_no_rewriting() {
        let args = [
            wkt("POINT(0 0)"),
            number(1.0),
            units("metre"),
            Term::NamedNode(NamedNode::new_unchecked("http://example.com/x")),
        ];
        assert!(to_crs84(&args).is_none());
        // And a literal neither side can read is left for `spargeo` to refuse, rather than
        // refused a step earlier where the error would be attributed to the wrong code.
        let unreadable = [crs_wkt(
            "http://www.opengis.net/def/crs/EPSG/0/2154",
            "POINT(0 0)",
        )];
        assert!(to_crs84(&unreadable).is_none());
    }

    /// Transformation is all-or-nothing. Half a reprojected polygon is a shape that exists
    /// nowhere, and returning it would be worse than returning nothing.
    #[test]
    fn a_geometry_with_one_untransformable_vertex_is_refused_whole() {
        // A latitude at the pole has no Web Mercator coordinate; the other vertex has one.
        let line = geo::LineString(vec![Coord { x: 0.0, y: 10.0 }, Coord { x: 0.0, y: 90.0 }]);
        let geom = Geometry::LineString(line);
        assert!(reproject(&geom, Crs::Crs84, Crs::WebMercator).is_none());
    }

    #[test]
    fn output_follows_the_input_serialisation() {
        let gj = Literal::new_typed_literal(
            r#"{"type":"Polygon","coordinates":[[[0,0],[0,1],[1,1],[1,0],[0,0]]]}"#.to_owned(),
            GEO_JSON_LITERAL,
        );
        let out = geof_boundary(&[gj.into()]).expect("boundary");
        let Term::Literal(literal) = &out else {
            panic!("expected a literal");
        };
        assert_eq!(literal.datatype(), GEO_JSON_LITERAL);
    }

    #[test]
    fn a_crs84_prefixed_literal_round_trips() {
        let prefixed = wkt("<http://www.opengis.net/def/crs/OGC/1.3/CRS84> POINT(0 0)");
        assert!(geof_buffer(&[prefixed, number(1.0), units("degree")]).is_some());
    }
}
