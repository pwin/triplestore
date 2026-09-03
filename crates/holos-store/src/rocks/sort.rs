//! External merge sort, so a bulk load's size is bounded by disk rather than by memory.
//!
//! Sorted ingestion needs each index order's keys in order, and the first version got them by
//! holding the whole load in memory and sorting it nine times. That works up to a cap and
//! then falls back to writing keys one at a time — which is correct, and is the difference
//! between "a billion triples in minutes" and not.
//!
//! So: fill a buffer, sort it, write it to disk as a **run**, repeat. At the end, merge the
//! runs back into one sorted stream per order and hand that to the file writer. Memory is
//! then one buffer plus one row per run, whatever the load's size.
//!
//! # Why runs rather than nine files ingested separately
//!
//! `RocksDB` will accept several files for one column family, and spilling could just write an
//! SST per buffer and ingest them all. It would also throw away most of the point: files that
//! overlap each other cannot all sit at the bottom level, so they land higher up and the
//! compaction the ingest exists to avoid happens anyway. One merged file per order does not
//! overlap itself.
//!
//! # The format
//!
//! A run is its keys, back to back, big-endian, fixed width — `N * 8` bytes each, the same
//! bytes the index uses. Nothing else: no header, no count, no checksum. It is written and
//! read by this module inside one call, deleted before the call returns, and never survives
//! the process that made it, so a format that could be versioned or validated would be
//! answering a question nobody can ask.
//!
//! Fixed width is what makes the merge cheap: the reader refills a block and slices it, with
//! no framing to parse and no allocation per row.

use crate::error::{Result, StorageError};
use holos_core::TermId;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use super::codec;

/// How many rows a run reader holds at a time.
///
/// The merge touches one row per run at a time but reads in blocks, so this trades memory
/// against syscalls. At `N = 4` a block is 32 KiB per run.
const READ_ROWS: usize = 1024;

/// The sorted runs written for one index order.
///
/// Owns its files and deletes them when it is dropped, so an error part-way through a load
/// cannot leave a gigabyte of scratch behind for someone to find later.
pub(super) struct Runs<const N: usize> {
    dir: PathBuf,
    family: &'static str,
    paths: Vec<PathBuf>,
}

impl<const N: usize> Runs<N> {
    pub(super) fn new(dir: &Path, family: &'static str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            family,
            paths: Vec::new(),
        }
    }

    /// Sorts `rows` into `perm`'s order and writes them out.
    ///
    /// Sorts **in place** and does not clear: the same buffer feeds all nine orders, one
    /// after another, and the caller clears it once they have all had it. Copying it per
    /// order would multiply by nine the memory this exists to bound.
    ///
    /// Re-sorting a buffer that a previous order already deduplicated is safe because the
    /// orders are permutations of the same components: two rows equal under one are equal
    /// under all of them, so the first dedup is the only one with anything to do.
    ///
    /// Deduplicated here as well as at the merge. Doing it twice is not redundant: a run that
    /// has already dropped its duplicates is smaller to write, smaller to read back, and the
    /// merge then only has to look across runs rather than within them.
    pub(super) fn spill(&mut self, rows: &mut Vec<[TermId; N]>, perm: [usize; N]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        sort_dedup(rows, perm);

        let path = self
            .dir
            .join(format!("{}.{}.run", self.family, self.paths.len()));
        let file = File::create(&path).map_err(StorageError::Io)?;
        let mut out = BufWriter::new(file);
        for row in rows.iter() {
            out.write_all(&key_bytes(row, perm))
                .map_err(StorageError::Io)?;
        }
        out.flush().map_err(StorageError::Io)?;

        self.paths.push(path);
        Ok(())
    }

    /// Every row, in order, across the runs and whatever is still in memory.
    ///
    /// `tail` is the buffer that was never big enough to spill. Passing it in rather than
    /// spilling it first saves a write and a read of the last block, which for a load that
    /// never filled its buffer is the whole load.
    pub(super) fn merge<'a>(
        &self,
        tail: &'a mut Vec<[TermId; N]>,
        perm: [usize; N],
    ) -> Result<Merged<'a, N>> {
        sort_dedup(tail, perm);

        let mut sources = Vec::with_capacity(self.paths.len());
        for path in &self.paths {
            sources.push(RunReader::open(path)?);
        }

        let mut heap = BinaryHeap::with_capacity(sources.len() + 1);
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(row) = source.next()? {
                heap.push(Reverse((row, index)));
            }
        }
        // The in-memory tail is just another source, and giving it the last index keeps the
        // ordering total: two equal keys are separated by their source, so the heap never has
        // to compare them further.
        let tail_index = sources.len();
        if let Some(row) = tail.first() {
            heap.push(Reverse((key_of(row, perm), tail_index)));
        }

        Ok(Merged {
            sources,
            heap,
            tail,
            tail_at: 0,
            tail_index,
            perm,
            last: None,
        })
    }
}

