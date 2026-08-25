#!/usr/bin/env bash
# Check prerequisites and build HOLOS. Idempotent; safe to re-run.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
FAILED=0

say()  { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; }
bad()  { printf '  \033[31mmiss\033[0m  %s\n' "$*"; FAILED=1; }
note() { printf '        %s\n' "$*"; }

say "Prerequisites"

if command -v cargo >/dev/null 2>&1; then
  RUSTV="$(rustc --version | awk '{print $2}')"
  ok "rust $RUSTV"
  # The workspace sets rust-version = 1.87.
  if [ "$(printf '1.87\n%s\n' "$RUSTV" | sort -V | head -1)" != "1.87" ]; then
    bad "rust 1.87 or newer is required (found $RUSTV)"
    note "rustup update stable"
  fi
else
  bad "rust — install from https://rustup.rs"
fi

# RocksDB compiles C++ through a build script and generates bindings with libclang.
# Without clang the rocksdb feature fails to build, and the failure is opaque, so it is
# worth catching here rather than 200 lines into a compile.
if command -v clang >/dev/null 2>&1 || [ -n "${LIBCLANG_PATH:-}" ]; then
  ok "clang / libclang (needed by the rocksdb bindings)"
else
  bad "clang — needed to build the rocksdb feature"
  note "debian/ubuntu:  sudo apt install clang libclang-dev build-essential"
  note "fedora:         sudo dnf install clang clang-devel"
  note "macos:          xcode-select --install"
  note "or build without persistence:  cargo build --release --no-default-features"
fi

if command -v curl >/dev/null 2>&1; then ok "curl (used by smoke.sh)"; else note "curl not found; smoke.sh needs it"; fi

[ "$FAILED" -eq 0 ] || { echo; echo "Install what is missing above, then re-run."; exit 1; }

echo
say "Building (release)"
cargo build --release --workspace

echo
say "Verifying"
cargo test --workspace --quiet 2>&1 | tail -20

echo
say "Built"
note "$ROOT/target/release/holos          command line"
note "$ROOT/target/release/holos-server   http service"
echo
note "Next:  deploy/load.sh <file.ttl>    then   deploy/run.sh"
