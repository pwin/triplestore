//! Arithmetic that HOLOS 0.26.1 answered wrongly, and that `spargebra` 0.4.8 and
//! `oxsdatatypes` 0.2.4 answer correctly.
//!
//! `spargebra` 0.4.6 grouped a chain of `-` or `/` from the right, so `10 - 2 - 3` was
//! `10 - (2 - 3)`, 11, and `24 / 4 / 2` was 12; and a signed literal after an operand could
//! take the operator before it, so `1 -2*3 -4` was -1 (oxigraph/oxigraph#1970, #1974).
//! `oxsdatatypes` 0.2.2 cut a decimal to one fractional digit before rounding it, so
//! `ROUND(-2.5001)` was -2, and rounded a double half away from zero, so `ROUND(-2.5e0)` was
//! -3. All of these are wrong numbers, not errors. Every value quoted here is what 0.26.1
//! returned; these tests pin the answers through the evaluator HOLOS runs.

use holos_engine::Engine;
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

fn value(expression: &str) -> String {
    let mut e = Engine::new();
    e.bulk_load(&b""[..], RdfFormat::Turtle, None)
        .expect("load");
    let session = Session::unrestricted(e.store()).expect("session");
    let view = e.view(&session);
    let query = format!("PREFIX xsd: <{XSD}> SELECT ({expression} AS ?x) WHERE {{}}");
    let results = Engine::query(&view, &query, None).expect("query");
    let QueryResults::Solutions(mut iter) = results else {
        panic!("expected a solutions result");
    };
    let solution = iter.next().expect("one row").expect("solution");
    solution
        .get("x")
        .map_or_else(String::new, ToString::to_string)
}

fn typed(lexical: &str, datatype: &str) -> String {
    format!("\"{lexical}\"^^<{XSD}{datatype}>")
}

#[test]
fn a_chain_of_minus_or_divide_groups_from_the_left() {
    assert_eq!(value("10 - 2 - 3"), typed("5", "integer"));
    assert_eq!(value("10 - 2 + 3"), typed("11", "integer"));
    assert_eq!(value("24 / 4 / 2"), typed("3", "decimal"));
}

#[test]
fn a_signed_literal_after_an_operand_does_not_take_the_operator_before_it() {
    // `10 - 2 -3` has no space between `-` and `3`, so the lexer reads `-3` as a literal.
    assert_eq!(value("10 - 2 -3"), typed("5", "integer"));
    assert_eq!(value("1 -2*3 -4"), typed("-9", "integer"));
    assert_eq!(value("10 - 2 -3*2"), typed("2", "integer"));
}

#[test]
fn round_takes_a_half_towards_positive_infinity() {
    // fn:round: "the one that is closest to positive infinity" when two are equally near.
    assert_eq!(value("ROUND(-2.5)"), typed("-2", "decimal"));
    assert_eq!(value("ROUND(-2.5001)"), typed("-3", "decimal"));
    assert_eq!(value("ROUND(-2.5e0)"), typed("-2", "double"));
    assert_eq!(value("ROUND(-1.5e0)"), typed("-1", "double"));
}
