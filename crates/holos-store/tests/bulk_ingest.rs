//! Sorted ingestion must produce exactly the store the ordinary write path would.
//!
//! `DESIGN.md` §6.1 wants `SstFileWriter` + `IngestExternalFile` for bulk loading, and the
//! measured win is a little under 2× on a whole load — the index writes go from about half of
//! it to almost none. But it is a *second* way to get data into the nine index orders, and
//! §12 rates "two storage implementations, two update paths" the most likely source of silent
//! corruption in this design. This is the third update path.
//!
//! So the property is equality, not plausibility: load the same documents both ways and
//! require the two stores to agree on every quad, every named graph, the count, and the
//! per-predicate statistics the planner reads. A load that is fast and drops one key in a
//! million is worse than a slow one, because nothing downstream will notice.
//!
//! Three things make this path able to differ where the batch path cannot:
//!
//! - **Keys must be strictly increasing in a file.** A `WriteBatch` takes the same key twice
//!   and shrugs; an `SstFileWriter` refuses. The bulk path defers duplicate detection, so
//!   duplicates certainly arrive here.
//! - **Nine orders, nine permutations.** Each is written from the same rows sorted a
//!   different way, and a permutation that disagrees with the one `insert_encoded` uses
//!   produces a file that is internally consistent and indexes the wrong thing.
//! - **Spilling.** Past a memory budget the load sorts what it has, writes it to a run file
//!   and starts again, then merges the runs back at the end. A row that appears either side
//!   of a spill reaches the merge from two places and must come out of it once.

#![cfg(feature = "rocksdb")]

use holos_store::{GraphFilter, Result, RocksStorage, Store};
use oxrdf::vocab::xsd;
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term, Triple};

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

/// A fixture with every shape the ingest has to get right.
fn fixture() -> Vec<Quad> {
    let mut quads = Vec::new();

    // Enough rows that the sort is doing real work and the orders genuinely differ.
    for i in 0..500 {
        let s = ex(&format!("s{i}"));
        // Default graph, so the three triple orders are exercised.
        quads.push(Quad {
            subject: s.clone().into(),
            predicate: ex("p"),
            object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        });
        quads.push(Quad {
            subject: s.clone().into(),
            predicate: ex("knows"),
            object: ex(&format!("s{}", (i + 7) % 500)).into(),
            graph_name: GraphName::DefaultGraph,
        });
        // Named graphs, so the six quad orders are too — and more than one, so the graph
        // component actually varies inside a key.
        quads.push(Quad {
            subject: s.clone().into(),
            predicate: ex("in"),
            object: ex(&format!("box{}", i % 3)).into(),
            graph_name: GraphName::NamedNode(ex(&format!("g{}", i % 4))),
        });
    }

    // A duplicate. The bulk path never checks, so this reaches the sort as two identical
    // rows, and a file writer that is handed both fails outright.
    quads.push(quads[0].clone());
    quads.push(quads[7].clone());

    // The term kinds with their own encoding paths, so the ingest is not only tested on IRIs.
    quads.push(Quad {
        subject: BlankNode::new_unchecked("b0").into(),
        predicate: ex("note"),
        object: Literal::new_simple_literal("x".repeat(600)).into(),
        graph_name: GraphName::DefaultGraph,
    });
    quads.push(Quad {
        subject: ex("oscar").into(),
        predicate: ex("said"),
        object: Term::Triple(Box::new(Triple {
            subject: ex("peggy").into(),
            predicate: ex("knows"),
            object: ex("trent").into(),
        })),
        graph_name: GraphName::NamedNode(ex("g0")),
    });

    quads
}

/// Everything a reader can see, in one comparable value.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    len: usize,
    quads: Vec<String>,
    graphs: Vec<String>,
    predicate_counts: Vec<(String, u64)>,
    /// One entry per index order, read through a pattern that routes to it.
    ///
    /// Without this the comparison is far weaker than it looks. A scan with nothing bound
    /// goes to `dspo` or `spog` and never touches the other seven orders, so a file written
    /// with the wrong permutation — internally sorted, internally consistent, indexing the
    /// wrong thing — reads back perfectly until someone binds a predicate. Which is to say:
    /// until a real query.
    by_order: Vec<(String, Vec<String>)>,
}

