//! What a range pushdown is worth to a *query*, rather than to a scan.
//!
//! `rangescan` measures the primitive: a bounded read against a full read plus a test. That
//! number is large — 226× in memory at 1% selectivity — and it is not the number a user
//! sees, because a query also decodes terms, builds solutions and serialises them. Quoting
//! the primitive's figure for the feature would overclaim.
//!
//! So this times whole queries. It is meant to be run either side of the change that wires
//! the pushdown into the bind join: check out the parent commit, run it, check out the
//! child, run it again. There is no runtime switch to compare against, on purpose — a
//! switch would be a second code path to keep correct for the sake of a benchmark.
//!
//! # What it found
//!
//! Measured against the same build with the pushdown disabled, so the comparison owes
//! nothing to the machine:
//!
//! ```text
//! selectivity      off        on   speed-up
//!          1%   397 ms    5.7 ms       69x
//!         10%   410 ms     61 ms      6.7x
//!         50%   495 ms    314 ms      1.6x
//!         90%   627 ms    540 ms      1.2x
//! ```
//!
//! It also found a regression that `rangescan` could not: the first wiring boxed *both*
//! scan branches, putting dynamic dispatch on every scan the operator makes rather than on
//! the bounded ones. At 90% selectivity that made a bounded query **slower** than no
//! pushdown at all. Measuring the primitive says nothing about that, which is the argument
//! for this file existing alongside the other one.
//!
//! ```text
//! cargo run --release -p holos-bench --bin rangequery
//! ```

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::{GraphFilter, Store};
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;
use std::sync::Arc;
use std::time::{Duration, Instant};

const EX: &str = "http://holos.example/";
const QUADS: usize = 400_000;

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn engine() -> Engine {
    let mut store = Store::new();
    for i in 0..QUADS {
        let subject = ex(&format!("s{i}"));
        store
            .insert(
                Quad {
                    subject: subject.clone().into(),
                    predicate: ex("age"),
                    object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
        store
            .insert(
                Quad {
                    subject: subject.into(),
                    predicate: ex("name"),
                    object: Literal::new_simple_literal(format!("n{i}")).into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    Engine::with_store(store)
}

/// The faster of three runs, and how many rows came back.
fn best(engine: &Engine, query: &str, options: &QueryOptions) -> (Duration, usize) {
    let mut best = Duration::MAX;
    let mut rows = 0;
    for _ in 0..3 {
        let session = Session::unrestricted(engine.store()).expect("session");
        let view = engine.view(&session);
        let started = Instant::now();
        let (results, _) = Engine::query_with(&view, query, options).expect("query");
        rows = match results {
            QueryResults::Solutions(iter) => iter.count(),
            _ => 0,
        };
        best = best.min(started.elapsed());
    }
    (best, rows)
}

fn main() {
    let engine = engine();
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let options = QueryOptions::new().reordering(stats);

    println!("{QUADS} subjects, one `age` and one `name` each\n");
    println!(
        "{:>12}  {:>10}  {:>14}  {:>16}",
        "selectivity", "rows", "whole query", "bounded scans"
    );

    for percent in [1, 5, 10, 25, 50, 90] {
        let cut = QUADS - QUADS * percent / 100;
        let query =
            format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:age ?o FILTER(?o > {cut}) }}");
        let (elapsed, rows) = best(&engine, &query, &options);

        // Whether the pushdown fired at all, so a run against a build without it is
        // distinguishable from a run where the query simply did not qualify.
        let session = Session::unrestricted(engine.store()).expect("session");
        let view = engine.view(&session);
        let _ = Engine::query_with(&view, &query, &options).expect("query");
        println!(
            "{:>11}%  {:>10}  {:>11.2} ms  {:>16}",
            percent,
            rows,
            elapsed.as_secs_f64() * 1e3,
            view.bounded_scans()
        );
    }

    println!();
    println!("Compare against the same binary built from the parent commit. A query does more");
    println!("than scan — it decodes terms and builds solutions — so this is smaller than the");
    println!("primitive's figure in `rangescan`, and it is the one a user would notice.");
}
