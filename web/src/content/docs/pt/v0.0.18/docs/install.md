---
title: 'Instalação e downloads'
description: 'Baixe a versão preliminar experimental Ku 0.0.18 e entenda os requisitos de plataforma e compilação.'
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
slug: pt/v0.0.18/docs/install
---

## Escolha a plataforma

[Baixar Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Sistema | Arquitetura           | Arquivo                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Há pacotes pré-compilados para estas três plataformas. Compilação cruzada não significa que todo sistema tenha um pacote publicado.

## Extraia e verifique

1. Verifique o arquivo compactado com `SHA256SUMS` da versão. Um `.manifest.json` separado descreve a integridade do arquivo e do seu conteúdo. As somas de verificação verificam a integridade; não são assinaturas do publicador.
2. Extraia **todo o arquivo compactado** em um diretório vazio e leia `README-INSTALL.md`. Preserve a organização relativa de `ku`, `libku.rlib`, `deps/` e `native-tls/`.
3. Confira a versão nesse diretório. No macOS / Linux:

```sh
./ku --version
```

No Windows PowerShell:

```powershell
.\ku.exe --version
```

A versão informada deve ser `0.0.18`. Opcionalmente, adicione este diretório a `PATH` e execute `ku --version`. Instale o arquivo incluído `ku-language-0.0.18.vsix` para integração com o VS Code.

Instalação para copiar: macOS requer curl, tar e shasum; Linux curl, tar, awk e sha256sum; Windows PowerShell e tar.exe. Os blocos verificam SHA256 e executam Hello Ku. PATH muda apenas no subshell POSIX ou é restaurado no PowerShell, sem alterar o global. O fonte Windows é UTF-8 sem BOM e aceita caminhos com espaços.

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

## Requisitos de execução e compilação

| Operação                                | Requisitos adicionais                                                                             |
| --------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI do Ku para a plataforma correspondente                                                        |
| `ku build hello.ku` padrão              | Rust **1.89.0** para o destino e seu vinculador de sistema; manter as dependências Rust incluídas |
| `ku build --backend c hello.ku`         | Compilador e vinculador C funcionais para o destino                                               |
| Programa nativo de banco de dados       | Bibliotecas dinâmicas externas correspondentes do cliente de banco de dados; não estão incluídas  |

O executor padrão incorpora apenas o código-fonte de entrada. Os módulos importados continuam sendo carregados de seus locais originais. A implantação de um grafo completo de importações locais independente dos fontes exige compilação nativa em C e continua sujeita aos [limites de suporte dos backends](/pt/v0.0.18/docs/support/).

O runner padrão invoca rustc. Selecione explicitamente Rust 1.89.0 conforme RELEASE.json e os rlibs incluídos; outro Rust padrão pode causar E0514. Ku deve estar no PATH temporário.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Compilar os fontes correspondentes

Os comandos a seguir compilam o código-fonte publicado.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Desinstale

A instalação usa arquivos compactados. Salve seus fontes e ajustes na pasta de instalação e depois remova a pasta, entradas de `PATH` adicionadas e extensão do VS Code.

Próximo passo: [execute o primeiro programa](/pt/v0.0.18/docs/getting-started/). Fonte do contrato de instalação: [arquivos de versões](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Pré-requisitos e medidas de memória de `ku supervise` estão no [suporte](/pt/v0.0.18/docs/support/). A CLI macOS executa programas, mas não oferece esse isolamento nativamente.
