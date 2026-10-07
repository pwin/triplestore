//! A filter's constant used as a binding, checked against an evaluation that does not use it.
//!
//! The rewrite in `holos_engine::equality` is only an optimisation if it cannot change an
//! answer, and the ways it could are specific: a candidate set missing a term the filter would
//! have accepted drops rows, and a join that keeps an unbound variable adds them. So the data
//! here is built from the near misses — the same text as a different kind of term, the same
//! number written differently, the same label in a neighbouring language — and every query is
//! answered three ways: through the bind join with statistics, through the evaluator, and by
//! `spareval` directly over the parsed data, with no rewrite anywhere. All three must agree,
//! row for row and in multiplicity.

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

fn data() -> String {
    let mut turtle = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    // Codes: one plain "GB" twice over, so multiplicity shows, and every near miss.
    for (subject, code) in [
        ("a", r#""GB""#),
        ("b", r#""GB""#),
        ("c", r#""GB"@en"#),
        ("d", r#""GB"^^ex:dt"#),
        ("e", "ex:GB"),
        ("f", r#""gb""#),
        ("g", "1"),
        ("h", r#""01"^^xsd:integer"#),
        ("i", "1.0"),
    ] {
        turtle.push_str(&format!("ex:{subject} ex:code {code} ; ex:kind ex:P .\n"));
    }
    // Names: the label, and the labels that are nearly it.
    for (subject, name) in [
        ("a", r#""Glasgow"@en"#),
        ("b", r#""Glasgow"@en"#),
        ("c", r#""Glasgow"@en-gb"#),
        ("d", r#""Glasgow""#),
        ("e", r#""Glasgow"@fr"#),
        ("f", r#""Glasgow"@en--ltr"#),
        ("g", r#""glasgow"@en"#),
        ("h", r#""Glasgow"^^ex:dt"#),
    ] {
        turtle.push_str(&format!("ex:{subject} ex:name {name} .\n"));
    }
    turtle.push_str("ex:a ex:extra \"x\" .\n");
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
            let mut cells: Vec<String> = s
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
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

/// `spareval` over the parsed data: no store, no rewrite, no fast path.
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

/// The answer, after checking that all three ways of getting it agree.
fn agreed(body: &str) -> Vec<String> {
    let query = format!("{P} {body}");
    let engine = engine();
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let fast = through(&engine, &query, &QueryOptions::new().reordering(stats));
    let plain = through(&engine, &query, &QueryOptions::new());
    let slow = through(&engine, &query, &QueryOptions::new().explaining());
    let expected = reference(&query);
    assert_eq!(fast, expected, "bind join with statistics: {body}");
    assert_eq!(plain, expected, "bind join without statistics: {body}");
    assert_eq!(slow, expected, "evaluator: {body}");
    expected
}

#[test]
fn a_plain_string_matches_itself_only_and_keeps_its_duplicates() {
    let rows = agreed(r#"SELECT ?s WHERE { ?s ex:code ?c FILTER(?c = "GB") }"#);
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn an_iri_matches_itself_only() {
    let rows = agreed("SELECT ?s WHERE { ?s ex:code ?c FILTER(?c = ex:GB) }");
    assert_eq!(rows, vec!["s=<http://example.com/e>"]);
}

#[test]
fn a_number_still_matches_every_spelling_of_it() {
    // Not pinned: 1, "01"^^xsd:integer and 1.0 are all `=` to 1.
    let rows = agreed("SELECT ?s WHERE { ?s ex:code ?c FILTER(?c = 1) }");
    assert_eq!(rows.len(), 3, "{rows:?}");
}

#[test]
fn same_term_matches_one_spelling() {
    let rows = agreed("SELECT ?s WHERE { ?s ex:code ?c FILTER(sameTerm(?c, 1)) }");
    assert_eq!(rows, vec!["s=<http://example.com/g>"]);
}

#[test]
fn the_label_idiom_finds_the_label_and_its_directional_form() {
    let rows = agreed(
        r#"SELECT ?s ?n WHERE { ?s ex:name ?n FILTER(lang(?n) = "en") FILTER(str(?n) = "Glasgow") }"#,
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
}

#[test]
fn the_shape_that_was_slow() {
    let rows = agreed(
        r#"SELECT * WHERE {
             ?s ex:kind ex:P ; ex:code ?c ; ex:name ?n .
             FILTER(?c = "GB")
             FILTER(lang(?n) = "en")
             FILTER(str(?n) = "Glasgow")
           } LIMIT 5"#,
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn a_constant_the_store_never_saw() {
    assert_eq!(
        agreed(r#"SELECT ?s WHERE { ?s ex:code ?c FILTER(?c = "absent") }"#),
        Vec::<String>::new()
    );
    assert_eq!(
        agreed(
            r#"SELECT ?s WHERE { ?s ex:name ?n FILTER(lang(?n) = "de" && str(?n) = "Glasgow") }"#
        ),
        Vec::<String>::new()
    );
}

/// `COUNT(*)` over nothing is one row saying 0. An empty `VALUES` from a pin the store cannot
/// satisfy made it no rows, because `sparopt` folds a statically empty input up through a
/// keyless `Group`. A short string was spared only because it is inlined into an id, and so
/// looks present; an IRI or a long string is not.
#[test]
fn counting_what_the_store_lacks_is_zero_not_nothing() {
    for pin in [
        "?c = ex:absent",
        r#"?c = "a string long enough that it cannot be inlined into an id""#,
        "?c IN ()",
        "?c IN (ex:absent, ex:alsoAbsent)",
        r#"lang(?c) = "cy" && str(?c) = "Glasgow""#,
    ] {
        let rows = agreed(&format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ ?s ex:code ?c FILTER({pin}) }}"
        ));
        assert_eq!(rows.len(), 1, "{pin}: {rows:?}");
        // And inside a subquery under a keyless count, where the group is further away.
        let rows = agreed(&format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ {{ SELECT ?s WHERE {{ ?s ex:code ?c FILTER({pin}) }} }} }}"
        ));
        assert_eq!(rows.len(), 1, "{pin}, in a subquery: {rows:?}");
    }
    // With a key, no input is no groups, and that is what an empty VALUES gives.
    let rows = agreed(
        "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s ex:code ?c FILTER(?c = ex:absent) } GROUP BY ?c",
    );
    assert_eq!(rows, Vec::<String>::new());
}

#[test]
fn an_optional_variable_is_left_to_the_filter() {
    let rows = agreed(
        r#"SELECT ?s WHERE { ?s ex:kind ex:P OPTIONAL { ?s ex:extra ?x } FILTER(?x = "x") }"#,
    );
    assert_eq!(rows, vec!["s=<http://example.com/a>"]);
}

#[test]
fn in_matches_each_listed_term() {
    let rows = agreed(r#"SELECT ?s WHERE { ?s ex:code ?c FILTER(?c IN ("GB", "gb", ex:GB)) }"#);
    assert_eq!(rows.len(), 4, "{rows:?}");
}

#[test]
fn a_term_listed_twice_still_matches_once() {
    let rows = agreed(r#"SELECT ?s WHERE { ?s ex:code ?c FILTER(?c IN ("GB", "GB")) }"#);
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn in_with_a_number_still_matches_every_spelling() {
    let rows = agreed(r#"SELECT ?s WHERE { ?s ex:code ?c FILTER(?c IN ("GB", 1)) }"#);
    assert_eq!(rows.len(), 5, "{rows:?}");
}

#[test]
fn an_empty_in_matches_nothing() {
    assert_eq!(
        agreed("SELECT ?s WHERE { ?s ex:code ?c FILTER(?c IN ()) }"),
        Vec::<String>::new()
    );
}

#[test]
fn a_disjunction_is_left_to_the_filter() {
    let rows = agreed(r#"SELECT ?s WHERE { ?s ex:code ?c FILTER(?c = "GB" || ?c = "gb") }"#);
    assert_eq!(rows.len(), 3, "{rows:?}");
}

#[test]
fn a_union_bound_on_both_sides() {
    let rows = agreed(
        r#"SELECT ?s WHERE { { ?s ex:code ?v } UNION { ?s ex:name ?v } FILTER(?v = "Glasgow") }"#,
    );
    assert_eq!(rows, vec!["s=<http://example.com/d>"]);
}
