//! `GRAPH` through the bind join must answer what the evaluator answers.
//!
//! Scanning used to be hard-wired to the default graph, so every `GRAPH` query fell back. The
//! failure mode of getting this wrong is the worst kind available to a store: answering
//! against the wrong graph looks exactly like answering, and under `DESIGN.md` §14 a named
//! graph is also a unit of access policy — so a scan that reached the wrong one would not
//! merely be incorrect, it would be a disclosure.
//!
//! Every query runs twice over the same view, once through `bindjoin` and once through the
//! evaluator, and the two multisets must be equal.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdf::{GraphName, NamedNode};
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const DEFAULT: &str = r#"
@prefix ex: <http://example.com/> .
ex:a ex:name "A-default" ; ex:kind ex:Thing .
ex:b ex:name "B-default" .
"#;

const G1: &str = r#"
@prefix ex: <http://example.com/> .
ex:a ex:name "A-one" ; ex:colour "red" .
ex:c ex:name "C-one" .
"#;

const G2: &str = r#"
@prefix ex: <http://example.com/> .
ex:a ex:name "A-two" ; ex:colour "blue" .
ex:d ex:name "D-two" .
"#;

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(DEFAULT.as_bytes(), RdfFormat::Turtle, None)
        .expect("default");
    for (iri, turtle) in [("g1", G1), ("g2", G2)] {
        engine
            .bulk_load_into_graph(
                turtle.as_bytes(),
                RdfFormat::Turtle,
                None,
                &GraphName::NamedNode(NamedNode::new_unchecked(format!(
                    "http://example.com/{iri}"
                ))),
            )
            .expect("named");
    }
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
fn a_named_graph_is_scanned_alone() {
    agrees(
        &format!("{PREFIX}SELECT ?s ?n WHERE {{ GRAPH ex:g1 {{ ?s ex:name ?n }} }}"),
        "only g1's names",
    );
}

/// The default graph is not one of the named ones, and `GRAPH ?g` must not reach it.
#[test]
fn a_graph_variable_ranges_over_named_graphs_only() {
    agrees(
        &format!("{PREFIX}SELECT ?g ?s ?n WHERE {{ GRAPH ?g {{ ?s ex:name ?n }} }}"),
        "g1 and g2, never the default graph",
    );
}

/// Two patterns in one block share the graph. The second must scan the graph the first
/// bound, not every graph again — and, more importantly, must not pair a subject from one
/// graph with a colour from another.
#[test]
fn two_patterns_in_one_block_stay_in_that_graph() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?g ?s ?n ?c WHERE {{ \
             GRAPH ?g {{ ?s ex:name ?n . ?s ex:colour ?c }} }}"
        ),
        "name and colour from the same graph",
    );
}

/// A pattern outside the block stays on the default graph. This is the join that would go
/// wrong if the scope were per-plan rather than per-pattern.
#[test]
fn the_default_graph_and_a_named_one_join() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?n ?k WHERE {{ ?s ex:kind ?k . GRAPH ex:g1 {{ ?s ex:name ?n }} }}"
        ),
        "kind from the default graph, name from g1",
    );
}

#[test]
fn two_blocks_name_two_graphs() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?a ?b WHERE {{ \
             GRAPH ex:g1 {{ ?s ex:colour ?a }} GRAPH ex:g2 {{ ?s ex:colour ?b }} }}"
        ),
        "the same subject in two graphs",
    );
}

#[test]
fn graph_composes_with_filter_union_values_and_optional() {
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?n WHERE {{ GRAPH ?g {{ ?s ex:name ?n }} FILTER(?g = ex:g2) }}"
        ),
        "a filter on the graph variable",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?n WHERE {{ \
             {{ GRAPH ex:g1 {{ ?s ex:name ?n }} }} UNION {{ GRAPH ex:g2 {{ ?s ex:name ?n }} }} }}"
        ),
        "a union of two graph blocks",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?n WHERE {{ VALUES ?s {{ ex:a ex:c }} \
             GRAPH ex:g1 {{ ?s ex:name ?n }} }}"
        ),
        "values driving a named-graph scan",
    );
    agrees(
        &format!(
            "{PREFIX}SELECT ?s ?k ?c WHERE {{ ?s ex:kind ?k . \
             OPTIONAL {{ GRAPH ex:g2 {{ ?s ex:colour ?c }} }} }}"
        ),
        "an optional whose body is a graph block",
    );
}

/// A graph nobody has heard of yields nothing, and must do so without a scan or an error.
#[test]
fn an_unknown_graph_is_empty() {
    agrees(
        &format!("{PREFIX}SELECT ?s WHERE {{ GRAPH ex:nowhere {{ ?s ex:name ?n }} }}"),
        "a graph the dictionary never saw",
    );
}

/// The fragment must actually be taking these, or every test above passes by falling back.
#[test]
fn the_fragment_is_actually_exercised() {
    for sparql in [
        "SELECT ?s ?n WHERE { GRAPH ex:g1 { ?s ex:name ?n } }",
        "SELECT ?g ?s ?n WHERE { GRAPH ?g { ?s ex:name ?n } }",
        "SELECT ?s ?n ?k WHERE { ?s ex:kind ?k . GRAPH ex:g1 { ?s ex:name ?n } }",
        "SELECT ?s ?a ?b WHERE { GRAPH ex:g1 { ?s ex:colour ?a } GRAPH ex:g2 { ?s ex:colour ?b } }",
        "SELECT ?s ?k ?c WHERE { ?s ex:kind ?k . OPTIONAL { GRAPH ex:g2 { ?s ex:colour ?c } } }",
    ] {
        let query = spargebra::SparqlParser::new()
            .parse_query(&format!("{PREFIX}{sparql}"))
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&query).is_some(),
            "the fragment should take this, or the agreement tests prove nothing: {sparql}"
        );
    }

    // A nested `GRAPH` re-scopes its block, and the inner-overrides-outer rule is not
    // implemented. Refused rather than answered against whichever graph won by accident.
    let nested =
        format!("{PREFIX}SELECT ?s WHERE {{ GRAPH ?g {{ GRAPH ex:g1 {{ ?s ex:name ?n }} }} }}");
    let query = spargebra::SparqlParser::new()
        .parse_query(&nested)
        .expect("parse");
    assert!(
        holos_engine::bindjoin::plan(&query).is_none(),
        "a nested GRAPH must be refused while re-scoping is unimplemented"
    );
}
