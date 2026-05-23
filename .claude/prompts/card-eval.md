# Card evaluation procedure

Sub-prompt called by `/trello-check` for each card it triages. Given one
card, produce one of three outcomes: **PLAN**, **QUESTIONS**, or **SPLIT
proposal**. The orchestrator (trello-check.md) handles column moves and
session-log writes; this file is the decision logic.

## Input

When this procedure starts:
- The card has just been moved to `Triage in progress` (lock against double
  processing).
- You have the board snapshot from the orchestrator's bootstrap (all cards
  in all columns) and recent `.gilb/session-log.md` entries.
- You have the card itself (id, name, desc, comments, attachments, labels).

## Steps

### A. Read the card

- `name` and `desc` fields.
- All `comments` — anything without `[meta]`/`[worker]` prefix is human input.
- All `attachments` — if they link to repo files or external resources,
  open or fetch them.
- `labels`, `due`, `members` — side context.

### B. Gather repo context

Before deciding, verify the relevant slice of the repo:

- If the card mentions filenames / modules / functions — open them with
  Read; confirm they exist now (not just in plans or your memory).
- If the task is architectural — check `spec.md` and `tauri-plan.md` for
  existing decisions on the topic.
- If the task targets a specific crate (gilb-a11y, gilb-db, gilb-engine,
  …) — glance at its `Cargo.toml` and `src/` layout.
- If the task references a phase from `tauri-plan.md` (Phase 0, Phase 3,
  …) — find that phase and understand its pre-conditions.
- If the task touches the Tauri frontend — paths under
  `apps/gilb-app-tauri/src/`, `index.html`, any `.css` / `.ts` / `.tsx`
  / `.jsx` file — read `.claude/prompts/ui-design.md` BEFORE writing
  the PLAN, and cite it explicitly in the PLAN's `## Approach`.

Stop when you've verified:
- Files you'll reference in the plan exist (or are explicitly new).
- Features you'll treat as "done" really are done.
- Decisions you'll treat as "made" are documented in spec.md / tauri-plan.md
  / commit history.

Do not go infinitely deep. Enough context to make a confident plan, not
exhaustive.

### C. Cross-card scan

Using the board snapshot from the orchestrator's bootstrap + recent
session-log entries:

- Is there a card in `Plan Proposed` / `Ready for AI` / `In Progress` /
  `Review` that overlaps with this one (same files, same feature, same
  module)? If yes → flag as dependency or duplicate (see Edge cases below).
- Was a related card recently in `Done` (per session-log)? Its outcome
  might inform the plan (e.g., a constraint surfaced during execution).
- Same author / label cluster active right now?

One mental scan, not a deep dive. Only fetch full content of another card
if a strong overlap suspicion arises.

### D. Gap analysis

Walk each category. Each is a potential QUESTION if no sensible default
exists.

| # | Category | What to check | Default OK when | Ask when |
|---|---|---|---|---|
| 1 | **What** | What behavior changes/is added? | title + desc describe one concrete observable change. | "Improve X", "Refactor Y" without a "better means what" criterion. |
| 2 | **Where** | Which files/crates/layers? | obvious from context. | Multiple candidate sites; selection needed. |
| 3 | **Why** | What user/system impact? | obvious from desc or general project context. | Unclear — risk of solving the wrong problem. |
| 4 | **Verify** | How will we know it's done? | a concrete `cargo test` / observable check can be written. | only "test manually" with no scenario described. |
| 5 | **Scope boundary** | What is NOT in scope? | task is atomic; nothing to cut. | wording is broad ("Add Windows support") and hides sub-tasks. |
| 6 | **Dependencies** | Depends on other in-flight work? | no cross-card deps (per step C), or deps already merged. | depends on something open — order it first. |
| 7 | **Edge cases** | Obvious branches (empty, error, permission denied) addressed? | simple logic with no significant branching. | complex flow without error-path notes. |
| 8 | **Size** | Fits in one PR? | 3-7 files, localized feature. | broad scope, "half the system" (see SPLIT signs below). |

