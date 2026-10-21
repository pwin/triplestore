# holosdb

An RDF 1.2 triplestore with SPARQL 1.2, where **access policy is enforced at the index
scan** rather than by rewriting queries.

```sh
pip install holosdb
```

```python
from holosdb import Store, Principal, Policy

store = Store()                    # in memory
store = Store("./var/db")          # persistent
store.load("data.trig")
store.load("dump.nq.gz")           # gzip: streamed, multi-member safe

for row in store.query("SELECT ?s ?name WHERE { ?s <http://example.com/name> ?name }"):
    print(row["s"], row["name"])
```

## Asking a question *as somebody*

This is the part no other binding in this ecosystem has. A query can carry a principal and
a policy, and the answer comes back filtered to what that principal may see:

```python
policy = (Policy()
          .deny_predicate("http://example.com/salary", except_role="hr")
          .label_graph("http://example.com/reviews", 3))

anyone = Principal.anonymous()
hr     = Principal("urn:user:alice", roles=["hr"], clearance=3)

store.query(q, principal=anyone, policy=policy)   # no salaries, no reviews
store.query(q, principal=hr,     policy=policy)   # both
```

Because the filtering happens at the scan and not above it:

> the answer to *Q* equals the answer *Q* would have over the sub-dataset the principal
> may see.

That holds for **every** query shape without anyone enumerating them. A `COUNT` cannot leak
the existence of hidden rows, and a `FILTER NOT EXISTS` cannot probe for them.

Use `Policy().fail_closed()` when a partial answer would be misread as a complete one — a
compliance report, a reconciliation total — and an error is better than a quiet omission.

## What else is here

```python
store.update("INSERT DATA { ... }")     # SPARQL Update: all or nothing, policy applies
store.validate("shapes.ttl")            # SHACL: conforms, violations, and each result
store.named_graphs()
store.predicate_counts()                # [(iri, quads), ...]: the store owner's view
store.dictionary_size                   # smaller than you expect: see below
holosdb.geosparql_functions()             # 45 of them
holosdb.has_rocksdb()                     # what this wheel actually contains
```

## Queries a program writes

For a query you did not write yourself — an agent's, a user's, a notebook's — the store can
bound it and explain it:

```python
store.query(q, timeout=5.0)        # QueryTimeoutError past five seconds
store.query(q, limit=100)          # at most 100 rows; the evaluation stops there
store.query(q, reorder=True)       # plan with statistics over the store
store.explain(q)                   # the plan, with each operator's row count and time
```

`reorder=True` is the one to reach for on a large store. It builds statistics once per
write — or loads the snapshot a persistent store keeps — and plans each query by them, which
for a badly ordered pattern is the difference between milliseconds and minutes. It also
lets a `DISTINCT`, `ORDER BY` or `GROUP BY` estimated at over ten million rows be refused at
once with `QueryRefusedError` rather than discovered by the operating system.

Failures come as types you can tell apart without reading the message: `SyntaxError`,
`PolicyError`, `QueryRefusedError` and `QueryTimeoutError`, all of them `HolosError`.

`validate()` returns every result, not just a count:

```python
>>> store.validate("shapes.ttl")["results"][0]
{'focus': 'http://example.com/bob', 'path': 'http://example.com/name', 'value': None,
 'severity': 'Violation', 'component': 'http://www.w3.org/ns/shacl#MinCountConstraintComponent',
 'shape': '_:...', 'message': '...'}
```

There is no memory ceiling here, unlike `holos-server`: enforcing one needs a counting
allocator, and a library does not install one in somebody else's process. A timeout and
`reorder=True` cover the runaway cases a ceiling would.

**GeoSPARQL** works through the ordinary query path, so it composes with policy — denying
`geo:asWKT` makes a spatial join find nothing:

```python
store.query("""
  PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
  PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
  SELECT ?site WHERE {
    ?site <http://www.opengis.net/ont/geosparql#asWKT> ?g .
    FILTER(geof:sfWithin(?g, geof:buffer(?depot, 5, uom:kilometre)))
  }
""")
```

**`dictionary_size` is reliably smaller than the number of terms** — every integer, float,
dateTime and short string is packed into its own 64-bit id and never reaches the dictionary.
A million triples produced 489,479 dictionary entries.

## Threading

A `Store` is safe to share across threads, and **the GIL is released** around every query
and every load. Concurrent readers genuinely run concurrently rather than taking turns.

## What is not here

Persistence is compiled into the wheel rather than installed beside it, so
`pip install holosdb[rocksdb]` is accepted but installs nothing; the published wheels already
have it. `PACKAGING.md` explains why a Python extra cannot do otherwise, and
`has_rocksdb()` reports what you actually got.

## Licence

MIT or Apache-2.0, at your option.
