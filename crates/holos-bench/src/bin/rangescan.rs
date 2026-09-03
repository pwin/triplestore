//! What is a range-bounded scan actually worth?
//!
//! `DESIGN.md` §5's order-preserving encodings exist so that `FILTER(?o > k)` can bound the
//! index rather than be tested against everything it returns. The bound is built and proven
//! correct; whether it is *worth wiring into the planner* is a separate question, and this
//! answers it before the wiring is written rather than after.
//!
//! The comparison is the one the planner would face: for a pattern whose object is filtered,
//! either read the whole predicate and test each quad, or read only the span and test what
//! comes back. Both go through `DatasetView`, so both pay policy — the difference is purely
//! how much is read.
//!
//! Selectivity is the axis that matters. A filter keeping 1% of a predicate should win big;
//! one keeping 90% should win nothing, because the span is nearly the whole predicate and
//! the bound is then pure overhead. If the crossover is at a selectivity nobody writes, the
//! feature is not worth the planner complexity.
//!
//! ```text
//! cargo run --release -p holos-bench --bin rangescan
//! ```

use holos_core::{Tag, TermId};
use holos_security::Session;
use holos_store::{GraphFilter, IdRange, Store};
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use spareval::QueryableDataset;
use std::time::{Duration, Instant};

const EX: &str = "http://holos.example/";
/// Enough that a scan is measurable and a store still builds in a few seconds.
const QUADS: usize = 400_000;

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn integer(n: i64) -> Term {
    Literal::new_typed_literal(n.to_string(), xsd::INTEGER).into()
}

/// One predicate, integers spread evenly over `0..QUADS`, plus a second predicate of the
/// same size so a scan that ignored the predicate would be obvious.
fn fill(store: &mut Store) {
    for i in 0..QUADS {
        let subject = ex(&format!("s{i}"));
        store
            .insert(
                Quad {
                    subject: subject.clone().into(),
                    predicate: ex("age"),
                    object: integer(i as i64),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
        store
            .insert(
                Quad {
                    subject: subject.into(),
                    predicate: ex("other"),
                    object: integer(i as i64),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
}

/// Reads the whole predicate and keeps what the span admits — what the planner does today.
fn scan_and_filter(
    view: &holos_engine::view::DatasetView<'_>,
    p: Option<TermId>,
    span: IdRange,
) -> usize {
    let mut kept = 0;
    for quad in
        QueryableDataset::internal_quads_for_pattern(&view, None, p.as_ref(), None, Some(None))
    {
        if quad.is_ok_and(|q| span.contains(q.object)) {
            kept += 1;
        }
    }
    kept
}

/// Reads only the span.
fn bounded(view: &holos_engine::view::DatasetView<'_>, p: Option<TermId>, span: IdRange) -> usize {
    view.quads_with_object_in(None, p, span, GraphFilter::Default)
        .filter(Result::is_ok)
        .count()
}

/// The faster of three runs, which is the honest estimate of what the work costs when the
/// machine is not doing something else.
fn best(mut f: impl FnMut() -> usize) -> (Duration, usize) {
    let mut best = Duration::MAX;
    let mut rows = 0;
    for _ in 0..3 {
        let started = Instant::now();
        rows = f();
        best = best.min(started.elapsed());
    }
    (best, rows)
}

fn measure(label: &str, store: &Store) {
    let mut session = Session::unrestricted(store).expect("session");
    let policy = session.policy(store).expect("policy").clone();
    let view = holos_engine::view::DatasetView::new(store, &policy);
    let age = store
        .lookup_term(ex("age").as_ref().into())
        .expect("lookup");

    let id = |n: i64| {
        store
            .lookup_term(integer(n).as_ref())
            .expect("lookup")
            .expect("integers are inline")
    };
    let top = TermId::new(Tag::Integer, holos_core::term_id::PAYLOAD_MAX);

    println!("\n{label} — {QUADS} quads under `age`, {QUADS} under `other`");
    println!(
        "{:>12}  {:>10}  {:>12}  {:>12}  {:>8}",
        "selectivity", "rows", "scan+filter", "bounded", "speed-up"
    );

    for percent in [1, 5, 10, 25, 50, 90] {
        let cut = QUADS as i64 - (QUADS as i64 * percent / 100);
        let span = IdRange {
            first: id(cut),
            last: top,
        };
        let (slow, a) = best(|| scan_and_filter(&view, age, span));
        let (fast, b) = best(|| bounded(&view, age, span));
        assert_eq!(a, b, "the two paths disagreed at {percent}%");
        println!(
            "{:>11}%  {:>10}  {:>10.2} ms  {:>10.2} ms  {:>7.1}x",
            percent,
            a,
            slow.as_secs_f64() * 1e3,
            fast.as_secs_f64() * 1e3,
            slow.as_secs_f64() / fast.as_secs_f64().max(f64::MIN_POSITIVE)
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut memory = Store::new();
    fill(&mut memory);
    measure("in memory", &memory);

    #[cfg(feature = "rocksdb")]
    {
        let dir = std::env::temp_dir().join("holos-rangescan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut rocks = Store::with_storage(holos_store::RocksStorage::open(&dir)?);
        rocks.begin_bulk_load()?;
        fill(&mut rocks);
        rocks.end_bulk_load()?;
        measure("rocksdb", &rocks);
    }

    println!();
    println!("The bound only reads the span; the unbounded scan reads the predicate and");
    println!("tests each quad. Both go through the view, so both pay policy — what differs");
    println!("is how much is read. Where the speed-up approaches 1x the bound is not worth");
    println!("planning for.");
    Ok(())
}
