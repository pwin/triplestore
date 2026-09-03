//! A bounded scan must return exactly what scanning and filtering returns.
//!
//! `DESIGN.md` §5 gives inline integers, floats and dateTimes ids whose numeric order is the
//! value's order, so `FILTER(?d > "2020-01-01")` can bound the index instead of testing
//! everything it hands back. That is only worth having if the bound is *exact*: a scan that
//! is fast and quietly drops a row is worse than a slow one, because the row it dropped
//! looks like data that was never there.
//!
//! So the property here is equality with the thing it replaces — for every span, on both
//! backends, in every graph shape. Two sources of error it is aimed at:
//!
//! - **The bound is off by one.** RocksDB's iterate bound is exclusive and a span is
//!   inclusive, so the endpoints are where this breaks, and every case below includes them.
//! - **The wrong index order.** A span can only bound a component the key puts first, or
//!   first after a bound prefix. Choosing `pos` where the predicate is unbound would scan a
//!   contiguous range of the wrong thing and return a plausible, wrong answer.

use holos_core::{Tag, TermId};
use holos_store::{GraphFilter, IdRange, Result, Store};
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad};

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

/// Ages 0..60 across two graphs and two predicates, plus values a numeric span must not
/// silently swallow.
fn fixture() -> Vec<Quad> {
    let mut quads = Vec::new();
    for i in 0..60 {
        let s = ex(&format!("p{i}"));
        quads.push(Quad {
            subject: s.clone().into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        });
        // A second predicate over the same value space: a span that forgot to bound the
        // predicate would pick these up too.
        quads.push(Quad {
            subject: s.clone().into(),
            predicate: ex("shoe"),
            object: Literal::new_typed_literal((i % 15).to_string(), xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        });
        quads.push(Quad {
            subject: s.clone().into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal((60 - i).to_string(), xsd::INTEGER).into(),
            graph_name: GraphName::NamedNode(ex(&format!("g{}", i % 3))),
        });
    }
    // Negative integers, which the biased encoding puts below zero and a naive unsigned
    // bound would put above everything.
    for i in 1..8 {
        quads.push(Quad {
            subject: ex(&format!("below{i}")).into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal(format!("-{i}"), xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        });
    }
    // Not integers at all. A numeric span must not reach them, and a span over the whole
    // dictionary tag must.
    quads.push(Quad {
        subject: ex("odd").into(),
        predicate: ex("age"),
        object: Literal::new_simple_literal("not a number").into(),
        graph_name: GraphName::DefaultGraph,
    });
    quads.push(Quad {
        subject: ex("odd").into(),
        predicate: ex("age"),
        object: Literal::new_typed_literal("41.5", xsd::DECIMAL).into(),
        graph_name: GraphName::DefaultGraph,
    });
    quads
}

fn loaded(store: &mut Store) -> Result<()> {
    for quad in fixture() {
        store.insert(quad.as_ref())?;
    }
    Ok(())
}

/// The id of an inline integer, which is what a span is built from.
fn age(store: &Store, n: i64) -> TermId {
    let literal = Literal::new_typed_literal(n.to_string(), xsd::INTEGER);
    store
        .lookup_term(literal.as_ref().into())
        .expect("lookup")
        .expect("integers are inline, so this exists whether or not the store has seen it")
}

/// What the bounded scan returns.
fn ranged(
    store: &Store,
    s: Option<TermId>,
    p: Option<TermId>,
    span: IdRange,
    graph: GraphFilter,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for quad in store.quads_with_object_in(s, p, span, graph) {
        out.push(store.decode_quad(quad?)?.to_string());
    }
    out.sort();
    Ok(out)
}

/// What scanning and filtering returns — the answer the bounded scan has to match.
fn filtered(
    store: &Store,
    s: Option<TermId>,
    p: Option<TermId>,
    span: IdRange,
    graph: GraphFilter,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for quad in store.quads_for_pattern(s, p, None, graph) {
        let quad = quad?;
        if span.contains(quad.object) {
            out.push(store.decode_quad(quad)?.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Every shape, against both answers.
fn agrees(store: &Store) -> Result<()> {
    let age_p = store.lookup_term(ex("age").as_ref().into())?;
    let g0 = store.lookup_term(ex("g0").as_ref().into())?;
    let subject = store.lookup_term(ex("p7").as_ref().into())?;

    let spans = [
        (
            "a middling window",
            IdRange {
                first: age(store, 10),
                last: age(store, 20),
            },
        ),
        (
            "one value",
            IdRange {
                first: age(store, 42),
                last: age(store, 42),
            },
        ),
        (
            "across zero",
            IdRange {
                first: age(store, -5),
                last: age(store, 5),
            },
        ),
        (
            "open at the top",
            IdRange {
                first: age(store, 55),
                last: TermId::new(Tag::Integer, holos_core::term_id::PAYLOAD_MAX),
            },
        ),
        (
            "open at the bottom",
            IdRange {
                first: TermId::new(Tag::Integer, 0),
                last: age(store, 3),
            },
        ),
        ("every integer", IdRange::whole_tag(Tag::Integer)),
        // The tag whose ids carry no value order at all: everything the inline codec
        // declined, which for a numeric comparison is exactly what must not be missed.
        ("every dictionary literal", IdRange::whole_tag(Tag::Literal)),
        (
            "empty",
            IdRange {
                first: age(store, 20),
                last: age(store, 10),
            },
        ),
    ];

    let graphs = [
        ("default", GraphFilter::Default),
        ("any named", GraphFilter::AnyNamed),
        ("everything", GraphFilter::Any),
    ];

    for (span_name, span) in spans {
        for (graph_name, graph) in graphs {
            for (shape, s, p) in [
                ("bare", None, None),
                ("predicate bound", None, age_p),
                ("subject bound", subject, None),
                ("both bound", subject, age_p),
            ] {
                assert_eq!(
                    ranged(store, s, p, span, graph)?,
                    filtered(store, s, p, span, graph)?,
                    "{span_name} / {graph_name} / {shape}"
                );
            }
        }
        // And one named graph, which uses a different index order again.
        if let Some(g) = g0 {
            assert_eq!(
                ranged(store, None, age_p, span, GraphFilter::Named(g))?,
                filtered(store, None, age_p, span, GraphFilter::Named(g))?,
                "{span_name} / one named graph"
            );
            assert_eq!(
                ranged(store, None, None, span, GraphFilter::Named(g))?,
                filtered(store, None, None, span, GraphFilter::Named(g))?,
                "{span_name} / one named graph / no predicate"
            );
        }
    }
    Ok(())
}

#[test]
fn memory_range_scans_agree_with_filtering() -> Result<()> {
    let mut store = Store::new();
    loaded(&mut store)?;
    agrees(&store)
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocks_range_scans_agree_with_filtering() -> Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = Store::with_storage(holos_store::RocksStorage::open(dir.path())?);
    loaded(&mut store)?;
    agrees(&store)
}

/// The two backends must also agree with *each other*, which catches an error made
/// identically in a scan and its filter.
#[cfg(feature = "rocksdb")]
#[test]
fn the_two_backends_answer_a_span_the_same_way() -> Result<()> {
    let mut memory = Store::new();
    loaded(&mut memory)?;
    let dir = tempfile::tempdir().expect("temp dir");
    let mut rocks = Store::with_storage(holos_store::RocksStorage::open(dir.path())?);
    loaded(&mut rocks)?;

    let span = IdRange {
        first: age(&memory, 10),
        last: age(&memory, 20),
    };
    let p = memory.lookup_term(ex("age").as_ref().into())?;
    assert_eq!(
        ranged(&memory, None, p, span, GraphFilter::Any)?,
        ranged(&rocks, None, p, span, GraphFilter::Any)?
    );
    Ok(())
}

/// A span is a narrowing, not a filter: it says what to *read*, and the endpoints are where
/// an off-by-one lives.
#[test]
fn the_endpoints_of_a_span_are_included() -> Result<()> {
    let mut store = Store::new();
    loaded(&mut store)?;
    let p = store.lookup_term(ex("age").as_ref().into())?;
    let span = IdRange {
        first: age(&store, 10),
        last: age(&store, 12),
    };
    let rows = ranged(&store, None, p, span, GraphFilter::Default)?;
    assert!(rows.iter().any(|r| r.contains("\"10\"")), "{rows:?}");
    assert!(rows.iter().any(|r| r.contains("\"12\"")), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("\"13\"")), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("\"9\"")), "{rows:?}");
    Ok(())
}

/// An integer span must not reach a decimal or a string, because their ids carry a different
/// tag — which is exactly why a *numeric* comparison needs more than the integer span.
#[test]
fn an_integer_span_holds_only_integers() -> Result<()> {
    let mut store = Store::new();
    loaded(&mut store)?;
    let p = store.lookup_term(ex("age").as_ref().into())?;
    let rows = ranged(
        &store,
        None,
        p,
        IdRange::whole_tag(Tag::Integer),
        GraphFilter::Any,
    )?;
    assert!(!rows.iter().any(|r| r.contains("41.5")), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains("not a number")), "{rows:?}");

    // And the dictionary span holds exactly those two.
    let rows = ranged(
        &store,
        None,
        p,
        IdRange::whole_tag(Tag::Literal),
        GraphFilter::Any,
    )?;
    assert!(rows.iter().any(|r| r.contains("41.5")), "{rows:?}");
    assert!(rows.iter().any(|r| r.contains("not a number")), "{rows:?}");
    Ok(())
}
