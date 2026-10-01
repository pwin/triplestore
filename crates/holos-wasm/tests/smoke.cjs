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
  const one = store.query('SELECT (STRUUID() AS ?u) WHERE {}', undefined)[0].u.value;
  const two = store.query('SELECT (STRUUID() AS ?u) WHERE {}', undefined)[0].u.value;
  assert.notStrictEqual(one, two, 'the same UUID twice means the seed is constant');
});

// NOW() reaches oxsdatatypes' clock, which is SystemTime by default and unimplemented on
// this target -- the `js` feature routes it to Date.now(). Without it this panics rather
// than returning a wrong answer, so any real year proves the feature is on.
check('NOW() returns a real time, so the wasm clock is wired up', () => {
  const t = store.query('SELECT (NOW() AS ?t) WHERE {}', undefined)[0].t;
  assert.strictEqual(t.termType, 'Literal');
  assert.strictEqual(t.datatype.value, 'http://www.w3.org/2001/XMLSchema#dateTime');
  assert.ok(/^20\d\d-/.test(t.value), `not a plausible dateTime: ${t.value}`);
});

check('reports a bad query as an Error rather than panicking', () => {
  assert.throws(() => store.query('SELECT ?s WHERE { bad', undefined), /syntax error/i);
  // And the store is still usable afterwards.
  assert.strictEqual(store.size, 10);
});

check('refuses an unknown format by name', () => {
  assert.throws(() => store.load('<a> <b> <c> .', 'jsonl', undefined), /unknown RDF format/);
});

// ---------------------------------------------------------------------------------
// The surface added in 0.17.0, and what each piece exists for.
//
// Each was added because a consumer needed it, not to round the API out: the VS Code
// extension's repair engine applies a SPARQL Update and then enumerates the store to diff
// it, and its Query Workbench preview wants a Turtle document rather than terms.
// ---------------------------------------------------------------------------------

check('SELECT returns terms in rdf-js shape, not strings', () => {
  const rows = store.query(
    'SELECT ?s ?l WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> ?l }', undefined);
  const iri = rows.find((r) => r.s.termType === 'NamedNode');
  assert.ok(iri, 'expected at least one IRI subject');
  assert.ok(iri.s.value.startsWith('https://'), iri.s.value);

  // A language-tagged literal carries both fields rdf-js specifies.
  const tagged = rows.map((r) => r.l).find((l) => l.language === 'en');
  assert.ok(tagged, 'expected a language-tagged label');
  assert.strictEqual(tagged.termType, 'Literal');
  assert.strictEqual(
    tagged.datatype.value, 'http://www.w3.org/1999/02/22-rdf-syntax-ns#langString');

  // An untagged one reports language "" rather than omitting the field, also per rdf-js.
  const plain = rows.map((r) => r.l).find((l) => l.value === 'Chassis');
  assert.strictEqual(plain.language, '');
  assert.strictEqual(plain.datatype.value, 'http://www.w3.org/2001/XMLSchema#string');
});

check('a blank node term carries its bare label, without the _: prefix', () => {
  const rows = store.query(
    'SELECT ?x WHERE { ?x a <http://www.w3.org/2002/07/owl#Restriction> }', undefined);
  assert.strictEqual(rows.length, 1);
  assert.strictEqual(rows[0].x.termType, 'BlankNode');
  assert.ok(!rows[0].x.value.startsWith('_:'), rows[0].x.value);
});

check('queryRdf serialises a CONSTRUCT as a document', () => {
  const turtle = store.queryRdf(
    'CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }', 'turtle', undefined);
  assert.strictEqual(typeof turtle, 'string');
  assert.ok(turtle.includes('label') || turtle.includes('Class'), turtle.slice(0, 200));
  const nt = store.queryRdf('CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }', 'ntriples', undefined);
  const lines = nt.split('\n').filter((l) => l.trim());
  assert.ok(lines.length > 0 && lines.every((l) => l.trim().endsWith('.')), 'not N-Triples');
});

check('queryRdf refuses a SELECT rather than returning an empty document', () => {
  assert.throws(
    () => store.queryRdf('SELECT ?s WHERE { ?s ?p ?o }', 'turtle', undefined),
    /CONSTRUCT or DESCRIBE/);
});

check('dump round-trips through a second store', () => {
  const nq = store.dump('nquads');
  const copy = new holos.Store();
  try {
    assert.strictEqual(copy.load(nq, 'nquads', undefined), store.size);
    assert.strictEqual(copy.size, store.size);
  } finally {
    copy.free?.();
  }
});

