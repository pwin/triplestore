//! `DISTINCT` and `ORDER BY` in bounded memory: sort, spill, merge.
//!
//! # Why this exists
//!
//! `spareval` evaluates `DISTINCT` by holding every distinct solution in a hash set. That is
//! the fastest thing to do when the answer fits and the only thing to do when it does not —
//! and on a large store it does not. A real deployment aborted on
//! `SELECT (count(distinct *) as ?count) WHERE { ?sub ?pred ?obj }`, where the set had to
//! hold every triple in the store.
//!
//! [`crate::memory`] stops that becoming a dead process and [`crate::admit`] declines it
//! before it starts, but neither lets the query *finish*. This does, by using the technique
//! every database uses for the same problem and that this codebase already uses twice —
//! `sort.rs` for index orders and `dictsort.rs` for the dictionary. Fill a buffer, sort it,
//! write it out, and merge the runs at the end. Memory tracks the buffer, not the answer.
//!
//! # Hashing while it fits, sorting when it does not
//!
//! A hash set answers "have I seen this?" in constant time and cannot answer it at all once
//! it outgrows memory. Sorting turns the question into "is this the same as the row before
//! it?", which needs no memory beyond the row — so duplicates can be dropped during a merge
//! of runs that were never all resident.
//!
//! Sorting *everything* was the first version of this, and it was measured at **15× slower**
//! than the hash set on a three-million-row `DISTINCT` that fitted in memory: 42.7 s against
//! 2.8 s. The cost was not the disk. It was encoding nine million terms to strings for the
//! sort, on a query that never needed a byte of it written.
//!
//! So rows are held in a hash set of `Term`s, with no encoding at all, until that set
//! outgrows its budget. Only then is it encoded, sorted and written as a run, and only a
//! query that overflows pays for any of it. A `DISTINCT` that fits now costs what it always
//! did, and one that does not, finishes.
//!
//! # What that costs in output order
//!
//! A query that never spills comes back in **hash order**; one that spills comes back
//! **sorted**. Neither is arrival order, and SPARQL promises no order for `DISTINCT`, so
//! both are conformant — but they are different from each other, and from what `spareval`
//! would have produced. A query that cares says `ORDER BY`, which re-sorts anyway.
//!
//! # Comparing rows as bytes
//!
//! Each row is encoded as its terms' N-Triples forms, length-prefixed, with a marker for
//! unbound. Two rows are equal exactly when their encodings are, because that serialisation
//! is injective over terms: `"1"^^xsd:integer` and `"01"^^xsd:integer` are different RDF
//! terms, they are different strings, and `DISTINCT` must keep both. Comparing bytes is
//! therefore comparing terms, and it is what lets a run be a file rather than a structure.

use oxrdf::{Term, Variable};
use rustc_hash::FxHashSet;
use spareval::{ExpressionTerm, QueryEvaluationError, QuerySolution, QuerySolutionIter};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// Bytes of rows held before a run is written out.
///
/// The whole point is that this, and not the size of the answer, is what a `DISTINCT` or an
/// `ORDER BY` costs. 128 MiB is large enough that ordinary queries never touch the disk and
/// small enough to leave room for everything else the query is doing.
pub const SPILL_BYTES: usize = 128 << 20;

/// Bytes a single operator may write to its scratch directory before it is stopped.
///
/// # Why a disk ceiling as well as a memory budget
///
/// [`SPILL_BYTES`] bounds what an operator *holds*; it says nothing about what it *writes*,
/// and a spilling operator writes in proportion to its input. Before 0.14.0 that was a
/// documented hazard reachable only by a `DISTINCT` under `--reorder`; when `ORDER BY`
/// started spilling it became reachable by any sort, on by default, and the failure it
/// replaced was a clean one — the memory ceiling refusing the query in seconds. Trading a
/// refusal for a full system volume is not an improvement, and the scratch directory is
/// very often *on* the system volume (see [`scratch_dir`]).
///
/// So the disk is bounded too, and going past it fails the query the way the memory ceiling
/// does: with the numbers and the flag that changes them. 16 GiB is far above any sort this
/// store is for — the 48.4-million-row sort it was sized against writes about 7 GB — and far
/// below what would fill a volume anyone would notice.
pub const SPILL_DISK_BYTES: usize = 16 << 30;

