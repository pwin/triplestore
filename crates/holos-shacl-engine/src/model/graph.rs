//! An indexed RDF graph.
//!
//! Triples are held in three fully-sorted permutations rather than in hash maps
//! of adjacency lists. Every lookup SHACL needs is a prefix range on one of
//! them, resolved with two binary searches and then walked as a contiguous
//! slice — which keeps path evaluation, the engine's hottest loop, free of both
//! pointer chasing and allocation.
//!
//! # Taking a delta
//!
//! The graph used to be immutable, and `DESIGN.md` §8 recorded what that cost: a
//! validator whose graph cannot change has to be handed a new one for every
//! commit, so the adapted engine could not gate a write path however good its
//! constraint coverage was. Measured at 250,000 quads, rebuilding cost 198 ms
//! against 0.4 µs for a validator that can be told what changed.
//!
//! [`Graph::apply`] closes that. The permutations stay sorted, so a delta is a
//! binary search and a memmove per row per index — and a *large* delta is not,
//! which is why it switches to a rebuild past a threshold rather than pretending
//! one strategy suits both a two-triple tick and a bulk load.

use super::term::TermId;

/// A triple of interned terms, stored in whatever component order its index
/// implies.
type Row = [TermId; 3];

/// Accumulates triples before they are sorted into a [`Graph`].
#[derive(Debug, Default)]
pub struct GraphBuilder {
    rows: Vec<Row>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, s: TermId, p: TermId, o: TermId) {
        self.rows.push([s, p, o]);
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Sorts and deduplicates into the three index permutations.
    pub fn build(mut self) -> Graph {
        self.rows.sort_unstable();
        self.rows.dedup();

        let spo = self.rows;
        let mut pos: Vec<Row> = spo.iter().map(|&[s, p, o]| [p, o, s]).collect();
        pos.sort_unstable();
        let mut osp: Vec<Row> = spo.iter().map(|&[s, p, o]| [o, s, p]).collect();
        osp.sort_unstable();

        Graph { spo, pos, osp }
    }
}

/// An RDF graph indexed for the access patterns SHACL uses.
///
/// `Clone` copies all three permutations — three words per triple — rather
/// than rebuilding and re-sorting them, which is what the rules engine wants
/// when it hands back a graph nothing was added to.
#[derive(Debug, Default, Clone)]
pub struct Graph {
    /// Sorted by `(subject, predicate, object)`.
    spo: Vec<Row>,
    /// Sorted by `(predicate, object, subject)`.
    pos: Vec<Row>,
    /// Sorted by `(object, subject, predicate)`.
    osp: Vec<Row>,
}

impl Graph {
    pub fn len(&self) -> usize {
        self.spo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spo.is_empty()
    }

    /// Objects of `(s, p, ?)` — the predicate-path hot path.
    #[inline]
    pub fn objects(&self, s: TermId, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix2(&self.spo, s, p).iter().map(|r| r[2])
    }

    /// The first object of `(s, p, ?)`, for functional properties.
    #[inline]
    pub fn object(&self, s: TermId, p: TermId) -> Option<TermId> {
        prefix2(&self.spo, s, p).first().map(|r| r[2])
    }

    /// Subjects of `(?, p, o)` — inverse paths and `sh:targetSubjectsOf`.
    #[inline]
    pub fn subjects(&self, p: TermId, o: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix2(&self.pos, p, o).iter().map(|r| r[2])
    }

    /// Every subject appearing with predicate `p`, in sorted order with
    /// duplicates retained.
    #[inline]
    pub fn subjects_of(&self, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix1(&self.pos, p).iter().map(|r| r[2])
    }

    /// Every object appearing with predicate `p`.
    #[inline]
    pub fn objects_of(&self, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix1(&self.pos, p).iter().map(|r| r[1])
    }

