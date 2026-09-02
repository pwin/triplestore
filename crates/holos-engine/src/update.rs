//! SPARQL 1.1 Update.
//!
//! # What is reused, and what is not
//!
//! `spargebra` parses the whole of Update, and `spareval::prepare_delete_insert` evaluates
//! the `WHERE` clause of `DELETE/INSERT` and instantiates the templates — which is the
//! hardest part, and it runs through [`DatasetView`] like any other read. That has a
//! consequence worth stating plainly: **the `WHERE` clause of an update is filtered by read
//! policy for free**, on exactly the same code path as a `SELECT`. A principal cannot
//! delete what it cannot see, because the pattern never matches it.
//!
//! What is written here is the part no library can supply: dispatching the eight operation
//! kinds onto a real store, checking write policy on every quad, and making the whole
//! update all-or-nothing.
//!
//! # Atomicity, honestly described
//!
//! An update is a sequence of operations, and SPARQL requires each to see the effects of
//! the ones before it. So they are applied in order, inside one [`Store`](holos_store::Store) commit scope: on
//! a persistent backend the writes accumulate into a single batch and are written once, and
//! every read the update itself makes is answered through that batch. If any operation
//! fails the scope is abandoned, and on a persistent backend that means nothing was ever
//! written.
//!
//! This gives **failure atomicity**, and — new with the scope — **crash atomicity**: a
//! process that dies mid-update leaves the store as it was, rather than with the operations
//! that happened to have been written already. What replaced a hand-rolled undo log is
//! worth stating: that log rolled back by writing the inverse of each change, so a rollback
//! could itself fail and leave a state nobody could describe. Abandoning a batch that was
//! never written cannot.
//!
//! It does **not** give isolation. A concurrent reader holding its own view can observe the
//! store before the commit or after it, and this makes no promise about which. Real
//! isolation needs the MVCC in `DESIGN.md` §6.1, which is not built. The `Engine` is behind
//! an `RwLock` in both the server and the Python binding, so in those two deployments a
//! writer excludes readers for the update's duration and the distinction does not arise; a
//! caller driving the `Engine` directly should know it does.

use crate::view::DatasetView;
use crate::{Engine, EngineError};
use holos_security::Session;
use oxrdf::{GraphName, GraphNameRef, NamedNode, Quad, Term};
use oxrdfio::RdfParser;
use spareval::DeleteInsertQuad;
use spargebra::algebra::GraphTarget;
use spargebra::term::{GroundQuad, GroundQuadPattern, GroundTerm, GroundTriple, QuadPattern};
use spargebra::{GraphUpdateOperation, SparqlParser, Update};

/// What an update changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// Quads that were not already present and now are.
    pub inserted: u64,
    /// Quads that were present and now are not.
    pub deleted: u64,
    /// Named graphs created, whether or not they hold anything.
    pub graphs_created: u64,
    /// Named graphs removed.
    pub graphs_dropped: u64,
}

/// Applies a SPARQL 1.1 update.
///
/// # Errors
///
/// Returns the first failure, having already restored the store to its prior contents.
/// A policy refusal is [`EngineError::AccessDenied`].
pub fn update(
    engine: &mut Engine,
    session: &mut Session,
    update: &str,
    base_iri: Option<&str>,
) -> Result<UpdateOutcome, EngineError> {
    let parsed = parse(update, base_iri)?;
    apply(engine, session, &parsed)
}

/// Parses an update without applying it.
///
/// Separate so a caller that has to adjust the parsed form first — the SPARQL Protocol's
/// `using-graph-uri`, which names a dataset outside the update text — can do so without
/// reaching for string surgery on the request.
///
/// # Errors
///
/// [`EngineError::Syntax`] for an unparseable update, or an unusable base IRI.
pub fn parse(update: &str, base_iri: Option<&str>) -> Result<Update, EngineError> {
    let mut parser = SparqlParser::new();
    if let Some(base) = base_iri {
        parser = parser
            .with_base_iri(base)
            .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;
    }
    Ok(parser.parse_update(update)?)
}

