//! The [SPARQL 1.1 Graph Store HTTP Protocol](https://www.w3.org/TR/sparql11-http-rdf-update/).
//!
//! REST verbs on whole graphs, for the jobs SPARQL Update makes awkward: fetch a graph as a
//! document, replace one wholesale, drop one. `PUT` in particular is the operation people
//! reach for and then write as `DROP GRAPH … ; INSERT DATA { GRAPH … }`, which is two
//! operations that can half-succeed where this is one that cannot.
//!
//! # Which graph a request means
//!
//! The specification defines two ways of saying, and they are not equally safe:
//!
//! * **Indirect** — the graph is named by a query parameter: `?graph=<encoded IRI>` for a
//!   named graph, or a bare `?default` for the default graph. Unambiguous, and what this
//!   implements.
//! * **Direct** — the request URI *is* the graph name. Only meaningful when the server
//!   knows its own external base URI, and a server behind a reverse proxy usually does not:
//!   it sees `/gsp/x` while the world sees `https://data.example.org/gsp/x`. Guessing would
//!   mint graph names that do not match the ones a client later asks for. It is therefore
//!   off unless `--gsp-base` supplies the base explicitly.
//!
//! # Status codes
//!
//! The specification is specific, and clients depend on the distinctions:
//!
//! | | Exists | Does not exist |
//! |---|---|---|
//! | `GET` / `HEAD` | 200 | **404** |
//! | `PUT` | 204 (replaced) | **201** (created) |
//! | `POST` | 204 (merged) | **201** (created) |
//! | `DELETE` | 204 | **404** |
//!
//! The 201-versus-204 split is how a client learns whether it created the graph, and the
//! 404 on `DELETE` is how it learns the graph was not there — both are answers, not errors.
//!
//! # Policy
//!
//! Every quad read goes through the ordinary [`DatasetView`](holos_engine::view::DatasetView),
//! and every quad written through [`Engine::insert`](holos_engine::Engine::insert), so
//! reading a graph shows what the principal may see and writing one is refused where policy
//! refuses it. `DELETE` and `PUT` require **read** as well as write on what they remove,
//! for the reason [`ACCESS-CONTROL.md`] gives: deletion that ignores read policy is an
//! oracle for whether hidden data exists.

use anyhow::Result;
use holos_engine::Engine;
use holos_security::Session;
use oxrdf::{GraphName, GraphNameRef, NamedNode, Quad};
use oxrdfio::{RdfFormat, RdfSerializer};
use std::collections::HashMap;

/// Which graph a request is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `?default`.
    Default,
    /// `?graph=<iri>`, or a direct request URI.
    Named(NamedNode),
}

impl Target {
    /// The graph name this target denotes.
    #[must_use]
    pub fn graph_name(&self) -> GraphName {
        match self {
            Self::Default => GraphName::DefaultGraph,
            Self::Named(n) => GraphName::NamedNode(n.clone()),
        }
    }
}

/// Why a request could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// Neither `?graph` nor `?default` was given, and direct identification is off.
    Missing,
    /// Both were given, which does not name one graph.
    Both,
    /// `?graph=` was not a valid IRI.
    NotAnIri(String),
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str(
                "name a graph with ?graph=<encoded IRI> or ?default (direct graph \
                 identification needs --gsp-base)",
            ),
            Self::Both => f.write_str("?graph and ?default together do not name one graph"),
            Self::NotAnIri(v) => write!(f, "?graph={v} is not a valid IRI"),
        }
    }
}

/// Works out which graph a request is about.
///
/// `direct_base`, when set, turns the request path into a graph IRI — direct graph
/// identification. It is only consulted when neither query parameter is present, so an
/// explicit `?graph` always wins over an inferred name.
///
/// # Errors
///
/// Returns [`TargetError`] when the request does not name exactly one graph.
pub fn target(
    params: &HashMap<String, String>,
    path: &str,
    direct_base: Option<&str>,
) -> Result<Target, TargetError> {
    let has_default = params.contains_key("default");
    let graph = params.get("graph");

    match (has_default, graph) {
        (true, Some(_)) => Err(TargetError::Both),
        (true, None) => Ok(Target::Default),
        (false, Some(iri)) => NamedNode::new(iri)
            .map(Target::Named)
            .map_err(|_| TargetError::NotAnIri(iri.clone())),
        (false, None) => match direct_base {
            None => Err(TargetError::Missing),
            Some(base) => {
                let joined = format!("{}{}", base.trim_end_matches('/'), path);
                NamedNode::new(&joined)
                    .map(Target::Named)
                    .map_err(|_| TargetError::NotAnIri(joined))
            }
        },
    }
}

