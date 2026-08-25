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
        let node = BlankNode::new_unchecked(format!("result{i}"));
        triple(
            report_node.clone().into(),
            iri(sh.result)?,
            node.clone().into(),
            &mut out,
        );
        triple(
            node.clone().into(),
            iri(sh.rdf_type)?,
            iri(sh.validation_result)?.into(),
            &mut out,
        );
        let Some(focus) = shapes_graph.term(result.focus_node)? else {
            continue;
        };
        triple(node.clone().into(), iri(sh.focus_node)?, focus, &mut out);
        triple(
            node.clone().into(),
            iri(sh.result_severity)?,
            iri(result.severity)?.into(),
            &mut out,
        );
        triple(
            node.clone().into(),
            iri(sh.source_constraint_component)?,
            iri(result.component)?.into(),
            &mut out,
        );
        if let Some(shape) = shapes_graph.term(result.source_shape)? {
            triple(node.clone().into(), iri(sh.source_shape)?, shape, &mut out);
        }
        if let Some(value) = result.value {
            if let Some(term) = shapes_graph.term(value)? {
                triple(node.clone().into(), iri(sh.value)?, term, &mut out);
            }
        }
        if let Some(path) = result.path {
            if let Some(term) = shapes_graph.term(path)? {
                // A compound path is a blank-node structure in the shapes graph. It is
                // copied into the report — naming it without its triples would leave the
                // reader unable to resolve it — and copied *per result*, under labels
                // unique to this result. Sharing one copy between two results makes a
                // graph that is not isomorphic to the one SHACL specifies, because the
                // two results would then share a node rather than each having their own.
                let renamed = rename(&term, i);
                triple(
                    node.clone().into(),
                    iri(sh.result_path)?,
                    renamed.clone(),
                    &mut out,
                );
                copy_subgraph(shapes_graph, path, i, &mut out)?;
            }
        }
        for message in &result.messages {
            if let Some(term) = shapes_graph.term(*message)? {
                triple(node.clone().into(), iri(sh.result_message)?, term, &mut out);
            }
        }
    }
    Ok(out)
}

/// A total order over results, so report rendering is deterministic.
fn sort_key(r: &ValidationResult) -> (TermId, Option<TermId>, Option<TermId>, TermId, TermId) {
    (r.focus_node, r.path, r.value, r.source_shape, r.component)
}

/// Relabels a blank node so one result's copy of a path cannot collide with another's.
fn rename(term: &Term, result: usize) -> Term {
    match term {
        Term::BlankNode(b) => BlankNode::new_unchecked(format!("p{result}_{}", b.as_str())).into(),
        other => other.clone(),
    }
}

/// Copies everything reachable from a blank node into the report, under labels unique to
/// one result.
fn copy_subgraph(
    graph: GraphView<'_>,
    root: TermId,
    result: usize,
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
            let subject = match rename(&Term::from(decoded.subject.clone()), result) {
                Term::NamedNode(n) => n.into(),
                Term::BlankNode(b) => b.into(),
                _ => continue,
            };
            out.push(Quad {
                subject,
                predicate: decoded.predicate,
                object: rename(&decoded.object, result),
                graph_name: GraphName::DefaultGraph,
            });
            if quad.object.tag() == holos_core::Tag::BlankNode {
                frontier.push(quad.object);
            }
        }
    }
    Ok(())
}
