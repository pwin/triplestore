//! Python bindings for HOLOS.
//!
//! The surface deliberately mirrors `pyoxigraph` where the two overlap — `Store`,
//! `store.query(...)`, iterating solutions — so that anyone already working in this
//! ecosystem does not have to learn a second vocabulary for the same ideas. What it adds is
//! the part no other binding has: [`Policy`] and [`Session`], so a Python caller can ask a
//! question *as a principal* and get back the answer that principal is entitled to.
//!
//! # Two things worth knowing
//!
//! **The GIL is released around every query and every load.** Both can run for a long time
//! and neither touches Python objects while running, so holding the GIL would block every
//! other thread in the process for no reason. `Storage` is `Send + Sync`, which is what
//! makes this sound rather than merely convenient.
//!
//! **Persistence is compiled in, not installed in.** A Python extra selects *dependencies*;
//! it cannot toggle compiled code. See `PACKAGING.md` for what is done instead, and
//! [`has_rocksdb`] for asking at runtime which kind of wheel you have.

#![forbid(unsafe_code)]
#![allow(clippy::needless_pass_by_value)] // pyo3 takes owned arguments

use holos_engine::Engine;
use holos_security::{
    Label, Modes, Policy as CorePolicy, Principal as CorePrincipal, PrincipalMatch, Rule, Scope,
    Semantics, Session as CoreSession,
};
use oxrdf::{GraphName, NamedNode, NamedOrBlankNode, Quad, Term};
use oxrdfio::RdfFormat;
use pyo3::exceptions::{PyIOError, PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use spareval::QueryResults;
use std::path::Path;
use std::sync::{Arc, RwLock};

// ---------------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------------

// The *public* module name, not the extension's. It becomes each exception's `__module__`,
// so a traceback reads `holosdb.SyntaxError` rather than naming the private `_holosdb`
// that users are not expected to know exists. The classes below say the same thing through
// `#[pyclass(module = ...)]`.
pyo3::create_exception!(holosdb, HolosError, pyo3::exceptions::PyException);
pyo3::create_exception!(holosdb, SyntaxError, HolosError);
pyo3::create_exception!(holosdb, PolicyError, HolosError);
pyo3::create_exception!(holosdb, QueryRefusedError, HolosError);
pyo3::create_exception!(holosdb, QueryTimeoutError, HolosError);

/// The engine's own classification ([`holos_engine::EngineError::kind`]), as exception types a
/// caller can catch separately. A refusal and a timeout are the deployment's limits, not the
/// query's mistake, and a program -- an agent rewriting its own query, say -- needs to tell
/// them from a syntax error without reading the message. All are [`HolosError`], so code
/// that catches that is unaffected.
fn map_engine_error(e: &holos_engine::EngineError) -> PyErr {
    // A typed exception names the kind already, so it carries the detail without the kind's
    // prefix: a read the policy denies raised `PolicyError("query evaluation failed: access
    // denied by policy")` from 0.23.0, where 0.22.0 had said "access denied by policy". The
    // catch-all keeps the prefix, since there it is the only word on what kind of failure
    // this was.
    match e.kind().0 {
        "syntax" => SyntaxError::new_err(e.detail()),
        "refused" => QueryRefusedError::new_err(e.detail()),
        "timeout" => QueryTimeoutError::new_err(e.detail()),
        "policy" => PolicyError::new_err(e.detail()),
        _ => HolosError::new_err(e.to_string()),
    }
}

/// An error raised while the results are drained, mapped as one raised before them would be.
/// A timeout usually lands here, between two rows, and used to arrive as a bare `HolosError`.
fn map_evaluation_error(e: spareval::QueryEvaluationError) -> PyErr {
    map_engine_error(&holos_engine::EngineError::from(e))
}

fn map_store_error(e: &holos_store::StorageError) -> PyErr {
    HolosError::new_err(e.to_string())
}

// ---------------------------------------------------------------------------------
// terms — kept deliberately thin
// ---------------------------------------------------------------------------------

/// Converts an RDF term into the closest natural Python value.
///
/// Literals of the numeric and boolean XSD types become `int`, `float` and `bool`; anything
/// else becomes `str`. That is lossy, which is why [`QuerySolution::term`] exists alongside
/// it to hand back the full N-Triples form when the distinction matters.
fn term_to_py(py: Python<'_>, term: &Term) -> PyResult<Py<PyAny>> {
    use oxrdf::vocab::xsd;
    Ok(match term {
        Term::NamedNode(n) => n.as_str().into_pyobject(py)?.into_any().unbind(),
        Term::BlankNode(b) => format!("_:{}", b.as_str())
            .into_pyobject(py)?
            .into_any()
            .unbind(),
        Term::Literal(l) => {
            let dt = l.datatype();
            if dt == xsd::BOOLEAN {
                (l.value() == "true")
                    .into_pyobject(py)?
                    .to_owned()
                    .into_any()
                    .unbind()
            } else if dt == xsd::INTEGER || dt == xsd::LONG || dt == xsd::INT {
                match l.value().parse::<i64>() {
                    Ok(v) => v.into_pyobject(py)?.into_any().unbind(),
                    Err(_) => l.value().into_pyobject(py)?.into_any().unbind(),
                }
            } else if dt == xsd::DOUBLE || dt == xsd::FLOAT || dt == xsd::DECIMAL {
                match l.value().parse::<f64>() {
                    Ok(v) => v.into_pyobject(py)?.into_any().unbind(),
                    Err(_) => l.value().into_pyobject(py)?.into_any().unbind(),
                }
            } else {
                l.value().into_pyobject(py)?.into_any().unbind()
            }
        }
        other => other.to_string().into_pyobject(py)?.into_any().unbind(),
    })
}

fn parse_named_node(iri: &str) -> PyResult<NamedNode> {
    NamedNode::new(iri)
        .map_err(|e| PyValueError::new_err(format!("`{iri}` is not a valid IRI: {e}")))
}

// ---------------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------------

/// Who is asking.
///
/// ```python
/// alice = Principal("urn:user:alice", roles=["hr"], clearance=3)
/// ```
#[pyclass(module = "holosdb", name = "Principal", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyPrincipal {
    inner: CorePrincipal,
}

