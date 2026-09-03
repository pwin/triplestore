//! Incremental revalidation in the adapted engine must not miss a violation.
//!
//! `DESIGN.md` §8 wanted the engine's constraint coverage on the write path, which needs two
//! things: a graph that takes a delta, and a way to decide what the delta made stale. The
//! first is exact by construction — `engine_delta.rs` checks it against a fresh bridge. The
//! second is a judgement, and the way it fails is silent: a dependency the index does not
//! know about is a violation nobody reports.
//!
//! So the property here is the safety direction, the same one the native evaluator is held
//! to. The fixture conforms to begin with, so after a delta the two reports must be *equal*:
//! anything a full run finds, revalidating the change alone must also find.
//!
//! The shapes graph carries a `sh:sparql` constraint deliberately. It is the reason this
//! engine exists, the reason the native evaluator refuses the graph, and the one constraint
//! whose dependencies live inside query text rather than in the shape.

use holos_shacl::engine::EngineRun;
use holos_shacl::incremental::Change;
use holos_shacl::{Options, ShaclError};
use holos_store::{GraphFilter, Store};
use oxrdf::dataset::CanonicalizationAlgorithm;
use oxrdf::{Dataset, GraphName, Quad};
use oxrdfio::{RdfFormat, RdfParser};

/// Everyone conforms to start with, so a delta's effect is the whole of the difference.
const SHAPES_AND_DATA: &str = r#"
@prefix ex:  <http://example.com/> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name  ; sh:minCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age   ; sh:maxCount 1 ; sh:datatype xsd:integer ;
                  sh:minInclusive 0 ; sh:maxInclusive 150 ] ;
    sh:property [ sh:path ( ex:knows ex:name ) ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:nickname ; sh:equals ex:alias ] ;
    sh:sparql [
        sh:message "a person must have an email" ;
        sh:select """SELECT $this WHERE { FILTER NOT EXISTS { $this <http://example.com/email> ?e } }""" ;
    ] .

ex:alice a ex:Person ; ex:name "Alice" ; ex:age 30 ; ex:email "a@example.com" ;
         ex:knows ex:bob ; ex:nickname "Al" ; ex:alias "Al" .
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

/// A report as a canonical, sorted graph so two of them are comparable.
fn canonical(run: &EngineRun, report: &holos_shacl_engine::report::ValidationReport) -> String {
    let graph = run.report_to_oxrdf(report);
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

/// The safety property, over one delta.
///
/// Both reports come from the *same* run, so their terms are interned in one table and the
/// rendered graphs are directly comparable. Two separately prepared runs would intern
/// independently and the comparison would be of labels rather than of meaning.
fn revalidation_finds_what_a_full_run_finds(delta: &str, added: bool, label: &str) {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);

    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    assert!(
        run.would_revalidate_incrementally(),
        "{label}: this fixture's dependencies are all bounded, so the fast path must apply"
    );
    let clean = run.validate().expect("validate");
    assert!(
        run.conforms(&clean),
        "{label}: the fixture must conform before the delta, or the comparison below is \
         between two different questions"
    );

    let changes = apply_to_store(&mut store, delta, added);
    let incremental = run.revalidate(&store, &changes).expect("revalidate");
    let full = run.validate().expect("validate");

    assert!(
        !full.results.is_empty(),
        "{label}: the delta introduced no violation, so this proves nothing"
    );
    assert_eq!(
        canonical(&run, &incremental),
        canonical(&run, &full),
        "{label}: revalidating the change alone missed something a full run finds"
    );
}

#[test]
fn a_missing_name_is_caught() {
    revalidation_finds_what_a_full_run_finds(
        "@prefix ex: <http://example.com/> . ex:carol a ex:Person .",
        true,
        "a new person with nothing",
    );
}

#[test]
fn a_removed_name_is_caught() {
    revalidation_finds_what_a_full_run_finds(
        r#"@prefix ex: <http://example.com/> . ex:bob ex:name "Bob" ."#,
        false,
        "sh:minCount, reached by removal",
    );
}

/// The SPARQL constraint's dependency is inside the query text, and nothing outside the
/// query knows it. If `sparql::predicates` did not walk the algebra, this change would
/// implicate no shape at all and the violation would go unreported.
#[test]
fn a_constraint_that_only_a_sparql_query_reads_is_caught() {
    revalidation_finds_what_a_full_run_finds(
        r#"@prefix ex: <http://example.com/> . ex:alice ex:email "a@example.com" ."#,
        false,
        "the SPARQL constraint's own predicate",
    );
}

/// The changed quad's subject is not the focus node: `ex:bob` is reached from `ex:alice`
/// down `( ex:knows ex:name )`. Attributing the work to the shape and then widening it back
/// to its focus nodes is what makes this visible.
#[test]
fn a_violation_reached_down_a_sequence_path_is_caught() {
    revalidation_finds_what_a_full_run_finds(
        "@prefix ex: <http://example.com/> . ex:bob <http://example.com/name> 7 .",
        true,
        "a value two hops from the focus node",
    );
}

/// `sh:equals ex:alias` reads a predicate no path mentions, so the only route from a write
/// to `ex:alias` to this shape is the constraint's own dependency.
#[test]
fn a_property_pair_constraints_sibling_is_caught() {
    revalidation_finds_what_a_full_run_finds(
        r#"@prefix ex: <http://example.com/> . ex:alice ex:alias "Al" ."#,
        false,
        "the sibling of a property-pair constraint",
    );
}

/// A new `rdf:type` pulls a node into a shape's scope without touching anything the shape's
/// own constraints read.
#[test]
fn a_newly_typed_node_becomes_a_focus_node() {
    revalidation_finds_what_a_full_run_finds(
        "@prefix ex: <http://example.com/> . ex:dave a ex:Person .",
        true,
        "a class target reached by a type change",
    );
}

/// A shape whose reads cannot be bounded must make the whole run fall back rather than check
/// less. `?p` as a predicate variable is the plainest way to say "reads everything".
#[test]
fn an_unbounded_constraint_forces_a_full_run() {
    let mut store = Store::new();
    load(
        &mut store,
        r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:S a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:sparql [
        sh:message "everything about this node matters" ;
        sh:select """SELECT $this WHERE { $this ?p "forbidden" }""" ;
    ] .

ex:alice a ex:Person .
"#,
    );
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    assert!(
        !run.would_revalidate_incrementally(),
        "a variable predicate cannot be indexed, and pretending otherwise loses violations"
    );

    // The write names a predicate no index entry could have mentioned.
    let changes = apply_to_store(
        &mut store,
        r#"@prefix ex: <http://example.com/> . ex:alice <http://example.com/anything> "forbidden" ."#,
        true,
    );
    let incremental = run.revalidate(&store, &changes).expect("revalidate");
    assert!(
        !run.conforms(&incremental),
        "the fallback must find what an index could not have pointed at"
    );
}

/// A change to a shape definition is refused here as it is by `apply`, rather than being
/// planned against shapes that no longer describe the graph.
#[test]
fn a_shape_change_is_refused_rather_than_planned_around() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let mut run = EngineRun::prepare(&store, options()).expect("prepare");
    let changes = apply_to_store(
        &mut store,
        r#"@prefix ex: <http://example.com/> .
           @prefix sh: <http://www.w3.org/ns/shacl#> .
           ex:PersonShape sh:targetClass ex:Robot ."#,
        true,
    );
    assert!(matches!(
        run.revalidate(&store, &changes),
        Err(ShaclError::Unsupported(_))
    ));
}
