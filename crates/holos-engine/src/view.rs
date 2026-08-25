//! The dataset view — the single chokepoint every read passes through.
//!
//! [`DatasetView`] implements `spareval`'s `QueryableDataset` over a [`Store`], which is
//! how HOLOS gets a conformant SPARQL 1.2 evaluator without writing one (`DESIGN.md` §4,
//! L0/L3: reuse the front end, replace the storage and, later, the planner).
//!
//! It is also where access policy is applied. Because *every* SPARQL operator — joins,
//! `OPTIONAL`, `MINUS`, `NOT EXISTS`, property paths, aggregates, subqueries — ultimately
//! obtains its quads from [`DatasetView::internal_quads_for_pattern`], filtering here
//! gives the property stated in [`holos_security::policy`]: the answer equals the answer
//! over the sub-dataset the principal may see. No operator can route around it, because no
//! operator has another source of quads.

// `Option<Option<TermId>>` is `spareval`'s own signature for a graph filter, not ours.
#![allow(clippy::option_option)]

use holos_core::{Tag, TermId, PAYLOAD_MASK};
use holos_security::{CompiledPolicy, Decision, Modes, Semantics};
use holos_store::{GraphFilter, StorageError, Store};
use oxrdf::{GraphName, Quad, Term};
use rustc_hash::FxHashMap;
use spareval::{InternalQuad, QueryableDataset};
use std::cell::{Cell, RefCell};

/// Something that went wrong while reading.
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    /// The policy refused, under [`Semantics::Fail`].
    #[error("access denied by policy")]
    AccessDenied,
    /// The storage layer could not answer.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A term id that this store's dictionary never issued.
    #[error("term {0:?} is not in the dictionary — the store may be corrupt")]
    UnknownTerm(TermId),
    /// More distinct query constants than the ephemeral space can hold.
    #[error("too many unknown terms in one query")]
    EphemeralExhausted,
}

/// Terms a query mentions that the store has never seen.
///
/// A pattern naming an unknown term matches nothing, but the term still has to round-trip
/// through the evaluator: `FILTER(?x = "never-seen")` must compare correctly, and a
/// `VALUES` clause must be able to bind and then return it. Giving them ids in a
/// per-query side table keeps the query read-only — the dictionary is never mutated to
/// answer a question.
#[derive(Debug, Default)]
struct Ephemeral {
    terms: Vec<Term>,
    index: FxHashMap<Term, u64>,
}

/// A read-only, policy-filtered view of a [`Store`].
#[derive(Debug)]
pub struct DatasetView<'a> {
    store: &'a Store,
    policy: &'a CompiledPolicy,
    ephemeral: RefCell<Ephemeral>,
    /// Quads the policy withheld during this view's lifetime. Operator telemetry only —
    /// never returned to the principal, because the count reveals that hidden data exists.
    filtered: Cell<u64>,
}

impl<'a> DatasetView<'a> {
    /// Opens a view.
    #[must_use]
    pub fn new(store: &'a Store, policy: &'a CompiledPolicy) -> Self {
        Self {
            store,
            policy,
            ephemeral: RefCell::new(Ephemeral::default()),
            filtered: Cell::new(0),
        }
    }

    /// The underlying store.
    #[must_use]
    pub fn store(&self) -> &'a Store {
        self.store
    }

    /// How many quads the policy has withheld through this view.
    #[must_use]
    pub fn filtered_count(&self) -> u64 {
        self.filtered.get()
    }

    /// Every quad this principal may read, decoded, optionally restricted to one graph.
    ///
    /// `graph` of `None` means the whole dataset — default graph and named graphs alike.
    ///
    /// This exists for the graph-level update operations. `CLEAR`, `DROP` and the like
    /// name a *wildcard* rather than a pattern, and a wildcard is exactly where a policy
    /// leak would hide: if deletion enumerated the store directly it would remove quads a
    /// `SELECT` would never have shown the principal, and the difference in what came back
    /// afterwards would reveal that they had been there. Going through the same
    /// `decide_quad` call as every other read is what stops that.
    ///
    /// The result is materialised because the caller is about to mutate the store, which
    /// cannot happen while a scan borrows it.
    ///
    /// # Errors
    ///
    /// Propagates storage failures, and under `Semantics::Fail` the policy's refusal.
    pub fn visible_quads(&self, graph: Option<&GraphName>) -> Result<Vec<Quad>, ViewError> {
        let filter = match graph {
            None => GraphFilter::Any,
            Some(GraphName::DefaultGraph) => GraphFilter::Default,
            Some(name) => {
                let Some(id) = self.store.lookup_term(graph_name_term(name))? else {
                    // A graph the dictionary never interned holds nothing.
                    return Ok(Vec::new());
                };
                if self.policy.graph_is_wholly_denied(Some(id)) {
                    return match self.policy.semantics() {
                        Semantics::Filter => Ok(Vec::new()),
                        Semantics::Fail => Err(ViewError::AccessDenied),
                    };
                }
                GraphFilter::Named(id)
            }
        };

        let mut out = Vec::new();
        for quad in self.store.quads_for_pattern(None, None, None, filter) {
            let quad = quad?;
            match self.policy.decide_quad(quad, Modes::READ) {
                Decision::Allow => out.push(self.store.decode_quad(quad)?),
                Decision::Filter => self.filtered.set(self.filtered.get() + 1),
                Decision::Fail => return Err(ViewError::AccessDenied),
            }
        }
        Ok(out)
    }

    fn intern_ephemeral(&self, term: Term) -> Result<TermId, ViewError> {
        let mut e = self.ephemeral.borrow_mut();
        if let Some(i) = e.index.get(&term) {
            return Ok(TermId::new(Tag::Ephemeral, *i));
        }
        let i = e.terms.len() as u64;
        if i > PAYLOAD_MASK {
            return Err(ViewError::EphemeralExhausted);
        }
        e.terms.push(term.clone());
        e.index.insert(term, i);
        Ok(TermId::new(Tag::Ephemeral, i))
    }
}

