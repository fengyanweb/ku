[CmdletBinding()]
param([switch]$CheckOnly)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion.Major -lt 7) { throw "archive-release.ps1 requires PowerShell 7 or newer" }
$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$packageScript = Join-Path $PSScriptRoot "package-release.ps1"
if (-not (Test-Path -LiteralPath $repo -PathType Container) -or
    ([IO.File]::GetAttributes($repo) -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
    -not (Test-Path -LiteralPath $packageScript -PathType Leaf) -or
    ([IO.File]::GetAttributes($packageScript) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw "cannot resolve a plain Ku source tree and package-release.ps1"
}

& $packageScript -ArchiveInternal -CheckOnly:$CheckOnly
