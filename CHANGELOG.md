# Changelog

Notable changes per release. Numbers quoted here are measured; the benchmarks that produce
them are in `BENCHMARKS.md` and are runnable.

## 0.9.0 — 2026-09-11

The release that made a **load** unable to take the process down, after 0.8.0 did the same
for queries. Found by loading 653,839,702 triples and watching.

### A bulk load's memory did not depend on the load. Then it did.

The streaming phase behaved exactly as designed: **2.4 GB peak across three and a half
hours**, sawtoothing as the dictionary filled, ingested and cleared. Before 0.7.0 the term
cache alone would have wanted about 62 GB, so this was the fix confirmed at forty-seven times
the scale it was verified at synthetically.

Then the final merge reached **18 GB** on a 32 GB machine — three times, once per triple
order. It survived. It should not have had to.

The memory was not HOLOS's. `index_opts` asked every index family for a bloom filter, and a
*full* filter is built in one piece: RocksDB's `FullFilterBlockBuilder` keeps a 64-bit hash
per key and turns the lot into bits at `finish`. A memtable flush does that for a few thousand
keys and nobody notices. A bulk load writes **one file per order for the whole store**, so the
vector is 8 bytes times every triple loaded — 5.2 GB at 653 million, and twice that at the
instant it doubles, because it holds the old buffer while filling the new one.

The fix is to cut the filter into partitions as the file is written, each one finished and
released, so what is held is one partition rather than one file. It needs the two-level index,
so both are set.

**It applies to the families, not only to the bulk writer,** and that was a second decision
taken after a second measurement. The first version partitioned the writer alone, on the
grounds that the read cost was unknown and the write side was on fire. Two things then turned
up. A whole filter is *pinned* by the table reader for as long as the file is open, which is
what makes the 653.8-million-triple store cost about **4 GB before it answers anything**. And
a `compact` rewrites every ingested file with the *family's* options — so partitioning only
the writer meant routine maintenance handed back exactly what the load had been fixed to
avoid.

`holos-bench`'s `filterread` measured the read side at 30 million triples, flipping that one
call and nothing else:

| | whole | partitioned |
|---|---|---|
| peak resident | 743 MiB | **275 MiB** |
| probe that misses | 2 us | 2 us |
| probe that hits | 1602 us | 1583 us |
| full scan | 5.7 s | 5.9 s |
| load | 100.0 s | 91.6 s |

No read penalty and 63% less held. The miss probe is the one that had to stay flat — it is the
case a bloom filter exists for, so an unchanged 2 us says the filter is still skipping files
rather than having quietly stopped working.

Measured by loading the same synthetic triples at two sizes, chosen so that only the index
grows — 4096 subjects and 64 predicates crossed with as many objects as the count needs, which
holds the dictionary at a few thousand terms whatever the count:

| triples | full filter | partitioned |
|---|---|---|
| 25,000,000 | 723 MiB | **271 MiB** |
| 100,000,000 | 2,558 MiB | **395 MiB** |
| growth for 4× the data | 3.54× — linear | **1.46×** |

The streaming phase took 55.2 s against 55.9 s and 220.1 s against 221.8 s across the two
builds — the bias check, since a change to the writer cannot affect the parse, and a
difference there would have meant the comparison was measuring something else. Extrapolating
the linear arm to 653.8 million predicts 16.3 GiB against the 18.0 GiB observed.

The merge got slightly *faster* too, by about a tenth at 100 million. Not building an 800 MB
vector turns out to be quicker than building one.

### Why nothing caught it

`crate::memory`'s ceiling reads a counting allocator, and a counting allocator counts Rust
allocations. This vector belongs to RocksDB's C++ one, so the ceiling could not see it, could
not have refused it, and would not have reported it. **A limit that cannot observe the thing
it is limiting is not a limit** — recorded here because the same blind spot covers every byte
RocksDB allocates, and the next one will not announce itself either.

### A bulk load needs half again the source file in scratch

Not a change, a measurement, because nothing had written it down and the number is large. The
653.8 million triple load held **48.3 GB of sorted runs** beside a 30.4 GB source, peaking
around 55 GB before the merge consumed them, to produce a **20.7 GB** store.

Scratch goes in `holos-ingest` beside the database, deliberately, so the ingest can move files
rather than copy them across a filesystem boundary. It is cleaned up on success and on
failure. But it means a load wants room for the source, the scratch and the store at once, and
`compact` checks its headroom before starting while a load does not. Sizing guidance is in
OPERATIONS.md; the preflight is the next piece.

### Verified end to end at 653.8 million triples

The whole path an operator takes, on this build: `holos stats --store E:/store2 --data
<30 GB Turtle> --bulk`, then `deploy/run.sh`, then queries over HTTP.

| | before | 0.9.0 |
|---|---|---|
| load, final-merge peak | 18,007 MiB | **3,413 MiB** |
| server, memory with the store open | ~4,000 MiB | **199 MiB** |
| server, peak during `COUNT(*)` | — | 1,230 MiB |
| `COUNT(*)` over HTTP | 653,839,702 | **653,839,702** in 3 m 56 s |
| load rate | 50,210/s | 48,869/s |

Same count, same predicate histogram, same rate; the merge spike is 5.3× smaller and the
store costs twenty times less to have open, because the partitioned filters are no longer
pinned by the table readers.

### The server names its store on its first line of output

That verification run began by reproducing a failure. A store loaded into one directory with
the CLI, `deploy/run.sh` started on its default `HOLOS_STORE=./var/store`, and every query
answered from an empty database with no error anywhere — the server had opened the empty
directory and created a fresh store in it. Geospatial functions with literal arguments kept
working because they never touch the store, which made it look like a partial failure rather
than the total one it was. Ten lines of startup output and none said where the data was.

Now the first line is `store E:/store2 — 653839702 quads`, or on the directory that was
being served, `store ./var/store — empty. Nothing has been loaded here; if that is a surprise,
check that this is the directory the load wrote to.` One `META_QUADS` read, so it is free.
OPERATIONS.md's loading section now leads with the pitfall and the `holos.env.local` override
that `run.sh` sources last.

### Recorded and not fixed: the spatial index is a full dictionary walk at startup

`refresh_spatial` runs unconditionally when the server starts, and its first build decodes
**every dictionary literal** — one `id2str` point lookup each — to ask whether it is a
geometry. On this store that is about 150 million lookups and **3½ minutes** before
"listening" appears, for zero geometries. Every restart pays it.

It cannot be skipped by checking whether any GeoSPARQL predicate has triples: `geometry_of`
decides by *datatype*, so a `wktLiteral` interned under any predicate at all is a geometry
the index must know about, and the watermark invariant — everything below it is indexed —
does not survive a build that looked at nothing. What it can be is cheap: `from..count` is a
contiguous id range in `id2str`, so one range iterator does the work of 150 million point
gets. That needs a range-decode method on the `Storage` trait for both backends, and is the
next piece.

### Measured, not changed: what a large store is actually slow at

A sweep of representative shapes against the 653.8-million-triple store, timed net of a 2.7 s
store open. Nothing here is a fix; it is what the next piece of work should be aimed at.

| shape | net | note |
|---|---|---|
| subject star, 11 rows | ~0.1 s | |
| object bound, selective | ~0.5 s | |
| two-hop join, `LIMIT 10` | ~0.1 s | |
| count over a bound predicate, 48.4M rows | ~19.6 s | 2.47M quads/s |
| `DISTINCT` over 3 distinct values | ~38 s | scans all 48.4M to find them |
| range `FILTER`, 48.4M rows | **123 s** | slower than the plain scan above |
| `ORDER BY` + `LIMIT 10`, 48.4M rows | **>15 min** | 8.4 GB buffered, then stopped by hand |

