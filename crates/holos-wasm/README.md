# holos-wasm

WebAssembly bindings for the HOLOS store and SPARQL engine — the same surface as the Python
bindings, for a JavaScript host.

```bash
node build.mjs           # both npm packages
node build.mjs nodejs    # just one
node tests/smoke.cjs     # assert the built module actually answers queries
```

`build.mjs` wraps `wasm-pack` because wasm-pack names every target's package after the
crate, so publishing both builds unpatched would be the same name twice — a collision rather
than two packages. It also adds the `LICENSE-APACHE`/`LICENSE-MIT` files to `files` (npm's
"always include a licence" rule does not recognise the suffixed names a dual-licensed project
uses, so a package claiming `MIT OR Apache-2.0` would ship neither text) and fills in
`repository`.

| Target | Package | For |
|---|---|---|
| `bundler` | `holos-wasm` | webpack, vite, rollup — ESM plus a separate `.wasm` |
| `nodejs` | `holos-wasm-node` | `require()` — the VS Code extension host, CLIs |

The plain name goes to the bundler build because that is what most consumers reach for, so
`npm i holos-wasm` in a web project does the expected thing.

```js
const holos = require('./pkg-node/holos_wasm.js');

const store = new holos.Store();
store.loadTurtle('<urn:a> <urn:p> "x" .', undefined);

store.query('ASK { ?s ?p ?o }', undefined);              // true
store.query('SELECT ?s WHERE { ?s ?p ?o }', undefined);  // [{ s: { termType: 'NamedNode',
                                                         //         value: 'urn:a' } }]
store.query('CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }', undefined);
                                                         // ['<urn:a> <urn:p> "x"']
store.queryRdf('CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }', 'turtle', undefined);
                                                         // '<urn:a> <urn:p> "x" .
'
store.update('INSERT DATA { <urn:b> <urn:p> "y" }', undefined);
                                    // { inserted: 1, deleted: 0, graphsCreated: 0, … }
store.dump('nquads');                // everything the store holds
```

| What you get back | From |
|---|---|
| `true` / `false` | ASK |
| terms in rdf-js shape — `{termType, value}`, plus `language` and `datatype` on a literal | SELECT |
| N-Triples strings, one per triple, no trailing separator | CONSTRUCT, DESCRIBE |
| a document in the format you asked for | `queryRdf` |
| `{inserted, deleted, graphsCreated, graphsDropped}` | `update` |

**SELECT returns terms, not strings, since 0.17.0.** They used to be N-Triples strings,
which made every consumer parse term syntax to reach a value, and no RDF library in JS
accepts a bare term string as input. `{termType, value}` is what rdf-js specifies and what
oxigraph returns, so a term can go straight to n3 or be compared field by field. A blank
node's `value` is the bare label, without `_:` — and it is this engine's label, not the one
in whatever document was loaded, because a parser may rename blank nodes and this one does.

CONSTRUCT stays N-Triples strings deliberately: it is the one serialisation every RDF
library in JS parses without first agreeing on a term model, and a graph is usually wanted
as a graph. `queryRdf` is there when a *document* is wanted instead — a preview pane showing
Turtle, or a payload for something that parses RDF but does not speak these shapes.

An unbound variable is left *off* a SELECT row rather than set to `null`: RDF has no null,
and "no value here" is a different fact from "the value null".

`dump` is a document rather than a quad iterator, because crossing the boundary once with a
document beats crossing it once per quad and every JS RDF library can parse what comes back.
Use `nquads` or `trig` for a store with named graphs: Turtle and N-Triples have nowhere to
put a graph name, and the serialiser **refuses** such a quad rather than writing it into the
default graph. That is the right choice — flattening would move data between graphs while
reporting success — and the error names the formats that work.

## What this build cannot do

Each of these is a property of the host, not of the engine. None is a stripped-down
reimplementation — it is the same `holos_engine::Engine`, compiled for a target with fewer
facilities.

