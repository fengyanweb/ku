---
title: 'Instalacja i pobieranie'
description: 'Pobierz eksperymentalne wydanie wstępne Ku 0.0.18 i poznaj wymagania platformy oraz budowania.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> Nowe informacje o v0.0.18 są na razie podane poniżej po angielsku; pozostałe tłumaczenie zachowano. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Wybierz platformę

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| System  | Architektura          | Archiwum                                     |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Nie zweryfikowano archiwów dla innych platform. Obsługa kompilacji skrośnej nie oznacza, że opublikowano pakiet dla każdego systemu lub architektury.

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

## Wymagania uruchamiania i budowania

| Operacja                                | Dodatkowe wymagania                                                                                      |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI dla odpowiedniej platformy                                                                        |
| Domyślne `ku build hello.ku`            | Rust **1.89.0** dla platformy docelowej i jej systemowy konsolidator; zachowaj dołączone zależności Rust |
| `ku build --backend c hello.ku`         | Działający docelowy kompilator C i konsolidator                                                          |
| Natywny program bazodanowy              | Odpowiednie zewnętrzne biblioteki dynamiczne klienta bazy danych; nie są dołączane                       |

Domyślny program uruchamiający osadza tylko źródło punktu wejścia. Importowane moduły nadal są ładowane z oryginalnych lokalizacji. Wdrożenie kompletnego lokalnego grafu importów bez źródeł wymaga natywnej kompilacji przez C i podlega [ograniczeniom zaplecza](/pl/docs/support/).

## Wypróbuj źródła rozwojowe

W lokalnej kopii repozytorium:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Odinstalowanie

Przy instalacji z archiwum usuń katalog rozpakowania, cofnij dodaną pozycję `PATH` i odinstaluj rozszerzenie VS Code. Najpierw zachowaj utworzone tam pliki źródłowe i konfigurację. Nie zweryfikowano osobnego instalatora jednym kliknięciem, instalacji menedżerem pakietów ani polecenia `ku uninstall`.

Dalej: [uruchom pierwszy program](/pl/docs/getting-started/). Źródło kontraktu instalacji: [archiwa wydań](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/pl/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
