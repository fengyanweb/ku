---
title: 'Projekt i współtworzenie'
description: 'Stan Ku, publiczne źródła, granice licencji i odtwarzalne zgłoszenia problemów.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Ku to język programowania i zestaw narzędzi: interpreter, domyślny wykonywalny runner i natywna kompilacja C obsługiwanego podzbioru.

## Źródła i licencjonowanie

Ku i rozszerzenie VS Code są udostępniane na licencji **Apache-2.0**. Zobacz [kod źródłowy](https://github.com/fengyanweb/ku), [wydanie 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) i [tekst licencji](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Komponenty zewnętrzne zachowują własne licencje. Pakiety wydania zawierają odpowiednie teksty licencji i informacje.

## Zgłoś problem

Odtwarzalne zgłoszenia umieszczaj w [Issues](https://github.com/fengyanweb/ku/issues) repozytorium. Dołącz:

Uwagi e-mailem: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Wersję Ku, commit Git dla źródeł rozwojowych oraz docelowy system / architekturę.
- Pełne polecenie, zaplecze i minimalne źródło `.ku`.
- Pełną diagnostykę lub ustrukturyzowany Error, oczekiwany i rzeczywisty wynik.
- Wersje narzędzi Rust / C przy problemach budowania oraz informację o lokalnych importach.

Nie przesyłaj tokenów, haseł, prawdziwych ciągów połączenia ani logów z poufnymi danymi biznesowymi. Raporty wydajności powinny zawierać środowisko, obciążenie, metodę pomiaru i surowe wyniki. Jedna udana próbka nie dowodzi przepustowości produkcyjnej.

## Przed współtworzeniem

Ogranicz zakres zmian i dołącz minimalny przykład odtwarzający problem oraz wyniki odpowiednich sprawdzeń. Kod, dokumentacja i przykłady powinny dotyczyć tej samej wersji. Opiekunowie sprawdzają zmiany przed ich połączeniem.

Witryna nie umożliwia wykonywania kodu Ku online.

Przejdź do [pierwszych kroków](/pl/docs/getting-started/), [granic obsługi](/pl/docs/support/) i [informacji o wydaniach](/pl/releases/).
