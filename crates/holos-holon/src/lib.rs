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
//! - **Step 2 runs when it is given somewhere to run.** Boundary rules need SHACL-AF
//!   fixpoint evaluation, which the adapted engine has. It used to need a fresh bridge per
//!   tick — the gap §8 named — and no longer does: [`Rules`] holds one bridged graph and
//!   keeps it current by delta. A caller that does not hold one gets a tick with no rule
//!   step, which is the old behaviour and is still what [`tick`] does on its own.
//! - **Step 5 recomputes rather than maintains.** §9 restricts incremental maintenance to a
//!   fragment of SPARQL and the Z-set machinery is not built. [`Regime::Maintained`] is
//!   refused rather than silently downgraded.
//! - **A tick is atomic but not isolated.** The whole of it — the delta, what the rules
//!   inferred, the event and the version bump — runs inside one [`Store`] commit scope, so a
//!   crash between applying the delta and writing the event can no longer leave the two
//!   disagreeing: on a persistent backend nothing is written until the scope commits. What is
//!   still missing is isolation. A concurrent reader with its own view sees the store before
//!   the commit or after it and this promises nothing about which; that is §6.1's MVCC, and
//!   it is not built. The server and the Python binding hold the engine behind an `RwLock`,
//!   so there the question does not arise.
//!
//!   Note what a scope does *not* replace: a commit the boundary **refuses** is still undone
//!   by a compensating write, because refusing is not failing. The event has to record that
//!   the attempt happened, so the tick keeps writing while unapplying the delta — an ordinary
//!   part of the commit rather than an unwind.
//!
//! The projection limit above is the honest limit of a walking skeleton. It demonstrates the
//! shape; it is not yet a thing to put a ledger in.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions, clippy::missing_errors_doc)]

pub mod branch;
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
    /// The request contradicts itself or the store's current state.
    ///
    /// A client error rather than a storage one: branching onto an id that is already
    /// registered, say. No retry changes it.
    #[error("{0}")]
    Invalid(String),
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
    tick_with_rules(engine, holon, session, delta, None)
}

/// [`tick`], with somewhere to run the boundary's rules.
///
/// Pass a [`Rules`] kept across ticks and step 2 runs: the rules fire to a fixpoint over the
/// scene as the delta leaves it, and what they infer is written into the scene alongside it.
/// Pass `None` and the step is skipped, which is what [`tick`] does.
///
/// **Inferences are part of the commit.** They are applied before validation, so the boundary
/// judges the scene the rules produced rather than the one the caller sent; they are recorded
/// in the event as ordinary additions, because a reader asking what changed should not have
/// to know which triples a rule wrote; and a refused tick takes them out again with
/// everything else. A rule that infers something the boundary forbids therefore *rejects the
/// commit* rather than quietly persisting — which is the point of running rules before
/// validation rather than after.
///
/// # Errors
///
/// As [`tick`], plus a rule set that does not reach a fixpoint in [`Rules::MAX_ROUNDS`], and
/// a [`Rules`] bridged against a different holon's scene.
pub fn tick_with_rules(
    engine: &mut Engine,
    holon: &Holon,
    session: &mut Session,
    delta: &Delta,
    rules: Option<&mut Rules>,
) -> Result<TickOutcome, HolonError> {
    // The scope is opened here rather than inside the body so that *every* way out of the
    // body passes through it — including the `?` on a store call, which no hand-placed undo
    // at each `return` would have caught.
    //
    // A caller may already have one open, in which case the tick joins that commit instead of
    // making one of its own and the rollback point is the caller's.
    let owned = !engine.store().in_scope();
    if owned {
        engine.store_mut().begin()?;
    }
    let outcome = tick_body(engine, holon, session, delta, rules);
    if owned {
        if outcome.is_ok() {
            engine.store_mut().commit()?;
        } else {
            engine.store_mut().rollback();
        }
    }
    outcome
}

/// The tick itself, with the commit scope already open around it.
fn tick_body(
    engine: &mut Engine,
    holon: &Holon,
    session: &mut Session,
    delta: &Delta,
    rules: Option<&mut Rules>,
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
        if !session
            .policy(engine.store())?
            .permits_quad(encoded, Modes::WRITE)
        {
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
                return Err(HolonError::WriteDenied(holon.scene.clone()));
            }
        }
        if engine.store_mut().remove(quad.as_ref())? {
            applied.push((Operation::Removed, triple.clone(), encoded));
        }
    }

    let mut changes: Vec<Change> = applied
        .iter()
        .map(|(op, _, encoded)| match op {
            Operation::Added => Change::added(*encoded),
            Operation::Removed => Change::removed(*encoded),
        })
        .collect();

    // --- 2. boundary rules to fixpoint --------------------------------------------------
    //
    // The inferences join `applied` and `changes`, which is what makes them part of the
    // commit rather than a side effect of it: validated with everything else, recorded in the
    // event, and undone with the rest if the commit is refused.
    if let Some(rules) = rules {
        // A runaway rule set is a failed tick, not a half-applied one — which is now the
        // scope's business rather than something to unwind by hand here.
        let inferred = rules.fire(engine, holon, &changes)?;
        for triple in inferred {
            let quad = into_scene(&triple, &scene);
            let encoded = engine.store_mut().encode_quad(quad.as_ref())?;
            // Policy applies to what a rule writes exactly as to what a caller writes. A rule
            // is not a way around the boundary of §14 — it runs inside a session, and the
            // session is the principal's.
            if !session
                .policy(engine.store())?
                .permits_quad(encoded, Modes::WRITE)
            {
                return Err(HolonError::WriteDenied(holon.scene.clone()));
            }
            if engine.store_mut().insert_encoded(encoded)? {
                applied.push((Operation::Added, triple, encoded));
                changes.push(Change::added(encoded));
            }
        }
    }

    // --- 3. validate against the boundary ------------------------------------------------

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

