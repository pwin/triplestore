//! Load and query timings across dataset sizes.
//!
//! ```text
//! cargo run --release -p holos-bench                       # the default three scales
//! cargo run --release -p holos-bench -- 100000 1000000
//! cargo run --release -p holos-bench -- --queries-only 1000000
//! ```
//!
//! Scales are given in **people**, and each person contributes roughly 7.5 quads once the
//! holarchy and the `knows` edges are counted — so the default `1_000_000` is about
//! 7.5 million quads, not one million. The report prints both numbers.
//!
//! # How the timings are taken
//!
//! **Loads** are measured once, from a cold empty store, including parsing from a file on
//! disk. Including the parse is deliberate: it is what a real load does, and excluding it
//! would produce a number nobody can reproduce with their own data.
//!
//! **Queries** are run in a warm process against an already-open store. Each is run once to
//! warm any caches, then timed three times, and the **median** is reported. Median rather
//! than mean because a single scheduler hiccup should not decide the number, and rather
//! than minimum because the best case of three is not what anyone experiences.
//!
//! Every query is fully consumed — the row count is checked — so a lazy iterator cannot
//! flatter a timing by not doing the work.
//!
//! # What is deliberately not claimed
//!
//! This is one machine, one synthetic dataset, one process. It is a **profile of where this
//! store spends time**, useful for comparing shapes against each other and sizes against
//! each other. It is not a comparison with any other store, and the absolute numbers will
//! not survive different hardware.

#![forbid(unsafe_code)]

mod data;
mod queries;

use anyhow::{Context, Result};
use holos_engine::Engine;
use holos_holon::{registry, tick, Delta, Holon};
use holos_security::{Principal, Session};
use holos_shacl::{CompiledShapes, Options as ShaclOptions};
use holos_store::GraphFilter;
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term, Triple};
use oxrdfio::RdfFormat;
use queries::{Case, Group};
use spareval::QueryResults;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One load measurement.
struct LoadResult {
    backend: &'static str,
    quads: u64,
    elapsed: Duration,
    on_disk: Option<u64>,
    dictionary: usize,
}

impl LoadResult {
    fn rate(&self) -> f64 {
        self.quads as f64 / self.elapsed.as_secs_f64()
    }
}

/// One query measurement.
struct QueryResult {
    label: &'static str,
    group: Group,
    rows: usize,
    median: Duration,
    /// Set when the query returned something other than the answer it must return.
    ///
    /// A timing for a query that produced the wrong rows measures nothing, so a mismatch is
    /// carried into the report rather than being quietly averaged in with the rest.
    wrong: Option<String>,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let queries_only = args.iter().any(|a| a == "--queries-only");
    let scales: Vec<usize> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .filter_map(|a| a.parse().ok())
        .collect();
    let scales = if scales.is_empty() {
        // Roughly 0.75M, 3.8M and 7.5M quads: a decade of spread, and small enough that a
        // full run finishes while somebody is still watching it.
        vec![100_000, 500_000, 1_000_000]
    } else {
        scales
    };

    let work = std::env::var("HOLOS_BENCH_DIR")
        .map_or_else(|_| std::env::temp_dir().join("holos-bench"), PathBuf::from);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    println!("# HOLOS benchmark\n");
    println!(
        "Machine: {} / {} logical cores",
        std::env::consts::OS,
        available_parallelism()
    );
    println!("Working directory: `{}`\n", work.display());

    let mut load_rows: Vec<(usize, LoadResult)> = Vec::new();
    let mut query_rows: Vec<(usize, Vec<QueryResult>)> = Vec::new();