| | Why |
|---|---|
| **No persistence** | RocksDB does not build for wasm32. The in-memory store is the default backend anyway (`holos-store`'s `rocksdb` feature is opt-in), so this is the ordinary build. |
| **No file paths** | `std::fs` compiles for wasm32 and fails at runtime. RDF arrives as a string the host has already read, so a load costs memory proportional to the document — a browser tab is not where a 60 GB dump goes. |
| **No spilling `ORDER BY`** | The external merge sort needs somewhere to put run files. A sort is bounded by memory here, and a large one exhausts the module rather than reaching for disk. |
| **No timeout, no memory ceiling** | Both are enforced by a watchdog thread sampling a clock, and this target has neither. `QueryOptions::guard` returns `None`, and this crate exposes no way to ask — so nobody can set a limit and believe it is being honoured. |
| **No network** | There is no HTTP client in here at all. `SERVICE` works, but only because the *host* fetches and hands the answer in — see below. |
| **One thread** | A load parses and interns in one serial loop rather than overlapping them. Same answer, same order, less throughput. |

Bringing the timeout back means a cooperative deadline checked in the row loop, with the
clock routed through the host. That is the one reduction above that is worth revisiting.

## `SERVICE`, without a network client

`store.query()` cannot answer a `SERVICE` clause. `store.queryFederated()` can, by asking the
host to do the fetching:

```js
for (let round = 0; round < 8; round += 1) {
  const pass = store.queryFederated(query, undefined);
  if (!pass.pending.length) return pass.result;
  for (const { endpoint, query: ask } of pass.pending) {
    // The host decides what may be called. Nothing in the wasm can reach the network.
    const res = await fetch(endpoint, {
      method: 'POST',
      headers: { 'Content-Type': 'application/sparql-query',
                 Accept: 'application/sparql-results+json' },
      body: ask,
    });
    store.cacheService(endpoint, ask, await res.text());
  }
}
throw new Error('SERVICE did not settle');
```

**A pass with anything `pending` has wrong results, and says so by having them.** The
unanswered `SERVICE` contributed no rows, so the join above it lost rows too. Discard `result`
whenever `pending` is non-empty rather than showing it.

### Why it is shaped like this

`spareval::DefaultServiceHandler::handle` is synchronous and `fetch` is not, and you cannot
await a Promise from synchronous wasm. Three ways round that, and they commit you to different
things:

| | Cost |
|---|---|
| synchronous `XMLHttpRequest` | deprecated, warns in every browser, blocks the tab, may stop working |
| a worker blocking on `Atomics.wait` | needs COOP/COEP cross-origin-isolation headers wherever the app is hosted, and breaks quietly if they are lost |
| **asking twice** | the query is evaluated more than once |

The third is what this does. It buys no deprecated API, no hosting requirement, and an
allow-list that lives in the host — which is the part that matters. `holos-engine`'s own
`service` module refuses remote `SERVICE` because on a *server* it is an SSRF primitive: a
query from a stranger makes the server fetch an address of the stranger's choosing. That
objection does not transfer here, and not because a browser is safer — because **this crate
cannot make a request at all**. There is nothing to point anywhere. The fetch is the host's, in
the user's own browser, with the user's own network position and CORS between them.

### More than two rounds is normal

A `SERVICE` whose pattern carries bindings from an earlier join only takes its final shape once
that join has rows, and the first pass gives it none — so an answer can reveal a request that
could not have been seen before. The same semi-naive iteration the rules engine uses. The cap
belongs to the host, because only the host knows how long it will wait.

The cache key is `(endpoint, query)` and the query is byte-exact: a reformatted query is a
different question and will be asked again. That is deliberate — matching a near-miss would
serve one endpoint's answer to another's question.

## Four things had to be cfg-ed for this target

The compile-time surface was almost nothing — one getrandom error. The work was runtime:
`std::thread::spawn`, `Instant::now` and `SystemTime::now` all *compile* for
wasm32-unknown-unknown and then panic when called, so a green
`cargo check --target wasm32-unknown-unknown` proves very little. `tests/smoke.cjs` is what
proves the rest, and each assertion in it corresponds to one row here.

| Where | Native | wasm32 |
|---|---|---|
| `holos-engine` `load_parsed` | scoped parse thread, bounded channel | serial parse→intern loop |
| `holos-engine` `QueryOptions::guard` | watchdog thread | `None` |
| `holos-engine` `functions::seed` | wall clock ⊕ process id | `getrandom`, else a stack address |
| `holos-engine` audit `at:` | `SystemTime::now()` | `UNIX_EPOCH`, which cannot be mistaken for a real time |

Two build-time halves come from outside the crate and are easy to lose:

* **`getrandom`** needs both the `wasm_js` feature *and* the `getrandom_backend="wasm_js"`
  cfg. The cfg lives in the workspace `.cargo/config.toml`, scoped to this target so no
  native build is affected. getrandom's own `compile_error!` says the feature alone is not
  enough. It is reached through `rand`, for oxrdf's blank node identifiers, and through
  `functions::seed` for `UUID()`/`STRUUID()`.
* **`oxsdatatypes`** needs its `js` feature, which routes the clock `NOW()` reaches to
  `js_sys::Date::now()`. Declared in this crate's wasm32 dependencies so cargo's feature
  unification turns it on for wasm builds only; native builds keep `SystemTime`.

`wasm-opt` is given `--enable-bulk-memory --enable-sign-ext`: current rustc emits those
instructions by default and the binaryen wasm-pack downloads rejects them otherwise, failing
the build with "Bulk memory operations require bulk memory". Both proposals shipped years
ago and are on in every runtime this targets, so enabling them beats turning `wasm-opt` off
and shipping a larger module.

## Not published yet

There is no `holos-wasm` on npm. `pkg-node/` and `pkg-bundler/` are build output and
gitignored by wasm-pack's own `.gitignore`. The sibling SHACL engine publishes
`shacl-wasm` and `shacl-wasm-node` from an equivalent crate, and that repository's
`release.yml` is the model for doing the same here.
