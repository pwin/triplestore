//! Did the range pushdown fire on *this* store?
//!
//! `rangequery` answers that for a store it builds itself, 400,000 subjects, and the answer
//! there is yes on both backends. On a 653.8-million-triple store the same query shape —
//! `SELECT ?s WHERE { ?s <p> ?o FILTER(?o > k) }`, integer objects, 0.55% selectivity — took
//! **2m44s**, which is a full scan wearing a selectivity label. Something differs between the
//! two and the timing alone cannot say what.
//!
//! `DatasetView::bounded_scans` counts the scans that were given a span, and it is the one
//! number that separates "the pushdown fired and was not worth it" from "the pushdown never
//! fired". The CLI does not expose it, so this does.
//!
//! ```text
//! cargo run --release -p holos-bench --bin pushdown -- <store-dir> <predicate-iri> <cut>
//! ```
//!
//! For example, against the store this was written for:
//!
//! ```text
//! cargo run --release -p holos-bench --bin pushdown -- \
//!     E:/store http://schema.org/postalCode 99000
//! ```
//!
//! Statistics are off by default and `--reorder` turns them on, because building them costs a
//! full scan and the question is whether they are what the pushdown depends on.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::{GraphFilter, Store};
use spareval::QueryResults;
use std::sync::Arc;
use std::time::Instant;

#[cfg(feature = "rocksdb")]
use holos_store::RocksStorage;

#[cfg(not(feature = "rocksdb"))]
fn main() {
    eprintln!("pushdown needs the rocksdb feature");
}

#[cfg(feature = "rocksdb")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let reorder = args.iter().any(|a| a == "--reorder");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if positional.len() < 3 {
        return Err("usage: pushdown <store-dir> <predicate-iri> <cut> [--reorder]".into());
    }
    let (dir, predicate, cut) = (positional[0], positional[1], positional[2]);

    let opening = Instant::now();
    let engine = Engine::with_store(Store::with_storage(RocksStorage::open(dir)?));
    println!("store opened in {:.1}s", opening.elapsed().as_secs_f64());

    let mut options = QueryOptions::new();
    if reorder {
        let building = Instant::now();
        let stats = Arc::new(Statistics::build(engine.store(), GraphFilter::Default)?);
        println!(
            "statistics built in {:.1}s",
            building.elapsed().as_secs_f64()
        );
        options = options.reordering(stats);
    }

    let query = format!("SELECT ?s WHERE {{ ?s <{predicate}> ?o FILTER(?o > {cut}) }}");
    println!("{query}");

    let session = Session::unrestricted(engine.store())?;
    let view = engine.view(&session);
    let started = Instant::now();
    let mut rows = 0u64;
    let (results, _) = Engine::query_with(&view, &query, &options)?;
    if let QueryResults::Solutions(solutions) = results {
        for solution in solutions {
            solution?;
            rows += 1;
        }
    }

    // The number this exists for. Zero means every scan read the whole predicate.
    println!(
        "rows={rows} in {:.1}s | bounded scans = {}",
        started.elapsed().as_secs_f64(),
        view.bounded_scans()
    );
    Ok(())
}
