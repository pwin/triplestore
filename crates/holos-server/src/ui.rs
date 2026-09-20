//! The SPARQL console.
//!
//! Served at `/`, pointed at this server's own `/query` endpoint. The console is
//! [MatGUI](https://github.com/Matdata-eu/MatGUI), the maintained MIT fork of YASGUI, loaded
//! from a CDN rather than adapted: it is a large JavaScript bundle with its own licence and
//! release cadence, and checking a minified copy into an RDF engine's source tree makes that
//! tree harder to audit, not easier.
//!
//! # What the console may talk to, and how that is enforced
//!
//! A console is the one part of this server that runs on someone else's machine, and a
//! query console handles the two things an operator least wants leaving the building: the
//! queries and their answers. So the page is built to make leaving impossible rather than
//! merely unintended, and the guarantee is the browser's, not the bundle's:
//!
//! * **A Content-Security-Policy** ([`csp`]) names every origin the page may reach. Scripts
//!   come from this server and the CDN and nowhere else; `connect-src 'self'` means the
//!   console can query the server that served it and *no other endpoint* — typing another
//!   URL into the endpoint box gets a refused request, not a leak. Images are this origin,
//!   the CDN, and the one tile host the operator chose. Nothing else is listed, so
//!   nothing else is reachable, whatever any plugin might try.
//! * **Pinned versions with subresource integrity.** Each CDN file is named by exact
//!   version and carries the SHA-384 of the bytes that were reviewed; the browser refuses
//!   to run a file that differs. A CDN that served something else would break the console
//!   visibly rather than run unreviewed code against the endpoint.
//! * **No referrer**, with one measured exception. The CDN learns nothing about the page
//!   that asked, and a link a user follows out of a result carries nothing back to its
//!   target. Tile requests carry the page's *origin* and nothing more — scheme, host and
//!   port, never a path or a query — because OpenStreetMap's tile policy requires a
//!   `Referer` and its servers answer 403 without one. The origin says which server asked,
//!   which the request's own address says already.
//! * **No inline script.** The page's configuration is served as `/ui/console.js` from this
//!   origin, so the policy needs no nonce, no hash, and no `'unsafe-inline'` for scripts.
//!
//! What the policy cannot close is the basemap: a map draws tiles, and which tiles it asks
//! for says where the user is looking. The default tile host is OpenStreetMap, as it always
//! was; `--ui-tiles none` draws geometries over a blank background and the page then reaches
//! the CDN and this server, nothing more. `--no-ui` remains the airtight option — the
//! endpoints need no network at all.
//!
//! # What the bundle brings, and what it does not
//!
//! MatGUI ships four result views beyond YASGUI's: a table with virtual scrolling, a map,
//! a node-edge graph for `CONSTRUCT` and `DESCRIBE`, and the raw response. The map plugin
//! (MIT) replaced the one this module used to carry: it reads WKT, GeoJSON, GML and GeoHash,
//! gets the EPSG:4326 axis order right, reprojects other SRIDs it knows, and turns a drawn
//! rectangle into a `geof:sfWithin` filter. An SRID it does not know it would look up at
//! `epsg.io`; the policy blocks that, and the geometry is skipped with a console warning.
//! The graph and table plugins are Apache-2.0 and are loaded, not copied — the same footing
//! as the Apache-2.0 crates in `THIRD-PARTY.md`.
//!
//! One gap is shared by every YASGUI fork, measured against this server: a `SELECT` binding
//! of an RDF 1.2 triple term (`"type": "triple"` in the JSON results) and a `CONSTRUCT`
//! that serialises `<<( … )>>` both land on the error tab. The protocol endpoints answer
//! them correctly; the console's parsers predate the syntax.

