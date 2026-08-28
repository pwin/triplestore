//! The RDF entailment suites, run as entailment rather than as parsing.
//!
//! `mf:PositiveEntailmentTest` and `mf:NegativeEntailmentTest` ask a different question from
//! every other suite here. `mf:action` is a *premise* graph and `mf:result` is a *conclusion*
//! graph, and the test asks whether the first entails the second under a named regime. The
//! generic RDF runner parsed the premise, round-tripped it through the store and compared it
//! against the conclusion as though that were the expected parse — a comparison that was
//! never going to hold, and whose failure was reported as `upstream:`, blaming a parser for
//! answering a question nobody asked it.
//!
//! # What entailment means here
//!
//! `G ⊨ E` holds when some *instance* of `E` is a subgraph of the closure of `G` under the
//! regime. An instance maps `E`'s blank nodes to terms; everything else must appear as it
//! is. So the check is a subgraph homomorphism with blank nodes as the variables, run over
//! the closure rather than over the assertions.
//!
//! Generalisation falls out of the same search for free: a graph entails the one obtained by
//! replacing a term with a fresh blank node, which is why `ex:a ex:b "10"` entails
//! `ex:a ex:b _:x`, and a blank node in the conclusion is already free to match any term.
//!
//! # What is implemented
//!
//! | Regime | Closure |
//! | --- | --- |
//! | `simple` | none — the instance check alone |
//! | `RDF` | rdf1: every predicate denotes an `rdf:Property` |
//! | `RDFS` | [`holos_engine::entailment`]'s rules |
//!
//! The RDFS closure is routed through the engine's own reasoner rather than reimplemented
//! here, so this suite tests the code that ships rather than a second copy written to pass
//! it.
//!
//! Datatype entailment is **not** implemented, and a test that turns on it is skipped by
//! name rather than failed. Deciding that `"010"^^xsd:integer` and `"10"^^xsd:integer`
//! denote the same value is a value-space question, and the same machinery decides whether
//! an ill-formed literal makes a graph inconsistent — which is what `mf:result false`
//! asserts. Answering half of that would be worse than not answering it.

use crate::manifest::TestEntry;
use crate::Outcome;
use oxrdf::{BlankNode, Dataset, Graph, GraphName, NamedNode, Quad, Term, Triple};
use std::collections::HashMap;

/// Which regime a test asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// No closure: the conclusion must be an instance of a subgraph of the premise.
    Simple,
    /// Simple, plus rdf1.
    Rdf,
    /// The RDFS rules.
    Rdfs,
}

impl Regime {
    fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "simple" => Some(Self::Simple),
            "RDF" => Some(Self::Rdf),
            "RDFS" => Some(Self::Rdfs),
            _ => None,
        }
    }
}

/// Whether this runner handles the test, so the caller knows not to send it elsewhere.
#[must_use]
pub fn handles(test: &TestEntry) -> bool {
    let kind = crate::local_name(&test.kind);
    kind == "PositiveEntailmentTest" || kind == "NegativeEntailmentTest"
}

/// Runs one entailment test.
#[must_use]
pub fn run(test: &TestEntry) -> Outcome {
    let positive = crate::local_name(&test.kind) == "PositiveEntailmentTest";

    let Some(regime) = test.mf_entailment_regime.as_deref().and_then(Regime::parse) else {
        return Outcome::skip(format!(
            "entailment regime {:?} is not implemented",
            test.mf_entailment_regime.as_deref().unwrap_or("(absent)")
        ));
    };

    // `mf:result false` asserts consistency rather than naming a conclusion, and every
    // inconsistency in these suites is a datatype judgement: an ill-formed literal, or two
    // datatypes declared to subsume one another under the intensional semantics.
    if test.result_is_false {
        return Outcome::skip(
            "asserts (in)consistency, which needs datatype entailment: not implemented",
        );
    }
    // A recogniser list means the answer turns on a datatype's value space whatever regime
    // the test also names.
    if !test.recognized_datatypes.is_empty() {
        return Outcome::skip(format!(
            "needs datatype entailment over {}: not implemented",
            test.recognized_datatypes.join(", ")
        ));
    }

    let Some(action) = test.action.as_ref() else {
        return Outcome::skip("no mf:action");
    };
    let Some(result) = test.result.as_ref() else {
        return Outcome::skip("no mf:result");
    };
    let premise =
        match crate::manifest::parse_dataset(action, &crate::manifest::base_for(test, action)) {
            Ok(d) => d,
            Err(e) => return Outcome::fail(format!("upstream: premise did not parse: {e}")),
        };
    let conclusion =
        match crate::manifest::parse_dataset(result, &crate::manifest::base_for(test, result)) {
            Ok(d) => d,
            Err(e) => return Outcome::fail(format!("upstream: conclusion did not parse: {e}")),
        };

    let closure = match close(&premise, regime) {
        Ok(g) => g,
        Err(e) => return Outcome::fail(format!("computing the {regime:?} closure: {e}")),
    };
    let holds = entails(&closure, &conclusion);

    match (positive, holds) {
        (true, true) | (false, false) => Outcome::Passed,
        (true, false) => Outcome::fail(format!(
            "{regime:?} entailment should hold and does not: the conclusion has no instance \
             in a closure of {} triples",
            closure.len()
        )),
        (false, true) => Outcome::fail(format!(
            "{regime:?} entailment should not hold and does: the conclusion has an instance \
             in a closure of {} triples",
            closure.len()
        )),
    }
}

