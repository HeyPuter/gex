#!/bin/sh
set -eu
ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
COUNT="${1:-1000000}"
exec cargo run --manifest-path "$ROOT/Cargo.toml" --release \
    -p gex-gl --example transport_bench -- "$COUNT"
