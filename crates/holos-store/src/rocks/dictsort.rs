//! Spillable sorted runs of variable-width dictionary rows.
//!
//! `sort.rs` does this for the index families, where a row is a fixed `[TermId; N]` and the
//! whole file is one width. The dictionary cannot use it: a `str2id` key is a serialised
//! term of any length up to 512 bytes (or a 17-byte hash past that), and an `id2str` value
//! is the term itself. So the run format here is length-prefixed rather than fixed-stride,
//! and that is the only real difference between the two modules.
//!
//! # Why it exists
//!
//! `loadprofile` measures the dictionary at **76%** of a bulk load on data with high term
//! cardinality, and stubbing out the two `Pending::Put`s that buffer its rows accounts for
//! **1.62×** of that. Those rows were going through a `WriteBatch` — memtable, flush,
//! compaction — while the nine index families next door were already being handed to
//! `IngestExternalFile` as sorted files `RocksDB` can adopt without rewriting. This closes
//! that gap.
//!
//! # Combining, not just deduplicating
//!
//! The index runs deduplicate: two equal keys are the same quad and one of them is dropped.
//! The dictionary cannot always do that, because one `str2id` key can legitimately carry
//! **several** ids.
//!
//! Past [`INLINE_KEY_MAX`](super::INLINE_KEY_MAX) a key is a 128-bit hash of the term, and
//! two different long terms that hash alike share a row whose value is the list of both
//! their ids — that list is what keeps a collision from conflating two distinct RDF terms.
//! Dropping one of those rows would lose a term permanently. So equal keys are **appended**
//! when the key is hashed, and last-wins when it is exact, where a repeat cannot occur at
//! all: the bulk load's term cache hands out one id per distinct term, so each exact key is
//! produced once.
//!
//! Doing the append here rather than by reading the existing row also fixes a hazard the
//! batch path has: that read cannot see rows the same load has buffered but not yet
//! written, so two colliding terms interned close together could produce a list missing
//! the first of them. Merging by key cannot miss one.

use super::codec::ID as ID_WIDTH;
use super::{StorageError, KEY_HASHED};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, StorageError>;

/// One dictionary row.
type Row = (Box<[u8]>, Box<[u8]>);

/// A row in the merge heap, tagged with the source it came from.
///
/// The source index sits between the key and the value on purpose: it makes the ordering
/// total, so two rows with the same key are separated without the heap ever comparing two
/// values.
type HeapRow = Reverse<(Box<[u8]>, usize, Box<[u8]>)>;

/// How many bytes of rows a family holds before sorting and spilling them.
///
/// Bytes rather than rows, because the rows are the thing that varies: a family of short
/// IRIs and one of long literals would peak an order of magnitude apart at the same count.
pub(super) const SPILL_BYTES: usize = 256 << 20;

/// Sorted runs of `(key, value)` rows for one column family.
///
/// Owns its files and deletes them when dropped, so a load that fails part-way cannot leave
/// scratch behind.
pub(super) struct DictRuns {
    dir: PathBuf,
    family: &'static str,
    /// Which flush of the load these belong to, in every run's name. Two windows are alive
    /// at once while one is being written out on its own thread, in the same directory,
    /// and a run named only by its family and number would be created twice.
    window: usize,
    paths: Vec<PathBuf>,
    buffer: Vec<Row>,
    bytes: usize,
    budget: usize,
}

impl DictRuns {
    pub(super) fn new(dir: &Path, family: &'static str, budget: usize) -> Self {
        Self {
            dir: dir.to_path_buf(),
            family,
            window: 0,
            paths: Vec::new(),
            buffer: Vec::new(),
            bytes: 0,
            budget,
        }
    }

    /// Names these runs after the flush they belong to.
    pub(super) fn in_window(mut self, window: usize) -> Self {
        self.window = window;
        self
    }

    /// Adds a row, spilling first if this one would take the buffer past its budget.
    pub(super) fn push(&mut self, key: Box<[u8]>, value: Box<[u8]>) -> Result<()> {
        // The two lengths are the row's cost in a run file; the pointers are its cost in
        // memory. Counting the payload is close enough to bound either.
        self.bytes += key.len() + value.len() + 8;
        self.buffer.push((key, value));
        if self.bytes >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.buffer.is_empty() && self.paths.is_empty()
    }

