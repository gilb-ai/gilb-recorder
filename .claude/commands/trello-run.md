---
description: Execute Ready for AI cards via worker iterations; auto-merge or escalate to Review
---

# /trello-run

Role: **execution meta-agent**. Invoked manually after the user has approved
plans by dragging cards into `Ready for AI`.

This file is the orchestrator. Worker prompts live in
`.claude/prompts/worker-iter1.md` and `.claude/prompts/worker-iterN.md`.
The acceptance check procedure lives in
`.claude/prompts/acceptance-check.md`. PLAN format lives in
`.claude/prompts/plan-format.md`.

The model is **not one-shot**. Meta spawns a worker, runs an acceptance
check, and if gaps remain, spawns the worker again (up to 3 iterations).
After acceptance passes, meta decides whether to auto-merge the PR or
leave it for human `Review`.

## Contract (what you must NOT do)

- Do NOT write code yourself — only the worker (a `claude -p` process in a
  git worktree) writes code.
- Do NOT open the PR yourself — only the worker (iteration 1) does that.
- Do NOT move a card to `Done` or `Review` without first running the
  acceptance check.
- Do NOT comment in cards without the `[meta] ` or `[worker] ` prefix.
- Do NOT continue working on a card after `Blocked` — the next action is
  the human's.
- Do NOT auto-merge if any auto-merge criterion fails — escalate to `Review`.

## Sources of truth

- `.claude/trello.json` — board, list IDs, `branch_prefix`, `worktree_root`,
  `worker_log_dir`, `auto_merge_criteria`, `session_log`.
- `.claude/prompts/worker-iter1.md`, `worker-iterN.md` — worker prompt templates.
- `.claude/prompts/acceptance-check.md` — verification procedure.
- `.claude/prompts/plan-format.md` — PLAN parsing contract.
- `trello-workflow.md` — full workflow doc.
- `CLAUDE.md` — commit style (worker reads it).
- `.gilb/session-log.md` — recent automation history.

## Algorithm

### Bootstrap (once)

1. Read `.claude/trello.json`. Extract `lists.{ready, in_progress, review, blocked, done}`,
   `branch_prefix`, `worktree_root`, `worker_log_dir`, `auto_merge_criteria`,
   `session_log`.
