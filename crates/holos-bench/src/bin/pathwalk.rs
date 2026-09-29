//! `ex:memberOf/ex:partOf*` — the query `BENCHMARKS.md` §3 measured at 25 seconds for five
//! rows, timed with the closure walk and without it.
//!
//! The shape is a holarchy of 341 units in a tree, people attached to leaves, and filler
//! triples so the store is large enough for the pathology to show. The pathology is that a
//! closure whose left side is a *variable* makes every term in the store a candidate start,
//! because a zero-length path connects everything to itself — so the cost tracks the dataset
//! rather than the 341 units actually walked.
//!
//! Both arms in one process over one store, so nothing differs but which operator answers:
//! `without_bind_join` forces the evaluator, which is what this did before the walk existed.
//!
//! ```text
//! pathwalk [filler quads]
//! ```

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdf::{GraphName, NamedNode, Quad};
use spareval::QueryResults;
use std::time::Instant;

const EX: &str = "http://holos.example/";

/// 341 units: a root, 20 children, 320 grandchildren. The same shape as the benchmark's.
const UNITS: usize = 341;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filler: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(500_000);

    let mut engine = Engine::new();
    let node = |s: String| NamedNode::new_unchecked(s);
    let mut quad = |s: NamedNode, p: &str, o: NamedNode| {
        engine
            .store_mut()
            .insert(
                Quad {
                    subject: s.into(),
                    predicate: NamedNode::new_unchecked(format!("{EX}{p}")),
                    object: o.into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    };

    // The tree: unit0 is the root, 1..21 are its children, the rest hang off those.
    for i in 1..UNITS {
        let parent = if i <= 20 { 0 } else { 1 + (i - 21) % 20 };
        quad(
            node(format!("{EX}unit{i}")),
            "partOf",
            node(format!("{EX}unit{parent}")),
        );
    }
    // One person on a deep leaf — the subject the benchmark uses.
    quad(
        node(format!("{EX}p1000")),
        "memberOf",
        node(format!("{EX}unit340")),
    );
    // Filler, so the store is big enough for an unanchored closure to be expensive.
    for i in 0..filler {
        quad(
            node(format!("{EX}filler{i}")),
            "mentions",
            node(format!("{EX}topic{}", i % 1000)),
        );
    }
    println!("{} quads, {UNITS} units\n", engine.store().len());

    let session = Session::unrestricted(engine.store())?;
    let queries = [
        (
            "memberOf/partOf*  (closure's left side is a variable)",
            format!("PREFIX ex: <{EX}> SELECT ?u WHERE {{ <{EX}p1000> ex:memberOf/ex:partOf* ?u }}"),
        ),
        (
            "unit340 partOf+   (anchored, was already fast)",
            format!("PREFIX ex: <{EX}> SELECT ?a WHERE {{ <{EX}unit340> ex:partOf+ ?a }}"),
        ),
        (
            "unit0 ^partOf+    (anchored inverse, was already fast)",
            format!("PREFIX ex: <{EX}> SELECT ?d WHERE {{ <{EX}unit0> ^ex:partOf+ ?d }}"),
        ),
    ];

    for (label, sparql) in &queries {
        let run = |options: &QueryOptions| -> Result<(usize, f64), Box<dyn std::error::Error>> {
            let view = engine.view(&session);
            let started = Instant::now();
            let (results, _) = Engine::query_with(&view, sparql, options)?;
            let QueryResults::Solutions(iter) = results else {
                return Err("solutions expected".into());
            };
            let mut n = 0;
            for row in iter {
                row?;
                n += 1;
            }
            Ok((n, started.elapsed().as_secs_f64()))
        };
        let (walked, walk_s) = run(&QueryOptions::new())?;
        let (evaluated, eval_s) = run(&QueryOptions::new().without_bind_join())?;
        assert_eq!(walked, evaluated, "the two paths must agree: {label}");
        println!("{label}");
        println!("  walk        {walk_s:9.4} s   {walked} rows");
        println!("  evaluator   {eval_s:9.4} s   {evaluated} rows");
        if walk_s > 0.0 {
            println!("  ratio       {:9.1}×\n", eval_s / walk_s);
        }
    }
    Ok(())
}
