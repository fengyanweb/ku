---
title: 'Instalacja i pobieranie'
description: 'Pobierz eksperymentalne wydanie wstępne Ku 0.0.18 i poznaj wymagania platformy oraz budowania.'
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
slug: pl/v0.0.18/docs/install
---

## Wybierz platformę

[Pobierz Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| System  | Architektura          | Archiwum                                     |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Gotowe pakiety są dostępne dla trzech podanych platform. Kompilacja skrośna nie oznacza opublikowanego pakietu dla każdego systemu.

## Rozpakuj i zweryfikuj

1. Porównaj archiwum z `SHA256SUMS` wydania. Osobny `.manifest.json` opisuje integralność archiwum i plików. Sumy kontrolne sprawdzają integralność, ale nie są podpisami wydawcy.
2. Rozpakuj **całe archiwum** do pustego katalogu i przeczytaj `README-INSTALL.md`. Zachowaj wzajemny układ `ku`, `libku.rlib`, `deps/` i `native-tls/`.
3. Sprawdź wersję z tego katalogu. W macOS / Linux:

```sh
./ku --version
```

W Windows PowerShell:

```powershell
.\ku.exe --version
```

Zgłoszona wersja powinna wynosić `0.0.18`. Opcjonalnie dodaj katalog do `PATH`, a następnie uruchom `ku --version`. Dla integracji z VS Code zainstaluj dołączone `ku-language-0.0.18.vsix`.

Instalacja do skopiowania: macOS wymaga curl, tar i shasum; Linux curl, tar, awk i sha256sum; Windows PowerShell i tar.exe. Bloki sprawdzają SHA256 i wykonują Hello Ku. PATH zmienia się tylko w podpowłoce POSIX lub jest przywracany w PowerShell, bez zmiany globalnej. Źródło Windows ma UTF-8 bez BOM i obsługuje ścieżki ze spacjami.

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

## Wymagania uruchamiania i budowania

| Operacja                                | Dodatkowe wymagania                                                                                      |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI dla odpowiedniej platformy                                                                        |
| Domyślne `ku build hello.ku`            | Rust **1.89.0** dla platformy docelowej i jej systemowy konsolidator; zachowaj dołączone zależności Rust |
| `ku build --backend c hello.ku`         | Działający docelowy kompilator C i konsolidator                                                          |
| Natywny program bazodanowy              | Odpowiednie zewnętrzne biblioteki dynamiczne klienta bazy danych; nie są dołączane                       |

Domyślny program uruchamiający osadza tylko źródło punktu wejścia. Importowane moduły nadal są ładowane z oryginalnych lokalizacji. Wdrożenie kompletnego lokalnego grafu importów bez źródeł wymaga natywnej kompilacji przez C i podlega [ograniczeniom zaplecza](/pl/v0.0.18/docs/support/).

Domyślny runner wywołuje rustc. Wybierz jawnie Rust 1.89.0 zgodnie z RELEASE.json i dołączonymi rlibs; inny domyślny Rust może powodować E0514. Ku musi być w tymczasowym PATH.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Budowanie odpowiednich źródeł

Poniższe polecenia kompilują opublikowany kod źródłowy.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Odinstalowanie

Instalacja korzysta z archiwów. Zachowaj własne źródła i ustawienia z katalogu instalacji, a następnie usuń katalog, dodane wpisy `PATH` i rozszerzenie VS Code.

Dalej: [uruchom pierwszy program](/pl/v0.0.18/docs/getting-started/). Źródło kontraktu instalacji: [archiwa wydań](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Wymagania i miary pamięci `ku supervise` opisuje [obsługa](/pl/v0.0.18/docs/support/). CLI macOS wykonuje programy, ale nie obsługuje tego trybu izolacji natywnie.
