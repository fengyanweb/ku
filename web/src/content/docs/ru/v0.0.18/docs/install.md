---
title: 'Установка и загрузка'
description: 'Загрузите экспериментальную предварительную версию Ku 0.0.18 и изучите требования к платформе и сборке.'
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
slug: ru/v0.0.18/docs/install
---

## Выбор платформы

[Скачать Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Система | Архитектура           | Архив                                        |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Готовые пакеты доступны для трёх указанных платформ. Кросс-компиляция не означает наличие опубликованного пакета для каждой системы.

## Распаковка и проверка

1. Сверьте архив с `SHA256SUMS` выпуска. Отдельный `.manifest.json` описывает целостность архива и файлов. Контрольные суммы проверяют целостность, но не являются подписями издателя.
2. Распакуйте **весь архив** в пустой каталог и прочитайте `README-INSTALL.md`. Сохраните взаимное расположение `ku`, `libku.rlib`, `deps/` и `native-tls/`.
3. Проверьте версию из этого каталога. В macOS / Linux:

```sh
./ku --version
```

В Windows PowerShell:

```powershell
.\ku.exe --version
```

Должна отображаться версия `0.0.18`. При желании добавьте каталог в `PATH`, затем выполните `ku --version`. Для интеграции с VS Code установите входящий в архив `ku-language-0.0.18.vsix`.

Готовые команды установки: macOS требует curl, tar и shasum; Linux — curl, tar, awk и sha256sum; Windows — PowerShell и tar.exe. Блоки проверяют SHA256 и запускают Hello Ku. PATH меняется только в дочерней оболочке POSIX либо восстанавливается в PowerShell; глобальные настройки не меняются. Исходник Windows записывается в UTF-8 без BOM; пути с пробелами поддерживаются.

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

## Требования для запуска и сборки

| Операция                                | Дополнительные требования                                                                              |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI для соответствующей платформы                                                                   |
| Обычная `ku build hello.ku`             | Rust **1.89.0** для целевой платформы и системный компоновщик; сохраните поставляемые зависимости Rust |
| `ku build --backend c hello.ku`         | Рабочий компилятор C и компоновщик для целевой платформы                                               |
| Нативная программа с базой данных       | Подходящие внешние динамические библиотеки клиента БД; они не входят в архив                           |

Средство запуска по умолчанию встраивает только исходный код точки входа. Импортируемые модули по-прежнему загружаются из исходных расположений. Развёртывание полного графа локальных импортов без исходных файлов требует нативной компиляции через C и подчиняется [ограничениям бэкенда](/ru/v0.0.18/docs/support/).

Runner по умолчанию вызывает rustc. Явно выберите Rust 1.89.0 согласно RELEASE.json и входящим rlibs; другая версия Rust может вызвать E0514. Ku должен находиться во временном PATH.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Сборка соответствующих исходников

Следующие команды собирают опубликованный исходный код.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Удаление

Установка выполняется из архива. Сохраните свои исходники и настройки в каталоге установки, затем удалите каталог, добавленные записи `PATH` и расширение VS Code.

Далее: [запустите первую программу](/ru/v0.0.18/docs/getting-started/). Источник правил установки: [архивы выпусков](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Условия платформ и показатели памяти `ku supervise` описаны в [поддержке](/ru/v0.0.18/docs/support/). CLI macOS выполняет программы, но нативно не поддерживает этот режим изоляции.
