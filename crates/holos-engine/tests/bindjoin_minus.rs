//! `MINUS`, blank nodes, and property paths through the bind join.
//!
//! `MINUS` removes a solution when a compatible one exists on the right *and the two share a
//! variable* — with disjoint domains it removes nothing, which is the rule people are
//! surprised by and the one an implementation gets wrong quietly. Everything here is checked
//! against the evaluator over the same store rather than against an expectation written out
//! by hand, because a subtraction that takes away one row too many looks like a working
//! query.
//!
//! Blank nodes and sequence paths share a file with it because they share a mechanism: the
//! parser desugars `:p/:q` into a BGP joined on an anonymous blank node, so both arrive as
//! the same problem — a pattern position that binds like a variable and cannot be projected.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .

ex:a a ex:Person ; ex:name "A" ; ex:knows ex:b ; ex:city ex:London .
ex:b a ex:Person ; ex:name "B" ; ex:knows ex:c .
ex:c a ex:Person ; ex:name "C" ; ex:city ex:Paris .
ex:d a ex:Person ; ex:name "D" .

ex:London ex:country ex:UK .
ex:Paris  ex:country ex:FR .
ex:x ex:unrelated ex:y .
"#;

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

fn render(results: QueryResults<'_>) -> Vec<String> {
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

fn agrees(sparql: &str, label: &str) {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let parsed = spargebra::SparqlParser::new()
        .parse_query(sparql)
        .expect("parse");

    let (results, _) = Engine::query_with(&view, sparql, &QueryOptions::new()).expect("query");
    let ours = render(results);
    let reference = render(
        Engine::evaluator()
            .prepare(&parsed)
            .execute(&view)
            .expect("reference"),
    );
    assert_eq!(
        ours, reference,
        "{label}: the bind join disagrees with the evaluator"
    );
}

const PREFIX: &str = "PREFIX ex: <http://example.com/>\n";

#[test]
fn minus_removes_the_rows_that_match() {
    agrees(
        &format!("{PREFIX}SELECT ?s WHERE {{ ?s a ex:Person MINUS {{ ?s ex:city ?c }} }}"),
        "ex:b and ex:d have no city",
    );
}

/// A right side that matches nothing takes nothing away.
#[test]
fn minus_over_a_pattern_that_never_matches_keeps_everything() {
    agrees(
        &format!("{PREFIX}SELECT ?s WHERE {{ ?s a ex:Person MINUS {{ ?s ex:absent ?c }} }}"),
        "nothing on the right",
    );
}

/// `MINUS` binds nothing. Its right side's variables must not escape into the outer scope,
/// which is what "evaluated over separate domains" means.
#[test]
fn minus_does_not_bind_its_own_variables() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?name WHERE {{ ?s ex:name ?name \
             MINUS {{ ?s ex:city ?c }} }}"
        ),
        "?c is not projected and does not join",
    );
}

/// Several right-hand matches remove the row once, not once each.
#[test]
fn minus_removes_a_row_once_however_many_times_it_matches() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s WHERE {{ ?s a ex:Person MINUS {{ ?s ex:knows ?k . ?k ex:name ?n }} }}"
        ),
        "a right side that can match more than once",
    );
}

#[test]
fn minus_composes_with_the_rest_of_the_fragment() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?c WHERE {{ ?s a ex:Person . OPTIONAL {{ ?s ex:city ?c }} \
             MINUS {{ ?s ex:knows ?k }} }}"
        ),
        "an optional and a minus together",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT DISTINCT ?s WHERE {{ ?s a ex:Person MINUS {{ ?s ex:city ?c }} }} LIMIT 2"
        ),
        "modifiers over a minus",
    );
    // A `MINUS` with something still to do after it. The verdict has to be read off the
    // right side alone: run the two together and "did it match" becomes "did the right side
    // match *and* everything after it succeed", which keeps rows that should have gone.
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?k WHERE {{ ?s a ex:Person MINUS {{ ?s ex:city ?c }}              OPTIONAL {{ ?s ex:knows ?k }} }}"
        ),
        "a minus followed by an optional",
    );
    // And the reverse, so neither order goes unchecked.
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?c WHERE {{ ?s a ex:Person              MINUS {{ ?s ex:knows ?k }} MINUS {{ ?s ex:city ?c }} }}"
        ),
        "two minuses in a row",
    );
}

/// `MINUS` binds nothing, so a variable only its right side mentions is out of scope and
/// unbound — not joined, and not an error.
#[test]
fn a_variable_only_the_minus_mentions_is_out_of_scope() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?c WHERE {{ ?s a ex:Person MINUS {{ ?s ex:city ?c }} }}"),
        "?c projected but bound only inside the minus",
    );
}

