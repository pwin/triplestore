//! Do bad estimates actually cost anything?
//!
//! `examples/estimator_accuracy.rs` shows the reused optimiser's estimates are wrong by
//! eight orders of magnitude. That is only worth fixing if it leads to worse *plans* — an
//! estimator can be badly calibrated and still rank alternatives correctly, in which case
//! the numbers are ugly and the plans are fine.
//!
//! So: run the same basic graph pattern with its triple patterns written in the best and
//! worst orders, and see whether the engine ends up doing the same work either way.
//!
//! ```text
//! cargo run --release -p holos-stats --example does_order_matter
//! ```

use holos_engine::Engine;
use holos_security::Session;
use oxrdf::vocab::rdf;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;
use std::time::Instant;

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let people: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(50_000);
    let orgs = 20;

    let mut engine = Engine::new();
    {
        let store = engine.store_mut();
        let mut add = |s: NamedNode, p: NamedNode, o: oxrdf::Term| {
            store
                .insert(
                    Quad {
                        subject: s.into(),
                        predicate: p,
                        object: o,
                        graph_name: GraphName::DefaultGraph,
                    }
                    .as_ref(),
                )
                .expect("insert");
        };
        for i in 0..people {
            let s = ex(&format!("person{i}"));
            add(s.clone(), rdf::TYPE.into_owned(), ex("Person").into());
            add(
                s.clone(),
                ex("name"),
                Literal::new_simple_literal(format!("P{i}")).into(),
            );
            add(
                s.clone(),
                ex("worksFor"),
                ex(&format!("org{}", i % orgs)).into(),
            );
            // One person in the whole dataset has this. A plan that starts here does
            // almost no work; a plan that starts anywhere else does a great deal.
            if i == 0 {
                add(
                    s,
                    ex("badgeNumber"),
                    Literal::new_simple_literal("0001").into(),
                );
            }
        }
        for i in 0..orgs {
            add(
                ex(&format!("org{i}")),
                ex("country"),
                Literal::new_simple_literal("GB").into(),
            );
        }
    }

    let session = Session::unrestricted(engine.store())?;
    let p = "PREFIX ex: <http://example.com/> PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ";

    // The same answer set, written two ways.
    let selective_first = format!(
        "{p} SELECT * WHERE {{ ?s ex:badgeNumber ?b . ?s rdf:type ex:Person . ?s ex:name ?n . ?s ex:worksFor ?o . ?o ex:country ?c }}"
    );
    let selective_last = format!(
        "{p} SELECT * WHERE {{ ?s rdf:type ex:Person . ?s ex:name ?n . ?s ex:worksFor ?o . ?o ex:country ?c . ?s ex:badgeNumber ?b }}"
    );

    let run = |sparql: &str| -> Result<(usize, f64), Box<dyn std::error::Error>> {
        let view = engine.view(&session);
        let started = Instant::now();
        let results = Engine::query(&view, sparql, None)?;
        let n = match results {
            QueryResults::Solutions(iter) => iter.count(),
            _ => 0,
        };
        Ok((n, started.elapsed().as_secs_f64()))
    };

    // Warm both paths so the comparison is not measuring first-touch effects.
    let _ = run(&selective_first)?;
    let _ = run(&selective_last)?;

    let (rows_a, time_a) = run(&selective_first)?;
    let (rows_b, time_b) = run(&selective_last)?;

    println!(
        "{} triples, {} subjects",
        engine.store().len(),
        people + orgs
    );
    println!("both queries return the same answers: {}", rows_a == rows_b);
    println!();
    println!("most selective pattern written first   {rows_a} rows in {time_a:.4}s");
    println!("most selective pattern written last    {rows_b} rows in {time_b:.4}s");
    let ratio = time_b.max(time_a) / time_a.min(time_b).max(f64::MIN_POSITIVE);
    println!();
    if ratio < 1.5 {
        println!("ratio {ratio:.2}x — the optimiser reordered them to the same plan, so its");
        println!("mis-calibration is not costing anything on this shape.");
    } else {
        println!("ratio {ratio:.1}x — written order survives into the plan, so an estimator");
        println!("that cannot tell a 1-row pattern from a 50,000-row one is choosing badly.");
    }
    Ok(())
}
