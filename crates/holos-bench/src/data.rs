//! The synthetic dataset, and why it is shaped the way it is.
//!
//! A benchmark dataset is an argument about what matters, so this one is built around the
//! three things the query battery needs to exercise:
//!
//! 1. **A holarchy.** Units nest inside units, to a fixed depth. This is what makes
//!    property paths meaningful rather than decorative: `ex:partOf+` walking a real tree is
//!    the query a holon model actually asks, because a holon *is* a whole that is also a
//!    part. A flat dataset would make every path query one hop and prove nothing.
//!
//! 2. **Skew.** Predicate frequencies differ by three orders of magnitude, and the
//!    `knows` graph is deliberately uneven — a few people know hundreds, most know a
//!    handful. Uniform data flatters a cardinality estimator; real RDF does not, and the
//!    whole point of measuring is to find where planning hurts.
//!
//! 3. **A shape worth validating.** Every person carries the properties a SHACL boundary
//!    can check, so the holon measurements validate something real rather than an empty
//!    constraint that always passes.
//!
//! # Comparability across scales
//!
//! Everything is derived from the row index with a fixed-seed hash rather than an RNG, so
//! a person's *own* properties — name, age, unit membership, badge — are identical whether
//! the run generated ten people or ten million. Query timings at different scales are
//! therefore asking the same question of the same entity.
//!
//! Edges are the exception, and necessarily so: a `knows` edge has to point at somebody who
//! exists, so its target depends on how many people there are. The neighbourhood of a given
//! person is not stable across scales, only its *size* is. That is the honest limit of the
//! comparison, and it is why the path queries are anchored to a person's degree class
//! rather than to a particular set of neighbours.

use oxrdf::vocab::{rdf, xsd};
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};

/// Namespace for everything generated here.
pub const EX: &str = "http://holos.example/";

/// How deep the unit holarchy goes. Five levels is enough that a transitive path has work
/// to do and shallow enough that the answer stays checkable by hand.
pub const DEPTH: usize = 5;

/// Branching factor of the holarchy.
pub const FANOUT: usize = 4;

#[must_use]
pub fn ex(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{local}"))
}

#[must_use]
pub fn person(i: usize) -> NamedNode {
    ex(&format!("person{i}"))
}

#[must_use]
pub fn unit(i: usize) -> NamedNode {
    ex(&format!("unit{i}"))
}

/// Number of units in a holarchy of [`DEPTH`] levels with [`FANOUT`] branching.
#[must_use]
pub fn unit_count() -> usize {
    let mut total = 0;
    let mut level = 1;
    for _ in 0..DEPTH {
        total += level;
        level *= FANOUT;
    }
    total
}

/// The parent of a unit in the holarchy, or `None` for the root.
#[must_use]
pub fn parent_of(i: usize) -> Option<usize> {
    if i == 0 {
        None
    } else {
        Some((i - 1) / FANOUT)
    }
}

/// A deterministic pseudo-random value in `0..range`, seeded only by `i` and `salt`.
///
/// A hash rather than an RNG, so the same `(i, salt)` always gives the same draw and a run
/// is reproducible without carrying a seed around. Note that the *result* still depends on
/// `range`, which is why edges are not stable across scales — see the module note.
fn scatter(i: usize, salt: u64, range: usize) -> usize {
    if range == 0 {
        return 0;
    }
    let mut x =
        (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x % range as u64) as usize
}

/// How many people `i` knows: heavy tail, so a few hubs dominate.
///
/// One person in a thousand knows 200; one in fifty knows 20; everyone else knows 3. That
/// spread is what makes a `knows+` path query behave differently depending on where it
/// starts, which is exactly the behaviour worth timing.
fn degree(i: usize) -> usize {
    if i % 1000 == 0 {
        200
    } else if i % 50 == 0 {
        20
    } else {
        3
    }
}

