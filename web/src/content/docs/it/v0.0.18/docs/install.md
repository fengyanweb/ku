---
title: 'Installazione e download'
description: 'Scarica la versione preliminare sperimentale Ku 0.0.18 e comprendi i requisiti di piattaforma e compilazione.'
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
slug: it/v0.0.18/docs/install
---

## Scegli la piattaforma

[Scarica Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Sistema | Architettura          | Archivio                                     |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Sono disponibili pacchetti precompilati per queste tre piattaforme. La compilazione incrociata non implica un pacchetto pubblicato per ogni sistema.

## Estrai e verifica

1. Verifica l'archivio con `SHA256SUMS` della versione. Un `.manifest.json` separato descrive l'integrità dell'archivio e dei file. Le somme di controllo verificano l'integrità; non sono firme dell'editore.
2. Estrai **l'intero archivio** in una directory vuota e leggi `README-INSTALL.md`. Conserva la disposizione relativa di `ku`, `libku.rlib`, `deps/` e `native-tls/`.
3. Controlla la versione da quella directory. Su macOS / Linux:

```sh
./ku --version
```

Su Windows PowerShell:

```powershell
.\ku.exe --version
```

La versione riportata dovrebbe essere `0.0.18`. Se vuoi, aggiungi questa directory a `PATH`, quindi esegui `ku --version`. Installa il file incluso `ku-language-0.0.18.vsix` per l'integrazione con VS Code.

Installazione da copiare: macOS richiede curl, tar e shasum; Linux curl, tar, awk e sha256sum; Windows PowerShell e tar.exe. I blocchi verificano SHA256 ed eseguono Hello Ku. PATH cambia solo nel subshell POSIX o viene ripristinato in PowerShell, mai globalmente. La fonte Windows è UTF-8 senza BOM e gestisce percorsi con spazi.

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

## Requisiti di esecuzione e compilazione

| Operazione                              | Requisiti aggiuntivi                                                                              |
| --------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI Ku per la piattaforma corrispondente                                                          |
| `ku build hello.ku` predefinito         | Rust **1.89.0** per la destinazione e il linker di sistema; conservare le dipendenze Rust incluse |
| `ku build --backend c hello.ku`         | Compilatore e linker C funzionanti per la destinazione                                            |
| Programma nativo per database           | Librerie dinamiche esterne del client database corrispondenti; non sono incluse                   |

Il runner predefinito incorpora solo il sorgente di ingresso. I moduli importati continuano a caricarsi dalle posizioni originali. Il rilascio di un grafo completo di importazioni locali indipendente dal sorgente richiede la compilazione nativa C e resta soggetto ai [limiti di supporto dei backend](/it/v0.0.18/docs/support/).

Il runner predefinito invoca rustc. Seleziona esplicitamente Rust 1.89.0 secondo RELEASE.json e gli rlibs inclusi; un Rust predefinito diverso può causare E0514. Ku deve essere nel PATH temporaneo.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Compilare i sorgenti corrispondenti

I seguenti comandi compilano il codice sorgente pubblicato.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Disinstalla

L’installazione usa archivi. Salva sorgenti e impostazioni creati nella cartella di installazione, quindi rimuovi cartella, voci `PATH` aggiunte ed estensione VS Code.

Passo successivo: [esegui il tuo primo programma](/it/v0.0.18/docs/getting-started/). Fonte del contratto di installazione: [archivi delle versioni](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Prerequisiti e misure di memoria di `ku supervise` sono nel [supporto](/it/v0.0.18/docs/support/). La CLI macOS esegue programmi, ma non supporta questo isolamento nativamente.
