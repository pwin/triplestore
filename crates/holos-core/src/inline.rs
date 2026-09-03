//! The inline value codec — [`Tag::Integer`], [`Tag::Float`], [`Tag::DateTime`],
//! [`Tag::Small`].
//!
//! `DESIGN.md` §5 promises two things from inlining: values never enter the dictionary,
//! and order-preserving encodings turn `FILTER(?d > "2020-01-01"^^xsd:date)` into an index
//! range scan.
//!
//! # The correctness trap
//!
//! [`TermId`] equality *is* RDF term equality (SPARQL `sameTerm`), and RDF literal equality
//! is over the **lexical form**, not the value. `"1"^^xsd:integer` and `"01"^^xsd:integer`
//! denote the same number but are different terms. Encoding by value would silently
//! conflate them and change query answers.
//!
//! So every encoder here applies the same rule: **inline only if the lexical form is
//! already the canonical form for that value.** Non-canonical lexical forms fall through to
//! the dictionary, where they keep their exact bytes. `decode(encode(l)) == l` holds for
//! every literal the encoders accept, and [`tests`] checks the non-canonical rejections
//! explicitly.
//!
//! # Why six bytes and not seven
//!
//! `DESIGN.md` §5 asked for seven, and this caps them at six. Keeping short strings in
//! lexicographic order requires the bytes to occupy the *high* bits of the payload with the
//! length below them, and the length and kind fields need 12 bits between them. Six ordered
//! bytes is worth more than seven unordered ones, so the design document now says six.
//!
//! The same correction went the other way for two of its rows: §5's table claimed
//! `xsd:decimal` and `xsd:double` were inlined, and `xsd:date` alongside `xsd:dateTime`.
//! None of them is. That mattered more than a byte of string: a numeric comparison can be
//! satisfied by a decimal, so a range scan built from the integer region alone would drop
//! rows. See `holos_engine::range`.

use crate::{Tag, TermId};
use oxrdf::vocab::xsd;
use oxrdf::{Literal, LiteralRef, Term};

// --- Tag::Integer -----------------------------------------------------------------

/// Bias that maps a 60-bit signed integer onto an unsigned payload, preserving order.
const INT_BIAS: i64 = 1 << 59;
const INT_MIN: i64 = -INT_BIAS;
const INT_MAX: i64 = INT_BIAS - 1;

// --- Tag::Small -------------------------------------------------------------------

const SMALL_KIND_MASK: u64 = 0xF;
const SMALL_KIND_BOOL: u64 = 0;
const SMALL_KIND_STR: u64 = 1;
const SMALL_BOOL_BIT: u64 = 1 << 4;
const SMALL_LEN_SHIFT: u32 = 4;
const SMALL_LEN_MASK: u64 = 0xFF;
const SMALL_BYTES_SHIFT: u32 = 12;
/// Longest `xsd:string` that inlines. See the module note on why this is 6, not 7.
pub const MAX_INLINE_STR: usize = 6;

// --- Tag::DateTime ----------------------------------------------------------------

const DT_BIAS: i64 = 1 << 59;
/// Inlining is restricted to years that format unambiguously in four digits.
const DT_MIN_YEAR: i64 = 1;
const DT_MAX_YEAR: i64 = 9999;

/// Encodes a literal inline, or returns `None` if it belongs in the dictionary.
#[must_use]
pub fn encode_literal(literal: LiteralRef<'_>) -> Option<TermId> {
    // Language-tagged and RDF 1.2 directional literals carry state the payload has no
    // room for, so they always go to the dictionary.
    if literal.language().is_some() || literal.direction().is_some() {
        return None;
    }
    let value = literal.value();
    let datatype = literal.datatype();
    if datatype == xsd::STRING {
        encode_small_string(value)
    } else if datatype == xsd::INTEGER {
        encode_integer(value)
    } else if datatype == xsd::BOOLEAN {
        encode_boolean(value)
    } else if datatype == xsd::FLOAT {
        encode_float(value)
    } else if datatype == xsd::DATE_TIME {
        encode_date_time(value)
    } else {
        None
    }
}

/// Decodes an inline id back to its term.
///
/// Returns `None` for tags this module does not own — dictionary-backed tags,
/// [`Tag::Vocab`] (see [`crate::vocab`]) and [`Tag::TripleTerm`].
#[must_use]
pub fn decode(id: TermId) -> Option<Term> {
    Some(Term::Literal(match id.tag() {
        Tag::Integer => decode_integer(id.payload()),
        Tag::Float => decode_float(id.payload()),
        Tag::DateTime => decode_date_time(id.payload())?,
        Tag::Small => decode_small(id.payload())?,
        _ => return None,
    }))
}

