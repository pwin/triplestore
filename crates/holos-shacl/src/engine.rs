//! Validation through the adapted SHACL_Engine.
//!
//! `DESIGN.md` §8 says to take SHACL_Engine's design and change one thing. In practice the
//! best of both turned out to be two validators, not one, and it is worth saying why
//! plainly rather than pretending a single path won.
//!
//! | | [`EngineRun`] (this module) | [`CompiledShapes`](crate::CompiledShapes) |
//! |---|---|---|
//! | Constraint coverage | SHACL Core, SPARQL constraints, node expressions, SHACL-AF rules, inference | SHACL Core |
//! | Reads | a bridged snapshot of the store | the live store |
//! | Cost to start | one pass over the graph, to bridge it | none |
//! | Incremental revalidation | no | yes |
//!
//! The split is forced by one fact about the adapted engine: its `Graph` is **immutable**
//! — three sorted arrays, built once — and its rules engine rebuilds the whole thing when
//! it adds a triple. So a delta cannot be pushed into a bridged graph cheaply, and a
//! validator that has to re-bridge the graph on every commit is not an incremental
//! validator however good its constraint coverage is.
//!
//! Hence: **the engine for coverage, the native evaluator for the write path.** Both sit
//! behind [`Validate`](crate::Validate), so the holon Boundary (§9) does not care which it
//! has. The gap this leaves is real and named in §8: the write path validates against
//! fewer constraint components than a full run does. Closing it means giving the engine's
//! `Graph` a merge-in-place, which is a bounded change and is not made here on spec.

use crate::bridge::{self, Bridged};
use crate::{Options, ShaclError};
use holos_shacl_engine::model::Graph as EngineGraph;
use holos_shacl_engine::report::ValidationReport;
use holos_shacl_engine::shapes::Shapes as EngineShapes;
use holos_shacl_engine::{validate as engine_validate, TermId as EngineId};
use holos_store::Store;
use oxrdf::Graph as OxGraph;

/// A bridged store with its shapes compiled, ready to validate.
pub struct EngineRun {
    bridged: Bridged,
    shapes_graph: EngineGraph,
    shapes: EngineShapes,
}

impl std::fmt::Debug for EngineRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineRun")
            .field("triples", &self.bridged.len())
            .field("shapes", &self.shapes.len())
            .finish()
    }
}

impl EngineRun {
    /// Bridges a store and compiles its shapes.
    ///
    /// This is where the cost of using the engine is paid — one pass over the data graph.
    /// No parsing happens: the quads come out of the store already interned and already in
    /// index order.
    pub fn prepare(store: &Store, options: Options) -> Result<Self, ShaclError> {
        let (bridged, shapes_graph) =
            bridge::bridge_pair(store, options.data_graph, options.shapes_graph)?;
        let shapes = EngineShapes::compile(&shapes_graph, &bridged.terms, &bridged.vocab)
            .map_err(|e| ShaclError::IllFormedShape(e.to_string()))?;
        Ok(Self {
            bridged,
            shapes_graph,
            shapes,
        })
    }

    /// How many triples were bridged.
    #[must_use]
    pub fn triples(&self) -> usize {
        self.bridged.len()
    }

    /// How many shapes were compiled.
    #[must_use]
    pub fn shapes(&self) -> usize {
        self.shapes.len()
    }

    /// Validates everything the shapes target.
    pub fn validate(&mut self) -> Result<ValidationReport, ShaclError> {
        engine_validate::validate_in(
            &self.bridged.graph,
            &self.shapes,
            &self.shapes_graph,
            &mut self.bridged.terms,
            &self.bridged.vocab,
        )
        .map_err(|e| ShaclError::Unsupported(e.to_string()))
    }

