//! A query that asks for too much memory is cancelled, and the process lives.
//!
//! This is the test the feature exists for, and it needs a real counting allocator to mean
//! anything — a memory ceiling checked against a counter nobody maintains passes every test
//! and protects nothing. An integration test is its own binary, so it can install one, and
//! the one it installs is `holos-server`'s own module rather than a copy: a copy would be a
//! second implementation to keep in agreement with the first, and the agreement is the
//! whole point.
//!
//! # Why almost everything is in one test function
//!
//! `holos_engine::memory` counts the **process**, and cargo runs `#[test]` functions as
//! parallel threads of one process. An earlier version of this file split the cases up and
//! they failed each other: while one test built a 400,000-quad store, every other test's
//! query looked like it had allocated 400,000 quads' worth, and a `COUNT(*)` that allocates
//! almost nothing was cancelled for passing an 8 MiB ceiling.
//!
//! That is a real property of the design, documented in `memory.rs` and mitigated there by
//! requiring a breach to persist across several samples — but it makes a *test* of the
//! ceiling meaningless unless one thing is allocating at a time. So the ceiling cases run in
//! sequence inside one function, and the noisy neighbour is gone.
//!
//! # What is not tested here
//!
//! The failing allocation itself. Provoking a real `handle_alloc_error` means allocating
//! more than the machine has, which aborts the test runner whether or not the guard works.
//! The ceiling is set small and the dataset kept modest instead: the mechanism — sample,
//! compare, cancel through the token — is identical at 8 MiB and at 8 GiB.

use holos_engine::memory;
use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;

// The server is a binary, so its modules reach a test through a path include rather than as
// a library, the same way `gsp.rs` does.
#[path = "../src/alloc.rs"]
mod alloc;

/// The real allocator, doing the real counting, for the duration of this binary.
#[global_allocator]
static ALLOCATOR: alloc::Counting<std::alloc::System> = alloc::Counting(std::alloc::System);

const EX: &str = "http://holos.example/";

/// Enough rows that a `DISTINCT` over them is worth tens of megabytes, and enough work that
/// the query outlives several 10 ms watchdog ticks.
const ROWS: usize = 400_000;

/// The ceiling the cases below share. Small enough that `DISTINCT` passes it quickly.
const CEILING: usize = 8 * 1024 * 1024;