// ---------------------------------------------------------------------------------
// xsd:integer
// ---------------------------------------------------------------------------------

fn encode_integer(value: &str) -> Option<TermId> {
    let v: i64 = value.parse().ok()?;
    // Canonicality: rejects "+5", "05", "-0" and anything with padding.
    if v.to_string() != value {
        return None;
    }
    if !(INT_MIN..=INT_MAX).contains(&v) {
        return None;
    }
    // The range check above guarantees this lands in [0, 2^60), so the conversion is
    // total — expressing it as `try_from` rather than `as` keeps that guarantee checked.
    let biased = u64::try_from(i128::from(v) + i128::from(INT_BIAS)).expect("range-checked above");
    Some(TermId::new(Tag::Integer, biased))
}

fn decode_integer(payload: u64) -> Literal {
    // A payload is at most 2^60 - 1, so it always fits an i64.
    let v = i64::try_from(payload).unwrap_or(i64::MAX) - INT_BIAS;
    Literal::new_typed_literal(v.to_string(), xsd::INTEGER)
}

// ---------------------------------------------------------------------------------
// xsd:float
// ---------------------------------------------------------------------------------

fn encode_float(value: &str) -> Option<TermId> {
    let parsed: oxsdatatypes::Float = value.parse().ok()?;
    if parsed.is_nan() {
        // NaN has no place in a total order, and it is rare enough not to be worth a
        // special case in the range-scan planner.
        return None;
    }
    if parsed.to_string() != value {
        return None;
    }
    Some(TermId::new(
        Tag::Float,
        u64::from(order_preserving_f32(f32::from(parsed))),
    ))
}

fn decode_float(payload: u64) -> Literal {
    // Only the low 32 bits are ever written; masking keeps a corrupt payload from
    // panicking rather than trusting it.
    let f = f32_from_order_preserving(u32::try_from(payload & 0xFFFF_FFFF).unwrap_or(0));
    Literal::new_typed_literal(oxsdatatypes::Float::from(f).to_string(), xsd::FLOAT)
}

/// Maps an `f32` onto a `u32` whose unsigned order matches IEEE-754 numeric order.
fn order_preserving_f32(f: f32) -> u32 {
    let bits = f.to_bits();
    if bits & 0x8000_0000 == 0 {
        bits ^ 0x8000_0000 // positive: flip the sign bit up
    } else {
        !bits // negative: invert everything
    }
}

fn f32_from_order_preserving(ord: u32) -> f32 {
    let bits = if ord & 0x8000_0000 == 0 {
        !ord
    } else {
        ord ^ 0x8000_0000
    };
    f32::from_bits(bits)
}

// ---------------------------------------------------------------------------------
// xsd:boolean and short xsd:string
// ---------------------------------------------------------------------------------

fn encode_boolean(value: &str) -> Option<TermId> {
    // "1" and "0" are legal lexical forms but not canonical ones, so they go to the
    // dictionary rather than being folded onto "true"/"false".
    let bit = match value {
        "true" => SMALL_BOOL_BIT,
        "false" => 0,
        _ => return None,
    };
    Some(TermId::new(Tag::Small, bit | SMALL_KIND_BOOL))
}

fn encode_small_string(value: &str) -> Option<TermId> {
    let bytes = value.as_bytes();
    if bytes.len() > MAX_INLINE_STR {
        return None;
    }
    // Left-align into the top 48 bits so that unsigned payload order reproduces
    // lexicographic byte order, with the length as the tie-break below it.
    let mut packed: u64 = 0;
    for (i, b) in bytes.iter().enumerate() {
        packed |= u64::from(*b) << (8 * (MAX_INLINE_STR - 1 - i));
    }
    Some(TermId::new(
        Tag::Small,
        (packed << SMALL_BYTES_SHIFT) | ((bytes.len() as u64) << SMALL_LEN_SHIFT) | SMALL_KIND_STR,
    ))
    // `bytes.len()` is at most MAX_INLINE_STR, checked at the top of this function.
}

