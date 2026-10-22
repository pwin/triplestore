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
on both sides, re-measured after the SHACL-SPARQL ports below:

| file | here | upstream | differing |
|---|---:|---:|---:|
| `sparql.rs` | 1249 | 1242 | 2491 |
| `report.rs` | 914 | 897 | 1811 |
| `model/graph.rs` | 467 | 334 | 801 |
| `model/vocab.rs` | 380 | 396 | 776 |
| `shapes.rs` | 1798 | 1623 | 631 |
| `validate.rs` | 1723 | 1834 | 297 |
| `model/interner.rs` | 144 | 177 | 321 |
| `model/mod.rs` | 120 | 121 | 241 |
| `inference.rs` | 396 | 563 | 175 |
| `model/term.rs` | 676 | 774 | 116 |
| `nodeexpr.rs` | 1071 | 1070 | 1 |

In `sparql.rs`, `report.rs`, `model/graph.rs` and `model/vocab.rs` the differing
lines outnumber the lines in either copy. Those are rewrites. A three-way merge
on them is not a merge, it is a rewrite with extra steps.

Seven files are byte-identical to upstream: `datatypes.rs`, `error.rs`,
`lib.rs`, `model/loader.rs`, `path.rs`, `rules.rs`, `valueset.rs`. Those are the
only ones where taking upstream wholesale is safe by inspection.
`nodeexpr.rs` differs from upstream by one line now, the comment naming the
upstream commit its SPARQL node expressions came from.

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

## Correctness fixes taken from upstream

Upstream found these by running the full W3C suites rather than the two Core
manifests this file used to describe. Each was ported on its own, against the
SHACL-SPARQL ratchets below, and each port says which upstream commit it came
from in a comment at the site.

| fix | upstream | tests it passes |
|---|---|---|
| no `sh:sourceConstraint` on a SPARQL-based component's results | 2006c4e | `optional-001`, `propertyValidator-select-001`, `validator-001` (1.0) |
| `{?var}` and `{$var}` substituted in a SPARQL constraint's `sh:message` | 2006c4e | `propertyValidator-select-001` (1.2) |
| prefixes with no `sh:prefixes` only from nodes typed `sh:ShapesGraph` | d7769bf | `prefixes-002` |
| `sh:values`, with the `sh:select` and `sh:sparqlExpr` node expressions | d7769bf | `property-select-001`, `property-sparqlExpr-001` |
| a subquery that uses `$this` must return it (SHACL 5.3.2, narrowed) | d7769bf, 1944802 | `pre-binding-006` |

The list kept here before named four of these. The first was not on it: it
shows only when reports are compared as the suite compares them, which this
harness did not do until the ratchets below were added.

The subquery rule is the **narrowed** version. As the specification words it,
the rule refuses any subquery in a pre-bound query that does not project
`$this`. Implemented that way upstream, it broke three graph-wide aggregate
checks in the consolidated ontology suite — subqueries that count across the
graph and have no reason to mention the focus node. Upstream narrowed it in
0.3.1 to refuse only a subquery that *uses* `$this` without projecting it, and
accepts `unsupported-sparql-004` as the price. Both engines substitute into the
algebra rather than supply initial bindings, so the projection requirement does
not carry across unchanged. `sparql::tests::a_subquery_must_return_this_only_if_it_uses_it`
pins both directions.

Not taken, because they had already converged here: `sh:pattern` results
without `sh:sourceConstraint`, `sh:nodeByExpression` results with it, messages
and severities annotated onto a constraint triple, and
`sh:reificationRequired`. Do not sync the files wholesale — see the divergence
table.

## What the conformance numbers actually say

This workspace measures the two Core manifests, 236 tests, running both
validators against the same expectations, and the SHACL-SPARQL sections through
the adapted engine only, since the native evaluator does not speak SHACL-SPARQL:

| suite | native evaluator | adapted engine |
|---|---:|---:|
| SHACL 1.0 Core | 98/98 | 98/98 |
| SHACL 1.2 Core | 138/138 | 138/138 |
| SHACL 1.0 `sparql` | — | 21/22 |
| SHACL 1.2 `sparql` | — | 23/26 |
| SHACL 1.2 `node-expr` (`sht:Validate` entries) | — | 2/2 |

The SHACL 1.0 failure is `unsupported-sparql-004`, described above. The three
SHACL 1.2 failures are `sparql/functions/`: SHACL 1.2's SPARQL functions, which
neither this engine nor upstream implements; upstream's copy of the suite
predates them.

An earlier version of this file justified carrying the adapted engine on
coverage: *127/138 on SHACL 1.2 Core against the native evaluator's 94*. **That
gap has closed.** Both validators now pass every Core test, so on these suites
the adapted engine buys no coverage, and the coverage argument for vendoring it
should not be repeated.

What remains is the architectural reason below, and the limits of these suites.
Upstream also runs the node-expression evaluation tests in `node-expr/` that are
not `sht:Validate` entries, and the SHACL-AF rules suite; neither is measured
here. SHACL-SPARQL now is, and that is where the adapted engine covers what the
native evaluator does not: 44 of 48 tests, against none.

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
3. `cargo test -p holos-conformance shacl` — seven ratchets in
   [`conformance/`](../../conformance/): Core native and adapted, and the
   SHACL-SPARQL and node-expression sections adapted. The Core four record no
   known failures, so a regression appears as a new line rather than a changed
   number. `HOLOS_CONFORMANCE_FULL_DIFF=1` prints every differing triple.
4. Re-measure before quoting. Every figure in the previous version of this file
   was wrong within two months, including the ones that were right when written.
