# Releasing Gilb

Releases are built, signed, and published by `.github/workflows/release.yml`
(macOS aarch64 + x86_64, Windows x64). The updater endpoint
(`tauri.conf.json` → `plugins.updater.endpoints`) points at the GitHub
Release's `latest.json`, so cutting a release is what ships an auto-update.

## One-time setup (repo secrets)

Set these under **Settings → Secrets and variables → Actions** (they are not
stored in the repo; a public repo does not expose them, and they are not given
to fork PRs):

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
| `WINDOWS_CERT_PFX_BASE64` | base64 of the code-signing `.pfx` |
| `WINDOWS_CERT_PASSWORD` | `.pfx` password |

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