**The range pushdown fires and still loses.** `bounded scans = 3` — it is not failing to
engage. But a numeric span must also include the whole `Tag::Literal` range, because
`xsd:decimal` is not an inline type and a dictionary-backed literal could satisfy the
comparison. For a predicate whose objects are literals, that reads all of them. The same query
with a cut matching **zero rows** took 123 s: the cost does not depend on selectivity at all.
`xsd:date` is not inline either, so ranges over dates pay this too. Inline is `Integer`,
`Float`, `DateTime`, `Small` — and `xsd:date` being absent while `xsd:dateTime` is present is
the kind of gap that looks like a typo in a dataset rather than a performance cliff.

**`--reorder` does not help, and on the CLI it cannot.** The statistics build is a full scan —
about 300 s on this store — and the CLI redoes it on *every invocation*, so admission control
and the `DISTINCT` spill are both effectively unreachable from the command line. A server
builds them once at startup, which is where they work.

**`rangequery` had never run against `RocksDB`.** It built an in-memory store and always had,
so the end-to-end range figures in BENCHMARKS.md are memory-only while the primitive in
`rangescan` was measured on both. It now takes an optional store directory, and reports
`bounded scans` beside each row so a zero — the pushdown not firing — cannot be mistaken for a
disappointing speedup.

**`--explain` runs the query.** `spareval`'s explanation is a profile, not a plan, so it
evaluates and then reports. On a 272 s query that is 272 s. The help text said "Print the query
plan as JSON instead of the results", which reads as plan-only; it now says what happens.

### Building statistics no longer costs more than the query they protect

`--reorder` kept, for every predicate, an `FxHashSet` of every distinct subject carrying it
and another of every distinct object — and asked them for nothing but their `len` at the end.
On the 653.8-million-triple store that is fourteen predicates over 48.4 million subjects, and
it peaked at **12,984 MiB**. Counting the subjects rather than collecting them took it to
**5,207 MiB**, measured on the same store.

Sketching the objects on top of that took it to **5,206 MiB** — that is, nowhere, and the
number is quoted rather than hidden because it corrects an estimate made here before it was
checked. What remains at that point is the cost of holding a 20.65 GB store open and scanning
it, not the object sets. The sketch is a *bound*, not a saving: the sets grew with distinct
objects per predicate and nothing capped them; 16 KiB per predicate is capped. This dataset
did not happen to exercise the case.

The shape of that is worse than the number. `--reorder` is what supplies the estimate,
the estimate is what admission control refuses on, and the spill is only reached from inside
the refusal — so **the only way to protect a server against a memory-hungry query was to run
one first**.

Distinct subjects need no set at all, and the scan order is why: it is in `spo`, so a
subject's quads arrive together and the subject is never seen again. By the time its
characteristic set closes, the predicates it carries are already known — one increment each,
exact, and free.

Objects have no such luck: in `spo` order they arrive scattered, so there is nothing
contiguous to count. They get the `HyperLogLog` sketch §7 always specified — **16 KiB per
predicate whatever the cardinality**, against sets that grew without limit.

The docstring here used to argue against a sketch, and it was right at the time: these counts
were about to be used to measure how accurate estimation *can* be, and an approximation in the
yardstick would have muddied the measurement it existed to support. That measurement has since
been made. The estimator's q-error is 1.1 — about 10% — and the sketch was measured too, at a
worst case of **2.3%** across cardinalities from 1 to five million, on numbers used only as
ratio denominators that are guarded against zero anyway.

The precision is the smaller of the two that were measured, deliberately. 2^16 registers would
have held the worst case to 1.0%, but a sketch is per *predicate*, so its memory is unbounded
in something the dataset controls: a store with Wikidata's eleven thousand properties spends
176 MB at 2^14 and 704 MB at 2^16. Swapping a structure that grew without limit in the data
for one that grows without limit in the schema, to buy precision four times finer than
anything downstream can use, would have been the same mistake in a new place.

Small counts are the case a raw `HyperLogLog` is worst at and real data is full of; a predicate
with two objects is ordinary, and an estimate of three there is a 50% error on a small
denominator. Linear counting below the threshold makes those exact rather than approximately
exact, and there is a test that says so for 1, 2, 3, 10, 100 and 1000.

Ids are mixed before they are hashed, and that is not ceremony. `TermId`s are handed out
sequentially by the dictionary, so their top bits — the ones a sketch reads as a register
index — barely vary across a whole load. Unmixed, every term would crowd into a handful of
registers. There is a test that a sparse spread of ids counts the same as a dense one.

The whole `holos-stats` suite passed with this counter **doubled**, which is how thoroughly
untested it was: every subject in the fixture had exactly one object per predicate, so
`triples`, `subjects` and `objects` were equal and any of the three would do. The new test
separates them — four triples, two subjects, three objects — and does fail when the counter is
wrong.

### A spilling `DISTINCT` writes to the system temp directory

Not a change, a warning, and it nearly cost a system volume to find. `--spill-distinct` bounds
a query's memory, not its disk, and it spills in proportion to the *answer*: the query above
reached 5.5 GB of runs in twenty-two minutes and was on course for about seventy, against
29 GB free on `C:` while the store sat on a volume with 162 GB.

The bulk loader faced the same choice and made the other one — its scratch goes beside the
database, on the volume the operator sized. A query cannot do that, having no store directory
to sit beside and sometimes no store on disk at all, so the placement stays with `TMPDIR`.
**Point it at the volume with the room.** OPERATIONS.md now says so, and also that a *killed*
process leaves its scratch behind, since the cleanup is on drop.

### `--max-blocking-rows` reaches the CLI, where the spill was unreachable without it

0.8.0 inverted the spill: it stopped being the default path and became what a query gets
*instead of* being refused, which means it is only ever reached from inside the over-budget
branch. The CLI set `--spill-distinct` and no budget, so after that inversion it had the flag,
the plumbing, and no protection — the failure being that nothing failed. Both flags are now
documented in `holos query --help`, which listed neither.

## 0.8.0 — 2026-09-10

> **Never published.** These changes were committed but not tagged, so no wheel or binary of
> 0.8.0 exists and PyPI went 0.7.0 to 0.9.0. The notes stay because the commits do, and
> because upgrading from 0.7.0 means reading both this section and the one above it.

The release that made a query unable to take the server down — and made the query that did
it twice finish instead.

0.7.0 shipped a memory ceiling. It did not work. A deployment aborted again with the ceiling
in force, and the numbers said why: an 8 GiB limit and a request for **16 GiB**. This is
that, properly, in three layers.

### The query

```sparql
SELECT (count(distinct *) as ?count) WHERE { ?sub ?pred ?obj . }
```

`spareval` answers it by holding every distinct solution in a hash set. The first crash asked
for 25 GiB, and the size named the structure to the byte: 2³⁰ buckets of a 24-byte
`InternalTuple` plus a control byte each. On a 667-million-triple store nothing was going to
make that fit.

Worth knowing: over the default graph the `DISTINCT` **buys nothing**. A store holds triples
as a set, so `COUNT(*)` gives the same answer from a `u64` counter in constant memory. It
differs only across a union of named graphs.

### `DISTINCT` spills instead of being refused

The one that lets the query *finish*. Rows are held in a hash set until it outgrows its
budget; then they are encoded, sorted, written as a run, and the runs are merged — the
technique `sort.rs` already uses for index orders and `dictsort.rs` for the dictionary.
`COUNT(DISTINCT *)` gets the best case, since the rows are only counted and never needed, so
the answer is **O(1) in memory however large the input**.

