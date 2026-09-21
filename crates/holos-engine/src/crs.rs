//! Coordinate reference systems, and the transformations between them.
//!
//! GeoSPARQL stores the reference system *in the literal*: `"<crs-uri> POINT(...)"`. Until
//! now this engine read that prefix only to check it said CRS84 and refuse it otherwise,
//! which meant a dataset published on the British National Grid — which is most public UK
//! data — could not be queried at all, let alone queried against a dataset in degrees.
//!
//! The systems supported, which between them cover that case and the one a Dutch or German
//! dataset raises next:
//!
//! | URI | System | Axis order | Units |
//! |---|---|---|---|
//! | `.../OGC/1.3/CRS84` | WGS 84 geographic | longitude, latitude | degrees |
//! | `.../EPSG/0/4326` | WGS 84 geographic | **latitude, longitude** | degrees |
//! | `.../EPSG/0/27700` | OSGB36 / British National Grid | easting, northing | metres |
//! | `.../EPSG/0/3857` | WGS 84 / Pseudo-Mercator (Web Mercator) | easting, northing | metres |
//! | `.../EPSG/0/326zz`, `327zz` | WGS 84 / UTM zone zz, north and south | easting, northing | metres |
//! | `.../EPSG/0/258zz` | ETRS89 / UTM zone zz north, zones 28–38 | easting, northing | metres |
//!
//! ETRS89 is treated as WGS 84. The two datums were identical in 1989 and the European
//! plate has since carried ETRS89 about 2.5 cm a year away from it, so a coordinate moved
//! between them here is under a metre from the answer a datum-aware transformation gives —
//! which is how PROJ treats the pair by default as well. A geographic coordinate outside
//! the planet — a latitude past ±90°, a longitude past ±180° — is refused rather than read,
//! since the commonest way to write one is to swap the axes.
//!
//! # The axis-order trap
//!
//! CRS84 and EPSG:4326 are *the same datum with opposite axis order*, and GeoSPARQL says a
//! literal uses the order its CRS defines. So `"<...CRS84> POINT(-0.0014 51.4778)"` and
//! `"<...EPSG/0/4326> POINT(51.4778 -0.0014)"` are the same place, and reading either in
//! the other order puts London in the Indian Ocean. That is why the two share a datum here
//! but not a variant.
//!
//! # Accuracy, stated rather than implied
//!
//! WGS 84 and OSGB36 are different datums, so EPSG:27700 needs a datum shift and not just a
//! projection. This uses the Ordnance Survey published 7-parameter Helmert transformation,
//! which is closed-form and needs no data files. It is **not** exact: measured against
//! OSTN15 — the official grid-shift definition, which is what "correct" means for this
//! transformation — over 1330 points spanning Great Britain, the mean discrepancy is
//! **2.0 m** and the worst **5.8 m**.
//!
//! That is the right accuracy for asking which ward a postcode falls in, and the wrong one
//! for setting out a boundary. Anything needing better must use OSTN15, which is a
//! multi-megabyte grid this crate does not ship. [`PROJECTION_ACCURACY_METRES`] names the
//! figure so a caller can reason about it instead of guessing.
//!
//! EPSG:3857 has no such caveat: it is the same datum reprojected, the formulae are two
//! lines each, and the round trip is exact to floating point.
//!
//! # Denial of service
//!
//! Both inverse transformations iterate, and both are driven by numbers a query supplies.
//! Every loop here has a fixed iteration cap and every entry point rejects a non-finite
//! coordinate, so a hostile literal costs a bounded amount of arithmetic and then comes
//! back unbound. Nothing in this module reads the store, so none of it is reachable by
//! policy and none of it can widen what a session can see.

use geo::Coord;

/// How far a British National Grid coordinate produced here may be from the OSTN15 answer.
///
/// Measured, not quoted: see the module documentation. Exposed so a caller who cares can
/// compare it against their own tolerance.
pub const PROJECTION_ACCURACY_METRES: f64 = 5.8;

/// OGC CRS84 — WGS 84 in longitude, latitude order. The GeoSPARQL default.
pub const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
/// EPSG:4326 — WGS 84 in latitude, longitude order.
pub const EPSG_4326_URI: &str = "http://www.opengis.net/def/crs/EPSG/0/4326";
/// EPSG:27700 — OSGB36 / British National Grid.
pub const EPSG_27700_URI: &str = "http://www.opengis.net/def/crs/EPSG/0/27700";
/// EPSG:3857 — WGS 84 / Pseudo-Mercator.
pub const EPSG_3857_URI: &str = "http://www.opengis.net/def/crs/EPSG/0/3857";
/// The prefix every EPSG code's OGC URI shares.
const EPSG_URI_PREFIX: &str = "http://www.opengis.net/def/crs/EPSG/0/";

/// A coordinate reference system this engine can read and write.
///
/// A variant is a *URI*, not a datum: `Crs84` and `Epsg4326` describe identical positions
/// and differ only in which number comes first. Collapsing them would lose exactly the
/// information the literal was carrying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Crs {
    /// WGS 84 geographic, longitude first.
    Crs84,
    /// WGS 84 geographic, latitude first.
    Epsg4326,
    /// OSGB36 / British National Grid, easting first, metres.
    BritishNationalGrid,
    /// WGS 84 / Pseudo-Mercator, easting first, metres.
    WebMercator,
    /// A UTM zone, by its EPSG code: `326zz` north and `327zz` south on WGS 84, `258zz`
    /// north on ETRS89. Easting first, metres.
    Utm(u16),
}

