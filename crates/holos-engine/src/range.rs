//! Turning `FILTER(?o > k)` into a bound on the index.
//!
//! `DESIGN.md` §5 gives inline integers, floats and dateTimes ids whose numeric order is the
//! value's order. So a comparison against a constant is, in principle, a range of term ids —
//! and [`holos_store::Store::quads_with_object_in`] can read that range instead of reading
//! everything and testing it.
//!
//! # The trap, which is the whole of this module
//!
//! **The span says what to read. The filter still says what matches.** So a span may admit
//! too much — that costs time — but must never admit too little, because what it excludes is
//! not filtered out, it is never seen. A row dropped that way does not look like an error; it
//! looks like data that was never in the store.
//!
//! That makes the interesting question "what could satisfy this comparison", and the answer
//! is wider than it first appears:
//!
//! - **A numeric comparison is not just integers.** The inline codec takes `xsd:integer` and
//!   `xsd:float`, and takes the float only when its canonical form round-trips. Every
//!   `xsd:decimal`, every `xsd:double`, every integer too large for 60 bits and every float
//!   that does not round-trip is a *dictionary* literal, whose id says nothing about its
//!   value. `2.5` and `"1e9999"^^xsd:decimal` are numbers and their ids are wherever the
//!   dictionary put them.
//! - **A dateTime comparison is not just `Tag::DateTime`,** for the same reason: a
//!   non-canonical lexical form goes to the dictionary.
//!
//! So every span this module produces includes the whole dictionary-literal region alongside
//! the ordered part. That sounds like it gives up the win, and it does not, for the reason
//! the inlining exists: in a store where numbers are numbers, the dictionary holds the IRIs
//! and the prose, and the numeric predicate's slice of it is empty. The span is then two
//! reads, one of which finds nothing.
//!
//! # What is deliberately not attempted
//!
//! `<` and `>` on strings. SPARQL orders strings by codepoint and `Tag::Small` orders them
//! by the same bytes, so it would work for short strings — but a longer string is a
//! dictionary literal, and a span admitting *all* of them for a string comparison is the
//! whole predicate. The bound would be sound and worthless, which is worse than not having
//! it, because it would look like an optimisation in a profile.

use holos_core::{Tag, TermId};
use holos_store::{IdRange, Store};
use oxrdf::vocab::xsd;
use oxrdf::{Literal, Term};
use spargebra::algebra::Expression;
use spargebra::term::Variable;

/// A comparison that can bound a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// `?v > k`
    Greater,
    /// `?v >= k`
    GreaterOrEqual,
    /// `?v < k`
    Less,
    /// `?v <= k`
    LessOrEqual,
}

impl Bound {
    /// The same comparison with its operands the other way round.
    ///
    /// `k < ?v` bounds `?v` exactly as `?v > k` does, and a query is as likely to be written
    /// one way as the other.
    const fn flipped(self) -> Self {
        match self {
            Self::Greater => Self::Less,
            Self::GreaterOrEqual => Self::LessOrEqual,
            Self::Less => Self::Greater,
            Self::LessOrEqual => Self::GreaterOrEqual,
        }
    }
}

/// A comparison of one variable against one constant.
#[derive(Debug, Clone)]
pub struct Comparison {
    /// The variable being bounded.
    pub variable: Variable,
    /// How it is bounded.
    pub bound: Bound,
    /// What it is bounded by.
    pub value: Literal,
}

/// Reads a comparison out of an expression, if it is one.
///
/// Only the shape `?v OP literal` and its mirror. Anything else — two variables, an
/// arithmetic expression, a function call — is not a bound on a single variable and is left
/// to the evaluator.
#[must_use]
pub fn comparison(expression: &Expression) -> Option<Comparison> {
    let (bound, left, right) = match expression {
        Expression::Greater(a, b) => (Bound::Greater, a, b),
        Expression::GreaterOrEqual(a, b) => (Bound::GreaterOrEqual, a, b),
        Expression::Less(a, b) => (Bound::Less, a, b),
        Expression::LessOrEqual(a, b) => (Bound::LessOrEqual, a, b),
        _ => return None,
    };
    match (left.as_ref(), right.as_ref()) {
        (Expression::Variable(v), other) => Some(Comparison {
            variable: v.clone(),
            bound,
            value: constant(other)?,
        }),
        (other, Expression::Variable(v)) => Some(Comparison {
            variable: v.clone(),
            bound: bound.flipped(),
            value: constant(other)?,
        }),
        _ => None,
    }
}

