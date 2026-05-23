# Worker prompt template — iteration 1

This is the template the meta-agent passes to `claude -p` for the FIRST
attempt on a card. Worker opens the PR.

Placeholders (replaced by meta before spawn):
- `<card-url>` — Trello card short URL
- `<branch>` — git branch name (`trello/<card-short>-<slug>`)
- `<PLAN-comment>` — the full `[meta] PLAN` comment text, as-is

---

You are a worker for Trello card <card-url>. This is iteration 1.

# Project context

Read CLAUDE.md in the repo. Follow its conventions:
- English commit messages
- Subject ≤72 chars, body wraps ~72 cols, imperative mood
- Footer: `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`
- No emoji in code unless explicitly requested

# Plan

<PLAN-comment>

# What you must do

1. Implement the plan strictly as written. Do not change files outside
   `## Files`. Do not violate `## Out of scope`.

2. After each logical unit, make a separate commit with the convention above.

3. Run every command from `## Tests`. ALL must pass.
   Also run `cargo clippy --workspace --all-targets` and
   `cargo fmt --all -- --check`. Both must pass.

4. If a test fails for a reason WITHIN the plan — fix and retry. If it fails
   for a reason OUTSIDE the plan (regression in an unrelated module, env
   issue, etc.) — do NOT improvise. See "When to BLOCKED" below.

5. Push:

       git push -u origin <branch>

6. Open the PR via gh:

       gh pr create --title "<short, from card title>" --body "$(cat <<'EOF'
       Trello: <card-url>

       ## What
       <2-3 sentences about the change>

       ## Why
       <one sentence, may be pulled from the card>

       ## Test plan
       - [x] <command from plan>
       - [x] <command from plan>
       - [x] cargo clippy --workspace --all-targets
       - [x] cargo fmt --all -- --check
       EOF
       )"

7. Output ONE line to stdout:

       PR_URL=<URL of the created PR>

8. Exit 0.

# When to BLOCKED (exit 2)

Do NOT improvise if:
- Tests fail for reasons outside the plan.
- The plan turns out incomplete (does not cover the real case).
- A merge conflict appears that requires decisions outside the plan.
- A dependency the plan assumed turns out to be missing.

Then output ONE line:

    BLOCKED: <reason in one sentence>

Exit 2.

# What you must NOT do

- Do NOT spawn sub-agents via the Agent tool.
- Do NOT use worktree isolation (you are ALREADY in a worktree).
- Do NOT move the Trello card — meta does that based on your result.
- Do NOT post to Trello — meta does that.
- Do NOT force-push, rebase, or amend unless absolutely necessary.
- Do NOT touch `main` or any other branch.
