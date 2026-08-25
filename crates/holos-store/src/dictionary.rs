//! The term dictionary — the state behind [`Tag::Iri`], [`Tag::Literal`],
//! [`Tag::BlankNode`] and [`Tag::TripleTerm`].
//!
//! Everything the inline codec and the vocabulary table decline ends up here, keyed by a
//! dense monotonic index. `DESIGN.md` §6.1 puts this in two RocksDB column families
//! (`id2str` and `str2id`) with merge-operator refcounts; this is the in-memory tier of
//! that design, and the API is deliberately the one a RocksDB-backed implementation would
//! also present — including the [`Result`] on every read, which the in-memory tier never
//! needs and the persistent one will.

use crate::error::{Result, StorageError};
use holos_core::{inline, vocab, Tag, TermId};
use oxrdf::{BlankNode, BlankNodeRef, Literal, LiteralRef, NamedNode, NamedNodeRef, Term, TermRef};
use rustc_hash::FxHashMap;

/// Bidirectional map between RDF terms and dense [`TermId`]s.
///
/// The three dictionary-backed kinds each get their own dense index space, because the tag
/// already separates them and sharing one counter would waste payload range.
#[derive(Debug, Default, Clone)]
pub struct Dictionary {
    iris: Vec<Box<str>>,
    iri_index: FxHashMap<Box<str>, u64>,
    blank_nodes: Vec<Box<str>>,
    blank_node_index: FxHashMap<Box<str>, u64>,
    literals: Vec<Literal>,
    literal_index: FxHashMap<Literal, u64>,
    /// RDF 1.2 triple terms, stored as their component ids. Recursive by construction: a
    /// component may itself be a [`Tag::TripleTerm`] id.
    triple_terms: Vec<[TermId; 3]>,
    triple_term_index: FxHashMap<[TermId; 3], u64>,
}

impl Dictionary {
    /// A dictionary holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct terms this dictionary has had to store.
    ///
    /// Inline and well-known terms are absent by design, so this is smaller — often much
    /// smaller — than the number of distinct terms in the data.
    ///
    /// Infallible because a persistent tier keeps this as a counter, not a scan.
    #[must_use]
    pub fn len(&self) -> usize {
        self.iris.len() + self.blank_nodes.len() + self.literals.len() + self.triple_terms.len()
    }

    /// True when nothing has been interned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Interns a term, allocating an id if it is new.
    pub fn encode(&mut self, term: TermRef<'_>) -> Result<TermId> {
        match term {
            TermRef::NamedNode(n) => self.encode_named_node(n),
            TermRef::BlankNode(b) => self.encode_blank_node(b),
            TermRef::Literal(l) => self.encode_literal(l),
            TermRef::Triple(t) => {
                // Encode the components first: a triple term is identified by the ids of
                // its parts, so nested triple terms intern bottom-up.
                let components = [
                    self.encode(t.subject.as_ref().into())?,
                    self.encode(t.predicate.as_ref().into())?,
                    self.encode(t.object.as_ref())?,
                ];
                if let Some(i) = self.triple_term_index.get(&components) {
                    return Ok(TermId::new(Tag::TripleTerm, *i));
                }
                let i = self.triple_terms.len() as u64;
                self.triple_terms.push(components);
                self.triple_term_index.insert(components, i);
                Ok(TermId::new(Tag::TripleTerm, i))
            }
        }
    }

    /// Interns an IRI.
    pub fn encode_named_node(&mut self, node: NamedNodeRef<'_>) -> Result<TermId> {
        if let Some(id) = vocab::encode_iri(node.as_str()) {
            return Ok(id);
        }
        if let Some(i) = self.iri_index.get(node.as_str()) {
            return Ok(TermId::new(Tag::Iri, *i));
        }
        let i = self.iris.len() as u64;
        let s: Box<str> = node.as_str().into();
        self.iris.push(s.clone());
        self.iri_index.insert(s, i);
        Ok(TermId::new(Tag::Iri, i))
    }

    /// Interns a blank node.
    pub fn encode_blank_node(&mut self, node: BlankNodeRef<'_>) -> Result<TermId> {
        if let Some(i) = self.blank_node_index.get(node.as_str()) {
            return Ok(TermId::new(Tag::BlankNode, *i));
        }
        let i = self.blank_nodes.len() as u64;
        let s: Box<str> = node.as_str().into();
        self.blank_nodes.push(s.clone());
        self.blank_node_index.insert(s, i);
        Ok(TermId::new(Tag::BlankNode, i))
    }

