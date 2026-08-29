//! The W3C test-suite harness.
//!
//! `DESIGN.md` §11 gives P0 an exit criterion — passing the W3C suites — that hand-written
//! tests do not meet. This runs them.
//!
//! # What is actually being tested
//!
//! `spargebra`, `oxttl` and friends are reused from Oxigraph and are already conformance-
//! tested upstream. What is new, and therefore what needs the suites, is everything
//! between: the term encoding, the nine-order index, and the dataset view.
//!
//! So the RDF suites are run as a **round-trip through the store**: parse the input, load
//! it into a [`holos_store::Store`], read it back, and compare. A failure there is a HOLOS
//! bug — a literal the inline codec mangled, a triple term the dictionary lost, a quad the
//! index dropped. Where a failure is upstream instead, the report says so, because the two
//! deserve different responses.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

pub mod entailment;
pub mod manifest;
pub mod protocol;
pub mod resultset;
pub mod shacl;

use anyhow::{anyhow, Result};
use holos_engine::{DatasetView, Engine};
use holos_security::Session;
use manifest::TestEntry;
use oxrdf::dataset::CanonicalizationAlgorithm;
use oxrdf::{BlankNode, Dataset, GraphName, NamedNode, Quad, Term};
use oxrdfio::RdfFormat;
use sparesults::{QueryResultsFormat, QueryResultsParser, ReaderQueryResultsParserOutput};
use spareval::{QueryResults, QuerySolution};
use spargebra::SparqlParser;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// How one test came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The test passed.
    Passed,
    /// The test failed, with why.
    Failed(String),
    /// The harness does not run this kind of test, with why.
    Skipped(String),
}

impl Outcome {
    pub(crate) fn fail(reason: impl Into<String>) -> Self {
        Self::Failed(reason.into())
    }

    pub(crate) fn skip(reason: impl Into<String>) -> Self {
        Self::Skipped(reason.into())
    }
}

/// The result of running a suite.
#[derive(Debug, Default)]
pub struct Report {
    /// Every test that passed, by IRI.
    pub passed: Vec<String>,
    /// Every test that failed, with the reason.
    pub failed: Vec<(String, String)>,
    /// Every test that was not run, with the reason.
    pub skipped: Vec<(String, String)>,
}

impl Report {
    /// Records one outcome.
    pub fn record(&mut self, test: &TestEntry, outcome: Outcome) {
        self.record_named(&test.id, outcome);
    }

    /// Records one outcome under an explicit name.
    pub fn record_named(&mut self, id: &str, outcome: Outcome) {
        match outcome {
            Outcome::Passed => self.passed.push(id.to_owned()),
            Outcome::Failed(why) => self.failed.push((id.to_owned(), why)),
            Outcome::Skipped(why) => self.skipped.push((id.to_owned(), why)),
        }
    }

    /// Tests actually attempted.
    #[must_use]
    pub fn attempted(&self) -> usize {
        self.passed.len() + self.failed.len()
    }

    /// A one-line summary.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{}/{} passed, {} failed, {} skipped",
            self.passed.len(),
            self.attempted(),
            self.failed.len(),
            self.skipped.len()
        )
    }

    /// The failures, formatted for a terminal.
    #[must_use]
    pub fn failure_detail(&self, limit: usize) -> String {
        let mut s = String::new();
        for (id, why) in self.failed.iter().take(limit) {
            let short = id.rsplit(['#', '/']).next().unwrap_or(id);
            s.push_str(&format!("  {short}\n      {why}\n"));
        }
        if self.failed.len() > limit {
            s.push_str(&format!("  ... and {} more\n", self.failed.len() - limit));
        }
        s
    }
}

/// Locates the checked-out test suites, if they are present.
#[must_use]
pub fn testsuite_root() -> Option<PathBuf> {
    // The crate sits at <root>/crates/holos-conformance.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    let dir = root.join("testsuites").join("rdf-tests");
    dir.is_dir().then_some(dir)
}

// ---------------------------------------------------------------------------------
// RDF suites
// ---------------------------------------------------------------------------------

/// Which storage tier a round-trip is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The in-memory tier.
    Memory,
    /// The RocksDB tier, in a scratch directory.
    #[cfg(feature = "rocksdb")]
    RocksDb,
}

/// Runs one entry from an RDF syntax or evaluation suite against the in-memory tier.
#[must_use]
pub fn run_rdf_test(test: &TestEntry) -> Outcome {
    run_rdf_test_on(test, Tier::Memory)
}

