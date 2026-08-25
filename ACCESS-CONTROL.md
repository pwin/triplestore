# Access control

The distinctive thing about this store. Read §1 and §2 even if you skip the rest — they
explain what the guarantee is and why it holds, and everything else is detail.

---

## 1. The guarantee

Policy is enforced **at the index scan**: each quad is decided individually as it comes off
the index, before any SPARQL operator sees it. Not by rewriting the query, and not by
filtering results afterwards.

> **The property that buys:**
> the answer to query *Q* equals the answer *Q* would have over the sub-dataset the
> principal is allowed to see.

That is stronger than it looks, because it holds for **every query shape** without anyone
enumerating them:

| | |
|---|---|
| `COUNT` | Cannot reveal that hidden rows exist — the aggregate counts the visible sub-dataset |
| `FILTER NOT EXISTS` | Cannot probe for hidden data — a withheld quad is genuinely absent, and "absent" is the honest answer |
| `OPTIONAL` | Leaves the variable unbound, exactly as if the quad were not in the store |
| Property paths | A path cannot traverse a withheld edge |
| GeoSPARQL | No exemption: denying `geo:asWKT` makes a spatial join find nothing |
| SPARQL Update | The `WHERE` clause is filtered by read policy on the same path as a `SELECT` |

Two of these are checked directly in the Python test suite
(`test_count_cannot_leak_hidden_rows`, `test_filter_not_exists_cannot_probe_for_hidden_rows`)
precisely because they are where a rewriting or post-filtering design leaks.

### Why the placement matters

An approach that rewrote the query would have to get every operator right, forever, including
operators added later. An approach that filtered results afterwards would already have let
`COUNT` see the hidden rows.

Deciding at the scan means **no operator can route around it, because no operator has another
way to reach the data**. There is exactly one path from a query to the indexes, and it goes
through `DatasetView::internal_quads_for_pattern`, which calls `decide_quad` on every quad.

It costs **8 ns per quad** — measured, and about 1% of what decoding the quad costs.

---

## 2. The three things a decision is made from

```
      Principal              Policy                    Quad
  ┌──────────────┐    ┌──────────────────┐    ┌──────────────────┐
  │ id           │    │ rules            │    │ subject          │
  │ roles        │ ─▶ │ graph labels     │ ◀─ │ predicate        │
  │ attributes   │    │ default effect   │    │ object           │
  │ clearance    │    │ semantics        │    │ graph            │
  └──────────────┘    └──────────────────┘    └──────────────────┘
           │                    │                      │
           └────────────────────┼──────────────────────┘
                                ▼
                      Allow │ Filter │ Fail
```

A **`Session`** binds a principal to a compiled policy. There is no way to construct a
`DatasetView` without one, which is what makes "every read is authorised" a type-level fact
rather than a convention.

---

## 3. Principals

```rust
let alice = Principal::new(NamedNode::new("urn:holos:principal:alice")?)
    .with_role("hr")
    .with_role("finance")
    .with_clearance(Label::level(3));
```

| Field | Meaning |
|---|---|
| `id` | Stable identifier. Conventionally `urn:holos:principal:<issuer>/<subject>` |
| `roles` | Role or group names — from `roles`, `groups`, `cognito:groups`, an LDAP/AD mapping |
| `attributes` | Every other verified claim, for attribute-based rules |
| `clearance` | Optional classification label. Absent means unclassified only |

`Principal::anonymous()` is the unauthenticated principal: no roles, no attributes, no
clearance. It is what every request gets when the server is not started with
`--trust-forwarded-identity`.

### From an identity provider

```rust
let principal = Principal::from_verified_claims(
    "https://login.example.com",   // issuer
    "alice@example.com",           // subject
    &claims,                       // every verified claim
    "groups",                      // which claim holds roles
);
```

Every claim that is not the roles claim becomes an attribute, so attribute rules reach IdP
data **without this crate knowing anything about the IdP's schema**.

> **`from_verified_claims` trusts its input completely, and says so in its name.** Verifying
> the token — signature, issuer, audience, expiry — happens before this call, at the edge.
> See §8.

---

## 4. Rules

A rule grants or refuses a **mode** over a **scope** to a **principal match**.

