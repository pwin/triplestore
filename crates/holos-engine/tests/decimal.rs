//! `xsd:decimal` arithmetic rounds to 18 fractional digits instead of failing.
//!
//! A decimal is held to 18 fractional digits, so a product of two of them can need 36. The
//! registry `oxsdatatypes` returned an error for any product it could not hold exactly, and
//! an error in `BIND` or a projection leaves the variable unbound: `1.3 * 31.583333333333333333`
//! had no value while `2 * 31.583333333333333333` did. Division failed the same way once the
//! dividend was large. XPath F&O §4.2 has an over-long decimal result "truncated or rounded in
//! an implementation-defined manner", and keeps FOAR0002 for overflow; the patched copy in
//! `vendor/oxsdatatypes` rounds half to even. These tests are the end-to-end half of that:
//! the evaluator HOLOS actually runs, reading its values from a query and from the store.

use holos_engine::Engine;
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const DATA: &str = r"
@prefix ex: <http://example.com/> .
ex:order ex:quantity 1.3 ; ex:rate 31.583333333333333333 .
ex:a ex:n 1 . ex:b ex:n 2 . ex:c ex:n 2 .
";

const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";

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

fn decimal(name: &str, lexical: &str) -> String {
    format!("{name}=\"{lexical}\"^^<{XSD_DECIMAL}>")
}

#[test]
fn a_product_with_more_than_18_fractional_digits_is_rounded() {
    // The reported case: the exact product is 41.0583333333333333329.
    assert_eq!(
        rows("SELECT ?x WHERE { BIND(1.3 * 31.583333333333333333 AS ?x) }"),
        [decimal("x", "41.058333333333333333")]
    );
    // And the one that already worked is unchanged.
    assert_eq!(
        rows("SELECT ?x WHERE { BIND(2 * 31.583333333333333333 AS ?x) }"),
        [decimal("x", "63.166666666666666666")]
    );
}

#[test]
fn a_product_of_stored_values_is_rounded() {
    assert_eq!(
        rows(
            "PREFIX ex: <http://example.com/>
             SELECT ?x WHERE { ex:order ex:quantity ?q ; ex:rate ?r BIND(?q * ?r AS ?x) }"
        ),
        [decimal("x", "41.058333333333333333")]
    );
}

#[test]
fn a_quotient_with_a_large_dividend_is_rounded() {
    assert_eq!(
        rows("SELECT ?x WHERE { BIND(1000 / 31.583333333333333333 AS ?x) }"),
        [decimal("x", "31.662269129287598945")]
    );
}

#[test]
fn an_inexact_quotient_rounds_half_to_even() {
    // Truncated before: 0.666666666666666666.
    assert_eq!(
        rows("SELECT ?x WHERE { BIND(2 / 3 AS ?x) }"),
        [decimal("x", "0.666666666666666667")]
    );
    // AVG is a sum divided by a count, so it follows.
    assert_eq!(
        rows("PREFIX ex: <http://example.com/> SELECT (AVG(?n) AS ?x) WHERE { ?s ex:n ?n }"),
        [decimal("x", "1.666666666666666667")]
    );
}

#[test]
fn overflow_is_still_an_error() {
    // 10^21 is past the largest decimal, about 1.7 · 10^20.
    assert_eq!(
        rows("SELECT ?x WHERE { BIND(100000000000.0 * 10000000000.0 AS ?x) }"),
        [""]
    );
    assert_eq!(rows("SELECT ?x WHERE { BIND(1.0 / 0.0 AS ?x) }"), [""]);
}
