//! Feeding the adapted engine from a HOLOS store.
//!
//! `DESIGN.md` §8 plans one change to SHACL_Engine: the validator should read the store's
//! own dictionary and indexes instead of loading a private copy. This is that change, and
//! it is worth being precise about how much of it is achieved.
//!
//! # What this does, and what it does not
//!
//! **Parsing is gone.** The engine's file loader parses Turtle, interns, sorts. Fed from a
//! populated store, none of that happens: the quads are already interned and already in
//! sorted index order, and the bridge walks them. Parsing is the dominant term in the load
//! cost, so this is the part that mattered.
//!
//! **A second copy of the term table is not gone.** The engine's `TermId` is a dense `u32`
//! *index* into its own interner — `index()` addresses arrays throughout — while HOLOS's is
//! a sparse tagged `u64` carrying inline values. They are structurally different handles,
//! not two spellings of the same one, so the engine cannot simply be handed HOLOS ids
//! without rewriting its term store and everything that indexes by it.
//!
//! So each **distinct** term is decoded once and re-interned once, and the mapping is
//! cached; repeat occurrences cost a hash lookup. On data with any term reuse — which is
//! all real RDF — that is far closer to free than to a reload. See §16 for the measurement
//! rather than a promise.

use holos_core::TermId as HolosId;
use holos_shacl_engine::model::{Graph, GraphBuilder, TermStore, Vocab};
use holos_shacl_engine::TermId as EngineId;
use holos_store::{GraphFilter, Store};
use rustc_hash::FxHashMap;

use crate::ShaclError;

/// A graph and term table the engine can validate, built from a HOLOS store.
pub struct Bridged {
    /// The data, in the engine's three sorted permutations.
    pub graph: Graph,
    /// The engine's interner, holding every term the graph mentions.
    pub terms: TermStore,
    /// The SHACL vocabulary, interned into `terms`.
    pub vocab: Vocab,
    /// HOLOS id → engine id, for translating a delta after the graph is built.
    forward: FxHashMap<HolosId, EngineId>,
}

impl std::fmt::Debug for Bridged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridged")
            .field("triples", &self.graph.len())
            .field("terms", &self.terms.len())
            .finish()
    }
}

impl Bridged {
    /// The engine id for a HOLOS id, if the bridge has seen it.
    ///
    /// `None` means the term is not in the bridged graph, so nothing in the engine's world
    /// refers to it — which for an incremental plan means there is nothing to revalidate.
    #[must_use]
    pub fn engine_id(&self, id: HolosId) -> Option<EngineId> {
        self.forward.get(&id).copied()
    }

    /// Translates a HOLOS id, interning it if the bridge has not seen it.
    ///
    /// The read-only [`Self::engine_id`] answers `None` for an unknown term, which is right
    /// for planning — nothing in the engine's world refers to it. Applying a delta is the
    /// other case: a newly written triple names terms the bridge could not have seen, and
    /// refusing them would silently drop the change.
    pub fn intern_id(&mut self, store: &Store, id: HolosId) -> Result<EngineId, ShaclError> {
        intern(store, &mut self.terms, &mut self.forward, id)
    }

    /// How many triples were bridged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.graph.len()
    }

    /// Whether the bridged graph is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.graph.is_empty()
    }
}

/// Builds one engine graph from one HOLOS graph.
pub fn bridge(store: &Store, graph: GraphFilter) -> Result<Bridged, ShaclError> {
    let mut terms = TermStore::new();
    let vocab = Vocab::new(&mut terms);
    let mut forward: FxHashMap<HolosId, EngineId> = FxHashMap::default();
    let mut builder = GraphBuilder::new();

    for quad in store.quads_for_pattern(None, None, None, graph) {
        let quad = quad?;
        let s = intern(store, &mut terms, &mut forward, quad.subject)?;
        let p = intern(store, &mut terms, &mut forward, quad.predicate)?;
        let o = intern(store, &mut terms, &mut forward, quad.object)?;
        builder.push(s, p, o);
    }

    Ok(Bridged {
        graph: builder.build(),
        terms,
        vocab,
        forward,
    })
}

/// Builds two engine graphs — data and shapes — sharing one term table.
///
/// They have to share it: an engine `TermId` only means anything relative to the store
/// that issued it, so a shape naming `ex:Person` and data typed `ex:Person` must resolve to
/// the same integer or nothing matches.
pub fn bridge_pair(
    store: &Store,
    data_graph: GraphFilter,
    shapes_graph: GraphFilter,
) -> Result<(Bridged, Graph), ShaclError> {
    let mut bridged = bridge(store, data_graph)?;
    let mut builder = GraphBuilder::new();
    for quad in store.quads_for_pattern(None, None, None, shapes_graph) {
        let quad = quad?;
        let s = intern(
            store,
            &mut bridged.terms,
            &mut bridged.forward,
            quad.subject,
        )?;
        let p = intern(
            store,
            &mut bridged.terms,
            &mut bridged.forward,
            quad.predicate,
        )?;
        let o = intern(store, &mut bridged.terms, &mut bridged.forward, quad.object)?;
        builder.push(s, p, o);
    }
    let shapes = builder.build();
    Ok((bridged, shapes))
}