#[pymethods]
impl PyPrincipal {
    #[new]
    #[pyo3(signature = (id=None, *, roles=None, clearance=None))]
    fn new(id: Option<&str>, roles: Option<Vec<String>>, clearance: Option<u16>) -> PyResult<Self> {
        let mut principal = match id {
            None => CorePrincipal::anonymous(),
            Some(id) => CorePrincipal::new(parse_named_node(id)?),
        };
        for role in roles.unwrap_or_default() {
            principal = principal.with_role(role);
        }
        if let Some(level) = clearance {
            principal = principal.with_clearance(Label::level(level));
        }
        Ok(Self { inner: principal })
    }

    /// An unauthenticated principal: no roles, no clearance.
    #[staticmethod]
    fn anonymous() -> Self {
        Self {
            inner: CorePrincipal::anonymous(),
        }
    }

    fn __repr__(&self) -> String {
        format!("Principal({:?})", self.inner.id.as_str())
    }
}

// ---------------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------------

/// What anyone is allowed to see.
///
/// Rules are added with a fluent API that mirrors the command line's flags:
///
/// ```python
/// policy = (Policy.deny_all()
///           .allow_graph("http://example.com/public")
///           .deny_predicate("http://example.com/salary", except_role="hr")
///           .label_graph("http://example.com/reviews", 3))
/// ```
///
/// More specific scopes win. At equal specificity **deny beats allow**, which is why
/// `except_role` exists: "deny salary to everyone except HR" is otherwise inexpressible,
/// because an allow rule for HR would never win against the deny.
#[pyclass(module = "holosdb", name = "Policy", from_py_object)]
#[derive(Clone)]
pub struct PyPolicy {
    inner: CorePolicy,
}

#[pymethods]
impl PyPolicy {
    /// Permit everything. The default, and the right starting point for a private store.
    #[new]
    fn new() -> Self {
        Self {
            inner: CorePolicy::permit_all(),
        }
    }

    /// Refuse everything not explicitly granted.
    #[staticmethod]
    fn deny_all() -> Self {
        Self {
            inner: CorePolicy::default(),
        }
    }

    /// Grant read on one named graph.
    fn allow_graph(&self, iri: &str) -> PyResult<Self> {
        Ok(Self {
            inner: self.inner.clone().with_rule(Rule::allow(
                Modes::READ,
                Scope::Graph(parse_named_node(iri)?),
                PrincipalMatch::Everyone,
            )),
        })
    }

    /// Refuse read on one predicate, optionally exempting a role.
    #[pyo3(signature = (iri, *, except_role=None))]
    fn deny_predicate(&self, iri: &str, except_role: Option<&str>) -> PyResult<Self> {
        let who = match except_role {
            None => PrincipalMatch::Everyone,
            Some(role) => PrincipalMatch::Not(Box::new(PrincipalMatch::Role(role.to_owned()))),
        };
        Ok(Self {
            inner: self.inner.clone().with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(parse_named_node(iri)?),
                who,
            )),
        })
    }

    /// Grant read on one predicate.
    fn allow_predicate(&self, iri: &str) -> PyResult<Self> {
        Ok(Self {
            inner: self.inner.clone().with_rule(Rule::allow(
                Modes::READ,
                Scope::Predicate(parse_named_node(iri)?),
                PrincipalMatch::Everyone,
            )),
        })
    }

    /// Classify a graph at a level. A principal reads it only if its clearance dominates.
    fn label_graph(&self, iri: &str, level: u16) -> PyResult<Self> {
        Ok(Self {
            inner: self
                .inner
                .clone()
                .with_graph_label(parse_named_node(iri)?, Label::level(level)),
        })
    }

    /// Raise on refusal instead of filtering silently.
    ///
    /// Filtering is the default because the filtered answer is the *correct* answer for
    /// that principal. Switch to failing when a partial answer would be misread as a
    /// complete one — a compliance report, a reconciliation total.
    fn fail_closed(&self) -> Self {
        Self {
            inner: self.inner.clone().with_semantics(Semantics::Fail),
        }
    }

    fn __repr__(&self) -> String {
        "Policy(...)".to_owned()
    }
}

// ---------------------------------------------------------------------------------
// query results
// ---------------------------------------------------------------------------------

/// One row of a SELECT result.
#[pyclass(module = "holosdb", name = "QuerySolution")]
pub struct PyQuerySolution {
    names: Arc<Vec<String>>,
    values: Vec<Option<Term>>,
}

