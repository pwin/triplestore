# Getting data in: every way to mint triples

Six routes into a graph or a holon, what each is for, and how each behaves when the target
is governed. If you only read one thing, read [§7](#7-what-changes-when-the-target-is-a-holon)
— it is the part that is different here.

| | Route | Use it when |
|---|---|---|
| [1](#1-load-a-file) | **Load a file** | You already have RDF |
| [2](#2-sparql-update) | **SPARQL Update** | You want to write from a query |
| [3](#3-tabular-data-csv-tsv-dataframes) | **CSV / TSV / dataframe + mapping** | Your data is a spreadsheet |
| [4](#4-the-api) | **The API** | You are writing a program |
| [5](#5-python) | **Python** | You are in a notebook |
| [6](#6-rdf-12-triple-terms) | **RDF 1.2 triple terms** | You need to say something *about* a statement |
| [7](#7-what-changes-when-the-target-is-a-holon) | **Into a holon** | The graph has invariants to defend |

---

## 1. Load a file

Seven serialisations, each also gzipped. The format comes from the extension.

```sh
holos stats --data data.ttl --store ./var/store --bulk
holos stats --data dump.nq.gz --store ./var/store --bulk    # streamed, multi-member safe
```

| Carries graph names | Does not |
|---|---|
| `.trig` `.nq` | `.ttl` `.nt` `.rdf` `.n3` `.jsonld` |

**That distinction is a correctness question, not a preference.** Loading a quad file as
N-Triples flattens every named graph into one. It is also why `holos dump` defaults to
N-Quads.

To put a triples-only file into a named graph, name the graph at load time — the API and
Python routes below both take one, and `LOAD … INTO GRAPH` does it from SPARQL.

---

## 2. SPARQL Update

```sparql
INSERT DATA {
  GRAPH <http://example.org/people> {
    <http://example.org/alice> a <http://example.org/Person> .
  }
}
```

```sh
holos update --store ./var/store --update-file changes.ru
curl -X POST -H 'Content-Type: application/sparql-update' --data @changes.ru \
     http://127.0.0.1:7878/update
```

All nine operations work — `INSERT DATA`, `DELETE DATA`, `DELETE/INSERT … WHERE`,
`DELETE WHERE`, `LOAD`, `CLEAR`, `CREATE`, `DROP`, and `SILENT` on each.

Three things to know:

- **All-or-nothing.** If any operation fails, the store is left exactly as it was —
  including the operations that had already succeeded.
- **`LOAD <http://…>` is refused.** Fetching a URL named inside a request is a server-side
  request forgery primitive. `file:` URLs load; remote ones return an error saying why.
- **Deriving new triples from existing ones** is what `INSERT … WHERE` is for:

```sparql
PREFIX ex: <http://example.org/>
INSERT { GRAPH ex:derived { ?s ex:fullName ?full } }
WHERE  { ?s ex:given ?g ; ex:family ?f BIND(CONCAT(?g, " ", ?f) AS ?full) }
```

---

## 3. Tabular data: CSV, TSV, dataframes

Most RDF starts life as a spreadsheet. Write a `CONSTRUCT` whose variables are the column
headers, in the [TARQL](https://tarql.github.io/) style:

```sparql
# people.rq
PREFIX ex: <http://example.org/>
CONSTRUCT {
  ?person a ex:Person ;
          ex:name  ?name ;
          ex:email ?email ;
          ex:sourceRow ?ROWNUM .
}
WHERE {
  BIND(IRI(CONCAT("http://example.org/person/", ?id)) AS ?person)
}
```

```rust
use holos_tabular::{load, source::{Csv, CsvOptions}, LoadOptions, Mapping};

let mapping = Mapping::from_path(Path::new("people.rq"))?;
let mut rows = Csv::from_reader(File::open("people.csv")?, &CsvOptions::default())?;
let report = load(&mut engine, &mut session, &mut rows, &mapping,
                  Some(&NamedNode::new("http://example.org/people")?),
                  &LoadOptions::default())?;
```

**An empty cell becomes `UNDEF`, not `""`.** Bob with no email gets no `ex:email` triple
rather than one with an empty string in it. That is TARQL's semantics and it is the one
people rely on.

`?ROWNUM` is the one-based row number. Headers that are not valid SPARQL variables —
`First Name`, `total (£)` — are rewritten by `CsvOptions { normalize: true, .. }`.

A dataframe uses the same mapping through `Frame`:

```rust
let frame = Frame::from_columns(vec![
    ("id".into(),   vec!["1".into(), "2".into()]),
    ("name".into(), vec!["Alice".into(), "Bob".into()]),
])?;
```

With the `polars` feature, `Frame::from_polars(&df)` converts a `DataFrame` directly. Every
column is rendered to its string form — the same thing a CSV export would do — so a load
from a frame and a load from the CSV of that frame produce identical triples. **Typing is
the mapping's job**: `xsd:integer(?age)` says what a column means, and guessing here would
turn a column of postcodes into integers.

The approach comes from [oxi-gen](https://github.com/semanticarts/oxi-gen). Its code is not
used; see [THIRD-PARTY.md](THIRD-PARTY.md).

---

## 4. The API

```rust
engine.insert(&mut session, Quad {
    subject:    NamedNode::new("http://example.org/alice")?.into(),
    predicate:  NamedNode::new("http://example.org/name")?,
    object:     Literal::new_simple_literal("Alice").into(),
    graph_name: GraphName::NamedNode(NamedNode::new("http://example.org/people")?),
}.as_ref())?;
```

`insert` takes a `&mut Session`, so **write policy applies**. There is no method that
writes without one. For bulk work, `engine.bulk_load(reader, format, base)` and
`bulk_load_into_graph(..., &graph)` parse and insert in one pass.

---

## 5. Python

```python
from holos import Store

store = Store("./var/db")
store.load("data.trig")
store.load("dump.nq.gz")
store.add("http://example.org/alice", "http://example.org/name", "Alice",
          graph="http://example.org/people")
store.update("INSERT DATA { <urn:a> <urn:b> <urn:c> }")
```

`add` maps Python types to RDF: `int` → `xsd:integer`, `float` → `xsd:double`,
`bool` → `xsd:boolean`, `str` → a plain literal. Wrap a string in `<>` to mean an IRI.

---

## 6. RDF 1.2 triple terms

The reason to care: **saying something about a statement** — who asserted it, when, with
what confidence — without the four-triple reification dance RDF 1.1 required and gave no
defined meaning to.

A triple term goes in **object position**, and `rdf:reifies` is what points at it:

```turtle
@prefix ex:  <http://example.org/> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:claim1 rdf:reifies <<( ex:carol ex:salary 102000 )>> ;
          ex:statedBy ex:payroll ;
          ex:statedOn "2026-08-01T09:00:00Z"^^xsd:dateTime ;
          ex:confidence 0.95 .
```

Two triples per annotated statement, where RDF 1.1 needed four.

**Minting them:** anywhere a term goes. In a file (above), from `INSERT DATA`, from a
`CONSTRUCT` mapping, or built with `TRIPLE(?s, ?p, ?o)`:

```sparql
PREFIX ex: <http://example.org/>
INSERT {
  GRAPH ex:provenance {
    [] rdf:reifies TRIPLE(?s, ex:salary, ?v) ;
       ex:statedBy ex:payroll ;
       ex:statedOn NOW() .
  }
}
WHERE { GRAPH ex:hr { ?s ex:salary ?v } }
```

**Querying them** is ordinary SPARQL — no side table:

```sparql
SELECT ?who ?when WHERE {
  ?c rdf:reifies <<( ex:carol ex:salary 102000 )>> ;
     ex:statedBy ?who ; ex:statedOn ?when .
}
```

`SUBJECT`, `PREDICATE`, `OBJECT` and `isTRIPLE` take one apart; `<<( ?s ?p ?o )>>` matches
one with variables. All are listed in [SPARQL-SURFACE.md](SPARQL-SURFACE.md).

> A triple term is **not asserted** by appearing inside `rdf:reifies`. Recording that
> somebody claimed Carol earns 102000 does not put that triple in your graph. That is the
> point, and it is what RDF 1.1 reification never managed to say clearly.

---

## 7. What changes when the target is a holon

Everything above writes into an ordinary named graph. Point any of it at a **holon's
scene** instead and one thing changes: the data has to get past the boundary.

```rust
let holon = Holon::new(NamedNode::new("urn:holon:people")?);
registry::register(&mut engine, &holon, &mut session)?;

// SHACL shapes into the holon's boundary graph.
engine.bulk_load_into_graph(shapes, RdfFormat::Turtle, None,
                            &GraphName::NamedNode(holon.boundary.clone()))?;

let outcome = tick(&mut engine, &holon, &mut session, &Delta::adding(triples))?;
if !outcome.committed() {
    // Refused. The scene is unchanged and the report says why.
}
```

**A tick is the write.** It applies the delta, revalidates only the focus nodes the delta
could have touched, and either commits or undoes everything. Measured at **0.91 ms per
commit against 140 ms for a full pass — 155× cheaper** ([BENCHMARKS.md](BENCHMARKS.md)),
which is what makes validating on every write affordable rather than aspirational.

### The routes above, through a holon

| Route | Through a holon |
|---|---|
| **File load** | Load into a staging graph, then tick the delta into the scene. A bulk load writes straight to storage and does **not** validate |
| **SPARQL Update** | Writes are policy-checked, but `INSERT DATA` into a scene does not run the boundary — derive the delta with a query, then tick it |
| **Tabular mapping** | Point `load` at the scene and **every mapped triple is policy-checked on the way in**. To validate as well, map into a staging graph and tick |
| **`engine.insert`** | Policy-checked, not validated. `tick` is the validating path |
| **Triple terms** | The event log is *made of* them — see below |

### What a tick records

Each commit writes per-statement provenance into the holon's event log, in RDF 1.2:

```turtle
_:change rdf:reifies <<( ex:alice ex:email "new@example.com" )>> ;
         holos:inTick _:tick ;
         holos:operation holos:Added .

_:tick a holos:Tick ;
       holos:version 42 ;
       prov:wasAssociatedWith <urn:holos:principal:alice> ;
       prov:startedAtTime "2026-08-25T09:00:00Z"^^xsd:dateTime ;
       holos:admitted true .
```

So "which tick asserted this exact triple, and who was responsible" is a plain SPARQL
query — **1.4 ms**, no audit database:

```sparql
SELECT ?v ?who WHERE {
  GRAPH <urn:holon:people/events> {
    ?c rdf:reifies <<( ex:alice ex:email "new@example.com" )>> ;
       holos:inTick ?t ; holos:operation holos:Added .
    ?t holos:version ?v ; prov:wasAssociatedWith ?who .
  }
}
```

### The pattern that ties it together

**Stage, tick, keep the provenance.** A spreadsheet arrives; the mapping turns it into
triples in a staging graph; the delta is ticked into the holon's scene, where the boundary
either admits it or refuses it; and either way the event log records what was attempted, by
whom, and whether it landed.

That is the whole argument for the holon layer: a governed graph where the invariants are
enforced **on the write path** rather than checked afterwards, at a cost proportional to the
change — and where the audit trail is the same RDF as everything else, queryable with the
same SPARQL, under the same access policy.

---

## Which route should I use?

| If you have | Use |
|---|---|
| RDF files | [Load a file](#1-load-a-file), with `--bulk` |
| A spreadsheet or dataframe | [A mapping](#3-tabular-data-csv-tsv-dataframes) |
| Data already in the store to transform | [`INSERT … WHERE`](#2-sparql-update) |
| A program generating triples | [The API](#4-the-api) or [Python](#5-python) |
| Statements to annotate | [Triple terms](#6-rdf-12-triple-terms) |
| A graph with rules that must hold | [A holon](#7-what-changes-when-the-target-is-a-holon) |

Reference: [SPARQL-SURFACE.md](SPARQL-SURFACE.md) ·
[ACCESS-CONTROL.md](ACCESS-CONTROL.md) · [OPERATIONS.md](OPERATIONS.md) ·
[BENCHMARKS.md](BENCHMARKS.md)
