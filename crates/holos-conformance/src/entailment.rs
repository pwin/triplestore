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
//! # Datatype entailment
//!
//! `mf:recognizedDatatypes` says which datatypes the recogniser knows, and for those, two
//! literals denoting the same *value* are interchangeable: `"042"^^xsd:integer` entails
//! `"42"^^xsd:integer`. Handled by canonicalising both graphs before the instance check, so
//! the search stays a comparison of terms rather than growing a notion of equality.
//!
//! A datatype the test recognises but this canonicaliser does not is a skip by name rather
//! than a guess: leaving `rdf:JSON` literals lexical would answer the negative tests right by
//! accident and the positive ones wrong, which is worse than declining both.
//!
//! Consistency — what `mf:result false` asserts — is still not decided. It is the other half
//! of the same machinery: an ill-formed literal of a recognised datatype makes a graph
//! inconsistent, and so does a range clash between two datatypes whose value spaces are
//! disjoint.

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

    // A datatype this canonicaliser does not know is declined rather than treated
    // lexically: that would answer the negative tests right by accident and the positive
    // ones wrong.
    let unsupported: Vec<&str> = test
        .recognized_datatypes
        .iter()
        .filter(|d| !CANONICALISABLE.contains(&d.as_str()))
        .map(String::as_str)
        .collect();
    if !unsupported.is_empty() {
        return Outcome::skip(format!(
            "recognises datatypes this canonicaliser does not: {}",
            unsupported.join(", ")
        ));
    }
    let recognized: std::collections::HashSet<&str> = test
        .recognized_datatypes
        .iter()
        .map(String::as_str)
        .collect();

    // `mf:result false` asserts (in)consistency rather than naming a conclusion. A positive
    // test says the premise entails falsehood — it has no model — and a negative one says it
    // has. Both are datatype judgements here: nothing else in these suites can make an RDF
    // graph unsatisfiable.
    if test.result_is_false {
        let Some(action) = test.action.as_ref() else {
            return Outcome::skip("no mf:action");
        };
        let premise = match crate::manifest::parse_dataset(
            action,
            &crate::manifest::base_for(test, action),
        ) {
            Ok(d) => d,
            Err(e) => return Outcome::fail(format!("upstream: premise did not parse: {e}")),
        };
        let closure = match close(&premise, regime) {
            Ok(g) => g,
            Err(e) => return Outcome::fail(format!("computing the {regime:?} closure: {e}")),
        };
        let broken = inconsistent(&closure, &recognized);
        return match (positive, broken) {
            (true, true) | (false, false) => Outcome::Passed,
            (true, false) => Outcome::fail(
                "the premise should be inconsistent and no datatype clash was found".to_owned(),
            ),
            (false, true) => Outcome::fail(
                "the premise should be consistent and a datatype clash was found".to_owned(),
            ),
        };
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

    let mut closure = match close(&premise, regime) {
        Ok(g) => g,
        Err(e) => return Outcome::fail(format!("computing the {regime:?} closure: {e}")),
    };
    // Witnesses before canonicalisation, because the type a literal witnesses is the
    // datatype the graph *wrote* — `"42"^^xsd:integer` is an `xsd:integer`, and it stays one
    // after canonicalising into the decimal value space it shares.
    add_literal_witnesses(&mut closure, &recognized);
    canonicalise(&mut closure, &recognized);
    let conclusion = canonicalised(&conclusion, &recognized);
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

/// The datatypes whose value space this can compute.
///
/// Everything else is left alone and, if a test *recognises* it, declines the test.
const CANONICALISABLE: &[&str] = &[
    "http://www.w3.org/2001/XMLSchema#integer",
    "http://www.w3.org/2001/XMLSchema#decimal",
    "http://www.w3.org/2001/XMLSchema#int",
    "http://www.w3.org/2001/XMLSchema#long",
    "http://www.w3.org/2001/XMLSchema#short",
    "http://www.w3.org/2001/XMLSchema#byte",
    "http://www.w3.org/2001/XMLSchema#nonNegativeInteger",
    "http://www.w3.org/2001/XMLSchema#positiveInteger",
    "http://www.w3.org/2001/XMLSchema#unsignedInt",
    "http://www.w3.org/2001/XMLSchema#unsignedLong",
    "http://www.w3.org/2001/XMLSchema#float",
    "http://www.w3.org/2001/XMLSchema#double",
    "http://www.w3.org/2001/XMLSchema#string",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString",
];

/// A literal rewritten so that equal values are equal terms, or `None` to leave it alone.
///
/// The integer family and `xsd:decimal` share one value space, so they canonicalise into it
/// together — which is what makes `"10"^^xsd:integer` entail `"10.0"^^xsd:decimal`.
///
/// `xsd:float` and `xsd:double` do not, and are keyed by their IEEE *bits* rather than by
/// comparison. That gets three things right at once that `==` gets wrong: `+0` and `-0` are
/// distinct values and must not be interchangeable, two lexical forms that round to one
/// binary value must be, and `NaN` is identical to itself.
fn canonical_literal(
    lit: &oxrdf::Literal,
    recognized: &std::collections::HashSet<&str>,
) -> Option<oxrdf::Literal> {
    let datatype = lit.datatype().as_str().to_owned();
    if !recognized.contains(datatype.as_str()) {
        // An unrecognised datatype is opaque: two lexical forms of it are two values, which
        // is exactly what `mf:unrecognizedDatatypes` asserts.
        return None;
    }
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let local = datatype.strip_prefix(XSD)?;
    // Not trimmed. `" 3 "^^xsd:int` is *ill-formed*, not a spelling of 3 — the suite is
    // explicit that a well-formed literal is unrelated to an ill-formed one even when they
    // differ only by whitespace. Trimming here made them one value and broke the test that
    // says they are two.
    let text = lit.value();
    match local {
        "float" => {
            let v: f32 = text.parse().ok()?;
            Some(oxrdf::Literal::new_typed_literal(
                format!("{:08x}", v.to_bits()),
                oxrdf::NamedNode::new_unchecked(format!("{XSD}float")),
            ))
        }
        "double" => {
            let v: f64 = text.parse().ok()?;
            Some(oxrdf::Literal::new_typed_literal(
                format!("{:016x}", v.to_bits()),
                oxrdf::NamedNode::new_unchecked(format!("{XSD}double")),
            ))
        }
        "integer" | "decimal" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "unsignedInt" | "unsignedLong" => {
            let v: oxsdatatypes::Decimal = text.parse().ok()?;
            Some(oxrdf::Literal::new_typed_literal(
                v.to_string(),
                oxrdf::NamedNode::new_unchecked(format!("{XSD}decimal")),
            ))
        }
        // `xsd:string` is already the datatype of a plain literal in RDF 1.1, so oxrdf has
        // unified them before this sees them. Trimming is *not* right here: whitespace is
        // significant in a string.
        "string" => None,
        _ => None,
    }
}

/// A term rewritten so equal values are equal terms, recursing through triple terms.
///
/// The recursion is not decoration: `opaque-literal` states the whole property inside a
/// triple term — `<<( :a :b "042"^^xsd:integer )>>` entails `<<( :a :b "42"^^xsd:integer )>>`
/// — so a canonicaliser that only looked at the top level would answer no.
fn canonical_term(term: &Term, recognized: &std::collections::HashSet<&str>) -> Option<Term> {
    match term {
        Term::Literal(lit) => canonical_literal(lit, recognized).map(Term::Literal),
        Term::Triple(inner) => {
            let object = canonical_term(&inner.object, recognized)?;
            Some(Term::Triple(Box::new(Triple {
                subject: inner.subject.clone(),
                predicate: inner.predicate.clone(),
                object,
            })))
        }
        _ => None,
    }
}

fn canonicalise(g: &mut Graph, recognized: &std::collections::HashSet<&str>) {
    if recognized.is_empty() {
        return;
    }
    let rewritten: Vec<(Triple, Triple)> = g
        .iter()
        .filter_map(|t| {
            let before = t.into_owned();
            let object = canonical_term(&before.object, recognized)?;
            let after = Triple {
                subject: before.subject.clone(),
                predicate: before.predicate.clone(),
                object,
            };
            (before != after).then_some((before, after))
        })
        .collect();
    for (before, after) in rewritten {
        g.remove(&before);
        g.insert(&after);
    }
}

fn canonicalised(d: &Dataset, recognized: &std::collections::HashSet<&str>) -> Dataset {
    let mut out = Dataset::new();
    for quad in d.iter() {
        let mut quad = quad.into_owned();
        if let Some(canonical) = canonical_term(&quad.object, recognized) {
            quad.object = canonical;
        }
        out.insert(&quad);
    }
    out
}

/// Witnesses for what a typed literal is an instance of.
///
/// A literal with datatype `D` is a `D` and an `rdfs:Literal`, but neither can be written
/// down: RDF admits a literal only in object position and both triples would have one as
/// their subject. The entailment is `s p _:b . _:b rdf:type D`, so it is a construction on
/// the graph under test — the same shape as the triple-term witnesses, and for the same
/// reason.
fn add_literal_witnesses(g: &mut Graph, recognized: &std::collections::HashSet<&str>) {
    let ty = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
    let rdfs_literal = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#Literal");
    let occurrences: Vec<Triple> = g
        .iter()
        .filter(|t| matches!(t.object, oxrdf::TermRef::Literal(_)))
        .map(oxrdf::TripleRef::into_owned)
        .collect();
    for triple in occurrences {
        let Term::Literal(lit) = &triple.object else {
            continue;
        };
        // Named from the *canonical* form, so two lexical forms of one value share a
        // witness — the point of recognising the datatype at all.
        let key = canonical_literal(lit, recognized)
            .map_or_else(|| triple.object.to_string(), |c| c.to_string());
        let witness = BlankNode::new_unchecked(format!("literal{:x}", fnv(&key)));
        g.insert(&Triple::new(
            triple.subject.clone(),
            triple.predicate.clone(),
            witness.clone(),
        ));
        g.insert(&Triple::new(
            witness.clone(),
            ty.clone(),
            rdfs_literal.clone(),
        ));
        // Its datatype, but only where the test says the recogniser knows it: an
        // unrecognised datatype IRI denotes nothing the graph can claim membership of.
        let datatype = lit.datatype();
        if recognized.contains(datatype.as_str()) {
            g.insert(&Triple::new(witness, ty.clone(), datatype.into_owned()));
        }
    }
}

/// The value space a datatype belongs to, for deciding whether two of them can clash.
///
/// A family, not a datatype: `xsd:integer` and `xsd:decimal` name the same values, so a
/// literal of one is in the other's value space. `xsd:float` and `xsd:double` name different
/// ones, and neither names a string.
fn value_space(datatype: &str) -> Option<&'static str> {
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    // A language-tagged string is a *pair* of a string and a tag, so its value space is
    // disjoint from `xsd:string` rather than a subset of it. That disjointness is the whole
    // content of `rdfs-entailment-test002`.
    if datatype == "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" {
        return Some("langString");
    }
    match datatype.strip_prefix(XSD)? {
        "integer" | "decimal" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "unsignedInt" | "unsignedLong" => Some("decimal"),
        "float" => Some("float"),
        "double" => Some("double"),
        "string" => Some("string"),
        _ => None,
    }
}

