// Runs the console's WKT parser against known geometries, with Leaflet stubbed, so the
// coordinate handling is checked by execution rather than by reading it.
//
// Driven by `map_plugin.rs`, which renders the console page and passes its path as argv[2].
// Run it directly with:
//
//     node crates/holos-server/tests/map_plugin.js <a-saved-console-page.html>
//
// The parser is lifted out of the page rather than imported, because the plugin is a
// self-contained IIFE served inside the HTML and has no module boundary to import from.
const fs = require('fs');

const page = fs.readFileSync(process.argv[2], 'utf8');
const start = page.indexOf('const WKT_DT =');
const end = page.indexOf('function isGeometry');
if (start < 0 || end < 0) { console.error('could not locate the parser in the page'); process.exit(2); }
const source = page.slice(start, end);

// Leaflet stub: record the shape and the coordinates it was handed.
const L = {
  circleMarker: (c) => ({ kind: 'point', coords: c }),
  polyline: (c) => ({ kind: 'line', coords: c }),
  polygon: (c) => ({ kind: 'polygon', coords: c }),
  geoJSON: (o) => ({ kind: 'geojson', coords: o }),
};

const fn = new Function('L', source + '; return { wktToLayers, geoJsonToLayers };');
const { wktToLayers, geoJsonToLayers } = fn(L);

let failures = 0;
function check(name, actual, expected) {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a === e) { console.log('  pass  ' + name); }
  else { console.log('  FAIL  ' + name + '\n        got      ' + a + '\n        expected ' + e); failures++; }
}

// Edinburgh is 55.9533 N, 3.1883 W. WKT says (lon lat); Leaflet wants [lat, lng].
check('POINT reverses to [lat, lng]',
  wktToLayers('POINT(-3.1883 55.9533)').layers.map(l => [l.kind, l.coords]),
  [['point', [55.9533, -3.1883]]]);

check('a CRS84 prefix is stripped',
  wktToLayers('<http://www.opengis.net/def/crs/OGC/1.3/CRS84> POINT(-4.2518 55.8642)').layers.map(l => [l.kind, l.coords]),
  [['point', [55.8642, -4.2518]]]);

// A projected CRS is not lon/lat degrees: drawing it would put the shape somewhere wrong.
check('a non-CRS84 CRS is refused rather than misplaced',
  wktToLayers('<http://www.opengis.net/def/crs/EPSG/0/27700> POINT(325000 674000)'),
  { layers: [], unsupported: 1 });

check('LINESTRING',
  wktToLayers('LINESTRING(-3.1 55.9, -4.2 55.8)').layers.map(l => [l.kind, l.coords]),
  [['line', [[55.9, -3.1], [55.8, -4.2]]]]);

check('POLYGON keeps its ring nesting',
  wktToLayers('POLYGON((-4.3 55.8, -3.1 55.8, -3.1 56.0, -4.3 55.8))').layers.map(l => [l.kind, l.coords]),
  [['polygon', [[[55.8, -4.3], [55.8, -3.1], [56.0, -3.1], [55.8, -4.3]]]]]);

check('POLYGON with a hole keeps both rings',
  wktToLayers('POLYGON((0 0, 4 0, 4 4, 0 0),(1 1, 2 1, 2 2, 1 1))').layers[0].coords.length, 2);

check('MULTIPOINT, parenthesised members',
  wktToLayers('MULTIPOINT((1 2),(3 4))').layers.map(l => l.coords),
  [[2, 1], [4, 3]]);

check('MULTIPOINT, bare members',
  wktToLayers('MULTIPOINT(1 2, 3 4)').layers.map(l => l.coords),
  [[2, 1], [4, 3]]);

check('MULTILINESTRING',
  wktToLayers('MULTILINESTRING((0 0, 1 1),(2 2, 3 3))').layers[0].coords,
  [[[0, 0], [1, 1]], [[2, 2], [3, 3]]]);

check('MULTIPOLYGON nests polygon > ring > point',
  wktToLayers('MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)),((2 2, 3 2, 3 3, 2 2)))').layers[0].coords.length, 2);

check('GEOMETRYCOLLECTION flattens its members',
  wktToLayers('GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))').layers.map(l => l.kind),
  ['point', 'line']);

check('a Z suffix is tolerated',
  wktToLayers('POINT Z(-3.1 55.9 120)').layers.length, 1);

check('lowercase keywords are accepted',
  wktToLayers('point(-3.1 55.9)').layers.map(l => l.coords), [[55.9, -3.1]]);

check('an unknown geometry type is counted, not thrown',
  wktToLayers('TRIANGLE((0 0, 1 0, 1 1, 0 0))'), { layers: [], unsupported: 1 });

check('rubbish is counted, not thrown',
  wktToLayers('not wkt at all'), { layers: [], unsupported: 1 });

check('GeoJSON parses',
  geoJsonToLayers('{"type":"Point","coordinates":[-3.1,55.9]}').layers.length, 1);

check('malformed GeoJSON is counted, not thrown',
  geoJsonToLayers('{oops'), { layers: [], unsupported: 1 });

console.log(failures ? '\n' + failures + ' failure(s)' : '\nall parser checks passed');
process.exit(failures ? 1 : 0);
