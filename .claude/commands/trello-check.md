---
description: Triage Backlog → Plan Proposed or Human Questions; execute confirmed splits
---

# /trello-check

Role: **triage meta-agent**. Invoked manually by the user.

This file is the orchestrator. The actual decision logic for a single card
lives in `.claude/prompts/card-eval.md`. The PLAN format lives in
`.claude/prompts/plan-format.md`.

## Contract (what you must NOT do)

- Do NOT spawn workers (that is `/trello-run`).
- Do NOT write code in the repo. No commits. No PRs.
- Do NOT move cards into `Ready for AI` — only the user does that.
- Do NOT comment on cards without the `[meta] ` prefix.
- Do NOT use the `Agent` tool — sequential triage.
- Do NOT create or archive cards EXCEPT in Phase 1 (split execution) when
  the user has explicitly confirmed a split.

## Sources of truth

- `.claude/trello.json` — board, list IDs, conventions, `labels.ai_generated`,
  `split_confirmation_phrase`.
- `.claude/prompts/card-eval.md` — per-card triage decision procedure.
- `.claude/prompts/plan-format.md` — PLAN comment canonical format.
- `trello-workflow.md` — full workflow doc.
- `.gilb/session-log.md` — recent automation history.
- `CLAUDE.md`, `spec.md`, `tauri-plan.md`, `research/*.md` — project context.

## Algorithm

### Bootstrap (once)

1. Read `.claude/trello.json` → get `board.id`, `lists.*`, `labels.ai_generated`,
   `split_confirmation_phrase`, `session_log` path.
2. Read the last 30 lines of `.gilb/session-log.md` (skip header) — recent
   activity context.
3. Via MCP `trello`, fetch open cards from **all columns** with at least
   `{id, name, shortLink, idList, labels, badges}` — board snapshot for
   cross-card awareness.

### Phase 1: Split execution

Process cards in `Human Questions` looking for confirmed splits.

For each card in `Human Questions`:
- Fetch its full comments via MCP.
- Look for the LATEST `[meta] TOO BIG — proposed split` comment (skip if
  none).
- Look for a SUBSEQUENT human comment (no `[meta]`/`[worker]` prefix)
  containing `split_confirmation_phrase` (case-insensitive, e.g.
  `split confirmed`). Skip card if no confirmation found.
- Skip if there's already a `[meta] SPLIT EXECUTED` comment (idempotency).

When confirmation found:
- Parse the numbered sub-task list from the TOO BIG comment. Each item:
  `**<sub-task title>** — <one-line scope>`.
- For each sub-task, create a new card via MCP:
  - `idList`: Backlog
  - `name`: the sub-task title (without `**` markdown)
  - `desc`: the sub-task scope + a footer line `Split from: <original-card-url>`
  - `idLabels`: `[labels.ai_generated]`
- For each newly-created card, immediately rename to add the `[<card_prefix>-<idShort>]`
  prefix (e.g. `[GILB-23]`). The `idShort` is in the create-card response.
  This keeps all cards on the board (human-created + AI-generated) on the
  same numbering scheme.
- Post a `[meta] SPLIT EXECUTED` comment on the original card with links to
  all new cards (use the `[GILB-N]` titles for readability).
- Archive the original card (`PUT /cards/<id>/closed` with `value=true`).
- Append to session-log: `<ts> <card> SPLIT-EXECUTED | created N sub-cards: <comma-list of [GILB-N] ids>`.

Cap: if a card's TOO BIG proposal has more than 5 sub-tasks, abort (post
`[meta] Refusing to split — more than 5 sub-tasks. Manual cleanup needed.`)
and skip.

### Phase 2: Backlog triage

Identify cards in `Backlog`. If none → reply "Backlog empty (split phase
processed N cards)" and exit.

For each Backlog card **sequentially**:

1. **Normalize title.** If the card's title doesn't start with
   `[<card_prefix>-<idShort>]` (e.g. `[GILB-42]`), rename it to add the
   prefix. Use the `idShort` field already in the card data.
   Cards created via Trello UI without the prefix get normalized here.
2. Move card to `Triage in progress` (lock).
3. Apply the procedure in `.claude/prompts/card-eval.md`. It returns:
   - `outcome` ∈ {`PLAN`, `QUESTIONS`, `SPLIT`}
   - `comment_body` (already formatted, including `[meta] ` prefix)
   - For `PLAN`: parsed metrics (confidence, value, risk, expected_iters, size)
3. Post `comment_body` on the card via MCP.
4. Move the card to the target column:
   - `PLAN` → `Plan Proposed`
   - `QUESTIONS` → `Human Questions`
   - `SPLIT` → `Human Questions`
5. Append to `.gilb/session-log.md`:
   ```
   <ISO UTC timestamp>  <card-short>  <EVENT>  | <summary>
   ```
   Where EVENT and summary by outcome:
   - PLAN: `TRIAGED→PLAN | conf=<N> risk=<low|med|high> size=<S|M|L>: <terse description>`
   - QUESTIONS: `TRIAGED→QUESTIONS | <gap categories, e.g. "What, Scope">`
   - SPLIT: `TRIAGED→SPLIT | <N> proposed sub-cards`

### Summary

After both phases:
```
Triage complete:
- Splits executed: <count>
- Backlog processed: <N>
  - → Plan Proposed: <M>
  - → Human Questions: <K>
  - → SPLIT proposed: <S>
```

## Failure modes

| Situation | Action |
|---|---|
| MCP `trello` not responding | Stop. Error to chat. Don't fall back to raw curl. |
| `.claude/trello.json` malformed | Stop. Don't guess fields. |
| `.gilb/session-log.md` missing | Create it (touch + header from existing template); proceed. |
| `card-eval.md` not readable | Stop. Don't inline the procedure. |
| Card creation fails (network, permission) | Stop. Card stays in its current state. Report. |
| Trying to archive original after sub-cards created but archive fails | Sub-cards exist; manual cleanup. Comment in original card noting the partial state. |
