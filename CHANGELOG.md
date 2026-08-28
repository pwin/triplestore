# Changelog

Notable changes per release. Numbers quoted here are measured; the benchmarks that produce
them are in `BENCHMARKS.md` and are runnable.

## Unreleased

### SHACL 1.2 Core is complete

138/138, from the 103/138 that shipped in 0.2.0. The remaining thirty-five were mostly not
new constraints but constraints whose *parameter* SHACL 1.2 widened without changing the
syntax — `sh:class` and `sh:datatype` take lists, `sh:equals`, `sh:disjoint`, `sh:lessThan`
and `sh:lessThanOrEquals` take paths — each failing silently in whichever direction hurt that
constraint most.

The last two are report-level rather than constraint-level:

- `sh:nodeByExpression` names a shape through a node expression. Core defines no node
  expression more interesting than a constant IRI, so what it adds over `sh:node` is the
  report: the expression is recorded as `sh:sourceConstraint`. A non-constant expression is
  left uncompiled rather than approximated.
- `sh:conformanceDisallows` says which severities disqualify. It is a property of the report,
  not of the shapes — it is the rule the data was judged by, which is the difference between
  "this is fine" and "this is fine *by these lights*". `Report::with_conformance_disallows`
  recomputes conformance against an explicit set and records it.

### RDFS entailment runs against the W3C suite, and it found two missing rules

`SPARQL 1.1: 476/477` was resting on **148 skipped tests**, 70 of them skipped as "needs an
entailment regime: L4 is not built" — written before `holos_engine::entailment` existed. Of
those 70, 36 name RDFS among their regimes; the rest need OWL or RIF.

The harness now materialises the RDFS closure for a test that asks for one, into the
*default* graph rather than beside it: under an entailment regime a basic graph pattern is
matched against the entailed graph, so the closure has to be the graph the query reads. A
test naming only OWL or RIF is still skipped, but now says which regime it wanted.

**SPARQL 1.1 goes 476/477 to 512/513** — 36 tests that had never been run, none of which
regressed anything.

Five of the 36 failed at first, and all five were the same gap: `rdfs6` and `rdfs10`, the
reflexivity of `rdfs:subPropertyOf` and `rdfs:subClassOf`. The module had left them out
alongside `rdfs4` on the grounds that they "entail `x rdf:type rdfs:Resource` for every term
in the graph". That is true of rdfs4 and false of the other two: those are bounded by the
number of properties and classes, which is the size of the *schema*. Both are implemented
now, and the note says what each rule actually costs rather than covering three with one
sentence. The reflexive statements are emitted as facts and kept out of the inference maps,
because a self-loop in the hierarchy makes rdfs7 and rdfs9 rewrite every triple to itself.

The differential rig also had to be told to stand down. It attributes a failure to upstream
when HOLOS and a reference evaluator agree over the same data — but under an entailment
regime they do *not* see the same data, so they agree exactly when the closure added nothing.
All five of these gaps were being filed under someone else's name.

### All four SHACL suites are complete

| Suite | Was | Now |
|---|---:|---:|
| SHACL 1.2 Core, native | 103/138 | **138/138** |
| SHACL 1.2 Core, adapted engine | 127/138 | **138/138** |
| SHACL 1.0 Core, native | 92/97 (+1 skipped) | **98/98** |
| SHACL 1.0 Core, adapted engine | 90/98 | **98/98** |

### `sh:resultPath` describes the path that was walked

The last two failures were the `path-strange` pair, where one node is both an `rdf:first` /
`rdf:rest` sequence and an `sh:inversePath`. SHACL's grammar admits one reading per node, so
such a shapes graph is ill-formed and a processor has to choose — and then say which it
chose. Two changes:

- A blank node bearing `rdf:first` is read as a sequence, checked before the keyed forms.
  Being a list is a property of the node's *structure* rather than a keyword written on it,
  so it is the stronger signal, and it is the reading the suite settled on.
- `sh:resultPath` is now **rendered from the compiled path** rather than copied out of the
  shapes graph. Copying carries whatever else the node happened to say, so a report could
  describe a path that was never walked. For every well-formed path the two are identical —
  which is why no other path test moved.

### The adapted engine reaches 138/138 too

HOLOS runs two validators — the native evaluator on the write path, because it revalidates
incrementally, and the adapted SHACL_Engine for constraint coverage — and the suite is run
against both. The adapted engine went 127/138 and 90/98 to **138/138 and 96/98**:

- `sh:pattern` reported the pattern string as `sh:sourceConstraint`. That property names the
  node which *stated* a constraint, and a Core constraint is stated by its shape, which
  `sh:sourceShape` already names. Five tests, one cause.
- Compound paths were copied into reports node-by-node rather than occurrence-by-occurrence,
  so `sh:path ( _:pinv _:pinv )` — legal, and meaning "inverse p, then inverse p" — arrived
  as one node where the expression has two. The same defect as the native one below, found
  independently in the other validator.
- `{| ... |}` annotations on a constraint were not read at all, so `sh:message` and
  `sh:severity` written that way were ignored.
- `sh:reificationRequired` was unimplemented, leaving a statement with no reifier vacuously
  conforming.
- `sh:nodeByExpression` did not report the expression it evaluated.

### Four SHACL 1.0 defects the 1.2 suite exposed

These had been passing the project's own ratchet. A stricter suite over the same code found
them:

- dates were compared as strings, so `"…T12:00:00-05:00"` and `"…T12:00:00"` were ordered when
  XSD says an untimezoned value is *indeterminate* against a timezoned one;
- `sh:lessThan` counted per value rather than per pair;
- every bounded integer type — `xsd:byte`, `xsd:short`, `xsd:unsignedInt` and six others —
  validated as unbounded, so `sh:datatype xsd:byte` accepted any integer;
