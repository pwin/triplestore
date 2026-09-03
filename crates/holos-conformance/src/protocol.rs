//! Driving a live server, for the protocol suites.
//!
//! `GraphStoreProtocolTest` and `ProtocolTest` are not queries. Each is a **scripted HTTP
//! conversation**, written in the W3C `ht:` vocabulary: an ordered list of requests, each
//! with a method, a path, headers and a body, and each with a set of acceptable response
//! statuses.
//!
//! ```turtle
//! mf:action [ a ht:Connection ;
//!             ht:requests ([ a ht:Request ;
//!                            ht:methodName "PUT" ;
//!                            ht:absolutePath "/gsp?default" ;
//!                            ht:body [ cnt:chars "…" ] ;
//!                            ht:resp [ mf:expectedStatus hts:OK, hts:Created ] ]) ]
//! ```
//!
//! So running them needs an actual server on an actual socket. This module supplies the two
//! halves: reading the script out of the manifest, and a small HTTP/1.1 client to replay it.
//!
//! # Why a hand-written client
//!
//! The requests are trivial — a method, a path, a few headers, a body — and every one closes
//! its connection. Pulling in an HTTP client crate for that would add a dependency tree to
//! the *test* harness in order to exercise a server this workspace already builds. Sixty
//! lines of `TcpStream` covers it, and keeps the harness free of anything the shipped code
//! does not already depend on.

use anyhow::{anyhow, Context, Result};
use oxrdf::{Graph, NamedNode, NamedOrBlankNodeRef, TermRef};
use std::io::{Read, Write};
use std::net::TcpStream;

const HT: &str = "http://www.w3.org/2011/http#";
const CNT: &str = "http://www.w3.org/2011/content#";
const HTS: &str = "http://www.w3.org/2011/http-statusCodes#";
const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

fn iri(ns: &str, local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{ns}{local}"))
}

/// One request in a scripted conversation.
#[derive(Debug, Clone)]
pub struct ScriptedRequest {
    /// HTTP method.
    pub method: String,
    /// Path, including any query string.
    pub path: String,
    /// Request headers, in order.
    pub headers: Vec<(String, String)>,
    /// Request body, if any.
    pub body: Option<String>,
    /// Statuses the specification accepts. **Any** of them passes.
    ///
    /// Several tests list `hts:OK, hts:Created, hts:NoContent` together, because the
    /// specification genuinely permits a server to choose. Insisting on one would fail a
    /// conforming implementation.
    pub expected_status: Vec<u16>,
    /// Status *classes* the specification accepts, as their leading digit.
    ///
    /// The Graph Store suite names exact codes; the SPARQL Protocol suite names classes
    /// (`hts:StatusCode2xx`), because the protocol leaves the choice within a class open.
    pub expected_status_class: Vec<u16>,
    /// `mf:expectedBoolean` — the answer an `ASK` must give.
    pub expected_boolean: Option<bool>,
    /// `mf:expectedFormat` — `"boolean"`, `"tabular"` or `"RDF"`.
    ///
    /// A family of serialisations rather than one media type: the protocol lets the server
    /// pick within the family, and every test that sets this also sets a matching `Accept`.
    pub expected_format: Option<String>,
}

impl ScriptedRequest {
    /// Whether a response status is one this request allows.
    ///
    /// No expectation accepts anything, which is what a request that only sets the scene
    /// for a later one wants.
    #[must_use]
    pub fn accepts_status(&self, status: u16) -> bool {
        if self.expected_status.is_empty() && self.expected_status_class.is_empty() {
            return true;
        }
        self.expected_status.contains(&status)
            || self.expected_status_class.contains(&(status / 100))
    }

    /// How the expectation reads in a failure message.
    #[must_use]
    pub fn expectation(&self) -> String {
        let mut parts: Vec<String> = self.expected_status.iter().map(u16::to_string).collect();
        parts.extend(self.expected_status_class.iter().map(|c| format!("{c}xx")));
        parts.join(" or ")
    }
}

/// A whole conversation.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// The requests, in order. Order matters: they build on each other's effects.
    pub requests: Vec<ScriptedRequest>,
}

