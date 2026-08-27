//! SPARQL 1.1 Update, end to end.
//!
//! Two things are being checked here, and the second matters more than the first:
//! that each operation does what the specification says, and that **policy is not
//! bypassable through the write path**. An update that could delete what a query could not
//! see would make every guarantee in §14 conditional on nobody having update rights.

use holos_engine::update::{apply, parse, update, with_protocol_dataset, UpdateOutcome};
use holos_engine::{Engine, EngineError};
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use oxrdf::{GraphName, NamedNode, Quad, Term};
use spareval::QueryResults;

const EX: &str = "http://example.com/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn engine_with(quads: &[(&str, &str, &str, Option<&str>)]) -> Engine {
    let mut engine = Engine::new();
    for (s, p, o, g) in quads {
        engine
            .store_mut()
            .insert(
                Quad {
                    subject: ex(s).into(),
                    predicate: ex(p),
                    object: Term::NamedNode(ex(o)),
                    graph_name: g.map_or(GraphName::DefaultGraph, |g| GraphName::NamedNode(ex(g))),
                }
                .as_ref(),
            )
            .expect("insert");
    }
    engine
}

fn unrestricted(engine: &Engine) -> Session {
    Session::unrestricted(engine.store()).expect("session")
}

fn count(engine: &Engine, session: &Session, sparql: &str) -> usize {
    let view = engine.view(session);
    let n = match Engine::query(&view, sparql, None).expect("query") {
        QueryResults::Solutions(iter) => iter.count(),
        _ => panic!("expected solutions"),
    };
    n
}

fn run(engine: &mut Engine, sparql: &str) -> Result<UpdateOutcome, EngineError> {
    let mut session = unrestricted(engine);
    update(engine, &mut session, sparql, None)
}

// --------------------------------------------------------------------------- the basics

#[test]
fn insert_data_adds_quads() {
    let mut engine = Engine::new();
    let outcome = run(
        &mut engine,
        &format!("INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }}"),
    )
    .expect("update");
    assert_eq!(outcome.inserted, 1);
    assert_eq!(engine.store().len(), 1);
}

#[test]
fn insert_data_is_idempotent() {
    let mut engine = Engine::new();
    let sparql = format!("INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }}");
    assert_eq!(run(&mut engine, &sparql).expect("first").inserted, 1);
    // The second insert changes nothing, and must report so: the count is "quads that
    // were not there and now are", not "quads named in the request".
    assert_eq!(run(&mut engine, &sparql).expect("second").inserted, 0);
    assert_eq!(engine.store().len(), 1);
}

#[test]
fn insert_data_into_a_named_graph() {
    let mut engine = Engine::new();
    run(
        &mut engine,
        &format!("INSERT DATA {{ GRAPH <{EX}g> {{ <{EX}a> <{EX}p> <{EX}b> }} }}"),
    )
    .expect("update");
    let session = unrestricted(&engine);
    assert_eq!(count(&engine, &session, "SELECT * WHERE { ?s ?p ?o }"), 0);
    assert_eq!(
        count(
            &engine,
            &session,
            "SELECT * WHERE { GRAPH ?g { ?s ?p ?o } }"
        ),
        1
    );
}

#[test]
fn delete_data_removes_exactly_what_it_names() {
    let mut engine = engine_with(&[("a", "p", "b", None), ("a", "p", "c", None)]);
    let outcome = run(
        &mut engine,
        &format!("DELETE DATA {{ <{EX}a> <{EX}p> <{EX}b> }}"),
    )
    .expect("update");
    assert_eq!(outcome.deleted, 1);
    assert_eq!(engine.store().len(), 1);
}

#[test]
fn delete_where_removes_matches() {
    let mut engine = engine_with(&[("a", "p", "b", None), ("c", "p", "d", None)]);
    let outcome = run(&mut engine, &format!("DELETE WHERE {{ ?s <{EX}p> ?o }}")).expect("update");
    assert_eq!(outcome.deleted, 2);
    assert_eq!(engine.store().len(), 0);
}

#[test]
fn delete_insert_rewrites_in_place() {
    let mut engine = engine_with(&[("a", "status", "draft", None)]);
    let outcome = run(
        &mut engine,
        &format!(
            "DELETE {{ ?s <{EX}status> <{EX}draft> }} \
             INSERT {{ ?s <{EX}status> <{EX}published> }} \
             WHERE  {{ ?s <{EX}status> <{EX}draft> }}"
        ),
    )
    .expect("update");
    assert_eq!((outcome.deleted, outcome.inserted), (1, 1));

    let session = unrestricted(&engine);
    assert_eq!(
        count(
            &engine,
            &session,
            &format!("SELECT * WHERE {{ ?s <{EX}status> <{EX}published> }}")
        ),
        1
    );
    assert_eq!(
        count(
            &engine,
            &session,
            &format!("SELECT * WHERE {{ ?s <{EX}status> <{EX}draft> }}")
        ),
        0
    );
}

