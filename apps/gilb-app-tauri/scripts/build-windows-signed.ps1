<#
.SYNOPSIS
  Build the Tauri app and sign the .exe + .msi with a code-signing cert.

.DESCRIPTION
  Signing is kept OUT of tauri.conf.json so a plain `npm run tauri build`
  stays unsigned (works for anyone without the cert). This script merges the
  signing settings in at build time via a temporary `-c` overlay.

  Two ways to supply the certificate:
    * In the Windows cert store already  -> pass -Thumbprint (or set
      $env:WINDOWS_CERT_THUMBPRINT).
    * As a PFX (CI)                      -> set $env:WINDOWS_CERT_PFX_BASE64
      and $env:WINDOWS_CERT_PASSWORD; the PFX is imported and its thumbprint
      derived automatically.

.EXAMPLE
  # local, cert already in store, native ARM64 build:
  pwsh scripts/build-windows-signed.ps1 -Thumbprint ABC123... -Target aarch64-pc-windows-msvc

.EXAMPLE
  # CI, from a base64 PFX secret (host target):
  $env:WINDOWS_CERT_PFX_BASE64 = '...'; $env:WINDOWS_CERT_PASSWORD = '...'
  pwsh scripts/build-windows-signed.ps1
#>
[CmdletBinding()]
param(
  [string]$Thumbprint   = $env:WINDOWS_CERT_THUMBPRINT,
  [string]$PfxBase64    = $env:WINDOWS_CERT_PFX_BASE64,
  [string]$PfxPassword  = $env:WINDOWS_CERT_PASSWORD,
  [string]$TimestampUrl = "http://timestamp.digicert.com",
  [string]$Target       = ""
)
$ErrorActionPreference = "Stop"

# CI path: import the PFX from a base64 secret and derive its thumbprint.
if ($PfxBase64) {
  $pfx = Join-Path $env:TEMP "gilb-signing.pfx"
  [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($PfxBase64))
  $sec = ConvertTo-SecureString $PfxPassword -AsPlainText -Force
  $cert = Import-PfxCertificate -FilePath $pfx -CertStoreLocation Cert:\CurrentUser\My -Password $sec
  $Thumbprint = $cert.Thumbprint
  Remove-Item $pfx -Force
}
if (-not $Thumbprint) {
  throw "No certificate thumbprint. Pass -Thumbprint / set WINDOWS_CERT_THUMBPRINT, or provide WINDOWS_CERT_PFX_BASE64 + WINDOWS_CERT_PASSWORD."
}

# Signing overlay, merged into the config for this build only (not committed).
$overlay = [ordered]@{
  bundle = [ordered]@{
    windows = [ordered]@{
      certificateThumbprint = $Thumbprint
      digestAlgorithm       = "sha256"
      timestampUrl          = $TimestampUrl
    }
  }
}
$overlayPath = Join-Path $env:TEMP "gilb-tauri-signing.json"
$overlay | ConvertTo-Json -Depth 6 | Set-Content -Path $overlayPath -Encoding utf8

$appDir = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $appDir
try {
  $tauriArgs = @("run", "tauri", "build", "--", "-c", $overlayPath)
  if ($Target) { $tauriArgs += @("--target", $Target) }
  & npm @tauriArgs
  if ($LASTEXITCODE -ne 0) { throw "tauri build failed ($LASTEXITCODE)" }
}
finally {
  Pop-Location
  Remove-Item $overlayPath -ErrorAction SilentlyContinue
}