    for &people in &scales {
        let path = work.join(format!("bench-{people}.nt"));
        let quads = ensure_dataset(people, &path)?;
        eprintln!("== {people} people, {quads} quads ==");

        if !queries_only {
            // In memory: the ceiling, with no write amplification to disk.
            let (engine, result) = load_memory(&path, quads)?;
            load_rows.push((people, result));

            // Queries run against the in-memory store, so query timings are not measuring
            // RocksDB's block cache warming up.
            let results = run_battery(&engine, people)?;
            query_rows.push((people, results));
            drop(engine);

            #[cfg(feature = "rocksdb")]
            {
                for (label, bulk) in [("rocksdb, --bulk", true), ("rocksdb, no --bulk", false)] {
                    let dir = work.join(format!(
                        "db-{people}-{}",
                        if bulk { "bulk" } else { "plain" }
                    ));
                    let _ = std::fs::remove_dir_all(&dir);
                    load_rows.push((people, load_rocksdb(label, &path, quads, &dir, bulk)?));
                    if !bulk {
                        // Two full copies of a ten-million-quad store is a lot of disk to
                        // leave lying around for a number already recorded.
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
            }
        } else {
            let (engine, _) = load_memory(&path, quads)?;
            query_rows.push((people, run_battery(&engine, people)?));
        }
    }

    // The holon measurements run at one scale only: they are about the *cost of a commit
    // relative to a full pass*, and that ratio is what matters, not how it moves with size.
    let holon = run_holon_suite(&work, *scales.first().unwrap_or(&100_000))?;

    report(&scales, &load_rows, &query_rows, &holon);
    Ok(())
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get)
}

/// Writes the dataset if it is not already there, and returns its quad count.
fn ensure_dataset(people: usize, path: &Path) -> Result<u64> {
    if path.is_file() {
        // Counting lines is cheaper than regenerating, and the generator is deterministic
        // so an existing file at this scale is the right one.
        let bytes = std::fs::read(path)?;
        return Ok(bytes.iter().filter(|b| **b == b'\n').count() as u64);
    }
    eprintln!("generating {}", path.display());
    let file = std::fs::File::create(path)?;
    let mut out = BufWriter::new(file);
    let n = data::write_ntriples(people, &mut out)?;
    out.flush()?;
    Ok(n)
}

fn load_memory(path: &Path, quads: u64) -> Result<(Engine, LoadResult)> {
    let mut engine = Engine::new();
    let file = std::fs::File::open(path)?;
    let started = Instant::now();
    let loaded = engine.bulk_load(std::io::BufReader::new(file), RdfFormat::NTriples, None)?;
    let elapsed = started.elapsed();
    let dictionary = engine.store().dictionary_len();
    Ok((
        engine,
        LoadResult {
            backend: "in memory",
            quads: loaded.max(quads as usize) as u64,
            elapsed,
            on_disk: None,
            dictionary,
        },
    ))
}

#[cfg(feature = "rocksdb")]
fn load_rocksdb(
    backend: &'static str,
    path: &Path,
    quads: u64,
    dir: &Path,
    bulk: bool,
) -> Result<LoadResult> {
    let storage = holos_store::RocksStorage::open(dir)?;
    let mut engine = Engine::with_store(holos_store::Store::with_storage(storage));
    let file = std::fs::File::open(path)?;

    let started = Instant::now();
    if bulk {
        engine.store_mut().begin_bulk_load();
    }
    let loaded = engine.bulk_load(std::io::BufReader::new(file), RdfFormat::NTriples, None)?;
    if bulk {
        engine.store_mut().end_bulk_load()?;
    } else {
        engine.store_mut().flush()?;
    }
    let elapsed = started.elapsed();
    let dictionary = engine.store().dictionary_len();
    drop(engine);

    Ok(LoadResult {
        backend,
        quads: loaded.max(quads as usize) as u64,
        elapsed,
        on_disk: Some(directory_size(dir)),
        dictionary,
    })
}

fn directory_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => total += directory_size(&entry.path()),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}

