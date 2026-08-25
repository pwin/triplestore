//! Property tests for the inline value codec.
//!
//! The unit tests in `inline.rs` check hand-picked cases. These check the two invariants
//! that must hold for *every* literal, because a single counterexample changes query
//! answers:
//!
//! 1. **Round-trip.** If a literal inlines, decoding its id reproduces it exactly —
//!    same lexical form, same datatype.
//! 2. **Injectivity.** Two literals that are different RDF terms never share an id.
//!
//! Invariant 2 is the one worth generating for. `TermId` equality is SPARQL `sameTerm`,
//! and RDF literal equality is over the *lexical form*: `"1"^^xsd:integer` and
//! `"01"^^xsd:integer` are the same number and different terms. An encoder that folds them
//! together silently changes what a query returns, and no amount of hand-picked cases
//! proves it does not happen somewhere in the literal zoo.

use holos_core::inline::{decode, encode_literal};
use holos_core::TermId;
use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode, Term};
use proptest::prelude::*;
use std::collections::HashMap;

/// The datatypes the codec has an opinion about, plus ones it must decline.
fn datatype() -> impl Strategy<Value = NamedNode> {
    prop_oneof![
        Just(xsd::STRING.into_owned()),
        Just(xsd::INTEGER.into_owned()),
        Just(xsd::BOOLEAN.into_owned()),
        Just(xsd::FLOAT.into_owned()),
        Just(xsd::DATE_TIME.into_owned()),
        // Declined by the codec, so these must always reach the dictionary.
        Just(xsd::DECIMAL.into_owned()),
        Just(xsd::DOUBLE.into_owned()),
        Just(xsd::DATE.into_owned()),
        Just(NamedNode::new_unchecked("http://example.com/custom")),
    ]
}

/// Lexical forms that stress the canonicality rule: valid and invalid, canonical and not.
fn lexical_form() -> impl Strategy<Value = String> {
    prop_oneof![
        // Free-form text, including the empty string and multi-byte characters.
        "[\\PC]{0,12}",
        // Integers, with and without the padding that makes them non-canonical.
        any::<i64>().prop_map(|v| v.to_string()),
        any::<i64>().prop_map(|v| format!("{v:+}")),
        any::<i32>().prop_map(|v| format!("{v:08}")),
        // Boundary values for the 60-bit inline integer window.
        Just((1_i64 << 59).to_string()),
        Just((-(1_i64 << 59)).to_string()),
        Just(((1_i64 << 59) - 1).to_string()),
        // Floats and their special values.
        any::<f32>().prop_map(|v| v.to_string()),
        Just("INF".to_owned()),
        Just("-INF".to_owned()),
        Just("NaN".to_owned()),
        // Booleans, canonical and legal-but-not.
        Just("true".to_owned()),
        Just("false".to_owned()),
        Just("1".to_owned()),
        Just("0".to_owned()),
        // Instants: UTC, offset, fractional, out of the inline year window.
        Just("2020-01-01T00:00:00Z".to_owned()),
        Just("2020-01-01T00:00:00.000Z".to_owned()),
        Just("2020-01-01T00:00:00+01:00".to_owned()),
        Just("2020-01-01T00:00:00".to_owned()),
        Just("0001-01-01T00:00:00Z".to_owned()),
        Just("9999-12-31T23:59:59Z".to_owned()),
        Just("-0500-01-01T00:00:00Z".to_owned()),
        (1i32..10000, 1u32..13, 1u32..29, 0u32..24, 0u32..60, 0u32..60).prop_map(
            |(y, m, d, h, mi, s)| format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        ),
    ]
}

fn literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        // Typed and simple literals.
        (lexical_form(), datatype()).prop_map(|(v, d)| Literal::new_typed_literal(v, d)),
        // Language-tagged literals, which must never inline.
        (lexical_form(), prop_oneof![Just("en"), Just("fr"), Just("de-CH")])
            .prop_map(|(v, tag)| Literal::new_language_tagged_literal_unchecked(v, tag)),
    ]
}

