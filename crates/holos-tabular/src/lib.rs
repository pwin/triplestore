//! Loading tabular data into named graphs with a SPARQL `CONSTRUCT` mapping.
//!
//! Most RDF starts life as a spreadsheet. The established way to get it from one to the
//! other is [TARQL](https://tarql.github.io/): write a `CONSTRUCT` where the column headers
//! are variables, and each row produces the triples the template describes.
//!
//! ```sparql
//! PREFIX ex: <http://example.org/>
//! CONSTRUCT {
//!   ?person a ex:Person ;
//!           ex:name  ?name ;
//!           ex:email ?email ;
//!           ex:row   ?ROWNUM .
//! }
//! WHERE {
//!   BIND(IRI(CONCAT("http://example.org/person/", ?id)) AS ?person)
//! }
//! ```
//!
//! # The approach, and where it came from
//!
//! [`semanticarts/oxi-gen`](https://github.com/semanticarts/oxi-gen) does this well and on
//! the same Oxigraph stack. Its code is **not** used here: oxi-gen is Apache-2.0 only,
//! this workspace is MIT **or** Apache-2.0, and taking Apache-only code would force the
//! combined work to Apache-only. The approach is the good idea, and the approach is not
//! what a licence covers — so it is implemented natively and credited here. The same
//! reasoning `DESIGN.md` §12 applies to Tentris.
//!
//! # How it works
//!
//! Rows are bound as a SPARQL `VALUES` block injected into the mapping's `WHERE` clause,
//! and the whole thing is evaluated **in batches** rather than once per row. That choice
//! does the work:
//!
//! * It uses nothing but public API — the mapping is ordinary SPARQL and the binding is an
//!   ordinary `VALUES`, so anything the evaluator can do, a mapping can do.
//! * A batch amortises parsing and planning across thousands of rows.
//! * An **empty cell becomes `UNDEF`**, not an empty string. That is TARQL's semantics and
//!   it is the one people rely on: `OPTIONAL`-shaped mappings work, and a blank column does
//!   not mint a triple with `""` in it.
//!
//! # What this buys over loading RDF directly
//!
//! The output goes into a **named graph**, through the ordinary write path — so the
//! [policy](holos_security) applies to every triple a mapping produces, and if the target
//! graph is a holon's scene, the boundary validates the mapping's output before it lands.
//! A spreadsheet cannot get bad data into a governed graph by coming in through a side
//! door, because there is no side door.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod frame;
pub mod mapping;
pub mod source;

pub use frame::Frame;
pub use mapping::Mapping;
pub use source::{CsvOptions, RowSource};

use holos_engine::Engine;
use holos_security::Session;
use oxrdf::{GraphName, NamedNode};

/// What a load did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoadReport {
    /// Rows read from the source.
    pub rows: u64,
    /// Triples the mapping produced, before deduplication.
    pub produced: u64,
    /// Quads that were not already present and now are.
    pub inserted: u64,
    /// Rows the mapping produced nothing from.
    ///
    /// Not an error — a mapping may legitimately skip rows with a `FILTER` — but a load
    /// where this equals `rows` means the mapping matched nothing, which almost always
    /// means a column name does not match a variable name.
    pub empty_rows: u64,
}

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum TabularError {
    /// The mapping was not a valid `CONSTRUCT`.
    #[error("{0}")]
    Mapping(String),
    /// The source could not be read.
    #[error("reading the source: {0}")]
    Source(String),
    /// Evaluating the mapping failed.
    #[error(transparent)]
    Engine(#[from] holos_engine::EngineError),
    /// Writing the result failed.
    #[error(transparent)]
    Storage(#[from] holos_store::StorageError),
}

/// How to run a load.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Rows evaluated per batch.
    ///
    /// Larger amortises planning further and costs more memory for the `VALUES` block.
    /// A thousand is comfortably past the point where planning stops mattering.
    pub batch: usize,
    /// Stop at the first row whose mapping fails, rather than continuing.
    ///
    /// Off by default: one malformed row in a million should not discard the other
    /// 999,999, and [`LoadReport`] records what happened either way.
    pub strict: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            batch: 1000,
            strict: false,
        }
    }
}

