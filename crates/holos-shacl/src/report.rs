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
use rustc_hash::FxHashSet;

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
            let renamed = rename(&term, label);
            triple(node.clone().into(), iri(sh.result_path)?, renamed, out);
            copy_subgraph(shapes_graph, path, label, out)?;
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

/// Relabels a blank node so one result's copy of a path cannot collide with another's.
fn rename(term: &Term, label: &str) -> Term {
    match term {
        Term::BlankNode(b) => BlankNode::new_unchecked(format!("p{label}_{}", b.as_str())).into(),
        other => other.clone(),
    }
}

/// Copies everything reachable from a blank node into the report, under labels unique to
/// one result.
fn copy_subgraph(
    graph: GraphView<'_>,
    root: TermId,
    label: &str,
    out: &mut Vec<Quad>,
) -> Result<(), ShaclError> {
    if holos_core::Tag::BlankNode != root.tag() {
        return Ok(());
    }
    let mut seen: FxHashSet<TermId> = FxHashSet::default();
    let mut frontier = vec![root];
    while let Some(node) = frontier.pop() {
        if !seen.insert(node) {
            continue;
        }
        for quad in graph
            .store()
            .quads_for_pattern(Some(node), None, None, graph.graph())
        {
            let quad = quad?;
            let decoded = graph.store().decode_quad(quad)?;
            let subject = match rename(&Term::from(decoded.subject.clone()), label) {
                Term::NamedNode(n) => n.into(),
                Term::BlankNode(b) => b.into(),
                _ => continue,
            };
            out.push(Quad {
                subject,
                predicate: decoded.predicate,
                object: rename(&decoded.object, label),
                graph_name: GraphName::DefaultGraph,
            });
            if quad.object.tag() == holos_core::Tag::BlankNode {
                frontier.push(quad.object);
            }
        }
    }
    Ok(())
}
