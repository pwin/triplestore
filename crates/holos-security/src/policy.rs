//! Fine-grained access policy, and the compilation that makes it affordable.
//!
//! # Where enforcement happens, and why it is the only place
//!
//! Every read in HOLOS reaches the data through one function —
//! [`Store::quads_for_pattern`](holos_store::Store::quads_for_pattern), wrapped by the
//! dataset view. Policy is applied *there*, at that single chokepoint, and nowhere else.
//!
//! That is the whole design. Enforcing by rewriting SPARQL is the common alternative and
//! it is a leak generator: there is always an operator the rewrite forgot — a property
//! path, a `MINUS`, a `NOT EXISTS`, a subquery, an aggregate over a hidden row. Filtering
//! at the scan gives a property that is easy to state and easy to believe:
//!
//! > For a principal `A` under policy `P`, the answer to any query `Q` equals the answer
//! > to `Q` evaluated over the sub-dataset `A` is permitted to see.
//!
//! Nothing above the scan can violate it, because nothing above the scan can see a quad
//! the scan did not yield. The property does have preconditions, and they are listed in
//! `DESIGN.md` §14 — statistics, materialised inference and projections each need their
//! own rule, because each is a way for a hidden quad to influence a visible answer.
//!
//! # Why this is affordable
//!
//! Evaluating rules per quad would be far too slow. Instead a [`Policy`] is *compiled*
//! against a store and a principal into [`CompiledPolicy`]: sets of dense [`TermId`]s.
//! Checking a quad is then a couple of integer set lookups. This is the dense-identifier
//! decision from `DESIGN.md` §5 paying off a second time.

use crate::principal::{Label, Principal};
use holos_core::TermId;
use holos_store::{EncodedQuad, Result, Store};
use oxrdf::NamedNode;
use rustc_hash::{FxHashMap, FxHashSet};

/// What a rule does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Permit the access.
    Allow,
    /// Refuse it. Deny always beats allow at the same specificity.
    Deny,
}

/// What happens to data a principal may not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantics {
    /// Hidden quads simply do not exist for this principal. Queries succeed and return
    /// the answer over the visible sub-dataset. Safe and composable; the cost is that a
    /// principal cannot tell an incomplete answer from a complete one.
    Filter,
    /// Touching forbidden data raises an error instead of silently narrowing the answer.
    /// Correct where a partial answer would be worse than no answer — reconciliation,
    /// regulatory reporting — at the cost of leaking the *existence* of hidden data.
    Fail,
}

/// The operations a rule can govern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes(u8);

impl Modes {
    /// Read quads.
    pub const READ: Self = Self(1);
    /// Insert or delete quads.
    pub const WRITE: Self = Self(2);
    /// Run validation, which can reveal shape structure.
    pub const VALIDATE: Self = Self(4);
    /// Change the boundary — shapes, rules, or the policy itself.
    ///
    /// Deliberately separate from [`Modes::WRITE`]. Authority to change the rules is not
    /// the same as authority to change the data, and conflating them is how a data-entry
    /// role becomes a privilege-escalation path.
    pub const ADMIN: Self = Self(8);
    /// Every mode.
    pub const ALL: Self = Self(15);

    /// Union of two mode sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether this set includes another.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// What a rule applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole dataset.
    Everything,
    /// One named graph — in holon terms, one holon's scene.
    Graph(NamedNode),
    /// One predicate, in every graph. The usual way to hide a sensitive column.
    Predicate(NamedNode),
    /// One predicate within one graph.
    GraphPredicate(NamedNode, NamedNode),
}

impl Scope {
    /// How specific this scope is. Higher wins.
    const fn specificity(&self) -> u8 {
        match self {
            Self::Everything => 0,
            Self::Graph(_) => 1,
            Self::Predicate(_) => 2,
            Self::GraphPredicate(_, _) => 3,
        }
    }
}

