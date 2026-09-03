//! The seam between the store's API and whatever actually holds the bytes.
//!
//! `DESIGN.md` §6 describes two tiers. Tier A is the system of record — in memory for
//! embedding and tests, on RocksDB for anything durable. Tier B, the hypertrie, is not
//! built and is gated on measurement (§13 Q2).
//!
//! # Why one trait rather than two
//!
//! The dictionary and the nine index orders are behind the *same* trait, not separate
//! ones, because a persistent backend has to write both atomically: a quad insert that
//! records index entries but loses the dictionary rows that decode them leaves a store
//! that cannot answer. Splitting the trait would make that atomicity impossible to state.
//!
//! # Why it exists before there are two implementations
//!
//! Building it after the second backend arrives means discovering, three layers up, which
//! `BTreeSet`-shaped assumptions leaked into the API. The scan already yields
//! [`Result`](crate::Result) for the same reason.

use crate::error::Result;
use crate::index::{EncodedQuad, GraphFilter, QuadScan};
use holos_core::TermId;
use oxrdf::{Term, TermRef};

/// A place quads and their terms live.
///
/// Reads take `&self` and writes `&mut self`, which is what gives a persistent
/// implementation its single-writer/many-readers discipline for free.
///
/// `Sync` as well as `Send`: the HTTP server (L6) puts a store behind an `RwLock` and
/// serves reads from many threads at once, which is exactly the access pattern the
/// `&self`/`&mut self` split was chosen for.
pub trait Storage: std::fmt::Debug + Send + Sync {
    // --- dictionary ---------------------------------------------------------------

