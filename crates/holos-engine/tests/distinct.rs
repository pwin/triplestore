//! `SELECT DISTINCT ?p WHERE { ?s ?p ?o }` from the store's predicate list, checked against the
//! evaluator under the same session.
//!
//! The counts the shortcut starts from cover every graph and ignore policy, so the two ways it
//! could go wrong are naming a predicate used only in a named graph, and naming one the session
//! may not read. The data has both, and every query runs under several policies — including
//! one that fails rather than filters, where the shortcut must fail too rather than answer.

use holos_engine::view::DatasetView;
use holos_engine::{Engine, QueryOptions};
use holos_security::policy::{PrincipalMatch, Rule, Scope, Semantics};
use holos_security::{Modes, Policy, Principal, Session};
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use spargebra::SparqlParser;

const EX: &str = "http://example.com/";
const P: &str = "PREFIX ex: <http://example.com/>";

fn ex(name: &str) -> oxrdf::NamedNode {
    oxrdf::NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn engine() -> Engine {
    let trig = format!(
        "@prefix ex: <{EX}> .
         ex:a ex:knows ex:b ; ex:name \"a\" ; ex:salary 10 .
         ex:b ex:knows ex:c ; ex:name \"b\" .
         _:x ex:tag ex:b .
         ex:g {{ ex:a ex:onlyInANamedGraph ex:b . ex:a ex:knows ex:c }}"
    );
    let mut engine = Engine::new();
    engine
        .bulk_load(trig.as_bytes(), RdfFormat::TriG, None)
        .expect("load");
    engine
}

fn policies() -> Vec<(&'static str, Policy)> {
    let allow_all = || Rule::allow(Modes::ALL, Scope::Everything, PrincipalMatch::Everyone);
    let deny_salary = || {
        Rule::deny(
            Modes::READ,
            Scope::Predicate(ex("salary")),
            PrincipalMatch::Everyone,
        )
    };
    vec![
        ("permissive", Policy::permit_all()),
        (
            "salary denied",
            Policy::default()
                .with_rule(allow_all())
                .with_rule(deny_salary()),
        ),
        (
            "nothing allowed",
            Policy::default().with_rule(Rule::deny(
                Modes::ALL,
                Scope::Everything,
                PrincipalMatch::Everyone,
            )),
        ),
        (
            "salary denied, failing",
            Policy::default()
                .with_semantics(Semantics::Fail)
                .with_rule(allow_all())
                .with_rule(deny_salary()),
        ),
    ]
}

/// The answer under `policy`, sorted, or the error text.
fn answer(
    engine: &Engine,
    policy: Policy,
    query: &str,
    options: &QueryOptions,
) -> Result<Vec<String>, String> {
    let store = engine.store();
    let mut session = Session::open(store, Principal::anonymous(), policy).expect("session");
    let compiled = session.policy(store).expect("policy").clone();
    let view = DatasetView::new(store, &compiled);
    let (results, _) = Engine::query_with(&view, query, options).map_err(|e| e.to_string())?;
    let QueryResults::Solutions(iter) = results else {
        panic!("expected solutions")
    };
    let mut rows = Vec::new();
    for solution in iter {
        let solution = solution.map_err(|e| e.to_string())?;
        rows.push(
            solution
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    Ok(rows)
}

/// The shortcut and the evaluator agree under every policy; the permissive answer is returned.
/// Rows are compared in order, because `ORDER BY` queries are among those checked.
fn agreed(body: &str) -> Vec<String> {
    agreed_under(body, true)
}

/// As [`agreed`], optionally leaving out the policy that fails rather than filters.
///
/// Under that policy a query fails exactly when its access path reads a denied quad, so two
/// paths that read different quads can disagree about failing while agreeing about every
/// answer — the bind join and the evaluator already do, on `?s ?p ?o . ?o ex:knows ?x`, before
/// any of this. The shortcut's own shapes are held to it; the shapes it leaves alone are not
/// this module's to settle.
fn agreed_under(body: &str, failing: bool) -> Vec<String> {
    let engine = engine();
    let query = format!("{P} {body}");
    let mut permissive = Vec::new();
    for (name, policy) in policies() {
        if !failing && name.ends_with("failing") {
            continue;
        }
        let fast = answer(&engine, policy.clone(), &query, &QueryOptions::new());
        let slow = answer(&engine, policy, &query, &QueryOptions::new().explaining());
        match (&fast, &slow) {
            (Ok(f), Ok(s)) => {
                let (mut f, mut s) = (f.clone(), s.clone());
                if !body.contains("ORDER BY") {
                    f.sort();
                    s.sort();
                }
                assert_eq!(f, s, "{name}: {body}");
                if name == "permissive" {
                    permissive = f;
                }
            }
            (Err(_), Err(_)) => {}
            _ => {
                panic!("{name}: one path failed and the other did not: {fast:?} / {slow:?}: {body}")
            }
        }
    }
    permissive
}

/// Whether the shortcut takes the query, under a permissive session.
fn taken(body: &str) -> bool {
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = SparqlParser::new()
        .parse_query(&format!("{P} {body}"))
        .expect("parse");
    holos_engine::distinct::materialised(&view, &query)
        .expect("store")
        .is_some()
}

#[test]
fn the_predicates_of_the_default_graph() {
    let body = "SELECT DISTINCT ?p WHERE { ?s ?p ?o }";
    assert!(taken(body));
    let rows = agreed(body);
    assert_eq!(rows.len(), 4, "knows, name, salary, tag: {rows:?}");
    assert!(
        !rows.iter().any(|r| r.contains("onlyInANamedGraph")),
        "a named graph's predicate is not the default graph's: {rows:?}"
    );
}

#[test]
fn a_denied_predicate_is_not_named() {
    let engine = engine();
    let policy = Policy::default()
        .with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Everyone,
        ))
        .with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(ex("salary")),
            PrincipalMatch::Everyone,
        ));
    let rows = answer(
        &engine,
        policy,
        &format!("{P} SELECT DISTINCT ?p WHERE {{ ?s ?p ?o }}"),
        &QueryOptions::new(),
    )
    .expect("filtered, not failed");
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("salary")), "{rows:?}");
}

