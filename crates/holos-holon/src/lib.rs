//! HOLOS L5 — holons.
//!
//! `DESIGN.md` §9. A holon is a **scene** (a named graph), a **boundary** (shapes bound to
//! that scene), an **event log** (append-only, PROV-O shaped, with RDF 1.2 reifiers), and
//! **projections** (registered queries). A commit is a **tick**, and it is one transaction:
//!
//! 1. apply the delta to the scene
//! 2. run boundary rules to fixpoint
//! 3. validate against boundary shapes — abort or admit-and-record, per holon policy
//! 4. append the event, with reifier-annotated provenance
//! 5. incrementally refresh the affected projections
//!
//! # What this build actually does
//!
//! Steps 1, 3 and 4 are real, and step 3 is *incremental* — it uses the revalidation of §8,
//! which is the whole reason that was built. The rest is honest about itself:
//!
//! - **Step 2 does not run.** Boundary rules need SHACL-AF fixpoint evaluation. The adapted
//!   engine has it, but only over a bridged snapshot, so firing rules per tick would mean
//!   re-bridging per tick. Named in §8 as the gap the immutable engine graph leaves.
//! - **Step 5 recomputes rather than maintains.** §9 restricts incremental maintenance to a
//!   fragment of SPARQL and the Z-set machinery is not built. [`Regime::Maintained`] is
//!   refused rather than silently downgraded.
//! - **A tick is not atomic.** There is no transaction underneath it: a rejected commit is
//!   undone by a compensating write, not a rollback, and a crash between applying the delta
//!   and writing the event leaves the two disagreeing. Making it atomic is what §6.1's MVCC
//!   and checkpoints are for, and neither is built.
//!
//! That last one is the honest limit of a walking skeleton. It demonstrates the shape;
//! it is not yet a thing to put a ledger in.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions, clippy::missing_errors_doc)]

pub mod event;
pub mod model;
pub mod registry;

pub use event::{Operation, TickRecord};
pub use model::{holos, system_graph, Admission, Holon, Projection, Regime};

use holos_engine::{DatasetView, Engine};
use holos_security::{Modes, Session};
use holos_shacl::incremental::Change;
use holos_shacl::{CompiledShapes, Options};
use holos_store::{EncodedQuad, GraphFilter, Store};
use oxrdf::{GraphName, NamedNode, Quad, Triple};

/// Anything a tick can fail with.
#[derive(Debug, thiserror::Error)]
pub enum HolonError {
    /// The storage layer could not answer.
    #[error(transparent)]
    Storage(#[from] holos_store::StorageError),
    /// Validation failed to run — distinct from finding violations.
    #[error(transparent)]
    Shacl(#[from] holos_shacl::ShaclError),
    /// A query failed.
    #[error(transparent)]
    Engine(#[from] holos_engine::EngineError),
    /// The principal may not write to this holon's scene.
    #[error("the principal may not write to {0}")]
    WriteDenied(NamedNode),
    /// A projection asked for a regime this build does not implement.
    #[error(
        "projection {0} asks to be incrementally maintained, which is not implemented \
         (DESIGN.md §9); register it as recomputed or leave it out"
    )]
    UnsupportedRegime(NamedNode),
}

/// What a tick proposes to change.
#[derive(Debug, Default, Clone)]
pub struct Delta {
    /// Triples to add to the scene.
    pub added: Vec<Triple>,
    /// Triples to remove from the scene.
    pub removed: Vec<Triple>,
}

impl Delta {
    /// A delta that adds triples.
    #[must_use]
    pub fn adding(triples: impl IntoIterator<Item = Triple>) -> Self {
        Self {
            added: triples.into_iter().collect(),
            removed: Vec::new(),
        }
    }

    /// Adds a triple.
    #[must_use]
    pub fn add(mut self, triple: Triple) -> Self {
        self.added.push(triple);
        self
    }

    /// Removes a triple.
    #[must_use]
    pub fn remove(mut self, triple: Triple) -> Self {
        self.removed.push(triple);
        self
    }

