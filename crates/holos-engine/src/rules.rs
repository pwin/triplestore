//! Recursive rules, run to a fixpoint: Datalog, written as SPARQL.
//!
//! # A rule is a `CONSTRUCT`
//!
//! ```sparql
//! # ancestors
//! CONSTRUCT { ?x ex:ancestorOf ?z }
//! WHERE     { ?x ex:parentOf ?y . ?y ex:ancestorOf ?z }
//! ```
//!
//! Head and body, which is what a Horn rule is. Writing rules in SPARQL rather than in a
//! syntax of their own is the whole reason this module is small: `spargebra` parses them,
//! `spareval` and [`crate::bindjoin`] evaluate them, [`crate::view::DatasetView`] applies
//! access policy to every read a rule makes, and the statistics reorder each body. `DESIGN.md`
//! §4 is explicit that the front end is reused rather than rewritten, and a second rule
//! language would have meant a second parser, a second evaluator and a second set of bugs.
//!
//! SWRL's DL-safe Horn subset maps onto the same shape, so it is a surface syntax over this
//! rather than a separate engine.
//!
//! # Where the derived facts go, and why not the default graph
//!
//! Into a graph of their own — [`DEFAULT_GRAPH_IRI`] — for the reasons [`crate::entailment`]
//! gives at length: they can be dropped again exactly, a reader can tell an inference from
//! something somebody asserted, and §14's policy treats materialised inference as carrying
//! information around the scan, so it has to be addressable.
//!
//! A rule body's default graph is the **assertions plus the derivations**, so a rule fires on
//! what another rule derived — which is what makes the fixpoint recursive rather than a single
//! pass. Not `union_default_graph`, which would be the obvious choice and is wrong here: that
//! is the union of the *named* graphs and deliberately leaves the store's own default graph
//! out, so every body matched nothing and every rule derived nothing. `QueryOptions::timeout`
//! warns about it; this module found out anyway. A rule wanting some other named graph names
//! it with `GRAPH`.
//!
//! # What is refused, and why each one
//!
//! A rule set is checked before anything runs, and four things are refused. None of them is
//! a limitation of the evaluator; each would break a property the fixpoint depends on.
//!
//! - **Negation** — `MINUS`, `NOT EXISTS`, `EXISTS`. Datalog with negation needs its rules
//!   *stratified*: evaluated in an order where nothing a rule negates can still grow. Without
//!   that, `p :- not q` and `q :- not p` have two equally good models and iteration picks
//!   whichever it happens to reach first. Refusing is not a stand-in for stratification, it is
//!   the alternative to shipping an engine that answers non-deterministically and says
//!   nothing about it.
//! - **A blank node in the template.** It is a *fresh* blank node per solution per round, so
//!   a rule deriving one derives a new fact every round for ever. This is value invention, and
//!   it is exactly what Datalog leaves out in order to terminate at all.
//! - **`BNODE`, `UUID`, `STRUUID`, `RAND`, `NOW`.** The same problem by another route, plus
//!   non-determinism: a body whose answer changes between rounds has no fixpoint to reach.
//! - **`SERVICE`.** A remote endpoint is not part of the fixpoint, cannot be re-read
//!   consistently across rounds, and would make termination depend on somebody else's data.
//!
//! What is allowed is everything monotone: joins, `UNION`, `OPTIONAL`, `FILTER`, property
//! paths including the closures [`crate::reach`] walks, and aggregation. Each of those can
//! only gain solutions as facts are added, which is what the fixpoint needs.
//!
//! # Iteration, and what it costs
//!
//! Each round evaluates every rule and inserts what is new; when a round adds nothing the
//! fixpoint is reached. This is **naïve** iteration: a round re-derives what earlier rounds
//! already derived and throws the duplicates away, which the store's `insert` reports for
//! free by returning whether the quad was new.
//!
//! Semi-naïve iteration — restricting each round to bindings that touch a fact the *previous*
//! round derived — is the standard improvement and is not done here yet. It needs each rule
//! body rewritten once per body pattern, with that pattern scoped to a graph holding the last
//! round's delta, and it is worth doing against a measurement rather than on principle.
//! [`crate::reach`] already has the shape for the one case that matters most, a closure over a
//! single predicate, and a transitive rule written as a property path gets that today without
//! any of this: `CONSTRUCT { ?x ex:ancestorOf ?z } WHERE { ?x ex:parentOf+ ?z }` is one round,
//! one walk, and no fixpoint at all.
//!
//! Two backstops, because a rule set is user input: [`DEFAULT_ROUNDS`] caps the iterations and
//! [`DEFAULT_BUDGET`] caps the facts. Hitting either is an error rather than a truncated
//! answer — a partial materialisation looks exactly like a complete one to every later query,
//! which is the worst way for this to fail.

