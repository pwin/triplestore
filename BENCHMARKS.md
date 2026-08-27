# Benchmarks

Load and query timings across dataset sizes, including property paths and the holonic
queries that have no equivalent in an ordinary triplestore.

```sh
cargo run --release -p holos-bench                     # the three default scales
cargo run --release -p holos-bench -- 100000 2000000   # your own
cargo run --release -p holos-bench -- --queries-only 1000000
```

Everything below is reproducible with that command. The harness lives in
[crates/holos-bench](crates/holos-bench) and prints these tables directly.

---

## What is being measured, and how

**Loads** are timed once, from a cold empty store, **including parsing** an N-Triples file
from disk. Including the parse is deliberate — it is what a real load does, and a figure
that excluded it would be one nobody could reproduce against their own data.

**Queries** run in a warm process against an already-open store. Each is executed once to
warm caches, then timed three times, and the **median** is reported. Median rather than
mean because one scheduler hiccup should not decide the number; rather than minimum because
the best of three is not what anyone actually experiences.

**Every query is fully consumed** and its row count checked. The result iterators are lazy,
so a query that is never drained has not been run — timing one would measure nothing.

**Queries are timed against the in-memory backend.** That isolates query cost from RocksDB's
block cache warming up, which would otherwise dominate the first runs and make the numbers
depend on how recently the store was opened rather than on the query.

### The dataset, and why it is shaped this way

A benchmark dataset is an argument about what matters. This one is built around three
properties:

**A real holarchy.** Units nest inside units, five levels deep, four-way branching — 341
units in total. This is what makes property paths meaningful rather than decorative: a
`ex:partOf+` walking a genuine tree is the query a holon model actually asks, because *a
holon is a whole that is also a part*. On a flat dataset every path query is one hop and
proves nothing.

**Skew.** Predicate frequencies span three orders of magnitude, and the `knows` graph is
deliberately uneven: one person in a thousand knows 200 others, one in fifty knows 20, the
rest know 3. Uniform data flatters a cardinality estimator. Real RDF does not, and finding
where planning hurts is the point.

**Something worth validating.** Every person carries the properties a SHACL boundary can
check, so the holon measurements validate real constraints rather than an empty shape that
always passes.

Scales are given in **people**; each contributes roughly 7.5 quads once the holarchy and
the `knows` edges are counted. The tables print both numbers.

### Comparability across scales

A person's own properties — name, age, unit membership, badge — are derived from their index
by a fixed hash, so person 7 is identical whether the run generated ten people or ten
million. Timings at different scales therefore ask the same question of the same entity.

**Edges are the exception, necessarily.** A `knows` edge must point at somebody who exists,
so its target depends on the population. A person's *neighbourhood* is not stable across
scales — only its **size** is, which is what the path timings depend on. That is the honest
limit of the comparison.

**The anchor is a hub.** Every query that starts from a named person starts from
`person1000`, which is in the top degree class — 200 outbound `knows` edges. That is
deliberate: anchoring on a typical person (3 edges) would make the path timings look far
better than the shape deserves, and the interesting question is what a transitive closure
costs when it starts somewhere well connected.

### What is deliberately not claimed

One machine, one synthetic dataset, one process. This is a **profile of where this store
spends its time** — useful for comparing query shapes against each other, and sizes against
each other. It is **not** a comparison with any other store, and the absolute numbers will
not survive different hardware.

---

## Results

Run on **windows / 8 logical cores**, release build.
Regenerate with `cargo run --release -p holos-bench`.

## Load timings

| People | Quads | Backend | Time | Rate | On disk | Dictionary |
|---:|---:|---|---:|---:|---:|---:|
| 100,000 | 753,218 | in memory | 3.87s | 194,384 quads/s | — | 200,881 |
| 100,000 | 753,218 | rocksdb, --bulk | 19.82s | 37,996 quads/s | 33 MB | 200,881 |
| 100,000 | 753,218 | rocksdb, no --bulk | 65.02s | 11,583 quads/s | 32 MB | 200,881 |
| 500,000 | 3,762,016 | in memory | 31.66s | 118,826 quads/s | — | 1,001,681 |
| 500,000 | 3,762,016 | rocksdb, --bulk | 177.71s | 21,168 quads/s | 171 MB | 1,001,681 |
| 500,000 | 3,762,016 | rocksdb, no --bulk | 435.57s | 8,637 quads/s | 171 MB | 1,001,681 |
| 1,000,000 | 7,523,015 | in memory | 66.52s | 113,090 quads/s | — | 2,002,681 |
| 1,000,000 | 7,523,015 | rocksdb, --bulk | 206.34s | 36,459 quads/s | 328 MB | 2,002,681 |
| 1,000,000 | 7,523,015 | rocksdb, no --bulk | 745.83s | 10,086 quads/s | 284 MB | 2,002,681 |

