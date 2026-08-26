//! Options a query can be run with.
//!
//! Each of these exists because the reused evaluator already supports it and HOLOS was not
//! using it. They are grouped into one struct rather than added as parameters so that the
//! call sites — the command line, the HTTP server, the Python binding — stay readable as
//! the list grows.

use oxrdf::{GraphName, NamedOrBlankNode, Term, Variable};
use spareval::CancellationToken;
use std::time::Duration;

/// How a query should be evaluated.
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    /// Base IRI for resolving relative IRIs in the query text.
    pub base_iri: Option<String>,

    /// Graphs to treat as the default graph, as `FROM` / `default-graph-uri` specify.
    ///
    /// `None` leaves the store's own default graph in place. `Some(vec![])` makes the
    /// default graph empty, which is what a client asking for no graphs at all means.
    pub default_graphs: Option<Vec<GraphName>>,

    /// Graphs `GRAPH ?g` may range over, as `FROM NAMED` / `named-graph-uri` specify.
    ///
    /// `None` leaves every named graph in the store available.
    pub named_graphs: Option<Vec<NamedOrBlankNode>>,

    /// Treat the union of the *named* graphs as the default graph.
    ///
    /// Not the union of everything: the store's own default graph is not among them. That
    /// is the SPARQL reading, and it surprises people often enough to be worth stating.
    pub union_default_graph: bool,

    /// Values to substitute for variables before evaluation.
    ///
    /// This is parameter binding, and it is the *only* way to get a value into a query
    /// without building query text around it. Interpolating a term into SPARQL source is
    /// how injection happens; substitution cannot inject because the value never passes
    /// through the parser.
    pub substitutions: Vec<(Variable, Term)>,

    /// Give up after this long.
    ///
    /// # How it is enforced, and what it cannot stop
    ///
    /// Two layers, because one is not enough:
    ///
    /// 1. **The evaluator's cancellation token.** The reused evaluator checks it whenever
    ///    it pulls from the dataset — so a query that is *reading* stops promptly. That is
    ///    what long-running queries over large stores are doing.
    /// 2. **The solution iterator.** Every row yielded is checked against the deadline, and
    ///    abandoning the iterator stops the work behind it, because evaluation is lazy.
    ///
    /// What neither layer stops is a query that blocks *inside a single step* without
    /// touching the store — a cross product over small relations already held in memory can
    /// spin for a long time between dataset reads. Bounding that needs cooperative
    /// cancellation inside the join operators, which is upstream work. A row-count or
    /// result-size cap is the practical defence, and this is not one.
    pub timeout: Option<Duration>,

    /// Collect the query plan, with per-operator statistics.
    pub explain: bool,

    /// Reorder each basic graph pattern by estimated cardinality before evaluating.
    ///
    /// The reused optimiser cannot be given statistics — there is no injection point — but
    /// written order survives into its plan, so applying the estimate *before* the query
    /// reaches it is the one lever available from outside. See `holos_stats::reorder`.
    ///
    /// Statistics are a snapshot: build them once and share the `Arc`. A stale snapshot
    /// makes plans worse, never wrong, because reordering a BGP cannot change its answer.
    pub reorder_with: Option<std::sync::Arc<holos_stats::Statistics>>,
    /// An R-tree to narrow GeoSPARQL topology relations with, when one side is constant.
    ///
    /// Optional and checked: an index built before a write is missing what the write added,
    /// so `crate::topology` verifies it still describes the store and falls back to a full
    /// scan if not. Supplying a stale one costs time, never correctness.
    pub spatial: Option<std::sync::Arc<crate::spatial::SpatialIndex>>,
}

impl QueryOptions {
    /// Nothing set: the store's own dataset, no timeout, no substitutions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the base IRI.
    #[must_use]
    pub fn with_base_iri(mut self, base: impl Into<String>) -> Self {
        self.base_iri = Some(base.into());
        self
    }

    /// Adds a graph to the default graph.
    #[must_use]
    pub fn with_default_graph(mut self, graph: GraphName) -> Self {
        self.default_graphs.get_or_insert_with(Vec::new).push(graph);
        self
    }

