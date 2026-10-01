//! WebAssembly bindings for the HOLOS store and SPARQL engine.
//!
//! The same surface as the Python bindings (`crates/holos-python`), reduced to what a
//! single-threaded host without a filesystem can honour: build a store, load RDF from a
//! string, ask a SPARQL query. A caller who knows the Python bindings will recognise the
//! shape, and — deliberately — the *answers* are identical, because both are thin wrappers
//! over the same `holos_engine::Engine`.
//!
//! # What a wasm build cannot do, and why
//!
//! None of these are oversights. Each is a property of the host rather than of the engine:
//!
//! * **No persistence.** RocksDB does not build for wasm32, so the store is in memory. That
//!   is the default backend anyway — `holos-store`'s `rocksdb` feature is opt-in — so this
//!   is the ordinary build rather than a stripped one.
//! * **No file paths.** `std::fs` compiles for wasm32 and then fails at runtime, so RDF
//!   arrives as a string the host has already read. Nothing is streamed, so a load costs
//!   memory proportional to the document; a browser tab is not where a 60 GB dump goes.
//! * **No spilling `ORDER BY`.** The external merge sort needs somewhere to put its run
//!   files. A sort here is bounded by memory instead, and a large one exhausts the module
//!   rather than reaching for disk.
//! * **No query timeout and no memory ceiling.** Both are enforced by a watchdog thread
//!   sampling a clock, and `wasm32-unknown-unknown` has neither.
//!   `holos_engine::QueryOptions::guard` returns `None` there, and this module exposes no
//!   way to ask for a limit — so nobody can set one and believe it is being honoured.
//! * **One thread.** A load parses and interns in one serial loop instead of overlapping
//!   them. Same answer, same order, less throughput.
//!
//! # Why the panic hook
//!
//! A Rust panic reaches JS as `RuntimeError: unreachable`, with no message and no stack,
//! which is indistinguishable from a bug in the host. `console_error_panic_hook` makes it
//! say what failed. Installed by [`start`], which wasm-bindgen calls on module
//! instantiation, so a host cannot forget to.
#![allow(clippy::needless_pass_by_value)]

mod service;

use holos_engine::Engine;
use holos_security::{Policy, Principal, Session};
use oxrdf::{GraphName, Quad, Term};
use oxrdfio::{RdfFormat, RdfSerializer};
use spareval::QueryResults;
use wasm_bindgen::prelude::*;

/// Installs the panic hook. Called by wasm-bindgen when the module is instantiated.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// The RDF syntax a name stands for.
///
/// The same names the Python bindings accept, so a format string is portable between them.
fn format_by_name(name: &str) -> Result<RdfFormat, JsValue> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "turtle" | "ttl" => RdfFormat::Turtle,
        "ntriples" | "nt" | "n-triples" => RdfFormat::NTriples,
        "trig" => RdfFormat::TriG,
        "nquads" | "nq" | "n-quads" => RdfFormat::NQuads,
        "rdfxml" | "rdf" | "xml" => RdfFormat::RdfXml,
        "n3" => RdfFormat::N3,
        other => {
            return Err(JsError::new(&format!(
                "unknown RDF format {other:?}; expected turtle, ntriples, trig, nquads, \
                 rdfxml or n3"
            ))
            .into())
        }
    })
}

fn failed(context: &str, error: &impl std::fmt::Display) -> JsValue {
    JsError::new(&format!("{context}: {error}")).into()
}

