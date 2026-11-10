---
title: 'Instalacja i pobieranie'
description: 'Pobierz Ku 0.0.20 dla swojego komputera. Każdy pakiet zawiera narzędzie wiersza poleceń, interpreter i rozszerzenie VS Code.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Pobierz **Ku 0.0.20** dla swojego komputera. Każdy pakiet zawiera narzędzie wiersza poleceń, interpreter i rozszerzenie VS Code.

## 1. Pobierz Ku

| System  | Architektura          | Pobierz                                                                                                                          |
| ------- | --------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / ARM64 | [Pobierz macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [Pobierz Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [Pobierz Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## 2. Rozpakuj i uruchom

Rozpakuj **cały pakiet** do pustego folderu, na przykład `ku` w swoim folderze użytkownika. Zachowaj wszystkie pliki razem; nie przenoś samego pliku wykonywalnego. Otwórz terminal w tym folderze.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

Powinien pojawić się napis `ku 0.0.20`. Możesz już uruchamiać i sprawdzać pliki `.ku`; te operacje nie wymagają instalacji Rust.

<details>
<summary>Opcjonalnie: używaj ku z dowolnego folderu</summary>

Jeśli pakiet rozpakowano do `~/ku`, dodaj tę linię do pliku startowego powłoki, na przykład `~/.zshrc` lub `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

W Windows otwórz „Edytuj zmienne środowiskowe dla konta”, zmodyfikuj zmienną użytkownika `Path` i dodaj pełną ścieżkę rozpakowanego folderu, na przykład `%USERPROFILE%\ku`. Zachowaj istniejące wpisy.

Otwórz nowy terminal i uruchom `ku --version`.

</details>

## 3. Zainstaluj rozszerzenie VS Code

W widoku rozszerzeń VS Code wybierz **… → Zainstaluj z pliku VSIX…**, wskaż `ku-language-0.0.20.vsix` w rozpakowanym folderze i ponownie załaduj okno.

Jeśli Ku nie znajduje się w `PATH`, ustaw `ku.executablePath` na pełną ścieżkę do `ku` (`ku.exe` w Windows).

<details>
<summary>Sprawdź pobrany plik</summary>

Porównaj sumę SHA-256 pakietu z [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS). [Strona wydania](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) zawiera też plik `.manifest.json` dla każdego pakietu z sumami kontrolnymi plików. Dalsze instrukcje znajdują się w `README-INSTALL.md` w pakiecie.

</details>

<details>
<summary>Kompilowanie własnych programów Ku</summary>

`ku run` i `ku check` nie wymagają Rust. Domyślne `ku build` wymaga linkera systemowego oraz dokładnej tożsamości kompilatora Rust zapisanej w zainstalowanym SDK; pakiety v0.0.20 używają **Rust 1.99.1**. `ku build --backend c` wymaga kompilatora C i linkera dla platformy docelowej. Natywne programy bazodanowe wymagają też odpowiednich bibliotek klienckich, których pakiet nie zawiera.

</details>

<details>
<summary>Odinstalowanie</summary>

Zachowaj własne pliki źródłowe i konfigurację, a następnie usuń rozpakowany folder, dodany wpis `PATH` oraz rozszerzenie VS Code.

</details>

[Dalej: uruchom pierwszy program](/pl/docs/getting-started/)
