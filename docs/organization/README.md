# Commit governance and release flow (private base)

This document defines commit/PR operating rules based on the `Code-Review-Loop` repository.
Items that require public repository sync are reflected as a sanitized version in the `public`
repo according to the `sync-policy` below.

## 1) Purpose

- Maintain code review quality (clear change history, reproducible merge records)
- Don't expose sensitive information in the public repository, but keep operating standards consistent
- Split PR-level reviews into small, confirmable units to reduce token/time cost

## 2) Commit rules (Conventional Commit)

### Format

`<type>(<scope>): <summary>`

Examples:
- `feat(review): isolate deterministic manifest assembly`
- `fix(policy): reduce false negative in test-surface guard`
- `docs(governance): add commit rules and PR validation checklist`
- `chore(ci): pin benchmark runner dependencies`

### Recommended types

- `feat`: Add a feature
- `fix`: Fix a bug/behavior
- `docs`: Documentation
- `refactor`: Behavior-preserving code structure improvement
- `perf`: Performance/efficiency improvement
- `test`: Strengthen tests
- `chore`: Build/tooling/meta changes
- `ci`: CI/CD pipeline changes
- `revert`: Rollback

### Summary rules

- Write `summary` in around 72 characters, using the imperative mood
- Don't chain multiple items with `AND`/`or` — state the core effect in one line
- No unnecessary prefixes (`WIP`, `temp`, `fix maybe`)

### Body (recommended)

Commits with just a one-line summary are allowed, but for impactful work, the structure below is recommended.

```text
Motivation: why it's needed (current problem/requirement)
Changes: what was changed (key files/reasons)
Validation: how it was validated (test/benchmark commands)
Risk & rollback: risk and how to revert
```

## 3) PR checklist (required)

`[ ]` Reflect the commit type/scope in the PR title

`[ ]` Verify the rationale for the change is linked to a README/design doc or an issue

`[ ]` Briefly note the number of affected files and the test scope

`[ ]` Attach a link to measurement artifacts when performance/precision/token cost changes

`[ ]` Specify checklist items for changes involving security/permissions/sensitive paths

`[ ]` Confirm whether public-release sync is needed

## 4) Branch & PR policy

- Default branch: `main`
- Feature/doc branches: `chore/*`, `feat/*`, `fix/*`
- As a rule, limit the scope of change per PR:
  - When only `docs` need to change, mark it with a `docs-only` label or in the title
  - Changes with broad simultaneous impact should be one PR, one purpose
- Avoid merging before review is complete; use the `merge` label only for items eligible for auto-merge

## 5) Public-release sync (public sync)

The publishable version of private rules is managed under the policy below.

- `Code-Review-Loop/docs/organization/` = **single source of truth**
- Only the items below are reflected in the public release (`full-review-benchmark-public`):
  1. Commit message format
  2. PR checklist
  3. Common branching/release principles
- `claude-config` sync targets:
  1. `skills/full-review/SKILL.md`
  2. `skills/full-review/workflow.js`
  3. `skills/full-review/scripts/*.mjs`
  4. `skills/full-review/references/*.md`
- Excluded from the public release:
  - Internal discussion logs
  - Private issue/account policy
  - Internal cost policy (e.g. account/license-specific restrictions)

Skill bundle sync is done via `full-review-benchmark-public`'s automated workflow, which creates/updates
a PR in `claude-config`.

If desired, manual sync can be verified locally.

- `full-review-benchmark-public/.github/workflows/sync-to-claude-config.yml`: automatic sync via GitHub Actions
- `full-review-benchmark-public/scripts/sync-to-claude-config-pr.sh`: for manual execution/verification
- `full-review-benchmark-public/scripts/sync-to-claude-config.sh --target <path> --commit`: temporary local sync

For rule/doc changes that require sync, it's recommended to bundle the public PR, the `claude-config` PR,
and the public repo PR/doc sync PR together in one operation.

## 7) Automated check for governance change sync

When governance files ( `docs/organization/`, PR template ) change, use the script below to quickly determine whether the public release needs updating.

```bash
./scripts/gov-sync-check.sh --base origin/main --head HEAD
```

Optional flags:

- `--base <ref>`: comparison base revision (default: `origin/main`)
- `--head <ref>`: comparison target revision (default: `HEAD`)
- `--out <file>`: save the checklist to a file

Recommended operation:

- When modifying `docs/organization/README.md` or `public-sync-mapping.md`,
  generate the checklist and paste it into the PR body's `Governance check` section.
- If the change requires public-release sync, update the public version in the same rule doc
  and immediately create the public PR.

## 8) Dispute/exception handling

- On rule conflicts, record the reasoning in the PR template's `Decision` section
- Label urgent fixes as `chore/security` or `chore/emergency`, and address whether any rule was violated
  in a post-hoc retrospective

## 9) Additional rationale/research reference document

- [research-and-evidence-survey-2026-07-29.md](research-and-evidence-survey-2026-07-29.md): an internal reference document summarizing the latest benchmarks/papers, implementation references, and tuning priorities from the perspective of code review accuracy, false-positive rate, and token cost.

When making operational/documentation changes, it's recommended to consult this reference document and add one line to the PR body noting whether the latest evidence has been reflected.