/// A query stopped for writing more scratch than it was allowed.
///
/// Carried out of the collector inside a `std::io::Error`, because that is what the write
/// path returns, and unwrapped again by whoever turns it into a query error so that the
/// client is told which ceiling rather than being handed an I/O failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskLimitExceeded {
    /// Bytes the operator had written when it was stopped.
    pub written: usize,
    /// The ceiling it passed.
    pub limit: usize,
}

impl std::fmt::Display for DiskLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "query cancelled: it spilled {} to scratch, over the {} ceiling \
             (raise it or remove it with --max-spill-disk, and see OPERATIONS.md for where \
             the scratch goes)",
            crate::memory::human(self.written),
            crate::memory::human(self.limit)
        )
    }
}

impl std::error::Error for DiskLimitExceeded {}

/// Turns the collector's `std::io::Error` into a query error, keeping a ceiling breach
/// recognisable instead of burying it as an I/O failure.
fn spill_error(e: std::io::Error) -> QueryEvaluationError {
    match e.downcast::<DiskLimitExceeded>() {
        Ok(over) => QueryEvaluationError::Dataset(Box::new(over)),
        Err(other) => QueryEvaluationError::Dataset(Box::new(other)),
    }
}

/// One encoded row, as written to a run file.
type Row = Vec<u8>;

/// One row as held in memory: the terms themselves, never serialised unless they spill.
type Live = Vec<Option<Term>>;

/// A directory of this collector's own, under the platform's temporary directory.
///
/// A counter as well as the process id: two collectors can be alive in one process — a
/// nested subquery, or a second query on another thread — and sharing a directory would
/// mean sharing run *filenames*, so one would silently read the other's rows.
///
/// `temp_dir`, which means `TMPDIR` on Unix and `TMP`/`TEMP` on Windows, is very often the
/// *system* volume rather than the one the store lives on. That is a sharper edge than it
/// looks: a `DISTINCT` over a large store spills in proportion to its answer, not to any
/// buffer, so it will happily write tens of gigabytes. Measured on a 653.8-million-triple
/// store, one reached 5.5 GB in twenty-two minutes and was heading for something near
/// seventy, against 29 GB free on that machine's `C:`.
///
/// The bulk loader had the same choice and made the other one: `ingest_dir` puts its scratch
/// beside the database, so it lands on the volume an operator sized for the store. This
/// cannot do that — a query is not attached to one store's directory, and an in-memory store
/// has no directory at all — so the placement stays the platform's and the operator's,
/// through the environment. OPERATIONS.md says to set it.
fn scratch_dir(prefix: &str) -> std::io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Collects rows, spilling sorted runs when the in-memory set outgrows its budget.
pub struct Distinct {
    dir: PathBuf,
    runs: Vec<PathBuf>,
    /// Bytes written to the runs so far, against [`Distinct::disk_limit`].
    written: usize,
    /// Bytes this collector may write before it gives up. Zero removes the ceiling.
    disk_limit: usize,
    /// Deduplicating as rows arrive, which is what makes the common case cost what
    /// `spareval` costs. Encoding happens only when this has to be written out.
    live: FxHashSet<Live>,
    bytes: usize,
    budget: usize,
    width: usize,
}

impl Distinct {
    /// A collector for rows of `width` variables, spilling past `budget` bytes.
    ///
    /// # Errors
    ///
    /// If the scratch directory cannot be made.
    pub fn new(width: usize, budget: usize) -> std::io::Result<Self> {
        // A counter, not just the thread id: two collectors can be alive on one thread —
        // a nested subquery, or a second query on a pooled thread — and sharing a directory
        // would mean sharing run *filenames*, so one would silently read the other's rows.
        let dir = scratch_dir("holos-distinct")?;
        Ok(Self {
            dir,
            runs: Vec::new(),
            written: 0,
            disk_limit: SPILL_DISK_BYTES,
            live: FxHashSet::default(),
            bytes: 0,
            budget,
            width,
        })
    }

    /// How many runs were written. Zero means the whole query fitted in memory.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// Replaces the scratch ceiling. Zero removes it. See [`SPILL_DISK_BYTES`].
    #[must_use]
    pub fn with_disk_limit(mut self, bytes: usize) -> Self {
        self.disk_limit = bytes;
        self
    }

    /// Where this collector's runs live, for an operator that wants to report it and for a
    /// test that wants to check they were cleaned up.
    #[must_use]
    pub fn scratch(&self) -> &std::path::Path {
        &self.dir
    }

