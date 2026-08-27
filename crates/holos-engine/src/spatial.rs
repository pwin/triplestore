//! An R-tree over the geometry literals in a store.
//!
//! §17 says the missing piece for geospatial scale is "an R-tree over geometry literals", and
//! that it was blocked on a planner that could route to it. That planner now exists (§7's
//! statistics and the algebra rewrite in [`crate::topology`]), so this is the index.
//!
//! # What it is for
//!
//! A topology relation evaluates a `geof:` function over whatever bindings reach it. Between
//! a variable and a constant region — *find everything inside this polygon*, the archetypal
//! spatial query — that means parsing and testing every geometry in the store.
//!
//! An R-tree turns that into the standard **filter-and-refine** shape: probe the tree with
//! the constant's bounding box, get back the geometries whose boxes overlap, and run the
//! exact predicate only on those. The complexity changes rather than the constant factor.
//!
//! # Refinement is not optional
//!
//! A bounding box says a geometry *may* satisfy the relation, never that it does. Two boxes
//! overlap constantly for shapes that share no point at all. **The exact predicate still runs
//! on every candidate** — the index only decides which candidates are worth testing. Skipping
//! that step is how a spatial index ends up fast and wrong.
//!
//! # Which relations can use it
//!
//! Every relation whose truth requires the two geometries to share a point, or one to contain
//! the other, implies their bounding boxes overlap. Those can be filtered by the tree.
//!
//! **Disjointness cannot.** `sfDisjoint` is true of everything *outside* a region, and
//! bounding-box overlap tells you nothing about it — restricting to overlapping boxes would
//! discard almost every correct answer. [`can_filter`] is where that lives, and it is a
//! correctness boundary rather than an optimisation one.
//!
//! # Policy
//!
//! This index answers questions, which makes it a second place §14's guarantee could be
//! broken. It stores only term ids and bounding boxes, and a candidate is a *proposal* — the
//! quads that mention it are still read through the policy-filtered view, so a principal
//! learns nothing about a geometry it may not see. The index is deliberately not consulted
//! for anything but narrowing a scan that then happens normally.

use crate::geo_ext;
use geo::{BoundingRect, Geometry, Rect};
use holos_core::TermId;
use holos_store::{GraphFilter, Store};
use rstar::{RTree, RTreeObject, AABB};

/// One indexed geometry: its bounding box and the term it belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Indexed {
    /// The geometry literal's term id.
    pub term: TermId,
    lower: [f64; 2],
    upper: [f64; 2],
}

impl RTreeObject for Indexed {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(self.lower, self.upper)
    }
}

/// An R-tree over every geometry literal in a store.
#[derive(Debug, Default)]
pub struct SpatialIndex {
    tree: RTree<Indexed>,
    /// What the store looked like when this was built.
    ///
    /// A spatial index can only ever *narrow* a scan, so the way it goes wrong is by
    /// omitting a geometry that was added after it was built — and an omission is a missing
    /// row, which nothing notices. Rather than rely on every caller refreshing it, the index
    /// carries the shape of the store it came from and [`SpatialIndex::is_current_for`]
    /// checks it. A mismatch means the index is not used and the query does the full scan:
    /// slower, and right.
    built_from: StoreShape,
}

/// A cheap description of a store's contents, for staleness detection.
///
/// Not a hash of the data — that would cost a full scan to check, which is the thing being
/// avoided. Quad count and dictionary size together move on any insert or delete of anything
/// new, which covers every way a geometry can enter or leave. A delete-then-insert that
/// restored both counts *and* changed a geometry would defeat it; that is why the server
/// rebuilds after every write rather than relying on this, and why this is the second line of
/// defence rather than the first.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StoreShape {
    quads: usize,
    terms: usize,
}

impl StoreShape {
    fn of(store: &Store) -> Self {
        Self {
            quads: store.len(),
            terms: store.dictionary_len(),
        }
    }
}

