//! The `CONSTRUCT` mapping, and how a batch of rows is bound into it.
//!
//! # Why `VALUES` rather than per-row substitution
//!
//! `spareval` offers `substitute_variable`, which looks like the obvious mechanism and is
//! not. It rejects a variable the query does not project, so a mapping using a column only
//! inside its `WHERE` clause would fail — and it binds one row at a time, so a million-row
//! file would plan a million queries.
//!
//! Injecting a `VALUES` block into the `WHERE` clause has neither problem. It is ordinary
//! SPARQL, so any variable the mapping mentions can be bound; and a batch of rows is one
//! query, so planning is amortised.

use crate::TabularError;
use holos_engine::Engine;
use holos_security::Session;
use oxrdf::{Literal, Triple, Variable};
use spargebra::algebra::GraphPattern;
use spargebra::term::GroundTerm;
use spareval::QueryResults;

/// The variable carrying the current row number.
///
/// One-based, matching TARQL and oxi-gen, because a spreadsheet's first data row is row 1
/// to everyone who is looking at the spreadsheet.
pub const ROWNUM: &str = "ROWNUM";

/// A parsed `CONSTRUCT` mapping.
#[derive(Debug, Clone)]
pub struct Mapping {
    query: spargebra::Query,
}

impl Mapping {
    /// Parses a mapping.
    ///
    /// # Errors
    ///
    /// Fails if the text is not valid SPARQL, or is not a `CONSTRUCT` — a `SELECT` would
    /// produce a result set rather than triples, and silently accepting one would give an
    /// empty graph and no explanation.
    pub fn parse(text: &str) -> Result<Self, TabularError> {
        let query = spargebra::SparqlParser::new()
            .parse_query(text)
            .map_err(|e| TabularError::Mapping(format!("the mapping did not parse: {e}")))?;
        if !matches!(query, spargebra::Query::Construct { .. }) {
            return Err(TabularError::Mapping(
                "a mapping must be a CONSTRUCT query — that is what produces triples".into(),
            ));
        }
        Ok(Self { query })
    }

