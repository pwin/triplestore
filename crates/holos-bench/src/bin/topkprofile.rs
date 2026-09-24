//! Where the time goes in a top-k query, by ablation rather than by instrumentation.
//!
//! `SELECT ?o WHERE { ?s <p> ?o } ORDER BY DESC(?o) LIMIT k` answers in 42 s on a
//! 654-million-quad store whose predicate holds 48.4 million rows. How much of that is the
//! disk and how much is the operator was an open question, and the tempting answer — time a
//! `COUNT(*)` and subtract — measures nothing, because a `Group` is outside
//! `holos_engine::bindjoin`'s fragment, so that query is scanned by the evaluator and this
//! one by the join, over different index orders.
//!
//! So the decomposition is made by running *the same* join over *the same* rows three
//! times, changing only what the sink does with each row:
//!
//! | phase | what it adds |
//! |---|---|
//! | `index` | the store alone: RocksDB iterating one predicate's slice of `POS` |
//! | `+ join` | the policy decision, the bindings, and a projected row per match |
//! | `+ decode` | the sort key through the view's decode cache, and into an `ExpressionTerm` |
//! | `+ heap` | offering the key to a heap of `k` |
//!
//! The differences are what each stage costs. Nothing is timed per row, so nothing is
//! perturbed by timing it: the only clocks are at the ends of a phase.
//!
//! Two passes, and the second is the one to read. The first warms the page cache, and
//! without it the phases would be compared at three different cache temperatures and the
//! later ones would look cheap for no reason but running later.
//!
//! ```text
//! topkprofile <store> [predicate] [k]
//! ```

use holos_core::TermId;
use holos_engine::{bindjoin, topk, Engine, ViewError};
use holos_security::Session;
use holos_store::{GraphFilter, RocksStorage, Store};
use oxrdf::{NamedNode, Term, TermRef};
use spareval::ExpressionTerm;
use spargebra::SparqlParser;
use std::time::Instant;

