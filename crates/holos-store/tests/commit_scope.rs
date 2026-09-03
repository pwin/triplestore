//! A commit scope must be invisible except for when the writes land.
//!
//! A single quad write is already atomic — every index order for it goes into one batch — so
//! what a scope adds is atomicity across *several*. The risk it introduces is the price of
//! that: writes are buffered, and a buffered write is invisible to a `get` and to an
//! iterator. If the buffer were not overlaid onto every read, a scope would quietly show its
//! caller the store as it was when the scope opened, and SPARQL Update's rule that each
//! operation sees the effects of the ones before it would break.
//!
//! So the shape of these tests is: run a sequence of operations twice, once inside a scope
//! and once outside, and require that every read agrees at every step. The only difference a
//! scope is allowed to make is *when* the bytes reach the disk — which the durability tests
//! at the bottom check by reopening the store.
//!
//! Both backends are exercised, because both implement the trait and only one of them
//! buffers: the in-memory store applies as it goes and journals an undo, so the same
//! assertions are checking two entirely different mechanisms.

use holos_store::{GraphFilter, MemoryStorage, Result, Store};
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, GraphNameRef, Literal, NamedNode, Quad, Term, Triple};

fn nn(s: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{s}"))
}

fn q(s: &str, p: &str, o: &str) -> Quad {
    Quad {
        subject: nn(s).into(),
        predicate: nn(p),
        object: nn(o).into(),
        graph_name: GraphName::DefaultGraph,
    }
}

fn qg(s: &str, p: &str, o: &str, g: &str) -> Quad {
    Quad {
        graph_name: nn(g).into(),
        ..q(s, p, o)
    }
}

/// Everything a reader can ask, in one comparable value.
///
/// Reading the whole store rather than the part a step touched is the point: a scope that
/// leaked would most likely do so somewhere the step was not looking.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    len: usize,
    quads: Vec<String>,
    graphs: Vec<String>,
    named_only: Vec<String>,
    contains: Vec<bool>,
}

fn read(store: &Store, probes: &[Quad]) -> Result<Reading> {
    let mut quads = Vec::new();
    for row in store.quads_for_pattern(None, None, None, GraphFilter::Any) {
        quads.push(format!("{}", store.decode_quad(row?)?));
    }
    quads.sort();

    let mut named_only = Vec::new();
    for row in store.quads_for_pattern(None, None, None, GraphFilter::AnyNamed) {
        named_only.push(format!("{}", store.decode_quad(row?)?));
    }
    named_only.sort();

    let mut graphs = Vec::new();
    for g in store.named_graphs()? {
        graphs.push(format!("{:?}", store.decode_term(g)?));
    }
    graphs.sort();

    let mut contains = Vec::new();
    for probe in probes {
        contains.push(store.contains(probe.as_ref())?);
    }

    Ok(Reading {
        len: store.len(),
        quads,
        graphs,
        named_only,
        contains,
    })
}

/// One thing a caller can do to a store.
#[derive(Debug, Clone)]
enum Step {
    Insert(Quad),
    Remove(Quad),
    CreateGraph(NamedNode),
    DropGraph(NamedNode),
}

fn perform(store: &mut Store, step: &Step) -> Result<bool> {
    match step {
        Step::Insert(quad) => store.insert(quad.as_ref()),
        Step::Remove(quad) => store.remove(quad.as_ref()),
        Step::CreateGraph(g) => store.insert_named_graph(&GraphName::NamedNode(g.clone())),
        Step::DropGraph(g) => store.remove_named_graph(GraphNameRef::NamedNode(g.as_ref())),
    }
}

