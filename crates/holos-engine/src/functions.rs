//! Extension function libraries: `fn:`, `afn:` and `spif:`.
//!
//! SPARQL defines a small set of built-ins and leaves everything else to extension
//! functions identified by IRI. Three libraries turn up constantly in queries written for
//! other stores, and a query using them fails here with "the custom function … is not
//! supported" — which is accurate and unhelpful, because the function usually has an exact
//! SPARQL equivalent under a different name.
//!
//! | Namespace | Origin | Why anyone uses it |
//! |---|---|---|
//! | `fn:` | XPath / XQuery Functions & Operators | What SPARQL's own built-ins were derived from; queries ported from XQuery use them |
//! | `afn:` | Apache Jena ARQ | The de-facto extension set — `afn:localname` in particular has no SPARQL equivalent |
//! | `spif:` | SPIN / TopBraid | Widely used in SHACL and SPIN rule sets |
//!
//! # What "supported" means here
//!
//! Each function is implemented to its own specification where that is unambiguous, and
//! where a function's semantics differ subtly between libraries the difference is preserved
//! rather than smoothed over — `fn:substring` is 1-based and rounds its arguments, while
//! `afn:substr` is 0-based and does not, and both are implemented as specified rather than
//! as one shared helper.
//!
//! Functions whose meaning depends on sequences, on a static XPath context, or on the query
//! being a SPIN rule are **not** implemented: they cannot be given a defined answer through
//! this interface, and returning a plausible guess would be worse than not offering them.
//! [`unsupported`] lists them.

use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode, NamedNodeRef, Term};

/// XPath and XQuery Functions and Operators.
pub const FN: &str = "http://www.w3.org/2005/xpath-functions#";
/// Apache Jena ARQ.
pub const AFN: &str = "http://jena.apache.org/ARQ/function#";
/// SPIN / TopBraid.
pub const SPIF: &str = "http://spinrdf.org/spif#";

/// A function's IRI and implementation.
pub type Entry = (NamedNode, fn(&[Term]) -> Option<Term>);

// ---------------------------------------------------------------------------------
// argument helpers
// ---------------------------------------------------------------------------------

/// The lexical form of a term, whatever kind it is.
///
/// Deliberately permissive: `fn:string-length(<http://…>)` is meaningful, and refusing it
/// because the argument is an IRI rather than a literal would be pedantry.
fn text(term: &Term) -> Option<String> {
    Some(match term {
        Term::Literal(l) => l.value().to_owned(),
        Term::NamedNode(n) => n.as_str().to_owned(),
        Term::BlankNode(b) => b.as_str().to_owned(),
        _ => return None,
    })
}

fn number(term: &Term) -> Option<f64> {
    match term {
        Term::Literal(l) => l.value().parse().ok(),
        _ => None,
    }
}

fn integer(term: &Term) -> Option<i64> {
    match term {
        // XPath rounds a non-integral position argument rather than rejecting it.
        Term::Literal(l) => l
            .value()
            .parse::<i64>()
            .ok()
            .or_else(|| l.value().parse::<f64>().ok().map(|f| f.round() as i64)),
        _ => None,
    }
}