#[test]
fn deletes_happen_before_inserts() {
    // The ordering rule in SPARQL 1.1 Update §3.1.3. If inserts ran first, the delete
    // template would match what had just been written and remove it again, leaving
    // nothing. One triple surviving is the whole assertion.
    let mut engine = engine_with(&[("a", "p", "old", None)]);
    run(
        &mut engine,
        &format!(
            "DELETE {{ ?s <{EX}p> ?o }} INSERT {{ ?s <{EX}p> <{EX}new> }} \
             WHERE {{ ?s <{EX}p> ?o }}"
        ),
    )
    .expect("update");
    assert_eq!(engine.store().len(), 1);
    let session = unrestricted(&engine);
    assert_eq!(
        count(
            &engine,
            &session,
            &format!("SELECT * WHERE {{ ?s <{EX}p> <{EX}new> }}")
        ),
        1
    );
}

#[test]
fn insert_where_copies_between_graphs() {
    let mut engine = engine_with(&[("a", "p", "b", None)]);
    run(
        &mut engine,
        &format!("INSERT {{ GRAPH <{EX}g> {{ ?s ?p ?o }} }} WHERE {{ ?s ?p ?o }}"),
    )
    .expect("update");
    assert_eq!(engine.store().len(), 2);
}

// --------------------------------------------------------------------------- graphs

#[test]
fn create_and_drop_a_graph() {
    let mut engine = Engine::new();
    let outcome = run(&mut engine, &format!("CREATE GRAPH <{EX}g>")).expect("create");
    assert_eq!(outcome.graphs_created, 1);

    let outcome = run(&mut engine, &format!("DROP GRAPH <{EX}g>")).expect("drop");
    assert_eq!(outcome.graphs_dropped, 1);
}

#[test]
fn creating_an_existing_graph_fails_unless_silent() {
    let mut engine = Engine::new();
    run(&mut engine, &format!("CREATE GRAPH <{EX}g>")).expect("create");
    assert!(run(&mut engine, &format!("CREATE GRAPH <{EX}g>")).is_err());
    assert!(run(&mut engine, &format!("CREATE SILENT GRAPH <{EX}g>")).is_ok());
}

#[test]
fn dropping_a_missing_graph_fails_unless_silent() {
    let mut engine = Engine::new();
    assert!(run(&mut engine, &format!("DROP GRAPH <{EX}missing>")).is_err());
    assert!(run(&mut engine, &format!("DROP SILENT GRAPH <{EX}missing>")).is_ok());
}

#[test]
fn clear_empties_a_graph_but_keeps_it() {
    let mut engine = engine_with(&[("a", "p", "b", Some("g"))]);
    let outcome = run(&mut engine, &format!("CLEAR GRAPH <{EX}g>")).expect("clear");
    assert_eq!(outcome.deleted, 1);
    assert_eq!(engine.store().len(), 0);
    // CLEAR is not DROP: the graph is still there to be inserted into.
    assert!(engine
        .store()
        .contains_named_graph(oxrdf::GraphNameRef::NamedNode(ex("g").as_ref()))
        .expect("lookup"));
}

#[test]
fn clear_default_leaves_named_graphs_alone() {
    let mut engine = engine_with(&[("a", "p", "b", None), ("c", "p", "d", Some("g"))]);
    run(&mut engine, "CLEAR DEFAULT").expect("clear");
    assert_eq!(engine.store().len(), 1);
}

#[test]
fn clear_all_removes_everything() {
    let mut engine = engine_with(&[("a", "p", "b", None), ("c", "p", "d", Some("g"))]);
    run(&mut engine, "CLEAR ALL").expect("clear");
    assert_eq!(engine.store().len(), 0);
}

#[test]
fn drop_named_leaves_the_default_graph() {
    let mut engine = engine_with(&[("a", "p", "b", None), ("c", "p", "d", Some("g"))]);
    run(&mut engine, "DROP NAMED").expect("drop");
    assert_eq!(engine.store().len(), 1);
}

// --------------------------------------------------------------------------- atomicity

#[test]
fn a_failing_operation_rolls_the_whole_update_back() {
    let mut engine = engine_with(&[("keep", "p", "b", None)]);
    let before = engine.store().len();

    // The first operation succeeds, the second cannot: the graph does not exist. The
    // update must leave nothing behind, including the successful first half.
    let result = run(
        &mut engine,
        &format!("INSERT DATA {{ <{EX}new> <{EX}p> <{EX}x> }} ; DROP GRAPH <{EX}missing>"),
    );
    assert!(result.is_err(), "the second operation should fail");
    assert_eq!(
        engine.store().len(),
        before,
        "the successful first operation must have been undone"
    );
}

