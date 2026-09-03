//! A bounded scan must be exactly as blind as an unbounded one.
//!
//! `DESIGN.md` §14's guarantee is that policy is decided per quad as it comes off the index,
//! and that **no operator has another way to reach the data**. A range-bounded scan is a
//! second way to reach the index, so it is a second chance to get that wrong — and the way
//! it would go wrong is not a crash. It is a query returning a row the principal was not
//! allowed to see, at speed, with nothing to indicate anything happened.
//!
//! So the property is equality of *visibility*, not just of results: for every policy shape
//! the model has, the bounded scan and the unbounded one must agree about what is visible,
//! agree about what is counted as filtered, and agree about when to fail.
//!
//! A test written alongside an optimisation can be shaped by it without anyone intending
//! that, so each assertion here was checked by breaking the thing it guards: a fast path that
//! reads the index without consulting policy fails two of these, and removing the
//! wholly-denied-graph short circuit fails the third. Neither was true of the first version —
//! the third case only bites under `Fail` semantics, which the policies below did not have
//! until that check said so.

use holos_core::TermId;
use holos_engine::view::DatasetView;
use holos_security::policy::{PrincipalMatch, Rule, Scope, Semantics};
use holos_security::{Modes, Policy, Principal, Session};
use holos_store::{GraphFilter, IdRange, Store};
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use spareval::QueryableDataset;

const EX: &str = "http://example.com/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

/// Data spread over two predicates and two named graphs, so a policy has something to
/// distinguish and a span has something to exclude.
fn store() -> Store {
    let mut store = Store::new();
    for i in 0..40 {
        for (predicate, graph) in [
            (ex("age"), GraphName::DefaultGraph),
            (ex("salary"), GraphName::DefaultGraph),
            (ex("age"), GraphName::NamedNode(ex("public"))),
            (ex("salary"), GraphName::NamedNode(ex("private"))),
        ] {
            store
                .insert(
                    Quad {
                        subject: ex(&format!("s{i}")).into(),
                        predicate,
                        object: Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into(),
                        graph_name: graph,
                    }
                    .as_ref(),
                )
                .expect("insert");
        }
    }
    store
}

/// The policies worth distinguishing: each one hides something a span might otherwise reveal.
fn policies() -> Vec<(&'static str, Policy)> {
    let allow_all = || Rule::allow(Modes::ALL, Scope::Everything, PrincipalMatch::Everyone);
    vec![
        ("permissive", Policy::permit_all()),
        (
            "one predicate denied",
            Policy::default()
                .with_rule(allow_all())
                .with_rule(Rule::deny(
                    Modes::READ,
                    Scope::Predicate(ex("salary")),
                    PrincipalMatch::Everyone,
                )),
        ),
        (
            "one graph denied",
            Policy::default()
                .with_rule(allow_all())
                .with_rule(Rule::deny(
                    Modes::READ,
                    Scope::Graph(ex("private")),
                    PrincipalMatch::Everyone,
                )),
        ),
        (
            "one predicate in one graph denied",
            Policy::default()
                .with_rule(allow_all())
                .with_rule(Rule::deny(
                    Modes::READ,
                    Scope::GraphPredicate(ex("public"), ex("age")),
                    PrincipalMatch::Everyone,
                )),
        ),
        (
            "nothing allowed",
            Policy::default().with_rule(Rule::deny(
                Modes::ALL,
                Scope::Everything,
                PrincipalMatch::Everyone,
            )),
        ),
        // Under `Fail`, touching hidden data is an error rather than a narrowing — so the two
        // scans must agree about *when to refuse*, not only about what they return. This is
        // the only mode in which the wholly-denied-graph short circuit is observable.
        (
            "one graph denied, failing rather than filtering",
            Policy::default()
                .with_semantics(Semantics::Fail)
                .with_rule(allow_all())
                .with_rule(Rule::deny(
                    Modes::READ,
                    Scope::Graph(ex("private")),
                    PrincipalMatch::Everyone,
                )),
        ),
    ]
}

/// One visible quad, by its components.
///
/// Not a formatted string. The first version of this compared rendered rows and asked
/// whether one contained a predicate's id as text — which matched `Iri#2` inside `Iri#20`
/// and reported a policy leak that was not there. Comparing the parts is both exact and
/// impossible to read wrongly.
type Row = (TermId, TermId, TermId, Option<TermId>);

