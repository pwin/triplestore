# HOLOS — what it is for, and where things are

The plan in one page, then the map. `DESIGN.md` is the full argument — a hundred kilobytes,
with a section number for every decision — and this exists so that you know which of its
sections to open. Every pointer is a path in this repository.

## Why this exists

**Not to be a faster triplestore.** Fast ones exist — QLever, Virtuoso, Tentris, RDFox — and
rebuilding one is a race already lost. What did not exist as one system, when this began in
August 2026, is the combination of four things that each exist alone:

1. **Worst-case-optimal join execution** — Tentris' contribution: joins whose cost is bounded
   by the size of the answer, not by the intermediate results a pairwise plan produces.
2. **A durable, transactional, RDF 1.2-native store** — what Oxigraph and Jena TDB2 are:
   triple terms as first-class values, atomic commits, checkpoints.
3. **SHACL on the write path** — validation as something the store *does to a commit*, not a
   library you run over a file afterwards. That is [SHACL_Engine](https://github.com/pwin/SHACL_Engine),
   generalised, and it is only affordable if revalidation is incremental.
4. **Versioned, shape-governed graph partitions with maintained views** — what the
   [Holon](https://www.w3.org/groups/cg/holon/) model asks for, in database primitives: a
   named graph that is a *scene*, a set of shapes that is its *boundary*, an event log that
   is its history, and a branch that is a checkpoint.

The combination is the thesis; everything else is subordinate to it. `DESIGN.md` §1 puts the
rule bluntly: if a decision does not serve one of those four, take the boring option.

**What that rule decided.** The boring parts are reused, not rebuilt — parsing, the SPARQL
algebra and the evaluator are Oxigraph's crates, and the W3C suites hold them to it. The
storage is owned, because the four need things off-the-shelf storage does not give: term ids
that are stable across the life of a store and *order-preserving* for inline values, so that
a range filter is an index bound and a join is a merge; an index with exactly one route in,
so that policy, a boundary's shapes and a holon's event log can all sit on the same
chokepoint at a cost of two relaxed loads per quad. That chokepoint — **access policy enforced
at the scan**, `DESIGN.md` §14 — is the one commitment a reader will notice everywhere,
because it is the reason the code is shaped the way it is.

Storage is planned in two tiers. **Tier A** is RocksDB, the system of record, and is built.
**Tier B** is a hypertrie held in memory for the worst-case-optimal joins of point 1, strictly
derived from Tier A and disposable; it is not built, and is deliberately gated on having a
good conventional planner to compare it against first (`DESIGN.md` §6.2, §13).

## The plan, and where it stands

`DESIGN.md` §11 is the roadmap, and its rule is that **each phase ends in a measurement, and
nothing proceeds without one**. The state as of this document:

| phase | what | state |
|---|---|---|
| P0 | Reused Oxigraph crates, the store, the evaluator, the W3C suites | ✅ done; every remaining suite failure is upstream |
| P1 | Term dictionary, order-preserving encodings, SST-ingest bulk loading | ✅ done; the loader's memory is flat in the load's size and it no longer asks the disk whether a term is new |
| P2 | Characteristic-set statistics, cost-based planning | ◐ statistics built and consumed; the bind join plans over them for its fragment; the rest of the language still goes to the reused planner |
| P3 | Hypertrie tier and worst-case-optimal joins | not started — gated on P2 |
| P4 | SHACL on native indexes, incremental revalidation | ✅ done; 236/236 W3C, a delta revalidates at 161× a full pass |
| P5 | Holons: partitions, boundary, event log, projections, time travel | ◐ walking skeleton; a tick is one atomic commit; owes isolation (needs MVCC), maintained projections, time travel |
| P6 | HTTP server, Python bindings, text and vector | ◐ server and bindings done; text/vector not started |

P0–P2 is a conventional engine. P3–P5 is the research. The work is arranged so that
abandoning P3 or P5 still leaves a usable store — which `DESIGN.md` §12 names as the main
insurance against the risks it lists, the first of which is *two index tiers, two update
paths*: the likeliest source of silent corruption, and the reason Tier B must be disposable
and every backend is held to differential tests against every other.

Memory has become a phase of its own, unplanned: 0.7.0 through 0.10.0 each bounded something
that had been growing with the data — the loader's term cache, a query's `DISTINCT`, a bulk
merge's bloom filter, the statistics build, the dictionary's own lookups. `DESIGN.md` §16a
records the pattern, and the changelog records each measurement.

## The shape

```
                  holos-cli        holos-server        holos-python
                  (command line)   (HTTP: SPARQL Protocol, GSP, console)
                        \               |                /
                         \              |               /
   L5  holos-holon        \        holos-engine  L3   /         holos-shacl  L4
       scenes, ticks,      ------  SPARQL query &  ------       validation, two
       event log, branch           update, policy view          engines behind one trait
                                        |
                     holos-security ----+---- holos-stats
                     principals, policy,      characteristic sets,
                     audit                    cardinality estimates
                                        |
                              holos-store  L2
                              one Store, two backends:
                              memory, RocksDB (13 column families)
                                        |
                              holos-core   L1
                              TermId: tagged 64-bit ids, inline codec, vocab
                                        |
                              oxrdf / oxttl / spargebra / spareval   L0
                              (reused from Oxigraph: terms, parsers, algebra, evaluator)
```

Two crates sit beside the stack rather than in it: `holos-conformance` runs the W3C suites
against everything above, and `holos-bench` holds every measurement the design cites.

## Two journeys

### A triple's life: from a file to the index

1. **Parse.** `oxrdfio` turns bytes into `oxrdf::Quad`s. Compressed and multi-format input is
   opened by [`crates/holos-engine/src/source.rs`](crates/holos-engine/src/source.rs).
2. **Encode each term to a `TermId`.** [`crates/holos-core/src/term_id.rs`](crates/holos-core/src/term_id.rs):
   a 4-bit tag and 60 bits of payload. Integers, floats, `xsd:dateTime`, booleans and short
   strings are encoded *inline* and order-preserving —
   [`crates/holos-core/src/inline.rs`](crates/holos-core/src/inline.rs) — so a range filter
   can become an index bound. `rdf:type` and friends are a static table,
   [`crates/holos-core/src/vocab.rs`](crates/holos-core/src/vocab.rs). Everything else goes
   through the dictionary.
3. **Intern in the dictionary.** [`crates/holos-store/src/dictionary.rs`](crates/holos-store/src/dictionary.rs)
   in memory; on RocksDB the `str2id` / `id2str` families in
   [`crates/holos-store/src/rocks/mod.rs`](crates/holos-store/src/rocks/mod.rs). During a bulk
   load a term cache and an in-memory bloom filter
   ([`rocks/seen.rs`](crates/holos-store/src/rocks/seen.rs)) decide whether a term is new
   without touching the disk.
4. **Index the quad in every order.** [`crates/holos-store/src/index.rs`](crates/holos-store/src/index.rs)
   defines the nine orders — three for the default graph (`spo`, `pos`, `osp`) and six for
   named graphs. A single write puts the key in all of them in one `WriteBatch`; a bulk load
   instead sorts and spills runs ([`rocks/sort.rs`](crates/holos-store/src/rocks/sort.rs),
   [`rocks/dictsort.rs`](crates/holos-store/src/rocks/dictsort.rs)), merges them at the end,
   and hands RocksDB one finished file per order. A bulk load is four threads: the parser
   (`load_parsed` in [`crates/holos-engine/src/lib.rs`](crates/holos-engine/src/lib.rs)),
   the loading thread that interns and issues ids in file order, a `SpillWorker` that sorts
   and writes the index runs, and a thread per dictionary window that writes its files
   (`DictFlush`, both in [`rocks/mod.rs`](crates/holos-store/src/rocks/mod.rs)). Only the
   loading thread touches the dictionary, which is what keeps ids identical across backends.

The entry points: `Store::insert` / `Store::remove` in
[`crates/holos-store/src/lib.rs`](crates/holos-store/src/lib.rs) for one quad;
`Engine::bulk_load` in [`crates/holos-engine/src/lib.rs`](crates/holos-engine/src/lib.rs)
for a file; `Store::begin` / `commit` / `rollback` for a transaction around several.

### A query's life: from text to rows

1. **Parse** with `spargebra` into algebra. Some queries the parser accepts and the
   specifications do not; [`crates/holos-engine/src/validate.rs`](crates/holos-engine/src/validate.rs)
   refuses those.
2. **Decide whether to run it at all.** [`crates/holos-engine/src/admit.rs`](crates/holos-engine/src/admit.rs)
   estimates what a blocking operator (`ORDER BY`, `DISTINCT`, keyed `GROUP BY`) would have
   to buffer, using [`crates/holos-stats`](crates/holos-stats/src/characteristic.rs), and
   refuses one over budget — unless it is a `DISTINCT`, which
   [`spill.rs`](crates/holos-engine/src/spill.rs) can answer by sorting to disk.
3. **Rewrite.** GeoSPARQL topology predicates become geometry lookups plus a filter
   ([`topology.rs`](crates/holos-engine/src/topology.rs)); a basic graph pattern is reordered
   by estimated cardinality ([`holos-stats/src/reorder.rs`](crates/holos-stats/src/reorder.rs)).
4. **Try the fast path.** [`bindjoin.rs`](crates/holos-engine/src/bindjoin.rs) is an index
   nested-loop join for the fragment it accepts, with range filters pushed into the scan by
   [`range.rs`](crates/holos-engine/src/range.rs). Anything outside the fragment falls through
   to the reused evaluator, `spareval`.
5. **Every read goes through the view.** [`crates/holos-engine/src/view.rs`](crates/holos-engine/src/view.rs)
   is `DatasetView`, and `internal_quads_for_pattern` there is the one route to the indexes.
   It calls `decide` on each quad — the policy in
   [`crates/holos-security/src/policy.rs`](crates/holos-security/src/policy.rs) — and reads the
   memory ceiling from [`memory.rs`](crates/holos-engine/src/memory.rs). Nothing bypasses this,
   including the fast path.
6. **Decode and serialise.** Ids become terms through the dictionary; `sparesults` writes
   JSON, XML, CSV or TSV.

The entry point is `Engine::query_with` in
[`crates/holos-engine/src/lib.rs`](crates/holos-engine/src/lib.rs); updates go through
[`update.rs`](crates/holos-engine/src/update.rs) and are one transaction each.

## The crates

| crate | layer | what it holds | start reading at |
|---|---|---|---|
| `holos-core` | L1 | `TermId`, the inline value codec, the well-known vocabulary | [`term_id.rs`](crates/holos-core/src/term_id.rs) |
| `holos-store` | L2 | The `Store` API, the `Storage` trait, both backends, the index orders, bulk loading | [`storage.rs`](crates/holos-store/src/storage.rs), then [`rocks/mod.rs`](crates/holos-store/src/rocks/mod.rs) |
| `holos-security` | — | Principals, the compiled access policy, the audit record | [`policy.rs`](crates/holos-security/src/policy.rs) |
| `holos-stats` | L3 | Characteristic sets, HyperLogLog sketches, cardinality estimation, pattern reordering; snapshots kept with the store | [`characteristic.rs`](crates/holos-stats/src/characteristic.rs) |
| `holos-engine` | L3 | Query and update evaluation; the policy view; admission control; spilling; the bind join; GeoSPARQL and its CRS transforms; the spatial index; entailment | [`lib.rs`](crates/holos-engine/src/lib.rs), then [`view.rs`](crates/holos-engine/src/view.rs) |
| `holos-shacl` | L4 | Validation over the store's own indexes: a native validator and a bridge to the adapted engine | [`lib.rs`](crates/holos-shacl/src/lib.rs) |
| `holos-shacl-engine` | L4 | The adapted [SHACL_Engine](https://github.com/pwin/SHACL_Engine): full SHACL, node expressions, rules | [`lib.rs`](crates/holos-shacl-engine/src/lib.rs) |
| `holos-holon` | L5 | Holons: a scene, a boundary enforced on write, an event log, branching via checkpoints | [`lib.rs`](crates/holos-holon/src/lib.rs), and `HOLONS.md` |
| `holos-server` | L6 | SPARQL 1.2 Protocol and Graph Store Protocol over HTTP, the YASGUI console, the counting allocator | [`main.rs`](crates/holos-server/src/main.rs) |
| `holos-cli` | — | `holos`: load, query, update, stats, backup, compact, validate, entail | [`main.rs`](crates/holos-cli/src/main.rs) |
| `holos-python` | — | The `holosdb` package on PyPI | [`lib.rs`](crates/holos-python/src/lib.rs) |
| `holos-conformance` | — | The W3C RDF 1.2, SPARQL 1.2, protocol and SHACL suites, as tests | [`lib.rs`](crates/holos-conformance/src/lib.rs) |
| `holos-bench` | — | One binary per question the design needed answered | [`src/bin/`](crates/holos-bench/src/bin/) |

## Four invariants worth knowing before you change anything

**Policy is enforced at the scan, nowhere else.** `DatasetView::internal_quads_for_pattern`
is the only route to the indexes and `decide` runs on every quad it yields. A new way to
read the store that does not go through it is a policy bypass. `DESIGN.md` §14.

**Term ids are stable and order-preserving.** An id, once issued, is never reissued or
changed; inline ids sort as their values do. Both backends must issue the *same* ids for the
same insertion sequence — [`crates/holos-store/tests/backend_parity.rs`](crates/holos-store/tests/backend_parity.rs)
holds them to it. `DESIGN.md` §5.

**A commit is one `WriteBatch`.** Every index-order key of every quad in an update lands
atomically or not at all; a crash sits before or after, never between. The scope machinery in
[`rocks/mod.rs`](crates/holos-store/src/rocks/mod.rs) buffers and overlays so reads inside a
scope see its own writes. The store's `generation` counter moves with every such change.

**Memory is bounded by construction, not by hope.** Bulk loads spill; `DISTINCT` spills;
doomed queries are refused from their estimate; a ceiling is read at the scan. What the
ceiling cannot see — RocksDB's own allocations — is documented in `DESIGN.md` §16a and was
the source of the last two large bugs.

## Where the evidence is

Numbers in the documentation are measured, and the measurement is checked in:

- **`BENCHMARKS.md`** — the standing profile, regenerated by `cargo run --release -p holos-bench`.
- **`CHANGELOG.md`** — per release, what changed and the measurement that justified it, including
  the ones that failed.
- **[`crates/holos-bench/src/bin/`](crates/holos-bench/src/bin/)** — each file's header states
  the question it answers. `loadprofile` for where a load spends its time; `sstmem` and
  `filterread` for the bulk writer's memory and the read cost of partitioned filters;
  `rangequery` and `pushdown` for whether range filters reach the index; `scanrate` for the
  raw floor.
- **Tests** — differential where possible: memory against RocksDB, spilled against not,
  optimised against plain. A fix here usually arrives with a test that fails when it is
  reverted; the commit message says which.

## Other documents

| document | read it when |
|---|---|
| `README.md` | you want the feature table and the current W3C pass counts |
| `DESIGN.md` | you want to know *why*, or you are about to change a design decision |
| `OPERATIONS.md` | you are running it: flags, sizing, disk, the things that bite |
| `ACCESS-CONTROL.md` | you are writing a policy or wiring identity through a front door |
| `SPARQL-SURFACE.md` | you want to know which SPARQL, GeoSPARQL and extension functions are supported |
| `HOLONS.md` | you are using the L5 layer |
| `MINTING-TRIPLES.md` | you are minting RDF 1.2 triple terms and provenance |
| `docs/holos-manual.html` | you want the user manual as one page |

## If you want to…

| …then look at |
|---|
| **add a SPARQL extension function** → [`holos-engine/src/functions.rs`](crates/holos-engine/src/functions.rs), and `SPARQL-SURFACE.md` |
| **add a storage backend** → implement [`Storage`](crates/holos-store/src/storage.rs), then run `backend_parity.rs` against it |
| **change how a term is encoded** → [`holos-core/src/inline.rs`](crates/holos-core/src/inline.rs); it changes the on-disk format, so read `DESIGN.md` §5 first |
| **add a policy rule** → [`holos-security/src/policy.rs`](crates/holos-security/src/policy.rs); it compiles to a per-quad decision, so measure with `scanrate` |
| **make a query shape faster** → [`bindjoin.rs`](crates/holos-engine/src/bindjoin.rs) if it is a join, [`range.rs`](crates/holos-engine/src/range.rs) if it is a filter, [`holos-stats`](crates/holos-stats/src/) if the plan is wrong |
| **make loading faster** → [`rocks/mod.rs`](crates/holos-store/src/rocks/mod.rs) around `encode_into` and `flush_dictionary`; `loadprofile` tells you which phase you moved |
| **add an HTTP endpoint** → [`holos-server/src/main.rs`](crates/holos-server/src/main.rs), the `match` on method and path in `dispatch` |
| **add a CLI command** → [`holos-cli/src/main.rs`](crates/holos-cli/src/main.rs), the `match` in `main` and the `USAGE` text beside it |
