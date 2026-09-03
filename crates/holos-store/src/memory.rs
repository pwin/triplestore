//! The in-memory tier.
//!
//! [`Dictionary`] and [`QuadIndex`] behind the [`Storage`] trait. This is what an embedded
//! caller gets when it does not want a file on disk, and what the test suites run against
//! by default.

use crate::dictionary::Dictionary;
use crate::error::{Result, StorageError};
use crate::index::{EncodedQuad, GraphFilter, IdRange, QuadIndex, QuadScan};
use crate::storage::Storage;
use holos_core::TermId;
use oxrdf::{Term, TermRef};
use rustc_hash::FxHashMap;

/// One write to undo if a commit scope is abandoned.
#[derive(Debug, Clone, Copy)]
enum Undo {
    Inserted(EncodedQuad),
    Removed(EncodedQuad),
    GraphCreated(TermId),
    GraphDropped(TermId),
}

/// Quads and terms held in memory.
#[derive(Debug, Default, Clone)]
pub struct MemoryStorage {
    dictionary: Dictionary,
    index: QuadIndex,
    predicate_counts: FxHashMap<TermId, u64>,
    /// The open commit scope's journal, if one is open.
    ///
    /// An in-memory store applies as it goes and undoes on failure, rather than buffering and
    /// applying at the end. That is the right shape here for the reason the trait gives: a
    /// crash loses the whole store, so the only thing a scope can add is *failure* atomicity,
    /// and reads see their own writes without any arrangement.
    ///
    /// The dictionary is deliberately not journalled. It is append-only, an entry with no
    /// quad referring to it is unreachable rather than wrong, and `holos compact` is what
    /// reclaims those — rolling it back would mean invalidating ids a caller may hold.
    undo: Option<Vec<Undo>>,
}

impl MemoryStorage {
    /// An empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The term dictionary, for inspection.
    #[must_use]
    pub fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// The quad index, for inspection.
    #[must_use]
    pub fn index(&self) -> &QuadIndex {
        &self.index
    }

    /// Notes a write, if a scope is open to note it in.
    fn record(&mut self, entry: Undo) {
        if let Some(undo) = self.undo.as_mut() {
            undo.push(entry);
        }
    }

    fn decrement(&mut self, predicate: TermId) {
        if let Some(n) = self.predicate_counts.get_mut(&predicate) {
            *n -= 1;
            if *n == 0 {
                self.predicate_counts.remove(&predicate);
            }
        }
    }
}

impl Storage for MemoryStorage {
    fn encode(&mut self, term: TermRef<'_>) -> Result<TermId> {
        self.dictionary.encode(term)
    }

    fn lookup(&self, term: TermRef<'_>) -> Result<Option<TermId>> {
        self.dictionary.lookup(term)
    }

    fn decode(&self, id: TermId) -> Result<Option<Term>> {
        self.dictionary.decode(id)
    }

    fn dictionary_len(&self) -> usize {
        self.dictionary.len()
    }

    fn begin(&mut self) -> Result<()> {
        if self.undo.is_some() {
            return Err(StorageError::corruption(
                "a commit scope is already open; nesting them would read as two commits and \
                 be one",
            ));
        }
        self.undo = Some(Vec::new());
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if self.undo.take().is_none() {
            return Err(StorageError::corruption("no commit scope is open"));
        }
        // Nothing to write: an in-memory store applies as it goes and the journal exists only
        // to undo. Dropping it is the commit.
        Ok(())
    }

    fn rollback(&mut self) {
        let Some(undo) = self.undo.take() else {
            return;
        };
        // Backwards, so a quad inserted and then removed within the scope comes back to the
        // state it started in rather than to whichever the last operation happened to be.
        for entry in undo.into_iter().rev() {
            let outcome = match entry {
                Undo::Inserted(quad) => self.remove_encoded(quad),
                Undo::Removed(quad) => self.insert_encoded(quad),
                Undo::GraphCreated(g) => self.remove_named_graph(g),
                Undo::GraphDropped(g) => self.insert_named_graph(g),
            };
            // The index is in memory and these are the inverses of operations it just
            // performed, so a failure here is not something a caller can act on. Ignored
            // rather than propagated, which is what lets `rollback` be infallible and the
            // failure path have no failure path of its own.
            let _ = outcome;
        }
    }

