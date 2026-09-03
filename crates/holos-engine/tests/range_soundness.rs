//! A span must never exclude a row the filter would have kept.
//!
//! `holos_engine::range` turns `FILTER(?o > k)` into spans of term ids so a scan can be
//! bounded instead of read whole. The span decides what is *read*; the filter still decides
//! what matches. So the span may admit too much — that costs time — but if it admits too
//! little the row is never seen, and a row lost that way does not look like a bug. It looks
//! like data that was never in the store.
//!
//! Testing the spans against each other cannot find that. This tests them against the
//! evaluator: run the real query, take every row it returns, and require the span to admit
//! it. The oracle is SPARQL's own comparison semantics, which is the thing the span has to
//! agree with and the thing a hand-written check would get wrong in the same direction.
//!
//! The data is chosen to be awkward in the ways the encoding is:
//!
//! - `xsd:integer` and `xsd:float` are inline and ordered; `xsd:decimal` and `xsd:double`
//!   are **not**, and are numbers all the same. A span over the integer region alone would
//!   drop every one of them.
//! - An integer too large for 60 bits falls out of the inline range and into the dictionary,
//!   still comparing as a number.
//! - `xsd:dateTime` compares only against dateTimes, and a non-canonical spelling of one is
//!   a dictionary literal.

use holos_engine::range::{comparison, spans};
use holos_engine::Engine;
use holos_security::Session;
use holos_store::Store;
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use spareval::QueryResults;
use spargebra::algebra::{Expression, GraphPattern};
use spargebra::SparqlParser;

const EX: &str = "http://example.com/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn typed(value: &str, datatype: oxrdf::NamedNodeRef<'_>) -> Term {
    Literal::new_typed_literal(value, datatype).into()
}

/// Values that a numeric comparison has to reach, spread across every encoding they can land
/// in.
fn values() -> Vec<Term> {
    let mut out = Vec::new();
    for i in -20..40 {
        out.push(typed(&i.to_string(), xsd::INTEGER));
    }
    for v in ["0.5", "2.5", "29.5", "30.5", "-3.5", "1000.25"] {
        out.push(typed(v, xsd::DECIMAL));
        out.push(typed(v, xsd::DOUBLE));
        out.push(typed(v, xsd::FLOAT));
    }
    // Beyond 60 bits: a number the inline codec cannot take.
    out.push(typed("576460752303423489", xsd::INTEGER));
    out.push(typed("-576460752303423489", xsd::INTEGER));
    // Not numbers at all. The filter excludes them; the span may or may not admit them, and
    // either is correct.
    out.push(Literal::new_simple_literal("thirty").into());
    out.push(ex("thirty").into());
    out.push(typed("2020-06-01T12:00:00Z", xsd::DATE_TIME));
    out
}

