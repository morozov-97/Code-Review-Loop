#!/usr/bin/env bash
# #209: runs the single-lens baseline N times independently per diff (no shared state between
# runs), so aggregate.py-style analysis can test self-consistency (majority-vote across
# independent passes) against the full-pipeline and single-pass numbers from run_benchmark.sh.
#
# Usage:
#   python3 extract.py --repo /path/to/target/repo --out-dir ./szz-out
#   ./run_self_consistency.sh --repo /path/to/target/repo --szz-dir ./szz-out --out-dir ./szz-out --reps 3
#
# Writes ./szz-out/self-consistency/<case>/rep-<N>/{report.md,manifest.json,stdout.log}.
set -euo pipefail

repo=""
szz_dir=""
out_dir=""
reps=3
parallel=3
spec=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo) repo="$2"; shift 2 ;;
    --szz-dir) szz_dir="$2"; shift 2 ;;
    --out-dir) out_dir="$2"; shift 2 ;;
    --reps) reps="$2"; shift 2 ;;
    --parallel) parallel="$2"; shift 2 ;;
    --spec) spec="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$repo" || -z "$szz_dir" || -z "$out_dir" ]]; then
  echo "usage: $0 --repo <path> --szz-dir <path from extract.py, or with diffs/ already extracted> --out-dir <path> [--reps N] [--parallel N] [--spec <path>]" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
bin="${CODEREVIEW_BIN:-$repo_root/target/release/codereview}"
spec="${spec:-$repo_root/specs/default.toml}"
sc_dir="$out_dir/self-consistency"

if [[ ! -x "$bin" ]]; then
  echo "codereview binary not found: $bin -- run 'cargo build --release' first" >&2
  exit 1
fi

if [[ ! -d "$szz_dir/diffs" ]]; then
  echo "no diffs/ under $szz_dir -- run run_benchmark.sh (or extract.py + its diff-extraction step) first" >&2
  exit 1
fi

mkdir -p "$sc_dir"

run_one() {
  local diff_file="$1" rep="$2"
  local base
  base="$(basename "$diff_file" .patch)"
  local case_out="$sc_dir/$base/rep-$rep"
  mkdir -p "$case_out"
  ( cd "$repo" && "$bin" review --spec "$spec" --diff "$diff_file" --backend openrouter \
      --lenses "" --concurrency 1 --max-rounds 1 --out "$case_out" ) > "$case_out/stdout.log" 2>&1
  echo "done: $base rep-$rep (exit=$?)"
}
export -f run_one
export bin spec sc_dir repo

jobs_file="$(mktemp)"
trap 'rm -f "$jobs_file"' EXIT
for f in "$szz_dir"/diffs/*.patch; do
  for r in $(seq 1 "$reps"); do
    echo "$f $r" >> "$jobs_file"
  done
done

xargs -P "$parallel" -n 2 bash -c 'run_one "$@"' _ < "$jobs_file"

echo "Any case whose stdout.log shows 'refusing to send diff' hit the local secret scanner --" >&2
echo "inspect it by hand before deciding whether to re-run with --allow-sensitive-input." >&2
