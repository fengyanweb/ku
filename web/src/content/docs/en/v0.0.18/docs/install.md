---
title: Install Ku
description: Download Ku 0.0.18, verify and extract the macOS, Windows or Linux archive, and run your first program.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/0.0.18.md',
    '.github/scripts/export-release.py',
  ]
slug: en/v0.0.18/docs/install
---

Install **Ku 0.0.18**. This experimental version is not recommended for production. Download the matching archive from the [release page](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1), verify its checksum, and extract it.

## Choose a platform

Prebuilt archives are available for these three platforms. Each has a matching `.manifest.json`; the Release `SHA256SUMS` covers the three archives and three manifests. `native_ci-*` CI artifacts are examples, not installers.

| System  | Architecture          | Archive                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

See [platform support](/en/v0.0.18/docs/support/) for other source-build targets. The commands below download and check the matching archive.

## macOS · Apple Silicon

Requires `curl`, `tar`, and the system `shasum`. Copy the entire block into a terminal:

```sh
(
  set -eu
  base="https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1"
  archive="ku-v0.0.18-aarch64-apple-darwin.tar.gz"
  work="$(mktemp -d "${TMPDIR:-/tmp}/Ku install.XXXXXX")"
  cd "$work"
  curl --fail --location --output "$archive" "$base/$archive"
  curl --fail --location --output SHA256SUMS "$base/SHA256SUMS"
  awk -v name="$archive" '$2 == name { print }' SHA256SUMS > archive.sha256
  test "$(wc -l < archive.sha256 | tr -d ' ')" = 1
  shasum -a 256 -c archive.sha256
  mkdir "Ku 0.0.18"
  tar -xzf "$archive" -C "Ku 0.0.18"
  cd "Ku 0.0.18"
  export PATH="$PWD:$PATH"
  ku --version
  cat > hello.ku <<'KU'
fn main() {
    println("Hello Ku")
}
KU
  ku check hello.ku
  ku run hello.ku
  printf 'Ku directory: %s\n' "$PWD"
)
```

## Linux · x64 GNU

Requires `curl`, `tar`, `awk`, and `sha256sum`. Run on a matching Linux system:

```sh
(
  set -eu
  base="https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1"
  archive="ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz"
  work="$(mktemp -d "${TMPDIR:-/tmp}/Ku install.XXXXXX")"
  cd "$work"
  curl --fail --location --output "$archive" "$base/$archive"
  curl --fail --location --output SHA256SUMS "$base/SHA256SUMS"
  awk -v name="$archive" '$2 == name { print }' SHA256SUMS > archive.sha256
  test "$(wc -l < archive.sha256 | tr -d ' ')" = 1
  sha256sum -c archive.sha256
  mkdir "Ku 0.0.18"
  tar -xzf "$archive" -C "Ku 0.0.18"
  cd "Ku 0.0.18"
  export PATH="$PWD:$PATH"
  ku --version
  cat > hello.ku <<'KU'
fn main() {
    println("Hello Ku")
}
KU
  ku check hello.ku
  ku run hello.ku
  printf 'Ku directory: %s\n' "$PWD"
)
```

## Windows · x64

Run in PowerShell on Windows with `tar.exe` available. The source is written as UTF-8 without a BOM; paths containing spaces remain separate, quoted arguments:

