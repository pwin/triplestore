# Packaging `holosdb` for PyPI

## The `rocksdb` extra, and what it can honestly do

`pip install holosdb[rocksdb]` works and installs nothing extra. That is not a bug, and it is
worth being precise about why, because the name promises something Python cannot deliver.

**A Python extra selects dependencies.** It runs at install time and its only power is to
pull in more distributions. RocksDB here is not a distribution — it is C++ compiled *into*
the extension module, selected by a Cargo feature at build time. By the time `pip` reads the
extra, that decision is months old and baked into the wheel.

Three ways out, and why one was chosen:

| Approach | Why not |
|---|---|
| Two distributions, `holosdb` and `holosdb-rocksdb`, with the extra pulling the second | Two PyO3 extension modules means **two mutually incompatible `Store` types**. A `Store` from one cannot be passed to a function typed against the other, and the error when someone tries is baffling. This is the trap it looks like the obvious answer |
| A pure-Python shim dispatching to whichever module is present | Doubles the API surface, and every type that crosses the boundary needs converting. Real cost, no real gain |
| **One wheel, persistence compiled in, extra kept as a no-op** ✅ | One `Store` type. `pip install holosdb[rocksdb]` does not error for anyone who writes it out of habit, and the intent stays greppable in a requirements file |

So: **the published wheels carry persistence.** Ask a wheel what it has rather than assuming:

```python
import holosdb
holosdb.has_rocksdb()   # True for the published wheels
```

A slim build without persistence is available from source, for size-constrained targets
(Lambda layers, WASM later):

```sh
maturin build --release --no-default-features
```

That wheel raises `NotImplementedError` from `Store(path=...)` with a message saying so,
rather than failing obscurely.

## Building

```sh
pip install maturin
cd crates/holos-python

maturin develop --release      # build and install into the current venv
maturin build --release        # produce a wheel in target/wheels/
```

The build needs everything the Rust build needs — **clang and a C++ toolchain** — because
RocksDB compiles C++ and generates its bindings with libclang. This is the single most
common cause of a failed build; `deploy/setup.sh` checks for it.

### Why abi3

`pyo3` is configured with `abi3-py39`, so **one wheel per platform serves every CPython
3.9+** instead of one wheel per minor version. With a statically linked RocksDB inside, each
wheel is tens of megabytes; multiplying that by six Python versions for no functional gain
would be careless with other people's bandwidth.

## Wheels for release

`maturin-action` builds the matrix. The pieces that matter:

- **Linux** — build inside **`manylinux_2_28`**. `manylinux2014` is CentOS 7, and while its
  toolchain compiles RocksDB, it has no clang new enough for the libclang that
  `librocksdb-sys` needs to run bindgen; the build ends in a page of undefined `clang_*`
  symbols. 2_28 is AlmaLinux 8, where `dnf install clang-devel` is the whole fix. The cost
  is glibc 2.28 — CentOS 7 and Ubuntu 18.04 cannot install the wheel, and both are long out
  of support.
- **libclang, everywhere.** bindgen *loads* libclang at build time rather than linking it,
  and neither hosted runner puts one where it looks. macOS has it under
  `$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib`, Windows under
  `C:\Program Files\LLVM\bin`; the workflow sets `LIBCLANG_PATH` to each and fails loudly
  if the file is absent, rather than thirty minutes into a RocksDB compile.
- **macOS** — both arches are cross-compiled from the arm64 runner. The Intel runner
  label `macos-13` has been retired; a job requesting it queues forever rather than
  failing, which cancels the run and every job downstream of it.
- **Windows** — MSVC. Needs "Desktop development with C++" installed.
- **sdist** — publish one. It is what lets anyone on an unusual platform build from source,
  and it is the only artifact that stays useful when a wheel matrix goes stale.

The workflow is [`.github/workflows/python.yml`](../../.github/workflows/python.yml). Five
jobs: `wheels` (five platforms), `sdist`, `test-wheel`, `publish-testpypi`, `publish`.

### Publishing, and the two destinations

Both use **trusted publishing** (OIDC), so there is no API token in repository secrets — the
single highest-value change available for a package's supply chain.

| Destination | Trigger | Environment |
|---|---|---|
| TestPyPI | a manual run with `publish_testpypi` ticked | `testpypi` |
| PyPI | pushing a `v*` tag | `pypi` |

Each destination needs its own trusted publisher, registered on that site, naming this
repository, **the workflow file by name** (`python.yml`), and the environment above. Before a
project's first upload it is a *pending* publisher — which is also what reserves the name —
and becomes an ordinary one once something has been published.

TestPyPI is opt-in rather than automatic on every manual run, because an upload cannot be
undone and a version number cannot be reused. `skip-existing: true` keeps a repeat run from
failing the workflow when that version has already gone up, which is what happens the second
time anyone tries the pipeline without bumping.

### Cutting a release

```sh
# 1. Bump the version. It is inherited from [workspace.package], so it is two places:
#    the `version` field and the path-dependency pins just below it.
$EDITOR Cargo.toml

# 2. Rehearse: Actions -> python -> Run workflow, with publish_testpypi ticked.
#    Then install from TestPyPI into a clean interpreter and check it imports.
pip install --index-url https://test.pypi.org/simple/ \
            --extra-index-url https://pypi.org/simple/ holosdb

# 3. Tag. That is what publishes to PyPI.
git tag -a v0.1.0 -m "0.1.0" && git push origin v0.1.0
```

The `--extra-index-url` in step 2 is not optional: TestPyPI does not mirror ordinary
dependencies, so an install that needs any will fail without it.

## Testing a built wheel

CI installs the built wheel into a clean interpreter and runs the tests **against that**,
rather than against the working tree. `maturin develop` would test a build nobody receives;
installing the artifact is what catches a packaging fault — a missing `.pyi`, an absent
`py.typed`, a module renamed in one place and not another.

Locally:

```sh
cd crates/holos-python
python -m maturin build --release
python -m venv /tmp/v && /tmp/v/bin/pip install ../../target/wheels/holosdb-*.whl pytest
/tmp/v/bin/python -m pytest tests -v
```

### What ships inside the wheel

```
holosdb/__init__.py        the public surface and its docstring
holosdb/_holosdb.pyd|so    the extension module, RocksDB linked in
holosdb/_holosdb.pyi       type stubs
holosdb/py.typed           PEP 561 marker — without it the stubs above are ignored
```

`py.typed` is easy to leave out and fails silently: mypy and pyright skip the stubs of any
package that does not declare itself typed, so the types simply have no effect and nothing
reports an error.

## The GIL

Every query and every load runs inside `py.detach(...)`, so the GIL is released for the
duration. Both can run for a long time and neither touches Python objects while running, so
holding it would stall every other thread in the process for nothing. `Storage` is
`Send + Sync`, which is what makes this sound rather than merely convenient.

The consequence worth knowing: **a `Store` is safe to share across Python threads**, and
concurrent readers genuinely run concurrently rather than taking turns.