    /// Reads a mapping from a file.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read, or does not hold a valid `CONSTRUCT`.
    pub fn from_path(path: &std::path::Path) -> Result<Self, TabularError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| TabularError::Mapping(format!("reading {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    /// Every variable the mapping mentions, which is what a column has to match.
    ///
    /// Exposed so a caller can warn about a column the mapping never reads, or a variable
    /// no column supplies — the two commonest reasons a load produces nothing.
    #[must_use]
    pub fn variables(&self) -> Vec<String> {
        let mut out = std::collections::BTreeSet::new();
        if let spargebra::Query::Construct {
            template, pattern, ..
        } = &self.query
        {
            for t in template {
                for term in [&t.subject, &t.object] {
                    if let spargebra::term::TermPattern::Variable(v) = term {
                        out.insert(v.as_str().to_owned());
                    }
                }
                if let spargebra::term::NamedNodePattern::Variable(v) = &t.predicate {
                    out.insert(v.as_str().to_owned());
                }
            }
            collect_pattern_variables(pattern, &mut out);
        }
        out.into_iter().collect()
    }

    /// Evaluates the mapping over one batch of rows.
    ///
    /// # Errors
    ///
    /// Propagates evaluation failures.
    pub fn apply(
        &self,
        engine: &Engine,
        session: &Session,
        rows: &[crate::source::Row],
    ) -> Result<Vec<Triple>, TabularError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // One column set for the whole batch. A row missing a column contributes UNDEF for
        // it rather than an empty string, which is what makes an empty cell mean "absent".
        let mut columns: Vec<String> = Vec::new();
        for row in rows {
            for (name, _) in &row.cells {
                if !columns.iter().any(|c| c == name) {
                    columns.push(name.clone());
                }
            }
        }
        columns.push(ROWNUM.to_owned());

        let variables: Vec<Variable> = columns
            .iter()
            .filter_map(|c| Variable::new(c).ok())
            .collect();
        if variables.len() != columns.len() {
            return Err(TabularError::Mapping(format!(
                "a column name is not a valid SPARQL variable: {:?}. Use `normalize` to \
                 rewrite headers, or rename the column",
                columns
                    .iter()
                    .filter(|c| Variable::new(c.as_str()).is_err())
                    .collect::<Vec<_>>()
            )));
        }

        let bindings: Vec<Vec<Option<GroundTerm>>> = rows
            .iter()
            .map(|row| {
                variables
                    .iter()
                    .map(|v| {
                        if v.as_str() == ROWNUM {
                            return Some(GroundTerm::Literal(Literal::new_typed_literal(
                                row.number.to_string(),
                                oxrdf::vocab::xsd::INTEGER,
                            )));
                        }
                        row.cells
                            .iter()
                            .find(|(name, _)| name == v.as_str())
                            // An empty cell is UNDEF, not "". This is the whole reason
                            // OPTIONAL-shaped mappings work.
                            .filter(|(_, value)| !value.is_empty())
                            .map(|(_, value)| {
                                GroundTerm::Literal(Literal::new_simple_literal(value))
                            })
                    })
                    .collect()
            })
            .collect();

        let bound = self.with_values(variables, bindings);
        if std::env::var("HOLOS_TABULAR_DEBUG").is_ok() {
            eprintln!("--- rewritten mapping ---
{}
---", bound.to_sse());
        }
        let view = engine.view(session);
        let results = Engine::query_prepared(&view, &bound)?;
        match results {
            QueryResults::Graph(triples) => {
                let mut out = Vec::new();
                for triple in triples {
                    out.push(triple.map_err(|e| TabularError::Mapping(e.to_string()))?);
                }
                Ok(out)
            }
            _ => Err(TabularError::Mapping(
                "a CONSTRUCT mapping must produce triples".into(),
            )),
        }
    }

    /// The mapping with a `VALUES` block joined onto its `WHERE` clause.
    fn with_values(
        &self,
        variables: Vec<Variable>,
        bindings: Vec<Vec<Option<GroundTerm>>>,
    ) -> spargebra::Query {
        let row_variables = variables.clone();
        let values = GraphPattern::Values {
            variables,
            bindings,
        };
        match &self.query {
            spargebra::Query::Construct {
                template,
                dataset,
                pattern,
                base_iri,
            } => spargebra::Query::Construct {
                template: template.clone(),
                dataset: dataset.clone(),
                pattern: inject(pattern, values, &row_variables),
                base_iri: base_iri.clone(),
            },
            // `parse` refuses anything else, so this is unreachable.
            other => other.clone(),
        }
    }
}

/// Puts a `VALUES` block at the **base** of a pattern, where the rows are in scope for
/// everything above it.
///
/// Wrapping the whole pattern instead — `Join(Values, pattern)` — looks equivalent and is
/// not, and the difference is the entire mechanism. `WHERE { BIND(f(?id) AS ?x) }` parses
/// to `Extend { inner: Bgp[], ... }`, and the `Extend` evaluates its expression against
/// *its own inner*. Joined from outside, that inner is the empty BGP, `?id` is unbound, the
/// expression errors, `?x` never binds, and the mapping silently produces nothing.
///
/// So the block is pushed down the left spine — through the operators that wrap a pattern
/// while keeping its bindings in scope — until it reaches a leaf, which is where SPARQL's
/// own text order would have put it.
///
/// # Widening the projection
///
/// A `CONSTRUCT`'s pattern carries a `Project` computed when the query was parsed, listing
/// what the *original* `WHERE` clause binds. For `CONSTRUCT { ?p ex:name ?name } WHERE
/// { BIND(f(?id) AS ?p) }` that list is just `?p`, because without rows `?name` is unbound
/// and projecting it would be meaningless. Rows change that, so the columns are added to
/// the list on the way past — otherwise every column except the one feeding a `BIND` is
/// discarded before the template can use it, and the mapping quietly emits a fraction of
/// the triples it describes.
fn inject(
    pattern: &GraphPattern,
    values: GraphPattern,
    row_variables: &[Variable],
) -> GraphPattern {
    match pattern {
        // A leaf: join here.
        GraphPattern::Bgp { patterns } if patterns.is_empty() => values,
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {
            GraphPattern::Join {
                left: Box::new(values),
                right: Box::new(pattern.clone()),
            }
        }

        // Wrappers: descend, because whatever they compute must see the rows.
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: Box::new(inject(inner, values, row_variables)),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        GraphPattern::Filter { inner, expr } => GraphPattern::Filter {
            inner: Box::new(inject(inner, values, row_variables)),
            expr: expr.clone(),
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: Box::new(inject(inner, values, row_variables)),
            expression: expression.clone(),
        },
        GraphPattern::Distinct { inner } => GraphPattern::Distinct {
            inner: Box::new(inject(inner, values, row_variables)),
        },
        GraphPattern::Reduced { inner } => GraphPattern::Reduced {
            inner: Box::new(inject(inner, values, row_variables)),
        },
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => GraphPattern::Slice {
            inner: Box::new(inject(inner, values, row_variables)),
            start: *start,
            length: *length,
        },
        GraphPattern::Project { inner, variables } => {
            let mut widened = variables.clone();
            for v in row_variables {
                if !widened.contains(v) {
                    widened.push(v.clone());
                }
            }
            GraphPattern::Project {
                inner: Box::new(inject(inner, values, row_variables)),
                variables: widened,
            }
        }
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: Box::new(inject(inner, values, row_variables)),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },

        // Binary operators: the left side is the one text order puts the rows before.
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: Box::new(inject(left, values, row_variables)),
            right: right.clone(),
        },
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => GraphPattern::LeftJoin {
            left: Box::new(inject(left, values, row_variables)),
            right: right.clone(),
            expression: expression.clone(),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: Box::new(inject(left, values, row_variables)),
            right: right.clone(),
        },

        // A UNION has two independent branches and no single base, and GRAPH/SERVICE
        // change the dataset under them. Joining from outside is correct for these.
        other => GraphPattern::Join {
            left: Box::new(values),
            right: Box::new(other.clone()),
        },
    }
}

