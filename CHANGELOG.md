# Changelog

Notable changes per release. Numbers quoted here are measured; the benchmarks that produce
them are in `BENCHMARKS.md` and are runnable.

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