use oxrdf::{GraphName, NamedNode, Quad, Triple};
use spareval::QueryResults;
use spargebra::algebra::{Expression, GraphPattern};
use spargebra::term::TermPattern;
use spargebra::{Query, SparqlParser};

use holos_security::Session;

use crate::{Engine, EngineError, QueryOptions};

/// What a rule body reads: the store's default graph and the derived graph, merged.
///
/// Exposed because a caller verifying what was derived wants to read the same thing the rules
/// did, and reaching for `union_default_graph` instead is the mistake this module already made
/// once.
#[must_use]
pub fn reading(graph: &NamedNode) -> QueryOptions {
    let mut options = QueryOptions::new();
    options.default_graphs = Some(vec![
        GraphName::DefaultGraph,
        GraphName::NamedNode(graph.clone()),
    ]);
    options
}

/// Where derived facts go unless the caller names somewhere else.
pub const DEFAULT_GRAPH_IRI: &str = "https://holos.dev/ns#derived";

/// Facts a materialisation may derive before it is refused.
pub const DEFAULT_BUDGET: usize = 10_000_000;

/// Rounds a fixpoint may take before it is refused.
///
/// A monotone rule set over a finite store terminates, so a set still deriving after this many
/// rounds is either enormous or wrong — most often a rule whose head repeats its body with the
/// arguments swapped, which derives for as long as it is asked to. Sixty-four is far past the
/// depth of any recursion these rules are written for: transitive closure over a hierarchy is
/// bounded by its depth, and a hierarchy sixty-four deep is a different kind of problem.
pub const DEFAULT_ROUNDS: usize = 64;

/// One rule: a `CONSTRUCT` with its source text and a name for diagnostics.
#[derive(Debug)]
struct Rule {
    /// What to call it when something goes wrong: the `#` comment above it, or its position.
    name: String,
    /// Kept as text because [`Engine::query_with`] takes text, and re-serialising a parsed
    /// query each round would be work for nothing.
    text: String,
}

/// A checked set of rules, ready to run.
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

/// What a materialisation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derived {
    /// Facts the rules produced that were not already present.
    pub added: usize,
    /// Rounds before nothing new appeared. One means the rules are not recursive in practice.
    pub rounds: usize,
}

