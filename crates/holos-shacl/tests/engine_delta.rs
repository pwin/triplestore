//! Applying a delta must be indistinguishable from bridging the store again.
//!
//! `DESIGN.md` §8 said the adapted engine could not gate a write path because its graph was
//! immutable: every commit meant re-bridging the whole store, which at 250,000 quads cost
//! 198 ms and grew with the store rather than with the change. `EngineRun::apply` takes the
//! delta instead.
//!
//! That is only worth having if it is *exact*. A validator that is fast and slightly out of
//! date is worse than a slow one, because the answer it gives is trusted. So the property
//! checked here is equality with the thing it replaces: after any delta, the incrementally
//! updated run must report what a freshly prepared one reports — same results, same graph,
//! up to blank-node isomorphism.

use holos_shacl::engine::EngineRun;
use holos_shacl::incremental::Change;
use holos_shacl::{Options, ShaclError};
use holos_store::{GraphFilter, Store};
use oxrdf::dataset::CanonicalizationAlgorithm;
use oxrdf::{Dataset, GraphName, NamedNode, Quad};
use oxrdfio::{RdfFormat, RdfParser};

const SHAPES_AND_DATA: &str = r#"
@prefix ex:  <http://example.com/> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name  ; sh:minCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age   ; sh:maxCount 1 ; sh:datatype xsd:integer ;
                  sh:minInclusive 0 ; sh:maxInclusive 150 ] ;
    sh:property [ sh:path ex:knows ; sh:nodeKind sh:IRI ] ;
    sh:sparql [
        sh:message "a person must have an email" ;
        sh:select """SELECT $this WHERE { FILTER NOT EXISTS { $this <http://example.com/email> ?e } }""" ;
    ] .

ex:alice a ex:Person ; ex:name "Alice" ; ex:age 30 ; ex:email "a@example.com" .
ex:bob   a ex:Person ; ex:name "Bob"   ; ex:age 41 ; ex:email "b@example.com" .
"#;

fn options() -> Options {
    Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    }
}

fn load(store: &mut Store, turtle: &str) {
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(turtle.as_bytes()) {
        store.insert(quad.expect("parse").as_ref()).expect("insert");
    }
}

/// A report as a canonical graph, so two runs are comparable whatever order they produced
/// their results in and whatever blank-node labels they chose.
fn canonical(run: &mut EngineRun) -> String {
    let report = run.validate().expect("validate");
    let graph = run.report_to_oxrdf(&report);
    let mut dataset = Dataset::new();
    for triple in graph.iter() {
        dataset.insert(&Quad {
            subject: triple.subject.into_owned(),
            predicate: triple.predicate.into_owned(),
            object: triple.object.into_owned(),
            graph_name: GraphName::DefaultGraph,
        });
    }
    dataset.canonicalize(CanonicalizationAlgorithm::Unstable);
    let mut lines: Vec<String> = dataset.iter().map(|q| q.to_string()).collect();
    lines.sort();
    lines.join("\n")
}

/// Applies `turtle` to a store as a delta, and returns the changes that describe it.
fn apply_to_store(store: &mut Store, turtle: &str, added: bool) -> Vec<Change> {
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    let mut changes = Vec::new();
    for quad in parser.for_reader(turtle.as_bytes()) {
        let quad = quad.expect("parse");
        let encoded = store.encode_quad(quad.as_ref()).expect("encode");
        if added {
            store.insert_encoded(encoded).expect("insert");
            changes.push(Change::added(encoded));
        } else {
            store.remove_encoded_quad(encoded).expect("remove");
            changes.push(Change::removed(encoded));
        }
    }
    changes
}

/// The property, over one delta.
///
/// `changes_the_verdict` says whether the report itself should move. It usually does, and
/// where it does not — a conforming addition leaves an empty report empty — report equality
/// on its own would hold even if the delta had been dropped entirely. The triple count is
/// what proves it landed, so both are checked and the weaker case says which it is.
fn agrees_with_a_fresh_bridge(delta: &str, added: bool, changes_the_verdict: bool, label: &str) {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);

    let mut incremental = EngineRun::prepare(&store, options()).expect("prepare");
    let before = canonical(&mut incremental);

    let changes = apply_to_store(&mut store, delta, added);
    incremental
        .apply(&store, &changes)
        .expect("apply the delta");

    let mut fresh = EngineRun::prepare(&store, options()).expect("re-prepare");
    assert_eq!(
        incremental.triples(),
        fresh.triples(),
        "{label}: the delta did not land in the bridged graph"
    );
    assert_eq!(
        canonical(&mut incremental),
        canonical(&mut fresh),
        "{label}: the incrementally updated run disagrees with a fresh bridge"
    );
    if changes_the_verdict {
        assert_ne!(
            before,
            canonical(&mut fresh),
            "{label}: the report did not move, so report equality proves nothing here"
        );
    }
}

