//! A tick is one commit, and a failed tick is no commit.
//!
//! `tick.rs` covers what a tick *means*: the boundary admits or refuses, and the event log
//! records the attempt either way. This file covers the other axis — what happens when a tick
//! does not finish. There are two different things being asked for, and they are easy to
//! conflate:
//!
//! - A commit the boundary **refuses** is unapplied but still recorded. That is not a
//!   failure, it is the boundary doing its job, and the event has to survive it.
//! - A tick that **fails** — policy denies a write, a rule set runs away, a store call
//!   errors — leaves nothing at all, not even an event.
//!
//! The second used to be done by writing the inverse of each change at each `return`, which
//! meant every path out of the tick had to remember, and an error raised by `?` inside a
//! store call remembered nothing. It is now a commit scope opened around the whole tick, so
//! the compiler cannot route around it.
//!
//! The failure that would be quietest is a scope left open: the tick returns, nothing looks
//! wrong, and the *next* tick cannot begin. So every test here ticks again afterwards.

use holos_engine::Engine;
use holos_holon::{registry, tick, Delta, Holon, HolonError};
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use holos_store::Store;
use oxrdf::vocab::rdf;
use oxrdf::{Literal, NamedNode, Triple};

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

fn holon() -> Holon {
    Holon::new(NamedNode::new_unchecked("urn:holon:people"))
}

fn admin_session(store: &Store) -> Session {
    Session::open(
        store,
        Principal::anonymous().with_role("admin"),
        Policy::default().with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Role("admin".into()),
        )),
    )
    .expect("session")
}

/// A principal that may write anything except `ex:name`.
///
/// Denying *everything* would be no test at all: the first triple of the delta would be
/// refused before the tick had applied any of it, and "nothing was left behind" would hold
/// without a rollback ever running. Denying one predicate puts the refusal in the middle —
/// the type triple is already in the scene when the name triple is turned away.
fn half_denied_session(store: &Store) -> Session {
    Session::open(
        store,
        Principal::anonymous().with_role("half"),
        Policy::default()
            .with_rule(Rule::allow(
                Modes::ALL,
                Scope::Everything,
                PrincipalMatch::Role("half".into()),
            ))
            .with_rule(Rule::deny(
                Modes::WRITE,
                Scope::Predicate(ex("name")),
                PrincipalMatch::Role("half".into()),
            )),
    )
    .expect("session")
}

fn person(name: &str, label: &str) -> Vec<Triple> {
    vec![
        Triple {
            subject: ex(name).into(),
            predicate: rdf::TYPE.into_owned(),
            object: ex("Person").into(),
        },
        Triple {
            subject: ex(name).into(),
            predicate: ex("name"),
            object: Literal::new_simple_literal(label).into(),
        },
    ]
}

/// An engine with the holon registered and no boundary, so nothing is refused and the only
/// way a tick can end early is by failing.
fn prepared() -> (Engine, Holon, Session) {
    let mut engine = Engine::new();
    let h = holon();
    let mut session = admin_session(engine.store());
    registry::register(&mut engine, &h, &mut session).expect("register");
    (engine, h, session)
}

fn scene_size(engine: &Engine, holon: &Holon) -> usize {
    holos_holon::graph_size(engine.store(), &holon.scene)
}

fn events_size(engine: &Engine, holon: &Holon) -> usize {
    holos_holon::graph_size(engine.store(), &holon.events)
}

#[test]
fn a_denied_tick_leaves_nothing_at_all() {
    let (mut engine, h, mut session) = prepared();
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice")),
    )
    .expect("the first tick");

    let scene = scene_size(&engine, &h);
    let events = events_size(&engine, &h);
    let version = registry::version(&engine, &h).expect("version");
    let total = engine.store().len();

    // The denial lands on the second triple of the delta, so the first is already in the
    // scene when the tick fails and the rollback has something real to undo.
    let mut half = half_denied_session(engine.store());
    let outcome = tick(
        &mut engine,
        &h,
        &mut half,
        &Delta::adding(person("bob", "Bob")),
    );
    assert!(
        matches!(outcome, Err(HolonError::WriteDenied(_))),
        "expected a write denial, got {outcome:?}"
    );

    assert_eq!(
        scene_size(&engine, &h),
        scene,
        "the scene kept a denied write"
    );
    assert_eq!(
        events_size(&engine, &h),
        events,
        "a failed tick is not an event: nothing was committed for one to describe"
    );
    assert_eq!(registry::version(&engine, &h).expect("version"), version);
    assert_eq!(
        engine.store().len(),
        total,
        "a failed tick left something outside the scene"
    );
}

#[test]
fn a_failed_tick_does_not_leave_a_scope_open() {
    let (mut engine, h, mut session) = prepared();

    let mut half = half_denied_session(engine.store());
    tick(
        &mut engine,
        &h,
        &mut half,
        &Delta::adding(person("bob", "Bob")),
    )
    .expect_err("the denied tick");

    assert!(
        !engine.store().in_scope(),
        "the failed tick left its commit scope open"
    );

    // The real test of that: the next tick must work. A leaked scope would refuse to open a
    // new one, and every tick after the first failure would fail for an unrelated reason.
    let outcome = tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("carol", "Carol")),
    )
    .expect("the tick after a failure");
    assert!(outcome.admitted);
    assert_eq!(scene_size(&engine, &h), 2);
}

#[test]
fn a_successful_tick_does_not_leave_a_scope_open() {
    let (mut engine, h, mut session) = prepared();
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice")),
    )
    .expect("tick");
    assert!(
        !engine.store().in_scope(),
        "a committed tick left its scope open"
    );
}

#[test]
fn a_tick_inside_a_caller_scope_joins_that_commit() {
    let (mut engine, h, mut session) = prepared();

    // A caller with its own scope open: the tick belongs to that commit rather than making
    // one of its own, so abandoning the caller's scope abandons the tick too. Two ticks, so
    // the second one also has to find the scope already open and leave it that way.
    engine.store_mut().begin().expect("begin");
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice")),
    )
    .expect("the first tick");
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("bob", "Bob")),
    )
    .expect("the second tick");
    assert_eq!(scene_size(&engine, &h), 4);
    assert!(
        engine.store().in_scope(),
        "the tick closed a scope it did not open"
    );

    engine.store_mut().rollback();
    assert_eq!(
        scene_size(&engine, &h),
        0,
        "abandoning the caller's scope must abandon the ticks inside it"
    );
    assert_eq!(events_size(&engine, &h), 0);
}
