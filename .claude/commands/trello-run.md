---
description: Execute Ready for AI cards via worker iterations; auto-merge or escalate to Review. Accepts an optional single-card ref and an optional --parallel N flag.
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

## Invocation

Three forms, all run from a Claude Code session in the repo:

- `/trello-run` — process every card currently in `Ready for AI`,
  sequentially.
- `/trello-run <card-ref>` — process exactly one card in `Ready for AI`.
  `<card-ref>` accepts any of:
  - `shortLink`, e.g. `6WV4zR2P`
  - `<prefix>-<idShort>`, e.g. `GILB-3` (prefix from
    `.claude/trello.json` `card_prefix`)
  - full Trello URL, e.g. `https://trello.com/c/6WV4zR2P` or
    `https://trello.com/c/6WV4zR2P/3-gilb-3-mcp-endpoints`

  If the resolved card is NOT in `Ready for AI` → reply
  `Card <ref> is not in Ready for AI (currently in <list>). Move it to Ready for AI first.`
  and exit without changes.

- `/trello-run --parallel N` (also accepted: `-p N`, `--p N`, `--pN`,
  `-pN`) — process every Ready-for-AI card with up to **N** workers
  running concurrently. `N` is clamped to `[1, 4]`. Default without the
  flag is `N=1` (sequential). The flag is allowed together with a
  card-ref, but for a single-card invocation it's a no-op (one card =
  at most one worker).

Combinations:
- `/trello-run GILB-3` → exactly that card, sequential by construction.
- `/trello-run -p2` → all Ready cards, up to 2 in flight.
- `/trello-run --parallel 3 GILB-3` → single card; flag ignored with a
  warning line.

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

1. Parse the invocation (see `## Invocation`):
   - Detect `--parallel N` / `-p N` / `--p N` / `--pN` / `-pN`. Clamp to
     `[1, 4]`. Default `N=1`. Reject non-integer / out-of-range with
     `Invalid --parallel value: <raw>. Expected integer in [1, 4].` and
     exit.
   - The remaining non-flag positional, if any, is `<card-ref>`. Reject
     `≥2` positionals with
     `Multiple card refs given. /trello-run accepts at most one card ref.`
     and exit.
2. Read `.claude/trello.json`. Extract
   `lists.{ready, in_progress, review, blocked, done}`, `branch_prefix`,
   `worktree_root`, `worker_log_dir`, `auto_merge_criteria`, `session_log`,
   `card_prefix`.
