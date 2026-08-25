//! Revalidating only what a change could have affected.
//!
//! This is the addition `DESIGN.md` §8 makes beyond porting a validator, and it is what
//! makes SHACL affordable on the write path. Without it, every commit costs a full
//! validation and the holon Boundary (§9) is unaffordable; with it, the cost tracks the
//! size of the change.
//!
//! # How the affected set is derived
//!
//! The shape compiler already built the index: predicate → shapes whose result could
//! change. For a delta, three things can make a node need rechecking.
//!
//! 1. **The node is a subject of a changed quad**, and some shape reads that predicate.
//! 2. **The node is an object of a changed quad**, because `sh:targetObjectsOf` and
//!    inverse paths look upstream.
//! 3. **The node's type changed**, so shapes targeting that class now apply — or no
//!    longer do, which is why a *removed* `rdf:type` matters as much as an added one.
//!
//! # What it does not do
//!
//! It is an over-approximation, deliberately. A shape reached only through a long
//! `sh:node` chain from a changed node is not tracked, so the affected set is computed
//! from direct predicate use and widened by [`Plan::widen_to_targets`]. Under-reporting
//! would silently let an invalid graph through; over-reporting only costs time.
//! [`tests`] checks the property that matters: the incremental result agrees with a full
//! validation.

use crate::ir::{ShapeIdx, Shapes};
use crate::vocab::Sh;
use holos_store::EncodedQuad;
use rustc_hash::{FxHashMap, FxHashSet};

/// A quad that was added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// The quad.
    pub quad: EncodedQuad,
    /// True for an insert, false for a delete.
    pub added: bool,
}

impl Change {
    /// An inserted quad.
    #[must_use]
    pub fn added(quad: EncodedQuad) -> Self {
        Self { quad, added: true }
    }

    /// A removed quad.
    #[must_use]
    pub fn removed(quad: EncodedQuad) -> Self {
        Self { quad, added: false }
    }
}

/// The work a delta implies: which shapes to check at which focus nodes.
#[derive(Debug, Default, Clone)]
pub struct Plan {
    work: FxHashSet<(ShapeIdx, holos_core::TermId)>,
}

impl Plan {
    /// The (shape, focus node) pairs to evaluate, in a deterministic order.
    #[must_use]
    pub fn work(&self) -> Vec<(ShapeIdx, holos_core::TermId)> {
        let mut out: Vec<_> = self.work.iter().copied().collect();
        out.sort_unstable();
        out
    }

    /// How many checks the plan implies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.work.len()
    }

    /// Whether the change affects nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.work.is_empty()
    }
}

/// Works out what a delta makes stale.
///
/// `type_index` supplies the classes a node belongs to *after* the change, which the
/// caller reads from the store — the planner deliberately does no I/O of its own so it can
/// run inside a commit.
#[must_use]
pub fn plan(
    shapes: &Shapes,
    sh: &Sh,
    changes: &[Change],
    mut classes_of: impl FnMut(holos_core::TermId) -> Vec<holos_core::TermId>,
) -> Plan {
    let mut plan = Plan::default();
    let mut class_cache: FxHashMap<holos_core::TermId, Vec<holos_core::TermId>> =
        FxHashMap::default();

    for change in changes {
        let quad = change.quad;

        // 1 and 2: any shape that reads this predicate may now answer differently, at
        // either end of the quad.
        for &idx in shapes.shapes_touching(quad.predicate) {
            plan.work.insert((idx, quad.subject));
            plan.work.insert((idx, quad.object));
        }

        // 3: a changed rdf:type moves a node in or out of a class target. A *removal*
        // matters as much as an addition — the node may have been valid only because the
        // shape did not apply to it, or may now escape a shape that should still catch it.
        if quad.predicate == sh.rdf_type {
            for &idx in shapes.shapes_targeting_class(quad.object) {
                plan.work.insert((idx, quad.subject));
            }
            // Subclasses of the changed type carry their own shapes.
            for class in class_cache
                .entry(quad.subject)
                .or_insert_with(|| classes_of(quad.subject))
                .clone()
            {
                for &idx in shapes.shapes_targeting_class(class) {
                    plan.work.insert((idx, quad.subject));
                }
            }
        }
    }
    plan
}

impl Plan {
    /// Adds every shape that targets a node explicitly.
    ///
    /// `sh:targetNode` names a focus node outright, so a change anywhere in that node's
    /// neighbourhood has to re-run it even if no predicate in the index matched.
    pub fn widen_to_targets(&mut self, shapes: &Shapes, nodes: &[holos_core::TermId]) {
        for (i, shape) in shapes.all().iter().enumerate() {
            let idx = ShapeIdx(u32::try_from(i).unwrap_or(u32::MAX));
            for target in &shape.targets {
                if let crate::ir::Target::Node(n) = target {
                    if nodes.contains(n) {
                        self.work.insert((idx, *n));
                    }
                }
            }
        }
    }

    /// Restricts the plan to shapes that actually have targets.
    ///
    /// A shape reached only by reference is validated as part of its parent, never on its
    /// own, so evaluating it standalone would report violations SHACL does not.
    pub fn restrict_to_targeted(&mut self, shapes: &Shapes) {
        let targeted: FxHashSet<ShapeIdx> = shapes.targeted().iter().copied().collect();
        self.work.retain(|(idx, _)| targeted.contains(idx));
    }

    /// Drops focus nodes a shape's targets do not actually select.
    ///
    /// The predicate index is an over-approximation; this is where the surplus is trimmed,
    /// once, against the shape's real target definition.
    pub fn restrict_to_focus(
        &mut self,
        mut selects: impl FnMut(ShapeIdx, holos_core::TermId) -> bool,
    ) {
        self.work.retain(|(idx, focus)| selects(*idx, *focus));
    }
}
