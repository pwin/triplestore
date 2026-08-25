//! The event graph.
//!
//! `DESIGN.md` §9: an append-only versioned delta log, PROV-O shaped, **with RDF 1.2
//! reifiers annotating individual triples with who, when and why.**
//!
//! That last clause is the reason RDF 1.2 matters to this design rather than being a
//! checkbox. Provenance about a *statement* has always been awkward in RDF — standard
//! reification is four triples and no semantics, and a named graph per statement does not
//! scale. A reifier plus a triple term says it in two triples and says it exactly:
//!
//! ```text
//! _:change  rdf:reifies  <<( ex:alice ex:email "new@example.com" )>> ;
//!           holos:inTick _:tick ;
//!           holos:operation holos:Added .
//! ```
//!
//! The tick node carries who and when once; each change points at it. So the log answers
//! "who changed this triple, and in what commit" without duplicating the actor on every row
//! — and §5 already made a triple term cost one term id, so the log is not expensive.

use crate::model::{holos, Holon};
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term, Triple};

/// The PROV-O namespace.
const PROV: &str = "http://www.w3.org/ns/prov#";

fn prov(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{PROV}{local}"))
}

/// One recorded change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// The triple was added.
    Added,
    /// The triple was removed.
    Removed,
}

impl Operation {
    fn iri(self) -> NamedNode {
        match self {
            Self::Added => holos("Added"),
            Self::Removed => holos("Removed"),
        }
    }
}

/// Everything one tick records.
pub struct TickRecord<'a> {
    /// The holon that ticked.
    pub holon: &'a Holon,
    /// The version this tick produced.
    pub version: u64,
    /// Seconds since the Unix epoch.
    pub at: i64,
    /// Who committed it.
    pub principal: &'a NamedNode,
    /// The changes, in the order they were applied.
    pub changes: Vec<(Operation, Triple)>,
    /// Whether the boundary admitted the commit.
    pub admitted: bool,
    /// How many violations the boundary found.
    pub violations: usize,
}