#[pymethods]
impl PyQuerySolution {
    /// Look a binding up by variable name (no leading `?`) or by position.
    fn __getitem__(&self, py: Python<'_>, key: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let index = if let Ok(name) = key.extract::<String>() {
            self.names
                .iter()
                .position(|n| *n == name)
                .ok_or_else(|| PyKeyError::new_err(format!("no variable named {name:?}")))?
        } else {
            key.extract::<usize>()?
        };
        match self.values.get(index).and_then(Option::as_ref) {
            None => Ok(py.None()),
            Some(term) => term_to_py(py, term),
        }
    }

    fn __len__(&self) -> usize {
        self.values.len()
    }

    /// The variable names, in projection order.
    #[getter]
    fn variables(&self) -> Vec<String> {
        self.names.as_ref().clone()
    }

    /// The raw N-Triples form of one binding, when the Python conversion would lose
    /// something that matters — a language tag, an unusual datatype, a blank node label.
    fn term(&self, name: &str) -> PyResult<Option<String>> {
        let index = self
            .names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| PyKeyError::new_err(format!("no variable named {name:?}")))?;
        Ok(self.values[index].as_ref().map(ToString::to_string))
    }

    /// As a plain dict.
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let dict = PyDict::new(py);
        for (name, value) in self.names.iter().zip(&self.values) {
            match value {
                None => dict.set_item(name, py.None())?,
                Some(term) => dict.set_item(name, term_to_py(py, term)?)?,
            }
        }
        Ok(dict.into_any().unbind())
    }

    fn __repr__(&self) -> String {
        let pairs: Vec<String> = self
            .names
            .iter()
            .zip(&self.values)
            .map(|(n, v)| match v {
                None => format!("{n}=None"),
                Some(t) => format!("{n}={t}"),
            })
            .collect();
        format!("QuerySolution({})", pairs.join(", "))
    }
}

// ---------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------

/// An RDF 1.2 quad store.
///
/// ```python
/// from holosdb import Store
///
/// store = Store()                       # in memory
/// store = Store("./var/db")             # persistent, needs a RocksDB build
/// store.load("data.trig")
/// for row in store.query("SELECT * WHERE { ?s ?p ?o }"):
///     print(row["s"])
/// ```
///
/// Only one process may hold a persistent store directory at a time — RocksDB takes an
/// exclusive lock on it.
#[pyclass(module = "holosdb", name = "Store")]
pub struct PyStore {
    // An RwLock rather than a Mutex because the store is genuinely many-readers /
    // single-writer, and queries — the common case — only need a read.
    //
    // The Option is what makes `close()` mean something. Dropping the last Python
    // reference is not enough on its own: a persistent store holds an exclusive RocksDB
    // lock on its directory for as long as the engine exists, and on Windows that also
    // prevents the directory being deleted. Callers need a way to say "done with this
    // now" that does not depend on when the garbage collector gets round to it.
    engine: Arc<RwLock<Option<Engine>>>,
    path: Option<String>,
    // The statistics `reorder=True` plans with, and the write generation they describe. A
    // write moves the generation and the next reordered query rebuilds them -- or loads them,
    // since `Statistics::cached` keeps a snapshot with a persistent store.
    statistics: std::sync::Mutex<Option<(u64, Arc<holos_stats::Statistics>)>>,
}

/// The error a caller gets for touching a store after closing it.
fn closed() -> PyErr {
    HolosError::new_err("this Store has been closed")
}

fn poisoned() -> PyErr {
    HolosError::new_err("the store lock is poisoned")
}

impl PyStore {
    fn with_engine<R>(&self, f: impl FnOnce(&Engine) -> PyResult<R>) -> PyResult<R> {
        let guard = self.engine.read().map_err(|_| poisoned())?;
        f(guard.as_ref().ok_or_else(closed)?)
    }

    fn with_engine_mut<R>(&self, f: impl FnOnce(&mut Engine) -> PyResult<R>) -> PyResult<R> {
        let mut guard = self.engine.write().map_err(|_| poisoned())?;
        f(guard.as_mut().ok_or_else(closed)?)
    }

    /// Statistics for the current state of the store, built or loaded at most once per write.
    fn statistics(&self) -> PyResult<Arc<holos_stats::Statistics>> {
        let generation = self.with_engine(|e| Ok(e.store().generation()))?;
        if let Some((cached, stats)) = self.statistics.lock().map_err(|_| poisoned())?.as_ref() {
            if *cached == generation {
                return Ok(Arc::clone(stats));
            }
        }
        // Mutable, because a persistent store keeps the snapshot it builds for next time.
        let (generation, stats) = self.with_engine_mut(|engine| {
            let stats = holos_stats::Statistics::cached(
                engine.store_mut(),
                holos_store::GraphFilter::Default,
            )
            .map_err(|e| HolosError::new_err(e.to_string()))?;
            Ok((engine.store().generation(), Arc::new(stats)))
        })?;
        *self.statistics.lock().map_err(|_| poisoned())? = Some((generation, Arc::clone(&stats)));
        Ok(stats)
    }
}

/// The options a query from Python runs with: the command line's, plus what was asked for.
///
/// The spill budget and the admission budget are the CLI's defaults, set whether or not
/// `reorder` is: admission needs statistics to estimate anything, so without them it never
/// refuses, and with them a `DISTINCT` over most of a large store is refused in a
/// millisecond -- or spilled -- rather than discovered by the operating system.
fn query_options(
    base_iri: Option<&str>,
    timeout: Option<f64>,
    statistics: Option<Arc<holos_stats::Statistics>>,
) -> PyResult<holos_engine::QueryOptions> {
    let mut options = holos_engine::QueryOptions::new()
        .spilling(holos_engine::spill::SPILL_BYTES)
        .with_blocking_budget(holos_engine::admit::DEFAULT_BLOCKING_ROWS);
    if let Some(base) = base_iri {
        options = options.with_base_iri(base);
    }
    if let Some(seconds) = timeout {
        if !(seconds.is_finite() && seconds > 0.0) {
            return Err(PyValueError::new_err(format!(
                "timeout must be a positive number of seconds, not {seconds}"
            )));
        }
        options = options.with_timeout(std::time::Duration::from_secs_f64(seconds));
    }
    if let Some(statistics) = statistics {
        options = options.reordering(statistics);
    }
    Ok(options)
}

