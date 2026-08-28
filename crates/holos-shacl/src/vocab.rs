//! The SHACL vocabulary, as term ids.
//!
//! Every one of these is in the well-known table (`holos_core::vocab`), so resolving them
//! touches no store and no dictionary: `sh:minCount` is a compile-time constant, and
//! comparing against it is an integer comparison. That is the whole reason the table
//! exists (`DESIGN.md` §5), and SHACL is where it pays off most — a validator mentions
//! these terms constantly.

use holos_core::{vocab, TermId};

macro_rules! sh_vocab {
    ($($field:ident => $iri:expr),* $(,)?) => {
        /// Every vocabulary term the validator needs, resolved once.
        #[derive(Debug, Clone, Copy)]
        #[allow(missing_docs, clippy::struct_field_names)]
        pub struct Sh {
            $(pub $field: TermId,)*
        }

        impl Sh {
            /// Resolves the vocabulary.
            ///
            /// # Panics
            /// If a term is missing from the well-known table, which is a build-time bug
            /// rather than anything a user can cause. [`tests::every_term_resolves`]
            /// guards it.
            #[must_use]
            pub fn new() -> Self {
                Self {
                    $($field: vocab::encode_iri($iri)
                        .unwrap_or_else(|| panic!("{} is not in the well-known table", $iri)),)*
                }
            }
        }
    };
}

const SH: &str = "http://www.w3.org/ns/shacl#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";

macro_rules! sh {
    ($n:literal) => {
        concat!("http://www.w3.org/ns/shacl#", $n)
    };
}
macro_rules! rdf {
    ($n:literal) => {
        concat!("http://www.w3.org/1999/02/22-rdf-syntax-ns#", $n)
    };
}
macro_rules! rdfs {
    ($n:literal) => {
        concat!("http://www.w3.org/2000/01/rdf-schema#", $n)
    };
}

