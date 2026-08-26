//! Graph Store Protocol semantics.
//!
//! These drive the module directly rather than over HTTP. The HTTP layer is thin — resolve
//! a target, take a lock, pick a status code — while the behaviour worth pinning is what
//! each verb *does* to the store, and testing that without a socket makes the failures
//! legible.
//!
//! The status-code mapping is checked in `main.rs`'s dispatch and by hand against a running
//! server; what is checked here is that `PUT` replaces where `POST` merges, that `DELETE`
//! removes the graph rather than emptying it, and that policy is not bypassable.

use holos_engine::Engine;
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use oxrdf::{GraphNameRef, NamedNode};
use oxrdfio::RdfFormat;

// The server is a binary, so its modules are reachable through the binary's own test
// harness rather than as a library. This mirrors the module under test.
#[path = "../src/gsp.rs"]
mod gsp;

const EX: &str = "http://example.org/";

fn graph() -> gsp::Target {
    gsp::Target::Named(NamedNode::new_unchecked(format!("{EX}g1")))
}

fn turtle(local: &str, value: &str) -> Vec<u8> {
    format!("<{EX}{local}> <{EX}p> \"{value}\" .\n").into_bytes()
}

fn setup() -> (Engine, Session) {
    let engine = Engine::new();
    let session = Session::open(engine.store(), Principal::anonymous(), Policy::permit_all())
        .expect("session");
    (engine, session)
}