/// Which principals a rule applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalMatch {
    /// Everyone, including the anonymous principal.
    Everyone,
    /// Anyone holding a role or group.
    Role(String),
    /// Anyone with a claim value — the attribute-based case.
    Attribute {
        /// Claim name.
        key: String,
        /// Required value.
        value: String,
    },
    /// One specific identity.
    Identity(NamedNode),
    /// Every sub-match must hold.
    All(Vec<PrincipalMatch>),
    /// At least one sub-match must hold.
    Any(Vec<PrincipalMatch>),
    /// Everyone the sub-match does *not* select.
    ///
    /// Needed because "deny this to everyone except role R" is the single most common
    /// shape a real policy takes, and specificity cannot express it: a deny and an allow
    /// written at the same scope resolve deny-first by design, so the exception has to
    /// live in the principal match rather than in the scope.
    Not(Box<PrincipalMatch>),
}

impl PrincipalMatch {
    /// Whether this matches a principal.
    #[must_use]
    pub fn matches(&self, principal: &Principal) -> bool {
        match self {
            Self::Everyone => true,
            Self::Role(r) => principal.has_role(r),
            Self::Attribute { key, value } => principal.has_attribute(key, value),
            Self::Identity(id) => principal.id == *id,
            Self::All(subs) => subs.iter().all(|s| s.matches(principal)),
            Self::Any(subs) => subs.iter().any(|s| s.matches(principal)),
            Self::Not(sub) => !sub.matches(principal),
        }
    }
}

/// One access rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Allow or deny.
    pub effect: Effect,
    /// Which operations.
    pub modes: Modes,
    /// What data.
    pub scope: Scope,
    /// Which principals.
    pub applies_to: PrincipalMatch,
}

impl Rule {
    /// An allow rule.
    #[must_use]
    pub fn allow(modes: Modes, scope: Scope, applies_to: PrincipalMatch) -> Self {
        Self {
            effect: Effect::Allow,
            modes,
            scope,
            applies_to,
        }
    }

    /// A deny rule.
    #[must_use]
    pub fn deny(modes: Modes, scope: Scope, applies_to: PrincipalMatch) -> Self {
        Self {
            effect: Effect::Deny,
            modes,
            scope,
            applies_to,
        }
    }
}

/// A complete policy for a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// What happens when no rule matches. Should be [`Effect::Deny`] in anything but a
    /// development store.
    pub default_effect: Effect,
    /// Filter or fail.
    pub semantics: Semantics,
    /// The rules, in no significant order — specificity decides, not position.
    pub rules: Vec<Rule>,
    /// Classification labels attached to named graphs. A principal must be cleared for a
    /// graph's label *in addition to* passing the rules.
    pub graph_labels: Vec<(NamedNode, Label)>,
    /// Label carried by the default graph.
    pub default_graph_label: Label,
}

impl Default for Policy {
    /// Deny everything. The only safe default.
    fn default() -> Self {
        Self {
            default_effect: Effect::Deny,
            semantics: Semantics::Filter,
            rules: Vec::new(),
            graph_labels: Vec::new(),
            default_graph_label: Label::default(),
        }
    }
}

impl Policy {
    /// A policy that permits everything, for embedding HOLOS where the host application
    /// is the only caller and does its own authorization.
    #[must_use]
    pub fn permit_all() -> Self {
        Self {
            default_effect: Effect::Allow,
            ..Self::default()
        }
    }

    /// Adds a rule.
    #[must_use]
    pub fn with_rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Labels a named graph.
    #[must_use]
    pub fn with_graph_label(mut self, graph: NamedNode, label: Label) -> Self {
        self.graph_labels.push((graph, label));
        self
    }

    /// Sets the semantics.
    #[must_use]
    pub fn with_semantics(mut self, semantics: Semantics) -> Self {
        self.semantics = semantics;
        self
    }