```rust
Rule::deny(Modes::READ, Scope::Predicate(salary), PrincipalMatch::Everyone)
```

### Modes

| Mode | Covers |
|---|---|
| `READ` | Reading quads |
| `WRITE` | Inserting and deleting quads |
| `VALIDATE` | Running validation, which can reveal shape structure even when the data is hidden |
| `ADMIN` | Changing the policy itself |
| `ALL` | Every mode |

**`ADMIN` is deliberately separate from `WRITE`.** Authority to change the rules is not the
same as authority to change the data, and conflating them is how a data-entry role becomes a
privilege-escalation path.

### Scopes, and their specificity

| Scope | Specificity | Use |
|---|---:|---|
| `Everything` | 0 | The default posture |
| `Graph(g)` | 1 | One named graph — in holon terms, one holon's scene |
| `Predicate(p)` | 2 | One predicate everywhere. The usual way to hide a sensitive column |
| `GraphPredicate(g, p)` | 3 | One predicate within one graph |

### Principal matches

| Match | Selects |
|---|---|
| `Everyone` | Everyone, including anonymous |
| `Role(r)` | Anyone holding that role or group |
| `Attribute { key, value }` | Anyone with that claim value |
| `Identity(iri)` | One specific principal |
| `All(vec![…])` | Everyone every sub-match selects |
| `Any(vec![…])` | Everyone at least one sub-match selects |
| `Not(m)` | Everyone `m` does **not** select |

---

## 5. How conflicts resolve

Two rules in order of precedence:

1. **More specific scope wins.** A `Predicate` rule beats an `Everything` rule.
2. **At equal specificity, deny beats allow.**

Rule 2 is a safe default, and it creates one problem worth understanding.

### Why `Not` exists

The most common shape a real policy takes is *"deny this to everyone **except** role R"*.
With deny-beats-allow, this does not work:

```rust
// WRONG — the deny wins, and HR sees nothing either.
policy
    .with_rule(Rule::deny(Modes::READ, Scope::Predicate(salary), PrincipalMatch::Everyone))
    .with_rule(Rule::allow(Modes::READ, Scope::Predicate(salary), PrincipalMatch::Role("hr")))
```

Both rules are at specificity 2, so the deny wins and HR is locked out along with everyone
else. Specificity cannot express the exception, because the exception is about *who*, not
about *what*.

The negation is what makes it writable:

```rust
// RIGHT — the rule simply does not select HR.
policy.with_rule(Rule::deny(
    Modes::READ,
    Scope::Predicate(salary),
    PrincipalMatch::Not(Box::new(PrincipalMatch::Role("hr".into()))),
))
```

On the command line and in Python this is `--except-role hr` / `except_role="hr"`.

---

## 6. Classification labels

Beyond rules, a graph can carry a lattice label — a **level** plus a set of **compartments**.

```rust
policy.with_graph_label(reviews_graph, Label { level: 3, compartments: {"HR"} })
```

A principal reads a labelled graph only if its clearance **dominates** the label:

```
clearance.level ≥ label.level   AND   label.compartments ⊆ clearance.compartments
```

That is the standard Bell–LaPadula reading. Level alone is a total order; compartments add
need-to-know, so clearance 5 without the `HR` compartment still cannot read `HR` data.

**Clearance is checked first and cannot be overridden by any rule.** A label says the
principal is not permitted to *know the data exists*, and no allow rule elsewhere in the
policy should be able to undo that. A principal with no clearance at all sees only
unclassified data — level 0, no compartments.

### Information flow

`Label::join` gives the least upper bound: the label a derived fact must carry when it was
inferred from two labelled facts.

```rust
let derived = fact_a_label.join(&fact_b_label);
```

This is the rule that stops materialised inference laundering restricted data into an
unrestricted conclusion — combining a level-3 fact with a level-1 fact yields level 3, not
level 1. It is available for rule engines and projections to use; **nothing applies it
automatically**, because nothing in the current build materialises inferences.

---

## 7. Filter or fail

A refusal has two possible semantics, and the choice is more consequential than it looks.

| | Behaviour | Right when |
|---|---|---|
| **`Filter`** (default) | The query runs; withheld quads are simply not there. Nothing reveals that anything was withheld | A partial answer **is** the correct answer for that principal |
| **`Fail`** | The query errors | A partial answer would be **misread as a complete one** |

