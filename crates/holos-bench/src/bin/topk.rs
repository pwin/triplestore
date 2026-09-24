//! What `ORDER BY … LIMIT` costs from the heap against the evaluator's full sort.
//!
//! The query is the one `holos_engine::topk` was written for — the latest few dates on a
//! predicate — over an in-memory store of `n` people with an `xsd:date` each, so the
//! difference measured is the operator's and not the disk's. The evaluator's path is
//! reached by asking for an explanation, which the heap declines; the answer is checked to
//! be the same both ways.
//!
//! ```text
//! topk [rows] [limit]
//! ```

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;
use std::time::Instant;

const EX: &str = "http://holos.example/";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows: usize = std::env::args()
        .nth(1)
        .map_or(Ok(1_000_000), |s| s.parse())?;
    let limit: usize = std::env::args().nth(2).map_or(Ok(4), |s| s.parse())?;

    let mut engine = Engine::new();
    let predicate = NamedNode::new_unchecked(format!("{EX}deathDate"));
    // A linear congruential walk, so the dates arrive in no useful order.
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    for i in 0..rows {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let day = (seed >> 33) % 73_000; // two hundred years of days
        let year = 1800 + day / 365;
        let month = 1 + (day % 365) / 31;
        let dom = 1 + (day % 365) % 28;
        engine.store_mut().insert(
            Quad {
                subject: NamedNode::new_unchecked(format!("{EX}p{i}")).into(),
                predicate: predicate.clone(),
                object: Literal::new_typed_literal(
                    format!("{year:04}-{month:02}-{dom:02}"),
                    xsd::DATE,
                )
                .into(),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )?;
    }
    println!("{rows} rows, LIMIT {limit}\n");

    let session = Session::unrestricted(engine.store())?;
    let sparql = format!(
        "PREFIX ex: <{EX}> SELECT ?o WHERE {{ ?s ex:deathDate ?o }} ORDER BY DESC(?o) LIMIT {limit}"
    );

    let run = |options: &QueryOptions| -> Result<(Vec<String>, f64), Box<dyn std::error::Error>> {
        let view = engine.view(&session);
        let started = Instant::now();
        let (results, _) = Engine::query_with(&view, &sparql, options)?;
        let QueryResults::Solutions(iter) = results else {
            return Err("solutions expected".into());
        };
        let mut out = Vec::new();
        for solution in iter {
            let solution = solution?;
            out.push(solution.get(0).map(ToString::to_string).unwrap_or_default());
        }
        Ok((out, started.elapsed().as_secs_f64()))
    };

    let (heap, heap_s) = run(&QueryOptions::new())?;
    println!("heap            {heap_s:8.3} s   {}", heap.join(" "));
    let (_, no_join_s) = run(&QueryOptions::new().without_bind_join())?;
    println!("heap, evaluator {no_join_s:8.3} s");
    let (full, full_s) = run(&QueryOptions::new().explaining())?;
    println!("evaluator sort  {full_s:8.3} s   {}", full.join(" "));
    assert_eq!(heap, full, "the two paths must agree");
    println!("\nratio {:.1}×", full_s / heap_s);

    // And the same sort with no LIMIT, which the heap declines: every row comes back, so
    // this is the collector against the evaluator's buffer rather than against the heap.
    let whole = format!("PREFIX ex: <{EX}> SELECT ?o WHERE {{ ?s ex:deathDate ?o }} ORDER BY ?o");
    let count = |options: &QueryOptions| -> Result<(usize, f64), Box<dyn std::error::Error>> {
        let view = engine.view(&session);
        let started = Instant::now();
        let (results, _) = Engine::query_with(&view, &whole, options)?;
        let QueryResults::Solutions(iter) = results else {
            return Err("solutions expected".into());
        };
        let mut n = 0;
        let mut last: Option<String> = None;
        for solution in iter {
            let value = solution?.get(0).map(ToString::to_string).unwrap_or_default();
            if let Some(previous) = &last {
                assert!(*previous <= value, "out of order at row {n}");
            }
            last = Some(value);
            n += 1;
        }
        Ok((n, started.elapsed().as_secs_f64()))
    };

    println!("\nORDER BY with no LIMIT, every row out");
    // A budget the sort overruns many times over, so the runs are real.
    let (spilled_n, spilled_s) = count(&QueryOptions::new().spilling(8 << 20))?;
    println!("spilling 8 MiB  {spilled_s:8.3} s   {spilled_n} rows");
    let (whole_n, whole_s) = count(&QueryOptions::new())?;
    println!("evaluator       {whole_s:8.3} s   {whole_n} rows");
    assert_eq!(spilled_n, whole_n, "the two paths must agree");
    Ok(())
}
