//! The bind join with a range-bounded scan, checked against the evaluator it bypasses.
//!
//! A filter that bounds a pattern's object turns the scan into a range read — measured at
//! 226× in memory for a filter keeping 1% of a predicate, and never worse than 1.1×
//! (`rangescan`). The bound decides what is *read*; the filter still decides what matches,
//! so the answer must not move at all.
//!
//! That is what this checks, the same way the rest of the bind join is checked: run the query
//! through the fast path and through `spareval`, and require the two to agree. The failure
//! this is aimed at is not a wrong number of rows — it is a *missing* row, from a span that
//! excluded something the filter would have kept, which no amount of staring at the fast
//! path alone would reveal.
//!
//! The data is deliberately mixed. `xsd:integer` and `xsd:float` are inline and ordered;
//! `xsd:decimal` and `xsd:double` are not, and are numbers all the same. A pushdown that
//! forgot them would pass every test written only over integers.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use std::sync::Arc;

/// Values across every encoding a numeric comparison has to reach, in two graphs.
fn data() -> String {
    let mut out = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..60 {
        out.push_str(&format!(
            "ex:s{i} ex:age {i} ; ex:name \"n{i}\" ; ex:shoe {} .\n",
            i % 12
        ));
    }
    // Numbers the inline codec declines, which a span must still admit.
    out.push_str("ex:d1 ex:age \"30.5\"^^xsd:decimal .\n");
    out.push_str("ex:d2 ex:age \"41.25\"^^xsd:double .\n");
    out.push_str("ex:d3 ex:age \"2.5\"^^xsd:float .\n");
    out.push_str("ex:d4 ex:age \"576460752303423489\"^^xsd:integer .\n");
    // And things a numeric comparison must not match at all.
    out.push_str("ex:x1 ex:age \"thirty\" .\n");
    out.push_str("ex:x2 ex:age ex:thirty .\n");
    out.push_str("ex:x3 ex:age \"2020-06-01T12:00:00Z\"^^xsd:dateTime .\n");
    // A named graph, so the graph orders are exercised too.
    out.push_str("ex:g1 { ex:n1 ex:age 5 . ex:n2 ex:age 55 . }\n");
    out
}

fn engine() -> Engine {
    let mut engine = Engine::new();
    let (trig, turtle) = {
        let all = data();
        let cut = all.find("ex:g1 {").expect("named graph block");
        (all[cut..].to_owned(), all[..cut].to_owned())
    };
    engine
        .bulk_load(turtle.as_bytes(), RdfFormat::Turtle, None)
        .expect("load default graph");
    let prefixed = format!(
        "@prefix ex: <http://example.com/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n{trig}"
    );
    engine
        .bulk_load(prefixed.as_bytes(), RdfFormat::TriG, None)
        .expect("load named graph");
    engine
}

fn rows(engine: &Engine, query: &str, options: &QueryOptions) -> Vec<String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let (results, _) = Engine::query_with(&view, query, options).expect("query");
    let mut out: Vec<String> = match results {
        QueryResults::Solutions(iter) => iter
            .map(|s| {
                let s = s.expect("solution");
                let mut cells: Vec<String> = s
                    .iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect();
                cells.sort();
                cells.join(" ")
            })
            .collect(),
        _ => panic!("expected solutions"),
    };
    out.sort();
    out
}

/// The same query both ways. `spareval` is reached by asking for an explanation, which the
/// fast path declines to produce.
fn both(engine: &Engine, query: &str) -> (Vec<String>, Vec<String>) {
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let fast = rows(
        engine,
        query,
        &QueryOptions::new().reordering(Arc::clone(&stats)),
    );
    let slow = rows(engine, query, &QueryOptions::new().explaining());
    (fast, slow)
}

const P: &str = "PREFIX ex: <http://example.com/> \
                 PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";

fn agree(query: &str) {
    let engine = engine();
    let (fast, slow) = both(&engine, query);
    assert_eq!(fast, slow, "the bounded path disagreed with the evaluator");
    assert!(
        !fast.is_empty(),
        "the query matched nothing, so it proves nothing: {query}"
    );
}

#[test]
fn every_comparison_agrees() {
    for filter in [
        "?o > 30",
        "?o >= 30",
        "?o < 30",
        "?o <= 30",
        "?o > 0",
        "?o < 10",
        "30 < ?o",
        "30 >= ?o",
        // A negative bound, which the parser hands over as a unary minus.
        "?o > -5",
        // Bounds that fall outside the inline range entirely.
        "?o > 1000000",
        "?o < 1000000",
    ] {
        agree(&format!(
            "{P} SELECT ?s ?o WHERE {{ ?s ex:age ?o FILTER({filter}) }}"
        ));
    }
}

