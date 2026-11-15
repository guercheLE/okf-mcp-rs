#!/usr/bin/env bash
# Re-downloads the third-party JS bundles vendored under assets/vendor/ and
# verifies them against a pinned sha256, instead of trusting whatever a
# `curl`/`npm pack` happened to produce. The checked-in files are read via
# `include_str!` at compile time (src/explorer/server.rs), so they stay
# committed to git for offline/reproducible builds and crates.io publishing
# -- this script is the auditable way to update or re-verify them, and is
# what .github/workflows/vendor-assets.yml runs to publish them as release
# assets.
set -euo pipefail
cd "$(dirname "$0")/.."

# name|version|url|sha256
ASSETS=(
  "mermaid.min.js|11.16.1|https://cdn.jsdelivr.net/npm/mermaid@11.16.1/dist/mermaid.min.js|18327bef70d96fb505fe7287d9f6a7362ebf07ff6576ddfaffb1a06f3e1a2954"
  "3d-force-graph.min.js|1.80.0|https://cdn.jsdelivr.net/npm/3d-force-graph@1.80.0/dist/3d-force-graph.min.js|d96e738edcca580edd524730c1c6b05ed2efce028c23ca95db1bf43033a72e42"
)

mode="${1:-fetch}"
case "$mode" in
  fetch|--verify) ;;
  *)
    echo "usage: $0 [fetch|--verify]" >&2
    exit 2
    ;;
esac

out_dir="assets/vendor"
mkdir -p "$out_dir"

status=0
for entry in "${ASSETS[@]}"; do
  IFS='|' read -r name version url expected_sha <<<"$entry"
  dest="$out_dir/$name"

  if [[ "$mode" == "--verify" ]]; then
    if [[ ! -f "$dest" ]]; then
      echo "error: $dest is missing" >&2
      status=1
      continue
    fi
    actual_sha=$(shasum -a 256 "$dest" | cut -d' ' -f1)
    if [[ "$actual_sha" != "$expected_sha" ]]; then
      echo "error: $dest does not match the pinned sha256 for $name@$version" >&2
      echo "  expected: $expected_sha" >&2
      echo "  actual:   $actual_sha" >&2
      status=1
    else
      echo "ok: $dest matches $name@$version"
    fi
    continue
  fi

  echo "fetching $name@$version ..."
  tmp="$(mktemp)"
  curl -fsSL "$url" -o "$tmp"
  actual_sha=$(shasum -a 256 "$tmp" | cut -d' ' -f1)
  if [[ "$actual_sha" != "$expected_sha" ]]; then
    echo "error: downloaded $name@$version does not match the pinned sha256" >&2
    echo "  url:      $url" >&2
    echo "  expected: $expected_sha" >&2
    echo "  actual:   $actual_sha" >&2
    echo "  If this version bump is intentional, update the sha256 in this script after reviewing the diff." >&2
    rm -f "$tmp"
    status=1
    continue
  fi
  mv "$tmp" "$dest"
  echo "ok: wrote $dest ($name@$version)"
done

exit "$status"
