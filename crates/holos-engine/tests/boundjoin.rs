//! The bound join, checked by looking at what the endpoint was actually sent.
//!
//! Two things have to hold, and they pull in opposite directions. The clause must *arrive*
//! carrying the keys — otherwise the optimisation is not happening and a capped endpoint still
//! answers the wrong question. And the answer must not change — otherwise the optimisation is a
//! bug, and the way a bound join goes wrong is by sending too few keys, which silently drops
//! rows rather than failing.
//!
//! So every test here uses a handler that both answers the query and records the pattern it was
//! given, and the recorded pattern is compared against what should have been pushed. The
//! handler serves from a local dataset, so "the answer" is checkable exactly.

use holos_engine::Engine;
use holos_security::Session;
use oxiri::Iri;
use oxrdf::{Dataset, GraphName, NamedNode, Quad};
use oxrdfio::RdfFormat;
use spareval::{DefaultServiceHandler, QueryEvaluationError, QueryResults, QuerySolutionIter};
use spargebra::SparqlParser;
use spargebra::algebra::GraphPattern;
use std::sync::{Arc, Mutex};

const EX: &str = "http://example.com/";
const REMOTE: &str = "http://remote.example/sparql";

/// Local data: five towns, three of which have a link to a remote resource.
fn local() -> String {
    let mut turtle = format!("@prefix ex: <{EX}> .\n");
    for (town, remote) in [
        ("hay", Some("r-hay")),
        ("sedbergh", Some("r-sedbergh")),
        ("wigtown", Some("r-wigtown")),
        ("nowhere", None),
        ("elsewhere", None),
    ] {
        turtle.push_str(&format!("ex:{town} a ex:Town ; ex:name \"{town}\" .\n"));
        if let Some(remote) = remote {
            turtle.push_str(&format!("ex:{town} ex:sameAs ex:{remote} .\n"));
        }
    }
    turtle
}

/// The endpoint's data: a population for two of the three linked resources, and a great many
/// other populations, so a query that fails to restrict itself pulls in far more than it wants.
fn remote() -> Dataset {
    let population = NamedNode::new_unchecked(format!("{EX}population"));
    let mut quads: Vec<Quad> = Vec::new();
    for (resource, people) in [("r-sedbergh", 2765), ("r-hay", 1500)] {
        quads.push(Quad::new(
            NamedNode::new_unchecked(format!("{EX}{resource}")),
            population.clone(),
            oxrdf::Literal::from(people),
            GraphName::DefaultGraph,
        ));
    }
    for i in 0..200 {
        quads.push(Quad::new(
            NamedNode::new_unchecked(format!("{EX}other{i}")),
            population.clone(),
            oxrdf::Literal::from(i),
            GraphName::DefaultGraph,
        ));
    }
    Dataset::from_iter(quads)
}