## Query timings

Median of three runs, in milliseconds, against the in-memory store.

| Query | Group | Rows | 100,000 | 500,000 | 1,000,000 |
|---|---|---:|---:|---:|---:|
| point lookup | access | 1 | 0.14 | 0.05 | 0.08 |
| subject fan-out | access | 205 | 0.35 | 0.58 | 0.25 |
| rare predicate scan | access | 200 | 0.37 | 1.8 | 3.8 |
| common predicate count | access | 1 | 36.3 | 176 | 368 |
| object lookup | access | 412 | 0.61 | 1.8 | 6.1 |
| 3-way star | join | 100 | 385 | 2321 | 5235 |
| selective join, written well | join | 20 | 43.6 | 197 | 405 |
| selective join, written badly | join | 20 | 357 | 2431 | 5399 |
| 2-hop, anchored | join | 50 | 136 | 702 | 1347 |
| 2-hop, unanchored | join | 20 | 902 | 6050 | 14141 |
| OPTIONAL | join | 100 | 0.26 | 1.1 | 1.8 |
| FILTER NOT EXISTS | join | 20 | 0.13 | 0.30 | 0.14 |
| path: one-or-more up | property path | 4 | 0.07 | 0.11 | 0.13 |
| path: zero-or-more up | property path | 5 | 0.14 | 0.14 | 0.22 |
| path: inverse closure down | property path | 340 | 0.82 | 0.67 | 0.70 |
| path: sequence + closure | property path | 5 | 2012 | 19667 | 25208 |
| path: alternation | property path | 201 | 0.22 | 0.20 | 0.54 |
| path: bounded social closure | property path | 500 | 261 | 6456 | 3612 |
| path: negated set | property path | 5 | 0.09 | 0.08 | 0.13 |
| path: count descendants | property path | 1 | 0.55 | 0.68 | 0.69 |

### What each query isolates

- **point lookup** (access) — one subject, one predicate: the spo index and nothing else
- **subject fan-out** (access) — every triple about one subject — a star with no join
- **rare predicate scan** (access) — a predicate on 1 subject in 500: the pos index, and the pattern a good planner should start from
- **common predicate count** (access) — an aggregate over every person: a full predicate scan
- **object lookup** (access) — bound object, unbound subject: the osp index
- **3-way star** (join) — three predicates on one subject variable — the shape RDF queries mostly are, and what characteristic sets estimate exactly
- **selective join, written well** (join) — the rare predicate first, so any plan is a good plan
- **selective join, written badly** (join) — the same answers with the rare predicate last. The gap between this row and the one above is the cost of having no cost-based planner
- **2-hop, anchored** (join) — friends-of-a-friend from one known person: bounded work
- **2-hop, unanchored** (join) — the same shape with no starting point — this is the one that hurts
- **OPTIONAL** (join) — left join against a sparse predicate
- **FILTER NOT EXISTS** (join) — negation — and the operator most likely to leak, which is why §14 enforces policy below it rather than beside it
- **path: one-or-more up** (property path) — ex:partOf+ — every ancestor of a leaf unit. The holarchy walk: a holon is a part, and this asks which wholes it belongs to
- **path: zero-or-more up** (property path) — ex:partOf* — the same, including the unit itself. The difference between * and + is whether a whole counts as its own part
- **path: inverse closure down** (property path) — ^ex:partOf+ — every unit beneath the root. Inverse plus transitive, which is the expensive direction: it fans out instead of climbing
- **path: sequence + closure** (property path) — ex:memberOf/ex:partOf* — from a person to every unit they are transitively in. The query a holonic membership question actually is
- **path: alternation** (property path) — ex:knows|ex:memberOf — a union of two very differently sized predicates, so the cost is dominated by the larger
- **path: bounded social closure** (property path) — ex:knows+ from one person. Unbounded transitive closure over a heavy-tailed graph — the worst case in this battery, and LIMIT is the only thing keeping it finite
- **path: negated set** (property path) — !(ex:knows) — everything about a subject except one predicate
- **path: count descendants** (property path) — an aggregate over a transitive path — the shape a rollup report takes

