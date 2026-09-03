//! A guided run through a holon, using the files in `examples/holon/`.
//!
//! This is the worked example `HOLONS.md` describes. It exists as a program rather than a
//! transcript pasted into a document for one reason: a transcript goes stale silently, and a
//! program that no longer compiles fails the build. Every number and message the manual
//! quotes comes from running this.
//!
//! ```text
//! cargo run -p holos-holon --example tour
//! ```

use holos_engine::Engine;
use holos_holon::{registry, tick_with_rules, Admission, Delta, Holon, Rules, TickOutcome};
use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use holos_store::{GraphFilter, Store};
use oxrdf::{GraphName, NamedNode, Quad, Triple};
use oxrdfio::{RdfFormat, RdfParser};
use std::error::Error;

const WO: &str = "https://example.org/workorders#";
const DELTAS: &str = "https://example.org/deltas#";

fn wo(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{WO}{name}"))
}

/// The example data, compiled in so the tour runs from any directory.
const BOUNDARY: &str = include_str!("../../../examples/holon/boundary.ttl");
const SCENE: &str = include_str!("../../../examples/holon/scene.ttl");
const DELTAS_TRIG: &str = include_str!("../../../examples/holon/deltas.trig");

// --------------------------------------------------------------------------- printing

fn heading(n: u32, title: &str) {
    println!("\n\x1b[1m{n}. {title}\x1b[0m");
    println!("{}", "-".repeat(70));
}

/// Reports a tick in the terms a holon actually answers in.
fn report(engine: &Engine, label: &str, outcome: &Result<TickOutcome, holos_holon::HolonError>) {
    match outcome {
        Ok(o) if o.committed() => println!(
            "  {label:<28} committed as version {}, {} triples applied",
            o.version, o.applied
        ),
        Ok(o) => {
            println!(
                "  {label:<28} REFUSED at version {}, {} violation(s)",
                o.version, o.violations
            );
            if let Some(report) = &o.report {
                for line in messages(engine, report) {
                    println!("        {line}");
                }
            }
        }
        Err(e) => println!("  {label:<28} FAILED: {e}"),
    }
}

/// The human-readable half of a validation report.
///
/// A report carries term *ids*, not terms — the validator works on the store's own encoding
/// and never decodes anything it does not have to. Turning them back into text is the
/// caller's job, and one lookup per line is what it costs.
fn messages(engine: &Engine, report: &holos_shacl::Report) -> Vec<String> {
    let term = |id| {
        engine
            .store()
            .decode_term(id)
            .ok()
            .flatten()
            .map_or_else(|| "?".to_owned(), |t| short(&t.to_string()))
    };
    report
        .results
        .iter()
        .map(|r| {
            let what = match r.messages.first() {
                Some(m) => term(*m).trim_matches('"').to_owned(),
                // No `sh:message` on the shape, so name the constraint that failed instead.
                None => format!("violates {}", term(r.component)),
            };
            let at = match r.path {
                Some(p) => format!("{} {}", term(r.focus_node), term(p)),
                None => term(r.focus_node),
            };
            format!("{at} — {what}")
        })
        .collect()
}

/// Renders a term the way a person reading a terminal wants to see it.
///
/// Prefixes rather than full IRIs, and no angle brackets — including the ones inside a typed
/// literal's datatype, which is why this cannot be a `trim`.
fn short(text: &str) -> String {
    text.replace(WO, "wo:")
        .replace("http://www.w3.org/2001/XMLSchema#", "xsd:")
        .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
        .replace("http://www.w3.org/2000/01/rdf-schema#", "rdfs:")
        .replace(holos_holon::model::NS, "holos:")
        .replace(['<', '>'], "")
}

// --------------------------------------------------------------------------- reading

