//! Reachability over one predicate: the traversal behind a closure property path.
//!
//! # What this is for
//!
//! `?s :partOf* ?o` asks which pairs are connected by any number of `:partOf` edges. Answering
//! it is a graph traversal, and where it starts decides what it costs. From a *bound* node it
//! is a walk of the edges that exist: the 341-node holarchy in `BENCHMARKS.md` §3 climbs four
//! edges in 0.13 ms. From an *unbound* node every term in the dataset is a candidate start,
//! because a zero-length path connects everything to itself, and the cost becomes proportional
//! to the store — the same benchmark measured 25 seconds for five rows that way, 36,000×
//! slower for 68× fewer rows.
//!
//! So this traverses from a seed set, and [`crate::bindjoin`] only reaches it once one side of
//! the closure is bound. Which side does not matter: [`Direction`] walks the edges forwards or
//! backwards, so a bound object is as good as a bound subject.
//!
//! # Why it is its own module
//!
//! Two callers, one of them not written yet. A closure property path is a fixpoint over one
//! predicate; a recursive rule — Datalog, or SWRL's Horn subset — is a fixpoint over a rule
//! body, and the machinery is the same: a frontier of what is newly known, a visited set of
//! what is known already, and a round that derives only from the frontier. That last part is
//! what makes iteration *semi-naïve* rather than naïve, and it is the difference between
//! re-deriving the whole relation every round and deriving each fact once. Building it here,
//! against the view rather than against `bindjoin`'s internals, is what lets the rule engine
//! reuse it rather than reimplement it.
//!
//! # Every read goes through the view
//!
//! Which means access policy applies to a traversal exactly as it applies to a scan: an edge
//! the principal may not see is an edge the walk does not cross. A traversal that read the
//! store directly would be a way around §14, and there would be no test that noticed.

use holos_core::TermId;
use rustc_hash::FxHashSet;
use spareval::QueryableDataset;

use crate::{DatasetView, ViewError};

/// Which way the edges are followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Seeds are subjects; the walk collects objects. `?s :p* ?o` with `?s` bound.
    Forward,
    /// Seeds are objects; the walk collects subjects. `?s :p* ?o` with `?o` bound, and
    /// what `^:p` means when it is the closure's inner path.
    Backward,
}

impl Direction {
    /// The direction a walk takes, given which side is bound and whether the inner path is
    /// itself inverted. Two inversions cancel.
    #[must_use]
    pub fn of(from_subject: bool, inverse_path: bool) -> Self {
        if from_subject == inverse_path {
            Self::Backward
        } else {
            Self::Forward
        }
    }
}

/// How many edges a path may cross: SPARQL's `*`, `+` and `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hops {
    /// `*` — zero or more. The seed reaches itself.
    ZeroOrMore,
    /// `+` — one or more. The seed reaches itself only around a cycle.
    OneOrMore,
    /// `?` — zero or one. The seed and its immediate neighbours.
    ZeroOrOne,
}

impl Hops {
    /// Whether a seed is an answer for itself, with no edge crossed.
    const fn includes_seed(self) -> bool {
        matches!(self, Self::ZeroOrMore | Self::ZeroOrOne)
    }

    /// Whether the walk continues past the first edge.
    const fn unbounded(self) -> bool {
        matches!(self, Self::ZeroOrMore | Self::OneOrMore)
    }
}

/// Whether a traversal ran to the end or was stopped by its consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// Every reachable node was offered.
    Complete,
    /// The consumer asked to stop — a reachability test that found its answer.
    Stopped,
}

