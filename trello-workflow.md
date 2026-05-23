# Trello workflow for Claude Code

How we drive work on gilb through Trello cards and Claude Code (CC) running on
a persistent remote machine. Trello acts as a task queue; CC pulls work,
refines plans with the human only when needed, executes, and auto-merges when
confidence is high.

The doc is reproducible: with this file you can stand up the same workflow in
another repo / on another machine.

## Decisions baked in (v1)

| Aspect | Decision |
|---|---|
| Where CC runs | Persistent remote machine, always on |
| Trigger | Manual: user invokes `/check-trello` or `/run-trello` in CC |
| Worker model | One `claude -p` headless process per card, in its own git worktree |
| Plan approval | User drags card from `Plan Proposed` → `Ready for AI` |
| Card → PR ratio | Strictly **1 card = 1 PR**. Bigger scope must be split at the planning stage |
| Worker blockers | Dedicated `Blocked` column with explanatory comment |
| Auto-acceptance | Meta-agent auto-merges PR if confidence ≥ 7, CI green, risk ≤ medium (any iteration count, as long as acceptance check passes). Otherwise → `Review` for human |
| Iteration limit | 3 worker iterations per card before forced `Blocked` |
| Multi-project | One CC session per project, config in repo (`.claude/trello.json`) |
| Trello API | MCP server, secrets in `~/.claude.json` (user-global), `BOARD_ID` in repo |
| Knowledge store | Filesystem: `.gilb/session-log.md` + Trello board itself as authoritative state |

## Card identifiers

Every card on the board has a `[GILB-N]` prefix in its title (e.g.
`[GILB-23] Add health-check endpoint`). The `N` is Trello's `idShort` —
a board-scoped auto-incrementing integer that Trello assigns on card
creation.

- User-created cards: prefix yourself when creating, or let next
  `/check-trello` rename (it normalizes any prefix-less card it sees in
  Backlog).
- AI-created cards (split execution only in v1): meta renames immediately
  after Trello returns the new `idShort`.
- The prefix is for human readability; API calls still use `id` or
  `shortLink`.
- `card_prefix` is configurable in `.claude/trello.json`.

## Board layout: 9 columns

In left-to-right order, names case-sensitive:

1. **Backlog** — user drops raw card ideas here
2. **Triage in progress** — meta-agent is currently reading the card (lock)
3. **Human Questions** — meta found gaps that require a human decision
4. **Plan Proposed** — meta wrote a PLAN, waiting for manual approval
5. **Ready for AI** — plan approved, eligible for worker execution
6. **In Progress** — `claude -p` worker is running (or iterating)
7. **Review** — PR created, meta did NOT auto-merge (low confidence, big PR, CI failed, etc.) — human attention needed
8. **Blocked** — worker hit unrecoverable issue OR hit iteration limit (≥3)
9. **Done** — meta auto-merged PR successfully

## Card lifecycle

```
┌─ Backlog ─────────────────────────────────┐
│ User creates card: title + description    │
└───────────┬───────────────────────────────┘
            │ /check-trello
            ▼
┌─ Triage in progress ──────────────────────┐
│ Meta-agent:                               │
│  1. reads description + context           │
│  2. cross-references spec.md / planning   │
│  3. checks recent .gilb/session-log.md    │
│  4. queries full board for related cards  │
│  5. runs gap analysis                     │
└─────┬─────────────────┬───────────────────┘
      │ gaps found      │ all clear
      ▼                 ▼
┌─ Human Questions ──┐  ┌─ Plan Proposed ────────────┐
│ Comment with       │  │ Comment with PLAN:         │
│ - QUESTIONS or     │  │ Scope / Files / Approach   │
│ - TOO BIG split    │  │ Tests / Out of scope       │
│   proposal         │  │ Metrics (conf / value /    │
│                    │  │  risk / iters / size)      │
│ Human answers      │  │                            │
│ and drags back     │  │ Human drags →              │
│ to Backlog OR      │  │                            │
│ confirms split     │  └────────┬───────────────────┘
│ (see Split flow    │           │
│ section)           │           ▼
└────────────────────┘  ┌─ Ready for AI ─────────────┐
                        │ Sits until /run-trello     │
                        └────────┬───────────────────┘
                                 │
                                 ▼
                        ┌─ In Progress ──────────────┐
                        │ Worker iteration loop:     │
                        │  iter 1: spawn → check     │
                        │  iter 2: spawn → check     │
                        │  iter 3: spawn → check     │
                        │ Each iter posts result as  │
                        │ [meta] comment in card.    │
                        └────────┬───────────────────┘
                                 │
       ┌─────────────────────────┼────────────────────────────┐
       │ acceptance pass +       │ acceptance pass but        │ all 3 iters
       │ auto-merge criteria     │ criteria NOT met OR        │ failed acceptance
       │ met (conf≥7,            │ CI red                     │ OR worker
       │ CI green, risk≤med)     │                            │ BLOCKED itself
       ▼                         ▼                            ▼
┌─ Done ───────────┐    ┌─ Review ───────────────┐  ┌─ Blocked ──────────┐
│ Meta auto-merged │    │ PR ready for human     │  │ Comment with full  │
│ PR via           │    │ review. Comment        │  │ iter history +     │
│ `gh pr merge     │    │ explains WHY auto-     │  │ remaining gaps.    │
│  --merge`.       │    │ merge was skipped.     │  │ Human moves back   │
│ Card commented   │    │ Human merges or        │  │ to Ready for AI    │
│ with PR # and    │    │ requests changes.      │  │ or Backlog after   │
│ iter summary.    │    │ Card moves manually.   │  │ fixing.            │
└──────────────────┘    └────────────────────────┘  └────────────────────┘
```