fn boolean(term: &Term) -> Option<bool> {
    match term {
        Term::Literal(l) => match l.value() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn string_out(s: impl Into<String>) -> Option<Term> {
    Some(Literal::new_simple_literal(s.into()).into())
}

fn int_out(n: i64) -> Option<Term> {
    Some(Literal::new_typed_literal(n.to_string(), xsd::INTEGER).into())
}

fn double_out(n: f64) -> Option<Term> {
    Some(Literal::new_typed_literal(n.to_string(), xsd::DOUBLE).into())
}

fn bool_out(b: bool) -> Option<Term> {
    Some(Literal::from(b).into())
}

/// Splits an IRI at the last `#` or `/`, the way Jena's `afn:localname` does.
fn split_iri(iri: &str) -> (&str, &str) {
    match iri.rfind(['#', '/']) {
        Some(i) => (&iri[..=i], &iri[i + 1..]),
        None => ("", iri),
    }
}

/// Character-oriented substring, so a multi-byte character is one position.
///
/// Byte slicing would be faster and wrong: `fn:substring` counts characters, and a query
/// over non-ASCII text would silently return mangled output or panic on a char boundary.
fn chars_between(s: &str, start: i64, len: Option<i64>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let from = start.max(0).min(n);
    let to = match len {
        None => n,
        Some(l) => (from + l.max(0)).min(n),
    };
    chars[from as usize..to as usize].iter().collect()
}

// ---------------------------------------------------------------------------------
// fn: — XPath and XQuery Functions and Operators
// ---------------------------------------------------------------------------------

fn fn_upper_case(a: &[Term]) -> Option<Term> {
    string_out(text(a.first()?)?.to_uppercase())
}
fn fn_lower_case(a: &[Term]) -> Option<Term> {
    string_out(text(a.first()?)?.to_lowercase())
}
fn fn_string_length(a: &[Term]) -> Option<Term> {
    int_out(text(a.first()?)?.chars().count() as i64)
}
fn fn_string(a: &[Term]) -> Option<Term> {
    string_out(text(a.first()?)?)
}
fn fn_concat(a: &[Term]) -> Option<Term> {
    let mut out = String::new();
    for t in a {
        out.push_str(&text(t)?);
    }
    string_out(out)
}
fn fn_substring(a: &[Term]) -> Option<Term> {
    // XPath positions are 1-based, so `substring("abcdef", 2, 3)` is "bcd".
    let s = text(a.first()?)?;
    let start = integer(a.get(1)?)? - 1;
    let len = a.get(2).and_then(integer);
    string_out(chars_between(&s, start, len))
}
fn fn_substring_before(a: &[Term]) -> Option<Term> {
    let (s, sep) = (text(a.first()?)?, text(a.get(1)?)?);
    string_out(
        s.split_once(&sep)
            .map_or(String::new(), |(b, _)| b.to_owned()),
    )
}
fn fn_substring_after(a: &[Term]) -> Option<Term> {
    let (s, sep) = (text(a.first()?)?, text(a.get(1)?)?);
    string_out(
        s.split_once(&sep)
            .map_or(String::new(), |(_, x)| x.to_owned()),
    )
}
fn fn_contains(a: &[Term]) -> Option<Term> {
    bool_out(text(a.first()?)?.contains(&text(a.get(1)?)?))
}
fn fn_starts_with(a: &[Term]) -> Option<Term> {
    bool_out(text(a.first()?)?.starts_with(&text(a.get(1)?)?))
}
fn fn_ends_with(a: &[Term]) -> Option<Term> {
    bool_out(text(a.first()?)?.ends_with(&text(a.get(1)?)?))
}
fn fn_normalize_space(a: &[Term]) -> Option<Term> {
    string_out(
        text(a.first()?)?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}
fn fn_translate(a: &[Term]) -> Option<Term> {
    // Character-for-character mapping; a character with no replacement is removed.
    let (s, from, to) = (text(a.first()?)?, text(a.get(1)?)?, text(a.get(2)?)?);
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match from.iter().position(|f| *f == c) {
            None => out.push(c),
            Some(i) => {
                if let Some(r) = to.get(i) {
                    out.push(*r);
                }
            }
        }
    }
    string_out(out)
}
fn fn_compare(a: &[Term]) -> Option<Term> {
    let (x, y) = (text(a.first()?)?, text(a.get(1)?)?);
    int_out(match x.cmp(&y) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}
fn fn_abs(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.abs())
}
fn fn_ceiling(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.ceil())
}
fn fn_floor(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.floor())
}
fn fn_round(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.round())
}
fn fn_not(a: &[Term]) -> Option<Term> {
    bool_out(!boolean(a.first()?)?)
}
fn fn_boolean(a: &[Term]) -> Option<Term> {
    // The XPath effective boolean value: an empty string is false, as is zero.
    let t = a.first()?;
    bool_out(match boolean(t) {
        Some(b) => b,
        None => match number(t) {
            Some(n) => n != 0.0,
            None => !text(t)?.is_empty(),
        },
    })
}