/// Every node reachable from `seeds`, each offered exactly once.
///
/// `emit` is given each answer and returns whether to keep going, so a query that only needs
/// to know *whether* two nodes are connected stops at the first hit rather than walking the
/// component.
///
/// # Distinct, because SPARQL says so
///
/// §9.3 makes an arbitrary-length path a question of *connectivity*: the answer is whether a
/// path exists, not how many there are, so a node reachable by two routes is one solution.
/// The visited set is therefore not an optimisation that happens to also terminate cycles —
/// it is the semantics, and a walk without it would answer a different query and never
/// finish on a cycle.
///
/// # Errors
///
/// Whatever the view raises: a scan that could not be answered, a policy refusal under Fail
/// semantics, or the memory ceiling.
pub fn reachable(
    view: &DatasetView<'_>,
    predicate: TermId,
    direction: Direction,
    hops: Hops,
    graph: Option<Option<&TermId>>,
    seeds: impl IntoIterator<Item = TermId>,
    mut emit: impl FnMut(TermId) -> Result<bool, ViewError>,
) -> Result<Walk, ViewError> {
    // Two sets, not one, and the difference is the semantics of `+`.
    //
    // A seed of `:p+` is not an answer for having crossed no edge — but it *is* an answer if
    // an edge leads back to it, which on a cycle it does: `<f> :p+ ?x` over `f → g → f`
    // answers both `g` and `f`. One set conflating "expanded already" with "emitted already"
    // gets that wrong, and it is wrong in the silent direction: the answer is short by one
    // row and nothing raises. Both tests for it were written before the code passed them.
    let mut emitted: FxHashSet<TermId> = FxHashSet::default();
    let mut expanded: FxHashSet<TermId> = FxHashSet::default();
    let mut frontier: Vec<TermId> = Vec::new();

    for seed in seeds {
        if !expanded.insert(seed) {
            continue;
        }
        frontier.push(seed);
        if hops.includes_seed() && emitted.insert(seed) && !emit(seed)? {
            return Ok(Walk::Stopped);
        }
    }

    // Semi-naïve: each round expands only what the last round newly reached. A round that
    // expanded everything visited so far would re-read every edge it had already crossed,
    // which on a long chain is the difference between linear and quadratic — and it is the
    // same reason a rule engine keeps a frontier rather than re-deriving its whole relation.
    let mut next: Vec<TermId> = Vec::new();
    while !frontier.is_empty() {
        for node in frontier.drain(..) {
            let (subject, object) = match direction {
                Direction::Forward => (Some(node), None),
                Direction::Backward => (None, Some(node)),
            };
            for quad in view.internal_quads_for_pattern(
                subject.as_ref(),
                Some(&predicate),
                object.as_ref(),
                graph,
            ) {
                let quad = quad?;
                let reached = match direction {
                    Direction::Forward => quad.object,
                    Direction::Backward => quad.subject,
                };
                if emitted.insert(reached) && !emit(reached)? {
                    return Ok(Walk::Stopped);
                }
                // `?` crosses one edge, so what it reached is never expanded.
                if hops.unbounded() && expanded.insert(reached) {
                    next.push(reached);
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next);
    }
    Ok(Walk::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use holos_security::Session;
    use oxrdf::{NamedNode, TermRef};
    use oxrdfio::RdfFormat;

    const EX: &str = "http://example.com/";

    /// A chain a→b→c→d, a branch b→e, and a cycle f→g→f.
    const DATA: &str = r"
        @prefix ex: <http://example.com/> .
        ex:a ex:p ex:b .
        ex:b ex:p ex:c .
        ex:c ex:p ex:d .
        ex:b ex:p ex:e .
        ex:f ex:p ex:g .
        ex:g ex:p ex:f .
    ";

    fn engine() -> Engine {
        let mut e = Engine::new();
        e.bulk_load(DATA.as_bytes(), RdfFormat::Turtle, None).unwrap();
        e
    }

    fn walk(from: &str, direction: Direction, hops: Hops) -> Vec<String> {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let id = |local: &str| {
            let node = NamedNode::new_unchecked(format!("{EX}{local}"));
            e.store().lookup_term(TermRef::from(&node)).unwrap().unwrap()
        };
        let mut out = Vec::new();
        reachable(
            &view,
            id("p"),
            direction,
            hops,
            Some(None),
            [id(from)],
            |reached| {
                out.push(
                    view.decode_term(reached)
                        .unwrap()
                        .map(|t| t.to_string().replace(EX, "ex:").replace(['<', '>'], ""))
                        .unwrap_or_default(),
                );
                Ok(true)
            },
        )
        .unwrap();
        out.sort();
        out
    }

    #[test]
    fn zero_or_more_includes_the_seed_and_everything_downstream() {
        assert_eq!(
            walk("a", Direction::Forward, Hops::ZeroOrMore),
            ["ex:a", "ex:b", "ex:c", "ex:d", "ex:e"]
        );
    }

    #[test]
    fn one_or_more_leaves_the_seed_out() {
        assert_eq!(
            walk("a", Direction::Forward, Hops::OneOrMore),
            ["ex:b", "ex:c", "ex:d", "ex:e"]
        );
    }

    #[test]
    fn zero_or_one_crosses_a_single_edge() {
        assert_eq!(
            walk("a", Direction::Forward, Hops::ZeroOrOne),
            ["ex:a", "ex:b"]
        );
        assert_eq!(
            walk("b", Direction::Forward, Hops::ZeroOrOne),
            ["ex:b", "ex:c", "ex:e"]
        );
    }

    #[test]
    fn backward_climbs_to_the_root() {
        assert_eq!(
            walk("d", Direction::Backward, Hops::OneOrMore),
            ["ex:a", "ex:b", "ex:c"]
        );
    }

    /// A cycle must terminate, and must not emit a node twice.
    #[test]
    fn a_cycle_terminates_and_repeats_nothing() {
        assert_eq!(walk("f", Direction::Forward, Hops::ZeroOrMore), ["ex:f", "ex:g"]);
        // `+` around a cycle does reach the seed again, and that is one solution, not none
        // and not two.
        assert_eq!(walk("f", Direction::Forward, Hops::OneOrMore), ["ex:f", "ex:g"]);
    }

    /// A consumer that stops is not walked past. The chain from `a` has five answers; asking
    /// for one must not visit the rest.
    #[test]
    fn a_consumer_can_stop_the_walk() {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let id = |local: &str| {
            let node = NamedNode::new_unchecked(format!("{EX}{local}"));
            e.store().lookup_term(TermRef::from(&node)).unwrap().unwrap()
        };
        let mut seen = 0;
        let outcome = reachable(
            &view,
            id("p"),
            Direction::Forward,
            Hops::ZeroOrMore,
            Some(None),
            [id("a")],
            |_| {
                seen += 1;
                Ok(false)
            },
        )
        .unwrap();
        assert_eq!(outcome, Walk::Stopped);
        assert_eq!(seen, 1, "the walk continued after being told to stop");
    }

    /// Two seeds share one visited set, so a node both reach is emitted once.
    #[test]
    fn seeds_share_the_visited_set() {
        let e = engine();
        let session = Session::unrestricted(e.store()).unwrap();
        let view = e.view(&session);
        let id = |local: &str| {
            let node = NamedNode::new_unchecked(format!("{EX}{local}"));
            e.store().lookup_term(TermRef::from(&node)).unwrap().unwrap()
        };
        let mut out = Vec::new();
        reachable(
            &view,
            id("p"),
            Direction::Forward,
            Hops::OneOrMore,
            Some(None),
            [id("a"), id("b")],
            |reached| {
                out.push(reached);
                Ok(true)
            },
        )
        .unwrap();
        let distinct: FxHashSet<TermId> = out.iter().copied().collect();
        assert_eq!(out.len(), distinct.len(), "a node was emitted twice");
        // c, d, e from either seed, and b from a.
        assert_eq!(out.len(), 4);
    }
}