/// Applies the protocol's `using-graph-uri` / `using-named-graph-uri` to a parsed update.
///
/// The SPARQL Protocol lets a client name the dataset an update's `WHERE` runs against in
/// the request rather than the update text. Since that is exactly what `USING` does, this
/// sets `USING` on every operation that has a `WHERE` to run.
///
/// # Both at once is an error
///
/// The protocol says so in as many words: a request carrying these parameters *and* an
/// update carrying `USING` or `WITH` is a client error, not something to resolve by
/// preferring one. Silently overriding would run the update over a dataset the author of
/// the text did not choose.
///
/// Operations with no `WHERE` — `INSERT DATA`, `LOAD`, `CLEAR` — have no dataset to name
/// and are left alone.
///
/// # Errors
///
/// [`EngineError::Syntax`] when an operation already names its own dataset.
pub fn with_protocol_dataset(
    parsed: &mut Update,
    default_graphs: Vec<NamedNode>,
    named_graphs: Vec<NamedNode>,
) -> Result<(), EngineError> {
    if default_graphs.is_empty() && named_graphs.is_empty() {
        return Ok(());
    }
    let dataset = spargebra::algebra::QueryDataset {
        default: default_graphs,
        named: (!named_graphs.is_empty()).then_some(named_graphs),
    };
    for operation in &mut parsed.operations {
        if let GraphUpdateOperation::DeleteInsert { using, .. } = operation {
            if using.is_some() {
                return Err(EngineError::BadRequest(
                    "the request names a dataset with using-graph-uri and the update also \
                     names one with USING or WITH; the protocol permits only one"
                        .to_owned(),
                ));
            }
            *using = Some(dataset.clone());
        }
    }
    Ok(())
}

/// Applies an already-parsed update.
///
/// Separate from [`update`] so a conformance harness can tell a syntax failure apart from
/// an evaluation failure, exactly as the query path does.
pub fn apply(
    engine: &mut Engine,
    session: &mut Session,
    parsed: &Update,
) -> Result<UpdateOutcome, EngineError> {
    // A caller may already have one open — a holon tick applies its delta inside one, and an
    // update run from there belongs to that commit rather than to a commit of its own. Then
    // the rollback point is the caller's: an operation that fails still unwinds everything,
    // just at the outer boundary.
    let owned = !engine.store().in_scope();
    if owned {
        engine.store_mut().begin()?;
    }

    let mut outcome = UpdateOutcome::default();
    for operation in &parsed.operations {
        if let Err(e) = apply_one(engine, session, operation, &mut outcome) {
            if owned {
                engine.store_mut().rollback();
            }
            return Err(e);
        }
    }
    if owned {
        engine.store_mut().commit()?;
    }
    Ok(outcome)
}

fn apply_one(
    engine: &mut Engine,
    session: &mut Session,
    operation: &GraphUpdateOperation,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    match operation {
        GraphUpdateOperation::InsertData { data } => {
            for quad in data {
                insert(engine, session, &algebra_quad(quad), outcome)?;
            }
            Ok(())
        }

        GraphUpdateOperation::DeleteData { data } => {
            for quad in data {
                remove(engine, session, &ground_quad(quad), outcome)?;
            }
            Ok(())
        }

        GraphUpdateOperation::DeleteInsert {
            delete,
            insert: insert_template,
            using,
            pattern,
        } => delete_insert(
            engine,
            session,
            delete,
            insert_template,
            using.clone(),
            pattern,
            outcome,
        ),

        GraphUpdateOperation::Load {
            silent,
            source,
            destination,
        } => {
            let result = load(
                engine,
                session,
                source,
                &algebra_graph_name(destination),
                outcome,
            );
            silence(result, *silent)
        }

        GraphUpdateOperation::Clear { silent, graph } => {
            let result = clear(engine, session, graph, outcome);
            silence(result, *silent)
        }

        GraphUpdateOperation::Create { silent, graph } => {
            let result = create(engine, graph, outcome);
            silence(result, *silent)
        }

        GraphUpdateOperation::Drop { silent, graph } => {
            let result = drop_graph(engine, session, graph, outcome);
            silence(result, *silent)
        }
    }
}

/// `SILENT` suppresses the operation's own failure, but never a policy refusal.
///
/// The specification says a silent operation must not report *its* error. It does not say a
/// principal may quietly bypass authorisation — treating a denial as "silently fine" would
/// turn `SILENT` into a way to probe what one is not allowed to touch.
fn silence(result: Result<(), EngineError>, silent: bool) -> Result<(), EngineError> {
    match result {
        Err(EngineError::AccessDenied) => Err(EngineError::AccessDenied),
        Err(_) if silent => Ok(()),
        other => other,
    }
}

// ---------------------------------------------------------------------------------
// the two primitives everything else is built from
// ---------------------------------------------------------------------------------

fn insert(
    engine: &mut Engine,
    session: &mut Session,
    quad: &Quad,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    if engine.insert(session, quad.as_ref())? {
        outcome.inserted += 1;
    }
    Ok(())
}

