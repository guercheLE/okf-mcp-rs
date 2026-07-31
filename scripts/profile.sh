#!/usr/bin/env bash
# CPU profiling via samply (works on macOS/Linux without sudo/dtrace).
# Heap profiling via dhat-rs is a separate opt-in command so allocator
# instrumentation cannot distort the CPU profile.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v samply >/dev/null 2>&1; then
  echo "error: samply not found. Install it with:" >&2
  echo "  cargo install samply" >&2
  echo "  samply setup   # macOS only: codesigns samply for process attach" >&2
  exit 1
fi

mkdir -p profile

profile_query="${PROFILE_QUERY:-how do we handle API rate limits}"
profile_warm_seconds="${PROFILE_WARM_SECONDS:-30}"

cargo build --release --bin okf-mcp
cargo build --release --example profile_search

# Cold-start: an empty vault (just a `.okf/` marker) is enough — the
# dominant cold-start cost is process startup + first-time embedding model
# load, not the size of whatever it's searching.
cold_start_vault="$(mktemp -d "${TMPDIR:-/tmp}/okf-mcp-cold-start-vault.XXXXXX")"
mkdir -p "$cold_start_vault/.okf"

samply record --save-only --unstable-presymbolicate -o profile/cold-start.json.gz -- \
  ./target/release/okf-mcp --vault "$cold_start_vault" search "$profile_query"

python3 scripts/samply_to_text.py \
  profile/cold-start.json.gz \
  profile/cold-start-folded-stacks.txt \
  profile/cold-start-cpu-top.txt

ready_file="$(mktemp "${TMPDIR:-/tmp}/okf-mcp-profile-ready.XXXXXX")"
rm -f "$ready_file"
warm_pid=""
samply_pid=""
cleanup() {
  if [[ -n "$samply_pid" ]] && kill -0 "$samply_pid" >/dev/null 2>&1; then
    kill -INT "$samply_pid" >/dev/null 2>&1 || true
    wait "$samply_pid" >/dev/null 2>&1 || true
  fi
  if [[ -n "$warm_pid" ]] && kill -0 "$warm_pid" >/dev/null 2>&1; then
    kill -TERM "$warm_pid" >/dev/null 2>&1 || true
    wait "$warm_pid" >/dev/null 2>&1 || true
  fi
  rm -f "$ready_file"
  rm -rf "$cold_start_vault"
}
trap cleanup EXIT

./target/release/examples/profile_search "$ready_file" "$profile_warm_seconds" &
warm_pid=$!
for _ in {1..300}; do
  if [[ -f "$ready_file" ]]; then
    break
  fi
  if ! kill -0 "$warm_pid" >/dev/null 2>&1; then
    echo "error: warm-search profiling harness exited before becoming ready" >&2
    wait "$warm_pid" || true
    exit 1
  fi
  sleep 0.1
done
if [[ ! -f "$ready_file" ]]; then
  echo "error: warm-search profiling harness did not become ready" >&2
  exit 1
fi

samply record --save-only --unstable-presymbolicate \
  --pid "$warm_pid" -o profile/warm-search.json.gz &
samply_pid=$!
sleep 5
kill -INT "$samply_pid"
wait "$samply_pid"
samply_pid=""

python3 scripts/samply_to_text.py \
  profile/warm-search.json.gz \
  profile/warm-search-folded-stacks.txt \
  profile/warm-search-cpu-top.txt
cleanup
trap - EXIT

{
  echo "# okf-mcp search bottleneck report"
  echo
  echo "Generated $(date -u +%Y-%m-%dT%H:%M:%SZ)."
  echo
  coverage_report="target/coverage/production-lcov.info"
  if [ ! -f "$coverage_report" ]; then
    coverage_report="target/coverage/lcov.info"
  fi
  if [ -f "$coverage_report" ]; then
    echo "## Coverage gaps (most missed lines)"
    echo
    echo '```'
    awk '
      /^SF:/ { file=substr($0,4); hit=0; total=0 }
      /^DA:/ { total++; split($0,a,","); if (a[2]+0 > 0) hit++ }
      /^end_of_record/ { if (total > 0 && hit < total) { missed=total-hit; pct=100*hit/total; printf "%6d missed  %6.1f%% covered  %s  (%d/%d lines)\n", missed, pct, file, hit, total } }
    ' "$coverage_report" | sort -nr -k1,1 | head -20
    echo '```'
    echo
  fi
  echo "## Cold-start hottest functions (CPU, self-time sample count)"
  echo
  echo '```'
  tail -n +2 profile/cold-start-cpu-top.txt | head -20
  echo '```'
  echo
  echo "## Warm-search hottest functions (CPU, self-time sample count)"
  echo
  echo '```'
  tail -n +2 profile/warm-search-cpu-top.txt | head -20
  echo '```'
} > profile/bottleneck-report.md

echo
echo "Profile written to:"
echo "  profile/cold-start.json.gz (interactive cold-start profile)"
echo "  profile/warm-search.json.gz (interactive steady-state search profile)"
echo "  profile/*-cpu-top.txt      (LLM/tool-ingestable summaries)"
echo "  profile/bottleneck-report.md (combined coverage + hot-path summary; run scripts/coverage.sh first for the coverage section)"
echo
echo "For heap profiling: bash scripts/profile-heap.sh"