/// The console bundle, pinned. Bumping the version means recomputing the hashes: fetch the
/// two files, `openssl dgst -sha384 -binary | openssl base64 -A`, and review what changed.
const YASGUI_JS: (&str, &str) = (
    "https://unpkg.com/@matdata/yasgui@6.1.0/build/yasgui.min.js",
    "sha384-YA1K8IKkhKUAAlSv/YBH3p4JnBrhVum6nNVI9AEil/+aTH2u9fWtzEfXz1iwqUiX",
);
const YASGUI_CSS: (&str, &str) = (
    "https://unpkg.com/@matdata/yasgui@6.1.0/build/yasgui.min.css",
    "sha384-4yQyR1GtYcpKPLzfFQTtQigHr7DJOc/9s/QLzlD3c5YqsB5JQGY8fxY9KJDIiV9R",
);
/// Leaflet, which the map plugin expects on the page as `window.L`.
const LEAFLET_JS: (&str, &str) = (
    "https://unpkg.com/leaflet@1.9.4/dist/leaflet.js",
    "sha384-cxOPjt7s7Iz04uaHJceBmS+qpjv2JkIHNVcuOrM+YHwZOmJGBXI00mdUXEq65HTH",
);
const LEAFLET_CSS: (&str, &str) = (
    "https://unpkg.com/leaflet@1.9.4/dist/leaflet.css",
    "sha384-sHL9NAb7lN7rfvG5lfHpm643Xkcjzp4jFvuavGOndn6pjVqS6ny56CAt3nsEVT4H",
);

/// The origin every pinned file above comes from; the policy names it once.
const CDN: &str = "https://unpkg.com";

/// The default basemap: the tile template Leaflet's own examples use.
pub const DEFAULT_TILES: &str = "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png";

/// A one-pixel transparent GIF, the "basemap" behind `--ui-tiles none`. The map plugin
/// needs a basemap to draw at all, so the sealed console gets one that never leaves the
/// page.
const BLANK_TILE: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

/// Where the page's script is served from. Same origin, so the policy allows it as
/// `'self'` and the page carries no inline script at all.
pub const SCRIPT_PATH: &str = "/ui/console.js";

/// The console page.
///
/// Everything executable is a `<script src>` naming this origin or a pinned CDN file; the
/// configuration lives in [`script`], served at [`SCRIPT_PATH`].
#[must_use]
pub fn page(title: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>{title_html}</title>
<link href="{yasgui_css}" integrity="{yasgui_css_sri}" crossorigin="anonymous" rel="stylesheet" type="text/css">
<link href="{leaflet_css}" integrity="{leaflet_css_sri}" crossorigin="anonymous" rel="stylesheet" type="text/css">
<link href="/ui/console.css" rel="stylesheet" type="text/css">
</head>
<body>
<header>
  <h1>{title_html}</h1>
  <span class="meta">endpoint <code>/query</code></span>
  <span class="meta">SPARQL 1.2 &middot; RDF 1.2 triple terms &middot; GeoSPARQL &middot; map and graph views</span>
</header>
<div id="yasgui"></div>
<script src="{leaflet_js}" integrity="{leaflet_js_sri}" crossorigin="anonymous"></script>
<script src="{yasgui_js}" integrity="{yasgui_js_sri}" crossorigin="anonymous"></script>
<script src="{script_path}"></script>
</body>
</html>
"#,
        title_html = html_escape(title),
        yasgui_css = YASGUI_CSS.0,
        yasgui_css_sri = YASGUI_CSS.1,
        leaflet_css = LEAFLET_CSS.0,
        leaflet_css_sri = LEAFLET_CSS.1,
        leaflet_js = LEAFLET_JS.0,
        leaflet_js_sri = LEAFLET_JS.1,
        yasgui_js = YASGUI_JS.0,
        yasgui_js_sri = YASGUI_JS.1,
        script_path = SCRIPT_PATH,
    )
}

/// The page's own stylesheet, served at `/ui/console.css` so the policy can say
/// `style-src 'self'` and the CDN — nothing inline of ours.
#[must_use]
pub fn stylesheet() -> &'static str {
    r"
