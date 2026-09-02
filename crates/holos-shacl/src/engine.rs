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
use crate::incremental::Change;
use crate::{Options, ShaclError};
use holos_shacl_engine::model::Graph as EngineGraph;
use holos_shacl_engine::report::ValidationReport;
use holos_shacl_engine::shapes::{ShapeId, Shapes as EngineShapes};
use holos_shacl_engine::{validate as engine_validate, TermId as EngineId};
use holos_store::Store;
use oxrdf::Graph as OxGraph;

/// Whether a quad could change what the shapes compile to.
///
/// The blunt question — did anything in the shapes *graph* change — is useless in the
/// commonest configuration, where shapes and data share one graph and every data write is
/// therefore a write to the shapes graph. The useful question is narrower: shapes are built
/// out of SHACL vocabulary, so a triple that uses none of it cannot have defined one.
///
/// Conservative in the direction that costs time rather than correctness. Three cases count:
///
/// * a predicate in the SHACL namespace — every constraint, target and path parameter;
/// * `rdf:type` naming a SHACL class or `rdfs:Class`, which is how a node *becomes* a shape,
///   including SHACL 1.2's `sh:ShapeClass` and the implicit class target;
/// * `rdf:first` and `rdf:rest`, because `sh:path ( ex:a ex:b )` and `sh:in ( 1 2 3 )` are
///   built from list cells whose predicates are RDF rather than SHACL.
///
/// Anything else is data. A change to a value a `sh:hasValue` names does not change the
/// shape — it changes whether the data satisfies it, which is what validation is for.
fn affects_shapes(store: &Store, quad: holos_store::EncodedQuad) -> Result<bool, ShaclError> {
    const SH: &str = "http://www.w3.org/ns/shacl#";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    const RDFS_CLASS: &str = "http://www.w3.org/2000/01/rdf-schema#Class";

    let iri = |id: holos_core::TermId| -> Result<Option<String>, ShaclError> {
        Ok(match store.decode_term(id)? {
            Some(oxrdf::Term::NamedNode(n)) => Some(n.into_string()),
            _ => None,
        })
    };

    let Some(predicate) = iri(quad.predicate)? else {
        return Ok(false);
    };
    if predicate.starts_with(SH) {
        return Ok(true);
    }
    if predicate == format!("{RDF}first") || predicate == format!("{RDF}rest") {
        return Ok(true);
    }
    if predicate == format!("{RDF}type") {
        if let Some(object) = iri(quad.object)? {
            return Ok(object.starts_with(SH) || object == RDFS_CLASS);
        }
    }
    Ok(false)
}

/// Whether a quad's graph is the one a filter selects.
///
/// The same four cases the store's own scan uses. Written out rather than borrowed because
/// a delta is matched one quad at a time, and the store's version is a scan predicate.
fn in_graph(graph_name: Option<holos_core::TermId>, filter: holos_store::GraphFilter) -> bool {
    match filter {
        holos_store::GraphFilter::Default => graph_name.is_none(),
        holos_store::GraphFilter::Named(g) => graph_name == Some(g),
        holos_store::GraphFilter::AnyNamed => graph_name.is_some(),
        holos_store::GraphFilter::Any => true,
    }
}

