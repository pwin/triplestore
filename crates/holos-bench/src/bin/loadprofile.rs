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
//! ```text
//! cargo run --release -p holos-bench --bin loadprofile [file.nt]
//! ```

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
    for quad in RdfParser::from_format(RdfFormat::NTriples)
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
fn parse_and_encode(
    path: &str,
    mut store: Store,
    bulk: bool,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let started = Instant::now();
    if bulk {
        store.begin_bulk_load()?;
    }
    for quad in RdfParser::from_format(RdfFormat::NTriples)
        .rename_blank_nodes()
        .for_reader(open(path)?)
    {
        store.encode_quad(quad?.as_ref())?;
    }
    if bulk {
        store.end_bulk_load()?;
    }
    Ok(started.elapsed())
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
    engine.bulk_load(open(path)?, RdfFormat::NTriples, None)?;
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
        let bulk_encode = parse_and_encode(&path, rocks_at(scratch("bulkencode")?)?, true)?;
        report(
            "6. + dictionary, bulk mode",
            quads,
            bulk_encode,
            Some(parse),
        );
        let rocks_bulk = full_load(&path, rocks_at(scratch("bulk")?)?, true)?;
        report(
            "7. + index, bulk mode",
            quads,
            rocks_bulk,
            Some(bulk_encode),
        );
        return Ok(());
    }

    let memory_encode = parse_and_encode(&path, Store::new(), false)?;
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
        let rocks_encode = parse_and_encode(
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

        let bulk_encode = parse_and_encode(
            &path,
            Store::with_storage(RocksStorage::open(scratch("bulkencode")?)?),
            true,
        )?;
        report(
            "6. + dictionary, on RocksDB, bulk mode",
            quads,
            bulk_encode,
            Some(parse),
        );

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
