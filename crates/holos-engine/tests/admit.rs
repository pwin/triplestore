//! A query that cannot finish is refused before it starts.
//!
//! The memory ceiling in `holos_engine::memory` already stops a runaway query taking the
//! process down, but it does so several gigabytes and some minutes in. This is the other
//! half: an `ORDER BY` over six hundred million rows was never going to finish, and an
//! estimate costs a walk of the algebra.
//!
//! The asymmetry that shapes every test here: **refusing a query that would have worked is
//! a bug the user cannot work around**, while admitting one that should have been refused
//! merely falls back to the ceiling. So there are as many tests below for what must *not*
//! be refused as for what must.

use holos_engine::admit::{self, DEFAULT_BLOCKING_ROWS};
use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::{GraphFilter, Store};
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;
use std::sync::Arc;

const EX: &str = "http://holos.example/";
const ROWS: usize = 20_000;

fn engine() -> Engine {
    let mut store = Store::new();
    for i in 0..ROWS {
        let subject = NamedNode::new_unchecked(format!("{EX}s{i}"));
        store
            .insert(
                Quad {
                    subject: subject.clone().into(),
                    predicate: NamedNode::new_unchecked(format!("{EX}p")),
                    object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
        // A rare predicate, so an estimate that ignored selectivity would be caught.
        if i < 10 {
            store
                .insert(
                    Quad {
                        subject: subject.into(),
                        predicate: NamedNode::new_unchecked(format!("{EX}rare")),
                        object: Literal::new_simple_literal("x").into(),
                        graph_name: GraphName::DefaultGraph,
                    }
                    .as_ref(),
                )
                .expect("insert");
        }
    }
    Engine::with_store(store)
}

fn stats(engine: &Engine) -> Arc<Statistics> {
    Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"))
}

/// Runs a query, returning the error text if it was refused or failed.
fn run(engine: &Engine, query: &str, options: &QueryOptions) -> Result<usize, String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let (results, _) = Engine::query_with(&view, query, options).map_err(|e| e.to_string())?;
    match results {
        QueryResults::Solutions(iter) => {
            let mut n = 0;
            for solution in iter {
                solution.map_err(|e| e.to_string())?;
                n += 1;
            }
            Ok(n)
        }
        _ => Ok(0),
    }
}

fn capped(engine: &Engine, rows: u64) -> QueryOptions {
    QueryOptions::new()
        .reordering(stats(engine))
        .with_blocking_budget(rows)
}

/// The headline: an `ORDER BY` over everything is declined, and says what it estimated.
#[test]
fn an_order_by_over_the_whole_store_is_refused_with_its_estimate() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }} ORDER BY ?o");

    let error = run(&engine, &query, &capped(&engine, 1_000)).expect_err("should be refused");
    assert!(error.contains("ORDER BY"), "{error}");
    assert!(
        error.contains("estimated"),
        "must say what it estimated: {error}"
    );
    assert!(
        error.contains("--max-blocking-rows"),
        "must say how to raise it: {error}"
    );
}

/// And a `LIMIT` must not be offered as the fix for an `ORDER BY`, because it is not one.
///
/// `spareval` sorts the whole input before applying the slice, so `ORDER BY ... LIMIT 10`
/// over a billion rows buffers a billion rows. Advice that does not work is worse than none.
#[test]
fn the_refusal_does_not_claim_a_limit_would_help_an_order_by() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }} ORDER BY ?o");
    let error = run(&engine, &query, &capped(&engine, 1_000)).expect_err("refused");
    assert!(
        error.contains("A LIMIT will not help"),
        "an ORDER BY refusal must say a LIMIT is not the fix: {error}"
    );
}

/// `DISTINCT` is different, and the message says so: an engine may stop at k distinct rows.
#[test]
fn a_distinct_refusal_says_a_limit_may_help() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT DISTINCT ?o WHERE {{ ?s ex:p ?o }}");
    let error = run(&engine, &query, &capped(&engine, 1_000)).expect_err("refused");
    assert!(error.contains("DISTINCT"), "{error}");
    assert!(error.contains("A LIMIT may help"), "{error}");
}

// -----------------------------------------------------------------------------------
// What must not be refused. Each of these would be a bug the user cannot work around.
// -----------------------------------------------------------------------------------

/// A query with no blocking operator streams, however much data it touches.
#[test]
fn a_plain_scan_is_never_refused_however_large() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }}");
    assert_eq!(
        run(&engine, &query, &capped(&engine, 1)).expect("streams"),
        ROWS
    );
}