/// Results drained into owned values, `limit` rows or triples at most. Stopping early stops
/// the evaluation too, since the evaluator produces rows only as they are asked for.
fn collect(results: QueryResults<'_>, limit: Option<usize>) -> PyResult<Collected> {
    let cap = limit.unwrap_or(usize::MAX);
    match results {
        QueryResults::Boolean(value) => Ok(Collected::Boolean(value)),
        QueryResults::Graph(triples) => {
            let mut out = Vec::new();
            for triple in triples.take(cap) {
                out.push(triple.map_err(map_evaluation_error)?.to_string());
            }
            Ok(Collected::Graph(out))
        }
        QueryResults::Solutions(solutions) => {
            let names: Arc<Vec<String>> = Arc::new(
                solutions
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect(),
            );
            let mut rows = Vec::new();
            for solution in solutions.take(cap) {
                let solution = solution.map_err(map_evaluation_error)?;
                rows.push(
                    names
                        .iter()
                        .map(|n| solution.get(n.as_str()).cloned())
                        .collect::<Vec<_>>(),
                );
            }
            Ok(Collected::Solutions(names, rows))
        }
    }
}

/// Every row read and discarded, for an explanation: its statistics are counted as rows go
/// through the operators, so a plan written before the results are drained shows zeroes.
fn drain(results: QueryResults<'_>) -> PyResult<()> {
    match results {
        QueryResults::Boolean(_) => {}
        QueryResults::Graph(triples) => {
            for triple in triples {
                triple.map_err(map_evaluation_error)?;
            }
        }
        QueryResults::Solutions(solutions) => {
            for solution in solutions {
                solution.map_err(map_evaluation_error)?;
            }
        }
    }
    Ok(())
}

#[pymethods]
impl PyStore {
    #[new]
    #[pyo3(signature = (path=None))]
    fn new(path: Option<String>) -> PyResult<Self> {
        let engine = match &path {
            None => Engine::new(),
            Some(p) => open_persistent(p)?,
        };
        Ok(Self {
            engine: Arc::new(RwLock::new(Some(engine))),
            path,
            statistics: std::sync::Mutex::new(None),
        })
    }

    /// Quads in the store.
    fn __len__(&self) -> PyResult<usize> {
        self.with_engine(|e| Ok(e.store().len()))
    }

    /// Distinct terms in the dictionary.
    ///
    /// Reliably smaller than you expect, because every integer, float, dateTime and short
    /// string is inlined into its 64-bit id and never reaches the dictionary at all.
    #[getter]
    fn dictionary_size(&self) -> PyResult<usize> {
        self.with_engine(|e| Ok(e.store().dictionary_len()))
    }

