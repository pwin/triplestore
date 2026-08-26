//! The RocksDB tier — `DESIGN.md` §6.1.
//!
//! Oxigraph's column-family layout, because it is already the right answer: six quad
//! orders, three default-graph triple orders that avoid paying for a graph column, and the
//! dictionary alongside. What differs is what goes *in* the keys — dense 64-bit
//! [`TermId`]s rather than 128-bit hashes, which shrinks a quad key from about 64 bytes to
//! 32 and keeps the door open for the Tier B trie (§5).
//!
//! # What is here, and what §6.1 still owes
//!
//! Implemented: the thirteen column families, atomic writes through a single batch,
//! prefix-bounded scans, persisted statistics, and a bulk-load path.
//!
//! Not yet: user-defined timestamps for MVCC and time travel, merge-operator refcounts on
//! the dictionary, checkpoints for branching, and BlobDB value separation. Each is a
//! discrete addition rather than a redesign, which is the point of building the seam
//! first.

mod codec;

use crate::error::{Result, StorageError};
use crate::index::{EncodedQuad, GraphFilter, QuadScan};
use crate::storage::Storage;
use codec::{key, prefix_upper_bound, put_id, put_term, read_id, split_key, StoredTerm};
use holos_core::{inline, vocab, Tag, TermId, FORMAT_VERSION};
use oxrdf::{Term, TermRef};
use rocksdb::{
    ColumnFamilyDescriptor, DBCompressionType, IteratorMode, Options, ReadOptions, WriteBatch,
    WriteOptions, DB,
};
use rustc_hash::FxHashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

// --- column families ---------------------------------------------------------------

/// RocksDB's unnamed default family, where the metadata keys live.
const DEFAULT_CF: &str = "default";
const ID2STR: &str = "id2str";
const STR2ID: &str = "str2id";
const GRAPHS: &str = "graphs";
const STATS: &str = "stats";
/// Quad orders with the graph last, for patterns where the graph is unbound.
const SPOG: &str = "spog";
const POSG: &str = "posg";
const OSPG: &str = "ospg";
/// Quad orders with the graph first, for patterns where the graph is bound.
const GSPO: &str = "gspo";
const GPOS: &str = "gpos";
const GOSP: &str = "gosp";
/// Default-graph triple orders — three components per key, no graph column.
const DSPO: &str = "dspo";
const DPOS: &str = "dpos";
const DOSP: &str = "dosp";

const QUAD_ORDERS: [&str; 6] = [SPOG, POSG, OSPG, GSPO, GPOS, GOSP];
const TRIPLE_ORDERS: [&str; 3] = [DSPO, DPOS, DOSP];

/// Metadata keys in the default column family.
const META_VERSION: &[u8] = b"format_version";
const META_QUADS: &[u8] = b"quad_count";
const META_NEXT: [(&[u8], Tag); 4] = [
    (b"next_iri", Tag::Iri),
    (b"next_blank", Tag::BlankNode),
    (b"next_literal", Tag::Literal),
    (b"next_triple_term", Tag::TripleTerm),
];

/// Serialised terms at or below this length are their own dictionary key. Longer ones are
/// keyed by a hash — see [`RocksStorage::lookup`] for why that stays exact.
const INLINE_KEY_MAX: usize = 512;
const KEY_EXACT: u8 = 0;
const KEY_HASHED: u8 = 1;

/// Quads and terms in a RocksDB database.
#[derive(Debug)]
pub struct RocksStorage {
    db: DB,
    /// Next dense index per dictionary-backed tag. Held in memory, persisted in the same
    /// batch as the row it numbers, so a crash cannot reissue an id.
    next: FxHashMap<Tag, u64>,
    quad_count: u64,
    predicate_counts: FxHashMap<TermId, u64>,
    /// `Some` while a bulk load is running: see [`RocksStorage::begin_bulk_load`].
    bulk: Option<BulkState>,
}

/// What a bulk load has to remember that the database cannot yet answer.
///
/// Writes are buffered, so a `get` during the load does not see them. Two consequences
/// have to be handled rather than tolerated:
///
/// - **Term ids.** Interning the same new term twice within one buffer would allocate two
///   ids for one term and silently alias them. The map below is therefore mandatory, not
///   an optimisation, and it bounds a load's memory by its number of *distinct* terms.
///   Loads bigger than memory need the external merge sort that `DESIGN.md` §6.1 pairs
///   with `SstFileWriter`, which is not built yet.
/// - **Counters.** Duplicate quads cannot be detected against the buffer cheaply, so
///   `quad_count` and the predicate statistics are recomputed by one pass over the index
///   when the load finishes, rather than maintained during it.
/// One buffered write, held as owned bytes.
///
/// A `rocksdb::WriteBatch` would be the natural buffer and cannot be used: it wraps a raw
/// pointer and is therefore not `Sync`, which would make the whole store un-shareable
/// across the HTTP server's request threads. Owning the bytes costs one allocation per
/// key over a batch's shared buffer, and buys a store that many readers can hold at once.
#[derive(Debug)]
enum Pending {
    Put(&'static str, Box<[u8]>, Box<[u8]>),
    Delete(&'static str, Box<[u8]>),
}

#[derive(Default)]
struct BulkState {
    pending: Vec<Pending>,
    /// Terms interned since the last write, keyed by their **exact serialised bytes**.
    ///
    /// Not by the `str2id` key: past 512 bytes that key is a hash, and two different long
    /// literals sharing one would be handed the same id. On disk that case is resolved by
    /// verifying candidates against `id2str`, but a buffered term is not on disk yet, so
    /// the buffer has to be exact.
    terms: FxHashMap<Vec<u8>, TermId>,
}

impl std::fmt::Debug for BulkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkState")
            .field("buffered_ops", &self.pending.len())
            .field("distinct_terms", &self.terms.len())
            .finish()
    }
}

