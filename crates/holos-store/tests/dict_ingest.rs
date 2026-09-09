//! A bulk load's dictionary survives being sorted, spilled, merged and ingested.
//!
//! The dictionary rows of a bulk load no longer go through a `WriteBatch`; they are
//! accumulated, sorted, spilled to run files when they outgrow their budget, merged back and
//! handed to `IngestExternalFile` — the same treatment the nine index orders already had.
//!
//! Every way that can go wrong produces a store that **looks** fine. A dropped `str2id` row
//! makes a term intern a second time under a new id, so the same IRI becomes two nodes and a
//! join that should match silently does not. A dropped `id2str` row makes an id undecodable,
//! so a quad that is in every index returns nothing. Neither shows up as an error at load
//! time, which is why the assertions here are about reading every term back rather than
//! about the load returning `Ok`.
//!
//! The spill budget is set to a few bytes so the merge path runs on a few thousand terms
//! instead of a quarter of a gigabyte of them. That is the whole reason
//! `set_dict_spill_bytes` is public.

#![cfg(feature = "rocksdb")]

use holos_store::{RocksStorage, Store};
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};

/// Distinct subjects, and so distinct dictionary entries, for each of several predicates.
const SUBJECTS: usize = 4_000;

fn quads() -> Vec<Quad> {
    let mut out = Vec::new();
    for i in 0..SUBJECTS {
        let subject = NamedNode::new_unchecked(format!("http://example.org/subject/{i}"));
        // A long literal, to push some keys past the 512-byte inline limit and onto the
        // hashed path — the one whose values are candidate lists that have to be merged
        // rather than deduplicated.
        let long = "x".repeat(600);
        out.push(Quad {
            subject: subject.clone().into(),
            predicate: NamedNode::new_unchecked("http://example.org/p"),
            object: Literal::new_simple_literal(format!("{long}{i}")).into(),
            graph_name: GraphName::DefaultGraph,
        });
        out.push(Quad {
            subject: subject.clone().into(),
            predicate: NamedNode::new_unchecked("http://example.org/q"),
            object: NamedNode::new_unchecked(format!("http://example.org/object/{i}")).into(),
            graph_name: GraphName::DefaultGraph,
        });
        // A repeated object, so the term cache is exercised alongside the new terms.
        out.push(Quad {
            subject: subject.into(),
            predicate: NamedNode::new_unchecked("http://example.org/kind"),
            object: NamedNode::new_unchecked("http://example.org/Thing").into(),
            graph_name: GraphName::DefaultGraph,
        });
    }
    out
}

fn load(spill_bytes: usize) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut storage = RocksStorage::open(dir.path()).expect("open");
    storage.set_dict_spill_bytes(spill_bytes);
    let mut store = Store::with_storage(storage);

    store.begin_bulk_load().expect("begin");
    for quad in quads() {
        store.insert(quad.as_ref()).expect("insert");
    }
    store.end_bulk_load().expect("end");
    (dir, store)
}

/// Every term that went in comes back, by id and by lookup, after a load that spilled.
///
/// Both directions matter and they fail differently. `lookup_term` going wrong means the
/// term would be interned again under a second id; `decode_term` going wrong means an id in
/// the index names nothing.
#[test]
fn every_term_round_trips_through_a_spilled_dictionary() {
    // Small enough that four thousand subjects flush a few dozen times over. Each
    // flush ingests and then clears the term cache, so this exercises the path where a
    // repeated term has to be found in an ingested file rather than in memory.
    let (_dir, store) = load(256 * 1024);

    for quad in quads() {
        for term in [
            Term::from(quad.subject.clone()),
            Term::from(quad.predicate.clone()),
            quad.object.clone(),
        ] {
            let id = store
                .lookup_term(term.as_ref())
                .expect("lookup")
                .unwrap_or_else(|| panic!("{term} was not in the dictionary after the load"));
            let back = store
                .decode_term(id)
                .expect("decode")
                .unwrap_or_else(|| panic!("{term} interned as an id that decodes to nothing"));
            assert_eq!(back, term, "a term came back as something else");
        }
    }
}