    /// The named graph IRIs.
    fn named_graphs(&self) -> PyResult<Vec<String>> {
        self.with_engine(|engine| {
            let store = engine.store();
            let ids = store.named_graphs().map_err(|e| map_store_error(&e))?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Ok(Some(term)) = store.decode_term(id) {
                    out.push(term.to_string());
                }
            }
            Ok(out)
        })
    }

    /// Whether this wheel was built with persistence.
    #[getter]
    fn is_persistent(&self) -> bool {
        self.path.is_some()
    }

    /// Load an RDF file, decompressing it if the name ends in `.gz`.
    ///
    /// The format comes from the extension unless `format` is given. `.nt.gz` and `.nq.gz`
    /// are how large RDF is distributed, and both are read as a stream.
    ///
    /// `bulk=True` buffers writes and skips the write-ahead log — about 2.4x faster, at the
    /// cost of an interrupted load having to be discarded rather than resumed.
    #[pyo3(signature = (path, *, format=None, graph=None, bulk=false))]
    fn load(
        &self,
        py: Python<'_>,
        path: &str,
        format: Option<&str>,
        graph: Option<&str>,
        bulk: bool,
    ) -> PyResult<usize> {
        let rdf_format = match format {
            Some(name) => format_by_name(name)?,
            None => format_for_path(path)?,
        };
        let graph_name = match graph {
            None => None,
            Some(iri) => Some(parse_named_node(iri)?),
        };
        // Decompressing when the name says to, and streamed either way — a 60 GB dump
        // costs no more memory than a small one.
        let reader = holos_engine::source::reader(Path::new(path))
            .map_err(|e| PyIOError::new_err(format!("opening {path}: {e}")))?;

        // Parsing and inserting touch no Python objects, and a large file takes minutes.
        py.detach(move || {
            let mut outer = self.engine.write().map_err(|_| poisoned())?;
            let guard = outer.as_mut().ok_or_else(closed)?;
            if bulk {
                guard
                    .store_mut()
                    .begin_bulk_load()
                    .map_err(|e| map_store_error(&e))?;
            }
            let result = match graph_name {
                None => guard.bulk_load(reader, rdf_format, None),
                Some(name) => guard.bulk_load_into_graph(reader, rdf_format, None, &name.into()),
            };
            if bulk {
                guard
                    .store_mut()
                    .end_bulk_load()
                    .map_err(|e| map_store_error(&e))?;
            }
            result.map_err(|e| map_engine_error(&e))
        })
    }

    /// Add one quad. Subject and predicate are IRIs; the object may be an IRI or a literal.
    #[pyo3(signature = (subject, predicate, object, graph=None))]
    fn add(
        &self,
        subject: &str,
        predicate: &str,
        object: &Bound<'_, PyAny>,
        graph: Option<&str>,
    ) -> PyResult<bool> {
        let quad = Quad {
            subject: NamedOrBlankNode::NamedNode(parse_named_node(subject)?),
            predicate: parse_named_node(predicate)?,
            object: py_to_term(object)?,
            graph_name: match graph {
                None => GraphName::DefaultGraph,
                Some(iri) => GraphName::NamedNode(parse_named_node(iri)?),
            },
        };
        self.with_engine_mut(|engine| {
            engine
                .store_mut()
                .insert(quad.as_ref())
                .map_err(|e| map_store_error(&e))
        })
    }

    /// Run a SPARQL 1.2 query.
    ///
    /// Returns a list of [`QuerySolution`] for SELECT, a `bool` for ASK, and a list of
    /// N-Triples strings for CONSTRUCT and DESCRIBE.
    ///
    /// With `principal` and/or `policy`, the answer is the one that principal is entitled
    /// to — enforced at the index scan, so it holds for every query shape including
    /// `COUNT` and `FILTER NOT EXISTS`.
    ///
    /// `timeout` is in seconds; a query that runs past it raises `QueryTimeoutError`.
    /// `reorder=True` plans with statistics over the store, built or loaded once per write,
    /// which on a large store is the difference between seconds and minutes for a badly
    /// ordered pattern; it also lets a `DISTINCT`, `ORDER BY` or `GROUP BY` estimated over
    /// ten million rows be refused at once with `QueryRefusedError`. `limit` returns at most
    /// that many rows or triples and stops the evaluation there: ask for one more than you
    /// will show to learn whether there were more.
    ///
    /// There is no memory ceiling here, unlike `holos-server`: enforcing one needs a counting
    /// allocator, which this library does not install in someone else's process.
    #[pyo3(signature = (query, *, principal=None, policy=None, base_iri=None, timeout=None, reorder=false, limit=None))]
    #[allow(clippy::too_many_arguments)]
    fn query(
        &self,
        py: Python<'_>,
        query: &str,
        principal: Option<PyPrincipal>,
        policy: Option<PyPolicy>,
        base_iri: Option<&str>,
        timeout: Option<f64>,
        reorder: bool,
        limit: Option<usize>,
    ) -> PyResult<Py<PyAny>> {
        let core_policy = policy.map_or_else(CorePolicy::permit_all, |p| p.inner);
        let core_principal = principal.map_or_else(CorePrincipal::anonymous, |p| p.inner);

        // Evaluate without the GIL, then build Python objects with it. Solutions borrow the
        // view, so they are drained into owned terms inside the detached section.
        let collected = py.detach(|| -> PyResult<Collected> {
            let statistics = if reorder {
                Some(self.statistics()?)
            } else {
                None
            };
            let options = query_options(base_iri, timeout, statistics)?;
            let outer = self.engine.read().map_err(|_| poisoned())?;
            let guard = outer.as_ref().ok_or_else(closed)?;
            let session = CoreSession::open(guard.store(), core_principal, core_policy)
                .map_err(|e| PolicyError::new_err(e.to_string()))?;
            let view = guard.view(&session);
            let (results, _) =
                Engine::query_with(&view, query, &options).map_err(|e| map_engine_error(&e))?;
            collect(results, limit)
        })?;

        match collected {
            Collected::Boolean(v) => Ok(v.into_pyobject(py)?.to_owned().into_any().unbind()),
            Collected::Graph(triples) => Ok(PyList::new(py, triples)?.into_any().unbind()),
            Collected::Solutions(names, rows) => {
                let list = PyList::empty(py);
                for values in rows {
                    list.append(Py::new(
                        py,
                        PyQuerySolution {
                            names: Arc::clone(&names),
                            values,
                        },
                    )?)?;
                }
                Ok(list.into_any().unbind())
            }
        }
    }

    /// The plan a query runs with, as a dict, with each operator's row count and time.
    ///
    /// The query is run to the end and its results discarded, because the counts are taken as
    /// rows pass through the operators. The plan is the evaluator's: a query the store answers
    /// by a faster route — the index join, or `SELECT DISTINCT ?p` from the predicate counts —
    /// is shown as the evaluator would run it, which is the plan to read when it is slow.
    /// The same arguments as `query`, without `limit`.
    #[pyo3(signature = (query, *, principal=None, policy=None, base_iri=None, timeout=None, reorder=false))]
    #[allow(clippy::too_many_arguments)]
    fn explain(
        &self,
        py: Python<'_>,
        query: &str,
        principal: Option<PyPrincipal>,
        policy: Option<PyPolicy>,
        base_iri: Option<&str>,
        timeout: Option<f64>,
        reorder: bool,
    ) -> PyResult<Py<PyAny>> {
        let core_policy = policy.map_or_else(CorePolicy::permit_all, |p| p.inner);
        let core_principal = principal.map_or_else(CorePrincipal::anonymous, |p| p.inner);
        let json = py.detach(|| -> PyResult<String> {
            let statistics = if reorder {
                Some(self.statistics()?)
            } else {
                None
            };
            let options = query_options(base_iri, timeout, statistics)?.explaining();
            let outer = self.engine.read().map_err(|_| poisoned())?;
            let guard = outer.as_ref().ok_or_else(closed)?;
            let session = CoreSession::open(guard.store(), core_principal, core_policy)
                .map_err(|e| PolicyError::new_err(e.to_string()))?;
            let view = guard.view(&session);
            let (results, explanation) =
                Engine::query_with(&view, query, &options).map_err(|e| map_engine_error(&e))?;
            drain(results)?;
            let explanation = explanation
                .ok_or_else(|| HolosError::new_err("the engine returned no plan for this query"))?;
            let mut out = Vec::new();
            explanation
                .write_in_json(&mut out)
                .map_err(|e| HolosError::new_err(e.to_string()))?;
            String::from_utf8(out).map_err(|e| HolosError::new_err(e.to_string()))
        })?;
        Ok(py.import("json")?.call_method1("loads", (json,))?.unbind())
    }

    /// Every predicate in the store and how many quads use it, most used first.
    ///
    /// The store's own counts, kept exactly on every write, so this costs nothing however
    /// large the store is. They cover **every graph** and **ignore policy**: they are the
    /// store owner's view. For what a given principal can see, ask
    /// `SELECT DISTINCT ?p WHERE { ?s ?p ?o }` with that principal, which the store also
    /// answers from these counts, confirming each predicate visible before naming it.
    fn predicate_counts(&self) -> PyResult<Vec<(String, u64)>> {
        self.with_engine(|engine| {
            let store = engine.store();
            let mut out = Vec::new();
            for (id, count) in store.predicate_histogram() {
                if let Some(Term::NamedNode(predicate)) =
                    store.decode_term(id).map_err(|e| map_store_error(&e))?
                {
                    out.push((predicate.as_str().to_owned(), count));
                }
            }
            Ok(out)
        })
    }

    /// Applies a SPARQL 1.1 update.
    ///
    /// Returns what changed:
    /// `{"inserted": n, "deleted": n, "graphsCreated": n, "graphsDropped": n}`.
    ///
    /// **All-or-nothing.** If any operation fails the store is left exactly as it was, so
    /// a refused write cannot leave half an update behind. Policy applies to every quad
    /// written, and the WHERE clause is filtered by read policy on the same path as a
    /// SELECT — a principal cannot delete what it cannot see.
    #[pyo3(signature = (update, *, principal=None, policy=None, base_iri=None))]
    fn update(
        &self,
        py: Python<'_>,
        update: &str,
        principal: Option<PyPrincipal>,
        policy: Option<PyPolicy>,
        base_iri: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        let core_policy = policy.map_or_else(CorePolicy::permit_all, |p| p.inner);
        let core_principal = principal.map_or_else(CorePrincipal::anonymous, |p| p.inner);
        let update = update.to_owned();
        let base = base_iri.map(ToOwned::to_owned);

        let outcome = py.detach(move || -> PyResult<holos_engine::update::UpdateOutcome> {
            let mut outer = self.engine.write().map_err(|_| poisoned())?;
            let engine = outer.as_mut().ok_or_else(closed)?;
            let mut session = CoreSession::open(engine.store(), core_principal, core_policy)
                .map_err(|e| PolicyError::new_err(e.to_string()))?;
            holos_engine::update::update(engine, &mut session, &update, base.as_deref()).map_err(
                |e| match e {
                    holos_engine::EngineError::AccessDenied => PolicyError::new_err(e.to_string()),
                    other => map_engine_error(&other),
                },
            )
        })?;

        let dict = PyDict::new(py);
        dict.set_item("inserted", outcome.inserted)?;
        dict.set_item("deleted", outcome.deleted)?;
        dict.set_item("graphsCreated", outcome.graphs_created)?;
        dict.set_item("graphsDropped", outcome.graphs_dropped)?;
        Ok(dict.into_any().unbind())
    }

    /// Validate against SHACL shapes loaded from a file.
    ///
    /// The shapes are read alongside the data but not kept: nothing of them is in the store
    /// afterwards, so validating twice gives the same answer twice.
    ///
    /// Returns `conforms`, `violations` (how many results, of every severity), `engine`, and
    /// `results`: one dict per result with `focus`, `path`, `value`, `severity`
    /// (`Violation`, `Warning` or `Info`), `component`, `shape` and `message`, each `None`
    /// where the report gives none. IRIs come as themselves, blank nodes as `_:label`, and a
    /// value literal in N-Triples form so that its datatype survives.
    ///
    /// `engine="adapted"` covers considerably more of SHACL; `engine="native"` reads the
    /// live store and is the only one that can revalidate a delta. `adapted` is the default
    /// here and for `holos validate` alike.
    #[pyo3(signature = (shapes, *, engine="adapted"))]
    fn validate(&self, py: Python<'_>, shapes: &str, engine: &str) -> PyResult<Py<PyAny>> {
        let (conforms, count, rows, leftovers) =
            py.detach(|| validate_against(&self.engine, shapes, engine))?;
        for (graph, quads) in leftovers {
            PyErr::warn(
                py,
                &py.get_type::<pyo3::exceptions::PyUserWarning>(),
                &std::ffi::CString::new(format!(
                    "<{graph}> holds {quads} quads that an earlier Store.validate left in this \
                     store; they are not read, and store.update(\"DROP GRAPH <{graph}>\") \
                     removes them"
                ))
                .unwrap_or_default(),
                1,
            )?;
        }
        let dict = PyDict::new(py);
        dict.set_item("conforms", conforms)?;
        dict.set_item("violations", count)?;
        dict.set_item("engine", engine)?;
        let results = PyList::empty(py);
        for row in rows {
            let result = PyDict::new(py);
            result.set_item("focus", row.focus)?;
            result.set_item("path", row.path)?;
            result.set_item("value", row.value)?;
            result.set_item("severity", row.severity)?;
            result.set_item("component", row.component)?;
            result.set_item("shape", row.shape)?;
            result.set_item("message", row.message)?;
            results.append(result)?;
        }
        dict.set_item("results", results)?;
        Ok(dict.into_any().unbind())
    }

    /// Release the store, and with it the exclusive lock on its directory.
    ///
    /// Dropping the last Python reference is not enough on its own — the engine lives
    /// until the garbage collector says otherwise, and until then no other process (and on
    /// Windows, no attempt to delete the directory) can touch it. Closing twice is fine.
    ///
    /// Prefer the context manager, which cannot be forgotten:
    ///
    /// ```python
    /// with Store("./var/db") as store:
    ///     store.query(...)
    /// ```
    fn close(&self) -> PyResult<()> {
        let mut guard = self.engine.write().map_err(|_| poisoned())?;
        if let Some(mut engine) = guard.take() {
            // Flush before dropping: a clean close means the next open replays no
            // write-ahead log.
            engine
                .store_mut()
                .flush()
                .map_err(|e| map_store_error(&e))?;
        }
        Ok(())
    }

    /// Whether [`close`] has been called.
    #[getter]
    fn is_closed(&self) -> PyResult<bool> {
        Ok(self.engine.read().map_err(|_| poisoned())?.is_none())
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) -> PyResult<bool> {
        self.close()?;
        // False: never swallow an exception raised inside the block.
        Ok(false)
    }

    fn __repr__(&self) -> PyResult<String> {
        if self.engine.read().map_err(|_| poisoned())?.is_none() {
            return Ok("Store(closed)".to_owned());
        }
        let n = self.with_engine(|e| Ok(e.store().len()))?;
        Ok(match &self.path {
            None => format!("Store(in memory, {n} quads)"),
            Some(p) => format!("Store({p:?}, {n} quads)"),
        })
    }
}