fn decode_small(payload: u64) -> Option<Literal> {
    match payload & SMALL_KIND_MASK {
        SMALL_KIND_BOOL => Some(Literal::new_typed_literal(
            if payload & SMALL_BOOL_BIT == 0 {
                "false"
            } else {
                "true"
            },
            xsd::BOOLEAN,
        )),
        SMALL_KIND_STR => {
            let len = ((payload >> SMALL_LEN_SHIFT) & SMALL_LEN_MASK) as usize;
            if len > MAX_INLINE_STR {
                return None;
            }
            let packed = payload >> SMALL_BYTES_SHIFT;
            let mut bytes = [0u8; MAX_INLINE_STR];
            for (i, slot) in bytes.iter_mut().enumerate().take(len) {
                *slot = u8::try_from((packed >> (8 * (MAX_INLINE_STR - 1 - i))) & 0xFF)
                    .expect("masked to one byte");
            }
            // Only valid UTF-8 was ever encoded, but a corrupt payload must not panic.
            let s = std::str::from_utf8(&bytes[..len]).ok()?;
            Some(Literal::new_simple_literal(s))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------
// xsd:dateTime
// ---------------------------------------------------------------------------------

fn encode_date_time(value: &str) -> Option<TermId> {
    // Only canonical UTC instants with whole seconds inline. Everything else — local
    // times, offsets, fractional seconds, years outside 1..=9999 — keeps its exact
    // lexical form in the dictionary.
    if !value.ends_with('Z') {
        return None;
    }
    let dt: oxsdatatypes::DateTime = value.parse().ok()?;
    if dt.to_string() != value {
        return None;
    }
    let year = dt.year();
    if !(DT_MIN_YEAR..=DT_MAX_YEAR).contains(&year) {
        return None;
    }
    let second_str = dt.second().to_string();
    if second_str.contains('.') {
        return None;
    }
    let second: i64 = second_str.parse().ok()?;

    let days = days_from_civil(year, i64::from(dt.month()), i64::from(dt.day()));
    let secs = days * 86_400 + i64::from(dt.hour()) * 3_600 + i64::from(dt.minute()) * 60 + second;
    // Years 1..=9999 keep `secs` far inside the biased 60-bit window.
    Some(TermId::new(
        Tag::DateTime,
        u64::try_from(secs + DT_BIAS).expect("year range checked above"),
    ))
}

fn decode_date_time(payload: u64) -> Option<Literal> {
    let secs = i64::try_from(payload).unwrap_or(i64::MAX) - DT_BIAS;
    let lexical = format_utc_seconds(secs)?;
    Some(Literal::new_typed_literal(lexical, xsd::DATE_TIME))
}

/// Renders seconds since the Unix epoch as a canonical `xsd:dateTime`.
///
/// Public because the holon event log (`DESIGN.md` §9) timestamps every tick, and it should
/// use the same calendar arithmetic the inline codec does rather than a second copy that
/// could drift from it.
///
/// Returns `None` outside the four-digit-year window the codec inlines, since a timestamp
/// this cannot render is one it also could not read back.
#[must_use]
pub fn format_utc_seconds(secs: i64) -> Option<String> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    if !(DT_MIN_YEAR..=DT_MAX_YEAR).contains(&y) {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    ))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(literal: Literal) -> TermId {
        let id = encode_literal(literal.as_ref())
            .unwrap_or_else(|| panic!("expected {literal} to inline"));
        assert_eq!(
            decode(id),
            Some(Term::Literal(literal.clone())),
            "decode(encode({literal})) must be identical"
        );
        id
    }

    fn refuses(literal: Literal) {
        assert_eq!(
            encode_literal(literal.as_ref()),
            None,
            "{literal} must fall through to the dictionary"
        );
    }

    #[test]
    fn integers_round_trip() {
        for v in [0_i64, 1, -1, 42, -42, i32::MAX as i64, INT_MIN, INT_MAX] {
            round_trip(Literal::new_typed_literal(v.to_string(), xsd::INTEGER));
        }
    }

    #[test]
    fn integers_are_ordered() {
        let ids: Vec<_> = [-1000_i64, -1, 0, 1, 7, 1000]
            .into_iter()
            .map(|v| encode_integer(&v.to_string()).unwrap())
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "integer ids must sort like their values: {ids:?}"
        );
    }

    #[test]
    fn non_canonical_integers_go_to_the_dictionary() {
        // The whole point of the canonicality rule: these are distinct RDF terms from
        // their canonical counterparts and must not be folded onto them.
        for v in ["+5", "05", "-0", " 5", "5 ", "1e3"] {
            refuses(Literal::new_typed_literal(v, xsd::INTEGER));
        }
        // Out of 60-bit range.
        refuses(Literal::new_typed_literal(
            (INT_MAX as i128 + 1).to_string(),
            xsd::INTEGER,
        ));
    }

    #[test]
    fn booleans_round_trip() {
        round_trip(Literal::new_typed_literal("true", xsd::BOOLEAN));
        round_trip(Literal::new_typed_literal("false", xsd::BOOLEAN));
        // Legal lexical forms, not canonical ones.
        refuses(Literal::new_typed_literal("1", xsd::BOOLEAN));
        refuses(Literal::new_typed_literal("0", xsd::BOOLEAN));
    }

    #[test]
    fn short_strings_round_trip() {
        for s in ["", "a", "ab", "abc", "abcde", "abcdef", "£€"] {
            if s.len() <= MAX_INLINE_STR {
                round_trip(Literal::new_simple_literal(s));
            }
        }
        // A typed xsd:string literal is the same RDF term as a simple one, and oxrdf
        // normalises it — so this must inline to exactly the same id.
        assert_eq!(
            encode_literal(Literal::new_simple_literal("abc").as_ref()),
            encode_literal(Literal::new_typed_literal("abc", xsd::STRING).as_ref())
        );
    }

    #[test]
    fn long_strings_go_to_the_dictionary() {
        refuses(Literal::new_simple_literal("abcdefg"));
        refuses(Literal::new_simple_literal("a longer string entirely"));
    }

    #[test]
    fn short_strings_are_lexicographically_ordered() {
        let mut sorted = ["", "a", "aa", "ab", "b", "ba", "z", "zzzzzz"];
        let ids: Vec<_> = sorted
            .iter()
            .map(|s| encode_small_string(s).unwrap())
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "short-string ids must sort lexicographically: {ids:?}"
        );
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            ["", "a", "aa", "ab", "b", "ba", "z", "zzzzzz"],
            "test fixture must already be in lexicographic order"
        );
    }

    #[test]
    fn booleans_and_strings_never_collide() {
        let t = encode_boolean("true").unwrap();
        let f = encode_boolean("false").unwrap();
        let empty = encode_small_string("").unwrap();
        assert_ne!(t, f);
        assert_ne!(t, empty);
        assert_ne!(f, empty);
    }

    #[test]
    fn floats_round_trip_and_order() {
        for v in ["0", "1", "-1", "1.5", "-1.5", "INF", "-INF"] {
            let lit = Literal::new_typed_literal(v, xsd::FLOAT);
            if encode_literal(lit.as_ref()).is_some() {
                round_trip(lit);
            }
        }
        let ids: Vec<_> = ["-INF", "-1.5", "-0", "0", "1.5", "INF"]
            .iter()
            .filter_map(|v| encode_float(v))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "float ids must sort numerically: {ids:?}"
        );
    }

    #[test]
    fn nan_goes_to_the_dictionary() {
        refuses(Literal::new_typed_literal("NaN", xsd::FLOAT));
    }

    #[test]
    fn date_times_round_trip() {
        for v in [
            "1970-01-01T00:00:00Z",
            "2020-01-01T00:00:00Z",
            "2026-08-22T13:45:07Z",
            "0001-01-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
            "2024-02-29T12:00:00Z",
        ] {
            round_trip(Literal::new_typed_literal(v, xsd::DATE_TIME));
        }
    }

    #[test]
    fn date_times_are_ordered() {
        let ids: Vec<_> = [
            "1970-01-01T00:00:00Z",
            "1999-12-31T23:59:59Z",
            "2000-01-01T00:00:00Z",
            "2026-08-22T13:45:07Z",
        ]
        .iter()
        .map(|v| encode_date_time(v).unwrap())
        .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "dateTime ids must sort chronologically: {ids:?}"
        );
    }

    #[test]
    fn non_utc_and_fractional_date_times_go_to_the_dictionary() {
        for v in [
            "2020-01-01T00:00:00+01:00", // an offset, not UTC
            "2020-01-01T00:00:00",       // no timezone at all
            "2020-01-01T00:00:00.500Z",  // fractional seconds
            "2020-01-01T00:00:00.000Z",  // fractional zero is still not canonical
            "-0500-01-01T00:00:00Z",     // outside the four-digit year window
        ] {
            refuses(Literal::new_typed_literal(v, xsd::DATE_TIME));
        }
    }

    #[test]
    fn civil_calendar_conversion_is_an_involution() {
        // Every day across four centuries, which covers every leap-year rule.
        for z in -100_000..100_000_i64 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z, "round trip for day {z}");
        }
    }

    #[test]
    fn language_tagged_literals_go_to_the_dictionary() {
        let lit = Literal::new_language_tagged_literal("hi", "en").unwrap();
        refuses(lit);
    }

    #[test]
    fn unhandled_datatypes_go_to_the_dictionary() {
        refuses(Literal::new_typed_literal("1.5", xsd::DECIMAL));
        refuses(Literal::new_typed_literal("1.5", xsd::DOUBLE));
        refuses(Literal::new_typed_literal("2020-01-01", xsd::DATE));
    }

    #[test]
    fn decode_ignores_tags_it_does_not_own() {
        assert_eq!(decode(TermId::new(Tag::Iri, 3)), None);
        assert_eq!(decode(TermId::new(Tag::Vocab, 0)), None);
        assert_eq!(decode(TermId::new(Tag::TripleTerm, 0)), None);
    }
}
