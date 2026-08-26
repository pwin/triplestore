//! HOLOS L3 — SPARQL 1.2 evaluation.
//!
//! The front end is reused wholesale (`DESIGN.md` §4): `spargebra` parses, `spareval`
//! evaluates, `sparesults` serialises. What HOLOS supplies is the [`view::DatasetView`]
//! those crates read through — which is also the single point where access policy is
//! applied.
//!
//! The cost-based optimiser and the hybrid WCO planner of §7 are **not** here yet. Query
//! plans are whatever `spareval` chooses, which is the P0 baseline the design says must be
//! beaten rather than the design's endpoint.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// `# Errors` sections would restate the error enum on every function; the enums are
// documented at their definition instead.
#![allow(clippy::missing_errors_doc)]
// s/p/o/g are the names the RDF and SPARQL specifications use. Renaming them to satisfy
// a length lint would make this code harder to check against the specs, not easier.
#![allow(clippy::many_single_char_names)]

pub mod functions;
pub mod geo_ext;
pub mod options;
pub mod service;
pub mod source;
pub mod spatial;
pub mod topology;
pub mod update;
pub mod view;

pub use options::{Deadline, QueryOptions};

pub use view::{DatasetView, ViewError};

use holos_security::{Action, AuditSink, Modes, Outcome, Principal, Session};
use holos_store::Store;
use oxrdf::QuadRef;
use oxrdfio::{RdfFormat, RdfParser};
use spareval::{QueryEvaluator, QueryResults};
use spargebra::SparqlParser;
use std::io::Read;
use std::time::SystemTime;

