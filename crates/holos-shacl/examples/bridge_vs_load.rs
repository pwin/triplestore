//! Does feeding the engine from a populated store actually beat parsing the file?
//!
//! `DESIGN.md` §8 rests on the claim that a validator sharing the store's data does not
//! pay the load cost again. The vendored engine still needs its own `Graph`, so the claim
//! reduces to a measurable question: is bridging from the store cheaper than the engine's
//! own loader, and by how much?
//!
//! ```text
//! cargo run --release -p holos-shacl --example bridge_vs_load -- <data.nt>
//! ```

use holos_shacl::bridge;
use holos_shacl_engine::model::{loader, TermStore};
use holos_store::{GraphFilter, Store};
use oxrdfio::{RdfFormat, RdfParser};
use std::io::BufReader;
use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: bridge_vs_load <data.nt>")?;

    // (1) The engine's own loader: parse the file, intern, sort.
    let started = Instant::now();
    let mut engine_terms = TermStore::new();
    let engine_graph = loader::load_file(Path::new(&path), 0, &mut engine_terms)?;
    let engine_load = started.elapsed();

    // (2) HOLOS loads the same file once, as any triplestore would.
    let started = Instant::now();
    let mut store = Store::new();
    for quad in RdfParser::from_format(RdfFormat::NTriples)
        .for_reader(BufReader::new(std::fs::File::open(&path)?))
    {
        store.insert(quad?.as_ref())?;
    }
    let store_load = started.elapsed();

    // (3) The bridge: build the engine's graph from the populated store, no parsing.
    let started = Instant::now();
    let bridged = bridge::bridge(&store, GraphFilter::Default)?;
    let bridge_time = started.elapsed();

    println!("triples                     {}", engine_graph.len());
    println!("engine terms (own loader)   {}", engine_terms.len());
    println!("engine terms (bridged)      {}", bridged.terms.len());
    println!();
    println!("engine loads the file       {:.3}s", engine_load.as_secs_f64());
    println!("bridge from a live store    {:.3}s", bridge_time.as_secs_f64());
    println!(
        "                            {:.1}x cheaper",
        engine_load.as_secs_f64() / bridge_time.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!();
    println!("(for context, HOLOS's own load of the same file: {:.3}s — paid once, and", store_load.as_secs_f64());
    println!(" shared with the query engine and the policy layer rather than by SHACL alone)");

    assert_eq!(
        engine_graph.len(),
        bridged.len(),
        "the two paths must produce the same number of triples"
    );
    Ok(())
}
