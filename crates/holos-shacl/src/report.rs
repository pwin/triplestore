//! Turning a validation report into RDF.
//!
//! SHACL_Engine's reproducibility property is worth keeping and is easy to lose: blank
//! nodes in a report get whatever labels the allocator happened to hand out, so two runs
//! over the same data produce graphs that differ textually while meaning the same thing.
//! That makes reports impossible to diff, which is exactly what an operator wants to do.
//!
//! Here the results are sorted into a total order first, and blank nodes are numbered from
//! that order. Same store, same shapes, same bytes.

use crate::access::GraphView;
use crate::vocab::Sh;
use crate::{Report, ShaclError, ValidationResult};
use holos_core::TermId;
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term};

/// Renders a report as RDF quads in the default graph.
pub fn to_quads(
    report: &Report,
    shapes_graph: GraphView<'_>,
    sh: &Sh,
) -> Result<Vec<Quad>, ShaclError> {
    let mut out = Vec::new();
    let report_node = BlankNode::new_unchecked("report");
    let iri = |id: TermId| -> Result<NamedNode, ShaclError> {
        match shapes_graph.term(id)? {
            Some(Term::NamedNode(n)) => Ok(n),
            _ => Err(ShaclError::IllFormedShape(format!(
                "{id:?} should be an IRI"
            ))),
        }
    };

    let triple = |s: Term, p: NamedNode, o: Term, out: &mut Vec<Quad>| {
        let subject = match s {
            Term::NamedNode(n) => n.into(),
            Term::BlankNode(b) => b.into(),
            // A literal or triple term can never be a subject; callers never pass one.
            _ => return,
        };
        out.push(Quad {
            subject,
            predicate: p,
            object: o,
            graph_name: GraphName::DefaultGraph,
        });
    };

    triple(
        report_node.clone().into(),
        iri(sh.rdf_type)?,
        iri(sh.validation_report)?.into(),
        &mut out,
    );
    triple(
        report_node.clone().into(),
        iri(sh.conforms)?,
        Literal::new_typed_literal(
            if report.conforms { "true" } else { "false" },
            oxrdf::vocab::xsd::BOOLEAN,
        )
        .into(),
        &mut out,
    );

    // Sorted before numbering: the blank-node labels are a function of the content, so two
    // runs over the same data produce byte-identical reports.
    let mut results = report.results.clone();
    results.sort_by_key(sort_key);

    for (i, result) in results.iter().enumerate() {
        write_result(
            result,
            &Term::from(report_node.clone()),
            sh.result,
            &format!("result{i}"),
            shapes_graph,
            sh,
            &mut out,
        )?;
    }
    Ok(out)
}