/// Renders a tick as quads in the holon's event graph.
///
/// Deterministic: blank nodes are numbered from the version and the change's position, so
/// the same tick produces the same bytes and two event logs can be diffed. That is the same
/// property the validation report keeps, and for the same reason — a log nobody can diff is
/// a log nobody audits.
#[must_use]
pub fn to_quads(record: &TickRecord<'_>) -> Vec<Quad> {
    let graph = GraphName::from(record.holon.events.clone());
    let tick = BlankNode::new_unchecked(format!("tick{}", record.version));
    let mut out = Vec::with_capacity(record.changes.len() * 3 + 8);

    let say = |s: Term, p: NamedNode, o: Term, out: &mut Vec<Quad>| {
        let subject = match s {
            Term::NamedNode(n) => n.into(),
            Term::BlankNode(b) => b.into(),
            _ => return,
        };
        out.push(Quad {
            subject,
            predicate: p,
            object: o,
            graph_name: graph.clone(),
        });
    };

    say(
        tick.clone().into(),
        rdf::TYPE.into_owned(),
        prov("Activity").into(),
        &mut out,
    );
    say(
        tick.clone().into(),
        rdf::TYPE.into_owned(),
        holos("Tick").into(),
        &mut out,
    );
    say(
        tick.clone().into(),
        holos("onHolon"),
        record.holon.id.clone().into(),
        &mut out,
    );
    say(
        tick.clone().into(),
        holos("version"),
        Literal::new_typed_literal(record.version.to_string(), xsd::INTEGER).into(),
        &mut out,
    );
    if let Some(stamp) = holos_core::inline::format_utc_seconds(record.at) {
        say(
            tick.clone().into(),
            prov("startedAtTime"),
            Literal::new_typed_literal(stamp, xsd::DATE_TIME).into(),
            &mut out,
        );
    }
    say(
        tick.clone().into(),
        prov("wasAssociatedWith"),
        record.principal.clone().into(),
        &mut out,
    );
    say(
        tick.clone().into(),
        holos("admitted"),
        Literal::new_typed_literal(
            if record.admitted { "true" } else { "false" },
            xsd::BOOLEAN,
        )
        .into(),
        &mut out,
    );
    if record.violations > 0 {
        say(
            tick.clone().into(),
            holos("violations"),
            Literal::new_typed_literal(record.violations.to_string(), xsd::INTEGER).into(),
            &mut out,
        );
    }

    for (i, (operation, triple)) in record.changes.iter().enumerate() {
        let change = BlankNode::new_unchecked(format!("change{}_{i}", record.version));
        // The reifier names the triple; the triple term *is* the triple. Two triples for
        // per-statement provenance, where RDF 1.1 needed four and gave them no meaning.
        say(
            change.clone().into(),
            rdf::REIFIES.into_owned(),
            Term::Triple(Box::new(triple.clone())),
            &mut out,
        );
        say(
            change.clone().into(),
            holos("inTick"),
            tick.clone().into(),
            &mut out,
        );
        say(
            change.clone().into(),
            holos("operation"),
            operation.iri().into(),
            &mut out,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{s}"))
    }

    /// The fixed parts of a tick: who, which holon, and one change.
    ///
    /// The version is not among them — each test sets its own on the `TickRecord`, and
    /// taking it here as well meant every caller passed the same number twice.
    fn record() -> (Holon, NamedNode, Vec<(Operation, Triple)>) {
        let holon = Holon::new(NamedNode::new_unchecked("urn:holon:people"));
        let principal = NamedNode::new_unchecked("urn:holos:principal:alice");
        let changes = vec![(
            Operation::Added,
            Triple {
                subject: nn("alice").into(),
                predicate: nn("email"),
                object: Literal::new_simple_literal("a@example.com").into(),
            },
        )];
        (holon, principal, changes)
    }

    #[test]
    fn a_tick_records_who_when_and_what() {
        let (holon, principal, changes) = record();
        let quads = to_quads(&TickRecord {
            holon: &holon,
            version: 1,
            at: 1_767_225_600,
            principal: &principal,
            changes,
            admitted: true,
            violations: 0,
        });
        let text: Vec<String> = quads.iter().map(ToString::to_string).collect();
        let joined = text.join("\n");

        assert!(joined.contains("prov#Activity"), "{joined}");
        assert!(joined.contains("prov#wasAssociatedWith"));
        assert!(joined.contains("startedAtTime"));
        // Every quad lands in the holon's event graph, never the scene.
        assert!(quads
            .iter()
            .all(|q| q.graph_name == GraphName::from(holon.events.clone())));
    }

    #[test]
    fn each_change_is_a_reified_triple_term() {
        let (holon, principal, changes) = record();
        let quads = to_quads(&TickRecord {
            holon: &holon,
            version: 7,
            at: 0,
            principal: &principal,
            changes,
            admitted: true,
            violations: 0,
        });
        let reifies = quads
            .iter()
            .find(|q| q.predicate == rdf::REIFIES.into_owned())
            .expect("a change should be reified");
        assert!(
            matches!(reifies.object, Term::Triple(_)),
            "rdf:reifies must point at a triple term, got {}",
            reifies.object
        );
    }

    #[test]
    fn rendering_is_deterministic() {
        // Two renderings of the same tick must be byte-identical, or the log cannot be
        // diffed and nobody will audit it.
        let (holon, principal, changes) = record();
        let build = || {
            to_quads(&TickRecord {
                holon: &holon,
                version: 3,
                at: 1_700_000_000,
                principal: &principal,
                changes: changes.clone(),
                admitted: false,
                violations: 2,
            })
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
        assert!(build().iter().any(|q| q.contains("violations")));
    }

    #[test]
    fn a_rejected_tick_is_still_recorded() {
        // The log records attempts, not just successes: a boundary that silently discards
        // what it refused leaves no evidence that anything was tried.
        let (holon, principal, changes) = record();
        let quads = to_quads(&TickRecord {
            holon: &holon,
            version: 2,
            at: 0,
            principal: &principal,
            changes,
            admitted: false,
            violations: 1,
        });
        let joined = quads
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("admitted"));
        assert!(joined.contains(r#""false""#), "{joined}");
    }
}