/// Translates one HOLOS id, decoding it only the first time it is seen.
fn intern(
    store: &Store,
    terms: &mut TermStore,
    forward: &mut FxHashMap<HolosId, EngineId>,
    id: HolosId,
) -> Result<EngineId, ShaclError> {
    if let Some(existing) = forward.get(&id) {
        return Ok(*existing);
    }
    let term = store
        .decode_term(id)?
        .ok_or_else(|| ShaclError::IllFormedShape(format!("{id:?} is not in the dictionary")))?;
    // Scope 0: HOLOS blank-node labels are already unique across the store, so the
    // engine's per-document scoping has nothing to disambiguate.
    let engine_id = terms.intern_oxrdf(term.as_ref(), 0);
    forward.insert(id, engine_id);
    Ok(engine_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{GraphName, Literal, NamedNode, Quad};

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{s}"))
    }

    #[test]
    fn a_store_bridges_into_an_engine_graph() {
        let mut store = Store::new();
        for (s, p, o) in [("a", "knows", "b"), ("b", "knows", "c")] {
            store
                .insert(
                    Quad {
                        subject: nn(s).into(),
                        predicate: nn(p),
                        object: nn(o).into(),
                        graph_name: GraphName::DefaultGraph,
                    }
                    .as_ref(),
                )
                .unwrap();
        }
        let bridged = bridge(&store, GraphFilter::Default).unwrap();
        assert_eq!(bridged.len(), 2);

        // A term that occurs twice must intern once, and translate consistently.
        let b = store.lookup_term(nn("b").as_ref().into()).unwrap().unwrap();
        let engine_b = bridged.engine_id(b).expect("b was bridged");
        assert_eq!(bridged.engine_id(b), Some(engine_b));
        assert_eq!(bridged.terms.iri(engine_b), Some(nn("b").as_str()));
    }

    #[test]
    fn literals_and_inline_values_survive_the_bridge() {
        // Inline ids never reach the HOLOS dictionary, so the bridge has to reconstruct
        // them from the tag rather than look them up.
        let mut store = Store::new();
        store
            .insert(
                Quad {
                    subject: nn("a").into(),
                    predicate: nn("age"),
                    object: Literal::new_typed_literal("42", oxrdf::vocab::xsd::INTEGER).into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .unwrap();
        let bridged = bridge(&store, GraphFilter::Default).unwrap();
        assert_eq!(bridged.len(), 1);
        let lit = store
            .lookup_term(
                Literal::new_typed_literal("42", oxrdf::vocab::xsd::INTEGER)
                    .as_ref()
                    .into(),
            )
            .unwrap()
            .unwrap();
        let engine_lit = bridged.engine_id(lit).expect("literal was bridged");
        assert_eq!(bridged.terms.lexical_form(engine_lit), Some("42"));
    }

    #[test]
    fn data_and_shapes_share_one_term_table() {
        // Without a shared table a shape naming ex:Person and data typed ex:Person would
        // resolve to different integers, and nothing would ever match.
        let mut store = Store::new();
        store
            .insert(
                Quad {
                    subject: nn("alice").into(),
                    predicate: oxrdf::vocab::rdf::TYPE.into_owned(),
                    object: nn("Person").into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .unwrap();
        let shapes_name = NamedNode::new_unchecked("urn:holos:shapes");
        store
            .insert(
                Quad {
                    subject: nn("PersonShape").into(),
                    predicate: NamedNode::new_unchecked("http://www.w3.org/ns/shacl#targetClass"),
                    object: nn("Person").into(),
                    graph_name: shapes_name.clone().into(),
                }
                .as_ref(),
            )
            .unwrap();
        let shapes_filter = GraphFilter::Named(
            store
                .lookup_term(shapes_name.as_ref().into())
                .unwrap()
                .unwrap(),
        );
        let (bridged, shapes) = bridge_pair(&store, GraphFilter::Default, shapes_filter).unwrap();
        assert_eq!(bridged.len(), 1);
        assert_eq!(shapes.len(), 1);

        let person = store
            .lookup_term(nn("Person").as_ref().into())
            .unwrap()
            .unwrap();
        let engine_person = bridged.engine_id(person).expect("bridged");
        // The same integer appears on both sides.
        assert!(shapes
            .objects_of(bridged.vocab.sh_targetClass)
            .any(|t| t == engine_person));
        assert!(bridged
            .graph
            .objects_of(
                bridged
                    .engine_id(
                        store
                            .lookup_term(oxrdf::vocab::rdf::TYPE.into())
                            .unwrap()
                            .unwrap()
                    )
                    .unwrap()
            )
            .any(|t| t == engine_person));
    }
}