/// Every triple in a named graph, rendered and sorted.
fn graph(engine: &Engine, name: &NamedNode) -> Vec<String> {
    let store: &Store = engine.store();
    let Ok(Some(id)) = store.lookup_term(name.as_ref().into()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = store
        .quads_for_pattern(None, None, None, GraphFilter::Named(id))
        .filter_map(|q| q.ok())
        .filter_map(|q| store.decode_quad(q).ok())
        .map(|q| {
            format!(
                "{} {} {}",
                short(&q.subject.to_string()),
                short(&q.predicate.to_string()),
                short(&q.object.to_string())
            )
        })
        .collect();
    out.sort();
    out
}

/// Loads a Turtle document straight into a named graph, bypassing the boundary.
fn seed(engine: &mut Engine, text: &str, into: &NamedNode) -> Result<(), Box<dyn Error>> {
    engine.bulk_load_into_graph(
        text.as_bytes(),
        RdfFormat::Turtle,
        Some(WO),
        &GraphName::NamedNode(into.clone()),
    )?;
    Ok(())
}

/// Reads one commit out of `deltas.trig`.
///
/// A delta is two sets, so it is two graphs: `<name>` is what the commit adds and
/// `<name>-remove`, if present, is what it takes away. RDF has no update-in-place, so
/// changing a value means saying both.
fn delta(name: &str) -> Result<Delta, Box<dyn Error>> {
    let additions = GraphName::NamedNode(NamedNode::new_unchecked(format!("{DELTAS}{name}")));
    let removals = GraphName::NamedNode(NamedNode::new_unchecked(format!("{DELTAS}{name}-remove")));

    let mut out = Delta::default();
    let mut found = false;
    for quad in RdfParser::from_format(RdfFormat::TriG)
        .with_base_iri(WO)?
        .for_reader(DELTAS_TRIG.as_bytes())
    {
        let Quad {
            subject,
            predicate,
            object,
            graph_name,
        } = quad?;
        let triple = Triple {
            subject,
            predicate,
            object,
        };
        if graph_name == additions {
            out = out.add(triple);
            found = true;
        } else if graph_name == removals {
            out = out.remove(triple);
        }
    }
    if !found {
        return Err(format!("no delta named {name} in deltas.trig").into());
    }
    Ok(out)
}

// --------------------------------------------------------------------------- sessions

/// A maintenance planner: may write anything in the scene.
fn planner(store: &Store) -> Result<Session, Box<dyn Error>> {
    Ok(Session::open(
        store,
        Principal::anonymous().with_role("planner"),
        Policy::default().with_rule(Rule::allow(
            Modes::ALL,
            Scope::Everything,
            PrincipalMatch::Role("planner".into()),
        )),
    )?)
}

/// A field engineer: may update an order, but may not sign one off.
///
/// The narrow rule wins over the broad one because it is more specific — a graph-and-
/// predicate scope beats an everything scope, so this is one rule added to a permissive
/// policy rather than a second policy written from scratch.
fn engineer(store: &Store, scene: &NamedNode) -> Result<Session, Box<dyn Error>> {
    Ok(Session::open(
        store,
        Principal::anonymous().with_role("engineer"),
        Policy::default()
            .with_rule(Rule::allow(
                Modes::ALL,
                Scope::Everything,
                PrincipalMatch::Role("engineer".into()),
            ))
            .with_rule(Rule::deny(
                Modes::WRITE,
                Scope::GraphPredicate(scene.clone(), wo("signedOffBy")),
                PrincipalMatch::Role("engineer".into()),
            )),
    )?)
}

// --------------------------------------------------------------------------- the tour

fn main() -> Result<(), Box<dyn Error>> {
    let holon = Holon::new(NamedNode::new_unchecked(format!("{WO}orders")))
        .with_admission(Admission::Reject)
        .with_projection(
            wo("OpenOrders"),
            format!(
                "SELECT ?order ?asset WHERE {{ GRAPH <{}> {{ \
                   ?order <{WO}asset> ?asset ; <{WO}status> ?status . \
                   FILTER(?status != <{WO}Closed>) }} }}",
                holon_scene()
            ),
        );

    let mut engine = Engine::new();

    heading(1, "Set the holon up");
    seed(&mut engine, BOUNDARY, &holon.boundary)?;
    seed(&mut engine, SCENE, &holon.scene)?;
    let mut session = planner(engine.store())?;
    registry::register(&mut engine, &holon, &mut session)?;
    println!("  scene    {}", holon.scene);
    println!("  boundary {}", holon.boundary);
    println!("  events   {}", holon.events);
    println!(
        "  {} triples in the scene, {} in the boundary, version {}",
        graph(&engine, &holon.scene).len(),
        graph(&engine, &holon.boundary).len(),
        registry::version(&engine, &holon)?
    );

    // Rules are prepared once and kept, not rebuilt per tick: that is what makes firing them
    // per commit affordable rather than a full re-bridge of the scene every time.
    let mut rules = Rules::prepare(&mut engine, &holon)?;
    println!(
        "  boundary rules: {}",
        if rules.is_some() {
            "prepared, and will fire on every tick"
        } else {
            "none in this boundary"
        }
    );

    heading(2, "A commit the boundary accepts");
    let outcome = tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &delta("raise-1002")?,
        rules.as_mut(),
    );
    report(&engine, "raise WO-1002", &outcome);

    heading(3, "A commit the boundary refuses");
    println!("  A status the vocabulary does not have. Note that the order is otherwise");
    println!("  complete: the tick is refused whole, not partially applied.\n");
    let before = graph(&engine, &holon.scene).len();
    let outcome = tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &delta("raise-1003-bad")?,
        rules.as_mut(),
    );
    report(&engine, "raise WO-1003", &outcome);
    println!(
        "\n  scene held {before} triples before the tick and {} after",
        graph(&engine, &holon.scene).len()
    );

    heading(4, "A rule firing inside a commit");
    println!("  The delta records a sign-off, a completion date, and moves the status. What");
    println!("  it does not say is that the order is now a wo:ClosedOrder — the boundary's");
    println!("  rule derives that, before the invariants are checked. It is also the one");
    println!("  commit here that removes a triple as well as adding some.\n");
    let outcome = tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &delta("signoff-1001")?,
        rules.as_mut(),
    );
    report(&engine, "sign off WO-1001", &outcome);
    for line in graph(&engine, &holon.scene) {
        if line.starts_with("wo:WO-1001") {
            println!("        {line}");
        }
    }

    heading(5, "A rule causing a refusal");
    println!("  The same sign-off, with no completion date. The rule closes the order; the");
    println!("  boundary then sees a closed order that cannot say when it was completed.");
    println!("  Running rules before validation is what makes this a refusal rather than a");
    println!("  bad row already written.\n");
    let outcome = tick_with_rules(
        &mut engine,
        &holon,
        &mut session,
        &delta("signoff-1002-incomplete")?,
        rules.as_mut(),
    );
    report(&engine, "sign off WO-1002", &outcome);

    heading(6, "Policy, on the write path");
    println!("  A field engineer may update an order but not sign one off. The refusal is");
    println!("  not the boundary's — it is the policy at the index scan, and it happens");
    println!("  before any shape is consulted.\n");
    let mut field = engineer(engine.store(), &holon.scene)?;
    let outcome = tick_with_rules(
        &mut engine,
        &holon,
        &mut field,
        &delta("signoff-1002-incomplete")?,
        rules.as_mut(),
    );
    report(&engine, "engineer signs off", &outcome);

    heading(7, "What the event log says");
    println!("  Every attempt is recorded, committed or not: a boundary that keeps no record");
    println!("  of what it refused cannot be audited.\n");
    let events = graph(&engine, &holon.events);
    println!(
        "  {} triples in the event log. The ticks it records:",
        events.len()
    );
    for line in &events {
        if line.contains("holos:version")
            || line.contains("holos:admitted")
            || line.contains("holos:violations")
        {
            println!("        {line}");
        }
    }

    heading(8, "Reading through a projection");
    println!("  Agents read projections, not the scene. This one lists the orders that are");
    println!("  still open.\n");
    let view = engine.view(&session);
    match holos_holon::projection(&view, &holon, &wo("OpenOrders")) {
        Some(Ok(spareval::QueryResults::Solutions(rows))) => {
            for row in rows {
                let row = row?;
                let order = row.get("order").map(|t| short(&t.to_string()));
                let asset = row.get("asset").map(|t| short(&t.to_string()));
                println!(
                    "        {} on {}",
                    order.unwrap_or_default(),
                    asset.unwrap_or_default()
                );
            }
        }
        Some(Ok(_)) => println!("        (not a SELECT)"),
        Some(Err(e)) => println!("        failed: {e}"),
        None => println!("        no projection by that name"),
    }

    println!();
    Ok(())
}

/// The scene graph's IRI, derived the same way [`Holon::new`] derives it.
fn holon_scene() -> String {
    format!("{WO}orders/scene")
}
