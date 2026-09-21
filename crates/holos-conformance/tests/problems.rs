//! A failed request over HTTP is answered with an RFC 9457 problem document.
//!
//! The unit tests in `holos-server` cover the document's shape; this covers the wire: a
//! real server on a real socket, a request that fails, and the status, media type and
//! members a client would act on. It reuses the harness the protocol suites use, and
//! skips like they do when the server binary is not built.

use holos_conformance::protocol::{self, ScriptedRequest};

mod harness;
use harness::{percent_encode, server_binary, Server};

fn get(address: &str, path: &str, accept: Option<&str>) -> protocol::HttpResponse {
    let mut headers = Vec::new();
    if let Some(accept) = accept {
        headers.push(("Accept".to_owned(), accept.to_owned()));
    }
    let request = ScriptedRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        headers,
        body: None,
        expected_status: Vec::new(),
        expected_status_class: Vec::new(),
        expected_boolean: None,
        expected_format: None,
    };
    protocol::send(address, &request).expect("a response")
}

/// A member's raw JSON value, by name, from a flat document. Enough for these tests and
/// keeps the harness free of a JSON dependency.
fn member<'a>(document: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\"{name}\":");
    let start = document.find(&key)? + key.len();
    let rest = &document[start..];
    let end = if rest.starts_with('"') {
        // A string: up to the closing quote, skipping escaped ones.
        let mut escaped = false;
        let mut end = None;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => {
                    end = Some(i + 1);
                    break;
                }
                _ => escaped = false,
            }
        }
        end?
    } else {
        rest.find([',', '}'])?
    };
    Some(&rest[..end])
}

#[test]
fn a_failed_query_is_answered_with_a_problem_document() {
    let Some(binary) = server_binary() else {
        eprintln!("skipping: holos-server is not built");
        return;
    };
    // Above both protocol suites' ranges.
    let Some(server) = Server::start(&binary, 18_400, &["--read-only"]) else {
        eprintln!("skipping: no server on port 18400");
        return;
    };
    let address = &server.address;

    // A syntax error: 400, the problem type, and where it is.
    let path = format!(
        "/query?query={}",
        percent_encode("SELECT ?s WHERE { ?s ?p }")
    );
    let response = get(address, &path, None);
    assert_eq!(response.status, 400, "{}", response.body);
    assert_eq!(
        response.header("content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        member(&response.body, "type"),
        Some("\"https://holos.dev/problems/syntax\"")
    );
    assert_eq!(member(&response.body, "status"), Some("400"));
    assert_eq!(member(&response.body, "line"), Some("1"));
    assert_eq!(member(&response.body, "column"), Some("26"));
    assert!(
        member(&response.body, "detail").is_some_and(|d| d.contains("error at 1:26")),
        "{}",
        response.body
    );

    // A function this server does not have: the query's mistake, so 400, and named.
    let path = format!(
        "/query?query={}",
        percent_encode("PREFIX x: <http://nosuch/> SELECT (x:fn(?s) AS ?v) WHERE { ?s ?p ?o }")
    );
    let response = get(address, &path, Some("application/sparql-results+json"));
    assert_eq!(response.status, 400, "{}", response.body);
    assert_eq!(
        member(&response.body, "type"),
        Some("\"https://holos.dev/problems/unknown-function\"")
    );
    assert!(
        member(&response.body, "detail").is_some_and(|d| d.contains("http://nosuch/fn")),
        "{}",
        response.body
    );

    // A client that asks for plain text and nothing else still gets the line of text.
    let path = format!(
        "/query?query={}",
        percent_encode("SELECT ?s WHERE { ?s ?p }")
    );
    let response = get(address, &path, Some("text/plain"));
    assert_eq!(response.status, 400);
    assert_eq!(response.header("content-type"), Some("text/plain"));
    assert!(
        response
            .body
            .starts_with("SPARQL syntax error: error at 1:26"),
        "{}",
        response.body
    );

    // A protocol mistake is a problem too.
    let response = get(address, "/query", None);
    assert_eq!(response.status, 400);
    assert_eq!(
        member(&response.body, "type"),
        Some("\"https://holos.dev/problems/bad-request\"")
    );

    // And so is the read-only refusal, with the status the console and a proxy act on.
    let request = ScriptedRequest {
        method: "POST".to_owned(),
        path: "/update".to_owned(),
        headers: vec![(
            "Content-Type".to_owned(),
            "application/sparql-update".to_owned(),
        )],
        body: Some("INSERT DATA { <urn:a> <urn:b> <urn:c> }".to_owned()),
        expected_status: Vec::new(),
        expected_status_class: Vec::new(),
        expected_boolean: None,
        expected_format: None,
    };
    let response = protocol::send(address, &request).expect("a response");
    assert_eq!(response.status, 403);
    assert_eq!(
        member(&response.body, "type"),
        Some("\"https://holos.dev/problems/read-only\"")
    );
}

/// A query stopped by `--timeout` is answered the same way: the `timeout` kind, and a
/// detail that names the limit and the flag, rather than the evaluator's bare "cancelled".
///
/// The failure arrives while the rows are being written, which is the path that used to
/// reach a client as an empty 500.
#[test]
fn a_query_past_its_time_limit_is_answered_with_a_problem_document() {
    let Some(binary) = server_binary() else {
        eprintln!("skipping: holos-server is not built");
        return;
    };
    let Some(server) = Server::start(&binary, 18_401, &["--timeout", "0.2"]) else {
        eprintln!("skipping: no server on port 18401");
        return;
    };
    let address = &server.address;

    // Three hundred triples: a three-way cross product over them is 27 million rows,
    // which is far more than a fifth of a second of streaming on any machine.
    let mut data = String::from("INSERT DATA {");
    for i in 0..300 {
        data.push_str(&format!(" <urn:s{i}> <urn:p> <urn:o{i}> ."));
    }
    data.push('}');
    let request = ScriptedRequest {
        method: "POST".to_owned(),
        path: "/update".to_owned(),
        headers: vec![(
            "Content-Type".to_owned(),
            "application/sparql-update".to_owned(),
        )],
        body: Some(data),
        expected_status: Vec::new(),
        expected_status_class: Vec::new(),
        expected_boolean: None,
        expected_format: None,
    };
    let response = protocol::send(address, &request).expect("a response");
    assert_eq!(response.status, 200, "{}", response.body);

    let path = format!(
        "/query?query={}",
        percent_encode("SELECT * WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }")
    );
    let response = get(address, &path, Some("application/sparql-results+json"));
    assert_eq!(response.status, 500, "{}", response.body);
    assert_eq!(
        response.header("content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        member(&response.body, "type"),
        Some("\"https://holos.dev/problems/timeout\"")
    );
    assert_eq!(
        member(&response.body, "title"),
        Some("\"The query ran past its time limit\"")
    );
    assert!(
        member(&response.body, "detail")
            .is_some_and(|d| d.contains("0.2 s time limit") && d.contains("--timeout")),
        "{}",
        response.body
    );
}
