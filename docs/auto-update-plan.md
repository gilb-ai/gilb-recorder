# Auto-update plan (macOS + Windows)

## Context

Gilb is a Tauri 2 desktop app (`apps/gilb-app-tauri`). It currently ships with
**no auto-update mechanism** — users would have to re-download and reinstall by
hand. We want the app to detect a new version, download it, verify it, install
it, and relaunch, on both **macOS** and **Windows** (the two platforms with a
real capture backend).

This document is the implementation plan only. Nothing here is wired yet.

### Current state (relevant facts)

- `tauri.conf.json`: `version = "1.0.0"`, bundle `targets = ["dmg","app","msi"]`,
  no `plugins.updater`, no `createUpdaterArtifacts`.
- Plugins present: `opener`, `dialog`, `autostart`. **No `updater`, no `process`.**
- macOS signing configured (`Developer ID Application: … (83856566PM)`,
  hardened runtime, entitlements). **Notarization is not wired in config.**
- Windows code-signing added on the `windows-capture-backend` branch
  (`scripts/build-windows-signed.ps1` + `windows-release.yml`, minisign-separate
  Authenticode).
- CI: `windows-build.yml` (unsigned `.msi`, x64) and `windows-release.yml`
  (signed `.msi`, x64). **No macOS release workflow.**
- The app bundles a sidecar (`binaries/gilb-mcp`) via `externalBin`; the
  `build-sidecars.sh` `beforeBuildCommand` stages it per target triple.

## How the Tauri 2 updater works (summary)

- Crates/packages: `tauri-plugin-updater` + `@tauri-apps/plugin-updater`; plus
  `tauri-plugin-process` + `@tauri-apps/plugin-process` for `relaunch()`.
- A dedicated **minisign-style updater keypair** (separate from Apple/Windows
  code-signing). Public key goes in `tauri.conf.json`; private key + password
  are build-time env (`TAURI_SIGNING_PRIVATE_KEY`,
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) and produce a `.sig` next to each bundle.
- `bundle.createUpdaterArtifacts: true` emits the updater artifacts:
  - macOS → `Gilb.app.tar.gz` (+ `.sig`) — needs the **`app`** bundle target.
  - Windows → NSIS `*-setup.exe` (+ `.sig`) — needs the **`nsis`** bundle target
    (recommended over MSI for in-place updates).
- The app polls an **endpoint** returning a JSON manifest; it compares versions,
  downloads the artifact for `{target}-{arch}` (e.g. `darwin-aarch64`,
  `windows-x86_64`), verifies the `.sig` against the embedded pubkey, installs,
  and relaunches.

Manifest shape (`latest.json`):
```json
{
  "version": "1.1.0",
  "notes": "…",
  "pub_date": "2026-06-04T00:00:00Z",
  "platforms": {
    "darwin-aarch64": { "signature": "<minisign>", "url": "https://…/Gilb_aarch64.app.tar.gz" },
    "darwin-x86_64":  { "signature": "…", "url": "https://…/Gilb_x64.app.tar.gz" },
    "windows-x86_64": { "signature": "…", "url": "https://…/Gilb_x64-setup.exe" }
  }
}
```

## Decisions (locked)

1. **Update host = GitHub Releases on `gilb-ai/gilb-recorder`.** The repo is
   public, so release assets and `latest.json` are anonymously downloadable —
   no server to run. Endpoint: the static updater JSON attached to the
   `latest` release. *(A custom endpoint / channels can come later if we want
   staged rollouts.)*
2. **Arch coverage:** macOS ships **separate `darwin-aarch64` + `darwin-x86_64`**
   builds (not a universal binary). Windows ships **`windows-x86_64` only** for
   now — ARM64 Windows runs it via the OS's x64 emulation; a native
   `windows-aarch64` build is deferred (see risks).
3. **Windows installer for updates = NSIS** (add `nsis` to targets). Keep `msi`
   for first-install distribution if desired; the updater consumes the NSIS one.
4. **macOS must be notarized**, not just signed — otherwise Gatekeeper blocks
   the app the updater swaps in. Wire notarization into the release build.
5. **Silent auto-update.** Check on launch and periodically; download + stage
   the update with no prompt. **Apply only when it's safe** — never call
   `relaunch()` during an active capture session (that would kill recording).
   Install on next app launch, or when idle / not recording. Also expose a
   manual "Check for updates" action for on-demand use.

## Work breakdown

### Phase 1 — Plugin + config wiring
- `npm run tauri add updater` and `tauri add process` (adds both crate + JS
  deps and capability entries). Verify `src-tauri/capabilities/*.json` grants
  `updater:default` + `process:allow-restart`.
- Generate keypair: `npm run tauri signer generate -- -w .tauri/gilb-updater.key`.
  Commit **only** the public key (into config); store the private key + password
  as CI secrets and in the maintainer's keychain.