/// How many buffered operations trigger a write.
const BULK_BATCH_OPS: usize = 64 * 1024;

impl RocksStorage {
    /// Opens or creates a store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        db_opts.set_compression_type(DBCompressionType::Lz4);
        // One writer, many readers — the discipline the `Storage` trait already encodes
        // by taking `&mut self` for writes.
        db_opts.increase_parallelism(num_cpus());
        // Three index copies per triple means a lot of small keys; the default 64 MiB
        // memtable flushes constantly under a load.
        db_opts.set_write_buffer_size(256 * 1024 * 1024);
        db_opts.set_max_write_buffer_number(4);
        db_opts.set_min_write_buffer_number_to_merge(2);
        db_opts.set_max_background_jobs(num_cpus());

        let mut families = vec![ColumnFamilyDescriptor::new(ID2STR, value_opts())];
        families.push(ColumnFamilyDescriptor::new(STR2ID, value_opts()));
        families.push(ColumnFamilyDescriptor::new(GRAPHS, index_opts(codec::ID)));
        families.push(ColumnFamilyDescriptor::new(STATS, value_opts()));
        for name in QUAD_ORDERS {
            families.push(ColumnFamilyDescriptor::new(name, index_opts(codec::ID)));
        }
        for name in TRIPLE_ORDERS {
            families.push(ColumnFamilyDescriptor::new(name, index_opts(codec::ID)));
        }

        let db = DB::open_cf_descriptors(&db_opts, path, families).map_err(rocks_err)?;

