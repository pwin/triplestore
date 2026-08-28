//! Reading W3C test manifests.
//!
//! Every suite in `w3c/rdf-tests` is described by a `manifest.ttl` that either lists test
//! entries (`mf:entries`, an RDF collection) or includes other manifests (`mf:include`,
//! also a collection). Walking that is all this module does.
//!
//! # The two bases
//!
//! A manifest is read with a **`file://` base**, so `mf:action <agg01.rq>` resolves to
//! something that can be opened. But the *content* of a test must be parsed against the
//! manifest's `mf:assumedTestBase` — the `https://w3c.github.io/...` URL the expected
//! output was generated with — or every relative IRI in the data resolves to a local path
//! and nothing matches. Both are carried on every [`TestEntry`].

use anyhow::{anyhow, bail, Context, Result};
use oxrdf::vocab::rdf;
use oxrdf::{Graph, NamedNode, NamedOrBlankNodeRef, Term, TermRef};
use oxrdfio::{RdfFormat, RdfParser};
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const QT: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-query#";
const SD: &str = "http://www.w3.org/ns/sparql-service-description#";

fn mf(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{MF}{local}"))
}

fn qt(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{QT}{local}"))
}

/// The update-test vocabulary, which is separate from `qt:` and shaped differently: the
/// action names a *request* rather than a query, and the result is a whole dataset rather
/// than a result set.
fn ut(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!(
        "http://www.w3.org/2009/sparql/tests/test-update#{local}"
    ))
}

fn sd(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SD}{local}"))
}

/// One test, with everything needed to run it.
#[derive(Debug, Clone)]
pub struct TestEntry {
    /// The test's own IRI — the stable name used in the known-failures ratchet.
    pub id: String,
    /// `rdf:type` of the entry, which decides how it is run.
    pub kind: String,
    /// `mf:name`.
    pub name: String,
    /// Suite label, derived from the manifest's directory. Used only for reporting.
    pub suite: String,
    /// `mf:action` when it is a plain file — the syntax and eval suites use this.
    pub action: Option<PathBuf>,
    /// `qt:query` — SPARQL suites.
    pub query: Option<PathBuf>,
    /// `qt:data` — the default graph.
    pub data: Option<PathBuf>,
    /// `qt:graphData` — named graphs, paired with the IRI that names them.
    pub graph_data: Vec<(String, PathBuf)>,
    /// `mf:result`.
    pub result: Option<PathBuf>,
    /// `ut:request` — the update text, for `UpdateEvaluationTest`.
    pub update_request: Option<PathBuf>,
    /// The scripted HTTP conversation, for the protocol suites.
    ///
    /// Present only on `GraphStoreProtocolTest` and `ProtocolTest`, whose `mf:action` is
    /// an `ht:Connection` rather than a query.
    pub script: Option<crate::protocol::Script>,
    /// `qt:serviceData` — federated endpoints, as `(endpoint IRI, local data file)`.
    ///
    /// The federated-query suite does not require a live endpoint: each `SERVICE` target
    /// is given an IRI and a file standing in for what it would return. So the tests are
    /// about the *evaluation* of `SERVICE`, not about HTTP.
    pub service_data: Vec<(String, PathBuf)>,
    /// Named graphs of the *expected* dataset, paired with the IRI naming each.
    pub result_graph_data: Vec<(String, PathBuf)>,
    /// The expected dataset's default graph.
    pub result_data: Option<PathBuf>,
    /// The base IRI test content must be parsed against. See the module note.
    pub base: String,
    /// The `sd:entailmentRegime` IRIs the test declares, if any.
    ///
    /// A list rather than a flag, because *which* regime it asks for decides whether this
    /// engine can run it at all: RDFS is materialisable here, OWL-Direct is not, and a
    /// single boolean cannot tell the two apart.
    pub entailment_regimes: Vec<String>,
}

impl TestEntry {
    /// Whether the test asks for an entailment regime at all.
    #[must_use]
    pub fn needs_entailment(&self) -> bool {
        !self.entailment_regimes.is_empty()
    }

