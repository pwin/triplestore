//! Validation reads its shapes from the store and leaves nothing behind.
//!
//! It used to load them into a named graph of the store it checked and never remove them. A
//! shapes file is mostly blank nodes, and blank nodes are renamed on every load, so each run
//! added another copy of every blank-node shape: one violation counted 1, 2, 3 on unchanged
//! data, and on a persistent store the copies survived a restart. Found by the SemOps manual,
//! whose script validated twice on one store and read the doubled count as an engine bug.
//!
//! So the property under test is not "the count is right once" but "nothing a validation does
//! is visible afterwards": the count is stable across runs, and the store's quads, named
//! graphs, write generation — and on disk, its dictionary — are as they were.

use holos_engine::Engine;
use holos_shacl::{CompiledShapes, Options};
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;

/// Two people, one without a name: one violation.
const DATA: &str = "@prefix ex: <http://example.com/> .
ex:alice a ex:Person ; ex:name \"Alice\" .
ex:bob a ex:Person .
";

/// The property shape is a blank node, as in most shapes files — which is what made each load
/// a new copy rather than a repeat.
const SHAPES: &str = "@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
";

fn violations(engine: &mut Engine) -> usize {
    let (count, _) = engine
        .with_transient_graph(
            SHAPES.as_bytes(),
            RdfFormat::Turtle,
            None,
            |store, shapes| {
                let shapes = CompiledShapes::compile(
                    store,
                    Options {
                        data_graph: GraphFilter::Default,
                        shapes_graph: shapes,
                    },
                )
                .expect("compile");
                shapes.validate(store).expect("validate").results.len()
            },
        )
        .expect("transient graph");
    count
}

/// What a validation must not change.
#[derive(Debug, PartialEq)]
struct Footprint {
    quads: usize,
    named_graphs: usize,
    generation: u64,
}

fn footprint(engine: &Engine) -> Footprint {
    Footprint {
        quads: engine.store().len(),
        named_graphs: engine.store().named_graphs().expect("graphs").len(),
        generation: engine.store().generation(),
    }
}

fn loaded() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

#[test]
fn the_count_does_not_climb_and_nothing_is_left() {
    let mut engine = loaded();
    let before = footprint(&engine);
    let counts: Vec<usize> = (0..3).map(|_| violations(&mut engine)).collect();
    assert_eq!(counts, vec![1, 1, 1], "each run saw the shapes once");
    assert_eq!(footprint(&engine), before);
    assert!(!engine.store().in_scope());
}

#[test]
fn a_panic_inside_still_leaves_nothing() {
    let mut engine = loaded();
    let before = footprint(&engine);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.with_transient_graph(SHAPES.as_bytes(), RdfFormat::Turtle, None, |_, _| {
            panic!("a validator failing midway")
        })
    }));
    assert!(outcome.is_err(), "the panic is the caller's to see");
    assert_eq!(footprint(&engine), before);
    assert!(
        !engine.store().in_scope(),
        "no scope left open to swallow later writes"
    );
    assert_eq!(violations(&mut engine), 1);
}

#[test]
fn input_that_does_not_parse_leaves_nothing() {
    let mut engine = loaded();
    let before = footprint(&engine);
    let result =
        engine.with_transient_graph("ex:a ex:b".as_bytes(), RdfFormat::Turtle, None, |_, _| ());
    assert!(result.is_err());
    assert_eq!(footprint(&engine), before);
    assert!(!engine.store().in_scope());
}

/// A store an earlier version validated still holds the copies it left. They are reported,
/// and never read as part of a new run.
#[test]
fn what_an_earlier_version_left_is_reported_and_not_read() {
    let mut engine = loaded();
    for _ in 0..2 {
        engine
            .bulk_load_into_graph(
                SHAPES.as_bytes(),
                RdfFormat::Turtle,
                None,
                &oxrdf::NamedNode::new_unchecked("urn:holos:python:shapes").into(),
            )
            .expect("an old run's leftovers");
    }
    let leftovers = engine.leftover_shapes_graphs().expect("leftovers");
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert_eq!(leftovers[0].0, "urn:holos:python:shapes");
    assert_eq!(
        violations(&mut engine),
        1,
        "the old copies are not part of this run"
    );
}

/// The graph is named afresh, so a store that uses the name for something else keeps it.
#[test]
fn a_store_already_using_the_name_is_not_disturbed() {
    let mut engine = loaded();
    engine
        .bulk_load(
            "<urn:holos:transient> <http://example.com/p> <http://example.com/o> .".as_bytes(),
            RdfFormat::NTriples,
            None,
        )
        .expect("load");
    let before = footprint(&engine);
    assert_eq!(violations(&mut engine), 1);
    assert_eq!(footprint(&engine), before);
}

#[test]
fn an_empty_input_is_an_empty_graph() {
    let mut engine = loaded();
    let before = footprint(&engine);
    let ((), loaded) = engine
        .with_transient_graph("".as_bytes(), RdfFormat::Turtle, None, |store, graph| {
            assert_eq!(store.quads_for_pattern(None, None, None, graph).count(), 0);
        })
        .expect("transient graph");
    assert_eq!(loaded, 0);
    assert_eq!(footprint(&engine), before);
}

/// On disk, where the leak survived a restart.
#[cfg(feature = "rocksdb")]
mod persistent {
    use super::{footprint, violations, DATA};
    use holos_engine::Engine;
    use holos_store::{RocksStorage, Store};
    use oxrdfio::RdfFormat;

    fn open(path: &std::path::Path) -> Engine {
        Engine::with_store(Store::with_storage(
            RocksStorage::open(path).expect("open the store"),
        ))
    }

    #[test]
    fn nothing_survives_a_restart_either() {
        let dir = tempfile::tempdir().expect("temp dir");
        let before;
        let dictionary;
        {
            let mut engine = open(dir.path());
            engine
                .bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None)
                .expect("load");
            engine.store_mut().flush().expect("flush");
            before = footprint(&engine);
            dictionary = engine.store().dictionary_len();
            let counts: Vec<usize> = (0..3).map(|_| violations(&mut engine)).collect();
            assert_eq!(counts, vec![1, 1, 1]);
            assert_eq!(footprint(&engine), before);
            engine.store_mut().flush().expect("flush");
        }
        let mut engine = open(dir.path());
        assert_eq!(footprint(&engine), before, "after reopening");
        assert_eq!(
            engine.store().dictionary_len(),
            dictionary,
            "the blank nodes of three shapes loads took no dictionary entries"
        );
        assert_eq!(violations(&mut engine), 1);
    }
}
