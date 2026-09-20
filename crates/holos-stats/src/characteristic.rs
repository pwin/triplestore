//! Characteristic sets, and estimation from them.
//!
//! A *characteristic set* is the set of predicates occurring on one subject. Group every
//! subject by its predicate-set and you have, for each shape of entity in the data, how many
//! entities have that shape and how many times each predicate occurs across them.
//!
//! That is exactly the structure a star pattern needs. `?s foaf:name ?n . ?s foaf:mbox ?m`
//! is asking "how many subjects have both of these predicates, and how many combinations do
//! they produce" — which is a sum over the characteristic sets that contain both, of the
//! count times the per-subject multiplicity of each. No independence assumption between
//! predicates is made, which is where the naive `selectivity(p1) × selectivity(p2)` estimate
//! goes wrong: in real RDF, predicates are *strongly* correlated, because entities of the
//! same kind carry the same properties.

use crate::hll::DistinctCount;
use crate::Pattern;
use holos_core::TermId;
use holos_store::{GraphFilter, Result, Store};
use rustc_hash::FxHashMap;

/// One distinct shape of subject, with how often it occurs.
#[derive(Debug, Clone)]
pub struct CharacteristicSet {
    /// The predicates every subject in this set carries, sorted.
    pub predicates: Vec<TermId>,
    /// How many subjects have exactly this predicate set.
    pub subjects: u64,
    /// Total occurrences of each predicate across those subjects.
    ///
    /// Divided by `subjects` this is the average multiplicity — the number of objects a
    /// subject of this shape has for that predicate, which is what a star pattern
    /// multiplies out.
    pub occurrences: FxHashMap<TermId, u64>,
}

impl CharacteristicSet {
    /// Average number of objects a subject of this shape has for `predicate`.
    #[must_use]
    pub fn multiplicity(&self, predicate: TermId) -> f64 {
        if self.subjects == 0 {
            return 0.0;
        }
        let occurrences = self.occurrences.get(&predicate).copied().unwrap_or(0);
        occurrences as f64 / self.subjects as f64
    }

    /// Whether this shape carries every predicate in `wanted`.
    #[must_use]
    pub fn covers(&self, wanted: &[TermId]) -> bool {
        wanted.iter().all(|p| self.occurrences.contains_key(p))
    }
}

/// Per-predicate counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct PredicateStats {
    /// Triples using this predicate.
    pub triples: u64,
    /// Distinct subjects.
    pub subjects: u64,
    /// Distinct objects.
    pub objects: u64,
}

/// Statistics over a graph.
#[derive(Debug, Clone, Default)]
pub struct Statistics {
    predicates: FxHashMap<TermId, PredicateStats>,
    sets: Vec<CharacteristicSet>,
    /// Index from a predicate to the characteristic sets carrying it, so estimating a star
    /// scans the sets that could match rather than all of them.
    by_predicate: FxHashMap<TermId, Vec<usize>>,
    total_triples: u64,
    total_subjects: u64,
    /// The store's [`Store::generation`] when this was built. A snapshot is exactly right
    /// while the store still reports it and exactly wrong once it does not; `save` and
    /// `load_cached` are the two sides of that check.
    generation: u64,
}