/// Reads the scripted conversation out of a test's `mf:action`.
///
/// # Errors
///
/// Fails when the action is not an `ht:Connection`, or its request list cannot be walked.
pub fn read_script(graph: &Graph, action: NamedOrBlankNodeRef<'_>) -> Result<Script> {
    let list = object(graph, action, &iri(HT, "requests"))
        .ok_or_else(|| anyhow!("no ht:requests on the connection"))?;
    let mut requests = Vec::new();
    for node in rdf_list(graph, list) {
        requests.push(read_request(graph, node)?);
    }
    Ok(Script { requests })
}

fn read_request(graph: &Graph, node: NamedOrBlankNodeRef<'_>) -> Result<ScriptedRequest> {
    let method = literal(graph, node, &iri(HT, "methodName")).unwrap_or_else(|| "GET".to_owned());
    let path = literal(graph, node, &iri(HT, "absolutePath"))
        .ok_or_else(|| anyhow!("a request has no ht:absolutePath"))?;

    let headers = object(graph, node, &iri(HT, "headers"))
        .map(|list| {
            rdf_list(graph, list)
                .into_iter()
                .filter_map(|h| {
                    Some((
                        literal(graph, h, &iri(HT, "fieldName"))?,
                        literal(graph, h, &iri(HT, "fieldValue"))?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    let body = object(graph, node, &iri(HT, "body"))
        .and_then(|b| literal(graph, b, &iri(CNT, "chars")))
        // The manifests indent bodies inside triple-quoted strings; the leading whitespace
        // is presentation, not content, and Turtle does not care.
        .map(|text| text.trim().to_owned());

    let response = object(graph, node, &iri(HT, "resp"));

    let mut expected_status = Vec::new();
    let mut expected_status_class = Vec::new();
    if let Some(resp) = response {
        for term in graph.objects_for_subject_predicate(resp, iri(MF, "expectedStatus").as_ref()) {
            let TermRef::NamedNode(n) = term else {
                continue;
            };
            if let Some(code) = status_code(n.as_str()) {
                expected_status.push(code);
            } else if let Some(class) = status_class(n.as_str()) {
                expected_status_class.push(class);
            }
        }
    }

    let expected_boolean = response
        .and_then(|resp| literal(graph, resp, &iri(MF, "expectedBoolean")))
        .and_then(|v| v.parse().ok());
    let expected_format =
        response.and_then(|resp| literal(graph, resp, &iri(MF, "expectedFormat")));

    Ok(ScriptedRequest {
        method,
        path,
        headers,
        body,
        expected_status,
        expected_status_class,
        expected_boolean,
        expected_format,
    })
}

/// Maps an `hts:StatusCode2xx`-style IRI to its leading digit.
fn status_class(iri: &str) -> Option<u16> {
    match iri.strip_prefix(HTS)? {
        "StatusCode2xx" => Some(2),
        "StatusCode3xx" => Some(3),
        "StatusCode4xx" => Some(4),
        "StatusCode5xx" => Some(5),
        _ => None,
    }
}

/// Maps an `hts:` status IRI to its numeric code.
fn status_code(iri: &str) -> Option<u16> {
    let local = iri.strip_prefix(HTS)?;
    Some(match local {
        "OK" => 200,
        "Created" => 201,
        "Accepted" => 202,
        "NoContent" => 204,
        "ResetContent" => 205,
        "MovedPermanently" => 301,
        "Found" => 302,
        "SeeOther" => 303,
        "NotModified" => 304,
        "BadRequest" => 400,
        "Unauthorized" => 401,
        "Forbidden" => 403,
        "NotFound" => 404,
        "MethodNotAllowed" => 405,
        "NotAcceptable" => 406,
        "Conflict" => 409,
        "Gone" => 410,
        "LengthRequired" => 411,
        "PayloadTooLarge" | "RequestEntityTooLarge" => 413,
        "UnsupportedMediaType" => 415,
        "InternalServerError" => 500,
        "NotImplemented" => 501,
        "ServiceUnavailable" => 503,
        _ => return None,
    })
}

fn object<'a>(
    graph: &'a Graph,
    subject: NamedOrBlankNodeRef<'_>,
    predicate: &NamedNode,
) -> Option<NamedOrBlankNodeRef<'a>> {
    graph
        .objects_for_subject_predicate(subject, predicate.as_ref())
        .find_map(|t| match t {
            TermRef::NamedNode(n) => Some(NamedOrBlankNodeRef::NamedNode(n)),
            TermRef::BlankNode(b) => Some(NamedOrBlankNodeRef::BlankNode(b)),
            _ => None,
        })
}

fn literal(
    graph: &Graph,
    subject: NamedOrBlankNodeRef<'_>,
    predicate: &NamedNode,
) -> Option<String> {
    graph
        .objects_for_subject_predicate(subject, predicate.as_ref())
        .find_map(|t| match t {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
            _ => None,
        })
}

/// Walks an RDF collection into a vector, preserving order.
fn rdf_list<'a>(graph: &'a Graph, head: NamedOrBlankNodeRef<'a>) -> Vec<NamedOrBlankNodeRef<'a>> {
    let (first, rest, nil) = (
        NamedNode::new_unchecked(RDF_FIRST),
        NamedNode::new_unchecked(RDF_REST),
        RDF_NIL,
    );
    let mut out = Vec::new();
    let mut node = Some(head);
    // A malformed list could cycle; the manifests are generated, but a test harness that
    // hangs is worse than one that stops.
    let mut guard = 0;
    while let Some(current) = node {
        if matches!(current, NamedOrBlankNodeRef::NamedNode(n) if n.as_str() == nil) {
            break;
        }
        guard += 1;
        if guard > 10_000 {
            break;
        }
        if let Some(item) = object(graph, current, &first) {
            out.push(item);
        }
        node = object(graph, current, &rest);
    }
    out
}

// ---------------------------------------------------------------------------------
// the client
// ---------------------------------------------------------------------------------

/// What a server said.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Response headers, lower-cased names.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: String,
}

impl HttpResponse {
    /// One header's value.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request and reads the whole response.
///
/// `Connection: close` throughout: a keep-alive parser would have to handle chunked
/// encoding and connection reuse for no benefit, since each scripted request is
/// independent.
///
/// # Errors
///
/// Fails on connection or I/O trouble, or a response that is not HTTP/1.x.
pub fn send(address: &str, request: &ScriptedRequest) -> Result<HttpResponse> {
    let mut stream =
        TcpStream::connect(address).with_context(|| format!("connecting to {address}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;

    let body = request.body.as_deref().unwrap_or_default();
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n",
        request.method, request.path
    );
    for (name, value) in &request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    // Always sent, including zero: a server is entitled to expect it on a write.
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("no header/body separator in the response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = String::from_utf8_lossy(&raw[split + 4..]).into_owned();

    let mut lines = head.lines();
    let status_line = lines.next().ok_or_else(|| anyhow!("empty response"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("unparseable status line: {status_line}"))?;

    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        })
        .collect();

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_iris_map_to_codes() {
        assert_eq!(status_code(&format!("{HTS}OK")), Some(200));
        assert_eq!(status_code(&format!("{HTS}Created")), Some(201));
        assert_eq!(status_code(&format!("{HTS}NoContent")), Some(204));
        assert_eq!(status_code(&format!("{HTS}NotFound")), Some(404));
        assert_eq!(status_code("http://example.org/NotAStatus"), None);
    }

    #[test]
    fn a_response_parses_into_status_headers_and_body() {
        let raw =
            b"HTTP/1.1 201 Created\r\nContent-Type: text/turtle\r\nX-Other: 1\r\n\r\nbody here";
        let r = parse_response(raw).expect("parse");
        assert_eq!(r.status, 201);
        assert_eq!(r.header("content-type"), Some("text/turtle"));
        assert_eq!(r.body, "body here");
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let raw = b"HTTP/1.1 200 OK\r\nCONTENT-TYPE: text/plain\r\n\r\n";
        let r = parse_response(raw).expect("parse");
        assert_eq!(r.header("content-type"), Some("text/plain"));
    }

    #[test]
    fn a_body_less_response_is_fine() {
        let raw = b"HTTP/1.1 204 No Content\r\n\r\n";
        let r = parse_response(raw).expect("parse");
        assert_eq!(r.status, 204);
        assert!(r.body.is_empty());
    }

    #[test]
    fn a_response_without_a_separator_is_an_error() {
        assert!(parse_response(b"HTTP/1.1 200 OK").is_err());
    }
}
