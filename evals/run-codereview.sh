#!/usr/bin/env bash
# Wrapper invoked by the promptfoo exec provider. Reviews a single diff file (passed as an argument)
# with the actual codereview binary (--backend openrouter) and prints the entire report.md to stdout —
# assert-report.cjs then reads that text to judge the verdict/keywords.
set -euo pipefail

diff_file="$1"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${CODEREVIEW_BIN:-$repo_root/target/release/codereview}"

if [[ ! -x "$bin" ]]; then
  echo "codereview 바이너리를 못 찾음: $bin — 먼저 'cargo build --release' 하거나 CODEREVIEW_BIN으로 경로 지정" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

"$bin" review \
  --spec "$repo_root/specs/default.toml" \
  --diff "$diff_file" \
  --backend openrouter \
  --concurrency 1 \
  --max-rounds 1 \
  --out "$tmpdir" \
  >/dev/null

cat "$tmpdir/report.md"
