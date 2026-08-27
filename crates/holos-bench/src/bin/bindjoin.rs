//! What would an index nested-loop join be worth?
//!
//! The plan for a star query is `LeftJoin(HashBuildLeftProbeRight, keys = ?s)`: it builds a
//! hash table from the left input and **scans the right input in full** to probe it. So even
//! when the left is 200 rows, the right is still a complete scan of a million.
//!
//! A triplestore should not do that. Once `?s` is bound to 200 values, the other patterns
//! are 200 prefix lookups into the `spo` index — work proportional to the *answer*, not to
//! the data. That is a bind join, and the reused evaluator has no such operator.
//!
//! This measures what it would be worth, by doing it by hand against the same store.
//!
//! ```text
//! cargo run --release -p holos-bench --bin bindjoin
//! ```

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::{GraphFilter, Store};
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use std::sync::Arc;
use std::time::Instant;

const EX: &str = "http://holos.example/";

fn iri(local: &str) -> oxrdf::NamedNode {
    oxrdf::NamedNode::new_unchecked(format!("{EX}{local}"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("holos-bench")
            .join("bench-100000.nt")
            .to_string_lossy()
            .into_owned()
    });
    let limit = 20_usize;

    let mut engine = Engine::new();
    let file = std::fs::File::open(&path)?;
    let n = engine.bulk_load(std::io::BufReader::new(file), RdfFormat::NTriples, None)?;
    println!("{n} quads\n");

    let store: &Store = engine.store();
    let session = Session::unrestricted(store)?;
    let stats = Arc::new(Statistics::build(store, GraphFilter::Default)?);

    let sparql = format!(
        "PREFIX ex: <{EX}> SELECT ?n ?u WHERE {{ \
         ?s ex:badgeNumber ?b . ?s ex:name ?n . ?s ex:memberOf ?u }} LIMIT {limit}"
    );

    // --- 1. the evaluator, best case: statistics applied ------------------
    let options = QueryOptions::new().reordering(Arc::clone(&stats));
    let started = Instant::now();
    let rows = {
        let view = engine.view(&session);
        let (results, _) = Engine::query_with(&view, &sparql, &options)?;
        match results {
            QueryResults::Solutions(iter) => iter.filter(Result::is_ok).count(),
            _ => 0,
        }
    };
    let evaluator = started.elapsed();
    println!(
        "query path (bind join)         {rows:>3} rows  {:>9.3} ms",
        evaluator.as_secs_f64() * 1000.0
    );

    // --- 2. the same answers, as a hand-written bind join -----------------
    //
    // Scan the selective pattern, then for each subject it yields, probe the other two by
    // (subject, predicate) prefix. Stop at the limit. This is what the plan above would do
    // if the evaluator had an index nested-loop operator.
    let badge = store
        .lookup_term(iri("badgeNumber").as_ref().into())?
        .ok_or("badgeNumber not interned")?;
    let name = store
        .lookup_term(iri("name").as_ref().into())?
        .ok_or("name not interned")?;
    let member = store
        .lookup_term(iri("memberOf").as_ref().into())?
        .ok_or("memberOf not interned")?;

    let started = Instant::now();
    let mut found = 0_usize;
    let mut probes = 0_u64;
    'outer: for quad in store.quads_for_pattern(None, Some(badge), None, GraphFilter::Default) {
        let subject = quad?.subject;
        // Probe: (subject, name, *) and (subject, memberOf, *).
        for name_quad in
            store.quads_for_pattern(Some(subject), Some(name), None, GraphFilter::Default)
        {
            let name_quad = name_quad?;
            probes += 1;
            for member_quad in
                store.quads_for_pattern(Some(subject), Some(member), None, GraphFilter::Default)
            {
                let member_quad = member_quad?;
                probes += 1;
                // Decode only what is returned — the whole point of a bind join is that
                // this happens `limit` times, not a million times.
                let _ = store.decode_term(name_quad.object)?;
                let _ = store.decode_term(member_quad.object)?;
                found += 1;
                if found >= limit {
                    break 'outer;
                }
            }
        }
    }
    let bind = started.elapsed();
    println!(
        "hand-written bind join         {found:>3} rows  {:>9.3} ms   ({probes} index probes)",
        bind.as_secs_f64() * 1000.0
    );

    println!(
        "\n{:.0}x — what a hand-written join still beats the general one by here.",
        evaluator.as_secs_f64() / bind.as_secs_f64().max(1e-9)
    );
    println!(
        "The first row goes through `holos_engine::bindjoin`, which probes rather than \n\
         scans and touches roughly the same {probes} keys. What separates the two is the \n\
         cost of being general — choosing the next pattern from statistics at each step, \n\
         hashing bindings, decoding terms — rather than the absence of an operator, which \n\
         is what this benchmark showed at 611x before one existed."
    );

    Ok(())
}
