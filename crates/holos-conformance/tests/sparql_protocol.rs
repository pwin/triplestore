//! The SPARQL 1.1 Protocol suite, run against a live server.
//!
//! ```text
//! cargo test -p holos-conformance --test sparql_protocol
//! ```
//!
//! These are the tests for the *protocol* rather than the language: how a query arrives
//! (URL parameter, form field, direct body), how the dataset is named on the wire
//! (`default-graph-uri`), which media type comes back, and — for a third of the suite —
//! which requests a server is obliged to refuse.
//!
//! # What the manifest leaves to the runner
//!
//! Three things, each of which has to be decided here:
//!
//! * **The endpoint.** Every `ht:absolutePath` starts `/sparql/`, and the manifest says in
//!   as many words that a runner must substitute its own path. This server splits query
//!   from update, so the substitution depends on what the request carries.
//! * **The dataset.** `ut:graphData` names graphs the server is expected to be holding
//!   before the conversation starts. They are loaded over the Graph Store Protocol.
//! * **The status.** Expectations are whole classes (`hts:StatusCode2xx`), because the
//!   protocol leaves the choice within a class to the server.

use holos_conformance::{manifest, protocol, testsuite_root};

mod harness;
use harness::{percent_encode, server_binary, trace, Server};

/// Where the suite's `/sparql/` prefix is redirected.
const QUERY_PATH: &str = "/query";
const UPDATE_PATH: &str = "/update";

#[test]
fn sparql_protocol() {
    let Some(root) = testsuite_root() else {
        eprintln!("skipping: run scripts/fetch-testsuites.sh first");
        return;
    };
    let Some(binary) = server_binary() else {
        eprintln!(
            "skipping: holos-server is not built. `cargo build -p holos-server` first, or run \
             `cargo test --workspace` which builds it."
        );
        return;
    };

    let manifest_path = root
        .join("sparql")
        .join("sparql11")
        .join("protocol")
        .join("manifest.ttl");
    if !manifest_path.is_file() {
        eprintln!("skipping: {} is absent", manifest_path.display());
        return;
    }

    let tests = manifest::load(&manifest_path).expect("reading the manifest");
    let scripted: Vec<_> = tests
        .iter()
        .filter(|t| t.kind.ends_with("ProtocolTest") && !t.kind.ends_with("GraphStoreProtocolTest"))
        .collect();
    assert!(
        !scripted.is_empty(),
        "the manifest holds no ProtocolTest entries"
    );

    let mut passed = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut skipped = Vec::new();
    // Above the Graph Store suite's range so the two can run at the same time.
    let mut port = 18_100_u16;

    for test in &scripted {
        let Some(script) = &test.script else {
            skipped.push((
                test.short_id().to_owned(),
                "no ht:Connection in the action".to_owned(),
            ));
            continue;
        };

        port += 1;
        // The Graph Store endpoint is how the dataset gets in, so it is configured even
        // though no test in this suite addresses it directly.
        let Some(server) = Server::start(
            &binary,
            port,
            &["--gsp-path", "/gsp", "--gsp-base", "http://www.example"],
        ) else {
            skipped.push((
                test.short_id().to_owned(),
                format!("no server on port {port}"),
            ));
            continue;
        };

        if let Err(why) = load_dataset(&server.address, &test.graph_data) {
            skipped.push((test.short_id().to_owned(), why));
            continue;
        }

        match replay(&server.address, script) {
            Ok(()) => passed.push(test.short_id().to_owned()),
            Err(why) => failed.push((test.short_id().to_owned(), why)),
        }
    }

    let attempted = passed.len() + failed.len();
    println!(
        "\nsparql-protocol: {}/{} passed, {} failed, {} skipped\n",
        passed.len(),
        attempted,
        failed.len(),
        skipped.len()
    );
    for (id, why) in &failed {
        println!("  {id}\n      {why}");
    }
    for (id, why) in &skipped {
        println!("  {id} (skipped)\n      {why}");
    }

    holos_conformance::ratchet_named("sparql-protocol", &failed);
}

