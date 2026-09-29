//! A closure property path walked by the join answers what the evaluator answers.
//!
//! Property-path semantics are the kind of thing that looks obvious and is not. `:p+` is not
//! "`:p*` minus the start": over a cycle the start *is* reachable from itself, so it is an
//! answer. An arbitrary-length path yields distinct pairs, so a node reached two ways is one
//! row and not two. A zero-length path connects a term to itself whether or not it has any
//! edges — including a term that appears only as an object, and including one under a
//! predicate the store has never seen.
//!
//! None of that is verifiable by inspection, and all of it is verifiable differentially: the
//! same query answered by the join and by `spareval`, over graphs built to contain each trap.
//! `holos_engine::bindjoin` is skipped with `without_bind_join`, which is what makes the two
//! arms two implementations rather than one.

use holos_engine::{DatasetView, Engine, QueryOptions};
use holos_security::Session;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

/// A chain, a branch, a cycle, a diamond that reaches one node two ways, a node reachable
/// only as an object, and a second predicate so a walk cannot ignore which edges it crosses.
const DATA: &str = r#"
    @prefix ex: <http://example.com/> .
    ex:a ex:p ex:b .
    ex:b ex:p ex:c .
    ex:c ex:p ex:d .
    ex:b ex:p ex:e .
    ex:f ex:p ex:g .
    ex:g ex:p ex:f .
    ex:h ex:p ex:i .
    ex:h ex:p ex:j .
    ex:i ex:p ex:k .
    ex:j ex:p ex:k .
    ex:a ex:q ex:zz .
    ex:leaf ex:memberOf ex:b .
    ex:sink ex:other ex:a .
"#;

fn engine() -> Engine {
    let mut e = Engine::new();
    e.bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    e
}