impl RuleSet {
    /// Parses a rule document: a shared prologue, then one or more `CONSTRUCT` blocks.
    ///
    /// The prologue is every line before the first `CONSTRUCT` — `PREFIX` and `BASE`
    /// declarations, which apply to every rule, so they are written once. Each rule then
    /// starts with `CONSTRUCT` at the beginning of a line, and a `#` comment immediately above
    /// it names it.
    ///
    /// SPARQL has no syntax for several queries in one document, so the split is this module's
    /// and is deliberately dull: a keyword at column zero, which is how the examples are laid
    /// out anyway and which no legal `CONSTRUCT` body can contain.
    ///
    /// # Errors
    ///
    /// A rule that does not parse, is not a `CONSTRUCT`, or uses something [the module
    /// documentation](self) refuses. The message names the rule.
    pub fn parse(text: &str, base: Option<&str>) -> Result<Self, EngineError> {
        let mut prologue = String::new();
        let mut blocks: Vec<(String, String)> = Vec::new();
        let mut pending_name: Option<String> = None;

        for line in text.lines() {
            let starts_rule = line
                .strip_prefix("CONSTRUCT")
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric()));
            if starts_rule {
                let name = pending_name
                    .take()
                    .unwrap_or_else(|| format!("rule {}", blocks.len() + 1));
                blocks.push((name, String::new()));
            }
            // A comment is a candidate name for whatever rule comes next, wherever in the
            // document it sits — between two rules as much as before the first. Anything
            // else that is not blank clears it, so a comment further up does not end up
            // naming a rule it is nowhere near.
            if !starts_rule {
                let trimmed = line.trim();
                if let Some(comment) = trimmed.strip_prefix('#') {
                    pending_name = Some(comment.trim().to_owned());
                } else if !trimmed.is_empty() {
                    pending_name = None;
                }
            }
            match blocks.last_mut() {
                None => {
                    prologue.push_str(line);
                    prologue.push('\n');
                }
                Some((_, body)) => {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }

        if blocks.is_empty() {
            return Err(EngineError::BadRequest(
                "no rules: a rule document holds one or more CONSTRUCT blocks, each starting \
                 at the beginning of a line"
                    .to_owned(),
            ));
        }

        let mut parser = SparqlParser::new();
        if let Some(base) = base {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| EngineError::Io(std::io::Error::other(e.to_string())))?;
        }

        let mut rules = Vec::with_capacity(blocks.len());
        for (name, body) in blocks {
            let text = format!("{prologue}{body}");
            let parsed = parser.clone().parse_query(&text).map_err(|e| {
                EngineError::BadRequest(format!("{name} does not parse: {e}"))
            })?;
            if let Err(why) = check(&parsed) {
                return Err(EngineError::BadRequest(format!("{name}: {why}")));
            }
            rules.push(Rule { name, text });
        }
        Ok(Self { rules })
    }

    /// How many rules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether there are none. A checked set is never empty, so this is always false — it
    /// exists because `clippy` asks for it beside `len`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Each rule's name, in order, for an operator who wants to see what was loaded.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.rules.iter().map(|rule| rule.name.as_str())
    }
}

/// Whether a parsed rule is one this can run to a fixpoint. See [the module
/// documentation](self) for why each refusal is a refusal.
fn check(query: &Query) -> Result<(), String> {
    let Query::Construct {
        template, pattern, ..
    } = query
    else {
        return Err("a rule must be a CONSTRUCT: SELECT, ASK and DESCRIBE derive nothing".into());
    };
    for triple in template {
        for term in [&triple.subject, &triple.object] {
            if matches!(term, TermPattern::BlankNode(_)) {
                return Err(
                    "the template contains a blank node, which is a fresh node every round, \
                     so the rule would derive something new for ever"
                        .into(),
                );
            }
        }
    }
    walk(pattern)
}

/// Walks a rule body for anything that breaks monotonicity or termination.
fn walk(pattern: &GraphPattern) -> Result<(), String> {
    match pattern {
        GraphPattern::Minus { .. } => {
            return Err("MINUS is negation, which needs the rules stratified".into())
        }
        GraphPattern::Service { .. } => {
            return Err("SERVICE reads data outside the fixpoint".into())
        }
        GraphPattern::Filter { expr, inner } => {
            expression(expr)?;
            return walk(inner);
        }
        GraphPattern::Extend {
            inner, expression: e, ..
        } => {
            expression(e)?;
            return walk(inner);
        }
        GraphPattern::LeftJoin {
            left,
            right,
            expression: e,
        } => {
            if let Some(e) = e {
                expression(e)?;
            }
            walk(left)?;
            return walk(right);
        }
        GraphPattern::Join { left, right } | GraphPattern::Union { left, right } => {
            walk(left)?;
            return walk(right);
        }
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Group { inner, .. } => return walk(inner),
        // A BGP, a path, `VALUES`, the empty pattern: nothing to refuse.
        _ => {}
    }
    Ok(())
}