fn remove(
    engine: &mut Engine,
    session: &mut Session,
    quad: &Quad,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    if engine.remove(session, quad.as_ref())? {
        outcome.deleted += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------
// DELETE / INSERT ... WHERE
// ---------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn delete_insert(
    engine: &mut Engine,
    session: &mut Session,
    delete: &[GroundQuadPattern],
    insert_template: &[QuadPattern],
    using: Option<spargebra::algebra::QueryDataset>,
    pattern: &spargebra::algebra::GraphPattern,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    // The delta is collected in full before anything is applied. That is forced by the
    // borrow checker — evaluating needs `&self`, applying needs `&mut self` — and it is
    // also what SPARQL requires: the templates are instantiated against the state *before*
    // the operation, not against a store changing underneath the iterator.
    let (to_delete, to_insert) = {
        let view = engine.view(session);
        let evaluator = Engine::evaluator();
        let prepared = evaluator.prepare_delete_insert(
            delete.to_vec(),
            insert_template.to_vec(),
            None,
            using,
            pattern,
        );
        let mut to_delete = Vec::new();
        let mut to_insert = Vec::new();
        for item in prepared.execute(&view)? {
            match item? {
                DeleteInsertQuad::Delete(quad) => to_delete.push(quad),
                DeleteInsertQuad::Insert(quad) => to_insert.push(quad),
            }
        }
        (to_delete, to_insert)
    };

    // Deletions first, then insertions: SPARQL 1.1 Update §3.1.3. It matters whenever the
    // two templates overlap — `DELETE { ?s ?p ?o } INSERT { ?s ?p ?new }` on the same
    // subject would otherwise remove what it had just written.
    for quad in to_delete {
        remove(engine, session, &quad, outcome)?;
    }
    for quad in to_insert {
        insert(engine, session, &quad, outcome)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------
// graph-level operations
// ---------------------------------------------------------------------------------

fn create(
    engine: &mut Engine,
    graph: &NamedNode,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    let name = GraphName::NamedNode(graph.clone());
    if engine.store().contains_named_graph(name.as_ref())? {
        return Err(EngineError::Io(std::io::Error::other(format!(
            "the graph {graph} already exists"
        ))));
    }
    if engine.store_mut().insert_named_graph(&name)? {
        outcome.graphs_created += 1;
    }
    Ok(())
}

/// Every quad a target names, as a list, so the store can be mutated while iterating.
fn quads_in(
    engine: &Engine,
    session: &Session,
    target: &GraphTarget,
) -> Result<Vec<Quad>, EngineError> {
    let view = engine.view(session);
    Ok(match target {
        GraphTarget::DefaultGraph => quads_in_graph(&view, Some(&GraphName::DefaultGraph))?,
        GraphTarget::NamedNode(name) => {
            quads_in_graph(&view, Some(&GraphName::NamedNode(name.clone())))?
        }
        // "All graphs" includes the default graph; "named graphs" does not.
        GraphTarget::AllGraphs => quads_in_graph(&view, None)?,
        GraphTarget::NamedGraphs => quads_in_graph(&view, None)?
            .into_iter()
            .filter(|q| q.graph_name != GraphName::DefaultGraph)
            .collect(),
    })
}

fn clear(
    engine: &mut Engine,
    session: &mut Session,
    target: &GraphTarget,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    if let GraphTarget::NamedNode(name) = target {
        if !engine
            .store()
            .contains_named_graph(GraphNameRef::NamedNode(name.as_ref()))?
        {
            return Err(EngineError::Io(std::io::Error::other(format!(
                "no such graph: {name}"
            ))));
        }
    }
    for quad in quads_in(engine, session, target)? {
        remove(engine, session, &quad, outcome)?;
    }
    Ok(())
}

fn drop_graph(
    engine: &mut Engine,
    session: &mut Session,
    target: &GraphTarget,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    // DROP is CLEAR plus removing the graph itself from the catalogue.
    clear(engine, session, target, outcome)?;

    let names: Vec<NamedNode> = match target {
        GraphTarget::NamedNode(name) => vec![name.clone()],
        GraphTarget::NamedGraphs | GraphTarget::AllGraphs => named_graph_nodes(engine)?,
        // The default graph cannot be dropped, only emptied, which CLEAR above did.
        GraphTarget::DefaultGraph => Vec::new(),
    };
    for name in names {
        if engine
            .store_mut()
            .remove_named_graph(GraphNameRef::NamedNode(name.as_ref()))?
        {
            outcome.graphs_dropped += 1;
        }
    }
    Ok(())
}

fn named_graph_nodes(engine: &Engine) -> Result<Vec<NamedNode>, EngineError> {
    let store = engine.store();
    let mut out = Vec::new();
    for id in store.named_graphs()? {
        if let Some(Term::NamedNode(node)) = store.decode_term(id)? {
            out.push(node);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------
// LOAD
// ---------------------------------------------------------------------------------

fn load(
    engine: &mut Engine,
    session: &mut Session,
    source: &NamedNode,
    destination: &GraphName,
    outcome: &mut UpdateOutcome,
) -> Result<(), EngineError> {
    let iri = source.as_str();

    // No HTTP client here, on purpose. Fetching an arbitrary URL named inside a query is a
    // server-side request forgery primitive: it would let anyone with update rights make
    // the server issue requests to hosts only the server can reach. If remote LOAD is
    // wanted it needs an explicit allow-list, which is a policy decision and belongs with
    // the rest of §14 rather than buried here.
    let path = if let Some(rest) = iri.strip_prefix("file:///") {
        percent_decode(rest)
    } else if let Some(rest) = iri.strip_prefix("file://") {
        percent_decode(rest)
    } else if iri.starts_with("http://") || iri.starts_with("https://") {
        return Err(EngineError::Io(std::io::Error::other(format!(
            "LOAD <{iri}>: remote fetch is not enabled; only file: URLs are loadable"
        ))));
    } else {
        percent_decode(iri)
    };

    // Through the shared opener, so `LOAD <file:///dump.nq.gz>` works like every other
    // load rather than being the one path that cannot read a compressed file.
    let (format, reader) = crate::source::open(std::path::Path::new(&path))
        .map_err(|e| EngineError::Io(std::io::Error::other(format!("LOAD <{iri}>: {e}"))))?;

    let parser = RdfParser::from_format(format)
        .with_base_iri(iri)
        .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;

    let mut loaded = Vec::new();
    for quad in parser.for_reader(reader) {
        let quad = quad?;
        // A LOAD targets one graph: whatever graph the source names is overridden.
        loaded.push(Quad {
            subject: quad.subject,
            predicate: quad.predicate,
            object: quad.object,
            graph_name: destination.clone(),
        });
    }
    for quad in loaded {
        insert(engine, session, &quad, outcome)?;
    }
    Ok(())
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

// ---------------------------------------------------------------------------------
// ground terms -> ordinary terms
// ---------------------------------------------------------------------------------

/// `spargebra` carries its own `GraphName` and `Quad`, differing from `oxrdf`'s only in
/// that a graph name can never be a blank node. Converting at the boundary keeps the rest
/// of this module speaking one vocabulary.
fn algebra_graph_name(graph: &spargebra::term::GraphName) -> GraphName {
    match graph {
        spargebra::term::GraphName::NamedNode(n) => GraphName::NamedNode(n.clone()),
        spargebra::term::GraphName::DefaultGraph => GraphName::DefaultGraph,
    }
}

fn algebra_quad(quad: &spargebra::term::Quad) -> Quad {
    Quad {
        subject: quad.subject.clone(),
        predicate: quad.predicate.clone(),
        object: quad.object.clone(),
        graph_name: algebra_graph_name(&quad.graph_name),
    }
}

/// A *ground* quad has no variables and no blank nodes — which is what makes
/// `DELETE DATA` able to name exactly one quad rather than a pattern.
fn ground_quad(quad: &GroundQuad) -> Quad {
    Quad {
        subject: quad.subject.clone().into(),
        predicate: quad.predicate.clone(),
        object: ground_term(&quad.object),
        graph_name: algebra_graph_name(&quad.graph_name),
    }
}

fn ground_term(term: &GroundTerm) -> Term {
    match term {
        GroundTerm::NamedNode(n) => Term::NamedNode(n.clone()),
        GroundTerm::Literal(l) => Term::Literal(l.clone()),
        GroundTerm::Triple(t) => Term::Triple(Box::new(ground_triple(t))),
    }
}

fn ground_triple(triple: &GroundTriple) -> oxrdf::Triple {
    oxrdf::Triple {
        subject: triple.subject.clone().into(),
        predicate: triple.predicate.clone(),
        object: ground_term(&triple.object),
    }
}

/// Reading through the view rather than the store is what makes `CLEAR` and `DROP` obey
/// read policy: a principal cannot delete, through a wildcard, quads a `SELECT` would not
/// have shown it.
fn quads_in_graph(
    view: &DatasetView<'_>,
    graph: Option<&GraphName>,
) -> Result<Vec<Quad>, EngineError> {
    Ok(view.visible_quads(graph)?)
}