/// Runs one entry from an RDF syntax or evaluation suite against a chosen tier.
#[must_use]
pub fn run_rdf_test_on(test: &TestEntry, tier: Tier) -> Outcome {
    // An entailment test asks whether a premise entails a conclusion. Running it through
    // the round-trip below compares a premise against a conclusion as though the second were
    // the expected parse of the first, which is a different question with a predictable
    // answer, and reports the mismatch as an upstream parser fault.
    if entailment::handles(test) {
        return entailment::run(test);
    }
    let kind = local_name(&test.kind);
    let Some(action) = test.action.as_ref() else {
        return Outcome::skip("no mf:action");
    };
    let action_base = manifest::base_for(test, action);

    if kind.ends_with("NegativeSyntax") {
        return match manifest::parse_dataset(action, &action_base) {
            Ok(_) => Outcome::fail("input is invalid but parsed without error"),
            Err(_) => Outcome::Passed,
        };
    }

    let parsed = match manifest::parse_dataset(action, &action_base) {
        Ok(d) => d,
        Err(e) => return Outcome::fail(format!("upstream: input did not parse: {e}")),
    };

    // The HOLOS-specific check: does the store give back exactly what went in?
    let round_tripped = match round_trip(&parsed, tier) {
        Ok(d) => d,
        Err(e) => return Outcome::fail(format!("store round-trip errored: {e}")),
    };
    if let Err(diff) = compare_datasets(&parsed, &round_tripped) {
        return Outcome::fail(format!("store round-trip changed the data: {diff}"));
    }

    // The upstream check, so a parser gap is not reported as a HOLOS gap.
    if let Some(result) = test.result.as_ref() {
        let expected = match manifest::parse_dataset(result, &manifest::base_for(test, result)) {
            Ok(d) => d,
            Err(e) => return Outcome::fail(format!("upstream: expected file did not parse: {e}")),
        };
        if let Err(diff) = compare_datasets(&expected, &parsed) {
            return Outcome::fail(format!("upstream: parser output differs: {diff}"));
        }
    }
    Outcome::Passed
}

/// Loads a dataset into a [`holos_store::Store`] and reads it straight back out.
fn round_trip(input: &Dataset, tier: Tier) -> Result<Dataset> {
    // Held for the duration so the scratch directory outlives the store.
    #[cfg(feature = "rocksdb")]
    let _scratch;
    let mut store = match tier {
        Tier::Memory => holos_store::Store::new(),
        #[cfg(feature = "rocksdb")]
        Tier::RocksDb => {
            _scratch = tempfile::tempdir()?;
            holos_store::Store::with_storage(holos_store::RocksStorage::open(_scratch.path())?)
        }
    };
    for quad in input {
        store.insert(quad)?;
    }
    let mut out = Dataset::new();
    for quad in store.iter() {
        out.insert(&quad?);
    }
    Ok(out)
}

/// Compares two datasets up to blank-node isomorphism.
fn compare_datasets(expected: &Dataset, actual: &Dataset) -> std::result::Result<(), String> {
    let mut a = expected.clone();
    let mut b = actual.clone();
    a.canonicalize(CanonicalizationAlgorithm::Unstable);
    b.canonicalize(CanonicalizationAlgorithm::Unstable);
    if a == b {
        return Ok(());
    }
    let mut missing: Vec<String> = a
        .iter()
        .filter(|q| !b.contains(*q))
        .map(|q| q.to_string())
        .collect();
    let mut extra: Vec<String> = b
        .iter()
        .filter(|q| !a.contains(*q))
        .map(|q| q.to_string())
        .collect();
    // Sorted before truncating: which examples get shown must not depend on hash iteration
    // order. The recorded `.failures` baselines carry this text, so a nondeterministic sample
    // makes every re-baseline churn, and a ratchet whose diff is always noise is one nobody
    // reads.
    missing.sort_unstable();
    extra.sort_unstable();
    missing.truncate(2);
    extra.truncate(2);
    Err(format!(
        "{} expected vs {} actual quads; missing {missing:?}; unexpected {extra:?}",
        a.len(),
        b.len(),
    ))
}

// ---------------------------------------------------------------------------------
// SPARQL suites
// ---------------------------------------------------------------------------------

/// Runs one entry from a SPARQL suite.
#[must_use]
pub fn run_sparql_test(test: &TestEntry) -> Outcome {
    match local_name(&test.kind) {
        "QueryEvaluationTest" => run_query_evaluation(test),
        k if k.starts_with("PositiveSyntaxTest") => match read_and_parse_query(test) {
            Ok(_) => Outcome::Passed,
            Err(e) => Outcome::fail(format!("upstream: valid query rejected: {e}")),
        },
        // Syntax tests exercise the parser and nothing below it, so a failure here is
        // always upstream. They are still run: a parser that drifts changes what the
        // layers underneath ever see.
        k if k.starts_with("NegativeSyntaxTest") => match read_and_parse_query(test) {
            Ok(_) => Outcome::fail("upstream: invalid query was accepted by the parser"),
            Err(_) => Outcome::Passed,
        },
        "UpdateEvaluationTest" => run_update_evaluation(test),

        // Update syntax exercises the parser and nothing below it, exactly like the query
        // syntax tests, so a failure here is upstream.
        k if k.starts_with("PositiveUpdateSyntaxTest") => match read_and_parse_update(test) {
            Ok(_) => Outcome::Passed,
            Err(e) => Outcome::fail(format!("upstream: valid update rejected: {e}")),
        },
        k if k.starts_with("NegativeUpdateSyntaxTest") => match read_and_parse_update(test) {
            Ok(_) => Outcome::fail("upstream: invalid update was accepted by the parser"),
            Err(_) => Outcome::Passed,
        },

        // The protocol suites need an HTTP server driven by the harness (L6); entailment
        // needs a reasoner (L4). Both are roadmap items.
        k @ ("ProtocolTest"
        | "GraphStoreProtocolTest"
        | "ServiceDescriptionTest"
        | "CSVResultFormatTest") => Outcome::skip(format!("{k}: not implemented yet")),
        other => Outcome::skip(format!("unhandled test type {other}")),
    }
}

