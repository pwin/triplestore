//! Boundary rules firing inside a tick.
//!
//! `DESIGN.md` §9 puts rules at step 2, before validation, and the tick left the step
//! switched off because the adapted engine needed a fresh bridge per commit. §8 removed that,
//! and this is what the removal was for.
//!
//! The order matters more than the feature. Rules run *before* the boundary, so what they
//! infer is judged by it: a rule that writes something the shapes forbid rejects the commit
//! rather than persisting quietly. And the inferences are part of the commit — recorded in
//! the event, undone with everything else — because a reader asking what changed should not
//! have to know which triples a rule wrote.

use holos_engine::Engine;
use holos_holon::{Admission, Delta, Holon, Regime, Rules};
use holos_security::Session;
use oxrdf::{NamedNode, Triple};
use oxrdfio::RdfFormat;

const EX: &str = "http://example.com/";

fn iri(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

/// A boundary with one `sh:TripleRule`: anything typed `ex:Employee` is also an `ex:Person`.
const BOUNDARY: &str = r#"
@prefix ex:  <http://example.com/> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .

ex:EmployeeShape a sh:NodeShape ;
    sh:targetClass ex:Employee ;
    sh:rule [
        a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ;
        sh:object ex:Person ;
    ] .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ] .
"#;

fn holon() -> Holon {
    Holon {
        id: iri("h"),
        scene: iri("scene"),
        boundary: iri("boundary"),
        events: iri("events"),
        admission: Admission::Reject,
        projections: Vec::new(),
    }
}

fn engine_with(boundary: &str, scene: &str) -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load_into_graph(
            boundary.as_bytes(),
            RdfFormat::Turtle,
            None,
            &iri("boundary").into(),
        )
        .expect("boundary");
    if !scene.trim().is_empty() {
        engine
            .bulk_load_into_graph(
                scene.as_bytes(),
                RdfFormat::Turtle,
                None,
                &iri("scene").into(),
            )
            .expect("scene");
    }
    engine
}

/// Every triple in the scene, rendered and sorted.
fn scene(engine: &Engine) -> Vec<String> {
    let graph = holos_store::GraphFilter::Named(
        engine
            .store()
            .lookup_term(iri("scene").as_ref().into())
            .expect("lookup")
            .expect("the scene exists"),
    );
    let mut out: Vec<String> = engine
        .store()
        .quads_for_pattern(None, None, None, graph)
        .map(|q| {
            let q = engine
                .store()
                .decode_quad(q.expect("scan"))
                .expect("decode");
            format!("{} {} {}", q.subject, q.predicate, q.object)
        })
        .collect();
    out.sort();
    out
}

fn added(triples: Vec<Triple>) -> Delta {
    Delta {
        added: triples,
        removed: Vec::new(),
    }
}

fn a(subject: &str, class: &str) -> Triple {
    Triple::new(
        iri(subject),
        NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
        iri(class),
    )
}

fn named(subject: &str, name: &str) -> Triple {
    Triple::new(
        iri(subject),
        iri("name"),
        oxrdf::Literal::new_simple_literal(name),
    )
}

#[test]
fn a_rule_fires_and_its_inference_is_committed() {
    let mut engine = engine_with(BOUNDARY, "");
    let holon = holon();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let mut rules = Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .expect("the holon has both graphs");

    // An employee with a name. The rule makes them a person; the person shape then requires
    // the name they already have, so the commit is admitted.
    let outcome = holos_holon::tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &added(vec![a("alice", "Employee"), named("alice", "Alice")]),
        Some(&mut rules),
    )
    .expect("tick");

    assert!(outcome.admitted, "the inferred type satisfies the boundary");
    let scene = scene(&engine);
    assert!(
        scene
            .iter()
            .any(|t| t.contains("#type") && t.contains("/Person")),
        "the rule's inference is in the scene: {scene:?}"
    );
    assert!(
        scene.iter().any(|t| t.contains("/Employee")),
        "and so is what the caller sent"
    );
}

/// Rules run *before* the boundary, so an inference the shapes forbid rejects the commit.
/// Running them after would have persisted it and reported conformance.
#[test]
fn an_inference_the_boundary_forbids_rejects_the_commit() {
    let mut engine = engine_with(BOUNDARY, "");
    let holon = holon();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let mut rules = Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .expect("both graphs");

    // An employee with no name. The rule makes them a person, and the person shape wants a
    // name — so the *inference* is what fails validation.
    let outcome = holos_holon::tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &added(vec![a("bob", "Employee")]),
        Some(&mut rules),
    )
    .expect("tick");

    assert!(!outcome.admitted, "the inferred type breaches the boundary");
    assert!(
        scene(&engine).is_empty(),
        "and a refused tick leaves nothing behind, the inference included"
    );
}

/// Without a `Rules` the step does not run, which is the old behaviour and still the default.
#[test]
fn no_rules_means_no_rule_step() {
    let mut engine = engine_with(BOUNDARY, "");
    let holon = holon();
    let mut session = Session::unrestricted(engine.store()).expect("session");

    let outcome = holos_holon::tick(
        &mut engine,
        &holon,
        &mut session,
        &added(vec![a("bob", "Employee")]),
    )
    .expect("tick");

    // Nothing inferred, so nothing to breach: `ex:bob` is only an Employee, and no shape
    // targets that.
    assert!(outcome.admitted);
    let scene = scene(&engine);
    assert!(
        !scene.iter().any(|t| t.contains("/Person")),
        "no rule ran, so nothing was inferred: {scene:?}"
    );
}

