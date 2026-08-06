# Public sync mapping (private -> public)

## Reference repos

- Private canonical: `kimdzhekhon/Code-Review-Loop`
- Public mirror (sanitized): `kimdzhekhon/full-review-benchmark-public` (workflow benchmark repo)
- Private tooling mirror (automated sync via public workflow): `kimdzhekhon/claude-config`

## Sync targets

- Commit/PR message rules
- Common principles for change scope and impact
- Routine for checking security/permission changes
- Principles for release/metric reporting suitable for public disclosure
- `skills/full-review` bundle (in principle, a sync target when public judgment/execution policy changes)

## Excluded from sync

- Internal operational decision rationale and raw audit-metric data
- Specific user/account/organization information
- Detailed figures from internal security/cost policy

## Proposed cadence

- When a rule changes: one internal PR + one public PR
- Quarterly: a 10-minute script review checking rule adherence rate (add automation if needed)
- End of quarter: check for doc-execution mismatches (whether template/rule items are actually reflected in PRs)

## Sync method

- Determine whether public sync is needed using the `~/.github/pull_request_template.md`
  and `scripts/gov-sync-check.sh` checklist.
- The `full-review` skill bundle runs automated PR sync from the public repo to
  `claude-config`.
  - Automatic path:
    - `full-review-benchmark-public/.github/workflows/sync-to-claude-config.yml`
    - `full-review-benchmark-public/scripts/sync-to-claude-config-pr.sh`
  - Manual path (local debugging):
    - `full-review-benchmark-public/scripts/sync-to-claude-config.sh --target ../claude-config`
    - Use `--commit` to create a temporary local commit

## Operational automated check checklist

- When editing governance files in `kimdzhekhon/Code-Review-Loop`, run `scripts/gov-sync-check.sh --base origin/main --head HEAD`.
- If the result is `Public-release sync required: Yes`, attach the public-release plan to the PR body.
- Once the public release is updated, mark the same checklist item as `Done`.