/// A sequence that touches every path the overlay has to cover.
fn script() -> Vec<Step> {
    vec![
        // Fresh quads, default and named.
        Step::Insert(q("alice", "knows", "bob")),
        Step::Insert(qg("alice", "knows", "carol", "g1")),
        // A duplicate: must be reported as no change, which is a *read* of the scope's own
        // write. Without the overlay this returns true and the quad is counted twice.
        Step::Insert(q("alice", "knows", "bob")),
        // Written then unwritten inside the scope.
        Step::Insert(q("dave", "knows", "erin")),
        Step::Remove(q("dave", "knows", "erin")),
        // Removed then written back: the database still holds it, so an overlaid scan must
        // yield it exactly once.
        Step::Remove(q("alice", "knows", "bob")),
        Step::Insert(q("alice", "knows", "bob")),
        // A graph created empty, and one dropped with quads in it.
        Step::CreateGraph(nn("g2")),
        Step::Insert(qg("frank", "knows", "grace", "g2")),
        Step::Insert(qg("heidi", "knows", "ivan", "g3")),
        Step::DropGraph(nn("g3")),
        // A long literal: past the inline threshold it interns, and past the key threshold
        // its dictionary key is a hash, so this is the candidate-list path.
        Step::Insert(Quad {
            subject: nn("judy").into(),
            predicate: nn("note"),
            object: Literal::new_typed_literal("x".repeat(600), xsd::STRING).into(),
            graph_name: GraphName::DefaultGraph,
        }),
        // The same long literal again, as an object of a different subject: it must resolve
        // to the id the scope minted, not a second one, or the two quads stop joining.
        Step::Insert(Quad {
            subject: nn("mallory").into(),
            predicate: nn("note"),
            object: Literal::new_typed_literal("x".repeat(600), xsd::STRING).into(),
            graph_name: GraphName::DefaultGraph,
        }),
        // A quoted triple, whose parts intern bottom-up.
        Step::Insert(Quad {
            subject: nn("oscar").into(),
            predicate: nn("said"),
            object: Term::Triple(Box::new(Triple {
                subject: nn("peggy").into(),
                predicate: nn("knows"),
                object: nn("trent").into(),
            })),
            graph_name: GraphName::DefaultGraph,
        }),
    ]
}

fn probes() -> Vec<Quad> {
    vec![
        q("alice", "knows", "bob"),
        q("dave", "knows", "erin"),
        qg("alice", "knows", "carol", "g1"),
        qg("heidi", "knows", "ivan", "g3"),
        qg("frank", "knows", "grace", "g2"),
        q("nobody", "knows", "nothing"),
    ]
}

/// A store the whole script has already been applied to, so the script's removals have
/// something to remove and its re-insertions have something to have been there.
fn seed(store: &mut Store) -> Result<()> {
    store.insert(q("alice", "knows", "bob").as_ref())?;
    store.insert(qg("heidi", "knows", "ivan", "g3").as_ref())?;
    store.insert(
        Quad {
            subject: nn("victor").into(),
            predicate: nn("note"),
            object: Literal::new_typed_literal("y".repeat(600), xsd::STRING).into(),
            graph_name: GraphName::DefaultGraph,
        }
        .as_ref(),
    )?;
    Ok(())
}

/// Runs the script step by step, reading the whole store after each one.
fn trace(store: &mut Store) -> Result<Vec<(bool, Reading)>> {
    let probes = probes();
    let mut out = Vec::new();
    for step in script() {
        let changed = perform(store, &step)?;
        out.push((changed, read(store, &probes)?));
    }
    Ok(out)
}

/// The headline property: a scope changes nothing a reader inside it can see.
fn reads_agree(mut scoped: Store, mut plain: Store) -> Result<()> {
    seed(&mut scoped)?;
    seed(&mut plain)?;

    scoped.begin()?;
    assert!(scoped.in_scope());
    let inside = trace(&mut scoped)?;
    let outside = trace(&mut plain)?;

    for (step, (a, b)) in inside.iter().zip(&outside).enumerate() {
        assert_eq!(
            a, b,
            "step {step} of the script read differently in a scope"
        );
    }

    scoped.commit()?;
    assert!(!scoped.in_scope());
    // And after the commit, which is where the buffered writes finally land.
    assert_eq!(read(&scoped, &probes())?, read(&plain, &probes())?);
    Ok(())
}