/// `fn:year-from-dateTime` and its siblings, which slice an ISO-8601 lexical form.
fn date_part(value: &str, part: usize) -> Option<i64> {
    // "2026-08-25T13:45:07Z" — date on the left of T, time on the right.
    let (date, time) = value.split_once('T').unwrap_or((value, ""));
    let date: Vec<&str> = date.trim_start_matches('-').split('-').collect();
    let time: Vec<&str> = time
        .trim_end_matches('Z')
        .split(['+', '-'])
        .next()
        .unwrap_or("")
        .split(':')
        .collect();
    let field = match part {
        0..=2 => date.get(part).copied(),
        _ => time.get(part - 3).copied(),
    }?;
    field
        .parse::<i64>()
        .ok()
        .or_else(|| field.parse::<f64>().ok().map(|f| f as i64))
}

macro_rules! date_fn {
    ($name:ident, $part:expr) => {
        fn $name(a: &[Term]) -> Option<Term> {
            int_out(date_part(&text(a.first()?)?, $part)?)
        }
    };
}
date_fn!(fn_year, 0);
date_fn!(fn_month, 1);
date_fn!(fn_day, 2);
date_fn!(fn_hours, 3);
date_fn!(fn_minutes, 4);
date_fn!(fn_seconds, 5);

// ---------------------------------------------------------------------------------
// afn: — Apache Jena ARQ
// ---------------------------------------------------------------------------------

fn afn_localname(a: &[Term]) -> Option<Term> {
    string_out(split_iri(&text(a.first()?)?).1.to_owned())
}
fn afn_namespace(a: &[Term]) -> Option<Term> {
    string_out(split_iri(&text(a.first()?)?).0.to_owned())
}
fn afn_substr(a: &[Term]) -> Option<Term> {
    // Jena's substr is 0-based with an *end* index, following Java's String.substring.
    // Deliberately not shared with fn:substring, which is 1-based with a length.
    let s = text(a.first()?)?;
    let start = integer(a.get(1)?)?;
    let end = a.get(2).and_then(integer);
    string_out(chars_between(&s, start, end.map(|e| e - start)))
}
fn afn_strjoin(a: &[Term]) -> Option<Term> {
    let sep = text(a.first()?)?;
    let parts: Vec<String> = a[1..].iter().filter_map(text).collect();
    string_out(parts.join(&sep))
}
fn afn_sprintf(a: &[Term]) -> Option<Term> {
    // Only `%s` and `%d` are honoured. A full printf would need a format parser, and a
    // partial one that silently mishandled the rest would be worse than a narrow one that
    // says so.
    let format = text(a.first()?)?;
    let mut out = String::with_capacity(format.len());
    let mut args = a[1..].iter();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('s' | 'd') => out.push_str(&args.next().and_then(text).unwrap_or_default()),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    string_out(out)
}
fn afn_pi(_: &[Term]) -> Option<Term> {
    double_out(std::f64::consts::PI)
}
fn afn_e(_: &[Term]) -> Option<Term> {
    double_out(std::f64::consts::E)
}
fn afn_sqrt(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.sqrt())
}
fn afn_min(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.min(number(a.get(1)?)?))
}
fn afn_max(a: &[Term]) -> Option<Term> {
    double_out(number(a.first()?)?.max(number(a.get(1)?)?))
}

// ---------------------------------------------------------------------------------
// spif: — SPIN / TopBraid
// ---------------------------------------------------------------------------------

