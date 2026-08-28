//! What incremental SHACL validation would be worth, measured before it is built.
//!
//! `DESIGN.md` §8 records that the adapted engine covers more constraint components than the
//! native evaluator but cannot revalidate a delta, because its `Graph` is immutable and a
//! change means bridging the whole store again. Closing that is bounded work — and worth
//! doing only if the re-bridge is actually what costs, which is what this measures.
//!
//! Three numbers per size:
//!
//! * **prepare** — bridging the store and compiling the shapes, paid on every commit today.
//! * **validate** — the full run that follows it.
//! * **native revalidate** — what the incremental path costs for the same one-triple change,
//!   as the target to aim at.
//!
//! Run with `cargo run -p holos-bench --release --bin shaclinc`.

use holos_engine::Engine;
use holos_shacl::incremental::Change;
use holos_shacl::{CompiledShapes, Options};
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use std::time::Instant;

const EX: &str = "http://example.com/";

/// A shapes graph with a handful of constraints, and `n` people to check them against.
fn dataset(n: usize) -> String {
    let mut s = String::with_capacity(n * 120 + 1024);
    s.push_str(
        "@prefix ex: <http://example.com/> .\n\
         @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         ex:PersonShape a sh:NodeShape ;\n\
           sh:targetClass ex:Person ;\n\
           sh:property [ sh:path ex:name  ; sh:minCount 1 ; sh:datatype xsd:string ] ;\n\
           sh:property [ sh:path ex:age   ; sh:maxCount 1 ; sh:datatype xsd:integer ;\n\
                         sh:minInclusive 0 ; sh:maxInclusive 150 ] ;\n\
           sh:property [ sh:path ex:email ; sh:pattern \"^[^@]+@[^@]+$\" ] ;\n\
           sh:property [ sh:path ex:knows ; sh:nodeKind sh:IRI ] .\n",
    );
    for i in 0..n {
        s.push_str(&format!(
            "ex:p{i} a ex:Person ; ex:name \"P{i}\" ; ex:age {} ; ex:email \"p{i}@example.com\" ; \
             ex:knows ex:p{} .\n",
            i % 90,
            (i + 1) % n.max(1)
        ));
    }
    s
}

fn engine(turtle: &str) -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(turtle.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

fn options() -> Options {
    Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    }
}

/// Median of `runs`, so one scheduling hiccup does not become the headline.
fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() {
    println!(
        "{:>9}  {:>12}  {:>12}  {:>12}  {:>13}  {:>9}",
        "quads", "prepare", "apply", "validate", "native reval", "prep/apply"
    );

    for n in [1_000usize, 10_000, 50_000] {
        let engine = engine(&dataset(n));
        let quads = engine.store().len();

        let prepare = median(
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    let run = holos_shacl::engine::EngineRun::prepare(engine.store(), options())
                        .expect("prepare");
                    let e = t.elapsed().as_secs_f64() * 1e3;
                    drop(run);
                    e
                })
                .collect(),
        );

        let mut run =
            holos_shacl::engine::EngineRun::prepare(engine.store(), options()).expect("prepare");
        let validate = median(
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    let report = run.validate().expect("validate");
                    let e = t.elapsed().as_secs_f64() * 1e3;
                    drop(report);
                    e
                })
                .collect(),
        );

        // The native path, over the same store and the same one-triple change: what the
        // adapted engine would cost if it could take a delta.
        let shapes = CompiledShapes::compile(engine.store(), options()).expect("compile");
        // A *data* triple, not the first quad in the store: the first is a shape definition,
        // and `EngineRun::apply` rightly refuses those.
        let name = engine
            .store()
            .lookup_term(oxrdf::NamedNodeRef::new_unchecked("http://example.com/name").into())
            .expect("lookup")
            .expect("ex:name is in the store");
        let target = engine
            .store()
            .quads_for_pattern(None, Some(name), None, GraphFilter::Default)
            .next()
            .expect("a data quad")
            .expect("decode");
        let changes = vec![Change::added(target)];
        let reval = median(
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    let report = shapes
                        .revalidate(engine.store(), &changes)
                        .expect("revalidate");
                    let e = t.elapsed().as_secs_f64() * 1e3;
                    drop(report);
                    e
                })
                .collect(),
        );

        // What `EngineRun::apply` costs for the same one-triple change: the delta path that
        // replaces the re-bridge.
        let change = vec![Change::added(target)];
        let apply = median(
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    run.apply(engine.store(), &change).expect("apply");
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect(),
        );

        println!(
            "{quads:>9}  {prepare:>10.3} ms  {apply:>10.4} ms  {validate:>10.3} ms               {reval:>10.4} ms  {:>7.0}x",
            prepare / apply.max(1e-9)
        );
    }

    println!();
    println!(
        "`prepare` is paid on every commit the adapted engine gates, because its graph cannot\n\
         take a delta. The ratio is what §8 is worth: how much cheaper the same guarantee is\n\
         when the validator can be told what changed. `{EX}` is the fixture namespace."
    );
}