/// Answers from a local dataset, and keeps every pattern it was handed.
#[derive(Clone)]
struct Recording {
    data: Arc<Dataset>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Recording {
    fn new() -> Self {
        Self {
            data: Arc::new(remote()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The patterns handed over, as SPARQL, deduplicated.
    ///
    /// Deduplicated because `spareval` calls the handler once per solution arriving from the
    /// left: thirty local rows ask the same question thirty times, and what this is about is
    /// *which* question.
    fn patterns(&self) -> Vec<String> {
        let mut out = self.seen.lock().expect("lock").clone();
        out.sort();
        out.dedup();
        out
    }
}

impl DefaultServiceHandler for Recording {
    type Error = QueryEvaluationError;

    fn handle(
        &self,
        _service: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, Self::Error> {
        let variables = holos_engine::service::pattern_variables(pattern);
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: GraphPattern::Project {
                inner: Box::new(pattern.clone()),
                variables: variables.clone(),
            },
            base_iri: base_iri.cloned(),
        };
        self.seen.lock().expect("lock").push(query.to_string());

        let evaluated = Engine::evaluator()
            .prepare(&query)
            .execute(self.data.as_ref())?;
        let QueryResults::Solutions(solutions) = evaluated else {
            return Ok(QuerySolutionIter::new(
                Arc::from(variables),
                std::iter::empty(),
            ));
        };
        let names: Arc<[oxrdf::Variable]> = Arc::from(solutions.variables().to_vec());
        let rows: Vec<_> = solutions.collect();
        Ok(QuerySolutionIter::new(names, rows))
    }
}

fn engine() -> Engine {
    loaded(&local())
}

fn loaded(turtle: &str) -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(turtle.as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

/// Runs `sparql` through the handler path, returning the rows and the patterns the handler saw.
fn run(sparql: &str) -> (Vec<String>, Vec<String>) {
    run_on(&local(), sparql)
}

fn run_on(turtle: &str, sparql: &str) -> (Vec<String>, Vec<String>) {
    let engine = loaded(turtle);
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = SparqlParser::new().parse_query(sparql).expect("parse");
    let handler = Recording::new();
    let results = Engine::query_prepared_with_handler(&view, &query, handler.clone())
        .expect("query with a handler");

    let mut rows: Vec<String> = match results {
        QueryResults::Solutions(iter) => iter
            .map(|solution| {
                let solution = solution.expect("solution");
                let mut cells: Vec<String> = solution
                    .iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect();
                cells.sort();
                cells.join(" ")
            })
            .collect(),
        QueryResults::Boolean(value) => vec![value.to_string()],
        QueryResults::Graph(triples) => triples
            .map(|t| t.expect("triple").to_string())
            .collect::<Vec<_>>(),
    };
    rows.sort();
    (rows, handler.patterns())
}

const PREFIX: &str = "PREFIX ex: <http://example.com/>";

#[test]
fn the_keys_from_the_join_are_sent_with_the_question() {
    let (rows, sent) = run(&format!(
        "{PREFIX}
         SELECT ?name ?population WHERE {{
           ?town a ex:Town ; ex:name ?name ; ex:sameAs ?remote .
           SERVICE <{REMOTE}> {{ ?remote ex:population ?population }}
         }}"
    ));

    // One question, naming the three resources the local data linked to -- not the 203 the
    // endpoint holds.
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("VALUES"), "{}", sent[0]);
    for resource in ["r-hay", "r-sedbergh", "r-wigtown"] {
        assert!(
            sent[0].contains(resource),
            "{resource} missing from {}",
            sent[0]
        );
    }
    assert!(
        !sent[0].contains("other"),
        "the block should name only the local keys: {}",
        sent[0]
    );

    // And the answer is the two the endpoint had a population for.
    assert_eq!(
        rows,
        vec![
            "name=\"hay\" population=\"1500\"^^<http://www.w3.org/2001/XMLSchema#integer>"
                .to_owned(),
            "name=\"sedbergh\" population=\"2765\"^^<http://www.w3.org/2001/XMLSchema#integer>"
                .to_owned(),
        ]
    );
}

#[test]
fn a_service_inside_an_optional_is_bound_too() {
    // q106's shape. The OPTIONAL keeps the rows the endpoint cannot answer, and the keys still
    // have to reach it -- an unbound clause here is what left the column empty for every row.
    let (rows, sent) = run(&format!(
        "{PREFIX}
         SELECT ?name ?population WHERE {{
           ?town a ex:Town ; ex:name ?name ; ex:sameAs ?remote .
           OPTIONAL {{ SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }}
         }}"
    ));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("VALUES"), "{}", sent[0]);
    // Three rows, two of them with a population: the OPTIONAL kept wigtown.
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows.iter().filter(|r| r.contains("population")).count(), 2);
}

#[test]
fn a_clause_sharing_nothing_is_left_alone() {
    // No variable in common, so there are no keys to push and nothing to probe for. The clause
    // must go out as written rather than with an empty block in it.
    let (_rows, sent) = run(&format!(
        "{PREFIX}
         SELECT ?town ?who WHERE {{
           ?town a ex:Town .
           SERVICE <{REMOTE}> {{ ?who ex:population ?anything }}
         }}"
    ));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(!sent[0].contains("VALUES"), "{}", sent[0]);
}

#[test]
fn a_key_unbound_in_any_row_is_not_pushed() {
    // `?remote` is bound by an OPTIONAL here, so two of the five towns leave it unbound. Those
    // rows join against the whole remote pattern, so restricting it to the three bound keys
    // would lose rows -- the clause has to go out unrestricted.
    let (rows, sent) = run(&format!(
        "{PREFIX}
         SELECT ?name ?population WHERE {{
           ?town a ex:Town ; ex:name ?name .
           OPTIONAL {{ ?town ex:sameAs ?remote }}
           SERVICE <{REMOTE}> {{ ?remote ex:population ?population }}
         }}"
    ));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        !sent[0].contains("VALUES"),
        "an unbound key must not be pushed: {}",
        sent[0]
    );
    // The two towns with no link join against every remote population, which is the semantics
    // the unrestricted clause preserves and the reason not to push here.
    assert!(rows.len() > 2, "{} rows", rows.len());
}

#[test]
fn the_two_branches_of_a_union_do_not_lend_each_other_keys() {
    // `?remote` is bound in the other branch, not in the one the clause is in. Pushing those
    // keys would restrict a clause they have nothing to do with.
    let (_rows, sent) = run(&format!(
        "{PREFIX}
         SELECT ?remote ?population WHERE {{
           {{ ?town ex:sameAs ?remote }}
           UNION
           {{ SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }}
         }}"
    ));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(!sent[0].contains("VALUES"), "{}", sent[0]);
}

#[test]
fn pushing_keys_does_not_change_the_answer() {
    // The assertion that matters. Every query runs both ways -- the bound join is on the
    // handler path, and `query_prepared_with_services` does not apply it -- and the rows must
    // agree exactly, including multiplicity and unbound columns.
    for sparql in [
        format!(
            "{PREFIX} SELECT ?name ?population WHERE {{
               ?town a ex:Town ; ex:name ?name ; ex:sameAs ?remote .
               SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }}"
        ),
        format!(
            "{PREFIX} SELECT ?name ?population WHERE {{
               ?town a ex:Town ; ex:name ?name ; ex:sameAs ?remote .
               OPTIONAL {{ SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }} }}"
        ),
        format!(
            "{PREFIX} SELECT ?name WHERE {{
               ?town a ex:Town ; ex:name ?name ; ex:sameAs ?remote .
               SERVICE <{REMOTE}> {{ ?remote ex:population ?population }}
               FILTER( ?population > 2000 ) }}"
        ),
        format!(
            "{PREFIX} SELECT DISTINCT ?population WHERE {{
               ?town ex:sameAs ?remote .
               SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }} ORDER BY ?population"
        ),
        format!("{PREFIX} ASK {{ ?town ex:sameAs ?remote .
               SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }}"),
        format!(
            "{PREFIX} CONSTRUCT {{ ?town ex:knownPopulation ?population }} WHERE {{
               ?town ex:sameAs ?remote .
               SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }}"
        ),
    ] {
        let (bound, _) = run(&sparql);
        let unbound = without_the_bound_join(&sparql);
        assert_eq!(bound, unbound, "the bound join changed the answer to {sparql}");
    }
}