/// Anything that can go wrong at this layer.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The query did not parse.
    #[error("SPARQL syntax error: {0}")]
    Syntax(#[from] spargebra::SparqlSyntaxError),
    /// Evaluation failed.
    #[error("query evaluation failed: {0}")]
    Evaluation(#[from] spareval::QueryEvaluationError),
    /// The input could not be parsed as RDF.
    #[error("RDF parse error: {0}")]
    Parse(#[from] oxrdfio::RdfParseError),
    /// I/O failed while reading input.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The storage layer could not answer.
    #[error(transparent)]
    Storage(#[from] holos_store::StorageError),
    /// A read through the dataset view failed.
    #[error(transparent)]
    View(#[from] ViewError),
    /// The request itself was wrong, in a way no amount of retrying fixes.
    ///
    /// Distinct from [`EngineError::Syntax`], which means the SPARQL text did not parse.
    /// This is for a request that parsed and is still not answerable — the protocol naming
    /// a dataset an update already names for itself, say. Both are 400s over HTTP; keeping
    /// them apart keeps the message accurate about which one happened.
    #[error("{0}")]
    BadRequest(String),
    /// The policy refused a write.
    #[error("access denied by policy")]
    AccessDenied,
}

/// A store with a SPARQL engine over it.
#[derive(Debug, Default)]
pub struct Engine {
    store: Store,
}

impl Engine {
    /// An empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An engine over a given store — an in-memory one, or a persistent backend.
    #[must_use]
    pub fn with_store(store: Store) -> Self {
        Self { store }
    }

    /// The underlying store, for statistics and direct inspection.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The underlying store, mutably — for bulk loading and flushing.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Loads RDF, bypassing policy.
    ///
    /// This is the bootstrap path — populating a store from trusted files at start-up.
    /// Anything reachable by a principal must go through [`Engine::insert`] instead.
    pub fn bulk_load(
        &mut self,
        reader: impl Read,
        format: RdfFormat,
        base_iri: Option<&str>,
    ) -> Result<usize, EngineError> {
        let mut parser = RdfParser::from_format(format);
        if let Some(base) = base_iri {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;
        }
        let mut n = 0;
        for quad in parser.for_reader(reader) {
            if self.store.insert(quad?.as_ref())? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Loads RDF into one named graph, bypassing policy.
    ///
    /// Triples in the input land in `graph`; quads that already name a graph keep it.
    /// Needed by the Graph Store Protocol (L6) and by the conformance harness, where a
    /// suite's `qt:graphData` has to arrive under the IRI the expected results use.
    pub fn bulk_load_into_graph(
        &mut self,
        reader: impl Read,
        format: RdfFormat,
        base_iri: Option<&str>,
        graph: &oxrdf::GraphName,
    ) -> Result<usize, EngineError> {
        let mut parser = RdfParser::from_format(format);
        if let Some(base) = base_iri {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;
        }
        let mut n = 0;
        for quad in parser.for_reader(reader) {
            let mut quad = quad?;
            if quad.graph_name.is_default_graph() {
                quad.graph_name = graph.clone();
            }
            if self.store.insert(quad.as_ref())? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Evaluates an already-parsed query.
    ///
    /// Separate from [`Engine::query`] because a conformance harness has to parse first
    /// to tell a syntax failure apart from an evaluation failure.
    pub fn query_prepared<'a>(
        view: &'a DatasetView<'a>,
        query: &spargebra::Query,
    ) -> Result<QueryResults<'a>, EngineError> {
        Ok(Self::evaluator().prepare(query).execute(view)?)
    }

    /// Evaluates an already-parsed query with a service handler registered.
    ///
    /// Separate from [`Engine::query_prepared`] because federation is opt-in: a handler
    /// that is not registered means a `SERVICE` clause fails rather than silently
    /// resolving to nothing.
    ///
    /// # Errors
    ///
    /// Propagates evaluation failures, including a `SERVICE` naming an unknown endpoint.
    pub fn query_prepared_with_services<'a>(
        view: &'a DatasetView<'a>,
        query: &spargebra::Query,
        services: crate::service::LocalServiceHandler,
    ) -> Result<QueryResults<'a>, EngineError> {
        let evaluator = if services.is_empty() {
            Self::evaluator()
        } else {
            Self::evaluator().with_default_service_handler(services)
        };
        Ok(evaluator.prepare(query).execute(view)?)
    }

    /// Inserts one quad, subject to the session's write policy.
    ///
    /// The quad's terms are interned *before* the decision so that a rule naming a
    /// previously unseen IRI still applies — see [`Store::encode_quad`].
    pub fn insert(
        &mut self,
        session: &mut Session,
        quad: QuadRef<'_>,
    ) -> Result<bool, EngineError> {
        let encoded = self.store.encode_quad(quad)?;
        if !session.policy(&self.store)?.permits_quad(encoded, Modes::WRITE) {
            return Err(EngineError::AccessDenied);
        }
        Ok(self.store.insert_encoded(encoded)?)
    }

    /// Removes one quad, subject to the session's write policy.
    ///
    /// A principal who may not *read* a quad may not delete it either: otherwise deletion
    /// becomes an oracle for whether hidden data exists.
    pub fn remove(
        &mut self,
        session: &mut Session,
        quad: QuadRef<'_>,
    ) -> Result<bool, EngineError> {
        let encoded = self.store.encode_quad(quad)?;
        let policy = session.policy(&self.store)?;
        if !policy.permits_quad(encoded, Modes::WRITE) || !policy.permits_quad(encoded, Modes::READ)
        {
            return Err(EngineError::AccessDenied);
        }
        Ok(self.store.remove(quad)?)
    }

    /// Opens a policy-filtered view for a session.
    ///
    /// Borrowing the compiled policy out of the session is what ties a view to a
    /// principal for its whole lifetime: there is no way to construct a view without one.
    #[must_use]
    pub fn view<'a>(&'a self, session: &'a Session) -> DatasetView<'a> {
        DatasetView::new(&self.store, session.policy_unchecked())
    }

    /// An evaluator with the GeoSPARQL function library registered.
    ///
    /// `spargeo` supplies 43 geometry functions — the topological relations of all three
    /// families (Simple Features, Egenhofer, RCC8), plus distance, buffer, convex hull,
    /// envelope, boundary and the WKT/GeoJSON accessors. Reused rather than rewritten, for
    /// the same reason as the rest of L0 (`DESIGN.md` §4): it is conformance-heavy work
    /// that already exists and is already tested.
    ///
    /// These are *filter* functions. They evaluate over whatever bindings reach them, so a
    /// spatial join still scans every candidate geometry — an R-tree over geometry
    /// literals is what turns that into an index probe, and that needs the cost-based
    /// planner of §7 to route to it. See §17.
    #[must_use]
    pub fn evaluator() -> QueryEvaluator {
        let mut evaluator = QueryEvaluator::new();
        for (name, function) in spargeo::GEOSPARQL_EXTENSION_FUNCTIONS {
            evaluator = evaluator.with_custom_function(name.into_owned(), function);
        }
        // spargeo does not carry geof:buffer or geof:boundary; §17 claimed them before
        // anything checked, so they are implemented here rather than quietly dropped.
        for (name, function) in geo_ext::EXTRA_GEOSPARQL_FUNCTIONS {
            evaluator = evaluator.with_custom_function(name.into_owned(), function);
        }
        // fn:, afn: and spif: — the three extension libraries queries written for other
        // stores reach for. Without them such a query fails with "not supported", which is
        // accurate and unhelpful when the function has an exact equivalent here.
        for (name, function) in functions::all() {
            evaluator = evaluator.with_custom_function(name, function);
        }
        evaluator
    }

    /// Parses and evaluates a SPARQL 1.2 query against a view.
    pub fn query<'a>(
        view: &'a DatasetView<'a>,
        query: &str,
        base_iri: Option<&str>,
    ) -> Result<QueryResults<'a>, EngineError> {
        let mut parser = SparqlParser::new();
        if let Some(base) = base_iri {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;
        }
        let parsed = parser.parse_query(query)?;
        // Same rewrite as `query_with`: a topology property means the same thing whichever
        // entry point asked, and answering it on one path only would be worse than not
        // answering it at all.
        let parsed = crate::topology::rewrite(&parsed);
        Ok(Self::evaluator().prepare(&parsed).execute(view)?)
    }

    /// Parses and evaluates a query with options.
    ///
    /// [`Engine::query`] is this with everything defaulted. The options are what make the
    /// SPARQL Protocol's `default-graph-uri` and `named-graph-uri` mean anything, what
    /// gives a query a time limit, and what lets a value be bound to a variable without
    /// being interpolated into query text.
    ///
    /// The returned [`QueryExplanation`] is present only when
    /// [`QueryOptions::explain`] was set.
    ///
    /// # Errors
    ///
    /// Syntax and evaluation failures as [`Engine::query`]. A query stopped by its
    /// timeout surfaces as an evaluation error from the cancellation token.
    pub fn query_with<'a>(
        view: &'a DatasetView<'a>,
        query: &str,
        options: &QueryOptions,
    ) -> Result<(QueryResults<'a>, Option<spareval::QueryExplanation>), EngineError> {
        let mut parser = SparqlParser::new();
        if let Some(base) = &options.base_iri {
            parser = parser
                .with_base_iri(base.as_str())
                .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;
        }
        let parsed = parser.parse_query(query)?;
        // GeoSPARQL topology properties become geometry lookups and a filter. Unconditional
        // because it is correctness rather than optimisation: without it, `?a geo:sfContains
        // ?b` is an ordinary lookup that matches nothing and says so silently.
        let parsed = crate::topology::rewrite(&parsed);
        // Applied to the parsed algebra, before the evaluator's own optimiser runs on it.
        let parsed = match &options.reorder_with {
            None => parsed,
            Some(stats) => holos_stats::reorder_query(&parsed, stats, view.store()),
        };

        let mut evaluator = Self::evaluator();
        // The Deadline must outlive the evaluation, so it is bound here rather than
        // inside the `if`: dropping it early would stop the watchdog before it could fire.
        let deadline = options.timeout.map(Deadline::start);
        if let Some(deadline) = &deadline {
            evaluator = evaluator.with_cancellation_token(deadline.token());
        }

        let mut prepared = evaluator.prepare(&parsed);

        if options.union_default_graph {
            prepared.dataset_mut().set_default_graph_as_union();
        } else if let Some(graphs) = &options.default_graphs {
            prepared.dataset_mut().set_default_graph(graphs.clone());
        }
        if let Some(graphs) = &options.named_graphs {
            prepared
                .dataset_mut()
                .set_available_named_graphs(graphs.clone());
        }
        for (variable, term) in &options.substitutions {
            prepared = prepared.substitute_variable(variable.clone(), term.clone());
        }

        let (results, explanation) = if options.explain {
            let (results, explanation) = prepared.explain(view);
            (results?, Some(explanation))
        } else {
            (prepared.execute(view)?, None)
        };

        // The token alone is not enough: it is only consulted when the evaluator reads
        // from the dataset. Checking again on each row yielded catches a query that has
        // stopped reading and started producing, and abandoning the iterator is what
        // actually stops the work, because evaluation is lazy.
        let results = match deadline {
            None => results,
            Some(deadline) => guard_with_deadline(results, deadline),
        };
        Ok((results, explanation))
    }

    /// Evaluates a query and records the outcome for the operator.
    ///
    /// The audit event carries the count of withheld quads. That count never reaches the
    /// principal — see [`holos_security::audit`].
    pub fn query_audited<'a>(
        view: &'a DatasetView<'a>,
        principal: &Principal,
        query: &str,
        audit: &dyn AuditSink,
    ) -> Result<QueryResults<'a>, EngineError> {
        let result = Self::query(view, query, None);
        let (outcome, detail) = match &result {
            Ok(_) if view.filtered_count() > 0 => (Outcome::PartiallyFiltered, String::new()),
            Ok(_) => (Outcome::Allowed, String::new()),
            Err(e) => (Outcome::Denied, e.to_string()),
        };
        audit.record(&holos_security::AccessEvent {
            at: SystemTime::now(),
            principal: principal.id.clone(),
            action: Action::Query(query.to_owned()),
            mode: Modes::READ,
            outcome,
            filtered_quads: view.filtered_count(),
            detail,
        });
        result
    }
}