/// `COUNT(*)` is the instructive one: an aggregate, and a `u64` counter rather than a buffer.
#[test]
fn counting_is_not_refused_by_a_budget_that_stops_distinct() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT (COUNT(*) AS ?n) WHERE {{ ?s ex:p ?o }}");
    assert_eq!(
        run(&engine, &query, &capped(&engine, 1)).expect("counts"),
        1
    );
}

/// A blocking operator over a *selective* pattern is fine, and the estimate must know it.
///
/// This is the test that would fail if the estimator ignored selectivity and used the store
/// size for every pattern — which is the easy way to write it and the wrong one.
#[test]
fn an_order_by_over_a_selective_pattern_is_admitted() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s WHERE {{ ?s ex:rare ?o }} ORDER BY ?s");
    assert_eq!(
        run(&engine, &query, &capped(&engine, 1_000)).expect("ten rows is not a runaway"),
        10
    );
}

/// Without statistics there is no estimate, so nothing is refused.
///
/// A server started without `--reorder` must not start declining queries; the ceiling is
/// still there, and a refusal based on a number nobody computed would be a guess.
#[test]
fn no_statistics_means_no_refusal() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }} ORDER BY ?o");
    let options = QueryOptions::new().with_blocking_budget(1);
    assert_eq!(
        run(&engine, &query, &options).expect("no stats, no refusal"),
        ROWS
    );
}

/// And with no budget set, statistics alone change nothing.
#[test]
fn no_budget_means_no_refusal() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }} ORDER BY ?o");
    let options = QueryOptions::new().reordering(stats(&engine));
    assert_eq!(run(&engine, &query, &options).expect("no budget"), ROWS);
}

/// The default budget is far above anything this store is for.
#[test]
fn the_default_budget_admits_an_ordinary_query() {
    let engine = engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }} ORDER BY ?o");
    assert_eq!(
        run(&engine, &query, &capped(&engine, DEFAULT_BLOCKING_ROWS)).expect("ordinary"),
        ROWS
    );
}

/// The estimate has to be in the right order of magnitude, or the budget means nothing.
#[test]
fn the_estimate_is_close_to_the_truth() {
    let engine = engine();
    let stats = stats(&engine);
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:p ?o }} ORDER BY ?o");
    let parsed = spargebra::SparqlParser::new()
        .parse_query(&query)
        .expect("parses");

    // Budget of zero, so whatever it estimated comes back.
    let blocking = admit::over_budget(&parsed, &stats, engine.store(), 0).expect("blocking");
    assert_eq!(blocking.operator, "ORDER BY");
    #[allow(
        clippy::cast_precision_loss,
        reason = "a q-error ratio, printed to one decimal place"
    )]
    let truth = ROWS as f64;
    let ratio = (blocking.rows as f64 / truth).max(truth / blocking.rows as f64);
    assert!(
        ratio < 2.0,
        "estimated {} rows against {ROWS} actual, a q-error of {ratio:.1}",
        blocking.rows
    );
}

/// The query that took down a real server, refused before it starts.
///
/// ```sparql
/// SELECT (count(distinct *) as ?count) WHERE { ?sub ?pred ?obj . }
/// ```
///
/// `spareval` answers it with `CountDistinctTuple`, which holds every distinct solution in
/// a `HashSet<InternalTuple>` — 24 bytes of table per row plus a heap `Vec` behind each. On
/// a 667-million-triple store that is tens of gigabytes, and the allocation that finally
/// fails calls `handle_alloc_error`, which aborts the process rather than failing the
/// request.
///
/// Note what it is *not*: `COUNT(*)` over the same pattern streams in constant memory, and
/// over the default graph the two agree, because a store holds triples as a set. The
/// `DISTINCT` costs everything and buys nothing here.
#[test]
fn the_query_that_took_down_a_server_is_refused() {
    let engine = engine();
    let query = "SELECT (count(distinct *) as ?count) WHERE { ?sub ?pred ?obj . }";

    let error = run(&engine, query, &capped(&engine, 1_000)).expect_err("must be refused");
    assert!(error.contains("GROUP BY"), "{error}");
    assert!(error.contains("estimated"), "{error}");

    // And the same shape without DISTINCT is admitted, because it streams.
    let counted = "SELECT (count(*) as ?count) WHERE { ?sub ?pred ?obj . }";
    assert_eq!(
        run(&engine, counted, &capped(&engine, 1)).expect("streams"),
        1
    );
}