        // Refuse a store written by an incompatible encoding rather than silently
        // decoding its term ids as something else (`holos_core::FORMAT_VERSION`).
        match db.get(META_VERSION).map_err(rocks_err)? {
            Some(bytes) => {
                let found = u32::from_be_bytes(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| StorageError::corruption("bad format version"))?,
                );
                if found != FORMAT_VERSION {
                    return Err(StorageError::corruption(format!(
                        "store was written with format version {found}, this build speaks \
                         {FORMAT_VERSION}"
                    )));
                }
            }
            None => db
                .put(META_VERSION, FORMAT_VERSION.to_be_bytes())
                .map_err(rocks_err)?,
        }

        let mut next = FxHashMap::default();
        for (meta_key, tag) in META_NEXT {
            next.insert(tag, read_u64(&db, meta_key)?.unwrap_or(0));
        }
        let quad_count = read_u64(&db, META_QUADS)?.unwrap_or(0);

        // Statistics are small — one row per predicate — so they are read once at open
        // and served from memory, which is what lets `predicate_count` be infallible.
        let mut predicate_counts = FxHashMap::default();
        let cf = cf(&db, STATS)?;
        for row in db.iterator_cf(cf, IteratorMode::Start) {
            let (k, v) = row.map_err(rocks_err)?;
            predicate_counts.insert(read_id(&k)?, u64::from_be_bytes(to_8(&v)?));
        }

        Ok(Self {
            db,
            next,
            quad_count,
            predicate_counts,
            bulk: None,
        })
    }

    /// Starts a bulk load: writes are buffered into large batches and the write-ahead
    /// log is skipped.
    ///
    /// A load interrupted part-way leaves the store in an unspecified state and must be
    /// discarded — that is the trade being made for the speed. `DESIGN.md` §6.1 wants
    /// `SstFileWriter` ingestion here eventually, which is faster still *and* keeps crash
    /// safety, at the cost of needing its input sorted.
    fn start_bulk(&mut self) {
        self.bulk = Some(BulkState::default());
        // Auto-compaction during a load is wasted work: it rewrites levels that the rest
        // of the load is about to invalidate. Turned off here and re-enabled, followed by
        // one deliberate compaction, when the load ends.
        self.set_compactions(false);
    }

    /// Enables or disables automatic compaction on every index family.
    fn set_compactions(&self, enabled: bool) {
        let value = if enabled { "false" } else { "true" };
        for name in QUAD_ORDERS.iter().chain(TRIPLE_ORDERS.iter()) {
            if let Ok(handle) = cf(&self.db, name) {
                // A failure here costs throughput, never correctness, so it is not fatal.
                let _ = self
                    .db
                    .set_options_cf(handle, &[("disable_auto_compactions", value)]);
            }
        }
    }

    /// Ends a bulk load, writing what is buffered and rebuilding the counters.
    fn finish_bulk(&mut self) -> Result<()> {
        if let Some(state) = self.bulk.take() {
            self.write_pending(state.pending, true)?;
        }
        self.set_compactions(true);
        self.recount()
    }

    /// Recomputes the quad count and predicate statistics from the index.
    ///
    /// One pass, at the end of a load, instead of maintaining counters against writes the
    /// buffer has not yet made visible.
    fn recount(&mut self) -> Result<()> {
        let mut counts: FxHashMap<TermId, u64> = FxHashMap::default();
        let mut total: u64 = 0;
        for row in self.scan_order(DSPO, &[])? {
            let [_, p, _] = split_key::<3>(&row?)?;
            *counts.entry(p).or_insert(0) += 1;
            total += 1;
        }
        for row in self.scan_order(GSPO, &[])? {
            let [_, _, p, _] = split_key::<4>(&row?)?;
            *counts.entry(p).or_insert(0) += 1;
            total += 1;
        }
        let mut batch = WriteBatch::default();
        let stats = cf(&self.db, STATS)?;
        for old in self.predicate_counts.keys() {
            batch.delete_cf(stats, put_id(*old));
        }
        for (p, n) in &counts {
            batch.put_cf(stats, put_id(*p), n.to_be_bytes());
        }
        batch.put(META_QUADS, total.to_be_bytes());
        self.db.write(batch).map_err(rocks_err)?;
        // The counters are now authoritative again.
        self.predicate_counts = counts;
        self.quad_count = total;
        Ok(())
    }

    /// Compacts every column family. Worth running once after a bulk load.
    pub fn compact(&mut self) -> Result<()> {
        for name in QUAD_ORDERS.iter().chain(TRIPLE_ORDERS.iter()) {
            self.db
                .compact_range_cf(cf(&self.db, name)?, None::<&[u8]>, None::<&[u8]>);
        }
        Ok(())
    }

    /// Records one write, buffering it during a bulk load.
    fn put(&mut self, family: &'static str, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        match self.bulk.as_mut() {
            Some(state) => {
                state
                    .pending
                    .push(Pending::Put(family, key.into(), value.into()));
                Ok(())
            }
            None => {
                let mut batch = WriteBatch::default();
                batch.put_cf(cf(&self.db, family)?, key, value);
                self.db.write(batch).map_err(rocks_err)
            }
        }
    }

    /// Records one delete, buffering it during a bulk load.
    fn delete(&mut self, family: &'static str, key: Vec<u8>) -> Result<()> {
        match self.bulk.as_mut() {
            Some(state) => {
                state.pending.push(Pending::Delete(family, key.into()));
                Ok(())
            }
            None => {
                let mut batch = WriteBatch::default();
                batch.delete_cf(cf(&self.db, family)?, key);
                self.db.write(batch).map_err(rocks_err)
            }
        }
    }

    /// Applies a group of writes atomically, or buffers them.
    fn commit_ops(&mut self, ops: Vec<Pending>) -> Result<()> {
        if let Some(state) = self.bulk.as_mut() {
            state.pending.extend(ops);
            if state.pending.len() >= BULK_BATCH_OPS {
                let pending = std::mem::take(&mut state.pending);
                return self.write_pending(pending, true);
            }
            return Ok(());
        }
        self.write_pending(ops, false)
    }

    /// Turns buffered operations into one `WriteBatch` and writes it.
    fn write_pending(&self, pending: Vec<Pending>, bulk: bool) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        for op in pending {
            match op {
                Pending::Put(family, key, value) => {
                    batch.put_cf(cf(&self.db, family)?, key, value);
                }
                Pending::Delete(family, key) => {
                    batch.delete_cf(cf(&self.db, family)?, key);
                }
            }
        }
        if bulk {
            self.db
                .write_opt(batch, &bulk_write_opts())
                .map_err(rocks_err)
        } else {
            self.db.write(batch).map_err(rocks_err)
        }
    }

    /// Allocates the next dense id for a tag and records the new counter in `batch`.
    fn allocate(&mut self, tag: Tag, ops: &mut Vec<Pending>) -> Result<TermId> {
        let slot = self.next.entry(tag).or_insert(0);
        let index = *slot;
        *slot += 1;
        let meta_key = META_NEXT
            .iter()
            .find(|(_, t)| *t == tag)
            .map(|(k, _)| *k)
            .ok_or_else(|| StorageError::corruption(format!("{tag:?} has no id counter")))?;
        ops.push(Pending::Put(
            DEFAULT_CF,
            meta_key.to_vec().into(),
            (index + 1).to_be_bytes().to_vec().into(),
        ));
        Ok(TermId::new(tag, index))
    }

    /// The `str2id` key for a serialised term, and whether candidates need verifying.
    fn dictionary_key(serialised: &[u8]) -> (Vec<u8>, bool) {
        if serialised.len() <= INLINE_KEY_MAX {
            let mut k = Vec::with_capacity(serialised.len() + 1);
            k.push(KEY_EXACT);
            k.extend_from_slice(serialised);
            (k, false)
        } else {
            let mut k = Vec::with_capacity(17);
            k.push(KEY_HASHED);
            k.extend_from_slice(&hash128(serialised));
            (k, true)
        }
    }

    /// Interns a term, writing any new rows into `batch`.
    fn encode_into(&mut self, term: TermRef<'_>, ops: &mut Vec<Pending>) -> Result<TermId> {
        // Well-known and inline terms never touch the dictionary at all (§5).
        if let TermRef::NamedNode(n) = term {
            if let Some(id) = vocab::encode_iri(n.as_str()) {
                return Ok(id);
            }
        }
        if let TermRef::Literal(l) = term {
            if let Some(id) = inline::encode_literal(l) {
                return Ok(id);
            }
        }

        // A triple term is identified by the ids of its parts, so nested terms intern
        // bottom-up and a deep RDF 1.2 term costs one row per distinct sub-term.
        let (tag, components) = match term {
            TermRef::NamedNode(_) => (Tag::Iri, None),
            TermRef::BlankNode(_) => (Tag::BlankNode, None),
            TermRef::Literal(_) => (Tag::Literal, None),
            TermRef::Triple(t) => {
                let ids = [
                    self.encode_into(t.subject.as_ref().into(), ops)?,
                    self.encode_into(t.predicate.as_ref().into(), ops)?,
                    self.encode_into(t.object.as_ref(), ops)?,
                ];
                (Tag::TripleTerm, Some(ids))
            }
        };

        // Serialised straight from the borrowed term: cloning it to an owned 
        // first cost three string allocations per quad on the hottest path there is.
        let serialised = put_term(term, components);
        let (dict_key, hashed) = Self::dictionary_key(&serialised);

        if let Some(state) = self.bulk.as_ref() {
            if let Some(existing) = state.terms.get(&serialised) {
                return Ok(*existing);
            }
        }
        if let Some(existing) = self.resolve(&dict_key, hashed, term)? {
            // Remember it: without this every *repeated* term costs a random read, which
            // dominates a bulk load — a term appearing a thousand times is looked up a
            // thousand times.
            if let Some(state) = self.bulk.as_mut() {
                state.terms.insert(serialised, existing);
            }
            return Ok(existing);
        }

        let id = self.allocate(tag, ops)?;
        if let Some(state) = self.bulk.as_mut() {
            state.terms.insert(serialised.clone(), id);
        }
        ops.push(Pending::Put(
            ID2STR,
            put_id(id).to_vec().into(),
            serialised.clone().into(),
        ));
        if hashed {
            // Append to the candidate list rather than replacing it, so a hash collision
            // between two genuinely different terms keeps both.
            let mut candidates = self
                .db
                .get_cf(cf(&self.db, STR2ID)?, &dict_key)
                .map_err(rocks_err)?
                .unwrap_or_default();
            candidates.extend_from_slice(&put_id(id));
            ops.push(Pending::Put(STR2ID, dict_key.into(), candidates.into()));
        } else {
            ops.push(Pending::Put(
                STR2ID,
                dict_key.into(),
                put_id(id).to_vec().into(),
            ));
        }
        Ok(id)
    }

    /// Resolves a dictionary key to an id.
    ///
    /// For a hashed key the stored value is a list of candidates, each of which is decoded
    /// and compared against the term itself. That is what keeps a hash collision from
    /// conflating two distinct RDF terms — the failure mode §5 exists to prevent.
    fn resolve(&self, dict_key: &[u8], hashed: bool, term: TermRef<'_>) -> Result<Option<TermId>> {
        let Some(value) = self
            .db
            .get_cf(cf(&self.db, STR2ID)?, dict_key)
            .map_err(rocks_err)?
        else {
            return Ok(None);
        };
        if !hashed {
            return Ok(Some(read_id(&value)?));
        }
        for chunk in value.chunks(codec::ID) {
            let candidate = read_id(chunk)?;
            if self.decode(candidate)?.as_ref().map(oxrdf::Term::as_ref) == Some(term) {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    fn scan_order(&self, order: &'static str, prefix: &[TermId]) -> Result<RowScan<'_>> {
        let lower = key(prefix);
        let upper = prefix_upper_bound(&lower);
        let mut opts = ReadOptions::default();
        if lower.is_empty() {
            // Without a bound prefix the iterator must not be constrained by the
            // column family's prefix extractor.
            opts.set_total_order_seek(true);
        } else {
            opts.set_iterate_lower_bound(lower.clone());
        }
        if let Some(upper) = upper {
            opts.set_iterate_upper_bound(upper);
        }
        Ok(RowScan {
            inner: self
                .db
                .iterator_cf_opt(cf(&self.db, order)?, opts, IteratorMode::Start),
        })
    }
}

/// Rows coming back from one column family.
struct RowScan<'a> {
    inner: rocksdb::DBIteratorWithThreadMode<'a, DB>,
}

impl Iterator for RowScan<'_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|row| row.map(|(k, _)| k.into_vec()).map_err(rocks_err))
    }
}

