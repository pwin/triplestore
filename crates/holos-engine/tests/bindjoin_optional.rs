//! `OPTIONAL` through the bind join must answer what the evaluator answers.
//!
//! `OPTIONAL` is the construct that does not compose. A left join neither commutes nor
//! associates with a join, so an operator that reorders — which is the entire point of this
//! one — can change the answer rather than the time it takes to get it. And it changes it
//! *quietly*: a wrong left join returns rows, just not the right ones.
//!
//! So the test is differential and the comparison is exact. Every query here runs twice, once
//! through `bindjoin` and once through the evaluator with the operator refused, and the two
//! multisets of solutions must be equal. A query the fragment declines is not a failure — the
//! fallback is the design — but it must be *declined*, not answered differently, and
//! [`the_fragment_is_actually_exercised`] checks the ones that matter are taken.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .

ex:a  a ex:Person ; ex:name "A" ; ex:age 30 ; ex:city ex:London .
ex:b  a ex:Person ; ex:name "B" ; ex:age 41 .
ex:c  a ex:Person ; ex:name "C" ; ex:city ex:Paris .
ex:d  a ex:Person .
ex:e  a ex:Person ; ex:name "E" ; ex:name "E2" ; ex:city ex:London ; ex:city ex:Paris .

ex:London ex:country ex:UK .
ex:Paris  ex:country ex:FR .
ex:Berlin ex:country ex:DE .
"#;

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

/// Solutions as sorted, rendered rows — a multiset comparison that survives any ordering.
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

/// The two answers must be the same multiset.
///
/// The reference side is the evaluator over the same view — same store, same policy, same
/// custom functions. The only difference is which operator answered, which is what makes a
/// disagreement attributable.
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
fn an_unmatched_optional_leaves_its_variables_unbound() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city WHERE {{ ?s a ex:Person . OPTIONAL {{ ?s ex:city ?city }} }}"
        ),
        "ex:b and ex:d have no city",
    );
}

#[test]
fn a_matched_optional_binds_them() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?name WHERE {{ ?s a ex:Person . OPTIONAL {{ ?s ex:name ?name }} }}"
        ),
        "most have a name",
    );
}

/// One left row against several right rows is a cross product, not a choice.
#[test]
fn an_optional_matching_twice_yields_two_rows() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city WHERE {{ ?s ex:name ?n . OPTIONAL {{ ?s ex:city ?city }} }}"
        ),
        "ex:e has two names and two cities",
    );
}

/// Two optionals chain: the second sees what the first bound. Their order is part of the
/// meaning, which is why they are not reordered among themselves.
#[test]
fn two_optionals_chain_in_source_order() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city ?country WHERE {{\n\
             ?s a ex:Person .\n\
             OPTIONAL {{ ?s ex:city ?city }}\n\
             OPTIONAL {{ ?city ex:country ?country }} }}"
        ),
        "the second optional reads the first's binding",
    );
}

/// A filter *inside* the optional decides whether it matched. Applied afterwards it would
/// drop the row instead of leaving the variables unbound — a different answer.
#[test]
fn a_filter_inside_the_optional_decides_the_match() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?age WHERE {{ ?s a ex:Person . \
             OPTIONAL {{ ?s ex:age ?age FILTER(?age > 35) }} }}"
        ),
        "ex:a is 30, so its optional does not match and ?age is unbound",
    );
}

/// The left join's own condition, which is the same thing written differently.
#[test]
fn the_left_join_condition_decides_the_match() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city WHERE {{ ?s ex:age ?age . \
             OPTIONAL {{ ?s ex:city ?city FILTER(?age < 35) }} }}"
        ),
        "the condition reads the left side too",
    );
}

/// A filter *outside* the optional applies to the joined result, unbound variables included —
/// which for SPARQL means an error, which means false.
#[test]
fn a_filter_outside_the_optional_applies_after_it() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city WHERE {{ {{ ?s a ex:Person . \
             OPTIONAL {{ ?s ex:city ?city }} }} FILTER(BOUND(?city)) }}"
        ),
        "BOUND over an optional variable",
    );
}