enum Collected {
    Boolean(bool),
    Graph(Vec<String>),
    Solutions(Arc<Vec<String>>, Vec<Vec<Option<Term>>>),
}

// ---------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------

#[cfg(feature = "rocksdb")]
fn open_persistent(path: &str) -> PyResult<Engine> {
    let storage = holos_store::RocksStorage::open(Path::new(path))
        .map_err(|e| PyIOError::new_err(format!("opening the store at {path}: {e}")))?;
    Ok(Engine::with_store(holos_store::Store::with_storage(
        storage,
    )))
}

#[cfg(not(feature = "rocksdb"))]
fn open_persistent(_path: &str) -> PyResult<Engine> {
    Err(pyo3::exceptions::PyNotImplementedError::new_err(
        "this wheel was built without persistence; install the default holos wheel, or \
         construct Store() with no path for an in-memory store",
    ))
}

fn py_to_term(value: &Bound<'_, PyAny>) -> PyResult<Term> {
    use oxrdf::Literal;
    if let Ok(b) = value.extract::<bool>() {
        return Ok(Literal::from(b).into());
    }
    if let Ok(i) = value.extract::<i64>() {
        return Ok(Literal::from(i).into());
    }
    if let Ok(f) = value.extract::<f64>() {
        return Ok(Literal::from(f).into());
    }
    let s: String = value
        .extract()
        .map_err(|_| PyValueError::new_err("object must be a str, int, float or bool"))?;
    // A bare string that parses as an absolute IRI is meant as one; anything else is a
    // plain literal. Wrap in <> to force an IRI, or pass a literal to force a literal.
    if let Some(inner) = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        return Ok(parse_named_node(inner)?.into());
    }
    Ok(Literal::new_simple_literal(s).into())
}

