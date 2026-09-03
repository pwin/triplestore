//! Who is asking.
//!
//! # Enterprise interoperability
//!
//! HOLOS does not authenticate anyone. Signature verification, token introspection,
//! Kerberos, mTLS and SAML all belong at the edge — an HTTP front door, a sidecar, or the
//! embedding application. What arrives here is an **already-verified** set of claims.
//!
//! That boundary is deliberate. It means HOLOS interoperates with whatever the enterprise
//! already runs (OIDC/OAuth2, LDAP groups, SPIFFE identities) by translating its claims
//! into a [`Principal`], and it keeps cryptography out of the query path.
//!
//! The translation is the interesting part: **a principal is RDF**. Claims become triples
//! in a principal graph, so an access rule can be written against a principal with the
//! same vocabulary used for everything else, and no second policy language is needed.

use oxrdf::{Literal, NamedNode, Quad, Term};
use std::collections::{BTreeMap, BTreeSet};

/// The HOLOS system vocabulary namespace.
pub const NS: &str = "https://holos.rdf/ns#";

/// Builds a term in the HOLOS system vocabulary.
#[must_use]
pub fn holos(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS}{local}"))
}

/// A classification label drawn from a lattice.
///
/// Modelled as a level plus a set of compartments, which covers both the simple
/// `public < internal < confidential` ladders most enterprises use and the
/// level-plus-compartment schemes used where data is formally classified. Dominance is
/// the standard test: a clearance dominates a label when its level is at least as high
/// **and** it holds every compartment the label names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Label {
    /// Sensitivity level. Higher is more restricted.
    pub level: u16,
    /// Compartments (need-to-know sets) the label requires.
    pub compartments: BTreeSet<String>,
}

impl Label {
    /// A label at a level with no compartments.
    #[must_use]
    pub fn level(level: u16) -> Self {
        Self {
            level,
            compartments: BTreeSet::new(),
        }
    }

    /// Whether a clearance carrying `self` may see data labelled `other`.
    #[must_use]
    pub fn dominates(&self, other: &Self) -> bool {
        self.level >= other.level && other.compartments.is_subset(&self.compartments)
    }

    /// The least upper bound — the label a derived fact must carry when it was inferred
    /// from facts carrying `self` and `other`.
    ///
    /// This is the information-flow rule that keeps materialised inference from
    /// laundering restricted data into an unrestricted conclusion.
    #[must_use]
    pub fn join(&self, other: &Self) -> Self {
        Self {
            level: self.level.max(other.level),
            compartments: self
                .compartments
                .union(&other.compartments)
                .cloned()
                .collect(),
        }
    }
}

/// An authenticated identity, with whatever the identity provider asserted about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable identifier. Conventionally `urn:holos:principal:<issuer>/<subject>`.
    pub id: NamedNode,
    /// Role or group names, from `roles`, `groups`, or an LDAP/AD group mapping.
    pub roles: BTreeSet<String>,
    /// Every other verified claim, as multi-valued strings. Used by attribute-based rules.
    pub attributes: BTreeMap<String, BTreeSet<String>>,
    /// What this principal is cleared to see. `None` means "unclassified only".
    pub clearance: Option<Label>,
}

impl Principal {
    /// A principal with an identifier and nothing else — no roles, no clearance.
    ///
    /// Under a deny-by-default policy this principal can read nothing, which is the
    /// correct starting point.
    #[must_use]
    pub fn new(id: NamedNode) -> Self {
        Self {
            id,
            roles: BTreeSet::new(),
            attributes: BTreeMap::new(),
            clearance: None,
        }
    }

    /// The anonymous principal, for unauthenticated access.
    #[must_use]
    pub fn anonymous() -> Self {
        Self::new(NamedNode::new_unchecked("urn:holos:principal:anonymous"))
    }

    /// Adds a role.
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.roles.insert(role.into());
        self
    }

    /// Adds an attribute value.
    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes
            .entry(key.into())
            .or_default()
            .insert(value.into());
        self
    }

    /// Sets the clearance.
    #[must_use]
    pub fn with_clearance(mut self, clearance: Label) -> Self {
        self.clearance = Some(clearance);
        self
    }

    /// Whether this principal holds a role.
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }

    /// Whether this principal has an attribute with a value.
    #[must_use]
    pub fn has_attribute(&self, key: &str, value: &str) -> bool {
        self.attributes.get(key).is_some_and(|v| v.contains(value))
    }

    /// Whether this principal may see data carrying a label.
    #[must_use]
    pub fn is_cleared_for(&self, label: &Label) -> bool {
        match &self.clearance {
            Some(c) => c.dominates(label),
            // No clearance still sees unclassified data — level 0, no compartments.
            None => *label == Label::default(),
        }
    }

    /// Builds this principal from verified identity-provider claims.
    ///
    /// `roles_claim` names the claim holding role or group membership — `roles`,
    /// `groups`, `cognito:groups`, whatever the deployment's IdP emits. Every other claim
    /// becomes an attribute, so attribute-based rules can reach it without this crate
    /// knowing anything about the IdP's schema.
    ///
    /// The caller must have verified the token first. This function trusts its input
    /// completely and says so.
    #[must_use]
    pub fn from_verified_claims(
        issuer: &str,
        subject: &str,
        claims: &BTreeMap<String, BTreeSet<String>>,
        roles_claim: &str,
    ) -> Self {
        let id = NamedNode::new_unchecked(format!(
            "urn:holos:principal:{}/{}",
            urn_escape(issuer),
            urn_escape(subject)
        ));
        let mut principal = Self::new(id);
        for (key, values) in claims {
            if key == roles_claim {
                principal.roles.extend(values.iter().cloned());
            } else {
                principal
                    .attributes
                    .insert(key.clone(), values.iter().cloned().collect());
            }
        }
        principal
    }

    /// This principal as RDF, for a policy that wants to reason over it with SPARQL or
    /// SHACL rather than with the compiled fast path.
    ///
    /// The quads land in a caller-chosen graph, which should be a system graph the
    /// principal itself cannot read — otherwise a principal could inspect, and infer
    /// from, the policy inputs about other principals.
    #[must_use]
    pub fn to_quads(&self, graph: &NamedNode) -> Vec<Quad> {
        let mut quads = vec![Quad {
            subject: self.id.clone().into(),
            predicate: oxrdf::vocab::rdf::TYPE.into_owned(),
            object: holos("Principal").into(),
            graph_name: graph.clone().into(),
        }];
        for role in &self.roles {
            quads.push(Quad {
                subject: self.id.clone().into(),
                predicate: holos("role"),
                object: Literal::new_simple_literal(role).into(),
                graph_name: graph.clone().into(),
            });
        }
        for (key, values) in &self.attributes {
            for value in values {
                quads.push(Quad {
                    subject: self.id.clone().into(),
                    predicate: holos(&format!("claim_{}", sanitise(key))),
                    object: Literal::new_simple_literal(value).into(),
                    graph_name: graph.clone().into(),
                });
            }
        }
        if let Some(clearance) = &self.clearance {
            quads.push(Quad {
                subject: self.id.clone().into(),
                predicate: holos("clearanceLevel"),
                object: Term::Literal(Literal::new_typed_literal(
                    clearance.level.to_string(),
                    oxrdf::vocab::xsd::INTEGER,
                )),
                graph_name: graph.clone().into(),
            });
            for compartment in &clearance.compartments {
                quads.push(Quad {
                    subject: self.id.clone().into(),
                    predicate: holos("compartment"),
                    object: Literal::new_simple_literal(compartment).into(),
                    graph_name: graph.clone().into(),
                });
            }
        }
        quads
    }
}

