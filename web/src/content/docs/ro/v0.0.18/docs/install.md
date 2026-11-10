---
title: 'Instalare și descărcare'
description: 'Descarcă versiunea preliminară experimentală Ku 0.0.18 și înțelege cerințele de platformă și compilare.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
    'docs/v0.0.18-r1.md',
  ]
slug: ro/v0.0.18/docs/install
---

## Alege platforma

[Descarcă Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Sistem  | Arhitectură           | Arhivă                                       |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Pachetele precompilate sunt oferite pentru cele trei platforme indicate. Compilarea încrucișată nu implică un pachet publicat pentru fiecare sistem.

## Extragere și verificare

1. Verifică arhiva folosind `SHA256SUMS` al versiunii. Un `.manifest.json` separat descrie integritatea arhivei și fișierelor. Sumele de control verifică integritatea, nu sunt semnături ale editorului.
2. Extrage **întreaga arhivă** într-un director gol și citește `README-INSTALL.md`. Păstrează dispunerea relativă a `ku`, `libku.rlib`, `deps/` și `native-tls/`.
3. Verifică versiunea din acel director. Pe macOS / Linux:

```sh
./ku --version
```

În Windows PowerShell:

```powershell
.\ku.exe --version
```

Versiunea raportată trebuie să fie `0.0.18`. Opțional, adaugă directorul în `PATH`, apoi rulează `ku --version`. Instalează `ku-language-0.0.18.vsix` inclus pentru integrarea VS Code.

Instalare de copiat: macOS necesită curl, tar și shasum; Linux curl, tar, awk și sha256sum; Windows PowerShell și tar.exe. Blocurile verifică SHA256 și rulează Hello Ku. PATH se schimbă doar în subshellul POSIX sau se restaurează în PowerShell, nu global. Sursa Windows este UTF-8 fără BOM și acceptă căi cu spații.

### macOS · Apple Silicon

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

### Linux · x64 GNU

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

### Windows · x64

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

```text
ku 0.0.18
Hello Ku
```

## Cerințe de rulare și compilare

| Operație                                | Cerințe suplimentare                                                                     |
| --------------------------------------- | ---------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI pentru platforma corespunzătoare                                                  |
| `ku build hello.ku` implicit            | Rust **1.89.0** pentru țintă și linkerul sistemului; păstrează dependențele Rust incluse |
| `ku build --backend c hello.ku`         | Un compilator C și un linker funcționale pentru țintă                                    |
| Program nativ cu bază de date           | Biblioteci dinamice externe potrivite ale clientului bazei de date; nu sunt incluse      |

Lansatorul implicit încorporează doar sursa punctului de intrare. Modulele importate se încarcă în continuare din locațiile originale. Distribuirea întregului graf de importuri locale independent de surse necesită compilare nativă prin C și rămâne supusă [limitelor backendurilor](/ro/v0.0.18/docs/support/).

Runnerul implicit apelează rustc. Selectează explicit Rust 1.89.0 conform RELEASE.json și rlibs incluse; alt Rust implicit poate cauza E0514. Ku trebuie să fie în PATH temporar.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Construirea surselor corespunzătoare

Comenzile următoare compilează codul sursă publicat.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Dezinstalare

Instalarea folosește arhive. Salvează sursele și setările create în directorul instalării, apoi șterge directorul, intrările `PATH` adăugate și extensia VS Code.

Urmează: [rulează primul program](/ro/v0.0.18/docs/getting-started/). Sursa contractului de instalare: [arhivele versiunilor](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Condițiile și măsurile de memorie `ku supervise` sunt în [suport](/ro/v0.0.18/docs/support/). CLI macOS rulează programe, dar nu suportă nativ acest mod de izolare.