### Split flow (AI card creation, v1)

The only path where the meta-agent creates Trello cards. Triggered by you,
not initiated by the AI:

```
Card in Backlog has scope too big
        │
        ▼ /check-trello (triage)
Meta posts [meta] TOO BIG with proposed 2-5 sub-tasks,
moves card to Human Questions
        │
        ▼ you read the proposal
        │
   ┌────┴────┐
   │         │
   ▼         ▼
You comment  You refine scope
"split       in a comment
confirmed"   and drag back
   │         to Backlog
   ▼
On next /check-trello, meta creates sub-cards
in Backlog (label: ai-generated), posts
[meta] SPLIT EXECUTED with links, archives the
original card.
```

Only SPLIT cards bear the `ai-generated` label in v1. Worker spinoff,
post-merge follow-up, and pattern-observation card creation are explicit
non-goals for now (see "Explicit non-goals").

## Slash commands

Three commands, all manually invoked by user in their CC session on the remote.

### `/check-trello` — triage + split execution

Meta-agent:

1. Reads board state and recent session-log entries (cross-card context).
2. **Phase 1: Split execution.** Scans `Human Questions` for cards where:
   - There's a `[meta] TOO BIG — proposed split` comment, AND
   - There's a SUBSEQUENT human comment containing `split confirmed`
     (case-insensitive).

   For each match, creates the proposed sub-cards in `Backlog` (with label
   `ai-generated`), posts `[meta] SPLIT EXECUTED` on the original, and
   archives the original. Idempotent — skips cards already marked
   `SPLIT EXECUTED`.

3. **Phase 2: Backlog triage.** Iterates over all cards in `Backlog`:
   - Moves card to `Triage in progress` (lock).
   - Calls `.claude/prompts/card-eval.md` for per-card decision.
   - Posts the comment from card-eval and moves card to the target column:
     - `PLAN` → `Plan Proposed`
     - `QUESTIONS` → `Human Questions`
     - `SPLIT` → `Human Questions` (split proposal, not yet executed)
4. Appends a line per card to `.gilb/session-log.md`.
5. Does NOT spawn workers. Does NOT write code.

### `/trello-normalize` — title normalization (utility)

Standalone utility. Scans every card on the board (all columns, including
archived) and adds the `[<card_prefix>-<idShort>]` prefix to any card
without it. Does NOT triage. Does NOT comment. Does NOT move cards.

Use after a bulk Trello-UI import, when a card slipped past `/check-trello`
in a non-Backlog column, or on first onboarding of an existing board.

`/check-trello` already normalizes Backlog cards as Phase 2 step 1; this
command is the superset for the rest of the board.

### `/run-trello` — execution + auto-acceptance