    /// Interns a term, allocating an id if it is new.
    fn encode(&mut self, term: TermRef<'_>) -> Result<TermId>;

    /// Looks a term up without interning it. `Ok(None)` means it has never been seen.
    fn lookup(&self, term: TermRef<'_>) -> Result<Option<TermId>>;

    /// Turns an id back into a term. `Ok(None)` means this store never issued it.
    fn decode(&self, id: TermId) -> Result<Option<Term>>;

    /// How many terms the dictionary holds.
    ///
    /// Inline and well-known terms are absent by design, so this is smaller — often much
    /// smaller — than the number of distinct terms in the data. Access policy uses it as
    /// a staleness signal, so it must be cheap: a counter, never a scan.
    fn dictionary_len(&self) -> usize;

    /// How many ids have been issued for one dictionary-backed tag.
    ///
    /// Each kind has its own dense index space, so this is also an enumeration bound:
    /// `TermId::new(tag, i)` for `i` in `0..dictionary_count_for(tag)` is every id issued for
    /// that tag. Must be a counter rather than a scan, for the same reason
    /// [`Storage::dictionary_len`] must be.
    fn dictionary_count_for(&self, tag: holos_core::Tag) -> usize;

    // --- quads --------------------------------------------------------------------

    /// Indexes an already-encoded quad. `Ok(true)` if it was not already present.
    fn insert_encoded(&mut self, quad: EncodedQuad) -> Result<bool>;

    /// Removes an encoded quad. `Ok(true)` if it was present.
    fn remove_encoded(&mut self, quad: EncodedQuad) -> Result<bool>;

    /// Whether an encoded quad is present.
    fn contains_encoded(&self, quad: EncodedQuad) -> Result<bool>;

    /// Every quad matching a pattern. `None` in a position means unbound.
    fn scan(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
        graph: GraphFilter,
    ) -> QuadScan<'_>;

    /// Number of quads. A counter, never a scan.
    fn len(&self) -> usize;

    /// True when no quads are stored.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // --- named graphs -------------------------------------------------------------

    /// Records a named graph, which may hold no quads.
    fn insert_named_graph(&mut self, graph: TermId) -> Result<bool>;

    /// Drops a named graph and everything in it.
    fn remove_named_graph(&mut self, graph: TermId) -> Result<bool>;

    /// Whether a named graph exists.
    fn contains_named_graph(&self, graph: TermId) -> Result<bool>;

    /// Every named graph, whether or not it holds quads.
    ///
    /// Returns a `Vec` rather than an iterator: the count is small, and materialising it
    /// keeps a persistent backend from holding an iterator across a write.
    fn named_graphs(&self) -> Result<Vec<TermId>>;

    // --- statistics -----------------------------------------------------------------

    /// How many quads use a predicate.
    ///
    /// Infallible in both tiers: the counts are held in memory and persisted alongside
    /// the data, not scanned. §7 replaces this with characteristic sets and HLL sketches;
    /// §14.6 constrains who may see any of it, because a global count reveals data a
    /// principal may not be allowed to read.
    fn predicate_count(&self, predicate: TermId) -> u64;

    /// Every predicate with its count, most frequent first, deterministically ordered.
    fn predicate_histogram(&self) -> Vec<(TermId, u64)>;

    // --- durability -------------------------------------------------------------------

    /// Makes everything written so far durable. A no-op for the in-memory tier.
    fn flush(&mut self) -> Result<()>;

    /// Writes a consistent snapshot of the store to `destination`.
    ///
    /// The point of a checkpoint is that it can be taken while the store is *open* and
    /// being written to, which is what makes an online backup possible. Copying the
    /// directory instead requires stopping the service, because an LSM tree in mid-flight
    /// is not a set of files that can be copied one at a time.
    ///
    /// The default is a refusal rather than a silent copy: a backend that cannot produce a
    /// consistent snapshot should say so, not hand back something that looks like one.
    ///
    /// # Errors
    ///
    /// [`StorageError::Unsupported`] on a backend without checkpoints, and whatever the
    /// backend reports otherwise.
    fn checkpoint(&self, destination: &std::path::Path) -> Result<()> {
        let _ = destination;
        Err(crate::StorageError::Unsupported(
            "this storage backend cannot take a checkpoint".to_owned(),
        ))
    }

    /// Opens a commit scope: writes until [`Storage::commit`] land together or not at all.
    ///
    /// A single quad write is already atomic — every index order for it goes into one
    /// batch — so what this adds is atomicity across *several*. An update or a holon tick is
    /// a sequence of quad writes, and without a scope a crash between two of them leaves half
    /// a commit: `DESIGN.md` §9's "a crash between applying the delta and writing the event
    /// leaves the two disagreeing".
    ///
    /// What it does **not** give is isolation. A reader with its own handle can see the store
    /// before the commit or after it, and this makes no promise about which — the server and
    /// the Python binding hold the engine behind an `RwLock`, so a writer excludes readers
    /// there and the question does not arise. Isolation is `DESIGN.md` §6.1's MVCC and is
    /// still not built.
    ///
    /// Reads inside the scope see the scope's own writes. A backend that buffered without
    /// arranging that would break duplicate detection, which is a read.
    ///
    /// The default is a no-op pair, which gives a backend that cannot do better the same
    /// *interface* and a weaker guarantee. That is a deliberate choice against a default that
    /// returned an error: a caller should be able to write correct code once, and a backend
    /// where a crash loses everything anyway — the in-memory one — satisfies "all or nothing"
    /// vacuously.
    ///
    /// # Errors
    ///
    /// Nesting is refused rather than flattened: a scope inside a scope reads as two commits
    /// and is one, and silently making it one is how a caller comes to believe in a rollback
    /// point that does not exist.
    fn begin(&mut self) -> Result<()> {
        Ok(())
    }

    /// Commits a scope opened by [`Storage::begin`].
    ///
    /// # Errors
    ///
    /// A write failure, or no scope open.
    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    /// Abandons a scope, leaving the store as it was when [`Storage::begin`] was called.
    ///
    /// Infallible on purpose. It is called on the failure path, and a rollback that can fail
    /// leaves a state nobody can describe — which is exactly what the compensating-write
    /// undo it replaces could do.
    fn rollback(&mut self) {}

    /// Whether a commit scope is open.
    fn in_scope(&self) -> bool {
        false
    }

    /// Announces a bulk load, so a backend can buffer writes and skip its log.
    ///
    /// A default no-op rather than a downcast: a caller should be able to ask any backend
    /// for its fastest load path without knowing which backend it has.
    ///
    /// # Errors
    ///
    /// A commit scope open. The two both buffer, and a backend that started a load inside a
    /// scope would route the load's writes into the scope and then rebuild its counters from
    /// a database that has not seen them. They are refused in both directions, so neither can
    /// be the one that happens to be checked.
    fn begin_bulk_load(&mut self) -> Result<()> {
        Ok(())
    }

    /// Ends a bulk load, writing anything buffered.
    fn end_bulk_load(&mut self) -> Result<()> {
        self.flush()
    }
}
