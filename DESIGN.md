# HOLOS — design study for a new RDF 1.2 triplestore & SPARQL 1.2 engine

*Working name. Date: 2026-08-22.*

**Status: the thesis is demonstrable end to end.** P0 met, P1 partial; RocksDB Tier A, SHACL, GeoSPARQL, the HTTP server and the holon layer built. A Rust workspace in
[`crates/`](crates/) builds a working store — tagged term ids, the nine-order index, SPARQL 1.2
over reused Oxigraph crates, and fine-grained access policy enforced at the scan (§14).

**Conformance: 3,145 of 3,284 W3C tests pass, and every one of the 139 failures is upstream.**
Not one is a HOLOS bug. The suites and how that attribution is made are in §15. 90 further
unit and property tests pass.

Storage now has two backends behind one trait — in memory, and RocksDB with the nine
column families of §6.1. They are held to strict parity, including identical term ids for an
identical insertion sequence, and the RDF suites run through both.

L4 validates against the store's own indexes, and **incremental revalidation is 161×
faster than a full pass** on a 400k-triple graph — the mechanism §8 needs for the holon
Boundary to gate a commit. SHACL_Engine itself is now adapted (§8), which is
what §8 planned from the start.

L6 is up: **SPARQL 1.2 over HTTP with a YASGUI console**, and access policy applies to it
with no extra code because every request opens a `Session` (§10). **GeoSPARQL** runs through
the ordinary query path (§17).

**L5 exists as a walking skeleton** (§9): a holon's scene is a named graph, its boundary is
shapes enforced *on the write path*, its event log carries per-triple provenance through
RDF 1.2 reifiers, and a commit costs **41× less than a full validation** — 165 commits/s
against a 300k-triple scene. That is the four-way combination of §1 doing something no other
system does, and it is the point the whole design was aimed at.

What is *not* built: the cost-based optimiser (§7), the hypertrie tier (§6.2), the holon
layer (§9), SHACL's SPARQL-based constraints and AF rules, and — within §6.1 —
`SstFileWriter` ingestion, MVCC timestamps, merge-operator refcounts, checkpoints and
BlobDB. See §11 for where the line falls, and §16 for what the code actually measures.

---

## 1. Verdict

**Yes — and the reason it's worth doing is not "a faster triplestore".** There are already fast
triplestores (QLever, Virtuoso, Tentris, RDFox). Rebuilding one of those is a losing race.

What does *not* exist as a single system, as of August 2026, is the combination of:

