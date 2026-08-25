//! The in-memory tier.
//!
//! [`Dictionary`] and [`QuadIndex`] behind the [`Storage`] trait. This is what an embedded
//! caller gets when it does not want a file on disk, and what the test suites run against
//! by default.

use crate::dictionary::Dictionary;
use crate::error::Result;
use crate::index::{EncodedQuad, GraphFilter, QuadIndex, QuadScan};
use crate::storage::Storage;
use holos_core::TermId;
use oxrdf::{Term, TermRef};
use rustc_hash::FxHashMap;

/// Quads and terms held in memory.
#[derive(Debug, Default, Clone)]
pub struct MemoryStorage {
    dictionary: Dictionary,
    index: QuadIndex,
    predicate_counts: FxHashMap<TermId, u64>,
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

    fn insert_encoded(&mut self, quad: EncodedQuad) -> Result<bool> {
        if self.index.insert(quad)? {
            *self.predicate_counts.entry(quad.predicate).or_insert(0) += 1;
            if let Some(g) = quad.graph_name {
                self.index.insert_named_graph(g)?;
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn remove_encoded(&mut self, quad: EncodedQuad) -> Result<bool> {
        if self.index.remove(quad)? {
            self.decrement(quad.predicate);
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
        self.index.insert_named_graph(graph)
    }

    fn remove_named_graph(&mut self, graph: TermId) -> Result<bool> {
        let doomed = self
            .index
            .quads_for_pattern(None, None, None, GraphFilter::Named(graph))
            .collect::<Result<Vec<_>>>()?;
        for quad in doomed {
            self.decrement(quad.predicate);
        }
        self.index.remove_named_graph(graph)
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