    /// Whether anything has been spilled, or the buffer has reached its budget.
    ///
    /// The caller uses this to decide when to write the family out and ingest it, which is
    /// what lets the load forget the terms it has interned. A run already on disk counts:
    /// once one exists, the budget has been reached at least once.
    pub(super) fn at_budget(&self) -> bool {
        !self.paths.is_empty() || self.bytes >= self.budget
    }

    /// Sorts what is buffered and writes it out as one run.
    fn spill(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_by(|a, b| a.0.cmp(&b.0));

        let path = self
            .dir
            .join(format!("{}.{}.{}.dictrun", self.family, self.window, self.paths.len()));
        let mut out = BufWriter::new(File::create(&path).map_err(StorageError::Io)?);
        for (key, value) in &self.buffer {
            write_row(&mut out, key, value)?;
        }
        out.flush().map_err(StorageError::Io)?;

        self.paths.push(path);
        self.buffer.clear();
        self.bytes = 0;
        Ok(())
    }

    /// Every row, in key order, across the runs and whatever is still buffered.
    ///
    /// Consumes the runs: the merge borrows the tail and the readers, and nothing else may
    /// touch them while it is streaming.
    pub(super) fn merge(mut self) -> Result<Merged> {
        self.buffer.sort_by(|a, b| a.0.cmp(&b.0));

        let mut sources = Vec::with_capacity(self.paths.len());
        for path in &self.paths {
            sources.push(RunReader::open(path)?);
        }

        let mut heap = BinaryHeap::with_capacity(sources.len() + 1);
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(row) = source.next()? {
                heap.push(Reverse((row.0, index, row.1)));
            }
        }
        // The buffered tail is one more source, given the last index so that two equal keys
        // are still totally ordered and the heap never compares their values.
        let tail_index = sources.len();
        if let Some((key, value)) = self.buffer.first() {
            heap.push(Reverse((key.clone(), tail_index, value.clone())));
        }

        Ok(Merged {
            runs: self,
            sources,
            heap,
            tail_at: 0,
            tail_index,
        })
    }
}

impl Drop for DictRuns {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One sorted stream over every run and the buffered tail.
pub(super) struct Merged {
    /// The accumulator, held for two reasons: its buffered tail is one of the sources
    /// below, and its `Drop` deletes the run files once the readers are done with them.
    runs: DictRuns,
    sources: Vec<RunReader>,
    heap: BinaryHeap<HeapRow>,
    tail_at: usize,
    tail_index: usize,
}

impl Merged {
    /// The next row, with the values of equal keys combined.
    pub(super) fn next(&mut self) -> Result<Option<Row>> {
        let Some(Reverse((key, source, value))) = self.heap.pop() else {
            return Ok(None);
        };
        self.advance(source)?;

        let mut combined = value;
        // Every other row with this key, in whatever order the heap gives them.
        while let Some(Reverse((next_key, _, _))) = self.heap.peek() {
            if *next_key != key {
                break;
            }
            let Some(Reverse((_, next_source, next_value))) = self.heap.pop() else {
                break;
            };
            self.advance(next_source)?;
            combined = combine(&key, combined, &next_value);
        }
        Ok(Some((key, combined)))
    }