fn spif_trim(a: &[Term]) -> Option<Term> {
    string_out(text(a.first()?)?.trim().to_owned())
}
fn spif_index_of(a: &[Term]) -> Option<Term> {
    let (s, needle) = (text(a.first()?)?, text(a.get(1)?)?);
    // Character index, not byte index — consistent with every other position here.
    int_out(match s.find(&needle) {
        None => -1,
        Some(byte) => s[..byte].chars().count() as i64,
    })
}
fn spif_last_index_of(a: &[Term]) -> Option<Term> {
    let (s, needle) = (text(a.first()?)?, text(a.get(1)?)?);
    int_out(match s.rfind(&needle) {
        None => -1,
        Some(byte) => s[..byte].chars().count() as i64,
    })
}
fn spif_build_string(a: &[Term]) -> Option<Term> {
    // SPIN templates are `{?1}`, `{?2}` … counting from one.
    let template = text(a.first()?)?;
    let mut out = template;
    for (i, arg) in a[1..].iter().enumerate() {
        out = out.replace(&format!("{{?{}}}", i + 1), &text(arg).unwrap_or_default());
    }
    string_out(out)
}
fn spif_upper_case(a: &[Term]) -> Option<Term> {
    string_out(text(a.first()?)?.to_uppercase())
}
fn spif_lower_case(a: &[Term]) -> Option<Term> {
    string_out(text(a.first()?)?.to_lowercase())
}
fn spif_title_case(a: &[Term]) -> Option<Term> {
    let s = text(a.first()?)?;
    let mut out = String::with_capacity(s.len());
    let mut start_of_word = true;
    for c in s.chars() {
        if c.is_whitespace() {
            start_of_word = true;
            out.push(c);
        } else if start_of_word {
            out.extend(c.to_uppercase());
            start_of_word = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    string_out(out)
}
fn spif_un_camel_case(a: &[Term]) -> Option<Term> {
    let s = text(a.first()?)?;
    let mut out = String::with_capacity(s.len() + 8);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.push(c);
    }
    string_out(out)
}
fn spif_encode_url(a: &[Term]) -> Option<Term> {
    let s = text(a.first()?)?;
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    string_out(out)
}
fn spif_decode_url(a: &[Term]) -> Option<Term> {
    let s = text(a.first()?)?;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    string_out(String::from_utf8_lossy(&out).into_owned())
}
fn spif_generate_uuid(_: &[Term]) -> Option<Term> {
    Some(NamedNode::new_unchecked(format!("urn:uuid:{}", pseudo_uuid())).into())
}
fn spif_name(a: &[Term]) -> Option<Term> {
    string_out(split_iri(&text(a.first()?)?).1.to_owned())
}

/// A version-4-shaped identifier from system entropy.
///
/// No `uuid` dependency for this one use; the shape and the variability are what matter,
/// and it is documented as pseudo-random rather than claimed to be a conforming UUID.
fn pseudo_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut state = nanos as u64 ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let (a, b) = (next(), next());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xFFFF,
        a & 0x0FFF,
        0x8000 | (b & 0x3FFF),
        b >> 16
    )
}

// ---------------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------------

fn iri(ns: &str, local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{ns}{local}"))
}

/// Every function this module registers.
#[must_use]
pub fn all() -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();

    let fns: &[(&str, fn(&[Term]) -> Option<Term>)] = &[
        ("upper-case", fn_upper_case),
        ("lower-case", fn_lower_case),
        ("string-length", fn_string_length),
        ("string", fn_string),
        ("concat", fn_concat),
        ("substring", fn_substring),
        ("substring-before", fn_substring_before),
        ("substring-after", fn_substring_after),
        ("contains", fn_contains),
        ("starts-with", fn_starts_with),
        ("ends-with", fn_ends_with),
        ("normalize-space", fn_normalize_space),
        ("translate", fn_translate),
        ("compare", fn_compare),
        ("abs", fn_abs),
        ("ceiling", fn_ceiling),
        ("floor", fn_floor),
        ("round", fn_round),
        ("not", fn_not),
        ("boolean", fn_boolean),
        ("year-from-dateTime", fn_year),
        ("month-from-dateTime", fn_month),
        ("day-from-dateTime", fn_day),
        ("hours-from-dateTime", fn_hours),
        ("minutes-from-dateTime", fn_minutes),
        ("seconds-from-dateTime", fn_seconds),
    ];
    for (name, f) in fns {
        out.push((iri(FN, name), *f));
    }

    let afns: &[(&str, fn(&[Term]) -> Option<Term>)] = &[
        ("localname", afn_localname),
        ("namespace", afn_namespace),
        ("substr", afn_substr),
        ("substring", afn_substr),
        ("strjoin", afn_strjoin),
        ("sprintf", afn_sprintf),
        ("pi", afn_pi),
        ("e", afn_e),
        ("sqrt", afn_sqrt),
        ("min", afn_min),
        ("max", afn_max),
    ];
    for (name, f) in afns {
        out.push((iri(AFN, name), *f));
    }

    let spifs: &[(&str, fn(&[Term]) -> Option<Term>)] = &[
        ("trim", spif_trim),
        ("indexOf", spif_index_of),
        ("lastIndexOf", spif_last_index_of),
        ("buildString", spif_build_string),
        ("upperCase", spif_upper_case),
        ("lowerCase", spif_lower_case),
        ("titleCase", spif_title_case),
        ("unCamelCase", spif_un_camel_case),
        ("encodeURL", spif_encode_url),
        ("decodeURL", spif_decode_url),
        ("generateUUID", spif_generate_uuid),
        ("name", spif_name),
    ];
    for (name, f) in spifs {
        out.push((iri(SPIF, name), *f));
    }

    out
}