**It is not the default, and measuring is why.** On three million rows it returned the right
answer in **23.6 s against `spareval`'s 2.2 s**, using **1,259 MiB against 396 MiB** — with
the disk never touched. The cost is structural rather than a bug: `spareval` deduplicates on
internal term ids that are never decoded, while this path works above its public surface,
where every solution has already been materialised into heap-allocated terms.

So the fast path stays, and spilling is what a query gets *instead of being refused* — it
engages exactly where `--max-blocking-rows` would have declined, and therefore needs
`--reorder` too. Ten times slower is a large price for an answer and no price at all against
the alternative.

Rows compare as bytes, encoded from their N-Triples forms — injective over terms, which is
what makes byte equality term equality. `"1"^^xsd:integer` and `"01"^^xsd:integer` are
different RDF terms and `DISTINCT` keeps both; an encoding that compared *values* would
under-count and never say so.

A spilled `DISTINCT` comes back sorted and an unspilled one in hash order. Neither is arrival
order, and SPARQL promises none.

### Doomed queries are refused from their estimate

`--max-blocking-rows`, default 10,000,000, needs `--reorder`. An `ORDER BY`, `DISTINCT` or
keyed `GROUP BY` whose input is estimated over budget is declined in a millisecond rather
than discovered slowly, using the characteristic-set estimates already built for join
ordering — q-error 1.1, ample for telling a thousand rows from a billion.

Tried *after* spilling, deliberately: a query that can be answered must not be refused for
being large.

Not every aggregate blocks, and the tests found that out by refusing `COUNT(*)` — which
streams. A group buffers when it has a grouping key or a `DISTINCT` aggregate. And a `LIMIT`
rescues a `DISTINCT` but not an `ORDER BY`, since the k smallest cannot be known without
sorting all of them; the refusal says which case it is rather than giving advice that does
not work.

### The ceiling is read at the scan, not on a timer

0.7.0 sampled every 10 ms and wanted three consecutive breaches. A `Vec` doubling from 8 GiB
asks for 16 while still holding the old 8 — **24 GiB in hand** — so it goes from *at* the
ceiling to fatally past it in one allocation, and no sampling interval fits inside that step.
Worse, `used > limit` is false while a buffer sits exactly at the limit.

It is now read in `decide`, which `DESIGN.md` §14 already makes the one route to the indexes.
An operator buffering toward an abort is buffering *from the scan*, so it is refused on the
first quad past the line. Armed after the store opens, so it measures growth past the resting
set rather than counting the dataset against the budget.

`--max-query-memory` now says the thing that was missing: **set it to at most a third of the
memory you can spare**. A ceiling near the machine's limit does not prevent the abort, it
chooses where it happens.

### What is still true

None of this catches a query that allocates without reading, and nothing short of a fallible
allocator could — `handle_alloc_error` aborts rather than unwinding.

And only `DISTINCT` spills. `ORDER BY` and keyed `GROUP BY` still buffer, so a large one is
refused rather than answered. The same collector generalises to an external merge sort, which
is the next piece.

## 0.7.0 — 2026-09-09

The release that made a load's memory stop growing, and the first one you can download a
binary from.

### A bulk load's memory no longer tracks the file

0.6.0 held every dictionary row until the end of a load and ingested them as one sorted
file. That made the term cache load-bearing: `resolve` reads the database, a buffered row is
not there yet, so the only thing stopping a term being interned twice was the in-memory
cache — which therefore had to hold **every term the load had seen**.

It grows at **222 bytes per distinct term**, measured across a 6.7× range. A generated
person dataset carries 1,268,458 distinct terms per 3M triples, so the 32 GB file it came
from has around 281 million and the cache alone would want **62 GB**. On a 32 GB machine
that load cannot finish — and it aborts the process rather than failing the request, exactly
as the query path did before 0.6.0 put a ceiling on it.

Flushing both dictionary families when the buffer reaches its budget makes the rows
readable, which makes the cache an optimisation again rather than a correctness requirement,
so it can be cleared. Memory then tracks the budget instead of the file:

| distinct terms | 0.6.0 | 0.7.0 |
|---:|---:|---:|
| 900,000 | 714 MiB | 500 MiB |
| 6,000,000 | 1,796 MiB | 392 MiB |

The slope goes from **+222 bytes per additional distinct term to −22**. Six million distinct
terms now peak *lower* than nine hundred thousand.

The cost is several ingested files per load instead of one. At an artificially small 32 MiB
budget that is 52,419 quads/s against 38,844 — a consistent 26% — but at the 256 MiB default
the two rounds disagree about which build is faster, so it is inside the noise.
`set_dict_spill_bytes` is the knob.

Both families are flushed together and the cache cleared only afterwards, because a term is
safe to forget only once *both* of its rows are readable. The test for the failure this
could cause — a repeated term handed a second id after the cache is cleared, splitting one
node into two so a join that should match silently does not — asserts on `dictionary_len`,
so double-interning is visible rather than inferred.

### Binaries, at last

The Releases page stopped at 0.1.1 while PyPI went to 0.6.0: every version since was tagged
and published as wheels, and none produced anything a person could download and run.
`pip install holosdb` serves Python callers and does nothing for someone who wants the
server or the CLI, for whom the alternative was a Rust toolchain and a RocksDB compile.

A tag now builds `holos` and `holos-server` for five targets — Linux x86-64 and aarch64,
macOS arm64 and x86-64, Windows x64 — checks that both binaries start before packaging
them, and publishes a release with a `SHA256SUMS` file. The release notes are the changelog
section for that version, so the two cannot drift.

It is a separate workflow from the wheels on purpose: that one is gated on a PyPI trusted
publisher, this one needs permission to write to the repository, and a single workflow
holding both is a wider blast radius than either job needs.

### Repository

Consolidated to one branch. `feature/initial_release` held only four ceremonial merge
commits and nothing unique; release tags keep those commits reachable.

## 0.6.0 — 2026-09-09

The release that stopped one query killing the server, and made a bulk load's
dictionary as cheap to write as its indexes.

Both came out of running HOLOS against real data rather than the synthetic set: a 32 GB
Turtle file of generated people, and a 32 GB machine that a single `COUNT(DISTINCT *)`
took down.

### A query can no longer abort the process

`SELECT (COUNT(DISTINCT *) AS ?n) WHERE { ?s ?p ?o }` reads like a counting query.
`spareval` answers it by holding every distinct solution in a hash set, so on a large store
it asks for tens of gigabytes, and the allocation that finally fails calls
`handle_alloc_error` — which **aborts the process**, taking the listener, every other
client's in-flight query and any write in progress with it. There is no catching it.

The failing request was for 25 GiB, and the size named the structure to the byte: 2^30
buckets of a 24-byte `InternalTuple` plus a control byte each. Only `DISTINCT` and
`COUNT(DISTINCT *)` hold a set of those — plain `COUNT(*)` is a bare `u64` counter and
would have streamed in constant memory.

`holos-server` now installs a counting allocator, and the deadline watchdog samples it and
cancels through the same token a timeout uses. **`--max-query-memory`, default 8 GiB.**
Cancellation is checked once per quad, so a query filling a hash set is stopped between
rows while the set is still small enough to free.

Every library crate keeps `#![forbid(unsafe_code)]`; the twenty lines that need `unsafe`
live in the binary.

Two limits are documented rather than hidden. The counter is **process-wide**, so it cannot
attribute a byte to a query — a breach must persist across three samples before it cancels,
and the ceiling is a backstop against one runaway query rather than a per-query quota. And
a cancelled aggregate does not yield one row: `spareval` forwards one error per remaining
input row.

### A bulk load's dictionary is ingested, not batched

