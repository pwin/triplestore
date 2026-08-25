# Provenance

This crate is an adaptation of **[pwin/SHACL_Engine](https://github.com/pwin/SHACL_Engine)**,
vendored from its `shacl` crate.

| | |
|---|---|
| Upstream | https://github.com/pwin/SHACL_Engine |
| Commit | `874e6e57254817394b548eac2f977478f69f1700` |
| Date | 2026-08-19 |
| Licence | MIT OR Apache-2.0 — the same terms as this workspace |

The original `LICENSE-MIT` and `LICENSE-APACHE` are kept alongside this file. Copyright
remains with the upstream authors.

## Why it is vendored rather than depended on

`DESIGN.md` §8 plans to take the SHACL_Engine design and change exactly one thing: the
validator should read the store's own dictionary and indexes instead of loading a private
copy. That change is not expressible from outside the crate — it means giving the engine a
second way to acquire a graph, and adding an entry point that validates a *chosen set of
focus nodes* so incremental revalidation can drive it. Both are edits to the engine, so the
engine lives here.

## What was changed

Every deviation from upstream is listed here, so the two can be compared and so a future
upstream bump has a checklist.

| Change | Why |
|---|---|
| Crate renamed `shacl` → `holos-shacl-engine` | Avoids colliding with the upstream crate name |
| `parallel` feature off by default | HOLOS feeds the engine from a populated store, not from files, so the parallel Turtle parser is dead weight in the common path. Still available. |
| Added `bridge` module | Builds the engine's `Graph` and `TermStore` straight from a `holos_store::Store`. This is the §8 change: no parse, no second copy of the source text. |
| Added `validate::validate_nodes` | Validates a chosen set of (shape, focus node) pairs. Upstream validates every target; incremental revalidation needs to validate a subset. |
| `report::to_oxrdf` scopes each result's copy of a property path | Upstream renders one shared blank node for a compound `sh:resultPath` across every result that reports it. The W3C expected reports give each result its own copy, so under isomorphism comparison every complex-path test failed on report structure rather than on the violation. |

## What was *not* changed

The constraint evaluation, the shape compiler, the path engine, node expressions, SHACL-AF
rules, the SPARQL adapter and inference are upstream's, unmodified — as is the report
writer apart from the one path-scoping change listed above. They
are the reason this crate is here: 418/426 W3C conformance is a great deal of careful work
that would be foolish to rewrite.
