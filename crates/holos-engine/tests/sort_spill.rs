//! `ORDER BY` in bounded memory gives the same answer as `ORDER BY` in unbounded memory.
//!
//! The same discipline as `spill.rs` next door, and for the same reason: every way a
//! sort-spill-merge can go wrong produces a *plausible* answer. A run written unsorted, a
//! merge that prefers the wrong source, a key encoded one way and read back another — each
//! yields rows in an order that looks like an order, raises nothing, and is wrong. So the
//! central test is differential: the same query through a budget that never spills and a
//! budget that spills constantly, asserting the two agree row for row.
//!
//! The comparison is made over keys of **one** datatype throughout. That is not laziness:
//! SPARQL defines no order between, say, an integer and a string, `holos_engine::topk`
//! documents how the engine extends the order there, and a differential test over a mixed
//! key would be asserting that two implementations agree about something neither is obliged
//! to. Single-datatype keys are a total order, so the answer is fully determined and any
//! disagreement is a defect.

use holos_engine::{DatasetView, Engine, QueryOptions};
use holos_security::Session;
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;

const EX: &str = "http://holos.example/";

/// Enough rows that a small budget spills many times over.
const ROWS: usize = 4_000;

/// A store of `ROWS` people, each with a date and a rank, in an order that is not sorted by
/// either — a run boundary must therefore fall in the middle of the key space.
fn engine() -> Engine {
    let mut engine = Engine::new();
    for i in 0..ROWS {
        // A multiplier coprime with ROWS, so the dates arrive shuffled but every one is
        // distinct and the expected order is known. 336 = 12 months of 28, so the date is
        // injective over `day` and a sort on it has no ties to break.
        let day = (i * 1_237) % ROWS;
        let (year, month, dom) = (1800 + day / 336, 1 + (day % 336) / 28, 1 + day % 28);
        let subject = NamedNode::new_unchecked(format!("{EX}p{i:05}"));
        let quad = |predicate: &str, object: Literal| Quad {
            subject: subject.clone().into(),
            predicate: NamedNode::new_unchecked(format!("{EX}{predicate}")),
            object: object.into(),
            graph_name: GraphName::DefaultGraph,
        };
        engine
            .store_mut()
            .insert(
                quad(
                    "died",
                    Literal::new_typed_literal(
                        format!("{year:04}-{month:02}-{dom:02}"),
                        xsd::DATE,
                    ),
                )
                .as_ref(),
            )
            .expect("insert");
        // Ten ranks over four thousand rows, so almost every comparison on `?rank` ties and
        // the tie-break — arrival order, across a spill as well as within a run — is what
        // the answer depends on.
        engine
            .store_mut()
            .insert(
                quad(
                    "rank",
                    Literal::new_typed_literal((i % 10).to_string(), xsd::INTEGER),
                )
                .as_ref(),
            )
            .expect("insert");
    }
    engine
}

fn rows(view: &DatasetView<'_>, query: &str, options: &QueryOptions) -> Vec<String> {
    let (results, _) = Engine::query_with(view, query, options).expect("the query is answered");
    let QueryResults::Solutions(iter) = results else {
        panic!("expected solutions");
    };
    iter.map(|solution| {
        solution
            .expect("a row")
            .values()
            .iter()
            .map(|t| t.as_ref().map_or_else(String::new, ToString::to_string))
            .collect::<Vec<_>>()
            .join("\t")
    })
    .collect()
}

/// Spills on almost every row. Small enough that the merge has many runs to weave.
fn spilling() -> QueryOptions {
    QueryOptions::new().spilling(4 * 1024)
}

/// The same collector with room to hold everything, so it sorts in memory and writes no
/// runs. This is what spilling is measured against: the same code, the same order, one
/// difference — whether the rows went to disk on the way.
fn in_memory() -> QueryOptions {
    QueryOptions::new().spilling(64 << 20)
}

/// No budget, so the sort falls through to the evaluator.
fn evaluator() -> QueryOptions {
    QueryOptions::new()
}