/// A bridged store with its shapes compiled, ready to validate.
pub struct EngineRun {
    bridged: Bridged,
    shapes_graph: EngineGraph,
    shapes: EngineShapes,
    /// Kept so [`Self::apply`] can tell which graph a changed quad belongs to. A delta is
    /// reported over the whole store; only what lands in the data graph is this run's.
    options: Options,
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
            options,
        })
    }

    /// Brings the bridged graph up to date with a store delta, instead of bridging again.
    ///
    /// This is what `DESIGN.md` §8 called for. The engine covers more constraint components
    /// than the native evaluator, but its graph could not change, so gating a commit with it
    /// meant re-bridging the whole store every time: 198 ms at 250,000 quads against 0.4 µs
    /// for a validator that can be told what changed, and growing with the store rather than
    /// with the change.
    ///
    /// Changes outside the data graph are ignored — a delta is reported over the whole store
    /// and most of it is not this run's business.
    ///
    /// # Errors
    ///
    /// [`ShaclError::Unsupported`] when a change lands in the *shapes* graph. Those cannot be
    /// applied incrementally at all: the shapes are compiled into a flat IR at `prepare`
    /// time, and a changed shape invalidates that wholesale. Saying so is the point — the
    /// alternative is validating new data against shapes that no longer exist.
    pub fn apply(&mut self, store: &Store, changes: &[Change]) -> Result<(), ShaclError> {
        for change in changes {
            if in_graph(change.quad.graph_name, self.options.shapes_graph)
                && affects_shapes(store, change.quad)?
            {
                return Err(ShaclError::Unsupported(
                    "a shape definition changed; the compiled shapes must be rebuilt with \
                     EngineRun::prepare rather than updated in place"
                        .to_owned(),
                ));
            }
        }

        let mut added = Vec::new();
        let mut removed = Vec::new();
        for change in changes {
            if !in_graph(change.quad.graph_name, self.options.data_graph) {
                continue;
            }
            let row = [
                self.bridged.intern_id(store, change.quad.subject)?,
                self.bridged.intern_id(store, change.quad.predicate)?,
                self.bridged.intern_id(store, change.quad.object)?,
            ];
            if change.added {
                added.push(row);
            } else {
                removed.push(row);
            }
        }
        self.bridged.graph.apply(&added, &removed);
        Ok(())
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

    /// Applies a delta and validates only what it made stale.
    ///
    /// The other half of `DESIGN.md` §8. [`Self::apply`] keeps the graph current in constant
    /// time; this decides what to re-check, so the cost of gating a commit stops scaling with
    /// the store in both halves rather than one.
    ///
    /// The algorithm is the native evaluator's, over the engine's own dependency index —
    /// mirrored rather than reinvented, because the failure mode of getting it wrong is a
    /// violation admitted rather than a slow answer:
    ///
    /// 1. a change to `p` implicates every shape that reads `p`, at both ends of the quad;
    /// 2. a changed `rdf:type` implicates every shape targeting that class, at the subject;
    /// 3. the work is attributed *upwards* to targeted ancestors, because a change almost
    ///    always lands on an anonymous property shape which is never a focus of its own;
    /// 4. pairs the targets do not actually select are dropped, and any shape left with
    ///    nothing to do is widened back to all of its focus nodes — a shape reached down a
    ///    property path has its focus node upstream of the quad that changed.
    ///
    /// Steps 3 and 4 are what make it safe. Over-reporting costs time; under-reporting lets
    /// an invalid graph through.
    ///
    /// # Falling back
    ///
    /// A shapes graph carrying a constraint whose reads cannot be bounded — a SPARQL
    /// constraint matching `?s ?p ?o`, a `sh:target` selector, a node expression — has no
    /// index entry that a change could match, so this runs a full validation instead of
    /// silently checking less. [`Self::would_revalidate_incrementally`] says which it is
    /// without running either.
    ///
    /// # Errors
    ///
    /// As [`Self::apply`]: a change to a shape definition is refused rather than absorbed.
    pub fn revalidate(
        &mut self,
        store: &Store,
        changes: &[Change],
    ) -> Result<ValidationReport, ShaclError> {
        self.apply(store, changes)?;
        if !self.shapes.unconditional().is_empty() {
            return self.validate();
        }

        let rdf_type = self.bridged.vocab.rdf_type;
        let mut work: std::collections::HashSet<(ShapeId, EngineId)> =
            std::collections::HashSet::new();
        let mut implicated: std::collections::HashSet<ShapeId> = std::collections::HashSet::new();
        // Shapes whose violation can sit at a node the delta never mentions.
        let mut widen: std::collections::HashSet<ShapeId> = std::collections::HashSet::new();

        for change in changes {
            if !in_graph(change.quad.graph_name, self.options.data_graph) {
                continue;
            }
            let (Some(subject), Some(predicate), Some(object)) = (
                self.bridged.engine_id(change.quad.subject),
                self.bridged.engine_id(change.quad.predicate),
                self.bridged.engine_id(change.quad.object),
            ) else {
                // `apply` has just interned every term of every change it kept, so a miss
                // here means the change was not this run's — nothing refers to it.
                continue;
            };

            let shapes = &self.shapes;
            let implicate =
                |id: ShapeId,
                 node: EngineId,
                 work: &mut std::collections::HashSet<(ShapeId, EngineId)>,
                 implicated: &mut std::collections::HashSet<ShapeId>,
                 widen: &mut std::collections::HashSet<ShapeId>| {
                    let upstream = shapes.focus_may_be_upstream(id);
                    for ancestor in shapes.targeted_ancestors(id) {
                        implicated.insert(ancestor);
                        work.insert((ancestor, node));
                        if upstream {
                            widen.insert(ancestor);
                        }
                    }
                };

            for &id in self.shapes.shapes_touching(predicate) {
                implicate(id, subject, &mut work, &mut implicated, &mut widen);
                implicate(id, object, &mut work, &mut implicated, &mut widen);
            }
            if predicate == rdf_type {
                for &id in self.shapes.shapes_targeting_class(object) {
                    implicate(id, subject, &mut work, &mut implicated, &mut widen);
                }
                // A node's other types carry their own shapes, and the change may have made
                // one of those shapes apply where it did not before.
                let classes: Vec<EngineId> =
                    self.bridged.graph.objects(subject, rdf_type).collect();
                for class in classes {
                    for &id in self.shapes.shapes_targeting_class(class) {
                        implicate(id, subject, &mut work, &mut implicated, &mut widen);
                    }
                }
            }
        }

        let mut focus: std::collections::HashMap<ShapeId, Vec<EngineId>> =
            std::collections::HashMap::new();
        for &id in &implicated {
            let mut nodes = engine_validate::focus_nodes_of(
                id,
                &self.bridged.graph,
                &self.shapes,
                &self.shapes_graph,
                &mut self.bridged.terms,
                &self.bridged.vocab,
            )
            .map_err(|e| ShaclError::Unsupported(e.to_string()))?;
            nodes.sort_unstable();
            nodes.dedup();
            focus.insert(id, nodes);
        }
        work.retain(|(id, node)| {
            focus
                .get(id)
                .is_some_and(|nodes| nodes.binary_search(node).is_ok())
        });
        // Widen a shape that got nothing — and one whose focus node can be upstream of the
        // change, which the endpoint attribution reaches only by accident.
        for &id in &implicated {
            if !widen.contains(&id) && work.iter().any(|(i, _)| *i == id) {
                continue;
            }
            for node in focus.get(&id).into_iter().flatten() {
                work.insert((id, *node));
            }
        }

        let mut work: Vec<(ShapeId, EngineId)> = work.into_iter().collect();
        work.sort_unstable();
        engine_validate::validate_nodes(
            &work,
            &self.bridged.graph,
            &self.shapes,
            &self.shapes_graph,
            &mut self.bridged.terms,
            &self.bridged.vocab,
        )
        .map_err(|e| ShaclError::Unsupported(e.to_string()))
    }

    /// Runs the shapes graph's SHACL-AF rules to a fixpoint and returns what they inferred.
    ///
    /// The triples come back rather than being written anywhere: a rule infers a *statement*,
    /// and where that statement belongs — which graph, under whose policy, undone by what if
    /// the commit is refused — is the caller's question, not this one's. `holos_holon` puts
    /// them in the scene and records them so a rejected tick takes them out again.
    ///
    /// Only what the rules *added*. The engine's rule evaluation returns the whole closure,
    /// premises included, and handing that back would make a caller diff it to find out what
    /// happened.
    ///
    /// # Why this can be per-commit now
    ///
    /// It could not before. The rules need the data as an engine `Graph`, that graph was
    /// immutable, and building one per commit costs the whole scene — which is why
    /// `DESIGN.md` §9 left the holon tick's rule step switched off and named §8 as the
    /// blocker. With [`Self::apply`] the graph tracks a delta in constant time, so a caller
    /// that keeps one run alive across commits pays the bridge once.
    ///
    /// # Errors
    ///
    /// [`ShaclError::Unsupported`] when the rules do not reach a fixpoint inside
    /// `max_rounds`, which is what a rule set that infers new terms without end looks like
    /// from outside. Nothing is returned in that case rather than a partial closure: the
    /// caller asked what the rules entail, and half an answer to that is not an answer.
    pub fn infer(&mut self, max_rounds: usize) -> Result<Vec<oxrdf::Triple>, ShaclError> {
        let before = self.bridged.graph.clone();
        let after = holos_shacl_engine::rules::apply_iterated(
            &before,
            &self.shapes,
            &self.shapes_graph,
            &mut self.bridged.terms,
            &self.bridged.vocab,
            max_rounds,
        )
        .map_err(|e| ShaclError::Unsupported(e.to_string()))?;

        let terms = &self.bridged.terms;
        let mut out = Vec::new();
        for [s, p, o] in after.iter() {
            if before.contains(s, p, o) {
                continue;
            }
            let (subject, predicate, object) =
                (terms.to_oxrdf(s), terms.to_oxrdf(p), terms.to_oxrdf(o));
            let (oxrdf::Term::NamedNode(predicate), Ok(subject)) =
                (predicate, oxrdf::NamedOrBlankNode::try_from(subject))
            else {
                // A rule can only infer a well-formed triple, so this is unreachable in
                // practice. Skipping rather than panicking keeps a malformed rule set a bad
                // inference instead of a crash in a commit path.
                continue;
            };
            out.push(oxrdf::Triple {
                subject,
                predicate,
                object,
            });
        }
        Ok(out)
    }

    /// Whether [`Self::revalidate`] can work from the delta, or must validate everything.
    ///
    /// False when some shape reads more than the index can name. Worth asking before a
    /// commit rather than after, because the answer decides whether the gate is cheap.
    #[must_use]
    pub fn would_revalidate_incrementally(&self) -> bool {
        self.shapes.unconditional().is_empty()
    }

    /// Renders a report as RDF.
    #[must_use]
    pub fn report_to_oxrdf(&self, report: &ValidationReport) -> OxGraph {
        report.to_oxrdf(
            &self.bridged.terms,
            &self.bridged.vocab,
            &self.shapes_graph,
            &self.shapes,
            &self.all_severities(),
        )
    }

    /// Renders a report judged by an explicit set of disqualifying severities.
    ///
    /// The rendered report says which set it was, as `sh:conformanceDisallows`, so the
    /// verdict travels with the rule that produced it.
    #[must_use]
    pub fn report_to_oxrdf_with(
        &self,
        report: &ValidationReport,
        disallowed: &[EngineId],
    ) -> OxGraph {
        report.to_oxrdf_declaring(
            &self.bridged.terms,
            &self.bridged.vocab,
            &self.shapes_graph,
            &self.shapes,
            disallowed,
            true,
        )
    }

    /// Resolves an IRI to this run's term id, if the bridged graph knows it.
    ///
    /// A severity the graph has never seen cannot be the severity of any result, so it
    /// disqualifies nothing and `None` is the right answer rather than an error.
    #[must_use]
    pub fn term_for_iri(&self, iri: &str) -> Option<EngineId> {
        self.bridged.terms.get_named_node(iri)
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