#[test]
fn ordered_and_limited() {
    let body = "SELECT DISTINCT ?p WHERE { ?s ?p ?o } ORDER BY DESC(?p) LIMIT 2";
    assert!(taken(body));
    let rows = agreed(body);
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn reduced_and_blank_nodes() {
    assert!(taken("SELECT REDUCED ?p WHERE { ?s ?p ?o }"));
    agreed("SELECT REDUCED ?p WHERE { ?s ?p ?o }");
    assert!(taken("SELECT DISTINCT ?p WHERE { [] ?p [] }"));
    agreed("SELECT DISTINCT ?p WHERE { [] ?p [] }");
}

#[test]
fn other_shapes_are_left_alone_and_still_agree() {
    for body in [
        // The same variable twice: a self-loop, not mere use of the predicate.
        "SELECT DISTINCT ?p WHERE { ?s ?p ?s }",
        // A constant: which predicates point *at* ex:b.
        "SELECT DISTINCT ?p WHERE { ?s ?p ex:b }",
        // Two patterns.
        "SELECT DISTINCT ?p WHERE { ?s ?p ?o . ?o ex:knows ?x }",
        // A filter.
        "SELECT DISTINCT ?p WHERE { ?s ?p ?o FILTER(isIRI(?o)) }",
        // Projecting more than the predicate.
        "SELECT DISTINCT ?p ?s WHERE { ?s ?p ?o }",
        // Not distinct.
        "SELECT ?p WHERE { ?s ?p ?o }",
        // Ordered by something that is not the predicate.
        "SELECT DISTINCT ?p WHERE { ?s ?p ?o } ORDER BY ?o",
        // A named graph.
        "SELECT DISTINCT ?p WHERE { GRAPH ?g { ?s ?p ?o } }",
    ] {
        assert!(!taken(body), "{body}");
        agreed_under(body, false);
    }
}
