//! A holon ticks: the scene changes only if the boundary allows it, and the event log
//! records the attempt either way.
//!
//! This is the walking skeleton of `DESIGN.md` §9, and the thing the rest of the project
//! was built to reach. Nothing else in the workspace does this: a named graph whose
//! invariants are enforced *on the write path* by shapes bound to it, with per-triple
//! provenance recorded through RDF 1.2 reifiers, at a cost proportional to the change.

use holos_engine::Engine;
use holos_holon::{registry, tick, Admission, Delta, Holon};
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use holos_store::{GraphFilter, Store};
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{Literal, NamedNode, Quad, Triple};
use oxrdfio::{RdfFormat, RdfParser};

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

fn holon() -> Holon {
    Holon::new(NamedNode::new_unchecked("urn:holon:people"))
}

/// A boundary requiring every Person to have exactly one name and a plausible age.
const BOUNDARY: &str = r#"
@prefix ex:   <http://example.com/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age  ; sh:maxCount 1 ; sh:datatype xsd:integer ;
                  sh:minInclusive 0 ; sh:maxInclusive 150 ] .
"#;

/// Loads the boundary shapes into the holon's boundary graph.
fn install_boundary(engine: &mut Engine, holon: &Holon) {
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(BOUNDARY.as_bytes()) {
        let mut quad = quad.expect("parse");
        quad.graph_name = holon.boundary.clone().into();
        engine.store_mut().insert(quad.as_ref()).expect("insert");
    }
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

fn person(name: &str, label: &str, age: i32) -> Vec<Triple> {
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
        Triple {
            subject: ex(name).into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal(age.to_string(), xsd::INTEGER).into(),
        },
    ]
}

fn scene_size(engine: &Engine, holon: &Holon) -> usize {
    holos_holon::graph_size(engine.store(), &holon.scene)
}

fn events_size(engine: &Engine, holon: &Holon) -> usize {
    holos_holon::graph_size(engine.store(), &holon.events)
}

/// Sets up an engine with the holon registered and its boundary installed.
fn prepared() -> (Engine, Holon, Session) {
    let mut engine = Engine::new();
    let h = holon();
    install_boundary(&mut engine, &h);
    let mut session = admin_session(engine.store());
    registry::register(&mut engine, &h, &mut session).expect("register");
    (engine, h, session)
}

#[test]
fn a_conforming_tick_commits() {
    let (mut engine, h, mut session) = prepared();

    let outcome = tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice", 30)),
    )
    .expect("tick");

    assert!(outcome.admitted, "a conforming commit must be admitted");
    assert_eq!(outcome.version, 1);
    assert_eq!(outcome.violations, 0);
    assert_eq!(outcome.applied, 3);
    assert_eq!(scene_size(&engine, &h), 3, "the scene holds the new triples");
    assert!(events_size(&engine, &h) > 0, "the tick was recorded");
    assert_eq!(registry::version(&engine, &h).unwrap(), 1);
}

#[test]
fn a_violating_tick_is_rejected_and_the_scene_is_unchanged() {
    let (mut engine, h, mut session) = prepared();

    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice", 30)),
    )
    .expect("first tick");
    let before = scene_size(&engine, &h);

    // 900 is outside the boundary's range.
    let outcome = tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("bob", "Bob", 900)),
    )
    .expect("tick");

    assert!(!outcome.admitted, "the boundary should have refused this");
    assert!(outcome.violations > 0);
    assert_eq!(outcome.applied, 0);
    assert_eq!(
        scene_size(&engine, &h),
        before,
        "a refused commit must leave the scene exactly as it was"
    );
    // And the refusal is on the record, not silently dropped.
    assert!(events_size(&engine, &h) > 0);
}

#[test]
fn admit_and_record_keeps_the_data_and_the_violation() {
    let (mut engine, _, mut session) = prepared();
    let h = holon().with_admission(Admission::AdmitAndRecord);

    let outcome = tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("bob", "Bob", 900)),
    )
    .expect("tick");

    assert!(outcome.admitted, "this policy accepts imperfect data");
    assert!(outcome.violations > 0, "and still records what was wrong");
    assert_eq!(scene_size(&engine, &h), 3);
}

#[test]
fn the_event_log_carries_per_triple_provenance() {
    let (mut engine, h, mut session) = prepared();
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice", 30)),
    )
    .expect("tick");

    // Every change is a reifier pointing at a triple term — the RDF 1.2 shape §9 asks for.
    let store = engine.store();
    let events = store
        .lookup_term(h.events.as_ref().into())
        .unwrap()
        .expect("events graph");
    let reifies = store.lookup_term(rdf::REIFIES.into()).unwrap().unwrap();
    let mut triple_terms = 0;
    for quad in store.quads_for_pattern(None, Some(reifies), None, GraphFilter::Named(events)) {
        let quad = quad.unwrap();
        assert_eq!(
            quad.object.tag(),
            holos_core::Tag::TripleTerm,
            "rdf:reifies must point at a triple term"
        );
        triple_terms += 1;
    }
    assert_eq!(triple_terms, 3, "one reifier per changed triple");
}