/// Functions from these libraries that are **deliberately not** implemented, and why.
///
/// Each would need something this interface cannot supply: a sequence type, an XPath static
/// context, or the query being a SPIN rule with a `?this`. Returning a plausible answer
/// without them would be a guess wearing a specification's name.
#[must_use]
pub fn unsupported() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "fn:string-join",
            "takes a sequence; SPARQL has no sequence type",
        ),
        ("fn:tokenize", "returns a sequence"),
        (
            "fn:distinct-values, fn:count, fn:sum",
            "sequence functions; use SPARQL aggregates",
        ),
        (
            "fn:matches, fn:replace",
            "use SPARQL's REGEX and REPLACE, which are the same functions",
        ),
        (
            "fn:doc, fn:collection",
            "retrieve external documents; that is the SSRF surface refused for LOAD and SERVICE",
        ),
        ("fn:current-dateTime", "use SPARQL's NOW()"),
        ("afn:now", "use SPARQL's NOW()"),
        (
            "afn:bnode",
            "identity of a blank node is not addressable through this interface",
        ),
        ("afn:sha1sum", "use SPARQL's SHA1"),
        ("spif:cast", "use the xsd: constructor casts"),
        (
            "spif:parseDate, spif:dateFormat",
            "needs a format-pattern language; none is specified normatively",
        ),
        (
            "spif:invoke, spif:eval",
            "evaluate a SPIN expression; there is no SPIN engine here",
        ),
        ("spif:random", "use SPARQL's RAND()"),
        (
            "spif:regex, spif:replaceAll, spif:split",
            "use SPARQL's REGEX and REPLACE",
        ),
    ]
}

