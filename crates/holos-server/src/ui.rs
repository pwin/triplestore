//! The YASGUI console.
//!
//! Served at `/`, pointed at this server's own `/query` endpoint. YASGUI itself is loaded
//! from a CDN rather than adapted: it is a large JavaScript bundle with its own licence
//! and release cadence, and checking a minified copy into an RDF engine's source tree
//! makes that tree harder to audit, not easier.
//!
//! The consequence is worth stating rather than hiding: **the console needs network access
//! to a CDN.** The SPARQL endpoints do not — they are the actual interface, and they work
//! offline. `--no-ui` turns the console off entirely for a deployment that should not be
//! reaching out to anything.
//!
//! # The map plugin
//!
//! YASR ships four result plugins — table, response, boolean, error. The map and chart
//! views on Triply\'s hosted YASGUI are not among them: they are not MIT licensed and
//! cannot be included programmatically, and neither the upstream repository nor the
//! Zazuko fork contains them.
//!
//! So this adds one. [`MAP_PLUGIN`] is a YASR plugin written against the documented
//! `Yasr.registerPlugin` interface, drawing `geo:wktLiteral` and `geo:geoJSONLiteral`
//! bindings on a Leaflet map. It shares nothing with Triply\'s implementation beyond the
//! plugin interface itself.
//!
//! Two details that are easy to get wrong and produce a map that looks plausible:
//!
//! * **Coordinate order.** WKT is `(x y)`, and under CRS84 that is `(longitude latitude)`.
//!   Leaflet takes `[latitude, longitude]`. Every pair has to be reversed, and a map with
//!   them the wrong way round still renders — somewhere in the sea off Somalia, usually.
//! * **The CRS prefix.** A GeoSPARQL WKT literal may begin with a CRS URI in angle
//!   brackets. It has to be stripped before parsing, and if it names anything other than
//!   CRS84 the coordinates are not lon/lat degrees and the geometry is skipped rather than
//!   drawn in the wrong place.

/// Media types of the two geometry serialisations the map plugin understands.
const WKT_DATATYPE: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const GEOJSON_DATATYPE: &str = "http://www.opengis.net/ont/geosparql#geoJSONLiteral";