    /// Compiles this policy for one principal against one store.
    ///
    /// Resolution to [`TermId`]s is what makes enforcement cheap, and it is also why the
    /// result can go stale: an IRI a rule names may not be in the dictionary yet. See
    /// [`CompiledPolicy::is_stale`].
    ///
    /// A storage failure is propagated rather than swallowed. Treating an unreadable
    /// dictionary as "no rule matched" would fail permissive, which is the one direction
    /// an authorization decision must never fail in.
    pub fn compile(&self, store: &Store, principal: &Principal) -> Result<CompiledPolicy> {
        let mut graph: FxHashMap<TermId, (u8, Effect, Modes)> = FxHashMap::default();
        let mut predicate: FxHashMap<TermId, (u8, Effect, Modes)> = FxHashMap::default();
        let mut graph_predicate: FxHashMap<(TermId, TermId), (u8, Effect, Modes)> =
            FxHashMap::default();
        let mut everything: Option<(u8, Effect, Modes)> = None;
        let mut unresolved = Vec::new();

        let resolve = |n: &NamedNode| store.lookup_term(n.as_ref().into());

        for rule in self.rules.iter().filter(|r| r.applies_to.matches(principal)) {
            let entry = (rule.scope.specificity(), rule.effect, rule.modes);
            match &rule.scope {
                Scope::Everything => merge(&mut everything, entry),
                Scope::Graph(g) => match resolve(g)? {
                    Some(id) => merge_map(&mut graph, id, entry),
                    None => unresolved.push(g.clone()),
                },
                Scope::Predicate(p) => match resolve(p)? {
                    Some(id) => merge_map(&mut predicate, id, entry),
                    None => unresolved.push(p.clone()),
                },
                Scope::GraphPredicate(g, p) => match (resolve(g)?, resolve(p)?) {
                    (Some(g), Some(p)) => merge_map(&mut graph_predicate, (g, p), entry),
                    _ => {
                        unresolved.push(g.clone());
                        unresolved.push(p.clone());
                    }
                },
            }
        }

        let mut labels = FxHashMap::default();
        let mut forbidden_graphs = FxHashSet::default();
        for (g, label) in &self.graph_labels {
            if let Some(id) = resolve(g)? {
                if !principal.is_cleared_for(label) {
                    forbidden_graphs.insert(id);
                }
                labels.insert(id, label.clone());
            } else {
                unresolved.push(g.clone());
            }
        }

        Ok(CompiledPolicy {
            default_effect: self.default_effect,
            semantics: self.semantics,
            everything,
            graph,
            predicate,
            graph_predicate,
            forbidden_graphs,
            default_graph_forbidden: !principal.is_cleared_for(&self.default_graph_label),
            compiled_at_dictionary_len: store.dictionary_len(),
            unresolved,
        })
    }
}

fn merge(slot: &mut Option<(u8, Effect, Modes)>, entry: (u8, Effect, Modes)) {
    match slot {
        None => *slot = Some(entry),
        Some(existing) => *existing = resolve_conflict(*existing, entry),
    }
}

fn merge_map<K: std::hash::Hash + Eq>(
    map: &mut FxHashMap<K, (u8, Effect, Modes)>,
    key: K,
    entry: (u8, Effect, Modes),
) {
    map.entry(key)
        .and_modify(|existing| *existing = resolve_conflict(*existing, entry))
        .or_insert(entry);
}

/// Two rules at the same scope: deny wins, and the mode sets union.
fn resolve_conflict(
    a: (u8, Effect, Modes),
    b: (u8, Effect, Modes),
) -> (u8, Effect, Modes) {
    match (a.1, b.1) {
        (Effect::Deny, Effect::Deny) => (a.0, Effect::Deny, a.2.union(b.2)),
        (Effect::Deny, Effect::Allow) => a,
        (Effect::Allow, Effect::Deny) => b,
        (Effect::Allow, Effect::Allow) => (a.0, Effect::Allow, a.2.union(b.2)),
    }
}

/// A policy resolved against a store's dictionary, for one principal.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    default_effect: Effect,
    semantics: Semantics,
    everything: Option<(u8, Effect, Modes)>,
    graph: FxHashMap<TermId, (u8, Effect, Modes)>,
    predicate: FxHashMap<TermId, (u8, Effect, Modes)>,
    graph_predicate: FxHashMap<(TermId, TermId), (u8, Effect, Modes)>,
    forbidden_graphs: FxHashSet<TermId>,
    default_graph_forbidden: bool,
    compiled_at_dictionary_len: usize,
    unresolved: Vec<NamedNode>,
}

/// What the policy decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Permitted.
    Allow,
    /// Not permitted; omit it from the answer.
    Filter,
    /// Not permitted; abort the query.
    Fail,
}

