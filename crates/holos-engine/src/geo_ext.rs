//! `geof:buffer` and `geof:boundary`.
//!
//! `spargeo` supplies 43 GeoSPARQL functions, but not these two — a fact this codebase
//! found the hard way, having claimed them in `DESIGN.md` §17 before probing for them.
//! They are the two most commonly reached-for constructive operators after the topological
//! relations, so they are implemented here and registered alongside the rest.
//!
//! Everything about the literal handling matches `spargeo`'s conventions deliberately, so
//! the two sets compose: CRS84 only, `geo:wktLiteral` and `geo:geoJSONLiteral` accepted on
//! input, output in whichever of the two the arguments used, and the same OGC unit IRIs.
//! A function that round-tripped literals differently from its neighbours would be worse
//! than one that did not exist.

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
/// The only reference system either `spargeo` or this module accepts.
const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
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

/// The functions this module adds to, or replaces in, `spargeo`'s 43.
pub const EXTRA_GEOSPARQL_FUNCTIONS: [(NamedNodeRef<'static>, fn(&[Term]) -> Option<Term>); 7] = [
    (BUFFER, geof_buffer),
    (BOUNDARY, geof_boundary),
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
fn set_operation(local: &str, args: &[Term]) -> Option<Term> {
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

/// A WKT literal may carry a leading `<crs-uri>`. Only CRS84 is accepted.
fn parse_wkt_literal(value: &str) -> Option<Geometry> {
    let mut value = value.trim_start();
    if let Some(rest) = value.strip_prefix('<') {
        let (system, rest) = rest.split_once('>').unwrap_or((rest, ""));
        if system != CRS84_URI {
            return None;
        }
        value = rest.trim_start();
    }
    Geometry::try_from_wkt_str(value).ok()
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

    #[test]
    fn a_non_crs84_literal_is_refused() {
        let odd = Literal::new_typed_literal(
            "<http://www.opengis.net/def/crs/EPSG/0/27700> POINT(0 0)".to_owned(),
            WKT_LITERAL,
        );
        assert!(geof_boundary(&[odd.into()]).is_none());
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