/// Whether an expression is deterministic and invents no terms.
///
/// [`crate::bindjoin::inspect_expression`] already draws exactly this line, for its own
/// reasons — it cannot evaluate `EXISTS` without a dataset, and it must not evaluate `NOW()`
/// twice in one query — so this reuses it rather than keeping a second list of the same
/// functions that would drift from it.
fn expression(expr: &Expression) -> Result<(), String> {
    if crate::bindjoin::inspect_expression(expr, &mut Vec::new()) {
        return Ok(());
    }
    Err(
        "the body uses EXISTS, or one of BNODE, UUID, STRUUID, RAND and NOW: the first is \
         negation, and the rest either invent terms or answer differently each round"
            .into(),
    )
}

/// Runs `rules` to a fixpoint, adding what they derive to `graph`.
///
/// # Errors
///
/// Evaluation failures from any rule, and a refusal if the fixpoint passes `budget` facts or
/// `max_rounds` rounds. Either refusal leaves the facts derived so far in place: they are
/// sound — every one of them follows from the rules — but the set is not complete, and
/// `Derived` is not returned for a run that did not finish, so a caller cannot mistake one
/// for the other.
pub fn materialise(
    engine: &mut Engine,
    session: &mut Session,
    rules: &RuleSet,
    graph: &NamedNode,
    budget: usize,
    max_rounds: usize,
) -> Result<Derived, EngineError> {
    // Assertions and derivations together, so a rule fires on what another rule derived.
    let options = reading(graph);

    let graph_name = GraphName::NamedNode(graph.clone());
    let mut added = 0usize;
    let mut rounds = 0usize;

    loop {
        if rounds >= max_rounds {
            return Err(EngineError::Refused(format!(
                "the rules were still deriving after {max_rounds} rounds. A monotone rule set \
                 over a finite store terminates, so this is either far larger than the round \
                 cap or a rule that derives for ever. Raise the cap, or look for a rule whose \
                 head repeats its body."
            )));
        }
        rounds += 1;

        // Collected before anything is inserted, because the results borrow the view and the
        // view borrows the store that the insert needs mutably. One round's derivations is
        // what this holds, and `budget` is what bounds it.
        let mut fresh: Vec<Triple> = Vec::new();
        for rule in &rules.rules {
            let view = engine.view(session);
            let (results, _) = Engine::query_with(&view, &rule.text, &options).map_err(|e| {
                match e {
                    // A rule that fails names itself; the message otherwise says only that
                    // some query failed, and a rule set is many queries.
                    EngineError::BadRequest(why) => {
                        EngineError::BadRequest(format!("{}: {why}", rule.name))
                    }
                    other => other,
                }
            })?;
            let QueryResults::Graph(triples) = results else {
                return Err(EngineError::BadRequest(format!(
                    "{} did not produce triples, which a CONSTRUCT always does",
                    rule.name
                )));
            };
            for triple in triples {
                fresh.push(triple?);
            }
        }

        let mut this_round = 0usize;
        for triple in fresh {
            let quad = Quad {
                subject: triple.subject,
                predicate: triple.predicate,
                object: triple.object,
                graph_name: graph_name.clone(),
            };
            // `insert` answers whether the quad was new, which is the round's delta without
            // a set of every known fact held beside the store.
            if engine.store_mut().insert(quad.as_ref())? {
                this_round += 1;
                added += 1;
                if added > budget {
                    return Err(EngineError::Refused(format!(
                        "the rules derived more than {budget} facts. Raise the budget with \
                         --max-derived, or narrow the rules."
                    )));
                }
            }
        }

        if this_round == 0 {
            return Ok(Derived { added, rounds });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdfio::RdfFormat;

    const PREFIX: &str = "PREFIX ex: <http://example.com/>\n";

    /// A four-generation chain, so a transitive rule needs several rounds.
    const DATA: &str = r"
        @prefix ex: <http://example.com/> .
        ex:a ex:parentOf ex:b .
        ex:b ex:parentOf ex:c .
        ex:c ex:parentOf ex:d .
    ";

    fn engine() -> Engine {
        let mut e = Engine::new();
        e.bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None).unwrap();
        e
    }

    fn derived_graph() -> NamedNode {
        NamedNode::new_unchecked(DEFAULT_GRAPH_IRI)
    }

    fn run(rules: &str) -> Result<(Derived, Engine), EngineError> {
        let set = RuleSet::parse(&format!("{PREFIX}{rules}"), None)?;
        let mut e = engine();
        let mut session = Session::unrestricted(e.store()).unwrap();
        let outcome = materialise(
            &mut e,
            &mut session,
            &set,
            &derived_graph(),
            DEFAULT_BUDGET,
            DEFAULT_ROUNDS,
        )?;
        Ok((outcome, e))
    }

    /// The ancestors of everything, which needs the rule to fire on its own output.
    #[test]
    fn a_recursive_rule_reaches_its_fixpoint() {
        let (outcome, _) = run(
            "CONSTRUCT { ?x ex:ancestorOf ?y } WHERE { ?x ex:parentOf ?y }\n\
             CONSTRUCT { ?x ex:ancestorOf ?z } WHERE { ?x ex:parentOf ?y . ?y ex:ancestorOf ?z }\n",
        )
        .expect("runs");
        // a→b, a→c, a→d, b→c, b→d, c→d
        assert_eq!(outcome.added, 6, "{outcome:?}");
        // One round to seed, one per generation of chaining, one to find nothing new.
        assert!(outcome.rounds >= 3, "{outcome:?}");
    }

    /// The derived facts are visible to an ordinary query afterwards, which is the point of
    /// materialising them.
    #[test]
    fn what_was_derived_can_be_queried() {
        let (_, e) = run(
            "CONSTRUCT { ?x ex:ancestorOf ?y } WHERE { ?x ex:parentOf ?y }\n\
             CONSTRUCT { ?x ex:ancestorOf ?z } WHERE { ?x ex:parentOf ?y . ?y ex:ancestorOf ?z }\n",
        )
        .expect("runs");
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let (results, _) = Engine::query_with(
            &view,
            &format!("{PREFIX}SELECT ?y WHERE {{ ex:a ex:ancestorOf ?y }}"),
            &reading(&derived_graph()),
        )
        .expect("query");
        let QueryResults::Solutions(iter) = results else {
            panic!("expected solutions");
        };
        assert_eq!(iter.count(), 3, "a's descendants are b, c and d");
    }

    /// A property path does the same closure in one round and no fixpoint, which is the
    /// cheaper way to write it now that the walk exists.
    #[test]
    fn a_path_rule_needs_one_round() {
        let (outcome, _) =
            run("CONSTRUCT { ?x ex:ancestorOf ?z } WHERE { ?x ex:parentOf+ ?z }\n").expect("runs");
        assert_eq!(outcome.added, 6);
        assert_eq!(outcome.rounds, 2, "one round to derive, one to find nothing new");
    }

    /// A rule that derives only what is already there stops after one round.
    #[test]
    fn a_rule_deriving_nothing_new_stops_at_once() {
        let (outcome, _) =
            run("CONSTRUCT { ?x ex:parentOf ?y } WHERE { ?x ex:parentOf ?y }\n").expect("runs");
        // The assertions are in the default graph and the derivations in their own, so the
        // same triple in a second graph *is* a new quad: derived once, then nothing.
        assert_eq!(outcome.added, 3);
        assert_eq!(outcome.rounds, 2);
    }

    #[test]
    fn the_refusals_are_refused() {
        let cases = [
            // A document with no CONSTRUCT has no rules, which is what it is told.
            ("SELECT ?x WHERE { ?x ex:parentOf ?y }", "no rules"),
            (
                "CONSTRUCT { ?x ex:knows _:b } WHERE { ?x ex:parentOf ?y }",
                "blank node",
            ),
            (
                "CONSTRUCT { ?x ex:knows ?y } WHERE { ?x ex:parentOf ?y MINUS { ?x ex:parentOf ex:c } }",
                "MINUS",
            ),
            (
                "CONSTRUCT { ?x ex:knows ?y } WHERE { ?x ex:parentOf ?y FILTER NOT EXISTS { ?y ex:parentOf ?z } }",
                "EXISTS",
            ),
            (
                "CONSTRUCT { ?x ex:at ?n } WHERE { ?x ex:parentOf ?y BIND(NOW() AS ?n) }",
                "NOW",
            ),
            (
                "CONSTRUCT { ?x ex:id ?n } WHERE { ?x ex:parentOf ?y BIND(UUID() AS ?n) }",
                "UUID",
            ),
        ];
        for (rules, expected) in cases {
            let error = RuleSet::parse(&format!("{PREFIX}{rules}\n"), None)
                .expect_err(&format!("must be refused: {rules}"))
                .to_string();
            assert!(
                error.contains(expected),
                "wrong reason for {rules}: {error}"
            );
        }
    }

    /// A rule set that derives for ever is stopped by the round cap rather than running.
    ///
    /// `?y ex:before ?x` from `?x ex:before ?y` alternates without ever settling, because each
    /// round derives the pair it did not have. It does terminate eventually — the domain is
    /// finite — but the point is that a cap exists and reports itself.
    #[test]
    fn the_round_cap_refuses_rather_than_running() {
        let set = RuleSet::parse(
            &format!("{PREFIX}CONSTRUCT {{ ?x ex:step ?z }} WHERE {{ ?x ex:step ?y . ?y ex:parentOf ?z }}\nCONSTRUCT {{ ?x ex:step ?y }} WHERE {{ ?x ex:parentOf ?y }}\n"),
            None,
        )
        .expect("parses");
        let mut e = engine();
        let mut session = Session::unrestricted(e.store()).unwrap();
        // One round is not enough for a chain, so the cap bites.
        let error = materialise(&mut e, &mut session, &set, &derived_graph(), DEFAULT_BUDGET, 1)
            .expect_err("the cap must refuse");
        assert!(error.to_string().contains("after 1 rounds"), "{error}");
    }

    /// And the fact budget refuses too, rather than filling the store.
    #[test]
    fn the_budget_refuses() {
        let set = RuleSet::parse(
            &format!("{PREFIX}CONSTRUCT {{ ?x ex:ancestorOf ?z }} WHERE {{ ?x ex:parentOf+ ?z }}\n"),
            None,
        )
        .expect("parses");
        let mut e = engine();
        let mut session = Session::unrestricted(e.store()).unwrap();
        let error = materialise(&mut e, &mut session, &set, &derived_graph(), 2, DEFAULT_ROUNDS)
            .expect_err("the budget must refuse");
        assert!(error.to_string().contains("more than 2 facts"), "{error}");
    }

    /// The prologue is shared, and a `#` comment above a rule names it in diagnostics.
    #[test]
    fn a_comment_names_a_rule() {
        let error = RuleSet::parse(
            "PREFIX ex: <http://example.com/>\n\
             # the ancestor seed\n\
             CONSTRUCT { ?x ex:knows _:b } WHERE { ?x ex:parentOf ?y }\n",
            None,
        )
        .expect_err("refused")
        .to_string();
        assert!(error.contains("the ancestor seed"), "{error}");
    }

    #[test]
    fn a_document_with_no_rules_says_so() {
        let error = RuleSet::parse("PREFIX ex: <http://example.com/>\n", None)
            .expect_err("refused")
            .to_string();
        assert!(error.contains("no rules"), "{error}");
    }
}