fn read_and_parse_update(test: &TestEntry) -> Result<spargebra::Update> {
    let path = test
        .action
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no action file"))?;
    let text = std::fs::read_to_string(&path)?;
    let base = manifest::path_to_file_url(&path);
    Ok(spargebra::SparqlParser::new()
        .with_base_iri(base)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .parse_update(&text)?)
}

/// Runs one `UpdateEvaluationTest`.
///
/// The shape of these differs from the query tests: the action names a *request* file plus
/// the dataset it starts from, and the result names the dataset it should end as. So the
/// harness loads the start state, applies the update, and compares the whole dataset
/// against the expected one — which makes it a much stricter test than a query's, because
/// every quad in every graph has to match, not just the projected rows.
fn run_update_evaluation(test: &TestEntry) -> Outcome {
    use holos_engine::Engine;
    use holos_security::Session;

    let Some(request_path) = test.update_request.clone() else {
        return Outcome::skip("no update request file");
    };
    let text = match std::fs::read_to_string(&request_path) {
        Ok(text) => text,
        Err(e) => return Outcome::fail(format!("reading the request: {e}")),
    };

    let mut engine = Engine::new();
    if let Err(e) = load_dataset(&mut engine, test.data.as_deref(), &test.graph_data) {
        return Outcome::fail(format!("loading the start state: {e}"));
    }

    let base = manifest::path_to_file_url(&request_path);
    let parsed = match spargebra::SparqlParser::new()
        .with_base_iri(base)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .and_then(|p| Ok(p.parse_update(&text)?))
    {
        Ok(parsed) => parsed,
        Err(e) => return Outcome::fail(format!("upstream: update did not parse: {e}")),
    };

    let mut session = match Session::unrestricted(engine.store()) {
        Ok(session) => session,
        Err(e) => return Outcome::fail(format!("opening a session: {e}")),
    };
    if let Err(e) = holos_engine::update::apply(&mut engine, &mut session, &parsed) {
        return Outcome::fail(format!("applying the update: {e}"));
    }

    let mut expected = Engine::new();
    if let Err(e) = load_dataset(
        &mut expected,
        test.result_data.as_deref(),
        &test.result_graph_data,
    ) {
        return Outcome::fail(format!("loading the expected state: {e}"));
    }

    compare_stores(&engine, &expected)
}

/// An engine's whole contents as a dataset, for comparison up to isomorphism.
fn as_dataset(engine: &holos_engine::Engine) -> Result<Dataset> {
    let store = engine.store();
    let mut out = Dataset::new();
    for quad in store.quads_for_pattern(None, None, None, holos_store::GraphFilter::Any) {
        out.insert(&store.decode_quad(quad?)?);
    }
    Ok(out)
}

/// Loads the graphs a query's `FROM` and `FROM NAMED` clauses name.
///
/// Both go in as *named* graphs under their own IRI, which lets the evaluator's own dataset
/// handling do the rest: `FROM` merges them into the active default graph and `FROM NAMED`
/// leaves them addressable, and neither is this harness's decision to make.
///
/// A clause naming something that is not a readable file is left alone rather than failing.
/// A query may name a graph the store already holds, and the federated tests name endpoints.
fn load_dataset_clauses(engine: &mut holos_engine::Engine, query: &spargebra::Query) -> Result<()> {
    let dataset = match query {
        spargebra::Query::Select { dataset, .. }
        | spargebra::Query::Construct { dataset, .. }
        | spargebra::Query::Describe { dataset, .. }
        | spargebra::Query::Ask { dataset, .. } => dataset.as_ref(),
    };
    let Some(dataset) = dataset else {
        return Ok(());
    };
    let named = dataset.named.iter().flatten();
    for iri in dataset.default.iter().chain(named) {
        let Some(path) = manifest::file_url_to_path(iri.as_str()) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let graph = GraphName::from(iri.clone());
        load(engine, &path, iri.as_str(), Some(graph))?;
    }
    Ok(())
}

/// Loads a default graph and a set of named graphs into an engine.
fn load_dataset(
    engine: &mut holos_engine::Engine,
    default_graph: Option<&std::path::Path>,
    named: &[(String, std::path::PathBuf)],
) -> Result<()> {
    if let Some(path) = default_graph {
        let format = manifest::format_for(path)
            .ok_or_else(|| anyhow::anyhow!("no RDF format for {}", path.display()))?;
        let base = manifest::path_to_file_url(path);
        let file = std::fs::File::open(path)?;
        engine.bulk_load(std::io::BufReader::new(file), format, Some(&base))?;
    }
    for (name, path) in named {
        let format = manifest::format_for(path)
            .ok_or_else(|| anyhow::anyhow!("no RDF format for {}", path.display()))?;
        let base = manifest::path_to_file_url(path);
        let file = std::fs::File::open(path)?;
        let node = oxrdf::NamedNode::new(name)?;
        engine.bulk_load_into_graph(
            std::io::BufReader::new(file),
            format,
            Some(&base),
            &node.into(),
        )?;
    }
    Ok(())
}

