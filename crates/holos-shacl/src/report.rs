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
use crate::ir::{Path, PathIdx, Shapes};
use crate::vocab::Sh;
use crate::{Report, ShaclError, ValidationResult};
use holos_core::TermId;
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term};

/// Renders a report as RDF quads in the default graph.
pub fn to_quads(
    report: &Report,
    shapes_graph: GraphView<'_>,
    shapes: &Shapes,
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

    if let Some(disallowed) = &report.conformance_disallows {
        for severity in disallowed {
            if let Some(term) = shapes_graph.term(*severity)? {
                triple(
                    report_node.clone().into(),
                    iri(sh.conformance_disallows)?,
                    term,
                    &mut out,
                );
            }
        }
    }

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
            shapes,
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
    shapes: &Shapes,
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
    if let Some(source) = result.source_constraint {
        if let Some(term) = shapes_graph.term(source)? {
            triple(node.clone().into(), iri(sh.source_constraint)?, term, out);
        }
    }
    if let Some(value) = result.value {
        if let Some(term) = shapes_graph.term(value)? {
            triple(node.clone().into(), iri(sh.value)?, term, out);
        }
    }
    if let Some(path) = result.path {
        // The path is *rendered from what was compiled*, not copied from the shapes graph.
        //
        // Those differ when a path node carries triples that are not part of the path it was
        // read as — `[ rdf:first ex:p ; rdf:rest ( ex:q ) ; sh:inversePath ex:p ]` is both a
        // sequence and an inverse, and the compiler picks one. Copying the node would put
        // the rejected reading in the report too, describing a path that was never walked.
        //
        // Rendered per result, under blank-node labels unique to this one. Sharing a copy
        // between two results makes them share a node, and the graph is then not isomorphic
        // to the one SHACL specifies.
        let compiled = shapes
            .by_node(result.source_shape)
            .and_then(|idx| shapes.shape(idx).path);
        let rendered = match compiled {
            Some(compiled) => Some(render_path(
                shapes,
                compiled,
                shapes_graph,
                sh,
                label,
                &mut 0,
                out,
            )?),
            // No compiled path: `sh:closed` reports the offending predicate, which is an
            // IRI and stands for itself.
            None => shapes_graph.term(path)?,
        };
        if let Some(rendered) = rendered {
            triple(node.clone().into(), iri(sh.result_path)?, rendered, out);
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
            shapes,
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

/// Renders a compiled path into the report as SHACL's RDF form.
///
/// Rendering rather than copying is what makes `sh:resultPath` describe the path that was
/// actually walked. The two coincide for every well-formed path — the render of a compiled
/// sequence is the same list the shapes graph held — and diverge exactly where a path node
/// says more than one thing, which is where copying is wrong.
///
/// `next` numbers this result's blank nodes, so each occurrence gets its own. A path that
/// mentions the same sub-expression twice has two occurrences of it, and writing one shared
/// node would leave the reader unable to recover the expression.
fn render_path(
    shapes: &Shapes,
    path: PathIdx,
    shapes_graph: GraphView<'_>,
    sh: &Sh,
    label: &str,
    next: &mut u64,
    out: &mut Vec<Quad>,
) -> Result<Term, ShaclError> {
    let iri = |id: TermId| -> Result<NamedNode, ShaclError> {
        match shapes_graph.term(id)? {
            Some(Term::NamedNode(n)) => Ok(n),
            _ => Err(ShaclError::IllFormedShape(format!(
                "{id:?} should be an IRI"
            ))),
        }
    };
    let fresh = |next: &mut u64| {
        let b = BlankNode::new_unchecked(format!("p{label}_{}", *next));
        *next += 1;
        b
    };
    // One blank node carrying one predicate: `[ sh:inversePath <inner> ]` and its siblings.
    let wrap = |predicate: TermId,
                inner: PathIdx,
                next: &mut u64,
                out: &mut Vec<Quad>|
     -> Result<Term, ShaclError> {
        let object = render_path(shapes, inner, shapes_graph, sh, label, next, out)?;
        let here = BlankNode::new_unchecked(format!("p{label}_{}", *next));
        *next += 1;
        out.push(Quad {
            subject: here.clone().into(),
            predicate: iri(predicate)?,
            object,
            graph_name: GraphName::DefaultGraph,
        });
        Ok(here.into())
    };

    Ok(match shapes.path(path) {
        Path::Predicate(p) => Term::NamedNode(iri(*p)?),
        Path::Inverse(inner) => wrap(sh.inverse_path, *inner, next, out)?,
        Path::ZeroOrMore(inner) => wrap(sh.zero_or_more_path, *inner, next, out)?,
        Path::OneOrMore(inner) => wrap(sh.one_or_more_path, *inner, next, out)?,
        Path::ZeroOrOne(inner) => wrap(sh.zero_or_one_path, *inner, next, out)?,
        Path::Sequence(parts) => render_list(parts, shapes, shapes_graph, sh, label, next, out)?,
        Path::Alternative(parts) => {
            let object = render_list(parts, shapes, shapes_graph, sh, label, next, out)?;
            let here = fresh(next);
            out.push(Quad {
                subject: here.clone().into(),
                predicate: iri(sh.alternative_path)?,
                object,
                graph_name: GraphName::DefaultGraph,
            });
            here.into()
        }
    })
}

/// Renders a list of paths as an RDF list.
fn render_list(
    parts: &[PathIdx],
    shapes: &Shapes,
    shapes_graph: GraphView<'_>,
    sh: &Sh,
    label: &str,
    next: &mut u64,
    out: &mut Vec<Quad>,
) -> Result<Term, ShaclError> {
    let nil = match shapes_graph.term(sh.rdf_nil)? {
        Some(Term::NamedNode(n)) => n,
        _ => {
            return Err(ShaclError::IllFormedShape(
                "rdf:nil should be an IRI".into(),
            ))
        }
    };
    let first = match shapes_graph.term(sh.rdf_first)? {
        Some(Term::NamedNode(n)) => n,
        _ => {
            return Err(ShaclError::IllFormedShape(
                "rdf:first should be an IRI".into(),
            ))
        }
    };
    let rest = match shapes_graph.term(sh.rdf_rest)? {
        Some(Term::NamedNode(n)) => n,
        _ => {
            return Err(ShaclError::IllFormedShape(
                "rdf:rest should be an IRI".into(),
            ))
        }
    };
    // Built back to front, so each cell knows the tail it points at.
    let mut tail = Term::NamedNode(nil);
    for part in parts.iter().rev() {
        let value = render_path(shapes, *part, shapes_graph, sh, label, next, out)?;
        let cell = BlankNode::new_unchecked(format!("p{label}_{}", *next));
        *next += 1;
        out.push(Quad {
            subject: cell.clone().into(),
            predicate: first.clone(),
            object: value,
            graph_name: GraphName::DefaultGraph,
        });
        out.push(Quad {
            subject: cell.clone().into(),
            predicate: rest.clone(),
            object: tail,
            graph_name: GraphName::DefaultGraph,
        });
        tail = cell.into();
    }
    Ok(tail)
}
