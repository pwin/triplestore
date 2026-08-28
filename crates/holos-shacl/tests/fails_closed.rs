//! The write path must not answer *conforms* for a constraint it cannot check.
//!
//! `DESIGN.md` §9 makes [`CompiledShapes`] the validator that gates a commit, and §8 records
//! that it covers SHACL Core while `engine::EngineRun` covers more. The dangerous reading of
//! that sentence is "it checks less"; the true one was "it checks less and says nothing".
//! A shapes graph carrying `sh:sparql` compiled without complaint, the constraint was
//! dropped, and validation returned `conforms = true` for data a full run rejects.
//!
//! A gate that fails open is worse than no gate, because it is trusted. These tests pin the
//! refusal.

use holos_shacl::{CompiledShapes, Options, ShaclError};
use holos_store::{GraphFilter, Store};
use oxrdfio::{RdfFormat, RdfParser};

fn load(turtle: &str) -> Store {
    let mut store = Store::new();
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(turtle.as_bytes()) {
        store.insert(quad.expect("parse").as_ref()).expect("insert");
    }
    store
}

fn options() -> Options {
    Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    }
}

const SPARQL_CONSTRAINT: &str = r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:sparql [
        sh:message "a person must have a name" ;
        sh:select """SELECT $this WHERE { FILTER NOT EXISTS { $this <http://example.com/name> ?n } }""" ;
    ] .

ex:bob a ex:Person .
"#;

#[test]
fn a_sparql_constraint_is_refused_rather_than_dropped() {
    let store = load(SPARQL_CONSTRAINT);
    match CompiledShapes::compile(&store, options()) {
        Err(ShaclError::Unsupported(what)) => {
            assert!(
                what.contains("sh:sparql"),
                "the refusal should name the construct, got: {what}"
            );
        }
        Err(e) => panic!("expected Unsupported, got {e}"),
        Ok(shapes) => {
            let report = shapes.validate(&store).expect("validate");
            panic!(
                "the write path compiled a sh:sparql shape and reported conforms={}: a \
                 constraint it cannot evaluate was silently dropped",
                report.conforms
            );
        }
    }
}

/// The adapted engine does implement it, and this is what makes the refusal a redirection
/// rather than a dead end.
#[test]
fn the_adapted_engine_catches_what_the_write_path_refuses() {
    let store = load(SPARQL_CONSTRAINT);
    let mut run = holos_shacl::engine::EngineRun::prepare(&store, options()).expect("prepare");
    let report = run.validate().expect("validate");
    assert!(
        !run.conforms(&report),
        "ex:bob has no name and the SPARQL constraint says that is a violation"
    );
}

/// The refusal must not be so eager that ordinary shapes stop compiling. Presentation
/// properties carry no validation meaning, so ignoring them ignores nothing.
#[test]
fn presentation_properties_do_not_trip_the_refusal() {
    let store = load(
        r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [
        sh:path ex:name ;
        sh:name "name" ;
        sh:description "what the person is called" ;
        sh:order 1 ;
        sh:minCount 1 ;
        sh:datatype xsd:string ;
    ] .

ex:bob a ex:Person .
"#,
    );
    let shapes = CompiledShapes::compile(&store, options()).expect("compiles");
    let report = shapes.validate(&store).expect("validate");
    assert!(!report.conforms, "ex:bob has no name, so sh:minCount fails");
}

/// A SHACL-AF rule changes what the data *is*, so a validator that ignores it validates a
/// graph nobody has yet.
#[test]
fn a_shacl_af_rule_is_refused() {
    let store = load(
        r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:rule [
        a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:kind ;
        sh:object "person" ;
    ] .

ex:bob a ex:Person .
"#,
    );
    assert!(
        matches!(
            CompiledShapes::compile(&store, options()),
            Err(ShaclError::Unsupported(_))
        ),
        "sh:rule is not implemented here and must not be ignored"
    );
}
