//! How wrong are the estimates, and does having a store to consult fix it?
//!
//! `DESIGN.md` §13 Q2 asks whether the hypertrie is worth building *given a good cost-based
//! optimiser*, and says P2 answers it empirically. That question cannot be approached
//! without first knowing how good estimates can get, because a planner is only as good as
//! what it estimates with.
//!
//! So: a set of query shapes, each run to get its **true** cardinality, then estimated two
//! ways — by the constant table the reused optimiser uses, and by characteristic sets. The
//! error is reported as *q-error*, the standard measure: `max(estimate/actual,
//! actual/estimate)`, so being 100× over and 100× under both score 100. A perfect estimate
//! is 1.
//!
//! ```text
//! cargo run --release -p holos-stats --example estimator_accuracy
//! ```

use holos_engine::Engine;
use holos_security::Session;
use holos_stats::{baseline, Pattern, Statistics};
use holos_store::GraphFilter;
use oxrdf::vocab::rdf;
use oxrdf::{GraphName, Literal, NamedNode, Quad};
use spareval::QueryResults;

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{name}"))
}

/// A dataset with deliberately skewed predicate frequencies and correlated shapes — which
/// is what real RDF looks like, and what a constant table cannot see.
fn seed(engine: &mut Engine, people: usize, orgs: usize) {
    let mut add = |s: NamedNode, p: NamedNode, o: oxrdf::Term| {
        engine
            .store_mut()
            .insert(
                Quad {
                    subject: s.into(),
                    predicate: p,
                    object: o,
                    graph_name: GraphName::DefaultGraph,
                }
                .as_ref(),
            )
            .expect("insert");
    };
    for i in 0..people {
        let s = ex(&format!("person{i}"));
        add(s.clone(), rdf::TYPE.into_owned(), ex("Person").into());
        add(
            s.clone(),
            ex("name"),
            Literal::new_simple_literal(format!("P{i}")).into(),
        );
        add(
            s.clone(),
            ex("email"),
            Literal::new_simple_literal(format!("p{i}@x")).into(),
        );
        // A third of people have a nickname: a genuinely rarer predicate.
        if i % 3 == 0 {
            add(
                s.clone(),
                ex("nickname"),
                Literal::new_simple_literal(format!("nick{i}")).into(),
            );
        }
        // Everyone works for an org: a join out of the star.
        add(
            s,
            ex("worksFor"),
            ex(&format!("org{}", i % orgs.max(1))).into(),
        );
    }
    for i in 0..orgs {
        let s = ex(&format!("org{i}"));
        add(s.clone(), rdf::TYPE.into_owned(), ex("Org").into());
        add(
            s.clone(),
            ex("legalName"),
            Literal::new_simple_literal(format!("O{i}")).into(),
        );
        add(s, ex("country"), Literal::new_simple_literal("GB").into());
    }
}

struct Case {
    label: &'static str,
    sparql: String,
    patterns: Vec<Pattern>,
}