## Holon timings

A scene of 60,000 quads, a four-constraint boundary, 200 commits.

| | Time |
|---|---:|
| Full validation of the scene | 140 ms |
| **One accepted commit** | **0.91 ms** |
| One rejected commit | 1.1 ms |
| **Commit vs full pass** | **155× cheaper** |
| Commits per second | 1103 |

### Holonic queries

| Query | Rows | Time |
|---|---:|---:|
| holon: registry lookup | 1 | 0.08 ms |
| holon: scene size | 1 | 18.7 ms |
| holon: tick history | 20 | 1.3 ms |
| holon: what changed in a tick | 3 | 0.60 ms |
| holon: provenance of one statement | 1 | 1.4 ms |
| holon: rejected commits | 1 | 0.21 ms |
| holon: change volume per tick | 10 | 1.6 ms |
| holon: scene joined to log | 20 | 24.3 ms |

#### What each holonic query isolates

- **holon: registry lookup** — find a holon's three graphs from its IRI
- **holon: scene size** — how big is the graph this holon is responsible for
- **holon: tick history** — every commit, newest first, with who made it — PROV over the event log
- **holon: what changed in a tick** — the triple terms a tick added or removed. `rdf:reifies` pointing at a triple term is RDF 1.2 doing in two triples what RDF 1.1 needed four for — and gave no defined meaning to
- **holon: provenance of one statement** — which tick asserted this exact triple, and who was responsible. The question a per-statement audit asks, answered without a side table
- **holon: rejected commits** — ticks the boundary refused, and how many violations each had. A boundary that rejects is only useful if the refusal is queryable
- **holon: change volume per tick** — an aggregate over the log — how much each commit actually moved
- **holon: scene joined to log** — current state joined to its own history, across two named graphs — the query that would need two databases without this model

---

## Reading the results

Four things in these tables matter more than the rest. Two of them were not what this
document predicted before the numbers came in.

### 1. Storage is linear and healthy

**Bytes per quad does not move with scale**: 33 MB for 753k quads, 328 MB for 7.5M — about
**44 bytes per quad** at both ends. Three index copies of every quad plus LZ4, over dense
64-bit ids, is what keeps that flat.

**Scans are linear too.** `common predicate count` walks every `ex:name` triple and costs
36 ms → 176 ms → 368 ms across a 10× growth in data. That is the storage layer behaving
exactly as it should, and it is the baseline every other row should be judged against.

The dictionary grows at almost exactly **two terms per person** — the person's IRI and their
name literal. Age, membership and badge do not appear, because integers and short strings
are inlined into their 64-bit ids and never reach the dictionary at all.

**`--bulk` is worth more than previously measured**: 3.3–3.6× rather than the 2.4× recorded
at one million quads in `DESIGN.md` §16. The advantage grows with the dataset, because the
write-ahead log it skips grows with it too.

### 2. The planner is the ceiling, and the gap widens with scale

| | 100k | 500k | 1M |
|---|---:|---:|---:|
| selective join, **written well** | 43.6 ms | 197 ms | 405 ms |
| selective join, **written badly** | 357 ms | 2,431 ms | 5,399 ms |
| **penalty** | **8×** | **12×** | **13×** |

Same answers, same data, same `LIMIT 20`. The only difference is whether the rare predicate
(200 rows) appears before or after the common ones (750,000 rows). The reused optimiser
estimates cardinality from a **fixed lookup table**, so it cannot tell those apart and cannot
reorder to fix a bad ordering.

**The penalty grows with the data** — 8× to 13× across a 10× increase. That is why planning
rather than storage is the binding constraint on scale: a well-written query stays linear,
and a badly written one degrades faster than the dataset grows.

`2-hop, unanchored` is the same story at its worst: **14 seconds to return 20 rows**, against
1.3 s for the anchored form of the same shape.

### 3. Property paths are cheap — until the closure's left side is a variable

This is the finding that contradicts what this section originally predicted. The expectation
was that inverse closures would be expensive because they fan out. They are not:

| | Rows | 1M |
|---|---:|---:|
| `<unit340> ex:partOf+ ?a` — climb to the root | 4 | **0.13 ms** |
| `<unit340> ex:partOf* ?a` — same, plus itself | 5 | **0.22 ms** |
| `<unit0> ^ex:partOf+ ?d` — **every** unit below the root | 340 | **0.70 ms** |
| `<person> ex:memberOf/ex:partOf* ?u` | **5** | **25,208 ms** |

The inverse closure returns **340 rows in 0.7 ms**. The sequence returns **5 rows in 25
seconds** — 36,000× slower for 68× fewer rows, using the same `partOf*` operator over the
same 341-node holarchy.

The difference is what the closure's left side is bound to. With a constant, evaluation
starts there and climbs four edges. With a **variable** — whether written as a sequence
`memberOf/partOf*` or as two separate patterns, which measure the same — a zero-length path
makes *every term in the dataset* a candidate for the left side, and the cost becomes
proportional to the dataset rather than to the holarchy. The timings confirm it: 2.0 s →
19.7 s → 25.2 s tracks the **dataset** size, not the 341 units actually being walked.

`ex:partOf+` in the same position costs roughly a tenth of `ex:partOf*`, because requiring at
least one edge lets evaluation start from the 341-entry `partOf` index instead of from every
term.

> **Practical consequence.** Anchor a closure to a constant where you can. Where you cannot,
> prefer `+` to `*` and add the zero-length case as a `UNION` branch that **repeats the
> anchor**:
>
> ```sparql
> { <:p1000> ex:memberOf ?leaf . ?leaf ex:partOf+ ?u }
> UNION
> { <:p1000> ex:memberOf ?u }
> ```
>
> Measured **1.7× faster** at 750k quads and **1.5×** at 7.5M, for identical answers.
>
> Repeating the anchor is not optional. Writing the second branch as `{ BIND(?leaf AS ?u) }`
> looks equivalent and is not — a `UNION` branch does not see `?leaf` from outside the group,
> so the `BIND` yields nothing and the row for the leaf itself is silently dropped. That form
> returned **4 rows where the path returns 5**, which is exactly the kind of rewrite that
> looks like an optimisation and is a bug.

This is a known-hard case in SPARQL property-path evaluation rather than a defect unique to
this store, and it lives in the reused evaluator rather than in anything here. It is recorded
because it is the single largest surprise in the battery, and because a planner that knew
the holarchy was 341 nodes would route around it.

### 3b. Reordering the pattern removes the penalty entirely

The estimator was measured at a mean q-error of **1.1** against the reused optimiser's
**2×10⁸**, and the penalty for a badly ordered query at **13×**. Neither establishes that
applying the first to the second helps — `sparopt` runs its own optimiser over whatever it
is given and could have undone the work.

It does not. `cargo run --release -p holos-bench --bin reorder`, at 7.5M quads:

| Query | As written | Reordered | |
|---|---:|---:|---|
| selective join, **written badly** | 4,560.6 ms | **320.6 ms** | **14.2× faster** |
| selective join, written well | 321.8 ms | 331.5 ms | unchanged |
| 4-way star, worst order | 8,004.4 ms | **647.0 ms** | **12.4× faster** |
| type + rare predicate | 4,663.2 ms | 1,773.1 ms | 2.6× faster |
| 3-way star, no rare arm | 4,850.4 ms | 4,639.2 ms | — |
| 2-hop, unanchored | 11,826.7 ms | 9,332.8 ms | 1.3× faster |
| **total** | **34,227 ms** | **17,044 ms** | **2.0×** |

The row that matters is the first two together. The badly written query now costs **320.6 ms
against the well-written one's 321.8 ms** — the same, to within noise. The penalty is not
reduced, it is **gone**: query cost no longer depends on how the query was typed.

Where there was nothing to fix — a star with no selective arm, an already-good order — the
change is nil, and the cost of trying is the reordering itself, about 3 ms.

**How it works, and why it is not a planner.** `sparopt` has no injection point, so the
statistics are applied by rewriting the algebra *before* the query reaches the evaluator. It
chooses join order and nothing else — not join algorithms, not access paths — and it can only
work because written order survives into the plan, which the earlier 3× experiment
established. It is the one lever available from outside.