```powershell
$ErrorActionPreference = 'Stop'
$base = 'https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1'
$archive = 'ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz'
$work = Join-Path ([IO.Path]::GetTempPath()) ('Ku install ' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$archivePath = Join-Path $work $archive
$sumsPath = Join-Path $work 'SHA256SUMS'
Invoke-WebRequest -Uri "$base/$archive" -OutFile $archivePath
Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath
$pattern = '^[0-9a-fA-F]{64}\s+' + [regex]::Escape($archive) + '$'
$lines = @(Get-Content -LiteralPath $sumsPath | Where-Object { $_ -match $pattern })
if ($lines.Count -ne 1) { throw 'Missing or ambiguous SHA256SUMS entry' }
$expected = ($lines[0] -split '\s+')[0]
$actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
if ($actual -ne $expected) { throw 'SHA256 mismatch; do not extract or run' }
$kuDir = Join-Path $work 'Ku 0.0.18'
New-Item -ItemType Directory -Path $kuDir | Out-Null
tar -xzf $archivePath -C $kuDir
if ($LASTEXITCODE -ne 0) { throw 'Archive extraction failed' }
$hello = @'
fn main() {
    println("Hello Ku")
}
'@
[IO.File]::WriteAllText((Join-Path $kuDir 'hello.ku'), $hello + "`n", [Text.UTF8Encoding]::new($false))
$previousPath = $env:Path
Push-Location -LiteralPath $kuDir
try {
    $env:Path = "$kuDir;$env:Path"
    ku.exe --version
    if ($LASTEXITCODE -ne 0) { throw 'Version check failed' }
    ku.exe check hello.ku
    if ($LASTEXITCODE -ne 0) { throw 'Source check failed' }
    ku.exe run hello.ku
    if ($LASTEXITCODE -ne 0) { throw 'Program failed' }
} finally {
    $env:Path = $previousPath
    Pop-Location
}
Write-Output "Ku directory: $kuDir"
```

## Expected results and installation layout

Each flow verifies the downloaded archive against the actual Release `SHA256SUMS` before extracting, checking the version, and running Hello Ku. Checksums establish file integrity, not a publisher's digital signature. Expected version output:

```text
ku 0.0.18
```

After `ku check` succeeds, `ku run hello.ku` prints:

```text
Hello Ku
```

The script prints the installation directory. POSIX PATH changes apply only inside the parenthesized subshell; PowerShell restores PATH in `finally`. Neither changes a global or persistent PATH. Afterwards, call `ku` / `ku.exe` using that directory's full path. If desired, temporarily add it to PATH in your current terminal; that separate change ends when you close the terminal.

Archive files extract directly into the destination, without an extra top-level package folder. Preserve the relative layout of `ku` (`ku.exe` on Windows), `libku.rlib`, `deps/`, and `native-tls/`, and read `README-INSTALL.md`. The package also contains `RELEASE.json`, a file-level `SHA256SUMS`, `THIRD_PARTY.json`, and `ku-language-0.0.18.vsix`. The internal checksum list has a different purpose from the identically named Release asset.

## VS Code extension

Choose “Install from VSIX” in the VS Code extensions menu and select `ku-language-0.0.18.vsix` in the installation directory. Reinstall when upgrading from the original 0.0.18 extension even though the version is unchanged. With the `code` command available, run from that directory:

```sh
code --install-extension ./ku-language-0.0.18.vsix --force
```

Ku's own code and extension have no open-source license grant; bundled third-party licenses apply only to their respective components. See [project and licensing](/en/v0.0.18/docs/project/).

## Running and building

| Operation                               | Additional requirements                                                                         |
| --------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `ku check hello.ku` / `ku run hello.ku` | Complete Ku package for the platform; no separate Rust or C compiler needed                     |
| Default `ku build hello.ku`             | Rust **1.89.0** and the system linker for the target; keep the bundled `libku.rlib` and `deps/` |
| `ku build --backend c hello.ku`         | Working target C compiler and linker                                                            |
| Native database program                 | Matching database client dynamic libraries, installed separately; not bundled                   |

The default runner invokes `rustc`, which must match `rust_toolchain` in the bundled `RELEASE.json`. With Rustup, install toolchain 1.89.0 and explicitly select it in a terminal where Ku is on the temporary PATH. Another default Rust version can be incompatible with the bundled rlibs:

```sh
rustup run 1.89.0 ku build hello.ku
```

In Windows PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

The default build creates an executable interpreter runner, embedding only the entry source. Imported modules still load from their original locations. The C backend compiles supported programs and can expand the complete local import graph during generation; see [backend support](/en/v0.0.18/docs/support/). A default runner does not mean all source has been compiled to native C.

The support page also describes `ku supervise` prerequisites and memory limits. The macOS CLI can run programs, but this isolation mode is not natively available there.

## Build the matching source

Requires Git, Rust 1.89.0, and the target linker. Start in a new directory:

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

When reporting a problem, include the commit from `RELEASE.json`, platform, backend, and full command.

## Uninstall

Preserve programs and configuration you created in the installation directory before deleting it, and uninstall the VS Code extension. The steps above do not persist PATH changes; undo any you added separately. Installation currently uses archives, with no separate uninstall command.

Next: [run your first program](/en/v0.0.18/docs/getting-started/).
