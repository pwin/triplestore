//! The well-known vocabulary table — [`Tag::Vocab`](crate::Tag::Vocab).
//!
//! `DESIGN.md` §5: static compile-time-constant ids for `rdf:`, `rdfs:`, `owl:`, `xsd:`,
//! `sh:` and `prov:`, so that `rdf:type` is a constant the planner can pattern-match on
//! rather than a dictionary lookup. These terms dominate every real dataset, so keeping
//! them out of the dictionary is worth the table.
//!
//! # Format stability
//!
//! A term's payload is its index in [`VOCAB`]. That index is part of the on-disk format:
//! **append only, never reorder or delete.** [`tests::table_is_consistent`] guards the
//! table/lookup pairing but cannot guard against a reorder in a later revision, so the
//! store writes a format version alongside its data.

use crate::{Tag, TermId};
use std::collections::HashMap;
use std::sync::OnceLock;

macro_rules! vocab_table {
    ($($iri:literal),* $(,)?) => {
        /// Every well-known IRI, indexed by payload. Append-only.
        pub const VOCAB: &[&str] = &[$($iri),*];
    };
}

vocab_table![
    // --- rdf:
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#first",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#value",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#dirLangString",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#HTML",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON",
    // RDF 1.2 reification — see DESIGN.md §5. `rdf:reifies` is an ordinary predicate,
    // which is exactly why triple terms cost so little.
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#subject",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Statement",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#List",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Bag",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Seq",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Alt",
    // --- rdfs:
    "http://www.w3.org/2000/01/rdf-schema#label",
    "http://www.w3.org/2000/01/rdf-schema#comment",
    "http://www.w3.org/2000/01/rdf-schema#subClassOf",
    "http://www.w3.org/2000/01/rdf-schema#subPropertyOf",
    "http://www.w3.org/2000/01/rdf-schema#domain",
    "http://www.w3.org/2000/01/rdf-schema#range",
    "http://www.w3.org/2000/01/rdf-schema#Class",
    "http://www.w3.org/2000/01/rdf-schema#Resource",
    "http://www.w3.org/2000/01/rdf-schema#Literal",
    "http://www.w3.org/2000/01/rdf-schema#Datatype",
    "http://www.w3.org/2000/01/rdf-schema#Container",
    "http://www.w3.org/2000/01/rdf-schema#seeAlso",
    "http://www.w3.org/2000/01/rdf-schema#isDefinedBy",
    "http://www.w3.org/2000/01/rdf-schema#member",
    // --- owl:
    "http://www.w3.org/2002/07/owl#Class",
    "http://www.w3.org/2002/07/owl#ObjectProperty",
    "http://www.w3.org/2002/07/owl#DatatypeProperty",
    "http://www.w3.org/2002/07/owl#AnnotationProperty",
    "http://www.w3.org/2002/07/owl#Ontology",
    "http://www.w3.org/2002/07/owl#Thing",
    "http://www.w3.org/2002/07/owl#Nothing",
    "http://www.w3.org/2002/07/owl#sameAs",
    "http://www.w3.org/2002/07/owl#differentFrom",
    "http://www.w3.org/2002/07/owl#imports",
    "http://www.w3.org/2002/07/owl#inverseOf",
    "http://www.w3.org/2002/07/owl#equivalentClass",
    "http://www.w3.org/2002/07/owl#equivalentProperty",
    "http://www.w3.org/2002/07/owl#disjointWith",
    "http://www.w3.org/2002/07/owl#TransitiveProperty",
    "http://www.w3.org/2002/07/owl#SymmetricProperty",
    "http://www.w3.org/2002/07/owl#FunctionalProperty",
    "http://www.w3.org/2002/07/owl#InverseFunctionalProperty",
    // --- xsd:
    "http://www.w3.org/2001/XMLSchema#string",
    "http://www.w3.org/2001/XMLSchema#boolean",
    "http://www.w3.org/2001/XMLSchema#decimal",
    "http://www.w3.org/2001/XMLSchema#integer",
    "http://www.w3.org/2001/XMLSchema#double",
    "http://www.w3.org/2001/XMLSchema#float",
    "http://www.w3.org/2001/XMLSchema#date",
    "http://www.w3.org/2001/XMLSchema#dateTime",
    "http://www.w3.org/2001/XMLSchema#dateTimeStamp",
    "http://www.w3.org/2001/XMLSchema#time",
    "http://www.w3.org/2001/XMLSchema#duration",
    "http://www.w3.org/2001/XMLSchema#dayTimeDuration",
    "http://www.w3.org/2001/XMLSchema#yearMonthDuration",
    "http://www.w3.org/2001/XMLSchema#anyURI",
    "http://www.w3.org/2001/XMLSchema#long",
    "http://www.w3.org/2001/XMLSchema#int",
    "http://www.w3.org/2001/XMLSchema#short",
    "http://www.w3.org/2001/XMLSchema#byte",
    "http://www.w3.org/2001/XMLSchema#nonNegativeInteger",
    "http://www.w3.org/2001/XMLSchema#positiveInteger",
    "http://www.w3.org/2001/XMLSchema#nonPositiveInteger",
    "http://www.w3.org/2001/XMLSchema#negativeInteger",
    "http://www.w3.org/2001/XMLSchema#unsignedLong",
    "http://www.w3.org/2001/XMLSchema#unsignedInt",
    "http://www.w3.org/2001/XMLSchema#unsignedShort",
    "http://www.w3.org/2001/XMLSchema#unsignedByte",
    "http://www.w3.org/2001/XMLSchema#gYear",
    "http://www.w3.org/2001/XMLSchema#gMonth",
    "http://www.w3.org/2001/XMLSchema#gDay",
    "http://www.w3.org/2001/XMLSchema#gYearMonth",
    "http://www.w3.org/2001/XMLSchema#gMonthDay",
    "http://www.w3.org/2001/XMLSchema#base64Binary",
    "http://www.w3.org/2001/XMLSchema#hexBinary",
    "http://www.w3.org/2001/XMLSchema#normalizedString",
    "http://www.w3.org/2001/XMLSchema#token",
    "http://www.w3.org/2001/XMLSchema#language",
    "http://www.w3.org/2001/XMLSchema#Name",
    "http://www.w3.org/2001/XMLSchema#NCName",
    "http://www.w3.org/2001/XMLSchema#NMTOKEN",
    // --- sh: (SHACL — L4 is a first-class subsystem, DESIGN.md §8)
    "http://www.w3.org/ns/shacl#NodeShape",
    "http://www.w3.org/ns/shacl#PropertyShape",
    "http://www.w3.org/ns/shacl#property",
    "http://www.w3.org/ns/shacl#path",
    "http://www.w3.org/ns/shacl#targetClass",
    "http://www.w3.org/ns/shacl#targetNode",
    "http://www.w3.org/ns/shacl#targetSubjectsOf",
    "http://www.w3.org/ns/shacl#targetObjectsOf",
    "http://www.w3.org/ns/shacl#datatype",
    "http://www.w3.org/ns/shacl#nodeKind",
    "http://www.w3.org/ns/shacl#minCount",
    "http://www.w3.org/ns/shacl#maxCount",
    "http://www.w3.org/ns/shacl#class",
    "http://www.w3.org/ns/shacl#node",
    "http://www.w3.org/ns/shacl#or",
    "http://www.w3.org/ns/shacl#and",
    "http://www.w3.org/ns/shacl#not",
    "http://www.w3.org/ns/shacl#xone",
    "http://www.w3.org/ns/shacl#in",
    "http://www.w3.org/ns/shacl#hasValue",
    "http://www.w3.org/ns/shacl#pattern",
    "http://www.w3.org/ns/shacl#flags",
    "http://www.w3.org/ns/shacl#minLength",
    "http://www.w3.org/ns/shacl#maxLength",
    "http://www.w3.org/ns/shacl#minInclusive",
    "http://www.w3.org/ns/shacl#maxInclusive",
    "http://www.w3.org/ns/shacl#minExclusive",
    "http://www.w3.org/ns/shacl#maxExclusive",
    "http://www.w3.org/ns/shacl#closed",
    "http://www.w3.org/ns/shacl#ignoredProperties",
    "http://www.w3.org/ns/shacl#severity",
    "http://www.w3.org/ns/shacl#message",
    "http://www.w3.org/ns/shacl#name",
    "http://www.w3.org/ns/shacl#description",
    "http://www.w3.org/ns/shacl#deactivated",
    "http://www.w3.org/ns/shacl#Violation",
    "http://www.w3.org/ns/shacl#Warning",
    "http://www.w3.org/ns/shacl#Info",
    "http://www.w3.org/ns/shacl#ValidationReport",
    "http://www.w3.org/ns/shacl#ValidationResult",
    "http://www.w3.org/ns/shacl#conforms",
    "http://www.w3.org/ns/shacl#result",
    "http://www.w3.org/ns/shacl#focusNode",
    "http://www.w3.org/ns/shacl#resultPath",
    "http://www.w3.org/ns/shacl#value",
    "http://www.w3.org/ns/shacl#sourceShape",
    "http://www.w3.org/ns/shacl#sourceConstraintComponent",
    "http://www.w3.org/ns/shacl#resultSeverity",
    "http://www.w3.org/ns/shacl#resultMessage",
    "http://www.w3.org/ns/shacl#IRI",
    "http://www.w3.org/ns/shacl#Literal",
    "http://www.w3.org/ns/shacl#BlankNode",
    "http://www.w3.org/ns/shacl#rule",
    "http://www.w3.org/ns/shacl#condition",
    "http://www.w3.org/ns/shacl#TripleRule",
    "http://www.w3.org/ns/shacl#SPARQLRule",
    "http://www.w3.org/ns/shacl#subject",
    "http://www.w3.org/ns/shacl#predicate",
    "http://www.w3.org/ns/shacl#object",
    "http://www.w3.org/ns/shacl#construct",
    "http://www.w3.org/ns/shacl#order",
    // --- sh: continued. Appended for the L4 validator (DESIGN.md §8); appending is
    // safe, reordering is not, and either way FORMAT_VERSION moves.
    "http://www.w3.org/ns/shacl#inversePath",
    "http://www.w3.org/ns/shacl#alternativePath",
    "http://www.w3.org/ns/shacl#zeroOrMorePath",
    "http://www.w3.org/ns/shacl#oneOrMorePath",
    "http://www.w3.org/ns/shacl#zeroOrOnePath",
    "http://www.w3.org/ns/shacl#languageIn",
    "http://www.w3.org/ns/shacl#uniqueLang",
    "http://www.w3.org/ns/shacl#equals",
    "http://www.w3.org/ns/shacl#disjoint",
    "http://www.w3.org/ns/shacl#lessThan",
    "http://www.w3.org/ns/shacl#lessThanOrEquals",
    "http://www.w3.org/ns/shacl#qualifiedValueShape",
    "http://www.w3.org/ns/shacl#qualifiedMinCount",
    "http://www.w3.org/ns/shacl#qualifiedMaxCount",
    "http://www.w3.org/ns/shacl#qualifiedValueShapesDisjoint",
    "http://www.w3.org/ns/shacl#shapesGraph",
    "http://www.w3.org/ns/shacl#detail",
    "http://www.w3.org/ns/shacl#Shape",
    "http://www.w3.org/ns/shacl#target",
    "http://www.w3.org/ns/shacl#IRIOrLiteral",
    "http://www.w3.org/ns/shacl#BlankNodeOrIRI",
    "http://www.w3.org/ns/shacl#BlankNodeOrLiteral",
    "http://www.w3.org/ns/shacl#ClassConstraintComponent",
    "http://www.w3.org/ns/shacl#DatatypeConstraintComponent",
    "http://www.w3.org/ns/shacl#NodeKindConstraintComponent",
    "http://www.w3.org/ns/shacl#MinCountConstraintComponent",
    "http://www.w3.org/ns/shacl#MaxCountConstraintComponent",
    "http://www.w3.org/ns/shacl#MinExclusiveConstraintComponent",
    "http://www.w3.org/ns/shacl#MinInclusiveConstraintComponent",
    "http://www.w3.org/ns/shacl#MaxExclusiveConstraintComponent",
    "http://www.w3.org/ns/shacl#MaxInclusiveConstraintComponent",
    "http://www.w3.org/ns/shacl#MinLengthConstraintComponent",
    "http://www.w3.org/ns/shacl#MaxLengthConstraintComponent",
    "http://www.w3.org/ns/shacl#PatternConstraintComponent",
    "http://www.w3.org/ns/shacl#LanguageInConstraintComponent",
    "http://www.w3.org/ns/shacl#UniqueLangConstraintComponent",
    "http://www.w3.org/ns/shacl#EqualsConstraintComponent",
    "http://www.w3.org/ns/shacl#DisjointConstraintComponent",
    "http://www.w3.org/ns/shacl#LessThanConstraintComponent",
    "http://www.w3.org/ns/shacl#LessThanOrEqualsConstraintComponent",
    "http://www.w3.org/ns/shacl#NotConstraintComponent",
    "http://www.w3.org/ns/shacl#AndConstraintComponent",
    "http://www.w3.org/ns/shacl#OrConstraintComponent",
    "http://www.w3.org/ns/shacl#XoneConstraintComponent",
    "http://www.w3.org/ns/shacl#NodeConstraintComponent",
    "http://www.w3.org/ns/shacl#PropertyConstraintComponent",
    "http://www.w3.org/ns/shacl#QualifiedMinCountConstraintComponent",
    "http://www.w3.org/ns/shacl#QualifiedMaxCountConstraintComponent",
    "http://www.w3.org/ns/shacl#ClosedConstraintComponent",
    "http://www.w3.org/ns/shacl#HasValueConstraintComponent",
    "http://www.w3.org/ns/shacl#InConstraintComponent",
    // --- SHACL 1.2 additions. Appended, because a payload is an index into this table and
    // that index is on disk; inserting these next to their 1.0 relatives would renumber
    // every term after them.
    "http://www.w3.org/ns/shacl#minListLength",
    "http://www.w3.org/ns/shacl#maxListLength",
    "http://www.w3.org/ns/shacl#uniqueMembers",
    "http://www.w3.org/ns/shacl#memberShape",
    "http://www.w3.org/ns/shacl#singleLine",
    "http://www.w3.org/ns/shacl#MinListLengthConstraintComponent",
    "http://www.w3.org/ns/shacl#MaxListLengthConstraintComponent",
    "http://www.w3.org/ns/shacl#UniqueMembersConstraintComponent",
    "http://www.w3.org/ns/shacl#MemberShapeConstraintComponent",
    "http://www.w3.org/ns/shacl#SingleLineConstraintComponent",
    "http://www.w3.org/ns/shacl#Debug",
    "http://www.w3.org/ns/shacl#Trace",
    "http://www.w3.org/ns/shacl#subsetOf",
    "http://www.w3.org/ns/shacl#SubsetOfConstraintComponent",
    "http://www.w3.org/ns/shacl#uniqueValuesFor",
    "http://www.w3.org/ns/shacl#UniqueValuesForConstraintComponent",
    "http://www.w3.org/ns/shacl#someValue",
    "http://www.w3.org/ns/shacl#SomeValueConstraintComponent",
    "http://www.w3.org/ns/shacl#ByTypes",
    "http://www.w3.org/ns/shacl#shape",
    // --- prov: (the holon Event graph is PROV-O shaped, DESIGN.md §9)
    "http://www.w3.org/ns/prov#Entity",
    "http://www.w3.org/ns/prov#Activity",
    "http://www.w3.org/ns/prov#Agent",
    "http://www.w3.org/ns/prov#wasGeneratedBy",
    "http://www.w3.org/ns/prov#wasDerivedFrom",
    "http://www.w3.org/ns/prov#wasAttributedTo",
    "http://www.w3.org/ns/prov#wasAssociatedWith",
    "http://www.w3.org/ns/prov#wasInvalidatedBy",
    "http://www.w3.org/ns/prov#used",
    "http://www.w3.org/ns/prov#startedAtTime",
    "http://www.w3.org/ns/prov#endedAtTime",
    "http://www.w3.org/ns/prov#generatedAtTime",
];