sh_vocab! {
    // --- RDF and RDFS
    rdf_type => rdf!("type"),
    rdf_first => rdf!("first"),
    rdf_rest => rdf!("rest"),
    rdf_nil => rdf!("nil"),
    rdf_lang_string => rdf!("langString"),
    rdfs_class => rdfs!("Class"),
    rdfs_subclass_of => rdfs!("subClassOf"),

    // --- shape kinds and targets
    node_shape => sh!("NodeShape"),
    property_shape => sh!("PropertyShape"),
    target_class => sh!("targetClass"),
    target_node => sh!("targetNode"),
    target_subjects_of => sh!("targetSubjectsOf"),
    target_objects_of => sh!("targetObjectsOf"),

    // --- structure
    path => sh!("path"),
    property => sh!("property"),
    node => sh!("node"),
    deactivated => sh!("deactivated"),
    severity => sh!("severity"),
    message => sh!("message"),

    // --- paths
    inverse_path => sh!("inversePath"),
    alternative_path => sh!("alternativePath"),
    zero_or_more_path => sh!("zeroOrMorePath"),
    one_or_more_path => sh!("oneOrMorePath"),
    zero_or_one_path => sh!("zeroOrOnePath"),

    // --- constraint parameters
    class => sh!("class"),
    datatype => sh!("datatype"),
    node_kind => sh!("nodeKind"),
    min_count => sh!("minCount"),
    max_count => sh!("maxCount"),
    min_inclusive => sh!("minInclusive"),
    max_inclusive => sh!("maxInclusive"),
    min_exclusive => sh!("minExclusive"),
    max_exclusive => sh!("maxExclusive"),
    min_length => sh!("minLength"),
    max_length => sh!("maxLength"),
    pattern => sh!("pattern"),
    flags => sh!("flags"),
    language_in => sh!("languageIn"),
    unique_lang => sh!("uniqueLang"),
    // SHACL 1.2 list constraints. They take an RDF list as the *value node* and say
    // something about its members, which is a different shape of check from everything
    // above: the value is a handle to a structure rather than a value in itself.
    min_list_length => sh!("minListLength"),
    max_list_length => sh!("maxListLength"),
    unique_members => sh!("uniqueMembers"),
    member_shape => sh!("memberShape"),
    single_line => sh!("singleLine"),
    subset_of => sh!("subsetOf"),
    unique_values_for => sh!("uniqueValuesFor"),
    some_value => sh!("someValue"),
    by_types => sh!("ByTypes"),
    shape => sh!("shape"),
    shape_class => sh!("ShapeClass"),
    rdf_reifies => rdf!("reifies"),
    equals => sh!("equals"),
    disjoint => sh!("disjoint"),
    less_than => sh!("lessThan"),
    less_than_or_equals => sh!("lessThanOrEquals"),
    not => sh!("not"),
    and => sh!("and"),
    or => sh!("or"),
    xone => sh!("xone"),
    closed => sh!("closed"),
    ignored_properties => sh!("ignoredProperties"),
    has_value => sh!("hasValue"),
    r#in => sh!("in"),
    qualified_value_shape => sh!("qualifiedValueShape"),
    qualified_min_count => sh!("qualifiedMinCount"),
    qualified_max_count => sh!("qualifiedMaxCount"),
    qualified_value_shapes_disjoint => sh!("qualifiedValueShapesDisjoint"),

    // --- node kinds
    iri => sh!("IRI"),
    blank_node => sh!("BlankNode"),
    literal => sh!("Literal"),
    iri_or_literal => sh!("IRIOrLiteral"),
    blank_node_or_iri => sh!("BlankNodeOrIRI"),
    blank_node_or_literal => sh!("BlankNodeOrLiteral"),

    // --- severities
    violation => sh!("Violation"),
    warning => sh!("Warning"),
    info => sh!("Info"),
    // SHACL 1.2 adds two levels below `sh:Info`. They are diagnostic rather than
    // judgemental: a report may carry them and still say the data conforms.
    debug => sh!("Debug"),
    trace => sh!("Trace"),

    // --- report vocabulary
    validation_report => sh!("ValidationReport"),
    validation_result => sh!("ValidationResult"),
    conforms => sh!("conforms"),
    result => sh!("result"),
    focus_node => sh!("focusNode"),
    result_path => sh!("resultPath"),
    value => sh!("value"),
    source_shape => sh!("sourceShape"),
    source_constraint_component => sh!("sourceConstraintComponent"),
    result_severity => sh!("resultSeverity"),
    result_message => sh!("resultMessage"),
    detail => sh!("detail"),

    // --- constraint components
    class_component => sh!("ClassConstraintComponent"),
    datatype_component => sh!("DatatypeConstraintComponent"),
    node_kind_component => sh!("NodeKindConstraintComponent"),
    min_count_component => sh!("MinCountConstraintComponent"),
    max_count_component => sh!("MaxCountConstraintComponent"),
    min_inclusive_component => sh!("MinInclusiveConstraintComponent"),
    max_inclusive_component => sh!("MaxInclusiveConstraintComponent"),
    min_exclusive_component => sh!("MinExclusiveConstraintComponent"),
    max_exclusive_component => sh!("MaxExclusiveConstraintComponent"),
    min_length_component => sh!("MinLengthConstraintComponent"),
    max_length_component => sh!("MaxLengthConstraintComponent"),
    pattern_component => sh!("PatternConstraintComponent"),
    language_in_component => sh!("LanguageInConstraintComponent"),
    unique_lang_component => sh!("UniqueLangConstraintComponent"),
    min_list_length_component => sh!("MinListLengthConstraintComponent"),
    max_list_length_component => sh!("MaxListLengthConstraintComponent"),
    unique_members_component => sh!("UniqueMembersConstraintComponent"),
    member_shape_component => sh!("MemberShapeConstraintComponent"),
    single_line_component => sh!("SingleLineConstraintComponent"),
    subset_of_component => sh!("SubsetOfConstraintComponent"),
    unique_values_for_component => sh!("UniqueValuesForConstraintComponent"),
    some_value_component => sh!("SomeValueConstraintComponent"),
    equals_component => sh!("EqualsConstraintComponent"),
    disjoint_component => sh!("DisjointConstraintComponent"),
    less_than_component => sh!("LessThanConstraintComponent"),
    less_than_or_equals_component => sh!("LessThanOrEqualsConstraintComponent"),
    not_component => sh!("NotConstraintComponent"),
    and_component => sh!("AndConstraintComponent"),
    or_component => sh!("OrConstraintComponent"),
    xone_component => sh!("XoneConstraintComponent"),
    node_component => sh!("NodeConstraintComponent"),
    property_component => sh!("PropertyConstraintComponent"),
    qualified_min_count_component => sh!("QualifiedMinCountConstraintComponent"),
    qualified_max_count_component => sh!("QualifiedMaxCountConstraintComponent"),
    closed_component => sh!("ClosedConstraintComponent"),
    has_value_component => sh!("HasValueConstraintComponent"),
    in_component => sh!("InConstraintComponent"),
}

impl Default for Sh {
    fn default() -> Self {
        Self::new()
    }
}

/// The SHACL namespace.
#[must_use]
pub fn namespace() -> &'static str {
    SH
}

/// The RDF namespace.
#[must_use]
pub fn rdf_namespace() -> &'static str {
    RDF
}

/// The RDFS namespace.
#[must_use]
pub fn rdfs_namespace() -> &'static str {
    RDFS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_term_resolves() {
        // `Sh::new` panics on a missing term; this is the guard that the well-known table
        // and this struct have not drifted apart.
        let sh = Sh::new();
        assert_ne!(sh.min_count, sh.max_count);
        assert_ne!(sh.node_shape, sh.property_shape);
    }

    #[test]
    fn resolution_touches_no_store() {
        // Every SHACL term is well-known, so the validator never pays a dictionary lookup
        // for its own vocabulary — the point of DESIGN.md §5's table.
        let sh = Sh::new();
        assert_eq!(sh.rdf_type.tag(), holos_core::Tag::Vocab);
        assert_eq!(sh.min_count.tag(), holos_core::Tag::Vocab);
        assert_eq!(sh.closed_component.tag(), holos_core::Tag::Vocab);
    }
}
