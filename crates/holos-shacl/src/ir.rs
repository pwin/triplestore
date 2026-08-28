//! The compiled shape IR, and the compiler that builds it.
//!
//! `DESIGN.md` §8 keeps one property from SHACL_Engine above all others: **a shapes graph
//! is compiled once, before validation starts, and evaluating a constraint never queries
//! the shapes graph again.** Everything below serves that.
//!
//! Shapes live in a flat [`Vec`] and reference each other by index, not by pointer or by
//! IRI, so a nested `sh:node` costs an array offset. Every parameter is resolved to a
//! [`TermId`] at compile time, so the inner loops compare integers. Regular expressions
//! are compiled here too, not per value node.
//!
//! The compiler also builds the dependency index that makes incremental revalidation
//! possible — see [`Shapes::shapes_touching`] and `crate::incremental`.

use crate::access::GraphView;
use crate::vocab::Sh;
use crate::ShaclError;
use holos_core::TermId;
use holos_store::Result as StoreResult;
use oxrdf::Term;
use regex::{Regex, RegexBuilder};
use rustc_hash::{FxHashMap, FxHashSet};

/// Index of a shape in [`Shapes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShapeIdx(pub u32);

/// Index of a path in [`Shapes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathIdx(pub u32);

/// How a shape acquires focus nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `sh:targetNode` — one node, named directly.
    Node(TermId),
    /// `sh:targetClass`, and the implicit target of a shape that is also a class.
    Class(TermId),
    /// `sh:targetSubjectsOf`.
    SubjectsOf(TermId),
    /// `sh:targetObjectsOf`.
    ObjectsOf(TermId),
}

/// A SHACL property path.
#[derive(Debug, Clone)]
pub enum Path {
    /// A single predicate.
    Predicate(TermId),
    /// `sh:inversePath`.
    Inverse(PathIdx),
    /// An RDF list of paths, followed in order.
    Sequence(Vec<PathIdx>),
    /// `sh:alternativePath`.
    Alternative(Vec<PathIdx>),
    /// `sh:zeroOrMorePath`.
    ZeroOrMore(PathIdx),
    /// `sh:oneOrMorePath`.
    OneOrMore(PathIdx),
    /// `sh:zeroOrOnePath`.
    ZeroOrOne(PathIdx),
}

/// What `sh:nodeKind` allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeKindSpec {
    /// IRIs are allowed.
    pub iri: bool,
    /// Blank nodes are allowed.
    pub blank: bool,
    /// Literals are allowed.
    pub literal: bool,
}

/// One compiled constraint.
#[derive(Debug, Clone)]
pub enum Constraint {
    /// `sh:class`
    Class(TermId),
    /// `sh:datatype` — one datatype, or a list of them.
    ///
    /// SHACL 1.2 allows `sh:datatype ( xsd:string rdf:langString )`, satisfied by any of
    /// them. Read as a single IRI the list's blank node matches nothing, so every value
    /// violates — which is how this surfaced: thirty triples reported where nine were
    /// expected.
    Datatype(Vec<TermId>),
    /// `sh:nodeKind`
    NodeKind(NodeKindSpec),
    /// `sh:minCount`
    MinCount(usize),
    /// `sh:maxCount`
    MaxCount(usize),
    /// `sh:minInclusive`
    MinInclusive(TermId),
    /// `sh:maxInclusive`
    MaxInclusive(TermId),
    /// `sh:minExclusive`
    MinExclusive(TermId),
    /// `sh:maxExclusive`
    MaxExclusive(TermId),
    /// `sh:minLength`
    MinLength(usize),
    /// `sh:maxLength`
    MaxLength(usize),
    /// `sh:pattern`, compiled once at shape-compile time rather than per value node.
    Pattern(Box<CompiledPattern>),
    /// `sh:languageIn`
    LanguageIn(Vec<String>),
    /// `sh:uniqueLang`
    UniqueLang,
    /// `sh:equals`
    Equals(TermId),
    /// `sh:disjoint`
    Disjoint(TermId),
    /// `sh:lessThan` — a path, because SHACL 1.2 allows `sh:lessThan ( ex:a ex:b )`.
    LessThan(PathIdx),
    /// `sh:lessThanOrEquals`, likewise.
    LessThanOrEquals(PathIdx),
    /// `sh:not`
    Not(ShapeIdx),
    /// `sh:and`
    And(Vec<ShapeIdx>),
    /// `sh:or`
    Or(Vec<ShapeIdx>),
    /// `sh:xone`
    Xone(Vec<ShapeIdx>),
    /// `sh:node`
    Node(ShapeIdx),
    /// `sh:property`
    Property(ShapeIdx),
    /// `sh:qualifiedValueShape` with its counts.
    Qualified(Box<Qualified>),
    /// `sh:closed`
    Closed(Vec<TermId>),
    /// `sh:hasValue`
    HasValue(TermId),
    /// `sh:in`
    In(Vec<TermId>),
    /// `sh:minListLength` — the value node is a list of at least this many members.
    MinListLength(usize),
    /// `sh:maxListLength` — at most this many.
    MaxListLength(usize),
    /// `sh:uniqueMembers` — no member of the list appears twice.
    UniqueMembers,
    /// `sh:memberShape` — every member of the list conforms to this shape.
    MemberShape(ShapeIdx),
    /// `sh:singleLine` — the lexical form contains no line break.
    SingleLine,
    /// `sh:subsetOf` — every value node is also reached by this *path* from the focus
    /// node. A path rather than a predicate: SHACL 1.2 allows
    /// `sh:subsetOf ( ex:a ex:b )`, and reading it as an IRI makes every value violate.
    SubsetOf(PathIdx),
    /// `sh:uniqueValuesFor` — no two focus nodes of this shape share a key.
    ///
    /// The properties are a **composite key**, not a sequence path: `( skos:notation
    /// skos:inScheme )` means the *pair* must be unique, not that one is reached through the
    /// other. `sh:subsetOf` takes a list and means the opposite thing, which is why these
    /// are compiled apart rather than sharing a helper.
    ///
    /// Unlike every other constraint here, it is not a statement about one focus node, so it
    /// is evaluated once per shape rather than once per node — see `Validator::validate_all`.
    UniqueValuesFor(Vec<TermId>),
}

