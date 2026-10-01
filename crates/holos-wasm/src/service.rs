//! `SERVICE` for a host that can only fetch asynchronously.
//!
//! # The problem this shape solves
//!
//! `spareval::DefaultServiceHandler::handle` is synchronous, and in a browser the only way to
//! reach an endpoint is `fetch`, which is not. You cannot await a Promise from synchronous
//! wasm. The three ways round that are not equal:
//!
//! * **Synchronous `XMLHttpRequest`** fits the trait exactly and is the least code. It is also
//!   deprecated, logs a warning in every browser, blocks the tab while it waits, and may stop
//!   working.
//! * **A worker blocking on `Atomics.wait`** is proper blocking with no deprecated API, and
//!   needs COOP/COEP cross-origin-isolation headers wherever the app is hosted. It breaks
//!   quietly if those headers are ever lost.
//! * **Asking twice**, which is this. The handler answers from a cache the host fills. A pass
//!   that finds the cache empty records what it wanted and returns no solutions; the host
//!   fetches those with an ordinary `fetch`, puts them in, and the query is run again. Repeat
//!   until a pass asks for nothing new, and that pass's answer is the answer.
//!
//! The third costs repeated evaluation and buys three things: no deprecated API, no hosting
//! requirement, and the allow-list ends up in the host, which is where it belongs. Nothing
//! here can reach the network at all -- it has no way to -- so a `SERVICE` IRI this crate was
//! never given an answer for is simply unanswered rather than fetched. That is a stronger
//! position than an allow-list inside the engine, and it is why the SSRF objection in
//! `holos-engine`'s `service` module does not transfer: there is no request for an attacker to
//! point anywhere.
//!
//! # Why the cache key is a query string
//!
//! The trait hands over a `GraphPattern`, which means nothing to a JavaScript caller. Wrapping
//! it in a `SELECT` of the variables it binds -- the same move `LocalServiceHandler` makes --
//! produces a query the host can POST as it stands, and makes the thing it fetched and the
//! thing the cache is keyed on identical. The host never sees a pattern.
//!
//! # Why this terminates
//!
//! A pass can discover a request it could not have discovered earlier: a `SERVICE` whose
//! pattern carries bindings from an earlier join only takes its final shape once that join has
//! rows, and the first pass gives it none. So each round can reveal more, exactly as the rules
//! engine's semi-naive iteration does. It stops because the set of requests grows
//! monotonically and the host caps the rounds -- an endpoint that returns different answers to
//! the same query on every call would otherwise iterate forever, and that is a broken endpoint
//! rather than a case to accommodate.
use holos_engine::service::pattern_variables;
use oxiri::Iri;
use oxrdf::NamedNode;
use spareval::{DefaultServiceHandler, QueryEvaluationError, QuerySolutionIter};
use spargebra::algebra::GraphPattern;
use sparesults::{QueryResultsFormat, QueryResultsParser, ReaderQueryResultsParserOutput};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// One thing the host has to fetch: an endpoint, and the query to send it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pending {
    pub endpoint: String,
    pub query: String,
}

/// The cache, and what the last pass could not answer.
///
/// `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` because `DefaultServiceHandler` is `Send +
/// Sync`. wasm is single-threaded, so the lock is never contended; it is here to satisfy the
/// bound rather than to coordinate anything.
#[derive(Debug, Default)]
struct Shared {
    /// `(endpoint, query)` to the endpoint's SPARQL Results JSON.
    answers: BTreeMap<(String, String), String>,
    /// What this pass asked for and did not find, in a stable order.
    missing: BTreeMap<Pending, ()>,
}

/// Answers `SERVICE` from a cache the host fills, and records what it could not answer.
#[derive(Clone, Debug, Default)]
pub struct CachingServiceHandler {
    shared: Arc<Mutex<Shared>>,
}