/// A graph name as a term, for dictionary lookup.
fn graph_name_term(graph: &GraphName) -> oxrdf::TermRef<'_> {
    match graph {
        GraphName::NamedNode(n) => n.as_ref().into(),
        GraphName::BlankNode(b) => b.as_ref().into(),
        // Callers filter this case out before reaching here.
        GraphName::DefaultGraph => oxrdf::TermRef::NamedNode(oxrdf::NamedNodeRef::new_unchecked(
            "urn:holos:default-graph",
        )),
    }
}

/// Named graphs holding at least one quad this principal may read.
///
/// A graph the principal cannot read must not be enumerable at all: listing it leaks that
/// it exists, which is information the policy meant to withhold.
fn visible_named_graphs(view: &DatasetView<'_>) -> Result<Vec<TermId>, ViewError> {
    let mut out = Vec::new();
    for graph in view.store.named_graphs()? {
        if view.policy.graph_is_wholly_denied(Some(graph)) {
            continue;
        }
        let mut readable = false;
        for quad in view
            .store
            .quads_for_pattern(None, None, None, GraphFilter::Named(graph))
        {
            if view.policy.decide_quad(quad?, Modes::READ) == Decision::Allow {
                readable = true;
                break;
            }
        }
        if readable {
            out.push(graph);
        }
    }
    Ok(out)
}

impl<'a> QueryableDataset<'a> for &'a DatasetView<'a> {
    type InternalTerm = TermId;
    type Error = ViewError;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&TermId>,
        predicate: Option<&TermId>,
        object: Option<&TermId>,
        graph_name: Option<Option<&TermId>>,
    ) -> impl Iterator<Item = Result<InternalQuad<TermId>, ViewError>> + use<'a> {
        let view: &'a DatasetView<'a> = self;
        let graph = match graph_name {
            Some(None) => GraphFilter::Default,
            Some(Some(g)) => GraphFilter::Named(*g),
            // SPARQL's `GRAPH ?g` ranges over named graphs only, never the default graph.
            None => GraphFilter::AnyNamed,
        };

        // Skipping a wholly denied graph is not just an optimisation: under Fail
        // semantics it makes the refusal about the graph the query asked for, rather
        // than about whichever quad happened to be scanned first.
        if let GraphFilter::Named(g) = graph {
            if view.policy.graph_is_wholly_denied(Some(g)) {
                return match view.policy.semantics() {
                    // Under Filter semantics the graph is simply empty for this
                    // principal, which is exactly what "the answer over the visible
                    // sub-dataset" means.
                    Semantics::Filter => {
                        Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + 'a>
                    }
                    Semantics::Fail => Box::new(std::iter::once(Err(ViewError::AccessDenied))),
                };
            }
        }

        let (s, p, o) = (subject.copied(), predicate.copied(), object.copied());
        Box::new(
            view.store
                .quads_for_pattern(s, p, o, graph)
                .filter_map(move |quad| {
                    let quad = match quad {
                        Ok(quad) => quad,
                        Err(e) => return Some(Err(ViewError::from(e))),
                    };
                    match view.policy.decide_quad(quad, Modes::READ) {
                        Decision::Allow => Some(Ok(InternalQuad {
                            subject: quad.subject,
                            predicate: quad.predicate,
                            object: quad.object,
                            graph_name: quad.graph_name,
                        })),
                        Decision::Filter => {
                            view.filtered.set(view.filtered.get() + 1);
                            None
                        }
                        Decision::Fail => Some(Err(ViewError::AccessDenied)),
                    }
                }),
        )
    }

    fn internal_named_graphs(&self) -> impl Iterator<Item = Result<TermId, ViewError>> + use<'a> {
        let view: &'a DatasetView<'a> = self;
        match visible_named_graphs(view) {
            Ok(graphs) => graphs.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(e) => vec![Err(e)],
        }
        .into_iter()
    }

    fn internalize_term(&self, term: Term) -> Result<TermId, ViewError> {
        match self.store.lookup_term(term.as_ref())? {
            Some(id) => Ok(id),
            None => self.intern_ephemeral(term),
        }
    }

    fn externalize_term(&self, term: TermId) -> Result<Term, ViewError> {
        if term.tag() == Tag::Ephemeral {
            return self
                .ephemeral
                .borrow()
                .terms
                .get(usize::try_from(term.payload()).map_err(|_| ViewError::UnknownTerm(term))?)
                .cloned()
                .ok_or(ViewError::UnknownTerm(term));
        }
        self.store
            .decode_term(term)?
            .ok_or(ViewError::UnknownTerm(term))
    }
}
