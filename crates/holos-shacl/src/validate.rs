//! Constraint evaluation.
//!
//! Everything works on [`TermId`]s. Two consequences of the §5 encoding show up directly
//! here and are worth naming, because they are the reason the validator reads the store's
//! ids rather than a private copy:
//!
//! - **`sh:datatype` often needs no dictionary lookup.** An inline id carries its datatype
//!   in its tag, so `xsd:integer` is a tag comparison rather than a decode.
//! - **The range constraints are id comparisons.** Inline integers, floats and instants
//!   are encoded order-preservingly, so `sh:minInclusive` on them is `<=` on the ids.
//!   [`compare`] falls back to decoding only when a value did not inline.

use crate::access::{is_literal, node_kind, GraphView, NodeKind};
use crate::ir::{Constraint, Path, PathIdx, Shape, ShapeIdx, Shapes};
use crate::vocab::Sh;
use crate::{Report, ShaclError, ValidationResult};
use holos_core::{Tag, TermId};
use oxrdf::vocab::xsd;
use oxrdf::Term;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Ordering;

/// How deep shape references may nest before the validator gives up.
///
/// SHACL_Engine uses the same limit and for the same reason: recursion through
/// `sh:node` and the logical constraints is bounded by the *stack*, while walking data is
/// not. A shapes graph that references itself is legal, so this has to be a limit rather
/// than an error at compile time.
pub const MAX_DEPTH: u32 = 48;

/// Evaluates compiled shapes against a data graph.
pub struct Validator<'a> {
    shapes: &'a Shapes,
    data: GraphView<'a>,
    shapes_graph: GraphView<'a>,
    sh: &'a Sh,
}

impl<'a> Validator<'a> {
    /// Prepares a validator.
    ///
    /// `shapes_graph` is needed only to render property paths into the report; constraint
    /// evaluation never reads it, which is the compile-once property of `DESIGN.md` §8.
    #[must_use]
    pub fn new(
        shapes: &'a Shapes,
        data: GraphView<'a>,
        shapes_graph: GraphView<'a>,
        sh: &'a Sh,
    ) -> Self {
        Self {
            shapes,
            data,
            shapes_graph,
            sh,
        }
    }

    /// Validates every targeted shape.
    pub fn validate_all(&self) -> Result<Report, ShaclError> {
        let mut results = Vec::new();
        for idx in self.shapes.targeted() {
            let focus_nodes = self.focus_nodes(*idx)?;
            for focus in &focus_nodes {
                self.validate_shape(*idx, *focus, 0, &mut results)?;
            }
            // `sh:uniqueValuesFor` compares focus nodes with each other, so it cannot be
            // answered while looking at one of them. It runs here, once per shape, with the
            // whole target set in hand.
            self.unique_values_for(*idx, &focus_nodes, &mut results)?;
        }
        self.data_declared_targets(&mut results)?;
        Ok(Report::new(results))
    }

    /// `sh:shape` — targets the *data* declares for itself.
    ///
    /// Every other target is written on the shape, and so is known when the shapes graph is
    /// compiled. This one is written on the node: `ex:Thing sh:shape ex:SomeShape` says
    /// "validate me against that", which the compiler cannot see because it only reads the
    /// shapes graph.
    ///
    /// So it runs as its own pass rather than as a `Target` variant. Making every shape
    /// carry an implicit data-declared target would work, but it would also make every shape
    /// *targeted*, and `targeted` is what incremental revalidation and `targeted_ancestors`
    /// are built on — a change with a much wider blast radius than the feature deserves.
    fn data_declared_targets(&self, results: &mut Vec<ValidationResult>) -> Result<(), ShaclError> {
        for quad in
            self.data
                .store()
                .quads_for_pattern(None, Some(self.sh.shape), None, self.data.graph())
        {
            let quad = quad?;
            let Some(idx) = self.shapes.by_node(quad.object) else {
                // A node naming something that is not a shape is not an error: the shapes
                // graph simply has nothing to say about it.
                continue;
            };
            self.validate_shape(idx, quad.subject, 0, results)?;
        }
        Ok(())
    }

    /// `sh:uniqueValuesFor`, across a shape's whole target set.
    ///
    /// The key is a *tuple* of values, one per named property, and a node claims every
    /// combination its values allow: two notations and one scheme is two keys. Two focus
    /// nodes clash when they claim the same key, and both are reported — neither is more at
    /// fault than the other.
    ///
    /// Only targets count. A node outside the target set may hold the same value without
    /// anything being wrong, which the suite checks with a deliberately named
    /// `ex:UnrelatedNodeThatIsNotInTarget`.
    fn unique_values_for(
        &self,
        idx: ShapeIdx,
        focus_nodes: &[TermId],
        results: &mut Vec<ValidationResult>,
    ) -> Result<(), ShaclError> {
        let shape = self.shapes.shape(idx);
        for constraint in &shape.constraints {
            let Constraint::UniqueValuesFor(properties) = constraint else {
                continue;
            };
            let component = constraint.component(self.sh);
            let mut claimants: FxHashMap<Vec<TermId>, Vec<TermId>> = FxHashMap::default();
            for &focus in focus_nodes {
                for key in self.keys_of(focus, properties)? {
                    claimants.entry(key).or_default().push(focus);
                }
            }
            // Sorted so a report is a function of the data rather than of hash order.
            let mut offenders: Vec<TermId> = claimants
                .into_values()
                .filter(|nodes| nodes.len() > 1)
                .flatten()
                .collect();
            offenders.sort_unstable();
            offenders.dedup();
            for focus in offenders {
                results.push(self.result(shape, focus, None, component));
            }
        }
        Ok(())
    }