/// Percent-escapes the characters that would break an IRI, so an attacker-controlled
/// subject claim cannot forge a different principal's identifier.
fn urn_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// Reduces a claim name to something safe to append to an IRI.
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lattice_dominance() {
        let public = Label::default();
        let internal = Label::level(1);
        let secret_alpha = Label {
            level: 2,
            compartments: ["ALPHA".to_owned()].into_iter().collect(),
        };
        let secret_alpha_beta = Label {
            level: 2,
            compartments: ["ALPHA".to_owned(), "BETA".to_owned()]
                .into_iter()
                .collect(),
        };

        assert!(internal.dominates(&public));
        assert!(!public.dominates(&internal));
        assert!(secret_alpha.dominates(&internal));
        // A higher level is not enough without the compartment.
        assert!(!secret_alpha.dominates(&secret_alpha_beta));
        assert!(secret_alpha_beta.dominates(&secret_alpha));
    }

    #[test]
    fn join_is_the_least_upper_bound() {
        let a = Label {
            level: 1,
            compartments: ["ALPHA".to_owned()].into_iter().collect(),
        };
        let b = Label {
            level: 3,
            compartments: ["BETA".to_owned()].into_iter().collect(),
        };
        let j = a.join(&b);
        assert_eq!(j.level, 3);
        assert_eq!(j.compartments.len(), 2);
        // A fact derived from both is at least as restricted as either premise.
        assert!(j.dominates(&a));
        assert!(j.dominates(&b));
    }

    #[test]
    fn no_clearance_sees_only_unclassified() {
        let p = Principal::anonymous();
        assert!(p.is_cleared_for(&Label::default()));
        assert!(!p.is_cleared_for(&Label::level(1)));
    }

    #[test]
    fn claims_become_roles_and_attributes() {
        let claims: BTreeMap<String, BTreeSet<String>> = [
            (
                "groups".to_owned(),
                ["analyst".to_owned(), "eu-staff".to_owned()]
                    .into_iter()
                    .collect(),
            ),
            (
                "department".to_owned(),
                ["finance".to_owned()].into_iter().collect(),
            ),
        ]
        .into_iter()
        .collect();

        let p = Principal::from_verified_claims(
            "https://login.example.com",
            "user-17",
            &claims,
            "groups",
        );
        assert!(p.has_role("analyst"));
        assert!(p.has_role("eu-staff"));
        assert!(p.has_attribute("department", "finance"));
        assert!(!p.has_role("department"), "non-role claims stay attributes");
    }

    #[test]
    fn hostile_claims_cannot_forge_an_identifier() {
        // A subject claim containing IRI syntax must not let one principal impersonate
        // another by ending the path early.
        let claims = BTreeMap::new();
        let attacker = Principal::from_verified_claims(
            "https://idp",
            "victim> <urn:holos:principal:admin",
            &claims,
            "groups",
        );
        let admin = Principal::new(NamedNode::new_unchecked("urn:holos:principal:admin"));
        assert_ne!(attacker.id, admin.id);
        assert!(
            !attacker.id.as_str().contains(' '),
            "escaped: {}",
            attacker.id
        );
        assert!(!attacker.id.as_str().contains('>'));
    }

    #[test]
    fn principals_serialise_to_rdf() {
        let g = NamedNode::new_unchecked("urn:holos:graph:principals");
        let p = Principal::new(NamedNode::new_unchecked("urn:holos:principal:a"))
            .with_role("analyst")
            .with_attribute("department", "finance")
            .with_clearance(Label::level(2));
        let quads = p.to_quads(&g);
        assert!(quads.iter().all(|q| q.graph_name == g.clone().into()));
        assert!(quads.iter().any(|q| q.predicate == holos("role")));
        assert!(quads.iter().any(|q| q.predicate == holos("clearanceLevel")));
    }
}
