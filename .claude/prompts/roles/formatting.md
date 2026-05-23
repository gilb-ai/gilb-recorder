# Role: formatting

Universal output rules. Apply on top of any other role.

## General

- No emoji in any output (commits, comments, PR body, chat, code)
  unless explicitly requested.
- No preamble (`Here is...`, `I will now...`, `Let me start by...`)
  and no trailing summary unless the user asked for one.
- Markdown is allowed but optional; plain text is fine when shorter.
- Inline code in backticks for paths, commands, function names, flag
  values. Fenced blocks only when the snippet wouldn't fit one line.

## Output contract for workers spawned via `claude -p`

When you are a worker driven by `/trello-run`, `stdout` must contain
exactly one of these two lines and NOTHING else:

```
PR_URL=<url>
```

— on success, after `git push` and `gh pr create` (iter 1) or
`git push` (iter ≥ 2). For iter ≥ 2 the URL must match the PR opened
in iter 1.

```
BLOCKED: <one sentence reason>
```

— when you cannot finish per the plan. The sentence must be parseable
in isolation (no "see above", no "as discussed earlier"). Exit code
must be `2` for `BLOCKED`, `0` for `PR_URL=`.

All other communication — progress notes, audit findings, code
review, explanations — belongs in commits or PR comments, never in
`stdout`. Meta parses your `stdout` deterministically; extra lines
trigger a `Worker produced ambiguous output` failure.