/// A compiled `sh:pattern`.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    /// The compiled expression.
    pub regex: Regex,
    /// The pattern as written, for the validation result.
    pub source: TermId,
}

/// A compiled `sh:qualifiedValueShape` group.
#[derive(Debug, Clone)]
pub struct Qualified {
    /// The shape values are counted against.
    pub shape: ShapeIdx,
    /// `sh:qualifiedMinCount`.
    pub min: Option<usize>,
    /// `sh:qualifiedMaxCount`.
    pub max: Option<usize>,
    /// `sh:qualifiedValueShapesDisjoint`.
    pub disjoint: bool,
    /// Sibling qualified shapes, used when `disjoint` is set.
    pub siblings: Vec<ShapeIdx>,
}

impl Constraint {
    /// The `sh:sourceConstraintComponent` a violation of this constraint reports.
    #[must_use]
    pub fn component(&self, sh: &Sh) -> TermId {
        match self {
            Self::Class(_) => sh.class_component,
            Self::Datatype(_) => sh.datatype_component,
            Self::NodeKind(_) => sh.node_kind_component,
            Self::MinCount(_) => sh.min_count_component,
            Self::MaxCount(_) => sh.max_count_component,
            Self::MinInclusive(_) => sh.min_inclusive_component,
            Self::MaxInclusive(_) => sh.max_inclusive_component,
            Self::MinExclusive(_) => sh.min_exclusive_component,
            Self::MaxExclusive(_) => sh.max_exclusive_component,
            Self::MinLength(_) => sh.min_length_component,
            Self::MaxLength(_) => sh.max_length_component,
            Self::Pattern(_) => sh.pattern_component,
            Self::LanguageIn(_) => sh.language_in_component,
            Self::UniqueLang => sh.unique_lang_component,
            Self::MinListLength(_) => sh.min_list_length_component,
            Self::MaxListLength(_) => sh.max_list_length_component,
            Self::UniqueMembers => sh.unique_members_component,
            Self::MemberShape(_) => sh.member_shape_component,
            Self::SingleLine => sh.single_line_component,
            Self::SubsetOf(_) => sh.subset_of_component,
            Self::UniqueValuesFor(_) => sh.unique_values_for_component,
            Self::Equals(_) => sh.equals_component,
            Self::Disjoint(_) => sh.disjoint_component,
            Self::LessThan(_) => sh.less_than_component,
            Self::LessThanOrEquals(_) => sh.less_than_or_equals_component,
            Self::Not(_) => sh.not_component,
            Self::And(_) => sh.and_component,
            Self::Or(_) => sh.or_component,
            Self::Xone(_) => sh.xone_component,
            Self::Node(_) => sh.node_component,
            Self::Property(_) => sh.property_component,
            Self::Qualified(q) => {
                if q.min.is_some() {
                    sh.qualified_min_count_component
                } else {
                    sh.qualified_max_count_component
                }
            }
            Self::Closed(_) => sh.closed_component,
            Self::HasValue(_) => sh.has_value_component,
            Self::In(_) => sh.in_component,
        }
    }
}