impl Storage for RocksStorage {
    fn encode(&mut self, term: TermRef<'_>) -> Result<TermId> {
        let mut ops = Vec::new();
        let id = self.encode_into(term, &mut ops)?;
        self.commit_ops(ops)?;
        Ok(id)
    }

    fn lookup(&self, term: TermRef<'_>) -> Result<Option<TermId>> {
        if let TermRef::NamedNode(n) = term {
            if let Some(id) = vocab::encode_iri(n.as_str()) {
                return Ok(Some(id));
            }
        }
        if let TermRef::Literal(l) = term {
            if let Some(id) = inline::encode_literal(l) {
                return Ok(Some(id));
            }
        }
        let components = match term {
            TermRef::Triple(t) => {
                let (Some(s), Some(p), Some(o)) = (
                    self.lookup(t.subject.as_ref().into())?,
                    self.lookup(t.predicate.as_ref().into())?,
                    self.lookup(t.object.as_ref())?,
                ) else {
                    return Ok(None);
                };
                Some([s, p, o])
            }
            _ => None,
        };
        let serialised = put_term(term, components);
        let (dict_key, hashed) = Self::dictionary_key(&serialised);
        self.resolve(&dict_key, hashed, term)
    }

    fn decode(&self, id: TermId) -> Result<Option<Term>> {
        if id.tag() == Tag::Vocab {
            return Ok(vocab::decode_iri(id).map(|s| oxrdf::NamedNode::new_unchecked(s).into()));
        }
        if id.tag().is_inline() {
            return Ok(inline::decode(id));
        }
        let Some(bytes) = self
            .db
            .get_cf(cf(&self.db, ID2STR)?, put_id(id))
            .map_err(rocks_err)?
        else {
            return Ok(None);
        };
        Ok(match codec::read_term(&bytes)? {
            StoredTerm::Complete(term) => Some(term),
            StoredTerm::Triple([s, p, o]) => {
                let subject = match self.decode(s)? {
                    Some(Term::NamedNode(n)) => n.into(),
                    Some(Term::BlankNode(b)) => b.into(),
                    Some(_) => {
                        return Err(StorageError::corruption(format!(
                            "triple term {id:?} has a non-node subject"
                        )))
                    }
                    None => return Ok(None),
                };
                let Some(Term::NamedNode(predicate)) = self.decode(p)? else {
                    return Err(StorageError::corruption(format!(
                        "triple term {id:?} has a non-IRI predicate"
                    )));
                };
                let Some(object) = self.decode(o)? else {
                    return Ok(None);
                };
                Some(Term::Triple(Box::new(oxrdf::Triple {
                    subject,
                    predicate,
                    object,
                })))
            }
        })
    }

