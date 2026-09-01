#!/usr/bin/env bash
# Build the example plugins (C / C++ / Zig / Rust) for the extension tests.
# Run from the repo root before `cargo test --test ffi`.
# Zig is optional (skipped when not on PATH).

set -euo pipefail
cd "$(dirname "$0")/.."

echo "== C plugin"
cc -shared -fPIC -O2 -I include -o plugins/c/rot13.so plugins/c/rot13.c

echo "== C++ plugin"
c++ -shared -fPIC -O2 -std=c++17 -I include -o plugins/cpp/example.so plugins/cpp/example.cpp

echo "== Rust plugin"
(cd plugins/rust && cargo build --release --quiet)

if command -v zig >/dev/null 2>&1; then
  echo "== Zig plugin"
  (cd plugins/zig && zig build-lib -dynamic -O ReleaseFast -lc rot13.zig)
else
  echo "== Zig plugin SKIPPED (zig not on PATH)"
fi

echo "done."
