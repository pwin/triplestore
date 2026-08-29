//! Blank node labels are scoped to the document they appear in.
//!
//! RDF 1.1 §3.5: a blank node identifier is local to the file it is written in. `_:a` in two
//! documents denotes two different things, and a loader that keeps the label merges them
//! into one node — asserting an identity neither document stated, out of nothing but a
//! coincidence of spelling.
//!
//! It is easy to miss because it only shows up when two documents happen to choose the same
//! label, and `_:a`, `_:b0` and `_:genid1` are what every serialiser reaches for first. The
//! W3C dataset suite tests it directly: `dataset-09b` joins a default graph against a named
//! one on `?s`, over two files that both use blank node subjects, and the answer is *no rows*
//! precisely because those subjects are different nodes.

use holos_engine::Engine;
use oxrdf::{GraphName, NamedNode};
use oxrdfio::RdfFormat;
use std::collections::HashSet;

const DOC: &[u8] = b"_:a <http://example.com/p> <http://example.com/1> .\n";

fn subjects(engine: &Engine) -> HashSet<String> {
    engine
        .store()
        .iter()
        .map(|q| q.expect("decode").subject.to_string())
        .collect()
}

#[test]
fn two_documents_using_one_label_get_two_blank_nodes() {
    let mut engine = Engine::new();
    engine
        .bulk_load(DOC, RdfFormat::NTriples, None)
        .expect("first document");
    engine
        .bulk_load(DOC, RdfFormat::NTriples, None)
        .expect("second document");

    assert_eq!(
        subjects(&engine).len(),
        2,
        "the two documents each declared their own `_:a`, so the store holds two nodes"
    );
}

/// The same across a named graph, which is how the dataset suite reaches it.
#[test]
fn a_named_graph_does_not_share_labels_with_the_default_one() {
    let mut engine = Engine::new();
    engine
        .bulk_load(DOC, RdfFormat::NTriples, None)
        .expect("default graph");
    engine
        .bulk_load_into_graph(
            DOC,
            RdfFormat::NTriples,
            None,
            &GraphName::NamedNode(NamedNode::new_unchecked("http://example.com/g")),
        )
        .expect("named graph");

    assert_eq!(
        subjects(&engine).len(),
        2,
        "a join between the two graphs on the subject must find nothing"
    );
}

/// Renaming must not disturb anything that is *not* a blank node, which is what makes it
/// safe to do unconditionally.
#[test]
fn iris_and_literals_are_untouched() {
    let mut engine = Engine::new();
    let doc = br#"<http://example.com/s> <http://example.com/p> "v" .
<http://example.com/s> <http://example.com/q> <http://example.com/o> .
"#;
    engine
        .bulk_load(doc.as_ref(), RdfFormat::NTriples, None)
        .expect("first");
    // Loading it again adds nothing: without blank nodes the two documents say the same
    // thing, and RDF is a set.
    engine
        .bulk_load(doc.as_ref(), RdfFormat::NTriples, None)
        .expect("second");
    assert_eq!(engine.store().len(), 2);
    assert_eq!(subjects(&engine).len(), 1);
}