1. **Worst-case-optimal join execution** (Tentris' contribution) on top of
2. **a durable, transactional, RDF-1.2-native store** (Oxigraph/TDB2's contribution), with
3. **SHACL as a write-path subsystem rather than a bolt-on library** (the SHACL_Engine
   contribution, generalised), and
4. **versioned, shape-governed named-graph partitions with incrementally maintained views**
   — which is exactly what the Holon model asks for, expressed in database primitives.

Each of the four exists in isolation. Their combination is the thesis. Everything below is
subordinate to it: if a design decision doesn't serve one of those four, take the boring option
and reuse someone else's code.

The honest counter-position is in §12.

---

## 2. What each input actually contributes

| Source | Take this | Leave this |
|---|---|---|
| **Oxigraph** | Crate decomposition (`oxrdf`, `oxttl`, `oxrdfio`, `oxjsonld`, `spargebra`, `sparesults`, `oxsdatatypes`) — conformance-heavy, boring, already correct, MIT/Apache-2.0. The 6+3 column-family index layout. Preliminary RDF 1.2 / SPARQL 1.2 support already in tree. | The evaluator. The project says plainly that "SPARQL query evaluation has not been optimized yet": tuple-at-a-time iterators, pairwise index-nested-loop joins, 128-bit `StrHash` term keys, no cost model. |
| **pwin/SHACL_Engine** | Interned terms as integers; three fully-sorted flat arrays (SPO/POS/OSP) with binary search instead of hash maps; **compile shapes once into flat IR**; deterministic, byte-reproducible reports; 418/426 W3C conformance. | Its structural limits, all of which are artefacts of being a *library* rather than part of a store: whole graph must be resident, loading dominates runtime (validation is under 25% of wall clock at 100k instances), named graphs flattened into one, single-pass rules needing `--iterate-rules N`. A store fixes all four for free. |
| **Apache Jena 5.5** | The RDF 1.2 model decisions: `StatementTerm` as a distinct `RDFNode`; **triple terms permitted in object position only** (narrower than the old RDF-star CG); reifiers as ordinary terms via `Model.createReifier`. Also: the Graph/DatasetGraph/Model layering, ARQ's extension points, TDB2's MVCC (single writer, many readers). | The JVM, and the object-per-term memory model. |
| **Tentris** | RDF as a sparse order-3/4 boolean tensor; the **hypertrie**, which gives constant-time slices on *any* dimension combination and therefore subsumes all six permutation indexes in one structure; BGP → Einstein summation → worst-case-optimal multi-join; hash-consing of identical subtries (2022); **incremental insert/delete** (ISWC 2025) which removes the historical "bulk-load only" objection. | The research prototype's scope: it answers `SELECT` / `SELECT DISTINCT` / `ASK` over well-designed BGP + `OPTIONAL` patterns only. Full SPARQL lives in the commercial fork. Papers are freely implementable; **verify the licence of any code you actually adapt**. |
| **Holon CG / "What is a Holon"** | The four-layer decomposition — **Scene** (mutable current state), **Boundary** (SHACL shapes + rules = the legal transitions), **Event** (append-only provenance/causality), **Projection** (the externally visible view). And the framing of a graph as a *state machine that ticks*, where agents read projections and never touch the scene. | The Game-of-Life metaphor as an implementation guide, and any commitment to a vocabulary that is still a CG draft. Build the mechanism; keep the vocabulary swappable. |
| **RocksDB** | LSM substrate: column families, prefix bloom filters, `SstFileWriter` + `IngestExternalFile` for bulk load, merge operators for counters, checkpoints for cheap consistent snapshots and branches, BlobDB value separation, and user-defined timestamps for MVCC + time travel. | The assumption that RocksDB should hold the *hot join structure*. An LSM is excellent as a system of record and poor at the pointer-chasing random access a WCO trie join needs. See §6.2. |

---

## 3. Non-goals

Naming them now prevents scope collapse later.

- **Not distributed.** Single-node, embeddable, with an HTTP server on top. Sharding an RDF graph
  well is a research programme of its own; the holon partitioning in §9 is the eventual seam if
  you ever want it, but v1 is one node.
- **Not a new data model.** Everything, including all holon metadata, is plain RDF 1.2 in system
  named graphs. The moment holons need a non-RDF representation, the project has failed its own
  premise.
- **Not OWL-DL reasoning.** RDFS and OWL-RL via the rule engine, materialised. No tableau reasoner.
- **Not a property-graph database.** RDF 1.2 triple terms give you an edge-property model that is
  good enough; a native LPG view can be a later projection type.

---

## 4. Architecture

```
+--------------------------------------------------------------------------+
| L6  Interfaces                                                           |
|     embedded Rust API | SPARQL 1.2 Protocol + GSP server | PyO3 | WASM    |
+--------------------------------------------------------------------------+
| L5  Holon layer                                                          |
|     holon registry - tick transaction - event log - incremental          |
|     projection maintenance (Z-sets) - time travel - branch/merge         |
+--------------------------------------------------------------------------+
| L4  Validation & inference                                               |
|     compiled SHACL IR - incremental revalidation - semi-naive rules      |
|     to fixpoint - DRed on delete - RDFS/OWL-RL profiles                  |
+--------------------------------------------------------------------------+
| L3  Query engine                                                         |
|     spargebra parse -> cost-based optimizer (char. sets + HLL sketches)  |
|     -> hybrid physical plan (vectorized binary joins x WCO multi-join)   |
|     -> morsel-driven parallel execution over TermId batches              |
+--------------------------------------------------------------------------+
| L2  Storage                                                              |
|     Tier A: RocksDB - 6 quad orders + 3 default-graph orders, MVCC       |
|     Tier B: hash-consed hypertrie per hot graph - derived, rebuildable   |
+--------------------------------------------------------------------------+
| L1  Term dictionary - dense tagged 64-bit TermIds, order-preserving      |
|     numeric/temporal encoding, inline small values, recursive triple     |
|     terms                                                                |
+--------------------------------------------------------------------------+
| L0  Terms & I/O - reuse oxrdf, oxsdatatypes, oxttl, oxrdfxml, oxjsonld,  |
|     oxrdfio, sparesults, spargebra  (do not rewrite these)               |
+--------------------------------------------------------------------------+
```

---

## 5. L1 — Term dictionary and identifier encoding

This is the first place to deliberately diverge from Oxigraph, and the divergence sets up
everything above it.

Oxigraph keys quads by a 128-bit `StrHash` per term, so a quad key is around 64 bytes and there is
no dense integer space. SHACL_Engine interns to `u32` and gets integer comparisons in its inner
loops. A WCO trie index needs the SHACL_Engine property — **dense, small, ordered ids** — because
trie fan-out is stored as sorted arrays or roaring bitmaps of child ids, which a 128-bit hash
makes impossible.

**`TermId` = 64 bits, tagged in the top 4 bits:**

| Tag | Meaning |
| --- | --- |
| `0x0` | Dictionary-backed IRI — 60-bit dense monotonic id |
| `0x1` | Dictionary-backed literal |
| `0x2` | Blank node (graph-scoped; see §13) |
| `0x3` | **Well-known vocabulary** — static, compile-time-constant ids for `rdf:`, `rdfs:`, `owl:`, `xsd:`, `sh:`, `prov:`. `rdf:type` becomes a constant the optimizer can pattern-match on. |
| `0x4` | Inline `xsd:integer` (60-bit, order-preserving two's-complement bias) |
| `0x5` | Inline `xsd:decimal` / `xsd:double` (order-preserving IEEE-754 flip) |
| `0x6` | Inline `xsd:date` / `xsd:dateTime` (order-preserving, UTC-normalised) |
| `0x7` | Inline `xsd:boolean`, and short strings of 7 bytes or fewer |
| `0x8` | **Triple term** — index into the triple-term side table |
| `0x9`–`0xF` | Reserved (vector-embedding handles, geometry handles, …) |

Two consequences worth stating explicitly:

- **Order-preserving encodings turn `FILTER(?d > "2020-01-01"^^xsd:date)` into an index range
  scan** — the cheapest large win available, and one most stores don't take.
- **Inline values never enter the dictionary**, so a dataset of measurements doesn't pay dictionary
  cost for its numbers.

**Dictionary** lives in two RocksDB column families, `id2str` and `str2id`, with a merge-operator
refcount so deletion can reclaim. Large literals go to BlobDB. Dense id allocation is a single
atomic counter — fine for a single-node writer.

**Triple terms (RDF 1.2).** A triple term resolves to an `(s, p, o)` triple of `TermId`s in a `tt`
column family: deduplicated, recursive (a triple term may contain one). Because RDF 1.2 restricts
triple terms to object position — Jena's `StatementTerm` enforces exactly this — the index burden
is bounded: no permutation index needs to handle a triple term in subject position. Reifiers are
ordinary IRIs or blank nodes and `rdf:reifies` is an ordinary predicate, so the
`<< :s :p :o ~ :r >>` Turtle sugar decomposes into two entirely conventional triples plus one
triple term. **RDF 1.2 costs one term tag and one side table, not a redesign.** This is the single
most important thing to get right early, because retrofitting it later touches every index.

---

## 6. L2 — Storage

### 6.1 Tier A — RocksDB, used properly

Keep Oxigraph's proven layout, which is already the right answer:

- `spog`, `posg`, `ospg`, `gspo`, `gpos`, `gosp` — six quad orders
- `dspo`, `dpos`, `dosp` — three default-graph triple orders (avoids paying for a graph column)
- `graphs`, `id2str`, `str2id`, `tt`, `stats`, `default`

But use the RocksDB features Oxigraph currently leaves on the table:

- **Prefix extractor + prefix bloom filter per index CF.** A `?s :p :o` lookup becomes a
  bloom-filtered prefix seek instead of an iterator walk.
- **`SstFileWriter` + `IngestExternalFile` for bulk load.** External-merge-sort the encoded quads,
  write SSTs directly, ingest them. This is the difference between a billion triples in minutes and
  in hours, and it is the only way the load-time problem that dominates SHACL_Engine's benchmarks
  goes away.
- **Merge operators** for dictionary refcounts and for the statistics counters in §7 — no
  read-modify-write on the write path.
- **Checkpoints** for consistent backups *and* for holon branching: a checkpoint is a cheap
  hard-linked fork of a dataset. **Built.** `Store::checkpoint` takes one while the store is
  open and being written to; `holos backup` and `POST /backup` expose it. Two refusals rather
  than a wrong answer: during a bulk load, whose writes are buffered outside RocksDB, and on
  an in-memory store, which has no files to snapshot. Holon branching is
  `holos_holon::branch` — §9.
- **User-defined timestamps** for MVCC and time travel. Caveat, stated plainly: UDT is still marked
  experimental upstream. Fallback if it disappoints — an explicit monotonic version suffix in the
  key, the TiKV approach, at the cost of writing your own GC.
- **Value separation (BlobDB)** for literals above a size threshold.

Delta-encode successive keys within an index CF; with dense ids and sorted order this is very
effective, and it is why dense ids matter for the cold tier too, not just the hot one.

### 6.2 Tier B — hypertrie hot tier (memory-resident, derived)

**The central architectural bet.** Worst-case-optimal joins need trie-shaped structures with
constant-time slicing on arbitrary dimension subsets and heavy random access. LSM-trees are the
wrong shape for that. Rather than compromise, run both:

- Tier A is the **system of record**: durable, transactional, complete.
- Tier B is a **hash-consed hypertrie per hot graph**, built lazily the first time a graph is
  queried, evicted under memory pressure, kept live by the ISWC-2025 incremental insert/delete
  algorithm, and stamped with the same MVCC version as Tier A.

Tier B is *derived state*. It is never the authority; it can always be dropped and rebuilt from
Tier A. That property is what makes the correctness argument tractable: the two tiers can be
differentially fuzz-tested against each other, and any divergence is a bug in Tier B, never a
data-loss event.

Hash-consing (the "Hashing the Hypertrie" result) is not optional here — it is what makes the
memory cost of a second full index survivable, because in real RDF the same subtries recur
constantly.

**Cost, stated honestly:** roughly a doubling of resident memory for hot graphs, plus a second
update path. Mitigations are laziness, per-graph granularity, eviction, and the fact that the
derived tier is disposable. If memory measurements come in badly at P3, the fallback is to keep the
hypertrie only for graphs under a size threshold and serve everything else from Tier A with binary
joins — degraded, not broken.

---

## 7. L3 — Query engine

**Front end:** reuse `spargebra` for parsing and `sparesults` for result serialisation. Start with
`sparopt`; replace it once statistics exist.

**Statistics.** The highest-leverage estimator for RDF is **characteristic sets** (Neumann &
Moerkotte): the set of distinct predicate-sets occurring on subjects. They give accurate cardinality
estimates for star patterns, which is what RDF queries mostly are, and they are what separates
engines that plan well from engines that guess. Maintain alongside them: per-predicate triple
counts, distinct-subject and distinct-object counts via HyperLogLog sketches updated through
RocksDB merge operators, and per-graph totals.

**Physical planning is hybrid, and this matters.** "WCO everywhere" is a trap. WCO joins win on
cyclic and join-heavy patterns; binary hash and merge joins win on selective chains and stars. The
2025 SPARQLoscope results are the cautionary evidence — MillenniumDB leans hard on WCO and is not
automatically fast; QLever, with conventional but well-tuned joins, is fastest on basic graph
patterns. So: decompose each BGP into connected components, estimate each, choose per component.
The planner must be able to pick either.

**Execution model:** morsel-driven parallelism over columnar batches of `TermId` (Arrow-shaped),
not tuple-at-a-time iterators. This is the other place Oxigraph's ceiling is structural rather than
incidental. Operators spill to disk for large `ORDER BY` / `GROUP BY` / hash builds.

**Property paths** get dedicated operators — bitmap-visited BFS for `*` and `+`, with a
transitive-closure cache keyed by (predicate, direction, version). Property paths are where most
engines fall over on real workloads and where a new engine can win visibly.

**SPARQL 1.2 surface:** `VERSION` declaration; `TRIPLE()`, `SUBJECT()`, `PREDICATE()`, `OBJECT()`,
`isTRIPLE()`; `@en--ltr` base-direction literals; the tightened `OPTIONAL` / `MINUS` / `NOT EXISTS`
semantics. Note that SPARQL 1.2 Query is still a Working Draft (20 Aug 2026) while RDF 1.2 Concepts
is a Candidate Recommendation (7 Apr 2026) — so pin to RDF 1.2 semantics and treat the SPARQL
surface as movable until CR.

### The one operator that exists so far

Everything above is the plan. What is *built* is a single physical operator, `holos_engine::
bindjoin`, because measurement said it was the one thing missing rather than the next thing on
a list.

`spareval` joins by building a hash table from the left input and scanning the right input in
full. Knowing that `?s` is bound to two hundred values does not help it, because there is no
operator that can use the binding. On a three-pattern star over 753,199 quads that costs 43.9
ms against 0.072 ms hand-written — a factor of 611, rising with the data. It was also what
stopped the spatial index of §17 from paying off: narrowing fifty thousand geometries to four
changes nothing if the join then scans all fifty thousand anyway. One missing operator, felt
from two directions.

An index nested-loop join closes it: **0.535 ms on the query path, about 82×**. The remaining
10× to hand-written is generality — per-step re-estimation, hashed bindings, dictionary
decoding — not a missing operator.

**The fragment is deliberately small.** `SELECT` in the default graph over basic graph
patterns, `JOIN`, `UNION`, `VALUES` and `FILTER`, with `DISTINCT`, `LIMIT`, `OFFSET` and a
projection. Everything else is refused and falls back, so the fragment can grow without the
growth risking what already works. It has grown twice: `FILTER` bought **72×** on a selective
filtered star, because the predicate prunes branches before the remaining patterns are
scanned at all; `JOIN`, `UNION` and `VALUES` bought **2,408×** on a spatial query, for the
reason below.

**Filters are borrowed, not reimplemented.** The predicate is evaluated by `spareval`'s own
expression evaluator through this engine's function registry, so `FILTER` semantics here are
the evaluator's. SPARQL comparison is value-based with numeric promotion, and an independent
`=` that mishandled `"1"^^xsd:integer` against `"1.0"^^xsd:decimal` would be exactly the
silent wrong answer this module exists to avoid. Two classes are refused rather than
approximated, because the borrowed evaluator has neither a dataset nor an evaluation context:
`EXISTS`, which would answer `false` for every solution, and `NOW`/`RAND`/`UUID`/`STRUUID`/
`BNODE`.

**Ordering is chosen at each step, not once.** Once the first pattern binds `?s`, the others
stop being predicate scans and become subject-and-predicate lookups — a different and far
smaller estimate. Ordering once, before anything is bound, would miss exactly the effect the
operator exists to exploit.

**Policy is structural, per §14.1.** Scans go through the same `QueryableDataset` call
`spareval` makes, so `decide_quad` runs on every quad. A fast path that read the store
directly would be a way around the guarantee; this one has no such path available to it.

**It may give up half way, and that is a feature.** This operator materialises where the
evaluator streams. For the shapes it is for, the answer is small — that is the premise of the
fragment. For a shape that slipped through and is not small, it is the difference between a
slow query and a dead machine: `SELECT * WHERE { ?a ?b ?c . ?d ?e ?f }` over 20,000 triples is
400 million rows, and it was found materialising 13.7 GB with its own 60 ms timeout unfired,
because the cancellation token is consulted by the evaluator that had just been skipped. So
evaluation now runs under a row budget and the deadline's token, and hitting either returns
`None` — *ask the evaluator* — discarding the partial rows rather than returning them short.

That bug is worth recording precisely, because nothing about it was *incorrect*. Given enough
time and memory the answer would have been right. Being right eventually is a different
property from being usable, and only the first was under test.

**What the differential tests could not see.** Three bugs got past them, and they share a
shape: the fast path took over a query carrying something it did not model. Comparing answers
finds none of that directly, because two of the three do not change an answer. Besides the
cross product, `QueryOptions::substitutions` was silently dropped — the gate read
`touches_dataset`, which reports on the dataset, while a substitution changes the *query* —
and `FROM` was ignored, which *did* return wrong rows: it lives in `Query::Select::dataset`,
beside the pattern rather than inside it, so a check that reads only the pattern answers a
different question over the store's default graph.

**And the suites that should have caught them could not reach the code.** There are three
ways into evaluation. The fast path was attached to `Engine::query_with`, reached by the HTTP
server; the §15 conformance runner evaluates through `Engine::query_prepared_with_services`,
and the Python binding and audited CLI path through `Engine::query` — both of which went
straight to the evaluator. Roughly a thousand W3C queries appeared to cover this operator
while executing none of it, and the most-used surface never received it.

All three now share one `try_bind_join`, and a test compares their answers against the
evaluator's so they cannot drift apart again. With the suites actually reaching it,
`sparql10`, `sparql11` and `sparql12` are unchanged — the first real evidence the operator
agrees with the evaluator on SPARQL nobody wrote for it.

The general lesson is worth more than the operator: **a fast path attached to one entry point
inherits the reputation of the test suites it never runs under.** Coverage is a property of
the path taken, not of the tests that exist.

**What it took to make the spatial index pay.** `FILTER` was expected to be enough and was
not, which is worth recording because the mistake was a failure to look. A routed GeoSPARQL
query does not reach the planner as a filtered BGP: §17's rewrite turns
`?f geo:sfWithin <window>` into a geometry lookup joined in as a *union* of ordinary
patterns, and the spatial index joins a `VALUES` of candidate geometries onto that, giving
`Filter(Join(Join(Bgp, Union(Bgp, Union(Bgp, Bgp))), Values))`. All four node kinds had to be
in the fragment before any of it could use a bind join.

With them in, the spatial benchmark goes from **no measurable difference** — the state §17
recorded, where narrowing fifty thousand geometries to four changed nothing because the join
scanned all fifty thousand anyway — to **2,408×** at fifty thousand geometries. Two
components, each individually correct and individually tested, that did not compose until a
third existed.

Two boundaries fell out of it, both about `UNION` making possibilities that a conjunctive
plan never had. A filter can only be hoisted out of a join when its variables are *certainly*
bound in the subtree it was written against — `{ ?a ?b ?c FILTER(?d = 1) } { ?d ?e ?f }`
being the counter-example — so the plan tracks certainly-bound separately from
possibly-bound. And a `VALUES` term absent from the dictionary sends the query back to the
evaluator, because such a term still binds while having no term id, and interning one would
be a write in the middle of a read.

---

## 8. L4 — SHACL as a subsystem, not a library

> **Built, and it ended up as two validators rather than one.**
>
> `crates/holos-shacl-engine` is [pwin/SHACL_Engine](https://github.com/pwin/SHACL_Engine)
> adapted — the plan this section always described. `crates/holos-shacl`
> supplies the store bridge, the incremental planner, and a `Validate` trait that hides
> which validator is in use.
>
> | | Adapted engine | Native evaluator |
> |---|---|---|
> | Coverage | SHACL Core, SPARQL constraints, node expressions, SHACL-AF rules, inference | SHACL Core |
> | W3C SHACL 1.2 Core | **127/138** | 94/138 |
> | W3C SHACL 1.0 Core | 90/98 | **92/97** |
> | Reads | a bridged snapshot | the live store |
> | Incremental revalidation | no | **yes, 161×** |
>
> The split is forced by one fact: the adapted engine's `Graph` is immutable, so a delta
> cannot be pushed into it cheaply and a validator that re-bridges on every commit is not
> incremental however good its coverage. Hence **the engine for coverage, the native
> evaluator for the write path** — and a named gap, since the write path then checks fewer
> constraint components than a full run. Closing it means giving the engine's `Graph` a
> merge-in-place, which is bounded work and is not done on spec.
>
> What follows is the design both were built to.

Take the SHACL_Engine design wholesale — compile-once flat IR, integer-interned terms, sorted index
access, deterministic reports — and change one thing: **it reads the store's own dictionary and
indexes instead of loading its own copy.**

That single change deletes the dominant cost in its current benchmarks, where loading exceeds
validation by roughly 3× at 100k instances. It also fixes the named-graph flattening limitation,
because the store already has per-graph indexes; validation gains an explicit data-graph selector
(*this named graph* / *union* / *whole dataset*).

Two additions beyond a port:

**Incremental revalidation.** Build a shape → predicate/class dependency index at compile time. On
commit, intersect the delta's predicates with that index to derive the affected focus-node set, and
revalidate only those. Without this, SHACL-on-the-write-path is unaffordable; with it, cost is
roughly proportional to the size of the change. This is the enabling mechanism for the holon
Boundary layer.

**Rules to fixpoint.** Semi-naive evaluation to a genuine fixpoint rather than single-pass with an
`--iterate-rules N` escape hatch, plus DRed-style incremental maintenance on delete. Keep
SHACL_Engine's honesty about the caveats: rules using negation-as-failure are non-monotonic under
iteration, and `sh:closed` interacts badly with inference. Those should be *diagnosed by the
compiler* — rejected or warned at shape-compile time — rather than silently producing wrong
answers, which is the current failure mode.

---

## 9. L5 — The holon layer

> **Built as a walking skeleton.** `crates/holos-holon` implements the scene, the boundary,
> the event log and the tick. A conforming commit lands; a violating one is refused and the
> scene is left exactly as it was; `AdmitAndRecord` keeps imperfect data *and* the record of
> what was wrong with it. Every changed triple gets a reifier pointing at a triple term, so
> the log answers "who changed this statement, in which commit". Validation is incremental,
> so a commit costs the size of its own change: **165 commits/s on a 300k-triple scene, 41×
> cheaper than a full pass** (§16).
>
> Three things are deliberately *not* there, and each is visible in the code rather than
> quietly skipped:
>
> - **A tick is not atomic.** A refused commit is undone by a compensating write, not a
>   rollback, and a crash between applying the delta and writing the event leaves the two
>   disagreeing. Closing this is what §6.1's MVCC and checkpoints are for.
> - **Boundary rules do not fire.** SHACL-AF fixpoint evaluation exists in the adapted
>   engine but only over a bridged snapshot, so firing rules per tick would mean re-bridging
>   per tick — the gap §8 names.
> - **Projections recompute rather than maintain.** `Regime::Maintained` is *refused*, not
>   silently downgraded: claiming a guarantee the build does not provide would be worse than
>   declining it.
>
> What follows is the design it was built to.

This is the differentiator, and where the modelling ideas become storage primitives rather than
conventions.

**A holon is a first-class stored object** with four parts, each mapped to something the engine
already has:

| Holon layer | Engine primitive |
|---|---|
| **Scene graph** — mutable current state | a named graph, versioned, the unit of transaction |
| **Boundary graph** — legal transitions | a compiled shapes graph + rule set bound to that scene, enforced on commit |
| **Event graph** — provenance, history, causality | append-only versioned delta log, PROV-O shaped, with RDF 1.2 reifiers annotating individual triples with who / when / why |
| **Projection graph** — the visible surface | registered SPARQL queries maintained as materialised views |

**A commit is a tick**, and it is one MVCC transaction:

1. apply the delta to the scene
2. run boundary rules to fixpoint
3. validate against boundary shapes — on violation either abort or admit-and-record, per holon policy
4. append the event, with reifier-annotated provenance
5. incrementally refresh the affected projections

**Projections are incrementally maintained** using Z-set / differential-dataflow techniques (the
DBSP line of work). This is the piece that makes the holon story a database contribution rather
than a naming scheme: agents query projections, projections are cheap because they are maintained
by delta rather than recomputed, and the scene is never exposed. The Holon article's "clean
architectural separation between reading state and triggering transitions" is, in database terms,
exactly *read from an incrementally maintained view, write through a validated transaction*.

**Honest restriction:** general SPARQL is not incrementally maintainable. Restrict registered
projections to a maintainable fragment — BGP + filters + projection + distinct + simple aggregation
over that. Anything outside the fragment is accepted but recomputed on read, and the registry says
which regime a projection is in. Pretending otherwise would be the design's worst possible lie.

**Time travel and branching** fall out of the substrate: `AT VERSION n` / `AT TIME t` reads use
RocksDB user-defined timestamps (or version-suffixed keys); a branch is a checkpoint plus a fresh
event-log head.

> **Branching is built; time travel is not.** `holos_holon::branch` creates a holon starting
> from another's scene and boundary, with a fresh event log opening on a `holos:branchedFrom`
> record naming the parent and the version it diverged at. The two then move independently.
>
> The two halves of §6.1's sentence turn out to live in different places. The **checkpoint**
> is `Store::checkpoint`, a hard-linked fork of the whole dataset — cheap, but a separate
> store, so the branches cannot be queried together. The **fresh event-log head** is the
> holon-level branch, inside one store, where they can. Which to reach for depends on the
> question: forking a dataset to try a migration wants the first; comparing two futures of
> one holon wants the second.
>
> A holon branch copies its scene rather than linking it, because nothing in RDF lets two
> named graphs share storage and pretending otherwise would mean a write to one silently
> changing the other. It therefore costs the size of the scene.
>
> Versions continue rather than restart: a branch taken at parent version 7 has version 7 and
> its first tick is 8. Restarting at zero would make "version 3" ambiguous between two
> lineages whose scenes are genuinely related.
>
> `AT VERSION n` still needs the MVCC substrate, which is not built. Auditability — which the Holon article flags as an explicit architectural choice —
becomes a per-holon retention policy rather than a schema decision.

**All holon metadata is RDF** in a system graph, queryable with ordinary SPARQL. No new model.

---

## 10. L6 — Interfaces

> **Partly built.** `holos-server` serves the SPARQL 1.2 Protocol over HTTP with a YASGUI
> console at `/`, the Graph Store Protocol at `/graph`, and SPARQL 1.1 Update at `/update`.
> Both W3C protocol suites pass in full (34/34 and 13/13). PyO3 bindings are built and
> packaged as `holosdb`. **WASM is not built.**

Embedded Rust API first; then SPARQL 1.2 Protocol + Graph Store Protocol over HTTP (the
Fuseki-equivalent); then PyO3 and WASM bindings, following the pattern both Oxigraph and
SHACL_Engine already use.

### What the HTTP layer inherits for free

**Access policy.** Every request opens a `Session`, and there is no path from a request to
the data that avoids one, so §14's chokepoint covers HTTP without a line of policy code in
the server. Starting it with `--deny-predicate` hides that predicate from every query the
endpoint answers, and a classified graph disappears from `SELECT DISTINCT ?g` as well as
from its own contents.

**Identity stays at the edge.** §14.5 puts token verification in front of the store, so the
server reads *already-verified* claims from `X-Holos-Principal`, `X-Holos-Roles` and
`X-Holos-Clearance` — and **refuses to read them at all** unless started with
`--trust-forwarded-identity`. Trusting those headers on an open port would let any client
name its own roles, which is the exact inverse of what §14 is for.

**Threading matches the store.** Reads take `&self` and writes `&mut self`, so an `RwLock`
and a thread per request is the natural shape. That is also what forced `Storage: Sync`,
which in turn forced the RocksDB backend to stop holding a `WriteBatch` across calls — a raw
pointer in that type would have made the whole store un-shareable.

### The console

YASGUI is loaded from a CDN rather than adapted: it is a large JavaScript bundle with its
own licence and release cadence, and a minified copy in an RDF engine's tree makes that tree
harder to audit. The consequence is stated rather than hidden — **the console needs network
access; the endpoints do not**, and `--no-ui` removes it entirely.

Optional module: text and vector indexes exposed as SPARQL service or property functions, for
hybrid retrieval. This is the concrete reason an LLM-agent system would choose this store over an
incumbent, and it aligns with the Holon CG's stated interest in grounding conversational and
computational systems.

---

## 11. Roadmap

Each phase ends in something demonstrable and measurable. Do not proceed to the next phase without
the measurement.

| Phase | Deliverable | Exit criterion |
|---|---|---|
| **P0** ✅ | Oxigraph crates + store + evaluator | *Done, in-memory.* SPARQL 1.2 and RDF 1.2 triple terms evaluate end to end, and the W3C suites pass with no HOLOS-attributable failure (§15). Still owes the RocksDB substrate. |
| **P1** ◐ | Dense `TermId` dictionary, order-preserving encodings, SST-ingest bulk loader | *Encoding and the RocksDB tier done.* Measured **41k quads/s** persistent, **208k/s** in memory (§16). Owes `SstFileWriter` ingestion and range filters compiled into index scans. **The original 500k/s exit criterion was written before any measurement and is withdrawn** — see §16 for what replaces it. |
| **P2** ◐ | Characteristic-set statistics, cost-based optimizer, vectorized binary joins | *Statistics built and measured.* Characteristic sets estimate 6 of 7 query shapes exactly — mean q-error **1.1** against the reused optimiser's **2×10⁸** — and bad estimates cost a measured **3×** (§16). Owes the optimiser itself: the reused one has no injection point, so consuming these statistics means owning the planner. |
| **P3** | Hypertrie hot tier + WCO multi-join + hybrid planner | Wins on cyclic and join-heavy queries without regressing star and chain queries; memory overhead measured and within budget. **Gated on P2's planner**, not just its statistics — §13 Q2 compares against a *well-planned* binary join, which does not exist yet. |
| **P4** ◐ | SHACL subsystem on native indexes + incremental revalidation | *Core built.* 92/97 W3C SHACL Core (§15). Revalidation measured at **161×** a full pass, so the cost does track the delta (§16). Owes SPARQL-based constraints, SHACL-AF rules, and the SHACL 1.2 additions. |
| **P5** ◐ | Holon layer: versioned partitions, event log, IVM projections, time travel | *Walking skeleton built.* Scene, boundary, event log and the tick all work, validated incrementally at 41× a full pass (§16). Owes: atomicity (needs §6.1's MVCC), boundary rules to fixpoint, incrementally maintained projections, and time travel. |
| **P6** ◐ | HTTP protocol server, PyO3/WASM bindings, text + vector module | *Server built* — SPARQL 1.2 Protocol (**34/34**), Graph Store Protocol (**13/13**), SPARQL Update, YASGUI console, PyO3 bindings, policy enforced per request (§10). Owes WASM and the text/vector module. |

P0–P2 is a conventional, low-risk, well-understood engine. P3–P5 is the research content. Structure
the work so that abandoning P3 or P5 still leaves a usable product — that is the main insurance
policy against the risks below.

---

## 12. Risks, and how each one kills the project

**Two index tiers, two update paths.** The most likely source of silent corruption. Mitigation:
Tier B is strictly derived and disposable; differential property-based testing against Tier A on
every commit in CI; a `VERIFY` operation that rebuilds and diffs.

**Hypertrie memory cost.** Could make Tier B unusable on real datasets. Mitigation: hash-consing,
per-graph laziness, eviction, size-threshold fallback. Measure at P3 before committing further.

**RocksDB user-defined timestamps are experimental.** Time travel and MVCC depend on them.
Mitigation: version-suffixed keys as a designed-in fallback, with the abstraction boundary placed
so the swap stays local.

**Incremental view maintenance for SPARQL.** Only a fragment is maintainable. Mitigation: state the
fragment in the API, recompute outside it, never pretend.

**SPARQL conformance long tail.** Chronically underestimated. Mitigation: P0 exists precisely to
absorb it, and reusing `spargebra` / `sparesults` / `oxrdfio` removes most of it outright.

**Specifications in motion.** SPARQL 1.2 Query is a Working Draft; SHACL 1.2 Core is a Working
Draft (3 Aug 2026); the Holon CG has published no specification yet. Mitigation: pin to RDF 1.2
Concepts (CR, 7 Apr 2026), keep the SPARQL surface behind a version flag — the `VERSION`
declaration exists for exactly this — and treat holon vocabulary as configuration.

**Licensing.** Oxigraph is MIT/Apache-2.0 and the Tentris research repo is Apache-2.0/MIT, so both
are reusable; Jena is Apache-2.0 (readable as a reference, not reusable in-tree into a differently
licensed codebase without care). The hypertrie library's own licence needs checking separately, and
a commercial Tentris exists — **implement from the papers rather than copying code into this tree unless the
licence is verified.**

**"Why does this exist?"** The strongest risk. QLever is faster today; Oxigraph is more mature;
RDFox reasons better; Fluree already ships versioning. The answer has to stay the four-way
combination in §1, and specifically the holon layer — the moment this becomes "a slightly faster
triplestore", it should stop.

---

## 13. Open questions

1. **Quads or holons as the primitive?** This design keeps quads and layers holons above. The more
   radical alternative makes the holon the storage primitive and derives the dataset view. That buys
   tighter versioning and cleaner isolation, and costs interoperability with every existing RDF
   tool. The conservative choice is taken here; it should be a conscious decision, not a default.
2. **Is the hypertrie worth it given a good cost-based optimizer?** *Partly answered — §16.*
   Since that was written, the statistics have been **applied**: reordering each basic graph
   pattern before evaluation makes a badly ordered join **14× faster** and leaves a
   well-ordered one unchanged, so query cost no longer depends on how a query was typed. The
   remaining gap to a real planner is join-algorithm choice and access-path selection, which
   still needs owning the evaluator. Q2 proper — whether a WCO join beats a *well-planned*
   binary join — is now closer to being askable than it was, but the binary joins in question
   are still `sparopt`'s.
   The precondition holds: characteristic sets cut mean q-error from 2×10⁸ to **1.1**, and
   mis-estimation costs a measured **3×** on a five-pattern query. So accurate planning is
   both achievable and worth having. What is still open is Q2 proper — whether a
   *well-planned* binary join leaves anything for a worst-case-optimal join to win. That
   needs a planner consuming the statistics, and the reused optimiser offers no injection
   point, so P3 is now gated on a build decision (own the planner) rather than on
   enthusiasm.
3. **Blank-node scoping across holon versions and branches** — skolemise on write, or carry
   graph-scoped labels? Skolemisation is simpler and makes diffs meaningful; it changes what
   round-trips.
4. **Does the Boundary layer double as the authorization surface?** If agents only ever read
   projections, it can. That is an attractive property and needs a security review before it is
   relied on.
5. **Which benchmark is the target?** SPARQLoscope is the most comprehensive current option and
   should be the reported number, with WatDiv and the Wikidata query logs alongside for join-heavy
   and real-world coverage respectively.

---

## 14. Security

§13 Q4 asked whether the holon Boundary layer could double as the authorization surface. It
can — but only if enforcement sits in one specific place, and only if three other subsystems
are given their own rule. Both halves of that are below.

### 14.1 Enforce at the scan, never in a query rewrite

Every read in HOLOS reaches data through one function: `quads_for_pattern`, wrapped by the
dataset view. Policy is applied there and nowhere else. That gives a property worth stating
formally, because it is the whole security argument:

> For a principal **A** under policy **P**, the answer to any query **Q** equals the answer
> to **Q** evaluated over the sub-dataset that **A** is permitted to see.

The usual alternative — rewriting the SPARQL algebra to add authorization filters — is a leak
generator. There is always an operator the rewrite forgot: a property path, a `MINUS`, a
`NOT EXISTS`, a subquery, an aggregate that counts a row it may not display. Filtering at the
scan cannot be routed around, because nothing above the scan has another source of quads. The
implementation carries a test named for exactly this
(`policy_survives_every_operator_that_defeats_query_rewriting`).

### 14.2 Granularity

Five levels, coarse to fine. The one that matters most is the second: a named graph is already
a column in every index key and a first-class filter, so per-graph authorization costs almost
nothing — and a named graph *is* a holon's scene.

| Level | Governs | Cost |
|---|---|---|
| Dataset | Whether this principal may open the store at all | free |
| **Named graph / holon** | Read, write, validate, administer — per graph | one integer set lookup |
| Predicate | Hiding a sensitive column across every graph | one integer set lookup |
| Graph × predicate | The exception case: HR may see salaries, but only in the HR graph | one integer set lookup |
| Classification label | A lattice of level + compartments, against the principal's clearance | one integer set lookup |

Specificity decides: graph × predicate beats predicate beats graph beats the default. At equal
specificity, **deny wins**. Exceptions therefore live in the principal match rather than in a
competing allow — `PrincipalMatch::Not` exists because "deny this to everyone except role R" is
the commonest shape a real policy takes, and specificity cannot express it.

Clearance is checked **before** the rules and is not overridable by them. A label says the
principal may not know the data exists; no allow rule elsewhere in the policy should be able to
undo that.

### 14.3 Why it is affordable

Evaluating rules per quad would be hopeless. A policy is *compiled* against a store and a
principal into sets of dense `TermId`s, so a per-quad check is a couple of integer lookups.
This is the §5 dense-identifier decision paying off a second time — a 128-bit hashed term key
would make these sets far more expensive.

Compilation introduces one hazard, and it fails in the dangerous direction. A rule naming an IRI
the dictionary has never seen cannot resolve to an id, so it enforces nothing; if that IRI later
arrives in the data, the compiled policy is **less restrictive than the policy it came from**.
Compiled policies therefore record the dictionary size they were built against and report
themselves stale, and a session recompiles rather than trusting a cached decision.

### 14.4 Filter or fail

Two semantics, chosen per policy:

- **Filter** (default) — hidden quads simply do not exist for this principal. Safe, composable,
  and what makes the §14.1 property true. The cost: a principal cannot tell an incomplete answer
  from a complete one.
- **Fail** — touching forbidden data raises an error. Correct where a partial answer is worse
  than none (reconciliation, regulatory reporting), at the cost of revealing that hidden data
  exists.

### 14.5 Enterprise interoperability

HOLOS authenticates nobody. Verification — OIDC/OAuth2 token signatures, Kerberos/SPNEGO, mTLS,
SAML — belongs at the edge: an HTTP front door, a sidecar, or the embedding application. What
crosses into HOLOS is an **already-verified** claim set. That keeps cryptography off the query
path, and it means HOLOS interoperates with whatever identity system is already deployed rather
than competing with it.

The translation is the interesting part: **a principal is RDF**. Claims become triples in a
system graph, so an access rule can be written against a principal using the same vocabulary as
everything else, and there is no second policy language to learn or to get wrong.

| Enterprise concern | How it lands |
|---|---|
| Identity | Verified claims → `Principal`: id, roles/groups, arbitrary attributes |
| RBAC | `PrincipalMatch::Role` |
| ABAC | `PrincipalMatch::Attribute`, over any claim the IdP emits |
| Classification / MLS | `Label` — level plus compartments, with lattice dominance |
| External PDP (OPA, XACML, Cedar) | `PolicyProvider`, consulted per session rather than per quad |
| Audit / SIEM | `AuditSink`; the holon Event graph is already an append-only PROV-O log |
| Encryption at rest | Platform and storage layer. Per-holon keys would additionally make crypto-shredding a holon a real erasure mechanism |
| Encryption in transit | TLS at the protocol server |
| Least privilege in-process | A `Session` is a capability. No API reads quads without one, so no future code path can quietly acquire ambient authority |

One escaping detail worth naming: a subject claim is attacker-influenced text and it is
concatenated into a principal IRI. It is percent-escaped, and there is a test that a subject
containing IRI syntax cannot forge another principal's identifier.

### 14.6 The three places information escapes the scan

The §14.1 property holds only because every read goes through one door. Three subsystems can
carry information around it, and each needs its own rule. **None of these is implemented yet** —
they are constraints on P4 and P5.

**Statistics.** Per-predicate counts, HyperLogLog sketches and characteristic sets are computed
over the whole store. Exposing a global count tells a principal about data they cannot read.
Statistics must be either policy-scoped or operator-only, and must never reach a
principal-visible `EXPLAIN` or result.

**Materialised inference.** If a rule derives a visible triple from hidden premises, the
conclusion leaks the premises. The information-flow answer is standard, and it is what
`Label::join` exists for: **a derived fact carries the least upper bound of its premises'
labels.** A rule engine that ignores this launders restricted data into an unrestricted
conclusion.

**Projections.** A holon projection is computed once and read by many. A view computed with
elevated privilege and read by a low-privilege agent is a *declassification*. That is a feature —
controlled declassification through views is precisely how an agent-facing surface is made safe —
but it must be explicit: a projection declares its own label, and someone with `Modes::ADMIN`
authority signs it off. It must never happen by accident.

### 14.7 Write-side authority

Reads are only half of it.

- **Deleting requires read as well as write.** Otherwise "did the delete succeed" is an oracle
  for whether hidden data exists.
- **`Modes::ADMIN` is separate from `Modes::WRITE`** by design. Authority to change the shapes,
  rules or policy is not authority to change the data; conflating them turns a data-entry role
  into a privilege-escalation path.
- **The event log is append-only.** No mode grants the right to rewrite history — auditability a
  sufficiently privileged principal can erase is not auditability.
- **Boundary shapes gate what may be written; policy gates who may write it.** Both run inside
  the same commit.

### 14.8 What is not claimed

- **No covert-channel resistance.** Timing and resource-exhaustion channels are not addressed. A
  principal who can measure query latency can probably learn something about data volume they
  cannot read.
- **No cell-level redaction.** A quad is visible or it is not; there is no "return the row with
  the object masked". That is expressible as a projection and belongs there.
- **No formal verification.** The §14.1 property is argued from the chokepoint and tested against
  the operators known to defeat rewriting. It is not proved.
- **No encryption.** See §14.5 — deliberately out of scope for this layer.

---

## 15. Conformance

`cargo test -p holos-conformance`, against the `w3c/rdf-tests` suites fetched by
`scripts/fetch-testsuites.sh`. The suites are not committed to this tree; without them these tests skip,
so a fresh checkout still builds green.

| Suite | Passing | Failing | Skipped | HOLOS bugs |
|---|---|---|---|---|
| RDF 1.1 | 987 / 1041 | 54 | 0 | **0** |
| RDF 1.2 | 1326 / 1406 | 80 | 0 | **0** |
| SPARQL 1.1 | 523 / 524 | 1 | 101 | **0** |
| SPARQL 1.2 | 262 / 266 | 4 | 3 | **0** |
| SPARQL 1.0 | 262 / 263 | 1 | 20 | **0** |
| SPARQL Protocol | **34 / 34** | 0 | 0 | **0** |
| Graph Store Protocol | **13 / 13** | 0 | 0 | **0** |
| SHACL Core | 92 / 97 | 5 | 1 | 5 |
| SHACL 1.2 Core (native) | 94 / 138 | 44 | 0 | 44 |
| SHACL Core (adapted engine) | 90 / 98 | 8 | 0 | 8 |
| SHACL 1.2 Core (adapted engine) | **127 / 138** | 11 | 0 | 11 |

### What is actually under test

`spargebra`, `oxttl` and `spareval` are reused from Oxigraph and already conformance-tested
upstream. What is new — and therefore what needs the suites — is everything between: the term
encoding, the nine-order index, and the dataset view. So the suites are run in a shape that
isolates that.

**The RDF suites run as a round-trip through the store.** Parse the input, load it into a
`Store`, read it back, compare up to blank-node isomorphism. A difference there is a HOLOS bug:
a literal the inline codec mangled, a triple term the dictionary lost, a quad an index dropped.
2,447 tests round-trip with no change, which is the strongest evidence available that the §5
encoding is faithful — including the canonicality rule that keeps `"1"^^xsd:integer` and
`"01"^^xsd:integer` distinct.

**The SPARQL suites run with an oracle.** When a query test fails, the harness re-runs the same
query through the same evaluator over an `oxrdf::Dataset` — the reference storage `spareval`
ships with — and compares HOLOS against it:

- both give the same answer → the storage is faithful and the gap is in the evaluator
- the answers differ → HOLOS lost or changed something, and that is a real bug

Every SPARQL 1.1 test that fails against its expected results agrees with the reference
storage, so each is evaluator behaviour rather than a storage fault. This is the differential
rig §12 asks for against the future Tier B hypertrie, built early because it is exactly as
useful now.

### The protocol suites need a server, so they get one

`GraphStoreProtocolTest` and `ProtocolTest` are not queries. Each is a scripted HTTP
conversation in the W3C `ht:` vocabulary — a list of requests with methods, paths, headers,
bodies and sets of acceptable response statuses. Short-circuiting the socket and calling the
handler directly would test the handler rather than the server, which is not what these
tests are for.

So `cargo test -p holos-conformance --test protocol` and `--test sparql_protocol` start the
real `holos-server` binary on an ephemeral port, over an empty in-memory store, and replay
each conversation against it. A fresh server per test: the scripts build on their own
effects — `PUT` then `GET` then `DELETE` on one graph — so a store carried between tests
would make results depend on execution order.

Three things the manifests leave to the runner, each decided in `tests/sparql_protocol.rs`:

* **The endpoint.** Every path starts `/sparql/`, and the manifest says in as many words
  that a runner substitutes its own. This server splits query from update, so the choice
  follows what the request carries — an `update` parameter or an update media type.
* **The dataset.** `ut:graphData` names graphs the server must already hold. They are loaded
  over the Graph Store Protocol, which is both convenient and one more exercise of it.
* **The status.** Expectations are whole classes (`hts:StatusCode2xx`), because the protocol
  leaves the choice within a class open.

Both suites went from *nothing run* to fully passing, and getting there found real gaps
rather than harness problems. What the Graph Store suite exposed: `DELETE` emptied a graph
but left its catalogue entry, so a second `DELETE` could not answer 404; `POST` to the
endpoint itself was rejected rather than minting a graph and returning `Location`; and a
multipart upload stored only its first part — two documents in, one document stored, and a
204 saying it had worked. What the SPARQL Protocol suite exposed is in the list below.

### The protocol suite found six ways to be too permissive

Twelve of the thirty-four failed on the first run. Four groups, all real:

| What the server did | What the protocol requires |
|---|---|
| Refused `using-graph-uri` outright | Name the update's dataset from the request |
| Could not parse `CONSTRUCT { <s> <p> 1 }` | Resolve relative IRIs against a service base URI |
| Answered two `query=` parameters | 400 — the request is ambiguous, not a list |
| Ran a POST body with no `Content-Type` | 400 — a form body and a query body are indistinguishable without one |
| Decoded a `charset=UTF-16` body as UTF-8 | 400 — the protocol fixes both media types at UTF-8 |
| Accepted `using-graph-uri` beside `WITH` | 400 — carrying both is a client error, not a precedence question |

The first is the substantial one. `using-graph-uri` is applied to the *parsed* update rather
than by editing its text, which is what makes the last row possible: an operation that
already carries `USING` or `WITH` is visible as such, so the conflict can be reported instead
of silently resolved. Overriding either way would run an update over a dataset its author did
not choose.

### A checkout can corrupt a test suite

Worth recording because it cost a morning and would have gone on being invisible.

Two Graph Store tests failed with 415 on a request whose `Content-Type` was plainly correct.
The cause was neither the server nor the harness: Git for Windows ships with
`core.autocrlf=true`, and the fixtures had been checked out with every LF rewritten to CRLF.
The manifest bodies contain Turtle `\r` escapes — deliberate, because the multipart format
requires CRLF — so each line ending arrived as **CR CR LF**, and the multipart parser could
not find a header separator.

The fixtures are byte-exact test data, and a checkout that rewrites them changes what the
suite asserts. `scripts/fetch-testsuites.sh` and its PowerShell twin now pin
`core.autocrlf=false` and `core.eol=lf`, re-checkout an existing clone under the new setting,
and warn if a CR survives anyway. Re-running every suite afterwards showed no other result
had been affected — but nothing had been *checking*, which was the actual problem.

### SHACL is measured differently, and the failures are ours

The RDF and SPARQL suites test storage under a reused front end, so a failure is usually
upstream. SHACL is different: `holos-shacl` is new code all the way down, so **every SHACL
failure is a HOLOS failure** and the table says so.

The five remaining SHACL Core failures are one ill-formed-literal edge case, two complex
property-path shapes, one numeric-comparison case and one `sh:lessThan` case. The 44 SHACL
1.2 failures are 1.2 features that were never implemented — severity and message declared on
constraint components rather than shapes, the 1.2 deactivation rules, node expressions.
SPARQL-based constraints and SHACL-AF rules are also absent; they need pre-binding through
L3 and the fixpoint engine §8 describes.

Getting there took three fixes, two of which were in the *harness* rather than the
validator, and both were found by reading a failing diff rather than guessing: the expected
report was being extracted by following every object from `mf:result`, which dragged each
shape's whole definition and then each blank-node focus node's data into the "expected"
graph. Only blank nodes reachable through report-structural predicates belong there. The
third was real: `"aldi"^^xsd:integer` is a well-formed RDF term and not an integer, and
SHACL calls that a violation — a datatype IRI comparison alone is not enough.

### The SPARQL 1.0 suite was on disk and never run

Added late, and the reason is worth keeping. `FROM` and `FROM NAMED` had no coverage in this
tree at all: the SPARQL 1.1 suite contains exactly two queries using `FROM`, and both are
`CONSTRUCT`. The nineteen tests that exercise dataset specification live in the SPARQL 1.0
suite, which `scripts/fetch-testsuites.sh` has always cloned and no ratchet ever read.

It went unnoticed because the newer suite looks like it supersedes the older one. It does not
— SPARQL 1.1's manifests test what 1.1 *added*. Anything 1.0 settled is tested in 1.0's
manifests and nowhere else.

`sparql10` is now a ratchet of its own at **262/263**, its single known failure a parser gap
upstream in `spargebra`.

### The skips are not a hiding place

101 SPARQL 1.1 tests are skipped, down from 304, and each skip names a roadmap item rather
than a mystery: **70** need an entailment regime, which is the reasoner at L4 (§8); **25**
are upstream evaluator behaviour, confirmed by the oracle; **3** are `CSVResultFormatTest`
and **3** `ServiceDescriptionTest`, neither of which the harness reads yet.

The 203 that used to be skipped and now run are SPARQL Update (94), the two protocol suites
(47), update syntax (55), and the DAWG `rs:ResultSet` result encoding — which took SPARQL 1.0
from 45% coverage to 93% by itself.

`cargo run --release -p holos-conformance --example coverage` prints coverage beside
correctness, because a suite that skips 300 of 625 tests can still report 100%. It takes the
protocol suites' results from their ratcheted baselines rather than re-running them, and says
so; an absent baseline counts as a skip, since claiming coverage from a run that did not
happen is the dishonesty the example exists to prevent.

### The ratchet

Each suite has a checked-in known-failure list under `conformance/`. A run fails if a passing
test starts failing **and** if a listed test starts passing — the second direction matters as
much as the first, because a stale list is a list nobody trusts. Re-baseline deliberately with
`HOLOS_UPDATE_CONFORMANCE=1`.

---

## 16. What the store measures

One million triples, N-Triples, eight predicates, 489,479 distinct dictionary terms, on a
Windows laptop. Release build. Numbers include parsing.

| Configuration | Throughput | On disk |
|---|---|---|
| In memory | 208,161 quads/s | — |
| RocksDB, `--bulk` | 40,782 quads/s | 48 MB |
| RocksDB, no `--bulk` | 17,782 quads/s | — |

### The P1 target was wrong, and here is the evidence

§11 originally set P1's exit criterion at 500k triples/s. That number was written before
anything had been measured. It is roughly 12× the persistent path's actual rate and is
withdrawn rather than left standing as an unearned claim.

Three measurements say where the time goes, and rule out the guesses:

- **Re-loading the same file into an already-populated store runs at 45k quads/s** — only 11%
  faster than the cold load, despite allocating no ids and writing no dictionary rows. Term
  interning is not the bottleneck.
- **`--bulk` is 3.3–3.6× faster than not**, measured across three scales in
  [BENCHMARKS.md](BENCHMARKS.md) — the advantage grows with the dataset. So the write-ahead log and
  per-quad batching do cost real time, and buffering recovers it.
- **100k and 1M load at the same rate**, so the cost is linear. There is no algorithmic defect
  to find.

What remains is the per-quad cost of pushing three to six index keys through the memtable,
and it is the same whether a key is new or an overwrite. That is precisely the work
`SstFileWriter` ingestion skips: sorted SST files are handed to the LSM directly, bypassing
the memtable. §6.1 named it; it is not built.

**The replacement criterion:** P1 exits when SST ingestion lands and the persistent bulk path
is measured again — against this 41k/s baseline, on a dataset large enough that it cannot be
held in memory. A target is worth setting once there is a measurement to set it against.

Three tuning attempts did *not* move the number, and are recorded so they are not repeated:
disabling auto-compaction during the load, enlarging the memtables, and removing the
allocations on the term-lookup path. The first two remain in the code because they are
correct on their own terms; none of them was the bottleneck.

### SHACL, and the claim §8 rests on

400,000 triples, 100,000 instances, four shapes, 10% of instances seeded to violate.

| Phase | Time |
|---|---|
| Load into the store | 2.218s |
| Compile the shapes | 0.0005s |
| Full validation | 0.812s (20,000 results — exactly the seeded violations) |
| Full validation after a one-triple change | 0.975s |
| **Incremental revalidation of that change** | **0.006s (1 result)** |

**161×.** That is the number §8 needs: it is what makes SHACL affordable on the write path,
and therefore what makes the holon Boundary (§9) able to gate every commit rather than run
as a nightly batch.

Two smaller results are worth keeping. **Compiling the shapes costs half a millisecond** —
the compile-once property doing its job, so it is free to hold compiled shapes and validate
repeatedly. And **validation does no loading at all**: the 2.2s load is the store's, paid
once and shared with the query engine and the policy layer. A validator that is a library
pays it again into its own structures, which is the cost §8 set out to delete.

The honest qualifier: this is not a like-for-like benchmark against SHACL_Engine. It is a
measurement of the mechanism the design predicted, on a workload chosen to exercise it. A
comparison against SHACL_Engine on identical inputs is still owed.

### Does the bridge earn its keep?

The adapted engine keeps its own `Graph`, so §8's claim reduces to a measurable question:
is feeding it from a populated store cheaper than letting it parse the file?

| | Time |
|---|---|
| Engine parses the file itself | 1.625s |
| Bridge from a live store | **0.446s** |
| | **3.6× cheaper** |

Both produce the same 400,000 triples. Parsing was the dominant term, and the bridge removes
it. What the bridge does *not* remove is a second term table: the engine's `TermId` is a
dense `u32` index into its own interner while HOLOS's is a sparse tagged `u64`, so each
**distinct** term is decoded once and re-interned once, with repeats costing a hash lookup.
That is the honest limit of "reads the store's own dictionary" — parsing goes, the term
table does not, and rewriting the engine's interner to take HOLOS ids would touch everything
that indexes by them.

End to end on the same 400k graph, both finding exactly the same 20,000 violations:

| | Prepare | Validate | Total |
|---|---|---|---|
| Native, live store | 0.001s | 0.823s | 0.824s |
| Adapted, bridged | 0.522s | **0.224s** | 0.746s |

The engine validates **3.7× faster** — flat sorted arrays of `u32` beat a store-backed scan —
and spends the difference on bridging. It wins on total time *and* on coverage, which is why
it is the default for a full validation.

### What a holon tick costs

100,000 instances, 300,000 triples in the scene, a four-shape boundary. Validation runs
inside every commit.

| | Time |
|---|---|
| Full validation of the scene | 0.250s |
| One tick | 0.0076s |
| 200 ticks, per commit | **0.0061s — 165 commits/s** |
| A *rejected* tick | 0.0101s |
| | **41× cheaper than a full pass** |

That ratio is what decides whether §9 is a design or a wish. Validation inside every commit
is only sane if a commit costs the size of its own change; at full-pass cost a Boundary would
be a nightly batch job wearing a transaction's clothes, and the holon model would collapse
back into "validate the warehouse overnight and hope".

A refusal costs slightly *more* than an acceptance, because it also undoes what it applied.
That is the right way round: the system pays for rejecting bad data, not for accepting it.

The honest reading of 165 commits/s: it is not a high-throughput write path, and it is not
meant to be. It is the cost of a fully validated, fully attributed commit against a
non-trivial scene — the alternative being 4/s if each commit revalidated everything.

### Two other numbers worth keeping

**The dictionary holds 489,479 terms for a million triples.** Every `xsd:integer`, every
`xsd:float` and every city name of six bytes or fewer inlined into its id and never reached
storage — the §5 encoding doing exactly what it was designed to do.

**48 MB on disk for a 90 MB source file**, with three index copies of every triple. Dense
64-bit ids plus LZ4 are why; a 128-bit hashed key would roughly double the index.

### How far it goes

> **Measured.** 10,000,000 triples, eight predicates, on the same laptop.

| | 1M | 10M |
|---|---:|---:|
| Bulk load | 41k quads/s | **35.6k quads/s** |
| On disk | 48 MB | **453 MB** |
| Dictionary terms | 489,479 | 3,755,433 |
| Bytes per quad on disk | ~48 | **~47** |

Load throughput fell **13%** going up an order of magnitude, and bytes-per-quad did not
move at all — so nothing in the storage layer degrades super-linearly across that range.
Three index copies of every quad plus LZ4, over dense 64-bit ids, is what holds the figure
flat.

Query cost at 10M, from a cold process (about 0.4s of that is start-up):

| | Time |
|---|---:|
| Point lookup by subject | 0.44s |
| `COUNT` over one predicate (1.25M rows) | 2.14s |
| **Full scan of all 10M quads** | **15.1s — ~660k quads/s** |
| 3-way star with a bound object, `LIMIT 20` | 2.83s |
| 2-hop join, `LIMIT 20` | **13.7s** |
| the same 2-hop join anchored to one subject | **2.03s** |

**The last two rows are the whole story about scale here.** A `LIMIT 20` two-hop join
should touch a few dozen quads. Instead it costs almost exactly a full scan — because the
planner cannot tell which pattern is selective and starts from the wrong one. Anchoring the
same query by hand makes it **6.7× faster**. That is §16's 3× penalty, measured again at a
hundred times the data, and it has grown with the dataset exactly as one would expect.

So the ceiling is not the storage:

- **Storage scales as expected.** ~47 bytes/quad and a 660k quads/s scan mean 100M quads is
  about 4.5 GB and a 2.5-minute full scan; 1B is about 45 GB and 25 minutes. Those are
  ordinary numbers for a single node.
- **Loading becomes the first real obstacle.** At 35k quads/s, 1B quads is roughly 8 hours.
  `SstFileWriter` ingestion (§6.1, not built) is the fix, and it is the same one P1 is
  already waiting on.
- **Planning becomes the binding constraint well before either.** Once a full scan costs
  minutes, a query that accidentally does one because the estimator misjudged a pattern is
  no longer a nuisance but a failure. Everything above ~100M quads depends on P2's planner
  existing.

**Honest limits of this measurement:** 10M is the largest dataset actually loaded. Beyond
that the figures above are extrapolation from two points, on one laptop, with a synthetic
eight-predicate dataset far more uniform than real RDF. Real data has skew, and skew is
precisely what a constant-table estimator handles worst.

---

### What the planner is flying blind on

> **Measured.** `cargo run --release -p holos-stats --example estimator_accuracy`

§7 claims characteristic sets are "what separates engines that plan well from engines that
guess". §13 Q2 makes P3 conditional on that claim being true. Both were assertions until
this measurement.

The reused optimiser reorders joins with an estimator that is a **fixed lookup table**:
`?s <p> ?o` is estimated at 1,000,000 rows whether that predicate occurs three times or three
million. It has no access to the data at all. That is not a criticism — an optimiser shipped
as a library without a store has nothing to consult, and a constant is the only honest thing
to return from that position. But it is exactly the gap a store-aware estimator fills, so the
two were run against the same queries and the same ground truth. Error is reported as
**q-error** (`max(est/actual, actual/est)`), which scores 100× over and 100× under alike; a
perfect estimate is 1.

20,000 people and 100 organisations — 86,967 triples, 20,100 subjects, 3 distinct shapes:

| query shape | actual | constants | q-error | char. sets | q-error |
|---|---:|---:|---:|---:|---:|
| one common predicate | 20,000 | 1,000,000 | 50 | 20,000 | **1.0** |
| one rare predicate | 100 | 1,000,000 | 10,000 | 100 | **1.0** |
| two-predicate star | 20,000 | 10⁹ | 50,000 | 20,000 | **1.0** |
| star with a rarer arm | 6,667 | 10⁹ | 149,993 | 6,667 | **1.0** |
| four-predicate star | 20,000 | 10¹³ | 5×10⁸ | 10,000 | 2.0 |
| star that never co-occurs | **0** | 10⁹ | 10⁹ | **0** | **1.0** |
| org star | 100 | 10⁹ | 10⁷ | 100 | **1.0** |
| **mean q-error** | | | **215,744,292** | | **1.1** |
| **worst q-error** | | | **10⁹** | | **2.0** |

Six of seven shapes are estimated *exactly*. The row that matters most is the last shape: a
star over two predicates that **never occur on the same subject**. It has zero answers, and
the constant table predicts a billion rows — the single worst thing an estimator can do,
because a planner told to expect a billion rows will build for a billion rows. Characteristic
sets return 0, because they record which predicates actually co-occur rather than assuming
predicates are independent. In real RDF they are strongly correlated: entities of the same
kind carry the same properties, which is the whole reason the naive
`selectivity(p₁) × selectivity(p₂)` product goes wrong.

**The one case this gets wrong, and why.** The four-predicate star scores 2.0: 10,000
estimated against 20,000 actual. The `rdf:type ex:Person` arm has a *bound object*, and the
estimator divides by the number of distinct objects for `rdf:type` — which is 2 (`Person`,
`Org`) — assuming the subjects split evenly between them. They do not; 20,000 of 20,100 are
people. The standard fix is a most-common-values list per predicate, which every mature
optimiser keeps. It is not implemented here, because tuning until the benchmark reads 1.0
would be fitting the estimator to its own test.

### Does being wrong cost anything?

> **Measured.** `cargo run --release -p holos-stats --example does_order_matter`

A badly *calibrated* estimator can still rank alternatives correctly, in which case the
numbers above are ugly and the plans are fine. So: the same five-pattern query, returning the
same single answer, with its most selective pattern written first and then last. One pattern
matches exactly 1 triple in 150,021; the others match 50,000 each.

| | Rows | Time |
|---|---:|---:|
| Most selective pattern written first | 1 | 0.0777s |
| Most selective pattern written last | 1 | 0.2352s |
| | | **3.0×** |

Written order survives into the plan. The estimator cannot tell a 1-row pattern from a
50,000-row one, so it cannot fix a bad order, and the query pays 3× for how it happened to be
typed. Five patterns is a small query; the penalty grows with the number of orderings
available.

### What this settles, and what it does not

It settles the **precondition** for §13 Q2: accurate estimates over RDF are cheap and
available, and inaccurate ones cost real time. It does not yet answer Q2 itself, which asks
whether the hypertrie beats a *well-planned* binary join. Answering that needs a planner that
consumes these statistics — and there is no injection point:
`Optimizer::optimize_graph_pattern` is a free function called internally by the evaluator, so
using these numbers means owning the planner rather than configuring one.

That is now a decision with numbers attached rather than a preference, which is what P2 was
for. It also moves §17's R-tree from "blocked on the optimiser" to "blocked on the same
planner", and it is the same seam in both cases.

**What is deliberately not claimed:** this measures estimation quality and one plan-order
penalty. It is not an end-to-end query benchmark, and no claim about query throughput is made
here. §13 Q5's benchmark choice remains open.

---

## 17. Geospatial

> **Built.** GeoSPARQL runs through the ordinary query path.

`spargeo` supplies **43** GeoSPARQL functions — the topological relations of all three
families (Simple Features, Egenhofer, RCC8), plus distance, area, length, centroid, convex
hull, envelope, the set operations and the GeoJSON accessor. HOLOS adds **2 more**,
`geof:buffer` and `geof:boundary`, in `holos-engine`'s `geo_ext`: **45 in total**.

> An earlier draft of this section listed buffer and boundary as though `spargeo` provided
> them. It does not, and nothing had checked — the claim survived until a function probe ran
> against a live endpoint and two of the names came back unknown. They are implemented now,
> which is why the sentence above is true; it was not when it was first written.

### The set operations are wrapped, not just registered

`geof:union`, `geof:intersection`, `geof:difference` and `geof:symDifference` are
`spargeo`'s implementations called through a wrapper in `geo_ext`, because their output
needed correcting.

`geo`'s boolean operations go through `i_overlay`, which works on an integer grid and
converts back on the way out. Coordinates that are exactly representable survive; the rest
come back shifted by around 1e-10. `-83.2` becomes `-83.20000000009313`.

That is about 0.01 mm on the ground and harmless for anything measuring distance. It is
**not** harmless for the exact topological predicates, which turn on whether two boundaries
coincide:

```text
sfTouches(C, A)             → true
sfTouches(C, union(A, D))   → false     ← the same shared edge, now 1e-10 apart
```

`sfTouches`, `sfEquals` and `sfCrosses` therefore stopped composing with any computed
geometry, and did so silently — the answer was wrong, not merely imprecise. It was found by
validating against Ontotext's GeoSPARQL example, not by a test here.

**The fix is snapping, not rounding**, and the distinction is the whole design. Rounding
every output coordinate to the inputs' decimal places would also move genuinely *new*
vertices: two integer-coordinate edges crossing at x = 1.5 would round to 2, turning a
correct intersection into a wrong one. Instead each output coordinate is compared against the
coordinates that went in and replaced only when within 1e-9 of one — so a preserved vertex
returns to its exact input value, while a computed intersection point, matching no input, is
left exactly as produced.

Wrapping `spargeo` rather than reimplementing keeps one implementation of the operation
itself; only the coordinates are touched on the way out.

The two additions match `spargeo`'s literal conventions exactly — CRS84 only, both
`wktLiteral` and `geoJSONLiteral` accepted, output in whichever the arguments used, the same
OGC unit IRIs — because a function that round-tripped literals differently from its
neighbours would be worse than one that did not exist. `geof:buffer` computes a metric
radius in a local equirectangular projection centred on the geometry, so a 100 km buffer
spans about twice as many degrees of longitude at 60°N as at the equator, which is correct;
it is **not** a geodesic buffer and should not be trusted over continental distances.

All 45 are registered on the evaluator, so they compose with the rest of SPARQL and with
everything below: a WKT literal is a typed literal that takes the dictionary path like any
other, and **access policy applies to geometry** — denying `geo:asWKT` makes a spatial join
find nothing, because §14's property does not get a geospatial exemption.

Reused rather than rewritten, for the same reason as the rest of L0 (§4): conformance-heavy
geometry code that already exists and is already tested.

### The index is updated, not rebuilt

The R-tree earns 2,408× on reads, and was rebuilt in full on every write — which made that a
property of a read-only store. One write to a 200,000-geometry store cost about four thousand
probes' worth of work, and `is_current_for` correctly made every query in the meantime fall
back to a full scan.

Measuring where a rebuild's time went pointed at the answer. The quad scan is **7%**;
decoding terms and parsing their WKT is **57%**; packing the tree is **36%**. Avoiding the
decode and the parse was clearly the prize — but it turned out the scan could go too.

**§5's dictionary makes the scan unnecessary.** Each dictionary-backed kind has its own
dense, append-only index space, so every literal ever interned is `TermId::new(Tag::Literal,
i)` for some `i` below the current count. An index that remembers how far it has read finds
everything since by reading from there. No scan of the store, no set of terms already
examined, and a cost proportional to what was interned rather than to what is held:
**0.1–0.3 ms whether the store holds 50,000 geometries or 200,000**, against a 56–271 ms
rebuild.

**Why it may insert and never delete.** The index is a superset filter, and §17's rewrite
joins the `VALUES` it produces back against the store — so a geometry the index still lists
after its quads are gone fails to join and contributes no row. Omitting a geometry is a
silently missing answer; keeping one that has left is invisible. So the index tracks the
*dictionary* rather than the store, and the dictionary is append-only by construction. What
it holds is bounded by how many distinct geometries have ever been interned, not by how many
are currently reachable — the same growth §5 already accepts for the dictionary itself.

That also made the staleness check **exact rather than heuristic**. It compared quad and term
counts, which declared the index stale after any write whatsoever. Now the index holds a
geometry for every literal below its watermark, and every geometry in the store is a literal
in the dictionary, so a level watermark means nothing can be missing — and a write that adds
quads over geometries already interned costs nothing at all.

Past ten thousand one-at-a-time inserts the tree repacks itself from what it already holds,
because `rstar` builds a far better tree from a bulk load than from repeated insertion. That
pays the tree construction and none of the parsing.

Two tests carry it: one asserts a refreshed index answers *identically* to a rebuilt one
across several probe windows and rounds of writing; the other deletes every geometry quad and
asserts the leftover entries produce no rows, which is what makes over-inclusion safe rather
than merely convenient.

### Reclaiming the dictionary: `holos compact`

The dictionary is append-only, and §5 depends on that — so does everything derived from it,
including the spatial index's watermark. Deleting quads therefore reclaims their index
entries and nothing else: the terms they used stay interned for ever, and a store that has
churned carries a dictionary sized by every term it has *ever* seen.

Neither backup nor restart clears it. A backup is a RocksDB checkpoint — the SST files are
hard-linked — so a restore hands back the same dictionary, dead entries included. That is the
right behaviour for a backup and it is worth being explicit about, because "restore from
backup" is the intuition people reach for.

**Tombstoning was the plan and the evidence went against it.** Freeing a dictionary slot in
place means proving nothing refers to it, and an RDF 1.2 triple term holds its components by
id — so a term can be referenced while *no quad mentions it at all*:

```text
<claim> <says> <<( <a> <p> "v" )>> .
```

`<a>` and `<p>` are interned IRIs appearing in no quad. Measured, not assumed: both come back
with zero direct references. A check that looked only at quads would free them and leave the
triple term pointing at nothing. Tombstoning is also only a partial reclaim — the id is
retired for ever and the slot remains — so the trade was *online but partial, with data
corruption as the failure mode*.

Copying has no such failure mode. It writes only terms it has just read, so anything
reachable arrives with its referents and anything unreachable is left behind by construction
rather than by analysis. It is offline, and it is complete.

`holos compact --store <DIR> --to <DIR>` writes a fresh store beside the old one, never over
it, so a failure leaves the original untouched. It reads the store **directly rather than
through a policy**: `holos dump` writes what a principal may see, and a maintenance operation
that silently dropped the quads the operator happens not to be cleared for would be a
data-loss bug wearing a security feature's clothes.

Three things it checks rather than reports, because a compaction that quietly lost data is
the worst outcome a maintenance command can have: the quad count in, the quad count copied,
and the quad count out must agree, and so must the named graph counts. Empty named graphs are
copied explicitly — the Graph Store Protocol can tell an empty graph from an absent one.

This is not RocksDB's own `compact_range`, which reclaims SST space from deleted keys and
cannot renumber a dictionary. The two are complementary; only this one shrinks the term
space.

### Reclaiming what the index keeps: `POST /maintenance/purge`

Tracking the dictionary rather than the store buys a refresh in a fraction of a millisecond,
and costs entries for geometries whose quads have been deleted. Nothing is wrong while they
sit there — a departed geometry fails to join and contributes no row — but the index grows
with everything ever interned, and **a restart does not clear it**, because it is rebuilt
from the dictionary. Reclaiming needs an explicit step.

The trap is that a purge cannot simply forget what it drops. Re-inserting a quad over an
already-interned literal interns nothing, so the literal count does not move and the
dictionary walk would never revisit it — the geometry would be back in the store and absent
from the index, which is a silently missing answer rather than a slow one. Verified rather
than assumed: a delete followed by a re-insert leaves the literal count exactly where it was.

So a purge converts an index entry into a **watchlist** entry: a bare term id instead of a
bounding box in a tree, checked on each subsequent refresh for whether it has been referenced
again, and skipped entirely unless the store has grown. That leaves the reclaim worthwhile —
a term id against a tree node — and costs one index probe per watchlist entry on refreshes
that follow a write. While the watchlist is non-empty, staleness falls back to comparing quad
counts, which is the heuristic this design otherwise avoids, confined to the one case that
needs it.

**No timer.** The endpoint exists so that whatever already schedules work on the host — cron,
a systemd timer, a Kubernetes CronJob — can call it, exactly as `deploy/backup.sh` calls
`/backup`. A server that schedules its own maintenance is a server that does something
surprising at three in the morning. The guard is the same three-way shape as `/backup`:
absent unless `--purge-role` is set, the principal must hold that role, and identity is
untrusted by default.

### The spatial index was gated on an unrelated flag

Found while testing the purge endpoint, and older than any of this work. `refresh_spatial()`
at startup sat inside `if config.reorder`, so a server started **without** `--reorder` — the
default — had no spatial index at all until its first write, and every GeoSPARQL query until
then did a full scan. Reordering and spatial routing are unrelated features that happened to
be refreshed in the same place.

Worth recording because of how it survived: every test that exercised routing supplied the
index directly through `QueryOptions`, and every server test that wrote to the store built
one as a side effect. The only configuration that showed it was the default one, queried
before anything was written.

### `geof:distance` only worked between two points

Found by re-running the OGC GeoSPARQL example dataset through a live server, query by query,
rather than through the test suite. `geof:distance(?point, ?polygon, uom:metre)` came back
with **no binding at all** — not an error, not a zero, an unbound variable that `ORDER BY`
then sorted to the top. Against that dataset it was every Polygon and every LineString in it:
five of the ten geometries.

The cause is upstream. `spargeo`'s implementation reads both operands as points and returns
`None` for anything else, and an extension function returning `None` is indistinguishable
from one whose arguments were the wrong type. GeoSPARQL defines the function for any two
geometries, as the shortest distance between a point of one and a point of the other.

It is replaced in `geo_ext`, registered after `spargeo`'s so it wins, in the same way the set
operations already are. Intersecting geometries are zero apart; otherwise the minimum is
taken over every vertex of each geometry against its closest point on the other, in both
directions. That is exact rather than a sample: for two straight segments that do not cross
the closest pair is always attained at an endpoint of one of them, and a polyline or polygon
boundary is a union of segments.

The closest pair is located in planar degrees and then measured with the same Haversine
formula `spargeo` uses, so **a point-to-point call returns exactly the number it returned
before** — asserted by a test, and confirmed against the four point-to-point distances the
example dataset produces, which are unchanged to the last digit.

### What is missing, and where it goes

These are **filter** functions. They evaluate over whatever bindings reach them, so
`geof:sfWithin(?point, ?region)` scans every candidate geometry rather than probing an
index. On a small dataset that is fine; on a national gazetteer it is not.

The missing piece is an **R-tree over geometry literals**. §5 reserved term tags `0x9`–`0xF`
for "geometry handles" so a geometry could carry an index handle in its id rather than needing
a side table, and routing was said to be blocked on the P2 optimiser.

> **The index is built, in memory.** `holos_engine::spatial::SpatialIndex` is an `rstar`
> R-tree over every geometry literal in a store, keyed by bounding box. `rstar` was already
> in the tree through `geo`, so it cost no new dependency.
>
> **Measured**, per §17's own instruction not to call anything fast without one. A point
> cloud over a 1000 × 1000 extent, probed with a 10 × 10 window — about 0.01% of it, the
> shape of a "what is near here" query:
>
> | Geometries | Build | Scan | Probe | Speed-up |
> |---:|---:|---:|---:|---:|
> | 10,000 | 24.9 ms | 17.3 ms | 35.1 µs | **492×** |
> | 50,000 | 99.1 ms | 75.0 ms | 20.3 µs | **3,692×** |
> | 200,000 | 550.0 ms | 371.6 ms | 135.7 µs | **2,738×** |
>
> `cargo run --release -p holos-bench --bin spatial`. The benchmark asserts that the scan and
> the probe find the *same* matches rather than reporting both and trusting the reader; a
> faster answer that is a different answer is not an optimisation.
>
> Three things the numbers do not say. **Build is a real cost** — 550 ms at 200,000 — paid
> once and not yet incremental, so an `INSERT` currently invalidates it. **The speed-up is
> not monotonic** because the probe times are near the clock's resolution and the match counts
> differ; the shape is what matters, not the third digit. And **candidates equal matches here
> only because points have degenerate bounding boxes** — over polygons the tree proposes more
> than it should and refinement discards the rest, which is the normal case.
>
> **Refinement is not optional.** A bounding box says a geometry *may* qualify, never that it
> does. The exact predicate still runs on every candidate; the index only decides which are
> worth testing.
>
> **Disjointness is deliberately not routed.** `sfDisjoint`, `ehDisjoint` and `rcc8dc` are
> true of nearly everything *outside* a probe, so bounding-box overlap is the wrong filter and
> would discard almost every correct answer. `spatial::can_filter` is that boundary, and it is
> about correctness rather than cost.
>
> Still to do: routing the [`topology`] rewrite through it when an operand is a constant
> region, incremental maintenance, and persisting handles in the reserved tag.

---

## Sources

- [Oxigraph](https://github.com/oxigraph/oxigraph) — crates, RocksDB column families, stated optimisation status
- [pwin/SHACL_Engine](https://github.com/pwin/SHACL_Engine) — interned terms, flat indexes, compiled IR, conformance and limitations
- [Apache Jena RDF-star / RDF 1.2](https://jena.apache.org/documentation/rdf-star/) and the [Jena 5.5.0 announcement](https://www.mail-archive.com/users@jena.apache.org/msg21156.html) — `StatementTerm`, object-position restriction, reifiers
- [Tentris](http://dice-research.org/Tentris/) · [Tentris – A Tensor-Based Triple Store (ISWC 2020)](https://papers.dice-research.org/2020/ISWC_Tentris/iswc2020_tentris_public.pdf) · [Hashing the Hypertrie (2022)](https://link.springer.com/chapter/10.1007/978-3-031-19433-7_4) · [Efficient Updates for Worst-Case Optimal Join Triple Stores (ISWC 2025)](https://papers.dice-research.org/2025/ISWC_Tentris-WCOJ-Update/public.pdf) · [research repo](https://github.com/dice-group/tentris)
- [W3C Holon Graph Community Group](https://www.w3.org/groups/cg/holon/) · [What Is a Holon, Part 1: The Graph](https://inferenceengineer.substack.com/p/what-is-a-holon-part-1-the-graph)
- [RDF 1.2 Concepts (CR, 7 Apr 2026)](https://www.w3.org/TR/rdf12-concepts/) · [RDF 1.2 Turtle](https://www.w3.org/TR/rdf12-turtle/) · [SPARQL 1.2 Query (WD, 20 Aug 2026)](https://www.w3.org/TR/sparql12-query/) · [SHACL 1.2 Core (WD, 3 Aug 2026)](https://www.w3.org/TR/shacl12-core/)
- [RocksDB](https://rocksdb.org/) · [User-defined timestamps](https://github.com/facebook/rocksdb/wiki/User-defined-Timestamp) · [Creating and ingesting SST files](https://github.com/facebook/rocksdb/wiki/Creating-and-Ingesting-SST-files)
- [Sparqloscope: a generic benchmark for SPARQL engines (ISWC 2025)](https://ad-publications.cs.uni-freiburg.de/ISWC_sparqloscope_BKTU_2025.pdf)