/// The headline: spilling changes nothing but where the rows waited.
///
/// Every shape the collector can be handed, each answered by the same collector twice —
/// once with room to hold the sort and once with a budget it overruns constantly — and
/// asserted row for row. Anything the merge does wrong shows up here.
#[test]
fn spilling_and_not_spilling_give_the_same_rows() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let p = format!("PREFIX ex: <{EX}> ");

    for query in [
        // Every row, ascending: the case that has nothing to lean on but the merge.
        format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o"),
        format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o)"),
        // Two columns out, so a row must stay assembled through the round trip.
        format!("{p}SELECT ?s ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o)"),
        // A slice applied after the sort rather than by the heap.
        format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o OFFSET 3990"),
        // Two keys, opposite directions, the second breaking the first's ties.
        format!("{p}SELECT ?r ?o WHERE {{ ?s ex:rank ?r ; ex:died ?o }} ORDER BY DESC(?r) ASC(?o)"),
        // A key that is not projected.
        format!("{p}SELECT ?s WHERE {{ ?s ex:rank ?r ; ex:died ?o }} ORDER BY ?o"),
        // An expression key, and a key that ties ten ways so the tie-break carries the order.
        format!("{p}SELECT ?r WHERE {{ ?s ex:rank ?r }} ORDER BY DESC(?r * 3) OFFSET 10 LIMIT 20"),
        format!("{p}SELECT ?s ?r WHERE {{ ?s ex:rank ?r }} ORDER BY ?r"),
        // Unbound sorts first, and there are plenty of them: only the top ranks match.
        format!(
            "{p}SELECT ?o ?x WHERE {{ ?s ex:died ?o OPTIONAL {{ ?s ex:rank ?x FILTER(?x > 7) }} }} ORDER BY ?x ?o"
        ),
    ] {
        let spilled = rows(&view, &query, &spilling());
        let whole = rows(&view, &query, &in_memory());
        assert_eq!(spilled.len(), whole.len(), "row count differs: {query}");
        assert_eq!(spilled, whole, "order differs: {query}");
        assert!(!spilled.is_empty(), "nothing to compare: {query}");
    }
}

/// And the collector's order is the evaluator's, wherever the answer is determined.
///
/// Scoped to sort keys that are distinct — the dates, which the fixture makes injective —
/// because that is exactly where SPARQL fixes the answer. Rows a sort's conditions do not
/// separate may come back in any order, and the two differ there: this collector is stable,
/// so tied rows keep the order they arrived in, while the evaluator sorts with
/// `sort_unstable_by` and leaves them wherever the sort put them. Both are conformant, and
/// only the stable one is repeatable.
#[test]
fn the_order_matches_the_evaluator_where_the_keys_are_distinct() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let p = format!("PREFIX ex: <{EX}> ");

    for query in [
        format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o"),
        format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o)"),
        format!("{p}SELECT ?s ?o WHERE {{ ?s ex:died ?o }} ORDER BY DESC(?o)"),
        format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o OFFSET 3990"),
        format!("{p}SELECT ?s WHERE {{ ?s ex:rank ?r ; ex:died ?o }} ORDER BY ?o"),
    ] {
        let spilled = rows(&view, &query, &spilling());
        let theirs = rows(&view, &query, &evaluator());
        assert_eq!(spilled, theirs, "differs from the evaluator: {query}");
    }
}

/// Tied rows are the same rows either way, whatever order they come back in.
///
/// The claim the test above scopes out, asserted as the multiset it really is: the engine
/// and the evaluator answer a tie-heavy sort with the same bag of rows, and agree on the
/// one column the sort does fix.
#[test]
fn a_tie_heavy_sort_is_the_same_rows_as_the_evaluators() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?r WHERE {{ ?s ex:rank ?r }} ORDER BY ?r");

    let mut spilled = rows(&view, &query, &spilling());
    let mut theirs = rows(&view, &query, &evaluator());
    assert_eq!(spilled.len(), ROWS);
    assert_eq!(spilled.len(), theirs.len());
    // The rank each row carries is fixed by the sort; only the subjects within a rank are
    // free, so the sequence of ranks must agree even where the rows do not.
    let ranks = |rows: &[String]| -> Vec<String> {
        rows.iter()
            .map(|row| row.split_once('\t').expect("two columns").1.to_owned())
            .collect()
    };
    assert_eq!(ranks(&spilled), ranks(&theirs), "the ranks are not in order");
    spilled.sort();
    theirs.sort();
    assert_eq!(spilled, theirs, "different rows, not merely a different order");
}

/// The differential test is worth nothing if the small budget never actually spilled.
///
/// Asserted through the collector directly, because the engine does not report it: a sort
/// of four thousand rows against a four-kilobyte budget must write runs, and if a later
/// change makes the budget generous enough to hold them the test above stops testing
/// anything.
#[test]
fn a_small_budget_really_writes_runs() {
    use holos_engine::spill::Sorted;
    use oxrdf::Term;
    use spareval::ExpressionTerm;
    use std::sync::Arc;

    let descending: Arc<[bool]> = Arc::from(vec![false]);
    let mut sorted = Sorted::new(1, descending, 4 * 1024).expect("collector");
    for i in 0..ROWS {
        let value = Literal::new_typed_literal(format!("{i:06}"), xsd::INTEGER);
        let key = vec![Some(ExpressionTerm::from(Term::from(value.clone())))];
        sorted.push(key, vec![Some(Term::from(value))]).expect("push");
    }
    assert!(sorted.runs() > 1, "nothing spilled: {} runs", sorted.runs());

    let scratch = sorted.scratch().to_path_buf();
    assert!(scratch.exists(), "the runs went somewhere else");
    let mut merged = sorted.merge().expect("merge");
    let mut seen = Vec::new();
    while let Some(row) = merged.next_row().expect("row") {
        seen.push(row[0].as_ref().map(ToString::to_string).unwrap_or_default());
    }
    assert_eq!(seen.len(), ROWS);
    let mut expected = seen.clone();
    expected.sort();
    assert_eq!(seen, expected, "the merge came back out of order");

    // Scratch files are the kind of thing that gets left on a disk for a year, and the
    // readers hold the run files open right up until the merge is dropped.
    drop(merged);
    assert!(!scratch.exists(), "{} was left behind", scratch.display());
}

