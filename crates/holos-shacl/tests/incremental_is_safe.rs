//! Incremental revalidation must not miss a violation a full validation would find.
//!
//! `DESIGN.md` §8 makes incremental revalidation the mechanism that lets the holon
//! Boundary (§9) gate every commit. That only works if it is *safe*: over-reporting costs
//! time, under-reporting lets an invalid graph through, which is the whole point of having
//! a Boundary at all.
//!
//! The property checked here is the safety direction:
//!
//! > every violation a full validation finds after a change, at a focus node the change
//! > touched, is also found by revalidating that change alone.

use holos_shacl::incremental::Change;
use holos_shacl::{CompiledShapes, Options, ValidationResult};
use holos_store::{GraphFilter, Store};
use oxrdfio::{RdfFormat, RdfParser};

const SHAPES_AND_DATA: &str = r#"
@prefix ex:   <http://example.com/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:Person a rdfs:Class .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name  ; sh:minCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age   ; sh:maxCount 1 ; sh:datatype xsd:integer ;
                  sh:minInclusive 0 ; sh:maxInclusive 150 ] ;
    sh:property [ sh:path ex:email ; sh:pattern "^[^@]+@[^@]+$" ] ;
    sh:property [ sh:path ex:knows ; sh:nodeKind sh:IRI ] ;
    # SHACL 1.2 constraints, here because two of them were added without their
    # dependencies being registered and this test did not notice. It could not: the
    # property it checks is only checked over the constraints the shapes actually use.
    sh:property [ sh:path ex:nickname ; sh:subsetOf ex:alias ] ;
    sh:property [ sh:path ex:tags ; sh:minListLength 1 ; sh:uniqueMembers true ] ;
    sh:uniqueValuesFor ex:email .

ex:alice a ex:Person ; ex:name "Alice" ; ex:age 30 ; ex:email "alice@example.com" .
ex:bob   a ex:Person ; ex:name "Bob"   ; ex:age 41 ; ex:email "bob@example.com" .
ex:carol a ex:Person ; ex:name "Carol" ; ex:age 28 ; ex:email "carol@example.com" .

# `ex:alias` is named by nothing except `sh:subsetOf`, so a write to it can only reach the
# shape through that constraint's dependency. Removing the alias below leaves the nickname
# outside the set it must be a subset of.
ex:carol ex:nickname "Caz" ; ex:alias "Caz" .

# `sh:targetWhere` selects focus nodes by *evaluating* a shape, so a write can pull a node
# into a shape's scope without touching anything that shape's own constraints read. Nothing
# but this filter names `ex:clearance`, so the only route from a write there to
# `ex:BadgeShape` runs through the target.
ex:Cleared a sh:NodeShape ;
    sh:class ex:Person ;
    sh:property [ sh:path ex:clearance ; sh:minCount 1 ] .

ex:BadgeShape a sh:NodeShape ;
    sh:targetWhere ex:Cleared ;
    sh:property [ sh:path ex:badge ; sh:minCount 1 ] .
"#;

fn load(store: &mut Store, turtle: &str) {
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(turtle.as_bytes()) {
        store.insert(quad.expect("parse").as_ref()).expect("insert");
    }
}

fn key(r: &ValidationResult) -> (u64, Option<u64>, Option<u64>, u64, u64) {
    (
        r.focus_node.to_raw(),
        r.path.map(holos_core::TermId::to_raw),
        r.value.map(holos_core::TermId::to_raw),
        r.source_shape.to_raw(),
        r.component.to_raw(),
    )
}

/// Applies a *removal* and checks the same safety property.
///
/// The inserting harness below cannot reach every case. `sh:subsetOf` is the example:
/// adding to the superset can never break a subset, so only a removal can turn a valid
/// graph invalid — and if the constraint's dependency is not registered, the shape is never
/// revalidated and the violation is missed.
fn check_removal(turtle_to_remove: &str, label: &str) {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let options = Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    };
    let shapes = CompiledShapes::compile(&store, options).expect("compile");

    let mut changes = Vec::new();
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(turtle_to_remove.as_bytes()) {
        let quad = quad.expect("parse");
        let encoded = store.encode_quad(quad.as_ref()).expect("encode");
        store.remove_encoded_quad(encoded).expect("remove");
        changes.push(Change::removed(encoded));
    }

    let after = shapes.validate(&store).expect("validate after").results;
    let incremental = shapes
        .revalidate(&store, &changes)
        .expect("revalidate")
        .results;
    let incremental_keys: Vec<_> = incremental.iter().map(key).collect();

    let touched: Vec<_> = changes
        .iter()
        .flat_map(|c| [c.quad.subject, c.quad.object])
        .collect();
    for result in &after {
        if !touched.contains(&result.focus_node) {
            continue;
        }
        assert!(
            incremental_keys.contains(&key(result)),
            "{label}: a full validation found a violation at a node the removal touched, \
             and revalidating the removal alone did not: {result:?}"
        );
    }
}

