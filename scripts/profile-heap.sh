#!/usr/bin/env bash
# Steady-state heap profiling via dhat-rs, over `search::query::hybrid_search`
# against a small fixture vault the profiling harness builds itself (see
# examples/profile_search.rs). DHAT starts only after the harness's warmup
# calls, so model/index initialization isn't mistaken for steady-state cost.
set -euo pipefail
cd "$(dirname "$0")/.."

export PROFILE_QUERY="${PROFILE_QUERY:-how do we handle API rate limits}"
export PROFILE_HEAP_WARMUPS="${PROFILE_HEAP_WARMUPS:-1}"
export PROFILE_HEAP_ITERATIONS="${PROFILE_HEAP_ITERATIONS:-5}"

cargo run --release --features profiling --example profile_search

echo
echo "Heap profile written to:"
echo "  dhat-heap.json (view at https://nnethercote.github.io/dh_view/dh_view.html)"
