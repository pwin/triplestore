//! The nine-order quad index — HOLOS L2, Tier A.
//!
//! `DESIGN.md` §6.1 keeps Oxigraph's layout because it is already the right answer:
//!
//! | family | holds | serves |
//! |---|---|---|
//! | `dspo` `dpos` `dosp` | default-graph **triples** | patterns confined to the default graph, without paying for a graph column |
//! | `spog` `posg` `ospg` | quads, graph **last** | patterns where the graph is unbound |
//! | `gspo` `gpos` `gosp` | quads, graph **first** | patterns where the graph is bound |
//!
//! Every pattern shape reduces to a prefix range over exactly one of the nine, which is
//! what [`QuadIndex::quads_for_pattern`] routes. This is a `BTreeSet` standing in for the
//! RocksDB column families of the design: the shape of the access — ordered prefix seek —
//! is the same, so the router above it does not change when the substrate does.
//!
//! Scans yield [`Result`] even though this tier never fails, because the RocksDB tier
//! will. See [`crate::error`].

use crate::error::Result;
use holos_core::{Tag, TermId};
use std::collections::BTreeSet;

/// A quad whose terms have been interned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EncodedQuad {
    /// Subject.
    pub subject: TermId,
    /// Predicate.
    pub predicate: TermId,
    /// Object.
    pub object: TermId,
    /// `None` for the default graph.
    pub graph_name: Option<TermId>,
}

/// An inclusive span of term ids, for a scan bounded by more than equality.
///
/// The point of `DESIGN.md` §5's order-preserving encodings: an inline integer, float or
/// dateTime has an id whose numeric order *is* the value's order, so `FILTER(?d > "2020-01-01")`
/// is a bound on the index rather than a test applied to everything it returns.
///
/// Inclusive at both ends because the caller is working in term ids, where "the next id
/// after this one" is a meaningful thing to compute and an exclusive end would make every
/// caller compute it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRange {
    /// Lowest id the span admits.
    pub first: TermId,
    /// Highest id the span admits.
    pub last: TermId,
}

impl IdRange {
    /// Every id with a given tag.
    ///
    /// What a caller needs for a tag whose ids are *not* ordered by value — a dictionary
    /// literal, say. A numeric comparison can be satisfied by an `xsd:decimal`, which the
    /// inline codec declines, so a sound range for one has to admit the whole dictionary
    /// region alongside the ordered part. See `holos_engine::range`.
    #[must_use]
    pub const fn whole_tag(tag: Tag) -> Self {
        Self {
            first: TermId::new(tag, 0),
            last: TermId::new(tag, holos_core::term_id::PAYLOAD_MAX),
        }
    }

    /// Whether the span admits an id.
    #[must_use]
    pub fn contains(&self, id: TermId) -> bool {
        self.first <= id && id <= self.last
    }

    /// Whether the span admits nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.first > self.last
    }
}

/// Which graphs a pattern is allowed to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphFilter {
    /// The default graph only.
    Default,
    /// One named graph.
    Named(TermId),
    /// Any named graph, but *not* the default graph — SPARQL's `GRAPH ?g` semantics.
    AnyNamed,
    /// Every graph, default included.
    Any,
}

/// Sentinels below and above every id an encoder can produce.
///
/// `MAX` is tag `0xF`, which is `Tag::Reserved` and never produced, so a range ending at
/// `[.., MAX]` covers every real key regardless of whether the end is inclusive. That is
/// worth saying because it makes the `..=` below unfalsifiable by test: swapping it for `..`
/// changes the answer only for a subject no store can hold.
const MIN: TermId = TermId::from_raw(u64::MIN);
const MAX: TermId = TermId::from_raw(u64::MAX);

/// A scan: an iterator of quads that can fail part-way through.
pub type QuadScan<'a> = Box<dyn Iterator<Item = Result<EncodedQuad>> + 'a>;

/// Nine sorted index orders over a set of quads.
#[derive(Debug, Default, Clone)]
pub struct QuadIndex {
    dspo: BTreeSet<[TermId; 3]>,
    dpos: BTreeSet<[TermId; 3]>,
    dosp: BTreeSet<[TermId; 3]>,
    spog: BTreeSet<[TermId; 4]>,
    posg: BTreeSet<[TermId; 4]>,
    ospg: BTreeSet<[TermId; 4]>,
    gspo: BTreeSet<[TermId; 4]>,
    gpos: BTreeSet<[TermId; 4]>,
    gosp: BTreeSet<[TermId; 4]>,
    /// Named graphs that exist, including ones holding no quads.
    graphs: BTreeSet<TermId>,
}