    fn dictionary_len(&self) -> usize {
        usize::try_from(self.next.values().sum::<u64>()).unwrap_or(usize::MAX)
    }

    fn insert_encoded(&mut self, quad: EncodedQuad) -> Result<bool> {
        // During a bulk load the buffer is not visible to a `get`, so duplicate detection
        // is deferred: writing the same key twice is idempotent in RocksDB, and the
        // counters are rebuilt at the end.
        if self.bulk.is_none() && self.contains_encoded(quad)? {
            return Ok(false);
        }
        let EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name,
        } = quad;
        let mut ops = Vec::with_capacity(10);
        let empty = || -> Box<[u8]> { Box::from(&[][..]) };
        match graph_name {
            None => {
                ops.push(Pending::Put(DSPO, key(&[s, p, o]).into(), empty()));
                ops.push(Pending::Put(DPOS, key(&[p, o, s]).into(), empty()));
                ops.push(Pending::Put(DOSP, key(&[o, s, p]).into(), empty()));
            }
            Some(g) => {
                ops.push(Pending::Put(SPOG, key(&[s, p, o, g]).into(), empty()));
                ops.push(Pending::Put(POSG, key(&[p, o, s, g]).into(), empty()));
                ops.push(Pending::Put(OSPG, key(&[o, s, p, g]).into(), empty()));
                ops.push(Pending::Put(GSPO, key(&[g, s, p, o]).into(), empty()));
                ops.push(Pending::Put(GPOS, key(&[g, p, o, s]).into(), empty()));
                ops.push(Pending::Put(GOSP, key(&[g, o, s, p]).into(), empty()));
                ops.push(Pending::Put(GRAPHS, put_id(g).to_vec().into(), empty()));
            }
        }
        if self.bulk.is_none() {
            self.quad_count += 1;
            ops.push(Pending::Put(
                DEFAULT_CF,
                META_QUADS.to_vec().into(),
                self.quad_count.to_be_bytes().to_vec().into(),
            ));
            let count = self.predicate_counts.entry(p).or_insert(0);
            *count += 1;
            ops.push(Pending::Put(
                STATS,
                put_id(p).to_vec().into(),
                count.to_be_bytes().to_vec().into(),
            ));
        }
        self.commit_ops(ops)?;
        Ok(true)
    }

    fn remove_encoded(&mut self, quad: EncodedQuad) -> Result<bool> {
        if !self.contains_encoded(quad)? {
            return Ok(false);
        }
        let EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name,
        } = quad;
        let mut ops = Vec::with_capacity(9);
        match graph_name {
            None => {
                ops.push(Pending::Delete(DSPO, key(&[s, p, o]).into()));
                ops.push(Pending::Delete(DPOS, key(&[p, o, s]).into()));
                ops.push(Pending::Delete(DOSP, key(&[o, s, p]).into()));
            }
            Some(g) => {
                ops.push(Pending::Delete(SPOG, key(&[s, p, o, g]).into()));
                ops.push(Pending::Delete(POSG, key(&[p, o, s, g]).into()));
                ops.push(Pending::Delete(OSPG, key(&[o, s, p, g]).into()));
                ops.push(Pending::Delete(GSPO, key(&[g, s, p, o]).into()));
                ops.push(Pending::Delete(GPOS, key(&[g, p, o, s]).into()));
                ops.push(Pending::Delete(GOSP, key(&[g, o, s, p]).into()));
                // The graph itself survives, matching SPARQL Update: DELETE DATA does not
                // drop a graph just because it emptied it.
            }
        }
        self.quad_count -= 1;
        ops.push(Pending::Put(
            DEFAULT_CF,
            META_QUADS.to_vec().into(),
            self.quad_count.to_be_bytes().to_vec().into(),
        ));
        decrement(&mut self.predicate_counts, p, &mut ops);
        self.commit_ops(ops)?;
        Ok(true)
    }

    fn contains_encoded(&self, quad: EncodedQuad) -> Result<bool> {
        let (order, k) = match quad.graph_name {
            None => (DSPO, key(&[quad.subject, quad.predicate, quad.object])),
            Some(g) => (GSPO, key(&[g, quad.subject, quad.predicate, quad.object])),
        };
        Ok(self
            .db
            .get_pinned_cf(cf(&self.db, order)?, k)
            .map_err(rocks_err)?
            .is_some())
    }

    fn scan(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        match graph {
            GraphFilter::Any => Box::new(
                self.scan(subject, predicate, object, GraphFilter::Default)
                    .chain(self.scan(subject, predicate, object, GraphFilter::AnyNamed)),
            ),
            _ => match self.routed_scan(subject, predicate, object, graph) {
                Ok(iter) => iter,
                Err(e) => Box::new(std::iter::once(Err(e))),
            },
        }
    }

    fn len(&self) -> usize {
        usize::try_from(self.quad_count).unwrap_or(usize::MAX)
    }

    fn insert_named_graph(&mut self, graph: TermId) -> Result<bool> {
        if self.contains_named_graph(graph)? {
            return Ok(false);
        }
        self.put(GRAPHS, put_id(graph).to_vec(), Vec::new())?;
        Ok(true)
    }

    fn remove_named_graph(&mut self, graph: TermId) -> Result<bool> {
        let existed = self.contains_named_graph(graph)?;
        let doomed = self
            .scan(None, None, None, GraphFilter::Named(graph))
            .collect::<Result<Vec<_>>>()?;
        for quad in doomed {
            self.remove_encoded(quad)?;
        }
        if existed {
            self.delete(GRAPHS, put_id(graph).to_vec())?;
        }
        Ok(existed)
    }

    fn contains_named_graph(&self, graph: TermId) -> Result<bool> {
        Ok(self
            .db
            .get_pinned_cf(cf(&self.db, GRAPHS)?, put_id(graph))
            .map_err(rocks_err)?
            .is_some())
    }

    fn named_graphs(&self) -> Result<Vec<TermId>> {
        let mut out = Vec::new();
        for row in self
            .db
            .iterator_cf(cf(&self.db, GRAPHS)?, IteratorMode::Start)
        {
            let (k, _) = row.map_err(rocks_err)?;
            out.push(read_id(&k)?);
        }
        Ok(out)
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
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    fn begin_bulk_load(&mut self) {
        self.start_bulk();
    }

    fn end_bulk_load(&mut self) -> Result<()> {
        self.finish_bulk()?;
        self.db.flush().map_err(rocks_err)
    }

    fn checkpoint(&self, destination: &std::path::Path) -> Result<()> {
        checkpoint_to(self, destination)
    }

    fn flush(&mut self) -> Result<()> {
        if self.bulk.is_some() {
            self.finish_bulk()?;
        }
        self.db.flush().map_err(rocks_err)?;
        for name in QUAD_ORDERS.iter().chain(TRIPLE_ORDERS.iter()) {
            self.db.flush_cf(cf(&self.db, name)?).map_err(rocks_err)?;
        }
        for name in [ID2STR, STR2ID, GRAPHS, STATS] {
            self.db.flush_cf(cf(&self.db, name)?).map_err(rocks_err)?;
        }
        Ok(())
    }
}

