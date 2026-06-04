<#
.SYNOPSIS
  Authenticode-sign one file via SSL.com CodeSignTool (eSigner cloud signing).

.DESCRIPTION
  Invoked by Tauri's `bundle.windows.signCommand` for each artifact (the app
  .exe and the NSIS installer), so signing happens during bundling — before
  Tauri computes the updater (.sig) signature, keeping the uploaded artifact in
  sync with its signature.

  Credentials and the CodeSignTool location come from the environment (set by
  the release workflow), never from the repo:
    CODE_SIGN_TOOL_PATH  directory containing CodeSignTool.bat
    ES_USERNAME          SSL.com eSigner username
    ES_PASSWORD          eSigner password
    ES_CREDENTIAL_ID     signing credential id
    ES_TOTP_SECRET       eSigner TOTP secret (base32)

  Usage: pwsh -File sign-windows.ps1 <path-to-file>
#>
param(
  [Parameter(Mandatory = $true, Position = 0)]
  [string]$File
)
$ErrorActionPreference = "Stop"

$tool = Join-Path $env:CODE_SIGN_TOOL_PATH "CodeSignTool.bat"
if (-not (Test-Path $tool)) {
  throw "CodeSignTool not found at $tool (CODE_SIGN_TOOL_PATH not set?)"
}

# `-override` signs the file in place, which is what Tauri expects.
& $tool sign `
  "-username=$env:ES_USERNAME" `
  "-password=$env:ES_PASSWORD" `
  "-credential_id=$env:ES_CREDENTIAL_ID" `
  "-totp_secret=$env:ES_TOTP_SECRET" `
  "-input_file_path=$File" `
  -override

if ($LASTEXITCODE -ne 0) {
  throw "CodeSignTool failed ($LASTEXITCODE) for $File"
}
