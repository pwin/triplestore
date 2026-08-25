# holos

An RDF 1.2 triplestore with SPARQL 1.2, where **access policy is enforced at the index
scan** rather than by rewriting queries.

```sh
pip install holos
```

```python
from holos import Store, Principal, Policy

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
store.validate("shapes.ttl")            # SHACL, {'conforms': False, 'violations': 12, ...}
store.named_graphs()
store.dictionary_size                   # smaller than you expect: see below
holos.geosparql_functions()             # 45 of them
holos.has_rocksdb()                     # what this wheel actually contains
```

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

`store.update(...)` raises `NotImplementedError`. **There is no SPARQL Update evaluator in
this build** — raising is deliberate, because silently accepting an update that did nothing
would be much worse than an error. Writes go through `add()` and `load()`.

Persistence is compiled into the wheel rather than installed beside it, so
`pip install holos[rocksdb]` is accepted but installs nothing; the published wheels already
have it. `PACKAGING.md` explains why a Python extra cannot do otherwise, and
`has_rocksdb()` reports what you actually got.

## Licence

MIT or Apache-2.0, at your option.