    /// All `(predicate, object)` pairs of `s` — used by `sh:closed`.
    #[inline]
    pub fn predicate_objects(&self, s: TermId) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        prefix1(&self.spo, s).iter().map(|r| (r[1], r[2]))
    }

    /// All `(subject, predicate)` pairs pointing at `o`.
    #[inline]
    pub fn subject_predicates(&self, o: TermId) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        prefix1(&self.osp, o).iter().map(|r| (r[1], r[2]))
    }

    #[inline]
    pub fn contains(&self, s: TermId, p: TermId, o: TermId) -> bool {
        self.spo.binary_search(&[s, p, o]).is_ok()
    }

    /// True if `s` appears as a subject of any triple.
    #[inline]
    pub fn has_subject(&self, s: TermId) -> bool {
        !prefix1(&self.spo, s).is_empty()
    }

    /// Every triple, in `(s, p, o)` order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = Row> + '_ {
        self.spo.iter().copied()
    }

    /// Adds one triple. `true` if it was not already present.
    ///
    /// Three binary searches and three memmoves. The memmove is the cost and it is
    /// bandwidth-bound rather than comparison-bound, which is why a handful of them beats
    /// re-sorting even a graph of moderate size by a wide margin.
    pub fn insert(&mut self, s: TermId, p: TermId, o: TermId) -> bool {
        let mut added = false;
        for (rows, row) in [
            (&mut self.spo, [s, p, o]),
            (&mut self.pos, [p, o, s]),
            (&mut self.osp, [o, s, p]),
        ] {
            if let Err(at) = rows.binary_search(&row) {
                rows.insert(at, row);
                added = true;
            }
        }
        added
    }

    /// Removes one triple. `true` if it was present.
    pub fn remove(&mut self, s: TermId, p: TermId, o: TermId) -> bool {
        let mut removed = false;
        for (rows, row) in [
            (&mut self.spo, [s, p, o]),
            (&mut self.pos, [p, o, s]),
            (&mut self.osp, [o, s, p]),
        ] {
            if let Ok(at) = rows.binary_search(&row) {
                rows.remove(at);
                removed = true;
            }
        }
        removed
    }

    /// Applies a batch of additions and removals.
    ///
    /// Removals are applied first, so a delta that removes and re-adds the same triple ends
    /// with it present — which is what a store reporting a value's replacement produces, and
    /// the other order would silently drop it.
    ///
    /// Below the threshold each row is an insert or a remove; above it the whole graph is
    /// rebuilt. The crossover is where `d` memmoves of `n` bytes stop being cheaper than one
    /// sort of `n + d` rows, and the exact point is not delicate: a holon tick carries a
    /// handful of triples and a bulk load carries all of them, so any threshold in the broad
    /// middle sends each to the right strategy.
    pub fn apply(&mut self, added: &[Row], removed: &[Row]) {
        const REBUILD_ABOVE: usize = 64;

        if added.len() + removed.len() > REBUILD_ABOVE.max(self.len() / 16) {
            let mut rows: Vec<Row> = Vec::with_capacity(self.spo.len() + added.len());
            let dropped: std::collections::HashSet<Row> = removed.iter().copied().collect();
            rows.extend(self.spo.iter().copied().filter(|r| !dropped.contains(r)));
            rows.extend(added.iter().copied());
            *self = GraphBuilder { rows }.build();
            return;
        }

        for &[s, p, o] in removed {
            self.remove(s, p, o);
        }
        for &[s, p, o] in added {
            self.insert(s, p, o);
        }
    }
}

/// The contiguous slice of `rows` whose first component equals `a`.
#[inline]
fn prefix1(rows: &[Row], a: TermId) -> &[Row] {
    let lo = rows.partition_point(|r| r[0] < a);
    let hi = rows[lo..].partition_point(|r| r[0] == a) + lo;
    &rows[lo..hi]
}