    /// Whether RDFS materialisation answers the regime this test asks for.
    ///
    /// A test names every regime it holds under, so RDFS appearing anywhere in the list is
    /// enough: the expected answers are the same under all of them. `ent:RDF` is weaker than
    /// RDFS, so a materialised RDFS closure entails everything it asks for and possibly more
    /// — which is why an RDF-only test is *not* accepted here, since "possibly more" is
    /// exactly what would break an exact result comparison.
    #[must_use]
    pub fn rdfs_entailment_suffices(&self) -> bool {
        self.entailment_regimes
            .iter()
            .any(|r| r.ends_with("/entailment/RDFS"))
    }

    /// Short local name, for readable failure output.
    #[must_use]
    pub fn short_id(&self) -> &str {
        self.id.rsplit(['#', '/']).next().unwrap_or(&self.id)
    }
}

/// Reads a manifest and everything it includes.
pub fn load(manifest: &Path) -> Result<Vec<TestEntry>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    load_into(manifest, &mut out, &mut seen)?;
    Ok(out)
}

fn load_into(manifest: &Path, out: &mut Vec<TestEntry>, seen: &mut HashSet<PathBuf>) -> Result<()> {
    let manifest = manifest
        .canonicalize()
        .with_context(|| format!("resolving {}", manifest.display()))?;
    if !seen.insert(manifest.clone()) {
        return Ok(());
    }
    let dir = manifest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", manifest.display()))?;
    let file_base = path_to_file_url(&manifest);

    let graph = parse_graph(&manifest, RdfFormat::Turtle, &file_base)
        .with_context(|| format!("parsing manifest {}", manifest.display()))?;

    let Some(root) = graph
        .subjects_for_predicate_object(rdf::TYPE, &Term::from(mf("Manifest")))
        .next()
    else {
        bail!("{} declares no mf:Manifest", manifest.display());
    };

    // Every test's content is parsed against this, not against the file:// base.
    let assumed_base = object(&graph, root, &mf("assumedTestBase"))
        .and_then(term_as_iri)
        .unwrap_or_else(|| file_base.clone());

    let suite = dir
        .file_name()
        .map_or_else(|| "?".to_owned(), |n| n.to_string_lossy().into_owned());

    for included in object(&graph, root, &mf("include"))
        .map(|t| collection(&graph, t))
        .transpose()?
        .unwrap_or_default()
    {
        let Some(iri) = term_as_iri(included) else {
            continue;
        };
        let Some(path) = file_url_to_path(&iri) else {
            continue;
        };
        load_into(&path, out, seen)?;
    }

    for entry in object(&graph, root, &mf("entries"))
        .map(|t| collection(&graph, t))
        .transpose()?
        .unwrap_or_default()
    {
        let TermRef::NamedNode(id) = entry.as_ref() else {
            continue;
        };
        let subject = NamedOrBlankNodeRef::from(id);
        let Some(kind) = object(&graph, subject, &rdf::TYPE.into_owned()).and_then(term_as_iri)
        else {
            continue;
        };

        let mut test = TestEntry {
            id: id.as_str().to_owned(),
            kind,
            name: object(&graph, subject, &mf("name"))
                .map_or_else(String::new, |t| literal_value(&t)),
            suite: suite.clone(),
            action: None,
            query: None,
            data: None,
            graph_data: Vec::new(),
            result: None,
            update_request: None,
            service_data: Vec::new(),
            script: None,
            result_graph_data: Vec::new(),
            result_data: None,
            base: assumed_base.clone(),
            entailment_regimes: Vec::new(),
        };

        match object(&graph, subject, &mf("action")) {
            // A plain file: the RDF syntax and eval suites, and SPARQL syntax tests.
            Some(Term::NamedNode(n)) => test.action = file_url_to_path(n.as_str()),
            // A blank node bundling query, data and named graphs: SPARQL evaluation, and
            // — with the ut: vocabulary instead of qt: — update evaluation.
            Some(Term::BlankNode(b)) => {
                let action = NamedOrBlankNodeRef::from(b.as_ref());
                test.query = object(&graph, action, &qt("query"))
                    .and_then(term_as_iri)
                    .and_then(|s| file_url_to_path(&s));
                test.update_request = object(&graph, action, &ut("request"))
                    .and_then(term_as_iri)
                    .and_then(|s| file_url_to_path(&s));
                test.data = object(&graph, action, &qt("data"))
                    .or_else(|| object(&graph, action, &ut("data")))
                    .and_then(term_as_iri)
                    .and_then(|s| file_url_to_path(&s));
                collect_graph_data(&graph, action, &assumed_base, &mut test.graph_data);
                collect_service_data(&graph, action, &mut test.service_data);
                // A protocol test's action is an ht:Connection. Reading it here keeps the
                // runner from having to re-parse the manifest.
                if object(
                    &graph,
                    action,
                    &NamedNode::new_unchecked("http://www.w3.org/2011/http#requests"),
                )
                .is_some()
                {
                    test.script = crate::protocol::read_script(&graph, action).ok();
                }
                // `sd:entailmentRegime` is one IRI or an RDF list of them, and a test
                // holds under any it names.
                if let Some(term) = object(&graph, action, &sd("entailmentRegime")) {
                    test.entailment_regimes = match &term {
                        Term::NamedNode(n) => vec![n.as_str().to_owned()],
                        _ => collection(&graph, term)
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(term_as_iri)
                            .collect(),
                    };
                }
            }
            _ => {}
        }
        // A SPARQL Protocol test hangs its `ut:graphData` off the *test*, not the action:
        // the graphs are the server's dataset, set up before the conversation starts,
        // rather than an argument to any one request.
        collect_graph_data(&graph, subject, &assumed_base, &mut test.graph_data);

        match object(&graph, subject, &mf("result")) {
            // A plain file: a result set or a graph.
            Some(Term::NamedNode(n)) => test.result = file_url_to_path(n.as_str()),
            // A blank node: the expected *dataset* of an update test.
            Some(Term::BlankNode(b)) => {
                let result = NamedOrBlankNodeRef::from(b.as_ref());
                test.result_data = object(&graph, result, &ut("data"))
                    .and_then(term_as_iri)
                    .and_then(|s| file_url_to_path(&s));
                collect_graph_data(&graph, result, &assumed_base, &mut test.result_graph_data);
            }
            _ => {}
        }

        out.push(test);
    }
    Ok(())
}

