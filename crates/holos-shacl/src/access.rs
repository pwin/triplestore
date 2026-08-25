//! Reading a graph out of the store, in term ids.
//!
//! `DESIGN.md` §8 turns on one change: the validator reads the store's own dictionary and
//! indexes instead of loading a private copy. This is that access layer. Everything it
//! returns is a [`TermId`], so constraint evaluation compares integers and never strings.
//!
//! It also fixes the limitation §8 names in SHACL_Engine: a data graph is selected with a
//! [`GraphFilter`], so "this named graph", "the union of named graphs" and "the whole
//! dataset" are all expressible. A holon's Boundary validates its own scene, which is not
//! possible if every graph is flattened into one.

use holos_core::{Tag, TermId};
use holos_store::{GraphFilter, Result, Store};
use oxrdf::{NamedNodeRef, Term};

/// A graph inside a store, addressed by term id.
#[derive(Debug, Clone, Copy)]
pub struct GraphView<'a> {
    store: &'a Store,
    graph: GraphFilter,
}

impl<'a> GraphView<'a> {
    /// Views one graph of a store.
    #[must_use]
    pub fn new(store: &'a Store, graph: GraphFilter) -> Self {
        Self { store, graph }
    }

    /// The store behind this view.
    #[must_use]
    pub fn store(&self) -> &'a Store {
        self.store
    }

    /// Which graph this view reads.
    #[must_use]
    pub fn graph(&self) -> GraphFilter {
        self.graph
    }

    /// Resolves an IRI to its id, if the store has seen it.
    pub fn id(&self, iri: NamedNodeRef<'_>) -> Result<Option<TermId>> {
        self.store.lookup_term(iri.into())
    }

    /// Turns an id back into a term.
    pub fn term(&self, id: TermId) -> Result<Option<Term>> {
        self.store.decode_term(id)
    }

    /// Objects of `subject predicate ?o`.
    pub fn objects(&self, subject: TermId, predicate: TermId) -> Result<Vec<TermId>> {
        self.store
            .quads_for_pattern(Some(subject), Some(predicate), None, self.graph)
            .map(|q| Ok(q?.object))
            .collect()
    }

    /// The single object of `subject predicate ?o`, or `None`.
    ///
    /// SHACL treats a repeated constraint parameter as ill-formed; taking the first in
    /// index order keeps validation deterministic rather than arbitrary.
    pub fn object(&self, subject: TermId, predicate: TermId) -> Result<Option<TermId>> {
        self.store
            .quads_for_pattern(Some(subject), Some(predicate), None, self.graph)
            .next()
            .transpose()
            .map(|q| q.map(|q| q.object))
    }

    /// Subjects of `?s predicate object`.
    pub fn subjects(&self, predicate: TermId, object: TermId) -> Result<Vec<TermId>> {
        self.store
            .quads_for_pattern(None, Some(predicate), Some(object), self.graph)
            .map(|q| Ok(q?.subject))
            .collect()
    }

    /// Every subject of `?s predicate ?o`.
    pub fn subjects_of(&self, predicate: TermId) -> Result<Vec<TermId>> {
        self.store
            .quads_for_pattern(None, Some(predicate), None, self.graph)
            .map(|q| Ok(q?.subject))
            .collect()
    }

    /// Every object of `?s predicate ?o`.
    pub fn objects_of(&self, predicate: TermId) -> Result<Vec<TermId>> {
        self.store
            .quads_for_pattern(None, Some(predicate), None, self.graph)
            .map(|q| Ok(q?.object))
            .collect()
    }

    /// Whether `subject predicate object` is in the graph.
    pub fn has(&self, subject: TermId, predicate: TermId, object: TermId) -> Result<bool> {
        Ok(self
            .store
            .quads_for_pattern(Some(subject), Some(predicate), Some(object), self.graph)
            .next()
            .transpose()?
            .is_some())
    }

    /// Every predicate used by a subject — what `sh:closed` needs.
    pub fn predicates_of(&self, subject: TermId) -> Result<Vec<TermId>> {
        self.store
            .quads_for_pattern(Some(subject), None, None, self.graph)
            .map(|q| Ok(q?.predicate))
            .collect()
    }

    /// Walks an RDF collection into a vector.
    ///
    /// Bounded rather than trusting `rdf:rest`: a cyclic list in a hostile shapes graph
    /// must not hang the validator.
    pub fn list(&self, head: TermId, rdf_first: TermId, rdf_rest: TermId, rdf_nil: TermId)
        -> Result<Vec<TermId>>
    {
        let mut out = Vec::new();
        let mut node = head;
        for _ in 0..100_000 {
            if node == rdf_nil {
                return Ok(out);
            }
            let Some(first) = self.object(node, rdf_first)? else {
                return Ok(out);
            };
            out.push(first);
            let Some(rest) = self.object(node, rdf_rest)? else {
                return Ok(out);
            };
            node = rest;
        }
        Ok(out)
    }
}

/// What kind of RDF node an id denotes, without decoding it.
///
/// The tag already says, which is why `sh:nodeKind` costs a bit-shift rather than a
/// dictionary lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// An IRI.
    Iri,
    /// A blank node.
    BlankNode,
    /// A literal.
    Literal,
    /// An RDF 1.2 triple term.
    TripleTerm,
}

/// Classifies an id.
#[must_use]
pub fn node_kind(id: TermId) -> NodeKind {
    match id.tag() {
        Tag::Iri | Tag::Vocab => NodeKind::Iri,
        Tag::BlankNode => NodeKind::BlankNode,
        Tag::TripleTerm => NodeKind::TripleTerm,
        // Every remaining tag is an inline or dictionary-backed literal.
        _ => NodeKind::Literal,
    }
}

/// Whether an id denotes a literal.
#[must_use]
pub fn is_literal(id: TermId) -> bool {
    node_kind(id) == NodeKind::Literal
}