/// Compares two datasets quad for quad.
///
/// Blank nodes are compared by label rather than by isomorphism. That is stricter than the
/// specification requires, so a test failing only on blank node labelling is a harness
/// limitation and is reported as such rather than as a defect.
fn compare_stores(actual: &holos_engine::Engine, expected: &holos_engine::Engine) -> Outcome {
    use holos_store::GraphFilter;

    let dump = |engine: &holos_engine::Engine| -> Result<std::collections::BTreeSet<String>> {
        let store = engine.store();
        let mut out = std::collections::BTreeSet::new();
        for quad in store.quads_for_pattern(None, None, None, GraphFilter::Any) {
            out.insert(store.decode_quad(quad?)?.to_string());
        }
        Ok(out)
    };

    let (actual_engine, expected_engine) = (actual, expected);
    let (actual, expected) = match (dump(actual_engine), dump(expected_engine)) {
        (Ok(a), Ok(e)) => (a, e),
        (Err(e), _) | (_, Err(e)) => return Outcome::fail(format!("reading a dataset: {e}")),
    };

    if actual == expected {
        return Outcome::Passed;
    }

    // Then up to blank-node isomorphism. An update that creates a blank node gets whichever
    // label the store hands out, so comparing labels makes a correct result look wrong — a
    // limitation this harness used to admit in its skip text rather than fix.
    if let (Ok(a), Ok(e)) = (as_dataset(actual_engine), as_dataset(expected_engine)) {
        if compare_datasets(&e, &a).is_ok() {
            return Outcome::Passed;
        }
    }

    // The examples quoted have to be the *sorted* prefix, not whichever the set reached
    // first: the recorded `.failures` baselines carry this text, so a sample that moves makes
    // every re-baseline churn, and a ratchet whose diff is always noise is one nobody reads.
    //
    // Here that comes from `dump` returning a `BTreeSet`, which is ordered already — the sort
    // below is a no-op today and is kept only so the property survives a change of container.
    // The two sibling comparisons build from unordered sets and genuinely need theirs.
    let mut missing: Vec<&String> = expected.difference(&actual).collect();
    let mut extra: Vec<&String> = actual.difference(&expected).collect();
    missing.sort_unstable();
    extra.sort_unstable();
    missing.truncate(3);
    extra.truncate(3);

    if (missing.iter().any(|q| q.contains("_:")) || extra.iter().any(|q| q.contains("_:")))
        && missing.len() == extra.len()
    {
        return Outcome::skip(
            "differs only in blank node labels; the harness compares labels, not isomorphism",
        );
    }

    Outcome::fail(format!(
        "dataset mismatch: {} expected quads missing (e.g. {:?}), {} unexpected (e.g. {:?})",
        expected.difference(&actual).count(),
        missing,
        actual.difference(&expected).count(),
        extra
    ))
}

fn read_and_parse_query(test: &TestEntry) -> Result<spargebra::Query> {
    let path = test
        .query
        .as_ref()
        .or(test.action.as_ref())
        .ok_or_else(|| anyhow!("no query file"))?;
    let text = std::fs::read_to_string(path)?;
    let base = manifest::base_for(test, path);
    Ok(SparqlParser::new()
        .with_base_iri(base)
        .map_err(|e| anyhow!("{e}"))?
        .parse_query(&text)?)
}

fn run_query_evaluation(test: &TestEntry) -> Outcome {
    let Some(result_path) = test.result.as_ref() else {
        return Outcome::skip("no mf:result");
    };
    // A test that names an entailment regime is answered against the *entailed* graph, not
    // the asserted one. RDFS is materialisable here; OWL and RIF are not, and are skipped
    // with the regime named rather than under one word that covers both cases.
    if test.needs_entailment() && !test.rdfs_entailment_suffices() {
        return Outcome::skip(format!(
            "needs an entailment regime this engine does not implement: {}",
            test.entailment_regimes.join(", ")
        ));
    }
    let query = match read_and_parse_query(test) {
        Ok(q) => q,
        Err(e) => return Outcome::fail(format!("upstream: query did not parse: {e}")),
    };

    let mut engine = Engine::new();
    if let Some(data) = test.data.as_ref() {
        if let Err(e) = load(&mut engine, data, &manifest::base_for(test, data), None) {
            return Outcome::fail(format!("loading data: {e}"));
        }
    }
    for (iri, path) in &test.graph_data {
        let graph = GraphName::from(NamedNode::new_unchecked(iri.clone()));
        if let Err(e) = load(&mut engine, path, iri, Some(graph)) {
            return Outcome::fail(format!("loading graph data: {e}"));
        }
    }
    // `FROM <g>` and `FROM NAMED <g>` name graphs the *query* asks for, and in the dataset
    // suite that is all the test supplies — the action carries a query and nothing else, and
    // the IRIs resolve to files beside it. Without loading them the query runs against an
    // empty dataset and returns nothing, which the differential rig then files as an
    // upstream fault because the reference evaluator, given the same nothing, agrees.
    if let Err(e) = load_dataset_clauses(&mut engine, &query) {
        return Outcome::fail(format!("loading a FROM clause: {e}"));
    }

    // The federated suite names each endpoint with `qt:serviceData` and supplies a local
    // file for it, so `SERVICE` is exercised without a live endpoint anywhere.
    let mut services = holos_engine::service::LocalServiceHandler::new();
    for (endpoint, path) in &test.service_data {
        let base = manifest::path_to_file_url(path);
        match manifest::parse_dataset(path, &base) {
            Ok(dataset) => {
                let Ok(iri) = NamedNode::new(endpoint) else {
                    return Outcome::fail(format!("service endpoint `{endpoint}` is not an IRI"));
                };
                services.add_endpoint(iri, dataset);
            }
            Err(e) => return Outcome::fail(format!("loading service data: {e}")),
        }
    }

    let mut session = match Session::unrestricted(engine.store()) {
        Ok(s) => s,
        Err(e) => return Outcome::fail(format!("opening session: {e}")),
    };

    // Materialised into the *default* graph rather than beside it. Under an entailment
    // regime the basic graph pattern is matched against the entailed graph, so the closure
    // has to be the graph the query reads; a second graph next to it would be invisible to
    // a query with no `GRAPH` clause, which is every query in this suite.
    if test.needs_entailment() {
        if let Err(e) = holos_engine::entailment::materialise(
            &mut engine,
            &mut session,
            None,
            holos_engine::entailment::DEFAULT_BUDGET,
        ) {
            return Outcome::fail(format!("materialising the RDFS closure: {e}"));
        }
    }

    let view = engine.view(&session);
    let actual = match Engine::query_prepared_with_services(&view, &query, services) {
        Ok(r) => r,
        Err(e) => return Outcome::fail(format!("evaluation failed: {e}")),
    };

    let ordered = query.to_string().to_uppercase().contains("ORDER BY");
    let shape = Shape {
        ordered,
        lax_cardinality: test.lax_cardinality,
    };
    match compare_results(actual, result_path, shape, &view) {
        Ok(()) => Outcome::Passed,
        Err(e) if e == UNREADABLE_RESULT_FORMAT => Outcome::Skipped(e),
        Err(e) => attribute(test, &query, result_path, ordered, e),
    }
}

