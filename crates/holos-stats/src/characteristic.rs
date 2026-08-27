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

use crate::Pattern;
use holos_core::TermId;
use holos_store::{GraphFilter, Result, Store};
use rustc_hash::{FxHashMap, FxHashSet};

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
}

impl Statistics {
    /// Builds statistics by scanning a graph.
    ///
    /// One pass in `spo` order, which is why the subject's predicates arrive together and
    /// a characteristic set can be closed without holding the whole graph in a map.
    ///
    /// Distinct counts are **exact**, not sketched. §7 specifies `HyperLogLog`, and that is
    /// the right answer at scale — but a sketch carries a couple of percent of error, and
    /// these numbers are about to be used to measure how accurate estimation *can* be. An
    /// approximation in the yardstick would muddy exactly the measurement it is there to
    /// support. HLL is a drop-in swap once the accuracy question is settled.
    ///
    /// # Errors
    ///
    /// Propagates any error raised while scanning the store.
    pub fn build(store: &Store, graph: GraphFilter) -> Result<Self> {
        let mut stats = Self::default();
        let mut distinct_subjects: FxHashMap<TermId, FxHashSet<TermId>> = FxHashMap::default();
        let mut distinct_objects: FxHashMap<TermId, FxHashSet<TermId>> = FxHashMap::default();

        // predicate-set -> (subject count, per-predicate occurrences)
        let mut shapes: FxHashMap<Vec<TermId>, (u64, FxHashMap<TermId, u64>)> =
            FxHashMap::default();

        let mut current_subject: Option<TermId> = None;
        let mut current: FxHashMap<TermId, u64> = FxHashMap::default();

        let close = |subject: Option<TermId>,
                     current: &mut FxHashMap<TermId, u64>,
                     shapes: &mut FxHashMap<Vec<TermId>, (u64, FxHashMap<TermId, u64>)>,
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
            }
            *total_subjects += 1;
            current.clear();
        };

        for quad in store.quads_for_pattern(None, None, None, graph) {
            let quad = quad?;
            stats.total_triples += 1;

            let entry = stats.predicates.entry(quad.predicate).or_default();
            entry.triples += 1;
            distinct_subjects
                .entry(quad.predicate)
                .or_default()
                .insert(quad.subject);
            distinct_objects
                .entry(quad.predicate)
                .or_default()
                .insert(quad.object);

            if current_subject != Some(quad.subject) {
                close(
                    current_subject,
                    &mut current,
                    &mut shapes,
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
            &mut stats.total_subjects,
        );

        for (predicate, subjects) in distinct_subjects {
            stats.predicates.entry(predicate).or_default().subjects = subjects.len() as u64;
        }
        for (predicate, objects) in distinct_objects {
            stats.predicates.entry(predicate).or_default().objects = objects.len() as u64;
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