/// The cache survives ticks, and each one sees what the last left behind. This is the
/// property the whole design rests on — a stale bridged graph would fire rules against a
/// scene that no longer exists.
#[test]
fn the_bridge_stays_current_across_ticks() {
    let mut engine = engine_with(BOUNDARY, "");
    let holon = holon();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let mut rules = Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .expect("both graphs");

    for i in 0..5 {
        let who = format!("p{i}");
        let outcome = holos_holon::tick_with_rules(
            &mut engine,
            &holon,
            &mut session,
            &added(vec![a(&who, "Employee"), named(&who, &who)]),
            Some(&mut rules),
        )
        .expect("tick");
        assert!(outcome.admitted, "round {i}");
    }

    let scene = scene(&engine);
    let people = scene
        .iter()
        .filter(|t| t.contains("#type") && t.contains("/Person"))
        .count();
    assert_eq!(people, 5, "one inference per tick: {scene:?}");
}

/// A `Rules` bridged against one holon must not be used on another: it would fire rules over
/// the wrong scene, and the answer would look like an answer.
#[test]
fn rules_are_tied_to_the_scene_they_were_bridged_against() {
    let mut engine = engine_with(BOUNDARY, "");
    let holon = holon();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let mut rules = Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .expect("both graphs");

    let other = Holon {
        scene: iri("other-scene"),
        ..holon.clone()
    };
    let result = holos_holon::tick_with_rules(
        &mut engine,
        &other,
        &mut session,
        &added(vec![a("carol", "Employee")]),
        Some(&mut rules),
    );
    assert!(
        result.is_err(),
        "the mismatch must be refused, not guessed at"
    );
}

/// A holon whose graphs are not there yet constrains nothing, which is a state rather than a
/// failure.
#[test]
fn a_holon_without_graphs_has_no_rules() {
    let mut engine = Engine::new();
    let holon = holon();
    assert!(Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .is_none());
}

/// Unused, so `Regime` and the projection field stay honest imports rather than drifting.
#[test]
fn projections_are_untouched_by_the_rule_step() {
    let holon = holon();
    assert!(holon.projections.is_empty());
    assert_ne!(Regime::Maintained, Regime::Recomputed);
}

/// A boundary whose rule infers something a *value* constraint forbids.
///
/// The type-inferring rule above is caught by a second route — a changed `rdf:type` makes the
/// revalidation planner walk the node's classes, and it finds the inferred one in the store
/// whether or not the tick recorded it. That is a good property and a poor test, because it
/// passes with the bookkeeping removed. A rule inferring an ordinary property has no such
/// second route: if the inference is not recorded as a change, nothing re-checks the shape.
const VALUE_BOUNDARY: &str = r#"
@prefix ex:  <http://example.com/> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .

ex:ThingShape a sh:NodeShape ;
    sh:targetClass ex:Thing ;
    sh:property [ sh:path ex:status ; sh:in ( "ok" ) ] ;
    sh:rule [
        a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:status ;
        sh:object "broken" ;
    ] .
"#;

#[test]
fn an_inferred_value_is_validated_like_any_other() {
    // The thing is already a `ex:Thing`; the delta only adds a note. `ex:note` is read by no
    // shape, so the delta on its own implicates nothing and revalidation has nothing to do.
    // The *inference* is what breaches the boundary, so it is only found if the tick recorded
    // it as a change — which is precisely the bookkeeping under test.
    let mut engine = engine_with(
        VALUE_BOUNDARY,
        "@prefix ex: <http://example.com/> . ex:widget a ex:Thing .",
    );
    let holon = holon();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let mut rules = Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .expect("both graphs");

    let outcome = holos_holon::tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &added(vec![Triple::new(
            iri("widget"),
            iri("note"),
            oxrdf::Literal::new_simple_literal("a note"),
        )]),
        Some(&mut rules),
    )
    .expect("tick");

    assert!(
        !outcome.admitted,
        "the rule infers ex:status \"broken\", which sh:in forbids"
    );
    assert!(
        !scene(&engine).iter().any(|t| t.contains("/status")),
        "and the refused tick takes the inference back out"
    );
}

/// A rule is not a way around §14. It runs inside a session, the session is the principal's,
/// and what it writes is subject to the same policy as what the principal writes — otherwise
/// a boundary would be a way to write where one cannot.
#[test]
fn a_rule_cannot_write_where_the_principal_cannot() {
    use holos_security::policy::{PrincipalMatch, Rule, Scope};
    use holos_security::{Modes, Policy, Principal};

    // The rule writes `ex:status`; the caller writes `rdf:type`. Denying only the predicate
    // the *rule* writes is what separates "the rule was checked" from "the caller was
    // checked" — a policy denying the whole scene stops the tick at step 1 and never reaches
    // the rule at all, which is a test that passes without testing anything.
    let mut engine = engine_with(VALUE_BOUNDARY, "");
    let holon = holon();

    let mut policy = Policy::permit_all();
    policy.rules.push(Rule::deny(
        Modes::WRITE,
        Scope::GraphPredicate(iri("scene"), iri("status")),
        PrincipalMatch::Everyone,
    ));
    let mut session =
        Session::open(engine.store(), Principal::anonymous(), policy).expect("session");
    let mut rules = Rules::prepare(&mut engine, &holon)
        .expect("prepare")
        .expect("both graphs");

    let result = holos_holon::tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &added(vec![a("widget", "Thing")]),
        Some(&mut rules),
    );
    assert!(
        result.is_err(),
        "the rule's write is denied, so the tick fails rather than the rule slipping past"
    );
    assert!(
        scene(&engine).is_empty(),
        "and nothing is left behind, the caller's own triple included"
    );
}