/// Whether a graph holds anything the principal may see.
///
/// "Exists" is answered through the policy-filtered view rather than from storage, so a
/// graph a principal may not read is **404** to them rather than 200-and-empty. Reporting
/// it as present-but-empty would confirm its existence, which is the thing the policy was
/// withholding.
///
/// # Errors
///
/// Propagates storage failures.
pub fn exists(engine: &Engine, session: &Session, target: &Target) -> Result<bool> {
    let view = engine.view(session);
    let quads = view.visible_quads(Some(&target.graph_name()))?;
    if !quads.is_empty() {
        return Ok(true);
    }
    // An empty named graph can still exist, if something created it.
    Ok(match target {
        Target::Default => false,
        Target::Named(n) => engine
            .store()
            .contains_named_graph(GraphNameRef::NamedNode(n.as_ref()))?,
    })
}

/// Serialises a graph.
///
/// # Errors
///
/// Propagates storage and serialisation failures.
pub fn read(
    engine: &Engine,
    session: &Session,
    target: &Target,
    format: RdfFormat,
) -> Result<Vec<u8>> {
    let view = engine.view(session);
    let quads = view.visible_quads(Some(&target.graph_name()))?;
    let mut writer = RdfSerializer::from_format(format).for_writer(Vec::new());
    for quad in quads {
        // A graph is served as triples: the graph name is the request, not the payload.
        writer.serialize_triple(oxrdf::TripleRef {
            subject: quad.subject.as_ref(),
            predicate: quad.predicate.as_ref(),
            object: quad.object.as_ref(),
        })?;
    }
    Ok(writer.finish()?)
}

/// Removes every quad in a graph, subject to policy.
///
/// Returns how many were removed.
///
/// # Errors
///
/// Propagates policy refusals and storage failures.
pub fn clear(engine: &mut Engine, session: &mut Session, target: &Target) -> Result<u64> {
    // Collected first: the store cannot be mutated while a scan borrows it, and going
    // through the view is what keeps deletion inside what the principal may read.
    let quads = {
        let view = engine.view(session);
        view.visible_quads(Some(&target.graph_name()))?
    };
    in_commit(engine, |engine| {
        let mut removed = 0;
        for quad in quads {
            if engine.remove(session, quad.as_ref())? {
                removed += 1;
            }
        }
        Ok(removed)
    })
}

/// Splits a `multipart/form-data` body into its typed parts.
///
/// The Graph Store Protocol allows a graph to be submitted as a file upload, which is what
/// a browser form produces. The body is a sequence of parts separated by a boundary, each
/// with its own headers.
///
/// **Every** part is returned, not just the first: a multipart upload carries one RDF
/// document per part, each with its own prefix declarations, and the specification's own
/// example sends two. Returning one would silently store half the graph.
///
/// Parts with no `Content-Type` are skipped — a form's ordinary text fields look like that,
/// and they are not RDF. An empty result means the caller should answer 415, the same
/// answer an unparseable single-part body gets.
#[must_use]
pub fn multipart_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<(&'a [u8], String)> {
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut start = 0;
    while let Some(at) = find(&body[start..], delimiter.as_bytes()) {
        let at = start + at;
        if start > 0 {
            // The two bytes before a boundary are the CRLF that terminates the part.
            parts.push(&body[start..at.saturating_sub(2).max(start)]);
        }
        start = at + delimiter.len();
        // `--boundary--` ends the body.
        if body[start..].starts_with(b"--") {
            break;
        }
        // Skip the CRLF after the boundary line.
        if body[start..].starts_with(b"\r\n") {
            start += 2;
        } else if body[start..].starts_with(b"\n") {
            start += 1;
        }
    }

    let mut out = Vec::new();
    for part in parts {
        // A part is its own little message: headers, a blank line, then content.
        let Some((at, width)) = find(part, b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| find(part, b"\n\n").map(|i| (i, 2)))
        else {
            continue;
        };
        let (headers, content) = (&part[..at], &part[at + width..]);
        let headers = String::from_utf8_lossy(headers);
        let media = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        });
        if let Some(media) = media {
            out.push((content, media));
        }
    }
    out
}

