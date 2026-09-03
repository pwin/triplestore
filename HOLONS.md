# Holon models

A named graph that enforces its own invariants **on the write path**, derives what follows
from what it was told, and keeps a record of every attempt — accepted or refused.

Read §1 and §2 even if you skip the rest: they say what a holon is and what a commit
actually does. Everything after that is detail, with a worked example you can run.

```
cargo run -p holos-holon --example tour
```

That command is the rest of this document, executing. The files it uses are in
[`examples/holon/`](examples/holon/), and every output quoted below comes from running it.

---

## 1. What a holon is

Three named graphs and a policy, travelling together under one name.

| Part | Graph | What it holds |
|---|---|---|
| **Scene** | `<holon>/scene` | Current state. The thing the holon is responsible for. |
| **Boundary** | `<holon>/boundary` | SHACL shapes and rules. What must be true, and what follows. |
| **Event log** | `<holon>/events` | Append-only. Every commit attempt, with per-triple provenance. |
| **Admission** | — | `Reject`, or `AdmitAndRecord`. What to do with a commit that violates. |

The graph names are derived from the holon's own IRI, so one name gives you all three and the
relationship is legible in any dump:

```rust
let holon = Holon::new(NamedNode::new_unchecked("https://example.org/workorders#orders"));
// scene    https://example.org/workorders#orders/scene
// boundary https://example.org/workorders#orders/boundary
// events   https://example.org/workorders#orders/events
```

**Why bother.** A schema you validate nightly is documentation with a cron job. The scene of
a holon cannot enter a state its boundary forbids, because the check is a precondition of the
write landing — so any reader of the scene may assume the invariants hold, and nobody has to
ask when it was last checked.

That is only affordable if a commit costs the size of its own change rather than the size of
the scene. It does: **165 commits/s against a 300k-triple scene, 41× cheaper than a full
pass** (`DESIGN.md` §16). At full-pass cost a boundary would be a nightly batch job wearing a
transaction's clothes.

## 2. What a tick does

A **tick** is one commit. It takes a **delta** — a set of triples to add and a set to remove —
and runs five steps.

```
1. apply the delta to the scene
2. fire the boundary's rules, to a fixpoint
3. validate the scene against the boundary's shapes
4. write the event
5. (projections are recomputed on read; nothing to do here)
```

Four things about that order are load-bearing.

**Rules run before validation.** What a rule infers is judged by the boundary, exactly like
what the caller wrote. A rule cannot be used to smuggle something past the shapes, and a rule
that infers something forbidden *rejects the commit* rather than persisting quietly. Run
afterwards, a bad inference would already be written when it was found to be wrong.

**Inferences are part of the commit.** They go into the scene alongside the delta, into the
event log as ordinary additions, and out again with everything else if the commit is refused.
A reader asking what changed should not have to know which triples a rule wrote.

**The event is written either way.** A boundary that keeps no record of what it refused
cannot be audited. A refused commit still gets a version number and an event saying what was
attempted and what was wrong with it.

**The whole tick is one atomic commit.** Delta, inferences, event and version bump go into a
single commit scope on the store; on the persistent backend that is one `WriteBatch`, written
once. A process that dies mid-tick leaves the store as it was, rather than with the scene
changed and no event to explain it. What is *not* provided is isolation from concurrent
readers — that needs MVCC, which is not built. The server and the Python binding hold the
engine behind an `RwLock`, so in those deployments the question does not arise.

## 3. Writing a boundary

The boundary is an ordinary SHACL shapes graph. From [`examples/holon/boundary.ttl`](examples/holon/boundary.ttl):

