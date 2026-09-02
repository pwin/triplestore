//! What the bind join is worth on `OPTIONAL`, which is where most real queries live.
//!
//! The operator's advantage is doing work proportional to the answer rather than to the
//! store, and `OPTIONAL` is where that ought to tell hardest: a left join over a selective
//! left side probes the optional per row, while a hash join builds the whole right side
//! first and then discovers most of it was never needed.
//!
//! Both columns are the same evaluator over the same store under the same policy. The only
//! difference is which operator answered, which is what makes the comparison attributable.
//!
//! Run with `cargo run -p holos-bench --release --bin bindopt`.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use std::sync::Arc;
use std::time::Instant;

const EX: &str = "http://example.com/";

/// `n` people, a tenth of whom have a nickname — so the optional mostly misses, which is the
/// common shape and the one a hash join wastes the most work on.
fn dataset(n: usize) -> String {
    let mut s = String::with_capacity(n * 110);
    s.push_str("@prefix ex: <http://example.com/> .\n");
    for i in 0..n {
        s.push_str(&format!(
            "ex:p{i} a ex:Person ; ex:name \"P{i}\" ; ex:badge {i} ; ex:memberOf ex:u{} .\n",
            i % 50
        ));
        if i % 10 == 0 {
            s.push_str(&format!("ex:p{i} ex:nickname \"N{i}\" .\n"));
        }
    }
    s
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{:>9}  {:>10}  {:>14}  {:>14}  {:>8}",
        "quads", "rows", "evaluator", "bind join", "speedup"
    );

    for n in [10_000usize, 100_000] {
        let mut engine = Engine::new();
        engine.bulk_load(dataset(n).as_bytes(), RdfFormat::Turtle, None)?;
        let store = engine.store();
        let session = Session::unrestricted(store)?;
        let stats = Arc::new(Statistics::build(store, GraphFilter::Default)?);
        let quads = store.len();

        // A selective left side — one unit out of fifty — with an optional hanging off it.
        // The `LIMIT` is what makes "proportional to the answer" mean anything.
        let sparql = format!(
            "PREFIX ex: <{EX}> SELECT ?s ?name ?nick WHERE {{ \
             ?s ex:memberOf ex:u7 . ?s ex:name ?name . \
             OPTIONAL {{ ?s ex:nickname ?nick }} }} LIMIT 20"
        );
        let parsed = spargebra::SparqlParser::new().parse_query(&sparql)?;
        assert!(
            holos_engine::bindjoin::plan(&parsed).is_some(),
            "the fragment must take this, or the comparison is of one operator with itself"
        );

        let count = |results: QueryResults<'_>| match results {
            QueryResults::Solutions(iter) => iter.filter(Result::is_ok).count(),
            _ => 0,
        };

        let options = QueryOptions::new().reordering(Arc::clone(&stats));
        let mut rows = 0;
        let ours = median(
            (0..5)
                .map(|_| {
                    let view = engine.view(&session);
                    let started = Instant::now();
                    let (results, _) = Engine::query_with(&view, &sparql, &options).expect("query");
                    rows = count(results);
                    started.elapsed().as_secs_f64() * 1e3
                })
                .collect(),
        );

        let reference = median(
            (0..5)
                .map(|_| {
                    let view = engine.view(&session);
                    let started = Instant::now();
                    let results = Engine::evaluator()
                        .prepare(&parsed)
                        .execute(&view)
                        .expect("reference");
                    let n = count(results);
                    assert_eq!(
                        n, rows,
                        "the two operators must agree before either is timed"
                    );
                    started.elapsed().as_secs_f64() * 1e3
                })
                .collect(),
        );

        println!(
            "{quads:>9}  {rows:>10}  {reference:>11.3} ms  {ours:>11.3} ms  {:>7.1}x",
            reference / ours.max(1e-9)
        );
    }

    println!();
    println!(
        "The optional misses nine times in ten, which is the shape that separates the two: a\n\
         hash join materialises the whole right side to discover it, an index nested-loop\n\
         probes per row and stops at the LIMIT. The counts are asserted equal before timing,\n\
         so a speedup can never come from answering less."
    );
    Ok(())
}
