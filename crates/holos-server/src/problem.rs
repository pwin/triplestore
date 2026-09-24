//! What a failed request is answered with: an RFC 9457 problem document.
//!
//! The SPARQL Protocol fixes the status a failure gets — 400 for a query that is wrong as
//! sent, 500 for one the service will not or cannot answer — and says the body should
//! explain, but leaves the explanation's shape to the implementation. Until 0.12.0 that
//! shape was a line of text, which a person could read and a program could only display.
//! [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) is the HTTP-wide standard for the
//! other half: a JSON object with `type`, `title`, `status` and `detail`, plus whatever the
//! failure knows that a caller could act on — for a syntax error, the line and column.
//!
//! The `type` is a URI under `https://holos.dev/problems/`, and its last segment is the
//! kind [`holos_engine::EngineError::kind`] assigns. A client can match on that where it
//! would otherwise have matched on prose, and the prose is free to improve. `OPERATIONS.md`
//! lists the kinds.
//!
//! The document is sent as `application/problem+json` unless the client asked for
//! `text/plain` and nothing else, in which case it gets the line of text it always did.
//! The console's error tab shows either; a script parsing JSON gets the structure.

use anyhow::Result;
use holos_engine::EngineError;
use tiny_http::Request;

use crate::ui::json_string;

/// Where a problem's `type` URI lives. The page it names is `OPERATIONS.md`'s errors
/// section; the URI is an identifier first and a link second, as the RFC intends.
const TYPE_PREFIX: &str = "https://holos.dev/problems/";

/// The one-line title a kind carries, so a client that shows only the title still says
/// something true. The `detail` says what happened this time.
fn title(kind: &str) -> &'static str {
    match kind {
        "syntax" => "The query does not parse",
        "rdf-parse" => "The RDF does not parse",
        "bad-request" => "The request cannot be answered as sent",
        "unknown-function" => "The query names a function this server does not have",
        "service" => "The query names a SERVICE this server cannot reach",
        "refused" => "The query was refused before it ran",
        "policy" => "Refused by policy",
        "read-only" => "This endpoint is read-only",
        "memory-ceiling" => "The query outgrew the memory ceiling",
        "spill-ceiling" => "The query outgrew the scratch-disk ceiling",
        "timeout" => "The query ran past its time limit",
        "evaluation" => "The query failed while running",
        "internal" => "The server failed",
        _ => "The request failed",
    }
}

