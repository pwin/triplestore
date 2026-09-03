//! Canonical JSON, for deciding when two `rdf:JSON` literals denote one value.
//!
//! RDF 1.2 gives `rdf:JSON` a value space of JSON *values*, not of the text that spells them,
//! so `{ "a":0, "b":1 }` and `{ "b":1, "a":0 }` are the same literal and `[ -0, 0 ]` and
//! `[ 0, -0 ]` are not. Deciding that needs the text parsed and re-emitted in a form where
//! equal values are equal strings.
//!
//! Hand-rolled rather than pulled in, for one reason that outweighs the hundred lines: the
//! number rule here is not the usual one. JSON numbers denote IEEE 754 doubles, and this has
//! to keep `-0` distinct from `0` while making `1E400` and `1E401` identical — both are
//! `+Infinity` — and `9007199254740992.5` identical to `9007199254740991.5`, which round to
//! one double. A serialiser that prints numbers back as decimal gets every one of those
//! wrong. Keying on the *bits* gets all four right at once, and it is the same trick this
//! crate already uses for `xsd:double`.
//!
//! Not a general JSON library. It parses what the suite writes and answers one question.

use std::fmt::Write as _;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Held as the double it denotes, since that is the value space.
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Members in the order written; canonicalisation sorts them.
    Object(Vec<(String, Json)>),
}

/// Parses JSON text, or `None` if it is not JSON.
///
/// `None` is not a failure to report: an `rdf:JSON` literal whose lexical form is not JSON is
/// ill-formed, and the caller decides what that means.
#[must_use]
pub fn parse(text: &str) -> Option<Json> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    p.space();
    let value = p.value()?;
    p.space();
    p.at.eq(&p.bytes.len()).then_some(value)
}

/// The canonical form of a value: equal values, equal strings.
#[must_use]
pub fn canonical(value: &Json) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

/// Canonical JSON for a literal's lexical form, or `None` if it is not JSON.
#[must_use]
pub fn canonical_text(text: &str) -> Option<String> {
    parse(text).as_ref().map(canonical)
}

fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        // By bits, so `-0` and `0` stay apart and everything that rounds together comes
        // together. See the module note.
        Json::Number(n) => {
            let _ = write!(out, "n{:016x}", n.to_bits());
        }
        Json::String(s) => {
            let _ = write!(out, "{s:?}");
        }
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // Sorted: a JSON object is a set of members, so the order they were written in is not
        // part of what it denotes. Duplicate keys are kept rather than resolved — the suite
        // does not write any, and picking a winner would be inventing a rule.
        Json::Object(members) => {
            let mut members: Vec<&(String, Json)> = members.iter().collect();
            members.sort_by(|a, b| a.0.cmp(&b.0));
            out.push('{');
            for (i, (key, value)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{key:?}:");
                write_canonical(value, out);
            }
            out.push('}');
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            return true;
        }
        false
    }

    fn word(&mut self, word: &str) -> bool {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            return true;
        }
        false
    }

    fn value(&mut self) -> Option<Json> {
        self.space();
        match *self.bytes.get(self.at)? {
            b'n' => self.word("null").then_some(Json::Null),
            b't' => self.word("true").then_some(Json::Bool(true)),
            b'f' => self.word("false").then_some(Json::Bool(false)),
            b'"' => self.string().map(Json::String),
            b'[' => self.array(),
            b'{' => self.object(),
            _ => self.number(),
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.eat(b']') {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.space();
            if self.eat(b']') {
                return Some(Json::Array(items));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.at += 1;
        let mut members = Vec::new();
        self.space();
        if self.eat(b'}') {
            return Some(Json::Object(members));
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            if !self.eat(b':') {
                return None;
            }
            members.push((key, self.value()?));
            self.space();
            if self.eat(b'}') {
                return Some(Json::Object(members));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out = String::new();
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    let escape = *self.bytes.get(self.at)?;
                    self.at += 1;
                    out.push(match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hex = self.bytes.get(self.at..self.at + 4)?;
                            self.at += 4;
                            let code =
                                u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                            // A lone surrogate is not a character. The suite writes none, and
                            // rejecting is better than substituting something else.
                            char::from_u32(code)?
                        }
                        _ => return None,
                    });
                }
                // Multi-byte UTF-8 arrives a byte at a time; gather the continuation bytes.
                _ if byte < 0x80 => out.push(byte as char),
                _ => {
                    let start = self.at - 1;
                    while matches!(self.bytes.get(self.at), Some(b) if b & 0xc0 == 0x80) {
                        self.at += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.bytes[start..self.at]).ok()?);
                }
            }
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.at;
        if self.eat(b'-') {}
        while matches!(self.bytes.get(self.at), Some(b) if b.is_ascii_digit()) {
            self.at += 1;
        }
        if self.eat(b'.') {
            while matches!(self.bytes.get(self.at), Some(b) if b.is_ascii_digit()) {
                self.at += 1;
            }
        }
        if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            if !self.eat(b'+') {
                let _ = self.eat(b'-');
            }
            while matches!(self.bytes.get(self.at), Some(b) if b.is_ascii_digit()) {
                self.at += 1;
            }
        }
        if self.at == start {
            return None;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()?
            .parse::<f64>()
            .ok()
            .map(Json::Number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &str, b: &str) -> bool {
        let (Some(a), Some(b)) = (canonical_text(a), canonical_text(b)) else {
            panic!("both should parse");
        };
        a == b
    }

    #[test]
    fn an_object_is_a_set_of_members() {
        assert!(same(r#"{ "a":0, "b":1 }"#, r#"{ "b":1, "a":0 }"#));
        assert!(!same(r#"{ "a":0 }"#, r#"{ "a":1 }"#));
    }

    #[test]
    fn an_array_is_a_sequence() {
        assert!(!same("[ -0, 0 ]", "[ 0, -0 ]"));
        assert!(same("[1,2,3]", "[ 1 , 2 , 3 ]"));
    }

    /// The four number cases the bit key exists for, and which a decimal re-serialisation
    /// gets wrong.
    #[test]
    fn numbers_are_the_doubles_they_denote() {
        assert!(!same("0", "-0"), "signed zeroes are distinct values");
        assert!(same("1E400", "1E401"), "both overflow to the same infinity");
        assert!(
            same("9007199254740992.5", "9007199254740991.5"),
            "both round to one double"
        );
        assert!(
            !same("9007199254740990.5", "9007199254740991.5"),
            "and these do not"
        );
        assert!(same("1.0", "1"), "same value, different spelling");
        assert!(same("1e2", "100"));
    }

    #[test]
    fn strings_and_escapes() {
        assert!(same(r#""aAb""#, r#""aAb""#));
        assert!(!same(r#""a""#, r#""b""#));
        assert!(same(r#""café""#, "\"caf\u{e9}\""));
    }

    #[test]
    fn nesting_survives() {
        assert!(same(
            r#"{ "x":[1,{"p":true,"q":null}], "y":{} }"#,
            r#"{ "y":{}, "x":[1,{"q":null,"p":true}] }"#
        ));
        assert!(!same(r#"{"x":[1,2]}"#, r#"{"x":[2,1]}"#));
    }

    #[test]
    fn text_that_is_not_json_is_declined() {
        for bad in ["", "{", "[1,]", "tru", "{\"a\"}", "1 2", "\"unterminated"] {
            assert!(canonical_text(bad).is_none(), "{bad:?} is not JSON");
        }
    }
}