/// The store holds what was put in it, and holds it once.
#[test]
fn a_spilled_load_stores_every_quad_exactly_once() {
    let (_dir, store) = load(256 * 1024);
    let expected = quads();
    assert_eq!(store.len(), expected.len());
    for quad in expected {
        assert!(
            store.contains(quad.as_ref()).expect("contains"),
            "{quad} was loaded and is not there"
        );
    }
}

/// A term interned before a flush is found after it, not interned again.
///
/// This is the property the whole periodic-flush design rests on. Each flush ingests both
/// dictionary families and then **clears the term cache**, which is only safe because the
/// rows are readable afterwards: `resolve` has to find them. If it did not, every repeated
/// term after the first flush would be handed a second id, and the store would hold two
/// nodes where the data has one — a join that should match would silently not.
///
/// `ex:Thing` is the object of every third quad, so it spans every flush there is. Counting
/// the dictionary is what makes the failure visible: double-interning shows up as a term
/// count that grew, and as quads that multiplied.
#[test]
fn a_term_interned_before_a_flush_is_not_interned_again_after_it() {
    let (_dir, store) = load(256 * 1024);

    // Three per subject: the subject IRI, the long literal, and the object IRI. Plus the
    // three predicates and the one shared `ex:Thing`.
    let expected = SUBJECTS * 3 + 4;
    assert_eq!(
        store.dictionary_len(),
        expected,
        "the dictionary holds {} terms where {expected} were interned, so a term was          given a second id after the cache was cleared",
        store.dictionary_len()
    );
}

/// Spilling must change nothing but memory.
///
/// The comparison is the point: a load that never spills takes the in-memory sort path, and
/// one that spills constantly takes the k-way merge. If those two disagree, the merge is
/// wrong, and no single-configuration test would say so.
#[test]
fn spilling_and_not_spilling_produce_the_same_dictionary() {
    let (_a, spilled) = load(256 * 1024);
    let (_b, whole) = load(usize::MAX);

    assert_eq!(spilled.len(), whole.len());
    for quad in quads() {
        for term in [
            Term::from(quad.subject.clone()),
            Term::from(quad.predicate.clone()),
            quad.object.clone(),
        ] {
            let from_spilled = spilled.lookup_term(term.as_ref()).expect("lookup");
            let from_whole = whole.lookup_term(term.as_ref()).expect("lookup");
            assert!(
                from_spilled.is_some(),
                "{term} missing from the spilled load"
            );
            // The ids themselves are allocation-order and so identical here, but what is
            // being asserted is that both loads can find the term and decode it back.
            assert_eq!(
                spilled
                    .decode_term(from_spilled.expect("id"))
                    .expect("decode"),
                whole.decode_term(from_whole.expect("id")).expect("decode"),
            );
        }
    }
}

/// A second load into the same store must find what the first one interned.
///
/// This is the case the ingest could break most quietly: the first load's rows arrive as an
/// ingested file rather than through the memtable, and if the second load cannot see them it
/// mints fresh ids for terms that already have them.
#[test]
fn a_second_load_reuses_the_first_loads_ids() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut storage = RocksStorage::open(dir.path()).expect("open");
    storage.set_dict_spill_bytes(256 * 1024);
    let mut store = Store::with_storage(storage);

    store.begin_bulk_load().expect("begin");
    for quad in quads() {
        store.insert(quad.as_ref()).expect("insert");
    }
    store.end_bulk_load().expect("end");

    let before: Vec<_> = quads()
        .iter()
        .map(|q| {
            store
                .lookup_term(q.subject.as_ref().into())
                .expect("lookup")
        })
        .collect();
    let quads_before = store.len();

    // The same data again. Nothing is new, so nothing should be interned and nothing added.
    store.begin_bulk_load().expect("begin");
    for quad in quads() {
        store.insert(quad.as_ref()).expect("insert");
    }
    store.end_bulk_load().expect("end");

    assert_eq!(
        store.len(),
        quads_before,
        "re-loading the same data added quads, so terms were interned twice"
    );
    let after: Vec<_> = quads()
        .iter()
        .map(|q| {
            store
                .lookup_term(q.subject.as_ref().into())
                .expect("lookup")
        })
        .collect();
    assert_eq!(before, after, "a term changed id between loads");
}
