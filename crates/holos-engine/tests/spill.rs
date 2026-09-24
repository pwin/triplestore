//! `DISTINCT` in bounded memory gives the same answer as `DISTINCT` in unbounded memory.
//!
//! Every way a sort-spill-merge can go wrong produces a *plausible* answer. A run written
//! unsorted merges into a stream that is not sorted, so duplicates separated by one row
//! survive and the count comes out high. A dedup that compares the wrong bytes merges rows
//! that differ, and the count comes out low. Neither raises an error, and neither is visible
//! without something to compare against.
//!
//! So the central test is differential: the same rows through a budget that never spills and
//! a budget that spills constantly, asserting the two agree. A single-configuration test
//! would pass against an implementation that silently loses half the answer.

use holos_engine::spill::{self, Distinct};
use oxrdf::{Literal, NamedNode, Term, Variable};
use spareval::{QuerySolution, QuerySolutionIter};
use std::sync::Arc;

const EX: &str = "http://holos.example/";

fn variables() -> Arc<[Variable]> {
    Arc::from(vec![
        Variable::new_unchecked("s"),
        Variable::new_unchecked("o"),
    ])
}

/// `count` rows over `distinct` distinct values, interleaved so duplicates are far apart.
///
/// Far apart on purpose: duplicates that arrive next to each other are caught by any
/// implementation, including a broken one that only compares neighbours in arrival order.
fn rows(count: usize, distinct: usize) -> Vec<Vec<Option<Term>>> {
    (0..count)
        .map(|i| {
            let n = i % distinct;
            vec![
                Some(Term::from(NamedNode::new_unchecked(format!("{EX}s{n}")))),
                Some(Term::from(Literal::new_simple_literal(format!(
                    "value {n}"
                )))),
            ]
        })
        .collect()
}

fn solutions(rows: Vec<Vec<Option<Term>>>) -> QuerySolutionIter<'static> {
    QuerySolutionIter::from_tuples(variables(), rows.into_iter().map(Ok))
}

/// Runs the deduplicator and returns the rows it kept, sorted for comparison.
fn deduplicated(rows: Vec<Vec<Option<Term>>>, budget: usize) -> Vec<String> {
    let out = spill::deduplicate(solutions(rows), budget).expect("dedup");
    let mut kept: Vec<String> = out
        .map(|solution| {
            let solution = solution.expect("solution");
            format!(
                "{:?}|{:?}",
                solution.get(0).map(std::string::ToString::to_string),
                solution.get(1).map(std::string::ToString::to_string)
            )
        })
        .collect();
    kept.sort();
    kept
}

/// The differential test. Spilling must change memory and nothing else.
#[test]
fn spilling_and_not_spilling_give_the_same_rows() {
    let input = rows(20_000, 5_000);

    let whole = deduplicated(input.clone(), usize::MAX);
    // Small enough that twenty thousand rows spill many times over.
    let spilled = deduplicated(input, 16 * 1024);

    assert_eq!(
        whole.len(),
        5_000,
        "the unspilled run lost or kept too many"
    );
    assert_eq!(spilled, whole, "spilling changed the answer");
}

/// And the spilled run must actually have spilled, or the test above compares two identical
/// code paths and proves nothing.
#[test]
fn a_small_budget_really_writes_runs() {
    let mut distinct = Distinct::new(2, 16 * 1024).expect("collector");
    for row in rows(20_000, 5_000) {
        let solution: QuerySolution = (variables(), row).into();
        distinct.push(&solution).expect("push");
    }
    assert!(
        distinct.runs() > 1,
        "the budget was too generous to spill, so the differential test is vacuous"
    );
}

/// Terms that differ only in ways a careless encoding would flatten must stay distinct.
///
/// `"1"^^xsd:integer` and `"01"^^xsd:integer` are different RDF terms with the same value,
/// and `DISTINCT` keeps both. An encoding that compared values, or that dropped the
/// datatype, would merge them and quietly under-count.
#[test]
fn terms_that_are_equal_in_value_but_not_in_form_stay_distinct() {
    let integer = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer");
    let mut input = Vec::new();
    for lexical in ["1", "01", "1.0"] {
        input.push(vec![
            Some(Term::from(NamedNode::new_unchecked(format!("{EX}s")))),
            Some(Term::from(Literal::new_typed_literal(
                lexical,
                integer.clone(),
            ))),
        ]);
    }
    // A plain string "1" is a fourth distinct term: same lexical form, different datatype.
    input.push(vec![
        Some(Term::from(NamedNode::new_unchecked(format!("{EX}s")))),
        Some(Term::from(Literal::new_simple_literal("1"))),
    ]);

    assert_eq!(deduplicated(input.clone(), usize::MAX).len(), 4);
    assert_eq!(
        deduplicated(input, 1).len(),
        4,
        "spilled, and merged too far"
    );
}

