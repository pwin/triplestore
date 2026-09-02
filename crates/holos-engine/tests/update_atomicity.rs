//! An update is one commit, on the backend where that costs something.
//!
//! The in-memory tests in `update.rs` already assert the semantics: a failing operation
//! leaves nothing behind, and a later operation sees what an earlier one did. Those pass on
//! the in-memory store almost for free, because it applies as it goes and unwinds with a
//! journal — nothing is ever deferred, so nothing can be missed.
//!
//! The persistent backend earns them differently. There an update runs inside a commit scope
//! that *buffers*: the writes accumulate into one batch and reach the database only at the
//! end, which is what makes a crash mid-update leave the store as it was. The price is that
//! every read the update makes has to be answered through the buffer rather than from the
//! database. Duplicate detection is such a read. So is the `WHERE` clause of the operation
//! after the one that just wrote.
//!
//! So this file runs the same properties against RocksDB, and adds the one that only exists
//! there: reopening the store and finding that an abandoned update left no trace on disk.

#![cfg(feature = "rocksdb")]

use holos_engine::Engine;
use holos_security::Session;
use holos_store::{RocksStorage, Store};
use std::path::Path;

const EX: &str = "http://example.com/";

fn open(path: &Path) -> Engine {
    Engine::with_store(Store::with_storage(
        RocksStorage::open(path).expect("open the store"),
    ))
}

fn run(engine: &mut Engine, text: &str) -> Result<(), String> {
    let mut session = Session::unrestricted(engine.store()).expect("session");
    holos_engine::update::update(engine, &mut session, text, None).map_err(|e| e.to_string())?;
    Ok(())
}

/// Every quad in the store, as sorted text, so two stores can be compared.
fn contents(engine: &Engine) -> Vec<String> {
    let mut out: Vec<String> = engine
        .store()
        .iter()
        .map(|q| q.expect("decode").to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn later_operations_see_earlier_ones_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut engine = open(dir.path());

    // The second operation's `WHERE` clause has to match a quad the first one wrote and has
    // not yet flushed. Answered from the database alone it matches nothing, the DELETE is a
    // no-op, and the update quietly does half of what it says.
    run(
        &mut engine,
        &format!(
            "INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }} ; \
             DELETE {{ ?s <{EX}p> ?o }} INSERT {{ ?s <{EX}q> ?o }} WHERE {{ ?s <{EX}p> ?o }}"
        ),
    )
    .expect("update");

    assert_eq!(
        contents(&engine),
        vec![format!("<{EX}a> <{EX}q> <{EX}b>")],
        "the second operation did not see what the first one wrote"
    );
}

#[test]
fn a_duplicate_inside_one_update_is_still_one_quad() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut engine = open(dir.path());

    // Duplicate detection is a read. Without the buffer overlaying it, the second insert
    // finds nothing in the database, writes the quad again, and counts it twice — so `len`
    // and the index stop agreeing.
    run(
        &mut engine,
        &format!(
            "INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }} ; \
             INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }}"
        ),
    )
    .expect("update");

    assert_eq!(engine.store().len(), 1);
    assert_eq!(contents(&engine).len(), 1);
}

#[test]
fn a_failing_operation_leaves_nothing_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let mut engine = open(dir.path());
        run(
            &mut engine,
            &format!("INSERT DATA {{ <{EX}keep> <{EX}p> <{EX}b> }}"),
        )
        .expect("the setup update");

        // First operation succeeds, second cannot: the graph does not exist.
        let result = run(
            &mut engine,
            &format!("INSERT DATA {{ <{EX}new> <{EX}p> <{EX}x> }} ; DROP GRAPH <{EX}missing>"),
        );
        assert!(result.is_err(), "the second operation should fail");
        assert_eq!(
            contents(&engine),
            vec![format!("<{EX}keep> <{EX}p> <{EX}b>")]
        );
    }

    // And on disk, which is the part the in-memory tests cannot check: the abandoned batch
    // was never written, so there is nothing for a reopen to find.
    let reopened = open(dir.path());
    assert_eq!(
        contents(&reopened),
        vec![format!("<{EX}keep> <{EX}p> <{EX}b>")],
        "a failed update left something behind on disk"
    );
    assert_eq!(reopened.store().len(), 1);
}

#[test]
fn an_update_that_never_returns_leaves_nothing_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let mut engine = open(dir.path());
        run(
            &mut engine,
            &format!("INSERT DATA {{ <{EX}keep> <{EX}p> <{EX}b> }}"),
        )
        .expect("the setup update");

        // The crash, near enough: a scope is opened and the process goes away without
        // committing. Nothing was written, so no recovery can produce a partial commit.
        engine.store_mut().begin().expect("begin");
        run(
            &mut engine,
            &format!("INSERT DATA {{ <{EX}lost> <{EX}p> <{EX}x> }}"),
        )
        .expect("the inner update");
        // The inner update joined the open scope rather than committing on its own, so it is
        // visible here and not yet anywhere else.
        assert_eq!(contents(&engine).len(), 2);
        drop(engine);
    }

    let reopened = open(dir.path());
    assert_eq!(
        contents(&reopened),
        vec![format!("<{EX}keep> <{EX}p> <{EX}b>")],
        "an uncommitted scope reached the disk"
    );
}

#[test]
fn a_successful_update_reaches_the_disk_whole() {
    let dir = tempfile::tempdir().expect("temp dir");
    let expected = {
        let mut engine = open(dir.path());
        run(
            &mut engine,
            &format!(
                "INSERT DATA {{ <{EX}a> <{EX}p> <{EX}b> }} ; \
                 INSERT DATA {{ GRAPH <{EX}g> {{ <{EX}c> <{EX}p> <{EX}d> }} }} ; \
                 CREATE GRAPH <{EX}empty>"
            ),
        )
        .expect("update");
        contents(&engine)
    };

    let reopened = open(dir.path());
    assert_eq!(contents(&reopened), expected);
    // The empty graph is part of the commit too, and it is written through a different path
    // from the quads — one that used to bypass the scope entirely.
    let names: Vec<String> = reopened
        .store()
        .named_graphs()
        .expect("named graphs")
        .into_iter()
        .map(|id| format!("{:?}", reopened.store().decode_term(id).expect("decode")))
        .collect();
    assert_eq!(names.len(), 2, "expected both named graphs, got {names:?}");
}
