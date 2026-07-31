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
- macOS and Windows both have real capture backends; Linux has a no-op
  one that only keeps the workspace compiling. Platform code must sit
  cleanly behind `cfg(target_os = ...)` — the shared crates must build
  everywhere, because CI builds them on Linux.

## Before opening a PR

Run, from the repo root, and make sure each command exits clean. This is
what CI runs, so a green run here is a green run there:

```sh
cargo fmt --all
cargo clippy --workspace --exclude gilb-app-tauri --exclude gilb-shell-tauri \
    --all-targets -- -D warnings
cargo test --workspace --exclude gilb-app-tauri --exclude gilb-shell-tauri
```

The two Tauri crates are excluded because they need GTK/WebKit to build
on Linux. On macOS, check them too — the shell has real logic in it:

```sh
cargo clippy -p gilb-shell-tauri --features assist --all-targets -- -D warnings
bash apps/gilb-app-tauri/scripts/build-sidecars.sh   # or cargo check fails on
cargo check -p gilb-app-tauri                        # the missing sidecars
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
