//! HOLOS L2 — the store.
//!
//! A [`Store`] is a facade over a [`Storage`] implementation: a term dictionary, nine
//! index orders and per-predicate counts, kept in step.
//!
//! This is Tier A of the design's two-tier storage (`DESIGN.md` §6). Two backends exist —
//! [`MemoryStorage`] for embedding and tests, [`RocksStorage`](rocks::RocksStorage) for
//! anything durable — and they present the same API because both offer the same access
//! shape: ordered prefix scans over the same nine orders. Tier B, the hypertrie hot tier,
//! is not built and §13 Q2 says it should be gated on measuring what a cost-based
//! optimiser alone buys.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// `# Errors` sections would restate the error enum on every function; the enums are
// documented at their definition instead.
#![allow(clippy::missing_errors_doc)]
// s/p/o/g are the names the RDF and SPARQL specifications use. Renaming them to satisfy
// a length lint would make this code harder to check against the specs, not easier.
#![allow(clippy::many_single_char_names)]

pub mod dictionary;
pub mod error;
pub mod index;
pub mod memory;
#[cfg(feature = "rocksdb")]
pub mod rocks;
pub mod storage;

pub use dictionary::Dictionary;
pub use error::{Result, StorageError};
pub use index::{EncodedQuad, GraphFilter, IdRange, QuadIndex, QuadScan};
pub use memory::MemoryStorage;
#[cfg(feature = "rocksdb")]
pub use rocks::RocksStorage;
pub use storage::Storage;

use holos_core::TermId;
use oxrdf::{GraphName, GraphNameRef, Quad, QuadRef, Term, TermRef};