The nine index families have been handed to `IngestExternalFile` as sorted files since
0.3.0. The two dictionary families never were, so every interned term paid for a memtable
write, a flush, and the compactions that followed.

On data with high term cardinality that is most of the load. The generated person dataset
has **900,000 distinct terms per 3M triples**, against 200,000 per 753k in the synthetic
benchmark, and the shares invert: dictionary 76%, parse 15%, index writes 9%. The
dictionary tracks *distinct terms*, not triples — two files of equal size and quad count
differing only in cardinality took 2.1 s and 12.2 s, a **6×** spread.

`dictsort.rs` is `sort.rs` for variable-width rows: length-prefixed runs, a byte budget, a
k-way merge, one SST per family. It **combines** rather than deduplicates, because past 512
bytes a `str2id` key is a hash and two colliding terms share a row whose value is the list
of both their ids. Dropping one would lose a term permanently. Merging by key also fixes a
hazard the batch path had, where a candidate read could not see rows the same load had
buffered.

Measured on 3M quads with 900k distinct terms: **16.33 s to 12.37 s**. The ratio is inside
this machine's noise floor and is not the evidence — the non-overlapping ranges are. Every
run of the new build landed in 11.68–12.61 s, every run of the old in 12.62–16.33 s.

### Four optimisations that were tried and measured as worthless

Recorded in `loadprofile` because each looked obviously right beforehand: a bloom filter on
`str2id` at high cardinality (**consistently slower** — a load *writes* that family, so
filter blocks are rebuilt on every flush); deriving the `str2id` key only after the caches
miss (0.99×); the flush in `end_bulk_load` (0.47 s, not the cost); and pausing dictionary
auto-compaction during a load (1.09×, and it trades write amplification for read).

`loadprofile` itself earned two fixes. It hardcoded N-Triples, so pointing it at a Turtle
file measured a parse that produced nothing. And it timed `end_bulk_load` inside the
interning phase — a conflation that sent an earlier round of optimisation after caches and
allocations that were never the problem.

## 0.5.0 — 2026-09-04

The release that let two datasets in different coordinate reference systems be
queried against each other.

0.4.0 was about making a filter reach the index. This one is about geometry, and about a
gap the specification leaves rather than one this project had: GeoSPARQL stores the
reference system inside the literal and then provides no way to change it. Everything else
here follows from closing that.

### GeoSPARQL reads more than one coordinate reference system

GeoSPARQL puts the reference system inside the literal and then defines no way to change it,
on the assumption that a dataset is internally consistent — which stops being true the
moment two are joined. `spargeo` refuses anything that is not CRS84, so until now so did
this engine, and Ordnance Survey open data was not awkward to query but impossible.

Four systems are now read — CRS84, EPSG:4326, EPSG:27700 and EPSG:3857 — and converted to
CRS84 on the way in. That one decision is what makes a grid reference and a GPS fix
comparable: two operands reach a function already in the same space, so
`geof:distance(?grid, ?gps, uom:metre)` is an ordinary distance. The 43 functions borrowed
from `spargeo` gain it through a wrapper on their arguments rather than 43 patches, and the
spatial index gains it because it decodes literals through the same path a query does.

Two functions are new: `holos:transform`, which converts an answer back out — in this
project's namespace, because a transform function in `geof:` would claim an OGC sanction it
does not have — and a replacement `geof:getSRID`, since `spargeo`'s reports CRS84
unconditionally, which was true when CRS84 was all it could parse.

**An unrecognised system is refused rather than assumed to be CRS84**, which is the whole
safety argument. Falling back would read an easting of 318086 metres as 318086 degrees of
longitude: no error, no empty result, just a geometry that joins in every comparison and
appears in no answer it belongs in.

**The accuracy is stated because it is not exact.** The British National Grid projection is
the Ordnance Survey series and reproduces their worked example to a millimetre; the datum
shift is their 7-parameter Helmert, which needs no data files and is **2.0 m mean, 5.8 m
worst** against OSTN15 measured over 1330 points across Great Britain. Every forward
transformation is checked against PROJ to a millimetre.

`geof:distance`, `geof:buffer` and `geof:envelope` were already complete and already worked
in metres; what was missing was the ability to point them at data that had not been
published in degrees.

## 0.4.0 — 2026-09-03

The release that made a filter reach the index, and finished P1.

0.3.0 made every write path atomic. This one is about reads: a `FILTER` that bounds a
pattern's object now bounds that pattern's scan, which is the promise `DESIGN.md` §5 has
carried since the first commit and had never kept. Along the way the load path stopped
holding a whole load in memory, three queries the specifications call invalid stopped being
answered, and the maintenance commands stopped being able to fill a disk.

### Range filters reach the index

`DESIGN.md` §5 has said since the beginning that order-preserving encodings exist so
`FILTER(?d > "2020-01-01")` can bound an index scan rather than be tested against everything
the scan returns. It does now. A filter that bounds a pattern's object bounds that pattern's
scan, measured end to end against the same build with the pushdown disabled:

| selectivity | off | on | speed-up |
|---:|---:|---:|---:|
| 1% | 397 ms | 5.7 ms | **69×** |
| 10% | 410 ms | 61 ms | 6.7× |
| 50% | 495 ms | 314 ms | 1.6× |
| 90% | 627 ms | 540 ms | 1.2× |

The whole feature turns on one asymmetry: **the span decides what is read, the filter still
decides what matches**. A span may admit too much, which costs time; if it admits too little
the row is never seen, and a row lost that way looks like data that was never stored.

Which makes "what could satisfy this comparison" the only question, and §5's own table was
wrong about the answer. It claimed `xsd:decimal` and `xsd:double` were inlined at tag `0x5`;
the encoder takes `xsd:float` only, and only when its canonical form round-trips. Decimals,
doubles, integers past 60 bits and awkwardly-spelled floats are all dictionary literals whose
ids say nothing about their values — and all still numbers. So every span includes the whole
dictionary region, and a numeric one includes the other numeric tag too. The table is
corrected.

Three things this needed, each with its own guarantee:

- **A bounded scan in the store**, on both backends — RocksDB iterate bounds one component
  past the prefix, `BTreeSet::range` in memory. Checked against scan-and-filter for every
  span, graph shape and binding combination.
- **A bounded path through the view**, which is `DESIGN.md` §14's chokepoint. A second way to
  reach the index is a second chance to reach it without asking policy, so `decide` and
  `denied_graph` are shared rather than reimplemented, and `range_policy.rs` asserts equality
  of *visibility* — what is seen, and when a request is refused. That test found the suite
  had no `Fail`-semantics policy, which is the only mode where the denied-graph short circuit
  is observable at all.
- **The pushdown itself**, in the bind join, verified against `spareval` the way the rest of
  that operator is.

### Queries the specifications call invalid are refused

Three of them were being evaluated: a triple term as another triple term's subject, a literal
as one, and `COUNT(COUNT(*))`. Inside `VALUES` the parser catches these; inside `BIND` it does
not, because there the term becomes an ordinary function call whose arguments are
unconstrained.

`holos_engine::validate` refuses them after parsing — the same judgement `holos-shacl` makes
when it declines a shapes graph it cannot check, and for the same reason: a query with no
defined answer still produces something that looks like data. All three had been recorded as
`upstream:` failures and were not. SPARQL 1.2 is **268/269**.

### A bulk load's memory no longer grows with the load

Sorted ingestion held everything in memory to sort it and, past a cap, fell back to writing
keys one at a time. The cap is now a spill threshold: fill a buffer, sort it, write a run per
index order, merge the runs at the end. Memory stays flat around half a gigabyte whatever is
loaded, and spilling costs about **12%** at thirty runs — where exceeding the budget used to
drop the load onto a path five times slower.

