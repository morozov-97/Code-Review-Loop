#!/usr/bin/env bash
# Runs the actual codereview binary against every diff produced by extract.py's positive/negative
# sets and writes one report per case under --out-dir. Real API calls -- costs real money and
# takes real wall-clock time (roughly N * 1-2 minutes at the default concurrency below).
#
# Usage:
#   python3 extract.py --repo /path/to/target/repo --out-dir ./szz-out
#   ./run_benchmark.sh --repo /path/to/target/repo --szz-dir ./szz-out --out-dir ./szz-out/results
#
# Requires OPENROUTER_API_KEY set and a release build at ../../target/release/codereview (relative
# to this script) unless CODEREVIEW_BIN overrides it.
set -euo pipefail

repo=""
szz_dir=""
out_dir=""
parallel=3
spec=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo) repo="$2"; shift 2 ;;
    --szz-dir) szz_dir="$2"; shift 2 ;;
    --out-dir) out_dir="$2"; shift 2 ;;
    --parallel) parallel="$2"; shift 2 ;;
    --spec) spec="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$repo" || -z "$szz_dir" || -z "$out_dir" ]]; then
  echo "usage: $0 --repo <path> --szz-dir <path from extract.py> --out-dir <path> [--parallel N] [--spec <path>]" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
bin="${CODEREVIEW_BIN:-$repo_root/target/release/codereview}"
spec="${spec:-$repo_root/specs/default.toml}"

if [[ ! -x "$bin" ]]; then
  echo "codereview binary not found: $bin -- run 'cargo build --release' first" >&2
  exit 1
fi

mkdir -p "$out_dir/diffs"

python3 - "$repo" "$szz_dir" "$out_dir/diffs" <<'PYEOF'
import json, subprocess, sys, os
repo, szz_dir, diffs_dir = sys.argv[1], sys.argv[2], sys.argv[3]
manifest = []
for label, fname in [("positive_bic", "positive.json"), ("negative_clean", "negative.json")]:
    path = os.path.join(szz_dir, fname)
    if not os.path.exists(path):
        continue
    entries = json.load(open(path))
    prefix = "pos" if label == "positive_bic" else "neg"
    for i, e in enumerate(entries):
        out_name = f"{prefix}-{i:02d}-{e['sha'][:8]}.patch"
        diff = subprocess.run(["git", "show", e["sha"]], cwd=repo, capture_output=True, text=True, check=True).stdout
        open(os.path.join(diffs_dir, out_name), "w").write(diff)
        manifest.append({**e, "file": out_name})
json.dump(manifest, open(os.path.join(diffs_dir, "..", "manifest.json"), "w"), indent=2, ensure_ascii=False)
print(f"extracted {len(manifest)} diffs", file=sys.stderr)
PYEOF

run_one() {
  local diff_file="$1"
  local base
  base="$(basename "$diff_file" .patch)"
  local case_out="$out_dir/results/$base"
  mkdir -p "$case_out"
  ( cd "$repo" && "$bin" review --spec "$spec" --diff "$diff_file" --backend openrouter \
      --concurrency 2 --max-rounds 1 --out "$case_out" ) > "$case_out/stdout.log" 2>&1
  echo "done: $base (exit=$?)"
}
export -f run_one
export bin spec out_dir repo

find "$out_dir/diffs" -name '*.patch' | xargs -P "$parallel" -I{} bash -c 'run_one "$@"' _ {}

echo "Any case whose stdout.log shows 'refusing to send diff' hit the local secret scanner --" >&2
echo "inspect it by hand before deciding whether to re-run with --allow-sensitive-input." >&2
