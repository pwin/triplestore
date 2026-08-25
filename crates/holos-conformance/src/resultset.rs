//! Reading expected results encoded as RDF.
//!
//! The SPARQL 1.0 suite predates the SPARQL Results formats, so its expected answers are
//! serialised in the DAWG `rs:` vocabulary as RDF/XML rather than as `.srx` or `.srj`:
//!
//! ```text
//! [] a rs:ResultSet ;
//!    rs:resultVariable "name" ;
//!    rs:solution [ rs:index 1 ;
//!                  rs:binding [ rs:variable "name" ; rs:value "Alice" ] ] .
//! ```
//!
//! Not reading this was costing **152 of the 283 SPARQL 1.0 tests** — reported honestly as
//! skips, but skips that said nothing about the engine because the harness, not the store,
//! was the thing that could not cope. This module removes that excuse.
//!
//! # Two details that matter
//!
//! **`rs:index` is optional and only sometimes meaningful.** It carries the intended row
//! order for `ORDER BY` tests. Where present it is used to sort; where absent the solutions
//! are a bag, exactly as the unordered comparison already assumes.
//!
//! **An absent binding is not the same as a bound empty string.** A solution simply omits
//! the `rs:binding` for a variable it does not bind, which is how `OPTIONAL` results are
//! represented. Building the row from the bindings present — rather than from the variable
//! list — is what keeps unbound unbound.

use crate::manifest;
use anyhow::{anyhow, Result};
use oxrdf::vocab::rdf;
use oxrdf::{Dataset, NamedNode, NamedOrBlankNodeRef, Term, TermRef, Variable};
use spareval::QuerySolution;
use std::path::Path;
use std::sync::Arc;

const RS: &str = "http://www.w3.org/2001/sw/DataAccess/tests/result-set#";

fn rs(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{RS}{local}"))
}

/// Whether a file is likely to hold an RDF-encoded result set.
///
/// Extension only. The manifest says which files are results, so this decides *how* to
/// read one, not *whether* it is one.
#[must_use]
pub fn is_rdf_encoded(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rdf" | "ttl" | "n3" | "nt" | "xml")
    )
}

/// What an RDF-encoded result file holds.
#[derive(Debug)]
pub enum Expected {
    /// An `ASK` answer, from `rs:boolean`.
    Boolean(bool),
    /// A `SELECT` answer, in `rs:index` order where the file gives one.
    Solutions(Vec<QuerySolution>),
}

/// Reads an expected result set encoded in the DAWG `rs:` vocabulary.
///
/// # Errors
///
/// Fails when the file does not parse as RDF, or parses but holds no `rs:ResultSet`.
pub fn read(path: &Path) -> Result<Expected> {
    let base = manifest::path_to_file_url(path);
    let dataset = manifest::parse_dataset(path, &base)?;
    from_dataset(&dataset)
}