impl CompiledPolicy {
    /// A compiled policy that permits everything. For tests and single-tenant embedding.
    #[must_use]
    pub fn permit_all() -> Self {
        Self {
            default_effect: Effect::Allow,
            semantics: Semantics::Filter,
            everything: None,
            graph: FxHashMap::default(),
            predicate: FxHashMap::default(),
            graph_predicate: FxHashMap::default(),
            forbidden_graphs: FxHashSet::default(),
            default_graph_forbidden: false,
            compiled_at_dictionary_len: usize::MAX,
            unresolved: Vec::new(),
        }
    }

    /// Whether the store has grown since this policy was compiled.
    ///
    /// A rule naming an IRI the dictionary had never seen could not be resolved to an id,
    /// so it enforces nothing. If that IRI arrives later, the compiled policy is *less
    /// restrictive than the policy it came from* — the dangerous direction. Callers must
    /// recompile when this returns `true`; [`crate::Session`] does it automatically.
    #[must_use]
    pub fn is_stale(&self, store: &Store) -> bool {
        !self.unresolved.is_empty()
            && store.dictionary_len() != self.compiled_at_dictionary_len
    }

    /// IRIs the policy names that the store had not seen at compile time.
    #[must_use]
    pub fn unresolved(&self) -> &[NamedNode] {
        &self.unresolved
    }

    /// The configured semantics.
    #[must_use]
    pub fn semantics(&self) -> Semantics {
        self.semantics
    }

    /// Decides one quad.
    #[must_use]
    pub fn decide_quad(&self, quad: EncodedQuad, mode: Modes) -> Decision {
        // Clearance is checked first and is not overridable by a rule: a label says the
        // principal is not permitted to know the data exists, and no allow rule elsewhere
        // in the policy should be able to undo that.
        let cleared = match quad.graph_name {
            None => !self.default_graph_forbidden,
            Some(g) => !self.forbidden_graphs.contains(&g),
        };
        if !cleared {
            return self.refusal();
        }

        // Most specific match wins; deny beats allow at equal specificity, which
        // resolve_conflict has already applied within each level.
        let mut best: Option<(u8, Effect)> = None;
        let mut consider = |slot: Option<(u8, Effect, Modes)>| {
            if let Some((spec, effect, modes)) = slot {
                if modes.contains(mode) && best.is_none_or(|(s, _)| spec >= s) {
                    best = Some((spec, effect));
                }
            }
        };
        // Each `get` on an empty map is still a hash and a probe, so the emptiness check
        // is worth making explicit rather than leaving to the hasher.
        consider(self.everything);
        if let Some(g) = quad.graph_name {
            if !self.graph.is_empty() {
                consider(self.graph.get(&g).copied());
            }
        }
        if !self.predicate.is_empty() {
            consider(self.predicate.get(&quad.predicate).copied());
        }
        if !self.graph_predicate.is_empty() {
            if let Some(g) = quad.graph_name {
                consider(self.graph_predicate.get(&(g, quad.predicate)).copied());
            }
        }

        match best.map_or(self.default_effect, |(_, e)| e) {
            Effect::Allow => Decision::Allow,
            Effect::Deny => self.refusal(),
        }
    }

    /// Whether a quad is permitted, treating [`Semantics::Fail`] as "not permitted".
    ///
    /// Convenience for the write path, where there is no answer to narrow and a refusal
    /// is always an error.
    #[must_use]
    pub fn permits_quad(&self, quad: EncodedQuad, mode: Modes) -> bool {
        self.decide_quad(quad, mode) == Decision::Allow
    }

    /// Whether an entire graph can be skipped without examining its quads.
    ///
    /// A scan over a graph the principal cannot see at all should not read it, both for
    /// speed and so that a `Fail`-semantics policy fails on the graph rather than on the
    /// first unlucky quad.
    #[must_use]
    pub fn graph_is_wholly_denied(&self, graph: Option<TermId>) -> bool {
        let cleared = match graph {
            None => !self.default_graph_forbidden,
            Some(g) => !self.forbidden_graphs.contains(&g),
        };
        if !cleared {
            return true;
        }
        // Only safe to conclude "wholly denied" when nothing more specific could
        // re-allow part of it.
        let graph_effect = graph
            .and_then(|g| self.graph.get(&g).copied())
            .map(|(_, e, _)| e);
        let no_finer_allows = self
            .predicate
            .values()
            .chain(self.graph_predicate.values())
            .all(|(_, e, _)| *e == Effect::Deny);
        matches!(graph_effect, Some(Effect::Deny)) && no_finer_allows
    }