    /// Every key tuple a focus node claims, as the cross product of its values.
    ///
    /// A node missing a value for any key property claims no key at all, and so cannot
    /// collide with anything — which is right: a partial key is not a key.
    fn keys_of(
        &self,
        focus: TermId,
        properties: &[TermId],
    ) -> Result<Vec<Vec<TermId>>, ShaclError> {
        let mut keys: Vec<Vec<TermId>> = vec![Vec::new()];
        for &property in properties {
            let values = self.data.objects(focus, property)?;
            if values.is_empty() {
                return Ok(Vec::new());
            }
            let mut extended = Vec::with_capacity(keys.len() * values.len());
            for key in &keys {
                for &value in &values {
                    let mut next = key.clone();
                    next.push(value);
                    extended.push(next);
                }
            }
            keys = extended;
        }
        Ok(keys)
    }

    /// Validates a chosen set of focus nodes against a chosen set of shapes.
    ///
    /// The entry point incremental revalidation uses: the same evaluation, over the part
    /// of the graph a change could have affected.
    pub fn validate_selected(&self, work: &[(ShapeIdx, TermId)]) -> Result<Report, ShaclError> {
        let mut results = Vec::new();
        for (idx, focus) in work {
            self.validate_shape(*idx, *focus, 0, &mut results)?;
        }

        // `sh:uniqueValuesFor` compares focus nodes with each other, so it cannot be decided
        // from a partial working set: a node added by this change may collide with one the
        // change never touched. For shapes carrying it, the whole target set is recovered
        // and the constraint evaluated over all of it.
        //
        // That costs the target set for those shapes and nothing for any other, which is the
        // price of a constraint that is global by nature. Skipping it instead would make
        // incremental revalidation *unsafe* — it would miss a violation a full run finds,
        // which is the one thing §8 requires it never to do.
        let mut done: FxHashSet<usize> = FxHashSet::default();
        for (idx, _) in work {
            if !done.insert(idx.0 as usize) {
                continue;
            }
            let shape = self.shapes.shape(*idx);
            if !shape
                .constraints
                .iter()
                .any(|c| matches!(c, Constraint::UniqueValuesFor(_)))
            {
                continue;
            }
            let all = self.focus_nodes(*idx)?;
            self.unique_values_for(*idx, &all, &mut results)?;
        }

        Ok(Report::new(results))
    }