/// Runs one case three times and reports the median.
fn time_case(engine: &Engine, session: &Session, case: &Case) -> Result<QueryResult> {
    let mut rows = 0;
    let mut samples = Vec::with_capacity(3);

    for run in 0..4 {
        let view = engine.view(session);
        let started = Instant::now();
        let results = Engine::query(&view, &case.sparql, None)
            .with_context(|| format!("running `{}`", case.label))?;
        // Consuming fully is what makes the timing honest: the iterators are lazy, so a
        // query that is never drained has not been run.
        let n = match results {
            QueryResults::Solutions(iter) => {
                let mut n = 0;
                for solution in iter {
                    solution?;
                    n += 1;
                }
                n
            }
            QueryResults::Boolean(_) => 1,
            QueryResults::Graph(iter) => {
                let mut n = 0;
                for triple in iter {
                    triple?;
                    n += 1;
                }
                n
            }
        };
        let elapsed = started.elapsed();
        // The first run warms caches and is discarded.
        if run > 0 {
            samples.push(elapsed);
        }
        rows = n;
    }

    samples.sort();
    let wrong = case
        .expect_rows
        .and_then(|want| (want != rows).then(|| format!("expected {want} rows, got {rows}")));
    Ok(QueryResult {
        label: case.label,
        group: case.group,
        rows,
        median: samples[samples.len() / 2],
        wrong,
    })
}

