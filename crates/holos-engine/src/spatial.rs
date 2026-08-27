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
use holos_core::Tag;
use holos_core::TermId;
use holos_store::Store;
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
///
/// # Why the contents sit behind a lock
///
/// The index is shared as an `Arc` between the query path and the write path, and it is now
/// *updated* rather than replaced. Interior mutability keeps that from requiring a new `Arc`
/// on every write — and the critical sections are short, because [`SpatialIndex::candidates`]
/// is called once while a query is being rewritten, not held across its evaluation.
#[derive(Debug, Default)]
pub struct SpatialIndex {
    inner: std::sync::RwLock<Inner>,
}

/// What the index holds, and how far through the dictionary it has read.
#[derive(Debug, Default)]
struct Inner {
    tree: RTree<Indexed>,
    /// How many literal ids had been issued when this index last caught up.
    ///
    /// The dictionary gives each kind its own dense, append-only index space, so every
    /// literal ever interned is `TermId::new(Tag::Literal, i)` for some `i` below the
    /// current count. Remembering how far it has read is all the index needs to find
    /// everything added since — no scan of the store, and no set of terms already examined.
    watermark: usize,
    /// Geometries a purge removed, which are still interned.
    ///
    /// A purge cannot simply forget them. Re-inserting a quad over an already-interned
    /// literal does not intern anything, so the literal count does not move and the walk
    /// above would never revisit it — the geometry would be back in the store and absent
    /// from the index, which is a silently missing answer.
    ///
    /// So what a purge really does is convert an index entry into a *watchlist* entry: a
    /// bare term id rather than a bounding box in a tree, checked on refresh for whether it
    /// has been referenced again.
    dormant: rustc_hash::FxHashSet<TermId>,
    /// The store's quad count when the watchlist was last checked.
    ///
    /// Deletions cannot resurrect anything, so the check is skipped unless the store has
    /// grown.
    quads_at_last_check: usize,
    /// Entries inserted one at a time since the last bulk load.
    ///
    /// `rstar` packs a far better tree from a bulk load than from repeated inserts, so a
    /// long run of incremental updates slowly degrades lookup. Past a threshold the index
    /// rebuilds from what it already holds — no re-parsing, just a repack.
    inserted_since_pack: usize,
}

