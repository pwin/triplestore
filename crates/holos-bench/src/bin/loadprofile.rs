//! Where does a persistent bulk load actually spend its time?
//!
//! `DESIGN.md` §6.1 wants `SstFileWriter` + `IngestExternalFile` here, on the grounds that it
//! is "the difference between a billion triples in minutes and in hours". That is a claim
//! about one phase of the load — turning encoded keys into files RocksDB will accept — and it
//! only pays if that phase is where the time goes. Building it first and measuring afterwards
//! would be the wrong order: if parsing or the dictionary dominates, a perfect ingest path
//! buys a few percent.
//!
//! So this peels the load apart and times each layer against the one below it. Each run does
//! strictly more than the last, so the difference between two adjacent lines is the cost of
//! the layer that was added.
//!
//! One trap is worth naming, because the first version of this fell into it: a phase that
//! interns terms *outside* bulk mode writes one `WriteBatch` per term, so it measures the
//! write path rather than the dictionary and makes the dictionary look four times its real
//! cost. The bulk-mode phases are the ones to subtract.
//!
//! # What it found, on 753k quads
//!
//! ```text
//! parse                          0.7 s   11%
//! dictionary (bulk mode)         2.0 s   32%
//! index writes (bulk mode)       3.4 s   55%
//!                                6.1 s   123k quads/s
//! ```
//!
//! So `SstFileWriter` is aimed at the right half: the index writes are the majority of a
//! bulk load, and the ceiling on that work is a bit over 2× rather than the order of
//! magnitude §6.1 implies. Two other things fell out of the same run:
//!
//! - **Bulk mode is already worth 2.8×** — 17.0 s without it against 6.1 s with.
//! - **A bloom filter on `str2id` is worth nothing at this scale.** It was tried, on the
//!   theory that interning misses would be skipping levels; measured, the family is small
//!   enough to live in the block cache and phase 8 is unchanged. Reverted rather than kept
//!   on the theory. Worth retrying where the dictionary does not fit in memory.
//!
//! # What it found on data with high term cardinality
//!
//! The profile above is of a synthetic set with heavy term reuse — 200k distinct terms for
//! 753k quads. Run against a generated person dataset with **900k distinct terms per 3M
//! triples**, the shares inverted:
//!
//! ```text
//! parse                          3.7 s   15%
//! dictionary                    19.8 s   76%
//! index writes                   2.5 s    9%
//! ```
//!
//! The one reproducible, large signal is that **the dictionary tracks distinct terms, not
//! triples**. Two files of 3M quads and equal size, differing only in cardinality:
//!
//! ```text
//! 500 distinct terms       dictionary layer   2.1 s
//! 900,000 distinct terms   dictionary layer  12.2 s      6x
//! ```
//!
//! A cache hit costs about 230 ns; a genuinely new term costs about 19 µs. Of that, the
//! `str2id` read is ~3.7 µs — and **~8 s of the remaining cost is buffering and writing the
//! dictionary rows**, measured by stubbing those two `Pending::Put`s out: 21.3 s against
//! 13.1 s, **1.62×**. The fix that phase 7 already had is the one this wants:
//! `SstFileWriter` + `IngestExternalFile` for `str2id` and `id2str` instead of a
//! `WriteBatch`. It needs the spill-and-merge treatment `sort.rs` gives the indexes, over
//! variable-width keys rather than fixed, or a load whose dictionary exceeds memory cannot
//! use it.
//!
//! # Four things that were tried and are not worth retrying
//!
//! Recorded because each looked obviously right beforehand, and measuring is the only thing
//! that separated them from the one above.
//!
//! - **A bloom filter on `str2id`, at high cardinality.** The note above invited this at a
//!   scale where the family does not fit in cache. Tried there: **consistently slower**,
//!   three rounds out of three. A bulk load *writes* this family continuously, so filter
//!   blocks are rebuilt on every flush and compaction, and that lands on the write path.
//! - **Deriving the `str2id` key only after the caches miss**, rather than always. Strictly
//!   less work, and 0.99× — one small allocation avoided 8M times is worth ~0.3 s against a
//!   15 s phase, which is under the noise.
//! - **The flush in `end_bulk_load`.** Suspected of hiding the cost; it is **0.47 s**. The
//!   dictionary is written incrementally during the load, in `BULK_BATCH_OPS` batches.
//! - **Pausing auto-compaction on the dictionary families during a bulk load**, which
//!   `start_bulk` does for the nine index families and not for these two. 1.09×, inside the
//!   noise — and there is a reason it is not free: `resolve` *reads* `str2id` throughout the
//!   load, so suppressing compaction trades write amplification for read amplification.
//!
//! # Reading these numbers
//!
//! This machine varies by ±40% on the dictionary phase — 12.6 to 22.1 s across runs of an
//! identical build — so nothing below about 1.5× is resolvable here without many rounds.
//! Two separate A/B attempts were contaminated before that was understood, and both times
//! the tell was the **parse** phase differing between builds, which no change to the
//! dictionary or the index can affect. Alternate which build runs first and check parse
//! before believing anything else in the table.
//!
//! ```text
//! cargo run --release -p holos-bench --bin loadprofile [file.nt] [bulk] [spill]
//! ```
//!
//! `bulk` runs only the phases a bulk load goes through, which is what an A/B of the write
//! path needs. `spill` is a buffer size in quads, passed to `set_ingest_limit`: forcing a
//! small buffer is how the external merge sort gets measured at a scale a person will wait
//! for, rather than by loading four million quads to reach the first spill.