Meta-agent:
1. Reads board state, recent session-log entries.
2. Iterates over all cards in `Ready for AI`, one at a time:
   - Moves card to `In Progress`, creates git worktree.
   - Runs **iteration loop** (max 3):
     - Spawns `claude -p` worker with plan (iter 1) or plan + previous gaps (iter 2+).
     - Worker writes code, commits, pushes, opens PR (iter 1) or pushes to existing PR (iter 2+).
     - Meta runs **acceptance check** against the plan.
     - If acceptance passes → break loop.
     - If gaps remain and iter < 3 → comment gaps, increment iter, loop.
     - If gaps remain and iter == 3 → move to `Blocked`.
   - After acceptance: runs **auto-merge decision**.
     - All criteria met (confidence ≥ 7, CI green, risk ≤ medium): `gh pr merge --merge`, move to `Done`.
     - Any criterion missed: move to `Review` with comment explaining which criterion blocked auto-merge.
3. Appends per-card lines to `.gilb/session-log.md`.

## PLAN format (canonical)

Written by meta-agent as the first `[meta] PLAN` comment when triage produces
a plan. Worker reads this as its contract.

```
[meta] PLAN

## Scope
<what is included — concrete and measurable, 1-3 sentences>

## Files
- `path/to/file.rs` — what changes (one line)
- `path/to/other.rs` (new) — what is created
- `path/to/third.rs` — what changes

## Approach
<3-5 sentences: key decisions, ordering, non-obvious nuances.
Explain HOW, not a restatement of Scope.>

## Tests
- `cargo test -p <crate> <filter>` — what it covers
- `cargo clippy --workspace --all-targets` — mandatory
- `cargo fmt --all -- --check` — mandatory
- <manual step, if needed>

## Out of scope
<what is NOT done in this card — explicit boundary>

## Metrics
- Confidence: <0-10> — how sure the plan reaches merge without rework
  Why: <one line>
- Value: <low|medium|high> — impact on project
  Why: <one line>
- Risk: <low|medium|high> — chance things go off-plan
  Why: <one line>
- Expected iterations: <1|2|3> — meta's estimate of worker passes needed
- Estimated size: <S|M|L>. S = <300 LOC, M = 300-800, L = 800-1500.
  If L → reconsider SPLIT.
```

If meta computes Confidence < 7 during self-check → downgrade to `Human Questions`
instead of publishing PLAN. The plan isn't ready.

## Worker contract

Worker (a `claude -p` process spawned by `/run-trello`) reads the PLAN from
the card and follows it strictly. Worker is given:

- The `[meta] PLAN` comment text in full.
- The card URL (for the PR description).
- The branch name + worktree path (cwd).
- The iteration number (iter 1 vs N).
- For iter ≥ 2: the gaps reported by meta after the previous iteration.

