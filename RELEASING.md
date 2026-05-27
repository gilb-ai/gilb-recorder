# Releasing gilb

This document is the runbook for the **maintainer** cutting a signed,
notarized macOS build. It does not apply to development builds —
`npm run tauri dev` and `cargo build` do not need anything below.

Distribution today is manual: ship the resulting `.dmg` to recipients
out of band. Auto-update via `tauri-plugin-updater` is planned for a
later release, once the repo is public and GitHub Releases can serve
as the update endpoint without baked-in tokens.

## Prerequisites

One-time setup on the build machine:

- macOS 13 (Ventura) or later.
- **Xcode Command Line Tools**: `xcode-select --install`.
- **Rust** stable toolchain. `rustup show` should print a stable
  channel; the workspace pins `edition = "2021"`.
- **Node** 20+ and **npm**.
- **Apple Developer ID Application certificate** installed in the
  login Keychain. Verify with:

  ```sh
  security find-identity -v -p codesigning
  ```

  The output must include a line like:

  ```
  1) ABC… "Developer ID Application: Leonid Dinershtein (83856566PM)"
  ```

  This is the identity referenced by `signingIdentity` in
  `apps/gilb-app-tauri/src-tauri/tauri.conf.json`.
- **Apple ID app-specific password** generated at
  <https://appleid.apple.com/account/manage> → Sign-In and Security →
  App-Specific Passwords. This is *not* your iCloud password.

### One-time notarization profile

Store the notarization credentials in the Keychain once, so the build
does not need them in plain env vars:

```sh
xcrun notarytool store-credentials gilb-notary \
  --apple-id  <your-apple-id-email> \
  --team-id   83856566PM \
  --password  <app-specific-password>
```

The profile name `gilb-notary` is what we reference below as
`@keychain:gilb-notary`.

## Pre-release sanity checks

Run from the repo root. All of these must be green before building:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Frontend builds clean:

```sh
cd apps/gilb-app-tauri
npm install
npm run build      # tsc + vite, writes apps/gilb-app-tauri/dist/
cd ../..
```

Headless capture smoke (requires Accessibility + Input Monitoring
granted to your terminal — Terminal.app, iTerm, etc.):

```sh
cargo run -p gilb-a11y --bin gilb-a11y-cli -- --seconds 5 --db /tmp/gilb-smoke.sqlite
sqlite3 /tmp/gilb-smoke.sqlite 'SELECT count(*), kind FROM actions GROUP BY kind'
rm /tmp/gilb-smoke.sqlite
```

You should see at least a few rows of `focus_change`, `click`, etc.

## Build

Bump the version first if needed. The version lives in **three**
independent places — npm and Tauri JSON do not read Cargo:

- `Cargo.toml` → `[workspace.package] version = "..."`
- `apps/gilb-app-tauri/src-tauri/tauri.conf.json` → `"version": "..."`
- `apps/gilb-app-tauri/package.json` → `"version": "..."`

After bumping, confirm there are no stale references:

```sh
grep -rn '"0\.1\.0"\|= "0\.1\.0"' . \
  --include='*.toml' --include='*.json' \
  --exclude-dir=node_modules --exclude-dir=target
```

Export the signing/notarization env vars and run the build:

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: Leonid Dinershtein (83856566PM)"
export APPLE_ID="<your-apple-id-email>"
export APPLE_TEAM_ID="83856566PM"
export APPLE_PASSWORD="@keychain:gilb-notary"

cd apps/gilb-app-tauri
npm install                # if not done already
npm run tauri build
```

Tauri 2 will:

1. Run `cargo build --release` for the workspace.
2. Run `npm run build` (tsc + vite) and embed the frontend.
3. Bundle `.app`, `.dmg`, and `.app.tar.gz`.
4. Codesign with `APPLE_SIGNING_IDENTITY` + hardened runtime +
   `entitlements.plist`.
5. Submit the bundle to Apple via `notarytool submit --wait` using
   `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID`.
6. Staple the notarization ticket to the `.dmg` and `.app`.

Artifacts land in:

```
apps/gilb-app-tauri/src-tauri/target/release/bundle/
  ├── dmg/gilb_<version>_<arch>.dmg
  ├── macos/gilb.app
  └── macos/gilb.app.tar.gz
```

`<arch>` is `aarch64` on Apple Silicon, `x64` on Intel. We only ship
the arch we built on; cross-arch builds are not configured.

## Verify the artifact

The DMG must pass all three checks before it leaves the machine:

```sh
DMG=apps/gilb-app-tauri/src-tauri/target/release/bundle/dmg/gilb_*.dmg

# Gatekeeper acceptance — must say "Notarized Developer ID".
spctl -a -vvv -t install "$DMG"

# Notarization ticket is stapled to the DMG.
stapler validate "$DMG"

# Inside the .app: signature chains to Apple, team ID is correct.
APP=apps/gilb-app-tauri/src-tauri/target/release/bundle/macos/gilb.app
codesign -dv --verbose=4 "$APP"
# Expect: TeamIdentifier=83856566PM, Authority chain ending at
# "Apple Root CA".
```

If any check fails, the DMG is not safe to send. Fix the cause; do
not ship a non-notarized build.

## Tag the release

Only after the artifact verifies cleanly:

```sh
git pull --rebase            # do not skip; the mac and dev box may have diverged
git tag -a v<version> -m "release v<version>"
git push origin v<version>
```

## Distribution

For each recipient, send:

1. The `.dmg` file directly (email, signed URL, whatever channel is
   appropriate for that recipient).
2. A link to [`INSTALL.md`](./INSTALL.md) — install steps, permission
   prompts, and what data the app collects.

## Troubleshooting

- **`notarytool` complains about credentials.** Re-run
  `xcrun notarytool store-credentials gilb-notary …`. The profile
  name in the Keychain must match what `APPLE_PASSWORD` references
  (`@keychain:gilb-notary`).
- **`spctl` says "source=Unnotarized Developer ID".** Notarization
  did not run or did not staple. Re-check that the four `APPLE_*`
  env vars were set in the same shell that ran `npm run tauri build`,
  and inspect the Tauri build log for `notarytool submit` lines.
- **`codesign` says "code object is not signed at all".** The signing
  identity in `tauri.conf.json` does not match anything in your
  Keychain. Verify with `security find-identity -v -p codesigning`;
  rebuild after fixing the name.
- **Build fails on a fresh checkout with linker errors against
  `core-graphics` / `accessibility-sys`.** macOS SDK headers are
  missing. Re-run `xcode-select --install` and reboot if needed.