:root { color-scheme: light dark; }
body {
  margin: 0;
  font: 15px/1.5 ui-sans-serif, system-ui, -apple-system, 'Segoe UI', sans-serif;
  background: #f4f5f7;
  color: #15181c;
}
@media (prefers-color-scheme: dark) {
  body { background: #0f1215; color: #e3e7eb; }
  header { border-color: #2b323a; }
  .meta { color: #8b949e; }
}
header {
  padding: 14px 20px;
  border-bottom: 1px solid #d5d9de;
  display: flex;
  gap: 16px;
  align-items: baseline;
  flex-wrap: wrap;
}
h1 { font-size: 16px; margin: 0; letter-spacing: -0.01em; }
.meta { font: 12px ui-monospace, SFMono-Regular, Menlo, monospace; color: #5c646e; }
#yasgui { margin: 0; }
"
}

/// The console's configuration, as the script served at [`SCRIPT_PATH`].
///
/// `endpoint` is where queries go — `/query` on this server. `tiles` is the basemap's
/// tile template, or `None` for a blank one. The map plugin reads its options from
/// `Yasr.defaults.plugins.geo`, set before the console is constructed; the per-instance
/// `yasr.plugins` option a standalone YASR would take is not what a Yasgui tab passes on.
#[must_use]
pub fn script(endpoint: &str, tiles: Option<&str>) -> String {
    let (name, template, attribution) = match tiles {
        Some(t) => ("OpenStreetMap", t, "&copy; OpenStreetMap contributors"),
        None => ("None", BLANK_TILE, ""),
    };
    format!(
        r#"// Served by the same server as the page, so the page carries no inline script and its
// Content-Security-Policy names two script sources: this origin and the CDN.
(function () {{
  Yasgui.Yasr.defaults.plugins.geo = {{
    basemaps: {{
      {name}: L.tileLayer({template}, {{
        attribution: {attribution},
        // The page sends no referrer; tile requests send the origin alone. OpenStreetMap's
        // tile policy requires a Referer and its servers answer 403 to a request without
        // one, and the origin discloses nothing the request's address does not.
        referrerPolicy: "strict-origin"
      }})
    }},
    defaultBasemap: {name_json}
  }};
  // On the window so a script or a test can reach the console; nothing else needs it.
  window.holosConsole = new Yasgui(document.getElementById("yasgui"), {{
    // The console is a convenience over the protocol endpoint, so it is configured with
    // the same defaults a command-line client would use: this server, POST, JSON results.
    requestConfig: {{
      endpoint: {endpoint},
      method: "POST",
      // Sent on every request so a reverse proxy in front of this server can attach or
      // strip credentials without the console needing to know how (DESIGN.md §14.5).
      withCredentials: true
    }},
    copyEndpointOnNewTab: true,
    // The endpoint box would otherwise suggest public endpoints; the policy refuses every
    // one of them, so the suggestion is withdrawn rather than offered and then refused.
    endpointCatalogueOptions: {{ getData: function () {{ return []; }}, keys: [] }}
  }});
}})();
"#,
        name = name,
        name_json = json_string(name),
        template = json_string(template),
        attribution = json_string(attribution),
        endpoint = json_string(endpoint),
    )
}

/// The `Content-Security-Policy` for the page: every origin it may reach, and no other.
///
/// `tiles` is the basemap template; its host becomes the one image origin beyond this
/// server and the CDN. `'unsafe-inline'` is granted to *styles* only — the editor injects
/// its stylesheets at runtime — and an inline style cannot carry data anywhere the
/// `img-src` and `font-src` lists do not already permit.
#[must_use]
pub fn csp(tiles: Option<&str>) -> String {
    let mut img = format!("'self' data: blob: {CDN}");
    if let Some(host) = tiles.and_then(tile_origin) {
        img.push(' ');
        img.push_str(&host);
    }
    format!(
        "default-src 'none'; \
         script-src 'self' {CDN}; \
         style-src 'self' 'unsafe-inline' {CDN}; \
         img-src {img}; \
         font-src {CDN} data:; \
         connect-src 'self'; \
         worker-src blob:; \
         base-uri 'none'; \
         form-action 'self'; \
         frame-ancestors 'none'"
    )
}

/// The origin a tile template reaches, in the form a policy takes.
///
/// Leaflet's `{s}` subdomain placeholder becomes a wildcard label: a template on
/// `{s}.tile.openstreetmap.org` allows `*.tile.openstreetmap.org`. Anything that is not an
/// `https://` or `http://` URL gets no origin, and the map then has nowhere to load tiles
/// from — which is the safe failure.
fn tile_origin(template: &str) -> Option<String> {
    let (scheme, rest) = template.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let host = rest.split('/').next()?.replace("{s}", "*");
    if host.is_empty() || host.contains('{') {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// Escapes a string for embedding in HTML text or an element.
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
            // `<` is escaped because this may land inside a <script> element, where `</`
            // would otherwise be able to close it early.
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
    fn the_page_carries_no_inline_script() {
        let page = page("HOLOS");
        // Every <script> names a source; none has a body.
        for piece in page.split("<script").skip(1) {
            let tag = piece.split('>').next().unwrap_or("");
            assert!(tag.contains("src="), "an inline script: <script{tag}>");
        }
        assert!(page.contains(r#"<script src="/ui/console.js">"#));
        assert!(page.contains(r#"<meta name="referrer" content="no-referrer">"#));
    }

    #[test]
    fn every_cdn_file_is_pinned_and_integrity_checked() {
        let page = page("HOLOS");
        for (url, sri) in [YASGUI_JS, YASGUI_CSS, LEAFLET_JS, LEAFLET_CSS] {
            assert!(
                url.starts_with(CDN),
                "{url} is not on the CDN the policy allows"
            );
            assert!(
                url.contains("@6.1.0/") || url.contains("@1.9.4/"),
                "{url} floats rather than pinning a version"
            );
            assert!(sri.starts_with("sha384-"), "{url} has no integrity hash");
            assert!(
                page.contains(&format!(
                    r#""{url}" integrity="{sri}" crossorigin="anonymous""#
                )),
                "{url} is not loaded with its hash"
            );
        }
    }

    #[test]
    fn the_policy_lets_the_console_reach_this_server_and_the_cdn_only() {
        let policy = csp(Some(DEFAULT_TILES));
        assert!(policy.contains("default-src 'none'"));
        assert!(policy.contains("connect-src 'self';"));
        assert!(policy.contains(&format!("script-src 'self' {CDN};")));
        assert!(policy.contains(
            "img-src 'self' data: blob: https://unpkg.com https://*.tile.openstreetmap.org;"
        ));
        assert!(policy.contains("frame-ancestors 'none'"));
        assert!(!policy.contains("unsafe-eval"));
        // Inline styles are the one allowance, and only for styles.
        assert!(policy.contains("style-src 'self' 'unsafe-inline'"));
        assert!(!policy.contains("script-src 'self' 'unsafe-inline'"));
    }

    #[test]
    fn a_sealed_console_names_no_tile_host_at_all() {
        let policy = csp(None);
        assert!(policy.contains("img-src 'self' data: blob: https://unpkg.com;"));
        let script = script("/query", None);
        assert!(script.contains(BLANK_TILE));
        assert!(!script.contains("openstreetmap"));
    }

    #[test]
    fn a_tile_template_becomes_one_origin() {
        assert_eq!(
            tile_origin(DEFAULT_TILES).as_deref(),
            Some("https://*.tile.openstreetmap.org")
        );
        assert_eq!(
            tile_origin("https://tiles.example.org/osm/{z}/{x}/{y}.png").as_deref(),
            Some("https://tiles.example.org")
        );
        assert_eq!(tile_origin("ftp://tiles.example.org/{z}"), None);
        assert_eq!(tile_origin("not a url"), None);
        assert_eq!(tile_origin("https://{a}.example.org/{z}"), None);
    }

    #[test]
    fn the_endpoint_is_embedded_in_the_script() {
        let script = script("/query", Some(DEFAULT_TILES));
        assert!(script.contains(r#"endpoint: "/query""#));
        assert!(script.contains("Yasgui.Yasr.defaults.plugins.geo"));
        assert!(
            script.contains(r#"L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png""#)
        );
        // OpenStreetMap answers 403 to a tile request with no Referer, so the tiles — and
        // only the tiles — carry the page's origin.
        assert!(script.contains(r#"referrerPolicy: "strict-origin""#));
        assert!(page("HOLOS").contains("<title>HOLOS</title>"));
    }

    #[test]
    fn a_hostile_endpoint_cannot_break_out_of_the_script() {
        // The endpoint is operator-supplied, but an operator pasting something odd should
        // get a broken console rather than an injected page.
        let script = script("</script><script>alert(1)</script>", Some(DEFAULT_TILES));
        assert!(
            !script.contains("</script><script>alert(1)"),
            "the script element was closed early"
        );
        assert!(script.contains("\\u003c/script"));
        let page = page("<img src=x onerror=alert(1)>");
        assert!(!page.contains("<img src=x"));
    }
}
