//! Queries the parser accepts and the specifications do not.
//!
//! `spargebra` is a good parser and this is not a complaint about it: a parser that is
//! slightly permissive is a reasonable thing to be, and the W3C negative-syntax suites exist
//! precisely because the boundary is hard. But a store that *evaluates* a query the
//! specification calls invalid is answering a question that has no defined answer, and the
//! answer it gives will look like data.
//!
//! So the store checks, after parsing, and refuses. Two things, both of which the suites
//! test and the parser lets through:
//!
//! - **A triple term's subject.** RDF 1.2 allows a triple term only in object position, so
//!   `<<( <<(:s :p :o)>> :q :z )>>` and `<<( "literal" :q :z )>>` are not terms. Inside
//!   `VALUES` the parser catches them; inside `BIND` it does not, because there the term
//!   becomes an ordinary function call and the function's arguments are unconstrained.
//! - **Nested aggregates.** `COUNT(COUNT(*))` has no meaning: an aggregate consumes a
//!   solution sequence and produces one value, so there is nothing for the outer one to
//!   consume. SPARQL 1.1 §11.4 forbids it in the grammar.
//!
//! # Why here rather than in a fork of the parser
//!
//! Because it is a different question. The parser's job is the grammar; this is the store
//! deciding what it will answer, which is the same judgement `holos-shacl` makes when it
//! refuses a shapes graph it cannot check. Both refuse rather than guess, and for the same
//! reason: a wrong answer that looks right is worse than an error.

use crate::EngineError;
use spargebra::algebra::{AggregateExpression, Expression, Function, GraphPattern};
use spargebra::term::Variable;
use spargebra::Query;

/// Refuses a parsed query the specifications call invalid.
///
/// # Errors
///
/// [`EngineError::BadRequest`] naming what is wrong, in the terms the person who wrote the
/// query would use — not the terms the algebra uses, because they did not write the algebra.
pub fn check(query: &Query) -> Result<(), EngineError> {
    match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Describe { pattern, .. } => pattern_ok(pattern),
        Query::Construct {
            pattern, template, ..
        } => {
            // A CONSTRUCT template is ground-ish triples rather than expressions, so there
            // is nothing here the parser has not already settled — but the WHERE clause is
            // an ordinary pattern and gets the same treatment as any other.
            let _ = template;
            pattern_ok(pattern)
        }
    }
}

/// Walks a pattern, and everything that can hold an expression inside it.
fn pattern_ok(pattern: &GraphPattern) -> Result<(), EngineError> {
    match pattern {
        // Nothing here can hold an expression. Triple terms *can* appear in a BGP, but the
        // parser constrains a pattern's subject by the grammar, which is why the negative
        // tests that put one there already fail to parse.
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {
            Ok(())
        }

        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            pattern_ok(left)?;
            pattern_ok(right)
        }

        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            pattern_ok(left)?;
            pattern_ok(right)?;
            match expression {
                Some(expression) => expr_ok(expression),
                None => Ok(()),
            }
        }

        GraphPattern::Filter { expr, inner } => {
            expr_ok(expr)?;
            pattern_ok(inner)
        }

        GraphPattern::Extend {
            inner, expression, ..
        } => {
            expr_ok(expression)?;
            pattern_ok(inner)
        }

        // A federated pattern is evaluated by the remote endpoint, which will apply its own
        // judgement — but the text we send it is the text we were given, so refusing what is
        // invalid here means not asking someone else to answer it either.
        GraphPattern::Service { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Project { inner, .. } => pattern_ok(inner),

        GraphPattern::OrderBy { inner, expression } => {
            for order in expression {
                match order {
                    spargebra::algebra::OrderExpression::Asc(e)
                    | spargebra::algebra::OrderExpression::Desc(e) => expr_ok(e)?,
                }
            }
            pattern_ok(inner)
        }

        GraphPattern::Group {
            inner,
            aggregates,
            variables: _,
        } => {
            aggregates_ok(aggregates)?;
            for (_, aggregate) in aggregates {
                if let AggregateExpression::FunctionCall { expr, .. } = aggregate {
                    expr_ok(expr)?;
                }
            }
            pattern_ok(inner)
        }
    }
}

