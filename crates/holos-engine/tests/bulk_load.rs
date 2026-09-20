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

// --- the parse thread ----------------------------------------------------------------------
//
// Parsing runs on its own thread and hands batches to the loading thread over a bounded
// channel. That introduces three ways a load could go wrong that a single loop could not:
// a parse error partway through has to reach the caller as an error, not as a truncated
// load that reports success; quads have to arrive in file order, since ids are issued in
// arrival order and both backends are held to identical ids for identical input; and a
// document larger than one batch, or than the whole queue, has to load completely.

/// A few kilobytes of a triple per line, enough to cross several batch boundaries.
fn many_lines(n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..n {
        out.extend_from_slice(
            format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\" .\n").as_bytes(),
        );
    }
    out
}

#[test]
fn a_document_larger_than_the_queue_loads_completely_and_in_order() {
    // 8,192 per batch and 4 batches of queue: 50,000 lines is well past both.
    let doc = many_lines(50_000);
    let mut engine = Engine::new();
    let n = engine
        .bulk_load(doc.as_slice(), RdfFormat::NTriples, None)
        .expect("load");
    assert_eq!(n, 50_000);
    assert_eq!(engine.store().len(), 50_000);

    // In order: the store issues ids as quads arrive, so `s0` must have the lowest subject
    // id and `s49999` the highest. A batch delivered out of order would show up here.
    let first = engine
        .store()
        .lookup_term(
            NamedNode::new_unchecked("http://example.com/s0")
                .as_ref()
                .into(),
        )
        .expect("lookup")
        .expect("present");
    let last = engine
        .store()
        .lookup_term(
            NamedNode::new_unchecked("http://example.com/s49999")
                .as_ref()
                .into(),
        )
        .expect("lookup")
        .expect("present");
    assert!(
        first < last,
        "s0 ({first:?}) must be issued before s49999 ({last:?})"
    );
}

#[test]
fn a_parse_error_partway_through_is_an_error_not_a_short_load() {
    // Ten thousand good lines, one bad one, ten thousand more. With the parser a batch
    // ahead of the loader, the error arrives after some quads have already been inserted;
    // the caller must still see it.
    let mut doc = many_lines(10_000);
    doc.extend_from_slice(b"this is not a triple\n");
    doc.extend_from_slice(&many_lines(10_000));

    let mut engine = Engine::new();
    let outcome = engine.bulk_load(doc.as_slice(), RdfFormat::NTriples, None);
    assert!(
        matches!(outcome, Err(holos_engine::EngineError::Parse(_))),
        "expected a parse error, got {outcome:?}"
    );
    // What loaded before the error is whatever it is; what must not have happened is the
    // lines *after* it loading as if nothing were wrong.
    assert!(
        engine.store().len() <= 10_000,
        "lines after the parse error must not load, got {}",
        engine.store().len()
    );
}

#[test]
fn an_empty_document_loads_nothing_and_returns() {
    let mut engine = Engine::new();
    let n = engine
        .bulk_load(&b""[..], RdfFormat::NTriples, None)
        .expect("empty is fine");
    assert_eq!(n, 0);
}
