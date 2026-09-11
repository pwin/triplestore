//! `DISTINCT` in bounded memory: sort, spill, merge.
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
use spareval::{QueryEvaluationError, QuerySolution, QuerySolutionIter};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// Bytes of encoded rows held before a run is written out.
///
/// The whole point is that this, and not the size of the answer, is what a `DISTINCT`
/// costs. 128 MiB is large enough that ordinary queries never touch the disk and small
/// enough to leave room for everything else the query is doing.
pub const SPILL_BYTES: usize = 128 << 20;

/// One encoded row, as written to a run file.
type Row = Vec<u8>;

/// One row as held in memory: the terms themselves, never serialised unless they spill.
type Live = Vec<Option<Term>>;

/// Collects rows, spilling sorted runs when the in-memory set outgrows its budget.
pub struct Distinct {
    dir: PathBuf,
    runs: Vec<PathBuf>,
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // `temp_dir`, which means `TMPDIR` on Unix and `TMP`/`TEMP` on Windows, and which is
        // very often the *system* volume rather than the one the store lives on. That is a
        // sharper edge than it looks: a `DISTINCT` over a large store spills in proportion to
        // its answer, not to any buffer, so it will happily write tens of gigabytes. Measured
        // on a 653.8-million-triple store, this reached 5.5 GB in twenty-two minutes and was
        // heading for something near seventy, against 29 GB free on that machine's `C:`.
        //
        // The bulk loader had the same choice and made the other one: `ingest_dir` puts its
        // scratch beside the database, so it lands on the volume an operator sized for the
        // store. This cannot do that — a query is not attached to one store's directory, and
        // an in-memory store has no directory at all — so the placement stays the platform's
        // and the operator's, through the environment. OPERATIONS.md says to set it.
        let dir = std::env::temp_dir().join(format!(
            "holos-distinct-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            runs: Vec::new(),
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
            write_row(&mut out, row)?;
        }
        out.flush()?;
        rows.clear();

        self.runs.push(path);
        self.bytes = 0;
        Ok(())
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
) -> Result<QuerySolutionIter<'static>, QueryEvaluationError> {
    let variables: Arc<[Variable]> = Arc::from(input.variables().to_vec());
    let width = variables.len();
    let io = |e: std::io::Error| QueryEvaluationError::Dataset(Box::new(e));

    let mut distinct = Distinct::new(width, budget).map_err(io)?;
    for solution in input {
        distinct.push(&solution?).map_err(io)?;
    }

    let mut merged = distinct.merge().map_err(io)?;
    let rows = std::iter::from_fn(move || match merged.next_row() {
        Ok(Some(row)) => Some(Ok(row)),
        Ok(None) => None,
        Err(e) => Some(Err(QueryEvaluationError::Dataset(Box::new(e)))),
    });
    Ok(QuerySolutionIter::from_tuples(variables, rows))
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
    let mut out = Vec::with_capacity(width);
    let mut at = 0;
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
    out
}

fn write_row(out: &mut BufWriter<File>, row: &[u8]) -> std::io::Result<()> {
    out.write_all(&u32::try_from(row.len()).unwrap_or(u32::MAX).to_le_bytes())?;
    out.write_all(row)
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
