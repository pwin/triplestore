# Packaging `holos` for PyPI

## The `rocksdb` extra, and what it can honestly do

`pip install holos[rocksdb]` works and installs nothing extra. That is not a bug, and it is
worth being precise about why, because the name promises something Python cannot deliver.

**A Python extra selects dependencies.** It runs at install time and its only power is to
pull in more distributions. RocksDB here is not a distribution — it is C++ compiled *into*
the extension module, selected by a Cargo feature at build time. By the time `pip` reads the
extra, that decision is months old and baked into the wheel.

Three ways out, and why one was chosen:

| Approach | Why not |
|---|---|
| Two distributions, `holos` and `holos-rocksdb`, with the extra pulling the second | Two PyO3 extension modules means **two mutually incompatible `Store` types**. A `Store` from one cannot be passed to a function typed against the other, and the error when someone tries is baffling. This is the trap it looks like the obvious answer |
| A pure-Python shim dispatching to whichever module is present | Doubles the API surface, and every type that crosses the boundary needs converting. Real cost, no real gain |
| **One wheel, persistence compiled in, extra kept as a no-op** ✅ | One `Store` type. `pip install holos[rocksdb]` does not error for anyone who writes it out of habit, and the intent stays greppable in a requirements file |

So: **the published wheels carry persistence.** Ask a wheel what it has rather than assuming:

```python
import holos
holos.has_rocksdb()   # True for the published wheels
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

- **Linux** — build inside `manylinux2014`, which has a new enough toolchain for RocksDB.
  `manylinux_2_28` also works and produces smaller wheels; it drops older distributions.
- **macOS** — build `universal2`, or `x86_64` and `aarch64` separately.
- **Windows** — MSVC. Needs "Desktop development with C++" installed.
- **sdist** — publish one. It is what lets anyone on an unusual platform build from source,
  and it is the only artifact that stays useful when a wheel matrix goes stale.

```yaml
# .github/workflows/python.yml — the shape of it
jobs:
  wheels:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: PyO3/maturin-action@v1
        with:
          command: build
          args: --release --out dist --manifest-path crates/holos-python/Cargo.toml
          manylinux: 2014
      - uses: actions/upload-artifact@v4
        with: { name: wheels-${{ matrix.os }}, path: dist }

  publish:
    needs: wheels
    runs-on: ubuntu-latest
    environment: pypi
    permissions:
      id-token: write          # trusted publishing — no long-lived token in a secret
    steps:
      - uses: actions/download-artifact@v4
      - uses: pypa/gh-action-pypi-publish@release/v1
```

Use **PyPI trusted publishing** (OIDC). It removes the long-lived API token from repository
secrets entirely, which is the single highest-value thing you can do for a package's supply
chain.

## Testing a built wheel

```sh
maturin develop --release
python -m pytest crates/holos-python/tests -v
```

## The GIL

Every query and every load runs inside `py.detach(...)`, so the GIL is released for the
duration. Both can run for a long time and neither touches Python objects while running, so
holding it would stall every other thread in the process for nothing. `Storage` is
`Send + Sync`, which is what makes this sound rather than merely convenient.

The consequence worth knowing: **a `Store` is safe to share across Python threads**, and
concurrent readers genuinely run concurrently rather than taking turns.
