---
title: 'Błędy i rozwiązywanie problemów'
description: 'Diagnozuj błędy instalacji Ku, importów, własności, JSON i możliwości zaplecza.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
  ]
---

Dokumentacja opisuje zamrożony zakres eksperymentalnego wydania wstępnego v0.0.18. Faktyczna publikacja i pliki do pobrania są zapisane w GitHub Releases. Diagnostyka kompilatora może się zmieniać podczas eksperymentalnego rozwoju. W zgłoszeniu problemu podaj wersję CLI, commit Git, zaplecze, pełne polecenie i minimalne źródło.

## Rozpoznaj plik wykonywalny Ku

```sh
ku --version
ku -h
ku check hello.ku
```

Jeżeli nie znaleziono `ku`, uruchom plik wykonywalny z katalogu rozpakowania. W Windows użyj `ku.exe`. Jeśli domyślnej kompilacji brakuje Rust lub bibliotek do konsolidacji, sprawdź [wymagania instalacji](/pl/docs/install/) i zachowaj pełny układ archiwum zamiast kopiować samo CLI.

## Czytanie diagnostyki

Zwykłe sprawdzanie podaje plik, pozycję w źródle, fragment kodu i dostępne wskazówki naprawy. Integracje narzędziowe mogą używać:

```sh
ku check --json hello.ku
```

Wyjście to **JSON Lines**, jeden komunikat diagnostyczny na wiersz. Pola obejmują `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` i `helps`. Sukces jest cichy; brak JSON na wyjściu nie oznacza, że sprawdzanie nie zostało wykonane.

| Diagnostyka | Częsta przyczyna                       | Co zrobić                                                                           |
| ----------- | -------------------------------------- | ----------------------------------------------------------------------------------- |
| `E0105`     | Użycie `let`                           | Zadeklaruj zmienną pierwszym przypisaniem                                           |
| `E0302`     | Warunek `if` / `while` inny niż bool   | Napisz jawne porównanie logiczne                                                    |
| `E0601`     | Nieznany moduł biblioteki standardowej | Sprawdź rzeczywistą ścieżkę, np. `std.json`                                         |
| `E0605`     | Brak wymaganego jawnego importu        | Zaimportuj moduł, np. `import fs from "std.fs"`                                     |
| `E0901`     | Odczyt przeniesionej wartości          | Jawnie sklonuj niezależną wartość lub użyj obsługiwanego synchronicznego pożyczania |
| `E0913`     | Pożyczony parametr async               | Użyj parametru z własnością; sklonuj u wywołującego, jeśli potrzebny jest oryginał  |
| `E0918`     | `&value` w miejscu wywołania           | Przekaż zwykły argument; pożyczanie deklaruje sygnatura                             |

To podsumowanie zarejestrowanej diagnostyki, a nie lista wszystkich błędów. Aktualny nadrzędny rejestr to [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/error.rs).

## Błędy wykonania mają osobny kontrakt

Wartości `domain/code` błędu Error w czasie wykonania różnią się od kodów kompilatora `E`. Niepoprawny JSON może zwrócić `json/parse_error`; brakujący klucz obiektu — `object/missing_key`. Użyj `?` po wywołaniu mogącym się nie powieść albo odczytaj pola w `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

Ten program dydaktyczny celowo wywołuje obsługiwalny błąd parsowania. Zgłoszone `byte N` jest przesunięciem bajtowym UTF-8 liczonym od zera. Nie jest to liczba znaków w pobliżu tekstu chińskiego ani emoji.

## Natywne budowanie zostało odrzucone

Pomyślny `ku check` potwierdza wyłącznie przejście sprawdzania frontendu. Natywne C i LLVM mają własne ograniczenia. Gdy konstrukcja jest jawnie odrzucona, sprawdź [macierz obsługi](/pl/docs/support/). Zwiększenie limitu czasu lub sukces w innym zapleczu nie weryfikuje nieobsługiwanej ścieżki.

`ku build --native file.ku` bez `-o` jedynie zapisuje C. Aby skompilować i skonsolidować plik wykonywalny, użyj `ku build --backend c file.ku` i zainstaluj docelowy zestaw narzędzi C.

Błędy bazy danych `execution_unknown` i `execution_completed_without_result` nie mogą powodować automatycznego ponawiania. Zachowaj dowody żądania i operacji biznesowej, a następnie ustal rzeczywisty stan wykonania. Weryfikacja lokalnej witryny nie wymaga połączenia z produkcyjną bazą danych.

Więcej szczegółów: [przewodnik diagnostyczny repozytorium](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/diagnostics.md).
