//! RDFS entailment, materialised into a named graph.
//!
//! # The gap this closes
//!
//! A store answers what is *in* it. `my:hasExactGeometry rdfs:subPropertyOf geo:hasGeometry`
//! says that every use of the first is a use of the second, and nothing in the query path
//! acts on that: §17's topology rewrite looks for `geo:hasGeometry` and does not find
//! `my:hasExactGeometry`, so a feature-level GeoSPARQL query against the OGC example dataset
//! comes back with the *geometries* rather than the features they belong to. That is the
//! most concrete instance, and the general one is that every SPARQL query is a query over
//! asserted triples only.
//!
//! Materialising the closure makes the entailed triples real, at which point everything that
//! reads the store — the query path, the topology rewrite, SHACL, the statistics — sees them
//! without any of them having to know about entailment.
//!
//! # Why into a separate graph
//!
//! Entailed triples go into a graph of their own, `holos:entailed` by default, rather than
//! alongside the assertions. Three reasons, in order of how much they matter:
//!
//! 1. **They can be removed again.** A schema changes, or the entailment was a mistake;
//!    `DROP GRAPH <holos:entailed>` undoes it exactly. Mixed into the default graph there is
//!    no way to tell an inference from a statement anyone made.
//! 2. **A reader can tell them apart.** "Who said this?" is a question RDF is supposed to be
//!    able to answer, and an inference nobody asserted is a different kind of thing from a
//!    triple in the source document.
//! 3. **§14 already assumes it.** Access policy treats materialised inference as something
//!    that carries information around the scan, so it has to be addressable.
//!
//! The cost is that queries only see them under the union default graph, or by naming the
//! graph. That is the trade, and it is stated rather than hidden.
//!
//! # The rules
//!
//! | Rule | From | Entails |
//! | --- | --- | --- |
//! | rdfs2 | `p rdfs:domain c` and `x p y` | `x rdf:type c` |
//! | rdfs3 | `p rdfs:range c` and `x p y` | `y rdf:type c` |
//! | rdfs5 | `p rdfs:subPropertyOf q`, `q … r` | `p rdfs:subPropertyOf r` |
//! | rdfs7 | `p rdfs:subPropertyOf q` and `x p y` | `x q y` |
//! | rdfs9 | `c rdfs:subClassOf d` and `x rdf:type c` | `x rdf:type d` |
//! | rdfs11 | `c rdfs:subClassOf d`, `d … e` | `c rdfs:subClassOf e` |
//!
//! The axiomatic and reflexive rules — rdfs4, rdfs6, rdfs10 and the RDF/RDFS axioms — are
//! left out deliberately. They entail `x rdf:type rdfs:Resource` for every term in the
//! graph, which no query is improved by and which would multiply a graph's size for nothing.
//! This is the same set, and the same reasoning, as `holos_shacl_engine::inference`, which
//! does it over the SHACL crate's own graph model rather than over a store.
//!
//! # Cost
//!
//! The closure of a schema is not linear in its size: `n` classes in a `rdfs:subClassOf`
//! chain entail about `n²/2` transitive statements, and rdfs7 rewrites every matching triple
//! once per super-property. So it runs under a budget and refuses rather than running a
//! machine out of memory — a caller who asked for something expensive gets told, which is
//! more useful than a process that disappears.

use crate::{Engine, EngineError};
use holos_core::{vocab, TermId};
use holos_security::{Modes, Session};
use holos_store::{EncodedQuad, GraphFilter};
use rustc_hash::{FxHashMap, FxHashSet};

/// The graph entailed triples are written to unless the caller names another.
pub const DEFAULT_GRAPH_IRI: &str = "https://holos.dev/ns#entailed";

/// How many triples the closure may add before it is abandoned.
///
/// Deliberately generous — this is a bound on a mistake, not a tuning knob. A schema that
/// entails ten million triples is a schema worth looking at before materialising it.
pub const DEFAULT_BUDGET: usize = 10_000_000;