`P1` is complete.

### Maintenance commands refuse what will not fit

The store grows and nothing shrinks it unasked. `holos compact` writes a second store beside
the first and could discover at 90% that the disk was full; `holos backup` hard-links, so it
fits immediately and then pins the files compaction can no longer reclaim.

Both now check. `compact` refuses with both numbers and a `--force` for when the estimate is
too conservative, `backup` warns because the same-filesystem case genuinely fits, `POST
/backup` answers **507** so a scheduler can tell insufficient storage from a fault, and
`holos stats` reports size, free space and whether a compaction would fit — so the question
can be asked before a maintenance window rather than during one.

### Every benchmark table re-measured

The tables in `BENCHMARKS.md` predated both the bind join and sorted ingestion; the old query
rows had a 3-way star at 385 ms where it is now 0.22 ms. All four are replaced from one run,
and `DESIGN.md`'s scaling table is re-measured at both ends of a 14× span. Bytes-per-quad does
not move across it — 34.8 at 753k quads, 35.2 at 10.5M — which is the figure that says nothing
in the storage layer degrades super-linearly.

## 0.3.0 — 2026-09-03

The release that made every write path atomic, and finished SHACL 1.2 Core.

A store that validates on commit is only worth having if a commit is a commit. Before this,
an update or a holon tick was a *sequence* of individually-atomic quad writes, and a crash
between two of them left half of one behind. Four separate places could leave a failed write
partly applied, and one of them could destroy a graph it was asked to replace. That is the
theme; the SHACL, entailment and join work below is the rest of it.

### A commit is one batch

`Storage::begin` / `commit` / `rollback` accumulate a whole update or tick into a single
RocksDB `WriteBatch`, written once. A process that dies mid-commit now leaves the store as it
was, rather than with the operations that happened to have been written already.

Buffering is what makes it atomic and is also the hazard: a buffered write is invisible to a
`get` and to an iterator, and SPARQL requires each update operation to see the effects of the
ones before it. So the scope carries a read overlay for quads, named graphs and the
dictionary — including the reverse id-to-term map, which the bulk path has no use for because
a load does not read. `tests/commit_scope.rs` runs one script inside a scope and outside it
and requires every read to agree at every step.

It replaces two hand-rolled undo logs — `update.rs`'s `Journal` and the tick's compensating
writes — which rolled back by writing the inverse of each change, so a rollback could itself
fail and leave a state nobody could describe. Abandoning a batch that was never written
cannot. Three bugs turned up under it, all of the shape "a write that did not go through the
place that knows about buffering": `put` and `delete` wrote straight to the database, `lookup`
resolved terms against it alone so `contains` could not see a quad the scope had just written,
and the in-memory store created a quad's graph through the index directly so its journal never
learned about it.

A scope past a few hundred thousand quads is refused rather than silently split, because a
split batch keeps the interface and drops the guarantee.

### Four places a failed write left half of itself behind

- **`PUT /gsp` could destroy the graph it was replacing.** A replace is clear-then-merge and
  the body is not parsed until the merge, so a malformed body emptied the graph, failed, and
  returned the client a 400 saying the request had not happened. A policy refusal partway
  through did the same, later. The sequence lived as a comment in the HTTP handler, which is
  why it was never tested; it is now `gsp::write`, where the atomicity belongs to the
  operation and the failure can be tested.
- **`branch` left half a branch.** The scene and boundary are copied without a policy check
  and only then does `register` ask for `ADMIN`, so a principal who may write but may not
  register got all the way through the copying before failing — leaving a scene under a name
  nothing pointed at.
- **`register` and `set_version`** could leave a holon definition missing its admission quad,
  or a holon with no version at all between the remove and the insert.
- **`POST` and `DELETE` on the Graph Store Protocol** wrote quad by quad, and `drop_graph`
  could leave the existing-but-empty graph its own documentation exists to prevent.

### Boundary rules stopped believing in refused commits

`Rules` keeps its bridged copy of the scene current *by delta*, which is what makes firing
rules per commit affordable. It was told about a tick's delta before anyone knew whether the
delta would be kept, and nothing told it when the tick was refused. The bridge went on holding
triples the scene had never accepted, and every later tick inferred from them — with the
damage surfacing one commit later, on a different subject, as an inference with no visible
cause.

The fix belongs one level down, with the thing that owns the graph: `EngineRun::checkpoint`,
then `accept` or `revert`. A caller that never checkpoints records nothing and pays nothing.

Writing it exposed an older bug in `EngineRun::apply`. A delta is an ordered sequence; the
graph underneath takes two unordered sets and removes everything in one before adding
everything in the other. Partitioning a sequence into those sets loses the order, so a row
both added *and* removed within one delta came out **present**, whichever way round the caller
meant it. `apply` now reduces to a net effect per row, last change winning.

### Bulk loads are ingested as sorted files

`SstFileWriter` + `IngestExternalFile`, which `DESIGN.md` §6.1 has wanted since the start. A
load holds the encoded quads, sorts them once per index order, writes one file per order and
hands them to RocksDB, which can adopt a non-overlapping file at the bottom level instead of
pushing every key through the memtable, the log, and then the levels again.

**1.7× on a whole load** — 10.7 s to 6.4 s for 753k quads, interleaved, eight pairs. The
`loadprofile` benchmark was written *first*, and it is why the number is 1.7× rather than the
order of magnitude the roadmap implied: the index writes were 55% of a load and are now almost
none, so what is left is parsing and the dictionary. It also settled a change made on a
misreading — a bloom filter on `str2id` — which measured as worth nothing and was reverted
rather than kept on the theory.

Not the external merge sort: past `MAX_SST_QUADS` a load falls back to the buffered-batch
path, so the win comes with a memory budget.

### A guide to holon models

[HOLONS.md](HOLONS.md) and `examples/holon/`, driven by `cargo run -p holos-holon --example
tour` — a maintenance work-order model with four commits chosen so each shows a different
answer a boundary can give: one accepted, one refused, one where a rule fires, and one where
the rule is what causes the refusal. The tour is a program rather than a transcript because a
transcript goes stale in silence and a program that stops compiling fails the build.

### Removed

- **`holos-tabular`.** The TARQL-style CSV → RDF loader was never wired into the CLI, the
  server or the Python binding, and shipping a crate nothing consumes is worse than not
  shipping it. Its route is gone from `MINTING-TRIPLES.md`. The code is in the history if it
  is wanted back, and the licence reasoning that produced it is still in `THIRD-PARTY.md`
  because the constraint has not changed.

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

### Boundary rules fire in a tick

Step 2 of the holon tick was switched off, and the code said why: SHACL-AF fixpoint evaluation
exists in the adapted engine but needed a fresh bridge per commit. That was the gap §8 named,
and closing §8 is what removed it — so this is the payoff from that work rather than a new
capability bolted on.

`holos_holon::Rules` holds one bridged graph across ticks and keeps it current by delta;
`EngineRun::infer` runs the rules to a fixpoint and returns *what they added*, leaving where
those triples belong to the caller. `tick_with_rules` puts them in the scene.

The order is the point. Rules run **before** validation, so what they infer is judged by the
boundary: a rule writing something the shapes forbid **rejects the commit** rather than
persisting quietly. The inferences are ordinary additions in the event, and a refused tick
takes them out with everything else — a reader asking what changed should not have to know
which triples a rule wrote. And a rule is not a way around §14: it runs inside the session, so
what it writes meets the same policy as what the caller writes.

### A refusal that was too wide

Wiring this up ran straight into the fail-closed check added earlier, which refused any shapes
graph containing `sh:rule` — making rules and the incremental write path mutually exclusive.