Statistics cost one pass over the store (3.1 s at 7.5M quads) and are reusable; the server
builds them at start-up and rebuilds after an update. A stale snapshot makes a plan worse,
never wrong, because reordering a basic graph pattern cannot change its answer — which the
`reordering_preserves_the_pattern_set` test holds to account.

Turn it on with `--reorder` on either the command line or the server.

### 3c. What is left is a join operator, not a better estimate

With the order already optimal, the plan for a star is still:

```
LeftJoin(HashBuildLeftProbeRight, keys = ?s)
├── QuadPattern(?s ex:badgeNumber ?b)     200 rows — the hash build
└── QuadPattern(?s ex:name ?n)          750,000 rows — scanned in full to probe
```

It builds a hash table from the left and **scans the right in full**. Knowing that `?s` is
bound to 200 values does not help, because there is no operator that can use it.

A triplestore should instead probe the `spo` index once per bound subject — work
proportional to the *answer* rather than to the data. `cargo run --release -p holos-bench
--bin bindjoin` does that by hand, against the same store, for the same 20 rows:

| | 750k quads | 7.5M quads |
|---|---:|---:|
| Evaluator, already reordered | 43.9 ms | 370.2 ms |
| Hand-written bind join | **0.072 ms** | **0.076 ms** |
| | **611×** | **4,846×** |

The second row is the important one: **0.072 ms and 0.076 ms** — 40 index probes either
way. A bind join is constant in the size of the store, while the hash join grows with it.
That is why the multiple rises from 611× to 4,846× across a 10× increase in data, and why it
will keep rising.

This is not an estimation problem. The order is optimal in both columns. It is the absence
of an index nested-loop operator, and it cannot be fixed from outside the evaluator.

### 3d. The operator now exists

`holos_engine::bindjoin` is that operator, sitting ahead of `spareval` in the query path for
the shapes it can answer. The same benchmark, on the same 753,199-quad store:

| | Before | After |
|---|---:|---:|
| Query path, 20 rows from a three-pattern star | 43.9 ms | **0.535 ms** |
| Hand-written bind join | 0.072 ms | 0.084 ms |
| Gap to hand-written | **611×** | **~10×** |

**About 82× on the real query path.** What is left is the difference between a general
implementation and a hand-written one for a single known shape: choosing the next pattern
from statistics at each step, hashed bindings, decoding through the dictionary. That is a
tuning problem rather than a missing operator, and a much less urgent one.

Three things kept it honest:

* **The fragment is small.** `SELECT` in the default graph over basic graph patterns, `JOIN`,
  `UNION`, `VALUES` and `FILTER`, with `DISTINCT`, `LIMIT`, `OFFSET` and a projection.
  `OPTIONAL`, `GRAPH`, `MINUS`, aggregation, `ORDER BY`, property paths, `FROM`, blank nodes
  and triple terms are all refused and fall back. A fast path that is wrong is worse than no
  fast path.
* **Ordering is decided at each step, not once.** After the first pattern binds `?s`, the
  others stop being predicate scans and become subject-and-predicate lookups — a different
  estimate entirely. Ordering up front would miss the effect the operator exists for.
* **Policy is structural.** Scans go through the same `QueryableDataset` call `spareval`
  makes, so `decide_quad` runs on every quad. The fast path has no way to read around §14.

Seventeen differential tests run the same SPARQL through both paths and compare exactly —
rows, bindings and multiplicity — including that duplicates survive without `DISTINCT`, which
a bind join that deduplicated by accident would otherwise pass.

#### Three bugs, and the coverage that was not there

The differential tests all passed. Three bugs survived them, all of one kind: **the fast path
took over queries carrying something it did not model.** Comparing answers cannot find that,
because two of the three do not change an answer.

* **A substitution was dropped.** `QueryOptions::substitutions` binds a variable without
  interpolating it into query text. The gate excluding it read `!options.touches_dataset()`,
  and `touches_dataset` — accurately, given its name — reports on the dataset, not on the
  query. A substituted query was answered as though nothing had been bound. The comment above
  that gate already said substitutions were excluded; only the code disagreed.
* **A cross product materialised 13.7 GB.** `SELECT * WHERE { ?a ?b ?c . ?d ?e ?f }` over
  20,000 triples is 400 million rows. It is inside the fragment, legitimately — but this
  operator *materialises* where the evaluator streams, and it consulted no cancellation
  token, because the token is checked by the evaluator it had just skipped. The query's 60 ms
  timeout never fired. It was found as a test binary sitting at 13.7 GB resident and climbing.