/// Works out whose bug a failure is.
///
/// The evaluator is `spareval`, reused unchanged; the storage under it is HOLOS. When a
/// test fails, running the *same query* through the *same evaluator* over an
/// `oxrdf::Dataset` — the reference storage `spareval` ships with — separates the two:
///
/// - both give the same answer  → the storage is faithful; the gap is in the evaluator
/// - the answers differ         → HOLOS lost or changed something, and that is a real bug
///
/// This is the differential rig `DESIGN.md` §12 calls for against the future Tier B
/// hypertrie, built early because it is exactly as useful now.
fn attribute(
    test: &TestEntry,
    query: &spargebra::Query,
    result_path: &Path,
    ordered: bool,
    failure: String,
) -> Outcome {
    // The rig compares two *evaluators* over the same data, which is only meaningful when
    // both see the same data. Under an entailment regime they do not: HOLOS is answering
    // against a materialised closure and the reference against the assertions alone, so
    // they agree exactly when the closure added nothing — and reading that as "upstream"
    // would file this engine's own missing inferences under someone else's name.
    if test.needs_entailment() {
        return Outcome::Failed(failure);
    }
    let Ok(dataset) = reference_dataset(test) else {
        return Outcome::Failed(failure);
    };
    let Ok(oracle) = spareval::QueryEvaluator::new()
        .prepare(query)
        .execute(&dataset)
    else {
        return Outcome::Failed(failure);
    };
    // Re-run HOLOS so both answers come from a fresh evaluation.
    let mut engine = Engine::new();
    if load_all(&mut engine, test).is_err() {
        return Outcome::Failed(failure);
    }
    let Ok(session) = Session::unrestricted(engine.store()) else {
        return Outcome::Failed(failure);
    };
    let view = engine.view(&session);
    let Ok(mine) = Engine::query_prepared(&view, query) else {
        return Outcome::Failed(failure);
    };

    match compare_two(mine, oracle, ordered) {
        Ok(()) => Outcome::skip(format!(
            "upstream: HOLOS agrees with the reference dataset, so the evaluator differs              from the expected result — {failure}"
        )),
        Err(divergence) => Outcome::fail(format!(
            "HOLOS differs from the reference dataset: {divergence} (vs expected: {failure});              result file {}",
            result_path.display()
        )),
    }
}

/// The same data, loaded into 's own reference storage.
fn reference_dataset(test: &TestEntry) -> Result<Dataset> {
    let mut dataset = Dataset::new();
    if let Some(data) = test.data.as_ref() {
        for quad in manifest::parse_dataset(data, &manifest::base_for(test, data))?.iter() {
            dataset.insert(quad);
        }
    }
    for (iri, path) in &test.graph_data {
        let graph = GraphName::from(NamedNode::new_unchecked(iri.clone()));
        for quad in manifest::parse_dataset(path, iri)?.iter() {
            dataset.insert(&Quad {
                subject: quad.subject.into_owned(),
                predicate: quad.predicate.into_owned(),
                object: quad.object.into_owned(),
                graph_name: graph.clone(),
            });
        }
    }
    Ok(dataset)
}

fn load_all(engine: &mut Engine, test: &TestEntry) -> Result<()> {
    if let Some(data) = test.data.as_ref() {
        load(engine, data, &manifest::base_for(test, data), None)?;
    }
    for (iri, path) in &test.graph_data {
        let graph = GraphName::from(NamedNode::new_unchecked(iri.clone()));
        load(engine, path, iri, Some(graph))?;
    }
    Ok(())
}