    /// Interns a literal, inlining it if the codec accepts it.
    pub fn encode_literal(&mut self, literal: LiteralRef<'_>) -> Result<TermId> {
        if let Some(id) = inline::encode_literal(literal) {
            return Ok(id);
        }
        if let Some(i) = self.literal_index.get(&literal.into_owned()) {
            return Ok(TermId::new(Tag::Literal, *i));
        }
        let i = self.literals.len() as u64;
        let owned = literal.into_owned();
        self.literals.push(owned.clone());
        self.literal_index.insert(owned, i);
        Ok(TermId::new(Tag::Literal, i))
    }

    /// Looks a term up without interning it.
    ///
    /// `Ok(None)` means the store has never seen the term — query constants take this
    /// path, and a pattern mentioning an unknown term matches nothing. It is distinct from
    /// `Err`, which means the lookup could not be performed at all.
    pub fn lookup(&self, term: TermRef<'_>) -> Result<Option<TermId>> {
        Ok(match term {
            // `unwrap_or` would be wrong here: its argument is evaluated eagerly, so the
            // dictionary miss would fire even when the term is well-known.
            TermRef::NamedNode(n) => match vocab::encode_iri(n.as_str()) {
                Some(id) => Some(id),
                None => self
                    .iri_index
                    .get(n.as_str())
                    .map(|i| TermId::new(Tag::Iri, *i)),
            },
            TermRef::BlankNode(b) => self
                .blank_node_index
                .get(b.as_str())
                .map(|i| TermId::new(Tag::BlankNode, *i)),
            TermRef::Literal(l) => match inline::encode_literal(l) {
                Some(id) => Some(id),
                None => self
                    .literal_index
                    .get(&l.into_owned())
                    .map(|i| TermId::new(Tag::Literal, *i)),
            },
            TermRef::Triple(t) => {
                let (Some(s), Some(p), Some(o)) = (
                    self.lookup(t.subject.as_ref().into())?,
                    self.lookup(t.predicate.as_ref().into())?,
                    self.lookup(t.object.as_ref())?,
                ) else {
                    return Ok(None);
                };
                self.triple_term_index
                    .get(&[s, p, o])
                    .map(|i| TermId::new(Tag::TripleTerm, *i))
            }
        })
    }

    /// Turns an id back into a term.
    ///
    /// `Ok(None)` means this dictionary never issued the id.
    pub fn decode(&self, id: TermId) -> Result<Option<Term>> {
        Ok(match id.tag() {
            Tag::Iri => self
                .iris
                .get(index_of(id)?)
                .map(|s| NamedNode::new_unchecked(s.as_ref()).into()),
            Tag::BlankNode => self
                .blank_nodes
                .get(index_of(id)?)
                .map(|s| BlankNode::new_unchecked(s.as_ref()).into()),
            Tag::Literal => self.literals.get(index_of(id)?).cloned().map(Into::into),
            Tag::Vocab => vocab::decode_iri(id).map(|s| NamedNode::new_unchecked(s).into()),
            Tag::TripleTerm => {
                let Some([s, p, o]) = self.triple_terms.get(index_of(id)?).copied() else {
                    return Ok(None);
                };
                let subject = match self.decode(s)? {
                    Some(Term::NamedNode(n)) => n.into(),
                    Some(Term::BlankNode(b)) => b.into(),
                    // RDF 1.2 forbids literals and triple terms in subject position, so a
                    // stored triple term violating that is damage, not a query result.
                    Some(_) => {
                        return Err(StorageError::corruption(format!(
                            "triple term {id:?} has a non-node subject"
                        )))
                    }
                    None => return Ok(None),
                };
                let Some(Term::NamedNode(predicate)) = self.decode(p)? else {
                    return Err(StorageError::corruption(format!(
                        "triple term {id:?} has a non-IRI predicate"
                    )));
                };
                let Some(object) = self.decode(o)? else {
                    return Ok(None);
                };
                Some(Term::Triple(Box::new(oxrdf::Triple {
                    subject,
                    predicate,
                    object,
                })))
            }
            _ => inline::decode(id),
        })
    }
}