fn collect_pattern_variables(
    pattern: &GraphPattern,
    out: &mut std::collections::BTreeSet<String>,
) {
    use spargebra::term::{NamedNodePattern, TermPattern};
    match pattern {
        GraphPattern::Bgp { patterns } => {
            for p in patterns {
                for term in [&p.subject, &p.object] {
                    if let TermPattern::Variable(v) = term {
                        out.insert(v.as_str().to_owned());
                    }
                }
                if let NamedNodePattern::Variable(v) = &p.predicate {
                    out.insert(v.as_str().to_owned());
                }
            }
        }
        GraphPattern::Extend {
            inner, expression, ..
        } => {
            collect_pattern_variables(inner, out);
            collect_expression_variables(expression, out);
        }
        GraphPattern::Filter { inner, expr } => {
            collect_pattern_variables(inner, out);
            collect_expression_variables(expr, out);
        }
        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right }
        | GraphPattern::LeftJoin { left, right, .. } => {
            collect_pattern_variables(left, out);
            collect_pattern_variables(right, out);
        }
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Group { inner, .. } => collect_pattern_variables(inner, out),
        _ => {}
    }
}

fn collect_expression_variables(
    expression: &spargebra::algebra::Expression,
    out: &mut std::collections::BTreeSet<String>,
) {
    use spargebra::algebra::Expression;
    match expression {
        Expression::Variable(v) => {
            out.insert(v.as_str().to_owned());
        }
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => {
            collect_expression_variables(a, out);
            collect_expression_variables(b, out);
        }
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            collect_expression_variables(a, out);
        }
        Expression::FunctionCall(_, args) => {
            for a in args {
                collect_expression_variables(a, out);
            }
        }
        Expression::Coalesce(args) => {
            for a in args {
                collect_expression_variables(a, out);
            }
        }
        Expression::If(a, b, c) => {
            collect_expression_variables(a, out);
            collect_expression_variables(b, out);
            collect_expression_variables(c, out);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_select_is_refused() {
        // Accepting one would produce an empty graph and no explanation.
        let outcome = Mapping::parse("SELECT * WHERE { ?s ?p ?o }");
        assert!(outcome.is_err());
        assert!(
            format!("{}", outcome.unwrap_err()).contains("CONSTRUCT"),
            "the message should say what a mapping has to be"
        );
    }

    #[test]
    fn a_syntax_error_says_so() {
        assert!(Mapping::parse("CONSTRUCT { not sparql").is_err());
    }

    #[test]
    fn variables_are_found_in_both_template_and_where() {
        let m = Mapping::parse(
            r#"PREFIX ex: <http://e/>
               CONSTRUCT { ?s ex:name ?name }
               WHERE { BIND(IRI(CONCAT("http://e/", ?id)) AS ?s) FILTER(?keep = "y") }"#,
        )
        .expect("mapping");
        let vars = m.variables();
        for expected in ["s", "name", "id", "keep"] {
            assert!(vars.contains(&expected.to_owned()), "missing {expected} in {vars:?}");
        }
    }
}