/// An unbound slot is a value like any other, and distinguishable from a bound one.
#[test]
fn unbound_slots_survive_the_round_trip() {
    let bound = Term::from(NamedNode::new_unchecked(format!("{EX}s")));
    let input = vec![
        vec![Some(bound.clone()), None],
        vec![None, Some(bound.clone())],
        vec![None, None],
        // A repeat of the first, to prove the encoding of `None` is stable.
        vec![Some(bound), None],
    ];
    assert_eq!(deduplicated(input.clone(), usize::MAX).len(), 3);
    assert_eq!(deduplicated(input, 1).len(), 3);
}

/// Nothing in, nothing out — and no run files left behind.
#[test]
fn an_empty_input_produces_no_rows() {
    assert!(deduplicated(Vec::new(), usize::MAX).is_empty());
    assert!(deduplicated(Vec::new(), 1).is_empty());
}

/// Scratch files are the kind of thing that gets left on a disk for a year.
///
/// Asserted against *this* collector's directory rather than by counting what is in the
/// temp directory: these tests run in parallel and each creates its own, so a count is a
/// race rather than a check.
#[test]
fn run_files_are_cleaned_up() {
    let scratch = {
        let mut distinct = Distinct::new(2, 16 * 1024).expect("collector");
        for row in rows(20_000, 5_000) {
            let solution: QuerySolution = (variables(), row).into();
            distinct.push(&solution).expect("push");
        }
        assert!(
            distinct.runs() > 1,
            "nothing spilled, so nothing to clean up"
        );
        assert!(distinct.scratch().exists(), "the runs went somewhere else");
        distinct.scratch().to_path_buf()
    };
    assert!(!scratch.exists(), "{} was left behind", scratch.display());
}

/// Two collectors alive at once must not share a directory, or one reads the other's runs.
#[test]
fn two_collectors_do_not_collide() {
    let a = Distinct::new(2, 16 * 1024).expect("collector");
    let b = Distinct::new(2, 16 * 1024).expect("collector");
    assert_ne!(a.scratch(), b.scratch());
}

// -----------------------------------------------------------------------------------
// End to end: the spilling DISTINCT must answer what the hash-set DISTINCT answers.
// -----------------------------------------------------------------------------------

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use spareval::QueryResults;

const N: usize = 8_000;

/// Every subject appears twice, so `DISTINCT` has real work and the answer is known.
fn store_engine() -> Engine {
    let mut store = holos_store::Store::new();
    for i in 0..N {
        for p in ["a", "b"] {
            store
                .insert(
                    oxrdf::Quad {
                        subject: NamedNode::new_unchecked(format!("{EX}s{i}")).into(),
                        predicate: NamedNode::new_unchecked(format!("{EX}{p}")),
                        object: Literal::new_simple_literal("x").into(),
                        graph_name: oxrdf::GraphName::DefaultGraph,
                    }
                    .as_ref(),
                )
                .expect("insert");
        }
    }
    Engine::with_store(store)
}

/// Options that force the spill path: statistics, a one-row blocking budget so admission
/// control fires, and a spill budget so the answer comes from sorting rather than a refusal.
///
/// Needed since spilling stopped being the default. It is 10x slower than `spareval`'s hash
/// set on a result that fits, so it is reserved for results that do not — which means a test
/// that only sets `spilling_distinct` exercises `spareval` and proves nothing.
fn forced(engine: &Engine, spill_bytes: usize) -> QueryOptions {
    let stats = std::sync::Arc::new(
        holos_stats::Statistics::build(engine.store(), holos_store::GraphFilter::Default)
            .expect("statistics"),
    );
    QueryOptions::new()
        .reordering(stats)
        .with_blocking_budget(1)
        .spilling(spill_bytes)
}