/// The rows a query returns, sorted, so only the multiset is compared — an arbitrary-length
/// path fixes which pairs are answers and not the order they arrive in.
fn rows(view: &DatasetView<'_>, query: &str, options: &QueryOptions) -> Vec<String> {
    let (results, _) = Engine::query_with(view, query, options).expect("answered");
    let QueryResults::Solutions(iter) = results else {
        panic!("expected solutions");
    };
    let mut out: Vec<String> = iter
        .map(|s| {
            s.expect("row")
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

/// Every closure shape, each answered both ways.
#[test]
fn the_join_and_the_evaluator_agree() {
    let e = engine();
    let session = Session::unrestricted(e.store()).expect("session");
    let view = e.view(&session);
    let p = "PREFIX ex: <http://example.com/> ";

    for query in [
        // Anchored subject, the plain cases.
        format!("{p}SELECT ?o WHERE {{ ex:a ex:p* ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:a ex:p+ ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:a ex:p? ?o }}"),
        // A cycle: `+` reaches its own start.
        format!("{p}SELECT ?o WHERE {{ ex:f ex:p* ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:f ex:p+ ?o }}"),
        // A diamond: `k` is reachable two ways and is one row.
        format!("{p}SELECT ?o WHERE {{ ex:h ex:p+ ?o }}"),
        // Anchored object, so the walk runs backwards.
        format!("{p}SELECT ?s WHERE {{ ?s ex:p* ex:d }}"),
        format!("{p}SELECT ?s WHERE {{ ?s ex:p+ ex:d }}"),
        format!("{p}SELECT ?s WHERE {{ ?s ex:p? ex:d }}"),
        // Both ends bound: a reachability test, true and false.
        format!("{p}SELECT ?x WHERE {{ ex:a ex:p+ ex:d BIND(1 AS ?x) }}"),
        format!("{p}SELECT ?x WHERE {{ ex:d ex:p+ ex:a BIND(1 AS ?x) }}"),
        format!("{p}SELECT ?x WHERE {{ ex:a ex:p* ex:a BIND(1 AS ?x) }}"),
        format!("{p}SELECT ?x WHERE {{ ex:a ex:p+ ex:a BIND(1 AS ?x) }}"),
        // An inverse closure.
        format!("{p}SELECT ?s WHERE {{ ex:d ^ex:p+ ?s }}"),
        format!("{p}SELECT ?s WHERE {{ ex:d ^ex:p* ?s }}"),
        // The shape this work exists for: a sequence whose closure's left side is a variable
        // that an earlier pattern binds.
        format!("{p}SELECT ?u WHERE {{ ex:leaf ex:memberOf/ex:p* ?u }}"),
        format!("{p}SELECT ?u WHERE {{ ex:leaf ex:memberOf/ex:p+ ?u }}"),
        format!("{p}SELECT ?u WHERE {{ ex:leaf ex:memberOf ?b . ?b ex:p* ?u }}"),
        // A predicate the store has never seen: only the zero-length case answers.
        format!("{p}SELECT ?o WHERE {{ ex:a ex:nosuch* ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:a ex:nosuch+ ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:a ex:nosuch? ?o }}"),
        // A term that appears only as an object still reaches itself.
        format!("{p}SELECT ?o WHERE {{ ex:zz ex:p* ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:zz ex:p+ ?o }}"),
        // A closure joined against another pattern on the far end.
        format!("{p}SELECT ?o WHERE {{ ex:a ex:p+ ?o . ?o ex:p ?deeper }}"),
        // A closure over a predicate that exists, from a subject that has no such edge.
        format!("{p}SELECT ?o WHERE {{ ex:sink ex:p* ?o }}"),
        // Neither end bound: the shape the join declines, which must still answer.
        format!("{p}SELECT ?s ?o WHERE {{ ?s ex:p+ ?o }}"),
        format!("{p}SELECT ?s ?o WHERE {{ ?s ex:p* ?o }}"),
        format!("{p}SELECT ?s ?o WHERE {{ ?s ex:p? ?o }}"),
        // A compound inner path, which the walk refuses and the evaluator answers.
        format!("{p}SELECT ?o WHERE {{ ex:a (ex:p|ex:q)+ ?o }}"),
        format!("{p}SELECT ?o WHERE {{ ex:a (ex:p/ex:p)* ?o }}"),
    ] {
        let joined = rows(&view, &query, &QueryOptions::new());
        let evaluated = rows(&view, &query, &QueryOptions::new().without_bind_join());
        assert_eq!(joined, evaluated, "differs for: {query}");
    }
}

/// The answers themselves, spelled out, so a change that moves *both* arms together is
/// caught. The differential test above cannot see that.
#[test]
fn the_answers_are_what_the_graph_says() {
    let e = engine();
    let session = Session::unrestricted(e.store()).expect("session");
    let view = e.view(&session);
    let p = "PREFIX ex: <http://example.com/> ";
    let names = |query: &str| -> Vec<String> {
        rows(&view, query, &QueryOptions::new())
            .into_iter()
            .map(|row| row.rsplit('/').next().unwrap_or_default().trim_end_matches('>').to_owned())
            .collect()
    };

    assert_eq!(
        names(&format!("{p}SELECT ?o WHERE {{ ex:a ex:p* ?o }}")),
        ["a", "b", "c", "d", "e"]
    );
    assert_eq!(
        names(&format!("{p}SELECT ?o WHERE {{ ex:a ex:p+ ?o }}")),
        ["b", "c", "d", "e"]
    );
    // The cycle: `+` from `f` reaches `f` again, so both are answers.
    assert_eq!(
        names(&format!("{p}SELECT ?o WHERE {{ ex:f ex:p+ ?o }}")),
        ["f", "g"]
    );
    // The diamond: `k` two ways, one row.
    assert_eq!(
        names(&format!("{p}SELECT ?o WHERE {{ ex:h ex:p+ ?o }}")),
        ["i", "j", "k"]
    );
    // Zero length under a predicate that does not exist.
    assert_eq!(
        names(&format!("{p}SELECT ?o WHERE {{ ex:a ex:nosuch* ?o }}")),
        ["a"]
    );
    assert!(names(&format!("{p}SELECT ?o WHERE {{ ex:a ex:nosuch+ ?o }}")).is_empty());
    // The sequence this work exists for: `leaf memberOf b`, then `b p*`.
    assert_eq!(
        names(&format!("{p}SELECT ?u WHERE {{ ex:leaf ex:memberOf/ex:p* ?u }}")),
        ["b", "c", "d", "e"]
    );
}

/// The join must *accept* these shapes, or the differential test above compares `spareval`
/// with itself and would pass against a closure implementation that never runs.
#[test]
fn the_fragment_accepts_a_closure_and_still_refuses_what_it_cannot_walk() {
    use spargebra::SparqlParser;
    let p = "PREFIX ex: <http://example.com/> ";
    let accepts = |query: &str| {
        let parsed = SparqlParser::new().parse_query(query).expect("parses");
        holos_engine::bindjoin::plan(&parsed).is_some()
    };

    // Walked.
    assert!(accepts(&format!("{p}SELECT ?o WHERE {{ ex:a ex:p* ?o }}")));
    assert!(accepts(&format!("{p}SELECT ?o WHERE {{ ex:a ex:p+ ?o }}")));
    assert!(accepts(&format!("{p}SELECT ?o WHERE {{ ex:a ex:p? ?o }}")));
    assert!(accepts(&format!("{p}SELECT ?s WHERE {{ ?s ex:p+ ex:d }}")));
    assert!(accepts(&format!("{p}SELECT ?s WHERE {{ ex:d ^ex:p+ ?s }}")));
    // The shape this work exists for.
    assert!(accepts(&format!(
        "{p}SELECT ?u WHERE {{ ex:leaf ex:memberOf/ex:p* ?u }}"
    )));

    // A compound inner path is not a single-predicate walk, so the fragment still refuses it
    // rather than approximating one.
    assert!(!accepts(&format!("{p}SELECT ?o WHERE {{ ex:a (ex:p|ex:q)+ ?o }}")));
    assert!(!accepts(&format!("{p}SELECT ?o WHERE {{ ex:a (ex:p/ex:p)* ?o }}")));
    // A negated property set is not a closure at all.
    assert!(!accepts(&format!("{p}SELECT ?o WHERE {{ ex:a !ex:p ?o }}")));
}
