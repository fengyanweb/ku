---
title: 'Installation und Downloads'
description: 'Laden Sie die experimentelle Vorabversion Ku 0.0.18 herunter und verstehen Sie Plattform- und Build-Anforderungen.'
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
slug: de/v0.0.18/docs/install
---

## Plattform auswählen

[Ku 0.0.18 herunterladen](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| System  | Architektur           | Archiv                                       |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Vorgefertigte Pakete werden für die drei genannten Plattformen angeboten. Cross-Kompilierung bedeutet kein veröffentlichtes Paket für jedes System.

## Entpacken und prüfen

1. Prüfen Sie das Archiv anhand der `SHA256SUMS` der Veröffentlichung. Eine separate `.manifest.json` beschreibt die Integrität des Archivs und der Dateien. Prüfsummen prüfen die Integrität und sind keine Signaturen des Herausgebers.
2. Entpacken Sie das **gesamte Archiv** in ein leeres Verzeichnis und lesen Sie `README-INSTALL.md`. Behalten Sie die relative Anordnung von `ku`, `libku.rlib`, `deps/` und `native-tls/` bei.
3. Prüfen Sie die Version aus diesem Verzeichnis. Unter macOS / Linux:

```sh
./ku --version
```

Unter Windows PowerShell:

```powershell
.\ku.exe --version
```

Die gemeldete Version sollte `0.0.18` sein. Optional können Sie dieses Verzeichnis zum `PATH` hinzufügen und danach `ku --version` ausführen. Installieren Sie für die VS-Code-Integration die mitgelieferte Datei `ku-language-0.0.18.vsix`.

Kopierbare Installation: macOS benötigt curl, tar und shasum, Linux curl, tar, awk und sha256sum, Windows PowerShell und tar.exe. Die Blöcke prüfen SHA256 und führen Hello Ku aus. PATH gilt nur im POSIX-Subshell beziehungsweise wird in PowerShell wiederhergestellt; kein globaler PATH wird geändert. Windows-Quellen sind UTF-8 ohne BOM; Pfade mit Leerzeichen sind berücksichtigt.

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

## Laufzeit- und Build-Anforderungen

| Vorgang                                 | Zusätzliche Anforderungen                                                                                  |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku-CLI für die passende Plattform                                                                          |
| Standardmäßiges `ku build hello.ku`     | Rust **1.89.0** für das Zielsystem und dessen System-Linker; mitgelieferte Rust-Abhängigkeiten beibehalten |
| `ku build --backend c hello.ku`         | Funktionsfähiger C-Compiler und Linker für das Zielsystem                                                  |
| Natives Datenbankprogramm               | Passende externe dynamische Datenbankclientbibliotheken; sie sind nicht enthalten                          |

Der Standard-Runner bettet nur den Einstiegsquelltext ein. Importierte Module werden weiterhin von ihren ursprünglichen Orten geladen. Eine vom Quelltext unabhängige Bereitstellung des vollständigen lokalen Importgraphen erfordert native C-Kompilierung und unterliegt weiterhin den [Grenzen der Backend-Unterstützung](/de/v0.0.18/docs/support/).

Der Standard-Runner ruft rustc auf. Wählen Sie ausdrücklich Rust 1.89.0 passend zu RELEASE.json und den mitgelieferten rlibs; ein anderes Standard-Rust kann E0514 verursachen. Ku muss im temporären PATH liegen.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Den passenden Quellstand bauen

Die folgenden Befehle bauen den veröffentlichten Quellstand.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Deinstallation

Die Installation erfolgt aus Archiven. Sichern Sie eigene Quellen und Einstellungen im Installationsordner, entfernen Sie anschließend den Ordner, eigene `PATH`-Einträge und die VS-Code-Erweiterung.

Als Nächstes: [Ihr erstes Programm ausführen](/de/v0.0.18/docs/getting-started/). Quelle des Installationsvertrags: [Veröffentlichungsarchive](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Die Plattformbedingungen und Speichermaße von `ku supervise` stehen unter [Unterstützung](/de/v0.0.18/docs/support/). Die macOS-CLI führt Programme aus, unterstützt diesen Isolationsmodus aber nicht nativ.