The refusal was wrong. SHACL-AF states that executing rules is a *separate operation* from
validation, so a validator ignoring `sh:rule` is doing what the specification says validation
does, not silently dropping a check. The reasoning behind the original refusal — that a rule
changes what the data is, so ignoring it validates a graph nobody has yet — is about a
pipeline, not about validation. What belongs on that list is what would change an *answer*: a
constraint that would be dropped, or a target that would select different focus nodes.

Corrected, and the test that asserted the old behaviour now records why it was wrong.

### `MINUS`, blank nodes and property paths — and two constructs that stay out

`MINUS` shares a structure with `OPTIONAL`, because they ask the same question — did the right
side match this left row — and differ only in what they do with the answer. It removes a
solution when a compatible one exists on the right *and the two share a variable*; with
disjoint domains it removes nothing, which is the rule people are surprised by. A right side
sharing nothing certainly bound on the left is refused rather than reasoned about: the shared
variable might be one an `OPTIONAL` left unbound, and then whether the row goes depends on the
data.

**Blank nodes now bind like the variables they are**, which turned out to be the larger half
of "property paths". The parser desugars `?s :p/:q ?o` into a BGP joined on an anonymous blank
node, so sequence paths were being refused for containing a blank node rather than for being
paths — and `^:p` was already a swapped pattern. Renaming brings both in, along with the
`[ ]` syntax. The name the rename produces is deliberately not valid SPARQL, because `_:x` and
`?x` are different things and a rename that mapped them together would join them.

Alternative paths (`:p|:q`) become union branches. The closure paths — `*`, `+`, `?` — and
negated sets are refused: they need a fixpoint traversal this operator does not have.

**`ORDER BY` and aggregation stay out, and the reason is worth stating.** Both are
implementable; neither is implementable *safely*. SPARQL leaves the relative order of
incomparable terms to the implementation, so matching the evaluator's sequence means matching
its tie-breaking — duplicating code rather than reusing it, and creating two implementations
that must agree exactly. Aggregation is the same argument over a larger surface: `COUNT
DISTINCT`, `AVG` over mixed numerics, `GROUP_CONCAT` separators, and SPARQL's error semantics
throughout. The fallback costs time; a second implementation that drifts costs answers.

The fragment is now `SELECT` over BGP, `JOIN`, `UNION`, `VALUES`, `FILTER`, `OPTIONAL`,
`GRAPH`, `MINUS`, hiding-nothing subqueries, blank nodes, and sequence, inverse and
alternative paths — with `DISTINCT`, `LIMIT`, `OFFSET` and projection.

### `GRAPH` and subqueries join it too

Scanning was hard-wired to the default graph, so every `GRAPH` query fell back. The scope is
now carried **per pattern** rather than per plan, which is what lets a selective pattern in
the default graph drive a scan of a named one — and lets `GRAPH ?g { ?s :p ?o . ?s :q ?r }`
scan one graph for the second pattern instead of all of them, once the first has bound `?g`.

Getting this wrong is the worst failure available to a store: answering against the wrong
graph looks exactly like answering, and under §14 a named graph is a unit of access policy, so
a misdirected scan would be a disclosure rather than a mistake. Eight differential tests, and
four of five mutations caught by a named test — the fifth narrows a scan that the binding
check would reject anyway, so it costs time rather than correctness, and the comment says so.

Subqueries are spliced in when their projection **hides nothing**. A variable a subquery binds
and does not project is invisible outside it, and flattening would expose it to join against
an outer variable that happens to share the name:

```sparql
SELECT ?s ?o WHERE { { SELECT ?s WHERE { ?s ex:p ?o } } ?s ex:q ?o }
```

Flattened, the two `?o`s become one and the query demands a subject's `ex:p` and `ex:q` values
coincide — so it answers nothing where the right answer has rows. That query is refused, and
the refusal is pinned by a test that checks the *answer*, not just the refusal. A subquery
carrying its own `LIMIT`, `DISTINCT` or `ORDER BY` arrives wrapped in a `Slice` and is refused
too: those change how many solutions there are, which no join does.

Widening the subquery case means renaming hidden variables before splicing, at which point a
collision cannot arise. That is a real fix rather than a wider guess, and it is not done here.

### `OPTIONAL` joins the bind join's fragment

The fragment was `SELECT` over BGP, `JOIN`, `UNION`, `VALUES` and `FILTER`. `OPTIONAL` is the
commonest construct outside it, so real queries fell back to the evaluator and paid the
measured 3× that bad join ordering costs.

| quads | evaluator | bind join |
|---:|---:|---:|
| 41,000 | 0.411 ms | **0.058 ms** |
| 410,000 | 2.698 ms | **0.071 ms** |

`OPTIONAL` is the construct that does not compose, so most of the work is in what the operator
*refuses*. A left join neither commutes nor associates with a join: hoisting one past a
required pattern turns `(A ⟕ B) ⋈ C` into `(A ⋈ C) ⟕ B`, and those agree only when nothing
outside the optional reads a variable only it binds. That is the well-designedness condition,
and a query failing it is declined rather than answered differently.

The refusal is deliberately wider than that condition. Flattening loses the difference between
`A OPTIONAL{B} OPTIONAL{C}`, where `C` really does see what `B` bound, and
`{A OPTIONAL{B}} . {C OPTIONAL{D}}`, where `D` does not — both arrive as one list of items.
Rather than reconstruct the nesting, any optional whose fresh variables something else reads
goes to the evaluator.

Twelve differential tests, each comparing the operator against the evaluator over the same
store, because a wrong left join returns rows — just not the right ones. Five of six mutations
are caught by a named test; the sixth is prevented twice over, by the ordering guard and by the
cost estimate, and the comments say so rather than implying either is load-bearing alone.