/// Extracts a result set from an already-parsed graph.
///
/// # Errors
///
/// Fails when there is no `rs:ResultSet` node, or a solution is malformed.
pub fn from_dataset(dataset: &Dataset) -> Result<Expected> {
    let result_set = rs("ResultSet");
    let node = dataset
        .quads_for_predicate(rdf::TYPE)
        .find(|q| q.object == TermRef::NamedNode(result_set.as_ref()))
        .map(|q| q.subject)
        .ok_or_else(|| anyhow!("no rs:ResultSet in the file"))?;

    // An ASK result carries rs:boolean and no solutions.
    if let Some(value) = object_of(dataset, node, &rs("boolean")) {
        if let TermRef::Literal(literal) = value {
            return Ok(Expected::Boolean(literal.value() == "true"));
        }
    }

    // The declared variables. Order is not significant here — the comparison is by name —
    // but the list has to be complete, because a QuerySolution pairs values to it.
    let mut variables: Vec<Variable> = dataset
        .quads_for_subject(node)
        .filter(|q| q.predicate == rs("resultVariable").as_ref())
        .filter_map(|q| match q.object {
            TermRef::Literal(l) => Variable::new(l.value()).ok(),
            _ => None,
        })
        .collect();
    variables.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    variables.dedup();
    let variables: Arc<[Variable]> = Arc::from(variables);

    let mut rows: Vec<(Option<i64>, Vec<Option<Term>>)> = Vec::new();

    for quad in dataset.quads_for_subject(node) {
        if quad.predicate != rs("solution").as_ref() {
            continue;
        }
        let solution = match quad.object {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::NamedNode(n),
            TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
            _ => continue,
        };

        let index = object_of(dataset, solution, &rs("index")).and_then(|t| match t {
            TermRef::Literal(l) => l.value().parse::<i64>().ok(),
            _ => None,
        });

        let mut values: Vec<Option<Term>> = vec![None; variables.len()];
        for binding in dataset.quads_for_subject(solution) {
            if binding.predicate != rs("binding").as_ref() {
                continue;
            }
            let node = match binding.object {
                TermRef::NamedNode(n) => NamedOrBlankNodeRef::NamedNode(n),
                TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
                _ => continue,
            };
            let (Some(name), Some(value)) = (
                object_of(dataset, node, &rs("variable")),
                object_of(dataset, node, &rs("value")),
            ) else {
                continue;
            };
            let TermRef::Literal(name) = name else {
                continue;
            };
            // A variable named by a binding but never declared would leave the row wider
            // than the header; the suite does not do this, and silently dropping it is
            // better than producing a solution that cannot be compared.
            if let Some(i) = variables.iter().position(|v| v.as_str() == name.value()) {
                values[i] = Some(value.into_owned());
            }
        }
        rows.push((index, values));
    }

    // Sort by rs:index where every row has one. A partial ordering is not an ordering, and
    // guessing at one would make an ORDER BY test pass for the wrong reason.
    if rows.iter().all(|(i, _)| i.is_some()) {
        rows.sort_by_key(|(i, _)| i.unwrap_or(0));
    }

    Ok(Expected::Solutions(
        rows.into_iter()
            .map(|(_, values)| QuerySolution::from((Arc::clone(&variables), values)))
            .collect(),
    ))
}

