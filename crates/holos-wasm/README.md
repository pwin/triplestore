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

store.query('ASK { ?s ?p ?o }', undefined);            // boolean
store.query('SELECT ?s WHERE { ?s ?p ?o }', undefined); // [{ s: '<urn:a>' }]
store.query('CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }', undefined);
                                                        // ['<urn:a> <urn:p> "x"']
```

A CONSTRUCT comes back as N-Triples strings, one per triple, with no trailing separator.
That is the same mapping `holos-python` uses, and N-Triples specifically because it is the
one serialisation every RDF library in JS can parse without first agreeing on a term model.
An unbound variable is left *off* a SELECT row rather than set to `null`: RDF has no null,
and "no value here" is a different fact from "the value null".

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
| **One thread** | A load parses and interns in one serial loop rather than overlapping them. Same answer, same order, less throughput. |

Bringing the timeout back means a cooperative deadline checked in the row loop, with the
clock routed through the host. That is the one reduction above that is worth revisiting.

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