#[test]
fn rollback_does_not_delete_pre_existing_quads() {
    // The subtle failure mode: if the journal recorded *attempted* inserts rather than
    // *effective* ones, undoing would delete a quad that was already there before the
    // update ran. Inserting something that already exists and then failing is exactly
    // the case that catches it.
    let mut engine = engine_with(&[("a", "p", "b", None)]);
    let result = run(
        &mut engine,
        &format!("INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }} ; DROP GRAPH <{EX}missing>"),
    );
    assert!(result.is_err());
    assert_eq!(
        engine.store().len(),
        1,
        "the pre-existing quad must survive the rollback"
    );
}

#[test]
fn a_successful_multi_operation_update_keeps_everything() {
    let mut engine = Engine::new();
    let outcome = run(
        &mut engine,
        &format!(
            "INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }} ; \
             INSERT DATA {{ <{EX}c> <{EX}p> <{EX}d> }} ; \
             CREATE GRAPH <{EX}g>"
        ),
    )
    .expect("update");
    assert_eq!(outcome.inserted, 2);
    assert_eq!(outcome.graphs_created, 1);
    assert_eq!(engine.store().len(), 2);
}

#[test]
fn later_operations_see_earlier_ones() {
    // SPARQL requires sequential semantics within one update request.
    let mut engine = Engine::new();
    run(
        &mut engine,
        &format!(
            "INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }} ; \
             DELETE WHERE {{ ?s <{EX}p> ?o }}"
        ),
    )
    .expect("update");
    assert_eq!(
        engine.store().len(),
        0,
        "the DELETE must have seen what the INSERT just wrote"
    );
}

// --------------------------------------------------------------------------- policy

fn deny_writes_to(predicate: &str) -> Policy {
    Policy::permit_all().with_rule(Rule::deny(
        Modes::WRITE,
        Scope::Predicate(ex(predicate)),
        PrincipalMatch::Everyone,
    ))
}

#[test]
fn a_write_denied_by_policy_is_refused() {
    let mut engine = Engine::new();
    let mut session = Session::open(
        engine.store(),
        Principal::anonymous(),
        deny_writes_to("secret"),
    )
    .expect("session");

    let result = update(
        &mut engine,
        &mut session,
        &format!("INSERT DATA {{ <{EX}a> <{EX}secret> <{EX}b> }}"),
        None,
    );
    assert!(matches!(result, Err(EngineError::AccessDenied)));
    assert_eq!(engine.store().len(), 0);
}

#[test]
fn a_refused_write_rolls_back_the_permitted_part() {
    // The property that makes policy on the write path worth having: a partially applied
    // update would let a principal keep whatever the policy had not got round to refusing.
    let mut engine = Engine::new();
    let mut session = Session::open(
        engine.store(),
        Principal::anonymous(),
        deny_writes_to("secret"),
    )
    .expect("session");

    let result = update(
        &mut engine,
        &mut session,
        &format!(
            "INSERT DATA {{ <{EX}a> <{EX}allowed> <{EX}b> }} ; \
             INSERT DATA {{ <{EX}a> <{EX}secret> <{EX}b> }}"
        ),
        None,
    );
    assert!(matches!(result, Err(EngineError::AccessDenied)));
    assert_eq!(
        engine.store().len(),
        0,
        "the allowed insert must not survive a denied one"
    );
}

#[test]
fn silent_does_not_silence_a_policy_refusal() {
    // SILENT suppresses the *operation's* error. Letting it suppress a denial would turn
    // it into a way to probe what one may not touch, and would report success for work
    // that never happened.
    let mut engine = engine_with(&[("a", "p", "b", Some("g"))]);
    let policy = Policy::permit_all().with_rule(Rule::deny(
        Modes::WRITE,
        Scope::Everything,
        PrincipalMatch::Everyone,
    ));
    let mut session =
        Session::open(engine.store(), Principal::anonymous(), policy).expect("session");

    let result = update(
        &mut engine,
        &mut session,
        &format!("CLEAR SILENT GRAPH <{EX}g>"),
        None,
    );
    assert!(matches!(result, Err(EngineError::AccessDenied)));
    assert_eq!(engine.store().len(), 1);
}