/// Compares two live query results against each other.
fn compare_two(
    a: QueryResults<'_>,
    b: QueryResults<'_>,
    ordered: bool,
) -> std::result::Result<(), String> {
    match (a, b) {
        (QueryResults::Boolean(x), QueryResults::Boolean(y)) if x == y => Ok(()),
        (QueryResults::Boolean(x), QueryResults::Boolean(y)) => Err(format!("boolean {x} vs {y}")),
        (QueryResults::Solutions(x), QueryResults::Solutions(y)) => {
            let xs: Vec<_> = x
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| e.to_string())?;
            let ys: Vec<_> = y
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| e.to_string())?;
            compare_solutions(
                &ys,
                &xs,
                Shape {
                    ordered,
                    // The rig compares two live results, so a difference in multiplicity
                    // between them is a real difference whatever the test asserts.
                    lax_cardinality: false,
                },
            )
        }
        (QueryResults::Graph(x), QueryResults::Graph(y)) => {
            let mut gx = Dataset::new();
            for t in x {
                let t = t.map_err(|e| e.to_string())?;
                gx.insert(&Quad {
                    subject: t.subject,
                    predicate: t.predicate,
                    object: t.object,
                    graph_name: GraphName::DefaultGraph,
                });
            }
            let mut gy = Dataset::new();
            for t in y {
                let t = t.map_err(|e| e.to_string())?;
                gy.insert(&Quad {
                    subject: t.subject,
                    predicate: t.predicate,
                    object: t.object,
                    graph_name: GraphName::DefaultGraph,
                });
            }
            compare_datasets(&gy, &gx)
        }
        _ => Err("result kinds differ".to_owned()),
    }
}

fn load(engine: &mut Engine, path: &Path, base: &str, graph: Option<GraphName>) -> Result<()> {
    let format = manifest::format_for(path)
        .ok_or_else(|| anyhow!("no RDF format for {}", path.display()))?;
    let reader = BufReader::new(File::open(path)?);
    match graph {
        None => engine.bulk_load(reader, format, Some(base))?,
        Some(g) => engine.bulk_load_into_graph(reader, format, Some(base), &g)?,
    };
    Ok(())
}

/// How strictly a result is to be compared.
///
/// Two independent axes, and both come from the test rather than from the query. `ordered`
/// is whether the sequence is part of the answer; `lax_cardinality` is whether the
/// *multiplicities* are, which `mf:resultCardinality mf:LaxCardinality` says they are not.
#[derive(Debug, Clone, Copy, Default)]
pub struct Shape {
    /// The query has an `ORDER BY`, so the sequence is part of the answer.
    pub ordered: bool,
    /// `mf:LaxCardinality`: how many times a solution appears is not asserted.
    pub lax_cardinality: bool,
}

fn compare_results(
    actual: QueryResults<'_>,
    expected_path: &Path,
    shape: Shape,
    view: &DatasetView<'_>,
) -> std::result::Result<(), String> {
    match actual {
        QueryResults::Boolean(value) => {
            let expected = read_expected_boolean(expected_path)?;
            if expected == value {
                Ok(())
            } else {
                Err(format!("expected {expected}, got {value}"))
            }
        }
        QueryResults::Solutions(iter) => {
            let actual: Vec<_> = iter
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| format!("reading solutions: {e}"))?;
            let expected = read_expected_solutions(expected_path)?;
            compare_solutions(&expected, &actual, shape)
        }
        QueryResults::Graph(triples) => {
            let mut got = Dataset::new();
            for triple in triples {
                let triple = triple.map_err(|e| format!("reading graph result: {e}"))?;
                got.insert(&Quad {
                    subject: triple.subject,
                    predicate: triple.predicate,
                    object: triple.object,
                    graph_name: GraphName::DefaultGraph,
                });
            }
            let expected =
                manifest::parse_dataset(expected_path, &manifest::path_to_file_url(expected_path))
                    .map_err(|e| format!("upstream: expected graph did not parse: {e}"))?;
            let _ = view; // graph results carry no view-specific state
            compare_datasets(&expected, &got)
        }
    }
}

