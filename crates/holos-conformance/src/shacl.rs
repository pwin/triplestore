//! The W3C SHACL test suite.
//!
//! These manifests are shaped differently from the RDF and SPARQL ones: each test is a
//! single Turtle file holding the data graph, the shapes graph *and* the expected
//! validation report, and the manifest just chains them with `mf:include`. So this reader
//! is separate rather than a special case bolted onto `super::manifest`.
//!
//! Comparison is up to blank-node isomorphism, which matters more here than elsewhere: a
//! validation report is mostly blank nodes, and the labels are arbitrary.

use anyhow::{anyhow, Result};
use holos_shacl::{CompiledShapes, Options};
use holos_store::{GraphFilter, Store};
use oxrdf::dataset::CanonicalizationAlgorithm;
use oxrdf::vocab::rdf;
use oxrdf::{Dataset, Graph, GraphName, NamedNode, NamedOrBlankNodeRef, Quad, Term, TermRef};
use oxrdfio::{RdfFormat, RdfParser};
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const SHT: &str = "http://www.w3.org/ns/shacl-test#";

fn mf(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{MF}{local}"))
}

fn sht(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SHT}{local}"))
}

/// One SHACL test: a file, and the node inside it that describes the test.
#[derive(Debug, Clone)]
pub struct ShaclTest {
    /// A stable name, for the ratchet.
    pub id: String,
    /// The Turtle file holding data, shapes and expected report.
    pub path: PathBuf,
}

/// Collects every test a manifest chains together.
pub fn load(manifest: &Path) -> Result<Vec<ShaclTest>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    collect(manifest, &mut out, &mut seen)?;
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn collect(manifest: &Path, out: &mut Vec<ShaclTest>, seen: &mut HashSet<PathBuf>) -> Result<()> {
    let manifest = manifest.canonicalize()?;
    if !seen.insert(manifest.clone()) {
        return Ok(());
    }
    let base = super::manifest::path_to_file_url(&manifest);
    let graph = parse_graph(&manifest, &base)?;

    let Some(root) = graph
        .subjects_for_predicate_object(rdf::TYPE, &Term::from(mf("Manifest")))
        .next()
    else {
        return Ok(());
    };

    // `mf:include` is a plain repeated property here, not an RDF collection.
    let mut included: Vec<String> = graph
        .objects_for_subject_predicate(root, mf("include").as_ref())
        .filter_map(|o| match o {
            TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
            _ => None,
        })
        .collect();
    included.sort();

    for iri in included {
        let Some(path) = super::manifest::file_url_to_path(&iri) else {
            continue;
        };
        if path.file_name().is_some_and(|n| n == "manifest.ttl") {
            collect(&path, out, seen)?;
        } else if is_test_file(&path)? {
            let id = test_id(&path);
            out.push(ShaclTest { id, path });
        }
    }
    Ok(())
}

fn is_test_file(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let base = super::manifest::path_to_file_url(path);
    let graph = parse_graph(path, &base)?;
    let validate = Term::from(sht("Validate"));
    let found = graph
        .subjects_for_predicate_object(rdf::TYPE, &validate)
        .next()
        .is_some();
    Ok(found)
}

/// A stable id: the last two path segments, which is what the suite names tests by.
fn test_id(path: &Path) -> String {
    let file = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let dir = path
        .parent()
        .and_then(Path::file_name)
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    format!("{dir}/{file}")
}

fn parse_graph(path: &Path, base: &str) -> Result<Graph> {
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri(base)
        .map_err(|e| anyhow!("bad base {base}: {e}"))?;
    let mut graph = Graph::new();
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

/// Which validator a SHACL run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// The native evaluator, reading the live store.
    Native,
    /// The adapted SHACL_Engine, fed through the bridge.
    Adapted,
}

/// Runs one SHACL test with the native evaluator.
#[must_use]
pub fn run(test: &ShaclTest) -> super::Outcome {
    run_with(test, Engine::Native)
}

/// Runs one SHACL test with a chosen validator.
#[must_use]
pub fn run_with(test: &ShaclTest, engine: Engine) -> super::Outcome {
    match run_inner(test, engine) {
        Ok(outcome) => outcome,
        Err(e) => super::Outcome::Failed(format!("{e:#}")),
    }
}