use holos_engine::Engine;
use holos_store::Store;
use oxrdfio::{RdfFormat, RdfParser};
use std::io::BufReader;
use std::time::{Duration, Instant};

#[cfg(feature = "rocksdb")]
use holos_store::RocksStorage;

fn open(path: &str) -> std::io::Result<BufReader<std::fs::File>> {
    Ok(BufReader::new(std::fs::File::open(path)?))
}

/// The serialisation a path names, by extension.
///
/// Hardcoded to N-Triples until a 32 GB Turtle file needed profiling, at which point the
/// tool was measuring a parse that produced nothing. Inferring costs one match and lets the
/// profile be run against whatever a user actually has.
fn format_of(path: &str) -> RdfFormat {
    match path.rsplit('.').next().unwrap_or("") {
        "ttl" => RdfFormat::Turtle,
        "trig" => RdfFormat::TriG,
        "nq" => RdfFormat::NQuads,
        "rdf" | "xml" => RdfFormat::RdfXml,
        "n3" => RdfFormat::N3,
        _ => RdfFormat::NTriples,
    }
}

fn report(label: &str, quads: u64, elapsed: Duration, previous: Option<Duration>) {
    let seconds = elapsed.as_secs_f64();
    let added = previous.map_or(String::new(), |before| {
        let delta = seconds - before.as_secs_f64();
        format!(
            "  (+{:>7.2} s, {:>6.0} ns/quad for this layer)",
            delta,
            delta * 1e9 / quads as f64
        )
    });
    println!(
        "{label:<46} {:>9.2} s  {:>10} quads/s{added}",
        seconds,
        format!("{:.0}", quads as f64 / seconds)
    );
}

/// Parses the file and throws the quads away.
fn parse_only(path: &str) -> Result<(u64, Duration), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut n = 0;
    for quad in RdfParser::from_format(format_of(path))
        .rename_blank_nodes()
        .for_reader(open(path)?)
    {
        let _ = quad?;
        n += 1;
    }
    Ok((n, started.elapsed()))
}