proptest! {
    /// Anything the codec accepts must decode back to exactly the same term.
    #[test]
    fn accepted_literals_round_trip(lit in literal()) {
        if let Some(id) = encode_literal(lit.as_ref()) {
            prop_assert_eq!(
                decode(id),
                Some(Term::Literal(lit.clone())),
                "decode(encode({})) must be identical", lit
            );
        }
    }

    /// Two literals that are different RDF terms must never collide on one id.
    ///
    /// This is the invariant that a value-based encoding would violate, and the reason
    /// every encoder checks canonicality before inlining.
    #[test]
    fn encoding_is_injective(lits in prop::collection::vec(literal(), 1..40)) {
        let mut seen: HashMap<TermId, Literal> = HashMap::new();
        for lit in lits {
            let Some(id) = encode_literal(lit.as_ref()) else { continue };
            if let Some(previous) = seen.insert(id, lit.clone()) {
                prop_assert_eq!(
                    previous.clone(), lit.clone(),
                    "id {:?} was issued for two distinct terms: {} and {}",
                    id, previous, lit
                );
            }
        }
    }

    /// A literal the codec declines must stay declined. Determinism matters because the
    /// dictionary and the codec must agree on which terms each of them owns.
    #[test]
    fn the_decision_to_inline_is_deterministic(lit in literal()) {
        prop_assert_eq!(
            encode_literal(lit.as_ref()).is_some(),
            encode_literal(lit.as_ref()).is_some()
        );
    }

    /// Language-tagged literals carry state the payload has no room for.
    #[test]
    fn language_tagged_literals_never_inline(v in lexical_form()) {
        let lit = Literal::new_language_tagged_literal_unchecked(v, "en");
        prop_assert!(encode_literal(lit.as_ref()).is_none());
    }

    /// Inline integers must sort like the numbers they denote — this is what turns a
    /// range filter into an index scan (DESIGN.md §5).
    #[test]
    fn inline_integers_are_order_preserving(a in any::<i32>(), b in any::<i32>()) {
        let enc = |v: i32| encode_literal(
            Literal::new_typed_literal(v.to_string(), xsd::INTEGER).as_ref()
        );
        let (Some(ida), Some(idb)) = (enc(a), enc(b)) else { return Ok(()) };
        prop_assert_eq!(
            ida.cmp(&idb), a.cmp(&b),
            "ids for {} and {} must sort like the values", a, b
        );
    }

    /// So must inline instants.
    #[test]
    fn inline_date_times_are_order_preserving(
        a in 0i64..253_000_000_000,
        b in 0i64..253_000_000_000,
    ) {
        // Two whole-second UTC instants, rendered canonically.
        let render = |secs: i64| {
            let days = secs.div_euclid(86_400);
            let rem = secs.rem_euclid(86_400);
            // 1970-01-01 plus `days`, via the same civil-calendar arithmetic the codec uses.
            let z = days + 719_468;
            let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60
            )
        };
        let enc = |secs: i64| encode_literal(
            Literal::new_typed_literal(render(secs), xsd::DATE_TIME).as_ref()
        );
        let (Some(ida), Some(idb)) = (enc(a), enc(b)) else { return Ok(()) };
        prop_assert_eq!(ida.cmp(&idb), a.cmp(&b));
    }

    /// And inline short strings, lexicographically.
    #[test]
    fn inline_short_strings_are_order_preserving(a in "[ -~]{0,6}", b in "[ -~]{0,6}") {
        let enc = |s: &str| encode_literal(Literal::new_simple_literal(s).as_ref());
        let (Some(ida), Some(idb)) = (enc(&a), enc(&b)) else { return Ok(()) };
        prop_assert_eq!(
            ida.cmp(&idb), a.as_bytes().cmp(b.as_bytes()),
            "ids for {:?} and {:?} must sort lexicographically", a, b
        );
    }
}