/// Rows whose keys do not separate them keep the order they arrived in, spilled or not.
///
/// The ranks tie ten ways over four thousand rows, so this is most of the answer. A merge
/// that preferred the later run, or an unstable sort inside a run, would shuffle them and
/// still look sorted.
#[test]
fn ties_keep_arrival_order_across_a_spill() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!(
        "PREFIX ex: <{EX}> SELECT ?s ?r WHERE {{ ?s ex:rank ?r }} ORDER BY ?r"
    );

    let spilled = rows(&view, &query, &spilling());
    assert_eq!(spilled.len(), ROWS);

    // Within each rank the subjects must be ascending, because that is the order the store
    // hands them back and nothing in the sort is entitled to change it.
    let mut by_rank: Vec<(String, Vec<String>)> = Vec::new();
    for row in &spilled {
        let (subject, rank) = row.split_once('\t').expect("two columns");
        match by_rank.last_mut() {
            Some((last, subjects)) if last == rank => subjects.push(subject.to_owned()),
            _ => by_rank.push((rank.to_owned(), vec![subject.to_owned()])),
        }
    }
    assert_eq!(by_rank.len(), 10, "one group per rank, in order");
    for (rank, subjects) in &by_rank {
        let mut sorted = subjects.clone();
        sorted.sort();
        assert_eq!(subjects, &sorted, "rank {rank} came back out of arrival order");
    }
}

/// An empty answer, and a sort of one row, through the spilling path.
#[test]
fn the_edges_hold() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let p = format!("PREFIX ex: <{EX}> ");

    assert!(rows(
        &view,
        &format!("{p}SELECT ?o WHERE {{ ?s ex:nothing ?o }} ORDER BY ?o"),
        &spilling()
    )
    .is_empty());
    assert_eq!(
        rows(
            &view,
            &format!("{p}SELECT ?o WHERE {{ <{EX}p00000> ex:died ?o }} ORDER BY ?o"),
            &spilling()
        )
        .len(),
        1
    );
    // Past the end of the answer.
    assert!(rows(
        &view,
        &format!("{p}SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o OFFSET 99999"),
        &spilling()
    )
    .is_empty());
}

/// A sort that would write more scratch than it is allowed is refused, not left to fill
/// the volume.
///
/// The hazard this closes: `--spill-bytes` bounds what an operator *holds* and says nothing
/// about what it *writes*, and a sort writes in proportion to its input. Until the ceiling
/// existed, a sort over a large store would write until the disk holding the scratch
/// directory — very often the system volume — was full, where before it spilled at all the
/// same query was refused in seconds by the memory ceiling. Trading a refusal for a full
/// volume is not an improvement.
#[test]
fn a_sort_that_spills_too_much_is_refused() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o");

    // Spills constantly, and is allowed a fraction of what that adds up to.
    let options = QueryOptions::new().spilling(4 * 1024).with_spill_disk(64 * 1024);
    let error = match Engine::query_with(&view, &query, &options) {
        Err(e) => e,
        Ok((results, _)) => {
            let QueryResults::Solutions(iter) = results else {
                panic!("expected solutions");
            };
            iter.filter_map(Result::err)
                .next()
                .expect("the ceiling should have stopped this sort")
                .into()
        }
    };

    assert_eq!(error.kind(), ("spill-ceiling", 500), "{error}");
    let detail = error.detail();
    assert!(
        detail.contains("spilled") && detail.contains("--max-spill-disk"),
        "the detail must name what it wrote and the flag: {detail}"
    );
}

/// And the same sort finishes when the ceiling is removed, so the test above is measuring
/// the ceiling and not some other failure.
#[test]
fn the_same_sort_finishes_without_a_ceiling() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!("PREFIX ex: <{EX}> SELECT ?s ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o");
    let options = QueryOptions::new().spilling(4 * 1024).with_spill_disk(0);
    assert_eq!(rows(&view, &query, &options).len(), ROWS);
}

/// A sort small enough never to spill is untouched by the ceiling, however low it is set.
#[test]
fn a_sort_that_never_spills_ignores_the_ceiling() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!(
        "PREFIX ex: <{EX}> SELECT ?o WHERE {{ ?s ex:died ?o }} ORDER BY ?o LIMIT 5000"
    );
    let options = QueryOptions::new().spilling(64 << 20).with_spill_disk(1);
    assert_eq!(rows(&view, &query, &options).len(), ROWS);
}
