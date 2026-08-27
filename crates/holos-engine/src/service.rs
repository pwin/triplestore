//! SPARQL 1.1 Federated Query — the `SERVICE` clause.
//!
//! # Two very different things share this name
//!
//! **Serving a `SERVICE` clause from local data.** The endpoint IRI is a name, and what it
//! names is a dataset this process already holds. No network, no trust decision. That is
//! what [`LocalServiceHandler`] does, and it is what the W3C federated-query tests need —
//! they specify each endpoint with `qt:serviceData`, giving an IRI and a local file.
//!
//! **Calling a remote endpoint over HTTP.** A different proposition entirely, because
//! `SERVICE <http://…>` in a query submitted by a user makes *the server* issue a request
//! to a host of the user's choosing. That is a server-side request forgery primitive: it
//! reaches cloud instance metadata at `169.254.169.254`, internal admin interfaces, and
//! anything else the server can route to but the user cannot. Remote `LOAD` is refused for
//! exactly this reason (see [`crate::update`]), and remote `SERVICE` cannot be enabled on
//! looser terms than `LOAD` was refused on.
//!
//! So this module implements the local case, and defines the seam an allow-listed HTTP
//! handler plugs into. See `ACCESS-CONTROL.md` for why the allow-list is a policy decision
//! rather than a configuration flag.
//!
//! # What federation does to the access-control guarantee
//!
//! §14's property is that the answer to a query equals the answer over *the sub-dataset the
//! principal may see*. Results arriving from another service have not passed through
//! `decide_quad` and are not part of that sub-dataset, so the property does not extend to
//! them — it holds for the local contribution only. A deployment that enables federation is
//! choosing to trust the endpoints it allow-lists, and should say so in its own threat
//! model rather than assuming the local guarantee covers them.

use oxiri::Iri;
use oxrdf::{Dataset, NamedNode};
use spareval::{DefaultServiceHandler, QueryEvaluationError, QueryResults, QuerySolutionIter};
use spargebra::algebra::GraphPattern;
use std::collections::HashMap;
use std::sync::Arc;

/// Serves `SERVICE` clauses from datasets held in this process.
///
/// Each endpoint IRI maps to a dataset. A `SERVICE` naming an endpoint that was not
/// registered fails — deliberately, rather than returning no solutions, because an empty
/// answer from a misspelled endpoint is indistinguishable from an endpoint that genuinely
/// had nothing, and the two want different fixes.
#[derive(Debug, Default, Clone)]
pub struct LocalServiceHandler {
    endpoints: HashMap<NamedNode, Arc<Dataset>>,
}

impl LocalServiceHandler {
    /// A handler serving nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a dataset under an endpoint IRI.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: NamedNode, data: Dataset) -> Self {
        self.endpoints.insert(endpoint, Arc::new(data));
        self
    }

    /// Adds a dataset to an endpoint, merging with anything already registered for it.
    ///
    /// The test suite gives some endpoints more than one file, so registering twice has to
    /// accumulate rather than replace.
    pub fn add_endpoint(&mut self, endpoint: NamedNode, data: Dataset) {
        match self.endpoints.get(&endpoint) {
            None => {
                self.endpoints.insert(endpoint, Arc::new(data));
            }
            Some(existing) => {
                let mut merged = Dataset::from_iter(existing.iter().map(|q| q.into_owned()));
                for quad in data.iter() {
                    merged.insert(quad);
                }
                self.endpoints.insert(endpoint, Arc::new(merged));
            }
        }
    }

    /// Endpoints this handler knows.
    #[must_use]
    pub fn endpoints(&self) -> Vec<&NamedNode> {
        self.endpoints.keys().collect()
    }

    /// Whether anything is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }
}

impl DefaultServiceHandler for LocalServiceHandler {
    type Error = QueryEvaluationError;

    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, Self::Error> {
        let Some(dataset) = self.endpoints.get(service_name) else {
            return Err(QueryEvaluationError::UnsupportedService(
                service_name.clone(),
            ));
        };
        // Cloning self into the nested evaluator is what makes a SERVICE inside a SERVICE
        // work. Without it the inner clause meets an evaluator with no handler registered
        // and fails — which is exactly what the `service3` conformance test does.
        // Recursion terminates because the nesting is bounded by the query's own text.
        self.evaluate_against(dataset, pattern, base_iri)
    }
}

