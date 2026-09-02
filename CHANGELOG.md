# Changelog

Notable changes per release. Numbers quoted here are measured; the benchmarks that produce
them are in `BENCHMARKS.md` and are runnable.

## Unreleased

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

What the audit did *not* find: the remaining sparql11 group is honestly upstream. Almost all
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
