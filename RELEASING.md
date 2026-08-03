# Releasing Gilb

Releases are built, signed, and published by `.github/workflows/release.yml`
(macOS aarch64 + x86_64, Windows x64). The updater endpoint
(`tauri.conf.json` → `plugins.updater.endpoints`) points at the GitHub
Release's `latest.json`, so cutting a release is what ships an auto-update.

Nothing here applies to development builds — `cargo build` and
`npm run tauri dev` need none of it.

## Branching & releases

Trunk-based: work lands on `main` via PRs, releases are cut by tagging. There
is **no long-lived release branch** — the auto-updater always points clients at
the latest stable release, so we maintain a single line. `main` is branch-
protected and kept green by CI (`.github/workflows/ci.yml` runs the Rust
checks on every PR), so any commit on `main` is in principle shippable.

A release branch would only earn its keep once we need to hotfix an already-
shipped version while `main` holds unreleasable work, or maintain multiple
version lines — neither applies yet. If that day comes, cut a short-lived
`release/x.y` for stabilization, tag from it, then delete it.

## When the release workflow runs

It triggers on exactly two things — nothing else (regular commits, branch
pushes, and PRs do **not** run it):

- **Pushing a `v*` tag** (e.g. `git push origin v1.0.1`) — the normal path.
- **Manual dispatch**: *Actions → Release → Run workflow*, with a tag input.

The workflow file must exist in the tagged commit, so the tag has to point at a
commit that already contains `.github/workflows/release.yml`.

## Cutting a release

1. Bump the version. It lives in **three** places — npm and the Tauri config do
   not read Cargo, so all three move together:

   - `Cargo.toml` → `[workspace.package] version`
   - `apps/gilb-app-tauri/src-tauri/tauri.conf.json` → `version`
   - `apps/gilb-app-tauri/package.json` → `version`

   CI fails if they disagree (the *Versions in sync* step in `ci.yml`). The
   Cargo one is not cosmetic: it is stamped into every recorded session row
   (`gilb_db::sessions`) and reported as the MCP server's version, so a stale
   value quietly mislabels data.

2. Commit, then tag and push:

   ```sh
   git tag v1.0.5
   git push origin v1.0.5
   ```

3. The workflow builds + signs all platforms and creates a **draft** GitHub
   Release with the installers, the updater bundles (`*.app.tar.gz`,
   `*-setup.exe`) + their `.sig`, and `latest.json`.

4. Review the draft, then **publish** it. Once published, installed apps pick
   up the update on their next check (launch / every 6h).

## Beta / pre-release builds

Tag with a semver pre-release suffix (e.g. `v1.1.0-beta.1`, `v1.2.0-rc.1`).
The workflow marks that GitHub Release as **pre-release**, so
`releases/latest` — the stable updater endpoint — **skips it**: stable users
never auto-update onto a beta. Testers install the pre-release build manually
from its release page. (No separate beta auto-update channel yet.)

## Repo secrets

Set under **Settings → Secrets and variables → Actions**. They are not stored
in the repo, are not exposed by making it public, and are not given to fork
PRs. Listed here for setup and rotation:

| Secret | What |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | content of the updater private key (`~/.tauri/gilb-updater.key`) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | its password (`""` if none) |
| `APPLE_CERTIFICATE` | base64 of the Developer ID `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | `.p12` password |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: <name> (<team id>)` |
| `APPLE_API_ISSUER` | App Store Connect API issuer id |
| `APPLE_API_KEY` | App Store Connect API key id |
| `APPLE_API_KEY_BASE64` | base64 of the `AuthKey_*.p8` |
| `ES_USERNAME` | SSL.com eSigner username |
| `ES_PASSWORD` | SSL.com eSigner password |
| `ES_CREDENTIAL_ID` | eSigner signing credential id |
| `ES_TOTP_SECRET` | eSigner TOTP secret (base32) |

Windows is Authenticode-signed via SSL.com CodeSignTool (cloud/eSigner) —
there is no local PFX. The release workflow downloads CodeSignTool and signs
each artifact through `scripts/sign-windows.ps1`.

`gh secret set NAME < file` (or `--body "value"`) is the quickest way.

The updater key is the one secret with no recovery path: lose it and existing
installs can never be updated again, because they only accept bundles signed by
its public half (baked into `tauri.conf.json`). Keep the backup somewhere that
survives the build machine.

## Building signed locally

Only needed to reproduce a release build outside CI — a one-off `.dmg` for a
specific recipient, or debugging a signing failure. Requires a Developer ID
Application certificate in the login Keychain (`security find-identity -v -p
codesigning` must list it) and a notarization profile stored once:

```sh
xcrun notarytool store-credentials gilb-notary \
  --apple-id  <your-apple-id-email> \
  --team-id   <your-team-id> \
  --password  <app-specific-password>   # appleid.apple.com, not your iCloud password
```

Then:

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: <name> (<team id>)"
export APPLE_ID="<your-apple-id-email>"
export APPLE_TEAM_ID="<your-team-id>"
export APPLE_PASSWORD="<app-specific-password>"   # the same one as in the profile

cd apps/gilb-app-tauri
npm install
npm run tauri build
```

Note: `APPLE_PASSWORD="@keychain:gilb-notary"` does **not** work — tauri-cli
passes the value as a literal password and notarization fails with 401. If
you'd rather not put the app-specific password in the environment, build with
`APPLE_PASSWORD` unset (bundling fails at the notarize step, after signing)
and finish by hand:

```sh
cd target/release/bundle/macos
ditto -c -k --keepParent Gilb.app Gilb.zip
xcrun notarytool submit Gilb.zip --keychain-profile gilb-notary --wait
xcrun stapler staple Gilb.app
hdiutil create -volname Gilb -srcfolder Gilb.app -ov -format UDZO \
  ../dmg/Gilb_<version>_aarch64.dmg
codesign --sign "$APPLE_SIGNING_IDENTITY" ../dmg/Gilb_<version>_aarch64.dmg
xcrun notarytool submit ../dmg/Gilb_<version>_aarch64.dmg \
  --keychain-profile gilb-notary --wait
xcrun stapler staple ../dmg/Gilb_<version>_aarch64.dmg
```

`beforeBuildCommand` stages the sidecars (`scripts/build-sidecars.sh` builds
`gilb-mcp` and `gilb-analyzer` in release mode as `binaries/<name>-<triple>`),
then Tauri builds the workspace, embeds the frontend, bundles `.app` / `.dmg` /
`.app.tar.gz`, codesigns the main binary *and* each sidecar with the hardened
runtime + `entitlements.plist`, notarizes, and staples.

Artifacts land in `apps/gilb-app-tauri/src-tauri/target/release/bundle/`.

## Verifying an artifact

Before a locally built `.dmg` leaves the machine — all three must pass:

```sh
DMG=target/release/bundle/dmg/Gilb_*.dmg
APP=target/release/bundle/macos/Gilb.app

spctl -a -vvv -t install "$DMG"      # must say "Notarized Developer ID"
stapler validate "$DMG"              # ticket is stapled
codesign -dv --verbose=4 "$APP"      # authority chain ends at "Apple Root CA"
codesign -dv --verbose=4 "$APP/Contents/MacOS/gilb-mcp"
```

A failure here means the build is not safe to distribute. Fix the cause rather
than shipping an unnotarized build — Gatekeeper will refuse it on the far end
anyway.

## Troubleshooting

- **`notarytool` complains about credentials, or notarization fails with 401
  "Invalid credentials".** `APPLE_PASSWORD` must be the app-specific password
  itself — tauri-cli does not resolve the `@keychain:` syntax. Either put the
  password in `APPLE_PASSWORD` or finish notarization by hand with
  `--keychain-profile gilb-notary` (see above). If the profile itself is
  broken, re-run `xcrun notarytool store-credentials gilb-notary …`.
- **A hand-built `.dmg` says "source=Unnotarized Developer ID" while the
  `.app` inside is accepted.** The signed `.dmg` needs its own notarization
  ticket: submit the `.dmg` to `notarytool` and staple it too.
- **`spctl` says "source=Unnotarized Developer ID".** Notarization did not run
  or did not staple. Check that the `APPLE_*` vars were set in the same shell
  that ran `npm run tauri build`, and look for `notarytool submit` in the log.
- **`codesign` says "code object is not signed at all".** `APPLE_SIGNING_IDENTITY`
  matches nothing in the Keychain — compare against
  `security find-identity -v -p codesigning`.
- **Fresh checkout fails to link `core-graphics` / `accessibility-sys`.** macOS
  SDK headers are missing: `xcode-select --install`.
- **`resource path 'binaries/gilb-mcp-<triple>' doesn't exist`.** The sidecar
  step did not run or failed — run
  `bash apps/gilb-app-tauri/scripts/build-sidecars.sh` and read its output.

## Notes

- macOS ships separate `aarch64` + `x86_64` builds; Windows ships `x64` only
  (ARM64 Windows runs it under emulation — native ARM64 is a future addition).
- The updater installs silently and relaunches; an active recording is stopped
  cleanly first (see `apps/gilb-app-tauri/src/main.ts`).