/// The contiguous slice of `rows` whose first two components equal `(a, b)`.
#[inline]
fn prefix2(rows: &[Row], a: TermId, b: TermId) -> &[Row] {
    let lo = rows.partition_point(|r| (r[0], r[1]) < (a, b));
    let hi = rows[lo..].partition_point(|r| (r[0], r[1]) == (a, b)) + lo;
    &rows[lo..hi]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u32) -> TermId {
        TermId(n)
    }

    /// `:a :p :b`, `:a :p :c`, `:a :q :b`, `:d :p :b`
    fn sample() -> Graph {
        let mut b = GraphBuilder::new();
        b.push(t(0), t(1), t(2));
        b.push(t(0), t(1), t(3));
        b.push(t(0), t(4), t(2));
        b.push(t(5), t(1), t(2));
        b.push(t(0), t(1), t(2)); // duplicate
        b.build()
    }

    #[test]
    fn deduplicates_on_build() {
        assert_eq!(sample().len(), 4);
    }

    #[test]
    fn objects_selects_by_subject_and_predicate() {
        let g = sample();
        let got: Vec<_> = g.objects(t(0), t(1)).collect();
        assert_eq!(got, vec![t(2), t(3)]);
        assert_eq!(g.objects(t(0), t(4)).collect::<Vec<_>>(), vec![t(2)]);
        assert_eq!(g.objects(t(9), t(1)).count(), 0, "absent subject");
    }

    #[test]
    fn subjects_selects_by_predicate_and_object() {
        let g = sample();
        let got: Vec<_> = g.subjects(t(1), t(2)).collect();
        assert_eq!(got, vec![t(0), t(5)]);
        assert_eq!(g.subjects(t(1), t(9)).count(), 0);
    }

    #[test]
    fn predicate_and_object_projections() {
        let g = sample();
        // `pos` is ordered by (predicate, object, subject), so subjects come
        // back grouped by object rather than sorted.
        assert_eq!(
            g.subjects_of(t(1)).collect::<Vec<_>>(),
            vec![t(0), t(5), t(0)]
        );
        assert_eq!(
            g.objects_of(t(1)).collect::<Vec<_>>(),
            vec![t(2), t(2), t(3)]
        );
        assert_eq!(g.objects_of(t(99)).count(), 0);
    }

    #[test]
    fn adjacency_views() {
        let g = sample();
        let mut po: Vec<_> = g.predicate_objects(t(0)).collect();
        po.sort();
        assert_eq!(po, vec![(t(1), t(2)), (t(1), t(3)), (t(4), t(2))]);

        let mut sp: Vec<_> = g.subject_predicates(t(3)).collect();
        sp.sort();
        assert_eq!(sp, vec![(t(0), t(1))]);
    }

    #[test]
    fn contains_and_has_subject() {
        let g = sample();
        assert!(g.contains(t(0), t(1), t(2)));
        assert!(!g.contains(t(0), t(1), t(9)));
        assert!(g.has_subject(t(5)));
        assert!(!g.has_subject(t(2)), "only ever an object");
    }

    #[test]
    fn empty_graph_answers_everything_emptily() {
        let g = GraphBuilder::new().build();
        assert!(g.is_empty());
        assert_eq!(g.objects(t(0), t(1)).count(), 0);
        assert!(!g.contains(t(0), t(1), t(2)));
    }

    // ----------------------------------------------------------------- merge in place

    /// Every triple in the graph, read back through each of the three indexes separately.
    ///
    /// A mutation that updates `spo` and forgets `pos` leaves a graph that answers one
    /// question correctly and another wrongly, and every caller-level test would still pass
    /// as long as it happened to ask the first. So the check is per index, not per graph.
    fn dump(g: &Graph) -> (Vec<Row>, Vec<Row>, Vec<Row>) {
        let mut by_spo: Vec<Row> = g.iter().collect();
        let mut by_pos: Vec<Row> = Vec::new();
        let mut by_osp: Vec<Row> = Vec::new();
        // `subjects_of` walks `pos`; `subject_predicates` walks `osp`.
        let predicates: std::collections::BTreeSet<TermId> = by_spo.iter().map(|r| r[1]).collect();
        for p in predicates {
            for sub in g.subjects_of(p) {
                for o in g.objects(sub, p) {
                    by_pos.push([sub, p, o]);
                }
            }
        }
        let objects: std::collections::BTreeSet<TermId> = by_spo.iter().map(|r| r[2]).collect();
        for o in objects {
            for (sub, p) in g.subject_predicates(o) {
                by_osp.push([sub, p, o]);
            }
        }
        by_spo.sort_unstable();
        by_pos.sort_unstable();
        by_pos.dedup();
        by_osp.sort_unstable();
        by_osp.dedup();
        (by_spo, by_pos, by_osp)
    }

    fn rebuilt(rows: &[Row]) -> Graph {
        let mut b = GraphBuilder::new();
        for &[s, p, o] in rows {
            b.push(s, p, o);
        }
        b.build()
    }

    /// A deterministic pseudo-random sequence, so a failure is reproducible.
    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (*seed >> 33) as u32
    }

    #[test]
    fn an_inserted_triple_is_visible_through_every_index() {
        let mut g = sample();
        assert!(g.insert(t(9), t(1), t(2)), "not present before");
        assert!(!g.insert(t(9), t(1), t(2)), "and idempotent after");
        let (spo, pos, osp) = dump(&g);
        assert!(spo.contains(&[t(9), t(1), t(2)]));
        assert!(pos.contains(&[t(9), t(1), t(2)]), "pos index missed it");
        assert!(osp.contains(&[t(9), t(1), t(2)]), "osp index missed it");
        assert!(g.contains(t(9), t(1), t(2)));
    }

    #[test]
    fn a_removed_triple_is_gone_from_every_index() {
        let mut g = sample();
        assert!(g.remove(t(0), t(1), t(2)), "present before");
        assert!(!g.remove(t(0), t(1), t(2)), "and idempotent after");
        let (spo, pos, osp) = dump(&g);
        assert!(!spo.contains(&[t(0), t(1), t(2)]));
        assert!(!pos.contains(&[t(0), t(1), t(2)]), "pos index kept it");
        assert!(!osp.contains(&[t(0), t(1), t(2)]), "osp index kept it");
        assert!(!g.contains(t(0), t(1), t(2)));
    }

    #[test]
    fn removing_something_absent_changes_nothing() {
        let mut g = sample();
        let before = dump(&g);
        assert!(!g.remove(t(77), t(77), t(77)));
        assert_eq!(before, dump(&g));
    }

    #[test]
    fn a_delta_that_removes_and_re_adds_ends_with_the_triple_present() {
        // A store reporting a value's replacement produces exactly this, and applying the
        // additions first would drop it.
        let mut g = sample();
        g.apply(&[[t(0), t(1), t(2)]], &[[t(0), t(1), t(2)]]);
        assert!(g.contains(t(0), t(1), t(2)));
    }

    /// The differential property: however a graph is reached, it answers like one built from
    /// the same triples in one go.
    ///
    /// Run either side of the rebuild threshold, because the two strategies are separate
    /// code and a test that only exercised one would leave the other unwitnessed. The
    /// threshold itself is a performance choice and no correctness test can pin it: moving it
    /// changes which branch runs and nothing else, which is exactly the property these assert.
    fn agrees_with_a_rebuild(batch: usize, rounds: usize, seed: u64) {
        let mut seed = seed;
        let mut g = Graph::default();
        let mut model: std::collections::BTreeSet<Row> = std::collections::BTreeSet::new();

        for _ in 0..rounds {
            let mut added = Vec::new();
            let mut removed = Vec::new();
            for _ in 0..batch {
                let row = [
                    t(lcg(&mut seed) % 12),
                    t(lcg(&mut seed) % 4),
                    t(lcg(&mut seed) % 12),
                ];
                if lcg(&mut seed) % 3 == 0 {
                    removed.push(row);
                } else {
                    added.push(row);
                }
            }
            // The model applies them in the same order the graph promises to.
            for r in &removed {
                model.remove(r);
            }
            for r in &added {
                model.insert(*r);
            }
            g.apply(&added, &removed);

            let expected: Vec<Row> = model.iter().copied().collect();
            assert_eq!(
                dump(&g),
                dump(&rebuilt(&expected)),
                "batch={batch} after applying {} additions and {} removals",
                added.len(),
                removed.len()
            );
            assert_eq!(g.len(), expected.len());
        }
    }

    #[test]
    fn small_deltas_agree_with_a_rebuild() {
        agrees_with_a_rebuild(4, 60, 0x5eed);
    }

    #[test]
    fn large_deltas_agree_with_a_rebuild() {
        // Past `REBUILD_ABOVE`, so this exercises the other branch.
        agrees_with_a_rebuild(200, 12, 0xc0ffee);
    }
}