fn triples_in(engine: &Engine, session: &Session, target: &gsp::Target) -> usize {
    let bytes = gsp::read(engine, session, target, RdfFormat::NTriples).expect("read");
    String::from_utf8(bytes)
        .expect("utf8")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[test]
fn a_graph_starts_absent() {
    let (engine, session) = setup();
    assert!(!gsp::exists(&engine, &session, &graph()).expect("exists"));
}

#[test]
fn post_merges_and_put_replaces() {
    // The entire distinction between the two verbs. Getting it backwards would silently
    // accumulate data a client expected to have been replaced.
    let (mut engine, mut session) = setup();
    let target = graph();

    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("a", "1"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("b", "2"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    assert_eq!(
        triples_in(&engine, &session, &target),
        2,
        "POST accumulates"
    );

    // PUT is clear-then-merge.
    gsp::clear(&mut engine, &mut session, &target).expect("clear");
    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("z", "9"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    assert_eq!(triples_in(&engine, &session, &target), 1, "PUT replaces");
}

#[test]
fn delete_removes_the_graph_not_just_its_contents() {
    // If DELETE only emptied the graph, a second DELETE could not answer 404 and a client
    // could not tell "I removed it" from "it was not there".
    let (mut engine, mut session) = setup();
    let target = graph();

    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("a", "1"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    gsp::create(&mut engine, &target).expect("create");
    assert!(gsp::exists(&engine, &session, &target).expect("exists"));

    gsp::drop_graph(&mut engine, &mut session, &target).expect("drop");
    assert!(
        !gsp::exists(&engine, &session, &target).expect("exists"),
        "the graph must be gone, not empty"
    );
    assert!(!engine
        .store()
        .contains_named_graph(GraphNameRef::NamedNode(
            NamedNode::new_unchecked(format!("{EX}g1")).as_ref()
        ))
        .expect("catalogue"));
}

#[test]
fn clear_leaves_the_graph_existing_but_empty() {
    // The counterpart to the test above: CLEAR and DROP are different operations, and
    // SPARQL Update exposes both.
    let (mut engine, mut session) = setup();
    let target = graph();
    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("a", "1"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    gsp::create(&mut engine, &target).expect("create");

    gsp::clear(&mut engine, &mut session, &target).expect("clear");
    assert_eq!(triples_in(&engine, &session, &target), 0);
    assert!(
        gsp::exists(&engine, &session, &target).expect("exists"),
        "an emptied graph still exists"
    );
}

#[test]
fn an_empty_graph_can_be_created() {
    let (mut engine, session) = setup();
    let target = graph();
    gsp::create(&mut engine, &target).expect("create");
    assert!(gsp::exists(&engine, &session, &target).expect("exists"));
    assert_eq!(triples_in(&engine, &session, &target), 0);
}

#[test]
fn a_body_that_fails_to_parse_leaves_nothing_behind() {
    // Parsing completes before anything is written, so a body that goes wrong half way
    // through does not leave half a graph.
    let (mut engine, mut session) = setup();
    let target = graph();
    let half_bad = b"<http://example.org/a> <http://example.org/p> \"ok\" .\nthis is not turtle";
    assert!(gsp::merge(
        &mut engine,
        &mut session,
        &target,
        half_bad,
        RdfFormat::NTriples,
        None
    )
    .is_err());
    assert_eq!(
        triples_in(&engine, &session, &target),
        0,
        "the valid first line must not have been written"
    );
}

#[test]
fn writing_a_denied_predicate_is_refused() {
    // A REST verb is not a way around policy.
    let engine = Engine::new();
    let policy = Policy::permit_all().with_rule(Rule::deny(
        Modes::WRITE,
        Scope::Predicate(NamedNode::new_unchecked(format!("{EX}p"))),
        PrincipalMatch::Everyone,
    ));
    let mut session =
        Session::open(engine.store(), Principal::anonymous(), policy).expect("session");
    let mut engine = engine;
    assert!(gsp::merge(
        &mut engine,
        &mut session,
        &graph(),
        &turtle("a", "1"),
        RdfFormat::NTriples,
        None
    )
    .is_err());
}

#[test]
fn a_graph_the_principal_cannot_read_is_absent_to_them() {
    // Reporting it as present-but-empty would confirm it exists, which is what the policy
    // was withholding. 404 is the honest answer.
    let mut engine = Engine::new();
    let mut writer = Session::open(engine.store(), Principal::anonymous(), Policy::permit_all())
        .expect("session");
    let target = graph();
    gsp::merge(
        &mut engine,
        &mut writer,
        &target,
        &turtle("a", "1"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    gsp::create(&mut engine, &target).expect("create");

    let hidden = Policy::permit_all().with_rule(Rule::deny(
        Modes::READ,
        Scope::Graph(NamedNode::new_unchecked(format!("{EX}g1"))),
        PrincipalMatch::Everyone,
    ));
    let restricted =
        Session::open(engine.store(), Principal::anonymous(), hidden).expect("session");

    assert_eq!(triples_in(&engine, &restricted, &target), 0);
    // The catalogue still holds the graph, so `exists` reports true — the HTTP layer's
    // 404 comes from the *visible* content being empty for this principal. Recorded here
    // because it is the one place the two notions of "exists" differ.
    let visible = gsp::read(&engine, &restricted, &target, RdfFormat::NTriples).expect("read");
    assert!(visible.is_empty(), "no quad may leak through");
}

#[test]
fn the_default_graph_is_addressable() {
    let (mut engine, mut session) = setup();
    let target = gsp::Target::Default;
    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("d", "0"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    assert_eq!(triples_in(&engine, &session, &target), 1);
    assert!(gsp::exists(&engine, &session, &target).expect("exists"));
}

#[test]
fn a_graph_is_served_as_triples_not_quads() {
    // The graph name is the request, not the payload. Serialising quads would put the
    // name in the body where a client asked for a document.
    let (mut engine, mut session) = setup();
    let target = graph();
    gsp::merge(
        &mut engine,
        &mut session,
        &target,
        &turtle("a", "1"),
        RdfFormat::NTriples,
        None,
    )
    .expect("merge");
    let body = String::from_utf8(
        gsp::read(&engine, &session, &target, RdfFormat::NTriples).expect("read"),
    )
    .expect("utf8");
    assert!(
        !body.contains("g1"),
        "the graph name must not appear: {body}"
    );
}