fn object_of<'a>(
    dataset: &'a Dataset,
    subject: NamedOrBlankNodeRef<'_>,
    predicate: &NamedNode,
) -> Option<TermRef<'a>> {
    dataset
        .quads_for_subject(subject)
        .find(|q| q.predicate == predicate.as_ref())
        .map(|q| q.object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{BlankNode, Literal, Quad};

    fn dataset_from_turtle(turtle: &str) -> Dataset {
        use oxrdfio::{RdfFormat, RdfParser};
        let mut dataset = Dataset::new();
        for quad in RdfParser::from_format(RdfFormat::Turtle).for_slice(turtle.as_bytes()) {
            dataset.insert(quad.expect("parse").as_ref());
        }
        dataset
    }

    #[test]
    fn reads_a_boolean_result() {
        let d = dataset_from_turtle(
            r#"@prefix rs: <http://www.w3.org/2001/sw/DataAccess/tests/result-set#> .
               [] a rs:ResultSet ; rs:boolean true ."#,
        );
        match from_dataset(&d).expect("read") {
            Expected::Boolean(b) => assert!(b),
            other => panic!("expected a boolean, got {other:?}"),
        }
    }

    #[test]
    fn reads_solutions_in_index_order() {
        let d = dataset_from_turtle(
            r#"@prefix rs: <http://www.w3.org/2001/sw/DataAccess/tests/result-set#> .
               [] a rs:ResultSet ;
                  rs:resultVariable "name" ;
                  rs:solution [ rs:index 2 ; rs:binding [ rs:variable "name" ; rs:value "Bob" ] ] ;
                  rs:solution [ rs:index 1 ; rs:binding [ rs:variable "name" ; rs:value "Alice" ] ] ."#,
        );
        let Expected::Solutions(rows) = from_dataset(&d).expect("read") else {
            panic!("expected solutions");
        };
        assert_eq!(rows.len(), 2);
        // rs:index is what makes an ORDER BY test checkable, so it must be honoured.
        assert_eq!(
            rows[0].get("name").map(ToString::to_string),
            Some("\"Alice\"".to_owned())
        );
        assert_eq!(
            rows[1].get("name").map(ToString::to_string),
            Some("\"Bob\"".to_owned())
        );
    }

    #[test]
    fn an_omitted_binding_stays_unbound() {
        // How OPTIONAL results are represented: the variable is declared, the binding is
        // simply absent. Filling it with anything would turn an unbound into a bound.
        let d = dataset_from_turtle(
            r#"@prefix rs: <http://www.w3.org/2001/sw/DataAccess/tests/result-set#> .
               [] a rs:ResultSet ;
                  rs:resultVariable "a" ; rs:resultVariable "b" ;
                  rs:solution [ rs:binding [ rs:variable "a" ; rs:value "x" ] ] ."#,
        );
        let Expected::Solutions(rows) = from_dataset(&d).expect("read") else {
            panic!("expected solutions");
        };
        assert_eq!(rows.len(), 1);
        assert!(rows[0].get("a").is_some());
        assert!(rows[0].get("b").is_none(), "b must stay unbound");
    }

    #[test]
    fn typed_and_plain_literals_survive() {
        let d = dataset_from_turtle(
            r#"@prefix rs: <http://www.w3.org/2001/sw/DataAccess/tests/result-set#> .
               @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
               [] a rs:ResultSet ;
                  rs:resultVariable "n" ;
                  rs:solution [ rs:binding [ rs:variable "n" ;
                                rs:value "42"^^xsd:integer ] ] ."#,
        );
        let Expected::Solutions(rows) = from_dataset(&d).expect("read") else {
            panic!("expected solutions");
        };
        let term = rows[0].get("n").expect("bound");
        assert!(
            term.to_string().contains("XMLSchema#integer"),
            "the datatype must survive: {term}"
        );
    }

    #[test]
    fn iri_values_survive() {
        let d = dataset_from_turtle(
            r#"@prefix rs: <http://www.w3.org/2001/sw/DataAccess/tests/result-set#> .
               [] a rs:ResultSet ;
                  rs:resultVariable "s" ;
                  rs:solution [ rs:binding [ rs:variable "s" ;
                                rs:value <http://example.com/a> ] ] ."#,
        );
        let Expected::Solutions(rows) = from_dataset(&d).expect("read") else {
            panic!("expected solutions");
        };
        assert_eq!(
            rows[0].get("s").map(ToString::to_string),
            Some("<http://example.com/a>".to_owned())
        );
    }

    #[test]
    fn a_file_without_a_result_set_is_an_error() {
        let d = Dataset::from_iter([Quad::new(
            BlankNode::default(),
            NamedNode::new_unchecked("http://example.com/p"),
            Literal::new_simple_literal("x"),
            oxrdf::GraphName::DefaultGraph,
        )]);
        assert!(from_dataset(&d).is_err());
    }

    #[test]
    fn rows_without_an_index_keep_their_order_unsorted() {
        // A partial ordering is not an ordering. If some rows lack rs:index the set is a
        // bag, and the unordered comparison is what applies.
        let d = dataset_from_turtle(
            r#"@prefix rs: <http://www.w3.org/2001/sw/DataAccess/tests/result-set#> .
               [] a rs:ResultSet ;
                  rs:resultVariable "n" ;
                  rs:solution [ rs:binding [ rs:variable "n" ; rs:value "one" ] ] ;
                  rs:solution [ rs:binding [ rs:variable "n" ; rs:value "two" ] ] ."#,
        );
        let Expected::Solutions(rows) = from_dataset(&d).expect("read") else {
            panic!("expected solutions");
        };
        assert_eq!(rows.len(), 2);
    }
}