fn engine() -> Engine {
    let mut store = holos_store::Store::new();
    for i in 0..ROWS {
        store
            .insert(
                Quad {
                    subject: NamedNode::new_unchecked(format!("{EX}s{i}")).into(),
                    predicate: NamedNode::new_unchecked(format!("{EX}p")),
                    object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    Engine::with_store(store)
}

/// Runs a query to completion, returning the first error if it produced one.
///
/// Stopping at the first error is the contract a client sees, and it matters here for a
/// reason worth recording: a **cancelled aggregate does not yield one row**. `spareval`
/// evaluates `COUNT(*)` by consuming its child into an accumulator, and when that child is
/// cancelled it forwards one error per remaining child row — so a cancelled count over
/// 400,000 quads yields hundreds of thousands of `Err`s, not one. An earlier version of this
/// file asserted `rows.len() == 1` on a cancelled query and was wrong to.
fn drain(engine: &Engine, query: &str, options: &QueryOptions) -> Result<usize, String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let (results, _) = Engine::query_with(&view, query, options).map_err(|e| e.to_string())?;
    let QueryResults::Solutions(iter) = results else {
        return Ok(0);
    };
    let mut rows = 0;
    for solution in iter {
        solution.map_err(|e| e.to_string())?;
        rows += 1;
    }
    Ok(rows)
}

fn distinct_query() -> String {
    format!("PREFIX ex: <{EX}> SELECT DISTINCT ?s ?p ?o WHERE {{ ?s ?p ?o }}")
}

fn count_query() -> String {
    format!("PREFIX ex: <{EX}> SELECT (COUNT(*) AS ?n) WHERE {{ ?s ?p ?o }}")
}

/// The allocator has to actually be counting, or nothing else here tests anything.
///
/// Kept separate because it depends only on the counter moving, not on a ceiling, and it is
/// the assertion that makes every other one non-vacuous.
#[test]
fn the_counter_is_live_in_this_binary() {
    memory::mark_tracking();
    assert!(memory::tracking(), "mark_tracking did not take");

    let before = memory::live_bytes();
    assert!(
        before > 0,
        "the allocator is not counting: {before} bytes live"
    );

    let held: Vec<u8> = vec![7; 32 * 1024 * 1024];
    let during = memory::live_bytes();
    assert!(
        during >= before + 32 * 1024 * 1024,
        "32 MiB allocated but the counter moved from {before} to {during}"
    );
    drop(held);
}

/// Everything that depends on the ceiling, in sequence.
#[test]
fn the_ceiling_stops_a_runaway_and_leaves_everything_else_alone() {
    memory::mark_tracking();
    let engine = engine();
    let capped = QueryOptions::new().with_memory_limit(CEILING);

    // -----------------------------------------------------------------------------
    // 1. The shape that killed a real server is cancelled rather than fatal.
    //
    // `spareval` answers `SELECT DISTINCT ?s ?p ?o` by holding every distinct solution in
    // a `HashSet<InternalTuple<TermId>>` — 24 bytes of table per row plus a heap `Vec`
    // behind each — so it grows without bound, and the allocation that finally fails
    // aborts the process rather than failing the request.
    // -----------------------------------------------------------------------------
    let error = drain(&engine, &distinct_query(), &capped).expect_err("the ceiling should fire");
    assert!(
        error.contains("over the") && error.contains("limit"),
        "cancelled, but not for the stated reason: {error}"
    );
    assert!(
        error.contains("--max-query-memory"),
        "the message must say how to raise it: {error}"
    );

    // -----------------------------------------------------------------------------
    // 2. The same query, uncapped, is left alone.
    //
    // Without this, case 1 would pass against a build that refused every query — a way of
    // surviving a memory ceiling that nobody wants.
    // -----------------------------------------------------------------------------
    let rows =
        drain(&engine, &distinct_query(), &QueryOptions::new()).expect("no ceiling, no refusal");
    assert_eq!(rows, ROWS);

    // -----------------------------------------------------------------------------
    // 3. A ceiling refuses the query that earns it and nothing else.
    //
    // `COUNT(*)` over the same data is what the bug report meant to run: `spareval` answers
    // it with a bare `u64` counter and no hash table at all, so it must stream to an answer
    // under the very ceiling that stops `DISTINCT` dead. If this failed, the ceiling would
    // be a blunt refusal of large datasets rather than of runaway queries.
    // -----------------------------------------------------------------------------
    {
        let session = Session::unrestricted(engine.store()).expect("session");
        let view = engine.view(&session);
        let (results, _) = Engine::query_with(&view, &count_query(), &capped).expect("query");
        let QueryResults::Solutions(iter) = results else {
            panic!("expected solutions");
        };
        let first = iter
            .into_iter()
            .next()
            .expect("a count produces a row")
            .expect("counting must not be cancelled by a ceiling that stops DISTINCT");
        let count = first.get("n").expect("a count").to_string();
        assert!(count.starts_with(&format!("\"{ROWS}\"")), "counted {count}");
    }

    // -----------------------------------------------------------------------------
    // 4. The process is still here, and the engine still answers.
    //
    // The whole feature is the difference between a failed request and a dead server.
    // -----------------------------------------------------------------------------
    for _ in 0..2 {
        assert!(drain(&engine, &distinct_query(), &capped).is_err());
    }
    assert!(
        drain(&engine, "ASK { }", &QueryOptions::new()).is_ok(),
        "the engine is unusable after a cancellation"
    );
}
