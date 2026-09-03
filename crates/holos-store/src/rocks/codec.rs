//! The on-disk byte encodings.
//!
//! Two rules govern everything here.
//!
//! **Keys are big-endian.** A [`TermId`] is written most-significant byte first, so
//! RocksDB's lexicographic key order *is* numeric `TermId` order. That is what makes a
//! bound pattern prefix a contiguous range, and it is what carries the order-preserving
//! inline encodings of `DESIGN.md` §5 through to the disk: a range filter over
//! `xsd:integer` stays a range scan.
//!
//! **A term's bytes identify it exactly.** The dictionary is keyed by the serialised term,
//! not by a hash of it, so two terms that differ anywhere — including in a lexical form
//! that denotes the same value — get different keys. See [`super::dict`] for the one place
//! that has to bend, and how it stays correct.

use crate::error::{Result, StorageError};
use holos_core::TermId;
use oxrdf::{BaseDirection, BlankNode, Literal, NamedNode, Term, TermRef};

/// Bytes per term id in a key.
pub const ID: usize = 8;

/// Writes a term id big-endian.
#[must_use]
pub fn put_id(id: TermId) -> [u8; ID] {
    id.to_raw().to_be_bytes()
}

/// Reads a term id big-endian.
pub fn read_id(bytes: &[u8]) -> Result<TermId> {
    let arr: [u8; ID] = bytes
        .get(..ID)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| StorageError::corruption("key is shorter than a term id"))?;
    Ok(TermId::from_raw(u64::from_be_bytes(arr)))
}

/// Builds a key from term ids, in the order given.
#[must_use]
pub fn key(ids: &[TermId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * ID);
    for id in ids {
        out.extend_from_slice(&put_id(*id));
    }
    out
}

/// Splits a key back into its term ids.
pub fn split_key<const N: usize>(bytes: &[u8]) -> Result<[TermId; N]> {
    if bytes.len() != N * ID {
        return Err(StorageError::corruption(format!(
            "expected a {}-byte key, got {}",
            N * ID,
            bytes.len()
        )));
    }
    let mut out = [TermId::from_raw(0); N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = read_id(&bytes[i * ID..])?;
    }
    Ok(out)
}