/// The document, as bytes.
///
/// `extra` members follow the standard four; each value is already JSON — a number, or a
/// string through [`json_string`].
#[must_use]
pub fn document(kind: &str, status: u16, detail: &str, extra: &[(&str, String)]) -> String {
    let mut out = format!(
        r#"{{"type":{},"title":{},"status":{status},"detail":{}"#,
        json_string(&format!("{TYPE_PREFIX}{kind}")),
        json_string(title(kind)),
        json_string(detail),
    );
    for (name, value) in extra {
        out.push_str(&format!(r#","{name}":{value}"#));
    }
    out.push('}');
    out
}

/// Whether the client asked for plain text and nothing else — the one case that keeps the
/// pre-0.12.0 answer. A client naming JSON, `*/*`, or nothing gets the document.
fn wants_text(accept: Option<&str>) -> bool {
    let Some(accept) = accept else {
        return false;
    };
    let mut names_text = false;
    for item in accept.split(',') {
        let media = item.split(';').next().unwrap_or("").trim();
        if media == "text/plain" {
            names_text = true;
        } else if media == "*/*" || media.ends_with("/json") || media.ends_with("+json") {
            return false;
        }
    }
    names_text
}

/// Answers with a problem of the given kind.
pub fn send(
    request: Request,
    kind: &str,
    status: u16,
    detail: &str,
    extra: &[(&str, String)],
    accept: Option<&str>,
) -> Result<()> {
    if wants_text(accept) {
        return crate::respond(request, status, "text/plain", detail.as_bytes().to_vec());
    }
    let body = document(kind, status, detail, extra);
    crate::respond(
        request,
        status,
        "application/problem+json",
        body.into_bytes(),
    )
}

/// Answers a request that was wrong as sent: a protocol parameter given twice, a body of
/// the wrong type, a dataset the update also names for itself.
pub fn bad_request(request: Request, why: &str, accept: Option<&str>) -> Result<()> {
    send(request, "bad-request", 400, why, &[], accept)
}

/// Answers with the engine's own failure, classified by [`EngineError::kind`].
///
/// A syntax error carries its `line` and `column` as extension members, which is what an
/// editor would highlight; the text of the failure keeps them too, so a client reading
/// only `detail` loses nothing.
pub fn failed(request: Request, error: &EngineError, accept: Option<&str>) -> Result<()> {
    let (kind, status) = error.kind();
    let mut extra = Vec::new();
    if let Some((line, column)) = error.position() {
        extra.push(("line", line.to_string()));
        extra.push(("column", column.to_string()));
    }
    // What a person should read is the whole message for plain text, where there is no
    // title to carry the kind, and the detail alone in the document, where there is.
    if wants_text(accept) {
        return crate::respond(
            request,
            status,
            "text/plain",
            error.to_string().into_bytes(),
        );
    }
    send(request, kind, status, &error.detail(), &extra, accept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_has_the_four_standard_members_and_the_extras() {
        let doc = document(
            "syntax",
            400,
            r#"error at 1:26: expected "}""#,
            &[("line", "1".to_owned()), ("column", "26".to_owned())],
        );
        assert_eq!(
            doc,
            r#"{"type":"https://holos.dev/problems/syntax","title":"The query does not parse","status":400,"detail":"error at 1:26: expected \"}\"","line":1,"column":26}"#
        );
    }

    #[test]
    fn every_kind_the_engine_assigns_has_a_title_of_its_own() {
        for kind in [
            "syntax",
            "rdf-parse",
            "bad-request",
            "unknown-function",
            "service",
            "refused",
            "policy",
            "read-only",
            "memory-ceiling",
            "timeout",
            "evaluation",
            "internal",
        ] {
            assert_ne!(
                title(kind),
                title("no-such-kind"),
                "{kind} falls through to the generic title"
            );
        }
    }

    #[test]
    fn plain_text_is_kept_only_for_a_client_that_asks_for_nothing_else() {
        assert!(wants_text(Some("text/plain")));
        assert!(wants_text(Some("text/plain; q=0.9, text/html")));
        assert!(!wants_text(None));
        assert!(!wants_text(Some("*/*")));
        assert!(!wants_text(Some("text/plain, */*")));
        assert!(!wants_text(Some("application/json")));
        assert!(!wants_text(Some("application/sparql-results+json")));
        assert!(!wants_text(Some(
            "application/problem+json, text/plain;q=0.5"
        )));
    }

    #[test]
    fn a_syntax_error_is_placed_and_a_refusal_is_not() {
        let engine = holos_engine::Engine::new();
        let session = holos_security::Session::open(
            engine.store(),
            holos_security::Principal::anonymous(),
            holos_security::Policy::permit_all(),
        )
        .expect("session");
        let view = engine.view(&session);
        let syntax = holos_engine::Engine::query(&view, "SELECT ?s WHERE { ?s ?p }", None)
            .err()
            .expect("does not parse");
        assert_eq!(syntax.kind(), ("syntax", 400));
        assert_eq!(syntax.position(), Some((1, 26)));
        assert!(!syntax.detail().starts_with("SPARQL syntax error"));
        assert!(syntax.to_string().starts_with("SPARQL syntax error"));

        let refused =
            EngineError::Refused("refusing ORDER BY over an estimated 12 rows".to_owned());
        assert_eq!(refused.kind(), ("refused", 500));
        assert_eq!(refused.position(), None);
    }
}
