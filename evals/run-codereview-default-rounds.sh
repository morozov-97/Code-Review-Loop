#!/usr/bin/env bash
# #176: identical to run-codereview.sh except it does NOT force --max-rounds 1 — lets the CLI's
# own default (2 rounds) apply. Every other eval case uses the fast single-round path, so the
# suite as a whole never exercised the actual default-path cost/behavior; this is the one case
# that does (wired to the sql-injection fixture in promptfooconfig.yaml, since that's the case
# with documented cross-round non-determinism — see this directory's README).
set -euo pipefail

diff_file="$1"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${CODEREVIEW_BIN:-$repo_root/target/release/codereview}"

if [[ ! -x "$bin" ]]; then
  echo "codereview binary not found: $bin — run 'cargo build --release' first, or set CODEREVIEW_BIN to its path" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

"$bin" review \
  --spec "$repo_root/specs/default.toml" \
  --diff "$diff_file" \
  --backend openrouter \
  --concurrency 1 \
  --out "$tmpdir" \
  >/dev/null

cat "$tmpdir/report.md"
echo ""
calls="$(grep -m1 '"calls"' "$tmpdir/manifest.json" 2>/dev/null | grep -o '[0-9]\+' | head -1)"
echo "<!-- MANIFEST_PROVIDER_CALLS: ${calls:-unknown} -->"
