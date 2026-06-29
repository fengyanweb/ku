param(
    [switch]$InstallExtension
)

$ErrorActionPreference = "Stop"
$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new()

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$cargoToml = Get-Content -LiteralPath "Cargo.toml" -Raw -Encoding UTF8
if ($cargoToml -notmatch 'version\s*=\s*"([^"]+)"') {
    throw "Cannot read package.version from Cargo.toml"
}
$version = $Matches[1]

Write-Host "Building Ku $version release..."
cargo build --release

New-Item -ItemType Directory -Force -Path "release" | Out-Null
Copy-Item -LiteralPath "target\release\ku.exe" -Destination "release\ku.exe" -Force
Copy-Item -LiteralPath "target\release\libku.rlib" -Destination "release\libku.rlib" -Force
if (Test-Path "target\release\ku.pdb") {
    Copy-Item -LiteralPath "target\release\ku.pdb" -Destination "release\ku.pdb" -Force
}

Push-Location "editors\vscode-ku"
try {
    npm test
    npx @vscode/vsce package --out "ku-language-$version.vsix"
    if ($InstallExtension) {
        code --install-extension ".\ku-language-$version.vsix" --force
    }
}
finally {
    Pop-Location
}

Write-Host "Release artifacts:"
Write-Host "  release\ku.exe"
Write-Host "  release\libku.rlib"
Write-Host "  editors\vscode-ku\ku-language-$version.vsix"