/// Patterns chosen so that between them they route to all nine orders.
///
/// Which order answers a scan is decided by which components are bound (`routed_scan`), so
/// the bindings are the test: `(_, p, o)` is the only way to reach `dpos`, `(s, _, o)` the
/// only way to reach `dosp`, and so on through the graph orders.
fn by_order(store: &Store) -> Result<Vec<(String, Vec<String>)>> {
    let s = store.lookup_term(ex("s3").as_ref().into())?;
    let p = store.lookup_term(ex("knows").as_ref().into())?;
    let o = store.lookup_term(ex("s10").as_ref().into())?;
    let in_p = store.lookup_term(ex("in").as_ref().into())?;
    let box_o = store.lookup_term(ex("box1").as_ref().into())?;
    let g1 = store.lookup_term(ex("g1").as_ref().into())?;

    let named = g1.map_or(GraphFilter::AnyNamed, GraphFilter::Named);
    let cases: Vec<(&str, Option<_>, Option<_>, Option<_>, GraphFilter)> = vec![
        // Default graph: dspo, dpos, dosp.
        ("dspo s..", s, None, None, GraphFilter::Default),
        ("dpos .po", None, p, o, GraphFilter::Default),
        ("dpos .p.", None, p, None, GraphFilter::Default),
        ("dosp s.o", s, None, o, GraphFilter::Default),
        ("dosp ..o", None, None, o, GraphFilter::Default),
        // One named graph: gspo, gpos, gosp.
        ("gspo g,s..", s, None, None, named),
        ("gpos g,.p.", None, in_p, None, named),
        ("gosp g,..o", None, None, box_o, named),
        // Any named graph: spog, posg, ospg.
        ("spog s..", s, None, None, GraphFilter::AnyNamed),
        ("posg .p.", None, in_p, None, GraphFilter::AnyNamed),
        ("ospg ..o", None, None, box_o, GraphFilter::AnyNamed),
    ];

    let mut out = Vec::new();
    for (label, s, p, o, graph) in cases {
        let mut rows = Vec::new();
        for row in store.quads_for_pattern(s, p, o, graph) {
            rows.push(store.decode_quad(row?)?.to_string());
        }
        rows.sort();
        out.push((label.to_owned(), rows));
    }
    Ok(out)
}

fn read(store: &Store) -> Result<Reading> {
    let mut quads = Vec::new();
    for row in store.quads_for_pattern(None, None, None, GraphFilter::Any) {
        quads.push(store.decode_quad(row?)?.to_string());
    }
    quads.sort();

    let mut graphs = Vec::new();
    for id in store.named_graphs()? {
        graphs.push(format!("{:?}", store.decode_term(id)?));
    }
    graphs.sort();

    // The statistics the planner reads. They are rebuilt by a scan at the end of a load, so
    // this also checks that the scan sees what the ingest wrote.
    let mut predicate_counts = Vec::new();
    for name in ["p", "knows", "in", "note", "said"] {
        if let Some(id) = store.lookup_term(ex(name).as_ref().into())? {
            predicate_counts.push((name.to_owned(), store.predicate_count(id)));
        }
    }

    Ok(Reading {
        len: store.len(),
        quads,
        graphs,
        predicate_counts,
        by_order: by_order(store)?,
    })
}

fn load(store: &mut Store, quads: &[Quad], bulk: bool) -> Result<()> {
    if bulk {
        store.begin_bulk_load()?;
    }
    for quad in quads {
        store.insert(quad.as_ref())?;
    }
    if bulk {
        store.end_bulk_load()?;
    }
    Ok(())
}

fn opened(dir: &tempfile::TempDir) -> Result<Store> {
    Ok(Store::with_storage(RocksStorage::open(dir.path())?))
}

#[test]
fn a_sorted_ingest_matches_an_ordinary_load() -> Result<()> {
    let quads = fixture();

    let plain_dir = tempfile::tempdir().expect("temp dir");
    let mut plain = opened(&plain_dir)?;
    load(&mut plain, &quads, false)?;

    let bulk_dir = tempfile::tempdir().expect("temp dir");
    let mut bulk = opened(&bulk_dir)?;
    load(&mut bulk, &quads, true)?;

    // And the other half of the pair: a load this size fits in one buffer, so the spill
    // machinery must stay out of its way entirely.
    assert_eq!(bulk.bulk_spills(), 0, "a small load spilled");
    assert_eq!(read(&bulk)?, read(&plain)?);
    Ok(())
}