fn run_inner(test: &ShaclTest, engine: Engine) -> Result<super::Outcome> {
    let base = super::manifest::path_to_file_url(&test.path);
    let graph = parse_graph(&test.path, &base)?;

    let Some(entry) = graph
        .subjects_for_predicate_object(rdf::TYPE, &Term::from(sht("Validate")))
        .next()
    else {
        return Ok(super::Outcome::skip("no sht:Validate entry"));
    };

    let expected = expected_report(&graph, entry)?;

    // Most tests put data and shapes in the same file, referenced as `<>`. Some point at
    // separate files, and then the two graphs have to stay apart — which is exactly the
    // per-graph selection §8 adds, exercised here for free.
    let action = graph
        .object_for_subject_predicate(entry, mf("action").as_ref())
        .ok_or_else(|| anyhow!("test has no mf:action"))?;
    let action_node = match action {
        TermRef::BlankNode(b) => NamedOrBlankNodeRef::from(b),
        TermRef::NamedNode(n) => NamedOrBlankNodeRef::from(n),
        _ => return Err(anyhow!("mf:action is not a node")),
    };
    let data_iri = graph_ref(&graph, action_node, "dataGraph");
    let shapes_iri = graph_ref(&graph, action_node, "shapesGraph");

    let mut store = Store::new();
    let separate = data_iri.is_some() && shapes_iri.is_some() && data_iri != shapes_iri;

    load_into(&mut store, &resolve(&test.path, data_iri.as_deref()), None)?;
    let shapes_graph_name = if separate {
        let name = NamedNode::new_unchecked("urn:holos:conformance:shapes");
        load_into(
            &mut store,
            &resolve(&test.path, shapes_iri.as_deref()),
            Some(name.clone()),
        )?;
        GraphFilter::Named(
            store
                .lookup_term(name.as_ref().into())?
                .ok_or_else(|| anyhow!("shapes graph name did not intern"))?,
        )
    } else {
        GraphFilter::Default
    };

    let options = Options {
        data_graph: GraphFilter::Default,
        shapes_graph: shapes_graph_name,
    };
    if engine == Engine::Adapted {
        return run_adapted(&store, options, &expected);
    }

    let shapes = match CompiledShapes::compile(&store, options) {
        Ok(s) => s,
        Err(holos_shacl::ShaclError::Unsupported(what)) => {
            return Ok(super::Outcome::skip(format!("unsupported: {what}")))
        }
        Err(e) => return Ok(super::Outcome::fail(format!("compiling shapes: {e}"))),
    };
    let report = match shapes.validate(&store) {
        Ok(r) => r,
        Err(holos_shacl::ShaclError::Unsupported(what)) => {
            return Ok(super::Outcome::skip(format!("unsupported: {what}")))
        }
        Err(e) => return Ok(super::Outcome::fail(format!("validating: {e}"))),
    };
    let actual = to_dataset(shapes.report_to_quads(&store, &report)?);

    Ok(match compare(&expected, &actual) {
        Ok(()) => super::Outcome::Passed,
        Err(diff) => super::Outcome::Failed(diff),
    })
}

/// Runs a test through the adapted engine, bridged from the store.
fn run_adapted(
    store: &Store,
    options: Options,
    expected: &Dataset,
) -> Result<super::Outcome> {
    let mut run = match holos_shacl::engine::EngineRun::prepare(store, options) {
        Ok(r) => r,
        Err(e) => return Ok(super::Outcome::fail(format!("preparing the engine: {e}"))),
    };
    let report = match run.validate() {
        Ok(r) => r,
        Err(e) => return Ok(super::Outcome::fail(format!("validating: {e}"))),
    };
    let graph = run.report_to_oxrdf(&report);
    let mut actual = Dataset::new();
    for triple in graph.iter() {
        actual.insert(&Quad {
            subject: triple.subject.into_owned(),
            predicate: triple.predicate.into_owned(),
            object: triple.object.into_owned(),
            graph_name: GraphName::DefaultGraph,
        });
    }
    Ok(match compare(expected, &actual) {
        Ok(()) => super::Outcome::Passed,
        Err(diff) => super::Outcome::Failed(diff),
    })
}

