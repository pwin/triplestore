//! What does a holon tick actually cost?
//!
//! `DESIGN.md` §9 puts validation inside every commit. That is only a sane design if a tick
//! costs the size of its own change rather than the size of the scene — otherwise a
//! Boundary is a nightly batch job wearing a transaction's clothes.
//!
//! ```text
//! cargo run --release -p holos-holon --example tick_cost
//! ```

use holos_engine::Engine;
use holos_holon::{registry, tick, Delta, Holon};
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use holos_shacl::{CompiledShapes, Options};
use holos_store::GraphFilter;
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{Literal, NamedNode, Quad, Triple};
use std::time::Instant;

const BOUNDARY: &str = r#"
@prefix ex:   <http://example.com/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age  ; sh:maxCount 1 ; sh:datatype xsd:integer ;
                  sh:minInclusive 0 ; sh:maxInclusive 150 ] .
"#;

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scale: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);

    let mut engine = Engine::new();
    let holon = Holon::new(NamedNode::new_unchecked("urn:holon:people"));

    // Boundary shapes.
    for quad in oxrdfio::RdfParser::from_format(oxrdfio::RdfFormat::Turtle)
        .with_base_iri("http://example.com/")?
        .for_reader(BOUNDARY.as_bytes())
    {
        let mut quad = quad?;
        quad.graph_name = holon.boundary.clone().into();
        engine.store_mut().insert(quad.as_ref())?;
    }

    // A scene big enough that "validate everything" would be visibly expensive.
    let started = Instant::now();
    for i in 0..scale {
        for triple in [
            (rdf::TYPE.into_owned(), oxrdf::Term::from(ex("Person"))),
            (
                ex("name"),
                Literal::new_simple_literal(format!("P{i}")).into(),
            ),
            (
                ex("age"),
                Literal::new_typed_literal((20 + i % 60).to_string(), xsd::INTEGER).into(),
            ),
        ] {
            engine.store_mut().insert(
                Quad {
                    subject: ex(&format!("person{i}")).into(),
                    predicate: triple.0,
                    object: triple.1,
                    graph_name: holon.scene.clone().into(),
                }
                .as_ref(),
            )?;
        }
    }
    let seeded = started.elapsed();

    let mut session = Session::open(
        engine.store(),
        Principal::anonymous().with_role("admin"),
        Policy::default().with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Role("admin".into()),
        )),
    )?;
    registry::register(&mut engine, &holon, &mut session)?;

    // What a full validation of the scene costs, for comparison.
    let scene = GraphFilter::Named(
        engine
            .store()
            .lookup_term(holon.scene.as_ref().into())?
            .ok_or("scene graph")?,
    );
    let boundary = GraphFilter::Named(
        engine
            .store()
            .lookup_term(holon.boundary.as_ref().into())?
            .ok_or("boundary graph")?,
    );
    let shapes = CompiledShapes::compile(
        engine.store(),
        Options {
            data_graph: scene,
            shapes_graph: boundary,
        },
    )?;
    let started = Instant::now();
    let full = shapes.validate(engine.store())?;
    let full_time = started.elapsed();

    // One conforming tick.
    let good = |i: usize| -> Vec<Triple> {
        vec![
            Triple {
                subject: ex(&format!("new{i}")).into(),
                predicate: rdf::TYPE.into_owned(),
                object: ex("Person").into(),
            },
            Triple {
                subject: ex(&format!("new{i}")).into(),
                predicate: ex("name"),
                object: Literal::new_simple_literal(format!("New {i}")).into(),
            },
        ]
    };

    let started = Instant::now();
    let outcome = tick(&mut engine, &holon, &mut session, &Delta::adding(good(0)))?;
    let one_tick = started.elapsed();

    // A batch of ticks, to get a per-commit rate rather than one sample.
    let batch: u32 = 200;
    let started = Instant::now();
    for i in 1..=batch as usize {
        tick(&mut engine, &holon, &mut session, &Delta::adding(good(i)))?;
    }
    let batch_time = started.elapsed();

    // And a rejected one, to show a refusal costs the same as an acceptance.
    let bad = vec![
        Triple {
            subject: ex("badger").into(),
            predicate: rdf::TYPE.into_owned(),
            object: ex("Person").into(),
        },
        Triple {
            subject: ex("badger").into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal("900", xsd::INTEGER).into(),
        },
    ];
    let started = Instant::now();
    let refused = tick(&mut engine, &holon, &mut session, &Delta::adding(bad))?;
    let reject_time = started.elapsed();

    println!(
        "scene                {} instances, {} triples",
        scale,
        scale * 3
    );
    println!("seeded in            {:.2}s", seeded.as_secs_f64());
    println!(
        "full validation      {:.3}s ({} results)",
        full_time.as_secs_f64(),
        full.results.len()
    );
    println!(
        "one tick             {:.5}s (version {}, admitted {})",
        one_tick.as_secs_f64(),
        outcome.version,
        outcome.admitted
    );
    println!(
        "{batch} ticks           {:.3}s  =>  {:.5}s each, {:.0} commits/s",
        batch_time.as_secs_f64(),
        batch_time.as_secs_f64() / f64::from(batch),
        f64::from(batch) / batch_time.as_secs_f64()
    );
    println!(
        "a rejected tick      {:.5}s ({} violations, applied {})",
        reject_time.as_secs_f64(),
        refused.violations,
        refused.applied
    );
    println!(
        "\ntick vs full pass    {:.0}x cheaper",
        full_time.as_secs_f64() / (batch_time.as_secs_f64() / f64::from(batch))
    );
    Ok(())
}
