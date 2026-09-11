//! Does a bulk load's memory depend on how much is loaded?
//!
//! It is supposed not to. `sort.rs` spills fixed-size buffers and merges them, so the
//! streaming phase is flat by construction, and a 667-million-triple load confirmed it —
//! 2.4 GB peak across three hours. Then the final merge reached **18 GB** on a 32 GB machine.
//!
//! The merge itself is bounded too (one block per run, a heap of one row per run), so the
//! memory was not HOLOS's. It was RocksDB's: `index_opts` asked for a *full* bloom filter,
//! and `FullFilterBlockBuilder` holds a 64-bit hash per key until `finish`. A memtable flush
//! builds one for a few thousand keys. A bulk load builds one for **every triple in the
//! store**, in one piece, three times over.
//!
//! # What this measures, and why it does not need an A/B build
//!
//! Peak resident memory against load size, at two sizes. The question is not "is one build
//! faster than another" — the trap this repository has fallen into twice, where the *parse*
//! phase differed between builds and gave a 23% win that was not real. The question is
//! whether one number scales with another, which one build answers on its own:
//!
//! - if peak memory roughly doubles when the triple count doubles, it is proportional to the
//!   load, and no store is large enough to be safe;
//! - if it stays flat, the load is bounded and the size stops mattering.
//!
//! # Isolating the index
//!
//! The generated triples are a product of three small pools — 4096 subjects, 64 predicates,
//! and as many objects as the count needs. Every triple is distinct, so the index holds
//! exactly `count` keys per order, while the dictionary stays at a few thousand terms
//! whatever the count. That way the only quantity that grows with the load is the one under
//! test. Reusing a term pool this heavily is unlike real data, which is the point: real data
//! would grow the dictionary too and confound the two.
//!
//! # Running it
//!
//! ```text
//! cargo run --release -p holos-bench --bin sstmem -- <count> <dir>
//! ```
//!
//! Peak memory is read from outside, by whatever launches it — the process knows its own
//! Rust allocations and this is deliberately not one of them.

use holos_engine::Engine;
use holos_store::Store;
use oxrdfio::RdfFormat;
use std::io::{Read, Write};
use std::time::Instant;

#[cfg(feature = "rocksdb")]
use holos_store::RocksStorage;

/// How many subjects and predicates the generated triples draw from.
///
/// Their product with the object pool is the triple count, so these two fix how many terms
/// the dictionary ever sees: 4096 + 64 + `count` / 262144.
const SUBJECTS: usize = 4096;
const PREDICATES: usize = 64;

/// N-Triples for `count` distinct triples, generated as they are read.
///
/// A reader rather than a file because the file would be the experiment's largest cost: at
/// 40 million triples it is about 2.5 GB to write and read back, all of it page cache
/// competing with the thing being measured.
struct Generated {
    count: usize,
    at: usize,
    block: Vec<u8>,
    pos: usize,
}

impl Generated {
    fn new(count: usize) -> Self {
        Self {
            count,
            at: 0,
            block: Vec::with_capacity(80 * 1024),
            pos: 0,
        }
    }
}

impl Read for Generated {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.block.len() {
            if self.at == self.count {
                return Ok(0);
            }
            self.block.clear();
            self.pos = 0;
            // A block at a time, so the per-line formatting is not also a per-call cost.
            while self.block.len() < 64 * 1024 && self.at < self.count {
                let i = self.at;
                let s = i % SUBJECTS;
                let p = (i / SUBJECTS) % PREDICATES;
                let o = i / (SUBJECTS * PREDICATES);
                writeln!(
                    self.block,
                    "<http://holos.test/s{s}> <http://holos.test/p{p}> <http://holos.test/o{o}> ."
                )?;
                self.at += 1;
            }
        }
        let n = out.len().min(self.block.len() - self.pos);
        out[..n].copy_from_slice(&self.block[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(not(feature = "rocksdb"))]
fn main() {
    eprintln!("sstmem needs the rocksdb feature: the memory under test is RocksDB's");
}

#[cfg(feature = "rocksdb")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let count: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .ok_or("usage: sstmem <count> <dir>")?;
    let dir = std::path::PathBuf::from(args.next().ok_or("usage: sstmem <count> <dir>")?);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }

    let started = Instant::now();
    let mut store = Store::with_storage(RocksStorage::open(dir.clone())?);
    store.begin_bulk_load()?;
    let mut engine = Engine::with_store(store);
    engine.bulk_load(Generated::new(count), RdfFormat::NTriples, None)?;
    let loaded = started.elapsed();

    // The phase under test. `end_bulk_load` is what merges the runs and writes one SST per
    // order, so it is where a filter built in one piece would be built.
    let merging = Instant::now();
    engine.store_mut().end_bulk_load()?;
    let merged = merging.elapsed();

    println!(
        "count={count} stream={:.1}s merge={:.1}s total={:.1}s",
        loaded.as_secs_f64(),
        merged.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