/// One compiled shape.
#[derive(Debug, Clone)]
pub struct Shape {
    /// The shape's own node in the shapes graph.
    pub id: TermId,
    /// `sh:path`, present exactly on property shapes.
    pub path: Option<PathIdx>,
    /// The `sh:path` object as it appears in the shapes graph.
    ///
    /// Kept alongside the compiled path because a validation result reports
    /// `sh:resultPath`, and for a compound path that means copying the path's own
    /// triples into the report rather than naming a blank node the reader cannot resolve.
    pub path_node: Option<TermId>,
    /// How this shape finds focus nodes. Empty for a shape only reached by reference.
    pub targets: Vec<Target>,
    /// The constraints to check.
    pub constraints: Vec<Constraint>,
    /// `sh:severity`, defaulting to `sh:Violation`.
    pub severity: TermId,
    /// `sh:message`, in shapes-graph order.
    pub messages: Vec<TermId>,
    /// `sh:deactivated` — the shape is compiled but never evaluated.
    pub deactivated: bool,
}

/// A compiled shapes graph.
#[derive(Debug, Clone)]
pub struct Shapes {
    shapes: Vec<Shape>,
    paths: Vec<Path>,
    by_id: FxHashMap<TermId, ShapeIdx>,
    targeted: Vec<ShapeIdx>,
    /// Predicate → shapes whose result could change when a quad on that predicate does.
    by_predicate: FxHashMap<TermId, Vec<ShapeIdx>>,
    /// Class → shapes that target it, so a new `rdf:type` finds its shapes.
    by_target_class: FxHashMap<TermId, Vec<ShapeIdx>>,
    /// Shape → the shapes that reference it.
    ///
    /// Needed by incremental revalidation. A change usually implicates an *anonymous*
    /// property shape, which has no targets of its own and is only ever evaluated through
    /// its parent — so the work has to be attributed upwards to a shape that does have
    /// targets, or the change is silently ignored.
    parents: FxHashMap<ShapeIdx, Vec<ShapeIdx>>,
}

impl Shapes {
    /// Every compiled shape.
    #[must_use]
    pub fn all(&self) -> &[Shape] {
        &self.shapes
    }

    /// One shape by index.
    #[must_use]
    pub fn shape(&self, idx: ShapeIdx) -> &Shape {
        &self.shapes[idx.0 as usize]
    }

    /// One path by index.
    #[must_use]
    pub fn path(&self, idx: PathIdx) -> &Path {
        &self.paths[idx.0 as usize]
    }

    /// The shape a node denotes, if it was compiled.
    #[must_use]
    pub fn by_node(&self, id: TermId) -> Option<ShapeIdx> {
        self.by_id.get(&id).copied()
    }

    /// Shapes that have targets — the entry points for a full validation.
    #[must_use]
    pub fn targeted(&self) -> &[ShapeIdx] {
        &self.targeted
    }

    /// Shapes whose result could change when a quad using `predicate` is added or removed.
    ///
    /// This is the index that makes incremental revalidation cost the size of the change
    /// rather than the size of the graph (`DESIGN.md` §8).
    #[must_use]
    pub fn shapes_touching(&self, predicate: TermId) -> &[ShapeIdx] {
        self.by_predicate.get(&predicate).map_or(&[], Vec::as_slice)
    }

    /// Shapes targeting a class.
    #[must_use]
    pub fn shapes_targeting_class(&self, class: TermId) -> &[ShapeIdx] {
        self.by_target_class.get(&class).map_or(&[], Vec::as_slice)
    }