/// What a materialisation did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Entailed {
    /// Triples the closure produced that were not already present.
    pub added: usize,
    /// Rounds of the fixpoint before nothing new appeared.
    pub rounds: usize,
}

/// The vocabulary terms the rules turn on.
struct Rdfs {
    ty: TermId,
    sub_class_of: TermId,
    sub_property_of: TermId,
    domain: TermId,
    range: TermId,
}

impl Rdfs {
    /// All five are in the well-known table, so this cannot fail in practice; it returns an
    /// `Option` rather than panicking because a table edit is the sort of thing that should
    /// surface as an error and not as a crash in a maintenance command.
    fn new() -> Option<Self> {
        Some(Self {
            ty: vocab::encode_iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")?,
            sub_class_of: vocab::encode_iri("http://www.w3.org/2000/01/rdf-schema#subClassOf")?,
            sub_property_of: vocab::encode_iri(
                "http://www.w3.org/2000/01/rdf-schema#subPropertyOf",
            )?,
            domain: vocab::encode_iri("http://www.w3.org/2000/01/rdf-schema#domain")?,
            range: vocab::encode_iri("http://www.w3.org/2000/01/rdf-schema#range")?,
        })
    }
}

/// Materialises the RDFS closure of `engine`'s data into `graph`.
///
/// Reads every graph, including any previous entailment, so running it twice is idempotent
/// rather than compounding: the second run finds everything already present and adds
/// nothing.
///
/// # Errors
///
/// Storage failures, a write the session's policy refuses, and [`EngineError::BadRequest`]
/// when the closure exceeds `budget`.
pub fn materialise(
    engine: &mut Engine,
    session: &mut Session,
    graph: TermId,
    budget: usize,
) -> Result<Entailed, EngineError> {
    let Some(rdfs) = Rdfs::new() else {
        return Err(EngineError::Io(std::io::Error::other(
            "the RDFS vocabulary is missing from the well-known table",
        )));
    };

    // Everything already asserted, so the closure only ever reports what it *adds*. Held as
    // triples rather than quads: entailment is about the statements, and the same statement
    // in two graphs entails the same things once.
    let mut known: FxHashSet<(TermId, TermId, TermId)> = FxHashSet::default();
    for quad in engine
        .store()
        .quads_for_pattern(None, None, None, GraphFilter::Any)
    {
        let q = quad?;
        known.insert((q.subject, q.predicate, q.object));
    }
    let asserted = known.clone();

    // The schema, read once. It is tiny next to the data and consulted on every triple, so
    // walking the store for it per round would dominate the whole operation.
    let mut super_properties: FxHashMap<TermId, FxHashSet<TermId>> = FxHashMap::default();
    let mut super_classes: FxHashMap<TermId, FxHashSet<TermId>> = FxHashMap::default();
    let mut domains: FxHashMap<TermId, FxHashSet<TermId>> = FxHashMap::default();
    let mut ranges: FxHashMap<TermId, FxHashSet<TermId>> = FxHashMap::default();
    for &(s, p, o) in &known {
        if p == rdfs.sub_property_of {
            super_properties.entry(s).or_default().insert(o);
        } else if p == rdfs.sub_class_of {
            super_classes.entry(s).or_default().insert(o);
        } else if p == rdfs.domain {
            domains.entry(s).or_default().insert(o);
        } else if p == rdfs.range {
            ranges.entry(s).or_default().insert(o);
        }
    }

    // rdfs5 and rdfs11: transitive closure of the two hierarchies, computed on the maps
    // before touching the data, because every later rule consults them.
    close_transitively(&mut super_properties);
    close_transitively(&mut super_classes);

    let mut fresh: Vec<(TermId, TermId, TermId)> = Vec::new();
    let mut push = |triple: (TermId, TermId, TermId),
                    known: &mut FxHashSet<(TermId, TermId, TermId)>,
                    fresh: &mut Vec<(TermId, TermId, TermId)>| {
        if known.insert(triple) {
            fresh.push(triple);
        }
    };

    // The transitive hierarchies, as triples. Each map is walked with its own predicate:
    // deciding which one from the contents would misfile a term that is both a sub-property
    // and a sub-class, which nothing forbids.
    for (map, predicate) in [
        (&super_properties, rdfs.sub_property_of),
        (&super_classes, rdfs.sub_class_of),
    ] {
        for (&sub, supers) in map {
            for &sup in supers {
                push((sub, predicate, sup), &mut known, &mut fresh);
            }
        }
    }

    // rdfs2, rdfs3, rdfs7, rdfs9 over the data, to a fixpoint. Each round works from what
    // the previous one produced rather than rescanning everything, so a schema that entails
    // nothing costs one pass.
    let mut frontier: Vec<(TermId, TermId, TermId)> = known.iter().copied().collect();
    let mut rounds = 0usize;
    while !frontier.is_empty() {
        rounds += 1;
        let mut next = Vec::new();
        for (s, p, o) in frontier.drain(..) {
            // rdfs7
            if let Some(supers) = super_properties.get(&p) {
                for &q in supers {
                    push((s, q, o), &mut known, &mut next);
                }
            }
            // rdfs2
            if let Some(classes) = domains.get(&p) {
                for &c in classes {
                    push((s, rdfs.ty, c), &mut known, &mut next);
                }
            }
            // rdfs3
            if let Some(classes) = ranges.get(&p) {
                for &c in classes {
                    push((o, rdfs.ty, c), &mut known, &mut next);
                }
            }
            // rdfs9
            if p == rdfs.ty {
                if let Some(supers) = super_classes.get(&o) {
                    for &d in supers {
                        push((s, rdfs.ty, d), &mut known, &mut next);
                    }
                }
            }
        }
        if known.len().saturating_sub(asserted.len()) > budget {
            return Err(EngineError::BadRequest(format!(
                "the RDFS closure exceeded {budget} new triples and was abandoned; nothing \
                 was written. A schema entailing this much is worth reading before it is \
                 materialised."
            )));
        }
        fresh.extend(next.iter().copied());
        frontier = next;
    }

    // Written only now, so a closure that overruns its budget leaves the store untouched.
    let mut added = 0usize;
    for (s, p, o) in fresh {
        // An entailed triple that repeats an assertion is not worth storing twice; the
        // interesting content of the entailment graph is what was *not* already said.
        if asserted.contains(&(s, p, o)) {
            continue;
        }
        let quad = EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name: Some(graph),
        };
        if !session
            .policy(engine.store())?
            .permits_quad(quad, Modes::WRITE)
        {
            return Err(EngineError::AccessDenied);
        }
        if engine.store_mut().insert_encoded(quad)? {
            added += 1;
        }
    }

    Ok(Entailed { added, rounds })
}

/// Replaces each entry with everything reachable from it.
///
/// Iterative rather than recursive, and it tolerates cycles: `a rdfs:subClassOf b` with `b
/// rdfs:subClassOf a` is legal RDFS, means both are equivalent, and a naive walk of it does
/// not terminate.
fn close_transitively(map: &mut FxHashMap<TermId, FxHashSet<TermId>>) {
    let keys: Vec<TermId> = map.keys().copied().collect();
    for key in keys {
        let mut reached: FxHashSet<TermId> = FxHashSet::default();
        let mut stack: Vec<TermId> = map.get(&key).into_iter().flatten().copied().collect();
        while let Some(next) = stack.pop() {
            if !reached.insert(next) {
                continue;
            }
            if let Some(further) = map.get(&next) {
                stack.extend(further.iter().copied());
            }
        }
        // A cycle can put the key back in its own super-set. `x rdfs:subClassOf x` is true
        // and useless, and rdfs9 would then re-derive every type statement for ever.
        reached.remove(&key);
        map.insert(key, reached);
    }
}