impl QuadIndex {
    /// An index holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of quads stored. Infallible: a persistent tier keeps this as a counter.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dspo.len() + self.gspo.len()
    }

    /// True when no quads are stored. Named graphs may still exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Inserts a quad. `Ok(true)` if it was not already present.
    pub fn insert(&mut self, quad: EncodedQuad) -> Result<bool> {
        let EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name,
        } = quad;
        match graph_name {
            None => {
                if !self.dspo.insert([s, p, o]) {
                    return Ok(false);
                }
                self.dpos.insert([p, o, s]);
                self.dosp.insert([o, s, p]);
            }
            Some(g) => {
                if !self.gspo.insert([g, s, p, o]) {
                    return Ok(false);
                }
                self.gpos.insert([g, p, o, s]);
                self.gosp.insert([g, o, s, p]);
                self.spog.insert([s, p, o, g]);
                self.posg.insert([p, o, s, g]);
                self.ospg.insert([o, s, p, g]);
                self.graphs.insert(g);
            }
        }
        Ok(true)
    }

    /// Removes a quad. `Ok(true)` if it was present.
    ///
    /// The named graph itself survives an emptying removal, matching SPARQL Update, where
    /// `DELETE DATA` does not drop the graph.
    pub fn remove(&mut self, quad: EncodedQuad) -> Result<bool> {
        let EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name,
        } = quad;
        match graph_name {
            None => {
                if !self.dspo.remove(&[s, p, o]) {
                    return Ok(false);
                }
                self.dpos.remove(&[p, o, s]);
                self.dosp.remove(&[o, s, p]);
            }
            Some(g) => {
                if !self.gspo.remove(&[g, s, p, o]) {
                    return Ok(false);
                }
                self.gpos.remove(&[g, p, o, s]);
                self.gosp.remove(&[g, o, s, p]);
                self.spog.remove(&[s, p, o, g]);
                self.posg.remove(&[p, o, s, g]);
                self.ospg.remove(&[o, s, p, g]);
            }
        }
        Ok(true)
    }

    /// Records a named graph that may hold no quads.
    pub fn insert_named_graph(&mut self, graph: TermId) -> Result<bool> {
        Ok(self.graphs.insert(graph))
    }

    /// Drops a named graph and everything in it. `Ok(true)` if it existed.
    pub fn remove_named_graph(&mut self, graph: TermId) -> Result<bool> {
        let victims = self
            .quads_for_pattern(None, None, None, GraphFilter::Named(graph))
            .collect::<Result<Vec<_>>>()?;
        for quad in victims {
            self.remove(quad)?;
        }
        Ok(self.graphs.remove(&graph))
    }

    /// Every named graph, whether or not it holds quads.
    pub fn named_graphs(&self) -> impl Iterator<Item = Result<TermId>> + '_ {
        self.graphs.iter().copied().map(Ok)
    }

    /// Whether a named graph exists.
    pub fn contains_named_graph(&self, graph: TermId) -> Result<bool> {
        Ok(self.graphs.contains(&graph))
    }

    /// Which of the nine orders a pattern would be answered from.
    ///
    /// Exposed for tests and for `EXPLAIN`-style output: routing is the part of this layer
    /// most worth being able to assert on.
    #[must_use]
    pub fn plan(
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
        graph: GraphFilter,
    ) -> &'static str {
        let bound = (subject.is_some(), predicate.is_some(), object.is_some());
        match graph {
            GraphFilter::Default => match bound {
                (true, _, false) | (true, true, true) => "dspo",
                (false, true, _) => "dpos",
                (_, false, true) => "dosp",
                (false, false, false) => "dspo",
            },
            GraphFilter::Named(_) => match bound {
                (true, _, false) | (true, true, true) => "gspo",
                (false, true, _) => "gpos",
                (_, false, true) => "gosp",
                (false, false, false) => "gspo",
            },
            GraphFilter::AnyNamed | GraphFilter::Any => match bound {
                (true, _, false) | (true, true, true) => "spog",
                (false, true, _) => "posg",
                (_, false, true) => "ospg",
                (false, false, false) => "spog",
            },
        }
    }

    /// Every quad matching a pattern. `None` in a position means unbound.
    pub fn quads_for_pattern(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        object: Option<TermId>,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        match graph {
            GraphFilter::Default => self.scan_default(subject, predicate, object),
            GraphFilter::Named(g) => self.scan_named(g, subject, predicate, object),
            GraphFilter::AnyNamed => self.scan_any_named(subject, predicate, object),
            GraphFilter::Any => Box::new(
                self.scan_default(subject, predicate, object)
                    .chain(self.scan_any_named(subject, predicate, object)),
            ),
        }
    }

    // --- default graph: the d* orders, three components per key -------------------

    /// Quads matching the pattern whose **object** lies inside `span`.
    ///
    /// The order is chosen so the span bounds the scan rather than filtering it: with a
    /// predicate bound that is `pos`, where the object is the component straight after the
    /// prefix; without one it is `osp`, where the object leads. A bound subject is checked
    /// afterwards, because in both of those orders it sits behind the object and cannot
    /// narrow a contiguous range.
    ///
    /// The caller still applies its filter. This narrows *what is read*; it does not decide
    /// what matches, and it must therefore admit at least everything that could.
    #[must_use]
    pub fn quads_with_object_in(
        &self,
        subject: Option<TermId>,
        predicate: Option<TermId>,
        span: IdRange,
        graph: GraphFilter,
    ) -> QuadScan<'_> {
        if span.is_empty() {
            return Box::new(std::iter::empty());
        }
        let rows: QuadScan<'_> = match graph {
            GraphFilter::Default => self.range_default(predicate, span),
            GraphFilter::Named(g) => self.range_named(g, predicate, span),
            GraphFilter::AnyNamed => self.range_any_named(predicate, span),
            GraphFilter::Any => Box::new(
                self.range_default(predicate, span)
                    .chain(self.range_any_named(predicate, span)),
            ),
        };
        match subject {
            None => rows,
            Some(s) => Box::new(rows.filter(move |q| match q {
                Ok(quad) => quad.subject == s,
                Err(_) => true,
            })),
        }
    }

    fn range_default(&self, p: Option<TermId>, span: IdRange) -> QuadScan<'_> {
        let quad = |s, p, o| EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name: None,
        };
        match p {
            Some(p) => Box::new(
                self.dpos
                    .range([p, span.first, MIN]..=[p, span.last, MAX])
                    .map(move |&[p, o, s]| Ok(quad(s, p, o))),
            ),
            None => Box::new(
                self.dosp
                    .range([span.first, MIN, MIN]..=[span.last, MAX, MAX])
                    .map(move |&[o, s, p]| Ok(quad(s, p, o))),
            ),
        }
    }

    fn range_named(&self, g: TermId, p: Option<TermId>, span: IdRange) -> QuadScan<'_> {
        let quad = move |s, p, o| EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name: Some(g),
        };
        match p {
            Some(p) => Box::new(
                self.gpos
                    .range([g, p, span.first, MIN]..=[g, p, span.last, MAX])
                    .map(move |&[_, p, o, s]| Ok(quad(s, p, o))),
            ),
            None => Box::new(
                self.gosp
                    .range([g, span.first, MIN, MIN]..=[g, span.last, MAX, MAX])
                    .map(move |&[_, o, s, p]| Ok(quad(s, p, o))),
            ),
        }
    }

    fn range_any_named(&self, p: Option<TermId>, span: IdRange) -> QuadScan<'_> {
        let quad = |s, p, o, g| EncodedQuad {
            subject: s,
            predicate: p,
            object: o,
            graph_name: Some(g),
        };
        match p {
            Some(p) => Box::new(
                self.posg
                    .range([p, span.first, MIN, MIN]..=[p, span.last, MAX, MAX])
                    .map(move |&[p, o, s, g]| Ok(quad(s, p, o, g))),
            ),
            None => Box::new(
                self.ospg
                    .range([span.first, MIN, MIN, MIN]..=[span.last, MAX, MAX, MAX])
                    .map(move |&[o, s, p, g]| Ok(quad(s, p, o, g))),
            ),
        }
    }

    fn scan_default(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
    ) -> QuadScan<'_> {
        let quad = |subject, predicate, object| EncodedQuad {
            subject,
            predicate,
            object,
            graph_name: None,
        };
        match (s, p, o) {
            (Some(s), Some(p), Some(o)) => {
                Box::new(self.scan3(&self.dspo, &[s, p, o], move |[s, p, o]| quad(s, p, o)))
            }
            (Some(s), Some(p), None) => {
                Box::new(self.scan3(&self.dspo, &[s, p], move |[s, p, o]| quad(s, p, o)))
            }
            (Some(s), None, Some(o)) => {
                Box::new(self.scan3(&self.dosp, &[o, s], move |[o, s, p]| quad(s, p, o)))
            }
            (Some(s), None, None) => {
                Box::new(self.scan3(&self.dspo, &[s], move |[s, p, o]| quad(s, p, o)))
            }
            (None, Some(p), Some(o)) => {
                Box::new(self.scan3(&self.dpos, &[p, o], move |[p, o, s]| quad(s, p, o)))
            }
            (None, Some(p), None) => {
                Box::new(self.scan3(&self.dpos, &[p], move |[p, o, s]| quad(s, p, o)))
            }
            (None, None, Some(o)) => {
                Box::new(self.scan3(&self.dosp, &[o], move |[o, s, p]| quad(s, p, o)))
            }
            (None, None, None) => {
                Box::new(self.scan3(&self.dspo, &[], move |[s, p, o]| quad(s, p, o)))
            }
        }
    }

    // --- one named graph: the g* orders, graph first ------------------------------

    fn scan_named(
        &self,
        g: TermId,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
    ) -> QuadScan<'_> {
        let quad = move |subject, predicate, object| EncodedQuad {
            subject,
            predicate,
            object,
            graph_name: Some(g),
        };
        match (s, p, o) {
            (Some(s), Some(p), Some(o)) => {
                Box::new(self.scan4(&self.gspo, &[g, s, p, o], move |[_, s, p, o]| quad(s, p, o)))
            }
            (Some(s), Some(p), None) => {
                Box::new(self.scan4(&self.gspo, &[g, s, p], move |[_, s, p, o]| quad(s, p, o)))
            }
            (Some(s), None, Some(o)) => {
                Box::new(self.scan4(&self.gosp, &[g, o, s], move |[_, o, s, p]| quad(s, p, o)))
            }
            (Some(s), None, None) => {
                Box::new(self.scan4(&self.gspo, &[g, s], move |[_, s, p, o]| quad(s, p, o)))
            }
            (None, Some(p), Some(o)) => {
                Box::new(self.scan4(&self.gpos, &[g, p, o], move |[_, p, o, s]| quad(s, p, o)))
            }
            (None, Some(p), None) => {
                Box::new(self.scan4(&self.gpos, &[g, p], move |[_, p, o, s]| quad(s, p, o)))
            }
            (None, None, Some(o)) => {
                Box::new(self.scan4(&self.gosp, &[g, o], move |[_, o, s, p]| quad(s, p, o)))
            }
            (None, None, None) => {
                Box::new(self.scan4(&self.gspo, &[g], move |[_, s, p, o]| quad(s, p, o)))
            }
        }
    }

    // --- any named graph: the *g orders, graph last -------------------------------

    fn scan_any_named(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
    ) -> QuadScan<'_> {
        let quad = |subject, predicate, object, g| EncodedQuad {
            subject,
            predicate,
            object,
            graph_name: Some(g),
        };
        match (s, p, o) {
            (Some(s), Some(p), Some(o)) => {
                Box::new(self.scan4(&self.spog, &[s, p, o], move |[s, p, o, g]| quad(s, p, o, g)))
            }
            (Some(s), Some(p), None) => {
                Box::new(self.scan4(&self.spog, &[s, p], move |[s, p, o, g]| quad(s, p, o, g)))
            }
            (Some(s), None, Some(o)) => {
                Box::new(self.scan4(&self.ospg, &[o, s], move |[o, s, p, g]| quad(s, p, o, g)))
            }
            (Some(s), None, None) => {
                Box::new(self.scan4(&self.spog, &[s], move |[s, p, o, g]| quad(s, p, o, g)))
            }
            (None, Some(p), Some(o)) => {
                Box::new(self.scan4(&self.posg, &[p, o], move |[p, o, s, g]| quad(s, p, o, g)))
            }
            (None, Some(p), None) => {
                Box::new(self.scan4(&self.posg, &[p], move |[p, o, s, g]| quad(s, p, o, g)))
            }
            (None, None, Some(o)) => {
                Box::new(self.scan4(&self.ospg, &[o], move |[o, s, p, g]| quad(s, p, o, g)))
            }
            (None, None, None) => {
                Box::new(self.scan4(&self.spog, &[], move |[s, p, o, g]| quad(s, p, o, g)))
            }
        }
    }

    // --- prefix range helpers -----------------------------------------------------

    /// Prefix range over a three-component order.
    ///
    /// Because ids of one tag form a contiguous block ordered by payload, and unbound
    /// positions widen to the whole `TermId` space, a bound prefix is exactly a
    /// `BTreeSet` range — the in-memory equivalent of a RocksDB prefix seek.
    fn scan3<'a>(
        &'a self,
        set: &'a BTreeSet<[TermId; 3]>,
        prefix: &[TermId],
        unpermute: impl Fn([TermId; 3]) -> EncodedQuad + 'a,
    ) -> impl Iterator<Item = Result<EncodedQuad>> + 'a {
        let mut lo = [MIN; 3];
        let mut hi = [MAX; 3];
        for (i, v) in prefix.iter().enumerate() {
            lo[i] = *v;
            hi[i] = *v;
        }
        set.range(lo..=hi).copied().map(unpermute).map(Ok)
    }

    /// Prefix range over a four-component order.
    fn scan4<'a>(
        &'a self,
        set: &'a BTreeSet<[TermId; 4]>,
        prefix: &[TermId],
        unpermute: impl Fn([TermId; 4]) -> EncodedQuad + 'a,
    ) -> impl Iterator<Item = Result<EncodedQuad>> + 'a {
        let mut lo = [MIN; 4];
        let mut hi = [MAX; 4];
        for (i, v) in prefix.iter().enumerate() {
            lo[i] = *v;
            hi[i] = *v;
        }
        set.range(lo..=hi).copied().map(unpermute).map(Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holos_core::Tag;

    fn id(n: u64) -> TermId {
        TermId::new(Tag::Iri, n)
    }

    fn quad(s: u64, p: u64, o: u64, g: Option<u64>) -> EncodedQuad {
        EncodedQuad {
            subject: id(s),
            predicate: id(p),
            object: id(o),
            graph_name: g.map(id),
        }
    }

    /// A small fixture spanning the default graph and two named graphs.
    fn fixture() -> QuadIndex {
        let mut ix = QuadIndex::new();
        for q in [
            quad(1, 10, 100, None),
            quad(1, 11, 101, None),
            quad(2, 10, 100, None),
            quad(1, 10, 100, Some(900)),
            quad(3, 12, 102, Some(900)),
            quad(1, 11, 103, Some(901)),
        ] {
            assert!(ix.insert(q).unwrap(), "fixture must not contain duplicates");
        }
        ix
    }

    fn collect(
        ix: &QuadIndex,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        g: GraphFilter,
    ) -> Vec<EncodedQuad> {
        let mut v: Vec<_> = ix
            .quads_for_pattern(s.map(id), p.map(id), o.map(id), g)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        v.sort_unstable();
        v
    }

    /// The reference implementation every routed scan is checked against: filter
    /// everything. If a permutation or an un-permutation is wrong, this catches it.
    fn brute_force(
        ix: &QuadIndex,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        g: GraphFilter,
    ) -> Vec<EncodedQuad> {
        let mut v: Vec<_> = ix
            .quads_for_pattern(None, None, None, GraphFilter::Any)
            .map(std::result::Result::unwrap)
            .filter(|q| {
                s.is_none_or(|s| q.subject == id(s))
                    && p.is_none_or(|p| q.predicate == id(p))
                    && o.is_none_or(|o| q.object == id(o))
                    && match g {
                        GraphFilter::Default => q.graph_name.is_none(),
                        GraphFilter::Named(g) => q.graph_name == Some(g),
                        GraphFilter::AnyNamed => q.graph_name.is_some(),
                        GraphFilter::Any => true,
                    }
            })
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn every_pattern_shape_agrees_with_brute_force() {
        let ix = fixture();
        let terms = [None, Some(1), Some(2), Some(3), Some(99)];
        let preds = [None, Some(10), Some(11), Some(99)];
        let objs = [None, Some(100), Some(103), Some(99)];
        let graphs = [
            GraphFilter::Default,
            GraphFilter::Named(id(900)),
            GraphFilter::Named(id(901)),
            GraphFilter::Named(id(999)),
            GraphFilter::AnyNamed,
            GraphFilter::Any,
        ];
        // 5 × 4 × 4 × 6 = 480 pattern shapes, each routed to one of the nine orders and
        // compared against an unindexed filter.
        for s in terms {
            for p in preds {
                for o in objs {
                    for g in graphs {
                        assert_eq!(
                            collect(&ix, s, p, o, g),
                            brute_force(&ix, s, p, o, g),
                            "pattern s={s:?} p={p:?} o={o:?} g={g:?} \
                             routed to {}",
                            QuadIndex::plan(s.map(id), p.map(id), o.map(id), g)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn routing_picks_the_index_whose_prefix_is_bound() {
        // The point of holding nine orders is that no pattern ever needs a full scan
        // followed by a filter. These are the cases that justify each family.
        assert_eq!(
            QuadIndex::plan(None, Some(id(1)), None, GraphFilter::Default),
            "dpos"
        );
        assert_eq!(
            QuadIndex::plan(None, None, Some(id(1)), GraphFilter::Default),
            "dosp"
        );
        assert_eq!(
            QuadIndex::plan(Some(id(1)), None, None, GraphFilter::Default),
            "dspo"
        );
        assert_eq!(
            QuadIndex::plan(None, Some(id(1)), None, GraphFilter::Named(id(9))),
            "gpos"
        );
        assert_eq!(
            QuadIndex::plan(None, None, Some(id(1)), GraphFilter::AnyNamed),
            "ospg"
        );
    }

    #[test]
    fn insert_is_idempotent_and_remove_is_complete() {
        let mut ix = fixture();
        let before = ix.len();
        assert!(
            !ix.insert(quad(1, 10, 100, None)).unwrap(),
            "duplicate insert"
        );
        assert_eq!(ix.len(), before);

        assert!(ix.remove(quad(1, 10, 100, None)).unwrap());
        assert!(!ix.remove(quad(1, 10, 100, None)).unwrap(), "double remove");
        assert_eq!(ix.len(), before - 1);

        // Removal must clear every order, not just the one that reports length — so the
        // removed quad is unreachable from dspo, dpos and dosp alike...
        assert_eq!(
            collect(&ix, Some(1), Some(10), Some(100), GraphFilter::Default),
            vec![]
        );
        // ...while its siblings, which share a prefix in each of those orders, survive.
        assert_eq!(
            collect(&ix, None, Some(10), Some(100), GraphFilter::Default),
            vec![quad(2, 10, 100, None)]
        );
        assert_eq!(
            collect(&ix, None, None, Some(100), GraphFilter::Default),
            vec![quad(2, 10, 100, None)]
        );
        // The same triple in a named graph is a different quad and is untouched.
        assert_eq!(
            collect(&ix, Some(1), Some(10), Some(100), GraphFilter::AnyNamed),
            vec![quad(1, 10, 100, Some(900))]
        );
    }

    #[test]
    fn named_graph_removal_takes_its_quads_with_it() {
        let mut ix = fixture();
        assert!(ix.contains_named_graph(id(900)).unwrap());
        assert!(ix.remove_named_graph(id(900)).unwrap());
        assert!(!ix.contains_named_graph(id(900)).unwrap());
        assert_eq!(
            collect(&ix, None, None, None, GraphFilter::Named(id(900))),
            vec![]
        );
        // The other named graph is untouched.
        assert_eq!(
            collect(&ix, None, None, None, GraphFilter::Named(id(901))).len(),
            1
        );
        assert!(
            !ix.remove_named_graph(id(900)).unwrap(),
            "second removal is a no-op"
        );
    }

    #[test]
    fn empty_named_graphs_exist_without_quads() {
        let mut ix = QuadIndex::new();
        assert!(ix.insert_named_graph(id(900)).unwrap());
        assert!(!ix.insert_named_graph(id(900)).unwrap());
        assert!(ix.contains_named_graph(id(900)).unwrap());
        assert!(ix.is_empty(), "an empty graph holds no quads");
        assert_eq!(
            ix.named_graphs().collect::<Result<Vec<_>>>().unwrap(),
            vec![id(900)]
        );
    }

    #[test]
    fn default_graph_is_not_a_named_graph() {
        // SPARQL's GRAPH ?g must never bind ?g to the default graph.
        let ix = fixture();
        assert!(collect(&ix, None, None, None, GraphFilter::AnyNamed)
            .iter()
            .all(|q| q.graph_name.is_some()));
        assert_eq!(ix.named_graphs().count(), 2);
    }
}
