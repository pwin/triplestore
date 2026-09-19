//! The write generation moves exactly when the set of quads does, on both backends.
//!
//! It exists so that something derived from the quads — a statistics snapshot — can ask
//! "has anything changed since I was made?" and get an exact answer. Every case below is a
//! way that answer could be wrong: a change that failed to move it, a non-change that
//! moved it, or a restart that forgot it.

use holos_store::{GraphFilter, Result, Store};
use oxrdf::{GraphName, NamedNode, Quad};

fn nn(s: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{s}"))
}

fn quad(s: &str, p: &str, o: &str) -> Quad {
    Quad {
        subject: nn(s).into(),
        predicate: nn(p),
        object: nn(o).into(),
        graph_name: GraphName::DefaultGraph,
    }
}

/// Every store the test runs against, so nothing is checked on one backend and assumed on
/// the other.
fn stores() -> Vec<(&'static str, Store, Option<tempfile::TempDir>)> {
    let mut out = vec![("memory", Store::new(), None)];
    #[cfg(feature = "rocksdb")]
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let rocks = Store::with_storage(holos_store::RocksStorage::open(dir.path()).expect("open"));
        out.push(("rocksdb", rocks, Some(dir)));
    }
    out
}

#[test]
fn an_insert_and_a_delete_each_move_it_and_a_no_op_does_not() -> Result<()> {
    for (name, mut store, _dir) in stores() {
        let g0 = store.generation();
        let q = quad("a", "p", "b");

        assert!(store.insert(q.as_ref())?);
        let g1 = store.generation();
        assert!(g1 > g0, "{name}: an insert must move the generation");

        // The same quad again is not a change. `insert` says so and the generation agrees.
        assert!(!store.insert(q.as_ref())?);
        assert_eq!(store.generation(), g1, "{name}: re-inserting a present quad is not a change");

        assert!(store.remove(q.as_ref())?);
        let g2 = store.generation();
        assert!(g2 > g1, "{name}: a delete must move the generation");

        assert!(!store.remove(q.as_ref())?);
        assert_eq!(store.generation(), g2, "{name}: removing an absent quad is not a change");
    }
    Ok(())
}

/// The case `quad_count` cannot see: delete one, insert another, same count, different data.
#[test]
fn a_swap_that_keeps_the_count_still_moves_it() -> Result<()> {
    for (name, mut store, _dir) in stores() {
        store.insert(quad("a", "p", "b").as_ref())?;
        let count = store.len();
        let g = store.generation();

        store.remove(quad("a", "p", "b").as_ref())?;
        store.insert(quad("a", "p", "c").as_ref())?;

        assert_eq!(store.len(), count, "{name}: the count is unchanged by construction");
        assert_ne!(store.generation(), g, "{name}: the data changed and the generation must say so");
    }
    Ok(())
}

/// A rolled-back scope leaves the quads as they were, and must leave the generation as it
/// was too. The in-memory backend undoes through the same insert and remove that advance
/// the counter, which is exactly how this could go wrong.
#[test]
fn a_rolled_back_scope_puts_it_back_and_a_committed_one_does_not() -> Result<()> {
    for (name, mut store, _dir) in stores() {
        store.insert(quad("a", "p", "b").as_ref())?;
        let before = store.generation();

        store.begin()?;
        store.insert(quad("x", "p", "y").as_ref())?;
        store.remove(quad("a", "p", "b").as_ref())?;
        store.rollback();
        assert_eq!(
            store.generation(),
            before,
            "{name}: after a rollback nothing changed, so the generation is where it started"
        );
        assert_eq!(store.len(), 1);

        store.begin()?;
        store.insert(quad("x", "p", "y").as_ref())?;
        store.commit()?;
        assert!(store.generation() > before, "{name}: a committed scope is a change");
    }
    Ok(())
}

/// A bulk load is a change however many quads it brings, and one that replaced quads while
/// keeping the count is still a change.
#[test]
fn a_bulk_load_moves_it() -> Result<()> {
    for (name, mut store, _dir) in stores() {
        let g0 = store.generation();
        store.begin_bulk_load()?;
        store.insert(quad("a", "p", "b").as_ref())?;
        store.insert(quad("c", "p", "d").as_ref())?;
        store.end_bulk_load()?;
        assert!(store.generation() > g0, "{name}: a bulk load must move the generation");
        assert_eq!(
            store.quads_for_pattern(None, None, None, GraphFilter::Default).count(),
            2
        );
    }
    Ok(())
}

/// The counter is persisted in the same batch as the change, so a reopened store carries on
/// from where it was rather than from zero — otherwise every restart would look like a
/// change to whatever compared against it, and a snapshot from before it would look fresh
/// to a store that has since been written to and reopened at the same number.
#[cfg(feature = "rocksdb")]
#[test]
fn it_survives_a_reopen() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let after_writes = {
        let mut store = Store::with_storage(holos_store::RocksStorage::open(dir.path())?);
        store.insert(quad("a", "p", "b").as_ref())?;
        store.insert(quad("c", "p", "d").as_ref())?;
        store.remove(quad("a", "p", "b").as_ref())?;
        store.generation()
    };
    assert!(after_writes >= 3, "three changes were made");

    let reopened = Store::with_storage(holos_store::RocksStorage::open(dir.path())?);
    assert_eq!(reopened.generation(), after_writes);
    Ok(())
}

/// The snapshot slot round-trips and is untouched by writes to the quads: keeping a
/// snapshot is not itself a change, and a change does not erase the snapshot — it is the
/// generation recorded *inside* the snapshot that says whether it still applies.
#[test]
fn the_snapshot_slot_is_kept_and_is_not_a_change() -> Result<()> {
    for (name, mut store, _dir) in stores() {
        assert_eq!(store.load_statistics()?, None, "{name}: nothing saved yet");
        let g = store.generation();
        store.save_statistics(b"anything")?;
        assert_eq!(store.generation(), g, "{name}: saving a snapshot is not a write to the quads");
        assert_eq!(store.load_statistics()?.as_deref(), Some(&b"anything"[..]));

        store.insert(quad("a", "p", "b").as_ref())?;
        assert_eq!(
            store.load_statistics()?.as_deref(),
            Some(&b"anything"[..]),
            "{name}: a write leaves the snapshot in place; its own generation is the check"
        );
        store.save_statistics(b"replaced")?;
        assert_eq!(store.load_statistics()?.as_deref(), Some(&b"replaced"[..]));
    }
    Ok(())
}
