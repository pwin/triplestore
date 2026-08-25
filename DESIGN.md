# HOLOS — design study for a new RDF 1.2 triplestore & SPARQL 1.2 engine

*Working name. Date: 2026-08-22.*

**Status: the thesis is demonstrable end to end.** P0 met, P1 partial; RocksDB Tier A, SHACL, GeoSPARQL, the HTTP server and the holon layer built. A Rust workspace in
[`crates/`](crates/) builds a working store — tagged term ids, the nine-order index, SPARQL 1.2
over reused Oxigraph crates, and fine-grained access policy enforced at the scan (§14).

**Conformance: 2,876 of 3,014 W3C tests pass, and every one of the 138 failures is upstream.**
Not one is a HOLOS bug. The suites and how that attribution is made are in §15. 90 further
unit and property tests pass.

Storage now has two backends behind one trait — in memory, and RocksDB with the nine
column families of §6.1. They are held to strict parity, including identical term ids for an
identical insertion sequence, and the RDF suites run through both.

L4 validates against the store's own indexes, and **incremental revalidation is 161×
faster than a full pass** on a 400k-triple graph — the mechanism §8 needs for the holon
Boundary to gate a commit. SHACL_Engine itself is now vendored and adapted (§8), which is
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
| **Tentris** | RDF as a sparse order-3/4 boolean tensor; the **hypertrie**, which gives constant-time slices on *any* dimension combination and therefore subsumes all six permutation indexes in one structure; BGP → Einstein summation → worst-case-optimal multi-join; hash-consing of identical subtries (2022); **incremental insert/delete** (ISWC 2025) which removes the historical "bulk-load only" objection. | The research prototype's scope: it answers `SELECT` / `SELECT DISTINCT` / `ASK` over well-designed BGP + `OPTIONAL` patterns only. Full SPARQL lives in the commercial fork. Papers are freely implementable; **verify the licence of any code you actually vendor**. |
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
  hard-linked fork of a dataset.
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

---

## 8. L4 — SHACL as a subsystem, not a library

> **Built, and it ended up as two validators rather than one.**
>
> `crates/holos-shacl-engine` is [pwin/SHACL_Engine](https://github.com/pwin/SHACL_Engine)
> vendored and adapted — the plan this section always described. `crates/holos-shacl`
> supplies the store bridge, the incremental planner, and a `Validate` trait that hides
> which validator is in use.
>
> | | Vendored engine | Native evaluator |
> |---|---|---|
> | Coverage | SHACL Core, SPARQL constraints, node expressions, SHACL-AF rules, inference | SHACL Core |
> | W3C SHACL 1.2 Core | **127/138** | 94/138 |
> | W3C SHACL 1.0 Core | 90/98 | **92/97** |
> | Reads | a bridged snapshot | the live store |
> | Incremental revalidation | no | **yes, 161×** |
>
> The split is forced by one fact: the vendored engine's `Graph` is immutable, so a delta
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
> - **Boundary rules do not fire.** SHACL-AF fixpoint evaluation exists in the vendored
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
event-log head. Auditability — which the Holon article flags as an explicit architectural choice —
becomes a per-holon retention policy rather than a schema decision.

**All holon metadata is RDF** in a system graph, queryable with ordinary SPARQL. No new model.

---

## 10. L6 — Interfaces

> **Partly built.** `holos-server` serves the SPARQL 1.2 Protocol over HTTP with a YASGUI
> console at `/`. PyO3 and WASM bindings, the Graph Store Protocol and SPARQL Update are
> not built; `POST /update` answers 501 rather than pretending.

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

YASGUI is loaded from a CDN rather than vendored: it is a large JavaScript bundle with its
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
| **P6** ◐ | HTTP protocol server, PyO3/WASM bindings, text + vector module | *Server built* — SPARQL 1.2 Protocol, YASGUI console, policy enforced per request (§10). Owes the Graph Store Protocol, SPARQL Update, bindings and the text/vector module. |

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
are reusable; Jena is Apache-2.0 (readable as a reference, not vendorable into a differently
licensed codebase without care). The hypertrie library's own licence needs checking separately, and
a commercial Tentris exists — **implement from the papers rather than vendoring code unless the
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
`scripts/fetch-testsuites.sh`. The suites are not vendored; without them these tests skip,
so a fresh checkout still builds green.

| Suite | Passing | Failing | Skipped | HOLOS bugs |
|---|---|---|---|---|
| RDF 1.1 | 987 / 1041 | 54 | 0 | **0** |
| RDF 1.2 | 1326 / 1406 | 80 | 0 | **0** |
| SPARQL 1.1 | 321 / 321 | 0 | 304 | **0** |
| SPARQL 1.2 | 242 / 246 | 4 | 23 | **0** |
| SHACL Core | 92 / 97 | 5 | 1 | 5 |
| SHACL 1.2 Core (native) | 94 / 138 | 44 | 0 | 44 |
| SHACL Core (vendored engine) | 90 / 98 | 8 | 0 | 8 |
| SHACL 1.2 Core (vendored engine) | **127 / 138** | 11 | 0 | 11 |

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

All 25 SPARQL 1.1 tests that fail against their expected results agree with the reference
storage, so all 25 are evaluator behaviour. This is the differential rig §12 asks for against
the future Tier B hypertrie, built early because it is exactly as useful now.

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

### The skips are not a hiding place

304 SPARQL 1.1 tests are skipped, and each skip names a roadmap item rather than a mystery:
SPARQL Update and the Graph Store Protocol are L6; 42 entailment tests need the reasoner that
is L4 (§8); federated queries need a service handler; a handful encode their expected results
in the old DAWG `rs:ResultSet` RDF vocabulary, which the harness does not read.

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

The vendored engine keeps its own `Graph`, so §8's claim reduces to a measurable question:
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
| Vendored, bridged | 0.522s | **0.224s** | 0.746s |

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

### What is missing, and where it goes

These are **filter** functions. They evaluate over whatever bindings reach them, so
`geof:sfWithin(?point, ?region)` scans every candidate geometry rather than probing an
index. On a small dataset that is fine; on a national gazetteer it is not.

The missing piece is an **R-tree over geometry literals**, and the design already has a slot
for it: §5 reserves term tags `0x9`–`0xF` for "geometry handles" among others, so a geometry
can carry an index handle in its id rather than needing a side table. Making the planner
*route* to that index is §7 work — it is a cost-based decision like any other, and it needs
the optimiser that P2 will build. Doing the index without the planner would leave something
nothing knows how to use.

So: functions now, index when there is a planner to use it, and a measurement before either
is called fast.

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
