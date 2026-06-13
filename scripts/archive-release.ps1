param(
    [switch] $CheckOnly,
    [switch] $SkipBuild
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repo

$cargoToml = Get-Content -LiteralPath "Cargo.toml" -Encoding UTF8
$versionLine = $cargoToml | Where-Object { $_ -match "^version\s*=" } | Select-Object -First 1
if (-not $versionLine -or $versionLine -notmatch "version\s*=\s*`"([^`"]+)`"") {
    throw "failed to read version from Cargo.toml"
}
$version = $Matches[1]
$historyDir = Join-Path "history" "v$version"
$releaseExe = Join-Path "target" "release\ku.exe"
$releaseLib = Join-Path "target" "release\libku.rlib"

if (-not $SkipBuild) {
    cargo build --release
}

foreach ($path in @($releaseExe, $releaseLib)) {
    if (-not (Test-Path -LiteralPath $path)) {
        throw "missing release artifact: $path"
    }
}

if ($CheckOnly) {
    Write-Output "archive check ok: ku $version -> $historyDir"
    exit 0
}

New-Item -ItemType Directory -Force -Path "release" | Out-Null
New-Item -ItemType Directory -Force -Path $historyDir | Out-Null
Copy-Item -LiteralPath $releaseExe -Destination "release\ku.exe" -Force
Copy-Item -LiteralPath $releaseLib -Destination "release\libku.rlib" -Force
Copy-Item -LiteralPath $releaseExe -Destination (Join-Path $historyDir "ku.exe") -Force
Copy-Item -LiteralPath $releaseLib -Destination (Join-Path $historyDir "libku.rlib") -Force

Write-Output "archive ok: ku $version"