impl RocksStorage {
    /// Routes a pattern to the one order whose prefix it binds — the same decision
    /// [`QuadIndex::plan`](crate::QuadIndex::plan) makes for the in-memory tier.
    fn routed_scan(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphFilter,
    ) -> Result<QuadScan<'_>> {
        Ok(match graph {
            GraphFilter::Default => {
                let (order, prefix, un): (_, Vec<TermId>, fn([TermId; 3]) -> EncodedQuad) =
                    match (s, p, o) {
                        (Some(s), Some(p), Some(o)) => (DSPO, vec![s, p, o], un_dspo),
                        (Some(s), Some(p), None) => (DSPO, vec![s, p], un_dspo),
                        (Some(s), None, Some(o)) => (DOSP, vec![o, s], un_dosp),
                        (Some(s), None, None) => (DSPO, vec![s], un_dspo),
                        (None, Some(p), Some(o)) => (DPOS, vec![p, o], un_dpos),
                        (None, Some(p), None) => (DPOS, vec![p], un_dpos),
                        (None, None, Some(o)) => (DOSP, vec![o], un_dosp),
                        (None, None, None) => (DSPO, vec![], un_dspo),
                    };
                Box::new(
                    self.scan_order(order, &prefix)?
                        .map(move |k| Ok(un(split_key::<3>(&k?)?))),
                )
            }
            GraphFilter::Named(g) => {
                let (order, prefix, un): (_, Vec<TermId>, fn([TermId; 4]) -> EncodedQuad) =
                    match (s, p, o) {
                        (Some(s), Some(p), Some(o)) => (GSPO, vec![g, s, p, o], un_gspo),
                        (Some(s), Some(p), None) => (GSPO, vec![g, s, p], un_gspo),
                        (Some(s), None, Some(o)) => (GOSP, vec![g, o, s], un_gosp),
                        (Some(s), None, None) => (GSPO, vec![g, s], un_gspo),
                        (None, Some(p), Some(o)) => (GPOS, vec![g, p, o], un_gpos),
                        (None, Some(p), None) => (GPOS, vec![g, p], un_gpos),
                        (None, None, Some(o)) => (GOSP, vec![g, o], un_gosp),
                        (None, None, None) => (GSPO, vec![g], un_gspo),
                    };
                Box::new(
                    self.scan_order(order, &prefix)?
                        .map(move |k| Ok(un(split_key::<4>(&k?)?))),
                )
            }
            GraphFilter::AnyNamed => {
                let (order, prefix, un): (_, Vec<TermId>, fn([TermId; 4]) -> EncodedQuad) =
                    match (s, p, o) {
                        (Some(s), Some(p), Some(o)) => (SPOG, vec![s, p, o], un_spog),
                        (Some(s), Some(p), None) => (SPOG, vec![s, p], un_spog),
                        (Some(s), None, Some(o)) => (OSPG, vec![o, s], un_ospg),
                        (Some(s), None, None) => (SPOG, vec![s], un_spog),
                        (None, Some(p), Some(o)) => (POSG, vec![p, o], un_posg),
                        (None, Some(p), None) => (POSG, vec![p], un_posg),
                        (None, None, Some(o)) => (OSPG, vec![o], un_ospg),
                        (None, None, None) => (SPOG, vec![], un_spog),
                    };
                Box::new(
                    self.scan_order(order, &prefix)?
                        .map(move |k| Ok(un(split_key::<4>(&k?)?))),
                )
            }
            GraphFilter::Any => unreachable!("handled by the caller"),
        })
    }
}