/// And it must still be there after a reopen: the files are ingested rather than written
/// through the log, so "the store agrees with itself in memory" is not the question.
#[test]
fn an_ingested_store_survives_a_reopen() -> Result<()> {
    let quads = fixture();
    let dir = tempfile::tempdir().expect("temp dir");

    let expected = {
        let mut store = opened(&dir)?;
        load(&mut store, &quads, true)?;
        read(&store)?
    };

    let reopened = Store::with_storage(RocksStorage::open(dir.path())?);
    assert_eq!(read(&reopened)?, expected);
    Ok(())
}

/// A load that spills to disk must produce exactly the store one that did not produces.
///
/// The budget is set to 100 quads against a fixture of about 1,500, so this load spills
/// fifteen times and every order is merged from fifteen runs plus an in-memory tail. That is
/// the machinery lifting the memory ceiling, exercised at a scale where a test can check the
/// answer exactly rather than approximately.
#[test]
fn a_load_that_spills_to_disk_still_matches() -> Result<()> {
    let quads = fixture();

    let plain_dir = tempfile::tempdir().expect("temp dir");
    let mut plain = opened(&plain_dir)?;
    load(&mut plain, &quads, false)?;

    let bulk_dir = tempfile::tempdir().expect("temp dir");
    let mut storage = RocksStorage::open(bulk_dir.path())?;
    storage.set_ingest_limit(100);
    let mut bulk = Store::with_storage(storage);
    load(&mut bulk, &quads, true)?;

    // That it *spilled* is the point, and equality alone cannot show it: a load that
    // buffered everything would produce exactly the same store, so without this the test
    // passes with the spill removed entirely.
    assert!(
        bulk.bulk_spills() > 10,
        "expected this load to spill repeatedly, got {}",
        bulk.bulk_spills()
    );
    assert_eq!(read(&bulk)?, read(&plain)?);
    Ok(())
}

/// Loading into a store that already holds data: the ingested files overlap what is there,
/// which is the case RocksDB cannot place at the bottom level and has to work harder for.
#[test]
fn ingesting_over_existing_data_merges_rather_than_replaces() -> Result<()> {
    let quads = fixture();
    let (first, second) = quads.split_at(quads.len() / 2);

    let plain_dir = tempfile::tempdir().expect("temp dir");
    let mut plain = opened(&plain_dir)?;
    load(&mut plain, first, false)?;
    load(&mut plain, second, false)?;

    let bulk_dir = tempfile::tempdir().expect("temp dir");
    let mut bulk = opened(&bulk_dir)?;
    load(&mut bulk, first, true)?;
    load(&mut bulk, second, true)?;

    assert_eq!(read(&bulk)?, read(&plain)?);
    Ok(())
}

/// The scratch files are not left behind.
///
/// They sit inside the database directory so the ingest can move rather than copy them, which
/// means a leak would be picked up by `holos backup` and shipped with the store.
#[test]
fn a_finished_load_leaves_no_scratch_files() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = opened(&dir)?;
    load(&mut store, &fixture(), true)?;
    drop(store);

    assert!(
        !dir.path().join("holos-ingest").exists(),
        "the ingest directory outlived the load"
    );
    Ok(())
}

/// The size a maintenance operation has to fit.
///
/// Counted through the whole directory rather than from RocksDB's `total-sst-files-size`,
/// which leaves out the write-ahead log and the manifest — and a backup that cannot
/// hard-link has to copy those too.
#[test]
fn a_store_can_say_how_much_disk_it_uses() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = opened(&dir)?;

    let empty = store.on_disk_bytes().expect("a persistent store knows");
    load(&mut store, &fixture(), true).expect("load");
    store.flush().expect("flush");
    let loaded = store.on_disk_bytes().expect("still knows");

    assert!(loaded > empty, "{loaded} should exceed {empty}");
    // An in-memory store has no files, and saying `Some(0)` would read as "an empty store on
    // disk" rather than "not on disk at all".
    assert_eq!(Store::new().on_disk_bytes(), None);
    Ok(())
}
