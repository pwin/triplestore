//! `GROUP BY` over the bind join, checked against an evaluation that does not use it.
//!
//! `holos_engine::group` moves where a group's input comes from and nothing else, so the
//! thing to check is that the aggregates cannot tell: every query here is answered through it
//! with and without statistics, through the evaluator alone, and by `spareval` over the parsed
//! data, and all four must agree. The data carries what aggregates are particular about —
//! integers, decimals and doubles in one group, a string where a number is summed, a value left
//! unbound by `OPTIONAL`, duplicate rows, and blank nodes, which a `VALUES` table cannot hold.
//!
//! Counts differ from group to group, so a `LIMIT` over an ordering has no ties to break
//! differently, and `GROUP_CONCAT` and `SAMPLE` are only asked of one-member groups — both are
//! free to pick their order, and a test of them on larger groups would be testing that freedom.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::GraphFilter;
use oxrdf::Dataset;
use oxrdfio::{RdfFormat, RdfParser};
use spareval::QueryResults;
use spargebra::SparqlParser;
use std::sync::Arc;

const P: &str = "PREFIX ex: <http://example.com/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";

/// Departments of one, two, three and four people, each with a salary of a different numeric
/// type; a bonus for some; and one department whose people are blank nodes.
fn data() -> String {
    let mut turtle = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    let mut person = 0;
    for (dept, size) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
        for i in 0..size {
            let salary = match i % 3 {
                0 => format!("{}", 100 + person),
                1 => format!("\"{}.5\"^^xsd:decimal", 100 + person),
                _ => format!("\"{}e0\"^^xsd:double", 100 + person),
            };
            turtle.push_str(&format!(
                "ex:p{person} ex:dept ex:{dept} ; ex:salary {salary} ; ex:name \"person {person}\" .\n"
            ));
            if person % 2 == 0 {
                turtle.push_str(&format!("ex:p{person} ex:bonus 10 .\n"));
            }
            person += 1;
        }
    }
    // A second salary for p9, d's fourth member, that is not a number: SUM over department d
    // has an error to propagate.
    turtle.push_str("ex:p9 ex:dept ex:d ; ex:salary \"unknown\" ; ex:name \"person 9\" .\n");
    // A department of blank nodes.
    turtle.push_str("_:x ex:dept ex:e ; ex:salary 1 ; ex:name \"x\" .\n");
    turtle.push_str("_:y ex:dept ex:e ; ex:salary 2 ; ex:name \"y\" .\n");
    turtle
}

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(data().as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

fn render(results: QueryResults<'_>) -> Vec<String> {
    let QueryResults::Solutions(iter) = results else {
        panic!("expected solutions")
    };
    let mut out: Vec<String> = iter
        .map(|s| {
            let s = s.expect("solution");
            // A blank node's label is the store's to choose, and differs from the parsed
            // data's; which row it is in is what is compared.
            let mut cells: Vec<String> = s
                .iter()
                .map(|(v, t)| match t {
                    oxrdf::Term::BlankNode(_) => format!("{}=_:b", v.as_str()),
                    t => format!("{}={t}", v.as_str()),
                })
                .collect();
            cells.sort();
            cells.join(" ")
        })
        .collect();
    out.sort();
    out
}

fn through(engine: &Engine, query: &str, options: &QueryOptions) -> Vec<String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let (results, _) = Engine::query_with(&view, query, options).expect("query");
    render(results)
}

fn reference(query: &str) -> Vec<String> {
    let mut dataset = Dataset::new();
    for quad in RdfParser::from_format(RdfFormat::Turtle).for_slice(data().as_bytes()) {
        dataset.insert(&quad.expect("quad"));
    }
    let query = SparqlParser::new().parse_query(query).expect("parse");
    render(
        Engine::evaluator()
            .prepare(&query)
            .execute(&dataset)
            .expect("evaluate"),
    )
}

fn agreed(body: &str) -> Vec<String> {
    let query = format!("{P} {body}");
    let engine = engine();
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let fast = through(&engine, &query, &QueryOptions::new().reordering(stats));
    let plain = through(&engine, &query, &QueryOptions::new());
    let slow = through(&engine, &query, &QueryOptions::new().explaining());
    let expected = reference(&query);
    assert_eq!(fast, expected, "with statistics: {body}");
    assert_eq!(plain, expected, "without statistics: {body}");
    assert_eq!(slow, expected, "evaluator: {body}");
    expected
}

/// Whether the group's input was taken from the bind join.
fn materialised(body: &str) -> bool {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = SparqlParser::new()
        .parse_query(&format!("{P} {body}"))
        .expect("parse");
    holos_engine::group::materialised(&view, &query, None, None)
        .expect("store")
        .is_some()
}

#[test]
fn count_per_group() {
    let rows = agreed("SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d } GROUP BY ?d");
    assert_eq!(rows.len(), 5, "{rows:?}");
}

