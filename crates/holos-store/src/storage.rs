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
use crate::index::{EncodedQuad, GraphFilter, IdRange, QuadScan};
use holos_core::TermId;
use oxrdf::{Term, TermRef};

/// A place quads and their terms live.
///
/// Reads take `&self` and writes `&mut self`, which is what gives a persistent
/// How many times a bulk load asked the on-disk dictionary for a term, and what it cost.
///
/// The term cache in front of the dictionary is cleared at every mid-load flush, so a term
/// seen again afterwards is a *hit* here — one read that finds it — and every new term is a
/// *miss* — one read that does not, before the term is allocated. Both are random reads
/// against a family that grows with the load, which is why they are counted rather than
/// assumed: on a 653.8-million-triple load the dictionary layer ran 4.2× slower per quad
/// than a 3-million-triple profile predicted, and these two are the candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BulkResolves {
    /// Reads that found the term: a recurring term, re-looked-up after a flush dropped it.
    pub hits: u64,
    /// Reads that did not: a term seen for the first time.
    pub misses: u64,
    /// Time spent inside those reads, in nanoseconds.
    pub nanos: u64,
    /// Reads that never happened, because the load could prove the term was new. Each of
    /// these would have been a miss.
    pub skipped: u64,
    /// Cache entries carried across a flush because they were hit often enough to be worth
    /// keeping, summed over every flush. Each one saved a hit for the next window.
    pub retained: u64,
    /// Reads by cost: under 10 µs, 10–50, 50–200, and over 200. A mean of 28 µs can be
    /// every read costing 28, or most costing 5 and a few costing 300 — a CPU problem or a
    /// disk problem — and only the shape says which.
    pub buckets: [u64; 4],
}

impl BulkResolves {
    /// Files a read of `nanos` into its cost bucket.
    pub fn record(&mut self, nanos: u64) {
        self.nanos += nanos;
        let i = match nanos {
            n if n < 10_000 => 0,
            n if n < 50_000 => 1,
            n if n < 200_000 => 2,
            _ => 3,
        };
        self.buckets[i] += 1;
    }
}

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

    /// A counter that moves every time the set of quads changes.
    ///
    /// Insert, delete, bulk load: each advances it, and a rolled-back scope puts it back.
    /// Two reads that return the same value bracket an interval in which no quad was added
    /// or removed — which is the question anything derived from the quads has to ask before
    /// trusting itself. Statistics are the case in hand: a snapshot taken at generation `g`
    /// describes the store exactly while `generation()` still returns `g`, and describes
    /// something else the moment it does not.
    ///
    /// `quad_count` cannot serve, because deleting a quad and inserting another leaves it
    /// where it was. Nor can the dictionary's size, since the dictionary is append-only and
    /// the new quad may name only terms it already held. This is the counter those two are
    /// not.
    fn generation(&self) -> u64;

    /// Keeps a statistics snapshot with the store, replacing any it already holds.
    ///
    /// The bytes are opaque here; `holos-stats` owns the format and records the generation
    /// inside them. This does **not** advance the generation — a snapshot describes the
    /// quads and is not one of them.
    ///
    /// # Errors
    ///
    /// If the write fails.
    fn save_statistics(&mut self, bytes: &[u8]) -> Result<()>;

    /// The snapshot [`Storage::save_statistics`] kept, if there is one.
    ///
    /// # Errors
    ///
    /// If the read fails.
    fn load_statistics(&self) -> Result<Option<Vec<u8>>>;

    /// How many ids have been issued for one dictionary-backed tag.
    ///
    /// Each kind has its own dense index space, so this is also an enumeration bound:
    /// `TermId::new(tag, i)` for `i` in `0..dictionary_count_for(tag)` is every id issued for
    /// that tag. Must be a counter rather than a scan, for the same reason
    /// [`Storage::dictionary_len`] must be.
    fn dictionary_count_for(&self, tag: holos_core::Tag) -> usize;

    /// Decodes every id issued for `tag` in `from..to`, ascending, handing each to `f`.
    ///
    /// Ids the store never issued are skipped, and an error from `f` stops the walk. The
    /// default is `to - from` calls to [`Storage::decode`], which is correct for any backend
    /// and costs a point lookup per id. A backend whose dictionary is a sorted structure
    /// should override it with one range read, because the difference is not small: the
    /// spatial index's first build walks every literal, and on a 653-million-triple store
    /// that was **three and a half minutes** of point lookups before the server would listen,
    /// for a dataset holding no geometries at all.
    ///
    /// # Errors
    ///
    /// Whatever [`Storage::decode`] or `f` returns.
    fn for_each_in_range(
        &self,
        tag: holos_core::Tag,
        from: usize,
        to: usize,
        f: &mut dyn FnMut(TermId, Term) -> Result<()>,
    ) -> Result<()> {
        for i in from..to {
            let id = TermId::new(tag, i as u64);
            if let Some(term) = self.decode(id)? {
                f(id, term)?;
            }
        }
        Ok(())
    }

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

    /// Quads matching the pattern whose **object** lies inside `span`.
    ///
    /// The narrowing `DESIGN.md` §5's order-preserving encodings exist for: an inline
    /// integer, float or dateTime has an id whose order is its value's order, so a
    /// comparison against a constant is a bound on the index rather than a test applied to
    /// everything the index returns.
    ///
    /// **The span narrows what is read; it does not decide what matches.** A caller still
    /// applies its own filter, and must pass a span admitting at least everything that
    /// could match — a numeric comparison, for instance, can be satisfied by an
    /// `xsd:decimal`, which the inline codec declines and the dictionary therefore holds in
    /// no particular order.
    ///
    /// The default implementation scans and filters, which is correct and buys nothing. A
    /// backend overrides it when its index can be bounded.
    fn quads_with_object_in(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        span: IdRange,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        Box::new(
            self.scan(subject, predicate, None, graph)
                .filter(move |quad| match quad {
                    Ok(quad) => span.contains(quad.object),
                    Err(_) => true,
                }),
        )
    }

    /// How many times the most recent bulk load spilled its buffer to disk.
    ///
    /// Zero for a backend that holds everything in memory anyway, which is why the default
    /// is zero rather than an error: "it did not spill" is true of them.
    fn bulk_spills(&self) -> usize {
        0
    }

    /// What the dictionary cost during a bulk load, as the load saw it.
    ///
    /// Zero for a backend with no dictionary on disk. See [`BulkResolves`].
    fn bulk_resolves(&self) -> BulkResolves {
        BulkResolves::default()
    }

    /// How many bytes this store occupies on disk, if it is on disk at all.
    ///
    /// `None` for a backend with no files. Used to decide whether a maintenance operation
    /// will fit before it starts: a checkpoint eventually owes this much, and a compaction
    /// needs it immediately because it writes a second store beside the first.
    ///
    /// Everything is counted, not just the SST files — the write-ahead log, the manifest and
    /// anything else in the directory all have to be copied by a backup that cannot hard-link.
    fn on_disk_bytes(&self) -> Option<u64> {
        None
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
