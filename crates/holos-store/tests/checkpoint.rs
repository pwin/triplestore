//! Checkpoints: consistent snapshots taken while the store is open.
//!
//! The property under test is the one that makes an online backup possible — that the
//! snapshot can be taken *without stopping writes* and still opens as a complete store.
//! Copying the directory instead cannot do that, because an LSM tree in mid-flight is not a
//! set of files that can be copied one at a time.
//!
//! These run only with the `rocksdb` feature, since the in-memory tier has no files to
//! snapshot and says so.

#![cfg(feature = "rocksdb")]

use holos_store::{RocksStorage, StorageError, Store};
use oxrdf::{GraphName, NamedNode, Quad, Term};

const EX: &str = "http://example.org/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

fn quad(n: usize) -> Quad {
    Quad {
        subject: ex(&format!("s{n}")).into(),
        predicate: ex("p"),
        object: Term::NamedNode(ex(&format!("o{n}"))),
        graph_name: GraphName::DefaultGraph,
    }
}

/// A directory that does not exist yet, inside one that does.
fn destination(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "holos-checkpoint-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    // Deliberately not created: RocksDB requires the destination to be absent.
    dir
}

fn store_at(path: &std::path::Path) -> Store {
    Store::with_storage(RocksStorage::open(path).expect("opening a rocksdb store"))
}

#[test]
fn a_checkpoint_of_an_open_store_holds_everything_written() {
    let live = destination("live");
    let snapshot = destination("snap");

    let mut store = store_at(&live);
    for n in 0..100 {
        store.insert(quad(n).as_ref()).expect("insert");
    }

    // Taken while the store is open — no flush, no close, no stopping.
    store.checkpoint(&snapshot).expect("checkpoint");
    assert!(
        snapshot.is_dir(),
        "nothing was written to {}",
        snapshot.display()
    );

    // The live store is unaffected and still usable.
    store
        .insert(quad(1000).as_ref())
        .expect("the store is still writable");
    assert_eq!(store.len(), 101);

    let copy = store_at(&snapshot);
    assert_eq!(
        copy.len(),
        100,
        "the snapshot should hold what was written before it was taken, and nothing after"
    );
}

#[test]
fn a_checkpoint_is_not_disturbed_by_later_writes() {
    // The point of a *consistent* snapshot: what happens after it is taken cannot leak into
    // it, even though the two share their files through hard links.
    let live = destination("live2");
    let snapshot = destination("snap2");

    let mut store = store_at(&live);
    for n in 0..50 {
        store.insert(quad(n).as_ref()).expect("insert");
    }
    store.checkpoint(&snapshot).expect("checkpoint");

    for n in 50..500 {
        store.insert(quad(n).as_ref()).expect("insert");
    }
    store.flush().expect("flush");

    assert_eq!(store.len(), 500);
    assert_eq!(
        store_at(&snapshot).len(),
        50,
        "later writes leaked into the snapshot"
    );
}

#[test]
fn a_destination_that_already_exists_is_refused() {
    // RocksDB will not write into an existing directory, which is what makes timestamped
    // destinations the right pattern rather than a fixed path.
    let live = destination("live3");
    let snapshot = destination("snap3");
    std::fs::create_dir_all(&snapshot).expect("creating the destination");

    let mut store = store_at(&live);
    store.insert(quad(1).as_ref()).expect("insert");
    assert!(
        store.checkpoint(&snapshot).is_err(),
        "an existing destination should be refused, not written into"
    );
}

#[test]
fn a_checkpoint_during_a_bulk_load_is_refused() {
    // Bulk-load writes are buffered in this process rather than in RocksDB, so a checkpoint
    // taken now would be internally consistent and missing data — which is worse than a
    // failure, because it looks like a good backup.
    let live = destination("live4");
    let snapshot = destination("snap4");

    let mut store = store_at(&live);
    store.begin_bulk_load().expect("begin a bulk load");
    store.insert(quad(1).as_ref()).expect("insert");

    let error = store.checkpoint(&snapshot).expect_err("should refuse");
    assert!(
        matches!(error, StorageError::Unsupported(_)),
        "expected a refusal explaining why, got {error:?}"
    );
    assert!(
        !snapshot.exists(),
        "a refused checkpoint must not leave a partial directory behind"
    );
}

#[test]
fn an_in_memory_store_says_it_cannot() {
    // Not a silent no-op and not a copy of nothing: a backend that cannot produce a
    // consistent snapshot has to say so, or a backup script will believe it succeeded.
    let store = Store::new();
    let error = store
        .checkpoint(&destination("mem"))
        .expect_err("an in-memory store has no files to snapshot");
    assert!(
        matches!(error, StorageError::Unsupported(_)),
        "got {error:?}"
    );
}
