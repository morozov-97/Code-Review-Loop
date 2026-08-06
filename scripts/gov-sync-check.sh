#!/usr/bin/env bash

set -euo pipefail

BASE_REF="origin/main"
HEAD_REF="HEAD"
OUT_FILE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)
      BASE_REF="$2"
      shift 2
      ;;
    --head)
      HEAD_REF="$2"
      shift 2
      ;;
    --out)
      OUT_FILE="$2"
      shift 2
      ;;
    -h|--help)
      cat <<EOF
Usage: $(basename "$0") [--base <ref>] [--head <ref>] [--out <file>]

  --base  base ref for diff (default: origin/main)
  --head  head ref for diff (default: HEAD)
  --out   write checklist to file in addition to stdout
EOF
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 1
      ;;
  esac
done

if ! git rev-parse --verify "$BASE_REF" >/dev/null 2>&1; then
  echo "[WARN] base ref '$BASE_REF' not found locally. Trying remote fetch..." >&2
  git fetch origin "$BASE_REF" >/dev/null 2>&1 || true
fi

if ! git rev-parse --verify "$HEAD_REF" >/dev/null 2>&1; then
  echo "[WARN] head ref '$HEAD_REF' not found locally. Trying fetch..." >&2
  git fetch origin "$HEAD_REF" >/dev/null 2>&1 || true
fi

changed_files=()
while IFS= read -r file; do
  changed_files+=( "$file" )
done < <(
  git diff --name-only "$BASE_REF...$HEAD_REF" -- docs/organization .github/pull_request_template.md | sed '/^$/d'
)

if [[ ${#changed_files[@]} -eq 0 ]]; then
  output="No governance files changed between $BASE_REF and $HEAD_REF."
  echo "$output"
  if [[ -n "$OUT_FILE" ]]; then
    printf '%s\n' "$output" > "$OUT_FILE"
  fi
  exit 0
fi

needs_public_sync="no"
if printf '%s\n' "${changed_files[@]}" | grep -E -q '^docs/organization/|^\.github/pull_request_template\.md$'; then
  needs_public_sync="yes"
fi

timestamp=$(date -u '+%Y-%m-%d %H:%M:%SZ')

output="# Governance sync checklist

## Generated at: $timestamp

## Comparison range
- base: <code>$BASE_REF</code>
- head: <code>$HEAD_REF</code>

## Changed governance files
"
for f in "${changed_files[@]}"; do
  output+="- $f
"
done

output+="
## Public-release sync required: $needs_public_sync
"

if [[ "$needs_public_sync" == "yes" ]]; then
  output+="
### Public sync action list
"
  output+="- [ ] Re-check whether the public repo's PR template / contribution rules changed
"
  output+="- [ ] Update docs/COMMIT-RULES-PUBLIC.md to include only publishable items
"
  output+="- [ ] Confirm the public repo's README links are up to date
"
  output+="- [ ] Summarize the reason/rationale for the change (excluding sensitive info) in the PR body
"
  output+="- [ ] Record the public PR number/URL for tracking
"
  output+="
Recommended: in the private PR body, mark \"Required\" for \"Public-release sync required\" and list the target public files.
"
else
  output+="
### Action
- [ ] Confirm why the scope of this internal rule change is excluded from public sync
"
fi

if [[ -n "$OUT_FILE" ]]; then
  printf '%s\n' "$output" > "$OUT_FILE"
  echo "Wrote checklist to: $OUT_FILE"
fi

printf '%s\n' "$output"
