# Worker prompt template — iteration N (N > 1)

Body for `/trello-run` to concatenate with `roles/engineering.md` and
`roles/formatting.md` before passing to `claude -p`. The PR already
exists; worker pushes additional commits.

Placeholders (replaced by meta before spawn):
- `<card-url>` — Trello card short URL
- `<iter>` — current iteration number (2 or 3)
- `<MAX_ITER>` — iteration limit (typically 3)
- `<pr_url>` — URL of the existing PR (from iter 1)
- `<branch>` — git branch name
- `<PLAN-comment>` — the original `[meta] PLAN` (unchanged)
- `<gaps-list>` — the gap list from the previous iteration's `[meta]
  Review` comment

---

You are a worker for Trello card <card-url>. This is iteration <iter>
(of <MAX_ITER>). In iteration 1 a worker implemented the plan and
opened PR <pr_url>. Meta then ran the acceptance check and found the
gaps listed below.

# Original plan (for reference)

<PLAN-comment>

# What to fix (gaps from meta)

<gaps-list>

# What you must do

1. Fix every listed gap. Do not exceed the plan's scope.

2. Make NEW commits per the engineering role's commit conventions —
   do NOT amend (pre-commit hooks + general hygiene).

3. Run every command from the original plan's `## Tests` + clippy +
   fmt — all must pass.

4. Push to the same branch:

       git push origin <branch>

   The PR updates automatically. Do NOT open a new PR. Do NOT
   force-push without need.

5. Emit the success line per the formatting role: `PR_URL=<same URL as
   before>`. Exit 0.

# If you cannot fix at least one gap

Emit `BLOCKED: <gap you could not close and why>` per the formatting
role and exit 2.

# Same restrictions as iteration 1

- No sub-agents.
- No worktree isolation (already in worktree).
- No card touch, no Trello touch.
- No force-push without need.
- No `main` or other branches.
- No new PR.