/// The exclusive upper bound for a prefix scan, or `None` when the prefix is empty or
/// all-`0xFF` and the scan therefore runs to the end of the column family.
///
/// RocksDB's iterate-upper-bound is exclusive, so a prefix has to be turned into the next
/// key that does not share it.
#[must_use]
pub fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last != 0xFF {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

// ---------------------------------------------------------------------------------
// Term serialisation
// ---------------------------------------------------------------------------------

const T_IRI: u8 = 0;
const T_BLANK: u8 = 1;
const T_SIMPLE: u8 = 2;
const T_TYPED: u8 = 3;
const T_LANG: u8 = 4;
const T_LANG_DIR: u8 = 5;
const T_TRIPLE: u8 = 6;

/// Serialises a term.
///
/// Triple terms are stored as the ids of their components, not recursively inline, so a
/// deeply nested RDF 1.2 term costs one row per distinct sub-term rather than a blob that
/// grows with depth.
#[must_use]
pub fn put_term(term: TermRef<'_>, triple_components: Option<[TermId; 3]>) -> Vec<u8> {
    let mut out = Vec::new();
    match term {
        TermRef::NamedNode(n) => {
            out.push(T_IRI);
            out.extend_from_slice(n.as_str().as_bytes());
        }
        TermRef::BlankNode(b) => {
            out.push(T_BLANK);
            out.extend_from_slice(b.as_str().as_bytes());
        }
        TermRef::Literal(l) => match (l.language(), l.direction()) {
            (Some(lang), Some(dir)) => {
                out.push(T_LANG_DIR);
                out.push(match dir {
                    BaseDirection::Ltr => 0,
                    BaseDirection::Rtl => 1,
                });
                put_str(&mut out, lang);
                out.extend_from_slice(l.value().as_bytes());
            }
            (Some(lang), None) => {
                out.push(T_LANG);
                put_str(&mut out, lang);
                out.extend_from_slice(l.value().as_bytes());
            }
            (None, _) => {
                let datatype = l.datatype();
                if datatype == oxrdf::vocab::xsd::STRING {
                    out.push(T_SIMPLE);
                    out.extend_from_slice(l.value().as_bytes());
                } else {
                    out.push(T_TYPED);
                    put_str(&mut out, datatype.as_str());
                    out.extend_from_slice(l.value().as_bytes());
                }
            }
        },
        TermRef::Triple(_) => {
            out.push(T_TRIPLE);
            let ids =
                triple_components.expect("a triple term must be serialised with its component ids");
            for id in ids {
                out.extend_from_slice(&put_id(id));
            }
        }
    }
    out
}

/// What [`read_term`] produced: either a complete term, or a triple term that still needs
/// its components resolved.
pub enum StoredTerm {
    /// A term that stands alone.
    Complete(Term),
    /// An RDF 1.2 triple term, as the ids of its subject, predicate and object.
    Triple([TermId; 3]),
}

/// Deserialises a term.
pub fn read_term(bytes: &[u8]) -> Result<StoredTerm> {
    let (&tag, rest) = bytes
        .split_first()
        .ok_or_else(|| StorageError::corruption("empty term row"))?;
    Ok(match tag {
        T_IRI => StoredTerm::Complete(NamedNode::new_unchecked(as_str(rest)?).into()),
        T_BLANK => StoredTerm::Complete(BlankNode::new_unchecked(as_str(rest)?).into()),
        T_SIMPLE => StoredTerm::Complete(Literal::new_simple_literal(as_str(rest)?).into()),
        T_TYPED => {
            let (datatype, value) = read_str(rest)?;
            StoredTerm::Complete(
                Literal::new_typed_literal(as_str(value)?, NamedNode::new_unchecked(datatype))
                    .into(),
            )
        }
        T_LANG => {
            let (lang, value) = read_str(rest)?;
            StoredTerm::Complete(
                Literal::new_language_tagged_literal_unchecked(as_str(value)?, lang).into(),
            )
        }
        T_LANG_DIR => {
            let (&d, rest) = rest
                .split_first()
                .ok_or_else(|| StorageError::corruption("truncated directional literal"))?;
            let direction = match d {
                0 => BaseDirection::Ltr,
                1 => BaseDirection::Rtl,
                other => {
                    return Err(StorageError::corruption(format!(
                        "unknown base direction {other}"
                    )))
                }
            };
            let (lang, value) = read_str(rest)?;
            StoredTerm::Complete(
                Literal::new_directional_language_tagged_literal_unchecked(
                    as_str(value)?,
                    lang,
                    direction,
                )
                .into(),
            )
        }
        T_TRIPLE => StoredTerm::Triple(split_key::<3>(rest)?),
        other => {
            return Err(StorageError::corruption(format!(
                "unknown term tag {other}"
            )))
        }
    })
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let len = u32::try_from(s.len()).expect("a string longer than 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn read_str(bytes: &[u8]) -> Result<(&str, &[u8])> {
    let len: [u8; 4] = bytes
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| StorageError::corruption("truncated length prefix"))?;
    let len = u32::from_be_bytes(len) as usize;
    let s = bytes
        .get(4..4 + len)
        .ok_or_else(|| StorageError::corruption("truncated string"))?;
    Ok((as_str(s)?, &bytes[4 + len..]))
}

fn as_str(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes)
        .map_err(|e| StorageError::corruption(format!("term row is not UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_core::Tag;

    fn id(n: u64) -> TermId {
        TermId::new(Tag::Iri, n)
    }

    #[test]
    fn key_order_matches_term_id_order() {
        // Everything about prefix scanning rests on this.
        let mut ids: Vec<TermId> = vec![
            TermId::new(Tag::Small, 5),
            TermId::new(Tag::Iri, 300),
            TermId::new(Tag::Iri, 2),
            TermId::new(Tag::Integer, 1),
        ];
        ids.sort_unstable();
        let keys: Vec<Vec<u8>> = ids.iter().map(|i| key(&[*i])).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "byte order must reproduce TermId order");
    }

    #[test]
    fn keys_split_back() {
        let k = key(&[id(1), id(2), id(3), id(4)]);
        assert_eq!(k.len(), 32);
        assert_eq!(split_key::<4>(&k).unwrap(), [id(1), id(2), id(3), id(4)]);
        assert!(split_key::<3>(&k).is_err(), "length must be checked");
    }

    #[test]
    fn prefix_bounds() {
        assert_eq!(prefix_upper_bound(&[1, 2, 3]), Some(vec![1, 2, 4]));
        assert_eq!(prefix_upper_bound(&[1, 2, 0xFF]), Some(vec![1, 3]));
        assert_eq!(prefix_upper_bound(&[0xFF, 0xFF]), None);
        assert_eq!(prefix_upper_bound(&[]), None);
    }

    #[test]
    fn terms_round_trip() {
        let cases: Vec<Term> = vec![
            NamedNode::new_unchecked("http://example.com/a").into(),
            BlankNode::new_unchecked("b0").into(),
            Literal::new_simple_literal("plain").into(),
            Literal::new_simple_literal("").into(),
            Literal::new_typed_literal("42", oxrdf::vocab::xsd::INTEGER).into(),
            Literal::new_language_tagged_literal_unchecked("bonjour", "fr").into(),
            Literal::new_directional_language_tagged_literal_unchecked(
                "שלום",
                "he",
                BaseDirection::Rtl,
            )
            .into(),
            // A lexical form that must stay distinct from its canonical twin.
            Literal::new_typed_literal("042", oxrdf::vocab::xsd::INTEGER).into(),
        ];
        for term in &cases {
            let bytes = put_term(term.as_ref(), None);
            match read_term(&bytes).unwrap() {
                StoredTerm::Complete(got) => assert_eq!(&got, term, "round trip {term}"),
                StoredTerm::Triple(_) => panic!("{term} is not a triple term"),
            }
        }
        // Distinct terms must serialise to distinct bytes, or the dictionary conflates them.
        let mut seen = std::collections::HashSet::new();
        for term in &cases {
            assert!(
                seen.insert(put_term(term.as_ref(), None)),
                "collision on {term}"
            );
        }
    }

    #[test]
    fn triple_terms_carry_their_component_ids() {
        let inner = oxrdf::Triple {
            subject: NamedNode::new_unchecked("http://example.com/s").into(),
            predicate: NamedNode::new_unchecked("http://example.com/p"),
            object: Literal::new_simple_literal("o").into(),
        };
        let components = [id(1), id(2), id(3)];
        let outer = Term::Triple(Box::new(inner));
        let bytes = put_term(outer.as_ref(), Some(components));
        match read_term(&bytes).unwrap() {
            StoredTerm::Triple(got) => assert_eq!(got, components),
            StoredTerm::Complete(t) => panic!("expected a triple term, got {t}"),
        }
    }

    #[test]
    fn a_typed_literal_and_a_lang_literal_with_the_same_value_differ() {
        let typed = Literal::new_typed_literal("x", NamedNode::new_unchecked("urn:d"));
        let lang = Literal::new_language_tagged_literal_unchecked("x", "en");
        let (typed, lang): (Term, Term) = (typed.into(), lang.into());
        assert_ne!(
            put_term(typed.as_ref(), None),
            put_term(lang.as_ref(), None),
            "the tag byte must keep these apart"
        );
    }
}