- `tauri.conf.json`:
  - `bundle.createUpdaterArtifacts: true`
  - add `"nsis"` to `bundle.targets` (per-OS targets are fine: keep dmg/app on
    mac, add nsis on win)
  - `plugins.updater`: `pubkey`, `endpoints` (the GitHub Releases `latest.json`
    URL), `windows.installMode: "passive"`.

### Phase 2 — App-side update flow (silent)
- On launch (and on a periodic timer) call `check()`. If an update exists,
  `downloadAndInstall(onProgress)` silently — no prompt. Surface a small status
  in `#message` (e.g. "Updating…", "Update ready").
- **Recording guard:** never `relaunch()` while a capture session is active.
  Track recording state (already in `EngineStatus.recording`); if recording,
  stage the update and apply on next launch (or when capture stops / the app is
  idle). Only auto-relaunch when not recording.
- Keep a manual "Check for updates" action for on-demand use; reuse
  `plugin-dialog` only for surfacing errors, not for gating installs.

### Phase 3 — Release CI (the bulk of the work)
Adopt the official **`tauri-apps/tauri-action`** for a unified, tag-driven
release, replacing the ad-hoc `windows-release.yml`:
- Trigger: pushing a `v*` tag (after bumping `version` in `tauri.conf.json` +
  `package.json` + workspace as needed).
- Matrix of runners:
  - `macos-latest` — two builds, `--target aarch64-apple-darwin` and
    `--target x86_64-apple-darwin`, each signed with Developer ID, **notarized**,
    emitting `*.app.tar.gz` + `.sig`.
  - `windows-latest` (x64) for `windows-x86_64` — builds NSIS,
    Authenticode-signs, emits `*-setup.exe` + `.sig`. (Native `windows-aarch64`
    deferred.)
- Env/secrets passed to every job:
  - `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (updater).
  - macOS: `APPLE_CERTIFICATE` (+ password), `APPLE_SIGNING_IDENTITY`,
    `APPLE_ID`, `APPLE_PASSWORD` (app-specific), `APPLE_TEAM_ID` (83856566PM) —
    or an App Store Connect API key.
  - Windows: the existing `WINDOWS_CERT_PFX_BASE64` + `WINDOWS_CERT_PASSWORD`.
- `tauri-action` with `includeUpdaterJson: true` builds, signs, creates the
  GitHub Release, uploads artifacts, and generates/attaches `latest.json`
  pointing at them. Keep the sidecar build (`beforeBuildCommand`) intact.

### Phase 4 — Verification
- Build `v1.0.0` and `v1.0.1` releases; install `1.0.0`, confirm the app detects
  `1.0.1`, downloads, verifies, installs, relaunches into the new version — on
  **both** an ARM64 Windows box and an Apple Silicon mac.
- Tamper test: a bad/edited artifact must fail signature verification and not
  install.
- Confirm the notarized mac app launches post-update with no Gatekeeper prompt,
  and the Windows NSIS update keeps the app signed (Authenticode `Valid`).

## Files this will touch / add

- `apps/gilb-app-tauri/src-tauri/tauri.conf.json` — updater plugin, pubkey,
  endpoints, `createUpdaterArtifacts`, `nsis` target, version bumps.
- `apps/gilb-app-tauri/src-tauri/Cargo.toml` + `package.json` — updater +
  process plugins.
- `apps/gilb-app-tauri/src-tauri/capabilities/*.json` — updater/process perms.
- `apps/gilb-app-tauri/src/main.ts` (+ maybe a Rust command) — check/install UX.
- `.github/workflows/release.yml` (new, tauri-action) — supersedes
  `windows-release.yml` for the signed/published path.
- `docs/RELEASING.md` (new) — how to cut a release (version bump → tag → CI).

## Risks / open questions

- **Updater key custody.** Losing the private key means no future updates can be
  signed for installed clients. Plan a secure backup (the public key is baked
  into every shipped binary and can't be rotated for existing installs).
- **macOS notarization** adds Apple secrets + a notarization wait to CI; it's the
  most fiddly part. Must be solid or updates won't launch.
- **Native `windows-aarch64` deferred.** ARM64 Windows (incl. the dev VM) runs
  the x64 build via OS emulation — fine functionally, but heavier for a
  background recorder. Adding a native ARM64 artifact later needs a
  `windows-11-arm` runner or x64→arm64 cross with the ARM64 MSVC toolchain.
- **GitHub Releases as endpoint** is fine while the repo is public. If it ever
  goes private, asset downloads need auth → would require a proxy/custom host.
- **Sidecar signing** (gilb-mcp.exe / the mac sidecar) should be covered by the
  release signing so the updated bundle is fully trusted.
- **Channels / staged rollout** not in scope v1 (single `latest` channel).

## Out of scope (v1)

Linux (no capture backend yet); delta updates; update channels
(beta/stable); in-app changelog rendering beyond the manifest `notes`.