/// The same query with the keys left out, for comparison.
///
/// Reached by handing the handler a query that has already been rewritten to push nothing,
/// which is what `with_pushed_keys` does when every clause gets `None`. That exercises the real
/// evaluation path rather than a second implementation of it.
fn without_the_bound_join(sparql: &str) -> Vec<String> {
    use holos_engine::boundjoin;

    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = SparqlParser::new().parse_query(sparql).expect("parse");
    let pattern = boundjoin::pattern_of(&query);
    let nothing = vec![None; boundjoin::service_candidates(pattern).len()];
    let query = boundjoin::with_pattern(&query, boundjoin::with_pushed_keys(pattern, nothing));

    let handler = Recording::new();
    let evaluated = Engine::evaluator()
        .with_default_service_handler(handler)
        .prepare(&query)
        .execute(&view)
        .expect("execute");
    let mut rows: Vec<String> = match evaluated {
        QueryResults::Solutions(iter) => iter
            .map(|solution| {
                let solution = solution.expect("solution");
                let mut cells: Vec<String> = solution
                    .iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect();
                cells.sort();
                cells.join(" ")
            })
            .collect(),
        QueryResults::Boolean(value) => vec![value.to_string()],
        QueryResults::Graph(triples) => triples
            .map(|t| t.expect("triple").to_string())
            .collect::<Vec<_>>(),
    };
    rows.sort();
    rows
}

#[test]
fn a_blank_node_key_is_not_sent() {
    // A blank-node label is scoped to the document it was parsed from, so it names nothing at
    // the far end. Pushing it would send a key that cannot match, and `VALUES` cannot carry one
    // anyway.
    let (_rows, sent) = run_on(
        &format!("@prefix ex: <{EX}> .
ex:town ex:sameAs [ ex:kind \"anonymous\" ] .
"),
        &format!(
            "{PREFIX} SELECT ?population WHERE {{
               ?town ex:sameAs ?remote .
               SERVICE <{REMOTE}> {{ ?remote ex:population ?population }} }}"
        ),
    );
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        !sent[0].contains("VALUES"),
        "a blank node must not be pushed: {}",
        sent[0]
    );
}
