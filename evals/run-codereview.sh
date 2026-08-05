#!/usr/bin/env bash
# promptfoo exec provider가 호출하는 래퍼. 인자로 받은 diff 파일 하나를 실제 codereview
# 바이너리(--backend openrouter)로 리뷰하고, report.md 전체를 표준출력으로 낸다 —
# assert-report.cjs가 그 텍스트를 보고 verdict/키워드를 판정한다.
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
