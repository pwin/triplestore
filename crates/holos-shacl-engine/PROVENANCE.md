# Provenance

This crate is an **adaptation** of **[pwin/SHACL_Engine](https://github.com/pwin/SHACL_Engine)**,
taken from its `shacl` crate and maintained in this tree.

| | |
|---|---|
| Upstream | https://github.com/pwin/SHACL_Engine |
| Commit | `874e6e57254817394b548eac2f977478f69f1700` |
| Date | 2026-08-19 |
| Licence | MIT OR Apache-2.0 — the same terms as this workspace |

The original `LICENSE-MIT` and `LICENSE-APACHE` are kept alongside this file, and copyright
remains with the upstream authors.

## Why it is adapted in-tree rather than depended on

`DESIGN.md` §8 plans to take the SHACL_Engine design and change exactly one thing: the
validator should read the store's own dictionary and indexes instead of loading a private
copy. That is not expressible from outside the crate — it means giving the engine a second
way to acquire a graph, and adding an entry point that validates a *chosen set of focus
nodes* so incremental revalidation can drive it. Both are edits to the engine, so the engine
lives here.

## What was changed

Every deviation from upstream is listed here, so the two can be compared and so a future
upstream bump has a checklist. Each edit inside the source carries a `HOLOS change:`
comment, so `grep -rn "HOLOS change:" src/` is the authoritative list and this table is a
description of it.

| Change | Where | Why |
|---|---|---|
| Crate renamed `shacl` → `holos-shacl-engine` | `Cargo.toml` | Avoids colliding with the upstream crate name |
| `parallel` feature off by default | `Cargo.toml` | HOLOS feeds the engine from a populated store rather than from files, so the parallel Turtle parser is dead weight on the common path. Still available behind the feature |
| Added `validate::validate_nodes` | `src/validate.rs` | Validates a chosen set of (shape, focus node) pairs. Upstream validates every target; incremental revalidation needs a subset |
| `report::to_oxrdf` scopes each result's copy of a property path | `src/report.rs` | Upstream renders one shared blank node for a compound `sh:resultPath` across every result reporting it. The W3C expected reports give each result its own copy, so under isomorphism comparison every complex-path test failed on report *structure* rather than on the violation |

### Correction

An earlier version of this file also listed a `bridge` module as a change to this crate. It
is not one — [`holos-shacl`](../holos-shacl/src/bridge.rs) holds the bridge, and it builds
the engine's `Graph` and `TermStore` from a `holos_store::Store` using only this crate's
public API. Fewer edits to upstream than the record claimed, which is the direction one
would rather be wrong in, but the record was wrong and is now right.

## What was *not* changed

Constraint evaluation, the shape compiler, the path engine, node expressions, SHACL-AF
rules, the SPARQL adapter and inference are upstream's, unmodified — as is the report writer
apart from the one path-scoping change above.

They are the reason this crate is here. Upstream reports 418/426 on the W3C suites; that
figure is upstream's own and is not re-measured here, but the conformance this build does
measure — **127/138** on SHACL 1.2 Core against the native evaluator's 94 — is a great deal
of careful work that would be foolish to rewrite.

## Keeping in step with upstream

1. `grep -rn "HOLOS change:" src/` — the edits inside the source.
2. The `Cargo.toml` differences above.
3. `cargo test -p holos-conformance shacl` — the ratchet catches anything a bump breaks.
