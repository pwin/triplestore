//! RDFS entailment, materialised.
//!
//! The load-bearing test is the last one: the OGC GeoSPARQL example uses
//! `rdfs:subPropertyOf` to attach geometries, and without entailment a feature-level
//! topology query returns the geometries rather than the features. That is the gap this
//! exists to close, so it is checked end to end through a real query rather than by counting
//! triples.

use holos_core::TermId;
use holos_engine::entailment::{self, Entailed};
use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use spareval::QueryResults;

const EX: &str = "http://example.com/";

fn entailed_graph(engine: &mut Engine) -> TermId {
    engine
        .store_mut()
        .encode_quad(
            oxrdf::Quad {
                subject: oxrdf::NamedNode::new_unchecked(format!("{EX}marker")).into(),
                predicate: oxrdf::NamedNode::new_unchecked(format!("{EX}marker")),
                object: oxrdf::Term::NamedNode(oxrdf::NamedNode::new_unchecked(
                    entailment::DEFAULT_GRAPH_IRI,
                )),
                graph_name: oxrdf::GraphName::DefaultGraph,
            }
            .as_ref(),
        )
        .expect("encode")
        .object
}

fn load(turtle: &str) -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(turtle.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

fn run(engine: &mut Engine) -> Entailed {
    let graph = entailed_graph(engine);
    let mut session = Session::unrestricted(engine.store()).expect("session");
    entailment::materialise(
        engine,
        &mut session,
        Some(graph),
        entailment::DEFAULT_BUDGET,
    )
    .expect("materialise")
}

/// Rows of a query, with the entailed graph folded into the default one.
fn ask(engine: &Engine, sparql: &str) -> Vec<String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    // Both graphs, named explicitly. `union_default_graph` would be the wrong tool: it is
    // the union of the *named* graphs and leaves the store's own default graph out, so a
    // query written that way sees the entailment and not the data it was drawn from.
    let options = QueryOptions::new()
        .with_default_graph(oxrdf::GraphName::DefaultGraph)
        .with_default_graph(oxrdf::GraphName::NamedNode(
            oxrdf::NamedNode::new_unchecked(entailment::DEFAULT_GRAPH_IRI),
        ));
    let (results, _) = Engine::query_with(&view, sparql, &options).expect("query");
    let mut out: Vec<String> = match results {
        QueryResults::Solutions(iter) => iter
            .map(|s| {
                let s = s.expect("solution");
                let mut cells: Vec<String> = s
                    .iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect();
                cells.sort();
                cells.join(" ")
            })
            .collect(),
        _ => panic!("expected solutions"),
    };
    out.sort();
    out
}

#[test]
fn rdfs7_carries_a_sub_property_up_to_its_super() {
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:father rdfs:subPropertyOf ex:parent .\n\
         ex:alice ex:father ex:bob .\n"
    ));
    assert!(ask(
        &engine,
        &format!("SELECT ?x WHERE {{ ?x <{EX}parent> ?y }}")
    )
    .is_empty());
    let report = run(&mut engine);
    assert!(report.added > 0);
    assert_eq!(
        ask(
            &engine,
            &format!("SELECT ?x WHERE {{ ?x <{EX}parent> ?y }}")
        ),
        vec![format!("x=<{EX}alice>")]
    );
}

#[test]
fn rdfs9_carries_a_type_up_a_class_hierarchy() {
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:Dog rdfs:subClassOf ex:Mammal .\n\
         ex:Mammal rdfs:subClassOf ex:Animal .\n\
         ex:rex a ex:Dog .\n"
    ));
    run(&mut engine);
    // Transitive: the chain is two long, so `ex:Animal` only appears if rdfs11 ran before
    // rdfs9 rather than a single hop being applied once.
    assert_eq!(
        ask(&engine, &format!("SELECT ?c WHERE {{ <{EX}rex> a ?c }}")).len(),
        3,
        "Dog, Mammal and Animal"
    );
}

#[test]
fn domain_and_range_produce_types() {
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:owns rdfs:domain ex:Person ; rdfs:range ex:Pet .\n\
         ex:alice ex:owns ex:rex .\n"
    ));
    run(&mut engine);
    assert_eq!(
        ask(&engine, &format!("SELECT ?s WHERE {{ ?s a <{EX}Person> }}")),
        vec![format!("s=<{EX}alice>")]
    );
    assert_eq!(
        ask(&engine, &format!("SELECT ?s WHERE {{ ?s a <{EX}Pet> }}")),
        vec![format!("s=<{EX}rex>")]
    );
}

