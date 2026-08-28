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
use rustc_hash::FxHashSet;
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
            for focus in self.focus_nodes(*idx)? {
                self.validate_shape(*idx, focus, 0, &mut results)?;
            }
        }
        Ok(Report::new(results))
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
        for constraint in &shape.constraints {
            self.check(shape, constraint, focus, &values, depth, results)?;
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
            Constraint::Datatype(datatype) => {
                for &v in values {
                    // The datatype has to match *and* the lexical form has to be valid for
                    // it. `"aldi"^^xsd:integer` is a perfectly well-formed RDF term and
                    // not an integer; SHACL calls that a violation, so a datatype IRI
                    // comparison alone is not enough.
                    if self.datatype_of(v)? != Some(*datatype) || !self.well_formed(v)? {
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
            Constraint::Equals(predicate) => {
                let others = self.data.objects(focus, *predicate)?;
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
            Constraint::Disjoint(predicate) => {
                let others = self.data.objects(focus, *predicate)?;
                for &v in values {
                    if others.contains(&v) {
                        violate_value(v, results);
                    }
                }
            }
            Constraint::LessThan(predicate) | Constraint::LessThanOrEquals(predicate) => {
                let inclusive = matches!(constraint, Constraint::LessThanOrEquals(_));
                let others = self.data.objects(focus, *predicate)?;
                for &v in values {
                    for &o in &others {
                        let ok = match self.compare(v, o)? {
                            Some(Ordering::Less) => true,
                            Some(Ordering::Equal) => inclusive,
                            _ => false,
                        };
                        if !ok {
                            violate_value(v, results);
                            break;
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
            Constraint::Closed(ignored) => {
                let allowed = self.allowed_predicates(shape, ignored);
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

    fn language_of(&self, id: TermId) -> Result<Option<String>, ShaclError> {
        Ok(match self.data.term(id)? {
            Some(Term::Literal(l)) => l.language().map(str::to_owned),
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
        if la.datatype() == lb.datatype() {
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
        d if d == xsd("integer")
            || d == xsd("long")
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
            value.parse::<xs::Integer>().is_ok()
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
