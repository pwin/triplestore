//! The query battery.
//!
//! Each query is here because it isolates one thing. A benchmark that only reports "queries
//! per second" tells you nothing about *which* queries, and for a store whose known weak
//! point is planning rather than storage, that distinction is the entire result.
//!
//! The battery is in four groups:
//!
//! - **Access shapes** — what the indexes cost, with the planner mostly out of the way.
//! - **Joins** — where the planner starts to matter.
//! - **Property paths** — the transitive operators, walked over a real holarchy.
//! - **Holonic** — provenance and history questions, asked of the event log.

use crate::data::EX;

/// One timed query.
pub struct Case {
    /// Short label for the report table.
    pub label: &'static str,
    /// What this query is here to isolate. Printed in the long-form report.
    pub tests: &'static str,
    /// The SPARQL.
    pub sparql: String,
    pub group: Group,
    /// Rows this query must return, where the answer is knowable in advance.
    ///
    /// A benchmark that measures a query returning the wrong answers is measuring nothing.
    /// The holarchy has a fixed shape, so every path query over it has an arithmetic
    /// answer — and checking it costs nothing, because the rows are being counted anyway.
    /// `None` where the count depends on the scale.
    pub expect_rows: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Access,
    Join,
    Path,
    Holon,
}

impl Group {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Join => "join",
            Self::Path => "property path",
            Self::Holon => "holonic",
        }
    }
}

fn prefixes() -> String {
    format!(
        "PREFIX ex: <{EX}> \
         PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
         PREFIX holos: <https://holos.rdf/ns#> \
         PREFIX prov: <http://www.w3.org/ns/prov#> "
    )
}