    /// Whether the delta changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// What a tick did.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    /// The version this tick produced.
    pub version: u64,
    /// Whether the boundary admitted the commit.
    pub admitted: bool,
    /// Violations the boundary found.
    pub violations: usize,
    /// How many triples actually changed the scene.
    pub applied: usize,
    /// The validation report, when the boundary had anything to say.
    pub report: Option<holos_shacl::Report>,
}

impl TickOutcome {
    /// Whether the scene now reflects the delta.
    #[must_use]
    pub fn committed(&self) -> bool {
        self.admitted
    }
}

/// Applies one tick to a holon.
///
/// The write authorisation is per quad and goes through the session, so §14's policy covers
/// a holon commit the same way it covers everything else: `Modes::WRITE` on the scene.
/// Changing the *boundary* is deliberately not this function's job — that needs
/// `Modes::ADMIN`, which §14.7 keeps separate precisely so a data-entry role cannot rewrite
/// the rules it is being held to.
pub fn tick(
    engine: &mut Engine,
    holon: &Holon,
    session: &mut Session,
    delta: &Delta,
) -> Result<TickOutcome, HolonError> {
    for projection in &holon.projections {
        if projection.regime == Regime::Maintained {
            return Err(HolonError::UnsupportedRegime(projection.id.clone()));
        }
    }

    let scene = GraphName::from(holon.scene.clone());
    let version = registry::next_version(engine, holon)?;

    // --- 1. apply the delta to the scene -----------------------------------------------
    let mut applied: Vec<(Operation, Triple, EncodedQuad)> = Vec::new();
    for triple in &delta.added {
        let quad = into_scene(triple, &scene);
        let encoded = engine.store_mut().encode_quad(quad.as_ref())?;
        if !session.policy(engine.store())?.permits_quad(encoded, Modes::WRITE) {
            undo(engine, &applied)?;
            return Err(HolonError::WriteDenied(holon.scene.clone()));
        }
        if engine.store_mut().insert_encoded(encoded)? {
            applied.push((Operation::Added, triple.clone(), encoded));
        }
    }
    for triple in &delta.removed {
        let quad = into_scene(triple, &scene);
        let encoded = engine.store_mut().encode_quad(quad.as_ref())?;
        {
            let policy = session.policy(engine.store())?;
            // Removing needs read as well as write: otherwise "did the delete land" is an
            // oracle for whether hidden data exists (§14.7).
            if !policy.permits_quad(encoded, Modes::WRITE)
                || !policy.permits_quad(encoded, Modes::READ)
            {
                undo(engine, &applied)?;
                return Err(HolonError::WriteDenied(holon.scene.clone()));
            }
        }
        if engine.store_mut().remove(quad.as_ref())? {
            applied.push((Operation::Removed, triple.clone(), encoded));
        }
    }

    // --- 2. boundary rules to fixpoint --------------------------------------------------
    // Not run. See the module note: SHACL-AF fixpoint evaluation exists in the adapted
    // engine but only over a bridged snapshot, and re-bridging per tick would cost the
    // whole graph. Left visible rather than silently skipped.

    // --- 3. validate against the boundary ------------------------------------------------
    let changes: Vec<Change> = applied
        .iter()
        .map(|(op, _, encoded)| match op {
            Operation::Added => Change::added(*encoded),
            Operation::Removed => Change::removed(*encoded),
        })
        .collect();

    let (violations, report) = validate(engine, holon, &changes)?;
    let admitted = violations == 0 || holon.admission == Admission::AdmitAndRecord;

    if !admitted {
        undo(engine, &applied)?;
    }

    // --- 4. append the event --------------------------------------------------------------
    // Written whether or not the commit was admitted: a boundary that discards what it
    // refused leaves no evidence anything was attempted.
    let principal = session.principal().id.clone();
    let record = TickRecord {
        holon,
        version,
        at: now_seconds(),
        principal: &principal,
        changes: applied
            .iter()
            .map(|(op, triple, _)| (*op, triple.clone()))
            .collect(),
        admitted,
        violations,
    };
    for quad in event::to_quads(&record) {
        engine.store_mut().insert(quad.as_ref())?;
    }
    registry::set_version(engine, holon, version)?;

    // --- 5. projections -------------------------------------------------------------------
    // Recomputed on read; nothing to refresh here. `projection` runs them.

    Ok(TickOutcome {
        version,
        admitted,
        violations,
        applied: if admitted { applied.len() } else { 0 },
        report,
    })
}

/// Validates the scene against the boundary, incrementally.
///
/// This is where §8's incremental revalidation earns its place: a tick pays for the size of
/// its own change, not the size of the scene, which is what makes a boundary affordable on
/// every commit rather than nightly.
fn validate(
    engine: &Engine,
    holon: &Holon,
    changes: &[Change],
) -> Result<(usize, Option<holos_shacl::Report>), HolonError> {
    let scene_id = engine
        .store()
        .lookup_term(holon.scene.as_ref().into())?
        .map(GraphFilter::Named);
    let boundary_id = engine
        .store()
        .lookup_term(holon.boundary.as_ref().into())?
        .map(GraphFilter::Named);
    let (Some(data_graph), Some(shapes_graph)) = (scene_id, boundary_id) else {
        // A holon with no boundary graph constrains nothing. That is a legitimate state —
        // a holon can be created before its shapes are — not an error.
        return Ok((0, None));
    };

    let shapes = CompiledShapes::compile(
        engine.store(),
        Options {
            data_graph,
            shapes_graph,
        },
    )?;
    if shapes.shapes().is_empty() {
        return Ok((0, None));
    }
    let report = shapes.revalidate(engine.store(), changes)?;
    Ok((report.results.len(), Some(report)))
}

/// Undoes what a rejected tick applied.
///
/// A compensating write, not a rollback: there is no transaction underneath. If this fails
/// the scene is left inconsistent, which is exactly the hole §6.1's MVCC would close.
fn undo(
    engine: &mut Engine,
    applied: &[(Operation, Triple, EncodedQuad)],
) -> Result<(), HolonError> {
    for (operation, _, encoded) in applied.iter().rev() {
        match operation {
            Operation::Added => {
                engine.store_mut().remove_encoded_quad(*encoded)?;
            }
            Operation::Removed => {
                engine.store_mut().insert_encoded(*encoded)?;
            }
        }
    }
    Ok(())
}

/// Runs one of a holon's projections.
///
/// Agents read projections and never the scene (§9). Recomputed on every read in this
/// build — see the module note.
pub fn projection<'a>(
    view: &'a DatasetView<'a>,
    holon: &Holon,
    id: &NamedNode,
) -> Option<Result<spareval::QueryResults<'a>, HolonError>> {
    let projection = holon.projections.iter().find(|p| &p.id == id)?;
    Some(
        Engine::query(view, &projection.query, None)
            .map_err(HolonError::from),
    )
}

fn into_scene(triple: &Triple, scene: &GraphName) -> Quad {
    Quad {
        subject: triple.subject.clone(),
        predicate: triple.predicate.clone(),
        object: triple.object.clone(),
        graph_name: scene.clone(),
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Reads a store's quads for a graph, for tests and inspection.
#[must_use]
pub fn graph_size(store: &Store, graph: &NamedNode) -> usize {
    let Ok(Some(id)) = store.lookup_term(graph.as_ref().into()) else {
        return 0;
    };
    store
        .quads_for_pattern(None, None, None, GraphFilter::Named(id))
        .filter(std::result::Result::is_ok)
        .count()
}