    fn in_scope(&self) -> bool {
        self.undo.is_some()
    }

    fn dictionary_count_for(&self, tag: holos_core::Tag) -> usize {
        self.dictionary.count_for(tag)
    }

    fn insert_encoded(&mut self, quad: EncodedQuad) -> Result<bool> {
        // A quad creates its graph, and `QuadIndex::insert` does that itself — so whether the
        // graph is new has to be asked *before* the insert. Asking after gets `false` every
        // time, and the graph is left behind by a rollback that undid everything else.
        // Only worth asking while a scope is open, since otherwise nobody is journalling it.
        let new_graph = match quad.graph_name {
            Some(g) if self.undo.is_some() => !self.index.contains_named_graph(g)?,
            _ => false,
        };
        if self.index.insert(quad)? {
            *self.predicate_counts.entry(quad.predicate).or_insert(0) += 1;
            if let Some(g) = quad.graph_name {
                self.index.insert_named_graph(g)?;
                // Journalled separately from the quad because it is a separate write: undoing
                // the quad does not undo the graph, and a rollback that put the quads back
                // but left the graph behind leaves a named graph nothing ever created.
                if new_graph {
                    self.record(Undo::GraphCreated(g));
                }
            }
            self.record(Undo::Inserted(quad));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn remove_encoded(&mut self, quad: EncodedQuad) -> Result<bool> {
        if self.index.remove(quad)? {
            self.decrement(quad.predicate);
            self.record(Undo::Removed(quad));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn contains_encoded(&self, quad: EncodedQuad) -> Result<bool> {
        Ok(self
            .index
            .quads_for_pattern(
                Some(quad.subject),
                Some(quad.predicate),
                Some(quad.object),
                match quad.graph_name {
                    Some(g) => GraphFilter::Named(g),
                    None => GraphFilter::Default,
                },
            )
            .next()
            .transpose()?
            .is_some())
    }

    fn scan(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        self.index
            .quads_for_pattern(subject, predicate, object, graph)
    }

    fn len(&self) -> usize {
        self.index.len()
    }

    fn insert_named_graph(&mut self, graph: TermId) -> Result<bool> {
        // Recorded before the call so the journal is written once rather than in each arm.
        let created = !self.index.contains_named_graph(graph)?;
        if created {
            self.record(Undo::GraphCreated(graph));
        }
        self.index.insert_named_graph(graph)
    }

    fn remove_named_graph(&mut self, graph: TermId) -> Result<bool> {
        let doomed = self
            .index
            .quads_for_pattern(None, None, None, GraphFilter::Named(graph))
            .collect::<Result<Vec<_>>>()?;
        // Each quad separately, so a rollback puts the graph's contents back and not merely
        // its name. Dropping a graph is the one write whose undo is not a single operation.
        for quad in &doomed {
            self.record(Undo::Removed(*quad));
        }
        for quad in doomed {
            self.decrement(quad.predicate);
        }
        if self.index.contains_named_graph(graph)? {
            // Dropped, so the undo is to create it again. Recording `GraphCreated` here —
            // which is what the first version of this did — makes the rollback drop a graph
            // the scope had only ever read.
            self.record(Undo::GraphDropped(graph));
        }
        self.index.remove_named_graph(graph)
    }

    fn quads_with_object_in(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        span: IdRange,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        self.index
            .quads_with_object_in(subject, predicate, span, graph)
    }

    fn contains_named_graph(&self, graph: TermId) -> Result<bool> {
        self.index.contains_named_graph(graph)
    }

    fn named_graphs(&self) -> Result<Vec<TermId>> {
        self.index.named_graphs().collect()
    }

    fn predicate_count(&self, predicate: TermId) -> u64 {
        self.predicate_counts.get(&predicate).copied().unwrap_or(0)
    }

    fn predicate_histogram(&self) -> Vec<(TermId, u64)> {
        let mut v: Vec<_> = self
            .predicate_counts
            .iter()
            .map(|(k, n)| (*k, *n))
            .collect();
        // Sort by count, then by id, so the output is deterministic across runs — the
        // reproducibility property SHACL_Engine gets right and worth keeping everywhere.
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