/// Parses and interns every term, without touching the index.
///
/// `bulk` matters more than it looks. Outside bulk mode each interned term is its own
/// `WriteBatch`, so the phase measures the write path rather than the dictionary; inside it
/// the writes are buffered exactly as they are during a real load, which is the number the
/// index phase has to be subtracted from.
///
/// # Two numbers, not one
///
/// Returns interning and flushing separately, and the split is the point. A bulk load
/// buffers every dictionary row and writes them all in `end_bulk_load`, so timing the whole
/// function attributes that write to "the dictionary" — which reads as though interning a
/// term were expensive, when interning is a hash insert and the cost is a `WriteBatch` of a
/// million rows at the end. Reported as one number, this phase sent an earlier round of
/// optimisation after caches and allocations that were never the problem.
fn parse_and_encode(
    path: &str,
    mut store: Store,
    bulk: bool,
) -> Result<(Duration, Duration), Box<dyn std::error::Error>> {
    let started = Instant::now();
    if bulk {
        store.begin_bulk_load()?;
    }
    for quad in RdfParser::from_format(format_of(path))
        .rename_blank_nodes()
        .for_reader(open(path)?)
    {
        store.encode_quad(quad?.as_ref())?;
    }
    let interned = started.elapsed();
    let flushed = Instant::now();
    if bulk {
        store.end_bulk_load()?;
    }
    Ok((interned, flushed.elapsed()))
}

/// One line for the flush, so the write is not read as part of the interning above it.
fn report_flush(quads: u64, flush: Duration) {
    if flush.as_secs_f64() < 0.001 {
        return;
    }
    println!(
        "     of which dictionary flush           {:>8.2} s   {:>7.0} ns/quad",
        flush.as_secs_f64(),
        flush.as_nanos() as f64 / quads as f64
    );
}

/// A store for the persistent phases, with the spill threshold a third argument can set.
///
/// Forcing a small buffer is how the external merge sort gets measured at a scale a person
/// will wait for: the alternative is loading four million quads to reach the first spill.
#[cfg(feature = "rocksdb")]
fn rocks_at(dir: std::path::PathBuf) -> Result<Store, Box<dyn std::error::Error>> {
    let mut storage = RocksStorage::open(dir)?;
    if let Some(limit) = std::env::args().nth(3).and_then(|a| a.parse().ok()) {
        storage.set_ingest_limit(limit);
    }
    Ok(Store::with_storage(storage))
}

/// The whole pipeline.
fn full_load(
    path: &str,
    mut store: Store,
    bulk: bool,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let started = Instant::now();
    if bulk {
        store.begin_bulk_load()?;
    }
    let mut engine = Engine::with_store(store);
    engine.bulk_load(open(path)?, format_of(path), None)?;
    if bulk {
        engine.store_mut().end_bulk_load()?;
    } else {
        engine.store_mut().flush()?;
    }
    Ok(started.elapsed())
}

/// How long a dictionary lookup takes when the term is not there.
///
/// The case a whole-key bloom filter on `str2id` exists for, and the one a fresh load does
/// not exercise: with everything still in L0 there are no levels to skip. Measured after a
/// compaction, on a store that holds data, which is where a lookup that misses would
/// otherwise touch every level.
fn miss_lookups(store: &Store, n: usize) -> Duration {
    let started = Instant::now();
    for i in 0..n {
        let iri = oxrdf::NamedNode::new_unchecked(format!("http://holos.example/absent/{i}"));
        let found = store.lookup_term(iri.as_ref().into()).expect("lookup");
        assert!(found.is_none(), "the probe terms must not be in the store");
    }
    started.elapsed()
}

/// The path a previous phase wrote to, without clearing it.
fn scratch_existing(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join("holos-loadprofile").join(name)
}

