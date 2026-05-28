# Contributing

Thanks for thinking about helping. gilb is small and the workflow is
intentionally light.

## Ground rules

- Code, commit messages, PR titles, and PR descriptions are all in
  **English**.
- Don't add a feature without first opening an issue describing the
  problem it solves. For bug fixes you can jump straight to a PR.
- Don't add a runtime dependency unless you've thought about (and
  written down) why the standard library or an existing workspace
  crate isn't enough. We try to keep the dependency graph small.
- macOS is the only supported platform today. Windows / Linux code is
  welcome but must be cleanly behind `cfg(target_os = ...)`.

## Before opening a PR

Run, from the repo root, and make sure each command exits clean:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If your change touches the Tauri frontend:

```sh
cd apps/gilb-app-tauri
npm install
npm run build
```

If your change touches the database schema or any column read by
`apps/gilb-mcp`, update `apps/gilb-mcp/help.md` in the same PR — that
file is the user-facing contract for LLM clients.

If your change touches macOS accessibility / permissions code, smoke
the headless capture for a few seconds against a temp database:

```sh
cargo run -p gilb-a11y --bin gilb-a11y-cli -- --seconds 5 --db /tmp/gilb-smoke.sqlite
```

## Commit style

Match the existing log:

- Subject in the imperative, lowercase scope prefix, ≤72 chars.
  Example: `a11y/macos: drop redundant unsafe around children.get`.
- Body wraps around 72 columns, explains the *why* — the diff already
  shows the *what*.
- One logical change per commit. Squash before opening the PR if you
  have noise.

## Code review

PRs are reviewed by the maintainers. Expect questions about anything
that grows the permission surface, allocates without a reason, or
introduces unsafe.

## Security

If you've found a vulnerability, don't open a public issue — see
[`SECURITY.md`](./SECURITY.md).