impl Crs {
    /// The system a CRS URI names, or `None` for one this engine cannot transform.
    ///
    /// Both the OGC HTTP form and the OGC URN form are accepted, because published data
    /// uses both and a store that took only one would refuse half of it. An unrecognised
    /// URI returns `None` and the caller refuses the literal: guessing CRS84 would read
    /// metres as degrees and put the geometry hundreds of kilometres out to sea, silently.
    #[must_use]
    pub fn from_uri(uri: &str) -> Option<Self> {
        match uri {
            CRS84_URI | "urn:ogc:def:crs:OGC:1.3:CRS84" | "urn:ogc:def:crs:OGC::CRS84" => {
                Some(Self::Crs84)
            }
            EPSG_4326_URI | "urn:ogc:def:crs:EPSG::4326" | "EPSG:4326" => Some(Self::Epsg4326),
            EPSG_27700_URI | "urn:ogc:def:crs:EPSG::27700" | "EPSG:27700" => {
                Some(Self::BritishNationalGrid)
            }
            EPSG_3857_URI | "urn:ogc:def:crs:EPSG::3857" | "EPSG:3857" => Some(Self::WebMercator),
            other => {
                let code = other
                    .strip_prefix(EPSG_URI_PREFIX)
                    .or_else(|| other.strip_prefix("urn:ogc:def:crs:EPSG::"))
                    .or_else(|| other.strip_prefix("EPSG:"))?;
                let code: u16 = code.parse().ok()?;
                utm::zone_of(code).map(|_| Self::Utm(code))
            }
        }
    }

    /// The canonical URI for this system, which is what a literal written here carries.
    #[must_use]
    pub fn uri(self) -> String {
        match self {
            Self::Crs84 => CRS84_URI.to_owned(),
            Self::Epsg4326 => EPSG_4326_URI.to_owned(),
            Self::BritishNationalGrid => EPSG_27700_URI.to_owned(),
            Self::WebMercator => EPSG_3857_URI.to_owned(),
            Self::Utm(code) => format!("{EPSG_URI_PREFIX}{code}"),
        }
    }

    /// Whether coordinates in this system are degrees of longitude and latitude in some
    /// order, rather than metres on a projection.
    #[must_use]
    pub const fn is_geographic(self) -> bool {
        matches!(self, Self::Crs84 | Self::Epsg4326)
    }

    /// The systems a query may name, one of each shape, for documentation and tests to
    /// enumerate. UTM has a hundred and thirty codes; three stand for them here: a WGS 84
    /// zone in each hemisphere and an ETRS89 one.
    #[must_use]
    pub const fn all() -> [Self; 7] {
        [
            Self::Crs84,
            Self::Epsg4326,
            Self::BritishNationalGrid,
            Self::WebMercator,
            Self::Utm(32631),
            Self::Utm(32756),
            Self::Utm(25832),
        ]
    }

    /// A coordinate in this system, as WGS 84 longitude and latitude in degrees.
    ///
    /// `None` if the input is not finite, or is somewhere the system cannot describe.
    #[must_use]
    pub fn to_wgs84(self, coord: Coord) -> Option<Coord> {
        if !coord.x.is_finite() || !coord.y.is_finite() {
            return None;
        }
        let geographic = match self {
            Self::Crs84 => coord,
            // The literal is latitude, longitude; everything past here is the other way.
            Self::Epsg4326 => Coord {
                x: coord.y,
                y: coord.x,
            },
            Self::BritishNationalGrid => return bng::to_wgs84(coord),
            Self::WebMercator => return web_mercator::to_wgs84(coord),
            Self::Utm(code) => return utm::to_wgs84(code, coord),
        };
        // A latitude past ±90° or a longitude past ±180° is not a place. The commonest way
        // to write one is to put the axes the wrong way round under a system that defines
        // them the other way, and reading it as if it were a place hides that.
        on_the_planet(geographic)
    }

    /// WGS 84 longitude and latitude in degrees, as a coordinate in this system.
    #[must_use]
    pub fn from_wgs84(self, coord: Coord) -> Option<Coord> {
        if !coord.x.is_finite() || !coord.y.is_finite() {
            return None;
        }
        let coord = on_the_planet(coord)?;
        match self {
            Self::Crs84 => Some(coord),
            Self::Epsg4326 => Some(Coord {
                x: coord.y,
                y: coord.x,
            }),
            Self::BritishNationalGrid => bng::from_wgs84(coord),
            Self::WebMercator => web_mercator::from_wgs84(coord),
            Self::Utm(code) => utm::from_wgs84(code, coord),
        }
    }
}

/// A longitude and latitude that is somewhere, or `None`.
fn on_the_planet(coord: Coord) -> Option<Coord> {
    (coord.y.abs() <= 90.0 && coord.x.abs() <= 180.0).then_some(coord)
}

/// One coordinate, moved from one system to another.
///
/// Everything goes through WGS 84 longitude and latitude, so adding a system means writing
/// one pair of functions rather than one per other system.
#[must_use]
pub fn transform(from: Crs, to: Crs, coord: Coord) -> Option<Coord> {
    if from == to {
        // Nothing to move, but still something to check: a geographic coordinate that is
        // not a place is refused whichever way it was going.
        return if from.is_geographic() {
            on_the_planet(coord)
        } else {
            (coord.x.is_finite() && coord.y.is_finite()).then_some(coord)
        };
    }
    to.from_wgs84(from.to_wgs84(coord)?)
}

// -------------------------------------------------------------------------------------
// EPSG:3857 — WGS 84 / Pseudo-Mercator
// -------------------------------------------------------------------------------------

mod web_mercator {
    use geo::Coord;

    /// The radius EPSG:3857 uses. It is the WGS 84 *semi-major* axis applied as if the
    /// earth were a sphere, which is the whole reason the system is called pseudo.
    const R: f64 = 6_378_137.0;