/// What one phase of the ablation did.
struct Phase {
    name: &'static str,
    rows: u64,
    seconds: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let store_path = args
        .next()
        .ok_or("topkprofile <store> [predicate] [k]")?;
    let predicate = args
        .next()
        .unwrap_or_else(|| "http://schema.org/deathDate".to_owned());
    let k: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(4);
    let passes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);

    let opened = Instant::now();
    let store = Store::with_storage(RocksStorage::open(&store_path)?);
    let engine = Engine::with_store(store);
    let session = Session::unrestricted(engine.store())?;
    println!(
        "{store_path}: {} quads, opened in {:.1} s",
        engine.store().len(),
        opened.elapsed().as_secs_f64()
    );

    let sparql =
        format!("SELECT ?o WHERE {{ ?s <{predicate}> ?o }} ORDER BY DESC(?o) LIMIT {k}");
    println!("{sparql}\n");
    let parsed = SparqlParser::new().parse_query(&sparql)?;
    let plan = topk::plan(&parsed).ok_or("not a shape the heap answers")?;
    let join = bindjoin::plan(&plan.inner).ok_or("the body is outside the join's fragment")?;

    // The inner query projects what the output and the keys need; the key is the one
    // variable this query sorts by, so its column is where that variable landed.
    let key_at = join
        .variables()
        .iter()
        .position(|v| v.as_str() == "o")
        .ok_or("the body does not project the sort key")?;

    let predicate_id = engine
        .store()
        .lookup_term(TermRef::from(&NamedNode::new(predicate.clone())?))?
        .ok_or("the store has never seen that predicate")?;

    for pass in 1..=passes {
        let mut phases = Vec::new();

        // ---- 0. the index, with nothing above it -------------------------------------
        {
            let started = Instant::now();
            let rows = engine
                .store()
                .quads_for_pattern(None, Some(predicate_id), None, GraphFilter::Default)
                .count() as u64;
            phases.push(Phase {
                name: "index",
                rows,
                seconds: started.elapsed().as_secs_f64(),
            });
        }

        // ---- 1. and the join over it -------------------------------------------------
        {
            let view = engine.view(&session);
            let mut rows = 0_u64;
            let started = Instant::now();
            let mut sink = |_row: &[Option<TermId>]| -> Result<(), ViewError> {
                rows += 1;
                Ok(())
            };
            let whole = join.evaluate_each(&view, None, limits(), &mut sink)?;
            assert!(whole, "the join abandoned the scan");
            phases.push(Phase {
                name: "+ join",
                rows,
                seconds: started.elapsed().as_secs_f64(),
            });
        }

        // ---- 2. and the key decoded --------------------------------------------------
        {
            let view = engine.view(&session);
            let mut rows = 0_u64;
            let mut bound = 0_u64;
            let started = Instant::now();
            let mut sink = |row: &[Option<TermId>]| -> Result<(), ViewError> {
                rows += 1;
                if let Some(key) = decode_key(&view, row, key_at)? {
                    // Kept off the optimiser's list of things it may discard.
                    bound += u64::from(!matches!(key, ExpressionTerm::BooleanLiteral(_)));
                }
                Ok(())
            };
            let whole = join.evaluate_each(&view, None, limits(), &mut sink)?;
            assert!(whole, "the join abandoned the scan");
            assert!(bound > 0, "every key came back unbound");
            phases.push(Phase {
                name: "+ decode",
                rows,
                seconds: started.elapsed().as_secs_f64(),
            });
        }

        // ---- 3. and offered to a heap of k -------------------------------------------
        {
            let view = engine.view(&session);
            let mut rows = 0_u64;
            // `k` is small, so the held rows are kept sorted in a vector: the per-row cost
            // is one comparison against the worst, which is the heap's too.
            let mut held: Vec<Option<ExpressionTerm>> = Vec::with_capacity(k + 1);
            let descending = [true];
            let started = Instant::now();
            let mut sink = |row: &[Option<TermId>]| -> Result<(), ViewError> {
                rows += 1;
                let key = decode_key(&view, row, key_at)?;
                let one = [key];
                if held.len() == k {
                    let worst = std::slice::from_ref(held.last().expect("k > 0"));
                    if topk::cmp_keys(&one, worst, &descending) != std::cmp::Ordering::Less {
                        return Ok(());
                    }
                    held.pop();
                }
                let at = held
                    .iter()
                    .position(|h| {
                        topk::cmp_keys(&one, std::slice::from_ref(h), &descending)
                            == std::cmp::Ordering::Less
                    })
                    .unwrap_or(held.len());
                let [key] = one;
                held.insert(at, key);
                Ok(())
            };
            let whole = join.evaluate_each(&view, None, limits(), &mut sink)?;
            assert!(whole, "the join abandoned the scan");
            assert_eq!(held.len(), k.min(rows as usize), "the heap did not fill");
            phases.push(Phase {
                name: "+ heap",
                rows,
                seconds: started.elapsed().as_secs_f64(),
            });
            if pass == 2 {
                let answer: Vec<String> = held
                    .iter()
                    .map(|k| k.clone().map(Term::from).map_or_else(String::new, |t| t.to_string()))
                    .collect();
                println!("answer: {}\n", answer.join(" "));
            }
        }

        report(pass, &phases);
    }
    Ok(())
}

/// No row budget and no deadline: the sink holds nothing, so there is nothing to bound.
fn limits<'a>() -> bindjoin::Limits<'a> {
    bindjoin::Limits {
        rows: None,
        token: None,
    }
}

/// The sort key of one row, exactly as `holos_engine::topk` reads it on the id path.
fn decode_key(
    view: &holos_engine::DatasetView<'_>,
    row: &[Option<TermId>],
    at: usize,
) -> Result<Option<ExpressionTerm>, ViewError> {
    match row.get(at).and_then(Option::as_ref) {
        Some(id) => Ok(view.decode_term(*id)?.map(ExpressionTerm::from)),
        None => Ok(None),
    }
}

fn report(pass: usize, phases: &[Phase]) {
    println!("pass {pass}{}", if pass == 1 { " (warming)" } else { "" });
    let mut previous = 0.0;
    for phase in phases {
        let added = phase.seconds - previous;
        #[allow(
            clippy::cast_precision_loss,
            reason = "a rate printed to the nearest thousand rows"
        )]
        let rate = phase.rows as f64 / phase.seconds;
        println!(
            "  {:<10} {:8.2} s   {:+7.2} s   {:>12} rows   {:.2} M rows/s",
            phase.name,
            phase.seconds,
            added,
            phase.rows,
            rate / 1e6
        );
        previous = phase.seconds;
    }
    println!();
}
