//! What a holon is, as data.
//!
//! `DESIGN.md` §9 maps the four layers of the Holon model onto things the engine already
//! has, and the mapping is the whole point — none of this is a new data model:
//!
//! | Holon layer | Engine primitive |
//! |---|---|
//! | **Scene** — mutable current state | a named graph, the unit of transaction |
//! | **Boundary** — the legal transitions | a compiled shapes graph bound to that scene |
//! | **Event** — provenance and causality | an append-only log, PROV-O shaped, with RDF 1.2 reifiers |
//! | **Projection** — the visible surface | registered SPARQL queries |
//!
//! Every holon's own definition is RDF in a system graph, so a holon is queryable with
//! ordinary SPARQL and nothing here needs a bespoke representation. §3 makes that a
//! non-goal to break: *"the moment holons need a non-RDF representation, the project has
//! failed its own premise."*

use oxrdf::NamedNode;

/// The HOLOS system vocabulary namespace.
pub const NS: &str = "https://holos.rdf/ns#";

/// Builds a term in the HOLOS system vocabulary.
#[must_use]
pub fn holos(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS}{local}"))
}

/// The graph every holon's definition lives in.
#[must_use]
pub fn system_graph() -> NamedNode {
    NamedNode::new_unchecked("urn:holos:system")
}

/// What a holon does with a commit its boundary rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Refuse the commit. The scene keeps its previous state.
    ///
    /// The default, and the one that makes a Boundary a boundary: a holon whose invariants
    /// can be violated by any writer is a holon with documentation, not rules.
    Reject,
    /// Accept the commit and record the violations in the event log.
    ///
    /// For ingesting data that is known to be imperfect, where the alternative is losing
    /// it. The violations become part of the holon's history rather than a silent gap.
    AdmitAndRecord,
}

impl Admission {
    /// The IRI naming this policy.
    #[must_use]
    pub fn iri(self) -> NamedNode {
        match self {
            Self::Reject => holos("Reject"),
            Self::AdmitAndRecord => holos("AdmitAndRecord"),
        }
    }
}

/// How a projection is kept up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// Maintained incrementally from the delta.
    ///
    /// **Not implemented.** §9 restricts this to a maintainable fragment of SPARQL, and the
    /// Z-set machinery that would do the maintaining is not built. The variant exists so
    /// the registry can say which regime a projection is in rather than leaving a caller to
    /// assume; declaring one today is refused rather than silently downgraded.
    Maintained,
    /// Recomputed on read.
    ///
    /// Correct for any query, and the only regime this build actually runs.
    Recomputed,
}

/// A registered query making part of a holon visible.
///
/// §9: agents read projections and never touch the scene. That separation is what makes a
/// holon safe to expose — and, per §14.6, what makes a projection a *declassification* if
/// it is computed with more privilege than its readers have.
#[derive(Debug, Clone)]
pub struct Projection {
    /// The projection's name.
    pub id: NamedNode,
    /// The SPARQL query that computes it.
    pub query: String,
    /// How it is kept current.
    pub regime: Regime,
}

/// A holon.
#[derive(Debug, Clone)]
pub struct Holon {
    /// The holon's own IRI.
    pub id: NamedNode,
    /// The named graph holding current state.
    pub scene: NamedNode,
    /// The named graph holding the shapes and rules that govern it.
    pub boundary: NamedNode,
    /// The named graph holding the append-only event log.
    pub events: NamedNode,
    /// What to do with a commit the boundary rejects.
    pub admission: Admission,
    /// The registered projections.
    pub projections: Vec<Projection>,
}

impl Holon {
    /// A holon with graphs derived from its own IRI.
    ///
    /// Deriving rather than requiring three more IRIs keeps the common case to one name,
    /// and makes the relationship between a holon and its graphs legible in any dump.
    #[must_use]
    pub fn new(id: NamedNode) -> Self {
        let base = id.as_str().trim_end_matches(['#', '/']).to_owned();
        Self {
            scene: NamedNode::new_unchecked(format!("{base}/scene")),
            boundary: NamedNode::new_unchecked(format!("{base}/boundary")),
            events: NamedNode::new_unchecked(format!("{base}/events")),
            id,
            admission: Admission::Reject,
            projections: Vec::new(),
        }
    }

    /// Sets what happens to a rejected commit.
    #[must_use]
    pub fn with_admission(mut self, admission: Admission) -> Self {
        self.admission = admission;
        self
    }

    /// Registers a projection.
    #[must_use]
    pub fn with_projection(mut self, id: NamedNode, query: impl Into<String>) -> Self {
        self.projections.push(Projection {
            id,
            query: query.into(),
            regime: Regime::Recomputed,
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphs_are_derived_from_the_holon_iri() {
        let h = Holon::new(NamedNode::new_unchecked("urn:holon:orders"));
        assert_eq!(h.scene.as_str(), "urn:holon:orders/scene");
        assert_eq!(h.boundary.as_str(), "urn:holon:orders/boundary");
        assert_eq!(h.events.as_str(), "urn:holon:orders/events");
    }

    #[test]
    fn a_trailing_separator_is_not_doubled() {
        let h = Holon::new(NamedNode::new_unchecked("http://example.com/orders#"));
        assert_eq!(h.scene.as_str(), "http://example.com/orders/scene");
    }

    #[test]
    fn rejecting_is_the_default() {
        // A holon whose invariants any writer can break is documentation, not a boundary.
        let h = Holon::new(NamedNode::new_unchecked("urn:holon:x"));
        assert_eq!(h.admission, Admission::Reject);
    }
}