impl Statistics {
    /// Builds statistics by scanning a graph.
    ///
    /// One pass in `spo` order, which is why the subject's predicates arrive together and
    /// a characteristic set can be closed without holding the whole graph in a map.
    ///
    /// # Distinct counts, and what they cost
    ///
    /// Both were once exact sets, one per predicate, kept only to be asked their `len`. On a
    /// 653.8-million-triple store with 48.4 million subjects that peaked at **12,984 MiB** —
    /// and it peaked on the wrong side of the problem, because `--reorder` is what supplies
    /// the estimate that admission control refuses on, so protecting a server against a
    /// memory-hungry query began by running one.
    ///
    /// **Subjects** are now counted rather than collected, and the `spo` order is what makes
    /// that exact. A subject's quads arrive together and the subject is never seen again, so
    /// by the time its characteristic set closes the predicates it carries are known: one
    /// increment each. No set, no error. That alone took the build to **5,207 MiB**.
    ///
    /// **Objects** arrive scattered in this order and have no such trick, so they get the
    /// `HyperLogLog` sketch §7 always specified — [`crate::hll`], 16 KiB per predicate
    /// whatever the cardinality, measured at a worst case of 2.3% against the estimator's own
    /// 10%. The docstring here used to argue against a sketch on the grounds that these
    /// numbers were about to measure how accurate estimation *can* be, and an approximation
    /// in the yardstick would muddy that. It was right at the time. The measurement has since
    /// been made.
    ///
    /// That second change bought nothing measurable on the store above — the peak came back
    /// at 5,206 MiB against 5,207 — because by then the remainder was the store being open
    /// and scanned rather than the object sets. It is a bound rather than a saving: the sets
    /// grew with the data and this does not.
    ///
    /// # One graph at a time
    ///
    /// Counting subjects rather than collecting them means this now *depends* on each subject
    /// being visited once, which holds for [`GraphFilter::Default`] and [`GraphFilter::Named`]
    /// — both scan one graph, in subject order. Under `AnyNamed` or `Any` a subject present in
    /// two graphs is visited twice and would be counted twice.
    ///
    /// Those two filters were already wrong here for the same reason, and more visibly: the
    /// subject closes once per graph, so it contributes a characteristic set per graph and
    /// inflates `total_subjects` too. The set-based count was the one number that survived it.
    /// Every caller passes `Default`. Making the union case correct means deciding what a
    /// characteristic set even means across graphs — per subject, or per subject per graph —
    /// which is a semantic question, not an implementation one.
    ///
    /// # Errors
    ///
    /// Propagates any error raised while scanning the store.
    pub fn build(store: &Store, graph: GraphFilter) -> Result<Self> {
        let mut stats = Self::default();
        // Read first, so a write that lands during the scan makes this snapshot stale by
        // its own account rather than silently claiming the generation after it.
        stats.generation = store.generation();
        // Counted, not collected: `close` sees each subject exactly once and knows which
        // predicates it carried, so a set here would only be rediscovering that. The
        // docstring has why this is exact and what it depends on.
        let mut distinct_subjects: FxHashMap<TermId, u64> = FxHashMap::default();
        // Objects arrive scattered in `spo` order, so there is nothing contiguous to count
        // and they get a sketch instead — fixed size per predicate. See `crate::hll`.
        let mut distinct_objects: FxHashMap<TermId, DistinctCount> = FxHashMap::default();

        // predicate-set -> (subject count, per-predicate occurrences)
        let mut shapes: FxHashMap<Vec<TermId>, (u64, FxHashMap<TermId, u64>)> =
            FxHashMap::default();

        let mut current_subject: Option<TermId> = None;
        let mut current: FxHashMap<TermId, u64> = FxHashMap::default();

        let close = |subject: Option<TermId>,
                     current: &mut FxHashMap<TermId, u64>,
                     shapes: &mut FxHashMap<Vec<TermId>, (u64, FxHashMap<TermId, u64>)>,
                     subjects_with: &mut FxHashMap<TermId, u64>,
                     total_subjects: &mut u64| {
            if subject.is_none() || current.is_empty() {
                current.clear();
                return;
            }
            let mut key: Vec<TermId> = current.keys().copied().collect();
            key.sort_unstable();
            let entry = shapes
                .entry(key)
                .or_insert_with(|| (0, FxHashMap::default()));
            entry.0 += 1;
            for (predicate, n) in current.iter() {
                *entry.1.entry(*predicate).or_insert(0) += *n;
                // This subject carried this predicate, and will not be seen again.
                *subjects_with.entry(*predicate).or_insert(0) += 1;
            }
            *total_subjects += 1;
            current.clear();
        };

        for quad in store.quads_for_pattern(None, None, None, graph) {
            let quad = quad?;
            stats.total_triples += 1;

            let entry = stats.predicates.entry(quad.predicate).or_default();
            entry.triples += 1;
            distinct_objects
                .entry(quad.predicate)
                .or_default()
                .add(quad.object);

            if current_subject != Some(quad.subject) {
                close(
                    current_subject,
                    &mut current,
                    &mut shapes,
                    &mut distinct_subjects,
                    &mut stats.total_subjects,
                );
                current_subject = Some(quad.subject);
            }
            *current.entry(quad.predicate).or_insert(0) += 1;
        }
        close(
            current_subject,
            &mut current,
            &mut shapes,
            &mut distinct_subjects,
            &mut stats.total_subjects,
        );

        for (predicate, subjects) in distinct_subjects {
            stats.predicates.entry(predicate).or_default().subjects = subjects;
        }
        for (predicate, objects) in distinct_objects {
            stats.predicates.entry(predicate).or_default().objects = objects.estimate();
        }

        for (predicates, (subjects, occurrences)) in shapes {
            let index = stats.sets.len();
            for predicate in &predicates {
                stats
                    .by_predicate
                    .entry(*predicate)
                    .or_default()
                    .push(index);
            }
            stats.sets.push(CharacteristicSet {
                predicates,
                subjects,
                occurrences,
            });
        }
        Ok(stats)
    }

