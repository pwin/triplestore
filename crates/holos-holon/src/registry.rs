//! Holons, stored as RDF.
//!
//! §3 makes it a non-goal to break: *"everything, including all holon metadata, is plain
//! RDF 1.2 in system named graphs. The moment holons need a non-RDF representation, the
//! project has failed its own premise."* So a holon's definition — its graphs, its
//! admission policy, its projections, its current version — is triples in
//! [`system_graph`](crate::system_graph), queryable with ordinary SPARQL like anything else.

use crate::model::{holos, system_graph, Admission, Holon, Regime};
use crate::HolonError;
use holos_engine::Engine;
use holos_security::{Modes, Session};
use holos_store::{GraphFilter, Store};
use oxrdf::vocab::{rdf, xsd};
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};

/// Writes a holon's definition into the system graph.
///
/// Registering a holon is an **administrative** act, not a write: it decides which shapes a
/// scene is held to. §14.7 keeps `Modes::ADMIN` separate from `Modes::WRITE` for exactly
/// this reason — otherwise a principal that can add data could redefine the boundary that
/// is supposed to constrain it.
pub fn register(
    engine: &mut Engine,
    holon: &Holon,
    session: &mut Session,
) -> Result<(), HolonError> {
    let graph = GraphName::from(system_graph());
    let mut quads = vec![
        quad(&holon.id, rdf::TYPE.into_owned(), holos("Holon"), &graph),
        quad(&holon.id, holos("scene"), holon.scene.clone(), &graph),
        quad(&holon.id, holos("boundary"), holon.boundary.clone(), &graph),
        quad(&holon.id, holos("events"), holon.events.clone(), &graph),
        quad(&holon.id, holos("admits"), holon.admission.iri(), &graph),
    ];
    for projection in &holon.projections {
        quads.push(quad(
            &holon.id,
            holos("projection"),
            projection.id.clone(),
            &graph,
        ));
        quads.push(Quad {
            subject: projection.id.clone().into(),
            predicate: holos("query"),
            object: Literal::new_simple_literal(projection.query.clone()).into(),
            graph_name: graph.clone(),
        });
        quads.push(Quad {
            subject: projection.id.clone().into(),
            predicate: holos("regime"),
            object: match projection.regime {
                Regime::Maintained => holos("Maintained"),
                Regime::Recomputed => holos("Recomputed"),
            }
            .into(),
            graph_name: graph.clone(),
        });
    }

    for q in &quads {
        let encoded = engine.store_mut().encode_quad(q.as_ref())?;
        if !session
            .policy(engine.store())?
            .permits_quad(encoded, Modes::ADMIN)
        {
            return Err(HolonError::WriteDenied(system_graph()));
        }
        engine.store_mut().insert_encoded(encoded)?;
    }
    Ok(())
}

/// Reads a holon back out of the system graph.
///
/// Returns `None` when nothing at that IRI is a holon.
pub fn load(store: &Store, id: &NamedNode) -> Result<Option<Holon>, HolonError> {
    let Some(graph) = graph_filter(store, &system_graph())? else {
        return Ok(None);
    };
    let Some(subject) = store.lookup_term(id.as_ref().into())? else {
        return Ok(None);
    };
    let Some(holon_class) = store.lookup_term(holos("Holon").as_ref().into())? else {
        return Ok(None);
    };
    let Some(rdf_type) = store.lookup_term(rdf::TYPE.into())? else {
        return Ok(None);
    };
    let declared = store
        .quads_for_pattern(Some(subject), Some(rdf_type), Some(holon_class), graph)
        .next()
        .transpose()?
        .is_some();
    if !declared {
        return Ok(None);
    }

    let mut holon = Holon::new(id.clone());
    if let Some(Term::NamedNode(n)) = object(store, graph, subject, &holos("scene"))? {
        holon.scene = n;
    }
    if let Some(Term::NamedNode(n)) = object(store, graph, subject, &holos("boundary"))? {
        holon.boundary = n;
    }
    if let Some(Term::NamedNode(n)) = object(store, graph, subject, &holos("events"))? {
        holon.events = n;
    }
    if let Some(Term::NamedNode(n)) = object(store, graph, subject, &holos("admits"))? {
        holon.admission = if n == holos("AdmitAndRecord") {
            Admission::AdmitAndRecord
        } else {
            Admission::Reject
        };
    }
    Ok(Some(holon))
}

/// The version this holon is currently at.
pub fn version(engine: &Engine, holon: &Holon) -> Result<u64, HolonError> {
    let store = engine.store();
    let Some(graph) = graph_filter(store, &system_graph())? else {
        return Ok(0);
    };
    let Some(subject) = store.lookup_term(holon.id.as_ref().into())? else {
        return Ok(0);
    };
    Ok(
        match object(store, graph, subject, &holos("version"))? {
            Some(Term::Literal(l)) => l.value().parse().unwrap_or(0),
            _ => 0,
        },
    )
}

/// The version the next tick will produce.
pub fn next_version(engine: &Engine, holon: &Holon) -> Result<u64, HolonError> {
    Ok(version(engine, holon)? + 1)
}

/// Records the holon's new version, replacing the old one.
///
/// The version lives in the system graph rather than the event log because it is *current
/// state*, and the event log is append-only. Storing a mutable counter in an append-only
/// log is how logs stop being append-only.
pub fn set_version(engine: &mut Engine, holon: &Holon, version: u64) -> Result<(), HolonError> {
    let graph = GraphName::from(system_graph());
    let previous = self::version(engine, holon)?;
    if previous != 0 {
        let old = Quad {
            subject: holon.id.clone().into(),
            predicate: holos("version"),
            object: Literal::new_typed_literal(previous.to_string(), xsd::INTEGER).into(),
            graph_name: graph.clone(),
        };
        engine.store_mut().remove(old.as_ref())?;
    }
    let new = Quad {
        subject: holon.id.clone().into(),
        predicate: holos("version"),
        object: Literal::new_typed_literal(version.to_string(), xsd::INTEGER).into(),
        graph_name: graph,
    };
    engine.store_mut().insert(new.as_ref())?;
    Ok(())
}

fn object(
    store: &Store,
    graph: GraphFilter,
    subject: holos_core::TermId,
    predicate: &NamedNode,
) -> Result<Option<Term>, HolonError> {
    let Some(p) = store.lookup_term(predicate.as_ref().into())? else {
        return Ok(None);
    };
    let Some(quad) = store
        .quads_for_pattern(Some(subject), Some(p), None, graph)
        .next()
        .transpose()?
    else {
        return Ok(None);
    };
    Ok(store.decode_term(quad.object)?)
}

fn graph_filter(store: &Store, graph: &NamedNode) -> Result<Option<GraphFilter>, HolonError> {
    Ok(store
        .lookup_term(graph.as_ref().into())?
        .map(GraphFilter::Named))
}

fn quad(
    subject: &NamedNode,
    predicate: NamedNode,
    object: NamedNode,
    graph: &GraphName,
) -> Quad {
    Quad {
        subject: subject.clone().into(),
        predicate,
        object: object.into(),
        graph_name: graph.clone(),
    }
}