    /// Adds a graph `GRAPH ?g` may range over.
    #[must_use]
    pub fn with_named_graph(mut self, graph: NamedOrBlankNode) -> Self {
        self.named_graphs.get_or_insert_with(Vec::new).push(graph);
        self
    }

    /// Binds a value to a variable without it passing through the parser.
    #[must_use]
    pub fn with_substitution(mut self, variable: impl Into<Variable>, term: impl Into<Term>) -> Self {
        self.substitutions.push((variable.into(), term.into()));
        self
    }

    /// Sets a wall-clock limit.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Asks for the plan alongside the results.
    #[must_use]
    pub fn explaining(mut self) -> Self {
        self.explain = true;
        self
    }

    /// Reorders basic graph patterns using these statistics.
    #[must_use]
    pub fn reordering(mut self, stats: std::sync::Arc<holos_stats::Statistics>) -> Self {
        self.reorder_with = Some(stats);
        self
    }

    /// Narrow topology relations with this spatial index where it applies.
    #[must_use]
    pub fn with_spatial(mut self, index: std::sync::Arc<crate::spatial::SpatialIndex>) -> Self {
        self.spatial = Some(index);
        self
    }

    /// Whether anything here changes the dataset the query sees.
    #[must_use]
    pub fn touches_dataset(&self) -> bool {
        self.default_graphs.is_some() || self.named_graphs.is_some() || self.union_default_graph
    }
}

/// A running timeout.
///
/// Holding one keeps the watchdog alive; dropping it stops the watchdog without cancelling.
/// That distinction matters — a query that finishes early must not leave a thread sleeping
/// until its deadline, and must not have its token tripped afterwards.
pub struct Deadline {
    token: CancellationToken,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for Deadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // CancellationToken has no Debug of its own; what a reader wants here is whether
        // it has fired, not the atomic behind it.
        f.debug_struct("Deadline")
            .field("expired", &self.expired())
            .finish()
    }
}

impl Deadline {
    /// Starts a watchdog that cancels `token` after `timeout`.
    #[must_use]
    pub fn start(timeout: Duration) -> Self {
        let token = CancellationToken::new();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watch_token = token.clone();
        let watch_done = std::sync::Arc::clone(&done);

        std::thread::spawn(move || {
            // Wake periodically rather than sleeping the whole timeout, so a query that
            // finishes in a millisecond does not leave a thread parked for the full
            // duration. The granularity is deliberately coarse: cancellation is a backstop,
            // not a scheduler.
            let tick = Duration::from_millis(50).min(timeout);
            let deadline = std::time::Instant::now() + timeout;
            loop {
                std::thread::sleep(tick);
                if watch_done.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    watch_token.cancel();
                    return;
                }
            }
        });

        Self { token, done }
    }

    /// The token to hand the evaluator.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Whether the deadline fired.
    #[must_use]
    pub fn expired(&self) -> bool {
        self.token.is_cancelled()
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_fires() {
        let deadline = Deadline::start(Duration::from_millis(60));
        assert!(!deadline.expired());
        std::thread::sleep(Duration::from_millis(250));
        assert!(deadline.expired());
    }

    #[test]
    fn dropping_a_deadline_does_not_cancel() {
        let token = {
            let deadline = Deadline::start(Duration::from_secs(30));
            deadline.token()
        };
        // The Deadline is gone; the token it handed out must stay uncancelled, or a query
        // that outlived its options object would be killed for no reason.
        std::thread::sleep(Duration::from_millis(120));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn options_build_up() {
        let options = QueryOptions::new()
            .with_base_iri("http://example.com/")
            .with_default_graph(GraphName::DefaultGraph)
            .with_timeout(Duration::from_secs(5))
            .explaining();
        assert!(options.touches_dataset());
        assert!(options.explain);
        assert_eq!(options.timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn plain_options_leave_the_dataset_alone() {
        assert!(!QueryOptions::new().touches_dataset());
    }
}