/// Whether a literal's lexical form is in its own datatype's lexical space.
fn well_formed(lit: &oxrdf::Literal) -> bool {
    let text = lit.value();
    match value_space(lit.datatype().as_str()) {
        Some("decimal") => text.parse::<oxsdatatypes::Decimal>().is_ok(),
        Some("float") => text.parse::<f32>().is_ok(),
        Some("double") => text.parse::<f64>().is_ok(),
        // Every string is a well-formed string.
        Some("string") | None => true,
        Some(_) => true,
    }
}

/// Every literal a term carries, including inside triple terms.
fn literals_in(term: &Term) -> Vec<oxrdf::Literal> {
    match term {
        Term::Literal(lit) => vec![lit.clone()],
        Term::Triple(inner) => literals_in(&inner.object),
        _ => Vec::new(),
    }
}

/// Whether a graph has no model, which for these suites is always a datatype clash.
///
/// Two ways to reach one, and both need the datatype to be *recognised*: an unrecognised
/// datatype IRI denotes nothing in particular, so no literal written with it can be wrong.
/// That is why `datatypes-non-well-formed-literal-1` and `-2` share a premise and disagree —
/// the only difference between them is the recogniser.
///
/// 1. An ill-formed literal. `"flargh"^^xsd:integer` denotes nothing, and a graph asserting
///    it asserts something unsatisfiable.
/// 2. A range clash. `ex:p rdfs:range xsd:string` with `ex:a ex:p "25"^^xsd:integer` says the
///    same term is both an integer and a string, and those value spaces are disjoint.
fn inconsistent(g: &Graph, recognized: &std::collections::HashSet<&str>) -> bool {
    if recognized.is_empty() {
        return false;
    }
    let range = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#range");
    let mut ranges: HashMap<String, Vec<String>> = HashMap::new();
    for t in g.iter() {
        // Keyed by the *subject*: `ex:p rdfs:range xsd:string` says what `ex:p`'s objects are.
        if t.predicate == range.as_ref() {
            if let (
                oxrdf::NamedOrBlankNodeRef::NamedNode(property),
                oxrdf::TermRef::NamedNode(class),
            ) = (t.subject, t.object)
            {
                ranges
                    .entry(property.as_str().to_owned())
                    .or_default()
                    .push(class.as_str().to_owned());
            }
        }
    }

    for t in g.iter() {
        // Through triple terms as well as at the top level: `malformed-literal` states the
        // whole claim inside one, and a graph asserting a triple term asserts what it says.
        if literals_in(&t.object.into_owned())
            .iter()
            .any(|lit| recognized.contains(lit.datatype().as_str()) && !well_formed(lit))
        {
            return true;
        }
        let oxrdf::TermRef::Literal(lit) = t.object else {
            continue;
        };
        let datatype = lit.datatype().as_str().to_owned();
        for class in ranges.get(t.predicate.as_str()).into_iter().flatten() {
            if !recognized.contains(class.as_str()) {
                continue;
            }
            // Disjoint value spaces, so nothing can be in both. A datatype outside the
            // families this understands is not judged: `None` means "no opinion", and an
            // opinion is what an inconsistency claim is.
            match (value_space(&datatype), value_space(class)) {
                (Some(a), Some(b)) if a != b => return true,
                _ => {}
            }
        }
    }
    false
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
    // Sorted, so the search visits candidates in the same order on every run. The answer
    // does not depend on the order — that is what the backtracking is for — but the work
    // does, and a conformance tool whose cost and diagnostics move with a hash seed is one
    // whose results cannot be compared between runs.
    let mut facts: Vec<Triple> = closure.iter().map(oxrdf::TripleRef::into_owned).collect();
    facts.sort_by_cached_key(ToString::to_string);
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdfio::{RdfFormat, RdfParser};

    /// The checker decides ninety-odd conformance tests and had no tests of its own. A
    /// mutation audit put numbers on that: removing the binding-consistency check, or the
    /// predicate comparison, each cost only four of them. The suites exercise the search;
    /// they do not stress it, because most fixtures are small and most conclusions differ
    /// from their premises in one place. These do stress it.
    fn graph(turtle: &str) -> Graph {
        let mut g = Graph::new();
        let parser = RdfParser::from_format(RdfFormat::Turtle)
            .with_base_iri("http://example.com/")
            .expect("base");
        for quad in parser.for_reader(turtle.as_bytes()) {
            let quad = quad.expect("parse");
            g.insert(&Triple {
                subject: quad.subject,
                predicate: quad.predicate,
                object: quad.object,
            });
        }
        g
    }

    fn dataset(turtle: &str) -> Dataset {
        let mut d = Dataset::new();
        for triple in graph(turtle).iter() {
            d.insert(&oxrdf::Quad {
                subject: triple.subject.into_owned(),
                predicate: triple.predicate.into_owned(),
                object: triple.object.into_owned(),
                graph_name: GraphName::DefaultGraph,
            });
        }
        d
    }

    const PREFIX: &str = "@prefix : <http://example.com/ns#> .\n";

    fn holds(premise: &str, conclusion: &str) -> bool {
        entails(
            &graph(&format!("{PREFIX}{premise}")),
            &dataset(&format!("{PREFIX}{conclusion}")),
        )
    }

    #[test]
    fn a_subgraph_is_entailed_and_a_stranger_is_not() {
        assert!(holds(":a :p :b . :c :q :d .", ":a :p :b ."));
        assert!(!holds(":a :p :b .", ":a :p :c ."));
    }

    #[test]
    fn the_predicate_is_not_a_wildcard() {
        // Removing the predicate comparison was caught by only four conformance tests, none
        // of which stated this as the thing they were checking.
        assert!(!holds(":a :p :b .", ":a :q :b ."));
    }

    #[test]
    fn a_blank_node_generalises_any_term() {
        // Generalisation needs no rule of its own: a blank node in the conclusion is free.
        assert!(holds(":a :p :b .", ":a :p _:x ."));
        assert!(holds(":a :p \"10\" .", ":a :p _:x ."));
        assert!(holds(":a :p :b .", "_:x :p :b ."));
    }

    #[test]
    fn one_blank_node_cannot_denote_two_things() {
        // The property the binding map exists for. Both goals match *some* triple; what
        // must fail is matching them with inconsistent values for `_:x`.
        assert!(!holds(":a :p :b . :c :p :d .", "_:x :p :b . _:x :p :d ."));
        assert!(holds(":a :p :b . :a :p :d .", "_:x :p :b . _:x :p :d ."));
    }

    #[test]
    fn distinct_blank_nodes_may_denote_the_same_thing() {
        // An instance mapping is a function, not an injection: two conclusion blank nodes
        // are allowed to land on one term. Requiring them to differ would reject
        // entailments that hold.
        assert!(holds(":a :p :b .", "_:x :p _:y ."));
        assert!(holds(":a :p :a .", "_:x :p _:x ."));
    }

    #[test]
    fn the_search_backtracks_rather_than_taking_the_first_match() {
        // Both `:a` and `:b` satisfy the first goal, and only `:b` satisfies the second, so
        // a search that committed to its first candidate would answer no. The facts are
        // visited in sorted order, which puts the dead end first — without that this test
        // would pass or fail on a hash seed, and a version of it that did exactly that
        // failed to catch the mutation it was written for.
        assert!(holds(
            ":a :p :two . :b :p :two . :b :q :three .",
            "_:x :p :two . _:x :q :three ."
        ));
    }

    #[test]
    fn a_conclusion_larger_than_the_premise_can_still_hold() {
        // Two goals may map onto one fact. Nothing requires an instance to be injective on
        // triples either.
        assert!(holds(":a :p :b .", "_:x :p :b . _:y :p :b ."));
    }

    #[test]
    fn blank_nodes_inside_triple_terms_are_variables() {
        // RDF 1.2. Comparing triple terms whole made this fail, which is what eight tests in
        // the rdf12 semantics suite are about.
        assert!(holds(
            ":a1 :p1 <<( :a :b :c )>> .",
            ":a1 :p1 <<( _:x :b :c )>> ."
        ));
        assert!(holds(
            ":a1 :p1 <<( :a :b :c )>> .",
            ":a1 :p1 <<( :a :b _:x )>> ."
        ));
        assert!(!holds(
            ":a1 :p1 <<( :a :b :c )>> .",
            ":a1 :p1 <<( _:x :b :d )>> ."
        ));
    }

    #[test]
    fn a_blank_node_stays_consistent_across_a_triple_term_boundary() {
        // The same variable appearing inside a triple term and outside it must denote one
        // thing. This is what separates `same-bnode-same-triple-term` from
        // `different-bnodes-same-triple-term` in the suite.
        assert!(holds(
            ":a :q :a . :a :p <<( :a :b :c )>> .",
            "_:x :q _:x . _:x :p <<( _:x :b :c )>> ."
        ));
        assert!(!holds(
            ":a :q :a . :d :p <<( :a :b :c )>> .",
            "_:x :q _:x . _:x :p <<( _:x :b :c )>> ."
        ));
    }

    #[test]
    fn a_predicate_inside_a_triple_term_is_still_not_a_wildcard() {
        assert!(!holds(
            ":a1 :p1 <<( :a :b :c )>> .",
            ":a1 :p1 <<( :a :other :c )>> ."
        ));
    }

    #[test]
    fn rdf1_is_what_separates_the_rdf_regime_from_simple() {
        // The one thing the RDF closure adds. Without it the conclusion names a triple no
        // premise contains.
        let premise = dataset(&format!("{PREFIX}:a :p :b ."));
        let conclusion = dataset(&format!(
            "{PREFIX}@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n\
             :p a rdf:Property ."
        ));
        assert!(!entails(
            &close(&premise, Regime::Simple).expect("simple"),
            &conclusion
        ));
        assert!(entails(
            &close(&premise, Regime::Rdf).expect("rdf"),
            &conclusion
        ));
    }

    #[test]
    fn a_triple_term_witnesses_a_proposition() {
        // Not expressible as a triple — RDF admits a triple term only as an object — so the
        // entailment is stated with a fresh blank node, and the witness construction is what
        // makes it hold.
        let premise = dataset(&format!("{PREFIX}:a1 :p1 <<( :a :b :c )>> ."));
        let conclusion = dataset(&format!(
            "{PREFIX}@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             :a1 :p1 _:pp . _:pp a rdfs:Proposition ."
        ));
        assert!(entails(
            &close(&premise, Regime::Rdfs).expect("rdfs"),
            &conclusion
        ));
        assert!(
            !entails(
                &close(&premise, Regime::Simple).expect("simple"),
                &conclusion
            ),
            "simple entailment says nothing about what a triple term denotes"
        );
    }
}
