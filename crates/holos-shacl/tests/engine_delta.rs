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

// ------------------------------------------------------------------ checkpoint and revert

/// The bridge tracks what it was told, not the store — so a caller must be able to untell it.
///
/// This is the trap `apply` documents. A caller applies a delta, something goes wrong, the
/// store goes back, and the run is left holding triples that no longer exist. Nothing here
/// can notice: not going and looking is the entire point of applying a delta.
///
/// The property is the same one the rest of this file checks, asked of the undo: after
/// `checkpoint`, `apply`, `revert`, the run must report exactly what a freshly prepared one
/// reports over the store as it now stands.
#[test]
fn reverting_a_checkpoint_matches_a_fresh_bridge() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);

    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let before = canonical(&mut run);

    // A delta that moves the verdict, so a revert that did nothing would be caught.
    let bad = "@prefix ex: <http://example.com/> . ex:carol a ex:Person ; ex:age 900 .";
    run.checkpoint();
    let changes = apply_to_store(&mut store, bad, true);
    run.apply(&store, &changes).expect("apply");
    assert_ne!(
        canonical(&mut run),
        before,
        "the delta must change the report"
    );

    // The store goes back, as it would after a refused commit, and so must the run.
    for change in &changes {
        store.remove_encoded_quad(change.quad).expect("remove");
    }
    run.revert(&store);

    assert!(!run.is_stale());
    assert_eq!(
        canonical(&mut run),
        before,
        "revert did not restore the graph"
    );

    let mut fresh = EngineRun::prepare(&store, options()).expect("prepare");
    assert_eq!(
        canonical(&mut run),
        canonical(&mut fresh),
        "a reverted run disagrees with a bridge built from the store"
    );
}

/// Accepting is the other half, and it must not undo anything.
///
/// Worth its own test because `revert` doing nothing would pass a test that only checked
/// `accept`, and `accept` behaving like `revert` would pass a test that only checked the
/// happy path of a tick.
#[test]
fn accepting_a_checkpoint_keeps_the_delta() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);

    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let before = canonical(&mut run);

    let bad = "@prefix ex: <http://example.com/> . ex:carol a ex:Person ; ex:age 900 .";
    run.checkpoint();
    let changes = apply_to_store(&mut store, bad, true);
    run.apply(&store, &changes).expect("apply");
    run.accept();

    let after = canonical(&mut run);
    assert_ne!(after, before);

    let mut fresh = EngineRun::prepare(&store, options()).expect("prepare");
    assert_eq!(after, canonical(&mut fresh), "accept lost the delta");

    // Accepting must *close* the checkpoint, not just decline to undo. Applying more
    // afterwards and then reverting proves it: with the checkpoint still open, this second
    // change would be swept up and undone by a revert nobody asked for.
    let more = apply_to_store(
        &mut store,
        "@prefix ex: <http://example.com/> . ex:dave a ex:Person ; ex:name \"Dave\" .",
        true,
    );
    run.apply(&store, &more).expect("apply");
    let with_dave = canonical(&mut run);
    assert_ne!(with_dave, after);

    run.revert(&store);
    assert_eq!(
        canonical(&mut run),
        with_dave,
        "revert undid a change made after the checkpoint had been accepted"
    );
}

// ------------------------------------------------------------------------- ordered deltas

/// A delta is an ordered sequence, and a row touched twice in one must end where the
/// sequence leaves it.
///
/// The graph underneath takes two unordered sets — remove all of these, then add all of
/// those — so partitioning a sequence into them loses the order, and a row that appears in
/// both lists comes out present whichever way round the caller meant it. A holon delta that
/// adds a triple and removes it again arrives here as exactly that pair.
#[test]
fn a_row_added_and_then_removed_in_one_delta_ends_absent() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let before = canonical(&mut run);

    // Added and removed again, in that order, in one delta. The store never keeps it, so a
    // fresh bridge is the `before` graph and the run must agree.
    let carol = "@prefix ex: <http://example.com/> . ex:carol a ex:Person ; ex:age 900 .";
    let mut changes = apply_to_store(&mut store, carol, true);
    changes.extend(apply_to_store(&mut store, carol, false));
    run.apply(&store, &changes).expect("apply");

    assert_eq!(
        canonical(&mut run),
        before,
        "a row added and removed in one delta was left in the graph"
    );
    let mut fresh = EngineRun::prepare(&store, options()).expect("prepare");
    assert_eq!(canonical(&mut run), canonical(&mut fresh));
}

/// And the other way round: removed then added again must end present.
#[test]
fn a_row_removed_and_then_added_in_one_delta_ends_present() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let before = canonical(&mut run);

    let alices_name = "@prefix ex: <http://example.com/> . ex:alice ex:name \"Alice\" .";
    let mut changes = apply_to_store(&mut store, alices_name, false);
    changes.extend(apply_to_store(&mut store, alices_name, true));
    run.apply(&store, &changes).expect("apply");

    assert_eq!(canonical(&mut run), before);
    let mut fresh = EngineRun::prepare(&store, options()).expect("prepare");
    assert_eq!(canonical(&mut run), canonical(&mut fresh));
}

/// Reverting a checkpoint that touched one row twice must restore what it started as, not
/// what it last was.
///
/// This is what makes the reversal in `revert` more than decoration: undoing the *last*
/// change to a row leaves it wherever the middle of the sequence put it.
#[test]
fn reverting_restores_a_row_touched_twice() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let before = canonical(&mut run);

    // Alice's name goes away and comes back, inside the checkpoint. The row is present at
    // the checkpoint and present at the end, so undoing only the last change would remove
    // it — and the report would then complain that Alice has no name.
    let alices_name = "@prefix ex: <http://example.com/> . ex:alice ex:name \"Alice\" .";
    run.checkpoint();
    let mut changes = apply_to_store(&mut store, alices_name, false);
    changes.extend(apply_to_store(&mut store, alices_name, true));
    run.apply(&store, &changes).expect("apply");
    run.revert(&store);

    assert_eq!(
        canonical(&mut run),
        before,
        "revert left a row where the middle of the delta put it"
    );
    let mut fresh = EngineRun::prepare(&store, options()).expect("prepare");
    assert_eq!(canonical(&mut run), canonical(&mut fresh));
}

/// Without a checkpoint, nothing is recorded — so a run that never asks pays no memory for
/// a log it will not use, and `revert` has nothing to undo.
#[test]
fn a_revert_without_a_checkpoint_does_nothing() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);

    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let changes = apply_to_store(
        &mut store,
        "@prefix ex: <http://example.com/> . ex:carol a ex:Person ; ex:age 900 .",
        true,
    );
    run.apply(&store, &changes).expect("apply");
    let after = canonical(&mut run);

    run.revert(&store);
    assert_eq!(
        canonical(&mut run),
        after,
        "revert undid a change no checkpoint was open for"
    );
}
