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
//! # Sorting rather than hashing
//!
//! A hash set answers "have I seen this?" in constant time and cannot answer it at all once
//! it outgrows memory. Sorting turns the question into "is this the same as the row before
//! it?", which needs no memory beyond the row — so duplicates can be dropped during a merge
//! of runs that were never all resident.
//!
//! The cost is that the output comes back **sorted rather than in arrival order**. SPARQL
//! does not order the results of `DISTINCT`, so this is allowed; a query that cares says
//! `ORDER BY`, which re-sorts anyway.
//!
//! # Comparing rows as bytes
//!
//! Each row is encoded as its terms' N-Triples forms, length-prefixed, with a marker for
//! unbound. Two rows are equal exactly when their encodings are, because that serialisation
//! is injective over terms: `"1"^^xsd:integer` and `"01"^^xsd:integer` are different RDF
//! terms, they are different strings, and `DISTINCT` must keep both. Comparing bytes is
//! therefore comparing terms, and it is what lets a run be a file rather than a structure.

use oxrdf::{Term, Variable};
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

/// One encoded row.
type Row = Vec<u8>;

/// Collects rows, spilling sorted runs, and yields each distinct row once.
pub struct Distinct {
    dir: PathBuf,
    runs: Vec<PathBuf>,
    buffer: Vec<Row>,
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
        let dir = std::env::temp_dir().join(format!(
            "holos-distinct-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            runs: Vec::new(),
            buffer: Vec::new(),
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

    /// Adds a solution.
    ///
    /// # Errors
    ///
    /// If a run cannot be written.
    pub fn push(&mut self, solution: &QuerySolution) -> std::io::Result<()> {
        let row = encode(solution, self.width);
        self.bytes += row.len() + std::mem::size_of::<Row>();
        self.buffer.push(row);
        if self.bytes >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    /// Sorts what is buffered, drops its duplicates, and writes it out.
    ///
    /// Deduplicating here as well as at the merge is not redundant: a run that has already
    /// dropped its duplicates is smaller to write, smaller to read back, and the merge then
    /// only has to look across runs rather than within them.
    fn spill(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable();
        self.buffer.dedup();

        let path = self.dir.join(format!("{}.run", self.runs.len()));
        let mut out = BufWriter::new(File::create(&path)?);
        for row in &self.buffer {
            write_row(&mut out, row)?;
        }
        out.flush()?;

        self.runs.push(path);
        self.buffer.clear();
        self.bytes = 0;
        Ok(())
    }

    /// Every distinct row, in encoded order.
    ///
    /// # Errors
    ///
    /// If a run cannot be read back.
    pub fn merge(mut self) -> std::io::Result<Merged> {
        self.buffer.sort_unstable();
        self.buffer.dedup();

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
        if let Some(row) = self.buffer.first() {
            heap.push(Reverse((row.clone(), tail)));
        }
        Ok(Merged {
            owner: self,
            sources,
            heap,
            tail,
            tail_at: 0,
            last: None,
        })
    }
}

impl Drop for Distinct {
    fn drop(&mut self) {
        for path in &self.runs {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// One sorted, deduplicated stream over every run and the buffered tail.
pub struct Merged {
    owner: Distinct,
    sources: Vec<RunReader>,
    heap: BinaryHeap<Reverse<(Row, usize)>>,
    tail: usize,
    tail_at: usize,
    /// The row just emitted, so its duplicates in other runs can be skipped.
    last: Option<Row>,
}

impl Merged {
    /// The next distinct row.
    ///
    /// # Errors
    ///
    /// If a run cannot be read back.
    pub fn next_row(&mut self) -> std::io::Result<Option<Vec<Option<Term>>>> {
        loop {
            let Some(Reverse((row, source))) = self.heap.pop() else {
                return Ok(None);
            };
            self.advance(source)?;
            if self.last.as_ref() == Some(&row) {
                // The same row from another run. Dropping it here is the whole reason the
                // stream is sorted.
                continue;
            }
            let decoded = decode(&row, self.owner.width);
            self.last = Some(row);
            return Ok(Some(decoded));
        }
    }

    fn advance(&mut self, source: usize) -> std::io::Result<()> {
        if source == self.tail {
            self.tail_at += 1;
            if let Some(row) = self.owner.buffer.get(self.tail_at) {
                self.heap.push(Reverse((row.clone(), self.tail)));
            }
        } else if let Some(row) = self.sources[source].next()? {
            self.heap.push(Reverse((row, source)));
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
fn encode(solution: &QuerySolution, width: usize) -> Row {
    let mut out = Vec::with_capacity(width * 16);
    for slot in 0..width {
        match solution.get(slot) {
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

struct RunReader {
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