fn format_by_name(name: &str) -> PyResult<RdfFormat> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "turtle" | "ttl" => RdfFormat::Turtle,
        "ntriples" | "nt" | "n-triples" => RdfFormat::NTriples,
        "trig" => RdfFormat::TriG,
        "nquads" | "nq" | "n-quads" => RdfFormat::NQuads,
        "rdfxml" | "rdf" | "xml" => RdfFormat::RdfXml,
        "n3" => RdfFormat::N3,
        "jsonld" | "json-ld" => RdfFormat::JsonLd {
            profile: oxrdfio::JsonLdProfileSet::empty(),
        },
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown RDF format {other:?}; expected turtle, ntriples, trig, nquads, \
                 rdfxml, n3 or jsonld"
            )))
        }
    })
}

/// The RDF format a file name implies, seeing through a `.gz` suffix.
fn format_for_path(path: &str) -> PyResult<RdfFormat> {
    holos_engine::source::format_for_path(Path::new(path)).ok_or_else(|| {
        PyValueError::new_err(format!(
            "cannot infer an RDF format from {path}; expected .ttl .nt .trig .nq .rdf .n3 \
             .jsonld, optionally with .gz, or pass format=..."
        ))
    })
}

/// One validation result, as text a caller can show or act on.
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct ResultRow {
    focus: Option<String>,
    path: Option<String>,
    component: Option<String>,
    severity: Option<String>,
    value: Option<String>,
    shape: Option<String>,
    message: Option<String>,
}

/// An IRI as itself, a blank node as `_:label`, a literal in its N-Triples form so that its
/// datatype or language survives.
fn term_text(term: &Term) -> String {
    match term {
        Term::NamedNode(n) => n.as_str().to_owned(),
        Term::BlankNode(b) => format!("_:{}", b.as_str()),
        other => other.to_string(),
    }
}

/// The results of a SHACL validation report, read the same way from either engine's report:
/// every `sh:result` and the properties the specification gives one. A path that is not a
/// single predicate comes back as the blank node that heads it.
fn results_from_report(triples: impl IntoIterator<Item = oxrdf::Triple>) -> Vec<ResultRow> {
    use std::collections::HashMap;
    const SH: &str = "http://www.w3.org/ns/shacl#";
    let result_predicate = format!("{SH}result");
    let mut by_subject: HashMap<NamedOrBlankNode, Vec<(String, Term)>> = HashMap::new();
    let mut results: Vec<NamedOrBlankNode> = Vec::new();
    for triple in triples {
        let predicate = triple.predicate.as_str().to_owned();
        if predicate == result_predicate {
            match &triple.object {
                Term::NamedNode(n) => results.push(n.clone().into()),
                Term::BlankNode(b) => results.push(b.clone().into()),
                _ => {}
            }
        }
        by_subject
            .entry(triple.subject)
            .or_default()
            .push((predicate, triple.object));
    }
    let mut rows: Vec<ResultRow> = results
        .iter()
        .map(|result| {
            let properties = by_subject
                .get(result)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let get = |local: &str| {
                let iri = format!("{SH}{local}");
                properties.iter().find(|(p, _)| *p == iri).map(|(_, o)| o)
            };
            ResultRow {
                focus: get("focusNode").map(term_text),
                path: get("resultPath").map(term_text),
                value: get("value").map(term_text),
                severity: get("resultSeverity").map(|t| {
                    let text = term_text(t);
                    text.strip_prefix(SH)
                        .map_or_else(|| text.clone(), str::to_owned)
                }),
                component: get("sourceConstraintComponent").map(term_text),
                shape: get("sourceShape").map(term_text),
                message: get("resultMessage").map(|t| match t {
                    Term::Literal(l) => l.value().to_owned(),
                    other => term_text(other),
                }),
            }
        })
        .collect();
    rows.sort();
    rows
}