/// The closure of a premise under a regime.
fn close(premise: &Dataset, regime: Regime) -> Result<Graph, String> {
    let mut g = Graph::new();
    for quad in premise.iter() {
        g.insert(&Triple {
            subject: quad.subject.into_owned(),
            predicate: quad.predicate.into_owned(),
            object: quad.object.into_owned(),
        });
    }
    match regime {
        Regime::Simple => Ok(g),
        // rdf1, and only rdf1. The other RDF axioms describe the `rdf:` vocabulary in terms
        // of itself; no test in these suites asks about them, and adding them would put a
        // vocabulary description into every closure to no end.
        Regime::Rdf => {
            let ty = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
            let property =
                NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#Property");
            let predicates: Vec<NamedNode> = g.iter().map(|t| t.predicate.into_owned()).collect();
            for p in predicates {
                g.insert(&Triple::new(p, ty.clone(), property.clone()));
            }
            Ok(g)
        }
        Regime::Rdfs => {
            let mut closed = rdfs_closure(&g)?;
            add_proposition_witnesses(&mut closed);
            Ok(closed)
        }
    }
}

/// Witnesses for what a triple term denotes.
///
/// RDF 1.2 says a triple term denotes a proposition, so a graph containing `s p <<( t )>>`
/// entails `s p _:b` together with `_:b rdf:type rdfs:Proposition`. It cannot be written as
/// `<<( t )>> rdf:type rdfs:Proposition`, because RDF admits a triple term only in object
/// position and that triple has one as its subject — which is why this is a construction on
/// the closure being tested rather than a rule in the reasoner. Materialising it into a store
/// would mean inventing blank nodes nobody asserted.
///
/// The blank node is named from the triple term it witnesses, so the same term gets the same
/// witness wherever it appears: that is what makes `same-bnode-same-triple-term` hold and
/// keeps two distinct terms distinct.
fn add_proposition_witnesses(g: &mut Graph) {
    let ty = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
    let proposition = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#Proposition");
    let occurrences: Vec<Triple> = g
        .iter()
        .filter(|t| matches!(t.object, oxrdf::TermRef::Triple(_)))
        .map(oxrdf::TripleRef::into_owned)
        .collect();
    for triple in occurrences {
        let witness =
            BlankNode::new_unchecked(format!("proposition{:x}", fnv(&triple.object.to_string())));
        g.insert(&Triple::new(
            triple.subject.clone(),
            triple.predicate.clone(),
            witness.clone(),
        ));
        g.insert(&Triple::new(witness, ty.clone(), proposition.clone()));
    }
}

/// A stable name for a witness. Any function of the term would do; this one is short and
/// does not pull in a dependency for the purpose.
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Whether `conclusion` has an instance that is a subgraph of `closure`.
fn entails(closure: &Graph, conclusion: &Dataset) -> bool {
    let mut goals: Vec<Triple> = Vec::new();
    for quad in conclusion.iter() {
        if quad.graph_name != oxrdf::GraphNameRef::DefaultGraph {
            // Entailment is defined over graphs; a conclusion in a named graph is not
            // something these suites produce, and guessing at one would be worse than
            // declining it.
            return false;
        }
        goals.push(Triple {
            subject: quad.subject.into_owned(),
            predicate: quad.predicate.into_owned(),
            object: quad.object.into_owned(),
        });
    }
    // Ground goals first. One either matches or fails outright, so putting them ahead of the
    // goals carrying blank nodes prunes the search before any binding is guessed.
    goals.sort_by_key(|t| blank_count(t));
    let facts: Vec<Triple> = closure.iter().map(oxrdf::TripleRef::into_owned).collect();
    search(&goals, &facts, &mut HashMap::new())
}