/// One RDF term as a JavaScript object in rdf-js shape.
///
/// `{termType, value}`, plus `language` and `datatype` on a literal — the same fields
/// rdf-js specifies and oxigraph returns, so a caller can hand one to n3 or compare it
/// field by field rather than parsing term syntax out of a string.
///
/// A blank node's `value` is the bare label, without `_:`. That is what rdf-js says and
/// what every JS library expects; it is also worth knowing that the label is this engine's
/// own and not the one in whatever document was loaded, because a parser may rename blank
/// nodes and this one does.
///
/// An RDF 1.2 triple term reports `termType: "Quad"` with its N-Triples rendering in
/// `value`, rather than being decomposed into subject/predicate/object. Nothing that
/// consumes this binding asks for one yet, and a half-built nested shape would mislead
/// where a plain string does not; decomposing it is a change to make when something needs
/// it.
fn term_to_js(term: &Term) -> JsValue {
    let object = js_sys::Object::new();
    let set = |key: &str, value: &JsValue| {
        // Setting a property on a fresh object cannot fail, and returning Result from here
        // would push a `?` into every call site for a case that does not arise.
        let _ = js_sys::Reflect::set(&object, &JsValue::from_str(key), value);
    };
    match term {
        Term::NamedNode(node) => {
            set("termType", &JsValue::from_str("NamedNode"));
            set("value", &JsValue::from_str(node.as_str()));
        }
        Term::BlankNode(node) => {
            set("termType", &JsValue::from_str("BlankNode"));
            set("value", &JsValue::from_str(node.as_str()));
        }
        Term::Literal(literal) => {
            set("termType", &JsValue::from_str("Literal"));
            set("value", &JsValue::from_str(literal.value()));
            // rdf-js carries both, always: language is "" for a typed literal, and the
            // datatype of a language-tagged string is rdf:langString.
            set(
                "language",
                &JsValue::from_str(literal.language().unwrap_or("")),
            );
            let datatype = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &datatype,
                &JsValue::from_str("termType"),
                &JsValue::from_str("NamedNode"),
            );
            let _ = js_sys::Reflect::set(
                &datatype,
                &JsValue::from_str("value"),
                &JsValue::from_str(literal.datatype().as_str()),
            );
            set("datatype", &datatype);
        }
        other => {
            set("termType", &JsValue::from_str("Quad"));
            set("value", &JsValue::from_str(&other.to_string()));
        }
    }
    object.into()
}

/// An in-memory RDF 1.2 store with a SPARQL 1.2 engine over it.
#[wasm_bindgen]
pub struct Store {
    engine: Engine,
    /// Answers to `SERVICE` clauses, supplied by the host. See `service.rs` for why a cache
    /// rather than a network client, and `queryFederated` for how the host fills it.
    services: crate::service::CachingServiceHandler,
}

