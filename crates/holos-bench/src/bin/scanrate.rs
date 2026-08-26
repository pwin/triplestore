//! How fast can the store hand out quads, with no evaluator above it?
//!
//! The query battery shows a three-pattern star costing about as much as three full
//! predicate scans, which points at the scan rather than the join. This isolates it: the
//! same iteration the evaluator drives, with nothing on top.
//!
//! ```text
//! cargo run --release -p holos-bench --bin scanrate
//! ```

use holos_engine::Engine;
use holos_security::Session;
use holos_store::{GraphFilter, Store};
use oxrdfio::RdfFormat;
use std::time::Instant;

fn rate(label: &str, quads: u64, seconds: f64) {
    println!(
        "{label:<44} {:>12} quads/s  {:>8.0} ns/quad",
        format_args!("{:.0}", quads as f64 / seconds),
        seconds * 1e9 / quads as f64
    );
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
    println!("{n} quads from {path}\n");

    let store: &Store = engine.store();
    let name = store
        .lookup_term(oxrdf::NamedNodeRef::new_unchecked("http://holos.example/name").into())?
        .expect("ex:name");

    // 1. The raw encoded scan: term ids straight off the index, nothing decoded.
    let started = Instant::now();
    let mut count = 0_u64;
    for quad in store.quads_for_pattern(None, None, None, GraphFilter::Default) {
        let _ = quad?;
        count += 1;
    }
    rate("raw scan, everything", count, started.elapsed().as_secs_f64());

    // 2. A prefix scan on one predicate — what a QuadPattern actually does.
    let started = Instant::now();
    let mut count = 0_u64;
    for quad in store.quads_for_pattern(None, Some(name), None, GraphFilter::Default) {
        let _ = quad?;
        count += 1;
    }
    rate("raw scan, one predicate", count, started.elapsed().as_secs_f64());

    // 3. The same, decoded to terms — the cost of leaving the id domain.
    let started = Instant::now();
    let mut count = 0_u64;
    for quad in store.quads_for_pattern(None, Some(name), None, GraphFilter::Default) {
        let _ = store.decode_quad(quad?)?;
        count += 1;
    }
    rate("raw scan + decode", count, started.elapsed().as_secs_f64());

    // 4. Through the policy-filtered view, which is what the evaluator sees. The gap
    //    between this and (2) is what enforcement costs per quad.
    let session = Session::unrestricted(store)?;
    let view = engine.view(&session);
    let started = Instant::now();
    let count = view.visible_quads(None)?.len() as u64;
    rate("through the view (decoded, collected)", count, started.elapsed().as_secs_f64());

    // 4b. Where does decode time go? Decoding one id repeatedly removes the scan and the
    //     cache effects, leaving the construction cost of an owned term.
    {
        let subject = store
            .lookup_term(
                oxrdf::NamedNodeRef::new_unchecked("http://holos.example/person1000").into(),
            )?
            .expect("person1000");
        let reps = 2_000_000_u64;

        let started = Instant::now();
        for _ in 0..reps {
            let _ = std::hint::black_box(store.decode_term(std::hint::black_box(subject))?);
        }
        rate("decode one IRI, repeatedly", reps, started.elapsed().as_secs_f64());

        // A literal carries a value and a datatype, so it is the expensive shape.
        let literal = store
            .quads_for_pattern(None, Some(name), None, GraphFilter::Default)
            .next()
            .expect("a name quad")?
            .object;
        let started = Instant::now();
        for _ in 0..reps {
            let _ = std::hint::black_box(store.decode_term(std::hint::black_box(literal))?);
        }
        rate("decode one literal, repeatedly", reps, started.elapsed().as_secs_f64());

        // The floor: what the id costs to hand back with nothing built from it.
        let started = Instant::now();
        for _ in 0..reps {
            let _ = std::hint::black_box(std::hint::black_box(subject).payload());
        }
        rate("id payload only (the floor)", reps, started.elapsed().as_secs_f64());
    }

    // 4c. The policy decision alone. The earlier attribution of ~510 ns/quad to this was
    //     wrong — it compared a full scan retaining 750k decoded quads against a
    //     single-predicate scan that dropped each one. This measures the decision itself.
    {
        let quads: Vec<_> = store
            .quads_for_pattern(None, None, None, GraphFilter::Default)
            .collect::<Result<Vec<_>, _>>()?;
        let policy = session.policy_unchecked();
        let started = Instant::now();
        let mut allowed = 0_u64;
        for _ in 0..10 {
            for q in &quads {
                if policy.decide_quad(*q, holos_security::Modes::READ)
                    == holos_security::Decision::Allow
                {
                    allowed += 1;
                }
            }
        }
        rate("policy decide_quad alone", allowed, started.elapsed().as_secs_f64());

        // Scan and decode, dropping each — the same shape as (3) but over every quad, so
        // it is comparable with the view measurement above.
        let started = Instant::now();
        let mut n = 0_u64;
        for q in &quads {
            let _ = std::hint::black_box(store.decode_quad(*q)?);
            n += 1;
        }
        rate("decode all, dropped immediately", n, started.elapsed().as_secs_f64());

        // The same, retained. The gap against the line above is what holding the results
        // costs, which is what the view measurement was actually showing.
        let started = Instant::now();
        let mut kept = Vec::with_capacity(quads.len());
        for q in &quads {
            kept.push(store.decode_quad(*q)?);
        }
        rate("decode all, retained in a Vec", kept.len() as u64, started.elapsed().as_secs_f64());
    }

    // 5. What the evaluator measures end to end, for the same predicate.
    let started = Instant::now();
    let results = Engine::query(
        &view,
        "SELECT ?s WHERE { ?s <http://holos.example/name> ?o }",
        None,
    )?;
    let n = match results {
        spareval::QueryResults::Solutions(iter) => {
            let mut n = 0;
            for s in iter {
                s?;
                n += 1;
            }
            n
        }
        _ => 0,
    };
    rate("evaluator, same predicate", n, started.elapsed().as_secs_f64());

    Ok(())
}