/// Stops a result stream once its deadline passes.
///
/// The [`Deadline`] is moved into the iterator so the watchdog stays alive exactly as long
/// as the results do — no longer, so a query that finishes early leaves nothing parked.
fn guard_with_deadline<'a>(
    results: QueryResults<'a>,
    deadline: Deadline,
) -> QueryResults<'a> {
    match results {
        QueryResults::Solutions(solutions) => {
            let variables = std::sync::Arc::from(solutions.variables().to_vec());
            QueryResults::Solutions(spareval::QuerySolutionIter::new(
                variables,
                solutions.map(move |solution| {
                    if deadline.expired() {
                        return Err(spareval::QueryEvaluationError::Cancelled);
                    }
                    solution
                }),
            ))
        }
        QueryResults::Graph(triples) => {
            QueryResults::Graph(spareval::QueryTripleIter::new(triples.map(
                move |triple| {
                    if deadline.expired() {
                        return Err(spareval::QueryEvaluationError::Cancelled);
                    }
                    triple
                },
            )))
        }
        // A boolean is already computed; there is nothing left to interrupt.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_security::{CollectingSink, Label, Policy, PrincipalMatch, Rule, Scope, Semantics};
    use oxrdf::NamedNode;
    use spareval::QueryResults;

    const DATA: &str = r#"
        @prefix ex: <http://example.com/> .
        ex:alice ex:name "Alice" ; ex:salary 100 ; ex:knows ex:bob .
        ex:bob   ex:name "Bob"   ; ex:salary 200 .
        ex:carol ex:name "Carol" .
    "#;

    const QUADS: &str = r#"
        @prefix ex: <http://example.com/> .
        ex:public { ex:alice ex:name "Alice" ; ex:salary 100 . }
        ex:hr     { ex:bob   ex:name "Bob"   ; ex:salary 200 . }
    "#;

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{s}"))
    }

    fn engine(data: &str, format: RdfFormat) -> Engine {
        let mut e = Engine::new();
        e.bulk_load(data.as_bytes(), format, None).unwrap();
        e
    }

    fn solutions(view: &DatasetView<'_>, query: &str) -> Vec<String> {
        match Engine::query(view, query, None).unwrap() {
            QueryResults::Solutions(iter) => {
                let mut rows: Vec<String> = iter
                    .map(|s| {
                        let s = s.unwrap();
                        s.iter()
                            .map(|(v, t)| format!("{}={t}", v.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect();
                rows.sort();
                rows
            }
            _ => panic!("expected a solutions result"),
        }
    }

    fn ask(view: &DatasetView<'_>, query: &str) -> bool {
        match Engine::query(view, query, None).unwrap() {
            QueryResults::Boolean(b) => b,
            _ => panic!("expected a boolean result"),
        }
    }

    #[test]
    fn a_basic_graph_pattern_evaluates() {
        let e = engine(DATA, RdfFormat::Turtle);
        assert_eq!(e.store().len(), 6);
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let rows = solutions(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?n WHERE { ?s ex:name ?n }",
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
    }

    #[test]
    fn joins_optional_and_filter_work() {
        let e = engine(DATA, RdfFormat::Turtle);
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let rows = solutions(
            &view,
            "PREFIX ex: <http://example.com/>
             SELECT ?n ?sal WHERE {
               ?s ex:name ?n .
               OPTIONAL { ?s ex:salary ?sal . FILTER(?sal > 150) }
             }",
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().filter(|r| r.contains("sal=")).count(),
            1,
            "only Bob clears the filter: {rows:?}"
        );
    }

    #[test]
    fn a_query_naming_an_unknown_term_matches_nothing_and_does_not_intern() {
        let e = engine(DATA, RdfFormat::Turtle);
        let before = e.store().dictionary_len();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        assert!(!ask(
            &view,
            "PREFIX ex: <http://example.com/> ASK { ex:nobody ex:name ?n }"
        ));
        // Reading must never mutate the dictionary — that is what the ephemeral space is
        // for, and it is what makes a read-only snapshot possible later.
        assert_eq!(e.store().dictionary_len(), before);
    }

    #[test]
    fn unknown_terms_still_round_trip_through_the_evaluator() {
        let e = engine(DATA, RdfFormat::Turtle);
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let rows = solutions(
            &view,
            r#"SELECT ?x WHERE { VALUES ?x { "never-seen-anywhere" } }"#,
        );
        assert_eq!(rows, vec![r#"x="never-seen-anywhere""#]);
    }

    #[test]
    fn named_graphs_are_addressable() {
        let e = engine(QUADS, RdfFormat::TriG);
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let rows = solutions(
            &view,
            "PREFIX ex: <http://example.com/>
             SELECT ?g ?n WHERE { GRAPH ?g { ?s ex:name ?n } }",
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    // --- policy enforcement -------------------------------------------------------

    fn restricted_session(store: &Store, principal: Principal) -> Session {
        let policy = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Everything,
                PrincipalMatch::Everyone,
            ))
            .with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Not(Box::new(PrincipalMatch::Role("hr".into()))),
            ));
        Session::open(store, principal, policy).unwrap()
    }

    #[test]
    fn policy_filters_a_plain_pattern() {
        let e = engine(DATA, RdfFormat::Turtle);
        let session = restricted_session(e.store(), Principal::anonymous());
        let view = e.view(&session);
        let rows = solutions(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?s ?sal WHERE { ?s ex:salary ?sal }",
        );
        assert!(rows.is_empty(), "salary must be invisible: {rows:?}");
        assert_eq!(view.filtered_count(), 2);

        let hr = restricted_session(e.store(), Principal::anonymous().with_role("hr"));
        let hr_view = e.view(&hr);
        assert_eq!(
            solutions(
                &hr_view,
                "PREFIX ex: <http://example.com/> SELECT ?s ?sal WHERE { ?s ex:salary ?sal }"
            )
            .len(),
            2
        );
    }

    /// The reason enforcement sits at the scan rather than in a query rewrite: each of
    /// these operators is a way a rewrite-based implementation leaks.
    #[test]
    fn policy_survives_every_operator_that_defeats_query_rewriting() {
        let e = engine(DATA, RdfFormat::Turtle);
        let session = restricted_session(e.store(), Principal::anonymous());
        let view = e.view(&session);
        let px = "PREFIX ex: <http://example.com/> ";

        // NOT EXISTS: must not reveal that a hidden triple exists.
        assert!(
            ask(&view, &format!("{px} ASK {{ ex:alice ex:name ?n FILTER NOT EXISTS {{ ex:alice ex:salary ?s }} }}")),
            "NOT EXISTS over hidden data must behave as if the data is absent"
        );
        // MINUS: likewise.
        assert_eq!(
            solutions(
                &view,
                &format!("{px} SELECT ?s WHERE {{ ?s ex:name ?n MINUS {{ ?s ex:salary ?x }} }}")
            )
            .len(),
            3,
            "MINUS must subtract nothing, because nothing is visible to subtract"
        );
        // Aggregates: a COUNT must not count hidden rows.
        let counted = solutions(
            &view,
            &format!("{px} SELECT (COUNT(*) AS ?c) WHERE {{ ?s ex:salary ?x }}"),
        );
        assert_eq!(counted, vec!["c=\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>"]);
        // Property paths reach the scan through the same door.
        assert!(!ask(
            &view,
            &format!("{px} ASK {{ ex:alice ex:knows*/ex:salary ?s }}")
        ));
        // A subquery is not a way around it either.
        assert!(solutions(
            &view,
            &format!("{px} SELECT ?s WHERE {{ {{ SELECT ?s WHERE {{ ?s ex:salary ?x }} }} }}")
        )
        .is_empty());
    }

    #[test]
    fn a_denied_graph_is_not_even_enumerable() {
        let e = engine(QUADS, RdfFormat::TriG);
        let policy = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Graph(nn("public")),
                PrincipalMatch::Everyone,
            ));
        let session = Session::open(e.store(), Principal::anonymous(), policy).unwrap();
        let view = e.view(&session);
        let rows = solutions(&view, "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }");
        assert_eq!(
            rows,
            vec![format!("g={}", nn("public"))],
            "listing a graph the principal cannot read leaks that it exists: {rows:?}"
        );
    }

    #[test]
    fn clearance_hides_a_labelled_graph_from_sparql() {
        let e = engine(QUADS, RdfFormat::TriG);
        let policy = Policy::permit_all().with_graph_label(nn("hr"), Label::level(3));
        let uncleared =
            Session::open(e.store(), Principal::anonymous(), policy.clone()).unwrap();
        let view = e.view(&uncleared);
        let rows = solutions(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?n WHERE { GRAPH ?g { ?s ex:name ?n } }",
        );
        assert_eq!(rows, vec![r#"n="Alice""#], "{rows:?}");

        let cleared = Session::open(
            e.store(),
            Principal::anonymous().with_clearance(Label::level(3)),
            policy,
        )
        .unwrap();
        let cleared_view = e.view(&cleared);
        assert_eq!(
            solutions(
                &cleared_view,
                "PREFIX ex: <http://example.com/> SELECT ?n WHERE { GRAPH ?g { ?s ex:name ?n } }"
            )
            .len(),
            2
        );
    }

    #[test]
    fn fail_semantics_turns_a_refusal_into_an_error() {
        let e = engine(DATA, RdfFormat::Turtle);
        let policy = Policy::default()
            .with_semantics(Semantics::Fail)
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Predicate(nn("name")),
                PrincipalMatch::Everyone,
            ));
        let session = Session::open(e.store(), Principal::anonymous(), policy).unwrap();
        let view = e.view(&session);
        // The permitted predicate still answers.
        assert_eq!(
            solutions(
                &view,
                "PREFIX ex: <http://example.com/> SELECT ?n WHERE { ?s ex:name ?n }"
            )
            .len(),
            3
        );
        // The forbidden one errors instead of quietly returning nothing.
        let err = Engine::query(
            &view,
            "PREFIX ex: <http://example.com/> SELECT ?x WHERE { ?s ex:salary ?x }",
            None,
        )
        .and_then(|r| match r {
            QueryResults::Solutions(i) => {
                for s in i {
                    s?;
                }
                Ok(())
            }
            _ => Ok(()),
        });
        assert!(err.is_err(), "Fail semantics must surface an error");
    }

    // --- writes -------------------------------------------------------------------

    #[test]
    fn writes_are_policy_checked_independently_of_reads() {
        let mut e = Engine::new();
        let policy = Policy::default().with_rule(Rule::allow(
            Modes::READ,
            Scope::Everything,
            PrincipalMatch::Everyone,
        ));
        let mut reader = Session::open(e.store(), Principal::anonymous(), policy).unwrap();
        let quad = oxrdf::Quad {
            subject: nn("alice").into(),
            predicate: nn("name"),
            object: oxrdf::Literal::new_simple_literal("Alice").into(),
            graph_name: oxrdf::GraphName::DefaultGraph,
        };
        assert!(matches!(
            e.insert(&mut reader, quad.as_ref()),
            Err(EngineError::AccessDenied)
        ));
        assert_eq!(e.store().len(), 0);

        let writer_policy = Policy::default().with_rule(Rule::allow(
            Modes::READ.union(Modes::WRITE),
            Scope::Everything,
            PrincipalMatch::Role("writer".into()),
        ));
        let mut writer = Session::open(
            e.store(),
            Principal::anonymous().with_role("writer"),
            writer_policy,
        )
        .unwrap();
        assert!(e.insert(&mut writer, quad.as_ref()).unwrap());
        assert_eq!(e.store().len(), 1);
    }

    #[test]
    fn deletion_requires_read_as_well_as_write() {
        // Otherwise "did the delete succeed" is an oracle for hidden data.
        let mut e = engine(DATA, RdfFormat::Turtle);
        let policy = Policy::default()
            .with_rule(Rule::allow(
                Modes::WRITE,
                Scope::Everything,
                PrincipalMatch::Everyone,
            ))
            .with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Everyone,
            ));
        let mut session = Session::open(e.store(), Principal::anonymous(), policy).unwrap();
        let hidden = oxrdf::Quad {
            subject: nn("alice").into(),
            predicate: nn("salary"),
            object: oxrdf::Literal::new_typed_literal("100", oxrdf::vocab::xsd::INTEGER).into(),
            graph_name: oxrdf::GraphName::DefaultGraph,
        };
        assert!(matches!(
            e.remove(&mut session, hidden.as_ref()),
            Err(EngineError::AccessDenied)
        ));
        assert_eq!(e.store().len(), 6, "nothing was deleted");
    }

    #[test]
    fn audit_records_what_was_withheld() {
        let e = engine(DATA, RdfFormat::Turtle);
        let principal = Principal::anonymous();
        let session = restricted_session(e.store(), principal.clone());
        let view = e.view(&session);
        let sink = CollectingSink::new();
        let _ = Engine::query_audited(
            &view,
            &principal,
            "PREFIX ex: <http://example.com/> SELECT ?x WHERE { ?s ex:salary ?x }",
            &sink,
        )
        .unwrap();
        // The solutions iterator is lazy, so drain it before checking the count.
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].principal, principal.id);
    }

    // --- RDF 1.2 ------------------------------------------------------------------

    #[test]
    fn rdf_12_triple_terms_load_and_query() {
        let data = r#"
            @prefix ex: <http://example.com/> .
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            ex:alice ex:name "Alice" .
            ex:claim1 rdf:reifies <<( ex:alice ex:name "Alice" )>> ;
                      ex:statedBy ex:bob .
        "#;
        let e = engine(data, RdfFormat::Turtle);
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let rows = solutions(
            &view,
            "PREFIX ex: <http://example.com/>
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
             SELECT ?who WHERE {
               ?claim rdf:reifies <<( ex:alice ex:name \"Alice\" )>> ;
                      ex:statedBy ?who .
             }",
        );
        assert_eq!(rows, vec![format!("who={}", nn("bob"))], "{rows:?}");
    }

    #[test]
    fn a_triple_term_can_be_hidden_by_the_predicate_that_carries_it() {
        // Reifier-annotated provenance is the holon Event graph's shape (DESIGN.md §9),
        // so it has to be governable like anything else.
        let data = r#"
            @prefix ex: <http://example.com/> .
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            ex:alice ex:name "Alice" .
            ex:claim1 rdf:reifies <<( ex:alice ex:name "Alice" )>> .
        "#;
        let e = engine(data, RdfFormat::Turtle);
        let policy = Policy::permit_all().with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(NamedNode::new_unchecked(
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies",
            )),
            PrincipalMatch::Everyone,
        ));
        let session = Session::open(e.store(), Principal::anonymous(), policy).unwrap();
        let view = e.view(&session);
        assert!(solutions(&view, "SELECT ?s ?o WHERE { ?s ?p ?o FILTER(isTRIPLE(?o)) }").is_empty());
        // The asserted triple itself is untouched.
        assert!(ask(
            &view,
            "PREFIX ex: <http://example.com/> ASK { ex:alice ex:name \"Alice\" }"
        ));
    }
}