#[test]
fn a_principal_cannot_delete_what_it_cannot_read() {
    // The WHERE clause runs through the same DatasetView as a SELECT, so a hidden quad
    // never matches the pattern and never reaches the delete list. This is the test that
    // the update path does not have its own way into the indexes.
    let mut engine = engine_with(&[("a", "salary", "b", None), ("a", "name", "c", None)]);
    let policy = Policy::permit_all().with_rule(Rule::deny(
        Modes::READ,
        Scope::Predicate(ex("salary")),
        PrincipalMatch::Everyone,
    ));
    let mut session =
        Session::open(engine.store(), Principal::anonymous(), policy).expect("session");

    let outcome =
        update(&mut engine, &mut session, "DELETE WHERE { ?s ?p ?o }", None).expect("update");

    assert_eq!(outcome.deleted, 1, "only the readable quad was deletable");
    assert_eq!(
        engine.store().len(),
        1,
        "the unreadable quad must still be there"
    );
}

// --------------------------------------------------------------------------- parsing

#[test]
fn a_syntax_error_is_reported_as_one() {
    let mut engine = Engine::new();
    assert!(matches!(
        run(&mut engine, "INSERT DATA { this is not sparql"),
        Err(EngineError::Syntax(_))
    ));
}

#[test]
fn an_already_parsed_update_can_be_applied() {
    // The seam the conformance harness needs, so it can tell a parse failure apart from
    // an evaluation failure.
    let parsed = spargebra::SparqlParser::new()
        .parse_update(&format!("INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }}"))
        .expect("parse");
    let mut engine = Engine::new();
    let mut session = unrestricted(&engine);
    let outcome = apply(&mut engine, &mut session, &parsed).expect("apply");
    assert_eq!(outcome.inserted, 1);
}

#[test]
fn remote_load_is_refused_with_a_reason() {
    // Fetching an arbitrary URL named inside a query is a server-side request forgery
    // primitive. Refusing is a decision, so the message has to say so rather than looking
    // like a missing feature.
    let mut engine = Engine::new();
    let err = run(&mut engine, "LOAD <http://example.com/data.ttl>").expect_err("should refuse");
    let message = err.to_string();
    assert!(
        message.contains("remote fetch is not enabled"),
        "unhelpful message: {message}"
    );
}

// ---------------------------------------------------------------------------------
// the protocol's dataset
// ---------------------------------------------------------------------------------

/// `using-graph-uri` says which graphs the `WHERE` matches against.
///
/// Without it a client can only name the dataset inside the update text, which the SPARQL
/// Protocol explicitly offers as an alternative.
#[test]
fn the_protocol_can_name_the_dataset_an_update_matches_against() {
    let mut engine = engine_with(&[
        ("alice", "knows", "bob", Some("g1")),
        ("carol", "knows", "dave", Some("g2")),
    ]);
    let mut session = unrestricted(&engine);

    // No graph named in the text: the dataset comes from the request.
    let mut parsed = parse(
        "INSERT { <http://example.com/found> <http://example.com/p> ?o } WHERE { ?s <http://example.com/knows> ?o }",
        None,
    )
    .expect("parses");
    with_protocol_dataset(&mut parsed, vec![ex("g1")], Vec::new()).expect("no conflict");

    let outcome = apply(&mut engine, &mut session, &parsed).expect("applies");
    assert_eq!(
        outcome.inserted, 1,
        "only g1 was in the dataset, so only bob should have been found"
    );
}

#[test]
fn naming_the_dataset_twice_is_refused() {
    // The protocol says a request carrying both is an error rather than something to
    // resolve by preferring one. Overriding the text silently would run the update over a
    // dataset its author did not choose.
    let mut parsed = parse(
        "WITH <http://example.com/g1> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }",
        None,
    )
    .expect("parses");
    let error = with_protocol_dataset(&mut parsed, vec![ex("g2")], Vec::new())
        .expect_err("both at once must be refused");
    assert!(
        matches!(error, EngineError::BadRequest(_)),
        "a client error, not a server one: {error:?}"
    );
}

#[test]
fn an_update_with_nothing_to_match_is_left_alone() {
    // INSERT DATA has no WHERE, so there is no dataset for the parameters to name. The
    // request is not thereby an error — the parameters simply do not apply.
    let mut engine = Engine::new();
    let mut session = unrestricted(&engine);
    let mut parsed = parse(
        "INSERT DATA { <http://example.com/a> <http://example.com/p> <http://example.com/b> }",
        None,
    )
    .expect("parses");
    with_protocol_dataset(&mut parsed, vec![ex("g1")], Vec::new()).expect("not a conflict");
    assert_eq!(
        apply(&mut engine, &mut session, &parsed)
            .expect("applies")
            .inserted,
        1
    );
}

#[test]
fn no_parameters_change_nothing() {
    let text = "WITH <http://example.com/g1> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }";
    let mut parsed = parse(text, None).expect("parses");
    let before = format!("{parsed:?}");
    // An empty dataset is not a conflict even against an update that names its own.
    with_protocol_dataset(&mut parsed, Vec::new(), Vec::new()).expect("no conflict");
    assert_eq!(
        format!("{parsed:?}"),
        before,
        "the update must be untouched"
    );
}