#[test]
fn memory_reads_agree_inside_a_scope() -> Result<()> {
    reads_agree(Store::new(), Store::new())
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocks_reads_agree_inside_a_scope() -> Result<()> {
    let a = tempfile::tempdir().expect("temp dir");
    let b = tempfile::tempdir().expect("temp dir");
    reads_agree(
        Store::with_storage(holos_store::RocksStorage::open(a.path())?),
        Store::with_storage(holos_store::RocksStorage::open(b.path())?),
    )
}

/// A rollback returns the store to the reading it opened with — exactly, not approximately.
fn rollback_restores(mut store: Store) -> Result<()> {
    seed(&mut store)?;
    let before = read(&store, &probes())?;

    store.begin()?;
    trace(&mut store)?;
    // Nothing in the script is a no-op, so this is a real state to unwind.
    assert_ne!(read(&store, &probes())?, before);
    store.rollback();

    assert!(!store.in_scope());
    assert_eq!(read(&store, &probes())?, before);
    Ok(())
}

#[test]
fn memory_rollback_restores() -> Result<()> {
    rollback_restores(Store::new())
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocks_rollback_restores() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    rollback_restores(Store::with_storage(holos_store::RocksStorage::open(
        dir.path(),
    )?))
}

/// Nesting is refused rather than flattened: a scope inside a scope reads as two rollback
/// points and would be one.
fn nesting_is_refused(mut store: Store) -> Result<()> {
    store.begin()?;
    assert!(store.begin().is_err(), "a nested scope was accepted");
    store.rollback();
    // And a commit with nothing open is a caller error, not a silent success.
    assert!(store.commit().is_err(), "a commit with no scope succeeded");
    // A rollback with nothing open is not, because it is the failure path and must be safe
    // to call when the caller does not know whether a scope survived.
    store.rollback();
    Ok(())
}

#[test]
fn memory_nesting_is_refused() -> Result<()> {
    nesting_is_refused(Store::new())
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocks_nesting_is_refused() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    nesting_is_refused(Store::with_storage(holos_store::RocksStorage::open(
        dir.path(),
    )?))
}

/// An in-memory store has nothing to make durable, so it satisfies the trait's default
/// weakly and deliberately: `begin` is accepted, and the guarantee it gives is the journal.
#[test]
fn memory_scope_is_a_journal_not_a_batch() -> Result<()> {
    let mut store = Store::with_storage(MemoryStorage::new());
    store.begin()?;
    store.insert(q("alice", "knows", "bob").as_ref())?;
    // Applied as it goes, which is what makes the reads above agree for free.
    assert!(store.contains(q("alice", "knows", "bob").as_ref())?);
    store.rollback();
    assert!(!store.contains(q("alice", "knows", "bob").as_ref())?);
    Ok(())
}

/// The whole point, on the only backend where it can be observed: nothing reaches the disk
/// until the commit, so a process that dies mid-scope leaves the store as it was.
#[cfg(feature = "rocksdb")]
#[test]
fn rocks_a_scope_is_all_or_nothing_on_disk() -> Result<()> {
    use holos_store::RocksStorage;

    let dir = tempfile::tempdir().expect("temp dir");
    {
        let mut store = Store::with_storage(RocksStorage::open(dir.path())?);
        seed(&mut store)?;
        store.begin()?;
        trace(&mut store)?;
        // Dropped without committing — the crash, near enough: the batch was never written,
        // so no amount of recovery can produce a partial commit from it.
        drop(store);
    }
    let reopened = Store::with_storage(RocksStorage::open(dir.path())?);
    let mut fresh = Store::with_storage(RocksStorage::open(
        tempfile::tempdir().expect("temp dir").path(),
    )?);
    seed(&mut fresh)?;
    assert_eq!(
        read(&reopened, &probes())?,
        read(&fresh, &probes())?,
        "an abandoned scope left something behind"
    );

    // And the same scope committed does reach the disk, whole.
    let dir = tempfile::tempdir().expect("temp dir");
    let expected = {
        let mut store = Store::with_storage(RocksStorage::open(dir.path())?);
        seed(&mut store)?;
        store.begin()?;
        trace(&mut store)?;
        store.commit()?;
        read(&store, &probes())?
    };
    let reopened = Store::with_storage(RocksStorage::open(dir.path())?);
    assert_eq!(read(&reopened, &probes())?, expected);
    Ok(())
}

/// A scope and a bulk load both buffer, and interleaving two buffers is how a load ends up
/// half in a batch nobody writes.
#[cfg(feature = "rocksdb")]
#[test]
fn rocks_a_scope_and_a_bulk_load_are_exclusive() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = Store::with_storage(holos_store::RocksStorage::open(dir.path())?);
    store.begin_bulk_load()?;
    assert!(store.begin().is_err(), "a scope opened inside a bulk load");
    store.end_bulk_load()?;

    // And the other way. Refusing only the direction that happens to be checked leaves the
    // other one silently doing the wrong thing.
    store.begin()?;
    assert!(
        store.begin_bulk_load().is_err(),
        "a bulk load started inside a scope"
    );
    store.rollback();
    store.begin_bulk_load()?;
    store.end_bulk_load()?;
    Ok(())
}
