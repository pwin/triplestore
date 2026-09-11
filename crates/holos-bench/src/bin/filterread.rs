//! What does a partitioned bloom filter cost to *read*?
//!
//! 0.9.0 gave the bulk load's `SstFileWriter` `partition_filters` and a two-level index,
//! because a whole filter is built in one piece and one piece covering a whole store is
//! gigabytes (see `holos-store`'s `bulk_index_opts`, and `sstmem` for that measurement). The
//! write side is settled. The read side was left explicitly unmeasured, and this measures it.
//!
//! # Why it might cost something
//!
//! A whole filter is loaded once and held by the table reader for as long as the file is open.
//! A partitioned one is fetched a partition at a time through the block cache, so a lookup
//! whose partition is not cached pays a read for it, and the default block cache is small. The
//! trade is bounded write memory against possibly slower reads, and *possibly* is the word
//! that needs replacing with a number.
//!
//! # The workload
//!
//! Three phases, because a bloom filter earns its keep in exactly one of them:
//!
//! - **hits** — look up subjects that exist. The filter says "maybe" and the file is read
//!   anyway, so this mostly measures the index.
//! - **misses** — look up subjects that do not. This is the case the filter exists for: it
//!   says "no" and a file is skipped without being touched. If partitioning hurts, it hurts
//!   most here, because the filter itself now has to be fetched to answer.
//! - **scan** — read everything. Filters are irrelevant; this is a control that should come
//!   out the same either way, and if it does not, the comparison is measuring something else.
//!
//! # Running it
//!
//! ```text
//! cargo run --release -p holos-bench --bin filterread -- <triples> <dir>
//! ```
//!
//! To compare the two filter shapes, flip `write_and_ingest` between `bulk_index_opts` and
//! `index_opts`, rebuild, and run again with the same arguments. One variable, and the scan
//! phase is there to say whether anything else moved.

use holos_core::{Tag, TermId};
use holos_engine::Engine;
use holos_store::{GraphFilter, Store};
use oxrdfio::RdfFormat;
use std::io::{Read, Write};
use std::time::Instant;

#[cfg(feature = "rocksdb")]
use holos_store::RocksStorage;

/// Subjects and predicates the generated triples draw from.
///
/// The same shape `sstmem` uses: a product of small pools, so every triple is distinct while
/// the dictionary stays tiny and the index is the only thing that grows.
const SUBJECTS: usize = 4096;
const PREDICATES: usize = 64;

/// Lookups per phase. Enough that a per-lookup cost of tens of microseconds is visible above
/// the noise of a few seconds of wall clock.
const PROBES: usize = 20_000;

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
    eprintln!("filterread needs the rocksdb feature");
}

#[cfg(feature = "rocksdb")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let count: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .ok_or("usage: filterread <triples> <dir>")?;
    let dir = std::path::PathBuf::from(args.next().ok_or("usage: filterread <triples> <dir>")?);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }

    let started = Instant::now();
    let mut store = Store::with_storage(RocksStorage::open(dir.clone())?);
    store.begin_bulk_load()?;
    let mut engine = Engine::with_store(store);
    engine.bulk_load(Generated::new(count), RdfFormat::NTriples, None)?;
    engine.store_mut().end_bulk_load()?;
    let load = started.elapsed();
    let store = engine.store();

    // No compaction step, because there is nothing for it to do: the ingest hands each index
    // order one merged, non-overlapping file, which lands at the bottom level. That is the
    // natural state of a freshly loaded store and the one worth measuring.

    // Ids for subjects that exist, and for subjects that do not. Resolving them up front keeps
    // the dictionary lookup out of the timed section — the index is what is under test.
    let mut present = Vec::with_capacity(PROBES);
    for i in 0..PROBES {
        let iri = oxrdf::NamedNode::new_unchecked(format!(
            "http://holos.test/s{}",
            (i * 7_919) % SUBJECTS
        ));
        if let Some(id) = store.lookup_term(iri.as_ref().into())? {
            present.push(id);
        }
    }
    // Absent ids are synthesised rather than looked up: a term the dictionary never issued has
    // no id to find, and the point is to probe the *index* for a subject that is not there.
    let ceiling = present.iter().map(|id| id.payload()).max().unwrap_or(0);
    let absent: Vec<TermId> = (1..=PROBES as u64)
        .map(|n| TermId::new(Tag::Iri, ceiling + n))
        .collect();

    let hits = probe(store, &present)?;
    let misses = probe(store, &absent)?;

    let scanning = Instant::now();
    let mut scanned = 0u64;
    for quad in store.quads_for_pattern(None, None, None, GraphFilter::Default) {
        quad?;
        scanned += 1;
    }
    let scan = scanning.elapsed();

    println!(
        "count={count} load={:.1}s | hits={:.0}us/probe ({} found) \
misses={:.0}us/probe | scan={:.1}s ({scanned} quads)",
        load.as_secs_f64(),
        hits.0.as_secs_f64() * 1e6 / present.len() as f64,
        hits.1,
        misses.0.as_secs_f64() * 1e6 / absent.len() as f64,
        scan.as_secs_f64(),
    );
    Ok(())
}

/// Times a subject-bound scan per id, returning the total and how many quads came back.
///
/// A bound subject is the prefix the extractor was configured for, so this is the lookup shape
/// the prefix bloom filter was put there to serve.
#[cfg(feature = "rocksdb")]
fn probe(
    store: &Store,
    subjects: &[TermId],
) -> Result<(std::time::Duration, u64), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut found = 0u64;
    for id in subjects {
        for quad in store.quads_for_pattern(Some(*id), None, None, GraphFilter::Default) {
            quad?;
            found += 1;
        }
    }
    Ok((started.elapsed(), found))
}