#[test]
fn numeric_aggregates_promote_across_types() {
    agreed(
        "SELECT ?d (SUM(?s) AS ?sum) (AVG(?s) AS ?avg) (MIN(?s) AS ?min) (MAX(?s) AS ?max)
         WHERE { ?p ex:dept ?d ; ex:salary ?s } GROUP BY ?d",
    );
}

#[test]
fn a_sum_over_a_string_is_an_error_not_a_number() {
    let rows = agreed(
        "SELECT ?d (SUM(?s) AS ?sum) WHERE { ?p ex:dept ?d ; ex:salary ?s FILTER(?d = ex:d) } GROUP BY ?d",
    );
    assert_eq!(rows, vec!["d=<http://example.com/d>"]);
}

#[test]
fn count_distinct_and_count_of_an_optional() {
    agreed(
        "SELECT ?d (COUNT(DISTINCT ?b) AS ?kinds) (COUNT(?b) AS ?with) (COUNT(*) AS ?all)
         WHERE { ?p ex:dept ?d OPTIONAL { ?p ex:bonus ?b } } GROUP BY ?d",
    );
}

#[test]
fn having_filters_groups() {
    let rows = agreed(
        "SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d } GROUP BY ?d HAVING (COUNT(*) > 2)",
    );
    // c has three and d four; p9 is d's fourth, and its second salary adds no member.
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn ordered_and_limited_on_the_aggregate() {
    let rows = agreed(
        "SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d } GROUP BY ?d ORDER BY DESC(?n) LIMIT 2",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn one_group_without_group_by() {
    agreed("SELECT (COUNT(*) AS ?n) (SUM(?s) AS ?total) WHERE { ?p ex:dept ex:c ; ex:salary ?s }");
}

#[test]
fn an_empty_input_still_counts_zero() {
    let body = "SELECT (COUNT(*) AS ?n) WHERE { ?p ex:dept ex:nowhere }";
    assert!(materialised(body), "answered without the evaluator's scan");
    let rows = agreed(body);
    assert_eq!(rows.len(), 1, "{rows:?}");
}

#[test]
fn an_empty_input_with_a_key_is_no_groups() {
    let body =
        "SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d FILTER(?d = ex:nowhere) } GROUP BY ?d";
    assert_eq!(agreed(body), Vec::<String>::new());
}

#[test]
fn concat_and_sample_of_one_member_groups() {
    agreed(
        "SELECT ?p (GROUP_CONCAT(?name) AS ?names) (SAMPLE(?d) AS ?dept)
         WHERE { ?p ex:dept ?d ; ex:name ?name FILTER(?d != ex:e) } GROUP BY ?p",
    );
}

#[test]
fn a_filter_pinned_group_like_the_slow_one() {
    let rows = agreed(
        r#"SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d ; ex:name ?name FILTER(?d = ex:c) FILTER(str(?name) != "") } GROUP BY ?d"#,
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
}

#[test]
fn blank_nodes_go_to_the_evaluator_and_still_agree() {
    let body = "SELECT ?p (COUNT(*) AS ?n) WHERE { ?p ex:dept ex:e } GROUP BY ?p";
    assert!(
        !materialised(body),
        "a VALUES table cannot carry a blank node"
    );
    let rows = agreed(body);
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn grouping_on_an_expression_goes_to_the_evaluator_and_still_agrees() {
    let body = "SELECT ?big (COUNT(*) AS ?n) WHERE { ?p ex:salary ?s } GROUP BY (?s > 104 AS ?big)";
    assert!(
        !materialised(body),
        "BIND is outside the bind join's fragment"
    );
    agreed(body);
}

#[test]
fn the_input_is_taken_from_the_bind_join_where_it_can_be() {
    assert!(materialised(
        "SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d FILTER(?d != ex:e) } GROUP BY ?d"
    ));
    assert!(materialised(
        "SELECT ?d (COUNT(*) AS ?n) WHERE { ?p ex:dept ?d FILTER(?d != ex:e) } GROUP BY ?d HAVING (COUNT(*) > 1) ORDER BY ?n LIMIT 3"
    ));
    // Not a group at all.
    assert!(!materialised("SELECT ?d WHERE { ?p ex:dept ?d }"));
}

/// A `UNION` of a pattern with itself is every row twice, and `COUNT(*)` must see both: the
/// table has to carry multiplicity, not just the distinct rows.
#[test]
fn duplicate_input_rows_are_counted_twice() {
    let body = "SELECT ?d (COUNT(*) AS ?n) WHERE { { ?p ex:dept ?d } UNION { ?p ex:dept ?d } FILTER(?d != ex:e) } GROUP BY ?d";
    assert!(materialised(body));
    let rows = agreed(body);
    assert!(
        rows.iter()
            .any(|r| r.starts_with("d=<http://example.com/d>") && r.contains("\"8\"")),
        "department d's four, twice: {rows:?}"
    );
}