    /// Pushes the next row from whichever source just gave one up.
    fn advance(&mut self, source: usize) -> Result<()> {
        if source == self.tail_index {
            self.tail_at += 1;
            if let Some((key, value)) = self.runs.buffer.get(self.tail_at) {
                self.heap
                    .push(Reverse((key.clone(), self.tail_index, value.clone())));
            }
        } else if let Some(row) = self.sources[source].next()? {
            self.heap.push(Reverse((row.0, source, row.1)));
        }
        Ok(())
    }
}

/// How two values sharing a key are reconciled. See the module documentation.
fn combine(key: &[u8], first: Box<[u8]>, second: &[u8]) -> Box<[u8]> {
    if key.first() == Some(&KEY_HASHED) {
        // A candidate list: both ids have to survive, or one of the two colliding terms
        // becomes unresolvable.
        //
        // Deduplicated, because the two rows may share a prefix. Each was built by reading
        // whatever list the database already held for this key and appending one id, so two
        // colliding terms interned in the same load both carry every *previous* id as well
        // as their own. Concatenating blindly would repeat those. A duplicate is harmless —
        // `resolve` compares candidates against the term and a repeat just costs one more
        // comparison — but a list that doubles every time a load touches it is not.
        let mut out = first.into_vec();
        for id in second.chunks(ID_WIDTH) {
            if !out.chunks(ID_WIDTH).any(|seen| seen == id) {
                out.extend_from_slice(id);
            }
        }
        out.into_boxed_slice()
    } else {
        // An exact key is produced once per distinct term, so this is unreachable in a
        // load. Taking the later row is what a `WriteBatch` would have done.
        second.to_vec().into_boxed_slice()
    }
}

/// Reads one run file back, a row at a time.
struct RunReader {
    file: BufReader<File>,
}

impl RunReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: BufReader::with_capacity(64 * 1024, File::open(path).map_err(StorageError::Io)?),
        })
    }

    fn next(&mut self) -> Result<Option<Row>> {
        let mut lengths = [0u8; 8];
        match self.file.read_exact(&mut lengths) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(StorageError::Io(e)),
        }
        let key_len = u32::from_le_bytes([lengths[0], lengths[1], lengths[2], lengths[3]]) as usize;
        let value_len =
            u32::from_le_bytes([lengths[4], lengths[5], lengths[6], lengths[7]]) as usize;

        let mut key = vec![0u8; key_len];
        self.file.read_exact(&mut key).map_err(StorageError::Io)?;
        let mut value = vec![0u8; value_len];
        self.file.read_exact(&mut value).map_err(StorageError::Io)?;
        Ok(Some((key.into_boxed_slice(), value.into_boxed_slice())))
    }
}

