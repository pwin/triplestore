//! HOLOS L1 — tagged 64-bit term identifiers.
//!
//! This crate owns the encoding that everything above it depends on: how an RDF term
//! becomes a [`TermId`], and how a [`TermId`] becomes an RDF term again. It holds no
//! state — the dictionary that backs [`Tag::Iri`], [`Tag::Literal`] and
//! [`Tag::BlankNode`] lives in `holos-store`.
//!
//! See `DESIGN.md` §5 for why the identifier is a dense 60-bit payload rather than
//! Oxigraph's 128-bit `StrHash`, and [`inline`] for the canonicality rule that keeps
//! [`TermId`] equality identical to RDF term equality.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// `# Errors` sections would restate the error enum on every function; the enums are
// documented at their definition instead.
#![allow(clippy::missing_errors_doc)]
// s/p/o/g are the names the RDF and SPARQL specifications use. Renaming them to satisfy
// a length lint would make this code harder to check against the specs, not easier.
#![allow(clippy::many_single_char_names)]

pub mod inline;
pub mod term_id;
pub mod vocab;

pub use term_id::{Tag, TermId, PAYLOAD_MASK, PAYLOAD_MAX, TAG_SHIFT};

/// The on-disk format version.
///
/// Bumped whenever a [`Tag`] is renumbered, an inline codec changes, or the
/// [`vocab::VOCAB`] table is *appended to or* reordered.
///
/// Appending is the subtle one. A term that was dictionary-backed under the old table
/// becomes [`Tag::Vocab`] under the new one, so a store written before the append still
/// holds a dictionary row for it — and the same IRI would then have two ids, which breaks
/// the one-id-per-term invariant that [`TermId`] equality depends on. A persistent store
/// records this number and refuses to open under a build that does not match.
///
/// | Version | Change |
/// |---|---|
/// | 1 | Initial encoding |
/// | 2 | SHACL path, constraint-parameter and constraint-component terms appended to the vocabulary (§8) |
pub const FORMAT_VERSION: u32 = 2;
