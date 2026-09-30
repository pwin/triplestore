#!/usr/bin/env node
// Builds the npm package(s) for holos-wasm.
//
// Wraps `wasm-pack build` to fix up three things it cannot express itself:
//
//   * **The package name.** wasm-pack names every target's package after the crate, so
//     publishing both builds would be the same name twice -- a collision, not two packages.
//     The plain `holos-wasm` goes to the bundler build, because that is what most consumers
//     reach for; the Node build is the suffixed one, so `npm i holos-wasm` in a web project
//     does the expected thing.
//
//   * **The LICENSE files.** wasm-pack copies them into the output directory, but the
//     package.json it generates carries an explicit `files` array, and npm's
//     "always include a licence" rule does not match the suffixed
//     `LICENSE-APACHE`/`LICENSE-MIT` names a dual-licensed project uses. A package whose
//     package.json claims `"license": "MIT OR Apache-2.0"` while carrying neither licence
//     text is not acceptable, so they are listed explicitly. `npm pack --dry-run` is how to
//     check.
//
//   * **`repository`**, which wasm-pack warns about on every build.
//
// Usage:
//   node build.mjs            # both targets
//   node build.mjs nodejs     # just one
//
// Then, to check the result actually works rather than merely exists:
//   node tests/smoke.cjs
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPOSITORY = 'https://github.com/pwin/triplestore';
const LICENSES = ['LICENSE-APACHE', 'LICENSE-MIT'];

// `nodejs` for require()-based consumers -- the VS Code extension host, CLIs -- and
// `bundler` for webpack/vite/rollup, which want ESM plus a separate .wasm. The two are not
// interchangeable, which is why both are published.
const TARGETS = {
  nodejs: { outDir: 'pkg-node', name: 'holos-wasm-node', blurb: 'CommonJS, for Node and require()' },
  bundler: { outDir: 'pkg-bundler', name: 'holos-wasm', blurb: 'ESM, for bundlers' },
};

const requested = process.argv.slice(2);
const targets = requested.length ? requested : Object.keys(TARGETS);

for (const target of targets) {
  const spec = TARGETS[target];
  if (!spec) {
    console.error(`unknown target ${target}: expected one of ${Object.keys(TARGETS).join(', ')}`);
    process.exit(1);
  }
  const { outDir, name, blurb } = spec;

  console.log(`\n=== wasm-pack build --target ${target} --out-dir ${outDir} ===`);
  execFileSync('wasm-pack', ['build', '--target', target, '--out-dir', outDir, '--release'], {
    cwd: here,
    stdio: 'inherit',
  });

  const pkgPath = join(here, outDir, 'package.json');
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));

  const missing = LICENSES.filter((f) => !existsSync(join(here, outDir, f)));
  if (missing.length) {
    console.error(`  licence file(s) missing from ${outDir}: ${missing.join(', ')}`);
    process.exit(1);
  }
  pkg.files = [...new Set([...(pkg.files ?? []), ...LICENSES])];
  pkg.repository = { type: 'git', url: `git+${REPOSITORY}.git` };
  pkg.homepage = REPOSITORY;
  pkg.name = name;
  // Says which build this is, since the two are otherwise identical prose and
  // `npm i holos-wasm` in a Node project is a mistake worth naming.
  pkg.description = `${pkg.description} (${blurb})`;

  writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`, 'utf8');
  console.log(`  patched ${outDir}/package.json: name=${name}, +licences, +repository`);
}