/// Backtracking search for an instance mapping.
///
/// Exponential in the worst case, which is what subgraph homomorphism is. The graphs in
/// these suites are a handful of triples each, and the ground-first ordering means the
/// exponent only ever applies to the blank nodes a conclusion actually has.
fn search(goals: &[Triple], facts: &[Triple], binding: &mut HashMap<BlankNode, Term>) -> bool {
    let Some((goal, rest)) = goals.split_first() else {
        return true;
    };
    for fact in facts {
        let saved = binding.clone();
        if unify(goal, fact, binding) && search(rest, facts, binding) {
            return true;
        }
        *binding = saved;
    }
    false
}

/// Matches one conclusion triple against one closure triple, extending the binding.
fn unify(goal: &Triple, fact: &Triple, binding: &mut HashMap<BlankNode, Term>) -> bool {
    if goal.predicate != fact.predicate {
        return false;
    }
    let goal_subject = Term::from(goal.subject.clone());
    let fact_subject = Term::from(fact.subject.clone());
    bind(&goal_subject, &fact_subject, binding) && bind(&goal.object, &fact.object, binding)
}

/// Binds one position. A position is a variable exactly when it is a blank node.
///
/// Triple terms are matched *through*, not compared whole. RDF 1.2 lets a blank node stand
/// for a term inside a triple term — `<<( :a :b :c )>>` entails `<<( _:x :b :c )>>` — so a
/// structural equality test would answer no to an entailment that holds. The recursion also
/// keeps one blank node consistent across the two places it can appear, inside a triple term
/// and outside it, which is what distinguishes `same-bnode-same-triple-term` from
/// `different-bnodes-same-triple-term`.
fn bind(goal: &Term, fact: &Term, binding: &mut HashMap<BlankNode, Term>) -> bool {
    match (goal, fact) {
        (Term::BlankNode(b), _) => match binding.get(b) {
            Some(already) => already == fact,
            None => {
                binding.insert(b.clone(), fact.clone());
                true
            }
        },
        (Term::Triple(g), Term::Triple(f)) => {
            g.predicate == f.predicate
                && bind(
                    &Term::from(g.subject.clone()),
                    &Term::from(f.subject.clone()),
                    binding,
                )
                && bind(&g.object, &f.object, binding)
        }
        (other, _) => other == fact,
    }
}

/// How many blank nodes a goal carries, counting inside triple terms.
///
/// Only an ordering heuristic, but it has to see through triple terms or a goal whose only
/// variable is nested reads as ground and gets tried first for no reason.
fn blank_count(t: &Triple) -> usize {
    blanks_in(&Term::from(t.subject.clone())) + blanks_in(&t.object)
}

fn blanks_in(t: &Term) -> usize {
    match t {
        Term::BlankNode(_) => 1,
        Term::Triple(inner) => {
            blanks_in(&Term::from(inner.subject.clone())) + blanks_in(&inner.object)
        }
        _ => 0,
    }
}

/// The RDFS closure, computed through the engine's own reasoner.
fn rdfs_closure(g: &Graph) -> Result<Graph, String> {
    use holos_engine::{entailment, Engine};
    use holos_security::Session;

    let mut engine = Engine::new();
    for triple in g.iter() {
        let quad = Quad {
            subject: triple.subject.into_owned(),
            predicate: triple.predicate.into_owned(),
            object: triple.object.into_owned(),
            graph_name: GraphName::DefaultGraph,
        };
        engine
            .store_mut()
            .insert(quad.as_ref())
            .map_err(|e| e.to_string())?;
    }
    let mut session = Session::unrestricted(engine.store()).map_err(|e| e.to_string())?;
    entailment::materialise(&mut engine, &mut session, None, entailment::DEFAULT_BUDGET)
        .map_err(|e| e.to_string())?;

    let mut out = Graph::new();
    for quad in engine.store().iter() {
        let quad = quad.map_err(|e| e.to_string())?;
        out.insert(&Triple {
            subject: quad.subject,
            predicate: quad.predicate,
            object: quad.object,
        });
    }
    Ok(out)
}