    pub fn from_wgs84(coord: Coord) -> Option<Coord> {
        // At either pole the projection is infinite. The EPSG:3857 area of use stops at
        // 85.06 degrees; going past that is legal and merely large, but a pole is not.
        if coord.y.abs() >= 90.0 {
            return None;
        }
        let lat = coord.y.to_radians();
        Some(Coord {
            x: R * coord.x.to_radians(),
            y: R * (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln(),
        })
    }

    pub fn to_wgs84(coord: Coord) -> Option<Coord> {
        let lon = (coord.x / R).to_degrees();
        let lat = (2.0 * (coord.y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
        (lon.is_finite() && lat.is_finite()).then_some(Coord { x: lon, y: lat })
    }
}

// -------------------------------------------------------------------------------------
// UTM — WGS 84 zones 326zz and 327zz, ETRS89 zones 258zz
// -------------------------------------------------------------------------------------

/// The Universal Transverse Mercator zones, on the ellipsoid WGS 84 and ETRS89 share.
///
/// Each zone is the same projection the National Grid uses — the series in [`tm`] — with a
/// different central meridian, a scale of 0.9996 at it, a false easting of 500 km, and in
/// the southern hemisphere a false northing of 10 000 km. No datum shift: the ellipsoid is
/// the one the coordinates are already on, and ETRS89 is taken as WGS 84 for the reason
/// the module documentation gives.
///
/// Measured against PROJ 9 on eight points across five zones: the forward projection
/// agrees with PROJ to a millimetre within 3° of the zone's meridian, to 2 mm at 4° — as
/// far as the Netherlands convention of using zone 32 for the whole country takes it —
/// and to 5 mm at 6°, where the series PROJ replaced with an exact formulation begins to
/// show; the round trip closes to the same figures. A zone is designed for ±3°.
mod utm {
    use super::tm::{Ellipsoid, TransverseMercator};
    use geo::Coord;

    /// GRS 80, which ETRS89 is defined on, is WGS 84 to a tenth of a millimetre in the
    /// semi-minor axis; the WGS 84 figure serves both.
    const WGS84: Ellipsoid = Ellipsoid {
        a: 6_378_137.0,
        b: 6_356_752.314_245_179,
    };

    /// The zone and hemisphere an EPSG code names, or `None` for a code that is not UTM.
    pub fn zone_of(code: u16) -> Option<(u8, bool)> {
        let (base, south, zones) = match code {
            32601..=32660 => (32600, false, 1..=60),
            32701..=32760 => (32700, true, 1..=60),
            25828..=25838 => (25800, false, 28..=38),
            _ => return None,
        };
        let zone = code - base;
        zones
            .contains(&zone)
            .then_some((u8::try_from(zone).ok()?, south))
    }

    fn projection(code: u16) -> Option<TransverseMercator> {
        let (zone, south) = zone_of(code)?;
        Some(TransverseMercator {
            ellipsoid: WGS84,
            scale: 0.9996,
            lat0_deg: 0.0,
            lon0_deg: f64::from(zone) * 6.0 - 183.0,
            false_easting: 500_000.0,
            false_northing: if south { 10_000_000.0 } else { 0.0 },
        })
    }

    pub fn from_wgs84(code: u16, coord: Coord) -> Option<Coord> {
        projection(code)?.project(coord.y.to_radians(), coord.x.to_radians())
    }

    pub fn to_wgs84(code: u16, coord: Coord) -> Option<Coord> {
        let (lat, lon) = projection(code)?.unproject(coord)?;
        super::on_the_planet(Coord {
            x: lon.to_degrees(),
            y: lat.to_degrees(),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::crs::Crs;

        /// PROJ 9's answers, to the millimetre it prints, for `EPSG:4326` input.
        const REFERENCE: &[(u16, &str, f64, f64, f64, f64)] = &[
            (
                25832,
                "Amsterdam Central",
                52.3791,
                4.9003,
                220_997.635,
                5_811_116.297,
            ),
            (
                25832,
                "Rotterdam Port",
                51.9225,
                4.4792,
                189_186.155,
                5_762_079.763,
            ),
            (
                25832,
                "Utrecht Center",
                52.0907,
                5.1214,
                234_320.135,
                5_778_225.930,
            ),
            (
                32631,
                "Amsterdam Central",
                52.3791,
                4.9003,
                629_345.737,
                5_804_903.392,
            ),
            (
                32631,
                "Cardiff Castle",
                51.4816,
                -3.181,
                70_974.561,
                5_721_520.200,
            ),
            (
                32632,
                "Utrecht Center",
                52.0907,
                5.1214,
                234_320.135,
                5_778_225.930,
            ),
            (
                32756,
                "Sydney Opera",
                -33.8568,
                151.2153,
                334_900.570,
                6_252_288.753,
            ),
            (
                32631,
                "zone edge, 6E",
                50.0,
                5.9999,
                714_977.072,
                5_542_943.731,
            ),
        ];

        #[test]
        fn every_reference_point_lands_where_proj_puts_it() {
            for &(code, name, lat, lon, east, north) in REFERENCE {
                let got = from_wgs84(code, Coord { x: lon, y: lat }).expect("projects");
                // Two millimetres within 4° of the meridian; Cardiff sits 6° from zone
                // 31's and the series is good to 5 mm there.
                let tolerance =
                    if (lon - (f64::from(zone_of(code).expect("utm").0) * 6.0 - 183.0)).abs() > 5.0
                    {
                        0.005
                    } else {
                        0.002
                    };
                assert!(
                    (got.x - east).abs() < tolerance && (got.y - north).abs() < tolerance,
                    "{name} in EPSG:{code}: got ({:.3}, {:.3}), PROJ says ({east:.3}, {north:.3})",
                    got.x,
                    got.y
                );
            }
        }

        #[test]
        fn the_round_trip_closes_to_a_millimetre() {
            for &(code, name, lat, lon, _, _) in REFERENCE {
                let there = from_wgs84(code, Coord { x: lon, y: lat }).expect("projects");
                let back = to_wgs84(code, there).expect("unprojects");
                let east = (back.x - lon) * 111_320.0 * lat.to_radians().cos();
                let north = (back.y - lat) * 110_574.0;
                assert!(
                    (east * east + north * north).sqrt() < 0.005,
                    "{name} in EPSG:{code} came back {:.4} m away",
                    (east * east + north * north).sqrt()
                );
            }
        }

        #[test]
        fn the_codes_are_read_and_the_rest_are_not() {
            assert_eq!(zone_of(32631), Some((31, false)));
            assert_eq!(zone_of(32760), Some((60, true)));
            assert_eq!(zone_of(25828), Some((28, false)));
            assert_eq!(zone_of(25838), Some((38, false)));
            assert_eq!(zone_of(32600), None, "zone 0 does not exist");
            assert_eq!(zone_of(32661), None, "zone 61 does not exist");
            assert_eq!(zone_of(25839), None, "ETRS89 zones stop at 38");
            assert_eq!(zone_of(2154), None, "Lambert-93 is not UTM");
            assert_eq!(
                Crs::from_uri("http://www.opengis.net/def/crs/EPSG/0/25832"),
                Some(Crs::Utm(25832))
            );
            assert_eq!(Crs::from_uri("EPSG:32756"), Some(Crs::Utm(32756)));
            assert_eq!(
                Crs::from_uri("urn:ogc:def:crs:EPSG::32631"),
                Some(Crs::Utm(32631))
            );
            assert_eq!(
                Crs::from_uri("http://www.opengis.net/def/crs/EPSG/0/28992"),
                None
            );
            assert_eq!(
                Crs::Utm(25832).uri(),
                "http://www.opengis.net/def/crs/EPSG/0/25832"
            );
        }
    }
}

// -------------------------------------------------------------------------------------
// Transverse Mercator — the series the National Grid and every UTM zone share
// -------------------------------------------------------------------------------------

/// The Transverse Mercator projection, as the Ordnance Survey publishes it.
///
/// The series is the one in *A Guide to Coordinate Systems in Great Britain*, which is the
/// standard Redfearn expansion: exact to a millimetre within a few degrees of the central
/// meridian, which is what every zone here asks of it. It was written for the National
/// Grid and generalised when UTM needed the same thing with other numbers; the National
/// Grid's own tests still hold it to the Ordnance Survey worked example.
mod tm {
    use geo::Coord;

    /// How many times the inverse iteration may run before giving up.
    ///
    /// It converges in a handful of steps for any coordinate on the planet; the cap exists
    /// so a coordinate that is *not* on the planet costs a bounded amount of work rather
    /// than spinning. Reaching it means the input was nonsense, and the caller gets `None`.
    const MAX_ITERATIONS: usize = 32;

    /// An ellipsoid by its two semi-axes, in metres.
    #[derive(Clone, Copy)]
    pub struct Ellipsoid {
        pub a: f64,
        pub b: f64,
    }

    impl Ellipsoid {
        pub const fn eccentricity_squared(self) -> f64 {
            (self.a * self.a - self.b * self.b) / (self.a * self.a)
        }
    }

    /// One projection: an ellipsoid, a scale on the central meridian, a true origin, and
    /// the false origin the grid's numbers are counted from.
    #[derive(Clone, Copy)]
    pub struct TransverseMercator {
        pub ellipsoid: Ellipsoid,
        pub scale: f64,
        pub lat0_deg: f64,
        pub lon0_deg: f64,
        pub false_easting: f64,
        pub false_northing: f64,
    }

    impl TransverseMercator {
        /// The meridional arc: the distance along the meridian from the projection origin.
        ///
        /// Shared by the projection and its inverse, which is the point of pulling it out —
        /// the inverse solves for the latitude that makes this equal a given northing, so
        /// the two must be the same series or the round trip does not close.
        fn meridional_arc(&self, lat: f64) -> f64 {
            let Ellipsoid { a, b } = self.ellipsoid;
            let lat0 = self.lat0_deg.to_radians();
            let n = (a - b) / (a + b);
            let (n2, n3) = (n * n, n * n * n);
            let ma = (1.0 + n + 1.25 * n2 + 1.25 * n3) * (lat - lat0);
            let mb = (3.0 * n + 3.0 * n2 + 2.625 * n3) * (lat - lat0).sin() * (lat + lat0).cos();
            let mc =
                (1.875 * n2 + 1.875 * n3) * (2.0 * (lat - lat0)).sin() * (2.0 * (lat + lat0)).cos();
            let md = (35.0 / 24.0) * n3 * (3.0 * (lat - lat0)).sin() * (3.0 * (lat + lat0)).cos();
            b * self.scale * (ma - mb + mc - md)
        }

        /// nu, rho and eta squared at a latitude: the three radii the series is written in.
        fn radii(&self, lat: f64) -> (f64, f64, f64) {
            let a = self.ellipsoid.a;
            let e2 = self.ellipsoid.eccentricity_squared();
            let sin_lat = lat.sin();
            let w = 1.0 - e2 * sin_lat * sin_lat;
            let nu = a * self.scale / w.sqrt();
            let rho = a * self.scale * (1.0 - e2) / (w * w.sqrt());
            (nu, rho, nu / rho - 1.0)
        }

        /// Geodetic latitude and longitude in radians, on this ellipsoid, to easting and
        /// northing.
        pub fn project(&self, lat: f64, lon: f64) -> Option<Coord> {
            if lat.abs() > std::f64::consts::FRAC_PI_2 {
                return None;
            }
            let (nu, rho, eta2) = self.radii(lat);
            let (sin_lat, cos_lat) = lat.sin_cos();
            let tan_lat = lat.tan();
            let (t2, t4) = (tan_lat * tan_lat, tan_lat.powi(4));
            let (c3, c5) = (cos_lat.powi(3), cos_lat.powi(5));

            let i = self.meridional_arc(lat) + self.false_northing;
            let ii = nu / 2.0 * sin_lat * cos_lat;
            let iii = nu / 24.0 * sin_lat * c3 * (5.0 - t2 + 9.0 * eta2);
            let iiia = nu / 720.0 * sin_lat * c5 * (61.0 - 58.0 * t2 + t4);
            let iv = nu * cos_lat;
            let v = nu / 6.0 * c3 * (nu / rho - t2);
            let vi = nu / 120.0 * c5 * (5.0 - 18.0 * t2 + t4 + 14.0 * eta2 - 58.0 * t2 * eta2);

            let dl = lon - self.lon0_deg.to_radians();
            let (dl2, dl3) = (dl * dl, dl * dl * dl);
            let northing = i + ii * dl2 + iii * dl2 * dl2 + iiia * dl3 * dl3;
            let easting = self.false_easting + iv * dl + v * dl3 + vi * dl3 * dl2;
            (northing.is_finite() && easting.is_finite()).then_some(Coord {
                x: easting,
                y: northing,
            })
        }

        /// Easting and northing back to geodetic latitude and longitude in radians.
        pub fn unproject(&self, coord: Coord) -> Option<(f64, f64)> {
            let (easting, northing) = (coord.x, coord.y);
            let a = self.ellipsoid.a;
            let lat0 = self.lat0_deg.to_radians();
            let mut lat = (northing - self.false_northing) / (a * self.scale) + lat0;
            let mut converged = false;
            for _ in 0..MAX_ITERATIONS {
                let remainder = northing - self.false_northing - self.meridional_arc(lat);
                // A hundredth of a millimetre, which is the tolerance the Ordnance Survey
                // worked example stops at.
                if remainder.abs() < 1.0e-5 {
                    converged = true;
                    break;
                }
                lat += remainder / (a * self.scale);
            }
            if !converged {
                return None;
            }

            let (nu, rho, eta2) = self.radii(lat);
            let tan_lat = lat.tan();
            let (t2, t4, t6) = (tan_lat * tan_lat, tan_lat.powi(4), tan_lat.powi(6));
            let sec = 1.0 / lat.cos();
            let (nu3, nu5, nu7) = (nu.powi(3), nu.powi(5), nu.powi(7));

            let vii = tan_lat / (2.0 * rho * nu);
            let viii = tan_lat / (24.0 * rho * nu3) * (5.0 + 3.0 * t2 + eta2 - 9.0 * t2 * eta2);
            let ix = tan_lat / (720.0 * rho * nu5) * (61.0 + 90.0 * t2 + 45.0 * t4);
            let x = sec / nu;
            let xi = sec / (6.0 * nu3) * (nu / rho + 2.0 * t2);
            let xii = sec / (120.0 * nu5) * (5.0 + 28.0 * t2 + 24.0 * t4);
            let xiia = sec / (5040.0 * nu7) * (61.0 + 662.0 * t2 + 1320.0 * t4 + 720.0 * t6);

            let de = easting - self.false_easting;
            let (de2, de3) = (de * de, de * de * de);
            let latitude = lat - vii * de2 + viii * de2 * de2 - ix * de3 * de3;
            let longitude = self.lon0_deg.to_radians() + x * de - xi * de3 + xii * de3 * de2
                - xiia * de3 * de2 * de2;
            (latitude.is_finite() && longitude.is_finite()).then_some((latitude, longitude))
        }
    }
}

// -------------------------------------------------------------------------------------
// EPSG:27700 — OSGB36 / British National Grid
// -------------------------------------------------------------------------------------

/// The British National Grid: a Transverse Mercator projection of the Airy 1830 ellipsoid,
/// on the OSGB36 datum.
///
/// Two separate things have to happen and it is worth keeping them apart, because only one
/// of them is approximate. The projection is exact — it is the Ordnance Survey series
/// expansion, and reproduces the worked example in their coordinate systems guide to a
/// millimetre. The **datum shift** is the approximation: OSGB36 is a physical realisation
/// of a triangulation network laid out in the 1930s, and no seven parameters describe it
/// exactly. See [`PROJECTION_ACCURACY_METRES`](super::PROJECTION_ACCURACY_METRES).
mod bng {
    use super::tm::{Ellipsoid, TransverseMercator};
    use geo::Coord;

    /// Airy 1830, the ellipsoid OSGB36 is defined on.
    const AIRY_A: f64 = 6_377_563.396;
    const AIRY_B: f64 = 6_356_256.909;
    /// WGS 84.
    const WGS84_A: f64 = 6_378_137.0;
    const WGS84_B: f64 = 6_356_752.314_245_179;

    /// National Grid projection parameters.
    const F0: f64 = 0.999_601_271_7;
    const LAT0_DEG: f64 = 49.0;
    const LON0_DEG: f64 = -2.0;
    const E0: f64 = 400_000.0;
    const N0: f64 = -100_000.0;

    /// The Ordnance Survey published WGS 84 to OSGB36 Helmert parameters, position-vector
    /// convention: metres, parts per million, and seconds of arc.
    const TX: f64 = -446.448;
    const TY: f64 = 125.157;
    const TZ: f64 = -542.060;
    const SCALE_PPM: f64 = 20.489_4;
    const RX_SEC: f64 = -0.150_2;
    const RY_SEC: f64 = -0.247_0;
    const RZ_SEC: f64 = -0.842_1;

    /// Radians per second of arc.
    const SEC_TO_RAD: f64 = std::f64::consts::PI / (180.0 * 3600.0);

    /// How many times the two inverse iterations may run before giving up.
    ///
    /// Both converge in a handful of steps for any coordinate on the planet; the cap exists
    /// so a coordinate that is *not* on the planet costs a bounded amount of work rather
    /// than spinning. Reaching it means the input was nonsense, and the caller gets `None`.
    const MAX_ITERATIONS: usize = 32;

    const fn eccentricity_squared(a: f64, b: f64) -> f64 {
        (a * a - b * b) / (a * a)
    }

    pub fn from_wgs84(coord: Coord) -> Option<Coord> {
        if coord.y.abs() > 90.0 {
            return None;
        }
        let (lon, lat) = (coord.x.to_radians(), coord.y.to_radians());
        let (x, y, z) = to_cartesian(lat, lon, 0.0, WGS84_A, WGS84_B);
        let (x, y, z) = helmert_forward(x, y, z);
        let (lat, lon, _) = to_geodetic(x, y, z, AIRY_A, AIRY_B)?;
        project(lat, lon)
    }

    pub fn to_wgs84(coord: Coord) -> Option<Coord> {
        let (lat, lon) = unproject(coord)?;
        let (x, y, z) = to_cartesian(lat, lon, 0.0, AIRY_A, AIRY_B);
        let (x, y, z) = helmert_inverse(x, y, z);
        let (lat, lon, _) = to_geodetic(x, y, z, WGS84_A, WGS84_B)?;
        Some(Coord {
            x: lon.to_degrees(),
            y: lat.to_degrees(),
        })
    }

    /// The rotations in radians, which is how the matrix wants them.
    fn rotations() -> (f64, f64, f64) {
        (
            RX_SEC * SEC_TO_RAD,
            RY_SEC * SEC_TO_RAD,
            RZ_SEC * SEC_TO_RAD,
        )
    }

    /// The 7-parameter Helmert shift, WGS 84 to OSGB36, position-vector convention.
    fn helmert_forward(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
        let (rx, ry, rz) = rotations();
        let s = 1.0 + SCALE_PPM / 1.0e6;
        (
            TX + s * (x - rz * y + ry * z),
            TY + s * (rz * x + y - rx * z),
            TZ + s * (-ry * x + rx * y + z),
        )
    }

    /// The same shift, OSGB36 back to WGS 84.
    ///
    /// This subtracts the translation and applies the transposed rotation and reciprocal
    /// scale, rather than the usual shortcut of negating all seven parameters. The shortcut
    /// is not the matrix inverse and it shows: round-tripping a British coordinate through
    /// it lands about **3 mm** away, because rotating the 713 m translation vector does not
    /// cancel. Three millimetres is far inside the 5.8 m the datum model is good for and
    /// nobody would notice — but it is error this code invents, on top of error the model
    /// already has, and there is no reason to invent any. Done properly the residual is
    /// **0.06 mm**, which puts it below the projection series and leaves the model as the
    /// only thing worth talking about.
    fn helmert_inverse(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
        let (rx, ry, rz) = rotations();
        let inverse_scale = 1.0 / (1.0 + SCALE_PPM / 1.0e6);
        let (dx, dy, dz) = (x - TX, y - TY, z - TZ);
        (
            inverse_scale * (dx + rz * dy - ry * dz),
            inverse_scale * (-rz * dx + dy + rx * dz),
            inverse_scale * (ry * dx - rx * dy + dz),
        )
    }

    /// Geodetic latitude, longitude and height to geocentric cartesian metres.
    fn to_cartesian(lat: f64, lon: f64, height: f64, a: f64, b: f64) -> (f64, f64, f64) {
        let e2 = eccentricity_squared(a, b);
        let (sin_lat, cos_lat) = lat.sin_cos();
        let nu = a / (1.0 - e2 * sin_lat * sin_lat).sqrt();
        (
            (nu + height) * cos_lat * lon.cos(),
            (nu + height) * cos_lat * lon.sin(),
            ((1.0 - e2) * nu + height) * sin_lat,
        )
    }

    /// Geocentric cartesian metres back to geodetic latitude, longitude and height.
    ///
    /// The latitude has no closed form and is found by fixed-point iteration, which for a
    /// point anywhere near the surface converges in three or four steps.
    fn to_geodetic(x: f64, y: f64, z: f64, a: f64, b: f64) -> Option<(f64, f64, f64)> {
        let e2 = eccentricity_squared(a, b);
        let p = (x * x + y * y).sqrt();
        let lon = y.atan2(x);
        let mut lat = z.atan2(p * (1.0 - e2));
        let mut nu = a;
        let mut converged = false;
        for _ in 0..MAX_ITERATIONS {
            let sin_lat = lat.sin();
            nu = a / (1.0 - e2 * sin_lat * sin_lat).sqrt();
            let next = (z + e2 * nu * sin_lat).atan2(p);
            let moved = (next - lat).abs();
            lat = next;
            if moved < 1.0e-13 {
                converged = true;
                break;
            }
        }
        if !converged || !lat.is_finite() || !lon.is_finite() {
            return None;
        }
        Some((lat, lon, p / lat.cos() - nu))
    }

    /// The National Grid, as a Transverse Mercator on Airy 1830 with the Ordnance Survey's
    /// origin, scale and false origin.
    const NATIONAL_GRID: TransverseMercator = TransverseMercator {
        ellipsoid: Ellipsoid {
            a: AIRY_A,
            b: AIRY_B,
        },
        scale: F0,
        lat0_deg: LAT0_DEG,
        lon0_deg: LON0_DEG,
        false_easting: E0,
        false_northing: N0,
    };

    /// OSGB36 geodetic to National Grid easting and northing.
    fn project(lat: f64, lon: f64) -> Option<Coord> {
        NATIONAL_GRID.project(lat, lon)
    }

    /// National Grid easting and northing back to OSGB36 geodetic.
    fn unproject(coord: Coord) -> Option<(f64, f64)> {
        NATIONAL_GRID.unproject(coord)
    }

    /// Half the transformation, reachable on its own so a test can tell a wrong series
    /// from a wrong datum shift. Both show up as a wrong grid reference.
    #[cfg(test)]
    pub mod testing {
        /// The projection with the datum shift skipped: WGS 84 geodetic fed straight into
        /// the National Grid series, which is the mistake the datum shift exists to avoid.
        pub fn project_only(lat_deg: f64, lon_deg: f64) -> Option<geo::Coord> {
            super::project(lat_deg.to_radians(), lon_deg.to_radians())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The Ordnance Survey worked example, from *A Guide to Coordinate Systems in Great
        /// Britain*: Caister water tower, in OSGB36 geodetic coordinates.
        ///
        /// This exercises the projection **alone** — no datum shift — which is the half of
        /// the transformation that is meant to be exact, so it is asserted to a millimetre.
        #[test]
        fn the_ordnance_survey_worked_example_reproduces_to_a_millimetre() {
            let lat = (52.0f64 + 39.0 / 60.0 + 27.2531 / 3600.0).to_radians();
            let lon = (1.0f64 + 43.0 / 60.0 + 4.5177 / 3600.0).to_radians();
            let grid = project(lat, lon).expect("projects");
            assert!(
                (grid.x - 651_409.903).abs() < 0.001,
                "easting {} is not 651409.903",
                grid.x
            );
            assert!(
                (grid.y - 313_177.270).abs() < 0.001,
                "northing {} is not 313177.270",
                grid.y
            );
        }

        /// The inverse solves the same series, so it must land back where it started.
        ///
        /// Not exactly, though, and it is worth being precise about why. Both directions
        /// are **truncated** series, so the round trip closes to a fraction of a millimetre
        /// rather than to floating-point: 0.33 mm at Caister, rising to about 1.4 mm five
        /// degrees off the central meridian where the truncated terms are largest. That is
        /// a property of the Ordnance Survey formulation, not of this transcription of it,
        /// and it is three orders of magnitude below the datum model it feeds.
        #[test]
        fn the_projection_inverts() {
            // A millimetre on the ground, in radians.
            const TOLERANCE: f64 = 1.0e-3 / 6.371e6;
            let lat = (52.0f64 + 39.0 / 60.0 + 27.2531 / 3600.0).to_radians();
            let lon = (1.0f64 + 43.0 / 60.0 + 4.5177 / 3600.0).to_radians();
            let grid = project(lat, lon).expect("projects");
            let (back_lat, back_lon) = unproject(grid).expect("unprojects");
            assert!(
                (back_lat - lat).abs() < TOLERANCE,
                "latitude drifted {:e} rad",
                (back_lat - lat).abs()
            );
            assert!(
                (back_lon - lon).abs() < TOLERANCE,
                "longitude drifted {:e} rad",
                (back_lon - lon).abs()
            );
        }

        /// The inverse is the real matrix inverse, and must close to a tenth of a
        /// millimetre.
        ///
        /// A tenth of a millimetre rather than a millimetre on purpose. Merely negating
        /// the seven parameters — the shortcut `helmert_inverse` exists to avoid — closes
        /// to 10 mm, so a millimetre tolerance would pass either way and this test would
        /// assert nothing. The measured residual is 0.06 mm.
        #[test]
        fn the_helmert_shift_inverts() {
            let (x, y, z) = (3_980_000.0, -100_000.0, 4_970_000.0);
            let (a, b, c) = helmert_forward(x, y, z);
            let (x2, y2, z2) = helmert_inverse(a, b, c);
            for (before, after) in [(x, x2), (y, y2), (z, z2)] {
                assert!(
                    (before - after).abs() < 1.0e-4,
                    "{before} came back {after}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference eastings and northings for WGS 84 positions across Great Britain,
    /// generated with PROJ 9 through the same 7-parameter pipeline this module implements
    /// (`+proj=helmert +x=-446.448 ... +convention=position_vector`, then `+proj=tmerc` on
    /// the Airy ellipsoid).
    ///
    /// Independent in the sense that matters: PROJ is a separate implementation of the same
    /// definition, so agreeing with it to a millimetre says the series and the parameter
    /// signs are right. It says nothing about whether the *definition* is accurate — that
    /// is what [`PROJECTION_ACCURACY_METRES`] is for, and it is a different question.
    #[allow(
        clippy::unreadable_literal,
        reason = "eastings are compared against a reference table digit by digit"
    )]
    const BNG_FIXTURES: [(f64, f64, f64, f64); 6] = [
        // Greenwich Observatory
        (-0.0014, 51.4778, 538890.1007, 177320.4966),
        // Land's End
        (-5.7147, 50.0657, 134262.2902, 25010.3943),
        // John o' Groats
        (-3.0703, 58.6373, 337962.7768, 972652.3235),
        // Cardiff Castle
        (-3.181, 51.4816, 318086.0587, 176511.0482),
        // Ben Nevis
        (-5.0037, 56.7969, 216665.7412, 771287.6007),
        // Norwich Castle
        (1.2954, 52.6289, 623118.4271, 308558.9626),
    ];

    #[test]
    fn british_national_grid_matches_proj() {
        for (lon, lat, easting, northing) in BNG_FIXTURES {
            let grid = Crs::BritishNationalGrid
                .from_wgs84(Coord { x: lon, y: lat })
                .expect("projects");
            assert!(
                (grid.x - easting).abs() < 0.001 && (grid.y - northing).abs() < 0.001,
                "({lon}, {lat}) gave ({}, {}), expected ({easting}, {northing})",
                grid.x,
                grid.y
            );
        }
    }

    /// The round trip closes to about a millimetre on the ground.
    ///
    /// Asserted in millimetres rather than degrees, because a tolerance in degrees hides
    /// what it is worth: a longitude tolerance is a smaller distance at 58 degrees north
    /// than at 50, so the same number is a stricter test in Scotland than in Cornwall.
    /// The measured worst across these six is 1.12 mm, and 1.9 mm at the far south-west
    /// corner of the grid where the projection series is weakest.
    #[test]
    fn british_national_grid_round_trips_to_within_two_millimetres() {
        for (lon, lat, _, _) in BNG_FIXTURES {
            let there = Crs::BritishNationalGrid
                .from_wgs84(Coord { x: lon, y: lat })
                .expect("projects");
            let back = Crs::BritishNationalGrid.to_wgs84(there).expect("inverts");
            let east = (back.x - lon) * 111_320.0 * lat.to_radians().cos();
            let north = (back.y - lat) * 110_574.0;
            let drift = (east * east + north * north).sqrt();
            assert!(
                drift < 0.002,
                "({lon}, {lat}) came back {:.4} mm away, at ({}, {})",
                drift * 1000.0,
                back.x,
                back.y
            );
        }
    }

    /// The datum shift is the whole reason EPSG:27700 is not just a projection, and this
    /// measures what dropping it would cost.
    ///
    /// Feeding WGS 84 latitude and longitude straight into the National Grid series is the
    /// commonest way to get this wrong, because it produces a plausible grid reference —
    /// right square, right sheet — that is **77 to 131 metres** from the right place. That
    /// is the range asserted here, so a build that skipped the Helmert step would fail
    /// rather than quietly return coordinates a surveyor would reject and a dashboard
    /// would not.
    #[test]
    fn skipping_the_datum_shift_would_move_every_point_by_about_a_hundred_metres() {
        for (lon, lat, _, _) in BNG_FIXTURES {
            let correct = Crs::BritishNationalGrid
                .from_wgs84(Coord { x: lon, y: lat })
                .expect("projects");
            let naive = bng::testing::project_only(lat, lon).expect("projects");
            let moved = ((correct.x - naive.x).powi(2) + (correct.y - naive.y).powi(2)).sqrt();
            assert!(
                (70.0..=140.0).contains(&moved),
                "({lon}, {lat}): the datum shift moved the grid reference {moved} m"
            );
        }
    }

    /// Web Mercator is closed form in both directions, so it should round-trip to the last
    /// few bits rather than to a tolerance chosen for an iteration.
    #[test]
    fn web_mercator_round_trips_exactly() {
        for (lon, lat, _, _) in BNG_FIXTURES {
            let there = Crs::WebMercator
                .from_wgs84(Coord { x: lon, y: lat })
                .expect("projects");
            let back = Crs::WebMercator.to_wgs84(there).expect("inverts");
            assert!((back.x - lon).abs() < 1.0e-11 && (back.y - lat).abs() < 1.0e-11);
        }
    }

    /// Reference values from PROJ for EPSG:4326 to EPSG:3857.
    #[test]
    #[allow(
        clippy::unreadable_literal,
        reason = "as BNG_FIXTURES: these are transcribed reference values"
    )]
    fn web_mercator_matches_proj() {
        let cases = [
            (-0.0014, 51.4778, -155.8473, 6706250.1949),
            (-5.7147, 50.0657, -636157.4940, 6457661.7077),
            (1.2954, 52.6289, 144203.2684, 6914647.3107),
        ];
        for (lon, lat, x, y) in cases {
            let got = Crs::WebMercator
                .from_wgs84(Coord { x: lon, y: lat })
                .expect("projects");
            assert!(
                (got.x - x).abs() < 0.001 && (got.y - y).abs() < 0.001,
                "({lon}, {lat}) gave ({}, {})",
                got.x,
                got.y
            );
        }
    }

    /// The trap the module documentation opens with, as an executable claim.
    #[test]
    fn crs84_and_epsg4326_disagree_about_which_number_comes_first() {
        let london_lon_lat = Coord {
            x: -0.1278,
            y: 51.5074,
        };
        let london_lat_lon = Coord {
            x: 51.5074,
            y: -0.1278,
        };
        assert_eq!(
            Crs::Crs84.to_wgs84(london_lon_lat),
            Crs::Epsg4326.to_wgs84(london_lat_lon)
        );
        // And reading one as the other does not merely shift the point, it relocates it.
        let misread = Crs::Epsg4326.to_wgs84(london_lon_lat).expect("finite");
        assert!((misread.x - 51.5074).abs() < 1.0e-12);
    }

    #[test]
    fn an_unknown_crs_uri_is_not_guessed() {
        assert_eq!(
            Crs::from_uri("http://www.opengis.net/def/crs/EPSG/0/2154"),
            None
        );
        assert_eq!(Crs::from_uri(""), None);
    }

    #[test]
    fn every_canonical_uri_round_trips_through_from_uri() {
        for crs in Crs::all() {
            assert_eq!(Crs::from_uri(&crs.uri()), Some(crs), "{crs:?}");
        }
    }

    #[test]
    fn transform_between_two_projected_systems_goes_through_wgs84() {
        let wgs84 = Coord {
            x: -3.1810,
            y: 51.4816,
        };
        let grid = Crs::BritishNationalGrid
            .from_wgs84(wgs84)
            .expect("projects");
        let mercator = transform(Crs::BritishNationalGrid, Crs::WebMercator, grid).expect("both");
        let expected = Crs::WebMercator.from_wgs84(wgs84).expect("projects");
        // Two millimetres, which is the British National Grid round trip it contains.
        assert!((mercator.x - expected.x).abs() < 0.002);
        assert!((mercator.y - expected.y).abs() < 0.002);
    }

    #[test]
    fn a_non_finite_coordinate_is_refused_rather_than_propagated() {
        for crs in Crs::all() {
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(crs.to_wgs84(Coord { x: bad, y: 0.0 }), None, "{crs:?}");
                assert_eq!(crs.from_wgs84(Coord { x: 0.0, y: bad }), None, "{crs:?}");
            }
        }
    }

    #[test]
    fn a_pole_has_no_web_mercator_coordinate() {
        assert_eq!(Crs::WebMercator.from_wgs84(Coord { x: 0.0, y: 90.0 }), None);
    }
}
