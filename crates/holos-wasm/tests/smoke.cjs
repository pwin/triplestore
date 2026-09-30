#!/usr/bin/env node
//
// What the wasm build has to get right, asserted against a built module.
//
//   wasm-pack build --release --target nodejs --out-dir pkg-node
//   node tests/smoke.cjs
//
// Why a node script rather than `cargo test`: the things most likely to break here do not
// fail at compile time. `std::thread::spawn`, `Instant::now` and `SystemTime::now` all
// *compile* for wasm32-unknown-unknown and then panic when called, so a green
// `cargo check --target wasm32-unknown-unknown` says almost nothing. Only running the
// module in a real host exercises the paths that were cfg-ed for it, and each assertion
// below corresponds to one of them.
//
// Why not wasm-bindgen-test: it needs its own runner and a headless browser for the parts
// that matter. This is the host the extension actually uses.
'use strict';

const path = require('node:path');
const assert = require('node:assert');

const PKG = path.join(__dirname, '..', 'pkg-node', 'holos_wasm.js');
let holos;
try {
  holos = require(PKG);
} catch (err) {
  console.error(`could not load ${PKG}`);
  console.error('build it first: wasm-pack build --release --target nodejs --out-dir pkg-node');
  console.error(String(err).split('\n')[0]);
  process.exit(2);
}

const TTL = `
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix ex:   <https://example.org/w#> .

ex:A a owl:Class ; rdfs:label "A"@en .
ex:B rdfs:subClassOf ex:A .
ex:C rdfs:subClassOf ex:B .
ex:Chassis a owl:Class ; rdfs:label "Chassis" .
ex:Car a owl:Class ; rdfs:subClassOf [ a owl:Restriction ; rdfs:label "has a wheel" ] .
`;

let failures = 0;
function check(what, fn) {
  try {
    fn();
    console.log(`  ok   ${what}`);
  } catch (err) {
    failures += 1;
    console.error(`  FAIL ${what}`);
    console.error(`       ${String(err).split('\n')[0]}`);
  }
}

const store = new holos.Store();

// A load is a serial parse-then-intern loop on wasm32 -- the scoped parse thread the native
// build uses would compile and then panic here. Same quads, same order, less throughput.
check('loads Turtle from a string', () => {
  // Ten: three rdf:type, three rdfs:label, two rdfs:subClassOf naming a class, plus the
  // anonymous restriction's own type and label.
  const n = store.loadTurtle(TTL, undefined);
  assert.strictEqual(n, 10, `expected 10 quads, got ${n}`);
  assert.strictEqual(store.size, 10);
  assert.ok(store.dictionarySize > 0);
});

check('answers ASK', () => {
  assert.strictEqual(
    store.query('ASK { ?s a <http://www.w3.org/2002/07/owl#Class> }', undefined), true);
  assert.strictEqual(store.query('ASK { <urn:nothing> ?p ?o }', undefined), false);
});

check('answers SELECT, and leaves an unbound variable off the row', () => {
  const rows = store.query(
    'SELECT ?s ?missing WHERE { ?s a <http://www.w3.org/2002/07/owl#Class> }', undefined);
  assert.ok(Array.isArray(rows) && rows.length === 3, `got ${rows.length} rows`);
  // RDF has no null, so "no value here" is an absent key rather than a null one.
  assert.ok(rows.every((row) => !('missing' in row)));
});

// The reason this crate exists: the extension reads CONSTRUCT results, and it reads them as
// N-Triples strings. A trailing separator is deliberately absent, matching the Python
// bindings, so a caller reassembling a document adds one.
check('answers CONSTRUCT as N-Triples strings with no trailing separator', () => {
  const out = store.query(
    'CONSTRUCT { ?s <urn:is> ?o } WHERE { ?s a ?o }', undefined);
  // Four typed subjects: three named classes and the anonymous restriction.
  assert.ok(Array.isArray(out) && out.length === 4, `got ${out.length}`);
  for (const line of out) {
    assert.strictEqual(typeof line, 'string');
    assert.ok(!line.trimEnd().endsWith('.'), `unexpected separator: ${line}`);
  }
});

// STR() of a blank node is a type error (SPARQL 1.1 17.4.2.5), which is exactly why the
// checks guard it with isBlank(). A conformant engine has to drop the unguarded message and
// keep the guarded one -- if this ever passes both, the guard in 29 shipped checks has
// quietly stopped being load-bearing.
check('treats STR() of a blank node as a type error', () => {
  const build = (bind) => `
    PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
    CONSTRUCT { ?e <urn:msg> ?msg } WHERE {
      ?e rdfs:label ?l . FILTER(LANG(?l) = "")
      ${bind}
      BIND(CONCAT("on ", ?f) AS ?msg)
    }`;
  const unguarded = store.query(build('BIND(STR(?e) AS ?f)'), undefined);
  const guarded = store.query(
    build('BIND(IF(isBlank(?e), "[a blank node]", STR(?e)) AS ?f)'), undefined);
  assert.strictEqual(unguarded.length, 1, 'the blank node message should have been dropped');
  assert.strictEqual(guarded.length, 2, 'the guard should keep both');
  assert.ok(guarded.some((t) => t.includes('[a blank node]')));
});

check('walks a transitive property path', () => {
  const rows = store.query(
    'PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n'
    + 'SELECT ?x WHERE { ?x rdfs:subClassOf+ <https://example.org/w#A> }', undefined);
  assert.strictEqual(rows.length, 2, `expected B and C, got ${rows.length}`);
});

// Natively this seeds from the wall clock and the process id, neither of which exists on
// wasm32. It asks the host for entropy instead, so two calls differing is the evidence that
// the getrandom backend is wired up rather than silently returning a constant.
check('STRUUID() varies, so the wasm entropy source is live', () => {
  const one = store.query('SELECT (STRUUID() AS ?u) WHERE {}', undefined)[0].u;
  const two = store.query('SELECT (STRUUID() AS ?u) WHERE {}', undefined)[0].u;
  assert.notStrictEqual(one, two, 'the same UUID twice means the seed is constant');
});

// NOW() reaches oxsdatatypes' clock, which is SystemTime by default and unimplemented on
// this target -- the `js` feature routes it to Date.now(). Without it this panics rather
// than returning a wrong answer, so any real year proves the feature is on.
check('NOW() returns a real time, so the wasm clock is wired up', () => {
  const t = store.query('SELECT (NOW() AS ?t) WHERE {}', undefined)[0].t;
  assert.ok(/^"20\d\d-/.test(t), `not a plausible dateTime: ${t}`);
});

check('reports a bad query as an Error rather than panicking', () => {
  assert.throws(() => store.query('SELECT ?s WHERE { bad', undefined), /syntax error/i);
  // And the store is still usable afterwards.
  assert.strictEqual(store.size, 10);
});

check('refuses an unknown format by name', () => {
  assert.throws(() => store.load('<a> <b> <c> .', 'jsonl', undefined), /unknown RDF format/);
});

console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