#[test]
fn a_new_violating_node_is_seen() {
    agrees_with_a_fresh_bridge(
        "@prefix ex: <http://example.com/> . ex:carol a ex:Person .",
        true,
        true,
        "a person with no name, age or email",
    );
}

#[test]
fn a_new_conforming_node_is_seen() {
    agrees_with_a_fresh_bridge(
        r#"@prefix ex: <http://example.com/> .
           ex:dave a ex:Person ; ex:name "Dave" ; ex:email "d@example.com" ."#,
        true,
        false,
        "a person who conforms",
    );
}

#[test]
fn a_removal_is_seen() {
    // Taking the email away makes `ex:alice` fail the SPARQL constraint — which is the
    // constraint the native evaluator refuses, and the reason this engine exists.
    agrees_with_a_fresh_bridge(
        r#"@prefix ex: <http://example.com/> . ex:alice ex:email "a@example.com" ."#,
        false,
        true,
        "a removal that introduces a violation",
    );
}

#[test]
fn a_value_out_of_range_is_seen() {
    agrees_with_a_fresh_bridge(
        "@prefix ex: <http://example.com/> . ex:bob <http://example.com/age> 999 .",
        true,
        true,
        "a second age, out of range",
    );
}

/// Terms the bridge never saw have to be interned as they arrive, or the change is dropped.
#[test]
fn a_delta_naming_entirely_new_terms_is_applied() {
    agrees_with_a_fresh_bridge(
        r#"@prefix ex: <http://example.com/> .
           ex:erin a ex:Person ; ex:name "Erin" ; ex:knows "not an iri" ."#,
        true,
        true,
        "new subject, new value, new literal",
    );
}

/// A long run of deltas, so an error that only shows up after several applications has
/// somewhere to appear. Re-preparing after each one would hide exactly that.
#[test]
fn the_graph_stays_exact_across_many_deltas() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut incremental = EngineRun::prepare(&store, options()).expect("prepare");

    for i in 0..25 {
        let add = format!(
            "@prefix ex: <http://example.com/> .\n\
             ex:p{i} a ex:Person ; ex:name \"P{i}\" ; ex:age {} .\n",
            i * 7
        );
        let changes = apply_to_store(&mut store, &add, true);
        incremental.apply(&store, &changes).expect("apply");

        if i % 3 == 0 {
            let drop = format!(
                "@prefix ex: <http://example.com/> . ex:p{i} <http://example.com/name> \"P{i}\" .\n"
            );
            let changes = apply_to_store(&mut store, &drop, false);
            incremental.apply(&store, &changes).expect("apply");
        }

        let mut fresh = EngineRun::prepare(&store, options()).expect("re-prepare");
        assert_eq!(
            canonical(&mut incremental),
            canonical(&mut fresh),
            "diverged at round {i}"
        );
    }
}

/// Changes outside the data graph are not this run's business.
#[test]
fn a_change_in_another_graph_is_ignored() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let before = run.triples();

    let elsewhere = Quad {
        subject: NamedNode::new_unchecked("http://example.com/zoe").into(),
        predicate: NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
        object: NamedNode::new_unchecked("http://example.com/Person").into(),
        graph_name: GraphName::NamedNode(NamedNode::new_unchecked("http://example.com/other")),
    };
    let encoded = store.encode_quad(elsewhere.as_ref()).expect("encode");
    store.insert_encoded(encoded).expect("insert");

    run.apply(&store, &[Change::added(encoded)]).expect("apply");
    assert_eq!(
        run.triples(),
        before,
        "a quad in a named graph is not in the default data graph"
    );
}

/// A changed shapes graph cannot be absorbed, and saying so is the point.
#[test]
fn a_change_to_the_shapes_graph_is_refused() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");

    // Same graph filter for data and shapes in this fixture, so any default-graph change
    // reaches the shapes. That is the case that must not be silently absorbed: the shapes
    // are compiled once, and new data validated against stale shapes is the failure mode
    // this whole exercise exists to avoid.
    let changes = apply_to_store(
        &mut store,
        r#"@prefix ex: <http://example.com/> .
           @prefix sh: <http://www.w3.org/ns/shacl#> .
           ex:PersonShape sh:targetClass ex:Robot ."#,
        true,
    );
    assert!(
        matches!(run.apply(&store, &changes), Err(ShaclError::Unsupported(_))),
        "a shapes change must be refused rather than half-applied"
    );
}