/// Queries over the bulk dataset.
///
/// `anchor` is a subject that exists at every scale, so the same query can be run against
/// 100k and 10M rows and mean the same thing.
#[must_use]
pub fn battery(anchor: usize) -> Vec<Case> {
    let p = prefixes();
    let subject = format!("{EX}person{anchor}");

    vec![
        // ---------------------------------------------------------- access
        Case {
            label: "point lookup",
            tests: "one subject, one predicate: the spo index and nothing else",
            sparql: format!("{p} SELECT ?n WHERE {{ <{subject}> ex:name ?n }}"),
            group: Group::Access,
            expect_rows: Some(1),
        },
        Case {
            label: "subject fan-out",
            tests: "every triple about one subject — a star with no join",
            sparql: format!("{p} SELECT ?pr ?o WHERE {{ <{subject}> ?pr ?o }}"),
            group: Group::Access,
            expect_rows: None,
        },
        Case {
            label: "rare predicate scan",
            tests: "a predicate on 1 subject in 500: the pos index, and the pattern a \
                    good planner should start from",
            sparql: format!("{p} SELECT ?s ?b WHERE {{ ?s ex:badgeNumber ?b }}"),
            group: Group::Access,
            expect_rows: None,
        },
        Case {
            label: "common predicate count",
            tests: "an aggregate over every person: a full predicate scan",
            sparql: format!("{p} SELECT (COUNT(*) AS ?n) WHERE {{ ?s ex:name ?o }}"),
            group: Group::Access,
            expect_rows: Some(1),
        },
        Case {
            label: "object lookup",
            tests: "bound object, unbound subject: the osp index",
            sparql: format!("{p} SELECT ?s WHERE {{ ?s ex:memberOf <{EX}unit300> }}"),
            group: Group::Access,
            expect_rows: None,
        },
        // ---------------------------------------------------------- joins
        Case {
            label: "3-way star",
            tests: "three predicates on one subject variable — the shape RDF queries \
                    mostly are, and what characteristic sets estimate exactly",
            sparql: format!(
                "{p} SELECT ?n ?a ?u WHERE {{ ?s ex:name ?n ; ex:age ?a ; ex:memberOf ?u }} LIMIT 100"
            ),
            group: Group::Join,
            expect_rows: None,
        },
        Case {
            label: "selective join, written well",
            tests: "the rare predicate first, so any plan is a good plan",
            sparql: format!(
                "{p} SELECT ?n ?u WHERE {{ ?s ex:badgeNumber ?b . ?s ex:name ?n . ?s ex:memberOf ?u }} LIMIT 20"
            ),
            group: Group::Join,
            expect_rows: Some(20),
        },
        Case {
            label: "selective join, written badly",
            tests: "the same answers with the rare predicate last. The gap between this \
                    row and the one above is the cost of having no cost-based planner",
            sparql: format!(
                "{p} SELECT ?n ?u WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?u . ?s ex:badgeNumber ?b }} LIMIT 20"
            ),
            group: Group::Join,
            expect_rows: Some(20),
        },
        Case {
            label: "2-hop, anchored",
            tests: "friends-of-a-friend from one known person: bounded work",
            sparql: format!(
                "{p} SELECT ?fn WHERE {{ <{subject}> ex:knows ?f . ?f ex:knows ?ff . ?ff ex:name ?fn }} LIMIT 50"
            ),
            group: Group::Join,
            expect_rows: None,
        },
        Case {
            label: "2-hop, unanchored",
            tests: "the same shape with no starting point — this is the one that hurts",
            sparql: format!(
                "{p} SELECT ?an ?bn WHERE {{ ?a ex:knows ?b . ?a ex:name ?an . ?b ex:name ?bn }} LIMIT 20"
            ),
            group: Group::Join,
            expect_rows: Some(20),
        },
        Case {
            label: "OPTIONAL",
            tests: "left join against a sparse predicate",
            sparql: format!(
                "{p} SELECT ?n ?b WHERE {{ ?s ex:name ?n OPTIONAL {{ ?s ex:badgeNumber ?b }} }} LIMIT 100"
            ),
            group: Group::Join,
            expect_rows: Some(100),
        },
        Case {
            label: "FILTER NOT EXISTS",
            tests: "negation — and the operator most likely to leak, which is why §14 \
                    enforces policy below it rather than beside it",
            sparql: format!(
                "{p} SELECT ?n WHERE {{ ?s ex:name ?n FILTER NOT EXISTS {{ ?s ex:badgeNumber ?b }} }} LIMIT 20"
            ),
            group: Group::Join,
            expect_rows: Some(20),
        },
        // ---------------------------------------------------- property paths
        Case {
            label: "path: one-or-more up",
            tests: "ex:partOf+ — every ancestor of a leaf unit. The holarchy walk: a \
                    holon is a part, and this asks which wholes it belongs to",
            sparql: format!("{p} SELECT ?ancestor WHERE {{ <{EX}unit340> ex:partOf+ ?ancestor }}"),
            group: Group::Path,
            expect_rows: Some(4),
        },
        Case {
            label: "path: zero-or-more up",
            tests: "ex:partOf* — the same, including the unit itself. The difference \
                    between * and + is whether a whole counts as its own part",
            sparql: format!("{p} SELECT ?ancestor WHERE {{ <{EX}unit340> ex:partOf* ?ancestor }}"),
            group: Group::Path,
            expect_rows: Some(5),
        },
        Case {
            label: "path: inverse closure down",
            tests: "^ex:partOf+ — every unit beneath the root. Inverse plus transitive, \
                    which is the expensive direction: it fans out instead of climbing",
            sparql: format!("{p} SELECT ?descendant WHERE {{ <{EX}unit0> ^ex:partOf+ ?descendant }}"),
            group: Group::Path,
            expect_rows: Some(340),
        },
        Case {
            label: "path: sequence + closure",
            tests: "ex:memberOf/ex:partOf* — from a person to every unit they are \
                    transitively in. The query a holonic membership question actually is",
            sparql: format!(
                "{p} SELECT ?u WHERE {{ <{subject}> ex:memberOf/ex:partOf* ?u }}"
            ),
            group: Group::Path,
            expect_rows: Some(5),
        },
        Case {
            label: "path: alternation",
            tests: "ex:knows|ex:memberOf — a union of two very differently sized \
                    predicates, so the cost is dominated by the larger",
            sparql: format!("{p} SELECT ?o WHERE {{ <{subject}> ex:knows|ex:memberOf ?o }}"),
            group: Group::Path,
            expect_rows: None,
        },
        Case {
            label: "path: bounded social closure",
            tests: "ex:knows+ from one person. Unbounded transitive closure over a \
                    heavy-tailed graph — the worst case in this battery, and LIMIT is \
                    the only thing keeping it finite",
            sparql: format!("{p} SELECT ?reached WHERE {{ <{subject}> ex:knows+ ?reached }} LIMIT 500"),
            group: Group::Path,
            expect_rows: Some(500),
        },
        Case {
            label: "path: negated set",
            tests: "!(ex:knows) — everything about a subject except one predicate",
            sparql: format!("{p} SELECT ?o WHERE {{ <{subject}> !(ex:knows) ?o }}"),
            group: Group::Path,
            expect_rows: None,
        },
        Case {
            label: "path: count descendants",
            tests: "an aggregate over a transitive path — the shape a rollup report takes",
            sparql: format!(
                "{p} SELECT (COUNT(DISTINCT ?d) AS ?n) WHERE {{ ?d ex:partOf+ <{EX}unit0> }}"
            ),
            group: Group::Path,
            expect_rows: Some(1),
        },
    ]
}

