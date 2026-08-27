//! Branching a holon.
//!
//! §9 defines a branch as "a checkpoint plus a fresh event-log head". What that has to mean
//! in practice is the subject of these tests: the branch starts from the parent's state, the
//! two then move independently, and the branch says where it came from.
//!
//! Independence is the property worth the most attention. A branch that shared storage with
//! its parent would look correct until the first divergent write, and then be wrong in a way
//! that is very hard to see.

use holos_engine::Engine;
use holos_holon::branch::{branch, branch_point};
use holos_holon::model::Holon;
use holos_holon::{registry, tick, Delta};
use holos_security::Session;
use oxrdf::{Literal, NamedNode, Triple};

const EX: &str = "http://example.org/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn triple(subject: &str, predicate: &str, value: &str) -> Triple {
    Triple {
        subject: ex(subject).into(),
        predicate: ex(predicate),
        object: Literal::new_simple_literal(value).into(),
    }
}

fn setup() -> (Engine, Session, Holon) {
    let mut engine = Engine::new();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let holon = Holon::new(ex("holon/trunk"));
    registry::register(&mut engine, &holon, &mut session).expect("register");
    (engine, session, holon)
}

fn scene_values(engine: &Engine, session: &Session, holon: &Holon) -> Vec<String> {
    let view = engine.view(session);
    let query = format!(
        "SELECT ?v WHERE {{ GRAPH <{}> {{ ?s <{EX}name> ?v }} }} ORDER BY ?v",
        holon.scene.as_str()
    );
    let results = Engine::query(&view, &query, None).expect("query");
    let values: Vec<String> = match results {
        spareval::QueryResults::Solutions(iter) => iter
            .map(|s| s.expect("solution").get("v").expect("bound").to_string())
            .map(|t| t.trim_matches('"').to_owned())
            .collect(),
        _ => panic!("expected solutions"),
    };
    values
}

#[test]
fn a_branch_starts_from_the_parents_scene() {
    let (mut engine, mut session, trunk) = setup();
    tick(
        &mut engine,
        &trunk,
        &mut session,
        &Delta::adding([triple("a", "name", "alpha")]),
    )
    .expect("tick");

    let child = branch(&mut engine, &trunk, ex("holon/feature"), &mut session).expect("branch");
    assert_eq!(scene_values(&engine, &session, &child), ["alpha"]);
}

#[test]
fn the_two_scenes_move_independently() {
    // The property that makes it a branch rather than an alias. A shared scene would look
    // right until the first divergent write and then be silently wrong.
    let (mut engine, mut session, trunk) = setup();
    tick(
        &mut engine,
        &trunk,
        &mut session,
        &Delta::adding([triple("a", "name", "shared")]),
    )
    .expect("tick");

    let child = branch(&mut engine, &trunk, ex("holon/feature"), &mut session).expect("branch");

    tick(
        &mut engine,
        &trunk,
        &mut session,
        &Delta::adding([triple("b", "name", "trunk-only")]),
    )
    .expect("tick");
    tick(
        &mut engine,
        &child,
        &mut session,
        &Delta::adding([triple("c", "name", "branch-only")]),
    )
    .expect("tick");

    assert_eq!(
        scene_values(&engine, &session, &trunk),
        ["shared", "trunk-only"]
    );
    assert_eq!(
        scene_values(&engine, &session, &child),
        ["branch-only", "shared"]
    );
}

#[test]
fn a_branch_records_where_it_came_from() {
    let (mut engine, mut session, trunk) = setup();
    for value in ["one", "two", "three"] {
        tick(
            &mut engine,
            &trunk,
            &mut session,
            &Delta::adding([triple(value, "name", value)]),
        )
        .expect("tick");
    }
    let version = registry::version(&engine, &trunk).expect("version");
    assert_eq!(version, 3, "three ticks should be three versions");

    let child = branch(&mut engine, &trunk, ex("holon/feature"), &mut session).expect("branch");
    let point = branch_point(&engine, &child)
        .expect("reading the branch point")
        .expect("a branch should record its origin");

    assert_eq!(point.parent, trunk.id);
    assert_eq!(point.version, 3, "the branch point is the parent's version");
}

#[test]
fn versions_continue_rather_than_restart() {
    // Restarting at zero would make "version 2" ambiguous between two lineages whose scenes
    // are genuinely related. Continuing keeps the branch point legible.
    let (mut engine, mut session, trunk) = setup();
    for value in ["one", "two"] {
        tick(
            &mut engine,
            &trunk,
            &mut session,
            &Delta::adding([triple(value, "name", value)]),
        )
        .expect("tick");
    }

    let child = branch(&mut engine, &trunk, ex("holon/feature"), &mut session).expect("branch");
    assert_eq!(registry::version(&engine, &child).expect("version"), 2);

    tick(
        &mut engine,
        &child,
        &mut session,
        &Delta::adding([triple("next", "name", "next")]),
    )
    .expect("tick");
    assert_eq!(
        registry::version(&engine, &child).expect("version"),
        3,
        "the branch's first tick continues the parent's numbering"
    );
    assert_eq!(
        registry::version(&engine, &trunk).expect("version"),
        2,
        "and the parent is unmoved by it"
    );
}

#[test]
fn a_holon_that_was_not_branched_says_so() {
    let (mut engine, mut session, trunk) = setup();
    tick(
        &mut engine,
        &trunk,
        &mut session,
        &Delta::adding([triple("a", "name", "alpha")]),
    )
    .expect("tick");
    assert_eq!(
        branch_point(&engine, &trunk).expect("reading"),
        None,
        "a trunk holon has no origin to report"
    );
}

#[test]
fn branching_onto_a_registered_id_is_refused() {
    // Silently merging two scenes would be the worst available outcome: it destroys the
    // parent's state and the branch's in one operation, with no error.
    let (mut engine, mut session, trunk) = setup();
    let other = Holon::new(ex("holon/other"));
    registry::register(&mut engine, &other, &mut session).expect("register");

    let error = branch(&mut engine, &trunk, ex("holon/other"), &mut session)
        .expect_err("should refuse an id that is already a holon");
    assert!(
        matches!(error, holos_holon::HolonError::Invalid(_)),
        "got {error:?}"
    );
}

#[test]
fn a_branch_of_an_empty_holon_is_an_empty_branch() {
    // Not an error, and not a holon missing its scene: a holon can legitimately be created
    // before anything is put in it.
    let (mut engine, mut session, trunk) = setup();
    let child = branch(&mut engine, &trunk, ex("holon/feature"), &mut session).expect("branch");
    assert!(scene_values(&engine, &session, &child).is_empty());
    assert_eq!(
        branch_point(&engine, &child)
            .expect("reading")
            .map(|p| p.version),
        Some(0)
    );
}