// --- key un-permutation --------------------------------------------------------------

fn triple(s: TermId, p: TermId, o: TermId) -> EncodedQuad {
    EncodedQuad {
        subject: s,
        predicate: p,
        object: o,
        graph_name: None,
    }
}

fn quad(s: TermId, p: TermId, o: TermId, g: TermId) -> EncodedQuad {
    EncodedQuad {
        subject: s,
        predicate: p,
        object: o,
        graph_name: Some(g),
    }
}

fn un_dspo([s, p, o]: [TermId; 3]) -> EncodedQuad {
    triple(s, p, o)
}
fn un_dpos([p, o, s]: [TermId; 3]) -> EncodedQuad {
    triple(s, p, o)
}
fn un_dosp([o, s, p]: [TermId; 3]) -> EncodedQuad {
    triple(s, p, o)
}
fn un_spog([s, p, o, g]: [TermId; 4]) -> EncodedQuad {
    quad(s, p, o, g)
}
fn un_posg([p, o, s, g]: [TermId; 4]) -> EncodedQuad {
    quad(s, p, o, g)
}
fn un_ospg([o, s, p, g]: [TermId; 4]) -> EncodedQuad {
    quad(s, p, o, g)
}
fn un_gspo([g, s, p, o]: [TermId; 4]) -> EncodedQuad {
    quad(s, p, o, g)
}
fn un_gpos([g, p, o, s]: [TermId; 4]) -> EncodedQuad {
    quad(s, p, o, g)
}
fn un_gosp([g, o, s, p]: [TermId; 4]) -> EncodedQuad {
    quad(s, p, o, g)
}