impl<const N: usize> Drop for Runs<N> {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A sorted, deduplicated stream over runs and an in-memory tail.
pub(super) struct Merged<'a, const N: usize> {
    sources: Vec<RunReader<N>>,
    heap: BinaryHeap<Reverse<([u8; 32], usize)>>,
    tail: &'a [[TermId; N]],
    tail_at: usize,
    tail_index: usize,
    perm: [usize; N],
    last: Option<[u8; 32]>,
}

impl<const N: usize> Merged<'_, N> {
    /// The next key, as the bytes the index stores.
    ///
    /// `Ok(None)` ends the stream. Duplicates are dropped here, which is what lets the same
    /// quad appear in two runs — it will, whenever a load mentions it either side of a spill.
    pub(super) fn next(&mut self) -> Result<Option<[u8; 32]>> {
        loop {
            let Some(Reverse((key, index))) = self.heap.pop() else {
                return Ok(None);
            };

            // Refill from wherever that key came from, before deciding whether to emit it.
            if index == self.tail_index {
                self.tail_at += 1;
                if let Some(row) = self.tail.get(self.tail_at) {
                    self.heap.push(Reverse((key_of(row, self.perm), index)));
                }
            } else if let Some(row) = self.sources[index].next()? {
                self.heap.push(Reverse((row, index)));
            }

            if self.last.as_ref() == Some(&key) {
                continue;
            }
            self.last = Some(key);
            return Ok(Some(key));
        }
    }

    /// How many bytes of each emitted key are the key.
    ///
    /// The heap carries a fixed 32-byte array because a const-generic array cannot be its
    /// own `Ord` key inside a `BinaryHeap` without a wrapper per width; the unused tail is
    /// zero and is trimmed here rather than compared.
    pub(super) const fn width() -> usize {
        N * codec::ID
    }
}

/// One run file, read back a block at a time.
struct RunReader<const N: usize> {
    file: BufReader<File>,
    block: Vec<u8>,
    at: usize,
    filled: usize,
}

impl<const N: usize> RunReader<N> {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(StorageError::Io)?;
        Ok(Self {
            file: BufReader::new(file),
            block: vec![0; READ_ROWS * N * codec::ID],
            at: 0,
            filled: 0,
        })
    }

    fn next(&mut self) -> Result<Option<[u8; 32]>> {
        let width = N * codec::ID;
        if self.at >= self.filled {
            self.filled = read_block(&mut self.file, &mut self.block, width)?;
            self.at = 0;
            if self.filled == 0 {
                return Ok(None);
            }
        }
        let mut out = [0u8; 32];
        out[..width].copy_from_slice(&self.block[self.at..self.at + width]);
        self.at += width;
        Ok(Some(out))
    }
}

/// Fills `block` with whole rows, returning how many bytes are usable.
///
/// A run is a whole number of rows by construction, so a short read is the end of the file
/// rather than a torn row — but a `read` is allowed to return less than asked for at any
/// time, so this loops until the block is full or the file is done.
fn read_block(file: &mut BufReader<File>, block: &mut [u8], width: usize) -> Result<usize> {
    let mut filled = 0;
    while filled < block.len() {
        match file.read(&mut block[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(StorageError::Io(e)),
        }
    }
    if filled % width != 0 {
        return Err(StorageError::corruption(format!(
            "a sort run ended mid-row: {filled} bytes is not a multiple of {width}"
        )));
    }
    Ok(filled)
}

/// Sorts by the permuted key and drops duplicates.
fn sort_dedup<const N: usize>(rows: &mut Vec<[TermId; N]>, perm: [usize; N]) {
    rows.sort_unstable_by_key(|row| perm.map(|i| row[i]));
    rows.dedup_by(|a, b| perm.map(|i| a[i]) == perm.map(|i| b[i]));
}

/// The index key for a row, as bytes, padded to a fixed width for the heap.
fn key_of<const N: usize>(row: &[TermId; N], perm: [usize; N]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..N * codec::ID].copy_from_slice(&key_bytes(row, perm));
    out
}

