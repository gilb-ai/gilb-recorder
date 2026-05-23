# /trello-questions

Role: **interactive question-answering meta-agent**. Invoked manually by the
user when they have time to unblock cards stuck in `Human Questions`.

Walks the `Human Questions` column. For each card with a real outstanding
question (not blocked-by, not pending-split-confirm), surfaces the single
most consequential question via the `AskUserQuestion` tool with explicit
options + reasoning, then records the user's answer as a `[meta] ANSWERED`
comment. Cards with the main fork answered move back to `Backlog` so the
next `/trello-check` re-triages with the new info.

This complements `/trello-check`'s AUTO-ANSWERED branch (`card-eval.md`
step F): /trello-check handles safe defaults autonomously; /trello-questions
handles the gaps that genuinely need the human.

## Contract (what you must NOT do)

- Do NOT use the `Agent` tool — sequential per-card iteration.
- Do NOT spawn workers (that is `/trello-run`).
- Do NOT skip cards by default. If a card has no `[meta] QUESTIONS` body,
  note it and move on (don't try to invent questions).
- Do NOT batch multiple cards into one `AskUserQuestion` call. One card,
  one question per `AskUserQuestion` turn.
- Do NOT ask questions whose answers /trello-check could now auto-answer
  per `card-eval.md` step F. Those route to "SKIP — re-run /trello-check"
  instead; the user shouldn't waste turns on them.
- Do NOT move cards into `Ready for AI` — only the user does that.
- Do NOT comment on cards without the `[meta] ` prefix.

## Sources of truth

- `.claude/trello.json` — board, list IDs (`human_questions`, `backlog`).
- `.claude/prompts/card-eval.md` — question categories + auto-answer policy.
- `trello-workflow.md` — full workflow.
- `.gilb/session-log.md` — recent automation history.

## Algorithm

### Bootstrap

1. Read `.claude/trello.json` → `lists.human_questions`, `lists.backlog`,
   `card_prefix`, `session_log`.
2. Read last 30 lines of `.gilb/session-log.md` for context.
3. Via MCP `trello`, fetch open cards in `Human Questions`. For each,
   fetch comments.
4. Categorize each card by its latest `[meta] ...` comment:
   - **`[meta] ANSWERED` / `[meta] DEFERRED`** as the latest meta entry →
     skip (already handled this session or earlier).
   - **`[meta] TOO BIG — proposed split`** with no subsequent `split
     confirmed` → skip (needs split confirmation, not Q&A; tell the user
     how to confirm if they want).
   - **`[meta] QUESTIONS` body starts with "Blocked by"** → skip (upstream
     dependency, not a user-input gap). Note in summary.
   - **`[meta] QUESTIONS`** with real questions → include for Q&A.
   - **No `[meta] QUESTIONS` comment at all** → flag as mis-categorized;
     mention in summary, do not Q&A.

### Per card

For each included card, sequentially:

1. **Parse the latest `[meta] QUESTIONS` body.** Extract the numbered
   list. Each item matches:
   ```
   N. **<category>** — <question text>
      Context: <one-line reason>
   ```
   Build `[{category, question, context}, ...]`.

2. **Auto-answer eligibility check.** If EVERY parsed question fits the
   four-condition policy in `card-eval.md` step F (safe category +
   confidence ≥ 8 + reversible + ≤2 per card), skip the user prompt:
   ```
   [meta] SKIP — re-run /trello-check
   All gaps are now eligible for /trello-check's AUTO-ANSWERED fast-path
   (safe categories, reversible defaults). Move this card to Backlog and
   run /trello-check; it will post [meta] ASSUMED + PLAN without needing
   user input here.
   ```
   Move card to Backlog. Move on to next card.

3. **Pick the single most-consequential question.** Priority order:
   1. First `What` (product fork — biggest blast radius).
   2. First `Why` (motivation; if missing, scope drifts).
   3. First `Size` ONLY when it implies a SPLIT decision (changes PR
      count, not just plan content).
   4. First `Dependencies` (cross-card coordination).
   5. First in the list.

   The other questions become *derived* — usually answerable as
   defaults once the main fork is decided (see step 6).

4. **Frame as `AskUserQuestion`.**
   - `question`: `[<card_prefix>-<idShort>] ` + the question text,
     compressed to one line if needed. Add ≤1 sentence of repo-grounded
     context (where the choice lands, what depends on it).
   - `header`: ≤12 chars (e.g. `Whisper`, `Redesign`, `Helper IPC`).
   - `options` (3–4):
     - Each `label` is a concrete choice (1–5 words).
     - Each `description` is 1–2 lines explaining what choosing it means
       for the codebase, follow-up cards, and the derived defaults.
     - ALWAYS include EITHER a `Defer` option (leave in Human Questions)
       OR a `Close/Archive` option when the card might be premature —
       not both; pick whichever fits. This gives the user an escape
       hatch that's not "Other / free text".

5. **Surface via `AskUserQuestion`.** Wait for the response.

6. **Record the answer + derived defaults.**

   **Normal answer (user picked a technical option):**
   - Post on the card:
     ```
     [meta] ANSWERED (interactive session, user)

     **Q (<category>):** <user's exact choice label>

     <Optional: 1–4 bullets enumerating derived defaults for the OTHER
     questions on the card. Use the same `Q<N> (<category>):` framing
     as `card-eval.md`. Mark each "(default; override here if wrong)".>

     Re-triage on next /trello-check will produce a PLAN with these
     answers baked in. To override any answer, comment with the
     alternative before that runs.
     ```
   - Move card to `Backlog`.

   **Defer (`Defer` option chosen):**
   - Post:
     ```
     [meta] DEFERRED (interactive session, user)
     Card kept in Human Questions for a later decision.
     ```
   - Leave card in `Human Questions`.

   **Close (`Close/Archive` option chosen):**
   - Post:
     ```
     [meta] CLOSED (interactive session, user)
     Reason: <one-line per the user's chosen option description>
     ```
   - Archive the card.

   **Free-text `Other`** (user typed a custom answer):
   - Treat as a normal answer with the verbatim text as the choice.
   - Post `[meta] ANSWERED` with the text. Move card to `Backlog`.

7. **Append to `.gilb/session-log.md`:**
   ```
   <ISO UTC ts>  <card-short>  <EVENT>  | <summary>
   ```
   Where:
   - Normal/free-text answer:
     `ANSWERED-INTERACTIVE | <category>: <terse choice>`
   - Defer: `DEFERRED-INTERACTIVE | <category>: question left open`
   - Close: `CLOSED-INTERACTIVE | <reason>`
   - Skip (auto-answerable):
     `SKIPPED-INTERACTIVE | all gaps eligible for /trello-check auto-answer`

### Summary

After all included cards are processed, print once:

```
Interactive answer session complete:
- Cards walked: <N>
  - → Backlog (answered):   <M>
  - → Closed (archived):    <K>
  - → Left in Human Q (deferred): <D>
  - → Routed to /trello-check (all gaps auto-answerable): <A>
- Skipped (blocked-by upstream, or pending split-confirmation): <S>
- Mis-categorized (in Human Questions without a QUESTIONS comment): <X>
```

Then suggest a single next action — e.g. `Run /trello-check to re-triage
the <M+A> cards now in Backlog`, or `Comment <split_confirmation_phrase>
on the <S-blocked-by-split> cards if you want them split-executed`.

## Failure modes

| Situation | Action |
|---|---|
| MCP `trello` not responding | Stop. Error to chat. Don't fall back to raw curl. |
| `.claude/trello.json` malformed | Stop. Don't guess fields. |
| `.gilb/session-log.md` missing | Create from template; proceed. Log the recovery action. |
| Card has no `[meta] QUESTIONS` body but is in `Human Questions` | Skip the Q&A, list in the "Mis-categorized" tally in the summary. |
| User cancels `AskUserQuestion` (no answer returned) | Treat as Defer. Post `[meta] DEFERRED`. Continue to next card. |
| User picks the Close/Archive option | Confirm in chat once before archiving, since archive is hard to undo from this skill. |
| Card was moved out of `Human Questions` between bootstrap and processing (race) | Skip silently; list in summary as "moved during session". |