// --- helpers ---------------------------------------------------------------------------

/// Takes a hard-linked consistent snapshot of an open store.
///
/// RocksDB's own checkpoint: it flushes the write-ahead log and then hard-links the SST
/// files into `destination`, so the copy is consistent, near-instant, and initially costs
/// almost no disk. Two consequences worth knowing:
///
/// * **Hard links need the same filesystem.** Checkpointing to another mount makes RocksDB
///   copy the files instead — still correct, no longer instant.
/// * **A checkpoint pins the SST files it links.** They cannot be deleted while it exists,
///   so as compaction proceeds the snapshot and the live store diverge and disk use climbs.
///   A checkpoint left lying around is a slow disk leak; retention is the caller's job.
///
/// # Errors
///
/// Refuses while a bulk load is running, because those writes are buffered in this process
/// rather than in RocksDB: a checkpoint taken then would be internally consistent and
/// missing data, which is worse than a failure. Otherwise propagates RocksDB's own error —
/// including the refusal to write into a directory that already exists.
fn checkpoint_to(storage: &RocksStorage, destination: &std::path::Path) -> Result<()> {
    if storage.bulk.is_some() {
        return Err(StorageError::Unsupported(
            "a bulk load is in progress; its writes are buffered outside RocksDB, so a \
             checkpoint taken now would be consistent and incomplete"
                .to_owned(),
        ));
    }
    let checkpoint = rocksdb::checkpoint::Checkpoint::new(&storage.db).map_err(rocks_err)?;
    checkpoint.create_checkpoint(destination).map_err(rocks_err)
}

fn cf<'a>(db: &'a DB, name: &str) -> Result<&'a rocksdb::ColumnFamily> {
    db.cf_handle(name)
        .ok_or_else(|| StorageError::corruption(format!("column family {name} is missing")))
}

fn rocks_err(e: rocksdb::Error) -> StorageError {
    StorageError::Io(std::io::Error::other(e.to_string()))
}

fn read_u64(db: &DB, key: &[u8]) -> Result<Option<u64>> {
    Ok(match db.get(key).map_err(rocks_err)? {
        Some(v) => Some(u64::from_be_bytes(to_8(&v)?)),
        None => None,
    })
}

fn to_8(bytes: &[u8]) -> Result<[u8; 8]> {
    bytes
        .try_into()
        .map_err(|_| StorageError::corruption("expected an 8-byte value"))
}

fn decrement(counts: &mut FxHashMap<TermId, u64>, predicate: TermId, ops: &mut Vec<Pending>) {
    if let Some(n) = counts.get_mut(&predicate) {
        *n -= 1;
        if *n == 0 {
            counts.remove(&predicate);
            ops.push(Pending::Delete(STATS, put_id(predicate).to_vec().into()));
        } else {
            ops.push(Pending::Put(
                STATS,
                put_id(predicate).to_vec().into(),
                n.to_be_bytes().to_vec().into(),
            ));
        }
    }
}

/// A 128-bit digest, used only to bound dictionary key length. Collisions are resolved by
/// verifying candidates, so this needs to be fast and well-spread, not cryptographic.
fn hash128(bytes: &[u8]) -> [u8; 16] {
    let mut a = DefaultHasher::new();
    bytes.hash(&mut a);
    let mut b = DefaultHasher::new();
    0xA5A5_A5A5_u32.hash(&mut b);
    bytes.hash(&mut b);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.finish().to_be_bytes());
    out[8..].copy_from_slice(&b.finish().to_be_bytes());
    out
}

fn bulk_write_opts() -> WriteOptions {
    let mut opts = WriteOptions::default();
    // A load that is interrupted is discarded and restarted, so the log buys nothing.
    opts.disable_wal(true);
    opts
}

fn value_opts() -> Options {
    let mut opts = Options::default();
    opts.set_compression_type(DBCompressionType::Lz4);
    opts
}

/// Index column families get a fixed-length prefix extractor so RocksDB can build a
/// prefix bloom filter over the first term of a key — the common case for a bound
/// subject, predicate or graph (`DESIGN.md` §6.1). Scans with no bound prefix opt out
/// through `total_order_seek`.
fn index_opts(prefix_len: usize) -> Options {
    let mut opts = value_opts();
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(prefix_len));
    let mut block = rocksdb::BlockBasedOptions::default();
    block.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&block);
    opts
}

fn num_cpus() -> i32 {
    i32::try_from(
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(2),
    )
    .unwrap_or(2)
}