/// The `boundary=` parameter of a `multipart/form-data` content type.
#[must_use]
pub fn multipart_boundary(content_type: &str) -> Option<String> {
    if !content_type
        .split(';')
        .next()?
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    content_type.split(';').find_map(|p| {
        let (name, value) = p.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Mints a name for a graph the client asked the store to create.
///
/// `POST` to the protocol endpoint with no target means "create a graph and tell me its
/// name" — Graph Store Protocol §5.5. The name has to be one the client can then address,
/// so it is built under the configured base and returned in `Location`.
#[must_use]
pub fn mint_graph_name(base: &str, path: &str) -> NamedNode {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    NamedNode::new_unchecked(format!("{}{}/{stamp:x}", base.trim_end_matches('/'), path))
}

/// Removes a graph entirely: its quads *and* its entry in the catalogue.
///
/// The difference from [`clear`] is what `DELETE` means. Emptying a graph leaves it
/// existing-but-empty, so a second `DELETE` answers 204 where the specification says 404 —
/// and a client cannot tell "I removed it" from "it was not there". `PUT` registers the
/// graph, so without this the distinction is lost the moment anything is written.
///
/// # Errors
///
/// Propagates policy refusals and storage failures.
pub fn drop_graph(engine: &mut Engine, session: &mut Session, target: &Target) -> Result<u64> {
    // Emptying the graph and removing its name are two writes, and between them the graph is
    // in exactly the existing-but-empty state this function exists to avoid.
    in_commit(engine, |engine| {
        let removed = clear(engine, session, target)?;
        if let Target::Named(name) = target {
            engine
                .store_mut()
                .remove_named_graph(GraphNameRef::NamedNode(name.as_ref()))?;
        }
        Ok(removed)
    })
}

/// Runs `f` inside a commit scope, joining the caller's if one is already open.
///
/// Joining rather than nesting is what lets the HTTP handler wrap a whole `PUT` — a clear
/// and one merge per uploaded document — in a single commit while each of those functions
/// stays atomic when called on its own.
fn in_commit<T>(engine: &mut Engine, f: impl FnOnce(&mut Engine) -> Result<T>) -> Result<T> {
    let owned = !engine.store().in_scope();
    if owned {
        engine.store_mut().begin()?;
    }
    let result = f(engine);
    if owned {
        if result.is_ok() {
            engine.store_mut().commit()?;
        } else {
            engine.store_mut().rollback();
        }
    }
    result
}

/// Parses a body and merges it into a graph, subject to policy.
///
/// Returns how many quads were added.
///
/// # Errors
///
/// Propagates parse failures, policy refusals and storage failures.
pub fn merge(
    engine: &mut Engine,
    session: &mut Session,
    target: &Target,
    body: &[u8],
    format: RdfFormat,
    base_iri: Option<&str>,
) -> Result<u64> {
    let graph = target.graph_name();
    let mut parser = oxrdfio::RdfParser::from_format(format);
    if let Some(base) = base_iri {
        parser = parser
            .with_base_iri(base)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // Parsed in full before anything is written, so a body that fails to parse half way
    // through does not leave half a graph behind.
    let mut quads = Vec::new();
    for quad in parser.for_slice(body) {
        let quad = quad?;
        quads.push(Quad {
            subject: quad.subject,
            predicate: quad.predicate,
            object: quad.object,
            graph_name: graph.clone(),
        });
    }

    // Parsing in full above stops a malformed body leaving half a graph. It does not stop a
    // *policy* refusal doing the same: the quads are legal RDF and the five hundredth is the
    // one the principal may not write. The scope covers both.
    in_commit(engine, |engine| {
        let mut added = 0;
        for quad in quads {
            if engine.insert(session, quad.as_ref())? {
                added += 1;
            }
        }
        Ok(added)
    })
}

/// Applies the body of a `PUT` or a `POST`, atomically.
///
/// `PUT` replaces and `POST` merges, and that difference is the whole distinction between
/// the two verbs. It lives here rather than in the HTTP handler because the interesting part
/// is not the verb, it is that **replacing is two operations and must not be observable, or
/// interruptible, between them**.
///
/// A `PUT` clears the graph and then parses. So a body that turns out to be malformed, or a
/// quad the principal may not write, used to leave the graph emptied and unreplaced — with
/// the client holding a 400 telling it the request had failed. Deleting someone's data is
/// not an acceptable outcome of a request that failed. The scope makes the clear and the
/// merges land together or not at all.
///
/// Several documents because a graph submitted as a file upload arrives as
/// `multipart/form-data` with one per part. A `PUT` clears once, not once per part, or the
/// parts would replace one another instead of accumulating.
///
/// # Errors
///
/// Propagates parse failures, policy refusals and storage failures — having left the graph
/// as it was.
pub fn write(
    engine: &mut Engine,
    session: &mut Session,
    target: &Target,
    documents: &[(Vec<u8>, RdfFormat)],
    replace: bool,
) -> Result<u64> {
    in_commit(engine, |engine| {
        if replace {
            clear(engine, session, target)?;
        }
        let mut added = 0;
        for (content, format) in documents {
            added += merge(engine, session, target, content, *format, None)?;
        }
        // Registers the graph, so a `PUT` of an empty body still creates one and a later
        // `DELETE` can answer 404 rather than "it was never here".
        create(engine, target)?;
        Ok(added)
    })
}

/// Ensures a named graph exists even with nothing in it.
///
/// # Errors
///
/// Propagates storage failures.
pub fn create(engine: &mut Engine, target: &Target) -> Result<()> {
    if let Target::Named(name) = target {
        engine
            .store_mut()
            .insert_named_graph(&GraphName::NamedNode(name.clone()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_graph_parameter_names_a_named_graph() {
        assert_eq!(
            target(&params(&[("graph", "http://example.org/g")]), "/gsp", None),
            Ok(Target::Named(NamedNode::new_unchecked(
                "http://example.org/g"
            )))
        );
    }

    #[test]
    fn default_names_the_default_graph() {
        assert_eq!(
            target(&params(&[("default", "")]), "/gsp", None),
            Ok(Target::Default)
        );
    }

    #[test]
    fn both_together_are_refused() {
        // They do not name one graph, and picking one would be a guess about intent.
        assert_eq!(
            target(
                &params(&[("default", ""), ("graph", "http://example.org/g")]),
                "/gsp",
                None
            ),
            Err(TargetError::Both)
        );
    }

    #[test]
    fn neither_is_refused_unless_direct_identification_is_configured() {
        assert_eq!(
            target(&params(&[]), "/gsp/x", None),
            Err(TargetError::Missing)
        );
        assert_eq!(
            target(&params(&[]), "/gsp/x", Some("http://data.example.org")),
            Ok(Target::Named(NamedNode::new_unchecked(
                "http://data.example.org/gsp/x"
            )))
        );
    }

    #[test]
    fn an_explicit_graph_beats_direct_identification() {
        // Otherwise a server with a base configured could never be asked about a graph
        // whose name does not live under that base.
        assert_eq!(
            target(
                &params(&[("graph", "http://elsewhere/g")]),
                "/gsp/x",
                Some("http://data.example.org")
            ),
            Ok(Target::Named(NamedNode::new_unchecked(
                "http://elsewhere/g"
            )))
        );
    }

    #[test]
    fn a_malformed_graph_iri_is_reported_as_such() {
        assert_eq!(
            target(&params(&[("graph", "not an iri")]), "/gsp", None),
            Err(TargetError::NotAnIri("not an iri".to_owned()))
        );
    }

    #[test]
    fn a_multipart_boundary_is_recognised() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=abc123"),
            Some("abc123".to_owned())
        );
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"quoted\""),
            Some("quoted".to_owned())
        );
        // Not multipart at all.
        assert_eq!(multipart_boundary("text/turtle"), None);
    }

    #[test]
    fn the_rdf_part_of_a_multipart_body_is_found() {
        let body = b"--B\r\nContent-Disposition: form-data; name=\"f\"\r\nContent-Type: text/turtle\r\n\r\n<http://a> <http://b> <http://c> .\r\n--B--\r\n";
        let parts = multipart_parts(body, "B");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1, "text/turtle");
        assert!(
            String::from_utf8_lossy(parts[0].0).contains("http://a"),
            "got {:?}",
            String::from_utf8_lossy(parts[0].0)
        );
    }

    #[test]
    fn every_part_of_a_multipart_body_is_returned() {
        // The bug this pins: taking only the first part stored half the graph, and the
        // request still answered 204, so nothing said data had been dropped.
        let body = b"--B\r\nContent-Type: text/turtle\r\n\r\n<http://a> <http://p> \"one\" .\r\n\r\n--B\r\nContent-Type: text/turtle\r\n\r\n<http://a> <http://q> \"two\" .\r\n\r\n--B--\r\n";
        let parts = multipart_parts(body, "B");
        assert_eq!(parts.len(), 2, "both documents belong in the graph");
        assert!(String::from_utf8_lossy(parts[0].0).contains("one"));
        assert!(String::from_utf8_lossy(parts[1].0).contains("two"));
    }

    #[test]
    fn a_multipart_body_with_no_typed_part_yields_nothing() {
        // Which the caller turns into 415, the same answer an unparseable body gets.
        let body = b"--B\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nplain\r\n--B--\r\n";
        assert!(multipart_parts(body, "B").is_empty());
    }

    #[test]
    fn a_minted_graph_name_is_under_the_base_and_unique() {
        let a = mint_graph_name("http://www.example", "/gsp");
        let b = mint_graph_name("http://www.example", "/gsp");
        assert!(a.as_str().starts_with("http://www.example/gsp/"), "{a}");
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn the_base_is_joined_without_doubling_a_slash() {
        assert_eq!(
            target(&params(&[]), "/x", Some("http://e/")),
            Ok(Target::Named(NamedNode::new_unchecked("http://e/x")))
        );
    }
}