    fn refusal(&self) -> Decision {
        match self.semantics {
            Semantics::Filter => Decision::Filter,
            Semantics::Fail => Decision::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{GraphName, Quad};

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{s}"))
    }

    /// A store with a public graph, a restricted graph, and a sensitive predicate.
    fn fixture() -> Store {
        let mut store = Store::new();
        for (s, p, o, g) in [
            ("alice", "name", "a", Some("public")),
            ("alice", "salary", "100", Some("public")),
            ("bob", "name", "b", Some("hr")),
            ("bob", "salary", "200", Some("hr")),
            ("carol", "name", "c", None),
        ] {
            store
                .insert(
                    Quad {
                    subject: nn(s).into(),
                    predicate: nn(p),
                    object: nn(o).into(),
                        graph_name: g.map_or(GraphName::DefaultGraph, |g| nn(g).into()),
                    }
                    .as_ref(),
                )
                .unwrap();
        }
        store
    }

    fn visible(store: &Store, compiled: &CompiledPolicy) -> Vec<String> {
        let mut v: Vec<_> = store
            .quads_for_pattern(None, None, None, holos_store::GraphFilter::Any)
            .map(std::result::Result::unwrap)
            .filter(|q| compiled.permits_quad(*q, Modes::READ))
            .map(|q| store.decode_quad(q).unwrap())
            .map(|q| format!("{} {}", q.subject, q.predicate))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn default_is_deny_everything() {
        let store = fixture();
        let p = Principal::anonymous();
        let compiled = Policy::default().compile(&store, &p).unwrap();
        assert_eq!(visible(&store, &compiled), Vec::<String>::new());
    }

    #[test]
    fn graph_level_grant() {
        let store = fixture();
        let p = Principal::anonymous().with_role("reader");
        let compiled = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Graph(nn("public")),
                PrincipalMatch::Role("reader".into()),
            ))
            .compile(&store, &p).unwrap();
        let seen = visible(&store, &compiled);
        assert_eq!(seen.len(), 2, "only the public graph: {seen:?}");
        assert!(seen.iter().all(|s| s.contains("alice")));
    }