fn run_battery(engine: &Engine, people: usize) -> Result<Vec<QueryResult>> {
    let session = Session::unrestricted(engine.store())?;
    // An anchor that exists at every scale and has the heavy-tailed degree, so the path
    // queries have something to walk.
    let anchor = if people > 1000 { 1000 } else { 0 };
    let mut out = Vec::new();
    for case in queries::battery(anchor) {
        out.push(time_case(engine, &session, &case)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------
// holon suite
// ---------------------------------------------------------------------------------

/// What the holon measurements produced.
struct HolonResults {
    scene_quads: usize,
    full_validation: Duration,
    accepted_tick: Duration,
    rejected_tick: Duration,
    ticks: u64,
    queries: Vec<QueryResult>,
}

/// Registers a holon, drives it through a run of commits, then queries its history.
///
/// The scene is populated *through the holon*, one tick at a time, so the event log is a
/// real record rather than something synthesised for the benchmark. That costs more setup
/// time than bulk-loading the scene would, and it is the only way the provenance queries
/// mean anything.
fn run_holon_suite(work: &Path, people: usize) -> Result<HolonResults> {
    let _ = work;
    let scene_people = people.min(20_000);
    let ticks = 200_u64;

    let mut engine = Engine::new();
    let holon = Holon::new(NamedNode::new_unchecked("urn:holon:people"));
    let mut session = Session::open(
        engine.store(),
        Principal::anonymous(),
        unrestricted_policy(),
    )?;

    registry::register(&mut engine, &holon, &mut session)?;

    // The boundary, into the holon's own boundary graph.
    engine.bulk_load_into_graph(
        std::io::Cursor::new(data::BOUNDARY_SHAPES.as_bytes()),
        RdfFormat::Turtle,
        None,
        &GraphName::NamedNode(holon.boundary.clone()),
    )?;

    // Seed the scene directly: this part is setup, not measurement.
    {
        let store = engine.store_mut();
        for i in 0..scene_people {
            for quad in person_quads(i, &holon.scene) {
                store.insert(quad.as_ref())?;
            }
        }
    }
    let scene_quads = holos_holon::graph_size(engine.store(), &holon.scene);

    // A full pass over the whole scene, for the ratio the design rests on.
    let shapes = CompiledShapes::compile(
        engine.store(),
        ShaclOptions {
            data_graph: GraphFilter::Named(
                engine
                    .store()
                    .lookup_term(holon.scene.as_ref().into())?
                    .context("scene graph did not intern")?,
            ),
            shapes_graph: GraphFilter::Named(
                engine
                    .store()
                    .lookup_term(holon.boundary.as_ref().into())?
                    .context("boundary graph did not intern")?,
            ),
        },
    )?;
    let started = Instant::now();
    let _ = shapes.validate(engine.store())?;
    let full_validation = started.elapsed();

    // A run of accepted commits.
    let started = Instant::now();
    for t in 0..ticks {
        let i = scene_people + t as usize;
        let delta = Delta::adding(person_triples(i));
        let outcome = tick(&mut engine, &holon, &mut session, &delta)?;
        anyhow::ensure!(outcome.committed(), "tick {t} was rejected unexpectedly");
    }
    let accepted_total = started.elapsed();

    // One the boundary must refuse: an age outside the shape's range.
    let bad = Delta::adding([
        Triple {
            subject: data::person(999_999).into(),
            predicate: rdf::TYPE.into_owned(),
            object: Term::NamedNode(data::ex("Person")),
        },
        Triple {
            subject: data::person(999_999).into(),
            predicate: data::ex("name"),
            object: Literal::new_simple_literal("Too Old").into(),
        },
        Triple {
            subject: data::person(999_999).into(),
            predicate: data::ex("age"),
            object: Literal::new_typed_literal("900", xsd::INTEGER).into(),
        },
    ]);
    let started = Instant::now();
    let refused = tick(&mut engine, &holon, &mut session, &bad)?;
    let rejected_tick = started.elapsed();
    anyhow::ensure!(
        !refused.committed(),
        "the boundary should have refused this"
    );

    let queries = {
        let session = Session::unrestricted(engine.store())?;
        // A person committed *through a tick*, so the provenance queries have a record to
        // find. Version numbering starts at 1, so the last tick is `ticks`.
        let last = scene_people + ticks as usize - 1;
        let tracked = data::person(last);
        let tracked_name = format!("Person {last}");
        let cases = queries::holon_battery(
            holon.id.as_str(),
            holon.scene.as_str(),
            holon.events.as_str(),
            tracked.as_str(),
            &tracked_name,
            ticks,
        );
        let mut out = Vec::new();
        for case in cases {
            out.push(time_case(&engine, &session, &case)?);
        }
        out
    };

    Ok(HolonResults {
        scene_quads,
        full_validation,
        accepted_tick: accepted_total / u32::try_from(ticks).unwrap_or(1),
        rejected_tick,
        ticks,
        queries,
    })
}

fn unrestricted_policy() -> holos_security::Policy {
    holos_security::Policy::permit_all()
}

fn person_triples(i: usize) -> Vec<Triple> {
    vec![
        Triple {
            subject: data::person(i).into(),
            predicate: rdf::TYPE.into_owned(),
            object: Term::NamedNode(data::ex("Person")),
        },
        Triple {
            subject: data::person(i).into(),
            predicate: data::ex("name"),
            object: Literal::new_simple_literal(format!("Person {i}")).into(),
        },
        Triple {
            subject: data::person(i).into(),
            predicate: data::ex("age"),
            object: Literal::new_typed_literal((20 + i % 50).to_string(), xsd::INTEGER).into(),
        },
    ]
}

fn person_quads(i: usize, graph: &NamedNode) -> Vec<Quad> {
    person_triples(i)
        .into_iter()
        .map(|t| Quad {
            subject: t.subject,
            predicate: t.predicate,
            object: t.object,
            graph_name: GraphName::NamedNode(graph.clone()),
        })
        .collect()
}

// ---------------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------------

fn ms(d: Duration) -> String {
    let millis = d.as_secs_f64() * 1000.0;
    if millis < 1.0 {
        format!("{millis:.2}")
    } else if millis < 100.0 {
        format!("{millis:.1}")
    } else {
        format!("{millis:.0}")
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.0}", bytes as f64 / 1_048_576.0)
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn report(
    scales: &[usize],
    loads: &[(usize, LoadResult)],
    queries: &[(usize, Vec<QueryResult>)],
    holon: &HolonResults,
) {
    println!("\n## Load timings\n");
    println!("| People | Quads | Backend | Time | Rate | On disk | Dictionary |");
    println!("|---:|---:|---|---:|---:|---:|---:|");
    for (people, load) in loads {
        println!(
            "| {} | {} | {} | {:.2}s | {} quads/s | {} | {} |",
            thousands(*people as u64),
            thousands(load.quads),
            load.backend,
            load.elapsed.as_secs_f64(),
            thousands(load.rate() as u64),
            load.on_disk
                .map_or_else(|| "—".to_owned(), |b| format!("{} MB", mb(b))),
            thousands(load.dictionary as u64),
        );
    }

    let mismatches: Vec<(usize, &QueryResult)> = queries
        .iter()
        .flat_map(|(people, rs)| rs.iter().map(move |r| (*people, r)))
        .filter(|(_, r)| r.wrong.is_some())
        .collect();
    let holon_mismatches: Vec<&QueryResult> =
        holon.queries.iter().filter(|r| r.wrong.is_some()).collect();

    // Loud, and above the tables: a timing for a query that returned the wrong rows is not
    // a slow result, it is a meaningless one, and averaging it in with the rest would hide
    // that. The holarchy has a fixed shape, so most of these answers are arithmetic.
    if !mismatches.is_empty() || !holon_mismatches.is_empty() {
        println!("\n> **These timings are not trustworthy.** A query returned a different");
        println!("> number of rows than the dataset's fixed shape says it must, so it is");
        println!("> not measuring what its label claims.\n>");
        for (people, r) in &mismatches {
            println!(
                "> - `{}` at {} people — {}",
                r.label,
                thousands(*people as u64),
                r.wrong.as_deref().unwrap_or("")
            );
        }
        for r in &holon_mismatches {
            println!("> - `{}` — {}", r.label, r.wrong.as_deref().unwrap_or(""));
        }
        println!();
    }

    println!("\n## Query timings\n");
    println!("Median of three runs, in milliseconds, against the in-memory store.\n");

    print!("| Query | Group | Rows |");
    for people in scales {
        print!(" {} |", thousands(*people as u64));
    }
    println!();
    print!("|---|---|---:|");
    for _ in scales {
        print!("---:|");
    }
    println!();

    if let Some((_, first)) = queries.first() {
        for (i, case) in first.iter().enumerate() {
            print!(
                "| {} | {} | {} |",
                case.label,
                case.group.label(),
                thousands(case.rows as u64)
            );
            for (_, results) in queries {
                match results.get(i) {
                    Some(r) => print!(" {} |", ms(r.median)),
                    None => print!(" — |"),
                }
            }
            println!();
        }
    }

    println!("\n### What each query isolates\n");
    if let Some((_, first)) = queries.first() {
        let cases = queries::battery(1000);
        for (case, result) in cases.iter().zip(first) {
            println!(
                "- **{}** ({}) — {}",
                result.label,
                result.group.label(),
                case.tests
            );
        }
    }

    println!("\n## Holon timings\n");
    println!(
        "A scene of {} quads, a four-constraint boundary, {} commits.\n",
        thousands(holon.scene_quads as u64),
        holon.ticks
    );
    println!("| | Time |");
    println!("|---|---:|");
    println!(
        "| Full validation of the scene | {} ms |",
        ms(holon.full_validation)
    );
    println!(
        "| **One accepted commit** | **{} ms** |",
        ms(holon.accepted_tick)
    );
    println!("| One rejected commit | {} ms |", ms(holon.rejected_tick));
    let ratio = holon.full_validation.as_secs_f64() / holon.accepted_tick.as_secs_f64().max(1e-9);
    println!("| **Commit vs full pass** | **{ratio:.0}× cheaper** |");
    println!(
        "| Commits per second | {:.0} |",
        1.0 / holon.accepted_tick.as_secs_f64().max(1e-9)
    );

    println!("\n### Holonic queries\n");
    println!("| Query | Rows | Time |");
    println!("|---|---:|---:|");
    for case in &holon.queries {
        println!(
            "| {} | {} | {} ms |",
            case.label,
            thousands(case.rows as u64),
            ms(case.median)
        );
    }

    println!("\n#### What each holonic query isolates\n");
    let cases = queries::holon_battery(
        "urn:holon:people",
        "urn:holon:people/scene",
        "urn:holon:people/events",
        "",
        "",
        0,
    );
    for (case, result) in cases.iter().zip(&holon.queries) {
        println!("- **{}** — {}", result.label, case.tests);
    }
}