    /// Adds a solution, deduplicating it against what is already held.
    ///
    /// # Errors
    ///
    /// If a run cannot be written.
    pub fn push(&mut self, solution: &QuerySolution) -> std::io::Result<()> {
        let row: Live = (0..self.width).map(|i| solution.get(i).cloned()).collect();
        let size = footprint(&row);
        if self.live.insert(row) {
            self.bytes += size;
        }
        if self.bytes >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    /// Encodes what is held, sorts it, and writes it out as a run.
    ///
    /// The rows are already distinct — the set saw to that — so this only has to sort. The
    /// merge still compares across runs, because a row dropped from one run may appear in
    /// another.
    fn spill(&mut self) -> std::io::Result<()> {
        if self.live.is_empty() {
            return Ok(());
        }
        let mut rows = self.encoded();

        let path = self.dir.join(format!("{}.run", self.runs.len()));
        let mut out = BufWriter::new(File::create(&path)?);
        for row in &rows {
            self.written += write_row(&mut out, row)?;
        }
        out.flush()?;
        rows.clear();

        self.runs.push(path);
        self.bytes = 0;
        over_disk_limit(self.written, self.disk_limit)
    }

    /// What is held, encoded and sorted, leaving the set empty.
    fn encoded(&mut self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .live
            .drain()
            .map(|row| encode_row(&row, self.width))
            .collect();
        rows.sort_unstable();
        rows
    }

    /// Every distinct row.
    ///
    /// When nothing spilled this hands back the set as it stands — no encoding, no sorting,
    /// no files — which is the case the hybrid exists to keep cheap.
    ///
    /// # Errors
    ///
    /// If a run cannot be read back.
    pub fn merge(mut self) -> std::io::Result<Merged> {
        if self.runs.is_empty() {
            let live: Vec<Live> = self.live.drain().collect();
            return Ok(Merged::InMemory {
                rows: live.into_iter(),
                _owner: self,
            });
        }
        let buffer = self.encoded();

        let mut sources = Vec::with_capacity(self.runs.len());
        for path in &self.runs {
            sources.push(RunReader::open(path)?);
        }
        let mut heap = BinaryHeap::with_capacity(sources.len() + 1);
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(row) = source.next()? {
                heap.push(Reverse((row, index)));
            }
        }
        let tail = sources.len();
        if let Some(row) = buffer.first() {
            heap.push(Reverse((row.clone(), tail)));
        }
        Ok(Merged::Spilled {
            _owner: self,
            buffer,
            sources,
            heap,
            tail,
            tail_at: 0,
            last: None,
        })
    }
}

/// A row's cost in memory, near enough to bound a buffer by.
///
/// Counted from the terms' own lengths rather than by serialising them, because serialising
/// is the thing this design exists to avoid on the common path.
fn footprint(row: &Live) -> usize {
    row.iter()
        .map(|slot| match slot {
            None => 8,
            Some(Term::NamedNode(n)) => n.as_str().len() + 24,
            Some(Term::BlankNode(b)) => b.as_str().len() + 24,
            Some(Term::Literal(l)) => l.value().len() + 64,
            Some(_) => 64,
        })
        .sum::<usize>()
        + std::mem::size_of::<Live>()
}