check('dump keeps a named graph, and refuses a format that cannot carry one', () => {
  const s2 = new holos.Store();
  try {
    s2.load('<urn:g> { <urn:a> <urn:p> <urn:b> }', 'trig', undefined);
    assert.ok(s2.dump('nquads').includes('<urn:g>'), 'N-Quads dropped the graph name');
    // Turtle has nowhere to put a graph name, and the serialiser refuses rather than
    // writing the quad into the default graph. That is the right choice -- flattening would
    // move data between graphs and report success -- so the test pins the refusal, and that
    // the message says which formats do work.
    assert.throws(() => s2.dump('turtle'), /nquads or trig/);
  } finally {
    s2.free?.();
  }
});

check('update inserts and deletes, and reports what changed', () => {
  const s2 = new holos.Store();
  try {
    const out = s2.update('INSERT DATA { <urn:a> <urn:p> "x" . <urn:b> <urn:p> "y" }', undefined);
    assert.strictEqual(out.inserted, 2);
    assert.strictEqual(out.deleted, 0);
    assert.strictEqual(s2.size, 2);
    const back = s2.update('DELETE WHERE { <urn:a> ?p ?o }', undefined);
    assert.strictEqual(back.deleted, 1);
    assert.strictEqual(s2.size, 1);
  } finally {
    s2.free?.();
  }
});

check('a refused update leaves the store exactly as it was', () => {
  const s2 = new holos.Store();
  try {
    s2.update('INSERT DATA { <urn:a> <urn:p> "x" }', undefined);
    assert.throws(() => s2.update('INSERT DATA { not valid sparql', undefined));
    assert.strictEqual(s2.size, 1, 'a failed update changed the store');
  } finally {
    s2.free?.();
  }
});

// ---------------------------------------------------------------------------------
// SERVICE, answered from the host rather than from the network.
//
// The module has no network client, so these assertions need no endpoint and reach nothing.
// That is the design rather than a convenience of the test: a SERVICE IRI the host has not
// answered is simply unanswered, so there is no request for anyone to point anywhere.
// ---------------------------------------------------------------------------------

/** A SPARQL Results JSON document, as an endpoint would return one. */
function resultsJson(vars, rows) {
  return JSON.stringify({
    head: { vars },
    results: { bindings: rows },
  });
}

check('a SERVICE nobody has answered is reported, not fetched and not failed', () => {
  const s2 = new holos.Store();
  try {
    s2.update('INSERT DATA { <urn:a> <urn:p> "x" }', undefined);
    const pass = s2.queryFederated(
      'SELECT * WHERE { <urn:a> <urn:p> ?o SERVICE <https://endpoint.invalid/sparql> { ?s ?p2 ?o2 } }',
      undefined);
    assert.strictEqual(pass.pending.length, 1, JSON.stringify(pass.pending));
    assert.strictEqual(pass.pending[0].endpoint, 'https://endpoint.invalid/sparql');
    // The pending entry carries a query the host can POST as it stands -- not a pattern.
    assert.match(pass.pending[0].query, /SELECT/i);
    assert.match(pass.pending[0].query, /WHERE/i);
  } finally {
    s2.free?.();
  }
});

check('an answered SERVICE joins with the local data', () => {
  const s2 = new holos.Store();
  try {
    s2.load('<urn:a> <urn:name> "Hay" .', 'ntriples', undefined);
    const query =
      'SELECT ?name ?pop WHERE { <urn:a> <urn:name> ?name '
      + 'SERVICE <https://remote.example/sparql> { ?town <urn:pop> ?pop } }';

    const first = s2.queryFederated(query, undefined);
    assert.strictEqual(first.pending.length, 1);
    // The unanswered SERVICE contributed nothing, so the join lost its rows -- which is why a
    // pass with anything pending must be discarded rather than shown.
    assert.strictEqual(first.result.length, 0);

    s2.cacheService(
      first.pending[0].endpoint,
      first.pending[0].query,
      resultsJson(['town', 'pop'], [
        { town: { type: 'uri', value: 'urn:hay' }, pop: { type: 'literal', value: '1500' } },
      ]),
    );

    const second = s2.queryFederated(query, undefined);
    assert.strictEqual(second.pending.length, 0, 'asked again for something it was given');
    assert.strictEqual(second.result.length, 1);
    assert.strictEqual(second.result[0].name.value, 'Hay');
    assert.strictEqual(second.result[0].pop.value, '1500');
  } finally {
    s2.free?.();
  }
});