- compound paths were copied into reports as graphs rather than trees, so a report described
  a different path than the one that failed.

### `sh:targetWhere` reaches the revalidation frontier

`sh:targetWhere` selects focus nodes by evaluating a shape, so a write can pull a node into a
shape's scope without touching anything that shape's own constraints read. The dependency
walk covered constraints only, so the outer shape was never revalidated. Incremental
revalidation is what lets the Boundary gate a commit (`DESIGN.md` §8), and a gap in it is a
violation admitted, not merely a violation reported late.

### Conformance diagnostics are deterministic

The recorded `.failures` baselines quote a sample of the differing quads, chosen and ordered
by hash iteration. Every re-baseline churned, so the ratchet's diff was noise and hid real
movement. Sorted before sampling: two consecutive re-baselines are now byte-identical.

## 0.2.0

The release that made the query engine fast on the shapes it was slowest at, and made the
spatial index worth having.

### An index nested-loop join

`spareval` joins by building a hash table from the left input and scanning the right in full,
so knowing `?s` is bound to two hundred values does not help it. `holos_engine::bindjoin` is
an operator that can use a binding.

| | Before | After |
|---|---:|---:|
| Three-pattern star, 20 rows from 753,199 quads | 43.9 ms | **0.535 ms** |
| The same with a selective `FILTER` | 20.7 ms | **0.286 ms** |
| GeoSPARQL window over 50,000 geometries | 381 ms | **158 µs** |

The fragment is deliberately small — `SELECT` in the default graph over basic graph patterns,
`JOIN`, `UNION`, `VALUES` and `FILTER`, with `DISTINCT`, `LIMIT`, `OFFSET` and a projection.
Anything else is refused and falls back to the evaluator.

Filters are evaluated by `spareval`'s own expression evaluator through this engine's function
registry, not by a second implementation, so `FILTER` semantics are the evaluator's.

### The spatial index actually pays

The R-tree added in 0.1 narrowed fifty thousand geometries to four and changed nothing,
because the join scanned all fifty thousand regardless. With an operator able to use the
narrowing, the same benchmark shows **2,408×** at fifty thousand geometries.

It is also now maintained rather than rebuilt. A write used to cost a full rebuild — 178–271
ms at 200,000 geometries. It now costs **0.1–0.3 ms, independent of store size**, because the
index catches up by reading the term dictionary from a watermark rather than scanning the
store.

### Fixes

- **`geof:distance` only worked between two points.** Every Polygon and LineString returned
  an *unbound* variable rather than a distance — five of the ten geometries in the OGC
  GeoSPARQL example. Replaced with a shortest-distance implementation for any two geometries.
  Point-to-point results are unchanged to the last digit.
- **The spatial index was not built at start-up** unless `--reorder` was given, which is not
  the default. A normally-started server had no index until its first write, and every
  GeoSPARQL query until then did a full scan.
- **`FROM` and `FROM NAMED` were ignored** by the query fast path, which answered over the
  store's default graph instead. Found by adding the SPARQL 1.0 test suite, whose `dataset`
  directory was the only coverage `FROM` had.
- **`QueryOptions::substitutions` was dropped** by the fast path, so a parameter-bound query
  was answered as though nothing had been bound.
- **A cross product could exhaust memory.** `SELECT * WHERE { ?a ?b ?c . ?d ?e ?f }` over
  20,000 triples materialised 13.7 GB with its own timeout unfired. Evaluation now runs under
  a row budget and the deadline's token.

### Maintenance

- **`holos compact --store <DIR> --to <DIR>`** rewrites a store, reclaiming dictionary entries
  left behind by deleted quads. The dictionary is append-only, so nothing else reclaims them —
  including a backup restore, which is a checkpoint and preserves them exactly.
- **`POST /maintenance/purge`** reclaims spatial index entries for geometries no longer
  referenced, guarded by `--purge-role`. No timer: schedule it as you already schedule
  backups.

### RDFS entailment

`holos_engine::entailment` materialises the RDFS closure into a graph of its own, so the
entailed triples are real ones every reader sees — the query path, the topology rewrite,
SHACL, the statistics — without any of them knowing about entailment. Six rules (rdfs2, 3, 5,
7, 9, 11); the axiomatic and reflexive ones are left out because they entail
`x rdf:type rdfs:Resource` for every term and no query is improved by it.

This is what makes the OGC GeoSPARQL example work at the feature level. It attaches
geometries with `my:hasExactGeometry rdfs:subPropertyOf geo:hasGeometry`, and the topology
rewrite looks for `geo:hasGeometry` — so before entailment a feature-level query returned the
geometries rather than the features.

### SHACL

SHACL 1.2 Core rises from 94/138 to 103/138: `sh:minListLength`, `sh:maxListLength`,
`sh:uniqueMembers`, `sh:memberShape` and `sh:singleLine`, plus `sh:detail` so a result about
a structure can explain itself.

### Conformance

The SPARQL 1.0 suite is now a ratchet of its own at 262/263. Graph Store Protocol 13/13 and
SPARQL Protocol 34/34 remain perfect; SPARQL 1.1 476/477, SPARQL 1.2 262/266.

## 0.1.1

Corrects the Documentation URL on the PyPI project page, which pointed at a `main` branch this
repository has never had and returned 404. PyPI metadata is immutable, so a release was the
only way to change it.

## 0.1.0

First public release: RDF 1.2 store with a term dictionary and nine RocksDB column families,
SPARQL 1.1/1.2 query and update, SHACL validation, the holon layer, GeoSPARQL, and access
policy enforced at the index scan.
