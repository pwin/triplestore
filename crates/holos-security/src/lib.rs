//! HOLOS security — principals, fine-grained access policy, and the audit record.
//!
//! `DESIGN.md` §13 Q4 asked whether the holon Boundary layer could double as the
//! authorization surface. This crate is the answer: yes, provided enforcement sits at the
//! scan and not in a query rewrite. See [`policy`] for that argument in full, and
//! `DESIGN.md` §14 for the parts of the system that need their own rule because they can
//! carry information around the scan — statistics, materialised inference, and
//! projections.
//!
//! Three things are deliberately *not* here:
//!
//! - **Authentication.** Token verification, Kerberos, mTLS and SAML belong at the edge.
//!   This crate consumes already-verified claims. See [`principal`].
//! - **Encryption.** At rest belongs to the storage layer and the platform's key
//!   management; in transit belongs to the protocol server.
//! - **A policy language.** Policy is RDF, or it comes from an external decision point
//!   through [`PolicyProvider`]. There is no bespoke DSL to learn or to get wrong.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// `# Errors` sections would restate the error enum on every function; the enums are
// documented at their definition instead.
#![allow(clippy::missing_errors_doc)]
// s/p/o/g are the names the RDF and SPARQL specifications use. Renaming them to satisfy
// a length lint would make this code harder to check against the specs, not easier.
#![allow(clippy::many_single_char_names)]

pub mod audit;
pub mod policy;
pub mod principal;

pub use audit::{AccessEvent, Action, AuditSink, CollectingSink, NullSink, Outcome};
pub use policy::{
    CompiledPolicy, Decision, Effect, Modes, Policy, PrincipalMatch, Rule, Scope, Semantics,
};
pub use principal::{Label, Principal};

use holos_store::{Result, Store};

/// A source of policy.
///
/// The in-graph case is [`Policy`] itself. This trait is the seam for the other case:
/// enterprises that have standardised on an external policy decision point — Open Policy
/// Agent, XACML, Cedar — and will not accept a second place where authorization is
/// decided.
///
/// An implementation is expected to cache. It is called once per session and again
/// whenever [`CompiledPolicy::is_stale`] fires, not once per quad.
pub trait PolicyProvider: Send + Sync {
    /// Returns the policy that applies to a principal.
    fn policy_for(&self, principal: &Principal) -> Policy;
}

impl PolicyProvider for Policy {
    fn policy_for(&self, _principal: &Principal) -> Policy {
        self.clone()
    }
}

/// A principal bound to a policy, with the compiled form cached.
///
/// This is the only way to get at data: there is no ambient authority, and no API that
/// reads quads without a session. Making the capability explicit is what stops a future
/// code path from quietly acquiring an unauthorized view.
#[derive(Debug, Clone)]
pub struct Session {
    principal: Principal,
    policy: Policy,
    compiled: CompiledPolicy,
}

impl Session {
    /// Opens a session for a principal against a store.
    pub fn open(store: &Store, principal: Principal, policy: Policy) -> Result<Self> {
        let compiled = policy.compile(store, &principal)?;
        Ok(Self {
            principal,
            policy,
            compiled,
        })
    }

    /// Opens a session that can do anything. For embedded single-tenant use, and tests.
    pub fn unrestricted(store: &Store) -> Result<Self> {
        Self::open(store, Principal::anonymous(), Policy::permit_all())
    }

    /// The principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The compiled policy, recompiling first if the store has moved on.
    ///
    /// Callers must go through this rather than caching the result, because a stale
    /// policy errs towards permitting — see [`CompiledPolicy::is_stale`].
    pub fn policy(&mut self, store: &Store) -> Result<&CompiledPolicy> {
        if self.compiled.is_stale(store) {
            self.compiled = self.policy.compile(store, &self.principal)?;
        }
        Ok(&self.compiled)
    }

    /// The compiled policy without a staleness check.
    ///
    /// Only for a read path that holds a snapshot the policy was compiled against.
    #[must_use]
    pub fn policy_unchecked(&self) -> &CompiledPolicy {
        &self.compiled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{GraphName, NamedNode, Quad};

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://example.com/{s}"))
    }

    #[test]
    fn a_session_recompiles_when_the_store_moves_on() {
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
        let policy = Policy::permit_all().with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(nn("salary")),
            PrincipalMatch::Everyone,
        ));
        let mut session = Session::open(&store, Principal::anonymous(), policy).unwrap();

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

        let salary = store
            .lookup_term(nn("salary").as_ref().into())
            .unwrap()
            .unwrap();
        let quad = store
            .quads_for_pattern(None, Some(salary), None, holos_store::GraphFilter::Any)
            .next()
            .unwrap()
            .unwrap();

        assert!(
            session.policy_unchecked().permits_quad(quad, Modes::READ),
            "the stale policy is the permissive one — this is the hazard"
        );
        assert!(
            !session
                .policy(&store)
                .unwrap()
                .permits_quad(quad, Modes::READ),
            "going through policy() must recompile and start denying"
        );
    }
}