fn answer(engine: &Engine, query: &str, options: &QueryOptions) -> Vec<String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let (results, _) = Engine::query_with(&view, query, options).expect("query");
    let QueryResults::Solutions(iter) = results else {
        panic!("expected solutions");
    };
    let mut rows: Vec<String> = iter
        .map(|s| {
            s.expect("solution")
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

/// `SELECT DISTINCT` through the spill gives what `spareval`'s hash set gives.
#[test]
fn a_spilled_select_distinct_matches_the_hash_set() {
    let engine = store_engine();
    let query = format!("PREFIX ex: <{EX}> SELECT DISTINCT ?s WHERE {{ ?s ?p ?o }}");

    let hashed = answer(&engine, &query, &QueryOptions::new());
    // A budget of a few kilobytes, so eight thousand subjects spill many times.
    let spilled = answer(&engine, &query, &forced(&engine, 4 * 1024));

    assert_eq!(
        hashed.len(),
        N,
        "the hash-set answer is not what was expected"
    );
    assert_eq!(spilled, hashed, "spilling changed the answer");
}

/// And with a `LIMIT`, which sits above the `DISTINCT` and must still apply.
#[test]
fn a_limit_above_a_spilled_distinct_still_applies() {
    let engine = store_engine();
    let query = format!("PREFIX ex: <{EX}> SELECT DISTINCT ?s WHERE {{ ?s ?p ?o }} LIMIT 5");
    let spilled = answer(&engine, &query, &forced(&engine, 4 * 1024));
    assert_eq!(spilled.len(), 5);
}

/// The query that took a server down, answered rather than refused.
///
/// `COUNT(DISTINCT *)` is the shape whose hash set held every distinct solution. Counted
/// through the spill it holds a buffer instead, so the answer is O(1) in memory however
/// large the input.
#[test]
fn the_query_that_took_down_a_server_is_answered_by_spilling() {
    let engine = store_engine();
    let query = "SELECT (count(distinct *) as ?count) WHERE { ?sub ?pred ?obj . }";

    let hashed = answer(&engine, query, &QueryOptions::new());
    let spilled = answer(&engine, query, &forced(&engine, 4 * 1024));

    assert_eq!(spilled, hashed, "spilling changed the count");
    assert!(
        spilled[0].contains(&(N * 2).to_string()),
        "counted {spilled:?}, expected {} distinct rows",
        N * 2
    );
}

/// A query with no `DISTINCT` must be untouched by the option being on.
#[test]
fn the_spill_option_leaves_other_queries_alone() {
    let engine = store_engine();
    let query = format!("PREFIX ex: <{EX}> SELECT ?s WHERE {{ ?s ex:a ?o }}");
    let plain = answer(&engine, &query, &QueryOptions::new());
    let with = answer(&engine, &query, &forced(&engine, 4 * 1024));
    assert_eq!(with, plain);
    assert_eq!(plain.len(), N);
}

/// Spilling wins over admission control: a query that can be answered must not be refused.
///
/// The two exist for different reasons. Admission control declines operators nothing can
/// bound; a `DISTINCT` that sorts and spills *is* bounded, so its size stops being a reason
/// to decline it. If the order were the other way round, turning on spilling would have no
/// effect on exactly the queries it was built for.
#[test]
fn a_spillable_distinct_is_answered_rather_than_refused() {
    let engine = store_engine();
    let query = "SELECT (count(distinct *) as ?count) WHERE { ?sub ?pred ?obj . }";
    let stats = std::sync::Arc::new(
        holos_stats::Statistics::build(engine.store(), holos_store::GraphFilter::Default)
            .expect("statistics"),
    );

    // A budget of one row: without spilling this is refused outright.
    let refusing = QueryOptions::new()
        .reordering(std::sync::Arc::clone(&stats))
        .with_blocking_budget(1);
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    assert!(
        Engine::query_with(&view, query, &refusing).is_err(),
        "the budget should refuse this without spilling"
    );
    drop(view);

    // The same budget, with spilling on: answered.
    let spilling = refusing.spilling(4 * 1024);
    let rows = answer(&engine, query, &spilling);
    assert!(
        rows[0].contains(&(N * 2).to_string()),
        "expected {} distinct rows, got {rows:?}",
        N * 2
    );
}