/// The index key for a row, exactly as wide as the index stores it.
///
/// Big-endian, which is why sorting the ids and sorting the bytes agree — the whole reason a
/// numeric sort can produce a file `RocksDB` will accept without re-comparing anything.
fn key_bytes<const N: usize>(row: &[TermId; N], perm: [usize; N]) -> Vec<u8> {
    let mut out = Vec::with_capacity(N * codec::ID);
    for i in perm {
        out.extend_from_slice(&codec::put_id(row[i]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> TermId {
        TermId::from_raw(n)
    }

    fn rows(values: &[[u64; 3]]) -> Vec<[TermId; 3]> {
        values
            .iter()
            .map(|[a, b, c]| [id(*a), id(*b), id(*c)])
            .collect()
    }

    /// Drains a merge into the keys it produced.
    fn drain<const N: usize>(mut merged: Merged<'_, N>) -> Vec<Vec<u8>> {
        let width = Merged::<N>::width();
        let mut out = Vec::new();
        while let Some(key) = merged.next().expect("merge") {
            out.push(key[..width].to_vec());
        }
        out
    }

    fn expected(values: &[[u64; 3]], perm: [usize; 3]) -> Vec<Vec<u8>> {
        let mut rows = rows(values);
        sort_dedup(&mut rows, perm);
        rows.iter().map(|r| key_bytes(r, perm)).collect()
    }

    #[test]
    fn a_load_that_never_spills_merges_from_memory_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runs: Runs<3> = Runs::new(dir.path(), "dspo");
        assert!(runs.paths.is_empty(), "nothing spilled");

        let values = [[3, 1, 2], [1, 1, 1], [2, 9, 9], [1, 1, 1]];
        let mut tail = rows(&values);
        let merged = runs.merge(&mut tail, [0, 1, 2]).expect("merge");
        assert_eq!(drain(merged), expected(&values, [0, 1, 2]));
    }

    /// The property that matters: spilling must not change the answer.
    #[test]
    fn spilling_produces_the_same_stream_as_not_spilling() {
        let dir = tempfile::tempdir().expect("temp dir");
        let values: Vec<[u64; 3]> = (0..500)
            .map(|i| [(i * 7919) % 101, (i * 104_729) % 53, i])
            .collect();

        for perm in [[0, 1, 2], [1, 2, 0], [2, 0, 1]] {
            let mut runs: Runs<3> = Runs::new(dir.path(), "dspo");
            // Spilled in three uneven pieces, so the runs are different lengths and the
            // boundaries fall in different places for each order.
            let mut all = rows(&values);
            for chunk in [0..173, 173..400, 400..500] {
                let mut piece: Vec<[TermId; 3]> = all[chunk].to_vec();
                runs.spill(&mut piece, perm).expect("spill");
            }
            assert!(!runs.paths.is_empty());
            let mut nothing = Vec::new();
            let merged = runs.merge(&mut nothing, perm).expect("merge");
            assert_eq!(drain(merged), expected(&values, perm), "perm {perm:?}");
            all.clear();
        }
    }

    /// A row either side of a spill boundary reaches the merge twice, and must come out once.
    #[test]
    fn duplicates_across_runs_are_emitted_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runs: Runs<3> = Runs::new(dir.path(), "dspo");

        let mut first = rows(&[[1, 1, 1], [2, 2, 2]]);
        runs.spill(&mut first, [0, 1, 2]).expect("spill");
        let mut second = rows(&[[2, 2, 2], [3, 3, 3]]);
        runs.spill(&mut second, [0, 1, 2]).expect("spill");
        // And once more in the tail, which is a third source for the same key.
        let mut tail = rows(&[[2, 2, 2]]);

        let merged = runs.merge(&mut tail, [0, 1, 2]).expect("merge");
        assert_eq!(
            drain(merged),
            expected(&[[1, 1, 1], [2, 2, 2], [3, 3, 3]], [0, 1, 2])
        );
    }

    #[test]
    fn runs_are_deleted_when_they_go_out_of_scope() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = {
            let mut runs: Runs<3> = Runs::new(dir.path(), "dspo");
            let mut rows = rows(&[[1, 2, 3]]);
            runs.spill(&mut rows, [0, 1, 2]).expect("spill");
            runs.paths.clone()
        };
        assert_eq!(paths.len(), 1);
        assert!(
            !paths[0].exists(),
            "a run outlived the load that made it: {}",
            paths[0].display()
        );
    }

    /// Four-wide keys are the named-graph orders, and the padding in the heap must not make
    /// a short key sort against a long one.
    #[test]
    fn quad_width_keys_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runs: Runs<4> = Runs::new(dir.path(), "gspo");
        let mut a: Vec<[TermId; 4]> =
            vec![[id(9), id(1), id(1), id(1)], [id(1), id(5), id(5), id(5)]];
        runs.spill(&mut a, [3, 0, 1, 2]).expect("spill");
        let mut tail: Vec<[TermId; 4]> = vec![[id(4), id(2), id(2), id(2)]];

        let merged = runs.merge(&mut tail, [3, 0, 1, 2]).expect("merge");
        let keys = drain(merged);
        assert_eq!(keys.len(), 3);
        assert_eq!(Merged::<4>::width(), 32);
        // Ascending on the permuted first component, which is the graph.
        let firsts: Vec<u64> = keys
            .iter()
            .map(|k| u64::from_be_bytes(k[..8].try_into().expect("8 bytes")))
            .collect();
        assert_eq!(firsts, vec![1, 2, 5]);
    }
}