/// Whether the data conforms, how many results, each result, and the shapes graphs an earlier
/// version left in the store (see `Engine::leftover_shapes_graphs`).
type Validated = (bool, usize, Vec<ResultRow>, Vec<(String, usize)>);

fn validate_against(
    engine: &Arc<RwLock<Option<Engine>>>,
    shapes_path: &str,
    which: &str,
) -> PyResult<Validated> {
    use holos_shacl::{CompiledShapes, Options as ShaclOptions};
    use holos_store::GraphFilter;

    if !matches!(which, "adapted" | "vendored" | "native") {
        return Err(PyValueError::new_err(format!(
            "unknown engine {which:?}; expected \"native\" or \"adapted\""
        )));
    }
    let format = format_for_path(shapes_path)?;
    let file = std::fs::File::open(shapes_path)
        .map_err(|e| PyIOError::new_err(format!("opening {shapes_path}: {e}")))?;

    let mut outer = engine.write().map_err(|_| poisoned())?;
    let guard = outer.as_mut().ok_or_else(closed)?;
    let leftovers = guard
        .leftover_shapes_graphs()
        .map_err(|e| map_engine_error(&e))?;

    // The shapes are read from the store, with the data's dictionary, but they are not the
    // store's: they go into a graph that is rolled back when validation ends. They used to
    // go into `urn:holos:python:shapes` and stay, so every call added another copy of every
    // blank-node shape and the counts climbed with the data unchanged.
    let (outcome, _) = guard
        .with_transient_graph(
            std::io::BufReader::new(file),
            format,
            None,
            |store, shapes_graph| -> PyResult<(bool, usize, Vec<ResultRow>)> {
                let options = ShaclOptions {
                    data_graph: GraphFilter::Default,
                    shapes_graph,
                };
                if which == "native" {
                    let shapes = CompiledShapes::compile(store, options)
                        .map_err(|e| HolosError::new_err(e.to_string()))?;
                    let report = shapes
                        .validate(store)
                        .map_err(|e| HolosError::new_err(e.to_string()))?;
                    // Read now: a result can name a shape's blank node, which exists only
                    // until the transient graph is rolled back.
                    let quads = shapes
                        .report_to_quads(store, &report)
                        .map_err(|e| HolosError::new_err(e.to_string()))?;
                    let rows = results_from_report(quads.into_iter().map(oxrdf::Triple::from));
                    Ok((report.conforms, report.results.len(), rows))
                } else {
                    // "vendored" was the earlier spelling of "adapted"; still accepted.
                    let mut run = holos_shacl::engine::EngineRun::prepare(store, options)
                        .map_err(|e| HolosError::new_err(e.to_string()))?;
                    let report = run
                        .validate()
                        .map_err(|e| HolosError::new_err(e.to_string()))?;
                    let graph = run.report_to_oxrdf(&report);
                    let rows = results_from_report(graph.iter().map(oxrdf::TripleRef::into_owned));
                    Ok((run.conforms(&report), report.results.len(), rows))
                }
            },
        )
        .map_err(|e| map_engine_error(&e))?;
    let (conforms, count, rows) = outcome?;
    Ok((conforms, count, rows, leftovers))
}

// ---------------------------------------------------------------------------------
// module
// ---------------------------------------------------------------------------------

/// Whether this build carries persistence.
///
/// A Python extra selects dependencies and cannot toggle compiled code, so this reports
/// what the wheel actually contains rather than what was requested at install time.
#[pyfunction]
fn has_rocksdb() -> bool {
    cfg!(feature = "rocksdb")
}

/// The geometry function IRIs this build registers.
///
/// Mostly `geof:`, plus `holos:transform`, which is here because GeoSPARQL defines no
/// transform function and one in the OGC namespace would claim a sanction it does not have.
#[pyfunction]
fn geosparql_functions() -> Vec<String> {
    let mut out: Vec<String> = spargeo_names();
    out.extend(
        holos_engine::geo_ext::function_iris()
            .into_iter()
            .map(|n| n.as_str().to_owned()),
    );
    out.sort();
    // Several of `geo_ext`'s entries *replace* one of `spargeo`'s rather than adding to
    // them -- the four set operations, `geof:distance` and `geof:getSRID` -- so
    // concatenating listed each of those twice. The evaluator itself keeps one registration
    // per IRI, so a list that showed duplicates was describing something that does not
    // exist.
    out.dedup();
    out
}

fn spargeo_names() -> Vec<String> {
    spargeo::GEOSPARQL_EXTENSION_FUNCTIONS
        .iter()
        .map(|(iri, _)| iri.as_str().to_owned())
        .collect()
}

#[pymodule]
fn _holosdb(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("HolosError", module.py().get_type::<HolosError>())?;
    module.add("SyntaxError", module.py().get_type::<SyntaxError>())?;
    module.add("PolicyError", module.py().get_type::<PolicyError>())?;
    module.add(
        "QueryRefusedError",
        module.py().get_type::<QueryRefusedError>(),
    )?;
    module.add(
        "QueryTimeoutError",
        module.py().get_type::<QueryTimeoutError>(),
    )?;
    module.add_class::<PyStore>()?;
    module.add_class::<PyPrincipal>()?;
    module.add_class::<PyPolicy>()?;
    module.add_class::<PyQuerySolution>()?;
    module.add_function(wrap_pyfunction!(has_rocksdb, module)?)?;
    module.add_function(wrap_pyfunction!(geosparql_functions, module)?)?;
    Ok(())
}