impl SpatialIndex {
    /// Builds the index by scanning the store for geometry literals.
    ///
    /// Every quad is examined, and an object that parses as a `geo:wktLiteral` or
    /// `geo:geoJSONLiteral` is indexed by its bounding box. Detection is by **datatype**
    /// rather than by predicate: a geometry attached with an application's own property is
    /// still a geometry, and keying on `geo:asWKT` would silently miss it.
    ///
    /// Bulk-loaded rather than inserted one at a time — `rstar` packs a far better tree that
    /// way, and the whole set is known here.
    ///
    /// # Errors
    ///
    /// Propagates storage failures. A term that does not parse as a geometry is skipped, not
    /// an error: a store may hold a malformed literal, and refusing to build the index over
    /// the whole store because of one is worse than indexing the rest.
    pub fn build(store: &Store) -> Result<Self, holos_store::StorageError> {
        // A term appears as the object of many quads; indexing it once is both correct and
        // very much cheaper on data where geometries are shared.
        let mut seen = rustc_hash::FxHashSet::default();
        let mut entries = Vec::new();
        for encoded in store.quads_for_pattern(None, None, None, GraphFilter::Any) {
            let object = encoded?.object;
            if !seen.insert(object) {
                continue;
            }
            let Some(term) = store.decode_term(object)? else {
                continue;
            };
            let Some(geometry) = geo_ext::geometry_of(&term) else {
                continue;
            };
            if let Some(entry) = index_entry(object, &geometry) {
                entries.push(entry);
            }
        }
        Ok(Self {
            tree: RTree::bulk_load(entries),
            built_from: StoreShape::of(store),
        })
    }

    /// Whether this index still describes `store`.
    ///
    /// **Routing must check this.** An index built before a write is missing whatever the
    /// write added, and using it would drop rows silently. Returning `false` costs a full
    /// scan; returning `true` wrongly costs a wrong answer.
    #[must_use]
    pub fn is_current_for(&self, store: &Store) -> bool {
        self.built_from == StoreShape::of(store)
    }

    /// How many geometries are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tree.size()
    }

    /// Whether the index holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tree.size() == 0
    }

    /// Every indexed geometry whose bounding box overlaps `probe`'s.
    ///
    /// A **superset** of the geometries that can satisfy a box-filterable relation with
    /// `probe`. The caller must still evaluate the exact predicate on each — see the module
    /// documentation.
    #[must_use]
    pub fn candidates(&self, probe: &Geometry) -> Vec<TermId> {
        let Some(rect) = probe.bounding_rect() else {
            return Vec::new();
        };
        self.candidates_in(&rect)
    }

    /// Every indexed geometry whose bounding box overlaps `rect`.
    #[must_use]
    pub fn candidates_in(&self, rect: &Rect) -> Vec<TermId> {
        let envelope =
            AABB::from_corners([rect.min().x, rect.min().y], [rect.max().x, rect.max().y]);
        self.tree
            .locate_in_envelope_intersecting(&envelope)
            .map(|indexed| indexed.term)
            .collect()
    }
}

/// The index entry for a geometry, if it has a bounding box.
///
/// An empty geometry has none, and is not indexed: it cannot overlap anything, so no probe
/// should ever return it.
fn index_entry(term: TermId, geometry: &Geometry) -> Option<Indexed> {
    let rect = geometry.bounding_rect()?;
    // NaN would make the tree's comparisons meaningless and is not a coordinate.
    let (min, max) = (rect.min(), rect.max());
    if [min.x, min.y, max.x, max.y].iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some(Indexed {
        term,
        lower: [min.x, min.y],
        upper: [max.x, max.y],
    })
}

/// Whether a topology relation can be narrowed by bounding boxes.
///
/// True when the relation holding implies the two bounding boxes overlap — which is every
/// relation that requires a shared point or containment.
///
/// **False for disjointness**, in all three families. Disjoint geometries usually have
/// *non*-overlapping boxes, so filtering to overlapping ones would discard nearly every
/// correct answer. This is a correctness boundary, not a performance one: routing
/// `sfDisjoint` through the index would return wrong results, quickly.
#[must_use]
pub fn can_filter(relation: &str) -> bool {
    !matches!(relation, "sfDisjoint" | "ehDisjoint" | "rcc8dc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjointness_is_never_routed_through_the_index() {
        // The one relation whose answers lie mostly *outside* any probe box. All three
        // families spell it differently and all three must be excluded.
        for relation in ["sfDisjoint", "ehDisjoint", "rcc8dc"] {
            assert!(!can_filter(relation), "{relation} must not be box-filtered");
        }
    }

    #[test]
    fn relations_requiring_shared_points_are_routed() {
        for relation in [
            "sfContains",
            "sfWithin",
            "sfIntersects",
            "sfOverlaps",
            "sfTouches",
            "sfCrosses",
            "sfEquals",
            "ehContains",
            "ehMeet",
            "rcc8tpp",
            "rcc8ntpp",
            "rcc8eq",
        ] {
            assert!(can_filter(relation), "{relation} should be box-filtered");
        }
    }

    #[test]
    fn an_empty_index_returns_no_candidates() {
        let index = SpatialIndex::default();
        assert!(index.is_empty());
        let probe: Geometry = geo::Point::new(0.0, 0.0).into();
        assert!(index.candidates(&probe).is_empty());
    }
}