/// How many one-at-a-time inserts are tolerated before the tree is repacked.
///
/// Repacking re-runs `bulk_load` over entries the index already has, so it costs the tree
/// construction and none of the parsing.
const REPACK_AFTER: usize = 10_000;

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
        let index = Self::default();
        index.catch_up(store, 0)?;
        Ok(index)
    }

    /// Brings the index up to date with `store`.
    ///
    /// # What it costs
    ///
    /// Proportional to the literals interned since the last call, and nothing else. There is
    /// no scan of the store: the dictionary hands out literal ids densely and never reuses
    /// one, so everything new sits above the watermark.
    ///
    /// # Why it never removes anything
    ///
    /// The index is a **superset filter**. `candidates` proposes, the exact predicate
    /// decides, and the `VALUES` clause built from its answers is joined back against the
    /// store — so a geometry whose quads have been deleted simply fails to join and
    /// contributes no row.
    ///
    /// That asymmetry is the whole design. **Omitting a geometry is a silently missing
    /// answer; keeping one that has left is invisible.** So the index tracks the
    /// *dictionary* rather than the store, and the dictionary is append-only by
    /// construction. What it holds is bounded by how many distinct geometries have ever been
    /// interned, not by how many are currently reachable.
    ///
    /// # Errors
    ///
    /// Propagates decoding failures from the store.
    pub fn refresh(&self, store: &Store) -> Result<(), holos_store::StorageError> {
        let from = self.inner.read().map_or(0, |inner| inner.watermark);
        self.catch_up(store, from)
    }

    /// Reads literal ids from `from` upwards, indexing every geometry among them.
    fn catch_up(&self, store: &Store, from: usize) -> Result<(), holos_store::StorageError> {
        let count = store.dictionary_count_for(Tag::Literal);
        let quads = store.len();

        // Nothing interned *and* nothing on the watchlist to re-check: already level. Both
        // halves matter — a geometry can come back without anything being interned, which is
        // exactly what the watchlist exists for, so returning on the literal count alone
        // would skip the check that catches it.
        {
            let Ok(inner) = self.inner.read() else {
                return Ok(());
            };
            if count <= from && inner.quads_at_last_check == quads {
                return Ok(());
            }
        }

        // Decoded and parsed before the write lock is taken: that is the expensive part and
        // there is no reason to hold readers off during it.
        let mut fresh = Vec::new();
        for i in from..count {
            let id = TermId::new(Tag::Literal, i as u64);
            let Some(term) = store.decode_term(id)? else {
                continue;
            };
            let Some(geometry) = geo_ext::geometry_of(&term) else {
                continue;
            };
            if let Some(entry) = index_entry(id, &geometry) {
                fresh.push(entry);
            }
        }

        let resurrected = self.resurrect(store)?;

        let Ok(mut inner) = self.inner.write() else {
            return Ok(());
        };
        // Another thread may have caught up further while this one was parsing; taking the
        // larger watermark keeps the invariant "everything below the watermark is indexed",
        // and the duplicate inserts that implies are harmless to a superset filter.
        if inner.tree.size() == 0 && inner.watermark == 0 && resurrected.is_empty() {
            inner.tree = RTree::bulk_load(fresh);
        } else {
            for entry in fresh {
                inner.tree.insert(entry);
                inner.inserted_since_pack += 1;
            }
        }
        for entry in resurrected {
            inner.dormant.remove(&entry.term);
            inner.tree.insert(entry);
            inner.inserted_since_pack += 1;
        }
        inner.quads_at_last_check = quads;
        inner.watermark = inner.watermark.max(count);
        if inner.inserted_since_pack >= REPACK_AFTER {
            // Repacking uses what is already in the tree, so no parsing is repeated.
            let entries: Vec<Indexed> = inner.tree.iter().copied().collect();
            inner.tree = RTree::bulk_load(entries);
            inner.inserted_since_pack = 0;
        }
        Ok(())
    }

    /// Watchlist entries that have been referenced again.
    ///
    /// Skipped entirely when the store has not grown, because a deletion cannot bring a
    /// geometry back. When it does run it costs one index probe per dormant entry, which is
    /// the standing price of having purged.
    fn resurrect(&self, store: &Store) -> Result<Vec<Indexed>, holos_store::StorageError> {
        let dormant: Vec<TermId> = {
            let Ok(inner) = self.inner.read() else {
                return Ok(Vec::new());
            };
            if inner.dormant.is_empty() || store.len() <= inner.quads_at_last_check {
                return Ok(Vec::new());
            }
            inner.dormant.iter().copied().collect()
        };

        let mut back = Vec::new();
        for term in dormant {
            if !referenced(store, term)? {
                continue;
            }
            let Some(decoded) = store.decode_term(term)? else {
                continue;
            };
            let Some(geometry) = geo_ext::geometry_of(&decoded) else {
                continue;
            };
            if let Some(entry) = index_entry(term, &geometry) {
                back.push(entry);
            }
        }
        Ok(back)
    }

    /// Drops indexed geometries that no quad refers to any more.
    ///
    /// # What this is for
    ///
    /// The index tracks the dictionary, and the dictionary never forgets — so a store that
    /// deletes geometries accumulates entries for them. Nothing is *wrong* while it does:
    /// the index is a superset filter and a departed geometry fails to join. But the entries
    /// cost memory in proportion to everything ever interned, and this is how that is
    /// reclaimed.
    ///
    /// # What it costs, afterwards
    ///
    /// Purging is not free forever. A dropped geometry becomes a **watchlist** entry rather
    /// than being forgotten, because re-inserting a quad over an already-interned literal
    /// interns nothing — so the dictionary walk that finds everything else would never
    /// revisit it. Each refresh after a purge therefore probes once per watchlist entry,
    /// though only when the store has actually grown.
    ///
    /// The trade is still strongly favourable: a watchlist entry is a bare term id, against
    /// a bounding box inside a tree.
    ///
    /// # Errors
    ///
    /// Propagates storage failures from the reference checks.
    pub fn purge(&self, store: &Store) -> Result<PurgeReport, holos_store::StorageError> {
        let entries: Vec<Indexed> = {
            let Ok(inner) = self.inner.read() else {
                return Ok(PurgeReport::default());
            };
            inner.tree.iter().copied().collect()
        };
        let examined = entries.len();

        let mut kept = Vec::with_capacity(examined);
        let mut dropped = Vec::new();
        for entry in entries {
            if referenced(store, entry.term)? {
                kept.push(entry);
            } else {
                dropped.push(entry.term);
            }
        }

        let Ok(mut inner) = self.inner.write() else {
            return Ok(PurgeReport::default());
        };
        let report = PurgeReport {
            examined,
            dropped: dropped.len(),
            retained: kept.len(),
        };
        inner.tree = RTree::bulk_load(kept);
        inner.inserted_since_pack = 0;
        inner.dormant.extend(dropped);
        inner.quads_at_last_check = store.len();
        Ok(report)
    }

    /// Whether this index still describes `store`.
    ///
    /// **Routing must check this.** An index that has not caught up is missing whatever was
    /// interned since, and using it would drop rows silently. Returning `false` costs a full
    /// scan; returning `true` wrongly costs a wrong answer.
    ///
    /// The check is exact rather than a heuristic: the index holds a geometry for every
    /// literal id below its watermark, and every geometry in the store is a literal in the
    /// dictionary. So a watermark level with the dictionary means nothing can be missing —
    /// including after a write that only added quads over geometries already interned, which
    /// a quad-count comparison would have called stale for no reason.
    ///
    /// After a purge the watermark alone is no longer sufficient, because a watchlist entry
    /// can be referenced again without anything being interned. While the watchlist is
    /// non-empty the store's quad count has to match what it was when the list was last
    /// checked — which is the heuristic this design otherwise avoids, confined to the one
    /// case that needs it.
    #[must_use]
    pub fn is_current_for(&self, store: &Store) -> bool {
        self.inner.read().is_ok_and(|inner| {
            inner.watermark >= store.dictionary_count_for(Tag::Literal)
                && (inner.dormant.is_empty() || inner.quads_at_last_check == store.len())
        })
    }

    /// How many geometries are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().map_or(0, |inner| inner.tree.size())
    }

    /// Whether the index holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
        let Ok(inner) = self.inner.read() else {
            // A poisoned lock costs the narrowing, not the answer: an empty candidate set
            // would be an omission, so the caller must treat this as "no index" instead.
            return Vec::new();
        };
        inner
            .tree
            .locate_in_envelope_intersecting(&envelope)
            .map(|indexed| indexed.term)
            .collect()
    }
}

/// What a purge did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PurgeReport {
    /// Geometries the index held before the purge.
    pub examined: usize,
    /// Of those, how many no quad refers to any more.
    pub dropped: usize,
    /// How many remain indexed.
    pub retained: usize,
}

/// Whether any quad, in any graph, has `term` as its object.
///
/// One probe of the object-first index rather than a scan — which is what makes a purge
/// proportional to what is indexed rather than to the store.
fn referenced(store: &Store, term: TermId) -> Result<bool, holos_store::StorageError> {
    for quad in store.quads_for_pattern(None, None, Some(term), holos_store::GraphFilter::Any) {
        quad?;
        return Ok(true);
    }
    Ok(false)
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