/// An RDF dataset.
#[derive(Debug)]
pub struct Store {
    inner: Box<dyn Storage>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// An empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::with_storage(MemoryStorage::new())
    }

    /// A store over a specific backend.
    pub fn with_storage(storage: impl Storage + 'static) -> Self {
        Self {
            inner: Box::new(storage),
        }
    }

    /// The backend, for inspection.
    #[must_use]
    pub fn storage(&self) -> &dyn Storage {
        self.inner.as_ref()
    }

    /// Number of quads.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when the store holds no quads.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// How many terms the dictionary holds.
    #[must_use]
    pub fn dictionary_len(&self) -> usize {
        self.inner.dictionary_len()
    }

    /// How many ids have been issued for one dictionary-backed tag.
    ///
    /// Each kind has its own dense index space, so this is an enumeration bound as well as a
    /// count: `TermId::new(tag, i)` for `i` in `0..dictionary_count_for(tag)` is every id
    /// issued for that tag, in issue order. Something that has examined the first `n` can
    /// find everything added since by walking from `n` — which is how the spatial index
    /// catches up with a write without rescanning the store.
    #[must_use]
    pub fn dictionary_count_for(&self, tag: holos_core::Tag) -> usize {
        self.inner.dictionary_count_for(tag)
    }

    /// How many quads use a given predicate.
    #[must_use]
    pub fn predicate_count(&self, predicate: TermId) -> u64 {
        self.inner.predicate_count(predicate)
    }

    /// Every predicate in the store with its count, most frequent first.
    #[must_use]
    pub fn predicate_histogram(&self) -> Vec<(TermId, u64)> {
        self.inner.predicate_histogram()
    }

    /// Interns a quad's terms without indexing it.
    ///
    /// Split out from [`Store::insert`] because the write path has to know a quad's ids
    /// *before* deciding whether it is permitted: an access policy keys on the graph and
    /// the predicate, and deciding on a quad whose terms are not yet interned would let a
    /// rule naming an unseen IRI silently fail to apply. Interning a term that never ends
    /// up in a quad is harmless — dictionary entries are not required to be used.
    pub fn encode_quad(&mut self, quad: QuadRef<'_>) -> Result<EncodedQuad> {
        Ok(EncodedQuad {
            subject: self.inner.encode(quad.subject.into())?,
            predicate: self.inner.encode(quad.predicate.into())?,
            object: self.inner.encode(quad.object)?,
            graph_name: match quad.graph_name {
                GraphNameRef::DefaultGraph => None,
                GraphNameRef::NamedNode(n) => Some(self.inner.encode(n.into())?),
                GraphNameRef::BlankNode(b) => Some(self.inner.encode(b.into())?),
            },
        })
    }

    /// Indexes an already-encoded quad. `Ok(true)` if it was not already present.
    pub fn insert_encoded(&mut self, encoded: EncodedQuad) -> Result<bool> {
        self.inner.insert_encoded(encoded)
    }

    /// Removes an already-encoded quad. `Ok(true)` if it was present.
    ///
    /// The counterpart to [`Store::insert_encoded`], for a caller that already holds the
    /// ids — the holon tick undoes a rejected commit this way, without re-resolving terms
    /// it resolved a moment ago.
    pub fn remove_encoded_quad(&mut self, encoded: EncodedQuad) -> Result<bool> {
        self.inner.remove_encoded(encoded)
    }

    /// Interns a quad and inserts it. `Ok(true)` if it was not already present.
    pub fn insert(&mut self, quad: QuadRef<'_>) -> Result<bool> {
        let encoded = self.encode_quad(quad)?;
        self.inner.insert_encoded(encoded)
    }

    /// Removes a quad. `Ok(true)` if it was present.
    ///
    /// The dictionary is not garbage-collected: `DESIGN.md` §6.1 puts refcounts on the
    /// dictionary column families via RocksDB merge operators, which is where reclamation
    /// belongs. Until then a removed term keeps its id, which is harmless — ids stay
    /// stable and nothing dangles.
    pub fn remove(&mut self, quad: QuadRef<'_>) -> Result<bool> {
        let Some(encoded) = self.lookup_quad(quad)? else {
            return Ok(false);
        };
        self.inner.remove_encoded(encoded)
    }

    /// Whether the store contains a quad.
    pub fn contains(&self, quad: QuadRef<'_>) -> Result<bool> {
        let Some(encoded) = self.lookup_quad(quad)? else {
            return Ok(false);
        };
        self.inner.contains_encoded(encoded)
    }

    /// Records a named graph, which may hold no quads.
    pub fn insert_named_graph(&mut self, graph: &GraphName) -> Result<bool> {
        let id = match graph {
            GraphName::DefaultGraph => return Ok(false),
            GraphName::NamedNode(n) => self.inner.encode(n.as_ref().into())?,
            GraphName::BlankNode(b) => self.inner.encode(b.as_ref().into())?,
        };
        self.inner.insert_named_graph(id)
    }

    /// Drops a named graph and its quads.
    pub fn remove_named_graph(&mut self, graph: GraphNameRef<'_>) -> Result<bool> {
        let Some(Some(id)) = self.lookup_graph_name(graph)? else {
            return Ok(false);
        };
        self.inner.remove_named_graph(id)
    }

    /// Whether a named graph exists.
    pub fn contains_named_graph(&self, graph: GraphNameRef<'_>) -> Result<bool> {
        let Some(Some(id)) = self.lookup_graph_name(graph)? else {
            return Ok(false);
        };
        self.inner.contains_named_graph(id)
    }

    /// Every named graph, whether or not it holds quads.
    pub fn named_graphs(&self) -> Result<Vec<TermId>> {
        self.inner.named_graphs()
    }

    /// Every quad matching an encoded pattern.
    pub fn quads_for_pattern(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        self.inner.scan(subject, predicate, object, graph)
    }

    /// Every quad in the store, decoded. Order is the `spog`/`dspo` index order, so it is
    /// stable across runs.
    pub fn iter(&self) -> impl Iterator<Item = Result<Quad>> + '_ {
        self.inner
            .scan(None, None, None, GraphFilter::Any)
            .map(|q| self.decode_quad(q?))
    }

    /// Makes everything written so far durable. A no-op for the in-memory tier.
    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }

    /// Writes a consistent snapshot of the store to `destination`.
    ///
    /// Works on an open store being written to, which is the whole point: copying the
    /// directory instead requires stopping the service, because an LSM tree in mid-flight is
    /// not a set of files that can be copied one at a time.
    ///
    /// `destination` must not already exist. Backends that link rather than copy require it
    /// to be on the same filesystem as the store to stay cheap.
    ///
    /// # Errors
    ///
    /// [`StorageError::Unsupported`] for an in-memory store, which has no files to snapshot,
    /// and while a bulk load is running. Otherwise the backend's own failure.
    pub fn checkpoint(&self, destination: &std::path::Path) -> Result<()> {
        self.inner.checkpoint(destination)
    }

    /// Opens a commit scope: everything written until [`Store::commit`] lands together.
    ///
    /// A single quad write is already atomic — every index order for it goes into one
    /// batch — so what this adds is atomicity across *several*. A SPARQL update or a holon
    /// tick is a sequence of quad writes, and without a scope a crash between two of them
    /// leaves half a commit behind.
    ///
    /// Reads inside the scope see the scope's own writes, which is what lets an update's
    /// later operations see what its earlier ones did. What the scope does **not** give is
    /// isolation from *other* holders of the store; see [`Storage::begin`].
    ///
    /// # Errors
    ///
    /// A scope already open, or a bulk load running.
    pub fn begin(&mut self) -> Result<()> {
        self.inner.begin()
    }

    /// Commits a scope opened by [`Store::begin`].
    ///
    /// # Errors
    ///
    /// A write failure, or no scope open.
    pub fn commit(&mut self) -> Result<()> {
        self.inner.commit()
    }

    /// Abandons a scope, leaving the store as it was when [`Store::begin`] was called.
    ///
    /// Infallible on purpose: it is the failure path, and a rollback that can itself fail
    /// leaves a state nobody can describe.
    pub fn rollback(&mut self) {
        self.inner.rollback();
    }

    /// Whether a commit scope is open.
    #[must_use]
    pub fn in_scope(&self) -> bool {
        self.inner.in_scope()
    }

    /// Quads matching the pattern whose object lies inside `span`.
    ///
    /// See [`Storage::quads_with_object_in`]. The span narrows the scan; the caller's own
    /// filter still decides what matches.
    #[must_use]
    pub fn quads_with_object_in(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        span: IdRange,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        self.inner
            .quads_with_object_in(subject, predicate, span, graph)
    }

    /// How many times the most recent bulk load spilled its buffer to disk.
    ///
    /// Always zero for a backend that does not spill. See
    /// [`RocksStorage::spills`](crate::RocksStorage::spills).
    #[must_use]
    pub fn bulk_spills(&self) -> usize {
        self.inner.bulk_spills()
    }

    /// How many bytes this store occupies on disk, or `None` for an in-memory one.
    ///
    /// What a maintenance operation has to fit: see [`Storage::on_disk_bytes`].
    #[must_use]
    pub fn on_disk_bytes(&self) -> Option<u64> {
        self.inner.on_disk_bytes()
    }

    /// Announces a bulk load, so the backend can buffer writes and skip its log.
    ///
    /// # Errors
    ///
    /// A commit scope open: see [`Storage::begin_bulk_load`].
    pub fn begin_bulk_load(&mut self) -> Result<()> {
        self.inner.begin_bulk_load()
    }

    /// Ends a bulk load, writing anything buffered and making it durable.
    pub fn end_bulk_load(&mut self) -> Result<()> {
        self.inner.end_bulk_load()
    }

    /// Decodes an encoded quad.
    ///
    /// Any id reachable from the index must be in the dictionary, so a missing one is
    /// corruption rather than an absent result.
    pub fn decode_quad(&self, quad: EncodedQuad) -> Result<Quad> {
        let subject = match self.require_term(quad.subject)? {
            Term::NamedNode(n) => n.into(),
            Term::BlankNode(b) => b.into(),
            other => {
                return Err(StorageError::corruption(format!(
                    "quad subject decoded to {other}, which cannot be a subject"
                )))
            }
        };
        let Term::NamedNode(predicate) = self.require_term(quad.predicate)? else {
            return Err(StorageError::corruption(
                "quad predicate decoded to a non-IRI",
            ));
        };
        let object = self.require_term(quad.object)?;
        let graph_name = match quad.graph_name {
            None => GraphName::DefaultGraph,
            Some(g) => match self.require_term(g)? {
                Term::NamedNode(n) => n.into(),
                Term::BlankNode(b) => b.into(),
                other => {
                    return Err(StorageError::corruption(format!(
                        "graph name decoded to {other}, which cannot name a graph"
                    )))
                }
            },
        };
        Ok(Quad {
            subject,
            predicate,
            object,
            graph_name,
        })
    }

    /// Looks up a term without interning it. `Ok(None)` means the store has not seen it.
    pub fn lookup_term(&self, term: TermRef<'_>) -> Result<Option<TermId>> {
        self.inner.lookup(term)
    }

    /// Decodes a term id. `Ok(None)` means the dictionary never issued it.
    pub fn decode_term(&self, id: TermId) -> Result<Option<Term>> {
        self.inner.decode(id)
    }

    /// Interns nothing: looks a quad's terms up, failing softly if any is unknown.
    fn lookup_quad(&self, quad: QuadRef<'_>) -> Result<Option<EncodedQuad>> {
        let (Some(subject), Some(predicate), Some(object), Some(graph_name)) = (
            self.inner.lookup(quad.subject.into())?,
            self.inner.lookup(quad.predicate.into())?,
            self.inner.lookup(quad.object)?,
            self.lookup_graph_name(quad.graph_name)?,
        ) else {
            return Ok(None);
        };
        Ok(Some(EncodedQuad {
            subject,
            predicate,
            object,
            graph_name,
        }))
    }

    /// `Ok(None)` means the graph term is unknown; `Ok(Some(None))` means the default graph.
    fn lookup_graph_name(&self, graph: GraphNameRef<'_>) -> Result<Option<Option<TermId>>> {
        Ok(match graph {
            GraphNameRef::DefaultGraph => Some(None),
            GraphNameRef::NamedNode(n) => self.inner.lookup(n.into())?.map(Some),
            GraphNameRef::BlankNode(b) => self.inner.lookup(b.into())?.map(Some),
        })
    }

    fn require_term(&self, id: TermId) -> Result<Term> {
        self.inner.decode(id)?.ok_or_else(|| {
            StorageError::corruption(format!("term {id:?} is indexed but not in the dictionary"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::vocab::rdf;
    use oxrdf::{Literal, NamedNode};

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{s}"))
    }

    fn quad(s: &str, p: &str, o: &str, g: Option<&str>) -> Quad {
        Quad {
            subject: nn(s).into(),
            predicate: nn(p),
            object: nn(o).into(),
            graph_name: g.map_or(GraphName::DefaultGraph, |g| nn(g).into()),
        }
    }

    #[test]
    fn quads_round_trip_through_encoding() {
        let mut store = Store::new();
        let quads = [
            quad("alice", "knows", "bob", None),
            quad("bob", "knows", "carol", None),
            quad("alice", "knows", "bob", Some("g1")),
        ];
        for q in &quads {
            assert!(store.insert(q.as_ref()).unwrap(), "insert {q}");
        }
        assert_eq!(store.len(), 3);

        let mut got: Vec<_> = store.iter().collect::<Result<Vec<_>>>().unwrap();
        let mut want = quads.to_vec();
        got.sort_by_key(ToString::to_string);
        want.sort_by_key(ToString::to_string);
        assert_eq!(got, want, "every quad must decode to what was inserted");
    }

    #[test]
    fn literals_and_triple_terms_survive_the_store() {
        let mut store = Store::new();
        // A literal that inlines, one that does not, and an RDF 1.2 reified triple.
        let inner = oxrdf::Triple {
            subject: nn("alice").into(),
            predicate: nn("age"),
            object: Literal::new_typed_literal("42", oxrdf::vocab::xsd::INTEGER).into(),
        };
        for q in [
            Quad {
                subject: nn("alice").into(),
                predicate: nn("age"),
                object: Literal::new_typed_literal("42", oxrdf::vocab::xsd::INTEGER).into(),
                graph_name: GraphName::DefaultGraph,
            },
            Quad {
                subject: nn("alice").into(),
                predicate: nn("bio"),
                object: Literal::new_simple_literal("a considerably longer string").into(),
                graph_name: GraphName::DefaultGraph,
            },
            Quad {
                subject: nn("claim1").into(),
                predicate: rdf::REIFIES.into_owned(),
                object: Term::Triple(Box::new(inner)),
                graph_name: GraphName::DefaultGraph,
            },
        ] {
            assert!(store.insert(q.as_ref()).unwrap());
            assert!(store.contains(q.as_ref()).unwrap(), "contains {q}");
        }
        assert_eq!(store.iter().count(), 3);
    }

    #[test]
    fn contains_is_false_for_unknown_terms() {
        let store = Store::new();
        // Must not intern anything just to answer the question.
        assert!(!store
            .contains(quad("nobody", "knows", "nothing", None).as_ref())
            .unwrap());
        assert_eq!(store.dictionary_len(), 0);
    }

    #[test]
    fn predicate_counts_track_inserts_and_removes() {
        let mut store = Store::new();
        store.insert(quad("a", "p", "x", None).as_ref()).unwrap();
        store.insert(quad("b", "p", "y", None).as_ref()).unwrap();
        store.insert(quad("c", "q", "z", None).as_ref()).unwrap();

        let p = store.lookup_term(nn("p").as_ref().into()).unwrap().unwrap();
        let q = store.lookup_term(nn("q").as_ref().into()).unwrap().unwrap();
        assert_eq!(store.predicate_count(p), 2);
        assert_eq!(store.predicate_count(q), 1);
        assert_eq!(store.predicate_histogram(), vec![(p, 2), (q, 1)]);

        store.remove(quad("a", "p", "x", None).as_ref()).unwrap();
        assert_eq!(store.predicate_count(p), 1);
        store.remove(quad("b", "p", "y", None).as_ref()).unwrap();
        assert_eq!(store.predicate_count(p), 0);
        assert_eq!(store.predicate_histogram(), vec![(q, 1)]);
    }

    #[test]
    fn removing_a_named_graph_updates_statistics() {
        let mut store = Store::new();
        store
            .insert(quad("a", "p", "x", Some("g1")).as_ref())
            .unwrap();
        store
            .insert(quad("b", "p", "y", Some("g1")).as_ref())
            .unwrap();
        store.insert(quad("c", "p", "z", None).as_ref()).unwrap();
        let p = store.lookup_term(nn("p").as_ref().into()).unwrap().unwrap();
        assert_eq!(store.predicate_count(p), 3);

        assert!(store.remove_named_graph(nn("g1").as_ref().into()).unwrap());
        assert_eq!(store.len(), 1, "only the default-graph quad remains");
        assert_eq!(
            store.predicate_count(p),
            1,
            "statistics must follow a graph drop, not just a quad delete"
        );
    }

    #[test]
    fn removing_an_unknown_quad_is_a_no_op() {
        let mut store = Store::new();
        store.insert(quad("a", "p", "x", None).as_ref()).unwrap();
        assert!(!store
            .remove(quad("a", "p", "unseen", None).as_ref())
            .unwrap());
        assert!(!store
            .remove(quad("unseen", "p", "x", None).as_ref())
            .unwrap());
        assert_eq!(store.len(), 1);
    }
}