Use `Fail` for a compliance report, a reconciliation total, a regulatory submission —
anywhere silently missing rows are worse than no answer at all. Use `Filter` for search,
browsing, dashboards, anywhere a principal should see their own slice without being told
about the rest.

```rust
policy.with_semantics(Semantics::Fail)   // --fail-closed / Policy().fail_closed()
```

Under `Fail`, a wholly denied *graph* fails on the graph rather than on whichever quad
happened to be scanned first — so the error names the thing the query asked for.

---

## 8. Enterprise integration

**The server authenticates nobody, deliberately.** Token verification, Kerberos, mTLS and
SAML belong at the edge, where the infrastructure to do them properly already exists. The
server sits behind that edge and reads *already-verified* claims from three headers:

```
X-Holos-Principal: alice@example.com
X-Holos-Roles: hr,finance
X-Holos-Clearance: 3
```

It refuses to read them at all unless started with `--trust-forwarded-identity`, and prints
which way it is running at start-up.

### The front door contract

> **Your proxy must strip whatever the client sent under those names before setting its
> own.** Without that, `--trust-forwarded-identity` means any caller can add
> `X-Holos-Roles: admin` to a curl command and be believed.

[deploy/Caddyfile](deploy/Caddyfile) does this with three `request_header -X-Holos-*` lines
before any handler runs.

[deploy/nginx.conf](deploy/nginx.conf) does it differently, and **the difference is a trap**:
nginx passes unknown client headers straight through, so each of the three must be set
explicitly — setting one is what overrides what the client sent. Omitting any single one of
them opens the hole.

`run.sh` and `run.ps1` warn if `--trust-forwarded-identity` is combined with a non-loopback
bind address, which is the shape that mistake takes.

### Mapping an IdP onto this model

| Enterprise concept | Here |
|---|---|
| OIDC / SAML subject | `Principal.id` |
| Group membership (AD, LDAP, `groups` claim) | `Principal.roles` → `PrincipalMatch::Role` |
| Any other verified claim | `Principal.attributes` → `PrincipalMatch::Attribute` |
| Clearance / classification | `Principal.clearance` → `Label` |
| RBAC | Rules matched on `Role` |
| ABAC | Rules matched on `Attribute` |
| Row/column-level security | `Scope::Graph` / `Scope::Predicate` |
| Mandatory access control | Graph labels, which rules cannot override |

---

## 9. Worked example

`examples/hr.trig` has a public graph, a classified reviews graph, and salaries.

```sh
holos-server --data examples/hr.trig \
  --trust-forwarded-identity \
  --deny-predicate http://example.com/salary \
  --label-graph http://example.com/reviews=3
```

**Salaries are denied, so the column comes back empty — the query still runs:**

```sh
$ curl -s -H 'Accept: text/csv' --data-urlencode \
    'query=SELECT ?name ?salary WHERE { GRAPH ?g { ?s ex:name ?name
                                        OPTIONAL { ?s ex:salary ?salary } } }' \
    localhost:7878/query
name,salary
Alice,
Bob,
Carol,
```

**The classified graph, anonymous — nothing:**

```sh
$ curl -s ... 'query=SELECT ?note WHERE { GRAPH ex:reviews { ?s ex:reviewNote ?note } }'
s,note
```

**The same query with clearance 3 forwarded:**

```sh
$ curl -s -H 'X-Holos-Clearance: 3' ...
s,note
http://example.com/alice,Ready for promotion.
```

Clearance 2 still sees nothing: 2 does not dominate 3.

### The same thing from Python

```python
from holos import Store, Principal, Policy

policy = (Policy()
          .deny_predicate("http://example.com/salary", except_role="hr")
          .label_graph("http://example.com/reviews", 3))

anyone = Principal.anonymous()
hr     = Principal("urn:user:alice", roles=["hr"], clearance=3)

store.query(q, principal=anyone, policy=policy)   # no salaries, no reviews
store.query(q, principal=hr,     policy=policy)   # both
```

---

## 10. The write path

Policy applies to writes too, and one asymmetry is deliberate:

- **Insert** requires `WRITE` on the quad.
- **Delete** requires `WRITE` **and `READ`**.