/// A boundary's rules, bridged once and kept current across ticks.
///
/// Rules are the one part of a tick that needs the *data* as an engine graph rather than as
/// a store, and building one costs the whole scene. Held across ticks it costs that once,
/// and each tick pays only for its own delta — which is the difference between a rule step
/// that can run on a write path and one that cannot.
///
/// Kept by the caller rather than inside the holon because it is a cache, and a cache with
/// an owner is one whose lifetime somebody has thought about. Dropping it is always safe:
/// the next [`Rules::prepare`] rebuilds it.
pub struct Rules {
    run: holos_shacl::engine::EngineRun,
    /// The scene these rules were bridged against, so using them on another is caught.
    scene: NamedNode,
}

impl Rules {
    /// How many rounds the fixpoint may take before it is called a runaway.
    ///
    /// A rule set that infers new terms without end does not converge, and there is no way to
    /// tell that apart from a slow one except by giving up somewhere. Sixteen is well past
    /// any rule depth a boundary is likely to have and short enough that a runaway is noticed
    /// in the tick that caused it rather than in a log the next morning.
    pub const MAX_ROUNDS: usize = 16;

    /// Bridges a holon's scene and boundary. `None` when the boundary is absent, which is a
    /// holon that constrains nothing rather than an error.
    ///
    /// The two graphs are treated differently on purpose. A **boundary** the dictionary has
    /// never seen holds no shapes and no rules, so there is nothing to prepare. An empty
    /// **scene** is an ordinary state — every holon has one before its first tick — so its
    /// IRI is interned rather than looked up, which is what the first tick would do anyway.
    ///
    /// # Errors
    ///
    /// A storage failure, or a boundary the adapted engine cannot compile.
    pub fn prepare(engine: &mut Engine, holon: &Holon) -> Result<Option<Self>, HolonError> {
        let Some(boundary_id) = engine
            .store()
            .lookup_term(holon.boundary.as_ref().into())?
            .map(GraphFilter::Named)
        else {
            return Ok(None);
        };
        // Interned by encoding a quad that names it. The quad is not inserted; encoding is
        // what puts the terms in the dictionary.
        let scene_id = GraphFilter::Named(
            engine
                .store_mut()
                .encode_quad(oxrdf::QuadRef::new(
                    holon.scene.as_ref(),
                    holon.scene.as_ref(),
                    holon.scene.as_ref(),
                    oxrdf::GraphNameRef::DefaultGraph,
                ))?
                .subject,
        );
        let (data_graph, shapes_graph) = (scene_id, boundary_id);
        let run = holos_shacl::engine::EngineRun::prepare(
            engine.store(),
            Options {
                data_graph,
                shapes_graph,
            },
        )?;
        Ok(Some(Self {
            run,
            scene: holon.scene.clone(),
        }))
    }

    /// Brings the bridged scene up to date and returns what the rules infer from it.
    fn fire(
        &mut self,
        engine: &Engine,
        holon: &Holon,
        changes: &[Change],
    ) -> Result<Vec<Triple>, HolonError> {
        if self.scene != holon.scene {
            return Err(HolonError::Shacl(holos_shacl::ShaclError::Unsupported(
                format!(
                    "these rules were bridged against <{}> and cannot be used on <{}>",
                    self.scene, holon.scene
                ),
            )));
        }
        self.run.apply(engine.store(), changes)?;
        Ok(self.run.infer(Self::MAX_ROUNDS)?)
    }
}

/// Unapplies the delta of a commit the boundary refused.
///
/// Deliberately *not* a rollback, and the only caller left is the refusal path. A refused
/// commit still has to write: the event records that the attempt happened and what it would
/// have changed, so discarding everything the tick did would discard the evidence. So this
/// is a compensating write inside the commit rather than an unwind of it — and if it fails,
/// the failure propagates and the scope around the tick discards the whole thing.
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
    Some(Engine::query(view, &projection.query, None).map_err(HolonError::from))
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