/// Loads rows into a named graph through a mapping.
///
/// Every triple goes through [`Engine::insert`], so write policy applies to each one and a
/// holon boundary on the target graph sees the mapping's output before it lands.
///
/// # Errors
///
/// Fails if the mapping cannot be evaluated, if the source cannot be read, or — under
/// [`LoadOptions::strict`] — on the first row that produces nothing.
pub fn load(
    engine: &mut Engine,
    session: &mut Session,
    source: &mut dyn RowSource,
    mapping: &Mapping,
    graph: Option<&NamedNode>,
    options: &LoadOptions,
) -> Result<LoadReport, TabularError> {
    let target = graph.map_or(GraphName::DefaultGraph, |g| GraphName::NamedNode(g.clone()));
    let mut report = LoadReport::default();
    let mut row_number: u64 = 0;

    loop {
        let batch = source
            .next_batch(options.batch, row_number)
            .map_err(TabularError::Source)?;
        if batch.is_empty() {
            break;
        }
        let batch_rows = batch.len() as u64;
        row_number += batch_rows;
        report.rows += batch_rows;

        // Evaluating needs `&self` and inserting needs `&mut self`, so the triples are
        // collected before anything is written. That is also what makes a row's output
        // atomic with respect to the mapping: the whole batch is computed against one
        // consistent view.
        let triples = mapping.apply(engine, session, &batch)?;
        report.produced += triples.len() as u64;
        if triples.is_empty() {
            report.empty_rows += batch_rows;
            if options.strict {
                return Err(TabularError::Mapping(format!(
                    "the mapping produced nothing for rows {}..{row_number} — usually a \
                     column name that does not match a variable name",
                    row_number - batch_rows
                )));
            }
        }

        for triple in triples {
            let quad = oxrdf::Quad {
                subject: triple.subject,
                predicate: triple.predicate,
                object: triple.object,
                graph_name: target.clone(),
            };
            if engine.insert(session, quad.as_ref())? {
                report.inserted += 1;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_security::Policy;

    const CSV: &str = "id,name,email\n1,Alice,alice@example.org\n2,Bob,\n";

    const MAPPING: &str = r#"
        PREFIX ex: <http://example.org/>
        CONSTRUCT {
          ?person a ex:Person ;
                  ex:name ?name ;
                  ex:email ?email ;
                  ex:row ?ROWNUM .
        }
        WHERE {
          BIND(IRI(CONCAT("http://example.org/person/", ?id)) AS ?person)
        }
    "#;

    fn run(csv: &str, mapping: &str) -> (Engine, LoadReport) {
        let mut engine = Engine::new();
        let mut session = Session::open(
            engine.store(),
            holos_security::Principal::anonymous(),
            Policy::permit_all(),
        )
        .expect("session");
        let map = Mapping::parse(mapping).expect("mapping");
        let mut source =
            source::Csv::from_reader(std::io::Cursor::new(csv.to_owned()), &CsvOptions::default())
                .expect("csv");
        let graph = NamedNode::new_unchecked("http://example.org/imported");
        let report = load(
            &mut engine,
            &mut session,
            &mut source,
            &map,
            Some(&graph),
            &LoadOptions::default(),
        )
        .expect("load");
        (engine, report)
    }

    fn count(engine: &Engine, sparql: &str) -> usize {
        let session = Session::unrestricted(engine.store()).expect("session");
        let view = engine.view(&session);
        let n = match holos_engine::Engine::query(&view, sparql, None).expect("query") {
            spareval::QueryResults::Solutions(iter) => iter.filter(Result::is_ok).count(),
            _ => 0,
        };
        n
    }

    #[test]
    fn rows_become_triples_in_the_named_graph() {
        let (engine, report) = run(CSV, MAPPING);
        assert_eq!(report.rows, 2);
        assert!(report.inserted > 0);
        assert_eq!(
            count(
                &engine,
                "SELECT ?s WHERE { GRAPH <http://example.org/imported> \
                 { ?s a <http://example.org/Person> } }"
            ),
            2
        );
    }

    #[test]
    fn an_empty_cell_produces_no_triple() {
        // TARQL's semantics, and the one people depend on: Bob has no email, so no
        // `ex:email ""` triple is minted for him.
        let (engine, _) = run(CSV, MAPPING);
        assert_eq!(
            count(
                &engine,
                "SELECT ?s WHERE { GRAPH <http://example.org/imported> \
                 { ?s <http://example.org/email> ?e } }"
            ),
            1,
            "only Alice has an email"
        );
    }

    #[test]
    fn rownum_is_available_and_one_based() {
        let (engine, _) = run(CSV, MAPPING);
        assert_eq!(
            count(
                &engine,
                "SELECT ?s WHERE { GRAPH <http://example.org/imported> \
                 { ?s <http://example.org/row> 1 } }"
            ),
            1,
            "the first data row is ROWNUM 1"
        );
    }

    #[test]
    fn nothing_lands_in_the_default_graph() {
        let (engine, _) = run(CSV, MAPPING);
        assert_eq!(count(&engine, "SELECT * WHERE { ?s ?p ?o }"), 0);
    }

    #[test]
    fn a_filter_can_skip_rows() {
        let mapping = r#"
            PREFIX ex: <http://example.org/>
            CONSTRUCT { ?person ex:name ?name }
            WHERE {
              BIND(IRI(CONCAT("http://example.org/p/", ?id)) AS ?person)
              FILTER(?name != "Bob")
            }
        "#;
        let (engine, report) = run(CSV, mapping);
        assert_eq!(report.rows, 2);
        assert_eq!(
            count(
                &engine,
                "SELECT ?s WHERE { GRAPH <http://example.org/imported> { ?s ?p ?o } }"
            ),
            1,
            "Bob was filtered out"
        );
    }

    #[test]
    fn write_policy_applies_to_what_a_mapping_produces() {
        // The point of going through the ordinary write path: a mapping is not a way
        // around policy.
        use holos_security::{Modes, PrincipalMatch, Rule, Scope};
        let mut engine = Engine::new();
        let policy = Policy::permit_all().with_rule(Rule::deny(
            Modes::WRITE,
            Scope::Predicate(NamedNode::new_unchecked("http://example.org/email")),
            PrincipalMatch::Everyone,
        ));
        let mut session = Session::open(
            engine.store(),
            holos_security::Principal::anonymous(),
            policy,
        )
        .expect("session");
        let map = Mapping::parse(MAPPING).expect("mapping");
        let mut source =
            source::Csv::from_reader(std::io::Cursor::new(CSV.to_owned()), &CsvOptions::default())
                .expect("csv");
        let graph = NamedNode::new_unchecked("http://example.org/imported");
        let outcome = load(
            &mut engine,
            &mut session,
            &mut source,
            &map,
            Some(&graph),
            &LoadOptions::default(),
        );
        assert!(
            matches!(
                outcome,
                Err(TabularError::Engine(
                    holos_engine::EngineError::AccessDenied
                ))
            ),
            "a denied predicate must stop the load, got {outcome:?}"
        );
    }

    #[test]
    fn a_mapping_that_matches_nothing_is_reported() {
        // The commonest mistake is a column name that does not match the variable, and it
        // silently produces an empty graph unless somebody says so.
        let mapping = r#"
            PREFIX ex: <http://example.org/>
            CONSTRUCT { ?s ex:name ?nmae }
            WHERE { BIND(IRI(CONCAT("http://example.org/p/", ?id)) AS ?s) }
        "#;
        let (_, report) = run(CSV, mapping);
        assert_eq!(report.produced, 0);
        assert_eq!(report.empty_rows, report.rows, "every row produced nothing");
    }
}