    /// The generation of the store this describes. See [`Store::generation`].
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The snapshot the store keeps, if it still describes the store.
    ///
    /// `None` means build. It covers three cases and deliberately does not distinguish
    /// them: nothing saved yet, a snapshot from an older format, or one from before the
    /// last write. All three have the same remedy, and a snapshot is a cache — a bad one is
    /// discarded, never reported.
    ///
    /// Only the default graph's statistics are ever kept, because every caller asks for
    /// those. Another filter is built fresh each time.
    ///
    /// # Errors
    ///
    /// If the store cannot be read. A snapshot that cannot be *decoded* is `Ok(None)`.
    pub fn load_cached(store: &Store, graph: GraphFilter) -> Result<Option<Self>> {
        if !matches!(graph, GraphFilter::Default) {
            return Ok(None);
        }
        let Some(bytes) = store.load_statistics()? else {
            return Ok(None);
        };
        Ok(Self::from_bytes(&bytes).filter(|stats| stats.generation == store.generation()))
    }

    /// Keeps this snapshot with the store, if it still describes the store.
    ///
    /// Returns whether it did. `false` means a write landed since `build` read the
    /// generation, and a snapshot that would be stale on arrival is not worth writing —
    /// the next `load_cached` would reject it anyway.
    ///
    /// # Errors
    ///
    /// If the write fails.
    pub fn save(&self, store: &mut Store) -> Result<bool> {
        if store.generation() != self.generation {
            return Ok(false);
        }
        store.save_statistics(&self.to_bytes())?;
        Ok(true)
    }

    /// Loads the snapshot if it is current, otherwise builds and keeps one.
    ///
    /// The one-call form for a caller that holds the store exclusively — the CLI. The server
    /// builds under a read lock so queries keep flowing during the scan, and uses the two
    /// halves separately.
    ///
    /// # Errors
    ///
    /// Whatever building or saving returns. A snapshot that fails to save is still returned:
    /// the statistics are right, they just will not be there next time.
    pub fn cached(store: &mut Store, graph: GraphFilter) -> Result<Self> {
        if let Some(stats) = Self::load_cached(store, graph)? {
            return Ok(stats);
        }
        let stats = Self::build(store, graph)?;
        if matches!(graph, GraphFilter::Default) {
            stats.save(store)?;
        }
        Ok(stats)
    }

    /// How many distinct subject shapes the data has.
    #[must_use]
    pub fn shape_count(&self) -> usize {
        self.sets.len()
    }

    /// Every characteristic set.
    #[must_use]
    pub fn sets(&self) -> &[CharacteristicSet] {
        &self.sets
    }

    /// Triples in the graph.
    #[must_use]
    pub fn total_triples(&self) -> u64 {
        self.total_triples
    }

    /// Distinct subjects in the graph.
    #[must_use]
    pub fn total_subjects(&self) -> u64 {
        self.total_subjects
    }

    /// Counts for one predicate.
    #[must_use]
    pub fn predicate(&self, predicate: TermId) -> PredicateStats {
        self.predicates.get(&predicate).copied().unwrap_or_default()
    }

    /// Estimated rows for a single triple pattern.
    ///
    /// Where the reused optimiser returns a constant, this reads the counts it actually has.
    #[must_use]
    pub fn estimate_pattern(&self, pattern: &Pattern) -> f64 {
        match (pattern.subject, pattern.predicate, pattern.object) {
            // Fully bound: it is there or it is not, and one row is the right guess.
            (Some(_), Some(_), Some(_)) => 1.0,
            (None, Some(p), None) => self.predicate(p).triples as f64,
            (Some(_), Some(p), None) => {
                // Rows per subject that uses this predicate.
                let stats = self.predicate(p);
                ratio(stats.triples, stats.subjects)
            }
            (None, Some(p), Some(_)) => {
                let stats = self.predicate(p);
                ratio(stats.triples, stats.objects)
            }
            (Some(_), None, None) => ratio(self.total_triples, self.total_subjects),
            (Some(_), None, Some(_)) => {
                // Both ends pinned, predicate free: rarely more than a couple of triples.
                ratio(self.total_triples, self.total_subjects.max(1)).min(2.0)
            }
            (None, None, Some(_)) => {
                let distinct_objects: u64 = self.predicates.values().map(|s| s.objects).sum();
                ratio(self.total_triples, distinct_objects)
            }
            (None, None, None) => self.total_triples as f64,
        }
    }

