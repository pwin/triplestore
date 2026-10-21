# Provenance

This crate began as a copy of the `shacl` crate from
**[pwin/SHACL_Engine](https://github.com/pwin/SHACL_Engine)** and has diverged
substantially since. It is no longer a patched copy of upstream and must not be
treated as one.

| | |
|---|---|
| Upstream | https://github.com/pwin/SHACL_Engine |
| Measured against | `986c31ba4e071560d51fed36e99abd8cb3747a87` (v0.3.4, 2026-10-21) |
| Originally taken from | `874e6e57254817394b548eac2f977478f69f1700` (2026-08-19) |
| Licence | MIT OR Apache-2.0 — the same terms as this workspace |

The original `LICENSE-MIT` and `LICENSE-APACHE` are kept alongside this file,
and copyright in the inherited code remains with the upstream authors.

## Read this before planning an upstream bump

An earlier version of this file described the crate as upstream `874e6e5` plus
two edits, named `grep -rn "HOLOS change:" src/` as the authoritative list of
deviations, and said that constraint evaluation, the shape compiler, node
expressions, SHACL-AF rules, the SPARQL adapter and inference were upstream's
and unmodified.

All of that was wrong, and wrong in the direction that causes damage: it invited
a wholesale sync.

There are two `HOLOS change:` markers in the source and **both describe edits
that are now upstream**. The grep returns the two things that have *converged*
and finds none of the thousands of lines that actually differ.

## Measured divergence

Line counts and differing lines, `diff` against v0.3.4, counting changed lines
on both sides:

| file | here | upstream | differing |
|---|---:|---:|---:|
| `sparql.rs` | 1115 | 1242 | 2357 |
| `report.rs` | 914 | 897 | 1811 |
| `model/graph.rs` | 467 | 334 | 801 |
| `model/vocab.rs` | 378 | 396 | 774 |
| `shapes.rs` | 1792 | 1623 | 635 |
| `validate.rs` | 1547 | 1834 | 401 |
| `model/interner.rs` | 144 | 177 | 321 |
| `model/mod.rs` | 120 | 121 | 241 |
| `inference.rs` | 396 | 563 | 175 |
| `model/term.rs` | 676 | 774 | 116 |
| `nodeexpr.rs` | 986 | 1070 | 84 |

In `sparql.rs`, `report.rs`, `model/graph.rs` and `model/vocab.rs` the differing
lines outnumber the lines in either copy. Those are rewrites. A three-way merge
on them is not a merge, it is a rewrite with extra steps.

Seven files are byte-identical to upstream: `datatypes.rs`, `error.rs`,
`lib.rs`, `model/loader.rs`, `path.rs`, `rules.rs`, `valueset.rs`. Those are the
only ones where taking upstream wholesale is safe by inspection.

`model/index.rs` exists upstream and not here. It caches a parsed graph on disk,
which has no use in a store that already holds the graph.

## The two documented edits are now upstream

| change | state |
|---|---|
| `validate::validate_nodes`, validating a chosen set of (shape, focus node) pairs | taken upstream as-is |
| `report::to_oxrdf` giving each result its own copy of a compound property path | taken upstream with it |

Upstream also arrived independently at several things done here:
`sh:conformanceDisallows` with `[Violation, Warning, Info]` as the default,
`sh:reificationRequired`, and the same shapes for the `Pattern` and
`ReifierShape` constraint variants. Convergence rather than inheritance, which
matters when reading a diff: a hunk that looks like a local edit may be
upstream's own answer to the same problem.

## Build differences

| difference | where | why |
|---|---|---|
| crate renamed `shacl` → `holos-shacl-engine` | `Cargo.toml` | avoids colliding with the upstream crate name. On crates.io `shacl` belongs to an unrelated project, so upstream does not publish there either |
| `parallel` off by default | `Cargo.toml` | the engine is fed from a populated store, not from files, so the parallel Turtle parser is dead weight on the common path. Still available behind the feature |
| no `rdf-12` feature | `Cargo.toml` | upstream made RDF 1.2 a default-on feature in 0.3.3 so an RDF 1.1 embedder can switch it off. Here it is hardwired on through `oxttl`'s own feature |
| `edition` and dependency versions given literally | `Cargo.toml` | upstream inherits them from its workspace, which this tree is not part of |
| no `mimalloc` | `Cargo.toml` | upstream's allocator choice; this workspace makes its own |

## Correctness fixes upstream has and this copy does not

All four were found upstream by running the full W3C suites rather than the two
Core manifests tracked here, and each is verified absent from this `src/` as of
v0.3.4.

- **`{?var}` and `{$var}` substitution** in the `sh:message` of a SPARQL
  constraint or SPARQL-based component. Here the placeholder reaches the report
  verbatim.
- **Prefix declarations with no `sh:prefixes`** taken only from nodes typed
  `sh:ShapesGraph`. Taking every `sh:declare` in the graph can pick the wrong
  declaration; the suite's `prefixes-002` plants one to catch exactly that.
- **`sh:values` with `sh:select` and `sh:sparqlExpr`** node expressions.
- **The SHACL 5.3.2 subquery rule**, with a caveat that matters more than the
  fix. The rule as the specification words it refuses any subquery in a
  pre-bound query that does not project `$this`. Implemented that way upstream,
  it reached 426/426 on the W3C suites and broke three graph-wide aggregate
  checks in the consolidated ontology suite — subqueries that count across the
  graph and have no reason to mention the focus node. Upstream narrowed it in
  0.3.1 to refuse only a subquery that *uses* `$this` without projecting it, and
  accepts 425/426 as the price. **Port the narrowed version.** The
  specification's rule is phrased for an engine that supplies initial bindings;
  both engines substitute into the algebra, so the projection requirement does
  not carry across unchanged.

Port them one at a time, each against the ratchet below. Do not sync the files
wholesale — see the divergence table.

## What the conformance numbers actually say

This workspace measures the two Core manifests, 236 tests, running both
validators against the same expectations:

| suite | native evaluator | adapted engine |
|---|---:|---:|
| SHACL 1.0 Core | 98/98 | 98/98 |
| SHACL 1.2 Core | 138/138 | 138/138 |

An earlier version of this file justified carrying the adapted engine on
coverage: *127/138 on SHACL 1.2 Core against the native evaluator's 94*. **That
gap has closed.** Both validators now pass every Core test, so on these suites
the adapted engine buys no coverage, and the coverage argument for vendoring it
should not be repeated.

What remains is the architectural reason below, and the limits of these suites.
236 Core tests are a subset of the 426 upstream runs across the full SHACL 1.0
and 1.2 suites; SHACL-AF rules, node expressions and SPARQL-based constraints
are covered thinly here or not at all, and upstream sits at 425/426 with the one
exception described above. Nothing in this tree measures that, so "the adapted
engine handles more than Core" is plausible and, here, unevidenced. Anyone
relying on it should extend the ratchet rather than cite this file.

## Why it is adapted in-tree rather than depended on

`DESIGN.md` §8 takes the SHACL_Engine design and changes one thing: the
validator reads the store's own dictionary and indexes instead of loading a
private copy. That is not expressible from outside the crate — it needs a second
way to acquire a graph, and an entry point that validates a chosen set of focus
nodes so incremental revalidation can drive it.

The focus-node entry point is no longer a reason on its own, since upstream took
it. Reading the store's dictionary directly is, and it is what `model/graph.rs`
and `model/interner.rs` are rewritten for.

The bridge is not in this crate. [`holos-shacl`](../holos-shacl/src/bridge.rs)
holds it, and builds this crate's `Graph` and `TermStore` from a
`holos_store::Store` through its public API.

## Keeping in step with upstream

1. The divergence table above, re-measured. `diff -u` against a checkout of the
   upstream tag is the only reliable account. The `HOLOS change:` markers are
   not one: there are two and both are stale.
2. The build differences above.
3. `cargo test -p holos-conformance shacl` — four ratchets in
   [`conformance/`](../../conformance/), native and adapted. All four currently
   record no known failures, so a regression appears as a new line rather than a
   changed number.
4. Re-measure before quoting. Every figure in the previous version of this file
   was wrong within two months, including the ones that were right when written.