/// Queries over a holon's scene, boundary and event log.
///
/// These are the ones that have no equivalent in an ordinary triplestore. The event log is
/// RDF 1.2: a change is a blank node that `rdf:reifies` a **triple term**, so "who changed
/// this, and when" is an ordinary SPARQL query rather than a side table.
///
/// `tracked` must name a subject that was committed **through a tick**, and
/// `tracked_version` the tick that committed it. Pointing them at a subject that was seeded
/// straight into the scene returns no rows and makes the timing meaningless — which is
/// exactly what the first version of this benchmark did.
#[must_use]
pub fn holon_battery(
    holon_iri: &str,
    scene: &str,
    events: &str,
    tracked: &str,
    tracked_name: &str,
    tracked_version: u64,
) -> Vec<Case> {
    let p = prefixes();
    vec![
        Case {
            label: "holon: registry lookup",
            tests: "find a holon's three graphs from its IRI",
            sparql: format!(
                "{p} SELECT ?scene ?boundary ?events WHERE {{ GRAPH <urn:holos:system> {{ \
                 <{holon_iri}> holos:scene ?scene ; holos:boundary ?boundary ; holos:events ?events }} }}"
            ),
            group: Group::Holon,
            expect_rows: Some(1),
        },
        Case {
            label: "holon: scene size",
            tests: "how big is the graph this holon is responsible for",
            sparql: format!(
                "{p} SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{scene}> {{ ?s ?pr ?o }} }}"
            ),
            group: Group::Holon,
            expect_rows: Some(1),
        },
        Case {
            label: "holon: tick history",
            tests: "every commit, newest first, with who made it — PROV over the event log",
            sparql: format!(
                "{p} SELECT ?v ?who ?at WHERE {{ GRAPH <{events}> {{ \
                 ?t a holos:Tick ; holos:version ?v ; prov:wasAssociatedWith ?who ; \
                 prov:startedAtTime ?at }} }} ORDER BY DESC(?v) LIMIT 20"
            ),
            group: Group::Holon,
            expect_rows: Some(20),
        },
        Case {
            label: "holon: what changed in a tick",
            tests: "the triple terms a tick added or removed. `rdf:reifies` pointing at a \
                    triple term is RDF 1.2 doing in two triples what RDF 1.1 needed four \
                    for — and gave no defined meaning to",
            sparql: format!(
                "{p} SELECT ?op ?stmt WHERE {{ GRAPH <{events}> {{ \
                 ?t a holos:Tick ; holos:version {tracked_version} . \
                 ?c holos:inTick ?t ; holos:operation ?op ; rdf:reifies ?stmt }} }}"
            ),
            group: Group::Holon,
            expect_rows: Some(3),
        },
        Case {
            label: "holon: provenance of one statement",
            tests: "which tick asserted this exact triple, and who was responsible. The \
                    question a per-statement audit asks, answered without a side table",
            sparql: format!(
                "{p} SELECT ?v ?who WHERE {{ GRAPH <{events}> {{ \
                 ?c rdf:reifies <<( <{tracked}> ex:name \"{tracked_name}\" )>> ; \
                    holos:inTick ?t ; holos:operation holos:Added . \
                 ?t holos:version ?v ; prov:wasAssociatedWith ?who }} }}"
            ),
            group: Group::Holon,
            expect_rows: Some(1),
        },
        Case {
            label: "holon: rejected commits",
            tests: "ticks the boundary refused, and how many violations each had. A \
                    boundary that rejects is only useful if the refusal is queryable",
            sparql: format!(
                "{p} SELECT ?v ?violations WHERE {{ GRAPH <{events}> {{ \
                 ?t a holos:Tick ; holos:version ?v ; holos:admitted false . \
                 OPTIONAL {{ ?t holos:violations ?violations }} }} }} LIMIT 20"
            ),
            group: Group::Holon,
            expect_rows: Some(1),
        },
        Case {
            label: "holon: change volume per tick",
            tests: "an aggregate over the log — how much each commit actually moved",
            sparql: format!(
                "{p} SELECT ?v (COUNT(?c) AS ?changes) WHERE {{ GRAPH <{events}> {{ \
                 ?c holos:inTick ?t . ?t holos:version ?v }} }} \
                 GROUP BY ?v ORDER BY DESC(?changes) LIMIT 10"
            ),
            group: Group::Holon,
            expect_rows: Some(10),
        },
        Case {
            label: "holon: scene joined to log",
            tests: "current state joined to its own history, across two named graphs — \
                    the query that would need two databases without this model",
            sparql: format!(
                "{p} SELECT ?n ?v WHERE {{ \
                 GRAPH <{scene}> {{ ?s ex:name ?n }} \
                 GRAPH <{events}> {{ ?c rdf:reifies <<( ?s ex:name ?n )>> ; holos:inTick ?t . \
                                     ?t holos:version ?v }} }} LIMIT 20"
            ),
            group: Group::Holon,
            expect_rows: None,
        },
    ]
}
