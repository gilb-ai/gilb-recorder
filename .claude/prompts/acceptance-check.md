# Acceptance check procedure

Sub-prompt called by `/trello-run` after each worker iteration to verify the
worker's deliverables match the PLAN. The orchestrator collects gaps and
decides next steps; this file is the verification logic.

## Input

When this procedure starts:
- The worker has just exited 0 with `PR_URL=<url>` in stdout.
- `<pr_url>` is known.
- The worktree path is the cwd of the worker (and you can `cd` there).
- The original `[meta] PLAN` is in your context (parsed per `plan-format.md`).
- You have meta-agent tools: Bash, Read, gh CLI, MCP `trello`.

## Output

A list `gaps[]` of short strings describing each failed check. Empty list =
acceptance passes. Also a one-line `gaps_summary` = `; `-joined short forms of
all gaps, used by the orchestrator for the session-log entry and the
at-a-glance audit comment.

## The 8 checks

Run all of them. Each failure adds to `gaps[]`. Do NOT short-circuit — the
human deserves a full picture if multiple things broke.

### 1. Files coverage

```bash
cd <worktree>
git diff --name-only origin/main...HEAD
```

Compare against `## Files` in the PLAN. Each plan entry must appear in the
diff, except entries with `(no code change)` annotation. For `(new)` entries
— confirm the file exists (`ls`).

Gap form: `File X from plan not modified` or `File Y (new) not created`.

### 2. No scope creep

Does the diff include files NOT in `## Files`?

Allowed exceptions:
- `Cargo.lock` — auto-updates with dependency changes.
- `.gitignore` — if new artifacts need ignoring.

Otherwise add a gap: `File X modified but not in plan. Justify or revert.`

### 3. Out of scope respected

Read `## Out of scope` from the PLAN. Verify the diff does not violate any
item.

Gap form: `Out of scope violated: <which item, what change>`.

### 4. Tests pass

For each command in `## Tests`:
```bash
cd <worktree>
<command>
```

Verify exit 0. No `--no-run`, no dry flags — actually execute.

Gap form: `Test <command> failed. Output: <last 20 lines>`.

### 5. Clippy clean

```bash
cd <worktree>
cargo clippy --workspace --all-targets 2>&1 | tail -50
```

Exit 0 AND no `warning:` lines in output.

Gap form: `Clippy: <first warning/error line>`.

### 6. Formatting clean

```bash
cd <worktree>
cargo fmt --all -- --check
```

Exit 0.

Gap form: `Formatting: cargo fmt --all -- --check failed`.

### 7. PR metadata correct

```bash
gh pr view <pr_url> --json title,body
```

Verify:
- `title` is not empty, not a default ("wip", "test"). The card's title is
  acceptable.
- `body` first line is exactly `Trello: <card-url>` (URL match).
- Body contains `## What`, `## Why`, `## Test plan` sections (markdown
  headers).

Gap form: `PR body missing section <X>` or `PR does not link to card`.

### 8. Commits hygiene

```bash
cd <worktree>
git log --format="%H%n%s%n%b%n---" origin/main..HEAD
```

For each commit:
- Subject ≤72 chars.
- Subject in imperative mood (Add X, Fix Y; not Added/Fixed).
- Body (if present) wraps near 72 columns.
- Footer line `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>` present.

Gap form: `Commit <short-sha>: <specific violation>`.

## After checks

Return:
- `gaps[]` — list of strings, each one full gap entry from the checks above.
- `gaps_summary` — one-line `; `-joined short forms (e.g.,
  `"Files: X missing; Clippy: 2 warnings; Commits: abc1234 no footer"`).

Empty `gaps[]` means acceptance passes — the orchestrator proceeds to the
auto-merge decision.

## Diagnostic tips (not gaps)

These are not gaps, but worth noting in the audit comment if observed:

- Test command from the plan does not exist yet (no `tests/` for that crate)
  — gap, but the framing should suggest extending the plan, not just
  retrying.
- All tests pass but coverage of the new code is suspiciously low (e.g.,
  no test touches the new code path) — not auto-flagged in v1; relies on
  the human reviewer at `Review`.
- Worker fixed the gap but introduced a different issue in a way that
  acceptance passes — same: not auto-flagged. Human catches at Review.

The acceptance check is a hygiene gate, not a code review. Auto-merge
trusts it; for everything else, the `Review` column exists.