One bug the tests found: `evaluate` seeded its pending-filter set with *every* filter, so an
optional's own condition was applied at the outer level. `OPTIONAL { ?s :city ?c FILTER(?age <
35) }` then dropped the row for a person over 35 instead of leaving them with no city.

### The last two mislabelled results

`service5` was the only test in any suite the differential rig blamed on HOLOS rather than
upstream. It is `SERVICE ?service` — a variable endpoint — and both HOLOS and the reference
evaluator raise the same error on it: *"the variable encoding the service name is unbound"*.
The rig collected one side, propagated its error, and reported a divergence between two runs
that had done the identical thing.

Failing the same way is the most important form of agreement a differential rig can observe,
and it was the one case it could not see. Both sides are collected before either is
inspected now, and identical errors count as agreement. **SPARQL 1.1 reaches 512/512** with
no failures, and the shared limitation is filed where it belongs.

Forty-seven protocol tests were skipped as *"not implemented yet"*. They are implemented, and
passing: the SPARQL Protocol suite is 34/34 and Graph Store Protocol 13/13, and the counts
match the skips exactly, because `manifest-all.ttl` includes the sub-manifests those suites
run directly. The skip now says where they ran rather than claiming they did not. Only
`ServiceDescriptionTest` and `CSVResultFormatTest` — three each — are genuinely unimplemented.

With that, every remaining line in every suite is accounted for and says something true:
44 RDF/XML parser failures and 33 evaluator differences upstream in `oxrdf` and `spargebra`,
five parser bugs likewise, 34 OWL and RIF entailment tests out of scope, two `rdf:XMLLiteral`
canonicalisations declined by name, and six protocol features not built.

### `rdf:JSON` canonicalisation

RDF 1.2 gives `rdf:JSON` a value space of JSON *values* rather than of the text spelling them,
so `{ "a":0, "b":1 }` and `{ "b":1, "a":0 }` are one literal and `[ -0, 0 ]` and `[ 0, -0 ]`
are two. `holos_conformance::json` parses and re-emits in a form where equal values are equal
strings, and the seven `rdf:JSON` tests move from skipped to passing.

Hand-rolled rather than a dependency, for one reason that outweighs the hundred lines: the
number rule is not the usual one. JSON numbers denote IEEE 754 doubles, and this has to keep
`-0` apart from `0` while making `1E400` and `1E401` identical — both are `+Infinity` — and
`9007199254740992.5` identical to `9007199254740991.5`, which round to one double. A
serialiser that prints numbers back as decimal gets all four wrong. Keying on the *bits* gets
all four right, and it is the same trick already used for `xsd:double`.

**RDF 1.2 reaches 1382/1405.** The single remaining skip in each RDF suite is
`rdf:XMLLiteral`, whose canonical form is XML C14N — a much larger job than this one, and
declined by name rather than approximated.

### The SPARQL suites' `upstream:` labels, audited

Forty-five skips read *"HOLOS agrees with the reference dataset, so the evaluator differs"*.
Three groups of them were the harness's own gaps, and one of those hid a real defect.

**`mf:resultCardinality mf:LaxCardinality`** was read zero times. `REDUCED` may return any
cardinality between one per distinct solution and the whole multiset; a fixture can only show
one permitted answer, and comparing multiplicities against it failed a conformant engine. Two
tests, and the manifest said so in as many words.

**`FROM` and `FROM NAMED` were never loaded.** In the dataset suite the action carries a
query and nothing else — the clauses name files beside it. Without loading them the query ran
against an empty dataset, and the differential rig then filed the result as upstream because
the reference evaluator, given the same nothing, agreed. The engine's own `FROM` handling
turned out to be correct in all four cases once it had data.

**Update results were compared by blank-node label**, which the skip text admitted rather than
fixed. `compare_datasets` already does isomorphism; the update path now uses it.

| | Before | After |
|---|---:|---:|
| SPARQL 1.0 | 262/263, 20 skipped | **275/276**, 7 skipped |
| SPARQL 1.2 | 262/266, 3 skipped | **265/269**, 0 skipped |

What the audit did *not* find: the remaining sparql11 group is upstream. Almost all
of it is numeric lexical form — `"1.0"^^xsd:decimal` against `"1"^^xsd:decimal`, `"3.0E4"`
against `"30000"` — and the suite compares RDF terms, so those really are evaluator
differences rather than comparison bugs.

### Blank nodes were shared between documents

Loading the dataset suite's files exposed this. `bulk_load` kept the blank node labels the
parser produced, so `_:a` in two documents became **one** node:

```
distinct subjects across the two documents: 1
```

RDF scopes a blank node label to the file it is written in. Merging them asserts an identity
neither document stated, out of nothing but a coincidence of spelling — and `_:a`, `_:b0` and
`_:genid1` are what every serialiser reaches for first, so the coincidence is the common case.
`holos load a.ttl && holos load b.ttl` was enough to hit it.

Renamed on the way in now, for both loaders. The W3C suite tests it directly: `dataset-09b`
joins a default graph against a named one over two files of blank-node subjects, and the
answer is no rows *because* those subjects are different nodes. It had been passing
vacuously, against data the harness never loaded.

### Datatype entailment, and a reasoner bug it exposed

The RDF suites' remaining skips were all datatype entailment. Two halves, both now decided:

- **Value spaces.** For a datatype the test says the recogniser knows, two literals denoting
  one value are interchangeable. Both graphs are canonicalised before the instance check, so
  the search stays a comparison of terms. The integer family and `xsd:decimal` share a value
  space and canonicalise into it together; `xsd:float` and `xsd:double` are keyed by their
  IEEE *bits*, which gets three things right that `==` gets wrong — `+0` and `-0` stay
  distinct, two lexical forms that round to one binary value become identical, and `NaN` is
  identical to itself.
- **Consistency**, which is what `mf:result false` asserts. An ill-formed literal of a
  recognised datatype makes a graph unsatisfiable, and so does a range clash between two
  disjoint value spaces. Both need the datatype to be *recognised* — that is the only
  difference between `datatypes-non-well-formed-literal-1` and `-2`, which share a premise
  and disagree.

| | Before | After |
|---|---:|---:|
| RDF 1.1 | 995/1016, 25 skipped | **1019/1040**, 1 skipped |
| RDF 1.2 | 1349/1372, 34 skipped | **1375/1398**, 8 skipped |

Every failure in both suites is still an RDF/XML parser test. The nine remaining skips are
`rdf:JSON` and `rdf:XMLLiteral`, whose canonical forms need parsers this does not have —
declined by name, because leaving them lexical would answer the negative tests right by
accident and the positive ones wrong.

Two things had to be *stopped* rather than added. `" 3 "^^xsd:int` is ill-formed, not a
spelling of 3, and trimming it made a well-formed literal equal to a malformed one. And the
canonicaliser has to reach inside triple terms, because `opaque-literal` and
`malformed-literal` both state their whole claim in one.

### `holos entail` could make a store unreadable

Running the range clash through the reasoner turned up a shipped bug. rdfs3 says
`ex:age rdfs:range xsd:integer` with `ex:alice ex:age 30` entails that 30 is an integer —
and RDF cannot write that down, because a subject is an IRI or a blank node. The reasoner
wrote it anyway; `insert_encoded` does not validate, so the quad went in and came back out as
a decode error. The most ordinary schema statement there is made a store unreadable.

The guard is `TermId::can_be_subject`, which is a named predicate for a reason: *"is it a
literal"* is the wrong question and gets the wrong answer. Five tags carry literals —
`Literal` for the dictionary-backed ones and `Integer`, `Float`, `DateTime` and `Small` for
the inline codecs — so the obvious check against `Tag::Literal` passes every inline literal
straight through, which is exactly what the first version of this fix did.

### The adapted engine's graph takes a delta

`DESIGN.md` §8 said closing this was bounded work, and the measurement said it was worth
doing: re-bridging the store cost **149 ms at 250,000 quads**, on every commit, growing with
the store rather than with the change.

`Graph::apply` merges into the three sorted permutations in place — a binary search and a
memmove per row per index, or a rebuild past a threshold, because a two-triple tick and a
bulk load do not want the same strategy. `EngineRun::apply` translates a store delta into it,
interning terms the bridge has not seen.

| quads | prepare (re-bridge) | `apply` (delta) |
|---:|---:|---:|
| 5,018 | 2.44 ms | **0.0007 ms** |
| 50,018 | 30.9 ms | **0.0009 ms** |
| 250,018 | 149.3 ms | **0.0009 ms** |

The constant is the result, not the ratio: `apply` does not move between 5,000 quads and
250,000.

Correctness is checked by equality with the thing it replaces — after any delta the updated
run must report exactly what a freshly bridged one reports, up to blank-node isomorphism,
including across a run of twenty-five interleaved additions and removals. A validator that is
fast and slightly stale is worse than a slow one, because its answer is trusted.

A change that alters a *shape definition* is refused rather than absorbed, since shapes are
compiled once at `prepare`. The test for that is narrower than "did the shapes graph change",
which is useless when shapes and data share a graph and every data write is therefore a write
to the shapes graph: a triple whose predicate is not SHACL vocabulary, not `rdf:first`,
`rdf:rest`, and not an `rdf:type` naming a SHACL class or `rdfs:Class`, cannot have defined a
shape.

### …and knows what a delta made stale

The other half. `Shapes` in the engine now carries a dependency index — which shapes read
which predicate, which target which class, which contain which — and `EngineRun::revalidate`
plans from it. The algorithm is the native evaluator's, mirrored rather than reinvented,
because a planner that misses a dependency admits a violation.

| quads | prepare + full validate | engine revalidate | native revalidate |
|---:|---:|---:|---:|
| 5,018 | 3.55 ms | **0.0072 ms** | 0.0072 ms |
| 50,018 | 27.0 ms | **0.0210 ms** | 0.0522 ms |
| 250,018 | 150 ms | **0.0845 ms** | 1.01 ms |

A `sh:sparql` constraint's dependencies live inside query text, so `sparql::predicates` walks
the parsed algebra for them — the parse already happened at compile time. A query that uses a
variable as a predicate genuinely reads everything, and such a shape is recorded as
unconditional and forces a full run rather than being guessed at.

The engine's revalidation is now faster than the native evaluator's at scale, which reverses
what the two were built for: the engine computes focus nodes from three flat sorted arrays
where the native one scans the store. The write path no longer trades coverage for speed.

### A soundness gap in the *shipped* incremental validator

Writing the engine's version surfaced a bug in the native evaluator that has been there all
along. A compound path faults the node whose path runs *through* the change:

```
sh:path ( ex:knows ex:name )     # ex:alice knows ex:bob
ex:bob ex:name 7 .               # writes to Bob, faults Alice
```

`ex:alice` appears in no changed quad. Both planners attributed a change to the endpoints of
the changed quad and widened a shape to all its focus nodes only when *nothing* survived that
filter — and here something did, the violation at Bob's own name, so the widening never fired
and the violation at Alice was never looked for. A full validation reported two violations;
revalidating the same change reported one. The Boundary would have admitted it.

Fixed in both: a shape whose path is anything other than a single predicate is widened
whenever it is implicated, not only when it is left with nothing to do.

The existing safety test could not have caught this. It requires every violation a full run
finds *at a focus node the change touched* to be found incrementally — and the whole point of
this case is a focus node the change does not touch. Both validators now have a test with no
touched-node filter to hide behind, and both fail without the fix.

### Coverage gaps a mutation audit found

Passing tests are not evidence that a rule is checked. Breaking each rule deliberately and
seeing whether anything notices is, and it found three rules with no witness at all —
`close_transitively` on the class hierarchy could be **deleted outright** and all 587 tests
still passed, because the rules that consume the closed hierarchy reach the same conclusions
through the fixpoint. What silently disappeared was `A rdfs:subClassOf C`, which only a query
about the schema would miss.

Every RDFS rule now has a test that fails when that rule is removed: rdfs2, 3, 5, 6, 7, 9,
10, 11, 12 and the `rdf:reifies` range axiom, ten mutations, ten named tests. A comment on
the rdfs9 test claiming it exercised rdfs11 is corrected — it does not, and the audit is how
that was established.

The entailment checker decides ninety-odd conformance tests and had none of its own; removing
its binding-consistency check cost only four of them. It has twelve now, covering
generalisation, one blank node not denoting two things, distinct blank nodes being allowed to
denote one, backtracking, blank nodes nested in triple terms, and the boundary between the
regimes. Its fact iteration is sorted, so the search visits candidates in the same order every
run — the answer never depended on that, but a test for backtracking does, and the first
version of that test passed on a hash seed rather than on the property.

The deterministic-diagnostics fix had been verified by hand and by nothing else. Three tests
pin it, one per comparison. The third turned out never to have been broken — it dumps into a
`BTreeSet`, which is ordered — so its comment claiming the sort provided the determinism is
corrected, and its test pins the observable property instead: it fails if the container is
ever swapped for an unordered one, which is how it would really break.

### The RDF entailment suites are run as entailment

All 134 failures in the RDF 1.1 and 1.2 suites were labelled `upstream:`. 92 of them were
not: the `rdf-mt` and `rdf-semantics` directories are `mf:PositiveEntailmentTest` and
`mf:NegativeEntailmentTest`, where `mf:action` is a *premise* and `mf:result` is a
*conclusion*. The generic runner parsed the premise, round-tripped it through the store, and
compared it against the conclusion as though that were the expected parse — a comparison
that was never going to hold, whose failure was then attributed to a parser for answering a
question nobody asked it.

`holos_conformance::entailment` decides them properly. `G ⊨ E` holds when some instance of
`E` is a subgraph of the closure of `G`, so the check is a subgraph homomorphism with blank
nodes as the variables, run over the closure. Generalisation falls out of the same search:
`ex:a ex:b "10"` entails `ex:a ex:b _:x` because a blank node in the conclusion is already
free to match any term. The RDFS closure is computed by `holos_engine::entailment` rather
than by a second reasoner written to pass the suite.

| | Before | After |
|---|---:|---:|
| RDF 1.1 | 987/1041, 54 failed | **995/1016**, 21 failed |
| RDF 1.2 | 1326/1406, 80 failed | **1349/1372**, 23 failed |

**Every remaining failure in both suites is an RDF/XML parser test** — genuinely upstream,
and now the only thing wearing that label. What is skipped is named: datatype entailment,
which decides both `"010"^^xsd:integer` = `"10"^^xsd:integer` and whether an ill-formed
literal makes a graph inconsistent, is not implemented and is not half-implemented.

Four gaps surfaced along the way:

- The instance check compared triple terms whole, so `<<( :a :b :c )>>` did not match
  `<<( _:x :b :c )>>`. RDF 1.2 lets a blank node stand for a term *inside* a triple term, so
  the match has to recurse — which also keeps one blank node consistent across the two
  places it can appear. Eight tests.
- **rdfs12** was missing: every `rdf:_n` is a sub-property of `rdfs:member`, which is how
  `<a> rdf:_1 <b>` comes to entail `<a> rdfs:member <b>`. RDFS states this as one axiom per
  `n`, infinitely many; only those for an `rdf:_n` the graph mentions are produced, since the
  rest entail nothing about anything in it.
- `rdf:reifies` has range `rdfs:Proposition`, an RDF 1.2 axiom rdfs3 then acts on.
- A triple term denotes a proposition — but that cannot be *written*, because RDF admits a
  triple term only in object position and the triple saying it has one as its subject. The
  entailment is `s p _:b . _:b rdf:type rdfs:Proposition`, so it is a construction on the
  graph under test rather than a rule in the reasoner: materialising it would put blank nodes
  nobody asserted into a store.

### The write path fails closed

`DESIGN.md` §9 makes the native evaluator the validator that gates a commit, and §8 records
that it covers SHACL Core while the adapted engine covers more. The dangerous reading of that
was "it checks less". The true one was "it checks less and says nothing":

```
NATIVE  conforms=true   results=0      # the validator that gates a commit
ENGINE  conforms=false  results=1      # a full run, same store, same shapes
```

A shapes graph carrying `sh:sparql` compiled without complaint, the constraint was dropped,
and the Boundary would have admitted data a full validation rejects. A gate that fails open
is worse than no gate, because it is trusted.

The compiler now refuses a shape carrying a SHACL construct it cannot evaluate, naming the
construct and pointing at `engine::EngineRun`, which implements it. The check is an
**allowlist** of the properties this evaluator understands, not a blocklist of ones to
reject — a blocklist goes stale the moment SHACL grows a construct, and the failure of a
stale blocklist is exactly the silence this fixes. Presentation properties (`sh:name`,
`sh:description`, `sh:order`, `sh:group`, `sh:defaultValue`) are on the list because ignoring
them ignores nothing.

The trade this leaves — `EngineRun` for coverage, the native evaluator for incremental
revalidation — is unchanged, but it is now visible rather than silent. Closing it properly
means giving the engine's graph a merge-in-place, which §8 already describes.

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