/// Whether a predicate carries report structure rather than pointing into the data.
///
/// Everything a validation report *contains* — its results, and the property-path
/// expressions inside them — versus everything it merely *references*: the focus node, the
/// value that failed, the shape that reported it.
fn is_structural(predicate: &str) -> bool {
    const SH: &str = "http://www.w3.org/ns/shacl#";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    matches!(
        predicate,
        p if p == format!("{SH}result")
            || p == format!("{SH}resultPath")
            || p == format!("{SH}detail")
            || p == format!("{SH}inversePath")
            || p == format!("{SH}alternativePath")
            || p == format!("{SH}zeroOrMorePath")
            || p == format!("{SH}oneOrMorePath")
            || p == format!("{SH}zeroOrOnePath")
            || p == format!("{RDF}first")
            || p == format!("{RDF}rest")
    )
}

/// The IRI a `sht:dataGraph` or `sht:shapesGraph` names.
fn graph_ref(graph: &Graph, action: NamedOrBlankNodeRef<'_>, local: &str) -> Option<String> {
    match graph.object_for_subject_predicate(action, sht(local).as_ref())? {
        TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => None,
    }
}

/// Resolves a graph reference to a file: `<>` means the test file itself.
fn resolve(test: &Path, iri: Option<&str>) -> PathBuf {
    iri.and_then(super::manifest::file_url_to_path)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| test.to_path_buf())
}

fn load_into(store: &mut Store, path: &Path, graph: Option<NamedNode>) -> Result<()> {
    let base = super::manifest::path_to_file_url(path);
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri(&base)
        .map_err(|e| anyhow!("bad base: {e}"))?;
    for quad in parser.for_reader(BufReader::new(File::open(path)?)) {
        let mut quad = quad?;
        if let Some(name) = &graph {
            quad.graph_name = name.clone().into();
        }
        store.insert(quad.as_ref())?;
    }
    Ok(())
}

/// Extracts the expected report — the subgraph reachable from `mf:result`.
fn expected_report(graph: &Graph, entry: NamedOrBlankNodeRef<'_>) -> Result<Dataset> {
    let Some(result) = graph.object_for_subject_predicate(entry, mf("result").as_ref()) else {
        return Err(anyhow!("test has no mf:result"));
    };
    let mut out = Dataset::new();
    let mut frontier = vec![result.into_owned()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(node) = frontier.pop() {
        // Only blank nodes expand. A report references IRIs — the focus node, the source
        // shape, the constraint component — and following those would drag each shape's
        // entire definition into the "expected report", which is not what the test asserts.
        let TermRef::BlankNode(b) = node.as_ref() else {
            continue;
        };
        let subject = NamedOrBlankNodeRef::from(b);
        if !seen.insert(node.to_string()) {
            continue;
        }
        for triple in graph.triples_for_subject(subject) {
            // Only follow blank nodes through predicates that carry *report* structure.
            // A blank node hanging off sh:focusNode or sh:value belongs to the data, and
            // expanding it would fold the data graph into the expected report.
            if is_structural(triple.predicate.as_str()) {
                frontier.push(triple.object.into_owned());
            }
            out.insert(&Quad {
                subject: triple.subject.into_owned(),
                predicate: triple.predicate.into_owned(),
                object: triple.object.into_owned(),
                graph_name: GraphName::DefaultGraph,
            });
        }
    }
    Ok(out)
}

fn to_dataset(quads: Vec<Quad>) -> Dataset {
    let mut out = Dataset::new();
    for quad in &quads {
        out.insert(quad);
    }
    out
}

fn compare(expected: &Dataset, actual: &Dataset) -> std::result::Result<(), String> {
    let mut a = expected.clone();
    let mut b = actual.clone();
    a.canonicalize(CanonicalizationAlgorithm::Unstable);
    b.canonicalize(CanonicalizationAlgorithm::Unstable);
    if a == b {
        return Ok(());
    }
    let missing: Vec<String> = a
        .iter()
        .filter(|q| !b.contains(*q))
        .take(3)
        .map(|q| q.to_string())
        .collect();
    let extra: Vec<String> = b
        .iter()
        .filter(|q| !a.contains(*q))
        .take(3)
        .map(|q| q.to_string())
        .collect();
    Err(format!(
        "{} expected vs {} actual triples; missing {missing:?}; unexpected {extra:?}",
        a.len(),
        b.len()
    ))
}

/// Locates the SHACL suites, if they are checked out.
#[must_use]
pub fn suite_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    let dir = root.join("testsuites").join("data-shapes");
    dir.is_dir().then_some(dir)
}