    /// Validates a chosen set of (shape node, focus node) pairs, named by HOLOS ids.
    ///
    /// Uses the `validate_nodes` entry point added to the adapted engine. Pairs naming
    /// terms the bridge never saw are dropped: nothing in the engine's world refers to
    /// them, so there is nothing there to revalidate.
    pub fn validate_nodes(
        &mut self,
        work: &[(holos_core::TermId, holos_core::TermId)],
    ) -> Result<ValidationReport, ShaclError> {
        let mut translated: Vec<(holos_shacl_engine::shapes::ShapeId, EngineId)> = Vec::new();
        for (shape_node, focus) in work {
            let (Some(shape_id), Some(focus_id)) = (
                self.bridged
                    .engine_id(*shape_node)
                    .and_then(|n| self.shapes.id_of(n)),
                self.bridged.engine_id(*focus),
            ) else {
                continue;
            };
            translated.push((shape_id, focus_id));
        }
        engine_validate::validate_nodes(
            &translated,
            &self.bridged.graph,
            &self.shapes,
            &self.shapes_graph,
            &mut self.bridged.terms,
            &self.bridged.vocab,
        )
        .map_err(|e| ShaclError::Unsupported(e.to_string()))
    }

    /// Every severity, which is what SHACL 1.0 conformance counts.
    ///
    /// A report conforms iff it holds no results *at all* — a `sh:Warning` breaks
    /// conformance just as a `sh:Violation` does. Passing only `sh:Violation` here is the
    /// SHACL 1.2 `sh:conformanceDisallows` behaviour, and getting it wrong makes a report
    /// claim conformance while carrying results, which the suite catches immediately.
    fn all_severities(&self) -> [EngineId; 3] {
        [
            self.bridged.vocab.sh_Violation,
            self.bridged.vocab.sh_Warning,
            self.bridged.vocab.sh_Info,
        ]
    }

    /// Renders a report as RDF.
    #[must_use]
    pub fn report_to_oxrdf(&self, report: &ValidationReport) -> OxGraph {
        report.to_oxrdf(
            &self.bridged.terms,
            &self.bridged.vocab,
            &self.shapes_graph,
            &self.all_severities(),
        )
    }

    /// Whether a report conforms.
    #[must_use]
    pub fn conforms(&self, report: &ValidationReport) -> bool {
        report.conforms(&self.all_severities())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_store::GraphFilter;
    use oxrdfio::{RdfFormat, RdfParser};

    const DOC: &str = r#"
@prefix ex:   <http://example.com/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:Person a rdfs:Class .
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:maxInclusive 150 ] .

ex:alice a ex:Person ; ex:age 30 .
ex:bob   a ex:Person ; ex:age 900 .
"#;

    fn store_from(turtle: &str) -> Store {
        let mut store = Store::new();
        let parser = RdfParser::from_format(RdfFormat::Turtle)
            .with_base_iri("http://example.com/")
            .expect("base");
        for quad in parser.for_reader(turtle.as_bytes()) {
            store.insert(quad.expect("parse").as_ref()).expect("insert");
        }
        store
    }

    #[test]
    fn the_engine_validates_a_bridged_store() {
        let store = store_from(DOC);
        let mut run = EngineRun::prepare(
            &store,
            Options {
                data_graph: GraphFilter::Default,
                shapes_graph: GraphFilter::Default,
            },
        )
        .expect("prepare");
        assert!(run.triples() > 0);
        assert!(run.shapes() > 0);

        let report = run.validate().expect("validate");
        assert!(!run.conforms(&report), "bob's age of 900 must be caught");
        assert_eq!(report.results.len(), 1);

        // And the report renders as RDF the same way upstream's does.
        let graph = run.report_to_oxrdf(&report);
        assert!(graph.len() > 3);
    }

    #[test]
    fn a_clean_store_conforms() {
        let store = store_from(
            r#"
@prefix ex:   <http://example.com/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
ex:Person a rdfs:Class .
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:maxInclusive 150 ] .
ex:alice a ex:Person ; ex:age 30 .
"#,
        );
        let mut run = EngineRun::prepare(
            &store,
            Options {
                data_graph: GraphFilter::Default,
                shapes_graph: GraphFilter::Default,
            },
        )
        .expect("prepare");
        let report = run.validate().expect("validate");
        assert!(run.conforms(&report));
    }
}