    #[test]
    fn predicate_deny_overrides_a_broader_allow() {
        // The canonical fine-grained case: read the graph, but never the salary column.
        let store = fixture();
        let p = Principal::anonymous().with_role("reader");
        let compiled = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Everything,
                PrincipalMatch::Everyone,
            ))
            .with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Everyone,
            ))
            .compile(&store, &p).unwrap();
        let seen = visible(&store, &compiled);
        assert_eq!(seen.len(), 3, "three names, no salaries: {seen:?}");
        assert!(!seen.iter().any(|s| s.contains("salary")));
    }

    #[test]
    fn a_more_specific_allow_beats_a_broader_deny() {
        // HR may see salaries, but only inside the hr graph.
        let store = fixture();
        let p = Principal::anonymous().with_role("hr");
        let compiled = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Everything,
                PrincipalMatch::Everyone,
            ))
            .with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Everyone,
            ))
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::GraphPredicate(nn("hr"), nn("salary")),
                PrincipalMatch::Role("hr".into()),
            ))
            .compile(&store, &p).unwrap();
        let seen = visible(&store, &compiled);
        assert!(
            seen.contains(&format!("{} {}", nn("bob"), nn("salary"))),
            "hr salary must be visible to hr: {seen:?}"
        );
        assert!(
            !seen.contains(&format!("{} {}", nn("alice"), nn("salary"))),
            "the public-graph salary is still denied: {seen:?}"
        );
    }

    #[test]
    fn deny_beats_allow_at_equal_specificity() {
        let store = fixture();
        let p = Principal::anonymous().with_role("a").with_role("b");
        let compiled = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Role("a".into()),
            ))
            .with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Role("b".into()),
            ))
            .compile(&store, &p).unwrap();
        assert!(!visible(&store, &compiled).iter().any(|s| s.contains("salary")));
    }

    #[test]
    fn modes_are_independent() {
        let store = fixture();
        let p = Principal::anonymous().with_role("reader");
        let compiled = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Everything,
                PrincipalMatch::Everyone,
            ))
            .compile(&store, &p).unwrap();
        let quad = store
            .quads_for_pattern(None, None, None, holos_store::GraphFilter::Any)
            .next()
            .unwrap()
            .unwrap();
        assert!(compiled.permits_quad(quad, Modes::READ));
        assert!(
            !compiled.permits_quad(quad, Modes::WRITE),
            "a read grant must not imply a write grant"
        );
        assert!(
            !compiled.permits_quad(quad, Modes::ADMIN),
            "and certainly not an admin grant"
        );
    }

    #[test]
    fn clearance_is_not_overridable_by_a_rule() {
        // Even an allow-everything rule must not defeat a classification label.
        let store = fixture();
        let p = Principal::anonymous().with_role("reader");
        let compiled = Policy::permit_all()
            .with_graph_label(nn("hr"), Label::level(3))
            .compile(&store, &p).unwrap();
        let seen = visible(&store, &compiled);
        assert!(
            !seen.iter().any(|s| s.contains("bob")),
            "uncleared principal saw labelled data: {seen:?}"
        );
        assert!(seen.iter().any(|s| s.contains("alice")));

        let cleared = Principal::anonymous().with_clearance(Label::level(3));
        let compiled = Policy::permit_all()
            .with_graph_label(nn("hr"), Label::level(3))
            .compile(&store, &cleared).unwrap();
        assert!(visible(&store, &compiled).iter().any(|s| s.contains("bob")));
    }

    #[test]
    fn attribute_based_rules_work() {
        let store = fixture();
        let finance = Principal::anonymous().with_attribute("department", "finance");
        let other = Principal::anonymous().with_attribute("department", "sales");
        let policy = Policy::default().with_rule(Rule::allow(
            Modes::READ,
            Scope::Everything,
            PrincipalMatch::Attribute {
                key: "department".into(),
                value: "finance".into(),
            },
        ));
        assert!(!visible(&store, &policy.compile(&store, &finance).unwrap()).is_empty());
        assert!(visible(&store, &policy.compile(&store, &other).unwrap()).is_empty());
    }

    #[test]
    fn an_exception_is_expressed_by_negating_the_principal_match() {
        // "Everyone may read; nobody except HR may read salaries." Writing the exception
        // as a same-scope allow would lose to the deny, so it belongs here.
        let store = fixture();
        let policy = Policy::default()
            .with_rule(Rule::allow(
                Modes::READ,
                Scope::Everything,
                PrincipalMatch::Everyone,
            ))
            .with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(nn("salary")),
                PrincipalMatch::Not(Box::new(PrincipalMatch::Role("hr".into()))),
            ));

        let outsider = Principal::anonymous();
        let seen = visible(&store, &policy.compile(&store, &outsider).unwrap());
        assert!(!seen.iter().any(|s| s.contains("salary")), "{seen:?}");
        assert_eq!(seen.len(), 3, "names are still visible: {seen:?}");

        let hr = Principal::anonymous().with_role("hr");
        let seen = visible(&store, &policy.compile(&store, &hr).unwrap());
        assert_eq!(seen.len(), 5, "HR sees everything: {seen:?}");
    }

    #[test]
    fn fail_semantics_reports_rather_than_narrows() {
        let store = fixture();
        let p = Principal::anonymous();
        let compiled = Policy::default()
            .with_semantics(Semantics::Fail)
            .compile(&store, &p).unwrap();
        let quad = store
            .quads_for_pattern(None, None, None, holos_store::GraphFilter::Any)
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(compiled.decide_quad(quad, Modes::READ), Decision::Fail);
    }

    #[test]
    fn a_policy_naming_unknown_iris_goes_stale_when_they_appear() {
        // The dangerous direction: a deny rule that resolved to nothing at compile time
        // would silently stop denying once the data arrives.
        let mut store = Store::new();
        store
            .insert(
                Quad {
                    subject: nn("a").into(),
                    predicate: nn("harmless"),
                    object: nn("x").into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .unwrap();
        let p = Principal::anonymous();
        let policy = Policy::permit_all().with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(nn("salary")),
            PrincipalMatch::Everyone,
        ));
        let compiled = policy.compile(&store, &p).unwrap();
        assert_eq!(compiled.unresolved(), &[nn("salary")]);
        assert!(!compiled.is_stale(&store));

        // Now the predicate the rule names actually shows up.
        store
            .insert(
                Quad {
                    subject: nn("a").into(),
                    predicate: nn("salary"),
                    object: nn("100").into(),
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .unwrap();
        assert!(
            compiled.is_stale(&store),
            "policy must be recompiled once its IRIs resolve"
        );
        let recompiled = policy.compile(&store, &p).unwrap();
        assert!(!visible(&store, &recompiled).iter().any(|s| s.contains("salary")));
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use holos_core::Tag;

    /// Gets an IRI into the dictionary, so a rule naming it resolves to an id.
    fn intern(store: &mut Store, iri: &NamedNode) {
        store
            .insert(
                oxrdf::Quad::new(
                    iri.clone(),
                    iri.clone(),
                    iri.clone(),
                    oxrdf::GraphName::DefaultGraph,
                )
                .as_ref(),
            )
            .expect("insert");
    }

    fn quad(g: Option<u64>, p: u64) -> EncodedQuad {
        EncodedQuad {
            subject: TermId::new(Tag::Iri, 1),
            predicate: TermId::new(Tag::Iri, p),
            object: TermId::new(Tag::Iri, 2),
            graph_name: g.map(|g| TermId::new(Tag::Iri, g)),
        }
    }

    /// Every policy shape decided across every mode, graph and predicate.
    ///
    /// Written while chasing a performance theory that turned out to be wrong —
    /// `decide_quad` costs 8 ns, not the 500 ns it had been blamed for — but kept, because
    /// a table of what each policy shape actually decides is worth having on its own.
    #[test]
    fn each_policy_shape_decides_as_specified() {
        let ex = |n: u32| NamedNode::new_unchecked(format!("http://example.com/{n}"));
        let mut store = Store::new();
        for n in 0..10 {
            intern(&mut store, &ex(n));
        }
        let principal = Principal::anonymous();
        let compile = |p: Policy| p.compile(&store, &principal).expect("compile");

        // permit-all allows everything, in every mode.
        let permissive = compile(Policy::permit_all());
        for mode in [Modes::READ, Modes::WRITE, Modes::ADMIN] {
            assert_eq!(permissive.decide_quad(quad(None, 1), mode), Decision::Allow);
            assert_eq!(permissive.decide_quad(quad(Some(3), 7), mode), Decision::Allow);
        }

        // deny-all filters everything.
        let restrictive = compile(Policy::default());
        assert_eq!(restrictive.decide_quad(quad(None, 1), Modes::READ), Decision::Filter);

        // ...and errors instead, under Fail semantics.
        let failing = compile(Policy::default().with_semantics(Semantics::Fail));
        assert_eq!(failing.decide_quad(quad(None, 1), Modes::READ), Decision::Fail);

        // A blanket rule scoped to one mode leaves the others on the default.
        let read_only = compile(Policy::default().with_rule(Rule::allow(
            Modes::READ,
            Scope::Everything,
            PrincipalMatch::Everyone,
        )));
        assert_eq!(read_only.decide_quad(quad(None, 1), Modes::READ), Decision::Allow);
        assert_eq!(read_only.decide_quad(quad(None, 1), Modes::WRITE), Decision::Filter);

        // A predicate rule beats the blanket default, and only for that predicate.
        let one_denied = compile(Policy::permit_all().with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(ex(7)),
            PrincipalMatch::Everyone,
        )));
        assert_eq!(
            one_denied.decide_quad(quad(None, term_of(&store, &ex(7))), Modes::READ),
            Decision::Filter
        );
        assert_eq!(
            one_denied.decide_quad(quad(None, term_of(&store, &ex(1))), Modes::READ),
            Decision::Allow
        );
    }

    fn term_of(store: &Store, iri: &NamedNode) -> u64 {
        store
            .lookup_term(iri.as_ref().into())
            .expect("lookup")
            .expect("interned")
            .payload()
    }
}