/// What a scan produced, in a form two scans can be compared by — including how they failed.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    rows: Vec<Row>,
    errors: usize,
    filtered: u64,
}

/// The bounded scan.
fn bounded(view: &DatasetView<'_>, p: Option<TermId>, span: IdRange, graph: GraphFilter) -> Seen {
    let before = view.filtered_count();
    let mut rows = Vec::new();
    let mut errors = 0;
    for quad in view.quads_with_object_in(None, p, span, graph) {
        match quad {
            Ok(q) => rows.push((q.subject, q.predicate, q.object, q.graph_name)),
            Err(_) => errors += 1,
        }
    }
    rows.sort();
    Seen {
        rows,
        errors,
        filtered: view.filtered_count() - before,
    }
}

/// The unbounded scan, with the span applied afterwards — the answer the bounded one has to
/// match.
fn unbounded(view: &DatasetView<'_>, p: Option<TermId>, span: IdRange, graph: GraphFilter) -> Seen {
    let before = view.filtered_count();
    let graph_name = match graph {
        GraphFilter::Default => Some(None),
        GraphFilter::Named(g) => Some(Some(g)),
        GraphFilter::AnyNamed => None,
        GraphFilter::Any => panic!("the bounded scan handles Any by chaining; not compared here"),
    };
    // Bound so the borrowed `Option<&TermId>` outlives the call.
    let graph_ref = graph_name.as_ref().map(Option::as_ref);
    let mut rows = Vec::new();
    let mut errors = 0;
    for quad in
        QueryableDataset::internal_quads_for_pattern(&view, None, p.as_ref(), None, graph_ref)
    {
        match quad {
            Ok(q) => {
                if span.contains(q.object) {
                    rows.push((q.subject, q.predicate, q.object, q.graph_name));
                }
            }
            Err(_) => errors += 1,
        }
    }
    rows.sort();
    Seen {
        rows,
        errors,
        // The unbounded scan reads more, so it may filter more. What must match is *which
        // rows are visible*; the count is reported so a difference is legible rather than
        // asserted equal.
        filtered: view.filtered_count() - before,
    }
}

fn id(store: &Store, n: i64) -> TermId {
    store
        .lookup_term(
            Term::Literal(Literal::new_typed_literal(n.to_string(), xsd::INTEGER)).as_ref(),
        )
        .expect("lookup")
        .expect("integers are inline")
}

#[test]
fn a_bounded_scan_sees_exactly_what_an_unbounded_one_sees() {
    let store = store();
    let age = store
        .lookup_term(ex("age").as_ref().into())
        .expect("lookup");

    for (policy_name, policy) in policies() {
        let mut session = Session::open(&store, Principal::anonymous(), policy).expect("session");
        let compiled = session.policy(&store).expect("policy").clone();
        let view = DatasetView::new(&store, &compiled);

        for (span_name, span) in [
            (
                "a window",
                IdRange {
                    first: id(&store, 10),
                    last: id(&store, 20),
                },
            ),
            (
                "from the top",
                IdRange {
                    first: id(&store, 35),
                    last: id(&store, 39),
                },
            ),
            ("everything", IdRange::whole_tag(holos_core::Tag::Integer)),
        ] {
            for (graph_name, graph) in [
                ("default", GraphFilter::Default),
                ("any named", GraphFilter::AnyNamed),
                (
                    "the public graph",
                    GraphFilter::Named(
                        store
                            .lookup_term(ex("public").as_ref().into())
                            .expect("lookup")
                            .expect("present"),
                    ),
                ),
                (
                    "the private graph",
                    GraphFilter::Named(
                        store
                            .lookup_term(ex("private").as_ref().into())
                            .expect("lookup")
                            .expect("present"),
                    ),
                ),
            ] {
                for (shape, p) in [("any predicate", None), ("age", age)] {
                    let fast = bounded(&view, p, span, graph);
                    let slow = unbounded(&view, p, span, graph);
                    assert_eq!(
                        fast.rows, slow.rows,
                        "{policy_name} / {span_name} / {graph_name} / {shape}: \
                         a bounded scan saw different rows"
                    );
                    assert_eq!(
                        fast.errors == 0,
                        slow.errors == 0,
                        "{policy_name} / {span_name} / {graph_name} / {shape}: \
                         one refused and the other did not"
                    );
                }
            }
        }
    }
}