Worker must NOT:
- Improvise outside the plan. If reality doesn't fit the plan → `BLOCKED: <reason>` to stdout, exit 2.
- Spawn sub-agents.
- Touch `main` or other branches.
- Move the Trello card (meta does that based on worker's exit code + output).

Worker output contract:
- Success: `PR_URL=<url>` on its own line in stdout, exit 0.
- Failure (controlled): `BLOCKED: <reason>` on its own line in stdout, exit 2.
- Crash: anything else → meta treats as crash, moves card to `Blocked`.

## Acceptance check (run by meta after each iteration)

Checklist; each failure adds to a `gaps[]` list:

1. **Files coverage** — every file in plan `## Files` is in `git diff origin/main...HEAD`.
2. **No scope creep** — diff has no files outside `## Files` (Cargo.lock + .gitignore allowed).
3. **Out of scope respected** — nothing forbidden in `## Out of scope` is touched.
4. **Tests pass** — every command from `## Tests` exits 0.
5. **Clippy clean** — `cargo clippy --workspace --all-targets` has no warnings.
6. **Formatting clean** — `cargo fmt --all -- --check` passes.
7. **PR metadata correct** — title is meaningful, body starts with `Trello: <card-url>`, has `## What` / `## Why` / `## Test plan`.
8. **Commits hygiene** — each subject ≤72 chars, English, imperative; bodies wrap ~72; `Co-Authored-By: Claude` footer present.

If `gaps == []` → acceptance passes, proceed to auto-merge decision.

## Auto-merge decision (run by meta after acceptance passes)

Auto-merge if ALL of:
- `metrics.confidence >= 7` (from PLAN)
- `metrics.risk in {low, medium}` (from PLAN)
- `gh pr checks <pr_url>` reports all green

Iteration count is recorded in the audit comment for observability but does NOT
gate auto-merge — acceptance check has already verified the final state
objectively. A high iter count is feedback on planning quality (confidence
should drop next time), not on code correctness.

If all met:
```bash
gh pr merge <pr_url> --merge --delete-branch
```
Move card to `Done` with comment summarizing iterations + PR link.

Else: move card to `Review`. Comment lists WHICH criteria blocked auto-merge,
so the human reviewer knows what to focus on.

## Comment conventions in cards

Prefix everything from automation:
- `[meta] ` — meta-agent: plans, questions, audit, decisions.
- `[worker] ` — worker process: starts, PR links, blockers. (Meta posts these on
  behalf of worker, derived from worker's stdout.)

Anything without a prefix is human input.

Required meta comments per card:
- **At triage**: `[meta] PLAN` OR `[meta] QUESTIONS` OR `[meta] TOO BIG`
- **At each iteration in /run-trello**: `[meta] Iteration <N>: ...` (either accepted or with gaps)
- **At completion**: `[meta] AUTO-MERGED` OR `[meta] READY FOR REVIEW` OR `[meta] BLOCKED`

Worker proxy comments:
- **At start**: `[worker] Starting iter <N>. Branch: <branch>.`
- **At PR open** (iter 1 only): `[worker] PR opened: <url>`
- **At block**: `[worker] BLOCKED: <reason>. Log: <path>.`

## Memory layout (v1)

Where each kind of state lives.

| Layer | Where | What | Lifetime | Authoritative? |
|---|---|---|---|---|
| Chat context | CC session in-memory | User messages, meta thinking, tool results | Until session ends | No |
| Worker runtime log | `.gilb/worker-logs/<card>-iter<N>.log` | Full worker stdout/stderr | Until manual cleanup | No |
| Worker workspace | `<worktree_root>/<card>-<slug>/` | Code + git history of the branch | Until manual `git worktree remove` | No |
| Cross-card history | `.gilb/session-log.md` | Append-only event log per card | Forever (gitignored, per-machine) | No |
| Cross-session state | Trello board via MCP | Cards, columns, comments | Forever (Trello server) | **Yes** |
| Project static | `CLAUDE.md`, `spec.md`, `tauri-plan.md`, `research/`, `.claude/trello.json` | Architecture, conventions, board config | In git | **Yes** |
| Secrets | `~/.claude.json` (chmod 600) | TRELLO_API_KEY/TOKEN under `mcpServers.trello.env` | User-global, not in git | **Yes** |

### Knowledge sources used by meta and worker

**Project static (slow-moving)** — auto-loaded in any CC session in the repo:
- `CLAUDE.md` — conventions, build commands, architecture.
- `spec.md` — target architecture, deviations across plans.
- `tauri-plan.md` — phased roadmap.
- `research/*.md` — reference-project breakdowns.

**Workflow runtime (fast-moving)** — pulled at the start of each `/check-trello`
or `/run-trello` invocation:
- **Trello board itself** — meta queries the full board (all columns) for
  cross-card awareness ("there's a related card in Plan Proposed", "this
  duplicates a recently-Done card").
- **`.gilb/session-log.md`** — meta reads the last N entries (default 30) to
  recall recent activity patterns. Format:
  ```
  <ISO timestamp UTC>  <card-short-id>  <EVENT>  | <one-line summary>
  ```
  Event types listed in `.gilb/session-log.md` header.

### Key principle

A Trello card with `[meta] PLAN` + iteration history (`[meta] Iteration N: ...`)
+ final outcome (`[meta] AUTO-MERGED` / `READY FOR REVIEW` / `BLOCKED`) is the
**complete knowledge artifact** for that task. Meta and worker can recover full
context just by reading the card via Trello API. The session-log is a fast
cross-card index, not the source of truth.

### Worker memory access

Worker gets a minimal slice via its prompt:
- The PLAN (passed in full).
- The cwd worktree (it operates there).
- Prior iteration's gaps (only on iter ≥ 2).
- Project static layer (CLAUDE.md auto-loaded).

Worker does NOT read session-log or query Trello directly. It only outputs to
stdout (PR_URL or BLOCKED), and meta interprets.

## Optional v2: OpenViking knowledge store

[OpenViking](https://openviking.ai) is ByteDance's "context filesystem for
agents": hierarchical knowledge stored under `viking://` URIs with semantic
search and L0/L1 summaries. Designed for exactly this use case.

**Not in v1** because:
- We have 1-10 cards in flight at once — semantic search is overkill.
- Card + session-log already cover cross-card recall.
- Setup requires Ollama (or external API), embedding + VLM model, separate
  background service, Claude Code plugin install, shell wrapper.

**When to consider v2:**
- Volume grows past ~50 active cards and finding relevant past cards becomes
  manual work.
- Cross-project knowledge sharing (multiple repos with their own boards) and
  you want a unified knowledge graph.
- You want semantic recall of architectural decisions across months of work.

**v2 setup outline** (when you decide to flip):

1. **Configure external models** (NOT Ollama) — pick one of:
   - **Volcengine** (Doubao) — native to OpenViking (it's from ByteDance).
     Env: `OPENVIKING_EMBEDDING_API_KEY`, `OPENVIKING_VLM_API_KEY`,
     `OPENVIKING_ARK_API_KEY`. Provider in `ov.conf`: `"volcengine"`.
   - **OpenAI** — standard. Provider: `"openai"`, model:
     `text-embedding-3-small` for embeddings, `gpt-4o-mini` for VLM.
   - **Azure OpenAI** — provider: `"azure"`, requires `api_base`.
   - **VikingDB / Voyage / Jina** — also supported per
     `openviking_cli/utils/config/embedding_config.py`.

2. **`~/.openviking/ov.conf`** (JSON):
   ```json
   {
     "server": { "host": "127.0.0.1", "port": 1933 },
     "storage": { "workspace": "/root/.openviking/data" },
     "embedding": {
       "dense": {
         "provider": "openai",
         "model": "text-embedding-3-small",
         "api_key": "<from env or here>",
         "dimension": 1536
       }
     },
     "vlm": {
       "provider": "openai",
       "model": "gpt-4o-mini",
       "api_key": "<same>"
     }
   }
   ```

3. **Start the server**:
   ```bash
   nohup openviking-server > /root/.openviking/server.log 2>&1 &
   curl http://127.0.0.1:1933/health   # should respond
   ```

4. **Install the Claude Code plugin**:
   ```bash
   git clone https://github.com/volcengine/OpenViking /opt/openviking
   claude plugin marketplace add /opt/openviking/examples
   claude plugin install claude-code-memory-plugin@openviking-plugins-local
   ```

5. **Shell wrapper** in `~/.bashrc` (per OpenViking README — injects env
   vars for the plugin):
   ```bash
   claude() {
     local _ov_conf="${OPENVIKING_CLI_CONFIG_FILE:-$HOME/.openviking/ovcli.conf}"
     if [ -f "$_ov_conf" ] && command -v jq >/dev/null 2>&1; then
       local _ov_url _ov_key
       _ov_url=$(jq -r '.url // empty' "$_ov_conf" 2>/dev/null)
       _ov_key=$(jq -r '.api_key // empty' "$_ov_conf" 2>/dev/null)
       OPENVIKING_URL="${OPENVIKING_URL:-$_ov_url}" \
       OPENVIKING_API_KEY="${OPENVIKING_API_KEY:-$_ov_key}" \
         command claude "$@"
     else
       command claude "$@"
     fi
   }
   ```

6. **Workflow changes for v2** (will need to update
   `.claude/prompts/card-eval.md` and the orchestrators):
   - Replace "read last 30 lines of session-log" with "query
     `viking://memories/cards/recent/` via the OpenViking MCP tool".
   - After each card outcome, write to
     `viking://memories/cards/<card-id>/<event>.md` instead of (or in
     addition to) `.gilb/session-log.md`.
   - For architectural decisions, write to
     `viking://memories/decisions/<topic>.md`. Worker can be granted read
     access to recall constraints.
   - Worker prompts gain an optional `<context>` block with relevant
     OpenViking recall results.

The package is already installed (`openviking-cli`, `openviking-server`
binaries available) — only config + server start + plugin are missing.

## File layout in this repo

```
.claude/                         # committed
├── trello.json                  # board id, list ids, branch_prefix, auto-merge criteria, labels
├── commands/                    # slash command orchestrators
│   ├── check-trello.md          # /check-trello: triage + split execution
│   └── run-trello.md            # /run-trello: iteration loop + auto-merge
└── prompts/                     # reusable sub-prompts (called by orchestrators)
    ├── card-eval.md             # per-card triage decision procedure
    ├── plan-format.md           # PLAN canonical structure + self-check
    ├── acceptance-check.md      # 8-item deliverable verification
    ├── worker-iter1.md          # worker prompt template, iteration 1
    └── worker-iterN.md          # worker prompt template, iterations 2-3
.gilb/                           # gitignored runtime state (everything per-card)
├── session-log.md               # append-only event log (cross-card)
├── worker-logs/                 # worker stdout/stderr per iteration
│   └── <card-short>-iter<N>.log
└── worktrees/                   # git worktrees, one per active card
    └── <card-short>-<slug>/
trello-workflow.md               # this file (operational doc, English)
CLAUDE.md                        # convention pointer to this file
```

All per-card runtime state lives under `.gilb/` in the project folder
(gitignored). Nothing in `/tmp/`, nothing outside the repo.

## Setup (one-time, for a new remote or new project)

### 1. Trello credentials

**API key**: https://trello.com/power-ups/admin → New Power-Up → "API Key" tab →
Generate. Copy the 32-char hex.

**Token**: open this URL in browser (substitute API key):
```
https://trello.com/1/authorize?expiration=never&scope=read,write&response_type=token&name=claude-code-<project>&key=<API_KEY>
```
Click Allow → copy the 64-char token.

**Board ID**: open the board → URL `https://trello.com/b/<short-id>/<name>` →
`<short-id>` works for most calls. Full 24-hex ID: append `.json` to the URL.

### 2. Create the board

9 columns, exact case-sensitive names:
```
Backlog
Triage in progress
Human Questions
Plan Proposed
Ready for AI
In Progress
Review
Blocked
Done
```

### 3. Wire Trello MCP in `~/.claude.json`

User-global, NOT in repo:
```json
{
  "mcpServers": {
    "trello": {
      "command": "npx",
      "args": ["-y", "@delorenj/mcp-server-trello"],
      "env": {
        "TRELLO_API_KEY": "<your key>",
        "TRELLO_TOKEN": "<your token>",
        "TRELLO_BOARD_ID": "<short id>"
      }
    }
  }
}
```
Then `chmod 600 ~/.claude.json`.

### 4. Commit `.claude/trello.json` in repo

Public board metadata (id, list names, conventions). Example: see this repo's
`.claude/trello.json`.

### 5. Create `.gilb/session-log.md`

Empty file with header (see existing file as template). Gitignored.

### 6. Smoke-test

In CC inside the project repo:
```
/check-trello
```
Should report "Backlog empty" or process whatever's in Backlog. Then create a
trivial test card in Backlog, re-run, watch it land in Plan Proposed.

## Secret rotation

- Token and API key live ONLY in `~/.claude.json` (user-global, chmod 600).
- If leaked (chat with LLM, logs, screenshot) → immediately go to
  https://trello.com/your/account → "API Keys" → Revoke. Regenerate, update
  `~/.claude.json`.
- API key alone is harmless without a token, but if there's strong suspicion,
  rotate it too via the same Power-Up admin page.
- When moving to a new remote machine: transfer `~/.claude.json` over secure
  channel OR regenerate token (revoke old one first).

## Explicit non-goals (v1)

- No Trello webhook → remote (needs public endpoint, overkill for manual
  trigger).
- No cloud routine via `/schedule` (remote is always on; would add cost
  without benefit).
- No card split into multiple PRs. Big scope = split the CARD at planning,
  not the PR at execution.
- No committing of `~/.claude.json` or any file containing `TRELLO_TOKEN`.
- No worker improvisation outside the plan. Mismatch → `BLOCKED`.
- No OpenViking / Ollama / RAG infra. Filesystem + Trello cover v1.
- No automatic worktree cleanup. Done manually or by a future script.

## Open questions (to revisit after first cards run)

- When should worktrees be removed? Candidate: when card hits `Done` →
  `git worktree remove`. Adds a destructive action; needs care.
- Parallel worker execution: currently strictly sequential per `/run-trello`.
  Add a limit (max 2?) once we see real throughput needs.
- Decisions store (`.gilb/decisions/` or `decisions/` at root) — wire when we
  have a first real cross-card decision to record. Not in v1.
- AI card creation beyond SPLIT: worker spinoff (B), post-merge follow-up
  (C), pattern observation (D). Not in v1; revisit once SPLIT confirmation
  works and we have throughput data.