fn write_row(out: &mut BufWriter<File>, key: &[u8], value: &[u8]) -> Result<()> {
    let key_len = u32::try_from(key.len())
        .map_err(|_| StorageError::corruption("a dictionary key longer than 4 GiB"))?;
    let value_len = u32::try_from(value.len())
        .map_err(|_| StorageError::corruption("a dictionary value longer than 4 GiB"))?;
    out.write_all(&key_len.to_le_bytes())
        .map_err(StorageError::Io)?;
    out.write_all(&value_len.to_le_bytes())
        .map_err(StorageError::Io)?;
    out.write_all(key).map_err(StorageError::Io)?;
    out.write_all(value).map_err(StorageError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rocks::KEY_EXACT;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("a scratch directory")
    }

    fn row(key: &[u8], value: &[u8]) -> Row {
        (
            key.to_vec().into_boxed_slice(),
            value.to_vec().into_boxed_slice(),
        )
    }

    fn drain(mut merged: Merged) -> Vec<Row> {
        let mut out = Vec::new();
        while let Some(row) = merged.next().expect("merge") {
            out.push(row);
        }
        out
    }

    fn exact(n: u8) -> Vec<u8> {
        vec![KEY_EXACT, n]
    }

    /// A load that never fills its buffer is a sort of what is in memory, and must give the
    /// same answer as one that spilled.
    #[test]
    fn a_run_that_never_spills_sorts_from_memory_alone() {
        let scratch = dir();
        let mut runs = DictRuns::new(scratch.path(), "str2id", usize::MAX);
        for n in [5u8, 1, 3, 2, 4] {
            runs.push(exact(n).into_boxed_slice(), vec![n].into_boxed_slice())
                .expect("push");
        }
        let rows = drain(runs.merge().expect("merge"));
        let keys: Vec<u8> = rows.iter().map(|(k, _)| k[1]).collect();
        assert_eq!(keys, [1, 2, 3, 4, 5]);
    }

    /// The whole point of spilling is that it changes nothing but memory.
    #[test]
    fn spilling_produces_the_same_stream_as_not_spilling() {
        let scratch = dir();
        let values: Vec<u8> = (0..64).rev().collect();

        let mut whole = DictRuns::new(scratch.path(), "a", usize::MAX);
        let mut spilled = DictRuns::new(scratch.path(), "b", 64);
        for n in &values {
            whole
                .push(exact(*n).into_boxed_slice(), vec![*n].into_boxed_slice())
                .expect("push");
            spilled
                .push(exact(*n).into_boxed_slice(), vec![*n].into_boxed_slice())
                .expect("push");
        }
        assert!(
            !spilled.paths.is_empty(),
            "the budget was too generous to spill, so this tests nothing"
        );
        assert_eq!(
            drain(whole.merge().expect("merge")),
            drain(spilled.merge().expect("merge"))
        );
    }

    /// A hashed key carries a list of candidate ids, and every one of them has to survive:
    /// dropping one makes a term unresolvable, which is the failure the candidate list
    /// exists to prevent.
    #[test]
    fn equal_hashed_keys_have_their_candidate_lists_appended() {
        let scratch = dir();
        let mut runs = DictRuns::new(scratch.path(), "str2id", 1);
        let key = vec![KEY_HASHED, 9];
        // A budget of 1 byte spills after every row, so these land in different runs and
        // the merge is what has to bring them back together.
        // Two ids, each the width `resolve` reads them back at, and a shared prefix: both
        // terms read the same pre-existing list before appending their own.
        let shared = vec![9u8; ID_WIDTH];
        let mut first = shared.clone();
        first.extend_from_slice(&[1u8; ID_WIDTH]);
        let mut second = shared.clone();
        second.extend_from_slice(&[2u8; ID_WIDTH]);
        runs.push(key.clone().into_boxed_slice(), first.into_boxed_slice())
            .expect("push");
        runs.push(key.clone().into_boxed_slice(), second.into_boxed_slice())
            .expect("push");
        let rows = drain(runs.merge().expect("merge"));
        assert_eq!(rows.len(), 1, "one key, one row");
        let ids: Vec<&[u8]> = rows[0].1.chunks(ID_WIDTH).collect();
        assert_eq!(
            ids,
            vec![&shared[..], &[1u8; ID_WIDTH][..], &[2u8; ID_WIDTH][..]],
            "both candidates must survive, and the shared prefix must not repeat"
        );
    }

    /// An exact key cannot repeat within a load, and if it somehow did, appending two ids
    /// would produce a value that decodes as neither.
    #[test]
    fn equal_exact_keys_take_the_later_value_rather_than_appending() {
        let scratch = dir();
        let mut runs = DictRuns::new(scratch.path(), "str2id", 1);
        runs.push(exact(7).into_boxed_slice(), vec![1].into_boxed_slice())
            .expect("push");
        runs.push(exact(7).into_boxed_slice(), vec![2].into_boxed_slice())
            .expect("push");
        let rows = drain(runs.merge().expect("merge"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.len(), 1, "an exact value must not grow");
    }

    /// Variable widths are the entire reason this module is not `sort.rs`.
    #[test]
    fn rows_of_wildly_different_widths_round_trip() {
        let scratch = dir();
        let mut runs = DictRuns::new(scratch.path(), "id2str", 128);
        let expected: Vec<Row> = (0..40u8)
            .map(|n| row(&[KEY_EXACT, n], &vec![n; usize::from(n) * 7]))
            .collect();
        for (key, value) in &expected {
            runs.push(key.clone(), value.clone()).expect("push");
        }
        assert_eq!(drain(runs.merge().expect("merge")), expected);
    }

    /// Scratch files are the kind of thing that gets left on a disk for a year.
    #[test]
    fn run_files_are_deleted_when_the_runs_go_out_of_scope() {
        let scratch = dir();
        {
            let mut runs = DictRuns::new(scratch.path(), "str2id", 1);
            for n in 0..8u8 {
                runs.push(exact(n).into_boxed_slice(), vec![n].into_boxed_slice())
                    .expect("push");
            }
            assert!(!runs.paths.is_empty());
        }
        let left: Vec<_> = std::fs::read_dir(scratch.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(left.is_empty(), "{} run files left behind", left.len());
    }

    #[test]
    fn an_empty_accumulator_merges_to_nothing() {
        let scratch = dir();
        let runs = DictRuns::new(scratch.path(), "str2id", usize::MAX);
        assert!(runs.is_empty());
        assert!(drain(runs.merge().expect("merge")).is_empty());
    }
}