/// Reads `qt:serviceData` from an action node.
///
/// Each entry is a blank node carrying `qt:endpoint` — the IRI the query names — and
/// `qt:data`, the file standing in for that endpoint's contents.
fn collect_service_data(
    graph: &Graph,
    node: NamedOrBlankNodeRef<'_>,
    out: &mut Vec<(String, PathBuf)>,
) {
    for entry in graph.objects_for_subject_predicate(node, qt("serviceData").as_ref()) {
        let inner = match entry {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::NamedNode(n),
            TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
            _ => continue,
        };
        let Some(endpoint) = object(graph, inner, &qt("endpoint")).and_then(term_as_iri) else {
            continue;
        };
        let Some(path) = object(graph, inner, &qt("data"))
            .and_then(term_as_iri)
            .and_then(|s| file_url_to_path(&s))
        else {
            continue;
        };
        out.push((endpoint, path));
    }
}

/// Reads `qt:graphData` / `ut:graphData` from an action or result node.
///
/// Two shapes occur. The query suites point straight at a file, and the graph is named by
/// the URL the expected results use — the assumed base plus the file name, not the local
/// path. The update suites wrap it in a blank node carrying `ut:graph` and an
/// `rdfs:label` holding the graph IRI, and there the label is authoritative.
fn collect_graph_data(
    graph: &Graph,
    node: NamedOrBlankNodeRef<'_>,
    assumed_base: &str,
    out: &mut Vec<(String, PathBuf)>,
) {
    for predicate in [qt("graphData"), ut("graphData")] {
        for g in graph.objects_for_subject_predicate(node, predicate.as_ref()) {
            match g {
                TermRef::NamedNode(n) => {
                    if let Some(path) = file_url_to_path(n.as_str()) {
                        out.push((rebase(assumed_base, &path), path));
                    }
                }
                TermRef::BlankNode(b) => {
                    let inner = NamedOrBlankNodeRef::from(b);
                    let Some(path) = object(graph, inner, &ut("graph"))
                        .and_then(term_as_iri)
                        .and_then(|s| file_url_to_path(&s))
                    else {
                        continue;
                    };
                    let label = object(graph, inner, &rdfs("label"))
                        .and_then(|t| match t {
                            Term::Literal(l) => Some(l.value().to_owned()),
                            other => term_as_iri(other),
                        })
                        .unwrap_or_else(|| rebase(assumed_base, &path));
                    out.push((label, path));
                }
                _ => {}
            }
        }
    }
}

