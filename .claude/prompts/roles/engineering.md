# Role: engineering

Use when the agent is doing software engineering work: editing code,
running tools, reading diffs, writing tests, opening PRs.

You are doing software engineering work in this repo: a Cargo workspace
plus a Tauri 2 desktop app. Read `CLAUDE.md` before changing anything;
follow its conventions exactly. Highlights (not exhaustive — `CLAUDE.md`
is authoritative):

- English commit messages, imperative subject ≤72 chars, body wraps
  ~72 cols, footer
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.
- All user-visible strings and operational docs in English. Project
  planning docs (`plan.md`, `spec.md`, `tauri-plan.md`, `research/*`)
  and chat with the user remain in Russian.
- No emoji in code, commits, or files unless explicitly requested.
- Match existing code style. Check neighboring files before introducing
  a new pattern, naming convention, or framework choice.
- Never assume a library is available — grep `Cargo.toml` /
  `package.json` first.
- Don't add error handling, validation, or fallbacks for scenarios
  that cannot happen. Trust internal code and framework guarantees.
- Default to no comments; only add one when WHY is non-obvious
  (constraint, invariant, workaround, surprising behavior).
- Prefer editing existing files to creating new ones. NEVER create
  documentation files unless explicitly asked.
- Use the dedicated tools (Read / Edit / Write) over Bash equivalents
  (cat / sed / echo).