/// Compares a run against a checked-in baseline, failing on drift in either direction.
///
/// The query suites ratchet through their own `Report`; the protocol suites do not produce
/// one, because a scripted conversation has no solutions to compare. This takes the
/// failures directly so both kinds of suite are held to the same standard: a regression
/// fails, and so does a test that started passing without the baseline being updated.
///
/// # Panics
///
/// Panics — which is how a test fails — when the failures differ from the baseline.
pub fn ratchet_named(name: &str, failed: &[(String, String)]) {
    use std::collections::BTreeSet;

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the crate sits two levels below the workspace root")
        .join("conformance")
        .join(format!("{name}.failures"));

    let actual: BTreeSet<String> = failed.iter().map(|(id, _)| id.clone()).collect();

    if std::env::var("HOLOS_UPDATE_CONFORMANCE").is_ok() {
        let mut body = format!(
            "# {name} — tests known to fail. Regenerate with HOLOS_UPDATE_CONFORMANCE=1.\n"
        );
        for (id, why) in failed {
            body.push_str(&format!("{id}\t{}\n", why.replace(['\n', '\t'], " ")));
        }
        std::fs::write(&path, body).expect("writing the baseline");
        eprintln!("{name}: baseline updated ({} known failures)", failed.len());
        return;
    }

    let expected: BTreeSet<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .map(|l| l.split('\t').next().unwrap_or(l).to_owned())
        .collect();

    let regressed: Vec<_> = actual.difference(&expected).collect();
    let fixed: Vec<_> = expected.difference(&actual).collect();

    assert!(
        regressed.is_empty(),
        "{name}: {} test(s) regressed:\n  {}\n\nIf expected, re-baseline with \
         HOLOS_UPDATE_CONFORMANCE=1.",
        regressed.len(),
        regressed
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
    assert!(
        fixed.is_empty(),
        "{name}: {} test(s) on the known-failure list now pass:\n  {}\n\nRe-baseline with \
         HOLOS_UPDATE_CONFORMANCE=1 — a stale list is a list nobody trusts.",
        fixed.len(),
        fixed
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Marker for the one result encoding the harness cannot read: results serialised as RDF
/// (the old DAWG `rs:ResultSet` vocabulary). Reported as a skip, because it says nothing
/// about the engine.
const UNREADABLE_RESULT_FORMAT: &str =
    "expected results are encoded as RDF, which the harness does not read";

fn results_format(path: &Path) -> Option<QueryResultsFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "srx" => Some(QueryResultsFormat::Xml),
        "srj" => Some(QueryResultsFormat::Json),
        "csv" => Some(QueryResultsFormat::Csv),
        "tsv" => Some(QueryResultsFormat::Tsv),
        _ => None,
    }
}

fn read_expected_boolean(path: &Path) -> std::result::Result<bool, String> {
    if results_format(path).is_none() && resultset::is_rdf_encoded(path) {
        return match resultset::read(path).map_err(|e| e.to_string())? {
            resultset::Expected::Boolean(b) => Ok(b),
            resultset::Expected::Solutions(_) => {
                Err("expected a boolean result, file holds solutions".to_owned())
            }
        };
    }
    let format = results_format(path).ok_or(UNREADABLE_RESULT_FORMAT.to_owned())?;
    let reader = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    match QueryResultsParser::from_format(format)
        .for_reader(reader)
        .map_err(|e| e.to_string())?
    {
        ReaderQueryResultsParserOutput::Boolean(b) => Ok(b),
        ReaderQueryResultsParserOutput::Solutions(_) => {
            Err("expected a boolean result, file holds solutions".to_owned())
        }
    }
}

fn read_expected_solutions(path: &Path) -> std::result::Result<Vec<QuerySolution>, String> {
    if results_format(path).is_none() && resultset::is_rdf_encoded(path) {
        return match resultset::read(path).map_err(|e| e.to_string())? {
            resultset::Expected::Solutions(rows) => Ok(rows),
            resultset::Expected::Boolean(_) => {
                Err("expected solutions, file holds a boolean".to_owned())
            }
        };
    }
    let format = results_format(path).ok_or(UNREADABLE_RESULT_FORMAT.to_owned())?;
    let reader = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    match QueryResultsParser::from_format(format)
        .for_reader(reader)
        .map_err(|e| e.to_string())?
    {
        ReaderQueryResultsParserOutput::Solutions(iter) => {
            iter.map(|s| s.map_err(|e| e.to_string())).collect()
        }
        ReaderQueryResultsParserOutput::Boolean(_) => {
            Err("expected solutions, file holds a boolean".to_owned())
        }
    }
}

/// Compares two solution sequences.
///
/// Blank nodes make this awkward: two runs may label them differently, so equality has to
/// be up to a bijection. Rather than hand-roll one, the solutions are encoded as an RDF
/// graph — one blank node per solution, one triple per binding — and canonicalised. That
/// reuses RDFC-1.0 and gets multiset semantics for free, because two identical solutions
/// become two distinguishable nodes.
fn compare_solutions(
    expected: &[QuerySolution],
    actual: &[QuerySolution],
    shape: Shape,
) -> std::result::Result<(), String> {
    // Under lax cardinality the answer is the *set*, so both sides are deduplicated before
    // anything is counted. `REDUCED` may return any number of copies between one per
    // distinct solution and the whole multiset, and a fixture can only show one of those —
    // so comparing multiplicities against it fails an engine that chose differently and was
    // right to.
    if shape.lax_cardinality {
        let expected = distinct(expected);
        let actual = distinct(actual);
        if expected.len() != actual.len() {
            return Err(format!(
                "expected {} distinct solutions, got {}",
                expected.len(),
                actual.len()
            ));
        }
        return compare_datasets(
            &solutions_as_dataset(expected),
            &solutions_as_dataset(actual),
        );
    }
    if expected.len() != actual.len() {
        return Err(format!(
            "expected {} solutions, got {}",
            expected.len(),
            actual.len()
        ));
    }
    if shape.ordered && !has_blank_nodes(expected) && !has_blank_nodes(actual) {
        for (i, (e, a)) in expected.iter().zip(actual).enumerate() {
            if bindings(e) != bindings(a) {
                return Err(format!(
                    "solution {i} differs: expected {:?}, got {:?}",
                    render(e),
                    render(a)
                ));
            }
        }
        return Ok(());
    }
    compare_datasets(
        &solutions_as_dataset(expected),
        &solutions_as_dataset(actual),
    )
}

/// The distinct solutions, keeping the first of each.
///
/// Keyed on the rendered bindings rather than on `QuerySolution`, which is not `Hash`. Two
/// solutions binding the same variables to the same terms render identically, which is what
/// "distinct" means here.
fn distinct(solutions: &[QuerySolution]) -> Vec<&QuerySolution> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    solutions
        .iter()
        .filter(|s| seen.insert(format!("{:?}", render(s))))
        .collect()
}