fn rdfs(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://www.w3.org/2000/01/rdf-schema#{local}"))
}

fn parse_graph(path: &Path, format: RdfFormat, base: &str) -> Result<Graph> {
    let mut graph = Graph::new();
    let parser = RdfParser::from_format(format)
        .with_base_iri(base)
        .map_err(|e| anyhow!("bad base IRI {base}: {e}"))?;
    for quad in parser.for_reader(BufReader::new(File::open(path)?)) {
        let quad = quad?;
        graph.insert(&oxrdf::Triple {
            subject: quad.subject,
            predicate: quad.predicate,
            object: quad.object,
        });
    }
    Ok(graph)
}

fn object(graph: &Graph, subject: NamedOrBlankNodeRef<'_>, predicate: &NamedNode) -> Option<Term> {
    graph
        .object_for_subject_predicate(subject, predicate.as_ref())
        .map(TermRef::into_owned)
}

/// Walks an RDF collection (`rdf:first` / `rdf:rest` / `rdf:nil`).
fn collection(graph: &Graph, head: Term) -> Result<Vec<Term>> {
    let mut out = Vec::new();
    let mut node = head;
    let nil = Term::from(rdf::NIL.into_owned());
    // A malformed manifest must not spin forever.
    for _ in 0..100_000 {
        if node == nil {
            return Ok(out);
        }
        let subject = match node.as_ref() {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::from(n),
            TermRef::BlankNode(b) => NamedOrBlankNodeRef::from(b),
            _ => bail!("collection cell is not a node"),
        };
        let Some(first) = object(graph, subject, &rdf::FIRST.into_owned()) else {
            bail!("collection cell has no rdf:first");
        };
        out.push(first);
        let Some(rest) = object(graph, subject, &rdf::REST.into_owned()) else {
            bail!("collection cell has no rdf:rest");
        };
        node = rest;
    }
    bail!("collection did not terminate")
}

fn term_as_iri(term: Term) -> Option<String> {
    match term {
        Term::NamedNode(n) => Some(n.into_string()),
        _ => None,
    }
}

fn literal_value(term: &Term) -> String {
    match term {
        Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    }
}

/// `<assumed base directory>/<file name>` — the IRI a test file is known by.
fn rebase(assumed_base: &str, path: &Path) -> String {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let dir = assumed_base
        .rsplit_once('/')
        .map_or(assumed_base, |(head, _)| head);
    format!("{dir}/{name}")
}

/// Converts a filesystem path into a `file://` URL.
///
/// Windows paths need the backslashes flipped and a slash before the drive letter, which
/// is why this is not just string concatenation.
#[must_use]
pub fn path_to_file_url(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s); // Windows extended-length prefix
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Converts a `file://` URL back into a path, undoing percent-encoding.
#[must_use]
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Infers an RDF format from a file extension.
#[must_use]
pub fn format_for(path: &Path) -> Option<RdfFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "ttl" => Some(RdfFormat::Turtle),
        "nt" => Some(RdfFormat::NTriples),
        "trig" => Some(RdfFormat::TriG),
        "nq" => Some(RdfFormat::NQuads),
        "rdf" | "owl" => Some(RdfFormat::RdfXml),
        "n3" => Some(RdfFormat::N3),
        "jsonld" => Some(RdfFormat::JsonLd {
            profile: oxrdfio::JsonLdProfileSet::empty(),
        }),
        _ => None,
    }
}

/// Reads an RDF file into a dataset, parsed against `base`.
pub fn parse_dataset(path: &Path, base: &str) -> Result<oxrdf::Dataset> {
    let format = format_for(path).ok_or_else(|| anyhow!("no RDF format for {}", path.display()))?;
    let parser = RdfParser::from_format(format)
        .with_base_iri(base)
        .map_err(|e| anyhow!("bad base IRI {base}: {e}"))?;
    let mut dataset = oxrdf::Dataset::new();
    for quad in parser.for_reader(BufReader::new(File::open(path)?)) {
        dataset.insert(&quad?);
    }
    Ok(dataset)
}

/// The IRI a test file is known by, for use as a parse base.
#[must_use]
pub fn base_for(test: &TestEntry, path: &Path) -> String {
    rebase(&test.base, path)
}
