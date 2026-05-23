# PLAN format (canonical)

Single source of truth for what a `[meta] PLAN` comment must look like in a
Trello card. Read by `/trello-check` (writes it) and `/trello-run` (parses
it). When you change this file, both commands pick up the new format on
their next run — no other updates needed.

## Format

The first line of the comment is always exactly `[meta] PLAN`. Then the
sections below appear in this order:

```
[meta] PLAN

## Scope
<what is included — concrete and measurable, 1-3 sentences>

## Files
- `path/to/file.rs` — what changes (one line per file)
- `path/to/other.rs` (new) — what is created
- `path/to/third.rs` — what changes

## Approach
<3-5 sentences: key decisions, ordering, non-obvious nuances.
Explain HOW, not a restatement of Scope.>

## Tests
- `cargo test -p <crate> <filter>` — what it covers
- `cargo clippy -p <crate> --all-targets` — mandatory, one entry per
  crate in `## Files`
- `cargo fmt -p <crate> -- --check` — mandatory, one entry per crate
  in `## Files`
- <manual step, if needed> — what and how to verify

**Scoping rule for clippy / fmt.** By default, scope these to the
crates the card actually touches: one `-p <crate>` entry per crate
listed in `## Files`. Use `--workspace` / `--all` ONLY when the card
deliberately covers the whole workspace — typically a card whose
`## Files` spans three or more distinct crates, OR a card whose
explicit purpose is workspace-wide cleanup. The default scoped form
prevents pre-existing drift in untouched crates from blocking this
card's gates (see GILB-10).

## Out of scope
<what is NOT done in this card — explicit boundary so the worker doesn't drift>

## Metrics
- Confidence: <0-10> — how sure the plan reaches merge without rework
  Why: <one line justification>
- Value: <low|medium|high> — impact on project
  Why: <one line>
- Risk: <low|medium|high> — chance things go off-plan
  Why: <one line>
- Expected iterations: <1|2|3> — your estimate of worker passes needed
- Estimated size: <S|M|L>. S = <300 LOC, M = 300-800, L = 800-1500.
  If L → reconsider SPLIT.

## Cross-card notes
<optional: only if the triage step found something relevant>
- Related to card <url>: <how>
- Depends on <card url>: <how>
- Duplicates <card url>: <how>
```

## Self-check before publishing

Walk this checklist; if any item fails — rework or downgrade to QUESTIONS
(see `card-eval.md`).

- [ ] Every file in `## Files` actually exists in the repo today, or is
      marked `(new)`.
- [ ] Every command in `## Tests` will actually run (correct crate name,
      test filter resolves).
- [ ] `## Tests` includes a `cargo clippy ... --all-targets` entry and
      a `cargo fmt ... -- --check` entry.
- [ ] Clippy / fmt scope in `## Tests` matches the crate scope of
      `## Files` — one `-p <crate>` per crate touched. Workspace-wide
      (`--workspace` / `--all`) only when `## Files` spans 3+ crates
      or the card is deliberately a workspace-wide cleanup.
- [ ] `## Scope` and `## Out of scope` together cover anything a reader
      might wonder about.
- [ ] `## Approach` explains HOW; it does not repeat `## Scope` content.
- [ ] The plan leaves NO decisions to the worker — every choice that
      affects implementation is already made.
- [ ] **Confidence ≥ 7.** If lower, the plan is not ready — produce
      QUESTIONS instead, asking what would raise confidence.

## Parsing contract (for /trello-run)

`/trello-run` parses these exact section headers (`## Scope`, `## Files`,
`## Approach`, `## Tests`, `## Out of scope`, `## Metrics`). If a card has
a `[meta] PLAN` comment that lacks `## Metrics` or has unparseable values
(non-numeric Confidence, unknown Risk), the card goes to `Blocked` with a
note pointing here.

For `## Metrics`, the parser expects these exact field names (case-sensitive):
- `Confidence:` integer 0-10
- `Value:` one of `low`, `medium`, `high`
- `Risk:` one of `low`, `medium`, `high`
- `Expected iterations:` integer 1-3
- `Estimated size:` one of `S`, `M`, `L`

`## Cross-card notes` is optional and not parsed; it's for human readers.