impl Drop for Distinct {
    fn drop(&mut self) {
        for path in &self.runs {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The distinct rows, however they ended up being held.
pub enum Merged {
    /// Nothing spilled: the set is the answer, and never touched a disk or a serialiser.
    InMemory {
        /// The set, drained. Still `Live` terms: nothing was ever encoded.
        rows: std::vec::IntoIter<Live>,
        /// Held only so the (empty) scratch directory is cleaned up on drop.
        _owner: Distinct,
    },
    /// Runs on disk, merged with whatever was still held.
    Spilled {
        /// Held so the run files outlive the readers and are removed after them.
        _owner: Distinct,
        /// What the set still held when the last run was written, sorted. One more source.
        buffer: Vec<Row>,
        /// A reader per run on disk, each holding one block.
        sources: Vec<RunReader>,
        /// The merge frontier: the next row from every source, smallest first.
        heap: BinaryHeap<Reverse<(Row, usize)>>,
        /// The index standing for `buffer` in the heap, one past the last run.
        tail: usize,
        /// How far `buffer` has been consumed.
        tail_at: usize,
        /// The row just emitted, so its duplicates in other runs can be skipped.
        last: Option<Row>,
    },
}

impl Merged {
    /// The next distinct row.
    ///
    /// # Errors
    ///
    /// If a run cannot be read back.
    pub fn next_row(&mut self) -> std::io::Result<Option<Vec<Option<Term>>>> {
        let width = match self {
            Self::InMemory { rows, .. } => return Ok(rows.next()),
            Self::Spilled { _owner, .. } => _owner.width,
        };
        loop {
            let Self::Spilled {
                heap, last, buffer, ..
            } = self
            else {
                unreachable!("checked above")
            };
            let Some(Reverse((row, source))) = heap.pop() else {
                return Ok(None);
            };
            let repeat = last.as_ref() == Some(&row);
            let _ = buffer;
            self.advance(source)?;
            if repeat {
                // The same row from another run. Dropping it here is the whole reason the
                // stream is sorted.
                continue;
            }
            let decoded = decode(&row, width);
            if let Self::Spilled { last, .. } = self {
                *last = Some(row);
            }
            return Ok(Some(decoded));
        }
    }

    fn advance(&mut self, source: usize) -> std::io::Result<()> {
        let Self::Spilled {
            buffer,
            sources,
            heap,
            tail,
            tail_at,
            ..
        } = self
        else {
            return Ok(());
        };
        if source == *tail {
            *tail_at += 1;
            if let Some(row) = buffer.get(*tail_at) {
                heap.push(Reverse((row.clone(), *tail)));
            }
        } else if let Some(row) = sources[source].next()? {
            heap.push(Reverse((row, source)));
        }
        Ok(())
    }
}

/// Applies a bounded-memory `DISTINCT` to a stream of solutions.
///
/// Consumes the input eagerly — it has to, since the last row read may be the duplicate of
/// the first — but never holds more than `budget` bytes of it at once.
///
/// # Errors
///
/// Propagates evaluation errors from `input`, and any failure to write or read a run.
pub fn deduplicate(
    input: QuerySolutionIter<'_>,
    budget: usize,
    disk_limit: Option<usize>,
) -> Result<QuerySolutionIter<'static>, QueryEvaluationError> {
    let variables: Arc<[Variable]> = Arc::from(input.variables().to_vec());
    let width = variables.len();

    let mut distinct = Distinct::new(width, budget).map_err(spill_error)?;
    if let Some(limit) = disk_limit {
        distinct = distinct.with_disk_limit(limit);
    }
    for solution in input {
        distinct.push(&solution?).map_err(spill_error)?;
    }

    let mut merged = distinct.merge().map_err(spill_error)?;
    let rows = std::iter::from_fn(move || match merged.next_row() {
        Ok(Some(row)) => Some(Ok(row)),
        Ok(None) => None,
        Err(e) => Some(Err(spill_error(e))),
    });
    Ok(QuerySolutionIter::from_tuples(variables, rows))
}

// ---------------------------------------------------------------------------------
// ORDER BY
// ---------------------------------------------------------------------------------

/// Collects rows with their sort keys, spilling sorted runs when the buffer outgrows its
/// budget, and merges them into one ordered stream.
///
/// # Why a second collector rather than the one above
///
/// [`Distinct`] answers "have I seen this row?", and any total order over encoded rows will
/// do for that — which is why it compares raw bytes. A sort has to come back in *SPARQL's*
/// order, and that order is over decoded terms: `"9"^^xsd:integer` sorts before
/// `"10"^^xsd:integer` and after them as strings. So this holds each row's sort keys as
/// terms beside it and compares with [`crate::topk::cmp_keys`] — the same comparison the
/// heap in [`crate::topk`] uses, so a query answered by either comes back the same way.
///
/// The cost of that choice is one decode per row per run, when a run is read back; the
/// alternative, an order-preserving byte encoding of every SPARQL datatype, is a great deal
/// of code to write and to be wrong in, and would still have to fall back to the terms for
/// the datatypes it did not cover.
///
/// # Stability
///
/// Runs are sorted with a **stable** sort and merged preferring the earlier run, and the
/// buffer that never reached a run sorts last because it arrived last. So rows whose keys
/// do not separate them come back in the order they arrived, whether or not anything
/// spilled — which is what lets [`crate::topk`]'s heap and this agree on a query that ties.
pub struct Sorted {
    dir: PathBuf,
    runs: Vec<PathBuf>,
    /// Bytes written to the runs so far, against [`Sorted::disk_limit`].
    written: usize,
    /// Bytes this collector may write before it gives up. Zero removes the ceiling.
    disk_limit: usize,
    /// What is held: each row with the keys it is sorted by.
    live: Vec<(Keys, Live)>,
    bytes: usize,
    budget: usize,
    key_width: usize,
    row_width: usize,
    descending: Arc<[bool]>,
}

/// One row's sort keys, in the order the `ORDER BY` names them.
type Keys = Vec<Option<ExpressionTerm>>;

impl Sorted {
    /// A collector for rows of `row_width` variables sorted by `descending.len()` keys,
    /// spilling past `budget` bytes.
    ///
    /// # Errors
    ///
    /// If the scratch directory cannot be made.
    pub fn new(
        row_width: usize,
        descending: Arc<[bool]>,
        budget: usize,
    ) -> std::io::Result<Self> {
        let dir = scratch_dir("holos-sort")?;
        Ok(Self {
            dir,
            runs: Vec::new(),
            written: 0,
            disk_limit: SPILL_DISK_BYTES,
            live: Vec::new(),
            bytes: 0,
            budget,
            key_width: descending.len(),
            row_width,
            descending,
        })
    }

    /// How many runs were written. Zero means the whole sort fitted in memory.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// Replaces the scratch ceiling. Zero removes it. See [`SPILL_DISK_BYTES`].
    #[must_use]
    pub fn with_disk_limit(mut self, bytes: usize) -> Self {
        self.disk_limit = bytes;
        self
    }

    /// Where this collector's runs live, for an operator that wants to report it and for a
    /// test that wants to check they were cleaned up.
    #[must_use]
    pub fn scratch(&self) -> &std::path::Path {
        &self.dir
    }

    /// Adds a row and the keys it sorts by.
    ///
    /// # Errors
    ///
    /// If a run cannot be written.
    pub fn push(&mut self, keys: Keys, row: Live) -> std::io::Result<()> {
        self.bytes += footprint(&row) + keys_footprint(&keys);
        self.live.push((keys, row));
        if self.bytes >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    /// Sorts what is held and writes it out as a run.
    fn spill(&mut self) -> std::io::Result<()> {
        if self.live.is_empty() {
            return Ok(());
        }
        self.sort_live();
        let path = self.dir.join(format!("{}.sortrun", self.runs.len()));
        let mut out = BufWriter::new(File::create(&path)?);
        for (keys, row) in self.live.drain(..) {
            let mut record = encode_row(&key_terms(&keys), self.key_width);
            record.extend_from_slice(&encode_row(&row, self.row_width));
            self.written += write_row(&mut out, &record)?;
        }
        out.flush()?;
        self.runs.push(path);
        self.bytes = 0;
        over_disk_limit(self.written, self.disk_limit)
    }

    /// Sorts what is held, in place. Stable, so equal keys keep arrival order — see the
    /// note on stability above, and `sort_unstable_by` would additionally be entitled to
    /// panic on the intransitivity [`crate::topk::cmp_keys`] documents.
    fn sort_live(&mut self) {
        let descending = Arc::clone(&self.descending);
        self.live
            .sort_by(|(a, _), (b, _)| crate::topk::cmp_keys(a, b, &descending));
    }

    /// Every row, in order.
    ///
    /// When nothing spilled this is the sorted buffer — no encoding, no files — which is
    /// the case that has to cost what an in-memory sort costs.
    ///
    /// # Errors
    ///
    /// If a run cannot be read back.
    pub fn merge(mut self) -> std::io::Result<SortedRows> {
        self.sort_live();
        if self.runs.is_empty() {
            let rows: Vec<Live> = std::mem::take(&mut self.live)
                .into_iter()
                .map(|(_, row)| row)
                .collect();
            return Ok(SortedRows(Rows::InMemory {
                rows: rows.into_iter(),
                _owner: self,
            }));
        }

        let (key_width, row_width) = (self.key_width, self.row_width);
        let descending = Arc::clone(&self.descending);
        let buffer: Vec<(Keys, Live)> = std::mem::take(&mut self.live);
        let mut sources = Vec::with_capacity(self.runs.len());
        for path in &self.runs {
            sources.push(RunReader::open(path)?);
        }
        // One past the last run: the buffer arrived after every run, so ranking it last
        // among equal keys is what keeps the whole merge stable.
        let tail = sources.len();

        let mut heap = BinaryHeap::with_capacity(tail + 1);
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(record) = source.next()? {
                let (keys, row) = split_record(&record, key_width, row_width);
                heap.push(Reverse(Head {
                    keys,
                    row,
                    source: index,
                    descending: Arc::clone(&descending),
                }));
            }
        }
        if let Some((keys, row)) = buffer.first() {
            heap.push(Reverse(Head {
                keys: keys.clone(),
                row: row.clone(),
                source: tail,
                descending: Arc::clone(&descending),
            }));
        }

        Ok(SortedRows(Rows::Spilled {
            buffer,
            sources,
            heap,
            tail,
            tail_at: 0,
            key_width,
            row_width,
            descending,
            _owner: self,
        }))
    }
}

impl Drop for Sorted {
    fn drop(&mut self) {
        for path in &self.runs {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The next row of one merge source, and where it came from.
struct Head {
    keys: Keys,
    row: Live,
    /// Which run, or the index standing for the buffer.
    source: usize,
    descending: Arc<[bool]>,
}

impl Ord for Head {
    /// The sort's own order, with the source breaking a tie. Earlier runs hold earlier
    /// rows, so preferring the lower index is what makes the merge stable.
    fn cmp(&self, other: &Self) -> Ordering {
        crate::topk::cmp_keys(&self.keys, &other.keys, &self.descending)
            .then(self.source.cmp(&other.source))
    }
}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for Head {}

/// The sorted rows, whatever it took to produce them.
///
/// Opaque: whether anything spilled changes how the next row is found and nothing else, so
/// it is not the caller's business.
pub struct SortedRows(Rows);

/// However the rows ended up being held.
enum Rows {
    /// Nothing spilled: the buffer, sorted, still holding its terms.
    InMemory {
        rows: std::vec::IntoIter<Live>,
        /// Held only so the (empty) scratch directory is cleaned up on drop.
        _owner: Sorted,
    },
    /// Runs on disk, merged with whatever was still held.
    Spilled {
        /// What was still held when the last run was written, sorted. One more source.
        buffer: Vec<(Keys, Live)>,
        /// A reader per run on disk, each holding one block.
        sources: Vec<RunReader>,
        /// The merge frontier: the next row from every source, smallest first.
        heap: BinaryHeap<Reverse<Head>>,
        /// The index standing for `buffer` in the heap, one past the last run.
        tail: usize,
        /// How far `buffer` has been consumed.
        tail_at: usize,
        key_width: usize,
        row_width: usize,
        descending: Arc<[bool]>,
        /// Held so the run files are removed when the merge is done with them. Last, so
        /// that the readers above are closed before it runs.
        _owner: Sorted,
    },
}

impl SortedRows {
    /// The next row, in order.
    ///
    /// # Errors
    ///
    /// If a run cannot be read back.
    pub fn next_row(&mut self) -> std::io::Result<Option<Live>> {
        match &mut self.0 {
            Rows::InMemory { rows, .. } => Ok(rows.next()),
            Rows::Spilled {
                buffer,
                sources,
                heap,
                tail,
                tail_at,
                key_width,
                row_width,
                descending,
                ..
            } => {
                let Some(Reverse(head)) = heap.pop() else {
                    return Ok(None);
                };
                if head.source == *tail {
                    *tail_at += 1;
                    if let Some((keys, row)) = buffer.get(*tail_at) {
                        heap.push(Reverse(Head {
                            keys: keys.clone(),
                            row: row.clone(),
                            source: *tail,
                            descending: Arc::clone(descending),
                        }));
                    }
                } else if let Some(record) = sources[head.source].next()? {
                    let (keys, row) = split_record(&record, *key_width, *row_width);
                    heap.push(Reverse(Head {
                        keys,
                        row,
                        source: head.source,
                        descending: Arc::clone(descending),
                    }));
                }
                Ok(Some(head.row))
            }
        }
    }
}

/// The keys and the row a record holds, written one after the other by [`Sorted::spill`].
fn split_record(record: &[u8], key_width: usize, row_width: usize) -> (Keys, Live) {
    let (keys, at) = decode_at(record, key_width, 0);
    let (row, _) = decode_at(record, row_width, at);
    (
        keys.into_iter()
            .map(|term| term.map(ExpressionTerm::from))
            .collect(),
        row,
    )
}

/// Sort keys as the terms they came from, for encoding.
fn key_terms(keys: &[Option<ExpressionTerm>]) -> Live {
    keys.iter()
        .map(|key| key.clone().map(Term::from))
        .collect()
}

/// What a row's keys cost in memory, near enough to bound a buffer by.
fn keys_footprint(keys: &[Option<ExpressionTerm>]) -> usize {
    keys.iter()
        .map(|key| match key {
            None => 8,
            Some(ExpressionTerm::NamedNode(n)) => n.as_str().len() + 24,
            Some(ExpressionTerm::BlankNode(b)) => b.as_str().len() + 24,
            Some(ExpressionTerm::StringLiteral(v)) => v.len() + 32,
            Some(ExpressionTerm::LangStringLiteral { value, .. }) => value.len() + 48,
            Some(ExpressionTerm::OtherTypedLiteral { value, datatype }) => {
                value.len() + datatype.as_str().len() + 48
            }
            Some(_) => 48,
        })
        .sum::<usize>()
        + std::mem::size_of::<Keys>()
}

// ---------------------------------------------------------------------------------
// encoding
// ---------------------------------------------------------------------------------

/// A row as bytes: one slot per variable, unbound marked, bound length-prefixed.
fn encode_row(row: &Live, width: usize) -> Row {
    let mut out = Vec::with_capacity(width * 16);
    for slot in 0..width {
        match row.get(slot).and_then(Option::as_ref) {
            None => out.push(0),
            Some(term) => {
                out.push(1);
                let text = term.to_string();
                // Big-endian, so the byte order of the length agrees with its numeric order
                // and two encodings compare as their terms do rather than by length digits.
                out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_be_bytes());
                out.extend_from_slice(text.as_bytes());
            }
        }
    }
    out
}

/// The terms an encoded row holds.
///
/// A term that will not parse back comes out unbound rather than panicking. That cannot
/// happen for a term this module encoded — `Term`'s `Display` and `FromStr` are inverses —
/// and treating it as unbound is what the rest of the engine does with a term it cannot
/// resolve.
fn decode(row: &[u8], width: usize) -> Vec<Option<Term>> {
    decode_at(row, width, 0).0
}

/// [`decode`] from an offset, reporting where it stopped, so that two encodings can share
/// one record — a sort key and then the row it belongs to.
fn decode_at(row: &[u8], width: usize, from: usize) -> (Vec<Option<Term>>, usize) {
    let mut out = Vec::with_capacity(width);
    let mut at = from;
    for _ in 0..width {
        match row.get(at) {
            Some(0) => {
                at += 1;
                out.push(None);
            }
            Some(_) => {
                at += 1;
                let Some(length) = row.get(at..at + 4) else {
                    out.push(None);
                    continue;
                };
                let length = u32::from_be_bytes([length[0], length[1], length[2], length[3]]);
                at += 4;
                let end = at + length as usize;
                let text = row.get(at..end).and_then(|b| std::str::from_utf8(b).ok());
                at = end;
                out.push(text.and_then(|t| t.parse::<Term>().ok()));
            }
            None => out.push(None),
        }
    }
    (out, at)
}

/// Writes one length-prefixed record, and reports how many bytes that took.
fn write_row(out: &mut BufWriter<File>, row: &[u8]) -> std::io::Result<usize> {
    out.write_all(&u32::try_from(row.len()).unwrap_or(u32::MAX).to_le_bytes())?;
    out.write_all(row)?;
    Ok(row.len() + 4)
}

/// `Ok` while the scratch written is within the ceiling, and the breach otherwise.
///
/// Checked after a run rather than after a row: a run is the unit that is written, the
/// overshoot is one run at most, and a comparison per row would be a comparison per row.
fn over_disk_limit(written: usize, limit: usize) -> std::io::Result<()> {
    if limit != 0 && written > limit {
        return Err(std::io::Error::other(DiskLimitExceeded { written, limit }));
    }
    Ok(())
}

/// Reads one run file back, a row at a time.
pub struct RunReader {
    file: BufReader<File>,
}

impl RunReader {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self {
            file: BufReader::with_capacity(64 * 1024, File::open(path)?),
        })
    }

    fn next(&mut self) -> std::io::Result<Option<Row>> {
        let mut length = [0u8; 4];
        match self.file.read_exact(&mut length) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let mut row = vec![0u8; u32::from_le_bytes(length) as usize];
        self.file.read_exact(&mut row)?;
        Ok(Some(row))
    }
}