/// Writes one result, and any results nested underneath it.
///
/// `link` is the predicate joining `parent` to this result: `sh:result` from the report
/// itself, `sh:detail` from an enclosing result. They are not interchangeable — a result is
/// not a report, so hanging a nested one off `sh:result` would claim the validator produced
/// it directly rather than as an explanation of something else.
///
/// `label` names this result's blank node and is derived from its position, so two runs over
/// the same data still produce byte-identical output.
#[allow(clippy::too_many_arguments)]
fn write_result(
    result: &ValidationResult,
    parent: &Term,
    link: TermId,
    label: &str,
    shapes_graph: GraphView<'_>,
    sh: &Sh,
    out: &mut Vec<Quad>,
) -> Result<(), ShaclError> {
    let iri = |id: TermId| -> Result<NamedNode, ShaclError> {
        match shapes_graph.term(id)? {
            Some(Term::NamedNode(n)) => Ok(n),
            _ => Err(ShaclError::IllFormedShape(format!(
                "{id:?} should be an IRI"
            ))),
        }
    };
    let triple = |s: Term, p: NamedNode, o: Term, out: &mut Vec<Quad>| {
        let subject = match s {
            Term::NamedNode(n) => n.into(),
            Term::BlankNode(b) => b.into(),
            _ => return,
        };
        out.push(Quad {
            subject,
            predicate: p,
            object: o,
            graph_name: GraphName::DefaultGraph,
        });
    };

    let node = BlankNode::new_unchecked(label.to_owned());
    triple(parent.clone(), iri(link)?, node.clone().into(), out);
    triple(
        node.clone().into(),
        iri(sh.rdf_type)?,
        iri(sh.validation_result)?.into(),
        out,
    );
    let Some(focus) = shapes_graph.term(result.focus_node)? else {
        return Ok(());
    };
    triple(node.clone().into(), iri(sh.focus_node)?, focus, out);
    triple(
        node.clone().into(),
        iri(sh.result_severity)?,
        iri(result.severity)?.into(),
        out,
    );
    triple(
        node.clone().into(),
        iri(sh.source_constraint_component)?,
        iri(result.component)?.into(),
        out,
    );
    if let Some(shape) = shapes_graph.term(result.source_shape)? {
        triple(node.clone().into(), iri(sh.source_shape)?, shape, out);
    }
    if let Some(value) = result.value {
        if let Some(term) = shapes_graph.term(value)? {
            triple(node.clone().into(), iri(sh.value)?, term, out);
        }
    }
    if let Some(path) = result.path {
        if let Some(term) = shapes_graph.term(path)? {
            // A compound path is a blank-node structure in the shapes graph. It is copied
            // into the report — naming it without its triples would leave the reader unable
            // to resolve it — and copied *per result*, under labels unique to this result.
            // Sharing one copy between two results makes a graph that is not isomorphic to
            // the one SHACL specifies, because the two results would then share a node
            // rather than each having their own.
            let renamed = copy_path(shapes_graph, path, &term, label, out)?;
            triple(node.clone().into(), iri(sh.result_path)?, renamed, out);
        }
    }
    for message in &result.messages {
        if let Some(term) = shapes_graph.term(*message)? {
            triple(node.clone().into(), iri(sh.result_message)?, term, out);
        }
    }

    let mut details = result.details.clone();
    details.sort_by_key(sort_key);
    for (j, detail) in details.iter().enumerate() {
        write_result(
            detail,
            &Term::from(node.clone()),
            sh.detail,
            &format!("{label}d{j}"),
            shapes_graph,
            sh,
            out,
        )?;
    }
    Ok(())
}

/// A total order over results, so report rendering is deterministic.
fn sort_key(r: &ValidationResult) -> (TermId, Option<TermId>, Option<TermId>, TermId, TermId) {
    (r.focus_node, r.path, r.value, r.source_shape, r.component)
}

/// Copies everything reachable from a blank node into the report, under labels unique to
/// one result.
fn copy_path(
    graph: GraphView<'_>,
    root: TermId,
    root_term: &Term,
    label: &str,
    out: &mut Vec<Quad>,
) -> Result<Term, ShaclError> {
    if holos_core::Tag::BlankNode != root.tag() {
        return Ok(root_term.clone());
    }
    let mut next = 0u64;
    copy_occurrence(graph, root, label, &mut next, 0, out)
}

/// Copies one *occurrence* of a path node, and everything below it.
///
/// Deliberately a tree rather than a graph copy. A shapes graph may reach the same blank
/// node twice — `sh:path ( _:p _:p )` with `_:p sh:inversePath ex:p` is legal and says
/// "inverse p, then inverse p" — and the report is expected to carry the path *expression*,
/// in which those are two occurrences. Copying node-by-node instead of occurrence-by-
/// occurrence writes the shared node once, and the result is a graph the reader cannot read
/// back as the path it came from.
///
/// `depth` is a cycle guard. A well-formed path expression is finite and acyclic, but a
/// shapes graph is data and can say otherwise; a copier that trusted it would not return.
fn copy_occurrence(
    graph: GraphView<'_>,
    node: TermId,
    label: &str,
    next: &mut u64,
    depth: u32,
    out: &mut Vec<Quad>,
) -> Result<Term, ShaclError> {
    const MAX_DEPTH: u32 = 64;
    let Some(term) = graph.term(node)? else {
        return Ok(Term::BlankNode(BlankNode::new_unchecked(format!(
            "p{label}_missing"
        ))));
    };
    if holos_core::Tag::BlankNode != node.tag() || depth >= MAX_DEPTH {
        return Ok(term);
    }

    let here = BlankNode::new_unchecked(format!("p{label}_{}", *next));
    *next += 1;

    for quad in graph
        .store()
        .quads_for_pattern(Some(node), None, None, graph.graph())
    {
        let quad = quad?;
        let decoded = graph.store().decode_quad(quad)?;
        let object = copy_occurrence(graph, quad.object, label, next, depth + 1, out)?;
        out.push(Quad {
            subject: here.clone().into(),
            predicate: decoded.predicate,
            object,
            graph_name: GraphName::DefaultGraph,
        });
    }
    Ok(here.into())
}