fn q_error(estimate: f64, actual: f64) -> f64 {
    let estimate = estimate.max(1.0);
    let actual = actual.max(1.0);
    (estimate / actual).max(actual / estimate)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let people: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);
    let orgs = people / 200;

    let mut engine = Engine::new();
    seed(&mut engine, people, orgs);
    let stats = Statistics::build(engine.store(), GraphFilter::Default)?;

    let id = |n: &NamedNode| {
        engine
            .store()
            .lookup_term(n.as_ref().into())
            .expect("lookup")
            .expect("term")
    };
    let (name, email, nickname, works, legal, country, rdf_type) = (
        id(&ex("name")),
        id(&ex("email")),
        id(&ex("nickname")),
        id(&ex("worksFor")),
        id(&ex("legalName")),
        id(&ex("country")),
        engine.store().lookup_term(rdf::TYPE.into())?.expect("type"),
    );

    let prefix = "PREFIX ex: <http://example.com/> PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ";
    let cases = vec![
        Case {
            label: "one common predicate",
            sparql: format!("{prefix} SELECT * WHERE {{ ?s ex:name ?n }}"),
            patterns: vec![Pattern::single(None, Some(name), None)],
        },
        Case {
            label: "one rare predicate",
            sparql: format!("{prefix} SELECT * WHERE {{ ?s ex:legalName ?n }}"),
            patterns: vec![Pattern::single(None, Some(legal), None)],
        },
        Case {
            label: "two-predicate star",
            sparql: format!("{prefix} SELECT * WHERE {{ ?s ex:name ?n ; ex:email ?e }}"),
            patterns: vec![
                Pattern::star(0, name, None),
                Pattern::star(0, email, None),
            ],
        },
        Case {
            label: "star with a rarer arm",
            sparql: format!("{prefix} SELECT * WHERE {{ ?s ex:name ?n ; ex:nickname ?k }}"),
            patterns: vec![
                Pattern::star(0, name, None),
                Pattern::star(0, nickname, None),
            ],
        },
        Case {
            label: "four-predicate star",
            sparql: format!(
                "{prefix} SELECT * WHERE {{ ?s rdf:type ex:Person ; ex:name ?n ; ex:email ?e ; ex:worksFor ?o }}"
            ),
            patterns: vec![
                Pattern::star(0, rdf_type, Some(id(&ex("Person")))),
                Pattern::star(0, name, None),
                Pattern::star(0, email, None),
                Pattern::star(0, works, None),
            ],
        },
        Case {
            label: "star that never co-occurs",
            sparql: format!("{prefix} SELECT * WHERE {{ ?s ex:email ?e ; ex:legalName ?l }}"),
            patterns: vec![
                Pattern::star(0, email, None),
                Pattern::star(0, legal, None),
            ],
        },
        Case {
            label: "org star",
            sparql: format!("{prefix} SELECT * WHERE {{ ?o ex:legalName ?l ; ex:country ?c }}"),
            patterns: vec![
                Pattern::star(0, legal, None),
                Pattern::star(0, country, None),
            ],
        },
    ];

    println!(
        "{people} people, {orgs} orgs — {} triples, {} subjects, {} distinct shapes\n",
        stats.total_triples(),
        stats.total_subjects(),
        stats.shape_count()
    );
    println!(
        "{:<28} {:>10} {:>14} {:>9} {:>14} {:>9}",
        "query shape", "actual", "constants", "q-err", "char. sets", "q-err"
    );
    println!("{}", "-".repeat(90));

    let session = Session::unrestricted(engine.store())?;
    let mut worst_baseline = 1.0_f64;
    let mut worst_ours = 1.0_f64;
    let mut sum_baseline = 0.0;
    let mut sum_ours = 0.0;

    for case in &cases {
        let view = engine.view(&session);
        let results = Engine::query(&view, &case.sparql, None)?;
        let actual = match results {
            QueryResults::Solutions(iter) => iter.count() as f64,
            _ => 0.0,
        };

        let base = baseline::estimate_bgp(&case.patterns);
        let ours = stats.estimate_bgp(&case.patterns);
        let (qb, qo) = (q_error(base, actual), q_error(ours, actual));
        worst_baseline = worst_baseline.max(qb);
        worst_ours = worst_ours.max(qo);
        sum_baseline += qb;
        sum_ours += qo;

        println!(
            "{:<28} {:>10.0} {:>14.0} {:>9.1} {:>14.0} {:>9.1}",
            case.label, actual, base, qb, ours, qo
        );
    }

    let n = cases.len() as f64;
    println!("{}", "-".repeat(90));
    println!(
        "{:<28} {:>10} {:>14} {:>9.1} {:>14} {:>9.1}",
        "mean q-error",
        "",
        "",
        sum_baseline / n,
        "",
        sum_ours / n
    );
    println!(
        "{:<28} {:>10} {:>14} {:>9.1} {:>14} {:>9.1}",
        "worst q-error", "", "", worst_baseline, "", worst_ours
    );
    println!(
        "\ncharacteristic sets are {:.0}x better on the mean, {:.0}x on the worst case",
        (sum_baseline / n) / (sum_ours / n).max(1.0),
        worst_baseline / worst_ours.max(1.0)
    );
    Ok(())
}