3. Read last 30 lines of `.gilb/session-log.md` — skim for patterns
   (e.g., a card you're about to work on was just BLOCKED — check why).
4. Via MCP `trello`, fetch open cards from all columns for cross-card
   view, AND specifically the contents of the `ready` list. From these:
   - **If `<card-ref>` was given:** resolve it (see "Card ref resolution"
     below). If the resolved card isn't in `ready` → exit per the
     Invocation rules. Targets list = `[that card]`.
   - **Otherwise:** targets list = all cards currently in `ready`. If
     empty → reply "Ready for AI empty" and exit.
5. Ensure directories exist: `mkdir -p <worktree_root> <worker_log_dir>`.

#### Card ref resolution

Apply these rules in order against the user-supplied `<card-ref>`:

1. If it matches `^https?://trello\.com/c/([A-Za-z0-9]{8})(?:/.*)?$` →
   `shortLink` = group 1.
2. Else if it matches `^[A-Za-z0-9]{8}$` → `shortLink` = the ref itself.
3. Else if it matches `^<card_prefix>-(\d+)$` (case-insensitive on
   `card_prefix`) → look up the card whose `idShort` equals that
   integer on the board.
4. Else → exit with
   `Unrecognized card ref: <ref>. Expected shortLink, <prefix>-<id>, or trello.com/c/ URL.`

If shortLink lookup yields no card → exit with
`Card <ref> not found on board.`.

### Per card

For each card in the resolved targets list:

1. **Phase 1: Prepare** (see below).
2. **Phase 2: Iteration loop** (see below).
3. **Phase 3: Auto-merge decision** (only if Phase 2 ended in acceptance).
4. **Phase 4: Finalize** (session-log entry; card already moved by prior phases).

Execution order:
- **`N == 1`** — strictly sequential: one card finishes (through Phase 4)
  before the next begins.
- **`N > 1`** — meta keeps up to `N` cards "in flight" (each in its own
  Phase 2 iteration loop, with its own worktree/branch/PR/log). Cards
  enter and leave in flight independently. See `## Parallel execution`
  for the orchestration contract.

### Summary

Print once at the end (after all in-flight cards have finished):

```
Execution complete (parallelism N=<N>):
- Cards processed: <count>
  - → Done (auto-merged): <M>
  - → Review (escalated): <R>
  - → Blocked: <K>
- Mean iterations: <num>
```

For single-card invocation, replace `Cards processed: <count>` with the
card title + shortLink.

---

## Parallel execution

Only applies when `N > 1` AND the targets list has more than one card.

### Concurrency model

Meta keeps a queue of pending cards and a set of in-flight cards (size
≤ `N`). The unit of concurrency is **one card's Phase 1+2 pipeline** —
the meta-agent never runs two iteration loops in series for the same
card simultaneously, only across different cards.

Per-card pipeline is unchanged: Phase 1 → Phase 2 (iter loop) → Phase 3
(auto-merge decision) → Phase 4 (finalize). The acceptance check
(Phase 2.3) and the worker spawn (Phase 2.1) are both per-card, so they
run interleaved across the in-flight set.

### Spawning loop

```
pending  = targets list (FIFO)
in_flight = {}            # cardId → {worktree, branch, pr_url, iter, log_path, ...}

while pending or in_flight:
    while pending and len(in_flight) < N:
        card = pending.pop(0)
        run Phase 1 for card           # sync, fast (worktree create + Trello move)
        spawn iter 1 worker for card   # async (background)
        in_flight[card.id] = state

    wait for ANY in-flight worker to finish    # harness notification
    for each finished card:
        run Phase 2.2 (parse) + 2.3 (acceptance) + 2.4 (decide)
        if needs another iteration:
            spawn iter <iter+1> worker for card    # stays in_flight
        else:
            run Phase 3 (auto-merge decision) + Phase 4
            remove from in_flight
```

### Concurrency-specific rules

- **Phase 1 is serialized.** Worktree creation, the `In Progress` move,
  and the `[meta] Starting work` comment for the next card all happen
  on the meta-agent's main thread before the next worker spawn. This
  keeps the Trello card list ordered and avoids `git` racing itself in
  the parent repo.
- **Worker spawns are async.** Each is `claude -p ... &` (background)
  with its log path written to its own file in `<worker_log_dir>`.
- **Acceptance checks are serialized** per finished worker. Meta runs
  one acceptance procedure at a time (it competes for the same `cargo`
  / `gh` / Trello-API tools as Phase 1 and Phase 3); this prevents
  cargo registry locks and Trello rate-limit storms.
- **Auto-merge decisions are serialized.** Only one `gh pr merge` at a
  time. If two cards both reach Phase 3, the second waits.
- **Per-card iteration counter is independent.** Card A's iter 3 does
  not affect card B's iter limits.
- **Stop condition.** A `Blocked` decision for one card never aborts
  in-flight work on other cards. The summary at the end reports per-card
  outcomes.

### Cross-card conflicts (best-effort handling)

- Two PRs that both touch overlapping files in `main`: when the second
  one tries `gh pr merge` after the first has merged, conflicts may
  appear. Treat as an auto-merge blocker per "Phase 3" → escalate to
  `Review` with the `gh` error message in the comment.
- Two workers running cargo against the same workspace from different
  worktrees: each worktree has its own `target/`, so this is allowed.
  If RAM pressure is a concern, lower `N`.
- `MCP trello` rate-limit: if a Trello API call fails with HTTP 429,
  retry once after 5s; on second failure, treat as `MCP trello fails
  mid-card` per Failure modes → stop. Leave already-spawned workers to
  finish but do not start new ones.

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
| `<card-ref>` not in `Ready for AI` | Exit early per Invocation rules; no state change. |
| `<card-ref>` resolves to no card on the board | Exit with `Card <ref> not found on board.` |
| `--parallel N` with `N` outside `[1, 4]` or non-integer | Exit with `Invalid --parallel value: <raw>. Expected integer in [1, 4].` |
| `--parallel N` set but only one card in targets | Treat as `N=1`; no warning needed. |
| Parallel run, one card hits `Blocked` | Other in-flight cards continue. Pending queue continues to drain. |
