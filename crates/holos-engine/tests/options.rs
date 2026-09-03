//! Query options: dataset selection, substitution, timeouts, explanations.
//!
//! Each of these was already supported by the reused evaluator and simply not wired up.
//! The dataset one was worse than unwired — the HTTP server *parsed*
//! `default-graph-uri` and then ignored it, so a client asking for one graph silently got
//! answers over another.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use oxrdf::{GraphName, NamedNode, Quad, Term, Variable};
use spareval::QueryResults;
use std::time::Duration;

const EX: &str = "http://example.com/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

/// One triple in the default graph, one in `ex:g1`, one in `ex:g2`.
fn engine() -> Engine {
    let mut engine = Engine::new();
    for (s, g) in [
        ("default", GraphName::DefaultGraph),
        ("one", GraphName::NamedNode(ex("g1"))),
        ("two", GraphName::NamedNode(ex("g2"))),
    ] {
        engine
            .store_mut()
            .insert(
                Quad {
                    subject: ex(s).into(),
                    predicate: ex("p"),
                    object: Term::NamedNode(ex("o")),
                    graph_name: g,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    engine
}

fn subjects(
    engine: &Engine,
    session: &Session,
    sparql: &str,
    options: &QueryOptions,
) -> Vec<String> {
    let view = engine.view(session);
    let (results, _) = Engine::query_with(&view, sparql, options).expect("query");
    let mut out = match results {
        QueryResults::Solutions(iter) => iter
            .map(|s| {
                s.expect("solution")
                    .get("s")
                    .map(ToString::to_string)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>(),
        _ => panic!("expected solutions"),
    };
    out.sort();
    out
}

// --------------------------------------------------------------- dataset selection

#[test]
fn by_default_only_the_default_graph_is_queried() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let found = subjects(
        &engine,
        &session,
        "SELECT ?s WHERE { ?s ?p ?o }",
        &QueryOptions::new(),
    );
    assert_eq!(found, vec![format!("<{EX}default>")]);
}

#[test]
fn default_graph_uri_selects_a_named_graph_as_the_default() {
    // This is the bug the option fixes: before, this query answered over the store's
    // default graph regardless of what the client asked for.
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let options = QueryOptions::new().with_default_graph(GraphName::NamedNode(ex("g1")));
    let found = subjects(&engine, &session, "SELECT ?s WHERE { ?s ?p ?o }", &options);
    assert_eq!(found, vec![format!("<{EX}one>")]);
}

#[test]
fn several_graphs_merge_into_the_default_graph() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let options = QueryOptions::new()
        .with_default_graph(GraphName::NamedNode(ex("g1")))
        .with_default_graph(GraphName::NamedNode(ex("g2")));
    let found = subjects(&engine, &session, "SELECT ?s WHERE { ?s ?p ?o }", &options);
    assert_eq!(found, vec![format!("<{EX}one>"), format!("<{EX}two>")]);
}

#[test]
fn named_graph_uri_restricts_what_graph_can_range_over() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");

    // Unrestricted: both named graphs are visible.
    let all = subjects(
        &engine,
        &session,
        "SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o } }",
        &QueryOptions::new(),
    );
    assert_eq!(all.len(), 2);

    let options = QueryOptions::new().with_named_graph(ex("g2").into());
    let restricted = subjects(
        &engine,
        &session,
        "SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o } }",
        &options,
    );
    assert_eq!(restricted, vec![format!("<{EX}two>")]);
}

#[test]
fn the_union_default_graph_is_the_union_of_the_named_graphs() {
    // Worth pinning down, because "union default graph" sounds like it should include the
    // store default graph as well. It does not: the two named graphs are unioned, and the
    // one triple sitting in the default graph is not among them.
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let mut options = QueryOptions::new();
    options.union_default_graph = true;
    let found = subjects(&engine, &session, "SELECT ?s WHERE { ?s ?p ?o }", &options);
    assert_eq!(found, vec![format!("<{EX}one>"), format!("<{EX}two>")]);
}

// --------------------------------------------------------------- substitution

#[test]
fn a_variable_can_be_bound_without_touching_the_query_text() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let options = QueryOptions::new()
        .with_substitution(Variable::new_unchecked("s"), Term::NamedNode(ex("default")));
    let found = subjects(&engine, &session, "SELECT ?s WHERE { ?s ?p ?o }", &options);
    assert_eq!(found, vec![format!("<{EX}default>")]);

    // The same query bound to a subject that is not there. Without this, the assertion
    // above passes whether the substitution was applied or silently dropped — `ex:default`
    // is the only subject in the default graph, so ignoring the binding gives the same
    // answer. That is exactly how a fast path that did not model substitution went
    // unnoticed here while failing `substitution_cannot_inject_sparql` next door.
    let absent = QueryOptions::new()
        .with_substitution(Variable::new_unchecked("s"), Term::NamedNode(ex("absent")));
    let found = subjects(&engine, &session, "SELECT ?s WHERE { ?s ?p ?o }", &absent);
    assert!(found.is_empty(), "no triple has ex:absent as its subject");
}

#[test]
fn substitution_cannot_inject_sparql() {
    // The point of parameter binding. A value that would be catastrophic if interpolated
    // into query text is inert as a substitution, because it never reaches the parser —
    // it stays one literal term.
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let hostile = Term::Literal(oxrdf::Literal::new_simple_literal(
        "} INSERT DATA { <urn:evil> <urn:p> <urn:o> } #",
    ));
    let options = QueryOptions::new().with_substitution(Variable::new_unchecked("o"), hostile);

    let view = engine.view(&session);
    let (results, _) =
        Engine::query_with(&view, "SELECT ?s ?o WHERE { ?s ?p ?o }", &options).expect("query");
    let n = match results {
        QueryResults::Solutions(iter) => iter.count(),
        _ => panic!("expected solutions"),
    };
    // No triple has that literal as its object, so nothing matches, and nothing was
    // executed as SPARQL.
    assert_eq!(n, 0);
    assert_eq!(engine.store().len(), 3, "the store is untouched");
}

// --------------------------------------------------------------- explain

#[test]
fn a_plan_comes_back_when_asked_for() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);

    let (_, none) = Engine::query_with(&view, "SELECT ?s WHERE { ?s ?p ?o }", &QueryOptions::new())
        .expect("query");
    assert!(none.is_none(), "no plan unless it was asked for");

    let (results, plan) = Engine::query_with(
        &view,
        "SELECT ?s WHERE { ?s ?p ?o }",
        &QueryOptions::new().explaining(),
    )
    .expect("query");
    // Statistics are only populated once the results have been consumed.
    if let QueryResults::Solutions(iter) = results {
        let _ = iter.count();
    }
    let plan = plan.expect("a plan");
    let mut json = Vec::new();
    plan.write_in_json(&mut json).expect("serialise");
    let json = String::from_utf8(json).expect("utf8");
    assert!(json.contains("plan"), "unexpected plan JSON: {json}");
}