/// Puts the test's `ut:graphData` into the server before the conversation starts.
///
/// Over the Graph Store Protocol rather than by starting the server on a pre-loaded store:
/// it needs no extra server flag, and it exercises a path this workspace has tests for.
fn load_dataset(address: &str, graphs: &[(String, std::path::PathBuf)]) -> Result<(), String> {
    for (name, path) in graphs {
        let body = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let media = match path.extension().and_then(|e| e.to_str()) {
            Some("ttl") => "text/turtle",
            Some("rdf" | "xml") => "application/rdf+xml",
            // The suite's files are N-Triples.
            _ => "application/n-triples",
        };
        let request = protocol::ScriptedRequest {
            method: "PUT".to_owned(),
            path: format!("/gsp?graph={}", percent_encode(name)),
            headers: vec![("content-type".to_owned(), media.to_owned())],
            body: Some(body),
            expected_status: Vec::new(),
            expected_status_class: Vec::new(),
            expected_boolean: None,
            expected_format: None,
        };
        let response =
            protocol::send(address, &request).map_err(|e| format!("loading <{name}>: {e}"))?;
        if response.status / 100 != 2 {
            return Err(format!(
                "loading <{name}> answered {}: {}",
                response.status, response.body
            ));
        }
    }
    Ok(())
}

/// Replays one conversation, checking status, `ASK` answer and result family.
fn replay(address: &str, script: &protocol::Script) -> Result<(), String> {
    for (i, request) in script.requests.iter().enumerate() {
        let mut request = request.clone();
        request.path = redirect(&request);
        let request = &request;
        trace(request);

        let response = protocol::send(address, request).map_err(|e| {
            format!(
                "request {} ({} {}): {e}",
                i + 1,
                request.method,
                request.path
            )
        })?;

        let where_ = format!("request {} ({} {})", i + 1, request.method, request.path);

        if !request.accepts_status(response.status) {
            return Err(format!(
                "{where_} answered {} — the specification accepts {}. Body: {}",
                response.status,
                request.expectation(),
                response.body.trim().chars().take(200).collect::<String>()
            ));
        }

        // A refusal has nothing further to check: the body of a 4xx is a diagnostic, and
        // the specification does not say what it holds.
        if response.status / 100 != 2 {
            continue;
        }

        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .to_owned();

        if let Some(family) = &request.expected_format {
            if !format_matches(&content_type, family) {
                return Err(format!(
                    "{where_} answered with `{content_type}`, which is not a {family} \
                     serialisation"
                ));
            }
        }

        if let Some(expected) = request.expected_boolean {
            let Some(actual) = boolean_in(&response.body) else {
                return Err(format!(
                    "{where_} was expected to answer the ASK {expected}, but no boolean \
                     could be read out of `{}`",
                    response.body.trim().chars().take(120).collect::<String>()
                ));
            };
            if actual != expected {
                return Err(format!(
                    "{where_} answered the ASK {actual}, expected {expected}"
                ));
            }
        }
    }
    Ok(())
}

/// Rewrites the manifest's `/sparql/` prefix onto this server's endpoints.
///
/// The suite assumes one endpoint for both operations and tells runners to substitute their
/// own; this server has two, so the choice is made from what the request carries. An
/// `update` parameter or an update body means the update endpoint — including for the
/// deliberately-bad requests, where sending them to the query endpoint would produce the
/// right status code for the wrong reason.
fn redirect(request: &protocol::ScriptedRequest) -> String {
    let endpoint = if is_update(request) {
        UPDATE_PATH
    } else {
        QUERY_PATH
    };
    match request.path.split_once('?') {
        Some((_, query)) => format!("{endpoint}?{query}"),
        None => endpoint.to_owned(),
    }
}

/// Whether a request is an update rather than a query.
fn is_update(request: &protocol::ScriptedRequest) -> bool {
    let content_type = request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    if content_type.starts_with("application/sparql-update") {
        return true;
    }
    // A form field or a URL parameter named `update`. Matching on the parameter name
    // rather than the body's text avoids mistaking a query that merely mentions the word.
    let query = request.path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let body = request.body.as_deref().unwrap_or("");
    [query, body].iter().any(|source| {
        source
            .split('&')
            .any(|pair| pair.split_once('=').is_some_and(|(k, _)| k == "update"))
    })
        // `using-graph-uri` only exists on an update request.
        || query.contains("using-graph-uri")
        || body.contains("using-graph-uri")
}

