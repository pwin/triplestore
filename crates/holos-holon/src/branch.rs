//! Branching a holon: a checkpoint plus a fresh event-log head.
//!
//! §9 defines a branch that way, and the two halves live in different places:
//!
//! * **The checkpoint** is [`holos_store::Store::checkpoint`] — a hard-linked fork of the
//!   whole dataset. Cheap and consistent, but it produces a *separate store*, so the two
//!   branches cannot be queried together.
//! * **The fresh event-log head** is this module: a branch inside one store, so parent and
//!   child are both addressable from the same SPARQL query.
//!
//! Which one to reach for depends on the question. Forking a dataset to try a migration
//! wants the checkpoint. Exploring two futures of the same holon and comparing them wants
//! this.
//!
//! # What a branch inherits, and what it does not
//!
//! | | Inherited | Why |
//! |---|---|---|
//! | Scene | **copied** | The branch starts from the parent's state; that is what makes it a branch rather than a new holon |
//! | Boundary | **copied** | It begins governed the same way, and may then diverge — a branch exists partly to change the rules |
//! | Projections | **copied** | Same views over a different future |
//! | Admission | **copied** | |
//! | Event log | **not copied** | The fresh head. The branch's history is its own, and begins with why it exists |
//!
//! The scene is copied quad by quad rather than linked. Nothing in RDF lets two named graphs
//! share storage, and pretending otherwise would mean a write to one silently changing the
//! other. A branch therefore costs the size of the scene — cheap for the holons this is for,
//! and honest about it.
//!
//! # Versions continue rather than restart
//!
//! A branch created at parent version 7 has version 7, and its first tick is 8. Restarting
//! at zero would make "version 3" ambiguous between the two lineages while their scenes are
//! genuinely related. Continuing keeps the branch point legible: the numbers say when they
//! diverged, and [`branch_point`] says from what.

use crate::model::{holos, Holon};
use crate::registry;
use crate::HolonError;
use holos_engine::Engine;
use holos_security::Session;
use holos_store::GraphFilter;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};

/// Where a branch came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPoint {
    /// The holon this one was branched from.
    pub parent: NamedNode,
    /// The parent's version at the moment of the branch.
    pub version: u64,
}

/// Creates a new holon starting from another's current state.
///
/// The new holon's graphs are derived from `new_id` exactly as [`Holon::new`] derives them,
/// so a branch is an ordinary holon in every respect except that its event log opens with a
/// record of where it came from.
///
/// The `session` is not decoration: a branch copies a scene, and copying is a write. It goes
/// through the same policy every other write does, so a principal cannot obtain a copy of a
/// scene it may not write by branching it.
///
/// # Errors
///
/// * [`HolonError::Invalid`] if the new id is already registered — branching onto an
///   existing holon would merge two scenes silently.
/// * [`HolonError::WriteDenied`] if the policy refuses the copy.
/// * Storage failures while copying the scene.
pub fn branch(
    engine: &mut Engine,
    source: &Holon,
    new_id: NamedNode,
    session: &mut Session,
) -> Result<Holon, HolonError> {
    if registry::load(engine.store(), &new_id)?.is_some() {
        return Err(HolonError::Invalid(format!(
            "{new_id} is already registered; branching onto it would merge two scenes"
        )));
    }

    // Five multi-write operations, and a half-finished branch is worse than none: a scene
    // copied with no registry entry is invisible, and a registry entry with no boundary is a
    // holon that silently constrains nothing. One scope around the lot, so it is one commit.
    let owned = crate::begin_commit(engine)?;
    let result = branch_body(engine, source, new_id, session);
    crate::end_commit(engine, owned, result.is_ok())?;
    result
}

fn branch_body(
    engine: &mut Engine,
    source: &Holon,
    new_id: NamedNode,
    session: &mut Session,
) -> Result<Holon, HolonError> {
    let version = registry::version(engine, source)?;
    let mut child = Holon::new(new_id);
    child.admission = source.admission;
    child.projections = source.projections.clone();

    // Scene and boundary are copied; the event log deliberately is not.
    copy_graph(engine, &source.scene, &child.scene)?;
    copy_graph(engine, &source.boundary, &child.boundary)?;

    registry::register(engine, &child, session)?;
    registry::set_version(engine, &child, version)?;
    record_branch_point(
        engine,
        &child,
        &BranchPoint {
            parent: source.id.clone(),
            version,
        },
    )?;

    Ok(child)
}