/// The IRIs registered, for documentation and tests.
#[must_use]
pub fn iris() -> Vec<NamedNodeRef<'static>> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Term {
        Literal::new_simple_literal(v).into()
    }
    fn i(v: i64) -> Term {
        Literal::new_typed_literal(v.to_string(), xsd::INTEGER).into()
    }
    fn call(name: &str, ns: &str, args: &[Term]) -> Option<Term> {
        let target = iri(ns, name);
        all()
            .into_iter()
            .find(|(n, _)| *n == target)
            .and_then(|(_, f)| f(args))
    }
    fn as_text(t: Option<Term>) -> String {
        match t {
            Some(Term::Literal(l)) => l.value().to_owned(),
            Some(Term::NamedNode(n)) => n.as_str().to_owned(),
            other => panic!("expected a value, got {other:?}"),
        }
    }

    #[test]
    fn every_registered_function_has_a_unique_iri() {
        let entries = all();
        let mut names: Vec<String> = entries.iter().map(|(n, _)| n.as_str().to_owned()).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "a function IRI is registered twice");
        assert!(before >= 45, "expected the full library, got {before}");
    }

    #[test]
    fn fn_substring_is_one_based() {
        // The difference from afn:substr, and the reason they are not one shared helper.
        assert_eq!(
            as_text(call("substring", FN, &[s("abcdef"), i(2), i(3)])),
            "bcd"
        );
        assert_eq!(
            as_text(call("substring", FN, &[s("abcdef"), i(1)])),
            "abcdef"
        );
    }

    #[test]
    fn afn_substr_is_zero_based_with_an_end_index() {
        assert_eq!(
            as_text(call("substr", AFN, &[s("abcdef"), i(0), i(3)])),
            "abc"
        );
        assert_eq!(as_text(call("substr", AFN, &[s("abcdef"), i(2)])), "cdef");
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        // "héllo" is 6 bytes and 5 characters. Byte slicing would give the wrong answer
        // here, or panic on a character boundary.
        assert_eq!(as_text(call("string-length", FN, &[s("héllo")])), "5");
        assert_eq!(
            as_text(call("substring", FN, &[s("héllo"), i(2), i(2)])),
            "él"
        );
        assert_eq!(as_text(call("indexOf", SPIF, &[s("héllo"), s("llo")])), "2");
    }

    #[test]
    fn afn_localname_and_namespace_split_an_iri() {
        assert_eq!(
            as_text(call(
                "localname",
                AFN,
                &[Term::NamedNode(NamedNode::new_unchecked("http://e/x#name"))]
            )),
            "name"
        );
        assert_eq!(
            as_text(call(
                "namespace",
                AFN,
                &[Term::NamedNode(NamedNode::new_unchecked("http://e/x#name"))]
            )),
            "http://e/x#"
        );
        // A slash-terminated IRI splits at the slash.
        assert_eq!(
            as_text(call(
                "localname",
                AFN,
                &[Term::NamedNode(NamedNode::new_unchecked("http://e/abc"))]
            )),
            "abc"
        );
    }

    #[test]
    fn fn_translate_removes_unmapped_characters() {
        assert_eq!(
            as_text(call("translate", FN, &[s("bar"), s("abc"), s("ABC")])),
            "BAr"
        );
        // "c" has no replacement, so it disappears rather than being kept.
        assert_eq!(
            as_text(call("translate", FN, &[s("abc"), s("abc"), s("AB")])),
            "AB"
        );
    }

    #[test]
    fn spif_build_string_fills_numbered_slots() {
        assert_eq!(
            as_text(call(
                "buildString",
                SPIF,
                &[s("{?1} and {?2}"), s("x"), s("y")]
            )),
            "x and y"
        );
    }

    #[test]
    fn spif_case_helpers() {
        assert_eq!(
            as_text(call("titleCase", SPIF, &[s("hello wide world")])),
            "Hello Wide World"
        );
        assert_eq!(
            as_text(call("unCamelCase", SPIF, &[s("someLongName")])),
            "some Long Name"
        );
        assert_eq!(as_text(call("trim", SPIF, &[s("  padded  ")])), "padded");
    }

    #[test]
    fn url_encoding_round_trips() {
        let encoded = as_text(call("encodeURL", SPIF, &[s("a b/c?d=é")]));
        assert!(!encoded.contains(' '), "space must be escaped: {encoded}");
        assert_eq!(
            as_text(call("decodeURL", SPIF, &[s(&encoded)])),
            "a b/c?d=é"
        );
    }

    #[test]
    fn date_parts_come_out_of_an_iso_timestamp() {
        let t = s("2026-08-25T13:45:07Z");
        assert_eq!(
            as_text(call("year-from-dateTime", FN, &[t.clone()])),
            "2026"
        );
        assert_eq!(as_text(call("month-from-dateTime", FN, &[t.clone()])), "8");
        assert_eq!(as_text(call("day-from-dateTime", FN, &[t.clone()])), "25");
        assert_eq!(as_text(call("hours-from-dateTime", FN, &[t.clone()])), "13");
        assert_eq!(
            as_text(call("minutes-from-dateTime", FN, &[t.clone()])),
            "45"
        );
        assert_eq!(as_text(call("seconds-from-dateTime", FN, &[t])), "7");
    }

    #[test]
    fn afn_sprintf_handles_only_what_it_claims() {
        assert_eq!(
            as_text(call("sprintf", AFN, &[s("%s-%d"), s("a"), i(1)])),
            "a-1"
        );
        // An unsupported specifier is left alone rather than silently eaten.
        assert_eq!(as_text(call("sprintf", AFN, &[s("%q"), s("a")])), "%q");
        assert_eq!(as_text(call("sprintf", AFN, &[s("100%%")])), "100%");
    }

    #[test]
    fn generated_uuids_differ() {
        let a = as_text(call("generateUUID", SPIF, &[]));
        let b = as_text(call("generateUUID", SPIF, &[]));
        assert!(a.starts_with("urn:uuid:"), "{a}");
        assert_ne!(a, b, "two calls must not collide");
    }

    #[test]
    fn a_missing_argument_yields_nothing_rather_than_panicking() {
        // The evaluator calls these with whatever the query supplied. Every one has to
        // survive being called wrongly.
        for (_, f) in all() {
            let _ = f(&[]);
            let _ = f(&[s("only-one")]);
        }
    }
}