#[test]
fn a_cycle_in_the_hierarchy_terminates() {
    // `a rdfs:subClassOf b` with `b rdfs:subClassOf a` is legal RDFS — it says the two are
    // equivalent — and a naive transitive walk of it does not terminate. Reflexive results
    // are dropped as well: `x rdfs:subClassOf x` is true, useless, and would make rdfs9
    // re-derive every type statement for ever.
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:A rdfs:subClassOf ex:B .\n\
         ex:B rdfs:subClassOf ex:A .\n\
         ex:thing a ex:A .\n"
    ));
    run(&mut engine);
    assert_eq!(
        ask(&engine, &format!("SELECT ?c WHERE {{ <{EX}thing> a ?c }}")).len(),
        2,
        "A and B, and not an endless walk between them"
    );
}

#[test]
fn running_it_twice_adds_nothing_the_second_time() {
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:father rdfs:subPropertyOf ex:parent .\n\
         ex:parent rdfs:subPropertyOf ex:relative .\n\
         ex:alice ex:father ex:bob .\n"
    ));
    let first = run(&mut engine);
    assert!(first.added > 0);
    let second = run(&mut engine);
    assert_eq!(
        second.added, 0,
        "a second run should find everything already there rather than compounding"
    );
}

#[test]
fn entailed_triples_are_separable_from_asserted_ones() {
    // The reason they go in their own graph: an inference can be undone, and a reader can
    // tell it from something somebody stated.
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:father rdfs:subPropertyOf ex:parent .\n\
         ex:alice ex:father ex:bob .\n"
    ));
    let before = engine.store().len();
    run(&mut engine);
    let graph = entailed_graph(&mut engine);

    let in_entailed = engine
        .store()
        .quads_for_pattern(None, None, None, GraphFilter::Named(graph))
        .count();
    assert!(in_entailed > 0, "the entailment went somewhere else");
    assert_eq!(
        engine
            .store()
            .quads_for_pattern(None, None, None, GraphFilter::Default)
            .count(),
        before,
        "the default graph must be exactly what was asserted"
    );
}

#[test]
fn a_closure_over_budget_is_refused_and_writes_nothing() {
    let mut engine = load(&format!(
        "@prefix ex: <{EX}> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:a rdfs:subPropertyOf ex:b .\n\
         ex:b rdfs:subPropertyOf ex:c .\n\
         ex:c rdfs:subPropertyOf ex:d .\n\
         ex:x ex:a ex:y .\n"
    ));
    let before = engine.store().len();
    let graph = entailed_graph(&mut engine);
    let after_marker = engine.store().len();
    let mut session = Session::unrestricted(engine.store()).expect("session");
    let outcome = entailment::materialise(&mut engine, &mut session, Some(graph), 1);
    assert!(outcome.is_err(), "a budget of one should not be met here");
    assert_eq!(
        engine.store().len(),
        after_marker,
        "a refused closure must leave the store as it found it"
    );
    let _ = before;
}

#[test]
fn the_geosparql_example_gains_its_feature_level_geometries() {
    // The case this was built for. The OGC example attaches geometries with
    // `my:hasExactGeometry`, declared `rdfs:subPropertyOf geo:hasGeometry` — and §17's
    // topology rewrite looks for `geo:hasGeometry`, so before entailment a feature-level
    // query finds nothing and the shorthand appears broken.
    const MY: &str = "http://example.org/ApplicationSchema#";
    const GEO: &str = "http://www.opengis.net/ont/geosparql#";
    let mut engine = Engine::new();
    engine
        .bulk_load(
            std::fs::File::open("../../examples/geosparql-example.rdf").expect("the OGC example"),
            RdfFormat::RdfXml,
            None,
        )
        .expect("load");

    let query = format!(
        "PREFIX my: <{MY}> PREFIX geo: <{GEO}> \
         SELECT ?f WHERE {{ ?f geo:sfWithin \
         \"<http://www.opengis.net/def/crs/OGC/1.3/CRS84> \
         Polygon((-83.6 34.1, -83.2 34.1, -83.2 34.5, -83.6 34.5, -83.6 34.1))\"\
         ^^geo:wktLiteral }}"
    );

    let before = ask(&engine, &query);
    assert!(
        !before
            .iter()
            .any(|r| r.contains("#A>") || r.contains("#B>")),
        "before entailment the query should reach geometries, not features: {before:?}"
    );

    run(&mut engine);
    let after = ask(&engine, &query);
    assert!(
        after.iter().any(|r| r.contains("#A>")),
        "after entailment the feature itself should be found: {after:?}"
    );
    assert!(
        after.len() > before.len(),
        "entailment should add features rather than replace geometries: {before:?} -> {after:?}"
    );
}
