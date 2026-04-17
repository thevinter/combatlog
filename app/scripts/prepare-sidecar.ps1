# PowerShell equivalent of prepare-sidecar.sh for Windows hosts.
# Downloads the Node.js binary for the host target and stages it into
# src-tauri\binaries\ with the target-triple suffix Tauri expects.
# Also copies the root parser-harness.js into src-tauri\resources\.

[CmdletBinding()]
param(
  [string]$Target = "",
  [string]$NodeVersion = $(if ($env:NODE_VERSION) { $env:NODE_VERSION } else { "v20.18.1" })
)

$ErrorActionPreference = "Stop"

Set-Location -Path (Join-Path $PSScriptRoot "..")
$Root = (Resolve-Path "..").Path
$BinariesDir = "src-tauri\binaries"
$ResourcesDir = "src-tauri\resources"

if (-not $Target) {
  $hostLine = (& rustc -vV) | Select-String -Pattern '^host: '
  if (-not $hostLine) { throw "could not detect host target via rustc -vV" }
  $Target = ($hostLine -split ': ')[1].Trim()
}

switch ($Target) {
  "x86_64-pc-windows-msvc" {
    $NodeArchive = "node-$NodeVersion-win-x64.zip"
    $NodeBinPath = "node-$NodeVersion-win-x64\node.exe"
    $Ext = ".exe"
  }
  "x86_64-pc-windows-gnu" {
    $NodeArchive = "node-$NodeVersion-win-x64.zip"
    $NodeBinPath = "node-$NodeVersion-win-x64\node.exe"
    $Ext = ".exe"
  }
  "aarch64-pc-windows-msvc" {
    $NodeArchive = "node-$NodeVersion-win-arm64.zip"
    $NodeBinPath = "node-$NodeVersion-win-arm64\node.exe"
    $Ext = ".exe"
  }
  default {
    Write-Error "unsupported target: $Target"
    exit 2
  }
}

New-Item -ItemType Directory -Force -Path $BinariesDir, $ResourcesDir | Out-Null
Copy-Item -Force (Join-Path $Root "parser-harness.js") (Join-Path $ResourcesDir "parser-harness.js")

$Dest = Join-Path $BinariesDir ("node-{0}{1}" -f $Target, $Ext)
if (Test-Path $Dest) {
  Write-Host "sidecar already present: $Dest"
  exit 0
}

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("sidecar-" + [guid]::NewGuid().ToString("N")))
try {
  $url = "https://nodejs.org/dist/$NodeVersion/$NodeArchive"
  $archivePath = Join-Path $tmp.FullName $NodeArchive
  Write-Host "downloading $url"
  Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing
  Expand-Archive -Path $archivePath -DestinationPath $tmp.FullName -Force
  Copy-Item -Force (Join-Path $tmp.FullName $NodeBinPath) $Dest
  Write-Host "staged sidecar: $Dest"
}
finally {
  Remove-Item -Recurse -Force $tmp.FullName -ErrorAction SilentlyContinue
}
