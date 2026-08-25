//! HOLOS L4 — SHACL validation over the store's own indexes.
//!
//! `DESIGN.md` §8: take the SHACL_Engine design — compile-once flat IR, integer-interned
//! terms, sorted index access, deterministic reports — and change one thing. **The
//! validator reads the store's dictionary and indexes instead of loading a private copy.**
//!
//! That single change is what the layer exists to test. SHACL_Engine's own benchmarks
//! have loading exceeding validation by roughly threefold at 100k instances; a validator
//! that shares the store's ids does not load at all.
//!
//! Two things follow that a library cannot do:
//!
//! - **A data graph is selected, not flattened.** [`Options::data_graph`] takes a
//!   [`GraphFilter`], so a holon's Boundary can validate its own scene (§9).
//! - **Revalidation can be incremental.** [`incremental`] derives the focus nodes a delta
//!   could have affected, which is what makes validation affordable on the write path.
//!
//! # Scope
//!
//! SHACL Core, less the parts that need machinery this workspace does not have yet:
//! SPARQL-based constraints and targets need pre-binding through L3, SHACL-AF rules need
//! the fixpoint engine of §8, and node expressions are SHACL 1.2. Exact conformance
//! numbers come from the W3C suite rather than from this paragraph — see
//! `crates/holos-conformance`.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions, clippy::missing_errors_doc)]

pub mod access;
pub mod bridge;
pub mod engine;
pub mod incremental;
pub mod ir;
pub mod report;
pub mod validate;
pub mod vocab;

pub use access::GraphView;
pub use ir::{Compiler, Constraint, Shape, ShapeIdx, Shapes};
pub use validate::Validator;
pub use vocab::Sh;

use holos_core::TermId;
use holos_store::{GraphFilter, Store};

/// Anything that can go wrong compiling or evaluating shapes.
#[derive(Debug, thiserror::Error)]
pub enum ShaclError {
    /// The storage layer could not answer.
    #[error(transparent)]
    Storage(#[from] holos_store::StorageError),
    /// The shapes graph does not describe valid shapes.
    #[error("ill-formed shapes graph: {0}")]
    IllFormedShape(String),
    /// A construct this validator does not implement yet.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Shape references nested past the limit.
    #[error("shape references nest more than {0} deep")]
    TooDeep(u32),
}

/// One violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationResult {
    /// The node the shape was checked at.
    pub focus_node: TermId,
    /// `sh:resultPath` — the `sh:path` of the reporting property shape, if any.
    pub path: Option<TermId>,
    /// `sh:value` — the value that failed, where the constraint names one.
    pub value: Option<TermId>,
    /// `sh:sourceShape`.
    pub source_shape: TermId,
    /// `sh:sourceConstraintComponent`.
    pub component: TermId,
    /// `sh:resultSeverity`.
    pub severity: TermId,
    /// `sh:resultMessage`, in shapes-graph order.
    pub messages: Vec<TermId>,
}

/// The outcome of a validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Whether the data conforms.
    pub conforms: bool,
    /// Every violation found.
    pub results: Vec<ValidationResult>,
}

impl Report {
    /// Builds a report from results.
    #[must_use]
    pub fn new(results: Vec<ValidationResult>) -> Self {
        Self {
            conforms: results.is_empty(),
            results,
        }
    }

    /// A conforming report.
    #[must_use]
    pub fn conforming() -> Self {
        Self::new(Vec::new())
    }
}

/// Which graphs to read.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// The data graph to validate.
    ///
    /// The limitation §8 names in SHACL_Engine is that named graphs are flattened into
    /// one. Selecting a graph is what lets a holon validate its own scene.
    pub data_graph: GraphFilter,
    /// The graph the shapes are read from. Often the same as the data graph.
    pub shapes_graph: GraphFilter,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            data_graph: GraphFilter::Any,
            shapes_graph: GraphFilter::Any,
        }
    }
}

/// Something that can validate a store against shapes.
///
/// The seam exists so an external engine can be substituted for this one without the
/// holon Boundary (§9) knowing which it has.
pub trait Validate {
    /// Validates everything the shapes target.
    fn validate(&self, store: &Store) -> Result<Report, ShaclError>;
}

/// Compiled shapes, ready to validate a store repeatedly.
///
/// Holding this rather than a shapes graph *is* the compile-once property: constructing it
/// reads the shapes graph, and validating never does.
#[derive(Debug, Clone)]
pub struct CompiledShapes {
    shapes: Shapes,
    options: Options,
    sh: Sh,
}