fn lookup() -> &'static HashMap<&'static str, u64> {
    static LOOKUP: OnceLock<HashMap<&'static str, u64>> = OnceLock::new();
    LOOKUP.get_or_init(|| {
        VOCAB
            .iter()
            .enumerate()
            .map(|(i, iri)| (*iri, i as u64))
            .collect()
    })
}

/// Returns the well-known [`TermId`] for an IRI, if it has one.
#[must_use]
pub fn encode_iri(iri: &str) -> Option<TermId> {
    lookup().get(iri).map(|i| TermId::new(Tag::Vocab, *i))
}

/// Returns the IRI string for a [`Tag::Vocab`] id.
///
/// Returns `None` for a non-vocab tag, or for an index this build does not know — which
/// happens when a store written by a newer revision is opened by an older one.
#[must_use]
pub fn decode_iri(id: TermId) -> Option<&'static str> {
    if id.tag() != Tag::Vocab {
        return None;
    }
    VOCAB.get(usize::try_from(id.payload()).ok()?).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        for (i, iri) in VOCAB.iter().enumerate() {
            let id = encode_iri(iri).unwrap_or_else(|| panic!("{iri} not found in lookup"));
            assert_eq!(id.tag(), Tag::Vocab);
            assert_eq!(id.payload(), i as u64, "payload must equal table index");
            assert_eq!(decode_iri(id), Some(*iri), "round trip for {iri}");
        }
    }

    #[test]
    fn table_has_no_duplicates() {
        // A duplicate would make two distinct payloads decode to the same IRI, breaking
        // the one-id-per-term invariant that TermId equality relies on.
        assert_eq!(
            lookup().len(),
            VOCAB.len(),
            "duplicate IRI in the vocabulary table"
        );
    }

    #[test]
    fn rdf_type_is_a_constant() {
        let id = encode_iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type").unwrap();
        assert_eq!(id.payload(), 0, "rdf:type is the first entry, by design");
    }

    #[test]
    fn unknown_iris_are_not_in_the_table() {
        assert_eq!(encode_iri("http://example.com/thing"), None);
        assert_eq!(decode_iri(TermId::new(Tag::Iri, 0)), None);
        assert_eq!(decode_iri(TermId::new(Tag::Vocab, 1_000_000)), None);
    }
}