/// Refuses an aggregate whose input is another aggregate in the same group.
///
/// The parser flattens `COUNT(COUNT(*))` the way the algebra says to: the inner aggregate is
/// hoisted into the group and the outer one refers to its result by a generated variable. So
/// what a nested aggregate *looks like* here is one aggregate reading a variable that another
/// aggregate in the same group binds — which no query anyone wrote by hand can produce,
/// because those variable names are the parser's own.
fn aggregates_ok(aggregates: &[(Variable, AggregateExpression)]) -> Result<(), EngineError> {
    if aggregates.len() < 2 {
        return Ok(());
    }
    let bound: Vec<&Variable> = aggregates.iter().map(|(v, _)| v).collect();
    for (_, aggregate) in aggregates {
        let AggregateExpression::FunctionCall { expr, .. } = aggregate else {
            continue;
        };
        for variable in &bound {
            if mentions(expr, variable) {
                return Err(EngineError::BadRequest(
                    "an aggregate cannot be the argument of another aggregate: an aggregate \
                     turns a group of solutions into one value, so there is no group left \
                     for the outer one to work on (SPARQL 1.1 §11.4)"
                        .to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// Whether an expression reads a variable.
fn mentions(expression: &Expression, variable: &Variable) -> bool {
    let mut found = false;
    walk(expression, &mut |e| {
        if let Expression::Variable(v) = e {
            if v == variable {
                found = true;
            }
        }
    });
    found
}

/// Checks an expression, and every pattern nested inside it.
///
/// An `EXISTS` carries a whole graph pattern, which can hold aggregates and expressions of
/// its own. Checking the expression alone would leave a nested aggregate inside a
/// `FILTER EXISTS` unrefused — invalid in exactly the same way, and harder to notice.
fn expr_ok(expression: &Expression) -> Result<(), EngineError> {
    triple_terms_ok(expression)?;
    for pattern in exists_patterns(expression) {
        pattern_ok(pattern)?;
    }
    Ok(())
}

/// Refuses a triple term whose subject cannot be one.
fn triple_terms_ok(expression: &Expression) -> Result<(), EngineError> {
    let mut bad: Option<&'static str> = None;
    walk(expression, &mut |e| {
        if bad.is_some() {
            return;
        }
        let Expression::FunctionCall(Function::Triple, args) = e else {
            return;
        };
        // A malformed arity is the parser's business, not this check's.
        let Some(subject) = args.first() else {
            return;
        };
        bad = match subject {
            Expression::Literal(_) => Some("a literal"),
            Expression::FunctionCall(Function::Triple, _) => Some("another triple term"),
            _ => None,
        };
    });

    match bad {
        None => Ok(()),
        Some(what) => Err(EngineError::BadRequest(format!(
            "the subject of a triple term cannot be {what}: RDF 1.2 allows a triple term \
             only as the object of a triple, and its subject must be an IRI or a blank node"
        ))),
    }
}

/// Applies `visit` to an expression and everything inside it.
///
/// Written once rather than per check, because the shape of an `Expression` is the parser's
/// and will grow: a walker that misses a new variant fails to compile, where two hand-rolled
/// recursions would each silently stop looking.
fn walk<'a>(expression: &'a Expression, visit: &mut impl FnMut(&'a Expression)) {
    visit(expression);
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => {}

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
            walk(a, visit);
            walk(b, visit);
        }

        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            walk(a, visit);
        }

        Expression::If(a, b, c) => {
            walk(a, visit);
            walk(b, visit);
            walk(c, visit);
        }

        Expression::In(a, rest) => {
            walk(a, visit);
            for e in rest {
                walk(e, visit);
            }
        }

        Expression::Coalesce(all) | Expression::FunctionCall(_, all) => {
            for e in all {
                walk(e, visit);
            }
        }

        // An EXISTS holds a whole pattern, which can hold expressions of its own. They are
        // reached by `expr_ok` rather than here: this walker is about one expression, and
        // recursing into a pattern from inside it would visit the same nodes twice. Kept as
        // its own arm although the body matches the leaves above, because it is not a leaf —
        // merging them would lose the one place that says so.
        #[allow(clippy::match_same_arms)]
        Expression::Exists(_) => {}
    }
}

/// Patterns inside an `EXISTS`, which `walk` deliberately does not enter.
fn exists_patterns(expression: &Expression) -> Vec<&GraphPattern> {
    let mut out = Vec::new();
    walk(expression, &mut |e| {
        if let Expression::Exists(pattern) = e {
            out.push(pattern.as_ref());
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    fn parsed(query: &str) -> Query {
        SparqlParser::new().parse_query(query).expect("parse")
    }

    fn refused(query: &str) -> String {
        check(&parsed(query))
            .expect_err("should have been refused")
            .to_string()
    }

    fn accepted(query: &str) {
        check(&parsed(query)).expect("should have been accepted");
    }

    // --- triple terms ---------------------------------------------------------------

    /// `tripleterm-subject-03`: a triple term as the subject of a triple term.
    #[test]
    fn a_triple_term_cannot_be_a_triple_terms_subject() {
        let why = refused(
            "PREFIX : <http://e/> SELECT * { BIND( <<( <<(:s :p :o )>> :q :z )>> AS ?X ) }",
        );
        assert!(why.contains("another triple term"), "{why}");
    }

    /// `tripleterm-subject-06`: a literal as the subject of a triple term.
    #[test]
    fn a_literal_cannot_be_a_triple_terms_subject() {
        let why = refused("PREFIX : <http://e/> SELECT * { BIND( <<( \"l\" :q :z )>> AS ?X ) }");
        assert!(why.contains("a literal"), "{why}");
    }

    /// The thing RDF 1.2 *does* allow, which a check that was one step too broad would take
    /// with it.
    #[test]
    fn a_triple_term_in_object_position_is_fine() {
        accepted("PREFIX : <http://e/> SELECT * { BIND( <<( :s :q <<(:a :b :c)>> )>> AS ?X ) }");
        accepted("PREFIX : <http://e/> SELECT * { BIND( <<( :s :q \"lit\" )>> AS ?X ) }");
    }

    /// A variable subject is not known to be bad until it is bound, so it is not this
    /// check's business — the evaluator's type error is the right answer there.
    #[test]
    fn a_variable_subject_is_left_alone() {
        accepted("PREFIX : <http://e/> SELECT * { BIND( <<( ?s :q :z )>> AS ?X ) }");
    }

    /// Nesting is found wherever it is, not only at the top of an expression.
    #[test]
    fn a_bad_triple_term_is_found_inside_another_expression() {
        let why = refused(
            "PREFIX : <http://e/> SELECT * { FILTER( isTriple(IF(true, <<( \"l\" :q :z )>>, \
             :other)) ) }",
        );
        assert!(why.contains("a literal"), "{why}");
    }

    // --- aggregates -----------------------------------------------------------------

    /// `nested-aggregate-functions`.
    #[test]
    fn an_aggregate_cannot_take_an_aggregate() {
        let why = refused("SELECT (COUNT(COUNT(*)) AS ?c) WHERE {}");
        assert!(why.contains("another aggregate"), "{why}");
        let why = refused("SELECT (SUM(MAX(?v)) AS ?c) WHERE { VALUES ?v { 1 2 } }");
        assert!(why.contains("another aggregate"), "{why}");
    }

    /// Several aggregates side by side are ordinary, and the check must not read them as
    /// nested just because there is more than one.
    #[test]
    fn aggregates_beside_each_other_are_fine() {
        accepted(
            "SELECT (COUNT(?v) AS ?n) (SUM(?v) AS ?s) (AVG(?v) AS ?a) \
             WHERE { VALUES ?v { 1 2 3 } }",
        );
    }

    /// An expression *over* an aggregate's result is not a nested aggregate: the arithmetic
    /// happens after the grouping, outside it.
    #[test]
    fn arithmetic_on_an_aggregate_is_fine() {
        accepted("SELECT (COUNT(?v) + 1 AS ?n) WHERE { VALUES ?v { 1 2 3 } }");
        accepted("SELECT (SUM(?v) / COUNT(?v) AS ?mean) WHERE { VALUES ?v { 1 2 3 } }");
    }

    /// An aggregate over an expression of ordinary variables is the common case.
    #[test]
    fn an_aggregate_over_an_expression_is_fine() {
        accepted("SELECT (SUM(?a + ?b) AS ?s) WHERE { VALUES (?a ?b) { (1 2) } }");
    }

    /// An `EXISTS` carries a pattern, and what is invalid inside one is invalid.
    #[test]
    fn a_pattern_inside_an_exists_is_checked_too() {
        let why = refused(
            "PREFIX : <http://e/> SELECT * WHERE { \
             FILTER EXISTS { SELECT (COUNT(COUNT(*)) AS ?c) WHERE {} } }",
        );
        assert!(why.contains("another aggregate"), "{why}");
    }

    #[test]
    fn a_query_with_nothing_wrong_with_it_passes() {
        accepted("SELECT * WHERE { ?s ?p ?o }");
        accepted(
            "PREFIX : <http://e/> SELECT ?s (COUNT(?o) AS ?n) WHERE { ?s :p ?o } \
             GROUP BY ?s HAVING (COUNT(?o) > 1) ORDER BY DESC(?n) LIMIT 5",
        );
    }
}