impl CompiledShapes {
    /// Compiles the shapes a store holds.
    pub fn compile(store: &Store, options: Options) -> Result<Self, ShaclError> {
        let sh = Sh::new();
        let graph = GraphView::new(store, options.shapes_graph);
        let shapes = Compiler::new(graph, &sh).compile()?;
        Ok(Self {
            shapes,
            options,
            sh,
        })
    }

    /// The compiled shapes.
    #[must_use]
    pub fn shapes(&self) -> &Shapes {
        &self.shapes
    }

    /// The vocabulary these shapes were compiled against.
    #[must_use]
    pub fn vocabulary(&self) -> &Sh {
        &self.sh
    }

    /// The graph selection.
    #[must_use]
    pub fn options(&self) -> Options {
        self.options
    }

    /// Validates a store.
    pub fn validate(&self, store: &Store) -> Result<Report, ShaclError> {
        self.validator(store).validate_all()
    }

    /// Revalidates only what a delta could have affected.
    ///
    /// The cost tracks the size of the change rather than the size of the graph, which is
    /// what makes SHACL affordable on the write path (`DESIGN.md` §8).
    pub fn revalidate(
        &self,
        store: &Store,
        changes: &[incremental::Change],
    ) -> Result<Report, ShaclError> {
        let validator = self.validator(store);
        let data = GraphView::new(store, self.options.data_graph);
        let rdf_type = self.sh.rdf_type;

        let plan = incremental::plan(&self.shapes, &self.sh, changes, |node| {
            data.objects(node, rdf_type).unwrap_or_default()
        });

        // A change almost always implicates an *anonymous* property shape, which has no
        // targets and is only ever evaluated through its parent. Attributing the work
        // upwards to a shape that does have targets is what makes the change visible at
        // all; without it, revalidation silently finds nothing.
        let mut attributed: rustc_hash::FxHashSet<(ShapeIdx, TermId)> =
            rustc_hash::FxHashSet::default();
        let mut implicated: rustc_hash::FxHashSet<ShapeIdx> = rustc_hash::FxHashSet::default();
        for (idx, node) in plan.work() {
            for ancestor in self.shapes.targeted_ancestors(idx) {
                implicated.insert(ancestor);
                attributed.insert((ancestor, node));
            }
        }

        // The candidate focus nodes are the endpoints of changed quads, which is an
        // over-approximation in one direction and an under-approximation in the other:
        // a shape reached down a property path has its focus node further upstream. So
        // trim what the targets do not select, then widen any shape left with nothing to
        // do back to all of its focus nodes. Over-reporting costs time; under-reporting
        // would let an invalid graph through.
        let mut focus_cache: rustc_hash::FxHashMap<ShapeIdx, Vec<TermId>> =
            rustc_hash::FxHashMap::default();
        for idx in &implicated {
            focus_cache.insert(*idx, validator.focus_nodes(*idx).unwrap_or_default());
        }
        attributed.retain(|(idx, focus)| {
            focus_cache
                .get(idx)
                .is_some_and(|nodes| nodes.binary_search(focus).is_ok())
        });
        for idx in &implicated {
            if attributed.iter().any(|(i, _)| i == idx) {
                continue;
            }
            for focus in focus_cache.get(idx).into_iter().flatten() {
                attributed.insert((*idx, *focus));
            }
        }

        let mut work: Vec<(ShapeIdx, TermId)> = attributed.into_iter().collect();
        work.sort_unstable();
        validator.validate_selected(&work)
    }

    /// Renders a report as RDF, deterministically.
    pub fn report_to_quads(
        &self,
        store: &Store,
        report: &Report,
    ) -> Result<Vec<oxrdf::Quad>, ShaclError> {
        report::to_quads(
            report,
            GraphView::new(store, self.options.shapes_graph),
            &self.sh,
        )
    }

    fn validator<'a>(&'a self, store: &'a Store) -> Validator<'a> {
        Validator::new(
            &self.shapes,
            GraphView::new(store, self.options.data_graph),
            GraphView::new(store, self.options.shapes_graph),
            &self.sh,
        )
    }
}

impl Validate for CompiledShapes {
    fn validate(&self, store: &Store) -> Result<Report, ShaclError> {
        Self::validate(self, store)
    }
}