* **`FROM` was ignored, and that one returned wrong rows.** `FROM` and `FROM NAMED` live in
  `Query::Select::dataset`, *beside* the pattern rather than inside it, so a check that reads
  only the pattern sees an answerable query and answers a different one. `SELECT ?s FROM <g1>
  WHERE { ?s ?p ?o }` returned the default graph's rows instead of `g1`'s.

The middle one is the most instructive, because nothing about it was *incorrect*: given time
and memory it would have produced the right answer. Being right eventually is a different
property from being usable, and only the first was under test.

The first two now run under `bindjoin::Limits` — a row budget and the deadline's token —
where hitting either returns `None`, meaning *ask the evaluator*, discarding the partial rows
rather than returning them short. The budget counts `seen` as well as `out`, because under
`DISTINCT` a large `OFFSET` grows one and not the other. The third is refused outright.

#### The suites that were supposed to catch this could not reach it

The honest part. There are three ways into evaluation, and the fast path was wired into one
of them. The W3C conformance runner reaches evaluation through
`Engine::query_prepared_with_services`, and the Python binding and audited CLI path through
`Engine::query`; both went straight to the evaluator. **Roughly a thousand W3C queries
appeared to be covering this operator and were not executing a line of it**, and the
most-used surface — the published `holosdb` package — never got the operator at all.

All three now share one `try_bind_join`, with a test comparing their answers against the
evaluator's so they cannot drift apart again. With the suites actually reaching it,
`sparql10`, `sparql11` and `sparql12` are unchanged to the test — the first real evidence the
operator agrees with the evaluator on SPARQL it did not have written for it.

A second gap sat underneath: `FROM` had **no coverage in this repo at all**. The SPARQL 1.1
suite contains exactly two queries using it and both are `CONSTRUCT`. The nineteen tests that
exercise dataset specification are in the SPARQL 1.0 suite, which was on disk and had never
been run. It is now the `sparql10` ratchet, at 262/263. A newer suite is not a superset of an
older one — 1.1's manifests test what 1.1 *added*.

#### 3e. Filters, pushed down

A `FILTER` is applied as soon as the last variable it mentions is bound, rather than after
the join. On the same store, a selective filter over the three-pattern star:

| | Rows | Time |
|---|---:|---:|
| Evaluator, filter after the join | 19 | 20.7 ms |
| Bind join, filter pushed down | 19 | **0.286 ms** |
| | | **72×** |

Steady-state over repeated runs. The first measurement taken was 67.5 ms against 0.948 ms —
the same multiple, on a cold cache; the absolute numbers move by a factor of three between a
cold and a warm store, and the ratio does not.

Row counts are asserted equal in the benchmark, not reported and hoped over. The benchmark
also asserts the filter matches *something*: the first version of it compared a string-typed
`badgeNumber` against a number, which SPARQL treats as a type error and an eliminated
solution, so it measured two ways of returning nothing and called the result 1×.

The predicate is evaluated by **`spareval`'s own expression evaluator**, through this
engine's function registry. That is the point: SPARQL comparison is value-based with numeric
promotion across `xsd` types, and a second implementation of `=` that got
`"1"^^xsd:integer` versus `"1.0"^^xsd:decimal` wrong would be a silent wrong answer. Only two
classes are refused, both because the borrowed evaluator runs against an empty dataset and
without an evaluation context: `EXISTS`, which would quietly answer `false` everywhere, and
`NOW`/`RAND`/`UUID`/`STRUUID`/`BNODE`.

Conjunctions are split, because `A && B` can only be applied once the later half is bound and
splitting lets the earlier half prune sooner.

#### 3f. `JOIN`, `UNION`, `VALUES` — and the spatial index finally pays

`FILTER` alone was expected to make the spatial index and the bind join compose. It did not,
and the reason is worth keeping: a routed GeoSPARQL query does not reach the planner as a
filtered BGP. `topology::rewrite` turns `?f geo:sfWithin <window>` into a geometry lookup
joined in as a *union* of ordinary patterns, and the spatial index then joins a `VALUES` of
candidate geometries onto that. The actual shape is