/// Where a holon was branched from, if it was.
///
/// Reads the branch record out of the holon's own event log, so provenance travels with the
/// holon rather than living in a side table that a dump would lose.
///
/// # Errors
///
/// Storage failures while reading the event graph.
pub fn branch_point(engine: &Engine, holon: &Holon) -> Result<Option<BranchPoint>, HolonError> {
    let store = engine.store();
    let Some(events) = store.lookup_term(holon.events.as_ref().into())? else {
        // The event graph has never been written, so the holon cannot be a branch.
        return Ok(None);
    };
    let mut parent = None;
    let mut version = None;
    for encoded in store.quads_for_pattern(None, None, None, GraphFilter::Named(events)) {
        let quad = store.decode_quad(encoded?)?;
        if quad.predicate == holos("branchedFrom") {
            if let Term::NamedNode(n) = &quad.object {
                parent = Some(n.clone());
            }
        } else if quad.predicate == holos("branchedAtVersion") {
            if let Term::Literal(l) = &quad.object {
                version = l.value().parse().ok();
            }
        }
    }
    Ok(match (parent, version) {
        (Some(parent), Some(version)) => Some(BranchPoint { parent, version }),
        // A holon with one half of the record is a bug rather than an un-branched holon, but
        // reporting "not a branch" is the safe reading: it understates provenance instead of
        // inventing it.
        _ => None,
    })
}

/// Copies every quad of one named graph into another.
fn copy_graph(engine: &mut Engine, from: &NamedNode, to: &NamedNode) -> Result<(), HolonError> {
    // Read everything out before writing any of it: inserting while iterating the same store
    // would be reading and mutating at once, and the borrow checker is right to object.
    let quads: Vec<Quad> = match engine.store().lookup_term(from.as_ref().into())? {
        None => Vec::new(),
        Some(source) => {
            let store = engine.store();
            let mut out = Vec::new();
            for encoded in store.quads_for_pattern(None, None, None, GraphFilter::Named(source)) {
                let q = store.decode_quad(encoded?)?;
                out.push(Quad {
                    subject: q.subject,
                    predicate: q.predicate,
                    object: q.object,
                    graph_name: GraphName::NamedNode(to.clone()),
                });
            }
            out
        }
    };
    if quads.is_empty() {
        // Still create the graph, so a branch of an empty-scened holon is a holon with an
        // empty scene rather than one with no scene at all.
        engine
            .store_mut()
            .insert_named_graph(&GraphName::NamedNode(to.clone()))?;
        return Ok(());
    }
    for quad in quads {
        engine.store_mut().insert(quad.as_ref())?;
    }
    Ok(())
}

/// Opens the branch's event log with why it exists.
fn record_branch_point(
    engine: &mut Engine,
    child: &Holon,
    point: &BranchPoint,
) -> Result<(), HolonError> {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let subject = child.id.clone();
    let graph = GraphName::NamedNode(child.events.clone());
    let quads = vec![
        Quad {
            subject: subject.clone().into(),
            predicate: holos("branchedFrom"),
            object: Term::NamedNode(point.parent.clone()),
            graph_name: graph.clone(),
        },
        Quad {
            subject: subject.clone().into(),
            predicate: holos("branchedAtVersion"),
            object: Term::Literal(Literal::new_typed_literal(
                point.version.to_string(),
                oxrdf::vocab::xsd::INTEGER,
            )),
            graph_name: graph.clone(),
        },
        Quad {
            subject: subject.into(),
            predicate: holos("branchedAtTime"),
            object: Term::Literal(Literal::new_typed_literal(
                at.to_string(),
                oxrdf::vocab::xsd::INTEGER,
            )),
            graph_name: graph,
        },
    ];
    for quad in quads {
        engine.store_mut().insert(quad.as_ref())?;
    }
    Ok(())
}
