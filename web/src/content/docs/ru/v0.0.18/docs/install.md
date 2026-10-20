---
title: Установка и загрузка
description: Загрузите экспериментальную предварительную версию Ku 0.0.18 и
  изучите требования к платформе и сборке.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - Cargo.toml
  - docs/v0.0.18.md
  - docs/history.md
  - .github/scripts/export-release.py
slug: ru/v0.0.18/docs/install
---

> Дополнения для v0.0.18 ниже пока приведены на английском; остальной перевод сохранён. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Выбор платформы

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| Система | Архитектура           | Архив                                        |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Архивы для других платформ не проверялись. Поддержка кросс-компиляции не означает наличие опубликованного пакета для каждой ОС или архитектуры.

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

## Требования для запуска и сборки

| Операция                                | Дополнительные требования                                                                              |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI для соответствующей платформы                                                                   |
| Обычная `ku build hello.ku`             | Rust **1.89.0** для целевой платформы и системный компоновщик; сохраните поставляемые зависимости Rust |
| `ku build --backend c hello.ku`         | Рабочий компилятор C и компоновщик для целевой платформы                                               |
| Нативная программа с базой данных       | Подходящие внешние динамические библиотеки клиента БД; они не входят в архив                           |

Средство запуска по умолчанию встраивает только исходный код точки входа. Импортируемые модули по-прежнему загружаются из исходных расположений. Развёртывание полного графа локальных импортов без исходных файлов требует нативной компиляции через C и подчиняется [ограничениям бэкенда](/ru/v0.0.18/docs/support/).

## Проверка исходного кода разработки

Из локальной копии репозитория:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Удаление

При установке из архива удалите каталог распаковки, отмените добавленную запись `PATH` и удалите расширение VS Code. Сначала сохраните созданные там исходные файлы и настройки. Отдельный установщик в один клик, установка через менеджер пакетов и команда `ku uninstall` не проверены.

Далее: [запустите первую программу](/ru/v0.0.18/docs/getting-started/). Источник правил установки: [архивы выпусков](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/ru/v0.0.18/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
