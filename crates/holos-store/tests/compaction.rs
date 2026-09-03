//! Compaction: rewriting a store keeps everything and reclaims the rest.
//!
//! The dictionary is append-only, so deleting quads never reclaims the terms they used. The
//! only thing that does is writing the live data into a fresh store — and the property that
//! matters is not that it is smaller, but that nothing was lost on the way.

use holos_core::Tag;
use holos_store::{GraphFilter, Store};
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term, Triple};

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

/// Copies every quad and every named graph into a fresh store.
///
/// The same thing `holos compact` does, in the two lines that carry its meaning: it writes
/// only terms it has just read, so anything reachable arrives with its referents and
/// anything unreachable is left behind by construction rather than by analysis.
fn compact(source: &Store) -> Store {
    let mut fresh = Store::new();
    for id in source.named_graphs().expect("graphs") {
        let Some(term) = source.decode_term(id).expect("decode") else {
            continue;
        };
        let name = match term {
            Term::NamedNode(n) => GraphName::NamedNode(n),
            Term::BlankNode(b) => GraphName::BlankNode(b),
            _ => continue,
        };
        fresh.insert_named_graph(&name).expect("graph");
    }
    for quad in source.iter() {
        fresh.insert(quad.expect("read").as_ref()).expect("insert");
    }
    fresh
}

/// Every quad, as comparable text, so two stores can be checked against each other.
fn contents(store: &Store) -> Vec<String> {
    let mut out: Vec<String> = store.iter().map(|q| q.expect("read").to_string()).collect();
    out.sort();
    out
}

fn churned() -> Store {
    let mut store = Store::new();
    for i in 0..40 {
        store
            .insert(
                Quad {
                    subject: ex(&format!("s{i}")).into(),
                    predicate: ex("p"),
                    object: Term::Literal(Literal::new_simple_literal(format!(
                        "a value long enough not to be inlined, number {i}"
                    ))),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    for i in 0..30 {
        store
            .remove(
                Quad {
                    subject: ex(&format!("s{i}")).into(),
                    predicate: ex("p"),
                    object: Term::Literal(Literal::new_simple_literal(format!(
                        "a value long enough not to be inlined, number {i}"
                    ))),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("remove");
    }
    store
}

#[test]
fn deleting_quads_does_not_reclaim_their_terms() {
    // The premise. If this ever stops being true, compaction stops being necessary and this
    // whole command can go.
    let store = churned();
    assert_eq!(store.len(), 10);
    assert!(
        store.dictionary_count_for(Tag::Literal) >= 40,
        "the dictionary should still hold every literal ever interned, not just the ten \
         still in use: {}",
        store.dictionary_count_for(Tag::Literal)
    );
}

#[test]
fn compaction_reclaims_what_deletion_left_behind() {
    let store = churned();
    let before = store.dictionary_len();
    let compacted = compact(&store);

    assert_eq!(compacted.len(), store.len(), "quads must survive");
    assert_eq!(
        contents(&compacted),
        contents(&store),
        "and be the same quads"
    );
    assert!(
        compacted.dictionary_len() < before,
        "compaction reclaimed nothing: {before} terms before, {} after",
        compacted.dictionary_len()
    );
}

#[test]
fn a_triple_terms_components_survive_compaction() {
    // The case that ruled out reclaiming in place. An RDF 1.2 triple term holds its
    // components by id, so `ex:inner` and `ex:said` below are interned IRIs that appear in
    // no quad at all — a reference check that looked only at quads would free them and leave
    // the triple term pointing at nothing.
    //
    // Copying cannot make that mistake, because it writes what it reads. This asserts the
    // hazard exists *and* that compaction is untroubled by it.
    let mut store = Store::new();
    let inner = Triple::new(
        ex("inner"),
        ex("said"),
        Term::Literal(Literal::new_simple_literal(
            "a value long enough not to be inlined",
        )),
    );
    store
        .insert(
            Quad {
                subject: ex("claim").into(),
                predicate: ex("says"),
                object: Term::Triple(Box::new(inner)),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )
        .expect("insert");

    // The hazard, stated as an assertion rather than as a comment.
    let hidden = store
        .lookup_term(Term::NamedNode(ex("inner")).as_ref())
        .expect("lookup")
        .expect("interned");
    assert_eq!(hidden.tag(), Tag::Iri, "it reached the dictionary");
    let mentions = store
        .quads_for_pattern(None, None, None, GraphFilter::Any)
        .filter(|q| {
            let q = q.as_ref().expect("read");
            q.subject == hidden || q.predicate == hidden || q.object == hidden
        })
        .count();
    assert_eq!(
        mentions, 0,
        "the premise of this test is that no quad mentions it directly"
    );

    let compacted = compact(&store);
    assert_eq!(contents(&compacted), contents(&store));
    assert_eq!(
        compacted.len(),
        1,
        "the quad carrying the triple term must survive"
    );
    // And the triple term still decodes, which is what freeing its components would break.
    let quad = compacted.iter().next().expect("a quad").expect("read");
    assert!(
        matches!(quad.object, Term::Triple(_)),
        "the triple term did not survive as a triple term: {:?}",
        quad.object
    );
}

#[test]
fn compaction_keeps_named_graphs_including_empty_ones() {
    // An empty named graph is a thing the Graph Store Protocol can distinguish from an
    // absent one, so losing it is a real change and not a tidy-up.
    let mut store = Store::new();
    store
        .insert(
            Quad {
                subject: ex("s").into(),
                predicate: ex("p"),
                object: Term::NamedNode(ex("o")),
                graph_name: GraphName::NamedNode(ex("g1")),
            }
            .as_ref(),
        )
        .expect("insert");
    store
        .insert_named_graph(&GraphName::NamedNode(ex("empty")))
        .expect("graph");

    let compacted = compact(&store);
    assert_eq!(
        compacted.named_graphs().expect("graphs").len(),
        store.named_graphs().expect("graphs").len(),
        "an empty named graph was dropped"
    );
    assert!(compacted
        .contains_named_graph(GraphName::NamedNode(ex("empty")).as_ref())
        .expect("contains"));
    assert_eq!(contents(&compacted), contents(&store));
}

#[test]
fn compaction_keeps_blank_node_identity() {
    // Blank node labels are what tie a subject to its object across quads. If compaction
    // relabelled them, a structure would come out shredded into unrelated fragments.
    let mut store = Store::new();
    let node = BlankNode::new_unchecked("shared");
    for predicate in ["street", "city"] {
        store
            .insert(
                Quad {
                    subject: node.clone().into(),
                    predicate: ex(predicate),
                    object: Term::Literal(Literal::new_simple_literal("value")),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    store
        .insert(
            Quad {
                subject: ex("person").into(),
                predicate: ex("address"),
                object: Term::BlankNode(node.clone()),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )
        .expect("insert");

    let compacted = compact(&store);
    assert_eq!(contents(&compacted), contents(&store));
    let linked = compacted
        .quads_for_pattern(None, None, None, GraphFilter::Any)
        .filter_map(|q| q.ok())
        .count();
    assert_eq!(linked, 3, "all three quads, still pointing at one node");
}

#[test]
fn compacting_a_store_that_has_lost_nothing_changes_nothing() {
    let mut store = Store::new();
    for i in 0..10 {
        store
            .insert(
                Quad {
                    subject: ex(&format!("s{i}")).into(),
                    predicate: ex("p"),
                    object: Term::NamedNode(ex("o")),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    }
    let compacted = compact(&store);
    assert_eq!(contents(&compacted), contents(&store));
    assert_eq!(compacted.dictionary_len(), store.dictionary_len());
}