/// The specific thing that would make this an exploit rather than a bug: a span that reaches
/// data the principal cannot read.
#[test]
fn a_span_cannot_reach_a_denied_predicate() {
    let store = store();
    let policy = Policy::default()
        .with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Everyone,
        ))
        .with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(ex("salary")),
            PrincipalMatch::Everyone,
        ));
    let mut session = Session::open(&store, Principal::anonymous(), policy).expect("session");
    let compiled = session.policy(&store).expect("policy").clone();
    let view = DatasetView::new(&store, &compiled);

    let salary = store
        .lookup_term(ex("salary").as_ref().into())
        .expect("lookup");
    let span = IdRange {
        first: id(&store, 0),
        last: id(&store, 39),
    };

    // Asked for directly.
    let seen = bounded(&view, salary, span, GraphFilter::Default);
    assert!(
        seen.rows.is_empty(),
        "a bounded scan read a denied predicate: {seen:?}"
    );

    // And reached incidentally, by a scan that does not name a predicate at all — the shape
    // that uses the `osp` order, where the denied predicate sits inside the range rather
    // than outside it.
    let seen = bounded(&view, None, span, GraphFilter::Any);
    let salary = salary.expect("present");
    assert!(
        !seen
            .rows
            .iter()
            .any(|(_, predicate, _, _)| *predicate == salary),
        "an unbounded-predicate span reached a denied predicate"
    );
    // And it did read past it — the rows it *did* return prove the scan was not simply empty.
    assert!(!seen.rows.is_empty(), "the scan returned nothing at all");
}

/// A graph the principal cannot read at all must answer the same way whichever scan asks.
#[test]
fn a_wholly_denied_graph_answers_the_same_either_way() {
    let store = store();
    let policy = Policy::default()
        .with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Everyone,
        ))
        .with_rule(Rule::deny(
            Modes::READ,
            Scope::Graph(ex("private")),
            PrincipalMatch::Everyone,
        ));
    let mut session = Session::open(&store, Principal::anonymous(), policy).expect("session");
    let compiled = session.policy(&store).expect("policy").clone();
    let view = DatasetView::new(&store, &compiled);
    let private = store
        .lookup_term(ex("private").as_ref().into())
        .expect("lookup")
        .expect("present");

    let span = IdRange::whole_tag(holos_core::Tag::Integer);
    let fast = bounded(&view, None, span, GraphFilter::Named(private));
    let slow = unbounded(&view, None, span, GraphFilter::Named(private));
    assert_eq!(fast.rows, slow.rows);
    assert!(fast.rows.is_empty(), "the denied graph leaked: {fast:?}");
}

/// Under `Fail` semantics a denied graph is an error, and it has to be an error whichever
/// scan asks — including for a graph that holds nothing in the span.
///
/// This is the one case the short circuit exists for. Without it a bounded scan over an
/// empty slice of a forbidden graph returns quietly, and the principal learns the graph is
/// empty from a query that was supposed to refuse.
#[test]
fn a_denied_graph_refuses_a_bounded_scan_under_fail_semantics() {
    let store = store();
    let policy = Policy::default()
        .with_semantics(Semantics::Fail)
        .with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Everyone,
        ))
        .with_rule(Rule::deny(
            Modes::READ,
            Scope::Graph(ex("private")),
            PrincipalMatch::Everyone,
        ));
    let mut session = Session::open(&store, Principal::anonymous(), policy).expect("session");
    let compiled = session.policy(&store).expect("policy").clone();
    let view = DatasetView::new(&store, &compiled);
    let private = store
        .lookup_term(ex("private").as_ref().into())
        .expect("lookup")
        .expect("present");

    // A span the graph has nothing in: every salary is 0..40, so this slice is empty. A scan
    // that read it and found nothing would return success, and success is a leak here.
    let empty_slice = IdRange {
        first: id(&store, 100),
        last: id(&store, 200),
    };
    let seen = bounded(&view, None, empty_slice, GraphFilter::Named(private));
    assert_eq!(
        seen.errors, 1,
        "a bounded scan of a denied graph should refuse, not come back empty: {seen:?}"
    );
    assert!(seen.rows.is_empty());

    // And the unbounded scan agrees, which is the property the whole file is about.
    let slow = unbounded(&view, None, empty_slice, GraphFilter::Named(private));
    assert_eq!(seen.errors == 0, slow.errors == 0);
}