fn scratch(name: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("holos-loadprofile").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("holos-bench")
            .join("bench-1000000.nt")
            .to_string_lossy()
            .into_owned()
    });

    // `bulk` runs only the phases a bulk load actually goes through, which is what an A/B of
    // the ingest path needs: the one-write-at-a-time phases cost half a minute, say nothing
    // about it, and their noise crowds out the signal.
    let only_bulk = std::env::args().nth(2).as_deref() == Some("bulk");

    let (quads, parse) = parse_only(&path)?;
    println!("{quads} quads from {path}\n");
    report("1. parse only", quads, parse, None);

    #[cfg(feature = "rocksdb")]
    if only_bulk {
        let (interned, flush) = parse_and_encode(&path, rocks_at(scratch("bulkencode")?)?, true)?;
        // The cumulative figure stays interning *plus* flush, so the index phase below
        // still subtracts the whole of this one; the breakdown is reported beneath it.
        let bulk_encode = interned + flush;
        report(
            "6. + dictionary, bulk mode",
            quads,
            bulk_encode,
            Some(parse),
        );
        report_flush(quads, flush);
        let rocks_bulk = full_load(&path, rocks_at(scratch("bulk")?)?, true)?;
        report(
            "7. + index, bulk mode",
            quads,
            rocks_bulk,
            Some(bulk_encode),
        );
        return Ok(());
    }

    let (memory_encode, _) = parse_and_encode(&path, Store::new(), false)?;
    report(
        "2. + dictionary, in memory",
        quads,
        memory_encode,
        Some(parse),
    );

    let memory_full = full_load(&path, Store::new(), false)?;
    report(
        "3. + index, in memory",
        quads,
        memory_full,
        Some(memory_encode),
    );

    #[cfg(feature = "rocksdb")]
    {
        let (rocks_encode, _) = parse_and_encode(
            &path,
            Store::with_storage(RocksStorage::open(scratch("encode")?)?),
            false,
        )?;
        report(
            "4. + dictionary, on RocksDB, one write at a time",
            quads,
            rocks_encode,
            Some(parse),
        );

        let rocks_plain = full_load(
            &path,
            Store::with_storage(RocksStorage::open(scratch("plain")?)?),
            false,
        )?;
        report(
            "5. + index, on RocksDB, one write at a time",
            quads,
            rocks_plain,
            Some(rocks_encode),
        );

        let (interned, flush) = parse_and_encode(
            &path,
            Store::with_storage(RocksStorage::open(scratch("bulkencode")?)?),
            true,
        )?;
        let bulk_encode = interned + flush;
        report(
            "6. + dictionary, on RocksDB, bulk mode",
            quads,
            bulk_encode,
            Some(parse),
        );
        report_flush(quads, flush);

        let rocks_bulk = full_load(
            &path,
            Store::with_storage(RocksStorage::open(scratch("bulk")?)?),
            true,
        )?;
        report(
            "7. + index, on RocksDB, bulk mode",
            quads,
            rocks_bulk,
            Some(bulk_encode),
        );

        // Reopened rather than reused: the load leaves its SSTs on disk and its caches warm,
        // and a lookup that misses is interesting precisely when it has to consult files.
        let loaded = Store::with_storage(RocksStorage::open(scratch_existing("bulk"))?);
        let probes = 200_000;
        let misses = miss_lookups(&loaded, probes);
        println!(
            "{:<46} {:>9.2} s  {:>10} lookups/s",
            "8. dictionary misses on the loaded store",
            misses.as_secs_f64(),
            format!("{:.0}", probes as f64 / misses.as_secs_f64())
        );

        println!();
        let ingest = rocks_bulk.as_secs_f64() - bulk_encode.as_secs_f64();
        println!(
            "The index phase of a bulk load is {:.0}% of it ({:.2} s of {:.2} s).",
            100.0 * ingest / rocks_bulk.as_secs_f64(),
            ingest,
            rocks_bulk.as_secs_f64()
        );
        println!(
            "That is the ceiling on what SST ingestion can win: the other {:.0}% is parsing \
             and the dictionary.",
            100.0 * bulk_encode.as_secs_f64() / rocks_bulk.as_secs_f64()
        );
    }

    Ok(())
}