#[test]
fn a_removal_is_recorded_and_undone_on_rejection() {
    let (mut engine, h, mut session) = prepared();
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice", 30)),
    )
    .expect("first tick");
    let before = scene_size(&engine, &h);

    // Removing the name breaks sh:minCount 1, so the boundary must refuse and put it back.
    let outcome = tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::default().remove(Triple {
            subject: ex("alice").into(),
            predicate: ex("name"),
            object: Literal::new_simple_literal("Alice").into(),
        }),
    )
    .expect("tick");

    assert!(!outcome.admitted, "removing a required property must fail");
    assert_eq!(
        scene_size(&engine, &h),
        before,
        "the removed triple must be back"
    );
}

#[test]
fn writing_to_a_scene_needs_write_authority() {
    let mut engine = Engine::new();
    let h = holon();
    install_boundary(&mut engine, &h);
    let mut admin = admin_session(engine.store());
    registry::register(&mut engine, &h, &mut admin).expect("register");

    // A principal with read but no write.
    let mut reader = Session::open(
        engine.store(),
        Principal::anonymous(),
        Policy::default().with_rule(Rule::allow(
            Modes::READ,
            Scope::Everything,
            PrincipalMatch::Everyone,
        )),
    )
    .expect("session");

    let result = tick(
        &mut engine,
        &h,
        &mut reader,
        &Delta::adding(person("alice", "Alice", 30)),
    );
    assert!(result.is_err(), "a reader must not be able to commit");
    assert_eq!(scene_size(&engine, &h), 0, "and must change nothing");
}

#[test]
fn registering_a_holon_needs_admin_authority() {
    // §14.7: authority to change the rules is not authority to change the data.
    let mut engine = Engine::new();
    let h = holon();
    let mut writer = Session::open(
        engine.store(),
        Principal::anonymous(),
        Policy::default().with_rule(Rule::allow(
            Modes::READ.union(Modes::WRITE),
            Scope::Everything,
            PrincipalMatch::Everyone,
        )),
    )
    .expect("session");
    assert!(
        registry::register(&mut engine, &h, &mut writer).is_err(),
        "a writer must not be able to define a boundary"
    );
}

#[test]
fn a_holon_round_trips_through_the_system_graph() {
    // §3: holon metadata is plain RDF, so it must be readable back out with no side table.
    let (engine, h, _) = prepared();
    let loaded = registry::load(engine.store(), &h.id)
        .expect("load")
        .expect("the holon is registered");
    assert_eq!(loaded.scene, h.scene);
    assert_eq!(loaded.boundary, h.boundary);
    assert_eq!(loaded.events, h.events);
    assert_eq!(loaded.admission, h.admission);
}

#[test]
fn a_holon_with_no_boundary_constrains_nothing() {
    // A holon can legitimately exist before its shapes do.
    let mut engine = Engine::new();
    let h = Holon::new(NamedNode::new_unchecked("urn:holon:empty"));
    let mut session = admin_session(engine.store());
    registry::register(&mut engine, &h, &mut session).expect("register");

    let outcome = tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("anyone", "Anyone", 900)),
    )
    .expect("tick");
    assert!(outcome.admitted);
    assert_eq!(outcome.violations, 0);
}

#[test]
fn an_unimplemented_projection_regime_is_refused_not_downgraded() {
    // §9 restricts incremental maintenance to a fragment of SPARQL and the machinery is not
    // built. Silently recomputing a projection that asked to be maintained would be a lie
    // about what the system guarantees.
    let mut engine = Engine::new();
    let mut h = Holon::new(NamedNode::new_unchecked("urn:holon:p"))
        .with_projection(ex("names"), "SELECT ?n WHERE { ?s <http://example.com/name> ?n }");
    h.projections[0].regime = holos_holon::Regime::Maintained;
    let mut session = admin_session(engine.store());

    let result = tick(&mut engine, &h, &mut session, &Delta::default());
    assert!(matches!(
        result,
        Err(holos_holon::HolonError::UnsupportedRegime(_))
    ));
}

#[test]
fn versions_advance_one_tick_at_a_time() {
    let (mut engine, h, mut session) = prepared();
    for expected in 1..=3 {
        let outcome = tick(
            &mut engine,
            &h,
            &mut session,
            &Delta::adding(person(&format!("p{expected}"), "Someone", 30)),
        )
        .expect("tick");
        assert_eq!(outcome.version, expected);
        assert_eq!(registry::version(&engine, &h).unwrap(), expected);
    }
}

#[test]
fn a_projection_reads_the_scene() {
    let (mut engine, _, mut session) = prepared();
    let h = holon().with_projection(
        ex("names"),
        "PREFIX ex: <http://example.com/> SELECT ?n WHERE { GRAPH ?g { ?s ex:name ?n } }",
    );
    tick(
        &mut engine,
        &h,
        &mut session,
        &Delta::adding(person("alice", "Alice", 30)),
    )
    .expect("tick");

    let view = engine.view(&session);
    let results = holos_holon::projection(&view, &h, &ex("names"))
        .expect("the projection is registered")
        .expect("it runs");
    match results {
        spareval::QueryResults::Solutions(iter) => {
            assert_eq!(iter.count(), 1, "the projection sees the committed scene");
        }
        _ => panic!("expected solutions"),
    }
}
