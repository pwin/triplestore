//! The audit record.
//!
//! Every enterprise deployment of a data store has to answer "who saw what, and what were
//! they refused". HOLOS gets most of the way there for free, because `DESIGN.md` §9
//! already puts an append-only PROV-O event log at the centre of a holon. Access
//! decisions are one more kind of event.
//!
//! # One rule about who reads this
//!
//! An audit event says how many quads a principal was refused. That number is itself
//! sensitive — it tells the principal that hidden data exists, which is exactly what
//! [`Semantics::Filter`](crate::Semantics) is meant not to reveal. **Audit events go to
//! the operator, never back to the principal in a query response.**

use crate::policy::Modes;
use oxrdf::NamedNode;
use std::sync::Mutex;
use std::time::SystemTime;

/// What the principal attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A SPARQL query. Carries the query text.
    Query(String),
    /// A scan of a pattern, below the query level.
    Scan,
    /// An insert or delete.
    Write,
    /// A change to shapes, rules, or policy.
    Administer,
}

/// How it went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Completed with nothing withheld.
    Allowed,
    /// Completed, but some quads were filtered out.
    PartiallyFiltered,
    /// Refused outright.
    Denied,
}

/// One decision, as it will be written to the event log.
#[derive(Debug, Clone)]
pub struct AccessEvent {
    /// When.
    pub at: SystemTime,
    /// Who.
    pub principal: NamedNode,
    /// What they tried.
    pub action: Action,
    /// In what mode.
    pub mode: Modes,
    /// How it went.
    pub outcome: Outcome,
    /// How many quads the policy withheld. Operator-only — see the module note.
    pub filtered_quads: u64,
    /// Free-text detail for the operator.
    pub detail: String,
}

/// Somewhere audit events go.
///
/// Implementations forward to the holon event graph, to syslog, to OTLP, or to whatever
/// SIEM the deployment runs. The trait is `Send + Sync` so a sink can be shared across
/// query threads.
pub trait AuditSink: Send + Sync {
    /// Records one event. Must not block the query path for long.
    fn record(&self, event: &AccessEvent);
}

/// Discards everything. The default for embedded use where the host does its own logging.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl AuditSink for NullSink {
    fn record(&self, _event: &AccessEvent) {}
}

/// Keeps events in memory. For tests, and for small deployments that drain it
/// periodically.
#[derive(Debug, Default)]
pub struct CollectingSink(Mutex<Vec<AccessEvent>>);

impl CollectingSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn events(&self) -> Vec<AccessEvent> {
        self.0.lock().expect("audit sink poisoned").clone()
    }

    /// Removes and returns everything recorded so far.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    pub fn drain(&self) -> Vec<AccessEvent> {
        std::mem::take(&mut *self.0.lock().expect("audit sink poisoned"))
    }
}

impl AuditSink for CollectingSink {
    fn record(&self, event: &AccessEvent) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collecting_sink_round_trips() {
        let sink = CollectingSink::new();
        sink.record(&AccessEvent {
            at: SystemTime::now(),
            principal: NamedNode::new_unchecked("urn:holos:principal:a"),
            action: Action::Query("SELECT * WHERE { ?s ?p ?o }".into()),
            mode: Modes::READ,
            outcome: Outcome::PartiallyFiltered,
            filtered_quads: 3,
            detail: "salary predicate denied".into(),
        });
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].filtered_quads, 3);
        assert_eq!(events[0].outcome, Outcome::PartiallyFiltered);
        assert_eq!(sink.drain().len(), 1);
        assert!(sink.events().is_empty(), "drain must empty the sink");
    }
}
