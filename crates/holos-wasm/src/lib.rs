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

use holos_engine::Engine;
use holos_security::{Policy, Principal, Session};
use oxrdfio::RdfFormat;
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

/// An in-memory RDF 1.2 store with a SPARQL 1.2 engine over it.
#[wasm_bindgen]
pub struct Store {
    engine: Engine,
}

#[wasm_bindgen]
impl Store {
    /// A new, empty store.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Engine::new(),
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
    /// and an array of objects keyed by variable name for SELECT — the same mapping the
    /// Python bindings use. N-Triples specifically, because it is the one serialisation
    /// every RDF library in JS can parse without first agreeing on a term model.
    ///
    /// A returned triple carries no trailing separator, which also matches the Python
    /// bindings; a caller reassembling a document adds one.
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
                            js_sys::Reflect::set(
                                &row,
                                &JsValue::from_str(name),
                                &JsValue::from_str(&term.to_string()),
                            )?;
                        }
                    }
                    out.push(&row);
                }
                Ok(out.into())
            }
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}