```turtle
wo:WorkOrderShape
    a sh:NodeShape ;
    sh:targetClass wo:WorkOrder ;

    sh:property [
        sh:path wo:status ;
        sh:minCount 1 ;
        sh:maxCount 1 ;
        sh:in ( wo:Raised wo:InProgress wo:Closed ) ;
        sh:message "status must be one of Raised, InProgress, Closed" ;
    ] ;

    sh:property [
        sh:path wo:meterHours ;
        sh:maxCount 1 ;
        sh:datatype xsd:decimal ;
        sh:minInclusive 0 ;
        sh:maxInclusive 200000 ;
    ] .
```

Write `sh:message` on the shapes you expect to fire. Without one the report names the
constraint component, which tells a developer what broke and a user nothing.

### What the boundary refuses to accept

The validator that runs inside a tick is the **native incremental** one — it works on the
store's own indexes and revalidates only the focus nodes a delta touched, which is where the
41× comes from. It does not implement all of SHACL, and where it does not, **it refuses the
shapes graph rather than passing what it cannot check**.

That refusal is deliberate and it is the behaviour you want: a validator that silently
ignored a constraint would report `sh:conforms true` for data it never looked at, and the
boundary would be a boundary with a hole in it that nobody could see.

In practice it means SPARQL-based SHACL — `sh:SPARQLTarget`, `sh:sparql` constraints, node
expressions — belongs in an offline `holos validate` run, not in a boundary. The example
boundary carries a comment where it hit this, and says what it did instead:

```turtle
# A `sh:SPARQLTarget` selecting closed orders directly would read more naturally... It is not
# used, because only the adapted engine implements it and the native incremental validator
# that runs inside a tick *refuses* what it cannot check rather than passing it.
wo:ClosedOrderShape
    a sh:NodeShape ;
    sh:targetClass wo:ClosedOrder ;
    sh:property [ sh:path wo:completedOn ; sh:minCount 1 ;
                  sh:message "a closed work order must record when it was completed" ] .
```

If you want to know in advance, `EngineRun::would_revalidate_incrementally` answers it before
a commit rather than after.

## 4. Writing rules

A `sh:rule` in the boundary fires on every tick, to a fixpoint, before validation.

```turtle
wo:SignOffClosesTheOrder
    a sh:NodeShape ;
    sh:targetClass wo:WorkOrder ;
    sh:rule [
        a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate rdf:type ;
        sh:object wo:ClosedOrder ;
        sh:condition wo:HasSignOff ;
    ] .

wo:HasSignOff
    a sh:NodeShape ;
    sh:property [ sh:path wo:signedOffBy ; sh:minCount 1 ] .
```

> **The first mistake to make here.** A `sh:TripleRule` *adds* a triple; it does not replace
> one. A rule writing `wo:status wo:Closed` onto an order that already says
> `wo:status wo:InProgress` gives it two statuses and trips its own `sh:maxCount 1`. Derive a
> **class** instead — additive, and it says the same thing. The invariant then targets the
> derived class, which is the whole shape of the arrangement: the rule says what something
> *is*, and the boundary says what a thing of that kind must have.

Rules are opt-in per caller, because keeping them costs memory:

```rust
let mut rules = Rules::prepare(&mut engine, &holon)?;   // once
tick_with_rules(&mut engine, &holon, &mut session, &delta, rules.as_mut())?;   // per commit
```

`Rules` holds one bridged copy of the scene and keeps it current by delta. Preparing it per
tick would re-bridge the whole scene each commit, which is the cost that kept rules switched
off in earlier builds. Prepare once, keep it, pass it in. `tick` without the `_with_rules`
skips step 2 entirely.

**Two limits worth knowing.** Rules run to a fixpoint bounded by `Rules::MAX_ROUNDS` (16); a
rule set that has not settled by then fails the tick rather than looping. And inferences are
not *retracted* when the fact that justified them is removed — there is no DRed-style
maintenance on delete yet, so a derived triple outlives its premise until something removes
it explicitly.

## 5. A worked example

`examples/holon/` is a small maintenance work-order model: assets, work orders, a boundary
with one rule and four invariants, and four commits chosen so each shows a different way a
boundary can answer.

### An accepted commit