#[test]
fn optional_composes_with_union_and_values() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city WHERE {{\n\
             {{ ?s ex:name ?n }} UNION {{ ?s ex:age ?n }}\n\
             OPTIONAL {{ ?s ex:city ?city }} }}"
        ),
        "union on the left of an optional",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city WHERE {{\n\
             VALUES ?s {{ ex:a ex:b ex:zzz }}\n\
             OPTIONAL {{ ?s ex:city ?city }} }}"
        ),
        "values on the left of an optional",
    );
}

#[test]
fn optional_composes_with_distinct_limit_and_offset() {
    for tail in ["", "LIMIT 3", "OFFSET 2", "LIMIT 2 OFFSET 1"] {
        agrees(
            &format!(
                "{PREFIX}SELECT DISTINCT ?city WHERE {{ ?s a ex:Person . \
                 OPTIONAL {{ ?s ex:city ?city }} }} {tail}"
            ),
            &format!("distinct with `{tail}`"),
        );
    }
}

/// The non-well-designed case. `?city` escapes its optional into a required pattern, so
/// evaluating the optional last would join against a binding the original query never had.
/// The fragment must decline rather than answer differently — and the answer is what this
/// checks, because declining is only interesting if it keeps the result right.
#[test]
fn a_variable_escaping_its_optional_is_still_answered_correctly() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?city ?country WHERE {{\n\
             {{ ?s a ex:Person . OPTIONAL {{ ?s ex:city ?city }} }}\n\
             ?city ex:country ?country }}"
        ),
        "not well designed: the optional's variable is read outside it",
    );
}

/// An optional over an empty left side, and one that never matches at all.
#[test]
fn degenerate_optionals() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?x WHERE {{ ?s a ex:Nobody . OPTIONAL {{ ?s ex:name ?x }} }}"),
        "nothing on the left",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?x WHERE {{ ?s a ex:Person . OPTIONAL {{ ?s ex:absent ?x }} }}"
        ),
        "nothing on the right, ever",
    );
}

/// The tests above prove agreement, which a fragment that refused every one of them would
/// also prove. This proves the fragment is actually taken.
#[test]
fn the_fragment_is_actually_exercised() {
    let taken = [
        "SELECT ?s ?city WHERE { ?s a ex:Person . OPTIONAL { ?s ex:city ?city } }",
        "SELECT ?s ?age WHERE { ?s a ex:Person . OPTIONAL { ?s ex:age ?age FILTER(?age > 35) } }",
        "SELECT ?s ?c ?a WHERE { ?s a ex:Person . OPTIONAL { ?s ex:city ?c } OPTIONAL { ?s ex:age ?a } }",
    ];
    for sparql in taken {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_some(),
            "the fragment should take this, or the agreement tests prove nothing: {sparql}"
        );
    }

    // And two it must not take.
    //
    // The first is the plain escape: `?city` is read by a required pattern, so hoisting the
    // optional past it would join against a binding the original query never had.
    //
    // The second is subtler, and is why the refusal is not narrowed to only the first.
    // Flattening loses the difference between `A OPTIONAL{B} OPTIONAL{C}`, where `C` really
    // does see what `B` bound, and `{A OPTIONAL{B}} . {C OPTIONAL{D}}`, where `D` does not --
    // both arrive here as one list of items and one list of optionals. Rather than
    // reconstruct the nesting to tell them apart, an optional whose fresh variables anything
    // else reads is declined. It costs a fallback on a shape that is legal and uncommon.
    let refused = [
        "SELECT ?s ?city ?country WHERE { { ?s a ex:Person . OPTIONAL { ?s ex:city ?city } } ?city ex:country ?country }",
        "SELECT ?s ?c ?k WHERE { ?s a ex:Person . OPTIONAL { ?s ex:city ?c } OPTIONAL { ?c ex:country ?k } }",
    ];
    for sparql in refused {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_none(),
            "hoisting this optional is not sound, so it must be refused: {sparql}"
        );
    }
}