    /// Estimated rows for a star: several patterns sharing one subject variable.
    ///
    /// This is the characteristic-set estimate, and the reason the structure is worth
    /// building. It sums over the subject shapes that actually carry every predicate in the
    /// star, so a star over predicates that never co-occur estimates zero rather than the
    /// product of two large selectivities.
    #[must_use]
    pub fn estimate_star(&self, patterns: &[Pattern]) -> f64 {
        let predicates: Vec<TermId> = patterns.iter().filter_map(|p| p.predicate).collect();
        if predicates.len() != patterns.len() || predicates.is_empty() {
            // An unbound predicate takes the star out of characteristic-set territory;
            // fall back to multiplying the individual estimates.
            return patterns.iter().map(|p| self.estimate_pattern(p)).product();
        }
        if predicates.len() == 1 {
            return self.estimate_pattern(&patterns[0]);
        }

        // Only the sets carrying the rarest predicate can possibly carry all of them.
        let Some(rarest) = predicates
            .iter()
            .min_by_key(|p| self.by_predicate.get(p).map_or(usize::MAX, Vec::len))
        else {
            return 0.0;
        };
        let candidates = self.by_predicate.get(rarest).map_or(&[][..], Vec::as_slice);

        let mut total = 0.0;
        for &index in candidates {
            let set = &self.sets[index];
            if !set.covers(&predicates) {
                continue;
            }
            let mut rows = set.subjects as f64;
            for pattern in patterns {
                let Some(predicate) = pattern.predicate else {
                    continue;
                };
                rows *= set.multiplicity(predicate);
                if pattern.object.is_some() {
                    // A bound object cuts the rows by the predicate's object cardinality.
                    let objects = self.predicate(predicate).objects.max(1);
                    rows /= objects as f64;
                }
            }
            total += rows;
        }
        total
    }

    /// Estimated rows for a basic graph pattern.
    ///
    /// Patterns sharing a subject variable are grouped into stars and estimated together;
    /// the stars are then joined. Joining stars still assumes independence — a proper join
    /// estimator would need cross-star statistics, which §7 does not claim to have. The
    /// gain here is that the *within*-star correlation, which is where RDF's structure
    /// actually lives, is no longer thrown away.
    #[must_use]
    pub fn estimate_bgp(&self, patterns: &[Pattern]) -> f64 {
        let mut stars: FxHashMap<u32, Vec<Pattern>> = FxHashMap::default();
        let mut loose = Vec::new();
        for pattern in patterns {
            match pattern.subject_var {
                Some(var) => stars.entry(var).or_default().push(*pattern),
                None => loose.push(*pattern),
            }
        }
        let mut estimate = 1.0_f64;
        for (_, group) in stars {
            estimate *= self.estimate_star(&group);
        }
        for pattern in loose {
            estimate *= self.estimate_pattern(&pattern);
        }
        estimate
    }
}

// --- the snapshot format -----------------------------------------------------------------
//
// Big-endian, fixed-width, versioned by a leading byte, and nothing else. `by_predicate` is
// not written: it is derived from the sets and is rebuilt on read, which keeps the format
// from being able to disagree with itself.
//
// Sized by the schema, not the data: a store with fourteen predicates and two shapes is a
// few hundred bytes; one with ten thousand predicates and a hundred thousand shapes is some
// megabytes. Either is one key in the default family.

/// Bumped when the layout changes. A snapshot with another version is simply not current.
const SNAPSHOT_VERSION: u8 = 1;

impl Statistics {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(SNAPSHOT_VERSION);
        put_u64(&mut out, self.generation);
        put_u64(&mut out, self.total_triples);
        put_u64(&mut out, self.total_subjects);

        put_u32(&mut out, self.predicates.len());
        for (predicate, stats) in &self.predicates {
            put_u64(&mut out, predicate.to_raw());
            put_u64(&mut out, stats.triples);
            put_u64(&mut out, stats.subjects);
            put_u64(&mut out, stats.objects);
        }