// --------------------------------------------------------------- timeout

#[test]
fn a_generous_timeout_does_not_interfere() {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let options = QueryOptions::new().with_timeout(Duration::from_secs(30));
    let found = subjects(&engine, &session, "SELECT ?s WHERE { ?s ?p ?o }", &options);
    assert_eq!(found.len(), 1);
}

#[test]
fn a_long_running_query_is_stopped() {
    // What the timeout actually guarantees: a query that is *streaming rows* is cut off.
    // Both layers apply here — the evaluator checks its token when it reads from the
    // store, and the result iterator checks the deadline on every row.
    //
    // What it does not guarantee is stopping a query blocked inside a single step without
    // touching the store; see QueryOptions::timeout.
    let mut engine = Engine::new();
    for i in 0..20_000 {
        engine
            .store_mut()
            .insert(
                Quad {
                    subject: ex(&format!("s{i}")).into(),
                    predicate: ex("p"),
                    object: Term::NamedNode(ex(&format!("o{i}"))),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let options = QueryOptions::new().with_timeout(Duration::from_millis(60));

    let started = std::time::Instant::now();
    let outcome = Engine::query_with(&view, "SELECT * WHERE { ?a ?b ?c . ?d ?e ?f }", &options)
        .and_then(|(results, _)| match results {
            QueryResults::Solutions(iter) => {
                let mut n = 0_u64;
                for solution in iter {
                    solution?;
                    n += 1;
                }
                Ok(n)
            }
            _ => Ok(0),
        });
    let elapsed = started.elapsed();

    assert!(
        outcome.is_err(),
        "a 400-million-row cross product should have been cancelled, not completed"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "cancellation took too long: {elapsed:?}"
    );
}
