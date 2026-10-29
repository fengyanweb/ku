---
title: 'Instalación y descargas'
description: 'Descarga la versión preliminar experimental Ku 0.0.18 y conoce los requisitos de plataforma y compilación.'
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
slug: es/v0.0.18/docs/install
---

## Elige tu plataforma

[Descargar Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Sistema | Arquitectura          | Archivo                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Se ofrecen paquetes precompilados para estas tres plataformas. La compilación cruzada no significa que exista un paquete publicado para cada sistema.

## Extrae y verifica

1. Verifica el archivo con `SHA256SUMS` de la versión. Un `.manifest.json` independiente describe la integridad del archivo y de su contenido. Las sumas de comprobación verifican integridad; no son firmas del editor.
2. Extrae el **archivo completo** en un directorio vacío y lee `README-INSTALL.md`. Conserva la estructura relativa de `ku`, `libku.rlib`, `deps/` y `native-tls/`.
3. Comprueba la versión desde ese directorio. En macOS / Linux:

```sh
./ku --version
```

En Windows PowerShell:

```powershell
.\ku.exe --version
```

La versión indicada debería ser `0.0.18`. Si quieres, añade este directorio a `PATH` y ejecuta `ku --version`. Instala el archivo incluido `ku-language-0.0.18.vsix` para la integración con VS Code.

Instalación para copiar: macOS requiere curl, tar y shasum; Linux curl, tar, awk y sha256sum; Windows PowerShell y tar.exe. Los bloques verifican SHA256 y ejecutan Hello Ku. PATH solo cambia en el subshell POSIX o se restaura en PowerShell, sin modificar el global. La fuente Windows se escribe UTF-8 sin BOM y admite rutas con espacios.

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

## Requisitos de ejecución y compilación

| Operación                               | Requisitos adicionales                                                                                  |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI de Ku para la plataforma correspondiente                                                            |
| `ku build hello.ku` predeterminado      | Rust **1.89.0** para el destino y su enlazador de sistema; conservar las dependencias de Rust incluidas |
| `ku build --backend c hello.ku`         | Compilador y enlazador de C funcionales para el destino                                                 |
| Programa nativo de base de datos        | Bibliotecas dinámicas externas del cliente de base de datos correspondientes; no están incluidas        |

El ejecutor predeterminado solo incorpora el código fuente de entrada. Los módulos importados siguen cargándose desde sus ubicaciones originales. El despliegue de un grafo completo de importaciones locales sin depender del código fuente requiere compilación nativa en C y sigue sujeto a los [límites de compatibilidad de backends](/es/v0.0.18/docs/support/).

El runner predeterminado invoca rustc. Selecciona explícitamente Rust 1.89.0 conforme a RELEASE.json y los rlibs incluidos; otro Rust predeterminado puede causar E0514. Ku debe estar en el PATH temporal.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Compilar las fuentes correspondientes

Los siguientes comandos compilan el código fuente publicado.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Desinstala

La instalación usa archivos comprimidos. Guarda tus fuentes y ajustes del directorio de instalación y elimina después el directorio, las entradas de `PATH` que añadiste y la extensión de VS Code.

Siguiente paso: [ejecuta tu primer programa](/es/v0.0.18/docs/getting-started/). Fuente del contrato de instalación: [archivos de versiones](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Los requisitos y medidas de memoria de `ku supervise` están en [compatibilidad](/es/v0.0.18/docs/support/). La CLI de macOS ejecuta programas, pero no admite este aislamiento de forma nativa.