/// The literal an expression denotes, if it denotes one at parse time.
///
/// Not just `Expression::Literal`. A SPARQL parser puts the sign of a negative number in the
/// grammar rather than in the literal, so `-5` arrives as `UnaryMinus(Literal("5"))` — and
/// `FILTER(?temp < -5)` is not an unusual query. Reading only the bare form would leave the
/// bound silently not applying to half the number line, which is the kind of gap that shows
/// up as "the optimisation does nothing" long after anyone remembers why.
fn constant(expression: &Expression) -> Option<Literal> {
    match expression {
        Expression::Literal(literal) => Some(literal.clone()),
        Expression::UnaryPlus(inner) => constant(inner),
        Expression::UnaryMinus(inner) => negated(&constant(inner)?),
        _ => None,
    }
}

/// A numeric literal with its sign flipped.
///
/// Rendered by parsing and re-printing rather than by putting a `-` in front of the lexical
/// form: the value inside a `UnaryMinus` can itself be negative — `- -5` is legal — and
/// `--5` is not a number. `None` for anything not numeric, or that does not fit.
fn negated(literal: &Literal) -> Option<Literal> {
    let datatype = literal.datatype();
    if datatype == xsd::INTEGER || datatype == xsd::FLOAT {
        // The sign is flipped on the *lexical* form rather than by parsing and re-printing.
        // The inline codec only accepts a float whose canonical form round-trips, and the
        // form the parser produced is already canonical — re-printing it through an `f32`
        // could land on a different spelling of the same number and be declined for a reason
        // that has nothing to do with the query.
        let text = literal.value();
        let flipped = match text.strip_prefix('-') {
            Some(positive) => positive.to_owned(),
            None => format!("-{text}"),
        };
        Some(Literal::new_typed_literal(flipped, datatype.into_owned()))
    } else {
        // Everything else is declined for the same reason `ordered_tag` declines it: there
        // is no ordered region for a span to cut.
        None
    }
}

/// The spans a scan must read to be sure of finding everything the comparison admits.
///
/// `None` when the comparison cannot bound anything usefully — an unsupported datatype, a
/// value that does not encode, or a case where the sound span would be the whole predicate
/// anyway. A caller that gets `None` scans as it would have.
///
/// The returned spans are disjoint and in ascending order, which is what lets a caller read
/// them one after another without deduplicating.
#[must_use]
pub fn spans(store: &Store, comparison: &Comparison) -> Option<Vec<IdRange>> {
    let ordered = ordered_tag(&comparison.value)?;
    let pivot = store
        .lookup_term(Term::Literal(comparison.value.clone()).as_ref())
        .ok()
        .flatten()?;
    // The value has to encode into the tag whose order this is about. A `1e9999` decimal
    // parses and is a number, but its id is a dictionary id and carries no order — there is
    // nothing to cut the ordered region at.
    //
    // This overlaps with `ordered_tag` on purpose, and a mutation run will report the overlap
    // as dead: widening `ordered_tag` to accept a datatype it should not still fails here,
    // because the value will not encode into that tag. Two guards on the one path where
    // being wrong loses rows silently, rather than one guard and a comment about care.
    if pivot.tag() != ordered {
        return None;
    }

    let ordered_span = match comparison.bound {
        Bound::Greater => IdRange {
            first: next_up(pivot)?,
            last: TermId::new(ordered, holos_core::term_id::PAYLOAD_MAX),
        },
        Bound::GreaterOrEqual => IdRange {
            first: pivot,
            last: TermId::new(ordered, holos_core::term_id::PAYLOAD_MAX),
        },
        Bound::Less => IdRange {
            first: TermId::new(ordered, 0),
            last: next_down(pivot)?,
        },
        Bound::LessOrEqual => IdRange {
            first: TermId::new(ordered, 0),
            last: pivot,
        },
    };
    if ordered_span.is_empty() {
        // Still not nothing: the dictionary region can hold a match even when no inline
        // value can. `?v < -2^59` excludes every inline integer and not every integer.
        return Some(companions(ordered));
    }

    let mut out = companions(ordered);
    out.push(ordered_span);
    out.sort_by_key(|span| span.first);
    Some(out)
}

/// The unordered regions a comparison of this kind must also read.
///
/// Always the dictionary literals, because that is where every value the inline codec
/// declined ended up. For a numeric comparison it is also *the other numeric tag*: an
/// `xsd:integer` and an `xsd:float` are both numbers and compare against each other, and
/// their ids are in different regions entirely.
fn companions(ordered: Tag) -> Vec<IdRange> {
    let mut out = vec![IdRange::whole_tag(Tag::Literal)];
    match ordered {
        Tag::Integer => out.push(IdRange::whole_tag(Tag::Float)),
        Tag::Float => out.push(IdRange::whole_tag(Tag::Integer)),
        // A dateTime compares only against dateTimes; anything else is a type error, which
        // the filter reports and a span cannot help with.
        _ => {}
    }
    out
}