**Rule of thumb:** ask only when the answer actually changes the plan. If a
default is reasonable, describe it in the PLAN (e.g., "Buffer size: 4096,
matches ACTION_CHANNEL_CAPACITY") and don't ask.

### E. Detect SPLIT

If the task is too big for one PR, you produce a split proposal instead of
a plan. Signs of "too big":
- Plan would touch >5 distinct top-level directories.
- Naturally divides into independently testable stages.
- desc already contains a numbered list of 3+ significant steps.
- Touches multiple phases from `tauri-plan.md`.
- Your would-be Confidence in a single PLAN would be < 7 purely because of
  scope (not unknowns).

### F. Decide outcome

First check SPLIT (F1). Otherwise walk the gaps from step D:

- **No gaps** → take F3 PLAN.
- **Some gaps, but every one is auto-answerable** per the policy below → take
  F3 PLAN, prefacing it with one `[meta] ASSUMED` comment per gap so the
  audit trail records each guess.
- **Any gap is NOT auto-answerable** → take F2 QUESTIONS for all gaps (don't
  mix; partial auto-answers fragment the audit and the human has to read both
  branches).

#### Auto-answer policy

Apply ALL four conditions per gap. If any fails, the gap is not
auto-answerable and the whole card goes F2 QUESTIONS.

1. **Category is on the safe list.** Auto-answerable: `Where`, `Verify`,
   `Edge`, `Scope` (boundary, i.e. what's NOT in scope; not "Scope is too
   broad — split it"). Not auto-answerable: `What` (epic-shaping product
   call), `Why` (motivation/incident), `Dependencies` (cross-card
   coordination), `Size` when it implies SPLIT.
2. **Personal confidence on the answer is ≥ 8/10** AND the resulting PLAN's
   overall Confidence stays ≥ 7. If guessing forces overall confidence to 7
   only by stretching, drop to QUESTIONS — the threshold is "I'd defend this
   in code review", not "it's probably fine".
3. **The guess is reversible in one follow-up PR.** Reversible: a file-path
   choice, a test-command shape, a default value, a state-machine variant
   name. NOT reversible without serious churn: a schema/migration shape, a
   public API contract (trait signature, wire format), a dependency or
   crate choice that pulls in transitive deps, a UI palette / typography
   pick. Use the rule "if I'm wrong, can a follow-up PR fix it without a
   migration?" — if no, that's an F2 question.
4. **Cap: ≤2 auto-answers per card.** Three or more guesses on one card is
   substantial accumulated uncertainty; route to F2 QUESTIONS instead so
   the human weighs in once before all the choices compound.

#### Outcomes

**F1. SPLIT** — too big for one card.

Comment in card (orchestrator moves to `Human Questions`):

```
[meta] TOO BIG — proposed split

This task is larger than one PR. I suggest splitting it into:

1. **<sub-task 1 title>** — <one-line scope>
2. **<sub-task 2 title>** — <one-line scope>
3. **<sub-task 3 title>** — <one-line scope>

To confirm: comment the exact phrase `split confirmed` (case-insensitive)
on this card. On the next /trello-check I will create the sub-cards in
Backlog (labeled `ai-generated`) and archive this one.

To reject: refine the scope in a comment so it fits one PR, then move
back to Backlog.
```

Do not create or archive cards yourself in this step. The split-execution
phase of `/trello-check` handles that when it sees the confirmation
phrase.

**F2. QUESTIONS** — at least one gap is not auto-answerable.

Comment in card (orchestrator moves to `Human Questions`):

```
[meta] QUESTIONS

1. **<category>** — <concrete question with proof from file/code/spec>
   Context: <1-2 lines on why this matters>

2. **<category>** — <…>

I cannot produce a plan without these answers — skipping this card.
```

Categories from step D table: `What`, `Where`, `Why`, `Verify`, `Scope`,
`Dependencies`, `Edge`, `Size`. Tag each question so the human grasps the
gap type quickly. Include every gap from this card here — don't mix with
auto-answers (see the rule at the top of step F).

**F3. PLAN** — everything clear, OR every gap was auto-answerable.

Produce a `[meta] PLAN` comment following the canonical format and
self-check in `plan-format.md`. The orchestrator moves the card to `Plan
Proposed`.

If any gap was auto-answered, post one `[meta] ASSUMED` comment per gap
BEFORE the PLAN comment (so they appear above PLAN in the card's
chronological view):

```
[meta] ASSUMED (gap: <category>)

Answer: <one-line specific choice>
Reasoning: <one-to-two lines on why this default is safe — cite file paths or
research docs when relevant>
Override: comment with the alternative and move the card back to Backlog;
the next /trello-check will re-evaluate.
```

If self-check fails (especially overall Confidence < 7) → downgrade the
whole card to F2 QUESTIONS instead, including any gaps you'd planned to
auto-answer.

## Edge cases

| Situation | Outcome |
|---|---|
| Card has only a title, no desc | F2 QUESTIONS: "Expand the task. Title is not enough." |
| Card references a non-existent file | F2 QUESTIONS: "File `X` does not exist on `main`. Did you mean another?" |
| Looks like a duplicate of an existing card (from step C) | F2 QUESTIONS: "Possible duplicate of `<url>`. Close this one or merge?" |
| Card overlaps a card in In Progress / Review | F2 QUESTIONS: "This overlaps `<url>` currently in <column>. Wait for that to merge, or coordinate?" |
| Refactoring with no user-visible change | F3 PLAN is fine, but Scope must explicitly say "no functional change". Tests: include commands that confirm existing behavior is preserved. |
| Card is a "research/spike" (find out something) | F2 QUESTIONS: "This is research, not code. Move to `research/` manually or reformulate as 'on the basis of X — implement Y'." |
| MCP `trello` does not respond | Stop, error to chat. Don't use a curl fallback. |
| `.gilb/session-log.md` missing or unreadable | Create it (touch + header from existing template); proceed. Log the recovery action in chat. |

## Output

This procedure does not move the card or post comments by itself — the
orchestrator (`/trello-check`) does. It just produces:

- The outcome type: `PLAN`, `QUESTIONS`, or `SPLIT`.
- The comment body (formatted per F1/F2/F3 above).
- For PLAN: the parsed Metrics (confidence, value, risk, expected
  iterations, size) for the session-log summary line.