/// A YASR result plugin that draws GeoSPARQL geometries on a Leaflet map.
///
/// Written against the plugin interface YASR documents: a class taking the `yasr` instance,
/// with `priority`, `label`, `canHandleResults()`, `draw()` and `getIcon()`. Registered
/// through `Yasr.registerPlugin`.
///
/// Geometry support is the OGC simple-feature set: `POINT`, `LINESTRING`, `POLYGON`,
/// their `MULTI` forms and `GEOMETRYCOLLECTION`, plus any GeoJSON that Leaflet accepts.
/// Anything else is counted and reported rather than silently dropped, because a map that
/// quietly omits rows is worse than one that says it did.
const MAP_PLUGIN: &str = r#"
(function () {
  const WKT_DT = "__WKT_DATATYPE__";
  const GEOJSON_DT = "__GEOJSON_DATATYPE__";
  // GeoSPARQL's default CRS. Anything else is not lon/lat degrees, so it is not drawn.
  const CRS84 = /^<http:\/\/www\.opengis\.net\/def\/crs\/OGC\/1\.3\/CRS84>\s*/;
  const ANY_CRS = /^<[^>]*>\s*/;

  // --- WKT ---------------------------------------------------------------
  // A coordinate list: "1 2, 3 4" -> [[1,2],[3,4]]. WKT is (x y) = (lon lat) under CRS84,
  // and Leaflet wants [lat, lng], so each pair is reversed here and nowhere else.
  function coords(text) {
    const out = [];
    for (const pair of text.split(",")) {
      const n = pair.trim().split(/\s+/).map(Number);
      if (n.length < 2 || !isFinite(n[0]) || !isFinite(n[1])) return null;
      out.push([n[1], n[0]]);
    }
    return out.length ? out : null;
  }

  // Splits "(...), (...)" at depth 1 into its parenthesised groups.
  function groups(body) {
    const out = [];
    let depth = 0, start = -1;
    for (let i = 0; i < body.length; i++) {
      const c = body[i];
      if (c === "(") { if (depth === 0) start = i + 1; depth++; }
      else if (c === ")") { depth--; if (depth === 0) out.push(body.slice(start, i)); }
    }
    return out;
  }

  function wktToLayers(literal) {
    let text = literal.trim();
    if (/^</.test(text)) {
      if (!CRS84.test(text)) return { layers: [], unsupported: 1 };
      text = text.replace(ANY_CRS, "");
    }
    const m = /^([A-Za-z]+)\s*(?:Z|M|ZM)?\s*\(([\s\S]*)\)\s*$/.exec(text.trim());
    if (!m) return { layers: [], unsupported: 1 };
    const kind = m[1].toUpperCase();
    const body = m[2];

    switch (kind) {
      case "POINT": {
        const c = coords(body);
        return c ? { layers: [L.circleMarker(c[0], { radius: 6 })], unsupported: 0 }
                 : { layers: [], unsupported: 1 };
      }
      case "LINESTRING": {
        const c = coords(body);
        return c ? { layers: [L.polyline(c)], unsupported: 0 }
                 : { layers: [], unsupported: 1 };
      }
      case "POLYGON": {
        // First ring is the exterior, the rest are holes — which is exactly the array
        // shape Leaflet expects, so they pass through together.
        const rings = groups(body).map(coords);
        return rings.every(Boolean) && rings.length
          ? { layers: [L.polygon(rings)], unsupported: 0 }
          : { layers: [], unsupported: 1 };
      }
      case "MULTIPOINT": {
        // Both "((1 2),(3 4))" and the bare "(1 2, 3 4)" spelling are legal.
        const inner = groups(body);
        const pts = inner.length ? inner.map(coords).flat() : coords(body);
        return pts && pts.every(Boolean)
          ? { layers: pts.map((c) => L.circleMarker(c, { radius: 6 })), unsupported: 0 }
          : { layers: [], unsupported: 1 };
      }
      case "MULTILINESTRING": {
        const lines = groups(body).map(coords);
        return lines.every(Boolean) && lines.length
          ? { layers: [L.polyline(lines)], unsupported: 0 }
          : { layers: [], unsupported: 1 };
      }
      case "MULTIPOLYGON": {
        const polys = groups(body).map((p) => groups(p).map(coords));
        const ok = polys.length && polys.every((r) => r.length && r.every(Boolean));
        return ok ? { layers: [L.polygon(polys)], unsupported: 0 }
                  : { layers: [], unsupported: 1 };
      }
      case "GEOMETRYCOLLECTION": {
        // Members are themselves WKT, so recurse and merge the counts.
        let layers = [], unsupported = 0, depth = 0, start = 0;
        for (let i = 0; i <= body.length; i++) {
          const c = body[i];
          if (c === "(") depth++;
          else if (c === ")") depth--;
          if ((c === "," && depth === 0) || i === body.length) {
            const r = wktToLayers(body.slice(start, i));
            layers = layers.concat(r.layers);
            unsupported += r.unsupported;
            start = i + 1;
          }
        }
        return { layers: layers, unsupported: unsupported };
      }
      default:
        return { layers: [], unsupported: 1 };
    }
  }

  function geoJsonToLayers(literal) {
    try {
      return { layers: [L.geoJSON(JSON.parse(literal))], unsupported: 0 };
    } catch (e) {
      return { layers: [], unsupported: 1 };
    }
  }

  function isGeometry(binding) {
    return binding && (binding.datatype === WKT_DT || binding.datatype === GEOJSON_DT);
  }

  // --- the plugin --------------------------------------------------------
  class HolosMap {
    constructor(yasr) {
      this.yasr = yasr;
      this.priority = 5;
      this.label = "Map";
      this.map = null;
    }

    // Offered only when a geometry is actually present, so the tab does not appear on
    // results it could do nothing with.
    canHandleResults() {
      const rows = this.yasr.results && this.yasr.results.getBindings();
      if (!rows || !rows.length) return false;
      return rows.some((row) => Object.keys(row).some((v) => isGeometry(row[v])));
    }

    getIcon() {
      const span = document.createElement("span");
      span.innerHTML =
        '<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24"' +
        ' fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"' +
        ' stroke-linejoin="round"><polygon points="1 6 1 22 8 18 16 22 23 18 23 2 16 6 8 2 1 6"/>' +
        '<line x1="8" y1="2" x2="8" y2="18"/><line x1="16" y1="6" x2="16" y2="22"/></svg>';
      return span;
    }

    // Leaflet keeps internal state tied to the container; dropping the container without
    // telling it leaves listeners attached and the next draw misplaces the map.
    destroy() {
      if (this.map) { this.map.remove(); this.map = null; }
    }

    draw() {
      this.destroy();
      const rows = this.yasr.results.getBindings() || [];
      const vars = this.yasr.results.getVariables() || [];

      const container = document.createElement("div");
      container.style.height = "60vh";
      container.style.minHeight = "320px";
      this.yasr.resultsEl.appendChild(container);

      const note = document.createElement("div");
      note.style.cssText = "font:12px ui-monospace,monospace;color:#5c646e;padding:6px 2px";
      this.yasr.resultsEl.appendChild(note);

      this.map = L.map(container);
      L.tileLayer("https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
        maxZoom: 19,
        attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors',
      }).addTo(this.map);

      const drawn = L.featureGroup().addTo(this.map);
      let shapes = 0, skipped = 0;

      for (const row of rows) {
        for (const v of vars) {
          const binding = row[v];
          if (!isGeometry(binding)) continue;
          const parsed =
            binding.datatype === WKT_DT
              ? wktToLayers(binding.value)
              : geoJsonToLayers(binding.value);
          skipped += parsed.unsupported;
          if (!parsed.layers.length) continue;

          // The rest of the row is the popup, which is what makes the map a result view
          // rather than a picture: every shape still carries its bindings.
          const dl = document.createElement("dl");
          dl.style.cssText = "margin:0;font:12px ui-sans-serif,system-ui,sans-serif";
          for (const other of vars) {
            if (!row[other] || other === v) continue;
            const dt = document.createElement("dt");
            dt.textContent = other;
            dt.style.cssText = "font-weight:600;margin-top:4px";
            const dd = document.createElement("dd");
            dd.textContent = row[other].value;
            dd.style.cssText = "margin:0 0 0 8px;word-break:break-all";
            dl.appendChild(dt);
            dl.appendChild(dd);
          }
          for (const layer of parsed.layers) {
            if (dl.childNodes.length) layer.bindPopup(dl.cloneNode(true));
            drawn.addLayer(layer);
            shapes++;
          }
        }
      }

      if (shapes) {
        this.map.fitBounds(drawn.getBounds(), { padding: [24, 24], maxZoom: 16 });
      } else {
        this.map.setView([20, 0], 2);
      }
      note.textContent =
        shapes + " geometr" + (shapes === 1 ? "y" : "ies") + " drawn" +
        (skipped ? ", " + skipped + " not understood (unsupported type, or a CRS other than CRS84)" : "");

      // Leaflet measures the container on creation, and YASR may still have been laying
      // the tab out at that point; without this the tiles render into a zero-height box.
      setTimeout(() => this.map && this.map.invalidateSize(), 0);
    }
  }

  Yasgui.Yasr.registerPlugin("Map", HolosMap);
})();
"#;