        put_u32(&mut out, self.sets.len());
        for set in &self.sets {
            put_u32(&mut out, set.predicates.len());
            for predicate in &set.predicates {
                put_u64(&mut out, predicate.to_raw());
            }
            put_u64(&mut out, set.subjects);
            put_u32(&mut out, set.occurrences.len());
            for (predicate, n) in &set.occurrences {
                put_u64(&mut out, predicate.to_raw());
                put_u64(&mut out, *n);
            }
        }
        out
    }

    /// `None` for anything that is not exactly a snapshot this version wrote.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut at = Cursor { bytes, pos: 0 };
        if at.u8()? != SNAPSHOT_VERSION {
            return None;
        }
        let mut stats = Self {
            generation: at.u64()?,
            total_triples: at.u64()?,
            total_subjects: at.u64()?,
            ..Self::default()
        };

        for _ in 0..at.u32()? {
            let predicate = TermId::from_raw(at.u64()?);
            let entry = PredicateStats {
                triples: at.u64()?,
                subjects: at.u64()?,
                objects: at.u64()?,
            };
            stats.predicates.insert(predicate, entry);
        }

        for _ in 0..at.u32()? {
            let mut predicates = Vec::new();
            for _ in 0..at.u32()? {
                predicates.push(TermId::from_raw(at.u64()?));
            }
            let subjects = at.u64()?;
            let mut occurrences = FxHashMap::default();
            for _ in 0..at.u32()? {
                let predicate = TermId::from_raw(at.u64()?);
                occurrences.insert(predicate, at.u64()?);
            }
            let index = stats.sets.len();
            for predicate in &predicates {
                stats
                    .by_predicate
                    .entry(*predicate)
                    .or_default()
                    .push(index);
            }
            stats.sets.push(CharacteristicSet {
                predicates,
                subjects,
                occurrences,
            });
        }

        // Trailing bytes mean this is not the snapshot it claims to be.
        (at.pos == bytes.len()).then_some(stats)
    }
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: usize) {
    // Lengths of in-memory collections; a count past `u32::MAX` is not a real case and is
    // saturated rather than truncated so that it can never decode as a small number.
    out.extend_from_slice(&u32::try_from(v).unwrap_or(u32::MAX).to_be_bytes());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let slice = self.bytes.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes)
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

#[cfg(test)]
mod tests {
    // These compare estimates that are exact by construction (0.0 for "no subject
    // has both predicates", a table constant), so an epsilon would weaken the test.
    #![allow(clippy::float_cmp)]
    use super::*;
    use oxrdf::vocab::rdf;
    use oxrdf::{GraphName, Literal, NamedNode, Quad};

    fn ex(name: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{name}"))
    }

    /// Two kinds of entity that share no predicates beyond `rdf:type`.
    fn store() -> Store {
        let mut store = Store::new();
        let mut add = |s: NamedNode, p: NamedNode, o: oxrdf::Term| {
            store
                .insert(
                    Quad {
                        subject: s.into(),
                        predicate: p,
                        object: o,
                        graph_name: GraphName::DefaultGraph,
                    }
                    .as_ref(),
                )
                .unwrap();
        };
        for i in 0..100 {
            let s = ex(&format!("person{i}"));
            add(s.clone(), rdf::TYPE.into_owned(), ex("Person").into());
            add(
                s.clone(),
                ex("name"),
                Literal::new_simple_literal(format!("P{i}")).into(),
            );
            add(
                s,
                ex("email"),
                Literal::new_simple_literal(format!("p{i}@x")).into(),
            );
        }
        for i in 0..10 {
            let s = ex(&format!("org{i}"));
            add(s.clone(), rdf::TYPE.into_owned(), ex("Org").into());
            add(
                s,
                ex("legalName"),
                Literal::new_simple_literal(format!("O{i}")).into(),
            );
        }
        store
    }

    fn id(store: &Store, node: &NamedNode) -> TermId {
        store.lookup_term(node.as_ref().into()).unwrap().unwrap()
    }