/// Which tag holds this datatype in value order, if any does.
fn ordered_tag(value: &Literal) -> Option<Tag> {
    let datatype = value.datatype();
    if datatype == xsd::INTEGER {
        Some(Tag::Integer)
    } else if datatype == xsd::FLOAT {
        Some(Tag::Float)
    } else if datatype == xsd::DATE_TIME {
        Some(Tag::DateTime)
    } else {
        // Includes xsd:decimal and xsd:double on purpose. They are numbers, and the codec
        // does not inline them, so there is no ordered region for a span to cut — reading
        // the dictionary alone would be the whole predicate.
        None
    }
}

/// The next id above one, within its tag.
fn next_up(id: TermId) -> Option<TermId> {
    let payload = id.payload();
    if payload >= holos_core::term_id::PAYLOAD_MAX {
        return None;
    }
    Some(TermId::new(id.tag(), payload + 1))
}

/// The next id below one, within its tag.
fn next_down(id: TermId) -> Option<TermId> {
    let payload = id.payload();
    if payload == 0 {
        return None;
    }
    Some(TermId::new(id.tag(), payload - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    fn find(p: &spargebra::algebra::GraphPattern) -> Option<Expression> {
        match p {
            spargebra::algebra::GraphPattern::Filter { expr, .. } => Some(expr.clone()),
            spargebra::algebra::GraphPattern::Project { inner, .. }
            | spargebra::algebra::GraphPattern::Distinct { inner }
            | spargebra::algebra::GraphPattern::Slice { inner, .. } => find(inner),
            spargebra::algebra::GraphPattern::Join { left, right } => {
                find(left).or_else(|| find(right))
            }
            _ => None,
        }
    }

    fn filter_of(query: &str) -> Expression {
        let parsed = SparqlParser::new().parse_query(query).expect("parse");
        let spargebra::Query::Select { pattern, .. } = parsed else {
            panic!("expected a SELECT")
        };
        find(&pattern).expect("a filter")
    }

    fn integer(n: i64) -> Literal {
        Literal::new_typed_literal(n.to_string(), xsd::INTEGER)
    }

    fn store() -> Store {
        Store::new()
    }

    #[test]
    fn a_comparison_is_read_from_either_side() {
        let a = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(?o > 30) }")).expect("read");
        assert_eq!(a.bound, Bound::Greater);
        assert_eq!(a.value, integer(30));

        // `30 < ?o` bounds `?o` the same way, and is as natural to write.
        let b = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(30 < ?o) }")).expect("read");
        assert_eq!(b.bound, Bound::Greater);
        assert_eq!(b.value, integer(30));
    }

    #[test]
    fn what_is_not_a_bound_on_one_variable_is_left_alone() {
        for query in [
            "SELECT * { ?s ?p ?o FILTER(?o > ?x) }",
            "SELECT * { ?s ?p ?o FILTER(?o + 1 > 30) }",
            "SELECT * { ?s ?p ?o FILTER(?o = 30) }",
            "SELECT * { ?s ?p ?o FILTER(STRLEN(?o) > 3) }",
        ] {
            assert!(
                comparison(&filter_of(query)).is_none(),
                "should not have read a bound from {query}"
            );
        }
    }

    /// The property the module exists to hold: a span admits everything the filter would.
    #[test]
    fn a_numeric_span_admits_the_other_numeric_tag_and_the_dictionary() {
        let c = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(?o > 30) }")).expect("read");
        let spans = spans(&store(), &c).expect("bounded");

        let integer_30 = TermId::new(Tag::Integer, 0);
        let _ = integer_30;
        // A float and a dictionary literal both have to be reachable, because both can be
        // numbers greater than 30 and neither is in the integer region.
        assert!(
            spans.iter().any(|s| s.contains(TermId::new(Tag::Float, 0))),
            "floats must be admitted: {spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.contains(TermId::new(Tag::Literal, 0))),
            "dictionary literals must be admitted: {spans:?}"
        );
    }

    #[test]
    fn the_ordered_region_is_cut_at_the_right_place() {
        let store = store();
        let c = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(?o > 30) }")).expect("read");
        let spans = spans(&store, &c).expect("bounded");

        let id = |n: i64| {
            store
                .lookup_term(Term::Literal(integer(n)).as_ref())
                .expect("lookup")
                .expect("inline")
        };
        let admits = |n: i64| spans.iter().any(|s| s.contains(id(n)));

        assert!(!admits(30), "30 is not greater than 30");
        assert!(admits(31));
        assert!(admits(1_000_000));
        assert!(!admits(29));
        assert!(!admits(-5));
    }

    #[test]
    fn the_inclusive_forms_include_their_endpoint() {
        let store = store();
        let id = |n: i64| {
            store
                .lookup_term(Term::Literal(integer(n)).as_ref())
                .expect("lookup")
                .expect("inline")
        };

        for (query, at_endpoint) in [
            ("SELECT * { ?s ?p ?o FILTER(?o >= 30) }", true),
            ("SELECT * { ?s ?p ?o FILTER(?o > 30) }", false),
            ("SELECT * { ?s ?p ?o FILTER(?o <= 30) }", true),
            ("SELECT * { ?s ?p ?o FILTER(?o < 30) }", false),
        ] {
            let c = comparison(&filter_of(query)).expect("read");
            let spans = spans(&store, &c).expect("bounded");
            assert_eq!(
                spans.iter().any(|s| s.contains(id(30))),
                at_endpoint,
                "{query}"
            );
        }
    }

    /// `xsd:decimal` and `xsd:double` are numbers the codec does not inline, so there is no
    /// ordered region to cut and the sound span would be the whole predicate.
    #[test]
    fn a_datatype_with_no_ordered_region_is_declined() {
        for query in [
            "SELECT * { ?s ?p ?o FILTER(?o > 2.5) }",
            "SELECT * { ?s ?p ?o FILTER(?o > 1.0e6) }",
            "SELECT * { ?s ?p ?o FILTER(?o > \"abc\") }",
        ] {
            let c = comparison(&filter_of(query)).expect("read");
            assert!(
                spans(&store(), &c).is_none(),
                "should have declined to bound {query}"
            );
        }
    }

    /// dateTimes are ordered, and compare only against dateTimes — so the span is the
    /// ordered region plus the dictionary, and no other inline tag.
    #[test]
    fn a_datetime_span_does_not_admit_numbers() {
        let c = comparison(&filter_of(
            "SELECT * { ?s ?p ?o FILTER(?o > \"2020-01-01T00:00:00Z\"^^\
             <http://www.w3.org/2001/XMLSchema#dateTime>) }",
        ))
        .expect("read");
        let spans = spans(&store(), &c).expect("bounded");
        assert!(spans
            .iter()
            .any(|s| s.contains(TermId::new(Tag::Literal, 0))));
        assert!(
            !spans
                .iter()
                .any(|s| s.contains(TermId::new(Tag::Integer, 5))),
            "a dateTime never compares equal to a number: {spans:?}"
        );
    }

    /// A SPARQL parser puts a negative sign in the grammar, not in the literal, so half the
    /// number line arrives in a shape a naive reader misses entirely.
    #[test]
    fn a_negative_constant_is_read() {
        let c = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(?o > -5) }")).expect("read");
        assert_eq!(c.bound, Bound::Greater);
        assert_eq!(c.value, integer(-5));

        // And it bounds the right way round: -4 is greater than -5, -6 is not.
        let store = store();
        let spans = spans(&store, &c).expect("bounded");
        let id = |n: i64| {
            store
                .lookup_term(Term::Literal(integer(n)).as_ref())
                .expect("lookup")
                .expect("inline")
        };
        assert!(spans.iter().any(|s| s.contains(id(-4))));
        assert!(spans.iter().any(|s| s.contains(id(0))));
        assert!(!spans.iter().any(|s| s.contains(id(-6))));
        assert!(!spans.iter().any(|s| s.contains(id(-5))));
    }

    /// Double negation is legal SPARQL and must not become `--5`.
    #[test]
    fn a_doubly_negated_constant_comes_back_positive() {
        let c = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(?o > - -5) }")).expect("read");
        assert_eq!(c.value, integer(5));
    }

    /// A bound excluding every inline value still has to read the dictionary, because the
    /// dictionary is where the values too large to inline went.
    #[test]
    fn a_bound_below_every_inline_value_still_reads_the_dictionary() {
        let c = comparison(&filter_of(
            "SELECT * { ?s ?p ?o FILTER(?o < -576460752303423488) }",
        ))
        .expect("read");
        match spans(&store(), &c) {
            None => {}
            Some(spans) => assert!(
                spans
                    .iter()
                    .any(|s| s.contains(TermId::new(Tag::Literal, 0))),
                "the dictionary must still be read: {spans:?}"
            ),
        }
    }

    #[test]
    fn spans_come_back_in_order_and_do_not_overlap() {
        let c = comparison(&filter_of("SELECT * { ?s ?p ?o FILTER(?o > 30) }")).expect("read");
        let spans = spans(&store(), &c).expect("bounded");
        for pair in spans.windows(2) {
            assert!(
                pair[0].last < pair[1].first,
                "spans overlap or are unsorted: {spans:?}"
            );
        }
    }
}
