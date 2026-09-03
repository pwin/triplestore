//! Does reordering the basic graph pattern actually make queries faster?
//!
//! The estimator was measured at a mean q-error of 1.1 against the reused optimiser's
//! 2×10⁸, and the penalty for a badly ordered query was measured at 13× — but neither
//! establishes that applying the one to the other helps. `sparopt` runs its own optimiser
//! over whatever it is given, and could in principle undo the reordering.
//!
//! So: the same queries, with and without, over the same store.
//!
//! ```text
//! cargo run --release -p holos-bench --bin reorder
//! ```

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use std::sync::Arc;
use std::time::{Duration, Instant};

const EX: &str = "http://holos.example/";

fn run(
    engine: &Engine,
    session: &Session,
    sparql: &str,
    options: &QueryOptions,
) -> (usize, Duration) {
    let mut best = Duration::MAX;
    let mut rows = 0;
    for _ in 0..3 {
        let view = engine.view(session);
        let started = Instant::now();
        let (results, _) = Engine::query_with(&view, sparql, options).expect("query");
        rows = match results {
            QueryResults::Solutions(iter) => iter.filter(Result::is_ok).count(),
            _ => 0,
        };
        best = best.min(started.elapsed());
    }
    (rows, best)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("holos-bench")
            .join("bench-100000.nt")
            .to_string_lossy()
            .into_owned()
    });

    let mut engine = Engine::new();
    let file = std::fs::File::open(&path)?;
    let n = engine.bulk_load(std::io::BufReader::new(file), RdfFormat::NTriples, None)?;

    let started = Instant::now();
    let stats = Arc::new(Statistics::build(engine.store(), GraphFilter::Default)?);
    let build = started.elapsed();
    println!(
        "{n} quads; statistics built in {:.3}s over {} distinct subject shapes\n",
        build.as_secs_f64(),
        stats.shape_count()
    );

    let session = Session::unrestricted(engine.store())?;
    let plain = QueryOptions::new();
    let reordered = QueryOptions::new().reordering(Arc::clone(&stats));

    let p = format!("PREFIX ex: <{EX}> PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ");
    let cases: Vec<(&str, String)> = vec![
        (
            "selective join, written badly",
            format!("{p} SELECT ?n ?u WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?u . ?s ex:badgeNumber ?b }} LIMIT 20"),
        ),
        (
            "selective join, written well",
            format!("{p} SELECT ?n ?u WHERE {{ ?s ex:badgeNumber ?b . ?s ex:name ?n . ?s ex:memberOf ?u }} LIMIT 20"),
        ),
        (
            "4-way star, worst order",
            format!("{p} SELECT * WHERE {{ ?s ex:name ?n . ?s ex:age ?a . ?s ex:memberOf ?u . ?s ex:badgeNumber ?b }} LIMIT 20"),
        ),
        (
            "type + rare predicate",
            format!("{p} SELECT ?n WHERE {{ ?s rdf:type ex:Person . ?s ex:name ?n . ?s ex:badgeNumber ?b }} LIMIT 20"),
        ),
        (
            "3-way star, no rare arm",
            format!("{p} SELECT ?n ?a ?u WHERE {{ ?s ex:name ?n . ?s ex:age ?a . ?s ex:memberOf ?u }} LIMIT 100"),
        ),
        (
            "2-hop, unanchored",
            format!("{p} SELECT ?an ?bn WHERE {{ ?a ex:knows ?b . ?a ex:name ?an . ?b ex:name ?bn }} LIMIT 20"),
        ),
    ];

    println!(
        "{:<32} {:>6} {:>12} {:>12} {:>10}",
        "query", "rows", "as written", "reordered", "change"
    );
    println!("{}", "-".repeat(76));

    let mut total_plain = 0.0;
    let mut total_reordered = 0.0;

    for (label, sparql) in &cases {
        let (rows_a, plain_time) = run(&engine, &session, sparql, &plain);
        let (rows_b, fast_time) = run(&engine, &session, sparql, &reordered);
        assert_eq!(
            rows_a, rows_b,
            "{label}: reordering changed the answer — {rows_a} rows became {rows_b}"
        );

        let a = plain_time.as_secs_f64() * 1000.0;
        let b = fast_time.as_secs_f64() * 1000.0;
        total_plain += a;
        total_reordered += b;

        let change = if b < a {
            format!("{:.1}x faster", a / b.max(1e-9))
        } else if a / b > 0.95 {
            "—".to_owned()
        } else {
            format!("{:.1}x SLOWER", b / a.max(1e-9))
        };
        println!("{label:<32} {rows_a:>6} {a:>10.1}ms {b:>10.1}ms {change:>10}");
    }

    println!("{}", "-".repeat(76));
    println!(
        "{:<32} {:>6} {:>10.1}ms {:>10.1}ms {:>10}",
        "total",
        "",
        total_plain,
        total_reordered,
        format!("{:.1}x", total_plain / total_reordered.max(1e-9))
    );

    println!(
        "\nStatistics cost {:.3}s to build and are reusable across queries; a query that \
         saves more than that pays for them immediately.",
        build.as_secs_f64()
    );
    Ok(())
}