impl LocalServiceHandler {
    /// Evaluates a graph pattern against a dataset and materialises the solutions.
    ///
    /// The trait wants a `'static` iterator, and evaluating borrows the dataset, so the
    /// rows are collected. That is right for the local case — these datasets are small —
    /// and it is also what an HTTP handler would have to do, since a response body is
    /// consumed before it can be handed on.
    fn evaluate_against(
        &self,
        dataset: &Dataset,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, QueryEvaluationError> {
        // The handler is given a pattern, not a query. Wrapping it in a SELECT of what it
        // binds is what makes it executable, and projecting the pattern's own variables is
        // what keeps the join with the outer query correct.
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: GraphPattern::Project {
                inner: Box::new(pattern.clone()),
                variables: pattern_variables(pattern),
            },
            base_iri: base_iri.cloned(),
        };

        let evaluator = crate::Engine::evaluator().with_default_service_handler(self.clone());
        let results = evaluator.prepare(&query).execute(dataset)?;
        let QueryResults::Solutions(solutions) = results else {
            return Err(QueryEvaluationError::Service(
                "a SERVICE pattern must evaluate to solutions".into(),
            ));
        };

        let variables: Arc<[oxrdf::Variable]> = Arc::from(solutions.variables().to_vec());
        let rows: Vec<_> = solutions.collect::<Result<Vec<_>, _>>()?;
        Ok(QuerySolutionIter::new(variables, rows.into_iter().map(Ok)))
    }
}

/// Every variable a pattern mentions, in a stable order.
///
/// `GraphPattern` has no public accessor for this, so it is walked. Order has to be
/// deterministic or the same query would project its columns differently between runs.
fn pattern_variables(pattern: &GraphPattern) -> Vec<oxrdf::Variable> {
    let mut out: Vec<oxrdf::Variable> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    collect_variables(pattern, &mut out, &mut seen);
    out
}

