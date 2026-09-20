//! Does a batched dictionary lookup beat one read per term?
//!
//! The question a bulk load's remaining read cost turns on. After the seen filter, the hot
//! set and the whole filter on `str2id`, what is left is one block read per term that
//! recurs across windows — 43 million of them at 16 µs, 700 s of a 56-minute load. RocksDB's
//! `MultiGet` answers a batch of keys in one call, walking each file once for the whole batch
//! rather than once per key. Whether that is cheaper *here*, on this platform, with this
//! dictionary, is what this measures — before the load is rebuilt around batching, which
//! would mean serialising a batch of terms ahead of interning them, and is only worth doing
//! for a real number.
//!
//! The terms come from the front of the file the store was loaded from: they exist, their
//! blocks were not touched by the load's final phase, and they spread across the key space
//! the way the load's own lookups do. Lookups alternate between the two methods in chunks,
//! so drift in the page cache or the disk over the run lands on both alike.
//!
//! ```text
//! dictget <store> <file> [terms] [batch]
//! ```

use holos_store::Store;
use oxrdf::{Quad, Term, TermRef};
use oxrdfio::{RdfFormat, RdfParser};
use std::io::BufReader;
use std::time::{Duration, Instant};

use holos_store::RocksStorage;

fn format_of(path: &str) -> RdfFormat {
    match path.rsplit('.').next().unwrap_or("") {
        "ttl" => RdfFormat::Turtle,
        "trig" => RdfFormat::TriG,
        "nq" => RdfFormat::NQuads,
        _ => RdfFormat::NTriples,
    }
}

/// The first `n` distinct non-inline terms of the file, in the order they appear.
fn first_terms(path: &str, n: usize) -> Result<Vec<Term>, Box<dyn std::error::Error>> {
    let mut seen = rustc_hash::FxHashSet::default();
    let mut terms = Vec::with_capacity(n);
    let reader = BufReader::new(std::fs::File::open(path)?);
    for quad in RdfParser::from_format(format_of(path)).for_reader(reader) {
        let quad: Quad = quad?;
        for term in [
            Term::from(quad.subject.clone()),
            Term::from(quad.predicate.clone()),
            quad.object.clone(),
        ] {
            // Predicates and classes recur every line and would be answered from the block
            // cache after the first time; the terms that cost a read each are the ones
            // that appear once. Keep everything distinct and let the mix be the file's.
            if seen.insert(term.clone()) {
                terms.push(term);
                if terms.len() == n {
                    return Ok(terms);
                }
            }
        }
    }
    Ok(terms)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let store_path = args.next().expect("dictget <store> <file> [terms] [batch]");
    let file = args.next().expect("dictget <store> <file> [terms] [batch]");
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200_000);
    let batch: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(256);

    let terms = first_terms(&file, n)?;
    println!("{} terms from the front of {file}", terms.len());
    let store = Store::with_storage(RocksStorage::open(&store_path)?);

    // Chunks alternate: even ones are read one at a time, odd ones as a batch. Both sides
    // then see the same mix of term kinds and the same moment in the run.
    let mut single = (0usize, 0usize, Duration::ZERO);
    let mut batched = (0usize, 0usize, Duration::ZERO);
    for (i, chunk) in terms.chunks(batch).enumerate() {
        let refs: Vec<TermRef<'_>> = chunk.iter().map(Term::as_ref).collect();
        if i % 2 == 0 {
            let started = Instant::now();
            let mut found = 0;
            for term in &refs {
                found += usize::from(store.lookup_term(*term)?.is_some());
            }
            single = (
                single.0 + refs.len(),
                single.1 + found,
                single.2 + started.elapsed(),
            );
        } else {
            let started = Instant::now();
            let found = store
                .lookup_terms(&refs)?
                .iter()
                .filter(|a| a.is_some())
                .count();
            batched = (
                batched.0 + refs.len(),
                batched.1 + found,
                batched.2 + started.elapsed(),
            );
        }
    }
    for (label, (asked, found, took)) in [("one get per term", single), ("multiget", batched)] {
        println!(
            "{label:<18} {asked:>8} terms, {found:>8} found, {:>8.2} s, {:>6.1} us/term",
            took.as_secs_f64(),
            took.as_secs_f64() * 1e6 / asked as f64
        );
    }
    println!("batch size {batch}");
    Ok(())
}