/// A payload that does not fit `usize` cannot index anything this process allocated.
fn index_of(id: TermId) -> Result<usize> {
    usize::try_from(id.payload())
        .map_err(|_| StorageError::corruption(format!("term id {id:?} is out of range")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::vocab::{rdf, xsd};
    use oxrdf::Triple;

    fn round_trip(dict: &mut Dictionary, term: Term) -> TermId {
        let id = dict.encode(term.as_ref()).unwrap();
        assert_eq!(
            dict.decode(id).unwrap().as_ref(),
            Some(&term),
            "round trip {term}"
        );
        assert_eq!(
            dict.lookup(term.as_ref()).unwrap(),
            Some(id),
            "lookup must agree with encode for {term}"
        );
        id
    }

    #[test]
    fn interning_is_stable_and_deduplicating() {
        let mut d = Dictionary::new();
        let a = round_trip(&mut d, NamedNode::new_unchecked("http://example.com/a").into());
        let b = round_trip(&mut d, NamedNode::new_unchecked("http://example.com/b").into());
        let a_again = d
            .encode(NamedNodeRef::new_unchecked("http://example.com/a").into())
            .unwrap();
        assert_eq!(a, a_again, "the same IRI must get the same id");
        assert_ne!(a, b);
        assert_eq!(d.len(), 2, "only two strings should have been stored");
    }

    #[test]
    fn well_known_terms_never_reach_the_dictionary() {
        let mut d = Dictionary::new();
        let id = round_trip(&mut d, rdf::TYPE.into_owned().into());
        assert_eq!(id.tag(), Tag::Vocab);
        assert!(d.is_empty(), "rdf:type must not consume a dictionary slot");
    }

    #[test]
    fn inline_literals_never_reach_the_dictionary() {
        let mut d = Dictionary::new();
        round_trip(&mut d, Literal::new_typed_literal("42", xsd::INTEGER).into());
        round_trip(&mut d, Literal::new_simple_literal("abc").into());
        assert!(d.is_empty(), "inline values must not consume slots");

        // ...but a non-canonical form of the same value must, and must stay distinct.
        let canonical = d
            .encode(LiteralRef::new_typed_literal("42", xsd::INTEGER).into())
            .unwrap();
        let padded = d
            .encode(LiteralRef::new_typed_literal("042", xsd::INTEGER).into())
            .unwrap();
        assert_ne!(canonical, padded, "distinct RDF terms need distinct ids");
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn blank_nodes_and_iris_share_no_id_space() {
        let mut d = Dictionary::new();
        let iri = d
            .encode(NamedNodeRef::new_unchecked("http://example.com/x").into())
            .unwrap();
        let bnode = d.encode(BlankNodeRef::new_unchecked("x").into()).unwrap();
        assert_ne!(iri, bnode);
        assert_eq!(iri.tag(), Tag::Iri);
        assert_eq!(bnode.tag(), Tag::BlankNode);
        // Both are index 0 in their own space, which is exactly what the tag is for.
        assert_eq!(iri.payload(), bnode.payload());
    }

    #[test]
    fn triple_terms_round_trip() {
        let mut d = Dictionary::new();
        let inner = Triple {
            subject: NamedNode::new_unchecked("http://example.com/s").into(),
            predicate: NamedNode::new_unchecked("http://example.com/p"),
            object: Literal::new_simple_literal("o").into(),
        };
        let id = round_trip(&mut d, Term::Triple(Box::new(inner.clone())));
        assert_eq!(id.tag(), Tag::TripleTerm);

        // Deduplicated, and the components are interned once.
        let again = d.encode(TermRef::Triple(&inner)).unwrap();
        assert_eq!(id, again);
    }

    #[test]
    fn nested_triple_terms_round_trip() {
        // RDF 1.2 allows a triple term inside a triple term, in object position.
        let mut d = Dictionary::new();
        let inner = Triple {
            subject: NamedNode::new_unchecked("http://example.com/s").into(),
            predicate: NamedNode::new_unchecked("http://example.com/p"),
            object: Literal::new_simple_literal("o").into(),
        };
        let outer = Triple {
            subject: NamedNode::new_unchecked("http://example.com/claim").into(),
            predicate: rdf::REIFIES.into_owned(),
            object: Term::Triple(Box::new(inner)),
        };
        round_trip(&mut d, Term::Triple(Box::new(outer)));
    }

    #[test]
    fn lookup_of_a_triple_term_with_an_unknown_component_is_a_miss_not_an_error() {
        let d = Dictionary::new();
        let t = Triple {
            subject: NamedNode::new_unchecked("http://example.com/unseen").into(),
            predicate: NamedNode::new_unchecked("http://example.com/p"),
            object: Literal::new_simple_literal("o").into(),
        };
        assert_eq!(d.lookup(TermRef::Triple(&t)).unwrap(), None);
    }

    #[test]
    fn lookup_does_not_intern() {
        let d = Dictionary::new();
        assert_eq!(
            d.lookup(NamedNodeRef::new_unchecked("http://example.com/unseen").into())
                .unwrap(),
            None
        );
        assert_eq!(d.len(), 0, "lookup must leave the dictionary untouched");
    }

    #[test]
    fn decode_rejects_ids_it_did_not_issue() {
        let d = Dictionary::new();
        assert_eq!(d.decode(TermId::new(Tag::Iri, 99)).unwrap(), None);
        assert_eq!(d.decode(TermId::new(Tag::TripleTerm, 0)).unwrap(), None);
        assert_eq!(d.decode(TermId::new(Tag::Ephemeral, 0)).unwrap(), None);
    }
}