check('the cache key is the exact query string the pending entry carried', () => {
  const s2 = new holos.Store();
  try {
    const query = 'SELECT * WHERE { SERVICE <https://remote.example/sparql> { ?s <urn:p> ?o } }';
    const first = s2.queryFederated(query, undefined);
    assert.strictEqual(first.pending.length, 1);

    // A reformatted query is a different question. Caching under one must not answer the other,
    // because silently matching a near-miss would serve one endpoint's answer for another.
    s2.cacheService(first.pending[0].endpoint, `${first.pending[0].query} `,
      resultsJson(['s', 'o'], []));
    assert.strictEqual(s2.queryFederated(query, undefined).pending.length, 1,
      'a whitespace-different key was treated as a hit');

    s2.cacheService(first.pending[0].endpoint, first.pending[0].query,
      resultsJson(['s', 'o'], []));
    assert.strictEqual(s2.queryFederated(query, undefined).pending.length, 0);
  } finally {
    s2.free?.();
  }
});

check('a query with no SERVICE reports nothing pending', () => {
  const pass = store.queryFederated(
    'SELECT ?s WHERE { ?s a <http://www.w3.org/2002/07/owl#Class> }', undefined);
  assert.strictEqual(pass.pending.length, 0);
  assert.strictEqual(pass.result.length, 3);
});

check('queryFederated and query agree on result shape', () => {
  const q = 'SELECT ?s WHERE { ?s a <http://www.w3.org/2002/07/owl#Class> }';
  assert.deepStrictEqual(store.queryFederated(q, undefined).result, store.query(q, undefined));
});

// ---------------------------------------------------------------------------------
// The three gaps 0.19.0 closed. Each was found by a consumer needing it, not by reading
// the API over.
// ---------------------------------------------------------------------------------

check('a literal carries its RDF 1.2 base direction', () => {
  const s2 = new holos.Store();
  try {
    s2.load('<urn:a> <urn:p> "hello"@ar--rtl .', 'ntriples', undefined);
    const [row] = s2.query('SELECT ?o WHERE { ?s <urn:p> ?o }', undefined);
    assert.strictEqual(row.o.direction, 'rtl');
    assert.strictEqual(row.o.language, 'ar');
    assert.strictEqual(
      row.o.datatype.value, 'http://www.w3.org/1999/02/22-rdf-syntax-ns#dirLangString');
  } finally {
    s2.free?.();
  }
});

check('a literal with no direction reports "" rather than omitting the field', () => {
  // The same convention as `language`: a field that is sometimes absent makes every reader
  // check before it can compare.
  const [row] = store.query(
    'SELECT ?l WHERE { <https://example.org/w#Chassis> ' +
    '<http://www.w3.org/2000/01/rdf-schema#label> ?l }', undefined);
  assert.strictEqual(row.l.direction, '');
});

check('a SELECT result names its projected variables, in order', () => {
  const rows = store.query('SELECT ?s ?l WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> ?l }', undefined);
  assert.deepStrictEqual(rows.variables, ['s', 'l']);
});

check('a variable unbound in every row is still named', () => {
  // The case the rows cannot show, and the reason this exists: `head.vars` must list every
  // projected variable, and a caller reading only the rows would never see this one.
  const rows = store.query(
    'SELECT ?s ?nothing WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> ?l }', undefined);
  assert.ok(rows.length > 0);
  assert.deepStrictEqual(rows.variables, ['s', 'nothing']);
  assert.ok(rows.every((r) => !('nothing' in r)), 'an unbound variable should be absent from a row');
});

check('an empty SELECT still has columns', () => {
  const rows = store.query('SELECT ?a ?b WHERE { ?a <urn:nothing> ?b }', undefined);
  assert.strictEqual(rows.length, 0);
  assert.deepStrictEqual(rows.variables, ['a', 'b']);
});

check('SELECT * names its variables alphabetically, not in the order the query mentions them', () => {
  // Pinned because it is a divergence worth knowing rather than a bug here: spargebra collects
  // the in-scope variables in pattern order and then sorts them, and does not record that the
  // projection was `*`, so this binding cannot restore the author's order without guessing.
  // An explicit projection keeps the order you wrote, which is the comparison that makes the
  // point.
  const star = store.query('SELECT * WHERE { ?zebra <urn:holos-test:p> ?apple }', undefined);
  assert.deepStrictEqual(star.variables, ['apple', 'zebra']);
  const explicit = store.query(
    'SELECT ?zebra ?apple WHERE { ?zebra <urn:holos-test:p> ?apple }', undefined);
  assert.deepStrictEqual(explicit.variables, ['zebra', 'apple']);
});

check('explain returns a plan with statistics from the run', () => {
  const json = store.explain('SELECT ?s WHERE { ?s a <http://www.w3.org/2002/07/owl#Class> }', undefined);
  const plan = JSON.parse(json);
  assert.ok(typeof plan === 'object' && plan !== null, json.slice(0, 200));
  // Statistics are gathered as rows flow through, so an explanation written before the results
  // were drained reports zeroes. Something non-zero here is the evidence they were.
  assert.ok(JSON.stringify(plan).length > 20, json);
});

console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