/// The pushdown must not fire where the object is already fixed, or where the filter is on
/// some other variable — and must not change the answer if it does.
#[test]
fn a_filter_on_another_variable_changes_nothing() {
    agree(&format!(
        "{P} SELECT ?s ?o ?n WHERE {{ ?s ex:age ?o ; ex:shoe ?n FILTER(?n > 5) }}"
    ));
    agree(&format!(
        "{P} SELECT ?s WHERE {{ ?s ex:age 30 FILTER(30 > 5) }}"
    ));
}

/// Two patterns, two bounds — the second scan runs with the first pattern's variable already
/// bound, which is the shape the operator exists for.
#[test]
fn two_bounded_patterns_agree() {
    agree(&format!(
        "{P} SELECT ?s ?a ?b WHERE {{ ?s ex:age ?a ; ex:shoe ?b FILTER(?a > 20 && ?b < 6) }}"
    ));
}

/// A bound inside `GRAPH`, which reaches the graph index orders.
#[test]
fn a_bound_inside_a_graph_agrees() {
    agree(&format!(
        "{P} SELECT ?g ?s ?o WHERE {{ GRAPH ?g {{ ?s ex:age ?o }} FILTER(?o > 10) }}"
    ));
}

/// A bound under `OPTIONAL`: the filter belongs to the optional, so a span must not narrow
/// the left side's scan.
#[test]
fn a_bound_under_an_optional_agrees() {
    agree(&format!(
        "{P} SELECT ?s ?o WHERE {{ ?s ex:name ?n OPTIONAL {{ ?s ex:age ?o FILTER(?o > 40) }} }}"
    ));
}

/// The comparison the module declines to bound must still give the right answer, by the
/// ordinary path.
#[test]
fn a_declined_comparison_agrees() {
    for filter in [
        "?o > 2.5",
        "?o > \"1.5\"^^xsd:double",
        "?o > \"abc\"",
        "?o != 30",
    ] {
        let engine = engine();
        let query = format!("{P} SELECT ?s ?o WHERE {{ ?s ex:age ?o FILTER({filter}) }}");
        let (fast, slow) = both(&engine, &query);
        assert_eq!(fast, slow, "{filter}");
    }
}

/// The case the whole design turns on: a decimal is a number and is not inline, so an
/// integer bound must still find it.
#[test]
fn a_decimal_survives_an_integer_bound() {
    let engine = engine();
    let query = format!("{P} SELECT ?s ?o WHERE {{ ?s ex:age ?o FILTER(?o > 30) }}");
    let (fast, slow) = both(&engine, &query);
    assert_eq!(fast, slow);
    assert!(
        fast.iter().any(|r| r.contains("30.5")),
        "the decimal above 30 was lost by the bounded scan: {fast:?}"
    );
    assert!(
        fast.iter().any(|r| r.contains("41.25")),
        "the double above 30 was lost: {fast:?}"
    );
    assert!(
        fast.iter().any(|r| r.contains("576460752303423489")),
        "the too-large integer was lost: {fast:?}"
    );
}

/// The pushdown has to actually happen.
///
/// Every other test here compares answers, and a pushdown changes no answer — so disabling
/// it entirely leaves them all passing. That is exactly the shape of a feature that quietly
/// stops working: correct, tested, and doing nothing. `bounded_scans` is the signal.
#[test]
fn a_bounded_query_really_bounds_its_scan() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);

    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let options = QueryOptions::new().reordering(stats);

    let (results, _) = Engine::query_with(
        &view,
        &format!("{P} SELECT ?s ?o WHERE {{ ?s ex:age ?o FILTER(?o > 30) }}"),
        &options,
    )
    .expect("query");
    if let QueryResults::Solutions(iter) = results {
        assert!(iter.count() > 0);
    }
    assert!(
        view.bounded_scans() > 0,
        "the filter should have bounded the scan, and did not"
    );
}

/// And it has to *not* happen where there is nothing to bound, or the counter says nothing.
#[test]
fn an_unfiltered_query_bounds_nothing() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let options = QueryOptions::new().reordering(stats);

    let (results, _) = Engine::query_with(
        &view,
        &format!("{P} SELECT ?s ?o WHERE {{ ?s ex:age ?o }}"),
        &options,
    )
    .expect("query");
    if let QueryResults::Solutions(iter) = results {
        assert!(iter.count() > 0);
    }
    assert_eq!(
        view.bounded_scans(),
        0,
        "a query with no comparison bounded a scan"
    );
}