```text
Filter(Join(Join(Bgp, Union(Bgp, Union(Bgp, Bgp))), Values))
```

so all four node kinds had to be in the fragment before any of it could use an index
nested-loop join. With them in:

| Geometries | Rows | Unindexed | Indexed | Speed-up |
|---:|---:|---:|---:|---:|
| 10,000 | 0 | 75.95 ms | **114.40 µs** | **664×** |
| 50,000 | 4 | 381.22 ms | **158.30 µs** | **2,408×** |

This is the same benchmark that, earlier in this work, showed the index making **no
difference at all** — narrowing fifty thousand geometries to four changed nothing while the
join scanned all fifty thousand regardless. Two components, each correct, that did not
compose until there was an operator able to use a binding.

Row counts are asserted equal at both scales, not reported and hoped over.

The plan is a flat list of items rather than a tree, because evaluation is a nested loop:
entering a `UNION` branch replaces that entry with the branch's own patterns and carries on.
Ordering is still chosen per step, which is what puts a `VALUES` of four candidates ahead of
a scan of fifty thousand — the whole behaviour the index exists to create. `UNION` is a
multiset union: a solution satisfying two branches is produced twice, which the geometry
lookup depends on for a resource carrying both `geo:hasGeometry` and
`geo:hasDefaultGeometry`.

Two boundaries are worth stating because they are not obvious:

* **Hoisting a filter out of a join is only sound when its variables are certainly bound
  inside the subtree it was written against.** `{ ?a ?b ?c FILTER(?d = 1) } { ?d ?e ?f }` is
  the counter-example — `?d` is unbound where the filter sits, so it errors and eliminates
  everything, while the same filter after the join would see `?d` bound. A `UNION` makes this
  live rather than theoretical, since a variable bound in one branch may be unbound in
  another, so the plan tracks *certainly* bound separately from *possibly* bound.
* **A `VALUES` term the dictionary has never seen sends the query back to the evaluator.**
  Such a term still binds — `VALUES ?x { <urn:absent> }` is one solution, not none — but
  representing it needs a term id that does not exist, and interning one would be a write in
  the middle of a read. It costs nothing where it matters: the `VALUES` the spatial index
  generates holds geometries read out of the store.

### 4. A commit costs the size of its change, not the size of the scene

The holon table is the one the design rests on. **0.91 ms per commit against 140 ms for a
full pass — 155× cheaper**, at 1,103 fully validated, fully attributed commits per second.

Validation inside *every* commit is only sane at that ratio. At full-pass cost a boundary
would be a nightly batch job wearing a transaction's clothes, and the holon model collapses
into "validate the warehouse overnight and hope".

A **rejected** commit costs slightly more than an accepted one — 1.1 ms against 0.91 ms —
because it also undoes what it applied. That is the right way round: the system pays for
refusing bad data, not for accepting it.

The holonic queries are worth reading alongside, and the striking thing is how ordinary they
are. **Provenance of one statement** — *which tick asserted this exact triple, and who was
responsible* — costs **1.4 ms** and is a plain SPARQL query against `rdf:reifies` pointing at
an RDF 1.2 **triple term**. Two triples per change, where RDF 1.1 reification needed four and
gave them no defined meaning. No side table, no separate audit database, and the same policy
enforcement as every other read.

**Scene joined to log** at 24 ms is the one that would need two databases in most
architectures: current state joined to its own history, across two named graphs, in one
query.

---

## Caveats worth stating plainly

- **The holon suite runs at one scale.** It measures a *ratio* — commit cost against
  full-pass cost — and that ratio is the claim. How it moves with scene size is a separate
  question this harness does not answer.
- **`--bulk` is not free.** It skips the write-ahead log, so a load interrupted part way
  must be discarded rather than resumed. Right for a load you can re-run; wrong for one you
  cannot.
- **In-memory query timings are a floor.** A RocksDB-backed query pays block cache misses on
  top. The shapes stay in the same relative order; the absolute numbers rise.
- **The `*` finding is upstream, not local.** Zero-length property paths with an unbound
  left side are evaluated by the reused evaluator, not by anything in this repository. It
  is recorded here because it dominates one row of the table and because a store-aware
  planner is what would route around it.
- **No concurrency is measured.** Every number here is single-threaded. The server is
  thread-per-request over an `RwLock`, so readers scale and a writer excludes them, but this
  harness does not measure that.