fn engine() -> Engine {
    let mut store = Store::new();
    for (i, value) in values().into_iter().enumerate() {
        store
            .insert(
                Quad {
                    subject: ex(&format!("s{i}")).into(),
                    predicate: ex("v"),
                    object: value,
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    Engine::with_store(store)
}

/// The `FILTER` expression of a query, as the algebra holds it.
fn filter_of(query: &str) -> Expression {
    let parsed = SparqlParser::new().parse_query(query).expect("parse");
    let spargebra::Query::Select { pattern, .. } = parsed else {
        panic!("expected a SELECT")
    };
    fn find(p: &GraphPattern) -> Option<Expression> {
        match p {
            GraphPattern::Filter { expr, .. } => Some(expr.clone()),
            GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Slice { inner, .. } => find(inner),
            GraphPattern::Join { left, right } => find(left).or_else(|| find(right)),
            _ => None,
        }
    }
    find(&pattern).expect("a filter")
}

/// Every object the query actually returns — the evaluator's own answer.
fn matching_objects(engine: &Engine, query: &str) -> Vec<Term> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let QueryResults::Solutions(rows) = Engine::query(&view, query, None).expect("query") else {
        panic!("expected solutions")
    };
    rows.map(|row| row.expect("row").get("o").expect("?o is projected").clone())
        .collect()
}

/// The property, for one filter.
fn span_admits_everything_that_matches(filter: &str) {
    let engine = engine();
    let query = format!("SELECT ?o WHERE {{ ?s <{EX}v> ?o FILTER({filter}) }}");

    let Some(c) = comparison(&filter_of(&query)) else {
        panic!("`{filter}` should be readable as a comparison");
    };
    let Some(spans) = spans(engine.store(), &c) else {
        // Declining to bound is always sound: the caller scans as it would have.
        return;
    };

    let matched = matching_objects(&engine, &query);
    assert!(
        !matched.is_empty(),
        "`{filter}` matched nothing, so it proves nothing"
    );

    for object in matched {
        let id = engine
            .store()
            .lookup_term(object.as_ref())
            .expect("lookup")
            .expect("a term the store returned is in the store");
        assert!(
            spans.iter().any(|span| span.contains(id)),
            "`{filter}` matches {object} but no span admits it — a bounded scan would \
             never have read it.\nspans: {spans:?}"
        );
    }
}

#[test]
fn integer_bounds_admit_every_row_they_match() {
    for filter in [
        "?o > 30", "?o >= 30", "?o < 30", "?o <= 30", "?o > 0", "?o < 0", "?o > -10", "?o <= -3",
        "30 < ?o", "30 >= ?o",
    ] {
        span_admits_everything_that_matches(filter);
    }
}

#[test]
fn float_bounds_admit_every_row_they_match() {
    for filter in [
        "?o > \"2.5\"^^<http://www.w3.org/2001/XMLSchema#float>",
        "?o < \"2.5\"^^<http://www.w3.org/2001/XMLSchema#float>",
        "?o >= \"-3.5\"^^<http://www.w3.org/2001/XMLSchema#float>",
    ] {
        span_admits_everything_that_matches(filter);
    }
}

#[test]
fn a_datetime_bound_admits_every_row_it_matches() {
    for filter in [
        "?o > \"2020-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>",
        "?o < \"2021-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>",
    ] {
        span_admits_everything_that_matches(filter);
    }
}

/// The case the module's doc comment is about: a decimal is a number, it is not inline, and
/// a span built only from the integer region would lose it.
#[test]
fn a_decimal_is_admitted_by_an_integer_bound() {
    let engine = engine();
    let query = format!("SELECT ?o WHERE {{ ?s <{EX}v> ?o FILTER(?o > 30) }}");
    let matched = matching_objects(&engine, &query);

    // The evaluator does compare a decimal against an integer, so the fixture is doing its
    // job and the assertion below is about something real.
    assert!(
        matched
            .iter()
            .any(|t| t.to_string().contains("30.5") || t.to_string().contains("1000.25")),
        "expected the decimals above 30 to match: {matched:?}"
    );
    span_admits_everything_that_matches("?o > 30");
}

/// A bounded scan, run for real, must return what an unbounded one filtered would.
#[test]
fn a_bounded_scan_finds_the_same_rows() {
    use holos_store::GraphFilter;

    let engine = engine();
    let store = engine.store();
    let query = format!("SELECT ?o WHERE {{ ?s <{EX}v> ?o FILTER(?o > 30) }}");
    let c = comparison(&filter_of(&query)).expect("a comparison");
    let spans = spans(store, &c).expect("bounded");
    let predicate = store.lookup_term(ex("v").as_ref().into()).expect("lookup");

    // Everything the spans would have the scan read.
    let mut read = std::collections::HashSet::new();
    for span in &spans {
        for quad in store.quads_with_object_in(None, predicate, *span, GraphFilter::Default) {
            read.insert(quad.expect("scan").object);
        }
    }

    for object in matching_objects(&engine, &query) {
        let id = store
            .lookup_term(object.as_ref())
            .expect("lookup")
            .expect("present");
        assert!(
            read.contains(&id),
            "a bounded scan never read {object}, which the filter matches"
        );
    }
}