    /// The focus nodes a shape's targets select.
    pub fn focus_nodes(&self, idx: ShapeIdx) -> Result<Vec<TermId>, ShaclError> {
        let shape = self.shapes.shape(idx);
        let mut out = Vec::new();
        for target in &shape.targets {
            match *target {
                crate::ir::Target::Node(n) => out.push(n),
                crate::ir::Target::Class(c) => out.extend(self.instances_of(c)?),
                crate::ir::Target::SubjectsOf(p) => out.extend(self.data.subjects_of(p)?),
                crate::ir::Target::ObjectsOf(p) => out.extend(self.data.objects_of(p)?),
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Instances of a class, following `rdfs:subClassOf` downwards.
    fn instances_of(&self, class: TermId) -> Result<Vec<TermId>, ShaclError> {
        let mut classes = vec![class];
        let mut seen: FxHashSet<TermId> = [class].into_iter().collect();
        let mut i = 0;
        // Bounded: a cyclic class hierarchy must not hang validation.
        while i < classes.len() && classes.len() < 10_000 {
            let current = classes[i];
            i += 1;
            for sub in self.data.subjects(self.sh.rdfs_subclass_of, current)? {
                if seen.insert(sub) {
                    classes.push(sub);
                }
            }
        }
        let mut out = Vec::new();
        for c in classes {
            out.extend(self.data.subjects(self.sh.rdf_type, c)?);
        }
        Ok(out)
    }

    /// Whether a node is an instance of a class, following `rdfs:subClassOf` upwards.
    fn is_instance_of(&self, node: TermId, class: TermId) -> Result<bool, ShaclError> {
        let mut queue: Vec<TermId> = self.data.objects(node, self.sh.rdf_type)?;
        let mut seen: FxHashSet<TermId> = queue.iter().copied().collect();
        let mut i = 0;
        while i < queue.len() && queue.len() < 10_000 {
            let current = queue[i];
            i += 1;
            if current == class {
                return Ok(true);
            }
            for super_class in self.data.objects(current, self.sh.rdfs_subclass_of)? {
                if seen.insert(super_class) {
                    queue.push(super_class);
                }
            }
        }
        Ok(false)
    }

    /// Validates one shape at one focus node.
    pub fn validate_shape(
        &self,
        idx: ShapeIdx,
        focus: TermId,
        depth: u32,
        results: &mut Vec<ValidationResult>,
    ) -> Result<(), ShaclError> {
        if depth > MAX_DEPTH {
            return Err(ShaclError::TooDeep(MAX_DEPTH));
        }
        let shape = self.shapes.shape(idx);
        if shape.deactivated {
            return Ok(());
        }
        let values = match shape.path {
            Some(path) => self.eval_path(path, focus)?,
            None => vec![focus],
        };
        for (i, constraint) in shape.constraints.iter().enumerate() {
            // A `{| sh:deactivated true |}` annotation switches off one constraint, where
            // `sh:deactivated` on the shape switches off all of them.
            if shape.annotations.get(i).is_some_and(|a| a.deactivated) {
                continue;
            }
            let before = results.len();
            self.check(shape, constraint, focus, &values, depth, results)?;
            // Message and severity annotations replace the shape's own, for results this
            // constraint produced. Applied afterwards rather than threaded through `check`,
            // which would mean passing them to twenty-five arms that do not care.
            if let Some(annotation) = shape.annotations.get(i) {
                for result in &mut results[before..] {
                    if !annotation.messages.is_empty() {
                        result.messages.clone_from(&annotation.messages);
                    }
                    if let Some(severity) = annotation.severity {
                        result.severity = severity;
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a shape holds at a node, without recording results.
    ///
    /// Used by the logical constraints, which need a verdict rather than a report.
    fn holds(&self, idx: ShapeIdx, focus: TermId, depth: u32) -> Result<bool, ShaclError> {
        let mut scratch = Vec::new();
        self.validate_shape(idx, focus, depth + 1, &mut scratch)?;
        Ok(scratch.is_empty())
    }

    #[allow(clippy::too_many_lines)]
    fn check(
        &self,
        shape: &Shape,
        constraint: &Constraint,
        focus: TermId,
        values: &[TermId],
        depth: u32,
        results: &mut Vec<ValidationResult>,
    ) -> Result<(), ShaclError> {
        let component = constraint.component(self.sh);
        let violate_value = |value: TermId, results: &mut Vec<ValidationResult>| {
            results.push(self.result(shape, focus, Some(value), component));
        };

        match constraint {
            // --- per value node -------------------------------------------------------
            Constraint::Class(class) => {
                for &v in values {
                    if !self.is_instance_of(v, *class)? {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::Datatype(datatypes) => {
                for &v in values {
                    // The datatype has to match *and* the lexical form has to be valid for
                    // it. `"aldi"^^xsd:integer` is a perfectly well-formed RDF term and
                    // not an integer; SHACL calls that a violation, so a datatype IRI
                    // comparison alone is not enough.
                    let matches = self.datatype_of(v)?.is_some_and(|d| datatypes.contains(&d));
                    if !matches || !self.well_formed(v)? {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::NodeKind(spec) => {
                for &v in values {
                    let ok = match node_kind(v) {
                        NodeKind::Iri => spec.iri,
                        NodeKind::BlankNode => spec.blank,
                        NodeKind::Literal => spec.literal,
                        // An RDF 1.2 triple term is none of the three node kinds SHACL
                        // Core names, so no `sh:nodeKind` admits it.
                        NodeKind::TripleTerm => false,
                    };
                    if !ok {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::MinInclusive(bound) => {
                self.range(values, *bound, results, shape, focus, component, |o| {
                    o != Ordering::Less
                })?;
            }
            Constraint::MaxInclusive(bound) => {
                self.range(values, *bound, results, shape, focus, component, |o| {
                    o != Ordering::Greater
                })?;
            }
            Constraint::MinExclusive(bound) => {
                self.range(values, *bound, results, shape, focus, component, |o| {
                    o == Ordering::Greater
                })?;
            }
            Constraint::MaxExclusive(bound) => {
                self.range(values, *bound, results, shape, focus, component, |o| {
                    o == Ordering::Less
                })?;
            }
            Constraint::MinLength(min) => {
                for &v in values {
                    match self.lexical(v)? {
                        Some(s) if s.chars().count() >= *min => {}
                        // A length constraint on a blank node is a violation, not an error.
                        _ => violate_value(v, results),
                    }
                }
            }
            Constraint::MaxLength(max) => {
                for &v in values {
                    match self.lexical(v)? {
                        Some(s) if s.chars().count() <= *max => {}
                        _ => violate_value(v, results),
                    }
                }
            }
            Constraint::Pattern(pattern) => {
                for &v in values {
                    if node_kind(v) == NodeKind::BlankNode {
                        violate_value(v, results);
                        continue;
                    }
                    match self.lexical(v)? {
                        Some(s) if pattern.regex.is_match(&s) => {}
                        _ => violate_value(v, results),
                    }
                }
            }
            Constraint::LanguageIn(langs) => {
                for &v in values {
                    let tag = self.language_of(v)?;
                    let ok = tag.is_some_and(|t| langs.iter().any(|l| language_matches(&t, l)));
                    if !ok {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::In(allowed) => {
                for &v in values {
                    if !allowed.contains(&v) {
                        violate_value(v, results);
                    }
                }
            }
            // --- SHACL 1.2 list constraints -------------------------------------------
            //
            // These take the value node as a *handle to a structure* rather than as a value,
            // which is a different shape of check from everything above. A value node that is
            // not a well-formed list fails: `sh:minListLength` is a statement about a list,
            // and something that is not one does not satisfy it.
            Constraint::MinListLength(min) => {
                for &v in values {
                    match self.rdf_list(v)? {
                        Some(members) if members.len() >= *min => {}
                        _ => violate_value(v, results),
                    }
                }
            }
            Constraint::MaxListLength(max) => {
                for &v in values {
                    match self.rdf_list(v)? {
                        Some(members) if members.len() <= *max => {}
                        _ => violate_value(v, results),
                    }
                }
            }
            Constraint::UniqueMembers => {
                for &v in values {
                    let Some(members) = self.rdf_list(v)? else {
                        violate_value(v, results);
                        continue;
                    };
                    // The outer result names the *list*, because that is what the shape was
                    // checking; each repeated member is reported underneath it as a
                    // `sh:detail`. One detail per distinct repeated value, not one per
                    // repetition — a member appearing three times is one thing wrong, said
                    // once.
                    let mut seen = FxHashSet::default();
                    let mut repeated = Vec::new();
                    for &m in &members {
                        if !seen.insert(m) && !repeated.contains(&m) {
                            repeated.push(m);
                        }
                    }
                    if repeated.is_empty() {
                        continue;
                    }
                    let mut outer = self.result(shape, focus, Some(v), component);
                    outer.details = repeated
                        .into_iter()
                        .map(|m| self.result(shape, focus, Some(m), component))
                        .collect();
                    results.push(outer);
                }
            }
            Constraint::MemberShape(inner) => {
                for &v in values {
                    let Some(members) = self.rdf_list(v)? else {
                        violate_value(v, results);
                        continue;
                    };
                    // As with `sh:uniqueMembers`: the list is what failed, and the members
                    // that failed are the explanation. The details here are the *inner
                    // shape's own results* rather than restatements of this constraint —
                    // they name the constraint that actually rejected the member, which is
                    // the only form that tells a reader why.
                    let mut details = Vec::new();
                    for member in members {
                        self.validate_shape(*inner, member, depth + 1, &mut details)?;
                    }
                    if details.is_empty() {
                        continue;
                    }
                    let mut outer = self.result(shape, focus, Some(v), component);
                    outer.details = details;
                    results.push(outer);
                }
            }
            Constraint::SingleLine => {
                for &v in values {
                    // Non-literals have no lexical form to be single-lined, and a literal
                    // carrying a line break is what this exists to reject.
                    // Every Unicode line break, not just the two ASCII ones. The suite uses
                    // a form feed and a vertical tab precisely because an implementation
                    // that looked only for \n and \r would pass the obvious cases and miss
                    // these.
                    const BREAKS: [char; 7] = [
                        '\u{000A}', // line feed
                        '\u{000B}', // vertical tab
                        '\u{000C}', // form feed
                        '\u{000D}', // carriage return
                        '\u{0085}', // next line
                        '\u{2028}', // line separator
                        '\u{2029}', // paragraph separator
                    ];
                    let breaks = match self.data.term(v)? {
                        Some(Term::Literal(l)) => l.value().contains(BREAKS),
                        // A non-literal has no lexical form to be single-lined.
                        _ => true,
                    };
                    if breaks {
                        violate_value(v, results);
                    }
                }
            }
            // `sh:someValue` is existential where `sh:node` is universal, so the result
            // is about the focus node rather than about any one value: no single value is at
            // fault for the absence of a conforming one.
            Constraint::SomeValue(inner) => {
                let mut any = false;
                for &v in values {
                    if self.holds(*inner, v, depth)? {
                        any = true;
                        break;
                    }
                }
                if !any {
                    results.push(self.result(shape, focus, None, component));
                }
            }
            Constraint::Node(inner) => {
                for &v in values {
                    if !self.holds(*inner, v, depth)? {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::Not(inner) => {
                for &v in values {
                    if self.holds(*inner, v, depth)? {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::And(members) => {
                for &v in values {
                    let mut all = true;
                    for m in members {
                        if !self.holds(*m, v, depth)? {
                            all = false;
                            break;
                        }
                    }
                    if !all {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::Or(members) => {
                for &v in values {
                    let mut any = false;
                    for m in members {
                        if self.holds(*m, v, depth)? {
                            any = true;
                            break;
                        }
                    }
                    if !any {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::Xone(members) => {
                for &v in values {
                    let mut count = 0;
                    for m in members {
                        if self.holds(*m, v, depth)? {
                            count += 1;
                        }
                    }
                    if count != 1 {
                        violate_value(v, results);
                    }
                }
            }

            // --- over the whole value set ---------------------------------------------
            Constraint::MinCount(min) => {
                if values.len() < *min {
                    results.push(self.result(shape, focus, None, component));
                }
            }
            Constraint::MaxCount(max) => {
                if values.len() > *max {
                    results.push(self.result(shape, focus, None, component));
                }
            }
            Constraint::HasValue(expected) => {
                if !values.contains(expected) {
                    results.push(self.result(shape, focus, None, component));
                }
            }
            Constraint::UniqueLang => {
                let mut seen: FxHashSet<String> = FxHashSet::default();
                let mut duplicated: FxHashSet<String> = FxHashSet::default();
                for &v in values {
                    if let Some(lang) = self.language_of(v)? {
                        if !seen.insert(lang.clone()) {
                            duplicated.insert(lang);
                        }
                    }
                }
                for _ in 0..duplicated.len() {
                    results.push(self.result(shape, focus, None, component));
                }
            }
            // `sh:subsetOf` is `sh:equals` in one direction only: every value node has to
            // be among the other property's values, and the other property may have more.
            // Evaluated once per shape in `validate_all`, not here: it is a statement
            // about the target set rather than about this focus node.
            Constraint::UniqueValuesFor(_) => {}
            Constraint::SubsetOf(path) => {
                let others = self.eval_path(*path, focus)?;
                for &v in values {
                    if !others.contains(&v) {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::Equals(path) => {
                let others = self.eval_path(*path, focus)?;
                for &v in values {
                    if !others.contains(&v) {
                        violate_value(v, results);
                    }
                }
                for o in others {
                    if !values.contains(&o) {
                        violate_value(o, results);
                    }
                }
            }
            Constraint::Disjoint(path) => {
                let others = self.eval_path(*path, focus)?;
                for &v in values {
                    if others.contains(&v) {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::LessThan(path) | Constraint::LessThanOrEquals(path) => {
                let inclusive = matches!(constraint, Constraint::LessThanOrEquals(_));
                let others = self.eval_path(*path, focus)?;
                // One result per failing *pair*, not per failing value. `ex:first` holding
                // 1 and 2 against `ex:second` holding "a" and "b" is four incomparable
                // pairs and the suite expects four results — two of them identical, because
                // the value that failed is all a result records. Stopping at the first
                // failure for a value reports two.
                for &v in values {
                    for &o in &others {
                        let ok = match self.compare(v, o)? {
                            Some(Ordering::Less) => true,
                            Some(Ordering::Equal) => inclusive,
                            _ => false,
                        };
                        if !ok {
                            violate_value(v, results);
                        }
                    }
                }
            }
            Constraint::Property(inner) => {
                // A property constraint is evaluated at the focus node, and reports its
                // own results rather than one summary violation.
                for &v in values {
                    self.validate_shape(*inner, v, depth + 1, results)?;
                }
            }
            Constraint::Qualified(q) => {
                let mut count = 0;
                for &v in values {
                    if !self.holds(q.shape, v, depth)? {
                        continue;
                    }
                    if q.disjoint {
                        let mut conflicts = false;
                        for sibling in &q.siblings {
                            if self.holds(*sibling, v, depth)? {
                                conflicts = true;
                                break;
                            }
                        }
                        if conflicts {
                            continue;
                        }
                    }
                    count += 1;
                }
                let too_few = q.min.is_some_and(|min| count < min);
                let too_many = q.max.is_some_and(|max| count > max);
                if too_few || too_many {
                    results.push(self.result(shape, focus, None, component));
                }
            }
            Constraint::Closed { ignored, by_types } => {
                let allowed = if *by_types {
                    self.allowed_by_types(focus, ignored)?
                } else {
                    self.allowed_predicates(shape, ignored)
                };
                for quad in
                    self.data
                        .store()
                        .quads_for_pattern(Some(focus), None, None, self.data.graph())
                {
                    let quad = quad?;
                    if !allowed.contains(&quad.predicate) {
                        results.push(ValidationResult {
                            focus_node: focus,
                            path: Some(quad.predicate),
                            value: Some(quad.object),
                            source_shape: shape.id,
                            component,
                            severity: shape.severity,
                            messages: shape.messages.clone(),
                            details: Vec::new(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Predicates `sh:closed` permits: those named by the shape's property shapes, plus
    /// `sh:ignoredProperties`.
    /// The predicates `sh:closed sh:ByTypes` admits at one focus node.
    ///
    /// Every class the node is an instance of, and every class those are subclasses of,
    /// contributes the property shapes of the shape that *is* that class. So a node typed as
    /// a subclass may use its superclass's properties, and a node typed only as the
    /// superclass may not use the subclass's — which is exactly what the suite checks.
    ///
    /// Unlike `sh:closed true`, this cannot be resolved when the shape is compiled: it
    /// depends on the focus node's types, which are data.
    fn allowed_by_types(
        &self,
        focus: TermId,
        ignored: &[TermId],
    ) -> Result<FxHashSet<TermId>, ShaclError> {
        let mut allowed: FxHashSet<TermId> = ignored.iter().copied().collect();
        // `rdf:type` is always admitted here, unlike under `sh:closed true`: it is the
        // mechanism this mode closes *by*, so flagging the triple that selects the allowed
        // set would make every typed node violate.
        allowed.insert(self.sh.rdf_type);
        let mut classes = self.data.objects(focus, self.sh.rdf_type)?;
        let mut seen: FxHashSet<TermId> = classes.iter().copied().collect();
        let mut i = 0;
        // Bounded for the same reason `instances_of` is: a cyclic class hierarchy must not
        // hang validation.
        while i < classes.len() && classes.len() < 10_000 {
            let class = classes[i];
            i += 1;
            for sup in self.data.objects(class, self.sh.rdfs_subclass_of)? {
                if seen.insert(sup) {
                    classes.push(sup);
                }
            }
        }

        // Every shape that applies to a node of these classes contributes its properties:
        // the shape that *is* the class, any shape targeting the class, and anything those
        // reach through `sh:node`. A node typed as a subclass may therefore use properties
        // declared anywhere in that reachable set, which is what makes the mode "by types"
        // rather than "by this shape".
        let mut queue: Vec<ShapeIdx> = Vec::new();
        for &class in &classes {
            queue.extend(self.shapes.by_node(class));
            queue.extend(self.shapes.shapes_targeting_class(class).iter().copied());
        }
        let mut visited: FxHashSet<usize> = FxHashSet::default();
        let mut j = 0;
        while j < queue.len() {
            let idx = queue[j];
            j += 1;
            if !visited.insert(idx.0 as usize) {
                continue;
            }
            let shape = self.shapes.shape(idx);
            allowed.extend(self.allowed_predicates(shape, &[]));
            for constraint in &shape.constraints {
                if let Constraint::Node(inner) = constraint {
                    queue.push(*inner);
                }
            }
        }
        Ok(allowed)
    }

    fn allowed_predicates(&self, shape: &Shape, ignored: &[TermId]) -> FxHashSet<TermId> {
        let mut allowed: FxHashSet<TermId> = ignored.iter().copied().collect();
        for constraint in &shape.constraints {
            if let Constraint::Property(idx) = constraint {
                if let Some(path) = self.shapes.shape(*idx).path {
                    if let Path::Predicate(p) = self.shapes.path(path) {
                        allowed.insert(*p);
                    }
                }
            }
        }
        allowed
    }

    fn range(
        &self,
        values: &[TermId],
        bound: TermId,
        results: &mut Vec<ValidationResult>,
        shape: &Shape,
        focus: TermId,
        component: TermId,
        ok: impl Fn(Ordering) -> bool,
    ) -> Result<(), ShaclError> {
        for &v in values {
            let satisfied = self.compare(v, bound)?.is_some_and(&ok);
            if !satisfied {
                results.push(self.result(shape, focus, Some(v), component));
            }
        }
        Ok(())
    }

    fn result(
        &self,
        shape: &Shape,
        focus: TermId,
        value: Option<TermId>,
        component: TermId,
    ) -> ValidationResult {
        ValidationResult {
            focus_node: focus,
            path: shape.path_node,
            value,
            source_shape: shape.id,
            component,
            severity: shape.severity,
            messages: shape.messages.clone(),
            details: Vec::new(),
        }
    }

    // --- term inspection ---------------------------------------------------------------

    /// A value's datatype.
    ///
    /// Inline ids carry it in the tag, so the common numeric and string cases never touch
    /// the dictionary.
    fn datatype_of(&self, id: TermId) -> Result<Option<TermId>, ShaclError> {
        let well_known = |iri| holos_core::vocab::encode_iri(iri);
        Ok(match id.tag() {
            Tag::Integer => well_known(xsd::INTEGER.as_str()),
            Tag::Float => well_known(xsd::FLOAT.as_str()),
            Tag::DateTime => well_known(xsd::DATE_TIME.as_str()),
            Tag::Small => match self.data.term(id)? {
                Some(Term::Literal(l)) => well_known(l.datatype().as_str()),
                _ => None,
            },
            Tag::Literal => match self.data.term(id)? {
                Some(Term::Literal(l)) => self.data.id(l.datatype())?,
                _ => None,
            },
            _ => None,
        })
    }

    /// Whether a literal's lexical form is valid for its datatype.
    ///
    /// Inline ids are valid by construction: the codec only inlines a literal whose
    /// lexical form is already the canonical form for its value (`DESIGN.md` §5), so
    /// anything that inlined has already been parsed successfully. Only dictionary-backed
    /// literals need checking, which is most of the cost avoided.
    fn well_formed(&self, id: TermId) -> Result<bool, ShaclError> {
        if id.tag() != Tag::Literal {
            return Ok(true);
        }
        let Some(Term::Literal(l)) = self.data.term(id)? else {
            return Ok(true);
        };
        Ok(lexical_is_valid(l.value(), l.datatype().as_str()))
    }

    /// A value's lexical form, or `None` for a blank node.
    fn lexical(&self, id: TermId) -> Result<Option<String>, ShaclError> {
        Ok(match self.data.term(id)? {
            Some(Term::Literal(l)) => Some(l.value().to_owned()),
            Some(Term::NamedNode(n)) => Some(n.into_string()),
            _ => None,
        })
    }

    /// The members of a well-formed RDF list, or `None` if `head` is not one.
    ///
    /// `None` covers every way a list can be malformed — a cell with no `rdf:first`, more
    /// than one `rdf:rest`, or a cycle — and the callers all treat that as a violation
    /// rather than as an empty list. A constraint like `sh:minListLength` is a statement
    /// *about a list*, and something that is not a list does not satisfy it.
    ///
    /// The visited set is what makes a cyclic list terminate. `rdf:rest` loops are not
    /// expressible in Turtle's list syntax but are perfectly expressible in the triples
    /// underneath it, so a validator that trusted the shape of its input would hang on data
    /// it is supposed to be checking.
    fn rdf_list(&self, head: TermId) -> Result<Option<Vec<TermId>>, ShaclError> {
        let mut members = Vec::new();
        let mut visited = FxHashSet::default();
        let mut cell = head;
        while cell != self.sh.rdf_nil {
            if !visited.insert(cell) {
                return Ok(None);
            }
            let first = self.data.objects(cell, self.sh.rdf_first)?;
            let rest = self.data.objects(cell, self.sh.rdf_rest)?;
            let ([f], [r]) = (first.as_slice(), rest.as_slice()) else {
                return Ok(None);
            };
            members.push(*f);
            cell = *r;
        }
        Ok(Some(members))
    }

    /// A literal's language, *including its base direction*.
    ///
    /// `sh:uniqueLang` asks whether two values share a language, and RDF 1.2's
    /// `rdf:dirLangString` makes that a question with a third component. The suite settles
    /// it: `"A"@ar`, `"A"@ar--ltr` and `"A"@ar--rtl` are valid together, so the direction is
    /// part of the identity. Returning the bare language tag reports them as three
    /// duplicates of one — sixteen triples where nine were expected.
    fn language_of(&self, id: TermId) -> Result<Option<String>, ShaclError> {
        Ok(match self.data.term(id)? {
            Some(Term::Literal(l)) => l.language().map(|lang| match l.direction() {
                Some(direction) => format!("{lang}--{direction}"),
                None => lang.to_owned(),
            }),
            _ => None,
        })
    }

    /// Orders two values.
    ///
    /// The fast path is the point: two inline ids of the same tag are already in value
    /// order (`DESIGN.md` §5), so comparing them is comparing integers. Anything else
    /// decodes and compares properly.
    fn compare(&self, a: TermId, b: TermId) -> Result<Option<Ordering>, ShaclError> {
        if a.tag() == b.tag() && matches!(a.tag(), Tag::Integer | Tag::Float | Tag::DateTime) {
            return Ok(Some(a.cmp(&b)));
        }
        if !is_literal(a) || !is_literal(b) {
            return Ok(None);
        }
        let (Some(Term::Literal(la)), Some(Term::Literal(lb))) =
            (self.data.term(a)?, self.data.term(b)?)
        else {
            return Ok(None);
        };
        if let (Ok(na), Ok(nb)) = (la.value().parse::<f64>(), lb.value().parse::<f64>()) {
            // Both look numeric: compare as numbers, which is what SHACL's range
            // constraints mean even across xsd:integer and xsd:decimal.
            if is_numeric(la.datatype().as_str()) && is_numeric(lb.datatype().as_str()) {
                return Ok(na.partial_cmp(&nb));
            }
        }
        // Date and time types have an order relation of their own, and it is not the
        // lexical one. `"2002-10-10T12:00:00-05:00"` and `"2002-10-10T12:00:00"` differ only
        // by a timezone the second does not have, and XSD calls that pair *indeterminate*
        // rather than ordered: a value without a timezone could stand for any instant in a
        // 28-hour window. Comparing the strings puts them in an order and reports the
        // constraint satisfied, which is the quiet kind of wrong.
        //
        // `compare` returning `None` is already read as "not satisfied" by `range`, so an
        // indeterminate comparison becomes a violation, which is what the suite expects.
        if la.datatype() == lb.datatype() {
            if let Some(ordering) = temporal_cmp(la.datatype().as_str(), la.value(), lb.value()) {
                return Ok(ordering);
            }
            return Ok(Some(la.value().cmp(lb.value())));
        }
        Ok(None)
    }

    // --- paths --------------------------------------------------------------------------

    /// Every node reachable from `start` along a path.
    pub fn eval_path(&self, path: PathIdx, start: TermId) -> Result<Vec<TermId>, ShaclError> {
        let mut out = Vec::new();
        self.walk(path, start, &mut out)?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn walk(&self, path: PathIdx, from: TermId, out: &mut Vec<TermId>) -> Result<(), ShaclError> {
        match self.shapes.path(path) {
            Path::Predicate(p) => out.extend(self.data.objects(from, *p)?),
            Path::Inverse(inner) => {
                if let Path::Predicate(p) = self.shapes.path(*inner) {
                    out.extend(self.data.subjects(*p, from)?);
                } else {
                    // An inverse of a compound path: walk the inner path from every node
                    // and keep those that reach `from`.
                    return Err(ShaclError::Unsupported(
                        "sh:inversePath over a compound path".to_owned(),
                    ));
                }
            }
            Path::Sequence(steps) => {
                let mut current = vec![from];
                for step in steps {
                    let mut next = Vec::new();
                    for node in current {
                        self.walk(*step, node, &mut next)?;
                    }
                    next.sort_unstable();
                    next.dedup();
                    current = next;
                }
                out.extend(current);
            }
            Path::Alternative(options) => {
                for option in options {
                    self.walk(*option, from, out)?;
                }
            }
            Path::ZeroOrMore(inner) => {
                out.push(from);
                self.closure(*inner, from, out)?;
            }
            Path::OneOrMore(inner) => self.closure(*inner, from, out)?,
            Path::ZeroOrOne(inner) => {
                out.push(from);
                self.walk(*inner, from, out)?;
            }
        }
        Ok(())
    }

    /// Transitive closure, with a visited set so a cycle in the data terminates.
    fn closure(
        &self,
        inner: PathIdx,
        from: TermId,
        out: &mut Vec<TermId>,
    ) -> Result<(), ShaclError> {
        let mut seen: FxHashSet<TermId> = FxHashSet::default();
        let mut frontier = vec![from];
        while let Some(node) = frontier.pop() {
            let mut next = Vec::new();
            self.walk(inner, node, &mut next)?;
            for n in next {
                if seen.insert(n) {
                    out.push(n);
                    frontier.push(n);
                }
            }
        }
        Ok(())
    }

    /// The shapes graph, for rendering paths into a report.
    #[must_use]
    pub fn shapes_graph(&self) -> GraphView<'a> {
        self.shapes_graph
    }
}

/// Whether a lexical form parses as its datatype.
///
/// Datatypes this does not know are accepted: an unrecognised datatype has no lexical
/// space to check against, and rejecting it would fail data the validator simply does not
/// understand.
fn lexical_is_valid(value: &str, datatype: &str) -> bool {
    use oxsdatatypes as xs;
    let xsd = |n: &str| format!("http://www.w3.org/2001/XMLSchema#{n}");
    match datatype {
        // `xsd:integer` is unbounded; every type derived from it is not, and a lexical
        // form alone does not say which. `"300"^^xsd:byte` parses as an integer perfectly
        // well and is still not a byte — SHACL calls that ill-formed, and checking only the
        // lexical form reports the value as valid.
        d if d == xsd("integer") => value.parse::<xs::Integer>().is_ok(),
        d if d == xsd("long")
            || d == xsd("int")
            || d == xsd("short")
            || d == xsd("byte")
            || d == xsd("nonNegativeInteger")
            || d == xsd("positiveInteger")
            || d == xsd("nonPositiveInteger")
            || d == xsd("negativeInteger")
            || d == xsd("unsignedLong")
            || d == xsd("unsignedInt")
            || d == xsd("unsignedShort")
            || d == xsd("unsignedByte") =>
        {
            let Ok(n) = value.parse::<i128>() else {
                return false;
            };
            let (low, high): (i128, i128) = match () {
                () if d == xsd("byte") => (-128, 127),
                () if d == xsd("short") => (-32_768, 32_767),
                () if d == xsd("int") => (i128::from(i32::MIN), i128::from(i32::MAX)),
                () if d == xsd("long") => (i128::from(i64::MIN), i128::from(i64::MAX)),
                () if d == xsd("unsignedByte") => (0, 255),
                () if d == xsd("unsignedShort") => (0, 65_535),
                () if d == xsd("unsignedInt") => (0, i128::from(u32::MAX)),
                () if d == xsd("unsignedLong") => (0, i128::from(u64::MAX)),
                () if d == xsd("nonNegativeInteger") => (0, i128::MAX),
                () if d == xsd("positiveInteger") => (1, i128::MAX),
                () if d == xsd("nonPositiveInteger") => (i128::MIN, 0),
                () if d == xsd("negativeInteger") => (i128::MIN, -1),
                () => (i128::MIN, i128::MAX),
            };
            (low..=high).contains(&n)
        }
        d if d == xsd("decimal") => value.parse::<xs::Decimal>().is_ok(),
        d if d == xsd("double") => value.parse::<xs::Double>().is_ok(),
        d if d == xsd("float") => value.parse::<xs::Float>().is_ok(),
        d if d == xsd("boolean") => value.parse::<xs::Boolean>().is_ok(),
        d if d == xsd("dateTime") || d == xsd("dateTimeStamp") => {
            value.parse::<xs::DateTime>().is_ok()
        }
        d if d == xsd("duration") => value.parse::<xs::Duration>().is_ok(),
        d if d == xsd("dayTimeDuration") => value.parse::<xs::DayTimeDuration>().is_ok(),
        d if d == xsd("yearMonthDuration") => value.parse::<xs::YearMonthDuration>().is_ok(),
        _ => true,
    }
}

fn is_numeric(datatype: &str) -> bool {
    matches!(
        datatype,
        "http://www.w3.org/2001/XMLSchema#integer"
            | "http://www.w3.org/2001/XMLSchema#decimal"
            | "http://www.w3.org/2001/XMLSchema#float"
            | "http://www.w3.org/2001/XMLSchema#double"
            | "http://www.w3.org/2001/XMLSchema#long"
            | "http://www.w3.org/2001/XMLSchema#int"
            | "http://www.w3.org/2001/XMLSchema#short"
            | "http://www.w3.org/2001/XMLSchema#byte"
            | "http://www.w3.org/2001/XMLSchema#nonNegativeInteger"
            | "http://www.w3.org/2001/XMLSchema#positiveInteger"
            | "http://www.w3.org/2001/XMLSchema#nonPositiveInteger"
            | "http://www.w3.org/2001/XMLSchema#negativeInteger"
            | "http://www.w3.org/2001/XMLSchema#unsignedLong"
            | "http://www.w3.org/2001/XMLSchema#unsignedInt"
            | "http://www.w3.org/2001/XMLSchema#unsignedShort"
            | "http://www.w3.org/2001/XMLSchema#unsignedByte"
    )
}

/// BCP 47 basic filtering: `en` matches `en-GB`, but not the other way round.
fn language_matches(tag: &str, range: &str) -> bool {
    if range.is_empty() {
        return false;
    }
    let tag = tag.to_ascii_lowercase();
    let range = range.to_ascii_lowercase();
    tag == range || tag.starts_with(&format!("{range}-"))
}

/// Compares two lexical forms of a date or time type by XSD's order relation.
///
/// The outer `Option` says whether this function handled the datatype at all; the inner one
/// is the comparison, where `None` means *indeterminate* — a real XSD outcome for values
/// whose timezones leave their order undecided.
fn temporal_cmp(datatype: &str, a: &str, b: &str) -> Option<Option<Ordering>> {
    use std::str::FromStr;
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let local = datatype.strip_prefix(XSD)?;
    match local {
        "dateTime" => {
            let (a, b) = (
                oxsdatatypes::DateTime::from_str(a).ok()?,
                oxsdatatypes::DateTime::from_str(b).ok()?,
            );
            Some(a.partial_cmp(&b))
        }
        "date" => {
            let (a, b) = (
                oxsdatatypes::Date::from_str(a).ok()?,
                oxsdatatypes::Date::from_str(b).ok()?,
            );
            Some(a.partial_cmp(&b))
        }
        "time" => {
            let (a, b) = (
                oxsdatatypes::Time::from_str(a).ok()?,
                oxsdatatypes::Time::from_str(b).ok()?,
            );
            Some(a.partial_cmp(&b))
        }
        _ => None,
    }
}
