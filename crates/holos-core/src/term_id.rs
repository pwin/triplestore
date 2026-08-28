//! Tagged 64-bit term identifiers — HOLOS L1.
//!
//! See `DESIGN.md` §5. The layout is
//!
//! ```text
//! 63..60  tag      (4 bits)
//! 59..0   payload  (60 bits) — a dense dictionary index, or the value itself
//! ```
//!
//! The reason for a dense integer rather than Oxigraph's 128-bit `StrHash` is stated in
//! the design: trie fan-out in the planned Tier B hypertrie is stored as sorted arrays or
//! roaring bitmaps of child ids, which a 128-bit hash forecloses. The dense id is what
//! keeps that door open, and it shrinks Tier A keys from ~64 bytes to 32 at the same time.

use std::fmt;

/// Bit position of the 4-bit tag.
pub const TAG_SHIFT: u32 = 60;
/// Mask covering the 60-bit payload.
pub const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;
/// Largest value a payload can hold.
pub const PAYLOAD_MAX: u64 = PAYLOAD_MASK;

/// What the payload of a [`TermId`] means.
///
/// Tag values are part of the on-disk format. Never renumber an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Tag {
    /// Dictionary-backed IRI.
    Iri = 0x0,
    /// Dictionary-backed literal — anything the inline codec declined.
    Literal = 0x1,
    /// Blank node, scoped to the store.
    BlankNode = 0x2,
    /// Well-known vocabulary term: payload indexes a static table, so `rdf:type`
    /// is a compile-time constant the planner can match on.
    Vocab = 0x3,
    /// Inline `xsd:integer`, order-preserving.
    Integer = 0x4,
    /// Inline `xsd:float`, order-preserving.
    Float = 0x5,
    /// Inline `xsd:dateTime` in canonical UTC form, order-preserving.
    DateTime = 0x6,
    /// Inline `xsd:boolean` or a short `xsd:string` — see [`crate::inline`].
    Small = 0x7,
    /// RDF 1.2 triple term: payload indexes the triple-term side table.
    TripleTerm = 0x8,
    /// A term that appears in a query but not in the store. Matches nothing; round-trips
    /// through a per-query side table so results and `FILTER` comparisons stay correct.
    Ephemeral = 0xE,
    /// Reserved — never produced by the encoder.
    Reserved = 0xF,
}

impl Tag {
    /// Decodes a 4-bit tag. Unknown bit patterns become [`Tag::Reserved`].
    #[inline]
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            0x0 => Self::Iri,
            0x1 => Self::Literal,
            0x2 => Self::BlankNode,
            0x3 => Self::Vocab,
            0x4 => Self::Integer,
            0x5 => Self::Float,
            0x6 => Self::DateTime,
            0x7 => Self::Small,
            0x8 => Self::TripleTerm,
            0xE => Self::Ephemeral,
            _ => Self::Reserved,
        }
    }

    /// True for tags whose payload is a dictionary index rather than a value.
    #[inline]
    #[must_use]
    pub const fn is_dictionary_backed(self) -> bool {
        matches!(self, Self::Iri | Self::Literal | Self::BlankNode)
    }

    /// True for tags whose payload carries the term's value with no indirection.
    #[inline]
    #[must_use]
    pub const fn is_inline(self) -> bool {
        matches!(
            self,
            Self::Vocab | Self::Integer | Self::Float | Self::DateTime | Self::Small
        )
    }
}

/// An interned RDF term.
///
/// Equality on `TermId` is exactly RDF term equality (SPARQL `sameTerm`), which is why the
/// inline codec refuses any literal whose lexical form is not canonical — see
/// [`crate::inline`] for why that matters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermId(u64);

impl TermId {
    /// Builds an id from a tag and payload.
    ///
    /// # Panics
    /// If `payload` does not fit in 60 bits.
    #[inline]
    #[must_use]
    pub const fn new(tag: Tag, payload: u64) -> Self {
        assert!(payload <= PAYLOAD_MASK, "TermId payload overflows 60 bits");
        Self(((tag as u64) << TAG_SHIFT) | payload)
    }

    /// Builds an id from its raw 64-bit representation.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw 64-bit representation, as stored in an index key.
    #[inline]
    #[must_use]
    pub const fn to_raw(self) -> u64 {
        self.0
    }

    /// The tag.
    #[inline]
    #[must_use]
    pub const fn tag(self) -> Tag {
        Tag::from_bits((self.0 >> TAG_SHIFT) as u8)
    }

    /// The 60-bit payload.
    #[inline]
    #[must_use]
    pub const fn payload(self) -> u64 {
        self.0 & PAYLOAD_MASK
    }

    /// Lowest id sharing this id's tag — the start of a tag-wide range scan.
    #[inline]
    #[must_use]
    pub const fn tag_floor(tag: Tag) -> Self {
        Self((tag as u64) << TAG_SHIFT)
    }

    /// Whether a term of this kind can stand as a triple's subject.
    ///
    /// IRIs and blank nodes can; literals and triple terms cannot. Worth a named predicate
    /// because "is it a literal" is the wrong question and gets the wrong answer: five tags
    /// carry literals — `Literal` for the dictionary-backed ones and `Integer`, `Float`,
    /// `DateTime` and `Small` for the inline codecs — so a check against `Tag::Literal` alone
    /// passes every inline literal straight through. That mistake let a reasoner write
    /// `30 rdf:type xsd:integer` into a store, which encodes and does not decode.
    #[inline]
    #[must_use]
    pub const fn can_be_subject(self) -> bool {
        matches!(self.tag(), Tag::Iri | Tag::BlankNode | Tag::Vocab)
    }

    /// Highest id sharing this id's tag — the end of a tag-wide range scan.
    #[inline]
    #[must_use]
    pub const fn tag_ceil(tag: Tag) -> Self {
        Self(((tag as u64) << TAG_SHIFT) | PAYLOAD_MASK)
    }
}

impl fmt::Debug for TermId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}#{}", self.tag(), self.payload())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_and_payload_round_trip() {
        for tag in [
            Tag::Iri,
            Tag::Literal,
            Tag::BlankNode,
            Tag::Vocab,
            Tag::Integer,
            Tag::Float,
            Tag::DateTime,
            Tag::Small,
            Tag::TripleTerm,
            Tag::Ephemeral,
        ] {
            for payload in [0, 1, 42, PAYLOAD_MAX - 1, PAYLOAD_MAX] {
                let id = TermId::new(tag, payload);
                assert_eq!(id.tag(), tag, "tag round trip for {tag:?}/{payload}");
                assert_eq!(id.payload(), payload, "payload round trip");
                assert_eq!(TermId::from_raw(id.to_raw()), id);
            }
        }
    }

    #[test]
    fn ids_sort_by_tag_then_payload() {
        // Range scans over an index depend on this: all ids of one tag form one
        // contiguous block, ordered by payload within it.
        let a = TermId::new(Tag::Integer, 7);
        let b = TermId::new(Tag::Integer, 8);
        let c = TermId::new(Tag::Small, 0);
        assert!(a < b);
        assert!(b < c);
        assert!(TermId::tag_floor(Tag::Integer) <= a);
        assert!(a <= TermId::tag_ceil(Tag::Integer));
        assert!(TermId::tag_ceil(Tag::Integer) < TermId::tag_floor(Tag::Small));
    }

    #[test]
    #[should_panic(expected = "overflows 60 bits")]
    fn payload_overflow_is_caught() {
        let _ = TermId::new(Tag::Iri, PAYLOAD_MAX + 1);
    }
}