fn collect_variables(
    pattern: &GraphPattern,
    out: &mut Vec<oxrdf::Variable>,
    seen: &mut std::collections::BTreeSet<String>,
) {
    use spargebra::term::{NamedNodePattern, TermPattern};

    let push = |v: &oxrdf::Variable,
                out: &mut Vec<oxrdf::Variable>,
                seen: &mut std::collections::BTreeSet<String>| {
        if seen.insert(v.as_str().to_owned()) {
            out.push(v.clone());
        }
    };
    let from_term = |t: &TermPattern, out: &mut Vec<_>, seen: &mut _| {
        if let TermPattern::Variable(v) = t {
            push(v, out, seen);
        }
    };

    match pattern {
        GraphPattern::Bgp { patterns } => {
            for p in patterns {
                from_term(&p.subject, out, seen);
                from_term(&p.object, out, seen);
                if let NamedNodePattern::Variable(v) = &p.predicate {
                    push(v, out, seen);
                }
            }
        }
        GraphPattern::Path {
            subject, object, ..
        } => {
            from_term(subject, out, seen);
            from_term(object, out, seen);
        }
        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right }
        | GraphPattern::LeftJoin { left, right, .. } => {
            collect_variables(left, out, seen);
            collect_variables(right, out, seen);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. } => collect_variables(inner, out, seen),
        GraphPattern::Graph { name, inner } => {
            if let NamedNodePattern::Variable(v) = name {
                push(v, out, seen);
            }
            collect_variables(inner, out, seen);
        }
        GraphPattern::Extend {
            inner, variable, ..
        } => {
            collect_variables(inner, out, seen);
            push(variable, out, seen);
        }
        GraphPattern::Project { variables, .. } => {
            for v in variables {
                push(v, out, seen);
            }
        }
        GraphPattern::Group {
            variables,
            aggregates,
            ..
        } => {
            for v in variables {
                push(v, out, seen);
            }
            for (v, _) in aggregates {
                push(v, out, seen);
            }
        }
        GraphPattern::Values { variables, .. } => {
            for v in variables {
                push(v, out, seen);
            }
        }
        GraphPattern::Service { inner, name, .. } => {
            if let NamedNodePattern::Variable(v) = name {
                push(v, out, seen);
            }
            collect_variables(inner, out, seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{GraphName, Literal, Quad};
    use spargebra::SparqlParser;

    fn ex(name: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.org/{name}"))
    }

    fn dataset(pairs: &[(&str, &str)]) -> Dataset {
        Dataset::from_iter(pairs.iter().map(|(s, o)| {
            Quad::new(
                ex(s),
                ex("interest"),
                Literal::new_simple_literal(*o),
                GraphName::DefaultGraph,
            )
        }))
    }

    fn run(handler: LocalServiceHandler, sparql: &str) -> Vec<String> {
        let query = SparqlParser::new().parse_query(sparql).expect("parse");
        let evaluator = crate::Engine::evaluator().with_default_service_handler(handler);
        let local = Dataset::new();
        let rows = match evaluator.prepare(&query).execute(&local).expect("execute") {
            QueryResults::Solutions(iter) => {
                let mut out: Vec<String> = iter
                    .map(|s| {
                        s.expect("solution")
                            .iter()
                            .map(|(v, t)| format!("{v}={t}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect();
                out.sort();
                out
            }
            _ => panic!("expected solutions"),
        };
        rows
    }

    #[test]
    fn a_service_clause_reads_its_endpoint() {
        let handler = LocalServiceHandler::new()
            .with_endpoint(ex("sparql"), dataset(&[("a", "SPARQL"), ("b", "RDF")]));
        let rows = run(
            handler,
            "SELECT ?s ?i WHERE { SERVICE <http://example.org/sparql> \
             { ?s <http://example.org/interest> ?i } }",
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("SPARQL")));
    }

    #[test]
    fn two_endpoints_are_kept_apart() {
        // The point of federation: each name resolves to its own data, and a query can
        // join across them.
        let handler = LocalServiceHandler::new()
            .with_endpoint(ex("one"), dataset(&[("a", "from-one")]))
            .with_endpoint(ex("two"), dataset(&[("a", "from-two")]));

        let one = run(
            handler.clone(),
            "SELECT ?i WHERE { SERVICE <http://example.org/one> \
             { ?s <http://example.org/interest> ?i } }",
        );
        let two = run(
            handler,
            "SELECT ?i WHERE { SERVICE <http://example.org/two> \
             { ?s <http://example.org/interest> ?i } }",
        );
        assert!(one[0].contains("from-one"), "{one:?}");
        assert!(two[0].contains("from-two"), "{two:?}");
    }

    #[test]
    fn an_unregistered_endpoint_is_an_error_not_an_empty_answer() {
        // Returning no solutions would make a misspelled endpoint indistinguishable from
        // one that genuinely had nothing to say, and the two need different fixes.
        let handler = LocalServiceHandler::new();
        let query = SparqlParser::new()
            .parse_query("SELECT ?s WHERE { SERVICE <http://example.org/absent> { ?s ?p ?o } }")
            .expect("parse");
        let evaluator = crate::Engine::evaluator().with_default_service_handler(handler);
        let local = Dataset::new();
        let outcome = evaluator
            .prepare(&query)
            .execute(&local)
            .and_then(|r| match r {
                QueryResults::Solutions(iter) => {
                    iter.collect::<Result<Vec<_>, _>>().map(|v| v.len())
                }
                _ => Ok(0),
            });
        assert!(
            outcome.is_err(),
            "an unknown endpoint must not answer quietly"
        );
    }

    #[test]
    fn service_silent_swallows_the_failure() {
        // SPARQL says SILENT turns a service failure into no bindings rather than an error.
        let handler = LocalServiceHandler::new();
        let rows = run(
            handler,
            "SELECT ?s WHERE { OPTIONAL { SERVICE SILENT <http://example.org/absent> \
             { ?s ?p ?o } } }",
        );
        assert_eq!(
            rows.len(),
            1,
            "SILENT should yield one empty solution: {rows:?}"
        );
    }

    #[test]
    fn adding_to_an_endpoint_accumulates() {
        // The suite gives some endpoints more than one file.
        let mut handler = LocalServiceHandler::new();
        handler.add_endpoint(ex("merged"), dataset(&[("a", "first")]));
        handler.add_endpoint(ex("merged"), dataset(&[("b", "second")]));
        let rows = run(
            handler,
            "SELECT ?i WHERE { SERVICE <http://example.org/merged> \
             { ?s <http://example.org/interest> ?i } }",
        );
        assert_eq!(rows.len(), 2, "both files must be present: {rows:?}");
    }

    #[test]
    fn a_service_result_joins_with_local_data() {
        let handler = LocalServiceHandler::new()
            .with_endpoint(ex("remote"), dataset(&[("a", "shared"), ("b", "other")]));
        let query = SparqlParser::new()
            .parse_query(
                "SELECT ?s WHERE { ?s <http://example.org/local> true . \
                 SERVICE <http://example.org/remote> { ?s <http://example.org/interest> ?i } }",
            )
            .expect("parse");
        let local = Dataset::from_iter([Quad::new(
            ex("a"),
            ex("local"),
            Literal::from(true),
            GraphName::DefaultGraph,
        )]);
        let evaluator = crate::Engine::evaluator().with_default_service_handler(handler);
        let n = match evaluator.prepare(&query).execute(&local).expect("execute") {
            QueryResults::Solutions(iter) => iter.filter(Result::is_ok).count(),
            _ => 0,
        };
        assert_eq!(n, 1, "only ex:a is in both the local data and the service");
    }
}