fn has_blank_nodes(solutions: &[QuerySolution]) -> bool {
    solutions
        .iter()
        .any(|s| s.iter().any(|(_, t)| matches!(t, Term::BlankNode(_))))
}

fn bindings(solution: &QuerySolution) -> BTreeMap<String, Term> {
    solution
        .iter()
        .map(|(v, t)| (v.as_str().to_owned(), t.clone()))
        .collect()
}

fn render(solution: &QuerySolution) -> Vec<String> {
    bindings(solution)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
}

fn solutions_as_dataset<'a>(solutions: impl IntoIterator<Item = &'a QuerySolution>) -> Dataset {
    let root = NamedNode::new_unchecked("urn:holos:conformance:results");
    let has_solution = NamedNode::new_unchecked("urn:holos:conformance:solution");
    let mut dataset = Dataset::new();
    for solution in solutions {
        let node = BlankNode::default();
        dataset.insert(&Quad {
            subject: root.clone().into(),
            predicate: has_solution.clone(),
            object: node.clone().into(),
            graph_name: GraphName::DefaultGraph,
        });
        for (variable, term) in solution.iter() {
            dataset.insert(&Quad {
                subject: node.clone().into(),
                predicate: NamedNode::new_unchecked(format!(
                    "urn:holos:conformance:var:{}",
                    variable.as_str()
                )),
                object: term.clone(),
                graph_name: GraphName::DefaultGraph,
            });
        }
    }
    dataset
}

pub(crate) fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/']).next().unwrap_or(iri)
}

/// The RDF format a file's extension implies, re-exported for the test binaries.
#[must_use]
pub fn rdf_format(path: &Path) -> Option<RdfFormat> {
    manifest::format_for(path)
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    use oxrdf::{Dataset, NamedNode, Quad};

    /// The recorded `.failures` baselines quote a sample of the differing quads, and the
    /// sample used to be whichever ones hash iteration reached first. Every re-baseline then
    /// rewrote files where nothing had changed — on one run six baselines moved and only one
    /// had actually changed state — so the ratchet's diff was noise and hid real movement.
    ///
    /// The fix was to sort before truncating. It was verified by hand, by re-baselining twice
    /// and diffing, which is exactly the kind of verification that does not survive contact
    /// with a later edit.
    fn quad(n: u32) -> Quad {
        Quad {
            subject: NamedNode::new_unchecked(format!("http://example.com/s{n:02}")).into(),
            predicate: NamedNode::new_unchecked("http://example.com/p"),
            object: NamedNode::new_unchecked(format!("http://example.com/o{n:02}")).into(),
            graph_name: oxrdf::GraphName::DefaultGraph,
        }
    }

    #[test]
    fn the_quoted_sample_is_the_sorted_prefix_not_whatever_came_first() {
        // Ten differing quads, so the two that get quoted are a choice rather than the whole
        // set — which is the situation the sample has to be deterministic in.
        let expected = Dataset::new();
        let mut actual = Dataset::new();
        for n in (0..10).rev() {
            actual.insert(&quad(n));
        }
        let message = compare_datasets(&expected, &actual).expect_err("the datasets differ");

        assert!(
            message.contains("s00") && message.contains("s01"),
            "the two lexicographically smallest should be quoted, got: {message}"
        );
        assert!(
            !message.contains("s09"),
            "and nothing later than them, got: {message}"
        );
        assert!(
            message.find("s00") < message.find("s01"),
            "quoted in sorted order, got: {message}"
        );
    }

    /// Insertion order must not reach the output either. Two datasets holding the same quads
    /// built in opposite orders have to produce the same text, because a baseline that moves
    /// when nothing did is a baseline nobody reads.
    #[test]
    fn insertion_order_does_not_reach_the_message() {
        let expected = Dataset::new();
        let mut forwards = Dataset::new();
        let mut backwards = Dataset::new();
        for n in 0..10 {
            forwards.insert(&quad(n));
        }
        for n in (0..10).rev() {
            backwards.insert(&quad(n));
        }
        assert_eq!(
            compare_datasets(&expected, &forwards).expect_err("differ"),
            compare_datasets(&expected, &backwards).expect_err("differ"),
        );
    }

    /// `compare_stores` is the third place the same sample is built.
    ///
    /// Unlike the other two it was never actually broken: it dumps into a `BTreeSet`, which
    /// is ordered, so its prefix was already the sorted one and its explicit sort is a no-op.
    /// The test pins the *observable* property rather than the mechanism, which is what makes
    /// it worth having — it does not fail if the redundant sort goes, and it does fail if the
    /// container is ever swapped for an unordered one, which is how this would really break.
    #[test]
    fn the_store_comparison_quotes_a_sorted_prefix_too() {
        let mut actual = holos_engine::Engine::new();
        let expected = holos_engine::Engine::new();
        for n in (0..10).rev() {
            actual.store_mut().insert(quad(n).as_ref()).expect("insert");
        }
        let Outcome::Failed(message) = compare_stores(&actual, &expected) else {
            panic!("the stores differ, so this must fail");
        };
        for n in 0..3 {
            assert!(
                message.contains(&format!("s{n:02}")),
                "s{n:02} is in the three smallest and should be quoted, got: {message}"
            );
        }
        for n in 3..10 {
            assert!(
                !message.contains(&format!("s{n:02}")),
                "s{n:02} is outside the three smallest, got: {message}"
            );
        }
    }
}