    /// The targeted shapes a shape is evaluated under.
    ///
    /// Walks the reference graph upwards until it reaches shapes that have targets. For a
    /// shape that is itself targeted, that is the shape itself. For an anonymous property
    /// shape it is whichever node shape declares it.
    #[must_use]
    pub fn targeted_ancestors(&self, idx: ShapeIdx) -> Vec<ShapeIdx> {
        let mut out = Vec::new();
        let mut seen: FxHashSet<ShapeIdx> = FxHashSet::default();
        let mut frontier = vec![idx];
        while let Some(current) = frontier.pop() {
            if !seen.insert(current) {
                continue;
            }
            if !self.shape(current).targets.is_empty() {
                out.push(current);
                // Keep climbing: a targeted shape can itself be nested inside another.
            }
            if let Some(parents) = self.parents.get(&current) {
                frontier.extend(parents.iter().copied());
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// How many shapes were compiled.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    /// Whether the shapes graph produced nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }
}

/// Every shape a shape refers to.
fn referenced_shapes(shape: &Shape) -> Vec<ShapeIdx> {
    let mut out = Vec::new();
    for constraint in &shape.constraints {
        match constraint {
            Constraint::Not(i)
            | Constraint::Node(i)
            | Constraint::Property(i)
            | Constraint::MemberShape(i) => out.push(*i),
            Constraint::And(v) | Constraint::Or(v) | Constraint::Xone(v) => {
                out.extend(v.iter().copied());
            }
            Constraint::Qualified(q) => {
                out.push(q.shape);
                out.extend(q.siblings.iter().copied());
            }
            _ => {}
        }
    }
    out
}

/// Builds [`Shapes`] from a shapes graph.
pub struct Compiler<'a> {
    graph: GraphView<'a>,
    sh: &'a Sh,
    shapes: Vec<Shape>,
    paths: Vec<Path>,
    by_id: FxHashMap<TermId, ShapeIdx>,
    in_progress: FxHashSet<TermId>,
}

impl<'a> Compiler<'a> {
    /// Prepares a compiler over a shapes graph.
    #[must_use]
    pub fn new(graph: GraphView<'a>, sh: &'a Sh) -> Self {
        Self {
            graph,
            sh,
            shapes: Vec::new(),
            paths: Vec::new(),
            by_id: FxHashMap::default(),
            in_progress: FxHashSet::default(),
        }
    }

    /// Compiles every shape the graph declares.
    pub fn compile(mut self) -> Result<Shapes, ShaclError> {
        for node in self.root_shape_nodes()? {
            self.shape_for(node)?;
        }

        let mut targeted: Vec<ShapeIdx> = (0..self.shapes.len())
            .map(|i| ShapeIdx(u32::try_from(i).unwrap_or(u32::MAX)))
            .filter(|i| !self.shapes[i.0 as usize].targets.is_empty())
            .collect();
        // Deterministic order in, deterministic report out.
        targeted.sort_unstable_by_key(|i| self.shapes[i.0 as usize].id);

        let mut by_predicate: FxHashMap<TermId, Vec<ShapeIdx>> = FxHashMap::default();
        let mut by_target_class: FxHashMap<TermId, Vec<ShapeIdx>> = FxHashMap::default();
        for (i, shape) in self.shapes.iter().enumerate() {
            let idx = ShapeIdx(u32::try_from(i).unwrap_or(u32::MAX));
            for target in &shape.targets {
                match target {
                    Target::Class(c) => by_target_class.entry(*c).or_default().push(idx),
                    Target::SubjectsOf(p) | Target::ObjectsOf(p) => {
                        by_predicate.entry(*p).or_default().push(idx);
                    }
                    Target::Node(_) => {}
                }
            }
            for predicate in self.predicates_of(shape) {
                by_predicate.entry(predicate).or_default().push(idx);
            }
        }
        for shapes in by_predicate
            .values_mut()
            .chain(by_target_class.values_mut())
        {
            shapes.sort_unstable();
            shapes.dedup();
        }

        let mut parents: FxHashMap<ShapeIdx, Vec<ShapeIdx>> = FxHashMap::default();
        for (i, shape) in self.shapes.iter().enumerate() {
            let idx = ShapeIdx(u32::try_from(i).unwrap_or(u32::MAX));
            for child in referenced_shapes(shape) {
                parents.entry(child).or_default().push(idx);
            }
        }
        for entry in parents.values_mut() {
            entry.sort_unstable();
            entry.dedup();
        }

        Ok(Shapes {
            shapes: self.shapes,
            paths: self.paths,
            by_id: self.by_id,
            targeted,
            by_predicate,
            by_target_class,
            parents,
        })
    }

    /// Predicates a shape reads, for the dependency index.
    ///
    /// Only the shape's *own* path and the predicates its comparison constraints name;
    /// nested shapes contribute their own entries because they are compiled as shapes in
    /// their own right.
    fn predicates_of(&self, shape: &Shape) -> Vec<TermId> {
        let mut out = Vec::new();
        if let Some(path) = shape.path {
            self.path_predicates(path, &mut out);
        }
        for constraint in &shape.constraints {
            match constraint {
                Constraint::Equals(p) | Constraint::Disjoint(p) => out.push(*p),
                // Path-valued comparisons. Their predicates have to be walked out of the
                // path rather than taken directly, or incremental revalidation stops
                // noticing writes to them — the constraint would still be evaluated on a
                // full run and silently skipped on a partial one.
                Constraint::LessThan(path)
                | Constraint::LessThanOrEquals(path)
                | Constraint::SubsetOf(path) => self.path_predicates(*path, &mut out),
                // A composite key reads each of its properties.
                Constraint::UniqueValuesFor(properties) => out.extend(properties.iter().copied()),
                // A class constraint reads rdf:type, so a new type triple must reach it.
                Constraint::Class(_) => out.push(self.sh.rdf_type),
                _ => {}
            }
        }
        out
    }

    fn path_predicates(&self, path: PathIdx, out: &mut Vec<TermId>) {
        match &self.paths[path.0 as usize] {
            Path::Predicate(p) => out.push(*p),
            Path::Inverse(p) | Path::ZeroOrMore(p) | Path::OneOrMore(p) | Path::ZeroOrOne(p) => {
                self.path_predicates(*p, out);
            }
            Path::Sequence(ps) | Path::Alternative(ps) => {
                for p in ps {
                    self.path_predicates(*p, out);
                }
            }
        }
    }

    /// Nodes that are shapes in their own right: anything declared a node or property
    /// shape, anything carrying a target, and anything with a `sh:path`.
    fn root_shape_nodes(&self) -> Result<Vec<TermId>, ShaclError> {
        let mut nodes = Vec::new();
        for class in [self.sh.node_shape, self.sh.property_shape] {
            nodes.extend(self.graph.subjects(self.sh.rdf_type, class)?);
        }
        for parameter in [
            self.sh.target_class,
            self.sh.target_node,
            self.sh.target_subjects_of,
            self.sh.target_objects_of,
            self.sh.path,
        ] {
            nodes.extend(self.graph.subjects_of(parameter)?);
        }
        nodes.sort_unstable();
        nodes.dedup();
        Ok(nodes)
    }

    /// Compiles the shape a node denotes, reusing it if already compiled.
    fn shape_for(&mut self, node: TermId) -> Result<ShapeIdx, ShaclError> {
        if let Some(existing) = self.by_id.get(&node) {
            return Ok(*existing);
        }
        if !self.in_progress.insert(node) {
            // A shape referring to itself is legal; the *evaluator* breaks the cycle with
            // a depth limit. The compiler only has to avoid recursing forever, so it hands
            // back a placeholder that the outer call will fill in.
            return Ok(self.reserve(node));
        }
        let idx = self.reserve(node);
        let shape = self.build(node, idx)?;
        self.shapes[idx.0 as usize] = shape;
        self.in_progress.remove(&node);
        Ok(idx)
    }

    fn reserve(&mut self, node: TermId) -> ShapeIdx {
        if let Some(existing) = self.by_id.get(&node) {
            return *existing;
        }
        let idx = ShapeIdx(u32::try_from(self.shapes.len()).unwrap_or(u32::MAX));
        self.shapes.push(Shape {
            id: node,
            path: None,
            path_node: None,
            targets: Vec::new(),
            constraints: Vec::new(),
            severity: self.sh.violation,
            messages: Vec::new(),
            deactivated: false,
        });
        self.by_id.insert(node, idx);
        idx
    }

    fn build(&mut self, node: TermId, _idx: ShapeIdx) -> Result<Shape, ShaclError> {
        let g = self.graph;
        let sh = self.sh;

        let path_node = g.object(node, sh.path)?;
        let path = match path_node {
            Some(p) => Some(self.compile_path(p, 0)?),
            None => None,
        };

        let mut targets = Vec::new();
        for n in g.objects(node, sh.target_node)? {
            targets.push(Target::Node(n));
        }
        for c in g.objects(node, sh.target_class)? {
            targets.push(Target::Class(c));
        }
        for p in g.objects(node, sh.target_subjects_of)? {
            targets.push(Target::SubjectsOf(p));
        }
        for p in g.objects(node, sh.target_objects_of)? {
            targets.push(Target::ObjectsOf(p));
        }
        // Implicit class target: a shape that is also an rdfs:Class targets its instances.
        if g.has(node, sh.rdf_type, sh.rdfs_class)?
            && (g.has(node, sh.rdf_type, sh.node_shape)?
                || g.has(node, sh.rdf_type, sh.property_shape)?)
        {
            targets.push(Target::Class(node));
        }

        let severity = g.object(node, sh.severity)?.unwrap_or(sh.violation);
        let messages = g.objects(node, sh.message)?;
        let deactivated = matches!(g.object(node, sh.deactivated)?, Some(v) if self.is_true(v));

        let constraints = self.compile_constraints(node)?;

        Ok(Shape {
            id: node,
            path,
            path_node,
            targets,
            constraints,
            severity,
            messages,
            deactivated,
        })
    }

    fn is_true(&self, id: TermId) -> bool {
        self.graph
            .term(id)
            .ok()
            .flatten()
            .is_some_and(|t| matches!(&t, Term::Literal(l) if l.value() == "true"))
    }

    fn integer(&self, id: TermId) -> Option<usize> {
        match self.graph.term(id).ok().flatten() {
            Some(Term::Literal(l)) => l.value().parse().ok(),
            _ => None,
        }
    }

    fn string(&self, id: TermId) -> Option<String> {
        match self.graph.term(id).ok().flatten() {
            Some(Term::Literal(l)) => Some(l.value().to_owned()),
            Some(Term::NamedNode(n)) => Some(n.into_string()),
            _ => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn compile_constraints(&mut self, node: TermId) -> Result<Vec<Constraint>, ShaclError> {
        let g = self.graph;
        let sh = self.sh;
        let mut out = Vec::new();

        for c in g.objects(node, sh.class)? {
            out.push(Constraint::Class(c));
        }
        for d in g.objects(node, sh.datatype)? {
            let datatypes = if g.object(d, sh.rdf_first)?.is_some() {
                self.list(d)?
            } else {
                vec![d]
            };
            if !datatypes.is_empty() {
                out.push(Constraint::Datatype(datatypes));
            }
        }
        for k in g.objects(node, sh.node_kind)? {
            // SHACL 1.2 allows a *list* of kinds — `sh:nodeKind ( sh:BlankNode sh:IRI )` —
            // as well as a single one. The list is a union: a value satisfying any of them
            // satisfies the constraint, which is why the flags are or-ed rather than one
            // constraint being pushed per member.
            let kinds = if g.object(k, sh.rdf_first)?.is_some() {
                self.list(k)?
            } else {
                vec![k]
            };
            let mut spec = NodeKindSpec {
                iri: false,
                blank: false,
                literal: false,
            };
            let mut any = false;
            for kind in kinds {
                if let Some(one) = self.node_kind_spec(kind) {
                    spec.iri |= one.iri;
                    spec.blank |= one.blank;
                    spec.literal |= one.literal;
                    any = true;
                }
            }
            if any {
                out.push(Constraint::NodeKind(spec));
            }
        }
        if let Some(v) = g.object(node, sh.min_count)?.and_then(|v| self.integer(v)) {
            out.push(Constraint::MinCount(v));
        }
        if let Some(v) = g.object(node, sh.max_count)?.and_then(|v| self.integer(v)) {
            out.push(Constraint::MaxCount(v));
        }
        for (parameter, make) in [
            (
                sh.min_inclusive,
                Constraint::MinInclusive as fn(TermId) -> Constraint,
            ),
            (sh.max_inclusive, Constraint::MaxInclusive),
            (sh.min_exclusive, Constraint::MinExclusive),
            (sh.max_exclusive, Constraint::MaxExclusive),
            (sh.has_value, Constraint::HasValue),
            (sh.equals, Constraint::Equals),
            (sh.disjoint, Constraint::Disjoint),
        ] {
            for v in g.objects(node, parameter)? {
                out.push(make(v));
            }
        }
        // `sh:lessThan` and `sh:lessThanOrEquals` take a *path* in SHACL 1.2, not just a
        // predicate — `sh:lessThan ( ex:a ex:b )`. Read as an IRI a sequence path finds
        // nothing to compare against, and the constraint silently passes.
        for (parameter, make) in [
            (
                sh.less_than,
                Constraint::LessThan as fn(PathIdx) -> Constraint,
            ),
            (sh.less_than_or_equals, Constraint::LessThanOrEquals),
        ] {
            for v in g.objects(node, parameter)? {
                let path = self.compile_path(v, 0)?;
                out.push(make(path));
            }
        }
        if let Some(v) = g.object(node, sh.min_length)?.and_then(|v| self.integer(v)) {
            out.push(Constraint::MinLength(v));
        }
        if let Some(v) = g.object(node, sh.max_length)?.and_then(|v| self.integer(v)) {
            out.push(Constraint::MaxLength(v));
        }
        for p in g.objects(node, sh.pattern)? {
            let Some(source) = self.string(p) else {
                continue;
            };
            let flags = g.object(node, sh.flags)?.and_then(|f| self.string(f));
            let mut builder = RegexBuilder::new(&source);
            if let Some(flags) = &flags {
                builder.case_insensitive(flags.contains('i'));
                builder.dot_matches_new_line(flags.contains('s'));
                builder.multi_line(flags.contains('m'));
                builder.ignore_whitespace(flags.contains('x'));
            }
            match builder.build() {
                Ok(regex) => out.push(Constraint::Pattern(Box::new(CompiledPattern {
                    regex,
                    source: p,
                }))),
                // An uncompilable pattern is an ill-formed shapes graph. Reported once at
                // compile time rather than as a mystery per value node.
                Err(e) => {
                    return Err(ShaclError::IllFormedShape(format!(
                        "sh:pattern {source:?} does not compile: {e}"
                    )))
                }
            }
        }
        if let Some(list) = g.object(node, sh.language_in)? {
            let langs = self.list(list)?;
            out.push(Constraint::LanguageIn(
                langs.into_iter().filter_map(|l| self.string(l)).collect(),
            ));
        }
        if matches!(g.object(node, sh.unique_lang)?, Some(v) if self.is_true(v)) {
            out.push(Constraint::UniqueLang);
        }
        if let Some(list) = g.object(node, sh.r#in)? {
            out.push(Constraint::In(self.list(list)?));
        }
        if let Some(v) = g
            .object(node, sh.min_list_length)?
            .and_then(|v| self.integer(v))
        {
            out.push(Constraint::MinListLength(v));
        }
        if let Some(v) = g
            .object(node, sh.max_list_length)?
            .and_then(|v| self.integer(v))
        {
            out.push(Constraint::MaxListLength(v));
        }
        if matches!(g.object(node, sh.unique_members)?, Some(v) if self.is_true(v)) {
            out.push(Constraint::UniqueMembers);
        }
        if matches!(g.object(node, sh.single_line)?, Some(v) if self.is_true(v)) {
            out.push(Constraint::SingleLine);
        }
        for p in g.objects(node, sh.subset_of)? {
            let path = self.compile_path(p, 0)?;
            out.push(Constraint::SubsetOf(path));
        }
        for k in g.objects(node, sh.unique_values_for)? {
            // A list is a composite key; a bare IRI is a key of one. Detected by looking for
            // `rdf:first` rather than by trying to read a list and seeing what happens,
            // because a property IRI that happens to have an `rdf:first` in the shapes graph
            // is not a thing that occurs and a silent empty key would be.
            let properties = if g.object(k, sh.rdf_first)?.is_some() {
                self.list(k)?
            } else {
                vec![k]
            };
            if !properties.is_empty() {
                out.push(Constraint::UniqueValuesFor(properties));
            }
        }

        // Logical constraints and nested shapes.
        for n in g.objects(node, sh.not)? {
            let idx = self.shape_for(n)?;
            out.push(Constraint::Not(idx));
        }
        for n in g.objects(node, sh.member_shape)? {
            let idx = self.shape_for(n)?;
            out.push(Constraint::MemberShape(idx));
        }
        for (parameter, make) in [
            (sh.and, Constraint::And as fn(Vec<ShapeIdx>) -> Constraint),
            (sh.or, Constraint::Or),
            (sh.xone, Constraint::Xone),
        ] {
            for list in g.objects(node, parameter)? {
                let members = self.list(list)?;
                let mut indices = Vec::with_capacity(members.len());
                for m in members {
                    indices.push(self.shape_for(m)?);
                }
                out.push(make(indices));
            }
        }
        for n in g.objects(node, sh.node)? {
            let idx = self.shape_for(n)?;
            out.push(Constraint::Node(idx));
        }
        for n in g.objects(node, sh.property)? {
            let idx = self.shape_for(n)?;
            out.push(Constraint::Property(idx));
        }

        // Qualified value shapes. Siblings matter only when disjoint is set.
        let qualified = g.objects(node, sh.qualified_value_shape)?;
        if !qualified.is_empty() {
            let min = g
                .object(node, sh.qualified_min_count)?
                .and_then(|v| self.integer(v));
            let max = g
                .object(node, sh.qualified_max_count)?
                .and_then(|v| self.integer(v));
            let disjoint = matches!(g.object(node, sh.qualified_value_shapes_disjoint)?, Some(v) if self.is_true(v));
            let siblings = if disjoint {
                self.sibling_qualified_shapes(node)?
            } else {
                Vec::new()
            };
            for q in qualified {
                let shape = self.shape_for(q)?;
                // A min and a max are two separate constraints: they report different
                // components, and the suite checks both.
                if min.is_some() {
                    out.push(Constraint::Qualified(Box::new(Qualified {
                        shape,
                        min,
                        max: None,
                        disjoint,
                        siblings: siblings.clone(),
                    })));
                }
                if max.is_some() {
                    out.push(Constraint::Qualified(Box::new(Qualified {
                        shape,
                        min: None,
                        max,
                        disjoint,
                        siblings: siblings.clone(),
                    })));
                }
            }
        }

        if matches!(g.object(node, sh.closed)?, Some(v) if self.is_true(v)) {
            let mut ignored = Vec::new();
            if let Some(list) = g.object(node, sh.ignored_properties)? {
                ignored = self.list(list)?;
            }
            out.push(Constraint::Closed(ignored));
        }

        Ok(out)
    }

    /// The other `sh:qualifiedValueShape`s under the same parent shape.
    fn sibling_qualified_shapes(&mut self, node: TermId) -> Result<Vec<ShapeIdx>, ShaclError> {
        let g = self.graph;
        let sh = self.sh;
        let mut out = Vec::new();
        for parent in g.subjects(sh.property, node)? {
            for sibling in g.objects(parent, sh.property)? {
                if sibling == node {
                    continue;
                }
                for q in g.objects(sibling, sh.qualified_value_shape)? {
                    let idx = self.shape_for(q)?;
                    out.push(idx);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn node_kind_spec(&self, kind: TermId) -> Option<NodeKindSpec> {
        let sh = self.sh;
        let spec = |iri, blank, literal| {
            Some(NodeKindSpec {
                iri,
                blank,
                literal,
            })
        };
        match kind {
            k if k == sh.iri => spec(true, false, false),
            k if k == sh.blank_node => spec(false, true, false),
            k if k == sh.literal => spec(false, false, true),
            k if k == sh.iri_or_literal => spec(true, false, true),
            k if k == sh.blank_node_or_iri => spec(true, true, false),
            k if k == sh.blank_node_or_literal => spec(false, true, true),
            _ => None,
        }
    }

    fn list(&self, head: TermId) -> StoreResult<Vec<TermId>> {
        self.graph
            .list(head, self.sh.rdf_first, self.sh.rdf_rest, self.sh.rdf_nil)
    }

    /// Compiles a path expression, bounded so a cyclic shapes graph cannot hang the
    /// compiler.
    fn compile_path(&mut self, node: TermId, depth: u32) -> Result<PathIdx, ShaclError> {
        if depth > 32 {
            return Err(ShaclError::IllFormedShape(
                "property path nests more than 32 deep".to_owned(),
            ));
        }
        let g = self.graph;
        let sh = self.sh;

        // A path that is an IRI is a predicate; anything else is a blank-node expression.
        if !matches!(
            holos_core::Tag::from_bits((node.to_raw() >> holos_core::TAG_SHIFT) as u8),
            holos_core::Tag::BlankNode
        ) {
            return Ok(self.push_path(Path::Predicate(node)));
        }

        for (parameter, make) in [
            (sh.inverse_path, 0u8),
            (sh.zero_or_more_path, 1),
            (sh.one_or_more_path, 2),
            (sh.zero_or_one_path, 3),
        ] {
            if let Some(inner) = g.object(node, parameter)? {
                let inner = self.compile_path(inner, depth + 1)?;
                return Ok(self.push_path(match make {
                    0 => Path::Inverse(inner),
                    1 => Path::ZeroOrMore(inner),
                    2 => Path::OneOrMore(inner),
                    _ => Path::ZeroOrOne(inner),
                }));
            }
        }
        if let Some(list) = g.object(node, sh.alternative_path)? {
            let members = self.list(list)?;
            let mut compiled = Vec::with_capacity(members.len());
            for m in members {
                compiled.push(self.compile_path(m, depth + 1)?);
            }
            return Ok(self.push_path(Path::Alternative(compiled)));
        }
        // Anything else that is a blank node with rdf:first is a sequence path.
        if g.object(node, sh.rdf_first)?.is_some() {
            let members = self.list(node)?;
            let mut compiled = Vec::with_capacity(members.len());
            for m in members {
                compiled.push(self.compile_path(m, depth + 1)?);
            }
            return Ok(self.push_path(Path::Sequence(compiled)));
        }
        Err(ShaclError::IllFormedShape(
            "sh:path is a blank node with no recognised path expression".to_owned(),
        ))
    }

    fn push_path(&mut self, path: Path) -> PathIdx {
        let idx = PathIdx(u32::try_from(self.paths.len()).unwrap_or(u32::MAX));
        self.paths.push(path);
        idx
    }
}
