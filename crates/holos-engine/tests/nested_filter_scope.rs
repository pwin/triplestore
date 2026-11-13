//! A `FILTER` in a group nested inside `OPTIONAL` stays in that group.
//!
//! `spargebra` 0.4.6 parsed `OPTIONAL { { P FILTER(f) } }` as if the braces were one pair, so
//! `f` became the `OPTIONAL`'s own condition and could see the variables bound outside it.
//! Inside the nested group those variables are not in scope: `f` is an error there, the group
//! has no rows, and the `OPTIONAL` adds nothing. `spargebra` 0.4.8 keeps the filter where it
//! was written. This is W3C `dawg-optional-filter-005-not-simplified`, but the conformance
//! harness files a test whose answer matches the reference evaluator's as an upstream skip
//! rather than a failure, so a regression there would not fail the ratchet. This test would.

use holos_engine::Engine;
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:book1 ex:title "TITLE 1" ; ex:price 10 .
ex:book2 ex:title "TITLE 2" ; ex:price 20 .
ex:book3 ex:title "TITLE 3" .
"#;

fn rows(query: &str) -> Vec<String> {
    let mut e = Engine::new();
    e.bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    let session = Session::unrestricted(e.store()).expect("session");
    let view = e.view(&session);
    let results = Engine::query(&view, query, None).expect("query");
    let QueryResults::Solutions(iter) = results else {
        panic!("expected a solutions result");
    };
    iter.map(|s| {
        s.expect("solution")
            .iter()
            .map(|(v, t)| format!("{}={t}", v.as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    })
    .collect()
}

const PRICE_20: &str = "price=\"20\"^^<http://www.w3.org/2001/XMLSchema#integer>";

#[test]
fn a_filter_in_a_nested_group_does_not_see_the_outer_variables() {
    // 0.26.1 gave TITLE 2 its price here.
    assert_eq!(
        rows(
            r#"PREFIX ex: <http://example.com/>
               SELECT ?title ?price WHERE {
                 ?book ex:title ?title .
                 OPTIONAL { { ?book ex:price ?price FILTER(?title = "TITLE 2") } }
               } ORDER BY ?title"#
        ),
        [
            r#"title="TITLE 1""#,
            r#"title="TITLE 2""#,
            r#"title="TITLE 3""#
        ]
    );
}

#[test]
fn a_filter_directly_in_optional_is_its_condition() {
    // With one pair of braces the filter is the left join's condition, which sees ?title.
    assert_eq!(
        rows(
            r#"PREFIX ex: <http://example.com/>
               SELECT ?title ?price WHERE {
                 ?book ex:title ?title .
                 OPTIONAL { ?book ex:price ?price FILTER(?title = "TITLE 2") }
               } ORDER BY ?title"#
        ),
        [
            r#"title="TITLE 1""#.to_owned(),
            format!(r#"title="TITLE 2" {PRICE_20}"#),
            r#"title="TITLE 3""#.to_owned(),
        ]
    );
}
