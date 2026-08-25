//! The YASGUI console.
//!
//! Served at `/`, pointed at this server's own `/query` endpoint. YASGUI itself is loaded
//! from a CDN rather than vendored: it is a large JavaScript bundle with its own licence
//! and release cadence, and checking a minified copy into an RDF engine's source tree
//! makes that tree harder to audit, not easier.
//!
//! The consequence is worth stating rather than hiding: **the console needs network access
//! to a CDN.** The SPARQL endpoints do not — they are the actual interface, and they work
//! offline. `--no-ui` turns the console off entirely for a deployment that should not be
//! reaching out to anything.

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
  <span class="meta">SPARQL 1.2 &middot; RDF 1.2 triple terms &middot; GeoSPARQL</span>
</header>
<div id="yasgui"></div>
<script src="https://unpkg.com/@zazuko/yasgui@4/build/yasgui.min.js"></script>
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
fn json_string(s: &str) -> String {
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