/// The rule people are surprised by: disjoint domains remove nothing at all, however many
/// solutions the right side has. Refused here rather than implemented, so what matters is
/// that the answer is still right.
#[test]
fn minus_with_a_disjoint_domain_removes_nothing() {
    agrees(
        &format!("{PREFIX}SELECT ?s WHERE {{ ?s a ex:Person MINUS {{ ?x ex:unrelated ?y }} }}"),
        "nothing shared, so nothing removed",
    );
}

// ------------------------------------------------------------------ blank nodes and paths

#[test]
fn a_blank_node_binds_like_a_variable() {
    agrees(
        &format!("{PREFIX}SELECT ?s WHERE {{ ?s ex:knows [ ex:name ?n ] }}"),
        "the `[ ]` form",
    );
    agrees(
        &format!("{PREFIX}SELECT ?s ?n WHERE {{ ?s ex:knows _:m . _:m ex:name ?n }}"),
        "an explicit blank node label",
    );
}

/// `_:x` and `?x` are different things, and a rename that mapped them together would join
/// them. The name the rename produces is not valid SPARQL, so it cannot collide.
#[test]
fn a_blank_node_does_not_collide_with_a_variable_of_the_same_name() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?x WHERE {{ ?s ex:knows _:x . ?s ex:name ?x }}"),
        "`_:x` beside `?x`",
    );
}

#[test]
fn sequence_and_inverse_paths_are_ordinary_patterns() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?c WHERE {{ ?s ex:city/ex:country ?c }}"),
        "a sequence path",
    );
    agrees(
        &format!("{PREFIX}SELECT ?s ?k WHERE {{ ?s ^ex:knows ?k }}"),
        "an inverse path",
    );
    agrees(
        &format!("{PREFIX}SELECT ?s ?n WHERE {{ ?s ex:knows/ex:knows/ex:name ?n }}"),
        "a longer sequence",
    );
}

#[test]
fn an_alternative_path_is_a_union() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?o WHERE {{ ?s ex:name|ex:city ?o }}"),
        "two alternatives",
    );
    agrees(
        &format!("{PREFIX}SELECT ?s ?o WHERE {{ ?s ex:name|ex:city|ex:knows ?o }}"),
        "three alternatives",
    );
}

/// The closure paths need a fixpoint traversal this operator does not have. Refused, and the
/// answers still have to be right.
#[test]
fn closure_paths_are_answered_by_the_evaluator() {
    for (sparql, label) in [
        ("SELECT ?s ?k WHERE { ?s ex:knows+ ?k }", "one or more"),
        ("SELECT ?s ?k WHERE { ?s ex:knows* ?k }", "zero or more"),
        ("SELECT ?s ?k WHERE { ?s ex:knows? ?k }", "zero or one"),
    ] {
        agrees(&format!("{PREFIX}{sparql}"), label);
    }
}

#[test]
fn the_fragment_is_actually_exercised() {
    for sparql in [
        "SELECT ?s WHERE { ?s a ex:Person MINUS { ?s ex:city ?c } }",
        "SELECT ?s WHERE { ?s ex:knows [ ex:name ?n ] }",
        "SELECT ?s ?c WHERE { ?s ex:city/ex:country ?c }",
        "SELECT ?s ?k WHERE { ?s ^ex:knows ?k }",
        "SELECT ?s ?o WHERE { ?s ex:name|ex:city ?o }",
    ] {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_some(),
            "the fragment should take this, or the agreement tests prove nothing: {sparql}"
        );
    }

    // What stays out, and why.
    //
    // `ORDER BY` and aggregation need a comparator and a grouping engine of their own.
    // SPARQL leaves the order of incomparable terms to the implementation, so matching the
    // evaluator's sequence means matching its tie-breaking — duplicating the code rather than
    // reusing it, and two implementations that must agree exactly are a defect waiting to
    // happen. The closure paths need a fixpoint traversal. A `MINUS` sharing nothing with its
    // left side removes nothing, and rather than reason about whether the shared variable
    // might be one an optional left unbound, it is declined.
    for sparql in [
        "SELECT ?n WHERE { ?s ex:name ?n } ORDER BY ?n",
        "SELECT (COUNT(*) AS ?c) WHERE { ?s ex:name ?n }",
        "SELECT ?s ?k WHERE { ?s ex:knows+ ?k }",
        "SELECT ?s WHERE { ?s a ex:Person MINUS { ?x ex:unrelated ?y } }",
    ] {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_none(),
            "this is outside the fragment and must be refused: {sparql}"
        );
    }
}