#[wasm_bindgen]
impl Store {
    /// A new, empty store.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Engine::new(),
            services: crate::service::CachingServiceHandler::new(),
        }
    }

    /// Quads the store holds.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn size(&self) -> usize {
        self.engine.store().len()
    }

    /// Distinct terms in the dictionary.
    ///
    /// Reliably smaller than expected, because every integer, float, dateTime and short
    /// string is inlined into its 64-bit id and never reaches the dictionary at all.
    #[wasm_bindgen(getter, js_name = dictionarySize)]
    #[must_use]
    pub fn dictionary_size(&self) -> usize {
        self.engine.store().dictionary_len()
    }

    /// Load RDF from a string, returning how many quads were added.
    ///
    /// `format` takes the same names as the Python bindings. `base` resolves relative IRIs
    /// in the document.
    pub fn load(
        &mut self,
        text: &str,
        format: &str,
        base: Option<String>,
    ) -> Result<usize, JsValue> {
        let rdf_format = format_by_name(format)?;
        self.engine
            .bulk_load(text.as_bytes(), rdf_format, base.as_deref())
            .map_err(|e| failed("loading RDF", &e))
    }

    /// Load Turtle, which is the common case.
    #[wasm_bindgen(js_name = loadTurtle)]
    pub fn load_turtle(&mut self, text: &str, base: Option<String>) -> Result<usize, JsValue> {
        self.load(text, "turtle", base)
    }

    /// Run a SPARQL 1.2 query.
    ///
    /// Returns a boolean for ASK, an array of N-Triples strings for CONSTRUCT and DESCRIBE,
    /// and for SELECT an array of objects keyed by variable name whose values are terms in
    /// RDF/JS shape — `{termType, value}`, plus `language` and `datatype` on a literal.
    ///
    /// **Terms rather than strings, since 0.17.0.** They were rendered as N-Triples strings,
    /// which made every consumer parse term syntax to get at a value, and no RDF library in
    /// JS accepts a bare term string as input. `{termType, value}` is what rdf-js specifies
    /// and what oxigraph returns, so a caller can hand these straight to n3 or compare them
    /// field by field. `queryRdf` is there for when a *document* is wanted instead.
    ///
    /// CONSTRUCT stays N-Triples strings, deliberately: it is one serialisation every RDF
    /// library in JS can parse without first agreeing on a term model, and a graph is
    /// usually wanted as a graph. A returned triple carries no trailing separator, matching
    /// the Python bindings; a caller reassembling a document adds one.
    pub fn query(&self, query: &str, base: Option<String>) -> Result<JsValue, JsValue> {
        // Anonymous and permit-all: policy enforcement is a server concern, and a module
        // running inside the caller's own process has nobody to keep secrets from. The
        // engine still goes through the session and view, so this is the same evaluation
        // path the server takes rather than a shortcut around it.
        let session = Session::open(
            self.engine.store(),
            Principal::anonymous(),
            Policy::permit_all(),
        )
        .map_err(|e| failed("opening a session", &e))?;
        let view = self.engine.view(&session);
        let results =
            Engine::query(&view, query, base.as_deref()).map_err(|e| failed("querying", &e))?;

        Self::results_to_js(results)
    }

    /// Apply a SPARQL 1.1 Update, returning what changed:
    /// `{inserted, deleted, graphsCreated, graphsDropped}`.
    ///
    /// **All-or-nothing.** If any operation fails the store is left exactly as it was, so a
    /// refused write cannot leave half an update behind.
    pub fn update(&mut self, update: &str, base: Option<String>) -> Result<JsValue, JsValue> {
        let mut session = Session::open(
            self.engine.store(),
            Principal::anonymous(),
            Policy::permit_all(),
        )
        .map_err(|e| failed("opening a session", &e))?;
        let outcome =
            holos_engine::update::update(&mut self.engine, &mut session, update, base.as_deref())
                .map_err(|e| failed("updating", &e))?;

        let out = js_sys::Object::new();
        for (key, value) in [
            ("inserted", outcome.inserted),
            ("deleted", outcome.deleted),
            ("graphsCreated", outcome.graphs_created),
            ("graphsDropped", outcome.graphs_dropped),
        ] {
            let _ = js_sys::Reflect::set(
                &out,
                &JsValue::from_str(key),
                // f64, because a JS number is one and usize would silently become a BigInt
                // for large values -- which is a different type to a caller doing
                // arithmetic on it.
                &JsValue::from_f64(value as f64),
            );
        }
        Ok(out.into())
    }

    /// Run a CONSTRUCT or DESCRIBE and return the graph serialised in `format`.
    ///
    /// For when a caller wants a *document* rather than terms: a preview pane showing
    /// Turtle, or a payload to hand to something that parses RDF but does not speak this
    /// binding's shapes. `query` remains the way to get the triples themselves.
    ///
    /// A SELECT or ASK is refused rather than serialised, because neither produces a graph
    /// and "here is an empty document" would be the wrong way to say so.
    #[wasm_bindgen(js_name = queryRdf)]
    pub fn query_rdf(
        &self,
        query: &str,
        format: &str,
        base: Option<String>,
    ) -> Result<String, JsValue> {
        let rdf_format = format_by_name(format)?;
        let session = Session::open(
            self.engine.store(),
            Principal::anonymous(),
            Policy::permit_all(),
        )
        .map_err(|e| failed("opening a session", &e))?;
        let view = self.engine.view(&session);
        let results =
            Engine::query(&view, query, base.as_deref()).map_err(|e| failed("querying", &e))?;

        let QueryResults::Graph(triples) = results else {
            return Err(JsError::new(
                "queryRdf needs a CONSTRUCT or DESCRIBE; a SELECT or ASK produces no graph to                  serialise -- use query() for those",
            )
            .into());
        };

        let mut writer = RdfSerializer::from_format(rdf_format).for_writer(Vec::new());
        for triple in triples {
            let triple = triple.map_err(|e| failed("reading a result triple", &e))?;
            writer
                .serialize_triple(triple.as_ref())
                .map_err(|e| failed("serialising a result triple", &e))?;
        }
        let bytes = writer
            .finish()
            .map_err(|e| failed("finishing the document", &e))?;
        String::from_utf8(bytes).map_err(|e| failed("the serialiser produced invalid UTF-8", &e))
    }

    /// Query results as the JavaScript values documented on [`Self::query`].
    ///
    /// Shared by `query` and `queryFederated` rather than written twice: the two have to
    /// produce the same shapes or a host cannot move between them, and two copies of this
    /// match is how they would stop.
    fn results_to_js(results: QueryResults<'_>) -> Result<JsValue, JsValue> {
        match results {
            QueryResults::Boolean(value) => Ok(JsValue::from_bool(value)),
            QueryResults::Graph(triples) => {
                let out = js_sys::Array::new();
                for triple in triples {
                    let triple = triple.map_err(|e| failed("reading a result triple", &e))?;
                    out.push(&JsValue::from_str(&triple.to_string()));
                }
                Ok(out.into())
            }
            QueryResults::Solutions(solutions) => {
                let names: Vec<String> = solutions
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect();
                let out = js_sys::Array::new();
                for solution in solutions {
                    let solution = solution.map_err(|e| failed("reading a solution", &e))?;
                    let row = js_sys::Object::new();
                    for name in &names {
                        // An unbound variable is left off the object rather than set to
                        // null. "This row has no value here" and "this row has the value
                        // null" are different facts, and RDF has no null to mean the first.
                        if let Some(term) = solution.get(name.as_str()) {
                            js_sys::Reflect::set(&row, &JsValue::from_str(name), &term_to_js(term))?;
                        }
                    }
                    out.push(&row);
                }
                Ok(out.into())
            }
        }
    }

    /// Run a query that may contain `SERVICE`, one pass at a time.
    ///
    /// Returns `{pending: [{endpoint, query}, …]}` when the engine asked for an endpoint this
    /// store has no answer for, and `{result: …}` -- shaped exactly as `query` returns -- when
    /// it did not. The host fetches each pending query, hands it back with `cacheService`, and
    /// calls this again:
    ///
    /// ```js
    /// for (let round = 0; round < 8; round += 1) {
    ///   const pass = store.queryFederated(q, undefined);
    ///   if (!pass.pending.length) return pass.result;
    ///   for (const { endpoint, query } of pass.pending) {
    ///     // The host decides what may be called. Nothing in the wasm can reach the network.
    ///     store.cacheService(endpoint, query, await fetchSparqlJson(endpoint, query));
    ///   }
    /// }
    /// throw new Error('SERVICE did not settle');
    /// ```
    ///
    /// **A pass with anything pending has wrong results, and says so by having them.** The
    /// unanswered `SERVICE` contributed no rows, so the join above it lost rows too. Discard
    /// `result` whenever `pending` is non-empty rather than showing it.
    ///
    /// More than two rounds are possible and not a fault: a `SERVICE` whose pattern carries
    /// bindings from an earlier join only takes its final shape once that join has rows, so an
    /// answer can reveal a request that could not have been seen before. The cap belongs to the
    /// host because only the host knows how long it is prepared to wait.
    ///
    /// A query with no `SERVICE` never consults the handler, so `pending` is empty and this is
    /// `query` with one more allocation.
    #[wasm_bindgen(js_name = queryFederated)]
    pub fn query_federated(&self, query: &str, base: Option<String>) -> Result<JsValue, JsValue> {
        self.services.begin_pass();

        let session = Session::open(
            self.engine.store(),
            Principal::anonymous(),
            Policy::permit_all(),
        )
        .map_err(|e| failed("opening a session", &e))?;
        let view = self.engine.view(&session);
        // Parsed here rather than inside the engine, because the handler-taking entry point
        // takes a parsed query -- it is the one place a caller supplies its own handler, so it
        // cannot also be the one that hides the parse. Same construction holos-engine uses.
        let mut parser = spargebra::SparqlParser::new();
        if let Some(base) = base.as_deref() {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| failed("the base IRI is not an IRI", &e))?;
        }
        let parsed = parser
            .parse_query(query)
            .map_err(|e| failed("parsing the query", &e))?;
        let results =
            Engine::query_prepared_with_handler(&view, &parsed, self.services.clone())
                .map_err(|e| failed("querying", &e))?;
        let value = Self::results_to_js(results)?;

        let pending = js_sys::Array::new();
        for request in self.services.pending() {
            let entry = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &entry,
                &JsValue::from_str("endpoint"),
                &JsValue::from_str(&request.endpoint),
            );
            let _ = js_sys::Reflect::set(
                &entry,
                &JsValue::from_str("query"),
                &JsValue::from_str(&request.query),
            );
            pending.push(&entry);
        }

        let out = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&out, &JsValue::from_str("pending"), &pending);
        let _ = js_sys::Reflect::set(&out, &JsValue::from_str("result"), &value);
        Ok(out.into())
    }

    /// Give the store an endpoint's answer to one of `queryFederated`'s pending queries.
    ///
    /// `results` is SPARQL Results JSON, which is what an endpoint returns for
    /// `Accept: application/sparql-results+json`. `query` must be the string the pending entry
    /// carried, byte for byte: it is the cache key, so a reformatted query is a different
    /// question and will be asked again.
    #[wasm_bindgen(js_name = cacheService)]
    pub fn cache_service(&self, endpoint: &str, query: &str, results: &str) {
        self.services.cache(endpoint, query, results);
    }

    /// Everything the store holds, serialised in `format`.
    ///
    /// Use N-Quads or TriG for a store with named graphs. Turtle and N-Triples have nowhere
    /// to put a graph name, and the serialiser **refuses** such a quad rather than writing
    /// it into the default graph — measured, and the right behaviour: flattening would move
    /// data between graphs while reporting success. The error says which formats can carry
    /// it. A store with only a default graph serialises to any of the four.
    ///
    /// This is how a caller enumerates the store: there is no quad iterator on this binding,
    /// because crossing the boundary once with a document beats crossing it once per quad,
    /// and every JS RDF library can parse what comes back.
    pub fn dump(&self, format: &str) -> Result<String, JsValue> {
        let rdf_format = format_by_name(format)?;
        let session = Session::open(
            self.engine.store(),
            Principal::anonymous(),
            Policy::permit_all(),
        )
        .map_err(|e| failed("opening a session", &e))?;
        let view = self.engine.view(&session);
        // The default graph and the named graphs in one pass, which is what the CLI's own
        // `dump` does -- a plain `?s ?p ?o` would silently omit every named graph.
        let results = Engine::query(
            &view,
            "SELECT * WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }",
            None,
        )
        .map_err(|e| failed("reading the store", &e))?;
        let QueryResults::Solutions(solutions) = results else {
            return Err(JsError::new("the dump query did not return solutions").into());
        };

        let mut writer = RdfSerializer::from_format(rdf_format).for_writer(Vec::new());
        for solution in solutions {
            let solution = solution.map_err(|e| failed("reading a quad", &e))?;
            let (Some(s), Some(p), Some(o)) =
                (solution.get("s"), solution.get("p"), solution.get("o"))
            else {
                continue;
            };
            // A literal or triple term in subject position, or anything but an IRI as a
            // predicate, cannot be written as a quad. Nothing in the store can hold one, so
            // these arms are unreachable rather than a silent drop -- but skipping is the
            // safe reading either way.
            let subject = match s {
                Term::NamedNode(n) => oxrdf::NamedOrBlankNode::NamedNode(n.clone()),
                Term::BlankNode(b) => oxrdf::NamedOrBlankNode::BlankNode(b.clone()),
                _ => continue,
            };
            let Term::NamedNode(predicate) = p else {
                continue;
            };
            let graph_name = match solution.get("g") {
                Some(Term::NamedNode(n)) => GraphName::NamedNode(n.clone()),
                Some(Term::BlankNode(b)) => GraphName::BlankNode(b.clone()),
                _ => GraphName::DefaultGraph,
            };
            let named_graph = !matches!(graph_name, GraphName::DefaultGraph);
            writer
                .serialize_quad(
                    Quad {
                        subject,
                        predicate: predicate.clone(),
                        object: o.clone(),
                        graph_name,
                    }
                    .as_ref(),
                )
                .map_err(|e| {
                    // oxrdfio's own message -- "Only quads in the default graph can be
                    // serialized to a RDF graph format" -- says what is wrong but not what
                    // to do about it, and this is the one failure a caller will actually
                    // hit. Naming the formats that work turns it into an instruction.
                    if named_graph {
                        JsError::new(&format!(
                            "dumping to {format}: this store has named graphs and {format}                              cannot carry one. Use nquads or trig. ({e})"
                        ))
                        .into()
                    } else {
                        failed("serialising a quad", &e)
                    }
                })?;
        }
        let bytes = writer
            .finish()
            .map_err(|e| failed("finishing the document", &e))?;
        String::from_utf8(bytes).map_err(|e| failed("the serialiser produced invalid UTF-8", &e))
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}