/// Applies a change and checks the safety property.
fn check_change(turtle_delta: &str, label: &str) {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    let options = Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    };
    let shapes = CompiledShapes::compile(&store, options).expect("compile");
    let before: Vec<_> = shapes
        .validate(&store)
        .expect("validate before")
        .results
        .iter()
        .map(key)
        .collect();

    // Apply the delta, recording exactly which quads changed.
    let mut changes = Vec::new();
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(turtle_delta.as_bytes()) {
        let quad = quad.expect("parse");
        let encoded = store.encode_quad(quad.as_ref()).expect("encode");
        store.insert_encoded(encoded).expect("insert");
        changes.push(Change::added(encoded));
    }

    let after = shapes.validate(&store).expect("validate after").results;
    let incremental = shapes
        .revalidate(&store, &changes)
        .expect("revalidate")
        .results;

    let after_keys: Vec<_> = after.iter().map(key).collect();
    let incremental_keys: Vec<_> = incremental.iter().map(key).collect();

    // Safety: every violation the change introduced must be caught incrementally.
    for k in &after_keys {
        if before.contains(k) {
            continue;
        }
        assert!(
            incremental_keys.contains(k),
            "{label}: incremental revalidation missed a new violation {k:?}\n\
             full-after found {} results, incremental found {}",
            after.len(),
            incremental.len()
        );
    }

    // Soundness: it must not invent violations a full validation does not find.
    for k in &incremental_keys {
        assert!(
            after_keys.contains(k),
            "{label}: incremental revalidation reported {k:?}, which a full validation does not"
        );
    }
}

#[test]
fn a_new_violating_value_is_caught() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           ex:alice ex:email "not-an-email" ."#,
        "bad email",
    );
}

#[test]
fn a_second_value_breaching_max_count_is_caught() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           ex:bob ex:age 42 ."#,
        "duplicate age",
    );
}

#[test]
fn an_out_of_range_value_is_caught() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           ex:carol ex:age 900 ."#,
        "age out of range",
    );
}

#[test]
fn a_wrong_datatype_is_caught() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           ex:alice ex:name 42 ."#,
        "numeric name",
    );
}

#[test]
fn a_literal_where_an_iri_belongs_is_caught() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           ex:bob ex:knows "not an iri" ."#,
        "literal in an IRI slot",
    );
}

/// A brand-new node only becomes a focus node because its `rdf:type` arrived, which is the
/// case the predicate index alone would miss.
#[test]
fn a_newly_typed_node_becomes_a_focus_node() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           ex:dave a ex:Person ; ex:email "dave-at-example" ."#,
        "new person",
    );
}

/// Compiling once and validating twice must give the same answer, byte for byte.
#[test]
fn reports_are_deterministic() {
    let mut store = Store::new();
    load(&mut store, SHAPES_AND_DATA);
    load(
        &mut store,
        r#"@prefix ex: <http://example.com/> .
           ex:alice ex:email "bad" . ex:bob ex:age 900 ."#,
    );
    let options = Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    };
    let shapes = CompiledShapes::compile(&store, options).expect("compile");

    let render = || {
        let report = shapes.validate(&store).expect("validate");
        let mut lines: Vec<String> = shapes
            .report_to_quads(&store, &report)
            .expect("render")
            .iter()
            .map(ToString::to_string)
            .collect();
        lines.sort();
        lines
    };
    assert_eq!(render(), render(), "two runs must render identically");
    assert!(!render().is_empty());
}

// ------------------------------------------------- the SHACL 1.2 constraints

#[test]
fn a_subset_of_violation_survives_a_removal() {
    // `sh:subsetOf` was compiled without registering its dependency at all, and an inserting
    // test could not show it: the shape is woken by its own `sh:path ex:nickname` whatever
    // the constraint declares. It takes a removal from the *other* side to expose it, and
    // `ex:alias` is named by nothing else in the shapes, so this is the only route to it.
    check_removal(
        r#"@prefix ex: <http://example.com/> .
           ex:carol ex:alias "Caz" ."#,
        "removing the alias a nickname had to be a subset of",
    );
}

#[test]
fn a_list_constraint_violation_survives_incremental_revalidation() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
           ex:alice ex:tags ex:taglist .
           ex:taglist rdf:first "x" ; rdf:rest ex:taglist2 .
           ex:taglist2 rdf:first "x" ; rdf:rest rdf:nil ."#,
        "a list whose members repeat",
    );
}

#[test]
fn a_unique_values_for_violation_survives_incremental_revalidation() {
    // The hard one, and the reason `validate_selected` recovers the whole target set for
    // shapes carrying this constraint. A global uniqueness property cannot be decided from
    // a partial working set: the node this change adds collides with `ex:alice`, which the
    // change never touched and which therefore is not in the working set at all.
    check_change(
        r#"@prefix ex: <http://example.com/> .
           @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
           ex:dave a ex:Person ; ex:name "Dave" ; ex:email "alice@example.com" ."#,
        "a duplicate email, colliding with a node the change did not touch",
    );
}

/// A node pulled into a shape's scope by `sh:targetWhere` must be revalidated.
///
/// The write is to `ex:clearance`, which `ex:BadgeShape` does not mention: it makes `ex:bob`
/// conform to the filter shape, and *that* makes him a focus node of a shape he violates.
/// Reaching `ex:BadgeShape` from the write means following the target, not a constraint.
#[test]
fn a_node_newly_selected_by_target_where_is_revalidated() {
    check_change(
        r#"@prefix ex: <http://example.com/> .
           ex:bob ex:clearance "secret" ."#,
        "target-where selection",
    );
}