    #[test]
    fn characteristic_sets_find_the_shapes_in_the_data() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).unwrap();
        assert_eq!(
            stats.shape_count(),
            2,
            "there are exactly two kinds of subject"
        );
        assert_eq!(stats.total_subjects(), 110);
        assert_eq!(stats.total_triples(), 320);
    }

    #[test]
    fn a_single_pattern_uses_real_counts() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).unwrap();
        let name = id(&store, &ex("name"));
        let legal = id(&store, &ex("legalName"));

        // The reused optimiser would call both of these 10,000.
        assert!(
            (stats.estimate_pattern(&Pattern::single(None, Some(name), None)) - 100.0).abs() < 1.0
        );
        assert!(
            (stats.estimate_pattern(&Pattern::single(None, Some(legal), None)) - 10.0).abs() < 1.0
        );
    }

    #[test]
    fn a_star_over_co_occurring_predicates_is_accurate() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).unwrap();
        let name = id(&store, &ex("name"));
        let email = id(&store, &ex("email"));

        // Every person has exactly one name and one email, so the star is 100 rows.
        let estimate =
            stats.estimate_star(&[Pattern::star(0, name, None), Pattern::star(0, email, None)]);
        assert!(
            (estimate - 100.0).abs() < 1.0,
            "expected about 100 rows, estimated {estimate}"
        );
    }

    #[test]
    fn a_star_over_predicates_that_never_co_occur_estimates_zero() {
        // This is the case an independence assumption gets badly wrong: multiplying two
        // selectivities gives a healthy-looking number for a join that has no answers.
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).unwrap();
        let email = id(&store, &ex("email"));
        let legal = id(&store, &ex("legalName"));

        let estimate =
            stats.estimate_star(&[Pattern::star(0, email, None), Pattern::star(0, legal, None)]);
        assert_eq!(
            estimate, 0.0,
            "no subject has both an email and a legalName"
        );

        // What multiplying selectivities would have said, for contrast.
        let naive = stats.estimate_pattern(&Pattern::single(None, Some(email), None))
            * stats.estimate_pattern(&Pattern::single(None, Some(legal), None));
        assert!(naive > 900.0, "the naive estimate is {naive}, not zero");
    }

    /// A subject with several objects for one predicate — the case that tells a *subject*
    /// count from a triple count.
    ///
    /// `store()` above gives every subject exactly one object per predicate, so `triples`,
    /// `subjects` and `objects` come out equal for all four and a count that returned any of
    /// the three would pass. That blindness was real: distinct subjects are now counted from
    /// the `spo` scan order rather than collected into a set, and the whole suite passed with
    /// the increment doubled. This separates the three.
    #[test]
    fn distinct_subjects_and_objects_are_counted_per_predicate() {
        let email = ex("email");
        let mut store = Store::new();
        {
            let mut add = |s: NamedNode, p: NamedNode, o: oxrdf::Term| {
                store
                    .insert(
                        Quad {
                            subject: s.into(),
                            predicate: p,
                            object: o,
                            graph_name: GraphName::DefaultGraph,
                        }
                        .as_ref(),
                    )
                    .unwrap();
            };
            let shared = Literal::new_simple_literal("shared@x");
            // Alice has three addresses, Bob one, and one of Alice's is also Bob's. So the
            // three counts are four triples, two subjects and three objects — all different.
            add(ex("alice"), email.clone(), shared.clone().into());
            add(
                ex("alice"),
                email.clone(),
                Literal::new_simple_literal("a2@x").into(),
            );
            add(
                ex("alice"),
                email.clone(),
                Literal::new_simple_literal("a3@x").into(),
            );
            add(ex("bob"), email.clone(), shared.into());
            add(
                ex("alice"),
                ex("name"),
                Literal::new_simple_literal("Alice").into(),
            );
        }

        let stats = Statistics::build(&store, GraphFilter::Default).unwrap();
        let counts = stats.predicates[&id(&store, &email)];
        assert_eq!(counts.triples, 4, "four email triples");
        assert_eq!(counts.subjects, 2, "over two subjects");
        assert_eq!(
            counts.objects, 3,
            "with three distinct addresses between them"
        );

        // And a predicate on one subject only, so a count that leaked across predicates shows.
        let name = stats.predicates[&id(&store, &ex("name"))];
        assert_eq!((name.triples, name.subjects, name.objects), (1, 1, 1));
    }

    /// The snapshot must give back exactly the statistics that went in — every count, every
    /// shape, and the index derived from the shapes — because a snapshot that drifted would
    /// make estimates wrong in a way nothing downstream can detect.
    #[test]
    fn a_snapshot_round_trips_exactly() {
        let store = store();
        let built = Statistics::build(&store, GraphFilter::Default).unwrap();
        let back = Statistics::from_bytes(&built.to_bytes()).expect("decodes");

        assert_eq!(back.generation, built.generation);
        assert_eq!(back.total_triples, built.total_triples);
        assert_eq!(back.total_subjects, built.total_subjects);
        assert_eq!(back.predicates.len(), built.predicates.len());
        for (p, s) in &built.predicates {
            let b = back.predicates[p];
            assert_eq!(
                (b.triples, b.subjects, b.objects),
                (s.triples, s.subjects, s.objects)
            );
        }
        assert_eq!(back.sets.len(), built.sets.len());
        for (a, b) in built.sets.iter().zip(&back.sets) {
            assert_eq!(a.predicates, b.predicates);
            assert_eq!(a.subjects, b.subjects);
            assert_eq!(a.occurrences, b.occurrences);
        }
        assert_eq!(
            back.by_predicate, built.by_predicate,
            "the index is rebuilt from the sets"
        );

        // And the estimates, which is what any of it is for.
        let name = id(&store, &ex("name"));
        let email = id(&store, &ex("email"));
        let star = [Pattern::star(0, name, None), Pattern::star(0, email, None)];
        assert_eq!(back.estimate_star(&star), built.estimate_star(&star));
    }

    /// Anything that is not a snapshot this version wrote is `None`, never a partial or a
    /// panic: a cache is discarded, not trusted and not reported.
    #[test]
    fn a_snapshot_that_is_not_one_is_refused() {
        let store = store();
        let good = Statistics::build(&store, GraphFilter::Default)
            .unwrap()
            .to_bytes();

        assert!(Statistics::from_bytes(&[]).is_none(), "empty");
        let mut wrong_version = good.clone();
        wrong_version[0] = SNAPSHOT_VERSION.wrapping_add(1);
        assert!(
            Statistics::from_bytes(&wrong_version).is_none(),
            "another version"
        );
        assert!(
            Statistics::from_bytes(&good[..good.len() / 2]).is_none(),
            "truncated"
        );
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(
            Statistics::from_bytes(&trailing).is_none(),
            "trailing bytes"
        );
        assert!(
            Statistics::from_bytes(&good).is_some(),
            "and the real one still decodes"
        );
    }

    /// The whole point: a snapshot is used while the store is unchanged and rebuilt the
    /// moment it is not. Both directions, because a cache that is never invalidated and one
    /// that is never used are each a bug that passes a round-trip test.
    #[test]
    fn a_snapshot_is_used_until_a_write_and_not_after() {
        let mut store = store();
        assert!(
            Statistics::load_cached(&store, GraphFilter::Default)
                .unwrap()
                .is_none(),
            "nothing kept yet"
        );

        let first = Statistics::cached(&mut store, GraphFilter::Default).unwrap();
        assert!(
            store.load_statistics().unwrap().is_some(),
            "cached() kept a snapshot"
        );
        let loaded = Statistics::load_cached(&store, GraphFilter::Default)
            .unwrap()
            .expect("the store is unchanged, so the snapshot is current");
        assert_eq!(loaded.generation, first.generation);
        assert_eq!(loaded.total_triples, first.total_triples);

        // One write, and the snapshot no longer describes the store.
        store
            .insert(
                Quad {
                    subject: ex("newcomer").into(),
                    predicate: ex("name"),
                    object: Literal::new_simple_literal("N").into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .unwrap();
        assert!(
            Statistics::load_cached(&store, GraphFilter::Default)
                .unwrap()
                .is_none(),
            "a write moved the generation, so the snapshot must be refused"
        );

        // A stale snapshot is not saved either: `save` re-checks.
        assert!(
            !first.save(&mut store).unwrap(),
            "stale statistics are not kept"
        );

        let second = Statistics::cached(&mut store, GraphFilter::Default).unwrap();
        assert_eq!(
            second.total_triples,
            first.total_triples + 1,
            "rebuilt from the store"
        );
        assert!(
            Statistics::load_cached(&store, GraphFilter::Default)
                .unwrap()
                .is_some(),
            "and kept again"
        );
    }

    #[test]
    fn an_unknown_predicate_estimates_nothing() {
        let store = store();
        let stats = Statistics::build(&store, GraphFilter::Default).unwrap();
        let unseen = TermId::new(holos_core::Tag::Iri, 999_999);
        assert_eq!(
            stats.estimate_pattern(&Pattern::single(None, Some(unseen), None)),
            0.0
        );
    }
}