/// Whether a media type belongs to the family the manifest names.
///
/// `mf:expectedFormat` names a family, not a media type — the protocol lets a server pick
/// within one, and every test that sets it also sends a matching `Accept`. `"boolean"` and
/// `"tabular"` are both SPARQL results formats; what separates them is the shape of the
/// answer, which `mf:expectedBoolean` checks separately.
fn format_matches(content_type: &str, family: &str) -> bool {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match family {
        "boolean" | "tabular" => matches!(
            media.as_str(),
            "application/sparql-results+json"
                | "application/sparql-results+xml"
                | "text/csv"
                | "text/tab-separated-values"
        ),
        "RDF" => matches!(
            media.as_str(),
            "text/turtle"
                | "application/rdf+xml"
                | "application/n-triples"
                | "application/ld+json"
                | "application/trig"
                | "application/n-quads"
                | "text/n3"
        ),
        // An unknown family is not something to fail a server over.
        _ => true,
    }
}

/// Reads the answer out of an `ASK` result, whatever it was serialised as.
///
/// JSON says `"boolean": true`, XML says `<boolean>true</boolean>`, CSV says `true`. All
/// three come down to which of the two words appears, and exactly one of them does.
fn boolean_in(body: &str) -> Option<bool> {
    let lower = body.to_ascii_lowercase();
    match (lower.contains("true"), lower.contains("false")) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        path: &str,
        body: Option<&str>,
        content_type: Option<&str>,
    ) -> protocol::ScriptedRequest {
        protocol::ScriptedRequest {
            method: "POST".to_owned(),
            path: path.to_owned(),
            headers: content_type
                .map(|c| vec![("content-type".to_owned(), c.to_owned())])
                .unwrap_or_default(),
            body: body.map(str::to_owned),
            expected_status: Vec::new(),
            expected_status_class: Vec::new(),
            expected_boolean: None,
            expected_format: None,
        }
    }

    #[test]
    fn a_query_goes_to_the_query_endpoint() {
        let r = request("/sparql/?query=ASK%20%7B%7D", None, None);
        assert_eq!(redirect(&r), "/query?query=ASK%20%7B%7D");
    }

    #[test]
    fn an_update_parameter_picks_the_update_endpoint() {
        let r = request(
            "/sparql/",
            Some("update=CLEAR%20ALL"),
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(redirect(&r), "/update");
        assert!(is_update(&r));
    }

    #[test]
    fn a_direct_update_body_picks_the_update_endpoint() {
        let r = request(
            "/sparql/",
            Some("CLEAR ALL"),
            Some("application/sparql-update"),
        );
        assert!(is_update(&r));
    }

    #[test]
    fn a_query_that_mentions_update_is_still_a_query() {
        // The word appears in the query text; the parameter name is what decides.
        let r = request(
            "/sparql/",
            Some("query=SELECT%20%3Fupdate%20WHERE%7B%7D"),
            None,
        );
        assert!(
            !is_update(&r),
            "matched on the text rather than the parameter"
        );
    }

    #[test]
    fn booleans_are_read_out_of_every_serialisation() {
        assert_eq!(boolean_in(r#"{"head":{},"boolean":true}"#), Some(true));
        assert_eq!(
            boolean_in("<sparql><boolean>false</boolean></sparql>"),
            Some(false)
        );
        assert_eq!(boolean_in("true"), Some(true));
        // A SELECT result mentioning neither word, and one mentioning both, are both
        // unreadable as booleans rather than silently guessed at.
        assert_eq!(boolean_in("x,y\n1,2"), None);
    }

    #[test]
    fn result_families_are_told_apart() {
        assert!(format_matches("application/sparql-results+json", "boolean"));
        assert!(format_matches("text/csv; charset=utf-8", "tabular"));
        assert!(format_matches("text/turtle", "RDF"));
        assert!(!format_matches("text/turtle", "tabular"));
        assert!(!format_matches("application/sparql-results+json", "RDF"));
    }
}
