//! Subqueries through the bind join must answer what the evaluator answers.
//!
//! SPARQL evaluates a subquery bottom-up and joins its *projected* solutions. Flattening its
//! patterns into the surrounding conjunction is the same thing only when the projection hides
//! nothing — a variable the subquery binds and does not project is invisible outside it, and
//! flattening would expose it to join against an outer variable sharing the name.
//!
//! So the interesting content here is the refusals, and they are checked as refusals rather
//! than described: a comment claiming something is outside the fragment goes stale the moment
//! it is not.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:a ex:p ex:x ; ex:q ex:m ; ex:name "A" .
ex:b ex:p ex:y ; ex:q ex:n ; ex:name "B" .
ex:c ex:p ex:x ; ex:name "C" .
ex:x ex:kind ex:First .
ex:y ex:kind ex:Second .
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
fn a_select_star_subquery_is_a_grouping() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?o WHERE {{ {{ SELECT * WHERE {{ ?s ex:p ?o }} }} }}"),
        "braces used for readability",
    );
}

#[test]
fn a_subquery_projecting_everything_it_binds_is_spliced_in() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?o ?k WHERE {{ \
             {{ SELECT ?s ?o WHERE {{ ?s ex:p ?o }} }} ?o ex:kind ?k }}"
        ),
        "the subquery hides nothing, so its patterns join the conjunction",
    );
}

#[test]
fn a_subquery_composes_with_the_rest_of_the_fragment() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?o ?n WHERE {{ \
             {{ SELECT * WHERE {{ ?s ex:p ?o }} }} \
             OPTIONAL {{ ?s ex:name ?n }} }}"
        ),
        "a subquery on the left of an optional",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?o WHERE {{ \
             {{ SELECT * WHERE {{ ?s ex:p ?o FILTER(?o = ex:x) }} }} }}"
        ),
        "a filter inside a subquery",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT DISTINCT ?o WHERE {{ {{ SELECT * WHERE {{ ?s ex:p ?o }} }} }} LIMIT 2"
        ),
        "outer modifiers over a subquery",
    );
}

/// Whatever the fragment does, the answers have to be right — including for the shapes it
/// declines, which is the whole point of declining them.
#[test]
fn the_refused_shapes_are_still_answered_correctly() {
    for (sparql, label) in [
        (
            "SELECT ?s WHERE { { SELECT ?s WHERE { ?s ex:p ?o } } }",
            "hides ?o",
        ),
        // The case that makes the refusal necessary rather than cautious. The subquery hides
        // `?o`; the outer query binds a variable of that name to something else entirely.
        // Correct: the subquery contributes `?s`, and the outer pattern supplies `?o` from
        // `ex:q`. Flattened: the two `?o`s become one and the query demands that a subject's
        // `ex:p` and `ex:q` values coincide, which they never do — so it answers nothing.
        (
            "SELECT ?s ?o WHERE { { SELECT ?s WHERE { ?s ex:p ?o } } ?s ex:q ?o }",
            "hides ?o, and the outer query binds that name to something else",
        ),
        (
            "SELECT ?s ?o WHERE { { SELECT ?s WHERE { ?s ex:p ?o } } ?s ex:p ?o }",
            "hides ?o, and the outer query uses that name",
        ),
        (
            "SELECT ?s WHERE { ?s ex:q ?m . { SELECT ?s WHERE { ?s ex:p ?o } LIMIT 1 } }",
            "a subquery with its own LIMIT",
        ),
    ] {
        agrees(&format!("{PREFIX}{sparql}"), label);
    }
}

#[test]
fn the_fragment_is_actually_exercised() {
    for sparql in [
        "SELECT ?s ?o WHERE { { SELECT * WHERE { ?s ex:p ?o } } }",
        "SELECT ?s ?o ?k WHERE { { SELECT ?s ?o WHERE { ?s ex:p ?o } } ?o ex:kind ?k }",
        "SELECT ?s ?o ?n WHERE { { SELECT * WHERE { ?s ex:p ?o } } OPTIONAL { ?s ex:name ?n } }",
    ] {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_some(),
            "the fragment should take this, or the agreement tests prove nothing: {sparql}"
        );
    }

    // A projection that hides a variable, and a subquery carrying its own modifier. Both are
    // refused: the first because flattening would expose `?o` to the outer scope, the second
    // because `LIMIT` changes how many solutions there are and no join does that.
    for sparql in [
        "SELECT ?s WHERE { { SELECT ?s WHERE { ?s ex:p ?o } } }",
        "SELECT ?s WHERE { ?s ex:q ?m . { SELECT ?s WHERE { ?s ex:p ?o } LIMIT 1 } }",
    ] {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_none(),
            "this subquery is not a plain join, so it must be refused: {sparql}"
        );
    }
}