A principal who may not *read* a quad may not delete it either — otherwise deletion becomes
an oracle for whether hidden data exists. Delete it, see whether the count changed, and the
policy has told you what it was hiding.

For SPARQL Update specifically:

- The `WHERE` clause is filtered by **read** policy, on the same path as a `SELECT`. **A
  principal cannot delete what it cannot see**, because the pattern never matches it.
- Every quad written is checked for `WRITE`.
- An update is **all-or-nothing**: a refusal partway through rolls back the operations that
  already succeeded, so a denied write cannot leave half an update behind.
- **`SILENT` does not silence a policy refusal.** The specification says a silent operation
  suppresses *its own* error; letting it swallow a denial would turn `SILENT` into a way to
  probe what one may not touch, and would report success for work that never happened.

---

## 11. Audit

```rust
let audit = CollectingSink::new();
Engine::query_audited(&view, session.principal(), query, &audit)?;
```

An `AccessEvent` records the principal, the action, the mode, the outcome
(`Allowed` / `PartiallyFiltered` / `Denied`), and **how many quads were withheld**.

> **That count never reaches the principal.** It is operator telemetry only. Returning "your
> answer was filtered, 14 rows withheld" would tell the principal exactly what the policy was
> trying not to tell them — the existence and the volume of the hidden data.

On the command line, `--audit` prints the record to stderr. Without it, a non-zero withheld
count still prints a one-line note to **stderr only**, for the same reason.

`NullSink` discards; `CollectingSink` accumulates in memory. Anything implementing
`AuditSink` can forward to a SIEM.

---

## 12. What this does *not* protect against

Stated plainly, because finding these out later is worse.

| Not protected | Why, and what to do |
|---|---|
| **Timing and volume side channels** | A query over a denied graph returns faster than one over a large permitted graph. Nothing here normalises timing. Mitigate at the edge if your threat model includes it |
| **The policy itself is not secret** | A principal cannot read denied data, but nothing hides the *shape* of the rules. Do not encode secrets in predicate or graph IRIs |
| **No encryption at rest** | RocksDB stores plaintext. Use full-disk or filesystem encryption |
| **No encryption in transit** | The server speaks plain HTTP. Terminate TLS at the front door — both supplied configs do |
| **Anyone who can read the store directory reads everything** | Policy is enforced by the engine, not by the storage. File permissions are the boundary; the systemd unit sets `ProtectSystem=strict` and one `ReadWritePaths` |
| **`--role` / `--clearance` flags apply to every request** | Development shortcuts. The run scripts warn when they are set |
| **No rate limiting or query cost limits** | `--timeout` bounds a query that is *reading or streaming*; one blocked inside a single in-memory step is not interruptible. Bound result sizes at the edge |
| **Labels are not applied to inferences automatically** | `Label::join` exists and nothing calls it, because nothing materialises inferences yet. If you add a rule engine, apply it |
| **A stale compiled policy** | A rule naming an IRI the dictionary had not yet seen could not resolve to an id. `CompiledPolicy::is_stale()` reports when the store has grown since compilation; recompile |

---

## 13. Reference

| | |
|---|---|
| Enforcement point | [crates/holos-engine/src/view.rs](crates/holos-engine/src/view.rs) |
| Policy model and resolution | [crates/holos-security/src/policy.rs](crates/holos-security/src/policy.rs) |
| Principals and the lattice | [crates/holos-security/src/principal.rs](crates/holos-security/src/principal.rs) |
| Audit | [crates/holos-security/src/audit.rs](crates/holos-security/src/audit.rs) |
| Design rationale | [DESIGN.md §14](DESIGN.md) |
| Deployment | [OPERATIONS.md](OPERATIONS.md) |

### Command-line flags

```
--deny-all               Deny by default instead of permit-all
--allow-graph <IRI>      Grant read on a named graph
--allow-predicate <IRI>  Grant read on a predicate
--deny-predicate <IRI>   Refuse read on a predicate
--except-role <NAME>     Make --deny-predicate not apply to this role
--label-graph <IRI>=<N>  Classify a graph at level N
--clearance <N>          Give the principal clearance N
--role <NAME>            Give the principal a role
--fail-closed            Error on refusal instead of filtering
--audit                  Print the audit record to stderr
```