/// Writes the dataset for `people` people to a sink.
///
/// The default graph holds everything; the holon measurements move their own slice into a
/// scene graph afterwards, which is the realistic pattern — data arrives flat and a holon
/// takes responsibility for part of it.
pub fn generate(people: usize, mut emit: impl FnMut(Quad)) {
    let units = unit_count();
    let graph = GraphName::DefaultGraph;

    // --- the holarchy -----------------------------------------------------
    for i in 0..units {
        emit(Quad {
            subject: unit(i).into(),
            predicate: rdf::TYPE.into_owned(),
            object: Term::NamedNode(ex("Unit")),
            graph_name: graph.clone(),
        });
        emit(Quad {
            subject: unit(i).into(),
            predicate: ex("unitName"),
            object: Literal::new_simple_literal(format!("Unit {i}")).into(),
            graph_name: graph.clone(),
        });
        if let Some(parent) = parent_of(i) {
            emit(Quad {
                subject: unit(i).into(),
                predicate: ex("partOf"),
                object: Term::NamedNode(unit(parent)),
                graph_name: graph.clone(),
            });
        }
    }

    // --- people -----------------------------------------------------------
    for i in 0..people {
        let s = person(i);
        emit(Quad {
            subject: s.clone().into(),
            predicate: rdf::TYPE.into_owned(),
            object: Term::NamedNode(ex("Person")),
            graph_name: graph.clone(),
        });
        emit(Quad {
            subject: s.clone().into(),
            predicate: ex("name"),
            object: Literal::new_simple_literal(format!("Person {i}")).into(),
            graph_name: graph.clone(),
        });
        emit(Quad {
            subject: s.clone().into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal((20 + i % 50).to_string(), xsd::INTEGER).into(),
            graph_name: graph.clone(),
        });
        // Members are attached to leaf units, so `memberOf/partOf*` has the full depth to
        // climb rather than landing near the root.
        let leaf_start = unit_count() - FANOUT.pow(DEPTH as u32 - 1);
        let leaf = leaf_start + scatter(i, 1, units - leaf_start);
        emit(Quad {
            subject: s.clone().into(),
            predicate: ex("memberOf"),
            object: Term::NamedNode(unit(leaf)),
            graph_name: graph.clone(),
        });

        // A rare predicate: one person in five hundred. This is the one a constant-table
        // estimator gets most wrong, and the one a good plan should start from.
        if i % 500 == 0 {
            emit(Quad {
                subject: s.clone().into(),
                predicate: ex("badgeNumber"),
                object: Literal::new_simple_literal(format!("B{i:07}")).into(),
                graph_name: graph.clone(),
            });
        }

        for k in 0..degree(i).min(people.saturating_sub(1).max(1)) {
            let other = scatter(i, 2 + k as u64, people);
            if other != i {
                emit(Quad {
                    subject: s.clone().into(),
                    predicate: ex("knows"),
                    object: Term::NamedNode(person(other)),
                    graph_name: graph.clone(),
                });
            }
        }
    }
}

/// Serialises the dataset as N-Triples into a writer.
///
/// Writing a file rather than inserting directly is deliberate: every load timing in the
/// report then includes parsing, which is what a real load does and what makes the numbers
/// comparable with any other store's published figures.
pub fn write_ntriples(people: usize, out: &mut impl std::io::Write) -> std::io::Result<u64> {
    let mut n = 0_u64;
    let mut err = None;
    generate(people, |quad| {
        if err.is_some() {
            return;
        }
        if let Err(e) = writeln!(out, "{} {} {} .", quad.subject, quad.predicate, quad.object) {
            err = Some(e);
        } else {
            n += 1;
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(n),
    }
}

/// A SHACL boundary for the holon measurements.
///
/// Deliberately non-trivial: four constraints across two property shapes, so revalidating
/// a delta has real work to do rather than short-circuiting on an empty shape.
pub const BOUNDARY_SHAPES: &str = r#"
@prefix ex:   <http://holos.example/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ;
                  sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age ;
                  sh:maxCount 1 ; sh:datatype xsd:integer ;
                  sh:minInclusive 0 ; sh:maxInclusive 150 ] .
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_holarchy_is_a_tree() {
        let units = unit_count();
        assert_eq!(units, 1 + 4 + 16 + 64 + 256);
        assert_eq!(parent_of(0), None, "unit0 is the root");
        for i in 1..units {
            let parent = parent_of(i).expect("non-root has a parent");
            assert!(
                parent < i,
                "parents come before children, so the tree is acyclic"
            );
        }
    }

    #[test]
    fn every_unit_reaches_the_root() {
        for i in 0..unit_count() {
            let mut hops = 0;
            let mut at = i;
            while let Some(parent) = parent_of(at) {
                at = parent;
                hops += 1;
                assert!(
                    hops <= DEPTH,
                    "unit{i} did not reach the root in {DEPTH} hops"
                );
            }
            assert_eq!(at, 0);
        }
    }

    #[test]
    fn a_persons_own_properties_do_not_depend_on_the_scale() {
        // What makes timings comparable across scales: person 7 has the same name, age,
        // unit and badge whether the run generated ten people or ten thousand. Edges are
        // excluded because a `knows` target must exist, so it cannot be scale-free.
        let take = |people: usize| {
            let mut out = Vec::new();
            generate(people, |q| {
                let subject = q.subject.to_string();
                let predicate = q.predicate.to_string();
                if subject.contains("person7>") && !predicate.contains("knows") {
                    out.push(q.to_string());
                }
            });
            out.sort();
            out
        };
        assert_eq!(take(10), take(10_000));
    }

    #[test]
    fn degree_classes_are_stable_across_scales() {
        // The neighbourhood changes with scale; its size must not, or a path query would
        // be doing different amounts of work at each size for reasons unrelated to size.
        let degree_of = |i: usize, people: usize| {
            let mut n = 0;
            generate(people, |q| {
                if q.subject.to_string().contains(&format!("person{i}>"))
                    && q.predicate.to_string().contains("knows")
                {
                    n += 1;
                }
            });
            n
        };
        // person1000 is a hub, person7 is not.
        assert_eq!(degree_of(1000, 5_000), degree_of(1000, 20_000));
        assert_eq!(degree_of(7, 5_000), degree_of(7, 20_000));
        assert!(degree_of(1000, 5_000) > degree_of(7, 5_000));
    }

    #[test]
    fn quad_count_is_predictable() {
        let mut n = 0;
        generate(1000, |_| n += 1);
        // Units contribute a fixed amount; people contribute their own quads plus edges.
        assert!(n > 1000 * 4, "got {n}");
    }
}
