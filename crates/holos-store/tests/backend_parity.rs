//! Both backends must behave identically.
//!
//! `DESIGN.md` §12 rates "two storage implementations, two update paths" the most likely
//! source of silent corruption, and prescribes differential testing as the answer. This is
//! that test, applied to the two Tier A backends. The same rig is what the future Tier B
//! hypertrie will be checked against.
//!
//! Parity here is deliberately strict: not just "the same quads come back", but **the same
//! term ids**. Both backends allocate densely in encounter order, so an identical insertion
//! sequence must produce byte-identical keys. That makes the on-disk format a function of
//! the data alone, which is what a future bulk loader, a checkpoint and a replica all need.

#![cfg(feature = "rocksdb")]

use holos_core::TermId;
use holos_store::{GraphFilter, Result, RocksStorage, Store};
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term, Triple};

fn nn(s: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{s}"))
}

/// A fixture that exercises every term kind the encoding distinguishes.
fn fixture() -> Vec<Quad> {
    let inner = Triple {
        subject: nn("alice").into(),
        predicate: nn("age"),
        object: Literal::new_typed_literal("42", xsd::INTEGER).into(),
    };
    vec![
        // Plain IRIs, default graph.
        Quad {
            subject: nn("alice").into(),
            predicate: nn("knows"),
            object: nn("bob").into(),
            graph_name: GraphName::DefaultGraph,
        },
        // An inline literal — must never reach the dictionary.
        Quad {
            subject: nn("alice").into(),
            predicate: nn("age"),
            object: Literal::new_typed_literal("42", xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        },
        // A non-canonical lexical form of the same value: a *different* RDF term.
        Quad {
            subject: nn("alice").into(),
            predicate: nn("paddedAge"),
            object: Literal::new_typed_literal("042", xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        },
        // A literal too long to inline.
        Quad {
            subject: nn("alice").into(),
            predicate: nn("bio"),
            object: Literal::new_simple_literal("a considerably longer string").into(),
            graph_name: GraphName::DefaultGraph,
        },
        // Language and direction.
        Quad {
            subject: nn("alice").into(),
            predicate: rdf::TYPE.into_owned(),
            object: Literal::new_language_tagged_literal_unchecked("bonjour", "fr").into(),
            graph_name: GraphName::DefaultGraph,
        },
        // Blank nodes in both subject and object position.
        Quad {
            subject: BlankNode::new_unchecked("b0").into(),
            predicate: nn("relatedTo"),
            object: BlankNode::new_unchecked("b1").into(),
            graph_name: GraphName::DefaultGraph,
        },
        // Named graphs.
        Quad {
            subject: nn("bob").into(),
            predicate: nn("knows"),
            object: nn("carol").into(),
            graph_name: nn("g1").into(),
        },
        Quad {
            subject: nn("carol").into(),
            predicate: nn("knows"),
            object: nn("alice").into(),
            graph_name: nn("g2").into(),
        },
        // The same triple in two graphs — two distinct quads.
        Quad {
            subject: nn("alice").into(),
            predicate: nn("knows"),
            object: nn("bob").into(),
            graph_name: nn("g1").into(),
        },
        // An RDF 1.2 reified triple, and a nested one.
        Quad {
            subject: nn("claim1").into(),
            predicate: rdf::REIFIES.into_owned(),
            object: Term::Triple(Box::new(inner.clone())),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: nn("claim2").into(),
            predicate: rdf::REIFIES.into_owned(),
            object: Term::Triple(Box::new(Triple {
                subject: nn("claim1").into(),
                predicate: rdf::REIFIES.into_owned(),
                object: Term::Triple(Box::new(inner)),
            })),
            graph_name: nn("g2").into(),
        },
        // A literal long enough to take the hashed dictionary-key path.
        Quad {
            subject: nn("alice").into(),
            predicate: nn("essay"),
            object: Literal::new_simple_literal("x".repeat(2000)).into(),
            graph_name: GraphName::DefaultGraph,
        },
    ]
}

fn load(store: &mut Store, quads: &[Quad]) -> Result<()> {
    for quad in quads {
        store.insert(quad.as_ref())?;
    }
    Ok(())
}

/// Every pattern shape, so no permutation or un-permutation goes unchecked.
fn all_patterns(store: &Store) -> Result<Vec<(String, Vec<String>)>> {
    let bound = |s: &str| store.lookup_term(nn(s).as_ref().into());
    let subjects = [None, bound("alice")?, bound("bob")?, bound("claim1")?];
    let predicates = [None, bound("knows")?, bound("age")?];
    let objects = [None, bound("bob")?, bound("carol")?];
    let graphs = [
        GraphFilter::Default,
        GraphFilter::AnyNamed,
        GraphFilter::Any,
        GraphFilter::Named(bound("g1")?.expect("g1 is in the fixture")),
        GraphFilter::Named(bound("g2")?.expect("g2 is in the fixture")),
    ];

    let mut out = Vec::new();
    for s in subjects {
        for p in predicates {
            for o in objects {
                for g in graphs {
                    let label = format!("{s:?}/{p:?}/{o:?}/{g:?}");
                    let mut rows: Vec<String> = store
                        .quads_for_pattern(s, p, o, g)
                        .map(|q| Ok(store.decode_quad(q?)?.to_string()))
                        .collect::<Result<Vec<_>>>()?;
                    rows.sort();
                    out.push((label, rows));
                }
            }
        }
    }
    Ok(out)
}

fn term_ids(store: &Store, quads: &[Quad]) -> Result<Vec<Option<TermId>>> {
    let mut out = Vec::new();
    for quad in quads {
        out.push(store.lookup_term(quad.subject.as_ref().into())?);
        out.push(store.lookup_term(quad.predicate.as_ref().into())?);
        out.push(store.lookup_term(quad.object.as_ref())?);
    }
    Ok(out)
}

#[test]
fn the_two_backends_agree_on_everything() -> Result<()> {
    let quads = fixture();
    let dir = tempfile::tempdir().expect("temp dir");

    let mut memory = Store::new();
    let mut rocks = Store::with_storage(RocksStorage::open(dir.path())?);
    load(&mut memory, &quads)?;
    load(&mut rocks, &quads)?;

    assert_eq!(memory.len(), rocks.len(), "quad count");
    assert_eq!(memory.len(), quads.len(), "every fixture quad was stored");
    assert_eq!(
        memory.dictionary_len(),
        rocks.dictionary_len(),
        "dictionary size — the same terms must have been interned, and only those"
    );

    // Strict parity: identical insertion order must give identical ids in both backends.
    assert_eq!(
        term_ids(&memory, &quads)?,
        term_ids(&rocks, &quads)?,
        "term ids must be a function of the data, not of the backend"
    );

    for ((label, from_memory), (_, from_rocks)) in all_patterns(&memory)?
        .into_iter()
        .zip(all_patterns(&rocks)?)
    {
        assert_eq!(from_memory, from_rocks, "pattern {label} disagrees");
    }

    let mut memory_graphs = memory.named_graphs()?;
    let mut rocks_graphs = rocks.named_graphs()?;
    memory_graphs.sort_unstable();
    rocks_graphs.sort_unstable();
    assert_eq!(memory_graphs, rocks_graphs, "named graphs");
    assert_eq!(
        memory.predicate_histogram(),
        rocks.predicate_histogram(),
        "predicate statistics"
    );
    Ok(())
}

#[test]
fn deletes_agree_too() -> Result<()> {
    let quads = fixture();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut memory = Store::new();
    let mut rocks = Store::with_storage(RocksStorage::open(dir.path())?);
    load(&mut memory, &quads)?;
    load(&mut rocks, &quads)?;

    for quad in quads.iter().step_by(3) {
        assert_eq!(
            memory.remove(quad.as_ref())?,
            rocks.remove(quad.as_ref())?,
            "removing {quad}"
        );
        // Removing twice must be a no-op in both.
        assert_eq!(memory.remove(quad.as_ref())?, rocks.remove(quad.as_ref())?);
    }
    assert_eq!(memory.len(), rocks.len());
    assert_eq!(memory.predicate_histogram(), rocks.predicate_histogram());

    // Dropping a graph takes its quads with it, identically.
    assert_eq!(
        memory.remove_named_graph(nn("g1").as_ref().into())?,
        rocks.remove_named_graph(nn("g1").as_ref().into())?
    );
    assert_eq!(memory.len(), rocks.len());
    for ((label, a), (_, b)) in all_patterns(&memory)?
        .into_iter()
        .zip(all_patterns(&rocks)?)
    {
        assert_eq!(a, b, "after deletion, pattern {label} disagrees");
    }
    Ok(())
}

#[test]
fn data_survives_a_reopen() -> Result<()> {
    let quads = fixture();
    let dir = tempfile::tempdir().expect("temp dir");

    let (expected_len, expected_dict, expected_rows, expected_stats) = {
        let mut store = Store::with_storage(RocksStorage::open(dir.path())?);
        load(&mut store, &quads)?;
        store.flush()?;
        let mut rows: Vec<String> = store
            .iter()
            .map(|q| Ok(q?.to_string()))
            .collect::<Result<Vec<_>>>()?;
        rows.sort();
        (
            store.len(),
            store.dictionary_len(),
            rows,
            store.predicate_histogram(),
        )
    };

    let reopened = Store::with_storage(RocksStorage::open(dir.path())?);
    assert_eq!(reopened.len(), expected_len, "quad count survived");
    assert_eq!(
        reopened.dictionary_len(),
        expected_dict,
        "id counters survived, so the next id issued will not collide"
    );
    let mut rows: Vec<String> = reopened
        .iter()
        .map(|q| Ok(q?.to_string()))
        .collect::<Result<Vec<_>>>()?;
    rows.sort();
    assert_eq!(
        rows, expected_rows,
        "every quad decoded the same after reopen"
    );
    assert_eq!(
        reopened.predicate_histogram(),
        expected_stats,
        "statistics survived"
    );
    Ok(())
}

#[test]
fn a_reopened_store_keeps_allocating_fresh_ids() -> Result<()> {
    // The counters are persisted in the same batch as the row they number. If they were
    // not, a reopen would reissue ids already in use and silently alias two terms.
    let dir = tempfile::tempdir().expect("temp dir");
    let first = {
        let mut store = Store::with_storage(RocksStorage::open(dir.path())?);
        let id = store.encode_quad(
            Quad {
                subject: nn("a").into(),
                predicate: nn("p"),
                object: Literal::new_simple_literal("a long value that will not inline").into(),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )?;
        store.flush()?;
        id
    };

    let mut store = Store::with_storage(RocksStorage::open(dir.path())?);
    let second = store.encode_quad(
        Quad {
            subject: nn("b").into(),
            predicate: nn("p"),
            object: Literal::new_simple_literal("a different long value, also not inline").into(),
            graph_name: GraphName::DefaultGraph,
        }
        .as_ref(),
    )?;
    assert_ne!(
        first.subject, second.subject,
        "IRI ids must not be reissued"
    );
    assert_ne!(
        first.object, second.object,
        "literal ids must not be reissued"
    );
    assert_eq!(
        first.predicate, second.predicate,
        "the same predicate must resolve to the same id across a reopen"
    );
    Ok(())
}

#[test]
fn long_literals_take_the_hashed_key_path_and_stay_distinct() -> Result<()> {
    // Past 512 serialised bytes the dictionary keys by hash. Two long literals differing
    // in one byte must still be two terms — the property §5 exists to protect.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut rocks = Store::with_storage(RocksStorage::open(dir.path())?);
    let mut memory = Store::new();

    let a = Literal::new_simple_literal(format!("{}a", "x".repeat(3000)));
    let b = Literal::new_simple_literal(format!("{}b", "x".repeat(3000)));

    for store in [&mut rocks, &mut memory] {
        let ia = store.encode_quad(
            Quad {
                subject: nn("s").into(),
                predicate: nn("p"),
                object: a.clone().into(),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )?;
        let ib = store.encode_quad(
            Quad {
                subject: nn("s").into(),
                predicate: nn("p"),
                object: b.clone().into(),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )?;
        assert_ne!(ia.object, ib.object, "two long literals were conflated");
        assert_eq!(
            store.decode_term(ia.object)?,
            Some(Term::Literal(a.clone())),
            "long literal round trip"
        );
        assert_eq!(
            store.decode_term(ib.object)?,
            Some(Term::Literal(b.clone()))
        );
        // Re-interning must find the existing id, not allocate a second one.
        assert_eq!(store.lookup_term(a.as_ref().into())?, Some(ia.object));
        assert_eq!(store.lookup_term(b.as_ref().into())?, Some(ib.object));
    }
    Ok(())
}

/// A bulk load must produce exactly what an ordinary load produces.
///
/// This test exists because it did not, and nothing caught it: the bulk path buffered
/// writes by replaying one `WriteBatch` into another, and `WriteBatchIterator` carries no
/// column family, so every index write was silently dropped. A million triples loaded to
/// forty-five. Parity between backends was green throughout — the gap was that no test
/// exercised the bulk path at all.
#[test]
fn a_bulk_load_stores_exactly_what_an_ordinary_load_does() -> Result<()> {
    // Enough quads to cross the internal batch threshold more than once.
    let mut quads = Vec::new();
    for i in 0..40_000u32 {
        quads.push(Quad {
            subject: nn(&format!("s{i}")).into(),
            predicate: nn(&format!("p{}", i % 7)),
            object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
            graph_name: if i % 3 == 0 {
                GraphName::DefaultGraph
            } else {
                nn(&format!("g{}", i % 4)).into()
            },
        });
    }

    let plain_dir = tempfile::tempdir().expect("temp dir");
    let bulk_dir = tempfile::tempdir().expect("temp dir");

    let mut plain = Store::with_storage(RocksStorage::open(plain_dir.path())?);
    load(&mut plain, &quads)?;
    plain.flush()?;

    let mut bulk = Store::with_storage(RocksStorage::open(bulk_dir.path())?);
    bulk.begin_bulk_load();
    load(&mut bulk, &quads)?;
    bulk.end_bulk_load()?;

    assert_eq!(bulk.len(), quads.len(), "every quad survived the bulk load");
    assert_eq!(plain.len(), bulk.len(), "quad count");
    assert_eq!(
        plain.dictionary_len(),
        bulk.dictionary_len(),
        "dictionary size"
    );
    assert_eq!(
        plain.predicate_histogram(),
        bulk.predicate_histogram(),
        "statistics rebuilt at the end of a bulk load must match the maintained ones"
    );

    let mut plain_graphs = plain.named_graphs()?;
    let mut bulk_graphs = bulk.named_graphs()?;
    plain_graphs.sort_unstable();
    bulk_graphs.sort_unstable();
    assert_eq!(plain_graphs, bulk_graphs, "named graphs");

    let rows = |s: &Store| -> Result<Vec<String>> {
        let mut v: Vec<String> = s
            .iter()
            .map(|q| Ok(q?.to_string()))
            .collect::<Result<_>>()?;
        v.sort();
        Ok(v)
    };
    assert_eq!(
        rows(&plain)?,
        rows(&bulk)?,
        "every quad decodes identically"
    );
    Ok(())
}

/// A bulk load must not hand the same term two ids.
#[test]
fn a_bulk_load_interns_a_repeated_term_once() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = Store::with_storage(RocksStorage::open(dir.path())?);
    store.begin_bulk_load();
    // The same long literal, used by many subjects, must intern once — the buffered-term
    // map is what makes that true before anything reaches disk.
    let shared = Literal::new_simple_literal("a shared value that is far too long to inline");
    let mut ids = Vec::new();
    for i in 0..5_000u32 {
        let encoded = store.encode_quad(
            Quad {
                subject: nn(&format!("s{i}")).into(),
                predicate: nn("p"),
                object: shared.clone().into(),
                graph_name: GraphName::DefaultGraph,
            }
            .as_ref(),
        )?;
        store.insert_encoded(encoded)?;
        ids.push(encoded.object);
    }
    store.end_bulk_load()?;
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "the shared literal was given more than one id"
    );
    assert_eq!(
        store.decode_term(ids[0])?,
        Some(Term::Literal(shared)),
        "and it still decodes"
    );
    assert_eq!(store.len(), 5_000);
    Ok(())
}