/// The plugin source with the geometry datatypes filled in.
///
/// The JavaScript cannot share a constant with Rust, so the IRIs are substituted rather
/// than written twice. Placeholders rather than `format!` because the plugin is full of
/// braces and escaping every one of them would make it unreadable.
fn map_plugin() -> String {
    MAP_PLUGIN
        .replace("__WKT_DATATYPE__", WKT_DATATYPE)
        .replace("__GEOJSON_DATATYPE__", GEOJSON_DATATYPE)
}

/// The console page, with the endpoint baked in.
#[must_use]
pub fn console(endpoint: &str, title: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title_html}</title>
<link href="https://unpkg.com/@zazuko/yasgui@4/build/yasgui.min.css" rel="stylesheet" type="text/css">
<link href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" rel="stylesheet" type="text/css">
<style>
  :root {{ color-scheme: light dark; }}
  body {{
    margin: 0;
    font: 15px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
    background: #f4f5f7;
    color: #15181c;
  }}
  @media (prefers-color-scheme: dark) {{
    body {{ background: #0f1215; color: #e3e7eb; }}
    header {{ border-color: #2b323a !important; }}
    .meta {{ color: #8b949e !important; }}
  }}
  header {{
    padding: 14px 20px;
    border-bottom: 1px solid #d5d9de;
    display: flex;
    gap: 16px;
    align-items: baseline;
    flex-wrap: wrap;
  }}
  h1 {{ font-size: 16px; margin: 0; letter-spacing: -0.01em; }}
  .meta {{
    font: 12px ui-monospace, SFMono-Regular, Menlo, monospace;
    color: #5c646e;
  }}
  #yasgui {{ margin: 0; }}
</style>
</head>
<body>
<header>
  <h1>{title_html}</h1>
  <span class="meta">endpoint <code>{endpoint_html}</code></span>
  <span class="meta">SPARQL 1.2 &middot; RDF 1.2 triple terms &middot; GeoSPARQL &middot; map view</span>
</header>
<div id="yasgui"></div>
<script src="https://unpkg.com/@zazuko/yasgui@4/build/yasgui.min.js"></script>
<script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
<script>{map_plugin}</script>
<script>
  // The console is a convenience over the protocol endpoint, so it is configured with the
  // same defaults a command-line client would use: this server, POST, JSON results.
  const yasgui = new Yasgui(document.getElementById("yasgui"), {{
    requestConfig: {{
      endpoint: {endpoint_json},
      method: "POST",
      // Sent on every request so a reverse proxy in front of this server can attach or
      // strip credentials without the console needing to know how (DESIGN.md §14.5).
      withCredentials: true
    }},
    copyEndpointOnNewTab: true
  }});
</script>
</body>
</html>
"#,
        map_plugin = map_plugin(),
        title_html = html_escape(title),
        endpoint_html = html_escape(endpoint),
        endpoint_json = json_string(endpoint),
    )
}

/// Escapes a string for embedding in HTML text or an element.
///
/// The endpoint appears twice on this page — once inside the script and once in the
/// header — and escaping only the script copy is exactly the hole this closes. It is the
/// kind of thing a test finds and a reading does not.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Escapes a string for embedding in JavaScript source.
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // `<` is escaped because this lands inside a <script> element, where `</` would
            // otherwise be able to close it early.
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_plugin_is_registered_and_leaflet_is_loaded() {
        let page = console("/query", "HOLOS");
        assert!(page.contains(r#"Yasgui.Yasr.registerPlugin("Map", HolosMap)"#));
        assert!(page.contains("leaflet@1.9.4/dist/leaflet.js"), "the script is missing");
        assert!(page.contains("leaflet@1.9.4/dist/leaflet.css"), "the stylesheet is missing");
    }

    #[test]
    fn the_plugin_looks_for_the_datatypes_the_engine_emits() {
        // The IRIs are substituted into the JavaScript from the Rust constants, so this
        // checks the substitution happened rather than that two copies agree.
        let page = console("/query", "HOLOS");
        for datatype in [WKT_DATATYPE, GEOJSON_DATATYPE] {
            assert!(
                page.contains(datatype),
                "the plugin does not mention {datatype}"
            );
        }
        assert!(
            !page.contains("__WKT_DATATYPE__") && !page.contains("__GEOJSON_DATATYPE__"),
            "a placeholder survived into the page"
        );
    }

    #[test]
    fn the_plugin_reverses_coordinates_for_leaflet() {
        // WKT is (x y), which under CRS84 is (longitude latitude); Leaflet takes
        // [latitude, longitude]. Getting this backwards still renders a map, which is why
        // it is worth pinning: the failure is a plausible-looking picture of the wrong
        // place rather than an error.
        let page = console("/query", "HOLOS");
        assert!(
            page.contains("out.push([n[1], n[0]])"),
            "the coordinate pair is no longer being reversed"
        );
    }

    #[test]
    fn the_endpoint_is_embedded() {
        let page = console("/query", "HOLOS");
        assert!(page.contains(r#"endpoint: "/query""#));
        assert!(page.contains("<title>HOLOS</title>"));
    }

    #[test]
    fn a_hostile_endpoint_cannot_break_out_of_the_script() {
        // The endpoint is operator-supplied, but an operator pasting something odd should
        // get a broken console rather than an injected page.
        let page = console("</script><script>alert(1)</script>", "HOLOS");
        assert!(
            !page.contains("</script><script>alert(1)"),
            "the script element was closed early"
        );
        assert!(page.contains("\\u003c/script"));
    }
}