impl CachingServiceHandler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an endpoint's answer to a query, as SPARQL Results JSON.
    pub fn cache(&self, endpoint: &str, query: &str, results_json: &str) {
        if let Ok(mut shared) = self.shared.lock() {
            shared
                .answers
                .insert((endpoint.to_owned(), query.to_owned()), results_json.to_owned());
        }
    }

    /// What the last pass wanted and did not have. Cleared by [`Self::begin_pass`].
    #[must_use]
    pub fn pending(&self) -> Vec<Pending> {
        self.shared
            .lock()
            .map(|s| s.missing.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Forgets what the previous pass asked for, keeping the answers.
    ///
    /// Called before each pass so `pending()` describes *this* pass. Without it a request that
    /// has since been answered would still be listed, and the loop would never see an empty
    /// list to stop on.
    pub fn begin_pass(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.missing.clear();
        }
    }
}

/// The pattern as a query an endpoint can answer.
///
/// Projecting the pattern's own variables is what keeps the join with the outer query correct;
/// `LocalServiceHandler` does the same and for the same reason.
fn pattern_as_query(pattern: &GraphPattern, base_iri: Option<&Iri<String>>) -> String {
    spargebra::Query::Select {
        dataset: None,
        pattern: GraphPattern::Project {
            inner: Box::new(pattern.clone()),
            variables: pattern_variables(pattern),
        },
        base_iri: base_iri.cloned(),
    }
    .to_string()
}

impl DefaultServiceHandler for CachingServiceHandler {
    type Error = QueryEvaluationError;

    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, Self::Error> {
        let endpoint = service_name.as_str().to_owned();
        let query = pattern_as_query(pattern, base_iri);
        let variables: Arc<[oxrdf::Variable]> = Arc::from(pattern_variables(pattern));

        let cached = self
            .shared
            .lock()
            .ok()
            .and_then(|s| s.answers.get(&(endpoint.clone(), query.clone())).cloned());

        let Some(json) = cached else {
            // Recorded and answered with nothing, rather than failed. Failing would end the
            // evaluation at the first unanswered endpoint, so one pass would discover one
            // request; returning empty lets the pass run to the end and name everything it
            // wanted. This pass's *results* are wrong by construction and the caller is
            // expected to discard them -- which is safe only because `pending()` is non-empty
            // to say so.
            if let Ok(mut shared) = self.shared.lock() {
                shared.missing.insert(Pending { endpoint, query }, ());
            }
            return Ok(QuerySolutionIter::new(variables, std::iter::empty()));
        };

        // The endpoint's own variables, not the pattern's: a SELECT may answer with fewer
        // columns than were asked for, and the join has to see what actually came back.
        let parser = QueryResultsParser::from_format(QueryResultsFormat::Json);
        let reader = parser
            .for_reader(json.as_bytes())
            .map_err(|e| QueryEvaluationError::Service(Box::new(ServiceFailure(format!(
                "{endpoint} returned results that could not be read: {e}"
            )))))?;
        let ReaderQueryResultsParserOutput::Solutions(solutions) = reader else {
            return Err(QueryEvaluationError::Service(Box::new(ServiceFailure(
                format!("{endpoint} answered a SELECT with a boolean"),
            ))));
        };

        let endpoint_vars: Arc<[oxrdf::Variable]> = Arc::from(solutions.variables().to_vec());
        let mut rows = Vec::new();
        for solution in solutions {
            let solution = solution.map_err(|e| {
                QueryEvaluationError::Service(Box::new(ServiceFailure(format!(
                    "{endpoint} returned a row that could not be read: {e}"
                ))))
            })?;
            rows.push(Ok(solution));
        }
        Ok(QuerySolutionIter::new(endpoint_vars, rows.into_iter()))
    }
}

/// A `SERVICE` failure with a message, since `QueryEvaluationError::Service` wants an error.
#[derive(Debug)]
struct ServiceFailure(String);

impl std::fmt::Display for ServiceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ServiceFailure {}