2. Read last 30 lines of `.gilb/session-log.md` — skim for patterns (e.g.,
   a card you're about to work on was just BLOCKED — check why).
3. Via MCP `trello`, fetch open cards from all columns for cross-card view.
4. Ensure directories exist: `mkdir -p <worktree_root> <worker_log_dir>`.

### Per card (sequential, NOT parallel)

For each card in `Ready for AI`:

1. **Phase 1: Prepare** (see below).
2. **Phase 2: Iteration loop** (see below).
3. **Phase 3: Auto-merge decision** (only if Phase 2 ended in acceptance).
4. **Phase 4: Finalize** (session-log entry; card already moved by prior phases).

If `Ready for AI` is empty → reply "Ready for AI empty" and exit.

### Summary

```
Execution complete:
- Cards processed: <N>
  - → Done (auto-merged): <M>
  - → Review (escalated): <R>
  - → Blocked: <K>
- Mean iterations: <num>
```

---

## Phase 1: Prepare

a. Extract the `[meta] PLAN` comment from the card (latest one if multiple).
   If absent → move card to `Blocked` with
   `[meta] No PLAN comment. Run /trello-check first.` Append session-log
   `BLOCKED | no PLAN`. Skip.

b. Parse PLAN per `.claude/prompts/plan-format.md`. Extract `## Metrics`:
   `confidence`, `risk`, `expected_iterations`. If `## Metrics` missing or
   unparseable → `Blocked` with
   `[meta] PLAN has no parseable ## Metrics. Re-triage required.` Skip.

c. Generate `<slug>` from card title: lowercase, replace `[^a-z0-9-]` with
   `-`, collapse repeats, trim to 40 chars. `<card-short>` = first 8 chars
   of `shortLink`.

d. Branch: `<branch_prefix><card-short>-<slug>`. Worktree:
   `<worktree_root>/<card-short>-<slug>` (relative to repo root).

e. Create worktree:
   ```bash
   git fetch origin main
   git worktree add <worktree-path> -b <branch> origin/main
   ```
   If branch already exists → Blocked,
   `[meta] Branch <branch> already exists. Manual cleanup.` Skip.

f. Move card to `In Progress`. Comment:
   ```
   [meta] Starting work
   Branch: <branch>
   Worktree: <worktree-path>
   PLAN confidence: <conf>/10, risk: <risk>, expected iters: <N>
   Iteration limit: 3
   ```

---

## Phase 2: Iteration loop

In-memory state for this card:
- `iter` = 1
- `MAX_ITER` = 3
- `pr_url` = null
- `iter_log` = []  // list of dicts: {iter, outcome, gaps_count, gaps_summary, log_path}

### Step 2.1 — Spawn worker

Log path: `<worker_log_dir>/<card-short>-iter<iter>.log`.

Build the worker prompt from the template:
- `iter == 1` → `.claude/prompts/worker-iter1.md`, substitute `<card-url>`,
  `<branch>`, `<PLAN-comment>` placeholders.
- `iter > 1` → `.claude/prompts/worker-iterN.md`, substitute `<card-url>`,
  `<iter>`, `<MAX_ITER>`, `<pr_url>`, `<branch>`, `<PLAN-comment>`,
  `<gaps-list>` (from previous iteration's audit comment).

Spawn:
```bash
cd <worktree-path>
claude -p "<prompt>" --output-format text > <log> 2>&1
EXIT=$?
```

### Step 2.2 — Parse worker result

Read the worker log; look at exit code + stdout patterns.

- `EXIT == 0` and contains `PR_URL=<url>`:
  - If `iter == 1` → store `pr_url = <url>`.
  - If `iter > 1` → verify URL matches stored `pr_url`. Mismatch → Blocked
    `[worker] Iter <iter> opened NEW PR <new> instead of pushing to <pr_url>.`
  - Proceed to Step 2.3.
- `EXIT == 2` and contains `BLOCKED: <reason>`:
  - Move to `Blocked`. Comment:
    `[worker] BLOCKED (iter <iter>): <reason>\nLog: <log>`.
  - Append iter_log entry. Exit loop, skip Phase 3, go to Phase 4 (Blocked path).
- Any other (crash, timeout, unexpected exit):
  - Move to `Blocked`. Comment:
    `[worker] Worker crashed (iter <iter>, exit <EXIT>)\nLog: <log>`.
  - Append iter_log entry. Exit loop.

### Step 2.3 — Acceptance check

Run the procedure in `.claude/prompts/acceptance-check.md`. It returns:
- `gaps[]` — list of strings (empty if all checks pass).
- `gaps_summary` — one-line `; `-joined short form.

### Step 2.4 — Decide outcome of this iteration

**`gaps == []`** → acceptance passed.
- Append iter_log: `{iter, outcome: "accepted", gaps_count: 0, gaps_summary: "—", log_path}`.
- Comment in card:
  ```
  [meta] Iteration <iter>: ACCEPTED ✓
  Log: <log>
  ```
- **break** out of loop. Proceed to Phase 3.

**`gaps != []` and `iter < MAX_ITER`**:
- Append iter_log: `{iter, outcome: "needs_fix", gaps_count: <N>, gaps_summary, log_path}`.
- Comment in card (full audit so user need not open PR):
  ```
  [meta] Iteration <iter>: <N> gap(s), retrying

  **Acceptance gaps:**
  1. <gap 1 — file/line/command>
  2. <gap 2>
  ...

  **Worker log:** <log>
  **Will retry as iteration <iter+1>.**
  ```
- Mirror to PR via `gh pr comment <pr-num> --body "..."`:
  ```
  [meta] Review (iter <iter>) — fixes required:

  1. <gap 1>
  2. <gap 2>
  ...

  Iteration <iter+1> will be spawned automatically.
  ```
- Increment `iter`, continue loop.

**`gaps != []` and `iter == MAX_ITER`**:
- Append iter_log: `{iter, outcome: "max_iter_reached", gaps_count: <N>, gaps_summary, log_path}`.
- Move card to `Blocked`. Comment (full history):
  ```
  [meta] BLOCKED after <MAX_ITER> iterations ✗

  **Iteration history:**
  - iter 1: <iter_log[0].outcome> — <gaps_count>: <gaps_summary>
  - iter 2: <iter_log[1].outcome> — <gaps_count>: <gaps_summary>
  - iter 3: <iter_log[2].outcome> — <gaps_count>: <gaps_summary>

  **Remaining gaps after iter <MAX_ITER>:**
  1. <gap 1>
  ...

  **PR:** <pr_url>
  **Logs:** <worker_log_dir>/<card-short>-iter*.log

  Manual intervention needed. After fixing, move card to Ready for AI or
  Backlog.
  ```
- Mirror short gap list to PR via `gh pr comment`.
- Exit loop. Skip Phase 3. Go to Phase 4 (Blocked path).

---

## Phase 3: Auto-merge decision

Only runs if Phase 2 ended with acceptance passing (gaps empty).

Read `auto_merge_criteria` from `.claude/trello.json`:
- `min_confidence` (default 7)
- `max_risk` (default "medium")
- `require_ci_green` (default true)
- `strategy` (default "merge")

Collect failures into `merge_blockers[]`.

### Check 1: Confidence
- From PLAN `## Metrics: Confidence: <N>`.
- Pass if `confidence >= min_confidence`.
- Fail: `Confidence <N> < required <min_confidence>`.

### Check 2: Risk
- From PLAN `## Metrics: Risk: <low|medium|high>`.
- Pass if `risk` is at or below `max_risk` (order: low < medium < high).
- Fail: `Risk <risk> > max allowed <max_risk>`.

### Check 3: CI green
- If `require_ci_green` is false → skip.
- `gh pr checks <pr_url>` or `gh pr view <pr_url> --json statusCheckRollup`.
- Pass if all required checks are SUCCESS. If no checks are configured for
  the repo at all → treat as pass and note "no CI configured" in audit.
- Fail per failing check: `CI: <check name>: <status>`.

### Decision

**`merge_blockers == []`** → **auto-merge**.
```bash
gh pr merge <pr_url> --merge --delete-branch
```
(Strategy: `merge` = merge commit, per user choice. If `auto_merge_criteria.strategy`
is something else, adjust the flag: `--squash` or `--rebase`.)

Move card to `Done`. Comment:
```
[meta] AUTO-MERGED ✓
PR: <pr_url>
Strategy: <strategy>
Iterations: <iter> / 3
Confidence: <N>/10, Risk: <risk>
Branch deleted (origin).

Iteration history:
- iter 1: <outcome> — <gaps_summary>
- iter 2: <outcome> — <gaps_summary>  (if applicable)
```

Append session-log: `<ts> <card-short> MERGED | iter=<N> conf=<N> PR#<num>`.

Skip Phase 4 (finalized here).

**`merge_blockers != []`** → **escalate to Review**.

Move card to `Review`. Comment:
```
[meta] READY FOR REVIEW — auto-merge skipped

**Auto-merge blockers:**
1. <blocker 1>
2. <blocker 2>
...

PR: <pr_url>
Iterations: <iter> / 3

Iteration history:
- iter 1: <outcome> — <gaps_summary>
- iter 2: <outcome> — <gaps_summary>  (if applicable)

Acceptance check passed; the criteria above kept this from auto-merge. Please review.
```

Append session-log: `<ts> <card-short> REVIEW | iter=<N> blockers=<count>: <comma-joined>`.

Skip Phase 4 (finalized here).

---

## Phase 4: Finalize

Catch-all: append the session-log entry if not yet written. Paths from
Phase 2 (Blocked due to crash, blocked, or max-iter) come here without
writing — handle them:

- Crash/blocked: `<ts> <card-short> BLOCKED | <reason short form>`
- Max-iter: `<ts> <card-short> BLOCKED | max-iter exhausted, <N> gaps: <gaps_summary>`

For long-running cards, also write `STARTED` at the end of Phase 1 (gives
visibility into in-flight work). Then the terminal event (`MERGED` /
`REVIEW` / `BLOCKED`) replaces or follows.

**Do not** remove the worktree. It stays for human inspection / re-iteration.

---

## Failure modes

| Situation | Action |
|---|---|
| `gh` not authenticated | Stop. Already-processed cards keep their status. |
| Worktree path occupied by remnants | Blocked: "worktree exists, manual cleanup". Do not delete. |
| Worker needs env vars (secrets) not in worktree env | Blocked. Don't forward secrets yourself. |
| PR conflicts with main by acceptance time | Gap: "PR has merge conflicts with main. Rebase needed." → iteration. |
| Worker iter N opened NEW PR instead of pushing existing | Blocked: explicit message. |
| MCP `trello` fails mid-card | Stop. Card stays in current state. |
| Worker log empty / unreadable | Blocked: "Worker produced empty log." |
| Auto-merge succeeds but card move to Done fails | Comment in card that merge happened; chat error. Manual card move. |
| `gh pr merge` fails (branch protection, conflicts) | Treat as auto-merge blocker; move to Review with `gh` error in comment. |
| Worker prompt template file (`worker-iter1.md`, `worker-iterN.md`) missing | Stop. Don't inline a fallback. |
| `acceptance-check.md` missing | Stop. Don't skip acceptance. |
