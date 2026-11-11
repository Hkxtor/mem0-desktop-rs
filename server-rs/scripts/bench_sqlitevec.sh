#!/usr/bin/env bash
# Local wrapper for the M5/R9 perf baseline.
#   server-rs/scripts/bench_sqlitevec.sh            # full 1536-dim, 10k + 100k
#   PERF_DIMS=64 server-rs/scripts/bench_sqlitevec.sh   # fast smoke
set -euo pipefail
cd "$(dirname "$0")/.."
cargo run --release -p vector-sqlitevec --bin perf
