# Releasing Gilb

Releases are built, signed, and published by `.github/workflows/release.yml`
(macOS aarch64 + x86_64, Windows x64). The updater endpoint
(`tauri.conf.json` → `plugins.updater.endpoints`) points at the GitHub
Release's `latest.json`, so cutting a release is what ships an auto-update.

## When the release workflow runs

It triggers on exactly two things — nothing else (regular commits, branch
pushes, and PRs do **not** run it):

- **Pushing a `v*` tag** (e.g. `git push origin v1.0.1`) — the normal path.
- **Manual dispatch**: *Actions → Release → Run workflow*, with a tag input.

The workflow file must exist in the tagged commit, so the tag has to point at a
commit that already contains `.github/workflows/release.yml`.

## One-time setup (repo secrets) — already configured

These 12 secrets are set under **Settings → Secrets and variables → Actions**
(they are not stored in the repo; a public repo does not expose them, and they
are not given to fork PRs). Listed here for reference / rotation:

| Secret | What |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | content of `~/.tauri/gilb-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | its password (`""` if none) |
| `APPLE_CERTIFICATE` | base64 of the Developer ID `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | `.p12` password |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: … (83856566PM)` |
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

## Cutting a release

1. Bump the version in `apps/gilb-app-tauri/src-tauri/tauri.conf.json` and
   `apps/gilb-app-tauri/package.json` (keep them in sync).
2. Commit, then tag and push:
   ```sh
   git tag v1.0.1
   git push origin v1.0.1
   ```
3. The workflow builds + signs all platforms and creates a **draft** GitHub
   Release with the installers, the updater bundles (`*.app.tar.gz`,
   `*-setup.exe`) + their `.sig`, and `latest.json`.
4. Review the draft, then **publish** it. Once published, installed apps pick up
   the update on their next check (launch / every 6h).

(You can also trigger the workflow manually via *Actions → Release → Run
workflow* with a tag input.)

## Notes

- macOS ships separate `aarch64` + `x86_64` builds; Windows ships `x64` only
  (ARM64 Windows runs it via emulation — native ARM64 is a future addition).
- The updater installs silently and relaunches; an active recording is stopped
  cleanly first (see `src/main.ts`).
- Losing the updater private key means existing installs can never be updated
  again — keep the secret-manager backup safe.