```
2. A commit the boundary accepts
  raise WO-1002                committed as version 1, 5 triples applied
```

### A refused commit

`wo:Awaiting-Parts` is not in the status vocabulary. Everything else about the order is fine,
which is the point — the tick is refused **whole**, not partially applied.

```
3. A commit the boundary refuses
  raise WO-1003                REFUSED at version 2, 1 violation(s)
        wo:WO-1003 wo:status — status must be one of Raised, InProgress, Closed

  scene held 20 triples before the tick and 20 after
```

Note the version number advanced anyway. Versions count *attempts*, not successes, so an
audit can tell "nothing happened" from "something was tried and turned away".

### A rule firing

The delta records a sign-off, a completion date, and moves the status. It does not say the
order is a `wo:ClosedOrder` — the rule derives that, and the boundary then holds it to what a
closed order must have.

```
4. A rule firing inside a commit
  sign off WO-1001             committed as version 3, 5 triples applied
        wo:WO-1001 rdf:type wo:ClosedOrder        <- inferred
        wo:WO-1001 rdf:type wo:WorkOrder
        wo:WO-1001 wo:asset wo:pump-14
        wo:WO-1001 wo:completedOn "2026-09-03"^^xsd:date
        wo:WO-1001 wo:meterHours "8412.5"^^xsd:decimal
        wo:WO-1001 wo:raisedOn "2026-08-14"^^xsd:date
        wo:WO-1001 wo:signedOffBy wo:eng-mahmoud
        wo:WO-1001 wo:status wo:Closed
        wo:WO-1001 wo:summary "Seal weeping at the drive end."
```

This is also the one commit in the example that **removes** a triple. RDF has no
update-in-place, so moving the status means adding the new statement and removing the old
one; a delta is two sets for exactly this reason.

### A rule causing a refusal

The same sign-off on the order raised in commit 1, with no completion date. The rule derives
`wo:ClosedOrder`; the boundary then sees a closed order that cannot say when it was completed.

```
5. A rule causing a refusal
  sign off WO-1002             REFUSED at version 4, 1 violation(s)
        wo:WO-1002 wo:completedOn — a closed work order must record when it was completed
```

Run the rules *after* validation and this data would already be in the scene.

## 6. Policy, on the write path

A holon does not have its own permission system. It uses the store's, and the store's is
enforced at the index scan — see [ACCESS-CONTROL.md](ACCESS-CONTROL.md).

Every quad a tick writes is checked for `WRITE`, and **that includes what a rule writes**. A
boundary is not a way to write where the principal cannot: rules run inside the caller's
session, and the session is the caller's.

```rust
// A field engineer may update an order, but may not sign one off.
Policy::default()
    .with_rule(Rule::allow(Modes::ALL, Scope::Everything,
                           PrincipalMatch::Role("engineer".into())))
    .with_rule(Rule::deny(Modes::WRITE,
                          Scope::GraphPredicate(scene.clone(), wo("signedOffBy")),
                          PrincipalMatch::Role("engineer".into())))
```

The narrow rule wins over the broad one because it is more specific — a graph-and-predicate
scope beats an everything scope — so this is one rule added to a permissive policy rather
than a policy rewritten from scratch. That is how this composes with an enterprise IdP: map
groups to roles once, then express exceptions as narrow denies.

```
6. Policy, on the write path
  engineer signs off           FAILED: the principal may not write to
                               <https://example.org/workorders#orders/scene>
```

Note that this is a **failure**, not a refusal: a policy denial fails the tick, so there is no
event and no version. A refusal is the boundary doing its job on a commit the principal was
entitled to attempt; a denial means they were not entitled to attempt it. Removal needs
`READ` as well as `WRITE`, or "did the delete land" becomes an oracle for whether hidden data
exists.

## 7. The event log

Every attempt, committed or not, with per-triple provenance in RDF 1.2:

```turtle
_:change rdf:reifies <<( wo:WO-1001 wo:signedOffBy wo:eng-mahmoud )>> ;
         holos:inTick _:tick3 ;
         holos:operation holos:Added .

_:tick3 holos:version 3 ;
        holos:admitted true .
```

It is ordinary RDF in an ordinary named graph, so "who changed this statement, in which
commit, and was it accepted" is a SPARQL query like any other. From the tour:

```
7. What the event log says
  81 triples in the event log. The ticks it records:
        _:tick1 holos:admitted "true"^^xsd:boolean
        _:tick1 holos:version "1"^^xsd:integer
        _:tick2 holos:admitted "false"^^xsd:boolean
        _:tick2 holos:version "2"^^xsd:integer
        _:tick2 holos:violations "1"^^xsd:integer
        ...
```

## 8. Projections

Agents read projections, not the scene. A projection is a registered SPARQL query, and
exposing one instead of the scene is what lets a holon be readable without being writable —
and, where the query runs with more privilege than its readers have, what makes it a
deliberate declassification rather than an accident.

```rust
let holon = Holon::new(id).with_projection(
    wo("OpenOrders"),
    "SELECT ?order ?asset WHERE { GRAPH <...scene> { \
       ?order wo:asset ?asset ; wo:status ?status . FILTER(?status != wo:Closed) } }",
);
```

```
8. Reading through a projection
        wo:WO-1002 on wo:compressor-3
```

WO-1001 is absent because commit 3 closed it. Projections are **recomputed on read** in this
build. `Regime::Maintained` — incrementally maintained from the delta — is *refused* rather
than silently downgraded to recomputation: claiming a guarantee the build does not provide
would be worse than declining it.

## 9. Branching

`holos_holon::branch` forks a holon: the child starts from the parent's scene and boundary,
records where it came from, and then moves independently. Versions continue rather than
restart, so a version number is unique across a lineage.

That is the cheap-fork primitive a "try this change against real data" workflow needs. On the
persistent backend the underlying store-level equivalent is `Store::checkpoint`, a hard-linked
snapshot taken while the store is open — see [OPERATIONS.md](OPERATIONS.md).

## 10. What is not built

Stated here rather than discovered later.

- **No isolation.** Ticks are atomic but a concurrent reader with its own view may see the
  store before or after a commit with no promise about which. Needs MVCC (`DESIGN.md` §6.1).
- **No incremental projections.** Recomputed on read; `Regime::Maintained` is refused.
- **No time travel.** The event log records every change, but there is no `AT VERSION n`
  query surface over it yet.
- **No inference retraction.** A derived triple outlives its premise.
- **No CLI or HTTP surface.** Holons are reachable from the Rust API only. `holos` and
  `holos-server` expose SPARQL, SHACL validation and the Graph Store Protocol, but not ticks.

## 11. Reference

| | |
|---|---|
| Example data | [`examples/holon/`](examples/holon/) |
| Runnable tour | `cargo run -p holos-holon --example tour` |
| Source | [`crates/holos-holon/`](crates/holos-holon/) |
| Design | `DESIGN.md` §9 |
| Access control | [ACCESS-CONTROL.md](ACCESS-CONTROL.md) |
| Operations | [OPERATIONS.md](OPERATIONS.md) |

```rust
use holos_holon::{registry, tick, tick_with_rules, Admission, Delta, Holon, Rules};

let holon = Holon::new(id).with_admission(Admission::Reject);
registry::register(&mut engine, &holon, &mut session)?;      // once
let mut rules = Rules::prepare(&mut engine, &holon)?;        // once, optional

let delta = Delta::adding(triples).remove(old_triple);
let outcome = tick_with_rules(&mut engine, &holon, &mut session, &delta, rules.as_mut())?;

outcome.committed();    // did the boundary admit it
outcome.version;        // the attempt's number, admitted or not
outcome.violations;     // how many
outcome.report;         // the SHACL report, if it did not conform
```
